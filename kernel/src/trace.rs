//! `talaria.trace=<LBA>` — the early kernel log into memory and its flush ONTO THE DISK.
//!
//! Why. On `*.metal` the kernel has not a single
//! working output channel: raw COM1 does not reach the instance console
//! at all, and `get-console-output` there is a snapshot of the firmware's screen, into which
//! a direct write into the VGA text buffer did not get either. The only
//! probe that proved to work (`PROOF=<LBA>`) is a trace
//! on the disk itself: the volume survives the instance, and after a run a helper reads it.
//!
//! Hence the design: ALL the kernel's output from the first line accumulates in a static
//! buffer (that is cheap: an atomic index + a byte), and once the block layer is up,
//! the whole buffer is written as raw sectors at an absolute LBA. After that the flush
//! repeats at every phase (`mark`), so that after a hang the disk holds
//! the log up to exactly the last point passed.
//!
//! The layout on disk (the LBA comes from the cmdline):
//!   LBA + 0        the header: `TALARIA-TRACE len=<N> lost=<M> mark=<phase>`
//!   LBA + 1 ..     the log itself, as text, `CAP` bytes
//!
//! Collection is switched off (`stop`) if no trace was requested: the datapath must not pay
//! for diagnostics with a single instruction beyond the flag check.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The capacity of the early log buffer. 64 KB = 128 sectors: the whole kernel bring-up up to
/// entering a mode fits with room to spare (locally that is ~8 KB).
pub const CAP: usize = 64 * 1024;
const SECT: usize = 512;

static mut BUF: [u8; CAP] = [0; CAP];
static mut HDR: [u8; SECT] = [0; SECT];
static LEN: AtomicUsize = AtomicUsize::new(0);
static LOST: AtomicUsize = AtomicUsize::new(0);
static LBA: AtomicUsize = AtomicUsize::new(0);
static COLLECT: AtomicBool = AtomicBool::new(true);
static ARMED: AtomicBool = AtomicBool::new(false);
static FLUSHES: AtomicUsize = AtomicUsize::new(0);


/// Whether a trace on disk was requested.
#[inline]
pub fn armed() -> bool { ARMED.load(Ordering::Relaxed) }

/// One output byte into the buffer. Called from `serial::putb` — keep it cheap.
#[inline]
pub fn push(b: u8) {
    if !COLLECT.load(Ordering::Relaxed) { return; }
    let i = LEN.fetch_add(1, Ordering::Relaxed);
    if i < CAP { unsafe { *(&raw mut BUF).cast::<u8>().add(i) = b; } }
    else { LEN.store(CAP, Ordering::Relaxed); LOST.fetch_add(1, Ordering::Relaxed); }
}

