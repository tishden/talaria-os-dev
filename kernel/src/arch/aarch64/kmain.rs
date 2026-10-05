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
pub(super) static AP_UP: [AtomicU64; MAX_AP] = [const { AtomicU64::new(0) }; MAX_AP];

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

/// What the UEFI stub (`kernel/uefi-stub`, the arm64 half) leaves in `x0` instead of a
/// device tree: the firmware's facts the kernel cannot ask for after `ExitBootServices`.
/// The layout is the stub's `BootBlock` — change them together.
#[repr(C)]
pub struct BootBlock {
    /// `BOOT_MAGIC`.
    pub magic: u64,
    /// The ACPI RSDP (0 — the firmware gave none).
    pub rsdp: u64,
    /// The device tree the firmware published as a configuration table (0 — none; a server has none).
    pub dtb: u64,
    /// The command line, NUL-terminated.
    pub cmdline: u64,
    /// The memory map in the PVH format (`MemEntry`), and how many entries.
    pub memmap: u64,
    pub memmap_entries: u64,
    /// The firmware's clock at the stub's start, seconds since the epoch (0 — unknown).
    pub epoch_s: u64,
}
/// "TALBOOT1", little-endian.
pub const BOOT_MAGIC: u64 = 0x3154_4F4F_424C_4154;

/// The stub's block at `x0`, if that is what `x0` points at.
fn boot_block_at(x0: u64) -> Option<&'static BootBlock> {
    if x0 == 0 || x0 & 7 != 0 { return None; }
    // SAFETY: the loader's contract puts either a tree or the block at `x0`, in memory the
    // identity addresses cover; the magic is read first (an aligned 8-byte load).
    let b = unsafe { &*(x0 as *const BootBlock) };
    if b.magic == BOOT_MAGIC { Some(b) } else { None }
}
/// The stub's memory map as a slice (at most `BB_MAX` entries are used).
fn boot_map(b: &BootBlock) -> &'static [MemEntry] {
    if b.memmap == 0 { return &[]; }
    // SAFETY: the stub's array, `memmap_entries` long, in memory it allocated for us.
    unsafe { core::slice::from_raw_parts(b.memmap as *const MemEntry, (b.memmap_entries as usize).min(BB_MAX)) }
}
/// A firmware's map is in dozens of pieces (the device tree's is one or two ranges).
const BB_MAX: usize = 256;
static mut BBMAP: [MemEntry; BB_MAX] = [MemEntry { addr: 0, size: 0, typ: 0, _r: 0 }; BB_MAX];

/// The machine's description -> `hvm_start_info`. From a device tree: its RAM ranges become
/// `memmap` entries of type 1 and `bootargs` the command line. From the UEFI stub's block:
/// its memory map (already in the PVH format), its command line and the RSDP. Everything
/// is COPIED into the kernel's statics: the tree and the block live in memory the frame
/// allocator will hand out.
fn start_info_from(tree: Option<&[u8]>, s: &dtb::Summary, bb: Option<&BootBlock>) -> u64 {
    unsafe {
        let si = &mut *core::ptr::addr_of_mut!(SI);
        let cl = &mut *core::ptr::addr_of_mut!(CMDLINE);
        let mut have_cmdline = false;
        if let Some(b) = bb {
            let src = boot_map(b);
            let mm = &mut *core::ptr::addr_of_mut!(BBMAP);
            mm[..src.len()].copy_from_slice(src);
            si.memmap_paddr = core::ptr::addr_of!(BBMAP) as u64;
            si.memmap_entries = src.len() as u32;
            si.rsdp_paddr = b.rsdp;
            if b.cmdline != 0 {
                let mut n = 0;
                while n < CMDLINE_MAX - 1 {
                    let c = core::ptr::read_volatile((b.cmdline + n as u64) as *const u8);
                    if c == 0 { break; }
                    cl[n] = c; n += 1;
                }
                cl[n] = 0;
                have_cmdline = n > 0;
            }
        } else {
            let mm = &mut *core::ptr::addr_of_mut!(MEMMAP);
            for i in 0..s.nmem { mm[i] = MemEntry { addr: s.mem[i].base, size: s.mem[i].size, typ: 1, _r: 0 }; }
            si.memmap_paddr = core::ptr::addr_of!(MEMMAP) as u64;
            si.memmap_entries = s.nmem as u32;
        }
        if let (false, Some(tree), Some(ba)) = (have_cmdline, tree, s.bootargs) {
            let n = ba.len.min(CMDLINE_MAX - 1);
            cl[..n].copy_from_slice(&tree[ba.off..ba.off + n]);
            cl[n] = 0;
            have_cmdline = true;
        }
        if have_cmdline { si.cmdline_paddr = core::ptr::addr_of!(CMDLINE) as u64; }
        core::ptr::addr_of!(SI) as u64
    }
}

