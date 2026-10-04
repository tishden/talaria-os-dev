#!/bin/bash
# The network block of the arm64 gate (docs/ARM64-PLAN.md §10.3 step 1): virtio-net over the
# tree's virtio-mmio windows, a tap, and the clients next to the guest. It runs INSIDE the
# stand image (talaria-qemu-aarch64: qemu, iproute, socat, python3) with NET_ADMIN and
# /dev/net/tun — `kernel/test-aarch64.sh` starts it and adds its totals; by hand:
#   docker run --rm --cap-add NET_ADMIN --device /dev/net/tun -v "$PWD:/w" -w /w \
#     -e TAL_IMG=kernel/target/aarch64-talaria/release/talaria.img talaria-qemu-aarch64 \
#     bash kernel/test-aarch64-net.sh
# The environment: TAL_IMG (the kernel image), TAL_ACCEL (the QEMU accelerator flags),
# TAL_KVM=1 (judge the timing lines too), TAL_DISK (the TFS1 volume), TAL_EXT2 (the ext2
# image with forksrv; empty — that check is skipped). Paths are relative to /w.
#
# What TCG changes. The emulator is 50-100 times slower than the machine, so a client must
# not give up early (socat's half-close timer is 0.5 s by default — `-t` below), and the
# COUNTS of the parking gates depend on how often two waiters meet: under TCG they are
# judged for their OUTCOME (no exit by the ceiling with data in hand), under KVM for the count.
set -u
IMG=${TAL_IMG:-kernel/target/aarch64-talaria/release/talaria.img}
ACCEL=${TAL_ACCEL:--cpu max}
KVM=${TAL_KVM:-0}
DISK=${TAL_DISK:-kernel/target/disk-a64.img}
EXT2=${TAL_EXT2:-}
PASS=0; FAIL=0
O=/tmp/o.log
ip tuntap add tap0 mode tap 2>/dev/null
ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
ip link show tap0 >/dev/null 2>&1 || { echo "  FAIL  net: no tap0 (the container needs --cap-add NET_ADMIN --device /dev/net/tun)"; echo "NET TOTAL: PASS=0 FAIL=1"; exit 1; }

