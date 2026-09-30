//! Bringing the arm64 machine up — phase 1 of docs/ARM64-PLAN.md. What exists so far:
//! the UART, the exception vectors, the early identity map, the device tree. The
//! banner prints the facts the rest of the port depends on (the exception level,
//! the tree's contents, the core's id registers, the counter's frequency), and the
//! tree is translated into the ONE boot record the kernel below knows —
//! the PVH `hvm_start_info` with its memory map and command line, exactly as
//! `boot::mb2_convert` does for GRUB on x86 — so that `mem::init`, `read_cmdline`
//! and everything after them can come over unchanged. Then it stops: nothing above
//! `arch::` is compiled for this target yet.

use core::sync::atomic::{AtomicU64, Ordering};
use super::{cpu, dtb, gic, mmu, percpu, probes, psci, timer, trap, uart};

/// A secondary core's stack (its PSCI context is the stack's top) and its "I am up"
/// mark. The BSP's stack is `boot_stack` in boot.rs.
const AP_STACK: usize = 16 * 1024;
const MAX_AP: usize = probes::MAX_AP;
#[repr(C, align(16))]
struct ApStack([u8; AP_STACK]);
static mut AP_STACKS: [ApStack; MAX_AP] = [const { ApStack([0; AP_STACK]) }; MAX_AP];
static AP_UP: [AtomicU64; MAX_AP] = [const { AtomicU64::new(0) }; MAX_AP];

/// Spin for `ns` of counter time (the bring-up has no scheduler to sleep in).
fn spin_ns(ns: u64) {
    let t0 = cpu::counter();
    let n = timer::ticks_for_ns(ns);
    while cpu::counter().wrapping_sub(t0) < n { core::hint::spin_loop(); }
}

/// The kernel's boot record: the PVH `hvm_start_info` (the layout `mem::init` and
/// `main::read_cmdline` read by offset) filled from the device tree.
#[repr(C)]
struct StartInfo {
    magic: u32, version: u32, flags: u32, nr_modules: u32,
    modlist_paddr: u64, cmdline_paddr: u64, rsdp_paddr: u64,
    memmap_paddr: u64, memmap_entries: u32, _reserved: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct MemEntry { addr: u64, size: u64, typ: u32, _r: u32 }

const PVH_MAGIC: u32 = 0x336E_C578;
const CMDLINE_MAX: usize = 2048;

static mut SI: StartInfo = StartInfo {
    magic: PVH_MAGIC, version: 1, flags: 0, nr_modules: 0, modlist_paddr: 0,
    cmdline_paddr: 0, rsdp_paddr: 0, memmap_paddr: 0, memmap_entries: 0, _reserved: 0,
};
static mut MEMMAP: [MemEntry; dtb::MAX_MEM] = [MemEntry { addr: 0, size: 0, typ: 0, _r: 0 }; dtb::MAX_MEM];
static mut CMDLINE: [u8; CMDLINE_MAX] = [0; CMDLINE_MAX];

/// The tree the loader left, as a slice: its header says how long it is. `None` when
/// `x0` did not point at a tree (a loader that gives none, or garbage).
fn tree_at(dtb: u64) -> Option<&'static [u8]> {
    if dtb == 0 { return None; }
    // SAFETY: the loader's contract puts the tree at `x0` in memory the identity map
    // covers; the header is read first, and only as many bytes as it names are used.
    let head = unsafe { core::slice::from_raw_parts(dtb as *const u8, 8) };
    let total = dtb::total_size(head)?;
    if !(40..=(4 << 20)).contains(&total) { return None; }
    Some(unsafe { core::slice::from_raw_parts(dtb as *const u8, total) })
}

/// The device tree -> `hvm_start_info`: RAM ranges become `memmap` entries of type 1,
/// `bootargs` becomes the command line. The tree itself is NOT reserved in the map
/// here: the allocator starts above the kernel and the loader puts the tree well
/// above it too (`virt`: at RAM base + 128 MiB), and the ranges the kernel keeps
/// point INTO the tree only until the copy below is made.
fn start_info_from(tree: &[u8], s: &dtb::Summary) -> u64 {
    unsafe {
        let mm = &mut *core::ptr::addr_of_mut!(MEMMAP);
        for i in 0..s.nmem { mm[i] = MemEntry { addr: s.mem[i].base, size: s.mem[i].size, typ: 1, _r: 0 }; }
        let si = &mut *core::ptr::addr_of_mut!(SI);
        si.memmap_paddr = core::ptr::addr_of!(MEMMAP) as u64;
        si.memmap_entries = s.nmem as u32;
        if let Some(ba) = s.bootargs {
            let cl = &mut *core::ptr::addr_of_mut!(CMDLINE);
            let n = ba.len.min(CMDLINE_MAX - 1);
            cl[..n].copy_from_slice(&tree[ba.off..ba.off + n]);
            cl[n] = 0;
            si.cmdline_paddr = core::ptr::addr_of!(CMDLINE) as u64;
        }
        core::ptr::addr_of!(SI) as u64
    }
}

