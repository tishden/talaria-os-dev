//! The cores and the tick under the names the shared kernel uses on x86-64 — `arch::smp`
//! (docs/ARM64-PLAN.md §10.1, stage 3.1). The MECHANISM is the GICv3 (`gic.rs`), the generic
//! timer (`timer.rs`) and PSCI (`psci.rs`); the POLICY (the slice and its knobs) is
//! `crate::slice`, re-exported here as on x86. The vocabulary is the LAPIC's, kept on
//! purpose: `apic_of` is the core's affinity (`MPIDR`), `send_ipi` is an SGI,
//! `lapic_timer_init` arms the deadline, `lapic_eoi` is a no-op because the IRQ vector
//! ends every interrupt itself. Renaming the callers is a later, mechanical commit; this
//! one is about the port compiling.

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use super::{cpu, gic, psci, timer, trap};

pub use crate::slice::{TIMER_SLICE, SLICE_NS, NETSLICE_NS, IDLESLICE_NS, netslice_from_cmdline, idleslice_from_cmdline,
    long_tick, want_period_ns, effective_slice_ns};

/// An AP's role at the start (as on x86): 0 = the cell's datapath, 1 = the sync self-test,
/// 2 = the scheduler, 3 = the function in `AP_HOOK`.
pub static AP_MODE: AtomicU8 = AtomicU8::new(0);
pub static AP_HOOK: AtomicU64 = AtomicU64::new(0);
pub static AP_ALIVE: AtomicU64 = AtomicU64::new(0);
pub static AP_HEARTBEAT: AtomicU64 = AtomicU64::new(0);
pub static AP_STAGE: AtomicU32 = AtomicU32::new(0);
/// x86 knobs and counters that have no arm64 meaning; they read as zero.
pub static AP_CR4_MASK: AtomicU64 = AtomicU64::new(0);
pub static PV_EOI_MISS: AtomicU64 = AtomicU64::new(0);
pub static IPI_PV: AtomicU64 = AtomicU64::new(0);
/// IPIs sent (the SGI path; the name is the x86 counter's).
pub static IPI_ICR: AtomicU64 = AtomicU64::new(0);
pub static RETUNES: AtomicU64 = AtomicU64::new(0);
/// The counter's frequency plays the timer's (the generic timer counts the same clock).
pub static LAPIC_HZ: AtomicU64 = AtomicU64::new(0);

pub const MAXCPU: usize = crate::percpu::MAXCPU;
static NCPU: AtomicU32 = AtomicU32::new(1);
/// The affinity (`MPIDR_EL1` without the flag bits) of every logical core; u64::MAX = unknown.
static AFF_OF: [AtomicU64; MAXCPU] = [const { AtomicU64::new(u64::MAX) }; MAXCPU];

pub fn x2apic() -> bool { false }
pub fn legacy_quiesce(_cmdline: &str) {}
pub fn force_xapic() {}
pub fn pveoi_from_cmdline(_cmdline: &str) {}
pub fn pveoi_enable_here() {}
pub fn steal_enable_here() {}
pub fn steal_on() -> bool { false }
pub fn steal_ns(_cpu: usize) -> u64 { 0 }
pub fn vcpu_preempted(_cpu: usize) -> bool { false }
pub fn ask_host_flush(_cpu: usize) -> bool { false }
pub const KVM_VCPU_PREEMPTED: u8 = 1;
pub const KVM_VCPU_FLUSH_TLB: u8 = 2;

/// This core's interrupt controller side, idempotent: the redistributor, the CPU interface,
/// the SGIs the kernel uses and the timer PPI enabled.
pub fn lapic_enable() {
    unsafe {
        gic::init_cpu();
        for sgi in 0..16u32 { gic::enable_local(sgi); }
        gic::enable_local(trap::TIMER_PPI.load(Ordering::Relaxed));
    }
}
/// Ending an interrupt is the IRQ vector's job on arm64 (`trap::irq` writes `ICC_EOIR1`
/// for every INTID it acknowledged), so the explicit call the x86 handlers make is nothing here.
/// # Safety
/// The x86 twin is `unsafe extern "C"`; the callers keep their blocks.
pub unsafe fn lapic_eoi() {}

