//! The performance counters on arm64: PMUv3 through its system registers (Arm ARM D11,
//! "The Performance Monitors Extension"). The surface is x86's (`arch::x86_64::pmu`): the
//! cycles, the instructions, the last-level cache, the branch predictor, on EVERY core, with
//! the snapshots per core kept by the shared `perfsample` — the numbers a person reads are
//! the same quantities on both machines.
//!
//! ## What lies where
//!
//! | counter | event | what the operator needs it for |
//! |---|---|---|
//! | `PMCCNTR_EL0` | the cycles (always 64 bits, `PMCR.LC`) | the IPC's denominator |
//! | an event counter | `INST_RETIRED` (0x08) | the IPC's numerator |
//! | an event counter | `LL_CACHE_MISS_RD` (0x37), else `L2D_CACHE_REFILL` (0x17) | how many times we went into memory |
//! | an event counter | `BR_MIS_PRED_RETIRED` (0x22), else `BR_MIS_PRED` (0x10) | a branch that costs a pipeline |
//! | an event counter | `LL_CACHE_RD` (0x36), else `L2D_CACHE` (0x16) | the misses' denominator |
//! | an event counter | `BR_RETIRED` (0x21), else `BR_PRED` (0x12) | the predictor's denominator |
//!
//! The order is the priority: x86's reasoning holds — with few counters the NUMERATORS are
//! kept and the rates become per thousand instructions (MPKI). There is no reference-cycles
//! counter on arm64: the generic timer is the reference and `Sample::tsc` carries it.
//!
//! ## The width: the one real difference from x86
//!
//! An event counter is 32 bits before PMUv3p5 (`PMCR.LP`): at 3 G instructions a second it
//! wraps in 1.4 s, and `Sample::delta`'s 64-bit subtraction over two snapshots a few seconds
//! apart would lie by multiples of 2^32. Three ways, the first the machine allows:
//!
//! 1. **PMUv3p5**: `PMCR.LP` makes every event counter 64 bits. One counter per event.
//! 2. **CHAIN** (event 0x1E, declared in `PMCEID0`): the odd counter of a pair counts the
//!    even one's overflows — a 64-bit counter out of two. Half the events fit.
//! 3. **Widened by the reader**: the 32-bit value, with the wraps counted by the core that
//!    reads it (`HI`/`LO` below). Correct as long as the core reads its counters once per
//!    wrap — the tick's snapshot does, every 20 ms; a DEDICATED core unread for longer than a
//!    wrap loses the wraps it slept through. QEMU's TCG models no CHAIN; that is where this
//!    path runs. The boot line names the width, so the operator knows which one it is.
//!
//! ## Declared versus counting
//!
//! As on x86 the declaration is checked against the deed: `PMCEID0/1` says which common
//! events the core implements, and after programming every counter is read twice around a
//! short loop — one that did not move is dropped (`NONE`), not reported as a zero. Under
//! TCG `INST_RETIRED` is implemented only with `-icount`; without it the IPC is honestly
//! absent.
//!
//! EL0 may READ the counters (`PMUSERENR_EL0.CR|ER`, x86's `CR4.PCE`): the data plane takes
//! its IPC without a syscall. It may not write them — `EN` is left clear, since with it EL0
//! could stop the counters every core's observer reads.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

pub use crate::perfsample::Sample;
pub use crate::perfsample::{configure_from_cmdline, note_here, note_tick, ipi_check,
    refresh_all, last, poke};

// The common events (Arm ARM D11.11).
const EV_INST_RETIRED: u32 = 0x08;
const EV_BR_MIS_PRED: u32 = 0x10;
const EV_BR_PRED: u32 = 0x12;
const EV_L2D_CACHE: u32 = 0x16;
const EV_L2D_CACHE_REFILL: u32 = 0x17;
const EV_CHAIN: u32 = 0x1E;
const EV_BR_RETIRED: u32 = 0x21;
const EV_BR_MIS_PRED_RETIRED: u32 = 0x22;
const EV_LL_CACHE_RD: u32 = 0x36;
const EV_LL_CACHE_MISS_RD: u32 = 0x37;

