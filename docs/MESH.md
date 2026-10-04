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

## Capabilities

`datad.mesh` and `datad.mesh_http` are advertised only by builds that serve the matching services.