/// The cores the device tree (later: the MADT) named, in the order they were listed; the
/// boot core is logical 0 whatever its affinity.
pub fn topology_init() {
    let me = cpu::mpidr() & 0xFF00_FFFFFF;
    AFF_OF[0].store(me, Ordering::SeqCst);
    let mut n = 1usize;
    for i in 0..super::probes::MAX_AP {
        let aff = super::probes::CPU_MPIDR[i].load(Ordering::Relaxed);
        if aff == u64::MAX || aff == me || n >= MAXCPU { continue; }
        AFF_OF[n].store(aff, Ordering::SeqCst);
        n += 1;
    }
    NCPU.store(n as u32, Ordering::SeqCst);
}
pub fn selftest_pack() {}
pub fn pack_order(_ids: &[u32], _sh: u32, _out: &mut [u32]) {}
pub fn nr_cpus() -> usize { NCPU.load(Ordering::SeqCst) as usize }
/// The affinity of a logical core, truncated to the 32 bits the callers carry (Aff0..Aff2
/// and Aff3 in the top byte): the fallback before `topology_init` is the number itself.
pub fn apic_of(cpu: usize) -> u32 {
    if cpu >= MAXCPU { return cpu as u32; }
    let v = AFF_OF[cpu].load(Ordering::SeqCst);
    if v == u64::MAX { cpu as u32 } else { pack_aff(v) }
}
/// The affinity (MPIDR without the flag bits) of a logical core; None before the map is built.
pub fn mpidr_of(cpu: usize) -> Option<u64> {
    if cpu >= MAXCPU { return None; }
    let v = AFF_OF[cpu].load(Ordering::SeqCst);
    if v != u64::MAX { Some(v) } else if cpu == 0 { Some(cpu::mpidr() & 0xFF00_FFFFFF) } else { None }
}
fn pack_aff(mpidr: u64) -> u32 { ((mpidr & 0xFF_FFFF) | ((mpidr >> 32) & 0xFF) << 24) as u32 }
fn unpack_aff(a: u32) -> u64 { (a as u64 & 0xFF_FFFF) | ((a as u64 >> 24) << 32) }
pub fn logical_of(apic: u32) -> usize {
    for l in 0..MAXCPU { if AFF_OF[l].load(Ordering::SeqCst) == unpack_aff(apic) { return l; } }
    usize::MAX
}
pub fn my_logical() -> usize {
    let l = logical_of(pack_aff(cpu::mpidr() & 0xFF00_FFFFFF));
    if l == usize::MAX { 0 } else { l }
}
pub fn bsp_apic_id() -> u32 { apic_of(0) }

/// Bring the logical core `cpu` up through PSCI `CPU_ON` with a stack of its own; it
/// arrives in `rust_ap_main` (kmain.rs) and takes the role `AP_MODE` says.
pub fn start_cpu(cpu: usize) -> bool {
    if cpu == 0 || cpu >= nr_cpus() { return false; }
    start_ap(apic_of(cpu))
}
pub fn start_ap(apic_id: u32) -> bool {
    let l = logical_of(apic_id);
    if l == usize::MAX || l >= super::probes::MAX_AP { return false; }
    if crate::percpu::ONLINE.load(Ordering::SeqCst) & (1 << l) != 0 { return true; }
    // The stack the core's pickup loop re-enters on when a task EXITS on it (x86 sets it at
    // the same place). Without it the first thread to finish on a secondary moved the core
    // onto sp = 0: the fault's own entry faulted, forever and in silence, and every task
    // queued on that core waited for it (`futexlat` stopped after its first phase).
    crate::sched::set_ap_stack(l, super::kmain::ap_stack_top(l));
    let rc = super::kmain::start_secondary(l, unpack_aff(apic_id));
    if rc != psci::SUCCESS && rc != psci::ALREADY_ON { return false; }
    let t0 = cpu::counter();
    let lim = timer::ticks_for_ns(2_000_000_000);
    // A cell's core never enters ONLINE (`kmain::rust_ap_main`); its report is AP_UP.
    let cell = crate::runmode::CELL_AP.load(Ordering::SeqCst);
    while crate::percpu::ONLINE.load(Ordering::SeqCst) & (1 << l) == 0
        && !(cell && super::kmain::AP_UP[l].load(Ordering::Acquire) != 0) {
        if cpu::counter().wrapping_sub(t0) > lim { return false; }
        core::hint::spin_loop();
    }
    true
}

