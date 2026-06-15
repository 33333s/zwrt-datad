/*
 * u60-datad - unified device-state aggregator for U60Pro plugins.
 *
 * Single producer: polls a fixed set of ubus getters at a controlled rate,
 * normalizes them into one flat JSON snapshot, and publishes it atomically to
 * a tmpfs file. Any number of consumers (devui, web, scripts) read that file,
 * so ubus load is decoupled from consumer count. No vendor libs.
 *
 * SPDX-License-Identifier: MIT
 */
#include "json.h"

#include <ctype.h>
#include <errno.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#ifndef STATE_DIR
#define STATE_DIR "/tmp/u60-datad"
#endif
#define STATE_FILE STATE_DIR "/state.json"
#define STATE_TMP  STATE_DIR "/state.json.tmp"
#define LOG_FILE   "/data/logfs/key.log"

#define RAW_MAX 8192
#define LOG_RAW_MAX (512 * 1024)
#define LOG_TAIL_LINES 6000

struct qos_info {
    int qci;
    int ambr_dl_raw;
    int ambr_ul_raw;
    int ambr_dl_unit;
    int ambr_ul_unit;
    double ambr_dl;
    double ambr_ul;
    int have_qci;
    int have_ambr;
};

static volatile sig_atomic_t g_run = 1;
static void on_signal(int s) { (void)s; g_run = 0; }

/* Run `ubus call <svc> <method> [args]` and capture stdout. 0 on output. */
static int run_ubus(const char *svc, const char *method, const char *args,
                    char *out, size_t outlen)
{
    char cmd[640];
    if (args && *args)
        snprintf(cmd, sizeof cmd, "ubus -t 3 call %s %s '%s' 2>/dev/null", svc, method, args);
    else
        snprintf(cmd, sizeof cmd, "ubus -t 3 call %s %s 2>/dev/null", svc, method);

    FILE *fp = popen(cmd, "r");
    if (!fp) { out[0] = 0; return -1; }
    size_t n = fread(out, 1, outlen - 1, fp);
    out[n] = 0;
    pclose(fp);
    return n > 0 ? 0 : -1;
}

/* Read a single uci value via `uci get`. Empty string on failure. */
static void uci_get(const char *key, char *out, size_t outlen)
{
    char cmd[160];
    snprintf(cmd, sizeof cmd, "uci -q get %s 2>/dev/null", key);
    out[0] = 0;
    FILE *fp = popen(cmd, "r");
    if (!fp) return;
    if (fgets(out, outlen, fp)) {
        size_t n = strlen(out);
        while (n && (out[n - 1] == '\n' || out[n - 1] == '\r')) out[--n] = 0;
    }
    pclose(fp);
}

/* ---- append helpers for building the snapshot ---- */
struct buf { char *p; size_t cap; size_t len; };

static void bappend(struct buf *b, const char *fmt, ...)
{
    if (b->len >= b->cap) return;
    va_list ap; va_start(ap, fmt);
    int n = vsnprintf(b->p + b->len, b->cap - b->len, fmt, ap);
    va_end(ap);
    if (n > 0) b->len += (size_t)n;
}

/* Emit "key":"<string value of src[srckey]>" with JSON escaping of quotes. */
static void emit_str(struct buf *b, const char *key, const char *src, const char *srckey)
{
    char v[256];
    if (!json_get(src, srckey, v, sizeof v)) v[0] = 0;
    bappend(b, "\"%s\":\"", key);
    for (char *c = v; *c; c++) {
        if (*c == '"' || *c == '\\') bappend(b, "\\%c", *c);
        else if ((unsigned char)*c < 0x20) bappend(b, " ");
        else bappend(b, "%c", *c);
    }
    bappend(b, "\"");
}

static void emit_int(struct buf *b, const char *key, const char *src, const char *srckey, long def)
{
    bappend(b, "\"%s\":%ld", key, json_get_int(src, srckey, def));
}

static void emit_double(struct buf *b, const char *key, double val)
{
    bappend(b, "\"%s\":%.3f", key, val);
}

