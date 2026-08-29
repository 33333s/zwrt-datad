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
#include "web_crypto.h"

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
#define SMS_UNREAD_POLL_MS 5000
#define POWER_POLL_SEC 2
#define SLOW_STATE_POLL_SEC 5

#define HTTP_BIND_ADDR "127.0.0.1"
#define HTTP_PORT 9460
#define HTTP_LAN_PORT 9461
#define HTTP_AUTH_TOKEN_FILE "/data/zwrt-datad/auth.token"
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
#define UBUS_CATALOG_MAX 131072
#define UBUS_CALL_RESULT_MAX 1048576
#define UBUS_CALL_RESPONSE_MAX (UBUS_CALL_RESULT_MAX + 4096)
#define TOPFLOW_MODEM_COUNT 3
#define TOPFLOW_EXTERNAL_MODEM_COUNT 2
#define TOPFLOW_THERMAL_POLL_SEC 30
#define TOPFLOW_EXTERNAL_QOS_POLL_SEC 60
#define TOPFLOW_V3E_TEMP_PATH "/sys/devices/virtual/power/zte_power/adc2_temp"
#define TOPFLOW_FAN_PWM_PATH "/sys/class/hwmon/hwmon0/pwm1"
#define TOPFLOW_FAN_RPM_PATH "/sys/class/hwmon/hwmon0/fan1_input"
#define TOPFLOW_FAN_THERMAL_ENABLE_PATH "/sys/class/hwmon/hwmon0/device/thermal_enable"
#define TOPFLOW_LIQUID_THERMAL_ENABLE_PATH "/sys/class/leds/aw_vibrator/thermal_enable"
#define TOPFLOW_THERMAL_ROOT "/sys/class/thermal"
#define TOPFLOW_COOLING_CONFIG "/data/zwrt-datad/cooling.conf"
#define TOPFLOW_CUSTOM_CURVE_MAX 8
#define TOPFLOW_ICG_CONFIG "/home/icg/icg.conf"
#define TOPFLOW_PROC_ROOT "/proc"
#define TOPFLOW_PROC_NET_TCP "/proc/net/tcp"
#define TOPFLOW_AGGREGATION_TRAFFIC_POLL_SEC 60
#define TOPFLOW_AGGREGATION_FLOW_TIMEOUT_MS 1500
#define TOPFLOW_AGGREGATION_MAX_PATHS 4
#define TOPFLOW_ICG_MAX_SOCKETS 256
#define TOPFLOW_ICG_MAX_TCP_ENTRIES 512

static volatile sig_atomic_t g_run = 1;
static void on_signal(int s) { (void)s; g_run = 0; }
static volatile sig_atomic_t g_qos_refresh_req = 0;
static void on_qos_signal(int s) { (void)s; g_qos_refresh_req = 1; }
static volatile sig_atomic_t g_state_refresh_req = 0;
static int g_sample_interval_ms = 1000;

static char g_sms_list_cache[SMS_LIST_MAX] = "[]";
static int g_sms_list_valid;
static int g_sms_interface_detected;
static long g_sms_unread_cache = 0;
static char g_sim_cache[RAW_MAX];
static char g_uci_device_info[RAW_MAX] = "{}";
static char g_uci_device_info_no_battery[RAW_MAX] = "{}";
static char g_lan_interface[RAW_MAX];
static char g_wan4_interface[RAW_MAX];
static char g_wan6_interface[RAW_MAX];
static char g_lan_runtime[RAW_MAX];
static char g_cellular_runtime[RAW_MAX];
static char g_traffic_accounting[RAW_MAX];
static char g_traffic_limit[4096];
static char g_traffic_clear_day[4096];
static int g_topflow_multimodem_enabled;
static int g_full_ubus_enabled;
static char g_topflow_msim_netinfo[RAW_MAX];
static char g_topflow_v3t_sim[RAW_MAX];
static char g_topflow_wwaniface[TOPFLOW_EXTERNAL_MODEM_COUNT][RAW_MAX];
static char g_topflow_traffic[TOPFLOW_EXTERNAL_MODEM_COUNT][RAW_MAX];
static char g_topflow_wan4[TOPFLOW_MODEM_COUNT][RAW_MAX];
static char g_topflow_wan6[TOPFLOW_MODEM_COUNT][RAW_MAX];
static long g_topflow_external_temp[TOPFLOW_EXTERNAL_MODEM_COUNT];
static int g_topflow_external_temp_valid[TOPFLOW_EXTERNAL_MODEM_COUNT];
static time_t g_topflow_external_temp_sampled_at[TOPFLOW_EXTERNAL_MODEM_COUNT];
static time_t g_topflow_thermal_next_at;
static char g_topflow_mwan3_status[RAW_MAX];
static char g_topflow_mwan3_config[RAW_MAX];
static time_t g_topflow_mwan3_config_next_at;
static char g_topflow_aggregation_flow[4096];
static char g_topflow_icg_server_ip[64];
static int g_topflow_icg_tcp_port;
static int g_topflow_icg_udp_start_port;
static int g_topflow_icg_tcp_tunnel_count;
static int g_topflow_icg_provisioned;
static int g_topflow_icg_process_running;
static int g_topflow_mwan3_running;
static int g_topflow_icg_server_runtime;
static time_t g_topflow_aggregation_traffic_next_at;
static char g_ubus_catalog[UBUS_CATALOG_MAX];
static char g_ubus_call_args[HTTP_REQ_MAX];
static char g_ubus_call_result[UBUS_CALL_RESULT_MAX];
static char g_ubus_call_response[UBUS_CALL_RESPONSE_MAX];

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

enum optional_section_mode {
    OPTIONAL_SECTION_AUTO = -1,
    OPTIONAL_SECTION_HIDDEN = 0,
    OPTIONAL_SECTION_VISIBLE = 1
};

struct device_template_spec {
    const char *id;
    const char *label;
    int supported;
    int full_ubus;
    int thermal_zones;
    enum wifi_source_mode wifi_mode;
    enum client_source_mode client_mode;
    enum temp_source_mode temp_mode;
    enum network_source_mode network_mode;
    enum traffic_source_mode traffic_mode;
    enum optional_section_mode battery_section;
    enum optional_section_mode wifi_section;
    enum optional_section_mode nfc_section;
    enum optional_section_mode sms_section;
};

static const struct device_template_spec TEMPLATE_U60_MU5250 = {
    "MU5250",
    "MU5250",
    1,
    1,
    1,
    WIFI_SOURCE_U60_MAIN_2G,
    CLIENT_SOURCE_DHCP_ONLY,
    TEMP_SOURCE_U60_UBUS_ONLY,
    NETWORK_SOURCE_NWINFO_UBUS_ONLY,
    TRAFFIC_SOURCE_CID1,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO
};

static const struct device_template_spec TEMPLATE_G5PRO_MC8532B = {
    "MC8532B",
    "MC8532B",
    1,
    1,
    1,
    WIFI_SOURCE_COMPAT_AUTO,
    CLIENT_SOURCE_DHCP_THEN_ROUTER,
    TEMP_SOURCE_COMPAT_FALLBACK,
    NETWORK_SOURCE_NWINFO_UBUS_ONLY,
    TRAFFIC_SOURCE_CID1,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO
};

static const struct device_template_spec TEMPLATE_TOPFLOW_MU5252 = {
    "MU5252",
    "MU5252",
    1,
    1,
    1,
    WIFI_SOURCE_COMPAT_AUTO,
    CLIENT_SOURCE_DHCP_THEN_ROUTER,
    TEMP_SOURCE_U60_UBUS_ONLY,
    NETWORK_SOURCE_MU5252_UCI_FALLBACK,
    TRAFFIC_SOURCE_CID1_ACTIVE_SUBID,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO
};

static const struct device_template_spec TEMPLATE_G5MAX_MC7523 = {
    "MC7523",
    "MC7523",
    1,
    1,
    1,
    WIFI_SOURCE_COMPAT_AUTO,
    CLIENT_SOURCE_DHCP_THEN_ROUTER,
    TEMP_SOURCE_COMPAT_FALLBACK,
    NETWORK_SOURCE_NWINFO_UBUS_ONLY,
    TRAFFIC_SOURCE_CID1,
    OPTIONAL_SECTION_HIDDEN,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO
};

static const struct device_template_spec TEMPLATE_LEGACY_COMPAT = {
    "legacy_compat",
    "Legacy compatibility fallback",
    0,
    1,
    0,
    WIFI_SOURCE_COMPAT_AUTO,
    CLIENT_SOURCE_DHCP_THEN_ROUTER,
    TEMP_SOURCE_COMPAT_FALLBACK,
    NETWORK_SOURCE_NWINFO_UBUS_ONLY,
    TRAFFIC_SOURCE_CID1,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO,
    OPTIONAL_SECTION_AUTO
};

static int optional_section_enabled(enum optional_section_mode mode, int detected)
{
    if (mode == OPTIONAL_SECTION_VISIBLE) return 1;
    if (mode == OPTIONAL_SECTION_HIDDEN) return 0;
    return detected != 0;
}

static int json_has_any_key(const char *json, const char *const *keys, size_t key_count)
{
    char value[128];
    size_t i;
    if (!json || !*json) return 0;
    for (i = 0; i < key_count; i++) {
        if (json_get(json, keys[i], value, sizeof value)) return 1;
    }
    return 0;
}

/* Run `ubus call <svc> <method> [args]` and capture stdout. 0 on output. */
static int run_ubus(const char *svc, const char *method, const char *args,
                    char *out, size_t outlen)
{
    return device_ubus_call(svc, method, args, out, outlen);
}

static int setup_sms_crypto(void)
{
    char certificate_reply[8192];
    char certificate[8192];
    char enstr[2048];
    char args[2304];
    char reply[1024];

    if (web_crypto_session_ready()) return 1;
    if (!web_crypto_init()) return 0;
    if (run_ubus("zwrt_web", "web_crt_get", NULL,
                 certificate_reply, sizeof certificate_reply) != 0 ||
        !json_get(certificate_reply, "result", certificate, sizeof certificate) ||
        !web_crypto_prepare_registration(certificate, enstr, sizeof enstr)) {
        web_crypto_reset();
        return 0;
    }
    snprintf(args, sizeof args, "{\"web_enstr\":\"%s\"}", enstr);
    if (run_ubus("zwrt_web", "web_http_enstr_set", args, reply, sizeof reply) != 0) {
        web_crypto_reset();
        return 0;
    }
    return 1;
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

static int parse_sms_list(const char *sms_reply, char *out, size_t outlen,
                          int *crypto_failed)
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
            char num_raw[4096];
            char num_decrypted[4096];
            char num[512];
            char date_raw[64];
            char date[32];
            char tag[16];
            char text_raw[SMS_TEXT_HEX_MAX];
            char text_hex[SMS_TEXT_HEX_MAX];
            char text[SMS_TEXT_UTF8_MAX];
            long id = 0;
            int unread = 0;
            int num_crypto, text_crypto;

            if (!json_get(sms_obj, "id", id_raw, sizeof id_raw)) continue;
            if (!json_get(sms_obj, "num", num_raw, sizeof num_raw) &&
                !json_get(sms_obj, "number", num_raw, sizeof num_raw)) num_raw[0] = 0;
            if (!json_get(sms_obj, "date", date_raw, sizeof date_raw)) date_raw[0] = 0;
            if (!json_get(sms_obj, "tag", tag, sizeof tag)) {
                long t = json_get_int(sms_obj, "tag", 0);
                unread = t == 1 ? 1 : 0;
            } else {
                unread = (tag[0] == '1') ? 1 : 0;
            }
            if (!json_get(sms_obj, "text", text_raw, sizeof text_raw) &&
                !json_get(sms_obj, "content", text_raw, sizeof text_raw)) text_raw[0] = 0;

            id = strtol(id_raw, NULL, 10);
            format_sms_date(date_raw, date, sizeof date);

            num_crypto = web_crypto_decrypt_envelope(num_raw, num_decrypted, sizeof num_decrypted);
            text_crypto = web_crypto_decrypt_envelope(text_raw, text_hex, sizeof text_hex);
            if (num_crypto < 0 || text_crypto < 0) {
                if (crypto_failed) *crypto_failed = 1;
                continue;
            }
            if (num_crypto > 0)
                utf16be_hex_to_utf8(num_decrypted, num, sizeof num);
            else
                copy_text(num, sizeof num, num_raw);
            if (text_crypto == 0)
                snprintf(text_hex, sizeof text_hex, "%s", text_raw);
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
    g_sms_interface_detected = 1;
    if (json_get(cap, "sms_dev_unread_num", v, sizeof v)) dev = strtol(v, NULL, 10);
    if (json_get(cap, "sms_sim_unread_num", v, sizeof v)) sim = strtol(v, NULL, 10);
    return dev + sim;
}

static int join_sms_arrays(const char *left, const char *right,
                           char *out, size_t outlen)
{
    size_t left_len = left ? strlen(left) : 0;
    size_t right_len = right ? strlen(right) : 0;
    const char *left_body = left_len >= 2 ? left + 1 : "";
    const char *right_body = right_len >= 2 ? right + 1 : "";
    size_t left_body_len = left_len >= 2 ? left_len - 2 : 0;
    size_t right_body_len = right_len >= 2 ? right_len - 2 : 0;
    int comma = left_body_len > 0 && right_body_len > 0;
    if (left_body_len + right_body_len + (size_t)comma + 3 > outlen) return 0;
    out[0] = '[';
    memcpy(out + 1, left_body, left_body_len);
    size_t pos = 1 + left_body_len;
    if (comma) out[pos++] = ',';
    memcpy(out + pos, right_body, right_body_len);
    pos += right_body_len;
    out[pos++] = ']';
    out[pos] = 0;
    return 1;
}

static const char *next_sms_object(const char *cursor, char *out, size_t outlen)
{
    const char *start;
    int depth = 0, in_string = 0, escaped = 0;
    size_t length;

    if (!cursor || !out || outlen == 0) return NULL;
    while (*cursor && (isspace((unsigned char)*cursor) || *cursor == '[' || *cursor == ','))
        cursor++;
    if (*cursor == ']' || *cursor != '{') return NULL;
    start = cursor;
    for (; *cursor; cursor++) {
        char c = *cursor;
        if (in_string) {
            if (escaped) escaped = 0;
            else if (c == '\\') escaped = 1;
            else if (c == '"') in_string = 0;
            continue;
        }
        if (c == '"') in_string = 1;
        else if (c == '{') depth++;
        else if (c == '}' && --depth == 0) {
            cursor++;
            length = (size_t)(cursor - start);
            if (length >= outlen) return NULL;
            memcpy(out, start, length);
            out[length] = 0;
            return cursor;
        }
    }
    return NULL;
}

