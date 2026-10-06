//! Self-checks of the memory subsystem: mmap/munmap, W^X, guard pages, races.
//!
//! The mode's name is compared IN FULL (a `match` on the value of `talaria.test=`),
//! so a name that is a prefix of another can no longer start someone else's mode
//! — see `crate::cmdline`.

/// Run the self-check `name` if it belongs to this set.
/// Returns `true` if the name was recognised (the body may not have returned — many
/// modes end with `suite::park()`).
// Many gates do not return at all (`suite::park()` or starting a workload),
// so in sets where EVERY mode does that the trailing `true` is unreachable.
#[allow(unused_variables, unreachable_code)]
pub fn run(name: &str, cmdline: &str) -> bool {
    match name {
        "restartmem" => {
            // memory across restarts — memhog (a 64 MB mmap + a crash) under
            // supervise_threaded; the assert: the free drift between restarts is ~0 (the regions were returned).
            println!("talaria: restartmem — memory accounting across supervise restarts (memhog 64MB x3)");
            if crate::fs::init() {
                crate::supervise::supervise_threaded(b"/memhog\0".as_ptr(), &[b"memhog"], &[], 2);
            }
        }
        "memrace" => {
            crate::smp::start_from_cmdline(cmdline); // talaria.smp=N applies here too
            // Parallel work with memory: lazy pages, large pages,
            // mprotect, MADV_DONTNEED and TLB shootdowns between cores under load.
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/memrace", &[b"memrace"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "mem" => {
            static MT: &[u8] = user_elf!("memtest");
            println!("talaria: memtest {} bytes (2000x mmap+munmap, a reclaim check)", MT.len());
            crate::user::run(MT);
        }
        "wxorx" => {
            static MT2: &[u8] = user_elf!("memtest");
            println!("talaria: the memory protection self-test (W^X + NX + guard + a live #PF)");
            crate::user::wxorx_test(MT2);
        }
        "devnull" => {
            static DN: &[u8] = user_elf!("devnull.elf");
            println!("talaria: the fd table and /dev/null test");
            crate::user::run(DN);
        }
        "uaf" => {
            static UAF: &[u8] = user_elf!("uaftest");
            println!("talaria: the UAF test — an access after munmap must give a #PF");
            crate::user::run_expect_fault(UAF);
        }
        "allocfail" => {
            // A REFUSED REQUEST LEAVES THE ALLOCATOR WHOLE (docs/ENGINEERING-NOTES.md item 49):
            // one more frame than the bump has left must fail, the bump must keep every frame it
            // had, and the next small allocation must still succeed. The old order moved the
            // pointer before the bound check — the refusal took the rest of the bump with it.
            let left = crate::mem::bump_left();
            let refused = crate::mem::alloc_contig(left + 1).is_none();
            let after = crate::mem::bump_left();
            let small = crate::mem::alloc_contig(16);
            println!("allocfail: the bump had {} frames, a request of {} refused={}, it has {} after, a 64 KB block after it: {}",
                left, left + 1, refused, after, if small.is_some() { "yes" } else { "NO" });
            if let Some(b) = small { crate::mem::free_contig(b, 16); }
            if refused && after == left && small.is_some() { println!("allocfail: ALL OK"); } else { println!("allocfail: FAIL"); }
            crate::suite::park()
        }
        "dmapid" => {
            // THE ADDRESS KINDS ROUND-TRIP (docs/ARM64-PLAN.md §10.16, stage 1): for a frame of
            // the low allocator, of the high area, a `.bss` word and a stack word, the physical
            // address -> the kernel's pointer -> the physical address is the same number, and a
            // word written through `kptr` is read back through `kptr` of `kpa_of` of that pointer.
            // Stage 1 makes every helper the identity, so this pins the contract the direct map
            // (stage 2) must keep while it changes what `kva` returns.
            static mut BSS_WORD: u64 = 0;
            let stack_word: u64 = 0;
            let low = crate::mem::alloc_frame();
            let (hb, _) = crate::mem::high_area();
            let high = if hb != 0 { crate::mem::alloc_high(1, 4096) } else { None };
            let bss = crate::mem::kpa_of(core::ptr::addr_of!(BSS_WORD));
            let stk = crate::mem::kpa_of(&stack_word as *const u64);
            let mut bad = 0u32;
            let mut check = |what: &str, pa: u64| {
                let p = crate::mem::kptr::<u64>(pa);
                let back = crate::mem::kpa_of(p);
                // SAFETY: `pa` is RAM this test owns (a fresh frame, its own `.bss`/stack word).
                let rt = unsafe {
                    let old = core::ptr::read_volatile(p);
                    core::ptr::write_volatile(p, 0xD1A9_0000_0000_0000 | pa);
                    let v = core::ptr::read_volatile(crate::mem::kptr::<u64>(back));
                    core::ptr::write_volatile(p, old);
                    v
                };
                let ok = back == pa && crate::mem::kpa(crate::mem::kva(pa)) == pa && rt == (0xD1A9_0000_0000_0000 | pa);
                println!("dmapid: {} pa={:#x} kva={:#x} back={:#x} {}", what, pa, crate::mem::kva(pa), back, if ok { "OK" } else { "FAIL" });
                if !ok { bad += 1; }
            };
            match low { Some(f) => check("low frame", f), None => { println!("dmapid: no low frame"); } }
            match high { Some(f) => check("high frame", f), None => println!("dmapid: high frame: no high area (skipped)") }
            check(".bss word", bss);
            check("stack word", stk);
            let v = crate::mem::vaddr(core::ptr::addr_of!(BSS_WORD));
            if crate::mem::vaddr(crate::mem::vptr::<u64>(v)) != v { bad += 1; println!("dmapid: vptr/vaddr FAIL"); }
            if let Some(f) = low { crate::mem::free_contig(f, 1); }
            if low.is_some() && bad == 0 { println!("dmapid: ALL OK"); } else { println!("dmapid: FAIL ({} bad)", bad); }
            crate::suite::park()
        }
        "identhole" => {
            // A MINE OF THE CLASS "an identity address under someone else's AS". The kernel addresses physical
            // frames by their identity address (zeroing a fresh frame, a COW copy,
            // kernel stacks, walking the tables, the FS block cache). In THE WORKLOAD's
            // address space the low region is its own copy
            // of the GB0 directory, and the workload is entitled to punch a hole in it with its own `munmap`.
            // A NON-PRESENT stub used to be left in place of the removed page, and
            // the kernel's very first access to that frame under that AS gave a #PF in
            // ring 0 and killed the machine (caught on PostgreSQL as "it stopped
            // accepting connections"). The gate does exactly that and in the same second
            // requires the kernel to write by the identity address.
            println!("talaria: identhole — the identity window after the workload's munmap");
            let orig = crate::paging::current_cr3();
            const N: u64 = 16;
            let frame = match crate::mem::alloc_contig(N) { Some(f) => f, None => { println!("identhole: FAILED (no frames)"); crate::suite::park() } };
            let asp = crate::paging::new_process_space();
            unsafe { crate::paging::switch(asp); }
            // like the workload's MAP_SHARED: present + U/S in its own copy of GB0
            crate::paging::protect(frame, N * 4096, true, false);
            unsafe { core::ptr::write_volatile(crate::mem::kptr::<u64>(frame), 0x1234_5678) }; // DMAP: identhole is about the identity itself (stage 4 rewrites it)
            crate::paging::unmap_proc(frame, N);            // the workload's munmap
            // THE KERNEL touches the same frames by their identity address while staying under the workload's AS
            let mut rw_ok = true;
            unsafe {
                for p in 0..N {
                    let a = frame + p * 4096;
                    core::ptr::write_volatile(crate::mem::kptr::<u64>(a), 0xA5A5_0000 + p);
                    if core::ptr::read_volatile(crate::mem::kptr::<u64>(a)) != 0xA5A5_0000 + p { rw_ok = false; }
                }
            }
            let (holes, shadow, ro) = crate::paging::ident_check();
            // ring 3 must NOT have access to the removed page: U/S is cleared
            let user_gone = crate::paging::leaf_entry(frame).map(|e| e & 4 == 0).unwrap_or(false);
            unsafe { crate::paging::switch(orig); }
            crate::paging::free_address_space(asp);
            crate::mem::free_contig(frame, N);
            println!("identhole: the kernel's write by identity after munmap: {}", if rw_ok { "OK" } else { "FAILED" });
            println!("identhole: U/S cleared on the removed page: {}", if user_gone { "OK" } else { "FAILED" });
            println!("identhole: the identity invariant in the workload's AS: holes {} substitutions {} read-only {}", holes, shadow, ro);
            if rw_ok && user_gone && holes == 0 && shadow == 0 && ro == 0 {
                println!("identhole: ALL CHECKS PASSED");
            } else {
                println!("identhole: FAILED");
            }
            crate::suite::park()
        }
        "sharedpd" => {
            // A MINE OF THE CLASS "tearing an AS down frees SOMEONE ELSE'S table". The kernel PDPT
            // is copied to a process in full, and the directories behind its GB1-3 entries are
            // shared. While a gigabyte is a 1G leaf, the walk has nothing to touch; but
            // `map_mmio_wc` (the LLQ window of ENA on EC2) splits the gigabyte into
            // a directory and a table, and the old walk considered them its own: the very first
            // process to finish gave THE KERNEL's directory to the free list, the frame
            // was reissued, and a write to the NVMe doorbell under any CR3 gave a #PF
            // in ring 0 (PostgreSQL on EC2, the startup process after 0.3 s).
            // The gate splits the shared gigabyte exactly the same way and requires that after
            // the process is torn down the directory and the table stay in place and are NOT among
            // the free frames.
            println!("talaria: sharedpd — the kernel's shared directory after a process's AS is torn down");
            const P: u64 = 0xA000_0000; // GB2: locally there is neither RAM (512 MB) nor devices there
            let l3 = (P >> 30) as usize;
            let before = crate::paging::kernel_pdpt_entry(l3);
            let split = crate::paging::map_mmio_wc(P, 4096);
            let e3 = crate::paging::kernel_pdpt_entry(l3);
            let pd = e3 & 0x000F_FFFF_FFFF_F000;
            let is_pd = split && e3 & 1 != 0 && e3 & (1 << 7) == 0;
            println!("sharedpd: the gigabyte {:#x} was split for a device window: {} (the entry {:#x} -> {:#x})",
                P & !0x3FFF_FFFF, if is_pd { "OK" } else { "FAILED" }, before, e3);
            let asp = crate::paging::new_process_space();
            // the process has the same entry as the kernel — the directory is shared
            let e3p = unsafe { core::ptr::read_volatile(crate::mem::kptr::<u64>(*crate::mem::kptr::<u64>(asp) & 0x000F_FFFF_FFFF_F000).add(l3)) };
            println!("sharedpd: the process's entry matches the kernel's: {}", if e3p == e3 { "OK" } else { "FAILED" });
            crate::paging::free_address_space(asp);
            let after = crate::paging::kernel_pdpt_entry(l3);
            let pd_free = crate::mem::frame_is_free(pd);
            let leaf = crate::paging::leaf_entry(P).unwrap_or(0);
            let intact = after == e3 && leaf & 1 != 0 && (leaf & 0x000F_FFFF_FFFF_F000) == P;
            println!("sharedpd: the kernel's directory after the AS teardown: {} (the entry {:#x}, the leaf {:#x}, in the free list: {})",
                if intact && !pd_free { "OK" } else { "FAILED" }, after, leaf, pd_free);
            if is_pd && e3p == e3 && intact && !pd_free { println!("sharedpd: ALL CHECKS PASSED"); }
            else { println!("sharedpd: FAILED"); }
            crate::suite::park()
        }
        "kstackguard" => {
            // THE GUARD OF A FORKED TASK'S KERNEL STACK. `fork` allocated the child's stack while
            // the PARENT's space was active, and the guard went into the parent's private
            // directory of the kernel's gigabyte — the child, the only one running on the
            // stack, had none. `alloc_kstack_in` lays it in the child's space and the kernel's
            // reference one (which later spaces copy); a refused fork gives the stack back with
            // the guard taken off (`release_kstack`).
            println!("talaria: kstackguard — the guard of a kernel stack allocated for another space");
            fn check(ok: &mut bool, what: &str, good: bool) {
                println!("kstackguard: {}: {}", what, if good { "OK" } else { "FAILED" });
                *ok &= good;
            }
            let mut ok = true;
            let k = crate::paging::kernel_cr3();
            let parent = crate::paging::new_process_space();
            let child = crate::paging::new_process_space();
            let top = crate::sched::alloc_kstack_in(child).expect("kstackguard: stack");
            let base = top - crate::sched::KSTACK_PAGES * 4096;
            check(&mut ok, "the guard is in the child's space", crate::paging::is_guard_in(child, base));
            check(&mut ok, "the guard is in the kernel's reference space", crate::paging::is_guard_in(k, base));
            check(&mut ok, "only the bottom page is a guard", !crate::paging::is_guard_in(child, base + 4096));
            let later = crate::paging::new_process_space();
            check(&mut ok, "a space built later inherits it", crate::paging::is_guard_in(later, base));
            check(&mut ok, "the space that was not named stays untouched", !crate::paging::is_guard_in(parent, base));
            crate::paging::free_address_space(parent);
            crate::paging::free_address_space(child);
            crate::paging::free_address_space(later);
            crate::sched::release_kstack(top);
            check(&mut ok, "a released stack leaves no hole in the kernel's space", !crate::paging::is_guard_in(k, base));
            println!("kstackguard: {}", if ok { "ALL OK" } else { "FAILED" });
            crate::suite::park()
        }
        "highmem" => {
            // THE MAIN NODE'S MEMORY ABOVE 4 GB (`mem::high_init`). The workload's frames come
            // from there first; the gate checks what that rests on: the frames are in the area
            // and identity-mapped, a returned block comes back from the area's free list (not
            // the low one), the refcount table reaches the area's top, a process AS inherits
            // the identity leaf and its teardown leaves the kernel's leaf alone, and a MAP_FIXED
            // over such a gigabyte is refused. Booted with 4 GB on purpose: the RAM crosses 4 GB.
            println!("talaria: highmem — the main node's memory above 4 GB");
            let (hb, he) = crate::mem::high_area();
            if hb == 0 { println!("highmem: there is no high area (the RAM ends below 4 GB)"); crate::suite::park() }
            fn check(ok: &mut bool, what: &str, good: bool) {
                println!("highmem: {}: {}", what, if good { "OK" } else { "FAILED" });
                *ok &= good;
            }
            let mut ok = true;
            let low0 = crate::mem::free_low_frames();
            let a = crate::mem::alloc_contig_node(16, 255).unwrap_or(0);
            check(&mut ok, "the workload's frames come from the high area", a >= hb && a + 16 * 4096 <= he);
            check(&mut ok, "they are identity-mapped", a != 0 && crate::paging::virt_to_phys(a) == a);
            let mut same = a != 0;
            if a != 0 {
                // SAFETY: P1 — sixteen frames straight from the allocator, nobody else holds them.
                unsafe {
                    for i in 0..16 * 512u64 { core::ptr::write_volatile(crate::mem::kptr::<u64>(a + i * 8), a ^ i); }
                    for i in 0..16 * 512u64 { same &= core::ptr::read_volatile(crate::mem::kptr::<u64>(a + i * 8)) == a ^ i; }
                }
            }
            check(&mut ok, "a pattern written at the identity address reads back", same);
            // A 2 MB page for the workload comes from the area too (it came from the low
            // memory only, and a c7g.large's 1.5 GB workload ate the kernel's page tables).
            let h = crate::mem::alloc_workload_aligned(512, 0x20_0000).unwrap_or(0);
            check(&mut ok, "a workload's 2 MB page comes from the high area, aligned",
                h >= hb && h + 0x20_0000 <= he && h & 0x1F_FFFF == 0);
            if h != 0 { crate::mem::free_contig(h, 512); }
            crate::mem::free_contig(a, 16);
            let fl0 = crate::mem::free_list_pages();
            let b = crate::mem::alloc_contig_node(16, 255).unwrap_or(0);
            check(&mut ok, "a returned block is handed out again from the area's free list",
                b >= hb && b + 16 * 4096 <= he && crate::mem::free_list_pages() + 16 == fl0);
            check(&mut ok, "the low allocator was not touched", crate::mem::free_low_frames() == low0);
            let top = he - 4096;
            let out0 = crate::mem::RC_OUTSIDE.load(core::sync::atomic::Ordering::Relaxed);
            crate::mem::rc_set(top, 3);
            let rc = crate::mem::rc_get(top);
            crate::mem::rc_set(top, 0);
            check(&mut ok, "the refcount table reaches the area's top",
                rc == 3 && crate::mem::RC_OUTSIDE.load(core::sync::atomic::Ordering::Relaxed) == out0);
            let asp = crate::paging::new_process_space();
            let l3 = (b >> 30) as usize;
            let e3 = crate::paging::kernel_pdpt_entry(l3);
            // The process's PDPT entry (`phys_of_in` sees private 4K pages only).
            let e3p = crate::paging::pdpt_entry_in(asp, l3);
            check(&mut ok, "a process AS inherits the identity leaf", e3p == e3 && e3 & 1 != 0);
            crate::paging::free_address_space(asp);
            check(&mut ok, "the AS teardown leaves the kernel's leaf", crate::paging::kernel_pdpt_entry(l3) == e3 && e3 & 1 != 0);
            check(&mut ok, "a MAP_FIXED over the area's gigabyte is refused, the heap and mmap regions are not",
                crate::paging::ident_leaf_overlap(b, 1) && !crate::paging::ident_leaf_overlap(crate::user::PROC_HEAP_BASE, 1)
                    && !crate::paging::ident_leaf_overlap(0x6000_0000_0000, 1));
            crate::mem::free_contig(b, 16);
            println!("highmem: the area {:#x}..{:#x} ({} MB)", hb, he, (he - hb) / 1048576);
            println!("highmem: {}", if ok { "ALL OK" } else { "FAILED" });
            crate::suite::park()
        }
        "vmspace" => {
            // Per-process address spaces: one vaddr -> different physical pages depending on CR3.
            println!("talaria: the per-process address space test (a CR3 switch)");
            let orig = crate::paging::current_cr3();
            let pa = crate::mem::alloc_frame().unwrap();
            let pb = crate::mem::alloc_frame().unwrap();
            let asa = crate::paging::new_address_space();
            let asb = crate::paging::new_address_space();
            const V: u64 = 0x1_0000_0000; // 4 GB — a private region (the kernel does not use it)
            crate::paging::map_in(asa, V, pa, true, false);
            crate::paging::map_in(asb, V, pb, true, false);
            let (a, b) = unsafe {
                crate::paging::switch(asa); core::ptr::write_volatile(crate::mem::vptr::<u64>(V), 0xAAAA);
                crate::paging::switch(asb); core::ptr::write_volatile(crate::mem::vptr::<u64>(V), 0xBBBB);
                crate::paging::switch(asa); let a = core::ptr::read_volatile(crate::mem::vptr::<u64>(V));
                crate::paging::switch(asb); let b = core::ptr::read_volatile(crate::mem::vptr::<u64>(V));
                crate::paging::switch(orig); // return to the original space
                (a, b)
            };
            println!("vmspace: AS-A@{:#x}={:#x} AS-B@{:#x}={:#x} phys A={:#x} B={:#x}", V, a, V, b, pa, pb);
            if a == 0xAAAA && b == 0xBBBB && pa != pb {
                println!("vmspace: ISOLATION CONFIRMED (one vaddr -> different physical pages depending on CR3)");
            } else {
                println!("vmspace: FAILED (a={:#x} b={:#x})", a, b);
            }
            crate::suite::park()
        }
        _ => return false,
    }
    true
}
