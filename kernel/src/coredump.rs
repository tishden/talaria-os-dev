//! Core dumps: an ELF file of the `ET_CORE` format, which an ordinary GDB opens.
//!
//! ## Why a format and not "a dump of our own"
//!
//! The textual crash dump (`idt::rust_exception`) answers the question "what
//! happened" and is always printed — it requires neither a disk nor tools, and
//! that is the right first line of defence. But the questions "and what was in the variable" and
//! "show me the stack with names" it does not and cannot answer: for that a debugger is needed,
//! and the debugger needs ITS format.
//!
//! So the dump is written as an `ET_CORE`, and then the ordinary chain works:
//!
//! ```text
//! gdb <binary> <core>        (gdb) bt        (gdb) info registers
//!                            (gdb) thread apply all bt
//! ```
//!
//! There is no need to know anything Talaria-specific — this is exactly the file that
//! GDB expects from Linux.
//!
//! ## What we put in and what we do NOT put in
//!
//! | it is needed for | what we put in |
//! |---|---|
//! | the registers, `bt` | an `NT_PRSTATUS` for EVERY thread (the first is the crashed one) |
//! | the process's name | `NT_PRPSINFO` |
//! | the unwinding of the stack, the local variables | a `PT_LOAD` with the threads' STACKS |
//! | the code, the symbols | NOTHING: gdb reads them out of the binary you gave it |
//!
//! **The heap and the data are NOT dumped by default.** In ClickHouse that is 700 MB, and
//! writing them to the disk at the moment of a crash means with a high probability not
//! writing anything. The stacks of all the threads are a few megabytes, and that is enough for
//! `bt`, the arguments and the local variables. A full dump of the private memory is
//! switched on explicitly: `talaria.core=full`.
//!
//! ## The kernel too
//!
//! For a fault IN THE KERNEL the same `ET_CORE` is written, only the registers are the exception's
//! frame, and the `PT_LOAD` is the kernel stack of the crashed path. `gdb kernel/.../talaria
//! core-kernel` gives a `bt` over the kernel's code. The kernel's stack is unwound by the frame
//! pointers, so there will be names in the `bt` in a build of `kernel/build-prof.sh`; without
//! it gdb will show the top frame and the addresses (see PERFORMANCE-OPTIMIZATION-GUIDE
//! §4.2).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// ── ELF ──────────────────────────────────────────────────────────────────────
const ET_CORE: u16 = 4;
/// The machine of the dump: the one this kernel runs (`EM_X86_64` 62, `EM_AARCH64` 183).
const EM_THIS: u16 = crate::arch::ELF_MACHINE;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const PF_R: u32 = 4;
const PF_W: u32 = 2;

/// How many bytes of a thread's stack to put into the dump. 256 KB is enough for any sane
/// depth; a thread whose stack is deeper usually crashes because of that, and then
/// what matters is not the tail but the top — and that is what lies here.
const STACK_BYTES: u64 = 256 * 1024;
/// The ceiling on the number of segments: for a process with a hundred threads a dump must not
/// turn into a walk over the whole memory.
const MAXSEG: usize = 96;

/// Where to write the dump (`talaria.core=<path>`), empty — do not write.
const PATHMAX: usize = 96;
static mut CORE_PATH: [u8; PATHMAX] = [0; PATHMAX];
static CORE_PATH_LEN: AtomicU64 = AtomicU64::new(0);
/// `talaria.core=...,full` — put in ALL the process's private memory rather than the stacks.
static CORE_FULL: AtomicBool = AtomicBool::new(false);
/// A dump is already being written on this core: a recursion (a fault inside the writing of a dump) is bound
/// to stop rather than bring the machine down a second time.
static IN_DUMP: AtomicBool = AtomicBool::new(false);

pub fn enabled() -> bool { CORE_PATH_LEN.load(Ordering::Relaxed) != 0 }
pub fn full() -> bool { CORE_FULL.load(Ordering::Relaxed) }

