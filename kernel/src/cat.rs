//! §6.3: the partitioning of the L3 (Intel CAT) + the cache's size for checking the footprint
//! of the DMA rings.
//!
//! WHY THIS IS IN THE KERNEL rather than in the control plane. It is not only the scheduler that
//! cuts the tails. It was measured (POC-RESULTS v0.12, an m5zn.metal): a neighbour washing out
//! the shared L3 raises a latency-critical thread's p99 from 650 to 932 ns, and its p999 — from
//! 686 to 1216 ns. The line of that table "the same but with CAT" could not be
//! closed, and the reason was not architectural: in the AL2023 kernel
//! CONFIG_X86_CPU_RESCTRL is not built. In one's own OS that obstacle does not exist at all —
//! for a single-workload system CAT is not a framework of groups and a file system but
//! a mask in IA32_L3_MASK_n and a class number in IA32_PQR_ASSOC.
//!
//! The layout by CLASSES goes right after the layout by cores (v1.9): a pool of cores has already been
//! named by the operator (`pool=fast:1,2`), a share of the cache is hung on that same pool
//! (`llc=fast:0xf0`). The class is assigned to a core ONCE at the bring-up rather than at a
//! context switch: we have one workload, its threads do not take the cache away from one
//! another on a schedule — they take whole cores away. So the hot path of
//! switching does not get more expensive by a single cycle.
//!
//! ABOUT DDIO. There is no separate "DDIO knob" here and there will not be one. On server Intels
//! DDIO is always on, it cannot be switched off by software, and the MSR 0xC8B (IIO LLC WAYS) is
//! an undocumented model-specific knob: on an i7-3820 it gave out an EIO
//! (POC-RESULTS v0.15) and on different generations it lies differently. There are exactly two documented
//! levers, and both are here: the CAT masks and the footprint of the DMA rings against the L3
//! (`ring_footprint`).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use crate::percpu::MAXCPU;

const MSR_L3_MASK0: u32 = 0x0C90;
const MSR_PQR_ASSOC: u32 = 0x0C8F;

/// There are never more than 16 classes of service for the L3 CAT on any generation.
pub const MAXCOS: usize = 16;
/// The share of the L3 that we conservatively consider "the I/O write window" (DDIO
/// gives part of the associativity ways to it; on a Skylake-SP by default 2 out of
/// 11). CPUID does not report the exact number, so we take a knowingly safe
/// quarter: the rule is needed as a SIGNAL, not as a formula.
pub const DDIO_SHARE_DEN: u64 = 4;

static PRESENT: AtomicBool = AtomicBool::new(false);
static CBM_LEN: AtomicU32 = AtomicU32::new(0);
static COS_MAX: AtomicU32 = AtomicU32::new(0);
static SHAREABLE: AtomicU32 = AtomicU32::new(0);
static L3_BYTES: AtomicU64 = AtomicU64::new(0);
/// The class assigned to a logical core (0 — the default class: the whole cache).
static COS_OF: [AtomicU8; MAXCPU] = [const { AtomicU8::new(0) }; MAXCPU];
/// The masks really written into the hardware (for the report and for a repeated application on an AP).
static CLASS_MASK: [AtomicU32; MAXCOS] = [const { AtomicU32::new(0) }; MAXCOS];
static APPLIED: AtomicU32 = AtomicU32::new(0);

fn cpuid_count(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    let r = core::arch::x86_64::__cpuid_count(leaf, sub);
    (r.eax, r.ebx, r.ecx, r.edx)
}

unsafe fn wrmsr(msr: u32, v: u64) {
    core::arch::asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32,
        options(nostack, preserves_flags));
}
unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi,
        options(nostack, preserves_flags));
    (hi as u64) << 32 | lo as u64
}

pub fn present() -> bool { PRESENT.load(Ordering::Relaxed) }
pub fn cbm_len() -> u32 { CBM_LEN.load(Ordering::Relaxed) }
pub fn cos_max() -> u32 { COS_MAX.load(Ordering::Relaxed) }

