//! Running a static Linux binary in ring 3: GDT (kernel+user)+TSS, ELF loading,
//! the transition to user mode through iretq. The binary makes syscalls.
//!
//! # Safety
//!
//! This module writes a foreign file's bytes into memory a process will execute
//! and then leaves the kernel through `iretq`, so its `unsafe` rests on four
//! invariants. They are named L1..L4 at the sites rather than repeated there.
//!
//! - **L1. A LAUNCH path, not a syscall path.** Everything here runs either
//!   before the process exists (the boot/launch path: one core, no ring 3 yet) or
//!   from `execve`, which has already stopped the sister threads (`drain_proc`)
//!   and owns the task. Nobody else is looking at the address space being built,
//!   so the loaders may write into it with plain stores and change its page
//!   rights under no lock.
//! - **L2. The file's bytes are bounded by `elfhdr` before they are used.** An
//!   ELF comes from the disk, that is, from the workload's side: the header, the
//!   program headers (at most 32, into a fixed `[u8; 56 * 32]`) and the load span
//!   are decoded by `elfhdr`, which refuses a truncated or lying file instead of
//!   returning offsets that index past the buffer. Every raw write below derives
//!   its address and length from those decoded values.
//! - **L3. The pages written to are the kernel's own view of fresh frames.** A
//!   new process's segments, heap and stack come from `mem::alloc_contig` /
//!   `paging::map_leaf` into a `new_process_space`, and the kernel reaches them
//!   through the identity map (vaddr == phys) BEFORE the switch to the process's
//!   CR3. `paging::protect` gives them RW for the copy and the final W^X rights
//!   afterwards — without that first call a restart would write into the previous
//!   instance's read-only `.text` and take a #PF (CR0.WP).
//! - **L4. The `-> !` functions do not return.** They end in `iretq` into ring 3
//!   or in `suite::park()`; the kernel stack they were called on is abandoned on
//!   purpose, and the caller must have nothing left to do on it.
//!
//! `TSS`/`GDT` are `static mut` ARRAYS INDEXED BY CORE (`percpu::cpu_id()`): a
//! core touches only its own entry, which is what makes them race-free without a
//! lock (a TSS descriptor cannot be shared anyway — `ltr` sets its busy bit).
// The GDT/TSS and the ring-3 entry are the machine's: `arch::gdt` (docs/ARM64-PLAN.md §4.1).
pub use crate::arch::gdt::{set_rsp0, setup_gdt_cpu};
use crate::arch::gdt::{enter_user, switch_and_enter_user};
/// # Safety
/// As `setup_gdt_cpu`, with this core's number taken from `percpu`.
unsafe fn setup_gdt(kstack_top: u64) { setup_gdt_cpu(crate::percpu::cpu_id(), kstack_top); }

/// Harden the workload's stack: the bottom page is a guard (non-present, it catches an overflow
/// with a clear #PF), the rest is RW + NX (the stack is not executable, W^X). `bottom` is the bottom of the region.
fn harden_stack(bottom: u64, top: u64) {
    crate::paging::guard(bottom);
    crate::paging::protect(bottom + 4096, top - (bottom + 4096), true, false);
}

/// Set up GDT/TSS/syscall for user mode (without launching an ELF) — for threads.
///
/// # Safety
/// L1: it loads the GDT and the TR of the CURRENT core and arms the syscall MSRs,
/// so it must be called on the core the task will run on, while that core is not
/// in ring 3. `kstack_top` must be the top of a live kernel stack — the next
/// ring3 -> ring0 entry lands on it.
pub unsafe fn setup_thread_mode(kstack_top: u64) { setup_gdt(kstack_top); crate::syscall::init(); }

/// Load a large ELF STREAMING from the disk (TFS1 or ext2) straight into the segments'
/// vaddrs — without a buffer for the whole file (for binaries of tens of megabytes).
/// Returns the entry.
/// §3.3: reading through ext2 when that one is mounted (the process loader could do
/// it long ago, while the identity path read only TFS — on an ext2 image the workload simply
/// was not found).
/// # Safety
/// L1 + L2 + L3: `path` must be a NUL-terminated name of at most 256 bytes in the
/// KERNEL's memory (a literal or the manifest's buffer, never a pointer from ring
/// 3), and the segments are copied into the CURRENT address space by their own
/// vaddrs — so this is the identity-AS launch path, not a process's.
unsafe fn load_elf_from_disk(path: *const u8) -> (u64, u64, u64, u64) {
    let ext = crate::ext2::active();
    // SAFETY: L1 — `path` is the kernel's own NUL-terminated name; 256 is the bound the
    // callers' buffers are sized for, and the slice is cut at the NUL right away.
    let cpath = { let s = core::slice::from_raw_parts(path, 256); &s[..s.iter().position(|&c| c == 0).unwrap_or(255)] };
    let fd = if ext { crate::ext2::open(cpath, 0, 0) } else { crate::fs::open(path) };
    if fd < 0 { crate::println!("talaria: cannot open the binary on the disk"); return (0, 0, 0, 0); }
    let readat = |off: u64, dst: *mut u8, len: usize| -> usize {
        if ext { let r = crate::ext2::pread(fd, dst, len, off); if r < 0 { 0 } else { r as usize } }
        else { crate::fs::lseek(fd, off as i64, 0); let r = crate::fs::read(fd, dst, len); if r < 0 { 0 } else { r as usize } }
    };
    let mut hdr = [0u8; 64];
    readat(0, hdr.as_mut_ptr(), 64);
    let Some(eh) = crate::elfhdr::header(&hdr) else {
        crate::println!("talaria: the binary on the disk is not an ELF64 for x86-64");
        if ext { crate::ext2::close(fd); } else { crate::fs::close(fd); }
        return (0, 0, 0, 0);
    };
    let (entry, phoff, phentsize, phnum) = (eh.entry, eh.phoff as i64, eh.phentsize, eh.phnum);
    let mut ph = [0u8; 56 * 32]; // up to 32 phdrs
    readat(phoff as u64, ph.as_mut_ptr(), (phnum * phentsize).min(56 * 32));
    let mut base_vaddr = u64::MAX; // the vaddr of the segment with file offset 0 (the ELF header+phdr are there)
    // Reserve the image's identity region from the frame allocator (otherwise mmap/heaps
    // could climb onto the workload's code/data on a VM with a lot of memory).
    if let Some((lo, hi)) = crate::elfhdr::load_span(&ph, phentsize, phnum) {
        crate::mem::reserve(lo, hi - lo);
    }
    for p in crate::elfhdr::phdrs(&ph, phentsize, phnum).filter(|p| p.is_load()) {
        let off = p.off as i64;
        let vaddr = p.vaddr;
        let filesz = p.filesz as usize;
        let memsz = p.memsz as usize;
        if off == 0 { base_vaddr = vaddr; }
        // The segment's pages may have stayed R-X/RO from the PREVIOUS instance (a supervise
        // restart overwrites the image; W^X + CR0.WP: a write from the kernel into RO = #PF) —
        // return RW before copying, the final rights are set by protect below.
        crate::paging::protect(vaddr, memsz as u64, true, false);
        let mut done = 0usize;
        while done < filesz {
            let n = (filesz - done).min(1 << 20);
            let got = readat(off as u64 + done as u64, (vaddr as *mut u8).add(done), n);
            if got == 0 { break; }
            done += got;
        }
        if memsz > filesz { core::ptr::write_bytes((vaddr as *mut u8).add(filesz), 0, memsz - filesz); }
        // W^X per segment: p_flags (PF_X=1, PF_W=2) -> the rights of the segment's pages.
        let (w, x) = p.rights();
        if x { crate::arch::icache::sync(vaddr, memsz as u64); }   // the code is in the data cache; fetch must see it
        crate::paging::protect(vaddr, memsz as u64, w, x);
    }
    if ext { crate::ext2::close(fd); } else { crate::fs::close(fd); }
    let phdr_va = if base_vaddr != u64::MAX { base_vaddr + phoff as u64 } else { 0 };
    (entry, phdr_va, phentsize as u64, phnum as u64) // for AT_PHDR/AT_PHENT/AT_PHNUM (TLS!)
}

/// Build the initial SysV stack with ARBITRARY arguments (argc/argv/envp/auxv).
///
/// # Safety
/// L1 + L3: `top` must be the top of a stack region ALREADY mapped RW, with room
/// for the vectors below it (the callers map `sframes` pages first), and `args`
/// must outlive the copy. The `build_stack_*` family shares this contract.
unsafe fn build_stack_args(top: u64, entry: u64, phdr: u64, phent: u64, phnum: u64, args: &[&[u8]]) -> u64 {
    // The workload will run in THE CURRENT space — so we put the vDSO into it too.
    build_stack_args_env(top, entry, phdr, phent, phnum, args, &[], 0, vdso_here())
}