/// Parse `talaria.core=<path>[,full]`. A path without a leading `/` is rejected:
/// the dump is written onto a mounted FS, and a relative path means nothing here.
pub fn configure_from_cmdline(cmdline: &str) {
    let Some(v) = crate::cmdline::value(cmdline, "talaria.core") else { return };
    let (path, full) = match v.split_once(',') { Some((p, f)) => (p, f.contains("full")), None => (v, false) };
    if !path.starts_with('/') || path.len() >= PATHMAX {
        crate::println!("talaria: talaria.core={} — an absolute path shorter than {} bytes is needed", v, PATHMAX);
        return;
    }
    let dst: &mut [u8; PATHMAX] = unsafe { &mut *core::ptr::addr_of_mut!(CORE_PATH) };
    dst[..path.len()].copy_from_slice(path.as_bytes());
    dst[path.len()] = 0;
    CORE_PATH_LEN.store(path.len() as u64, Ordering::SeqCst);
    CORE_FULL.store(full, Ordering::SeqCst);
    crate::println!("talaria: the core dumps are written into {} ({})", path,
        if full { "all the private memory" } else { "the threads' stacks" });
}

fn core_path() -> &'static [u8] {
    let n = CORE_PATH_LEN.load(Ordering::Relaxed) as usize;
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(CORE_PATH) as *const u8, n + 1) }
}

// ── the assembly of the file ─────────────────────────────────────────────────
/// One memory segment in a dump.
#[derive(Clone, Copy)]
struct Seg { vaddr: u64, len: u64, flags: u32 }

/// The accumulator: an ELF is written as a stream, so the offsets are computed in advance, and
/// the segments' contents are copied straight out of the process's memory.
struct Writer {
    fd: i64,
    off: u64,
    ok: bool,
}
impl Writer {
    fn put(&mut self, b: &[u8]) {
        if !self.ok || b.is_empty() { return; }
        let mut done = 0usize;
        while done < b.len() {
            let r = crate::ext2::write(self.fd, unsafe { b.as_ptr().add(done) }, b.len() - done);
            if r <= 0 { self.ok = false; return; }
            done += r as usize;
        }
        self.off += b.len() as u64;
    }
    fn put_u16(&mut self, v: u16) { self.put(&v.to_le_bytes()); }
    fn put_u32(&mut self, v: u32) { self.put(&v.to_le_bytes()); }
    fn put_u64(&mut self, v: u64) { self.put(&v.to_le_bytes()); }
    fn pad4(&mut self) { let n = (4 - (self.off % 4) as usize) % 4; if n > 0 { self.put(&[0u8; 4][..n]); } }
}

/// The registers in the order of `user_regs_struct` — that is exactly what GDB expects in an `NT_PRSTATUS`.
/// The order is not ours and must not be changed: fields swapped round would give not
/// "a format error" but a PLAUSIBLE wrong stack.
fn user_regs(f: &crate::idt::ExcFrame, fs_base: u64) -> [u64; crate::elfnote::NREGS] {
    crate::arch::signal::prstatus_regs(f, fs_base)
}

/// Write the note `NT_PRSTATUS` (the bytes are built by `elfnote`, which is tested on the host).
fn note_prstatus(w: &mut Writer, sig: u32, pid: i32, regs: &[u64; crate::elfnote::NREGS]) {
    w.put(&crate::elfnote::prstatus(sig, pid, regs));
    w.pad4();
}

/// The note `NT_PRPSINFO`: gdb prints the program's name out of it.
fn note_prpsinfo(w: &mut Writer, pid: i32, name: &[u8]) {
    w.put(&crate::elfnote::prpsinfo(pid, name));
    w.pad4();
}

/// The size of the notes, counted by the module that writes them.
fn notes_size(threads: usize) -> u64 { crate::elfnote::notes_size(threads) }

