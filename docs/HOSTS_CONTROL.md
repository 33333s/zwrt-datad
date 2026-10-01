# Hosts management

Hosts actions operate only on `/etc/hosts`. There is no caller-selected path,
shell command, or automatic update-block initialization.

## Authenticated local API

`POST /control` requires the existing datad Bearer token.

| Action | Parameters | Result |
| --- | --- | --- |
| `hosts.status` | `{}` | Capability, exact content, SHA-256 revision, backup and DNS-service status |
| `hosts.save` | `content`, `expected_revision` | Verified resulting revision |
| `hosts.restore` | `expected_revision` | Verified restored revision |

Both writes require `confirmed:true` in the request envelope. A revision is the
64-character lowercase SHA-256 of the exact UTF-8 file bytes. Content is limited
to 4096 bytes; cloud control envelopes retain their separate 8 KiB limit. Oversize
input is rejected, never truncated. Empty save inputs, invalid UTF-8 and control
characters other than tab, CR and LF are rejected; comments are preserved.
An actually empty current file still has its real content and revision and can
be repaired by saving nonempty text or restoring a valid backup. Restore can
recover an originally empty file exactly.

The current file must match `expected_revision`. A conflict does not overwrite
the newer file. The first save creates a private, fixed-path backup containing
the original bytes and ownership/mode; later saves keep that backup. Restore
also checks the current revision. The manager rejects symbolic links,
non-regular files and unsafe backup storage, serializes its writes, replaces the
file atomically and verifies the resulting bytes.

Writes require the fixed dnsmasq service. Success is returned only after file
readback and DNS reload succeed. A reload or readback failure can occur after
the file changed: the caller must read the current state instead of replaying
the write automatically. Saving the original text restores it without guessing
whether a prior request ran.

## On-demand cloud panel

An authenticated panel session receives `hosts_config` with `supported`,
`writable`, `content`, `revision`, `has_backup`, `dnsmasq`, and an optional finite
`error` code. Failed reads omit content and revision rather than inventing an
empty file. The active session refreshes this state every five seconds and after
Hosts controls. Hosts content is not included in ordinary MQTT telemetry,
local `/state`, or SSE.

Cloud writes use the existing confirmed v2 panel control channel. A successful
Hosts `control_result` additionally carries only the verified `revision`; it
does not return file content or command output. NMS must enforce current device
permissions and record an audit before sending either write, without logging
Hosts content. The UI waits for the matching state revision, preserves drafts
during refresh, and clears editor data when the session ends.
