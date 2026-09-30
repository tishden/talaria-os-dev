//! ACPI (PLAN-V1.1 §4.2): RSDP -> XSDT/RSDT -> MADT (the real CPU topology —
//! the LAPIC ids of all the cores instead of the assumption 1..N) and MCFG (the ECAM base for PCIe,
//! groundwork for §4.3). The RSDP arrives in start_info.rsdp_paddr: under UEFI it is put there by the
//! uefi-stub (out of the configuration tables), under QEMU microvm/PVH — by QEMU itself.
//! The tables are read once at the start (the identity map of the first 4 GB is already there);
//! without an RSDP (rsdp_paddr=0) everything silently stays on the fallbacks (LAPIC ids 1..N).
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// The ceiling on MADT records. 256 — so as to COUNT all the machine's cores (on a `c6i.metal`
/// there are 128 of them, and the former 64 truncated the table silently, which read as "the machine
/// is half the size"). How many of them the kernel really brings up is a separate ceiling
/// `percpu::MAXCPU`.
const MAX_CPUS: usize = 256;
static NCPU: AtomicU32 = AtomicU32::new(0);
static mut APIC_IDS: [u32; MAX_CPUS] = [0; MAX_CPUS];
static ECAM_BASE: AtomicU64 = AtomicU64::new(0);
static ECAM_BUS_END: AtomicU32 = AtomicU32::new(0);
static ECAM_BUS_START: AtomicU32 = AtomicU32::new(0);
static LAPIC_ADDR: AtomicU64 = AtomicU64::new(0);

unsafe fn rd8(a: u64) -> u8 { core::ptr::read_volatile(a as *const u8) }
unsafe fn rd32(a: u64) -> u32 { core::ptr::read_unaligned(a as *const u32) }
unsafe fn rd64(a: u64) -> u64 { core::ptr::read_unaligned(a as *const u64) }

unsafe fn sig(a: u64) -> [u8; 4] { [rd8(a), rd8(a + 1), rd8(a + 2), rd8(a + 3)] }

/// Parse the MADT: collect the APIC ids of the enabled cores (type 0 LAPIC, type 9 x2APIC).
unsafe fn parse_madt(t: u64) {
    let len = rd32(t + 4) as u64;
    LAPIC_ADDR.store(rd32(t + 36) as u64, Ordering::Relaxed);
    let mut p = t + 44;
    let mut n = 0usize;
    while p + 2 <= t + len {
        let (typ, el) = (rd8(p), rd8(p + 1) as u64);
        if el < 2 { break; }
        match typ {
            0 if el >= 8 => { // Processor Local APIC: acpi_id, apic_id(u8), flags(u32)
                let flags = rd32(p + 4);
                if flags & 3 != 0 && n < MAX_CPUS { APIC_IDS[n] = rd8(p + 3) as u32; n += 1; }
            }
            1 if el >= 12 => { // I/O APIC: id(u8)@+2, addr(u32)@+4, GSI base(u32)@+8
                // Without it there are no interrupts FROM DEVICES: the virtio-mmio line under
                // a microvm goes here rather than into the LAPIC.
                crate::ioapic::note(rd32(p + 4) as u64, rd32(p + 8));
            }
            9 if el >= 16 => { // Local x2APIC: apic_id(u32) @+4, flags @+8
                let flags = rd32(p + 8);
                if flags & 3 != 0 && n < MAX_CPUS { APIC_IDS[n] = rd32(p + 4); n += 1; }
            }
            _ => {}
        }
        p += el;
    }
    NCPU.store(n as u32, Ordering::Relaxed);
}

// ── SRAT: which core belongs to which NUMA node ─────────────────────────────
//
// The scheduler (`affinity.rs`) needs it: the threads of one process are brought together onto one
// node, because foreign memory on a two-socket machine costs several times more
// than one's own, and no affinity policy compensates for that. A `c6i.metal` has
// two sockets, QEMU by default has no table at all — and that is not "unknown" but
// "there is one node", which for a machine with one node is exactly true.
const MAX_NODES_MAP: usize = 256;
static mut NODE_APIC: [u32; MAX_NODES_MAP] = [u32::MAX; MAX_NODES_MAP]; // APIC id
static mut NODE_ID: [u8; MAX_NODES_MAP] = [0; MAX_NODES_MAP];           // its node
static NODE_N: AtomicU64 = AtomicU64::new(0);
static NODE_MASK: AtomicU64 = AtomicU64::new(0);
// The MEMORY regions by node (SRAT type 1). Cores without their own memory are half
// the benefit: to bring the threads onto a node and go on taking frames from a foreign one means
// to pay for locality and not get it.
const MAX_MEMREG: usize = 32;
static mut MEM_BASE: [u64; MAX_MEMREG] = [0; MAX_MEMREG];
static mut MEM_LEN: [u64; MAX_MEMREG] = [0; MAX_MEMREG];
static mut MEM_NODE: [u8; MAX_MEMREG] = [0; MAX_MEMREG];
static MEM_N: AtomicU64 = AtomicU64::new(0);

