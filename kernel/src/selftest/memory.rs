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
            unsafe { core::ptr::write_volatile(frame as *mut u64, 0x1234_5678) };
            crate::paging::unmap_proc(frame, N);            // the workload's munmap
            // THE KERNEL touches the same frames by their identity address while staying under the workload's AS
            let mut rw_ok = true;
            unsafe {
                for p in 0..N {
                    let a = frame + p * 4096;
                    core::ptr::write_volatile(a as *mut u64, 0xA5A5_0000 + p);
                    if core::ptr::read_volatile(a as *const u64) != 0xA5A5_0000 + p { rw_ok = false; }
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
            let e3p = unsafe { core::ptr::read_volatile(((*(asp as *const u64) & 0x000F_FFFF_FFFF_F000) as *const u64).add(l3)) };
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
                crate::paging::switch(asa); core::ptr::write_volatile(V as *mut u64, 0xAAAA);
                crate::paging::switch(asb); core::ptr::write_volatile(V as *mut u64, 0xBBBB);
                crate::paging::switch(asa); let a = core::ptr::read_volatile(V as *const u64);
                crate::paging::switch(asb); let b = core::ptr::read_volatile(V as *const u64);
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