/// How many consecutive mapped pages there are starting from `a` (the ceiling `max`).
fn mapped_run(a: u64, max: u64, user: bool) -> u64 {
    let mut n = 0;
    while n < max {
        let (present, _, us, _) = crate::paging::query(a + n);
        if !present || (user && !us) { break; }
        n += 4096;
    }
    n
}

/// A thread's stack as a segment: from the page under the `rsp` upwards, while there are pages.
fn stack_seg(rsp: u64, user: bool) -> Option<Seg> {
    if rsp < 0x1000 { return None; }
    let start = rsp & !0xFFF;
    let len = mapped_run(start, STACK_BYTES, user);
    if len == 0 { return None; }
    Some(Seg { vaddr: start, len, flags: PF_R | PF_W })
}

// ── the dump of a process ────────────────────────────────────────────────────
/// Write the core dump of a crashed PROCESS. `f` is the exception's frame of the crashed thread.
/// It returns the number of bytes written (0 — it was not written).
pub fn dump_process(f: &crate::idt::ExcFrame, sig: u32) -> u64 {
    if !enabled() { return 0; }
    if IN_DUMP.swap(true, Ordering::SeqCst) { return 0; }   // a fault inside a dump — not a recursion
    let r = dump_process_inner(f, sig);
    IN_DUMP.store(false, Ordering::SeqCst);
    r
}

/// The process's threads that were alive at the moment of the crash. A crash first kills and
/// stops the sisters (`idt`), and by the writing of the dump there are no live ones left — the list
/// is taken BEFORE that.
static SNAP_N: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static mut SNAP: [usize; MAXSEG] = [0; MAXSEG];
pub fn snapshot_threads() {
    let cur = crate::sched::current_id();
    let proc_id = crate::sched::proc_of(cur);
    let n = crate::sched::task_count().min(crate::sched::max_tasks());
    let mut k = 0usize;
    for t in 0..n {
        if k >= MAXSEG { break; }
        if t == cur || crate::sched::proc_of(t) != proc_id || !crate::sched::task_alive(t) { continue; }
        unsafe { SNAP[k] = t; }
        k += 1;
    }
    SNAP_N.store(k + 1, Ordering::SeqCst); // +1: "there is a snapshot", even an empty one
}

fn dump_process_inner(f: &crate::idt::ExcFrame, sig: u32) -> u64 {
    let cur = crate::sched::current_id();
    let proc_id = crate::sched::proc_of(cur);
    // The threads of THIS process: the crashed one goes first — gdb makes it the current one.
    let mut tids = [0usize; MAXSEG];
    let mut nt = 0usize;
    tids[nt] = cur; nt += 1;
    let snap = SNAP_N.swap(0, Ordering::SeqCst);
    if snap != 0 {
        for k in 0..snap - 1 { if nt >= MAXSEG { break; } tids[nt] = unsafe { SNAP[k] }; nt += 1; }
    } else {
        let n = crate::sched::task_count().min(crate::sched::max_tasks());
        for t in 0..n {
            if nt >= MAXSEG { break; }
            if t == cur || crate::sched::proc_of(t) != proc_id || !crate::sched::task_alive(t) { continue; }
            tids[nt] = t; nt += 1;
        }
    }
    // The segments: the crashed thread's stack is taken from the FRAME (there the rsp is live), for the rest —
    // from the context saved by the scheduler.
    let mut segs = [Seg { vaddr: 0, len: 0, flags: 0 }; MAXSEG];
    let mut ns = 0usize;
    if let Some(s) = stack_seg(crate::arch::signal::exc_sp(f), true) { segs[ns] = s; ns += 1; }
    for i in 1..nt {
        if ns >= MAXSEG { break; }
        let rsp = crate::arch::signal::exc_sp(&frame_of_task(tids[i]));
        if let Some(s) = stack_seg(rsp, true) {
            if !segs[..ns].iter().any(|o| o.vaddr == s.vaddr) { segs[ns] = s; ns += 1; }
        }
    }
    if full() { ns = add_private(&mut segs, ns); }
    let mut nm = [0u8; 16];
    proc_name(cur, &mut nm);
    write_core(sig, proc_id as i32, &nm, &tids[..nt], &segs[..ns], f, true)
}

