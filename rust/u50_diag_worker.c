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

#define CODE_COUNT 14
static const uint16 CODES[CODE_COUNT] = {0xB821, 0xB0C0, 0xB0E2, 0xB0E3,
                                         0xB0EC, 0xB0ED, 0x713A, 0x412F,
                                         0x512F, 0x5226,
                                         0xB800, 0xB801, 0xB80A, 0xB80B};
#define NAS_SM_IN  0xB800 /* NR5G NAS SM5G incoming */
#define NAS_SM_OUT 0xB801
#define MAX_PDU_TYPES 32
#define RATE_LIMIT_PER_SEC 500
#define MAX_QI 4

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
    /* QoS from NAS 5GSM PDU Session Establishment Accept */
    int five_qi[MAX_QI];       /* last seen 5QI values (0 = unused) */
    int five_qi_count;
    int ambr_dl_mant, ambr_dl_exp;  /* mantissa and exponent (kbps) */
    int ambr_ul_mant, ambr_ul_exp;
    int ambr_seen;             /* 0 = not seen yet */
    char nas_last_name[48];    /* last NAS message name */
    unsigned long nas_last_ts;
};
static struct stats S;

static const char *PDU_NAMES[] = {"", "bcch-bch", "bcch-dl-sch", "dl-ccch",
                                  "dl-dcch", "pcch", "ul-ccch", "ul-ccch1",
                                  "ul-dcch"};

/* ---- NAS 5GSM QoS extraction ------------------------------------
 * Pattern-based, verified on a U50 Pro (SA n78). The PDU Session
 * Establishment Accept (msg_type 0xC2) carries QoS Flow Descriptions
 * (IE 0x79) whose parameter list has 5QI as `01 01 XX` (id=1,len=1,val),
 * and Session-AMBR (IE 0x29) as `29 00 02 DL UL` (TLV-3, len 2). */

static const char *nas_5gsm_name(unsigned char mt) {
    switch (mt) {
    case 0xC1: return "PDU_SESSION_ESTABLISHMENT_REQUEST";
    case 0xC2: return "PDU_SESSION_ESTABLISHMENT_ACCEPT";
    case 0xC3: return "PDU_SESSION_ESTABLISHMENT_REJECT";
    case 0xD1: return "PDU_SESSION_MODIFICATION_REQUEST";
    case 0xD2: return "PDU_SESSION_MODIFICATION_ACCEPT";
    case 0xD3: return "PDU_SESSION_MODIFICATION_REJECT";
    case 0x90: return "PDU_SESSION_RELEASE_REQUEST";
    case 0x91: return "PDU_SESSION_RELEASE_ACCEPT";
    case 0x92: return "PDU_SESSION_RELEASE_REJECT";
    default:   return NULL;
    }
}

