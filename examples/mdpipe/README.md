# mdpipe — a teaching multithreaded workload

An "ingest → worker pool → write to disk" pipeline in C++17 that measures the
stage latency by percentiles. The hands-on guide `docs/HANDS-ON-GUIDE.md` is
built on it: monitoring, core placement, profiling, live debugging, a crash and
a core dump, file transfer, running on EC2.

```sh
make                                        # the binary + disk.img (ext2 with a journal)
make check                                  # Talaria ABI compatibility
./qrun.sh "talaria.mode=cell talaria.smp=4" 60 4
make crash && IMG=disk-crash.img ./qrun.sh "talaria.mode=cell talaria.smp=4 talaria.core=/core.mdpipe" 40 4
IMG=disk-crash.img ./qfiled.sh "pull /core.mdpipe out/core.mdpipe" && gdb mdpipe out/core.mdpipe
```

| File | Purpose |
|---|---|
| `mdpipe.cpp` | the workload; arguments `--secs --rate --workers --rounds --out --crash-at` |
| `Makefile` | `all`, `check`, `run`, `crash`, `prof`, `clean` |
| `qrun.sh` | QEMU `microvm` in docker; serial carries the kernel log and the operator console |
| `qgdb.sh` | QEMU `-M pc` for live debugging: COM2 on `127.0.0.1:1234`, an NVMe disk |
| `qfiled.sh` | the node in `filed` mode and `talaria-file` commands from the host `10.0.0.1` |

Needs: a built kernel (`kernel/`), the docker images `talaria-qemu2` and
`talaria-alpine-build`, `/dev/kvm`, `e2fsprogs`.