# bootn SMP APPEND [DISK]: the guest in the background with a NIC on tap0 (and a COPY of DISK
# on virtio-blk), the console into $O; sets $Q. `waitfor` then waits for the mode's own line.
# NICDEV (virtio-net-device by default) names the card's transport; KERNEL and FWOPT boot
# through the firmware instead (the UEFI stub under edk2).
bootn() {
  rm -f $O; local drv=""
  if [ -n "${3:-}" ]; then cp "/w/$3" /tmp/run.img; drv="-drive id=d0,file=/tmp/run.img,format=raw,if=none -device virtio-blk-device,drive=d0"; fi
  qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp "$1" -m 1024 -display none -serial file:$O ${FWOPT:-} \
    -kernel "${KERNEL:-/w/$IMG}" -append "console=ttyAMA0 $2 talaria.smp=$1" \
    -netdev tap,id=n0,ifname=tap0,script=no,downscript=no \
    -device ${NICDEV:-virtio-net-device},netdev=n0,mac=52:54:00:12:34:56 $drv >/tmp/q.log 2>&1 &
  Q=$!
}
# waitfor PAT SECS: until the pattern is on the console (or QEMU is gone).
waitfor() {
  local i; for i in $(seq 1 $(($2 * 2))); do
    grep -aqE "$1" $O 2>/dev/null && return 0
    kill -0 $Q 2>/dev/null || return 1
    sleep 0.5
  done; return 1
}
stopq() { kill $Q 2>/dev/null; wait $Q 2>/dev/null; }
ok()   { echo "  PASS  $1"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL  $1"; FAIL=$((FAIL+1)); tail -3 $O 2>/dev/null | cut -c1-160 | sed 's/^/      /'; [ -s $O ] || tail -2 /tmp/q.log | sed 's/^/      q: /'; }
assert() { if grep -aqE "$1" $O 2>/dev/null; then ok "$2"; else bad "$2 (no /$1/)"; fi; }
refute() { if grep -aqE "$1" $O 2>/dev/null; then bad "$2 (found /$1/)"; else ok "$2"; fi; }
BAD='\[trap\]|PANIC|an unknown number'
udp() { printf '%s' "$1" | timeout 4 socat -T3 - udp:10.0.0.2:9998 2>/dev/null; }
num() { grep -ao "$1" $O 2>/dev/null | tail -1 | grep -o '[0-9]*$'; }

echo "== the network on arm64: virtio-net over virtio-mmio (a tap, the clients next to the guest) =="
# userver: a musl UDP echo through the socket syscalls; the default is POLLING (no line).
bootn 1 "talaria.mode=userver talaria.netprof=4"; waitfor "userver: the UDP echo" 60
assert "virtio-net: the instance 0 — the window 0xa00" "virtio-net: the card is found in a device-tree window"
for i in 1 2 3 4 5 6 7 8 9 10; do udp RINGRING >/dev/null; done
R=$(udp PONGPONG); sleep 2
[ "$R" = PONGPONG ] && ok "userver: the UDP echo through the socket syscalls (recvfrom/sendto from EL0)" || bad "userver: the UDP echo (got '$R')"
refute "the card.s line GSI" "pollring: without the knob the card's line is NOT laid on a virtio-mmio (polling is the default)"
P=$(num 'stack_passes/turnaround=[0-9]*')
if [ -n "$P" ] && [ "$P" -lt 30000 ]; then ok "pollring: the stack's passes per turnaround $P (an idle pass is cancelled by the receive ring)"; else bad "pollring: the passes per turnaround '$P' (< 30000 was expected)"; fi
L=$(num 'the_ring_lied=[0-9]*'); H=$(num 'by_the_timers_debt=[0-9]*')
if [ -n "$L" ] && [ -n "$H" ] && [ "$((L * 100))" -le "$H" ]; then ok "pollring: the flag \"the card is silent\" lied $L times over $H passes by a timer debt (< 1 %)"; else bad "pollring: the ring lied '$L' times over '$H' passes"; fi
refute "$BAD" "userver: no trap, no unknown syscall"
stopq

# The card's line: the SPI the device tree names for the window, through the GIC to
# `netsys::nic_irq`; the waiter parks and a delivery wakes it.
bootn 1 "talaria.mode=userver talaria.netirq=1 talaria.netprof=4"; waitfor "userver: the UDP echo" 60
for i in 1 2 3 4 5 6 7 8 9 10; do udp PARKPARK >/dev/null; done
R=$(udp PARKPARK); sleep 2
[ "$R" = PARKPARK ] && ok "netpark: the echo is not lost while the waiter sleeps" || bad "netpark: the echo (got '$R')"
assert "the card.s line GSI [0-9]+ -> the vector" "netirq: the card's line is laid (the SPI is the device tree's, not derived)"
assert "a waiter on a socket PARKS" "netpark: the waiter parks, a delivery wakes it"
P=$(num 'stack_passes/turnaround=[0-9]*')
if [ -n "$P" ] && [ "$P" -lt 50000 ]; then ok "netpark: the stack's passes per turnaround $P"; else bad "netpark: the passes per turnaround '$P' (< 50000 was expected)"; fi
T=$(grep -ao 'rx->tx .*p50=[0-9]*' $O | tail -1 | sed 's/.*p50=//')
if [ "$KVM" = 1 ]; then
  if [ -n "$T" ] && [ "$T" -lt 200000 ]; then ok "netpark: rx->tx p50 $T ns — woken by a DELIVERY, not by the parking's ceiling (1 000 000 ns)"; else bad "netpark: rx->tx p50 '$T' ns (the ceiling is 1 000 000)"; fi
else
  if [ -n "$T" ] && [ "$T" -lt 1000000 ]; then ok "netpark: rx->tx p50 $T ns under TCG — below the parking's ceiling (the number is for the stand)"; else bad "netpark: rx->tx p50 '$T' ns (the ceiling is 1 000 000: the delivery does not wake)"; fi
fi
refute "$BAD" "netpark: no trap (the SPI is handled and ended)"
stopq

# TCP: accept + epoll_wait + read + write from a binary on the disk, with the line and the parking.
bootn 1 "talaria.mode=diskserver talaria.netirq=1" "$DISK"; waitfor "the workload.s first syscall" 60; sleep 3
rm -f /tmp/p.*; PIDS=""
for i in 1 2 3 4; do ( printf 'GET /p%s HTTP/1.0\r\n\r\n' $i | timeout 15 socat -t10 -T12 - tcp:10.0.0.2:80 2>/dev/null > /tmp/p.$i ) & PIDS="$PIDS $!"; done
for p in $PIDS; do wait $p; done
N=0; for i in 1 2 3 4; do grep -q "Talaria epoll server" /tmp/p.$i 2>/dev/null && N=$((N+1)); done
[ $N = 4 ] && ok "diskserver: TCP with the parking — accept/epoll/read/write (4/4)" || bad "diskserver: TCP $N/4"
refute "$BAD" "diskserver: no trap, no unknown syscall"
stopq

# A thread per connection waiting in poll() (ppoll on arm64): the parked are woken by a delivery.
mt() { PIDS=""
  for i in 1 2 3 4; do ( exec 3<>/dev/tcp/10.0.0.2/80; for k in $(seq 1 $1); do printf 'Q\n' >&3; head -c 51 <&3 >/dev/null; done; exec 3<&- ) & PIDS="$PIDS $!"; done
  for p in $PIDS; do wait $p; done; }
bootn 1 "talaria.mode=mtsrv talaria.netsolo=1" "$DISK"; waitfor "mtsrv: a thread per connection" 60; sleep 1
mt 30; waitfor "sched: all the [0-9]+ task" 20; sleep 2
assert "mtsrv: DONE \(4 connections\)" "mtsrv: four threads, each in poll() on its own socket, serve 30 requests each"
W=$(num 'woken by a delivery [0-9]*'); T=$(num 'by the backstop [0-9]*'); W=${W:-0}; T=${T:-0}
if [ "$KVM" = 1 ]; then
  if [ "$W" -ge 20 ] && [ "$T" -le $((W / 4)) ]; then ok "netsolo: the parked were woken by a DELIVERY $W times, by the backstop $T"; else bad "netsolo: the delivery '$W', the backstop '$T' (the backstop must be a rarity)"; fi
else
  if [ "$T" -le $(((W + 3) / 4)) ]; then ok "netsolo: the parked were woken by a delivery $W times, by the backstop $T (the count is for the stand)"; else bad "netsolo: the delivery '$W', the backstop '$T'"; fi
fi
refute "WITH DATA in hand [1-9]" "netsolo: not a single exit by the ceiling with the readiness in hand"
refute "$BAD" "mtsrv: no trap, no unknown syscall"
stopq

# A fork per connection (the PostgreSQL path): `mode=proc`-style launch from ext2 WITH a NIC.
if [ -n "$EXT2" ] && [ -f "/w/$EXT2" ]; then
  bootn 1 "talaria.mode=forksrv" "$EXT2"; waitfor "forksrv: a fork per connection" 60; sleep 1
  rm -f /tmp/fk.*; PIDS=""
  for i in 1 2 3 4 5 6; do ( printf 'GET /fk%s HTTP/1.0\r\n\r\n' $i | timeout 25 socat -t20 -T22 - tcp:10.0.0.2:80 2>/dev/null > /tmp/fk.$i ) & PIDS="$PIDS $!"; done
  for p in $PIDS; do wait $p; done
  N=0; for i in 1 2 3 4 5 6; do grep -q "FORKSRV OK" /tmp/fk.$i 2>/dev/null && N=$((N+1)); done
  [ $N = 6 ] && ok "forksrv: a fork per connection — all the 6 backends got the processor and answered" || bad "forksrv: the answers $N/6"
  waitfor "the ownership was dropped at close [6-9]" 20
  D=$(num 'the ownership was dropped at close [0-9]*')
  if [ -n "$D" ] && [ "$D" -ge 6 ]; then ok "forksrv: the ownership of the socket was dropped by the closing of the descriptor $D times"; else bad "forksrv: the ownership was dropped '$D' times (>= 6 was expected)"; fi
  refute "$BAD|\[fault\]" "forksrv: no trap, no fault, no unknown syscall"
  stopq
else
  echo "  SKIP  forksrv: no ext2 image with the binary (TAL_EXT2=)"
fi

# A thread in accept() against neighbours in mmap/munmap on four cores.
bootn 4 "talaria.test=netshoot" "$DISK"; waitfor "netshoot: ALL OK|netshoot: FAILED|PANIC|\[trap\]" 90
assert "netshoot: ALL OK" "netshoot: a thread waiting for the network against neighbours removing mappings (TLBI broadcast, four cores)"
refute "$BAD|did not acknowledge" "netshoot: no trap, not a single unacknowledged flush"
stopq

echo "== the outgoing network on arm64: DNS + connect (blocking/nonblocking) + ECONNREFUSED + close + the wall clock =="
python3 /w/kernel/tools/minidns.py 0.0.0.0 10.0.0.1 53 >/tmp/dns.log 2>&1 & DNSP=$!
socat -T5 TCP-LISTEN:5555,bind=10.0.0.1,reuseaddr,fork EXEC:cat >/dev/null 2>&1 & ECHOP=$!
ip addr add 10.0.0.9/24 dev tap0 2>/dev/null
python3 -c "
import socket, os, time
l=socket.socket(); l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); l.bind(('10.0.0.9',5557)); l.listen(4)
c,_=l.accept(); c.send(b'GO'); os.system('ip addr del 10.0.0.9/24 dev tap0'); time.sleep(8)" >/dev/null 2>&1 & DEADP=$!
python3 -c "
import socket, time, threading
got = [-1]
l = socket.socket(); l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
l.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096); l.bind(('10.0.0.1', 5558)); l.listen(4)
r = socket.socket(); r.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); r.bind(('10.0.0.1', 5559)); r.listen(4)
def slow():
    c, _ = l.accept(); time.sleep(0.6); n = 0
    try:
        while True:
            d = c.recv(65536)
            if not d: break
            n += len(d)
    except OSError:
        n = -n - 1
    got[0] = n
