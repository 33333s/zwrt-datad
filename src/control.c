/*
 * Private, allow-listed device control API.
 *
 * This layer intentionally exposes device semantics rather than legacy UFI
 * goform names. UFI remains responsible for translating its public API into
 * these actions.
 *
 * SPDX-License-Identifier: MIT
 */
#include "control.h"
#include "device_exec.h"
#include "json.h"

#include <ctype.h>
#include <errno.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CONTROL_PARAMS_MAX 16384
#define CONTROL_UBUS_ARGS_MAX 16384
#define CONTROL_UBUS_RESPONSE_MAX 32768

enum param_kind {
    PARAM_STRING,
    PARAM_INT,
    PARAM_BOOL
};

struct param_spec {
    const char *input;
    const char *output;
    enum param_kind kind;
    int required;
};

struct json_buf {
    char *data;
    size_t cap;
    size_t len;
};

static char g_device_session[256];
static char g_device_password_hash[129];

static void jb_add(struct json_buf *b, const char *fmt, ...)
{
    va_list ap;
    int n;
    size_t room;
    if (!b || b->len >= b->cap) return;
    room = b->cap - b->len;
    va_start(ap, fmt);
    n = vsnprintf(b->data + b->len, room, fmt, ap);
    va_end(ap);
    if (n <= 0) return;
    if ((size_t)n >= room) {
        b->len = b->cap - 1;
        b->data[b->len] = 0;
    } else {
        b->len += (size_t)n;
    }
}

static void jb_string(struct json_buf *b, const char *s)
{
    jb_add(b, "\"");
    if (!s) s = "";
    for (; *s; s++) {
        unsigned char c = (unsigned char)*s;
        if (c == '"' || c == '\\') jb_add(b, "\\%c", c);
        else if (c == '\n') jb_add(b, "\\n");
        else if (c == '\r') jb_add(b, "\\r");
        else if (c == '\t') jb_add(b, "\\t");
        else if (c < 0x20) jb_add(b, " ");
        else jb_add(b, "%c", c);
    }
    jb_add(b, "\"");
}

static void write_error(char *out, size_t outlen, const char *action,
                        const char *code, const char *message)
{
    struct json_buf b = {out, outlen, 0};
    if (!outlen) return;
    out[0] = 0;
    jb_add(&b, "{\"ok\":false,\"action\":");
    jb_string(&b, action ? action : "");
    jb_add(&b, ",\"error\":{\"code\":");
    jb_string(&b, code ? code : "control_failed");
    jb_add(&b, ",\"message\":");
    jb_string(&b, message ? message : "device control failed");
    jb_add(&b, "}}");
}

static void write_success(char *out, size_t outlen, const char *action,
                          const char *result)
{
    struct json_buf b = {out, outlen, 0};
    if (!outlen) return;
    out[0] = 0;
    jb_add(&b, "{\"ok\":true,\"action\":");
    jb_string(&b, action);
    jb_add(&b, ",\"result\":");
    if (result && (*result == '{' || *result == '[')) jb_add(&b, "%s", result);
    else if (result && *result) jb_string(&b, result);
    else jb_add(&b, "{}");
    jb_add(&b, "}");
}

/* Prefix validation failures so the HTTP layer can distinguish them from
 * device-side execution failures without leaking the marker to callers. */
static void set_invalid_error(char *err, size_t errlen, const char *fmt, ...)
{
    va_list ap;
    if (!err || errlen < 2) return;
    err[0] = '\1';
    va_start(ap, fmt);
    (void)vsnprintf(err + 1, errlen - 1, fmt, ap);
    va_end(ap);
}

static int valid_hex_hash(const char *s)
{
    size_t n = s ? strlen(s) : 0;
    if (n != 64) return 0;
    for (size_t i = 0; i < n; i++) if (!isxdigit((unsigned char)s[i])) return 0;
    return 1;
}

static int valid_band_list(const char *s)
{
    if (!s) return 0;
    for (; *s; s++) {
        if (!isdigit((unsigned char)*s) && *s != ',' && !isspace((unsigned char)*s)) return 0;
    }
    return 1;
}

static int valid_mac(const char *s)
{
    int octets = 0;
    if (!s) return 0;
    while (*s) {
        if (!isxdigit((unsigned char)s[0]) || !isxdigit((unsigned char)s[1])) return 0;
        s += 2;
        octets++;
        if (octets == 6) return *s == 0;
        if (*s != ':') return 0;
        s++;
    }
    return 0;
}

static int param_value(const char *params, const char *key, char *out, size_t outlen)
{
    if (!params || !json_get(params, key, out, outlen)) return 0;
    return out[0] != 0;
}

static int normalized_int(const char *s, long *value)
{
    char *end;
    long v;
    if (!s || !*s) return 0;
    v = strtol(s, &end, 10);
    while (*end && isspace((unsigned char)*end)) end++;
    if (*end) return 0;
    *value = v;
    return 1;
}

