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

#include <arpa/inet.h>
#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#define KEY_LOG_PATH "/data/logfs/key.log"
#define SIM_POLL_MS 5000
#define QOS_RETRY_MS 120000
#define SMS_REFRESH_EVERY 10

#define HTTP_BIND_ADDR "127.0.0.1"
#define HTTP_PORT 9460
#define HTTP_MAX_CLIENTS 16
#define HTTP_REQ_MAX 4096

#define RAW_MAX 32768
#define SMS_RESPONSE_MAX 1048576
#define SMS_LIST_MAX 1048576
#define SMS_TEXT_HEX_MAX 32768
#define SMS_TEXT_UTF8_MAX 16384
#define SMS_OBJECT_MAX (SMS_TEXT_HEX_MAX + 4096)
#define SNAP_MAX 1048576

static volatile sig_atomic_t g_run = 1;
static void on_signal(int s) { (void)s; g_run = 0; }
static volatile sig_atomic_t g_qos_refresh_req = 0;
static void on_qos_signal(int s) { (void)s; g_qos_refresh_req = 1; }

static char g_sms_list_cache[SMS_LIST_MAX] = "[]";
static int g_sms_list_valid;
static long g_sms_unread_cache = 0;

struct sse_client {
    int fd;
};

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
    size_t room = b->cap - b->len;
    int n = (room > 0) ? vsnprintf(b->p + b->len, room, fmt, ap) : -1;
    va_end(ap);
    if (n <= 0) return;
    if ((size_t)n >= room) {
        b->len = b->cap - 1;
        if (b->cap > 0) b->p[b->len] = 0;
    } else {
        b->len += (size_t)n;
    }
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

static int hex_val(char c)
{
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return 10 + (c - 'a');
    if (c >= 'A' && c <= 'F') return 10 + (c - 'A');
    return -1;
}

static int read_hex_pair(const char **pp, unsigned char *out)
{
    const char *p = *pp;
    int h1, h2;

    while (*p && !isxdigit((unsigned char)*p)) p++;
    if (!*p) return 0;
    h1 = hex_val(*p++);
    while (*p && !isxdigit((unsigned char)*p)) p++;
    if (!*p) return 0;
    h2 = hex_val(*p++);
    if (h1 < 0 || h2 < 0) return 0;

    *out = (unsigned char)((h1 << 4) | h2);
    *pp = p;
    return 1;
}

static size_t append_utf8_codepoint(char *out, size_t cap, size_t pos, uint32_t cp)
{
    if (cp <= 0x7F) {
        if (pos + 1 >= cap) return pos;
        out[pos++] = (char)cp;
        return pos;
    }
    if (cp <= 0x7FF) {
        if (pos + 2 >= cap) return pos;
        out[pos++] = (char)(0xC0 | (cp >> 6));
        out[pos++] = (char)(0x80 | (cp & 0x3F));
        return pos;
    }
    if (cp <= 0xFFFF) {
        if (pos + 3 >= cap) return pos;
        out[pos++] = (char)(0xE0 | (cp >> 12));
        out[pos++] = (char)(0x80 | ((cp >> 6) & 0x3F));
        out[pos++] = (char)(0x80 | (cp & 0x3F));
        return pos;
    }
    if (cp <= 0x10FFFF) {
        if (pos + 4 >= cap) return pos;
        out[pos++] = (char)(0xF0 | (cp >> 18));
        out[pos++] = (char)(0x80 | ((cp >> 12) & 0x3F));
        out[pos++] = (char)(0x80 | ((cp >> 6) & 0x3F));
        out[pos++] = (char)(0x80 | (cp & 0x3F));
        return pos;
    }
    return pos;
}

