#!/bin/sh
# Build the kernel WITH FRAME POINTERS — so that a ring 0 profile unwinds into a
# stack instead of showing a single address.
#
# WHY A SEPARATE BUILD. Unwinding follows the RBP chain (`obs::unwind`): parsing
# `.eh_frame` from the timer tick is not an option. So the kernel stack is visible
# only if the kernel is built with `-C force-frame-pointers=yes`. We did not make
# that the default, and not for reasons of taste: `talaria.test=mbench` gives a
# 114-119 ns syscall without frame pointers and 123-124 ns with them — about 4 %
# on the kernel's most frequent path. An OS that exists for latency does not pay
# 4 % all the time for diagnostics needed once in a while.
#
# THE WORKLOAD STACK DOES NOT DEPEND ON THIS BUILD: it is read from the
# workload's own frames, and the ordinary kernel needs nothing for that. This
# script is needed exactly when "who called" is asked about KERNEL CODE.
#
#   kernel/build-prof.sh                 # -> kernel/target-prof/.../talaria
#   ... -kernel kernel/target-prof/x86_64-talaria/release/talaria
#
# The regression passes on such a kernel too, but latency numbers must not be
# measured on it: they are 4 % worse by construction.
set -eu
cd "$(dirname "$0")"
# RUSTFLAGS REPLACES `target.*.rustflags` from .cargo/config.toml entirely, so
# the whole set is listed — otherwise the linker script is lost and the build
# lands in the wrong place.
RUSTFLAGS="-C link-arg=-Tlinker.ld -C relocation-model=static -C force-frame-pointers=yes" \
  cargo -Z json-target-spec build --release --target-dir target-prof "$@"
echo "kernel with frame pointers: kernel/target-prof/x86_64-talaria/release/talaria"
echo "the ring 0 profile now unwinds: talaria.prof=200 or 'perf on' in the console"
