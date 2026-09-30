# U50 Pro / U50S ARM32

This branch runs the mainline datad on the original ARM32 firmware of the ZTE U50S and U50 Pro. `device.api_template_supported` stays `0`: the model has no mainline template yet, and only the features listed below are implemented. The U50 Pro (MU5120, firmware `EN_CN_MU5120AV1.0.0B17`, GoAhead `BD_FLYMODEMMU5120V1.0.1B12`, kernel `sdxlemur 5.4.226`) was verified read-only on a real device; see the section at the end.

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


## Controls added in v0.10.47

Mapped onto OEM goform actions (same request shapes as the firmware WebUI):

- `band.set_lte` / `band.set_nr_sa` / `band.set_nr_nsa` — `BAND_SELECT` / `WAN_PERFORM_NR5G_SANSA_BAND_LOCK`.
- `cell.lock_lte` / `cell.lock_nr` / `cell.unlock_all` — `LTE_LOCK_CELL_SET` / `NR5G_LOCK_CELL_SET`.
- `traffic.set_limit` / `traffic.set_clear_day` — `DATA_LIMIT_SETTING` (the whole block is re-sent, untouched fields preserved); state `traffic.limit` / `traffic.clear_day` in the mainline shape.
- `client.block` / `client.unblock` — OEM blacklist (`setDeviceAccessControlList`, `AclMode` 2); refused while the firmware is in whitelist mode. `client.kick` blocks the station briefly and re-allows it (the firmware has no plain disassociate call). `client.rename` — `EDIT_HOSTNAME`. State `clients` (`total/wifi/lan/list/blocked`) comes from `station_list`, `lan_station_list` and `queryDeviceAccessControlList`.

Verified against a fake GoAhead in `tests/u50_platform_test.py`. Not yet exercised with a write on a real device: band/cell locks and traffic limits (they change live radio/billing behaviour).

## Controls added in v0.10.48

- `lan.set` — `DHCP_SETTING`: DHCP on/off, range and lease (whole hours). The LAN address/netmask cannot be changed: the firmware GoAhead only answers to `192.168.0.1`, which the daemon's own OEM channel needs. `dhcp_reboot_flag=1` is sent as the WebUI does.
- `lan.set_mtu` — `SET_DEVICE_MTU` (`tcp_mss` = mtu − 40).
- `wifi.set_module` — `SET_WIFI_INFO` master switch, read back from `wifi_onoff_state`.
- Not mapped: `dns.set` (the OEM has no LAN DNS call; DNS lives inside APN profiles), `wifi.configure` and the other Wi-Fi actions (the firmware is dual-chip and its per-chip SSID/security protocol is not verified yet).

Only exercised against the fake GoAhead so far.

## Reviewed panel read models (v0.10.51)

The on-demand NMS panel now obtains Wi-Fi and APN views from the OEM backend
instead of attempting ZWRT `uci`/`ubus` calls. Both views carry `writable: false`;
this change does not implement Wi-Fi configuration or APN writes. Unknown or
unavailable data stays unknown, and an invalid resource is omitted from the
panel instead of being presented as an empty successful configuration.

- Wi-Fi uses the original WebUI's single-command `queryAccessPointInfo`
  (`ResponseList`) read. The module switch remains in `state.wlan.enabled`.
  Main SSIDs are selected by
  `AccessPointIndex=0` and the OEM `Band` value (`b`/`a`), independently of chip
  ordering. Only SSID, authentication mode, enabled/hidden/PMF state, station
  limit, current country/channel and configured bandwidth policy are exposed.
  The country/channel lists contain only observed settings; they are not a
  complete hardware or regulatory capability catalog. Wi-Fi keys and raw OEM
  replies are never included.
- APN uses bounded `APN_config0..19`/`ipv6_APN_config0..19` batches and automatic
  profile/current-mode fields. The adapter parses the OEM serialized profiles
  into the same reviewed `id/name/apn/auth_mode/pdp_type/enabled` fields as the
  NMS panel. Embedded account names/passwords, raw profile strings and the OEM
  session are discarded. IDs identify read views and cannot be used for writes.
- The local read actions `wifi.status`, `wifi.dual_band_status`, `apn.list` and
  `client.access` are listed in `/capabilities`; they accept no parameters.
  `client.access` returns the same reviewed list/blocked/counts view as
  `state.clients`. Local Wi-Fi and APN reads also exclude credentials.
- DHCP leases use the OEM read key `dhcpLease_hour`, in hours. `state.dhcp`
  represents it as an explicit duration such as `24h`, so clients do not
  interpret the raw number as seconds. Missing/invalid hours are omitted.

The mappings and credential filtering are covered by synthetic OEM HTTP and
unit fixtures. They still require independent U50S readback and NMS-page
verification on the deployed firmware; U50 Pro remains unverified.

## U50 Pro (MU5120) on-device verification, 2026-09 (read-only)

The v0.10.49 armv7 binary was run from `/tmp` on a U50 Pro with `--u50-model u50pro` (one-shot and a short daemon on a loopback port, data dir under `/tmp`, auto-update disabled; no firmware configuration was changed). Everything the U50S runtime needs is present and answers:

