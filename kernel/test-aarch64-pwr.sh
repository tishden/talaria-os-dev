#!/bin/sh
# THE ARM64 POWER BUTTON through edk2 — the ACPI GED (`acpi::parse_ged`), what an EC2 Graviton's
# stop sends (ARM64-PLAN §11 item 1): `pwrbtn` (the process leaves on the SIGTERM) and `pwrbtnign`
# (it ignores it, the deadline's sync), each followed by `pwrbtnv` on the same disk — the file
# written without an fsync is there. Runs inside talaria-qemu-aarch64 (the QMP socket is local);
# called by kernel/test-aarch64.sh's UEFI block, the logs into /w/$TD (default kernel/target/a64-disks).
TD=${TD:-kernel/target/a64-disks}
EFI=/w/kernel/uefi-stub/target/aarch64-unknown-uefi/release/talaria-uefi-stub.efi
FW=/usr/share/edk2/aarch64/QEMU_EFI.silent.fd
boot() { # MODE LOG
  qemu-system-aarch64 -M virt,gic-version=3,its=on -cpu max -smp 2 -m 1024 -nographic -bios $FW \
    -kernel $EFI -append "console=ttyAMA0 talaria.test=$1 talaria.smp=2 talaria.pwrbtn=3 talaria.poweroff=1 talaria.pci=1" \
    -drive id=d0,file=/tmp/pwr.img,format=raw,if=none -device nvme,drive=d0,serial=tal001 \
    -qmp unix:/tmp/qmp.sock,server,nowait -no-reboot > "$2" 2>&1 &
  Q=$!
}
press() { python3 - <<'PY'
import socket,time
s=socket.socket(socket.AF_UNIX); s.connect('/tmp/qmp.sock'); s.settimeout(3); s.recv(4096)
s.send(b'{"execute":"qmp_capabilities"}'); time.sleep(0.2); s.recv(4096)
s.send(b'{"execute":"system_powerdown"}'); time.sleep(0.2); s.recv(4096)
PY
}
for m in ${MODES:-pwrbtn pwrbtnign}; do
  cp /w/kernel/target/a64-disks/ext2.img /tmp/pwr.img; rm -f /tmp/qmp.sock
  boot $m /w/$TD/pwr-$m.log
  t=0; until grep -aq "pid=1 \(waiting for\|ignoring\) SIGTERM" /w/$TD/pwr-$m.log || [ $t -gt 240 ]; do sleep 1; t=$((t+1)); done
  press
  t=0; while kill -0 $Q 2>/dev/null && [ $t -lt 60 ]; do sleep 1; t=$((t+1)); done
  if kill -0 $Q 2>/dev/null; then echo "$m: QEMU STILL ALIVE 60 s after the press"; kill $Q; wait $Q 2>/dev/null; else echo "$m: QEMU went out ${t}s after the press"; fi
  grep -a "power button\|GED\|SIGTERM\|synchronised\|did not leave\|power-off" /w/$TD/pwr-$m.log | cut -c1-170 | sed 's/^/   /'
  rm -f /tmp/qmp.sock; boot pwrbtnv /w/$TD/pwr-$m-v.log
  t=0; until grep -aq "pwrbtn: PERSISTED OK\|pwrbtn: FAIL" /w/$TD/pwr-$m-v.log || [ $t -gt 240 ]; do sleep 1; t=$((t+1)); done
  kill $Q 2>/dev/null; wait $Q 2>/dev/null
  grep -a "pwrbtn: PERSISTED\|pwrbtn: FAIL\|a JBD2 journal" /w/$TD/pwr-$m-v.log | cut -c1-150 | sed 's/^/   /'
done