// PMCR_EL0
const PMCR_E: u64 = 1 << 0;
const PMCR_P: u64 = 1 << 1;
const PMCR_C: u64 = 1 << 2;
const PMCR_LC: u64 = 1 << 6;
const PMCR_LP: u64 = 1 << 7;
/// The cycle counter's bit in PMCNTENSET/CLR.
const CCNT: u64 = 1 << 31;

/// The highest event counter this module addresses (the pairs of five chained events).
const MAXCTR: u32 = 12;

/// The logical events, in priority order, and where each landed.
const L_INS: usize = 0;
const L_LLCM: usize = 1;
const L_BRM: usize = 2;
const L_LLCR: usize = 3;
const L_BRR: usize = 4;
const NL: usize = 5;
const NONE: u32 = u32::MAX;
/// The event counter (the even one of a pair when chained) of each logical event.
static IDX: [AtomicU32; NL] = [const { AtomicU32::new(NONE) }; NL];
/// The event number each logical event got (after the fallbacks), for the boot line.
static EVT: [AtomicU32; NL] = [const { AtomicU32::new(0) }; NL];
/// 1 — 64-bit counters (PMUv3p5), 2 — chained pairs, 3 — 32 bits widened by the reader.
static WIDTH: AtomicU32 = AtomicU32::new(0);
static AVAILABLE: AtomicBool = AtomicBool::new(false);
static NCTR: AtomicU32 = AtomicU32::new(0);
static VERSION: AtomicU32 = AtomicU32::new(0);
static ARMED: AtomicU64 = AtomicU64::new(0);
/// `talaria.pmunarrow=1`: the reader's widening even where the machine has wider counters —
/// the path a core without CHAIN takes, made testable under QEMU (whose `max` is PMUv3p5).
/// The counters then start 64 K events below the wrap, so the first second crosses it.
pub static NARROW: AtomicBool = AtomicBool::new(false);
const NARROW_START: u64 = 0xFFFF_0000;

const MAXCPU: usize = crate::percpu::MAXCPU;
/// The reader's widening (`WIDTH` 3): the last 32-bit value read and the wraps above it,
/// per core and logical event. Touched by the owning core only, with interrupts masked.
static LO: [[AtomicU32; NL]; MAXCPU] = [const { [const { AtomicU32::new(0) }; NL] }; MAXCPU];
static HI: [[AtomicU64; NL]; MAXCPU] = [const { [const { AtomicU64::new(0) }; NL] }; MAXCPU];

macro_rules! sysreg_rw {
    ($rd:ident, $wr:ident, $name:literal) => {
        #[inline(always)]
        fn $rd() -> u64 { let v: u64; unsafe { core::arch::asm!(concat!("mrs {}, ", $name), out(reg) v, options(nomem, nostack)); } v }
        #[inline(always)]
        unsafe fn $wr(v: u64) { core::arch::asm!(concat!("msr ", $name, ", {}"), in(reg) v, options(nomem, nostack)); }
    };
}
sysreg_rw!(pmcr_r, pmcr_w, "pmcr_el0");
sysreg_rw!(pmccntr_r, pmccntr_w, "pmccntr_el0");
sysreg_rw!(pmccfiltr_r, pmccfiltr_w, "pmccfiltr_el0");
sysreg_rw!(pmcntenset_r, pmcntenset_w, "pmcntenset_el0");
sysreg_rw!(pmcntenclr_r, pmcntenclr_w, "pmcntenclr_el0");
sysreg_rw!(pmintenclr_r, pmintenclr_w, "pmintenclr_el1");
sysreg_rw!(pmovsclr_r, pmovsclr_w, "pmovsclr_el0");
sysreg_rw!(pmuserenr_r, pmuserenr_w, "pmuserenr_el0");
fn pmceid0() -> u64 { let v: u64; unsafe { core::arch::asm!("mrs {}, pmceid0_el0", out(reg) v, options(nomem, nostack)); } v }
fn pmceid1() -> u64 { let v: u64; unsafe { core::arch::asm!("mrs {}, pmceid1_el0", out(reg) v, options(nomem, nostack)); } v }
fn id_aa64dfr0() -> u64 { let v: u64; unsafe { core::arch::asm!("mrs {}, id_aa64dfr0_el1", out(reg) v, options(nomem, nostack)); } v }

