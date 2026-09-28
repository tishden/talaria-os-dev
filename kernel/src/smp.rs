//! Bringing the secondary cores (APs) up: a LAPIC INIT-SIPI-SIPI + a real-mode trampoline
//! (16-bit -> long mode), reusing the BSP's page tables. Partition-first:
//! the BSP is the control plane, an AP is a cell's core.
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

/// An AP's role at the start: 0 = the cell's datapath (the default), 1 = the sync self-test
/// (a ring0 mwait/hlt on a word), 2 = the scheduler of pinned threads (PLAN-SYNC),
/// 3 = a self-check function from `AP_HOOK` (ring 0, without the scheduler).
pub static AP_MODE: AtomicU8 = AtomicU8::new(0);
/// The AP's body in mode 3: `fn(the core's logical number) -> !`. It is needed by the gates that
/// need two CORES (not two threads) in ring 0 — for example, so that one holds a lock
/// while the other waits for it.
pub static AP_HOOK: AtomicU64 = AtomicU64::new(0);

const LAPIC: u64 = 0xFEE0_0000;
const TRAMP: u64 = 0x8000; // physical, real-mode SIPI vector 0x08

pub static AP_ALIVE: AtomicU64 = AtomicU64::new(0);
// The BSP's reference state for cloning onto the APs (the SMP symmetry): the threads migrate
// between the cores, so the CPU's features must coincide. We clone them as a batch, but the exact
// bits were ISOLATED by a bisect on a c6a.metal (an EPYC 7R13, talaria.apcr4mask=, §2.2):
//   CR4.OSXSAVE (bit 18) is the culprit of "ring3 on an AP is several times slower": without it
//     XCR0 is not applied, the extended state is unavailable, and smp2 gives a speedup of
//     0.47 instead of 2.02 (the other bits — PGE/OSXMMEXCPT/FSGSBASE/SMEP/SMAP —
//     do not affect the speed: 2.01-2.06);
//   CR4.OSFXSR (bit 9) is obligatory in general: the kernel is built with SSE2, without it an AP
//     falls on the very first SSE instruction (a #UD -> a crash dump).
// We clone CR0 for the sake of WP=1 (the COW protection of the kernel's writes) and the symmetry of NE/AM.
static BSP_CR0: AtomicU64 = AtomicU64::new(0);
static BSP_CR4: AtomicU64 = AtomicU64::new(0);
static BSP_XCR0: AtomicU64 = AtomicU64::new(0);
/// The mask of the CR4 bits NOT to apply on the APs (`talaria.apcr4mask=0x..`) —
/// a bisect instrument: which bit of CR4 exactly cures the threefold slowdown of ring 3
/// on an AP (see POC-RESULTS v0.55, the find on the EPYC).
pub static AP_CR4_MASK: AtomicU64 = AtomicU64::new(0);
pub static AP_HEARTBEAT: AtomicU64 = AtomicU64::new(0);
static AP_STACK_TOP: AtomicU64 = AtomicU64::new(0);

/// The LAPIC's mode: `false` — xAPIC (the MMIO window 0xFEE00000), `true` — x2APIC (the MSR
/// 0x800 + the reg/16). The flag is GLOBAL because the mode must coincide for all the
/// cores: an MSI's address (`0xFEE00000 | apic<<12`) and the sending of IPIs are computed
/// identically on every core, and "half the machine in x2APIC" is not a mode but
/// a bug. The BSP switches it on first and sets the flag; an AP sees the flag and switches it on at itself BEFORE
/// the first access to the LAPIC.
#[unsafe(no_mangle)]
pub static X2APIC: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// In which mode the LAPIC works (the diagnostics: `kstat` and the self-tests).
pub fn x2apic() -> bool { X2APIC.load(Ordering::Relaxed) }

/// The MSR number of a LAPIC register in x2APIC mode.
#[inline(always)]
fn x2_msr(reg: u64) -> u32 { 0x800 + (reg >> 4) as u32 }

unsafe fn lapic_write(reg: u64, val: u32) {
    if X2APIC.load(Ordering::Relaxed) { wrmsr(x2_msr(reg), val as u64); }
    else { core::ptr::write_volatile((LAPIC + reg) as *mut u32, val); }
}

/// Enable the LAPIC: mask all the LVTs (so as not to catch unexpected vectors),
/// then SVR bit 8 + the spurious vector 0xFF. It is needed for receiving IPIs.
/// Mask the legacy sources of interrupts left by the firmware (UEFI):
/// both 8259 PICs (OVMF keeps the PIT timer alive) + all the LAPIC's LVTs (the firmware's LVT0
/// is an ExtINT: the delivery path of the PIC's interrupts bypassing the IOAPIC). It is called
/// once before the first sti; on a microvm (there is no PIC) the writes into the ports are a no-op.
pub fn legacy_quiesce(cmdline: &str) {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") 0x21u16, in("al") 0xFFu8, options(nostack, nomem)); // the master PIC: everything is masked
        core::arch::asm!("out dx, al", in("dx") 0xA1u16, in("al") 0xFFu8, options(nostack, nomem)); // slave PIC
    }
    lapic_mode_select(cmdline);
    pvipi_select(cmdline);
    pveoi_from_cmdline(cmdline);
    lapic_enable();
    pveoi_enable_here(); // the exitless EOI (KVM PV EOI), if the host offers it
    steal_enable_here(); // how much time the host takes (and who is taken off the core right now)
}

/// A9: move the LAPIC into xAPIC mode if the firmware left x2APIC.
///
/// Why this exists at all. An access to the MMIO window 0xFEE00000 is lawful only in
/// xAPIC mode: with `IA32_APIC_BASE.EXTD` raised (bit 10, x2APIC) ANY
/// access to the window gives a #GP. Under QEMU and on Nitro guests the firmware leaves
/// xAPIC, so for three sessions that was not visible; on bare metal the firmware is entitled
/// to switch x2APIC on itself, and then the kernel would fall into a #GP at the VERY FIRST action
/// after the IDT — with the picture "control has been received, silence". The transition
/// x2APIC -> xAPIC is allowed only through a completely switched-off APIC, so
/// we write the MSR twice.
///
/// Now this is A FALLBACK path: `lapic_mode_select` prefers a real x2APIC
/// (the registers through the MSR 0x800+), and falls here if there is none in CPUID or it is
/// switched off by hand (`talaria.x2apic=0`).
pub fn force_xapic() {
    unsafe {
        let v = rdmsr(0x1B);
        if v & (1 << 10) == 0 { return; }
        wrmsr(0x1B, v & !((1 << 10) | (1 << 11)));   // the APIC is switched off entirely
        wrmsr(0x1B, (v & !(1 << 10)) | (1 << 11));   // it is switched on as xAPIC
        crate::println!("smp: the firmware left x2APIC — it has been moved to xAPIC (the MMIO {:#x})", LAPIC);
    }
}

unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nostack, nomem));
    ((hi as u64) << 32) | lo as u64
}
unsafe fn wrmsr(msr: u32, v: u64) {
    core::arch::asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack, nomem));
}

pub fn lapic_enable() {
    unsafe {
        // The mode is chosen by the BSP once per machine. An AP comes here in xAPIC (an INIT
        // resets the APIC into the legacy mode), while the flag is already global — which means
        // the VERY FIRST access to a register would go through an MSR with x2APIC
        // switched off, that is, into a #GP. So we switch it on locally before the writes.
        if X2APIC.load(Ordering::Relaxed) { x2apic_enable_here(); }
        lapic_write(0x320, 0x10000); // LVT Timer masked
        lapic_write(0x350, 0x10000); // LVT LINT0 masked
        lapic_write(0x360, 0x10000); // LVT LINT1 masked
        lapic_write(0x370, 0x10000); // LVT Error masked
        lapic_write(0x080, 0);       // TPR = 0 (accept all the priorities)
        lapic_write(0x0F0, 0x1FF);   // SVR: enable + spurious 0xFF
    }
}
unsafe fn lapic_read(reg: u64) -> u32 {
    if X2APIC.load(Ordering::Relaxed) { rdmsr(x2_msr(reg)) as u32 }
    else { core::ptr::read_volatile((LAPIC + reg) as *const u32) }
}

/// Write the ICR (send an IPI). In xAPIC that is TWO accesses to the MMIO — the upper
/// half with the addressee and the lower one, which starts the delivery; in x2APIC it is
/// ONE write of a 64-bit MSR. Under a hypervisor without APICv (and that is both c6i and our
/// stand) every access to the LAPIC's window is an exit to the host with an emulation of the instruction,
/// that is, about ten microseconds; a write of an MSR exits to the host too, but without
/// a parsing of the instruction — single microseconds. The sending of an IPI is the hot path
/// of a thread's wakeup, and here that is a difference of several times.
#[inline]
unsafe fn icr_write(dest: u32, low: u32) {
    if X2APIC.load(Ordering::Relaxed) { wrmsr(0x830, ((dest as u64) << 32) | low as u64); }
    else { lapic_write(0x310, dest << 24); lapic_write(0x300, low); }
}

/// THIS core's APIC id. In xAPIC it lies in the upper byte of the register 0x20, in
/// x2APIC it is the whole 32-bit value.
#[inline]
unsafe fn apic_id_here() -> u32 {
    let v = lapic_read(0x20);
    if X2APIC.load(Ordering::Relaxed) { v } else { v >> 24 }
}

/// Switch x2APIC on on THE CURRENT core. The transition xAPIC -> x2APIC is lawful only with
/// the APIC already switched on (bit 11), so we set both bits at once.
unsafe fn x2apic_enable_here() {
    let v = rdmsr(0x1B);
    if v & (1 << 10) != 0 { return; } // it is already on
    wrmsr(0x1B, v | (1 << 11) | (1 << 10));
}