#[unsafe(no_mangle)]
extern "C" fn rust_main_aarch64(dtb: u64) -> ! {
    // The tree is read BEFORE the MMU is on: the console's address is in it. With the
    // MMU off every access is Device-typed and must be aligned — the walker reads
    // 4-byte tokens at 4-byte offsets and bytes, nothing else.
    let tree = tree_at(dtb);
    let summary = tree.and_then(|t| dtb::parse(t).ok());
    if let Some(u) = summary.as_ref().and_then(|s| s.uart) { uart::set_base(u.base); }
    uart::init();
    crate::println!("talaria: boot (aarch64), EL{}, dtb={:#x}", cpu::current_el(), dtb);
    trap::init();
    // The identity map covers exactly the RAM the tree names (block 0 is the devices).
    // Without a tree: `virt`'s 1 GiB base and a generous size, so that the banner can
    // still say what went wrong.
    let (ram_base, ram_size) = match summary.as_ref() {
        Some(s) if s.nmem > 0 => {
            let lo = s.mem[..s.nmem].iter().map(|r| r.base).min().unwrap_or(0x4000_0000);
            let hi = s.mem[..s.nmem].iter().map(|r| r.base + r.size).max().unwrap_or(lo + (1 << 30));
            (lo, hi - lo)
        }
        _ => (0x4000_0000, 4 << 30),
    };
    unsafe { mmu::early_enable(ram_base, ram_size); }
    crate::println!("talaria: the MMU is on: sctlr={:#x} ttbr0={:#x}, RAM {:#x}..{:#x} in 1 GiB blocks",
        cpu::sctlr_el1(), cpu::ttbr0_el1(), ram_base, ram_base + ram_size);

    let midr = cpu::midr();
    crate::println!("talaria: cpu: implementer {:#x} part {:#x} r{}p{}, mpidr={:#x}",
        (midr >> 24) & 0xFF, (midr >> 4) & 0xFFF, (midr >> 20) & 0xF, midr & 0xF, cpu::mpidr());
    let hz = cpu::counter_hz();
    // In picoseconds: Graviton3 counts at 1.05 GHz (952 ps), QEMU's TCG at 62.5 MHz (16 000 ps).
    crate::println!("talaria: the counter: {} Hz ({} ps per tick), now {}", hz, if hz > 0 { 1_000_000_000_000 / hz } else { 0 }, cpu::counter());

    let (Some(tree), Some(s)) = (tree, summary) else {
        crate::println!("talaria: NO DEVICE TREE at x0 — the loader gave {:#x}; without ACPI or a tree the machine cannot be described", dtb);
        cpu::halt_forever();
    };
    crate::println!("talaria: the device tree: {} bytes at {:#x}; {} RAM range(s){}, {} cpu(s), {} virtio-mmio window(s)",
        tree.len(), dtb, s.nmem, if s.mem_seen > s.nmem { " (more were dropped)" } else { "" }, s.ncpu, s.nvirtio);
    for r in &s.mem[..s.nmem] { crate::println!("    RAM {:#012x}..{:#012x} ({} MiB)", r.base, r.base + r.size, r.size >> 20); }
    if let Some(u) = s.uart { crate::println!("    uart pl011 at {:#x}", u.base); }
    match (s.gicd, s.gicr, s.its) {
        (Some(d), Some(r), its) => crate::println!("    gic v3: distributor {:#x}, redistributors {:#x} (+{:#x}), its {:#x}",
            d.base, r.base, r.size, its.map(|i| i.base).unwrap_or(0)),
        _ => crate::println!("    gic v3: NOT in the tree — no interrupts until it is found (ACPI MADT on EC2)"),
    }
    crate::println!("    timer ppis: secure {} phys {} virt {} hyp {}; psci {}", s.timer_ppi[0], s.timer_ppi[1], s.timer_ppi[2], s.timer_ppi[3],
        match s.psci { Some(dtb::Psci::Hvc) => "hvc", Some(dtb::Psci::Smc) => "smc", None => "none" });
    if let Some(e) = s.ecam { crate::println!("    pcie ecam at {:#x} (+{:#x})", e.base, e.size); }
    let si = start_info_from(tree, &s);
    crate::println!("talaria: start_info at {:#x} ({} memmap entries)", si, s.nmem);

    // ── the first shared modules: the command line, the frame allocator, the page tables ──
    let cmdline = crate::read_cmdline(si);
    crate::println!("talaria: cmdline = \"{}\"", cmdline);
    crate::mem::init(si);
    for _ in 0..3 { if let Some(f) = crate::mem::alloc_frame() { crate::println!("talaria: alloc_frame -> {:#x}", f); } }
    crate::println!("talaria: free frames = {}", crate::mem::free_frames());
    crate::paging::init();
    // The tables just built are what the core runs on now: prove it with a page whose rights
    // change under us. A fresh frame is identity-mapped RW for the kernel; `protect_none`
    // must make it absent, `protect` present again — through `enc` on every write.
    crate::vdso::init();
    crate::vdso::map_into(crate::paging::kernel_cr3());
    unsafe { cpu::enable_el0_counter(); }
    dirty_probe();
    if let Some(f) = crate::mem::alloc_frame() {
        unsafe { core::ptr::write_volatile(f as *mut u64, 0x5EED_0000_0000_0001); }
        let (p0, w0, _, _) = crate::paging::query(f);
        crate::paging::protect_none(f, 4096);
        let (p1, _, _, _) = crate::paging::query(f);
        crate::paging::protect(f, 4096, true, false);
        let (p2, w2, _, _) = crate::paging::query(f);
        let back = unsafe { core::ptr::read_volatile(f as *const u64) };
        crate::println!("talaria: paging probe on {:#x}: present/rw {}/{} -> PROT_NONE present {} -> restored present/rw {}/{}, the word {} ({})",
            f, p0, w0, p1, p2, w2, if back == 0x5EED_0000_0000_0001 { "survived" } else { "was LOST" },
            if p0 && w0 && !p1 && p2 && w2 && back == 0x5EED_0000_0000_0001 { "as expected" } else { "NOT as expected" });
    }

    // ── phase 2 under the bring-up's own control: the GIC, the tick, the other cores, an IPI ──
    if let Some(p) = s.psci { psci::set_conduit(p == dtb::Psci::Hvc); }
    let (major, minor) = psci::version();
    crate::println!("talaria: psci {}.{} through {}", major, minor, if s.psci == Some(dtb::Psci::Hvc) { "hvc" } else { "smc" });
    let (Some(d), Some(r)) = (s.gicd, s.gicr) else {
        crate::println!("talaria: no GIC in the tree — no tick, no IPIs; stopping");
        cpu::halt_forever();
    };
    gic::set_frames(d.base, r.base, r.size);
    let nspi = unsafe { gic::init_distributor() };
    let mine = unsafe { gic::init_cpu() };
    crate::println!("talaria: gic v3 is up: {} SPIs; this core's redistributor {}", nspi, if mine { "found" } else { "NOT FOUND" });
    if s.timer_ppi[2] != 0 { trap::TIMER_PPI.store(s.timer_ppi[2], Ordering::Relaxed); }
    let ppi = trap::TIMER_PPI.load(Ordering::Relaxed);
    let me = cpu::cpu_index();
    unsafe { gic::enable_local(ppi); gic::enable_local(1); }
    let period = trap::TICK_NS.load(Ordering::Relaxed);
    timer::arm(period);
    unsafe { cpu::irq_enable(); }
    spin_ns(50_000_000);
    let ticks = timer::TICKS[me].load(Ordering::Relaxed);
    crate::println!("talaria: the tick: PPI {} at {} us — {} ticks in 50 ms on cpu{} ({})",
        ppi, period / 1000, ticks, me, if (40..=60).contains(&ticks) { "as expected" } else { "NOT the ~50 expected" });

    // The other cores: one PSCI call each, a stack of their own, a mark when they are up.
    let mut up = 1usize;
    for (i, &mpidr) in s.cpus[..s.ncpu].iter().enumerate() {
        if mpidr == cpu::mpidr() { continue; }
        if i >= MAX_AP { crate::println!("talaria: cpu{} (mpidr {:#x}): no stack slot — MAX_AP is {}", i, mpidr, MAX_AP); continue; }
        probes::CPU_MPIDR[i].store(mpidr, Ordering::Relaxed);
        let rc = start_secondary(i, mpidr);
        if rc != psci::SUCCESS { crate::println!("talaria: cpu{} (mpidr {:#x}): PSCI CPU_ON refused: {}", i, mpidr, rc); continue; }
        let t0 = cpu::counter();
        while AP_UP[i].load(Ordering::Acquire) == 0 && cpu::counter().wrapping_sub(t0) < timer::ticks_for_ns(2_000_000_000) { core::hint::spin_loop(); }
        if AP_UP[i].load(Ordering::Acquire) != 0 { up += 1; } else { crate::println!("talaria: cpu{} (mpidr {:#x}) did not report within 2 s", i, mpidr); }
    }
    crate::println!("talaria: {} of {} cores up", up, s.ncpu);
    if up > 1 {
        // An IPI to every secondary: SGI 1, counted in its handler.
        for &mpidr in s.cpus[..s.ncpu].iter() { if mpidr != cpu::mpidr() { gic::send_sgi(mpidr, 1); } }
        spin_ns(20_000_000);
        for (i, &mpidr) in s.cpus[..s.ncpu].iter().enumerate() {
            if mpidr == cpu::mpidr() { continue; }
            let idx = (mpidr & 0xFF) as usize;
            crate::println!("talaria: cpu{}: {} SGI(s) received, {} tick(s) of its own", i,
                trap::SGIS[idx.min(63)].load(Ordering::Relaxed), timer::TICKS[idx.min(63)].load(Ordering::Relaxed));
        }
    }
    unsafe { cpu::irq_disable(); }
    timer::stop();
    percpu::init_tpidr(cpu::cpu_index());
    probes::run(cmdline)
}