/// Like build_stack_args, but the write goes to the address (vaddr + woff), while the stored
/// pointers are vaddr. It allows building the stack of A FOREIGN AS through its phys (woff =
/// phys_base - vaddr_bottom) before switching CR3. woff=0 is the ordinary identity case.
///
/// # Safety
/// As `build_stack_args`, and `woff` must be exactly `phys_base - vaddr_bottom` of
/// the target stack: every write goes to `vaddr + woff`, so a wrong offset writes
/// into somebody else's frames.
unsafe fn build_stack_args_off(top: u64, entry: u64, phdr: u64, phent: u64, phnum: u64, args: &[&[u8]], woff: u64, vdso: u64) -> u64 {
    build_stack_args_env(top, entry, phdr, phent, phnum, args, &[], woff, vdso)
}

/// The full variant: argv (up to 48) + envp (up to 16 strings "K=V") + auxv.
///
/// # Safety
/// As `build_stack_args_off`.
unsafe fn build_stack_args_env(top: u64, entry: u64, phdr: u64, phent: u64, phnum: u64, args: &[&[u8]], envs: &[&[u8]], woff: u64, vdso: u64) -> u64 {
    build_stack_full(top, entry, phdr, phent, phnum, args, envs, woff, 0, entry, b"", vdso)
}
/// The full auxv (for the dynamic loader): AT_BASE (the base of ld.so), AT_ENTRY of the main image,
/// AT_PLATFORM/AT_EXECFN/AT_HWCAP/AT_CLKTCK/AT_UID.., AT_SECURE=0.
///
/// # Safety
/// As `build_stack_args_off`. Every store below is `wr(p)`, that is, inside the
/// mapped stack: `p` only decreases from `top` and the vectors are bounded (argv
/// 48, envp 16), so the writes stay within the region the caller mapped.
unsafe fn build_stack_full(top: u64, entry: u64, phdr: u64, phent: u64, phnum: u64, args: &[&[u8]], envs: &[&[u8]], woff: u64, at_base: u64, at_entry: u64,
    execfn: &[u8], vdso: u64) -> u64 {
    let wr = |v: u64| v.wrapping_add(woff); // vaddr -> the write address
    let mut p = top;
    p -= 16;
    for i in 0..16 { let r = crate::tsc::rdrand64().unwrap_or(crate::tsc::rdtsc()); *(wr(p) as *mut u8).add(i) = (r >> ((i % 8) * 8)) as u8; }
    let random = p;
    // the strings for auxv: AT_PLATFORM, AT_EXECFN
    p -= 8; core::ptr::copy_nonoverlapping(crate::arch::PLATFORM.as_ptr(), wr(p) as *mut u8, 8); let platform = p;
    let execfn_n = execfn.iter().position(|&c| c == 0).unwrap_or(execfn.len());
    p -= (execfn_n + 1) as u64; core::ptr::copy_nonoverlapping(execfn.as_ptr(), wr(p) as *mut u8, execfn_n); *(wr(p) as *mut u8).add(execfn_n) = 0;
        let execfn_p = p;
    let _ = entry;
    let mut argp = [0u64; 48];
    let n = args.len().min(48);
    for k in 0..n {
        let a = args[k];
        p -= (a.len() + 1) as u64;
        core::ptr::copy_nonoverlapping(a.as_ptr(), wr(p) as *mut u8, a.len());
        *(wr(p) as *mut u8).add(a.len()) = 0;
        argp[k] = p;
    }
    let mut envp = [0u64; 16];
    let ne = envs.len().min(16);
    for k in 0..ne {
        let a = envs[k];
        p -= (a.len() + 1) as u64;
        core::ptr::copy_nonoverlapping(a.as_ptr(), wr(p) as *mut u8, a.len());
        *(wr(p) as *mut u8).add(a.len()) = 0;
        envp[k] = p;
    }
    // argc, argv[0..n], NULL, envp[0..ne], NULL, auxv(PHDR,PHENT,PHNUM,PAGESZ,ENTRY,RANDOM,NULL)
    let mut items: [u64; 128] = [0; 128];
    let mut m = 0;
    items[m] = n as u64; m += 1;
    for k in 0..n { items[m] = argp[k]; m += 1; }
    items[m] = 0; m += 1;             // argv NULL
    for k in 0..ne { items[m] = envp[k]; m += 1; }
    items[m] = 0; m += 1;             // envp NULL
    items[m] = 3; m += 1; items[m] = phdr; m += 1;   // AT_PHDR  (for TLS!)
    items[m] = 4; m += 1; items[m] = phent; m += 1;  // AT_PHENT
    items[m] = 5; m += 1; items[m] = phnum; m += 1;  // AT_PHNUM
    items[m] = 6; m += 1; items[m] = 4096; m += 1;   // AT_PAGESZ
    items[m] = 9; m += 1; items[m] = at_entry; m += 1;  // AT_ENTRY (the main image, not ld.so)
    items[m] = 25; m += 1; items[m] = random; m += 1;// AT_RANDOM
    items[m] = 7; m += 1; items[m] = at_base; m += 1;  // AT_BASE (0 = static)
    items[m] = 8; m += 1; items[m] = 0; m += 1;        // AT_FLAGS
    items[m] = 11; m += 1; items[m] = 1000; m += 1;    // AT_UID
    items[m] = 12; m += 1; items[m] = 1000; m += 1;    // AT_EUID
    items[m] = 13; m += 1; items[m] = 1000; m += 1;    // AT_GID
    items[m] = 14; m += 1; items[m] = 1000; m += 1;    // AT_EGID
    items[m] = 23; m += 1; items[m] = 0; m += 1;       // AT_SECURE
    items[m] = 17; m += 1; items[m] = 100; m += 1;     // AT_CLKTCK
    items[m] = 15; m += 1; items[m] = platform; m += 1; // AT_PLATFORM
    items[m] = 31; m += 1; items[m] = execfn_p; m += 1; // AT_EXECFN
    items[m] = 16; m += 1; items[m] = crate::arch::AT_HWCAP; m += 1; // AT_HWCAP: the machine's word (cpuid.1:edx fpu..sse2 / the arm64 HWCAP bits)
    // AT_SYSINFO_EHDR: by it libc finds the vDSO and takes clock_gettime from it.
    // Without the auxv entry libc will not even look — it will simply go by syscall.
    //
    // WE DECLARE EXACTLY WHAT IS MAPPED. `vdso::VADDR` stood here without any
    // mapping at all, and it was the same mine that had already been found in three other
    // paths of creating an AS: the vector is given to the workload, there is no page at that address, and
    // libc's very first access to the vDSO table gives a #PF on a read at 0x7ffd…,
    // after which the workload is killed by signal 11. It was caught as "lua and quickjs do not
    // work" (`kernel/validate.sh`, B6) — that is, it looked like the open-source software was
    // broken rather than like a missing mapping: right after the entry into ring 3,
    // at `rdi=0x7ffd_0000_0000`, on a read at offset 0x20 (the header of the vDSO's ELF).
    //
    // The address comes FROM THE CALLER, because only it knows in which
    // space the workload will run. Mapping "into the current one" from here is not allowed:
    // when a process with its own AS is being assembled, the current one is THE KERNEL's space, while
    // 0x7ffd… and the workload's stack 0x7fffe… lie under ONE PML4 entry, and
    // a mapping into a foreign AS breaks the stack of a process that has not started yet (the gates
    // `sigtest` and `ext2v` failed exactly like that: a #PF on a write below rsp).
    // A zero means "there is no vDSO" — then we do not write the vector at all and libc goes by syscall.
    if vdso != 0 { items[m] = 33; m += 1; items[m] = vdso; m += 1; }
    items[m] = 0; m += 1; items[m] = 0; m += 1;      // AT_NULL
    let bytes = (m * 8) as u64;
    let sp = (p - bytes) & !0xF;
    for i in 0..m { core::ptr::write((wr(sp) as *mut u64).add(i), items[i]); }
    sp
}


/// Map the vDSO into the ACTIVE address space and return its address (0 — there is none).
/// The simple modes (`run`, `run_disk`) execute in the kernel's identity AS, they have no separate
/// pml4 — the mapping is put straight into the current CR3.
fn vdso_here() -> u64 {
    let cr3 = crate::paging::current_cr3() & !0xFFF;
    crate::vdso::map_into(cr3)
}

/// The result of loading an ELF image into a process's AS (for auxv and the dynamic loader).
pub struct ElfImage { pub entry: u64, pub phdr: u64, pub phent: u64, pub phnum: u64, pub base: u64, pub interp: [u8; 128], pub interp_len: usize }
const PIE_BASE: u64 = 0x5555_5555_0000;       // the base for an ET_DYN main image (PIE), as in Linux
const INTERP_BASE: u64 = 0x7f00_0000_0000;    // the base of the dynamic loader (ld-musl / ld-linux)