th = threading.Thread(target=slow, daemon=True); th.start()
r.settimeout(90)
try:
    c, _ = r.accept(); th.join(5); c.send(str(got[0]).encode()); c.close()
except OSError: pass" >/dev/null 2>&1 & SLOWP=$!
sleep 0.5
bootn 1 "talaria.test=connect" "$DISK"; waitfor "connect-test: ALL OK|connect-test: F|PANIC|\[trap\]" 90
assert "the wall clock.s base is the RTC \(pl031\)" "wall-clock: the base comes from the machine's real-time clock (the device tree's PL031)"
assert "wall: 20[2-9][0-9]-.*\(ok\)" "wall-clock: CLOCK_REALTIME gives today's date (>= 2025)"
assert "dns: talaria.test -> 10.0.0.1" "DNS: getaddrinfo through musl's resolver (UDP :53 -> the mini-DNS)"
assert "echo-blocking: 6 PING-B" "connect (blocking) + send/recv"
assert "echo-nonblock: 6 PING-N" "connect (O_NONBLOCK) + ppoll(POLLOUT) + SO_ERROR"
assert "refused: .*\(ok\)" "a connect to a closed port -> ECONNREFUSED"
assert "refused-nb: .*\(ok\)" "a refusal on a NON-BLOCKING connect: SO_ERROR gives +ECONNREFUSED, read gives -ECONNREFUSED"
assert "close-dead: .*\(ok\)" "close() with a dead peer returns at once; the next connection answers"
assert "close-slow: sent 49152, arrived 49152 \(ok\)" "close() with 48 KB unsent to a slow peer: the data arrives in full"
assert "sleep: .*\(ok\)" "nanosleep of 20 ms (the monotonic clock)"
assert "connect-test: ALL OK" "the outgoing network: the summary ALL OK"
refute "$BAD" "connect: no trap, no unknown syscall"
grep -aq "connect-test: ALL OK" $O || { grep -aE "wall:|dns:|echo-|refused|close-|sleep:|connect-test" $O | sed 's/^/  | /'; tail -3 /tmp/dns.log | sed 's/^/  | dns: /'; }
stopq; kill $DNSP $ECHOP $DEADP $SLOWP 2>/dev/null; ip addr del 10.0.0.9/24 dev tap0 2>/dev/null