/* Emit "key":"<v>" from a raw value (not from a JSON src) with quote escaping. */
static void emit_str_val(struct buf *b, const char *key, const char *v)
{
    bappend(b, "\"%s\":\"", key);
    for (const char *c = v; *c; c++) {
        if (*c == '"' || *c == '\\') bappend(b, "\\%c", *c);
        else if ((unsigned char)*c < 0x20) bappend(b, " ");
        else bappend(b, "%c", *c);
    }
    bappend(b, "\"");
}

static long mem_used_pct(const char *sysinfo)
{
    char mem[1024];
    if (!json_get(sysinfo, "memory", mem, sizeof mem)) return -1;
    long total = json_get_int(mem, "total", 0);
    long avail = json_get_int(mem, "available", 0);
    if (total <= 0) return -1;
    return (total - avail) * 100 / total;
}

/* Read a single integer from a sysfs file (0 on failure). */
static long read_long_file(const char *path)
{
    FILE *fp = fopen(path, "r");
    if (!fp) return 0;
    long v = 0;
    if (fscanf(fp, "%ld", &v) != 1) v = 0;
    fclose(fp);
    return v;
}

/* Instantaneous CPU usage % from /proc/stat (delta since the previous call). */
static long cpu_usage_pct(void)
{
    static unsigned long long prev_idle = 0, prev_total = 0;
    FILE *fp = fopen("/proc/stat", "r");
    if (!fp) return -1;
    char lbl[8];
    unsigned long long u = 0, n = 0, s = 0, i = 0, io = 0, ir = 0, si = 0, st = 0;
    int got = fscanf(fp, "%7s %llu %llu %llu %llu %llu %llu %llu %llu",
                     lbl, &u, &n, &s, &i, &io, &ir, &si, &st);
    fclose(fp);
    if (got < 5) return -1;
    unsigned long long idle = i + io;
    unsigned long long total = u + n + s + i + io + ir + si + st;
    unsigned long long dt = total - prev_total, di = idle - prev_idle;
    long pct = (prev_total && dt) ? (long)((dt - di) * 100 / dt) : -1;
    prev_idle = idle; prev_total = total;
    return pct;
}

/* Emit "list":[{name,ip,mac},...] of DHCP leases (current LAN devices). */
static void emit_client_list(struct buf *b)
{
    bappend(b, "\"list\":[");
    FILE *fp = fopen("/tmp/dhcp.leases", "r");
    int n = 0;
    if (fp) {
        char line[256];
        while (fgets(line, sizeof line, fp) && n < 32) {
            /* format: <expiry> <mac> <ip> <name> <clientid> */
            char mac[32] = "", ip[48] = "", name[80] = "-";
            if (sscanf(line, "%*s %31s %47s %79s", mac, ip, name) >= 2) {
                if (!name[0] || (name[0] == '*' && !name[1])) strcpy(name, "未命名");
                if (n) bappend(b, ",");
                bappend(b, "{");
                emit_str_val(b, "name", name); bappend(b, ",");
                emit_str_val(b, "ip", ip);     bappend(b, ",");
                emit_str_val(b, "mac", mac);
                bappend(b, "}");
                n++;
            }
        }
        fclose(fp);
    }
    bappend(b, "]");
}

