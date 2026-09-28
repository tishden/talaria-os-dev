#!/bin/sh
# Compile the benchmarks (app/*.java) with the same JDK that goes to Talaria, in an
# Alpine (musl) container. The classes go to build/app and into the image via mkdisk.sh.
set -eu
cd "$(dirname "$0")"
JDK=$(ls -d build/jdk-21* | head -1)
rm -rf build/app; mkdir -p build/app
docker run --rm -v "$PWD":/w -w /w alpine:3.20 \
  "$JDK/bin/javac" -d build/app app/*.java
ls -la build/app
