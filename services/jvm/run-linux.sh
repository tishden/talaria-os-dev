#!/bin/sh
# The reference: the same JDK and the same Bench.class on Linux (Alpine, musl) — as many
# cores as Talaria gets. Only that comparison makes sense: one binary, one machine.
#   run-linux.sh [Bench arguments]     env: SMP=N (cores, default 4)
set -eu
cd "$(dirname "$0")"
JDK=$(ls -d build/jdk-21* | head -1)
SMP=${SMP:-4}
ARGS=${JVM_ARGS:-"-XX:+UseSerialGC -Xms32m -Xmx256m -XX:ReservedCodeCacheSize=48m -XX:CompressedClassSpaceSize=32m -XX:MaxMetaspaceSize=128m -XX:-UsePerfData"}
docker run --rm --cpuset-cpus="0-$((SMP - 1))" -v "$PWD":/w -w /w alpine:3.20 \
  "$JDK/bin/java" $ARGS -cp build/app Bench "$@"
