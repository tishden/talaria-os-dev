//! `kstat` — the kernel's counters and structures by section.
//!
//! ## How it differs from `stat`
//!
//! `stat` prints EVERYTHING in one stream — the right format for a post-mortem
//! (the workload is over, the log went into a file, we read it whole). For a live machine it is
//! unusable: fifty lines scroll the screen away, while usually one of them is being looked for.
//! `kstat <section>` answers a specific question: the scheduler, memory,
//! the page tables, the FS, the network, synchronisation, interrupts, syscalls, the cache.
//!
//! ## The section's rule
//!
//! Every line must answer a question the operator CAN ASK, and next to a number there must
//! stand whatever it is compared with. "Cache misses 12000" is
//! not a fact, while "misses 12000 out of 3.4 million (0.35%)" is. Counters whose
//! correct value is zero are marked with a word: a silent non-zero error
//! counter goes unnoticed.
use super::{bytes, dur, pct};
use core::sync::atomic::Ordering::Relaxed;

const NC: usize = crate::percpu::MAXCPU;

/// 255 in the placement means "not set". Printing 255 is inviting someone to look for NUMA
/// node number 255, which exists on no machine.
fn unset(v: u8) -> super::S<8> {
    use core::fmt::Write;
    let mut s = super::S::new();
    if v == 255 { let _ = write!(s, "-"); } else { let _ = write!(s, "{}", v); }
    s
}

pub fn sched() {
    let n = crate::sched::task_count();
    crate::println!("kstat sched: tasks {} (the ceiling {}), scheduler cores {:#x}, the slice {}",
        n, crate::sched::max_tasks(), crate::sched::sched_cpus(), dur(crate::smp::effective_slice_ns()));
    crate::println!("kstat sched: switches {}, work steals {}, migrations {}, transplants on wakeup {}",
        crate::sched::switches_total(), crate::sched::STOLEN.load(Relaxed),
        crate::affinity::MIGRATIONS.load(Relaxed), crate::affinity::WAKE_MOVED.load(Relaxed));
    crate::println!("kstat sched: scheduler ticks {}, hardware ticks {}, the pulse {}",
        crate::sched::HEARTBEAT_TICKS.load(Relaxed), crate::sched::HW_TICKS.load(Relaxed),
        if crate::sched::FIRST_TICK.load(Relaxed) { "is going" } else { "THERE WAS NOT A SINGLE TICK" });
    crate::println!("kstat sched: the cores (the role, the timer, ready tasks, occupancy, switches):");
    for c in 0..NC {
        if crate::affinity::role(c) == 0 && c != 0 { continue; }
        let (busy_t, idle_t) = crate::sched::cpu_ticks(c);
        crate::println!("kstat sched:   core {:<2} {} timer {:<7} ready {:<3} busy {} (ticks {}/{}) switches {} node {} pool {}",
            c, super::role(c),
            if crate::sched::cpu_timer_armed(c) { "armed" } else { "IDLE" },
            crate::sched::runnable_on(c), dur(crate::sched::cpu_busy_ns(c)), busy_t, busy_t + idle_t,
            crate::sched::cpu_switches(c), unset(crate::affinity::node_of(c)), unset(crate::affinity::pool_of_cpu(c)));
    }
}

pub fn mem() {
    let total = crate::mem::ram_total_frames();
    let free = crate::mem::free_frames();
    let recl = crate::mem::free_list_pages();
    crate::println!("kstat mem: physical memory {} ({} frames) in {} areas",
        bytes(total * 4096), total, crate::mem::ram_regions());
    for i in 0..crate::mem::ram_regions() {
        let (b, l) = crate::mem::ram_region(i);
        crate::println!("kstat mem:   {:#012x}..{:#012x} — {}", b, b + l, bytes(l));
    }
    crate::println!("kstat mem: free {} = bump {} + returned {}; used {}",
        bytes((free + recl) * 4096), bytes(free * 4096), bytes(recl * 4096),
        bytes(total.saturating_sub(free + recl) * 4096));
    crate::println!("kstat mem: the kernel occupies {} ({:#x}..{:#x})",
        bytes(crate::mem::kernel_frames() * 4096), crate::mem::kernel_lo(), crate::mem::kernel_hi());
    crate::println!("kstat mem: allocations {}, frames handed out {} ({})",
        crate::metrics::frame_allocs(), crate::metrics::frames_given(),
        bytes(crate::metrics::frames_given() * 4096));
}

