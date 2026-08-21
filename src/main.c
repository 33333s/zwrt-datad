/*
 * zwrt-datad - unified device-state aggregator for OpenWRT device plugins.
 *
 * Single producer: polls a fixed set of ubus getters at a controlled rate,
 * normalizes them into one flat JSON snapshot, and serves it over HTTP/SSE.
 * Any number of consumers (devui, web, scripts) share that single producer,
 * so ubus load is decoupled from consumer count. No vendor libs.
 *
 * SPDX-License-Identifier: MIT
 */
#include "json.h"
#include "control.h"
#include "device_exec.h"
#include "system_ext.h"

#include <arpa/inet.h>
#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <stdint.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#define KEY_LOG_PATH "/data/logfs/key.log"
#define KEY_LOG_ROTATED_PATH "/data/logfs/key.log.0"
#define SIM_POLL_MS 5000
#define QOS_RETRY_MS 120000
#define SMS_REFRESH_EVERY 10

#define HTTP_BIND_ADDR "127.0.0.1"
#define HTTP_PORT 9460
#define HTTP_LAN_PORT 9461
#define HTTP_AUTH_TOKEN_FILE "/data/plugins/zwrt-datad/auth.token"
#define HTTP_MAX_CLIENTS 16
#define HTTP_REQ_MAX 65536
#define HTTP_PATH_MAX 1024
#define HTTP_TOKEN_MAX 256
#define HTTP_AUTH_SUBJECT_MAX 64
#define HTTP_AUTH_PARAM_MAX 256
#define HTTP_AUTH_HEADER_MAX 512
#define HTTP_AUTH_REPLY_MAX 1024
#define HTTP_AUTH_ARG_MAX 768
#define HTTP_AUTH_SESSIONS 16
#define HTTP_SESSION_TOKEN_BYTES 24
#define HTTP_SESSION_TTL_SEC (12 * 60 * 60)

#define RAW_MAX 32768
#define SMS_RESPONSE_MAX 1048576
#define SMS_LIST_MAX 1048576
#define SMS_TEXT_HEX_MAX 32768
#define SMS_TEXT_UTF8_MAX 16384
#define SMS_OBJECT_MAX (SMS_TEXT_HEX_MAX + 4096)
#define CLIENT_LIST_MAX 16384
#define SNAP_MAX 1048576

static volatile sig_atomic_t g_run = 1;
static void on_signal(int s) { (void)s; g_run = 0; }
static volatile sig_atomic_t g_qos_refresh_req = 0;
static void on_qos_signal(int s) { (void)s; g_qos_refresh_req = 1; }
static volatile sig_atomic_t g_state_refresh_req = 0;

static char g_sms_list_cache[SMS_LIST_MAX] = "[]";
static int g_sms_list_valid;
static long g_sms_unread_cache = 0;
static char g_sim_cache[RAW_MAX];
static char g_lan_interface[RAW_MAX];
static char g_wan4_interface[RAW_MAX];
static char g_wan6_interface[RAW_MAX];
static char g_lan_runtime[RAW_MAX];
static char g_cellular_runtime[RAW_MAX];
static char g_traffic_accounting[RAW_MAX];
static char g_traffic_limit[4096];
static char g_traffic_clear_day[4096];

struct sse_client {
    int fd;
};

struct http_listener {
    int fd;
    int auth_required;
    int lan_only;
};

struct auth_session {
    int active;
    time_t expires_at;
    char token[HTTP_TOKEN_MAX];
    char subject[HTTP_AUTH_SUBJECT_MAX];
};

struct auth_state {
    char static_token[HTTP_TOKEN_MAX];
    struct auth_session sessions[HTTP_AUTH_SESSIONS];
};

enum wifi_source_mode {
    WIFI_SOURCE_U60_MAIN_2G = 0,
    WIFI_SOURCE_COMPAT_AUTO = 1
};

enum client_source_mode {
    CLIENT_SOURCE_DHCP_ONLY = 0,
    CLIENT_SOURCE_DHCP_THEN_ROUTER = 1
};

enum temp_source_mode {
    TEMP_SOURCE_U60_UBUS_ONLY = 0,
    TEMP_SOURCE_COMPAT_FALLBACK = 1
};

enum network_source_mode {
    NETWORK_SOURCE_NWINFO_UBUS_ONLY = 0,
    NETWORK_SOURCE_MU5252_UCI_FALLBACK = 1
};

enum traffic_source_mode {
    TRAFFIC_SOURCE_CID1 = 0,
    TRAFFIC_SOURCE_CID1_ACTIVE_SUBID = 1
};

struct device_template_spec {
    const char *id;
    const char *label;
    int supported;
    enum wifi_source_mode wifi_mode;
    enum client_source_mode client_mode;
    enum temp_source_mode temp_mode;
    enum network_source_mode network_mode;
    enum traffic_source_mode traffic_mode;
};

static const struct device_template_spec TEMPLATE_U60_MU5250 = {
    "MU5250",
    "MU5250",
    1,
    WIFI_SOURCE_U60_MAIN_2G,
    CLIENT_SOURCE_DHCP_ONLY,
    TEMP_SOURCE_U60_UBUS_ONLY,
    NETWORK_SOURCE_NWINFO_UBUS_ONLY,
    TRAFFIC_SOURCE_CID1
};

static const struct device_template_spec TEMPLATE_G5PRO_MC8532B = {
    "MC8532B",
    "MC8532B",
    1,
    WIFI_SOURCE_COMPAT_AUTO,
    CLIENT_SOURCE_DHCP_THEN_ROUTER,
    TEMP_SOURCE_COMPAT_FALLBACK,
    NETWORK_SOURCE_NWINFO_UBUS_ONLY,
    TRAFFIC_SOURCE_CID1
};

static const struct device_template_spec TEMPLATE_TOPFLOW_MU5252 = {
    "MU5252",
    "MU5252",
    1,
    WIFI_SOURCE_COMPAT_AUTO,
    CLIENT_SOURCE_DHCP_THEN_ROUTER,
    TEMP_SOURCE_U60_UBUS_ONLY,
    NETWORK_SOURCE_MU5252_UCI_FALLBACK,
    TRAFFIC_SOURCE_CID1_ACTIVE_SUBID
};

static const struct device_template_spec TEMPLATE_LEGACY_COMPAT = {
    "legacy_compat",
    "Legacy compatibility fallback",
    0,
    WIFI_SOURCE_COMPAT_AUTO,
    CLIENT_SOURCE_DHCP_THEN_ROUTER,
    TEMP_SOURCE_COMPAT_FALLBACK,
    NETWORK_SOURCE_NWINFO_UBUS_ONLY,
    TRAFFIC_SOURCE_CID1
};

/* Run `ubus call <svc> <method> [args]` and capture stdout. 0 on output. */
static int run_ubus(const char *svc, const char *method, const char *args,
                    char *out, size_t outlen)
{
    return device_ubus_call(svc, method, args, out, outlen);
}

