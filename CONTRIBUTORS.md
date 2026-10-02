# Contributors

Thanks to everyone who has contributed code, testing, device research, and
review to `zwrt-datad`.

- [Shuhao Dong (@imshuhao)](https://github.com/imshuhao) — contributed the
  original active-SIM-slot, multiline SSE framing, HTTP socket inheritance,
  and MU5252 controls/telemetry work in
  [PR #25](https://github.com/33333s/zwrt-datad/pull/25). The focused changes
  were subsequently integrated through PRs
  [#27](https://github.com/33333s/zwrt-datad/pull/27),
  [#28](https://github.com/33333s/zwrt-datad/pull/28),
  [#29](https://github.com/33333s/zwrt-datad/pull/29), and
  [#31](https://github.com/33333s/zwrt-datad/pull/31).

- [kanoqwq (@kanoqwq)](https://github.com/kanoqwq) — contributed the U50 Pro
  (MU5120) on-device verification and adaptation: battery current sign and
  capacity fallback, CPU temperature sources, pseudo thermal-zone filtering,
  and the `WL_AND_5G` network-mode readback, in
  [PR #121](https://github.com/33333s/zwrt-datad/pull/121). It was rebased onto
  the `arm32` branch and integrated through
  [PR #124](https://github.com/33333s/zwrt-datad/pull/124) (`arm32-v0.10.54`).

- [faying (@faying)](https://github.com/faying) — shared the native ubus socket
  client and single-executor design from the MU5250 fork
  [faying/zte-u60-pro-mu5250-data-service](https://github.com/faying/zte-u60-pro-mu5250-data-service)
  in [issue #140](https://github.com/33333s/zwrt-datad/issues/140); datad's
  socket ubus access and prioritised executor (0.10.56) follow that approach.