pub fn paging() {
    crate::println!("kstat paging: faults {} (of them COW {}), COW copies {}",
        crate::metrics::page_faults(), crate::metrics::cow_faults(), crate::paging::cow_count());
    crate::println!("kstat paging: 2MB large pages handed out {} (reservations {}, traded {}, not found contiguous {})",
        crate::paging::HUGE_GIVEN.load(Relaxed), crate::paging::HUGE_RESERVED.load(Relaxed),
        crate::paging::HUGE_SPLIT.load(Relaxed), crate::paging::HUGE_NOMEM.load(Relaxed));
    // A TLB shootdown on neighbouring cores is a rare but expensive operation; the timeout here
    // must be zero: it means a core did not acknowledge the flush, i.e.
    // somewhere a TLB entry for a removed page lives on.
    let to = crate::paging::SHOOT_TIMEOUT.load(Relaxed);
    let (rej, giveup) = crate::paging::lazy_stats();
    crate::println!("kstat paging: lazy filling — frames rejected {}, give-ups {} (the norm is zero give-ups)", rej, giveup);
    // THE IDENTITY INVARIANT in the CURRENT address space. The kernel addresses
    // physical frames by their identity address, while the low area of the workload's AS is its
    // own copy of the directory: a hole here means a #PF in ring 0 on the kernel's very first
    // access to such a frame, and a substitution means a silent write into someone else's
    // page. The norm is three zeros; a non-zero value is printed as an ALARM.
    let (ih, ish, iro) = crate::paging::ident_check();
    crate::println!("kstat paging: identity 0..4GB — holes {} substitutions {} read-only {}{}",
        ih, ish, iro, if ih | ish | iro == 0 { "" } else { "  <- ALARM" });
    crate::println!("kstat paging: inter-core TLB shootdowns served {}, NOT ACKNOWLEDGED {}{}",
        crate::paging::TLB_SERVED.load(Relaxed), to,
        if to == 0 { " (the norm is zero)" } else { " <- MUST BE ZERO" });
    {   let hf = crate::paging::SHOOT_HOSTFLUSH.load(Relaxed);
        if hf != 0 || crate::smp::steal_on() {
            // A separate line, because this is NOT a failure: the neighbour was taken off its core
            // by the host, and flushing its TLB is the host's job (KVM_VCPU_FLUSH_TLB).
            crate::println!("kstat paging: acknowledgements not waited for (the neighbour was taken off its core by the host) {}", hf);
        }
    }
    // THE PRICE of waiting for an acknowledgement. It lands inside munmap/mprotect, i.e.
    // straight into a request's latency, and "zero give-ups" says nothing about it:
    // a 9 ms wait does not count as a give-up.
    {   let n = crate::paging::SHOOT_WAITS.load(Relaxed);
        if n != 0 {
            let hz = crate::tsc::calibrate_cached().max(1);
            let us = |c: u64| c.saturating_mul(1_000_000) / hz;
            crate::println!("kstat paging: acknowledgement waits {}, {} ms in total, the worst {} µs; longer than 10µs {} / 100µs {} / 1ms {}",
                n, us(crate::paging::SHOOT_WAIT_TSC.load(Relaxed)) / 1000,
                us(crate::paging::SHOOT_MAX_TSC.load(Relaxed)),
                crate::paging::SHOOT_OVER[0].load(Relaxed),
                crate::paging::SHOOT_OVER[1].load(Relaxed),
                crate::paging::SHOOT_OVER[2].load(Relaxed));
        }
    }
}