/// A full dump: all the private present pages of the address space.
/// The ranges are glued together, otherwise for a hundred megabytes there would have been twenty thousand
/// segments, and `PT_LOAD`s are not free either for us or for gdb.
fn add_private(segs: &mut [Seg; MAXSEG], mut ns: usize) -> usize {
    let cr3 = crate::paging::current_cr3() & !0xFFF;
    let mut a = 0x1000u64;
    // The upper bound is the end of the lower half: above it lives the kernel, and it has no place in a
    // process's dump.
    while a < 0x0000_8000_0000_0000 && ns < MAXSEG {
        let (present, _, us, _) = crate::paging::query(a);
        if !present || !us { a = next_gap(a); continue; }
        let len = mapped_run(a, 64 << 20, true);
        if len == 0 { a += 4096; continue; }
        if !segs[..ns].iter().any(|o| o.vaddr == a) { segs[ns] = Seg { vaddr: a, len, flags: PF_R | PF_W }; ns += 1; }
        a += len;
    }
    let _ = cr3;
    ns
}
/// The step of the search over a hole: one must not go page by page over the whole lower half (that is
/// 128 TB), so the empty areas are stepped over in 2 MB, and inside the occupied ones —
/// page by page.
fn next_gap(a: u64) -> u64 { (a + 0x20_0000) & !0x1F_FFFF }

// ── the dump of the kernel ───────────────────────────────────────────────────
/// Write the core dump of the KERNEL: the registers of the exception's frame + the kernel stack of the crashed
/// path. It is opened with `gdb kernel/target/.../talaria <file>`.
pub fn dump_kernel(f: &crate::idt::ExcFrame, sig: u32) -> u64 {
    if !enabled() { return 0; }
    if IN_DUMP.swap(true, Ordering::SeqCst) { return 0; }
    let cur = crate::sched::current_id();
    let mut segs = [Seg { vaddr: 0, len: 0, flags: 0 }; MAXSEG];
    let mut ns = 0usize;
    // The kernel's stack is the memory over which gdb will unwind the calls. `user=false`:
    // the kernel's pages are supervisor ones, the U/S bit must not be required.
    if let Some(s) = stack_seg(crate::arch::signal::exc_sp(f), false) { segs[ns] = s; ns += 1; }
    let tids = [cur];
    let r = write_core(sig, 0, b"talaria-kernel\0", &tids, &segs[..ns], f, false);
    IN_DUMP.store(false, Ordering::SeqCst);
    r
}

/// A dump of a LIVE task at the operator's request (the console's `core [tid]`).
///
/// There is one difference from a crash dump and it is essential: there are no exact registers.
/// The task stands in the scheduler, and the snapshot is taken from its saved context —
/// that is, it shows WHERE IT STANDS, not where it crashed. For a hung application
/// that is exactly the question that is asked.
pub fn dump_task(t: usize) -> u64 {
    if !enabled() {
        crate::println!("[core] the path is not set: talaria.core=<path> is needed on the kernel's command line");
        return 0;
    }
    if t >= crate::sched::max_tasks() || !crate::sched::task_alive(t) {
        crate::println!("[core] there is no live task t{}", t);
        return 0;
    }
    if IN_DUMP.swap(true, Ordering::SeqCst) { return 0; }
    let f = frame_of_task(t);
    let proc_id = crate::sched::proc_of(t);
    let mut tids = [0usize; MAXSEG];
    let mut nt = 0usize;
    tids[nt] = t; nt += 1;
    let n = crate::sched::task_count().min(crate::sched::max_tasks());
    for k in 0..n {
        if nt >= MAXSEG { break; }
        if k == t || crate::sched::proc_of(k) != proc_id || !crate::sched::task_alive(k) { continue; }
        tids[nt] = k; nt += 1;
    }
    let mut segs = [Seg { vaddr: 0, len: 0, flags: 0 }; MAXSEG];
    let mut ns = 0usize;
    for i in 0..nt {
        if ns >= MAXSEG { break; }
        let rsp = crate::arch::signal::exc_sp(&frame_of_task(tids[i]));
        if let Some(sg) = stack_seg(rsp, true) {
            if !segs[..ns].iter().any(|o| o.vaddr == sg.vaddr) { segs[ns] = sg; ns += 1; }
        }
    }
    let r = write_core(0, proc_id as i32, &crate::sched::comm(t), &tids[..nt], &segs[..ns], &f, true);
    IN_DUMP.store(false, Ordering::SeqCst);
    r
}

