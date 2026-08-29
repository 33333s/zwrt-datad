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

#include <arpa/inet.h>
#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
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
static volatile sig_atomic_t g_requested_interval_ms;

#define COOLING_CONFIG_DEFAULT "/data/zwrt-datad/cooling.conf"
#define FAN_PWM_DEFAULT "/sys/class/hwmon/hwmon0/pwm1"
#define FAN_THERMAL_ENABLE_DEFAULT "/sys/class/hwmon/hwmon0/device/thermal_enable"
#define LIQUID_DRIVE_DEFAULT "/sys/class/leds/aw_vibrator/atsin0"
#define LIQUID_THERMAL_ENABLE_DEFAULT "/sys/class/leds/aw_vibrator/thermal_enable"
#define THERMAL_ROOT_DEFAULT "/sys/class/thermal"
#define CUSTOM_CURVE_MAX_POINTS 8
#define CUSTOM_CURVE_HARD_FULL_SPEED_C 80
#define FAN_ALWAYS_ON_PWM 128
#define LIQUID_DRIVE_DURATION 1023
#define LIQUID_DRIVE_FREQUENCY 200
#define LIQUID_LOW_AMPLITUDE 60
#define LIQUID_HIGH_AMPLITUDE 200

enum fan_control_mode {
    FAN_MODE_MANUAL = 0,
    FAN_MODE_KERNEL = 1,
    FAN_MODE_CUSTOM = 2
};

struct fan_curve_point {
    int temperature;
    int pwm;
};

struct cooling_config {
    int fan_enabled;
    int fan_always_on;
    int fan_mode;
    int fan_speed_percent;
    int liquid_always_on;
    int liquid_level;
    int temperatures[3];
    int hysteresis[3];
    int custom_curve_count;
    struct fan_curve_point custom_curve[CUSTOM_CURVE_MAX_POINTS];
};

/* Automatic mode temporarily takes userspace ownership at the hard thermal
 * limit. Remember that transition so the next cooler sample can hand control
 * back to the kernel curve instead of leaving PWM 255 latched. */
static int g_fan_automatic_hard_override;

static const char *env_path(const char *name, const char *fallback)
{
    const char *value = getenv(name);
    return value && *value ? value : fallback;
}

static int read_text_file(const char *path, char *out, size_t outlen)
{
    FILE *fp;
    if (!path || !out || outlen < 2) return 0;
    fp = fopen(path, "r");
    if (!fp) return 0;
    if (!fgets(out, outlen, fp)) {
        fclose(fp);
        out[0] = 0;
        return 0;
    }
    fclose(fp);
    out[strcspn(out, "\r\n")] = 0;
    return 1;
}

static int write_text_file(const char *path, const char *value)
{
    int fd;
    size_t len, written = 0;
    if (!path || !value) return 0;
    fd = open(path, O_WRONLY | O_CLOEXEC | O_TRUNC);
    if (fd < 0) return 0;
    len = strlen(value);
    while (written < len) {
        ssize_t n = write(fd, value + written, len - written);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) {
            close(fd);
            return 0;
        }
        written += (size_t)n;
    }
    close(fd);
    return 1;
}

static int find_cooling_zone(char *out, size_t outlen)
{
    const char *fixed = getenv("ZWRT_DATAD_COOLING_ZONE_PATH");
    const char *root = env_path("ZWRT_DATAD_COOLING_THERMAL_ROOT", THERMAL_ROOT_DEFAULT);
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
        if (!read_text_file(path, type, sizeof type) || strcmp(type, "sys-therm-4")) continue;
        snprintf(out, outlen, "%s/%s", root, entry->d_name);
        closedir(dir);
        return 1;
    }
    closedir(dir);
    return 0;
}

static void cooling_config_defaults(struct cooling_config *cfg)
{
    memset(cfg, 0, sizeof *cfg);
    cfg->fan_always_on = -1;
    cfg->liquid_always_on = -1;
    cfg->liquid_level = 1;
    cfg->fan_mode = FAN_MODE_MANUAL;
    cfg->fan_speed_percent = 50;
    cfg->temperatures[0] = 44;
    cfg->temperatures[1] = 48;
    cfg->temperatures[2] = 53;
    cfg->hysteresis[0] = cfg->hysteresis[1] = cfg->hysteresis[2] = 4;
    cfg->custom_curve_count = 5;
    cfg->custom_curve[0] = (struct fan_curve_point){40, 0};
    cfg->custom_curve[1] = (struct fan_curve_point){45, 0};
    cfg->custom_curve[2] = (struct fan_curve_point){50, 76};
    cfg->custom_curve[3] = (struct fan_curve_point){60, 128};
    cfg->custom_curve[4] = (struct fan_curve_point){70, 255};
}

static int load_cooling_config(struct cooling_config *cfg)
{
    const char *path = env_path("ZWRT_DATAD_COOLING_CONFIG", COOLING_CONFIG_DEFAULT);
    FILE *fp;
    char line[128], key[64];
    int value, loaded = 0, mode_seen = 0;
    cooling_config_defaults(cfg);
    fp = fopen(path, "r");
    if (!fp) {
        cfg->fan_always_on = 0;
        cfg->liquid_always_on = 0;
        return 0;
    }
    while (fgets(line, sizeof line, fp)) {
        if (sscanf(line, "%63[^=]=%d", key, &value) != 2) continue;
        if (!strcmp(key, "fan_enabled")) cfg->fan_enabled = value != 0;
        else if (!strcmp(key, "fan_always_on")) cfg->fan_always_on = value != 0;
        else if (!strcmp(key, "fan_auto") && !mode_seen)
            cfg->fan_mode = value ? FAN_MODE_KERNEL : FAN_MODE_MANUAL;
        else if (!strcmp(key, "fan_mode")) {
            if (value >= FAN_MODE_MANUAL && value <= FAN_MODE_CUSTOM) {
                cfg->fan_mode = value;
                mode_seen = 1;
            }
        }
        else if (!strcmp(key, "fan_speed_percent")) cfg->fan_speed_percent = value;
        else if (!strcmp(key, "liquid_enabled") && cfg->liquid_always_on < 0)
            cfg->liquid_always_on = value != 0;
        else if (!strcmp(key, "liquid_always_on")) cfg->liquid_always_on = value != 0;
        else if (!strcmp(key, "liquid_level") && value >= 1 && value <= 2)
            cfg->liquid_level = value;
        else if (!strcmp(key, "temperature_1")) cfg->temperatures[0] = value;
        else if (!strcmp(key, "temperature_2")) cfg->temperatures[1] = value;
        else if (!strcmp(key, "temperature_3")) cfg->temperatures[2] = value;
        else if (!strcmp(key, "hysteresis_1")) cfg->hysteresis[0] = value;
        else if (!strcmp(key, "hysteresis_2")) cfg->hysteresis[1] = value;
        else if (!strcmp(key, "hysteresis_3")) cfg->hysteresis[2] = value;
        else if (!strcmp(key, "custom_curve_count")) {
            if (value >= 2 && value <= CUSTOM_CURVE_MAX_POINTS)
                cfg->custom_curve_count = value;
        } else {
            int index;
            if (sscanf(key, "custom_temperature_%d", &index) == 1 &&
                index >= 1 && index <= CUSTOM_CURVE_MAX_POINTS)
                cfg->custom_curve[index - 1].temperature = value;
            else if (sscanf(key, "custom_pwm_%d", &index) == 1 &&
                     index >= 1 && index <= CUSTOM_CURVE_MAX_POINTS)
                cfg->custom_curve[index - 1].pwm = value;
        }
        loaded = 1;
    }
    fclose(fp);
    /* 0.9.10 wrote fan_switch_status=1 while a custom curve was active.
     * Never migrate that legacy value to always-on or the saved curve would
     * unexpectedly become a fixed PWM 128 after upgrade. */
    if (cfg->fan_always_on < 0)
        cfg->fan_always_on = cfg->fan_mode == FAN_MODE_KERNEL ? cfg->fan_enabled : 0;
    /* Preserve an explicit kernel/automatic mode.  Only the removed legacy
     * manual/off state needs migration to the saved custom curve. */
    if (!cfg->fan_always_on && cfg->fan_mode == FAN_MODE_MANUAL)
        cfg->fan_mode = FAN_MODE_CUSTOM;
    if (cfg->liquid_always_on < 0) cfg->liquid_always_on = 0;
    return loaded;
}