unsafe fn parse_srat(t: u64) {
    let len = rd32(t + 4) as u64;
    let mut p = t + 48; // the header 36 + reserved 4 + reserved 8
    let mut n = 0usize;
    let mut mask = 0u64;
    while p + 2 <= t + len {
        let (typ, el) = (rd8(p), rd8(p + 1) as u64);
        if el < 2 { break; }
        match typ {
            // Processor Local APIC/SAPIC Affinity: the domain's low byte @+2,
            // apic_id @+3, flags @+4 (bit 0 — the record is fit), the domain's high bytes @+9..12
            0 if el >= 16 => {
                if rd32(p + 4) & 1 != 0 && n < MAX_NODES_MAP {
                    let dom = rd8(p + 2) as u32
                        | (rd8(p + 9) as u32) << 8 | (rd8(p + 10) as u32) << 16 | (rd8(p + 11) as u32) << 24;
                    NODE_APIC[n] = rd8(p + 3) as u32;
                    NODE_ID[n] = dom.min(254) as u8;
                    mask |= 1 << (dom & 63);
                    n += 1;
                }
            }
            // Memory Affinity: the domain @+2 (u32), the base @+8 (u64), the length @+16 (u64),
            // flags @+28 (bit 0 — the record is fit)
            1 if el >= 40 => {
                let n = MEM_N.load(Ordering::Relaxed) as usize;
                if rd32(p + 28) & 1 != 0 && n < MAX_MEMREG {
                    let len = rd64(p + 16);
                    if len != 0 {
                        MEM_BASE[n] = rd64(p + 8);
                        MEM_LEN[n] = len;
                        MEM_NODE[n] = rd32(p + 2).min(254) as u8;
                        MEM_N.store(n as u64 + 1, Ordering::Relaxed);
                    }
                }
            }
            // Processor Local x2APIC Affinity: the domain @+4, apic_id @+8, flags @+12
            2 if el >= 24
                && rd32(p + 12) & 1 != 0 && n < MAX_NODES_MAP => {
                    let dom = rd32(p + 4);
                    NODE_APIC[n] = rd32(p + 8);
                    NODE_ID[n] = dom.min(254) as u8;
                    mask |= 1 << (dom & 63);
                    n += 1;
                }
            _ => {}
        }
        p += el;
    }
    NODE_N.store(n as u64, Ordering::Relaxed);
    NODE_MASK.store(mask, Ordering::Relaxed);
}

/// The NUMA node of the core with the given APIC id. 255 — there is no SRAT table (a machine with one
/// node) or the core is not in it.
pub fn node_of_apic(apic: u32) -> u8 {
    let n = NODE_N.load(Ordering::Relaxed) as usize;
    for i in 0..n.min(MAX_NODES_MAP) {
        if unsafe { core::ptr::read_volatile(core::ptr::addr_of!(NODE_APIC[i])) } == apic {
            return unsafe { core::ptr::read_volatile(core::ptr::addr_of!(NODE_ID[i])) };
        }
    }
    255
}
/// How many NUMA nodes the firmware declared (0 — there is no SRAT).
pub fn nr_nodes() -> u32 { NODE_MASK.load(Ordering::Relaxed).count_ones() }

/// The node the physical address belongs to. 255 — there is no SRAT or the address is outside the
/// declared regions (for example, a hole for MMIO).
pub fn node_of_phys(addr: u64) -> u8 {
    let n = MEM_N.load(Ordering::Relaxed) as usize;
    for i in 0..n.min(MAX_MEMREG) {
        let (b, l) = unsafe {
            (core::ptr::read_volatile(core::ptr::addr_of!(MEM_BASE[i])),
             core::ptr::read_volatile(core::ptr::addr_of!(MEM_LEN[i])))
        };
        if addr >= b && addr < b + l { return unsafe { core::ptr::read_volatile(core::ptr::addr_of!(MEM_NODE[i])) }; }
    }
    255
}

