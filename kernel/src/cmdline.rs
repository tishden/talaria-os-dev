//! Parsing the kernel command line: BY TOKENS rather than by substring search.
//!
//! The kernel is configured by one string (QEMU's `-append`, multiboot2's `options`,
//! user-data on EC2), and the whole mode selection is an answer to the question "does it contain
//! `talaria.<key>=<value>`". Historically the answer came from `cmdline.contains(..)`,
//! and that is wrong exactly where one name is a prefix of another:
//!
//! ```text
//! cmdline = "talaria.test=fsbig"
//! cmdline.contains("talaria.test=fs")     -> true   // SOMEONE ELSE'S mode fires
//! cmdline.contains("talaria.test=fsbig")  -> true   // and ours too
//! ```
//!
//! This is not theory: with `talaria.test=fsbig` the `talaria.test=fs` block ran too
//! (it opened `/hello.txt`, which is not on that volume, and printed `fd=-2` into the run's
//! log). The class touched 11 names out of 106 — `fs`/`fsbench`/`fsbig`/`fsbigrace`/
//! `fsinfo`, `cpp`/`cpp2`/`cpp3`/`cppdtor`, `mem`/`memrace`, `exec`/`execmt`,
//! `ext2`/`ext2v`, `fork`/`forkp`, `sync`/`syncbench`, `dyn`/`dynsync`. Two of
//! them got manual patches of the form `&& !cmdline.contains("...")`, and eight
//! silently depended on whether the previous block parks.
//!
//! Here the key is compared in full, the value in full, and the tokens are split on
//! spaces exactly as the rest of the kernel splits them. There is no new behaviour
//! — there is an ambiguity removed.

/// A key's value: `key=<value>` as a separate token. `None` if the key is absent.
///
/// The key is accepted both WITHOUT `=` and WITH a trailing `=`. That is not a convenience but a closed
/// trap: the two forms look equally correct, while the second ALWAYS
/// returned `None` — `strip_prefix("talaria.wakebal=")` ate the equals sign,
/// and the following `strip_prefix('=')` no longer found it. The knob did not
/// fail and did not complain: it simply did nothing.
///
/// The price of that trap is known by name. `talaria.wakebal=` was created for
/// a head-to-head measurement of the placement rule on wakeup — and a whole sweep
/// "variant 1 against 2 against 3" compared THREE COPIES OF THE SAME code.
/// The numbers agreed to a tenth (110.0 / 110.0 / 111.0), and that looked like
/// an honest conclusion "the policy has nothing to do with it", while it was a report about a knob that did not work.
/// A knob that stays silent instead of refusing is more expensive than a missing one.
///
/// On a duplicate the first occurrence wins — just as with the `strip_prefix` in
/// the loop this function replaced.
pub fn value<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    let key = match key.strip_suffix('=') { Some(k) => k, None => key };
    cmdline
        .split_whitespace()
        .find_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// `key=val` is present EXACTLY — rather than as a prefix of a longer value.
pub fn has(cmdline: &str, key: &str, val: &str) -> bool {
    value(cmdline, key) == Some(val)
}

/// The key is present in any form (`key`, `key=`, `key=anything`).
pub fn present(cmdline: &str, key: &str) -> bool {
    cmdline
        .split_whitespace()
        .any(|t| t == key || t.strip_prefix(key).is_some_and(|r| r.starts_with('=')))
}

/// A boolean knob: `key=1`. Everything else (including `key=0`) is off.
pub fn flag(cmdline: &str, key: &str) -> bool {
    has(cmdline, key, "1")
}

/// An integer from `key=<N>` (decimal; with an `0x` prefix, hexadecimal).
pub fn num(cmdline: &str, key: &str) -> Option<u64> {
    let v = value(cmdline, key)?;
    match v.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => v.parse::<u64>().ok(),
    }
}

/// `talaria.test=<name>` — a self-check mode.
pub fn test_is(cmdline: &str, name: &str) -> bool {
    has(cmdline, "talaria.test", name)
}

/// `talaria.mode=<name>` — a production mode (a workload, the datapath, the shell).
pub fn mode_is(cmdline: &str, name: &str) -> bool {
    has(cmdline, "talaria.mode", name)
}

/// The self-check mode's name, if one is set.
pub fn test_name(cmdline: &str) -> Option<&str> {
    value(cmdline, "talaria.test")
}

