# Neighbor cell adapter

The optional `neighbor` block is included in `/state` and its SSE snapshots.
Collection is **off by default**. `--once` never starts a diagnostic collector.
Enable it through the authenticated local control API:

```json
{"action":"neighbor.set","params":{"enabled":true}}
```

`neighbor.status` returns the current block. Enabling is scoped to the current
datad process: every service restart returns to disabled, including migration from
an older saved `enabled:true`. Setting `enabled:false` waits for the owned worker to
exit and removes its private capture directory before success is returned.
`/data/zwrt-datad/neighbor.json` is kept atomically at `enabled:false` with mode
0600. `--neighbor` is an explicit process-only development override;
`--neighbor-config PATH` selects another setting file.

Example cell (illustrative, not a claim about a particular device):

```json
{
  "rat":"NR", "pci":123, "arfcn":640000, "band":78,
  "rsrp_dbm":-90.0, "frequency_relation":"inter",
  "frequency_evidence":"explicit", "samples":3, "direct_hits":1, "age_ms":1250
}
```

`status` is `disabled`, `stopping`, `starting`, `collecting`, `ready`, `empty`,
`stale`, `blocked`, `dependency_missing`, or `error`. `reason` distinguishes an
occupied DIAG collector, another datad neighbor instance, missing `diag_mdlog`,
capture limits, stalled input, worker failure and invalid configuration.
`empty` with `reason:no_supported_reports` means frames are arriving but no
supported report has been decoded. It does **not** mean that no neighbors exist.
`ready` means at least one unexpired cell remains after serving/CA filtering.
Identity-only cells have `rsrp_dbm:null`.

The block also exposes `generation`, `collector_running`, `capture_bytes`,
`sampled_at` (Unix seconds), `age_ms`, `partial`, `frames`, `malformed`, `discarded`,
`ambiguous_measurements` and `cells`. Ages measure when new report bytes were
observed locally, not an independently synchronized modem measurement timestamp.
`malformed` and `discarded` are cumulative counters for the current collection
generation; `partial` is true only when either kind of loss occurred within the
same 60-second horizon as visible neighbor observations.
Unchanged files do not refresh report ages. Data expires after 60 seconds; SIM,
registration identity, serving cell or RAT changes invalidate the generation and
restart the private capture. Equivalent unknown NR cell IDs (absent, zero,
UINT32_MAX and -1) are normalized, since MU5250 B28 alternates these values
without changing its serving PCI/frequency. LTE and NR are both retained in NSA. Serving and
aggregated carriers are excluded using the current firmware network response.

## Interpretation limits

- Unknown ARFCN, band and RSRP are JSON `null`. LTE EARFCN 0 is valid.
  An unknown-frequency observation remains separate from a confirmed-frequency
  observation even when RAT and PCI match, because PCI can be reused on another
  frequency. Consumers should label it as unresolved rather than presenting it
  as another confirmed cell or merging it without evidence.
- NR identity association requires the same capture and PCI within 256 decoded
  frames and 5 seconds. Multiple candidate frequencies remain ambiguous; a
  required anchor is never inferred from the serving frequency.
- `frequency_evidence` describes explicit identity, bounded association, or
  unknown frequency. `frequency_relation` is `intra`, `inter`, or `unknown`
  relative to the current serving frequency for that RAT.
- Signal output is the median of at most the most recent 64 accepted samples
  for each RAT/PCI/ARFCN. These diagnostic values are observations, not proof that
  a cell can be selected, locked or registered.
- The supplied signatures cover generic NR records and B15/B27/B31 layouts,
  plus two LTE layouts. Datad additionally recognizes the verified MU5250 B28
  NR layouts below. Hashes are firmware-specific. There is no generic B20
  decoder and no automatic discovery of unknown layouts.
- A 2026-09-12 MU5252 B20 probe confirmed valid DIAG traffic through the existing
  `diag-router`, despite the absence of `/dev/diag`. Its captured hashes did not
  match the supplied decoder. B20 neighbor decoding remains unverified; do not
  present an empty result as successful radio coverage measurement.
- MU5250 B28 NR decoding was verified on 2026-09-12 after identifying the B28
  hashes. `/state` and `/events` returned fresh n1/ARFCN 424130 and
  n78/ARFCN 628704 signal measurements. n79/ARFCN 723360 identities were also
  observed; cells without a valid associated measurement retain null signal.
  The current serving cell was excluded as it changed between PCI 219 and 676.
  Enabling persisted through a service restart with one collector.
  This validates the observed B28 NR reports; B28 LTE and other firmware builds
  have not been validated by that test.

## MU5250 B28 NR layouts