/// The BSP: choose the LAPIC's mode. `talaria.x2apic=0` — stay on xAPIC (a bisect).
///
/// x2APIC is taken when CPUID declares it (1:ECX bit 21). The cores' identifiers
/// stay the same meanwhile (we have fewer than 256 of them), so the MSI addresses and the parsing of the MADT
/// do not change — only THE PRICE of an access to the LAPIC changes.
fn lapic_mode_select(cmdline: &str) {
    let want = crate::cmdline::value(cmdline, "talaria.x2apic") != Some("0");
    // CPUID.1:ECX bit 21. Exactly ECX: in the EBX of the same leaf bit 21 is the fifth bit
    // of the field "how many logical processors", and on a machine with 32 vCPUs it is raised
    // by itself. Reading the wrong register, the kernel silently did not switch x2APIC on on
    // the machines that have it, and would have switched it on where it was not asked for.
    let (_, _, c, _) = unsafe { cpuid_raw(1) };
    let have = c & (1 << 21) != 0;
    if want && have {
        unsafe { x2apic_enable_here(); }
        X2APIC.store(true, Ordering::SeqCst);
        crate::println!("smp: the LAPIC is in x2APIC mode (the registers go through MSRs: the IPIs and the EOI without an emulation of MMIO)");
    } else {
        force_xapic();
        if !have { crate::println!("smp: x2APIC is not declared by CPUID — the xAPIC mode (the MMIO {:#x})", LAPIC); }
    }
}

/// ── THE ACKNOWLEDGEMENT OF AN INTERRUPT (EOI) ───────────────────────────────
///
/// An EOI is written at EVERY interrupt: the scheduler's tick, a wakeup IPI, a TLB
/// flush. On a stand without APICv (Sandy Bridge, c6i — everywhere where `enable_apicv=N`)
/// a write into the LAPIC's window is an exit to the host with AN EMULATION of the instruction: measured at 10.2 us
/// per write. With a slice of 0.5 ms that is 2 % of the processor for nothing, while at
/// a thread's wakeup it is a direct latency.
///
/// KVM offers the PV EOI protocol: before an entry into the guest the host raises bit 0 in
/// a word registered by the guest if there is nothing to acknowledge (there are no other
/// pending interrupts). The guest extinguishes the bit with an ordinary `btr` instruction in its own
/// memory and does NOT access the LAPIC at all — there is no exit to the host. If the bit is not
/// raised, we write the EOI as usual.
///
/// The word is deliberately NON-atomic: it belongs to one core, and it is corrected
/// only by the host at an entry into that core (the same way in Linux — `__test_and_clear_bit`).
const MSR_KVM_PV_EOI_EN: u32 = 0x4b56_4d04;
static PV_EOI_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// How many acknowledgements still required an access to the LAPIC (the `kstat`
/// diagnostics: with a live PV EOI on a computing workload there are single ones of them). ONLY
/// the misses are counted — a counter of the hits would stand in the hot path as a common cache line
/// that all the cores would pull at every interrupt.
#[unsafe(no_mangle)]
pub static PV_EOI_MISS: AtomicU64 = AtomicU64::new(0);

/// Is the PV EOI protocol allowed (`talaria.pveoi=0` — switch it off; a bisect).
static PV_EOI_WANT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
pub fn pveoi_from_cmdline(cmdline: &str) {
    if crate::cmdline::value(cmdline, "talaria.pveoi") == Some("0") {
        PV_EOI_WANT.store(false, Ordering::SeqCst);
    }
}

// ── THE STOLEN TIME AND THE PREEMPTED vCPUs (KVM steal-time) ────────────────
//
// WHAT FOR. The kernel measures ITSELF, while it executes on somebody else's processor: the host is entitled to take
// a vCPU off a core for milliseconds. While that is not visible, every such pause looks
// like our latency — and is analysed as ours. Exactly that question arose at
// the loss of `mysql select1` p99.99 = 18.5 ms with 17 TLB flushes that
// waited for an acknowledgement for THE FULL 10 ms and gave up: "the neighbour does not answer" and "the neighbour
// is not on a processor" are different diagnoses with different fixes.
//
// KVM gives both values in one structure per vCPU (the MSR 0x4b564d03):
// `steal` is the nanoseconds that were taken from this vCPU, and `preempted` is whether
// it is taken off a core RIGHT NOW. It is registered once per core.
const MSR_KVM_STEAL_TIME: u32 = 0x4b56_4d03;
/// The structure's layout is a contract with KVM (Documentation/virt/kvm/x86/msr.rst).
/// The alignment of 64 bytes is required by the same documentation.
#[repr(C, align(64))]
pub struct StealTime {
    pub steal: u64,
    pub version: u32,
    pub flags: u32,
    /// bit 0 — the vCPU is preempted by the host, bit 1 — a request to flush its TLB
    pub preempted: u8,
    pub _pad0: [u8; 3],
    pub _pad: [u32; 11],
}
const STEAL0: StealTime = StealTime { steal: 0, version: 0, flags: 0, preempted: 0, _pad0: [0; 3], _pad: [0; 11] };
static mut STEAL: [StealTime; MAXCPU] = [STEAL0; MAXCPU];
static STEAL_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Can the host flush a preempted vCPU's TLB at our request
/// (KVM_FEATURE_PV_TLB_FLUSH). Without that skipping the wait for an acknowledgement is
/// NOT ALLOWED: a stale TLB entry will stay at the returned neighbour.
static PV_TLB_FLUSH: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
pub const KVM_VCPU_PREEMPTED: u8 = 1;
pub const KVM_VCPU_FLUSH_TLB: u8 = 2;

