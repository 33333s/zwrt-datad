/* SPDX-License-Identifier: MIT
 * MU5252 wireless policy. Match UCI identities to live SSID + band before
 * touching an interface: the vendor renumbers wlan interfaces on reload.
 * No hotplug scripts, recursive reloads, or periodic enforcement against
 * other owners. A policy applies once per interface generation or edit.
 */
#include "wifi_control.h"
#include "device_exec.h"
#include "json.h"
#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#define AP_COUNT 6
struct radio {
    const char *section, *band;
    int enabled, dbm, requested, percent, channel;
};
struct iface {
    const char *section;
    char ssid[129], configured[32], encryption[64], psm[16], key[128];
    int band, enabled, hidden, isolate, has_key, live, exists;
};
struct live {
    char name[32], ssid[129], type[16], phy[24];
    int index, freq, ready, psm;
    double dbm;
};
struct snapshot {
    struct radio r[2];
    struct iface ap[AP_COUNT];
    struct live live[8];
    int count;
};
struct applied {
    int index, dbm, psm, attempts;
    unsigned long power_revision, attempt_revision;
    char ssid[129];
};
static struct applied applied[AP_COUNT];
static time_t next_tick;
static int force_tick;

struct buf {
    char *s;
    size_t size, used;
    int failed;
};
static void add(struct buf *b, const char *fmt, ...)
{
    if (b->failed)
        return;
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(b->s + b->used, b->size - b->used, fmt, ap);
    va_end(ap);
    if (n < 0 || (size_t)n >= b->size - b->used)
        b->failed = 1;
    else
        b->used += (size_t)n;
}
static void quote(struct buf *b, const char *s)
{
    add(b, "\"");
    for (const unsigned char *p = (const unsigned char *)s; *p; p++) {
        if (*p == '"' || *p == '\\')
            add(b, "\\%c", *p);
        else if (*p < 32)
            add(b, "\\u%04x", *p);
        else
            add(b, "%c", *p);
    }
    add(b, "\"");
}
static int getval(const char *obj, const char *key, char *out, size_t len)
{
    out[0] = 0;
    return json_get(obj, key, out, len);
}
static int cfgint(const char *obj, const char *key, int def)
{
    return (int)json_get_int(obj, key, def);
}
static int model_supported(void)
{
    char model[64];
    return device_uci_get("zwrt_common_info.common_config.model_name", model, sizeof model) == 0 &&
           !strcmp(model, "MU5252");
}
static int iw(const char *name, const char *verb, const char *option, const char *value, char *out,
              size_t len)
{
    const char *argv[] = {"iw", "dev", name, verb, option, value, NULL};
    return device_run_capture(argv, out, len);
}
static const char *runtime_dir(void);
static int ready(const char *name)
{
    char out[2048];
    const char *ctrl = (!strcmp(name, "wlan4") || !strcmp(name, "wlan5"))
                           ? runtime_dir()
                           : "/data/vendor/wifi/hostapd";
    const char *argv[] = {"hostapd_cli", "-p", ctrl, "-i", name, "status", NULL};
    return device_run_capture(argv, out, sizeof out) == 0 &&
           (strstr(out, "state=ENABLED\n") || strstr(out, "state=ENABLED\r"));
}
static void runtime(struct snapshot *s, int details)
{
    char output[8192], *save = NULL;
    const char *argv[] = {"iw", "dev", NULL};
    if (device_run_capture(argv, output, sizeof output) != 0)
        return;
    struct live *p = NULL;
    char phy[24] = "";
    for (char *line = strtok_r(output, "\n", &save); line; line = strtok_r(NULL, "\n", &save)) {
        while (isspace((unsigned char)*line))
            line++;
        unsigned number;
        if (sscanf(line, "phy#%u", &number) == 1)
            snprintf(phy, sizeof phy, "phy%u", number);
        else if (!strncmp(line, "Interface ", 10) && s->count < 8) {
            p = &s->live[s->count++];
            snprintf(p->name, sizeof p->name, "%s", line + 10);
            snprintf(p->phy, sizeof p->phy, "%s", phy);
            p->psm = -1;
            p->dbm = -1;
        } else if (p) {
            if (!strncmp(line, "ssid ", 5))
                snprintf(p->ssid, sizeof p->ssid, "%s", line + 5);
            else if (sscanf(line, "ifindex %u", &number) == 1)
                p->index = (int)number;
            else if (!strncmp(line, "type ", 5))
                snprintf(p->type, sizeof p->type, "%s", line + 5);
            else if (!strncmp(line, "channel ", 8)) {
                char *f = strchr(line, '(');
                if (f)
                    p->freq = atoi(f + 1);
            } else if (!strncmp(line, "txpower ", 8))
                p->dbm = strtod(line + 8, NULL);
        }
    }
    for (int i = 0; i < AP_COUNT; i++) {
        struct iface *a = &s->ap[i];
        int matches = 0, candidate = -1, exact = -1;
        a->live = -1;
        for (int j = 0; j < s->count; j++) {
            p = &s->live[j];
            if (strcmp(p->type, "AP") || strcmp(p->ssid, a->ssid) ||
                (p->freq >= 4900 ? 1 : 0) != a->band || p->freq == 0)
                continue;
            candidate = j;
            matches++;
            if (!strcmp(p->name, a->configured))
                exact = j;
        }
        if (exact >= 0)
            a->live = exact;
        else if (matches == 1)
            a->live = candidate;
    }
    /* Never assign one runtime interface to two configuration identities. */
    for (int i = 0; i < AP_COUNT; i++)
        for (int j = i + 1; j < AP_COUNT; j++)
            if (s->ap[i].live >= 0 && s->ap[i].live == s->ap[j].live)
                s->ap[i].live = s->ap[j].live = -1;
    if (details)
        for (int i = 0; i < s->count; i++) {
            p = &s->live[i];
            char value[128];
            p->ready = ready(p->name);
            if (iw(p->name, "get", "power_save", NULL, value, sizeof value) == 0) {
                if (strstr(value, "Power save: on"))
                    p->psm = 1;
                else if (strstr(value, "Power save: off"))
                    p->psm = 0;
            }
        }
}
static int snapshot(struct snapshot *s, int details)
{
    char full[32768], values[32768], obj[4096], key[128];
    memset(s, 0, sizeof *s);
    if (device_ubus_call("uci", "get", "{\"config\":\"wireless\"}", full, sizeof full) != 0 ||
        !json_is_valid_object(full) || !json_get(full, "values", values, sizeof values))
        return 0;
    for (int i = 0; i < 2; i++) {
        struct radio *r = &s->r[i];
        r->section = i ? "wifi1" : "wifi0";
        r->band = i ? "5g" : "2g";
        if (!json_get(values, r->section, obj, sizeof obj))
            return 0;
        r->enabled = !cfgint(obj, "disabled", 0);
        r->channel = cfgint(obj, "channel", 0);
        r->percent = cfgint(obj, "txpowerpercent", 100);
        r->dbm = cfgint(obj, "txpower", -1);
        r->requested = cfgint(obj, "datad_txpower_dbm", -1);
    }
    static const char *sections[] = {"main_2g", "guest_2g", "main_5g", "guest_5g"};
    for (int i = 0; i < 4; i++) {
        struct iface *a = &s->ap[i];
        a->section = sections[i];
        a->band = i / 2;
        a->live = -1;
        a->exists = 1;
        if (!json_get(values, a->section, obj, sizeof obj))
            return 0;
        a->enabled = !cfgint(obj, "disabled", 0);
        a->hidden = cfgint(obj, "hidden", 0);
        a->isolate = cfgint(obj, "isolate", 0);
        getval(obj, "ssid", a->ssid, sizeof a->ssid);
        getval(obj, "ifname", a->configured, sizeof a->configured);
        getval(obj, "encryption", a->encryption, sizeof a->encryption);
        getval(obj, "datad_psm", a->psm, sizeof a->psm);
        a->has_key = getval(obj, "key", key, sizeof key) && key[0];
        memset(key, 0, sizeof key);
    }
    values[0] = 0;
    if (device_ubus_call("uci", "get", "{\"config\":\"datad_wifi\"}", full, sizeof full) == 0 &&
        json_is_valid_object(full))
        (void)getval(full, "values", values, sizeof values);
    for (int i = 4; i < AP_COUNT; i++) {
        struct iface *a = &s->ap[i];
        a->section = i == 4 ? "datad_ssid_1" : "datad_ssid_2";
        a->live = -1;
        snprintf(a->configured, sizeof a->configured, "wlan%d", i);
        if (!values[0] || !json_get(values, a->section, obj, sizeof obj))
            continue;
        a->exists = 1;
        a->enabled = !cfgint(obj, "disabled", 0);
        a->hidden = cfgint(obj, "hidden", 0);
        a->isolate = cfgint(obj, "isolate", 0);
        getval(obj, "band", key, sizeof key);
        a->band = !strcmp(key, "5g");
        getval(obj, "ssid", a->ssid, sizeof a->ssid);
        getval(obj, "encryption", a->encryption, sizeof a->encryption);
        getval(obj, "datad_psm", a->psm, sizeof a->psm);
        a->has_key = getval(obj, "key", a->key, sizeof a->key) && a->key[0];
    }
    memset(full, 0, sizeof full);
    memset(values, 0, sizeof values);
    memset(obj, 0, sizeof obj);
    runtime(s, details);
    return 1;
}
static int regulatory_limit(struct snapshot *s, int band)
{
    struct live *p = NULL;
    for (int i = 0; i < AP_COUNT; i++)
        if (s->ap[i].band == band && s->ap[i].live >= 0) {
            p = &s->live[s->ap[i].live];
            break;
        }
    if (!p)
        return -1;
    char output[32768], *save = NULL;
    const char *argv[] = {"iw", "phy", p->phy, "info", NULL};
    if (device_run_capture(argv, output, sizeof output) != 0)
        return -1;
    for (char *line = strtok_r(output, "\n", &save); line; line = strtok_r(NULL, "\n", &save)) {
        int freq, chan;
        double dbm;
        while (isspace((unsigned char)*line))
            line++;
        if (sscanf(line, "* %d MHz [%d] (%lf dBm)", &freq, &chan, &dbm) == 3 && freq == p->freq &&
            !strstr(line, "disabled"))
            return (int)dbm;
    }
    return -1;
}
/* Separate configuration keeps the OEM QCMAP loader's fixed four-AP table
 * untouched. Only wlan4/wlan5 and our own pid/config files are managed here. */
