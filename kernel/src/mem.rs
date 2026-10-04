//! The allocator of physical frames from the PVH memory map (hvm_start_info.memmap).
//! A bump allocator over the largest RAM region above the kernel's image.
use core::sync::atomic::{AtomicU64, Ordering};

const FRAME: u64 = 4096;
const KERNEL_TOP: u64 = 32 * 1024 * 1024; // the historical minimum: the image+stack+tables below 32 MB
unsafe extern "C" { static __kernel_end: u8; }   // the end of the image (linker.ld, after .bss)
unsafe extern "C" { static __kernel_start: u8; } // the start of the image (linker.ld; the base is the parameter KBASE)
/// The start of the kernel's image. Not a constant: the base is set at link time (`--defsym KBASE=`),
/// and a build for a non-relocatable binary moves the kernel above its segments.
pub fn kernel_lo() -> u64 { core::ptr::addr_of!(__kernel_start) as u64 }
/// The first address BEYOND the kernel's image (rounded up): free memory starts there.
pub fn kernel_hi() -> u64 { kernel_top() }
/// The start of free physical memory: max(32 MB, the end of the kernel's image). An image with the regression's
/// embedded binaries outgrew 32 MB — a bump from 0x2000000 handed out frames OVER .bss (#UD rip=3).
fn kernel_top() -> u64 {
    let end = core::ptr::addr_of!(__kernel_end) as u64;
    KERNEL_TOP.max((end + FRAME - 1) & !(FRAME - 1))
}

static NEXT: AtomicU64 = AtomicU64::new(0);
static END: AtomicU64 = AtomicU64::new(0);
// THE MAIN NODE'S HIGH AREA: the RAM above 4 GB that no other node owns (`high_init`).
// It is mapped identity by 1 GB leaves before the first process address space exists, as
// the per-node areas are, and it serves the same consumers: the workload's memory (heaps,
// stacks, lazy and COW pages) through `alloc_contig_node`, and the ENA rings (`alloc_high`
// — a Nitro device accepts them only above 4 GB, see ena.rs). 0 — there is no such area.
static HIGH_BASE: AtomicU64 = AtomicU64::new(0);
static HIGH_NEXT: AtomicU64 = AtomicU64::new(0);
static HIGH_END: AtomicU64 = AtomicU64::new(0);

#[repr(C)]
struct MemEntry { addr: u64, size: u64, typ: u32, _r: u32 }

// ── the RAM map taken at the bring-up: it is needed LATER, when the SRAT is parsed ──
// The NUMA nodes become known only after `acpi::init`, while the memory map
// comes from the bootloader before it. We keep it here so that the per-node areas
// are built from the same source rather than from a second one that may diverge.
const MAX_RAM: usize = 24;
static mut RAM_BASE: [u64; MAX_RAM] = [0; MAX_RAM];
static mut RAM_LEN: [u64; MAX_RAM] = [0; MAX_RAM];
static RAM_N: AtomicU64 = AtomicU64::new(0);

/// Parse the memmap, pick the largest RAM region above KERNEL_TOP, print the map.
pub fn init(start_info: u64) {
    unsafe {
        let magic = *(start_info as *const u32);
        if magic != 0x336E_C578 { println_mem("there is no PVH start_info"); return; }
        let memmap_paddr = *((start_info + 40) as *const u64);
        let entries = *((start_info + 48) as *const u32);
        if memmap_paddr == 0 || entries == 0 { println_mem("there is no memmap"); return; }
        let tbl = memmap_paddr as *const MemEntry;
        let mut best_base = 0u64; let mut best_len = 0u64; let mut total_ram = 0u64;
        crate::println!("talaria: the PVH memory map ({} entries):", entries);
        for i in 0..entries as usize {
            let e = &*tbl.add(i);
            let kind = match e.typ { 1 => "RAM", 2 => "reserved", 3 => "ACPI", 4 => "NVS", _ => "?" };
            crate::println!("    [{:>2}] {:#012x}..{:#012x} {}", i, e.addr, e.addr + e.size, kind);
            if e.typ == 1 {
                total_ram += e.size;
                let k = RAM_N.load(Ordering::Relaxed) as usize;
                if k < MAX_RAM { RAM_BASE[k] = e.addr; RAM_LEN[k] = e.size; RAM_N.store(k as u64 + 1, Ordering::Relaxed); }
                // THE BOUNDARY IS ROUNDED UP TO 2 MB rather than taken tight against the
                // end of the image. The reason is not tidiness but memory protection:
                // GB0 is mapped BY 2 MB LEAVES (`paging::init`), and the leaf
                // into which the kernel's tail fell is entirely marked supervisor. The frames
                // from that leaf were handed out by the allocator as ordinary ones — and everything that
                // got into ring 3 (the workload's stack, the heap) lay on a page
                // that ring 3 HAS NO RIGHT to write to: a #PF "protection, write" on
                // the very first access.
                //
                // The error depended on where exactly the image ended: while the kernel's
                // tail lay close to the 2 MB boundary, several frames fell into the dangerous
                // zone, and they went to the kernel itself. As soon as the image
                // grew a little, the workload's stack drove into the zone. From the outside it
                // looked like "printf broke after some code was added", that is, it
                // pointed anywhere but here.
                //
                // The price is less than two megabytes per machine. The alternative (splitting
                // the leaf into 4K ones and marking the tail piece by piece) costs page tables and
                // a branch in the very earliest bring-up code for the sake of those same two megabytes.
                let base = e.addr.max((kernel_top() + 0x1F_FFFF) & !0x1F_FFFF);
                // A2: CUT IT OFF at 4 GB. Everything this allocator hands out is touched by the kernel
                // at its IDENTITY address (the page tables, the DMA rings,
                // the stacks), while our identity is exactly 0..4 GB (paging::init, GB0-3).
                // Previously the largest RAM region in general was picked — and on types with
                // a lot of memory (m7i.xlarge: 3 GB below and 13 GB above) it
                // turned out to be ABOVE 4 GB: the very first alloc_frame for the PML4 gave
                // the frame 0x1_0000_0000, while a write into it gave a #PF before our own tables
                // were even brought up. The instance silently fell into a crash dump, and the type
                // looked "unsupported". The memory above 4 GB is the high area (`high_init`),
                // mapped explicitly and handed out only to the consumers that can take it.
                let end = (e.addr + e.size).min(4u64 << 30);
                if end > base && end - base > best_len { best_len = end - base; best_base = base; }
            }
        }
        NEXT.store((best_base + FRAME - 1) & !(FRAME - 1), Ordering::Relaxed);
        ALLOC_BASE.store(NEXT.load(Ordering::Relaxed), Ordering::Relaxed);
        END.store(best_base + best_len, Ordering::Relaxed);
        crate::println!("talaria: RAM in total {} MB; the frame allocator: {:#x}..{:#x} ({} MB)",
            total_ram / 1048576, NEXT.load(Ordering::Relaxed), END.load(Ordering::Relaxed),
            (END.load(Ordering::Relaxed) - NEXT.load(Ordering::Relaxed)) / 1048576);
        // What the machine cannot see and what the refcount table does not cover is said
        // once the high area is known (`high_init`, after ACPI) — here it would be a lie
        // about every machine with RAM above 4 GB.
    }
}

