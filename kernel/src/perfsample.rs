//! One snapshot of a core's performance counters and the arithmetic over it — the IPC, the
//! miss rates per thousand instructions — shared by `obs::perf`, `top` and `kstat`. The
//! counters themselves are the machine's (`arch::pmu`: the x86 PMU through MSRs and `rdpmc`,
//! PMUv3 on arm64 through its system registers); the numbers a person reads are not, and
//! neither is where the snapshots of every core are kept (below).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A full snapshot of the CURRENT core's counters.
#[derive(Clone, Copy, Default)]
pub struct Sample {
    pub cycles: u64,
    pub instrs: u64,
    pub ref_cycles: u64,
    pub llc_miss: u64,
    pub llc_ref: u64,
    pub br_miss: u64,
    pub br_ret: u64,
    pub tsc: u64,
}
impl Sample {
    /// A zero snapshot — the initial value of the accumulators (`Default` in the `const` context
    /// of an array will not do).
    pub const ZERO: Sample = Sample { cycles: 0, instrs: 0, ref_cycles: 0, llc_miss: 0, llc_ref: 0, br_miss: 0, br_ret: 0, tsc: 0 };

    /// The difference of two snapshots of one and the same core.
    pub fn delta(&self, prev: &Sample) -> Sample {
        Sample {
            cycles: self.cycles.wrapping_sub(prev.cycles),
            instrs: self.instrs.wrapping_sub(prev.instrs),
            ref_cycles: self.ref_cycles.wrapping_sub(prev.ref_cycles),
            llc_miss: self.llc_miss.wrapping_sub(prev.llc_miss),
            llc_ref: self.llc_ref.wrapping_sub(prev.llc_ref),
            br_miss: self.br_miss.wrapping_sub(prev.br_miss),
            br_ret: self.br_ret.wrapping_sub(prev.br_ret),
            tsc: self.tsc.wrapping_sub(prev.tsc),
        }
    }
    /// Whether a DIFFERENCE of two snapshots of one core is one a core can produce: no more
    /// than 16 cycles per tick of the counter (the TSC, or the generic timer at 1 GHz+ — no core
    /// runs sixteen times faster), no more than 8 instructions per cycle (the widest retire of
    /// either machine). A counter that went back between the two shows here as a huge value.
    pub fn plausible(&self) -> bool {
        (self.tsc == 0 || self.cycles <= self.tsc.saturating_mul(16)) && self.instrs <= self.cycles.saturating_mul(8).max(1_000)
    }
    /// The IPC in hundredths (231 = 2.31). 0 — there is nothing to compute it from.
    pub fn ipc100(&self) -> u64 { if self.cycles > 0 { self.instrs * 100 / self.cycles } else { 0 } }
    /// The share of LLC misses in hundredths of a percent.
    pub fn llc_miss_pct100(&self) -> u64 { if self.llc_ref > 0 { self.llc_miss * 10_000 / self.llc_ref } else { 0 } }
    /// The share of the predictor's misses in hundredths of a percent.
    pub fn br_miss_pct100(&self) -> u64 { if self.br_ret > 0 { self.br_miss * 10_000 / self.br_ret } else { 0 } }
    /// Whether there are denominators for the shares (LLC references / branches in total).
    pub fn has_llc_ratio(&self) -> bool { self.llc_ref > 0 }
    pub fn has_br_ratio(&self) -> bool { self.br_ret > 0 }
    /// LLC misses per THOUSAND INSTRUCTIONS, in hundredths (1234 = 12.34 MPKI).
    /// The denominator is the instructions: any machine counts them, and the quantity is comparable
    /// between runs, whereas "the share of misses" requires a counter of references,
    /// for which there may be no slot.
    pub fn llc_mpki100(&self) -> u64 { if self.instrs > 0 { self.llc_miss * 100_000 / self.instrs } else { 0 } }
    /// The predictor's misses per thousand instructions, in hundredths.
    pub fn br_mpki100(&self) -> u64 { if self.instrs > 0 { self.br_miss * 100_000 / self.instrs } else { 0 } }
}

