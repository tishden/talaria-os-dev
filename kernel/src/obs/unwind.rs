//! Stack unwinding along the chain of frame pointers (RBP).
//!
//! ## Why RBP rather than DWARF
//!
//! The right answer to "who called" is the unwind tables (`.eh_frame`), and they
//! work even without frame pointers. But DWARF would have to be parsed FROM THE TIMER
//! TICK, i.e. from the body of an interrupt handler, where one may neither allocate
//! memory, nor go to the disk, nor take milliseconds. The RBP chain is read with
//! two loads per frame and fits into tens of nanoseconds — enough
//! to take it at every sample.
//!
//! The price of the decision is named honestly: **a stack is visible only for code built with
//! frame pointers**. For a workload that is `-fno-omit-frame-pointer`, for the
//! kernel `-C force-frame-pointers=yes` (enabled, see `kernel/.cargo/config.toml`).
//! For a foreign binary without them the unwinding stops at the very first frame, and that
//! is visible in the profile as depth 1 — rather than as an invented stack.
//!
//! ## Why the read must be fault-tolerant
//!
//! RBP in code without frame pointers is an ordinary general-purpose register: anything
//! may be in it. Checking the page before the read is not enough: a neighbouring
//! core may remove the mapping between the check and the load (ClickHouse calls
//! `munmap` constantly), and then a #PF arrives INSIDE THE TIMER HANDLER — i.e.
//! the kernel dies because of diagnostics. So the read is performed with A RECOVERY
//! POINT: the return address is put into a per-CPU slot before the load, and the
//! #PF handler (`idt::rust_exception`), seeing the slot filled, returns
//! control right after the load and marks the attempt as failed.

const MAXCPU: usize = crate::percpu::MAXCPU;

/// The stack depth one sample holds (the leaf + seven callers).
pub const MAXDEPTH: usize = 8;

/// The return address if the read fails (0 — no read is in progress). A per-core slot:
/// the value belongs to one core and lives for microseconds, there is nothing to share.
static mut FIXUP: [u64; MAXCPU] = [0; MAXCPU];
/// The "the read failed" flag — set by the #PF handler.
static mut HIT: [u64; MAXCPU] = [0; MAXCPU];
/// How many times a read failed over the whole boot: this value must be small.
/// A large one means the frame chain is garbage and the profile cannot be trusted.
static FAULTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[inline]
fn slot(base: *mut u64, cpu: usize) -> *mut u64 { unsafe { base.add(cpu) } }
#[inline]
fn fixup_slot(cpu: usize) -> *mut u64 { slot(core::ptr::addr_of_mut!(FIXUP) as *mut u64, cpu) }
#[inline]
fn hit_slot(cpu: usize) -> *mut u64 { slot(core::ptr::addr_of_mut!(HIT) as *mut u64, cpu) }

/// How many reads failed during unwinding (the `perf` diagnostics).
pub fn faults() -> u64 { FAULTS.load(core::sync::atomic::Ordering::Relaxed) }

/// The #PF handler asks: was this failure ours? It returns the address
/// control must return to (and clears the slot so that the point does not fire
/// twice).
pub fn fault_fixup() -> Option<u64> {
    let cpu = crate::percpu::cpu_id();
    if cpu >= MAXCPU { return None; }
    let f = unsafe { core::ptr::read_volatile(fixup_slot(cpu)) };
    if f == 0 { return None; }
    unsafe {
        core::ptr::write_volatile(fixup_slot(cpu), 0);
        core::ptr::write_volatile(hit_slot(cpu), 1);
    }
    FAULTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    Some(f)
}