static int build_args(const char *params, const struct param_spec *specs, size_t count,
                      char *out, size_t outlen, char *err, size_t errlen)
{
    struct json_buf b = {out, outlen, 0};
    int emitted = 0;
    out[0] = 0;
    jb_add(&b, "{");
    for (size_t i = 0; i < count; i++) {
        char value[4096];
        long number;
        int present = json_get(params, specs[i].input, value, sizeof value);
        if (!present) {
            if (specs[i].required) {
                set_invalid_error(err, errlen, "missing parameter: %s", specs[i].input);
                return 0;
            }
            continue;
        }
        if (specs[i].required && !value[0]) {
            set_invalid_error(err, errlen, "empty parameter: %s", specs[i].input);
            return 0;
        }
        if (emitted++) jb_add(&b, ",");
        jb_string(&b, specs[i].output);
        jb_add(&b, ":");
        switch (specs[i].kind) {
        case PARAM_STRING:
            jb_string(&b, value);
            break;
        case PARAM_INT:
            if (!normalized_int(value, &number)) {
                set_invalid_error(err, errlen, "invalid integer: %s", specs[i].input);
                return 0;
            }
            jb_add(&b, "%ld", number);
            break;
        case PARAM_BOOL:
            if (!strcmp(value, "1") || !strcmp(value, "true")) jb_add(&b, "true");
            else if (!strcmp(value, "0") || !strcmp(value, "false")) jb_add(&b, "false");
            else {
                set_invalid_error(err, errlen, "invalid boolean: %s", specs[i].input);
                return 0;
            }
            break;
        }
    }
    jb_add(&b, "}");
    if (b.len + 1 >= b.cap) {
        set_invalid_error(err, errlen, "control arguments too large");
        return 0;
    }
    return 1;
}

static int ubus_call(const char *service, const char *method, const char *args,
                     char *result, size_t result_len, char *err, size_t errlen)
{
    if (device_ubus_call(service, method, args, result, result_len) != 0) {
        snprintf(err, errlen, "%s.%s failed", service, method);
        return 0;
    }
    if (strstr(result, "\"error\"") || strstr(result, "Command failed")) {
        snprintf(err, errlen, "%s.%s returned an error", service, method);
        return 0;
    }
    return 1;
}

static int call_specs(const char *params, const char *service, const char *method,
                      const struct param_spec *specs, size_t count,
                      char *result, size_t result_len, char *err, size_t errlen)
{
    char args[CONTROL_UBUS_ARGS_MAX];
    if (!build_args(params, specs, count, args, sizeof args, err, errlen)) return 0;
    return ubus_call(service, method, args, result, result_len, err, errlen);
}

static int call_specs_require_any(const char *params, const char *service, const char *method,
                                  const struct param_spec *specs, size_t count,
                                  char *result, size_t result_len, char *err, size_t errlen)
{
    char args[CONTROL_UBUS_ARGS_MAX];
    if (!build_args(params, specs, count, args, sizeof args, err, errlen)) return 0;
    if (!strcmp(args, "{}")) {
        set_invalid_error(err, errlen, "no control fields supplied");
        return 0;
    }
    return ubus_call(service, method, args, result, result_len, err, errlen);
}

static int simple_fixed_call(const char *service, const char *method, const char *args,
                             char *result, size_t result_len, char *err, size_t errlen)
{
    return ubus_call(service, method, args, result, result_len, err, errlen);
}

static int control_login(const char *params, char *result, size_t result_len,
                         char *err, size_t errlen)
{
    char hash[129], args[256], session[256], upper[129];
    struct json_buf b = {args, sizeof args, 0};
    if (!param_value(params, "password_hash", hash, sizeof hash) || !valid_hex_hash(hash)) {
        set_invalid_error(err, errlen, "password_hash must be a 64 character SHA-256 hex value");
        return 0;
    }
    for (size_t i = 0; i <= strlen(hash); i++) upper[i] = (char)toupper((unsigned char)hash[i]);
    jb_add(&b, "{\"password\":"); jb_string(&b, upper); jb_add(&b, "}");
    if (!ubus_call("zwrt_web", "web_login", args, result, result_len, err, errlen)) return 0;
    if (json_get_int(result, "result", 0) != 0 ||
        !json_get(result, "ubus_rpc_session", session, sizeof session) || !session[0]) {
        snprintf(err, errlen, "device login rejected");
        return 0;
    }
    snprintf(g_device_password_hash, sizeof g_device_password_hash, "%s", upper);
    snprintf(g_device_session, sizeof g_device_session, "%s", session);
    return 1;
}

static int control_change_password(const char *params, char *result, size_t result_len,
                                   char *err, size_t errlen)
{
    char old_hash[129], new_hash[129], old_upper[129], new_upper[129];
    char args[512];
    struct json_buf b = {args, sizeof args, 0};
    if (!param_value(params, "old_hash", old_hash, sizeof old_hash) || !valid_hex_hash(old_hash) ||
        !param_value(params, "new_hash", new_hash, sizeof new_hash) || !valid_hex_hash(new_hash)) {
        set_invalid_error(err, errlen, "old_hash and new_hash must be SHA-256 hex values");
        return 0;
    }
    for (size_t i = 0; i <= strlen(old_hash); i++) old_upper[i] = (char)toupper((unsigned char)old_hash[i]);
    for (size_t i = 0; i <= strlen(new_hash); i++) new_upper[i] = (char)toupper((unsigned char)new_hash[i]);
    jb_add(&b, "{\"password_old\":"); jb_string(&b, old_upper);
    jb_add(&b, ",\"password_new\":"); jb_string(&b, new_upper); jb_add(&b, "}");
    if (!ubus_call("zwrt_web", "web_change_password", args, result, result_len, err, errlen)) return 0;
    snprintf(g_device_password_hash, sizeof g_device_password_hash, "%s", new_upper);
    g_device_session[0] = 0;
    return 1;
}