- `cfg` (`/usr/bin/cfg`) serves `cfg show` with every mapped key of the state snapshot populated (`model_name=MU5120`, `net_select`, NR SA n78 registration, factory band lists, DHCP, traffic, `modem_msn` for enrollment identity). `admin_Password` is the same 64-hex-digit SHA-256 the OEM login needs.
- GoAhead (`zte_topsw_goahead`, ports 80/443) answers unauthenticated `goform_get_cmd_process` reads (`network_provider_fullname` empty, `network_provider=UNICOM` — the cfg fallback already covers it), serves the `LD` login challenge, and the daemon's own session login succeeded, so `state.sms` returns the real message list. The binary contains every mapped write ID, including `SET_BEARER_PREFERENCE`, `BAND_SELECT`, `WAN_PERFORM_NR5G_SANSA_BAND_LOCK`, `DHCP_SETTING` and `setDeviceAccessControlList`.
- Kernel paths match: `bridge0`/`rmnet_data0`/`rndis0`/`wlan0`, `/etc_rw/ztembb/configs/dnsmasq.leases`, `/sbin/ip`, `/usr/sbin/iw`, systemd 244 (the U50S unit file works unchanged), `battery`/`usb`/`charger_zte`/`statistics_zte` power-supply nodes. CPU usage appears from the second sample in daemon mode as on the U50S.

Firmware differences found on the device and fixed for it:

- **Thermal zones.** sdxlemur names its sensors `cpuss-0-usr`/`mdmq6-0-usr` instead of `cpu0-0-usr`, so `thermal.cpu_celsius`/`system.cpu_temp` were absent; both names are now candidates. `vbat` (battery voltage in mV), `socd` (a counter reading 0.02 °C) and `modem-beamer-usr` (a floating mmWave sensor reading ~86 °C on this sub-6-only device) are dropped like the other pseudo-zones.
- **Battery.** The `battery` node reports current **positive while charging** (the U50S node was negative), so `bat_ua` is normalised against the recognized charging state instead of unconditionally negated. `capacity_mah` falls back to `battery/charge_full` (9532 mAh here) because the U50 Pro has no `battery_zte/nominal_capacity_mah_mbb`; its `charger_connect` is served by the `usb` fallback.
- **Network mode readback.** The firmware stores the 4G+5G preference as `net_select=WL_AND_5G` while `SET_BEARER_PREFERENCE` still takes `4G_AND_5G`; `network.set_mode` now treats that readback as verified.

**Storage constraint (unresolved).** The U50 Pro `/etc_rw` is only ~5.5 MiB total (the U50S has ~15 MiB) and `/data` is a 2 MiB OEM volume. With the ~4.6 MiB binary installed, the 7 MiB OTA free-space gate can never pass and the staged `zwrt-datad.new` does not fit either, so self-update cannot install on the U50 Pro as provisioned. Deployment needs a maintainer decision (different volume, lower gate with `/tmp` staging, or manual-only updates); the checks in this verification all ran from `/tmp`.

## On-device modem DIAG transport (v0.10.55)

The kernel exposes QRTR (`AF_QIPCRTR`) and the modem runs its DIAG service on it, so datad can talk to the modem diag stack in-process, without USB, `diag_mdlog` or the OEM `diag-router`. `state.u50_diag` reports the reachability probe (`available`, `node`, `port`, `reason`) plus the modem build parsed from the `DIAG_VER_F` reply (`modem_build_date`, `modem_build_name` — e.g. `Sep 15 2025 23:47:41` / `olympic.` on the verified U50 Pro). The block is refreshed at most once a minute and degrades to an honest `available:false` with a reason when any step fails.

Transport facts, all verified on the device and cross-checked against the open-source `linux-msm/diag` router:

- **Discovery.** qrtr name service (v5.4 control packets: `HELLO=2`, `NEW_SERVER=4`, `NEW_LOOKUP=10`, 20-byte `{cmd,service,instance,node,port}`, wildcard `0`); the modem is node 3 and reassigns ports every boot (observed 16/17/19 across boots), so the DIAG CMD service (`0x1001`, instance 1) is looked up dynamically. Never port-scan to find it — see the note below.
- **Commands.** A bare diag command (no HDLC, no wrapper) sent as one QRTR DATA packet to that port; replies come NHDLC-wrapped as `7e 01 <len:le16> <payload> <7e>`. Framed or wrapped requests are answered `[0x13 bad-command][request echoed]`.
- **Read-only by design.** Only `DIAG_VER_F` is sent; log masks, streams and diag-id registration are deliberately untouched until stream handling is designed, so the modem's diag state is never modified. Heavy exploratory probing (repeated `HELLO` broadcasts, a fake CNTL publication) was once observed to drop the modem's qrtr service registrations until the next reboot, while cellular data stayed up; the shipped probe sends one `HELLO`+one lookup per minute.
- Safety note: an early exploratory port sweep across live QMI services on the modem crashed the device once. All probing must go through the name service and target only the discovered DIAG CMD address.
