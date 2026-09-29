//! `talaria.suite=<LBA>` — running a LIST of modes from one image, across reboots.
//!
//! Why. On `*.metal` the price of one experiment is neither the build nor the
//! launch but the machine itself: the firmware takes minutes to come up, and the instance costs about
//! $6 an hour. Running one mode per image means paying for POST as many
//! times as we have tests. And there is no console there: the result of every run
//! has to be read from the volume anyway by a separate "stop — attach —
//! read" cycle.
//!
//! The design. The image (`tools/mk-tboot.sh SUITE=...`) gets two service
//! sectors and a log area:
//!   `slba`      the list of cmdline tails, one per line, ASCIIZ (4 sectors);
//!   `slba + 4`  the state: the magic `TALARIA-SUITE` + the number of the NEXT run;
//!   the log area — `SLOT` sectors per run (a header + the log text).
//! At boot the kernel reads the list, takes its item, appends it to the cmdline,
//! points the trace at its slot and increments the counter ON THE DISK. When the mode
//! has finished and is about to halt (`suite::park`), the kernel flushes the log and, if the
//! list still has items, reboots the machine. One instance launch covers the whole
//! list; one read of the volume gives all the logs.
//!
//! The counter lives on the disk because that is the only memory that survives
//! a reboot. If a run hung and the machine was killed from outside, the state on
//! the disk says at which item that happened.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Sectors per run: 1 header + 128 of log (64 KB = the buffer's capacity).
pub const SLOT: usize = 129;
/// How many sectors the list occupies.
pub const LIST_SECT: usize = 4;
const MAGIC: &[u8; 13] = b"TALARIA-SUITE";

static ARMED: AtomicBool = AtomicBool::new(false);
static MORE: AtomicBool = AtomicBool::new(false);
static IDX: AtomicUsize = AtomicUsize::new(0);
static mut LIST: [u8; LIST_SECT * 512] = [0; LIST_SECT * 512];
static mut TAIL: [u8; 256] = [0; 256];

/// Parse `talaria.suite=<LBA>` from the cmdline.
pub fn lba_from_cmdline(cmdline: &str) -> Option<usize> {
    for tok in cmdline.split_whitespace() {
        if let Some(v) = tok.strip_prefix("talaria.suite=") {
            if let Ok(n) = v.parse::<usize>() { return Some(n); }
        }
    }
    None
}

/// Read the list and the state, take our item and increment the counter on the disk.
/// Returns (the cmdline tail for this run, the run's number), or None if the
/// list is over or cannot be read. Requires the block layer to be up.
pub fn take(slba: usize) -> Option<(&'static str, usize)> {
    unsafe {
        let list = &mut *(&raw mut LIST);
        if !crate::fs::blk_read_raw(slba, list) { crate::println!("suite: the list at LBA {} cannot be read", slba); return None; }
        // the state
        let mut st = [0u8; 512];
        let idx = if crate::fs::blk_read_raw(slba + LIST_SECT, &mut st) && &st[..13] == MAGIC {
            u32::from_le_bytes([st[16], st[17], st[18], st[19]]) as usize
        } else { 0 };
        // find the idx-th line
        let mut start = 0usize;
        let mut n = 0usize;
        let mut found: Option<(usize, usize)> = None;
        let mut total = 0usize;
        for i in 0..list.len() {
            let c = list[i];
            if c == 0 { if i > start { total += 1; if n == idx { found = Some((start, i)); } } break; }
            if c == b'\n' {
                if i > start { if n == idx { found = Some((start, i)); } n += 1; total += 1; }
                start = i + 1;
            }
        }
        let (a, b) = match found { Some(v) => v, None => {
            crate::println!("suite: {} items, counter {} — the list is done", total, idx);
            return None;
        }};
        let tail = &mut *(&raw mut TAIL);
        let len = (b - a).min(tail.len() - 1);
        tail[..len].copy_from_slice(&list[a..a + len]);
        tail[len] = 0;
        // the counter goes to disk BEFORE the run: if the mode hangs, it will be visible from outside
        // at which item that happened, and a restart will continue from the next one.
        for c in st.iter_mut() { *c = 0; }
        st[..13].copy_from_slice(MAGIC);
        let next = (idx + 1) as u32;
        st[16..20].copy_from_slice(&next.to_le_bytes());
        st[20..24].copy_from_slice(&(total as u32).to_le_bytes());
        crate::fs::blk_write_raw(slba + LIST_SECT, &st);
        crate::fs::blk_flush();
        ARMED.store(true, Ordering::Relaxed);
        MORE.store(idx + 1 < total, Ordering::Relaxed);
        IDX.store(idx, Ordering::Relaxed);
        let s = core::str::from_utf8(&tail[..len]).unwrap_or("");
        crate::println!("suite: run {} of {}: {:?}", idx + 1, total, s);
        Some((s, idx))
    }
}