/// The number of memory regions in the SRAT and the i-th region (the base, the length, the node) — for
/// building per-node allocators and for printing the map.
pub fn nr_mem_regions() -> usize { (MEM_N.load(Ordering::Relaxed) as usize).min(MAX_MEMREG) }
pub fn mem_region(i: usize) -> (u64, u64, u8) {
    if i >= nr_mem_regions() { return (0, 0, 255); }
    unsafe {
        (core::ptr::read_volatile(core::ptr::addr_of!(MEM_BASE[i])),
         core::ptr::read_volatile(core::ptr::addr_of!(MEM_LEN[i])),
         core::ptr::read_volatile(core::ptr::addr_of!(MEM_NODE[i])))
    }
}

/// Parse the MCFG: the ECAM base address of segment 0 (for the PCIe enumeration of §4.3).
/// §4.8: there may be SEVERAL records in the table (one segment per group of buses) —
/// we print them all, and take segment 0; the address's offset is counted from the record's
/// start_bus rather than from zero (for us it has so far always been zero).
unsafe fn parse_mcfg(t: u64) {
    let len = rd32(t + 4) as u64;
    let mut p = t + 44;
    let mut first = true;
    while p + 16 <= t + len {
        let base = rd64(p);
        let seg = rd8(p + 8) as u32 | ((rd8(p + 9) as u32) << 8);
        let (bs, be) = (rd8(p + 10), rd8(p + 11));
        crate::println!("acpi: an MCFG record: the base {:#x} the segment {} the buses {}..{}", base, seg, bs, be);
        if first && seg == 0 {
            ECAM_BASE.store(base, Ordering::Relaxed);
            ECAM_BUS_START.store(bs as u32, Ordering::Relaxed);
            first = false;
        }
        // We take the MAXIMUM boundary over all the records of segment 0: on a c7i the ENA card
        // lives on bus 0x27 (39) — it falls into the SECOND record (36..42) rather than
        // into the first (1..35), and it cannot be reached by a walk over the bridges of bus 0.
        if seg == 0 && be as u32 > ECAM_BUS_END.load(Ordering::Relaxed) {
            ECAM_BUS_END.store(be as u32, Ordering::Relaxed);
        }
        p += 16;
    }
}

// ── Powering the machine off (ACPI S5) ──────────────────────────────────────
//
// Why a kernel that "works forever anyway" needs this. An experiment on bare metal
// ends with the kernel standing in a `hlt` — while the instance keeps burning
// $6 an hour until somebody outside guesses that everything is already over. There is no console there,
// and "the set has been passed" differs from "it hung on the third item" only by the time
// on the clock. A real power-off solves both tasks at once: the billing stops,
// and the instance's state (`stopped`) becomes an UNAMBIGUOUS signal "it reached the
// end" — the very one that was missing all through session 9.
static PM1A_CNT: AtomicU32 = AtomicU32::new(0);
static PM1B_CNT: AtomicU32 = AtomicU32::new(0);
static SLP_TYPA: AtomicU32 = AtomicU32::new(u32::MAX);
static SLP_TYPB: AtomicU32 = AtomicU32::new(u32::MAX);

/// The FADT: the power management ports + the DSDT's address (the object `\_S5` lies in it).
unsafe fn parse_fadt(t: u64) {
    let len = rd32(t + 4) as u64;
    PM1A_CNT.store(rd32(t + 64), Ordering::Relaxed);
    PM1B_CNT.store(rd32(t + 68), Ordering::Relaxed);
    // The power button: the PM1a event block (@56, the length @88 — half for the STS,
    // half for the EN) and the entry into ACPI mode (SMI_CMD @48, ACPI_ENABLE @52).
    if len >= 89 {
        PM1A_EVT.store(rd32(t + 56), Ordering::Relaxed);
        PM1_EVT_LEN.store(rd8(t + 88) as u32, Ordering::Relaxed);
        SMI_CMD.store(rd32(t + 48), Ordering::Relaxed);
        ACPI_ENABLE.store(rd8(t + 52) as u32, Ordering::Relaxed);
    }
    // The DSDT: a 32-bit field @40, a 64-bit X_DSDT @140 (ACPI >= 2.0)
    let mut dsdt = rd32(t + 40) as u64;
    if len >= 148 { let x = rd64(t + 140); if x != 0 { dsdt = x; } }
    if dsdt != 0 { parse_s5(dsdt); }
}