/// How much memory the allocators can hand out at all: the main one, the high area and the
/// other nodes' areas (the bump extents, not what is left).
fn seen_bytes() -> u64 {
    let mut s = END.load(Ordering::Relaxed).saturating_sub(ALLOC_BASE.load(Ordering::Relaxed));
    s += HIGH_END.load(Ordering::Relaxed).saturating_sub(HIGH_BASE.load(Ordering::Relaxed));
    for n in 0..MAX_NODES {
        s += NODE_END[n].load(Ordering::Relaxed).saturating_sub(NODE_BASE[n].load(Ordering::Relaxed));
    }
    s
}

/// The ceiling of the identity map above 4 GB. A process's private heap is mapped at
/// `user::PROC_HEAP_BASE` in the same PML4 slot as the identity gigabytes, so an
/// identity leaf there would be overlaid by the heap in every process — and the kernel,
/// writing a frame at its identity address under that AS, would write into the heap.
fn ident_ceiling() -> u64 { crate::user::PROC_HEAP_BASE }

/// Build the main node's high area: the largest piece of RAM above 4 GB (and below
/// `ident_ceiling`) that no other node's area holds, mapped identity by 1 GB at once.
///
/// WHY IT IS NEEDED. The main allocator stops at 4 GB (A2 above), and that was the whole
/// machine's memory as far as a workload could tell: 850 MB of a `c7g.large`'s 3995 (its
/// RAM is 1-2 GiB and 16-18.9 GiB), 3 GB of a `c6i.xlarge`'s 8. The frames above serve
/// what tolerates a 1 GB identity leaf: a process's own pages are mapped by 4K leaves
/// at their user addresses, and the kernel touches them only at their identity address
/// (zeroing, a COW copy) — never a guard or an `unguard`. The kernel's own structures
/// (page tables, kernel stacks, the FS cache) stay in the low memory, which is now left
/// to them instead of being eaten by the workload's heaps.
///
/// WHEN. After the node areas (the piece must not be another node's) and before the first
/// process address space — the same condition as theirs: an AS copies the kernel's PDPT
/// at its birth, and a leaf laid later is not in it.
fn high_init() {
    let main = MAIN_NODE.load(Ordering::Relaxed) as u8;
    let ceil = ident_ceiling();
    let nram = (RAM_N.load(Ordering::Relaxed) as usize).min(MAX_RAM);
    let (mut best_b, mut best_e) = (0u64, 0u64);
    for k in 0..nram {
        let (rb, rl) = unsafe {
            (core::ptr::read_volatile(core::ptr::addr_of!(RAM_BASE[k])),
             core::ptr::read_volatile(core::ptr::addr_of!(RAM_LEN[k])))
        };
        // A region may straddle 4 GB (QEMU's `virt` puts RAM at 1 GB, so 4 GB of it ends
        // at 5): the part below belongs to the main allocator, the part above is ours.
        let mut b = (rb.max(4u64 << 30).max(kernel_top()) + FRAME - 1) & !(FRAME - 1);
        let e = (rb + rl).min(ceil) & !(FRAME - 1);
        // The runs between the other nodes' areas.
        while b < e {
            let mut run_end = e;
            let mut inside = 0u64;
            for n in 0..MAX_NODES {
                let (nb, ne) = (NODE_BASE[n].load(Ordering::Relaxed), NODE_END[n].load(Ordering::Relaxed));
                if ne <= nb { continue; }
                if b >= nb && b < ne { inside = ne; break; }
                if nb > b && nb < run_end { run_end = nb; }
            }
            if inside != 0 { b = (inside + FRAME - 1) & !(FRAME - 1); continue; }
            // With an SRAT the piece must be the main node's: a foreign one would hand the
            // main node's workload remote memory under the name of its own.
            let own = |a: u64| { let n = crate::acpi::node_of_phys(a); n == 255 || n == main || main == 255 };
            if run_end - b > best_e - best_b && own(b) && own(run_end - 1) { best_b = b; best_e = run_end; }
            b = run_end;
        }
    }
    if best_e > best_b {
        // Mapped AT ONCE, for the reason `numa_init` gives; a piece that cannot be mapped is
        // not slow memory but a guaranteed fault, so it is dropped whole.
        let mut g = best_b & !((1u64 << 30) - 1);
        let mut ok = true;
        while g < best_e { if crate::paging::map_ram_1g(g) == 0 { ok = false; break; } g += 1 << 30; }
        if ok {
            HIGH_BASE.store(best_b, Ordering::Relaxed);
            HIGH_NEXT.store(best_b, Ordering::Relaxed);
            HIGH_END.store(best_e, Ordering::Relaxed);
            crate::println!("mem: the high area {:#x}..{:#x} ({} MB) — the workload's memory above 4 GB",
                best_b, best_e, (best_e - best_b) / 1048576);
        } else {
            crate::println!("mem: the high area {:#x}..{:#x} is NOT mapped — it is dropped", best_b, best_e);
        }
    }
    rc_grow();
    // LOUDLY, WHEN THE MACHINE CANNOT SEE ITS MEMORY. On the stand (c6i.xlarge, 8 GB)
    // the allocator once saw 3021 MB, and it is from THAT number that `ext2::cache_pick` takes
    // a sixteenth for the block cache (180 MB against Linux's page cache
    // over the whole machine). For three sessions that stood as a "known" item, and
    // not a single boot line said anything about it.
    let (seen, total) = (seen_bytes() / 1048576, ram_total_frames() * FRAME / 1048576);
    if total > seen * 3 / 2 {
        crate::println!("talaria: ATTENTION: the allocators see {} MB out of the machine's {} MB — the FS block cache is by default \
            computed from the VISIBLE share; set fscache= in the manifest explicitly", seen, total);
    }
}

