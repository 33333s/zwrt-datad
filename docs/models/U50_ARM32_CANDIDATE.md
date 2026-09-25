# U50 Pro / U50S ARM32 candidate

This branch adds an isolated, read-only candidate runtime for original-firmware U50 devices. It is **not** a production template. `device.api_template_supported` remains `0` until both devices are checked against live readbacks.

Firmware evidence: U50 Pro `u50prox62_full.bin` (SHA-256 `501b78f6f4b685a2caed6d5bae4b7193cb468bfb8dfdd3d26b7bf20225665148`) and U50S `BD_ZTE2SIMU50SV1.0.0B02` packages contain 32-bit ARM EABI5 root filesystems, `cfg`, `zte_topsw_*`, and GoAhead. The U50 Pro rootfs identifies as `sdxlemur-qti-distro-nogplv3-debug`; U50S identifies as `sdxprairie-mdm`. Neither inspected rootfs contains ZWRT `ubus`/`uci` tools. Their `zte_topsw_goahead` binaries differ, so a shared string name is only a lead, not a verified response contract. The raw firmware files and private partitions are not part of this repository.

The candidate reads only `/goform/goform_get_cmd_process` over loopback HTTP. The request uses a fixed allowlist: `model_name`, `wa_inner_version`, `network_type`, `network_provider_fullname`, `wan_active_band`, `battery_value`, and `signalbar`. It publishes device identity and a small set of clearly named network fields. Battery and signal values remain under `u50_unverified` because their units and semantics are not established. It never reads SIM identifiers, keys, or passwords. An HTML login response or unexpected JSON fails the probe instead of producing a healthy blank state.

Example for an isolated device test, after checking the actual model and local GoAhead access:

```sh
./build/zwrt-datad-armv7-candidate --u50-model u50pro --once
./build/zwrt-datad-armv7-candidate --u50-model u50s --bind 127.0.0.1 --port 19460
```

The candidate server exposes `/healthz`, `/version`, `/state`, and `/capabilities` on loopback only. It does not expose `/control`, `/ubus`, `/ota`, cloud, or WebShell. Build it with `scripts/build-arm32-candidate.sh`; the output is an experimental ARMv7 musl binary and does not change `version.json` or the ARM64 release asset.

Before formal support: verify the runtime `model_name`, GoAhead authentication, response types and units, sampling costs, Wi-Fi layout, battery presence, and behavior on both real models. Then map verified fields to the stable state contract, implement and independently validate only the required controls, and add model-specific fixtures. Do not set `api_template_supported=1` based on firmware strings alone.
