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
    while crate::percpu::ONLINE.load(Ordering::SeqCst) & (1 << l) == 0 {
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
    if p != 0 { p } else { trap::TICK_NS.load(Ordering::Relaxed) }
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
