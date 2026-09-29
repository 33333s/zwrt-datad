# U50 Pro / U50S ARM32

This branch runs the mainline datad on the original ARM32 firmware of the ZTE U50S (U50 Pro shares the code path but has not been exercised). `device.api_template_supported` stays `0`: the model has no mainline template yet, and only the features listed below are implemented.

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

## Runtime (v0.10.46): the mainline App on the U50S

Since v0.10.46 the U50S runs the same `App` as the ARM64 devices (cloud/NMS, OTA, WebShell, schedules, SMS forwarding, panel, history) and the same loopback HTTP API (`/state`, `/events`, `/control`, `/capabilities`, `/cloud/*`, `/ota/*`, `/webshell`). The platform-specific parts sit behind a few seams served by `u50_ctl.rs`: `state::collect` (OEM `cfg` + procfs/sysfs + GoAhead), `control::execute` (mapped OEM actions) and the SMS source in `sms.rs`. The former read-only "panel-only" runtime, its `/oem/read`/`/auth/*` routes and `--u50-enable-writes` are gone; those flags are still accepted and ignored so existing service definitions keep starting. State lives in `--u50-data-dir` (default `/etc_rw/zwrt-datad`, or the directory of a legacy `--u50-panel-config` path).

```sh
./zwrt-datad --u50-model u50s --once                                  # one state sample
./zwrt-datad --u50-model u50s --bind 127.0.0.1 --port 9460 [--u50-enable-webshell]
```

**The daemon's own OEM session.** OEM writes and message reads need a WebUI login. The firmware stores `admin_Password` as `SHA256(password)` in upper-case hex, which is exactly what the WebUI proves knowledge of (`SHA256(hash + LD)`). datad reads that root-only `cfg` value and logs in for itself; it never sees the password, never exposes the hash (not in `/state`, logs or replies) and keeps the session only in memory (re-login on expiry, one transparent retry). If the WebUI allows one admin session at a time, a person logging in at the same moment may be asked to retry.

**Controls** (`/control`, and NMS panel control when opted in): `device.reboot`, `device.poweroff`, `cellular.connect`, `cellular.disconnect`, `cellular.set` (`roaming` 0/1 → `SET_CONNECTION_MODE`, dial mode preserved; `connect_mode`; `enabled`), `network.set_mode` (`4G_AND_5G`, `Only_5G`, `Only_LTE`), `sim.set_slot`, `sms.send_raw`, `sms.delete`, `sms.mark_read`, plus the App-level `schedule.*`, `sms.forward.*`, `cloud.remote_features.set`, `state.refresh`, `state.set_interval`. Writes are read back where the firmware exposes the value (`verified`). Other mainline actions return 404 (`/capabilities` lists exactly what is implemented). Speed tests are intentionally not offered (the firmware has no curl, `speedtest.supported` is false).

**SMS.** `state.sms` (`unread`, `list[]`) comes from `sms_data_total` (device store and SIM, paged, decoded like mainline); `sms.send_raw` maps to `SEND_SMS` (UCS-2 hex, `encode_type=UNICODE`) and waits for the modem's verdict without retrying. SMS forwarding (HTTP/SMTP/phone, quotas, scheduled sends) is the mainline module unchanged.

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

- **Own signed manifest and update channel.** ARM32 lives on the `arm32` branch and is not part of `main`. Its releases (`scripts/publish-arm32-release.sh`) have their own tag namespace (tag `arm32-vX.Y.Z`, title `zwrt-datad-arm32 vX.Y.Z`), so the version numbers are independent of mainline and an existing tag is refused. They carry `zwrt-datad-armv7`, `.sha256`, `update-armv7.json` / `.sig`, `install-datad-armv7.sh` and `version.json`, and are published with `--latest=false`, because GitHub's *latest* release is picked by date and is the ARM64 line whose `update.json` every ARM64 device reads. U50 devices read the rolling prerelease `arm32-latest` (`https://github.com/33333s/zwrt-datad/releases/download/arm32-latest/update-armv7.json`), which the same script refreshes (binary and installer first, signed manifest last). The ARM64 `update.json` and every deployed ARM64 datad are untouched. v0.10.44 and older U50 builds looked at `releases/latest`; once mainline publishes a release without ARMv7 assets their update check fails, and they need one manual update to a build that uses the channel.
- **The daemon downloads the binary itself.** The firmware has no curl/wget, so datad fetches `zwrt-datad-armv7`, checks size and SHA-256 against the signed manifest, and stages it as `<data-dir>/zwrt-datad.new` (`--u50-data-dir`, default `/etc_rw/zwrt-datad`, same volume as the executable). The installer only verifies the pinned hash and `--version`, swaps atomically and restarts.
- **systemd.** The installer runs as the transient unit `zwrt-datad-ota` (`systemd-run`), so restarting `zwrt-datad.service` does not kill it. It waits until the service runs the new file and stays up, and otherwise restores the previous binary.
- **Storage gate.** The ~15 MB `/etc_rw` volume needs 7 MiB free (instead of 64 MiB) before an install starts.

The first move to an OTA-capable build must be manual; later versions update themselves.