/// The top of everything the allocators hand out (the refcount table must reach it).
fn alloc_top() -> u64 {
    let mut t = END.load(Ordering::Relaxed).max(HIGH_END.load(Ordering::Relaxed));
    for n in 0..MAX_NODES { t = t.max(NODE_END[n].load(Ordering::Relaxed)); }
    t
}

/// Make the refcount table reach `alloc_top`. The static table covers 16 GB; a machine
/// whose RAM goes higher (a `c7g.large` has its RAM at 16-18.9 GiB, a two-socket one has
/// its second node's above 130) gets a table from the high area — one byte per frame, a
/// quarter of a per mille of the memory it describes. Called once, before any process.
fn rc_grow() {
    // THE REFCOUNT TABLE MUST COVER EVERYTHING THE ALLOCATOR HANDS OUT.
    // A frame outside it is considered shared (see `rc_get`), that is, there will be no
    // corruption — but there will be extra copies and leaks, and one must know about that IN ADVANCE
    // rather than from a workload's crash an hour later. Previously the table covered 1 GB with
    // three gigabytes of the allocator, and not a single line said anything about it.
    let top = alloc_top();
    let need = top >> 12;
    if need <= rc_capacity_frames() { return; }
    let pages = need.div_ceil(FRAME);
    let t = alloc_high(pages, FRAME).or_else(|| {
        let a = alloc_contig(pages)?;
        unsafe { core::ptr::write_bytes(a as *mut u8, 0, (pages * FRAME) as usize) };
        Some(a)
    });
    match t {
        Some(a) => {
            // The old table's contents go with it: nothing is shared yet at this point, but
            // the copy makes the switch correct at any moment rather than at this one only.
            let (old, oldlen) = rc_table();
            unsafe { core::ptr::copy_nonoverlapping(old as *const u8, a as *mut u8, oldlen) };
            // The length first: a reader that sees the new base must see its length too.
            RC_LEN.store(pages * FRAME, Ordering::Relaxed);
            RC_PTR.store(a, Ordering::Release);
            crate::println!("mem: the COW refcount table: {} KB at {:#x}, it covers up to {:#x}", pages * 4, a, pages * FRAME * FRAME);
        }
        None => crate::println!("talaria: ATTENTION: the COW refcount table covers {} MB, while the allocators hand out up to {:#x} ({} MB) — the frames above \
            will be considered shared (extra copies and leaks, but not corruption)",
            rc_capacity_frames() * 4096 / 1048576, top, top / 1048576),
    }
}

// ── the memory by NUMA nodes ────────────────────────────────────────────────
//
// To bring a process's threads onto one node and keep handing them frames from a foreign one
// means paying for locality and not getting it: on a two-socket machine
// foreign memory costs several times more than one's own, and no affinity policy
// compensates for that. So every node has its own bump area.
//
// NODE 0's area is the very same main allocator (NEXT..END): the low
// memory always belongs to the first node, and giving it a second path would mean
// forking the kernel's busiest code for the sake of zero benefit. Additional paths
// appear only for THE REST of the nodes, whose memory on real machines
// lies above 4 GB. They are mapped identity by 1 GB AT ONCE, in `numa_init` — and
// that is not an optimisation but a condition of correctness: see there.
const MAX_NODES: usize = 8;
static NODE_BASE: [AtomicU64; MAX_NODES] = [const { AtomicU64::new(0) }; MAX_NODES]; // the start of the area (it does not move)
static NODE_NEXT: [AtomicU64; MAX_NODES] = [const { AtomicU64::new(0) }; MAX_NODES]; // bump
static NODE_END: [AtomicU64; MAX_NODES] = [const { AtomicU64::new(0) }; MAX_NODES];
/// The node the MAIN allocator belongs to (255 — there is no SRAT).
static MAIN_NODE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(255);

/// Build the per-node areas and then the main node's high area (`high_init`). It is called
/// AFTER `acpi::init` (before that there are no nodes yet). Without a SRAT there are no node
/// areas, and the high area is all the RAM above 4 GB.
pub fn numa_init() {
    if crate::acpi::nr_mem_regions() != 0 { node_areas_init(); }
    high_init();
}

fn node_areas_init() {
    let main = crate::acpi::node_of_phys(NEXT.load(Ordering::Relaxed));
    MAIN_NODE.store(main as u32, Ordering::Relaxed);
    // THE INTERSECTION of two maps: the SRAT says which node a range belongs to, while
    // the bootloader's map says where there is any RAM at all. Taking "the node by the region's start"
    // is not enough: the firmware is entitled to give one contiguous piece of RAM to two
    // nodes, and then half of the map will be described correctly while the other half silently will not.
    let (mb, me) = (NEXT.load(Ordering::Relaxed), END.load(Ordering::Relaxed));
    let nram = RAM_N.load(Ordering::Relaxed) as usize;
    for i in 0..crate::acpi::nr_mem_regions() {
        let (sb, sl, node) = crate::acpi::mem_region(i);
        if node == 255 || node as usize >= MAX_NODES || node == main || sl == 0 { continue; }
        let n = node as usize;
        for k in 0..nram.min(MAX_RAM) {
            let (rb, rl) = unsafe {
                (core::ptr::read_volatile(core::ptr::addr_of!(RAM_BASE[k])),
                 core::ptr::read_volatile(core::ptr::addr_of!(RAM_LEN[k])))
            };
            let mut b = sb.max(rb).max(kernel_top());
            // Not above `ident_ceiling`: on a two-socket machine with 256 GB the second
            // node's RAM crossed the process heap's gigabyte, and every process's heap
            // overlaid the identity of the frames there.
            let e = (sb + sl).min(rb + rl).min(ident_ceiling());
            if e <= b { continue; }
            // Do not climb into the main allocator: it already hands out those frames.
            if b < me && e > mb { b = me; if e <= b { continue; } }
            if e - b <= NODE_END[n].load(Ordering::Relaxed).saturating_sub(NODE_BASE[n].load(Ordering::Relaxed)) { continue; }
            let b0 = (b + FRAME - 1) & !(FRAME - 1);
            NODE_BASE[n].store(b0, Ordering::Relaxed);
            NODE_NEXT[n].store(b0, Ordering::Relaxed);
            NODE_END[n].store(e, Ordering::Relaxed);
        }
    }
    // Map the areas identity AT ONCE rather than as they are allocated.
    //
    // A process's address space is a COPY of the kernel's tables at the moment of its own
    // creation (`paging::new_process_space` copies the PDPT entries). A gigabyte
    // leaf added to the kernel's tables LATER will not get into such a copy — and
    // the kernel's very first write at the identity address of a frame from a foreign node gives a #PF from
    // ring 0. It is caught exactly like this: `#PF CR2=0x146000000 (not mapped, a write)`
    // with the area `0x140000000..0x200000000` alive.
    //
    // So we map BEFORE a single process address space appears.
    // An area that could not be mapped is discarded entirely: a node without
    // a mapping is not "slow memory" but a guaranteed crash.
    for n in 0..MAX_NODES {
        let (a, e) = (NODE_BASE[n].load(Ordering::Relaxed), NODE_END[n].load(Ordering::Relaxed));
        if e <= a { continue; }
        let mut g = a & !((1u64 << 30) - 1);
        let mut ok = true;
        while g < e { if crate::paging::map_ram_1g(g) == 0 { ok = false; break; } g += 1 << 30; }
        if !ok {
            crate::println!("mem: NUMA: the area of node {} ({:#x}..{:#x}) is NOT mapped — the node is discarded", n, a, e);
            NODE_BASE[n].store(0, Ordering::Relaxed);
            NODE_NEXT[n].store(0, Ordering::Relaxed);
            NODE_END[n].store(0, Ordering::Relaxed);
        }
    }
    crate::println!("mem: NUMA: the main allocator is on node {}, the separate areas:", main);
    let mut any = false;
    for n in 0..MAX_NODES {
        let (a, e) = (NODE_NEXT[n].load(Ordering::Relaxed), NODE_END[n].load(Ordering::Relaxed));
        if e > a { crate::println!("mem:   node {}: {:#x}..{:#x} ({} MB)", n, a, e, (e - a) / 1048576); any = true; }
    }
    // A node may lawfully have no area of its own: if ALL of its memory lies
    // inside the main allocator's range (a machine with one piece of RAM that
    // the firmware divided between the nodes), there is no separate pool for it — the frames
    // will come from the main one. That is not a refusal, but it is not locality either, so we say it.
    if !any { crate::println!("mem:   there are none (all the nodes' memory is covered by the main allocator)"); }
}