echo "== virtio-net over virtio-pci: the BARs our own (the device tree's window) and the firmware's (edk2) =="
# The same echo with the card on the PCI bus: with no firmware the kernel assigns the BARs
# the vendor capabilities name out of the host bridge's window; under edk2 it takes the ones
# the firmware assigned. The card is polled (its interrupt would be an MSI-X through the ITS).
NICDEV=virtio-net-pci bootn 1 "talaria.mode=userver talaria.pci=1"; waitfor "userver: the UDP echo" 60
assert "virtio-net: the instance 0 — PCI 00:[0-9a-f]+\.0 1af4:10(00|41) \(the virtio-pci transport" "virtio-pci: the card is found on the PCI bus (no firmware: our BARs)"
R=$(udp PCIPCIPC); [ "$R" = PCIPCIPC ] || { sleep 2; R=$(udp PCIPCIPC); }
[ "$R" = PCIPCIPC ] && ok "virtio-pci: the UDP echo through a card the kernel assigned the BARs of" || bad "virtio-pci: the echo (got '$R')"
refute "$BAD" "virtio-pci: no trap"
stopq
# Behind bridges, with no firmware to number them (docs/ARM64-PLAN.md §10.10): a root port,
# a switch's upstream and downstream ports, the card three buses down. The kernel numbers the
# buses depth first and opens each bridge's window around the BARs it gave the devices behind.
rm -f $O
qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp 1 -m 1024 -display none -serial file:$O \
  -kernel "/w/$IMG" -append "console=ttyAMA0 talaria.mode=userver talaria.pci=1 talaria.smp=1" \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no \
  -device pcie-root-port,id=rp0,bus=pcie.0,chassis=1 -device x3130-upstream,id=up,bus=rp0 \
  -device xio3130-downstream,id=dn,bus=up,chassis=3 \
  -device virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56,bus=dn >/tmp/q.log 2>&1 &