/// Load an ELF STREAMING from the disk (ext2/TFS) into a NEW address space of a process:
/// the segments go into fresh frames (identity access from the kernel context), map_leaf into the pml4
/// (PRIV -> fork COW). ET_EXEC goes by its own vaddrs; ET_DYN (PIE, ld.so) with the offset `dyn_base`.
/// Reads PT_INTERP (the loader's path). None — an error (not found/not an ELF/no memory).
///
/// # Safety
/// L1 + L2 + L3: `pml4` must be a page table of a process being built that nobody
/// runs yet (`paging::new_process_space`), and the copies go through the identity
/// view of its fresh frames before the switch to that CR3.
unsafe fn load_elf_into_space_disk(pml4: u64, path: &[u8], dyn_base: u64) -> Option<ElfImage> {
    let ext = crate::ext2::active();
    let h = if ext { crate::ext2::open(path, 0, 0) } else { crate::fs::open(path.as_ptr()) };
    if h < 0 { crate::println!("talaria: proc: the binary was not found on the disk"); return None; }
    let readat = |off: u64, dst: *mut u8, len: usize| -> usize {
        if ext { let r = crate::ext2::pread(h, dst, len, off); if r < 0 { 0 } else { r as usize } }
        else { crate::fs::lseek(h, off as i64, 0); let r = crate::fs::read(h, dst, len); if r < 0 { 0 } else { r as usize } }
    };
    let mut hdr = [0u8; 64];
    if readat(0, hdr.as_mut_ptr(), 64) != 64 { crate::println!("talaria: proc: not an ELF"); return None; }
    let Some(eh) = crate::elfhdr::header(&hdr) else { crate::println!("talaria: proc: not an ELF"); return None; };
    let base = if eh.is_pie() { dyn_base } else { 0 }; // ET_DYN -> the offset
    let entry = eh.entry + base;
    let (phoff, phentsize, phnum) = (eh.phoff, eh.phentsize, eh.phnum);
    let mut ph = [0u8; 56 * 32];
    readat(phoff, ph.as_mut_ptr(), (phnum * phentsize).min(56 * 32));
    let mut base_vaddr = u64::MAX;
    let mut img = ElfImage { entry, phdr: 0, phent: phentsize as u64, phnum: phnum as u64, base, interp: [0; 128], interp_len: 0 };
    // Reserve the image's identity range from the frame allocator: mmap in process mode
    // hands out identity addresses (vaddr==phys), and a frame with an address inside the image would overlap
    // the binary's PRIV pages in the process's AS (protect() would override the rights of .text -> an NX fault).
    let (mut lo, mut hi) = (u64::MAX, 0u64);
    for p in crate::elfhdr::phdrs(&ph, phentsize, phnum.min(32)) {
        if p.ptype == crate::elfhdr::PT_INTERP {
            let fsz = (p.filesz as usize).min(127);
            readat(p.off, img.interp.as_mut_ptr(), fsz);
            img.interp_len = img.interp[..fsz].iter().position(|&c| c == 0).unwrap_or(fsz);
            continue;
        }
        if !p.is_load() { continue; }
        let vaddr = p.vaddr + base;
        let memsz = p.memsz;
        lo = lo.min(vaddr & !0xFFF); hi = hi.max((vaddr + memsz + 0xFFF) & !0xFFF);
    }
    if hi > lo && lo < 0x1_0000_0000 { crate::mem::reserve(lo, hi - lo); }
    for p in crate::elfhdr::phdrs(&ph, phentsize, phnum.min(32)).filter(|p| p.is_load()) {
        let (writable, exec_) = p.rights();
        let off = p.off;
        let vaddr = p.vaddr + base;
        let filesz = p.filesz as usize;
        let memsz = p.memsz as usize;
        if off == 0 { base_vaddr = vaddr; }
        let start = vaddr & !0xFFF;
        let end = (vaddr + memsz as u64 + 0xFFF) & !0xFFF;
        let npages = (end - start) / 4096;
        // THE FAST PATH for large segments: a 2 MB piece goes into CONTIGUOUS
        // frames and is read by ONE request to the disk (ext2::pread_direct past the cache).
        // Page-by-page reading cost 134 000 requests of 4 KB on ClickHouse's 525 MB
        // image — eleven seconds for the loading alone.
        const CHUNK: u64 = 512; // 2 MB
        let mut q = 0u64;
        if ext {
            while q + CHUNK <= npages {
                let va = start + q * 4096;
                // we take a piece only where nothing is mapped yet
                let mut free = true;
                for k in 0..CHUNK { if crate::paging::phys_of_in(pml4, va + k * 4096).is_some() { free = false; break; } }
                if !free { break; }
                let fr = match crate::mem::alloc_contig(CHUNK) { Some(f) => f, None => break };
                if crate::paging::virt_to_phys(fr) != fr && fr < 0x1_0000_0000 { crate::mem::free_contig(fr, CHUNK); break; }
                core::ptr::write_bytes(fr as *mut u8, 0, (CHUNK * 4096) as usize);
                // the part of the piece covered by the file (the segment's tail is .bss, it stays zeroes)
                let chunk_lo = va.max(vaddr);
                let chunk_hi = (va + CHUNK * 4096).min(vaddr + filesz as u64);
                if chunk_hi > chunk_lo {
                    let foff = off + (chunk_lo - vaddr);
                    let dst = (fr + (chunk_lo - va)) as *mut u8;
                    let n = (chunk_hi - chunk_lo) as usize;
                    // a direct read is possible when both the file offset and the length are multiples of a block
                    let direct = foff % 4096 == 0 && n.is_multiple_of(4096)
                        && crate::ext2::pread_direct(h, dst, n, foff) == n as i64;
                    if !direct {
                        let mut done = 0usize;
                        while done < n { let got = readat(foff + done as u64, dst.add(done), n - done); if got == 0 { break; } done += got; }
                    }
                }
                if exec_ { crate::arch::icache::sync(fr, CHUNK * 4096); }
                for k in 0..CHUNK { crate::paging::map_leaf(pml4, va + k * 4096, fr + k * 4096, writable, true, exec_); }
                q += CHUNK;
            }
        }
        // Segments may share a page (PIE/ld.so without -z separate-code): if the page is already
        // mapped by the previous segment, we append into THE SAME frame and OR the rights.
        while q < npages {
            let va = start + q * 4096;
            let existing = crate::paging::phys_of_in(pml4, va);
            let (writable, exec) = (writable, exec_);
            let frame = match existing { Some(ph) => ph, None => {
                let fr = match crate::mem::alloc_frame() { Some(f) => f, None => { crate::println!("talaria: proc: no memory for a segment"); return None; } };
                core::ptr::write_bytes(fr as *mut u8, 0, 4096);
                crate::paging::map_leaf(pml4, va, fr, writable, true, exec);
                fr } };
            if existing.is_some() { crate::paging::or_perms_in(pml4, va, writable, exec); }
            // the portion of the file falling into this page
            let seg_lo = vaddr.max(va); let seg_hi = (vaddr + filesz as u64).min(va + 4096);
            if seg_hi > seg_lo {
                let foff = off + (seg_lo - vaddr);
                let dst = (frame + (seg_lo - va)) as *mut u8;
                let mut done = 0usize; let want = (seg_hi - seg_lo) as usize;
                while done < want { let got = readat(foff + done as u64, dst.add(done), want - done); if got == 0 { break; } done += got; }
            }
            // The page was mapped before it was filled (a segment may share it with the
            // previous one): the instruction cache is told AFTER the bytes are in.
            if exec { crate::arch::icache::sync(frame, 4096); }
            q += 1;
        }
    }
    if ext { crate::ext2::close(h); } else { crate::fs::close(h); }
    img.phdr = if base_vaddr != u64::MAX { base_vaddr + phoff } else { 0 };
    Some(img)
}

const BIG_STACK_FRAMES: u64 = 2048;      // an 8 MB process stack (PostgreSQL: max_stack_depth 2 MB + headroom)
const BIG_HEAP_FRAMES: u64 = 16384;      // a 64 MB brk heap of the process (the rest is mmap)

/// THE SIZE OF THE WORKLOAD'S MAIN STACK, BYTES — the one `getrlimit(RLIMIT_STACK)` must answer.
///
/// It is not one number: `mode=proc` gets 8 MB (`BIG_STACK_FRAMES`) and `mode=cell` 32 MB
/// (`DT_STACK_FRAMES`), while the embedded demo modes get kilobytes. Reporting a constant
/// meant reporting 32 MB to PostgreSQL, whose `max_stack_depth` is checked against exactly
/// this limit — that is, offering it four times the room it has. So the number is RECORDED
/// where the stack is actually mapped and read back from there.
pub static USTACK_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The limit to report: what was really mapped, and the cell's size while nothing has
/// been mapped yet (the kernel's own threads ask before a workload exists).
pub fn stack_limit() -> u64 {
    match USTACK_BYTES.load(core::sync::atomic::Ordering::Relaxed) {
        0 => DT_STACK_FRAMES * 4096,
        n => n,
    }
}
#[inline]
fn note_stack(frames: u64) { USTACK_BYTES.store(frames * 4096, core::sync::atomic::Ordering::Relaxed); }