/// Get the SLP_TYP for S5 out of the DSDT. We have no full AML interpreter and do not
/// need one: the object `\_S5` is always `Name(_S5, Package(){ typa, typb, ... })`,
/// and it is enough to find it by name and read the first two elements of the package.
/// That is what any minimal bootloader does; the risk is limited to the fact that with
/// an unfamiliar layout we simply will not find the values and will not power off.
unsafe fn parse_s5(dsdt: u64) {
    let len = rd32(dsdt + 4) as u64;
    if !(36..=1 << 22).contains(&len) { return; }
    let mut p = dsdt + 36;
    while p + 8 < dsdt + len {
        if sig(p) == *b"_S5_" {
            let mut q = p + 4;
            if rd8(q) == 0x08 { q += 1; }            // a NameOp before the name — skip it
            if rd8(q) != 0x12 { p += 1; continue; }  // PackageOp
            q += 1;
            let pkglen_lead = rd8(q);
            q += 1 + (pkglen_lead >> 6) as u64;      // PkgLength: 1 + the number of extra bytes
            q += 1;                                  // NumElements
            // the elements: 0x0A <byte> | 0x00 (Zero) | 0x01 (One)
            let mut val = [u32::MAX; 2];
            for v in val.iter_mut() {
                match rd8(q) {
                    0x0A => { *v = rd8(q + 1) as u32; q += 2; }
                    0x00 => { *v = 0; q += 1; }
                    0x01 => { *v = 1; q += 1; }
                    _ => break,
                }
            }
            if val[0] != u32::MAX {
                SLP_TYPA.store(val[0], Ordering::Relaxed);
                SLP_TYPB.store(if val[1] == u32::MAX { val[0] } else { val[1] }, Ordering::Relaxed);
            }
            return;
        }
        p += 1;
    }
}

/// Power the machine off (ACPI S5). It returns control only if it did not work —
/// the caller is bound to have a fallback path (a `hlt` or a reboot).
pub fn poweroff() {
    // No FADT ports on arm64: the firmware's PSCI is the one way off (docs/ARM64-PLAN.md §4.9).
    #[cfg(not(target_arch = "x86_64"))]
    crate::arch::cpu::power_off();
    #[cfg(target_arch = "x86_64")]
    poweroff_ports();
}
#[cfg(target_arch = "x86_64")]
fn poweroff_ports() {
    let a = PM1A_CNT.load(Ordering::Relaxed);
    let ta0 = SLP_TYPA.load(Ordering::Relaxed);
    if a == 0 {
        // There is no FADT at all (QEMU's microvirtual machines): what is left is to knock on the known
        // ACPI power-off ports of the i440fx/q35 and the "magic" Bochs port.
        // On REAL hardware that must not be done — the ports belong to who knows
        // what — so the branch is only for the case "there were no tables".
        unsafe {
            crate::arch::cpu::outw(0x604, 0x2000);
            crate::arch::cpu::outw(0xB004, 0x2000);
        }
        return;
    }
    // We know the port, but the SLP_TYP could not be got out of `\_S5`: we write 0 — that is the most
    // frequent value (in QEMU/SeaBIOS and in most firmwares S5 = type 0), and
    // we write it INTO OUR OWN port rather than at random into foreign addresses.
    let ta = if ta0 == u32::MAX { 0 } else { ta0 };
    let b = PM1B_CNT.load(Ordering::Relaxed);
    let tb0 = SLP_TYPB.load(Ordering::Relaxed);
    let tb = if tb0 == u32::MAX { 0 } else { tb0 };
    unsafe {
        let v = ((ta as u16) << 10) | (1 << 13); // SLP_TYP | SLP_EN
        crate::arch::cpu::outw(a as u16, v);
        if b != 0 {
            let v = ((tb as u16) << 10) | (1 << 13);
            crate::arch::cpu::outw(b as u16, v);
        }
    }
}