/// The node from which the main allocator takes frames (255 — there is no SRAT).
pub fn main_node() -> u8 { MAIN_NODE.load(Ordering::Relaxed) as u8 }

/// Allocate n contiguous frames PREFERABLY from the node `node`.
///
/// It is not a guarantee but a preference: if the node has no area of its own or it is
/// exhausted, the frames will come from the main allocator. Otherwise the NUMA policy would be able
/// to bring down the workload's launch on a machine where its node has run out — and that is worse than
/// slow memory.
///
/// The main node's memory is its high area first and the low allocator after it: the
/// callers are the workload's pages, and the low memory is what the kernel's own
/// structures cannot do without (`high_init`).
pub fn alloc_contig_node(n: u64, node: u8) -> Option<u64> {
    let idx = node as usize;
    if node == 255 || idx >= MAX_NODES || node == main_node() { return alloc_main(n); }
    // This node's free blocks (returned by free_contig) come before the bump.
    if let Some(a) = fl_node_take(n, NODE_BASE[idx].load(Ordering::Relaxed), NODE_END[idx].load(Ordering::Relaxed)) {
        crate::metrics::add_frames(n);
        return Some(a);
    }
    loop {
        let base = NODE_NEXT[idx].load(Ordering::Relaxed);
        if base == 0 { return alloc_main(n); }         // the node has no area of its own
        let end = base + n * FRAME;
        if end > NODE_END[idx].load(Ordering::Relaxed) { return alloc_main(n); } // the node is exhausted
        if NODE_NEXT[idx].compare_exchange(base, end, Ordering::Relaxed, Ordering::Relaxed).is_err() { continue; }
        // There is NOTHING to map here: the whole area is mapped in `numa_init`, before
        // the first process address space is created (see there — why
        // it cannot be deferred).
        crate::metrics::add_frames(n);
        return Some(base);
    }
}

/// Cut the low allocator down to `mb` MB from its start (`talaria.lowmem`, a gate's knob):
/// what a machine whose RAM is mostly above 4 GB leaves the kernel. Called before
/// `numa_init`; only the bump's end moves, nothing handed out is touched.
pub fn cap_low_mb(mb: u64) {
    let end = ALLOC_BASE.load(Ordering::Relaxed) + mb * 1048576;
    if end < END.load(Ordering::Relaxed) && end > NEXT.load(Ordering::Relaxed) {
        END.store(end, Ordering::Relaxed);
        crate::println!("mem: the low allocator is cut to {} MB (talaria.lowmem) — up to {:#x}", mb, end);
    }
}

/// The main node's high area as it was built (the gate `highmem`; (0, 0) — there is none).
pub fn high_area() -> (u64, u64) { (HIGH_BASE.load(Ordering::Relaxed), HIGH_END.load(Ordering::Relaxed)) }

/// What the workload may NOT take of the low memory when it has a high area to spend: the
/// kernel's own structures (the page tables of its processes above all) live there and have
/// nowhere else to go. Found on a `c7g.large`: `forktls`'s 1.5 GB went into 2 MB pages from
/// the low memory (~600 MB after the FS cache), and the next page table found no frame —
/// a kernel panic one syscall into the workload.
const LOW_RESERVE_FRAMES: u64 = 64 * 256; // 64 MB

/// May the workload's allocation of `n` frames fall back to the low allocator.
fn low_for_workload(n: u64) -> bool {
    HIGH_BASE.load(Ordering::Relaxed) == 0 || free_low_frames() >= n + LOW_RESERVE_FRAMES
}

/// The main node's frames for the workload: the high area, then the low allocator (above
/// its reserve, when a high area exists).
fn alloc_main(n: u64) -> Option<u64> {
    if n == 0 { return None; }
    let (hb, he) = (HIGH_BASE.load(Ordering::Relaxed), HIGH_END.load(Ordering::Relaxed));
    if hb != 0 {
        if let Some(a) = fl_node_take(n, hb, he) { crate::metrics::add_frames(n); return Some(a); }
        loop {
            let base = HIGH_NEXT.load(Ordering::Relaxed);
            let end = base + n * FRAME;
            if end > he { break; }
            if HIGH_NEXT.compare_exchange(base, end, Ordering::Relaxed, Ordering::Relaxed).is_err() { continue; }
            crate::metrics::add_frames(n);
            return Some(base);
        }
    }
    if !low_for_workload(n) { return None; }
    alloc_contig(n)
}

