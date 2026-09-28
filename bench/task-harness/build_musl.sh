#!/bin/bash
# Build the challenge's sender/receiver (~/sources/task/transport, the udp backend) statically
# against musl to run on Talaria: g++ from the talaria-alpine-build image (services/docker),
# base 0x10000000.
# Usage: build_musl.sh [TASK_ROOT]   -> TASK_ROOT/transport/bin-musl/{sender,receiver}
set -euo pipefail
TASK=${1:-$HOME/sources/task}
docker run --rm -v "$TASK":/t -w /t "${IMG:-talaria-alpine-build}" sh -c '
set -e
mkdir -p transport/bin-musl
CXX="g++ -std=c++17 -O2 -mssse3 -static -no-pie -Iharness/include -Itransport/include -Itransport/io -Wl,-Ttext-segment=0x10000000"
IO="transport/io/udp_backend.cpp transport/io/xdp_backend.cpp transport/io/dpdk_backend.cpp transport/io/tcp_backend.cpp transport/io/tcp_dpdk_backend.cpp transport/io/tcp_xdp_backend.cpp"
[ -f transport/io/talaria_backend.cpp ] && IO="$IO transport/io/talaria_backend.cpp"
$CXX -o transport/bin-musl/sender   transport/src/sender.cpp   $IO
$CXX -o transport/bin-musl/receiver transport/src/receiver.cpp $IO
ls -la transport/bin-musl/
'