#[cfg(not(talaria_host))]
/// A self-test of the parsing (the gate `talaria.test=cmdline`): it checks exactly what
/// the module was created for — that a prefix of someone else's name is NOT counted as a match.
pub fn selftest() -> bool {
    let mut ok = true;
    let mut check = |cond: bool, what: &str| {
        if !cond {
            crate::println!("cmdline-test: FAILED — {}", what);
            ok = false;
        }
    };

    // What it is all for: an exact value against a prefix.
    let c = "console=ttyS0 talaria.test=fsbig talaria.smp=4";
    check(test_is(c, "fsbig"), "fsbig was not recognised");
    check(!test_is(c, "fs"), "fs matched the prefix of fsbig");
    check(!test_is(c, "fsbigrace"), "fsbigrace matched the shorter fsbig");
    check(!test_is(c, "big"), "big matched the tail of the value");

    // The same class on the production modes and on the historical patches.
    check(mode_is("talaria.mode=procdemo", "procdemo"), "procdemo was not recognised");
    check(!mode_is("talaria.mode=procdemo", "proc"), "proc matched the prefix of procdemo");
    check(!test_is("talaria.test=dynsync", "dyn"), "dyn matched the prefix of dynsync");
    check(!test_is("talaria.test=syncbench", "sync"), "sync matched the prefix of syncbench");
    check(!test_is("talaria.test=cppdtor", "cpp"), "cpp matched the prefix of cppdtor");

    // The key is taken in full too: `talaria.smp` must not match `talaria.smpx`,
    // nor `test` match a foreign key that merely ends in `test`.
    check(value("talaria.smpx=4", "talaria.smp").is_none(), "the key matched by prefix");
    check(value("xtalaria.test=fs", "talaria.test").is_none(), "the key matched by the token's tail");

    // The values: numbers, flags, absence.
    check(num("talaria.smp=4", "talaria.smp") == Some(4), "the number was not parsed");
    check(num("talaria.trace=0x1000", "talaria.trace") == Some(0x1000), "hex was not parsed");
    check(num("talaria.smp=x", "talaria.smp").is_none(), "garbage was taken for a number");
    check(flag("talaria.poweroff=1", "talaria.poweroff"), "flag=1 was not raised");
    check(!flag("talaria.poweroff=0", "talaria.poweroff"), "flag=0 was raised");
    check(!flag("", "talaria.poweroff"), "a flag was raised on an empty cmdline");
    check(present("talaria.mode=cell", "talaria.mode"), "the key was not found");
    check(present("a talaria.pci b", "talaria.pci"), "a key without a value was not found");
    check(!present("talaria.modex=cell", "talaria.mode"), "the key was found by prefix");
    check(test_name("q talaria.test=htree w") == Some("htree"), "the mode's name was not extracted");
    check(test_name("q w").is_none(), "a mode's name was extracted from nothing");
    // A key in both forms — `key` and `key=` — must give the same value:
    // `talaria.wakebal=` (with `=`) NEVER worked, and a whole sweep
    // compared three copies of one kernel.
    check(num("talaria.wakebal=3", "talaria.wakebal=") == Some(3), "a key with `=` was not parsed");
    check(num("talaria.wakebal=3", "talaria.wakebal") == num("talaria.wakebal=3", "talaria.wakebal="), "the key's forms disagree");

    // EVERY `talaria.net*` knob must be accounted for: live (`NET_LIVE_OPTS`)
    // or NIC-opening (`NET_OPEN_OPTS`). An unaccounted one falls under
    // the prefix in `tail_touches_nic`, and the very first A/B through user-data will reopen
    // the NIC on a live node (`talaria.netscan=` -> `DEV_STS=0x21`,
    // forty minutes of stand time).
    for k in KNOBS {
        if !k.key.starts_with("talaria.net") { continue; }
        let listed = crate::NET_LIVE_OPTS.iter().chain(crate::NET_OPEN_OPTS.iter())
            .any(|p| { let p: &str = p; p.strip_suffix('=').unwrap_or(p) == k.key });
        check(listed, "a talaria.net* knob is not accounted for in NET_LIVE_OPTS/NET_OPEN_OPTS (main.rs) — it will reopen the NIC");
    }

    // The knob registry and the audit.
    if !audit_selftest() { ok = false; }

    crate::println!("cmdline-test: {}", if ok { "ALL OK" } else { "THERE ARE FAILURES" });
    ok
}

// ── The knob registry and the command line audit ─────────────────────────────
//
// Every `talaria.*` knob the kernel reads at all is listed HERE, with
// the kind of its value. The audit walks all the `talaria.*` tokens of the command line,
// prints each one's parsed value and REFUSES TO BOOT if it met a
// key that is not in the registry, a value of the wrong kind, or a single-valued key
// given twice. A typo in a knob must be a refusal rather than silence.
//
// Why so strict. Within one session two defects of the same kind: `talaria.wakebal=`
// never worked (the key was passed with `=`), and `talaria.smp=4` from user-data
// lost to the `talaria.smp=2` baked into the image — and both times the system silently
// executed NOT WHAT it had been told, while the measured numbers looked convincing. A knob
// about which one cannot say "it was applied" is more expensive than a missing one.
//
// The registry is not documentation but a gate: a new knob without a line here will not boot
// in a single test, and that is right.

/// The kind of a knob's value: how to check and print it.
#[derive(Clone, Copy)]
pub enum Kind {
    /// `1` or `0`.
    Flag,
    /// An integer: decimal or `0x…`.
    Num,
    /// One of the listed words.
    Word(&'static [&'static str]),
    /// An arbitrary string (a path, an address, a list of rules); printed as is.
    Text,
    /// Checked by a function (composite values of the form `a=b`, `N,eager`).
    Check(fn(&str) -> bool),
}

/// Whether the key may occur in the string SEVERAL times (the rules accumulate).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Rep { Once, Many }

pub struct Knob {
    pub key: &'static str,
    pub kind: Kind,
    pub rep: Rep,
    pub what: &'static str,
}

const fn once(key: &'static str, kind: Kind, what: &'static str) -> Knob { Knob { key, kind, rep: Rep::Once, what } }
const fn many(key: &'static str, kind: Kind, what: &'static str) -> Knob { Knob { key, kind, rep: Rep::Many, what } }

