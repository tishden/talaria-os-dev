//! The bring-up's probes: what the arm64 kernel can PROVE about the machine before the
//! shared modes come over (docs/ARM64-PLAN.md §9). Two kinds. The default sequence runs
//! on every boot and is what `kernel/test-aarch64.sh` asserts: EL0 with three `svc`
//! round trips and a controlled `brk`, the vDSO clock read from EL0. The named ones,
//! `talaria.test=ring|icache|bbm|svccost`, are the questions TCG cannot answer — it
//! keeps the caches coherent, executes on a TSO host and takes microseconds per
//! instruction — and are judged on the KVM stand:
//!
//! * `ring` — the SPSC ring of `fastpath.rs` between two cores, the exact fence set of
//!   §4.13, a checksum per slot: a corrupted slot means the fences are wrong;
//! * `icache` — code rewritten in a frame and run again, with and without
//!   `icache::sync` (§4.12): the synchronised run must answer anew, the other shows what
//!   a stale line looks like;
//! * `bbm` — a page split and flipped between RW and PROT_NONE while another core reads
//!   it through a guarded load (§4.3): a wrong value or a TLB conflict abort fails it;
//! * `svccost` — the price of an `svc` round trip, with and without the 528-byte
//!   FP/SIMD save in the vector (§4.5: the lazy-save decision needs both numbers).
//!
//! A probe that runs code at EL0 is a state machine: `eret` into EL0 never returns, so
//! the next step is an `extern "C" fn() -> !` the `brk` handler resumes the kernel at
//! (`trap::EL0_RETURN`), on the kernel stack the `eret` left.

use core::sync::atomic::{fence, AtomicU64, Ordering};
use super::{cpu, icache, timer, trap};

// ── a job for a secondary core ────────────────────────────────────────────────────
// The secondaries sit in `ap_idle_loop`; the boot core hands one a function and rings
// its doorbell (SGI 2). There is no scheduler yet: one job at a time, run to completion.

pub const MAX_AP: usize = 8;
static JOB: [AtomicU64; MAX_AP] = [const { AtomicU64::new(0) }; MAX_AP];
static JOB_DONE: [AtomicU64; MAX_AP] = [const { AtomicU64::new(0) }; MAX_AP];
/// The affinity of every core the tree named, by index (filled by `kmain`).
pub static CPU_MPIDR: [AtomicU64; MAX_AP] = [const { AtomicU64::new(u64::MAX) }; MAX_AP];

/// What a secondary core does after it came up: wait for a job, run it, wait again.
pub fn ap_idle_loop(me: usize) -> ! {
    let slot = me.min(MAX_AP - 1);
    loop {
        unsafe { cpu::halt(); }
        let j = JOB[slot].swap(0, Ordering::Acquire);
        if j != 0 {
            let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(j as usize) };
            f();
            JOB_DONE[slot].fetch_add(1, Ordering::Release);
        }
    }
}

/// Run `f` on core `cpu` (1..): the job is published, the core rung. `false` if the core
/// is not up or busy.
pub fn run_on(cpu: usize, f: fn()) -> bool {
    if cpu == 0 || cpu >= MAX_AP { return false; }
    let mpidr = CPU_MPIDR[cpu].load(Ordering::Relaxed);
    if mpidr == u64::MAX { return false; }
    if JOB[cpu].compare_exchange(0, f as usize as u64, Ordering::Release, Ordering::Relaxed).is_err() { return false; }
    super::gic::send_sgi(mpidr, 2);
    true
}

/// Wait until core `cpu` finished `n` more jobs than it had, or `ns` passed.
fn wait_done(cpu: usize, done_before: u64, n: u64, ns: u64) -> bool {
    let t0 = cpu::counter();
    let lim = timer::ticks_for_ns(ns);
    while JOB_DONE[cpu].load(Ordering::Acquire) < done_before + n {
        if cpu::counter().wrapping_sub(t0) > lim { return false; }
        core::hint::spin_loop();
    }
    true
}

fn ns_since(t0: u64) -> u64 {
    (cpu::counter().wrapping_sub(t0) as u128 * 1_000_000_000 / cpu::counter_hz().max(1) as u128) as u64
}