static void parse_nas_5gsm(unsigned char *data, int len) {
    /* data points at the 5GSM header: [0x2e][psid][pti][msg_type][...] */
    if (len < 5 || data[0] != 0x2e) return;
    unsigned char mt = data[3];
    const char *name = nas_5gsm_name(mt);
    if (name) {
        snprintf(S.nas_last_name, sizeof(S.nas_last_name), "%s", name);
        S.nas_last_ts = (unsigned long)time(NULL);
    }
    if (mt != 0xC2) return; /* only the Accept carries QoS + AMBR */

    /* Walk TLV-3 IEs: IEI(1) + Length(2 BE) + Value */
    int i = 4;
    while (i + 3 <= len) {
        unsigned iei = data[i];
        int ie_len = (data[i+1] << 8) | data[i+2];
        if (ie_len <= 0 || i + 3 + ie_len > len) break;
        unsigned char *val = data + i + 3;

        if (iei == 0x79) {
            /* QoS Flow Descriptions: [QFI+flags(1)][params...] */
            S.five_qi_count = 0;
            memset(S.five_qi, 0, sizeof(S.five_qi));
            int j = 0;
            while (j < ie_len && S.five_qi_count < MAX_QI) {
                /* Skip QFI byte */
                j++;
                /* Walk parameters: [id(1)][len(1)][value] */
                while (j + 2 < ie_len) {
                    unsigned pid = val[j];
                    unsigned plen = val[j+1];
                    if (pid == 0) break;
                    if (pid == 1 && plen == 1 && j + 2 < ie_len) {
                        int qi = val[j+2];
                        if (qi >= 1 && qi <= 255) {
                            S.five_qi[S.five_qi_count++] = qi;
                        }
                    }
                    j += 2 + plen;
                }
                if (j >= ie_len) break;
            }
        } else if (iei == 0x29 && ie_len >= 2) {
            /* Session-AMBR: [DL unit+mant(1)][UL unit+mant(1)] */
            S.ambr_dl_mant = val[0] >> 4;
            S.ambr_dl_exp  = val[0] & 0xF;
            S.ambr_ul_mant = val[1] >> 4;
            S.ambr_ul_exp  = val[1] & 0xF;
            S.ambr_seen = 1;
        }

        i += 3 + ie_len;
    }

    /* Fallback: pattern search for 5QI if IE walk missed it */
    if (S.five_qi_count == 0) {
        for (int j = 4; j + 2 < len; j++) {
            if (data[j] == 1 && data[j+1] == 1 && data[j+2] >= 1 &&
                data[j+2] <= 86) {
                S.five_qi[0] = data[j+2];
                S.five_qi_count = 1;
                break;
            }
        }
    }
    /* Fallback: pattern search for AMBR */
    if (!S.ambr_seen) {
        for (int j = 4; j + 4 < len; j++) {
            if (data[j] == 0x29 && data[j+1] == 0 && data[j+2] == 2) {
                S.ambr_dl_mant = data[j+3] >> 4;
                S.ambr_dl_exp  = data[j+3] & 0xF;
                S.ambr_ul_mant = data[j+4] >> 4;
                S.ambr_ul_exp  = data[j+4] & 0xF;
                S.ambr_seen = 1;
                break;
            }
        }
    }
}

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
    uint16 code = (uint16)(ptr[2] | (ptr[3] << 8));
    if (len >= 4) {
        for (int i = 0; i < CODE_COUNT; i++) {
            if (code == CODES[i]) {
                S.per_code[i]++;
                break;
            }
        }
    }
    /* NAS 5GSM: parse QoS (5QI) and AMBR from PDU Session messages */
    if ((code == NAS_SM_IN || code == NAS_SM_OUT) && len >= 16) {
        unsigned char *p = ptr + 12; /* skip len(2) code(2) ts(8) */
        /* The vendor header before the NAS message varies: try common
         * offsets until we see the 5GSM EPD (0x2e). */
        int plen = len - 12;
        for (int skip = 0; skip < 8 && skip < plen; skip++) {
            if (p[skip] == 0x2e) {
                parse_nas_5gsm(p + skip, plen - skip);
                break;
            }
        }
    }
    /* NR RRC OTA (0xB821): parse the v17 header for cell + PDU type */
    if (len >= 32 && code == 0xB821) {
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
    fprintf(f, "}");
    /* QoS: 5QI values and session AMBR from NAS 5GSM */
    if (S.five_qi_count > 0) {
        fprintf(f, ",\"five_qi\":[");
        for (int i = 0; i < S.five_qi_count; i++)
            fprintf(f, "%s%d", i ? "," : "", S.five_qi[i]);
        fprintf(f, "]");
    }
    if (S.ambr_seen) {
        fprintf(f, ",\"ambr\":{");
        fprintf(f, "\"dl_mant\":%d,\"dl_exp\":%d,", S.ambr_dl_mant,
                S.ambr_dl_exp);
        fprintf(f, "\"ul_mant\":%d,\"ul_exp\":%d}", S.ambr_ul_mant,
                S.ambr_ul_exp);
    }
    if (S.nas_last_name[0]) {
        fprintf(f, ",\"nas_last\":\"%s\",\"nas_ts\":%lu", S.nas_last_name,
                S.nas_last_ts);
    }
    fprintf(f, "}\n");
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
