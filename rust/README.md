# zwrt-datad Rust implementation

This directory contains the production implementation published from `main`.
It does not link or execute the archived C or Go datad; that history is retained
on the `c` branch.

The compatibility target is the public behavior documented in `docs/API.md`,
`docs/CONTROL_API.md`, `docs/STATE_SCHEMA.md`, and `docs/CLOUD.md`.

The production release includes normalized state, bounded SSE and WebShell,
authentication, allow-listed controls, neighbor parsing, cloud connectivity,
and signed OTA. CI exercises the Rust protocol, lifecycle, memory, service-token,
device-fixture and integration suites before a release binary can be built.