fn is_num(v: &str) -> bool {
    match v.strip_prefix("0x") {
        Some(h) => !h.is_empty() && u64::from_str_radix(h, 16).is_ok(),
        None => !v.is_empty() && v.parse::<u64>().is_ok(),
    }
}
/// `talaria.ena=<word>` or `txbench=N`/`rxbench=N` (see parse_early_opts).
fn ena_ok(v: &str) -> bool {
    const W: &[&str] = &["llq", "nollq", "accel", "noaccel", "phc", "rss", "rsstbl", "rsscq", "rsshash",
        "hibuf", "unmask", "mtu9001", "nowc", "noreadless", "meta", "nocqdb", "intr"];
    W.contains(&v)
        || v.strip_prefix("txbench=").is_some_and(is_num)
        || v.strip_prefix("rxbench=").is_some_and(is_num)
}
/// `talaria.ext2=nojournal|journal=data|dxcheck|dxlimit=N`.
fn ext2_ok(v: &str) -> bool {
    matches!(v, "nojournal" | "journal=data" | "dxcheck") || v.strip_prefix("dxlimit=").is_some_and(is_num)
}
/// `talaria.trace=<LBA>[,eager]`.
fn trace_ok(v: &str) -> bool {
    match v.split_once(',') { Some((n, rest)) => is_num(n) && rest == "eager", None => is_num(v) }
}
/// `talaria.top=<ms>[,rss]`.
fn top_ok(v: &str) -> bool {
    match v.split_once(',') { Some((n, rest)) => is_num(n) && rest == "rss", None => is_num(v) }
}
/// `talaria.console=<channel>[,nopoll]` — the channel `vga`/`serial`/`poll`, the flag `nopoll`.
fn console_ok(v: &str) -> bool {
    !v.is_empty() && v.split(',').all(|t| matches!(t, "vga" | "serial" | "poll" | "nopoll"))
}
/// `talaria.netirq=1|all|<GSI>|<a>-<b>`.
fn netirq_ok(v: &str) -> bool {
    v == "all" || is_num(v) || v.split_once('-').is_some_and(|(a, b)| is_num(a) && is_num(b))
}
/// `talaria.ip=dhcp|<a.b.c.d>[/<prefix>]`.
fn ip_ok(v: &str) -> bool {
    if v == "dhcp" { return true; }
    let (a, p) = match v.split_once('/') { Some((a, p)) => (a, Some(p)), None => (v, None) };
    a.split('.').filter(|o| o.parse::<u8>().is_ok()).count() == 4 && a.split('.').count() == 4
        && p.is_none_or(|p| p.parse::<u8>().is_ok_and(|n| n <= 32))
}
/// `talaria.ntp=aws|<a.b.c.d>[,<poll s>]` (`ntp::parse_knob`).
fn ntp_ok(v: &str) -> bool { crate::ntp::parse_knob(v).is_some() }
fn ipaddr_ok(v: &str) -> bool { v != "dhcp" && ip_ok(v) }
/// `talaria.prof=1|<hz>`.
fn prof_ok(v: &str) -> bool { is_num(v) }
/// `talaria.core=<path>[,full]`.
fn core_ok(v: &str) -> bool {
    let p = v.split_once(',').map_or(v, |(p, _)| p);
    p.starts_with('/') && v.split_once(',').is_none_or(|(_, f)| f == "full")
}

/// The production modes `talaria.mode=` (runmode.rs). A mode outside the list is a boot refusal,
/// as is an unknown knob.
pub const MODES: &[&str] = &[
    "busybox", "cell", "console", "diskserver", "eserver", "fast", "fastflood", "filed", "forksrv",
    "gdb", "httpd", "lua", "mongoose", "mtsrv", "proc", "procdemo", "quickjs",
    "redis", "server", "smoltcp", "sqlite", "supervise", "userver",
];