/// Register THE CURRENT core's structure of the stolen time.
pub fn steal_enable_here() {
    if !kvm_here() { return; }
    let (feat, _, _, _) = unsafe { cpuid_raw(0x4000_0001) };
    if feat & (1 << 5) == 0 { return; } // KVM_FEATURE_STEAL_TIME is not declared
    let cpu = crate::percpu::cpu_id();
    if cpu >= MAXCPU { return; }
    let pa = unsafe { core::ptr::addr_of!(STEAL[cpu]) as u64 }; // the kernel is mapped one to one
    unsafe { wrmsr(MSR_KVM_STEAL_TIME, pa | 1); }
    if feat & (1 << 9) != 0 { PV_TLB_FLUSH.store(true, Ordering::SeqCst); } // KVM_FEATURE_PV_TLB_FLUSH
    if !STEAL_ON.swap(true, Ordering::SeqCst) {
        crate::println!("smp: the KVM steal-time is on — one can see how much time the host took{}",
            if PV_TLB_FLUSH.load(Ordering::Relaxed) { " (and one may not wait for an acknowledgement from a neighbour taken off its core)" } else { "" });
    }
}
pub fn steal_on() -> bool { STEAL_ON.load(Ordering::Relaxed) }
/// How many nanoseconds the host took from the core `cpu` since the boot.
pub fn steal_ns(cpu: usize) -> u64 {
    if !steal_on() || cpu >= MAXCPU { return 0; }
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(STEAL[cpu].steal)) }
}
/// Is the vCPU `cpu` taken off its processor RIGHT NOW.
pub fn vcpu_preempted(cpu: usize) -> bool {
    if !steal_on() || cpu >= MAXCPU { return false; }
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(STEAL[cpu].preempted)) & KVM_VCPU_PREEMPTED != 0 }
}
/// Ask the host to flush a preempted neighbour's TLB at its return onto a
/// processor. true — the request has been accepted (there is no need to wait for an acknowledgement from it).
///
/// Linux does the same (`kvm_flush_tlb_multi`): a CAS over the `preempted` field —
/// it is also a check that the neighbour is still taken off. It came back in the meantime — the CAS did
/// not succeed, and we honestly wait for its acknowledgement.
pub fn ask_host_flush(cpu: usize) -> bool {
    if !PV_TLB_FLUSH.load(Ordering::Relaxed) || cpu >= MAXCPU { return false; }
    let p = unsafe { core::ptr::addr_of_mut!(STEAL[cpu].preempted) as *mut u8 };
    let a = unsafe { &*(p as *const core::sync::atomic::AtomicU8) };
    a.compare_exchange(KVM_VCPU_PREEMPTED, KVM_VCPU_PREEMPTED | KVM_VCPU_FLUSH_TLB,
        Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

/// Are we under KVM? (the signature of the hypervisor's CPUID leaves.)
fn kvm_here() -> bool {
    let (_, b, c, d) = unsafe { cpuid_raw(0x4000_0000) };
    b == u32::from_le_bytes(*b"KVMK") && c == u32::from_le_bytes(*b"VMKV") && d == u32::from_le_bytes(*b"M\0\0\0")
}

/// Register THE CURRENT core's PV EOI word with the host. It is called on every core.
pub fn pveoi_enable_here() {
    if !PV_EOI_WANT.load(Ordering::Relaxed) { return; }
    if !kvm_here() { return; } // it is not KVM — there is no protocol
    let (feat, _, _, _) = unsafe { cpuid_raw(0x4000_0001) };
    if feat & (1 << 6) == 0 { return; } // KVM_FEATURE_PV_EOI is not declared
    let pa = crate::percpu::pv_eoi_addr(crate::percpu::cpu_id());
    if pa == 0 { return; }
    unsafe { wrmsr(MSR_KVM_PV_EOI_EN, pa | 1); }
    if !PV_EOI_ON.swap(true, Ordering::SeqCst) {
        crate::println!("smp: the KVM PV EOI is on — an interrupt's acknowledgement without an exit to the host");
    }
}

// Acknowledge an interrupt (`lapic_eoi`). It is the entry point both for Rust and for the interrupt
// stubs, so it is written in assembly: a stub must not clobber
// more than it saved, while the compiler in this function is free to take an xmm (the kernel
// is built with SSE). The clobber is exactly: rax, rcx, rdx, the flags.
core::arch::global_asm!(r#"
.section .text
.global lapic_eoi
lapic_eoi:
    btrq $0, %gs:48          /* PV EOI: has the host already acknowledged it? (we extinguish the bit) */
    jc 1f                    /* yes — there is no exit to the host at all */
    lock incq PV_EOI_MISS(%rip)
    cmpb $0, X2APIC(%rip)
    je 2f
    mov $0x80b, %ecx         /* x2APIC: the EOI is an MSR write (without an emulation of the instruction) */
    xor %eax, %eax
    xor %edx, %edx
    wrmsr
    ret
2:  mov $0xFEE000B0, %rax    /* xAPIC: a write into the MMIO window */
    movl $0, (%rax)
1:  ret
"#, options(att_syntax));

unsafe extern "C" { pub fn lapic_eoi(); }

unsafe fn cpuid_raw(leaf: u32) -> (u32, u32, u32, u32) {
    let r = core::arch::x86_64::__cpuid(leaf);
    (r.eax, r.ebx, r.ecx, r.edx)
}

fn busy_wait(us: u64) {
    let hz = crate::tsc::calibrate_cached();
    let t0 = crate::tsc::rdtsc();
    while crate::tsc::rdtsc() - t0 < hz * us / 1_000_000 { core::hint::spin_loop(); }
}

// ── A10: THE LOGICAL numbering of the cores against the APIC id ─────────────
//
// Before A10 one and the same number played two incompatible roles:
//   * the core's logical number — a dense 0..N-1, it is what the scheduler operates with
//     (`CPU_OF`), the per-CPU areas, the ONLINE mask and — MOST IMPORTANTLY — the workload itself:
//     a `sched_setaffinity(CPU_SET(1))` in an ordinary Linux binary means "the second
//     core" rather than "the core with the APIC id 1";
//   * the APIC id — the core's physical address in the LAPIC, by which an INIT-SIPI and
//     any IPI are addressed.
//
// Under QEMU and on Nitro guests they coincide (the ids are dense: 0, 1, 2, …), so
// the substitution lived unnoticed for three generations. On bare metal the MADT returns SPARSE
// ids (c6i.metal: 0, 2, 4, 6, … — the HT twins are between them), and the coincidence
// falls apart: the kernel brought up an AP with the APIC id 2, created it as "cpu 2", while
// the workload asked for core 1 — the task went to nobody, `ONLINE & (1<<1)`
// was empty, and `sched_setaffinity` silently left the thread on the BSP. And that is A10.
//
// Now the APIC id lives ONLY at the boundary with the LAPIC (`start_ap`, `send_ipi`), while the whole
// rest of the kernel speaks in logical numbers. The logical 0 is always the BSP.
pub const MAXCPU: usize = crate::percpu::MAXCPU;
static NCPU: AtomicU32 = AtomicU32::new(1);
static APIC_OF: [AtomicU32; MAXCPU] = [const { AtomicU32::new(u32::MAX) }; MAXCPU];

/// Build the map "a logical number -> an APIC id" by the MADT. It is called on the BSP
/// AFTER `acpi::init` (without a MADT it is a dense fallback, as it was before A10).
pub fn topology_init() {
    let bsp = bsp_apic_id();
    APIC_OF[0].store(bsp, Ordering::SeqCst);
    let mut n = 1usize;
    let mut skipped = 0usize;
    for i in 0..crate::acpi::nr_cpus() as usize {
        if n >= MAXCPU { break; }
        let id = crate::acpi::apic_id(i);
        if id == bsp { continue; }
        // xAPIC addresses 8-bit ids: a core with an id > 254 cannot be woken by us (255 is
        // a broadcast). Skipping such a core silently is not allowed — that is exactly that class
        // of "we quietly lost a device" which session 9 was paid for.
        if id > 254 { skipped += 1; continue; }
        APIC_OF[n].store(id, Ordering::SeqCst);
        n += 1;
    }
    if n == 1 && crate::acpi::nr_cpus() == 0 {
        // ACPI is not parsed (a microvm without ACPI): a dense numbering from the BSP — exactly
        // the behaviour that was there before A10, and it is correct for QEMU.
        for l in 1..MAXCPU { APIC_OF[l].store(bsp + l as u32, Ordering::SeqCst); }
        n = MAXCPU;
    }
    NCPU.store(n as u32, Ordering::SeqCst);
    let show = n.min(8);
    let mut head = [0u32; 8];
    for l in 0..show { head[l] = APIC_OF[l].load(Ordering::SeqCst); }
    crate::println!("smp: the topology: {} logical cores (the BSP's apic {}) out of {} by the MADT, apic_of[0..{}]={:?}",
        n, bsp, crate::acpi::nr_cpus(), show, &head[..show]);
    if crate::acpi::nr_cpus() as usize > n + skipped {
        crate::println!("smp: ATTENTION: the machine has {} cores, the kernel uses {} (the ceiling percpu::MAXCPU)",
            crate::acpi::nr_cpus(), n);
    }
    if skipped > 0 {
        crate::println!("smp: ATTENTION: {} cores with an APIC id > 254 have been skipped — they need x2APIC", skipped);
    }
    phys_topology(n);
}

/// The width of the SMT level's shift by CPUID leaf 0x0B: `apic_id >> the shift` is the number
/// of the PHYSICAL core. Zero — there are no hyperthreads (or the leaf is unavailable).
fn smt_shift() -> u32 {
    let (maxleaf, ..) = unsafe { cpuid_raw(0) };
    if maxleaf < 0x0B { return 0; }
    let l0 = core::arch::x86_64::__cpuid_count(0x0B, 0);
    if (l0.ecx >> 8) & 0xFF != 1 { return 0; }
    l0.eax & 0x1F
}

/// `talaria.smtpack=1` — OCCUPY THE PHYSICAL CORES ONE BY ONE RATHER THAN SCATTERED.
///
/// WHAT FOR. `talaria.smp=N` takes the FIRST N logical numbers, while the MADT on a
/// `c6i.xlarge` enumerates the vCPUs so that the first two are different physical cores
/// (`apic_of=[0,2,1,3]`). Our waiter does not sleep by construction, which means that with `smp=2`
/// BOTH physical cores of the instance are occupied, and the host is left only with preempting
/// us — with a quantum of ~8 ms once every 5-15 s (session 34: a steal of 249 ms against 2 ms with
/// `smp=1`, and that is the whole p99.99 class in scoreboard 33). With `smp=1` the class disappears,
/// but the ladder's top step is not pulled by one core.
///
/// This knob gives a third variant: two threads of ONE physical core. The order
/// is rearranged so that first come the hyperthreads of the BSP's core, then the other
/// cores (inside — by the MADT's order). The threads' affinity itself knows nothing about the
/// rearrangement: it works with logical numbers, and what changed is only
/// which APIC id a number corresponds to.
fn smt_pack(n: usize) {
    let sh = smt_shift();
    if sh == 0 || n < 2 { return; }
    let mut ids = [0u32; MAXCPU];
    for l in 0..n { ids[l] = APIC_OF[l].load(Ordering::SeqCst); }
    let mut out = [0u32; MAXCPU];
    pack_order(&ids[..n], sh, &mut out[..n]);
    for l in 0..n { APIC_OF[l].store(out[l], Ordering::SeqCst); }
    crate::println!("smp: talaria.smtpack=1 — the logical numbers go by the hyperthreads of one physical core (a whole core is left to the host)");
}

/// THE REARRANGEMENT BY ITSELF (so that it can be judged without such a machine).
///
/// `ids[0]` is the BSP, it stays the logical zero always. Then come first its
/// hyperthreads (the same `id >> sh`), then the rest in the MADT's order. The gate
/// `talaria.test=smtpack` feeds it the `c6i.xlarge` layout (`[0,2,1,3]`) and
/// an already packed one — a second time the rearrangement must change nothing.
pub fn pack_order(ids: &[u32], sh: u32, out: &mut [u32]) {
    let n = ids.len().min(out.len());
    if n == 0 { return; }
    out[0] = ids[0];
    let bsp_core = ids[0] >> sh;
    let mut k = 1usize;
    for &id in &ids[1..n] { if id >> sh == bsp_core { out[k] = id; k += 1; } }
    for &id in &ids[1..n] { if id >> sh != bsp_core { out[k] = id; k += 1; } }
}

/// The gate `talaria.test=smtpack`: the rearrangement on synthetic layouts.
pub fn selftest_pack() {
    let mut out = [0u32; 4];
    pack_order(&[0, 2, 1, 3], 1, &mut out);
    let a = out == [0, 1, 2, 3];
    crate::println!("smtpack: the c6i.xlarge [0,2,1,3] -> {:?} (we expect [0,1,2,3]): {}", out, if a { "OK" } else { "FAILED" });
    let mut out2 = [0u32; 4];
    pack_order(&[0, 1, 2, 3], 1, &mut out2);
    let b = out2 == [0, 1, 2, 3];
    crate::println!("smtpack: the already packed [0,1,2,3] -> {:?} (the rearrangement is idempotent): {}", out2, if b { "OK" } else { "FAILED" });
    // Four cores without hyperthreads (the shift 2 — two threads per core on another
    // machine): the BSP's core is the first one, the rest in the MADT's order.
    let mut out3 = [0u32; 4];
    pack_order(&[4, 8, 5, 9], 1, &mut out3);
    let c = out3 == [4, 5, 8, 9];
    crate::println!("smtpack: the ids not from zero [4,8,5,9] -> {:?} (we expect [4,5,8,9]): {}", out3, if c { "OK" } else { "FAILED" });
    crate::println!("smtpack: {}", if a && b && c { "ALL OK" } else { "FAILED — the rearrangement of the hyperthreads is wrong" });
}

/// THE LAYOUT OF THE LOGICAL CORES OVER THE PHYSICAL ONES (session 35, step 2).
///
/// WHAT FOR. Session 34 showed: with `talaria.smp=2` the host preempts us with a quantum of
/// ~8 ms once every 5-15 s (a steal of 249 ms against 2 ms with `smp=1`), because two
/// of our waiters occupy BOTH physical cores of a `c6i.xlarge`. The question "and what if both
/// our vCPUs are seated on ONE physical core (a whole one is left to the host)"
/// was settled by guessing from the line `apic_of=[0,2,1,3]`. Now the layout
/// is printed: CPUID leaf 0x0B gives the width of the SMT level's shift, that is, the number
/// of the PHYSICAL core = `apic_id >> the shift` and the number of the thread in it = the remainder.
///
/// Without leaf 0x0B (an old hypervisor) an honest "unknown" is printed rather than
/// a guess by the parity of the id.
fn phys_topology(n: usize) {
    let sh = smt_shift();
    if sh == 0 {
        crate::println!("smp: the layout: the hyperthreads are not visible (CPUID 0x0B), {} logical = {} physical cores", n, n);
        return;
    }
    let smt_shift = sh;
    let show = n.min(8);
    let mut cores = [0u32; 8];
    let mut threads = [0u32; 8];
    for l in 0..show {
        let id = APIC_OF[l].load(Ordering::SeqCst);
        cores[l] = id >> smt_shift;
        threads[l] = id & ((1 << smt_shift) - 1);
    }
    // How many DIFFERENT physical cores our logical ones occupy — and that is
    // the answer to "have we left the host a whole core".
    let mut uniq = 0usize;
    for l in 0..show {
        let mut seen = false;
        for k in 0..l { if cores[k] == cores[l] { seen = true; break; } }
        if !seen { uniq += 1; }
    }
    crate::println!("smp: the layout by the physical cores (CPUID 0x0B, the SMT shift {}): the logical ones 0..{} -> the cores {:?} the threads {:?}; DIFFERENT physical cores {}",
        smt_shift, show, &cores[..show], &threads[..show], uniq);
}

/// How many logical cores are known (>=1: the BSP is always there).
pub fn nr_cpus() -> usize { NCPU.load(Ordering::SeqCst) as usize }

/// The APIC id of a logical core (the fallback is the number itself: before `topology_init`).
pub fn apic_of(cpu: usize) -> u32 {
    if cpu >= MAXCPU { return cpu as u32; }
    let v = APIC_OF[cpu].load(Ordering::SeqCst);
    if v == u32::MAX { cpu as u32 } else { v }
}

/// The logical number of a core by its APIC id (u32::MAX — the core is not in the map).
pub fn logical_of(apic: u32) -> usize {
    for l in 0..MAXCPU {
        if APIC_OF[l].load(Ordering::SeqCst) == apic { return l; }
    }
    usize::MAX
}

/// THE CURRENT core's logical number by the LAPIC (before `percpu::init`, where GS.base is not
/// set yet and `percpu::cpu_id()` must not be called).
pub fn my_logical() -> usize {
    let l = logical_of(unsafe { apic_id_here() });
    if l == usize::MAX { 0 } else { l }
}

/// Bring up the LOGICAL core cpu (1..nr_cpus()). A wrapper over `start_ap` so that
/// the APIC id does not leak into the calling code.
pub fn start_cpu(cpu: usize) -> bool {
    if cpu == 0 || cpu >= nr_cpus() { return false; }
    start_ap(apic_of(cpu))
}


// ── A10: where an AP stalled (the diagnostics on a machine without a console) ─
// An AP cannot print by itself: a `println!` on bare metal goes into the trace on the disk, while
// there it is written by the NVMe queue brought up by the BSP. So an AP only marks THE NUMBER
// of the step it passed, while it is printed by the BSP — at a timeout of `start_ap`.
pub static AP_STAGE: AtomicU32 = AtomicU32::new(0);
const AP_STAGES: [&str; 9] = [
    "the trampoline did not hand control over (a real mode -> a long mode)",
    "the entry into ap_entry (the long mode)", "the IDT", "CR4/XCR0/CR0", "the PAT", "it marked itself alive",
    "the LAPIC is on", "the per-CPU area", "the AP's mode is chosen",
];

/// Bring up an AP with a given APIC ID. It returns true if the core marked itself alive.
pub fn start_ap(apic_id: u32) -> bool {
    // The logical number is needed before any allocations: without it there is nothing to tie
    // the pickup loop's stack to, and there is no point in allocating anything.
    let lg = logical_of(apic_id);
    if lg >= MAXCPU {
        crate::println!("smp: the APIC id {} is outside the topology map — we do not bring the AP up", apic_id);
        return false;
    }
    unsafe {
        // 1. prepare the trampoline: copy the code, write in the CR3 and the stack
        let cr3: u64; core::arch::asm!("mov {}, cr3", out(reg) cr3);
        // 16 KB: run_cell's locals (rx_slot/tx_slot/tx_len with QS=256) are >6 KB; one
        // page overflowed into the neighbouring frames (it was caught as garbage in the TX ring)
        // §1.1: 32 KB of working stack + A GUARD PAGE under it. 128 KB were needed
        // while the virtio device travelled by value and was copied at every step
        // (run_datapath -> run_cell -> NicQ::new) — now the card is passed by
        // reference. The guard is obligatory: last time the stack went below the bottom into the cell's common
        // region and that looked like a corruption of the scheduler (a #UD with rip=3)
        // rather than like a stack overflow. Now there will be an honest #PF at the guard's address.
        let sbase = crate::mem::alloc_contig(9).unwrap();
        crate::paging::guard(sbase);
        let stack = sbase + 9 * 4096; // the top of the AP's stack
        AP_STACK_TOP.store(stack, Ordering::SeqCst);
        // The pickup loop's stack of the same core (AP_MODE=2): 4 pages + a guard.
        // We allocate it HERE, on the BSP and before the INIT: an AP enters ap_loop already with a ready
        // stack, while in .bss it occupies nothing (it was 16 KB × MAXCPU).
        let lbase = crate::mem::alloc_contig(5).unwrap();
        crate::paging::guard(lbase);
        crate::sched::set_ap_stack(lg, lbase + 5 * 4096);

        let src = tramp_start as *const () as *const u8;
        let len = (tramp_end as *const () as usize) - (tramp_start as *const () as usize);
        core::ptr::copy_nonoverlapping(src, TRAMP as *mut u8, len);
        // the patching of the parameters in the trampoline's data area (see the offsets in the asm)
        let cr3_off = (tramp_cr3 as *const () as usize) - (tramp_start as *const () as usize);
        let stack_off = (tramp_stack as *const () as usize) - (tramp_start as *const () as usize);
        let entry_off = (tramp_entry as *const () as usize) - (tramp_start as *const () as usize);
        core::ptr::write_volatile((TRAMP + cr3_off as u64) as *mut u64, cr3);
        core::ptr::write_volatile((TRAMP + stack_off as u64) as *mut u64, stack);
        core::ptr::write_volatile((TRAMP + entry_off as u64) as *mut u64, ap_entry as *const () as u64);

        // the reference of the BSP's state for the APs
        {
            let (cr0, cr4): (u64, u64);
            core::arch::asm!("mov {}, cr0", out(reg) cr0);
            core::arch::asm!("mov {}, cr4", out(reg) cr4);
            BSP_CR0.store(cr0, Ordering::SeqCst);
            BSP_CR4.store(cr4, Ordering::SeqCst);
            if cr4 & (1 << 18) != 0 { // OSXSAVE -> xgetbv is possible
                let (lo, hi): (u32, u32);
                core::arch::asm!("xgetbv", in("ecx") 0u32, out("eax") lo, out("edx") hi);
                BSP_XCR0.store(((hi as u64) << 32) | lo as u64, Ordering::SeqCst);
            }
        }
        // A snapshot of THE COUNTER of the live ones BEFORE the INIT: an AP may mark itself faster than the BSP
        // reaches the waiting loop (taking it after the SIPI means a lost increment).
        let before = AP_ALIVE.load(Ordering::SeqCst);
        AP_STAGE.store(0, Ordering::SeqCst);
        // 2. INIT
        icr_write(apic_id, 0x0000_4500); // INIT assert
        busy_wait(10_000);
        // 3. a SIPI x2 (the vector 0x08 -> 0x8000)
        for _ in 0..2 {
            icr_write(apic_id, 0x0000_4600 | 0x08);
            busy_wait(200);
        }
        // 4. wait for an increment of the counter of the live ones (the second and the subsequent APs: it is already non-zero)
        let t0 = crate::tsc::rdtsc();
        let hz = crate::tsc::calibrate_cached();
        while AP_ALIVE.load(Ordering::SeqCst) == before {
            if crate::tsc::rdtsc() - t0 > hz { // a 1 s timeout
                // A10: say WHERE the AP stalled. Without that "the AP did not come up" is
                // exactly one line for the whole path from the INIT to ap_loop, and on
                // a machine without a console choosing between "the SIPI went into nowhere" and
                // "it fell on the cloning of CR4" has to be done with runs of $2 each.
                let st = AP_STAGE.load(Ordering::SeqCst) as usize;
                crate::println!("smp: the AP apic {} did not mark itself within 1 s — the last step {}: {}",
                    apic_id, st, AP_STAGES[st.min(AP_STAGES.len() - 1)]);
                return false;
            }
            core::hint::spin_loop();
        }
        // The A12 remainder: "the core is alive" and "the core is READY" are not the same thing. An AP
        // marks itself alive RIGHT AFTER the cloning of the state, while it gets into the ONLINE mask
        // later, having passed the per-CPU area and the mode. A return by the first
        // event gave a race of the report: on a `c6i.metal` with `talaria.smp=16` all
        // the 15 APs started successfully, while the mask was printed as 0x7fff — the last one
        // simply did not manage to write itself. Worse than the report is that a workload that asked for
        // a core right after the start would get an EINVAL on a live core.
        // So for the scheduler's modes we wait exactly for THE READINESS.
        let mode = AP_MODE.load(Ordering::SeqCst);
        if mode == 1 || mode == 2 {
            let t1 = crate::tsc::rdtsc();
            while crate::percpu::ONLINE.load(Ordering::SeqCst) & (1 << lg) == 0 {
                if crate::tsc::rdtsc() - t1 > hz {
                    crate::println!("smp: the AP apic {} came alive, but it did not get into ONLINE within 1 s — the step {}",
                        apic_id, AP_STAGE.load(Ordering::SeqCst));
                    return false;
                }
                core::hint::spin_loop();
            }
        }
        // The same question a step further (V1.9). For the scheduler "the core
        // is ready" means "the core TAKES TASKS", that is, it reached the pickup loop
        // and marked itself in `sched::SCHED_CPUS`. Between ONLINE and the pool there are also the GDT/TSS and
        // the syscall setup, and a layout computed in that interval simply
        // would not see the core — the thread would stay on the control plane, while in
        // the log there would stand "the AP came up".
        if mode == 2 {
            let t2 = crate::tsc::rdtsc();
            while !crate::sched::sched_cpu(lg) {
                if crate::tsc::rdtsc() - t2 > hz {
                    crate::println!("smp: the AP apic {} is in ONLINE, but it did not reach the pickup loop within 1 s — the step {}",
                        apic_id, AP_STAGE.load(Ordering::SeqCst));
                    return false;
                }
                core::hint::spin_loop();
            }
        }
        let _ = lapic_read;
        true
    }
}

/// A12: what the scheduler's LVT timer was configured with (0 — it is not configured).
///
/// A core's LVT timer register is ONE, and the wait tier (`sync.rs`) takes the same one for a
/// TSC-deadline when it needs to wake up by time. When removing its deadline, it
/// previously simply MASKED the register — and the scheduler's periodic timer
/// died forever. On Nitro guests that did not surface (a TSC-deadline is not
/// declared there, the path is not taken), while on a `c6i.metal` it cost the production mode: after
/// the very first wait by time there was no preemption any more, a thread waited for its
/// partners, whom nobody gave a core, and from the outside that looked like
/// "the workload is silent". So we remember the timer's configuration and restore it.
/// PER CORE: the LAPIC timer is a core's register, and it is set up on every core separately.
/// While the scheduler lived only on the BSP, one cell was enough; with the cores of the common
/// pool an AP that stopped its own timer would zero a foreign one's configuration — and after
/// the very first wait by time there would be no preemption where there is one.
static TIMER_INITIAL: [core::sync::atomic::AtomicU32; MAXCPU] =
    [const { core::sync::atomic::AtomicU32::new(0) }; MAXCPU];
fn my_timer_slot() -> &'static core::sync::atomic::AtomicU32 {
    &TIMER_INITIAL[crate::percpu::cpu_id().min(MAXCPU - 1)]
}

/// Configure the periodic LAPIC timer (the vector 0x41) on the current core.
/// Stop the LAPIC timer (before a restart of the workload by the supervisor).
pub fn lapic_timer_stop() {
    my_timer_slot().store(0, Ordering::Relaxed);
    unsafe { lapic_write(0x380, 0); } // initial count = 0
}
/// `talaria.slice=N` — an override of the timer's initial counter (0 — as it is).
pub static TIMER_SLICE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The ticks of the LAPIC timer per second with the divisor /16 (0 — it has not been measured yet).
///
/// Measuring that IS OBLIGATORY. The initial counter is not nanoseconds: the LAPIC counts
/// at its own frequency, and it is different everywhere. Under KVM the APIC bus goes at 1 GHz, on a live
/// chip it comes from the reference crystal (often 100 MHz). One and the same hard-wired counter
/// 500000 with /16 gives 8 ms under a hypervisor and 80 ms on metal — while in
/// the code it reads as "500 us". Exactly that is what happened to us: the scheduler's slice was 8 ms
/// instead of the intended 500 us, and a thread that was not given the processor directly
/// waited for its turn for milliseconds. On a ClickHouse query in one thread that
/// cost a fifteenfold lag behind Linux.
pub static LAPIC_HZ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Measure the LAPIC timer's frequency by the TSC (once, on the BSP).
pub fn lapic_calibrate() {
    if LAPIC_HZ.load(Ordering::Relaxed) != 0 { return; }
    let hz = crate::tsc::calibrate_cached();
    if hz == 0 { return; }
    unsafe {
        lapic_write(0x3E0, 0x3);                      // the divisor /16
        lapic_write(0x320, 0x1_0000 | 0xFE);          // the LVT timer: one-shot, a masked vector is not needed
        lapic_write(0x380, 0xFFFF_FFFF);              // we count down from the maximum
        let t0 = crate::tsc::rdtsc();
        let c0 = lapic_read(0x390);
        let want = hz / 100;                          // a window of 10 ms
        while crate::tsc::rdtsc().wrapping_sub(t0) < want { core::hint::spin_loop(); }
        let c1 = lapic_read(0x390);
        let dt = crate::tsc::rdtsc().wrapping_sub(t0);
        lapic_write(0x380, 0);                        // stop
        let ticks = c0.wrapping_sub(c1) as u64;       // the counter goes DOWN
        if ticks == 0 || dt == 0 { return; }
        let lhz = ticks.saturating_mul(hz) / dt;
        LAPIC_HZ.store(lhz, Ordering::SeqCst);
        crate::println!("smp: the LAPIC timer is {} MHz with /16 (the slice is set in nanoseconds)", lhz / 1_000_000);
    }
}

/// The scheduler's slice by default, nanoseconds.
///
/// Half a millisecond is A MEASUREMENT rather than a taste. A sweep by the slice on the full ClickHouse
/// suite (`services/clickhouse/sweep.sh`, 4 cores, the median):
/// 8 ms — 308 ms per suite, 2 ms — 246, 0.5 ms — 216, 0.2 ms — 227. Below
/// half a millisecond the gain is eaten by the preemption itself (a growth on the rows
/// `uniqExact` and `LIKE` is visible at once), above it a thread waits for a foreign quantum.
///
/// The former eight milliseconds were not a choice but an inheritance: exactly that came out
/// under QEMU from the hard-wired LAPIC counter when the slice was still set by a
/// counter rather than by time. They were held by one bug — on a short slice the gate
/// `tsync` stalled — and it is closed: a waiter whose condition is fulfilled is now visible
/// to the scheduler by itself (`sync::ready_now`).
pub const SLICE_NS: u32 = 500_000;

/// A SHORT SLICE ON A CORE WHERE SOMEBODY WAITS FOR THE NETWORK (nanoseconds; 0 — off).
/// The knob `talaria.netslice=<us>`.
///
/// Why a separate slice. There are no interrupts from the card in this build: the stack
/// is driven ONLY from the syscalls, that is, a frame is seen by the thread that waits
/// for it in a `recv`/`epoll_wait`. While the core is held by a COMPUTING neighbour, the frame will be
/// seen by nobody, and the delivery's latency is bounded from below by the slice. In the measurement that
/// looks unambiguous: mongod's p50 is 69 us (better than Linux's), while its p99 is 530, that
/// is, exactly the quantum.
///
/// Cutting the slice for the whole machine is not allowed — it was chosen by a measurement of the
/// throughput (see SLICE_NS). So it is cut ONLY on that core and only
/// for the time while somebody there really waits for the network (`netsys::poller_active`),
/// and it comes back by itself.
pub static NETSLICE_NS: AtomicU64 = AtomicU64::new(100_000);

pub fn netslice_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::value(cmdline, "talaria.netslice") {
        if let Ok(us) = v.parse::<u64>() {
            NETSLICE_NS.store(us * 1000, Ordering::SeqCst);
            crate::println!("smp: the slice on a core with a network waiter is {} us{}", us,
                if us == 0 { " (it is off: the slice is the common one)" } else { "" });
        }
    }
}

/// A LONG TICK PERIOD ON A CORE WHERE THERE IS NOBODY TO PREEMPT (nanoseconds; 0 — off).
/// The knob `talaria.idleslice=<us>`.
///
/// A tick is A PREEMPTION. When there is exactly one ready task on a core, there is nobody
/// to preempt, and the whole tick goes on the housekeeping: the acknowledgement of the interrupt, the clock,
/// the cleanup, the accounting. It is paid for two thousand times a second meanwhile and costs about
/// two microseconds — that is, on a latency-critical thread it is exactly that tail
/// which is visible in the measurement and which Linux does not have.
///
/// The measurement (`actors pingpong raw`, 100 thousand turnarounds, nanoseconds):
///   the slice 0.5 ms — p50 213, p99 248, **p99.9 3767**
///   the slice 4 ms   — p50 229, p99 316, **p99.9 463**   (with Linux 300-500)
/// That is, the whole tail is the tick, and it is removed by thinning rather than by cheapening.
///
/// A HYSTERESIS IS OBLIGATORY. The sign "there is nobody to preempt" with a ping-pong flickers at
/// every turnaround (the partner is now ready, now asleep), while every reconfiguration is
/// a write into the LAPIC. So the period is lengthened only after `SOLO_HOLD` ticks in
/// a row in solitude, and is shortened at the very first observation of a neighbour.
///
/// WHY 2 MS. The break because of which the default was first set at 700 us
/// (the bistable collapse of `actors fanout tsync spread`) turned out NOT to be a property
/// of the long tick: the long tick was the trigger, while what fired was WORK STEALING —
/// it took a task from a core with two onto a core with one, that is, it turned
/// the skew over instead of smoothing it (see `affinity::steal_candidate`).
/// After the steal was fixed there is no collapse either at 2000 or at 4000 (eight runs
/// in a row), while the tail at 2 ms is better than at 700: `pingpong raw` p99.9 268 ns
/// against 473-579.
pub static IDLESLICE_NS: AtomicU64 = AtomicU64::new(2_000_000);
const SOLO_HOLD: u32 = 4;
static SOLO_RUN: [AtomicU32; MAXCPU] = [const { AtomicU32::new(0) }; MAXCPU];
/// The core is on a long tick right now — a waker must ring it explicitly (see sync.rs).
static LONG_TICK: [AtomicU32; MAXCPU] = [const { AtomicU32::new(0) }; MAXCPU];
static LAST_RETUNE: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
pub fn long_tick(cpu: usize) -> bool {
    cpu < MAXCPU && LONG_TICK[cpu].load(Ordering::Relaxed) != 0
}

pub fn idleslice_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::value(cmdline, "talaria.idleslice") {
        if let Ok(us) = v.parse::<u64>() {
            IDLESLICE_NS.store(us * 1000, Ordering::SeqCst);
            crate::println!("smp: a tick on a core without neighbours goes once every {} us{}", us,
                if us == 0 { " (it is off: the period is the common one)" } else { "" });
        }
    }
}

