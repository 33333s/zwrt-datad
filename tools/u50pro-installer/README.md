# U50 Pro ARMv7 installer

A static Linux Go executable for MU5120. Downloads use native HTTPS, IPv4,
embedded public CA certificates, a pinned binary SHA-256 and exact byte count.
No curl/wget, Go modules outside the standard library, or device credentials.
An offline `--file` input goes through the same checks.

The executable embeds the repository's deployment transaction, service helper,
unit and starter. It still needs the firmware's standard shell utilities and
systemd. It is not an unlocker or a firmware flasher.

Build with Go 1.25+, Python 3, and optionally UPX:

```sh
python3 scripts/package-u50pro-go.py \
  --ca-bundle /etc/ssl/certs/ca-certificates.crt --upx
```

The datad candidate, release provenance and version must agree. Rebuild the
installer when publishing a new datad binary. Output defaults to
`build/deploy_kano`; no server directory is embedded by default. Supply an HTTPS
directory using the packager's `--base-url` or the installer's `--base-url` at
runtime. Changing that directory does not replace the pinned release.
Compressed and unpacked executables, checksums, build metadata and CMD
instructions are emitted. The packager only copies explicit source/assets.

Tests (Linux; no device needed):

```sh
cd tools/u50pro-installer
go test ./...
```

The Go supervisor starts its worker with a new session and independent log
descriptors, avoiding a remote shell background launch. Disconnecting ADB after
the worker starts does not cancel deployment. The worker runs the existing
transaction, persists its PID and final result, and requires both a successful
exit and a verified transaction result. The observer follows actual output and
reports elapsed time. After three minutes the worker requests transaction
rollback with SIGTERM and waits for recovery; it never force-kills a rollback.
This deadline is not a hard process-kill limit (a stuck kernel operation may
still delay recovery). Systemd calls have their own timeouts.

The transaction checks the actual executable hash, socket ownership and stable
PID/start time before success. Prior binaries are retained. Failures retain
logs/backups and report rollback failures. A read-only UBI root stays read-only;
session-only installs are explicitly reported. Other firmware shapes are
rejected before staging.

Installer r2 derives the procfs port from decimal 9460 (24F4), measures the
stability window with monotonic uptime, and reuses the verified PID for probes
instead of rescanning every process each second. Real Linux socket tests cover
accepting 9460, rejecting 9444, and the ten-second stability window.

For a read-only network verification on a Linux development host, build the
generated `build/u50pro-go-installer` module for the host architecture and run
`--verify-download /path/that/does/not/exist`. This only downloads and checks the
pinned ELF; it does not install or execute datad.
