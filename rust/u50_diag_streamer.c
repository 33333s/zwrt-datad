/* On-device signaling streamer, embedded and supervised by datad.
 *
 * Same DCI initialization sequence as u50_diag_worker.c (proven on the U50
 * Pro firmware) plus a loopback TCP fan-out so PC-side consumers (the sigweb
 * relay through an adb forward) receive the raw log packets. Writes a bounded
 * status JSON every second and exits when the supervisor heartbeat goes stale,
 * so an orphaned streamer can never hold the DCI client forever.
 *
 * usage: diag-streamer <statusfile> <heartbeat> [port]   (default port 9483)
 */
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <stdlib.h>
#include <signal.h>
#include <time.h>
#include <dlfcn.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/select.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <arpa/inet.h>

typedef unsigned char byte;
typedef unsigned short uint16;

static const uint16 CODES[] = {
    0x1832,0x1C07,0x4176,0x4179,0xB060,0xB062,0xB063,0xB064,0xB066,0xB067,0xB081,0xB082,0xB083,0xB087,0xB091,0xB092,0xB093,0xB097,0xB0A4,0xB0A5,0xB0B1,0xB0B4,0xB0B5,0xB0C0,0xB0C1,0xB0C2,0xB0E0,0xB0E1,0xB0E2,0xB0E3,0xB0E4,0xB0E5,0xB0E6,0xB0EA,0xB0EB,0xB0EC,0xB0ED,0xB0EE,0xB0EF,0xB111,0xB113,0xB114,0xB115,0xB11B,0xB11D,0xB122,0xB123,0xB126,0xB129,0xB12A,0xB12E,0xB130,0xB132,0xB134,0xB139,0xB13C,0xB146,0xB14D,0xB14E,0xB15B,0xB160,0xB165,0xB166,0xB16A,0xB16B,0xB16C,0xB16D,0xB16E,0xB16F,0xB172,0xB173,0xB174,0xB175,0xB176,0xB179,0xB17D,0xB17E,0xB187,0xB18A,0xB18B,0xB18D,0xB18E,0xB18F,0xB192,0xB193,0xB194,0xB195,0xB196,0xB198,0xB19E,0xB1A0,0xB1A4,0xB1B0,0xB1C6,0xB1DC,0xB1F3,0xB800,0xB801,0xB808,0xB809,0xB80A,0xB80B,0xB80C,0xB80D,0xB814,0xB815,0xB821,0xB822,0xB823,0xB825,0xB826,0xB82C,0xB840,0xB841,0xB842,0xB84B,0xB84D,0xB84E,0xB857,0xB860,0xB861,0xB868,0xB869,0xB870,0xB871,0xB872,0xB873,0xB881,0xB883,0xB884,0xB885,0xB886,0xB887,0xB888,0xB889,0xB88A,0xB890,0xB896,0xB89B,0xB89C,0xB8A1,0xB8A7,0xB8AE,0xB8C9,0xB8D1,0xB8D2,0xB8D8,0xB8DD,0xB950,0xB959,0xB96D,0xB970,0xB97F,0xB9A7
};
#define NCODES (int)(sizeof(CODES)/sizeof(CODES[0]))
#define MAXC 4

static volatile sig_atomic_t stop_flag = 0;
static void on_stop(int s) { (void)s; stop_flag = 1; }

static int cli[MAXC] = {-1, -1, -1, -1};
static int g_sub_on = 0;      /* DIAG codes subscribed */
static char g_bind[24] = "127.0.0.1";
static unsigned long total_pkts = 0;
static unsigned long sent_pkts = 0;