// ── the AArch64 encodings the probes assemble from ────────────────────────────────
const SVC0: u32 = 0xD400_0001;
/// `movz x<rd>, #imm16`
fn movz(rd: u32, imm16: u32) -> u32 { 0xD280_0000 | (imm16 & 0xFFFF) << 5 | (rd & 31) }
/// `movk x<rd>, #imm16, lsl #(16*hw)`
fn movk(rd: u32, imm16: u32, hw: u32) -> u32 { 0xF280_0000 | (hw & 3) << 21 | (imm16 & 0xFFFF) << 5 | (rd & 31) }
/// `cmp x0, #imm12` = `subs xzr, x0, #imm12`
fn cmp_imm(imm: u32) -> u32 { 0xF100_001F | (imm & 0xFFF) << 10 }
/// `cmn x0, #imm` — `adds xzr, x0, #imm`: the compare of x0 against a NEGATIVE small value.
fn cmn_imm(imm: u32) -> u32 { 0xB100_001F | ((imm & 0xFFF) << 10) }
/// `subs x<rd>, x<rd>, #imm12`
fn subs_imm(rd: u32, imm: u32) -> u32 { 0xF100_0000 | (imm & 0xFFF) << 10 | (rd & 31) << 5 | (rd & 31) }
/// `b.ne .+4*words` (a negative `words` branches back)
fn b_ne(words: i32) -> u32 { 0x5400_0001 | ((words as u32) & 0x7FFFF) << 5 }
/// `brk #imm16`
fn brk(imm16: u32) -> u32 { 0xD420_0000 | (imm16 & 0xFFFF) << 5 }

/// Write `code` into the frame `at` and make it fetchable (§4.12).
fn place(at: u64, code: &[u32], sync: bool) {
    unsafe { for (i, w) in code.iter().enumerate() { core::ptr::write_volatile((at as *mut u32).add(i), *w); } }
    if sync { icache::sync(at, 4 * code.len() as u64); }
}

/// Into EL0 at `pc` with the stack `sp`, DAIF masked; the `brk` handler resumes the kernel
/// at `done`. Never returns.
fn enter_el0(pc: u64, sp: u64, done: extern "C" fn() -> !) -> ! {
    trap::EL0_RETURN.store(done as *const () as usize as u64, Ordering::Relaxed);
    unsafe {
        core::arch::asm!("msr sp_el0, {sp}", "msr elr_el1, {pc}", "msr spsr_el1, {ps}", "eret",
            sp = in(reg) sp, pc = in(reg) pc, ps = in(reg) 0x3C0u64, options(noreturn));
    }
}

fn two_frames() -> (u64, u64) {
    let (Some(code), Some(stack)) = (crate::mem::alloc_frame(), crate::mem::alloc_frame()) else {
        crate::println!("talaria: probe: no frame"); finish()
    };
    (code, stack + 4096)
}

/// `talaria.poweroff=1` — the machine powers itself off once the sequence has finished
/// (PSCI SYSTEM_OFF, on which QEMU exits): a gate or a boot loop under KVM then takes
/// seconds per boot rather than the boot's whole timeout. The same knob, the same
/// meaning as on x86 (`cmdline.rs`), read once in `run`.
static POWEROFF: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn finish() -> ! {
    crate::println!("talaria: arm64 phases 1-3(a) stop here — the kernel above arch/ is not compiled for this target yet (docs/ARM64-PLAN.md §5)");
    if POWEROFF.load(core::sync::atomic::Ordering::Relaxed) {
        crate::println!("talaria: poweroff (PSCI SYSTEM_OFF)");
        super::psci::system_off();
    }
    cpu::halt_forever()
}

/// Is the command line asking for the arch bring-up or one of its probes (else the kernel
/// proper boots through `boot_shared`)?
pub fn is_arch_test(cmdline: &str) -> bool {
    matches!(crate::cmdline::test_name(cmdline), Some("bringup") | Some("ring") | Some("icache") | Some("bbm") | Some("svccost"))
}

/// The dispatch: a named probe, or the bring-up's default sequence (`talaria.test=bringup`).
pub fn run(cmdline: &str) -> ! {
    POWEROFF.store(crate::cmdline::flag(cmdline, "talaria.poweroff"), core::sync::atomic::Ordering::Relaxed);
    match crate::cmdline::test_name(cmdline) {
        Some("ring") => ring(crate::cmdline::num(cmdline, "talaria.ringn").unwrap_or(200_000)),
        Some("icache") => icache_probe(),
        Some("bbm") => bbm(),
        Some("svccost") => svccost(),
        _ => el0_probe(),
    }
}