pub fn send_nmi(cpu: usize) { send_ipi(apic_of(cpu), crate::idt::IPI_VECTOR as u8); }
/// An inter-core call is an SGI: the vector IS the SGI number (0..15, `idt.rs`) — except
/// the timer's vector, which the shared code sends to make another core enter its
/// scheduler: that is `RESCHED_SGI` (masked to four bits it was SGI 11, which nobody
/// handles — `execve` waited a second for sister threads that never left their cores).
pub fn send_ipi(apic_id: u32, vector: u8) {
    IPI_ICR.fetch_add(1, Ordering::Relaxed);
    let sgi = if vector as usize == crate::idt::TIMER_VECTOR { crate::idt::RESCHED_SGI } else { (vector & 0xF) as u32 };
    gic::send_sgi(unpack_aff(apic_id), sgi);
}

// ── the tick: a deadline re-armed from the vector ────────────────────────────────
static TIMER_PERIOD: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
static TIMER_REM: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
static TIMER_ONESHOT: [AtomicU32; MAXCPU] = [const { AtomicU32::new(0) }; MAXCPU];
static LAST_RETUNE: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
fn me() -> usize { crate::percpu::cpu_id().min(MAXCPU - 1) }

pub fn lapic_calibrate() { LAPIC_HZ.store(cpu::counter_hz(), Ordering::SeqCst); }
/// Arm this core's tick at `period_ns` (the knob `talaria.slice=` overrides it).
pub fn lapic_timer_init(period_ns: u32) {
    let over = TIMER_SLICE.load(Ordering::Relaxed);
    let p = if over != 0 { over } else { period_ns } as u64;
    TIMER_PERIOD[me()].store(p, Ordering::Relaxed);
    trap::TICK_NS.store(p, Ordering::Relaxed);
    timer::arm(p);
}
pub fn lapic_timer_stop() { TIMER_PERIOD[me()].store(0, Ordering::Relaxed); timer::stop(); }
pub fn timer_running() -> bool { TIMER_PERIOD[me()].load(Ordering::Relaxed) != 0 }
pub fn my_period_ns() -> u64 { TIMER_PERIOD[me()].load(Ordering::Relaxed) }
pub fn timer_armed_on(c: usize) -> bool { c < MAXCPU && TIMER_PERIOD[c].load(Ordering::Relaxed) != 0 }
/// The period THIS core's vector re-arms with (the tick handler asks).
pub fn period_here() -> u64 { TIMER_PERIOD[me()].load(Ordering::Relaxed) }