#[unsafe(no_mangle)]
extern "C" fn rust_main_aarch64(x0: u64) -> ! {
    // `x0` is a device tree (`-kernel`, any loader that knows a Linux Image) or the UEFI
    // stub's block. Both are read BEFORE the MMU is on: the console's address is in them.
    // With the MMU off every access is Device-typed and must be aligned — the walkers
    // read bytes and aligned words, nothing else.
    let bb = boot_block_at(x0);
    let dtb = bb.map(|b| b.dtb).unwrap_or(x0);
    let tree = tree_at(dtb);
    let from_tree = tree.and_then(|t| dtb::parse(t).ok());
    // A firmware that gives an RSDP is described by ACPI — that is all a server gives.
    // The tree, when the firmware published one too (edk2 under QEMU), adds what the
    // static tables do not carry: the virtio-mmio windows and the real-time clock.
    let from_acpi = bb.filter(|b| b.rsdp != 0)
        .and_then(|b| unsafe { super::acpiboot::summary(b.rsdp) })
        .filter(|(s, _)| s.gicd.is_some() && s.gicr.is_some() && s.ncpu > 0);
    let mut acpi_found = None;
    let summary = match (from_acpi, from_tree) {
        (Some((mut s, found)), t) => {
            acpi_found = Some(found);
            // The RAM: what the firmware's map calls memory (the tables of ACPI — types 3
            // and 4 — must stay readable too), as ranges joined only where no whole gigabyte
            // lies between them. NOT one span: a Nitro guest has RAM at 1-2 GiB and from
            // 16 GiB, and the span made the PCI window between (the console's BAR at
            // 0x8004_8000) a Normal block of the early map.
            s.nmem = 0; s.mem_seen = 0;
            for e in bb.map(boot_map).unwrap_or(&[]) {
                if !matches!(e.typ, 1 | 3 | 4) || e.size == 0 { continue; }
                s.mem_seen += 1;
                let (a, end) = (e.addr, e.addr + e.size);
                if let Some(r) = s.mem[..s.nmem].iter_mut().find(|r| (a >> 30) <= ((r.base + r.size - 1) >> 30) + 1 && (r.base >> 30) <= ((end - 1) >> 30) + 1) {
                    let (lo, hi) = (r.base.min(a), (r.base + r.size).max(end));
                    *r = dtb::Range { base: lo, size: hi - lo };
                } else if s.nmem < dtb::MAX_MEM {
                    s.mem[s.nmem] = dtb::Range { base: a, size: e.size }; s.nmem += 1;
                } else {                                  // out of slots: the last one swallows it
                    let r = &mut s.mem[dtb::MAX_MEM - 1];
                    let (lo, hi) = (r.base.min(a), (r.base + r.size).max(end));
                    *r = dtb::Range { base: lo, size: hi - lo };
                }
            }
            s.mem_seen = s.nmem;
            if let Some(t) = t {
                s.virtio = t.virtio; s.nvirtio = t.nvirtio; s.virtio_seen = t.virtio_seen;
                s.rtc = t.rtc;
                if s.uart.is_none() { s.uart = t.uart; }
            }
            Some(s)
        }
        (None, t) => t,
    };
    // No console named by anybody: nothing is written (an address guessed on a machine
    // nobody described is an access to somebody else's register).
    match summary.as_ref().and_then(|s| s.uart.map(|u| (u, s.uart_kind))) {
        Some((u, dtb::UartKind::Pl011)) => uart::set_base(u.base),
        Some((u, dtb::UartKind::Ns16550 { shift, width })) => uart::set_16550(u.base, shift, width),
        None if bb.is_some() => uart::set_base(0),
        None => {}
    }
    if let Some(e) = bb.map(|b| b.epoch_s).filter(|&e| e != 0) { super::tsc::BOOT_EPOCH.store(e, Ordering::Relaxed); }
    uart::init();
    crate::println!("talaria: boot (aarch64), EL{}, {}={:#x}", cpu::current_el(), if bb.is_some() { "the UEFI stub's block" } else { "dtb" }, x0);
    trap::init();
    // The identity map covers exactly the RAM the description names, range by range (block 0
    // is the devices; a block with no RAM in it that holds a named device — the console, the
    // GIC, the ECAM — is a Device block: on `virt` everything of that kind sits below 1 GiB,
    // on a Graviton the 16550 is at 0xE2F0_0000 and the GIC above 512 GiB). Without a
    // description: `virt`'s 1 GiB base and a generous size, so that the banner can still
    // say what went wrong.
    let mut rams = [(0u64, 0u64); dtb::MAX_MEM];
    let nram = match summary.as_ref() {
        Some(s) if s.nmem > 0 => { for (i, r) in s.mem[..s.nmem].iter().enumerate() { rams[i] = (r.base, r.size); } s.nmem }
        _ => { rams[0] = (0x4000_0000, 4 << 30); 1 }
    };
    let (ram_base, ram_size) = {
        let lo = rams[..nram].iter().map(|r| r.0).min().unwrap_or(0);
        let hi = rams[..nram].iter().map(|r| r.0 + r.1).max().unwrap_or(0);
        (lo, hi - lo)
    };
    let devs: [u64; 5] = match summary.as_ref() {
        Some(s) => [s.uart, s.gicd, s.gicr, s.its, s.ecam].map(|r| r.map(|r| r.base).unwrap_or(0)),
        None => [0; 5],
    };
    unsafe { mmu::early_enable(&rams[..nram], &devs); }
    crate::println!("talaria: the MMU is on: sctlr={:#x} ttbr0={:#x}, RAM {:#x}..{:#x} in 1 GiB blocks",
        cpu::sctlr_el1(), cpu::ttbr0_el1(), ram_base, ram_base + ram_size);

    let midr = cpu::midr();
    crate::println!("talaria: cpu: implementer {:#x} part {:#x} r{}p{}, mpidr={:#x}",
        (midr >> 24) & 0xFF, (midr >> 4) & 0xFFF, (midr >> 20) & 0xF, midr & 0xF, cpu::mpidr());
    let hz = cpu::counter_hz();
    // In picoseconds: Graviton3 counts at 1.05 GHz (952 ps), QEMU's TCG at 62.5 MHz (16 000 ps).
    crate::println!("talaria: the counter: {} Hz ({} ps per tick), now {}", hz, if hz > 0 { 1_000_000_000_000 / hz } else { 0 }, cpu::counter());

    let Some(s) = summary else {
        crate::println!("talaria: NO DESCRIPTION OF THE MACHINE at x0 ({:#x}): neither a device tree nor the UEFI stub's block with usable ACPI tables", x0);
        cpu::halt_forever();
    };
    match (acpi_found, tree) {
        (Some(f), t) => crate::println!("talaria: ACPI (the RSDP {:#x}): MADT {}, GTDT {}, SPCR {}, FADT {}{}; {} cpu(s){}",
            bb.map(|b| b.rsdp).unwrap_or(0), f.madt, f.gtdt, f.spcr, f.fadt,
            match (f.uart_other, s.uart_kind) {
                (Some(_), _) => " (the console is of a type with no driver)",
                (None, _) if f.uart_pci.is_some() => " (the console is a 16550 behind PCI, its BAR 0)",
                (None, dtb::UartKind::Ns16550 { width: 4, .. }) => " (the console is a 16550, mmio32)",
                (None, dtb::UartKind::Ns16550 { .. }) => " (the console is a 16550)",
                _ => "",
            },
            s.ncpu, if t.is_some() { "; the firmware's device tree adds the virtio-mmio windows and the clock" } else { "" }),
        (None, Some(t)) => crate::println!("talaria: the device tree: {} bytes at {:#x}; {} RAM range(s){}, {} cpu(s), {} virtio-mmio window(s)",
            t.len(), dtb, s.nmem, if s.mem_seen > s.nmem { " (more were dropped)" } else { "" }, s.ncpu, s.nvirtio),
        (None, None) => {}
    }
    for r in &s.mem[..s.nmem] { crate::println!("    RAM {:#012x}..{:#012x} ({} MiB)", r.base, r.base + r.size, r.size >> 20); }
    if let Some(u) = s.uart {
        let kind = if s.uart_kind == dtb::UartKind::Pl011 { "pl011" } else { "16550" };
        match acpi_found.and_then(|f| f.uart_pci) {
            Some((b, d, f)) => crate::println!("    uart {} at {:#x} (BAR 0 of PCI {:02x}:{:02x}.{})", kind, u.base, b, d, f),
            None => crate::println!("    uart {} at {:#x}", kind, u.base),
        }
    }
    match (s.gicd, s.gicr, s.its) {
        (Some(d), Some(r), its) => crate::println!("    gic v3: distributor {:#x}, redistributors {:#x} (+{:#x}), its {:#x}",
            d.base, r.base, r.size, its.map(|i| i.base).unwrap_or(0)),
        _ => crate::println!("    gic v3: NOT in the tree — no interrupts until it is found (ACPI MADT on EC2)"),
    }
    crate::println!("    timer ppis: secure {} phys {} virt {} hyp {}; psci {}", s.timer_ppi[0], s.timer_ppi[1], s.timer_ppi[2], s.timer_ppi[3],
        match s.psci { Some(dtb::Psci::Hvc) => "hvc", Some(dtb::Psci::Smc) => "smc", None => "none" });
    if let Some(r) = s.rtc { crate::println!("    rtc pl031 at {:#x}", r.base); super::tsc::RTC_BASE.store(r.base, Ordering::Relaxed); }
    if let Some(e) = s.ecam {
        crate::println!("    pcie ecam at {:#x} (+{:#x}); the 32-bit memory window {:#x} (+{:#x})", e.base, e.size,
            s.pci_mem32.map(|r| r.base).unwrap_or(0), s.pci_mem32.map(|r| r.size).unwrap_or(0));
        // What the MCFG says on x86: the config window (1 MiB per bus) — and, since no firmware
        // ran before us, the window the BARs are assigned from. As many buses as the window
        // holds: nobody numbered the buses behind the bridges, `pci::number_bridge` does.
        crate::acpi::set_ecam(e.base, ((e.size >> 20).clamp(1, 256) - 1) as u8);
        if let Some(w) = s.pci_mem32 { crate::pci::set_mmio_window(w.base); }
    }
    let si = start_info_from(tree, &s, bb);
    crate::println!("talaria: start_info at {:#x} ({} memmap entries)", si, unsafe { (*core::ptr::addr_of!(SI)).memmap_entries });

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
    unsafe { cpu::enable_el0_access(); }
    dirty_probe();
    if let Some(f) = crate::mem::alloc_frame() {
        crate::paging::split_to_4k(f);                      // a 4K page of the kernel's 2M GB1 (as the dirty probe)
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
    // Every core the tree named, by index: `smp::topology_init` reads this map, and
    // `smp::start_cpu` brings a core up from it under the scheduler (`talaria.smp=`).
    for (i, &mpidr) in s.cpus[..s.ncpu].iter().enumerate() {
        if i < MAX_AP { probes::CPU_MPIDR[i].store(mpidr, Ordering::Relaxed); }
    }
    // MSI-X: the ITS the tree names (QEMU `virt,its=on`); without one the PCIe devices are
    // polled. After the core map: a collection is a core.
    if let Some(i) = s.its { unsafe { super::its::init(i.base); } }
    // The virtio-mmio windows, for the drivers' scan (`arch::virtio_mmio_slot`).
    for (i, &(base, intid)) in s.virtio[..s.nvirtio].iter().enumerate() {
        VIRTIO_WINDOWS[i].store(base, Ordering::Relaxed);
        VIRTIO_INTIDS[i].store(intid as u64, Ordering::Relaxed);
    }
    // THE FORK IN THE ROAD. The bring-up's own sequence (the tick counted, the secondaries
    // through PSCI, an SGI to each, EL0 and the vDSO probes) and the four probes TCG cannot
    // judge run under `talaria.test=bringup|ring|icache|bbm|svccost`. Anything else is the
    // kernel proper: the same `boot_shared` x86 enters after its early part, which ends in
    // the self-check or the production mode the command line names (docs/ARM64-PLAN.md §10.1).
    if !probes::is_arch_test(cmdline) {
        percpu::init(me);
        unsafe { gic::enable_local(ppi); for sgi in 0..16u32 { gic::enable_local(sgi); } }
        super::pmu::NARROW.store(crate::cmdline::flag(cmdline, "talaria.pmunarrow"), Ordering::Relaxed);
        super::pmu::USE_CHAIN.store(crate::cmdline::flag(cmdline, "talaria.pmuchain"), Ordering::Relaxed);
        crate::boot_shared(si, cmdline, cpu::counter_hz(), None);
    }
    // The bring-up's EL0 probes call the vDSO through the KERNEL's tables, so the image is
    // mapped there — here, and only here. Mapping it before the fork put a PDPT into the
    // kernel PML4's slot 0xFF, which `copy_kernel_pml4` then SHARED with every process: the
    // processes' stacks (0x7fff_e...) live in that slot, and two processes from disk ended up
    // on ONE stack (the first arm64 `procdisk` died on returns into 0 and into string bytes).
    crate::vdso::init();
    crate::vdso::map_into(crate::paging::kernel_cr3());
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

/// The `virtio,mmio` windows of the tree, by index (0 = none): `arch::virtio_mmio_slot`.
pub static VIRTIO_WINDOWS: [core::sync::atomic::AtomicU64; dtb::MAX_VIRTIO] = [const { core::sync::atomic::AtomicU64::new(0) }; dtb::MAX_VIRTIO];
/// The INTID of each window's line, by the same index (0 = none): `arch::virtio_mmio_gsi`.
pub static VIRTIO_INTIDS: [core::sync::atomic::AtomicU64; dtb::MAX_VIRTIO] = [const { core::sync::atomic::AtomicU64::new(0) }; dtb::MAX_VIRTIO];

/// PSCI `CPU_ON` for the core with `mpidr`, with the stack of slot `i`; the PSCI result.
pub fn start_secondary(i: usize, mpidr: u64) -> i64 {
    if i >= MAX_AP { return psci::INVALID_PARAMS; }
    unsafe extern "C" { fn ap_start(); }
    // The core starts with its MMU and data cache off: its reads of the kernel's root
    // (`mmu::root_for_new_core`) and its first stack frames go to memory past every cache.
    // A root still dirty in THIS core's cache would read as the stale value, and a line of
    // the stack cached here would later shadow what the secondary wrote past it. Under KVM
    // on a Graviton (S2FWB forces the guest cacheable) neither shows; on bare metal both
    // can — Linux cleans its `secondary_data` the same way.
    super::icache::clean_inval_poc(crate::paging::kernel_cr3_word(), 8);
    super::icache::clean_inval_poc(ap_stack_top(i) - 16384, 16384);
    psci::cpu_on(mpidr, ap_start as *const () as usize as u64, ap_stack_top(i))
}
/// The top of the boot stack of the secondary in slot `i` — also the stack its pickup loop
/// returns to when a task exits on it (`sched::set_ap_stack`).
pub fn ap_stack_top(i: usize) -> u64 {
    core::ptr::addr_of!(AP_STACKS) as u64 + (i.min(MAX_AP - 1) as u64 + 1) * AP_STACK as u64
}

/// A secondary core, from `ap_start` (boot.rs): the same map, vectors and GIC state
/// as the boot core, its own timer, then it waits for interrupts. `ctx` is its stack top.
/// Its role is `smp::AP_MODE` (3 = the function in `AP_HOOK`; the rest = the job loop
/// until the scheduler comes across).
#[unsafe(no_mangle)]
extern "C" fn rust_ap_main(ctx: u64) -> ! {
    unsafe { mmu::enable_here(); cpu::enable_el0_access(); }
    trap::init();
    let ok = unsafe { gic::init_cpu() };
    let me = cpu::cpu_index();
    if ok {
        unsafe { gic::enable_local(trap::TIMER_PPI.load(Ordering::Relaxed)); gic::enable_local(1); }
        timer::arm(trap::TICK_NS.load(Ordering::Relaxed));
    }
    crate::println!("talaria: cpu{} up (mpidr {:#x}, stack top {:#x}, redistributor {})", me, cpu::mpidr(), ctx, if ok { "found" } else { "NOT FOUND" });
    // The counters are per core (x86 programs them in `ap_entry` the same way): without this
    // a secondary's `perf` read the reset state, zeros, exactly where the workload runs.
    crate::pmu::init_cpu();
    let slot = me.min(MAX_AP - 1);
    AP_UP[slot].store(1, Ordering::Release);
    // The role, as x86's `ap_entry` reads it: 1 = the sync self-test, 2 = a core of the
    // scheduler's pool (the pickup loop), 3 = the function in AP_HOOK, else the bring-up's
    // job loop (no scheduler: one job at a time from `probes::run_on`).
    match super::smp::AP_MODE.load(Ordering::SeqCst) {
        1 => {
            percpu::init(me);
            unsafe { for sgi in 0..16u32 { gic::enable_local(sgi); } cpu::irq_enable(); }
            crate::sync::ap_selftest_loop()
        }
        2 => {
            percpu::init(me);
            unsafe { for sgi in 0..16u32 { gic::enable_local(sgi); } }
            crate::syscall::init_core(); // the entry only: the heap is the launch path's (see there)
            crate::sched::ap_loop(me)
        }
        3 => {
            percpu::init(me);
            unsafe { cpu::irq_enable(); }
            let h = super::smp::AP_HOOK.load(Ordering::SeqCst);
            if h != 0 {
                let f: fn(usize) -> ! = unsafe { core::mem::transmute::<usize, fn(usize) -> !>(h as usize) };
                f(me);
            }
            probes::ap_idle_loop(me)
        }
        // The cell's core (the datapath, `talaria.mode=cell|fast|fastflood`): as on x86 it
        // stays out of ONLINE — nobody schedules it or kicks it — and polls the card.
        _ if crate::runmode::CELL_AP.swap(false, Ordering::SeqCst) => {
            percpu::init_tpidr(me);
            crate::runmode::run_datapath()
        }
        _ => {
            percpu::init(me);
            unsafe { cpu::irq_enable(); }
            probes::ap_idle_loop(me)
        }
    }
}

/// The dirty bit on a 4K page: hardware DBM where the core has it (`TCR.HD`), else the
/// conservative "always dirty". Split a fresh frame's 2M leaf, arm the page, write, and
/// ask `take_dirty` twice.
fn dirty_probe() {
    let Some(f) = crate::mem::alloc_frame() else { return; };
    crate::paging::split_to_4k(f);                          // the kernel's GB1 is 2M leaves — split this one to a 4K page
    crate::paging::protect(f, 4096, true, false);           // RW+DBM, armed
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