/* Decode a UTF-16BE hex string (ZTE SMS content) into UTF-8. */
static void sms_decode(const char *hex, char *out, size_t cap)
{
    size_t o = 0;
    const char *p = hex;
    while (p[0] && p[1] && p[2] && p[3] && o + 5 < cap) {
        char b4[5] = { p[0], p[1], p[2], p[3], 0 };
        unsigned cp = (unsigned)strtol(b4, NULL, 16);
        p += 4;
        if (cp >= 0xD800 && cp <= 0xDBFF && p[0] && p[1] && p[2] && p[3]) { /* surrogate pair */
            char l4[5] = { p[0], p[1], p[2], p[3], 0 };
            unsigned lo = (unsigned)strtol(l4, NULL, 16);
            if (lo >= 0xDC00 && lo <= 0xDFFF) { cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00); p += 4; }
        }
        if (cp < 0x80) out[o++] = (char)cp;
        else if (cp < 0x800) { out[o++] = 0xC0 | (cp >> 6); out[o++] = 0x80 | (cp & 0x3F); }
        else if (cp < 0x10000) { out[o++] = 0xE0 | (cp >> 12); out[o++] = 0x80 | ((cp >> 6) & 0x3F); out[o++] = 0x80 | (cp & 0x3F); }
        else if (o + 4 < cap) { out[o++] = 0xF0 | (cp >> 18); out[o++] = 0x80 | ((cp >> 12) & 0x3F); out[o++] = 0x80 | ((cp >> 6) & 0x3F); out[o++] = 0x80 | (cp & 0x3F); }
    }
    out[o] = 0;
}

/* SMS: unread count (each call, cheap) + recent received list (refreshed every
 * ~10 calls; reading+decoding is heavier). Emits "sms":{unread,list:[...]}. */
static void emit_sms(struct buf *b)
{
    static int tick = 0;
    static long prev_unread = -1;
    static char list_items[6000] = "";   /* cached inner JSON of the list */
    char cap_raw[1024];
    run_ubus("zwrt_wms", "zwrt_wms_get_wms_capacity", NULL, cap_raw, sizeof cap_raw);
    long unread = json_get_int(cap_raw, "sms_dev_unread_num", 0) +
                  json_get_int(cap_raw, "sms_sim_unread_num", 0);

    /* reload the list every 10 ticks, OR immediately when the unread count
     * changes (new SMS, or the UI marked one read) so the per-item dot tracks. */
    int reload = (tick == 0) || (unread != prev_unread);
    prev_unread = unread;
    if (reload) {
        static char raw[16384];
        run_ubus("zwrt_wms", "zte_libwms_get_sms_data",
                 "{\"page\":0,\"data_per_page\":6,\"mem_store\":1,\"tags\":10,"
                 "\"order_by\":\"order by id desc\",\"sms_no_encode_flag\":\"1\"}",
                 raw, sizeof raw);
        struct buf lb = { list_items, sizeof list_items, 0 };
        char arr[15000];
        int n = 0;
        if (json_get(raw, "messages", arr, sizeof arr)) {
            for (char *q = arr; (q = strchr(q, '{')) && n < 8; ) {
                char *end = strchr(q, '}');
                if (!end) break;
                char obj[4096]; size_t L = (size_t)(end - q) + 1;
                if (L >= sizeof obj) L = sizeof obj - 1;
                memcpy(obj, q, L); obj[L] = 0;
                char num[40] = "", date[40] = "", tag[8] = "", hex[2048] = "";
                json_get(obj, "number", num, sizeof num);
                json_get(obj, "date", date, sizeof date);
                json_get(obj, "tag", tag, sizeof tag);
                json_get(obj, "content", hex, sizeof hex);
                long id = json_get_int(obj, "id", 0);
                /* date "YY,MM,DD,HH,MM,SS,+TZ" -> "MM-DD HH:MM" */
                char shown[16] = "";
                { int yy, mo, dd, hh, mi; if (sscanf(date, "%d,%d,%d,%d,%d", &yy, &mo, &dd, &hh, &mi) == 5)
                    snprintf(shown, sizeof shown, "%02d-%02d %02d:%02d", mo, dd, hh, mi); }
                char text[700]; sms_decode(hex, text, sizeof text);
                if (n) bappend(&lb, ",");
                bappend(&lb, "{");
                bappend(&lb, "\"id\":%ld,", id);
                emit_str_val(&lb, "num", num);    bappend(&lb, ",");
                emit_str_val(&lb, "date", shown); bappend(&lb, ",");
                /* received-message tag: "1" = unread, "0" = read */
                bappend(&lb, "\"unread\":%d,", (tag[0] == '1') ? 1 : 0);
                emit_str_val(&lb, "text", text);
                bappend(&lb, "}");
                n++;
                q = end + 1;
            }
        }
        list_items[lb.len] = 0;
    }
    tick = (tick + 1) % 10;

    bappend(b, "\"sms\":{\"unread\":%ld,\"list\":[%s]}", unread, list_items);
}