static size_t utf16be_hex_to_utf8(const char *hex, char *out, size_t outlen)
{
    const char *p = hex ? hex : "";
    size_t pos = 0;
    uint32_t cp;
    unsigned char b1, b2;

    while (*p && pos + 1 < outlen) {
        unsigned char hi, lo;
        if (!read_hex_pair(&p, &hi)) break;
        if (!read_hex_pair(&p, &lo)) break;
        cp = ((uint32_t)hi << 8) | lo;
        if (cp >= 0xD800 && cp <= 0xDBFF) {
            const char *p2 = p;
            if (read_hex_pair(&p2, &b1) && read_hex_pair(&p2, &b2)) {
                uint32_t low = ((uint32_t)b1 << 8) | b2;
                if (low >= 0xDC00 && low <= 0xDFFF) {
                    p = p2;
                    cp = 0x10000u + ((cp - 0xD800u) << 10) + (low - 0xDC00u);
                } else {
                    cp = 0xFFFDu;
                }
            } else {
                cp = 0xFFFDu;
            }
        } else if (cp >= 0xDC00 && cp <= 0xDFFF) {
            cp = 0xFFFDu;
        }
        pos = append_utf8_codepoint(out, outlen, pos, cp);
    }
    if (outlen > 0) out[pos] = 0;
    return pos;
}

static void format_sms_date(const char *raw, char *out, size_t outlen)
{
    int mm, dd, hh, mn, ss;
    if (raw && sscanf(raw, "%*d,%d,%d,%d,%d,%d", &mm, &dd, &hh, &mn, &ss) == 5)
        snprintf(out, outlen, "%02d-%02d %02d:%02d", mm, dd, hh, mn);
    else if (outlen > 0)
        out[0] = 0;
}

static int parse_sms_list(const char *sms_reply, char *out, size_t outlen)
{
    const char *arr = strstr(sms_reply, "\"list\"");
    const char *begin = arr ? strchr(arr, '[') : strchr(sms_reply, '[');
    const char *end, *p;
    int depth = 0;
    int in_str = 0;
    int esc = 0;
    int items = 0;
    struct buf outb = { out, outlen, 0 };

    if (!begin) {
        if (outlen >= 3) memcpy(out, "[]", 2), out[2] = 0;
        return 0;
    }

    for (end = begin; *end; end++) {
        char c = *end;
        if (in_str) {
            if (esc) { esc = 0; continue; }
            if (c == '\\') esc = 1;
            else if (c == '"') in_str = 0;
            continue;
        }
        if (c == '"') in_str = 1;
        else if (c == '[') depth++;
        else if (c == ']') {
            if (depth == 0) break;
            depth--;
            if (depth == 0) { end++; break; }
        }
    }
    if (!*end || depth != 0) {
        if (outlen >= 3) memcpy(out, "[]", 2), out[2] = 0;
        return 0;
    }

    bappend(&outb, "[");
    p = begin + 1;
    while (p < end && items < 32) {
        while (p < end && *p != '{') p++;
        if (p >= end) break;

        const char *obj_start = p;
        int od = 0;
        in_str = 0;
        esc = 0;
        for (; p < end; p++) {
            char c = *p;
            if (in_str) {
                if (esc) { esc = 0; continue; }
                if (c == '\\') esc = 1;
                else if (c == '"') in_str = 0;
                continue;
            }
            if (c == '"') in_str = 1;
            else if (c == '{') od++;
            else if (c == '}') {
                od--;
                if (od == 0) { p++; break; }
            }
        }
        if (od != 0 || p > end) break;

        char sms_obj[SMS_OBJECT_MAX];
        size_t len = (size_t)(p - obj_start);
        if (len >= sizeof sms_obj) continue;

        memcpy(sms_obj, obj_start, len);
        sms_obj[len] = 0;
        {
            char id_raw[64];
            char num[64];
            char date_raw[64];
            char date[32];
            char tag[16];
            char text_hex[SMS_TEXT_HEX_MAX];
            char text[SMS_TEXT_UTF8_MAX];
            long id = 0;
            int unread = 0;

            if (!json_get(sms_obj, "id", id_raw, sizeof id_raw)) continue;
            if (!json_get(sms_obj, "num", num, sizeof num) &&
                !json_get(sms_obj, "number", num, sizeof num)) num[0] = 0;
            if (!json_get(sms_obj, "date", date_raw, sizeof date_raw)) date_raw[0] = 0;
            if (!json_get(sms_obj, "tag", tag, sizeof tag)) {
                long t = json_get_int(sms_obj, "tag", 0);
                unread = t == 1 ? 1 : 0;
            } else {
                unread = (tag[0] == '1') ? 1 : 0;
            }
            if (!json_get(sms_obj, "text", text_hex, sizeof text_hex) &&
                !json_get(sms_obj, "content", text_hex, sizeof text_hex)) text_hex[0] = 0;

            id = strtol(id_raw, NULL, 10);
            format_sms_date(date_raw, date, sizeof date);
            utf16be_hex_to_utf8(text_hex, text, sizeof text);

            if (items) bappend(&outb, ",");
            bappend(&outb, "{\"id\":%ld,\"num\":\"", id);
            bappend_json_esc(&outb, num);
            bappend(&outb, "\",\"date\":\"");
            bappend_json_esc(&outb, date);
            bappend(&outb, "\",\"unread\":%d,\"text\":\"", unread);
            bappend_json_esc(&outb, text);
            bappend(&outb, "\"}");
            items++;
        }
    }
    bappend(&outb, "]");
    if (outb.len >= outb.cap) {
        out[outlen - 1] = 0;
        return 0;
    }
    return 1;
}