// ── THE SNAPSHOTS PER CORE ───────────────────────────────────────────────────
//
// A core's counters are read on THAT core only (`rdpmc` on x86, the PMU's system registers
// on arm64) — that is a property of the hardware, not our decision. So, for `top`/`perf` to
// show the core the workload sits on, the code that takes the snapshot is bound to execute
// EXACTLY THERE. There are exactly two such moments:
//
//   1. The scheduler's TICK on that core — for free (seven `rdpmc`s per slice,
//      that is fractions of a per mille of a cycle). That is how all the cores of the common pool are collected.
//   2. An EXPLICIT REQUEST of the operator (`perf poke <core>`) — one IPI. It is needed for a
//      DEDICATED core: there is no tick there at all (that is the point of a dedicated core),
//      and there exist no other ways to read its counters.
//
// The second path INTERRUPTS a latency-critical thread, so it is not automatic:
// the operator asks for it himself, knowing the price. To "peep" into a dedicated core
// automatically is exactly the jitter for the elimination of which the core was dedicated.
const MAXCPU: usize = crate::percpu::MAXCPU;
macro_rules! percpu_u64 { ($n:ident) => { static $n: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU]; } }
percpu_u64!(S_CYC); percpu_u64!(S_INS); percpu_u64!(S_REF);
percpu_u64!(S_LLCM); percpu_u64!(S_LLCR); percpu_u64!(S_BRM); percpu_u64!(S_BRR);
percpu_u64!(S_TSC); percpu_u64!(S_SEQ);
/// The cores for which an out-of-turn snapshot has been requested (see `poke`).
static WANT: AtomicU64 = AtomicU64::new(0);
/// Whether to take the counters in the scheduler's tick (`talaria.pmutick=<ms>`; 0 — do not take them).
static TICK_ON: AtomicBool = AtomicBool::new(true);
/// The STEP of taking the counters, in nanoseconds.
///
/// A snapshot is seven `rdpmc`s, and under a hypervisor EACH of them is an exit to the host (on
/// arm64 under KVM every PMU register access traps the same way):
/// 22.5 us per snapshot was measured (the tick's phases in `stat:`). On every tick at a slice of
/// 0.5 ms that is 4.5 % of the processor — as much as our whole gap with Linux on
/// computation. Meanwhile the observer needs not "the tick's rate" but a freshness of the order of
/// tens of milliseconds: `top` and `kstat` a human reads with his eyes. So the step
/// is given in TIME and by default is 20 ms — 0.1 % instead of 4.5 % at the same
/// observability. `perf poke` takes the counters out of turn and does not ask about the step.
static TICK_IV_NS: AtomicU64 = AtomicU64::new(20_000_000);

pub fn tick_enabled() -> bool { TICK_ON.load(Ordering::Relaxed) }
pub fn configure_from_cmdline(cmdline: &str) {
    let Some(v) = crate::cmdline::value(cmdline, "talaria.pmutick") else { return };
    let Ok(ms) = v.parse::<u64>() else { return };
    if ms == 0 {
        TICK_ON.store(false, Ordering::SeqCst);
        crate::println!("talaria: the PMU: the taking of the counters in the tick is SWITCHED OFF (talaria.pmutick=0)");
    } else {
        TICK_IV_NS.store(ms * 1_000_000, Ordering::SeqCst);
        crate::println!("talaria: the PMU: the counters are taken once every {} ms", ms);
    }
}

/// Take the CURRENT core's counters into its slot. It is called from the tick and from the IPI handler.
#[inline]
pub fn note_here() {
    if !crate::pmu::available() { return; }
    let c = crate::percpu::cpu_id();
    if c >= MAXCPU { return; }
    // A seqlock writer, with this core's interrupts masked: `top` takes its own core's
    // snapshot from the console's thread, and the tick on that core used to land in the
    // middle of it — the slot then held one moment's cycles and another's instructions, and
    // a reader on another core read the fields while they were being written. Both made a
    // core's cycles go BACK between two reads; the 64-bit wrap of that difference, summed
    // with a neighbour's, printed an IPC of 6.8e7 on a Graviton (`docs/ENGINEERING-NOTES.md` 44).
    let f = crate::arch::cpu::irq_flags();
    unsafe { crate::arch::cpu::irq_disable(); }
    S_SEQ[c].fetch_add(1, Ordering::Relaxed);              // odd: being written
    core::sync::atomic::fence(Ordering::Release);
    let s = crate::pmu::sample();
    S_CYC[c].store(s.cycles, Ordering::Relaxed);
    S_INS[c].store(s.instrs, Ordering::Relaxed);
    S_REF[c].store(s.ref_cycles, Ordering::Relaxed);
    S_LLCM[c].store(s.llc_miss, Ordering::Relaxed);
    S_LLCR[c].store(s.llc_ref, Ordering::Relaxed);
    S_BRM[c].store(s.br_miss, Ordering::Relaxed);
    S_BRR[c].store(s.br_ret, Ordering::Relaxed);
    S_TSC[c].store(s.tsc, Ordering::Relaxed);
    S_SEQ[c].fetch_add(1, Ordering::Release);              // even: whole
    unsafe { crate::arch::cpu::irq_restore(f); }
}
/// The same from the scheduler's tick — no more often than once per `TICK_IV_NS` per core.
/// The timestamp is taken from THE SNAPSHOT ITSELF (`S_TSC`), so no separate
/// state is needed for the step, and `perf poke` shifts it too.
#[inline]
pub fn note_tick() {
    if !tick_enabled() { return; }
    let c = crate::percpu::cpu_id();
    if c >= MAXCPU { return; }
    let iv = TICK_IV_NS.load(Ordering::Relaxed);
    let hz = crate::tsc::calibrate_cached();
    if iv != 0 && hz != 0 {
        let need = (hz / 1_000_000).saturating_mul(iv / 1000);
        if crate::tsc::rdtsc().wrapping_sub(S_TSC[c].load(Ordering::Relaxed)) < need { return; }
    }
    note_here();
}
/// The IPI handler asks: were we not the ones asked to take the counters?
///
/// First the READ, and only then the RMW. This handler is the common entry for all the
/// IPIs, including membarrier, which on a multithreaded workload comes often;
/// an unconditional `fetch_and` would add an atomic write into a shared cache line on
/// every such IPI for the sake of a flag that is almost always clear.
#[inline]
pub fn ipi_check() {
    if WANT.load(Ordering::Relaxed) == 0 { return; }
    let c = crate::percpu::cpu_id();
    if c < 64 && WANT.fetch_and(!(1u64 << c), Ordering::AcqRel) & (1 << c) != 0 { note_here(); }
}