static char extra_error[2][160];
static int extra_base_index[2], extra_attempts[2];
static time_t extra_started[2];
static const char *runtime_dir(void)
{
    const char *env = getenv("ZWRT_DATAD_WIFI_RUNTIME_DIR");
    return env && *env ? env : "/data/zwrt-datad/wifi";
}
static void extra_path(char *out, size_t len, const struct iface *a, const char *suffix)
{
    snprintf(out, len, "%s/%s.%s", runtime_dir(), a->section, suffix);
}
static int process_owned(const struct iface *a, pid_t *pid)
{
    char path[512], proc[64], command[4096];
    long value;
    FILE *fp;
    extra_path(path, sizeof path, a, "pid");
    fp = fopen(path, "r");
    if (!fp)
        return 0;
    int parsed = fscanf(fp, "%ld", &value);
    fclose(fp);
    if (parsed != 1 || value <= 1 || value > INT_MAX)
        return 0;
    snprintf(proc, sizeof proc, "/proc/%ld/cmdline", value);
    fp = fopen(proc, "r");
    if (!fp)
        return 0;
    size_t n = fread(command, 1, sizeof command - 1, fp);
    fclose(fp);
    command[n] = 0;
    for (size_t i = 0; i < n; i++)
        if (command[i] == 0)
            command[i] = ' ';
    extra_path(path, sizeof path, a, "conf");
    if (!strstr(command, "hostapd") || !strstr(command, path))
        return 0;
    *pid = (pid_t)value;
    return 1;
}
static int interface_marker(const struct iface *a, int write_marker)
{
    char path[512], boot[64], saved_boot[64];
    int index, saved_index;
    FILE *fp;
    fp = fopen("/proc/sys/kernel/random/boot_id", "r");
    if (!fp)
        return 0;
    if (fscanf(fp, "%63s", boot) != 1) {
        fclose(fp);
        return 0;
    }
    fclose(fp);
    snprintf(path, sizeof path, "/sys/class/net/%s/ifindex", a->configured);
    fp = fopen(path, "r");
    if (!fp)
        return 0;
    if (fscanf(fp, "%d", &index) != 1) {
        fclose(fp);
        return 0;
    }
    fclose(fp);
    extra_path(path, sizeof path, a, "interface");
    if (write_marker) {
        int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_NOFOLLOW, 0600);
        if (fd < 0)
            return 0;
        fp = fdopen(fd, "w");
        if (!fp) {
            close(fd);
            return 0;
        }
        fprintf(fp, "%s %d\n", boot, index);
        return fclose(fp) == 0;
    }
    fp = fopen(path, "r");
    if (!fp)
        return 0;
    int count = fscanf(fp, "%63s %d", saved_boot, &saved_index);
    fclose(fp);
    return count == 2 && saved_index == index && !strcmp(boot, saved_boot);
}