/// All the kernel's knobs. The order is alphabetical, so that a duplicate is visible to the eye.
pub const KNOBS: &[Knob] = &[
    once("talaria.acpidump", Kind::Flag, "print the ACPI tables' power-button devices (the GED, a PL061) as hex at boot"),
    once("talaria.apcr4mask", Kind::Num, "the CR4 bits that are NOT cloned onto APs (bisecting)"),
    once("talaria.appprof", Kind::Flag, "the app stage per thread (read->write) into the stat block"),
    once("talaria.cell", Kind::Word(&["identity"]), "a mode=cell workload in the kernel's identity AS"),
    once("talaria.clocksync", Kind::Num, "the clock discipline's period, ms (0 — off)"),
    once("talaria.cols", Kind::Num, "the operator terminal's width"),
    once("talaria.console", Kind::Check(console_ok), "the output channel: vga/serial/poll, the flag nopoll"),
    once("talaria.core", Kind::Check(core_ok), "ELF core dumps: path[,full]"),
    once("talaria.ctltoken", Kind::Text, "the control plane's token (/ctl)"),
    once("talaria.dirtyage", Kind::Num, "a pre-emptive flush of data older than N ms (0 — off, as before)"),
    once("talaria.dirtybatch", Kind::Num, "the ceiling of ONE age-flush kick, blocks (0 — auto: an eighth of the portion)"),
    once("talaria.dirtychunk", Kind::Num, "blocks per portion of the pre-emptive dirty-data flush (default 64)"),
    once("talaria.dirtyfrac", Kind::Num, "a pre-emptive flush when the dirty set exceeds 1/N of the cache (a larger N — earlier; default 4)"),
    once("talaria.dirtykick", Kind::Num, "the age-flush kick's period, ms (0 — auto: an eighth of talaria.dirtyage)"),
    once("talaria.dmap", Kind::Word(&["0", "1", "strict"]), "strict (the default) — the direct map, and no identity leaf over the map's RAM in a process space (ARM64-PLAN §10.16 stage 4b); 1 — the map, the identity kept in process spaces (the A/B); 0 — no direct map: the kernel reaches RAM by its identity (the fallback)"),
    once("talaria.dns", Kind::Check(ipaddr_ok), "the DNS address"),
    many("talaria.ena", Kind::Check(ena_ok), "the ENA driver's options"),
    once("talaria.epoch", Kind::Num, "CLOCK_REALTIME at startup, UNIX seconds"),
    once("talaria.evictbatch", Kind::Num, "blocks per eviction of a dirty slot — consecutive ones leave in one request (default 1 — one at a time)"),
    many("talaria.ext2", Kind::Check(ext2_ok), "the ext2 modes: nojournal/journal=data/dxcheck/dxlimit=N"),
    once("talaria.fast", Kind::Flag, "the zero-syscall datapath without a stack"),
    once("talaria.faultaudit", Kind::Flag, "1 — at a ring-3 fault, audit every live space: private frames against the ledger and the refcount, table frames against the leaves (ARM64-PLAN §11 item 0; nothing runs before the fault)"),
    once("talaria.filed", Kind::Num, "the file upload port (filed)"),
    once("talaria.filedtoken", Kind::Text, "the filed token"),
    once("talaria.forkconns", Kind::Num, "forksrv: how many connections to serve before exiting"),
    once("talaria.forkreap", Kind::Flag, "forksrv: reap the children as a postmaster does"),
    once("talaria.freetrace", Kind::Word(&["0", "1", "2"]), "1 — remember the last 8192 frame frees with their callers and the last COW copies; a ring-3 fault says whether its stack's frame (or the frame it was copied from) was freed and by whom; 2 — and quarantine dropped DMA bounces with a canary (ARM64-PLAN §11 item 0)"),
    once("talaria.fscache", Kind::Num, "the FS block cache, MB (0 — by memory)"),
    once("talaria.fscnt", Kind::Num, "the dirty set by a counter (1, the default) or by a walk over the whole cache (0 — as before)"),
    once("talaria.fsdevlock", Kind::Flag, "the FS lock while waiting for the disk (0 — a rollback)"),
    once("talaria.fstrace", Kind::Num, "print FS lock holds longer than N ms (an instrument)"),
    once("talaria.fua", Kind::Num, "a durable write: 1 — always FUA, 0 — never (the default follows the device's VWC)"),
    once("talaria.futexhang", Kind::Num, "print \"HUNG\" after N s of waiting on a futex"),
    once("talaria.gdb", Kind::Word(&["break"]), "stop in the debugger before the workload's first instruction (the gdb stub on COM2)"),
    once("talaria.gw", Kind::Check(ipaddr_ok), "the gateway"),
    once("talaria.heartbeat", Kind::Num, "the kernel's pulse every N ticks"),
    once("talaria.huge", Kind::Num, "the large-page threshold, MB (0 — off)"),
    once("talaria.idlepoll", Kind::Num, "the stack-pass window on a core without work, ns (0 — sleep)"),
    once("talaria.idleslice", Kind::Num, "the tick period with nobody ready, µs (0 — the common one)"),
    once("talaria.imds", Kind::Flag, "query IMDS at startup"),
    once("talaria.iowin", Kind::Num, "the interrupt window while waiting for the device: 0 spin / 1 the tick arrives / 2 the tick may preempt (default 2)"),
    once("talaria.ip", Kind::Check(ip_ok), "the address: dhcp or a.b.c.d/prefix"),
    once("talaria.irr", Kind::Num, "print the IRR on the first N syscalls"),
    once("talaria.jrnfree", Kind::Num, "the journal is free during the data flush (1, the default) or busy, as before (0)"),
    once("talaria.jrnwait", Kind::Num, "the threshold for the \"the journal is busy with a neighbour\" diagnosis, ms (default 50; 0 — silent)"),
    once("talaria.kecho", Kind::Num, "a UDP echo in the kernel, the port"),
    once("talaria.kq", Kind::Flag, "the kernel stacks' quarantine: a freed stack is poisoned and checked before reuse"),
    once("talaria.ksink", Kind::Num, "a UDP sink in the kernel, the port"),
    once("talaria.kts", Kind::Flag, "receive timestamps in the kernel"),
    once("talaria.kwin", Kind::Num, "0 — no kernel stacks' window: stacks are identity frames guarded per space (ARM64-PLAN §10.16 stage 3; the A/B and the fallback)"),
    once("talaria.lazytlb", Kind::Num, "0 — ring a neighbour in hlt on a TLB flush (the default 1: it flushes by itself on wakeup)"),
    once("talaria.lowmem", Kind::Num, "cap the allocator below 4 GB, MB (a gate: the shape of a machine whose RAM is mostly above)"),
    once("talaria.manifest", Kind::Text, "the path of the workload's manifest"),
    once("talaria.mode", Kind::Word(MODES), "the production mode"),
    once("talaria.mwin", Kind::Num, "0 — no devices' window: the kernel reaches device registers by their identity (ARM64-PLAN §10.16 stage 5; arm64's GB0 alias stays; the A/B and the fallback)"),
    many("talaria.net", Kind::Text, "an address rule by interface name"),
    once("talaria.netidle", Kind::Flag, "sleeping on the receive ring"),
    once("talaria.netirq", Kind::Check(netirq_ok), "the NIC's line: 0 off / 1 / all / GSI / a-b (the default is polling)"),
    once("talaria.netmix", Kind::Num, "a waiter with a non-socket in its set parks too, with this ceiling, ns (0 — it spins)"),
    once("talaria.netpark", Kind::Num, "the ceiling on parking a waiter, ns (0 — polling)"),
    once("talaria.netpause", Kind::Num, "pauses between stack passes for a network waiter (default 32)"),
    once("talaria.netprof", Kind::Num, "a profile of the datapath's stages, a summary every N turns"),
    once("talaria.netscan", Kind::Num, "the back stop for scanning the set under the lock, ns (0 — as before)"),
    once("talaria.netslice", Kind::Num, "the tick period on a core with a network waiter, µs"),
    once("talaria.netsolo", Kind::Flag, "one stack pass per machine"),
    once("talaria.netyield", Kind::Num, "a network waiter yields the core after N µs (0 — it does not)"),
    once("talaria.nic", Kind::Text, "the requested NIC driver"),
    once("talaria.nobalance", Kind::Flag, "switch the balancer off"),
    once("talaria.asid", Kind::Flag, "arm64: 0 — every space on ASID 0, a full TLB invalidation at every switch (the behaviour before ASIDs; the A/B)"),
    once("talaria.noshoot", Kind::Flag, "switch TLB shootdowns to neighbours off (diagnostics)"),
    once("talaria.nosteal", Kind::Flag, "switch work stealing off"),
    once("talaria.nowakebal", Kind::Flag, "switch placement on wakeup off"),
    once("talaria.ntp", Kind::Check(ntp_ok), "the wall clock's reference: SNTP to aws (169.254.169.123) or a.b.c.d, optionally ,<poll s> (default 16)"),
    once("talaria.pci", Kind::Flag, "bring PCI up (ECAM/ivshmem)"),
    many("talaria.pin", Kind::Text, "a thread placement rule"),
    once("talaria.pmuchain", Kind::Flag, "arm64: use a declared CHAIN for 64-bit counter pairs (off: a Graviton VM declares it and does not carry)"),
    once("talaria.pmunarrow", Kind::Flag, "arm64: the PMU's counters at 32 bits, widened by the reader (a test of the no-CHAIN path)"),
    once("talaria.pmutick", Kind::Num, "taking the PMU counters in the tick, ms (0 — off)"),
    once("talaria.pollgap", Kind::Num, "an idle stack pass no more often than every N ns"),
    once("talaria.pollhard", Kind::Num, "a hard polling ceiling, ns"),
    many("talaria.pool", Kind::Text, "a named core pool"),
    once("talaria.poweroff", Kind::Flag, "power the machine off once the mode has finished"),
    once("talaria.prof", Kind::Check(prof_ok), "the sampling profile: 1 or samples/s"),
    once("talaria.ptlog", Kind::Flag, "the page tables' journal: every entry write into a ring, dumped for a crashed thread's page"),
    once("talaria.pveoi", Kind::Flag, "KVM PV EOI (0 — off)"),
    once("talaria.pvipi", Kind::Flag, "KVM PV IPI (0 — off)"),
    once("talaria.pwrbtn", Kind::Num, "the power button: seconds for the workload to exit after a SIGTERM (0 — do not catch it)"),
    once("talaria.readahead", Kind::Num, "the read-ahead window, blocks (0 — off)"),
    once("talaria.rebootat", Kind::Word(&["mb2", "idt", "mem", "paging", "lapic"]), "a reboot at a bring-up point (metal)"),
    once("talaria.reqfirst", Kind::Num, "a thread with a request in progress does not yield the core to a network waiter, ns (0 — off)"),
    once("talaria.ringhonest", Kind::Num, "the \"the NIC is silent\" criterion: 1 honest (the phase bit), 0 the old one"),
    many("talaria.route", Kind::Text, "a route rule"),
    once("talaria.rows", Kind::Num, "the operator terminal's height"),
    once("talaria.runhist", Kind::Flag, "a histogram of ready tasks at the tick"),
    many("talaria.sched", Kind::Text, "the scheduler's flags (spread/balance/…)"),
    // THE GATE `shmrace` NEEDS THE RACE TO HAPPEN. The section that creates a shm object is
    // microseconds long, so four threads released from a barrier miss each other and the gate
    // proves only that shm works. `shmdelay=<us>` widens the window INSIDE the section (the
    // same trick as `ext2=dxlimit=N`, which squeezes an index node so that the split path can
    // be reached on a small directory); `shmlock=0` takes the lock away, and then the same
    // programme MUST fail — the A/B that proves the gate can fail at all.
    once("talaria.sharedid", Kind::Flag, "an anonymous MAP_SHARED in a process at its frames' identity address again (before stage 4a of the direct map; the A/B and the fallback)"),
    once("talaria.shmdelay", Kind::Num, "us of delay inside the shm creation section (the gate shmrace)"),
    once("talaria.shmlock", Kind::Num, "the lock of the shm tables: 0 — off (an A/B for the gate shmrace)"),
    once("talaria.shmrole", Kind::Word(&["ping", "pong"]), "the role in the ivshmem test"),
    once("talaria.sigtrace", Kind::Num, "a signal trace, the first N"),
    once("talaria.slice", Kind::Num, "the scheduler's slice, ns"),
    once("talaria.smp", Kind::Num, "the number of cores"),
    once("talaria.smtpack", Kind::Flag, "occupy the hyperthreads of one physical core rather than different cores (the host is left a whole one)"),
    once("talaria.stat", Kind::Num, "the stat block into the log every N ms"),
    once("talaria.statidle", Kind::Flag, "the stat block onto the line only when the network is quiet"),
    once("talaria.stealbal", Kind::Num, "the stealing balance rule (1 — as before)"),
    once("talaria.stealpush", Kind::Flag, "a push: a donor on a tick gives a spare ready task to an empty neighbour"),
    once("talaria.stealwait", Kind::Num, "the starvation threshold for stealbal=5, µs"),
    once("talaria.stop", Kind::Num, "a SIGTERM to the workload after N s"),
    once("talaria.strace", Kind::Num, "a trace of the first N syscalls"),
    once("talaria.stuck", Kind::Num, "the \"stuck\" threshold, ticks"),
    once("talaria.suite", Kind::Num, "the LBA of the mode list (a suite per image)"),
    once("talaria.switches", Kind::Num, "print the first N context switches"),
    once("talaria.sync", Kind::Word(&["hlt", "mwait", "mwaitx", "waitpkg"]), "the wait tier"),
    once("talaria.sysprof", Kind::Flag, "a histogram of syscalls by time in the kernel"),
    once("talaria.sysrow", Kind::Text, "print these syscall numbers regardless of the top ten"),
    once("talaria.test", Kind::Text, "the self-check mode"),
    once("talaria.tickmin", Kind::Num, "the minimum run before a tick rotation, ns"),
    once("talaria.tickprof", Kind::Flag, "the tick's cost by phase"),
    once("talaria.top", Kind::Check(top_ok), "a top snapshot every N ms[,rss]"),
    once("talaria.trace", Kind::Check(trace_ok), "the early trace on disk: LBA[,eager]"),
    once("talaria.trefs", Kind::Flag, "a trace of process references"),
    once("talaria.txdrop", Kind::Num, "the loss gate: drop every N-th transmitted frame (0 — none)"),
    once("talaria.uacheck", Kind::Num, "checking syscall pointers (uaccess.rs): 0 — off (an A/B of the price)"),
    once("talaria.userdata", Kind::Flag, "read the cmdline tail from EC2 user-data"),
    once("talaria.wakebal", Kind::Num, "the placement rule on wakeup"),
    once("talaria.wakeidle", Kind::Flag, "a core with a network waiter is free for a waking thread"),
    once("talaria.wakepreempt", Kind::Num, "the preemption threshold on wakeup, ns"),
    once("talaria.wakepull", Kind::Flag, "a network waiter takes a ready thread from a busy core"),
    once("talaria.watchframe", Kind::Num, "a physical frame (hex) whose every hand-out, free and refcount change is printed with its caller and task (ARM64-PLAN §11 item 0)"),
    once("talaria.wrwin", Kind::Flag, "the preemption window between portions of a long write (0 — as before)"),
    once("talaria.x2apic", Kind::Flag, "x2APIC (0 — off)"),
];