static int save_cooling_config(const struct cooling_config *cfg)
{
    const char *path = env_path("ZWRT_DATAD_COOLING_CONFIG", COOLING_CONFIG_DEFAULT);
    char temp[PATH_MAX];
    FILE *fp;
    int failed = 0;
    if (snprintf(temp, sizeof temp, "%s.tmp", path) >= (int)sizeof temp) return 0;
    fp = fopen(temp, "w");
    if (!fp) return 0;
    if (fprintf(fp,
                "fan_enabled=1\nfan_always_on=%d\nfan_auto=%d\nfan_mode=%d\nfan_speed_percent=%d\n"
                "liquid_enabled=%d\nliquid_always_on=%d\nliquid_level=%d\n"
                "temperature_1=%d\ntemperature_2=%d\ntemperature_3=%d\n"
                "hysteresis_1=%d\nhysteresis_2=%d\nhysteresis_3=%d\n"
                "custom_curve_count=%d\n",
                cfg->fan_always_on, cfg->fan_mode == FAN_MODE_KERNEL, cfg->fan_mode,
                cfg->fan_speed_percent,
                cfg->liquid_always_on, cfg->liquid_always_on, cfg->liquid_level,
                cfg->temperatures[0], cfg->temperatures[1], cfg->temperatures[2],
                cfg->hysteresis[0], cfg->hysteresis[1], cfg->hysteresis[2],
                cfg->custom_curve_count) < 0)
        failed = 1;
    for (int i = 0; !failed && i < cfg->custom_curve_count; i++) {
        if (fprintf(fp, "custom_temperature_%d=%d\ncustom_pwm_%d=%d\n",
                    i + 1, cfg->custom_curve[i].temperature,
                    i + 1, cfg->custom_curve[i].pwm) < 0)
            failed = 1;
    }
    if (fflush(fp) != 0 || fsync(fileno(fp)) != 0) failed = 1;
    if (fclose(fp) != 0) failed = 1;
    if (failed) {
        (void)unlink(temp);
        return 0;
    }
    if (rename(temp, path) != 0) {
        (void)unlink(temp);
        return 0;
    }
    return 1;
}

static int set_vendor_switch_status(const char *key, int enabled)
{
    char value[2];
    snprintf(value, sizeof value, "%d", enabled ? 1 : 0);
    if (device_uci_set(key, value) != 0) return 0;
    return device_uci_commit("zwrt_deviceui") == 0;
}

static int set_fan_pwm(int pwm)
{
    char value[16];
    if (pwm < 0 || pwm > 255) return 0;
    snprintf(value, sizeof value, "%d", pwm);
    return write_text_file(env_path("ZWRT_DATAD_FAN_PWM_PATH", FAN_PWM_DEFAULT), value);
}

static int set_fan_pwm_percent(int percent)
{
    if (percent < 0 || percent > 100) return 0;
    return set_fan_pwm((percent * 255 + 50) / 100);
}

static int set_fan_thermal_enabled(int enabled)
{
    return write_text_file(
        env_path("ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH", FAN_THERMAL_ENABLE_DEFAULT),
        enabled ? "1" : "0");
}

static int set_liquid_thermal_enabled(int enabled)
{
    return write_text_file(
        env_path("ZWRT_DATAD_LIQUID_THERMAL_ENABLE_PATH", LIQUID_THERMAL_ENABLE_DEFAULT),
        enabled ? "1" : "0");
}

static int set_cooling_zone_mode(int enabled)
{
    char zone[PATH_MAX], path[PATH_MAX];
    if (!find_cooling_zone(zone, sizeof zone)) return 0;
    if (snprintf(path, sizeof path, "%s/mode", zone) >= (int)sizeof path) return 0;
    return write_text_file(path, enabled ? "enabled" : "disabled");
}

static int clear_fan_cooling_state(void)
{
    const char *fixed = getenv("ZWRT_DATAD_FAN_COOLING_STATE_PATH");
    const char *root = env_path("ZWRT_DATAD_COOLING_THERMAL_ROOT", THERMAL_ROOT_DEFAULT);
    DIR *dir;
    struct dirent *entry;
    if (fixed && *fixed) return write_text_file(fixed, "0");
    dir = opendir(root);
    if (!dir) return 1;
    while ((entry = readdir(dir)) != NULL) {
        char path[PATH_MAX], type[64];
        if (strncmp(entry->d_name, "cooling_device", 14) != 0) continue;
        if (snprintf(path, sizeof path, "%s/%s/type", root, entry->d_name) >= (int)sizeof path)
            continue;
        if (!read_text_file(path, type, sizeof type) || strcmp(type, "pwm-fan")) continue;
        if (snprintf(path, sizeof path, "%s/%s/cur_state", root, entry->d_name) >=
            (int)sizeof path) {
            closedir(dir);
            return 0;
        }
        closedir(dir);
        return write_text_file(path, "0");
    }
    closedir(dir);
    return 1;
}

static int prepare_fan_pwm_control(void)
{
    /* On MU5252 thermal_enable powers the fan hardware. Keep it enabled;
     * disable only sys-therm-4 and clear pwm-fan's latched cooling state. */
    if (!set_fan_thermal_enabled(1)) return 0;
    if (!set_cooling_zone_mode(0)) return 0;
    if (clear_fan_cooling_state()) return 1;
    (void)set_cooling_zone_mode(1);
    return 0;
}

static int apply_datad_fan_pwm(int pwm)
{
    if (!prepare_fan_pwm_control()) return 0;
    if (set_fan_pwm(pwm)) return 1;
    /* Never leave automatic cooling disabled if userspace could not write
     * the requested PWM. */
    (void)set_cooling_zone_mode(1);
    return 0;
}

static int apply_fan_curve(const struct cooling_config *cfg)
{
    char zone[PATH_MAX], path[PATH_MAX], value[32];
    int ok = 1;
    if (!find_cooling_zone(zone, sizeof zone)) return 0;
    for (int i = 0; i < 3; i++) {
        if (snprintf(path, sizeof path, "%s/trip_point_%d_temp", zone, i) >= (int)sizeof path) {
            ok = 0;
            break;
        }
        snprintf(value, sizeof value, "%d", cfg->temperatures[i] * 1000);
        if (!write_text_file(path, value)) {
            ok = 0;
            break;
        }
        if (snprintf(path, sizeof path, "%s/trip_point_%d_hyst", zone, i) >= (int)sizeof path) {
            ok = 0;
            break;
        }
        snprintf(value, sizeof value, "%d", cfg->hysteresis[i] * 1000);
        if (!write_text_file(path, value)) {
            ok = 0;
            break;
        }
    }
    if (!set_cooling_zone_mode(1)) ok = 0;
    return ok;
}

