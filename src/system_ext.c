/* SPDX-License-Identifier: MIT */
#include "system_ext.h"

#include <ctype.h>
#include <dirent.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/statvfs.h>
#include <time.h>

#define CPU_MAX 32
#define THERMAL_MAX 64
#define SPEED_RING 16

struct cpu_counter {
    unsigned long long total;
    unsigned long long idle;
    int valid;
};

struct thermal_zone {
    char type[96];
    long temp_milli;
};

struct speed_sample {
    unsigned long long rx;
    unsigned long long tx;
    long long at_ms;
};

struct mem_values {
    unsigned long long total_kb, free_kb, available_kb;
    unsigned long long buffers_kb, cached_kb;
    unsigned long long swap_total_kb, swap_free_kb;
};

struct json_buf { char *p; size_t cap; size_t len; };

static struct cpu_counter g_cpu_prev[CPU_MAX + 1];
static struct speed_sample g_speed[SPEED_RING];
static size_t g_speed_count;

static void add(struct json_buf *b, const char *fmt, ...)
{
    va_list ap;
    int n;
    size_t room;
    if (b->len >= b->cap) return;
    room = b->cap - b->len;
    va_start(ap, fmt);
    n = vsnprintf(b->p + b->len, room, fmt, ap);
    va_end(ap);
    if (n <= 0) return;
    if ((size_t)n >= room) {
        b->len = b->cap - 1;
        b->p[b->len] = 0;
    } else b->len += (size_t)n;
}

static void add_string(struct json_buf *b, const char *s)
{
    add(b, "\"");
    if (!s) s = "";
    for (; *s; s++) {
        unsigned char c = (unsigned char)*s;
        if (c == '"' || c == '\\') add(b, "\\%c", c);
        else if (c < 0x20) add(b, " ");
        else add(b, "%c", c);
    }
    add(b, "\"");
}

static long long now_ms(void)
{
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) return 0;
    return (long long)ts.tv_sec * 1000LL + ts.tv_nsec / 1000000LL;
}

static int read_ull(const char *path, unsigned long long *out)
{
    FILE *fp = fopen(path, "r");
    int ok;
    if (!fp) return 0;
    ok = fscanf(fp, "%llu", out) == 1;
    fclose(fp);
    return ok;
}

static int sample_cpu(int usage[CPU_MAX], int *core_count)
{
    FILE *fp = fopen("/proc/stat", "r");
    char line[512];
    int total_usage = -1;
    int count = 0;
    if (!fp) { *core_count = 0; return -1; }

    while (fgets(line, sizeof line, fp)) {
        char label[32];
        unsigned long long user = 0, nice = 0, sys = 0, idle = 0;
        unsigned long long iowait = 0, irq = 0, softirq = 0, steal = 0;
        unsigned long long total, idle_all, dt, di;
        int idx;
        if (sscanf(line, "%31s %llu %llu %llu %llu %llu %llu %llu %llu",
                   label, &user, &nice, &sys, &idle, &iowait, &irq, &softirq, &steal) < 5)
            continue;
        if (!strcmp(label, "cpu")) idx = 0;
        else if (!strncmp(label, "cpu", 3) && isdigit((unsigned char)label[3])) {
            long core = strtol(label + 3, NULL, 10);
            if (core < 0 || core >= CPU_MAX) continue;
            idx = (int)core + 1;
            if ((int)core + 1 > count) count = (int)core + 1;
        } else continue;

        idle_all = idle + iowait;
        total = user + nice + sys + idle + iowait + irq + softirq + steal;
        if (g_cpu_prev[idx].valid && total >= g_cpu_prev[idx].total && idle_all >= g_cpu_prev[idx].idle) {
            dt = total - g_cpu_prev[idx].total;
            di = idle_all - g_cpu_prev[idx].idle;
            if (dt > 0) {
                int pct = (int)(((dt - di) * 1000ULL) / dt);
                if (idx == 0) total_usage = pct;
                else usage[idx - 1] = pct;
            }
        }
        g_cpu_prev[idx].total = total;
        g_cpu_prev[idx].idle = idle_all;
        g_cpu_prev[idx].valid = 1;
    }
    fclose(fp);
    *core_count = count;
    return total_usage;
}

static unsigned long long cpu_freq_khz(int core, const char *name)
{
    char path[192];
    unsigned long long value = 0;
    snprintf(path, sizeof path, "/sys/devices/system/cpu/cpu%d/cpufreq/%s", core, name);
    (void)read_ull(path, &value);
    return value;
}

