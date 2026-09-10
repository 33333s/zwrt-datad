#!/bin/bash
# Pinned OpenSSL LTS, built with the same musl toolchain as the release binary.
set -euo pipefail
VERSION=3.5.8
SHA256=a8f84a39918ec6415ce765d9b429d313ba97b8143169c172e734b9514464f5b2
TC="$HOME/aarch64--musl--stable-2025.08-1/bin"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/zwrt-datad/openssl-$VERSION-aarch64-musl"
PREFIX="$CACHE/install"
if [ ! -f "$PREFIX/lib/libcrypto.a" ]; then
    mkdir -p "$CACHE"
    ARCHIVE="$CACHE/openssl-$VERSION.tar.gz"
    [ -f "$ARCHIVE" ] || curl --fail --location --retry 3 --output "$ARCHIVE" "https://www.openssl.org/source/openssl-$VERSION.tar.gz" >&2
    echo "$SHA256  $ARCHIVE" | sha256sum -c - >&2
    tar -xzf "$ARCHIVE" -C "$CACHE"
    cd "$CACHE/openssl-$VERSION"
    PATH="$TC:$PATH" ./Configure linux-aarch64 --cross-compile-prefix=aarch64-linux- \
        --prefix="$PREFIX" --libdir=lib no-shared no-module no-dso no-tests no-apps \
        >"$CACHE/build.log" 2>&1
    PATH="$TC:$PATH" make -j"${BUILD_JOBS:-4}" build_libs >>"$CACHE/build.log" 2>&1
    PATH="$TC:$PATH" make install_dev >>"$CACHE/build.log" 2>&1
    install -m 644 LICENSE.txt "$PREFIX/LICENSE.txt"
fi
printf '%s\n' "$PREFIX"