// ── the default sequence: EL0 and the vDSO ────────────────────────────────────────

/// Phase 3's first step, proved with no ELF and no libc: a dozen instructions in a fresh
/// frame — three `svc`s through the REAL dispatcher (`syscall::rust_syscall`, the arm64
/// numbers decoded by `arch::abi`), then a `brk` — run at EL0 with a stack of their own.
/// `getpid` (172) and `gettid` (178) answer 1 for the first task; a number no table knows
/// (999) answers -ENOSYS (-38).
fn el0_probe() -> ! {
    let (code, sp) = two_frames();
    //   mov x8, #172; svc; cmp x0, #1;  b.ne wrong
    //   mov x8, #178; svc; cmp x0, #1;  b.ne wrong
    //   mov x8, #999; svc; cmn x0, #38; b.ne wrong
    //   brk #0 (the end)          wrong: brk #1
    let probe: [u32; 14] = [
        movz(8, 172), SVC0, cmp_imm(1), b_ne(13 - 3),
        movz(8, 178), SVC0, cmp_imm(1), b_ne(13 - 7),
        movz(8, 999), SVC0, cmn_imm(38), b_ne(13 - 11),
        brk(0), brk(1),
    ];
    place(code, &probe, true);
    crate::println!("talaria: el0 probe: code at {:#x}, stack at {:#x} — three svc round trips, then a brk", code, sp);
    enter_el0(code, sp, el0_probe_done)
}

extern "C" fn el0_probe_done() -> ! {
    let n = trap::SVCS.load(Ordering::Relaxed);
    crate::println!("talaria: el0 probe: back in EL1 — {} svc round trip(s), ended by brk #0 ({})", n, if n == 3 { "as expected" } else { "NOT the 3 expected" });
    vdso_probe()
}

/// The vDSO from EL0: `__kernel_clock_gettime(CLOCK_MONOTONIC, &ts)` through the image
/// mapped at `vdso::VADDR`, the result reported with `brk #2`. The kernel brackets the
/// call with two readings of its own clock: the vDSO's answer must lie between them.
fn vdso_probe() -> ! {
    let Some(off) = crate::vdso::symbol(b"__kernel_clock_gettime") else {
        crate::println!("talaria: vdso probe: no __kernel_clock_gettime in the image (an empty vDSO — no aarch64 cross compiler at build time?)");
        finish()
    };
    let entry = crate::vdso::VADDR + off;
    let (code, sp) = two_frames();
    //   sub sp, sp, #16; mov x0, #1; mov x1, sp; x9 = entry; blr x9; ldp x8, x9, [sp]; brk #2
    let probe: [u32; 11] = [
        0xD10043FF, movz(0, 1), 0x910003E1,
        movz(9, (entry & 0xFFFF) as u32), movk(9, ((entry >> 16) & 0xFFFF) as u32, 1),
        movk(9, ((entry >> 32) & 0xFFFF) as u32, 2), movk(9, ((entry >> 48) & 0xFFFF) as u32, 3),
        0xD63F0120, 0xA94027E8, brk(2), brk(1),
    ];
    place(code, &probe, true);
    T_BEFORE.store(crate::tsc::mono_ns(), Ordering::Relaxed);
    crate::println!("talaria: vdso probe: __kernel_clock_gettime at {:#x} (the image at {:#x}), calling it from EL0", entry, crate::vdso::VADDR);
    enter_el0(code, sp, vdso_probe_done)
}
static T_BEFORE: AtomicU64 = AtomicU64::new(0);

extern "C" fn vdso_probe_done() -> ! {
    let after = crate::tsc::mono_ns();
    let before = T_BEFORE.load(Ordering::Relaxed);
    let (sec, nsec) = (trap::PROBE_X8.load(Ordering::Relaxed), trap::PROBE_X9.load(Ordering::Relaxed));
    let got = sec.wrapping_mul(1_000_000_000).wrapping_add(nsec);
    crate::println!("talaria: vdso probe: EL0 read {}.{:09} s; the kernel's clock around the call: {} .. {} ns ({})",
        sec, nsec, before, after, if nsec < 1_000_000_000 && got >= before && got <= after { "as expected" } else { "NOT inside the bracket" });
    finish()
}