/// The L3's size in bytes (0 — it could not be determined).
pub fn l3_bytes() -> u64 { L3_BYTES.load(Ordering::Relaxed) }

/// The L3's size by CPUID. Intel — leaf 4, AMD — 0x8000001D (the format is the same; on an EPYC
/// there is no leaf 4 at all, and we validate on it — a c6a.metal).
fn detect_l3() -> u64 {
    let (max_std, _, _, _) = cpuid_count(0, 0);
    let (max_ext, _, _, _) = cpuid_count(0x8000_0000, 0);
    let leaf = if max_std >= 4 {
        4
    } else if max_ext >= 0x8000_001D {
        0x8000_001D
    } else {
        return 0;
    };
    for sub in 0..16 {
        let (a, b, c, _) = cpuid_count(leaf, sub);
        let ctype = a & 0x1F;
        if ctype == 0 { break; }            // there are no more sublevels
        let level = (a >> 5) & 0x7;
        if level != 3 { continue; }
        let line = (b & 0xFFF) as u64 + 1;
        let parts = ((b >> 12) & 0x3FF) as u64 + 1;
        let ways = ((b >> 22) & 0x3FF) as u64 + 1;
        let sets = c as u64 + 1;
        return line * parts * ways * sets;
    }
    // Leaf 4 exists on AMD, but the L3 is not described in it — we get it from the extended one.
    if leaf == 4 && max_ext >= 0x8000_001D {
        for sub in 0..16 {
            let (a, b, c, _) = cpuid_count(0x8000_001D, sub);
            let ctype = a & 0x1F;
            if ctype == 0 { break; }
            if (a >> 5) & 0x7 != 3 { continue; }
            let line = (b & 0xFFF) as u64 + 1;
            let parts = ((b >> 12) & 0x3FF) as u64 + 1;
            let ways = ((b >> 22) & 0x3FF) as u64 + 1;
            let sets = c as u64 + 1;
            return line * parts * ways * sets;
        }
    }
    0
}

/// Determine the capabilities and print them. It is called on the BSP at the bring-up.
pub fn init() {
    L3_BYTES.store(detect_l3(), Ordering::Relaxed);
    let (max_std, _, _, _) = cpuid_count(0, 0);
    // CPUID.(EAX=7,ECX=0):EBX[15] — RDT-A (Resource Director Technology,
    // allocation). Without it there may be no leaf 0x10 at all.
    let rdt_a = max_std >= 7 && cpuid_count(7, 0).1 & (1 << 15) != 0;
    if max_std >= 0x10 && rdt_a {
        let (_, b, _, _) = cpuid_count(0x10, 0);
        if b & (1 << 1) != 0 {                     // ResID 1 = L3 CAT
            let (a, b1, _, d) = cpuid_count(0x10, 1);
            CBM_LEN.store((a & 0x1F) + 1, Ordering::Relaxed);
            SHAREABLE.store(b1, Ordering::Relaxed);
            COS_MAX.store(d & 0xFFFF, Ordering::Relaxed);
            PRESENT.store(true, Ordering::Relaxed);
        }
    }
    let l3 = l3_bytes();
    if present() {
        crate::println!("cat: there is L3 CAT — ways in the mask {}, classes {} (0..{}), shared with the firmware {:#x}, the L3 {} KB",
            cbm_len(), cos_max() + 1, cos_max(), SHAREABLE.load(Ordering::Relaxed), l3 / 1024);
    } else {
        // The absence of CAT is the norm for a guest: KVM does not give the RDT allocation into
        // a guest, and on the dev stand (a Sandy Bridge-E) it is not there in the hardware. We say it
        // straight, so that `llc=` in the manifest does not look applied.
        crate::println!("cat: L3 CAT is unavailable (CPUID 0x10 does not declare ResID 1) — llc= in the manifest will be REJECTED; the L3 {} KB",
            l3 / 1024);
    }
}