static void copy_text(char *dst, size_t dstlen, const char *src)
{
    size_t n;
    if (!dst || dstlen == 0) return;
    if (!src) src = "";
    n = strlen(src);
    if (n >= dstlen) n = dstlen - 1;
    memmove(dst, src, n);
    dst[n] = 0;
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

static void emit_kv_str(struct buf *b, const char *key, const char *value)
{
    bappend(b, "\"%s\":\"", key);
    bappend_json_esc(b, value ? value : "");
    bappend(b, "\"");
}

static void emit_int(struct buf *b, const char *key, const char *src, const char *srckey, long def)
{
    bappend(b, "\"%s\":%ld", key, json_get_int(src, srckey, def));
}

static void emit_json_value(struct buf *b, const char *key, const char *src,
                            const char *srckey, const char *fallback)
{
    char value[RAW_MAX];
    bappend(b, "\"%s\":", key);
    if (src && *src && json_get(src, srckey, value, sizeof value) && value[0])
        bappend(b, "%s", value);
    else
        bappend(b, "%s", fallback);
}

static void emit_interface_status(struct buf *b, const char *name, const char *src)
{
    char up[16];
    bappend(b, "\"%s\":{", name);
    if (src && json_get(src, "up", up, sizeof up))
        bappend(b, "\"up\":%s,", !strcmp(up, "true") || !strcmp(up, "1") ? "true" : "false");
    else bappend(b, "\"up\":false,");
    emit_str(b, "proto", src, "proto"); bappend(b, ",");
    emit_str(b, "device", src, "l3_device"); bappend(b, ",");
    emit_json_value(b, "ipv4", src, "ipv4-address", "[]"); bappend(b, ",");
    emit_json_value(b, "ipv6", src, "ipv6-address", "[]"); bappend(b, ",");
    emit_json_value(b, "dns", src, "dns-server", "[]");
    bappend(b, "}");
}

static void normalize_profile_token(const char *src, char *out, size_t outlen)
{
    size_t n = 0;
    int prev_sep = 1;

    if (!outlen) return;
    out[0] = 0;
    if (!src) return;

    for (const unsigned char *p = (const unsigned char *)src; *p; p++) {
        if (isalnum(*p)) {
            if (n + 1 >= outlen) break;
            out[n++] = (char)tolower(*p);
            prev_sep = 0;
        } else if (!prev_sep && (*p == '-' || *p == '_' || *p == ' ' || *p == '/' || *p == '.')) {
            if (n + 1 >= outlen) break;
            out[n++] = '_';
            prev_sep = 1;
        }
    }

    while (n > 0 && out[n - 1] == '_') n--;
    out[n] = 0;
}

static int has_prefix(const char *s, const char *prefix)
{
    size_t n;
    if (!s || !prefix) return 0;
    n = strlen(prefix);
    return strncmp(s, prefix, n) == 0;
}

static void detect_device_identity(int with_board, const char *board_cache,
                                   int with_common, const char *common_cache,
                                   char *profile, size_t profile_n,
                                   char *profile_source, size_t profile_source_n,
                                   char *vendor, size_t vendor_n,
                                   char *model_name, size_t model_name_n,
                                   char *hardware_version, size_t hardware_version_n,
                                   char *market_name, size_t market_name_n,
                                   char *alias_name, size_t alias_name_n,
                                   char *board_name, size_t board_name_n)
{
    char hw_model[128];
    const char *source_value = NULL;
    const char *source_name = "unknown";

    if (profile_n) profile[0] = 0;
    if (profile_source_n) profile_source[0] = 0;
    if (vendor_n) vendor[0] = 0;
    if (model_name_n) model_name[0] = 0;
    if (hardware_version_n) hardware_version[0] = 0;
    if (market_name_n) market_name[0] = 0;
    if (alias_name_n) alias_name[0] = 0;
    if (board_name_n) board_name[0] = 0;
    if (sizeof hw_model) hw_model[0] = 0;

    if (with_common) {
        if (!json_get(common_cache, "manufacturer", vendor, vendor_n)) vendor[0] = 0;
        if (!json_get(common_cache, "model_name", model_name, model_name_n)) model_name[0] = 0;
        if (!json_get(common_cache, "hardware_version", hardware_version, hardware_version_n)) hardware_version[0] = 0;
        if (!json_get(common_cache, "device_market_name", market_name, market_name_n)) market_name[0] = 0;
        if (!json_get(common_cache, "device_alias_name", alias_name, alias_name_n)) alias_name[0] = 0;
    }
    if (with_board) {
        if (!json_get(board_cache, "board_name", board_name, board_name_n)) board_name[0] = 0;
    }

    if (hardware_version[0]) {
        size_t i = 0;
        while (hardware_version[i] &&
               hardware_version[i] != '_' &&
               i + 1 < sizeof hw_model) {
            hw_model[i] = hardware_version[i];
            i++;
        }
        hw_model[i] = 0;
    }

    if (model_name[0]) {
        source_value = model_name;
        source_name = "model_name";
    } else if (hw_model[0]) {
        source_value = hw_model;
        source_name = "hardware_version";
    } else if (board_name[0]) {
        source_value = board_name;
        source_name = "board_name";
    } else if (market_name[0]) {
        source_value = market_name;
        source_name = "device_market_name";
    } else if (alias_name[0]) {
        source_value = alias_name;
        source_name = "device_alias_name";
    }

    if (source_value && *source_value) {
        normalize_profile_token(source_value, profile, profile_n);
    }
    if (!profile[0] && profile_n > 0) {
        snprintf(profile, profile_n, "unknown");
        source_name = "unknown";
    }
    if (profile_source_n > 0) {
        snprintf(profile_source, profile_source_n, "%s", source_name);
    }
}

static const struct device_template_spec *
select_device_template(const char *model_name, const char *hardware_version)
{
    if ((model_name && !strcmp(model_name, "MU5250")) ||
        has_prefix(hardware_version, "MU5250_")) {
        return &TEMPLATE_U60_MU5250;
    }
    if ((model_name && !strcmp(model_name, "MC8532B")) ||
        has_prefix(hardware_version, "MC8532B")) {
        return &TEMPLATE_G5PRO_MC8532B;
    }
    if ((model_name && !strcmp(model_name, "MU5252")) ||
        has_prefix(hardware_version, "MU5252_")) {
        return &TEMPLATE_TOPFLOW_MU5252;
    }
    return &TEMPLATE_LEGACY_COMPAT;
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
#define QOS_CANDIDATE_MAX 32
struct qos_candidate {
    int used;
    /* A value seen in key.log must take precedence over rotated key.log.0. */
    int current_log;
    int has_plmn;
    int mcc;
    int mnc;
    int rank;
    unsigned long seq;
    int qci;
    int qci_valid;
    double ambr_dl;
    double ambr_ul;
    int ambr_dl_valid;
    int ambr_ul_valid;
};
struct qos_values {
    int qci;
    int qci_valid;
    double ambr_dl;
    double ambr_ul;
    int ambr_dl_valid;
    int ambr_ul_valid;
};
static struct qos_candidate g_qos_candidates[QOS_CANDIDATE_MAX];
static unsigned long g_qos_seq;
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

static int ascii_lower(int c)
{
    if (c >= 'A' && c <= 'Z') return c - 'A' + 'a';
    return c;
}

static const char *find_ci(const char *s, const char *needle);

static int contains_ci(const char *s, const char *needle)
{
    if (!s || !needle) return 0;
    return find_ci(s, needle) != NULL;
}

static const char *find_ci(const char *s, const char *needle)
{
    size_t nlen;

    if (!s || !needle) return 0;
    nlen = strlen(needle);
    if (nlen == 0) return s;
    for (; *s; s++) {
        size_t i;
        for (i = 0; i < nlen; i++) {
            if (!s[i] || ascii_lower((unsigned char)s[i]) != ascii_lower((unsigned char)needle[i]))
                break;
        }
        if (i == nlen) return s;
    }
    return NULL;
}

static int qos_line_is_non_data_dnn(const char *line)
{
    return contains_ci(line, "dnn=ims") ||
           contains_ci(line, "dnn=sos") ||
           contains_ci(line, "dnn=emergency") ||
           contains_ci(line, "access_point=ims");
}

static int qos_line_has_data_context(const char *line)
{
    if (qos_line_is_non_data_dnn(line)) return 0;
    return contains_ci(line, "dnn=") || contains_ci(line, "access_point=");
}

static int parse_digits_after_ci(const char *s, const char *tag, int *out)
{
    const char *p = s;

    while ((p = find_ci(p, tag)) != NULL) {
        int value = 0;
        int digits = 0;

        p += strlen(tag);
        if (*p < '0' || *p > '9') continue;
        while (*p >= '0' && *p <= '9' && digits < 6) {
            value = value * 10 + (*p - '0');
            p++;
            digits++;
        }
        if (digits > 0) {
            *out = value;
            return 1;
        }
    }
    return 0;
}

static int qos_line_plmn(const char *line, int *mcc, int *mnc)
{
    int parsed_mcc = 0;
    int parsed_mnc = 0;

    if (!parse_digits_after_ci(line, "mcc", &parsed_mcc)) return 0;
    if (!parse_digits_after_ci(line, "mnc", &parsed_mnc)) return 0;
    *mcc = parsed_mcc;
    *mnc = parsed_mnc;
    return parsed_mcc > 0;
}

static int qos_context_rank(const char *line, int *mcc, int *mnc, int *has_plmn)
{
    int rank = 0;

    if (!qos_line_has_data_context(line)) return 0;

    *mcc = 0;
    *mnc = 0;
    *has_plmn = qos_line_plmn(line, mcc, mnc);
    if (contains_ci(line, "access_point=")) rank = *has_plmn ? 30 : 20;
    else if (contains_ci(line, "dnn=")) rank = *has_plmn ? 30 : 10;
    return rank;
}

static int qos_candidate_has_values(const struct qos_candidate *cand)
{
    return cand->qci_valid || cand->ambr_dl_valid || cand->ambr_ul_valid;
}

static int qos_get_candidate(int has_plmn, int mcc, int mnc, int rank,
                             int current_log)
{
    int i;
    int free_idx = -1;
    int replace = -1;

    for (i = 0; i < QOS_CANDIDATE_MAX; i++) {
        struct qos_candidate *cand = &g_qos_candidates[i];

        if (!cand->used) {
            if (free_idx < 0) free_idx = i;
            continue;
        }
        if (cand->has_plmn == has_plmn &&
            (!has_plmn || (cand->mcc == mcc && cand->mnc == mnc))) {
            if (rank > cand->rank) cand->rank = rank;
            if (current_log) cand->current_log = 1;
            cand->seq = ++g_qos_seq;
            return i;
        }
        if (replace < 0 ||
            cand->rank < g_qos_candidates[replace].rank ||
            (cand->rank == g_qos_candidates[replace].rank &&
             cand->seq < g_qos_candidates[replace].seq)) {
            replace = i;
        }
    }

    if (free_idx >= 0) replace = free_idx;
    if (replace < 0) return -1;
    memset(&g_qos_candidates[replace], 0, sizeof g_qos_candidates[replace]);
    g_qos_candidates[replace].used = 1;
    g_qos_candidates[replace].has_plmn = has_plmn;
    g_qos_candidates[replace].mcc = has_plmn ? mcc : 0;
    g_qos_candidates[replace].mnc = has_plmn ? mnc : 0;
    g_qos_candidates[replace].rank = rank;
    g_qos_candidates[replace].current_log = current_log;
    g_qos_candidates[replace].seq = ++g_qos_seq;
    return replace;
}

static int qos_candidate_from_line(const char *line, int current_log)
{
    int mcc = 0;
    int mnc = 0;
    int has_plmn = 0;
    int rank = qos_context_rank(line, &mcc, &mnc, &has_plmn);

    if (rank <= 0) return -1;
    return qos_get_candidate(has_plmn, mcc, mnc, rank, current_log);
}

static void qos_candidate_set_qci(int idx, int qci)
{
    if (idx >= 0 && idx < QOS_CANDIDATE_MAX && g_qos_candidates[idx].used) {
        g_qos_candidates[idx].qci = qci;
        g_qos_candidates[idx].qci_valid = 1;
        g_qos_candidates[idx].seq = ++g_qos_seq;
    }
    g_qci = qci;
    g_qci_valid = 1;
}

static void qos_candidate_set_ambr(int idx, double dl, int dl_valid,
                                   double ul, int ul_valid)
{
    if (idx >= 0 && idx < QOS_CANDIDATE_MAX && g_qos_candidates[idx].used) {
        if (dl_valid) {
            g_qos_candidates[idx].ambr_dl = dl;
            g_qos_candidates[idx].ambr_dl_valid = 1;
        }
        if (ul_valid) {
            g_qos_candidates[idx].ambr_ul = ul;
            g_qos_candidates[idx].ambr_ul_valid = 1;
        }
        g_qos_candidates[idx].seq = ++g_qos_seq;
    }
    if (dl_valid) {
        g_ambr_dl = dl;
        g_ambr_dl_valid = 1;
    }
    if (ul_valid) {
        g_ambr_ul = ul;
        g_ambr_ul_valid = 1;
    }
}

static int qos_candidate_score(const struct qos_candidate *cand, int mcc, int mnc)
{
    int score = cand->current_log ? 1000 : 0;

    /* key.log is newer than key.log.0; PLMN only breaks ties within one log. */
    if (mcc > 0 && cand->has_plmn && cand->mcc == mcc && cand->mnc == mnc)
        return score + 300 + cand->rank;
    if (!cand->has_plmn)
        return score + 100 + cand->rank;
    return score + 10 + cand->rank;
}

static void select_qos_for_plmn(int mcc, int mnc, struct qos_values *out)
{
    int i;
    int best = -1;
    int best_score = -1;
    unsigned long best_seq = 0;
    int qci_score = -1, dl_score = -1, ul_score = -1;

    memset(out, 0, sizeof *out);
    for (i = 0; i < QOS_CANDIDATE_MAX; i++) {
        const struct qos_candidate *cand = &g_qos_candidates[i];
        int score;

        if (!cand->used || !qos_candidate_has_values(cand)) continue;
        score = qos_candidate_score(cand, mcc, mnc);

        if (score > best_score || (score == best_score && cand->seq > best_seq)) {
            best = i;
            best_score = score;
            best_seq = cand->seq;
        }
    }

    if (best >= 0) {
        const struct qos_candidate *cand = &g_qos_candidates[best];
        out->qci = cand->qci;
        out->qci_valid = cand->qci_valid;
        out->ambr_dl = cand->ambr_dl;
        out->ambr_dl_valid = cand->ambr_dl_valid;
        out->ambr_ul = cand->ambr_ul;
        out->ambr_ul_valid = cand->ambr_ul_valid;
    }

    /* A newer partial record must not hide a valid value retained from key.log.0. */
    for (i = 0; i < QOS_CANDIDATE_MAX; i++) {
        const struct qos_candidate *cand = &g_qos_candidates[i];
        int score;

        if (!cand->used || !qos_candidate_has_values(cand)) continue;
        score = qos_candidate_score(cand, mcc, mnc);
        if (!out->qci_valid && cand->qci_valid && score > qci_score) {
            out->qci = cand->qci;
            out->qci_valid = 1;
            qci_score = score;
        }
        if (!out->ambr_dl_valid && cand->ambr_dl_valid && score > dl_score) {
            out->ambr_dl = cand->ambr_dl;
            out->ambr_dl_valid = 1;
            dl_score = score;
        }
        if (!out->ambr_ul_valid && cand->ambr_ul_valid && score > ul_score) {
            out->ambr_ul = cand->ambr_ul;
            out->ambr_ul_valid = 1;
            ul_score = score;
        }
    }

    if (out->qci_valid || out->ambr_dl_valid || out->ambr_ul_valid) return;

    out->qci = g_qci;
    out->qci_valid = g_qci_valid;
    out->ambr_dl = g_ambr_dl;
    out->ambr_dl_valid = g_ambr_dl_valid;
    out->ambr_ul = g_ambr_ul;
    out->ambr_ul_valid = g_ambr_ul_valid;
}

static int qos_cache_has_values(void)
{
    return g_qci_valid || g_ambr_dl_valid || g_ambr_ul_valid;
}

static void clear_qos_cache(void)
{
    g_qci = 0;
    g_qci_valid = 0;
    g_ambr_dl = 0.0;
    g_ambr_ul = 0.0;
    g_ambr_dl_valid = 0;
    g_ambr_ul_valid = 0;
    memset(g_qos_candidates, 0, sizeof g_qos_candidates);
    g_qos_seq = 0;
}

static off_t file_size_or_zero(const char *path)
{
    struct stat st;
    if (stat(path, &st) != 0 || st.st_size < 0) return 0;
    return st.st_size;
}

static void scan_qos_file(const char *path, off_t floor, off_t *size_out,
                          int current_log)
{
    char line[2048];
    int qci;
    double dl = 0.0, ul = 0.0;
    int dl_valid, ul_valid;
    int qci_context_lines = 0;
    int qci_context_idx = -1;
    int pending_default_qci = 0;
    int pending_default_qci_lines = 0;
    int pending_default_qci_valid = 0;
    FILE *fp = fopen(path, "r");
    off_t size = file_size_or_zero(path);

    if (size_out) *size_out = size;
    if (!fp) return;
    if (floor > 0) {
        if (size <= 0 || floor > size) floor = 0;
        if (floor > 0 && fseeko(fp, floor, SEEK_SET) != 0) floor = 0;
        if (floor == 0 && fseeko(fp, 0, SEEK_SET) != 0) {
            fclose(fp);
            return;
        }
    }

    while (fgets(line, sizeof line, fp)) {
        int candidate_idx;

        if (!strstr(line, "[DATA]")) continue;

        candidate_idx = qos_candidate_from_line(line, current_log);
        if (candidate_idx >= 0) {
            qci_context_idx = candidate_idx;
            qci_context_lines = 4;
            if (pending_default_qci_valid) {
                qos_candidate_set_qci(candidate_idx, pending_default_qci);
                pending_default_qci_valid = 0;
                pending_default_qci_lines = 0;
            }
        }

        if (strstr(line, "qci") && parse_int_after(line, "qci", &qci)) {
            if (qci_context_idx >= 0 && qci_context_lines > 0) {
                qos_candidate_set_qci(qci_context_idx, qci);
            } else if (contains_ci(line, "default bearer qci")) {
                pending_default_qci = qci;
                pending_default_qci_lines = 4;
                pending_default_qci_valid = 1;
                if (!g_qci_valid) {
                    g_qci = qci;
                    g_qci_valid = 1;
                }
            } else if (!g_qci_valid) {
                g_qci = qci;
                g_qci_valid = 1;
            }
        }

        if (strstr(line, "session_ambr") && candidate_idx >= 0) {
            dl_valid = parse_session_ambr_mbps(line, "session_ambr_dl=", "session_ambr_dl_unit=", &dl);
            ul_valid = parse_session_ambr_mbps(line, "session_ambr_ul=", "session_ambr_ul_unit=", &ul);
            qos_candidate_set_ambr(candidate_idx, dl, dl_valid, ul, ul_valid);
        }

        if (strstr(line, "apn_ambr") && candidate_idx >= 0) {
            dl_valid = parse_double_after(line, "apn_ambr_dl_ext2=", &dl) ||
                       parse_double_after(line, "apn_ambr_dl_ext=", &dl) ||
                       (parse_double_after(line, "apn_ambr_dl=", &dl) && (dl /= 1000.0, 1));
            ul_valid = parse_double_after(line, "apn_ambr_ul_ext2=", &ul) ||
                       parse_double_after(line, "apn_ambr_ul_ext=", &ul) ||
                       (parse_double_after(line, "apn_ambr_ul=", &ul) && (ul /= 1000.0, 1));
            qos_candidate_set_ambr(candidate_idx, dl, dl_valid, ul, ul_valid);
        }
        if (qci_context_lines > 0 && --qci_context_lines == 0)
            qci_context_idx = -1;
        if (pending_default_qci_lines > 0 && --pending_default_qci_lines == 0)
            pending_default_qci_valid = 0;
    }
    fclose(fp);
}

static void refresh_qos_cache(void)
{
    off_t size = 0;
    scan_qos_file(KEY_LOG_PATH, g_qos_floor_off, &size, 1);
    g_qos_floor_off = size;
}

static void rescan_qos_cache(void)
{
    clear_qos_cache();
    g_qos_floor_off = 0;
    scan_qos_file(KEY_LOG_ROTATED_PATH, 0, NULL, 0);
    refresh_qos_cache();
}

static int g_active_subid = 1;

static int read_sim_signature(char *out, size_t outlen)
{
    char sim[RAW_MAX];
    char iccid[64], slot[16], imsi[32], state[32];

    out[0] = 0;
    if (run_ubus("zwrt_zte_mdm.api", "get_sim_info", NULL, sim, sizeof sim) != 0)
        return 0;
    copy_text(g_sim_cache, sizeof g_sim_cache, sim);

    if (!json_get(sim, "sim_iccid", iccid, sizeof iccid)) iccid[0] = 0;
    if (!json_get(sim, "current_sim_slot", slot, sizeof slot)) slot[0] = 0;
    if (!json_get(sim, "sim_imsi", imsi, sizeof imsi)) imsi[0] = 0;
    if (!json_get(sim, "sim_states", state, sizeof state)) state[0] = 0;

    if (slot[0]) {
        char *end = NULL;
        long subid = strtol(slot, &end, 10);
        if (end != slot && !*end && subid >= 1 && subid <= 6)
            g_active_subid = (int)subid;
    }

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

static int read_line_file(const char *path, char *out, size_t outlen)
{
    FILE *fp = fopen(path, "r");
    if (!fp) {
        if (outlen) out[0] = 0;
        return 0;
    }
    if (!fgets(out, outlen, fp)) {
        fclose(fp);
        if (outlen) out[0] = 0;
        return 0;
    }
    fclose(fp);
    out[strcspn(out, "\r\n")] = 0;
    return 1;
}

static long normalize_temp_reading(long raw)
{
    if (raw <= 0) return 0;
    if (raw >= 1000) return (raw + 500) / 1000;
    return raw;
}

static long read_cpu_temp_sysfs(void)
{
    static const char *roots[] = {
        "/sys/devices/virtual/thermal",
        "/sys/class/thermal"
    };
    char type_path[256], temp_path[256], type[128];
    long cpuss_sum = 0, cpuss_count = 0, fallback = 0;

    for (size_t i = 0; i < sizeof roots / sizeof roots[0]; i++) {
        DIR *dir = opendir(roots[i]);
        if (!dir) continue;

        struct dirent *de;
        while ((de = readdir(dir))) {
            long raw, temp;

            if (strncmp(de->d_name, "thermal_zone", 12) != 0) continue;

            int type_len = snprintf(type_path, sizeof type_path, "%s/%s/type", roots[i], de->d_name);
            int temp_len = snprintf(temp_path, sizeof temp_path, "%s/%s/temp", roots[i], de->d_name);
            if (type_len < 0 || (size_t)type_len >= sizeof type_path ||
                temp_len < 0 || (size_t)temp_len >= sizeof temp_path) continue;
            if (!read_line_file(type_path, type, sizeof type)) continue;

            raw = read_long_file(temp_path, LONG_MIN);
            if (raw == LONG_MIN || raw <= 0 || raw > 200000) continue;

            temp = normalize_temp_reading(raw);
            if (temp <= 0) continue;

            if (!strncmp(type, "cpuss", 5)) {
                cpuss_sum += temp;
                cpuss_count++;
            } else if (!fallback && (
                           !strncmp(type, "cpu", 3) ||
                           !strncmp(type, "sys-therm", 9) ||
                           !strcmp(type, "pmx75_tz"))) {
                fallback = temp;
            } else if (temp > fallback) {
                fallback = temp;
            }
        }
        closedir(dir);
        if (cpuss_count > 0) break;
    }

    if (cpuss_count > 0) return cpuss_sum / cpuss_count;
    return fallback;
}

static long read_cpu_temp_value_compat(const char *therm)
{
    static const char *keys[] = {
        "cpuss_temp",
        "cpu_temp",
        "temperature",
        "temp"
    };

    for (size_t i = 0; i < sizeof keys / sizeof keys[0]; i++) {
        long v = json_get_int(therm, keys[i], -1);
        if (v >= 0) return normalize_temp_reading(v);
    }

    return read_cpu_temp_sysfs();
}

static long read_cpu_temp_value_u60(const char *therm)
{
    long v = json_get_int(therm, "cpuss_temp", -1);
    if (v < 0) return 0;
    return normalize_temp_reading(v);
}

static void chomp(char *s)
{
    size_t n;
    if (!s) return;
    n = strlen(s);
    while (n && (s[n - 1] == '\n' || s[n - 1] == '\r')) s[--n] = 0;
}

static void parse_wifi_dhcp_output(FILE *fp,
                                   char *ssid, size_t ssid_n,
                                   char *key, size_t key_n,
                                   char *enc, size_t enc_n,
                                   int *enabled,
                                   char *ip, size_t ip_n,
                                   char *start, size_t start_n,
                                   char *limit, size_t limit_n,
                                   char *lease, size_t lease_n)
{
    char line[256];

    ssid[0] = key[0] = enc[0] = ip[0] = start[0] = limit[0] = lease[0] = 0;
    *enabled = 1;
    if (!fp) return;
    while (fgets(line, sizeof line, fp)) {
        chomp(line);
        if      (!strncmp(line, "SSID=", 5))  copy_text(ssid,  ssid_n,  line + 5);
        else if (!strncmp(line, "KEY=", 4))   copy_text(key,   key_n,   line + 4);
        else if (!strncmp(line, "ENC=", 4))   copy_text(enc,   enc_n,   line + 4);
        else if (!strncmp(line, "DIS=", 4))   *enabled = atoi(line + 4) ? 0 : 1;
        else if (!strncmp(line, "IP=", 3))    copy_text(ip,    ip_n,    line + 3);
        else if (!strncmp(line, "START=", 6)) copy_text(start, start_n, line + 6);
        else if (!strncmp(line, "LIMIT=", 6)) copy_text(limit, limit_n, line + 6);
        else if (!strncmp(line, "LEASE=", 6)) copy_text(lease, lease_n, line + 6);
    }
}

static void load_wifi_dhcp_u60(char *ssid, size_t ssid_n,
                               char *key, size_t key_n,
                               char *enc, size_t enc_n,
                               int *enabled,
                               char *ip, size_t ip_n,
                               char *start, size_t start_n,
                               char *limit, size_t limit_n,
                               char *lease, size_t lease_n)
{
    FILE *fp = popen(
        "printf 'SSID=%s\\n' \"$(uci -q get wireless.main_2g.ssid 2>/dev/null)\";"
        "printf 'KEY=%s\\n' \"$(uci -q get wireless.main_2g.key 2>/dev/null)\";"
        "printf 'ENC=%s\\n' \"$(uci -q get wireless.main_2g.encryption 2>/dev/null)\";"
        "printf 'DIS=%s\\n' \"$(uci -q get wireless.main_2g.disabled 2>/dev/null)\";"
        "printf 'IP=%s\\n' \"$(uci -q get network.lan.ipaddr 2>/dev/null)\";"
        "printf 'START=%s\\n' \"$(uci -q get dhcp.lan.start 2>/dev/null)\";"
        "printf 'LIMIT=%s\\n' \"$(uci -q get dhcp.lan.limit 2>/dev/null)\";"
        "printf 'LEASE=%s\\n' \"$(uci -q get dhcp.lan.leasetime 2>/dev/null)\"",
        "r");
    parse_wifi_dhcp_output(fp,
                           ssid, ssid_n,
                           key, key_n,
                           enc, enc_n,
                           enabled,
                           ip, ip_n,
                           start, start_n,
                           limit, limit_n,
                           lease, lease_n);
    if (fp) pclose(fp);
}

static void load_wifi_dhcp_compat(char *ssid, size_t ssid_n,
                                  char *key, size_t key_n,
                                  char *enc, size_t enc_n,
                                  int *enabled,
                                  char *ip, size_t ip_n,
                                  char *start, size_t start_n,
                                  char *limit, size_t limit_n,
                                  char *lease, size_t lease_n)
{
    FILE *fp = popen(
        "section='';"
        "fallback='';"
        "for s in main_2g main_5g; do "
        "  ssid=$(uci -q get wireless.$s.ssid 2>/dev/null);"
        "  [ -n \"$ssid\" ] || continue;"
        "  dis=$(uci -q get wireless.$s.disabled 2>/dev/null);"
        "  [ -n \"$fallback\" ] || fallback=$s;"
        "  [ \"$dis\" = \"1\" ] || { section=$s; break; };"
        "done;"
        "[ -n \"$section\" ] || section=$fallback;"
        "if [ -n \"$section\" ]; then "
        "  printf 'SSID=%s\\n' \"$(uci -q get wireless.$section.ssid 2>/dev/null)\";"
        "  printf 'KEY=%s\\n' \"$(uci -q get wireless.$section.key 2>/dev/null)\";"
        "  printf 'ENC=%s\\n' \"$(uci -q get wireless.$section.encryption 2>/dev/null)\";"
        "  printf 'DIS=%s\\n' \"$(uci -q get wireless.$section.disabled 2>/dev/null)\";"
        "fi;"
        "printf 'IP=%s\\n' \"$(uci -q get network.lan.ipaddr 2>/dev/null)\";"
        "printf 'START=%s\\n' \"$(uci -q get dhcp.lan.start 2>/dev/null)\";"
        "printf 'LIMIT=%s\\n' \"$(uci -q get dhcp.lan.limit 2>/dev/null)\";"
        "printf 'LEASE=%s\\n' \"$(uci -q get dhcp.lan.leasetime 2>/dev/null)\"",
        "r");
    parse_wifi_dhcp_output(fp,
                           ssid, ssid_n,
                           key, key_n,
                           enc, enc_n,
                           enabled,
                           ip, ip_n,
                           start, start_n,
                           limit, limit_n,
                           lease, lease_n);
    if (fp) pclose(fp);
}

static void load_wifi_dhcp_for_template(const struct device_template_spec *tpl,
                                        char *ssid, size_t ssid_n,
                                        char *key, size_t key_n,
                                        char *enc, size_t enc_n,
                                        int *enabled,
                                        char *ip, size_t ip_n,
                                        char *start, size_t start_n,
                                        char *limit, size_t limit_n,
                                        char *lease, size_t lease_n)
{
    if (tpl->wifi_mode == WIFI_SOURCE_U60_MAIN_2G) {
        load_wifi_dhcp_u60(ssid, ssid_n, key, key_n, enc, enc_n,
                           enabled, ip, ip_n, start, start_n, limit, limit_n, lease, lease_n);
        return;
    }
    load_wifi_dhcp_compat(ssid, ssid_n, key, key_n, enc, enc_n,
                          enabled, ip, ip_n, start, start_n, limit, limit_n, lease, lease_n);
}

static int append_client_entries_from_array(const char *arr_json, struct buf *outb, int *items)
{
    const char *p;
    int appended = 0;

    if (!arr_json || !items) return 0;
    p = strchr(arr_json, '[');
    if (!p) return 0;
    p++;

    while (*p) {
        const char *obj_start;
        size_t len;
        int depth = 0, in_str = 0, esc = 0;
        char obj[1024];
        char ip[64], mac[64], host[128];
        const char *name;

        while (*p && *p != '{' && *p != ']') p++;
        if (*p != '{') break;
        obj_start = p;

        for (; *p; p++) {
            char c = *p;
            if (in_str) {
                if (esc) { esc = 0; continue; }
                if (c == '\\') esc = 1;
                else if (c == '"') in_str = 0;
                continue;
            }
            if (c == '"') in_str = 1;
            else if (c == '{') depth++;
            else if (c == '}') {
                depth--;
                if (depth == 0) { p++; break; }
            }
        }
        if (depth != 0) break;

        len = (size_t)(p - obj_start);
        if (len >= sizeof obj) continue;

        memcpy(obj, obj_start, len);
        obj[len] = 0;

        if (!json_get(obj, "ip_address", ip, sizeof ip) &&
            !json_get(obj, "ip", ip, sizeof ip)) ip[0] = 0;
        if (!json_get(obj, "mac_address", mac, sizeof mac) &&
            !json_get(obj, "mac", mac, sizeof mac)) mac[0] = 0;
        if (!json_get(obj, "hostname", host, sizeof host) &&
            !json_get(obj, "name", host, sizeof host)) host[0] = 0;
        if (!ip[0] && !mac[0]) continue;

        name = (host[0] && strcmp(host, "--")) ? host : (mac[0] ? mac : ip);
        if (*items) bappend(outb, ",");
        (*items)++;
        appended = 1;
        bappend(outb, "{\"name\":\"");
        bappend_json_esc(outb, name);
        bappend(outb, "\",\"ip\":\"");
        bappend_json_esc(outb, ip);
        bappend(outb, "\",\"mac\":\"");
        bappend_json_esc(outb, mac);
        bappend(outb, "\"}");
    }

    return appended;
}

static int build_client_list_from_dhcp(char *out, size_t outlen)
{
    FILE *fp = fopen("/tmp/dhcp.leases", "r");
    char line[512];
    struct buf b = { out, outlen, 0 };
    int items = 0;

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
            if (items) bappend(&b, ",");
            items++;
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
    if (b.len >= b.cap) {
        if (outlen >= 3) memcpy(out, "[]", 2), out[2] = 0;
        return 0;
    }
    return items;
}

static int build_client_list_from_router(char *out, size_t outlen)
{
    char lan[RAW_MAX], wifi[RAW_MAX], arr[RAW_MAX];
    struct buf b = { out, outlen, 0 };
    int items = 0;

    bappend(&b, "[");
    if (run_ubus("zwrt_router.api", "router_lan_access_list",
                 "{\"start_id\":1,\"end_id\":64}", lan, sizeof lan) == 0 &&
        json_get(lan, "lan_access_list_info", arr, sizeof arr)) {
        append_client_entries_from_array(arr, &b, &items);
    }
    if (run_ubus("zwrt_router.api", "router_wireless_access_list",
                 "{\"start_id\":1,\"end_id\":64}", wifi, sizeof wifi) == 0 &&
        json_get(wifi, "wireless_access_list_info", arr, sizeof arr)) {
        append_client_entries_from_array(arr, &b, &items);
    }
    bappend(&b, "]");
    if (b.len >= b.cap) {
        if (outlen >= 3) memcpy(out, "[]", 2), out[2] = 0;
        return 0;
    }
    return items;
}

static void build_client_list_json_u60(char *out, size_t outlen)
{
    if (build_client_list_from_dhcp(out, outlen) > 0) return;
    if (outlen >= 3) memcpy(out, "[]", 2), out[2] = 0;
}

static void build_client_list_json_compat(char *out, size_t outlen)
{
    if (build_client_list_from_dhcp(out, outlen) > 0) return;
    if (build_client_list_from_router(out, outlen) > 0) return;
    if (outlen >= 3) memcpy(out, "[]", 2), out[2] = 0;
}

static void build_client_list_json_for_template(const struct device_template_spec *tpl,
                                                char *out, size_t outlen)
{
    if (tpl->client_mode == CLIENT_SOURCE_DHCP_ONLY) {
        build_client_list_json_u60(out, outlen);
        return;
    }
    build_client_list_json_compat(out, outlen);
}

static int load_thermal_snapshot_for_template(const struct device_template_spec *tpl,
                                              char *therm, size_t therm_n)
{
    if (tpl->temp_mode == TEMP_SOURCE_U60_UBUS_ONLY) {
        return run_ubus("zwrt_bsp.thermal", "get_cpu_temp", NULL, therm, therm_n);
    }
    if (run_ubus("zwrt_bsp.thermal", "get_cpu_temp", NULL, therm, therm_n) != 0)
        return run_ubus("zwrt_bsp.thermal", "list", NULL, therm, therm_n);
    return 0;
}

static long read_cpu_temp_for_template(const struct device_template_spec *tpl, const char *therm)
{
    if (tpl->temp_mode == TEMP_SOURCE_U60_UBUS_ONLY)
        return read_cpu_temp_value_u60(therm);
    return read_cpu_temp_value_compat(therm);
}

struct uci_net_field {
    const char *json_key;
    const char *primary_path;
    const char *fallback_path;
};

static int uci_show_value(const char *show, const char *path, char *out, size_t outlen)
{
    size_t path_len;
    const char *line;
    if (!show || !path || !out || outlen < 2) return -1;
    path_len = strlen(path);
    line = show;
    while (*line) {
        const char *end = strchr(line, '\n');
        size_t line_len = end ? (size_t)(end - line) : strlen(line);
        if (line_len > path_len && line[path_len] == '=' &&
            !strncmp(line, path, path_len)) {
            const char *value = line + path_len + 1;
            size_t value_len = line_len - path_len - 1;
            if (value_len >= 2 && value[0] == '\'' && value[value_len - 1] == '\'') {
                value++;
                value_len -= 2;
            }
            if (value_len >= outlen) value_len = outlen - 1;
            memcpy(out, value, value_len);
            out[value_len] = 0;
            return 0;
        }
        if (!end) break;
        line = end + 1;
    }
    out[0] = 0;
    return -1;
}

static int load_network_snapshot_mu5252_uci(char *out, size_t outlen)
{
    static const struct uci_net_field fields[] = {
        {"network_type", "zte_nwinfo.sys_info.network_type", NULL},
        {"signalbar", "zte_nwinfo.signal_strength.signalbar", NULL},
        {"network_provider_fullname", "zte_nwinfo.plmn_info.network_provider_fullname", NULL},
        {"wan_active_band", "zte_nwinfo.wan_active_band.GWLSA_band", "zte_nwinfo.wan_active_band.odu_nrband"},
        {"nr5g_action_band", "zte_nwinfo.wan_active_band.odu_nrband", "zte_nwinfo.wan_active_band.GWLSA_band"},
        {"nr5g_rsrp", "zte_nwinfo.signal_strength.nr5g_rsrp", NULL},
        {"nr5g_rsrq", "zte_nwinfo.signal_strength.nr5g_rsrq", NULL},
        {"nr5g_snr", "zte_nwinfo.signal_strength.nr5g_snr", NULL},
        {"nr5g_rssi", "zte_nwinfo.signal_strength.nr5g_rssi", NULL},
        {"lte_rsrp", "zte_nwinfo.signal_strength.lte_rsrp", NULL},
        {"lte_rsrq", "zte_nwinfo.signal_strength.lte_rsrq", NULL},
        {"lte_rssi", "zte_nwinfo.signal_strength.lte_rssi", NULL},
        {"lte_snr", "zte_nwinfo.signal_strength.lte_snr", NULL},
        {"rssi", "zte_nwinfo.signal_strength.rssi", NULL},
        {"rmcc", "zte_nwinfo.plmn_info.rmcc", NULL},
        {"rmnc", "zte_nwinfo.plmn_info.rmnc", NULL},
        {"nr5g_pci", "zte_nwinfo.cell_info.nr5g_pci", NULL},
        {"nr5g_cell_id", "zte_nwinfo.cell_info.nr5g_cellid", NULL},
        {"nr5g_action_channel", "zte_nwinfo.cell_info.nr5g_action_channel", NULL},
        {"nr5g_bandwidth", "zte_nwinfo.cell_info.nr5g_bandwidth", NULL},
        {"nrca", "zte_nwinfo.sys_info.nrca", "zte_nwinfo.sys_info.odu_nrca"},
        {"lteca", "zte_nwinfo.sys_info.lteca", "zte_nwinfo.sys_info.odu_lteca"},
        {"ltecasig", "zte_nwinfo.sys_info.ltecasig", NULL},
        {"net_select", "zte_nwinfo.sys_info.net_select", NULL},
        {"nr5g_sa_band_lock", "zte_nwinfo.band_lock.nr5g_sa_band_lock", NULL},
        {"nr5g_nsa_band_lock", "zte_nwinfo.band_lock.nr5g_nsa_band_lock", NULL},
        {"lte_band", "zte_nwinfo.band_lock.lte_ext_band_lock", NULL}
    };
    struct buf b = {out, outlen, 0};
    char show[RAW_MAX];
    int found = 0;

    if (!out || outlen < 3) return -1;
    out[0] = 0;
    if (device_uci_show("zte_nwinfo", show, sizeof show) != 0) {
        memcpy(out, "{}", 3);
        return -1;
    }
    bappend(&b, "{");
    for (size_t i = 0; i < sizeof fields / sizeof fields[0]; i++) {
        char value[256] = "";
        if (uci_show_value(show, fields[i].primary_path, value, sizeof value) != 0 &&
            fields[i].fallback_path)
            (void)uci_show_value(show, fields[i].fallback_path, value, sizeof value);
        if (i) bappend(&b, ",");
        bappend(&b, "\"%s\":\"", fields[i].json_key);
        bappend_json_esc(&b, value);
        bappend(&b, "\"");
        if (value[0]) found = 1;
    }
    bappend(&b, "}");
    if (b.len >= b.cap || !found) {
        memcpy(out, "{}", 3);
        return -1;
    }
    return 0;
}

static int load_network_snapshot_for_template(const struct device_template_spec *tpl,
                                              char *net, size_t net_n)
{
    if (run_ubus("zte_nwinfo_api", "nwinfo_get_netinfo", NULL, net, net_n) == 0)
        return 0;
    if (tpl->network_mode == NETWORK_SOURCE_MU5252_UCI_FALLBACK)
        return load_network_snapshot_mu5252_uci(net, net_n);
    return -1;
}

static int load_traffic_snapshot_for_template(const struct device_template_spec *tpl,
                                              char *traffic, size_t traffic_n)
{
    char args[160];
    if (tpl->traffic_mode == TRAFFIC_SOURCE_CID1_ACTIVE_SUBID) {
        snprintf(args, sizeof args,
                 "{\"source_module\":\"deviceui\",\"cid\":1,\"type\":1,\"subid\":%d}",
                 g_active_subid);
    } else {
        snprintf(args, sizeof args,
                 "{\"source_module\":\"deviceui\",\"cid\":1,\"type\":1}");
    }
    return run_ubus("zwrt_data", "get_wwandst", args, traffic, traffic_n);
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
    char device_profile[64], device_profile_source[64];
    char device_vendor[64], device_model_name[128], device_hw[128];
    char device_market_name[128], device_alias_name[128], device_board_name[128];
    char client_list[CLIENT_LIST_MAX];
    long chg_uv, chg_ua, bat_uv, bat_ua, cpu_temp;
    int cpu_usage, cpu_usage_tenths, wifi_enabled;
    char runtime_json[32768];
    int qos_mcc, qos_mnc;
    struct qos_values qos;
    const struct device_template_spec *device_template;

    detect_device_identity(with_board, board_cache,
                           with_common, common_cache,
                           device_profile, sizeof device_profile,
                           device_profile_source, sizeof device_profile_source,
                           device_vendor, sizeof device_vendor,
                           device_model_name, sizeof device_model_name,
                           device_hw, sizeof device_hw,
                           device_market_name, sizeof device_market_name,
                           device_alias_name, sizeof device_alias_name,
                           device_board_name, sizeof device_board_name);
    device_template = select_device_template(device_model_name, device_hw);

    load_network_snapshot_for_template(device_template, net, sizeof net);
    run_ubus("zwrt_bsp.battery", "list", NULL, batt, sizeof batt);
    run_ubus("zwrt_bsp.charger", "list", NULL, chg, sizeof chg);
    load_thermal_snapshot_for_template(device_template, therm, sizeof therm);
    run_ubus("zwrt_router.api", "router_get_user_list_num", NULL, rnum, sizeof rnum);
    run_ubus("zwrt_router.api", "router_get_status_no_auth", NULL, rstat, sizeof rstat);
    /* type:1 = realtime session stats; cid:1 = main PDN (rmnet_data0). */
    load_traffic_snapshot_for_template(device_template, traf, sizeof traf);
    run_ubus("system", "info", NULL, sysinfo, sizeof sysinfo);
    run_ubus("zwrt_bsp.usb", "list", NULL, usb, sizeof usb);
    run_ubus("zwrt_nfc", "zwrt_nfc_wifi_get", NULL, nfc, sizeof nfc);
    load_wifi_dhcp_for_template(device_template,
                                wifi_ssid, sizeof wifi_ssid,
                                wifi_key, sizeof wifi_key,
                                wifi_enc, sizeof wifi_enc,
                                &wifi_enabled,
                                dhcp_ip, sizeof dhcp_ip,
                                dhcp_start, sizeof dhcp_start,
                                dhcp_limit, sizeof dhcp_limit,
                                dhcp_lease, sizeof dhcp_lease);
    build_client_list_json_for_template(device_template, client_list, sizeof client_list);
    chg_uv = read_long_file("/sys/class/power_supply/usb/voltage_now", 0);
    chg_ua = read_long_file("/sys/class/power_supply/usb/current_now", 0);
    bat_uv = read_long_file("/sys/class/power_supply/battery/voltage_now", 0);
    bat_ua = read_long_file("/sys/class/power_supply/battery/current_now", 0);
    cpu_usage_tenths = system_ext_build_json(runtime_json, sizeof runtime_json);
    cpu_usage = cpu_usage_tenths >= 0 ? (cpu_usage_tenths + 5) / 10 : -1;
    cpu_temp = read_cpu_temp_for_template(device_template, therm);
    qos_mcc = json_get_int(net, "rmcc", 0);
    qos_mnc = json_get_int(net, "rmnc", 0);
    select_qos_for_plmn(qos_mcc, qos_mnc, &qos);

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
    emit_str(&b, "wan_status", rstat, "current_wan_status"); bappend(&b, ",");
    bappend(&b, "\"HSR\":false");
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
    emit_int(&b, "session_time", traf, "real_time", 0);          bappend(&b, ",");
    emit_int(&b, "day_rx_bytes", g_traffic_accounting, "day_rx_bytes", 0); bappend(&b, ",");
    emit_int(&b, "day_tx_bytes", g_traffic_accounting, "day_tx_bytes", 0); bappend(&b, ",");
    emit_int(&b, "month_rx_bytes", g_traffic_accounting, "month_rx_bytes", 0); bappend(&b, ",");
    emit_int(&b, "month_tx_bytes", g_traffic_accounting, "month_tx_bytes", 0); bappend(&b, ",");
    emit_int(&b, "total_rx_bytes", g_traffic_accounting, "total_rx_bytes", 0); bappend(&b, ",");
    emit_int(&b, "total_tx_bytes", g_traffic_accounting, "total_tx_bytes", 0); bappend(&b, ",");
    bappend(&b, "\"limit\":%s,\"clear_day\":%s",
            g_traffic_limit[0] ? g_traffic_limit : "{}",
            g_traffic_clear_day[0] ? g_traffic_clear_day : "{}");
    bappend(&b, "},");

    /* qos: last known bearer/QoS values cached from modem key.log */
    bappend(&b, "\"qos\":{");
    bappend(&b, "\"qci\":%d,", qos.qci_valid ? qos.qci : 0);
    if (qos.ambr_dl_valid) bappend(&b, "\"ambr_dl\":\"%.3f\",", qos.ambr_dl);
    else                 bappend(&b, "\"ambr_dl\":\"\",");
    if (qos.ambr_ul_valid) bappend(&b, "\"ambr_ul\":\"%.3f\",", qos.ambr_ul);
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

    /* Runtime interface state is refreshed at a lower cadence than radio data. */
    bappend(&b, "\"interfaces\":{");
    emit_interface_status(&b, "lan", g_lan_interface); bappend(&b, ",");
    emit_interface_status(&b, "wan4", g_wan4_interface); bappend(&b, ",");
    emit_interface_status(&b, "wan6", g_wan6_interface); bappend(&b, ",");
    bappend(&b, "\"lan_config\":%s,\"cellular\":%s",
            g_lan_runtime[0] ? g_lan_runtime : "{}",
            g_cellular_runtime[0] ? g_cellular_runtime : "{}");
    bappend(&b, "},");

    /* SIM identity and provisioning state. Values remain device-local. */
    bappend(&b, "\"sim\":{");
    emit_str(&b, "iccid", g_sim_cache, "sim_iccid"); bappend(&b, ",");
    emit_str(&b, "imsi", g_sim_cache, "sim_imsi"); bappend(&b, ",");
    emit_str(&b, "msisdn", g_sim_cache, "msisdn"); bappend(&b, ",");
    emit_str(&b, "state", g_sim_cache, "sim_states"); bappend(&b, ",");
    emit_str(&b, "modem_state", g_sim_cache, "modem_main_state"); bappend(&b, ",");
    emit_str(&b, "pin_status", g_sim_cache, "pin_status"); bappend(&b, ",");
    emit_int(&b, "current_slot", g_sim_cache, "current_sim_slot", 0); bappend(&b, ",");
    emit_int(&b, "dual_sim", g_sim_cache, "support_dual_sim", 0); bappend(&b, ",");
    emit_int(&b, "sim1_provision", g_sim_cache, "sim1_provision_state", 0); bappend(&b, ",");
    emit_int(&b, "sim2_provision", g_sim_cache, "sim2_provision_state", 0);
    bappend(&b, "},");

    /* dhcp */
    bappend(&b, "\"dhcp\":{");
    bappend(&b, "\"ip\":\"");        bappend_json_esc(&b, dhcp_ip);    bappend(&b, "\",");
    bappend(&b, "\"start\":\"");     bappend_json_esc(&b, dhcp_start); bappend(&b, "\",");
    bappend(&b, "\"limit\":\"");     bappend_json_esc(&b, dhcp_limit); bappend(&b, "\",");
    bappend(&b, "\"leasetime\":\""); bappend_json_esc(&b, dhcp_lease); bappend(&b, "\"");
    bappend(&b, "},");

    /* device identity + backend-selected source template. */
    bappend(&b, "\"device\":{");
    emit_kv_str(&b, "profile", device_profile);                bappend(&b, ",");
    emit_kv_str(&b, "profile_source", device_profile_source);  bappend(&b, ",");
    emit_kv_str(&b, "api_template", device_template->id);      bappend(&b, ",");
    emit_kv_str(&b, "api_template_label", device_template->label); bappend(&b, ",");
    bappend(&b, "\"api_template_supported\":%d,", device_template->supported);
    emit_kv_str(&b, "vendor", device_vendor);                  bappend(&b, ",");
    emit_kv_str(&b, "model_name", device_model_name);          bappend(&b, ",");
    emit_kv_str(&b, "hardware_version", device_hw);            bappend(&b, ",");
    emit_kv_str(&b, "market_name", device_market_name);        bappend(&b, ",");
    emit_kv_str(&b, "alias_name", device_alias_name);          bappend(&b, ",");
    emit_kv_str(&b, "board_name", device_board_name);
    bappend(&b, "},");

    /* system */
    bappend(&b, "\"system\":{");
    emit_int(&b, "uptime", sysinfo, "uptime", 0);    bappend(&b, ",");
    bappend(&b, "\"cpu_temp\":%ld,", cpu_temp);
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
    bappend(&b, "},");

    /* Realtime system details that are intentionally sampled once per daemon cycle. */
    bappend(&b, "\"runtime\":%s}", runtime_json[0] ? runtime_json : "{}");
}

static void refresh_interface_cache(void)
{
    char next[RAW_MAX];
    if (run_ubus("network.interface.lan", "status", NULL, next, sizeof next) == 0)
        copy_text(g_lan_interface, sizeof g_lan_interface, next);
    if (run_ubus("network.interface.zte_wan", "status", NULL, next, sizeof next) == 0)
        copy_text(g_wan4_interface, sizeof g_wan4_interface, next);
    if (run_ubus("network.interface.zte_wan6", "status", NULL, next, sizeof next) == 0)
        copy_text(g_wan6_interface, sizeof g_wan6_interface, next);
    if (run_ubus("zwrt_router.api", "router_get_lan_info", NULL, next, sizeof next) == 0)
        copy_text(g_lan_runtime, sizeof g_lan_runtime, next);
    if (run_ubus("zwrt_data", "get_wwaniface",
                 "{\"source_module\":\"web\",\"cid\":1,\"connect_status\":\"\"}",
                 next, sizeof next) == 0)
        copy_text(g_cellular_runtime, sizeof g_cellular_runtime, next);
    if (run_ubus("zwrt_data", "get_wwandst",
                 "{\"source_module\":\"web\",\"cid\":1,\"type\":4}",
                 next, sizeof next) == 0)
        copy_text(g_traffic_accounting, sizeof g_traffic_accounting, next);
    if (run_ubus("zwrt_data", "get_wwandst_monthlimit",
                 "{\"source_module\":\"web\",\"cid\":1}",
                 next, sizeof next) == 0)
        copy_text(g_traffic_limit, sizeof g_traffic_limit, next);
    if (run_ubus("zwrt_data", "get_wwandst_clearday",
                 "{\"source_module\":\"web\",\"cid\":1}",
                 next, sizeof next) == 0)
        copy_text(g_traffic_clear_day, sizeof g_traffic_clear_day, next);
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

static const char *http_header_value(const char *req, const char *name,
                                     char *out, size_t outlen)
{
    const char *line = strstr(req, "\r\n");
    size_t name_len = strlen(name);
    if (!line || !out || outlen == 0) return NULL;
    line += 2;
    while (*line && strncmp(line, "\r\n", 2) != 0) {
        const char *end = strstr(line, "\r\n");
        const char *value;
        size_t len;
        if (!end) return NULL;
        if ((size_t)(end - line) > name_len && !strncasecmp(line, name, name_len) &&
            line[name_len] == ':') {
            value = line + name_len + 1;
            while (value < end && isspace((unsigned char)*value)) value++;
            len = (size_t)(end - value);
            while (len && isspace((unsigned char)value[len - 1])) len--;
            if (len >= outlen) len = outlen - 1;
            memcpy(out, value, len);
            out[len] = 0;
            return out;
        }
        line = end + 2;
    }
    return NULL;
}

static int read_http_request(int fd, char *buf, size_t cap, int timeout_ms)
{
    size_t len = 0;
    size_t wanted = 0;

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
        if (!wanted) {
            char *header_end = strstr(buf, "\r\n\r\n");
            if (header_end) {
                char value[64];
                long body_len = 0;
                size_t header_len = (size_t)(header_end + 4 - buf);
                if (http_header_value(buf, "Content-Length", value, sizeof value)) {
                    char *end;
                    body_len = strtol(value, &end, 10);
                    if (end == value || body_len < 0) return -1;
                }
                if ((unsigned long)body_len > cap - 1 - header_len) return -2;
                wanted = header_len + (size_t)body_len;
            }
        }
        if (wanted && len >= wanted) {
            /* Do not expose pipelined or surplus bytes as part of this body. */
            buf[wanted] = 0;
            return (int)wanted;
        }
    }

    return -2;
}

static int parse_request_line(const char *req, char *method, size_t method_cap,
                              char *path, size_t path_cap)
{
    if (sscanf(req, "%15s %1023s", method, path) != 2) return -1;
    method[method_cap - 1] = 0;
    path[path_cap - 1] = 0;
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

static void write_http_unauthorized(int fd)
{
    static const char resp[] =
        "HTTP/1.1 401 Unauthorized\r\n"
        "WWW-Authenticate: Bearer realm=\"zwrt-datad-lan\"\r\n"
        "Connection: close\r\n"
        "Content-Length: 0\r\n"
        "\r\n";
    (void)write_all(fd, resp, sizeof resp - 1);
}

static int write_http_json_status(int fd, int code, const char *status,
                                  const char *body, size_t body_len)
{
    char hdr[320];
    int n = snprintf(hdr, sizeof hdr,
                     "HTTP/1.1 %d %s\r\n"
                     "Content-Type: application/json; charset=utf-8\r\n"
                     "Cache-Control: no-store\r\n"
                     "Connection: close\r\n"
                     "Content-Length: %zu\r\n"
                     "\r\n",
                     code, status, body_len);
    if (n <= 0) return -1;
    if (write_all(fd, hdr, (size_t)n) < 0) return -1;
    return write_all(fd, body, body_len);
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
    return write_http_json_status(fd, 200, "OK", snap, snap_len);
}

static int secure_equal(const char *a, const char *b)
{
    size_t alen = a ? strlen(a) : 0;
    size_t blen = b ? strlen(b) : 0;
    size_t count = alen > blen ? alen : blen;
    size_t diff = alen ^ blen;
    for (size_t i = 0; i < count; i++) {
        unsigned char ac = i < alen ? (unsigned char)a[i] : 0;
        unsigned char bc = i < blen ? (unsigned char)b[i] : 0;
        diff |= ac ^ bc;
    }
    return diff == 0;
}

static void trim_ascii_ws_inplace(char *s)
{
    size_t len;
    char *start = s;
    if (!s) return;
    while (*start && isspace((unsigned char)*start)) start++;
    if (start != s) memmove(s, start, strlen(start) + 1);
    len = strlen(s);
    while (len && isspace((unsigned char)s[len - 1])) s[--len] = 0;
}

static int load_token_file(const char *path, char *out, size_t outlen)
{
    FILE *fp;
    size_t n;
    if (!path || !*path || !out || outlen < 2) return -1;
    fp = fopen(path, "r");
    if (!fp) return -1;
    n = fread(out, 1, outlen - 1, fp);
    fclose(fp);
    out[n] = 0;
    trim_ascii_ws_inplace(out);
    return out[0] ? 0 : -1;
}

static int query_value(const char *query, const char *key, char *out, size_t outlen)
{
    size_t keylen;
    const char *p;
    if (!query || !*query || !key || !*key || !out || outlen < 2) return 0;
    keylen = strlen(key);
    p = query;
    while (*p) {
        const char *amp = strchr(p, '&');
        size_t seglen = amp ? (size_t)(amp - p) : strlen(p);
        if (seglen > keylen && p[keylen] == '=' && !strncmp(p, key, keylen)) {
            size_t vlen = seglen - keylen - 1;
            if (vlen >= outlen) vlen = outlen - 1;
            memcpy(out, p + keylen + 1, vlen);
            out[vlen] = 0;
            return 1;
        }
        if (!amp) break;
        p = amp + 1;
    }
    return 0;
}

static int request_header_bearer_token(const char *req, char *out, size_t outlen)
{
    char value[HTTP_AUTH_HEADER_MAX];
    const char *token;
    size_t token_len;
    if (!http_header_value(req, "Authorization", value, sizeof value) ||
        strncasecmp(value, "Bearer ", 7)) return 0;
    token = value + 7;
    while (*token && isspace((unsigned char)*token)) token++;
    token_len = strlen(token);
    while (token_len && isspace((unsigned char)token[token_len - 1])) token_len--;
    if (!token_len) return 0;
    if (token_len >= outlen) token_len = outlen - 1;
    memcpy(out, token, token_len);
    out[token_len] = 0;
    return 1;
}

static int base64_value(int c)
{
    if (c >= 'A' && c <= 'Z') return c - 'A';
    if (c >= 'a' && c <= 'z') return 26 + c - 'a';
    if (c >= '0' && c <= '9') return 52 + c - '0';
    if (c == '+') return 62;
    if (c == '/') return 63;
    return -1;
}

static int base64_decode(const char *src, unsigned char *out, size_t outlen)
{
    unsigned int bits = 0;
    int bit_count = 0;
    size_t len = 0;
    if (!src || !out || !outlen) return -1;
    while (*src) {
        int value;
        unsigned char c = (unsigned char)*src++;
        if (isspace(c)) continue;
        if (c == '=') break;
        value = base64_value(c);
        if (value < 0) return -1;
        bits = (bits << 6) | (unsigned int)value;
        bit_count += 6;
        while (bit_count >= 8) {
            bit_count -= 8;
            if (len >= outlen) return -1;
            out[len++] = (unsigned char)((bits >> bit_count) & 0xffU);
        }
    }
    return (int)len;
}

static int request_login_credentials(const char *req, const char *query,
                                     char *username, size_t username_len,
                                     char *password, size_t password_len)
{
    char value[HTTP_AUTH_HEADER_MAX];
    unsigned char decoded[HTTP_AUTH_HEADER_MAX];
    int decoded_len;
    char *separator;
    size_t username_size, password_size;
    (void)query;
    if (!username || username_len < 2 || !password || password_len < 2) return 0;
    if (http_header_value(req, "Authorization", value, sizeof value) &&
        !strncasecmp(value, "Basic ", 6)) {
        decoded_len = base64_decode(value + 6, decoded, sizeof decoded - 1);
        if (decoded_len <= 0) return 0;
        if (memchr(decoded, 0, (size_t)decoded_len)) return 0;
        decoded[decoded_len] = 0;
        separator = memchr(decoded, ':', (size_t)decoded_len);
        if (!separator) return 0;
        username_size = (size_t)(separator - (char *)decoded);
        separator++;
        password_size = (size_t)((char *)decoded + decoded_len - separator);
        if (!username_size || username_size >= username_len || password_size >= password_len)
            return 0;
        memcpy(username, decoded, username_size);
        username[username_size] = 0;
        memcpy(password, separator, password_size);
        password[password_size] = 0;
        return 1;
    }
    return 0;
}

static int request_vendor_webtoken(const char *req, const char *query,
                                   char *webtoken, size_t webtoken_len,
                                   char *tag, size_t tag_len, int *mode_out)
{
    char mode_buf[32];
    (void)query;
    if (!webtoken || webtoken_len < 2 || !tag || tag_len < 2 || !mode_out) return 0;
    if (!http_header_value(req, "X-Web-Token", webtoken, webtoken_len) &&
        !http_header_value(req, "X-ZTE-WebToken", webtoken, webtoken_len)) return 0;
    if (!http_header_value(req, "X-Z-Tag", tag, tag_len))
        snprintf(tag, tag_len, "zwrt-datad");
    *mode_out = 0;
    if (http_header_value(req, "X-Z-Mode", mode_buf, sizeof mode_buf)) {
        char *end = NULL;
        long value = strtol(mode_buf, &end, 10);
        if (end && !*end) *mode_out = (int)value;
    }
    return webtoken[0] != 0;
}

static int fill_random_bytes(unsigned char *out, size_t outlen)
{
    size_t offset = 0;
    int fd;
    if (!out || !outlen) return -1;
    fd = open("/dev/urandom", O_RDONLY);
    if (fd < 0) return -1;
    while (offset < outlen) {
        ssize_t n = read(fd, out + offset, outlen - offset);
        if (n < 0) {
            if (errno == EINTR) continue;
            close(fd);
            return -1;
        }
        if (!n) {
            close(fd);
            return -1;
        }
        offset += (size_t)n;
    }
    close(fd);
    return 0;
}

static void auth_clear_session(struct auth_session *session)
{
    if (!session) return;
    memset(session, 0, sizeof *session);
}

static void auth_state_init(struct auth_state *auth)
{
    if (auth) memset(auth, 0, sizeof *auth);
}

static int auth_load_static_token(struct auth_state *auth, const char *path)
{
    char token[HTTP_TOKEN_MAX];
    if (!auth) return -1;
    auth->static_token[0] = 0;
    if (load_token_file(path, token, sizeof token) < 0) return -1;
    snprintf(auth->static_token, sizeof auth->static_token, "%s", token);
    return 0;
}

static void auth_expire_sessions(struct auth_state *auth)
{
    time_t now;
    if (!auth) return;
    now = time(NULL);
    for (size_t i = 0; i < HTTP_AUTH_SESSIONS; i++) {
        if (auth->sessions[i].active && auth->sessions[i].expires_at <= now)
            auth_clear_session(&auth->sessions[i]);
    }
}

static int auth_generate_token(char *out, size_t outlen)
{
    static const char hex[] = "0123456789abcdef";
    unsigned char bytes[HTTP_SESSION_TOKEN_BYTES];
    if (!out || outlen < sizeof bytes * 2 + 1) return -1;
    if (fill_random_bytes(bytes, sizeof bytes) < 0) return -1;
    for (size_t i = 0; i < sizeof bytes; i++) {
        out[i * 2] = hex[bytes[i] >> 4];
        out[i * 2 + 1] = hex[bytes[i] & 0x0f];
    }
    out[sizeof bytes * 2] = 0;
    return 0;
}

static int auth_issue_session(struct auth_state *auth, const char *subject,
                              char *token_out, size_t token_out_len,
                              time_t *expires_at_out)
{
    size_t slot = 0;
    int found = 0;
    time_t now;
    if (!auth || !token_out || token_out_len < HTTP_TOKEN_MAX) return -1;
    auth_expire_sessions(auth);
    now = time(NULL);
    for (size_t i = 0; i < HTTP_AUTH_SESSIONS; i++) {
        if (!auth->sessions[i].active) {
            slot = i;
            found = 1;
            break;
        }
        if (!found || auth->sessions[i].expires_at < auth->sessions[slot].expires_at) {
            slot = i;
            found = 1;
        }
    }
    if (!found || auth_generate_token(auth->sessions[slot].token,
                                      sizeof auth->sessions[slot].token) < 0) return -1;
    auth->sessions[slot].active = 1;
    auth->sessions[slot].expires_at = now + HTTP_SESSION_TTL_SEC;
    snprintf(auth->sessions[slot].subject, sizeof auth->sessions[slot].subject,
             "%s", subject ? subject : "lan");
    snprintf(token_out, token_out_len, "%s", auth->sessions[slot].token);
    if (expires_at_out) *expires_at_out = auth->sessions[slot].expires_at;
    return 0;
}

static int auth_token_is_valid(struct auth_state *auth, const char *token)
{
    time_t now;
    if (!auth || !token || !*token) return 0;
    if (auth->static_token[0] && secure_equal(auth->static_token, token)) return 1;
    auth_expire_sessions(auth);
    now = time(NULL);
    for (size_t i = 0; i < HTTP_AUTH_SESSIONS; i++) {
        if (!auth->sessions[i].active) continue;
        if (secure_equal(auth->sessions[i].token, token)) {
            auth->sessions[i].expires_at = now + HTTP_SESSION_TTL_SEC;
            return 1;
        }
    }
    return 0;
}

static int request_presented_token(const char *req, const char *query,
                                   char *out, size_t outlen)
{
    if (request_header_bearer_token(req, out, outlen)) return 1;
    if (http_header_value(req, "X-Auth-Token", out, outlen)) return 1;
    return query_value(query, "access_token", out, outlen);
}

static int ipv4_addr_is_lan(uint32_t addr_be)
{
    uint32_t ip = ntohl(addr_be);
    if ((ip & 0xff000000U) == 0x0a000000U) return 1;
    if ((ip & 0xfff00000U) == 0xac100000U) return 1;
    if ((ip & 0xffff0000U) == 0xc0a80000U) return 1;
    if ((ip & 0xffff0000U) == 0xa9fe0000U) return 1;
    if ((ip & 0xffc00000U) == 0x64400000U) return 1;
    return (ip & 0xff000000U) == 0x7f000000U;
}

static int peer_is_allowed_lan(const struct sockaddr_in *peer)
{
    return peer && ipv4_addr_is_lan(peer->sin_addr.s_addr);
}

static int peer_ipv4_string(const struct sockaddr_in *peer, char *out, size_t outlen)
{
    return peer && out && outlen >= INET_ADDRSTRLEN &&
        inet_ntop(AF_INET, &peer->sin_addr, out, (socklen_t)outlen) != NULL;
}

static int auth_value_is_success_text(const char *value)
{
    if (!value || !*value) return 0;
    return !strcasecmp(value, "success") || !strcasecmp(value, "ok") ||
           !strcasecmp(value, "true") || !strcasecmp(value, "pass") ||
           !strcasecmp(value, "passed") || !strcasecmp(value, "logged") ||
           !strcasecmp(value, "logined") || !strcasecmp(value, "done");
}

static int auth_value_is_failure_text(const char *value)
{
    if (!value || !*value) return 0;
    return !strcasecmp(value, "fail") || !strcasecmp(value, "failed") ||
           !strcasecmp(value, "false") || !strcasecmp(value, "error") ||
           !strcasecmp(value, "denied") || !strcasecmp(value, "forbidden") ||
           !strcasecmp(value, "invalid") || !strcasecmp(value, "wrong") ||
           !strcasecmp(value, "unauthorized");
}

static int auth_reply_field_string(const char *reply, const char *key,
                                   char *out, size_t outlen)
{
    if (!json_get(reply, key, out, outlen)) return 0;
    trim_ascii_ws_inplace(out);
    return 1;
}

static int auth_reply_indicates_success(const char *reply)
{
    static const char *token_keys[] = {"webtoken", "token", "auth_token", "secondary_auth_token"};
    char value[128];
    if (!reply || !*reply) return 0;
    for (size_t i = 0; i < sizeof token_keys / sizeof token_keys[0]; i++)
        if (auth_reply_field_string(reply, token_keys[i], value, sizeof value) && value[0]) return 1;
    if (auth_reply_field_string(reply, "success", value, sizeof value) ||
        auth_reply_field_string(reply, "ok", value, sizeof value) ||
        auth_reply_field_string(reply, "web_login_flag", value, sizeof value) ||
        auth_reply_field_string(reply, "login_flag", value, sizeof value))
        return !strcmp(value, "1") || auth_value_is_success_text(value);
    if (auth_reply_field_string(reply, "code", value, sizeof value) ||
        auth_reply_field_string(reply, "errno", value, sizeof value) ||
        auth_reply_field_string(reply, "ret", value, sizeof value) ||
        auth_reply_field_string(reply, "result", value, sizeof value)) {
        char *end = NULL;
        long numeric = strtol(value, &end, 10);
        if (end && !*end) return numeric == 0;
        if (auth_value_is_success_text(value)) return 1;
        if (auth_value_is_failure_text(value)) return 0;
    }
    if (auth_reply_field_string(reply, "status", value, sizeof value) ||
        auth_reply_field_string(reply, "state", value, sizeof value) ||
        auth_reply_field_string(reply, "msg", value, sizeof value) ||
        auth_reply_field_string(reply, "message", value, sizeof value)) {
        if (auth_value_is_success_text(value)) return 1;
        if (auth_value_is_failure_text(value)) return 0;
    }
    return (auth_reply_field_string(reply, "username", value, sizeof value) ||
            auth_reply_field_string(reply, "user", value, sizeof value)) && value[0];
}

static int auth_verify_password_login(const char *username, const char *password)
{
    char args[HTTP_AUTH_ARG_MAX];
    char reply[HTTP_AUTH_REPLY_MAX];
    struct buf b = {args, sizeof args, 0};
    if (!username || !*username || !password) return 0;
    bappend(&b, "{\"username\":\""); bappend_json_esc(&b, username);
    bappend(&b, "\",\"password\":\""); bappend_json_esc(&b, password); bappend(&b, "\"}");
    return run_ubus("zwrt_web", "web_login", args, reply, sizeof reply) == 0 &&
        auth_reply_indicates_success(reply);
}

static int auth_verify_vendor_webtoken(const char *webtoken, int mode,
                                       const char *remote_addr, const char *tag)
{
    char args[HTTP_AUTH_ARG_MAX];
    char reply[HTTP_AUTH_REPLY_MAX];
    struct buf b = {args, sizeof args, 0};
    if (!webtoken || !*webtoken || !remote_addr || !*remote_addr || !tag || !*tag) return 0;
    bappend(&b, "{\"webtoken\":\""); bappend_json_esc(&b, webtoken);
    bappend(&b, "\",\"zmode\":%d,\"web_remote_addr\":\"", mode);
    bappend_json_esc(&b, remote_addr); bappend(&b, "\",\"z-tag\":\"");
    bappend_json_esc(&b, tag); bappend(&b, "\"}");
    if (run_ubus("zwrt_web", "webtoken_check", args, reply, sizeof reply) == 0 &&
        auth_reply_indicates_success(reply)) return 1;
    return run_ubus("zwrt_web", "web_security_check", args, reply, sizeof reply) == 0 &&
        auth_reply_indicates_success(reply);
}

static int auth_route_is_open(const struct http_listener *listener,
                              const char *method, const char *path)
{
    return listener && listener->lan_only && method && path && !strcmp(method, "POST") &&
        (!strcmp(path, "/auth/login") || !strcmp(path, "/auth/exchange"));
}

static void write_auth_json(int fd, int code, const char *status, const char *body)
{
    (void)write_http_json_status(fd, code, status, body, strlen(body));
}

static void handle_auth_login(int fd, const char *req, const char *query,
                              struct auth_state *auth)
{
    char username[HTTP_AUTH_PARAM_MAX], password[HTTP_AUTH_PARAM_MAX];
    char access_token[HTTP_TOKEN_MAX], body[512];
    time_t expires_at = 0;
    if (!request_login_credentials(req, query, username, sizeof username,
                                   password, sizeof password)) {
        write_auth_json(fd, 400, "Bad Request", "{\"ok\":false,\"error\":\"missing_credentials\"}");
        return;
    }
    if (!auth_verify_password_login(username, password)) {
        write_auth_json(fd, 401, "Unauthorized", "{\"ok\":false,\"error\":\"invalid_credentials\"}");
        return;
    }
    if (auth_issue_session(auth, username, access_token, sizeof access_token, &expires_at) < 0) {
        write_auth_json(fd, 500, "Internal Server Error", "{\"ok\":false,\"error\":\"token_issue_failed\"}");
        return;
    }
    snprintf(body, sizeof body,
             "{\"ok\":true,\"token_type\":\"Bearer\",\"access_token\":\"%s\","
             "\"expires_in\":%d,\"expires_at\":%lld}",
             access_token, HTTP_SESSION_TTL_SEC, (long long)expires_at);
    write_auth_json(fd, 200, "OK", body);
}

static void handle_auth_exchange(int fd, const char *req, const char *query,
                                 const struct sockaddr_in *peer, struct auth_state *auth)
{
    char webtoken[HTTP_AUTH_HEADER_MAX], tag[HTTP_AUTH_PARAM_MAX];
    char remote_addr[INET_ADDRSTRLEN], access_token[HTTP_TOKEN_MAX], body[512];
    time_t expires_at = 0;
    int mode = 0;
    if (!request_vendor_webtoken(req, query, webtoken, sizeof webtoken,
                                 tag, sizeof tag, &mode)) {
        write_auth_json(fd, 400, "Bad Request", "{\"ok\":false,\"error\":\"missing_webtoken\"}");
        return;
    }
    if (!peer_ipv4_string(peer, remote_addr, sizeof remote_addr)) {
        write_auth_json(fd, 500, "Internal Server Error", "{\"ok\":false,\"error\":\"peer_addr_unavailable\"}");
        return;
    }
    if (!auth_verify_vendor_webtoken(webtoken, mode, remote_addr, tag)) {
        write_auth_json(fd, 401, "Unauthorized", "{\"ok\":false,\"error\":\"invalid_webtoken\"}");
        return;
    }
    if (auth_issue_session(auth, "webtoken", access_token, sizeof access_token, &expires_at) < 0) {
        write_auth_json(fd, 500, "Internal Server Error", "{\"ok\":false,\"error\":\"token_issue_failed\"}");
        return;
    }
    snprintf(body, sizeof body,
             "{\"ok\":true,\"token_type\":\"Bearer\",\"access_token\":\"%s\","
             "\"expires_in\":%d,\"expires_at\":%lld}",
             access_token, HTTP_SESSION_TTL_SEC, (long long)expires_at);
    write_auth_json(fd, 200, "OK", body);
}

static int request_is_authorized(const char *req, const char *query,
                                 struct auth_state *auth)
{
    char token[HTTP_TOKEN_MAX];
    return request_presented_token(req, query, token, sizeof token) &&
        auth_token_is_valid(auth, token);
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

static void accept_ready_http_clients(const struct http_listener *listener,
                                      struct sse_client *clients, size_t nclients,
                                      const char *snap, size_t snap_len,
                                      struct auth_state *auth)
{
    char req[HTTP_REQ_MAX];
    char method[16];
    char path[HTTP_PATH_MAX];
    char control_response[65536];

    for (;;) {
        struct sockaddr_in peer;
        socklen_t peer_len = sizeof peer;
        int cli_fd = accept(listener->fd, (struct sockaddr *)&peer, &peer_len);
        if (cli_fd < 0) {
            if (errno == EINTR) continue;
            if (errno == EAGAIN || errno == EWOULDBLOCK) return;
            perror("accept");
            return;
        }

        if (listener->lan_only && !peer_is_allowed_lan(&peer)) {
            write_http_error(cli_fd, 403, "Forbidden");
            close(cli_fd);
            continue;
        }

        int read_rc = read_http_request(cli_fd, req, sizeof req, 2000);
        if (read_rc < 0 ||
            parse_request_line(req, method, sizeof method, path, sizeof path) < 0) {
            write_http_error(cli_fd, read_rc == -2 ? 413 : 400,
                             read_rc == -2 ? "Payload Too Large" : "Bad Request");
            close(cli_fd);
            continue;
        }

        char *query = strchr(path, '?');
        int open_auth_route;
        if (query) {
            *query++ = 0;
        }

        open_auth_route = auth_route_is_open(listener, method, path);

        if (!strcmp(path, "/healthz")) {
            if (strcmp(method, "GET") != 0) write_http_error(cli_fd, 405, "Method Not Allowed");
            else (void)write_http_text(cli_fd, "text/plain; charset=utf-8", "ok\n");
            close(cli_fd);
            continue;
        }

        if (!strcmp(path, "/") || !strcmp(path, "/index") || !strcmp(path, "/index.txt")) {
            if (strcmp(method, "GET") != 0) write_http_error(cli_fd, 405, "Method Not Allowed");
            else (void)write_http_text(cli_fd, "text/plain; charset=utf-8",
                                      "zwrt-datad HTTP API\n"
                                      "GET  /state        -> current JSON snapshot\n"
                                      "GET  /events       -> SSE state stream\n"
                                      "GET  /capabilities -> device control capabilities\n"
                                      "POST /control      -> allow-listed device control\n"
                                      "POST /auth/login   -> Basic login on LAN listener\n"
                                      "POST /auth/exchange -> vendor token exchange on LAN listener\n"
                                      "GET  /healthz      -> ok\n");
            close(cli_fd);
            continue;
        }

        if (listener->auth_required && !open_auth_route &&
            !request_is_authorized(req, query, auth)) {
            write_http_unauthorized(cli_fd);
            close(cli_fd);
            continue;
        }

        if (open_auth_route && !strcmp(path, "/auth/login")) {
            handle_auth_login(cli_fd, req, query, auth);
            close(cli_fd);
            continue;
        }

        if (open_auth_route && !strcmp(path, "/auth/exchange")) {
            handle_auth_exchange(cli_fd, req, query, &peer, auth);
            close(cli_fd);
            continue;
        }

        if (!strcmp(path, "/state")) {
            if (strcmp(method, "GET") != 0) write_http_error(cli_fd, 405, "Method Not Allowed");
            else (void)write_http_json(cli_fd, snap, snap_len);
            close(cli_fd);
            continue;
        }

        if (!strcmp(path, "/capabilities")) {
            const char *body = control_capabilities_json();
            if (strcmp(method, "GET") != 0) write_http_error(cli_fd, 405, "Method Not Allowed");
            else (void)write_http_json(cli_fd, body, strlen(body));
            close(cli_fd);
            continue;
        }

        if (!strcmp(path, "/control")) {
            char *body = strstr(req, "\r\n\r\n");
            struct control_result control_status;
            const char *http_status = "OK";
            if (strcmp(method, "POST") != 0) {
                write_http_error(cli_fd, 405, "Method Not Allowed");
                close(cli_fd);
                continue;
            }
            body = body ? body + 4 : NULL;
            control_status = control_execute(body, control_response, sizeof control_response);
            if (control_status.http_status == 400) http_status = "Bad Request";
            else if (control_status.http_status == 404) http_status = "Not Found";
            else if (control_status.http_status == 502) http_status = "Bad Gateway";
            (void)write_http_json_status(cli_fd, control_status.http_status, http_status,
                                         control_response, strlen(control_response));
            close(cli_fd);
            if (control_status.refresh_state) g_state_refresh_req = 1;
            continue;
        }

        if (!strcmp(path, "/events")) {
            if (strcmp(method, "GET") != 0) {
                write_http_error(cli_fd, 405, "Method Not Allowed");
                close(cli_fd);
                continue;
            }
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

static void wait_with_http(const struct http_listener *local_listener,
                           const struct http_listener *lan_listener,
                           struct sse_client *clients, size_t nclients,
                           const char *snap, size_t snap_len, int wait_ms,
                           struct auth_state *auth)
{
    struct timespec deadline;
    clock_gettime(CLOCK_MONOTONIC, &deadline);
    deadline.tv_sec += wait_ms / 1000;
    deadline.tv_nsec += (long)(wait_ms % 1000) * 1000000L;
    if (deadline.tv_nsec >= 1000000000L) {
        deadline.tv_sec += 1;
        deadline.tv_nsec -= 1000000000L;
    }

    while (g_run && !g_state_refresh_req) {
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
        int rc, maxfd = -1;

        FD_ZERO(&rfds);
        if (local_listener && local_listener->fd >= 0) {
            FD_SET(local_listener->fd, &rfds);
            maxfd = local_listener->fd;
        }
        if (lan_listener && lan_listener->fd >= 0) {
            FD_SET(lan_listener->fd, &rfds);
            if (lan_listener->fd > maxfd) maxfd = lan_listener->fd;
        }
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

        if (local_listener && local_listener->fd >= 0 && FD_ISSET(local_listener->fd, &rfds))
            accept_ready_http_clients(local_listener, clients, nclients, snap, snap_len, auth);
        if (lan_listener && lan_listener->fd >= 0 && FD_ISSET(lan_listener->fd, &rfds))
            accept_ready_http_clients(lan_listener, clients, nclients, snap, snap_len, auth);
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
    const char *lan_bind_addr = NULL;
    const char *auth_token_file = HTTP_AUTH_TOKEN_FILE;
    int auth_token_file_explicit = 0;
    int port = HTTP_PORT;
    int lan_port = HTTP_LAN_PORT;
    int sim_poll_every, interface_poll_every, qos_retry_every, qos_retry_left = 0;
    char sim_sig[160];
    int sim_sig_valid;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--once")) once = 1;
        else if (!strcmp(argv[i], "-i") && i + 1 < argc) interval_ms = atoi(argv[++i]);
        else if ((!strcmp(argv[i], "-b") || !strcmp(argv[i], "--bind")) && i + 1 < argc)
            bind_addr = argv[++i];
        else if ((!strcmp(argv[i], "-p") || !strcmp(argv[i], "--port")) && i + 1 < argc)
            port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--lan-bind") && i + 1 < argc)
            lan_bind_addr = argv[++i];
        else if (!strcmp(argv[i], "--lan-port") && i + 1 < argc)
            lan_port = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--auth-token-file") && i + 1 < argc) {
            auth_token_file = argv[++i];
            auth_token_file_explicit = 1;
        }
        else if (!strcmp(argv[i], "-h") || !strcmp(argv[i], "--help")) {
            puts("usage: zwrt-datad [--once] [-i ms] [-b addr] [-p port] "
                 "[--lan-bind addr] [--lan-port port] [--auth-token-file path]");
            return 0;
        } else {
            fprintf(stderr, "unknown or incomplete option: %s\n", argv[i]);
            return 2;
        }
    }
    if (interval_ms <= 0) interval_ms = 1000;

    sim_poll_every = (SIM_POLL_MS + interval_ms - 1) / interval_ms;
    interface_poll_every = sim_poll_every;
    qos_retry_every = (SIM_POLL_MS + interval_ms - 1) / interval_ms;
    if (sim_poll_every < 1) sim_poll_every = 1;
    if (interface_poll_every < 1) interface_poll_every = 1;
    if (qos_retry_every < 1) qos_retry_every = 1;

    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);
    signal(SIGUSR1, on_qos_signal);
    signal(SIGPIPE, SIG_IGN);

    struct http_listener local_listener = {-1, 0, 0};
    struct http_listener lan_listener = {-1, 1, 1};
    struct auth_state auth;
    struct sse_client clients[HTTP_MAX_CLIENTS];
    for (size_t i = 0; i < HTTP_MAX_CLIENTS; i++) clients[i].fd = -1;
    auth_state_init(&auth);

    if (!once) {
        int token_loaded = auth_load_static_token(&auth, auth_token_file) == 0;
        if (lan_bind_addr && *lan_bind_addr) {
            local_listener.auth_required = 0;
        } else if (auth_token_file_explicit) {
            if (!token_loaded) {
                fprintf(stderr, "cannot read a non-empty auth token from %s\n", auth_token_file);
                return 1;
            }
            local_listener.auth_required = 1;
        }
        if (strcmp(bind_addr, "127.0.0.1") && strcmp(bind_addr, "::1") &&
            !local_listener.auth_required)
            fprintf(stderr, "warning: non-loopback local binding without authentication\n");

        local_listener.fd = open_server_socket(bind_addr, port);
        if (local_listener.fd < 0) return 1;
        if (lan_bind_addr && *lan_bind_addr) {
            lan_listener.fd = open_server_socket(lan_bind_addr, lan_port);
            if (lan_listener.fd < 0) {
                close(local_listener.fd);
                return 1;
            }
        }
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
    scan_qos_file(KEY_LOG_ROTATED_PATH, 0, NULL, 0);
    refresh_qos_cache();
    sim_sig_valid = read_sim_signature(sim_sig, sizeof sim_sig);

    char snap[SNAP_MAX];
    static char last_snap[SNAP_MAX];
    static size_t last_snap_len;
    long cycle = 0;
    long last_sms_unread = read_sms_unread_count(g_sms_unread_cache);
    if (last_sms_unread >= 0) g_sms_unread_cache = last_sms_unread;

    do {
        int force_refresh = g_state_refresh_req;
        g_state_refresh_req = 0;
        if (cycle % 3600 == 0 && cycle != 0) {
            run_ubus("system", "board", NULL, board, sizeof board);
            run_ubus("zwrt_zte_mdm.api", "get_zwrt_common_info", NULL, common, sizeof common);
            run_ubus("zwrt_zte_mdm.api", "get_imei", NULL, imei, sizeof imei);
        }

        if (cycle == 0 || cycle % sim_poll_every == 0) {
            char cur_sig[160];
            int cur_valid = read_sim_signature(cur_sig, sizeof cur_sig);
            if (cur_valid != sim_sig_valid || (cur_valid && strcmp(cur_sig, sim_sig) != 0)) {
                if (cur_valid) copy_text(sim_sig, sizeof sim_sig, cur_sig);
                else sim_sig[0] = 0;
                sim_sig_valid = cur_valid;
                rescan_qos_cache();
                g_qos_refresh_req = 0;
                g_sms_list_valid = 0;
                qos_retry_left = qos_cache_has_values() ? 0 :
                    (QOS_RETRY_MS + interval_ms - 1) / interval_ms;
            }
        }

        if (force_refresh || cycle == 0 || cycle % interface_poll_every == 0)
            refresh_interface_cache();

        if (g_qos_refresh_req) {
            g_qos_refresh_req = 0;
            rescan_qos_cache();
            if (qos_cache_has_values()) qos_retry_left = 0;
        } else if (qos_retry_left > 0) {
            if (qos_retry_left == 1 || (qos_retry_left % qos_retry_every) == 0)
                refresh_qos_cache();
            if (qos_cache_has_values()) qos_retry_left = 0;
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

            wait_with_http(&local_listener, &lan_listener, clients, HTTP_MAX_CLIENTS,
                           snap, snap_len, interval_ms, &auth);
        }
        cycle++;
    } while (g_run);

    for (size_t i = 0; i < HTTP_MAX_CLIENTS; i++) sse_client_close(&clients[i]);
    if (lan_listener.fd >= 0) close(lan_listener.fd);
    if (local_listener.fd >= 0) close(local_listener.fd);
    return 0;
}