static double ambr_unit_to_mbps(int unit)
{
    switch (unit) {
    case 0: return 1.0 / 1000.0 / 1000.0; /* bps */
    case 1: return 1.0 / 1000.0;          /* Kbps */
    case 2: return 4.0 / 1000.0;          /* 4 Kbps */
    case 3: return 16.0 / 1000.0;         /* 16 Kbps */
    case 4: return 64.0 / 1000.0;         /* 64 Kbps */
    case 5: return 256.0 / 1000.0;        /* 256 Kbps */
    case 6: return 1.0;                   /* Mbps */
    default: return 0.0;
    }
}

/* Find the last `lines` lines of a file and copy them into `out`. */
static int read_log_tail(const char *path, size_t lines, char *out, size_t outlen)
{
    FILE *fp = fopen(path, "rb");
    if (!fp) { out[0] = 0; return -1; }

    if (fseek(fp, 0, SEEK_END) != 0) {
        fclose(fp);
        out[0] = 0;
        return -1;
    }

    long end = ftell(fp);
    if (end <= 0) {
        fclose(fp);
        out[0] = 0;
        return -1;
    }

    long pos = end;
    long start = 0;
    size_t seen = 0;
    char blk[4096];

    while (pos > 0 && seen <= lines) {
        size_t want = (pos >= (long)sizeof blk) ? sizeof blk : (size_t)pos;
        pos -= (long)want;

        if (fseek(fp, pos, SEEK_SET) != 0) break;
        size_t got = fread(blk, 1, want, fp);
        if (got == 0) break;

        for (long i = (long)got - 1; i >= 0; i--) {
            if (blk[i] == '\n') {
                seen++;
                if (seen > lines) {
                    start = pos + i + 1;
                    goto found_start;
                }
            }
        }
    }

found_start:
    if (start < 0) start = 0;
    if (start > end) start = end;

    long tail_len = end - start;
    if (tail_len <= 0) {
        fclose(fp);
        out[0] = 0;
        return -1;
    }

    if ((size_t)tail_len >= outlen) {
        start = end - (long)(outlen - 1);
        tail_len = end - start;
    }

    if (fseek(fp, start, SEEK_SET) != 0) {
        fclose(fp);
        out[0] = 0;
        return -1;
    }

    size_t n = fread(out, 1, (size_t)tail_len, fp);
    out[n] = 0;
    fclose(fp);
    return n > 0 ? 0 : -1;
}

static int line_get_int(const char *line, const char *key, int *out)
{
    const char *p = strstr(line, key);
    if (!p) return 0;

    p += strlen(key);
    while (*p && isspace((unsigned char)*p)) p++;
    if (*p != '=') return 0;
    p++;
    while (*p && isspace((unsigned char)*p)) p++;

    char *end;
    long v = strtol(p, &end, 10);
    if (end == p) return 0;
    *out = (int)v;
    return 1;
}