/// Enable the trace: write the buffer to disk starting at sector `lba`.
pub fn arm(lba: usize) {
    LBA.store(lba, Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    COLLECT.store(true, Ordering::Relaxed);
}

/// Stop accumulating (no trace was requested) — output from now on bypasses the buffer.
pub fn stop() { if !armed() { COLLECT.store(false, Ordering::Relaxed); } }

/// How much must accumulate since the last flush to flush by itself (bytes).
/// 512 rather than 2048: on c6i.metal a run stopped inside a mode's branch, and the last
/// printed lines did not reach the disk — the place where it stopped had to be guessed.
/// The price is writing 64 KB for every half kilobyte of log (on EBS that is milliseconds and
/// only in the diagnostic mode).
const AUTO_BYTES: usize = 512;
/// `talaria.trace=<LBA>,eager` — flush after EVERY line. A threshold of 512
/// bytes still leaves the last few lines unknown, and on a machine
/// without a console it is exactly those that are needed: the place where it stopped has to be guessed within
/// half a kilobyte, i.e. by a run costing $2-3. The price of the eager mode is
/// writing 64 KB per line (on EBS that is single-digit milliseconds), so it is only
/// enabled on explicit request.
static EAGER: AtomicBool = AtomicBool::new(false);
pub fn set_eager(v: bool) { EAGER.store(v, Ordering::Relaxed); }
static LAST: AtomicUsize = AtomicUsize::new(0);
static IN_FLUSH: AtomicBool = AtomicBool::new(false);

/// Flush if a noticeable amount has accumulated since the last time. Called from `serial`
/// AFTER the output lock is released (otherwise the disk write would happen under it, and on
/// an error path NVMe prints by itself — which would be a re-entry into the lock).
/// The point: phase marks are rare, and the kernel may stop in the middle of a phase; this way the disk
/// ends up with a log accurate to a couple of kilobytes before the place where it stopped.
/// A12: the depth of "we are inside an interrupt handler". Flushing the trace writes to disk
/// through NVMe, and the driver is not reentrant: if a timer tick arrives while
/// the main path is already submitting a command, a flush FROM THE HANDLER will get into the same
/// queue and both will wait for each other. Under QEMU the window is microscopic (the disk is
/// fast); on `c6i.metal` every write is milliseconds, and the very first tick
/// hung the kernel DEAD: the pulse was printed, while control never reached the scheduler's
/// decision. What has accumulated will travel on the next print outside an interrupt.
static IRQ_DEPTH: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
pub fn irq_enter() { IRQ_DEPTH.fetch_add(1, Ordering::Relaxed); }
pub fn irq_leave() { IRQ_DEPTH.fetch_sub(1, Ordering::Relaxed); }

pub fn maybe_flush() {
    // From an interrupt handler we write to disk only if the block layer is FREE:
    // otherwise a second command on top of the first hangs both (the driver is not reentrant).
    // A blanket ban on flushing from an interrupt would blind exactly the path where it is
    // needed, so the ban is targeted.
    if IRQ_DEPTH.load(Ordering::Relaxed) != 0 && crate::fs::blk_busy() { return; }
    if !armed() { return; }
    let len = LEN.load(Ordering::Relaxed);
    let need = if EAGER.load(Ordering::Relaxed) { 1 } else { AUTO_BYTES };
    if len.saturating_sub(LAST.load(Ordering::Relaxed)) < need { return; }
    if IN_FLUSH.swap(true, Ordering::Acquire) { return; }
    LAST.store(len, Ordering::Relaxed);
    flush();
    IN_FLUSH.store(false, Ordering::Release);
}

/// Flush what has accumulated to disk. `false` — no trace was requested or there is no disk yet.
pub fn flush() -> bool {
    if !armed() { return false; }
    let lba = LBA.load(Ordering::Relaxed);
    let len = LEN.load(Ordering::Relaxed).min(CAP);
    let lost = LOST.load(Ordering::Relaxed);
    let n = FLUSHES.fetch_add(1, Ordering::Relaxed) + 1;
    unsafe {
        let hdr = &mut *(&raw mut HDR);
        for b in hdr.iter_mut() { *b = 0; }
        let mut w = Wr { buf: hdr, pos: 0 };
        use core::fmt::Write;
        let _ = writeln!(w, "TALARIA-TRACE len={} lost={} flushes={} cap={}", len, lost, n, CAP);
        if !crate::fs::blk_write_raw(lba, hdr) { return false; }
        // The buffer is written IN FULL (rather than up to len): the tail's length changes from flush
        // to flush, and a partial write would leave fragments of the previous one on disk.
        let buf = &*(&raw const BUF);
        crate::fs::blk_write_raw(lba + 1, buf) && { crate::fs::blk_flush(); true }
    }
}

/// Mark a boot phase: a line into the log + an immediate flush to disk.
/// On metal that is the only way to learn how far the kernel got.
pub fn mark(tag: &str) {
    if !armed() { return; }
    crate::println!("talaria-trace: phase {}", tag);
    LAST.store(LEN.load(Ordering::Relaxed), Ordering::Relaxed);
    let ok = flush();
    if !ok { crate::println!("talaria-trace: the flush to disk failed (is there no disk yet?)"); }
}

/// Parse `talaria.trace=<LBA>` from the cmdline.
pub fn lba_from_cmdline(cmdline: &str) -> Option<usize> {
    for tok in cmdline.split_whitespace() {
        if let Some(v) = tok.strip_prefix("talaria.trace=") {
            // `talaria.trace=<LBA>[,eager]` — an eager flush: after every line.
            let (num, eager) = match v.split_once(',') {
                Some((n, rest)) => (n, rest.contains("eager")),
                None => (v, false),
            };
            if let Ok(n) = num.parse::<usize>() { set_eager(eager); return Some(n); }
        }
    }
    None
}

struct Wr<'a> { buf: &'a mut [u8], pos: usize }
impl core::fmt::Write for Wr<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if self.pos < self.buf.len() { self.buf[self.pos] = b; self.pos += 1; }
        }
        Ok(())
    }
}