/// The workload's ALIGNED frames (a 2 MB page): the high area, then the low allocator above
/// its reserve. `paging::lazy_fault` takes its huge pages here — `alloc_contig_aligned`
/// alone took them from the low memory only.
pub fn alloc_workload_aligned(n: u64, align: u64) -> Option<u64> {
    if n == 0 { return None; }
    let (hb, he) = (HIGH_BASE.load(Ordering::Relaxed), HIGH_END.load(Ordering::Relaxed));
    if hb != 0 {
        if let Some(a) = fl_node_take_aligned(n, align, hb, he) { crate::metrics::add_frames(n); return Some(a); }
        loop {
            let base = HIGH_NEXT.load(Ordering::Relaxed);
            let a = (base + align - 1) & !(align - 1);
            let end = a + n * FRAME;
            if end > he { break; }
            if HIGH_NEXT.compare_exchange(base, end, Ordering::Relaxed, Ordering::Relaxed).is_err() { continue; }
            // The alignment's gap goes into the area's free list rather than being lost.
            if a > base { fl_node_put(base, (a - base) / FRAME); }
            crate::metrics::add_frames(n);
            return Some(a);
        }
    }
    if !low_for_workload(n) { return None; }
    alloc_contig_aligned(n, align)
}

// ── the free list of released contiguous blocks (real reclaim, not only a bump) ──
const FL_MAX: usize = 512;
static mut FL_ADDR: [u64; FL_MAX] = [0; FL_MAX];
static mut FL_PAGES: [u64; FL_MAX] = [0; FL_MAX];
static mut FL_N: usize = 0;
static FL_LOCK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn fl_lock() {
    while FL_LOCK.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
}
fn fl_unlock() { FL_LOCK.store(false, Ordering::Release); }

// ── the free list OF THE PER-NODE areas ─────────────────────────────────────
//
// Separate from the main one, and that is not pedantry. A frame from a foreign node lies above
// 4 GB and is mapped identity BY GIGABYTE LEAVES; having got into the common free list, it
// would be handed to the very first `alloc_contig` — for page tables, a DMA ring,
// a kernel stack — that is, to code that expects a 4K leaf and calls `unguard`.
// The error class is the same as "the largest RAM region turned out to be above 4 GB" from A2:
// a silent crash where the memory is simply "not like that".
//
// The main node's high area shares this list (its frames are of the same kind), and with it
// the traffic of every process's exit — as many slots as the main list has, because a
// block that finds the list full is lost.
const FLN_MAX: usize = 512;
static mut FLN_ADDR: [u64; FLN_MAX] = [0; FLN_MAX];
static mut FLN_PAGES: [u64; FLN_MAX] = [0; FLN_MAX];
static mut FLN_N: usize = 0;

/// Does the address lie in the area of some node or in the high area (not in the main allocator).
fn in_node_area(addr: u64) -> bool {
    let hb = HIGH_BASE.load(Ordering::Relaxed);
    if hb != 0 && addr >= hb && addr < HIGH_END.load(Ordering::Relaxed) { return true; }
    for n in 0..MAX_NODES {
        let b = NODE_BASE[n].load(Ordering::Relaxed);
        if b != 0 && addr >= b && addr < NODE_END[n].load(Ordering::Relaxed) { return true; }
    }
    false
}

/// Take a block from the per-node free list within the bounds [lo, hi).
fn fl_node_take(n: u64, lo: u64, hi: u64) -> Option<u64> {
    if lo == 0 || n == 0 { return None; }
    fl_lock();
    unsafe {
        for i in 0..FLN_N {
            if FLN_PAGES[i] < n { continue; }
            let addr = FLN_ADDR[i];
            if addr < lo || addr + n * FRAME > hi { continue; }
            FLN_ADDR[i] += n * FRAME;
            FLN_PAGES[i] -= n;
            if FLN_PAGES[i] == 0 { FLN_N -= 1; FLN_ADDR[i] = FLN_ADDR[FLN_N]; FLN_PAGES[i] = FLN_PAGES[FLN_N]; }
            fl_unlock();
            return Some(addr);
        }
    }
    fl_unlock();
    None
}

/// As `fl_node_take`, an aligned piece of n frames inside a block within [lo, hi); the
/// block's head before the piece stays a block of its own, the tail goes back.
fn fl_node_take_aligned(n: u64, align: u64, lo: u64, hi: u64) -> Option<u64> {
    if lo == 0 || n == 0 { return None; }
    fl_lock();
    unsafe {
        for i in 0..FLN_N {
            let base = FLN_ADDR[i];
            let a = (base + align - 1) & !(align - 1);
            let pad = (a - base) / FRAME;
            if FLN_PAGES[i] < pad + n || base < lo || a + n * FRAME > hi { continue; }
            let tail_pages = FLN_PAGES[i] - pad - n;
            if pad > 0 { FLN_PAGES[i] = pad; } else {
                FLN_N -= 1; FLN_ADDR[i] = FLN_ADDR[FLN_N]; FLN_PAGES[i] = FLN_PAGES[FLN_N];
            }
            if tail_pages > 0 && FLN_N < FLN_MAX { FLN_ADDR[FLN_N] = a + n * FRAME; FLN_PAGES[FLN_N] = tail_pages; FLN_N += 1; }
            fl_unlock();
            return Some(a);
        }
    }
    fl_unlock();
    None
}

/// Return a block into the per-node free list (with coalescing, as in the main one).
fn fl_node_put(addr: u64, n: u64) {
    fl_lock();
    unsafe {
        let mut a = addr; let mut pages = n; let mut i = 0;
        while i < FLN_N {
            let bend = FLN_ADDR[i] + FLN_PAGES[i] * FRAME;
            if bend == a { a = FLN_ADDR[i]; pages += FLN_PAGES[i]; FLN_N -= 1; FLN_ADDR[i] = FLN_ADDR[FLN_N]; FLN_PAGES[i] = FLN_PAGES[FLN_N]; continue; }
            if a + pages * FRAME == FLN_ADDR[i] { pages += FLN_PAGES[i]; FLN_N -= 1; FLN_ADDR[i] = FLN_ADDR[FLN_N]; FLN_PAGES[i] = FLN_PAGES[FLN_N]; continue; }
            i += 1;
        }
        if FLN_N < FLN_MAX { FLN_ADDR[FLN_N] = a; FLN_PAGES[FLN_N] = pages; FLN_N += 1; }
    }
    fl_unlock();
}

