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

static long mem_used_pct(const char *sysinfo)
{
    char mem[1024];
    if (!json_get(sysinfo, "memory", mem, sizeof mem)) return -1;
    long total = json_get_int(mem, "total", 0);
    long avail = json_get_int(mem, "available", 0);
    if (total <= 0) return -1;
    return (total - avail) * 100 / total;
}

/* Poll everything and build the unified snapshot into `out`. */
static void build_snapshot(char *out, size_t outlen, int with_board, const char *board_cache)
{
    char net[RAW_MAX], batt[RAW_MAX], chg[RAW_MAX], therm[1024];
    char rnum[1024], rstat[1024], traf[RAW_MAX], sysinfo[2048];

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
    emit_int(&b, "charger_type", chg, "charger_type", 0);
    bappend(&b, "},");

    /* connected clients */
    bappend(&b, "\"clients\":{");
    emit_int(&b, "total", rnum, "access_total_num", 0); bappend(&b, ",");
    emit_int(&b, "wifi", rnum, "wireless_num", 0);      bappend(&b, ",");
    emit_int(&b, "lan", rnum, "lan_num", 0);
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

    /* system */
    bappend(&b, "\"system\":{");
    emit_int(&b, "uptime", sysinfo, "uptime", 0);    bappend(&b, ",");
    emit_int(&b, "cpu_temp", therm, "cpuss_temp", 0); bappend(&b, ",");
    bappend(&b, "\"mem_used_pct\":%ld,", mem_used_pct(sysinfo));
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
