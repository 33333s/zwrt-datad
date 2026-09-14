# Compatibility tracker

The Rust branch is never installed over the production binary until every required row is green on all supported devices.

| Area | Rust status | Production gate |
|---|---|---|
| CLI and static ARM64 musl build | implemented | real MU5252 `/tmp` smoke passed |
| `/healthz`, `/version`, `/state` | foundation implemented | normalized schema comparison pending |
| change-driven `/events` SSE | implemented | reconnect and client-limit tests pending |
| `/capabilities` | foundation implemented | exact action list pending |
| `/ubus`, `/ubus/list`, `/ubus/call` | implemented with validation, timeout and output cap | auth/list shape parity pending |
| authentication and LAN listener | pending | login/exchange/token expiry parity |
| normalized device state | pending | MU5250, MU5252, MC7523, MC8532B golden comparisons |
| allow-listed device controls | pending | readback, rollback and error-code parity |
| neighbor collection and parser | pending | bounded fuzzing and real capture replay |
| cloud MQTT/WebSocket | pending | protocol and failure-state parity |
| signed OTA | pending | Ed25519, safety gates, rollback and source ordering |

No Rust code may call the legacy datad binary or link the legacy C/Go objects. During migration, device execution is read-only on a separate port unless a control adapter has its own fixture and rollback tests.