fn knob(key: &str) -> Option<&'static Knob> { KNOBS.iter().find(|k| k.key == key) }

fn value_ok(kind: Kind, v: &str) -> bool {
    match kind {
        Kind::Flag => v == "1" || v == "0",
        Kind::Num => is_num(v),
        Kind::Word(ws) => ws.contains(&v),
        Kind::Text => !v.is_empty(),
        Kind::Check(f) => f(v),
    }
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Flag => "1 or 0",
        Kind::Num => "an integer",
        Kind::Word(_) => "one of the words",
        Kind::Text => "a string",
        Kind::Check(_) => "a composite value",
    }
}

/// The audit: print every `talaria.*` knob with its parsed value; `false`
/// — the string is NOT ACCEPTED (an unknown key, a value of the wrong kind, a duplicate of a
/// single-valued key). The reason is printed next to the key rather than at the end.
///
/// Tokens without the `talaria.` prefix (console=, virtio_mmio.device=, …) do not belong
/// to the kernel and are not checked.
pub fn audit(cmdline: &str) -> bool { audit_with(cmdline, true) }

/// The same audit; `verbose=false` — without printing (the self-test runs deliberately bad strings).
fn audit_with(cmdline: &str, verbose: bool) -> bool {
    let mut ok = true;
    macro_rules! say { ($($a:tt)*) => { if verbose { crate::println!($($a)*); } } }
    let seen_prev = |key: &str, upto: usize| -> bool {
        cmdline.split_whitespace().take(upto).any(|t| t.split_once('=').map_or(t, |(k, _)| k) == key)
    };
    for (i, tok) in cmdline.split_whitespace().enumerate() {
        if !tok.starts_with("talaria.") { continue; }
        let (key, val) = match tok.split_once('=') { Some((k, v)) => (k, Some(v)), None => (tok, None) };
        let Some(k) = knob(key) else {
            say!("talaria: the knob {} — AN UNKNOWN KEY (a typo?); the closest are: {}", tok, nearest(key));
            ok = false;
            continue;
        };
        let Some(val) = val else {
            say!("talaria: the knob {} — without a value (it needs {}={})", tok, key, kind_name(k.kind));
            ok = false;
            continue;
        };
        if !value_ok(k.kind, val) {
            match k.kind {
                Kind::Word(ws) => {
                    say!("talaria: the knob {} — the value is not from the list: {}", tok, Words(ws));
                }
                _ => say!("talaria: the knob {} — the value is of the wrong kind (expected {})", tok, kind_name(k.kind)),
            }
            ok = false;
            continue;
        }
        if k.rep == Rep::Once && seen_prev(key, i) {
            say!("talaria: the knob {} — the key is given TWICE, which value was meant?", tok);
            ok = false;
            continue;
        }
        if is_secret(key) { say!("talaria: the knob {} = *** ({} bytes) — {}", key, val.len(), k.what); }
        else { say!("talaria: the knob {} = {} — {}", key, val, k.what); }
    }
    ok
}