static int control_sim_slot(const char *params, char *result, size_t result_len,
                            char *err, size_t errlen)
{
    char raw[32], args[128], ignored[CONTROL_UBUS_RESPONSE_MAX];
    long slot, old_slot;
    if (!param_value(params, "slot", raw, sizeof raw) || !normalized_int(raw, &slot) || slot < 1 || slot > 2) {
        set_invalid_error(err, errlen, "slot must be 1 or 2");
        return 0;
    }
    old_slot = slot == 2 ? 1 : 2;
    snprintf(args, sizeof args, "{\"active_slot\":%ld,\"active_flag\":0}", old_slot);
    (void)device_ubus_call("zwrt_zte_mdm.api", "zwrt_mdm_change_provision_session",
                           args, ignored, sizeof ignored);
    snprintf(args, sizeof args, "{\"active_slot\":%ld,\"active_flag\":1}", slot);
    return ubus_call("zwrt_zte_mdm.api", "zwrt_mdm_change_provision_session",
                     args, result, result_len, err, errlen);
}

static int control_nfc(const char *params, char *result, size_t result_len,
                       char *err, size_t errlen)
{
    static const struct param_spec specs[] = {
        {"enabled", "switch", PARAM_INT, 1},
        {"flag", "flag", PARAM_INT, 0}
    };
    char ignored[CONTROL_UBUS_RESPONSE_MAX];
    if (!call_specs(params, "zwrt_nfc", "zwrt_nfc_wifi_set", specs,
                    sizeof specs / sizeof specs[0], result, result_len, err, errlen)) return 0;
    (void)device_ubus_call("zwrt_nfc", "zwrt_nfc_wifi_change", NULL,
                           ignored, sizeof ignored);
    return 1;
}

static int control_lan(const char *params, char *result, size_t result_len,
                       char *err, size_t errlen)
{
    static const struct param_spec specs[] = {
        {"ip", "ipaddr", PARAM_STRING, 0},
        {"netmask", "netmask", PARAM_STRING, 0},
        {"dhcp_disabled", "ignore", PARAM_INT, 0},
        {"dhcp_start", "zte_start", PARAM_STRING, 0},
        {"dhcp_end", "zte_end", PARAM_STRING, 0},
        {"lease_seconds", "leasetime", PARAM_STRING, 0}
    };
    if (!call_specs_require_any(params, "zwrt_router.api", "router_set_lan_para", specs,
                                sizeof specs / sizeof specs[0], result, result_len, err, errlen)) return 0;
    (void)device_uci_commit("network");
    (void)device_uci_commit("dhcp");
    return 1;
}

static int control_cell_unlock(char *result, size_t result_len, char *err, size_t errlen)
{
    char ignored[CONTROL_UBUS_RESPONSE_MAX];
    if (!ubus_call("zte_nwinfo_api", "nwinfo_lock_lte_cell",
                   "{\"lock_lte_pci\":\"0\",\"lock_lte_earfcn\":\"0\"}",
                   ignored, sizeof ignored, err, errlen)) return 0;
    return ubus_call("zte_nwinfo_api", "nwinfo_lock_nr_cell",
                     "{\"lock_nr_pci\":\"0\",\"lock_nr_earfcn\":\"0\",\"lock_nr_cell_band\":\"0\"}",
                     result, result_len, err, errlen);
}

static int control_wifi_configure(const char *params, char *result, size_t result_len,
                                  char *err, size_t errlen)
{
    static const char *allowed[] = {"main_2g", "main_5g", "guest_2g", "guest_5g"};
    static const struct { const char *input; const char *option; } fields[] = {
        {"ssid", "ssid"}, {"encryption", "encryption"}, {"key", "key"},
        {"pmf", "pmf"}, {"maxassoc", "maxassoc"}, {"hidden", "hidden"},
        {"isolate", "isolate"}
    };
    char section[64], path[128], values[sizeof fields / sizeof fields[0]][2048];
    char enabled_value[32];
    int present[sizeof fields / sizeof fields[0]];
    int enabled_present;
    long enabled = 0;
    int section_ok = 0, changed = 0;
    if (!param_value(params, "section", section, sizeof section)) {
        set_invalid_error(err, errlen, "missing parameter: section");
        return 0;
    }
    for (size_t i = 0; i < sizeof allowed / sizeof allowed[0]; i++)
        if (!strcmp(section, allowed[i])) section_ok = 1;
    if (!section_ok) {
        set_invalid_error(err, errlen, "unsupported wifi section");
        return 0;
    }

    /* Validate and copy every requested field before the first UCI mutation. */
    for (size_t i = 0; i < sizeof fields / sizeof fields[0]; i++) {
        present[i] = json_get(params, fields[i].input, values[i], sizeof values[i]);
        if (present[i]) changed = 1;
    }
    enabled_present = json_get(params, "enabled", enabled_value, sizeof enabled_value);
    if (enabled_present) {
        if (!normalized_int(enabled_value, &enabled) || (enabled != 0 && enabled != 1)) {
            set_invalid_error(err, errlen, "enabled must be 0 or 1");
            return 0;
        }
        changed = 1;
    }
    if (!changed) {
        set_invalid_error(err, errlen, "no wifi fields supplied");
        return 0;
    }

    for (size_t i = 0; i < sizeof fields / sizeof fields[0]; i++) {
        if (!present[i]) continue;
        snprintf(path, sizeof path, "wireless.%s.%s", section, fields[i].option);
        if (device_uci_set(path, values[i]) != 0) {
            (void)device_uci_revert("wireless");
            snprintf(err, errlen, "failed to set wifi option: %s", fields[i].input);
            return 0;
        }
    }
    if (enabled_present) {
        snprintf(path, sizeof path, "wireless.%s.disabled", section);
        if (device_uci_set(path, enabled ? "0" : "1") != 0) {
            (void)device_uci_revert("wireless");
            snprintf(err, errlen, "failed to set wifi enabled state");
            return 0;
        }
    }
    if (device_uci_commit("wireless") != 0) {
        (void)device_uci_revert("wireless");
        snprintf(err, errlen, "failed to commit wifi configuration");
        return 0;
    }
    if (device_wifi_reload() != 0) {
        snprintf(err, errlen, "wifi configuration committed but reload failed");
        return 0;
    }
    snprintf(result, result_len, "{\"section\":\"%s\"}", section);
    return 1;
}