static int extra_stop(const struct iface *a)
{
    pid_t pid;
    char path[512];
    int owned = process_owned(a, &pid);
    if (owned) {
        kill(pid, SIGTERM);
        for (int i = 0; i < 20; i++) {
            if (kill(pid, 0) != 0)
                break;
            struct timespec delay = {0, 50000000};
            nanosleep(&delay, NULL);
        }
        if (kill(pid, 0) == 0)
            return 0;
    }
    if (interface_marker(a, 0)) {
        const char *argv[] = {"iw", "dev", a->configured, "del", NULL};
        if (device_run_quiet(argv) != 0)
            return 0;
    }
    extra_path(path, sizeof path, a, "interface");
    unlink(path);
    extra_path(path, sizeof path, a, "pid");
    unlink(path);
    extra_path(path, sizeof path, a, "conf");
    unlink(path);
    return 1;
}
static int hostapd_copy_config(const struct snapshot *s, const struct iface *a, int base)
{
    char source[512], path[512], line[4096], key[96];
    snprintf(source, sizeof source, "/data/vendor/wifi/hostapd-%s.conf", s->live[base].name);
    FILE *src = fopen(source, "r");
    if (!src)
        return 0;
    if (mkdir(runtime_dir(), 0700) != 0 && errno != EEXIST) {
        fclose(src);
        return 0;
    }
    extra_path(path, sizeof path, a, "conf");
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_NOFOLLOW, 0600);
    if (fd < 0) {
        fclose(src);
        return 0;
    }
    FILE *dst = fdopen(fd, "w");
    if (!dst) {
        close(fd);
        fclose(src);
        return 0;
    }
    while (fgets(line, sizeof line, src)) {
        if (sscanf(line, "%95[^=]=", key) != 1)
            continue;
        if (!strcmp(key, "interface") || !strcmp(key, "ssid") || !strcmp(key, "ssid2") ||
            !strcmp(key, "bssid") || !strcmp(key, "ctrl_interface") || !strcmp(key, "bridge") ||
            !strcmp(key, "wpa") || !strncmp(key, "wpa_", 4) || !strncmp(key, "sae_", 4) ||
            !strncmp(key, "wps_", 4) || !strcmp(key, "ieee80211w") || !strcmp(key, "ap_isolate") ||
            !strcmp(key, "ignore_broadcast_ssid"))
            continue;
        fputs(line, dst);
    }
    fprintf(dst,
            "\ninterface=%s\nctrl_interface=%s\nbridge=br-lan\nssid=%s\nignore_broadcast_ssid=%"
            "d\nap_isolate=%d\nwps_state=0\n",
            a->configured, runtime_dir(), a->ssid, a->hidden, a->isolate);
    if (!strcmp(a->encryption, "none"))
        fputs("wpa=0\nieee80211w=0\n", dst);
    else {
        fprintf(dst,
                "wpa=2\nwpa_passphrase=%s\nrsn_pairwise=CCMP\nwpa_key_mgmt=%s\nieee80211w=%d\n",
                a->key,
                !strcmp(a->encryption, "sae")         ? "SAE"
                : !strcmp(a->encryption, "sae-mixed") ? "WPA-PSK SAE"
                                                      : "WPA-PSK",
                !strcmp(a->encryption, "sae")         ? 2
                : !strcmp(a->encryption, "sae-mixed") ? 1
                                                      : 0);
        if (!strcmp(a->encryption, "sae") || !strcmp(a->encryption, "sae-mixed"))
            fputs("sae_pwe=2\n", dst);
    }
    int ok = !ferror(src) && !ferror(dst);
    fclose(src);
    if (fclose(dst) != 0)
        ok = 0;
    return ok;
}
static int extra_start(const struct snapshot *s, const struct iface *a, int base)
{
    char pidfile[512], conf[512], log[512], out[256];
    pid_t pid;
    if (process_owned(a, &pid) || interface_marker(a, 0)) {
        if (!extra_stop(a))
            return 0;
    }
    char sys[128];
    snprintf(sys, sizeof sys, "/sys/class/net/%s", a->configured);
    if (access(sys, F_OK) == 0)
        return 0; /* Interface owned by another service. */
    if (!hostapd_copy_config(s, a, base))
        return 0;
    const char *add_argv[] = {
        "iw", "dev", s->live[base].name, "interface", "add", a->configured, "type", "__ap", NULL};
    if (device_run_capture(add_argv, out, sizeof out) != 0)
        return 0;
    if (!interface_marker(a, 1)) {
        const char *del_argv[] = {"iw", "dev", a->configured, "del", NULL};
        (void)device_run_quiet(del_argv);
        return 0;
    }
    extra_path(pidfile, sizeof pidfile, a, "pid");
    extra_path(conf, sizeof conf, a, "conf");
    extra_path(log, sizeof log, a, "log");
    /* Keep a bounded diagnostic file: each start replaces the previous log. */
    int fd = open(log, O_WRONLY | O_CREAT | O_TRUNC | O_NOFOLLOW, 0600);
    if (fd >= 0)
        close(fd);
    const char *argv[] = {"hostapd", "-B", "-P", pidfile, "-f", log, conf, NULL};
    if (device_run_capture(argv, out, sizeof out) != 0) {
        const char *del_argv[] = {"iw", "dev", a->configured, "del", NULL};
        (void)device_run_quiet(del_argv);
        return 0;
    }
    extra_started[!strcmp(a->section, "datad_ssid_1") ? 0 : 1] = time(NULL);
    return 1;
}
static void extra_reconcile(struct snapshot *s)
{
    for (int i = 4; i < AP_COUNT; i++) {
        struct iface *a = &s->ap[i];
        int slot = i - 4;
        pid_t pid;
        int owner = process_owned(a, &pid);
        if (!a->exists || !a->enabled || !s->r[a->band].enabled) {
            if (owner || interface_marker(a, 0))
                (void)extra_stop(a);
            extra_base_index[slot] = 0;
            extra_attempts[slot] = 0;
            continue;
        }
        int base = -1;
        for (int j = 0; j < 4; j++)
            if (s->ap[j].band == a->band && s->ap[j].live >= 0 &&
                ready(s->live[s->ap[j].live].name)) {
                base = s->ap[j].live;
                break;
            }
        if (base < 0)
            continue;
        int index = s->live[base].index;
        if (extra_base_index[slot] != index) {
            if (extra_base_index[slot] && owner)
                (void)extra_stop(a);
            extra_base_index[slot] = index;
            extra_attempts[slot] = 0;
        }
        if (a->live >= 0 && owner) {
            extra_error[slot][0] = 0;
            continue;
        }
        if (owner && extra_started[slot] && time(NULL) - extra_started[slot] < 120)
            continue;
        if (extra_attempts[slot] >= 3)
            continue;
        extra_attempts[slot]++;
        if (extra_start(s, a, base))
            extra_error[slot][0] = 0;
        else
            snprintf(extra_error[slot], sizeof extra_error[slot],
                     "Extra AP could not start; reserved interface may be in use");
    }
}
static int write_extra(const struct iface *a, int remove)
{
    char path[128], value[32];
    snprintf(path, sizeof path, "datad_wifi.%s", a->section);
    if (remove)
        return device_uci_delete(path) == 0 && device_uci_commit("datad_wifi") == 0;
    /* uci cannot create a missing package. Its initial empty file contains no
     * credentials and is private from the first open. */
    const char *config = getenv("ZWRT_DATAD_WIFI_CONFIG");
    if (!config || !*config)
        config = "/etc/config/datad_wifi";
    int fd = open(config, O_WRONLY | O_CREAT | O_NOFOLLOW, 0600);
    if (fd < 0)
        return 0;
    close(fd);
    if (device_uci_set(path, "wifi-iface") != 0)
        return 0;
    static const char *opts[] = {"band",     "ssid",   "encryption", "key",
                                 "disabled", "hidden", "isolate",    "datad_psm"};
    for (size_t j = 0; j < sizeof opts / sizeof opts[0]; j++) {
        const char *v;
        switch (j) {
        case 0:
            v = a->band ? "5g" : "2g";
            break;
        case 1:
            v = a->ssid;
            break;
        case 2:
            v = a->encryption;
            break;
        case 3:
            v = a->key;
            break;
        case 4:
            snprintf(value, sizeof value, "%d", !a->enabled);
            v = value;
            break;
        case 5:
            snprintf(value, sizeof value, "%d", a->hidden);
            v = value;
            break;
        case 6:
            snprintf(value, sizeof value, "%d", a->isolate);
            v = value;
            break;
        default:
            v = a->psm;
        }
        snprintf(path, sizeof path, "datad_wifi.%s.%s", a->section, opts[j]);
        if (device_uci_set(path, v) != 0) {
            device_uci_revert("datad_wifi");
            return 0;
        }
    }
    if (device_uci_commit("datad_wifi") != 0) {
        device_uci_revert("datad_wifi");
        return 0;
    }
    return 1;
}
static int extra_control(struct snapshot *s, const char *action, const char *params, char *result,
                         size_t len, char *err, size_t errlen)
{
    char value[256];
    int i = -1, create = !strcmp(action, "wifi.interface.create"),
        remove = !strcmp(action, "wifi.interface.delete");
    if (create) {
        for (int j = 4; j < AP_COUNT; j++)
            if (!s->ap[j].exists) {
                i = j;
                break;
            }
        if (i < 0) {
            snprintf(err, errlen, "\1two extra SSID slots are already configured");
            return 0;
        }
    } else {
        getval(params, "section", value, sizeof value);
        for (int j = 4; j < AP_COUNT; j++)
            if (!strcmp(value, s->ap[j].section) && s->ap[j].exists)
                i = j;
    }
    if (i < 0)
        goto invalid;
    struct iface old = s->ap[i], a = old;
    if (remove) {
        if (!extra_stop(&a) || !write_extra(&a, 1))
            goto failed;
        extra_error[i - 4][0] = 0;
        extra_base_index[i - 4] = extra_attempts[i - 4] = 0;
        snprintf(result, len, "{\"section\":\"%s\",\"changed\":true,\"deleted\":true}", a.section);
        return 1;
    }
    if (create) {
        a.enabled = 1;
        a.exists = 1;
        snprintf(a.encryption, sizeof a.encryption, "sae-mixed");
    }
    if (getval(params, "band", value, sizeof value)) {
        if (!strcmp(value, "5g"))
            a.band = 1;
        else if (!strcmp(value, "2g"))
            a.band = 0;
        else
            goto invalid;
    } else if (create)
        goto invalid;
    if (getval(params, "ssid", value, sizeof value)) {
        if (!value[0] || strlen(value) > 32 || strpbrk(value, "\r\n"))
            goto invalid;
        memcpy(a.ssid, value, strlen(value) + 1);
    } else if (create)
        goto invalid;
    if (getval(params, "encryption", value, sizeof value)) {
        if (strcmp(value, "none") && strcmp(value, "psk2+ccmp") && strcmp(value, "sae-mixed") &&
            strcmp(value, "sae"))
            goto invalid;
        memcpy(a.encryption, value, strlen(value) + 1);
    }
    if (getval(params, "key", value, sizeof value) && value[0]) {
        if (strlen(value) < 8 || strlen(value) > 63 || strpbrk(value, "\r\n"))
            goto invalid;
        memcpy(a.key, value, strlen(value) + 1);
    }
    if (strcmp(a.encryption, "none") && strlen(a.key) < 8)
        goto invalid;
    static const char *bools[] = {"enabled", "hidden", "isolate"};
    for (int k = 0; k < 3; k++)
        if (getval(params, bools[k], value, sizeof value)) {
            if (strcmp(value, "0") && strcmp(value, "1"))
                goto invalid;
            int v = !strcmp(value, "1");
            if (k == 0)
                a.enabled = v;
            else if (k == 1)
                a.hidden = v;
            else
                a.isolate = v;
        }
    /* Ambiguous identity would prevent reliable per-interface policy mapping. */
    for (int j = 0; j < AP_COUNT; j++)
        if (j != i && s->ap[j].exists && s->ap[j].band == a.band && !strcmp(s->ap[j].ssid, a.ssid))
            goto invalid;
    if (!write_extra(&a, 0))
        goto failed;
    if (!extra_stop(&old))
        goto rollback;
    int base = -1;
    for (int j = 0; j < 4; j++)
        if (s->ap[j].band == a.band && s->ap[j].live >= 0 && s->live[s->ap[j].live].ready) {
            base = s->ap[j].live;
            break;
        }
    if (a.enabled && s->r[a.band].enabled && base >= 0 && !extra_start(s, &a, base))
        goto rollback;
    extra_base_index[i - 4] = base < 0 ? 0 : s->live[base].index;
    extra_attempts[i - 4] = 0;
    extra_error[i - 4][0] = 0;
    memset(&applied[i], 0, sizeof applied[i]);
    snprintf(result, len, "{\"section\":\"%s\",\"saved\":true,\"changed\":true,\"pending\":%s}",
             a.section, a.enabled ? "true" : "false");
    return 1;
rollback:
    (void)extra_stop(&a);
    (void)write_extra(&old, create);
    snprintf(err, errlen, "extra SSID could not start; previous configuration restored");
    return 0;
invalid:
    snprintf(err, errlen, "\1invalid extra SSID settings or duplicate SSID on this band");
    return 0;
failed:
    snprintf(err, errlen, "could not save or stop extra SSID");
    return 0;
}