/// The tick's period that this core WANTS right now.
/// `solo` means "there are no other ready tasks on this core".
pub fn want_period_ns(cpu: usize, solo: bool) -> u64 {
    let c = cpu.min(MAXCPU - 1);
    let eff = effective_slice_ns();
    let ns = NETSLICE_NS.load(Ordering::Relaxed);
    if ns != 0 && ns < eff && crate::netsys::poller_active(cpu) {
        SOLO_RUN[c].store(0, Ordering::Relaxed);
        return ns;
    }
    let idle = IDLESLICE_NS.load(Ordering::Relaxed);
    // The profile by the tick (`talaria.prof=`) samples EXACTLY by the tick: to thin it
    // means to coarsen the profile fourfold exactly when it has been switched on.
    if !solo || idle <= eff || crate::sched::PROF_ON.load(Ordering::Relaxed) {
        SOLO_RUN[c].store(0, Ordering::Relaxed);
        return eff;
    }
    if SOLO_RUN[c].fetch_add(1, Ordering::Relaxed) + 1 >= SOLO_HOLD { idle } else { eff }
}

/// Reconfigure THIS core's tick period if it must become a different one.
///
/// We write into the LAPIC only when the value really changes: on a stand without APICv
/// a write into the LAPIC's window is an exit to the host with an emulation of the instruction (10 us), and
/// an unconditional reconfiguration at every tick would cost more than everything it
/// saves. The sign changes rarely (a workload is either a network one or not).
pub fn retune_timer(period_ns: u64) {
    let c = crate::percpu::cpu_id().min(MAXCPU - 1);
    let lhz = LAPIC_HZ.load(Ordering::Relaxed);
    if lhz == 0 { return; }
    let slot = my_timer_slot();
    if slot.load(Ordering::Relaxed) == 0 { return; }   // there is no tick on this core at all
    if TIMER_REM[c].load(Ordering::Relaxed) != 0 { return; } // the LVT is borrowed right now for a TSC-deadline
    let want = ((lhz as u128 * period_ns as u128 / 1_000_000_000u128) as u64).clamp(16, u32::MAX as u64) as u32;
    if slot.load(Ordering::Relaxed) == want { return; }
    // NOT MORE OFTEN THAN ONCE EVERY FOUR BASE PERIODS. A write into the initial value's
    // register RESTARTS the count: a reconfiguration more often than the period itself
    // means that the tick will NEVER come — the preemption on the core dies silently.
    // It was caught as a twofold bistable collapse of `actors fanout tsync spread`:
    // 1.5 us per message against 25 us, reproducibly and only with a long
    // tick. A fan-out's sign of "solitude" flickers at every message, an entry into
    // the scheduler by a waker's call is "a hardware tick" too, and the recharging
    // went faster than the counter managed to count down.
    let now = crate::tsc::rdtsc();
    let thz = crate::tsc::calibrate_cached();
    if thz != 0 {
        let guard_cy = (thz / 1_000_000).max(1) * (effective_slice_ns() * 4 / 1000).max(1);
        let last = LAST_RETUNE[c].load(Ordering::Relaxed);
        if last != 0 && now.wrapping_sub(last) < guard_cy { return; }
        LAST_RETUNE[c].store(now, Ordering::Relaxed);
    }
    slot.store(want, Ordering::Relaxed);
    LONG_TICK[c].store(if period_ns > effective_slice_ns() { 1 } else { 0 }, Ordering::Relaxed);
    RETUNES.fetch_add(1, Ordering::Relaxed);
    unsafe {
        lapic_write(0x3E0, 0x3);
        lapic_write(0x320, (crate::idt::TIMER_VECTOR as u32) | 0x2_0000); // periodic
        lapic_write(0x380, want);
    }
}
/// How many times the tick's period was reconfigured (for `stat:`): a large number is
/// a sign of chatter, and that costs writes into the LAPIC.
pub static RETUNES: AtomicU64 = AtomicU64::new(0);