pub fn fs() {
    match crate::shell::backend() {
        crate::shell::Backend::Ext2 => {
            let (bs, t, f, _, _) = crate::ext2::statfs();
            let (bs, t, f) = (bs as u64, t as u64, f as u64);
            crate::println!("kstat fs: ext2 RW, the block is {} B, {} in total ({} blocks), free {}, the journal is {}",
                bs, bytes(t * bs), t, bytes(f * bs),
                if crate::ext2::jrn_active() { "on" } else { "off" });
            let (h, m) = crate::ext2::cache_stats();
            if h + m != 0 {
                crate::println!("kstat fs: the block cache {} MB: hits {}, misses {} (bypassed {})",
                    crate::ext2::cache_mb(), h, m, pct(m * 10_000 / (h + m)));
            }
            let (rd, wr) = crate::ext2::stats();
            let (_, runs, flush) = crate::ext2::wr_stats();
            let (ra_n, ra_extra) = crate::ext2::ra_stats();
            crate::println!("kstat fs: {} blocks read, {} written ({} ranges), device barriers {}, read-aheads {} (+{} blocks)",
                rd, wr, runs, flush, ra_n, ra_extra);
            let (takes, cont, wait_us, held_us, to, alien, yields) = crate::ext2::lock_stats();
            if takes != 0 {
                crate::println!("kstat fs: the lock: takes {}, with a wait {} ({}), waited {}, held {}",
                    takes, cont, pct(cont * 10_000 / takes), dur(wait_us * 1000), dur(held_us * 1000));
                crate::println!("kstat fs: the lock: longer than a second {}, foreign entries {}, yielded the core {}{}",
                    to, alien, yields,
                    if to == 0 && alien == 0 { " (the first two: the norm is zero)" } else { " <- THE FIRST TWO MUST BE ZEROS" });
                let (chunks, bal, devwait) = crate::ext2::wb_stats();
                crate::println!("kstat fs: the flush without the lock: portions {}, the balance {}, waited for the device {}, yields {}", chunks, bal, devwait, crate::fs::dev_handoffs());
                let (tx, ck, b) = crate::ext2::barrier_stats();
                crate::println!("kstat fs: the journal: transactions {}, checkpoints {} (copies from the journal {}); the barriers: data {}, the transaction {}, the checkpoint {}, the journal's superblock {}, the recover flag {}, others {}; journal walks {}, waited for someone else's transaction longer than the diagnosis threshold {}",
                    tx, ck, crate::ext2::ckpt_old(), b[0], b[1], b[2], b[3], b[4], b[5], crate::ext2::JRN_BYPASS.load(core::sync::atomic::Ordering::Relaxed), crate::ext2::JRN_LONGWAIT.load(core::sync::atomic::Ordering::Relaxed));
            }
        }
        crate::shell::Backend::Tfs => crate::println!("kstat fs: TFS (read-only), files {}", crate::fs::nfiles()),
        crate::shell::Backend::None => crate::println!("kstat fs: the FS is not mounted"),
    }
}

