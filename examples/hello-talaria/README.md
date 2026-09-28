# hello-talaria — your own application on Talaria OS in five minutes

The reference path "source -> binary -> disk -> run" without a single kernel
change. The full guide: `docs/USER-GUIDE.md` §6.

## Steps

```sh
# 0) once: build the kernel (kernel/BUILD.md) and the docker toolchain images
# 1) build the binary and the disk (musl-gcc in docker + talaria-pack)
make
# 2) run it (QEMU/KVM in docker, the serial console goes to your terminal)
make run
# the C++ variant (STL, exceptions, std::thread):
make APP=hellopp run
```

Expected output (the kernel's own lines are still in Russian until the
translation of kernel messages lands):

```
talaria: a workload by the manifest /cell.manifest (restart=1, argc=2, envc=0)
hello-talaria: hello from ring 3! argc=2 argv[1]=from-manifest
hello-talaria: from disk: Hello from the Talaria OS disk!
hello-talaria: threads joined, counter=200000 (expected 200000)
hello-talaria: OK
supervise: the workload finished normally (code 0)
```

## What it shows

- **Build:** a static musl binary linked at `0x10000000`
  (`-Wl,-Ttext-segment=0x10000000` is required, see USER-GUIDE §6.1).
- **Packing:** `talaria-pack -o disk.img --app hello --args="..." greeting.txt` —
  one tool builds the image (TFS1/TFS2/ext2) and generates `/cell.manifest`.
- **Running:** `talaria.mode=cell` — the kernel reads the manifest and runs the
  workload under its supervisor (failure isolation, restart with `restart=`,
  watchdog).
- **Runtime:** stdio, arguments, files from disk, pthreads (clone/futex — the
  futex sleeps in the kernel with `mwait`, see DEVELOPMENT-GUIDE §6b).

## Checking your own binary

```sh
kernel/tools/talaria-check ./mybinary --secs 15
# -> the syscalls it made + a verdict: compatible / gaps (which syscalls) / crash
```

Next: networking (`--net`, tap/virtio — USER-GUIDE §7.3), a writable file system
(`talaria-pack --fs ext2`), the process model with fork and dynamic ELF
(`talaria.mode=proc`), pinning to a core (`--core N` + `talaria.smp=N`).