/// Read eight bytes at an address that may turn out to be anything.
/// `None` — it cannot be read (unaligned, the page is not mapped, or the read
/// failed on a #PF and was caught by the recovery point).
pub fn try_read(addr: u64, need_user: bool) -> Option<u64> {
    if addr & 7 != 0 || addr < 0x1000 { return None; }
    let cpu = crate::percpu::cpu_id();
    if cpu >= MAXCPU { return None; }
    // A cheap screening by the page tables: it removes the overwhelming majority of
    // garbage addresses without any exception at all. The recovery point is insurance
    // against a race with a neighbouring core, not the main mechanism.
    let (present, _, user, _) = crate::paging::query(addr);
    if !present || (need_user && !user) { return None; }
    let v: u64;
    unsafe {
        core::ptr::write_volatile(hit_slot(cpu), 0);
        let s = fixup_slot(cpu);
        core::arch::asm!(
            "lea {t}, [rip + 2f]",   // where to return if the load fails
            "mov [{s}], {t}",
            "xor {v:e}, {v:e}",      // on the failed path the value must be zero
            "mov {v}, [{a}]",
            "2:",
            "mov qword ptr [{s}], 0",
            t = out(reg) _,
            s = in(reg) s,
            v = out(reg) v,
            a = in(reg) addr,
            options(nostack),
        );
        if core::ptr::read_volatile(hit_slot(cpu)) != 0 { return None; }
    }
    Some(v)
}

/// A sensible ceiling on the size of ONE frame. A frame larger than sixteen megabytes
/// is not a frame but garbage in a register: the chain must be cut rather than followed.
const MAX_FRAME: u64 = 16 << 20;

/// The unwinding: `out[0]` is the address of the interrupted instruction, then the return addresses
/// from the nearest caller to the farthest. Returns the number of filled elements
/// (always >= 1).
///
/// Cutting the chain is a normal outcome rather than an error: code without frame pointers
/// should not have one. Every check below cuts off its own class of garbage, and
/// it is better to return depth 1 than to invent a stack.
pub fn walk(rip: u64, rbp: u64, user: bool, out: &mut [u64; MAXDEPTH]) -> usize {
    out[0] = rip;
    let mut n = 1;
    let mut fp = rbp;
    while n < MAXDEPTH {
        if fp == 0 || fp & 7 != 0 { break; }
        let Some(next) = try_read(fp, user) else { break };
        let Some(ret) = try_read(fp + 8, user) else { break };
        // A return address below the first page is not code under any circumstances.
        if ret < 0x1000 { break; }
        out[n] = ret;
        n += 1;
        // The stack grows DOWN: the caller's frame lies above. A violation of monotonicity
        // means a loop or garbage — and both end here.
        if next <= fp || next - fp > MAX_FRAME { break; }
        fp = next;
    }
    n
}

