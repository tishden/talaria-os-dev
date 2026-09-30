//! The PMU on a bare core through MSRs + rdpmc (without `perf_event_open` — there is none).
//!
//! We program the architectural counters (Intel SDM vol.3 §20). The observability of
//! performance is what this OS exists for, so more counters are needed
//! than "cycles and instructions", and they are needed ON EVERY CORE.
//!
//! ## What lies where
//!
//! | counter | what it counts | what the operator needs it for |
//! |---|---|---|
//! | FIXED0 | instructions retired | together with FIXED1 it gives the IPC |
//! | FIXED1 | core cycles (non-halted) | "the core is busy" ≠ "the core works" |
//! | FIXED2 | the TSC's reference cycles | turbo/throttling: FIXED1/FIXED2 |
//! | PMC0 | LLC misses | how many times we went into memory |
//! | PMC1 | LLC references | without a denominator the share of misses is not computed |
//! | PMC2 | branches retired | the denominator for the predictor |
//! | PMC3 | predictor misses | a branch that costs ~15 cycles |
//!
//! THE FIXED COUNTERS INSTEAD OF TWO GP ONES are not cosmetics. The cycles and the
//! instructions used to occupy PMC0/PMC1, and two GP ones were left for the cache counters, and for
//! the predictor — none: a machine with two GP counters (and under KVM that is
//! a common thing) could show neither the cache nor the branches. Fixed
//! counters exist in any arch-perfmon v2+, and they free up ALL the GP ones.
//!
//! ## Why on every core
//!
//! The counters' MSRs are PER-CORE. The former `init()` programmed only the
//! core on which it was called (the BSP, the control plane), and `rdpmc` on
//! a secondary core returned zero. That is, the counters kept silent exactly where the
//! workload lives. `init_cpu()` is called at the bring-up of every AP.
//!
//! The vPMU under KVM may be switched off — then version==0 or the counters stand still; the API
//! gives out `available()=false`, the caller falls back to the TSC.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

const IA32_PERFEVTSEL0: u32 = 0x186;
const IA32_PMC0: u32 = 0xC1;
const IA32_FIXED_CTR0: u32 = 0x309;
const IA32_FIXED_CTR_CTRL: u32 = 0x38D;
const IA32_PERF_GLOBAL_CTRL: u32 = 0x38F;

// PERFEVTSEL: event | (umask<<8) | USR(16) | OS(17) | EN(22)
const EN: u64 = (1 << 16) | (1 << 17) | (1 << 22); // USR+OS+EN
const EV_LLC_MISS: u64 = 0x2E | (0x41 << 8);       // LLC Misses (arch. event 5)
const EV_LLC_REF: u64 = 0x2E | (0x4F << 8);        // LLC References (arch. event 4)
const EV_BR_RET: u64 = 0xC4;                       // Branch Instructions Retired (arch. 6)
const EV_BR_MISS: u64 = 0xC5;                      // Branch Misses Retired (arch. 7)
const EV_CYCLES: u64 = 0x3C;                       // UnHalted Core Cycles (arch. 1)
const EV_INSTRS: u64 = 0xC0;                       // Instructions Retired (arch. 2)

/// The fixed counters are addressed by `rdpmc` with bit 30.
const FIXED: u32 = 1 << 30;
const F_INSTR: u32 = FIXED;
const F_CYCLES: u32 = FIXED | 1;
const F_REF: u32 = FIXED | 2;

unsafe fn wrmsr(msr: u32, val: u64) {
    core::arch::asm!("wrmsr", in("ecx") msr, in("eax") val as u32, in("edx") (val >> 32) as u32,
        options(nostack, preserves_flags));
}
#[inline]
unsafe fn rdpmc_raw(idx: u32) -> u64 {
    let (lo, hi): (u32, u32);
    core::arch::asm!("rdpmc", in("ecx") idx, out("eax") lo, out("edx") hi, options(nostack, nomem));
    (hi as u64) << 32 | lo as u64
}
/// The arch-perfmon version, the number of GP counters, the number of fixed ones (CPUID.0AH).
fn perfmon_info() -> (u32, u32, u32) {
    let r = core::arch::x86_64::__cpuid(0x0A);
    (r.eax & 0xFF, (r.eax >> 8) & 0xFF, r.edx & 0x1F)
}

static AVAILABLE: AtomicBool = AtomicBool::new(false);
static NGP: AtomicU32 = AtomicU32::new(0);
static NFIXED: AtomicU32 = AtomicU32::new(0);
/// The cores on which the counters are really programmed (`perf` will show which).
static ARMED: AtomicU64 = AtomicU64::new(0);

