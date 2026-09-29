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

The OEM WebUI responds with empty status values when a loopback request sends `Host: 127.0.0.1`; datad keeps the TCP destination on loopback but uses the validated LAN IP in `Host` and `Referer`, as the original WebUI does. `cfg` remains the fallback when GoAhead cannot return a field. Read-only U50S device checks confirmed the mapping; installation and service lifecycle must be verified separately.

For the U50S only, `--u50-enable-writes` adds guarded loopback routes. They are absent by default and are not available for U50 Pro:

- `POST /auth/login` with `{ "password": "..." }` uses the current OEM uppercase-hex SHA-256 LD challenge, confirms `loginfo=ok`, then returns a random 15-minute Bearer token. The password and OEM session remain in memory only.
- `GET /oem/read?cmd=key1,key2` requires that token and forwards bounded OEM reads. Up to 16 validated extra query parameters (for example `page` and `data_per_page`) are forwarded for OEM paged resources such as SMS. The raw response is only available to the authenticated caller; it is not copied into the public `/state`.
- `POST /control` with `{ "action":"u50.oem.goform", "goform_id":"SET_DEVICE_LED", "params":{"night_mode_switch":"0"}, "confirm":true }` requires the token. It accepts only action IDs present in the current U50S WebUI, validates field names/sizes, computes the fresh OEM RD challenge response, and returns `verified:false` until a separate readback proves the effect.
- `POST /auth/logout` invalidates the datad token. `/capabilities` lists the OEM IDs only when write compatibility is explicitly enabled.

These routes provide a bounded compatibility layer for the current OEM action set; they do not imply that every vendor command has been individually verified on hardware. Only same-value WebUI language and upgrade-notice writes were verified in a temporary opt-in process. Firmware upgrade, reboot, network disconnect, SMS transmission, and other consequential commands must not be used as smoke tests.

## Optional NMS read-only panel

For the current NMS device-reported onboarding flow, create the pending MQTT credentials in the target NMS account, then pass `{ "username":"enr_…", "password":"…" }` on standard input to `zwrt-datad --u50-model u50s --u50-enroll-dir /etc_rw/zwrt-datad`. This one-time operation requires a live `cfg` modem MSN, serial number, or valid IMEI, derives a stable UUID in memory, and saves `/etc_rw/zwrt-datad/cloud.json` as a private 0600 file. The raw hardware identifier is never included in public `/state`, MQTT telemetry, the saved config, or CLI output. Existing `cloud.json` is never overwritten by enrollment. Keep the issued MQTT password out of shell arguments, logs, and repository files.

The `arm32` branch includes the mainline NMS `datad_panel` stream. U50 uses a separate collector, so panel support requires an explicit private `cloud.json` path via `--u50-panel-config /path/to/cloud.json` when starting the U50 server. The file must be a regular file with no group/other permissions, and must contain valid enabled MQTT credentials and `remote_enabled: true`. Omitted broker and platform addresses use the mainline free NMS defaults; the member remote origin is added at runtime only for that default pair. Saved custom addresses are retained. No cloud connection or panel capability is started without the flag. `--once` cannot use it.

In this mode the device advertises only `datad.panel`, with no remote services, datad self-update, WebShell, or TCP management proxy. Remote commands other than `datad_panel` are rejected even if the file lists services. Panel frames reuse the mainline field allowlist and read-only WSS protocol; U50-specific raw `u50_cfg` and `u50_unverified` blocks are not exported. A live U50-to-NMS panel session and persistent startup require separate device and account verification.

The next candidate also maps the current firmware's LTE band/channel, Wi-Fi chip/modem thermal zones, traffic-limit switch, five-GHz/band-steering flags, and per-chip client counts when their OEM values are present. `/state` omits absent fields and keeps device identifiers and passwords out of the public snapshot. The OEM action bridge remains optional and reports `verified:false` for generic writes; clients must read back the affected OEM state to establish the result.

The SA signal mapping reads `nr5g_action_band`, `nr5g_action_channel`, `Z5g_rsrp` and `Z5g_SINR` from OEM `cfg`. Numeric fields are range-checked; a nonnumeric NR PCI is omitted rather than emitted as zero. `net.nr_snr` keeps the vendor decimal string because the shared state contract uses a string for this field.