static int apply_liquid_switch(int always_on, int level)
{
    const char *drive = env_path("ZWRT_DATAD_LIQUID_DRIVE_PATH", LIQUID_DRIVE_DEFAULT);
    if (always_on) {
        char value[48];
        int amplitude = level >= 2 ? LIQUID_HIGH_AMPLITUDE : LIQUID_LOW_AMPLITUDE;
        if (!set_liquid_thermal_enabled(0)) return 0;
        snprintf(value, sizeof value, "%d %d %d", LIQUID_DRIVE_DURATION,
                 amplitude, LIQUID_DRIVE_FREQUENCY);
        if (write_text_file(drive, value)) return 1;
        (void)set_liquid_thermal_enabled(1);
        return 0;
    }
    {
        int drive_ok = write_text_file(drive, "0 0 0");
        int thermal_ok = set_liquid_thermal_enabled(1);
        return drive_ok && thermal_ok;
    }
}

static long read_custom_curve_temperature(void)
{
    char zone[PATH_MAX], path[PATH_MAX], line[64], *end;
    long raw;
    if (!find_cooling_zone(zone, sizeof zone)) return -1;
    if (snprintf(path, sizeof path, "%s/temp", zone) >= (int)sizeof path) return -1;
    if (!read_text_file(path, line, sizeof line)) return -1;
    errno = 0;
    raw = strtol(line, &end, 10);
    if (errno || end == line || raw <= 0) return -1;
    return raw >= 1000 ? (raw + 500) / 1000 : raw;
}

static int custom_curve_pwm(const struct cooling_config *cfg, long temperature)
{
    const struct fan_curve_point *points = cfg->custom_curve;
    int count = cfg->custom_curve_count;
    if (temperature >= CUSTOM_CURVE_HARD_FULL_SPEED_C) return 255;
    if (count < 2 || temperature <= 0) return -1;
    if (temperature <= points[0].temperature) return points[0].pwm;
    for (int i = 1; i < count; i++) {
        long x0 = points[i - 1].temperature;
        long x1 = points[i].temperature;
        long y0 = points[i - 1].pwm;
        long y1 = points[i].pwm;
        if (temperature <= x1) {
            long numerator = (temperature - x0) * (y1 - y0);
            return (int)(y0 + (numerator + (x1 - x0) / 2) / (x1 - x0));
        }
    }
    return points[count - 1].pwm;
}

static int apply_custom_curve(const struct cooling_config *cfg, long temperature)
{
    int pwm = custom_curve_pwm(cfg, temperature);
    if (pwm < 0) return 0;
    return apply_datad_fan_pwm(pwm);
}

static int apply_fan_config(const struct cooling_config *cfg)
{
    long temperature = read_custom_curve_temperature();
    if (cfg->fan_always_on) {
        int ok = apply_datad_fan_pwm(temperature >= CUSTOM_CURVE_HARD_FULL_SPEED_C
                                     ? 255 : FAN_ALWAYS_ON_PWM);
        if (ok) g_fan_automatic_hard_override = 0;
        return ok;
    }
    if (cfg->fan_mode == FAN_MODE_KERNEL) {
        if (temperature >= CUSTOM_CURVE_HARD_FULL_SPEED_C ||
            (temperature <= 0 && g_fan_automatic_hard_override)) {
            int ok = apply_datad_fan_pwm(255);
            if (ok) g_fan_automatic_hard_override = 1;
            return ok;
        }
        if (!set_fan_thermal_enabled(1) || !apply_fan_curve(cfg)) return 0;
        g_fan_automatic_hard_override = 0;
        return 1;
    }
    if (cfg->fan_mode == FAN_MODE_CUSTOM) {
        int ok = apply_custom_curve(cfg, temperature);
        if (ok) g_fan_automatic_hard_override = 0;
        return ok;
    }
    if (temperature >= CUSTOM_CURVE_HARD_FULL_SPEED_C) return apply_datad_fan_pwm(255);
    if (!prepare_fan_pwm_control()) return 0;
    if (set_fan_pwm_percent(cfg->fan_speed_percent)) return 1;
    (void)set_cooling_zone_mode(1);
    return 0;
}

void control_restore_cooling_state(void)
{
    struct cooling_config cfg;
    if (!load_cooling_config(&cfg)) return;
    (void)apply_fan_config(&cfg);
    (void)apply_liquid_switch(cfg.liquid_always_on, cfg.liquid_level);
    (void)set_vendor_switch_status("zwrt_deviceui.Device.fan_switch_status",
                                   cfg.fan_always_on);
    (void)set_vendor_switch_status("zwrt_deviceui.Device.liquid_cooling_switch_status",
                                   cfg.liquid_always_on);
    (void)save_cooling_config(&cfg);
}

void control_cooling_tick(long temperature_celsius)
{
    struct cooling_config cfg;
    int pwm;
    if (!load_cooling_config(&cfg)) return;
    if (cfg.liquid_always_on) (void)apply_liquid_switch(1, cfg.liquid_level);
    if (!cfg.fan_always_on && cfg.fan_mode == FAN_MODE_KERNEL) {
        if (temperature_celsius >= CUSTOM_CURVE_HARD_FULL_SPEED_C) {
            if (apply_datad_fan_pwm(255))
                g_fan_automatic_hard_override = 1;
        } else if (temperature_celsius > 0 && g_fan_automatic_hard_override) {
            if (set_fan_thermal_enabled(1) && apply_fan_curve(&cfg))
                g_fan_automatic_hard_override = 0;
        }
        return;
    }
    g_fan_automatic_hard_override = 0;
    if (temperature_celsius >= CUSTOM_CURVE_HARD_FULL_SPEED_C) {
        pwm = 255;
    } else if (cfg.fan_always_on) {
        pwm = FAN_ALWAYS_ON_PWM;
    } else if (cfg.fan_mode == FAN_MODE_CUSTOM && temperature_celsius > 0) {
        pwm = custom_curve_pwm(&cfg, temperature_celsius);
    } else {
        return;
    }
    if (pwm < 0) return;
    /* Vendor sleep/wakeup events may re-enable the kernel zone or latch a
     * cooling state. Reassert userspace PWM ownership every sampling tick. */
    (void)apply_datad_fan_pwm(pwm);
}

void control_release_cooling_state(void)
{
    struct cooling_config cfg;
    if (!load_cooling_config(&cfg)) return;
    if (cfg.fan_mode != FAN_MODE_KERNEL || cfg.fan_always_on ||
        g_fan_automatic_hard_override) {
        (void)set_fan_thermal_enabled(1);
        (void)apply_fan_curve(&cfg);
        g_fan_automatic_hard_override = 0;
    }
    if (cfg.liquid_always_on) {
        (void)write_text_file(env_path("ZWRT_DATAD_LIQUID_DRIVE_PATH", LIQUID_DRIVE_DEFAULT),
                              "0 0 0");
        (void)set_liquid_thermal_enabled(1);
    }
    /* If datad is intentionally stopped, hand control back to the vendor
     * thermal driver. A restarted datad will immediately restore custom mode. */
}

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

static int control_wifi_dual_band_status(char *result, size_t result_len,
                                         char *err, size_t errlen)
{
    char current[CONTROL_UBUS_RESPONSE_MAX];
    int enabled;
    if (!ubus_call("zwrt_router.api", "router_get_wifi_isolate", "{}",
                   current, sizeof current, err, errlen)) return 0;
    enabled = json_get_int(current, "wifimain24_wifimain5_enable", 0) ? 1 : 0;
    snprintf(result, result_len,
             "{\"WiFiDualBandSupported\":\"1\",\"WiFiDualBandEnabled\":\"%d\",\"BandSteeringSwitch\":\"%d\"}",
             enabled, enabled);
    return 1;
}

