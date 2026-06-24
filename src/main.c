/*
 * zwrt-datad - unified device-state aggregator for OpenWRT device plugins.
 *
 * Single producer: polls a fixed set of ubus getters at a controlled rate,
 * normalizes them into one flat JSON snapshot, and publishes it atomically to
 * a tmpfs file. Any number of consumers (devui, web, scripts) read that file,
 * so ubus load is decoupled from consumer count. No vendor libs.
 *
 * SPDX-License-Identifier: MIT
 */
#include "json.h"

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

#define RAW_MAX 8192

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

static void bappend_json_esc(struct buf *b, const char *s)
{
    if (!s) s = "";
    for (const char *c = s; *c; c++) {
        if (*c == '"' || *c == '\\') bappend(b, "\\%c", *c);
        else if ((unsigned char)*c < 0x20) bappend(b, " ");
        else bappend(b, "%c", *c);
    }
}

/* Emit "key":"<string value of src[srckey]>" with JSON escaping of quotes. */
static void emit_str(struct buf *b, const char *key, const char *src, const char *srckey)
{
    char v[256];
    if (!json_get(src, srckey, v, sizeof v)) v[0] = 0;
    bappend(b, "\"%s\":\"", key);
    bappend_json_esc(b, v);
    bappend(b, "\"");
}

