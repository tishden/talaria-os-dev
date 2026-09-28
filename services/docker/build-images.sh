#!/bin/sh
# Build the Talaria OS toolchain docker images on a clean machine:
#   talaria-qemu2        — the QEMU/KVM stand (test.sh / validate.sh / services' run.sh)
#   talaria-musl         — musl-gcc for static user binaries (examples/, user/)
#   talaria-alpine-build — Alpine build-base for static builds of large open-source
#                          programs against musl (PostgreSQL/MariaDB/C++ etc.)
# Usage: services/docker/build-images.sh [tag prefix]
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
P=${1:-}
docker build -t "${P}talaria-qemu2"        -f "$HERE/Dockerfile.qemu"         "$HERE"
docker build -t "${P}talaria-musl"         -f "$HERE/Dockerfile.musl"         "$HERE"
docker build -t "${P}talaria-alpine-build" -f "$HERE/Dockerfile.alpine-build" "$HERE"
echo "OK: ${P}talaria-qemu2, ${P}talaria-musl, ${P}talaria-alpine-build"