static int write_status(const char *path, int client_id, const char *state,
                        const char *reason, int port) {
    char tmp[300];
    snprintf(tmp, sizeof(tmp), "%s.new", path);
    FILE *f = fopen(tmp, "w");
    if (!f) return -1;
    int n = 0;
    for (int i = 0; i < MAXC; i++) if (cli[i] >= 0) n++;
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    fprintf(f, "{\"state\":\"%s\",\"reason\":\"%s\",\"client_id\":%d,"
               "\"ts\":%ld,\"pid\":%ld,\"port\":%d,\"bind\":\"%s\","
               "\"clients\":%d,\"subscribed\":%d,"
               "\"total\":%lu,\"sent\":%lu,\"codes\":%d}",
            state, reason, client_id, (long)ts.tv_sec, (long)getpid(),
            port, g_bind, n, g_sub_on, total_pkts, sent_pkts, NCODES);
    fclose(f);
    return rename(tmp, path) == 0 ? 0 : -1;
}

/* DCI callback: forward each log packet to every connected consumer with a
 * [len u32][code u32] header, one writev syscall per client per packet.
 * A consumer that stops reading gets dropped (SO_SNDTIMEO), never blocking
 * the diag dispatch thread. */
static void logs_cb(unsigned char *ptr, int len) {
    total_pkts++;
    if (len < 4) return;
    unsigned short code = (unsigned short)(ptr[2] | (ptr[3] << 8));
    unsigned char hdr[8];
    *(int *)hdr = len;
    *(int *)(hdr + 4) = (int)code;
    struct iovec iov[2] = {{hdr, 8}, {ptr, (size_t)len}};
    for (int i = 0; i < MAXC; i++) {
        if (cli[i] < 0) continue;
        if (writev(cli[i], iov, 2) != 8 + len) {
            close(cli[i]); cli[i] = -1;
            continue;
        }
        sent_pkts++;
    }
}

