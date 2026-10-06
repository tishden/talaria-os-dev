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
                // Written through the kernel's pointer (the map, when it is on), read back through
                // it and through the identity: one frame, two addresses, the same word.
                let (rt, id) = unsafe {
                    let old = core::ptr::read_volatile(p);
                    core::ptr::write_volatile(p, 0xD1A9_0000_0000_0000 | pa);
                    let v = core::ptr::read_volatile(crate::mem::kptr::<u64>(back));
                    let w = core::ptr::read_volatile(crate::mem::vptr::<u64>(pa));
                    core::ptr::write_volatile(p, old);
                    (v, w)
                };
                let via_map = crate::mem::kva(pa) != pa;
                let ok = back == pa && crate::mem::kpa(crate::mem::kva(pa)) == pa && rt == (0xD1A9_0000_0000_0000 | pa)
                    && id == rt && via_map == crate::mem::DMAP_ON.load(core::sync::atomic::Ordering::Relaxed);
                println!("dmapid: {} pa={:#x} kva={:#x} back={:#x} identity-read {} {}", what, pa, crate::mem::kva(pa), back,
                    if id == rt { "same" } else { "DIFFERS" }, if ok { "OK" } else { "FAIL" });
                if !ok { bad += 1; }
            };
            match low { Some(f) => check("low frame", f), None => { println!("dmapid: no low frame"); } }
            match high { Some(f) => check("high frame", f), None => println!("dmapid: high frame: no high area (skipped)") }
            check(".bss word", bss);
            check("stack word", stk);
            let v = crate::mem::vaddr(core::ptr::addr_of!(BSS_WORD));
            if crate::mem::vaddr(crate::mem::vptr::<u64>(v)) != v { bad += 1; println!("dmapid: vptr/vaddr FAIL"); }
            if let Some(f) = low { crate::mem::free_contig(f, 1); }
            println!("dmapid: the direct map {} at {:#x}; kva of an address outside it: {} (the last {:#x})",
                if crate::mem::DMAP_ON.load(core::sync::atomic::Ordering::Relaxed) { "ON" } else { "OFF" }, crate::mem::DMAP_BASE,
                crate::mem::DMAP_MISS.load(core::sync::atomic::Ordering::Relaxed), crate::mem::DMAP_MISS_LAST.load(core::sync::atomic::Ordering::Relaxed));
            if low.is_some() && bad == 0 { println!("dmapid: ALL OK"); } else { println!("dmapid: FAIL ({} bad)", bad); }
            crate::suite::park()
        }
        "dmapstrict" => {
            // STAGE 4b OF THE DIRECT MAP (docs/ARM64-PLAN.md §10.16): under `talaria.dmap=strict` a
            // process space carries no identity leaf over the map's RAM (its fork child neither),
            // the kernel's own space still does, the kernel reaches a frame through the map under a
            // process space, and the frame's former identity address is the process's own: a page
            // mapped there is another frame, and the kernel's writes to the first do not show in it.
            let mut ok = true;
            let mut check = |what: &str, c: bool| { println!("dmapstrict: {} — {}", what, if c { "OK" } else { "FAIL" }); ok &= c; };
            check("strict is on", crate::paging::strict());
            let orig = crate::paging::current_cr3();
            let kern = crate::paging::strict_count_in(crate::paging::kernel_cr3());
            let asp = crate::paging::new_process_space();
            let n = crate::paging::strict_count_in(asp);
            println!("dmapstrict: identity 4K pages over the map's RAM: the kernel's space {}, a process space {}", kern, n);
            check("a process space has none, the kernel's space keeps them", n == 0 && kern > 0);
            let (kd, pd) = (crate::paging::strict_devgb_in(crate::paging::kernel_cr3()), crate::paging::strict_devgb_in(asp));
            println!("dmapstrict: identity gigabytes with no RAM (devices, holes): the kernel's space {}, a process space {}", kd, pd);
            check("a process space has no device gigabyte by its identity (the devices' window)", pd == 0);
            let child = crate::paging::fork_cow_space(asp);
            check("a fork child has none", crate::paging::strict_count_in(child) == 0);
            let (hb, _) = crate::mem::high_area();
            let low = crate::mem::alloc_frame();
            let high = if hb != 0 { crate::mem::alloc_high(1, 4096) } else { None };
            for (what, f) in [("a low frame", low), ("a high frame", high)] {
                let (Some(f), Some(g)) = (f, crate::mem::alloc_frame()) else { println!("dmapstrict: {}: none (skipped)", what); continue; };
                // the process's own page at f's identity address: frame g
                crate::paging::map_leaf_shared(asp, f, g, true, false, false);
                // SAFETY: f and g are frames this test owns; under `asp` the address f is the leaf
                // just mapped (g), and kptr goes through the map, which every space carries.
                let (seen, got) = unsafe {
                    core::ptr::write_volatile(crate::mem::kptr::<u64>(g), 0x0000_6A6A_0000_0000 | g);
                    crate::paging::switch(asp);
                    core::ptr::write_volatile(crate::mem::kptr::<u64>(f), 0x0000_F0F0_0000_0000 | f); // the kernel's write to f
                    let seen = core::ptr::read_volatile(crate::mem::vptr::<u64>(f)); // the process's page at f's identity address
                    let got = core::ptr::read_volatile(crate::mem::kptr::<u64>(f));
                    crate::paging::switch(orig);
                    (seen, got)
                };
                println!("dmapstrict: {} {:#x}: the kernel wrote {:#x} through the map, the process's page there reads {:#x}", what, f, got, seen);
                check(what, got == 0x0000_F0F0_0000_0000 | f && seen == 0x0000_6A6A_0000_0000 | g);
                crate::mem::free_contig(g, 1);
                crate::mem::free_contig(f, 1);
            }
            if let Some(h) = high {
                check("a MAP_FIXED over the high area's gigabyte is allowed", !crate::paging::ident_leaf_overlap(h, 1));
            }
            crate::paging::free_address_space(child);
            crate::paging::free_address_space(asp);
            println!("dmapstrict: {}", if ok { "ALL OK" } else { "FAILED" });
            crate::suite::park()
        }
        "mwin" => {
            // STAGE 5 OF THE DIRECT MAP (docs/ARM64-PLAN.md §10.16): the kernel reaches every device
            // register through the devices' window (slot 384), not through the identity. Every
            // device pointer the machine keeps is a window address whose physical address
            // round-trips; a process space has the window's slot (the same table as the kernel's)
            // and a device's ID word reads the same under it; no `mmio_va` fell back to the identity.
            let mut ok = true;
            let mut check = |what: &str, c: bool| { println!("mwin: {} — {}", what, if c { "OK" } else { "FAIL" }); ok &= c; };
            let on = crate::paging::MWIN_ON.load(core::sync::atomic::Ordering::Relaxed);
            let slot = ((crate::mem::MWIN_BASE >> 39) & 0x1FF) as usize;
            println!("mwin: the devices' window {} at {:#x}, misses {} (the last {:#x})", if on { "ON" } else { "OFF" },
                crate::mem::MWIN_BASE, crate::mem::MWIN_MISS.load(core::sync::atomic::Ordering::Relaxed), crate::mem::MWIN_MISS_LAST.load(core::sync::atomic::Ordering::Relaxed));
            if on {
                let mut ptrs = [("", 0u64); 5];
                for (i, p) in crate::arch::mwin_pointers().iter().enumerate() { ptrs[i] = *p; }
                ptrs[4] = ("ecam", crate::pci::ecam_va());
                for &(name, va) in ptrs.iter().filter(|p| p.1 != 0) {
                    let pa = crate::arch::mmio_pa(va);
                    println!("mwin: {} at {:#x} = physical {:#x}", name, va, pa);
                    check(name, crate::mem::in_mwin(va) && pa != va && crate::arch::mmio_va(pa) == va);
                }
                let w = crate::arch::mwin_witness();
                let asp = crate::paging::new_process_space();
                let same = crate::paging::top_entry_in(asp, slot) == crate::paging::top_entry_in(crate::paging::kernel_cr3(), slot);
                check("a process space shares the window's table", same);
                let orig = crate::paging::current_cr3();
                // SAFETY: the window's slot is in `asp` (checked above); the read is an ID register.
                let w2 = unsafe { crate::paging::switch(asp); let w2 = crate::arch::mwin_witness(); crate::paging::switch(orig); w2 };
                println!("mwin: a device's ID word {:#x} in the kernel's space, {:#x} in a process space", w, w2);
                check("a device reads the same in a process space", w != 0 && w == w2);
                crate::paging::free_address_space(asp);
                check("no device was reached by its identity", crate::mem::MWIN_MISS.load(core::sync::atomic::Ordering::Relaxed) == 0);
            }
            println!("mwin: {}", if ok && on { "ALL OK" } else if !on { "OFF" } else { "FAILED" });
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
            // like the workload's MAP_SHARED: present + U/S in its own copy of GB0 (under strict there
            // is no identity leaf to open: the frames are mapped at their address explicitly)
            if crate::paging::strict() {
                for p in 0..N { crate::paging::map_leaf_shared(asp, frame + p * 4096, frame + p * 4096, true, true, false); }
                crate::paging::flush_tlb_now();
            } else {
                crate::paging::protect(frame, N * 4096, true, false);
            }
            unsafe { core::ptr::write_volatile(crate::mem::kptr::<u64>(frame), 0x1234_5678) };
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
            // (under strict: no leaf at all — the identity there is gone from the process's space)
            let user_gone = crate::paging::leaf_entry(frame).map(|e| e & 4 == 0).unwrap_or(crate::paging::strict());
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
        "kstackpush" => {
            // A KERNEL STACK OVERFLOWED BY A PUSH (x86). The guard under a window stack faults; the
            // #PF then cannot push its own frame onto the same dead stack, and the double fault
            // that follows used to need one too — the machine triple-faulted with no word. The #DF
            // runs on IST1 now (`gdt::setup_gdt_cpu`) and the dump names the guard. The gate dies
            // by design: it expects the dump.
            #[cfg(target_arch = "x86_64")]
            {
                let top = crate::sched::alloc_kstack().expect("kstackpush: a stack");
                let base = top - crate::sched::KSTACK_PAGES * 4096;
                println!("kstackpush: a window stack {:#x}..{:#x} (window {}), its guard page {:#x}; pushing below its bottom",
                    base, top, crate::mem::in_kwin(base), base);
                // SAFETY: none intended — the stack is this test's, and the pushes run into its guard.
                unsafe { core::arch::asm!("mov rsp, {sp}", "2: push rax", "jmp 2b", sp = in(reg) base + 4096 + 64, options(noreturn)); }
            }
            #[cfg(target_arch = "aarch64")]
            {
                // arm64 pushes nothing itself, but the vector saves its frame on the same stack:
                // the overflowing store faults, and the entry's own save faults again
                let top = crate::sched::alloc_kstack().expect("kstackpush: a stack");
                let base = top - crate::sched::KSTACK_PAGES * 4096;
                println!("kstackpush: a window stack {:#x}..{:#x} (window {}), its guard page {:#x}; pushing below its bottom",
                    base, top, crate::mem::in_kwin(base), base);
                // SAFETY: none intended — the stack is this test's, and the stores run into its guard.
                unsafe { core::arch::asm!("mov sp, {sp}", "2: stp xzr, xzr, [sp, #-16]!", "b 2b", sp = in(reg) base + 4096 + 64, options(noreturn)); }
            }
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            { crate::suite::park() }
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
            if crate::paging::KWIN_ON.load(core::sync::atomic::Ordering::Relaxed) {
                // THE WINDOW (stage 3 of the direct map): one set of tables under slot 257 that
                // every space shares, the bottom page never mapped — a guard everywhere at once.
                let ws = ((crate::mem::KWIN_BASE >> 39) & 0x1FF) as usize;
                let kw = crate::paging::top_entry_in(k, ws);
                check(&mut ok, "the stack is in the window", crate::mem::in_kwin(base));
                check(&mut ok, "its bottom page is the guard (never mapped)", crate::paging::kwin_frame(base).is_none());
                check(&mut ok, "the other 15 pages are mapped", (1..crate::sched::KSTACK_PAGES).all(|p| crate::paging::kwin_frame(base + p * 4096).is_some()));
                let later = crate::paging::new_process_space();
                check(&mut ok, "the child, the parent and a later space share the window's tables",
                    kw & 1 != 0 && [child, parent, later].iter().all(|&a| crate::paging::top_entry_in(a, ws) == kw));
                // a live access through the kernel's recovering load: the guard faults, the stack does not
                check(&mut ok, "a read of the guard faults (recovered)", crate::obs::unwind::try_read(base, false).is_none());
                // SAFETY: the stack was just allocated and no task runs on it.
                let rw = unsafe {
                    let w = crate::mem::vptr::<u64>(top - 8);
                    core::ptr::write_volatile(w, 0x5741_434b);
                    core::ptr::read_volatile(w) == 0x5741_434b
                };
                check(&mut ok, "the top word is written and read back", rw);
                crate::paging::free_address_space(parent);
                crate::paging::free_address_space(child);
                crate::paging::free_address_space(later);
                let (_, built, _, _) = crate::sched::kwin_stats();
                crate::sched::release_kstack(top);
                let again = crate::sched::alloc_kstack().expect("kstackguard: again");
                let (given, built2, refused, held) = crate::sched::kwin_stats();
                check(&mut ok, "a released slot is taken again without new frames", built2 == built && again == top);
                crate::sched::release_kstack(again);
                println!("kstackguard: the window: given {} built {} refused {} held {}", given, built2, refused, held);
                println!("kstackguard: {}", if ok { "ALL OK" } else { "FAILED" });
                crate::suite::park()
            }
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
            // (under strict: a process space has none of the area's identity, `dmapstrict`)
            let st = crate::paging::strict();
            check(&mut ok, if st { "a process AS has no identity leaf over the area (strict)" } else { "a process AS inherits the identity leaf" },
                if st { e3p != e3 && e3 & 1 != 0 } else { e3p == e3 && e3 & 1 != 0 });
            crate::paging::free_address_space(asp);
            check(&mut ok, "the AS teardown leaves the kernel's leaf", crate::paging::kernel_pdpt_entry(l3) == e3 && e3 & 1 != 0);
            check(&mut ok, if st { "a MAP_FIXED over the area's gigabyte is allowed (strict), the heap and mmap regions too" }
                    else { "a MAP_FIXED over the area's gigabyte is refused, the heap and mmap regions are not" },
                crate::paging::ident_leaf_overlap(b, 1) != st && !crate::paging::ident_leaf_overlap(crate::user::PROC_HEAP_BASE, 1)
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