Q=$!; waitfor "userver: the UDP echo" 60
assert "pci: the bridge 00:01\.0 numbered \(no firmware\): buses 1\.\.3" "pci-bridges: the root port numbered (no firmware), its subordinate covers the switch below"
assert "virtio-net: the instance 0 — PCI 03:00\.0 " "pci-bridges: the card is found three buses down"
R=$(udp BRIDGEBR); [ "$R" = BRIDGEBR ] || { sleep 2; R=$(udp BRIDGEBR); }
[ "$R" = BRIDGEBR ] && ok "pci-bridges: the UDP echo through the bridges' windows the kernel opened" || bad "pci-bridges: the echo (got '$R')"
refute "$BAD" "pci-bridges: no trap"
stopq

EFI=kernel/uefi-stub/target/aarch64-unknown-uefi/release/talaria-uefi-stub.efi
FW=/usr/share/edk2/aarch64/QEMU_EFI.silent.fd
if [ -f "/w/$EFI" ] && [ -f "$FW" ] && [ "/w/$EFI" -nt "/w/${IMG%.img}" ]; then
  NICDEV=virtio-net-pci KERNEL="/w/$EFI" FWOPT="-bios $FW" bootn 1 "talaria.mode=userver talaria.pci=1"; waitfor "userver: the UDP echo" 200
  assert "pci: ECAM 0x[0-9a-f]+ \(MCFG\)" "uefi-virtio-pci: the bus from the ACPI MCFG"
  assert "virtio-net: the instance 0 — PCI 00:[0-9a-f]+\.0 1af4:10(00|41) \(the virtio-pci transport" "uefi-virtio-pci: the card behind the firmware path (its BARs)"
  R=$(udp UEFIUEFI); [ "$R" = UEFIUEFI ] || { sleep 2; R=$(udp UEFIUEFI); }
  [ "$R" = UEFIUEFI ] && ok "uefi-virtio-pci: the UDP echo behind edk2 (the gate's UEFI path has a NIC)" || bad "uefi-virtio-pci: the echo (got '$R')"
  refute "$BAD" "uefi-virtio-pci: no trap"
  stopq
else
  echo "== virtio-pci under edk2: SKIPPED — no fresh arm64 stub or no edk2 =="
fi

echo "== the zero-syscall cell on arm64: fastflood and fastapp (the rings between core 1 and ring 3) =="
# The datapath cell on core 1 bridges the card and two SPSC rings in shared memory; ring 3
# polls the rings with no syscall. fastflood fills the TX ring 100 000 times; fastapp
# reflects UDP and the cell counts the syscalls and faults per packet (zero).
bootn 2 "talaria.mode=fastflood talaria.fast=1"; waitfor "cell-diag: tx=100000" 200
assert "talaria: the zero-syscall cell on core 1" "fast: the cell runs on core 1 (not the bring-up's idle loop)"
assert "fastflood: 100000 frames" "fastflood: 100k frames through the cell's TX ring"
assert "cell-diag: tx=100000 [^k]*kicks=[0-9]{1,4} " "fastflood: the kick is amortised by the batch (<10% of the frames)"
refute "$BAD" "fastflood: no trap"
stopq
bootn 2 "talaria.mode=fast talaria.fast=1"; waitfor "fastapp: the zero-syscall echo is up" 120; sleep 1
R=$(printf FASTFAST | timeout 4 socat -T3 - udp:10.0.0.2:7777 2>/dev/null); [ "$R" = FASTFAST ] || { sleep 2; R=$(printf FASTFAST | timeout 4 socat -T3 - udp:10.0.0.2:7777 2>/dev/null); }
[ "$R" = FASTFAST ] && ok "fastapp: the UDP echo through the rings, ring 3 polling" || bad "fastapp: the echo (got '$R')"
waitfor "hotpath enforce: packets=1 " 10
assert "hotpath enforce: packets=1 Δsyscalls=0 Δfaults=0 -> OK zero-syscall" "fastapp: zero syscalls and zero faults per packet on the hot path"
refute "$BAD" "fastapp: no trap"
stopq

echo "NET TOTAL: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = 0 ]