static void events_cb(unsigned char *p, int l) { (void)p; (void)l; }

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s <statusfile> <heartbeat> [port] [bind]\n", argv[0]);
        return 64;
    }
    const char *status_path = argv[1];
    const char *heartbeat_path = argv[2];
    int port = argc >= 4 ? atoi(argv[3]) : 9483;
    if (port <= 0 || port > 65535) port = 9483;
    const char *bind_addr = argc >= 5 ? argv[4] : "127.0.0.1";
    struct in_addr in;
    /* Raw DIAG is an internal transport only. Public access is through datad's
     * authenticated HTTP endpoint; even a stale launcher cannot expose it. */
    if (!inet_aton(bind_addr, &in) || in.s_addr != htonl(INADDR_LOOPBACK)) {
        write_status(status_path, -1, "error", "loopback_required", port); return 64;
    }
    snprintf(g_bind, sizeof(g_bind), "%s", bind_addr);
    signal(SIGINT, on_stop);
    signal(SIGTERM, on_stop);
    write_status(status_path, -1, "starting", "", port);

    int ls = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(ls, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in a;
    memset(&a, 0, sizeof(a));
    a.sin_family = AF_INET;
    a.sin_addr = in;
    a.sin_port = htons((unsigned short)port);
    if (bind(ls, (struct sockaddr *)&a, sizeof(a)) < 0 || listen(ls, 4) < 0) {
        write_status(status_path, -1, "error", "bind_failed", port);
        return 65;
    }

    void *h = dlopen("libdiag.so.1", RTLD_NOW);
    if (!h) {
        write_status(status_path, -1, "error", "libdiag_missing", port);
        return 66;
    }
    int (*lsm_init)(byte *) = dlsym(h, "Diag_LSM_Init");
    int (*dci_init)(void) = dlsym(h, "diag_lsm_dci_init");
    int (*reg_client)(int *, unsigned short *, int, void *) =
        dlsym(h, "diag_register_dci_client");
    int (*reg_stream)(int, void (*)(unsigned char *, int),
                      void (*)(unsigned char *, int)) =
        dlsym(h, "diag_register_dci_stream_proc");
    int (*log_cfg)(int, int, uint16[], int) = dlsym(h, "diag_log_stream_config");
    int (*rel_client)(int *) = dlsym(h, "diag_release_dci_client");
    int (*deinit)(void) = dlsym(h, "Diag_LSM_DeInit");
    if (!lsm_init || !dci_init || !reg_client || !reg_stream || !log_cfg ||
        !rel_client) {
        write_status(status_path, -1, "error", "libdiag_symbols", port);
        return 67;
    }

    if (!lsm_init((byte *)"zwrt-datad")) {
        write_status(status_path, -1, "error", "lsm_init_failed", port);
        return 68;
    }
    int rc = dci_init();
    if (rc != 0 && rc != 1001) {
        if (deinit) deinit();
        write_status(status_path, -1, "error", "dci_init_failed", port);
        return 69;
    }
    int client_id = -1;
    unsigned short periph = 0xFFFE;
    int os_params = 0;
    rc = reg_client(&client_id, &periph, 0, &os_params);
    if (rc != 0 && rc != 1001) {
        if (deinit) deinit();
        write_status(status_path, -1, "error", "dci_register_failed", port);
        return 70;
    }
    reg_stream(client_id, logs_cb, events_cb);
    /* Subscription is enabled only while consumers are connected: with no
     * client the modem keeps logging anyway, and dispatching hundreds of
     * callbacks per second to a forwarder that drops them all is pure CPU
     * and battery cost. The first accept re-enables the codes. */
    write_status(status_path, client_id, "running", "", port);

    int stale = 0;
    while (!stop_flag) {
        struct timeval tv = {1, 0};
        fd_set rf;
        FD_ZERO(&rf);
        FD_SET(ls, &rf);
        if (select(ls + 1, &rf, 0, 0, &tv) > 0 && FD_ISSET(ls, &rf)) {
            struct sockaddr_in ca;
            socklen_t cl = sizeof(ca);
            int c = accept(ls, (struct sockaddr *)&ca, &cl);
            if (c >= 0) {
                struct timeval stv = {2, 0};
                setsockopt(c, SOL_SOCKET, SO_SNDTIMEO, &stv, sizeof(stv));
                int nd = 1;
                setsockopt(c, IPPROTO_TCP, TCP_NODELAY, &nd, sizeof(nd));
                /* Reap consumers that vanished without FIN (relay reboot,
                 * laptop sleep) instead of holding a dead slot. */
                int ka = 1, idle = 30, intvl = 10, cnt = 3;
                setsockopt(c, SOL_SOCKET, SO_KEEPALIVE, &ka, sizeof(ka));
                setsockopt(c, IPPROTO_TCP, TCP_KEEPIDLE, &idle, sizeof(idle));
                setsockopt(c, IPPROTO_TCP, TCP_KEEPINTVL, &intvl, sizeof(intvl));
                setsockopt(c, IPPROTO_TCP, TCP_KEEPCNT, &cnt, sizeof(cnt));
                int slot = -1;
                for (int i = 0; i < MAXC; i++)
                    if (cli[i] < 0) { slot = i; break; }
                if (slot < 0) close(c);
                else cli[slot] = c;
            }
        }
        int have_clients = 0;
        for (int i = 0; i < MAXC; i++) if (cli[i] >= 0) have_clients = 1;
        if (have_clients && !g_sub_on) {
            log_cfg(client_id, 1, (uint16 *)CODES, NCODES);
            g_sub_on = 1;
        } else if (!have_clients && g_sub_on) {
            log_cfg(client_id, 0, (uint16 *)CODES, NCODES);
            g_sub_on = 0;
        }
        if (write_status(status_path, client_id, "running", "", port) != 0)
            stop_flag = 1; /* status unwritable: ordered shutdown */
        struct stat hb;
        if (stat(heartbeat_path, &hb) != 0) {
            if (++stale > 10) stop_flag = 1;
        } else {
            long age = (long)time(NULL) - (long)hb.st_mtime;
            stale = age > 10 ? stale + 1 : 0;
            if (stale > 10) stop_flag = 1; /* supervisor gone: exit */
        }
    }
    for (int i = 0; i < MAXC; i++)
        if (cli[i] >= 0) { close(cli[i]); cli[i] = -1; }
    log_cfg(client_id, 0, (uint16 *)CODES, NCODES);
    rel_client(&client_id);
    if (deinit) deinit();
    write_status(status_path, -1, "stopped",
                 stale > 10 ? "supervisor_timeout" : "", port);
    return 0;
}
