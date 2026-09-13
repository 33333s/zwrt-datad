/* Adapted from U60 neighbor v0.7.1-multi's C signature decoder.
 * Original source SHA256: 65797dced3df038a11b3623032c40288f408a451f7667551593b21a657767995.
 * Bounded streaming, FCS, freshness and ambiguity handling are datad additions. */
#include "neighbor.h"
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define RECORDS 4096
#define FRAME_MAX 65536
#define ANCHOR_FRAMES 256
#define ANCHOR_MS 5000
#define CELL_SIGNALS 64
#define INPUT_MAX (32U * 1024U * 1024U)

typedef struct {
    uint64_t seq, capture;
    int64_t at_ms;
    int pci;
    uint32_t arfcn;
    int band, ssb;
    unsigned signature;
} DirectCell;
typedef struct {
    uint64_t seq, capture;
    int64_t at_ms;
    int rat, pci;
    uint32_t arfcn;
    int band;
    double rsrp;
    bool has_rsrp, require_direct_anchor;
} Snapshot;
typedef struct neighbor_parser {
    DirectCell direct[RECORDS];
    Snapshot snapshots[RECORDS];
    size_t direct_count, direct_next, snapshots_count, snapshots_next;
    uint64_t frames, malformed, discarded, reports, seq, capture;
    int64_t now_ms, seen_ms, malformed_at_ms, discarded_at_ms;
    unsigned char frame[FRAME_MAX];
    size_t frame_len;
    int escaped, dropping, have_capture;
} ParseState;

static void append_snapshot(ParseState *s, Snapshot *v) {
    v->at_ms = s->now_ms;
    size_t slot = s->snapshots_next++ % RECORDS;
    if (s->snapshots_count < RECORDS) s->snapshots_count++;
    else if (s->now_ms - s->snapshots[slot].at_ms <= NEIGHBOR_TTL_MS) {
        s->discarded++;
        s->discarded_at_ms = s->now_ms;
    }
    s->snapshots[slot] = *v;
    s->reports++; s->seen_ms = s->now_ms;
}
static uint32_t le32(const uint8_t *p) {
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) |
           ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}