/// Is the mask fit? It is a requirement of the HARDWARE, not ours: the bits are bound to be contiguous and
/// the mask non-zero — otherwise a wrmsr gives a #GP. That is exactly the class of error which
/// on a stand without CAT does not show up in any way, which is why it is checked separately.
pub fn mask_valid(m: u32, cbm_len: u32) -> bool {
    if m == 0 || cbm_len == 0 || cbm_len > 32 { return false; }
    if cbm_len < 32 && m >> cbm_len != 0 { return false; }  // bits beyond the length
    // contiguity: for a number of the form 0b0001_1110 a shift right to the first one
    // gives 2^k - 1
    let low = m >> m.trailing_zeros();
    low & low.wrapping_add(1) == 0
}

/// Write the mask of the class `cos` and READ IT BACK. The read-back here is not
/// over-insurance: an MSR silently ignores a write if the feature is not on or the mask is
/// unfit, and "we have applied it" without a check would have meant full isolation on paper
/// with a shared cache in reality (the msix gate in v1.9 was made for the same reason).
pub fn set_class(cos: u32, mask: u32) -> bool {
    if !present() || cos > cos_max() || !mask_valid(mask, cbm_len()) { return false; }
    unsafe {
        wrmsr(MSR_L3_MASK0 + cos, mask as u64);
        let back = rdmsr(MSR_L3_MASK0 + cos) as u32;
        if back != mask { return false; }
    }
    CLASS_MASK[cos as usize].store(mask, Ordering::Relaxed);
    true
}

/// The class already written into the hardware on this core (255 — not once yet).
static APPLIED_COS: [AtomicU8; MAXCPU] = [const { AtomicU8::new(255) }; MAXCPU];

/// Set the class if it has changed. It is called from the tick and from the idle loop
/// of picking tasks: the layout comes from the manifest LATER than the cores come up,
/// while sending an IPI for that for the sake of one MSR write is extra machinery. When there is
/// no CAT (a guest, the dev stand), this is one atomic load and a return.
pub fn apply_local_if_changed(cpu: usize) {
    if !PRESENT.load(Ordering::Relaxed) || cpu >= MAXCPU { return; }
    let want = COS_OF[cpu].load(Ordering::Relaxed);
    if APPLIED_COS[cpu].load(Ordering::Relaxed) == want { return; }
    apply_local(cpu);
    APPLIED_COS[cpu].store(want, Ordering::Relaxed);
}

/// Set on the CURRENT logical core the class assigned to it by the layout.
/// It is called from the core itself (IA32_PQR_ASSOC is a register of the logical processor).
pub fn apply_local(cpu: usize) {
    if !present() || cpu >= MAXCPU { return; }
    let cos = COS_OF[cpu].load(Ordering::Relaxed) as u64;
    unsafe {
        let v = rdmsr(MSR_PQR_ASSOC);
        wrmsr(MSR_PQR_ASSOC, (v & 0xFFFF_FFFF) | (cos << 32)); // we do not touch the RMID (the lower 32)
    }
    APPLIED.fetch_add(1, Ordering::Relaxed);
}

/// The cache class assigned to a core (0 — by default, the whole cache).
pub fn cos_of(cpu: usize) -> u8 { if cpu < MAXCPU { COS_OF[cpu].load(Ordering::Relaxed) } else { 0 } }
/// The mask of the L3's ways of the class `cos` and its share in bytes. (0, 0) — CAT is unavailable.
pub fn class_mask(cos: u32) -> (u32, u64) {
    if !present() || cos > cos_max().min(MAXCOS as u32 - 1) { return (0, 0); }
    let m = unsafe { rdmsr(MSR_L3_MASK0 + cos) } as u32;
    let ways = m.count_ones() as u64;
    let share = if cbm_len() > 0 { l3_bytes() * ways / cbm_len() as u64 } else { 0 };
    (m, share)
}
/// The share of the L3 (in bytes) that the core `cpu` got. 0 — there is no CAT.
pub fn share_of_cpu(cpu: usize) -> u64 { class_mask(cos_of(cpu) as u32).1 }