static int control_client_access(const char *action, const char *params,
                                 char *result, size_t result_len,
                                 char *err, size_t errlen)
{
    static const char *sections[] = {"main_2g", "main_5g", "guest_2g", "guest_5g"};
    char mac[32], path[128], kick_args[128], ignored[CONTROL_UBUS_RESPONSE_MAX];
    int block = !strcmp(action, "client.block");
    if (!param_value(params, "mac", mac, sizeof mac) || !valid_mac(mac)) {
        set_invalid_error(err, errlen, "invalid mac address");
        return 0;
    }
    for (char *p = mac; *p; p++) *p = (char)tolower((unsigned char)*p);
    for (size_t i = 0; i < sizeof sections / sizeof sections[0]; i++) {
        snprintf(path, sizeof path, "wireless.%s.macfilter", sections[i]);
        if (device_uci_set(path, "deny") != 0) {
            snprintf(err, errlen, "failed to set wifi macfilter");
            return 0;
        }
        snprintf(path, sizeof path, "wireless.%s.denymaclist", sections[i]);
        if (block) {
            (void)device_uci_list("del_list", path, mac);
            if (device_uci_list("add_list", path, mac) != 0) {
                snprintf(err, errlen, "failed to add denied mac");
                return 0;
            }
        } else {
            (void)device_uci_list("del_list", path, mac);
        }
    }
    if (device_uci_commit("wireless") != 0 || device_wifi_reload() != 0) {
        snprintf(err, errlen, "failed to commit client access policy");
        return 0;
    }
    if (block) {
        snprintf(kick_args, sizeof kick_args, "{\"macs\":\"%s\"}", mac);
        (void)device_ubus_call("zwrt_wlan", "kick_macs", kick_args, ignored, sizeof ignored);
    }
    snprintf(result, result_len, "{\"mac\":\"%s\",\"blocked\":%s}",
             mac, block ? "true" : "false");
    return 1;
}

static int control_apn_list(char *result, size_t result_len, char *err, size_t errlen)
{
    char mode[4096], automatic[CONTROL_UBUS_RESPONSE_MAX], manual[CONTROL_UBUS_RESPONSE_MAX];
    char enabled[4096];
    struct json_buf b = {result, result_len, 0};
    if (!ubus_call("zwrt_apn_object", "get_apn_mode", "{}", mode, sizeof mode, err, errlen) ||
        !ubus_call("zwrt_apn_object", "getAutoApnList", "{}", automatic, sizeof automatic, err, errlen) ||
        !ubus_call("zwrt_apn_object", "getManuApnList", "{}", manual, sizeof manual, err, errlen) ||
        !ubus_call("zwrt_apn_object", "get_enabled_manu_apn_id", "{}", enabled, sizeof enabled, err, errlen))
        return 0;
    result[0] = 0;
    jb_add(&b, "{\"mode\":%s,\"automatic\":%s,\"manual\":%s,\"enabled\":%s}",
           mode, automatic, manual, enabled);
    return b.len + 1 < b.cap;
}

static int control_usb_status(char *result, size_t result_len, char *err, size_t errlen)
{
    char typec[4096], usb[4096];
    struct json_buf b = {result, result_len, 0};
    if (!ubus_call("zwrt_bsp.typec", "list", "{}", typec, sizeof typec, err, errlen) ||
        !ubus_call("zwrt_bsp.usb", "list", "{}", usb, sizeof usb, err, errlen)) return 0;
    result[0] = 0;
    jb_add(&b, "{\"typec\":%s,\"usb\":%s}", typec, usb);
    return b.len + 1 < b.cap;
}

static void uci_json_field(struct json_buf *b, const char *key, const char *path)
{
    char value[2048];
    jb_string(b, key); jb_add(b, ":");
    if (device_uci_get(path, value, sizeof value) == 0) jb_string(b, value);
    else jb_string(b, "");
}

