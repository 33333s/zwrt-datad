# Repository Boundary

This public repository only carries clean-room state aggregation code and public API/schema documentation.

Keep in this repository:

- Device state aggregation and model templates
- Public `/state` and `/events` schema
- QoS, SMS, WiFi, client-list, battery, and traffic aggregation
- Public-safe build scripts and init script

Do not copy into this repository:

- Modem signaling capture/decode source files
- DCI, qmdl/qmdl2 replay, or vendor diag tooling
- Local device workflow notes, SSH targets, keys, or live test logs
- Private worklogs with device hashes, PIDs, capture samples, or lab-only findings

`net.HSR` remains part of the public schema, but this repository keeps it `false` until it has a public, signal-confirmed implementation. Frequency-only inference is not allowed.