static int status_json(struct snapshot *s, char *out, size_t len)
{
    struct buf b = {out, len, 0, 0};
    add(&b, "{\"supported\":true,\"max_configured_ssids\":6,\"extra_ssid_slots\":2,\"extra_ssid_"
            "supported\":true,\"hardware_limit_dbm\":null,\"radios\":[");
    for (int i = 0; i < 2; i++) {
        struct radio *r = &s->r[i];
        int limit = regulatory_limit(s, i);
        double reported = -1;
        for (int j = 0; j < AP_COUNT; j++)
            if (s->ap[j].band == i && s->ap[j].live >= 0 && s->live[s->ap[j].live].ready) {
                reported = s->live[s->ap[j].live].dbm;
                break;
            }
        add(&b,
            "%s{\"band\":\"%s\",\"enabled\":%s,\"percent\":%d,\"configured_dbm\":%d,\"requested_"
            "dbm\":",
            i ? "," : "", r->band, r->enabled ? "true" : "false", r->percent, r->dbm);
        if (r->requested < 0)
            add(&b, "null");
        else
            add(&b, "%d", r->requested);
        add(&b, ",\"reported_dbm\":");
        if (reported < 0)
            add(&b, "null");
        else
            add(&b, "%.2f", reported);
        add(&b, ",\"regulatory_limit_dbm\":");
        if (limit < 0)
            add(&b, "null");
        else
            add(&b, "%d", limit);
        add(&b, "}");
    }
    add(&b, "],\"interfaces\":[");
    for (int i = 0; i < AP_COUNT; i++) {
        struct iface *a = &s->ap[i];
        if (!a->exists)
            continue;
        struct live *p = a->live < 0 ? NULL : &s->live[a->live];
        add(&b, "%s{\"section\":\"%s\",\"kind\":\"%s\",\"band\":\"%s\",\"ssid\":", i ? "," : "",
            a->section,
            i >= 4  ? "extra"
            : i % 2 ? "guest"
                    : "main",
            s->r[a->band].band);
        quote(&b, a->ssid);
        add(&b, ",\"enabled\":%s,\"radio_enabled\":%s,\"active\":%s,\"ifname\":",
            a->enabled ? "true" : "false", s->r[a->band].enabled ? "true" : "false",
            p && p->ready ? "true" : "false");
        quote(&b, p ? p->name : "");
        add(&b, ",\"hidden\":%s,\"isolate\":%s,\"has_key\":%s,\"encryption\":",
            a->hidden ? "true" : "false", a->isolate ? "true" : "false",
            a->has_key ? "true" : "false");
        quote(&b, a->encryption);
        add(&b, ",\"txpower_dbm\":");
        if (p && p->ready)
            add(&b, "%.2f", p->dbm);
        else
            add(&b, "null");
        add(&b, ",\"error\":");
        quote(&b, i >= 4 ? extra_error[i - 4] : "");
        add(&b, ",\"psm_mode\":");
        quote(&b, a->psm[0] ? a->psm : "default");
        add(&b, ",\"psm_actual\":");
        if (!p || p->psm < 0)
            add(&b, "null");
        else
            add(&b, p->psm ? "true" : "false");
        add(&b, "}");
    }
    add(&b, "]}");
    return !b.failed;
}
static int set_option(const char *section, const char *option, const char *value)
{
    char path[128];
    snprintf(path, sizeof path, "%s.%s.%s",
             !strncmp(section, "datad_ssid_", 11) ? "datad_wifi" : "wireless", section, option);
    if (value)
        return device_uci_set(path, value) == 0;
    char old[128];
    if (device_uci_get(path, old, sizeof old) != 0)
        return 1;
    return device_uci_delete(path) == 0;
}
static int set_power(struct live *p, int dbm)
{
    char val[32], out[128];
    snprintf(val, sizeof val, "%d", dbm * 100);
    const char *argv[] = {"iw", "dev", p->name, "set", "txpower", "fixed", val, NULL};
    return device_run_capture(argv, out, sizeof out) == 0;
}
static int set_psm(struct live *p, int on)
{
    char out[128];
    if (iw(p->name, "set", "power_save", on ? "on" : "off", out, sizeof out) != 0)
        return 0;
    return iw(p->name, "get", "power_save", NULL, out, sizeof out) == 0 &&
           strstr(out, on ? "Power save: on" : "Power save: off") != NULL;
}
static int strict_int(const char *params, const char *key, int *value)
{
    char s[32], *end;
    if (!getval(params, key, s, sizeof s) || !s[0])
        return 0;
    long n = strtol(s, &end, 10);
    if (*end || n < 0 || n > 1000)
        return 0;
    *value = (int)n;
    return 1;
}
/* The vendor commits wireless and then reapplies percentage power after DFS.
 * Reapply the selected dBm once after each settled config revision. PSM remains
 * independent and is never reasserted just because another option changed. */
