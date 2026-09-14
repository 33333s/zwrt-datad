# Compatibility tracker

The Rust branch is never installed over the production binary until every required row is green on all supported devices.

| Area | Rust status | Production gate |
|---|---|---|
| CLI and static ARM64 musl build | implemented | real MU5252 `/tmp` smoke passed |
| `/healthz`, `/version`, `/state` | MC7523 has 273/273 leaf-path parity with no missing, extra, or type mismatches | MU5250, MU5252 and MC8532B golden comparisons pending |
| change-driven `/events` SSE | implemented; original framing suite passes | reconnect and client-limit tests pending |
| `/capabilities` | reports only the eleven currently implemented controls | expand with each verified action until exact parity |
| `/ubus`, `/ubus/list`, `/ubus/call` | implemented with validation, timeout, output cap and auth/list shape checks | complete |
| static and dynamic authentication | static token, LAN Basic login, vendor-token exchange, 48-byte-hex sessions and sliding expiry implemented | supported-device login smoke |
| normalized device state | MC7523 structure complete; battery, NFC, SMS, thermal and bounded QoS sources ported | finish MU5250, MU5252 and MC8532B value semantics |
| allow-listed device controls | ten read-only/session neighbor actions implemented; malformed JSON parity implemented | mutating actions require fixture, readback, rollback and error-code parity |
| bounded neighbor QTrace parser | parser and lifecycle implemented; original parser and HTTP suites pass | final supported-device smoke |
| cloud config | validation, password redaction, atomic 0600 persistence and LAN isolation implemented | MQTT/TLS reporting and WebSocket tunnel runtime |
| signed OTA | pending | Ed25519, safety gates, rollback and source ordering |

No Rust code may call the legacy datad binary or link the legacy C/Go objects. During migration, device execution is read-only on a separate port unless a control adapter has its own fixture and rollback tests.
