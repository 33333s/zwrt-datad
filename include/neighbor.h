/* Neighbor QTrace adapter. The original signature rules come from
 * U60 neighbor v0.7.1-multi; see docs/NEIGHBOR.md for provenance and limits. */
#ifndef ZWRT_NEIGHBOR_H
#define ZWRT_NEIGHBOR_H
#include <stddef.h>
#include <stdint.h>

#define NEIGHBOR_MAX_CELLS 128
#define NEIGHBOR_TTL_MS 60000
#define NEIGHBOR_JSON_MAX 65536

enum neighbor_rat { NEIGHBOR_NR = 0, NEIGHBOR_LTE = 1 };
enum neighbor_evidence { NEIGHBOR_UNKNOWN, NEIGHBOR_ASSOCIATED, NEIGHBOR_EXPLICIT };
struct neighbor_cell {
    int rat, pci, band, has_rsrp, evidence;
    uint32_t arfcn, samples, direct_hits;
    double rsrp_dbm;
    int64_t seen_ms;
};
struct neighbor_result {
    uint64_t frames, malformed, discarded, ambiguous, reports;
    int partial;
    size_t count;
    int64_t seen_ms;
    struct neighbor_cell cells[NEIGHBOR_MAX_CELLS];
};
struct neighbor_parser;
struct neighbor_parser *neighbor_parser_new(void);
void neighbor_parser_free(struct neighbor_parser *p);
void neighbor_parser_feed(struct neighbor_parser *p, const void *data, size_t length,
                          uint64_t capture, int64_t now_ms);
void neighbor_parser_end_file(struct neighbor_parser *p, uint64_t capture);
void neighbor_parser_result(struct neighbor_parser *p, int64_t now_ms,
                            struct neighbor_result *out);
int neighbor_parse_cli(int argc, char **argv);

/* Parent-side calls only copy bounded cached data or exchange nonblocking IPC.
 * No diagnostic command or QMDL parsing runs in datad's sampling loop. */
void neighbor_manager_init(int enabled, const char *config_file);
void neighbor_manager_tick(const char *net, const char *sim);
void neighbor_manager_json(char *out, size_t size, const char *net);
int neighbor_manager_set_enabled(int enabled, char *err, size_t errlen);
void neighbor_manager_stop(void);
#endif