static void parse_qos_log(char *logbuf, struct qos_info *qos)
{
    memset(qos, 0, sizeof *qos);

    char *line = logbuf;
    while (line && *line) {
        char *next = strchr(line, '\n');
        if (next) *next = 0;
        size_t len = strlen(line);
        if (len > 0 && line[len - 1] == '\r') line[len - 1] = 0;

        if (!qos->have_qci && strstr(line, "[DATA]") && strstr(line, "qci")) {
            int qci;
            if (line_get_int(line, "qci", &qci)) {
                qos->qci = qci;
                qos->have_qci = 1;
            }
        } else if (qos->have_qci && strstr(line, "[DATA]") && strstr(line, "qci")) {
            int qci;
            if (line_get_int(line, "qci", &qci))
                qos->qci = qci;
        }

        {
            int dl, dl_unit, ul, ul_unit;
            if (line_get_int(line, "session_ambr_dl", &dl) &&
                line_get_int(line, "session_ambr_dl_unit", &dl_unit) &&
                line_get_int(line, "session_ambr_ul", &ul) &&
                line_get_int(line, "session_ambr_ul_unit", &ul_unit)) {
                qos->ambr_dl_raw = dl;
                qos->ambr_dl_unit = dl_unit;
                qos->ambr_ul_raw = ul;
                qos->ambr_ul_unit = ul_unit;
                qos->ambr_dl = (double)dl * ambr_unit_to_mbps(dl_unit);
                qos->ambr_ul = (double)ul * ambr_unit_to_mbps(ul_unit);
                qos->have_ambr = 1;
            }
        }

        if (!next) break;
        line = next + 1;
    }
}

