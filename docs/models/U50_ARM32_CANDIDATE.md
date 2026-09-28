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

The ARM32 binary requires `--u50-model`; it cannot accidentally start the ZWRT collector. The candidate server binds only loopback and exposes `/healthz`, `/version`, `/state`, `/events`, and `/capabilities`. It does not expose `/ubus`, `/ota`, or WebShell. The OEM write bridge is an explicit U50S-only opt-in. Build with `scripts/build-arm32-candidate.sh`; this produces an experimental ARMv7 musl binary without changing the ARM64 release asset.

Static analysis cannot establish the real device's `cfg` values, GoAhead authentication/response types, network and battery units, sampling costs, or safe control semantics. Verify on each real model before expanding the stable state contract or marking the template supported. No installation or service replacement is part of this branch.

## Expanded read and OEM write bridge

The U50S read-only snapshot uses live-checked `cfg` keys for LTE bars/RSRP/RSRQ/SNR, battery percentage/temperature, cellular byte rates and counters, Wi-Fi enabled state, connected client count, SIM slot and modem state. The current WebUI's `transUnit(rate, true)` converts the rate from bytes per second to bits per second for display; datad keeps the source bytes-per-second value. Absent fields are omitted, not changed to zero. `/events` emits the same state snapshot on content changes, including an immediate initial event.

The OEM WebUI responds with empty status values when a loopback request sends `Host: 127.0.0.1`; datad keeps the TCP destination on loopback but uses the validated LAN IP in `Host` and `Referer`, as the original WebUI does. `cfg` remains the fallback when GoAhead cannot return a field. U50S 0.10.22 has been installed and checked on the borrowed device, but it is not a running service.

For the U50S only, `--u50-enable-writes` adds guarded loopback routes. They are absent by default and are not available for U50 Pro:

- `POST /auth/login` with `{ "password": "..." }` uses the current OEM uppercase-hex SHA-256 LD challenge, confirms `loginfo=ok`, then returns a random 15-minute Bearer token. The password and OEM session remain in memory only.
- `GET /oem/read?cmd=key1,key2` requires that token and forwards bounded OEM reads. Up to 16 validated extra query parameters (for example `page` and `data_per_page`) are forwarded for OEM paged resources such as SMS. The raw response is only available to the authenticated caller; it is not copied into the public `/state`.
- `POST /control` with `{ "action":"u50.oem.goform", "goform_id":"SET_DEVICE_LED", "params":{"night_mode_switch":"0"}, "confirm":true }` requires the token. It accepts only action IDs present in the current U50S WebUI, validates field names/sizes, computes the fresh OEM RD challenge response, and returns `verified:false` until a separate readback proves the effect.
- `POST /auth/logout` invalidates the datad token. `/capabilities` lists the OEM IDs only when write compatibility is explicitly enabled.

These routes provide a bounded compatibility layer for the current OEM action set; they do not imply that every vendor command has been individually verified on hardware. The borrowed device's installed binary remains read-only by default; only same-value WebUI language and upgrade-notice writes were verified in a temporary opt-in process. Firmware upgrade, reboot, network disconnect, SMS transmission, and other consequential commands must not be used as smoke tests.

## Optional NMS read-only panel

The `arm32` branch includes the mainline NMS `datad_panel` stream. U50 uses a separate collector, so panel support requires an explicit private `cloud.json` path via `--u50-panel-config /path/to/cloud.json` when starting the U50 server. The file must be a regular file with no group/other permissions, and must contain valid enabled MQTT credentials and `remote_enabled: true`. Omitted broker and platform addresses use the mainline free NMS defaults; the member remote origin is added at runtime only for that default pair. Saved custom addresses are retained. No cloud connection or panel capability is started without the flag. `--once` cannot use it.

In this mode the device advertises only `datad.panel`, with no remote services, datad self-update, WebShell, or TCP management proxy. Remote commands other than `datad_panel` are rejected even if the file lists services. Panel frames reuse the mainline field allowlist and read-only WSS protocol; U50-specific raw `u50_cfg` and `u50_unverified` blocks are not exported. NMS registration and a live U50 end-to-end panel session remain unverified. The installed U50S 0.10.22 binary does not contain this optional integration.

The next candidate also maps the current firmware's LTE band/channel, Wi-Fi chip/modem thermal zones, traffic-limit switch, five-GHz/band-steering flags, and per-chip client counts when their OEM values are present. `/state` omits absent fields and keeps device identifiers and passwords out of the public snapshot. The OEM action bridge remains optional and reports `verified:false` for generic writes; clients must read back the affected OEM state to establish the result.

The SA signal mapping reads `nr5g_action_band`, `nr5g_action_channel`, `Z5g_rsrp` and `Z5g_SINR` from OEM `cfg`. Numeric fields are range-checked; a nonnumeric NR PCI is omitted rather than emitted as zero. `net.nr_snr` keeps the vendor decimal string because the shared state contract uses a string for this field.