static bool valid_nr_pci(uint32_t n) { return n <= 1007; }
static bool valid_nr_arfcn(uint32_t n) { return n > 0 && n <= 3279165; }
static bool valid_nr_band(uint32_t n) { return n > 0 && n <= 1024; }
static double js_round_2(double n) { return floor(n * 100.0 + 0.5) / 100.0; }
static double q7_dbm(uint32_t n) { return js_round_2((double)(int32_t)n / 128.0); }
static bool plausible_q7_dbm(uint32_t n) {
    double d = (double)(int32_t)n / 128.0;
    return d >= -160.0 && d <= -20.0;
}
static bool normalize_lte_rsrp(uint32_t raw, double *out) {
    int32_t n = (int32_t)raw;
    if (n >= -140 && n <= -30) { *out = n; return true; }
    if (n >= -1400 && n <= -300) { *out = js_round_2((double)n / 10.0); return true; }
    return false;
}
static bool direct_from_signature(uint32_t hash, const uint32_t *a, size_t n,
                                  uint64_t seq, uint64_t capture, DirectCell *out) {
    uint32_t pci = UINT32_MAX, arfcn = 0, band = 0, ssb = 0;
    unsigned signature = 0;
    if (hash == 3640397572U && n >= 4) {
        band = a[0]; arfcn = a[1]; pci = a[2]; ssb = a[3]; signature = 1;
    } else if (hash == 3657515396U && n >= 6) {
        pci = a[2]; arfcn = a[4]; signature = 2;
    } else if ((hash == 3657212968U || hash == 4181705066U) && n >= 7) {
        pci = a[2]; ssb = a[3]; arfcn = a[4]; signature = hash == 3657212968U ? 3 : 4;
    } else if ((hash == 3657202244U || hash == 4181706276U) && n >= 6) {
        pci = a[1]; ssb = a[2]; arfcn = a[3]; signature = hash == 3657202244U ? 5 : 6;
    } else if (hash == 4188869441U && n >= 5) {
        arfcn = a[0]; pci = a[1]; signature = 7;
    } else if (hash == 3166370540U && n >= 3) {
        arfcn = a[0]; pci = a[1]; signature = 8;
    } else if ((hash == 0xd8facf74U || hash == 0xd8f773e8U) && n >= 4) {
        /* B15 measured/anchor identity: [NR band, ARFCN, PCI, SSB/rank]. */
        band = a[0]; arfcn = a[1]; pci = a[2]; ssb = a[3];
        signature = hash == 0xd8facf74U ? 9 : 10;
    } else if (hash == 0xd8f72994U && n >= 4) {
        /* B15 serving companion: [PCI, ARFCN, metric, role]. */
        pci = a[0]; arfcn = a[1]; ssb = a[3]; signature = 11;
    } else if (hash == 0xd9877e9cU && n >= 5) {
        /* B15 inter-frequency identity: [PCI, ARFCN, metric, role, state]. */
        pci = a[0]; arfcn = a[1]; ssb = a[3]; signature = 12;
    } else if (hash == 0xd8fc5cc0U && n >= 4) {
        /* B31 measured-cell identity: [NR band, ARFCN, PCI, SSB/rank]. */
        band = a[0]; arfcn = a[1]; pci = a[2]; ssb = a[3]; signature = 13;
    } else if ((hash == 0xd8fb8ad0U || hash == 0xd8f82d98U ||
                hash == 0xd8f84514U) && n >= 4) {
        /*
         * B27 measured/anchor identities: [NR band, ARFCN, PCI, SSB/rank].
         * The two anchor aliases are emitted together by this firmware.
         */
        band = a[0]; arfcn = a[1]; pci = a[2]; ssb = a[3];
        signature = hash == 0xd8fb8ad0U ? 14 :
                    (hash == 0xd8f82d98U ? 15 : 16);
    } else if ((hash == 3640387444U || hash == 3640166840U ||
                hash == 3640172852U) && n == 4) {
        /* MU5250 B28: [NR band, ARFCN, PCI, SSB/rank]. */
        band = a[0]; arfcn = a[1]; pci = a[2]; ssb = a[3];
        signature = 18;
    } else if (hash == 0xd8f7e394U && n >= 4) {
        /* B27 serving companion: [PCI, ARFCN, metric, role]. */
        pci = a[0]; arfcn = a[1]; ssb = a[3]; signature = 17;
    } else {
        return false;
    }
    if (!valid_nr_pci(pci) || !valid_nr_arfcn(arfcn) || (band && !valid_nr_band(band))) return false;
    *out = (DirectCell){.seq=seq, .capture=capture, .pci=(int)pci, .arfcn=arfcn, .band=(int)band, .ssb=(int)ssb, .signature=signature};
    return true;
}

