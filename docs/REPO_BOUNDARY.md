# Repository Boundary

This public repository carries clean-room device state aggregation, public-safe device control, and API/schema documentation.

Keep in this repository:

- Device state aggregation and model templates
- Public `/state` and `/events` schema
- QoS, SMS, WiFi, client-list, battery, and traffic aggregation
- Compile-time allow-listed device controls over local authenticated HTTP
- MU5252-only full ubus transport using validated service/method identifiers and argument-vector execution
- Safe `fork/exec` wrappers for fixed and MU5252 runtime-selected `ubus`, plus fixed `uci` and WiFi operations
- Public-safe build scripts and init script

Do not copy into this repository:

- Modem signaling capture/decode source files
- DCI, qmdl/qmdl2 replay, or vendor diag tooling
- Local device workflow notes, SSH targets, keys, or live test logs
- Private worklogs with device hashes, PIDs, capture samples, or lab-only findings
- UFI application concerns such as UUID, OTA, plugins, SQLite, schedulers, browser sessions, and frontend assets

`net.HSR` remains part of the public schema, but this repository keeps it `false` until it has a public, signal-confirmed implementation. Frequency-only inference is not allowed.