static int read_sms_unread_count(long fallback)
{
    char cap[SMS_RESPONSE_MAX];
    char v[64];
    long sim = 0, dev = 0;

    if (run_ubus("zwrt_wms", "zwrt_wms_get_wms_capacity", NULL, cap, sizeof cap) != 0)
        return fallback;
    if (json_get(cap, "sms_dev_unread_num", v, sizeof v)) dev = strtol(v, NULL, 10);
    if (json_get(cap, "sms_sim_unread_num", v, sizeof v)) sim = strtol(v, NULL, 10);
    return dev + sim;
}

static int refresh_sms_cache(void)
{
    char list_resp[SMS_RESPONSE_MAX];
    static char next_cache[SMS_LIST_MAX];
    if (run_ubus("zwrt_wms", "zte_libwms_get_sms_data",
                 "{\"page\":0,\"data_per_page\":32,\"mem_store\":1,\"tags\":10,\"order_by\":\"order by id desc\"}",
                 list_resp, sizeof list_resp) != 0) {
        return 0;
    }
    if (!parse_sms_list(list_resp, next_cache, sizeof next_cache)) {
        return 0;
    }
    size_t list_len = strnlen(next_cache, sizeof next_cache);
    memcpy(g_sms_list_cache, next_cache, list_len + 1);
    g_sms_list_cache[sizeof(g_sms_list_cache) - 1] = 0;
    g_sms_list_valid = 1;
    return 1;
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
static off_t g_qos_floor_off;

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

static int parse_session_ambr_mbps(const char *s, const char *value_key,
                                   const char *unit_key, double *out)
{
    const char *p = strstr(s, value_key);
    const char *u = strstr(s, unit_key);
    char *end;
    double value, scale;

    if (!p || !u) return 0;

    p += strlen(value_key);
    value = strtod(p, &end);
    if (end == p) return 0;

    u += strlen(unit_key);
    u = strchr(u, '(');
    if (!u) return 0;
    u++;
    scale = strtod(u, &end);
    if (end == u) return 0;

    if (strstr(end, "Gbps")) scale *= 1000.0;
    else if (strstr(end, "Mbps")) scale *= 1.0;
    else if (strstr(end, "Kbps") || strstr(end, "kbps")) scale /= 1000.0;
    else if (strstr(end, "bps")) scale /= 1000000.0;
    else return 0;

    *out = value * scale;
    return 1;
}

static void clear_qos_cache(void)
{
    g_qci = 0;
    g_qci_valid = 0;
    g_ambr_dl = 0.0;
    g_ambr_ul = 0.0;
    g_ambr_dl_valid = 0;
    g_ambr_ul_valid = 0;
}

static off_t file_size_or_zero(const char *path)
{
    struct stat st;
    if (stat(path, &st) != 0 || st.st_size < 0) return 0;
    return st.st_size;
}

static void refresh_qos_cache(void)
{
    char line[2048];
    int qci;
    double dl, ul;
    FILE *fp = fopen(KEY_LOG_PATH, "r");
    off_t size = file_size_or_zero(KEY_LOG_PATH);
    off_t floor = g_qos_floor_off;

    if (!fp) return;
    if (floor > 0) {
        if (size <= 0 || floor > size) floor = 0;
        if (floor > 0 && fseeko(fp, floor, SEEK_SET) != 0) floor = 0;
        if (floor == 0) rewind(fp);
    }

    while (fgets(line, sizeof line, fp)) {
        if (!strstr(line, "[DATA]")) continue;

        if (strstr(line, "qci") && parse_int_after(line, "qci", &qci)) {
            g_qci = qci;
            g_qci_valid = 1;
        }

        if (strstr(line, "session_ambr")) {
            if (parse_session_ambr_mbps(line, "session_ambr_dl=", "session_ambr_dl_unit=", &dl)) {
                g_ambr_dl = dl;
                g_ambr_dl_valid = 1;
            }

            if (parse_session_ambr_mbps(line, "session_ambr_ul=", "session_ambr_ul_unit=", &ul)) {
                g_ambr_ul = ul;
                g_ambr_ul_valid = 1;
            }
        }

        if (strstr(line, "apn_ambr")) {
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
    }
    fclose(fp);
    g_qos_floor_off = size;
}

static int read_sim_signature(char *out, size_t outlen)
{
    char sim[RAW_MAX];
    char iccid[64], slot[16], imsi[32], state[32];

    out[0] = 0;
    if (run_ubus("zwrt_zte_mdm.api", "get_sim_info", NULL, sim, sizeof sim) != 0)
        return 0;

    if (!json_get(sim, "sim_iccid", iccid, sizeof iccid)) iccid[0] = 0;
    if (!json_get(sim, "current_sim_slot", slot, sizeof slot)) slot[0] = 0;
    if (!json_get(sim, "sim_imsi", imsi, sizeof imsi)) imsi[0] = 0;
    if (!json_get(sim, "sim_states", state, sizeof state)) state[0] = 0;

    if (!iccid[0] && !slot[0] && !imsi[0] && !state[0]) return 0;
    snprintf(out, outlen, "slot=%s|iccid=%s|imsi=%s|state=%s",
             slot[0] ? slot : "-",
             iccid[0] ? iccid : "-",
             imsi[0] ? imsi : "-",
             state[0] ? state : "-");
    return 1;
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

    /* sms */
    bappend(&b, "\"sms\":{");
    bappend(&b, "\"unread\":%ld,", g_sms_unread_cache);
    bappend(&b, "\"list\":%s", g_sms_list_valid ? g_sms_list_cache : "[]");
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

static int set_nonblock(int fd)
{
    int fl = fcntl(fd, F_GETFL, 0);
    if (fl < 0) return -1;
    return fcntl(fd, F_SETFL, fl | O_NONBLOCK);
}

static int wait_readable(int fd, int timeout_ms)
{
    fd_set rfds;
    struct timeval tv;
    int rc;

    FD_ZERO(&rfds);
    FD_SET(fd, &rfds);
    tv.tv_sec = timeout_ms / 1000;
    tv.tv_usec = (timeout_ms % 1000) * 1000;

    do {
        rc = select(fd + 1, &rfds, NULL, NULL, &tv);
    } while (rc < 0 && errno == EINTR && g_run);

    return rc > 0;
}

static int write_all(int fd, const char *buf, size_t len)
{
    size_t off = 0;
    while (off < len) {
        ssize_t n = write(fd, buf + off, len - off);
        if (n > 0) {
            off += (size_t)n;
            continue;
        }
        if (n < 0 && errno == EINTR) continue;
        return -1;
    }
    return 0;
}

static void sse_client_close(struct sse_client *c)
{
    if (c->fd >= 0) close(c->fd);
    c->fd = -1;
}

static int read_http_request(int fd, char *buf, size_t cap, int timeout_ms)
{
    size_t len = 0;

    while (len + 1 < cap) {
        if (!wait_readable(fd, timeout_ms)) return -1;

        ssize_t n = read(fd, buf + len, cap - 1 - len);
        if (n == 0) return -1;
        if (n < 0) {
            if (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK) continue;
            return -1;
        }

        len += (size_t)n;
        buf[len] = 0;
        if (strstr(buf, "\r\n\r\n")) return 0;
    }

    return -1;
}

static int parse_request_line(const char *req, char *method, size_t method_cap,
                              char *path, size_t path_cap)
{
    if (sscanf(req, "%15s %255s", method, path) != 2) return -1;
    method[method_cap - 1] = 0;
    path[path_cap - 1] = 0;

    char *q = strchr(path, '?');
    if (q) *q = 0;
    return 0;
}

static void write_http_error(int fd, int code, const char *text)
{
    char buf[256];
    int n = snprintf(buf, sizeof buf,
                     "HTTP/1.1 %d %s\r\n"
                     "Connection: close\r\n"
                     "Content-Length: 0\r\n"
                     "\r\n",
                     code, text);
    if (n > 0) (void)write_all(fd, buf, (size_t)n);
}

static int write_http_text(int fd, const char *ctype, const char *body)
{
    char hdr[256];
    size_t body_len = strlen(body);
    int n = snprintf(hdr, sizeof hdr,
                     "HTTP/1.1 200 OK\r\n"
                     "Content-Type: %s\r\n"
                     "Cache-Control: no-store\r\n"
                     "Connection: close\r\n"
                     "Content-Length: %zu\r\n"
                     "\r\n",
                     ctype, body_len);
    if (n <= 0) return -1;
    if (write_all(fd, hdr, (size_t)n) < 0) return -1;
    return write_all(fd, body, body_len);
}

static int write_http_json(int fd, const char *snap, size_t snap_len)
{
    char hdr[256];
    int n = snprintf(hdr, sizeof hdr,
                     "HTTP/1.1 200 OK\r\n"
                     "Content-Type: application/json; charset=utf-8\r\n"
                     "Cache-Control: no-store\r\n"
                     "Connection: close\r\n"
                     "Content-Length: %zu\r\n"
                     "\r\n",
                     snap_len);
    if (n <= 0) return -1;
    if (write_all(fd, hdr, (size_t)n) < 0) return -1;
    return write_all(fd, snap, snap_len);
}

static int write_sse_handshake(int fd)
{
    static const char hdr[] =
        "HTTP/1.1 200 OK\r\n"
        "Content-Type: text/event-stream\r\n"
        "Cache-Control: no-cache\r\n"
        "Connection: keep-alive\r\n"
        "X-Accel-Buffering: no\r\n"
        "\r\n"
        "retry: 1000\n\n";
    return write_all(fd, hdr, sizeof hdr - 1);
}

static int sse_send_snapshot(int fd, const char *snap, size_t snap_len)
{
    static const char prefix[] = "event: state\ndata: ";
    static const char suffix[] = "\n\n";
    if (write_all(fd, prefix, sizeof prefix - 1) < 0) return -1;
    if (write_all(fd, snap, snap_len) < 0) return -1;
    return write_all(fd, suffix, sizeof suffix - 1);
}

static int open_server_socket(const char *bind_addr, int port)
{
    struct sockaddr_in addr;
    int fd;
    int one = 1;

    if (port <= 0 || port > 65535) {
        fprintf(stderr, "invalid port: %d\n", port);
        return -1;
    }

    fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        perror("socket");
        return -1;
    }

    if (setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one) < 0) {
        perror("setsockopt");
        close(fd);
        return -1;
    }

    memset(&addr, 0, sizeof addr);
    addr.sin_family = AF_INET;
    addr.sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, bind_addr, &addr.sin_addr) != 1) {
        fprintf(stderr, "invalid bind address: %s\n", bind_addr);
        close(fd);
        return -1;
    }

    if (bind(fd, (struct sockaddr *)&addr, sizeof addr) < 0) {
        perror("bind");
        close(fd);
        return -1;
    }
    if (listen(fd, 8) < 0) {
        perror("listen");
        close(fd);
        return -1;
    }
    if (set_nonblock(fd) < 0) {
        perror("fcntl");
        close(fd);
        return -1;
    }

    return fd;
}