static void process_qsh(ParseState *state, const uint8_t *frame, size_t length,
                        uint64_t seq, uint64_t capture) {
    if (length < 16 || frame[0] != 0x9d) return;
    int n = (int)frame[4] - 0x13;
    if (n < 0 || n > 236 || length < 16U + (size_t)n * 4U) return;
    uint32_t args[236];
    for (int i = 0; i < n; ++i) args[i] = le32(frame + 16 + (size_t)i * 4);
    uint32_t hash = le32(frame + 12);

    DirectCell direct;
    if (direct_from_signature(hash, args, (size_t)n, seq, capture, &direct)) {
        direct.at_ms = state->now_ms;
        size_t slot = state->direct_next++ % RECORDS;
        if (state->direct_count < RECORDS) state->direct_count++;
        else if (state->now_ms - state->direct[slot].at_ms <= NEIGHBOR_TTL_MS) {
            state->discarded++;
            state->discarded_at_ms = state->now_ms;
        }
        state->direct[slot] = direct;
        state->reports++; state->seen_ms = state->now_ms;
    }

    if ((hash == 3657540452U || hash == 3657523352U || hash == 4182273084U) &&
               n >= 12 && valid_nr_pci(args[3]) && valid_nr_pci(args[5]) &&
               plausible_q7_dbm(args[4]) && plausible_q7_dbm(args[6])) {
        Snapshot snapshot = {
            .seq = seq, .capture = capture, .rat = 0, .pci = (int)args[3],
            .arfcn = 0, .band = 0, .rsrp = q7_dbm(args[4]), .has_rsrp = true,
        };
        append_snapshot(state, &snapshot);
    } else if ((hash == 0xda0539fcU || hash == 0xda054a0cU) &&
               n >= (hash == 0xda0539fcU ? 12 : 11) &&
               valid_nr_pci(args[3]) && valid_nr_pci(args[5]) &&
               plausible_q7_dbm(args[4]) && plausible_q7_dbm(args[6])) {
        /*
         * B31 NR measurement snapshots.  Both layouts expose neighbor PCI
         * and Q7 RSRP at args[3:4], with a serving/anchor PCI and Q7 RSRP at
         * args[5:6].  Frequency is accepted only after the same capture has
         * an explicit B31 PCI+ARFCN identity record.
         */
        Snapshot snapshot = {
            .seq = seq, .capture = capture, .rat = 0, .pci = (int)args[3],
            .arfcn = 0, .band = 0, .rsrp = q7_dbm(args[4]), .has_rsrp = true,
            .require_direct_anchor = true,
        };
        append_snapshot(state, &snapshot);
    } else if ((hash == 0xda019f14U || hash == 0xda01af24U) &&
               n >= (hash == 0xda019f14U ? 12 : 11) &&
               valid_nr_pci(args[3]) && valid_nr_pci(args[5]) &&
               plausible_q7_dbm(args[4]) && plausible_q7_dbm(args[6])) {
        /* B15 NR measurement snapshots; the inter-frequency form is anchored. */
        Snapshot snapshot = {
            .seq = seq, .capture = capture, .rat = 0, .pci = (int)args[3],
            .arfcn = 0, .band = 0, .rsrp = q7_dbm(args[4]), .has_rsrp = true,
            .require_direct_anchor = hash == 0xda01af24U,
        };
        append_snapshot(state, &snapshot);
    } else if ((hash == 0xda06aa0cU || hash == 0xda06ba1cU) &&
               n >= (hash == 0xda06aa0cU ? 12 : 11) &&
               valid_nr_pci(args[3]) && valid_nr_pci(args[5]) &&
               plausible_q7_dbm(args[4]) && plausible_q7_dbm(args[6])) {
        /*
         * B27 NR measurement snapshots.  The 11-argument inter-frequency
         * form is accepted only when a matching explicit PCI+ARFCN identity
         * exists in the same capture; this prevents frequency guessing.
         */
        Snapshot snapshot = {
            .seq = seq, .capture = capture, .rat = 0, .pci = (int)args[3],
            .arfcn = 0, .band = 0, .rsrp = q7_dbm(args[4]), .has_rsrp = true,
            .require_direct_anchor = hash == 0xda06ba1cU,
        };
        append_snapshot(state, &snapshot);
    } else if ((hash == 3657934788U || hash == 3657937792U || hash == 3657920232U) && n == 12 &&
               args[4] != (uint32_t)-19968 && args[6] != (uint32_t)-19968 &&
               valid_nr_pci(args[3]) && valid_nr_pci(args[5]) &&
               plausible_q7_dbm(args[4]) && plausible_q7_dbm(args[6])) {
        /* MU5250 B28 NR comparisons, independently anchored. The firmware's
         * -156 dBm floor/default must not become a measured signal value. */
        Snapshot snapshot = {
            .seq = seq, .capture = capture, .rat = 0, .pci = (int)args[3],
            .arfcn = 0, .band = 0, .rsrp = q7_dbm(args[4]), .has_rsrp = true,
            .require_direct_anchor = true,
        };
        append_snapshot(state, &snapshot);
    } else if (hash == 3657646332U && n == 7 &&
               valid_nr_arfcn(args[1]) && valid_nr_pci(args[2]) &&
               args[3] != (uint32_t)-19968 && plausible_q7_dbm(args[3])) {
        /* B28 cell result: ARFCN, PCI and Q7 RSRP at indexes 1, 2 and 3.
         * Identity and signal are explicit in this one report. */
        Snapshot snapshot = {
            .seq = seq, .capture = capture, .rat = NEIGHBOR_NR, .pci = (int)args[2],
            .arfcn = args[1], .band = 0, .rsrp = q7_dbm(args[3]), .has_rsrp = true,
        };
        append_snapshot(state, &snapshot);
    } else if (hash == 3640546464U && n >= 4 && args[1] <= 503U && args[0] <= 262143U) {
        Snapshot snapshot = {
            .seq = seq, .capture = capture, .rat = 1, .pci = (int)args[1],
            .arfcn = args[0], .band = 0, .has_rsrp = false,
        };
        snapshot.has_rsrp = normalize_lte_rsrp(args[2], &snapshot.rsrp);
        append_snapshot(state, &snapshot);
    } else if (hash == 3644228716U && n >= 2 && args[1] <= 503U && args[0] <= 262143U) {
        Snapshot snapshot = {
            .seq = seq, .capture = capture, .rat = 1, .pci = (int)args[1],
            .arfcn = args[0], .band = 0, .has_rsrp = false,
        };
        append_snapshot(state, &snapshot);
    }
}