/// The event counter `n` and its type register: the registers are named by number, so each
/// index is its own instruction (the indirect `PMSELR` + `PMXEVCNTR` would be two accesses,
/// two traps under KVM).
macro_rules! by_index {
    ($n:expr, $op:ident) => {
        match $n {
            0 => $op!("0"), 1 => $op!("1"), 2 => $op!("2"), 3 => $op!("3"),
            4 => $op!("4"), 5 => $op!("5"), 6 => $op!("6"), 7 => $op!("7"),
            8 => $op!("8"), 9 => $op!("9"), 10 => $op!("10"), 11 => $op!("11"),
            _ => $op!(none),
        }
    };
}
#[inline(always)]
fn evcnt_r(n: u32) -> u64 {
    macro_rules! rd {
        (none) => { 0 };
        ($i:literal) => {{ let v: u64; unsafe { core::arch::asm!(concat!("mrs {}, pmevcntr", $i, "_el0"), out(reg) v, options(nomem, nostack)); } v }};
    }
    by_index!(n, rd)
}
unsafe fn evcnt_w(n: u32, v: u64) {
    macro_rules! wr {
        (none) => { () };
        ($i:literal) => { core::arch::asm!(concat!("msr pmevcntr", $i, "_el0, {}"), in(reg) v, options(nomem, nostack)) };
    }
    by_index!(n, wr)
}
unsafe fn evtype_w(n: u32, v: u64) {
    macro_rules! wr {
        (none) => { () };
        ($i:literal) => { core::arch::asm!(concat!("msr pmevtyper", $i, "_el0, {}"), in(reg) v, options(nomem, nostack)) };
    }
    by_index!(n, wr)
}

/// PMUv3 is there: `ID_AA64DFR0_EL1.PMUVer` names a version (0 — none, 0xF — an
/// IMPLEMENTATION DEFINED one this module does not speak).
fn pmu_version() -> u32 {
    let v = ((id_aa64dfr0() >> 8) & 0xF) as u32;
    if v == 0xF { 0 } else { v }
}
/// Whether the core implements the common event `ev` (`PMCEID0` 0x00..0x1F, `PMCEID1` 0x20..0x3F).
fn has_event(ev: u32) -> bool {
    match ev {
        0..=31 => pmceid0() >> ev & 1 != 0,
        32..=63 => pmceid1() >> (ev - 32) & 1 != 0,
        _ => false,
    }
}
fn pick(first: u32, second: u32) -> u32 {
    if has_event(first) { first } else if has_event(second) { second } else { NONE }
}

/// The raw value of the logical event `l` as the width makes it: 64 bits either way.
#[inline(always)]
fn read_logical(l: usize) -> u64 {
    let i = IDX[l].load(Ordering::Relaxed);
    if i == NONE { return 0; }
    match WIDTH.load(Ordering::Relaxed) {
        1 => evcnt_r(i),
        2 => {
            // The pair: the high half, the low, the high again — a carry between the two
            // reads would pair a fresh low with a stale high (or the reverse).
            let mut hi = evcnt_r(i + 1) & 0xFFFF_FFFF;
            let mut lo = evcnt_r(i) & 0xFFFF_FFFF;
            let hi2 = evcnt_r(i + 1) & 0xFFFF_FFFF;
            if hi2 != hi { lo = evcnt_r(i) & 0xFFFF_FFFF; hi = hi2; }
            hi << 32 | lo
        }
        _ => {
            let c = super::cpu::cpu_index().min(MAXCPU - 1);
            let f = super::cpu::irq_flags();
            unsafe { super::cpu::irq_disable(); }
            let v = (evcnt_r(i) & 0xFFFF_FFFF) as u32;
            if v < LO[c][l].load(Ordering::Relaxed) { HI[c][l].fetch_add(1 << 32, Ordering::Relaxed); }
            LO[c][l].store(v, Ordering::Relaxed);
            let r = HI[c][l].load(Ordering::Relaxed) | v as u64;
            unsafe { super::cpu::irq_restore(f); }
            r
        }
    }
}

/// Whether the counter of the logical event `l` (or the cycle counter, `None`) moves over a
/// short loop. Twice, as on x86: a vPMU may come alive at the first access.
fn counts(l: Option<usize>) -> bool {
    let rd = || match l { Some(l) => read_logical(l), None => pmccntr_r() };
    for _ in 0..2 {
        let a = rd();
        let mut x = 0u64;
        for k in 0..20_000u64 { x = core::hint::black_box(x.wrapping_add(k)); }
        if rd() != a { return true; }
    }
    false
}