// ── WHERE WHICH COUNTER LANDED ───────────────────────────────────────────────
//
// The layout is not a constant but THE RESULT OF A CHECK. The reason is concrete: KVM
// declares three fixed counters and version 2 through CPUID.0AH, while `rdpmc`
// on them returns ZERO — they are not emulated. The declaration cannot be trusted:
// the system silently loses the IPC (the very first quantity that is asked for), and
// from the outside that looks like "the workload executes no instructions".
//
// So `init_cpu` CHECKS the declaration against the deed: it sets the counters up, spins
// a short loop and looks at whether they have moved. They have not — the cycles and the
// instructions move over onto the general-purpose counters, and whatever there were not enough GP ones
// for is honestly declared absent (`NONE`).
const NONE: u32 = u32::MAX;
static I_CYC: AtomicU32 = AtomicU32::new(NONE);
static I_INS: AtomicU32 = AtomicU32::new(NONE);
static I_REF: AtomicU32 = AtomicU32::new(NONE);
static I_LLCM: AtomicU32 = AtomicU32::new(NONE);
static I_LLCR: AtomicU32 = AtomicU32::new(NONE);
static I_BRR: AtomicU32 = AtomicU32::new(NONE);
static I_BRM: AtomicU32 = AtomicU32::new(NONE);
/// How the counters landed: 0 — not up, 1 — the cycles/instructions are on the fixed ones,
/// 2 — everything is on GP ones (the fixed ones do not count).
static LAYOUT: AtomicU32 = AtomicU32::new(0);

#[inline]
fn get(i: &AtomicU32) -> Option<u32> { let v = i.load(Ordering::Relaxed); if v == NONE { None } else { Some(v) } }
#[inline]
unsafe fn rd(i: &AtomicU32) -> u64 { match get(i) { Some(x) => rdpmc_raw(x), None => 0 } }

/// It programs the GP counters with a list of events and enables them in the GLOBAL_CTRL.
unsafe fn arm_gp(events: &[u64]) -> u64 {
    let mut mask = 0u64;
    for (i, ev) in events.iter().enumerate() {
        wrmsr(IA32_PERFEVTSEL0 + i as u32, ev | EN);
        wrmsr(IA32_PMC0 + i as u32, 0);
        mask |= 1 << i;
    }
    mask
}
/// Whether the counter really counts: a short loop and a comparison of before/after.
/// Fifty microseconds once per core is the price of not lying to the operator.
unsafe fn counts(idx: u32) -> bool {
    let a = rdpmc_raw(idx);
    for _ in 0..20_000 { core::hint::spin_loop(); }
    rdpmc_raw(idx) != a
}