/// PSCI `CPU_ON` for the core with `mpidr`, with the stack of slot `i`; the PSCI result.
pub fn start_secondary(i: usize, mpidr: u64) -> i64 {
    if i >= MAX_AP { return psci::INVALID_PARAMS; }
    unsafe extern "C" { fn ap_start(); }
    let top = unsafe { core::ptr::addr_of!(AP_STACKS[i]) as u64 + AP_STACK as u64 };
    psci::cpu_on(mpidr, ap_start as *const () as usize as u64, top)
}

/// A secondary core, from `ap_start` (boot.rs): the same map, vectors and GIC state
/// as the boot core, its own timer, then it waits for interrupts. `ctx` is its stack top.
/// Its role is `smp::AP_MODE` (3 = the function in `AP_HOOK`; the rest = the job loop
/// until the scheduler comes across).
#[unsafe(no_mangle)]
extern "C" fn rust_ap_main(ctx: u64) -> ! {
    unsafe { mmu::enable_here(); }
    trap::init();
    let ok = unsafe { gic::init_cpu() };
    let me = cpu::cpu_index();
    if ok {
        unsafe { gic::enable_local(trap::TIMER_PPI.load(Ordering::Relaxed)); gic::enable_local(1); }
        timer::arm(trap::TICK_NS.load(Ordering::Relaxed));
    }
    crate::println!("talaria: cpu{} up (mpidr {:#x}, stack top {:#x}, redistributor {})", me, cpu::mpidr(), ctx, if ok { "found" } else { "NOT FOUND" });
    let slot = me.min(MAX_AP - 1);
    AP_UP[slot].store(1, Ordering::Release);
    percpu::init(me);
    unsafe { cpu::irq_enable(); }
    if super::smp::AP_MODE.load(Ordering::SeqCst) == 3 {
        let h = super::smp::AP_HOOK.load(Ordering::SeqCst);
        if h != 0 {
            let f: fn(usize) -> ! = unsafe { core::mem::transmute::<usize, fn(usize) -> !>(h as usize) };
            f(me);
        }
    }
    probes::ap_idle_loop(me)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    crate::println!("PANIC: {}", info);
    cpu::halt_irq_off_forever()
}