/// Program the counters ON THE CURRENT CORE. It returns false if there is no PMU.
pub fn init_cpu() -> bool {
    let ver = pmu_version();
    VERSION.store(ver, Ordering::Relaxed);
    if ver == 0 { return false; }
    let n = ((pmcr_r() >> 11) & 0x1F) as u32;
    NCTR.store(n, Ordering::Relaxed);
    let usable = n.min(MAXCTR);
    // The width (the module doc): PMUv3p5 is PMUVer >= 6.
    let width = if NARROW.load(Ordering::Relaxed) { 3 } else if ver >= 6 { 1 } else if has_event(EV_CHAIN) { 2 } else { 3 };
    let per = if width == 2 { 2 } else { 1 };
    let want = [
        pick(EV_INST_RETIRED, NONE),
        pick(EV_LL_CACHE_MISS_RD, EV_L2D_CACHE_REFILL),
        pick(EV_BR_MIS_PRED_RETIRED, EV_BR_MIS_PRED),
        pick(EV_LL_CACHE_RD, EV_L2D_CACHE),
        pick(EV_BR_RETIRED, EV_BR_PRED),
    ];
    unsafe {
        pmcntenclr_w(u32::MAX as u64);   // everything stopped while it is reprogrammed
        pmintenclr_w(u32::MAX as u64);   // no overflow interrupts: the counters are read, not sampled
        pmovsclr_w(u32::MAX as u64);
        pmccfiltr_w(0);                  // the cycles at EL0 and EL1
        let mut enable = CCNT;
        let mut next = 0u32;
        for (l, &ev) in want.iter().enumerate() {
            IDX[l].store(NONE, Ordering::Relaxed);
            EVT[l].store(ev, Ordering::Relaxed);
            if ev == NONE || next + per > usable { continue; }
            evtype_w(next, ev as u64);   // the filter bits zero: EL0 and EL1
            evcnt_w(next, 0);
            if per == 2 {
                evtype_w(next + 1, EV_CHAIN as u64);
                evcnt_w(next + 1, 0);
            }
            enable |= ((1u64 << per) - 1) << next;
            IDX[l].store(next, Ordering::Relaxed);
            next += per;
        }
        let c = super::cpu::cpu_index().min(MAXCPU - 1);
        for l in 0..NL { LO[c][l].store(0, Ordering::Relaxed); HI[c][l].store(0, Ordering::Relaxed); }
        WIDTH.store(width, Ordering::Relaxed);
        let lp = if width == 1 { PMCR_LP } else { 0 };
        pmcr_w(PMCR_E | PMCR_P | PMCR_C | PMCR_LC | lp); // P zeroes the event counters
        if width == 3 && NARROW.load(Ordering::Relaxed) {
            for l in 0..NL {
                let i = IDX[l].load(Ordering::Relaxed);
                if i != NONE { evcnt_w(i, NARROW_START); LO[c][l].store(NARROW_START as u32, Ordering::Relaxed); }
            }
        }
        pmcntenset_w(enable);
        super::cpu::isb();
        pmuserenr_w((1 << 2) | (1 << 3)); // CR | ER: EL0 reads the cycle and event counters
    }
    if !counts(None) { return false; }
    for l in 0..NL {
        if IDX[l].load(Ordering::Relaxed) != NONE && !counts(Some(l)) { IDX[l].store(NONE, Ordering::Relaxed); }
    }
    let c = super::cpu::cpu_index();
    if c < 64 { ARMED.fetch_or(1 << c, Ordering::SeqCst); }
    AVAILABLE.store(true, Ordering::SeqCst);
    true
}