static int read_thermal(struct thermal_zone zones[THERMAL_MAX])
{
    DIR *dir = opendir("/sys/class/thermal");
    struct dirent *ent;
    int count = 0;
    if (!dir) return 0;
    while ((ent = readdir(dir)) != NULL && count < THERMAL_MAX) {
        char path[256];
        FILE *fp;
        long temp;
        size_t n;
        if (strncmp(ent->d_name, "thermal_zone", 12)) continue;
        int path_len = snprintf(path, sizeof path, "/sys/class/thermal/%s/type", ent->d_name);
        if (path_len < 0 || (size_t)path_len >= sizeof path) continue;
        fp = fopen(path, "r");
        if (!fp) continue;
        if (!fgets(zones[count].type, sizeof zones[count].type, fp)) {
            fclose(fp); continue;
        }
        fclose(fp);
        n = strlen(zones[count].type);
        while (n && isspace((unsigned char)zones[count].type[n - 1])) zones[count].type[--n] = 0;
        path_len = snprintf(path, sizeof path, "/sys/class/thermal/%s/temp", ent->d_name);
        if (path_len < 0 || (size_t)path_len >= sizeof path) continue;
        fp = fopen(path, "r");
        if (!fp) continue;
        if (fscanf(fp, "%ld", &temp) != 1) { fclose(fp); continue; }
        fclose(fp);
        if (temp > -40000 && temp != 0) {
            zones[count].temp_milli = temp;
            count++;
        }
    }
    closedir(dir);
    return count;
}

static void read_mem(struct mem_values *m)
{
    FILE *fp = fopen("/proc/meminfo", "r");
    char line[256];
    memset(m, 0, sizeof *m);
    if (!fp) return;
    while (fgets(line, sizeof line, fp)) {
        char key[64];
        unsigned long long value;
        if (sscanf(line, "%63[^:]: %llu", key, &value) != 2) continue;
        if (!strcmp(key, "MemTotal")) m->total_kb = value;
        else if (!strcmp(key, "MemFree")) m->free_kb = value;
        else if (!strcmp(key, "MemAvailable")) m->available_kb = value;
        else if (!strcmp(key, "Buffers")) m->buffers_kb = value;
        else if (!strcmp(key, "Cached")) m->cached_kb = value;
        else if (!strcmp(key, "SwapTotal")) m->swap_total_kb = value;
        else if (!strcmp(key, "SwapFree")) m->swap_free_kb = value;
    }
    fclose(fp);
}

static unsigned long long count_table(const char *path, int has_header)
{
    FILE *fp = fopen(path, "r");
    char line[512];
    unsigned long long count = 0;
    if (!fp) return 0;
    while (fgets(line, sizeof line, fp)) count++;
    fclose(fp);
    if (has_header && count) count--;
    return count;
}

static int iface_bytes(const char *name, unsigned long long *rx, unsigned long long *tx)
{
    char path[192];
    snprintf(path, sizeof path, "/sys/class/net/%s/statistics/rx_bytes", name);
    if (!read_ull(path, rx)) return 0;
    snprintf(path, sizeof path, "/sys/class/net/%s/statistics/tx_bytes", name);
    return read_ull(path, tx);
}

static void user_bytes(unsigned long long *rx, unsigned long long *tx)
{
    unsigned long long rb, tb, sum_r = 0, sum_t = 0;
    int hit = 0;
    if (iface_bytes("br-lan", &rb, &tb)) { *rx = tb; *tx = rb; return; }
    const char *wifi[] = {"wlan0", "wlan2"};
    for (size_t i = 0; i < sizeof wifi / sizeof wifi[0]; i++) {
        if (iface_bytes(wifi[i], &rb, &tb)) { sum_r += rb; sum_t += tb; hit = 1; }
    }
    if (hit) { *rx = sum_t; *tx = sum_r; return; }
    const char *rmnet[] = {"rmnet_data0", "rmnet_ipa0"};
    for (size_t i = 0; i < sizeof rmnet / sizeof rmnet[0]; i++) {
        if (iface_bytes(rmnet[i], &rb, &tb)) { sum_r += rb; sum_t += tb; }
    }
    *rx = sum_r; *tx = sum_t;
}