struct neighbor_parser *neighbor_parser_new(void) { return calloc(1, sizeof(ParseState)); }
void neighbor_parser_free(struct neighbor_parser *p) { free(p); }
void neighbor_parser_end_file(struct neighbor_parser *p, uint64_t capture) {
    if (!p || !p->have_capture || p->capture != capture) return;
    if (p->frame_len || p->escaped || p->dropping) {
        p->malformed++;
        p->malformed_at_ms = p->now_ms;
    }
    p->frame_len = 0; p->escaped = 0; p->dropping = 0;
    p->have_capture = 0;
}
static int valid_fcs(const unsigned char *s, size_t n) {
    unsigned crc = 0xffff;
    for (size_t i = 0; i < n; i++) {
        crc ^= s[i];
        for (int j = 0; j < 8; j++) crc = (crc >> 1) ^ ((crc & 1) ? 0x8408 : 0);
    }
    return crc == 0xf0b8;
}
void neighbor_parser_feed(struct neighbor_parser *p, const void *input, size_t n,
                          uint64_t capture, int64_t now_ms) {
    if (!p || (!input && n)) return;
    if (p->have_capture && p->capture != capture) neighbor_parser_end_file(p, p->capture);
    p->capture = capture; p->have_capture = 1; p->now_ms = now_ms;
    const unsigned char *s = input;
    for (size_t i = 0; i < n; i++) {
        unsigned char b = s[i];
        if (b == 0x7e) {
            if (p->dropping || p->escaped || (p->frame_len &&
                (p->frame_len < 3 || !valid_fcs(p->frame, p->frame_len)))) {
                p->malformed++;
                p->malformed_at_ms = p->now_ms;
            }
            else if (p->frame_len) {
                p->frames++;
                process_qsh(p, p->frame, p->frame_len - 2, ++p->seq, capture);
            }
            p->frame_len = 0; p->escaped = 0; p->dropping = 0;
            continue;
        }
        if (p->dropping) continue;
        if (p->escaped) { b ^= 0x20; p->escaped = 0; }
        else if (b == 0x7d) { p->escaped = 1; continue; }
        if (p->frame_len == sizeof p->frame) { p->dropping = 1; continue; }
        p->frame[p->frame_len++] = b;
    }
}