// ── ring: the SPSC protocol of fastpath.rs between two cores ──────────────────────
// The region has the layout of `fastring` (two rings); only the RX ring is used: the
// producer is core 1, the consumer the boot core, the indices are volatile words with the
// SAME fences `fastpath.rs` puts around them — this probe judges those fences.

#[repr(C, align(64))]
struct Region([u8; crate::fastring::REGION_SZ]);
static mut REGION: Region = Region([0; crate::fastring::REGION_SZ]);
static RING_N: AtomicU64 = AtomicU64::new(0);
static RING_PRODUCED: AtomicU64 = AtomicU64::new(0);
/// The payload: 32 words per slot, word `w` of slot `i` is `mix(i, w)`.
const RING_WORDS: usize = 32;
fn mix(i: u64, w: u64) -> u64 { (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ w.wrapping_mul(0xD1B5_4A32_D192_ED03)).rotate_left((w & 63) as u32) }

fn ring_base() -> usize { core::ptr::addr_of_mut!(REGION) as usize }
fn p_prod(base: usize) -> *mut u32 { (base + crate::fastring::RX_OFF) as *mut u32 }
fn p_cons(base: usize) -> *mut u32 { (base + crate::fastring::RX_OFF + crate::fastring::CONS_OFF) as *mut u32 }
fn p_slot(base: usize, i: u32) -> *mut u64 { (base + crate::fastring::RX_OFF + crate::fastring::slot_off(i) + crate::fastring::HDR) as *mut u64 }

fn ring_producer() {
    let base = ring_base();
    let n = RING_N.load(Ordering::Relaxed);
    let mut prod: u32 = 0;
    for i in 0..n {
        loop {
            let cons = unsafe { core::ptr::read_volatile(p_cons(base)) };
            if crate::fastring::can_produce(prod, cons) { break; }
            core::hint::spin_loop();
        }
        let s = p_slot(base, prod);
        for w in 0..RING_WORDS { unsafe { core::ptr::write_volatile(s.add(w), mix(i, w as u64)); } }
        unsafe { core::ptr::write_volatile((s as usize - crate::fastring::HDR) as *mut u32, (RING_WORDS * 8) as u32); }
        fence(Ordering::Release);                       // the bytes before the index — as fastpath.rs
        prod = prod.wrapping_add(1);
        unsafe { core::ptr::write_volatile(p_prod(base), prod); }
        RING_PRODUCED.store(i + 1, Ordering::Relaxed);
    }
}

fn ring(n: u64) -> ! {
    let base = ring_base();
    unsafe { core::ptr::write_volatile(p_prod(base), 0); core::ptr::write_volatile(p_cons(base), 0); }
    RING_N.store(n, Ordering::Relaxed);
    crate::println!("talaria: ring probe: {} slots of {} words, core 1 produces, core 0 consumes (the fastpath fences)", n, RING_WORDS);
    let done0 = JOB_DONE[1].load(Ordering::Acquire);
    if !run_on(1, ring_producer) { crate::println!("talaria: ring probe: no core 1 to produce on — needs -smp 2 (NOT run)"); finish() }
    let t0 = cpu::counter();
    let mut cons: u32 = 0;
    let mut corrupt = 0u64;
    let mut bad_len = 0u64;
    let mut i: u64 = 0;
    let limit = timer::ticks_for_ns(600_000_000_000);
    while i < n {
        let prod = unsafe { core::ptr::read_volatile(p_prod(base)) };
        if !crate::fastring::can_consume(prod, cons) {
            if cpu::counter().wrapping_sub(t0) > limit { crate::println!("talaria: ring probe: the producer stalled at {} of {} (NOT finished)", RING_PRODUCED.load(Ordering::Relaxed), n); break; }
            core::hint::spin_loop(); continue;
        }
        fence(Ordering::Acquire);                       // the index before the bytes — as fastpath.rs
        let s = p_slot(base, cons);
        let len = unsafe { core::ptr::read_volatile((s as usize - crate::fastring::HDR) as *const u32) };
        if len as usize != RING_WORDS * 8 { bad_len += 1; }
        let mut wrong = false;
        for w in 0..RING_WORDS { if unsafe { core::ptr::read_volatile(s.add(w)) } != mix(i, w as u64) { wrong = true; } }
        if wrong { corrupt += 1; }
        fence(Ordering::Release);                       // the reads before the index — the §4.13 fix
        cons = cons.wrapping_add(1);
        unsafe { core::ptr::write_volatile(p_cons(base), cons); }
        i += 1;
    }
    let ns = ns_since(t0).max(1);
    wait_done(1, done0, 1, 5_000_000_000);
    crate::println!("talaria: ring probe: {} slots in {} ms — {} corrupted, {} with a wrong length, {} slots/s ({})",
        i, ns / 1_000_000, corrupt, bad_len, i * 1_000_000_000 / ns,
        if corrupt == 0 && bad_len == 0 && i == n { "as expected" } else { "NOT as expected" });
    finish()
}