/// Program the counters ON THE CURRENT CORE. It returns false if there is no PMU.
pub fn init_cpu() -> bool {
    let (version, ngp, nfix) = perfmon_info();
    if version == 0 {
        AVAILABLE.store(false, Ordering::SeqCst);
        return false;
    }
    NGP.store(ngp, Ordering::SeqCst);
    NFIXED.store(nfix, Ordering::SeqCst);
    unsafe {
        // CR4.PCE (bit 8): rdpmc from ring3 — the dataplane reads the counters without a syscall.
        let mut cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        cr4 |= 1 << 8;
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));

        wrmsr(IA32_PERF_GLOBAL_CTRL, 0); // stop everything for the time of the reprogramming
        // Attempt 1: the cycles and the instructions go onto the fixed ones, all the GP ones are free for
        // the cache and the predictor. That is the best layout, if the fixed ones work.
        let ng = ngp.min(4) as usize;
        let mut mask = arm_gp(&[EV_LLC_MISS, EV_LLC_REF, EV_BR_RET, EV_BR_MISS][..ng]);
        let mut ctrl = 0u64;
        for i in 0..(nfix.min(3) as usize) {
            ctrl |= 0b11 << (i * 4);
            wrmsr(IA32_FIXED_CTR0 + i as u32, 0);
            mask |= 1 << (32 + i);
        }
        wrmsr(IA32_FIXED_CTR_CTRL, ctrl);
        wrmsr(IA32_PERF_GLOBAL_CTRL, mask);

        // TWO ATTEMPTS, not one. An observation: under KVM the first check right
        // after the bring-up steadily says "the fixed ones do not count", while
        // a repeated programming of the very same MSRs already says "they do count". The vPMU
        // is switched on by the hypervisor lazily, at the first access to its own
        // registers, and the first interval is lost. Without the repeat a machine where
        // the fixed counters ARE THERE would forever have gone over to the fallback
        // layout and lost the predictor's counters.
        let mut fixed_ok = nfix >= 2 && counts(F_CYCLES) && counts(F_INSTR);
        if !fixed_ok && nfix >= 2 {
            wrmsr(IA32_PERF_GLOBAL_CTRL, 0);
            for i in 0..(nfix.min(3) as usize) { wrmsr(IA32_FIXED_CTR0 + i as u32, 0); }
            wrmsr(IA32_FIXED_CTR_CTRL, ctrl);
            wrmsr(IA32_PERF_GLOBAL_CTRL, mask);
            fixed_ok = counts(F_CYCLES) && counts(F_INSTR);
        }
        if fixed_ok {
            I_CYC.store(F_CYCLES, Ordering::SeqCst);
            I_INS.store(F_INSTR, Ordering::SeqCst);
            I_REF.store(if nfix >= 3 { F_REF } else { NONE }, Ordering::SeqCst);
            I_LLCM.store(if ng >= 1 { 0 } else { NONE }, Ordering::SeqCst);
            I_LLCR.store(if ng >= 2 { 1 } else { NONE }, Ordering::SeqCst);
            I_BRR.store(if ng >= 3 { 2 } else { NONE }, Ordering::SeqCst);
            I_BRM.store(if ng >= 4 { 3 } else { NONE }, Ordering::SeqCst);
            LAYOUT.store(1, Ordering::SeqCst);
        } else {
            // The fixed ones are declared but do not count (a typical vPMU under KVM):
            // everything goes onto GP ones. First come the cycles and the instructions: without the IPC the rest is not
            // interpretable, while cache misses without the IPC are just a number.
            //
            // WHAT WE SACRIFICE WHEN THERE ARE FOUR GP ONES. There are six events, four slots, and
            // the choice is not arbitrary: we take the NUMERATORS (LLC misses, the predictor's
            // misses) and give up the DENOMINATORS (LLC references,
            // branches in total). The denominator becomes the instructions, and the metric becomes
            // misses per thousand instructions (MPKI): it is comparable between
            // runs and between machines, whereas "the share of misses" without the number
            // of references is not computed at all. The reverse trade — to lose the very fact of
            // the misses for the sake of a pretty share — would be worse.
            wrmsr(IA32_PERF_GLOBAL_CTRL, 0);
            wrmsr(IA32_FIXED_CTR_CTRL, 0);
            let n = ngp as usize;
            let (evs, map): (&[u64], [u32; 5]) = if n >= 6 {
                // llc_miss, llc_ref, br_ret, br_miss
                (&[EV_CYCLES, EV_INSTRS, EV_LLC_MISS, EV_LLC_REF, EV_BR_RET, EV_BR_MISS], [2, 3, 4, 5, 6])
            } else if n >= 4 {
                (&[EV_CYCLES, EV_INSTRS, EV_LLC_MISS, EV_BR_MISS], [2, NONE, NONE, 3, 4])
            } else {
                (&[EV_CYCLES, EV_INSTRS], [NONE, NONE, NONE, NONE, 2])
            };
            let used = evs.len().min(n);
            let mask = arm_gp(&evs[..used]);
            wrmsr(IA32_PERF_GLOBAL_CTRL, mask);
            I_CYC.store(if used >= 1 { 0 } else { NONE }, Ordering::SeqCst);
            I_INS.store(if used >= 2 { 1 } else { NONE }, Ordering::SeqCst);
            I_REF.store(NONE, Ordering::SeqCst);
            I_LLCM.store(map[0], Ordering::SeqCst);
            I_LLCR.store(map[1], Ordering::SeqCst);
            I_BRR.store(map[2], Ordering::SeqCst);
            I_BRM.store(map[3], Ordering::SeqCst);
            LAYOUT.store(2, Ordering::SeqCst);
        }
    }
    let cpu = crate::percpu::cpu_id();
    if cpu < 64 { ARMED.fetch_or(1 << cpu, Ordering::SeqCst); }
    AVAILABLE.store(true, Ordering::SeqCst);
    true
}

/// Bring the PMU up on THIS core and announce it (it is called from the BSP at the boot).
pub fn init() -> bool {
    let (version, ngp, nfix) = perfmon_info();
    if !init_cpu() {
        crate::println!("talaria: the PMU is unavailable (arch-perfmon version={}) — a fallback to the TSC", version);
        return false;
    }
    let lay = LAYOUT.load(Ordering::SeqCst);
    crate::println!("talaria: the PMU is on (arch-perfmon v{}, {} GP + {} fixed); the layout: {}",
        version, ngp, nfix,
        if lay == 1 { "the cycles/instructions are the fixed ones, the GP ones are the cache and the predictor" }
        else { "the fixed ones ARE DECLARED BUT DO NOT COUNT (typical for a vPMU under KVM) — everything is on GP ones" });
    crate::println!("talaria: the PMU: cycles={} instructions={} LLC-miss={} LLC-ref={} branches={} predictor-miss={} ({} — there is no counter)",
        show(&I_CYC), show(&I_INS), show(&I_LLCM), show(&I_LLCR), show(&I_BRR), show(&I_BRM), "-");
    true
}
/// The counter's index for the log: `f1` — the fixed one no. 1, `2` — the GP one no. 2, `-` — there is none.
fn show(i: &AtomicU32) -> crate::obs::S<8> {
    use core::fmt::Write;
    let mut s = crate::obs::S::new();
    match get(i) {
        None => { let _ = write!(s, "-"); }
        Some(v) if v & FIXED != 0 => { let _ = write!(s, "f{}", v & !FIXED); }
        Some(v) => { let _ = write!(s, "{}", v); }
    }
    s
}