static void sample_speed(unsigned long long *rx_bps, unsigned long long *tx_bps, long long *window_ms)
{
    struct speed_sample sample;
    sample.at_ms = now_ms();
    user_bytes(&sample.rx, &sample.tx);
    if (g_speed_count == SPEED_RING) {
        memmove(&g_speed[0], &g_speed[1], sizeof g_speed[0] * (SPEED_RING - 1));
        g_speed_count--;
    }
    g_speed[g_speed_count++] = sample;
    *rx_bps = *tx_bps = 0;
    *window_ms = 0;
    if (g_speed_count >= 2) {
        struct speed_sample *old = &g_speed[0];
        struct speed_sample *cur = &g_speed[g_speed_count - 1];
        *window_ms = cur->at_ms - old->at_ms;
        if (*window_ms > 0) {
            if (cur->rx >= old->rx) *rx_bps = (cur->rx - old->rx) * 1000ULL / (unsigned long long)*window_ms;
            if (cur->tx >= old->tx) *tx_bps = (cur->tx - old->tx) * 1000ULL / (unsigned long long)*window_ms;
        }
    }
}

int system_ext_build_json(char *out, size_t outlen)
{
    struct json_buf b = {out, outlen, 0};
    int usage[CPU_MAX];
    int cores = 0;
    int total_usage;
    struct thermal_zone zones[THERMAL_MAX];
    int zone_count;
    struct mem_values mem;
    struct statvfs disk;
    unsigned long long rx_bps, tx_bps;
    long long speed_window_ms;
    memset(usage, 0xff, sizeof usage);
    out[0] = 0;

    total_usage = sample_cpu(usage, &cores);
    zone_count = read_thermal(zones);
    read_mem(&mem);
    sample_speed(&rx_bps, &tx_bps, &speed_window_ms);

    add(&b, "{\"cpu_usage_tenths\":%d,\"cpu_cores\":{", total_usage);
    for (int i = 0; i < cores; i++) {
        if (i) add(&b, ",");
        add(&b, "\"cpu%d\":%d", i, usage[i]);
    }
    add(&b, "},\"cpu_freq_mhz\":{");
    for (int i = 0; i < cores; i++) {
        unsigned long long cur = cpu_freq_khz(i, "scaling_cur_freq");
        unsigned long long max = cpu_freq_khz(i, "scaling_max_freq");
        if (!cur) cur = cpu_freq_khz(i, "cpuinfo_cur_freq");
        if (!max) max = cpu_freq_khz(i, "cpuinfo_max_freq");
        if (i) add(&b, ",");
        add(&b, "\"cpu%d\":{\"cur\":%llu,\"max\":%llu}", i, cur / 1000ULL, max / 1000ULL);
    }
    add(&b, "},\"thermal_zones\":[");
    for (int i = 0; i < zone_count; i++) {
        if (i) add(&b, ",");
        add(&b, "{\"type\":"); add_string(&b, zones[i].type);
        add(&b, ",\"temp_milli\":%ld}", zones[i].temp_milli);
    }
    add(&b, "],\"memory_kb\":{\"total\":%llu,\"free\":%llu,\"available\":%llu,"
            "\"buffers\":%llu,\"cached\":%llu,\"swap_total\":%llu,\"swap_free\":%llu},",
        mem.total_kb, mem.free_kb, mem.available_kb, mem.buffers_kb, mem.cached_kb,
        mem.swap_total_kb, mem.swap_free_kb);

    if (statvfs("/", &disk) == 0) {
        unsigned long long block = disk.f_frsize ? disk.f_frsize : disk.f_bsize;
        unsigned long long total = block * disk.f_blocks;
        unsigned long long avail = block * disk.f_bavail;
        unsigned long long free_bytes = block * disk.f_bfree;
        add(&b, "\"storage\":{\"total\":%llu,\"used\":%llu,\"available\":%llu},",
            total, total >= free_bytes ? total - free_bytes : 0, avail);
    } else add(&b, "\"storage\":{\"total\":0,\"used\":0,\"available\":0},");

    add(&b, "\"connections\":{\"tcp4\":%llu,\"tcp6\":%llu,\"udp4\":%llu,\"udp6\":%llu,\"unix\":%llu},",
        count_table("/proc/net/tcp", 1), count_table("/proc/net/tcp6", 1),
        count_table("/proc/net/udp", 1), count_table("/proc/net/udp6", 1),
        count_table("/proc/net/unix", 1));
    add(&b, "\"throughput\":{\"rx_bps\":%llu,\"tx_bps\":%llu,\"window_ms\":%lld}}",
        rx_bps, tx_bps, speed_window_ms);
    return total_usage;
}