/// The effective slice: the knob `talaria.slice=<ns>`, otherwise the default. The decisions
/// that depend on the quantum's length (for example "do not steal a cache-hot task")
/// must ask exactly it rather than a constant — otherwise the knob changes the scheduler's
/// behaviour only halfway.
pub fn effective_slice_ns() -> u64 {
    let over = TIMER_SLICE.load(Ordering::Relaxed);
    if over != 0 { over as u64 } else { SLICE_NS as u64 }
}

/// A self-test of the timer's calibration (`talaria.test=slice`).
///
/// One thing is checked: that a given slice is TIME rather than a counter. It is exactly here
/// that the kernel lived with an error for its whole life: `lapic_timer_init(500_000)` read as
/// "500 us", while it was the LAPIC's initial counter, whose frequency is its own on every
/// machine. Under QEMU that came out as 8 ms, on a `c6i.metal` as 320: one and the same number
/// meant different things by a factor of forty, and there was nothing to notice that without a measurement.
///
/// We measure directly: we charge the timer in one-shot for a known NUMBER OF NANOSECONDS and
/// look by the TSC at how much really passed. Plus we verify that
/// `lapic_timer_init` programs a counter corresponding to the slice.
pub fn slice_selftest() -> bool {
    let mut ok = true;
    let lhz = LAPIC_HZ.load(Ordering::Relaxed);
    let hz = crate::tsc::calibrate_cached();
    if lhz == 0 || hz == 0 {
        crate::println!("slice-test: FAIL — the LAPIC's frequency has not been measured (lapic={} tsc={})", lhz, hz);
        return false;
    }
    crate::println!("slice-test: the LAPIC is {} MHz with /16, the TSC is {} MHz, the default slice is {} ns",
        lhz / 1_000_000, hz / 1_000_000, SLICE_NS);
    // 1) one-shot for 10 ms: how much that is in the TSC in reality
    let want_ns: u64 = 10_000_000;
    let count = (lhz as u128 * want_ns as u128 / 1_000_000_000u128) as u32;
    let t0 = crate::tsc::rdtsc();
    unsafe {
        lapic_write(0x3E0, 0x3);
        lapic_write(0x320, 0x1_0000 | 0xFE);   // one-shot, masked: we count ourselves
        lapic_write(0x380, count);
        while lapic_read(0x390) != 0 { core::hint::spin_loop(); }
    }
    let got_ns = crate::tsc::rdtsc().wrapping_sub(t0) * 1_000_000_000 / hz;
    let err = got_ns.abs_diff(want_ns);
    crate::println!("slice-test: {} ns were ordered, {} ns were counted (the error is {} %)",
        want_ns, got_ns, err * 100 / want_ns);
    if err * 100 / want_ns > 5 { crate::println!("slice-test: FAIL — the timer counted the wrong time"); ok = false; }
    // 2) the counter the slice is programmed with must correspond to IT.
    // The effective slice is the knob `talaria.slice=` (also in nanoseconds), otherwise
    // the default; we check what is really programmed.
    let over = TIMER_SLICE.load(Ordering::Relaxed);
    let eff_ns = if over != 0 { over } else { SLICE_NS };
    // We program the EFFECTIVE slice rather than the default constant: the verification below
    // goes against `eff_ns`, and with a given `talaria.slice=` the test would compare
    // the default's counter with the expectation from the knob — that is, it would fail on a healthy
    // machine. The regression did not catch that: it runs `slice` without an override.
    lapic_timer_init(eff_ns);
    let initial = unsafe { lapic_read(0x380) };
    let want_cnt = (lhz as u128 * eff_ns as u128 / 1_000_000_000u128) as u64;
    let d = (initial as u64).abs_diff(want_cnt);
    crate::println!("slice-test: the effective slice {} ns -> the counter {} (we expected {})", eff_ns, initial, want_cnt);
    if want_cnt == 0 || d * 100 / want_cnt > 2 {
        crate::println!("slice-test: FAIL — the counter does not correspond to the slice (is the slice set NOT by time?)");
        ok = false;
    }
    // 3) the tick's period in milliseconds — so that the number is visible by eye in the log
    crate::println!("slice-test: the scheduler's tick goes once every {} us", eff_ns / 1000);
    if ok { crate::println!("slice-test: ALL OK"); } else { crate::println!("slice-test: FAILED"); }
    ok
}

