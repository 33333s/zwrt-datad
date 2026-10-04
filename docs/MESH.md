# Direct (WebRTC) data path

datad is the answering end of the direct session described in NMS `docs/P2P.md`. NMS stays the
rendezvous; the classic remote tunnel stays open as the fallback, so every failure here only ends the
direct path.

## Build feature

The `mesh` cargo feature (on by default) pulls in `str0m` with its pure-Rust crypto backend. It adds
about 2.5 MB to the static ARM64 binary. Builds that cannot afford that use `--no-default-features`:
such a daemon rejects `transport:"mesh"` with `mesh_unsupported` and never advertises the mesh
capabilities. The ARM32 line is built without it.

## `remote.open` with `transport:"mesh"`

`remote.open` gains `transport:"mesh"` and `mesh:{v,stun[],udp_ports,ttl_seconds,rendezvous_url}`. It is
refused with `invalid_mesh` unless:

- `v` is 1, `udp_ports` is one unprivileged port or a range of fewer than 1024 ports, `stun` has at most four
  well-formed `stun:host[:port]` entries and `ttl_seconds` is 1 to 3600;
- `rendezvous_url` is a `wss://` URL on the same approved origins as the tunnel, with no credentials or
  fragment, the path `/api/remote/device/<request_id>` and exactly the query `transport=mesh`.

A `router_web` session with an empty `target_ports` is mesh-only: no pooled tunnel connections are opened,
the data channel is the only transport. Other services ignore the mesh object and keep using the tunnel.

## Session

1. The rendezvous WebSocket is opened with `Authorization: Bearer <token>` and no `Origin`, and reopened with
   backoff while the session lives.
2. A `signal` carrying an `offer` builds a fresh peer connection (a new offer drops the old peer): answer,
   trickle ICE both ways, accept the data channel labelled `nms`.
3. ICE uses one UDP socket per local IPv4 address, all on the same port, preferring a random port of
   `udp_ports`. Candidates are host plus server-reflexive (a STUN binding probe on the ICE socket); there is no
   TURN. Browser candidates that are mDNS names cannot be parsed and are skipped.
4. The channel answers `{"t":"ping","n":N}` with `{"t":"pong","n":N}`. Binary frames are
   `[u32 BE header length][JSON header][payload]`, at most 256 KiB, header at most 32 KiB.
5. Everything ends, with no further remote data, on `remote.close`, when `ttl_seconds` (at most one hour)
   passes, when the browser has been away for more than 30 seconds (the first browser gets 120 seconds to
   arrive), or when the connection or rendezvous socket is lost for good.

## HTTP over the channel (`router_web`)

`http`, `http.body`, `http.ack` and `http.cancel` frames serve the device web UI from `127.0.0.1:<session
port>`:

- request header `{"t":"http","id":N,"method","path","headers":[[name,value],…],"body_len":L}` plus the first
  up to 128 KiB of the body as payload, then `http.body` frames until `body_len` bytes arrived. Methods are
  `GET HEAD POST PUT DELETE PATCH OPTIONS`; the path must be origin-form (an absolute URI, `//authority` or a
  `port` other than the session's is `target_not_allowed`); at most 64 headers, 32 MiB of body;
- the device opens one connection per request, sends `Host: 127.0.0.1:<port>`, rewrites `origin`
  (`http://127.0.0.1:<port>`) and path-form `referer` (`http://127.0.0.1:<port><path>`), drops hop-by-hop headers
  and `content-length`/`expect`, never follows redirects and refuses `101` upgrades;
- the reply is one `http.head` (`status`, lower-case `headers` with `set-cookie` repeated, `last` when there is no
  body) and `http.body` frames of at most 64 KiB; `Content-Length`, chunked and close-delimited bodies are all
  decoded. At most 1 MiB is unacknowledged per request (`http.ack`); `http.cancel` or losing the channel closes
  the upstream connection. At most 16 requests run at once (`busy` beyond that) and at most 32 MiB of request
  bodies are held per session. Errors are `{"id","ok":false,"error"}` with `upstream_unreachable`,
  `upstream_timeout`, `upstream_closed`, `bad_response`, `bad_request`, `body_too_large`, `too_large`, `busy`.

A `datad_files` session that negotiated a direct channel keeps its normal tunnel; the channel answers every
request with `ok:false,"unsupported"` so the browser uses the relay (files are not served directly yet).

## Capabilities

Builds with the `mesh` feature advertise `datad.mesh` and `datad.mesh_http` (together with the existing
`datad.remote_close`, which NMS also requires). Without the feature neither is advertised.

## Tests

- `cargo test mesh::` runs everything over loopback, including a str0m "browser" talking to the device through a
  fake rendezvous hub (ICE, DTLS, SCTP, a 3 MiB flow-controlled download byte for byte).
- `ZWRT_MESH_STUN_TEST=stun:stun.cloudflare.com:3478 cargo test stun_probe` needs the network and checks the
  server-reflexive candidate.
- `cargo test browser_interop -- --ignored --nocapture` serves a page that plays NMS's browser; open the printed
  URL in Chrome. Setting `MESH_NO_DEVICE=1 MESH_HUB_PORT=<port>` leaves the device end to a real device running
  `device_side` (`MESH_HUB`, `MESH_WEB_PORT`, `MESH_STUN`, `MESH_SECONDS`).