/// Whether a key's value is a secret (`talaria.filedtoken=`, `talaria.ctltoken=`, …).
///
/// The command line is printed in full on every boot, and on EC2 the whole
/// serial lies in `aws ec2 get-console-output` — i.e. it is readable by anyone who
/// has the right to a `DescribeInstances`-like read of the console. A token
/// printed there in the clear stops being a token.
pub fn is_secret(key: &str) -> bool { key.ends_with("token") }

/// The command line for printing: secret values are replaced with asterisks.
pub struct Masked<'a>(pub &'a str);
impl core::fmt::Display for Masked<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("\"")?;
        for (i, tok) in self.0.split_whitespace().enumerate() {
            if i > 0 { f.write_str(" ")?; }
            match tok.split_once('=') {
                Some((k, _)) if is_secret(k) => { f.write_str(k)?; f.write_str("=***")?; }
                _ => f.write_str(tok)?,
            }
        }
        f.write_str("\"")
    }
}

struct Words(&'static [&'static str]);
impl core::fmt::Display for Words {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, w) in self.0.iter().enumerate() {
            if i > 0 { f.write_str("/")?; }
            f.write_str(w)?;
        }
        Ok(())
    }
}

/// A hint on a typo: the registry's keys with the same beginning (the first 3 letters
/// after `talaria.`), at most four.
fn nearest(key: &str) -> Nearest<'_> { Nearest(key) }
struct Nearest<'a>(&'a str);
impl core::fmt::Display for Nearest<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let stem = self.0.strip_prefix("talaria.").unwrap_or(self.0);
        let head = &stem[..stem.len().min(3)];
        let mut n = 0;
        for k in KNOBS {
            let ks = &k.key["talaria.".len()..];
            if !head.is_empty() && ks.starts_with(head) {
                if n > 0 { f.write_str(", ")?; }
                f.write_str(k.key)?;
                n += 1;
                if n == 4 { break; }
            }
        }
        if n == 0 { f.write_str("(nothing similar)")?; }
        Ok(())
    }
}