/// Run a PROCESS from the disk (a large binary, argv+envp, auxv) in its own AS under the scheduler:
/// fork/COW, signals, fd tables (proc.rs). The path for fork workloads (the PostgreSQL postmaster).
///
/// # Safety
/// L1 + L4: the launch path — it is called once, from the boot core, when no ring 3
/// exists yet, and it does not return (it goes into the scheduler and then into
/// ring 3). `path`/`args`/`envs` are the kernel's own buffers (the manifest).
pub unsafe fn run_process_disk(path: &[u8], args: &[&[u8]], envs: &[&[u8]]) -> ! {
    use core::sync::atomic::Ordering;
    let kern_cr3 = crate::paging::current_cr3();
    let node = crate::affinity::pick_launch_node(); // the node before the first large allocation
    let kstack0 = crate::sched::alloc_kstack().unwrap();
    setup_gdt(kstack0);
    crate::syscall::init();
    crate::syscall::THREADED.store(true, Ordering::SeqCst);
    let pml4 = crate::paging::new_process_space();
    let img = match load_elf_into_space_disk(pml4, path, PIE_BASE) { Some(i) => i, None => crate::suite::park() };
    let (mut entry, phdr, phent, phnum) = (img.entry, img.phdr, img.phent, img.phnum);
    let mut at_base = 0u64;
    if img.interp_len > 0 { // a dynamic ELF: we load the interpreter (ld-musl/ld-linux), the entry is into it
        crate::println!("talaria: a dynamic ELF: the interpreter {} @ {:#x}", core::str::from_utf8(&img.interp[..img.interp_len]).unwrap_or("?"), INTERP_BASE);
        match load_elf_into_space_disk(pml4, &img.interp[..img.interp_len], INTERP_BASE) {
            Some(li) => { entry = li.entry; at_base = li.base; }
            None => { crate::println!("talaria: proc: the interpreter was not loaded"); crate::suite::park() }
        }
    }
    let vdso = crate::vdso::map_into(pml4); // the clock in user space (AT_SYSINFO_EHDR below)
    let (ustack_top, woff) = map_proc_stack(pml4, BIG_STACK_FRAMES);
    // the process's private heap (64 MB) — from the workload's node
    let hb = crate::mem::alloc_contig_node(BIG_HEAP_FRAMES, node).expect("proc: heap");
    core::ptr::write_bytes(hb as *mut u8, 0, (BIG_HEAP_FRAMES * 4096) as usize);
    for p in 0..BIG_HEAP_FRAMES { crate::paging::map_leaf(pml4, PROC_HEAP_BASE + p * 4096, hb + p * 4096, true, true, false); }
    let ustack = build_stack_full(ustack_top, entry, phdr, phent, phnum, args, envs, woff, at_base, img.entry, path, vdso);
    let tid = crate::sched::spawn_process(entry, ustack, kstack0, pml4);
    crate::sched::set_kstack_pages(tid, crate::sched::KSTACK_PAGES);
    crate::sched::set_heap(tid, PROC_HEAP_BASE, PROC_HEAP_BASE + BIG_HEAP_FRAMES * 4096);
    crate::proc::init_proc(tid);
    crate::fr::record(crate::fr::USER, entry);
    let _ = kern_cr3;
    crate::println!("talaria: a process from the disk in its own AS (fork/COW, signals, fd tables), entry={:#x}, argc={}, envc={}", entry, args.len(),
        envs.len());
    crate::smp::lapic_enable();
    crate::smp::lapic_timer_init(crate::smp::SLICE_NS);
    crate::sched::install();
    if crate::affinity::place_main(tid, basename(path)) { crate::sched::start_idle(); }
    // Without sti: IF=1 for the task will be restored by iretq (the spawn_user frame, RFLAGS 0x202).
    // sti here opened a race window: a pending interrupt (after a UEFI boot it is
    // already waiting in the IRR) hit before start(), rust_schedule saved THE BOOT
    // context into RSP[0] over the user frame — an eternal entry into itself (see sched::start).
    crate::sched::start();
}
static mut RD_STACK: u64 = 0;  // the workload's stack for run_disk (reused between supervise restarts)
static mut RD_KSTACK: u64 = 0; // the kernel stack for run_disk (likewise)
/// Run a large binary FROM THE DISK with arguments in ring 3 (the kernel-AS modes: redis/busybox/sqlite/lua).
/// # Safety
/// L1 + L4, and `path` is the kernel's NUL-terminated name (see
/// `load_elf_from_disk`): a single-threaded workload in the identity AS.
pub unsafe fn run_disk(path: *const u8, args: &[&[u8]]) -> ! {
    if RD_KSTACK == 0 { RD_KSTACK = crate::sched::alloc_kstack().unwrap(); }
    let kstack = RD_KSTACK;
    let stack_frames: u64 = 8192; // a 32 MB stack (large C++ software keeps big objects on the stack)
    note_stack(stack_frames);
    if RD_STACK == 0 { RD_STACK = crate::mem::alloc_contig(stack_frames).unwrap(); }
    let ub = RD_STACK;
    crate::paging::unguard(ub, stack_frames * 4096); // after a restart: return present (harden_stack set a guard)
    let ustack_top = ub + stack_frames * 4096;
    setup_gdt(kstack);
    crate::syscall::init();
    let (entry, phdr, phent, phnum) = load_elf_from_disk(path);
    if entry == 0 { crate::suite::park() }
    let ustack = build_stack_args(ustack_top, entry, phdr, phent, phnum, args);
    harden_stack(ub, ustack_top); // a guard page at the bottom + NX on the stack
    crate::fr::record(crate::fr::USER, entry);
    crate::println!("talaria: launching a binary from the disk in ring 3, entry={:#x}", entry);
    enter_user(entry, ustack, crate::arch::switch::USER_FLAGS_IRQ_OFF);
}

static mut DT_STACK: u64 = 0;  // the stack of the disk workload's main thread (reused between restarts)
const DT_STACK_FRAMES: u64 = 8192; // 32 MB
static mut DT_PML4: u64 = 0;   // the AS of the workload's previous launch (freed at a restart)
/// talaria.cell=identity — return the old behaviour (the workload in the kernel's identity AS).
pub static CELL_IDENTITY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Run a large binary FROM THE DISK under the preemptive scheduler (clone/futex threads),
/// with argv AND envp — the path for multi-threaded production software (mode=cell).
///
/// §8.6: the workload lives in ITS OWN address space (as in mode=proc) rather than in
/// the kernel's identity space. What that gives: the workload's segments/stack/heap are in
/// private pages (an error in it does not reach the kernel by an address that coincided with
/// a physical one), fork/COW works, and a file MAP_SHARED (§8.5) is mapped at a
/// virtual address rather than "wherever the cache landed". clone threads inherit cr3 (syscall.rs).
/// The old behaviour is talaria.cell=identity.
/// # Safety
/// L1 + L4, `path` as in `run_disk`: the multi-threaded cell launch.
pub unsafe fn run_disk_threaded(path: *const u8, args: &[&[u8]], envs: &[&[u8]]) -> ! {
    use core::sync::atomic::Ordering;
    if CELL_IDENTITY.load(Ordering::Relaxed) { run_disk_threaded_identity(path, args, envs); }
    let kmain = crate::sched::alloc_kstack().unwrap();
    setup_gdt(kmain);
    crate::syscall::init();
    crate::syscall::THREADED.store(true, Ordering::SeqCst);
    // a restart by the supervisor: the previous space is no longer needed
    if DT_PML4 != 0 {
        // A restart by the supervisor: return the frames of the previous space. Freeing
        // an AS is allowed only while NOT being under it — after the workload crashes CR3 still
        // points at its tables, and tearing the page tables down from under oneself hung
        // the kernel solid (it was caught on the third restart of memhog).
        crate::paging::switch(crate::paging::kernel_cr3());
        let stuck = crate::sched::aps_to_kernel_cr3(1000);
        if stuck > 0 { crate::println!("cell: {} AP(s) did not leave the tables of the previous instance within 1 s", stuck); }
        crate::paging::free_address_space(DT_PML4);
        DT_PML4 = 0;
    }
    // A11: marks of the steps of the workload's bring-up. They are cheap (a line per launch) and
    // irreplaceable on a machine without a console: in a run on c6i.metal the log broke off
    // between "launching the workload" and "the workload is in ring 3", and there are five steps between them,
    // every one of which could have been the place where it stopped.
    // The NUMA node is chosen BEFORE the first large allocation: the process's stack and heap
    // must land where its threads will live afterwards (see affinity.rs).
    let node = crate::affinity::pick_launch_node();
    crate::println!("cell: building my own address space");
    let pml4 = crate::paging::new_process_space();
    DT_PML4 = pml4;
    // THE CLOCK IN USER SPACE goes here too. A multi-threaded workload (mode=cell) received
    // an address space WITHOUT a vDSO, although the AT_SYSINFO_EHDR vector was handed to it
    // all the same: the very first call through the vDSO table is a #PF on a read at
    // 0x7ffd…, the workload is killed by signal 11. That is how the task challenge's harness stayed
    // silent: the sender crashed at the start, the producer dutifully sent into the ring, while the receiver
    // saw zero messages — and it looked like "the transport does not deliver".
    crate::vdso::map_into(pml4);
    crate::println!("cell: the space pml4={:#x} — reading the ELF from the disk", pml4);
    let plen = { let mut n = 0; while *path.add(n) != 0 { n += 1; } n };
    let pslice = core::slice::from_raw_parts(path, plen);
    let img = match load_elf_into_space_disk(pml4, pslice, PIE_BASE) {
        Some(i) => i,
        None => { crate::println!("talaria: the binary was not found on the disk"); crate::suite::park() }
    };
    let (mut entry, phdr, phent, phnum) = (img.entry, img.phdr, img.phent, img.phnum);
    let mut at_base = 0u64;
    if img.interp_len > 0 { // a dynamic ELF (ld-musl): the entry is into the interpreter
        crate::println!("talaria: a dynamic ELF: the interpreter {} @ {:#x}",
            core::str::from_utf8(&img.interp[..img.interp_len]).unwrap_or("?"), INTERP_BASE);
        match load_elf_into_space_disk(pml4, &img.interp[..img.interp_len], INTERP_BASE) {
            Some(li) => { entry = li.entry; at_base = li.base; }
            None => { crate::println!("talaria: cell: the interpreter was not loaded"); crate::suite::park() }
        }
    }
    crate::println!("cell: the ELF is loaded (entry={:#x}) — mapping a 32 MB stack", entry);
    // a 32 MB stack (large C++ software keeps big objects on the stack) and a 64 MB heap
    let (ustack_top, woff) = map_proc_stack(pml4, DT_STACK_FRAMES);
    crate::println!("cell: the stack is mapped — allocating a 64 MB heap");
    let hb = crate::mem::alloc_contig_node(BIG_HEAP_FRAMES, node).expect("cell: heap");
    // The heap's node is printed AS IT IS rather than as requested: "node 1 was asked for"
    // and "the heap is on node 1" are different statements, and the log needs the second one.
    crate::println!("cell: the heap {:#x} is allocated (node {}) — zeroing and mapping it",
        hb, crate::acpi::node_of_phys(hb));
    core::ptr::write_bytes(hb as *mut u8, 0, (BIG_HEAP_FRAMES * 4096) as usize);
    for p in 0..BIG_HEAP_FRAMES { crate::paging::map_leaf(pml4, PROC_HEAP_BASE + p * 4096, hb + p * 4096, true, true, false); }
    crate::println!("cell: the heap is mapped — building the argument stack");
    let ustack = build_stack_full(ustack_top, entry, phdr, phent, phnum, args, envs, woff, at_base, img.entry, pslice, crate::vdso::map_into(pml4));
    let tid = crate::sched::spawn_process(entry, ustack, kmain, pml4);
    crate::sched::set_kstack_pages(tid, crate::sched::KSTACK_PAGES);
    crate::sched::set_heap(tid, PROC_HEAP_BASE, PROC_HEAP_BASE + BIG_HEAP_FRAMES * 4096);
    crate::proc::init_proc(tid);
    crate::fr::record(crate::fr::USER, entry);
    crate::println!("talaria: a multi-threaded workload from the disk in ring 3 (its own AS), entry={:#x}, argc={}, envc={}", entry, args.len(), envs.len());
    crate::smp::lapic_enable();
    crate::smp::lapic_timer_init(crate::smp::SLICE_NS);
    crate::sched::install();
    // The layout by the manifest: the main thread may move to a dedicated core —
    // then the control plane is left without a task and goes idle.
    if crate::affinity::place_main(tid, basename(pslice)) { crate::sched::start_idle(); }
    crate::sched::start();
}

