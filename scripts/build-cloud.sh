#!/bin/sh
set -eu
cd "$(dirname "$0")/../cloud"
version=$(sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' ../version.json)
go test ./...
CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -trimpath -ldflags="-s -w -X main.version=$version" -o ../zwrt-datad-cloud-aarch64 .