#[cfg(not(talaria_host))]
/// A boot refusal: loud and final. The trace on disk is flushed and the
/// machine is powered off (on a stand — so that a run does not hang until its timeout); otherwise
/// the kernel stays in hlt. A silent "it was not applied" cost two measurement sessions.
pub fn refuse(cmdline: &str, origin: &str) -> ! {
    crate::println!("talaria: BOOT REFUSED: the command line ({}) contains something the kernel did not understand — see the lines above", origin);
    crate::println!("talaria: BOOT REFUSED: cmdline = {}", Masked(cmdline));
    crate::println!("talaria: BOOT REFUSED: all the kernel's knobs are listed in kernel/src/cmdline.rs (KNOBS), the modes in MODES, the self-checks in \
        selftest/");
    crate::trace::flush();
    crate::acpi::poweroff();
    crate::arch::cpu::halt_irq_off_forever()
}

/// A self-test of the registry and the audit (part of the gate `talaria.test=cmdline`).
pub fn audit_selftest() -> bool {
    let mut ok = true;
    let mut check = |cond: bool, what: &str| {
        if !cond {
            crate::println!("cmdline-test: FAILED — {}", what);
            ok = false;
        }
    };
    // The registry has no duplicates and is alphabetical (a duplicate in the registry is the same class
    // of error as a duplicate in the string).
    for w in KNOBS.windows(2) {
        check(w[0].key < w[1].key, "the knob registry is not alphabetical or contains a duplicate");
    }
    check(audit_with("console=ttyS0 talaria.test=cmdline talaria.smp=4 talaria.ip=dhcp", false), "the audit rejected a certainly correct string");
    check(!audit_with("talaria.chepuha=1", false), "the audit accepted an unknown key");
    check(!audit_with("talaria.smp", false), "the audit accepted a key without a value");
    check(!audit_with("talaria.smp=x", false), "the audit accepted a non-number in a numeric knob");
    check(!audit_with("talaria.nosteal=yes", false), "the audit accepted a non-flag in a flag");
    check(!audit_with("talaria.sync=nap", false), "the audit accepted a word not from the list");
    check(!audit_with("talaria.smp=2 talaria.smp=4", false), "the audit accepted a duplicate of a single-valued key");
    check(audit_with("talaria.ena=llq talaria.ena=rss", false), "the audit rejected an accumulating key");
    check(!audit_with("talaria.ena=turbo", false), "the audit accepted an unknown ena option");
    check(audit_with("talaria.trace=2048,eager talaria.top=500,rss talaria.core=/core,full", false), "the audit rejected composite values");
    check(!audit_with("talaria.trace=2048,lazy", false), "the audit accepted a wrong tail of a composite value");
    check(audit_with("talaria.ip=10.0.0.2/24 talaria.gw=10.0.0.1", false), "the audit rejected an address with a prefix");
    check(!audit_with("talaria.ip=10.0.0.256", false), "the audit accepted an address with an octet > 255");
    check(audit_with("talaria.console=vga,nopoll", false), "the audit rejected console=vga,nopoll");
    check(audit_with("virtio_mmio.device=512@0xd0000000:12 root=/dev/vda", false), "the audit touched foreign keys");
    // Every knob the kernel reads through this module must be in the registry
    // — that is the point: a knob outside the registry will not boot in a single test.
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_matches_whole_key_and_whole_token() {
        assert_eq!(value("a talaria.smp=4 b", "talaria.smp"), Some("4"));
        assert_eq!(value("talaria.smpx=4", "talaria.smp"), None);
        assert_eq!(value("xtalaria.smp=4", "talaria.smp"), None);
        assert_eq!(value("talaria.smp", "talaria.smp"), None);
        assert_eq!(value("talaria.smp=", "talaria.smp"), Some(""));
    }

    #[test]
    fn key_with_trailing_equals_is_the_same_key() {
        assert_eq!(value("talaria.wakebal=3", "talaria.wakebal="), Some("3"));
        assert_eq!(num("talaria.wakebal=3", "talaria.wakebal="), num("talaria.wakebal=3", "talaria.wakebal"));
    }

    #[test]
    fn first_occurrence_wins() {
        assert_eq!(value("talaria.smp=2 talaria.smp=4", "talaria.smp"), Some("2"));
    }

    #[test]
    fn mode_and_test_names_are_exact() {
        let c = "console=ttyS0 talaria.test=fsbig talaria.mode=procdemo";
        assert!(test_is(c, "fsbig"));
        assert!(!test_is(c, "fs"));
        assert!(!test_is(c, "fsbigrace"));
        assert!(mode_is(c, "procdemo"));
        assert!(!mode_is(c, "proc"));
        assert_eq!(test_name(c), Some("fsbig"));
        assert_eq!(test_name("console=ttyS0"), None);
    }

    #[test]
    fn numbers_decimal_hex_and_garbage() {
        assert_eq!(num("k=42", "k"), Some(42));
        assert_eq!(num("k=0x1F", "k"), Some(0x1F));
        assert_eq!(num("k=0x", "k"), None);
        assert_eq!(num("k=-1", "k"), None);
        assert_eq!(num("k=1e3", "k"), None);
        assert_eq!(num("k=18446744073709551616", "k"), None); // u64 overflow
        assert_eq!(num("", "k"), None);
    }

    #[test]
    fn flag_is_exactly_one() {
        assert!(flag("talaria.fast=1", "talaria.fast"));
        for v in ["0", "yes", "true", "11", ""] {
            assert!(!flag(&format!("talaria.fast={v}"), "talaria.fast"), "{v}");
        }
    }

    #[test]
    fn present_accepts_bare_and_valued_keys_only() {
        assert!(present("a talaria.pci b", "talaria.pci"));
        assert!(present("talaria.pci=1", "talaria.pci"));
        assert!(!present("talaria.pcix=1", "talaria.pci"));
        assert!(!present("talaria.pcix", "talaria.pci"));
    }

    #[test]
    fn registry_is_sorted_unique_and_namespaced() {
        for w in KNOBS.windows(2) {
            assert!(w[0].key < w[1].key, "{} !< {}", w[0].key, w[1].key);
        }
        for k in KNOBS {
            assert!(k.key.starts_with("talaria."), "{}", k.key);
            assert!(!k.key.ends_with('='), "{}", k.key);
            assert!(!k.what.is_empty(), "{}: no description", k.key);
        }
    }

    #[test]
    fn modes_are_sorted_and_unique() {
        for w in MODES.windows(2) { assert!(w[0] < w[1], "{} !< {}", w[0], w[1]); }
    }

    #[test]
    fn audit_accepts_good_lines() {
        for c in [
            "console=ttyS0 talaria.test=cmdline talaria.smp=4 talaria.ip=dhcp",
            "talaria.ena=llq talaria.ena=rss talaria.ena=txbench=100",
            "talaria.trace=2048,eager talaria.top=500,rss talaria.core=/core,full",
            "talaria.ip=10.0.0.2/24 talaria.gw=10.0.0.1 talaria.dns=10.0.0.1",
            "talaria.console=vga,nopoll talaria.netirq=3-5",
            "talaria.ext2=journal=data talaria.ext2=dxlimit=8",
            "talaria.uacheck=0 talaria.mode=cell",
            "virtio_mmio.device=512@0xd0000000:12 root=/dev/vda",
            "",
        ] {
            assert!(audit_with(c, false), "{c}");
        }
    }

    #[test]
    fn audit_refuses_bad_lines() {
        for c in [
            "talaria.chepuha=1",                  // unknown key
            "talaria.smp",                        // no value
            "talaria.smp=x",                      // not a number
            "talaria.nosteal=yes",                // not a flag
            "talaria.sync=nap",                   // not in the word list
            "talaria.smp=2 talaria.smp=4",        // repeated single-valued key
            "talaria.ena=turbo",                  // unknown ENA option
            "talaria.trace=2048,lazy",            // bad compound tail
            "talaria.ip=10.0.0.256",              // octet > 255
            "talaria.ip=10.0.0.1/33",             // prefix > 32
            "talaria.gw=dhcp",                    // gateway must be an address
            "talaria.core=core",                  // relative core path
            "talaria.console=",                   // empty
            "talaria.netirq=a-b",
        ] {
            assert!(!audit_with(c, false), "{c}");
        }
    }

    #[test]
    fn every_knob_accepts_a_value_of_its_kind() {
        for k in KNOBS {
            let v = match k.kind {
                Kind::Flag => "1",
                Kind::Num => "7",
                Kind::Word(ws) => ws[0],
                Kind::Text => "x",
                Kind::Check(_) => continue,
            };
            assert!(audit_with(&format!("{}={}", k.key, v), false), "{}={}", k.key, v);
        }
    }

    #[test]
    fn many_knobs_may_repeat_once_knobs_may_not() {
        for k in KNOBS {
            let v = match k.kind { Kind::Flag => "1", Kind::Num => "7", Kind::Word(ws) => ws[0], Kind::Text => "x", Kind::Check(_) => continue };
            let line = format!("{0}={1} {0}={1}", k.key, v);
            assert_eq!(audit_with(&line, false), k.rep == Rep::Many, "{line}");
        }
    }

    #[test]
    fn secrets_are_masked_in_printed_cmdline() {
        let shown = format!("{}", Masked("a talaria.filedtoken=hunter2 talaria.ctltoken=s3 b=c"));
        assert_eq!(shown, "\"a talaria.filedtoken=*** talaria.ctltoken=*** b=c\"");
        assert!(is_secret("talaria.filedtoken"));
        assert!(!is_secret("talaria.smp"));
    }

    #[test]
    fn nearest_suggests_keys_with_the_same_stem() {
        let s = format!("{}", nearest("talaria.smpp"));
        assert!(s.contains("talaria.smp"), "{s}");
        assert!(s.split(", ").count() <= 4);
        assert_eq!(format!("{}", nearest("talaria.")), "(nothing similar)");
    }

    #[test]
    fn in_kernel_audit_selftest_passes() {
        assert!(audit_selftest());
    }

    #[test]
    fn compound_value_checkers() {
        assert!(ip_ok("dhcp") && !ipaddr_ok("dhcp"));
        assert!(ip_ok("1.2.3.4") && ip_ok("1.2.3.4/0") && !ip_ok("1.2.3") && !ip_ok("1.2.3.4.5"));
        assert!(trace_ok("0x800") && trace_ok("2048,eager") && !trace_ok("eager"));
        assert!(top_ok("500") && top_ok("500,rss") && !top_ok("500,vss"));
        assert!(core_ok("/c") && core_ok("/c,full") && !core_ok("/c,half"));
        assert!(netirq_ok("all") && netirq_ok("5") && netirq_ok("3-7") && !netirq_ok("x"));
        assert!(ext2_ok("nojournal") && ext2_ok("dxlimit=4") && !ext2_ok("dxlimit="));
        assert!(ena_ok("rxbench=10") && !ena_ok("rxbench=x"));
    }
}