pub fn net() {
    if !crate::netsys::ready() { crate::println!("kstat net: the stack is not up"); return; }
    let (rx, tx, live) = crate::netsys::totals();
    let (l, u) = crate::netsys::listen_udp_count();
    crate::println!("kstat net: interfaces {}, connections {}, listeners {}, UDP {}; sockets in total {}+{}",
        crate::netsys::nifs(), live, l, u, crate::netsys::NSOCK, crate::netsys::NUDP);
    crate::println!("kstat net: bytes over the sockets rx {} tx {}", bytes(rx), bytes(tx));
    // Waits on the stack's shared lock: if this grows, the cores are fighting over the network, and that is
    // a question to the thread placement (pin=/pool= in the manifest) rather than to the lock itself.
    let (spins, skips) = crate::netsys::lock_spins();
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let t = crate::netsys::NET_TAKES.load(R);
        if t != 0 {
            let hz = crate::tsc::calibrate_cached().max(1);
            let ms = |c: u64| c / (hz / 1000).max(1);
            let us = |c: u64| c.saturating_mul(1_000_000) / hz;
            let c = crate::netsys::NET_CONT.load(R);
            crate::println!("kstat net: the stack's lock: takes {}, with a wait {} ({}%), waited {} ms, held {} ms ({} ns per take, the worst {} µs), longer than a second {}",
                t, c, c * 100 / t, ms(crate::netsys::NET_WAIT_TSC.load(R)),
                ms(crate::netsys::NET_HELD_TSC.load(R)),
                crate::netsys::NET_HELD_TSC.load(R).saturating_mul(1_000_000_000 / 1000) / (hz / 1000).max(1) / t,
                us(crate::netsys::NET_HELD_MAX.load(R)), crate::netsys::NET_LONGWAIT.load(R));
            let w = crate::netsys::NET_HELD_MAX_WHO.load(R);
            crate::println!("kstat net:   the worst hold: task {} syscall {} core {} from {}",
                w & 0xffff, (w >> 16) & 0xffff, (w >> 32) & 0xff,
                match (w >> 40) & 0xff { 1 => "a tick", 2 => "the NIC's line", _ => "a syscall" });
            let h = &crate::netsys::NET_HELD_HIST;
            crate::println!("kstat net:   holds <10 µs {}, <100 µs {}, <1 ms {}, >=1 ms {}",
                h[0].load(R), h[1].load(R), h[2].load(R), h[3].load(R));
            for e in crate::netsys::NET_HELD_LONG.iter() {
                let k = e.0.load(R);
                if k == u32::MAX { continue; }
                let n = e.1.load(R).max(1);
                match k {
                    0x1_0000 => crate::println!("kstat net:   long ones from a tick: {} ({} µs on average)", n, e.2.load(R) / n),
                    0x2_0000 => crate::println!("kstat net:   long ones from the NIC's line: {} ({} µs on average)", n, e.2.load(R) / n),
                    sc => crate::println!("kstat net:   long ones from syscall {}: {} ({} µs on average)", sc, n, e.2.load(R) / n),
                }
            }
        }
    }
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let n = crate::netsys::NIC_IRQS.load(R);
        let g = crate::netsys::NETIRQ_GSI.load(R);
        if n != 0 || g != u32::MAX {
            crate::println!("kstat net: interrupts from the NIC {} (the stack was busy {}), the line {}",
                n, crate::netsys::NIC_IRQ_BUSY.load(R), crate::netsys::netirq_line_name());
        }
    }
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let p = crate::netsys::NET_PARKS.load(R);
        if p != 0 {
            // Many parkings and almost no ceiling exits means the waiter is woken by
            // DELIVERY, i.e. the NIC's line works and polling the stack has been replaced by
            // an interrupt. The opposite ratio means the line is silent and the waiter
            // sleeps out the ceiling on every frame.
            let tmo = crate::netsys::NET_PARK_TMO.load(R);
            crate::println!("kstat net: waiter parkings {}, woken by delivery {}, exited on the ceiling {} ({}%); calls from delivery {}",
                p, crate::netsys::NET_PARK_WAKE.load(R), tmo, tmo * 100 / p,
                crate::netsys::NET_DELIV_WAKE.load(R));
        }
    }
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let sk = crate::netsys::POLL_SKIPPED.load(R);
        let hard = crate::netsys::POLL_HARD_RUNS.load(R);
        if sk != 0 || hard != 0 {
            // A zero in the last column is the proof that "the NIC is silent"
            // is determined correctly (see netsys::RING_LIED).
            crate::println!("kstat net: idle passes cancelled {} (done on the timers' debt {}), the ring lied {} times",
                sk, hard, crate::netsys::RING_LIED.load(R));
        }
    }
    crate::println!("kstat net: waits on the stack's lock {}, the watch stepped back {} times", spins, skips);
    // Who gets the CPU when a waiter yields it: DIRECTLY to the thread whose frame
    // arrived, or into the common rotation. A zero in the first column with a live
    // multithreaded workload means the direct handover does not fire —
    // and that is exactly the tail where p50 is fine while p99 equals the quantum.
    {
        let d = crate::netsys::YIELD_DIRECT.load(Relaxed);
        let r = crate::netsys::YIELD_ROTATE.load(Relaxed);
        if d + r != 0 {
            crate::println!("kstat net: CPU yields: directly {} ({}%), by rotation {}",
                d, d * 100 / (d + r), r);
        }
    }
    // The datapath borrowing the NIC. While this was silent, "the NIC is unavailable" and
    // "there is no traffic" looked IDENTICAL in the report — as zeros.
    {
        let ok = crate::l1::DEV_BORROW_OK.load(Relaxed);
        let busy = crate::l1::DEV_BORROW_BUSY.load(Relaxed);
        if ok + busy != 0 {
            let stolen = crate::l1::STOLEN.load(Relaxed);
            crate::println!("kstat net: NIC ownership: taken {}, busy {}, TAKEN FROM A DEAD ONE {}{}",
                ok, busy, stolen,
                if stolen != 0 { " <- a task vanished inside the stack (the norm is zero)" }
                else if busy > ok / 4 && busy > 1000 { " <- the NIC is often unavailable" } else { "" });
        }
    }
    for i in 0..crate::netsys::nifs() {
        // The driver's name and the counters are taken in ONE snapshot: every access to the
        // NIC may fail to get ownership, and separate calls gave a line
        // where the name is already "?" while the numbers are still real (or the other way round). Diagnostics
        // that contradict themselves are worse than none.
        let (drv, rn, rb, tn, tb, dr) = crate::netsys::if_snapshot(i);
        crate::println!("kstat net:   net{} ({}): rx {} frames / {}, tx {} frames / {}, transmit drops {}{}",
            i, drv, rn, bytes(rb), tn, bytes(tb), dr,
            if drv == "?" { " <- THE NIC IS UNAVAILABLE (ownership is busy), the numbers above were NOT taken" }
            else if dr != 0 { " <- THE TRANSMIT RING IS NOT BEING FREED" }
            else { "" });
    }
}