/// The dirty bit on a 4K page: hardware DBM where the core has it (`TCR.HD`), else the
/// conservative "always dirty". Split a fresh frame's 2M leaf, arm the page, write, and
/// ask `take_dirty` twice.
fn dirty_probe() {
    let Some(f) = crate::mem::alloc_frame() else { return; };
    crate::paging::protect(f, 4096, true, false);           // the 2M leaf -> a 4K page of its own
    let armed = !crate::paging::take_dirty(f) || !crate::paging::take_dirty(f); // drain whatever the split inherited
    unsafe { core::ptr::write_volatile(f as *mut u64, 1); }
    let d1 = crate::paging::take_dirty(f);
    let d2 = crate::paging::take_dirty(f);
    unsafe { core::ptr::write_volatile(f as *mut u64, 2); }
    let d3 = crate::paging::take_dirty(f);
    let hw = mmu::hw_dirty_on();
    let ok = if hw { armed && d1 && !d2 && d3 } else { d1 && d2 && d3 };
    crate::println!("talaria: dirty probe on {:#x}: {} — after a write {}, again {}, after another write {} ({})",
        f, if hw { "hardware DBM (TCR.HD)" } else { "no FEAT_HAFDBS: conservative, always dirty" }, d1, d2, d3,
        if ok { "as expected" } else { "NOT as expected" });
}