static int control_wifi_status(char *result, size_t result_len)
{
    static const char *sections[] = {"main_2g", "main_5g"};
    struct json_buf b = {result, result_len, 0};
    result[0] = 0;
    jb_add(&b, "{");
    for (size_t i = 0; i < sizeof sections / sizeof sections[0]; i++) {
        char path[128];
        if (i) jb_add(&b, ",");
        jb_string(&b, sections[i]); jb_add(&b, ":{");
        snprintf(path, sizeof path, "wireless.%s.ssid", sections[i]);
        uci_json_field(&b, "ssid", path); jb_add(&b, ",");
        snprintf(path, sizeof path, "wireless.%s.key", sections[i]);
        uci_json_field(&b, "key", path); jb_add(&b, ",");
        snprintf(path, sizeof path, "wireless.%s.encryption", sections[i]);
        uci_json_field(&b, "encryption", path); jb_add(&b, ",");
        snprintf(path, sizeof path, "wireless.%s.disabled", sections[i]);
        uci_json_field(&b, "disabled", path);
        jb_add(&b, "}");
    }
    jb_add(&b, "}");
    return b.len + 1 < b.cap;
}

static int control_sleep_status(char *result, size_t result_len)
{
    struct json_buf b = {result, result_len, 0};
    result[0] = 0;
    jb_add(&b, "{");
    uci_json_field(&b, "idle_seconds", "zwrt_sleep.ztmp_time.SysIdTime"); jb_add(&b, ",");
    uci_json_field(&b, "enabled", "zwrt_sleep.ztmp_switch.sleepSwitch"); jb_add(&b, ",");
    uci_json_field(&b, "wakeup", "zwrt_sleep.ztmp_switch.wakeupSwitch"); jb_add(&b, ",");
    uci_json_field(&b, "status", "zwrt_sleep.ztmp_status.sleepStatus");
    jb_add(&b, "}");
    return b.len + 1 < b.cap;
}

static int control_client_access_status(char *result, size_t result_len,
                                        char *err, size_t errlen)
{
    char config[CONTROL_UBUS_RESPONSE_MAX], lan[CONTROL_UBUS_RESPONSE_MAX];
    char wifi[CONTROL_UBUS_RESPONSE_MAX];
    struct json_buf b = {result, result_len, 0};
    if (!ubus_call("uci", "get", "{\"config\":\"wireless\",\"section\":\"main_2g\"}",
                   config, sizeof config, err, errlen) ||
        !ubus_call("zwrt_router.api", "router_lan_access_list", "{\"start_id\":1,\"end_id\":64}",
                   lan, sizeof lan, err, errlen) ||
        !ubus_call("zwrt_router.api", "router_wireless_access_list", "{\"start_id\":1,\"end_id\":64}",
                   wifi, sizeof wifi, err, errlen)) return 0;
    result[0] = 0;
    jb_add(&b, "{\"policy\":%s,\"lan\":%s,\"wifi\":%s}", config, lan, wifi);
    return b.len + 1 < b.cap;
}

static int control_band(const char *action, const char *params,
                        char *result, size_t result_len, char *err, size_t errlen)
{
    char bands[1024], args[1280];
    struct json_buf b = {args, sizeof args, 0};
    if (!param_value(params, "bands", bands, sizeof bands)) bands[0] = 0;
    if (!valid_band_list(bands)) {
        set_invalid_error(err, errlen, "bands must contain only numbers and commas");
        return 0;
    }
    if (!strcmp(action, "band.set_lte")) {
        jb_add(&b, "{\"lte_band\":"); jb_string(&b, bands); jb_add(&b, "}");
        return ubus_call("zte_nwinfo_api", "nwinfo_set_lte_ext_band", args,
                         result, result_len, err, errlen);
    }
    jb_add(&b, "{\"nr5g_type\":");
    jb_string(&b, !strcmp(action, "band.set_nr_nsa") ? "1" : "0");
    jb_add(&b, ",\"nr5g_band\":"); jb_string(&b, bands); jb_add(&b, "}");
    return ubus_call("zte_nwinfo_api", "nwinfo_set_nrbandlock", args,
                     result, result_len, err, errlen);
}

static int clear_qos_logs(char *result, size_t result_len, char *err, size_t errlen)
{
    static const char *paths[] = {"/data/logfs/key.log", "/data/logfs/key.log.0"};
    int cleared = 0;
    for (size_t i = 0; i < sizeof paths / sizeof paths[0]; i++) {
        if (truncate(paths[i], 0) == 0) {
            cleared++;
            continue;
        }
        if (errno == ENOENT) continue;
        snprintf(err, errlen, "failed to clear %s: %s", paths[i], strerror(errno));
        return 0;
    }
    raise(SIGUSR1);
    snprintf(result, result_len, "{\"cleared\":true,\"files\":%d}", cleared);
    return 1;
}

const char *control_capabilities_json(void)
{
    return
        "{\"schema_version\":1,\"control\":["
        "\"device.login\",\"device.logout\",\"device.change_password\","
        "\"device.reboot\",\"device.poweroff\","
        "\"cellular.connect\",\"cellular.disconnect\",\"cellular.set\","
        "\"network.set_mode\",\"band.set_lte\",\"band.set_nr_sa\",\"band.set_nr_nsa\","
        "\"cell.lock_lte\",\"cell.lock_nr\",\"cell.unlock_all\",\"sim.set_slot\","
        "\"wifi.status\",\"wifi.set_module\",\"wifi.set_chip\",\"wifi.configure\",\"lan.set\",\"dns.set\","
        "\"usb.status\",\"usb.set\",\"sleep.status\",\"sleep.set\",\"nfc.set\","
        "\"apn.list\",\"apn.set_mode\",\"apn.add\",\"apn.modify\",\"apn.delete\",\"apn.enable\","
        "\"traffic.set_limit\",\"traffic.set_clear_day\",\"traffic.calibrate\","
        "\"sms.send_raw\",\"sms.delete\",\"sms.mark_read\","
        "\"client.access\",\"client.block\",\"client.unblock\",\"client.kick\",\"client.rename\","
        "\"state.refresh\",\"qos.reload\",\"qos.clear\"],"
        "\"events\":[\"state\"],\"transport\":[\"http\",\"sse\"]}";
}

