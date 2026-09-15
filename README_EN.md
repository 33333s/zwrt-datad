# zwrt-datad

A unified data and control service for ZTE ARM64 5G routers.

[中文](README.md) · [Latest release](https://github.com/33333s/zwrt-datad/releases/latest) · [API documentation](docs/API.md)

## Overview

`zwrt-datad` runs locally on the router. It reads `ubus`, `uci`, `sysfs`, and selected device logs, normalizes model-specific interfaces into a stable JSON state, and exposes that state to UFI, WebUI clients, scripts, and other local services over HTTP and SSE.

The project is implemented in Rust. Device templates isolate firmware differences so consumers do not need to poll vendor APIs or parse logs independently. datad is limited to device data, device control, and its own signed updates; it does not include a frontend, plugin system, or UFI application logic. The previous C/Go implementation is retained on the `c` branch and is no longer used for new releases.

## Features

- Aggregates device, system, CPU, memory, storage, thermal, battery, and runtime state
- Aggregates SIM, cellular, signal, band, traffic, QoS, Wi-Fi, client, and SMS data
- Provides complete JSON snapshots through `GET /state` and change events through `GET /events`
- Normalizes fields through model templates and reports available operations through `/capabilities`
- Exposes constrained cellular, Wi-Fi, APN, SMS, power, and device controls through `POST /control`
- Provides access to the device's registered ubus objects for trusted management clients
- Includes optional isolated neighbor-cell collection with resource and expiry limits
- Includes optional NMS cloud connectivity and remote service entry points
- Verifies datad self-updates with Ed25519 signatures and SHA-256 hashes
- Ships as a single statically linked Rust ARM64 process with a one-second default sampling interval

## Supported devices

| Model | Product |
| --- | --- |
| `MU5250` | U60 Pro |
| `MC8532B` | G5 Pro |
| `MU5252` | TopFlow |
| `MC7523` | G5 Max WiFi |

At runtime, `device.api_template_supported = 1` means that a supported template was selected. Each model only exposes state blocks that the device actually supports. Consumers should test for field presence instead of inventing `0`, `-1`, or empty placeholder objects.

See [`docs/models/`](docs/models/) for model-specific data sources and behavior. An unknown model may use the compatibility template, but that does not mean the device is officially supported.

## One-command install or upgrade

Run as root on an ARM64/aarch64 device with writable `/data` storage:

```sh
curl -4fL --retry 3 \
  'https://github.com/33333s/zwrt-datad/releases/latest/download/install-datad.sh' \
  -o /tmp/install-datad.sh && \
sh /tmp/install-datad.sh
```

Run the same command again to upgrade. The installer:

1. Downloads the release binary and verifies its pinned SHA-256.
2. Starts the candidate on a temporary port and checks `/healthz` and `/state`.
3. Backs up the existing installation and atomically updates `/data/zwrt-datad`.
4. Removes duplicate legacy startup entries and writes one command to `/etc/rc.local`.
5. Starts the production service and verifies ports 9460/9461 and the single-process invariant.
6. Restores the previous files and service if any step fails.

datad does not install its own `/etc/init.d` script. The installer requires `curl`, `sha256sum`, `awk`, `cmp`, `stat`, `flock`, `mktemp`, `readlink`, and `od` on the device.

## Service management

```sh
sh /data/zwrt-datad/service.sh status
sh /data/zwrt-datad/service.sh start
sh /data/zwrt-datad/service.sh restart
sh /data/zwrt-datad/service.sh stop
```

Default paths and endpoints:

- Binary: `/data/zwrt-datad/zwrt-datad`
- Log: `/data/zwrt-datad/zwrt-datad.log`
- PID file: `/data/zwrt-datad/zwrt-datad.pid`
- Loopback API: `http://127.0.0.1:9460`
- LAN API: `http://<device-ip>:9461`

## Quick check

```sh
curl -fsS http://127.0.0.1:9460/healthz
curl -fsS http://127.0.0.1:9460/version
curl -fsS http://127.0.0.1:9460/state
curl -N http://127.0.0.1:9460/events
```

Port 9460 is the loopback API. The LAN API on port 9461 requires a Bearer Token obtained through `/auth/login` or `/auth/exchange`; see [`docs/API.md`](docs/API.md) for authentication details.

> **Security:** `POST /ubus/call` can invoke runtime-registered ubus methods, including writes that may reconfigure networking, disconnect the device, or reboot it. Expose it only to trusted management clients and enforce confirmation and policy in the caller.

## Documentation

- [`docs/API.md`](docs/API.md): HTTP, SSE, authentication, and command-line options
- [`docs/STATE_SCHEMA.md`](docs/STATE_SCHEMA.md): state schema contract
- [`docs/CONTROL_API.md`](docs/CONTROL_API.md): control actions and safety boundaries
- [`docs/models/`](docs/models/): supported device templates
- [`docs/RUNTIME.md`](docs/RUNTIME.md): runtime, logs, and service management
- [`docs/NEIGHBOR.md`](docs/NEIGHBOR.md): optional neighbor-cell collection
- [`docs/CLOUD.md`](docs/CLOUD.md): optional NMS cloud connection

## License and contributors

Licensed under the [MIT License](LICENSE). See [`CONTRIBUTORS.md`](CONTRIBUTORS.md) for project credits.