static void drain_or_close_sse_client(struct sse_client *c)
{
    char buf[256];
    ssize_t n = read(c->fd, buf, sizeof buf);
    if (n <= 0) {
        if (n == 0 || (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR))
            sse_client_close(c);
        return;
    }

    /* SSE is push-only here; if a client speaks back we drop it. */
    sse_client_close(c);
}

static void broadcast_sse_snapshot(struct sse_client *clients, size_t nclients,
                                   const char *snap, size_t snap_len)
{
    for (size_t i = 0; i < nclients; i++) {
        if (clients[i].fd < 0) continue;
        if (sse_send_snapshot(clients[i].fd, snap, snap_len) < 0)
            sse_client_close(&clients[i]);
    }
}

static void accept_ready_http_clients(int srv_fd, struct sse_client *clients, size_t nclients,
                                      const char *snap, size_t snap_len)
{
    char req[HTTP_REQ_MAX];
    char method[16];
    char path[256];

    for (;;) {
        int cli_fd = accept(srv_fd, NULL, NULL);
        if (cli_fd < 0) {
            if (errno == EINTR) continue;
            if (errno == EAGAIN || errno == EWOULDBLOCK) return;
            perror("accept");
            return;
        }

        if (read_http_request(cli_fd, req, sizeof req, 2000) < 0 ||
            parse_request_line(req, method, sizeof method, path, sizeof path) < 0) {
            write_http_error(cli_fd, 400, "Bad Request");
            close(cli_fd);
            continue;
        }

        if (strcmp(method, "GET") != 0) {
            write_http_error(cli_fd, 405, "Method Not Allowed");
            close(cli_fd);
            continue;
        }

        if (!strcmp(path, "/state")) {
            (void)write_http_json(cli_fd, snap, snap_len);
            close(cli_fd);
            continue;
        }

        if (!strcmp(path, "/healthz")) {
            (void)write_http_text(cli_fd, "text/plain; charset=utf-8", "ok\n");
            close(cli_fd);
            continue;
        }

        if (!strcmp(path, "/") || !strcmp(path, "/index") || !strcmp(path, "/index.txt")) {
            (void)write_http_text(cli_fd, "text/plain; charset=utf-8",
                                  "u60-datad dev HTTP API\n"
                                  "GET /state   -> current JSON snapshot\n"
                                  "GET /events  -> SSE stream\n"
                                  "GET /healthz -> ok\n");
            close(cli_fd);
            continue;
        }

        if (!strcmp(path, "/events")) {
            int slot = -1;
            for (size_t i = 0; i < nclients; i++) {
                if (clients[i].fd < 0) {
                    slot = (int)i;
                    break;
                }
            }
            if (slot < 0) {
                write_http_error(cli_fd, 503, "Too Many Clients");
                close(cli_fd);
                continue;
            }
            if (write_sse_handshake(cli_fd) < 0 ||
                sse_send_snapshot(cli_fd, snap, snap_len) < 0 ||
                set_nonblock(cli_fd) < 0) {
                close(cli_fd);
                continue;
            }
            clients[slot].fd = cli_fd;
            continue;
        }

        write_http_error(cli_fd, 404, "Not Found");
        close(cli_fd);
    }
}