pub fn sync() {
    let c = crate::sync::W_CALLS.load(Relaxed);
    crate::println!("kstat sync: the wait primitive is {}", crate::sync::mode_name(crate::sync::mode()));
    if c == 0 { crate::println!("kstat sync: there were no futex waits"); return; }
    crate::println!("kstat sync: futex waits {}: missed at once {}, woken {}, the word itself {}, a timeout {}, a signal {}",
        c, crate::sync::W_EAGAIN.load(Relaxed), crate::sync::W_WOKEN.load(Relaxed),
        crate::sync::W_PRED.load(Relaxed), crate::sync::W_TIMEOUT.load(Relaxed), crate::sync::W_SIG.load(Relaxed));
    crate::println!("kstat sync: exits {}, of them after sleeping {}, on \"the task is dead\" {}",
        crate::sync::W_EXIT.load(Relaxed), crate::sync::W_SLEPT.load(Relaxed), crate::sync::W_DEAD.load(Relaxed));
    let names = ["WAIT", "WAKE", "FD", "REQUEUE", "CMP_REQUEUE", "WAKE_OP", "LOCK_PI",
                 "UNLOCK_PI", "TRYLOCK_PI", "WAIT_BITSET", "WAKE_BITSET", "?11", "?12", "?13", "?14", "?15"];
    let mut any = false;
    for o in 0..16usize {
        let v = crate::syscall::FUTEX_OPS[o].load(Relaxed);
        if v == 0 { continue; }
        if !any { crate::print!("kstat sync: by operation:"); any = true; }
        crate::print!(" {}={}", names[o], v);
    }
    if any { crate::println!(""); }
}

pub fn irq() {
    crate::println!("kstat irq: timer ticks {}, IPIs received {}, the interrupt core {}",
        crate::idt::TICKS.load(Relaxed), crate::idt::IPI_COUNT.load(Relaxed), crate::affinity::irq_core());
    crate::println!("kstat irq: the timer's vector {:#x}, the IPI's vector {:#x}",
        crate::idt::TIMER_VECTOR, crate::idt::IPI_VECTOR);
}

pub fn cache() {
    crate::cat::dump();
    if crate::pmu::available() {
        crate::println!("kstat cache: the PMU — {} GP + {} fixed counters, cores with counters taken {:#x}",
            crate::pmu::ngp(), crate::pmu::nfixed(), crate::pmu::armed_mask());
    } else {
        crate::println!("kstat cache: the PMU is unavailable — cache misses are not counted");
    }
}

pub fn syscalls() { crate::metrics::top_syscalls(); }

const SECTIONS: [(&str, fn()); 9] = [
    ("sched", sched as fn()), ("mem", mem), ("paging", paging), ("fs", fs), ("net", net),
    ("sync", sync), ("irq", irq), ("cache", cache), ("syscalls", syscalls),
];

pub fn cmd(arg: &str) {
    let a = arg.trim();
    if a.is_empty() || a == "all" {
        for (i, (_, f)) in SECTIONS.iter().enumerate() { if i > 0 { crate::println!(""); } f(); }
        return;
    }
    for (name, f) in SECTIONS { if name == a { f(); return; } }
    crate::print!("kstat: there is no section '{}'. There are:", a);
    for (name, _) in SECTIONS { crate::print!(" {}", name); }
    crate::println!("");
}
