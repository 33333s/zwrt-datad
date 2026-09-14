# Compatibility tracker

The Rust branch is never installed over the production binary until every required row is green on all supported devices.

| Area | Rust status | Production gate |
|---|---|---|
| CLI and static ARM64 musl build | implemented | real MU5252 `/tmp` smoke passed |
| `/healthz`, `/version`, `/state` | MC7523 device shape parity passed; MU5252 TopFlow and all four supported-model read-only fixture matrices pass, including MC8532B UCI fallback | MU5250, MU5252 and MC8532B device golden comparisons pending |
| change-driven `/events` SSE | implemented; framing, reconnect, 16-client limit, 503 overflow and disconnect slot-release tests pass | complete |
| `/capabilities` | reports only the eighteen currently implemented controls | expand with each verified action until exact parity |
| `/ubus`, `/ubus/list`, `/ubus/call` | implemented with validation, timeout, output cap and auth/list shape checks | complete |
| static and dynamic authentication | static token, LAN Basic login, vendor-token exchange, 48-byte-hex sessions and sliding expiry implemented | supported-device login smoke |
| normalized device state | MC7523 structure complete; battery, NFC, SMS, thermal, bounded QoS, TopFlow aggregation/multi-WAN/cooling and slot-aware MU5252 modem state ported | finish MU5250 and MC8532B value semantics and normalized SMS payloads |
| allow-listed device controls | read-only/session neighbor/runtime actions plus MU5252 advanced Wi-Fi status implemented; malformed JSON parity implemented | mutating actions require fixture, readback, rollback and error-code parity |
| bounded neighbor QTrace parser | parser and lifecycle implemented; original parser and HTTP suites pass | final supported-device smoke |
| cloud config and runtime | validation, password redaction, atomic 0600 persistence, LAN isolation, MQTT/TLS QoS 1 reporting, hot reconfiguration and allow-listed WSS-to-localhost tunnels implemented; real local TLS MQTT/WSS roundtrips and MC7523 ARM64 `/tmp` config/status/0600 persistence smoke pass | production-platform interoperability |
| signed OTA | pure Rust config/status/check/update, custom-first source ordering with stale-source fallback, Ed25519 manifest verification, installer SHA-256, >10% battery and idle safety gates, retry state and result reconciliation implemented; MC7523 `/tmp` skipped stale signed netdisk 0.9.40 and verified signed GitHub 0.9.44 | never invoke install on a test device; final production-only upgrade/rollback acceptance remains |

No Rust code may call the legacy datad binary or link the legacy C/Go objects. During migration, device execution is read-only on a separate port unless a control adapter has its own fixture and rollback tests.