/// Whether we have anything to power the machine off with (for diagnostics in the log).
pub fn can_poweroff() -> bool {
    PM1A_CNT.load(Ordering::Relaxed) != 0 && SLP_TYPA.load(Ordering::Relaxed) != u32::MAX
}

// ── THE POWER BUTTON ─────────────────────────────────────────────────────────
//
// `aws ec2 stop-instances` (and `virsh shutdown`, and QEMU's `system_powerdown`) is
// A PRESS OF THE POWER BUTTON: the hypervisor sets the PWRBTN_STS bit in the PM1a_STS and
// waits for the guest to put the machine out itself. Several minutes later, having not waited,
// it de-energises it HARD. While the kernel did not listen to the button, every stop
// of an instance was a power cut: the FS journal survived it, the data did not
// (MariaDB on EC2 after a stop/start did not find a checkpoint in ib_logfile0, and
// the small writes not under an fsync were lost). Here is the regular stop:
//   the press -> a SIGTERM to all the processes -> the workload leaves by itself (exit_group
//   flushes the FS) -> `suite::park()` -> sync -> S5;
//   it did not leave in time (`talaria.pwrbtn=<s>`, the default 20) -> from the tick: sync -> S5.
// The polling goes from the BSP's tick four times a second — one read of a port, the SCI is not
// routed (we have no IOAPIC interrupt handler for it), while
// the PWRBTN_EN is set only so that the hypervisor sets the STS at all
// (QEMU without the EN silently drops the press).
static PM1A_EVT: AtomicU32 = AtomicU32::new(0);
static PM1_EVT_LEN: AtomicU32 = AtomicU32::new(0);
static SMI_CMD: AtomicU32 = AtomicU32::new(0);
static ACPI_ENABLE: AtomicU32 = AtomicU32::new(0);
static BTN_ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static BTN_NEXT_NS: AtomicU64 = AtomicU64::new(0);
static BTN_WAIT_NS: AtomicU64 = AtomicU64::new(20_000_000_000);
/// The moment of the forced power-off (mono ns); 0 — no stop has been requested.
static SHUTDOWN_AT_NS: AtomicU64 = AtomicU64::new(0);
const PWRBTN_BIT: u16 = 1 << 8;

#[cfg(target_arch = "x86_64")]
unsafe fn inw(port: u32) -> u16 { crate::arch::cpu::inw(port as u16) }
#[cfg(target_arch = "x86_64")]
unsafe fn outw(port: u32, v: u16) { crate::arch::cpu::outw(port as u16, v) }
#[cfg(target_arch = "x86_64")]
unsafe fn outb(port: u32, v: u8) { crate::arch::cpu::outb(port as u16, v) }
// The PM1 event/control blocks are port I/O: an x86 thing. On arm64 the tables (phase 6,
// from UEFI) carry no such ports — the button and the power-off go through PSCI — so the
// registers read as "nothing pending" and a write goes nowhere, and `init` finds no FADT.
#[cfg(not(target_arch = "x86_64"))]
unsafe fn inw(_port: u32) -> u16 { 0 }
#[cfg(not(target_arch = "x86_64"))]
unsafe fn outw(_port: u32, _v: u16) {}
#[cfg(not(target_arch = "x86_64"))]
unsafe fn outb(_port: u32, _v: u8) {}

/// The knob from the TAIL of the cmdline (the EC2 user-data): the button is armed before the tail is merged
/// (`main.rs`: the ACPI comes up before the network and the IMDS), and `talaria.pwrbtn=0` from
/// the user-data silently had no effect — `stop-instances --force` was still the
/// regular stop (a SIGTERM to mongod, sync, S5), and the power-cut probe
/// (powercut-ec2.sh) checked not a cut but the regular stop. The same class as
/// "the knobs from the user-data were not applied" of session 28. It is called after the merge.
pub fn button_from_tail(cmdline: &str) {
    let Some(wait_s) = crate::cmdline::num(cmdline, "talaria.pwrbtn") else { return };
    if wait_s == 0 {
        if BTN_ARMED.swap(false, Ordering::Relaxed) {
            let evt = PM1A_EVT.load(Ordering::Relaxed);
            let len = PM1_EVT_LEN.load(Ordering::Relaxed);
            if evt != 0 && len >= 4 { unsafe { let en = evt + len / 2; outw(en, inw(en) & !PWRBTN_BIT); } }
            crate::println!("acpi: the power button is SWITCHED OFF by the cmdline's tail (talaria.pwrbtn=0) — a stop from the outside will be a power cut");
        }
    } else if BTN_ARMED.load(Ordering::Relaxed) {
        BTN_WAIT_NS.store(wait_s.saturating_mul(1_000_000_000), Ordering::Relaxed);
        crate::println!("acpi: the power button: the deadline for leaving from the cmdline's tail — {} s", wait_s);
    } else { button_arm(wait_s); }
}

