# U50 Pro / U50S ARM32 candidate

This branch contains a read-only original-firmware adapter. It is **not** a production template: `device.api_template_supported` remains `0` until actual U50 Pro and U50S runtime responses are verified.

## Static firmware evidence

The U50 Pro `u50prox62_full.bin` (SHA-256 `501b78f6f4b685a2caed6d5bae4b7193cb468bfb8dfdd3d26b7bf20225665148`) and U50S `BD_ZTE2SIMU50SV1.0.0B02` packages contain ARM 32-bit EABI5 root filesystems. U50 Pro identifies as `sdxlemur-qti-distro-nogplv3-debug`; U50S identifies as `sdxprairie-mdm`. Both contain `/usr/bin/cfg`, `zte_topsw_*`, and GoAhead, and neither inspected rootfs contains ZWRT `ubus`/`uci` tools. Their core binaries differ. Raw firmware and private partitions are not in this repository.

Both firmwares' original shell scripts call `cfg get` for the same read-only keys used by this adapter:

| Firmware source | Candidate output |
|---|---|
| `cfg get model_name` | `device.model_name` |
| `cfg get integrate_version` | `system.sw_version` |
| `cfg get lan_ipaddr` | `dhcp.ip` after IP validation |
| `cfg get lan_netmask`, `wan_ipaddr`, `wan_gateway`, `ppp_status` | `u50_cfg` raw configuration/status fields; IPs validated, PPP code not interpreted |

Both GoAhead binaries contain the same read keys `model_name`, `wa_inner_version`, `network_type`, `network_provider_fullname`, `network_provider`, `battery_value`, `battery_temp`, `battery_status`, `signalbar`, and `simcard_status`. GoAhead is an **optional** supplement. If local HTTP requires login or returns an unexpected body, `cfg` identity and configuration remain available and `u50_sources.goform` reads `unavailable`. Network type/operator are mapped only when returned. Battery, signal and SIM status stay under `u50_unverified` because binary strings alone do not establish units or values. SIM identifiers, passwords and keys are not requested.

## Isolated use

```sh
./build/zwrt-datad-armv7-candidate --u50-model u50pro --once
./build/zwrt-datad-armv7-candidate --u50-model u50s --bind 127.0.0.1 --port 19460
```

The ARM32 binary requires `--u50-model`; it cannot accidentally start the ZWRT collector. The candidate server binds only loopback and exposes `/healthz`, `/version`, `/state`, and `/capabilities`. It does not expose `/control`, `/ubus`, `/ota`, cloud, or WebShell. Build with `scripts/build-arm32-candidate.sh`; this produces an experimental ARMv7 musl binary without changing the ARM64 release asset or `version.json`.

Static analysis cannot establish the real device's `cfg` values, GoAhead authentication/response types, network and battery units, sampling costs, or safe control semantics. Verify on each real model before expanding the stable state contract or marking the template supported. No installation or service replacement is part of this branch.