static void wait_with_http(int srv_fd, struct sse_client *clients, size_t nclients,
                           const char *snap, size_t snap_len, int wait_ms)
{
    struct timespec deadline;
    clock_gettime(CLOCK_MONOTONIC, &deadline);
    deadline.tv_sec += wait_ms / 1000;
    deadline.tv_nsec += (long)(wait_ms % 1000) * 1000000L;
    if (deadline.tv_nsec >= 1000000000L) {
        deadline.tv_sec += 1;
        deadline.tv_nsec -= 1000000000L;
    }

    while (g_run) {
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);

        long sec = deadline.tv_sec - now.tv_sec;
        long nsec = deadline.tv_nsec - now.tv_nsec;
        if (nsec < 0) {
            sec -= 1;
            nsec += 1000000000L;
        }
        if (sec < 0 || (sec == 0 && nsec <= 0)) return;

        fd_set rfds;
        struct timeval tv;
        int rc, maxfd = srv_fd;

        FD_ZERO(&rfds);
        FD_SET(srv_fd, &rfds);
        for (size_t i = 0; i < nclients; i++) {
            if (clients[i].fd < 0) continue;
            FD_SET(clients[i].fd, &rfds);
            if (clients[i].fd > maxfd) maxfd = clients[i].fd;
        }

        tv.tv_sec = sec;
        tv.tv_usec = nsec / 1000;
        do {
            rc = select(maxfd + 1, &rfds, NULL, NULL, &tv);
        } while (rc < 0 && errno == EINTR && g_run);

        if (rc < 0) {
            perror("select");
            return;
        }
        if (rc == 0) continue;

        if (FD_ISSET(srv_fd, &rfds))
            accept_ready_http_clients(srv_fd, clients, nclients, snap, snap_len);
        for (size_t i = 0; i < nclients; i++) {
            if (clients[i].fd >= 0 && FD_ISSET(clients[i].fd, &rfds))
                drain_or_close_sse_client(&clients[i]);
        }
    }
}