/// Arm the button: `wait_s` seconds for the workload to leave after the SIGTERM (0 — do not catch it).
pub fn button_arm(wait_s: u64) {
    if wait_s == 0 { crate::println!("acpi: the power button is switched off (talaria.pwrbtn=0) — a stop from the outside will be a power cut"); return; }
    BTN_WAIT_NS.store(wait_s.saturating_mul(1_000_000_000), Ordering::Relaxed);
    let evt = PM1A_EVT.load(Ordering::Relaxed);
    let len = PM1_EVT_LEN.load(Ordering::Relaxed);
    if evt == 0 || len < 4 {
        crate::println!("acpi: there is no power button — the PM1a_EVT_BLK is not set (a microvm/without a FADT): a stop from the outside will be a power cut");
        return;
    }
    let cnt = PM1A_CNT.load(Ordering::Relaxed);
    let smi = SMI_CMD.load(Ordering::Relaxed);
    let en_cmd = ACPI_ENABLE.load(Ordering::Relaxed);
    let mut sci_en = cnt != 0 && unsafe { inw(cnt) } & 1 != 0;
    // The firmware could have left the chipset in legacy mode: then the PM1 events are not
    // generated. The transition is a write of ACPI_ENABLE into the SMI_CMD, the chipset sets the SCI_EN.
    if !sci_en && cnt != 0 && smi != 0 && en_cmd != 0 {
        unsafe { outb(smi, en_cmd as u8); }
        let t0 = crate::tsc::rdtsc();
        let lim = crate::tsc::hz().max(1) / 10; // 100 ms
        while crate::tsc::rdtsc().wrapping_sub(t0) < lim {
            if unsafe { inw(cnt) } & 1 != 0 { sci_en = true; break; }
            core::hint::spin_loop();
        }
    }
    let en = evt + len / 2;
    unsafe {
        outw(evt, PWRBTN_BIT);                // clear a stale STS (a write of a one)
        outw(en, inw(en) | PWRBTN_BIT);       // PWRBTN_EN
    }
    BTN_ARMED.store(true, Ordering::Relaxed);
    crate::println!("acpi: the power button is armed (PM1a_EVT={:#x}, SCI_EN={}): a press -> a SIGTERM to the workload, {} s to leave, then sync and S5",
        evt, if sci_en { 1 } else { 0 }, wait_s);
}

/// Whether a regular stop has been requested (by the button or by a command): the paths "all the tasks have left"
/// are bound not to restart the workload but to put the machine out.
pub fn shutdown_requested() -> bool { SHUTDOWN_AT_NS.load(Ordering::Relaxed) != 0 }

/// Begin a regular stop: a SIGTERM to all the processes, a deadline for leaving.
pub fn request_shutdown(why: &str) {
    if SHUTDOWN_AT_NS.load(Ordering::Relaxed) != 0 { return; }
    let wait = BTN_WAIT_NS.load(Ordering::Relaxed);
    SHUTDOWN_AT_NS.store(crate::tsc::mono_ns().saturating_add(wait).max(1), Ordering::Relaxed);
    let n = crate::proc::term_all();
    crate::println!("acpi: {} — a SIGTERM to the processes ({}), I wait for them to leave for up to {} s, then sync and a power-off",
        why, n, wait / 1_000_000_000);
    if n == 0 { shutdown_now("there are no processes"); }
}