/* Poll everything and build the unified snapshot into `out`. */
static void build_snapshot(char *out, size_t outlen, int with_board, const char *board_cache)
{
    char net[RAW_MAX], batt[RAW_MAX], chg[RAW_MAX], therm[1024];
    char rnum[1024], rstat[1024], traf[RAW_MAX], sysinfo[2048], usb[1024], nfc[512];
    static char logtail[LOG_RAW_MAX];
    static char s_imei[32] = "", s_swver[80] = "";   /* static: fetch once */
    struct qos_info qos;

    run_ubus("zte_nwinfo_api", "nwinfo_get_netinfo", NULL, net, sizeof net);
    run_ubus("zwrt_bsp.battery", "list", NULL, batt, sizeof batt);
    run_ubus("zwrt_bsp.charger", "list", NULL, chg, sizeof chg);
    run_ubus("zwrt_bsp.thermal", "get_cpu_temp", NULL, therm, sizeof therm);
    run_ubus("zwrt_router.api", "router_get_user_list_num", NULL, rnum, sizeof rnum);
    run_ubus("zwrt_router.api", "router_get_status_no_auth", NULL, rstat, sizeof rstat);
    /* type:1 = realtime session stats; cid:1 = main PDN (rmnet_data0). */
    run_ubus("zwrt_data", "get_wwandst",
             "{\"source_module\":\"deviceui\",\"cid\":1,\"type\":1}", traf, sizeof traf);
    run_ubus("system", "info", NULL, sysinfo, sizeof sysinfo);
    run_ubus("zwrt_bsp.usb", "list", NULL, usb, sizeof usb);
    run_ubus("zwrt_nfc", "zwrt_nfc_wifi_get", NULL, nfc, sizeof nfc);
    if (read_log_tail(LOG_FILE, LOG_TAIL_LINES, logtail, sizeof logtail) == 0)
        parse_qos_log(logtail, &qos);
    else
        memset(&qos, 0, sizeof qos);

    /* qci/ambr come from rare PDU-setup log lines that scroll out of the tail
     * window over time; once seen, keep the last known good values. */
    static struct qos_info sticky;
    if (qos.have_qci) { sticky.qci = qos.qci; sticky.have_qci = 1; }
    if (qos.have_ambr) {
        sticky.ambr_dl = qos.ambr_dl; sticky.ambr_ul = qos.ambr_ul;
        sticky.ambr_dl_raw = qos.ambr_dl_raw; sticky.ambr_ul_raw = qos.ambr_ul_raw;
        sticky.ambr_dl_unit = qos.ambr_dl_unit; sticky.ambr_ul_unit = qos.ambr_ul_unit;
        sticky.have_ambr = 1;
    }
    qos = sticky;

    /* device identifiers change rarely: fetch once. */
    if (!s_imei[0]) {
        char r[256];
        if (run_ubus("zwrt_zte_mdm.api", "get_imei", NULL, r, sizeof r) == 0)
            if (!json_get(r, "imei", s_imei, sizeof s_imei)) s_imei[0] = 0;
    }
    if (!s_swver[0])
        uci_get("zwrt_common_info.common_config.wa_inner_version", s_swver, sizeof s_swver);

    struct buf b = { out, outlen, 0 };
    bappend(&b, "{\"ts\":%ld,", (long)time(NULL));

    /* network / signal */
    bappend(&b, "\"net\":{");
    emit_str(&b, "type", net, "network_type");      bappend(&b, ",");
    emit_int(&b, "bars", net, "signalbar", 0);       bappend(&b, ",");
    emit_str(&b, "operator", net, "network_provider_fullname"); bappend(&b, ",");
    emit_str(&b, "band", net, "wan_active_band");    bappend(&b, ",");
    emit_int(&b, "nr_rsrp", net, "nr5g_rsrp", 0);    bappend(&b, ",");
    emit_int(&b, "nr_rsrq", net, "nr5g_rsrq", 0);    bappend(&b, ",");
    emit_str(&b, "nr_snr", net, "nr5g_snr");         bappend(&b, ",");
    emit_int(&b, "nr_rssi", net, "nr5g_rssi", 0);    bappend(&b, ",");
    emit_int(&b, "lte_rsrp", net, "lte_rsrp", 0);    bappend(&b, ",");
    emit_int(&b, "lte_rsrq", net, "lte_rsrq", 0);    bappend(&b, ",");
    emit_int(&b, "lte_rssi", net, "lte_rssi", 0);    bappend(&b, ",");
    emit_str(&b, "lte_snr", net, "lte_snr");         bappend(&b, ",");
    emit_int(&b, "rssi", net, "rssi", 0);            bappend(&b, ",");
    emit_int(&b, "mcc", net, "rmcc", 0);             bappend(&b, ",");
    emit_int(&b, "mnc", net, "rmnc", 0);             bappend(&b, ",");
    emit_int(&b, "nr_pci", net, "nr5g_pci", 0);      bappend(&b, ",");
    emit_int(&b, "nr_cell_id", net, "nr5g_cell_id", 0); bappend(&b, ",");
    emit_int(&b, "nr_channel", net, "nr5g_action_channel", 0); bappend(&b, ",");
    emit_str(&b, "nr_bw", net, "nr5g_bandwidth");    bappend(&b, ",");
    /* CA descriptors: ';'-separated carriers, ','-separated fields. Passthrough. */
    emit_str(&b, "nrca", net, "nrca");               bappend(&b, ",");
    emit_str(&b, "lteca", net, "lteca");             bappend(&b, ",");
    /* network selection + band-lock (available/current bands, comma lists) */
    emit_str(&b, "net_select", net, "net_select");   bappend(&b, ",");
    emit_str(&b, "sa_bands", net, "nr5g_sa_band_lock");   bappend(&b, ",");
    emit_str(&b, "nsa_bands", net, "nr5g_nsa_band_lock"); bappend(&b, ",");
    emit_str(&b, "lte_bands", net, "lte_band");      bappend(&b, ",");
    emit_str(&b, "wan_status", rstat, "current_wan_status");
    bappend(&b, "},");

    /* battery / charger */
    bappend(&b, "\"battery\":{");
    emit_int(&b, "percent", batt, "battery_capacity", -1);   bappend(&b, ",");
    emit_int(&b, "temp", batt, "battery_temperature", 0);     bappend(&b, ",");
    emit_int(&b, "online", batt, "battery_online", 0);        bappend(&b, ",");
    emit_int(&b, "health", batt, "battery_health", 0);        bappend(&b, ",");
    emit_int(&b, "time_to_full", batt, "battery_time_to_full", -1); bappend(&b, ",");
    emit_int(&b, "charging", chg, "charge_status", 0);        bappend(&b, ",");
    emit_int(&b, "charger_connect", chg, "charger_connect", 0); bappend(&b, ",");
    emit_int(&b, "charger_type", chg, "charger_type", 0);        bappend(&b, ",");
    /* voltage/current from power_supply sysfs (µV / µA). chg_* = charger input
     * (usb), bat_* = battery; UI computes V, mA and charge/discharge power. */
    bappend(&b, "\"chg_uv\":%ld,", read_long_file("/sys/class/power_supply/usb/voltage_now"));
    bappend(&b, "\"chg_ua\":%ld,", read_long_file("/sys/class/power_supply/usb/current_now"));
    bappend(&b, "\"bat_uv\":%ld,", read_long_file("/sys/class/power_supply/battery/voltage_now"));
    bappend(&b, "\"bat_ua\":%ld",  read_long_file("/sys/class/power_supply/battery/current_now"));
    bappend(&b, "},");

    /* connected clients (count + per-device list from DHCP leases) */
    bappend(&b, "\"clients\":{");
    emit_int(&b, "total", rnum, "access_total_num", 0); bappend(&b, ",");
    emit_int(&b, "wifi", rnum, "wireless_num", 0);      bappend(&b, ",");
    emit_int(&b, "lan", rnum, "lan_num", 0);            bappend(&b, ",");
    emit_client_list(&b);
    bappend(&b, "},");

    /* SMS: unread count + recent received messages (decoded). */
    emit_sms(&b);
    bappend(&b, ",");

    /* main WiFi (2.4G/5G share one SSID). Key name is "wlan" not "wifi" so the
     * consumer's lookup doesn't collide with clients.wifi. */
    {
        char ssid[128], wkey[128], enc[64], dis[8];
        uci_get("wireless.main_2g.ssid", ssid, sizeof ssid);
        uci_get("wireless.main_2g.key", wkey, sizeof wkey);
        uci_get("wireless.main_2g.encryption", enc, sizeof enc);
        uci_get("wireless.main_2g.disabled", dis, sizeof dis);
        bappend(&b, "\"wlan\":{");
        emit_str_val(&b, "ssid", ssid); bappend(&b, ",");
        emit_str_val(&b, "key", wkey);  bappend(&b, ",");
        emit_str_val(&b, "enc", enc);   bappend(&b, ",");
        bappend(&b, "\"enabled\":%d", (dis[0] == '1') ? 0 : 1);
        bappend(&b, "},");
    }

    /* NFC (tap-to-share WiFi): switch 1 = on. */
    bappend(&b, "\"nfc\":{");
    emit_int(&b, "switch", nfc, "switch", 0);
    bappend(&b, "},");

    /* DHCP / LAN */
    {
        char ip[48], start[16], limit[16], lease[16];
        uci_get("network.lan.ipaddr", ip, sizeof ip);
        uci_get("dhcp.lan.zte_start", start, sizeof start);
        if (!start[0]) uci_get("dhcp.lan.start", start, sizeof start);
        uci_get("dhcp.lan.limit", limit, sizeof limit);
        uci_get("dhcp.lan.leasetime", lease, sizeof lease);
        bappend(&b, "\"dhcp\":{");
        emit_str_val(&b, "ip", ip);          bappend(&b, ",");
        emit_str_val(&b, "start", start);    bappend(&b, ",");
        emit_str_val(&b, "limit", limit);    bappend(&b, ",");
        emit_str_val(&b, "leasetime", lease);
        bappend(&b, "},");
    }

    /* traffic: realtime session counters + speeds (bytes/s). */
    bappend(&b, "\"traffic\":{");
    emit_int(&b, "rx_speed", traf, "real_rx_speed", 0);         bappend(&b, ",");
    emit_int(&b, "tx_speed", traf, "real_tx_speed", 0);         bappend(&b, ",");
    emit_int(&b, "max_rx_speed", traf, "real_max_rx_speed", 0); bappend(&b, ",");
    emit_int(&b, "max_tx_speed", traf, "real_max_tx_speed", 0); bappend(&b, ",");
    emit_int(&b, "rx_bytes", traf, "real_rx_bytes", 0);         bappend(&b, ",");
    emit_int(&b, "tx_bytes", traf, "real_tx_bytes", 0);         bappend(&b, ",");
    emit_int(&b, "session_time", traf, "real_time", 0);
    bappend(&b, "},");

    /* qos: parsed from the tail of key.log. */
    bappend(&b, "\"qos\":{");
    bappend(&b, "\"qci\":%d,", qos.have_qci ? qos.qci : 0);
    emit_double(&b, "ambr_dl", qos.have_ambr ? qos.ambr_dl : 0.0); bappend(&b, ",");
    emit_double(&b, "ambr_ul", qos.have_ambr ? qos.ambr_ul : 0.0); bappend(&b, ",");
    bappend(&b, "\"ambr_dl_raw\":%d,", qos.have_ambr ? qos.ambr_dl_raw : 0);
    bappend(&b, "\"ambr_ul_raw\":%d,", qos.have_ambr ? qos.ambr_ul_raw : 0);
    bappend(&b, "\"ambr_dl_unit\":%d,", qos.have_ambr ? qos.ambr_dl_unit : 0);
    bappend(&b, "\"ambr_ul_unit\":%d,", qos.have_ambr ? qos.ambr_ul_unit : 0);
    emit_str(&b, "usb_mode", usb, "mode");
    bappend(&b, "},");

    /* system */
    bappend(&b, "\"system\":{");
    emit_int(&b, "uptime", sysinfo, "uptime", 0);    bappend(&b, ",");
    emit_int(&b, "cpu_temp", therm, "cpuss_temp", 0); bappend(&b, ",");
    bappend(&b, "\"cpu_usage\":%ld,", cpu_usage_pct());
    bappend(&b, "\"mem_used_pct\":%ld,", mem_used_pct(sysinfo));
    { char mem[1024]; long tot = 0, av = 0;
      if (json_get(sysinfo, "memory", mem, sizeof mem)) {
          tot = json_get_int(mem, "total", 0); av = json_get_int(mem, "available", 0); }
      bappend(&b, "\"mem_total\":%ld,\"mem_avail\":%ld,", tot, av); }
    emit_str_val(&b, "sw_version", s_swver); bappend(&b, ",");
    emit_str_val(&b, "imei", s_imei);        bappend(&b, ",");
    if (with_board) {
        emit_str(&b, "model", board_cache, "model");        bappend(&b, ",");
        emit_str(&b, "hostname", board_cache, "hostname");  bappend(&b, ",");
        char rel[512];
        if (json_get(board_cache, "release", rel, sizeof rel))
            emit_str(&b, "fw", rel, "description");
        else
            bappend(&b, "\"fw\":\"\"");
    } else {
        bappend(&b, "\"model\":\"\",\"hostname\":\"\",\"fw\":\"\"");
    }
    bappend(&b, "}}");
}