/* Return pointer to the first byte after the leading {"ts":... , */
static const char *json_skip_ts(const char *snap, size_t len, size_t *out_len)
{
    const char *p;
    if (!snap || len < 6 || !out_len) return NULL;

    p = snap;
    while (*p && isspace((unsigned char)*p)) p++;
    if (*p != '{') return NULL;

    p++;
    while (*p && isspace((unsigned char)*p)) p++;
    if (strncmp(p, "\"ts\":", 5) != 0) return NULL;

    p = strchr(p, ',');
    if (!p) return NULL;
    p++;

    while (*p && isspace((unsigned char)*p)) p++;
    *out_len = len - (size_t)(p - snap);
    return p;
}

int main(int argc, char **argv)
{
    int once = 0, interval_ms = 1000;
    const char *bind_addr = HTTP_BIND_ADDR;
    int port = HTTP_PORT;
    int sim_poll_every, qos_retry_every, qos_retry_left = 0;
    char sim_sig[160];
    int sim_sig_valid;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--once")) once = 1;
        else if (!strcmp(argv[i], "-i") && i + 1 < argc) interval_ms = atoi(argv[++i]);
        else if ((!strcmp(argv[i], "-b") || !strcmp(argv[i], "--bind")) && i + 1 < argc)
            bind_addr = argv[++i];
        else if ((!strcmp(argv[i], "-p") || !strcmp(argv[i], "--port")) && i + 1 < argc)
            port = atoi(argv[++i]);
    }
    if (interval_ms <= 0) interval_ms = 1000;

    sim_poll_every = (SIM_POLL_MS + interval_ms - 1) / interval_ms;
    qos_retry_every = (SIM_POLL_MS + interval_ms - 1) / interval_ms;
    if (sim_poll_every < 1) sim_poll_every = 1;
    if (qos_retry_every < 1) qos_retry_every = 1;

    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);
    signal(SIGUSR1, on_qos_signal);
    signal(SIGPIPE, SIG_IGN);

    int srv_fd = -1;
    struct sse_client clients[HTTP_MAX_CLIENTS];
    for (size_t i = 0; i < HTTP_MAX_CLIENTS; i++) clients[i].fd = -1;

    if (!once) {
        srv_fd = open_server_socket(bind_addr, port);
        if (srv_fd < 0) return 1;
    }

    /* board info changes rarely: fetch once, refresh hourly. */
    static char board[RAW_MAX];
    static char common[RAW_MAX];
    static char imei[256];
    run_ubus("system", "board", NULL, board, sizeof board);
    run_ubus("zwrt_zte_mdm.api", "get_zwrt_common_info", NULL, common, sizeof common);
    run_ubus("zwrt_zte_mdm.api", "get_imei", NULL, imei, sizeof imei);
    clear_qos_cache();
    g_qos_floor_off = 0;
    refresh_qos_cache();
    sim_sig_valid = read_sim_signature(sim_sig, sizeof sim_sig);

    char snap[SNAP_MAX];
    static char last_snap[SNAP_MAX];
    static size_t last_snap_len;
    long cycle = 0;
    long last_sms_unread = read_sms_unread_count(g_sms_unread_cache);
    if (last_sms_unread >= 0) g_sms_unread_cache = last_sms_unread;

    do {
        if (cycle % 3600 == 0 && cycle != 0) {
            run_ubus("system", "board", NULL, board, sizeof board);
            run_ubus("zwrt_zte_mdm.api", "get_zwrt_common_info", NULL, common, sizeof common);
            run_ubus("zwrt_zte_mdm.api", "get_imei", NULL, imei, sizeof imei);
        }

        if (cycle == 0 || cycle % sim_poll_every == 0) {
            char cur_sig[160];
            int cur_valid = read_sim_signature(cur_sig, sizeof cur_sig);
            if (cur_valid != sim_sig_valid || (cur_valid && strcmp(cur_sig, sim_sig) != 0)) {
                if (cur_valid) snprintf(sim_sig, sizeof sim_sig, "%s", cur_sig);
                else sim_sig[0] = 0;
                sim_sig_valid = cur_valid;
                g_qos_floor_off = file_size_or_zero(KEY_LOG_PATH);
                clear_qos_cache();
                g_qos_refresh_req = 1;
                g_sms_list_valid = 0;
                qos_retry_left = (QOS_RETRY_MS + interval_ms - 1) / interval_ms;
            }
        }

        if (g_qos_refresh_req) {
            g_qos_refresh_req = 0;
            refresh_qos_cache();
            if (g_qci_valid || g_ambr_dl_valid || g_ambr_ul_valid) qos_retry_left = 0;
        } else if (qos_retry_left > 0) {
            if (qos_retry_left == 1 || (qos_retry_left % qos_retry_every) == 0)
                refresh_qos_cache();
            if (g_qci_valid || g_ambr_dl_valid || g_ambr_ul_valid) qos_retry_left = 0;
            else qos_retry_left--;
        }

        {
            long cur_sms_unread = read_sms_unread_count(g_sms_unread_cache);
            if (cur_sms_unread >= 0) g_sms_unread_cache = cur_sms_unread;
            if (!g_sms_list_valid || cycle % SMS_REFRESH_EVERY == 0 || g_sms_unread_cache != last_sms_unread) {
                if (refresh_sms_cache()) g_sms_list_valid = 1;
            }
            last_sms_unread = g_sms_unread_cache;
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
        {
            size_t snap_len = strlen(snap);
            int changed = 1;
            if (snap_len == last_snap_len) {
                changed = memcmp(last_snap, snap, snap_len) != 0;
            }
            if (changed) {
                size_t cur_payload_len = 0, last_payload_len = 0;
                const char *cur_payload = json_skip_ts(snap, snap_len, &cur_payload_len);
                const char *last_payload = last_snap_len ? json_skip_ts(last_snap, last_snap_len, &last_payload_len) : NULL;
                if (cur_payload && last_payload && cur_payload_len == last_payload_len &&
                    memcmp(cur_payload, last_payload, cur_payload_len) == 0) {
                    changed = 0;
                }
            }

            if (changed) {
                broadcast_sse_snapshot(clients, HTTP_MAX_CLIENTS, snap, snap_len);
                memcpy(last_snap, snap, snap_len + 1);
                last_snap_len = snap_len;
            }

            wait_with_http(srv_fd, clients, HTTP_MAX_CLIENTS, snap, snap_len, interval_ms);
        }
        cycle++;
    } while (g_run);

    for (size_t i = 0; i < HTTP_MAX_CLIENTS; i++) sse_client_close(&clients[i]);
    if (srv_fd >= 0) close(srv_fd);
    return 0;
}