static int control_wifi_set_dual_band(const char *params, char *result, size_t result_len,
                                      char *err, size_t errlen)
{
    char raw[32], current[CONTROL_UBUS_RESPONSE_MAX], override[128], args[CONTROL_UBUS_ARGS_MAX];
    int enabled;
    if (!param_value(params, "enabled", raw, sizeof raw)) {
        set_invalid_error(err, errlen, "missing parameter: enabled");
        return 0;
    }
    if (!strcmp(raw, "1") || !strcmp(raw, "true")) enabled = 1;
    else if (!strcmp(raw, "0") || !strcmp(raw, "false")) enabled = 0;
    else {
        set_invalid_error(err, errlen, "enabled must be boolean");
        return 0;
    }
    if (!ubus_call("zwrt_router.api", "router_get_wifi_isolate", "{}",
                   current, sizeof current, err, errlen)) return 0;
    snprintf(override, sizeof override, "{\"wifimain24_wifimain5_enable\":%d}", enabled);
    if (!json_merge_objects(current, override, args, sizeof args)) {
        snprintf(err, errlen, "invalid router_get_wifi_isolate response");
        return 0;
    }
    return ubus_call("zwrt_router.api", "router_set_wifi_isolate", args,
                     result, result_len, err, errlen);
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

struct wifi_power_band {
    const char *name;
    const char *section;
    int factory_limit_dbm;
};

static int wifi_power_model_supported(char *err, size_t errlen)
{
    char model[64] = "", hardware[128] = "";
    if (device_uci_get("zwrt_common_info.common_config.model_name",
                       model, sizeof model) == 0 && !strcmp(model, "MU5252"))
        return 1;
    if (device_uci_get("zwrt_common_info.common_config.hardware_version",
                       hardware, sizeof hardware) == 0 &&
        !strncmp(hardware, "MU5252_", strlen("MU5252_")))
        return 1;
    set_invalid_error(err, errlen, "wifi power control is only supported on MU5252");
    return 0;
}

static const struct wifi_power_band *wifi_power_band_from_params(const char *params,
                                                                  char *err, size_t errlen)
{
    static const struct wifi_power_band bands[] = {
        {"2g", "wifi0", 19},
        {"5g", "wifi1", 18}
    };
    char band[16];
    if (!param_value(params, "band", band, sizeof band)) {
        set_invalid_error(err, errlen, "missing parameter: band");
        return NULL;
    }
    for (size_t i = 0; i < sizeof bands / sizeof bands[0]; i++)
        if (!strcmp(band, bands[i].name)) return &bands[i];
    set_invalid_error(err, errlen, "band must be 2g or 5g");
    return NULL;
}

static int wifi_power_read_int(const char *section, const char *option, long *value)
{
    char path[96], raw[32];
    if (snprintf(path, sizeof path, "wireless.%s.%s", section, option) >= (int)sizeof path ||
        device_uci_get(path, raw, sizeof raw) != 0 || !normalized_int(raw, value))
        return 0;
    return 1;
}

static int control_wifi_power_status(char *result, size_t result_len,
                                     char *err, size_t errlen)
{
    static const struct wifi_power_band bands[] = {
        {"2g", "wifi0", 19},
        {"5g", "wifi1", 18}
    };
    struct json_buf b = {result, result_len, 0};
    if (!wifi_power_model_supported(err, errlen)) return 0;
    jb_add(&b, "{");
    for (size_t i = 0; i < sizeof bands / sizeof bands[0]; i++) {
        long percent, txpower, max_power, disabled;
        if (!wifi_power_read_int(bands[i].section, "txpowerpercent", &percent) ||
            !wifi_power_read_int(bands[i].section, "txpower", &txpower) ||
            !wifi_power_read_int(bands[i].section, "max_power", &max_power) ||
            !wifi_power_read_int(bands[i].section, "disabled", &disabled)) {
            snprintf(err, errlen, "failed to read %s wifi power configuration", bands[i].name);
            return 0;
        }
        if (i) jb_add(&b, ",");
        jb_string(&b, bands[i].name);
        jb_add(&b,
               ":{\"enabled\":%s,\"percent\":%ld,\"txpower_dbm\":%ld,"
               "\"limit_dbm\":%ld,\"factory_limit_dbm\":%d}",
               disabled ? "false" : "true", percent, txpower, max_power,
               bands[i].factory_limit_dbm);
    }
    jb_add(&b, "}");
    if (b.len + 1 >= b.cap) {
        snprintf(err, errlen, "wifi power status response too large");
        return 0;
    }
    return 1;
}

static int wifi_power_restore_committed(const struct wifi_power_band *band,
                                        long old_percent, long old_txpower, long old_limit)
{
    char path[96], value[32];
    const char *options[] = {"txpowerpercent", "txpower", "max_power"};
    long values[] = {old_percent, old_txpower, old_limit};
    for (size_t i = 0; i < sizeof options / sizeof options[0]; i++) {
        snprintf(path, sizeof path, "wireless.%s.%s", band->section, options[i]);
        snprintf(value, sizeof value, "%ld", values[i]);
        if (device_uci_set(path, value) != 0) return 0;
    }
    if (device_uci_commit("wireless") != 0) return 0;
    return device_wifi_reload() == 0;
}

static int control_wifi_power_set(const char *action, const char *params,
                                  char *result, size_t result_len,
                                  char *err, size_t errlen)
{
    const struct wifi_power_band *band = wifi_power_band_from_params(params, err, errlen);
    char raw[32], path[96], value[32];
    long new_percent, new_limit, old_percent, old_txpower, old_limit;
    int set_percent = !strcmp(action, "wifi.txpower.set_percent");
    int restore_limit = !strcmp(action, "wifi.txpower.restore_limit");
    int apply = !strcmp(action, "wifi.txpower.apply");
    int percent_present = 0, limit_present = 0;
    int percent_changed, limit_changed;
    if (!wifi_power_model_supported(err, errlen)) return 0;
    if (!band) return 0;
    if (!wifi_power_read_int(band->section, "txpowerpercent", &old_percent) ||
        !wifi_power_read_int(band->section, "txpower", &old_txpower) ||
        !wifi_power_read_int(band->section, "max_power", &old_limit)) {
        snprintf(err, errlen, "failed to read existing wifi power configuration");
        return 0;
    }

    new_percent = old_percent;
    new_limit = old_limit;
    if (apply) {
        percent_present = json_get(params, "percent", raw, sizeof raw);
        if (percent_present && !normalized_int(raw, &new_percent)) {
            set_invalid_error(err, errlen, "invalid percent");
            return 0;
        }
        limit_present = json_get(params, "limit_dbm", raw, sizeof raw);
        if (limit_present && !normalized_int(raw, &new_limit)) {
            set_invalid_error(err, errlen, "invalid limit_dbm");
            return 0;
        }
        if (!percent_present && !limit_present) {
            set_invalid_error(err, errlen, "percent or limit_dbm is required");
            return 0;
        }
    } else if (restore_limit) {
        limit_present = 1;
        new_limit = band->factory_limit_dbm;
    } else if (!param_value(params, set_percent ? "percent" : "limit_dbm", raw, sizeof raw)) {
        set_invalid_error(err, errlen, "missing power value");
        return 0;
    } else if (set_percent) {
        percent_present = 1;
        if (!normalized_int(raw, &new_percent)) {
            set_invalid_error(err, errlen, "invalid percent");
            return 0;
        }
    } else {
        limit_present = 1;
        if (!normalized_int(raw, &new_limit)) {
            set_invalid_error(err, errlen, "invalid limit_dbm");
            return 0;
        }
    }
    if (percent_present &&
        (new_percent < 10 || new_percent > 100 || new_percent % 10 != 0)) {
        set_invalid_error(err, errlen, "percent must be 10 to 100 in steps of 10");
        return 0;
    }
    if (limit_present && (new_limit < 1 || new_limit > 30)) {
        set_invalid_error(err, errlen, "limit_dbm must be between 1 and 30");
        return 0;
    }

    percent_changed = percent_present && new_percent != old_percent;
    limit_changed = limit_present &&
        (new_limit != old_txpower || new_limit != old_limit);
    if (!percent_changed && !limit_changed) {
        snprintf(result, result_len,
                 "{\"band\":\"%s\",\"changed\":false,\"percent\":%ld,"
                 "\"txpower_dbm\":%ld,\"limit_dbm\":%ld,\"factory_limit_dbm\":%d}",
                 band->name, old_percent, old_txpower, old_limit, band->factory_limit_dbm);
        return 1;
    }

    if (percent_changed) {
        snprintf(value, sizeof value, "%ld", new_percent);
        snprintf(path, sizeof path, "wireless.%s.txpowerpercent", band->section);
        if (device_uci_set(path, value) != 0) goto stage_failed;
    }
    if (limit_changed) {
        snprintf(value, sizeof value, "%ld", new_limit);
        snprintf(path, sizeof path, "wireless.%s.txpower", band->section);
        if (device_uci_set(path, value) != 0) goto stage_failed;
        snprintf(path, sizeof path, "wireless.%s.max_power", band->section);
        if (device_uci_set(path, value) != 0) goto stage_failed;
    }
    if (device_uci_commit("wireless") != 0) {
        (void)device_uci_revert("wireless");
        snprintf(err, errlen, "failed to commit wifi power configuration");
        return 0;
    }
    if (device_wifi_reload() != 0) {
        int rolled_back = wifi_power_restore_committed(band, old_percent, old_txpower, old_limit);
        snprintf(err, errlen, "wifi reload failed; previous configuration %s",
                 rolled_back ? "restored" : "could not be restored");
        return 0;
    }
    snprintf(result, result_len,
             "{\"band\":\"%s\",\"changed\":true,\"percent\":%ld,"
             "\"txpower_dbm\":%ld,\"limit_dbm\":%ld,\"factory_limit_dbm\":%d}",
             band->name, percent_changed ? new_percent : old_percent,
             limit_changed ? new_limit : old_txpower,
             limit_changed ? new_limit : old_limit, band->factory_limit_dbm);
    return 1;

stage_failed:
    (void)device_uci_revert("wireless");
    snprintf(err, errlen, "failed to stage wifi power configuration");
    return 0;
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

static int required_bool_param(const char *params, const char *name, int *value,
                               char *err, size_t errlen)
{
    char raw[32];
    if (!param_value(params, name, raw, sizeof raw)) {
        set_invalid_error(err, errlen, "missing parameter: %s", name);
        return 0;
    }
    if (!strcmp(raw, "1") || !strcmp(raw, "true")) *value = 1;
    else if (!strcmp(raw, "0") || !strcmp(raw, "false")) *value = 0;
    else {
        set_invalid_error(err, errlen, "%s must be boolean", name);
        return 0;
    }
    return 1;
}

static int control_aggregation_set(const char *params, char *result, size_t result_len,
                                   char *err, size_t errlen)
{
    int enabled;
    char args[192], ignored[CONTROL_UBUS_RESPONSE_MAX];
    if (!required_bool_param(params, "enabled", &enabled, err, errlen)) return 0;
    ignored[0] = 0;
    if (enabled) {
        snprintf(args, sizeof args,
                 "{\"opms_wan_mode\":\"SMULTIWAN\",\"wan_ippass_device_type\":\"\","
                 "\"wan_ippass_device_mac\":\"\"}");
        if (device_ubus_call_raw("zwrt_router.api", "router_set_wan_mode", args,
                                 ignored, sizeof ignored) != 0) {
            snprintf(err, errlen, "zwrt_router.api.router_set_wan_mode failed");
            return 0;
        }
        {
            const char *init = env_path("ZWRT_DATAD_MWAN3_INIT", "/etc/init.d/mwan3");
            const char *argv[] = {init, "stop", NULL};
            (void)device_run_quiet(argv);
        }
    } else {
        snprintf(args, sizeof args, "{\"agg_mode_switch\":0}");
        if (device_ubus_call_raw("zwrt_router.api", "router_stop_agg_mode", args,
                                 ignored, sizeof ignored) != 0) {
            snprintf(err, errlen, "zwrt_router.api.router_stop_agg_mode failed");
            return 0;
        }
        snprintf(args, sizeof args,
                 "{\"opms_wan_mode\":\"MULTIWAN\",\"wan_ippass_device_type\":\"\","
                 "\"wan_ippass_device_mac\":\"\"}");
        if (device_ubus_call_raw("zwrt_router.api", "router_set_wan_mode", args,
                                 ignored, sizeof ignored) != 0) {
            snprintf(err, errlen, "aggregation stopped but MULTIWAN mode switch failed");
            return 0;
        }
        {
            const char *init = env_path("ZWRT_DATAD_MWAN3_INIT", "/etc/init.d/mwan3");
            const char *argv[] = {init, "restart", NULL};
            if (device_run_quiet(argv) != 0) {
                snprintf(err, errlen, "aggregation disabled but mwan3 restart failed");
                return 0;
            }
        }
    }
    snprintf(result, result_len, "{\"enabled\":%s}", enabled ? "true" : "false");
    return 1;
}

static int valid_uci_section_name(const char *value)
{
    if (!value || !*value || strlen(value) > 63) return 0;
    for (; *value; value++)
        if (!isalnum((unsigned char)*value) && *value != '_' && *value != '-') return 0;
    return 1;
}

static int mwan3_section_is(const char *section, const char *type)
{
    char path[96], actual[64];
    if (!valid_uci_section_name(section)) return 0;
    snprintf(path, sizeof path, "mwan3.%s", section);
    return device_uci_get(path, actual, sizeof actual) == 0 && !strcmp(actual, type);
}

static int set_mwan3_option(const char *section, const char *option, const char *value,
                            char *err, size_t errlen)
{
    char path[160];
    snprintf(path, sizeof path, "mwan3.%s.%s", section, option);
    if (device_uci_set(path, value) == 0) return 1;
    snprintf(err, errlen, "failed to set mwan3 option: %s", option);
    return 0;
}

static int set_mwan3_int_option(const char *params, const char *section,
                                const char *option, long minimum, long maximum,
                                int *changed, char *err, size_t errlen)
{
    char raw[64], normalized[32];
    long value;
    if (!json_get(params, option, raw, sizeof raw)) return 1;
    if (!normalized_int(raw, &value) || value < minimum || value > maximum) {
        set_invalid_error(err, errlen, "%s must be between %ld and %ld",
                          option, minimum, maximum);
        return 0;
    }
    snprintf(normalized, sizeof normalized, "%ld", value);
    if (!set_mwan3_option(section, option, normalized, err, errlen)) return 0;
    *changed = 1;
    return 1;
}

static int replace_mwan3_list(const char *section, const char *option, const char *csv,
                              const char *item_type, char *err, size_t errlen)
{
    char copy[4096], path[160], *save = NULL, *token;
    int count = 0;
    if (strlen(csv) >= sizeof copy) {
        set_invalid_error(err, errlen, "%s is too long", option);
        return 0;
    }
    strcpy(copy, csv);
    snprintf(path, sizeof path, "mwan3.%s.%s", section, option);
    (void)device_uci_delete(path);
    for (token = strtok_r(copy, ", \t\r\n", &save); token;
         token = strtok_r(NULL, ", \t\r\n", &save)) {
        if (++count > 16) {
            set_invalid_error(err, errlen, "too many %s entries", option);
            return 0;
        }
        if (!strcmp(item_type, "address")) {
            unsigned char address[sizeof(struct in6_addr)];
            if (inet_pton(AF_INET, token, address) != 1 &&
                inet_pton(AF_INET6, token, address) != 1) {
                set_invalid_error(err, errlen, "invalid tracking address");
                return 0;
            }
        } else if (!mwan3_section_is(token, item_type)) {
            set_invalid_error(err, errlen, "unknown mwan3 %s: %s", item_type, token);
            return 0;
        }
        if (device_uci_list("add_list", path, token) != 0) {
            snprintf(err, errlen, "failed to update mwan3 list: %s", option);
            return 0;
        }
    }
    return 1;
}

static int apply_mwan3_if_active(int *applied)
{
    char mode[32];
    const char *init, *argv[3];
    *applied = 0;
    if (device_uci_get("zwrt_router.network.opms_wan_mode", mode, sizeof mode) != 0 ||
        strcmp(mode, "MULTIWAN")) return 1;
    init = env_path("ZWRT_DATAD_MWAN3_INIT", "/etc/init.d/mwan3");
    argv[0] = init; argv[1] = "restart"; argv[2] = NULL;
    if (device_run_quiet(argv) != 0) return 0;
    *applied = 1;
    return 1;
}

static int finish_mwan3_update(const char *section, int changed,
                               char *result, size_t result_len,
                               char *err, size_t errlen)
{
    int applied;
    if (!changed) {
        set_invalid_error(err, errlen, "no mwan3 fields supplied");
        return 0;
    }
    if (device_uci_commit("mwan3") != 0) {
        (void)device_uci_revert("mwan3");
        snprintf(err, errlen, "failed to commit mwan3 configuration");
        return 0;
    }
    if (!apply_mwan3_if_active(&applied)) {
        snprintf(err, errlen, "mwan3 configuration committed but restart failed");
        return 0;
    }
    snprintf(result, result_len, "{\"section\":\"%s\",\"applied\":%s}",
             section, applied ? "true" : "false");
    return 1;
}

static int control_multiwan_interface_set(const char *params, char *result, size_t result_len,
                                          char *err, size_t errlen)
{
    char section[64], track_ip[4096], method[32];
    int changed = 0;
    static const struct { const char *name; long min, max; } fields[] = {
        {"enabled", 0, 1}, {"reliability", 0, 16}, {"count", 1, 16},
        {"size", 1, 4096}, {"max_ttl", 1, 255}, {"check_quality", 0, 1},
        {"timeout", 1, 60}, {"interval", 1, 3600},
        {"failure_interval", 1, 3600}, {"recovery_interval", 1, 3600},
        {"down", 1, 100}, {"up", 1, 100}
    };
    if (!param_value(params, "section", section, sizeof section) ||
        !mwan3_section_is(section, "interface")) {
        set_invalid_error(err, errlen, "unknown mwan3 interface");
        return 0;
    }
    for (size_t i = 0; i < sizeof fields / sizeof fields[0]; i++)
        if (!set_mwan3_int_option(params, section, fields[i].name, fields[i].min,
                                  fields[i].max, &changed, err, errlen)) goto fail;
    if (json_get(params, "track_method", method, sizeof method)) {
        if (strcmp(method, "ping")) {
            set_invalid_error(err, errlen, "only ping tracking is supported");
            goto fail;
        }
        if (!set_mwan3_option(section, "track_method", method, err, errlen)) goto fail;
        changed = 1;
    }
    if (json_get(params, "track_ip", track_ip, sizeof track_ip)) {
        if (!replace_mwan3_list(section, "track_ip", track_ip, "address", err, errlen)) goto fail;
        changed = 1;
    }
    return finish_mwan3_update(section, changed, result, result_len, err, errlen);
fail:
    (void)device_uci_revert("mwan3");
    return 0;
}

static int control_multiwan_member_set(const char *params, char *result, size_t result_len,
                                       char *err, size_t errlen)
{
    char section[64]; int changed = 0;
    if (!param_value(params, "section", section, sizeof section) ||
        !mwan3_section_is(section, "member")) {
        set_invalid_error(err, errlen, "unknown mwan3 member"); return 0;
    }
    if (!set_mwan3_int_option(params, section, "metric", 1, 65535, &changed, err, errlen) ||
        !set_mwan3_int_option(params, section, "weight", 1, 1000, &changed, err, errlen)) {
        (void)device_uci_revert("mwan3"); return 0;
    }
    return finish_mwan3_update(section, changed, result, result_len, err, errlen);
}

static int control_multiwan_policy_set(const char *params, char *result, size_t result_len,
                                       char *err, size_t errlen)
{
    char section[64], members[4096], last[32]; int changed = 0;
    if (!param_value(params, "section", section, sizeof section) ||
        !mwan3_section_is(section, "policy")) {
        set_invalid_error(err, errlen, "unknown mwan3 policy"); return 0;
    }
    if (json_get(params, "last_resort", last, sizeof last)) {
        if (strcmp(last, "default") && strcmp(last, "unreachable") && strcmp(last, "blackhole")) {
            set_invalid_error(err, errlen, "invalid last_resort"); goto fail;
        }
        if (!set_mwan3_option(section, "last_resort", last, err, errlen)) goto fail;
        changed = 1;
    }
    if (json_get(params, "use_member", members, sizeof members)) {
        if (!replace_mwan3_list(section, "use_member", members, "member", err, errlen)) goto fail;
        changed = 1;
    }
    return finish_mwan3_update(section, changed, result, result_len, err, errlen);
fail:
    (void)device_uci_revert("mwan3"); return 0;
}

static int control_multiwan_rule_set(const char *params, char *result, size_t result_len,
                                     char *err, size_t errlen)
{
    char section[64], policy[64]; int changed = 0;
    if (!param_value(params, "section", section, sizeof section) ||
        !mwan3_section_is(section, "rule")) {
        set_invalid_error(err, errlen, "unknown mwan3 rule"); return 0;
    }
    if (json_get(params, "use_policy", policy, sizeof policy)) {
        if (!mwan3_section_is(policy, "policy")) {
            set_invalid_error(err, errlen, "unknown mwan3 policy"); goto fail;
        }
        if (!set_mwan3_option(section, "use_policy", policy, err, errlen)) goto fail;
        changed = 1;
    }
    if (!set_mwan3_int_option(params, section, "sticky", 0, 1, &changed, err, errlen) ||
        !set_mwan3_int_option(params, section, "logging", 0, 1, &changed, err, errlen)) goto fail;
    return finish_mwan3_update(section, changed, result, result_len, err, errlen);
fail:
    (void)device_uci_revert("mwan3"); return 0;
}

static const char *fan_mode_name(const struct cooling_config *cfg)
{
    if (cfg->fan_always_on) return "always_on";
    if (cfg->fan_mode == FAN_MODE_KERNEL) return "automatic";
    if (cfg->fan_mode == FAN_MODE_CUSTOM) return "custom";
    return "manual";
}

static int control_set_fan_mode(struct cooling_config *cfg, const char *mode,
                                char *result, size_t result_len,
                                char *err, size_t errlen)
{
    if (!strcmp(mode, "automatic")) {
        cfg->fan_always_on = 0;
        cfg->fan_mode = FAN_MODE_KERNEL;
    } else if (!strcmp(mode, "custom")) {
        cfg->fan_always_on = 0;
        cfg->fan_mode = FAN_MODE_CUSTOM;
    } else if (!strcmp(mode, "always_on")) {
        cfg->fan_always_on = 1;
    } else {
        set_invalid_error(err, errlen,
                          "mode must be automatic, custom, or always_on");
        return 0;
    }
    cfg->fan_enabled = 1;
    if (!apply_fan_config(cfg)) {
        snprintf(err, errlen, "fan control is unavailable");
        return 0;
    }
    if (!set_vendor_switch_status("zwrt_deviceui.Device.fan_switch_status",
                                  cfg->fan_always_on) ||
        !save_cooling_config(cfg)) {
        snprintf(err, errlen, "failed to persist fan state");
        return 0;
    }
    snprintf(result, result_len,
             "{\"enabled\":%s,\"always_on\":%s,\"mode\":\"%s\"}",
             cfg->fan_always_on ? "true" : "false",
             cfg->fan_always_on ? "true" : "false", fan_mode_name(cfg));
    return 1;
}

static int control_fan_enabled(const char *params, char *result, size_t result_len,
                               char *err, size_t errlen)
{
    struct cooling_config cfg;
    int enabled;
    (void)load_cooling_config(&cfg);
    if (!required_bool_param(params, "enabled", &enabled, err, errlen)) return 0;
    return control_set_fan_mode(&cfg, enabled ? "always_on" : "custom",
                                result, result_len, err, errlen);
}

static int control_fan_mode(const char *params, char *result, size_t result_len,
                            char *err, size_t errlen)
{
    struct cooling_config cfg;
    char mode[24];
    (void)load_cooling_config(&cfg);
    if (!param_value(params, "mode", mode, sizeof mode)) {
        set_invalid_error(err, errlen, "missing parameter: mode");
        return 0;
    }
    return control_set_fan_mode(&cfg, mode, result, result_len, err, errlen);
}

static int parse_custom_curve(const char *params, struct cooling_config *cfg,
                              char *err, size_t errlen)
{
    char array[4096];
    const char *p;
    int count = 0;
    if (!json_get(params, "points", array, sizeof array) || array[0] != '[') {
        set_invalid_error(err, errlen, "points must be an array");
        return 0;
    }
    p = array + 1;
    while (*p) {
        const char *start;
        int depth = 0, in_string = 0, escaped = 0;
        char object[256];
        size_t len;
        long temperature, pwm;
        while (*p && isspace((unsigned char)*p)) p++;
        if (*p == ',') { p++; continue; }
        if (*p == ']') break;
        if (*p != '{' || count >= CUSTOM_CURVE_MAX_POINTS) {
            set_invalid_error(err, errlen, "points must contain 2 to %d objects",
                              CUSTOM_CURVE_MAX_POINTS);
            return 0;
        }
        start = p;
        for (; *p; p++) {
            char c = *p;
            if (in_string) {
                if (escaped) escaped = 0;
                else if (c == '\\') escaped = 1;
                else if (c == '"') in_string = 0;
                continue;
            }
            if (c == '"') in_string = 1;
            else if (c == '{') depth++;
            else if (c == '}' && --depth == 0) { p++; break; }
        }
        if (depth != 0 || (len = (size_t)(p - start)) >= sizeof object) {
            set_invalid_error(err, errlen, "invalid curve point object");
            return 0;
        }
        memcpy(object, start, len);
        object[len] = 0;
        temperature = json_get_int(object, "temperature", LONG_MIN);
        pwm = json_get_int(object, "pwm", LONG_MIN);
        if (temperature < 20 || temperature > CUSTOM_CURVE_HARD_FULL_SPEED_C ||
            pwm < 0 || pwm > 255) {
            set_invalid_error(err, errlen,
                              "curve temperature must be 20..%d and pwm must be 0..255",
                              CUSTOM_CURVE_HARD_FULL_SPEED_C);
            return 0;
        }
        if (count > 0 &&
            (temperature <= cfg->custom_curve[count - 1].temperature ||
             pwm < cfg->custom_curve[count - 1].pwm)) {
            set_invalid_error(err, errlen,
                              "curve temperatures must increase and pwm must not decrease");
            return 0;
        }
        cfg->custom_curve[count].temperature = (int)temperature;
        cfg->custom_curve[count].pwm = (int)pwm;
        count++;
    }
    if (count < 2) {
        set_invalid_error(err, errlen, "curve requires at least 2 points");
        return 0;
    }
    cfg->custom_curve_count = count;
    return 1;
}

static int control_fan_curve(const char *params, char *result, size_t result_len,
                             char *err, size_t errlen)
{
    struct cooling_config cfg;
    struct json_buf response = {result, result_len, 0};
    (void)load_cooling_config(&cfg);
    if (!parse_custom_curve(params, &cfg, err, errlen)) return 0;
    cfg.fan_mode = FAN_MODE_CUSTOM;
    cfg.fan_enabled = 1;
    cfg.fan_always_on = 0;
    if (!apply_fan_config(&cfg)) {
        snprintf(err, errlen, "custom fan curve is unavailable");
        return 0;
    }
    if (!set_vendor_switch_status("zwrt_deviceui.Device.fan_switch_status", 0) ||
        !save_cooling_config(&cfg)) {
        snprintf(err, errlen, "failed to persist fan curve");
        return 0;
    }
    result[0] = 0;
    jb_add(&response,
           "{\"enabled\":false,\"always_on\":false,\"mode\":\"custom\",\"points\":[");
    for (int i = 0; i < cfg.custom_curve_count; i++) {
        if (i) jb_add(&response, ",");
        jb_add(&response, "{\"temperature\":%d,\"pwm\":%d}",
               cfg.custom_curve[i].temperature, cfg.custom_curve[i].pwm);
    }
    jb_add(&response, "]}");
    return response.len + 1 < response.cap;
}

static int control_liquid_enabled(const char *params, char *result, size_t result_len,
                                  char *err, size_t errlen)
{
    struct cooling_config cfg;
    int enabled;
    (void)load_cooling_config(&cfg);
    if (!required_bool_param(params, "enabled", &enabled, err, errlen)) return 0;
    cfg.liquid_always_on = enabled;
    if (!apply_liquid_switch(enabled, cfg.liquid_level)) {
        snprintf(err, errlen, "liquid cooling control is unavailable");
        return 0;
    }
    if (!set_vendor_switch_status("zwrt_deviceui.Device.liquid_cooling_switch_status", enabled) ||
        !save_cooling_config(&cfg)) {
        snprintf(err, errlen, "failed to persist liquid cooling state");
        return 0;
    }
    snprintf(result, result_len, "{\"enabled\":%s,\"always_on\":%s}",
             enabled ? "true" : "false", enabled ? "true" : "false");
    return 1;
}

static int control_liquid_mode(const char *params, char *result, size_t result_len,
                               char *err, size_t errlen)
{
    struct cooling_config cfg;
    char mode[24];
    (void)load_cooling_config(&cfg);
    if (!param_value(params, "mode", mode, sizeof mode)) {
        set_invalid_error(err, errlen, "mode must be automatic, low, or high");
        return 0;
    }
    if (!strcmp(mode, "automatic")) {
        cfg.liquid_always_on = 0;
    } else if (!strcmp(mode, "low")) {
        cfg.liquid_always_on = 1;
        cfg.liquid_level = 1;
    } else if (!strcmp(mode, "high")) {
        cfg.liquid_always_on = 1;
        cfg.liquid_level = 2;
    } else {
        set_invalid_error(err, errlen, "mode must be automatic, low, or high");
        return 0;
    }
    if (!apply_liquid_switch(cfg.liquid_always_on, cfg.liquid_level)) {
        snprintf(err, errlen, "liquid cooling control is unavailable");
        return 0;
    }
    if (!set_vendor_switch_status("zwrt_deviceui.Device.liquid_cooling_switch_status",
                                  cfg.liquid_always_on) ||
        !save_cooling_config(&cfg)) {
        snprintf(err, errlen, "failed to persist liquid cooling mode");
        return 0;
    }
    snprintf(result, result_len,
             "{\"enabled\":%s,\"always_on\":%s,\"mode\":\"%s\",\"level\":%d,\"amplitude\":%d,\"speed_percent\":%d}",
             cfg.liquid_always_on ? "true" : "false",
             cfg.liquid_always_on ? "true" : "false",
             cfg.liquid_always_on ? (cfg.liquid_level >= 2 ? "high" : "low") : "automatic",
             cfg.liquid_always_on ? cfg.liquid_level : 0,
             cfg.liquid_always_on ? (cfg.liquid_level >= 2 ? LIQUID_HIGH_AMPLITUDE : LIQUID_LOW_AMPLITUDE) : 0,
             cfg.liquid_always_on ? (cfg.liquid_level >= 2 ? 100 : 30) : 0);
    return 1;
}

static int control_state_set_interval(const char *params, char *result, size_t result_len,
                                      char *err, size_t errlen)
{
    long milliseconds = json_get_int(params, "milliseconds", LONG_MIN);
    if (milliseconds < 500 || milliseconds > 5000) {
        set_invalid_error(err, errlen, "milliseconds must be between 500 and 5000");
        return 0;
    }
    g_requested_interval_ms = (sig_atomic_t)milliseconds;
    snprintf(result, result_len, "{\"sample_interval_ms\":%ld}", milliseconds);
    return 1;
}

int control_take_requested_interval_ms(void)
{
    int value = (int)g_requested_interval_ms;
    g_requested_interval_ms = 0;
    return value;
}

const char *control_capabilities_json(void)
{
    return
        "{\"schema_version\":1,\"control\":["
        "\"device.login_info\",\"device.login\",\"device.logout\",\"device.session_status\",\"device.change_password\","
        "\"device.reboot\",\"device.poweroff\","
        "\"cellular.connect\",\"cellular.disconnect\",\"cellular.set\","
        "\"network.set_mode\",\"band.set_lte\",\"band.set_nr_sa\",\"band.set_nr_nsa\","
        "\"cell.lock_lte\",\"cell.lock_nr\",\"cell.unlock_all\",\"sim.set_slot\","
        "\"wifi.status\",\"wifi.dual_band_status\",\"wifi.set_dual_band\",\"wifi.set_module\",\"wifi.set_chip\",\"wifi.configure\","
        "\"wifi.txpower.status\",\"wifi.txpower.apply\",\"wifi.txpower.set_percent\",\"wifi.txpower.set_limit\",\"wifi.txpower.restore_limit\","
        "\"lan.set\",\"lan.set_mtu\",\"dns.set\","
        "\"usb.status\",\"usb.set\",\"sleep.status\",\"sleep.set\",\"nfc.set\","
        "\"apn.list\",\"apn.set_mode\",\"apn.add\",\"apn.modify\",\"apn.delete\",\"apn.enable\","
        "\"traffic.set_limit\",\"traffic.set_clear_day\",\"traffic.calibrate\","
        "\"sms.send_raw\",\"sms.delete\",\"sms.mark_read\","
        "\"client.access\",\"client.block\",\"client.unblock\",\"client.kick\",\"client.rename\","
        "\"aggregation.set\",\"multiwan.interface.set\",\"multiwan.member.set\",\"multiwan.policy.set\",\"multiwan.rule.set\","
        "\"cooling.fan.set_enabled\",\"cooling.fan.set_mode\",\"cooling.fan.set_curve\",\"cooling.liquid.set_enabled\",\"cooling.liquid.set_mode\","
        "\"state.refresh\",\"state.set_interval\",\"qos.reload\",\"qos.clear\"],"
        "\"events\":[\"state\"],"
        "\"discovery\":[\"ubus.list\",\"ubus.list_verbose\"],"
        "\"passthrough\":[\"ubus.call\"],"
        "\"transport\":[\"http\",\"sse\"]}";
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

    if (!strcmp(action, "device.login_info")) {
        ok = simple_fixed_call("zwrt_web", "web_login_info", "{}",
                               result, sizeof result, err, sizeof err);
        status.refresh_state = 0;
    } else if (!strcmp(action, "device.login")) {
        ok = control_login(params, result, sizeof result, err, sizeof err);
        status.refresh_state = 0;
    } else if (!strcmp(action, "device.logout")) {
        g_device_session[0] = g_device_password_hash[0] = 0;
        snprintf(result, sizeof result, "{\"logged_in\":false}");
        ok = 1;
        status.refresh_state = 0;
    } else if (!strcmp(action, "device.session_status")) {
        snprintf(result, sizeof result, "{\"logged_in\":%s}", g_device_session[0] ? "true" : "false");
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
    } else if (!strcmp(action, "wifi.dual_band_status")) {
        ok = control_wifi_dual_band_status(result, sizeof result, err, sizeof err);
        status.refresh_state = 0;
    } else if (!strcmp(action, "wifi.set_dual_band")) {
        ok = control_wifi_set_dual_band(params, result, sizeof result, err, sizeof err);
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
    } else if (!strcmp(action, "wifi.txpower.status")) {
        ok = control_wifi_power_status(result, sizeof result, err, sizeof err);
        status.refresh_state = 0;
    } else if (!strcmp(action, "wifi.txpower.apply") ||
               !strcmp(action, "wifi.txpower.set_percent") ||
               !strcmp(action, "wifi.txpower.set_limit") ||
               !strcmp(action, "wifi.txpower.restore_limit")) {
        ok = control_wifi_power_set(action, params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "lan.set")) {
        ok = control_lan(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "lan.set_mtu")) {
        static const struct param_spec specs[] = {{"mtu", "wan_mtu", PARAM_STRING, 1}};
        ok = call_specs(params, "zwrt_router.api", "router_set_wan_mtu", specs, 1,
                        result, sizeof result, err, sizeof err);
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
    } else if (!strcmp(action, "aggregation.set")) {
        ok = control_aggregation_set(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "multiwan.interface.set")) {
        ok = control_multiwan_interface_set(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "multiwan.member.set")) {
        ok = control_multiwan_member_set(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "multiwan.policy.set")) {
        ok = control_multiwan_policy_set(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "multiwan.rule.set")) {
        ok = control_multiwan_rule_set(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "cooling.fan.set_enabled")) {
        ok = control_fan_enabled(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "cooling.fan.set_mode")) {
        ok = control_fan_mode(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "cooling.fan.set_curve")) {
        ok = control_fan_curve(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "cooling.liquid.set_enabled")) {
        ok = control_liquid_enabled(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "cooling.liquid.set_mode")) {
        ok = control_liquid_mode(params, result, sizeof result, err, sizeof err);
    } else if (!strcmp(action, "state.refresh")) {
        snprintf(result, sizeof result, "{\"queued\":true}");
        ok = 1;
    } else if (!strcmp(action, "state.set_interval")) {
        ok = control_state_set_interval(params, result, sizeof result, err, sizeof err);
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