/// What the tick vector re-arms with: the scheduler's period, else the bring-up's
/// `TICK_NS`; 0 while a borrowed one-shot is out (`timer_tick_reload` re-arms).
pub fn rearm_period_ns() -> u64 {
    let c = me();
    if TIMER_ONESHOT[c].load(Ordering::Relaxed) != 0 || TIMER_REM[c].load(Ordering::Relaxed) != 0 { return 0; }
    let p = TIMER_PERIOD[c].load(Ordering::Relaxed);
    if p != 0 { return p; }
    // No period of the scheduler's: the bring-up's tick, but only until the scheduler owns the
    // timer — after that a zero period is a DEDICATED core (`sched::ap_loop` stopped its
    // tick on purpose), and a borrowed deadline firing there must not revive the tick.
    if super::idt::SCHED_TIMER.load(Ordering::Relaxed) != 0 { 0 } else { trap::TICK_NS.load(Ordering::Relaxed) }
}
/// Lend the tick's timer to a one-shot deadline (`sched`'s TSC-deadline waits): remember
/// what was left of the current period.
pub fn timer_borrow() {
    let c = me();
    if TIMER_PERIOD[c].load(Ordering::Relaxed) == 0 || TIMER_REM[c].load(Ordering::Relaxed) != 0 { return; }
    let cval = timer::compare_value();
    let now = cpu::counter();
    TIMER_REM[c].store(cval.wrapping_sub(now).max(1), Ordering::Relaxed);
}
/// After a borrowed one-shot fired: back to the period.
pub fn timer_tick_reload() {
    let c = me();
    if TIMER_ONESHOT[c].swap(0, Ordering::Relaxed) == 0 { return; }
    let p = TIMER_PERIOD[c].load(Ordering::Relaxed);
    if p != 0 { timer::arm(p); }
}
/// Give the timer back: the rest of the period as a one-shot, or the period itself.
pub fn lapic_timer_restore() {
    let c = me();
    let p = TIMER_PERIOD[c].load(Ordering::Relaxed);
    if p == 0 { timer::stop(); return; }
    let rem = TIMER_REM[c].swap(0, Ordering::Relaxed);
    if rem != 0 {
        TIMER_ONESHOT[c].store(1, Ordering::Relaxed);
        timer::arm_ticks(rem);
        return;
    }
    timer::arm(p);
}
/// Reconfigure this core's period if it must become a different one (the same guards as
/// on x86: not while borrowed, not more often than four base periods).
pub fn retune_timer(period_ns: u64) {
    let c = me();
    let cur = TIMER_PERIOD[c].load(Ordering::Relaxed);
    if cur == 0 || cur == period_ns || TIMER_REM[c].load(Ordering::Relaxed) != 0 { return; }
    let now = cpu::counter();
    let guard = timer::ticks_for_ns(effective_slice_ns() * 4);
    let last = LAST_RETUNE[c].load(Ordering::Relaxed);
    if last != 0 && now.wrapping_sub(last) < guard { return; }
    LAST_RETUNE[c].store(now, Ordering::Relaxed);
    TIMER_PERIOD[c].store(period_ns, Ordering::Relaxed);
    crate::slice::set_long_tick(c, period_ns > effective_slice_ns());
    RETUNES.fetch_add(1, Ordering::Relaxed);
    timer::arm(period_ns);
}
/// `talaria.test=slice`: a 10 ms one-shot measured by the counter itself is a tautology on
/// this machine (the timer IS the counter); what is checked is that the period is time.
pub fn slice_selftest() -> bool {
    let want_ns: u64 = 10_000_000;
    let t0 = cpu::counter();
    timer::arm(want_ns);
    while cpu::counter() < timer::compare_value() { core::hint::spin_loop(); }
    timer::stop();
    let got_ns = (cpu::counter().wrapping_sub(t0) as u128 * 1_000_000_000 / cpu::counter_hz().max(1) as u128) as u64;
    let err = got_ns.abs_diff(want_ns);
    crate::println!("slice-test: {} ns were ordered, {} ns were counted (the error is {} %)", want_ns, got_ns, err * 100 / want_ns);
    let ok = err * 100 / want_ns <= 5;
    crate::println!("slice-test: the scheduler's tick goes once every {} us", effective_slice_ns() / 1000);
    crate::println!("slice-test: {}", if ok { "ALL OK" } else { "FAILED" });
    ok
}
/// A one-shot at the counter value `target` on the tick's PPI: the timer's compare value.
/// The IRQ vector tells a borrowed one-shot from the tick by `TIMER_ONESHOT`... it does not
/// need to: a sleeper wakes on ANY interrupt and re-checks its word, and `lapic_timer_restore`
/// puts the period back.
/// # Safety
/// W3 — the caller has borrowed the timer (`timer_borrow`) and will give it back.
pub unsafe fn deadline_arm(target: u64) {
    let now = cpu::counter();
    timer::arm_ticks(target.saturating_sub(now).max(1));
}
/// # Safety
/// As `deadline_arm`.
pub unsafe fn deadline_disarm() { timer::stop(); }