struct group {
    struct neighbor_cell cell;
    double values[CELL_SIGNALS];
    size_t values_count, values_next;
    int conflict;
};
static struct group *group_for(struct group *groups, size_t *count, int rat, int pci,
                              uint32_t arfcn, int64_t seen_ms, int *partial) {
    for (size_t i = 0; i < *count; i++) {
        struct neighbor_cell *c = &groups[i].cell;
        if (c->rat == rat && c->pci == pci && c->arfcn == arfcn) {
            if (seen_ms > c->seen_ms) c->seen_ms = seen_ms;
            return &groups[i];
        }
    }
    if (*count == NEIGHBOR_MAX_CELLS) { *partial = 1; return NULL; }
    struct group *g = &groups[(*count)++];
    memset(g, 0, sizeof *g);
    g->cell.rat = rat; g->cell.pci = pci; g->cell.arfcn = arfcn; g->cell.seen_ms = seen_ms;
    return g;
}
static int doubles(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}
static int cell_order(const void *a, const void *b) {
    const struct neighbor_cell *x = a, *y = b;
    if (x->has_rsrp != y->has_rsrp) return y->has_rsrp - x->has_rsrp;
    if (x->has_rsrp && x->rsrp_dbm != y->rsrp_dbm) return x->rsrp_dbm < y->rsrp_dbm ? 1 : -1;
    if (x->rat != y->rat) return x->rat - y->rat;
    if (x->arfcn != y->arfcn) return x->arfcn < y->arfcn ? -1 : 1;
    return x->pci - y->pci;
}
static int recent(int64_t now, int64_t at) { return now >= at && now - at <= NEIGHBOR_TTL_MS; }
void neighbor_parser_result(struct neighbor_parser *p, int64_t now, struct neighbor_result *out) {
    memset(out, 0, sizeof *out);
    if (!p) return;
    out->frames = p->frames; out->malformed = p->malformed; out->discarded = p->discarded;
    out->reports = p->reports; out->seen_ms = p->seen_ms;
    out->partial =
        (p->malformed && recent(now, p->malformed_at_ms)) ||
        (p->discarded && recent(now, p->discarded_at_ms));
    struct group groups[NEIGHBOR_MAX_CELLS];
    size_t count = 0;
    int heads[1008], next[RECORDS];
    for (size_t i = 0; i < 1008; i++) heads[i] = -1;
    for (size_t i = 0; i < p->direct_count; i++) {
        DirectCell *d = &p->direct[i];
        next[i] = heads[d->pci]; heads[d->pci] = (int)i;
    }
    /* Traverse ring records in observation order so medians retain the newest
     * 64 signals, not a buffer-index-dependent subset after wrapping. */
    size_t first = p->snapshots_next >= RECORDS ? p->snapshots_next % RECORDS : 0;
    for (size_t off = 0; off < p->snapshots_count; off++) {
        Snapshot *s = &p->snapshots[(first + off) % RECORDS];
        if (!recent(now, s->at_ms)) continue;
        uint32_t arfcn = s->arfcn;
        int found = 0, ambiguous = 0, comparisons = 0;
        if (s->rat == NEIGHBOR_NR && !arfcn) {
            for (int i = heads[s->pci]; i >= 0; i = next[i]) {
                DirectCell *d = &p->direct[i];
                if (d->capture != s->capture) continue;
                uint64_t distance = d->seq > s->seq ? d->seq - s->seq : s->seq - d->seq;
                int64_t elapsed = d->at_ms > s->at_ms ? d->at_ms - s->at_ms : s->at_ms - d->at_ms;
                if (distance > ANCHOR_FRAMES || elapsed > ANCHOR_MS) continue;
                if (++comparisons > 512) { ambiguous = 1; out->partial = 1; break; }
                if (found && arfcn != d->arfcn) ambiguous = 1;
                arfcn = d->arfcn; found = 1;
            }
            if (ambiguous) { out->ambiguous++; arfcn = 0; }
            if (s->require_direct_anchor && (!found || ambiguous)) continue;
        }
        struct group *g = group_for(groups, &count, s->rat, s->pci, arfcn, s->at_ms, &out->partial);
        if (!g) continue;
        g->cell.samples++;
        if (arfcn || s->rat == NEIGHBOR_LTE) g->cell.evidence = s->rat == NEIGHBOR_LTE || s->arfcn ? NEIGHBOR_EXPLICIT : NEIGHBOR_ASSOCIATED;
        if (s->has_rsrp) {
            g->values[g->values_next++ % CELL_SIGNALS] = s->rsrp;
            if (g->values_count < CELL_SIGNALS) g->values_count++;
        }
    }
    first = p->direct_next >= RECORDS ? p->direct_next % RECORDS : 0;
    for (size_t off = 0; off < p->direct_count; off++) {
        DirectCell *d = &p->direct[(first + off) % RECORDS];
        if (!recent(now, d->at_ms)) continue;
        struct group *g = group_for(groups, &count, NEIGHBOR_NR, d->pci, d->arfcn, d->at_ms, &out->partial);
        if (!g) continue;
        g->cell.direct_hits++; g->cell.evidence = NEIGHBOR_EXPLICIT;
        if (d->band) {
            if (g->cell.band && g->cell.band != d->band) g->conflict = 1;
            g->cell.band = d->band;
        }
    }
    for (size_t i = 0; i < count; i++) {
        struct group *g = &groups[i];
        if (g->conflict) g->cell.band = 0;
        if (g->values_count) {
            qsort(g->values, g->values_count, sizeof(double), doubles);
            size_t m = g->values_count / 2;
            g->cell.rsrp_dbm = g->values_count & 1 ? g->values[m] : js_round_2((g->values[m-1]+g->values[m])/2);
            g->cell.has_rsrp = 1;
        }
        out->cells[out->count++] = g->cell;
    }
    qsort(out->cells, out->count, sizeof out->cells[0], cell_order);
}