static void atomic_write(const char *data, size_t len)
{
    FILE *fp = fopen(STATE_TMP, "w");
    if (!fp) return;
    fwrite(data, 1, len, fp);
    fclose(fp);
    rename(STATE_TMP, STATE_FILE);   /* atomic on same fs */
}

int main(int argc, char **argv)
{
    int once = 0, interval_ms = 1000;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--once")) once = 1;
        else if (!strcmp(argv[i], "-i") && i + 1 < argc) interval_ms = atoi(argv[++i]);
    }

    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);
    mkdir(STATE_DIR, 0755);

    /* board info changes rarely: fetch once, refresh hourly. */
    static char board[RAW_MAX];
    run_ubus("system", "board", NULL, board, sizeof board);

    char snap[RAW_MAX * 2];
    long cycle = 0;
    do {
        if (cycle % 3600 == 0 && cycle != 0)
            run_ubus("system", "board", NULL, board, sizeof board);

        build_snapshot(snap, sizeof snap, board[0] != 0, board);

        if (once) {
            fputs(snap, stdout);
            fputc('\n', stdout);
            break;
        }
        atomic_write(snap, strlen(snap));

        struct timespec ts = { interval_ms / 1000, (long)(interval_ms % 1000) * 1000000L };
        nanosleep(&ts, NULL);
        cycle++;
    } while (g_run);

    return 0;
}
