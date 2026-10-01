# Authenticated remote files

The optional `datad_files` service uses `nms-datad-files-v1` on a dedicated reverse
WSS session with target port zero. It inherits the existing remote/panel-control
device flag, origin validation, TLS verification, single-use session ticket and
maximum one-hour lifetime. No local file listener is opened. Ordinary panel
control retains its 8 KiB limit.

Requests contain `type:file_request`, `protocol_version:1`, a fresh 32 lowercase
hexadecimal `request_id`, `action`, object `params` and boolean `confirmed`.
Replies contain `type:file_result`, the same version/ID, `ok`, and either object
`result` or a finite `code`. The dedicated frame limit is 256 KiB, non-chunk
requests are limited to 64 KiB, and decoded transfer chunks are at most 128 KiB.
Execution is sequential with a bounded pending queue; request IDs cannot replay.

Supported actions are `files.status/list/stat/disk/mkdir/touch/rename/chmod/remove`,
`files.compress/extract`, `files.upload.begin/chunk/commit/abort`, and
`files.download.begin/chunk/end`. All filesystem and upload mutations require
confirmation. Downloads are read operations. Uploads preserve the 500 MiB per-file
limit; downloads stream without an artificial 500 MiB limit. A session has at most
four live transfers. Content is not included in MQTT, `/state`, SSE, or audit data.

Paths are absolute UTF-8 with bounded names. Metadata versions bind identity,
timestamps, size, owner and permissions. Mutations validate versions and checked
parent descriptors. Uploads stage private files, check offsets, declared size and
SHA-256, preserve existing ownership/mode, publish atomically, and verify the
result. A displaced conflicting file is restored when possible or retained in a
private recovery location; a conflict does not silently discard an external
update. Session closure aborts temporary transfers. Disk reservations preserve
space for device configuration. Existing file content is never truncated by touch.

Compression creates a same-parent `.tar.gz`. Extraction supports tar, gzip,
bzip2, xz and ZIP, validates the complete compression stream, and stages a checked
tree before publication. Relative links are retained only within the extraction
root. Path escape, unsafe links, excessive entry counts, decompression output or
decoder memory are rejected. Cancellation and expiry are checked during long
operations and before publication.

NMS must enforce current device ownership/control permission, origin/CSRF and
session validity on every request, audit writes before dispatch without paths or
content, and apply its existing per-account bandwidth policy. Native downloads
must use a current-session opaque transfer ID, not a public temporary file or URL
containing a private path. Known late replies cannot resolve another operation.
Unknown mutation outcomes are not automatically retried.

Special filesystem objects are not ordinary files. In particular, a zero-size
virtual file that actually yields bytes is rejected instead of returning a false
verified empty file. Full virtual/special-filesystem editing is not established by
the regular-file implementation. Metadata/stat and directory listings remain
separate from content transfer.