/// The binary's base name (after the last '/'), without the trailing \0 — layout rules
/// are written for it on an equal footing with the keyword `main`.
pub fn basename(path: &[u8]) -> &[u8] {
    let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
    let p = &path[..end];
    let b = p.iter().rposition(|&c| c == b'/').map(|i| i + 1).unwrap_or(0);
    &p[b..]
}

/// The former path of mode=cell: the workload in the kernel's identity space (talaria.cell=identity).
/// # Safety
/// L1 + L4, `path` as in `run_disk`. This is the identity-AS variant
/// (`talaria.cell=identity`): the workload shares the kernel's address space by
/// construction, which is that mode's model and the reason `uaccess` cannot
/// protect anything there but the kernel image.
unsafe fn run_disk_threaded_identity(path: *const u8, args: &[&[u8]], envs: &[&[u8]]) -> ! {
    use core::sync::atomic::Ordering;
    let kmain = crate::sched::alloc_kstack().unwrap();
    note_stack(DT_STACK_FRAMES);
    if DT_STACK == 0 { DT_STACK = crate::mem::alloc_contig(DT_STACK_FRAMES).unwrap(); }
    let ub = DT_STACK;
    let ustack_top = ub + DT_STACK_FRAMES * 4096;
    crate::paging::unguard(ub, DT_STACK_FRAMES * 4096); // after a restart: return present
    setup_gdt(kmain);
    crate::syscall::init();
    let (entry, phdr, phent, phnum) = load_elf_from_disk(path);
    if entry == 0 { crate::println!("talaria: the binary was not found on the disk"); crate::suite::park() }
    let ustack = build_stack_args_env(ustack_top, entry, phdr, phent, phnum, args, envs, 0, vdso_here());
    harden_stack(ub, ustack_top);
    crate::fr::record(crate::fr::USER, entry);
    crate::syscall::THREADED.store(true, Ordering::SeqCst);
    crate::sched::spawn_user(entry, ustack, kmain, 0, 0); // task 0 = the main thread
    crate::sched::set_kstack_pages(0, crate::sched::KSTACK_PAGES);
    crate::println!("talaria: a multi-threaded workload from the disk in ring 3 (identity), entry={:#x}, argc={}, envc={}", entry, args.len(), envs.len());
    crate::smp::lapic_enable();
    crate::smp::lapic_timer_init(crate::smp::SLICE_NS);
    crate::sched::install();
    {
        let plen = { let mut n = 0; while *path.add(n) != 0 { n += 1; } n };
        let name = basename(core::slice::from_raw_parts(path, plen));
        if crate::affinity::place_main(0, name) { crate::sched::start_idle(); }
    }
    // Without sti: IF=1 for the task will be restored by iretq (the spawn_user frame, RFLAGS 0x202).
    // sti here opened a race window: a pending interrupt (after a UEFI boot it is
    // already waiting in the IRR) hit before start(), rust_schedule saved THE BOOT
    // context into RSP[0] over the user frame — an eternal entry into itself (see sched::start).
    crate::sched::start();
}

/// Run a multi-threaded program: the main thread as the scheduler's task 0,
/// then start the preemptive scheduler (clone adds threads on the fly).
///
/// # Safety
/// L1 + L2 + L4: `elf` is an EMBEDDED image (`user_elf!`, see `require_embedded`),
/// the launch path, does not return.
pub unsafe fn run_threaded(elf: &[u8]) -> ! {
    use core::sync::atomic::Ordering;
    let kmain = crate::sched::alloc_kstack().unwrap();
    let ub = crate::mem::alloc_frame().unwrap();
    for _ in 0..15 { crate::mem::alloc_frame().unwrap(); } // 16 frames (a 64 KB stack for the main thread)
    note_stack(16);
    let ustack_top = ub + 16 * 4096;
    setup_gdt(kmain);
    crate::syscall::init();
    let entry = load_elf(elf);
    let ustack = build_stack(ustack_top, entry);
    harden_stack(ub, ustack_top); // a guard page at the bottom + NX on the main thread's stack
    crate::fr::record(crate::fr::USER, entry);
    crate::syscall::THREADED.store(true, Ordering::SeqCst);
    crate::sched::spawn_user(entry, ustack, kmain, 0, 0); // task 0 = the main thread
    crate::println!("talaria: a multi-threaded program in ring 3 (clone/futex/join)");
    crate::smp::lapic_enable();
    crate::smp::lapic_timer_init(crate::smp::SLICE_NS);
    crate::sched::install();
    if crate::affinity::place_main(0, b"main") { crate::sched::start_idle(); }
    // Without sti: IF=1 for the task will be restored by iretq (the spawn_user frame, RFLAGS 0x202).
    // sti here opened a race window: a pending interrupt (after a UEFI boot it is
    // already waiting in the IRR) hit before start(), rust_schedule saved THE BOOT
    // context into RSP[0] over the user frame — an eternal entry into itself (see sched::start).
    crate::sched::start();
}


/// An embedded ELF is empty when the kernel is built without the `selftest`
/// feature (see `user_elf!` in main.rs): say so and stop instead of parsing it.
fn require_embedded(elf: &[u8]) {
    if elf.is_empty() {
        crate::acpi::shutdown_now("this mode runs an embedded test binary, but the kernel \
            was built without the `selftest` feature (cargo build --features selftest, \
            after `make -C kernel/user all`)");
    }
}

