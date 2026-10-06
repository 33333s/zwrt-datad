# U50 Pro tiny download bootstrap

ARMv7/armhf executable using the firmware's `libcurl.so.4` and
`/lib/ld-linux-armhf.so.3`. Derived from the supplied tiny assembly downloader;
the maintained source is `tiny.S`. It contains no download URL or firmware library.
The current ELF is 876 bytes; it is not a general replacement for the curl CLI.

Build with Linux/WSL and `binutils-arm-linux-gnueabihf`:

```sh
sh tools/u50get/build.sh
python3 scripts/package-u50pro.py --output build/u50pro-tiny \
  --tiny-downloader build/u50get/u50get-tiny
```

Upload the generated `install-datad.sh` and `zwrt-datad-armv7` to the same
HTTP(S) directory. Deliver `bootstrap-u50pro.sh` separately (ADB or paste its
entire contents into the device shell). On the device:

```sh
export DATAD_BASE_URL='https://YOUR-SERVER/datad/u50pro'
sh /tmp/bootstrap-u50pro.sh
```

No curl or wget is required. The bootstrap embeds the compressed downloader,
checks its SHA-256, downloads and checks the exact installer script, and passes
the same temporary downloader to the installer for the daemon download. The
installer checks the daemon's pinned size, SHA-256, architecture and version
before using the existing backup/install/health/rollback transaction. Temporary
bootstrap files are removed after completion or failure. Repackage all files
together when updating a release. No server address is built in by default.

HTTP and redirects between HTTP/HTTPS are supported. HTTPS checks certificates
and hostnames by default. `DATAD_CA_FILE=/path/to/trusted.pem` selects a CA file;
`DATAD_TLS_INSECURE=1` explicitly skips TLS checks. The pinned content checks
remain active in both cases. Obtain the bootstrap itself from a trusted source:
its hashes cannot protect a bootstrap that has itself been replaced.

The raw helper interface is `u50get-tiny URL [CAFILE|--insecure] > output`.
It accepts only HTTP/HTTPS, follows at most five redirects, requires final 2xx,
uses 15-second connect/300-second total timeouts, and handles short writes/EINTR.
Failures return libcurl codes (22 HTTP error, 23 write failure, 28 timeout,
47 redirect loop, 60 certificate error). Raw shell redirection can leave a
partial file: use the checked bootstrap for installation.

Runtime tests (temporary local servers; no datad installation):

```sh
python3 tests/u50get_test.py --qemu /path/to/qemu-arm --sysroot /path/to/firmware-root
python3 tests/u50get_test.py --adb DEVICE_SERIAL
python3 tests/u50pro_download_test.py
```

Device tests use ADB reverse and a temporary test CA, never modify system trust,
and remove their files afterward. They cover both TLS modes, hostname checks,
redirects, non-HTTP schemes, HTTP errors, truncation, chunking and write failure.
Other firmware may have incompatible/missing libraries or CA certificates; the
standalone Go/offline installer remains available for those cases.
