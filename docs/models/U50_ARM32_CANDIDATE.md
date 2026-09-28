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
| `cfg get network_type`, `network_provider_fullname` | `net.type` and `net.operator` when nonempty; verified by read-only U50S device queries |

Both GoAhead binaries contain the same read keys `model_name`, `wa_inner_version`, `network_type`, `network_provider_fullname`, `network_provider`, `battery_value`, `battery_temp`, `battery_status`, `signalbar`, and `simcard_status`. GoAhead is an **optional** supplement. If local HTTP requires login or returns an unexpected body, `cfg` identity and configuration remain available and `u50_sources.goform` reads `unavailable`. Network type/operator prefer nonempty GoAhead values, then fall back to the live-verified `cfg` keys. On the tested U50S, unauthenticated GoAhead returned an empty `network_type` while `cfg get network_type` returned `LTE`. Battery, signal and SIM status stay under `u50_unverified` because binary strings alone do not establish units or values. SIM identifiers, passwords and keys are not requested.

## Isolated use

```sh
./build/zwrt-datad-armv7-candidate --u50-model u50pro --once
./build/zwrt-datad-armv7-candidate --u50-model u50s --bind 127.0.0.1 --port 19460
```

The ARM32 binary requires `--u50-model`; it cannot accidentally start the ZWRT collector. The candidate server binds only loopback and exposes `/healthz`, `/version`, `/state`, and `/capabilities`. It does not expose `/control`, `/ubus`, `/ota`, cloud, or WebShell. Build with `scripts/build-arm32-candidate.sh`; this produces an experimental ARMv7 musl binary without changing the ARM64 release asset or `version.json`.

Static analysis cannot establish the real device's `cfg` values, GoAhead authentication/response types, network and battery units, sampling costs, or safe control semantics. Verify on each real model before expanding the stable state contract or marking the template supported. No installation or service replacement is part of this branch.

## Expanded read and OEM write bridge

The U50S read-only snapshot uses live-checked `cfg` keys for LTE bars/RSRP/RSRQ/SNR, battery percentage/temperature, cellular byte rates and counters, Wi-Fi enabled state, connected client count, SIM slot and modem state. The current WebUI's `transUnit(rate, true)` converts the rate from bytes per second to bits per second for display; datad keeps the source bytes-per-second value. Absent fields are omitted, not changed to zero. `/events` emits the same state snapshot on content changes, including an immediate initial event.

The OEM WebUI responds with empty status values when a loopback request sends `Host: 127.0.0.1`; datad keeps the TCP destination on loopback but uses the validated LAN IP in `Host` and `Referer`, as the original WebUI does. `cfg` remains the fallback when GoAhead cannot return a field. The installed 0.10.19 binary predates this expanded mapping; this document describes the branch under development.

For the U50S only, `--u50-enable-writes` adds guarded loopback routes. They are absent by default and are not available for U50 Pro:

- `POST /auth/login` with `{ "password": "..." }` uses the current OEM SHA-256 LD challenge, confirms `loginfo=ok`, then returns a random 15-minute Bearer token. The password and OEM session remain in memory only.
- `GET /oem/read?cmd=key1,key2` requires that token and forwards bounded, explicit OEM reads.
- `POST /control` with `{ "action":"u50.oem.goform", "goform_id":"SET_DEVICE_LED", "params":{"night_mode_switch":"0"}, "confirm":true }` requires the token. It accepts only action IDs present in the current U50S WebUI, validates field names/sizes, computes the fresh OEM RD challenge response, and returns `verified:false` until a separate readback proves the effect.
- `POST /auth/logout` invalidates the datad token. `/capabilities` lists the OEM IDs only when write compatibility is explicitly enabled.

These routes provide a bounded compatibility layer for the current OEM action set; they do not imply that every vendor command has been individually verified on hardware. The borrowed device's live deployment remains read-only while owner-admin login and reversible write testing are pending. Firmware upgrade, reboot, network disconnect, SMS transmission, and other consequential commands must not be used as smoke tests.