/// What really stands in the hardware: the cache share of every class and which cores are in it.
/// It is exactly THIS that the operator needs to see, not what was written in the manifest.
pub fn dump() {
    if !present() {
        crate::println!("cat: L3 CAT is unavailable; the L3 {} KB", l3_bytes() / 1024);
        return;
    }
    crate::println!("cat: ways in the mask {}, classes {}, the L3 {} KB", cbm_len(), cos_max() + 1, l3_bytes() / 1024);
    for c in 0..=cos_max().min(MAXCOS as u32 - 1) {
        let m = unsafe { rdmsr(MSR_L3_MASK0 + c) } as u32;
        let mut cpus = 0u64;
        for i in 0..MAXCPU { if COS_OF[i].load(Ordering::Relaxed) as u32 == c { cpus |= 1u64 << i; } }
        let ways = m.count_ones() as u64;
        let share = if cbm_len() > 0 { l3_bytes() * ways / cbm_len() as u64 } else { 0 };
        crate::println!("cat:   the class {} = {:#x} ({} ways, ~{} KB) the cores {:#x}", c, m, ways, share / 1024, cpus);
    }
}

/// Apply the cache shares from the manifest. Every rule `llc=<to whom>:<mask>` has already been
/// parsed into (a mask of cores, a mask of cache ways) — a ready pair comes here, while
/// where the mask of cores came from (a pool, a number, `*`) was decided by the layout.
pub fn apply(rules: &[(u64, u32)]) -> u32 {
    if !present() {
        if !rules.is_empty() {
            crate::println!("cat: {} llc= rules are REJECTED — the L3 CAT is unavailable on this machine", rules.len());
        }
        return 0;
    }
    let mut cos = 1u32; // the class 0 is the default (the whole cache), we do not redefine it
    let mut done = 0u32;
    for &(cpus, mask) in rules {
        if cos > cos_max() {
            crate::println!("cat: there are no more classes (the ceiling {}) — the rule with the mask {:#x} is DROPPED", cos_max(), mask);
            continue;
        }
        if !mask_valid(mask, cbm_len()) {
            crate::println!("cat: the mask {:#x} is unfit (CONTIGUOUS bits within {} ways are needed) — the rule is DROPPED",
                mask, cbm_len());
            continue;
        }
        if !set_class(cos, mask) {
            crate::println!("cat: the class {} with the mask {:#x} was NOT ACCEPTED by the hardware (the read-back did not match)", cos, mask);
            continue;
        }
        for c in 0..MAXCPU {
            if cpus & (1u64 << c) != 0 { COS_OF[c].store(cos as u8, Ordering::Relaxed); }
        }
        let ways = mask.count_ones() as u64;
        let share = if cbm_len() > 0 { l3_bytes() * ways / cbm_len() as u64 } else { 0 };
        crate::println!("cat: the class {} = the mask {:#x} ({} out of {} ways, ~{} KB of the L3) -> the cores {:#x}",
            cos, mask, ways, cbm_len(), share / 1024, cpus);
        cos += 1; done += 1;
    }
    // The class 0 (everything else) is bound to STOP including foreign ways — otherwise the
    // isolation is one-sided: the latency thread is protected from the background one, while the background one
    // still washes out its share, and the whole point is lost.
    if done > 0 {
        let all = if cbm_len() >= 32 { u32::MAX } else { (1u32 << cbm_len()) - 1 };
        let mut taken = 0u32;
        for c in 1..=cos_max().min(MAXCOS as u32 - 1) {
            taken |= CLASS_MASK[c as usize].load(Ordering::Relaxed);
        }
        let rest = all & !taken;
        if mask_valid(rest, cbm_len()) && set_class(0, rest) {
            crate::println!("cat: the class 0 (everything else) has been narrowed to {:#x} — foreign shares are no longer visible to it", rest);
        } else {
            crate::println!("cat: ATTENTION — the remainder {:#x} is not contiguous, the class 0 has been left at the whole cache (the isolation is one-sided)", rest);
        }
    }
    done
}