/// Bring the PMU up on THIS core and announce it (the boot core, at the boot).
pub fn init() -> bool {
    if !init_cpu() {
        let v = VERSION.load(Ordering::Relaxed);
        if v == 0 { crate::println!("talaria: the PMU is unavailable (no PMUv3 on this core) — a fallback to the counter"); }
        else { crate::println!("talaria: the PMU is unavailable (PMUv3 declared, the cycle counter does not move) — a fallback to the counter"); }
        return false;
    }
    let v = VERSION.load(Ordering::Relaxed);
    crate::println!("talaria: the PMU is on (PMUv3{}, {} event counters + the cycle counter); the width: {}",
        match v { 1 => "", 4 => "p1", 5 => "p4", 6 => "p5", 7 => "p7", 8 => "p8", _ => "p?" },
        NCTR.load(Ordering::Relaxed),
        match WIDTH.load(Ordering::Relaxed) {
            1 => "64-bit counters (PMUv3p5)",
            2 => "chained pairs (CHAIN)",
            _ => "32-bit, widened by the reader (no CHAIN: a core unread for a wrap loses it)",
        });
    crate::println!("talaria: the PMU: cycles=ccnt instructions={} LLC-miss={} LLC-ref={} branches={} predictor-miss={} (- — there is no counter)",
        show(L_INS), show(L_LLCM), show(L_LLCR), show(L_BRR), show(L_BRM));
    true
}
/// "the counter's index:the event" for the log; `-` — none.
fn show(l: usize) -> crate::obs::S<16> {
    use core::fmt::Write;
    let mut s = crate::obs::S::new();
    match IDX[l].load(Ordering::Relaxed) {
        NONE => { let _ = write!(s, "-"); }
        i => { let _ = write!(s, "{}:{:#x}", i, EVT[l].load(Ordering::Relaxed)); }
    }
    s
}

pub fn available() -> bool { AVAILABLE.load(Ordering::Relaxed) }
pub fn armed_mask() -> u64 { ARMED.load(Ordering::Relaxed) }
/// The event counters the core has (x86's "GP"); arm64 has no fixed ones but the cycles.
pub fn ngp() -> u32 { NCTR.load(Ordering::Relaxed) }
pub fn nfixed() -> u32 { if available() { 1 } else { 0 } }

/// The instantaneous (cycles, instrs) of the CURRENT core. (0,0) — there are no counters.
pub fn read() -> (u64, u64) {
    if !available() { return (0, 0); }
    (pmccntr_r(), read_logical(L_INS))
}
/// The cache counters (LLC-miss, LLC-ref) of the current core.
pub fn read_cache() -> (u64, u64) {
    if !available() { return (0, 0); }
    (read_logical(L_LLCM), read_logical(L_LLCR))
}
/// The branch predictor (misses, branches in total) of the current core.
pub fn read_branch() -> (u64, u64) {
    if !available() { return (0, 0); }
    (read_logical(L_BRM), read_logical(L_BRR))
}

/// A full snapshot of the CURRENT core's counters.
pub fn sample() -> Sample {
    if !available() { return Sample { tsc: super::cpu::counter(), ..Sample::ZERO }; }
    Sample {
        cycles: pmccntr_r(), instrs: read_logical(L_INS), ref_cycles: 0,
        llc_miss: read_logical(L_LLCM), llc_ref: read_logical(L_LLCR),
        br_ret: read_logical(L_BRR), br_miss: read_logical(L_BRM),
        tsc: super::cpu::counter(),
    }
}

/// `talaria.test=pmu` with `talaria.pmunarrow=1`: the reader's widening ACROSS a wrap. The
/// instructions counter is set 4096 events below 2^32, a loop of a million iterations runs,
/// and the widened difference must be the loop's size — a widening that missed the wrap gives
/// a difference near 2^64. `None` — the instructions counter is not the narrow one here.
pub fn selftest_wrap() -> Option<(u64, u64)> {
    let i = IDX[L_INS].load(Ordering::Relaxed);
    if !available() || i == NONE || WIDTH.load(Ordering::Relaxed) != 3 { return None; }
    let c = super::cpu::cpu_index().min(MAXCPU - 1);
    let f = super::cpu::irq_flags();
    unsafe { super::cpu::irq_disable(); }
    let start = (1u64 << 32) - 4096;
    unsafe { evcnt_w(i, start); }
    LO[c][L_INS].store(start as u32, Ordering::Relaxed);
    let (c0, i0) = (pmccntr_r(), read_logical(L_INS));
    let mut x = 0u64;
    for k in 0..1_000_000u64 { x = core::hint::black_box(x.wrapping_add(k)); }
    let (c1, i1) = (pmccntr_r(), read_logical(L_INS));
    unsafe { super::cpu::irq_restore(f); }
    Some((i1.wrapping_sub(i0), c1.wrapping_sub(c0)))
}