static unsigned long power_revision;
static time_t power_revision_at;
static void observe_wireless_revision(time_t now)
{
    static ino_t inode;
    static time_t modified;
    static off_t size;
    const char *path = getenv("ZWRT_DATAD_WIRELESS_CONFIG");
    if (!path || !*path)
        path = "/etc/config/wireless";
    struct stat st;
    if (stat(path, &st) != 0)
        return;
    if (inode != st.st_ino || modified != st.st_mtime || size != st.st_size) {
        inode = st.st_ino;
        modified = st.st_mtime;
        size = st.st_size;
        power_revision++;
        power_revision_at = now;
    }
}

void wifi_runtime_tick(void)
{
    time_t now = time(NULL);
    if (!force_tick && now < next_tick)
        return;
    force_tick = 0;
    next_tick = now + 5;
    struct snapshot s;
    if (!snapshot(&s, 0))
        return;
    observe_wireless_revision(now);
    extra_reconcile(&s);
    for (int i = 0; i < AP_COUNT; i++) {
        struct iface *a = &s.ap[i];
        struct applied *last = &applied[i];
        int power = s.r[a->band].requested;
        /* Additional APs are outside the OEM loader; inherit its computed
         * radio power when the user has not selected a dBm override. */
        if (power < 0 && i >= 4)
            power = s.r[a->band].dbm;
        int psm = !strcmp(a->psm, "on") ? 1 : !strcmp(a->psm, "off") ? 0 : -1;
        if (a->live < 0 || !a->enabled || !s.r[a->band].enabled || (power < 0 && psm < 0)) {
            memset(last, 0, sizeof *last);
            continue;
        }
        struct live *p = &s.live[a->live];
        if (last->index != p->index || strcmp(last->ssid, a->ssid)) {
            memset(last, 0, sizeof *last);
            last->index = p->index;
            last->dbm = -2;
            last->psm = -2;
            snprintf(last->ssid, sizeof last->ssid, "%s", a->ssid);
            continue;
        }
        int need_power =
            power >= 0 && (last->dbm != power || last->power_revision != power_revision);
        int need_psm = psm >= 0 && last->psm != psm;
        if (last->attempt_revision != power_revision) {
            last->attempts = 0;
            last->attempt_revision = power_revision;
        }
        if (power < 0)
            last->dbm = -1;
        if (psm < 0)
            last->psm = -1;
        /* Give the OEM its final iw write after its atomic UCI commit. */
        if (need_power && now - power_revision_at < 5)
            need_power = 0;
        if ((!need_power && !need_psm) || last->attempts >= 3 || !ready(p->name))
            continue;
        last->attempts++;
        int ok = 1;
        if (need_power) {
            if (set_power(p, power)) {
                last->dbm = power;
                last->power_revision = power_revision;
            } else
                ok = 0;
        }
        if (need_psm) {
            if (set_psm(p, psm))
                last->psm = psm;
            else
                ok = 0;
        }
        if (ok)
            last->attempts = 0;
    }
}