/// Allocate n contiguous zeroed frames from the high area (RAM ABOVE 4 GB) with the
/// alignment align (in bytes). None — there is no such memory. It is used for the ENA rings
/// and the grown refcount table; the area is mapped already (`high_init`).
pub fn alloc_high(pages: u64, align: u64) -> Option<u64> {
    let cur = loop {
        let base = HIGH_NEXT.load(Ordering::Relaxed);
        if base == 0 { return None; }
        let cur = (base + align - 1) & !(align - 1);
        let end = cur + pages * FRAME;
        if end > HIGH_END.load(Ordering::Relaxed) { return None; }
        // A CAS, not a store: the workload's frames come from the same bump (`alloc_main`).
        if HIGH_NEXT.compare_exchange(base, end, Ordering::Relaxed, Ordering::Relaxed).is_ok() { break cur; }
    };
    unsafe { core::ptr::write_bytes(cur as *mut u8, 0, (pages * FRAME) as usize) };
    Some(cur)
}

/// Allocate one physical frame of 4 KB. None — exhausted.
pub fn alloc_frame() -> Option<u64> { alloc_contig(1) }

/// Allocate n contiguous frames aligned to `align` bytes (a power of two).
/// It is needed for 2M pages: the physical base of a 2M leaf must be a multiple of 2 MB.
/// If it did not work out to align — None (the caller will fall back to 4K pages).
pub fn alloc_contig_aligned(n: u64, align: u64) -> Option<u64> {
    if n == 0 || align < FRAME { return alloc_contig(n); }
    // The free list: we look for a block inside which an aligned piece fits.
    fl_lock();
    let (rb, re) = (RES_BASE.load(Ordering::Relaxed), RES_END.load(Ordering::Relaxed));
    unsafe {
        for i in 0..FL_N {
            let base = FL_ADDR[i];
            let a = (base + align - 1) & !(align - 1);
            let pad = (a - base) / FRAME;
            // The reserved window (the workload's image at its identity address) is bypassed
            // here too: the kernel zeroes the piece it handed out BY ITS PHYSICAL address as if by a
            // virtual one, and a frame from the window means a write into the workload's code. On
            // the 2M path that brought down not the workload any more but THE KERNEL: a #PF "a write into a present one"
            // right in the memset at 0x10000000.
            if re > rb && a < re && a + n * FRAME > rb { continue; }
            if FL_PAGES[i] >= pad + n {
                // the tail after the piece stays in the list; the "head" (pad) goes as a separate block
                let tail_addr = a + n * FRAME;
                let tail_pages = FL_PAGES[i] - pad - n;
                if pad > 0 { FL_PAGES[i] = pad; } else {
                    FL_N -= 1; FL_ADDR[i] = FL_ADDR[FL_N]; FL_PAGES[i] = FL_PAGES[FL_N];
                }
                if tail_pages > 0 && FL_N < FL_MAX { FL_ADDR[FL_N] = tail_addr; FL_PAGES[FL_N] = tail_pages; FL_N += 1; }
                fl_unlock();
                crate::paging::unguard(a, n * FRAME);
                crate::metrics::add_frames(n);
                return Some(a);
            }
        }
    }
    fl_unlock();
    // bump: move the pointer up to the alignment
    loop {
        let base = NEXT.load(Ordering::Relaxed);
        let a = (base + align - 1) & !(align - 1);
        let end = a + n * FRAME;
        if re > rb && end > rb && a < re {
            let _ = NEXT.compare_exchange(base, re, Ordering::Relaxed, Ordering::Relaxed);
            continue;
        }
        if end > END.load(Ordering::Relaxed) { return None; }
        if NEXT.compare_exchange(base, end, Ordering::Relaxed, Ordering::Relaxed).is_err() { continue; }
        crate::metrics::add_frames(n);
        return Some(a);
    }
}

/// Allocate n contiguous frames: first a first-fit in the free list (reclaim), otherwise the bump.
pub fn alloc_contig(n: u64) -> Option<u64> {
    if n == 0 { return None; }
    fl_lock();
    let (rb, re) = (RES_BASE.load(Ordering::Relaxed), RES_END.load(Ordering::Relaxed));
    unsafe {
        for i in 0..FL_N {
            if FL_PAGES[i] >= n {
                let addr = FL_ADDR[i];
                // THE RESERVED WINDOW IS BYPASSED BY THE FREE LIST TOO, not only by the bump.
                // The window is the physical addresses that in the workload's address
                // space are OVERLAID by its own image (the binary is linked at
                // 0x10000000, for MariaDB that is 185 MB). The kernel writes into a frame at its
                // identity address, and a frame from that window would mean a write into the workload's
                // code. The bump jumped over such addresses, while the free list handed
                // them back, and a "fresh" page was not allocated at all: the workload
                // got a SIGSEGV on its own memory (MariaDB — in a memset over a
                // just-mapped anonymous area).
                if re > rb && addr < re && addr + n * FRAME > rb { continue; }
                FL_ADDR[i] += n * FRAME;
                FL_PAGES[i] -= n;
                if FL_PAGES[i] == 0 { FL_N -= 1; FL_ADDR[i] = FL_ADDR[FL_N]; FL_PAGES[i] = FL_PAGES[FL_N]; }
                fl_unlock();
                // The frame may have been extinguished by a munmap guard: return present for the kernel's needs
                // (the page tables/DMA access it at its identity address).
                crate::paging::unguard(addr, n * FRAME);
                crate::metrics::add_frames(n);
                return Some(addr);
            }
        }
    }
    fl_unlock();
    // a bump bypassing the reserved region (the workload's image loaded at an identity
    // address, e.g. a workload at 0x30000000): a block touching it jumps beyond its end.
    loop {
        let base = NEXT.load(Ordering::Relaxed);
        let end = base + n * FRAME;
        if re > rb && end > rb && base < re {
            let _ = NEXT.compare_exchange(base, re, Ordering::Relaxed, Ordering::Relaxed);
            continue;
        }
        if NEXT.compare_exchange(base, end, Ordering::Relaxed, Ordering::Relaxed).is_err() { continue; }
        if end <= END.load(Ordering::Relaxed) { crate::metrics::add_frames(n); return Some(base); } else { return None; }
    }
}
static RES_BASE: AtomicU64 = AtomicU64::new(0);
static RES_END: AtomicU64 = AtomicU64::new(0);
/// Reserve the physical region [base, base+len) from the bump allocator (the identity
/// image of the workload). If the bump is already inside the region — move it beyond its end.
pub fn reserve(base: u64, len: u64) {
    let b = base & !(FRAME - 1);
    let e = (base + len + FRAME - 1) & !(FRAME - 1);
    RES_BASE.store(b, Ordering::Relaxed); RES_END.store(e, Ordering::Relaxed);
    let n = NEXT.load(Ordering::Relaxed);
    if n >= b && n < e { NEXT.store(e, Ordering::Relaxed); }
}