static void emit_int(struct buf *b, const char *key, const char *src, const char *srckey, long def)
{
    bappend(b, "\"%s\":%ld", key, json_get_int(src, srckey, def));
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

static long mem_field(const char *sysinfo, const char *key)
{
    char mem[1024];
    if (!json_get(sysinfo, "memory", mem, sizeof mem)) return 0;
    return json_get_int(mem, key, 0);
}

static int g_qci;
static int g_qci_valid;
static double g_ambr_dl, g_ambr_ul;
static int g_ambr_dl_valid, g_ambr_ul_valid;
static unsigned long long g_cpu_prev_total, g_cpu_prev_idle;
static int g_cpu_prev_valid;

static int parse_int_after(const char *s, const char *needle, int *out)
{
    const char *p = strstr(s, needle);
    if (!p) return 0;
    p += strlen(needle);
    while (*p && (*p < '0' || *p > '9')) p++;
    if (!*p) return 0;
    *out = atoi(p);
    return 1;
}

static int parse_double_after(const char *s, const char *needle, double *out)
{
    const char *p = strstr(s, needle);
    char *end;
    if (!p) return 0;
    p += strlen(needle);
    *out = strtod(p, &end);
    return end != p;
}

static void refresh_qos_cache(void)
{
    char line[1024];
    int qci;
    double dl, ul;
    FILE *fp = popen("grep -a '\\[DATA\\].*qci' /data/logfs/key.log 2>/dev/null | tail -n 1", "r");
    if (fp) {
        if (fgets(line, sizeof line, fp) && parse_int_after(line, "qci", &qci)) {
            g_qci = qci;
            g_qci_valid = 1;
        }
        pclose(fp);
    }

    fp = popen("grep -a 'apn_ambr' /data/logfs/key.log 2>/dev/null | tail -n 1", "r");
    if (!fp) return;
    if (fgets(line, sizeof line, fp)) {
        if (parse_double_after(line, "apn_ambr_dl_ext2=", &dl) ||
            parse_double_after(line, "apn_ambr_dl_ext=", &dl) ||
            (parse_double_after(line, "apn_ambr_dl=", &dl) && (dl /= 1000.0, 1))) {
            g_ambr_dl = dl;
            g_ambr_dl_valid = 1;
        }

        if (parse_double_after(line, "apn_ambr_ul_ext2=", &ul) ||
            parse_double_after(line, "apn_ambr_ul_ext=", &ul) ||
            (parse_double_after(line, "apn_ambr_ul=", &ul) && (ul /= 1000.0, 1))) {
            g_ambr_ul = ul;
            g_ambr_ul_valid = 1;
        }
    }
    pclose(fp);
}

static long read_long_file(const char *path, long def)
{
    FILE *fp = fopen(path, "r");
    long v;
    if (!fp) return def;
    if (fscanf(fp, "%ld", &v) != 1) v = def;
    fclose(fp);
    return v;
}

static int cpu_usage_pct(void)
{
    FILE *fp = fopen("/proc/stat", "r");
    char line[256];
    unsigned long long user, nice, sys, idle, iowait, irq, softirq, steal;
    unsigned long long total, idle_all, dt, di;
    int pct = -1;

    if (!fp) return -1;
    if (!fgets(line, sizeof line, fp)) { fclose(fp); return -1; }
    fclose(fp);
    if (sscanf(line, "cpu %llu %llu %llu %llu %llu %llu %llu %llu",
               &user, &nice, &sys, &idle, &iowait, &irq, &softirq, &steal) < 4)
        return -1;

    idle_all = idle + iowait;
    total = user + nice + sys + idle + iowait + irq + softirq + steal;
    if (g_cpu_prev_valid) {
        dt = total - g_cpu_prev_total;
        di = idle_all - g_cpu_prev_idle;
        if (dt > 0) pct = (int)(((dt - di) * 100ULL) / dt);
    }
    g_cpu_prev_total = total;
    g_cpu_prev_idle = idle_all;
    g_cpu_prev_valid = 1;
    return pct;
}

static void chomp(char *s)
{
    size_t n;
    if (!s) return;
    n = strlen(s);
    while (n && (s[n - 1] == '\n' || s[n - 1] == '\r')) s[--n] = 0;
}

static void load_wifi_dhcp(char *ssid, size_t ssid_n,
                           char *key, size_t key_n,
                           char *enc, size_t enc_n,
                           int *enabled,
                           char *ip, size_t ip_n,
                           char *start, size_t start_n,
                           char *limit, size_t limit_n,
                           char *lease, size_t lease_n)
{
    FILE *fp = popen(
        "echo SSID=$(uci -q get wireless.main_2g.ssid 2>/dev/null);"
        "echo KEY=$(uci -q get wireless.main_2g.key 2>/dev/null);"
        "echo ENC=$(uci -q get wireless.main_2g.encryption 2>/dev/null);"
        "echo DIS=$(uci -q get wireless.main_2g.disabled 2>/dev/null);"
        "echo IP=$(uci -q get network.lan.ipaddr 2>/dev/null);"
        "echo START=$(uci -q get dhcp.lan.start 2>/dev/null);"
        "echo LIMIT=$(uci -q get dhcp.lan.limit 2>/dev/null);"
        "echo LEASE=$(uci -q get dhcp.lan.leasetime 2>/dev/null)", "r");
    char line[256];

    ssid[0] = key[0] = enc[0] = ip[0] = start[0] = limit[0] = lease[0] = 0;
    *enabled = 1;
    if (!fp) return;
    while (fgets(line, sizeof line, fp)) {
        chomp(line);
        if      (!strncmp(line, "SSID=", 5))  snprintf(ssid,  ssid_n,  "%s", line + 5);
        else if (!strncmp(line, "KEY=", 4))   snprintf(key,   key_n,   "%s", line + 4);
        else if (!strncmp(line, "ENC=", 4))   snprintf(enc,   enc_n,   "%s", line + 4);
        else if (!strncmp(line, "DIS=", 4))   *enabled = atoi(line + 4) ? 0 : 1;
        else if (!strncmp(line, "IP=", 3))    snprintf(ip,    ip_n,    "%s", line + 3);
        else if (!strncmp(line, "START=", 6)) snprintf(start, start_n, "%s", line + 6);
        else if (!strncmp(line, "LIMIT=", 6)) snprintf(limit, limit_n, "%s", line + 6);
        else if (!strncmp(line, "LEASE=", 6)) snprintf(lease, lease_n, "%s", line + 6);
    }
    pclose(fp);
}

static void build_client_list_json(char *out, size_t outlen)
{
    FILE *fp = fopen("/tmp/dhcp.leases", "r");
    char line[512];
    struct buf b = { out, outlen, 0 };
    int first = 1;

    bappend(&b, "[");
    if (fp) {
        while (fgets(line, sizeof line, fp)) {
            long exp = 0;
            char mac[32] = "", ip[32] = "", host[96] = "", cid[160] = "";
            const char *name;
            if (sscanf(line, "%ld %31s %31s %95s %159s", &exp, mac, ip, host, cid) < 4)
                continue;
            (void)exp; (void)cid;
            name = (host[0] && strcmp(host, "*")) ? host : mac;
            if (!first) bappend(&b, ",");
            first = 0;
            bappend(&b, "{\"name\":\"");
            bappend_json_esc(&b, name);
            bappend(&b, "\",\"ip\":\"");
            bappend_json_esc(&b, ip);
            bappend(&b, "\",\"mac\":\"");
            bappend_json_esc(&b, mac);
            bappend(&b, "\"}");
        }
        fclose(fp);
    }
    bappend(&b, "]");
}

/* Poll everything and build the unified snapshot into `out`. */
static void build_snapshot(char *out, size_t outlen,
                           int with_board, const char *board_cache,
                           int with_common, const char *common_cache,
                           int with_imei, const char *imei_cache)
{
    char net[RAW_MAX], batt[RAW_MAX], chg[RAW_MAX], therm[1024];
    char rnum[1024], rstat[1024], traf[RAW_MAX], sysinfo[2048], usb[1024], nfc[1024];
    char wifi_ssid[128], wifi_key[128], wifi_enc[64];
    char dhcp_ip[32], dhcp_start[32], dhcp_limit[16], dhcp_lease[32];
    char client_list[4096];
    long chg_uv, chg_ua, bat_uv, bat_ua;
    int cpu_usage, wifi_enabled;

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
    refresh_qos_cache();
    load_wifi_dhcp(wifi_ssid, sizeof wifi_ssid,
                   wifi_key, sizeof wifi_key,
                   wifi_enc, sizeof wifi_enc,
                   &wifi_enabled,
                   dhcp_ip, sizeof dhcp_ip,
                   dhcp_start, sizeof dhcp_start,
                   dhcp_limit, sizeof dhcp_limit,
                   dhcp_lease, sizeof dhcp_lease);
    build_client_list_json(client_list, sizeof client_list);
    chg_uv = read_long_file("/sys/class/power_supply/usb/voltage_now", 0);
    chg_ua = read_long_file("/sys/class/power_supply/usb/current_now", 0);
    bat_uv = read_long_file("/sys/class/power_supply/battery/voltage_now", 0);
    bat_ua = read_long_file("/sys/class/power_supply/battery/current_now", 0);
    cpu_usage = cpu_usage_pct();

    struct buf b = { out, outlen, 0 };
    bappend(&b, "{\"ts\":%ld,", (long)time(NULL));

    /* network / signal */
    bappend(&b, "\"net\":{");
    emit_str(&b, "type", net, "network_type");      bappend(&b, ",");
    emit_int(&b, "bars", net, "signalbar", 0);       bappend(&b, ",");
    emit_str(&b, "operator", net, "network_provider_fullname"); bappend(&b, ",");
    emit_str(&b, "band", net, "wan_active_band");    bappend(&b, ",");
    emit_str(&b, "nr_band", net, "nr5g_action_band"); bappend(&b, ",");
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
    emit_str(&b, "nrca", net, "nrca");               bappend(&b, ",");
    emit_str(&b, "lteca", net, "lteca");             bappend(&b, ",");
    emit_str(&b, "ltecasig", net, "ltecasig");       bappend(&b, ",");
    emit_str(&b, "net_select", net, "net_select");   bappend(&b, ",");
    emit_str(&b, "sa_bands", net, "nr5g_sa_band_lock"); bappend(&b, ",");
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
    emit_int(&b, "charger_type", chg, "charger_type", 0);     bappend(&b, ",");
    bappend(&b, "\"chg_uv\":%ld,\"chg_ua\":%ld,\"bat_uv\":%ld,\"bat_ua\":%ld",
            chg_uv, chg_ua, bat_uv, bat_ua);
    bappend(&b, "},");

    /* connected clients */
    bappend(&b, "\"clients\":{");
    emit_int(&b, "total", rnum, "access_total_num", 0); bappend(&b, ",");
    emit_int(&b, "wifi", rnum, "wireless_num", 0);      bappend(&b, ",");
    emit_int(&b, "lan", rnum, "lan_num", 0);            bappend(&b, ",");
    bappend(&b, "\"list\":%s", client_list);
    bappend(&b, "},");

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

    /* qos: last known bearer/QoS values cached from modem key.log */
    bappend(&b, "\"qos\":{");
    bappend(&b, "\"qci\":%d,", g_qci_valid ? g_qci : 0);
    if (g_ambr_dl_valid) bappend(&b, "\"ambr_dl\":\"%.3f\",", g_ambr_dl);
    else                 bappend(&b, "\"ambr_dl\":\"\",");
    if (g_ambr_ul_valid) bappend(&b, "\"ambr_ul\":\"%.3f\",", g_ambr_ul);
    else                 bappend(&b, "\"ambr_ul\":\"\",");
    emit_str(&b, "usb_mode", usb, "mode");
    bappend(&b, "},");

    /* wlan */
    bappend(&b, "\"wlan\":{");
    bappend(&b, "\"ssid\":\""); bappend_json_esc(&b, wifi_ssid); bappend(&b, "\",");
    bappend(&b, "\"key\":\"");  bappend_json_esc(&b, wifi_key);  bappend(&b, "\",");
    bappend(&b, "\"enc\":\"");  bappend_json_esc(&b, wifi_enc);  bappend(&b, "\",");
    bappend(&b, "\"enabled\":%d", wifi_enabled);
    bappend(&b, "},");

    /* nfc */
    bappend(&b, "\"nfc\":{");
    emit_int(&b, "switch", nfc, "switch", 0);
    bappend(&b, "},");

    /* dhcp */
    bappend(&b, "\"dhcp\":{");
    bappend(&b, "\"ip\":\"");        bappend_json_esc(&b, dhcp_ip);    bappend(&b, "\",");
    bappend(&b, "\"start\":\"");     bappend_json_esc(&b, dhcp_start); bappend(&b, "\",");
    bappend(&b, "\"limit\":\"");     bappend_json_esc(&b, dhcp_limit); bappend(&b, "\",");
    bappend(&b, "\"leasetime\":\""); bappend_json_esc(&b, dhcp_lease); bappend(&b, "\"");
    bappend(&b, "},");

    /* system */
    bappend(&b, "\"system\":{");
    emit_int(&b, "uptime", sysinfo, "uptime", 0);    bappend(&b, ",");
    emit_int(&b, "cpu_temp", therm, "cpuss_temp", 0); bappend(&b, ",");
    bappend(&b, "\"cpu_usage\":%d,", cpu_usage);
    bappend(&b, "\"mem_used_pct\":%ld,", mem_used_pct(sysinfo));
    bappend(&b, "\"mem_total\":%ld,", mem_field(sysinfo, "total"));
    bappend(&b, "\"mem_avail\":%ld,", mem_field(sysinfo, "available"));
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
    if (with_common) {
        char sw[128];
        if (!json_get(common_cache, "wa_inner_version", sw, sizeof sw))
            json_get(common_cache, "integrate_version", sw, sizeof sw);
        bappend(&b, ",\"sw_version\":\"");
        for (char *c = sw; *c; c++) {
            if (*c == '"' || *c == '\\') bappend(&b, "\\%c", *c);
            else if ((unsigned char)*c < 0x20) bappend(&b, " ");
            else bappend(&b, "%c", *c);
        }
        bappend(&b, "\"");
    } else {
        bappend(&b, ",\"sw_version\":\"\"");
    }
    if (with_imei) {
        bappend(&b, ",");
        emit_str(&b, "imei", imei_cache, "imei");
    } else {
        bappend(&b, ",\"imei\":\"\"");
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
    static char common[RAW_MAX];
    static char imei[256];
    run_ubus("system", "board", NULL, board, sizeof board);
    run_ubus("zwrt_zte_mdm.api", "get_zwrt_common_info", NULL, common, sizeof common);
    run_ubus("zwrt_zte_mdm.api", "get_imei", NULL, imei, sizeof imei);

    char snap[RAW_MAX * 2];
    long cycle = 0;
    do {
        if (cycle % 3600 == 0 && cycle != 0) {
            run_ubus("system", "board", NULL, board, sizeof board);
            run_ubus("zwrt_zte_mdm.api", "get_zwrt_common_info", NULL, common, sizeof common);
            run_ubus("zwrt_zte_mdm.api", "get_imei", NULL, imei, sizeof imei);
        }

        build_snapshot(snap, sizeof snap,
                       board[0] != 0, board,
                       common[0] != 0, common,
                       imei[0] != 0, imei);

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