/// Whether the list has more items after the current one.
pub fn more() -> bool { ARMED.load(Ordering::Relaxed) && MORE.load(Ordering::Relaxed) }

/// The end of a mode: flush the trace and either reboot for the next item
/// or halt for good. It stands in place of `loop { hlt }` in every mode's branch —
/// without a list it behaves exactly like the old loop.
pub fn park() -> ! {
    crate::metrics::waiters_at_exit();
    crate::trace::flush();
    let shutdown = crate::acpi::shutdown_requested();
    // Before rebooting for the next item and before powering off — an FS flush:
    // otherwise everything the workload wrote without an fsync stays in the block cache.
    if shutdown { crate::acpi::shutdown_now("the workload exited"); } // the sync is inside
    if more() || ARMED.load(Ordering::Relaxed) || POWEROFF.load(Ordering::Relaxed) { crate::acpi::sync_fs(); }
    if more() {
        crate::println!("suite: the item is done — rebooting for the next one");
        crate::trace::flush();
        reboot();
    }
    // The list is over (or there was none, but we were asked to power off): we power the
    // machine off for real. On bare metal that is both a saving — an instance at $6 an hour
    // stops burning that very second — and an UNAMBIGUOUS signal from outside:
    // `stopped` means "it reached the end", and anything else means "it stalled".
    // The old `hlt` differed from a hang by nothing but the clock.
    if ARMED.load(Ordering::Relaxed) || POWEROFF.load(Ordering::Relaxed) {
        crate::println!("suite: the whole list is done — powering the machine off (ACPI S5{})",
            if crate::acpi::can_poweroff() { "" } else { ", FADT/_S5 not parsed — a blind attempt" });
        crate::trace::flush();
        crate::acpi::poweroff();
        // The machine does not go off instantly: the write to PM1a_CNT is handled by the chipset
        // (under QEMU by the monitor), and the CPU manages to execute a few more
        // instructions. Without this pause the log got a truncated "the power-off
        // did not work" line on a machine that was powering off right then.
        for _ in 0..200_000_000u64 { core::hint::spin_loop(); }
        crate::println!("suite: the power-off did not work — halting");
        crate::trace::flush();
    }
    crate::arch::cpu::halt_forever()
}

/// `talaria.poweroff=1` — power the machine off once the mode has finished (without a suite).
static POWEROFF: AtomicBool = AtomicBool::new(false);
pub fn set_poweroff(v: bool) { POWEROFF.store(v, Ordering::Relaxed); }

/// Reboot the machine. On bare metal that is also a SIGNAL: the instance console
/// shows the firmware banner again, and the number of banners tells whether the kernel got
/// to the needed point — the only channel that works BEFORE the disk comes up
/// (`talaria.rebootat=`).
pub fn reboot() -> ! {
    unsafe {
        // 0xCF9 = PCI reset control: 0x0E — a full reset. If the firmware does not
        // support it, we finish the job with a triple fault through an empty IDT.
        crate::arch::cpu::outb(0xCF9, 0x0E);
        let mut n = 0u64;
        while n < 100_000_000 { core::hint::spin_loop(); n += 1; }
        crate::arch::cpu::outb(0x64, 0xFE); // the keyboard controller
        n = 0;
        while n < 100_000_000 { core::hint::spin_loop(); n += 1; }
        crate::arch::cpu::triple_fault()
    }
}