// ── the common writer ────────────────────────────────────────────────────────
fn write_core(sig: u32, pid: i32, name: &[u8], tids: &[usize], segs: &[Seg],
              f: &crate::idt::ExcFrame, user: bool) -> u64 {
    if !crate::ext2::active() {
        crate::println!("[core] the FS is not mounted (or is read-only) — there is nowhere to write the dump");
        return 0;
    }
    let path = core_path();
    let fd = crate::ext2::open(path, 0o1 | 0o100 | 0o1000, 0o644); // O_WRONLY|O_CREAT|O_TRUNC
    if fd < 0 {
        crate::println!("[core] {} could not be opened (the code {})", core::str::from_utf8(&path[..path.len() - 1]).unwrap_or("?"), fd);
        return 0;
    }
    let nph = 1 + segs.len();                       // a PT_NOTE + the PT_LOADs
    let ehsz = 64u64;
    let phsz = 56u64;
    let notes_off = ehsz + phsz * nph as u64;
    let notes_len = notes_size(tids.len());
    // We put the segments' data with a page alignment: gdb does not require that,
    // but that way the file is read by an mmap and coincides with what readelf shows.
    let data_off = (notes_off + notes_len + 0xFFF) & !0xFFF;

    let mut w = Writer { fd, off: 0, ok: true };
    // ── Ehdr ──
    w.put(&[0x7F, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    w.put_u16(ET_CORE); w.put_u16(EM_THIS); w.put_u32(1);
    w.put_u64(0); w.put_u64(ehsz); w.put_u64(0);    // entry, phoff, shoff
    w.put_u32(0); w.put_u16(ehsz as u16);
    w.put_u16(phsz as u16); w.put_u16(nph as u16);
    w.put_u16(0); w.put_u16(0); w.put_u16(0);       // shentsize, shnum, shstrndx
    // ── Phdr: PT_NOTE ──
    w.put_u32(PT_NOTE); w.put_u32(0);
    w.put_u64(notes_off); w.put_u64(0); w.put_u64(0);
    w.put_u64(notes_len); w.put_u64(notes_len); w.put_u64(4);
    // ── the Phdr: a PT_LOAD per segment ──
    let mut off = data_off;
    for s in segs {
        w.put_u32(PT_LOAD); w.put_u32(s.flags);
        w.put_u64(off); w.put_u64(s.vaddr); w.put_u64(s.vaddr);
        w.put_u64(s.len); w.put_u64(s.len); w.put_u64(4096);
        off += s.len;
    }
    // ── the notes ──
    // The numbers are the same as the workload itself sees (getpid = the leader + 1, gettid =
    // the task + 1), and every thread has ITS OWN: gdb takes the pr_pid from the NT_PRSTATUS as
    // the LWP's number, and with a common zero for all the threads it complained "found threads with pid
    // 0" and handed out the numbers itself — `thread N` in the debugger matched nothing
    // that `top -t` and the kernel's log print.
    let ppid = if user { pid + 1 } else { pid };
    let tid_of = |t: usize| if user { t as i32 + 1 } else { pid };
    note_prpsinfo(&mut w, ppid, name);
    let fsb = if user { crate::sched::saved_fsbase(tids[0]) } else { 0 };
    note_prstatus(&mut w, sig, tid_of(tids[0]), &user_regs(f, fsb));
    // The rest of the threads: the registers from the saved context. The accuracy here is lower
    // (the thread stood in the scheduler rather than at the point of the fault), and that is honest: it really
    // did stand there.
    for &t in &tids[1..] {
        let tf = frame_of_task(t);
        note_prstatus(&mut w, 0, tid_of(t), &user_regs(&tf, crate::sched::saved_fsbase(t)));
    }
    // ── the alignment up to data_off and the segments themselves ──
    // Byte by byte is impossible: up to four thousand calls of a write into the FS for the sake of zeroes, and that
    // at the moment of a crash, when time is dearest of all.
    let zeros = [0u8; 256];
    while w.off < data_off && w.ok {
        let n = ((data_off - w.off) as usize).min(zeros.len());
        w.put(&zeros[..n]);
    }
    for s in segs {
        let mut a = s.vaddr;
        let end = s.vaddr + s.len;
        while a < end && w.ok {
            let chunk = (end - a).min(4096);
            w.put(unsafe { core::slice::from_raw_parts(a as *const u8, chunk as usize) });
            a += chunk;
        }
    }
    let total = w.off;
    let ok = w.ok;
    crate::ext2::close(fd);
    crate::ext2::flush_all(); // a dump without a flush to the disk is a dump that does not exist
    if !ok {
        crate::println!("[core] the writing broke off at {} B — the dump is incomplete", total);
        return 0;
    }
    crate::println!("[core] {} B into {}: threads {}, memory segments {} ({} KB)",
        total, core::str::from_utf8(&path[..path.len() - 1]).unwrap_or("?"),
        tids.len(), segs.len(), segs.iter().map(|s| s.len).sum::<u64>() / 1024);
    crate::println!("[core] to read it: gdb <binary> <file>; to take it off the node — kernel/tools/talaria-file <ip> pull {} ./core",
        core::str::from_utf8(&path[..path.len() - 1]).unwrap_or("?"));
    total
}

/// The name for the `NT_PRPSINFO`: the thread's name, if it named itself, otherwise the base name
/// of the started file. An empty name gdb prints as `Core was generated by \`\'`,
/// that is, it prints nothing — while the program's name is the first thing one looks at.
fn proc_name(t: usize, out: &mut [u8; 16]) {
    let c = crate::sched::comm(t);
    let l = crate::sched::comm_len(t);
    if l > 0 { out[..l].copy_from_slice(&c[..l]); return; }
    let mut path = [0u8; 128];
    let n = crate::fs::exe_path(&mut path);
    if n <= 0 { out[..7].copy_from_slice(b"talaria"); return; }
    let p = &path[..n as usize];
    let start = p.iter().rposition(|&b| b == b'/').map(|i| i + 1).unwrap_or(0);
    let base = &p[start..];
    let k = base.len().min(15);
    out[..k].copy_from_slice(&base[..k]);
}

/// A frame "as if" for a thread that stands in the scheduler: it has no exact registers,
/// but the rip/rsp/rbp are enough for gdb to show where it stood.
fn frame_of_task(t: usize) -> crate::idt::ExcFrame {
    // The thread stopped on the way out of a syscall: its place is there, in user space
    // (the context saved after that is the exit path in the kernel).
    if !crate::sched::task_alive(t) {
        if let Some(r) = crate::sched::exit_uregs(t) { return crate::arch::signal::frame_of_exit_uregs(&r); }
    }
    crate::arch::signal::frame_of_saved(crate::sched::saved_rip(t), crate::sched::saved_user_rsp(t), crate::sched::saved_user_rbp(t))
}