// ── The footprint of the DMA rings against the L3 ("leaky DMA") ─────────────────
// It was measured (POC-RESULTS v0.15): an RX ring that fits into the L3 gives 251 LLC misses per
// 1000 packets; a ring of 32 MB with an L3 of 24 MB — 1407, that is 5.6 times more:
// a fresh DMA buffer is evicted before it has been read. At present the rule is
// held by a CONSTANT (RX_RING=256 buffers ~ 512 KB). It will break silently on the
// day multi-queue receive is opened (A6): 32 queues of the same rings are
// already tens of megabytes, and the regression will come without a single message. So the footprint
// is computed and printed by itself.

/// The verdict on the footprint: (does it fit into the "I/O window", does it fit into the L3 at all).
/// Separately from the machine's cache sizes — otherwise the rule would be checked only on the
/// hardware where it is observed anyway.
pub fn footprint_verdict(bytes: u64, l3: u64) -> (bool, bool) {
    if l3 == 0 { return (true, true); } // the size is unknown — we keep silent rather than lie
    (bytes <= l3 / DDIO_SHARE_DEN, bytes <= l3)
}

/// (the footprint's bytes, does it fit into the "I/O window", does it fit into the L3 at all).
pub fn ring_footprint(queues: u64, ring_bufs: u64, buf: u64, desc_bytes: u64) -> (u64, bool, bool) {
    let bytes = queues * (ring_bufs * buf + desc_bytes);
    let (ddio, l3) = footprint_verdict(bytes, l3_bytes());
    (bytes, ddio, l3)
}

/// Compute and report. It returns false if the rings knowingly do not fit into the L3.
pub fn report_footprint(what: &str, queues: u64, ring_bufs: u64, buf: u64, desc_bytes: u64) -> bool {
    let (bytes, fits_ddio, fits_l3) = ring_footprint(queues, ring_bufs, buf, desc_bytes);
    let l3 = l3_bytes();
    crate::println!("cat: the footprint of the {} rings — {} queues, {} KB (the L3 {} KB)",
        what, queues, bytes / 1024, l3 / 1024);
    if !fits_l3 {
        crate::println!("cat: ATTENTION — the rings are BIGGER THAN THE L3: a fresh DMA buffer will be evicted before it is read (leaky DMA, x5.6 by LLC misses in v0.15)");
    } else if !fits_ddio {
        crate::println!("cat: ATTENTION — the rings are bigger than a quarter of the L3: the share for I/O writes is smaller than the whole cache, the margin has been eaten up");
    }
    fits_l3
}