/// Start the scheduler's periodic timer with the period `period_ns`.
/// While the LAPIC's frequency has not been measured, `period_ns` is interpreted as a counter (the old
/// behaviour) — otherwise the first call at the early bring-up would be left without a timer.
pub fn lapic_timer_init(period_ns: u32) {
    let over = TIMER_SLICE.load(Ordering::Relaxed);
    let period_ns = if over != 0 { over } else { period_ns };
    let lhz = LAPIC_HZ.load(Ordering::Relaxed);
    let initial = if lhz == 0 { period_ns } else {
        let c = (lhz as u128 * period_ns as u128 / 1_000_000_000u128) as u64;
        c.clamp(16, u32::MAX as u64) as u32
    };
    my_timer_slot().store(initial, Ordering::Relaxed);
    unsafe {
        lapic_write(0x3E0, 0x3); // divide config: /16
        lapic_write(0x320, (crate::idt::TIMER_VECTOR as u32) | 0x2_0000); // LVT timer: periodic
        lapic_write(0x380, initial); // initial count
    }
}

/// A12: read a LAPIC register (the diagnostics of the timer's state in the log).
pub fn lapic_peek(reg: u64) -> u32 { unsafe { lapic_read(reg) } }
/// Write a LAPIC register from outside the module (in the right mode — xAPIC or x2APIC).
pub fn lapic_poke(reg: u64, val: u32) { unsafe { lapic_write(reg, val); } }

/// A12: is a vector's IRR bit raised (an interrupt IS WAITING to be accepted). A raised IRR
/// with a zero counter means "there is an interrupt, but there is nobody to take it" —
/// that is, IF or TPR rather than the timer.
pub fn irr_pending(vector: u8) -> bool {
    let reg = 0x200 + (vector as u64 / 32) * 0x10;
    unsafe { lapic_read(reg) & (1 << (vector % 32)) != 0 }
}

/// A12: is a vector's ISR bit raised (the servicing is not closed by an EOI). A stuck
/// bit at the timer's vector means that the LAPIC will NEVER deliver that
/// vector again — there is no preemption. The diagnostics for the pulse: the reason is visible at once rather than
/// after three runs of $3 each.
pub fn isr_pending(vector: u8) -> bool {
    let reg = 0x100 + (vector as u64 / 32) * 0x10;
    unsafe { lapic_read(reg) & (1 << (vector % 32)) != 0 }
}