/// From the BSP's tick: the polling of the button (4 Hz) and the deadline of the forced power-off.
pub fn button_tick() {
    let now = crate::tsc::mono_ns();
    let at = SHUTDOWN_AT_NS.load(Ordering::Relaxed);
    if at != 0 {
        if now >= at { shutdown_now("the workload did not leave in time"); }
        return;
    }
    if !BTN_ARMED.load(Ordering::Relaxed) || now < BTN_NEXT_NS.load(Ordering::Relaxed) { return; }
    BTN_NEXT_NS.store(now + 250_000_000, Ordering::Relaxed);
    let evt = PM1A_EVT.load(Ordering::Relaxed);
    let sts = unsafe { inw(evt) };
    if sts & PWRBTN_BIT == 0 { return; }
    unsafe { outw(evt, PWRBTN_BIT); }
    request_shutdown("the power button (ACPI PWRBTN)");
}

/// Synchronise the FS and power the machine off. It is called from the tick too (IF=0): the flush goes
/// under the marker "in the tick", so that waiting for the device does not try to yield the core.
pub fn shutdown_now(why: &str) -> ! {
    SHUTDOWN_AT_NS.store(u64::MAX, Ordering::Relaxed);
    crate::println!("acpi: {} — a sync of the FS and a power-off (ACPI S5{})", why,
        if can_poweroff() { "" } else { ", the FADT/_S5 were not parsed — a blind attempt" });
    sync_fs();
    crate::trace::flush();
    poweroff();
    for _ in 0..200_000_000u64 { core::hint::spin_loop(); }
    crate::println!("acpi: the power-off did not work — a halt");
    crate::trace::flush();
    crate::arch::cpu::halt_forever()
}

/// The flush of the FS before a power-off/reboot: everything that lies in the block cache
/// goes to the disk, the journal is closed with a checkpoint. Without this any stop
/// was a power cut for the writes not under an fsync.
pub fn sync_fs() {
    if !crate::fs::mounted() { return; }
    let dirty = crate::ext2::dirty_count();
    let t0 = crate::tsc::rdtsc();
    crate::console::with_tick_marker(|| { crate::ext2::flush_all(); });
    let ms = crate::tsc::rdtsc().wrapping_sub(t0) / (crate::tsc::hz().max(1) / 1000).max(1);
    use core::sync::atomic::Ordering::Relaxed as R;
    let us = |t: u64| t / (crate::tsc::hz().max(1) / 1_000_000).max(1);
    crate::println!("acpi: the FS has been synchronised (there were {} dirty blocks, {} ms; writes {} — {} us, barriers {} — {} us, the worst {} us)", dirty,
        ms,
        crate::fs::DEV_WR_N.load(R), us(crate::fs::DEV_WR_TSC.load(R)), crate::fs::DEV_FL_N.load(R), us(crate::fs::DEV_FL_TSC.load(R)),
            us(crate::fs::DEV_FL_MAX.load(R)));
}

/// The entry point: rsdp_paddr from the start_info (0 — silently nothing).
/// Find the RSDP "the way a BIOS OS does it": the first kilobyte of the EBDA (its segment lies
/// in the BDA at 0x40E) and the ROM area 0xE0000..0xFFFFF, a step of 16 bytes, with a check of
/// the checksum of the first 20 bytes.
///
/// Why (A9). Until session 9 the RSDP's address came only from above: under UEFI it is put there by
/// `uefi-stub`, under QEMU — by the PVH itself. The BIOS path (our own bootloader tboot,
/// earlier GRUB) has it from nowhere: we do not collect the multiboot2 tags 14/15, and
/// `init(0)` simply exited. The outcome on EC2 was visible in the log directly: `rsdp=0x0`,
/// then neither the MADT (which means SMP would have gone by made-up APIC ids) nor the MCFG (which means
/// the ECAM is unknown and PCIe is probed blindly). The scan is exactly what any
/// bootloader on a BIOS machine does, and it is checked locally: in SeaBIOS the RSDP lies
/// in the same ROM area.
unsafe fn find_rsdp() -> u64 {
    unsafe fn probe(mut p: u64, end: u64) -> u64 {
        while p + 20 <= end {
            if sig(p) == *b"RSD " && sig(p + 4) == *b"PTR " {
                let mut sum = 0u8;
                for i in 0..20 { sum = sum.wrapping_add(rd8(p + i)); }
                if sum == 0 { return p; }
            }
            p += 16;
        }
        0
    }
    let ebda = (core::ptr::read_volatile(0x40E as *const u16) as u64) << 4;
    if (0x400..0xA_0000).contains(&ebda) {
        let r = probe(ebda, ebda + 1024);
        if r != 0 { return r; }
    }
    probe(0xE_0000, 0x10_0000)
}