/// Load the ELF's PT_LOAD segments at their vaddrs, return the entry point.
///
/// # Safety
/// L1 + L2 + L3, and the segments go to their OWN vaddrs in the current identity
/// AS — so this is only for the embedded images, whose vaddrs the kernel's linker
/// script keeps clear of the kernel itself.
unsafe fn load_elf(elf: &[u8]) -> u64 {
    require_embedded(elf);
    let Some(h) = crate::elfhdr::header(elf) else {
        crate::println!("talaria: the embedded binary is not an ELF64 for x86-64"); return 0;
    };
    let Some(tab) = elf.get(h.phoff as usize..) else { return 0; };
    for p in crate::elfhdr::phdrs(tab, h.phentsize, h.phnum).filter(|p| p.is_load()) {
        if !p.fits(elf.len()) {
            crate::println!("talaria: the binary's segment goes beyond the file (offset {:#x}, {} B)", p.off, p.filesz);
            return 0;
        }
        let (off, vaddr) = (p.off as usize, p.vaddr);
        let (filesz, memsz) = (p.filesz as usize, p.memsz as usize);
        core::ptr::copy_nonoverlapping(elf.as_ptr().add(off), vaddr as *mut u8, filesz);
        if memsz > filesz {
            core::ptr::write_bytes((vaddr as *mut u8).add(filesz), 0, memsz - filesz);
        }
        // W^X per segment: p_flags (PF_X=1, PF_W=2) -> the rights of the segment's pages.
        let (w, x) = p.rights();
        if x { crate::arch::icache::sync(vaddr, memsz as u64); }
        crate::paging::protect(vaddr, memsz as u64, w, x);
    }
    h.entry
}

/// Build the process's initial stack (SysV x86-64): argc/argv/envp/auxv.
/// Returns RSP (it points at argc, 16-aligned).
///
/// # Safety
/// As `build_stack_args` (`top` is the top of a mapped RW stack).
unsafe fn build_stack(top: u64, entry: u64) -> u64 {
    let mut p = top;
    // AT_RANDOM: 16 bytes
    p -= 16;
    for i in 0..16 { *(p as *mut u8).add(i) = (0x5A ^ i as u8).wrapping_mul(7); }
    let random = p;
    // the argv[0] string
    let arg = b"talaria-prog\0";
    p -= arg.len() as u64;
    core::ptr::copy_nonoverlapping(arg.as_ptr(), p as *mut u8, arg.len());
    let arg0 = p;
    // the array: argc, argv[0], NULL, envp NULL, auxv(PAGESZ,ENTRY,RANDOM,NULL)
    let items: [u64; 14] = [
        1,                    // argc
        arg0, 0,              // argv[0], NULL
        0,                    // envp NULL
        6, 4096,              // AT_PAGESZ
        9, entry,             // AT_ENTRY
        25, random,           // AT_RANDOM
        33, vdso_here(),      // AT_SYSINFO_EHDR (0 = there is no vDSO) — the clock without a syscall
        0, 0,                 // AT_NULL
    ];
    let bytes = (items.len() * 8) as u64;
    let sp = (p - bytes) & !0xF; // 16-align RSP(argc)
    for (i, &v) in items.iter().enumerate() {
        core::ptr::write((sp as *mut u64).add(i), v);
    }
    sp
}

/// Run the built-in static ELF (musl/raw) in ring 3.
pub fn run(elf: &[u8]) -> ! {
    let kstack = crate::sched::alloc_kstack().unwrap();
    // the user's stack: several frames
    let ub = crate::mem::alloc_frame().unwrap();
    for _ in 0..7 { crate::mem::alloc_frame().unwrap(); } // 8 frames in a row (a 32 KB stack)
    note_stack(8);
    let ustack_top = ub + 8 * 4096;
    // SAFETY: L1 + L2 + L3 + L4 — the launch path on the boot core: the stack frames
    // have just been allocated (RW identity), the ELF is an embedded one, and the block
    // ends in `iretq` into ring 3, so nothing returns onto this kernel stack.
    unsafe {
        setup_gdt(kstack);
        crate::syscall::init();
        let entry = load_elf(elf);
        let ustack = build_stack(ustack_top, entry);
        harden_stack(ub, ustack_top); // a guard page at the bottom + NX on the stack
        crate::fr::record(crate::fr::USER, entry);
        crate::println!("talaria: launching a static ELF in ring 3, entry={:#x}", entry);
        // into ring 3 with IF=0 (no interrupts in ring 3)
        enter_user(entry, ustack, crate::arch::switch::USER_FLAGS_IRQ_OFF);
    }
}

/// Enter ring3 at (rip,rsp) expecting a #PF; return the number of the caught vector.
/// It uses recovery (setjmp/longjmp): a fault in ring3 -> the #PF handler does a
/// longjmp back here (the same scheme as supervise).
///
/// # Safety
/// L1: the GDT/TSS of this core are already set up, `rsp` points at a mapped user
/// stack and `rip` at a user page. The fault is EXPECTED: recovery must be armed
/// only around this call, otherwise a foreign fault would longjmp into a dead
/// frame.
unsafe fn ring3_expect_fault(rip: u64, rsp: u64) -> u64 {
    use crate::supervise::{tal_setjmp, RECOVERY, RECOVERY_ARMED, CRASH_VEC};
    use core::sync::atomic::Ordering;
    CRASH_VEC.store(0, Ordering::SeqCst);
    if tal_setjmp(core::ptr::addr_of_mut!(RECOVERY)) == 0 {
        RECOVERY_ARMED.store(true, Ordering::SeqCst);
        enter_user(rip, rsp, crate::arch::switch::USER_FLAGS_IRQ_OFF);
    }
    RECOVERY_ARMED.store(false, Ordering::SeqCst);
    CRASH_VEC.load(Ordering::SeqCst)
}

/// A self-check of the memory protection. It loads a real musl binary and checks
/// that the loader set W^X per segment (.text=R-X, .data=RW-NX), a guard page and
/// an NX stack; then LIVE #PFs: (1) executing an NX page, (2) a write into RO .text.
pub fn wxorx_test(elf: &[u8]) -> ! {
    let kstack = crate::sched::alloc_kstack().unwrap();
    // SAFETY: L1 + L2 + L4 — the same launch path; the faults it provokes are caught by
    // recovery (`ring3_expect_fault`), and the gate's verdict is printed before `park`.
    unsafe {
        setup_gdt(kstack);
        crate::syscall::init();
        // 1) a real binary -> the per-segment protect is applied inside load_elf
        let _entry = load_elf(elf);
        let (_p1, tw, _tu, tx) = crate::paging::query(0x1000_1000); // .text (R E in musl)
        let (_p2, dw, _du, dx) = crate::paging::query(0x1000_8000); // .data (RW in musl)
        crate::println!("wxorx: .text @0x10001000 w={} x={} (we expect w=false x=true)", tw, tx);
        crate::println!("wxorx: .data @0x10008000 w={} x={} (we expect w=true x=false)", dw, dx);
        let text_ok = !tw && tx;
        let data_ok = dw && !dx;

        // 2) the workload's stack: a guard at the bottom + NX
        let sframes = 4u64;
        let sb = crate::mem::alloc_contig(sframes).unwrap();
        let stop = sb + sframes * 4096;
        harden_stack(sb, stop);
        let (gp, _gw, _gu, _gx) = crate::paging::query(sb);        // the guard = the bottom
        let (_p3, sw, _su, sx) = crate::paging::query(sb + 4096);  // a working page of the stack
        crate::println!("wxorx: guard @{:#x} present={} (we expect false)", sb, gp);
        crate::println!("wxorx: the stack w={} x={} (we expect w=true x=false)", sw, sx);
        let guard_ok = !gp;
        let stack_ok = sw && !sx;

        // 2b) the brk heap: NX (W^X on the heap; executing it is not allowed)
        let hb = crate::syscall::heap_base();
        let (_p4, hw, _hu, hx) = crate::paging::query(hb);
        crate::println!("wxorx: the heap @{:#x} w={} x={} (we expect w=true x=false)", hb, hw, hx);
        let heap_ok = hw && !hx;

        // 3) a live #PF: executing an NX page (an instruction fetch with NX -> #PF)
        let nxp = crate::mem::alloc_frame().unwrap();
        core::ptr::write_bytes(nxp as *mut u8, 0x90, 4096);       // a NOP fill
        crate::paging::protect(nxp, 4096, false, false);          // present, RO, NX, US
        let nx_vec = ring3_expect_fault(nxp, stop);
        crate::println!("wxorx: executing an NX page -> #{} (we expect 14)", nx_vec);

        // 4) a live #PF: a write into RO .text (a violation of W^X)
        let wxp = crate::mem::alloc_frame().unwrap();
        let code: [u8; 16] = [
            0x48, 0xB8, wxp as u8, (wxp>>8) as u8, (wxp>>16) as u8, (wxp>>24) as u8,
            (wxp>>32) as u8, (wxp>>40) as u8, (wxp>>48) as u8, (wxp>>56) as u8, // mov rax, imm64(wxp)
            0xC7, 0x00, 0x00, 0x00, 0x00, 0x00,                                 // mov dword [rax], 0
        ];
        core::ptr::copy_nonoverlapping(code.as_ptr(), wxp as *mut u8, 16);
        crate::paging::protect(wxp, 4096, false, true);           // present, R-X (RO), US
        let wx_vec = ring3_expect_fault(wxp, stop);
        crate::println!("wxorx: a write into RO .text -> #{} (we expect 14)", wx_vec);

        if text_ok && data_ok && guard_ok && stack_ok && heap_ok && nx_vec == 14 && wx_vec == 14 {
            crate::println!("wxorx: ALL CHECKS PASSED (W^X + NX + guard + an NX heap + a live #PF)");
        } else {
            crate::println!("wxorx: THERE ARE FAILURES text={} data={} guard={} stack={} heap={} nx={} wx={}",
                text_ok, data_ok, guard_ok, stack_ok, heap_ok, nx_vec, wx_vec);
        }
    }
    crate::suite::park();
}