/// Return the scheduler's timer after the wait tier borrowed the LVT for a
/// TSC-deadline. If there was no periodic timer on this core (the AP works
/// run-to-completion, without preemption) — we simply mask it, as before.
/// Is the scheduler's periodic timer spinning on THIS core.
///
/// It must be asked by anyone who is going to borrow the LVT for a TSC-deadline: the LAPIC's
/// timer has no way of "pausing and continuing", and the return
/// (`lapic_timer_restore`) starts the counter FROM ZERO. Which means a core that sleeps by
/// time more often than once per slice NEVER REACHES the end of the slice — it has no tick,
/// no rotation, no preemption. From the outside that looks not like
/// "a broken timer" but like "the threads on this core somehow do not spread out":
/// in `stat: the ticks per core` the BSP had 1800, while the secondary cores had 17-500.
pub fn timer_running() -> bool { my_timer_slot().load(Ordering::Relaxed) != 0 }
/// THIS core's current tick period in nanoseconds (0 — there is no tick). It is needed by
/// the wait tier: a deadline further than one period will be served by the tick itself.
pub fn my_period_ns() -> u64 {
    let slot = my_timer_slot().load(Ordering::Relaxed) as u64;
    let lhz = LAPIC_HZ.load(Ordering::Relaxed);
    if slot == 0 || lhz == 0 { return 0; }
    ((slot as u128 * 1_000_000_000u128) / lhz as u128) as u64
}
/// The same about A FOREIGN core (only for the diagnostics: printing a summary).
pub fn timer_armed_on(c: usize) -> bool { c < MAXCPU && TIMER_INITIAL[c].load(Ordering::Relaxed) != 0 }

// THE BORROWING OF THE LVT TIMER: how much of the slice was left and when it was occupied.
//
// A wait by time on a secondary core borrows the LVT for a TSC-deadline, and that
// was A SILENT KILLER OF THE PREEMPTION: the return started the periodic counter FROM ZERO,
// so a core sleeping by time more often than once per slice NEVER reached the end of the
// slice. There is no rotation on it, no preemption, while the timer meanwhile
// "works" — in `stat: the ticks per core` the BSP had 1800, the secondary ones 17-500.
//
// We fix it honestly: at the borrowing we remember THE REMAINDER of the counter and a TSC mark, at the return
// we start a one-shot for exactly the remainder minus what was slept, while the periodic mode
// is restored by the tick itself (`timer_tick_reload`). Pausing and continuing
// is impossible with the LAPIC (in the periodic mode the counter is recharged FROM the initial
// value's register, that is, the first period cannot be made shorter than the rest) —
// so it is the pair "a one-shot for the remainder + a restart from the handler".
static TIMER_REM: [AtomicU32; MAXCPU] = [const { AtomicU32::new(0) }; MAXCPU];
static TIMER_BORROW_TSC: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
static TIMER_ONESHOT: [AtomicU32; MAXCPU] = [const { AtomicU32::new(0) }; MAXCPU];

/// Occupy the LVT timer: remember the slice's remainder. It is called BEFORE the reprogramming of
/// the LVT into the TSC-deadline mode. A repeated borrowing without a return does not spoil the remainder.
pub fn timer_borrow() {
    let c = crate::percpu::cpu_id().min(MAXCPU - 1);
    if my_timer_slot().load(Ordering::Relaxed) == 0 { return; } // there is no tick here — there is nobody to borrow from
    if TIMER_REM[c].load(Ordering::Relaxed) != 0 { return; }    // it is already borrowed
    let rem = unsafe { lapic_read(0x390) };                     // the current count (it goes down)
    TIMER_REM[c].store(rem.max(1), Ordering::Relaxed);
    TIMER_BORROW_TSC[c].store(crate::tsc::rdtsc(), Ordering::Relaxed);
}

/// The tick has come: if the timer stood in one-shot on the remainder — restore the periodic
/// mode. It is called from the tick's handler on EVERY core.
pub fn timer_tick_reload() {
    let c = crate::percpu::cpu_id().min(MAXCPU - 1);
    if TIMER_ONESHOT[c].swap(0, Ordering::Relaxed) == 0 { return; }
    let initial = my_timer_slot().load(Ordering::Relaxed);
    if initial == 0 { return; }
    unsafe {
        lapic_write(0x3E0, 0x3);
        lapic_write(0x320, (crate::idt::TIMER_VECTOR as u32) | 0x2_0000); // periodic
        lapic_write(0x380, initial);
    }
}

pub fn lapic_timer_restore() {
    let initial = my_timer_slot().load(Ordering::Relaxed);
    if initial == 0 {
        unsafe { lapic_write(0x320, 0x1_0000); } // LVT timer masked
        return;
    }
    let c = crate::percpu::cpu_id().min(MAXCPU - 1);
    let rem = TIMER_REM[c].swap(0, Ordering::Relaxed);
    if rem != 0 {
        // How much of the slice the sleep ate. LAPIC_HZ is the frequency already AFTER the /16 divisor,
        // so the counters are converted from the TSC's cycles by a direct ratio of the frequencies.
        let lhz = LAPIC_HZ.load(Ordering::Relaxed);
        let thz = crate::tsc::calibrate_cached();
        let spent = if lhz == 0 || thz == 0 { 0 } else {
            (crate::tsc::rdtsc().wrapping_sub(TIMER_BORROW_TSC[c].load(Ordering::Relaxed)) as u128
                * lhz as u128 / thz as u128).min(u32::MAX as u128) as u32
        };
        let left = rem.saturating_sub(spent);
        // The slice was SLEPT THROUGH to the end — we start a new one entirely rather than fire a tick
        // immediately. The task slept through its time, it did not eat the processor, and taking
        // a quantum away from it at the very moment when it woke up is doubly unfair:
        // the rotation owes it nothing, while `parkNanos` pays for that with a tail
        // (the measurement: an immediate tick raised the p99 of the overshoot from 33-73 us to
        // 556-1052 and at the same time arranged a tick storm — 5-11 thousand ticks per
        // core against the former 2-4).
        if left != 0 {
            TIMER_ONESHOT[c].store(1, Ordering::Relaxed);
            unsafe {
                lapic_write(0x3E0, 0x3);
                lapic_write(0x320, crate::idt::TIMER_VECTOR as u32); // one-shot (bit 17 is removed)
                lapic_write(0x380, left);
            }
            return;
        }
    }
    unsafe {
        lapic_write(0x3E0, 0x3);
        lapic_write(0x320, (crate::idt::TIMER_VECTOR as u32) | 0x2_0000);
        lapic_write(0x380, initial);
    }
}

/// Send a fixed IPI of a vector to the core with a given APIC ID (the control plane
/// interrupts a cell's core — the basis of the µs preemption and of the cores' donation).
/// The current core's APIC id (the LAPIC 0x20; call it on the BSP before the APs start).
pub fn bsp_apic_id() -> u32 { unsafe { apic_id_here() } }

/// ── THE SENDING OF AN IPI ───────────────────────────────────────────────────
///
/// The price of the question. In xAPIC sending an IPI is TWO writes into the window 0xFEE00000
/// (the addressee and the command), while on a stand without APICv every such write costs an exit
/// to the host WITH AN EMULATION OF THE INSTRUCTION: measured at 10.2 us per write (see `lapic_eoi`).
/// Twenty microseconds for waking a neighbouring core — and that is the very item
/// by which we lose to everything that runs into a thread's wakeup.
///
/// KVM offers the hypercall `KVM_HC_SEND_IPI` for that: the mask of the addressees and the vector
/// in one `vmcall`, without a parsing of the instruction by the host. We take it when it is declared;
/// otherwise the former path through the ICR.
const KVM_HC_SEND_IPI: u64 = 10;
static PV_IPI: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// A hypercall on this machine is a `vmcall` (Intel) or a `vmmcall` (AMD).
static HC_AMD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// How many IPIs went by a hypercall and how many through the ICR (the `kstat` diagnostics).
pub static IPI_PV: AtomicU64 = AtomicU64::new(0);
pub static IPI_ICR: AtomicU64 = AtomicU64::new(0);

/// The Rust compiler does not give RBX for an inline-asm operand (it is a service one for LLVM),
/// so we put the hypercall's first argument into it by hand — around the call
/// we save and restore it.
#[inline]
unsafe fn hypercall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let r: i64;
    if HC_AMD.load(Ordering::Relaxed) {
        core::arch::asm!("xchg {a0}, rbx", "vmmcall", "xchg {a0}, rbx",
            a0 = inout(reg) a0 => _, inlateout("rax") nr => r, in("rcx") a1,
            in("rdx") a2, in("rsi") a3, options(nostack));
    } else {
        core::arch::asm!("xchg {a0}, rbx", "vmcall", "xchg {a0}, rbx",
            a0 = inout(reg) a0 => _, inlateout("rax") nr => r, in("rcx") a1,
            in("rdx") a2, in("rsi") a3, options(nostack));
    }
    r
}

/// The BSP: choose the way of sending IPIs. `talaria.pvipi=0` — only the ICR (a bisect).
fn pvipi_select(cmdline: &str) {
    let (_, b, c, d) = unsafe { cpuid_raw(0) };
    // «AuthenticAMD»: ebx="Auth", edx="enti", ecx="cAMD".
    if b == u32::from_le_bytes(*b"Auth") && d == u32::from_le_bytes(*b"enti") && c == u32::from_le_bytes(*b"cAMD") {
        HC_AMD.store(true, Ordering::SeqCst);
    }
    if crate::cmdline::value(cmdline, "talaria.pvipi") == Some("0") { return; }
    if !kvm_here() { return; }
    let (feat, _, _, _) = unsafe { cpuid_raw(0x4000_0001) };
    if feat & (1 << 11) == 0 { // KVM_FEATURE_PV_SEND_IPI
        crate::println!("smp: the KVM PV IPI is not declared — the inter-core calls go through the ICR");
        return;
    }
    PV_IPI.store(true, Ordering::SeqCst);
    crate::println!("smp: the KVM PV IPI is on — a call to a neighbouring core goes in one hypercall");
}