/// The GIC's pending and active bits for an INTID — what the x86 code asks the IRR/ISR.
pub fn irr_pending(vector: u8) -> bool { gic::is_pending(vector as u32) }
pub fn isr_pending(vector: u8) -> bool { gic::is_active(vector as u32) }
/// The LAPIC register window does not exist; the diagnostics that peek at it read zeros.
pub fn lapic_peek(_reg: u64) -> u32 { 0 }

/// `talaria.smp=N`: bring the secondaries up under the scheduler (mode 2).
pub fn start_from_cmdline(cmdline: &str) {
    let Some(n) = crate::cmdline::num(cmdline, "talaria.smp") else { return };
    let n = (n as usize).min(MAXCPU).min(nr_cpus());
    if n < 2 { return; }
    if crate::percpu::ONLINE.load(Ordering::SeqCst).count_ones() as usize >= n { return; }
    lapic_enable();
    AP_MODE.store(2, Ordering::SeqCst);
    for id in 1..n {
        if !start_cpu(id) { crate::println!("talaria: the AP{} did not come up (a -smp {} is needed)", id, n); break; }
    }
    crate::println!("talaria: the SMP scheduler: the online cores mask={:#x}", crate::percpu::ONLINE.load(Ordering::SeqCst));
}

// ── `talaria.test=wakeprobe`: what each way of waking a sleeping core costs ────────────────
//
// `wfe(monitor)` is the arm64 MWAIT: a sleeper arms the exclusive monitor on the futex word (or
// its core's doorbell) and waits in `wfe`; a waker stores, and where the policy wants the core
// rescheduled it sends an SGI to it, still asleep in `wfe`. `wfi+ipi` sleeps in `wfi` and is
// always woken by an SGI. The probe times all three on the first secondary, sleeping as a futex
// waiter does, after 30 us, 100 us, 300 us and 1 ms of sleep, four rounds each, and prints each
// way's per-length medians. Built for one question (docs/ENGINEERING-NOTES.md item 47): does a
// Graviton VM, where `examples/mdpipe`'s stage p99 is 34 us in the wfe tier and 6.8 us with
// `talaria.sync=hlt`, wake a `wfe` sleeper late? It does not — a store 140 ns, an SGI 2.3-2.5 us
// under `wfe` and under `wfi` alike, at every length — so the tail is the wfe tier's POLICY, not
// its wake. The tier is not chosen by this probe.
const WP_LEN: [u64; 4] = [30_000, 100_000, 300_000, 1_000_000];
const WP_REP: usize = 4;
const WP_WAYS: usize = 3;
const WP_N: usize = WP_WAYS * WP_LEN.len() * WP_REP;
const WP_WAY_NAME: [&str; WP_WAYS] = ["a store under wfe", "an SGI under wfe", "an SGI under wfi"];
/// The word the sleeper monitors, alone in its line: a store next to it (the kick flag, the round
/// counter) would clear the monitor and turn an SGI round into a store round.
#[repr(align(128))]
struct Line(AtomicU32);
static WP_WORD: Line = Line(AtomicU32::new(0));
static WP_KICK: Line = Line(AtomicU32::new(0));
static WP_READY: Line = Line(AtomicU32::new(0));
static WP_WOKE: [AtomicU64; WP_N] = [const { AtomicU64::new(0) }; WP_N];
fn wp_way(k: usize) -> usize { k / (WP_LEN.len() * WP_REP) }
fn wp_len(k: usize) -> usize { k % WP_LEN.len() }

/// `talaria.test=wakeprobe`: the first secondary up through the probe, the medians printed.
pub fn wake_probe() {
    lapic_enable();
    AP_HOOK.store(wfe_probe_ap as fn(usize) -> ! as usize as u64, Ordering::SeqCst);
    AP_MODE.store(3, Ordering::SeqCst);
    if start_cpu(1) { wfe_probe_judge(); crate::println!("wakeprobe: OK"); }
    else { crate::println!("wakeprobe: FAIL — the AP did not come up (-smp 2 is required)"); }
}

