# zwrt-datad Rust rewrite

This directory is the clean-room Rust implementation developed on the long-lived
`rust` branch. It does not link or execute the existing C or Go datad.

The compatibility target is the public behavior documented in `docs/API.md`,
`docs/CONTROL_API.md`, `docs/STATE_SCHEMA.md`, and `docs/CLOUD.md`.

Current milestone provides the standalone CLI, bounded shell-free command
runner, periodic snapshots, `/healthz`, `/version`, `/state`, change-driven SSE,
`/capabilities`, and full-name validated `/ubus` discovery/calls. Device control,
authentication, normalized state, neighbor parsing, cloud, and signed OTA remain
blocked from production use until their compatibility tests pass.

Never deploy this branch over `/data/zwrt-datad/zwrt-datad`. Device trials use a
separate port and data directory and begin read-only.