struct control_result control_execute(const char *request_json,
                                      char *response, size_t response_len)
{
    struct control_result status = {200, 1};
    char action[128], params[CONTROL_PARAMS_MAX], result[CONTROL_UBUS_RESPONSE_MAX], err[256];
    int ok = 0;

    if (!request_json || !json_is_valid_object(request_json)) {
        write_error(response, response_len, "", "invalid_request", "request body must be a complete JSON object");
        status.http_status = 400;
        status.refresh_state = 0;
        return status;
    }
    if (!json_get(request_json, "action", action, sizeof action) || !action[0]) {
        write_error(response, response_len, "", "invalid_request", "missing action");
        status.http_status = 400;
        status.refresh_state = 0;
        return status;
    }
    if (!json_get(request_json, "params", params, sizeof params)) {
        snprintf(params, sizeof params, "{}");
    } else if (!json_is_valid_object(params)) {
        write_error(response, response_len, action, "invalid_request", "params must be a JSON object");
        status.http_status = 400;
        status.refresh_state = 0;
        return status;
    }
    result[0] = err[0] = 0;

    if (!strcmp(action, "device.login")) {
        ok = control_login(params, result, sizeof result, err, sizeof err);
        status.refresh_state = 0;
    } else if (!strcmp(action, "device.logout")) {
        g_device_session[0] = g_device_password_hash[0] = 0;
        snprintf(result, sizeof result, "{\"logged_in\":false}");
        ok = 1;
        status.refresh_state = 0;
    } else if (!strcmp(action, "device.change_password")) {
        ok = control_change_password(params, result, sizeof result, err, sizeof err);
        status.refresh_state = 0;
    } else if (!strcmp(action, "device.reboot")) {
        ok = simple_fixed_call("zwrt_mc.device.manager", "device_reboot",
                               "{\"moduleName\":\"web\"}", result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "device.poweroff")) {
        ok = simple_fixed_call("zwrt_mc.device.manager", "device_poweroff",
                               "{\"moduleName\":\"web\"}", result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "cellular.connect") || !strcmp(action, "cellular.disconnect")) {
        const char *args = !strcmp(action, "cellular.connect")
            ? "{\"enable\":1,\"source_module\":\"WEBUI\",\"cid\":1}"
            : "{\"enable\":0,\"source_module\":\"WEBUI\",\"cid\":1}";
        ok = simple_fixed_call("zwrt_data", "set_wwaniface", args,
                               result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "cellular.set")) {
        static const struct param_spec specs[] = {
            {"enabled", "enable", PARAM_INT, 0},
            {"roaming", "roam_enable", PARAM_INT, 0},
            {"connect_mode", "connect_mode", PARAM_STRING, 0}
        };
        char args[CONTROL_UBUS_RESPONSE_MAX], current[CONTROL_UBUS_RESPONSE_MAX];
        char partial[CONTROL_UBUS_ARGS_MAX], overrides[CONTROL_UBUS_ARGS_MAX];
        struct json_buf b = {overrides, sizeof overrides, 0};
        if (build_args(params, specs, sizeof specs / sizeof specs[0], partial, sizeof partial, err, sizeof err)) {
            if (!strcmp(partial, "{}")) {
                set_invalid_error(err, sizeof err, "no cellular fields supplied");
            } else {
                size_t n = strlen(partial);
                if (n > 0 && partial[n - 1] == '}') partial[n - 1] = 0;
                jb_add(&b, "%s,\"source_module\":\"WEBUI\",\"cid\":1}", partial);
                if (ubus_call("zwrt_data", "get_wwaniface",
                              "{\"source_module\":\"web\",\"cid\":1,\"connect_status\":\"\"}",
                              current, sizeof current, err, sizeof err) &&
                    json_merge_objects(current, overrides, args, sizeof args)) {
                    ok = ubus_call("zwrt_data", "set_wwaniface", args,
                                   result, sizeof result, err, sizeof err);
                } else if (!err[0]) {
                    snprintf(err, sizeof err, "invalid get_wwaniface response");
                }
            }
        }
    } else if (!strcmp(action, "network.set_mode")) {
        static const struct param_spec specs[] = {{"mode", "net_select", PARAM_STRING, 1}};
        ok = call_specs(params, "zte_nwinfo_api", "nwinfo_set_netselect", specs, 1,
                        result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "band.set_lte") || !strcmp(action, "band.set_nr_sa") ||
               !strcmp(action, "band.set_nr_nsa")) {
        ok = control_band(action, params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "cell.lock_lte")) {
        static const struct param_spec specs[] = {
            {"pci", "lock_lte_pci", PARAM_STRING, 1},
            {"earfcn", "lock_lte_earfcn", PARAM_STRING, 1}
        };
        ok = call_specs(params, "zte_nwinfo_api", "nwinfo_lock_lte_cell", specs, 2,
                        result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "cell.lock_nr")) {
        static const struct param_spec specs[] = {
            {"pci", "lock_nr_pci", PARAM_STRING, 1},
            {"arfcn", "lock_nr_earfcn", PARAM_STRING, 1},
            {"band", "lock_nr_cell_band", PARAM_STRING, 1}
        };
        ok = call_specs(params, "zte_nwinfo_api", "nwinfo_lock_nr_cell", specs, 3,
                        result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "cell.unlock_all")) {
        ok = control_cell_unlock(result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "sim.set_slot")) {
        ok = control_sim_slot(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "wifi.status")) {
        ok = control_wifi_status(result, sizeof result);
        if (!ok) snprintf(err, sizeof err, "wifi status response too large");
        status.refresh_state = 0;
    } else if (!strcmp(action, "wifi.set_module")) {
        static const struct param_spec specs[] = {{"enabled", "SwitchOption", PARAM_INT, 1}};
        ok = call_specs(params, "zwrt_wlan", "set", specs, 1,
                        result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "wifi.set_chip")) {
        static const struct param_spec specs[] = {
            {"chip", "ChipEnum", PARAM_STRING, 1},
            {"guest_enabled", "GuestEnable", PARAM_INT, 0}
        };
        ok = call_specs(params, "zwrt_wlan", "set", specs, 2,
                        result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "wifi.configure")) {
        ok = control_wifi_configure(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "lan.set")) {
        ok = control_lan(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "dns.set")) {
        static const struct param_spec specs[] = {
            {"primary", "dns1", PARAM_STRING, 0},
            {"secondary", "dns2", PARAM_STRING, 0},
            {"manual_ipv4", "lan_dns_manual_enable", PARAM_INT, 0},
            {"manual_ipv6", "lan_dns_manual_enable_v6", PARAM_INT, 0}
        };
        ok = call_specs_require_any(params, "zwrt_router.api", "router_set_lan_dns", specs, 4,
                                    result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "usb.set")) {
        static const struct param_spec specs[] = {
            {"mode", "mode", PARAM_STRING, 0},
            {"port_switch", "usb_port_switch", PARAM_STRING, 0},
            {"network_protocol", "usb_network_protocal", PARAM_STRING, 0}
        };
        ok = call_specs_require_any(params, "zwrt_bsp.usb", "set", specs, 3,
                                    result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "usb.status")) {
        ok = control_usb_status(result, sizeof result, err, sizeof err);
        status.refresh_state = 0;
    } else if (!strcmp(action, "sleep.status")) {
        ok = control_sleep_status(result, sizeof result);
        if (!ok) snprintf(err, sizeof err, "sleep status response too large");
        status.refresh_state = 0;
    } else if (!strcmp(action, "sleep.set")) {
        static const struct param_spec specs[] = {{"seconds", "ufiSleepTime", PARAM_STRING, 1}};
        ok = call_specs(params, "zwrt_zte_sleep_faw.wakelock", "set_ufi_sleep", specs, 1,
                        result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "nfc.set")) {
        ok = control_nfc(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "apn.list")) {
        ok = control_apn_list(result, sizeof result, err, sizeof err);
        status.refresh_state = 0;
    } else if (!strcmp(action, "apn.set_mode")) {
        static const struct param_spec specs[] = {{"mode", "apn_mode", PARAM_INT, 1}};
        ok = call_specs(params, "zwrt_apn_object", "set_apn_mode", specs, 1,
                        result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "apn.add") || !strcmp(action, "apn.modify")) {
        static const struct param_spec specs[] = {
            {"profile_id", "profileId", PARAM_STRING, 0},
            {"name", "profilename", PARAM_STRING, 1},
            {"apn", "wanapn", PARAM_STRING, 1},
            {"username", "username", PARAM_STRING, 0},
            {"password", "password", PARAM_STRING, 0},
            {"auth_mode", "pppAuthMode", PARAM_INT, 0},
            {"pdp_type", "pdpType", PARAM_INT, 0},
            {"roaming_pdp_type", "roamingPdpType", PARAM_INT, 0}
        };
        const char *method = !strcmp(action, "apn.add") ? "add_manu_apn" : "modify_manu_apn";
        char profile_id[128];
        if (!strcmp(action, "apn.modify") &&
            !param_value(params, "profile_id", profile_id, sizeof profile_id)) {
            set_invalid_error(err, sizeof err, "missing parameter: profile_id");
        } else {
            ok = call_specs(params, "zwrt_apn_object", method, specs,
                            sizeof specs / sizeof specs[0], result, sizeof result, err, sizeof err);
        }
    } else if (!strcmp(action, "apn.delete") || !strcmp(action, "apn.enable")) {
        static const struct param_spec specs[] = {{"profile_id", "profileId", PARAM_STRING, 1}};
        const char *method = !strcmp(action, "apn.delete") ? "delete_manu_apn" : "enable_manu_apn_id";
        ok = call_specs(params, "zwrt_apn_object", method, specs, 1,
                        result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "traffic.set_limit")) {
        static const struct param_spec specs[] = {
            {"enabled", "enable", PARAM_INT, 1},
            {"value", "value", PARAM_STRING, 0},
            {"type", "type", PARAM_INT, 0},
            {"ratio", "ratio", PARAM_INT, 0}
        };
        char args[CONTROL_UBUS_ARGS_MAX], partial[CONTROL_UBUS_ARGS_MAX];
        struct json_buf b = {args, sizeof args, 0};
        if (build_args(params, specs, 4, partial, sizeof partial, err, sizeof err)) {
            size_t n = strlen(partial); if (n && partial[n - 1] == '}') partial[n - 1] = 0;
            jb_add(&b, "%s%s\"source_module\":\"web\",\"cid\":1}", partial,
                   strlen(partial) > 1 ? "," : "");
            ok = ubus_call("zwrt_data", "set_wwandst_monthlimit", args,
                           result, sizeof result, err, sizeof err);
        }
    } else if (!strcmp(action, "traffic.set_clear_day")) {
        static const struct param_spec specs[] = {{"day", "clearday", PARAM_INT, 1}};
        char args[CONTROL_UBUS_ARGS_MAX], partial[CONTROL_UBUS_ARGS_MAX];
        struct json_buf b = {args, sizeof args, 0};
        if (build_args(params, specs, 1, partial, sizeof partial, err, sizeof err)) {
            size_t n = strlen(partial); if (n && partial[n - 1] == '}') partial[n - 1] = 0;
            jb_add(&b, "%s,\"enable\":1,\"source_module\":\"web\",\"cid\":1}", partial);
            ok = ubus_call("zwrt_data", "set_wwandst_clearday", args,
                           result, sizeof result, err, sizeof err);
        }
    } else if (!strcmp(action, "traffic.calibrate")) {
        static const struct param_spec specs[] = {{"value", "value", PARAM_STRING, 1}};
        char args[CONTROL_UBUS_ARGS_MAX], partial[CONTROL_UBUS_ARGS_MAX];
        struct json_buf b = {args, sizeof args, 0};
        if (build_args(params, specs, 1, partial, sizeof partial, err, sizeof err)) {
            size_t n = strlen(partial); if (n && partial[n - 1] == '}') partial[n - 1] = 0;
            jb_add(&b, "%s,\"type\":2,\"source_module\":\"web\",\"cid\":1}", partial);
            ok = ubus_call("zwrt_data", "set_wwandst_calibmonth", args,
                           result, sizeof result, err, sizeof err);
        }
    } else if (!strcmp(action, "sms.send_raw")) {
        static const struct param_spec specs[] = {
            {"number", "number", PARAM_STRING, 1},
            {"message_hex", "message_body", PARAM_STRING, 1},
            {"sms_time", "sms_time", PARAM_STRING, 1}
        };
        char args[CONTROL_UBUS_ARGS_MAX], partial[CONTROL_UBUS_ARGS_MAX];
        struct json_buf b = {args, sizeof args, 0};
        if (build_args(params, specs, 3, partial, sizeof partial, err, sizeof err)) {
            size_t n = strlen(partial); if (n && partial[n - 1] == '}') partial[n - 1] = 0;
            jb_add(&b, "%s,\"encode_type\":\"UNICODE\",\"id\":\"-1\"}", partial);
            ok = ubus_call("zwrt_wms", "zte_libwms_send_sms", args,
                           result, sizeof result, err, sizeof err);
        }
    } else if (!strcmp(action, "sms.delete")) {
        static const struct param_spec specs[] = {{"ids", "id", PARAM_STRING, 1}};
        ok = call_specs(params, "zwrt_wms", "zwrt_wms_delete_sms", specs, 1,
                        result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "sms.mark_read")) {
        static const struct param_spec specs[] = {
            {"ids", "id", PARAM_STRING, 1},
            {"tag", "tag", PARAM_INT, 0}
        };
        ok = call_specs(params, "zwrt_wms", "zwrt_wms_modify_tag", specs, 2,
                        result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "client.access")) {
        ok = control_client_access_status(result, sizeof result, err, sizeof err);
        status.refresh_state = 0;
    } else if (!strcmp(action, "client.kick")) {
        static const struct param_spec specs[] = {{"macs", "macs", PARAM_STRING, 1}};
        ok = call_specs(params, "zwrt_wlan", "kick_macs", specs, 1,
                        result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "client.block") || !strcmp(action, "client.unblock")) {
        ok = control_client_access(action, params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "client.rename")) {
        char mac[32];
        static const struct param_spec specs[] = {
            {"mac", "mac", PARAM_STRING, 1},
            {"hostname", "hostname", PARAM_STRING, 1}
        };
        if (!param_value(params, "mac", mac, sizeof mac) || !valid_mac(mac)) {
            set_invalid_error(err, sizeof err, "invalid mac address");
        } else {
            ok = call_specs(params, "zwrt_router.api", "router_modify_lan_hostname", specs, 2,
                            result, sizeof result, err, sizeof err);
        }
    } else if (!strcmp(action, "state.refresh")) {
        snprintf(result, sizeof result, "{\"queued\":true}");
        ok = 1;
    } else if (!strcmp(action, "qos.reload")) {
        raise(SIGUSR1);
        snprintf(result, sizeof result, "{\"queued\":true}");
        ok = 1;
    } else if (!strcmp(action, "qos.clear")) {
        ok = clear_qos_logs(result, sizeof result, err, sizeof err);
    } else {
        write_error(response, response_len, action, "unknown_action", "unsupported control action");
        status.http_status = 404;
        status.refresh_state = 0;
        return status;
    }

    if (!ok) {
        const int invalid = err[0] == '\1';
        const char *message = invalid ? err + 1 : (err[0] ? err : "device call failed");
        write_error(response, response_len, action,
                    invalid ? "invalid_parameter" : "device_call_failed", message);
        status.http_status = invalid ? 400 : 502;
        status.refresh_state = 0;
        return status;
    }
    write_success(response, response_len, action, result);
    return status;
}
