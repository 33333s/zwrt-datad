/* zwrt-datad U50 signaling worker: a diag DCI client that subscribes to
 * NR/LTE RRC OTA logs through the vendor libdiag and reports a bounded
 * status JSON on stdout + <statusfile> every second. Cleanly deregisters on
 * exit. Crash-isolated from the datad main process by design. */
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <stdlib.h>
#include <signal.h>
#include <time.h>
#include <sys/stat.h>
#include <dlfcn.h>

typedef unsigned char byte;
typedef unsigned short uint16;

#define CODE_COUNT 10
static const uint16 CODES[CODE_COUNT] = {0xB821, 0xB0C0, 0xB0E2, 0xB0E3,
                                         0xB0EC, 0xB0ED, 0x713A, 0x412F,
                                         0x512F, 0x5226};
#define MAX_PDU_TYPES 32
#define RATE_LIMIT_PER_SEC 500

static volatile sig_atomic_t stop_flag = 0;
static void on_stop(int sig) { (void)sig; stop_flag = 1; }

struct stats {
    unsigned long total;
    unsigned long dropped;
    unsigned per_code[CODE_COUNT];
    unsigned pdu_types[MAX_PDU_TYPES];
    int pci, arfcn;
    unsigned long window_start;
    unsigned window_count;
    double rate;
};
static struct stats S;

static const char *PDU_NAMES[] = {"", "bcch-bch", "bcch-dl-sch", "dl-ccch",
                                  "dl-dcch", "pcch", "ul-ccch", "ul-ccch1",
                                  "ul-dcch"};

static void note_rate(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    unsigned long now = ts.tv_sec;
    if (!S.window_start) S.window_start = now;
    if (now - S.window_start >= 5) {
        S.rate = (double)S.window_count / (double)(now - S.window_start);
        S.window_start = now;
        S.window_count = 0;
    }
}

static void logs_cb(unsigned char *ptr, int len) {
    S.window_count++;
    if (S.window_count > RATE_LIMIT_PER_SEC) {
        S.dropped++;
        return;
    }
    S.total++;
    if (len >= 4) {
        uint16 code = (uint16)(ptr[2] | (ptr[3] << 8));
        for (int i = 0; i < CODE_COUNT; i++) {
            if (code == CODES[i]) {
                S.per_code[i]++;
                break;
            }
        }
    }
    /* NR RRC OTA (0xB821): parse the v17 header for cell + PDU type */
    if (len >= 32 && (uint16)(ptr[2] | (ptr[3] << 8)) == 0xB821) {
        unsigned char *p = ptr + 12; /* len(2) code(2) ts(8) */
        if (p[0] >= 17 && len >= 43) {
            S.pci = p[7] | (p[8] << 8);
            S.arfcn = (int)(p[17] | (p[18] << 8) | (p[19] << 16) |
                            ((unsigned)p[20] << 24));
            unsigned pdu = p[24];
            if (pdu < MAX_PDU_TYPES) S.pdu_types[pdu]++;
        }
    }
    note_rate();
}

static void events_cb(unsigned char *ptr, int len) { (void)ptr; (void)len; }

static int write_status(const char *path, int client_id, const char *state,
                        const char *reason) {
    char tmp[512];
    snprintf(tmp, sizeof(tmp), "%s.tmp", path);
    FILE *f = fopen(tmp, "w");
    if (!f) return -1;
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    fprintf(f, "{\"state\":\"%s\",\"reason\":\"%s\",\"client_id\":%d,"
               "\"ts\":%ld,\"total\":%lu,\"dropped\":%lu,\"rate\":%.1f,"
               "\"cell\":{\"pci\":%d,\"arfcn\":%d},"
               "\"codes\":{",
            state, reason, client_id, (long)ts.tv_sec, S.total, S.dropped,
            S.rate, S.pci, S.arfcn);
    for (int i = 0; i < CODE_COUNT; i++)
        fprintf(f, "%s\"0x%04x\":%u", i ? "," : "", CODES[i], S.per_code[i]);
    fprintf(f, "},\"pdu_types\":{");
    int first = 1;
    for (int i = 1; i < 12 && i < MAX_PDU_TYPES; i++) {
        if (S.pdu_types[i]) {
            fprintf(f, "%s\"%s\":%u", first ? "" : ",", PDU_NAMES[i],
                    S.pdu_types[i]);
            first = 0;
        }
    }
    fprintf(f, "}}\n");
    fclose(f);
    return rename(tmp, path);
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s <statusfile> <heartbeat>\n", argv[0]);
        return 64;
    }
    const char *status_path = argv[1];
    const char *heartbeat_path = argv[2];
    signal(SIGINT, on_stop);
    signal(SIGTERM, on_stop);

    void *h = dlopen("libdiag.so.1", RTLD_NOW);
    if (!h) {
        write_status(status_path, -1, "error", "libdiag_missing");
        return 65;
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
        write_status(status_path, -1, "error", "libdiag_symbols");
        return 66;
    }

    if (!lsm_init((byte *)"zwrt-datad")) {
        write_status(status_path, -1, "error", "lsm_init_failed");
        return 67;
    }
    int rc = dci_init();
    if (rc != 0 && rc != 1001) {
        if (deinit) deinit();
        write_status(status_path, -1, "error", "dci_init_failed");
        return 68;
    }
    int client_id = -1;
    unsigned short periph = 0xFFFE;
    int os_params = 0; /* the library dereferences this as int* */
    rc = reg_client(&client_id, &periph, 0, &os_params);
    if (rc != 0 && rc != 1001) {
        if (deinit) deinit();
        write_status(status_path, -1, "error", "dci_register_failed");
        return 69;
    }
    reg_stream(client_id, logs_cb, events_cb);
    log_cfg(client_id, 1, (uint16 *)CODES, CODE_COUNT);
    write_status(status_path, client_id, "running", "");

    int stale = 0;
    while (!stop_flag) {
        sleep(1);
        if (write_status(status_path, client_id, "running", "") != 0)
            stop_flag = 1; /* status file unwritable: ordered shutdown */
        struct stat hb;
        if (stat(heartbeat_path, &hb) != 0) {
            if (++stale > 10) stop_flag = 1;
        } else {
            long age = (long)time(NULL) - (long)hb.st_mtime;
            stale = age > 10 ? stale + 1 : 0;
            if (stale > 10) stop_flag = 1; /* supervisor gone: exit */
        }
    }
    /* clean teardown: unsubscribe, release the client, deinit the library */
    log_cfg(client_id, 0, (uint16 *)CODES, CODE_COUNT);
    rel_client(&client_id);
    if (deinit) deinit();
    write_status(status_path, -1, "stopped", stale > 10 ? "supervisor_timeout" : "");
    return 0;
}