static int merge_sms_page(const char *page, const char *cached,
                          char *out, size_t outlen)
{
    long seen[32];
    int seen_count = 0;
    int item_count = 0;
    char object[SMS_OBJECT_MAX];
    struct buf outb = { out, outlen, 0 };
    const char *sources[2] = { page, cached };

    bappend(&outb, "[");
    for (size_t source = 0; source < 2 && item_count < 32; source++) {
        const char *cursor = sources[source];
        while (item_count < 32 &&
               (cursor = next_sms_object(cursor, object, sizeof object)) != NULL) {
            long id = json_get_int(object, "id", LONG_MIN);
            int duplicate = 0;
            if (id != LONG_MIN) {
                for (int i = 0; i < seen_count; i++) {
                    if (seen[i] == id) {
                        duplicate = 1;
                        break;
                    }
                }
            }
            if (duplicate) continue;
            if (id != LONG_MIN && seen_count < (int)(sizeof seen / sizeof seen[0]))
                seen[seen_count++] = id;
            if (item_count) bappend(&outb, ",");
            bappend(&outb, "%s", object);
            item_count++;
        }
    }
    bappend(&outb, "]");
    return outb.len < outb.cap;
}

static int refresh_sms_cache(int full_refresh)
{
    static char nv_resp[SMS_RESPONSE_MAX];
    static char sim_resp[SMS_RESPONSE_MAX];
    static char nv_cache[SMS_LIST_MAX] = "[]";
    static char sim_cache[SMS_LIST_MAX] = "[]";
    static char nv_page[SMS_LIST_MAX];
    static char sim_page[SMS_LIST_MAX];
    static char next_nv[SMS_LIST_MAX];
    static char next_sim[SMS_LIST_MAX];
    static char next_cache[SMS_LIST_MAX];
    char nv_args[160];
    char sim_args[160];
    int page_size = full_refresh ? 32 : 8;
    int crypto_failed = 0;

    (void)setup_sms_crypto();
    snprintf(nv_args, sizeof nv_args,
             "{\"page\":0,\"data_per_page\":%d,\"mem_store\":1,\"tags\":10,\"order_by\":\"order by id desc\"}",
             page_size);
    snprintf(sim_args, sizeof sim_args,
             "{\"page\":0,\"data_per_page\":%d,\"mem_store\":0,\"tags\":10,\"order_by\":\"order by id desc\"}",
             page_size);
    if (run_ubus("zwrt_wms", "zte_libwms_get_sms_data",
                 nv_args,
                 nv_resp, sizeof nv_resp) != 0 ||
        run_ubus("zwrt_wms", "zte_libwms_get_sms_data",
                 sim_args,
                 sim_resp, sizeof sim_resp) != 0) {
        return 0;
    }
    g_sms_interface_detected = 1;
    if (!parse_sms_list(nv_resp, nv_page, sizeof nv_page, &crypto_failed) ||
        !parse_sms_list(sim_resp, sim_page, sizeof sim_page, &crypto_failed)) {
        return 0;
    }
    if (crypto_failed) {
        web_crypto_reset();
        return 0;
    }
    if (full_refresh) {
        copy_text(next_nv, sizeof next_nv, nv_page);
        copy_text(next_sim, sizeof next_sim, sim_page);
    } else if (!merge_sms_page(nv_page, nv_cache, next_nv, sizeof next_nv) ||
               !merge_sms_page(sim_page, sim_cache, next_sim, sizeof next_sim)) {
        return 0;
    }
    if (!join_sms_arrays(next_nv, next_sim, next_cache, sizeof next_cache)) return 0;
    copy_text(nv_cache, sizeof nv_cache, next_nv);
    copy_text(sim_cache, sizeof sim_cache, next_sim);
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
    if ((model_name && !strcmp(model_name, "MC7523")) ||
        has_prefix(hardware_version, "MC7523")) {
        return &TEMPLATE_G5MAX_MC7523;
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
static struct qos_values g_topflow_external_qos[TOPFLOW_EXTERNAL_MODEM_COUNT];
static time_t g_topflow_external_qos_sampled_at[TOPFLOW_EXTERNAL_MODEM_COUNT];
static time_t g_topflow_external_qos_next_at;

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

static int parse_topflow_external_qos(const char *text, struct qos_values *out)
{
    const char *line = text;
    int found = 0;

    if (!text || !out) return 0;
    memset(out, 0, sizeof *out);
    while (line && *line) {
        const char *end = strchr(line, '\n');
        size_t length = end ? (size_t)(end - line) : strlen(line);
        char record[512];
        int qci = 0, dl_kbps = 0, ul_kbps = 0;

        if (length >= sizeof record) length = sizeof record - 1;
        memcpy(record, line, length);
        record[length] = 0;
        if (strstr(record, "[DATA]") && contains_ci(record, "cid1") &&
            parse_int_after(record, "QCI=", &qci) &&
            parse_int_after(record, "DL_AMBR=", &dl_kbps) &&
            parse_int_after(record, "UL_AMBR=", &ul_kbps) &&
            qci > 0 && qci <= 255 && dl_kbps >= 0 && ul_kbps >= 0) {
            out->qci = qci;
            out->qci_valid = 1;
            out->ambr_dl = (double)dl_kbps / 1000.0;
            out->ambr_ul = (double)ul_kbps / 1000.0;
            out->ambr_dl_valid = 1;
            out->ambr_ul_valid = 1;
            found = 1;
        }
        line = end ? end + 1 : NULL;
    }
    return found;
}

static int topflow_external_adb_available(int index)
{
    static const char *usb_paths[TOPFLOW_EXTERNAL_MODEM_COUNT] = {"1-1", "1-2"};
    const char *adb_override = getenv("ZWRT_DATAD_ADB_BIN");
    char path[128];

    if (index < 0 || index >= TOPFLOW_EXTERNAL_MODEM_COUNT) return 0;
    if (adb_override && *adb_override) return 1;
    if (snprintf(path, sizeof path, "/sys/bus/usb/devices/%s/%s:1.3",
                 usb_paths[index], usb_paths[index]) >= (int)sizeof path)
        return 0;
    return access(path, F_OK) == 0;
}

static void refresh_topflow_external_qos_cache(time_t now)
{
    static const char *serials[TOPFLOW_EXTERNAL_MODEM_COUNT] = {
        "V3E1T12345", "V3E2T12345"
    };

    if (g_topflow_external_qos_next_at != 0 && now < g_topflow_external_qos_next_at)
        return;
    for (int i = 0; i < TOPFLOW_EXTERNAL_MODEM_COUNT; i++) {
        char lines[16384];
        struct qos_values values;
        if (!topflow_external_adb_available(i)) {
            memset(&g_topflow_external_qos[i], 0, sizeof g_topflow_external_qos[i]);
            g_topflow_external_qos_sampled_at[i] = 0;
            continue;
        }
        if (device_adb_read_qos_log(serials[i], lines, sizeof lines) == 0 &&
            parse_topflow_external_qos(lines, &values)) {
            g_topflow_external_qos[i] = values;
            g_topflow_external_qos_sampled_at[i] = now;
        }
    }
    g_topflow_external_qos_next_at = now + TOPFLOW_EXTERNAL_QOS_POLL_SEC;
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

static const char *runtime_path(const char *name, const char *fallback)
{
    const char *value = getenv(name);
    return value && *value ? value : fallback;
}

static long read_labeled_long_file(const char *path, const char *label, long def)
{
    char line[256], *start, *end;
    long value;
    if (!read_line_file(path, line, sizeof line)) return def;
    start = label && *label ? strstr(line, label) : line;
    if (!start) return def;
    start += label && *label ? strlen(label) : 0;
    while (*start == ':' || *start == '=' || isspace((unsigned char)*start)) start++;
    errno = 0;
    value = strtol(start, &end, 10);
    if (errno || end == start) return def;
    return value;
}

static int find_topflow_cooling_zone(char *out, size_t outlen)
{
    const char *fixed = getenv("ZWRT_DATAD_COOLING_ZONE_PATH");
    const char *root = runtime_path("ZWRT_DATAD_COOLING_THERMAL_ROOT", TOPFLOW_THERMAL_ROOT);
    DIR *dir;
    struct dirent *entry;
    if (fixed && *fixed) {
        snprintf(out, outlen, "%s", fixed);
        return access(out, F_OK) == 0;
    }
    dir = opendir(root);
    if (!dir) return 0;
    while ((entry = readdir(dir)) != NULL) {
        char path[PATH_MAX], type[64];
        if (strncmp(entry->d_name, "thermal_zone", 12) != 0) continue;
        if (snprintf(path, sizeof path, "%s/%s/type", root, entry->d_name) >= (int)sizeof path)
            continue;
        if (!read_line_file(path, type, sizeof type) || strcmp(type, "sys-therm-4")) continue;
        snprintf(out, outlen, "%s/%s", root, entry->d_name);
        closedir(dir);
        return 1;
    }
    closedir(dir);
    return 0;
}

static int read_uci_flag(const char *path, int fallback)
{
    char value[32];
    char *end;
    long parsed;
    if (device_uci_get(path, value, sizeof value) != 0 || !value[0]) return fallback;
    parsed = strtol(value, &end, 10);
    if (end == value) return fallback;
    return parsed != 0;
}

static long normalize_temp_reading(long raw);

struct topflow_curve_point {
    int temperature;
    int pwm;
};

static int load_topflow_custom_curve(int *fan_always_on, int *liquid_always_on,
                                     int *liquid_level,
                                     int *fan_mode, int *manual_speed_percent,
                                     struct topflow_curve_point *points, int *count)
{
    const char *config = runtime_path("ZWRT_DATAD_COOLING_CONFIG", TOPFLOW_COOLING_CONFIG);
    FILE *fp = fopen(config, "r");
    char line[128], key[64];
    int value, curve_count = 0, legacy_fan_enabled = -1, legacy_liquid_enabled = -1;
    if (fan_always_on) *fan_always_on = -1;
    if (liquid_always_on) *liquid_always_on = -1;
    if (liquid_level) *liquid_level = 1;
    if (fan_mode) *fan_mode = 0;
    if (manual_speed_percent) *manual_speed_percent = 0;
    if (count) *count = 0;
    if (!fp) return 0;
    while (fgets(line, sizeof line, fp)) {
        int index;
        if (sscanf(line, "%63[^=]=%d", key, &value) != 2) continue;
        if (!strcmp(key, "fan_enabled")) legacy_fan_enabled = value != 0;
        else if (!strcmp(key, "fan_always_on") && fan_always_on)
            *fan_always_on = value != 0;
        else if (!strcmp(key, "liquid_enabled")) legacy_liquid_enabled = value != 0;
        else if (!strcmp(key, "liquid_always_on") && liquid_always_on)
            *liquid_always_on = value != 0;
        else if (!strcmp(key, "liquid_level") && liquid_level && value >= 1 && value <= 2)
            *liquid_level = value;
        else if (!strcmp(key, "fan_mode") && fan_mode) *fan_mode = value;
        else if (!strcmp(key, "fan_speed_percent") && manual_speed_percent)
            *manual_speed_percent = value;
        else if (!strcmp(key, "custom_curve_count")) curve_count = value;
        else if (sscanf(key, "custom_temperature_%d", &index) == 1 &&
                 index >= 1 && index <= TOPFLOW_CUSTOM_CURVE_MAX)
            points[index - 1].temperature = value;
        else if (sscanf(key, "custom_pwm_%d", &index) == 1 &&
                 index >= 1 && index <= TOPFLOW_CUSTOM_CURVE_MAX)
            points[index - 1].pwm = value;
    }
    fclose(fp);
    if (fan_always_on && *fan_always_on < 0) {
        if (fan_mode && *fan_mode != 1) *fan_always_on = 0;
        else if (legacy_fan_enabled >= 0) *fan_always_on = legacy_fan_enabled;
    }
    if (liquid_always_on && *liquid_always_on < 0 && legacy_liquid_enabled >= 0)
        *liquid_always_on = legacy_liquid_enabled;
    if (count && curve_count >= 2 && curve_count <= TOPFLOW_CUSTOM_CURVE_MAX)
        *count = curve_count;
    return 1;
}

static int json_bool_or(const char *json, const char *key, int fallback)
{
    char value[16];
    if (!json_get(json, key, value, sizeof value)) return fallback;
    if (!strcasecmp(value, "true") || !strcmp(value, "1")) return 1;
    if (!strcasecmp(value, "false") || !strcmp(value, "0")) return 0;
    return fallback;
}

static double json_double_or(const char *json, const char *key, double fallback)
{
    char value[64], *end;
    double parsed;
    if (!json_get(json, key, value, sizeof value)) return fallback;
    errno = 0;
    parsed = strtod(value, &end);
    if (errno || end == value) return fallback;
    return parsed;
}

static const char *copy_next_json_object(const char *cursor, char *out, size_t outlen)
{
    const char *start;
    int depth = 0, in_string = 0, escaped = 0;
    size_t length;

    if (!cursor || !out || outlen == 0) return NULL;
    while (*cursor && (isspace((unsigned char)*cursor) || *cursor == '[' || *cursor == ','))
        cursor++;
    if (*cursor == ']' || *cursor != '{') return NULL;
    start = cursor;
    for (; *cursor; cursor++) {
        char c = *cursor;
        if (in_string) {
            if (escaped) escaped = 0;
            else if (c == '\\') escaped = 1;
            else if (c == '"') in_string = 0;
            continue;
        }
        if (c == '"') in_string = 1;
        else if (c == '{') depth++;
        else if (c == '}' && --depth == 0) {
            cursor++;
            length = (size_t)(cursor - start);
            if (length >= outlen) length = outlen - 1;
            memcpy(out, start, length);
            out[length] = 0;
            return cursor;
        }
    }
    return NULL;
}

static void trim_ascii(char *text)
{
    char *start;
    size_t length;
    if (!text) return;
    start = text;
    while (*start && isspace((unsigned char)*start)) start++;
    if (start != text) memmove(text, start, strlen(start) + 1);
    length = strlen(text);
    while (length && isspace((unsigned char)text[length - 1])) text[--length] = 0;
}

static void load_topflow_icg_config(void)
{
    const char *path = runtime_path("ZWRT_DATAD_ICG_CONFIG", TOPFLOW_ICG_CONFIG);
    FILE *fp = fopen(path, "r");
    char line[256], key[128], value[128];

    g_topflow_icg_server_ip[0] = 0;
    g_topflow_icg_tcp_port = 0;
    g_topflow_icg_udp_start_port = 0;
    if (!fp) return;
    while (fgets(line, sizeof line, fp)) {
        if (sscanf(line, " %127[^=]=%127[^\r\n]", key, value) != 2) continue;
        trim_ascii(key);
        trim_ascii(value);
        if (!strcmp(key, "AggregationServerIP"))
            copy_text(g_topflow_icg_server_ip, sizeof g_topflow_icg_server_ip, value);
        else if (!strcmp(key, "AggregationServerTcpPort"))
            g_topflow_icg_tcp_port = atoi(value);
        else if (!strcmp(key, "AggregationServerUdpStartPort"))
            g_topflow_icg_udp_start_port = atoi(value);
    }
    fclose(fp);
}

struct topflow_tcp_entry {
    uint32_t local_address;
    uint32_t remote_address;
    unsigned int local_port;
    unsigned int remote_port;
    unsigned int state;
    unsigned long inode;
};

static int proc_find_command(const char *command)
{
    const char *root = runtime_path("ZWRT_DATAD_PROC_ROOT", TOPFLOW_PROC_ROOT);
    DIR *dir = opendir(root);
    struct dirent *entry;
    if (!dir) return 0;
    while ((entry = readdir(dir)) != NULL) {
        char path[PATH_MAX], comm[128] = "";
        FILE *fp;
        char *end = NULL;
        long pid = strtol(entry->d_name, &end, 10);
        if (pid <= 0 || !end || *end) continue;
        snprintf(path, sizeof path, "%s/%ld/comm", root, pid);
        fp = fopen(path, "r");
        if (!fp) continue;
        if (fgets(comm, sizeof comm, fp)) trim_ascii(comm);
        fclose(fp);
        if (!strcmp(comm, command)) {
            closedir(dir);
            return (int)pid;
        }
    }
    closedir(dir);
    return 0;
}

static int socket_inode_owned(unsigned long inode, const unsigned long *owned,
                              size_t owned_count)
{
    for (size_t i = 0; i < owned_count; i++)
        if (owned[i] == inode) return 1;
    return 0;
}

static size_t load_topflow_icg_socket_inodes(unsigned long *owned, size_t cap)
{
    const char *fixture = getenv("ZWRT_DATAD_ICG_SOCKET_INODES");
    const char *root = runtime_path("ZWRT_DATAD_PROC_ROOT", TOPFLOW_PROC_ROOT);
    size_t count = 0;
    int pid;
    if (fixture && *fixture) {
        const char *cursor = fixture;
        while (*cursor && count < cap) {
            char *end = NULL;
            unsigned long inode = strtoul(cursor, &end, 10);
            if (end == cursor) break;
            owned[count++] = inode;
            cursor = *end ? end + 1 : end;
        }
        return count;
    }
    pid = proc_find_command("zte_icg_agg");
    if (pid > 0) {
        char directory[PATH_MAX];
        DIR *dir;
        struct dirent *entry;
        snprintf(directory, sizeof directory, "%s/%d/fd", root, pid);
        dir = opendir(directory);
        if (!dir) return 0;
        while ((entry = readdir(dir)) != NULL && count < cap) {
            char target[128];
            ssize_t length;
            unsigned long inode;
            if (entry->d_name[0] == '.') continue;
            length = readlinkat(dirfd(dir), entry->d_name, target, sizeof target - 1);
            if (length <= 0) continue;
            target[length] = 0;
            if (sscanf(target, "socket:[%lu]", &inode) == 1)
                owned[count++] = inode;
        }
        closedir(dir);
    }
    return count;
}

static int parse_topflow_tcp_entry(const char *line, struct topflow_tcp_entry *entry)
{
    return sscanf(line,
                  " %*u: %8x:%4x %8x:%4x %2x %*s %*s %*s %*u %*u %lu",
                  &entry->local_address, &entry->local_port,
                  &entry->remote_address, &entry->remote_port,
                  &entry->state, &entry->inode) == 6;
}

static void proc_ipv4_text(uint32_t value, char *out, size_t outlen)
{
    struct in_addr address;
    address.s_addr = value;
    if (!inet_ntop(AF_INET, &address, out, outlen)) out[0] = 0;
}

static int count_topflow_icg_tcp_tunnels(void)
{
    const char *path = runtime_path("ZWRT_DATAD_PROC_NET_TCP", TOPFLOW_PROC_NET_TCP);
    struct topflow_tcp_entry entries[TOPFLOW_ICG_MAX_TCP_ENTRIES];
    unsigned long owned[TOPFLOW_ICG_MAX_SOCKETS];
    unsigned int listeners[32];
    uint32_t peers_address[TOPFLOW_ICG_MAX_TCP_ENTRIES];
    unsigned int peers_port[TOPFLOW_ICG_MAX_TCP_ENTRIES];
    int peers_count[TOPFLOW_ICG_MAX_TCP_ENTRIES];
    size_t owned_count, entry_count = 0, listener_count = 0, peer_count = 0;
    FILE *fp;
    char line[512];
    int count = 0, best_peer = -1;

    g_topflow_icg_server_runtime = 0;
    owned_count = load_topflow_icg_socket_inodes(owned, TOPFLOW_ICG_MAX_SOCKETS);
    g_topflow_icg_process_running = owned_count > 0 || proc_find_command("zte_icg_agg") > 0;
    if (!owned_count) return 0;
    fp = fopen(path, "r");
    if (!fp) return 0;
    while (entry_count < TOPFLOW_ICG_MAX_TCP_ENTRIES &&
           fgets(line, sizeof line, fp)) {
        if (parse_topflow_tcp_entry(line, &entries[entry_count])) entry_count++;
    }
    fclose(fp);
    for (size_t i = 0; i < entry_count; i++) {
        if (entries[i].state == 0x0a &&
            socket_inode_owned(entries[i].inode, owned, owned_count) &&
            listener_count < sizeof listeners / sizeof listeners[0])
            listeners[listener_count++] = entries[i].local_port;
    }
    for (size_t i = 0; i < entry_count; i++) {
        int inbound = 0;
        if (entries[i].state != 0x01 ||
            !socket_inode_owned(entries[i].inode, owned, owned_count)) continue;
        for (size_t j = 0; j < listener_count; j++)
            if (entries[i].local_port == listeners[j]) inbound = 1;
        if (inbound) continue;
        count++;
        size_t peer;
        for (peer = 0; peer < peer_count; peer++)
            if (peers_address[peer] == entries[i].remote_address &&
                peers_port[peer] == entries[i].remote_port) break;
        if (peer == peer_count && peer_count < TOPFLOW_ICG_MAX_TCP_ENTRIES) {
            peers_address[peer] = entries[i].remote_address;
            peers_port[peer] = entries[i].remote_port;
            peers_count[peer] = 0;
            peer_count++;
        }
        if (peer < peer_count) {
            peers_count[peer]++;
            if (best_peer < 0 || peers_count[peer] > peers_count[best_peer])
                best_peer = (int)peer;
        }
    }
    if (best_peer >= 0) {
        proc_ipv4_text(peers_address[best_peer], g_topflow_icg_server_ip,
                       sizeof g_topflow_icg_server_ip);
        g_topflow_icg_tcp_port = (int)peers_port[best_peer];
        g_topflow_icg_server_runtime = 1;
    }
    return count;
}

static int topflow_decimal_bytes(const char *value)
{
    const unsigned char *cursor = (const unsigned char *)value;
    if (!cursor || !*cursor) return 0;
    while (*cursor) {
        if (!isdigit(*cursor)) return 0;
        cursor++;
    }
    return 1;
}

static int load_topflow_aggregation_traffic_uci(void)
{
    char remaining[128] = "", today_used[128] = "";
    int has_remaining =
        device_uci_get("zwrt_router.icgmwan.residual_flow", remaining,
                       sizeof remaining) == 0 &&
        topflow_decimal_bytes(remaining);
    int has_today_used =
        device_uci_get("zwrt_router.icgmwan.count_flow_today", today_used,
                       sizeof today_used) == 0 &&
        topflow_decimal_bytes(today_used);

    if (!has_remaining && !has_today_used) return 0;
    snprintf(g_topflow_aggregation_flow, sizeof g_topflow_aggregation_flow,
             "{\"residual_flow\":\"%s\",\"count_flow_today\":\"%s\",\"source\":\"uci\"}",
             has_remaining ? remaining : "", has_today_used ? today_used : "");
    return 1;
}

static void refresh_topflow_aggregation_cache(void)
{
    char next[RAW_MAX], mode[32] = "", icg_id[128] = "";
    time_t now = time(NULL);
    int enabled;

    if (run_ubus("mwan3", "status", NULL, next, sizeof next) == 0)
        copy_text(g_topflow_mwan3_status, sizeof g_topflow_mwan3_status, next);
    if (now >= g_topflow_mwan3_config_next_at) {
        g_topflow_mwan3_config_next_at = now + 5;
        if (device_uci_show("mwan3", next, sizeof next) == 0)
            copy_text(g_topflow_mwan3_config, sizeof g_topflow_mwan3_config, next);
    }
    load_topflow_icg_config();
    g_topflow_mwan3_running = proc_find_command("mwan3track") > 0;
    if (getenv("ZWRT_DATAD_MWAN3_RUNNING"))
        g_topflow_mwan3_running = atoi(getenv("ZWRT_DATAD_MWAN3_RUNNING")) != 0;
    g_topflow_icg_tcp_tunnel_count = count_topflow_icg_tcp_tunnels();
    g_topflow_icg_provisioned =
        device_uci_get("zwrt_router.icgmwan.IcgDevId", icg_id, sizeof icg_id) == 0 &&
        icg_id[0] != 0;

    if (device_uci_get("zwrt_router.network.opms_wan_mode", mode, sizeof mode) != 0)
        mode[0] = 0;
    enabled = !strcmp(mode, "SMULTIWAN");
    (void)load_topflow_aggregation_traffic_uci();
    if (!enabled || !g_topflow_icg_provisioned ||
        now < g_topflow_aggregation_traffic_next_at) return;
    g_topflow_aggregation_traffic_next_at = now + TOPFLOW_AGGREGATION_TRAFFIC_POLL_SEC;
    if (device_ubus_call_timeout("zwrt_icg_mdc.manager", "get_residual_flow", NULL,
                                 next, sizeof next,
                                 TOPFLOW_AGGREGATION_FLOW_TIMEOUT_MS) == 0 &&
        json_get_int(next, "error_code", 0) == 0) {
        char value[128];
        if (json_get(next, "residual_flow", value, sizeof value) ||
            json_get(next, "count_flow_today", value, sizeof value))
            copy_text(g_topflow_aggregation_flow, sizeof g_topflow_aggregation_flow, next);
    }
    (void)load_topflow_aggregation_traffic_uci();
}

static int mwan3_config_value(const char *section, const char *option,
                              char *out, size_t outlen)
{
    char key[256];
    const char *line = g_topflow_mwan3_config;
    size_t key_len;
    if (!section || !*section || !out || outlen < 2) return 0;
    if (option && *option)
        snprintf(key, sizeof key, "mwan3.%s.%s=", section, option);
    else
        snprintf(key, sizeof key, "mwan3.%s=", section);
    key_len = strlen(key);
    while (line && *line) {
        const char *end = strchr(line, '\n');
        size_t line_len = end ? (size_t)(end - line) : strlen(line);
        if (line_len >= key_len && !strncmp(line, key, key_len)) {
            const char *value = line + key_len;
            size_t value_len = line_len - key_len;
            if (value_len >= 2 && value[0] == '\'' && value[value_len - 1] == '\'') {
                value++;
                value_len -= 2;
            }
            if (value_len >= outlen) value_len = outlen - 1;
            memcpy(out, value, value_len);
            out[value_len] = 0;
            return 1;
        }
        line = end ? end + 1 : NULL;
    }
    out[0] = 0;
    return 0;
}

static void emit_mwan3_string_option(struct buf *b, const char *section,
                                     const char *option, int *emitted)
{
    char value[2048];
    if (!mwan3_config_value(section, option, value, sizeof value)) return;
    bappend(b, "%s\"%s\":\"", (*emitted)++ ? "," : "", option);
    bappend_json_esc(b, value);
    bappend(b, "\"");
}

static void emit_mwan3_list_option(struct buf *b, const char *section,
                                   const char *option, int *emitted)
{
    char value[4096], token[512];
    const char *p;
    int item = 0;
    if (!mwan3_config_value(section, option, value, sizeof value)) return;
    bappend(b, "%s\"%s\":[", (*emitted)++ ? "," : "", option);
    p = value;
    while (*p) {
        size_t n = 0;
        while (*p && (isspace((unsigned char)*p) || *p == '\'')) p++;
        while (*p && *p != '\'' && !isspace((unsigned char)*p)) {
            if (n + 1 < sizeof token) token[n++] = *p;
            p++;
        }
        token[n] = 0;
        while (*p == '\'') p++;
        if (!n) continue;
        if (item++) bappend(b, ",");
        bappend(b, "\"");
        bappend_json_esc(b, token);
        bappend(b, "\"");
    }
    bappend(b, "]");
}

static void emit_mwan3_section(struct buf *b, const char *section, const char *type,
                               int *emitted)
{
    static const char *interface_options[] = {
        "enabled", "family", "track_method", "reliability", "count", "size",
        "max_ttl", "check_quality", "timeout", "interval", "failure_interval",
        "recovery_interval", "down", "up"
    };
    static const char *member_options[] = {"interface", "metric", "weight"};
    static const char *rule_options[] = {
        "family", "proto", "src_ip", "dest_ip", "src_port", "dest_port",
        "sticky", "logging", "use_policy"
    };
    int field = 0;
    if ((*emitted)++) bappend(b, ",");
    bappend(b, "{\"id\":\"");
    bappend_json_esc(b, section);
    bappend(b, "\",\"type\":\"");
    bappend_json_esc(b, type);
    bappend(b, "\"");
    field = 2;
    if (!strcmp(type, "interface")) {
        for (size_t i = 0; i < sizeof interface_options / sizeof interface_options[0]; i++)
            emit_mwan3_string_option(b, section, interface_options[i], &field);
        emit_mwan3_list_option(b, section, "track_ip", &field);
    } else if (!strcmp(type, "member")) {
        for (size_t i = 0; i < sizeof member_options / sizeof member_options[0]; i++)
            emit_mwan3_string_option(b, section, member_options[i], &field);
    } else if (!strcmp(type, "policy")) {
        emit_mwan3_string_option(b, section, "last_resort", &field);
        emit_mwan3_list_option(b, section, "use_member", &field);
    } else if (!strcmp(type, "rule")) {
        for (size_t i = 0; i < sizeof rule_options / sizeof rule_options[0]; i++)
            emit_mwan3_string_option(b, section, rule_options[i], &field);
    } else if (!strcmp(type, "globals")) {
        emit_mwan3_string_option(b, section, "mmx_mask", &field);
    }
    bappend(b, "}");
}

static void emit_topflow_multiwan(struct buf *b, const char *mode)
{
    const char *line = g_topflow_mwan3_config;
    int emitted = 0;
    bappend(b, "\"multiwan\":{\"mode\":\"");
    bappend_json_esc(b, mode ? mode : "");
    bappend(b, "\",\"active\":%s,\"service_running\":%s,\"sections\":[",
            mode && !strcmp(mode, "MULTIWAN") ? "true" : "false",
            g_topflow_mwan3_running ? "true" : "false");
    while (line && *line) {
        const char *end = strchr(line, '\n');
        size_t line_len = end ? (size_t)(end - line) : strlen(line);
        if (line_len > 6 && !strncmp(line, "mwan3.", 6)) {
            const char *eq = memchr(line, '=', line_len);
            const char *dot = memchr(line + 6, '.', line_len - 6);
            if (eq && (!dot || dot > eq)) {
                char section[128], type[64];
                size_t sn = (size_t)(eq - (line + 6));
                size_t tn = line_len - (size_t)(eq - line) - 1;
                const char *tv = eq + 1;
                if (tn >= 2 && tv[0] == '\'' && tv[tn - 1] == '\'') { tv++; tn -= 2; }
                if (sn < sizeof section && tn < sizeof type) {
                    memcpy(section, line + 6, sn); section[sn] = 0;
                    memcpy(type, tv, tn); type[tn] = 0;
                    if (!strcmp(type, "interface") || !strcmp(type, "member") ||
                        !strcmp(type, "policy") || !strcmp(type, "rule") ||
                        !strcmp(type, "globals"))
                        emit_mwan3_section(b, section, type, &emitted);
                }
            }
        }
        line = end ? end + 1 : NULL;
    }
    bappend(b, "]},");
}

static void emit_topflow_aggregation_paths(struct buf *b, int *path_count,
                                           int *online_path_count)
{
    static const struct {
        const char *id;
        const char *label;
        const char *interface_name;
    } paths[TOPFLOW_AGGREGATION_MAX_PATHS] = {
        {"x75", "X75", "zte_mwan2"},
        {"v3e1", "V3E1", "zte_mwan3"},
        {"v3e2", "V3E2", "zte_mwan4"},
        {"ethernet", "Ethernet", "waneth"}
    };
    char interfaces[RAW_MAX];
    int emitted = 0;

    *path_count = 0;
    *online_path_count = 0;
    bappend(b, "\"paths\":[");
    if (!json_get(g_topflow_mwan3_status, "interfaces", interfaces, sizeof interfaces)) {
        bappend(b, "]");
        return;
    }
    for (size_t i = 0; i < sizeof paths / sizeof paths[0]; i++) {
        char path[8192], status[64] = "unknown", tracking[64] = "down";
        char targets[4096], target[1024], first_target[1024] = "";
        const char *cursor;
        int enabled, running, up, mwan_up, online, interface_up = 0;
        int interface_available = 0, interface_pending = 0;
        long uptime;
        double latency = -1.0, packet_loss = -1.0;

        if (!json_get(interfaces, paths[i].interface_name, path, sizeof path)) continue;
        enabled = json_bool_or(path, "enabled", 0);
        running = json_bool_or(path, "running", 0);
        mwan_up = json_bool_or(path, "up", 0);
        up = mwan_up;
        if (i < TOPFLOW_MODEM_COUNT) {
            interface_up = json_bool_or(g_topflow_wan4[i], "up", 0);
            interface_available = json_bool_or(g_topflow_wan4[i], "available", 0);
            interface_pending = json_bool_or(g_topflow_wan4[i], "pending", 0);
            if (interface_up) up = 1;
        }
        (void)json_get(path, "status", status, sizeof status);
        (void)json_get(path, "tracking", tracking, sizeof tracking);
        if (!enabled && !running && !up) continue;
        online = !strcasecmp(status, "online") || (running && mwan_up) ||
            (!g_topflow_mwan3_running && interface_up);
        uptime = json_get_int(path, "uptime", 0);
        if (json_get(path, "track_ip", targets, sizeof targets)) {
            double fallback_latency = -1.0, fallback_loss = -1.0;
            cursor = targets;
            while ((cursor = copy_next_json_object(cursor, first_target,
                                                    sizeof first_target)) != NULL) {
                char target_status[64] = "unknown";
                double target_latency = json_double_or(first_target, "latency", -1.0);
                double target_loss = json_double_or(first_target, "packetloss", -1.0);
                (void)json_get(first_target, "status", target_status,
                               sizeof target_status);
                if (strcasecmp(target_status, "skipped") && fallback_latency < 0.0) {
                    fallback_latency = target_latency;
                    fallback_loss = target_loss;
                }
                if (!strcasecmp(target_status, "online") ||
                    !strcasecmp(target_status, "up")) {
                    latency = target_latency;
                    packet_loss = target_loss;
                    break;
                }
            }
            if (latency < 0.0) {
                latency = fallback_latency;
                packet_loss = fallback_loss;
            }
        } else {
            targets[0] = 0;
        }

        if (emitted++) bappend(b, ",");
        bappend(b, "{\"id\":\"");
        bappend_json_esc(b, paths[i].id);
        bappend(b, "\",\"label\":\"");
        bappend_json_esc(b, paths[i].label);
        bappend(b, "\",\"interface\":\"");
        bappend_json_esc(b, paths[i].interface_name);
        bappend(b, "\",\"enabled\":%s,\"running\":%s,\"up\":%s,\"online\":%s,",
                enabled ? "true" : "false", running ? "true" : "false",
                up ? "true" : "false", online ? "true" : "false");
        bappend(b, "\"interface_up\":%s,\"interface_available\":%s,"
                "\"interface_pending\":%s,",
                interface_up ? "true" : "false",
                interface_available ? "true" : "false",
                interface_pending ? "true" : "false");
        bappend(b, "\"status\":\"");
        bappend_json_esc(b, status);
        bappend(b, "\",\"tracking\":\"");
        bappend_json_esc(b, tracking);
        bappend(b, "\",\"uptime_seconds\":%ld", uptime);
        if (latency >= 0) bappend(b, ",\"latency_ms\":%.2f", latency);
        if (packet_loss >= 0) bappend(b, ",\"packet_loss_percent\":%.2f", packet_loss);
        bappend(b, ",\"targets\":[");
        cursor = targets;
        int target_emitted = 0;
        while ((cursor = copy_next_json_object(cursor, target, sizeof target)) != NULL) {
            char ip[128] = "", target_status[64] = "unknown";
            double target_latency = json_double_or(target, "latency", -1.0);
            double target_loss = json_double_or(target, "packetloss", -1.0);
            (void)json_get(target, "ip", ip, sizeof ip);
            (void)json_get(target, "status", target_status, sizeof target_status);
            if (target_emitted++) bappend(b, ",");
            bappend(b, "{\"ip\":\"");
            bappend_json_esc(b, ip);
            bappend(b, "\",\"status\":\"");
            bappend_json_esc(b, target_status);
            bappend(b, "\",\"online\":%s",
                    (!strcasecmp(target_status, "online") ||
                     !strcasecmp(target_status, "up")) ? "true" : "false");
            if (target_latency >= 0) bappend(b, ",\"latency_ms\":%.2f", target_latency);
            if (target_loss >= 0) bappend(b, ",\"packet_loss_percent\":%.2f", target_loss);
            bappend(b, "}");
        }
        bappend(b, "]}");
        (*path_count)++;
        if (online) (*online_path_count)++;
    }
    bappend(b, "]");
}

static void emit_topflow_aggregation(struct buf *b, int enabled,
                                     const char *aggregation_mode)
{
    char remaining[128] = "", today_used[128] = "";
    int path_count, online_path_count;
    const char *state = !enabled ? "disabled" :
        (!g_topflow_icg_provisioned ? "unprovisioned" :
         (g_topflow_icg_tcp_tunnel_count > 0 ? "online" : "waiting"));

    bappend(b, "\"aggregation\":{\"enabled\":%s,\"mode\":\"",
            enabled ? "true" : "false");
    bappend_json_esc(b, aggregation_mode);
    bappend(b, "\",\"state\":\"%s\",\"provisioned\":%s,\"online\":%s,",
            state, g_topflow_icg_provisioned ? "true" : "false",
            g_topflow_icg_tcp_tunnel_count > 0 ? "true" : "false");
    bappend(b, "\"controller\":{\"icg_process_running\":%s,"
            "\"mwan3_running\":%s},",
            g_topflow_icg_process_running ? "true" : "false",
            g_topflow_mwan3_running ? "true" : "false");
    bappend(b, "\"tcp_tunnel_count\":%d,\"server\":{",
            g_topflow_icg_tcp_tunnel_count);
    if (g_topflow_icg_server_ip[0]) {
        bappend(b, "\"ip\":\"");
        bappend_json_esc(b, g_topflow_icg_server_ip);
        bappend(b, "\"");
    }
    if (g_topflow_icg_tcp_port > 0)
        bappend(b, "%s\"tcp_port\":%d", g_topflow_icg_server_ip[0] ? "," : "",
                g_topflow_icg_tcp_port);
    if (g_topflow_icg_udp_start_port > 0)
        bappend(b, "%s\"udp_start_port\":%d",
                (g_topflow_icg_server_ip[0] || g_topflow_icg_tcp_port > 0) ? "," : "",
                g_topflow_icg_udp_start_port);
    bappend(b, "%s\"source\":\"%s\"",
            (g_topflow_icg_server_ip[0] || g_topflow_icg_tcp_port > 0 ||
             g_topflow_icg_udp_start_port > 0) ? "," : "",
            g_topflow_icg_server_runtime ? "runtime" : "config");
    bappend(b, "},");
    emit_topflow_aggregation_paths(b, &path_count, &online_path_count);
    bappend(b, ",\"path_count\":%d,\"online_path_count\":%d",
            path_count, online_path_count);
    (void)json_get(g_topflow_aggregation_flow, "residual_flow", remaining, sizeof remaining);
    (void)json_get(g_topflow_aggregation_flow, "count_flow_today", today_used, sizeof today_used);
    if (remaining[0] || today_used[0]) {
        int has_field = 0;
        bappend(b, ",\"traffic\":{");
        if (remaining[0]) {
            if (topflow_decimal_bytes(remaining)) {
                bappend(b, "\"remaining_bytes\":%s", remaining);
                has_field = 1;
            }
            bappend(b, "%s\"remaining_raw\":\"", has_field ? "," : "");
            bappend_json_esc(b, remaining);
            bappend(b, "\"");
            has_field = 1;
        }
        if (today_used[0]) {
            if (topflow_decimal_bytes(today_used)) {
                bappend(b, "%s\"today_used_bytes\":%s",
                        has_field ? "," : "", today_used);
                has_field = 1;
            }
            bappend(b, "%s\"today_used_raw\":\"", has_field ? "," : "");
            bappend_json_esc(b, today_used);
            bappend(b, "\"");
        }
        bappend(b, "}");
    }
    bappend(b, "},");
}

static void emit_topflow_hardware_controls(struct buf *b)
{
    char zone[PATH_MAX], path[PATH_MAX], mode[32] = "disabled";
    char aggregation_mode[32] = "";
    long pwm, rpm, fan_thermal, liquid_thermal, cooling_temp = -1;
    int fan_enabled, liquid_enabled, aggregation_enabled, automatic = 0;
    int configured_fan_always_on = -1, configured_liquid_always_on = -1;
    int configured_liquid_level = 1;
    int configured_fan_mode = 0, configured_manual_speed = 0, custom_curve_count = 0;
    struct topflow_curve_point custom_curve[TOPFLOW_CUSTOM_CURVE_MAX] = {{0, 0}};
    long temperatures[3] = {44000, 48000, 53000};
    long hysteresis[3] = {4000, 4000, 4000};
    if (device_uci_get("zwrt_router.network.opms_wan_mode",
                       aggregation_mode, sizeof aggregation_mode) != 0)
        aggregation_mode[0] = 0;
    aggregation_enabled = !strcmp(aggregation_mode, "SMULTIWAN");

    if (find_topflow_cooling_zone(zone, sizeof zone)) {
        if (snprintf(path, sizeof path, "%s/mode", zone) < (int)sizeof path &&
            read_line_file(path, mode, sizeof mode))
            automatic = !strcmp(mode, "enabled");
        if (snprintf(path, sizeof path, "%s/temp", zone) < (int)sizeof path)
            cooling_temp = normalize_temp_reading(read_long_file(path, -1));
        for (int i = 0; i < 3; i++) {
            if (snprintf(path, sizeof path, "%s/trip_point_%d_temp", zone, i) < (int)sizeof path)
                temperatures[i] = read_long_file(path, temperatures[i]);
            if (snprintf(path, sizeof path, "%s/trip_point_%d_hyst", zone, i) < (int)sizeof path)
                hysteresis[i] = read_long_file(path, hysteresis[i]);
        }
    }
    if (!load_topflow_custom_curve(&configured_fan_always_on,
                                   &configured_liquid_always_on,
                                   &configured_liquid_level,
                                   &configured_fan_mode, &configured_manual_speed,
                                   custom_curve, &custom_curve_count))
        configured_fan_mode = 0;
    control_cooling_tick(cooling_temp);
    /* The tick may enter or leave the 80 C userspace override. Report the
     * post-transition zone state, not the value sampled before the hardware
     * change. */
    if (find_topflow_cooling_zone(zone, sizeof zone) &&
        snprintf(path, sizeof path, "%s/mode", zone) < (int)sizeof path &&
        read_line_file(path, mode, sizeof mode))
        automatic = !strcmp(mode, "enabled");
    pwm = read_long_file(runtime_path("ZWRT_DATAD_FAN_PWM_PATH", TOPFLOW_FAN_PWM_PATH), -1);
    rpm = read_long_file(runtime_path("ZWRT_DATAD_FAN_RPM_PATH", TOPFLOW_FAN_RPM_PATH), -1);
    fan_thermal = read_labeled_long_file(
        runtime_path("ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH", TOPFLOW_FAN_THERMAL_ENABLE_PATH),
        "thermal_enable", -1);
    liquid_thermal = read_labeled_long_file(
        runtime_path("ZWRT_DATAD_LIQUID_THERMAL_ENABLE_PATH", TOPFLOW_LIQUID_THERMAL_ENABLE_PATH),
        "thermal_enable", -1);
    fan_enabled = configured_fan_always_on >= 0
        ? configured_fan_always_on
        : read_uci_flag("zwrt_deviceui.Device.fan_switch_status", 0);
    liquid_enabled = configured_liquid_always_on >= 0
        ? configured_liquid_always_on
        : read_uci_flag("zwrt_deviceui.Device.liquid_cooling_switch_status", 0);

    emit_topflow_aggregation(b, aggregation_enabled, aggregation_mode);
    emit_topflow_multiwan(b, aggregation_mode);
    bappend(b, "\"cooling\":{\"fan\":{");
    bappend(b, "\"enabled\":%s,\"always_on\":%s,\"mode\":\"%s\",",
            fan_enabled ? "true" : "false",
            fan_enabled ? "true" : "false",
            fan_enabled ? "always_on" :
            (configured_fan_mode == 2 ? "custom" :
             (configured_fan_mode == 1 || automatic ? "automatic" : "manual")));
    bappend(b, "\"pwm\":%ld,\"max_pwm\":255,\"speed_percent\":%ld,",
            pwm, pwm >= 0 ? (pwm * 100 + 127) / 255 : -1);
    bappend(b, "\"manual_speed_percent\":%d,", configured_manual_speed);
    if (cooling_temp > 0) bappend(b, "\"temperature_celsius\":%ld,", cooling_temp);
    bappend(b, "\"hard_full_speed_celsius\":80,");
    if (rpm >= 0) bappend(b, "\"rpm\":%ld,", rpm);
    bappend(b, "\"thermal_enabled\":%s,\"kernel_zone_enabled\":%s,"
            "\"levels_percent\":[0,30,50,70]},",
            fan_thermal > 0 ? "true" : "false", automatic ? "true" : "false");
    bappend(b, "\"liquid\":{\"enabled\":%s,\"always_on\":%s,\"thermal_enabled\":%s,",
            liquid_enabled ? "true" : "false",
            liquid_enabled ? "true" : "false", liquid_thermal > 0 ? "true" : "false");
    bappend(b, "\"mode\":\"%s\",\"level\":%d,\"speed_percent\":%d,\"amplitude\":%d,\"levels_percent\":[30,100]},",
            liquid_enabled ? (configured_liquid_level >= 2 ? "high" : "low") : "automatic",
            liquid_enabled ? configured_liquid_level : 0,
            liquid_enabled ? (configured_liquid_level >= 2 ? 100 : 30) : 0,
            liquid_enabled ? (configured_liquid_level >= 2 ? 200 : 60) : 0);
    bappend(b, "\"factory_curve\":[");
    {
        static const int kernel_pwm[3] = {76, 128, 179};
        for (int i = 0; i < 3; i++) {
            if (i) bappend(b, ",");
            bappend(b, "{\"level\":%d,\"temperature_celsius\":%ld,"
                    "\"hysteresis_celsius\":%ld,\"pwm\":%d,\"speed_percent\":%d}",
                    i + 1, temperatures[i] / 1000, hysteresis[i] / 1000,
                    kernel_pwm[i], (kernel_pwm[i] * 100 + 127) / 255);
        }
    }
    bappend(b, "],\"custom_curve\":[");
    for (int i = 0; i < custom_curve_count; i++) {
        if (i) bappend(b, ",");
        bappend(b, "{\"temperature_celsius\":%d,\"pwm\":%d,\"speed_percent\":%d}",
                custom_curve[i].temperature, custom_curve[i].pwm,
                (custom_curve[i].pwm * 100 + 127) / 255);
    }
    bappend(b, "],\"curve\":[");
    if (custom_curve_count >= 2) {
        for (int i = 0; i < custom_curve_count; i++) {
            if (i) bappend(b, ",");
            bappend(b, "{\"temperature_celsius\":%d,\"pwm\":%d,\"speed_percent\":%d}",
                    custom_curve[i].temperature, custom_curve[i].pwm,
                    (custom_curve[i].pwm * 100 + 127) / 255);
        }
    } else {
        static const int kernel_pwm[3] = {76, 128, 179};
        for (int i = 0; i < 3; i++) {
            if (i) bappend(b, ",");
            bappend(b, "{\"level\":%d,\"temperature_celsius\":%ld,"
                    "\"hysteresis_celsius\":%ld,\"pwm\":%d,\"speed_percent\":%d}",
                    i + 1, temperatures[i] / 1000, hysteresis[i] / 1000,
                    kernel_pwm[i], (kernel_pwm[i] * 100 + 127) / 255);
        }
    }
    bappend(b, "]},");
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

struct uci_device_field {
    const char *key;
    const char *path;
};

/*
 * UCI is the firmware's persistent cache/configuration plane. Keep it in the
 * state snapshot so consumers retain useful fields if a vendor ubus getter is
 * missing, stale, or masks privacy-sensitive values (as MU5252 does for SIM
 * identities). Runtime counters and radio measurements still come from ubus.
 */
static void refresh_uci_device_info(void)
{
    static const struct uci_device_field fields[] = {
        {"iccid", "zwrt_zte_mdm.sim_info.sim_iccid"},
        {"imsi", "zwrt_zte_mdm.sim_info.sim_imsi"},
        {"msisdn", "zwrt_zte_mdm.sim_info.msisdn"},
        {"sim_slot", "zwrt_zte_mdm.sim_info.current_sim_slot"},
        {"operator", "zwrt_zte_mdm.sim_info.Operator"},
        {"sim_states", "zwrt_zte_mdm.sim_info.sim_states"},
        {"modem_main_state", "zwrt_zte_mdm.sim_info.modem_main_state"},
        {"pin_status", "zwrt_zte_mdm.sim_info.pin_status"},
        {"mcc", "zwrt_zte_mdm.sim_info.mdm_mcc"},
        {"mnc", "zwrt_zte_mdm.sim_info.mdm_mnc"},
        {"imei", "zwrt_zte_mdm.device_info.imei"},
        {"mac_address", "zwrt_zte_mdm.device_info.wlan_mac_address"},
        {"modem_msn", "zwrt_zte_mdm.device_info.modem_msn"},
        {"wa_inner_version", "zwrt_common_info.common_config.wa_inner_version"},
        {"hardware_version_ci", "zwrt_common_info.common_config.hardware_version"},
        {"integrate_version", "zwrt_common_info.common_config.integrate_version"},
        {"common_model_name", "zwrt_common_info.common_config.model_name"},
        {"device_alias_name", "zwrt_common_info.common_config.device_alias_name"},
        {"device_market_name", "zwrt_common_info.common_config.device_market_name"},
        {"lan_ipaddr", "network.lan.ipaddr"},
        {"lan_netmask", "network.lan.netmask"},
        {"wan_ipaddr", "network.zte_wan.ipaddr"},
        {"wan_netmask", "network.zte_wan.netmask"},
        {"wan_gateway", "network.zte_wan.gateway"},
        {"wan_dns", "network.zte_wan.dns"},
        {"wan6_addr", "network.zte_wan6.ip6addr"},
        {"wan6_gateway", "network.zte_wan6.ip6gw"},
        {"wan6_dns", "network.zte_wan6.dns"},
        {"dhcpEnabled", "dhcp.lan.ignore"},
        {"dhcpStart", "dhcp.lan.zte_start"},
        {"dhcpEnd", "dhcp.lan.zte_end"},
        {"dhcpLease_hour", "dhcp.lan.leasetime"},
        {"day_tx_bytes", "zwrt_data_commit.wwancid1dst.day_tx_bytes"},
        {"day_rx_bytes", "zwrt_data_commit.wwancid1dst.day_rx_bytes"},
        {"day_time", "zwrt_data_commit.wwancid1dst.day_time"},
        {"month_tx_bytes", "zwrt_data_commit.wwancid1dst.month_tx_bytes"},
        {"month_rx_bytes", "zwrt_data_commit.wwancid1dst.month_rx_bytes"},
        {"month_time", "zwrt_data_commit.wwancid1dst.month_time"},
        {"total_tx_bytes", "zwrt_data_commit.wwancid1dst.total_tx_bytes"},
        {"total_rx_bytes", "zwrt_data_commit.wwancid1dst.total_rx_bytes"},
        {"total_time", "zwrt_data_commit.wwancid1dst.total_time"},
        {"hostname", "system.@system[0].hostname"},
        {"timezone", "system.@system[0].timezone"},
        {"web_language", "zwrt_web.setting.web_language"},
        {"login_timeout", "zwrt_web.config.login_timeout"},
        {"login_fail_num", "zwrt_web.config.login_fail_num"},
        {"login_fail_lock_timeout", "zwrt_web.config.login_fail_lock_timeout"},
        {"device_model", "zwrt_tr069.DeviceInfo.ModelName"},
        {"device_manufacturer", "zwrt_tr069.DeviceInfo.Manufacturer"},
        {"hardware_version", "zwrt_tr069.DeviceInfo.HardwareVersion"},
        {"software_version", "zwrt_tr069.DeviceInfo.SoftwareVersion"},
        {"serial_number", "zwrt_tr069.DeviceInfo.SerialNumber"},
        {"battery_percent", "zwrt_zte_mc_tmp.battery.bat_percent"},
        {"battery_level", "zwrt_zte_mc_tmp.battery.bat_level"},
        {"battery_voltage", "zwrt_zte_mc_tmp.battery.bat_voltage"},
        {"battery_temperature", "zwrt_zte_mc_tmp.battery.bat_temperature"},
        {"battery_charging", "zwrt_zte_mc_tmp.battery.bat_charging"},
        {"battery_enable", "zwrt_zte_mc_tmp.battery.bat_enable"},
        {"power_adapter", "zwrt_zte_mc_tmp.battery.power_adapter"},
        {"mtu", "zwrt_router.network.mtu"},
        {"mss", "zwrt_router.network.mss"}
    };
    static const char *packages[] = {
        "zwrt_zte_mdm", "zwrt_common_info", "network", "dhcp",
        "zwrt_data_commit", "system", "zwrt_web", "zwrt_tr069",
        "zwrt_zte_mc_tmp", "zwrt_router"
    };
    struct buf b = {g_uci_device_info, sizeof g_uci_device_info, 0};
    struct buf without_battery = {
        g_uci_device_info_no_battery, sizeof g_uci_device_info_no_battery, 0
    };
    int emitted = 0;
    int emitted_without_battery = 0;

    bappend(&b, "{");
    bappend(&without_battery, "{");
    for (size_t p = 0; p < sizeof packages / sizeof packages[0]; p++) {
        char show[RAW_MAX];
        size_t package_len = strlen(packages[p]);
        if (device_uci_show(packages[p], show, sizeof show) != 0) continue;
        for (size_t i = 0; i < sizeof fields / sizeof fields[0]; i++) {
            char value[512];
            if (strncmp(fields[i].path, packages[p], package_len) ||
                fields[i].path[package_len] != '.') continue;
            if (uci_show_value(show, fields[i].path, value, sizeof value) != 0 || !value[0])
                continue;
            if (emitted++) bappend(&b, ",");
            bappend(&b, "\"%s\":\"", fields[i].key);
            bappend_json_esc(&b, value);
            bappend(&b, "\"");
            if (strncmp(fields[i].key, "battery_", 8) &&
                strcmp(fields[i].key, "power_adapter")) {
                if (emitted_without_battery++) bappend(&without_battery, ",");
                bappend(&without_battery, "\"%s\":\"", fields[i].key);
                bappend_json_esc(&without_battery, value);
                bappend(&without_battery, "\"");
            }
        }
    }
    bappend(&b, "}");
    bappend(&without_battery, "}");
    if (b.len >= b.cap) copy_text(g_uci_device_info, sizeof g_uci_device_info, "{}");
    if (without_battery.len >= without_battery.cap)
        copy_text(g_uci_device_info_no_battery,
                  sizeof g_uci_device_info_no_battery, "{}");
}

static int is_decimal_identity(const char *value)
{
    if (!value || !*value) return 0;
    for (const unsigned char *p = (const unsigned char *)value; *p; p++) {
        if (!isdigit(*p)) return 0;
    }
    return 1;
}

static int is_msisdn_identity(const char *value)
{
    const unsigned char *p = (const unsigned char *)value;
    if (!p || !*p) return 0;
    if (*p == '+') p++;
    if (!*p) return 0;
    for (; *p; p++) {
        if (!isdigit(*p)) return 0;
    }
    return 1;
}

static int is_valid_sim_identity(const char *key, const char *value)
{
    if (!strcmp(key, "msisdn")) return is_msisdn_identity(value);
    return is_decimal_identity(value);
}

static void emit_sim_identity(struct buf *b, const char *key,
                              const char *src, const char *src_key)
{
    char value[256], uci_value[256];
    if (!json_get(src, src_key, value, sizeof value)) value[0] = 0;
    if (!is_valid_sim_identity(key, value)) {
        value[0] = 0;
        if (json_get(g_uci_device_info, key, uci_value, sizeof uci_value) &&
            is_valid_sim_identity(key, uci_value))
            copy_text(value, sizeof value, uci_value);
    }
    bappend(b, "\"%s\":\"", key);
    bappend_json_esc(b, value);
    bappend(b, "\"");
}

static int load_network_snapshot_mu5252_uci(char *out, size_t outlen)
{
    static const struct uci_net_field fields[] = {
        {"network_type", "zte_nwinfo.sys_info.network_type", NULL},
        {"signalbar", "zte_nwinfo.signal_strength.signalbar", NULL},
        {"simcard_roam", "zte_nwinfo.sys_info.simcard_roam", NULL},
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
        {"lte_bandwidth", "zte_nwinfo.cell_info.lte_bandwidth", NULL},
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

/* Active local SIM slot (0 or 1) of external modem `index`, taken from
 * get_v3t_sim_info's v3t_<n>_st_slot. B20 keys its multi-SIM network fields by
 * this slot (msim_<modem>_<slot>_*), so reading a fixed slot 0 leaves the net
 * object empty whenever the modem is camped on slot 1 (issue #23). */
static int topflow_external_active_slot(int index)
{
    char key[64];
    long slot;

    if (index < 0 || index >= TOPFLOW_EXTERNAL_MODEM_COUNT) return 0;
    snprintf(key, sizeof key, "v3t_%d_st_slot", index + 1);
    slot = json_get_int(g_topflow_v3t_sim, key, 0);
    return (slot == 1) ? 1 : 0;
}

static int load_topflow_msim_netinfo_uci(char *out, size_t outlen)
{
    static const char *suffixes[] = {
        "net_select", "network_type", "rplmn_num", "network_provider",
        "domain_stat", "simcard_roam", "wan_active_band", "signalbar",
        "cell_id", "wan_active_channel", "lte_pci", "lte_tac", "lac_code",
        "lte_rsrp", "lte_rsrq", "lte_rssi", "lte_snr", "lte_band_lock",
        "operate_mode", "lte_bandwidth"
    };
    char show[RAW_MAX];
    struct buf b = {out, outlen, 0};
    int fields = 0;

    if (!out || outlen < 3) return -1;
    out[0] = 0;
    if (device_uci_show("zte_nwinfo", show, sizeof show) != 0) {
        memcpy(out, "{}", 3);
        return -1;
    }
    bappend(&b, "{");
    for (int modem = 1; modem <= TOPFLOW_EXTERNAL_MODEM_COUNT; modem++) {
        int slot = topflow_external_active_slot(modem - 1);
        for (size_t i = 0; i < sizeof suffixes / sizeof suffixes[0]; i++) {
            char path[160], key[128], value[256] = "";
            snprintf(path, sizeof path, "zte_nwinfo.sys_info.msim_%d_%d_%s",
                     modem, slot, suffixes[i]);
            if (uci_show_value(show, path, value, sizeof value) != 0 || !value[0])
                continue;
            snprintf(key, sizeof key, "msim_%d_%d_%s", modem, slot, suffixes[i]);
            if (fields++) bappend(&b, ",");
            bappend(&b, "\"%s\":\"", key);
            bappend_json_esc(&b, value);
            bappend(&b, "\"");
        }
    }
    bappend(&b, "}");
    if (b.len >= b.cap || fields == 0) {
        memcpy(out, "{}", 3);
        return -1;
    }
    return 0;
}

static int topflow_external_subid(int index)
{
    if (index < 0 || index >= TOPFLOW_EXTERNAL_MODEM_COUNT) return 0;
    return (index == 0 ? 3 : 5) + topflow_external_active_slot(index);
}

static void update_device_template_features(const char *common)
{
    char model[128] = "", hardware[128] = "";
    const struct device_template_spec *tpl;
    if (common && *common) {
        (void)json_get(common, "model_name", model, sizeof model);
        (void)json_get(common, "hardware_version", hardware, sizeof hardware);
    }
    g_topflow_multimodem_enabled = !strcmp(model, "MU5252") || has_prefix(hardware, "MU5252_");
    tpl = select_device_template(model, hardware);
    g_full_ubus_enabled = tpl->full_ubus;
}

static void refresh_topflow_multimodem_cache(void)
{
    static const char *wan4_services[TOPFLOW_MODEM_COUNT] = {
        "network.interface.zte_mwan2",
        "network.interface.zte_mwan3",
        "network.interface.zte_mwan4"
    };
    static const char *wan6_services[TOPFLOW_MODEM_COUNT] = {
        "network.interface.zte_mwan2_6",
        "network.interface.zte_mwan3_6",
        "network.interface.zte_mwan4_6"
    };
    char next[RAW_MAX];
    time_t now;

    if (!g_topflow_multimodem_enabled) return;

    /* Fetch the per-modem active SIM slot first: load_topflow_msim_netinfo_uci()
     * keys its lookups by v3t_<n>_st_slot, so g_topflow_v3t_sim must be current
     * before the UCI fallback runs (issue #23). */
    if (run_ubus("zwrt_zte_mdm.api", "get_v3t_sim_info", NULL,
                 next, sizeof next) == 0)
        copy_text(g_topflow_v3t_sim, sizeof g_topflow_v3t_sim, next);

    if (run_ubus("zte_nwinfo_api", "nwinfo_get_msim_netinfo", NULL,
                 next, sizeof next) == 0) {
        copy_text(g_topflow_msim_netinfo, sizeof g_topflow_msim_netinfo, next);
    } else if (load_topflow_msim_netinfo_uci(next, sizeof next) == 0) {
        copy_text(g_topflow_msim_netinfo, sizeof g_topflow_msim_netinfo, next);
    }

    for (int i = 1; i < TOPFLOW_MODEM_COUNT; i++) {
        if (run_ubus(wan4_services[i], "status", NULL, next, sizeof next) == 0)
            copy_text(g_topflow_wan4[i], sizeof g_topflow_wan4[i], next);
        if (run_ubus(wan6_services[i], "status", NULL, next, sizeof next) == 0)
            copy_text(g_topflow_wan6[i], sizeof g_topflow_wan6[i], next);
    }

    for (int i = 0; i < TOPFLOW_EXTERNAL_MODEM_COUNT; i++) {
        char args[192];
        int subid = topflow_external_subid(i);
        snprintf(args, sizeof args,
                 "{\"source_module\":\"deviceui\",\"cid\":1,\"subid\":%d}", subid);
        if (run_ubus("zwrt_data", "get_wwaniface", args, next, sizeof next) == 0)
            copy_text(g_topflow_wwaniface[i], sizeof g_topflow_wwaniface[i], next);
        snprintf(args, sizeof args,
                 "{\"source_module\":\"deviceui\",\"cid\":1,\"type\":1,\"subid\":%d}",
                 subid);
        if (run_ubus("zwrt_data", "get_wwandst", args, next, sizeof next) == 0)
            copy_text(g_topflow_traffic[i], sizeof g_topflow_traffic[i], next);
    }

    now = time(NULL);
    refresh_topflow_external_qos_cache(now);
    if (g_topflow_thermal_next_at == 0 || now >= g_topflow_thermal_next_at) {
        static const char *serials[TOPFLOW_EXTERNAL_MODEM_COUNT] = {
            "V3E1T12345", "V3E2T12345"
        };
        for (int i = 0; i < TOPFLOW_EXTERNAL_MODEM_COUNT; i++) {
            char raw[64], *end = NULL;
            long value;
            if (!topflow_external_adb_available(i)) {
                g_topflow_external_temp_valid[i] = 0;
                g_topflow_external_temp_sampled_at[i] = 0;
                continue;
            }
            if (device_adb_read_file(serials[i], TOPFLOW_V3E_TEMP_PATH,
                                     raw, sizeof raw) != 0) {
                g_topflow_external_temp_valid[i] = 0;
                g_topflow_external_temp_sampled_at[i] = 0;
                continue;
            }
            errno = 0;
            value = strtol(raw, &end, 10);
            while (end && *end && isspace((unsigned char)*end)) end++;
            if (errno || !end || end == raw || *end || value < -40 || value > 125) {
                g_topflow_external_temp_valid[i] = 0;
                g_topflow_external_temp_sampled_at[i] = 0;
                continue;
            }
            g_topflow_external_temp[i] = value;
            g_topflow_external_temp_valid[i] = 1;
            g_topflow_external_temp_sampled_at[i] = now;
        }
        g_topflow_thermal_next_at = now + TOPFLOW_THERMAL_POLL_SEC;
    }
}

static void emit_topflow_thermal_modems(struct buf *b, long x75_temp)
{
    static const char *ids[TOPFLOW_MODEM_COUNT] = {"x75", "v3e1", "v3e2"};
    bappend(b, "[{");
    emit_kv_str(b, "id", ids[0]); bappend(b, ",");
    bappend(b, "\"available\":%s,\"celsius\":", x75_temp > 0 ? "true" : "false");
    if (x75_temp > 0) bappend(b, "%ld", x75_temp);
    else bappend(b, "null");
    bappend(b, "}");
    for (int i = 0; i < TOPFLOW_EXTERNAL_MODEM_COUNT; i++) {
        bappend(b, ",{");
        emit_kv_str(b, "id", ids[i + 1]); bappend(b, ",");
        bappend(b, "\"available\":%s,\"celsius\":",
                g_topflow_external_temp_valid[i] ? "true" : "false");
        if (g_topflow_external_temp_valid[i]) bappend(b, "%ld", g_topflow_external_temp[i]);
        else bappend(b, "null");
        bappend(b, ",\"sampled_at\":%ld}",
                (long)g_topflow_external_temp_sampled_at[i]);
    }
    bappend(b, "]");
}

static void prefixed_key(char *out, size_t outlen, const char *prefix, const char *suffix)
{
    snprintf(out, outlen, "%s%s", prefix ? prefix : "", suffix ? suffix : "");
}

static void emit_prefixed_str(struct buf *b, const char *key, const char *src,
                              const char *prefix, const char *suffix)
{
    char source_key[128];
    prefixed_key(source_key, sizeof source_key, prefix, suffix);
    emit_str(b, key, src, source_key);
}

static void emit_prefixed_int(struct buf *b, const char *key, const char *src,
                              const char *prefix, const char *suffix, long def)
{
    char source_key[128];
    prefixed_key(source_key, sizeof source_key, prefix, suffix);
    emit_int(b, key, src, source_key, def);
}

static int get_prefixed_lte_bandwidth(const char *src, const char *prefix,
                                      const char *suffix, char *out, size_t outlen)
{
    char source_key[128], raw[64], *start, *end, *parsed_end;
    const char *canonical = NULL;
    double mhz;
    prefixed_key(source_key, sizeof source_key, prefix, suffix);
    if (!json_get(src, source_key, raw, sizeof raw)) return 0;
    start = raw;
    while (*start && isspace((unsigned char)*start)) start++;
    end = start + strlen(start);
    while (end > start && isspace((unsigned char)end[-1])) end--;
    if (end - start >= 3 && !strncasecmp(end - 3, "MHz", 3)) {
        end -= 3;
        while (end > start && isspace((unsigned char)end[-1])) end--;
    }
    *end = 0;
    errno = 0;
    mhz = strtod(start, &parsed_end);
    if (errno || parsed_end == start || *parsed_end) return 0;
    if (mhz > 1.39 && mhz < 1.41) canonical = "1.4";
    else if (mhz == 3.0) canonical = "3";
    else if (mhz == 5.0) canonical = "5";
    else if (mhz == 10.0) canonical = "10";
    else if (mhz == 15.0) canonical = "15";
    else if (mhz == 20.0) canonical = "20";
    if (!canonical || snprintf(out, outlen, "%s", canonical) >= (int)outlen) return 0;
    return 1;
}

static void emit_realtime_traffic(struct buf *b, const char *src)
{
    emit_int(b, "rx_speed", src, "real_rx_speed", 0); bappend(b, ",");
    emit_int(b, "tx_speed", src, "real_tx_speed", 0); bappend(b, ",");
    emit_int(b, "max_rx_speed", src, "real_max_rx_speed", 0); bappend(b, ",");
    emit_int(b, "max_tx_speed", src, "real_max_tx_speed", 0); bappend(b, ",");
    emit_int(b, "rx_bytes", src, "real_rx_bytes", 0); bappend(b, ",");
    emit_int(b, "tx_bytes", src, "real_tx_bytes", 0); bappend(b, ",");
    emit_int(b, "session_time", src, "real_time", 0);
}

static void emit_modem_qos(struct buf *b, const struct qos_values *qos,
                           time_t sampled_at)
{
    bappend(b, "\"qos\":{");
    bappend(b, "\"qci\":%d,", qos && qos->qci_valid ? qos->qci : 0);
    if (qos && qos->ambr_dl_valid)
        bappend(b, "\"ambr_dl\":\"%.3f\",", qos->ambr_dl);
    else
        bappend(b, "\"ambr_dl\":\"\",");
    if (qos && qos->ambr_ul_valid)
        bappend(b, "\"ambr_ul\":\"%.3f\",", qos->ambr_ul);
    else
        bappend(b, "\"ambr_ul\":\"\",");
    bappend(b, "\"sampled_at\":%ld}", (long)sampled_at);
}

static void emit_topflow_external_modem(struct buf *b, int index)
{
    static const char *ids[TOPFLOW_EXTERNAL_MODEM_COUNT] = {"v3e1", "v3e2"};
    static const char *ifnames[TOPFLOW_EXTERNAL_MODEM_COUNT] = {"V3E1net0", "V3E2net0"};
    static const char *wan_names[TOPFLOW_EXTERNAL_MODEM_COUNT] = {"zte_mwan3", "zte_mwan4"};
    static const char *usb_paths[TOPFLOW_EXTERNAL_MODEM_COUNT] = {"1-1", "1-2"};
    static const char *usb_ids[TOPFLOW_EXTERNAL_MODEM_COUNT] = {"19d2:0581", "19d2:1716"};
    static const char *adb_serials[TOPFLOW_EXTERNAL_MODEM_COUNT] = {"V3E1T12345", "V3E2T12345"};
    char net_prefix[32], sim_prefix[32], path[256], bandwidth[8];
    int subid = topflow_external_subid(index);
    int has_bandwidth;
    long carrier;
    int usb_present, adb_available;

    snprintf(net_prefix, sizeof net_prefix, "msim_%d_%d_", index + 1,
             topflow_external_active_slot(index));
    snprintf(sim_prefix, sizeof sim_prefix, "v3t_%d_", index + 1);
    has_bandwidth = get_prefixed_lte_bandwidth(
        g_topflow_msim_netinfo, net_prefix, "lte_bandwidth", bandwidth, sizeof bandwidth);
    snprintf(path, sizeof path, "/sys/class/net/%s/carrier", ifnames[index]);
    carrier = read_long_file(path, 0);
    snprintf(path, sizeof path, "/sys/bus/usb/devices/%s/idVendor", usb_paths[index]);
    usb_present = access(path, R_OK) == 0;
    snprintf(path, sizeof path, "/sys/bus/usb/devices/%s/%s:1.3", usb_paths[index], usb_paths[index]);
    adb_available = access(path, F_OK) == 0;

    bappend(b, "{");
    emit_kv_str(b, "id", ids[index]); bappend(b, ",");
    emit_kv_str(b, "role", "external_4g"); bappend(b, ",");
    emit_kv_str(b, "transport", "cdc-ecm"); bappend(b, ",");
    bappend(b, "\"subid\":%d,", subid);
    emit_kv_str(b, "ifname", ifnames[index]); bappend(b, ",");
    emit_kv_str(b, "wan_interface", wan_names[index]); bappend(b, ",");
    bappend(b, "\"usb\":{");
    emit_kv_str(b, "path", usb_paths[index]); bappend(b, ",");
    emit_kv_str(b, "id", usb_ids[index]); bappend(b, ",");
    bappend(b, "\"present\":%s,\"carrier\":%ld},", usb_present ? "true" : "false", carrier);
    bappend(b, "\"debug\":{");
    emit_kv_str(b, "transport", "adb"); bappend(b, ",");
    emit_kv_str(b, "serial", adb_serials[index]); bappend(b, ",");
    bappend(b, "\"available\":%s},", adb_available ? "true" : "false");
    bappend(b, "\"net\":{");
    emit_prefixed_str(b, "type", g_topflow_msim_netinfo, net_prefix, "network_type"); bappend(b, ",");
    emit_prefixed_int(b, "bars", g_topflow_msim_netinfo, net_prefix, "signalbar", 0); bappend(b, ",");
    emit_prefixed_str(b, "roaming", g_topflow_msim_netinfo, net_prefix, "simcard_roam"); bappend(b, ",");
    emit_prefixed_str(b, "operator", g_topflow_msim_netinfo, net_prefix, "network_provider"); bappend(b, ",");
    emit_prefixed_str(b, "plmn", g_topflow_msim_netinfo, net_prefix, "rplmn_num"); bappend(b, ",");
    emit_prefixed_str(b, "band", g_topflow_msim_netinfo, net_prefix, "wan_active_band"); bappend(b, ",");
    emit_prefixed_int(b, "lte_rsrp", g_topflow_msim_netinfo, net_prefix, "lte_rsrp", 0); bappend(b, ",");
    emit_prefixed_int(b, "lte_rsrq", g_topflow_msim_netinfo, net_prefix, "lte_rsrq", 0); bappend(b, ",");
    emit_prefixed_int(b, "lte_rssi", g_topflow_msim_netinfo, net_prefix, "lte_rssi", 0); bappend(b, ",");
    emit_prefixed_str(b, "lte_snr", g_topflow_msim_netinfo, net_prefix, "lte_snr"); bappend(b, ",");
    emit_prefixed_int(b, "lte_pci", g_topflow_msim_netinfo, net_prefix, "lte_pci", 0); bappend(b, ",");
    emit_prefixed_int(b, "cell_id", g_topflow_msim_netinfo, net_prefix, "cell_id", 0); bappend(b, ",");
    emit_prefixed_int(b, "channel", g_topflow_msim_netinfo, net_prefix, "wan_active_channel", 0);
    if (has_bandwidth) {
        bappend(b, ",");
        emit_kv_str(b, "bandwidth", bandwidth);
    }
    bappend(b, ",");
    emit_prefixed_str(b, "mode", g_topflow_msim_netinfo, net_prefix, "net_select"); bappend(b, ",");
    emit_prefixed_str(b, "operate_mode", g_topflow_msim_netinfo, net_prefix, "operate_mode");
    bappend(b, "},\"sim\":{");
    emit_prefixed_str(b, "state", g_topflow_v3t_sim, sim_prefix, "modem_main_state"); bappend(b, ",");
    emit_prefixed_int(b, "slot", g_topflow_v3t_sim, sim_prefix, "st_slot", 0); bappend(b, ",");
    emit_prefixed_str(b, "iccid", g_topflow_v3t_sim, sim_prefix, "sim_iccid"); bappend(b, ",");
    emit_prefixed_str(b, "imsi", g_topflow_v3t_sim, sim_prefix, "sim_imsi"); bappend(b, ",");
    emit_prefixed_str(b, "msisdn", g_topflow_v3t_sim, sim_prefix, "msisdn"); bappend(b, ",");
    emit_prefixed_str(b, "imei", g_topflow_v3t_sim, sim_prefix, "imei");
    bappend(b, "},\"wwan\":{");
    emit_str(b, "status", g_topflow_wwaniface[index], "connect_status"); bappend(b, ",");
    emit_str(b, "ipv4_ifname", g_topflow_wwaniface[index], "ipv4_dev_name"); bappend(b, ",");
    emit_str(b, "ipv6_ifname", g_topflow_wwaniface[index], "ipv6_dev_name");
    bappend(b, "},\"interfaces\":{");
    emit_interface_status(b, "ipv4", g_topflow_wan4[index + 1]); bappend(b, ",");
    emit_interface_status(b, "ipv6", g_topflow_wan6[index + 1]);
    bappend(b, "},\"traffic\":{");
    emit_realtime_traffic(b, g_topflow_traffic[index]);
    bappend(b, "},");
    emit_modem_qos(b, &g_topflow_external_qos[index],
                   g_topflow_external_qos_sampled_at[index]);
    bappend(b, "}");
}

static void emit_topflow_modems(struct buf *b, const char *net, const char *traffic,
                                const char *imei_cache, const struct qos_values *qos)
{
    char network_type[32] = "", bandwidth[32] = "";
    if (!g_topflow_multimodem_enabled) {
        bappend(b, "[]");
        return;
    }

    (void)json_get(net, "network_type", network_type, sizeof network_type);
    if (!strcmp(network_type, "SA") || !strcmp(network_type, "NSA"))
        (void)json_get(net, "nr5g_bandwidth", bandwidth, sizeof bandwidth);
    if (!bandwidth[0])
        (void)json_get(net, "lte_bandwidth", bandwidth, sizeof bandwidth);

    bappend(b, "[{");
    emit_kv_str(b, "id", "x75"); bappend(b, ",");
    emit_kv_str(b, "role", "integrated_5g"); bappend(b, ",");
    emit_kv_str(b, "transport", "rmnet"); bappend(b, ",");
    bappend(b, "\"subid\":%d,", g_active_subid);
    emit_kv_str(b, "ifname", "rmnet_data0"); bappend(b, ",");
    emit_kv_str(b, "wan_interface", "zte_mwan2"); bappend(b, ",");
    bappend(b, "\"net\":{");
    emit_str(b, "type", net, "network_type"); bappend(b, ",");
    emit_int(b, "bars", net, "signalbar", 0); bappend(b, ",");
    emit_str(b, "roaming", net, "simcard_roam"); bappend(b, ",");
    emit_str(b, "operator", net, "network_provider_fullname"); bappend(b, ",");
    emit_str(b, "band", net, "wan_active_band"); bappend(b, ",");
    emit_kv_str(b, "bandwidth", bandwidth); bappend(b, ",");
    emit_int(b, "nr_rsrp", net, "nr5g_rsrp", 0); bappend(b, ",");
    emit_int(b, "nr_rsrq", net, "nr5g_rsrq", 0); bappend(b, ",");
    emit_str(b, "nr_snr", net, "nr5g_snr"); bappend(b, ",");
    emit_int(b, "nr_pci", net, "nr5g_pci", 0); bappend(b, ",");
    emit_int(b, "nr_cell_id", net, "nr5g_cell_id", 0); bappend(b, ",");
    emit_int(b, "nr_channel", net, "nr5g_action_channel", 0); bappend(b, ",");
    emit_int(b, "lte_rsrp", net, "lte_rsrp", 0); bappend(b, ",");
    emit_int(b, "lte_rsrq", net, "lte_rsrq", 0); bappend(b, ",");
    emit_int(b, "lte_pci", net, "lte_pci", 0); bappend(b, ",");
    emit_int(b, "cell_id", net, "cell_id", 0);
    bappend(b, "},\"sim\":{");
    emit_str(b, "state", g_sim_cache, "sim_states"); bappend(b, ",");
    emit_int(b, "slot", g_sim_cache, "current_sim_slot", 0); bappend(b, ",");
    emit_str(b, "iccid", g_sim_cache, "sim_iccid"); bappend(b, ",");
    emit_sim_identity(b, "imsi", g_sim_cache, "sim_imsi"); bappend(b, ",");
    emit_sim_identity(b, "msisdn", g_sim_cache, "msisdn"); bappend(b, ",");
    emit_str(b, "imei", imei_cache, "imei");
    bappend(b, "},\"wwan\":{");
    emit_str(b, "status", g_cellular_runtime, "connect_status"); bappend(b, ",");
    emit_str(b, "ipv4_ifname", g_cellular_runtime, "ipv4_dev_name"); bappend(b, ",");
    emit_str(b, "ipv6_ifname", g_cellular_runtime, "ipv6_dev_name");
    bappend(b, "},\"interfaces\":{");
    emit_interface_status(b, "ipv4", g_topflow_wan4[0]); bappend(b, ",");
    emit_interface_status(b, "ipv6", g_topflow_wan6[0]);
    bappend(b, "},\"traffic\":{");
    emit_realtime_traffic(b, traffic);
    bappend(b, "},");
    emit_modem_qos(b, qos, 0);
    bappend(b, "}");
    for (int i = 0; i < TOPFLOW_EXTERNAL_MODEM_COUNT; i++) {
        bappend(b, ",");
        emit_topflow_external_modem(b, i);
    }
    bappend(b, "]");
}

/* Poll everything and build the unified snapshot into `out`. */
static void build_snapshot(char *out, size_t outlen,
                           int with_board, const char *board_cache,
                           int with_common, const char *common_cache,
                           int with_imei, const char *imei_cache,
                           int force_refresh)
{
    char net[RAW_MAX], traf[RAW_MAX];
    static char batt[RAW_MAX], chg[RAW_MAX], therm[1024];
    static char rnum[1024], rstat[1024], sysinfo[2048], usb[1024], nfc[1024];
    static char wifi_ssid[128], wifi_key[128], wifi_enc[64];
    static char dhcp_ip[32], dhcp_start[32], dhcp_limit[16], dhcp_lease[32];
    char device_profile[64], device_profile_source[64];
    char device_vendor[64], device_model_name[128], device_hw[128];
    char device_market_name[128], device_alias_name[128], device_board_name[128];
    static char client_list[CLIENT_LIST_MAX] = "[]";
    static int wifi_enabled;
    static time_t power_next_at;
    static time_t slow_state_next_at;
    time_t poll_now = time(NULL);
    long chg_uv, chg_ua, bat_uv, bat_ua, cpu_temp;
    int cpu_usage, cpu_usage_tenths;
    int show_battery, show_wifi, show_nfc, show_sms;
    char runtime_json[32768], thermal_zones_json[16384];
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
    if (force_refresh || power_next_at == 0 || poll_now >= power_next_at) {
        run_ubus("zwrt_bsp.battery", "list", NULL, batt, sizeof batt);
        run_ubus("zwrt_bsp.charger", "list", NULL, chg, sizeof chg);
        load_thermal_snapshot_for_template(device_template, therm, sizeof therm);
        power_next_at = poll_now + POWER_POLL_SEC;
    }
    /* type:1 = realtime session stats; cid:1 = main PDN (rmnet_data0). */
    load_traffic_snapshot_for_template(device_template, traf, sizeof traf);
    if (force_refresh || slow_state_next_at == 0 || poll_now >= slow_state_next_at) {
        run_ubus("zwrt_router.api", "router_get_user_list_num", NULL, rnum, sizeof rnum);
        run_ubus("zwrt_router.api", "router_get_status_no_auth", NULL, rstat, sizeof rstat);
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
        slow_state_next_at = poll_now + SLOW_STATE_POLL_SEC;
    }
    chg_uv = read_long_file("/sys/class/power_supply/usb/voltage_now", 0);
    chg_ua = read_long_file("/sys/class/power_supply/usb/current_now", 0);
    bat_uv = read_long_file("/sys/class/power_supply/battery/voltage_now", 0);
    bat_ua = read_long_file("/sys/class/power_supply/battery/current_now", 0);
    cpu_usage_tenths = system_ext_build_json(runtime_json, sizeof runtime_json,
                                             thermal_zones_json, sizeof thermal_zones_json);
    cpu_usage = cpu_usage_tenths >= 0 ? (cpu_usage_tenths + 5) / 10 : -1;
    cpu_temp = read_cpu_temp_for_template(device_template, therm);
    {
        static const char *const battery_keys[] = {
            "battery_capacity", "battery_online", "battery_health", "battery_temperature"
        };
        static const char *const nfc_keys[] = {"switch", "ap", "wifi_ap"};
        int battery_detected = json_has_any_key(
            batt, battery_keys, sizeof battery_keys / sizeof battery_keys[0]) ||
            bat_uv > 0 || bat_ua != 0;
        int wifi_detected = wifi_ssid[0] || wifi_key[0] || wifi_enc[0];
        int nfc_detected = json_has_any_key(
            nfc, nfc_keys, sizeof nfc_keys / sizeof nfc_keys[0]);
        show_battery = optional_section_enabled(device_template->battery_section, battery_detected);
        show_wifi = optional_section_enabled(device_template->wifi_section, wifi_detected);
        show_nfc = optional_section_enabled(device_template->nfc_section, nfc_detected);
        show_sms = optional_section_enabled(device_template->sms_section, g_sms_interface_detected);
    }
    qos_mcc = json_get_int(net, "rmcc", 0);
    qos_mnc = json_get_int(net, "rmnc", 0);
    select_qos_for_plmn(qos_mcc, qos_mnc, &qos);

    struct buf b = { out, outlen, 0 };
    bappend(&b, "{\"ts\":%ld,", (long)time(NULL));

    /* network / signal */
    bappend(&b, "\"net\":{");
    emit_str(&b, "type", net, "network_type");      bappend(&b, ",");
    emit_int(&b, "bars", net, "signalbar", 0);       bappend(&b, ",");
    emit_str(&b, "roaming", net, "simcard_roam");  bappend(&b, ",");
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
    emit_str(&b, "lte_supported_bands", net, "lte_band"); bappend(&b, ",");
    emit_str(&b, "nr_sa_supported_bands", net, "nr5g_sa_band_lock"); bappend(&b, ",");
    emit_str(&b, "nr_nsa_supported_bands", net, "nr5g_nsa_band_lock"); bappend(&b, ",");
    emit_str(&b, "wan_status", rstat, "current_wan_status"); bappend(&b, ",");
    bappend(&b, "\"HSR\":false");
    bappend(&b, "},");

    /* battery / charger */
    if (show_battery) {
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
    }

    /* connected clients */
    bappend(&b, "\"clients\":{");
    emit_int(&b, "total", rnum, "access_total_num", 0); bappend(&b, ",");
    emit_int(&b, "wifi", rnum, "wireless_num", 0);      bappend(&b, ",");
    emit_int(&b, "lan", rnum, "lan_num", 0);            bappend(&b, ",");
    bappend(&b, "\"list\":%s", client_list);
    bappend(&b, "},");

    /* sms */
    if (show_sms) {
        bappend(&b, "\"sms\":{");
        bappend(&b, "\"unread\":%ld,", g_sms_unread_cache);
        bappend(&b, "\"list\":%s", g_sms_list_valid ? g_sms_list_cache : "[]");
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
    if (show_wifi) {
        bappend(&b, "\"wlan\":{");
        bappend(&b, "\"ssid\":\""); bappend_json_esc(&b, wifi_ssid); bappend(&b, "\",");
        bappend(&b, "\"enc\":\"");  bappend_json_esc(&b, wifi_enc);  bappend(&b, "\",");
        bappend(&b, "\"enabled\":%d", wifi_enabled);
        bappend(&b, "},");
    }

    /* nfc */
    if (show_nfc) {
        bappend(&b, "\"nfc\":{");
        emit_int(&b, "switch", nfc, "switch", 0);
        bappend(&b, "},");
    }

    /* Template-normalized thermal data; delivered in /state and SSE snapshots. */
    bappend(&b, "\"thermal\":{\"cpu_celsius\":%ld,\"zones\":%s,\"modems\":",
            cpu_temp,
            device_template->thermal_zones && thermal_zones_json[0] ?
                thermal_zones_json : "[]");
    if (g_topflow_multimodem_enabled) emit_topflow_thermal_modems(&b, cpu_temp);
    else bappend(&b, "[]");
    bappend(&b, "},");

    /* MU5252-only aggregation and active cooling controls. Unsupported models
     * omit these blocks entirely so consumers can hide the corresponding UI. */
    if (g_topflow_multimodem_enabled)
        emit_topflow_hardware_controls(&b);

    /* Runtime interface state is refreshed at a lower cadence than radio data. */
    bappend(&b, "\"interfaces\":{");
    emit_interface_status(&b, "lan", g_lan_interface); bappend(&b, ",");
    emit_interface_status(&b, "wan4", g_wan4_interface); bappend(&b, ",");
    emit_interface_status(&b, "wan6", g_wan6_interface); bappend(&b, ",");
    bappend(&b, "\"lan_config\":%s,\"cellular\":%s",
            g_lan_runtime[0] ? g_lan_runtime : "{}",
            g_cellular_runtime[0] ? g_cellular_runtime : "{}");
    bappend(&b, "},");

    /* Persistent UCI state complements the realtime ubus sections above. */
    bappend(&b, "\"uci_device_info\":%s,",
            show_battery ?
                (g_uci_device_info[0] ? g_uci_device_info : "{}") :
                (g_uci_device_info_no_battery[0] ? g_uci_device_info_no_battery : "{}"));

    /* SIM identity and provisioning state. Values remain device-local. */
    bappend(&b, "\"sim\":{");
    emit_str(&b, "iccid", g_sim_cache, "sim_iccid"); bappend(&b, ",");
    emit_sim_identity(&b, "imsi", g_sim_cache, "sim_imsi"); bappend(&b, ",");
    emit_sim_identity(&b, "msisdn", g_sim_cache, "msisdn"); bappend(&b, ",");
    emit_str(&b, "state", g_sim_cache, "sim_states"); bappend(&b, ",");
    emit_str(&b, "modem_state", g_sim_cache, "modem_main_state"); bappend(&b, ",");
    emit_str(&b, "pin_status", g_sim_cache, "pin_status"); bappend(&b, ",");
    emit_int(&b, "current_slot", g_sim_cache, "current_sim_slot", 0); bappend(&b, ",");
    emit_int(&b, "dual_sim", g_sim_cache, "support_dual_sim", 0); bappend(&b, ",");
    emit_int(&b, "sim1_provision", g_sim_cache, "sim1_provision_state", 0); bappend(&b, ",");
    emit_int(&b, "sim2_provision", g_sim_cache, "sim2_provision_state", 0);
    bappend(&b, "},");

    /* MU5252/TopFlow exposes one integrated X75 and two CDC-ECM LTE modems. */
    bappend(&b, "\"modems\":");
    emit_topflow_modems(&b, net, traf, imei_cache, &qos);
    bappend(&b, ",");

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
    bappend(&b, "\"full_ubus\":%d,", device_template->full_ubus);
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

    bappend(&b, "\"sample_interval_ms\":%d,", g_sample_interval_ms);
    /* Realtime system details that are intentionally sampled once per daemon cycle. */
    bappend(&b, "\"runtime\":%s}", runtime_json[0] ? runtime_json : "{}");
}

static void refresh_interface_cache(void)
{
    char next[RAW_MAX];
    char args[192];
    if (run_ubus("network.interface.lan", "status", NULL, next, sizeof next) == 0)
        copy_text(g_lan_interface, sizeof g_lan_interface, next);
    if (run_ubus(g_topflow_multimodem_enabled ? "network.interface.zte_mwan2" :
                 "network.interface.zte_wan", "status", NULL, next, sizeof next) == 0)
        copy_text(g_wan4_interface, sizeof g_wan4_interface, next);
    if (run_ubus(g_topflow_multimodem_enabled ? "network.interface.zte_mwan2_6" :
                 "network.interface.zte_wan6", "status", NULL, next, sizeof next) == 0)
        copy_text(g_wan6_interface, sizeof g_wan6_interface, next);
    if (g_topflow_multimodem_enabled) {
        copy_text(g_topflow_wan4[0], sizeof g_topflow_wan4[0], g_wan4_interface);
        copy_text(g_topflow_wan6[0], sizeof g_topflow_wan6[0], g_wan6_interface);
    }
    if (run_ubus("zwrt_router.api", "router_get_lan_info", NULL, next, sizeof next) == 0)
        copy_text(g_lan_runtime, sizeof g_lan_runtime, next);
    if (g_topflow_multimodem_enabled)
        snprintf(args, sizeof args,
                 "{\"source_module\":\"web\",\"cid\":1,\"subid\":%d}", g_active_subid);
    else
        snprintf(args, sizeof args,
                 "{\"source_module\":\"web\",\"cid\":1,\"connect_status\":\"\"}");
    if (run_ubus("zwrt_data", "get_wwaniface", args, next, sizeof next) == 0)
        copy_text(g_cellular_runtime, sizeof g_cellular_runtime, next);
    if (g_topflow_multimodem_enabled)
        snprintf(args, sizeof args,
                 "{\"source_module\":\"web\",\"cid\":1,\"type\":4,\"subid\":%d}",
                 g_active_subid);
    else
        snprintf(args, sizeof args,
                 "{\"source_module\":\"web\",\"cid\":1,\"type\":4}");
    if (run_ubus("zwrt_data", "get_wwandst", args, next, sizeof next) == 0)
        copy_text(g_traffic_accounting, sizeof g_traffic_accounting, next);
    if (g_topflow_multimodem_enabled)
        snprintf(args, sizeof args,
                 "{\"source_module\":\"web\",\"cid\":1,\"subid\":%d}", g_active_subid);
    else
        snprintf(args, sizeof args, "{\"source_module\":\"web\",\"cid\":1}");
    if (run_ubus("zwrt_data", "get_wwandst_monthlimit", args, next, sizeof next) == 0)
        copy_text(g_traffic_limit, sizeof g_traffic_limit, next);
    if (run_ubus("zwrt_data", "get_wwandst_clearday", args, next, sizeof next) == 0)
        copy_text(g_traffic_clear_day, sizeof g_traffic_clear_day, next);
    refresh_uci_device_info();
    refresh_topflow_multimodem_cache();
    if (g_topflow_multimodem_enabled) refresh_topflow_aggregation_cache();
}

static int set_nonblock(int fd)
{
    int fl = fcntl(fd, F_GETFL, 0);
    if (fl < 0) return -1;
    return fcntl(fd, F_SETFL, fl | O_NONBLOCK);
}

/* Keep server/client sockets out of the external commands we exec (ubus, adb,
 * uci, ...). Without this the device-internal adb fork-server inherits and
 * pins the HTTP listener, so 9460/9461 stay in LISTEN after datad exits and
 * service.sh restart fails with "bind: Address in use" (issue #26). */
static int set_cloexec(int fd)
{
    int fl = fcntl(fd, F_GETFD, 0);
    if (fl < 0) return -1;
    return fcntl(fd, F_SETFD, fl | FD_CLOEXEC);
}

static int accept_cloexec(int listen_fd, struct sockaddr *addr, socklen_t *addrlen)
{
    int fd;
#ifdef __linux__
    fd = accept4(listen_fd, addr, addrlen, SOCK_CLOEXEC);
    if (fd >= 0 || errno != ENOSYS) return fd;
#endif
    fd = accept(listen_fd, addr, addrlen);
    if (fd >= 0) (void)set_cloexec(fd);
    return fd;
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

static int peer_is_loopback(const struct sockaddr_in *peer)
{
    uint32_t ip;
    if (!peer) return 0;
    ip = ntohl(peer->sin_addr.s_addr);
    return (ip & 0xff000000U) == 0x7f000000U;
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

static int valid_ubus_identifier(const char *value)
{
    if (!value || !*value) return 0;
    for (const unsigned char *p = (const unsigned char *)value; *p; p++) {
        if (!isalnum(*p) && *p != '_' && *p != '-' && *p != '.') return 0;
    }
    return 1;
}

static void write_ubus_call_error(int fd, int status, const char *status_text,
                                  const char *code, const char *message)
{
    char body[768];
    struct buf b = {body, sizeof body, 0};
    bappend(&b, "{\"ok\":false,\"error\":{\"code\":\"");
    bappend_json_esc(&b, code ? code : "ubus_error");
    bappend(&b, "\",\"message\":\"");
    bappend_json_esc(&b, message ? message : "ubus call failed");
    bappend(&b, "\"}}");
    (void)write_http_json_status(fd, status, status_text, body, strlen(body));
}

static int handle_full_ubus_call(int fd, const char *body)
{
    char service[256], method[256];
    const char *args = NULL;
    struct buf response = {g_ubus_call_response, sizeof g_ubus_call_response, 0};

    service[0] = method[0] = g_ubus_call_args[0] = g_ubus_call_result[0] = 0;
    if (!g_full_ubus_enabled) {
        write_ubus_call_error(fd, 404, "Not Found", "unsupported_device",
                              "full ubus calls are not enabled for this device template");
        return 0;
    }
    if (!body || !json_is_valid_object(body)) {
        write_ubus_call_error(fd, 400, "Bad Request", "invalid_request",
                              "request body must be a complete JSON object");
        return 0;
    }
    if (!json_get(body, "service", service, sizeof service) ||
        !json_get(body, "method", method, sizeof method) ||
        !valid_ubus_identifier(service) || !valid_ubus_identifier(method)) {
        write_ubus_call_error(fd, 400, "Bad Request", "invalid_target",
                              "service and method must be valid ubus identifiers");
        return 0;
    }
    if (json_get(body, "args", g_ubus_call_args, sizeof g_ubus_call_args)) {
        if (!json_is_valid_object(g_ubus_call_args)) {
            write_ubus_call_error(fd, 400, "Bad Request", "invalid_args",
                                  "args must be a JSON object");
            return 0;
        }
        args = g_ubus_call_args;
    }
    if (device_ubus_call_raw(service, method, args,
                             g_ubus_call_result, sizeof g_ubus_call_result) != 0) {
        write_ubus_call_error(fd, 502, "Bad Gateway", "device_call_failed",
                              "device ubus call failed");
        return 0;
    }

    bappend(&response, "{\"ok\":true,\"service\":\"");
    bappend_json_esc(&response, service);
    bappend(&response, "\",\"method\":\"");
    bappend_json_esc(&response, method);
    if (!g_ubus_call_result[0]) {
        bappend(&response, "\",\"result\":null}");
    } else if (json_is_valid_object(g_ubus_call_result)) {
        bappend(&response, "\",\"result\":%s}", g_ubus_call_result);
    } else {
        bappend(&response, "\",\"result_text\":\"");
        bappend_json_esc(&response, g_ubus_call_result);
        bappend(&response, "\"}");
    }
    (void)write_http_json_status(fd, 200, "OK", g_ubus_call_response,
                                 strlen(g_ubus_call_response));
    return 1;
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
    static const char event_line[] = "event: state\n";
    static const char data_prefix[] = "data: ";
    static const char lf[] = "\n";

    if (write_all(fd, event_line, sizeof event_line - 1) < 0) return -1;

    /* Per the SSE spec every physical line of the payload must carry its own
     * "data:" prefix; a bare newline terminates the event. The snapshot can
     * legitimately contain embedded newlines (e.g. pretty-printed JSON passed
     * through verbatim from ubus), so emit one "data:" line per segment. A
     * spec-compliant client rejoins the segments with "\n", reproducing the
     * original bytes. */
    size_t start = 0;
    for (size_t i = 0; i <= snap_len; i++) {
        if (i < snap_len && snap[i] != '\n') continue;
        if (write_all(fd, data_prefix, sizeof data_prefix - 1) < 0) return -1;
        if (i > start && write_all(fd, snap + start, i - start) < 0) return -1;
        if (write_all(fd, lf, sizeof lf - 1) < 0) return -1;
        start = i + 1;
    }
    return write_all(fd, lf, sizeof lf - 1);
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
    if (set_cloexec(fd) < 0) {
        perror("fcntl");
        close(fd);
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
        int cli_fd = accept_cloexec(listener->fd, (struct sockaddr *)&peer, &peer_len);
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
                                      "GET  /ubus         -> ubus object catalog (?verbose=1)\n"
                                      "POST /ubus/call    -> template-enabled full ubus call\n"
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

        if (!strcmp(path, "/ubus") || !strcmp(path, "/ubus/list")) {
            char verbose_value[16] = "";
            int verbose = query_value(query, "verbose", verbose_value, sizeof verbose_value) &&
                          (!strcmp(verbose_value, "1") || !strcmp(verbose_value, "true"));
            if (strcmp(method, "GET") != 0) {
                write_http_error(cli_fd, 405, "Method Not Allowed");
            } else if (device_ubus_list(verbose, g_ubus_catalog, sizeof g_ubus_catalog) != 0) {
                write_http_error(cli_fd, 502, "Bad Gateway");
            } else {
                (void)write_http_text(cli_fd, "text/plain; charset=utf-8", g_ubus_catalog);
            }
            close(cli_fd);
            continue;
        }

        if (!strcmp(path, "/ubus/call")) {
            char *body = strstr(req, "\r\n\r\n");
            int called = 0;
            int refresh_state = 1;
            char refresh_value[16];
            if (strcmp(method, "POST") != 0) {
                write_http_error(cli_fd, 405, "Method Not Allowed");
            } else if (!listener->auth_required && !peer_is_loopback(&peer)) {
                write_http_error(cli_fd, 403, "Forbidden");
            } else {
                body = body ? body + 4 : NULL;
                if (body && json_get(body, "refresh_state", refresh_value, sizeof refresh_value) &&
                    (!strcmp(refresh_value, "false") || !strcmp(refresh_value, "0")))
                    refresh_state = 0;
                called = handle_full_ubus_call(cli_fd, body);
            }
            close(cli_fd);
            if (called && refresh_state) g_state_refresh_req = 1;
            continue;
        }

        if (!strcmp(path, "/control")) {
            char *body = strstr(req, "\r\n\r\n");
            struct control_result control_status;
            const char *http_status = "OK";
            char action[128] = "";
            if (strcmp(method, "POST") != 0) {
                write_http_error(cli_fd, 405, "Method Not Allowed");
                close(cli_fd);
                continue;
            }
            body = body ? body + 4 : NULL;
            if (body) (void)json_get(body, "action", action, sizeof action);
            control_status = control_execute(body, control_response, sizeof control_response);
            if (control_status.http_status == 400) http_status = "Bad Request";
            else if (control_status.http_status == 404) http_status = "Not Found";
            else if (control_status.http_status == 502) http_status = "Bad Gateway";
            (void)write_http_json_status(cli_fd, control_status.http_status, http_status,
                                         control_response, strlen(control_response));
            close(cli_fd);
            if (control_status.http_status >= 200 && control_status.http_status < 300 &&
                !strncmp(action, "sms.", 4))
                g_sms_list_valid = 0;
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
    int sim_poll_every, interface_poll_every, sms_poll_every;
    int qos_retry_every, qos_retry_left = 0;
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
    g_sample_interval_ms = interval_ms;

    sim_poll_every = (SIM_POLL_MS + interval_ms - 1) / interval_ms;
    interface_poll_every = sim_poll_every;
    sms_poll_every = (SMS_UNREAD_POLL_MS + interval_ms - 1) / interval_ms;
    qos_retry_every = (SIM_POLL_MS + interval_ms - 1) / interval_ms;
    if (sim_poll_every < 1) sim_poll_every = 1;
    if (interface_poll_every < 1) interface_poll_every = 1;
    if (sms_poll_every < 1) sms_poll_every = 1;
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
    update_device_template_features(common);
    if (g_topflow_multimodem_enabled) control_restore_cooling_state();
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
        int requested_interval_ms = control_take_requested_interval_ms();
        if (requested_interval_ms >= 500 && requested_interval_ms <= 5000 &&
            requested_interval_ms != interval_ms) {
            interval_ms = requested_interval_ms;
            g_sample_interval_ms = interval_ms;
            sim_poll_every = (SIM_POLL_MS + interval_ms - 1) / interval_ms;
            interface_poll_every = sim_poll_every;
            sms_poll_every = (SMS_UNREAD_POLL_MS + interval_ms - 1) / interval_ms;
            qos_retry_every = (SIM_POLL_MS + interval_ms - 1) / interval_ms;
            if (sim_poll_every < 1) sim_poll_every = 1;
            if (interface_poll_every < 1) interface_poll_every = 1;
            if (sms_poll_every < 1) sms_poll_every = 1;
            if (qos_retry_every < 1) qos_retry_every = 1;
            if (qos_retry_left > 0)
                qos_retry_left = (QOS_RETRY_MS + interval_ms - 1) / interval_ms;
            cycle = 0;
        }
        int force_refresh = g_state_refresh_req;
        g_state_refresh_req = 0;
        if (cycle % 3600 == 0 && cycle != 0) {
            run_ubus("system", "board", NULL, board, sizeof board);
            run_ubus("zwrt_zte_mdm.api", "get_zwrt_common_info", NULL, common, sizeof common);
            run_ubus("zwrt_zte_mdm.api", "get_imei", NULL, imei, sizeof imei);
            update_device_template_features(common);
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
            g_topflow_external_qos_next_at = 0;
            if (g_topflow_multimodem_enabled)
                refresh_topflow_external_qos_cache(time(NULL));
            if (qos_cache_has_values()) qos_retry_left = 0;
        } else if (qos_retry_left > 0) {
            if (qos_retry_left == 1 || (qos_retry_left % qos_retry_every) == 0)
                refresh_qos_cache();
            if (qos_cache_has_values()) qos_retry_left = 0;
            else qos_retry_left--;
        }

        if (!g_sms_list_valid || cycle == 0 || cycle % sms_poll_every == 0) {
            long cur_sms_unread = read_sms_unread_count(g_sms_unread_cache);
            if (cur_sms_unread >= 0) g_sms_unread_cache = cur_sms_unread;
            if (!g_sms_list_valid || g_sms_unread_cache != last_sms_unread) {
                int full_sms_refresh = !g_sms_list_valid;
                if (refresh_sms_cache(full_sms_refresh)) g_sms_list_valid = 1;
            }
            last_sms_unread = g_sms_unread_cache;
        }

        build_snapshot(snap, sizeof snap,
                       board[0] != 0, board,
                       common[0] != 0, common,
                       imei[0] != 0, imei,
                       force_refresh || cycle == 0);

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

    if (g_topflow_multimodem_enabled) control_release_cooling_state();
    for (size_t i = 0; i < HTTP_MAX_CLIENTS; i++) sse_client_close(&clients[i]);
    if (lan_listener.fd >= 0) close(lan_listener.fd);
    if (local_listener.fd >= 0) close(local_listener.fd);
    return 0;
}