/// TAKE THE COUNTERS OFF THE CORES WHERE THE WORKLOAD IS RUNNING.
///
/// The snapshot is taken by the TICK (`note_tick`), and there may be no tick on a core at all:
/// a dedicated core works on the run-to-completion principle, and that is exactly the
/// core for whose sake the counters are read. Hence a measurement that broke down for
/// its own reason: in a workload pinned to the cores 0-1 the snapshots came from the cores
/// 2-3, which at that time were IDLING — and in the log there stood "IPC 0.06", that is
/// the characteristic of an idle spin passed off as the characteristic of the workload.
///
/// So before printing, the counters are asked for from every online core whose snapshot has
/// gone stale (or which had none): one IPI per core, and only on the printing path —
/// on the hot path this function does not exist.
pub fn refresh_all() {
    if !crate::pmu::available() { return; }
    note_here(); // our own core — without an IPI
    let me = crate::percpu::cpu_id();
    let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
    let hz = crate::tsc::calibrate_cached().max(1);
    let now = crate::tsc::rdtsc();
    for c in 0..MAXCPU {
        if c == me || online & (1u64 << c) == 0 { continue; }
        let (s, seq) = last(c);
        // "Fresh" means not older than 50 ms: over that time the workload does not manage
        // to change, while an extra IPI into a working core is not free.
        if seq != 0 && now.wrapping_sub(s.tsc) < hz / 20 { continue; }
        poke(c);
    }
}

/// The last snapshot of the core `c` and its sequence number (0 — there were no snapshots).
pub fn last(c: usize) -> (Sample, u64) {
    if c >= MAXCPU { return (Sample::default(), 0); }
    // The seqlock's reader (the writer is `note_here`): a slot read while its core writes it
    // is read again. A few tries; a core that keeps writing (it cannot — one snapshot per
    // 20 ms) would give the last read, as before.
    let mut out = (Sample::default(), 0);
    for _ in 0..4 {
        let seq = S_SEQ[c].load(Ordering::Acquire);
        if seq & 1 != 0 { core::hint::spin_loop(); continue; }
        out = (read_slot(c), seq);
        core::sync::atomic::fence(Ordering::Acquire);
        if S_SEQ[c].load(Ordering::Relaxed) == seq { break; }
    }
    out
}
fn read_slot(c: usize) -> Sample {
    Sample {
        cycles: S_CYC[c].load(Ordering::Relaxed),
        instrs: S_INS[c].load(Ordering::Relaxed),
        ref_cycles: S_REF[c].load(Ordering::Relaxed),
        llc_miss: S_LLCM[c].load(Ordering::Relaxed),
        llc_ref: S_LLCR[c].load(Ordering::Relaxed),
        br_miss: S_BRM[c].load(Ordering::Relaxed),
        br_ret: S_BRR[c].load(Ordering::Relaxed),
        tsc: S_TSC[c].load(Ordering::Relaxed),
    }
}

/// Ask the core `c` to take its counters RIGHT NOW (one IPI). true —
/// the snapshot has arrived. It INTERRUPTS the workload on that core: it is called only by an explicit
/// command of the operator.
pub fn poke(c: usize) -> bool {
    if !crate::pmu::available() || c >= MAXCPU { return false; }
    if c == crate::percpu::cpu_id() { note_here(); return true; }
    let before = S_SEQ[c].load(Ordering::Acquire);
    WANT.fetch_or(1u64 << c, Ordering::AcqRel);
    let apic = crate::smp::apic_of(c);
    if apic == u32::MAX { WANT.fetch_and(!(1u64 << c), Ordering::AcqRel); return false; }
    crate::smp::send_ipi(apic, crate::idt::IPI_VECTOR as u8);
    // We wait no longer than a millisecond: the core may be switched off or sitting with IF=0.
    let hz = crate::tsc::calibrate_cached().max(1);
    let deadline = crate::tsc::rdtsc() + hz / 1000;
    while crate::tsc::rdtsc() < deadline {
        let now = S_SEQ[c].load(Ordering::Acquire);
        if now != before && now & 1 == 0 { return true; }   // a new snapshot, and a whole one
        core::hint::spin_loop();
    }
    WANT.fetch_and(!(1u64 << c), Ordering::AcqRel);
    false
}