/// The secondary's half, then mode 2's own start (the SGIs, the syscall entry, the pickup loop).
fn wfe_probe_ap(me: usize) -> ! {
    unsafe {
        cpu::irq_disable();
        for sgi in 0..16u32 { gic::enable_local(sgi); }
    }
    for k in 0..WP_N {
        let want = k as u32 + 1;
        let w0 = WP_WORD.0.load(Ordering::Acquire);
        WP_READY.0.store(want, Ordering::Release);
        match wp_way(k) {
            // SAFETY (all three): W1/W3/W4 of sync.rs — a kernel word, on the core that sleeps, the
            // interrupt window exactly the wait instruction.
            0 => while WP_WORD.0.load(Ordering::Acquire) == w0 { unsafe { crate::sync::kwait_once(WP_WORD.0.as_ptr(), w0); } },
            1 => while WP_KICK.0.load(Ordering::Acquire) != want { unsafe { crate::sync::kwait_once(WP_WORD.0.as_ptr(), w0); } },
            _ => while WP_KICK.0.load(Ordering::Acquire) != want { unsafe { cpu::irq_enable_halt_disable(); } },
        }
        WP_WOKE[k].store(cpu::counter(), Ordering::Release);
    }
    crate::syscall::init_core();
    crate::sched::ap_loop(me)
}

/// The boot core's half. A sleeper not woken in 5 ms gets another SGI and the
/// round counts as 5 ms; 100 ms without the secondary's word ends the probe with the tier as it was.
fn wfe_probe_judge() {
    let hz = cpu::counter_hz().max(1);
    let ns = |t: u64| (t as u128 * 1_000_000_000 / hz as u128) as u64;
    let ticks = |n: u64| timer::ticks_for_ns(n);
    let ap = apic_of(1) as u64;
    let wake_sgi = crate::idt::WAKE_VECTOR as u32;
    let mut d = [0u64; WP_N];
    for k in 0..WP_N {
        let want = k as u32 + 1;
        let t0 = cpu::counter();
        while WP_READY.0.load(Ordering::Acquire) != want {
            if cpu::counter().wrapping_sub(t0) > ticks(100_000_000) { crate::println!("wakeprobe: the secondary did not arm round {}", k); return; }
            core::hint::spin_loop();
        }
        let s0 = cpu::counter();
        while cpu::counter().wrapping_sub(s0) < ticks(WP_LEN[wp_len(k)]) { core::hint::spin_loop(); }
        let t = cpu::counter();
        if wp_way(k) == 0 { WP_WORD.0.fetch_add(1, Ordering::Release); }
        else { WP_KICK.0.store(want, Ordering::Release); gic::send_sgi(ap, wake_sgi); }
        let mut kicked = false;
        loop {
            let w = WP_WOKE[k].load(Ordering::Acquire);
            if w != 0 { d[k] = ns(w.wrapping_sub(t)); break; }
            let el = cpu::counter().wrapping_sub(t);
            if !kicked && el > ticks(5_000_000) { kicked = true; WP_KICK.0.store(want, Ordering::Release); gic::send_sgi(ap, wake_sgi); }
            if el > ticks(100_000_000) { crate::println!("wakeprobe: round {} never woke", k); return; }
            core::hint::spin_loop();
        }
        if kicked { d[k] = d[k].max(5_000_000); }
    }
    let mut med = [[0u64; 4]; WP_WAYS];
    for way in 0..WP_WAYS {
        for l in 0..WP_LEN.len() {
            let mut v = [0u64; WP_REP];
            let mut j = 0;
            for k in 0..WP_N { if wp_way(k) == way && wp_len(k) == l { v[j] = d[k]; j += 1; } }
            v.sort_unstable();
            med[way][l] = v[WP_REP / 2];
        }
        crate::println!("wakeprobe: {}: the median wake after 30 us, 100 us, 300 us, 1 ms of sleep: {} {} {} {} ns",
            WP_WAY_NAME[way], med[way][0], med[way][1], med[way][2], med[way][3]);
    }
}