pub fn available() -> bool { AVAILABLE.load(Ordering::Relaxed) }
/// The mask of the cores on which the counters are programmed.
pub fn armed_mask() -> u64 { ARMED.load(Ordering::Relaxed) }
pub fn ngp() -> u32 { NGP.load(Ordering::Relaxed) }
pub fn nfixed() -> u32 { NFIXED.load(Ordering::Relaxed) }

/// The instantaneous (cycles, instrs) of the CURRENT core. (0,0) — there are no counters.
pub fn read() -> (u64, u64) {
    if !available() { return (0, 0); }
    unsafe { (rd(&I_CYC), rd(&I_INS)) }
}
/// The cache counters (LLC-miss, LLC-ref) of the current core.
pub fn read_cache() -> (u64, u64) {
    if !available() { return (0, 0); }
    unsafe { (rd(&I_LLCM), rd(&I_LLCR)) }
}
/// The branch predictor (misses, branches in total) of the current core.
pub fn read_branch() -> (u64, u64) {
    if !available() { return (0, 0); }
    unsafe { (rd(&I_BRM), rd(&I_BRR)) }
}

pub use crate::perfsample::Sample;

/// A full snapshot of the CURRENT core's counters.
pub fn sample() -> Sample {
    if !available() { return Sample { tsc: crate::tsc::rdtsc(), ..Sample::ZERO }; }
    unsafe {
        Sample {
            cycles: rd(&I_CYC), instrs: rd(&I_INS), ref_cycles: rd(&I_REF),
            llc_miss: rd(&I_LLCM), llc_ref: rd(&I_LLCR),
            br_ret: rd(&I_BRR), br_miss: rd(&I_BRM),
            tsc: crate::tsc::rdtsc(),
        }
    }
}
// ── THE SNAPSHOTS PER CORE ───────────────────────────────────────────────────
//
// `rdpmc` reads the counters of ITS OWN core only — that is a property of the hardware, not our
// decision. So, for `top`/`perf` to show the core the workload sits on,
// the code that takes the snapshot is bound to execute EXACTLY THERE. There are exactly two such moments:
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
/// A snapshot is seven `rdpmc`s, and under a hypervisor EACH of them is an exit to the host:
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
    if !available() { return; }
    let c = crate::percpu::cpu_id();
    if c >= MAXCPU { return; }
    let s = sample();
    S_CYC[c].store(s.cycles, Ordering::Relaxed);
    S_INS[c].store(s.instrs, Ordering::Relaxed);
    S_REF[c].store(s.ref_cycles, Ordering::Relaxed);
    S_LLCM[c].store(s.llc_miss, Ordering::Relaxed);
    S_LLCR[c].store(s.llc_ref, Ordering::Relaxed);
    S_BRM[c].store(s.br_miss, Ordering::Relaxed);
    S_BRR[c].store(s.br_ret, Ordering::Relaxed);
    S_TSC[c].store(s.tsc, Ordering::Relaxed);
    S_SEQ[c].fetch_add(1, Ordering::Release);
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
    if !available() { return; }
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
    let seq = S_SEQ[c].load(Ordering::Acquire);
    (Sample {
        cycles: S_CYC[c].load(Ordering::Relaxed),
        instrs: S_INS[c].load(Ordering::Relaxed),
        ref_cycles: S_REF[c].load(Ordering::Relaxed),
        llc_miss: S_LLCM[c].load(Ordering::Relaxed),
        llc_ref: S_LLCR[c].load(Ordering::Relaxed),
        br_miss: S_BRM[c].load(Ordering::Relaxed),
        br_ret: S_BRR[c].load(Ordering::Relaxed),
        tsc: S_TSC[c].load(Ordering::Relaxed),
    }, seq)
}

/// Ask the core `c` to take its counters RIGHT NOW (one IPI). true —
/// the snapshot has arrived. It INTERRUPTS the workload on that core: it is called only by an explicit
/// command of the operator.
pub fn poke(c: usize) -> bool {
    if !available() || c >= MAXCPU { return false; }
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
        if S_SEQ[c].load(Ordering::Acquire) != before { return true; }
        core::hint::spin_loop();
    }
    WANT.fetch_and(!(1u64 << c), Ordering::AcqRel);
    false
}