/// Return a contiguous block into the free list (coalescing with the adjoining blocks).
pub fn free_contig(addr: u64, n: u64) {
    if n == 0 || addr == 0 { return; }
    // A frame from a foreign node is returned into ITS OWN list: in the common one it would go to
    // the very first alloc_contig — to code that expects memory below 4 GB with 4K leaves.
    if in_node_area(addr) { fl_node_put(addr, n); return; }
    fl_lock();
    unsafe {
        let mut a = addr;
        let mut pages = n;
        let mut i = 0;
        while i < FL_N {
            let bend = FL_ADDR[i] + FL_PAGES[i] * FRAME;
            if bend == a { a = FL_ADDR[i]; pages += FL_PAGES[i]; FL_N -= 1; FL_ADDR[i] = FL_ADDR[FL_N]; FL_PAGES[i] = FL_PAGES[FL_N]; continue; }
            if a + pages * FRAME == FL_ADDR[i] { pages += FL_PAGES[i]; FL_N -= 1; FL_ADDR[i] = FL_ADDR[FL_N]; FL_PAGES[i] = FL_PAGES[FL_N]; continue; }
            i += 1;
        }
        if FL_N < FL_MAX { FL_ADDR[FL_N] = a; FL_PAGES[FL_N] = pages; FL_N += 1; }
        // the free list is full -> the block is lost (a leak is better than a panic); in practice it does not happen
    }
    fl_unlock();
}

/// The window of physical addresses reserved for the workload's image
/// (the binary's identity address). The frame allocator bypasses it; the check of the identity
/// invariant (`paging::ident_check`) needs it in order not to count a lawful substitution
/// as a defect: there the workload maps ITS OWN private frames.
pub fn reserved() -> (u64, u64) { (RES_BASE.load(Ordering::Relaxed), RES_END.load(Ordering::Relaxed)) }

/// Does the frame lie in the free list (the gate `sharedpd`: the kernel's shared directory after
/// the cleanup of a process's address space must NOT end up among the free ones).
pub fn frame_is_free(phys: u64) -> bool {
    fl_lock();
    let mut hit = false;
    unsafe {
        for i in 0..FL_N {
            if phys >= FL_ADDR[i] && phys < FL_ADDR[i] + FL_PAGES[i] * FRAME { hit = true; break; }
        }
    }
    fl_unlock();
    hit
}

/// How many pages there are in the free lists — the main one and the areas' (for telemetry/tests).
pub fn free_list_pages() -> u64 {
    fl_lock();
    let mut s = 0u64;
    unsafe {
        for i in 0..FL_N { s += FL_PAGES[i]; }
        for i in 0..FLN_N { s += FLN_PAGES[i]; }
    }
    fl_unlock();
    s
}

/// From which address the allocator started handing out frames. It is needed by exactly one check
/// — that it started ABOVE the supervisor leaf with the kernel's tail (see `paging::init`).
pub fn alloc_base() -> u64 { ALLOC_BASE.load(Ordering::Relaxed) }
static ALLOC_BASE: AtomicU64 = AtomicU64::new(0);

/// How many frames are free: the bumps' remainders — the main allocator, the high area and
/// the nodes' areas (without the free lists, see `free_total_frames`).
pub fn free_frames() -> u64 {
    let left = |n: u64, e: u64| if e > n { (e - n) / FRAME } else { 0 };
    let mut s = left(NEXT.load(Ordering::Relaxed), END.load(Ordering::Relaxed));
    if HIGH_BASE.load(Ordering::Relaxed) != 0 {
        s += left(HIGH_NEXT.load(Ordering::Relaxed), HIGH_END.load(Ordering::Relaxed));
    }
    for n in 0..MAX_NODES {
        if NODE_BASE[n].load(Ordering::Relaxed) != 0 {
            s += left(NODE_NEXT[n].load(Ordering::Relaxed), NODE_END[n].load(Ordering::Relaxed));
        }
    }
    s
}
/// The low allocator's remainder alone (bump + its free list): what the kernel's own
/// structures — page tables, kernel stacks, the FS block cache — can still get.
pub fn free_low_frames() -> u64 {
    let (n, e) = (NEXT.load(Ordering::Relaxed), END.load(Ordering::Relaxed));
    let mut s = if e > n { (e - n) / FRAME } else { 0 };
    fl_lock();
    unsafe { for i in 0..FL_N { s += FL_PAGES[i]; } }
    fl_unlock();
    s
}
/// The full free amount: the bump's remainder + the free list (reclaim). For telemetry/leak control:
/// free_frames alone understates it — the returned blocks live in the free list without moving NEXT.
pub fn free_total_frames() -> u64 { free_frames() + free_list_pages() }

/// How much PHYSICAL memory the machine has at all (frames of 4 KB, by the
/// e820/PVH map). Without it `top` can say "5 GB free", but not "out of how much".
pub fn ram_total_frames() -> u64 {
    let n = RAM_N.load(Ordering::Relaxed) as usize;
    let mut s = 0u64;
    unsafe { for i in 0..n.min(MAX_RAM) { s += core::ptr::read_volatile(core::ptr::addr_of!(RAM_LEN[i])) / FRAME; } }
    s
}
/// The number of RAM areas and one area by index (`kstat mem`).
pub fn ram_regions() -> usize { (RAM_N.load(Ordering::Relaxed) as usize).min(MAX_RAM) }
pub fn ram_region(i: usize) -> (u64, u64) {
    if i >= ram_regions() { return (0, 0); }
    unsafe { (core::ptr::read_volatile(core::ptr::addr_of!(RAM_BASE[i])),
              core::ptr::read_volatile(core::ptr::addr_of!(RAM_LEN[i]))) }
}
/// How much memory the kernel itself occupies (the image + .bss), in frames.
pub fn kernel_frames() -> u64 { (kernel_hi().saturating_sub(kernel_lo())) / FRAME }

fn println_mem(msg: &str) { crate::println!("talaria: mem: {}", msg); }