/// Load the ELF's PT_LOAD segments into A GIVEN address space onto PRIVATE
/// frames (privatize per process), with W^X by p_flags. Returns the entry. We copy
/// by the frames' PHYSICAL addresses (the kernel identity-maps them) — it can be done before the switch to the process.
///
/// # Safety
/// L1 + L2 + L3: `pml4` is a process's table nobody runs yet, `elf` is an embedded
/// image.
unsafe fn load_elf_into_space(pml4: u64, elf: &[u8]) -> u64 {
    require_embedded(elf);
    let Some(h) = crate::elfhdr::header(elf) else {
        crate::println!("talaria: the embedded binary is not an ELF64 for x86-64"); return 0;
    };
    let Some(tab) = elf.get(h.phoff as usize..) else { return 0; };
    let entry = h.entry;
    for p in crate::elfhdr::phdrs(tab, h.phentsize, h.phnum).filter(|p| p.is_load()) {
        if !p.fits(elf.len()) {
            crate::println!("talaria: the binary's segment goes beyond the file (offset {:#x}, {} B)", p.off, p.filesz);
            return 0;
        }
        let (w, x) = p.rights();
        let off = p.off as usize;
        let vaddr = p.vaddr;
        let filesz = p.filesz as usize;
        let memsz = p.memsz as usize;
        let start = vaddr & !0xFFF;
        let end = (vaddr + memsz as u64 + 0xFFF) & !0xFFF;
        let npages = (end - start) / 4096;
        let frames = crate::mem::alloc_contig(npages).expect("exec: frames");
        core::ptr::write_bytes(frames as *mut u8, 0, (npages * 4096) as usize); // zero (bss)
        core::ptr::copy_nonoverlapping(elf.as_ptr().add(off), (frames + (vaddr - start)) as *mut u8, filesz);
        if x { crate::arch::icache::sync(frames, npages * 4096); }
        for k in 0..npages {
            crate::paging::map_leaf(pml4, start + k * 4096, frames + k * 4096, w, true, x);
        }
    }
    entry
}

const PROC_STACK_TOP: u64 = 0x7fff_f000_0000; // the top of the process's private stack
const PROC_HEAP_BASE: u64 = 0x30_0000_0000;   // the base of the process's private heap (192 GB, a private region)
const PROC_HEAP_FRAMES: u64 = 4096;           // 16 MB of heap per process

/// Map the process's private heap (RW+NX+US, zeroed). Returns (base, top).
///
/// # Safety
/// L3: `pml4` is the process's own table; the frames come from `alloc_contig` and
/// are zeroed through the identity map before they are mapped, so the process
/// never sees another workload's leftovers.
unsafe fn map_proc_heap(pml4: u64) -> (u64, u64) {
    let base = crate::mem::alloc_contig(PROC_HEAP_FRAMES).expect("proc: heap");
    core::ptr::write_bytes(base as *mut u8, 0, (PROC_HEAP_FRAMES * 4096) as usize);
    for p in 0..PROC_HEAP_FRAMES {
        crate::paging::map_leaf(pml4, PROC_HEAP_BASE + p * 4096, base + p * 4096, true, true, false);
    }
    (PROC_HEAP_BASE, PROC_HEAP_BASE + PROC_HEAP_FRAMES * 4096)
}

/// Map the process's private stack (sframes pages) below PROC_STACK_TOP, RW+NX.
/// Below the bottom nothing is mapped (a natural guard page). Returns the vaddr of the top.
///
/// # Safety
/// L3, as `map_proc_heap`.
unsafe fn map_proc_stack(pml4: u64, sframes: u64) -> (u64, u64) {
    note_stack(sframes); // what RLIMIT_STACK must answer for the process about to run on it
    // The workload's stack comes from the same node as its heap and its threads (255 = there is one node
    // or no layout is set: then this is exactly the former alloc_contig).
    let sbase = crate::mem::alloc_contig_node(sframes, crate::affinity::launch_node()).expect("exec: stack");
    let vbottom = PROC_STACK_TOP - sframes * 4096;
    for p in 0..sframes {
        crate::paging::map_leaf(pml4, vbottom + p * 4096, sbase + p * 4096, true, true, false);
    }
    (PROC_STACK_TOP, sbase.wrapping_sub(vbottom)) // woff = phys - vaddr (for writing the stack before the switch)
}

/// exec: replace the image — load an ELF into a NEW private address space of the
/// process and run it in ring 3 (a single process; CR3 is switched).
///
/// # Safety
/// L1 + L2 + L4: an embedded image, the launch path, does not return.
pub unsafe fn exec(elf: &[u8]) -> ! {
    let kstack = crate::sched::alloc_kstack().unwrap();
    let pml4 = crate::paging::new_process_space();
    let entry = load_elf_into_space(pml4, elf);
    let (ustack_top, _woff) = map_proc_stack(pml4, 32); // a 128 KB stack
    let (hb, ht) = map_proc_heap(pml4);        // the process's private heap
    setup_gdt(kstack);
    crate::syscall::init();
    crate::sched::set_heap(0, hb, ht);         // the process's brk (task 0)
    crate::fr::record(crate::fr::USER, entry);
    crate::paging::switch(pml4); // activate the process's address space
    let ustack = build_stack(ustack_top, entry); // argv/auxv by vaddr (the pages are mapped)
    crate::println!("talaria: exec in the process's private AS, entry={:#x}", entry);
    enter_user(entry, ustack, crate::arch::switch::USER_FLAGS_IRQ_OFF);
}

/// Run SEVERAL PROCESSES, each in ITS OWN address space, under the
/// preemptive scheduler with a per-process CR3 switch. A proof of the process model.
///
/// # Safety
/// L1 + L2 + L4: embedded images, the launch path, does not return.
pub unsafe fn run_processes(elfs: &[&[u8]]) -> ! {
    use core::sync::atomic::Ordering;
    let kern_cr3 = crate::paging::current_cr3();
    let kstack0 = crate::sched::alloc_kstack().unwrap();
    setup_gdt(kstack0);        // GDT/TSS/syscall — once
    crate::syscall::init();
    crate::syscall::THREADED.store(true, Ordering::SeqCst); // exit = the end of the task, not of the whole kernel
    for (idx, elf) in elfs.iter().enumerate() {
        let pml4 = crate::paging::new_process_space();
        let entry = load_elf_into_space(pml4, elf);
        let (ustack_top, _woff) = map_proc_stack(pml4, 32);
        let (hb, ht) = map_proc_heap(pml4);             // the process's private heap
        let kstack = if idx == 0 { kstack0 } else { crate::sched::alloc_kstack().unwrap() };
        crate::paging::switch(pml4);                    // we temporarily activate the process's AS
        let ustack = build_stack(ustack_top, entry);    // we build its stack by vaddr
        crate::paging::switch(kern_cr3);                // we return to build the next one
        let tid = crate::sched::spawn_process(entry, ustack, kstack, pml4);
        crate::sched::set_heap(tid, hb, ht);            // per-process brk
        crate::fr::record(crate::fr::USER, entry);
    }
    crate::println!("talaria: {} process(es), each in its own address space (per-process CR3)", elfs.len());
    crate::smp::lapic_enable();
    crate::smp::lapic_timer_init(crate::smp::SLICE_NS);
    crate::sched::install();
    // Without sti: IF=1 for the task will be restored by iretq (the spawn_user frame, RFLAGS 0x202).
    // sti here opened a race window: a pending interrupt (after a UEFI boot it is
    // already waiting in the IRR) hit before start(), rust_schedule saved THE BOOT
    // context into RSP[0] over the user frame — an eternal entry into itself (see sched::start).
    crate::sched::start();
}