/// A self-check of the unwinding on a SYNTHETIC stack.
///
/// A real stack is no good for a gate: it depends on how the compiler
/// laid out those particular functions, and it would check code generation rather than the unwinder.
/// Here the frame chain is built by hand, and every check is responsible for its own
/// class of failure — a cut at zero, a loop, a step backwards, a frame that is too large,
/// an unmapped page, the depth ceiling.
pub fn selftest() -> bool {
    let mut ok = true;
    let mut check = |c: bool, what: &str| { if !c { crate::println!("unwind: FAIL {}", what); ok = false; } };

    // The frames go into a fresh piece of memory: it is mapped and belongs to the kernel.
    let Some(base) = crate::mem::alloc_contig(2) else {
        crate::println!("unwind: FAIL there is no memory for the synthetic stack");
        return false;
    };
    let mem = base as *mut u64;
    unsafe { core::ptr::write_bytes(mem as *mut u8, 0, 8192) };
    // A chain of three frames: f0 -> f1 -> f2 -> 0.
    // A frame: [fp] = the next fp, [fp+8] = the return address.
    let f0 = base + 0x100;
    let f1 = base + 0x200;
    let f2 = base + 0x300;
    unsafe {
        core::ptr::write_volatile(f0 as *mut u64, f1);
        core::ptr::write_volatile((f0 + 8) as *mut u64, 0xAAAA_1000);
        core::ptr::write_volatile(f1 as *mut u64, f2);
        core::ptr::write_volatile((f1 + 8) as *mut u64, 0xBBBB_2000);
        core::ptr::write_volatile(f2 as *mut u64, 0);
        core::ptr::write_volatile((f2 + 8) as *mut u64, 0xCCCC_3000);
    }
    let mut out = [0u64; MAXDEPTH];
    let n = walk(0xDEAD_0000, f0, false, &mut out);
    check(n == 4, "the depth of a chain of three frames");
    check(out[0] == 0xDEAD_0000, "the leaf is the address of the interrupted instruction");
    check(out[1] == 0xAAAA_1000 && out[2] == 0xBBBB_2000 && out[3] == 0xCCCC_3000, "the order of the return addresses");

    // A LOOP: a frame points at itself. Without the monotonicity check that is an infinite loop
    // INSIDE an interrupt handler, i.e. the machine hangs.
    unsafe { core::ptr::write_volatile(f0 as *mut u64, f0); }
    let n = walk(1, f0, false, &mut out);
    check(n == 2, "a loop is cut at the very first frame");

    // A STEP BACKWARDS: the next frame is below the current one. The stack grows down, so this is garbage.
    unsafe { core::ptr::write_volatile(f1 as *mut u64, f0); }
    let n = walk(1, f1, false, &mut out);
    check(n == 2, "a step backwards along the stack cuts the chain");

    // A FRAME THAT IS TOO LARGE.
    unsafe { core::ptr::write_volatile(f1 as *mut u64, f1 + MAX_FRAME + 4096); }
    let n = walk(1, f1, false, &mut out);
    check(n == 2, "a frame above the ceiling cuts the chain");

    // A NULL RETURN ADDRESS: the chain has ended.
    unsafe { core::ptr::write_volatile(f1 as *mut u64, f2); core::ptr::write_volatile((f1 + 8) as *mut u64, 0); }
    let n = walk(1, f1, false, &mut out);
    check(n == 1, "a null return address cuts the chain");

    // AN UNALIGNED POINTER.
    let n = walk(1, f0 + 3, false, &mut out);
    check(n == 1, "an unaligned frame pointer is rejected");

    // THE DEPTH CEILING: a long chain must be cut exactly at MAXDEPTH.
    unsafe {
        for i in 0..64u64 {
            let p = base + 0x400 + i * 16;
            core::ptr::write_volatile(p as *mut u64, p + 16);
            core::ptr::write_volatile((p + 8) as *mut u64, 0x9000_0000 + i);
        }
    }
    let n = walk(1, base + 0x400, false, &mut out);
    check(n == MAXDEPTH, "a chain longer than the ceiling is cut at MAXDEPTH");

    // AN UNMAPPED PAGE: the read must RETURN rather than bring the kernel down.
    let f_before = faults();
    let n = walk(1, 0x0000_7F00_0000_0000, false, &mut out);
    check(n == 1, "a frame in unmapped memory cuts the chain");
    check(faults() == f_before, "the screening by the page tables worked BEFORE the exception");

    // A FOREIGN RING. A ring 3 stack frame must lie on a page with the U/S bit:
    // otherwise the unwinder following a garbage RBP would read THE KERNEL'S MEMORY and hand
    // its contents into the workload's profile. The check must be done on a real
    // supervisor page — the kernel's image is one, while the allocator's frames
    // lie in the identity area, which is OPEN to the user (that is how
    // `talaria.cell=identity` works), and on them the check would mean nothing.
    let kaddr = crate::mem::kernel_lo() + 0x1000;
    check(try_read(kaddr, false).is_some(), "a kernel page is read as a ring 0 frame");
    check(try_read(kaddr, true).is_none(), "a kernel page is NOT accepted as a ring 3 frame");
    let n = walk(1, kaddr, true, &mut out);
    check(n == 1, "ring 3 unwinding is cut at a supervisor page");

    crate::mem::free_contig(base, 2);
    crate::println!("unwind: the unwinding checks {}", if ok { "all passed" } else { "THERE ARE FAILURES" });
    ok
}
