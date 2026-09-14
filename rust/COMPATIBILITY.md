# Compatibility tracker

The Rust branch is never installed over the production binary until every required row is green on all supported devices.

| Area | Rust status | Production gate |
|---|---|---|
| CLI and static ARM64 musl build | implemented | real MU5252 `/tmp` smoke passed |
| `/healthz`, `/version`, `/state` | build-time identity and firmware separation pass original suite | full normalized schema comparison pending |
| change-driven `/events` SSE | implemented | reconnect and client-limit tests pending |
| `/capabilities` | foundation implemented | exact action list pending |
| `/ubus`, `/ubus/list`, `/ubus/call` | implemented with validation, timeout and output cap | auth/list shape parity pending |
| static token authentication and LAN listener | implemented; original version/SSE suite passes | login/exchange and rotating session expiry pending |
| normalized device state | pending | MU5250, MU5252, MC7523, MC8532B golden comparisons |
| allow-listed device controls | pending | readback, rollback and error-code parity |
| bounded neighbor QTrace parser | implemented; original 20-case golden suite passes | real capture replay passes; collector lifecycle pending |
| cloud MQTT/WebSocket | pending | protocol and failure-state parity |
| signed OTA | pending | Ed25519, safety gates, rollback and source ordering |

No Rust code may call the legacy datad binary or link the legacy C/Go objects. During migration, device execution is read-only on a separate port unless a control adapter has its own fixture and rollback tests.