## Full mainline-shaped state (v0.10.39)

The U50S `/state` now carries the same blocks as the ZWRT collector wherever the hardware provides the data. Sources are the OEM `cfg` store (one `cfg show` per sample instead of one `cfg get` per key; only mapped keys are kept, passwords and cookies are dropped) and ordinary Linux interfaces (procfs/sysfs, `ip`, `iw`). No ubus/uci is used.

| Block | Source |
|---|---|
| `system` (`uptime`, `cpu_usage`, `cpu_temp`, `mem_*`, `hostname`, `fw`, `imei`) | `/proc`, thermal zone `cpu0-0-usr`, `cfg` |
| `runtime` (per-core CPU, frequency, memory, storage of `/etc_rw`, connections, throughput, link rates, thermal zones) | the shared mainline sampler; the LAN bridge is `bridge0` |
| `thermal` (`cpu_celsius`, `zones[]`, `protection`) | `/sys/class/thermal`. `-273000`, unreadable zones, PMIC `*-lvl*` pseudo-zones, `battery_zte` and the `-lowf` duplicates are dropped |
| `battery` (`percent`, `temp` from `cfg`; `charging`, `health`, `bat_uv/ua`, `chg_uv/ua`, `cycle_count`, `capacity_mah`) | `cfg` and `/sys/class/power_supply`; `bat_ua` is positive while charging |
| `interfaces.{lan,wan4,wan6}` | `ip -o addr` (ubus-shaped `ipv4[]/ipv6[]/dns[]`), so MQTT `upstream` addresses and the NMS panel work unchanged |
| `clients` (`wifi`, `lan`, `list[]`) | `iw dev wlan0 station dump` (the AP's own address is skipped) + dnsmasq leases for names; USB/wired neighbours only while `rndis0` has carrier |
| `net`: `mcc/mnc/plmn`, `roaming`, `roaming_allowed`, `nr_pci`, `nr_cell_id`, `nr_tac`, `nr_rsrq/rssi/bw`, band locks, `*_supported_bands`, `band_capabilities` | `cfg` |
| `sim` (`imsi`, `iccid`, `msisdn`, `state`), `traffic` (day/total/limit/peak), `wlan` (`ssid`, `enc`), `dhcp` (range/lease/netmask) | `cfg` |

**Hexadecimal values.** The OEM WebUI renders `cell_id`, `nr5g_cell_id` and `lte_pci`/`nr5g_pci` with `parseInt(value, 16)`, so datad decodes them as hex (a stored `384` is PCI 900). `nr5g_tac` comes from the same adapter and is decoded the same way; that last assumption is not proven (the WebUI never displays a TAC), so verify it against the operator before using it for positioning. This firmware has no LTE TAC key in its `cfg` store, so `net.lte_tac` is absent.

Band capabilities use the factory lists (`lte_band_1_64_factory` bitmask, `nr5g_*_band_factory`); `complete=false` and no supported-band strings when any list is missing. Unreadable fields are omitted rather than reported as 0.


## Boot autostart on the U50S (systemd)

The U50S root filesystem is read-only, has no `/etc/rc.local`, and no firmware script executes anything from the writable `/etc_rw`, `/data` or `/systemrw` volumes. Init is systemd (239), so autostart is a unit added to the root filesystem:

```sh
mount -o remount,rw /
cat > /etc/systemd/system/zwrt-datad.service <<'EOF'
[Unit]
Description=zwrt-datad (U50S read-only state and NMS panel)
ConditionPathExists=/etc_rw/zwrt-datad/zwrt-datad
ConditionPathExists=/etc_rw/zwrt-datad/cloud.json
After=rcS-zte-server.service

[Service]
Type=simple
ExecStart=/etc_rw/zwrt-datad/zwrt-datad --u50-model u50s --u50-panel-config /etc_rw/zwrt-datad/cloud.json --bind 127.0.0.1 --port 9460
Restart=on-failure
RestartSec=10
TimeoutStopSec=5
Nice=5

[Install]
WantedBy=multi-user.target
EOF
ln -s ../zwrt-datad.service /etc/systemd/system/multi-user.target.wants/zwrt-datad.service
sync; mount -o remount,ro /; systemctl daemon-reload
```

`Restart=on-failure` also covers the first seconds after boot when the OEM `cfg` store is not ready yet (datad exits until `cfg get model_name` works). The binary stays in `/etc_rw/zwrt-datad/`, so datad updates never touch the root filesystem again. `/etc_rw/zwrt-datad/service.sh` now only drives the unit (`start|stop|restart|status`). A firmware upgrade rewrites the root filesystem and removes the unit; repeat the steps above afterwards. Removal: remount read-write, delete the unit and the symlink, remount read-only.

## NMS remote terminal (opt-in, v0.10.41)

The mainline WebShell (PTY-backed, 4 sessions, 15 min idle, 1800 s cap) is available on the U50S panel runtime, but only through the NMS remote channel: the U50 server has no local `/webshell` route (ADB already provides a local shell). It stays off unless the owner makes **two** independent decisions:

1. start datad with `--u50-enable-webshell` (together with `--u50-panel-config`), and
2. set `"remote_webshell_enabled": true` in the private `cloud.json` (with `remote_enabled: true`).

Without both, a `webshell` request is answered `panel_only` / `webshell_disabled`. Proxies, remote control and updates remain rejected in this runtime. When enabled the device advertises `datad.webshell`, and NMS accepts the terminal only for the bound owner as for mainline devices. To enable on the device, add the flag to `ExecStart` in `/etc/systemd/system/zwrt-datad.service` (remount `/` read-write for the edit, then read-only again), edit `cloud.json`, and `systemctl daemon-reload && sh /etc_rw/zwrt-datad/service.sh restart`.

## Self-update (OTA), same as mainline (v0.10.44)

The U50 runtime now has the mainline update stack: the loopback `/ota/config`, `/ota/status`, `/ota/check`, `/ota/update` routes, the background auto-update (first check after 90 s, then every 6 h, installs after two idle minutes; `ZWRT_DATAD_OTA_DISABLE_AUTO=1` disables the background task), and the NMS `datad.update.check` / `datad.update.install` commands (advertised as `datad.update` when the panel config is active). Sources, Ed25519 verification against the embedded public key and the version/SHA pinning are the same code as ARM64.

Differences that come from the device:

- **Own signed manifest and update channel.** ARM32 lives on the `arm32` branch and is not part of `main`. Its releases (`scripts/publish-arm32-release.sh`) use the mainline naming (tag `vX.Y.Z`, title `zwrt-datad vX.Y.Z`, one shared version counter; an existing tag is refused) and carry `zwrt-datad-armv7`, `.sha256`, `update-armv7.json` / `.sig`, `install-datad-armv7.sh` and `version.json`. They are published with `--latest=false`, because GitHub's *latest* release is the ARM64 line whose `update.json` every ARM64 device reads. U50 devices read the rolling prerelease `arm32-latest` (`https://github.com/33333s/zwrt-datad/releases/download/arm32-latest/update-armv7.json`), which the same script refreshes (binary and installer first, signed manifest last). The ARM64 `update.json` and every deployed ARM64 datad are untouched. v0.10.44 and older U50 builds looked at `releases/latest`; once mainline publishes a release without ARMv7 assets their update check fails, and they need one manual update to a build that uses the channel.
- **The daemon downloads the binary itself.** The firmware has no curl/wget, so datad fetches `zwrt-datad-armv7`, checks size and SHA-256 against the signed manifest, and stages it as `<data-dir>/zwrt-datad.new` (`--u50-data-dir`, default `/etc_rw/zwrt-datad`, same volume as the executable). The installer only verifies the pinned hash and `--version`, swaps atomically and restarts.
- **systemd.** The installer runs as the transient unit `zwrt-datad-ota` (`systemd-run`), so restarting `zwrt-datad.service` does not kill it. It waits until the service runs the new file and stays up, and otherwise restores the previous binary.
- **Storage gate.** The ~15 MB `/etc_rw` volume needs 7 MiB free (instead of 64 MiB) before an install starts.

The first move to an OTA-capable build must be manual; later versions update themselves.