// ── the refcount of physical frames for COW (it removes the orphan leak at a fork) ──
//
// THE TABLE'S COVERAGE IS CORRECTNESS, NOT AN OPTIMISATION.
//
// It was: `1 << 18` frames, that is 1 GB, with the note "our RAM is <512 MB". On the
// EC2 stand the kernel prints `RAM in total 8047 MB; the frame allocator ... (3025 MB)` — two
// thirds of the frames handed out lay OUTSIDE the table, and for them `rc_get` silently returned
// A ZERO. And a zero means "there are no co-owners", and both consumers understood that
// literally:
//
//   cow_fault:  if rc_get(phys) <= 1 { make the page writable IN PLACE }
//               -> the parent's and the child's shared frame becomes writable without a
//                  copy, and they write into each other's memory;
//   unmap/exit: if rc_get(phys) > 1 { rc_drop } else { free_contig(phys) }
//               -> the frame is returned to the allocator WHILE THE SECOND OWNER IS ALIVE and
//                  is handed out to somebody else.
//
// From the outside it looked like this: PostgreSQL on the stand died around `fork` — now a
// backend on a garbage `%fs:0`, now THE POSTMASTER with an exception #GP(0) on the `retq` at the
// end of `_Fork` (a non-canonical return address, that is, ITS OWN stack corrupted),
// and the whole database died after it. It did not reproduce locally exactly because
// we ran with 512-1024 MB, where all the frames fall into the table; with 1.5 GB of address
// space the parent gets corrupted on the SECOND fork (the gate `forktls`).
//
// Hence two rules. The table covers 16 GB (`c6i.2xlarge` is the largest
// type the stand runs on), that is 4 MB of .bss. And a frame OUTSIDE the table is considered
// SHARED: an extra copy and a leaked frame are a price that can be paid,
// the silent corruption of somebody else's memory is not.
//
// A machine whose RAM reaches higher gets a larger table at the bring-up (`rc_grow`): RC_PTR
// and RC_LEN then name it, and the static one is no longer used. 0 — the static table.
const RC_MAX: usize = 1 << 22; // 4 194 304 frames = 16 GB
static mut REFCNT: [u8; RC_MAX] = [0; RC_MAX];
static RC_PTR: AtomicU64 = AtomicU64::new(0);
static RC_LEN: AtomicU64 = AtomicU64::new(0);
/// How many times the counter was asked about a frame OUTSIDE the table. A zero in a healthy system;
/// a non-zero one means forced copies and leaks rather than corruption, but the machine has more
/// memory than the table covers, and one must know about that.
pub static RC_OUTSIDE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The table in use: (its base, how many frames it covers).
#[inline]
fn rc_table() -> (*mut u8, usize) {
    let p = RC_PTR.load(Ordering::Acquire);
    if p == 0 { (core::ptr::addr_of_mut!(REFCNT) as *mut u8, RC_MAX) }
    else { (p as *mut u8, RC_LEN.load(Ordering::Relaxed) as usize) }
}
/// How many frames the refcount table covers (for the check at the bring-up).
pub fn rc_capacity_frames() -> u64 { rc_table().1 as u64 }

/// Set a frame's refcount (the number of COW references).
pub fn rc_set(phys: u64, v: u8) {
    let i = (phys >> 12) as usize;
    let (t, len) = rc_table();
    if i < len { unsafe { core::ptr::write_volatile(t.add(i), v); } }
}
/// Get the refcount. OUTSIDE the table it is "shared" (2) rather than "free" (0): see the header.
pub fn rc_get(phys: u64) -> u8 {
    let i = (phys >> 12) as usize;
    let (t, len) = rc_table();
    if i < len { unsafe { core::ptr::read_volatile(t.add(i)) } }
    else { RC_OUTSIDE.fetch_add(1, core::sync::atomic::Ordering::Relaxed); 2 }
}
/// +1 reference (for a fork). If it was 0 -> it becomes 2 (the parent+the child).
pub fn rc_share(phys: u64) { let c = rc_get(phys); rc_set(phys, if c == 0 { 2 } else { c.saturating_add(1) }); }
/// -1 reference, return the new value.
pub fn rc_drop(phys: u64) -> u8 { let c = rc_get(phys); let n = c.saturating_sub(1); rc_set(phys, n); n }

// ── The accounting of the workload's identity mmap regions (the kernel-AS modes: run_disk/run_disk_threaded) ──
// An anonymous mmap in identity mode takes frames with alloc_contig; munmap returns them, but
// a crashed/restarted instance does not call munmap — the mallocng arenas (hundreds of MB for large
// C++) were lost forever ("the memory across restarts"). At a restart supervise frees
// the surviving regions through mreg_reset_free. Process mode (a per-process AS) does not come here —
// it has free_address_space by refcount.
const MREG_MAX: usize = 1024;
static mut MREG_A: [u64; MREG_MAX] = [0; MREG_MAX]; // the region's base (0 = the slot is empty)
static mut MREG_P: [u64; MREG_MAX] = [0; MREG_MAX]; // the pages
static MREG_OVF: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Register an identity mmap region (a call from the mmap syscall, IF=0).
pub fn mreg_add(base: u64, pages: u64) {
    if base == 0 || pages == 0 { return; }
    unsafe {
        for i in 0..MREG_MAX {
            if MREG_A[i] == 0 { MREG_A[i] = base; MREG_P[i] = pages; return; }
        }
    }
    // The table is full: the region is not accounted for (it will leak at a restart). Not fatal.
    if !MREG_OVF.swap(true, core::sync::atomic::Ordering::Relaxed) {
        crate::println!("mem: the table of mmap regions is full — the accounting across restarts is incomplete");
    }
}
/// Remove the accounting from a range (munmap): a full coverage removes the slot, a partial one trims/splits it.
pub fn mreg_del(addr: u64, pages: u64) {
    let (s, e) = (addr, addr + pages * 4096);
    unsafe {
        for i in 0..MREG_MAX {
            if MREG_A[i] == 0 { continue; }
            let (rs, re) = (MREG_A[i], MREG_A[i] + MREG_P[i] * 4096);
            if e <= rs || s >= re { continue; }
            let head = if s > rs { (s - rs) / 4096 } else { 0 };
            let tail = if e < re { (re - e) / 4096 } else { 0 };
            MREG_A[i] = 0; MREG_P[i] = 0;
            if head > 0 { mreg_add(rs, head); }
            if tail > 0 { mreg_add(e, tail); }
        }
    }
}
/// Free ALL the accounted regions (a restart of an instance under supervise): remove
/// ring 3's access (a future code's old pointers give no UAF), keeping identity for the
/// kernel, + the return of the frames. It returns (regions, pages).
pub fn mreg_reset_free() -> (u64, u64) {
    let (mut nr, mut np) = (0u64, 0u64);
    unsafe {
        for i in 0..MREG_MAX {
            if MREG_A[i] == 0 { continue; }
            let (base, pages) = (MREG_A[i], MREG_P[i]);
            crate::paging::unmap_ident(base, pages);
            free_contig(base, pages);
            MREG_A[i] = 0; MREG_P[i] = 0;
            nr += 1; np += pages;
        }
    }
    MREG_OVF.store(false, core::sync::atomic::Ordering::Relaxed);
    (nr, np)
}