/// Run SEVERAL PROCESSES FROM THE DISK, each in ITS OWN private AS, under the
/// scheduler (per-process CR3). The disk analogue of run_processes — it closes A1.3
/// (run_disk-class workloads in isolated ASes rather than in the common kernel one). Identical
/// stack/heap vaddrs in all the processes -> a coincidence of the addresses proves the isolation.
/// # Safety
/// L1 + L2 + L4. `ELFBUF` below is a single static buffer reused for one image at a
/// time — which is sound here only because this is the launch path (L1): one core,
/// no concurrent loader.
pub unsafe fn run_processes_disk(paths: &[&[u8]]) -> ! {
    use core::sync::atomic::Ordering;
    static mut ELFBUF: [u8; 2 << 20] = [0; 2 << 20]; // 2 MB for an ELF (one at a time)
    let kern_cr3 = crate::paging::current_cr3();
    let kstack0 = crate::sched::alloc_kstack().unwrap();
    setup_gdt(kstack0);
    crate::syscall::init();
    crate::syscall::THREADED.store(true, Ordering::SeqCst);
    let mut launched = 0usize;
    for (idx, path) in paths.iter().enumerate() {
        let buf = &mut *core::ptr::addr_of_mut!(ELFBUF);
        let n = crate::fs::read_all(path, buf);
        if n == 0 { crate::println!("talaria: procdisk: the file was not found (idx={})", idx); continue; }
        let elf = &buf[..n];
        let pml4 = crate::paging::new_process_space();
        let entry = load_elf_into_space(pml4, elf);
        let (ustack_top, _woff) = map_proc_stack(pml4, 32);
        let (hb, ht) = map_proc_heap(pml4);
        let kstack = if launched == 0 { kstack0 } else { crate::sched::alloc_kstack().unwrap() };
        crate::paging::switch(pml4);
        let ustack = build_stack(ustack_top, entry);
        crate::paging::switch(kern_cr3);
        let tid = crate::sched::spawn_process(entry, ustack, kstack, pml4);
        crate::sched::set_heap(tid, hb, ht);
        crate::fr::record(crate::fr::USER, entry);
        launched += 1;
    }
    crate::println!("talaria: {} process(es) FROM THE DISK in separate ASes (identical vaddrs, per-process CR3)", launched);
    crate::smp::lapic_enable();
    crate::smp::lapic_timer_init(crate::smp::SLICE_NS);
    crate::sched::install();
    // Without sti: IF=1 for the task will be restored by iretq (the spawn_user frame, RFLAGS 0x202).
    // sti here opened a race window: a pending interrupt (after a UEFI boot it is
    // already waiting in the IRR) hit before start(), rust_schedule saved THE BOOT
    // context into RSP[0] over the user frame — an eternal entry into itself (see sched::start).
    crate::sched::start();
}

/// execve of A REAL image from ext2: a dynamic/PIE binary (PT_INTERP -> ld-musl/
/// ld-linux), the environment (envp) and the full auxv — the same as a launch by the manifest does.
///
/// What for: the `java` launcher (and not it alone) RESTARTS itself — it appends
/// LD_LIBRARY_PATH and does execve(/jdk/bin/java). The former execve loaded the ELF
/// "as is" (without the PIE base, without the interpreter, without envp) — control went
/// to the offset from the header, that is, into nowhere (#PF rip=0x127c). Such an execve
/// was good only for TFS statics.
///
/// Descriptors survive an exec (except FD_CLOEXEC), the signal handlers are
/// reset to SIG_DFL, the mmap region starts anew (proc::exec_reset).
/// The old address space stays with the task until it ends (freeing it
/// on a live CR3 is unsafe) — that is a one-off leak of an image, not a leak per stream.
/// # Safety
/// L1 (the `execve` half: the caller has already stopped the sister threads, see
/// `drain_proc` below) + L2 + L3 + L4. `path`/`argv`/`envp` must already be COPIES
/// in the kernel's memory — the syscall path copies them out of the process before
/// the old address space goes away.
pub unsafe fn execve_dyn(path: &[u8], argv: &[&[u8]], envp: &[&[u8]]) -> ! {
    let tid = crate::sched::current_id();
    // On a successful exec Linux leaves the process ONE thread — the calling one. The sisters
    // are marked dead and are taken off the cores SYNCHRONOUSLY (sched::exit_siblings
    // waits until none of them is listed as current): changing the address space under
    // a thread that is still executing in it is not allowed.
    crate::sched::exit_siblings(0);
    let node = crate::affinity::launch_node();
    let pml4 = crate::paging::new_process_space();
    let img = match load_elf_into_space_disk(pml4, path, PIE_BASE) {
        Some(i) => i,
        None => { crate::println!("talaria: execve: the image was not loaded"); crate::sched::exit_current_code(127); }
    };
    let (mut entry, phdr, phent, phnum) = (img.entry, img.phdr, img.phent, img.phnum);
    let mut at_base = 0u64;
    if img.interp_len > 0 {
        match load_elf_into_space_disk(pml4, &img.interp[..img.interp_len], INTERP_BASE) {
            Some(li) => { entry = li.entry; at_base = li.base; }
            None => { crate::println!("talaria: execve: the interpreter was not loaded"); crate::sched::exit_current_code(127); }
        }
    }
    let vdso = crate::vdso::map_into(pml4);
    let (ustack_top, woff) = map_proc_stack(pml4, BIG_STACK_FRAMES);
    let hb = match crate::mem::alloc_contig_node(BIG_HEAP_FRAMES, node) { Some(h) => h, None => { crate::println!("talaria: execve: no memory for the heap");
        crate::sched::exit_current_code(127); } };
    core::ptr::write_bytes(hb as *mut u8, 0, (BIG_HEAP_FRAMES * 4096) as usize);
    for p in 0..BIG_HEAP_FRAMES { crate::paging::map_leaf(pml4, PROC_HEAP_BASE + p * 4096, hb + p * 4096, true, true, false); }
    crate::proc::exec_reset(crate::sched::proc_of(tid));
    crate::sched::set_heap(tid, PROC_HEAP_BASE, PROC_HEAP_BASE + BIG_HEAP_FRAMES * 4096);
    crate::sched::set_heap_brk(tid, PROC_HEAP_BASE);
    crate::sched::set_task_cr3(tid, pml4);
    let ustack = build_stack_full(ustack_top, entry, phdr, phent, phnum, argv, envp, woff, at_base, img.entry, path, vdso);
    crate::fr::record(crate::fr::USER, entry);
    crate::println!("talaria: execve: {} threads 1, entry={:#x} (argc={}, envc={})",
        core::str::from_utf8(&path[..path.iter().position(|&c| c == 0).unwrap_or(path.len())]).unwrap_or("?"), entry, argv.len(), envp.len());
    switch_and_enter_user(pml4, entry, ustack, crate::arch::switch::USER_FLAGS_IRQ_ON);
}

/// execve: replace the image of THE CURRENT process task — load an ELF from the disk into a new
/// private AS, update the task's CR3, go into ring 3 with argv. It does not return on
/// success; if the file is missing it ends the task with code 127.
/// # Safety
/// As `execve_dyn`. `EXECBUF` is one static buffer: an `execve` runs with the
/// process's other threads already stopped, so there is no second loader in it.
pub unsafe fn execve(path: &[u8], argv: &[&[u8]]) -> ! {
    static mut EXECBUF: [u8; 2 << 20] = [0; 2 << 20]; // 2 MB for an ELF
    let buf = &mut *core::ptr::addr_of_mut!(EXECBUF);
    let n = crate::fs::read_all(path, buf);
    if n == 0 { crate::println!("talaria: execve: the file was not found"); crate::sched::exit_current_code(127); }
    let elf = &buf[..n];
    let pml4 = crate::paging::new_process_space();
    let entry = load_elf_into_space(pml4, elf);
    // We build the new AS's stack through ITS phys (woff) BEFORE switching CR3 — execve runs
    // on the child's user stack, which switch(pml4) will remap onto fresh frames.
    let (ustack_top, woff) = map_proc_stack(pml4, 32);
    let (hb, ht) = map_proc_heap(pml4);
    let tid = crate::sched::current_id();
    crate::sched::set_heap(tid, hb, ht);
    crate::sched::set_task_cr3(tid, pml4); // the task moves into the new AS
    // execve builds the stack of A FOREIGN (new) space while being in the old one:
    // the vDSO must land in the child's pml4 rather than in the current CR3.
    let ustack = build_stack_args_off(ustack_top, entry, 0, 0, 0, argv, woff, crate::vdso::map_into(pml4));
    crate::fr::record(crate::fr::USER, entry);
    // switch + iretq as A SINGLE asm: entry/ustack/pml4 in registers (taken before the switch from
    // the old stack). The push of the iret frame goes into the current rsp — it is mapped in the new AS
    // (the same stack region). No accesses to locals after mov cr3.
    switch_and_enter_user(pml4, entry, ustack, crate::arch::switch::USER_FLAGS_IRQ_OFF);
}

/// Run an ELF EXPECTING a #PF (for the protection tests: UAF/munmap). It catches a ring-3 fault
/// through recovery (like supervise) and prints the verdict. It does not return.
pub fn run_expect_fault(elf: &[u8]) -> ! {
    use crate::supervise::{tal_setjmp, RECOVERY, RECOVERY_ARMED, CRASH_VEC};
    use core::sync::atomic::Ordering;
    // SAFETY: L1 + L4 — the launch path. `tal_setjmp` records THIS frame and the #PF
    // handler longjmps back into it: the frame is alive for as long as it may be used,
    // because `run` never returns by itself and this function ends in `park`.
    unsafe {
        let recovered = tal_setjmp(core::ptr::addr_of_mut!(RECOVERY));
        if recovered == 0 {
            RECOVERY_ARMED.store(true, Ordering::SeqCst);
            run(elf); // it loads + iretq; a ring-3 #PF -> a recovery longjmp back here
        }
        RECOVERY_ARMED.store(false, Ordering::SeqCst);
        let vec = CRASH_VEC.load(Ordering::SeqCst);
        let cr2 = crate::arch::cpu::fault_addr();
        crate::println!("uaf: caught #{} after munmap CR2={:#x}", vec, cr2);
        if vec == 14 { crate::println!("uaf: ALL CHECKS PASSED (munmap removed the mapping, the UAF was prevented)"); }
        else { crate::println!("uaf: FAILURE (a #14 was expected, a #{} was received)", vec); }
        crate::suite::park();
    }
}