/// The self-test (`talaria.test=cat`). It checks what there is nothing else to check on a stand
/// without CAT with: the validation of the masks (violating it is a #GP on live hardware) and
/// the arithmetic of the rings' footprint. The hardware part — the read-back — goes only where
/// there is CAT.
pub fn selftest() -> bool {
    let mut ok = true;
    let mut check = |cond: bool, what: &str| {
        if !cond { crate::println!("cat-selftest: FAILURE — {}", what); ok = false; }
    };
    // The masks: contiguity and the bounds. 20 ways is a typical Skylake-SP.
    check(mask_valid(0x00F, 20), "0x00f (the 4 lower ways) is bound to be fit");
    check(mask_valid(0xF00, 20), "0xf00 (the 4 upper ways) is bound to be fit");
    check(mask_valid(0xFFFFF, 20), "a mask covering the whole cache is bound to be fit");
    check(!mask_valid(0, 20), "a zero mask is bound to be rejected (the hardware gives a #GP)");
    check(!mask_valid(0x0F0F, 20), "the mask with a hole 0x0f0f is bound to be rejected (the bits are not contiguous)");
    check(!mask_valid(0x101, 20), "the mask with a hole 0x101 is bound to be rejected");
    check(!mask_valid(0x1F, 4), "a mask wider than the declared length is bound to be rejected");
    check(mask_valid(0xF, 4), "a mask exactly of the declared length is bound to be fit");
    // The footprint of the rings: the numbers are taken from ena.rs, the threshold — from the measurement of v0.15.
    let l3 = l3_bytes();
    let one = 256u64 * 2048 + 512 * 16;          // one pair of ENA queues as it is now
    let (b1, ddio1, l31) = ring_footprint(1, 256, 2048, 512 * 16);
    let (b32, _, l332) = ring_footprint(32, 256, 2048, 512 * 16);
    check(b1 == one, "the footprint of one queue was computed wrongly");
    check(b32 == 32 * one, "the footprint of 32 queues was computed wrongly");
    if l3 >= 8 * 1024 * 1024 {
        check(ddio1 && l31, "one queue is bound to fit into the L3 with room to spare");
        check(!l332 || 32 * one <= l3, "32 queues on a machine with a small L3 are bound to be marked as NOT fitting");
    }
    // The verdict itself is on a FIXED L3 of 24 MB, the very one on which the
    // leaky DMA was measured in v0.15. Otherwise the rule would be checked only on hardware where it
    // is observed anyway, while it will break on other hardware.
    const L3REF: u64 = 24 * 1024 * 1024;
    let (d1, f1) = footprint_verdict(1 * 1024 * 1024, L3REF);
    let (d8, f8) = footprint_verdict(8 * 1024 * 1024, L3REF);
    let (d32, f32) = footprint_verdict(32 * 1024 * 1024, L3REF);
    check(d1 && f1, "1 MB of rings on an L3 of 24 MB is bound to be in the I/O window");
    check(!d8 && f8, "8 MB of rings on an L3 of 24 MB: it fits into the cache, into the window — no");
    check(!d32 && !f32, "32 MB of rings on an L3 of 24 MB are bound to be marked as NOT fitting (that is leaky DMA v0.15)");
    crate::println!("cat-selftest: the verdict on the footprint at an L3 of 24 MB: 1 MB={} 8 MB={} 32 MB={}",
        if d1 { "in the window" } else { "past it" },
        if f8 && !d8 { "in the L3, but not in the window" } else { "?" },
        if !f32 { "DOES NOT fit" } else { "?" });
    crate::println!("cat-selftest: the masks and the rings' footprint have been checked, the L3 {} KB, one queue {} KB, 32 queues {} KB",
        l3 / 1024, b1 / 1024, b32 / 1024);
    // The same report that is printed at the ENA's bring-up — on today's layout and
    // on a hypothetical multi-queue one (A6). The second case is what the
    // check exists for: it is bound to say out loud that the rings have stopped
    // fitting — silence here is indistinguishable from "everything is fine".
    report_footprint("ENA receive as it is now", 1, 256, 2048, 512 * 16);
    report_footprint("ENA receive with 32 queues (A6)", 32, 256, 2048, 512 * 16);
    // The hardware part: only where CAT is declared.
    if !present() || cbm_len() < 2 {
        crate::println!("cat-selftest: the L3 CAT is unavailable — the writing of the masks is SKIPPED (a KVM guest / a chip without RDT-A)");
    } else {
        let save = unsafe { rdmsr(MSR_L3_MASK0 + 1) } as u32;
        let all = if cbm_len() >= 32 { u32::MAX } else { (1u32 << cbm_len()) - 1 };
        let half = all & !(all >> (cbm_len() / 2)); // the upper half of the ways, contiguous
        let wrote = set_class(1, half);
        check(wrote, "the mask was not written into IA32_L3_MASK_1 or was not read back");
        apply_local(crate::percpu::cpu_id());
        let pqr = unsafe { rdmsr(MSR_PQR_ASSOC) };
        crate::println!("cat-selftest: the class 1 = {:#x} (written and read back), PQR_ASSOC={:#x}", half, pqr);
        if save != 0 { set_class(1, save); } // put it back as it was
    }
    crate::println!("cat-selftest: {}", if ok { "ALL OK" } else { "FAILURE" });
    ok
}