The initial B28 test received valid frames but none of the supplied hashes.
Comparison with its matching diagnostic dictionary and captured argument layouts
identified these fixed aliases; unknown hashes still remain undecoded.

| Hash | Exact arguments | Fields used | Dictionary fingerprint |
| --- | --- | --- | --- |
| 3640387444 | 4 | band, ARFCN, PCI at 0, 1, 2 | 7405bbb0 |
| 3640166840 | 4 | band, ARFCN, PCI at 0, 1, 2 | c029f52f |
| 3640172852 | 4 | band, ARFCN, PCI at 0, 1, 2 | a48c39de |
| 3657934788 | 12 | neighbor PCI/Q7 RSRP at 3/4, comparison PCI/Q7 RSRP at 5/6 | f4799b21 |
| 3657937792, 3657920232 | 12 | neighbor PCI/Q7 RSRP at 3/4, comparison PCI/Q7 RSRP at 5/6 | 333d3978 |
| 3657646332 | 7 | explicit ARFCN/PCI/Q7 RSRP at 1/2/3 | 0f1cbaa6 |

The 12-argument reports require an independent identity record within the normal
capture/time/sequence bounds. The 7-argument result contains its own identity and
signal; its ARFCN cannot be overwritten by a nearby same-PCI identity on another
frequency. Frequency is never filled from the serving cell. The B28 -156 dBm
floor/default is not emitted as a measured RSRP; independently identified cells
can still appear with `rsrp_dbm:null`.
The fingerprints document the mapping evidence; the dictionary is neither
embedded nor required at runtime. The synthetic regressions cover exact frame
shapes, invalid fields, missing/ambiguous/cross-file anchors, unmeasured values,
explicit-frequency preservation and HTTP filtering.

## Isolation and resource limits

The sampling process only exchanges bounded, nonblocking IPC and filters cached
results. A separate worker owns `diag_mdlog` and parses only newly appended bytes.
It does not kill foreign collectors or the firmware's `diag-router` broker.
A private flock prevents another instance from starting in the same runtime
directory. A foreign `diag_mdlog`, `diag_socket_log` or `diag_uart_log` blocks
collection. Tools started independently after the check cannot be made atomic
with this lock; the worker detects them on its next poll and stops its own child.

The default runtime is `/tmp/zwrt-datad-neighbor` (0700). Only owned `capture.*`
directories there are cleaned. Symlinks and special capture files are rejected.
The embedded mask is written inside the private capture directory, followed by
`diag_mdlog -f MASK -o RING -s 4 -n 4 -c -d`. The tree is limited to 32 MiB,
32 QMDL files, 256 scanned entries and bounded depth; diagnostic output is
truncated at 64 KiB. The worker reads at most 4 MiB per poll, uses 64 KiB frame
buffers and 4096 identity plus 4096 measurement records, and returns at most 128
cells. Lost/truncated input and discarded current records set `partial` while the
loss remains inside the current 60-second result horizon.
HDLC FCS and complete frame delimiters are required. Incomplete frames are not
joined across distinct capture files.

Collector failure/stalled input triggers bounded retry delays. Parent heartbeats
detect an unresponsive worker after 15 seconds. Shutdown sends TERM, then KILL to
the owned process group after 3 seconds, with a bounded parent wait. Linux parent
death signals also clean up the worker and collector.

For isolated development only, `ZWRT_DATAD_NEIGHBOR_DIR`,
`ZWRT_DATAD_NEIGHBOR_CONFIG` and `ZWRT_DATAD_DIAG_BIN` override the paths above.
These are process environment settings, not remotely writable control arguments.

## Offline parsing and tests

```sh
./zwrt-datad --neighbor-parse capture.qmdl [another.qmdl ...]
python3 tests/neighbor_parser_test.py ./zwrt-datad
python3 tests/neighbor_http_test.py ./zwrt-datad
```

Offline inputs must be regular nonsymlink files, with at most 32 files and 32 MiB
total. A missing, oversized or unreadable file produces exit 66 and `partial:true`,
even if other inputs decoded. Corrupt frames are counted and rejected.
Synthetic tests cover known layouts, streaming/TTL, ambiguity, resource limits,
authentication, context changes, lifecycle, exclusivity and failure visibility.
They do not substitute for firmware-specific real capture validation.

## Source provenance

Signature layouts and the original 2345-byte QTrace mask were adapted from the
user-supplied `v0.7.1-multi` U60 neighbor source. Original parser SHA-256:
`65797dced3df038a11b3623032c40288f408a451f7667551593b21a657767995`.
The archive, proprietary diagnostic dictionaries and real captures are kept out
of the repository. The resident shell service and frontend are not runtime
dependencies; bounded parsing, lifecycle, filtering and persistence live in C.