pub fn init(rsdp: u64) {
    let rsdp = if rsdp != 0 { rsdp } else {
        // The EBDA/ROM scan is a PC's: on arm64 the tables come from the firmware's pointer
        // (phase 6) or not at all, and the low addresses it would read are not RAM.
        #[cfg(not(target_arch = "x86_64"))]
        let r = 0u64;
        #[cfg(target_arch = "x86_64")]
        let r = unsafe { find_rsdp() };
        if r != 0 { crate::println!("acpi: the RSDP was found by scanning the BIOS areas: {:#x}", r); }
        else { crate::println!("acpi: the RSDP was not found (neither from the bootloader nor in the EBDA/ROM) — the fallbacks"); }
        r
    };
    if rsdp == 0 { return; }
    unsafe {
        if &sig(rsdp) != b"RSD " { return; } // "RSD PTR "
        let rev = rd8(rsdp + 15);
        // the XSDT (ACPI >= 2.0, 64-bit records) or the RSDT (32-bit records)
        let (sdt, wide) = if rev >= 2 && rd64(rsdp + 24) != 0 { (rd64(rsdp + 24), true) } else { (rd32(rsdp + 16) as u64, false) };
        if sdt == 0 { return; }
        let len = rd32(sdt + 4) as u64;
        let esz = if wide { 8 } else { 4 };
        let mut p = sdt + 36;
        while p + esz <= sdt + len {
            let t = if wide { rd64(p) } else { rd32(p) as u64 };
            if t != 0 {
                match &sig(t) {
                    b"APIC" => parse_madt(t),
                    b"SRAT" => parse_srat(t),
                    b"MCFG" => parse_mcfg(t),
                    b"FACP" => parse_fadt(t),
                    // `\_S5` does not always live in the DSDT: some firmwares (including
                    // the Nitro hypervisor — checked on a c6i.large: the PM1a_CNT was found,
                    // but the sleep type was not) move it into an SSDT. We look there too.
                    b"SSDT" => parse_s5(t),
                    _ => {}
                }
            }
            p += esz;
        }
    }
    let n = NCPU.load(Ordering::Relaxed);
    if n > 0 {
        let show = (n as usize).min(8); // the first 8 ids (on a metal — up to 64+, do not spam)
        crate::ioapic::dump();
    crate::println!("acpi: MADT cpus={} apic_ids[0..{}]={:?} lapic={:#x}",
            n, show, unsafe { &APIC_IDS[..show] }, LAPIC_ADDR.load(Ordering::Relaxed));
    }
    crate::println!("acpi: the power: PM1a_CNT={:#x} PM1b_CNT={:#x} SLP_TYPa={} SLP_TYPb={} (S5 {})",
        PM1A_CNT.load(Ordering::Relaxed), PM1B_CNT.load(Ordering::Relaxed),
        SLP_TYPA.load(Ordering::Relaxed) as i64, SLP_TYPB.load(Ordering::Relaxed) as i64,
        if can_poweroff() { "was parsed" } else { "was NOT parsed" });
    let e = ECAM_BASE.load(Ordering::Relaxed);
    if e != 0 { crate::println!("acpi: MCFG ecam={:#x} bus0..{}", e, ECAM_BUS_END.load(Ordering::Relaxed)); }
    crate::println!("acpi: SRAT: the NUMA nodes {} (records about cores {})",
        nr_nodes(), NODE_N.load(Ordering::Relaxed));
}

/// How many cores there are by the MADT (0 — the ACPI was not parsed: the callers fall back to 1..N).
pub fn nr_cpus() -> u32 { NCPU.load(Ordering::Relaxed) }
/// The APIC id of the i-th core by the MADT (the BSP is usually the first — for its id see lapic 0x20).
pub fn apic_id(i: usize) -> u32 { unsafe { APIC_IDS[i.min(MAX_CPUS - 1)] } }
/// The ECAM base from the MCFG (0 — there is none; §4.3).
pub fn ecam_base() -> u64 { ECAM_BASE.load(Ordering::Relaxed) }
/// The last bus covered by the ECAM window (the MCFG). §4.8: on the 7th generation the devices
/// live not only on bus 0, and the enumeration has to be carried up to this boundary.
pub fn ecam_bus_end() -> u8 { ECAM_BUS_END.load(Ordering::Relaxed).min(255) as u8 }