int wifi_control_execute(const char *action, const char *params, char *result, size_t len,
                         char *err, size_t errlen)
{
    struct snapshot s;
    if (!model_supported()) {
        if (!strcmp(action, "wifi.advanced.status")) {
            snprintf(result, len, "{\"supported\":false}");
            return 1;
        }
        snprintf(err, errlen, "advanced Wi-Fi unsupported by this model");
        return 0;
    }
    if (!snapshot(&s, 1)) {
        snprintf(err, errlen, "cannot read wireless configuration");
        return 0;
    }
    if (!strcmp(action, "wifi.advanced.status"))
        return status_json(&s, result, len);
    char value[128], old[32];
    int target = -1, pending = 0;
    const char *policy_package = "wireless";
    if (!strcmp(action, "wifi.interface.create") || !strcmp(action, "wifi.interface.delete") ||
        !strcmp(action, "wifi.interface.configure")) {
        int ok = extra_control(&s, action, params, result, len, err, errlen);
        force_tick = 1;
        return ok;
    }
    if (!strcmp(action, "wifi.psm.set")) {
        if (!getval(params, "section", value, sizeof value))
            goto invalid;
        for (int i = 0; i < AP_COUNT; i++)
            if (!strcmp(value, s.ap[i].section))
                target = i;
        if (target < 0 || !getval(params, "mode", value, sizeof value) ||
            (strcmp(value, "default") && strcmp(value, "on") && strcmp(value, "off")))
            goto invalid;
        struct iface *a = &s.ap[target];
        if (!a->exists)
            goto invalid;
        policy_package = target >= 4 ? "datad_wifi" : "wireless";
        snprintf(old, sizeof old, "%s", a->psm);
        if (!set_option(a->section, "datad_psm", strcmp(value, "default") ? value : NULL))
            goto persist_failed;
        if (device_uci_commit(policy_package) != 0)
            goto persist_failed;
        /* default releases ownership; the current driver state stays until its next reload. */
        if (strcmp(value, "default") && a->live >= 0 && s.live[a->live].ready) {
            if (!set_psm(&s.live[a->live], !strcmp(value, "on"))) {
                set_option(a->section, "datad_psm", old[0] ? old : NULL);
                device_uci_commit(policy_package);
                goto apply_failed;
            }
            applied[target] = (struct applied){.index = s.live[a->live].index,
                                               .dbm = s.r[a->band].requested,
                                               .psm = !strcmp(value, "on")};
            snprintf(applied[target].ssid, sizeof applied[target].ssid, "%s", a->ssid);
        } else
            pending = strcmp(value, "default") != 0;
    } else if (!strcmp(action, "wifi.txpower.set_dbm")) {
        if (!getval(params, "band", value, sizeof value))
            goto invalid;
        target = !strcmp(value, "2g") ? 0 : !strcmp(value, "5g") ? 1 : -1;
        if (target < 0)
            goto invalid;
        int dbm = -1,
            restore = getval(params, "mode", value, sizeof value) && !strcmp(value, "oem");
        if (!restore && (!strict_int(params, "dbm", &dbm) || dbm < 1 || dbm > 30))
            goto invalid;
        int limit = regulatory_limit(&s, target);
        if (!restore && limit >= 0 && dbm > limit) {
            snprintf(err, errlen, "\1requested power exceeds current channel limit (%d dBm)", limit);
            return 0;
        }
        struct radio *r = &s.r[target];
        snprintf(old, sizeof old, "%d", r->requested);
        snprintf(value, sizeof value, "%d", dbm);
        if (!set_option(r->section, "datad_txpower_dbm", restore ? NULL : value) ||
            device_uci_commit("wireless") != 0)
            goto persist_failed;
        int applied_count = 0;
        for (int i = 0; i < AP_COUNT; i++)
            if (s.ap[i].band == target && s.ap[i].live >= 0 && s.live[s.ap[i].live].ready) {
                if (!set_power(&s.live[s.ap[i].live], restore ? r->dbm : dbm)) {
                    set_option(r->section, "datad_txpower_dbm", r->requested < 0 ? NULL : old);
                    device_uci_commit("wireless");
                    for (int j = 0; j < AP_COUNT; j++)
                        if (s.ap[j].band == target && s.ap[j].live >= 0)
                            set_power(&s.live[s.ap[j].live],
                                      r->requested < 0 ? r->dbm : r->requested);
                    goto apply_failed;
                }
                applied_count++;
                memset(&applied[i], 0, sizeof applied[i]);
            }
        pending = !applied_count && !restore;
    } else {
        snprintf(err, errlen, "unsupported advanced Wi-Fi action");
        return 0;
    }
    force_tick = 1;
    struct timespec pause = {!strcmp(action, "wifi.txpower.set_dbm") ? 2 : 0, 500000000};
    nanosleep(&pause, NULL);
    snprintf(result, len, "{\"saved\":true,\"pending\":%s}", pending ? "true" : "false");
    return 1;
invalid:
    snprintf(err, errlen, "\1invalid Wi-Fi parameter");
    return 0;
persist_failed:
    device_uci_revert(policy_package);
    snprintf(err, errlen, "could not persist Wi-Fi policy");
    return 0;
apply_failed:
    snprintf(err, errlen, "driver rejected Wi-Fi policy; previous setting restored");
    return 0;
}