// ── icache: code rewritten under the instruction cache ─────────────────────────────
static IC_CODE: AtomicU64 = AtomicU64::new(0);
static IC_SP: AtomicU64 = AtomicU64::new(0);
static IC_STEP: AtomicU64 = AtomicU64::new(0);

fn ic_probe(answer: u32) -> [u32; 2] { [movz(8, answer), brk(2)] }

fn icache_probe() -> ! {
    let (code, sp) = two_frames();
    IC_CODE.store(code, Ordering::Relaxed); IC_SP.store(sp, Ordering::Relaxed);
    crate::println!("talaria: icache probe: a frame at {:#x} — run A, rewrite to B with icache::sync, rewrite to C without", code);
    place(code, &ic_probe(0xA), true);
    IC_STEP.store(1, Ordering::Relaxed);
    enter_el0(code, sp, icache_step)
}

extern "C" fn icache_step() -> ! {
    let (code, sp) = (IC_CODE.load(Ordering::Relaxed), IC_SP.load(Ordering::Relaxed));
    let got = trap::PROBE_X8.load(Ordering::Relaxed);
    match IC_STEP.fetch_add(1, Ordering::Relaxed) {
        1 => {
            crate::println!("talaria: icache probe: A answered {:#x} ({})", got, if got == 0xA { "as expected" } else { "NOT as expected" });
            place(code, &ic_probe(0xB), true);
            enter_el0(code, sp, icache_step)
        }
        2 => {
            crate::println!("talaria: icache probe: B (rewritten WITH sync) answered {:#x} ({})", got, if got == 0xB { "as expected" } else { "NOT as expected — a stale line after icache::sync" });
            place(code, &ic_probe(0xC), false);
            enter_el0(code, sp, icache_step)
        }
        _ => {
            // Not a verdict: a stale line is what the architecture PERMITS here, not what it
            // promises — the observation says how the failure the sync prevents looks.
            crate::println!("talaria: icache probe: C (rewritten WITHOUT sync) answered {:#x} — {}", got,
                if got == 0xC { "coherent this time (TCG always; a core may or may not)" } else { "a STALE line executed: this is what a missing icache::sync looks like" });
            finish()
        }
    }
}

// ── bbm: a page flipped under a neighbour's reads ─────────────────────────────────
const BBM_MAGIC: u64 = 0xB4EA_C0DE_0000_0001;
static BBM_ADDR: AtomicU64 = AtomicU64::new(0);
static BBM_STOP: AtomicU64 = AtomicU64::new(0);
static BBM_READS: AtomicU64 = AtomicU64::new(0);
static BBM_WRONG: AtomicU64 = AtomicU64::new(0);
static BBM_FAULTS: AtomicU64 = AtomicU64::new(0);

fn bbm_reader() {
    let mut reads = 0u64; let mut wrong = 0u64; let mut faults = 0u64;
    while BBM_STOP.load(Ordering::Acquire) == 0 {
        let a = BBM_ADDR.load(Ordering::Acquire);
        if a == 0 { core::hint::spin_loop(); continue; }
        match trap::guarded_load(a) {
            None => faults += 1,
            Some(v) if v == BBM_MAGIC => {}
            Some(_) => wrong += 1,
        }
        reads += 1;
    }
    BBM_READS.store(reads, Ordering::Relaxed); BBM_WRONG.store(wrong, Ordering::Relaxed); BBM_FAULTS.store(faults, Ordering::Relaxed);
}