int neighbor_parse_cli(int argc, char **argv) {
    if (argc < 1 || argc > 32) { fputs("usage: zwrt-datad --neighbor-parse FILE.qmdl [FILE.qmdl ...]\n", stderr); return 64; }
    struct neighbor_parser *p = neighbor_parser_new();
    if (!p) return 71;
    size_t total = 0;
    int failed = 0;
    unsigned char buf[65536];
    for (int i = 0; i < argc; i++) {
        int fd = open(argv[i], O_RDONLY | O_NONBLOCK | O_NOFOLLOW);
        struct stat st;
        if (fd < 0 || fstat(fd, &st) || !S_ISREG(st.st_mode) || st.st_size < 0 ||
            (uint64_t)st.st_size > INPUT_MAX || total + (uint64_t)st.st_size > INPUT_MAX) {
            if (fd >= 0) close(fd);
            fprintf(stderr, "cannot read bounded regular input: %s\n", argv[i]);
            failed = 1; continue;
        }
        size_t remaining = (size_t)st.st_size;
        total += remaining;
        while (remaining) {
            size_t request = remaining < sizeof buf ? remaining : sizeof buf;
            ssize_t n = read(fd, buf, request);
            if (n < 0 && errno == EINTR) continue;
            if (n <= 0) { failed = 1; break; }
            neighbor_parser_feed(p, buf, (size_t)n, (uint64_t)i + 1, 1);
            remaining -= (size_t)n;
        }
        close(fd); neighbor_parser_end_file(p, (uint64_t)i + 1);
    }
    struct neighbor_result r;
    neighbor_parser_result(p, 1, &r);
    printf("{\"source\":\"qtrace\",\"frames\":%" PRIu64 ",\"malformed\":%" PRIu64 ",\"discarded\":%" PRIu64
           ",\"ambiguous\":%" PRIu64 ",\"partial\":%s,\"cells\":[", r.frames, r.malformed, r.discarded, r.ambiguous,
           (failed || r.partial) ? "true" : "false");
    for (size_t i = 0; i < r.count; i++) {
        struct neighbor_cell *c = &r.cells[i];
        if (i) putchar(',');
        printf("{\"rat\":\"%s\",\"pci\":%d,\"arfcn\":", c->rat == NEIGHBOR_NR ? "NR" : "LTE", c->pci);
        if (c->arfcn || c->rat == NEIGHBOR_LTE) printf("%u", c->arfcn); else fputs("null", stdout);
        fputs(",\"band\":", stdout); if (c->band) printf("%d", c->band); else fputs("null", stdout);
        fputs(",\"rsrp_dbm\":", stdout); if (c->has_rsrp) printf("%.2f", c->rsrp_dbm); else fputs("null", stdout);
        printf(",\"samples\":%u,\"direct_hits\":%u}", c->samples, c->direct_hits);
    }
    puts("]}");
    neighbor_parser_free(p);
    return failed ? 66 : 0;
}