/// An NMI to the core `cpu` (the diagnostics: a core that does not take interrupts will answer all the same).
pub fn send_nmi(cpu: usize) {
    unsafe { icr_write(apic_of(cpu), 0x0000_4400); } // delivery mode NMI, assert
}
pub fn send_ipi(apic_id: u32, vector: u8) {
    // By a hypercall we send only to the addressees that fit into the low mask (we have fewer
    // than 64 cores by construction, `percpu::MAXCPU`). A refusal of the host (a negative
    // answer) must not cost a wakeup: we silently go to the ICR.
    if PV_IPI.load(Ordering::Relaxed) && apic_id < 64 {
        let r = unsafe { hypercall4(KVM_HC_SEND_IPI, 1u64 << apic_id, 0, 0, vector as u64) };
        if r >= 0 { IPI_PV.fetch_add(1, Ordering::Relaxed); return; }
        PV_IPI.store(false, Ordering::SeqCst); // the host refused — we do not try any more
    }
    IPI_ICR.fetch_add(1, Ordering::Relaxed);
    unsafe { icr_write(apic_id, 0x0000_4000 | vector as u32); } // fixed, assert, no-shorthand
}

/// An AP's entry point (the long mode). It marks itself alive and goes into a HLT loop.
#[unsafe(no_mangle)]
extern "C" fn ap_entry() -> ! {
    AP_STAGE.store(1, Ordering::SeqCst);
    crate::idt::init(); // its own IDT register (the table is common)
    AP_STAGE.store(2, Ordering::SeqCst);
    // Clone the BSP's state (see BSP_CR0/CR4/XCR0 above): the order is first CR4
    // (OSXSAVE and so on), then XCR0 (it requires CR4.OSXSAVE), then CR0 (WP and so on).
    unsafe {
        let bcr4 = BSP_CR4.load(Ordering::SeqCst) & !AP_CR4_MASK.load(Ordering::SeqCst);
        if bcr4 != 0 { core::arch::asm!("mov cr4, {}", in(reg) bcr4); }
        let bx = BSP_XCR0.load(Ordering::SeqCst);
        if bx != 0 && bcr4 & (1 << 18) != 0 {
            core::arch::asm!("xsetbv", in("ecx") 0u32, in("eax") (bx & 0xFFFF_FFFF) as u32, in("edx") (bx >> 32) as u32);
        }
        let bcr0 = BSP_CR0.load(Ordering::SeqCst);
        if bcr0 != 0 { core::arch::asm!("mov cr0, {}", in(reg) bcr0); }
    }
    AP_STAGE.store(3, Ordering::SeqCst);
    // A page's cacheability type is set by the PAT table of THE CURRENT core: without that
    // a WC page (the ENA's LLQ window) would be an ordinary WB on an AP.
    crate::paging::pat_init();
    AP_STAGE.store(4, Ordering::SeqCst);
    AP_ALIVE.fetch_add(1, Ordering::SeqCst);
    AP_HEARTBEAT.fetch_add(1, Ordering::Relaxed);
    AP_STAGE.store(5, Ordering::SeqCst);
    lapic_enable();
    // A10: THE LOGICAL number of this core rather than its APIC id. The scheduler, the per-CPU
    // areas and the ONLINE mask live in the logical numbering — the very one that
    // the workload's `sched_setaffinity` operates with.
    let id = my_logical();
    // GS.base comes BEFORE the first interrupt window and before the registration of the PV EOI word: both
    // of them read the per-CPU area gs-relatively. The core will enter the ONLINE mask
    // below and only if it is schedulable (the datapath has its own role).
    crate::percpu::init_gs(id);
    pveoi_enable_here(); // the PV EOI word is registered by EVERY core separately
    steal_enable_here(); // the structure of the stolen time goes on every core too
    AP_STAGE.store(6, Ordering::SeqCst);
    unsafe { core::arch::asm!("sti"); } // allow IPIs from the control plane
    crate::cat::apply_local_if_changed(id); // this core's share of L3 (if the layout is already set)
    // The PMU's counters are PER CORE: without this call an `rdpmc` on a secondary core
    // returns a zero, that is, the perf is silent exactly where the workload lives.
    crate::pmu::init_cpu();
    match AP_MODE.load(Ordering::SeqCst) {
        1 => {
            // mark ourselves in ONLINE (percpu): the self-test's membarrier must really IPI-ack the AP
            crate::percpu::init(id);
            AP_STAGE.store(7, Ordering::SeqCst);
            AP_STAGE.store(8, Ordering::SeqCst);
            crate::sync::ap_selftest_loop()
        }
        2 => { // PLAN-SYNC: an AP is a core of pinned threads (a per-CPU GDT/TSS/syscall + the pickup loop)
            crate::percpu::init(id);
            AP_STAGE.store(7, Ordering::SeqCst);
            unsafe { crate::user::setup_gdt_cpu(id, 0); }
            crate::syscall::init();
            AP_STAGE.store(8, Ordering::SeqCst);
            crate::sched::ap_loop(id);
        }
        3 => {
            crate::percpu::init(id);
            AP_STAGE.store(7, Ordering::SeqCst);
            AP_STAGE.store(8, Ordering::SeqCst);
            let f = AP_HOOK.load(Ordering::SeqCst);
            if f != 0 { let f: fn(usize) -> ! = unsafe { core::mem::transmute(f as usize) }; f(id); }
        }
        // A cell's core (the datapath) deliberately does NOT enter ONLINE: nobody
        // schedules it and nobody wakes it with an IPI — an extra membarrier kick there is only jitter.
        _ => { AP_STAGE.store(8, Ordering::SeqCst); }
    }
    if crate::runmode::TEST_IPI.load(Ordering::SeqCst) {
        loop { AP_HEARTBEAT.fetch_add(1, Ordering::Relaxed); for _ in 0..1_000_000 { core::hint::spin_loop(); } }
    }
    // An AP is a cell's core: we drive the datapath (a virtio-net echo)
    crate::runmode::run_datapath();
}

unsafe extern "C" {
    fn tramp_start();
    fn tramp_end();
    fn tramp_cr3();
    fn tramp_stack();
    fn tramp_entry();
}

core::arch::global_asm!(r#"
.section .text
.code16
.global tramp_start
.global tramp_end
.global tramp_cr3
.global tramp_stack
.global tramp_entry
tramp_start:
    cli
    cld
    mov $0x0800, %ax
    mov %ax, %ds
    lgdtl (tramp_gdt_desc - tramp_start)
    mov %cr0, %eax
    or $1, %eax
    mov %eax, %cr0
    ljmpl $0x18, $(pm32 - tramp_start + 0x8000)

.code32
pm32:
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    mov %ax, %fs
    mov %ax, %gs
    /* CR3 from the trampoline */
    mov (tramp_cr3 - tramp_start + 0x8000), %eax
    mov %eax, %cr3
    /* PAE + OSFXSR + OSXMMEXCPT (SSE for the code compiled with sse2) */
    mov %cr4, %eax
    or $0x620, %eax
    mov %eax, %cr4
    /* EFER.LME + EFER.NXE (the BSP sets the NX bits in the PTEs — without NXE on an AP that is an RSVD #PF) */
    mov $0xC0000080, %ecx
    rdmsr
    or $0x900, %eax
    wrmsr
    /* CR0: clear EM AND CD|NW (after an INIT CR0=0x60000010 — the cache is OFF;
       Intel-KVM ignores the guest's CD, AMD-KVM respects it: without a reset an AP on an EPYC
       executes ~4 times slower, while a MONITOR on a non-cacheable line is not
       raised — a kwait on an AP did not wake up from a store, the broadcast paths hung);
       set MP, switch PG on */
    mov %cr0, %eax
    and $0x9FFFFFFB, %eax
    or $0x80000003, %eax
    mov %eax, %cr0
    ljmpl $0x08, $(lm64 - tramp_start + 0x8000)

.code64
lm64:
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    movq (tramp_stack - tramp_start + 0x8000), %rsp
    movq (tramp_entry - tramp_start + 0x8000), %rax
    call *%rax
1:  hlt
    jmp 1b

.align 16
tramp_gdt:
    .quad 0
    .quad 0x00209A0000000000  /* 0x08 a 64-bit code (as in boot.rs -> the IDT sel) */
    .quad 0x00CF92000000FFFF  /* 0x10 data (flat, suitable in 32 and 64) */
    .quad 0x00CF9A000000FFFF  /* 0x18 a 32-bit code (for the protected-mode step) */
tramp_gdt_desc:
    .word tramp_gdt_desc - tramp_gdt - 1
    .long (tramp_gdt - tramp_start + 0x8000)
.align 8
tramp_cr3:   .quad 0
tramp_stack: .quad 0
tramp_entry: .quad 0
tramp_end:
"#, options(att_syntax));

/// `talaria.smp=N` — bring up N-1 APs as schedulers (2..=`percpu::MAXCPU`).
///
/// It is idempotent: if the needed number of cores is already online, it does nothing. It lives
/// here rather than in the cmdline parsing because all its work is the bringing up of cores.
pub fn start_from_cmdline(cmdline: &str) {
    // WE APPLY THE KNOB HERE rather than in `topology_init`. On EC2 the command line's
    // tail arrives from the user-data, and it is read AFTER the network is brought up —
    // that is, much later than the MADT is parsed. While the rearrangement stood in
    // `topology_init`, the knob was HONESTLY printed by the audit and did NOTHING:
    // the stand's phase `smt35-20260916-122712/B` passed with `apic_of=[0,2,1,3]` with
    // `talaria.smtpack=1` in the final cmdline (the same class as the three defects
    // of session 28). Here the cmdline is already merged, while the APs are not up yet — this is
    // the only point where the rearrangement is both lawful and effective.
    if crate::cmdline::value(cmdline, "talaria.smtpack") == Some("1") {
        smt_pack(nr_cpus());
        phys_topology(nr_cpus());     // the printing comes AFTER the rearrangement: one can see that it took effect
    }
    let Some(n) = crate::cmdline::num(cmdline, "talaria.smp") else { return };
    let n = (n as usize).min(crate::percpu::MAXCPU).min(nr_cpus());
    if n < 2 { return; }
    if crate::percpu::ONLINE.load(Ordering::SeqCst).count_ones() as usize >= n { return; } // they are already up
    lapic_enable();
    AP_MODE.store(2, Ordering::SeqCst);
    for id in 1..n {
        if !start_cpu(id) { println!("talaria: the AP{} did not come up (a -smp {} is needed)", id, n); break; }
    }
    println!("talaria: the SMP scheduler: the online cores mask={:#x}", crate::percpu::ONLINE.load(Ordering::SeqCst));
}