fn bbm() -> ! {
    const REGIONS: u64 = 48;   // 2 MiB each: the split (a block -> a table) happens once per region
    const FLIPS: u64 = 2_000;  // RW <-> PROT_NONE per region
    crate::println!("talaria: bbm probe: {} regions of 2 MiB, {} rights flips each, core 1 reads through a guarded load", REGIONS, FLIPS);
    let done0 = JOB_DONE[1].load(Ordering::Acquire);
    if !run_on(1, bbm_reader) { crate::println!("talaria: bbm probe: no core 1 to read on — needs -smp 2 (NOT run)"); finish() }
    let t0 = cpu::counter();
    let mut regions = 0u64;
    for _ in 0..REGIONS {
        let Some(r) = crate::mem::alloc_contig_aligned(512, 0x20_0000) else { break; };
        regions += 1;
        let a = r + 0x1000 * 7;  // a page inside the block, not its first
        unsafe { core::ptr::write_volatile(a as *mut u64, BBM_MAGIC); }
        BBM_ADDR.store(a, Ordering::Release);
        for _ in 0..FLIPS {
            crate::paging::protect(a, 4096, true, false);   // the first call splits the 2M leaf
            crate::paging::protect_none(a, 4096);
        }
        crate::paging::protect(a, 4096, true, false);
    }
    BBM_STOP.store(1, Ordering::Release);
    let finished = wait_done(1, done0, 1, 5_000_000_000);
    let conflicts = trap::CONFLICTS[1].load(Ordering::Relaxed);
    let (reads, wrong, faults) = (BBM_READS.load(Ordering::Relaxed), BBM_WRONG.load(Ordering::Relaxed), BBM_FAULTS.load(Ordering::Relaxed));
    crate::println!("talaria: bbm probe: {} regions, {} ms; the reader saw {} reads, {} faults (PROT_NONE phases), {} WRONG values, {} TLB conflict aborts ({})",
        regions, ns_since(t0) / 1_000_000, reads, faults, wrong, conflicts,
        if finished && wrong == 0 && conflicts == 0 && regions == REGIONS { "as expected" } else { "NOT as expected" });
    finish()
}

// ── svccost: the price of an svc round trip, with and without the FP/SIMD save ────
const SVC_N: u32 = 50_000;
static SVC_T0: AtomicU64 = AtomicU64::new(0);
static SVC_STEP: AtomicU64 = AtomicU64::new(0);
static SVC_CODE: AtomicU64 = AtomicU64::new(0);
static SVC_SP: AtomicU64 = AtomicU64::new(0);
static SVC_NS_WITH: AtomicU64 = AtomicU64::new(0);

fn svc_loop() -> [u32; 5] {
    //   mov x9, #N; 1: svc #0; subs x9, x9, #1; b.ne 1b; brk #2
    [movz(9, SVC_N), SVC0, subs_imm(9, 1), b_ne(-2), brk(2)]
}

fn svccost() -> ! {
    let (code, sp) = two_frames();
    SVC_CODE.store(code, Ordering::Relaxed); SVC_SP.store(sp, Ordering::Relaxed);
    place(code, &svc_loop(), true);
    crate::println!("talaria: svccost probe: {} svc round trips from EL0, first WITH the 528-byte FP/SIMD save in the vector, then without", SVC_N);
    trap::FPSAVE_ON.store(1, Ordering::Relaxed);
    SVC_STEP.store(1, Ordering::Relaxed);
    SVC_T0.store(cpu::counter(), Ordering::Relaxed);
    enter_el0(code, sp, svccost_step)
}

extern "C" fn svccost_step() -> ! {
    let ns = ns_since(SVC_T0.load(Ordering::Relaxed));
    let per = ns / SVC_N as u64;
    match SVC_STEP.fetch_add(1, Ordering::Relaxed) {
        1 => {
            SVC_NS_WITH.store(per, Ordering::Relaxed);
            crate::println!("talaria: svccost probe: WITH the FP/SIMD save: {} ns per svc round trip", per);
            trap::FPSAVE_ON.store(0, Ordering::Relaxed);
            SVC_T0.store(cpu::counter(), Ordering::Relaxed);
            enter_el0(SVC_CODE.load(Ordering::Relaxed), SVC_SP.load(Ordering::Relaxed), svccost_step)
        }
        _ => {
            trap::FPSAVE_ON.store(1, Ordering::Relaxed);
            let with = SVC_NS_WITH.load(Ordering::Relaxed);
            crate::println!("talaria: svccost probe: WITHOUT the FP/SIMD save: {} ns per svc round trip — the save costs {} ns ({})",
                per, with.saturating_sub(per), if per > 0 { "numbers taken" } else { "NOT measured" });
            finish()
        }
    }
}
