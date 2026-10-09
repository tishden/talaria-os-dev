//! ext2 with WRITING: a block of 1K-4K, direct + indirect (up to triple) blocks, the bitmaps
//! of the blocks/inodes, linear directories AND htree ones (`dir_index`, the `dx_*` half of
//! this file), a jbd2-compatible journal in ordered mode (the `jrn_*` half, §8.9-§8.14).
//! An image without either feature is mounted just as well — both are read out of the
//! superblock; the regression builds images both ways. The block cache is set-associative
//! with a write-back and a flush by age, by the balance and by `fsync`. The paths are
//! absolute, normalised by the caller (proc::resolve). The time is the kernel's wall clock.
//!
//! # The `unsafe` of this module
//!
//! This is the largest `unsafe` surface in the kernel, and it is of one kind: the whole
//! state of the file system — the superblock's fields, the cache's tables, the counters of
//! the dirty ones, the table of open files, the journal — lies in module `static mut`, and
//! the handles the code passes around are RAW POINTERS INTO THE CACHE rather than borrows.
//! What is at stake is not a crash but a corrupted file system on the disk: a block written
//! into after its slot has been handed to another block, metadata evicted in place before
//! their transaction committed, a counter that disagrees with the tables. The `unsafe` rests
//! on seven invariants, named E1..E7 at the sites.
//!
//! - **E1. Every entry into ext2 holds the FS lock** — `fs_lock()`, including the READING
//!   paths (the rule and what breaking it cost are written out at `fs_lock`). Therefore
//!   every `static mut` of this module is touched by exactly one task at a time, and every
//!   private `unsafe fn` here carries at least this condition: it is callable only from
//!   under the lock. Where a function needs more than E1, it says so at itself.
//! - **E2. A pointer into the cache lives only while its slot cannot be replaced.**
//!   `cblk`/`cblk_new`/`blk` hand out an address inside `CACHE_BASE`; the slot stays that
//!   block's until `cblk_mode` re-tags it, and re-tagging can only hit a slot that `victim`
//!   was allowed to choose — that is, one that is neither pinned (`CPIN`) nor in flight
//!   (`CWB`). So a pointer must not be held across a `cblk` for ANOTHER block of the same
//!   set unless the slot is pinned; and two `&mut` from `blk()` for the same block must
//!   never be alive at once — that is the ordinary aliasing rule, and here it is the reason
//!   the callers pass `&mut [u8; 128]` inode buffers around instead of re-reading.
//!   The one LONG-LIVED pointer into the cache is a file mapping: `mmap_prepare` pins the
//!   slots (`CPIN`) and hands their identity addresses to the process's page tables, so
//!   those slots must never be re-tagged for as long as the mapping lives — which is
//!   precisely what pinning buys, since `victim` never chooses a pinned way.
//!   In practice E2 shows up as ONE RULE that every one of the sixty-odd `blk()` uses in
//!   this file obeys: a reference from `blk()` is finished with BEFORE the next access to
//!   the cache. The bitmap-and-group-descriptor pairs (`free_block`, `alloc_inode`,
//!   `free_inode`) are the places where that is easiest to break — they touch two blocks a
//!   few lines apart — and they are written so that the first reference's last use comes
//!   before the second `blk()`. Nothing enforces this rule for us: `blk` returns a borrow
//!   with a lifetime the CALLER chooses, so the compiler will not object. It is checked by
//!   reading, and that is why the rule is written here rather than left to be rediscovered.
//! - **E3. The counters of the dirty ones have a single point of change.** Any change of
//!   `CTAG`/`CDIRTY`/`COWN`/`CJRN` is wrapped in `dcnt_del(slot) … dcnt_add(slot)`, and
//!   `DN_OLD` is a LOWER estimate of the oldest dirty datum (it may be older than the truth,
//!   never newer). Both are recomputed by a full walk in the gates `dcnt` and `dirtysmear`.
//! - **E4. Releasing the lock in the middle is explicit and forbidden by default inside a
//!   replacement.** The waits give the lock up only where `MayRelease` permits it; a slot's
//!   replacement and a transaction's body switch that off, because handing the lock over
//!   there would give a neighbour a slot or a journal window we have already taken. The
//!   permission itself (`FS_MAY_REL`) and the lock's own state (`FS_DEPTH`) are the two
//!   `static mut` that E1 CANNOT cover, since they are what the lock is made of:
//!   `FS_MAY_REL` is indexed by task and every task writes only its own cell, while
//!   `FS_DEPTH` belongs to whoever currently owns `FS_OWNER` — a non-owner only ever reads
//!   it together with `FS_OWNER`, and a stale read there yields "not mine", which is the
//!   answer that costs a wait rather than a corruption.
//! - **E5. Memory handed to the device is not touched until the request completes** — and
//!   the two flush paths hand it different memory, which is the whole reason they differ:
//!   * SYNCHRONOUS (an eviction in `writeback_slot`, `writeback_locked`): the cache slot
//!     ITSELF goes to the device, and the FS lock is held across the whole request. Nobody
//!     can re-tag or rewrite the slot while the device reads out of it, because everyone
//!     who could is waiting for the lock;
//!   * ASYNCHRONOUS (`writeback_unlocked`, §8.6/§8.13): the block is COPIED into the
//!     staging buffer (`WB_BUF`, or `DEFER_BUF` for a deferred last portion) under the lock,
//!     and only the copy is given to the device — after which the lock is released for the
//!     time of the disk. The slot is marked `CWB` for that whole time so that `victim` and
//!     the other flushes step over it; a neighbour is nevertheless allowed to DIRTY it again
//!     while it flies, and that is not a loss — the pass reports "something was left"
//!     (`left`) and the next one writes the new version.
//!   `CWB` is cleared with the FS lock RELEASED, which is why no counter of the dirty ones
//!   is kept by it (a counter would be a race) and why it is read and written volatile (E7).
//! - **E6. Dirty metadata are not written in place before their transaction commits.**
//!   An eviction of uncommitted metadata is a transaction boundary (§8.4): commit first,
//!   replace the slot after. A block already in the journal (`CJRN`) may go to its place at
//!   any moment (§8.8) — a checkpoint simply will not meet it any more.
//! - **E7. What a neighbour may change under us is read volatile.** `CWB`, `CTAG` on the
//!   replacement path and the journal's stage (`JBUSY_STAGE`) are read and written through
//!   `read_volatile`/`write_volatile`: they are looked at by cores that do not hold the FS
//!   lock (the completion of an I/O, the diagnostics of a stuck journal).
//!
//! The crate switches `unsafe_op_in_unsafe_fn` off (`main.rs`) and modules switch it back
//! on as they are documented. It stays OFF for this one, for the same reason as in
//! `sched.rs` and not as a shortcut: the contract of almost every `unsafe fn` here is E1 —
//! "the caller holds the FS lock" — which is a property of the WHOLE body, not of a
//! particular operation in it. Wrapping a hundred functions in one `unsafe {}` each would
//! indent thousands of lines to repeat what the signature and the passport already say,
//! and it would hide the places that are genuinely about an ORDER (the eviction's
//! commit-then-replace, the transaction's copies-then-commit) among the rest. Those are
//! called out at their sites instead. `sync.rs`, where the operations are few and their
//! order IS the contract, has the lint on.
use core::ptr::addr_of_mut;

const BSZ: usize = 4096;          // the maximum supported block size
/// THE CEILING of the number of the cache's slots (the memory for them is taken at the mount, not in the image).
/// It is exactly a ceiling rather than the size: how much to take is decided by `cache_pick` —
/// a sixteenth of the free memory. It was 16384 (64 MB), and on a machine with
/// gigabytes it cut the share down to itself: with ClickHouse with 3 GB the cache came out exactly
/// 64 MB instead of 192, and a MergeTree `TRUNCATE` took 1.45 s against Linux's 0.21.
/// The ceiling's price is the static tables of the slots, 16 bytes per slot: 2 MB of .bss.
///
/// THE CEILING RAN INTO A MEASUREMENT. Session 28 chose the cache's size on mongodb and
/// stopped exactly at 512 MB — that is, AT THE VERY CEILING (`fscache=512` in
/// the manifest) rather than at an optimum: there was nothing to ask for more with. Meanwhile the A/B
/// 180 -> 512 MB took `insert`'s p99.9 from 10404 down to 417 us, that is, the curve was still
/// going down. It has been raised to 1 GB (+3 MB of .bss) so that the next measurement could ask
/// the question "and what next?"; the size itself is still chosen by `cache_pick`.
const NC: usize = 262144;         // up to 1 GB
const MAXOPEN: usize = 1024;
pub const FD_BASE: i64 = 300;     // the files' handle range: 300..300+MAXOPEN
const ROOT: u32 = 2;

const EPERM: i64 = -1; const ENOENT: i64 = -2; const EIO: i64 = -5; const EBADF: i64 = -9;
const EEXIST: i64 = -17; const ENOTDIR: i64 = -20; const EISDIR: i64 = -21; const EINVAL: i64 = -22;
const EROFS: i64 = -30; const EMFILE: i64 = -24; const ENOSPC: i64 = -28; const ENOTEMPTY: i64 = -39; const ENAMETOOLONG: i64 = -36;
const ELOOP: i64 = -40;

// ── the superblock / the groups ──────────────────────────────────────────────────
static mut ACTIVE: bool = false;
static mut BS: usize = 4096;          // the block's size
static mut SPB: usize = 8;            // the sectors per block
static mut INODES_PER_GROUP: u32 = 0;
static mut BLOCKS_PER_GROUP: u32 = 0;
static mut INODE_SIZE: usize = 128;
static mut FIRST_DATA: u32 = 0;
static mut NGROUPS: usize = 0;
static mut GDT_BLOCK: u32 = 1;        // the first block of the group descriptor table
static mut TOTAL_BLOCKS: u32 = 0;
static mut SB: [u8; 1024] = [0; 1024];

// ── the block cache: direct-mapped, WRITE-BACK (the dirty blocks go to the disk at an eviction,
//    at an fsync/fdatasync/sync, exit_group and talaria.stop), the memory comes from the allocator at the mount ──
static mut CACHE_BASE: u64 = 0;       // the identity address of the cache's area (NC * BSZ)
static mut CTAG: [u32; NC] = [0; NC]; // the block's number + 1 (0 = empty)
static mut CDIRTY: [bool; NC] = [false; NC];
/// THE SLOT HOLDS A FRESHLY ALLOCATED DATA BLOCK not yet written: its allocation (the block map,
/// the bitmap, the size) is metadata that the next transaction will journal, whoever's fsync
/// commits it. Before any metadata transaction such blocks go to the disk — ext4's `ordered`:
/// otherwise a power cut after the commit leaves a file whose map points at blocks that were
/// never written. MariaDB's redo log rebuilt at its first start was exactly that on EC2 (both
/// machines): `ib_logfile0`'s new inode was committed by another file's fsync while its 32 MiB
/// stayed in the cache — the kernel has no background flush — and the hard stop read zeros
/// ("a redo log created before MariaDB 10.2.2", ARM64-PLAN §11 item 1). An overwrite of an
/// existing block is not marked: its old content is still a consistent file.
static mut CNEW: [bool; NC] = [false; NC];
// §8.3: the owner of a dirty slot is the inode whose DATA lie in it (0 = the metadata:
// the inodes, the bitmaps, the GDT, the indirect blocks). fsync(fd) writes only the slots
// of its own inode + the metadata rather than the whole cache.
static mut COWN: [u32; NC] = [0; NC];
// §8.4: the slot has already been written into the current epoch's journal and has not changed since — there is no need
// to put it into the next transaction; a checkpoint writes in place ONLY
// such (committed) blocks, the unjournalled ones stay dirty.
static mut CJRN: [bool; NC] = [false; NC];
/// §8.9: the block was in this epoch's journal and was CHANGED after the commit (CJRN is removed).
/// Its committed version lives only in the journal, and a checkpoint must put
/// IT in place (from the log block CJPOS) rather than skip the block: otherwise
/// after the journal is flushed the committed content is left nowhere.
static mut CJOLD: [bool; NC] = [false; NC];
static mut CJPOS: [u32; NC] = [0; NC];     // the log block of the last copy; bit 31 — the copy is escaped
// §8.5: the slot is occupied by a file MAP_SHARED mapping in a user AS — it must not be
// evicted (the user looks straight into this page of the cache).
static mut CPIN: [u16; NC] = [0; NC];
// §8.6: the slot is IN FLIGHT — its copy has been given to the device, the write has not completed yet, while
// the FS lock is RELEASED for that time. Such a slot must not be evicted: a read (§8.1
// goes without the device's lock) would return the OLD block from the disk while the new one is still in
// the queue. It is set under the FS lock, it is removed by the writer BEFORE the device's lock is
// released — the one that ran into such a slot waits for the device and takes it anew.
static mut CWB: [bool; NC] = [false; NC];
// §2.1: the cache is SET-ASSOCIATIVE (WAYS ways, an LRU inside a set).
// It was direct-mapped: the inode table and a file's data, having got into one slot,
// knocked each other out at every access, while a mapping's pin (§8.5) forbade
// caching every NCH-th block — a crutch of 64 "evicted" slots was kept for that.
// caching every NCH-th block — a crutch of 64 "evicted" slots was kept for that.
//
// The layout is PER WAY (slot = way * NSETS + bn % NSETS) rather than per set.
// That matters for the §8.2 coalescing: blocks that are neighbours by number and got into one
// way stay neighbours IN MEMORY too, so the writeback still glues
// them into one request to the device. With a per-set layout (set*WAYS + way)
// the neighbouring blocks would move apart by WAYS pages and the gluing would disappear.
const WAYS: usize = 4;
/// THE EFFECTIVE number of slots and sets (it is chosen at the mount).
///
/// It was the constant 2048 (8 MB) on any machine. For a file that fits into the cache
/// that is unnoticeable, while for a table of fifty megabytes it is 7 % of misses,
/// and every miss holds THE SINGLE FS lock through an access to the device.
/// Linux under the same load gives the whole free memory to the page cache; we
/// take its share, because a cell usually has a lot of memory, while "how much of it" the kernel
/// learns only at runtime.
static CACHE_N: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static CACHE_SETS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
#[inline] fn ncache() -> usize { CACHE_N.load(core::sync::atomic::Ordering::Relaxed) }
#[inline] fn nsets() -> usize { CACHE_SETS.load(core::sync::atomic::Ordering::Relaxed) }
/// `talaria.fscache=<MB>` — the size of the block cache (0 — choose it by the memory).
pub static FSCACHE_MB: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
pub fn fscache_from_cmdline(cmdline: &str) {
    if let Some(mb) = crate::cmdline::num(cmdline, "talaria.fscache") {
        FSCACHE_MB.store(mb as usize, core::sync::atomic::Ordering::SeqCst);
    }
}
/// How many slots to take: the knob, otherwise a sixteenth of the free memory,
/// clamped into [512 slots (2 MB), NC]. A share rather than "everything free": the FS cache must
/// not take memory away from the workload itself — it is the cell's one and main thing.
/// A multiple of WAYS, otherwise the layout by ways will go off.
fn cache_pick() -> usize {
    let mb = FSCACHE_MB.load(core::sync::atomic::Ordering::Relaxed);
    // The share is of ALL the free memory, but the cache itself comes from the low allocator
    // (`alloc_contig`), which the kernel's own structures live on: not more than half of it.
    let want = if mb != 0 { mb * (1 << 20) / BSZ } else {
        ((crate::mem::free_total_frames() / 16).min(crate::mem::free_low_frames() / 2)) as usize };
    (want.clamp(512, NC) / WAYS) * WAYS
}
// The cycle of the last access to a slot — an LRU inside a set.
static mut CUSE: [u32; NC] = [0; NC];
static mut CLOCK: u32 = 0;
/// SAFETY: E1 — under the FS lock; `slot` is an index the caller got from `find_slot`
/// or `victim`, that is, already `< ncache()`.
#[inline] unsafe fn touch(slot: usize) { CLOCK = CLOCK.wrapping_add(1); CUSE[slot] = CLOCK; }
#[inline] fn set_of(bn: u32) -> usize { (bn as usize) % nsets() }
#[inline] fn way_slot(way: usize, set: usize) -> usize { way * nsets() + set }
static mut DIRTY_N: usize = 0;        // how many slots are dirty (0 -> a flush is not needed at all)
// SESSION 39: THE DIRTY ONES ARE COUNTED BY A COUNTER RATHER THAN BY A WALK OVER THE WHOLE CACHE.
//
// The instrument of the `fsync` phases (docs/MEASUREMENTS.md §3, mongodb/insert) showed 320 us per
// call — 10 % of the price — of PURE processor on the walks over the whole cache (131072
// slots with fscache=512): `jrn_pending` twice (165 us) and the second pass of
// `writeback_locked` (156 us), which usually has nothing to write. Neither is about
// the disk or about durability, while the numbers were known in advance.
//
// So the three values the cache was asked about are kept by counters:
//   DN_DATA — the dirty slots with a file's DATA (COWN != 0); the dirty metadata,
//             accordingly, are DIRTY_N - DN_DATA;
//   JP_META/JP_DATA — the dirty ones that have NOT got into this epoch's journal yet
//             (`!CJRN`), by the selection metadata/data: that is exactly what
//             `jrn_pending` counted by a walk.
//
// A SINGLE POINT OF CHANGE. The counters depend on four arrays at once
// (CTAG/CDIRTY/COWN/CJRN), and keeping them "in place" would mean repeating the same
// arithmetic twenty times — a class of errors that has already cost sessions
// in this store. Any change of these arrays is wrapped in a pair of
// `dcnt_del(slot) ... dcnt_add(slot)`: take the slot off the counters by its OLD
// state, change anything, put it back by the NEW one. A missed
// wrapper is caught by the gate `dcnt` (`dcnt_audit` recomputes by a full walk).
//
// All four counters are changed ONLY under the FS lock — as DIRTY_N has been so far.
// `CWB` (a slot in flight) is deliberately not included in them: it is removed WITH THE FS
// LOCK RELEASED, and a counter by it would be a race.
static mut DN_DATA: usize = 0;        // of the dirty ones — the files' data (COWN != 0)
static mut JP_META: usize = 0;        // the dirty metadata not in the journal yet
static mut JP_DATA: usize = 0;        // the dirty data not in the journal yet
// STEP 1 OF SESSION 41: A CLOCK INSTEAD OF A WALK — THE OLDEST DIRTY ONE IS KNOWN FOR FREE.
//
// The flush by age (session 40) asked "is there anything that has lain long enough?" BY A WALK OVER
// THE WHOLE CACHE. While a kick happened once per the threshold itself (300 ms), that was unnoticeable;
// the smearing of the price (a kick eight times as often) would multiply the walk by eight too —
// 165 us of pure processor (session 38's instrument) once every 12 ms, right in somebody else's
// `write`. Exactly that class for which session 39 introduced the counters of the dirty ones.
//
// `DN_OLD` is A LOWER ESTIMATE of the oldest dstamp among the dirty DATA. The point is
// exactly in the direction of the error: the estimate may be OLDER than the truth (an extra walk,
// a price) but never newer (a missed block that has lain long enough, a lie). It is kept in
// the same single point of change as the counters, and is verified by the gate
// `dirtysmear` with a full recomputation (`dn_old_audit`).
static mut DN_OLD: u32 = 0;
static mut DN_OLD_SET: bool = false;
/// A slot with data became dirty at the time `t` — move the estimate if it is older.
///
/// SAFETY: E1 + E3 — under the FS lock, and only from `dcnt_add`, which is the single
/// point of change; moving the estimate only downwards is what keeps it a LOWER one.
#[inline] unsafe fn dn_old_note(t: u32) {
    if !DN_OLD_SET { DN_OLD = t; DN_OLD_SET = true; }
    else if (t.wrapping_sub(DN_OLD) as i32) < 0 { DN_OLD = t; }
}
/// Take the slot off the counters of the dirty ones by its CURRENT state.
///
/// SAFETY: E1 + E3 — under the FS lock. The pair `dcnt_del … dcnt_add` must bracket
/// EVERY change of `CTAG`/`CDIRTY`/`COWN`/`CJRN` for this slot: the subtraction reads the
/// tables as they are NOW, so calling it after the change would subtract by the new state
/// and leave the counters wrong. The gate `dcnt` recomputes them by a full walk.
#[inline] unsafe fn dcnt_del(s: usize) {
    if CTAG[s] == 0 || !CDIRTY[s] { return; }
    DIRTY_N -= 1;
    if COWN[s] != 0 { DN_DATA -= 1; if DN_DATA == 0 { DN_OLD_SET = false; } }
    if !CJRN[s] { if COWN[s] == 0 { JP_META -= 1; } else { JP_DATA -= 1; } }
}
/// Put the slot onto the counters of the dirty ones by its NEW state.
///
/// SAFETY: E1 + E3 — under the FS lock, as the closing half of the pair opened by
/// `dcnt_del`.
#[inline] unsafe fn dcnt_add(s: usize) {
    if CTAG[s] == 0 || !CDIRTY[s] { return; }
    DIRTY_N += 1;
    if COWN[s] != 0 { DN_DATA += 1; dn_old_note(CDIRTIME[s]); }
    if !CJRN[s] { if COWN[s] == 0 { JP_META += 1; } else { JP_DATA += 1; } }
}
/// The cache has been flushed entirely (a mount, a re-handout, a replay of the journal).
///
/// SAFETY: E1 + E3 — under the FS lock, and only where the TABLES have been reset too
/// (all `CTAG` zeroed): zeroing the counters while a dirty slot survives would lose that
/// block's flush.
#[inline] unsafe fn dcnt_reset() { DIRTY_N = 0; DN_DATA = 0; JP_META = 0; JP_DATA = 0; DN_OLD_SET = false; }
/// For the gate `dirtysmear`: (the estimate, the recomputation by a full walk, "the estimate is not newer
/// than the truth"). The third one is the invariant: an estimate newer than the truth would mean that
/// a block that has lain long enough will be noticed by NOT A SINGLE kick.
pub fn dn_old_audit() -> (u32, u32, bool) {
    let _fsg = fs_lock();
    // SAFETY: E1 — the lock is taken right here, and the walk only READS the tables.
    unsafe {
        let (mut set, mut min) = (false, 0u32);
        for s in 0..ncache() {
            if CTAG[s] == 0 || !CDIRTY[s] || COWN[s] == 0 { continue; }
            if !set { min = CDIRTIME[s]; set = true; }
            else if (CDIRTIME[s].wrapping_sub(min) as i32) < 0 { min = CDIRTIME[s]; }
        }
        let ok = !set || (DN_OLD_SET && (DN_OLD.wrapping_sub(min) as i32) <= 0);
        (if DN_OLD_SET { DN_OLD } else { 0 }, min, ok)
    }
}
/// For the gate `dcnt`: recompute everything by a full walk and return
/// (the counter, the recomputation) for each of the four values.
pub fn dcnt_audit() -> [(usize, usize); 4] {
    let _fsg = fs_lock();
    // SAFETY: E1 — the lock is taken right here, and the walk only READS the tables.
    unsafe {
        let (mut d, mut dd, mut jm, mut jd) = (0usize, 0usize, 0usize, 0usize);
        for s in 0..ncache() {
            if CTAG[s] == 0 || !CDIRTY[s] { continue; }
            d += 1;
            if COWN[s] != 0 { dd += 1; }
            if !CJRN[s] { if COWN[s] == 0 { jm += 1; } else { jd += 1; } }
        }
        [(DIRTY_N, d), (DN_DATA, dd), (JP_META, jm), (JP_DATA, jd)]
    }
}
pub const DCNT_NAMES: [&str; 4] = ["dirty", "of them data", "waiting for the journal: metadata", "waiting for the journal: data"];
/// `talaria.fscnt=0` — RETURN THE FORMER BEHAVIOUR (the walks over the whole cache).
///
/// The knob exists exactly so that the 10 % of the call's price are VISIBLE IN THE INSTRUMENT ON
/// ONE RUN of the stand rather than deduced by a comparison with the previous session: between
/// the runs not even the median reproduces (the rule of sessions 29 and 37), and "it became
/// faster" without an A/B on one stand is not a measurement. The counters with `fscnt=0`
/// are still kept (they are cheap and the gate `dcnt` verifies them), but they are NOT
/// used: `jrn_pending` walks the cache again, the second pass is called by
/// the sign "there is at least something dirty in the cache".
pub static FSCNT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
#[inline] fn fscnt() -> bool { FSCNT.load(core::sync::atomic::Ordering::Relaxed) }
pub fn fscnt_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::num(cmdline, "talaria.fscnt") {
        FSCNT.store(v != 0, core::sync::atomic::Ordering::SeqCst);
        crate::println!("ext2: the dirty ones {} (talaria.fscnt={})",
            if v != 0 { "are counted by a counter" } else { "ARE WALKED over the whole cache — as before session 39" }, v);
    }
}
// STEP 2 OF SESSION 39: THE INSTRUMENT BEFORE THE KNOB — HOW MUCH DIRT PER CALL AND OF WHAT AGE.
//
// The shape of class B's loss speaks for itself: the median and the p99 are won on the whole
// ladder, the `p99.9` is lost on the whole one. The explanation is at hand — "with Linux the background
// writeback carries the dirty data away IN ADVANCE, while with us the data travel INSIDE the call" — and from
// it a knob follows directly: a pre-emptive flush not by the dirty threshold (§8.7,
// a quarter of the cache) but by AGE, like `dirty_expire_centisecs`.
//
// Exactly here sessions 37 and 38 twice in a row bought the rule "the instrument before the change":
// the group commit and the async commit both died of a measurement that cost one phase
// of the stand. This knob's ceiling is measurable in advance too, and it is in two numbers:
//   * how many blocks of DATA there are per `fsync` call and how many blocks
//     the file has in total (the difference is those that arrived already clean: if they are almost all,
//     there is nothing to pre-empt);
//   * the AGE of a dirty block by the moment it is given to the device. If it is
//     single milliseconds, then Linux's background writeback (a kick once every 5 s, a term of
//     30 s) would NOT HAVE MANAGED to carry that block away either — and the explanation is wrong, while the knob
//     does not exist, however much it is tuned.
//
// The age is in units of 2^20 cycles (~0.3 ms at 3.5 GHz): there are no divisions on the hot
// path, a u32 is enough for two weeks of continuous dirt.
static mut CDIRTIME: [u32; NC] = [0; NC];
#[inline] fn dstamp_of(t: u64) -> u32 { (t >> 20) as u32 }
#[inline] fn dstamp() -> u32 { dstamp_of(crate::tsc::rdtsc()) }
// SESSION 41: THE AGE IS COMPUTED AS A SIGNED DIFFERENCE, AND THAT IS NOT PEDANTRY.
//
// The selection "older than the threshold" stood as `now.wrapping_sub(CDIRTIME[s]) >= thr` on
// a u32. A block's stamp may turn out to be NEWER than the snapshot `now`: the snapshot is taken at the entry
// into a pass, while the pass releases the FS lock for the time of the device (§8.6) — during that
// time a neighbour dirties blocks with A FRESH stamp. An unsigned difference turned
// "minus 0.4 ms" into 4 294 967 295 units, that is:
//   * the freshest block looked INFINITELY OLD and left by a pre-emptive flush —
//     exactly what the knob must not do (that is "the one who dirties pays",
//     but already without any benefit);
//   * the instrument wrote an age of 1 553 501 078 413 us (18 days) with a run of
//     five minutes — session 41's stand, the phase `talaria.dirtyage=10`.
// The real ages are three orders of magnitude smaller than 2^31 units (8.8 days), so
// a signed reading of the difference is correct even at an overflow of the counter.
#[inline] fn age_of(now: u32, stamp: u32) -> u32 {
    let d = now.wrapping_sub(stamp) as i32;
    if d < 0 { 0 } else { d as u32 }
}
#[inline] fn aged(now: u32, stamp: u32, thr: u32) -> bool {
    (now.wrapping_sub(stamp) as i32) >= thr as i32
}
/// The gate `agewrap`: the same arithmetic as on the hot path — checkable.
pub fn age_arith_check() -> bool {
    age_of(100, 40) == 60                       // an ordinary age
        && age_of(40, 100) == 0                 // the stamp is NEWER than the snapshot — not "18 days"
        && age_of(5, u32::MAX - 4) == 10        // an overflow of the counter
        && aged(100, 40, 60) && !aged(100, 40, 61)
        && !aged(40, 100, 1)                    // a fresh block has NOT lain long enough
        && aged(5, u32::MAX - 4, 10) && !aged(5, u32::MAX - 4, 11)
}
// STEP 1 OF SESSION 40: THE AGE BY POPULATION rather than as one number.
//
// Session 39's instrument answered "the average age of the dirty data by the moment of the flush is 2.7-3.2 s",
// and the whole knob "a pre-emptive flush by age" stands on that number. But the age is
// averaged over DIFFERENT populations at once: blocks are carried away by `fsync` itself, blocks are carried away by
// the §8.7 threshold from `write`, blocks are carried away by a slot's eviction and by sync/checkpoint.
// Session 39's arithmetic says that everything except the first is small (98.6 % of the blocks
// are attributed to the `fsync` calls), but that is AN INFERENCE rather than a measurement: if the seconds of the age
// turn out to be a property of a foreign population, then the `fsync` population has nothing to pre-empt —
// and the knob does not exist, however much it is tuned.
//
// The reason for a flush is passed AS A PARAMETER rather than by a global "current mode":
// the portions of neighbouring tasks go simultaneously (a flush by the threshold from a `write` falls
// exactly into the window of a foreign transaction — §8.13, a separate buffer is introduced there exactly for that),
// and a global variable would attribute them to each other.
#[derive(Clone, Copy, PartialEq)]
pub enum WbC { Fsync = 0, Balance = 1, Other = 2, Evict = 3, Age = 4 }
pub const WBC_N: usize = 5;
pub const WBC_NAMES: [&str; WBC_N] = ["fsync(fd)", "the write threshold", "other (sync, checkpoint, msync)", "a slot's eviction", "the age (talaria.dirtyage)"];
/// The age of the flushed blocks of DATA by population (portions, blocks, the sum,
/// the worst — in units of dstamp).
pub static WBAGE_N: [core::sync::atomic::AtomicU64; WBC_N] = [const { core::sync::atomic::AtomicU64::new(0) }; WBC_N];
pub static WBAGE_BLK: [core::sync::atomic::AtomicU64; WBC_N] = [const { core::sync::atomic::AtomicU64::new(0) }; WBC_N];
pub static WBAGE_SUM: [core::sync::atomic::AtomicU64; WBC_N] = [const { core::sync::atomic::AtomicU64::new(0) }; WBC_N];
pub static WBAGE_MAX: [core::sync::atomic::AtomicU64; WBC_N] = [const { core::sync::atomic::AtomicU64::new(0) }; WBC_N];
/// A portion of the ages of one flush — into the counters of its own population.
#[inline] fn wbage_note(c: WbC, k: usize, age_sum: u64, age_max: u32) {
    use core::sync::atomic::Ordering::Relaxed;
    if k == 0 { return; }
    let i = c as usize;
    WBAGE_N[i].fetch_add(1, Relaxed); WBAGE_BLK[i].fetch_add(k as u64, Relaxed);
    WBAGE_SUM[i].fetch_add(age_sum, Relaxed); WBAGE_MAX[i].fetch_max(age_max as u64, Relaxed);
}
/// The data per `fsync(fd)` call: the calls, the dirty blocks, the file's blocks.
pub static FSD_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static FSD_DIRTY: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static FSD_FILE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The file's size in blocks is put by `fsync` BEFORE the entry into the transaction (reading an inode under
/// JBUSY is not allowed: a cache miss there would mean an eviction inside the transaction).
static mut FSD_FBLK: [u32; crate::sched::NTASKS] = [0; crate::sched::NTASKS];
/// (portions, blocks, the sum of the ages in us, the worst one in us) OF ONE population —
/// for `stat:` and for the gate.
pub fn wbage_stats_of(c: usize) -> (u64, u64, u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    let hz = crate::tsc::hz().max(1);
    let to_us = |v: u64| v.saturating_mul(1 << 20) / (hz / 1_000_000).max(1);
    let i = c.min(WBC_N - 1);
    (WBAGE_N[i].load(Relaxed), WBAGE_BLK[i].load(Relaxed), to_us(WBAGE_SUM[i].load(Relaxed)), to_us(WBAGE_MAX[i].load(Relaxed)))
}
/// The same over ALL the populations at once — as session 39's instrument measured (for a check:
/// the sum of the populations must coincide with the former single number).
pub fn wbage_stats() -> (u64, u64, u64, u64) {
    let (mut n, mut b, mut s, mut m) = (0u64, 0u64, 0u64, 0u64);
    for i in 0..WBC_N {
        let (a, c, d, e) = wbage_stats_of(i);
        n += a; b += c; s += d; if e > m { m = e; }
    }
    (n, b, s, m)
}
/// (the fsync calls with data, the dirty blocks, the files' blocks) — for `stat:`.
pub fn fsd_stats() -> (u64, u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (FSD_CALLS.load(Relaxed), FSD_DIRTY.load(Relaxed), FSD_FILE.load(Relaxed))
}
static mut STATS_RD: u64 = 0; static mut STATS_WR: u64 = 0;
// The hits/misses of the block cache. Without them "the file system is slow" and "the cache
// is small for this workload" are indistinguishable, while they are cured differently: Linux under the same
// load keeps EVERYTHING it has memory for in a page, while with us the cache's
// size is a build constant.
static mut STATS_HIT: u64 = 0; static mut STATS_MISS: u64 = 0;
/// (the hits, the misses) of the block cache — for `stat:`.
///
/// SAFETY: E1 IS DELIBERATELY NOT TAKEN HERE, and this is the one exception the module
/// makes, shared by every accessor below. The readers are the printers of `stat:`, `top`,
/// `kstat` and `metrics`; making them take the FS lock would stall the file system for a
/// line of diagnostics, and would hang the printer exactly when the numbers are wanted
/// most — while the FS is stuck holding the lock. The counters are aligned `u64`/`usize`
/// written under the lock, so a reader without it sees a value that is a few increments
/// stale, never a torn one (x86-64 gives aligned loads and stores of this width
/// atomically). Nothing here decides anything — the values are printed.
pub fn cache_stats() -> (u64, u64) { unsafe { (STATS_HIT, STATS_MISS) } }
/// The size of the block cache in megabytes (for `stat:`).
pub fn cache_mb() -> usize { (ncache() * BSZ) >> 20 }
static mut STATS_FLUSH: u64 = 0;      // how many times we reached the device (a Flush)
static mut STATS_RUNS: u64 = 0;       // how many contiguous ranges were written (the coalescing)
static mut STATS_ARUN: u64 = 0;       // §2.4: the accesses to the bitmap for an allocation

fn rd32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3]]) }
fn rd16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o+1]]) }
fn wr32(b: &mut [u8], o: usize, v: u32) { b[o..o+4].copy_from_slice(&v.to_le_bytes()); }
fn wr16(b: &mut [u8], o: usize, v: u16) { b[o..o+2].copy_from_slice(&v.to_le_bytes()); }
/// SAFETY: `BS` is written ONCE, on the mount path, before the FS is usable by anyone
/// (`mount` sets it from the superblock's geometry and only then lets the volume be taken);
/// afterwards every reader sees a constant. This is why `bs()` itself is a safe fn.
fn bs() -> usize { unsafe { BS } }
fn now() -> u32 { (crate::tsc::wall_ns() / 1_000_000_000) as u32 }

/// The slot in which the block bn LIES (if it lies): a search over the set's ways.
///
/// SAFETY: E1 — under the FS lock; it only reads `CTAG`, and the returned index is a
/// slot of this set, so it is `< ncache()` by construction.
unsafe fn find_slot(bn: u32) -> Option<usize> {
    let set = set_of(bn);
    for w in 0..WAYS { let s = way_slot(w, set); if CTAG[s] == bn + 1 { return Some(s); } }
    None
}
/// Whom to evict in the set: first an empty way (then a sequential file
/// lands entirely in way 0 and the §8.2 coalescing works), otherwise a NON-pinned one with
/// the least recent access. The invariant "there is always a non-pinned way in a set"
/// is kept by mmap_prepare (§8.5), so a None here is impossible.
/// §8.7: among the candidates a CLEAN one is preferable to a dirty one, while dirty data are
/// preferable to dirty metadata. A miss on a clean one costs one read (131 us here, ~1 ms
/// on EBS); the eviction of dirty data is a synchronous write of 4 KB under the FS lock
/// (426 us, and there were 12 thousand of those over a run), of dirty metadata it is a whole
/// journal transaction with THREE barriers under the lock (24 ms each on a throttled
/// disk). Dirt does not pile up endlessly: a writer is held by the `dirty_balance` threshold.
/// SAFETY: E1 + E2 + E5 — under the FS lock. The choice is what makes E2 hold: a slot
/// that is pinned (`CPIN`) or in flight (`CWB`) is never returned, so a pointer handed out
/// earlier for such a slot cannot be invalidated behind its holder's back. `CWB` is read
/// with `read_volatile` (E7) because the neighbour that clears it does so with the FS lock
/// released. The wait for the landing drops `dev_lock` in the order FS -> device, which is
/// the order the whole kernel keeps.
unsafe fn victim(set: usize) -> usize {
    let mut best = usize::MAX;
    let mut best_rank = 0u8;
    for w in 0..WAYS {
        let s = way_slot(w, set);
        if CTAG[s] == 0 && CPIN[s] == 0 { return s; }
        if CPIN[s] > 0 || CWB[s] { continue; }
        let rank = if !CDIRTY[s] { 0 } else if COWN[s] != 0 { 1 } else { 2 };
        if best == usize::MAX || rank < best_rank || (rank == best_rank && CUSE[s].wrapping_sub(CUSE[best]) > u32::MAX / 2) {
            best = s; best_rank = rank;
        }
    }
    if best == usize::MAX {
        // All the non-pinned ways are in flight (§8.6): wait for the landing — it
        // completes together with the device's lock — and choose anew. Waiting for
        // the device while holding the FS lock is allowed by the order (the FS -> the device).
        loop {
            drop(crate::fs::dev_lock());
            let mut alt = usize::MAX;
            for w in 0..WAYS {
                let s = way_slot(w, set);
                if CPIN[s] > 0 || core::ptr::read_volatile(core::ptr::addr_of!(CWB[s])) { continue; }
                if alt == usize::MAX || CUSE[s].wrapping_sub(CUSE[alt]) > u32::MAX / 2 { alt = s; }
            }
            if alt != usize::MAX { return alt; }
            if pinned_in_set(set) >= WAYS { break; }
        }
        best = way_slot(0, set); // all the ways are pinned — that must not happen
    }
    best
}
/// The cache's slot for the block bn (it loads it from the disk at a miss). It returns a pointer to BS bytes.
/// SAFETY: E1 + E2 — under the FS lock; the returned pointer is valid for `BS` bytes only
/// until the next `cblk` for another block of the SAME set (see E2 and `cblk_mode`).
unsafe fn cblk(bn: u32) -> *mut u8 { cblk_mode(bn, true) }
/// §4.1: a slot for a JUST ALLOCATED block — WITHOUT a read from the disk. Its former
/// content is garbage by definition (the block was free), and there is no point in reading it.
/// It was found on a live EBS: the write phase of fsbench (512 blocks) cost 290 ms — those
/// were 512 round-trip latencies of the disk for reading blocks that were zeroed right away.
/// Under QEMU with its instant disk the difference is almost invisible.
/// SAFETY: E1 + E2 — as `cblk`, and additionally: the caller guarantees the block has JUST
/// been allocated, so skipping the read cannot show anybody another file's data — the slot
/// is overwritten in full by the caller (`zero_block` or a full-block write).
unsafe fn cblk_new(bn: u32) -> *mut u8 { cblk_mode(bn, false) }
/// THE READAHEAD: the blocks OF THE FILE that lie in a row on the disk go in one request.
///
/// What for. A read through the cache went BLOCK BY BLOCK, and every miss was a separate round
/// trip to the device: ClickHouse asks for a megabyte, the kernel makes two hundred
/// and fifty-six round trips of 4 KB — a `pread` of 9.09 ms PER CALL. And all that
/// time THE SINGLE FS lock is held, so the neighbouring cores do not read
/// in parallel but spin: on a table that does not fit into the cache four executor
/// threads came out TWICE AS SLOW AS ONE.
///
/// WHY EXACTLY LIKE THIS AND NOT STRAIGHT INTO THE SLOTS. The first attempt read the range straight
/// into the cache, choosing the slots by itself, and twice corrupted the data: a block that already
/// lay in another way got a second copy (a write went into one, while
/// a read came into the other — from the outside that looked like a lost file), and
/// after that was fixed a corruption of the inode bitmap remained. The reason is common:
/// the rules of a slot's ownership (the eviction of a dirty one, a pin from an mmap, the journal) live in
/// `cblk_mode`, and whoever lays blocks out bypassing it must repeat them —
/// that is, duplicate the subtlest logic in the kernel.
///
/// So here there is NOT A SINGLE new rule: a range is read into a separate window,
/// while the blocks are laid out over the slots by the regular `cblk_new` (it will both evict and
/// ask the journal), after which the window is copied into the slot. An extra memcpy per block
/// against a round trip to the device is an obviously profitable exchange.
///
/// We take only the blocks OF THIS file (their numbers are given by `bmap`), only the ones going in a row
/// on the disk and only the ones absent from the cache: a block that already lies in the cache
/// may be dirtier than the disk, and overwriting it with what was read is that very corruption.
const RA_MAX: usize = 32;                 // 128 KB per access
/// `talaria.readahead=<blocks>` — the readahead window, it is on by default.
///
/// What it gives, honestly: the number of ACCESSES TO THE DEVICE when reading a large file
/// falls by about twenty times (633 requests instead of twelve thousand
/// block-by-block ones) — ClickHouse asks for a megabyte, and without the gluing the kernel makes two
/// hundred and fifty-six round trips of 4 KB. An end-to-end gain IN TIME
/// is not visible on our stand: an access to the device goes under the FS lock, and
/// the neighbouring cores wait anyway — the bottleneck is there right now rather than in the number of
/// requests. On a device with a real round-trip latency (not QEMU on top of
/// the host's page cache) the difference must be noticeable, and the mechanism's price is
/// one memcpy per block.
///
/// The safety is not on trust: a range is read into a separate window, while the blocks are laid out
/// over the slots by the regular `cblk_new` — not a single new rule of a slot's
/// ownership, nothing is done bypassing `cblk_mode`. Only the blocks OF THIS
/// file are taken, only the ones going in a row on the disk and only the ones absent from the cache: a block
/// that already lies in the cache may be dirtier than the disk, and overwriting it with what was read is
/// a corruption. The gate is `talaria.test=fsbig` (a file four times larger than the cache, a check of
/// every byte) plus an e2fsck in the regression.
pub static RA_WINDOW: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(RA_MAX);
/// Set the readahead window (a common point for the knob and the manifest); return
/// what really took effect (the window is clamped by the ceiling of the buffer allocated at the mount).
pub fn ra_set(v: usize) -> usize {
    let v = v.min(RA_MAX);
    RA_WINDOW.store(v, core::sync::atomic::Ordering::SeqCst);
    v
}

pub fn ra_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::num(cmdline, "talaria.readahead") {
        let v = (v as usize).min(RA_MAX);
        RA_WINDOW.store(v, core::sync::atomic::Ordering::SeqCst);
        crate::println!("ext2: the readahead window = {} blocks{}", v, if v == 0 { " (it is off)" } else { "" });
    }
}
static mut RA_BUF: u64 = 0;               // the window (RA_MAX * BSZ), it is allocated at the mount
static mut STATS_RA: (u64, u64) = (0, 0); // (the accesses, the blocks picked up beyond the first)
/// (how many times we read ahead, how many blocks were picked up beyond the first) — for `stat:`.
/// SAFETY: the statistics exception of `cache_stats` — read without the FS lock on purpose.
pub fn ra_stats() -> (u64, u64) { unsafe { STATS_RA } }

/// SAFETY: E1 + E2 — under the FS lock. Two things carry the `unsafe` here:
/// * the staging window: `RA_BUF` is one allocation of `RA_MAX * BSZ` made at the mount, and
///   the slice is `n * BS` with `n < win <= RA_MAX` and `BS <= BSZ`, so it stays inside it;
/// * the handout into the cache: the loop calls `cblk_new` for `n` blocks that are
///   CONSECUTIVE by number, and consecutive numbers fall into different sets
///   (`bn % nsets()`, and `nsets() >> RA_MAX`), so no block of the run can evict another
///   block of the same run — without that, a pointer taken on one turn of the loop would be
///   invalidated on the next, which is exactly what E2 forbids.
unsafe fn prefetch_run(ino: u32, inb: &mut [u8; 128], fb0: u64, maxfb: u64) -> usize {
    let win = RA_WINDOW.load(core::sync::atomic::Ordering::Relaxed);
    if RA_BUF == 0 || BS != BSZ || maxfb < 2 || win < 2 { return 0; }
    let mut add = 0u32;
    let first = bmap(ino, inb, fb0, false, &mut add);
    if first == 0 || find_slot(first).is_some() { return 0; }
    let mut n = 1usize;
    while (n as u64) < maxfb && n < win {
        let b = bmap(ino, inb, fb0 + n as u64, false, &mut add);
        if b == 0 || b != first + n as u32 || b >= TOTAL_BLOCKS { break; }
        if find_slot(b).is_some() { break; }
        n += 1;
    }
    if n < 2 { return 0; }
    let buf = core::slice::from_raw_parts_mut(crate::mem::kptr::<u8>(RA_BUF), n * BS);
    if !crate::fs::blk_read((first as usize) * SPB, buf) { return 0; } // it did not work out — the ordinary path
    STATS_RD += n as u64;
    STATS_RA.0 += 1; STATS_RA.1 += n as u64 - 1;
    // The layout goes ONLY by the regular path. Blocks that are neighbours by number get into different
    // sets (`bn % NSETS`, NSETS >> RA_MAX), so they cannot evict each other inside
    // one range.
    for k in 0..n {
        let p = cblk_new(first + k as u32);
        core::ptr::copy_nonoverlapping(crate::mem::kptr::<u8>(RA_BUF).add(k * BS), p, BS);
    }
    n
}

/// SAFETY: E1 + E2 + E3 + E6 + E7 — under the FS lock. This is where E2 is CREATED: the
/// function either finds the block in its slot or takes a slot from `victim` and re-tags it,
/// and re-tagging is the only event that invalidates a pointer handed out earlier. Hence the
/// order inside: commit an uncommitted dirty metadata block first (E6, §8.4/§8.8) with the
/// lock pinned down (`MayRelease::set(false)`, E4 — releasing it inside the replacement
/// would hand a neighbour the slot we have already taken), then write a dirty slot back,
/// then read, and only then change `CTAG` — bracketed by `dcnt_del`/`dcnt_add` (E3).
/// `CTAG` is touched through `read_volatile`/`write_volatile` (E7).
unsafe fn cblk_mode(bn: u32, read: bool) -> *mut u8 {
    let slot = match find_slot(bn) { Some(s) => s, None => victim(set_of(bn)) };
    touch(slot);
    let tag = core::ptr::read_volatile(addr_of_mut!(CTAG[slot]));
    let p = crate::mem::kptr::<u8>(CACHE_BASE).add(slot * BSZ);
    if tag == bn + 1 { STATS_HIT += 1; } else { STATS_MISS += 1; }
    if tag != bn + 1 {
        // §8.4: evicting dirty METADATA in place while their transaction is not
        // committed is not allowed — that is a hole in the atomicity. An eviction becomes
        // a transaction's boundary: first a commit+a checkpoint, then the slot's replacement.
        // §8.8: if the block is already COMMITTED (CJRN), writing it in place is possible at
        // any moment — a checkpoint will simply not meet it any more. A commit
        // is needed only for an uncommitted one, and a checkpoint after the commit is not needed:
        // previously an eviction did a commit+a checkpoint (5 barriers under the FS
        // lock), and on a mongodb insert that was 38 checkpoints per 97 transactions.
        if tag != 0 && CDIRTY[slot] && COWN[slot] == 0 && !CJRN[slot] && JRN && !JBUSY {
            // THE SLOT'S REPLACEMENT IS GOING RIGHT NOW: releasing the lock inside this
            // transaction would mean giving a neighbour the slot we have already occupied.
            let _rel = MayRelease::set(false);
            EVICT_META.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            jrn_commit(0);
        }
        if tag != 0 && CDIRTY[slot] { writeback_slot(slot); } // the eviction of a dirty block
        if read {
            let buf = core::slice::from_raw_parts_mut(p, BS);
            if !crate::fs::blk_read((bn as usize) * SPB, buf) { crate::println!("ext2: an error reading the block {}", bn); }
            STATS_RD += 1;
        }
        dcnt_del(slot);
        core::ptr::write_volatile(addr_of_mut!(CTAG[slot]), bn + 1); CDIRTY[slot] = false; CNEW[slot] = false; COWN[slot] = 0; CJRN[slot] = false; CJOLD[slot] = false;
    }
    p
}
pub static EVICT_META: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static EVICT_DATA: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static EVICT_RUNS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static EVICT_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static EVICT_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// (the blocks evicted, the accesses to the device, the total tsc, the worst one) — for `stat:`.
pub fn evict_stats() -> (u64, u64, u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (EVICT_DATA.load(Relaxed), EVICT_RUNS.load(Relaxed), EVICT_TSC.load(Relaxed), EVICT_MAX.load(Relaxed))
}

/// §8.14 THE EVICTION OF A DIRTY SLOT GOES IN A BATCH rather than block by block
/// (`talaria.evictbatch=<blocks>`, the default 1 — as it was).
///
/// What was measured. Scoreboard 35 (step 0 of session 36): `mongodb/insert` loses to Linux
/// by the `p99.9` 1.6-1.8 times, and that is NOT the class "the foreign 8 ms" — multi-millisecond
/// events are there on both sides, and we have FEWER of them (207 against 352). We
/// lose on THE HALF-MILLISECOND scale: 450-678 us against 250-515.
///
/// Where our expense on that scale comes from. §8.7 names it: a miss whose
/// eviction candidate is dirty costs a SYNCHRONOUS write of 4 KB under the FS lock —
/// 426 us on EBS, and there were twelve thousand of those over a run. One block per
/// round trip to the device: an access costs as much as
/// an access of a quarter of a megabyte, but it pays off 4 KB.
///
/// What we do. When evicting a dirty slot, we take together with it THE TAIL OF CONTIGUOUS
/// dirty blocks of DATA — and give them to the device in one request. The slots
/// are laid out per way (`slot = way * NSETS + bn % NSETS`), so
/// blocks that are neighbours by number lie in neighbouring slots, and hence in neighbouring
/// pages of memory: one request, one contiguous area both there and there.
/// The neighbourhood IS CHECKED by the block's number (`CTAG[s] - 1 == bn + k`) rather than
/// assumed: the same remainder of the division belongs to the block NSETS further too.
///
/// Why this is safe (not a single new rule of a slot's ownership):
/// * the batch does not touch `CTAG` and does not move the blocks — the pointers handed out to the code that
///   called us for other slots stay valid; only the signs
///   "dirty/the owner" change, exactly as in `writeback_locked`;
/// * ONLY data (`COWN != 0`) are taken as companions: dirty metadata
///   require a transaction's boundary (a commit higher up in `cblk_mode`), and climbing into it
///   in passing is not allowed;
/// * data that went to the disk EARLIER than their transaction do not break
///   the ordered mode — the order "the data before the metadata" is only strengthened;
/// * the pinned (`CPIN`) and the already flying (`CWB`) slots are skipped.
///
/// The default is 1 (the former behaviour): the batch's price is chosen by the stand rather than by the author.
pub static EVICT_BATCH: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(1);
/// The batch's ceiling: 256 blocks = 1 MB, as much as the portion of `writeback_unlocked`.
const EVICT_MAX_BLOCKS: usize = 256;
pub fn evict_from_cmdline(cmdline: &str) {
    if let Some(b) = crate::cmdline::num(cmdline, "talaria.evictbatch") {
        let b = (b as usize).clamp(1, EVICT_MAX_BLOCKS);
        EVICT_BATCH.store(b, core::sync::atomic::Ordering::SeqCst);
        crate::println!("ext2: the eviction of a dirty slot in a batch of up to {} blocks{}", b,
                        if b == 1 { " (as it was: one by one)" } else { "" });
    }
}
/// SAFETY: E1 + E2 + E5 — under the FS lock. The batch takes only slots that are
/// contiguous BOTH by slot and by block number and that are neither pinned nor in flight, and
/// it does not touch `CTAG`: the blocks do not move, so pointers handed out for them stay
/// valid (E2). Only data (`COWN != 0`) are taken as companions — dirty metadata would need
/// a transaction boundary (E6), and climbing into one in passing is not allowed.
unsafe fn writeback_slot(slot: usize) {
    use core::sync::atomic::Ordering::Relaxed;
    let t0 = crate::tsc::rdtsc();
    let bn = CTAG[slot] - 1;
    // How many blocks will leave in one request. The evicted slot itself goes always, it
    // must be freed; the rest are companions, and only while they go
    // IN A ROW both by slots and by block numbers.
    let want = EVICT_BATCH.load(Relaxed).clamp(1, EVICT_MAX_BLOCKS);
    let mut k = 1usize;
    if want > 1 && BS == BSZ {
        let ns = nsets();
        let way_end = (slot / ns + 1) * ns;      // we do not go beyond the way's boundary
        let mut s = slot + 1;
        while s < way_end && s < ncache() && k < want {
            if CTAG[s] == 0 || !CDIRTY[s] || CWB[s] || CPIN[s] > 0 || COWN[s] == 0 { break; }
            if CTAG[s] - 1 != bn + k as u32 { break; }
            k += 1; s += 1;
        }
    }
    EVICT_DATA.fetch_add(k as u64, Relaxed);
    EVICT_RUNS.fetch_add(1, Relaxed);
    // An eviction is the third population of the age: nobody pre-empted this block, and
    // it left in place synchronously, under the FS lock.
    {
        let now = dstamp_of(t0);
        let (mut age_sum, mut age_max, mut age_k) = (0u64, 0u32, 0usize);
        for i in 0..k { let s = slot + i; if COWN[s] != 0 { let a = age_of(now, CDIRTIME[s]); age_sum += a as u64; if a > age_max { age_max = a; } age_k += 1; }
            }
        wbage_note(WbC::Evict, age_k, age_sum, age_max);
    }
    let p = crate::mem::kptr::<u8>(CACHE_BASE).add(slot * BSZ);
    let buf = core::slice::from_raw_parts(p, BS * k);
    if !crate::fs::blk_write((bn as usize) * SPB, buf) { crate::println!("ext2: an error writing the block {}", bn); }
    STATS_WR += k as u64;
    for i in 0..k {
        let s = slot + i;
        dcnt_del(s);
        CDIRTY[s] = false; CNEW[s] = false;
        if i > 0 { COWN[s] = 0; CJRN[s] = false; CJOLD[s] = false; }
    }
    let d = crate::tsc::rdtsc().wrapping_sub(t0);
    EVICT_TSC.fetch_add(d, Relaxed);
    if d > EVICT_MAX.load(Relaxed) { EVICT_MAX.store(d, Relaxed); }
}
/// Mark the block bn (in the cache) dirty — it will go to the disk at an eviction or a sync.
///
/// SAFETY: E1 + E3 — under the FS lock; the change of `CDIRTY`/`CJRN`/`CJOLD` is bracketed
/// by the counter pair. If the block is no longer in the cache there is nothing to mark:
/// it can only have left through `cblk_mode`, which writes a dirty slot out before re-tagging.
unsafe fn cflush(bn: u32) {
    if let Some(slot) = find_slot(bn) {
        dcnt_del(slot);
        if !CDIRTY[slot] { CDIRTY[slot] = true; CDIRTIME[slot] = dstamp(); }
        if CJRN[slot] { CJOLD[slot] = true; } // §8.9: the committed version is only in the journal
        CJRN[slot] = false; // it was changed after the journalling -> into the next transaction
        dcnt_add(slot);
    }
}
/// Mark a block dirty and record its owner (the inode) — for a per-file fsync (§8.3).
///
/// SAFETY: E1 + E3 — as `cflush`, and `COWN` is part of the bracketed state: it decides
/// whether the block counts as data or as metadata in every counter.
unsafe fn cflush_own(bn: u32, ino: u32) {
    if let Some(slot) = find_slot(bn) {
        dcnt_del(slot);
        if !CDIRTY[slot] { CDIRTY[slot] = true; CDIRTIME[slot] = dstamp(); }
        if CJRN[slot] { CJOLD[slot] = true; }
        COWN[slot] = ino; CJRN[slot] = false;
        dcnt_add(slot);
    }
}

/// What exactly to flush (§8.2/8.3/8.4).
#[derive(Clone, Copy, PartialEq)]
enum Sel {
    All,            // everything dirty (a sync)
    Data(u32),      // the data of one inode (the ordered mode before a commit)
    DataAll,        // the data of all the files
    DataAged(u32),  // the data of all the files OLDER than the threshold (in units of dstamp)
    DataNew,        // the freshly allocated data blocks of all the files (`CNEW`: before a metadata commit)
    Journaled,      // the metadata that already lie in the journal (a checkpoint)
    Own(u32),       // the inode's data + the metadata (an fsync without the journal)
}

/// §8.2 THE FLUSH'S CORE: walk the cache in ascending order of the slot, collect the selected dirty
/// blocks into CONTIGUOUS ranges and give them to the driver in a batch (`blk_write_many`).
/// Why the ranges come out: a block's slot = bn % NC, which means blocks that are neighbours by number
/// lie next to each other in the cache too — a range of the disk's blocks is a range of memory as well
/// (with BS == BSZ; with a smaller block the gluing is switched off).
/// It returns the number of the blocks written.
/// SAFETY: E1 + E5 — entered under the FS lock. The first pass gives the lock up for the
/// time of the device (`writeback_unlocked`, E4/E5); everything it walks afterwards it walks
/// with the lock held again.
unsafe fn writeback(sel: Sel, cause: WbC) -> usize {
    if DIRTY_N == 0 { return 0; }
    // §8.6: the files' data go in portions through a substitute buffer WITHOUT the FS lock for the time of
    // the disk; the metadata (there are few of them) go under the lock as before, right after.
    let (mut written, left) = writeback_unlocked(sel, usize::MAX, WB_BLOCKS, cause);
    // SESSION 39: THE SECOND PASS IS CALLED ONLY WHEN IT HAS SOMETHING TO WRITE.
    //
    // It goes over THE WHOLE cache (131072 slots with fscache=512), while the condition was
    // `DIRTY_N > 0` — "there is at least something dirty in the cache". Under mongodb that
    // was always enough: an fsync of WiredTiger's journal goes with the dirty blocks of OTHER
    // files, and the pass honestly walked the whole cache in order to write nothing.
    // The instrument: 156 us per call, 5 % of its price (docs/MEASUREMENTS.md §3).
    //
    // The condition now goes BY THE SELECTION rather than by "the dirty ones in general", and every branch
    // answers the question "what could the first pass have missed":
    //   * THE DATA — the first pass has already walked the whole cache and written everything that
    //     fitted; only the slots IN FLIGHT (CWB) could have been left and the case when
    //     the pass did not take place at all (there is no substitute buffer, the block is smaller than
    //     a page, the FS lock must not be released) — it reports exactly that
    //     with the flag `left`. `Sel::Data(0)` is the metadata, the first pass does not
    //     take them (it requires COWN != 0), so such a selection is judged as All;
    //   * THE METADATA (All/Own) are written ONLY by the second pass: DIRTY_N - DN_DATA;
    //   * A CHECKPOINT (Journaled) — the first pass takes only the journal's metadata
    //     (COWN == 0 && CJRN), while the second also takes the data in the journal:
    //     they are DN_DATA - JP_DATA (the dirty data minus those that have not seen the journal).
    let need = if !fscnt() { DIRTY_N > 0 } else { left || match sel {
        Sel::Data(ino) => ino == 0 && DIRTY_N > DN_DATA,
        Sel::DataAll | Sel::DataAged(_) | Sel::DataNew => false,
        Sel::All | Sel::Own(_) => DIRTY_N > DN_DATA,
        Sel::Journaled => DN_DATA > JP_DATA,
    } };
    // We leave the phase in its place even when there was no pass: the instrument
    // must show the disappearance of the 5 % of the call's price rather than stay silent about it.
    if need { let t0 = crate::tsc::rdtsc(); written += writeback_locked(sel, cause); ph_note(17, t0); }
    written
}

/// §8.7 THE DIRTY THRESHOLD: A WRITER THAT HAS CROSSED IT FLUSHES A PORTION ITSELF.
///
/// The kernel has no background flush: the dirty blocks piled up until an fsync or until
/// an eviction, while the eviction of a dirty one is a synchronous write under the FS lock, one
/// block at a time (§8.7 at `victim`). When there is more than a quarter of the cache dirty, every
/// `write` first gives the device a portion of somebody else's dirty data WITHOUT the FS lock
/// (§8.6) — that is Linux's `balance_dirty_pages`: the dirt is paid for by the one who
/// makes it, at the device's rate, rather than by the neighbours on the FS. The portion is small (256 KB,
/// ~2 ms on gp3), so that one write does not stand long.
/// THE PRE-EMPTIVE FLUSH OF DIRTY DATA FROM A WRITE (the threshold and the portion are knobs).
///
/// Session 30: the tail of `mongodb/insert` p99.9 is 4.3 ms against Linux's 0.46 ms (with
/// a BETTER median/p99/p99.99), the stand `diag-mongo-insert-20260913`. There are no barriers
/// (FUA, EBS has no volatile cache); the reads do not grow under load. The tail is
/// the `fdatasync` of WiredTiger's journal: the journal's dirty blocks pile up until the flush
/// threshold (1/4 of the cache = 128 MB with fscache=512) and go to EBS SYNCHRONOUSLY in one
/// flush of ~4 ms. With Linux the background writeback flushes the dirt in advance, and an fdatasync
/// flushes the small remainder in 0.46 ms. We DO HAVE a mechanism of a pre-emptive flush
/// (`writeback_unlocked`, a safe one: a copy + the order by the device's lock),
/// but the threshold is high — the flush starts late.
///
/// `talaria.dirtyfrac=N` — flush when there is more than 1/N of the cache dirty (a bigger N
/// means earlier/more aggressive; the default 4 = as it was). `talaria.dirtychunk=B` —
/// the blocks per portion of a flush from a write (the default 64 = 256 KB). The price of the aggression is
/// that the same dirt goes to EBS a little earlier and in smaller pieces (not extra traffic but a different
/// rate); to be checked by an A/B on the insert's tail.
pub static DIRTY_FRAC: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(4);
pub static WB_BALANCE_BLOCKS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(64);
pub static WB_BALANCE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub fn dirty_from_cmdline(cmdline: &str) {
    use core::sync::atomic::Ordering::SeqCst;
    if let Some(n) = crate::cmdline::num(cmdline, "talaria.dirtyfrac") {
        if n >= 1 { DIRTY_FRAC.store(n as usize, SeqCst);
            crate::println!("ext2: a pre-emptive flush when the dirty ones are > 1/{} of the cache", n); }
    }
    if let Some(b) = crate::cmdline::num(cmdline, "talaria.dirtychunk") {
        if b >= 1 { WB_BALANCE_BLOCKS.store((b as usize).min(WB_BLOCKS), SeqCst);
            crate::println!("ext2: the portion of the pre-emptive flush is {} blocks", b.min(WB_BLOCKS as u64)); }
    }
}
// ══ STEP 1 OF SESSION 40: THE PRE-EMPTIVE FLUSH BY THE AGE OF THE DIRTY DATA ═
//
// Session 39's instrument (`docs/MEASUREMENTS.md` §3) closed two questions and opened a knob:
//   * the §8.7 threshold DOES NOT APPLY to the `fsync` path: there are 45-50 dirty
//     data blocks per call with a threshold of 32768 — three orders of difference, and `dirtyfrac`
//     does not work here with any setting;
//   * the objection "the block was dirtied microseconds before the call" IS REFUTED: by
//     the moment of the flush the dirty data are 2.7-3.2 s old (the worst one 58 s). Such a block would be carried away even by
//     Linux's five-second kick — which means "with Linux the dirty data left in advance" is correct,
//     and there IS SOMETHING to pre-empt.
//
// As with Linux (`dirty_expire_centisecs` + a kick once every 5 s), but without a background thread:
// the one who dirties pays — a `write` once per period looks at THE CLOCK (two
// comparisons of u32, not a walk over the cache) and, if the period has expired, gives the device
// a portion of the data OLDER than the threshold. A walk over the cache happens only in the flush itself,
// once per period rather than at every write: session 39's rule ("a counter instead of
// a walk") is bought exactly here.
//
// `talaria.dirtyage=<ms>` — 0 (the default) means THE FORMER BEHAVIOUR: the knob
// is switched on only by a measurement on its own run, as session 39's rule requires.
pub static DIRTY_AGE_MS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
pub static AGE_FLUSHES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static AGE_BLOCKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The worst flush by age IN ONE GO — the main number of step 1 of session 41: it is
/// with it rather than with the average that the workload's thread caught under a kick pays.
pub static AGE_MAXBLK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The kicks to which the clock (`DN_OLD`) answered "there is nothing that has lain long enough" — without a walk over
/// the cache. Their share is exactly the price that the smearing did NOT pay.
pub static AGE_SKIPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// More than one portion per period — otherwise the flush will not catch up with the workload's rate:
/// mongodb on the ladder's lowest step dirties ~500 data blocks a second, while
/// the §8.7 portion is 64 blocks (256 KB).
const AGE_CHUNKS: usize = 4;
// STEP 1 OF SESSION 41: PAYING IN A BATCH IS NOT ALLOWED.
//
// Session 40 measured both sides of the knob: it takes the data off an `fsync` call
// (47 -> 9.4 dirty blocks, the age 2.1 s -> 29.7 ms), but `write` pays for that
// WITH A BATCH — 44 flushes of 136 blocks over a run, and the insert's `p99.9` is four times worse
// (380 -> 1532 us). The batch came out by construction: the kick's period EQUALLED
// the age's threshold, which means everything that had lain long enough over the period accumulated per kick.
//
// The change separates two numbers that with Linux are always separated
// (`dirty_expire_centisecs` is the age, `dirty_writeback_centisecs` is the
// kick's period): the kick goes EIGHT TIMES more often than the threshold and carries EIGHT TIMES less per time.
// The throughput is the same (256 blocks per 300 ms = 32 blocks per 37 ms),
// while a thread caught under a kick waits for the device instead of 544 KB — for 128.
const AGE_SPLIT: usize = 8;
/// `talaria.dirtykick=<ms>` — the kick's period; 0 (the default) = the threshold/AGE_SPLIT.
pub static DIRTY_KICK_MS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// `talaria.dirtybatch=<blocks>` — the ceiling of ONE kick; 0 (the default) =
/// AGE_CHUNKS * dirtychunk / AGE_SPLIT. Session 40's A/B is reproduced with the knobs:
/// `talaria.dirtykick=<the threshold> talaria.dirtybatch=256`.
pub static DIRTY_BATCH: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static mut AGE_LAST: u32 = 0;                 // the dstamp of the last flush by age
/// Milliseconds -> units of `dstamp` (2^20 cycles).
fn units_of_ms(ms: usize) -> u32 {
    if ms == 0 { return 0; }
    let hz = crate::tsc::hz();
    if hz == 0 { return 0; }
    (((ms as u64).saturating_mul(hz) / 1000) >> 20).max(1) as u32
}
/// The age's threshold in units of `dstamp`. 0 — the knob is off.
fn age_units() -> u32 { units_of_ms(DIRTY_AGE_MS.load(core::sync::atomic::Ordering::Relaxed)) }
/// The kick's period in units of `dstamp` (by default an eighth of the threshold).
fn kick_units(thr: u32) -> u32 {
    let ms = DIRTY_KICK_MS.load(core::sync::atomic::Ordering::Relaxed);
    if ms != 0 { units_of_ms(ms) } else { (thr / AGE_SPLIT as u32).max(1) }
}
/// The ceiling of one kick in blocks.
fn age_batch() -> usize {
    let b = DIRTY_BATCH.load(core::sync::atomic::Ordering::Relaxed);
    if b != 0 { return b; }
    (AGE_CHUNKS * WB_BALANCE_BLOCKS.load(core::sync::atomic::Ordering::Relaxed) / AGE_SPLIT).max(1)
}
/// The ceiling of one kick — for the gate `dirtysmear` (it judges the worst flush by it).
pub fn age_batch_pub() -> usize { age_batch() }
pub fn dirtyage_from_cmdline(cmdline: &str) {
    use core::sync::atomic::Ordering::SeqCst;
    if let Some(v) = crate::cmdline::num(cmdline, "talaria.dirtykick") { DIRTY_KICK_MS.store(v as usize, SeqCst); }
    if let Some(v) = crate::cmdline::num(cmdline, "talaria.dirtybatch") { DIRTY_BATCH.store(v as usize, SeqCst); }
    if let Some(v) = crate::cmdline::num(cmdline, "talaria.dirtyage") {
        DIRTY_AGE_MS.store(v as usize, SeqCst);
        if v == 0 { crate::println!("ext2: the pre-emptive flush of the data by age IS OFF (as before session 40)"); }
        else {
            let kick = DIRTY_KICK_MS.load(SeqCst);
            crate::println!("ext2: a pre-emptive flush of the data older than {} ms: a kick once every {} ms, up to {} blocks per kick",
                v, if kick != 0 { kick } else { (v as usize / AGE_SPLIT).max(1) }, age_batch());
        }
    }
}
/// (the flushes by age, the blocks in them, the worst flush, the kicks without a walk) — for
/// `stat:` and for the gate.
pub fn age_stats() -> (u64, u64, u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (AGE_FLUSHES.load(Relaxed), AGE_BLOCKS.load(Relaxed), AGE_MAXBLK.load(Relaxed), AGE_SKIPS.load(Relaxed))
}
/// Once per period give the device the data older than the threshold. It is called from `write`
/// next to the §8.7 threshold; at the write itself that is two comparisons, while between the kicks —
/// two more (the clock `DN_OLD`), but NOT a walk over the cache.
/// SAFETY: E1 + E3 — under the FS lock; the decision to walk at all is taken by `DN_OLD`,
/// whose error is one-sided by E3 (older than the truth, never newer), so a kick may come
/// a turn too early but never fails to come for a block that has lain long enough.
unsafe fn dirty_age_flush() {
    let thr = age_units();
    if thr == 0 || DN_DATA == 0 || WB_BUF == 0 { return; }
    let now = dstamp();
    if AGE_LAST != 0 && now.wrapping_sub(AGE_LAST) < kick_units(thr) { return; }
    // The clock instead of a walk: the oldest dirty one is known by a counter. The estimate
    // may be older than the truth — then we will make AN EXTRA pass and at its end
    // will correct it; newer than the truth it never is (`dn_old_audit`).
    if DN_OLD_SET && !aged(now, DN_OLD, thr) {
        AGE_SKIPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return;
    }
    AGE_LAST = now;
    let batch = age_batch();
    let chunk = batch.min(WB_BALANCE_BLOCKS.load(core::sync::atomic::Ordering::Relaxed)).max(1);
    let _rel = MayRelease::set(true); // the entry into a write: there is no state above
    let (n, left) = writeback_unlocked(Sel::DataAged(thr), batch.div_ceil(chunk), chunk, WbC::Age);
    // The pass reached the end of the cache and left nothing selected — which means everything
    // that stayed dirty is younger than the threshold. And that is the new estimate.
    if !left { DN_OLD = now.wrapping_sub(thr); DN_OLD_SET = DN_DATA != 0; }
    if n > 0 {
        use core::sync::atomic::Ordering::Relaxed;
        AGE_FLUSHES.fetch_add(1, Relaxed);
        AGE_BLOCKS.fetch_add(n as u64, Relaxed);
        AGE_MAXBLK.fetch_max(n as u64, Relaxed);
    }
}
/// SAFETY: E1 — under the FS lock, and it may release it inside the flush it calls (E4),
/// so the caller must be one that tolerates that: it is called from the write path, where
/// the inode buffer lives in the caller's stack rather than in the cache (E2).
unsafe fn dirty_balance() {
    use core::sync::atomic::Ordering::Relaxed;
    let frac = DIRTY_FRAC.load(Relaxed);
    if DIRTY_N * frac < ncache() || WB_BUF == 0 { return; }
    let _rel = MayRelease::set(true); // the entry into a write: there is no state above
    WB_BALANCE.fetch_add(1, Relaxed);
    let _ = writeback_unlocked(Sel::DataAll, 1, WB_BALANCE_BLOCKS.load(Relaxed), WbC::Balance);
}

/// §8.6 THE FLUSH OF DATA WITHOUT THE FS LOCK FOR THE TIME OF THE DISK.
///
/// What for. An `fsync` of a large file gave the device hundreds of megabytes UNDER THE FS
/// LOCK: on EC2 (gp3, 125 MB/s) a WiredTiger checkpoint cost a second, and for that
/// second ALL mongodb's threads stalled — even those that needed an `fstat` or
/// a hit in the cache. tlgen's tape showed a stop with a p50 of 0.8-1.4 s once a minute
/// at a third of the ceiling; with Linux on the same volume there are no such stops: there an fsync
/// blocks only its own caller.
///
/// How. The dirty data blocks are copied under the FS lock into a substitute buffer
/// (WB_BLOCKS blocks per portion), are marked clean and "in flight" (CWB), after
/// which the FS lock IS RELEASED, while the portion goes to the device under its lock.
/// A copy rather than a DMA from the slot: while there is no lock, the workload's thread may write into
/// the same block, and a DMA straight from the cache would give a torn block on the disk belonging to neither
/// version. The order of the writes of one block is preserved by the device's lock: any
/// next writer of that block copies it AFTER our release of the FS lock,
/// while the device's lock by that moment is already ours until the portion lands.
/// The device's lock is taken UNDER the FS lock (the order FS -> the device), but without
/// waiting under it: it is busy — the FS is released, the device is waited out from the outside.
/// The permission to release the lock is the same as at the barrier (`MayRelease`); without it,
/// without a buffer and for the metadata `writeback_locked` remains.
const WB_BLOCKS: usize = 256;            // 1 MB per portion: ~8 ms of disk on gp3
static mut WB_BUF: u64 = 0;              // the substitute buffer (WB_BLOCKS * BSZ), it is allocated at the mount
pub static WB_CHUNKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static WB_DEVWAIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
// §8.13: the LAST portion of an fsync's data is submitted and NOT waited for — it will be waited for by
// the descriptor's batch of the same transaction (see `fs::DEFER_MASK`). It needs its own
// buffer (WB_BUF will go under a neighbour's next portion right away) and a list of the ways
// that stay "in flight" until `wb_defer_finish`. Only the one that raised
// `WB_DEFER_TASK` defers — a flush by the threshold of a neighbouring thread that got into the same
// window writes as before.
//
// STEP 2 OF SESSION 40: THIS IS NOW A CLAIM rather than a consequence of JBUSY. The uniqueness
// of the deferring one rested on the fact that the data phase goes under an occupied journal:
// since the journal is one, the deferring one is one too. The journal is no longer occupied for the time of
// the data (`talaria.jrnfree=1`), and the uniqueness has to be kept by ourselves —
// with a claim under the FS lock, which is removed ONLY together with the portion
// (`wb_defer_finish`) rather than at the exit from the data phase.
static mut DEFER_BUF: u64 = 0;
static mut DEFER_SLOTS: [u32; WB_BLOCKS] = [0; WB_BLOCKS];
static mut DEFER_K: usize = 0;
static mut WB_DEFER_TASK: i64 = -1;
pub static WB_DEFERRED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Occupy the right to defer a portion (under the FS lock). `false` — it is occupied by a neighbour,
/// and then the portion is written synchronously, as before §8.13.
/// SAFETY: E1 — under the FS lock, which is what makes the claim a claim: the check and
/// the store of `WB_DEFER_TASK` are one critical section, so exactly one task wins.
unsafe fn wb_defer_claim() -> bool {
    if core::ptr::read_volatile(core::ptr::addr_of!(WB_DEFER_TASK)) >= 0 { return false; }
    // The -1 was stored by `wb_defer_finish` WITHOUT the FS lock, so the lock does not order
    // its earlier `CWB` clears before what this claim does next: the fence pairs with its
    // Release (a late clear landing over this claim's new `CWB` mark would let a second
    // write of the same block go to the device beside the deferred one).
    core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
    WB_DEFER_TASK = crate::sched::current_id() as i64;
    true
}
/// Remove the claim when deferring did not happen (the portion left synchronously).
/// SAFETY: E1 — under the FS lock; it gives the claim up ONLY if nothing was deferred
/// (`DEFER_K == 0`) and the claim is ours, otherwise a portion in flight would lose the
/// owner that must later wait for it.
unsafe fn wb_defer_unclaim() {
    if DEFER_K == 0 && WB_DEFER_TASK == crate::sched::current_id() as i64 { WB_DEFER_TASK = -1; }
}
/// Is the deferred portion OURS? (the claim lives until `wb_defer_finish`).
/// SAFETY: E1 — under the FS lock.
#[inline] unsafe fn wb_defer_mine() -> bool {
    DEFER_K > 0 && WB_DEFER_TASK == crate::sched::current_id() as i64
}
/// Wait for the deferred portion (usually the transaction has already waited for it) and return
/// its ways out of flight. Without the FS lock (as with the CWB of a synchronous portion).
/// SAFETY: E5 + E7 — and note it does NOT require E1: like the `CWB` of a synchronous
/// portion, the ways are taken out of flight with the FS lock RELEASED, and that is why
/// each `CWB` is cleared with `write_volatile`. What makes it sound is that the slots of
/// the deferred portion belong to this task alone: `DEFER_SLOTS`/`DEFER_K` are filled under
/// the lock by the task that won `wb_defer_claim`, and the claim is held until right here.
unsafe fn wb_defer_finish() {
    if DEFER_K == 0 { return; }
    if !crate::fs::blk_defer_wait() { crate::println!("ext2: an error of the deferred write of the data"); }
    for i in 0..DEFER_K { core::ptr::write_volatile(addr_of_mut!(CWB[DEFER_SLOTS[i] as usize]), false); }
    // The clears above, then the claim's end — in that order for another core (and for the
    // compiler, which may move a plain store of a `static mut` above volatile ones).
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    core::ptr::write_volatile(addr_of_mut!(DEFER_K), 0);
    core::ptr::write_volatile(addr_of_mut!(WB_DEFER_TASK), -1); // the claim is removed together with the portion
}
/// It returns (the blocks written, whether anything selected WAS LEFT unwalked).
///
/// SESSION 39: the second flag is not a decoration. By it `writeback` decides whether to call
/// the second pass over the whole cache, and lying with it would mean NOT WRITING a dirty
/// block while answering "it is written" — that very class of lie because of which the FS and device
/// locks were rewritten in sessions 26 and 31. So the flag is set in all
/// three cases when the pass did not finish something: it did not happen at all, it ran into
/// a slot IN FLIGHT (CWB — its copy is already at the device, but a neighbour managed to make it
/// dirty), and it left by the limit of the portions without reaching the end of the cache.
/// SAFETY: E1 + E3 + E4 + E5 + E7 — entered under the FS lock; the lock is given up only
/// around the device and only where `fs_release_allowed()` says it may be (E4), and taken
/// back before the walk continues. Everything the pass touches in the tables — the copy into
/// the staging buffer, the clearing of `CDIRTY`/`COWN`/`CJRN`, the raising of `CWB` — is
/// done with the lock held and bracketed by `dcnt_del`/`dcnt_add` (E3); only the LOWERING of
/// `CWB` happens with the lock released, volatile (E5, E7). The copy is bounded: `k < chunk
/// <= WB_BLOCKS` and the buffer is `WB_BLOCKS * BSZ` allocated at the mount, with the pass
/// disabled outright unless `BS == BSZ`.
unsafe fn writeback_unlocked(sel: Sel, max_chunks: usize, chunk: usize, cause: WbC) -> (usize, bool) {
    if WB_BUF == 0 || BS != BSZ { return (0, true); }
    let chunk = chunk.min(WB_BLOCKS);
    let mut chunks = 0usize;
    if !fs_release_allowed() { return (0, true); }
    let may_defer = DEFER_BUF != 0 && DEFER_K == 0 && WB_DEFER_TASK == crate::sched::current_id() as i64;
    // The selection is divided into "it fits" and "it fits AND is not in flight": the first is needed
    // in order to notice a skip, the second in order to make it.
    let aged_now = dstamp();
    let matches = |s: usize| -> bool {
        CTAG[s] != 0 && CDIRTY[s] && match sel {
            Sel::All | Sel::DataAll => COWN[s] != 0,
            // a flush by AGE: only what has lain longer than the threshold
            Sel::DataAged(thr) => COWN[s] != 0 && aged(aged_now, CDIRTIME[s], thr),
            Sel::Data(ino) | Sel::Own(ino) => COWN[s] != 0 && COWN[s] == ino,
            Sel::DataNew => COWN[s] != 0 && CNEW[s],
            // a checkpoint: the committed metadata (CJRN) may be put in place
            // without the FS lock — a copy is taken under the lock and equals the version in
            // the journal; the full journal (16 MB) under the lock would cost 130+ ms of the FS.
            Sel::Journaled => COWN[s] == 0 && CJRN[s],
        }
    };
    let selected = |s: usize| -> bool { matches(s) && !CWB[s] };
    let mut left = false;
    let mut written = 0usize;
    let mut s = 0usize;
    let n = ncache();
    loop {
        // THE DETAILS OF THE "DATA" PHASE (session 38). §8.13 defers the last
        // portion, that is, there must be no round trip to the device here — while the phase
        // cost 402 us per call. So the portion is broken down into four items:
        // the search for the slots and the copy into the substitute buffer (under the FS lock), the submission,
        // the handover of the device to a waiter and the return of the FS lock.
        let tcopy = crate::tsc::rdtsc();
        while s < n { if matches(s) { if !CWB[s] { break; } left = true; } s += 1; }
        if s >= n { break; }
        // The device goes under the FS lock, but WITHOUT waiting under it.
        let tdev = crate::tsc::rdtsc();
    let dg = dev_lock_nowait_fs();
    ph_note(11, tdev);
        let mut reqs: [(usize, crate::mem::Pa, u16); 64] = [(0, crate::mem::Pa::frame(0), 0); 64];
        let mut nr = 0usize;
        let mut slots: [u32; WB_BLOCKS] = [0; WB_BLOCKS];
        let mut k = 0usize;
        // the age of the dirty data goes by portion rather than by block: atomics per block would cost
        // more than the instrument itself (step 2 of session 39)
        let now = dstamp_of(tcopy);
        let (mut age_sum, mut age_max) = (0u64, 0u32);
        // §8.13: is this the last portion (does the remainder of the selected fit into it)?
        // Then it will go from its own buffer and may be deferred.
        let mut rem = 0usize;
        if may_defer && DEFER_K == 0 && chunks + 1 < max_chunks {
            let mut t = s;
            while t < n && rem <= chunk { if selected(t) { rem += 1; } t += 1; }
        }
        let buf = if rem > 0 && rem <= chunk { DEFER_BUF } else { WB_BUF };
        while s < n && k < chunk && nr < reqs.len() {
            // The selection is computed ONCE per slot: the walk over the cache is 248 us per
            // fsync call (session 38's instrument), and a second predicate here would cost
            // more than the whole second pass won by step 1.
            if matches(s) {
                if CWB[s] { left = true; }                 // the slot is in flight — it will be finished by the second pass
                else {
                let bn = CTAG[s] - 1;
                if COWN[s] != 0 { let a = age_of(now, CDIRTIME[s]); age_sum += a as u64; if a > age_max { age_max = a; } }
                core::ptr::copy_nonoverlapping(crate::mem::kptr::<u8>(CACHE_BASE).add(s * BSZ), crate::mem::kptr::<u8>(buf).add(k * BSZ), BS);
                dcnt_del(s);
                CDIRTY[s] = false; CNEW[s] = false; COWN[s] = 0; CJRN[s] = false; CJOLD[s] = false;
                core::ptr::write_volatile(addr_of_mut!(CWB[s]), true);
                slots[k] = s as u32;
                let lba = (bn as usize) * SPB;
                if nr > 0 && reqs[nr - 1].0 + reqs[nr - 1].2 as usize == lba { reqs[nr - 1].2 += SPB as u16; }
                else { reqs[nr] = (lba, crate::mem::Pa::frame(buf + (k * BSZ) as u64), SPB as u16); nr += 1; STATS_RUNS += 1; }
                STATS_WR += 1; written += 1; k += 1;
                }
            }
            s += 1;
        }
        wbage_note(cause, k, age_sum, age_max);
        ph_note(13, tcopy);
        let depth = fs_unlock_outermost();
        let tsend = crate::tsc::rdtsc();
        let mut deferred = false;
        if buf == DEFER_BUF && k == rem && k > 0 {
            deferred = crate::fs::blk_write_many_defer(&reqs[..nr]);
            if deferred {
                for i in 0..k { DEFER_SLOTS[i] = slots[i]; }
                DEFER_K = k;
                WB_DEFERRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        } else if !crate::fs::blk_write_many(&reqs[..nr]) { crate::println!("ext2: an error of the group write"); }
        ph_note(14, tsend);
        WB_CHUNKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if !deferred { for i in 0..k { core::ptr::write_volatile(addr_of_mut!(CWB[slots[i] as usize]), false); } }
        drop(dg);                        // the device comes first: the one holding it does not ask for the FS
        let thand = crate::tsc::rdtsc();
        crate::fs::dev_handoff();        // to a waiter — its turn, otherwise it stands behind the whole flushing
        ph_note(15, thand);
        let trel = crate::tsc::rdtsc();
        fs_relock(depth);
        ph_note(16, trel);
        chunks += 1;
        if DIRTY_N == 0 { break; }
        if chunks >= max_chunks { if s < n { left = true; } break; }   // we did not reach the end of the cache
    }
    (written, left)
}

/// The former flush UNDER the FS lock: the metadata, the journal's checkpoint, and everything — when
/// the lock must not be released or there is no substitute buffer.
/// SAFETY: E1 + E3 + E5 — under the FS lock for the WHOLE pass, the device included: the
/// blocks go to the driver straight out of their cache slots (no staging copy), which is
/// sound only because nobody who could rewrite or re-tag them can run while we hold the lock.
unsafe fn writeback_locked(sel: Sel, cause: WbC) -> usize {
    if DIRTY_N == 0 { return 0; }
    let mergeable = BS == BSZ;
    let mut reqs: [(usize, crate::mem::Pa, u16); 64] = [(0, crate::mem::Pa::frame(0), 0); 64];
    let mut nr = 0usize;
    let mut written = 0usize;
    let (mut run_bn, mut run_slot, mut run_len) = (0u32, 0usize, 0usize);
    // The age goes here too: the second pass carries the data away when there is no substitute buffer,
    // the block is smaller than a page or the first pass did not get there (`left`). The instrument,
    // which does not see a part of its population, answers "the age is small" the more often the
    // worse the path by which the block left.
    let now = dstamp();
    let (mut age_sum, mut age_max, mut age_k) = (0u64, 0u32, 0usize);
    for s in 0..ncache() {
        let take = CTAG[s] != 0 && CDIRTY[s] && match sel {
            Sel::All => true,
            Sel::Data(ino) => COWN[s] == ino,
            Sel::DataAll => COWN[s] != 0,
            Sel::DataAged(thr) => COWN[s] != 0 && aged(now, CDIRTIME[s], thr),
            Sel::DataNew => COWN[s] != 0 && CNEW[s],
            Sel::Journaled => CJRN[s],
            Sel::Own(ino) => COWN[s] == ino || COWN[s] == 0,
        };
        if !take {
            if run_len > 0 {
                reqs[nr] = ((run_bn as usize) * SPB, crate::mem::Pa::frame(CACHE_BASE + (run_slot * BSZ) as u64), (run_len * SPB) as u16);
                nr += 1; STATS_RUNS += 1; run_len = 0;
            }
        } else {
            let bn = CTAG[s] - 1;
            // The range's ceiling: the number of sectors in a request is a u16, and a range of
            // 8192+ blocks (32 MB in a row — an ordinary size for a sequential
            // file) overflowed it, leaving the blocks marked clean but
            // NOT WRITTEN.
            if run_len > 0 && mergeable && bn == run_bn + run_len as u32 && s == run_slot + run_len && run_len < 4096 {
                run_len += 1;
            } else {
                if run_len > 0 {
                    reqs[nr] = ((run_bn as usize) * SPB, crate::mem::Pa::frame(CACHE_BASE + (run_slot * BSZ) as u64), (run_len * SPB) as u16);
                    nr += 1; STATS_RUNS += 1;
                }
                run_bn = bn; run_slot = s; run_len = 1;
            }
            if COWN[s] != 0 { let a = age_of(now, CDIRTIME[s]); age_sum += a as u64; if a > age_max { age_max = a; } age_k += 1; }
            dcnt_del(s);
            CDIRTY[s] = false; CNEW[s] = false; COWN[s] = 0; CJRN[s] = false; CJOLD[s] = false;
            STATS_WR += 1; written += 1;
        }
        if nr >= reqs.len() - 1 { if !crate::fs::blk_write_many(&reqs[..nr]) { crate::println!("ext2: an error of the group write"); } nr = 0; }
    }
    if run_len > 0 {
        reqs[nr] = ((run_bn as usize) * SPB, crate::mem::Pa::frame(CACHE_BASE + (run_slot * BSZ) as u64), (run_len * SPB) as u16);
        nr += 1; STATS_RUNS += 1;
    }
    if nr > 0 && !crate::fs::blk_write_many(&reqs[..nr]) { crate::println!("ext2: an error of the group write"); }
    wbage_note(cause, age_k, age_sum, age_max);
    written
}

// ── The file system's lock ───────────────────────────────────────────────────
//
// The ext2 layer keeps ITS OWN state: the block cache (CTAG/CDIRTY), the bitmaps,
// the table of open files, the journal. All of that was written when the core was one, and
// stayed without any protection — while the workload's threads read files simultaneously from
// FOUR cores. Two cores raced into the virtio queue, and that is visible directly
// in the log: "ext2: an error reading the block N" in the middle of the JVM's work. An unnoticed portion
// of such errors is worse than the error itself: a wrong block could have arrived, and the JIT
// assembles code from it.
//
// The lock is one for the whole FS and RECURSIVE: ext2's operations call each other
// (open -> lookup -> a read of a directory), and forbidding that would mean rewriting
// the layer.
//
// THE MUTUAL EXCLUSION IS BY THE CORE (a CAS on `FS_OWNER`), THE RECURSION IS BY THE TASK
// (`FS_OWNER_TASK`). Previously both were recognised by the core's number, with
// the justification "the syscalls go with IF=0 and do not fall asleep inside ext2, which means the owner
// does not migrate". The justification is incomplete — that is admitted right there, a line below:
// a #PF on a user buffer INSIDE the section allows the interrupts. And if the owner is
// preempted and carried to another core, the recognition by the core breaks in both directions
// and both breakages are silent:
//   * the owner, having continued on the new core, does not recognise ITS OWN lock and stands waiting for
//     itself — forever (from the outside: "the task 11 has been holding the lock for 107 s, while it is
//     ready/running", mongodb/insert on EC2, 7 September);
//   * a foreign task that got onto the owner's FORMER core recognises the lock as its own
//     and enters ext2 WITHOUT it (that is what `FS_REENTER_ALIEN` counts).
// The recognition of the recursion by the task closes both cases; the counter
// `FS_RECUR_MIGRATED` says whether a migration of the owner happened at all.
static FS_OWNER: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);
static mut FS_DEPTH: u32 = 0;
/// How many times a wait for the lock went over a second (each one with a diagnosis into the
/// console; normally zero).
pub static FS_LOCK_TIMEOUT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many times the lock was BYPASSED — only from a panic (see `fs_lock`).
pub static FS_LOCK_BYPASS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub struct FsGuard(bool);
impl FsGuard {
    /// The lock is really ours (`false` — only a bypass from a panic).
    pub fn held(&self) -> bool { self.0 }
}
impl Drop for FsGuard {
    fn drop(&mut self) {
        if !self.0 { return; }
        // SAFETY: E4 — we are the owner (this guard is proof of it), and the owner is the
        // only writer of `FS_DEPTH`. The order at zero matters and is explained below.
        unsafe {
            FS_DEPTH -= 1;
            if FS_DEPTH == 0 {
                fs_lock_released();
                // The task is removed FIRST: while `FS_OWNER` is still occupied, nobody will
                // take the lock and see a foreign `FS_OWNER_TASK`.
                FS_OWNER_REAL.store(false, core::sync::atomic::Ordering::SeqCst);
                FS_OWNER_TASK.store(-1, core::sync::atomic::Ordering::SeqCst);
                FS_OWNER.store(-1, core::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

/// Take the FS lock (recursively). The wait IS BOUNDED in time: a crash dump
/// flushes the trace to the disk when a neighbouring core already stands dead, and an eternal
/// wait would eat the very dump for whose sake everything is done.
/// THE FS LOCK's counters. The lock on the whole file system is one and is taken with IF=0,
/// so "an FS syscall took 2 ms" may mean both "that much work" and "that much
/// waiting for a neighbour" — by the syscall's time that is indistinguishable, while it is cured by the opposite.
pub static FS_LOCK_TAKES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static FS_LOCK_CONT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static FS_LOCK_WAIT_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static FS_LOCK_HELD_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static FS_HELD_SINCE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static FS_OWNER_TASK: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(-1);
/// How many times ext2 was entered "recursively" by A FOREIGN task — that is, without holding the lock.
pub static FS_REENTER_ALIEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many times THE OWNER of the lock turned up on ANOTHER core — that is, it was
/// preempted and carried away in the middle of a critical section. With the old recognition by the core
/// every such case was an eternal wait for oneself.
pub static FS_RECUR_MIGRATED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// THE TASK'S NUMBER IS NOT ALWAYS MEANINGFUL. `sched::current_id()` returns the task of ITS OWN
/// core, while if there is no scheduler on it yet (the early boot, an AP under a bare
/// hook, the gates `fslockdiag`/`devlockdiag`) it returns the common `CURRENT`, THE SAME on all
/// the cores. Recognising the recursion by it is not allowed: two different cores would get one
/// number, and the second would enter ext2 "recursively" without the lock. So the ownership
/// by a task is recorded only when the task is A REAL one (its own core knows it);
/// in the other cases the former recognition by the core remains — there it is
/// correct, because without a scheduler nobody migrates.
static FS_OWNER_REAL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The real task number of this core, or None — there is no scheduler on it.
fn real_task() -> Option<usize> {
    let t = crate::percpu::cur_task();
    if t == u64::MAX || t as usize >= crate::sched::NTASKS { None } else { Some(t as usize) }
}
/// Does THE ONE WHO ASKS hold the FS lock (the task — if it is a real one, otherwise the core).
fn fs_held_by_me() -> bool {
    use core::sync::atomic::Ordering::SeqCst;
    // SAFETY: E4 — read together with `FS_OWNER`, which is the pairing that makes a stale
    // value harmless: it can only turn a "mine" into a "not mine", never the reverse.
    if unsafe { FS_DEPTH } == 0 || FS_OWNER.load(SeqCst) < 0 { return false; }
    match (FS_OWNER_REAL.load(SeqCst), real_task()) {
        (true, Some(t)) => FS_OWNER_TASK.load(SeqCst) == t as i64,
        _ => FS_OWNER.load(SeqCst) == crate::percpu::cpu_id() as i32,
    }
}
/// How many times a waiter for the lock gave up the processor instead of spinning (it is visible in `stat:`).
pub static FS_LOCK_YIELDS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many idle turns of the spin we tolerate before giving up the core. The threshold is low:
/// several hundred cycles are enough to wait out a typical take "in the cache", while
/// everything longer is already a trip to the device, and there is no point in waiting for that with a spin.
const SPIN_BEFORE_YIELD: u32 = 256;
/// (the portions of the flush without the lock, the flushes by the balance from a write, the waits for the device).
pub fn wb_stats() -> (u64, u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (WB_CHUNKS.load(Relaxed), WB_BALANCE.load(Relaxed), WB_DEVWAIT.load(Relaxed))
}
/// (the takes, with a wait, the wait in us, the holding in us, longer than a second, the foreign entries).
pub fn lock_stats() -> (u64, u64, u64, u64, u64, u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    let hz = crate::tsc::calibrate_cached().max(1);
    (FS_LOCK_TAKES.load(Relaxed), FS_LOCK_CONT.load(Relaxed),
     FS_LOCK_WAIT_TSC.load(Relaxed) / (hz / 1_000_000).max(1),
     FS_LOCK_HELD_TSC.load(Relaxed) / (hz / 1_000_000).max(1),
     FS_LOCK_TIMEOUT.load(Relaxed), FS_REENTER_ALIEN.load(Relaxed), FS_LOCK_YIELDS.load(Relaxed))
}

/// THE RULE: the lock is taken by EVERY entry into ext2 that touches the inodes, the directories,
/// the block cache or the device — including THE READING ones. A reader here is not harmless:
/// the `lookup` path goes through `cblk`, and that one evicts foreign slots and writes dirty
/// blocks to the disk. A missed lock on one READ corrupts the file system just as
/// reliably as a missed one on a write.
///
/// And that is how it was: `exists`, `is_dir_path`, `fd_size`, `fd_is_dir`, `dup_handle` and
/// `jrn_checkpoint` (which even WRITES) went without the lock. On one core that
/// never showed, on two it broke at once: the gate `fsbig` with `talaria.smp=1` is
/// green, with `smp=2` and `smp=4` a `mkdir` returns an EEXIST for a name that does not
/// exist, an `open` returns an ENOTDIR for a directory, files "disappear" after a `rename`. With
/// ClickHouse on a table of 10 million rows (there an insert is cut into pieces, and they are
/// merged in the background) that gave "Cannot open file s.cmrk2", "Cannot read all data"
/// and a broken granule in about half of the runs.
pub fn fs_lock() -> FsGuard {
    use core::sync::atomic::Ordering;
    let me = crate::percpu::cpu_id() as i32;
    let owner_cpu = FS_OWNER.load(Ordering::SeqCst);
    if fs_held_by_me() {
        // The recursion is OURS. If the owner's core is no longer ours meanwhile — the owner was
        // preempted and carried away in the middle of the section: with the old recognition by the core we would
        // now stand waiting for a lock that we hold ourselves.
        if owner_cpu != me {
            FS_RECUR_MIGRATED.fetch_add(1, Ordering::Relaxed);
            FS_OWNER.store(me, Ordering::SeqCst);   // the owner has moved together with the task
        }
        // SAFETY: E4 — `fs_held_by_me()` has just said the lock is ours, so we are the owner.
        unsafe { FS_DEPTH += 1; }
        return FsGuard(true);
    }
    // The core says "the lock is mine", while another task holds it — which means the owner was carried away from
    // this core, while we would have entered ext2 WITHOUT the lock. We count it and WAIT, like foreign ones.
    // SAFETY: E4 — a diagnostic read; it only decides whether to bump a counter.
    if owner_cpu == me && unsafe { FS_DEPTH } > 0 { FS_REENTER_ALIEN.fetch_add(1, Ordering::Relaxed); }
    FS_LOCK_TAKES.fetch_add(1, Ordering::Relaxed);
    let hz = crate::tsc::hz();
    let limit = if hz != 0 { hz } else { 3_000_000_000 }; // ~1 s until the first diagnosis
    let start = crate::tsc::rdtsc();
    let mut spun = false;
    let mut spins = 0u32;
    let mut reports = 0u32;
    let mut next_report = limit;
    loop {
        if FS_OWNER.compare_exchange(-1, me, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            // SAFETY: E4 — the compare_exchange above has just made us the owner.
            unsafe { FS_DEPTH = 1; }
            let now = crate::tsc::rdtsc();
            if spun {
                FS_LOCK_CONT.fetch_add(1, Ordering::Relaxed);
                FS_LOCK_WAIT_TSC.fetch_add(now.wrapping_sub(start), Ordering::Relaxed);
            }
            FS_HELD_SINCE.store(now, Ordering::Relaxed);
            // SAFETY: E4 + E1 — we own the lock from this point, so the trace's baseline is
            // ours to write.
            unsafe { FS_TRACE_RD0 = STATS_RD; FS_TRACE_WR0 = STATS_WR; FS_TRACE_FL0 = STATS_FLUSH; FS_TRACE_CH0 = WB_CHUNKS.load(Ordering::Relaxed);
                     FS_TRACE_EM0 = EVICT_META.load(Ordering::Relaxed); FS_TRACE_ED0 = EVICT_DATA.load(Ordering::Relaxed);
                         FS_TRACE_JS0 = JSB_FLUSH.load(Ordering::Relaxed); }
            let rt = real_task();
            FS_OWNER_REAL.store(rt.is_some(), Ordering::SeqCst);
            // The number is always printed in the diagnostics; in the recognition of the recursion it
            // participates only together with `FS_OWNER_REAL`.
            FS_OWNER_TASK.store(rt.unwrap_or_else(crate::sched::current_id) as i64, Ordering::SeqCst);
            return FsGuard(true);
        }
        spun = true;
        // DO NOT BURN THE CORE IN A WAIT. The lock is one for the whole FS and is held through
        // an access to the device — hundreds of microseconds, and on a cache miss
        // milliseconds. All that time a waiter spun with IF=0, that is, its core did
        // NOTHING, although ready threads of the same workload stood next to it: on
        // a large table that came out as 30 % of the takes with a wait and seconds of pure
        // spin over a run. We give the processor up if there is somebody to run on the core —
        // exactly as the futex wait tier does it; we will come back by a rotation and
        // recheck. A short spin before that is left deliberately: the overwhelming
        // majority of the takes meet no resistance at all, and a context
        // switch there would be more expensive than the wait itself.
        //
        // Giving it up is possible only while NOT OWNING the lock: an owner with IF=0 is not preempted
        // and does not migrate, so the recognition of the owner by the core's number stays
        // correct. A waiter is not an owner by definition.
        spins += 1;
        if spins > SPIN_BEFORE_YIELD {
            spins = 0;
            let cpu = crate::percpu::cpu_id();
            if crate::sched::cpu_has_other_runnable(cpu, crate::sched::current_id()) {
                FS_LOCK_YIELDS.fetch_add(1, Ordering::Relaxed);
                crate::sched::yield_origin(crate::sched::YO_FS); crate::sched::yield_now();
                continue;
            }
            // There is nobody to give the core to, while the lock is held by the one that waits for the disk: without a window
            // that is a tick's gap for its whole command (session 35). We have no lock —
            // the window risks nothing here.
            crate::fs::io_window();
        }
        // A neighbour that removes a mapping waits for OUR acknowledgement of a TLB flush, while
        // the spin here goes with the interrupts forbidden — the call will not reach us.
        // Without draining the mailbox the initiator waits 10 ms and GIVES UP, leaving us with
        // a stale TLB entry (the counter `did not acknowledge` in `stat`). The check
        // costs two atomic reads, and that is a price of integrity rather than of speed.
        crate::paging::tlb_service();
        core::hint::spin_loop();
        let waited = crate::tsc::rdtsc().wrapping_sub(start);
        if waited > next_report {
            // DO NOT GIVE UP SILENTLY (the same rule as at `fs::dev_lock`). Previously
            // after a second the wait returned an empty guard and the caller
            // entered ext2 WITHOUT the lock — "better to risk it than to stand forever".
            // But an entry without the lock corrupts the FS just as reliably as its absence
            // altogether (see the rule above about the readers), and it does that silently: from the outside
            // only a foreign block in a foreign file is visible hours later. A hanging
            // machine with the culprit's name in the console is better. A bypass goes only to a panic:
            // it needs the trace on the disk, while the order no longer matters.
            if reports == 0 { FS_LOCK_TIMEOUT.fetch_add(1, Ordering::Relaxed); }
            if crate::panicking() {
                FS_LOCK_BYPASS.fetch_add(1, Ordering::Relaxed);
                FS_LOCK_CONT.fetch_add(1, Ordering::Relaxed);
                FS_LOCK_WAIT_TSC.fetch_add(waited, Ordering::Relaxed);
                return FsGuard(false);
            }
            let held_ms = crate::tsc::rdtsc().wrapping_sub(FS_HELD_SINCE.load(Ordering::Relaxed)) / (hz / 1000).max(1);
            // NAME NOT ONLY THE NUMBER BUT ALSO THE HOLDER'S STATE. The task's number
            // answers "who", while a hang is always "why does it not
            // release": it runs and works long, it sleeps on a word, or it
            // is dead altogether (and then nobody will release the lock). On EC2 that was
            // analysed blindly: `the task 11 holds it, already 191 s` — and not a word
            // about whether it is alive (mongodb/insert, 7 September).
            let ot = FS_OWNER_TASK.load(Ordering::SeqCst);
            if ot >= 0 && (ot as usize) < crate::sched::NTASKS {
                let t = ot as usize;
                // WHERE IT IS EXECUTING RIGHT NOW rather than "which core it is assigned to".
                // `cpu_of` returns THE LAYOUT (the balancer changes it without looking
                // at whether the task is running), and the line "core 1 holds it, while it is
                // ready/running on core 3" read as "the owner was carried away",
                // although it meant only "it has been assigned to core 3". We ask the cores
                // themselves: whoever has it as the current one — there it runs.
                let mut on = -1i32;
                for c in 0..crate::percpu::MAXCPU {
                    if crate::percpu::cur_task_of(c) == t as u64 { on = c as i32; break; }
                }
                let sys = { let v = crate::syscall::LAST_SYS[t].load(Ordering::Relaxed);
                            if v == u64::MAX { -1 } else { (v >> 48) as i64 } };
                crate::println!("ext2: the FS lock did not give in within {} s: core {} holds it (the task {} \"{}\", already {} ms), it is {}; it RUNS on \
                    core {} (it is assigned to {}), the last syscall {}; the core's yields {}, the foreign entries {}, the owner moved {}; we keep waiting \
                    (give-ups 0)",
                    waited / hz.max(1), FS_OWNER.load(Ordering::SeqCst), t, crate::obs::comm(t), held_ms,
                    if crate::sched::task_dead(t) { "DEAD" } else if crate::sync::waiting_on(t) != 0 { "ASLEEP on a word (only the one that writes into the \
                        word will wake it)" } else { "ready" },
                    on, crate::sched::cpu_of(t), sys,
                    FS_LOCK_YIELDS.load(Ordering::Relaxed), FS_REENTER_ALIEN.load(Ordering::Relaxed),
                    FS_RECUR_MIGRATED.load(Ordering::Relaxed));
            } else {
                crate::println!("ext2: the FS lock did not give in within {} s: core {} holds it (the task {}, already {} ms), the core's yields {}; we keep \
                    waiting (give-ups 0)",
                    waited / hz.max(1), FS_OWNER.load(Ordering::SeqCst), ot, held_ms,
                    FS_LOCK_YIELDS.load(Ordering::Relaxed));
            }
            reports += 1;
            next_report = waited + limit * 10;
        }
    }
}
/// ── WAITING FOR THE DEVICE WITHOUT THE FS LOCK ──────────────────────────────
///
/// The FS lock protects THE STATE, while waiting for the disk has nothing to do with the
/// state. Previously the device's barrier (46 ms on this stand, 422 of them over a run)
/// was held under the FS lock, and together with it even the HITS in the block cache stalled —
/// 90 % of the accesses. Hence the tail of a mongodb insert of 136 ms against Linux's 0.5 on
/// the same disk.
///
/// Now for the time of an access to the device the FS lock IS RELEASED, while the order
/// is kept by the device's lock (`fs::dev_lock`) and the rule: the one holding the device
/// DOES NOT ASK for the FS lock. A cycle does not arise — the one inside the device waits
/// only for the hardware.
///
/// We release ONLY THE OUTERMOST entry (`FS_DEPTH == 1`): a nested call has
/// an operation above it that counts on the continuity of its section,
/// and releasing the lock on its behalf is not allowed.
/// `talaria.fsdevlock=0` — return the former behaviour: hold the FS lock for
/// the time of waiting for the device too.
///
/// IT IS ON BY DEFAULT. When the mechanism appeared, it gave nothing: the block
/// layer worked with a queue OF DEPTH ONE, and while a barrier was going, a read did not get through
/// regardless of the locks — there was no point in releasing the lock. Now the queue has
/// a depth (see `fs::io_one`, `fs::blk_flush`), foreign commands fly past
/// the barrier, and the release of the lock has become what it was done for: the hits in the block
/// cache (90 % of the accesses) no longer wait for somebody else's fsync.
static FS_DEVREL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
pub fn devlock_from_cmdline(cmdline: &str) {
    if crate::cmdline::value(cmdline, "talaria.fsdevlock") == Some("0") {
        FS_DEVREL.store(false, core::sync::atomic::Ordering::SeqCst);
        crate::println!("ext2: the FS lock is held during the wait for the device too (talaria.fsdevlock=0 — as it was)");
    }
}

/// May this task release the FS lock for the time of the device (the same condition
/// as at `fs_unlock_outermost`, without the release itself).
/// SAFETY: E4 — `FS_MAY_REL[t]` is this task's own cell.
fn fs_release_allowed() -> bool {
    use core::sync::atomic::Ordering;
    if !FS_DEVREL.load(Ordering::Relaxed) { return false; }
    let t = crate::sched::current_id();
    // SAFETY: E4 — our own cell of the per-task permission table.
    if t >= crate::sched::NTASKS || !unsafe { FS_MAY_REL[t] } { return false; }
    fs_held_by_me()
}
/// SAFETY: E4 — `FS_MAY_REL[t]` is this task's own cell, and `FS_DEPTH` is written here
/// only by the owner, which `fs_held_by_me()` has just confirmed we are.
fn fs_unlock_outermost() -> u32 {
    use core::sync::atomic::Ordering;
    if !FS_DEVREL.load(Ordering::Relaxed) { return 0; }
    let t = crate::sched::current_id();
    // SAFETY: E4 — our own cell of the per-task permission table.
    if t >= crate::sched::NTASKS || !unsafe { FS_MAY_REL[t] } { return 0; }
    if !fs_held_by_me() { return 0; }
    // SAFETY: E4 — `fs_held_by_me()` just confirmed we are the owner, the only writer.
    let depth = unsafe { let d = FS_DEPTH; FS_DEPTH = 0; d };
    if depth == 0 { return 0; }
    fs_lock_released();
    FS_OWNER_REAL.store(false, Ordering::SeqCst);
    FS_OWNER_TASK.store(-1, Ordering::SeqCst);
    FS_OWNER.store(-1, Ordering::SeqCst);
    FS_DEV_RELEASED.fetch_add(1, Ordering::Relaxed);
    depth
}
/// Return the lock released by `fs_unlock_outermost`. That cannot be done with a guard:
/// the ownership stayed with the caller (its own `FsGuard` will remove
/// the depth at the exit), so the new guard is deliberately forgotten.
/// SAFETY: E4 — `fs_lock()` has just made us the owner again, and the owner is the only
/// writer of `FS_DEPTH`.
fn fs_relock(depth: u32) {
    if depth == 0 { return; }
    core::mem::forget(fs_lock());        // it sets the depth to 1
    // SAFETY: E4 — `fs_lock()` on the line above has made us the owner again.
    unsafe { FS_DEPTH = depth; }         // ...and returns it to the other levels
}

/// WHO MAY RELEASE THE LOCK FOR THE TIME OF THE DISK — A PERMISSION rather than a guess.
///
/// Releasing the FS lock in the middle of an operation is allowed exactly when NOT A SINGLE frame
/// above on the stack holds a state that will outlive the release. Deducing that
/// from the lock's depth is impossible: `fsync` comes in through its own wrapper (the depth is 2), while
/// the eviction of a slot from `cblk` — on the contrary — calls a journal transaction FROM THE MIDDLE
/// of a slot's replacement, where releasing is not allowed for a single cycle.
///
/// So the permission is given explicitly and BY TASK: the independent synchronisation
/// operations (`flush_all`, `fsync`, `msync`) set it, an eviction removes it.
///
/// SAFETY: E4 — the array is indexed by task and each task touches only its own cell, so it
/// needs no lock of its own. The index is `sched::current_id()`, which the scheduler only
/// ever sets to a real task number or to `u64::MAX` (and that case falls back to `CURRENT`,
/// also a real number) — so it is always `< NTASKS`. Note that the guard is SPELT TWO WAYS
/// in this file: here it clamps (`.min(NTASKS - 1)`), while `fs_release_allowed` and
/// `fs_unlock_outermost` refuse outright. Neither spelling can fire today; if one ever did,
/// they would part company in a way that matters — a refusal costs a task its right to
/// release the lock, while a clamp would flip the permission of the LAST task, and handing
/// task `NTASKS - 1` the right to release the FS lock in the middle of a slot's replacement
/// is precisely the corruption E4 exists to prevent.
static mut FS_MAY_REL: [bool; crate::sched::NTASKS] = [false; crate::sched::NTASKS];
struct MayRelease(usize, bool);
impl MayRelease {
    fn set(v: bool) -> Self {
        let t = crate::sched::current_id().min(crate::sched::NTASKS - 1);
        // SAFETY: E4 — this task's own cell (see the note at `FS_MAY_REL` on the clamp).
        let old = unsafe { FS_MAY_REL[t] };
        // SAFETY: E4 — as above.
        unsafe { FS_MAY_REL[t] = v; }
        MayRelease(t, old)
    }
}
impl Drop for MayRelease {
    /// SAFETY: E4 — the cell restored is the one this guard took, `self.0`, which is the
    /// task that created it.
    fn drop(&mut self) { unsafe { FS_MAY_REL[self.0] = self.1; } }
}
/// How many times the FS lock was released for the time of waiting for the device (the diagnostics:
/// exactly that many operations stopped holding the whole FS).
pub static FS_DEV_RELEASED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// THE DEVICE'S LOCK WITHOUT WAITING UNDER THE FS LOCK. It is occupied — which means a neighbour is giving
/// a portion of data or is running a journal transaction with barriers: those are tens
/// of milliseconds for which THE WHOLE FS would stall (on a throttled disk a neighbour
/// stood 15-60 ms at every such meeting). We release the FS (if it is allowed),
/// wait it out from the outside, take the FS back and TRY AGAIN — rather than wait a second
/// time under the lock: between "it was released" and "we took it" the device manages to go
/// to a third one. When releasing is not allowed, we wait as before.
/// SAFETY: E1 — under the FS lock, and it keeps the kernel's lock order FS -> device: the
/// device's lock is taken while holding the FS one, never the other way round.
unsafe fn dev_lock_nowait_fs() -> crate::fs::DevGuard {
    for _ in 0..4096 {
        if let Some(g) = crate::fs::dev_trylock() { return g; }
        WB_DEVWAIT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let depth = fs_unlock_outermost();
        if depth == 0 { break; }
        crate::fs::dev_wait_free();
        fs_relock(depth);
    }
    crate::fs::dev_lock()
}

/// THE DEVICE'S BARRIER WITHOUT THE FS LOCK. A barrier does not touch a single byte of the state —
/// it waits for the hardware — so there is no point in holding the FS lock under it.
/// SAFETY: E1 — under the FS lock (it is the FS that decides a barrier is due).
unsafe fn dev_barrier() { dev_barrier_src(BAR_OTHER) }
/// Who asks for the barriers (the diagnostics: on a gp3-class disk a barrier is 15-30 ms, and
/// their number per fsync decides everything).
pub static BAR_SRC: [core::sync::atomic::AtomicU64; 6] = [const { core::sync::atomic::AtomicU64::new(0) }; 6];
const BAR_DATA: usize = 0; const BAR_TXN: usize = 1; const BAR_CKPT: usize = 2; const BAR_JSB: usize = 3; const BAR_RECOV: usize = 4;
    const BAR_OTHER: usize = 5;
/// SAFETY: E1 — under the FS lock; the statistics it keeps per source are module statics
/// like the rest.
unsafe fn dev_barrier_src(src: usize) {
    BAR_SRC[src].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // A barrier does NOT need the device's lock (see `fs::flush_lock`): everything of ours has already been
    // given to the device, while it is guarded from simultaneous barriers by its own lock.
    let depth = fs_unlock_outermost();   // we release the state
    crate::fs::blk_flush();
    STATS_FLUSH += 1;
    fs_relock(depth);
}
/// Does THE CURRENT task hold the FS lock (see `fs_lock`): the owner must not be given the core
/// up — the block layer asks about that. The question is exactly about the task: the owner's
/// core changes if it was preempted in the middle of a section.
pub fn fs_owned_here() -> bool { fs_held_by_me() }
/// Does the task `t` hold the FS lock — a question FROM A TICK, from any core (`fs_held_by_me`
/// asks about itself and is therefore no good for a tick). It is needed by the mode `talaria.iowin=1`:
/// the owner opened an interrupt window, and the tick must return to it.
pub fn fs_owner_task_is(t: usize) -> bool {
    use core::sync::atomic::Ordering::SeqCst;
    // SAFETY: E4 + E7 — a diagnostic read by a core that owns nothing, hence volatile.
    let depth = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(FS_DEPTH)) };
    depth > 0 && FS_OWNER_REAL.load(SeqCst) && FS_OWNER_TASK.load(SeqCst) == t as i64
}

/// Mark the release of the lock (it is called from the outer guard's `Drop`).
fn fs_lock_released() {
    use core::sync::atomic::Ordering::Relaxed;
    let t0 = FS_HELD_SINCE.load(Relaxed);
    if t0 == 0 { return; }
    let held = crate::tsc::rdtsc().wrapping_sub(t0);
    FS_LOCK_HELD_TSC.fetch_add(held, Relaxed);
    if held > FS_LOCK_HELD_MAX.load(Relaxed) { FS_LOCK_HELD_MAX.store(held, Relaxed); }
    // THE TRACE OF THE LONG HOLDINGS (talaria.fstrace=<ms>). The total "we held" does not
    // name the culprit; a line per every holding longer than the threshold does: what
    // the task was doing (the last syscall) and what that resulted in on the device
    // (the reads/writes/barriers during the holding). The printing goes with IF=0 —
    // it is an instrument for an analysis rather than for production.
    let thr = FS_TRACE_TSC.load(Relaxed);
    if thr != 0 && held > thr && FS_TRACE_LEFT.load(Relaxed) > 0 {
        FS_TRACE_LEFT.fetch_sub(1, Relaxed);
        let hz = crate::tsc::calibrate_cached().max(1);
        let t = crate::sched::current_id();
        let sc = if t < crate::sched::NTASKS { crate::syscall::LAST_SYS[t].load(Relaxed) >> 48 } else { u64::MAX };
        // SAFETY: E1 — printed by the task that has just held the lock, from its own baseline.
        unsafe {
            crate::println!("ext2: the FS lock was held for {} us: the task {} the syscall {}, under the lock reads {} writes {} barriers {} portions {} \
                evictions of metadata {} data blocks {} journal flushes {}, the flush's depth {}",
                held / (hz / 1_000_000).max(1), t, sc,
                STATS_RD - FS_TRACE_RD0, STATS_WR - FS_TRACE_WR0, STATS_FLUSH - FS_TRACE_FL0,
                WB_CHUNKS.load(Relaxed) - FS_TRACE_CH0,
                EVICT_META.load(Relaxed) - FS_TRACE_EM0, EVICT_DATA.load(Relaxed) - FS_TRACE_ED0, JSB_FLUSH.load(Relaxed) - FS_TRACE_JS0,
                core::ptr::read_volatile(core::ptr::addr_of!(DIRTY_N)));
        }
    }
}
pub static FS_LOCK_HELD_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static FS_TRACE_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static FS_TRACE_LEFT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(64);
static mut FS_TRACE_RD0: u64 = 0; static mut FS_TRACE_WR0: u64 = 0; static mut FS_TRACE_FL0: u64 = 0; static mut FS_TRACE_CH0: u64 = 0;
static mut FS_TRACE_EM0: u64 = 0; static mut FS_TRACE_ED0: u64 = 0; static mut FS_TRACE_JS0: u64 = 0;
/// `talaria.fstrace=<ms>` — print every holding of the FS lock longer than the threshold.
pub fn trace_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::value(cmdline, "talaria.fstrace") {
        if let Ok(ms) = v.parse::<u64>() {
            let hz = crate::tsc::calibrate_cached().max(1);
            FS_TRACE_TSC.store(ms * (hz / 1000).max(1), core::sync::atomic::Ordering::Relaxed);
            crate::println!("ext2: a trace of the holdings of the FS lock longer than {} ms (talaria.fstrace)", ms);
        }
    }
}

/// A DEVICE ERROR -> THE FS GOES READ-ONLY (like ext4's `errors=remount-ro`). While
/// the counter of the refusals of a write/a barrier (`fs::WRITE_FAILED`) equals zero — None; after
/// the first refusal every modifying operation gets an EROFS, while an fsync/sync/msync
/// in whose window the refusal happened gets an EIO. Previously a refusal was printed and swallowed:
/// a `write` and an `fsync` returned a success for data that are not on the disk.
fn ro_err() -> Option<i64> {
    use core::sync::atomic::Ordering::SeqCst;
    if crate::fs::WRITE_FAILED.load(SeqCst) == 0 { return None; }
    if !FS_RO_SAID.swap(true, SeqCst) {
        crate::println!("ext2: A DEVICE REFUSAL at a write (the refusals {}) — the FS has been moved to read-only, the changes are rejected (EROFS)",
            crate::fs::WRITE_FAILED.load(SeqCst));
    }
    Some(EROFS)
}
static FS_RO_SAID: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Only for the gate `ioerr`: bring the FS back from read-only (the counter of the refusals is zeroed).
pub fn ro_reset_for_test() { crate::fs::WRITE_FAILED.store(0, core::sync::atomic::Ordering::SeqCst); FS_RO_SAID.store(false,
    core::sync::atomic::Ordering::SeqCst); }
/// Has the counter of the refusals grown since the `snap` — then a synchronisation operation
/// must answer EIO rather than 0.
fn io_err_since(snap: u64) -> i64 {
    if crate::fs::WRITE_FAILED.load(core::sync::atomic::Ordering::SeqCst) != snap { let _ = ro_err(); EIO } else { 0 }
}
/// sync(2)/syncfs(2): like `flush_all`, but with an honest code — an EIO if in the flush's
/// window the device refused.
pub fn sync_all() -> i64 {
    let snap = crate::fs::WRITE_FAILED.load(core::sync::atomic::Ordering::SeqCst);
    flush_all();
    io_err_since(snap)
}
/// Flush all the dirty blocks to the disk (+ the device's barrier). sync/exit/stop.
pub fn flush_all() {
    let _fsg = fs_lock();
    let _rel = MayRelease::set(true); // there is nobody above us — the lock may be released
    if !active() { return; }
    // A cheap exit when there is nothing to synchronise: sync/exit_group are called often
    // (at every finishing fork), while the device's barrier is expensive.
    // (§8.8: JRECOVER — the journal's epoch is open, the recover flag is set; a sync must
    // close it, otherwise an e2fsck -n on the host will refuse the image.)
    // SAFETY: E1 — the lock is taken at the head of this function.
    unsafe { if DIRTY_N == 0 && !JDIRTY && !JRECOVER && mmap_count() == 0 { return; } }
    mmap_harvest(0, 0); // §8.5: the changes through the file mappings are data too
    // SAFETY: E1 + E4 — under the lock, and `MayRelease::set(true)` above says releasing it
    // around the device is allowed here: `flush_all` is an independent synchronisation, no
    // frame above it holds a cache pointer or an open transaction.
    unsafe {
        prea_release(0); // §2.4: the windows' remainders go back into the map BEFORE it is written to the disk
        // §8.11 and for a sync(2): under a foreign transaction — WAIT rather than write
        // the uncommitted metadata in place (that was the same hole as at the fsync).
        if JRN && JBUSY { let t0 = crate::tsc::rdtsc(); jrn_wait_free(); ph_note(8, t0); }
        if JRN && !JBUSY { jrn_commit(u32::MAX); jrn_checkpoint(); return; }
        let n = writeback(Sel::All, WbC::Other);
        // A barrier is needed ONLY if something was written: previously a Flush went unconditionally
        // at every fsync, including the empty ones (a database has thousands of those).
        if n > 0 { dev_barrier(); }
    }
}
/// SAFETY: the statistics exception of `cache_stats` — it is read without the lock by the
/// power-off path only to say how much there was to flush; the flush itself takes the lock.
pub fn dirty_count() -> usize { unsafe { DIRTY_N } }
/// The counters of the write subsystem: (the blocks written, the ranges, the barriers).
/// SAFETY: the statistics exception of `cache_stats`.
pub fn wr_stats() -> (u64, u64, u64) { unsafe { (STATS_WR, STATS_RUNS, STATS_FLUSH) } }
/// §2.4: how many times we had to go into the bitmap for blocks (the preallocation window
/// amortises it: one trip per PREA_RUN blocks of data).
/// SAFETY: the statistics exception of `cache_stats`.
pub fn alloc_scans() -> u64 { unsafe { STATS_ARUN } }
/// The block's bytes in the cache, as a slice.
///
/// SAFETY: E1 + E2 — under the FS lock, and THE LIFETIME `'a` IS CHOSEN BY THE CALLER, so
/// the compiler checks nothing here at all. Two conditions rest entirely on reading:
/// the returned borrow must be finished with before the next access to the cache (any
/// `blk`/`cblk`, and anything calling them), and it must not be alive alongside another
/// borrow of the SAME block. Both hold at every use in this file; E2 says why they matter
/// and where they are easiest to break. The length is `BS`, the slot is `BSZ >= BS`.
unsafe fn blk<'a>(bn: u32) -> &'a mut [u8] { core::slice::from_raw_parts_mut(cblk(bn), BS) }

/// SAFETY: `ACTIVE` is a flag set once by the mount and cleared once by the unmount; it is
/// read volatile (E7) because a core that holds no lock asks "is there a file system at
/// all" — and the answer `false` is the safe one to act on stale.
pub fn active() -> bool { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(ACTIVE)) } }
/// The volume's occupancy for the operator (`df`): (the block's size, the blocks in total, the free
/// blocks, the inodes in total, the free inodes). The superblock's counters are kept on the fly.
pub fn statfs() -> (usize, u32, u32, u32, u32) {
    let _fsg = fs_lock();
    // SAFETY: E1 — the lock is taken right here, so the shared borrow of `SB` cannot meet
    // the `&mut *addr_of_mut!(SB)` that the allocators take under the same lock.
    unsafe { let sb = &*core::ptr::addr_of!(SB); (BS, TOTAL_BLOCKS, rd32(sb, 12), rd32(sb, 0), rd32(sb, 16)) }
}
/// SAFETY: the statistics exception of `cache_stats`.
pub fn stats() -> (u64, u64) { unsafe { (STATS_RD, STATS_WR) } }

/// NUMA: move the block cache onto the node `node`.
///
/// The cache is allocated at the mounting — that is, EARLIER than the kernel learns about the
/// nodes at all: the manifest with the layout lies on the same disk, and reading it without
/// a mounted FS is impossible. The order cannot be rearranged, so the cache is moved
/// after the node is chosen — at the moment when only the reading of the
/// manifest has gone through it.
///
/// We move it ONLY when it is clean: not a single dirty slot, not a single page
/// pinned under an `mmap`. Otherwise the move would mean a loss of unflushed data
/// or a mapping pointing into freed memory — and that is exactly that
/// silent class of errors for whose sake everything else here is written.
/// It returns true if the cache really moved.
pub fn cache_relocate(node: u8) -> bool {
    let _fsg = fs_lock();
    if node == 255 { return false; }
    // SAFETY: E1 + E2 — under the lock, and the move is allowed ONLY on a wholly clean
    // cache: no dirty slot and no pinned page. That is what makes moving `CACHE_BASE`
    // sound at all — every pointer E2 governs is derived from it, so a move with a live
    // mapping would leave a process pointing into freed memory.
    unsafe {
        if CACHE_BASE == 0 || !ACTIVE { return false; }
        if DIRTY_N != 0 { return false; }                 // there are unflushed blocks
        for i in 0..ncache() { if COWN[i] != 0 { return false; } } // a page of the cache is mapped into a user AS
        let pages = (ncache() * BSZ / 4096) as u64;
        let old = CACHE_BASE;
        let new = match crate::mem::alloc_contig_node(pages, node) { Some(b) => b, None => return false };
        if crate::acpi::node_of_phys(new) != node { crate::mem::free_contig(new, pages); return false; }
        CACHE_BASE = new;
        for i in 0..ncache() { CTAG[i] = 0; CDIRTY[i] = false; CNEW[i] = false; } // we do not move the content — it will be reread
        dcnt_reset();
        crate::mem::free_contig(old, pages);
        crate::println!("ext2: the block cache has been moved onto the node {} ({:#x} -> {:#x}, {} MB)",
            node, old, new, pages * 4096 / 1048576);
        true
    }
}

/// REALLOCATE THE BLOCK CACHE FOR THE SIZE FROM THE MANIFEST (`fscache=<MB>`).
///
/// The cache's size is chosen at the mount, while the manifest lies ON THE SAME DISK — that
/// is, learning the workload's wish earlier than the mounting is impossible in principle.
/// So it is not "we will take it into account next time" but an honest reallocation right after
/// the manifest is read, when there is certainly nothing dirty in the cache.
///
/// The conditions are the same as at `cache_relocate`, and for the same reason: not a single
/// dirty slot and not a single page given into a user AS under an `mmap`.
/// The content is not moved — it will be reread from the disk.
///
/// We take the new area BEFORE giving the old one away: if a contiguous piece was not
/// found, it is better to stay with the old cache than without a cache at all.
/// The block cache's memory: from the high area (RAM above 4 GB) when there is one, else the
/// main allocator. The cache is the largest thing the kernel keeps, and below 4 GB it ate what a
/// workload's shared mappings must have there (they are handed out at their identity address
/// from the main allocator): on a `c7g.xlarge` the low part is 844 MB, the cache took 421 of it,
/// and PostgreSQL's 256 MB `shared_buffers` failed with "could not map anonymous shared memory:
/// Out of memory" — the first Graviton scoreboard lost its PostgreSQL side to it. The high area
/// is identity-mapped by 1 GB leaves in every space, so the kernel reaches the cache the same way,
/// and the devices take 64-bit addresses. `free_contig` returns such frames to the area's list.
fn cache_alloc(pages: u64) -> Option<u64> {
    crate::mem::alloc_high(pages, 4096).or_else(|| crate::mem::alloc_any(pages))
}

pub fn cache_resize(mb: usize) -> bool {
    let _fsg = fs_lock();
    // SAFETY: E1 + E2 — under the lock, with the same "clean cache only" condition as
    // `cache_relocate`, and for the same reason. The new area is taken BEFORE the old one
    // is given away, so a failure leaves the cache intact rather than absent.
    unsafe {
        if CACHE_BASE == 0 || !ACTIVE {
            crate::println!("ext2: the fscache= from the manifest IS REJECTED — the FS is not mounted");
            return false;
        }
        let dirty = DIRTY_N;
        if dirty != 0 {
            crate::println!("ext2: the fscache= from the manifest IS REJECTED — the cache has {} dirty blocks", dirty);
            return false;
        }
        for i in 0..ncache() {
            if COWN[i] != 0 {
                crate::println!("ext2: the fscache= from the manifest IS REJECTED — a page of the cache is mapped into the workload");
                return false;
            }
        }
        let was = ncache();
        FSCACHE_MB.store(mb, core::sync::atomic::Ordering::SeqCst);
        let want = cache_pick();
        if want == was {
            crate::println!("ext2: the block cache is already {} MB — there is nothing to change", was * BSZ / 1048576);
            return true;
        }
        let oldpages = (was * BSZ / 4096) as u64;
        let old = CACHE_BASE;
        let new = match cache_alloc((want * BSZ / 4096) as u64) {
            Some(b) => b,
            None => {
                FSCACHE_MB.store(0, core::sync::atomic::Ordering::SeqCst);
                crate::println!("ext2: the fscache={} MB IS REJECTED — there is no contiguous piece, the cache has stayed {} MB",
                    mb, was * BSZ / 1048576);
                return false;
            }
        };
        CACHE_BASE = new;
        CACHE_N.store(want, core::sync::atomic::Ordering::SeqCst);
        CACHE_SETS.store(want / WAYS, core::sync::atomic::Ordering::SeqCst);
        for i in 0..want { CTAG[i] = 0; CDIRTY[i] = false; CNEW[i] = false; COWN[i] = 0; }
        dcnt_reset();
        crate::mem::free_contig(old, oldpages);
        crate::println!("ext2: the block cache from the manifest: {} -> {} MB ({} slots)",
            was * BSZ / 1048576, want * BSZ / 1048576, want);
        true
    }
}

/// Mount ext2: the superblock at byte 1024. false — it is not ext2.
/// SAFETY: E1 — the mount path: it runs before anyone else can be inside the FS, which is
/// what lets it write the geometry (`BS`, `SPB`, the group counts) that everything else
/// afterwards reads as constant, and replay the journal into an empty cache.
pub fn mount() -> bool {
    let _fsg = fs_lock();
    // SAFETY: E1 — the lock is taken on the line above; see the passport of this fn.
    unsafe {
        let mut sb2 = [0u8; 1024];
        if !crate::fs::blk_read(2, &mut sb2) { return false; }
        // The geometry is read and CHECKED in `ext2sb` (it is tested on the host):
        // a superblock that does not add up is a refusal to mount rather than
        // arithmetic that wraps round on the first access.
        let g = match crate::ext2sb::parse(&sb2, BSZ) {
            Ok(g) => g,
            Err(crate::ext2sb::Bad::NotExt2) => return false,
            Err(crate::ext2sb::Bad::BlockTooBig(b)) => {
                crate::println!("ext2: a block of {} > {} is not supported", b, BSZ); return false;
            }
            Err(crate::ext2sb::Bad::Geometry) => {
                crate::println!("ext2: the superblock does not add up (the blocks {}, the first {}, per group {}, the inodes per group {}) — the volume is not \
                    taken",
                    rd32(&sb2, 4), rd32(&sb2, 20), rd32(&sb2, 32), rd32(&sb2, 40));
                return false;
            }
            Err(crate::ext2sb::Bad::Features(f)) => {
                crate::println!("ext2: incompatible features {:#x} (only filetype is needed)", f); return false;
            }
        };
        let b = g.block;
        BS = b; SPB = g.spb;
        TOTAL_BLOCKS = g.total_blocks;
        FIRST_DATA = g.first_data;
        BLOCKS_PER_GROUP = g.blocks_per_group;
        INODES_PER_GROUP = g.inodes_per_group;
        INODE_SIZE = g.inode_size;
        NGROUPS = g.ngroups;
        GDT_BLOCK = g.gdt_block;
        (*core::ptr::addr_of_mut!(SB)).copy_from_slice(&sb2);
        if CACHE_BASE == 0 {
            // We ask for as much as we chose; a contiguous piece was not found — we halve it
            // and try again. A cache smaller than expected is slow, while a refusal
            // of the mount means it does not work at all.
            let mut want = cache_pick();
            loop {
                match cache_alloc((want * BSZ / 4096) as u64) {
                    Some(b) => { CACHE_BASE = b; break; }
                    None if want > 512 => { want = (want / 2 / WAYS) * WAYS; }
                    None => { crate::println!("ext2: there is no memory for the cache"); return false; }
                }
            }
            CACHE_N.store(want, core::sync::atomic::Ordering::SeqCst);
            CACHE_SETS.store(want / WAYS, core::sync::atomic::Ordering::SeqCst);
        }
        // The readahead window (see `prefetch_run`). Without it the FS works, just
        // block by block: the absence of the window is not a mount error.
        if RA_BUF == 0 {
            if let Some(b) = crate::mem::alloc_any((RA_MAX * BSZ / 4096) as u64) { RA_BUF = b; }
        }
        // The flush's substitute buffer (§8.6). Without it the FS works — the flush goes under
        // the lock, as before.
        if WB_BUF == 0 {
            if let Some(b) = crate::mem::alloc_any((WB_BLOCKS * BSZ / 4096) as u64) { WB_BUF = b; }
        }
        // §8.13: the deferred portion's buffer — without it an fsync goes in three round trips.
        if DEFER_BUF == 0 && WB_BUF != 0 {
            if let Some(b) = crate::mem::alloc_any((WB_BLOCKS * BSZ / 4096) as u64) { DEFER_BUF = b; }
        }
        for i in 0..ncache() { CTAG[i] = 0; CDIRTY[i] = false; CNEW[i] = false; }
        for h in 0..MAXOPEN { OF_INO[h] = 0; }
        ACTIVE = true;
        dcnt_reset();
        for i in 0..ncache() { COWN[i] = 0; }
        let (tot, ngr, isz, bsz) = (TOTAL_BLOCKS, NGROUPS, INODE_SIZE, BSZ);
        crate::println!("ext2: it is mounted — the block is {} B, {} blocks ({} MB), {} groups, an inode is {} B, free {} blocks / {} inodes, the cache is {} \
            MB write-back{}",
            b, tot, (tot as usize * b) >> 20, ngr, isz, rd32(&sb2, 12), rd32(&sb2, 16), (ncache() * bsz) >> 20,
            if CACHE_BASE >= 1 << 32 { " (above 4 GB)" } else { "" });
        // §8.4: the journal (a replay when it is non-empty) comes after the cache and
        // the groups' maps are ready: the map of the journal's blocks is read through bmap.
        if JOFF { crate::println!("ext2: the journal is off by the flag talaria.ext2=nojournal"); } else { jrn_mount(); }
    }
    true
}

/// talaria.ext2=nojournal — do not use the journal (the diagnostics/a comparison).
static mut JOFF: bool = false;
/// SAFETY: a knob of the boot path, set before the mount and never touched afterwards.
pub fn set_nojournal() { unsafe { JOFF = true; } }
/// §2.5: talaria.ext2=journal=data — journal THE DATA together with the metadata
/// (by default it is the ordered mode: the data are written in place BEFORE the commit). The data in
/// the journal are more expensive in volume, but after a crash the file's content is restored too
/// rather than only the consistency of the metadata.
static mut JDATA: bool = false;
/// SAFETY: a knob of the boot path, set before the mount and never touched afterwards.
pub fn set_journal_data() { unsafe { JDATA = true; } }

/// Write the superblock (the primary one) to the disk.
/// SAFETY: E1 + E2 — under the FS lock; the `blk()` borrow of the superblock's block is
/// finished with inside the branch that took it, before `cflush`.
unsafe fn sb_flush() {
    let sbc = &*core::ptr::addr_of!(SB);
    if BS == 1024 { let b = blk(1); b[..1024].copy_from_slice(sbc); cflush(1); }
    else { let b = blk(0); b[1024..2048].copy_from_slice(sbc); cflush(0); }
}
/// The descriptor of the group g: (the GDT block, the offset).
/// SAFETY: `GDT_BLOCK` is geometry — written once at the mount, like `BS` (see `bs()`).
fn gd_loc(g: usize) -> (u32, usize) { let per = bs() / 32; (unsafe { GDT_BLOCK } + (g / per) as u32, (g % per) * 32) }

// ── the inodes ──────────────────────────────────────────────────────────────────
/// An inode's location: (the block, the offset in the block).
fn ino_loc(ino: u32) -> (u32, usize) {
    // SAFETY: E1 + E2 — under the FS lock (every caller holds it); the group descriptor's
    // borrow is consumed by `rd32` on the same line.
    unsafe {
        let g = ((ino - 1) / INODES_PER_GROUP) as usize;
        let idx = ((ino - 1) % INODES_PER_GROUP) as usize;
        let (gb, go) = gd_loc(g);
        let table = rd32(blk(gb), go + 8);
        let byte = idx * INODE_SIZE;
        (table + (byte / BS) as u32, byte % BS)
    }
}
/// A copy of an inode (the first 128 bytes).
/// SAFETY: E1 + E2 — under the FS lock; the borrow lives only for the copy OUT of the cache
/// into the caller's own buffer, which is exactly why the callers carry `[u8; 128]` around.
unsafe fn ino_read(ino: u32, out: &mut [u8; 128]) { let (b, o) = ino_loc(ino); out.copy_from_slice(&blk(b)[o..o + 128]); }
/// SAFETY: E1 + E2 + E3 — under the FS lock; the borrow lives only for the copy INTO the
/// cache, and `cflush` (which takes no borrow) marks the slot afterwards.
unsafe fn ino_write(ino: u32, inb: &[u8; 128]) { let (b, o) = ino_loc(ino); blk(b)[o..o + 128].copy_from_slice(inb); cflush(b); }
fn i_mode(i: &[u8; 128]) -> u16 { rd16(i, 0) }
fn i_size(i: &[u8; 128]) -> u64 { rd32(i, 4) as u64 | ((rd32(i, 108) as u64) << 32) }
fn set_size(i: &mut [u8; 128], s: u64) { wr32(i, 4, s as u32); wr32(i, 108, (s >> 32) as u32); }
fn i_links(i: &[u8; 128]) -> u16 { rd16(i, 26) }
fn is_dir(i: &[u8; 128]) -> bool { i_mode(i) & 0xF000 == 0x4000 }
fn is_reg(i: &[u8; 128]) -> bool { i_mode(i) & 0xF000 == 0x8000 }
fn is_lnk(i: &[u8; 128]) -> bool { i_mode(i) & 0xF000 == 0xA000 }

// ── the bitmaps ─────────────────────────────────────────────────────────────────
use crate::ext2bmap::{get as bit_get, set as bit_set};

// ── §2.4 THE BATCH ALLOCATION OF BLOCKS ─────────────────────────────────────
// It was: every block of a file had its own walk over the bitmap FROM THE START of the group (on
// a long file that is O(n^2): the 5000th block scanned 5000 bits), plus its own change
// of the group's descriptor and of the superblock.
// It became: (a) A CURSOR per group — the scan starts where the previous one ended;
//        (b) A RUN: for an inode's data up to PREA_RUN contiguous blocks are reserved at a time,
//            after which they are handed out from the window without an access to the map.
// The side but main effect is that a file lies on the disk CONTIGUOUSLY: that feeds both
// the write's coalescing (§8.2) and the file mapping (§2.2).
//
// There is NO leak of the reserved ones: the window's remainder is returned into the map before any
// write of the map to the disk (flush_all/jrn_commit) and at a close/truncate/unlink,
// so e2fsck never sees blocks that are marked but ownerless.
//
// §3.4: the run GROWS together with the file — 16, 32, 64 ... 256 blocks (with Linux that is done
// by the delayed allocation: there the volume is known at the flush). A real delalloc is
// unavailable to us without remaking the cache: a slot in it is addressed BY THE BLOCK'S
// NUMBER, which means a page of data must have a number already at the moment of the write; in order
// to decide later, the cache needs a second index (the inode, the block's number in the file) and the whole
// plumbing (the mmap pins, the journal's tags, the coalescing) on it. A growing run gives the
// same thing for whose sake a delalloc is made: a long file lies in a few contiguous
// pieces, while the trips into the bitmap become logarithmically few.
const PREA_N: usize = 8;
const PREA_RUN: u32 = 16;      // the first run
const PREA_MAX: u32 = 256;     // the ceiling (1 MB with a block of 4K)
const GHINTS: usize = 256;
#[derive(Clone, Copy)]
struct Prea { ino: u32, next: u32, left: u32, run: u32 }
static mut PREA: [Prea; PREA_N] = [Prea { ino: 0, next: 0, left: 0, run: PREA_RUN }; PREA_N];
// The cursor of the free room per group (a hint: a collision by the modulus is safe —
// in the worst case it is an extra scan).
static mut GHINT: [u32; GHINTS] = [0; GHINTS];

/// The number of bits (blocks) in the group g.
/// SAFETY: E1 — under the FS lock; it reads the geometry and the group's descriptor.
unsafe fn group_bits(g: usize) -> usize {
    crate::ext2bmap::group_bits(TOTAL_BLOCKS, FIRST_DATA, BLOCKS_PER_GROUP, g)
}

/// Allocate A RUN of up to `want` contiguous blocks. It returns (the first one, how many).
/// (0, 0) — there is no room. The blocks are NOT zeroed (that is done by the one handing them out).
/// SAFETY: E1 + E2 — under the FS lock. It walks the groups taking the bitmap and the group
/// descriptor of ONE group at a time, and each `blk()` borrow is done with before the next.
unsafe fn alloc_run(goal_group: usize, want: u32) -> (u32, u32) {
    STATS_ARUN += 1;
    let ng = NGROUPS;
    for k in 0..ng {
        let g = (goal_group + k) % ng;
        let (gb, go) = gd_loc(g);
        let free = rd16(blk(gb), go + 12);
        if free == 0 { continue; }
        let bmp = rd32(blk(gb), go);
        let nbits = group_bits(g);
        let hint = (GHINT[g % GHINTS] as usize).min(nbits);
        let Some(start) = crate::ext2bmap::find_free(blk(bmp), nbits, hint) else { continue };
        let n = crate::ext2bmap::take_run(blk(bmp), nbits, start, want);
        if n == 0 { continue; }
        cflush(bmp);
        let gd = blk(gb); wr16(gd, go + 12, free - n as u16); cflush(gb);
        let sbm = &mut *addr_of_mut!(SB); let v = rd32(sbm, 12).wrapping_sub(n); wr32(sbm, 12, v); sb_flush();
        GHINT[g % GHINTS] = (start + n as usize) as u32;
        return (FIRST_DATA + g as u32 * BLOCKS_PER_GROUP + start as u32, n);
    }
    (0, 0)
}
/// Zero a fresh block and mark it dirty.
/// SAFETY: E1 + E2 — under the FS lock; the block is freshly allocated, so `cblk_new` may
/// skip the read (its own passport), and the borrow is consumed by the zeroing.
unsafe fn zero_block(bn: u32) {
    core::ptr::write_bytes(cblk_new(bn), 0, BS);   // §4.1: without a read from the disk
    cflush(bn);
}

/// Allocate one block (the metadata: the indirect blocks, the directories). 0 = there is no room.
/// SAFETY: E1 — under the FS lock.
unsafe fn alloc_block(goal_group: usize) -> u32 {
    let (bn, n) = alloc_run(goal_group, 1);
    if n == 0 { return 0; }
    zero_block(bn);
    bn
}
/// §2.4: a block FOR the inode's DATA — from the preallocation window (a run is reserved
/// per one access to the map).
/// SAFETY: E1 — under the FS lock, which is what lets `&mut *addr_of_mut!(PREA)` be taken
/// at all: the window table is one per machine and only the lock holder ever touches it.
/// The borrow of `PREA` does not overlap the cache, so E2 is not at stake here.
unsafe fn alloc_data(ino: u32, goal_group: usize) -> u32 {
    let pr = &mut *addr_of_mut!(PREA);
    // §3.4: the size of the next run is remembered by this inode's window record — even
    // an empty one (prea_free leaves it as the hint "the file was growing").
    let mut want = PREA_RUN;
    for w in pr.iter_mut() {
        if w.ino == ino {
            if w.left > 0 {
                let bn = w.next; w.next += 1; w.left -= 1;
                zero_block(bn);
                return bn;
            }
            want = w.run;
        }
    }
    let (bn, n) = alloc_run(goal_group, want);
    if n == 0 { return 0; }
    // occupy/reuse a window for this inode (we keep the record even with n == 1 —
    // the growth step lives in it)
    let mut slot = usize::MAX;
    for (i, w) in pr.iter().enumerate() { if w.ino == ino || (w.left == 0 && w.ino == 0) { slot = i; break; } }
    if slot == usize::MAX { for (i, w) in pr.iter().enumerate() { if w.left == 0 { slot = i; break; } } }
    if slot == usize::MAX { slot = 0; }
    if pr[slot].left > 0 && pr[slot].ino != ino { prea_free(slot); }
    pr[slot] = Prea { ino, next: bn + 1, left: n - 1, run: (want * 2).min(PREA_MAX) };
    zero_block(bn);
    bn
}
/// Return the unused remainder of the window slot into the map. The record itself stays
/// a hint about the growth step (§3.4) until another inode occupies it.
/// SAFETY: E1 — under the FS lock; `slot` is an index into `PREA` the caller took from its
/// own walk over the same table.
unsafe fn prea_free(slot: usize) {
    let pr = &mut *addr_of_mut!(PREA);
    let (mut b, n) = (pr[slot].next, pr[slot].left);
    pr[slot].next = 0; pr[slot].left = 0;
    for _ in 0..n { free_block(b); b += 1; }
}
/// Return the remainders of an inode's windows (0 — of all of them).
/// SAFETY: E1 — under the FS lock. It must run BEFORE any write of the bitmaps to the disk
/// (a commit, a flush), otherwise e2fsck would see blocks marked busy and owned by nobody.
unsafe fn prea_release(ino: u32) {
    for slot in 0..PREA_N {
        let w = (*addr_of_mut!(PREA))[slot];
        if w.left > 0 && (ino == 0 || w.ino == ino) { prea_free(slot); }
    }
}
/// SAFETY: E1 + E2 + E3 — under the FS lock. Two `blk()` borrows live here (the bitmap and
/// the group's descriptor) and they do NOT overlap in time: `bm` is last used before
/// `blk(gb)` is taken — the rule of E2. The cache entry for the freed block is dropped at
/// the end under the counter pair (E3), because a record cached under that number is no
/// longer valid once the block is free.
unsafe fn free_block(bn: u32) {
    if bn < FIRST_DATA || bn >= TOTAL_BLOCKS { return; }
    let rel = bn - FIRST_DATA;
    let g = (rel / BLOCKS_PER_GROUP) as usize; let i = (rel % BLOCKS_PER_GROUP) as usize;
    let (gb, go) = gd_loc(g);
    let bmp = rd32(blk(gb), go);
    let bm = blk(bmp);
    if !bit_get(bm, i) { return; }
    bit_set(bm, i, false); cflush(bmp);
    if (i as u32) < GHINT[g % GHINTS] { GHINT[g % GHINTS] = i as u32; } // §2.4: the freed room is a candidate again
    let gd = blk(gb); wr16(gd, go + 12, rd16(gd, go + 12) + 1); cflush(gb);
    let sbm = &mut *addr_of_mut!(SB); let v = rd32(sbm, 12) + 1; wr32(sbm, 12, v); sb_flush();
    // a record that got into the cache under this number is no longer valid
    if let Some(slot) = find_slot(bn) {
        dcnt_del(slot);
        CTAG[slot] = 0; CDIRTY[slot] = false; CNEW[slot] = false; COWN[slot] = 0; CJRN[slot] = false; CJOLD[slot] = false;
    }
}
/// Allocate an inode (in the group goal_group if possible). 0 = there is none.
/// SAFETY: E1 + E2 — under the FS lock; as in `free_block`, the bitmap borrow is finished
/// with before the group descriptor's is taken.
unsafe fn alloc_inode(goal_group: usize, dir: bool) -> u32 {
    let ng = NGROUPS;
    for k in 0..ng {
        let g = (goal_group + k) % ng;
        let (gb, go) = gd_loc(g);
        let free = rd16(blk(gb), go + 14);
        if free == 0 { continue; }
        let bmp = rd32(blk(gb), go + 4);
        let bm = blk(bmp);
        for i in 0..INODES_PER_GROUP as usize {
            if !bit_get(bm, i) {
                bit_set(bm, i, true); cflush(bmp);
                let gd = blk(gb); wr16(gd, go + 14, free - 1);
                if dir { wr16(gd, go + 16, rd16(gd, go + 16) + 1); }
                cflush(gb);
                let sbm = &mut *addr_of_mut!(SB); let v = rd32(sbm, 16).wrapping_sub(1); wr32(sbm, 16, v); sb_flush();
                return g as u32 * INODES_PER_GROUP + i as u32 + 1;
            }
        }
    }
    0
}
/// SAFETY: E1 + E2 — under the FS lock; the same two-borrow shape as `free_block`.
unsafe fn free_inode(ino: u32, dir: bool) {
    let g = ((ino - 1) / INODES_PER_GROUP) as usize; let i = ((ino - 1) % INODES_PER_GROUP) as usize;
    let (gb, go) = gd_loc(g);
    let bmp = rd32(blk(gb), go + 4);
    let bm = blk(bmp); bit_set(bm, i, false); cflush(bmp);
    let gd = blk(gb); wr16(gd, go + 14, rd16(gd, go + 14) + 1);
    if dir { let d = rd16(gd, go + 16); if d > 0 { wr16(gd, go + 16, d - 1); } }
    cflush(gb);
    let sbm = &mut *addr_of_mut!(SB); let v = rd32(sbm, 16) + 1; wr32(sbm, 16, v); sb_flush();
}

// ── the file's block map ────────────────────────────────────────────────────────
/// The data block for the file block fb of the inode (inb is a copy of the inode, it is updated at an alloc).
/// alloc: allocate the missing ones (and the indirect ones). It returns the block's number (0 = a hole/there is no room).
/// It reports how many blocks were added (for i_blocks) through *added.
/// SAFETY: E1 + E2 — under the FS lock. It descends the indirect tree borrowing one block
/// at a time (`let b = blk(cur)` inside the step), and `inb` is the caller's copy of the
/// inode which it updates in place — the inode is written back by the caller.
unsafe fn bmap(ino: u32, inb: &mut [u8; 128], fb: u64, alloc: bool, added: &mut u32) -> u32 {
    let ptrs = (BS / 4) as u64;
    let goal = ((ino - 1) / INODES_PER_GROUP) as usize;
    let mut fb = fb;
    // level 0: the direct ones
    if fb < 12 {
        let o = 40 + (fb as usize) * 4;
        let mut b = rd32(inb, o);
        if b == 0 && alloc { b = alloc_data(ino, goal); if b != 0 { wr32(inb, o, b); *added += 1; } }
        return b;
    }
    fb -= 12;
    let (mut level, mut slot) = (1usize, 12usize);
    let mut span = ptrs;
    if fb >= ptrs { fb -= ptrs; level = 2; slot = 13; span = ptrs * ptrs; if fb >= span { fb -= span; level = 3; slot = 14; span = ptrs * ptrs * ptrs;
        if fb >= span { return 0; } } }
    // the root
    let mut cur = rd32(inb, 40 + slot * 4);
    if cur == 0 { if !alloc { return 0; } cur = alloc_block(goal); if cur == 0 { return 0; } wr32(inb, 40 + slot * 4, cur); *added += 1; }
    // the descent
    let mut lvl = level;
    while lvl > 0 {
        span /= ptrs;
        let idx = (fb / span) as usize; fb %= span;
        let b = blk(cur);
        let mut nxt = rd32(b, idx * 4);
        if nxt == 0 {
            if !alloc { return 0; }
            // a leaf (lvl == 1) is the inode's data: from the preallocation window (§2.4);
            // higher up it is an indirect block, it does not need a window
            nxt = if lvl == 1 { alloc_data(ino, goal) } else { alloc_block(goal) };
            if nxt == 0 { return 0; }
            let b = blk(cur); wr32(b, idx * 4, nxt); cflush(cur); *added += 1;
        }
        cur = nxt; lvl -= 1;
    }
    cur
}
/// Free the inode's whole block tree (a truncate down to 0).
/// SAFETY: E1 + E2 — under the FS lock; the recursion takes ONE indirect block's borrow per
/// level and finishes with it before descending, so no two are alive at once.
unsafe fn free_tree(b: u32, level: u32) {
    if b == 0 { return; }
    if level > 0 {
        let n = BS / 4;
        for i in 0..n { let c = rd32(blk(b), i * 4); if c != 0 { free_tree(c, level - 1); } }
    }
    free_block(b);
}
/// SAFETY: E1 — under the FS lock; `inb` is the caller's own copy of the inode (E2: that is
/// why inodes travel as `[u8; 128]` and not as borrows into the cache).
unsafe fn truncate_zero(inb: &mut [u8; 128]) {
    // A "fast" symbolic link has THE TARGET (a string) in the i_block array rather than
    // the blocks' numbers. Walking it as a block tree means freeing
    // "blocks" with numbers from ASCII: the bitmap goes off, while with an unlucky
    // coincidence somebody else's data are freed. It was caught as a divergence of the block map
    // in e2fsck after the links test.
    if is_lnk(inb) && i_size(inb) < 60 { set_size(inb, 0); return; }
    // §2.4: this inode's windows are no longer needed (see the callers: they know the inode)
    for i in 0..12 { let b = rd32(inb, 40 + i * 4); if b != 0 { free_tree(b, 0); wr32(inb, 40 + i * 4, 0); } }
    for (k, lvl) in [(12usize, 1u32), (13, 2), (14, 3)] { let b = rd32(inb, 40 + k * 4); if b != 0 { free_tree(b, lvl); wr32(inb, 40 + k * 4, 0); } }
    set_size(inb, 0); wr32(inb, 28, 0);
}
/// Shorten a file down to newsize (partially): free the data blocks beyond the end (the indirect ones stay).
/// SAFETY: E1 + E2 — under the FS lock; the tail block's borrow is taken and consumed
/// inside its own statement, before `cflush`.
unsafe fn truncate_to(ino: u32, inb: &mut [u8; 128], newsize: u64) {
    prea_release(ino); // §2.4: what is reserved for the inode goes into the map, the file is being shortened
    let old = i_size(inb);
    if newsize == 0 { truncate_zero(inb); return; }
    if newsize >= old { set_size(inb, newsize); return; }
    let first = newsize.div_ceil(BS as u64);
    let last = old.div_ceil(BS as u64);
    let mut fb = first;
    while fb < last {
        let mut add = 0;
        let b = bmap(ino, inb, fb, false, &mut add);
        if b != 0 { free_block(b); bmap_clear(ino, inb, fb); let bl = rd32(inb, 28); wr32(inb, 28, bl.saturating_sub(SPB as u32)); }
        fb += 1;
    }
    set_size(inb, newsize);
    // zero the tail of the last block (a read beyond size is truncated anyway; for cleanliness at a growth)
    let tail = (newsize % BS as u64) as usize;
    if tail != 0 { let mut add = 0; let b = bmap(ino, inb, newsize / BS as u64, false, &mut add); if b != 0 { let d = blk(b); for x in &mut d[tail..] { *x = 0;
        } cflush(b); } }
}
/// Zero the pointer to the file block fb (after a free_block).
/// SAFETY: E1 + E2 — under the FS lock; one indirect block borrowed at a time.
unsafe fn bmap_clear(ino: u32, inb: &mut [u8; 128], fb: u64) {
    let ptrs = (BS / 4) as u64;
    let _ = ino;
    let mut fb = fb;
    if fb < 12 { wr32(inb, 40 + (fb as usize) * 4, 0); return; }
    fb -= 12;
    let (mut level, mut slot, mut span) = (1usize, 12usize, ptrs);
    if fb >= ptrs { fb -= ptrs; level = 2; slot = 13; span = ptrs * ptrs; if fb >= span { fb -= span; level = 3; slot = 14; span = ptrs * ptrs * ptrs; } }
    let mut cur = rd32(inb, 40 + slot * 4);
    if cur == 0 { return; }
    while level > 0 {
        span /= ptrs;
        let idx = (fb / span) as usize; fb %= span;
        let nxt = rd32(blk(cur), idx * 4);
        if level == 1 { let b = blk(cur); wr32(b, idx * 4, 0); cflush(cur); return; }
        if nxt == 0 { return; }
        cur = nxt; level -= 1;
    }
}

// ── reading/writing an inode's data ─────────────────────────────────────────────
/// Read from the inode at the position off up to len bytes into dst. It returns how much was read.
/// `ra` is how many blocks it IS ALLOWED to pick up beyond the request if the read goes
/// sequentially (see `ra_hint`). Zero — read exactly what was requested.
/// A READ THAT RETURNED NOT WHAT WAS WRITTEN (session 32, goal 4). On session 31's
/// stand A mongod fell with `__inmem_row_leaf: illegal file format 0x0`:
/// an index page was read as zeroes. It did not reproduce on five stands.
/// There are three ways to get zeroes or a short read honestly by the FS — a hole (the block is not
/// allocated, `bmap` gave a 0), the request ran into the end of the file (it was shortened), a request beyond
/// the end (0 bytes). We count all three so that a repeat is visible in `stat:`.
pub static RD_HOLE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static RD_SHORT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static RD_PAST_EOF: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// SAFETY: E1 + E2, plus the condition on `dst`, which is a raw pointer from a PROCESS.
/// The syscall entry has already run it through `uaccess::check` -> `paging::user_range_ok`
/// for `len` writable bytes. That is not a formality in a single address space: the block
/// cache is identity-mapped WITHOUT U/S (`new_process_space`), so a process pointing `dst`
/// INTO the cache is refused there — which is what makes the `copy_nonoverlapping` below
/// non-overlapping, and what keeps one file's cached blocks from being written into
/// another's read. Each copy is bounded by `n <= BS - bo`, inside the block's slot.
unsafe fn ino_pread(ino: u32, inb: &mut [u8; 128], off: u64, dst: *mut u8, len: usize, ra: usize) -> usize {
    let size = i_size(inb);
    if off >= size { RD_PAST_EOF.fetch_add(1, core::sync::atomic::Ordering::Relaxed); return 0; }
    if len as u64 > size - off { RD_SHORT.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
    let len = len.min((size - off) as usize);
    let mut done = 0usize;
    while done < len {
        let pos = off + done as u64;
        let fb = pos / BS as u64; let bo = (pos % BS as u64) as usize;
        let n = (BS - bo).min(len - done);
        let mut add = 0;
        let b = bmap(ino, inb, fb, false, &mut add);
        if b == 0 { RD_HOLE.fetch_add(1, core::sync::atomic::Ordering::Relaxed); core::ptr::write_bytes(dst.add(done), 0, n); }
        else {
            // A miss at a block's boundary — take both the request's tail and the
            // readahead window in one access to the device (see `prefetch_run`).
            //
            // THE READAHEAD WINDOW IS OBLIGATORY. Previously the boundary was exactly what is asked for
            // right now, and on a read of a file by 4 KB (and that is how everybody reads whose own
            // buffer is smaller than our block — and that is half of the utilities) the window
            // came out as ONE block, that is, there was no readahead at all:
            // every block cost a separate access to the device. Measured with
            // `kernel/user/fsread`: 256 MB in pieces of 4 KB went at 169 MB/s against
            // 1680 MB/s in pieces of a megabyte — the same file, the same FS, the difference
            // is only in the request's size.
            if bo == 0 && find_slot(b).is_none() {
                let left = (len - done).div_ceil(BS).max(if done == 0 { ra } else { 0 });
                prefetch_run(ino, inb, fb, left as u64);
            }
            core::ptr::copy_nonoverlapping(cblk(b).add(bo), dst.add(done), n);
        }
        done += n;
    }
    done
}
/// Write into the inode at the position off. It updates the size/i_blocks/mtime, writes the inode. It returns how much was written or an error.
/// SAFETY: E1 + E2 + E3, and `src` carries the same condition as `dst` in `ino_pread`:
/// checked readable by `uaccess`, and unable to point into the cache because the cache has
/// no U/S. The per-block `cflush_own` marks the slot under the counter pair (E3).
unsafe fn ino_pwrite(ino: u32, inb: &mut [u8; 128], off: u64, src: *const u8, len: usize) -> i64 {
    let mut done = 0usize;
    let mut added = 0u32;
    while done < len {
        let pos = off + done as u64;
        let fb = pos / BS as u64; let bo = (pos % BS as u64) as usize;
        let n = (BS - bo).min(len - done);
        let before = added;
        let b = bmap(ino, inb, fb, true, &mut added);
        if b == 0 { if done == 0 { return ENOSPC; } break; }
        core::ptr::copy_nonoverlapping(src.add(done), cblk(b).add(bo), n);
        cflush_own(b, ino);  // §8.3: the slot belongs to this inode -> a per-file fsync
        if added != before { if let Some(sl) = find_slot(b) { CNEW[sl] = true; } } // see `CNEW`
        done += n;
    }
    let end = off + done as u64;
    if end > i_size(inb) { set_size(inb, end); }
    if added > 0 { let v = rd32(inb, 28) + added * SPB as u32; wr32(inb, 28, v); }
    let t = now(); wr32(inb, 16, t); wr32(inb, 12, t);
    ino_write(ino, inb);
    done as i64
}

// ── the directories ─────────────────────────────────────────────────────────────
/// Find the entry name in the directory dir. It returns (the ino, the file_type).
/// §3.1: with an indexed directory (EXT2_INDEX_FL) we first try a descent by
/// the htree; if the index's structure was not parsed — an honest linear walk (the htree's leaves
/// are ordinary blocks of entries, so the walk finds everything in any case).
/// SAFETY: E1 + E2 — under the FS lock; it dispatches to the htree or to the linear walk, and
/// both borrow one directory block at a time.
unsafe fn dir_lookup(dir: u32, dinb: &mut [u8; 128], name: &[u8]) -> Option<(u32, u8)> {
    if dx_enabled(dinb) {
        if let Some(r) = dx_lookup(dir, dinb, name) {
            if !DXCHECK { return r; }
            // talaria.ext2=dxcheck: verify the index against the walk (the diagnostics/the regression)
            let lin = dir_lookup_linear(dir, dinb, name);
            if lin.map(|x| x.0) != r.map(|x| x.0) {
                DX_MISS += 1;
                crate::println!("ext2: dxcheck: the index and the walk DIVERGE on a directory entry");
                return lin;
            }
            return r;
        }
        DX_NOIDX += 1;
    }
    dir_lookup_linear(dir, dinb, name)
}
/// SAFETY: E1 + E2 — under the FS lock; one directory block borrowed per turn of the walk.
unsafe fn dir_lookup_linear(dir: u32, dinb: &mut [u8; 128], name: &[u8]) -> Option<(u32, u8)> {
    let size = i_size(dinb);
    let nblk = size.div_ceil(BS as u64);
    for fb in 0..nblk {
        let mut add = 0;
        let b = bmap(dir, dinb, fb, false, &mut add);
        if b == 0 { continue; }
        LIN_BLKS += 1;
        if let Some(r) = crate::ext2dir::find(blk(b), name) { return Some(r); }
    }
    None
}
/// Add an entry to a directory (name, ino, ft). The error: ENOSPC.
/// §3.1: into an indexed directory an entry is put INTO THE LEAF responsible for its hash
/// (otherwise the index would stop finding it), at an overflow of the leaf there is a split
/// with a correction of the node. If the index cannot be extended, the directory IS DEINDEXED
/// (EXT2_INDEX_FL is removed) and lives on linearly — that is a correct state
/// of ext2/ext4: the leaves and the root already look like ordinary blocks of entries.
/// SAFETY: E1 + E2 — under the FS lock; the htree path may split a block, and every borrow it
/// takes is finished with before the next (see `dx_grow`, `dx_add`).
unsafe fn dir_add(dir: u32, dinb: &mut [u8; 128], name: &[u8], ino: u32, ft: u8) -> i64 {
    if dx_enabled(dinb) {
        match dx_add(dir, dinb, name, ino, ft) {
            Some(r) => return r,
            None => dx_degrade(dir, dinb),
        }
    } else if i_size(dinb) == BS as u64 && name.len() <= 255 {
        // A5: the directory outgrows one block — it is the very time to build an index
        // (ext4 does it at exactly this moment). While an entry fits into block 0,
        // we build nothing: an index for a dozen names is only extra reads.
        let mut add = 0;
        let b = bmap(dir, dinb, 0, false, &mut add);
        if b != 0 && dir_insert_in(b, name, ino, ft) { return 0; }
        if dx_make(dir, dinb) {
            match dx_add(dir, dinb, name, ino, ft) {
                Some(r) => return r,
                None => dx_degrade(dir, dinb),
            }
        }
    }
    dir_add_linear(dir, dinb, name, ino, ft)
}
/// SAFETY: E1 + E2 — under the FS lock; one directory block at a time, and a newly allocated
/// tail block comes from `cblk_new` already owned by this directory.
unsafe fn dir_add_linear(dir: u32, dinb: &mut [u8; 128], name: &[u8], ino: u32, ft: u8) -> i64 {
    let size = i_size(dinb);
    let nblk = size.div_ceil(BS as u64);
    for fb in 0..nblk {
        let mut add = 0;
        let b = bmap(dir, dinb, fb, false, &mut add);
        if b == 0 { continue; }
        if crate::ext2dir::insert(blk(b), name, ino, ft) { cflush(b); return 0; }
    }
    // a new block of the directory
    let mut added = 0;
    let b = bmap(dir, dinb, nblk, true, &mut added);
    if b == 0 { return ENOSPC; }
    crate::ext2dir::init(blk(b), name, ino, ft);
    cflush(b);
    set_size(dinb, (nblk + 1) * BS as u64);
    let v = rd32(dinb, 28) + added * SPB as u32; wr32(dinb, 28, v);
    ino_write(dir, dinb);
    0
}
/// Remove the entry name from the directory. false — it was not found.
/// SAFETY: E1 + E2 — under the FS lock; one directory block at a time.
unsafe fn dir_remove(dir: u32, dinb: &mut [u8; 128], name: &[u8]) -> bool {
    let size = i_size(dinb);
    let nblk = size.div_ceil(BS as u64);
    for fb in 0..nblk {
        let mut add = 0;
        let b = bmap(dir, dinb, fb, false, &mut add);
        if b == 0 { continue; }
        if crate::ext2dir::remove(blk(b), name) { cflush(b); return true; }
    }
    false
}
/// SAFETY: E1 + E2 — under the FS lock; a read-only walk, one block at a time.
unsafe fn dir_is_empty(dir: u32, dinb: &mut [u8; 128]) -> bool {
    let size = i_size(dinb);
    let nblk = size.div_ceil(BS as u64);
    for fb in 0..nblk {
        let mut add = 0;
        let b = bmap(dir, dinb, fb, false, &mut add);
        if b == 0 { continue; }
        if !crate::ext2dir::only_dots(blk(b)) { return false; }
    }
    true
}

// ═══ §3.1 HTREE (dir_index): the directory's hash index ═══════════════════════
//
// What for: a linear directory looks an entry up by walking all the blocks — on a directory of
// tens of thousands of files (a maildir, an object cache, a database's data directory) every
// open/stat costs tens of reads, while creating a file also costs a full walk
// in search of room. An htree turns that into 1-2 accesses.
//
// The format is the same as ext3/ext4's (otherwise an index left by Linux would have to
// be ignored, while ours would have to be rebuilt through an e2fsck -D):
//   the directory's block 0 = a dx_root: the entries "." and ".." (at ".." rec_len goes to the end of the block,
//     so the index is not visible to a linear reader), then a dx_root_info of 0x18
//     {reserved_zero u32, hash_version, info_length, indirect_levels, flags} and from
//     0x18+info_length an array of dx_entry {hash u32, block u32} (LE); the hash fields
//     of the zeroth entry are occupied by the pair {limit u16, count u16}, while its block is the leaf
//     for the hashes from zero;
//   a node (indirect_levels > 0) = a dummy entry of 8 bytes (ino 0, rec_len =
//     the block) and the same array from 0x08;
//   a leaf = an ordinary block of directory entries.
// A name's hash is ext4-compatible (half_md4/tea/legacy, the signedness of char and the seed from
// the superblock); it was verified byte by byte against `debugfs dx_hash`.
//
// What we do and what we do not: we READ an indexed directory by a descent, we WRITE with
// a split of a leaf and a correction of the node. We do not build an index from scratch (a linear directory
// stays linear) and we do not grow the tree deeper: when a node is full, the directory
// is deindexed (see dx_degrade) — that is a lawful state, e2fsck is clean.
const INDEX_FL: u32 = 0x1000;         // EXT2_INDEX_FL in the inode's i_flags
const COMPAT_DIR_INDEX: u32 = 0x20;   // s_feature_compat: dir_index
const DXLVL: usize = 3;               // the root + up to two levels of nodes
const DXENT: usize = 512;             // the ceiling of the entries in a leaf block (with 4K there are <= 341 of them)

static mut DX_PROBES: u64 = 0;    // the descents by the index
static mut DX_SPLITS: u64 = 0;    // the splits of a leaf
static mut DX_LEVELS: u64 = 0;    // A5: the levels of the tree added (the root -> a node)
static mut DX_NSPLITS: u64 = 0;   // A5: the splits of an index NODE
static mut DX_MADE: u64 = 0;      // A5: the directories indexed from scratch
static mut DX_DEGRADE: u64 = 0;   // the deindexed directories
static mut DX_NOIDX: u64 = 0;     // the cases "there is an index but it is not parsed" (a fallback to the walk)
static mut DX_MISS: u64 = 0;      // the divergences of the index with the walk (only with dxcheck)
static mut DX_BLKS: u64 = 0;      // the directory's blocks looked through by a descent by the index
static mut LIN_BLKS: u64 = 0;     // the directory's blocks looked through by a walk
static mut DXCHECK: bool = false;
static mut DXBUF: [u8; BSZ] = [0; BSZ];               // a copy of the leaf being split
static mut DXMAP: [crate::ext2dx::Slot; DXENT] = [crate::ext2dx::SLOT0; DXENT]; // (the hash, the offset, the room)

/// talaria.ext2=dxcheck — verify every find by the index against a linear walk.
/// SAFETY: a knob of the boot path, set before the mount.
pub fn set_dxcheck() { unsafe { DXCHECK = true; } }
/// talaria.ext2=dxlimit=N — consider an index node full already at N entries.
/// Only for the regression: a real root with a block of 4K holds 508 entries, that is,
/// the deindexing path (dx_degrade) in a test would otherwise require a directory of
/// tens of thousands of files. With the knob it is checked on a couple of thousand.
static mut DXLIMIT: usize = 0;
/// SAFETY: a knob of the boot path, set before the mount.
pub fn set_dxlimit(n: usize) { unsafe { DXLIMIT = n; } }
/// Has the node's capacity been squeezed by the knob? The gate `htree` asks the FS about that rather than parsing
/// the command line anew: the knob is one, while the answer to "which mode are we in" is here.
/// SAFETY: reads a boot-path knob that nothing changes afterwards.
pub fn dxlimit_set() -> bool { unsafe { DXLIMIT != 0 } }
/// The htree's counters: (the descents, the splits, deindexed, the index-not-parsed ones, the divergences).
/// SAFETY: the statistics exception of `cache_stats`.
pub fn dx_stats() -> (u64, u64, u64, u64, u64) { unsafe { (DX_PROBES, DX_SPLITS, DX_DEGRADE, DX_NOIDX, DX_MISS) } }
/// A5: the counters of the tree's GROWTH: (the levels added, the splits of a node, the directories
/// indexed from scratch). Previously all that was "we cannot do it" — at an overflow of a node a directory
/// was deindexed, while a linear one stayed linear.
/// SAFETY: the statistics exception of `cache_stats`.
pub fn dx_growth() -> (u64, u64, u64) { unsafe { (DX_LEVELS, DX_NSPLITS, DX_MADE) } }
/// A5: the depth of a directory's tree (indirect_levels) — 0 for a one-level one,
/// 1 after a level is added. None — the directory is not indexed/the index is not parsed.
pub fn dx_depth(path: &[u8]) -> Option<u8> {
    let _fsg = fs_lock();
    // SAFETY: E1 + E2 — the lock is taken on the line above; the root block's borrow is
    // consumed by the parsing.
    unsafe {
        let ino = lookup(path).ok()?;
        let mut inb = [0u8; 128]; ino_read(ino, &mut inb);
        if !dx_enabled(&inb) { return None; }
        let mut add = 0;
        let rb = bmap(ino, &mut inb, 0, false, &mut add);
        if rb == 0 { return None; }
        let d = blk(rb);
        if rd32(d, 0x18) != 0 { return None; }
        Some(d[0x1E])
    }
}
/// The price of a search in the directory's blocks: (by a descent by the index, by a walk). With
/// talaria.ext2=dxcheck both are counted — a direct comparison on the same names.
/// SAFETY: the statistics exception of `cache_stats`.
pub fn dx_blocks() -> (u64, u64) { unsafe { (DX_BLKS, LIN_BLKS) } }

/// The version of the directories' hash from the superblock (with a correction for the signedness of char of the
/// machine where the FS was created). None — the version is unknown to us.
/// SAFETY: E1 — under the FS lock; it reads the superblock's hash version.
unsafe fn dx_hash_ver() -> Option<u32> {
    let sb = &*core::ptr::addr_of!(SB);
    crate::ext2hash::effective_version(sb[252], rd32(sb, 352)) // s_def_hash_version, s_flags
}
/// A name's hash by the volume's rules (the low bit is always 0 — it serves as the sign of
/// a continuation of a chain of collisions).
/// SAFETY: E1 — under the FS lock; it reads the superblock's hash seed.
unsafe fn dx_hash(name: &[u8]) -> u32 {
    let sb = &*core::ptr::addr_of!(SB);
    let ver = match dx_hash_ver() { Some(v) => v, None => return 0 };
    let seed = [rd32(sb, 236), rd32(sb, 240), rd32(sb, 244), rd32(sb, 248)]; // s_hash_seed
    crate::ext2hash::name_hash(name, ver, &seed)
}

// ── the descent by the tree ─────────────────────────────────────────────────
#[derive(Clone, Copy)]
struct DxFrame { blk: u32, eoff: usize, at: usize, count: usize, limit: usize }
const DXF0: DxFrame = DxFrame { blk: 0, eoff: 0, at: 0, count: 0, limit: 0 };

/// Is the directory indexed and do we know its hash.
/// SAFETY: E1 — under the FS lock; `inb` is the caller's copy of the inode.
unsafe fn dx_enabled(inb: &[u8; 128]) -> bool {
    rd32(inb, 32) & INDEX_FL != 0
        && rd32(&*core::ptr::addr_of!(SB), 92) & COMPAT_DIR_INDEX != 0
        && BS >= 1024
        && dx_hash_ver().is_some()
}
/// Remove the index from a directory: from now on it is linear. The root and the nodes are empty entries
/// for a linear reader, the leaves are ordinary blocks, so the FS stays intact.
/// SAFETY: E1 + E2 — under the FS lock. Dropping the index is a LAWFUL state (the leaves are
/// ordinary blocks of entries), which is why a refusal anywhere in the htree can end here.
unsafe fn dx_degrade(dir: u32, dinb: &mut [u8; 128]) {
    let fl = rd32(dinb, 32);
    if fl & INDEX_FL == 0 { return; }
    wr32(dinb, 32, fl & !INDEX_FL);
    ino_write(dir, dinb);
    DX_DEGRADE += 1;
    crate::println!("ext2: the directory inode {} has been deindexed (the index does not extend) — from now on it is linear", dir);
}
/// A node's entries: (count, limit). None — the node is not parsed (see `ext2dx::head`).
/// SAFETY: E1 — under the FS lock; `d` is a borrow the caller already holds, and the parsing
/// itself lives in `ext2dx` over a plain slice.
#[inline]
unsafe fn dx_head(d: &[u8], eoff: usize) -> Option<(usize, usize)> { crate::ext2dx::head(d, eoff, BS) }
/// The descent to the leaf responsible for hash. It returns (the number of frames, the LOGICAL
/// block of the leaf in the directory). None — the index is not parsed (the structure is foreign/broken).
/// SAFETY: E1 + E2 — under the FS lock; it descends borrowing ONE index block per level and
/// keeping only numbers and offsets in the frames, never a borrow (that is what makes the
/// frames survive the next cache access — and what makes them merely STALE, not dangling,
/// after `dx_grow`).
unsafe fn dx_probe(dir: u32, dinb: &mut [u8; 128], hash: u32, fr: &mut [DxFrame; DXLVL]) -> Option<(usize, u64)> {
    let mut add = 0;
    let rb = bmap(dir, dinb, 0, false, &mut add);
    if rb == 0 { return None; }
    let (levels, mut eoff) = {
        let (arr, levels) = crate::ext2dx::root_info(blk(rb), BS)?;
        if levels + 1 > DXLVL { return None; }              // deeper than our frames
        (levels, arr)
    };
    let mut curb = rb;
    let mut nf = 0usize;
    loop {
        let (count, limit, child) = {
            let d = blk(curb);
            let (count, limit) = dx_head(d, eoff)?;
            let at = crate::ext2dx::descend(d, eoff, count, hash);
            fr[nf] = DxFrame { blk: curb, eoff, at, count, limit };
            (count, limit, crate::ext2dx::child_at(d, eoff, at) as u64)
        };
        let _ = (count, limit);
        nf += 1;
        if nf > levels { return Some((nf, child)); }
        let mut add = 0;
        let nb = bmap(dir, dinb, child, false, &mut add);
        if nb == 0 { return None; }
        curb = nb; eoff = 8;
    }
}
/// The next leaf of the chain of collisions (Linux's ext4_htree_next_block): we go up
/// while an entry in a node is the last one, step to the right and descend by the zeroth entries.
/// None — there is no continuation with the same hash.
/// SAFETY: E1 + E2 — under the FS lock; one index block borrowed at a time, as in `dx_probe`.
unsafe fn dx_next_leaf(dir: u32, dinb: &mut [u8; 128], fr: &mut [DxFrame; DXLVL], nf: usize, hash: u32) -> Option<u64> {
    let mut lvl = nf;
    loop {
        if lvl == 0 { return None; }
        if fr[lvl - 1].at + 1 < fr[lvl - 1].count { fr[lvl - 1].at += 1; break; }
        lvl -= 1;
    }
    // the hash of the entry we stepped onto: a continuation only at a coincidence
    let (mut child, f) = { let f = fr[lvl - 1]; let d = blk(f.blk);
        if crate::ext2dx::hash_at(d, f.eoff, f.at) & !1 != hash { return None; }
        (crate::ext2dx::child_at(d, f.eoff, f.at) as u64, f) };
    let _ = f;
    // below the level of the rise we take the leftmost entries
    for k in lvl..nf {
        let mut add = 0;
        let nb = bmap(dir, dinb, child, false, &mut add);
        if nb == 0 { return None; }
        let d = blk(nb);
        let (count, _limit) = dx_head(d, crate::ext2dx::NODE_ARRAY)?;
        fr[k] = DxFrame { blk: nb, eoff: crate::ext2dx::NODE_ARRAY, at: 0, count, limit: _limit };
        child = crate::ext2dx::child_at(d, crate::ext2dx::NODE_ARRAY, 0) as u64;
    }
    Some(child)
}
/// A search in ONE block of entries.
/// SAFETY: E1 + E2 — under the FS lock; a single block's borrow, consumed by the walk.
unsafe fn dir_lookup_in(b: u32, name: &[u8]) -> Option<(u32, u8)> {
    let d = blk(b);
    let mut o = 0usize;
    while o + 8 <= BS {
        let ino = rd32(d, o); let rl = rd16(d, o + 4) as usize; let nl = d[o + 6] as usize; let ft = d[o + 7];
        if rl < 8 || o + rl > BS || o + 8 + nl > BS { break; }
        if ino != 0 && nl == name.len() && &d[o + 8..o + 8 + nl] == name { return Some((ino, ft)); }
        o += rl;
    }
    None
}
/// An insertion into ONE block of entries (as dir_add_linear, but without a walk over the blocks).
/// SAFETY: E1 + E2 + E3 — under the FS lock; one block borrowed, then `cflush`.
unsafe fn dir_insert_in(b: u32, name: &[u8], ino: u32, ft: u8) -> bool {
    if crate::ext2dir::insert(blk(b), name, ino, ft) { cflush(b); return true; }
    false
}
/// A search by the index. None — the index is not parsed (the caller goes by a walk),
/// Some(None) — there is no such entry in the directory.
/// SAFETY: E1 + E2 — under the FS lock. `None` means "the index did not parse" and the caller
/// falls back to the linear walk — an unparsed index is never treated as "no such name".
unsafe fn dx_lookup(dir: u32, dinb: &mut [u8; 128], name: &[u8]) -> Option<Option<(u32, u8)>> {
    if name.len() > 255 { return Some(None); }
    let hash = dx_hash(name);
    let mut fr = [DXF0; DXLVL];
    let (nf, mut lb) = dx_probe(dir, dinb, hash, &mut fr)?;
    DX_PROBES += 1;
    DX_BLKS += nf as u64;   // the root + the nodes
    loop {
        let mut add = 0;
        let pb = bmap(dir, dinb, lb, false, &mut add);
        if pb == 0 { return Some(None); }
        DX_BLKS += 1;       // the leaf
        if let Some(r) = dir_lookup_in(pb, name) { return Some(Some(r)); }
        match dx_next_leaf(dir, dinb, &mut fr, nf, hash) { Some(n) => lb = n, None => return Some(None) }
    }
}

/// Lay the selected entries out into a block anew (`ext2dx::pack`: the last one
/// stretches rec_len to the end). false — the set does not fit into a block; then
/// the block is left alone and the caller must not go on with the split.
/// SAFETY: E1 + E2 — under the FS lock, and `src` MUST NOT point into the block cache: it
/// is written into `blk(dst)`, so a `src` inside the cache could overlap the destination.
/// Every caller passes `DXBUF` (see `dx_add`), which is a static scratch buffer outside the
/// cache — that is the whole reason the htree paths snapshot a block into `DXBUF` before
/// repacking instead of working in place.
unsafe fn dx_pack(dst: u32, src: &[u8], ents: &[crate::ext2dx::Slot]) -> bool {
    if !crate::ext2dx::pack(blk(dst), BS, src, ents) { return false; }
    cflush(dst);
    true
}
/// An index node's capacity. talaria.ext2=dxlimit=N squeezes it artificially: otherwise
/// an overflow of the root (508 entries with a block of 4K) in a test would require a directory
/// of tens of thousands of files.
/// SAFETY: E1 — under the FS lock; `DXLIMIT` is a knob of the boot path.
#[inline]
unsafe fn dx_cap(limit: usize) -> usize { if DXLIMIT != 0 { limit.min(DXLIMIT) } else { limit } }

/// A5: the room in a node has run out — GROW the tree rather than deindex the directory.
/// Two cases, as in `ext4_dx_add_entry`:
///   - THE ROOT is full and there are no levels yet: we create a level — the root's array of entries
///     moves into a new node block, while in the root one entry for it stays,
///     indirect_levels = 1;
///   - A NODE is full while there is room in the root: we split the node in half and write into
///     the root a reference to the second half.
/// We do not grow deeper than two levels — like Linux without largedir (its ceiling is
/// `ext2_dir_htree_level` = 2): the root is full with indirect_levels=1 -> false, and
/// the directory is deindexed, which stays a lawful state.
/// true — room has appeared; the caller's frames are stale, a new descent is needed.
/// SAFETY: E1 + E2 — under the FS lock. The shape that makes it sound is the SNAPSHOT: the
/// source node is copied into `DXBUF` before anything else is touched, so the later
/// `blk(nb)`/`blk(f.blk)`/`blk(p.blk)` may evict each other's slots freely — no borrow
/// outlives the next cache access. `DXBUF` itself is one buffer per machine and is safe
/// only because the FS lock is held for the whole of this function and nothing on the
/// directory path may release it (`FS_MAY_REL` is raised by `flush_all`/`fsync`/`msync`
/// alone, never here): a release between the snapshot and the repack would hand the buffer
/// to a neighbour and write one directory's entries into another's block.
unsafe fn dx_grow(dir: u32, dinb: &mut [u8; 128], fr: &[DxFrame; DXLVL], nf: usize) -> bool {
    if nf == 0 || nf > 2 { return false; }
    // A new block at the end of the directory is needed in both cases.
    let nblk = i_size(dinb).div_ceil(BS as u64);
    let mut added = 0u32;
    let nb = bmap(dir, dinb, nblk, true, &mut added);
    if nb == 0 { return false; }
    let src = &mut *addr_of_mut!(DXBUF);
    let ok = if nf == 1 {
        // ── a new level: the root's content -> a node ──
        let f = fr[0];
        core::ptr::copy_nonoverlapping(cblk(f.blk), src.as_mut_ptr(), BS);
        crate::ext2dx::node_write(blk(nb), BS, &src[f.eoff..f.eoff + f.count * 8]);
        cflush(nb);
        {
            let d = blk(f.blk);
            crate::ext2dx::set_count(d, f.eoff, 1);          // one entry is left in the root
            crate::ext2dx::set_child(d, f.eoff, 0, nblk as u32); // and it leads into the new node
            d[0x1E] = 1;                                     // indirect_levels
            cflush(f.blk);
        }
        DX_LEVELS += 1;
        true
    } else {
        // ── the split of a node: half of the entries -> a new node, a reference in the root ──
        let (p, f) = (fr[0], fr[1]);
        if p.count >= dx_cap(p.limit) || f.count < 2 { false } else {
            core::ptr::copy_nonoverlapping(cblk(f.blk), src.as_mut_ptr(), BS);
            let k = f.count / 2;                    // the entries [k, count) move away
            // The hash of the entry k is a real one: only the node's zeroth entry has its
            // "hash" field substituted (the pair {limit, count} lies there).
            let hash2 = crate::ext2dx::hash_at(src, f.eoff, k);
            // The REFERENCE in the parent goes first: if there is no room for it (the check above
            // says there is, but the room is counted in one place — `add_entry`), nothing has been
            // moved yet and the tree stays as it was. Otherwise the tail of the node's entries
            // would already live in the new block with nobody pointing at it.
            if !crate::ext2dx::add_entry(blk(p.blk), p.eoff, p.count, dx_cap(p.limit), p.at, hash2, nblk as u32) { false } else {
                cflush(p.blk);
                crate::ext2dx::node_write(blk(nb), BS, &src[f.eoff + k * 8..f.eoff + f.count * 8]);
                cflush(nb);
                { let d = blk(f.blk); crate::ext2dx::set_count(d, f.eoff, k); cflush(f.blk); }
                DX_NSPLITS += 1;
                true
            }
        }
    };
    // The block has been given to the directory in any case (in the unsuccessful branch it is simply empty —
    // a lawful block of a directory with one empty entry); the size and the block counter
    // must reflect that, otherwise e2fsck will see a divergence.
    if !ok {
        let d = blk(nb);
        for x in d[..BS].iter_mut() { *x = 0; }
        wr32(d, 0, 0); wr16(d, 4, BS as u16);
        cflush(nb);
    }
    set_size(dinb, (nblk + 1) * BS as u64);
    if added > 0 { let v = rd32(dinb, 28) + added * SPB as u32; wr32(dinb, 28, v); }
    ino_write(dir, dinb);
    ok
}

/// A5: build an index for a LINEAR directory (like `make_indexed_dir` in ext4).
/// It works when the directory occupies exactly one block and that one is full: block 0
/// becomes the root (the dots, the `dx_root_info`, an array of one entry), while all
/// the other entries move into a new leaf block. Previously a linear directory stayed
/// linear like that — an index appeared only from foreign tools
/// (`e2fsck -D`).
/// SAFETY: E1 + E2 — under the FS lock, with the same snapshot-into-`DXBUF` discipline as
/// `dx_grow` (and the same dependence on the lock not being released in the middle).
unsafe fn dx_make(dir: u32, dinb: &mut [u8; 128]) -> bool {
    if rd32(&*core::ptr::addr_of!(SB), 92) & COMPAT_DIR_INDEX == 0 || BS < 1024 { return false; }
    if rd32(dinb, 32) & INDEX_FL != 0 || i_size(dinb) != BS as u64 { return false; }
    if dx_hash_ver().is_none() { return false; }        // a hash this code does not implement
    let mut add = 0;
    let root = bmap(dir, dinb, 0, false, &mut add);
    if root == 0 { return false; }
    let src = &mut *addr_of_mut!(DXBUF);
    core::ptr::copy_nonoverlapping(cblk(root), src.as_mut_ptr(), BS);
    // the block's parsing: the dots stay in the root, everything else moves into the leaf
    let map = &mut *addr_of_mut!(DXMAP);
    let c = crate::ext2dx::collect(src, BS, map, true, &mut |nm| dx_hash(nm));
    if c.broken || c.overflow { return false; }
    let (Some((_, dot_ft)), Some((dd_ino, dd_ft))) = (c.dot, c.dotdot) else { return false };  // not a directory in the usual form
    let n = c.n;
    // The room is counted BEFORE a block is asked for: the records came from one block,
    // so they always fit — but a refusal after the allocation would leave the directory
    // with a block mapped beyond its size, and e2fsck counts that as a divergence.
    if !crate::ext2dx::fits(&map[..n], BS) { return false; }
    // the leaf (the logical block 1)
    let mut added = 0u32;
    let leaf = bmap(dir, dinb, 1, true, &mut added);
    if leaf == 0 { return false; }
    crate::ext2dx::sort(&mut map[..n]);              // by the hash, as after a split
    dx_pack(leaf, src, &map[..n]);
    // The root carries the superblock's RAW version (0..2), not the effective one the hashing
    // uses (`dx_hash_ver`: the unsigned-char volumes of arm64 hosts add 3 to it).
    let raw_hv = (*core::ptr::addr_of!(SB))[252] as u32;
    crate::ext2dx::root_write(blk(root), BS, dir, dot_ft, dd_ino, dd_ft, raw_hv, 1);
    cflush(root);
    let fl = rd32(dinb, 32) | INDEX_FL;
    wr32(dinb, 32, fl);
    set_size(dinb, 2 * BS as u64);
    if added > 0 { let v = rd32(dinb, 28) + added * SPB as u32; wr32(dinb, 28, v); }
    ino_write(dir, dinb);
    DX_MADE += 1;
    crate::println!("ext2: the directory inode {} has been indexed (an htree was built from scratch, {} names)", dir, n);
    true
}

/// An insertion into an indexed directory. Some(0|an error) — it is handled; None —
/// the index could not be extended, the caller deindexes the directory.
/// SAFETY: E1 + E2 — under the FS lock. Both machine-wide scratch buffers are used here —
/// `DXBUF` for the leaf's bytes and `DXMAP` for its parsed entries — and both rest on the
/// same condition as in `dx_grow`: the FS lock is held from the snapshot to the last
/// `dx_pack`, and the directory path has no release point inside that stretch. The frames
/// from `dx_probe` hold numbers rather than borrows, so re-descending after a growth is a
/// matter of staleness, not of dangling.
unsafe fn dx_add(dir: u32, dinb: &mut [u8; 128], name: &[u8], ino: u32, ft: u8) -> Option<i64> {
    if name.len() > 255 { return Some(ENAMETOOLONG); }
    let hash = dx_hash(name);
    let mut fr = [DXF0; DXLVL];
    // A descent -> an attempt at an insertion -> if the node under the leaf is full, we grow the tree and
    // descend anew (after a growth the frames are invalid). THREE passes are needed
    // rather than two: the addition of a level gives the new node the root's whole array of
    // entries, that is, the node is born already full, and after that its
    // split is required too. With two passes the directory was deindexed at this place —
    // the tree grew by exactly one level and lost its index right away.
    let (nf, lb) = {
        let mut r = None;
        for _ in 0..3 {
            let (nf, lb) = dx_probe(dir, dinb, hash, &mut fr)?;
            let mut add = 0;
            let leaf = bmap(dir, dinb, lb, false, &mut add);
            if leaf == 0 { return None; }
            if dir_insert_in(leaf, name, ino, ft) { return Some(0); }
            if fr[nf - 1].count < dx_cap(fr[nf - 1].limit) { r = Some((nf, lb)); break; }
            if !dx_grow(dir, dinb, &fr, nf) { return None; }
        }
        r?
    };
    let mut add = 0;
    let leaf = bmap(dir, dinb, lb, false, &mut add);
    if leaf == 0 { return None; }

    // ── the leaf is full: a split ──
    let f = fr[nf - 1];
    // the leaf's parsing: the hash, the place and the room per every live record
    let src = &mut *addr_of_mut!(DXBUF);
    core::ptr::copy_nonoverlapping(cblk(leaf), src.as_mut_ptr(), BS);
    let map = &mut *addr_of_mut!(DXMAP);
    let c = crate::ext2dx::collect(src, BS, map, false, &mut |nm| dx_hash(nm));
    // A leaf that is not parsed WHOLE must not be repacked: the records the walk did not
    // reach would be lost by the repack. The directory is deindexed instead — a lawful state.
    if c.broken || c.overflow { return None; }
    let n = c.n;
    // a sort by the hash (by insertions: there are hundreds of entries, it happens once per split)
    crate::ext2dx::sort(&mut map[..n]);
    // we divide by volume in half (like do_split in ext4)
    let sp = crate::ext2dx::split_point(&map[..n], BS)?;   // fewer than two records — nothing to divide
    // BOTH halves are counted before either block is rewritten: a repack that runs out of
    // room in the middle would lose the records it has not laid out yet.
    if !crate::ext2dx::fits(&map[..sp.keep], BS) || !crate::ext2dx::fits(&map[sp.keep..n], BS) { return None; }

    // a new block at the end of the directory
    let nblk = i_size(dinb).div_ceil(BS as u64);
    let mut added = 0u32;
    let nb = bmap(dir, dinb, nblk, true, &mut added);
    if nb == 0 { return Some(ENOSPC); }
    dx_pack(leaf, src, &map[..sp.keep]);
    dx_pack(nb, src, &map[sp.keep..n]);
    set_size(dinb, (nblk + 1) * BS as u64);
    if added > 0 { let v = rd32(dinb, 28) + added * SPB as u32; wr32(dinb, 28, v); }
    ino_write(dir, dinb);
    // the index's entry (the hash + the sign of a chain of collisions, the new LOGICAL block) — after at
    if !crate::ext2dx::add_entry(blk(f.blk), f.eoff, f.count, dx_cap(f.limit), f.at, sp.key, nblk as u32) { return None; }
    cflush(f.blk);
    DX_SPLITS += 1;
    let target = if hash >= sp.hash { nb } else { leaf };
    if dir_insert_in(target, name, ino, ft) { Some(0) } else { None } // it did not fit even after the division
}

// ── the symbolic links ──────────────────────────────────────────────────────────
//
// In ext2 a "fast" link (the target is shorter than 60 bytes) lies RIGHT IN THE INODE, in the array of
// pointers to the blocks; a long one lies in the first block of data. While the kernel did not know that,
// it read the target's bytes as the blocks' numbers: `/lib64/ld-linux-x86-64.so.2` in Ubuntu
// is a link, and the loader went to read "the block 1651076143". The images had to be
// assembled with dereferencing (`tar -ch`).
const SYMLINK_MAX: usize = 255;
const SYMLINK_HOPS: u32 = 8; // as in Linux: beyond that an ELOOP

/// Read a link's target into out; return the length (0 — it is not a link/it is empty/it is too long).
/// SAFETY: E1 + E2 — under the FS lock; a fast link is read out of `inb` (the caller's copy),
/// a slow one borrows its one block and copies out of it.
unsafe fn read_link(ino: u32, inb: &[u8; 128], out: &mut [u8; SYMLINK_MAX]) -> usize {
    if !is_lnk(inb) { return 0; }
    let sz = i_size(inb) as usize;
    if sz == 0 || sz > SYMLINK_MAX { return 0; }
    if sz < 60 {
        out[..sz].copy_from_slice(&inb[40..40 + sz]); // a fast link: the target is in i_block
        return sz;
    }
    let mut tmp = *inb;
    let mut added = 0u32;
    let b = bmap(ino, &mut tmp, 0, false, &mut added);
    if b == 0 { return 0; }
    let src = blk(b);
    out[..sz].copy_from_slice(&src[..sz]);
    sz
}

/// If the inode is a link, resolve it (a relative one is counted from `parent`).
/// depth bounds the chain of links (Linux: an ELOOP).
/// SAFETY: E1 — under the FS lock; `depth` bounds the chain (an ELOOP past it), so the
/// recursion cannot run away.
unsafe fn deref(ino: u32, parent: u32, depth: u32) -> Result<u32, i64> {
    if depth > SYMLINK_HOPS { return Err(ELOOP); }
    let mut inb = [0u8; 128];
    ino_read(ino, &mut inb);
    if !is_lnk(&inb) { return Ok(ino); }
    let mut tgt = [0u8; SYMLINK_MAX];
    let n = read_link(ino, &inb, &mut tgt);
    if n == 0 { return Err(ENOENT); }
    let (start, rest) = if tgt[0] == b'/' { (ROOT, &tgt[1..n]) } else { (parent, &tgt[..n]) };
    walk_at(start, rest, true, depth + 1)
}

/// Walk the path `path` from the inode `start`, resolving the links on the intermediate
/// components always, and on the last one — if `follow_last`.
/// SAFETY: E1 — under the FS lock; the inodes travel as `[u8; 128]` copies (E2), never as
/// borrows into the cache, which is what lets the walk take the next block at every step.
unsafe fn walk_at(start: u32, path: &[u8], follow_last: bool, depth: u32) -> Result<u32, i64> {
    if depth > SYMLINK_HOPS { return Err(ELOOP); }
    let path = match path.iter().position(|&c| c == 0) { Some(i) => &path[..i], None => path };
    let mut cur = start;
    let mut i = 0usize;
    while i < path.len() && path[i] == b'/' { i += 1; }
    if i >= path.len() { return Ok(cur); }
    loop {
        let s = i;
        while i < path.len() && path[i] != b'/' { i += 1; }
        let name = &path[s..i];
        let mut j = i; while j < path.len() && path[j] == b'/' { j += 1; }
        let last = j >= path.len();
        let mut inb = [0u8; 128]; ino_read(cur, &mut inb);
        if !is_dir(&inb) { return Err(ENOTDIR); }
        if name.len() > 255 { return Err(ENAMETOOLONG); }
        let next = if name == b"." { cur }
            else if name == b".." { match dir_lookup(cur, &mut inb, b"..") { Some((n, _)) => n, None => ROOT } }
            else { match dir_lookup(cur, &mut inb, name) { Some((n, _)) => n, None => return Err(ENOENT) } };
        if last {
            return if follow_last { deref(next, cur, depth) } else { Ok(next) };
        }
        cur = deref(next, cur, depth)?; // an intermediate component must be a directory
        i = j;
    }
}

// ── the paths ───────────────────────────────────────────────────────────────────
/// Parse an absolute path: the parent's inode and the last component's name.
/// It returns (parent_ino, name_start, name_len) or an error. The path "/" -> (0, root).
/// SAFETY: E1 — under the FS lock; it returns the parent's number and the offsets of the last
/// component INSIDE the caller's `path`, holding no borrow of its own.
unsafe fn walk_parent(path: &[u8]) -> Result<(u32, usize, usize), i64> {
    let path = match path.iter().position(|&c| c == 0) { Some(i) => &path[..i], None => path }; // a C string -> a slice
    let mut cur = ROOT;
    let mut i = 0usize;
    // skip the leading '/'
    while i < path.len() && path[i] == b'/' { i += 1; }
    if i >= path.len() { return Ok((0, 0, 0)); } // the root
    loop {
        let s = i;
        while i < path.len() && path[i] != b'/' { i += 1; }
        let name = &path[s..i];
        let mut j = i; while j < path.len() && path[j] == b'/' { j += 1; }
        if j >= path.len() { return Ok((cur, s, i - s)); } // the last component
        // an intermediate one: descend
        let mut inb = [0u8; 128]; ino_read(cur, &mut inb);
        if !is_dir(&inb) { return Err(ENOTDIR); }
        if name.len() > 255 { return Err(ENAMETOOLONG); }
        let next = if name == b"." { cur } else if name == b".." { match dir_lookup(cur, &mut inb, b"..") { Some((n, _)) => n, None => ROOT } }
            else { match dir_lookup(cur, &mut inb, name) { Some((n, _)) => n, None => return Err(ENOENT) } };
        cur = deref(next, cur, 0)?; // a symbolic link in the middle of the path is the directory behind it
        i = j;
    }
}
/// The inode by an absolute path (or an ENOENT).
/// SAFETY: E1 — one of the two `pub unsafe fn` of this module: the caller must already
/// hold the FS lock (the callers are `fs.rs` and this file's own entry points, which take
/// it). It is `unsafe` rather than lock-taking precisely so that the obligation is visible
/// at the call site — a `lookup` that took the lock itself would hide a recursion.
pub unsafe fn lookup(path: &[u8]) -> Result<u32, i64> {
    let _fsg = fs_lock();
    let (p, s, l) = walk_parent(path)?;
    if p == 0 { return Ok(ROOT); }
    let name = &path[s..s + l];
    if name == b"." { return Ok(p); }
    let mut pinb = [0u8; 128]; ino_read(p, &mut pinb);
    if !is_dir(&pinb) { return Err(ENOTDIR); }
    match dir_lookup(p, &mut pinb, name) { Some((n, _)) => Ok(n), None => Err(ENOENT) }
}

/// The inode by a path WITH FOLLOWING the symbolic link in the last component
/// (the open/stat semantics; lstat/unlink/rename need `lookup`).
/// SAFETY: E1 — as `lookup`: the caller holds the FS lock.
pub unsafe fn lookup_follow(path: &[u8]) -> Result<u32, i64> {
    let _fsg = fs_lock();
    let ino = lookup(path)?;
    let (p, _, _) = walk_parent(path)?;
    deref(ino, if p == 0 { ROOT } else { p }, 0)
}

/// readlink(path): copy the link's target into a buffer. It returns the length or an error.
pub fn readlink(path: &[u8], buf: *mut u8, len: usize) -> i64 {
    let _fsg = fs_lock();
    // SAFETY: E1 — the FS lock is taken on the line above. The raw pointer is a PROCESS
    // pointer already checked by `uaccess` for this length and direction; it cannot point
    // into the block cache, which is mapped without U/S (see `ino_pread`).
    unsafe {
        let ino = match lookup(path) { Ok(n) => n, Err(e) => return e };
        let mut inb = [0u8; 128]; ino_read(ino, &mut inb);
        if !is_lnk(&inb) { return EINVAL; }
        let mut tgt = [0u8; SYMLINK_MAX];
        let n = read_link(ino, &inb, &mut tgt);
        if n == 0 { return EINVAL; }
        let m = n.min(len);
        core::ptr::copy_nonoverlapping(tgt.as_ptr(), buf, m);
        m as i64
    }
}

/// A stat WITHOUT following the link (an lstat).
pub fn lstat(path: &[u8], st: *mut u8) -> i64 {
    let _fsg = fs_lock();
    // SAFETY: E1 — the FS lock is taken on the line above. The raw pointer is a PROCESS
    // pointer already checked by `uaccess` for this length and direction; it cannot point
    // into the block cache, which is mapped without U/S (see `ino_pread`).
    unsafe {
        let ino = match lookup(path) { Ok(n) => n, Err(e) => return e };
        let mut inb = [0u8; 128]; ino_read(ino, &mut inb);
        fill_stat(ino, &inb, st);
        0
    }
}

/// symlink(target, linkpath): create a symbolic link.
pub fn symlink(target: &[u8], path: &[u8]) -> i64 {
    let _fsg = fs_lock();
    if let Some(e) = ro_err() { return e; }
    // SAFETY: E1 — the FS lock is taken on the line above; the body is ordinary ext2 work.
    unsafe {
        let t = match target.iter().position(|&c| c == 0) { Some(i) => &target[..i], None => target };
        if t.is_empty() || t.len() > SYMLINK_MAX { return EINVAL; }
        let (p, s, l) = match walk_parent(path) { Ok(x) => x, Err(e) => return e };
        if p == 0 { return EEXIST; }
        let name = &path[s..s + l];
        let mut pinb = [0u8; 128]; ino_read(p, &mut pinb);
        if !is_dir(&pinb) { return ENOTDIR; }
        if dir_lookup(p, &mut pinb, name).is_some() { return EEXIST; }
        let goal = ((p - 1) / INODES_PER_GROUP) as usize;
        let n = alloc_inode(goal, false); if n == 0 { return ENOSPC; }
        let mut nb = [0u8; 128];
        wr16(&mut nb, 0, 0xA000 | 0o777); wr16(&mut nb, 26, 1);
        let now_t = now(); wr32(&mut nb, 8, now_t); wr32(&mut nb, 12, now_t); wr32(&mut nb, 16, now_t);
        set_size(&mut nb, t.len() as u64);
        if t.len() < 60 {
            nb[40..40 + t.len()].copy_from_slice(t);       // a fast link goes right into the inode
        } else {
            let b = alloc_block(goal); if b == 0 { return ENOSPC; }
            wr32(&mut nb, 40, b); wr32(&mut nb, 28, (BS / 512) as u32);
            let dst = blk(b); dst[..t.len()].copy_from_slice(t); for k in t.len()..BS { dst[k] = 0; }
            cflush(b);
        }
        ino_write(n, &nb);
        let r = dir_add(p, &mut pinb, name, n, 7); // ft=7 — symlink
        if r < 0 { return r; }
        0
    }
}

// ── the table of open files ─────────────────────────────────────────────────────
static mut OF_INO: [u32; MAXOPEN] = [0; MAXOPEN];
static mut OF_POS: [u64; MAXOPEN] = [0; MAXOPEN];
static mut OF_FL: [u32; MAXOPEN] = [0; MAXOPEN]; // O_APPEND (0x400), O_NONBLOCK and so on
// THE READAHEAD BY BEHAVIOUR. The expected offset of this descriptor's next read
// (u64::MAX — there is nothing to expect) and the accumulated window in blocks. The window
// grows while the reads go back to back, and is zeroed at the very first jump: that way
// a sequential walk over a file gets large accesses to the device, while
// a scattered read does not pay for extra blocks.
static mut OF_RANEXT: [u64; MAXOPEN] = [u64::MAX; MAXOPEN];
static mut OF_RAWIN: [u16; MAXOPEN] = [0; MAXOPEN];
const PLEN: usize = 128;
static mut OF_PATH: [[u8; PLEN]; MAXOPEN] = [[0; PLEN]; MAXOPEN]; // the handle's absolute path (for an openat(dirfd, rel))
static mut OF_PL: [u8; MAXOPEN] = [0; MAXOPEN];
/// The path a handle is open by (for the *at ones with a dirfd). An empty one — unknown/truncated.
/// SAFETY: E1 IS NOT TAKEN, and the returned `'static` is a lie the caller must not keep:
/// the slice points into the open-file table's row, which a `close` may reuse. The only
/// caller (`proc::resolve_at_slot`) copies the bytes into its own buffer on the next line,
/// so the borrow does not outlive the row. Bounds hold unconditionally — `OF_PL[i]` is set
/// to `min(len, PLEN)` with `PLEN == 128`, so the length never exceeds the row. What the
/// missing lock costs is a WRONG PATH, not memory: a thread closing this `dirfd` while
/// another resolves against it can be seen half-way, and that race is a use-after-close in
/// the application to begin with.
pub fn fd_path(h: i64) -> &'static [u8] { match of(h) { Some(i) => unsafe { let l = OF_PL[i] as usize;
    core::slice::from_raw_parts(addr_of_mut!(OF_PATH[i]) as *const u8, l) }, None => b"" } }
/// SAFETY: read volatile without the lock (E7): the answer only gets more conservative if
/// it is stale, and the bounds are checked before the read.
pub fn is_fd(h: i64) -> bool {
    h >= FD_BASE && ((h - FD_BASE) as usize) < MAXOPEN
        && unsafe { core::ptr::read_volatile(core::ptr::addr_of!(OF_INO[(h - FD_BASE) as usize])) } != 0
}
fn of(h: i64) -> Option<usize> { if is_fd(h) { Some((h - FD_BASE) as usize) } else { None } }
/// SAFETY: the unmount path — it runs when the file system is no longer in use, so there is
/// no holder of a row to take one from.
pub fn reset_open() { unsafe { for h in 0..MAXOPEN { OF_INO[h] = 0; } } }
/// SAFETY: E1 — under the FS lock: the scan for a free row and the claiming of it are one
/// critical section, which is what stops two opens taking the same row.
unsafe fn of_alloc(ino: u32, fl: u32) -> i64 {
    for h in 0..MAXOPEN { if OF_INO[h] == 0 { OF_INO[h] = ino; OF_POS[h] = 0; OF_FL[h] = fl; OF_RANEXT[h] = u64::MAX; OF_RAWIN[h] = 0;
        return FD_BASE + h as i64; } }
    EMFILE
}

const O_CREAT: u64 = 0o100; const O_EXCL: u64 = 0o200; const O_TRUNC: u64 = 0o1000; const O_APPEND: u64 = 0o2000; const O_DIRECTORY: u64 = 0o200000;

/// open(path, flags, mode) -> handle.
pub fn open(path: &[u8], flags: u64, mode: u32) -> i64 {
    let _fsg = fs_lock();
    if (flags & 3) != 0 || flags & O_TRUNC != 0 { if let Some(e) = ro_err() { return e; } } // a read-only FS: writing is not allowed
    // SAFETY: E1 — the FS lock is taken on the line above; the body is ordinary ext2 work.
    unsafe {
        let (p, s, l) = match walk_parent(path) { Ok(x) => x, Err(e) => return e };
        let mut inb = [0u8; 128];
        let ino = if p == 0 { ROOT } else {
            let name = &path[s..s + l];
            let mut pinb = [0u8; 128]; ino_read(p, &mut pinb);
            if !is_dir(&pinb) { return ENOTDIR; }
            match dir_lookup(p, &mut pinb, name) {
                Some((n, _)) => { if flags & O_CREAT != 0 && flags & O_EXCL != 0 { return EEXIST; } n }
                None => {
                    if flags & O_CREAT == 0 { return ENOENT; }
                    if let Some(e) = ro_err() { return e; }
                    // create an ordinary file
                    let goal = ((p - 1) / INODES_PER_GROUP) as usize;
                    let n = alloc_inode(goal, false); if n == 0 { return ENOSPC; }
                    let mut nb = [0u8; 128];
                    wr16(&mut nb, 0, 0x8000 | (mode as u16 & 0o7777)); wr16(&mut nb, 26, 1);
                    let t = now(); wr32(&mut nb, 8, t); wr32(&mut nb, 12, t); wr32(&mut nb, 16, t);
                    ino_write(n, &nb);
                    let r = dir_add(p, &mut pinb, name, n, 1); if r < 0 { return r; }
                    n
                }
            }
        };
        // The last component is a link: an open must open THE TARGET (except with O_NOFOLLOW).
        let ino = if flags & 0o400000 != 0 { ino } else { match deref(ino, if p == 0 { ROOT } else { p }, 0) { Ok(n) => n, Err(e) => return e } };
        ino_read(ino, &mut inb);
        if flags & O_DIRECTORY != 0 && !is_dir(&inb) { return ENOTDIR; }
        if is_dir(&inb) && (flags & 3) != 0 { return EISDIR; }
        if flags & O_TRUNC != 0 && is_reg(&inb) && (flags & 3) != 0 { truncate_zero(&mut inb); wr32(&mut inb, 16, now()); ino_write(ino, &inb); }
        let h = of_alloc(ino, (flags & (O_APPEND | 0o4000)) as u32);
        if h >= 0 { let i = (h - FD_BASE) as usize; let p = match path.iter().position(|&c| c == 0) { Some(k) => &path[..k], None => path };
            let l = p.len().min(PLEN); OF_PATH[i][..l].copy_from_slice(&p[..l]); OF_PL[i] = if p.len() <= PLEN { l as u8 } else { 0 }; }
        h
    }
}
pub fn close(h: i64) -> i64 {
    let _fsg = fs_lock();
    match of(h) {
    // SAFETY: E1 — the FS lock is taken on the line above; the body is ordinary ext2 work.
        Some(i) => unsafe {
            let ino = OF_INO[i];
            OF_INO[i] = 0;
            // §2.4: the file is closed — there is no point in keeping a reserved tail for it
            // (the other open descriptors of the same inode will get a new window).
            let mut still_open = false;
            for k in 0..MAXOPEN { if OF_INO[k] == ino { still_open = true; break; } }
            if !still_open { prea_release(ino); }
            0
        }
        None => EBADF,
    }
}
/// SAFETY: `i` comes from `of()`, hence `< MAXOPEN`; a plain read of a `u32` row.
pub fn fd_flags(h: i64) -> u32 { match of(h) { Some(i) => unsafe { OF_FL[i] }, None => 0 } }
/// SAFETY: `i` comes from `of()`, hence `< MAXOPEN`; only this row's flags are touched.
pub fn fd_set_flags(h: i64, fl: u32) { if let Some(i) = of(h) { unsafe { OF_FL[i] = (OF_FL[i] & O_APPEND as u32) | (fl & 0o4000); } } }

/// Read a piece of a file STRAIGHT into memory, BYPASSING the block cache, gluing the blocks that are neighbours on the
/// disk into ONE request to the device. The offset and the length are multiples of a block.
///
/// What for: the ELF loader read an image page by page, and ClickHouse's 525 MB
/// turned into 134 000 requests of 4 KB — eleven seconds for the loading
/// alone. There is no point in dragging a large image through a cache of 8 MB either: it will simply
/// evict it. The blocks lying in the cache (including the dirty ones) are taken from the cache —
/// otherwise one could read a stale version from the disk.
pub fn pread_direct(h: i64, dst: *mut u8, len: usize, off: u64) -> i64 {
    let _fsg = fs_lock();
    // SAFETY: E1 — the FS lock is taken on the line above. The raw pointer is a PROCESS
    // pointer already checked by `uaccess` for this length and direction; it cannot point
    // into the block cache, which is mapped without U/S (see `ino_pread`).
    unsafe {
        if !active() || BS != BSZ || !off.is_multiple_of(BS as u64) || !len.is_multiple_of(BS) { return EINVAL; }
        let i = match of(h) { Some(i) => i, None => return EBADF };
        let ino = OF_INO[i];
        let mut inb = [0u8; 128];
        ino_read(ino, &mut inb);
        let size = i_size(&inb);
        if off >= size { return 0; }
        let want = len.min((size - off) as usize);
        let nblk = want.div_ceil(BS);
        let mut added = 0u32;
        let mut k = 0usize;
        while k < nblk {
            let fb = off / BS as u64 + k as u64;
            let b = bmap(ino, &mut inb, fb, false, &mut added);
            if b == 0 { core::ptr::write_bytes(dst.add(k * BS), 0, BS); k += 1; continue; } // a hole — zeroes
            if let Some(slot) = find_slot(b) {                                              // a fresh version is in the cache
                core::ptr::copy_nonoverlapping(crate::mem::kptr::<u8>(CACHE_BASE).add(slot * BSZ), dst.add(k * BS), BS);
                k += 1; continue;
            }
            let mut run = 1usize;                                                           // the gluing of the neighbouring blocks
            while k + run < nblk && run < 512 {
                let nb = bmap(ino, &mut inb, off / BS as u64 + (k + run) as u64, false, &mut added);
                if nb != b + run as u32 || find_slot(nb).is_some() { break; }
                run += 1;
            }
            let sl = core::slice::from_raw_parts_mut(dst.add(k * BS), run * BS);
            if !crate::fs::blk_read((b as usize) * SPB, sl) { return EIO; }
            STATS_RD += run as u64;
            k += run;
        }
        want as i64
    }
}

/// How many blocks to take ahead for this descriptor: zero while the reads go
/// scattered; a growing window while they go back to back. It updates the state.
/// SAFETY: E1 — under the FS lock; `i` is a row index the caller got from `of()`, hence
/// `< MAXOPEN`.
unsafe fn ra_hint(i: usize, off: u64, len: usize) -> usize {
    let win = RA_WINDOW.load(core::sync::atomic::Ordering::Relaxed);
    if win < 2 { OF_RANEXT[i] = off + len as u64; return 0; }
    let seq = OF_RANEXT[i] == off;
    let blocks = len.div_ceil(BS);
    OF_RAWIN[i] = if seq { (OF_RAWIN[i] as usize * 2).clamp(8, win) as u16 } else { 0 };
    OF_RANEXT[i] = off + len as u64;
    if seq { (OF_RAWIN[i] as usize).max(blocks) } else { 0 }
}

pub fn pread(h: i64, buf: *mut u8, len: usize, off: u64) -> i64 {
    let _fsg = fs_lock();
    let i = match of(h) { Some(i) => i, None => return EBADF };
    // SAFETY: E1 — the FS lock is taken on the line above. The raw pointer is a PROCESS
    // pointer already checked by `uaccess` for this length and direction; it cannot point
    // into the block cache, which is mapped without U/S (see `ino_pread`).
    unsafe {
        let ino = OF_INO[i]; let mut inb = [0u8; 128]; ino_read(ino, &mut inb);
        if is_dir(&inb) { return EISDIR; }
        let ra = ra_hint(i, off, len);
        ino_pread(ino, &mut inb, off, buf, len, ra) as i64
    }
}
pub fn read(h: i64, buf: *mut u8, len: usize) -> i64 {
    let _fsg = fs_lock();
    let i = match of(h) { Some(i) => i, None => return EBADF };
    // SAFETY: `i` came from `of()`, hence `< MAXOPEN`; the position is this row's own.
    let r = pread(h, buf, len, unsafe { OF_POS[i] });
    // SAFETY: as above — the row's position advances by what was actually read.
    if r > 0 { unsafe { OF_POS[i] += r as u64; } }
    r
}
/// §8.10 A LONG WRITE GOES IN PORTIONS WITH THE BALANCE OF THE DIRTY DATA.
///
/// One write/fallocate of 100 MB (that is how WiredTiger creates a journal file)
/// went through `ino_pwrite` entirely under the FS lock: the cache (85 MB) became
/// dirty ENTIRELY, every next block evicted a dirty one with a synchronous write
/// one by one (3900 writes = 1.2 s on a gp3-class disk), and all that time
/// the FS stood still for everybody. Now the write goes in portions of BAL_BLOCKS blocks, and
/// between the portions the same balance works as at the entry into a write (§8.7): the dirty data
/// leave without the FS lock, the neighbours get through. The inode after a portion is reread —
/// while there was no lock, another writer of the same file could have touched it.
const BAL_BLOCKS: usize = 64;
/// SAFETY: E1 + E4 — under the FS lock, and this is one of the places that MAY let it go: the
/// balance flushes in portions (§8.6). That is sound here because everything this frame
/// holds is its own — `inb` is a copy of the inode and `src` is the process's buffer — and
/// no borrow into the cache is alive across the flush (E2).
unsafe fn ino_pwrite_balanced(ino: u32, inb: &mut [u8; 128], off: u64, src: *const u8, len: usize) -> i64 {
    let mut done = 0usize;
    while done < len {
        let piece = (len - done).min(BAL_BLOCKS * BS);
        let r = ino_pwrite(ino, inb, off + done as u64, src.add(done), piece);
        if r < 0 { return if done == 0 { r } else { done as i64 }; }
        done += r as usize;
        if (r as usize) < piece { break; }
        if done < len {
            dirty_balance();
            if WRWIN.load(core::sync::atomic::Ordering::Relaxed) { falloc_window(); }
            ino_read(ino, inb);
        }
    }
    done as i64
}
pub fn pwrite(h: i64, buf: *const u8, len: usize, off: u64) -> i64 {
    let _fsg = fs_lock();
    if let Some(e) = ro_err() { return e; }
    // §8.7 and the flush by age (session 40): with WiredTiger the whole write path is
    // a `pwrite64` (the syscall #18, session 34), and a knob that is not placed here would not
    // work on the very workload for whose sake it was introduced.
    // SAFETY: E1 + E4 — under the lock; both may flush in portions and give the lock up
    // around the device, which is allowed here because this frame holds nothing of its own.
    unsafe { dirty_balance(); dirty_age_flush(); }
    let i = match of(h) { Some(i) => i, None => return EBADF };
    // SAFETY: E1 — the lock is held by the caller's guard above; `i` came from `of()`.
    unsafe { let ino = OF_INO[i]; let mut inb = [0u8; 128]; ino_read(ino, &mut inb); if is_dir(&inb) { return EISDIR; } ino_pwrite_balanced(ino, &mut inb, off,
        buf, len) }
}
pub fn write(h: i64, buf: *const u8, len: usize) -> i64 {
    let _fsg = fs_lock();
    if let Some(e) = ro_err() { return e; }
    // SAFETY: E1 + E4 — as in `pwrite`.
    unsafe { dirty_balance(); dirty_age_flush(); } // §8.7 and the flush by age (session 40)
    let i = match of(h) { Some(i) => i, None => return EBADF };
    // SAFETY: E1 — the FS lock is taken on the line above. The raw pointer is a PROCESS
    // pointer already checked by `uaccess` for this length and direction; it cannot point
    // into the block cache, which is mapped without U/S (see `ino_pread`).
    unsafe {
        let ino = OF_INO[i]; let mut inb = [0u8; 128]; ino_read(ino, &mut inb);
        if is_dir(&inb) { return EISDIR; }
        let off = if OF_FL[i] & O_APPEND as u32 != 0 { i_size(&inb) } else { OF_POS[i] };
        let r = ino_pwrite_balanced(ino, &mut inb, off, buf, len);
        if r > 0 { OF_POS[i] = off + r as u64; }
        r
    }
}
pub fn lseek(h: i64, off: i64, whence: i64) -> i64 {
    let _fsg = fs_lock();
    let i = match of(h) { Some(i) => i, None => return EBADF };
    // SAFETY: E1 — the FS lock is taken on the line above; the body is ordinary ext2 work.
    unsafe {
        let base: i64 = match whence { 0 => 0, 1 => OF_POS[i] as i64, 2 => { let mut inb = [0u8; 128]; ino_read(OF_INO[i], &mut inb); i_size(&inb) as i64 }
            _ => return EINVAL };
        let np = base + off; if np < 0 { return EINVAL; }
        OF_POS[i] = np as u64; np
    }
}
pub fn ftruncate(h: i64, len: u64) -> i64 {
    let _fsg = fs_lock();
    if let Some(e) = ro_err() { return e; }
    let i = match of(h) { Some(i) => i, None => return EBADF };
    // SAFETY: E1 — as above; `i` came from `of()`, hence `< MAXOPEN`.
    unsafe { let ino = OF_INO[i]; let mut inb = [0u8; 128]; ino_read(ino, &mut inb); if is_dir(&inb) { return EISDIR; } truncate_to(ino, &mut inb, len);
        wr32(&mut inb, 16, now()); ino_write(ino, &inb); 0 }
}
pub fn truncate(path: &[u8], len: u64) -> i64 {
    let _fsg = fs_lock();
    if let Some(e) = ro_err() { return e; }
    // SAFETY: E1 — the lock is held above; the inode travels as the frame's own copy.
    unsafe { let ino = match lookup(path) { Ok(n) => n, Err(e) => return e }; let mut inb = [0u8; 128]; ino_read(ino, &mut inb); if is_dir(&inb) {
        return EISDIR; } truncate_to(ino, &mut inb, len); ino_write(ino, &inb); 0 }
}
/// fallocate(mode=0): append zeroes up to off+len (real blocks).
pub fn fallocate(h: i64, off: u64, len: u64) -> i64 {
    let _fsg = fs_lock();
    if let Some(e) = ro_err() { return e; }
    let i = match of(h) { Some(i) => i, None => return EBADF };
    // SAFETY: E1 — the FS lock is taken on the line above; the body is ordinary ext2 work.
    unsafe {
        let ino = OF_INO[i]; let mut inb = [0u8; 128]; ino_read(ino, &mut inb);
        let end = off + len; let size = i_size(&inb);
        let start = if size > off { size } else { off };
        static mut Z: [u8; 4096] = [0; 4096];
        let mut pos = start;
        let mut since = 0usize;
        while pos < end {
            let n = ((end - pos) as usize).min(4096);
            let r = ino_pwrite(ino, &mut inb, pos, core::ptr::addr_of!(Z) as *const u8, n);
            if r < 0 { return r; }
            pos += r as u64;
            since += 1;
            if since >= BAL_BLOCKS && pos < end { since = 0; dirty_balance(); falloc_window(); ino_read(ino, &mut inb); } // §8.10
        }
        0
    }
}
/// THE PREEMPTION WINDOW IN A LONG WRITE (session 32 — fallocate, session 35 — write).
///
/// The syscalls are executed with IF=0, and WiredTiger's fallocate (an appending of zeroes up to
/// 32 MB by 4 KB under the FS lock) held the core for 18 ms without a single point of yielding:
/// the instrument "the tick's gap" named #285 on core 1, while "the worst ready->cpu"
/// showed a conn thread woken on the same core and waiting exactly those 18.4 ms.
/// The waiters for the FS lock yield to each other (`fs_lock`), but THE OWNER yielded to
/// nobody. Once every BAL_BLOCKS blocks the lock is released entirely and the core is given to a
/// ready thread (if there is one) or an interrupt window is opened so that the tick
/// reaches the timers; the inode after that is reread (as after
/// `dirty_balance`).
///
/// SESSION 35: the same window goes into an ordinary write too (`ino_pwrite_balanced`). The instrument
/// named there the same gap as in the fallocate, only with the syscall `#18 pwrite64`
/// (16-18 ms, 786 and 421 gaps >=4 ms per boot with ZERO host time): a portion of
/// BAL_BLOCKS blocks evicts the dirty blocks from the cache one by one, and every
/// eviction is a command to EBS. Between the portions there is no state above for exactly
/// the same reason as in the fallocate: the inode is reread, while the dirty data have already
/// left by `dirty_balance`. The knob `talaria.wrwin=0` returns the former
/// behaviour (the window only in the fallocate) — it is for an A/B on the stand.
static WRWIN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
pub fn wrwin_from_cmdline(cmdline: &str) {
    if crate::cmdline::value(cmdline, "talaria.wrwin") == Some("0") {
        WRWIN.store(false, core::sync::atomic::Ordering::SeqCst);
        crate::println!("ext2: the preemption window in a long write is off (talaria.wrwin=0 — as before session 35)");
    }
}
pub static FALLOC_WIN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static FALLOC_YIELD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// SAFETY: E1 — under the FS lock; it walks the preallocation windows, the same table as
/// `alloc_data`.
unsafe fn falloc_window() {
    use core::sync::atomic::Ordering::Relaxed;
    if !crate::syscall::THREADED.load(Relaxed) { return; }
    let _rel = MayRelease::set(true); // there is no state above: the inode is reread
    let depth = fs_unlock_outermost();
    if depth == 0 { return; }
    FALLOC_WIN.fetch_add(1, Relaxed);
    let cpu = crate::percpu::cpu_id();
    if crate::sched::cpu_has_other_runnable(cpu, crate::sched::current_id()) {
        FALLOC_YIELD.fetch_add(1, Relaxed);
        crate::sched::yield_origin(crate::sched::YO_FS); crate::sched::yield_now();
    } else {
        crate::arch::cpu::irq_enable();
        for _ in 0..64 { core::hint::spin_loop(); }
        crate::arch::cpu::irq_disable();
    }
    fs_relock(depth);
}
/// fsync: the dirty blocks -> the disk + a virtio flush (a write-back cache; a simple strategy — everything at once).
/// §8.3 fsync(fd): ONLY this inode's data blocks and the dirty
/// metadata (the inodes/the bitmaps/the GDT/the superblock/the indirect ones — their owner is 0) go to the disk
/// rather than the whole cache, as before. Other files' data stay dirty — and that
/// is the point of an fsync of one descriptor.
pub fn fsync(h: i64) -> i64 {
    let t0 = crate::tsc::rdtsc();
    let me = crate::sched::current_id().min(crate::sched::NTASKS - 1);
    // SAFETY: the phase instrument's tables are indexed by task and `me` is this task, so
    // only our own row is touched; `me` is bounded by the caller.
    let outer = unsafe { let o = PH_IN[me]; PH_IN[me] = true; PH_ACC[me] = 0; o };
    let r = fsync_inner(h);
    let whole = crate::tsc::rdtsc().wrapping_sub(t0);
    ph_note(0, t0);
    // SAFETY: E1 — the FS lock is taken on the line above; the body is ordinary ext2 work.
    unsafe {
        if !outer {
            use core::sync::atomic::Ordering::Relaxed;
            let rest = whole.saturating_sub(PH_ACC[me]);
            FSYNC_PH[12].n.fetch_add(1, Relaxed);
            FSYNC_PH[12].tsc.fetch_add(rest, Relaxed);
            FSYNC_PH[12].max.fetch_max(rest, Relaxed);
        }
        PH_IN[me] = outer;
    }
    r
}
fn fsync_inner(h: i64) -> i64 {
    let tl = crate::tsc::rdtsc();
    let _fsg = fs_lock();
    ph_note(7, tl);
    if !active() { return 0; }
    let snap = crate::fs::WRITE_FAILED.load(core::sync::atomic::Ordering::SeqCst);
    // THE DETAILS OF "THE OTHER" (session 38): three items lying BETWEEN the phases, and
    // therefore falling into the uncovered remainder (171 us per call).
    let th = crate::tsc::rdtsc();
    mmap_harvest(0, 0); // §8.5
    ph_note(18, th);

    // SAFETY: E1 — the lock is held above; `i` came from `of()`.
    let ino = match of(h) { Some(i) => unsafe { OF_INO[i] }, None => { flush_all(); return io_err_since(snap); } };
    // STEP 2'S INSTRUMENT: the file's blocks are taken BEFORE the entry into the transaction (under JBUSY a miss
    // in the inode cache would mean an eviction inside the transaction).
    // SAFETY: E1 — the FS lock is taken on the line above; the body is ordinary ext2 work.
    unsafe {
        let t = crate::sched::current_id().min(crate::sched::NTASKS - 1);
        let mut inb = [0u8; 128]; ino_read(ino, &mut inb);
        FSD_FBLK[t] = (rd32(&inb, 28) as usize / SPB.max(1)) as u32;
    }
    let _rel = MayRelease::set(true); // an independent fsync: there is no state above
    // SAFETY: E1 — the FS lock is taken on the line above; the body is ordinary ext2 work.
    unsafe {
        let tp = crate::tsc::rdtsc();
        prea_release(ino); // §2.4
        ph_note(19, tp);
        // The journal (§8.4): the metadata get onto the disk through a transaction with barriers.
        if crate::ext2::jrn_active() { let r = jrn_commit(ino); if r != 0 { return r; } return io_err_since(snap); }
        let n = writeback(Sel::Own(ino), WbC::Fsync);
        if n > 0 { dev_barrier(); }
    }
    io_err_since(snap)
}

/// A stat buffer (a struct stat x86_64, 144 bytes) by the inode.
/// SAFETY: E1 — under the FS lock; `st` is the process's `struct stat`, checked writable by
/// `uaccess` for its full length and unable to point into the cache (no U/S).
unsafe fn fill_stat(ino: u32, inb: &[u8; 128], st: *mut u8) {
    core::ptr::write_bytes(st, 0, 144);
    core::ptr::write_unaligned(st as *mut u64, 0x801);                       // st_dev
    core::ptr::write_unaligned(st.add(8) as *mut u64, ino as u64);           // st_ino
    core::ptr::write_unaligned(st.add(16) as *mut u64, i_links(inb) as u64); // st_nlink
    core::ptr::write_unaligned(st.add(24) as *mut u32, i_mode(inb) as u32);  // st_mode
    // uid/gid = geteuid() (PostgreSQL checks the owner of the data directory)
    core::ptr::write_unaligned(st.add(28) as *mut u32, 1000);
    core::ptr::write_unaligned(st.add(32) as *mut u32, 1000);
    core::ptr::write_unaligned(st.add(48) as *mut i64, i_size(inb) as i64);  // st_size
    core::ptr::write_unaligned(st.add(56) as *mut i64, BS as i64);           // st_blksize
    core::ptr::write_unaligned(st.add(64) as *mut i64, rd32(inb, 28) as i64); // st_blocks (of 512 bytes)
    core::ptr::write_unaligned(st.add(72) as *mut i64, rd32(inb, 8) as i64);  // atime
    core::ptr::write_unaligned(st.add(88) as *mut i64, rd32(inb, 16) as i64); // mtime
    core::ptr::write_unaligned(st.add(104) as *mut i64, rd32(inb, 12) as i64); // ctime
}
pub fn fstat(h: i64, st: *mut u8) -> i64 {
    let _fsg = fs_lock();
    let i = match of(h) { Some(i) => i, None => return EBADF };
    // SAFETY: E1 — the lock is held above; `st` is a process pointer checked by `uaccess`.
    unsafe { let ino = OF_INO[i]; let mut inb = [0u8; 128]; ino_read(ino, &mut inb); fill_stat(ino, &inb, st); 0 }
}
pub fn stat(path: &[u8], st: *mut u8) -> i64 {
    let _fsg = fs_lock();
    // SAFETY: E1 — the lock is held above; `st` is a process pointer checked by `uaccess`.
    unsafe { let ino = match lookup_follow(path) { Ok(n) => n, Err(e) => return e }; let mut inb = [0u8; 128]; ino_read(ino, &mut inb); fill_stat(ino, &inb,
        st); 0 }
}
/// SAFETY: E1 — the lock is taken on the same line (it was NOT, once: see the rule at
/// `fs_lock` for what a lockless reader did to the file system).
pub fn exists(path: &[u8]) -> bool { let _fsg = fs_lock(); unsafe { lookup_follow(path).is_ok() } }
/// SAFETY: E1 — the lock is taken on the same line.
pub fn is_dir_path(path: &[u8]) -> bool { let _fsg = fs_lock(); unsafe { match lookup_follow(path) { Ok(n) => { let mut inb = [0u8; 128]; ino_read(n, &mut inb);
    is_dir(&inb) } Err(_) => false } } }

pub fn mkdir(path: &[u8], mode: u32) -> i64 {
    let _fsg = fs_lock();
    if let Some(e) = ro_err() { return e; }
    // SAFETY: E1 — the FS lock is taken on the line above; the body is ordinary ext2 work.
    unsafe {
        let (p, s, l) = match walk_parent(path) { Ok(x) => x, Err(e) => return e };
        if p == 0 { return EEXIST; }
        let name = &path[s..s + l];
        let mut pinb = [0u8; 128]; ino_read(p, &mut pinb);
        if !is_dir(&pinb) { return ENOTDIR; }
        if dir_lookup(p, &mut pinb, name).is_some() { return EEXIST; }
        let goal = ((p - 1) / INODES_PER_GROUP) as usize;
        let n = alloc_inode(goal, true); if n == 0 { return ENOSPC; }
        let mut nb = [0u8; 128];
        wr16(&mut nb, 0, 0x4000 | (mode as u16 & 0o7777)); wr16(&mut nb, 26, 2);
        let t = now(); wr32(&mut nb, 8, t); wr32(&mut nb, 12, t); wr32(&mut nb, 16, t);
        let b = alloc_block(goal); if b == 0 { free_inode(n, true); return ENOSPC; }
        wr32(&mut nb, 40, b); set_size(&mut nb, BS as u64); wr32(&mut nb, 28, SPB as u32);
        let d = blk(b);
        wr32(d, 0, n); wr16(d, 4, 12); d[6] = 1; d[7] = 2; d[8] = b'.';
        wr32(d, 12, p); wr16(d, 16, (BS - 12) as u16); d[18] = 2; d[19] = 2; d[20] = b'.'; d[21] = b'.';
        cflush(b);
        ino_write(n, &nb);
        let r = dir_add(p, &mut pinb, name, n, 2); if r < 0 { return r; }
        ino_read(p, &mut pinb); let lk = i_links(&pinb) + 1; wr16(&mut pinb, 26, lk); wr32(&mut pinb, 16, t); ino_write(p, &pinb);
        0
    }
}
/// unlink (a file) / rmdir (a directory) by the flag want_dir.
pub fn unlink(path: &[u8], want_dir: bool) -> i64 {
    let _fsg = fs_lock();
    if let Some(e) = ro_err() { return e; }
    // SAFETY: E1 — the FS lock is taken on the line above; the body is ordinary ext2 work.
    unsafe {
        let (p, s, l) = match walk_parent(path) { Ok(x) => x, Err(e) => return e };
        if p == 0 { return EPERM; }
        let name = &path[s..s + l];
        let mut pinb = [0u8; 128]; ino_read(p, &mut pinb);
        let (ino, _) = match dir_lookup(p, &mut pinb, name) { Some(x) => x, None => return ENOENT };
        let mut inb = [0u8; 128]; ino_read(ino, &mut inb);
        if is_dir(&inb) {
            if !want_dir { return EISDIR; }
            if !dir_is_empty(ino, &mut inb) { return ENOTEMPTY; }
            dir_remove(p, &mut pinb, name);
            prea_release(ino); truncate_zero(&mut inb); wr16(&mut inb, 26, 0); wr32(&mut inb, 20, now()); ino_write(ino, &inb);
            free_inode(ino, true);
            ino_read(p, &mut pinb); let lk = i_links(&pinb); if lk > 2 { wr16(&mut pinb, 26, lk - 1); } ino_write(p, &pinb);
            return 0;
        }
        if want_dir { return ENOTDIR; }
        dir_remove(p, &mut pinb, name);
        let lk = i_links(&inb);
        if lk <= 1 {
            // the open descriptors for this inode keep working with the data (we do not free it if it is open)
            let mut open_cnt = 0; for h in 0..MAXOPEN { if OF_INO[h] == ino { open_cnt += 1; } }
            if open_cnt == 0 { prea_release(ino); truncate_zero(&mut inb); free_inode(ino, false); }
            wr16(&mut inb, 26, 0); wr32(&mut inb, 20, now()); ino_write(ino, &inb);
            if open_cnt > 0 { ORPHAN_PENDING = true; }
        } else { wr16(&mut inb, 26, lk - 1); ino_write(ino, &inb); }
        0
    }
}
static mut ORPHAN_PENDING: bool = false;
pub fn rename(old: &[u8], new: &[u8]) -> i64 {
    let _fsg = fs_lock();
    if let Some(e) = ro_err() { return e; }
    // SAFETY: E1 — the FS lock is taken on the line above; the body is ordinary ext2 work.
    unsafe {
        let (op, os, ol) = match walk_parent(old) { Ok(x) => x, Err(e) => return e };
        let (np, ns, nl) = match walk_parent(new) { Ok(x) => x, Err(e) => return e };
        if op == 0 || np == 0 { return EPERM; }
        let oname = &old[os..os + ol]; let nname = &new[ns..ns + nl];
        let mut opinb = [0u8; 128]; ino_read(op, &mut opinb);
        let (ino, ft) = match dir_lookup(op, &mut opinb, oname) { Some(x) => x, None => return ENOENT };
        let mut npinb = [0u8; 128]; ino_read(np, &mut npinb);
        if !is_dir(&npinb) { return ENOTDIR; }
        if let Some((existing, _)) = dir_lookup(np, &mut npinb, nname) {
            if existing == ino { return 0; }
            let mut einb = [0u8; 128]; ino_read(existing, &mut einb);
            let r = unlink(new, is_dir(&einb)); if r < 0 { return r; }
            ino_read(np, &mut npinb);
        }
        let r = dir_add(np, &mut npinb, nname, ino, ft); if r < 0 { return r; }
        ino_read(op, &mut opinb);
        dir_remove(op, &mut opinb, oname);
        let mut inb = [0u8; 128]; ino_read(ino, &mut inb);
        if is_dir(&inb) && op != np {
            // update ".." and the parents' link counters
            let mut add = 0; let b = bmap(ino, &mut inb, 0, false, &mut add);
            if b != 0 { let d = blk(b); let rl = rd16(d, 4) as usize; if rd32(d, rl) != 0 && d[rl + 6] == 2 { wr32(d, rl, np); cflush(b); } }
            ino_read(op, &mut opinb); let lk = i_links(&opinb); if lk > 2 { wr16(&mut opinb, 26, lk - 1); } ino_write(op, &opinb);
            ino_read(np, &mut npinb); let lk = i_links(&npinb) + 1; wr16(&mut npinb, 26, lk); ino_write(np, &npinb);
        }
        0
    }
}
/// getdents64(handle, buf, len): linux_dirent64 {ino u64, off i64, reclen u16, type u8, name}.
pub fn getdents64(h: i64, buf: *mut u8, len: usize) -> i64 {
    let _fsg = fs_lock();
    let i = match of(h) { Some(i) => i, None => return EBADF };
    // SAFETY: E1 — the lock is taken above. `buf` is a PROCESS pointer checked writable by
    // `uaccess` for `len`, and unable to point into the cache (no U/S); the entries are
    // written with an explicit bound check against `len` at every record.
    unsafe {
        let ino = OF_INO[i]; let mut inb = [0u8; 128]; ino_read(ino, &mut inb);
        if !is_dir(&inb) { return ENOTDIR; }
        let size = i_size(&inb);
        let mut pos = OF_POS[i];
        let mut out = 0usize;
        while pos < size {
            let fb = pos / BS as u64; let bo = (pos % BS as u64) as usize;
            let mut add = 0;
            let b = bmap(ino, &mut inb, fb, false, &mut add);
            if b == 0 { pos = (fb + 1) * BS as u64; continue; }
            let d = blk(b);
            if bo + 8 > BS { pos = (fb + 1) * BS as u64; continue; }
            let eino = rd32(d, bo); let rl = rd16(d, bo + 4) as usize; let nl = d[bo + 6] as usize; let ft = d[bo + 7];
            if rl < 8 { pos = (fb + 1) * BS as u64; continue; }
            let next = pos + rl as u64;
            if eino != 0 {
                let rec = (19 + nl + 1 + 7) & !7;
                if out + rec > len { break; }
                let p = buf.add(out);
                core::ptr::write_unaligned(p as *mut u64, eino as u64);
                core::ptr::write_unaligned(p.add(8) as *mut i64, next as i64);
                core::ptr::write_unaligned(p.add(16) as *mut u16, rec as u16);
                core::ptr::write(p.add(18), match ft { 1 => 8, 2 => 4, 7 => 10, _ => 0 }); // DT_REG/DT_DIR/DT_LNK
                core::ptr::copy_nonoverlapping(d.as_ptr().add(bo + 8), p.add(19), nl);
                core::ptr::write(p.add(19 + nl), 0);
                out += rec;
            }
            pos = next;
        }
        OF_POS[i] = pos;
        if out == 0 && OF_POS[i] < size { return EINVAL; } // the buffer is too small
        out as i64
    }
}
/// Read a whole file by path into dst (for the ELF loader: streaming is better through open/read).
pub fn read_all(path: &[u8], dst: &mut [u8]) -> usize {
    let _fsg = fs_lock();
    // SAFETY: E1 — the lock is taken on the line above; `dst` is the kernel's own buffer
    // (the ELF loader's), not a process pointer.
    unsafe {
        let ino = match lookup(path) { Ok(n) => n, Err(_) => return 0 };
        let mut inb = [0u8; 128]; ino_read(ino, &mut inb);
        ino_pread(ino, &mut inb, 0, dst.as_mut_ptr(), dst.len(), 0)
    }
}
/// A file's size by handle.
/// SAFETY: E1 — the lock is taken on the same line (another of the readers that once went
/// without it).
pub fn fd_size(h: i64) -> u64 { let _fsg = fs_lock(); match of(h) { Some(i) => unsafe { let mut inb = [0u8; 128]; ino_read(OF_INO[i], &mut inb); i_size(&inb) },
    None => 0 } }

/// A self-test (talaria.test=ext2): create/write/read/rename/delete, the directories, a large file.
/// A self-test of the symbolic links: a read through a link, a link IN THE MIDDLE of a path,
/// readlink, the difference between stat/lstat, the creation of a link by the kernel and a cycle (an ELOOP).
/// The links are put into the image by test.sh (`ln -s`), and they are "fast" ones (the target is in the inode).
/// The gate `ioerr`: the device is substituted with a refusing one (`fs::FAIL_WRITES`) —
/// fsync/sync answer EIO rather than 0; after a refusal the FS is read-only (an EROFS at a
/// write/open(O_CREAT)/unlink); after the device is returned the data are read by
/// what they were written with.
pub fn ioerr_selftest() -> bool {
    use core::sync::atomic::Ordering::SeqCst;
    let mut ok = true;
    let mut check = |c: bool, what: &str| { if !c { crate::println!("ioerr: FAIL {}", what); ok = false; } else { crate::println!("ioerr: ok {}", what); } };
    let h = open(b"/ioerr.txt", O_CREAT | O_TRUNC | 2, 0o644);
    check(h >= 0, "create /ioerr.txt on a healthy device");
    check(write(h, b"hello".as_ptr(), 5) == 5, "write 5 bytes");
    check(fsync(h) == 0, "an fsync on a healthy device -> 0");
    crate::fs::FAIL_WRITES.store(true, SeqCst);
    check(write(h, b"world".as_ptr(), 5) == 5, "a write into the cache before the first refusal goes through");
    let r = fsync(h);
    check(r == EIO, "an fsync at a device refusal -> EIO (rather than 0)");
    check(crate::fs::WRITE_FAILED.load(SeqCst) > 0, "the refusal is counted (fs::WRITE_FAILED)");
    check(write(h, b"!".as_ptr(), 1) == EROFS, "a write after a refusal -> EROFS");
    check(open(b"/ioerr2.txt", O_CREAT | 2, 0o644) == EROFS, "an open(O_CREAT) after a refusal -> EROFS");
    check(open(b"/ioerr.txt", 0, 0) >= 0, "an open for reading after a refusal is allowed");
    check(unlink(b"/ioerr.txt", false) == EROFS, "an unlink after a refusal -> EROFS");
    check(sync_all() == EIO, "a sync(2) at a refusal -> EIO");
    crate::fs::FAIL_WRITES.store(false, SeqCst);
    ro_reset_for_test();
    check(fsync(h) == 0, "an fsync after the device is returned -> 0");
    check(close(h) == 0, "close");
    let h = open(b"/ioerr.txt", 0, 0);
    let mut buf = [0u8; 16];
    let n = read(h, buf.as_mut_ptr(), buf.len());
    check(n == 10 && &buf[..10] == b"helloworld", "the file is read by what it was written with (helloworld)");
    let _ = close(h);
    let _ = unlink(b"/ioerr.txt", false);
    crate::println!("ioerr: {}", if ok { "ALL OK" } else { "FAILED" });
    ok
}
pub fn symlink_selftest() -> bool {
    let _fsg = fs_lock();
    let mut ok = true;
    let mut check = |c: bool, what: &str| { if !c { crate::println!("symlink-test: FAIL {}", what); ok = false; } else { crate::println!("symlink-test: ok {}",
        what); } };
    // 1. open a file THROUGH a link — the target's content must be read
    let mut buf = [0u8; 64];
    let h = open(b"/link.txt", 0, 0);
    let n = if h >= 0 { read(h, buf.as_mut_ptr(), 64) } else { -1 };
    if h >= 0 { close(h); }
    check(n > 0 && &buf[..5] == b"hello", "a read through the link /link.txt");
    // 2. readlink returns the target
    let mut tb = [0u8; 64];
    let r = readlink(b"/link.txt", tb.as_mut_ptr(), 64);
    check(r == 9 && &tb[..9] == b"hello.txt", "readlink -> hello.txt");
    // 3. a link IN THE MIDDLE of a path (dirlink -> dir)
    let h = open(b"/dirlink/inside.txt", 0, 0);
    let n = if h >= 0 { read(h, buf.as_mut_ptr(), 64) } else { -1 };
    if h >= 0 { close(h); }
    check(n > 0 && &buf[..6] == b"inside", "a link in the middle of the path /dirlink/inside.txt");
    // 4. stat goes by the link, lstat is about the link itself
    let mut st1 = [0u8; 144]; let mut st2 = [0u8; 144];
    let a = stat(b"/link.txt", st1.as_mut_ptr());
    let b = lstat(b"/link.txt", st2.as_mut_ptr());
    // SAFETY: the two buffers are the selftest's own `struct stat` arrays, filled by
    // `stat`/`lstat` just above; the mode field sits at offset 24 and is read unaligned
    // because the buffer is a byte array.
    let m1 = unsafe { core::ptr::read_unaligned(st1.as_ptr().add(24) as *const u32) } & 0xF000;
    let m2 = unsafe { core::ptr::read_unaligned(st2.as_ptr().add(24) as *const u32) } & 0xF000;
    check(a == 0 && b == 0 && m1 == 0x8000 && m2 == 0xA000, "stat = the file, lstat = the link");
    // 5. the kernel itself creates a link, and the target is read through it
    let _ = unlink(b"/mklink.txt", false);
    check(symlink(b"hello.txt", b"/mklink.txt") == 0, "symlink() creates a link");
    let h = open(b"/mklink.txt", 0, 0);
    let n = if h >= 0 { read(h, buf.as_mut_ptr(), 64) } else { -1 };
    if h >= 0 { close(h); }
    check(n > 0 && &buf[..5] == b"hello", "a read through a link created by the kernel");
    // 6. a cycle of links must end with an ELOOP rather than with a hang
    let _ = unlink(b"/loopa", false); let _ = unlink(b"/loopb", false);
    let _ = symlink(b"loopb", b"/loopa");
    let _ = symlink(b"loopa", b"/loopb");
    check(open(b"/loopa", 0, 0) == ELOOP, "a cycle of links -> ELOOP");
    let _ = unlink(b"/loopa", false); let _ = unlink(b"/loopb", false); let _ = unlink(b"/mklink.txt", false);
    flush_all();
    ok
}

pub fn selftest() -> bool {
    let _fsg = fs_lock();
    let mut ok = true;
    let mut check = |c: bool, what: &str| { if !c { crate::println!("ext2-test: FAIL {}", what); ok = false; } else { crate::println!("ext2-test: ok {}", what);
        } };
    let h = open(b"/t/hello.txt", O_CREAT | 2, 0o644);
    check(h >= 0 || mkdir(b"/t", 0o755) == 0, "mkdir /t");
    let h = if h < 0 { open(b"/t/hello.txt", O_CREAT | 2, 0o644) } else { h };
    check(h >= 0, "create /t/hello.txt");
    let msg = b"hello from talaria ext2\n";
    check(write(h, msg.as_ptr(), msg.len()) == msg.len() as i64, "write");
    check(close(h) == 0, "close");
    let h = open(b"/t/hello.txt", 0, 0);
    let mut buf = [0u8; 64];
    let n = read(h, buf.as_mut_ptr(), 64);
    check(n == msg.len() as i64 && &buf[..msg.len()] == msg, "read back");
    close(h);
    // a large file: 300 KB (the direct + the indirect ones)
    let h = open(b"/t/big.bin", O_CREAT | O_TRUNC | 2, 0o644);
    static mut PAT: [u8; 4096] = [0; 4096];
    // SAFETY: the selftest's own pattern buffer, written before it is used and by one task.
    unsafe { for i in 0..4096 { PAT[i] = (i * 7 + 3) as u8; } }
    let mut wok = true;
    for _ in 0..75 { if write(h, core::ptr::addr_of!(PAT) as *const u8, 4096) != 4096 { wok = false; break; } }
    check(wok, "write 300K");
    check(fd_size(h) == 75 * 4096, "size 300K");
    let mut rb = [0u8; 4096];
    let r = pread(h, rb.as_mut_ptr(), 4096, 70 * 4096);
    // SAFETY: the selftest's own pattern buffer, compared by value.
    check(r == 4096 && rb == unsafe { *core::ptr::addr_of!(PAT) }, "pread @70 blocks");
    check(ftruncate(h, 10 * 4096) == 0 && fd_size(h) == 10 * 4096, "ftruncate 40K");
    close(h);
    check(rename(b"/t/big.bin", b"/t/big2.bin") == 0 && exists(b"/t/big2.bin") && !exists(b"/t/big.bin"), "rename");
    check(unlink(b"/t/big2.bin", false) == 0 && !exists(b"/t/big2.bin"), "unlink");
    check(mkdir(b"/t/sub", 0o700) == 0 && is_dir_path(b"/t/sub"), "mkdir sub");
    let h = open(b"/t/sub/a", O_CREAT | 2, 0o600); close(h);
    check(unlink(b"/t/sub", true) == ENOTEMPTY, "rmdir non-empty -> ENOTEMPTY");
    check(unlink(b"/t/sub/a", false) == 0 && unlink(b"/t/sub", true) == 0, "rmdir after unlink");
    // a getdents64 over /t
    let h = open(b"/t", O_DIRECTORY, 0);
    let mut db = [0u8; 512];
    let n = getdents64(h, db.as_mut_ptr(), 512);
    check(n > 0, "getdents64 /t");
    close(h);
    fsync(0);
    // §2.1: the conflict misses. WAYS blocks OF ONE set (their numbers differ by
    // NSETS) must coexist in the cache simultaneously: with a direct mapping
    // every walk knocked the previous one out and gave WAYS misses per round.
    // SAFETY: E1 — the selftest runs on the boot path with the FS lock taken by the helpers
    // it calls; it reads the geometry to pick block numbers of one set.
    unsafe {
        let base = FIRST_DATA + 1;
        if base + (WAYS as u32) * (nsets() as u32) < TOTAL_BLOCKS {
            for k in 0..WAYS { let _ = cblk(base + (k * nsets()) as u32); }   // the warm-up
            let (r0, _) = stats();
            for _ in 0..8 { for k in 0..WAYS { let _ = cblk(base + (k * nsets()) as u32); } }
            let (r1, _) = stats();
            check(r1 == r0, "there are no conflict misses: WAYS blocks of one set live in the cache together");
        }
    }
    let (rd, wr) = stats();
    crate::println!("ext2-test: I/O blocks read={} written={}", rd, wr);
    ok
}

// ═══ §8.4 THE JOURNAL (the JBD2 format) + THE BARRIERS ════════════════════════
//
// What for: ext2 without a journal after a crash at a write leaves the metadata in
// an arbitrary intermediate state (the bitmap says "occupied", while the inode
// does not know about that block and vice versa) — that is cured only by a full e2fsck.
//
// What has been done: a transactional journal OF THE METADATA in the ordered mode, IN THE
// JBD2 FORMAT — the volume is created by the regular `mkfs.ext2 -O has_journal`, the journal lives in the inode
// s_journal_inum (usually 8), and our journal is read by Linux: e2fsck/Linux's kernel
// will replay it themselves. The converse is true too — a journal left by Linux is
// replayed by us (jrn_replay at the mounting).
//
// The fsync protocol (ordered):
//   1. the file's data go to the disk IN PLACE (otherwise after a replay the metadata
//      would point at garbage in fresh blocks);
//   2. A BARRIER (the device's Flush);
//   3. into the journal: a descriptor block with the tags + the COPIES of the dirty metadata in a row;
//   4. A BARRIER — the descriptor and the copies must land BEFORE the commit, otherwise a replay
//      will apply garbage;
//   5. a commit block with the same transaction number;
//   6. A BARRIER — after it the fsync is entitled to return: a crash at any moment further on
//      is recovered by a replay.
// The metadata are written IN PLACE later (a checkpoint) — at the filling of the journal,
// at the eviction of a dirty slot, at a sync/a stop. And that is the gain: at an fsync
// the metadata leave in one sequential write into the journal instead of scattered
// over the disk.
use crate::jbd2::{MAGIC as JMAGIC, BT_DESCRIPTOR as JBT_DESCRIPTOR, BT_COMMIT as JBT_COMMIT,
                  BT_SB_V2 as JBT_SB_V2};
/// A descriptor and its copies go to the device in one batch (§2.3), and the batch is
/// JREQS records: slot 0 for the descriptor, the rest for the blocks.
const JREQS: usize = 64;
/// How many blocks one descriptor REALLY describes: the tags in a block
/// (the first one also carries 16 bytes of the UUID, and one tag is kept in reserve) — but not
/// more than fits into the batch. Counting by the tags while forgetting about the batch cost
/// durability: a transaction of 1018 blocks was estimated at 3 descriptors while it
/// occupied 17, and it MOVED across the end of the journal onto its own descriptor in
/// block 1. The fsync returned, while after a crash there was nothing to replay
/// (the gate journalbig1/journalbig2 — the journal is 1024 blocks, the file is 1500).
/// SAFETY: E1 — under the FS lock; pure arithmetic over the geometry.
#[inline]
unsafe fn jrn_per_desc() -> usize { crate::jbd2::per_desc(BS, JREQS) }
/// The log blocks per transaction of `want` blocks: the descriptors + the copies + the commit.
/// SAFETY: E1 — under the FS lock; pure arithmetic over the geometry.
#[inline]
unsafe fn jrn_need(want: usize) -> usize { crate::jbd2::need(BS, JREQS, want) }
const JMAX: usize = 8192;          // the ceiling of the journal's length (blocks), 32 MB with 4K

static mut JRN: bool = false;              // the journal is active (it has been found and is compatible)
static mut JBLK: [u32; JMAX] = [0; JMAX];  // a log block -> an FS block
static mut JLEN: usize = 0;                // the journal's length in blocks (s_maxlen)
static mut JFIRST: usize = 1;              // the first log block (s_first)
static mut JSEQ: u32 = 1;                  // the number of the NEXT transaction
static mut JHEAD: usize = 1;               // where to write the next transaction
static mut JDIRTY: bool = false;           // there is something committed but not checkpointed in the journal
static mut JCKPT_OLD: u64 = 0;              // §8.9: how many times a checkpoint put a copy from the journal in place
static mut JRECOVER: bool = false;         // the recover flag in the FS's superblock is set (once per mounting epoch)
static mut JSB_STALE: bool = true;         // the journal's superblock does not describe JFIRST/JSEQ — write it before a transaction
static mut JSTAGE: u64 = 0;                // 2 pages: the descriptor + the commit
// §8.12 THE SNAPSHOTS OF THE COPIES FOR THE DESCRIPTOR BATCH: JREQS-1 blocks. Previously the copies
// went by DMA straight from the cache's slots, and for the whole time of the batch (≈0.9 ms on gp3 —
// two round trips of EBS) the FS lock was held: without it the workload's thread could
// overwrite a slot under a DMA. Holding the lock also meant NOT YIELDING the core (see
// fs.rs `io_may_yield`): the fsyncing thread spun the polling of the completion on the core,
// while a connection thread standing behind it in the queue of the same core waited those 0.9
// ms — exactly the p99.9 of a mongodb insert (1.2 ms with Linux's 0.35; the FS lock meanwhile
// was waited for 48 times over a run — it is not about it). Now the copies are taken under
// the lock into this buffer (~20 us for 63 blocks), the lock is released, the batch flies
// with a yield of the core. The semantics are the same as there were between the descriptor and the commit
// (there the lock was released before too): a slot changed after the snapshot is marked
// CJOLD, and it is put in place by a checkpoint from the journal (§8.9).
static mut JCOPY: u64 = 0;
static mut JCOMMITS: u64 = 0;              // how many transactions have been committed
static mut JCKPTS: u64 = 0;                // how many checkpoints there have been (the journal is emptied)
static mut JBLOCKS: u64 = 0;               // how many blocks have gone through the journal
static mut JBUSY: bool = false;            // a protection against recursion (an eviction inside a commit)
static mut JBUSY_SINCE: u64 = 0;           // the TSC of the start of the journal's occupancy (for a waiter's diagnosis)
static mut JBUSY_TASK: i64 = -1;           // whose transaction is flying
// SESSION 39: WHAT A FOREIGN TRANSACTION STANDS ON — AN ANSWER BY PHASE rather than "the journal is busy".
//
// Session 38's instrument (docs/MEASUREMENTS.md §3): the phase "a foreign transaction" costs 238 us per
// call with THREE events over 120 s — that is, one stop drags on for hundreds of
// milliseconds (in session 37 such a one was 193 ms). And that is the mechanics of `p99.9`:
// a rare event with a huge price. `jrn_wait_free` named the holder by the task's
// number, while WHAT it stood on it did not, and "the journal was busy" does not become a change.
//
// The holder writes its phase IN ONE WORD at its boundary; a waiter reads that word
// where it yields the core anyway (once every 64 turns of the spin) and accumulates the wait by
// the holder's phases. An episode of waiting is added into the common counters once, at the
// exit — otherwise the neighbouring waiters would jostle in the atomics for the whole spin.
const JSTAGES: usize = 9;
// The index 0 is read by a waiter ONLY while the journal is busy (they wait only under JBUSY),
// so it is named by the waiter's meaning: there is a holder, but it stands in none of
// the marked phases right now.
pub const JSTAGE_NAMES: [&str; JSTAGES] = ["between the transaction's phases", "the data in place", "the journal's superblock",
    "the descriptor+the copies", "the barrier before the commit", "the commit", "the checkpoint", "the tail of the deferred data",
    "the gate (artificially)"];
static mut JBUSY_STAGE: usize = 0;
/// SAFETY: E7 — written volatile because it is read by cores that hold NOTHING: a waiter
/// looks at the holder's phase while spinning. `i < JSTAGES` at every call site (the only
/// externally driven one, `jrn_stage_for_test`, clamps).
#[inline] unsafe fn jstage(i: usize) { core::ptr::write_volatile(addr_of_mut!(JBUSY_STAGE), i); }
/// The wait for a foreign transaction broken down by THE HOLDER's phase (`n` is the episodes,
/// `tsc` is the cycles, `max` is the worst episode by this phase).
pub static JWAIT_ST: [Phase; JSTAGES] = [const { phase() }; JSTAGES];
/// The threshold of a diagnosis into the console (ms). It was hard-wired to one second — that is, the stops of
/// hundreds of milliseconds, of which the tail is made, were NEVER printed.
/// The knob `talaria.jrnwait=` (0 — stay silent entirely).
pub static JRNWAIT_MS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(50);
pub fn jrnwait_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::num(cmdline, "talaria.jrnwait") {
        JRNWAIT_MS.store(v as usize, core::sync::atomic::Ordering::SeqCst);
        crate::println!("ext2: the diagnosis \"the journal is busy with a neighbour\" from the threshold of {} ms{}", v,
                        if v == 0 { " (it is off)" } else { "" });
    }
}
/// (the episodes, the cycles, the worst one) by the holder's phase — for `stat:` and for the gate.
pub fn jwait_stage(i: usize) -> (u64, u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (JWAIT_ST[i].n.load(Relaxed), JWAIT_ST[i].tsc.load(Relaxed), JWAIT_ST[i].max.load(Relaxed))
}
/// How many times a waiter for the journal (an fsync/a sync under a foreign transaction) waited longer than
/// the diagnosis's threshold (`talaria.jrnwait=`, the default 50 ms).
pub static JRN_LONGWAIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// SAFETY: E1 + E6 — under the FS lock: claiming the journal is a test-and-set that must
/// not be split, and the lock is what makes it one.
unsafe fn jrn_busy_set() { JBUSY_SINCE = crate::tsc::rdtsc(); JBUSY_TASK = crate::sched::current_id() as i64; jstage(0); JBUSY = true; }
/// Release the journal: the holder's phase is removed TOGETHER with the flag, otherwise a waiter
/// would attribute its wait to a phase that nobody occupies any more.
/// SAFETY: E1 + E6 — under the FS lock, by the task that claimed the journal.
unsafe fn jrn_busy_clear() { jstage(0); JBUSY = false; }
static mut JIO: u64 = 0;                   // §2.3: the submissions to the device for writing the transactions

/// SAFETY: E7 — read volatile without the lock; it is a flag, and a stale answer only
/// makes the caller more careful.
pub fn jrn_active() -> bool { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(JRN)) } }
/// The journal's counters: (the transactions, the blocks of metadata, is there an unclosed checkpoint).
/// SAFETY: the statistics exception of `cache_stats`.
pub fn jrn_stats() -> (u64, u64, bool) { unsafe { (JCOMMITS, JBLOCKS, JDIRTY) } }
/// THE BREAKDOWN OF fsync BY PHASE (an instrument for the write's tail): how many times, how many
/// cycles and the worst case — per every phase, so that the 4 ms of the tail are visible
/// as "whose" price rather than deduced from the averages over all the writes. The phases:
/// 0 the whole fsync (from the entry to the exit, with the wait for the FS lock), 1 the file's data
/// in place, 2 the journal's superblock, 3 the descriptor+the copies (one submission),
/// 4 the commit block (a durable write), 5 the checkpoint.
///
/// SESSION 37: the phases 1-6 covered 3939 us out of the call's 5173 us, and 1.2 ms belonged
/// to NONE of them — that is, "fixing fsync" meant guessing. What was left between them
/// has been marked up exactly: 7 the wait for the FS lock at the entry (a neighbour is
/// inside the FS), 8 the wait for A FOREIGN transaction (§8.11 — the journal is busy and we stand),
/// 9 the barrier before the commit (it is also the transaction's only barrier: previously
/// only the commit itself was measured, while the wait for the hardware before it was not), 10 THE RETURN
/// of the FS lock after it was released for the time of the disk (the queue of the waiters manages to grow),
/// 11 the take of the device's lock. The remainder is printed as a separate line: until it
/// converges to zero, the breakdown is incomplete.
///
/// SESSION 38: the remainder converged (171 us per call, 7 %), but TWO phases stayed
/// not understood — "the data" (402 us, although §8.13 defers the last portion and
/// there must be no round trip to the device here) and "THE OTHER" itself. Both are broken down
/// BY DETAILS — by the phases 13 and further:
///   13-17 the flush of the pages (`writeback_unlocked` + the second pass under the lock):
///         the search and the copy into the substitute buffer, the submission of a portion to the device,
///         the handover of the device to a waiter, the return of the FS lock, the second pass
///         `writeback_locked` over the WHOLE cache;
///   18-21 what lies between the phases and therefore fell into "THE OTHER":
///         `mmap_harvest`, `prea_release`, the estimation of the transaction's capacity
///         (`while cap > 0 && jrn_need(cap) > ...`) and the passes of `jrn_pending`
///         over the whole cache (there are several of them per call, the cache is up to 131072 slots).
///
/// THE DETAILS DO NOT ENTER THE REMAINDER BY TASK (see PH_SUB): they lie INSIDE
/// the already covered phases, and adding them to the parent would mean subtracting the same
/// time twice — the remainder would lie with a zero.
pub struct Phase { pub n: core::sync::atomic::AtomicU64, pub tsc: core::sync::atomic::AtomicU64, pub max: core::sync::atomic::AtomicU64 }
const fn phase() -> Phase { Phase { n: core::sync::atomic::AtomicU64::new(0), tsc: core::sync::atomic::AtomicU64::new(0),
    max: core::sync::atomic::AtomicU64::new(0) } }
pub static FSYNC_PH: [Phase; 22] = [const { phase() }; 22];
pub const FSYNC_PH_NAMES: [&str; 22] = ["fsync", "the data", "the journal's superblock", "the descriptor+the copies", "the commit", "the checkpoint", "the \
    tail of the deferred data",
    "the FS lock at the entry", "a foreign transaction", "the barrier before the commit", "the return of the FS lock", "the device's lock", "THE OTHER (not \
        covered by the phases)",
    // THE DETAILS (inside the phases above, they do not enter the remainder) — the flush of the pages:
    "  the flush: the search and the copy into the buffer", "  the flush: the submission of a portion", "  the flush: the handover of the device", "  the \
        flush: the return of the FS lock", "  the flush: the second pass under the lock",
    // ...and what lay between the phases and was added into THE OTHER:
    "  the other: mmap_harvest", "  the other: prea_release", "  the other: the estimation of the journal's capacity", "  the other: the counting of the dirty \
        ones (jrn_pending)"];
/// The first DETAIL phase: the indices from it on lie INSIDE the phases 1-11 and do not
/// go into the remainder by task (otherwise the same time would be subtracted twice).
pub const PH_SUB: usize = 13;
/// THE REMAINDER IS COMPUTED BY TASK rather than by a subtraction of the sums. The phases 1-11 occur
/// outside an fsync too (a journal transaction is called by a slot's eviction, a `sync`, a closing of
/// a file), so "the phase 0 minus the sum of the rest" by the printing is not a remainder but
/// a difference of two DIFFERENT sets of calls. Here every task, having entered an fsync,
/// accumulates the time of ITS OWN phases, and at the exit the uncovered part goes into the phase 12: it honestly
/// answers the question "where did the 1.2 ms go" rather than hinting at it.
static mut PH_IN: [bool; crate::sched::NTASKS] = [false; crate::sched::NTASKS];
static mut PH_ACC: [u64; crate::sched::NTASKS] = [0; crate::sched::NTASKS];
#[inline] fn ph_note(i: usize, t0: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    let d = crate::tsc::rdtsc().wrapping_sub(t0);
    FSYNC_PH[i].n.fetch_add(1, Relaxed); FSYNC_PH[i].tsc.fetch_add(d, Relaxed); FSYNC_PH[i].max.fetch_max(d, Relaxed);
    if i > 0 && i < PH_SUB {
        let t = crate::sched::current_id();
        // SAFETY: the phase instrument's own row, bounds checked on the same line.
    unsafe { if t < crate::sched::NTASKS && PH_IN[t] { PH_ACC[t] += d; } }
    }
}
/// (the transactions, the checkpoints, the barriers: the data/the transaction/the checkpoint/the journal's superblock/the recover flag/the others).
/// SAFETY: the statistics exception of `cache_stats`.
pub fn ckpt_old() -> u64 { unsafe { JCKPT_OLD } }
pub fn barrier_stats() -> (u64, u64, [u64; 6]) {
    use core::sync::atomic::Ordering::Relaxed;
    let mut b = [0u64; 6];
    for i in 0..6 { b[i] = BAR_SRC[i].load(Relaxed); }
    // SAFETY: the statistics exception of `cache_stats`.
    unsafe { (JCOMMITS, JCKPTS, b) }
}
/// §2.3: how many SUBMISSIONS for writing the journal made (the descriptor+the copies is one).
/// SAFETY: the statistics exception of `cache_stats`.
pub fn jrn_io() -> u64 { unsafe { JIO } }

#[inline] fn be32(v: u32) -> u32 { v.to_be() }
#[inline] fn rdbe32(b: &[u8], o: usize) -> u32 { u32::from_be_bytes([b[o], b[o+1], b[o+2], b[o+3]]) }
#[inline] fn wrbe32(b: &mut [u8], o: usize, v: u32) { b[o..o+4].copy_from_slice(&v.to_be_bytes()); }

/// Read/write a journal block (the log index i) through the staging page.
/// SAFETY: E1 — under the FS lock; `i` is a log index the caller holds, `JBLK[i]` its block.
unsafe fn jread(i: usize, buf: &mut [u8]) -> bool { crate::fs::blk_read((JBLK[i] as usize) * SPB, &mut buf[..BS]) }
/// SAFETY: E1 — under the FS lock; as `jread`.
unsafe fn jwrite(i: usize, buf: &[u8]) -> bool { crate::fs::blk_write((JBLK[i] as usize) * SPB, &buf[..BS]) }
/// The journal's staging pages: `k == 0` is the descriptor, `k == 1` the commit.
///
/// SAFETY: E1 — under the FS lock, and TWO BOUNDS that nothing checks for us. `JSTAGE` is
/// `alloc_contig(2)`, that is exactly two pages of `BSZ`, so `k` must be 0 or 1; every call
/// site in this file passes a literal 0 or 1. And the `'static mut` means two live borrows
/// are sound only for DIFFERENT `k` — which is how they are taken (`jrn_replay` holds
/// `stage(0)` and `stage(1)` at once, and they are disjoint pages). Asking twice for the
/// same `k` would be two `&mut` to the same bytes.
unsafe fn stage(k: usize) -> &'static mut [u8] { core::slice::from_raw_parts_mut(crate::mem::kptr::<u8>(JSTAGE + (k * BSZ) as u64), BSZ) }

/// Find the journal (the inode s_journal_inum), parse its superblock, with a non-empty
/// journal REPLAY it, then mark it clean. It is called from mount().
/// SAFETY: E1 — under the FS lock (it is called from `mount`), and it is the one place allowed to
/// REPLAY: no other task can be inside the FS yet.
unsafe fn jrn_mount() {
    JRN = false;
    let inum = rd32(&*core::ptr::addr_of!(SB), 224);      // s_journal_inum
    if inum == 0 || rd32(&*core::ptr::addr_of!(SB), 92) & 0x4 == 0 { // has_journal (COMPAT)
        crate::println!("ext2: there is no journal (the volume was created without -O has_journal) — the mode without transactions");
        return;
    }
    if JSTAGE == 0 {
        match crate::mem::alloc_any(2) { Some(b) => JSTAGE = b, None => { crate::println!("ext2: there is no memory for the journal"); return; } }
    }
    if JCOPY == 0 {
        match crate::mem::alloc_any((JREQS - 1) as u64) { Some(b) => JCOPY = b, None => { crate::println!("ext2: there is no memory for the journal's \
            snapshots"); return; } }
    }
    // the map of the journal's blocks (the journal itself is an ordinary file of the FS)
    let mut inb = [0u8; 128];
    ino_read(inum, &mut inb);
    let jsize = i_size(&inb) as usize / BS;
    if jsize < 4 { crate::println!("ext2: the journal is too small ({} blocks)", jsize); return; }
    let n = jsize.min(JMAX);
    for fb in 0..n {
        let mut add = 0;
        let b = bmap(inum, &mut inb, fb as u64, false, &mut add);
        if b == 0 { crate::println!("ext2: there is a hole in the journal's file (the block {}) — we do not use the journal", fb); return; }
        JBLK[fb] = b;
    }
    // the journal's superblock (the log block 0), the fields are big-endian
    let sbuf = stage(0);
    if !jread(0, sbuf) { return; }
    if rdbe32(sbuf, 0) != JMAGIC { crate::println!("ext2: the journal's superblock has no magic — we do not use the journal"); return; }
    let bsz = rdbe32(sbuf, 12) as usize;
    let maxlen = rdbe32(sbuf, 16) as usize;
    let first = rdbe32(sbuf, 20) as usize;
    let seq = rdbe32(sbuf, 24);
    let start = rdbe32(sbuf, 28) as usize;
    let incompat = rdbe32(sbuf, 40);
    if bsz != BS { let bs = BS; crate::println!("ext2: the journal's block {} != {} — we do not use the journal", bsz, bs); return; }
    // Not supported: the 64BIT tags (0x2), the checksums v2/v3 (0x4/0x10),
    // an async commit (0x8), a fast commit (0x20) — with them the tags' format is different.
    if incompat & !0x1 != 0 { crate::println!("ext2: a journal with the features {:#x} is not supported — the mode without transactions", incompat); return; }
    JLEN = maxlen.min(n); JFIRST = first.max(1); JSEQ = seq.max(1); JHEAD = JFIRST; JDIRTY = false;
    JRN = true;
    let (jlen, jseq, jdata) = (JLEN, JSEQ, JDATA);
    crate::println!("ext2: a JBD2 journal — the inode {}, {} blocks, seq {}, start {}, the mode {}",
        inum, jlen, jseq, start, if jdata { "journal=data (the data go in the transaction)" } else { "ordered (the data go in place)" });
    if start != 0 { jrn_replay(start); }
    jrn_write_sb(0, JSEQ, true);
    JSB_STALE = true; JRECOVER = false;
}

/// The journal's blocks and a write in place, as `jbd2::recover` asks for them.
struct JMedia;
impl crate::jbd2::Media for JMedia {
    /// SAFETY: E1 — the replay runs on the mount path, under the FS lock, with nobody else
    /// inside the file system.
    fn log_read(&mut self, i: usize, out: &mut [u8]) -> bool { unsafe { jread(i, out) } }
    /// The block 0 (the FS's superblock is in it) is a lawful target — it is not skipped.
    /// SAFETY: E1 + E6 — as `log_read`: the replay puts committed blocks back in place
    /// before anyone can read them.
    fn fs_write(&mut self, block: u32, data: &[u8]) { unsafe { crate::fs::blk_write((block as usize) * SPB, data); } }
}

/// Replay the journal from the log block `start`. The algorithm and the two
/// passes are `jbd2::recover` (it is tested on the host over a journal in
/// memory: a torn tail, a wrap around the end, a stale sequence); here is what
/// only the machine has — the staging pages and the stale block cache.
/// SAFETY: E1 + E6 — under the FS lock, on the mount path only. It writes committed blocks back
/// to their places; the cache is empty at this point, so E2 has nothing to protect yet.
unsafe fn jrn_replay(start: usize) {
    let seq0 = JSEQ;
    let g = crate::jbd2::Geom { bs: BS, first: JFIRST, len: JLEN };
    let r = {
        let hdr = stage(0);
        let cpy = stage(1);
        crate::jbd2::recover(&mut JMedia, &g, start, seq0, hdr, cpy)
    };
    if r.txns == 0 { crate::println!("ext2: the journal is non-empty, but there are no intact transactions — there is nothing to replay"); return; }
    let applied = r.blocks;
    crate::fs::blk_flush();
    JSEQ = r.next_seq;
    // The block cache IS STALE: the mounting managed to read the superblock, the GDT and the inode
    // table (for the journal's inode) BEFORE the replay, while the replay wrote straight to the disk.
    // Without a flush of the cache the pre-crash version of the metadata is visible further on.
    for i in 0..ncache() { CTAG[i] = 0; CDIRTY[i] = false; CNEW[i] = false; COWN[i] = 0; CJRN[i] = false; CJOLD[i] = false; }
    dcnt_reset();
    let mut sb2 = [0u8; 1024];
    if crate::fs::blk_read(2, &mut sb2) { (*core::ptr::addr_of_mut!(SB)).copy_from_slice(&sb2); }
    let jseq = JSEQ;
    crate::println!("ext2: THE JOURNAL HAS BEEN REPLAYED — the transactions {}, the blocks {}, the next seq {}",
        r.txns, applied, jseq);
}

/// SAFETY: E1 — under the FS lock; it wraps the log index inside `[JFIRST, JLEN)`.
#[inline]
unsafe fn jnext(i: usize) -> usize { let n = i + 1; if n >= JLEN { JFIRST } else { n } }

/// The flag "the journal is not closed" (EXT3_FEATURE_INCOMPAT_RECOVER) in the FS's superblock.
/// It is written STRAIGHT to the disk (bypassing the cache) with a barrier: by it Linux understands that
/// the journal must be replayed rather than ask the operator. It is set once per
/// epoch (at the first transaction after a checkpoint), it is removed at a checkpoint.
/// SAFETY: E1 + E6 — under the FS lock; it sets the FS superblock's recover flag, which is what
/// tells a foreign `e2fsck`/Linux that the journal must be replayed.
unsafe fn set_recover(on: bool) {
    let sbm = &mut *addr_of_mut!(SB);
    let f = rd32(sbm, 96);
    let nf = if on { f | 0x4 } else { f & !0x4 };
    if nf == f { return; }
    wr32(sbm, 96, nf);
    let mut raw = [0u8; 1024];
    if !crate::fs::blk_read(2, &mut raw) { return; }
    raw[96..100].copy_from_slice(&nf.to_le_bytes());
    crate::fs::blk_write(2, &raw);
    dev_barrier_src(BAR_RECOV);          // the recover flag must land BEFORE the transaction — but not under the FS lock
    // Keep the copy in the cache consistent, BUT DO NOT DIRTY it: sb_flush() marked
    // the superblock's block dirty, and the journal got a perpetual motion machine — every
    // checkpoint removed the recovery flag, thereby creating a dirty block for
    // the next transaction (on sigtest that gave 44 transactions instead of 2).
    let (b, off) = if BS == 1024 { (1u32, 0usize) } else { (0u32, 1024usize) };
    if let Some(slot) = find_slot(b) {
        let p = crate::mem::kptr::<u8>(CACHE_BASE).add(slot * BSZ + off);
        core::ptr::copy_nonoverlapping(sbm.as_ptr(), p, 1024);
    }
}

/// Write the journal's superblock (s_start / s_sequence); a barrier goes on request.
///
/// §8.8: the superblock needs ITS OWN barrier only when the journal is declared EMPTY
/// (a sync/a shutdown: after it nothing is written into the journal, and there is nothing to
/// cover the write with). Inside an epoch the superblock is written BEFORE a transaction's descriptor and
/// becomes durable by its own first barrier — together with the blocks' copies.
/// Previously it cost a barrier per checkpoint and another one per the first transaction after
/// it: on a mongodb insert (gp3) that was 2 out of the 4-5 barriers per
/// fsync with a barrier's price of 15-20 ms.
/// SAFETY: E1 + E6 — under the FS lock; it writes the journal's superblock, which describes the
/// epoch's start. The ordering it takes part in is spelt out in `jrn_txn`.
unsafe fn jrn_write_sb(start: usize, seq: u32, barrier: bool) {
    let sb = stage(0);
    if !jread(0, sb) { return; }
    wrbe32(sb, 24, seq);
    wrbe32(sb, 28, start as u32);
    let _ = JBT_SB_V2;
    jwrite(0, sb);
    JSB_STALE = false;
    JSB_FLUSH.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if barrier { dev_barrier_src(BAR_JSB); }
}
static JSB_FLUSH: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// §2.5/§3.2: what we take into a transaction — only the metadata (a slot's owner is 0),
/// only the files' data, or both.
#[derive(Clone, Copy, PartialEq)]
enum JSel { Meta, Data, Both }
/// SAFETY: E1 — under the FS lock; `s` is a cache slot index of the walk.
#[inline]
unsafe fn jrn_take(s: usize, sel: JSel) -> bool {
    if CTAG[s] == 0 || !CDIRTY[s] || CJRN[s] { return false; }
    match sel { JSel::Meta => COWN[s] == 0, JSel::Data => COWN[s] != 0, JSel::Both => true }
}
/// How many blocks are still waiting to be journalled by this selection.
///
/// SESSION 39: there was a walk over the WHOLE cache (131072 slots with fscache=512), and per
/// one `fsync` it was called twice and more expensively — 165 us per call, 5 % of its price,
/// of pure processor. Now it is a read of the counters (`JP_META`/`JP_DATA`,
/// see `dcnt_add`): the predicate `jrn_take` is exactly their definition, and the gate `dcnt`
/// verifies the counters against a full recomputation. The phase 21 is left on purpose: the 10 %
/// of the call's price must disappear FROM THE INSTRUMENT on the stand rather than from a reasoning.
/// SAFETY: E1 + E3 — under the FS lock; since session 39 it answers from the counters
/// (`JP_META`/`JP_DATA`) rather than by walking the cache, which is why E3's single point of
/// change matters: a missed `dcnt` pair here would mean journalling the wrong set.
unsafe fn jrn_pending(sel: JSel) -> usize {
    let t0 = crate::tsc::rdtsc();
    let n = if fscnt() {
        match sel { JSel::Meta => JP_META, JSel::Data => JP_DATA, JSel::Both => JP_META + JP_DATA }
    } else {
        let mut n = 0; for s in 0..ncache() { if jrn_take(s, sel) { n += 1; } } n   // talaria.fscnt=0
    };
    ph_note(21, t0);
    n
}
/// A transaction: the inode `ino`'s data in place (0 — only the metadata, u32::MAX —
/// the data of all the files), then the metadata through the journal with barriers.
///
/// §3.2: a transaction LARGER THAN THE JOURNAL no longer falls the mode journal=data back to
/// ordered — it is cut into parts. The data travel in portions by the journal's capacity
/// (between the portions a checkpoint frees room), while the metadata go by the LAST
/// transaction: after a crash halfway the metadata stay old and do not
/// point at the data blocks that have not been recovered yet.
pub fn jrn_commit(ino: u32) -> i64 {
    let _fsg = fs_lock();
    // SAFETY: E1 + E4 + E6 — the lock is taken on the line above; the ordering of the data
    // phase, the claim of the journal and the transaction's body is spelt out at `jrn_txn`.
    unsafe {
        // §2.4: what is reserved but not used goes back into the map while
        // the map has not gone to the disk yet (otherwise e2fsck would see ownerless blocks).
        let tp = crate::tsc::rdtsc();
        prea_release(0);
        ph_note(19, tp);
        // STEP 2 OF SESSION 40: THE DATA IN PLACE GO BEFORE THE JOURNAL AND WITHOUT THE JOURNAL.
        //
        // 99 % of the wait for a foreign transaction was a wait for THIS phase (session 39's
        // instrument), while it does not concern the journal — the analysis of what `JBUSY`
        // protected here is written at `JRNFREE`. In journal=data the data go
        // THROUGH the journal, they have no separate phase, and there is nothing to pre-empt.
        let mut data_n = 0usize;
        let early_data = jrnfree() && JRN && !JDATA && ino != 0;
        if early_data { data_n = jrn_data_phase(ino, false); }
        // §8.11: the journal is busy with a neighbour (its transaction flies with the FS lock
        // released) — WAIT rather than bypass the journal. Previously a second fsync in that
        // window wrote ALL the dirty metadata in place, including the uncommitted ones
        // (a hole in the atomicity, and after §8.6 the window became wide — every barrier).
        if JRN && JBUSY { let t0 = crate::tsc::rdtsc(); jrn_wait_free(); ph_note(8, t0); }
        if !JRN || JBUSY {
            // We get here ONLY when the FS lock must not be released (a nested
            // call inside a transaction; `jrn_wait_free` does not give up by time) —
            // only THE DATA travel in place; the metadata will be taken by the flying transaction
            // or by the next fsync. The counter is visible in `stat:` as the journal's bypasses.
            JRN_BYPASS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let cause = if ino == u32::MAX || ino == 0 { WbC::Other } else { WbC::Fsync };
            let n = if JRN { if ino == 0 || early_data { data_n } else { writeback(if ino == u32::MAX { Sel::DataAll } else { Sel::Data(ino) }, cause) } }
                    else { writeback(if ino == 0 { Sel::All } else { Sel::Own(ino) }, cause) };
            // There will be no transaction — we wait for the deferred portion (if it is OURS) ourselves:
            // otherwise its ways would stay "in flight" until the next fsync.
            if wb_defer_mine() {
                let depth = fs_unlock_outermost();
                wb_defer_finish();
                fs_relock(depth);
            }
            if n > 0 { dev_barrier(); }
            return 0;
        }
        jrn_busy_set();
        // The capacity of ONE transaction: the log blocks minus the commit, the reserve and the descriptors.
        // (`jrn_need(cap) <= JLEN - JFIRST` — otherwise the transaction will move
        // across the end of the journal onto its own start)
        let room = if JLEN > JFIRST + 3 { JLEN - JFIRST - 3 } else { 0 };
        let tcap = crate::tsc::rdtsc();
        let mut cap = room - room / (jrn_per_desc() + 1);
        while cap > 0 && jrn_need(cap) > JLEN - JFIRST { cap -= 1; }
        ph_note(20, tcap);
        let with_data = JDATA && cap > 0;
        // 1) the file's data go in place. A barrier (ordered: the data go BEFORE the commit)
        // is NOT put separately: the transaction's first barrier stands before the commit
        // block and covers the data together with the copies — as with JBD2. The data need their own barrier
        // only if there will be no transaction (the metadata are clean).
        // §8.13: the last portion of the data is submitted WITHOUT a wait — it will be waited for by
        // the descriptor's batch (or by `wb_defer_finish` below, if there will be no
        // transaction). The phase "the data" from this moment measures the submission, the wait
        // sits in "the descriptor+the copies"; the deferred portions are counted separately.
        if !with_data && ino != 0 && !early_data { data_n = jrn_data_phase(ino, true); }
        // ORDERED (see `CNEW`): the metadata about to be journaled may hold the allocations of
        // OTHER files; their freshly allocated blocks go to the disk first, synchronously.
        if !with_data {
            let t0 = crate::tsc::rdtsc();
            let n = writeback(Sel::DataNew, WbC::Fsync);
            data_n += n;
            ORDERED_NEW.fetch_add(n as u64, core::sync::atomic::Ordering::Relaxed);
            ph_note(1, t0);
        }
        let mut journaled = 0usize;
        if with_data && jrn_pending(JSel::Both) <= cap {
            journaled += jrn_txn(JSel::Both, cap); // everything fits — one transaction, as before
        } else {
            // In portions — but exactly as much as was dirty AT THE ENTRY. The FS
            // lock is released at the barriers, and while a transaction flies, the neighbours
            // dirty new metadata; "pending > 0 after a transaction" no longer
            // means "it did not fit" — and by that sign a checkpoint made
            // an extra checkpoint with a barrier out of every second fsync
            // (a mongodb insert: 134 checkpoints per 251 transactions). The lack of
            // room is fixed by jrn_txn itself, while somebody else's dirt is somebody else's fsync's business.
            if with_data {
                let need = jrn_pending(JSel::Data); let mut got = 0usize;
                while got < need { let w = jrn_txn(JSel::Data, cap); if w == 0 { break; } got += w; }
                journaled += got;
            }
            let need = jrn_pending(JSel::Meta); let mut got = 0usize;
            while got < need { let w = jrn_txn(JSel::Meta, cap); if w == 0 { break; } got += w; }
            journaled += got;
        }
        if wb_defer_mine() {
            let t0 = crate::tsc::rdtsc();
            let depth = fs_unlock_outermost();
            jstage(7);
            wb_defer_finish();
            jstage(0);
            ph_note(6, t0);
            let tr = crate::tsc::rdtsc(); fs_relock(depth); ph_note(10, tr);
        }
        if data_n > 0 && journaled == 0 { dev_barrier_src(BAR_DATA); }
        jrn_busy_clear();
        let _ = be32(0);
        0
    }
}

pub static JRN_BYPASS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Freshly allocated data blocks of other files written before a metadata commit (`CNEW`).
pub static ORDERED_NEW: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
// ══ STEP 2 OF SESSION 40: THE JOURNAL IS NOT BUSY FOR THE TIME OF A FOREIGN DATA FLUSH ═══
//
// WHAT WAS MEASURED. Session 39's instrument broke the wait for a foreign transaction down by THE HOLDER's
// phase: 99 % is the phase "the data in place" (214-216 us per call in ONE
// episode per window, the worst episode 257-385 ms). Not the commit, not the descriptor, not
// the barrier. A neighbour stood for hundreds of milliseconds behind an operation that does not concern
// the journal: `jrn_commit` raised `JBUSY` BEFORE the data phase, while `jrn_wait_free`
// made one wait for exactly `JBUSY`.
//
// WHAT EXACTLY `JBUSY` PROTECTED IN THE DATA PHASE (written out before the change — plan 40):
//   1. THE JOURNAL'S STATICS (JHEAD/JSEQ/JDIRTY/JCOPY/JSTAGE, CJPOS/CJRN/CJOLD)
//      for the time of the released FS lock. The data phase DOES NOT TOUCH them: it writes
//      the slots with `COWN != 0` (the data), and their `CJRN`/`CJOLD` are already removed — in
//      the ordered mode the data do not go through the journal. The protection is superfluous;
//   2. THE UNIQUENESS OF THE DEFERRING ONE (§8.13): DEFER_BUF/DEFER_SLOTS/DEFER_K go
//      one per device, and the owner was recognised by "since the journal is mine, the portion
//      is mine too". It has been replaced with AN EXPLICIT claim (`wb_defer_claim`), which lives
//      until `wb_defer_finish` — without it two data phases at once would overwrite each other's
//      list of ways;
//   3. THE PROTECTION AGAINST RECURSION (§8.4/§8.8): the eviction of dirty METADATA inside
//      a transaction must not call `jrn_commit` again. There is no weakening but
//      a TIGHTENING: `JBUSY` now stands for a shorter time, which means an eviction more often
//      falls into the branch "first a commit" rather than into the branch "the journal is busy — we write
//      the uncommitted one in place";
//   4. THE ORDER "the data before the commit" FOR ONE'S OWN transaction. It is kept not by
//      `JBUSY` but by the barrier before the commit block (and by §8.13: the descriptor's batch
//      waits for the deferred data). Our data are submitted BEFORE we took
//      the journal — all the more before our barrier.
//
// WHAT THE CHANGE CHANGES HONESTLY: while our data fly, A NEIGHBOUR may commit
// its own transaction, and ALL the cache's dirty metadata get into it, including ours.
// After a crash exactly in that window a file's metadata may turn out to be newer than its
// data. That class is NOT A NEW one: `jrn_txn(JSel::Meta)` took the dirty
// metadata of all the inodes at once before too, so somebody else's `fsync` committed
// the metadata of a file whose data lie dirty in the cache before the change too (the ordered semantics
// here is per file — "the data OF THIS fsync before ITS commit" rather than a common one, as
// with jbd2). The change widens the window from "the data are dirty" to "the data are in flight";
// `fsync`'s promise is not affected by anything: the call has not returned yet.
//
// `talaria.jrnfree=0` — the former behaviour (the data under an occupied journal) for
// an A/B ON ONE RUN: session 39's rule — a performance change does not start without
// such a knob.
pub static JRNFREE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
#[inline] fn jrnfree() -> bool { JRNFREE.load(core::sync::atomic::Ordering::Relaxed) }
pub fn jrnfree_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::num(cmdline, "talaria.jrnfree") {
        JRNFREE.store(v != 0, core::sync::atomic::Ordering::SeqCst);
        crate::println!("ext2: the journal for the time of the data flush is {} (talaria.jrnfree={})",
            if v != 0 { "FREE" } else { "BUSY — as before session 40" }, v);
    }
}
/// For the gate `jrnfreedata`: delay ONE next data phase by N ms with
/// the FS lock RELEASED (in the same way as the device delays it). It is single-use:
/// it is consumed by whoever enters the phase first.
pub static JRN_STALL_MS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static JRN_STALL_HITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The phase "THE DATA IN PLACE" of a per-file fsync + the instrument of step 2 of session 39.
///
/// `holder` — are we going under an OCCUPIED journal (`talaria.jrnfree=0` or
/// journal=data): the holder's phase is marked only then, because the waiters
/// sample it exclusively under `JBUSY`.
/// SAFETY: E1 + E4 + E6 — under the FS lock. This is the ORDERED mode's first half: the file's
/// data reach the disk BEFORE the metadata that point at them are committed. It may release
/// the lock around the device (the flush goes in portions), which is why it runs before the
/// transaction's body rather than inside it.
unsafe fn jrn_data_phase(ino: u32, holder: bool) -> usize {
    let sel = if ino == u32::MAX { Sel::DataAll } else { Sel::Data(ino) };
    let t0 = crate::tsc::rdtsc();
    let claimed = wb_defer_claim();
    if holder { jstage(1); }
    let data_n = writeback(sel, if ino == u32::MAX { WbC::Other } else { WbC::Fsync });
    // THE GATE: a delay "like the device's" — with the FS lock released.
    let stall = JRN_STALL_MS.swap(0, core::sync::atomic::Ordering::SeqCst);
    if stall != 0 && fs_release_allowed() {
        JRN_STALL_HITS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        let depth = fs_unlock_outermost();
        let hz = crate::tsc::hz().max(1);
        let t = crate::tsc::rdtsc();
        while crate::tsc::rdtsc().wrapping_sub(t) < hz / 1000 * stall { crate::paging::tlb_service(); core::hint::spin_loop(); }
        fs_relock(depth);
    }
    if holder { jstage(0); }
    if claimed { wb_defer_unclaim(); }      // deferring did not work out — remove the claim
    ph_note(1, t0);
    // STEP 2'S INSTRUMENT: we count only a per-file fsync (the ino is set) — with
    // a `sync`/a closing of a file "the file's blocks" do not exist.
    if ino != u32::MAX {
        use core::sync::atomic::Ordering::Relaxed;
        let t = crate::sched::current_id().min(crate::sched::NTASKS - 1);
        FSD_CALLS.fetch_add(1, Relaxed);
        FSD_DIRTY.fetch_add(data_n as u64, Relaxed);
        FSD_FILE.fetch_add(FSD_FBLK[t] as u64, Relaxed);
    }
    data_n
}
/// Wait until a foreign transaction releases the journal: we give the FS lock up for that time
/// (if it is allowed) — the neighbour waits for it at the barriers.
///
/// THERE IS NO GIVING UP BY TIME. Previously the wait was bounded by ~1 s, and then
/// `jrn_commit` went bypassing the journal: the data in place, the metadata "will be taken by
/// the next fsync" — and it returned 0. That is, an fsync under a foreign transaction
/// longer than a second (on gp3 a journal at a checkpoint really is that) answered
/// "it is written" without having written the metadata: the same lie as at the device and
/// FS locks. Now we wait as long as is needed, while after a second we name the holder.
/// The only exit without a result is when the FS lock must not be released
/// (a nested call inside one's own transaction; such calls are fenced off
/// by `!JBUSY` at the callers anyway).
/// SAFETY: E1 + E4 — entered under the FS lock and it WAITS for a neighbour's transaction, giving
/// the lock up while it does (otherwise the whole FS would stall for the neighbour's
/// milliseconds). It returns holding the lock again. A waiter reads the holder's phase word
/// volatile (E7) to attribute the wait.
unsafe fn jrn_wait_free() {
    let hz = crate::tsc::hz();
    let ms = if hz != 0 { (hz / 1000).max(1) } else { u64::MAX };
    let start = crate::tsc::rdtsc();
    let thr = JRNWAIT_MS.load(core::sync::atomic::Ordering::Relaxed) as u64;
    let mut next_report = if hz != 0 && thr != 0 { ms.saturating_mul(thr) } else { u64::MAX };
    let mut reports = 0u32;
    // The wait is broken down by THE HOLDER's phase: our own per episode, into the common
    // counters once at the exit (see JWAIT_ST).
    let mut acc = [0u64; JSTAGES];
    let mut last_t = start;
    let mut last_st = core::ptr::read_volatile(core::ptr::addr_of!(JBUSY_STAGE)).min(JSTAGES - 1);
    let busy = || core::ptr::read_volatile(core::ptr::addr_of!(JBUSY));
    while busy() {
        let depth = fs_unlock_outermost();
        if depth == 0 { jwait_flush(&acc); return; }
        let mut spins = 0u32;
        while busy() {
            spins += 1;
            if spins > 64 {
                spins = 0;
                let cpu = crate::percpu::cpu_id();
                if crate::sched::cpu_has_other_runnable(cpu, crate::sched::current_id()) { crate::sched::yield_origin(crate::sched::YO_FS);
                    crate::sched::yield_now(); }
                else { crate::fs::io_window(); }   // the FS lock is released above — the window is lawful
                let now = crate::tsc::rdtsc();
                let st = core::ptr::read_volatile(core::ptr::addr_of!(JBUSY_STAGE)).min(JSTAGES - 1);
                acc[last_st] += now.wrapping_sub(last_t);
                last_t = now; last_st = st;
                let waited = now.wrapping_sub(start);
                if waited > next_report {
                    if reports == 0 { JRN_LONGWAIT.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
                    let since = core::ptr::read_volatile(core::ptr::addr_of!(JBUSY_SINCE));
                    // THE HOLDER IS NAMED BY THE TASK AND THE PHASE: "the journal was busy"
                    // does not become a change, while "the task 7 on the commit for 214 ms"
                    // does.
                    crate::println!("ext2: the journal is busy with a foreign transaction for {} ms (the task {} at the phase \"{}\", its transaction has been \
                        going {} ms), the task {} keeps waiting — we do not write the metadata in place (give-ups 0)",
                        waited / ms, core::ptr::read_volatile(core::ptr::addr_of!(JBUSY_TASK)),
                        JSTAGE_NAMES[st], crate::tsc::rdtsc().wrapping_sub(since) / ms, crate::sched::current_id());
                    reports += 1;
                    next_report = waited.saturating_add(ms.saturating_mul(1000));
                }
            }
            crate::paging::tlb_service();
            core::hint::spin_loop();
        }
        fs_relock(depth);
    }
    acc[last_st] += crate::tsc::rdtsc().wrapping_sub(last_t);
    jwait_flush(&acc);
}
/// An episode of waiting goes into the common counters (one pass, without atomics in the spin).
fn jwait_flush(acc: &[u64; JSTAGES]) {
    use core::sync::atomic::Ordering::Relaxed;
    for i in 0..JSTAGES {
        if acc[i] == 0 { continue; }
        JWAIT_ST[i].n.fetch_add(1, Relaxed);
        JWAIT_ST[i].tsc.fetch_add(acc[i], Relaxed);
        JWAIT_ST[i].max.fetch_max(acc[i], Relaxed);
    }
}

/// For the gate `jrnwaitdiag`: occupy the journal with a "foreign transaction" without a write.
/// SAFETY: the gate's entry point — it is called from the regression with no neighbour
/// inside the FS, and it claims/releases the journal through the ordinary pair.
pub fn jrn_busy_for_test(v: bool) { unsafe { if v { jrn_busy_set(); jstage(8); } else { jrn_busy_clear(); } } }
/// For the gate `jrnwaitdiag`: substitute the holder's phase (a check that the
/// wait is broken down BY PHASE rather than falls into one).
/// SAFETY: E7 — the only externally driven `jstage`, and it clamps into `[0, JSTAGES)`.
pub fn jrn_stage_for_test(i: usize) { unsafe { jstage(i.min(JSTAGES - 1)); } }
/// For the gate `fsyncph`: is the journal busy right now (a waiter needs to know that
/// it really stood in the queue rather than slipped through before the capture).
/// SAFETY: E7 — read volatile without the lock; it is a flag, and a stale answer only
/// makes the caller more careful.
pub fn jrn_busy_now() -> bool { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(JBUSY)) } }
/// For the gate `fsyncph`: (the calls, the cycles) of a phase — a snapshot before and after.
pub fn ph_get(i: usize) -> (u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (FSYNC_PH[i].n.load(Relaxed), FSYNC_PH[i].tsc.load(Relaxed))
}
/// For the gate `jrnwaitdiag`: an independent fsync of everything (like a sync(2)) — it waits for
/// the journal rather than bypassing it. It returns (the code, the bypasses after the call).
pub fn jrn_sync_for_test() -> (i64, u64) {
    let _rel = MayRelease::set(true);
    let r = jrn_commit(u32::MAX);
    (r, JRN_BYPASS.load(core::sync::atomic::Ordering::SeqCst))
}

/// One transaction: not more than `cap` blocks by the selection `sel` — the descriptor(s) with
/// the tags, the blocks' copies, a barrier, the commit, a barrier. It returns the blocks written
/// (0 — there is nothing to journal).
/// SAFETY: E1 + E4 + E5 + E6 + E7 — under the FS lock, and it is the strictest section in the
/// module. What it rests on beyond E1:
/// * THE DEVICE'S LOCK IS HELD FOR THE WHOLE TRANSACTION. The FS lock IS released at the
///   barriers, so the order "the copies, then the commit" can no longer be guaranteed by
///   it; the device's lock is what stops a neighbour's metadata write in place from landing
///   between the copies and the commit (E6);
/// * the two staging pages are disjoint (`stage(0)` the descriptor, `stage(1)` the commit),
///   and `stage` is safe only under that rule — see it;
/// * the blocks' copies are taken out of their cache slots under the lock, and the slots
///   are not re-tagged while the transaction runs (E2/E5);
/// * the phase word is written volatile at every boundary (E7) so that a waiter can name
///   the holder's phase without taking anything.
unsafe fn jrn_txn(sel: JSel, cap: usize) -> usize {
    let pending = jrn_pending(sel);
    if pending == 0 || cap == 0 { return 0; }
    let want = pending.min(cap);
    // The recover flag in the FS's superblock goes ONCE per mounting epoch (it is removed
    // only by a sync/a shutdown, when the journal is declared empty): by it Linux
    // only understands that the journal must be replayed, while an empty journal
    // is replayed into nothing. Previously the flag was set and removed around every
    // checkpoint — two barriers that ext4 does not have.
    if !JRECOVER { set_recover(true); JRECOVER = true; }
    // there must be enough room for the descriptor + the blocks + the commit; otherwise a checkpoint first
    let need = jrn_need(want);
    if JHEAD + need > JLEN { let t0 = crate::tsc::rdtsc(); jstage(6); jrn_checkpoint_locked(false); jstage(0); ph_note(5, t0); }
    // The journal's superblock describes the epoch's start (JFIRST, JSEQ): it is written BEFORE
    // the descriptor, it is made durable by the first barrier of this same transaction.
    // A crash before it: the superblock on the disk is either the old one and points at the already
    // checkpointed transactions (a repeat is harmless), or the new one and points
    // at a descriptor without a commit (there is nothing to replay).
    if JSB_STALE { let t0 = crate::tsc::rdtsc(); jstage(2); jrn_write_sb(JFIRST, JSEQ, false); jstage(0); ph_note(2, t0); }
    let seq = JSEQ;
    let mut written = 0usize;
    // WE HOLD THE DEVICE FOR THE WHOLE TRANSACTION. That is stricter than it was: previously
    // the order "the copies, then the commit" was kept by the fact that the FS lock was not released
    // at all — while now we release it at the barriers, and the guarantee must be given by
    // the device's lock. While it is ours, nobody else writes to the disk, and a foreign
    // write of metadata IN PLACE cannot stand between the copies and the commit.
    // But there is no point in WAITING for it under the FS lock: it is busy — which means a neighbour is giving
    // the device a portion of data (§8.6), and that is milliseconds for which the whole FS
    // would stall. We release it (if it is allowed), wait it out from the outside, take it anew.
    let tdev = crate::tsc::rdtsc();
    let dg = dev_lock_nowait_fs();
    ph_note(11, tdev);
    {
        let mut slot = 0usize;
        // 3) the descriptor + the blocks' copies; the tags may occupy several descriptors
        while slot < ncache() && written < want {
            // assemble the next descriptor block
            let d = stage(0);
            for b in d[..BS].iter_mut() { *b = 0; }
            crate::jbd2::put_header(d, JBT_DESCRIPTOR, seq);
            let mut o = crate::jbd2::HDR;
            let mut first = true;
            let dpos = JHEAD;
            // §2.3: the descriptor and the blocks' copies go to the device in ONE batch.
            // Previously the descriptor was written by a separate blk_write, while the copies went by a second
            // blk_write_many: two submissions where one is needed. We reserve slot 0
            // for the descriptor and fill it after the last tag is laid out.
            let mut reqs: [(usize, crate::mem::Pa, u16); JREQS] = [(0, crate::mem::Pa::frame(0), 0); JREQS];
            let mut nr = 1usize;
            let mut pos = jnext(dpos);
            let mut tags = 0usize;
            let mut last_tag = 0usize;
            while slot < ncache() && written < want {
                if !jrn_take(slot, sel) { slot += 1; continue; }
                if !crate::jbd2::tag_room(o, BS, first) || nr == reqs.len() { break; } // the room for a tag and the reserve
                let bn = CTAG[slot] - 1;
                let addr = CACHE_BASE + (slot * BSZ) as u64;
                // the escaping: a block that starts with the journal's magic must not be written as it is
                let head = core::ptr::read_volatile(crate::mem::kptr::<u32>(addr));
                let esc = head.to_be() == JMAGIC;
                last_tag = o;
                o = crate::jbd2::put_tag(d, o, bn, first, esc);
                first = false;
                if esc {
                    let e = stage(1);
                    core::ptr::copy_nonoverlapping(crate::mem::kptr::<u8>(addr), e.as_mut_ptr(), BS);
                    wrbe32(e, 0, 0);
                    jwrite(pos, e);
                } else {
                    // the slot's snapshot goes into JCOPY (§8.12), the DMA will go already without the FS lock
                    let cp = JCOPY + ((nr - 1) * BSZ) as u64;
                    core::ptr::copy_nonoverlapping(crate::mem::kptr::<u8>(addr), crate::mem::kptr::<u8>(cp), BS);
                    reqs[nr] = ((JBLK[pos] as usize) * SPB, crate::mem::Pa::frame(cp), SPB as u16); nr += 1;
                }
                CJPOS[slot] = pos as u32 | if esc { 0x8000_0000 } else { 0 };
                pos = jnext(pos);
                dcnt_del(slot);
                CJRN[slot] = true;   // the block is in the journal; it will be written in place by a checkpoint
                dcnt_add(slot);
                CJOLD[slot] = false; // a fresh copy replaces the previous one
                tags += 1; written += 1;
                slot += 1;
            }
            if tags == 0 { break; }
            // mark the last tag (after the first tag there are another 16 bytes of the UUID,
            // so we remember the position explicitly rather than compute it from the current offset)
            crate::jbd2::mark_last(d, last_tag);
            reqs[0] = ((JBLK[dpos] as usize) * SPB, crate::mem::Pa::frame(JSTAGE), SPB as u16);
            // §8.12: the batch goes WITHOUT the FS lock (the snapshots are already in JCOPY/JSTAGE, the FS's
            // state it does not touch; the journal's statics are closed by JBUSY, the device is
            // ours). The released lock allows `nvme_batch` to yield the core.
            let t0 = crate::tsc::rdtsc();
            let depth = fs_unlock_outermost();
            jstage(3);
            crate::fs::blk_write_many(&reqs[..nr]);   // §8.13: it will wait for the deferred data too
            if wb_defer_mine() { wb_defer_finish(); } // the ways come out of flight at once rather than after the commit
            jstage(0);
            ph_note(3, t0);
            // The return of the lock is ITS OWN phase (10) rather than the tail of "the descriptor+the copies":
            // while we were giving the blocks to the device, the neighbours queued up behind the lock,
            // and that is their queue rather than our write.
            let tr = crate::tsc::rdtsc(); fs_relock(depth); ph_note(10, tr);
            JIO += 1;
            // a move across the end of the journal onto its own start is a defect of the estimate
            // `jrn_need`; after a crash such a transaction cannot be replayed
            if pos < dpos { crate::println!("ext2: THE JOURNAL HAS BEEN OVERFLOWED by the transaction seq {} (the descriptor {}, the tail {}) — the estimate \
                of the room is wrong", seq, dpos, pos); }
            JHEAD = pos;
        }
    }
    if written == 0 { return 0; }
    // THE DEVICE'S PHASE: a barrier — the commit — a barrier. NOT A SINGLE
    // byte of the FS's state is touched here (only the layout buffer and the journal's statics, closed by
    // the flag JBUSY), so the FS lock is released for that time. The device is
    // released TOO, and before the FS (the one holding the device does not ask for the state):
    // "the copies are durable before the commit" is kept not by a lock but by the barriers themselves —
    // the descriptor and the copies are written before the first one, the commit is submitted after it, and no
    // foreign write between them changes the order (the metadata are written in place
    // only by a checkpoint, and that one is under JBUSY). Holding the device across two barriers
    // (~90 ms 13 times a second) cost the inserts a tail: a foreign write of data
    // waited behind it entirely.
    {
        drop(dg);
        let depth = fs_unlock_outermost();
        // 4) a barrier: the descriptor and the copies go before the commit
        // STEP 2 OF SESSION 40: A FOREIGN deferred portion of data goes before the commit too.
        // The journal is free for the time of a data flush, which means the neighbour's last
        // portion may be flying next to it; with VWC=0 a barrier does not cover it, while
        // its file's metadata travel in OUR transaction (`JSel::Meta` takes all
        // the cache's dirty metadata). It is cheap: usually the mask is empty, and that is one
        // atomic read.
        crate::fs::blk_defer_absorb();
        BAR_SRC[BAR_TXN].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let tb = crate::tsc::rdtsc();
        jstage(4);
        crate::fs::blk_flush(); STATS_FLUSH += 1;
        jstage(0);
        ph_note(9, tb);
        // 5) the commit block goes BY A DURABLE WRITE (FUA) rather than by "a write + a barrier".
        //
        // Here stood the transaction's second barrier, and the counter showed exactly
        // twice as many barriers as transactions (430 per 215, 750 per 375).
        // The second barrier is needed for the sake of ONE block — to make the commit durable —
        // and that is exactly FUA: the command completes when the block is on the medium.
        // The order "the copies are durable before the commit" is kept by the first barrier, as
        // before. jbd2 gets by with one barrier in exactly the same way.
        // Where there is no FUA (virtio-blk), `blk_write_durable` does a write and a barrier
        // — the behaviour and the price are the former ones, so the barrier counter grows only
        // there.
        let c = stage(0);
        for b in c[..BS].iter_mut() { *b = 0; }
        wrbe32(c, 0, JMAGIC); wrbe32(c, 4, JBT_COMMIT); wrbe32(c, 8, seq);
        if !crate::fs::fua_write() {
            BAR_SRC[BAR_TXN].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            STATS_FLUSH += 1;
        }
        let t0 = crate::tsc::rdtsc();
        jstage(5);
        crate::fs::blk_write_durable((JBLK[JHEAD] as usize) * SPB, &c[..BS]);
        jstage(0);
        ph_note(4, t0);
        JHEAD = jnext(JHEAD);
        let tr = crate::tsc::rdtsc(); fs_relock(depth); ph_note(10, tr);
    }
    JSEQ = seq.wrapping_add(1);
    JCOMMITS += 1; JBLOCKS += written as u64;
    if !JDIRTY { JDIRTY = true; } // the journal has become "dirty" (s_start is already set before the write)
    written
}

/// Write the journalled metadata in place; the journal starts anew.
///
/// A barrier after the write in place is obligatory: the next transaction will overwrite
/// the log blocks, and without it after a crash a block could turn out to be neither in the journal
/// nor in place. While declaring the journal EMPTY (s_start=0, removing recover — two
/// barriers) is pointless while the epoch continues: the superblock with the new (JFIRST, JSEQ)
/// will leave before the next transaction under its barrier. `clean` is for a sync and
/// a shutdown: after them the journal must be clean for Linux too.
/// SAFETY: E1 + E5 + E6 — under the FS lock and with the journal claimed (`JBUSY`): a checkpoint
/// puts the COMMITTED metadata into their places, which is exactly the window in which a
/// neighbour must not start a transaction.
unsafe fn jrn_checkpoint_locked(clean: bool) {
    let mut n = writeback(Sel::Journaled, WbC::Other);
    // §8.9: the blocks changed AFTER a commit are put in place from the journal —
    // by their committed version; their current content will travel in the next transaction.
    for s in 0..ncache() {
        if CTAG[s] == 0 || !CJOLD[s] || CJRN[s] { continue; }
        let pos = CJPOS[s];
        let buf = stage(1);
        if jread((pos & 0x7fff_ffff) as usize, buf) {
            if pos & 0x8000_0000 != 0 { wrbe32(buf, 0, JMAGIC); }
            crate::fs::blk_write(((CTAG[s] - 1) as usize) * SPB, buf);
            JCKPT_OLD += 1; n += 1;
        } else { crate::println!("ext2: the checkpoint: the log block {} for the block {} was not read", pos & 0x7fff_ffff, CTAG[s] - 1); }
        CJOLD[s] = false;
    }
    if n > 0 { dev_barrier_src(BAR_CKPT); }
    JCKPTS += 1;
    JHEAD = JFIRST;
    JDIRTY = false;
    JSB_STALE = true;
    if clean { jrn_write_sb(0, JSEQ, true); if JRECOVER { set_recover(false); JRECOVER = false; } JSB_STALE = true; }
}
/// A checkpoint from the outside (a sync, a stop): the journal is declared empty.
/// SAFETY: E1 + E6 — the lock is taken on the same line (it was NOT once, and this function
/// WRITES: see the rule at `fs_lock`), and the checkpoint runs only with the journal free.
pub fn jrn_checkpoint() { let _fsg = fs_lock(); unsafe { if JRN && !JBUSY { jrn_busy_set(); jstage(6); jrn_checkpoint_locked(true); jrn_busy_clear(); } } }

// ═══ §8.5 THE FILE MAPPING MAP_SHARED ═════════════════════════════════════════
//
// Previously an mmap(MAP_SHARED, fd) answered ENODEV: emulating it with a copy without a write
// back would mean losing data (InnoDB's redo, the mmapped journals). Now THE PAGES OF
// THE BLOCK CACHE ITSELF are mapped into a user AS — the same bytes that read/write see,
// without a second copy and without questions of coherence. The slots under a mapping are pinned
// (they must not be evicted), while the pages' "dirtiness" is learnt by the D bit in the page table
// (msync/munmap/fsync collect it and mark the slots dirty).
//
// THERE IS NO LONGER a limitation by the cache's layout: the pages are mapped one by one both in
// a process's AS (§2.2) and in the kernel's identity AS (§3.3 — there a window of virtual
// addresses outside the identity is set aside for them). A file scattered over the disk is mapped in the same
// way as a contiguous one; the former refusal ENODEV is left only for the case when
// the blocks cannot be raised into the cache and pinned.
const NMAP: usize = 16;
#[derive(Clone, Copy)]
struct Mapping { vaddr: u64, ino: u32, fb0: u64, pages: usize }
const MAP0: Mapping = Mapping { vaddr: 0, ino: 0, fb0: 0, pages: 0 };
static mut MAPS: [Mapping; NMAP] = [MAP0; NMAP];

/// §2.2: the cache's slot for the file block fb of the inode ino (without an allocation). None — there is no
/// such block or it is not in the cache (for a pinned mapping that does not happen).
/// SAFETY: E1 — under the FS lock; it returns a cache slot index, not a pointer (E2).
unsafe fn map_slot(ino: u32, fb: u64) -> Option<usize> {
    let mut inb = [0u8; 128];
    ino_read(ino, &mut inb);
    let mut add = 0;
    let bn = bmap(ino, &mut inb, fb, false, &mut add);
    if bn == 0 { return None; }
    find_slot(bn)
}

/// How many ways of a set are pinned right now (§2.1: the invariant "at least one way
/// of a set is evictable" — otherwise cblk has nowhere to put a miss).
/// SAFETY: E1 — under the FS lock; it counts the pinned ways of one set for `victim`.
unsafe fn pinned_in_set(set: usize) -> usize {
    let mut n = 0;
    for w in 0..WAYS { if CPIN[way_slot(w, set)] > 0 { n += 1; } }
    n
}

/// Prepare a file's mapping (the descriptor h, the offset off, the length len):
/// allocate the missing blocks, raise them into the cache and pin them. It returns the number of pages.
/// §2.2: THE CONTIGUITY of the window is no longer required — the cache's pages are mapped
/// one by one (a page table is page-by-page anyway), so a file scattered over the disk
/// no longer gets an ENODEV.
/// Will a mapping of such a length fit into the block cache in principle (by the sets'
/// capacity). A cheap check BEFORE the attempt: mmap decides whether to give a shared
/// mapping as the cache's pages or as a private copy (see syscall::mmap).
pub fn mmap_fits(len: usize) -> bool { len.div_ceil(BSZ) <= nsets() }

pub fn mmap_prepare(h: i64, off: u64, len: usize) -> Option<usize> {
    let _fsg = fs_lock();
    // SAFETY: E1 + E2 — the lock is taken on the line above, and this is the function that
    // CREATES the long-lived case of E2: the slots it raises are pinned (`CPIN`) and their
    // identity addresses go into a process's page tables, so they must not be re-tagged
    // while the mapping lives. The `pages > nsets()` refusal is what keeps `victim`'s own
    // invariant — that every set still has a non-pinned way to choose.
    unsafe {
    if !active() || BS != BSZ || !off.is_multiple_of(BS as u64) { return None; }
    let i = of(h)?;
    let pages = len.div_ceil(BS);
    if pages == 0 || pages > nsets() { return None; }
    let ino = OF_INO[i];
    let mut inb = [0u8; 128];
    ino_read(ino, &mut inb);
    let mut added = 0u32;
    let mut done = 0usize;
    while done < pages {
        let fb = off / BS as u64 + done as u64;
        let b = bmap(ino, &mut inb, fb, true, &mut added);
        if b == 0 { break; }
        let set = set_of(b);
        // do not occupy the set's last evictable way
        if find_slot(b).is_none() && pinned_in_set(set) >= WAYS - 1 { break; }
        let p = crate::mem::kpa_of(cblk(b));
        let slot = ((p - CACHE_BASE) / BSZ as u64) as usize;
        if CPIN[slot] == 0 && pinned_in_set(set) >= WAYS - 1 { break; }
        CPIN[slot] += 1;
        done += 1;
    }
    if done != pages {                       // it did not work out — roll the pins back
        for k in 0..done {
            let fb = off / BS as u64 + k as u64;
            if let Some(slot) = map_slot(ino, fb) { if CPIN[slot] > 0 { CPIN[slot] -= 1; } }
        }
        return None;
    }
    if added > 0 { let v = rd32(&inb, 28) + added * SPB as u32; wr32(&mut inb, 28, v); }
    let end = off + (pages * BS) as u64;
    if end > i_size(&inb) { set_size(&mut inb, end); }
    ino_write(ino, &inb);
    Some(pages)
    }
}
/// §2.2: roll back the pins of a prepared but failed mapping.
pub fn mmap_unprepare(h: i64, off: u64, pages: usize) {
    let _fsg = fs_lock();
    // SAFETY: E1 — the FS lock is taken on the line above.
    unsafe {
        let k = match of(h) { Some(k) => k, None => return };
        for i in 0..pages {
            if let Some(slot) = map_slot(OF_INO[k], off / BS as u64 + i as u64) {
                if CPIN[slot] > 0 { CPIN[slot] -= 1; }
            }
        }
    }
}
/// §2.2: the identity address of the page i of a prepared mapping (0 — there is none).
pub fn mmap_page(h: i64, off: u64, i: usize) -> u64 {
    let _fsg = fs_lock();
    // SAFETY: E1 — the FS lock is taken on the line above.
    unsafe {
        let k = match of(h) { Some(k) => k, None => return 0 };
        match map_slot(OF_INO[k], off / BS as u64 + i as u64) {
            Some(slot) => CACHE_BASE + (slot * BSZ) as u64,
            None => 0,
        }
    }
}
/// Remember the mapping (after the caller has mapped the pages into the AS).
pub fn mmap_register(vaddr: u64, off: u64, pages: usize, h: i64) {
    let _fsg = fs_lock();
    // SAFETY: E1 — the FS lock is taken on the line above.
    unsafe {
        let ino = match of(h) { Some(i) => OF_INO[i], None => 0 };
        let fb0 = off / BS as u64;
        for m in (*addr_of_mut!(MAPS)).iter_mut() {
            if m.pages == 0 {
                *m = Mapping { vaddr, ino, fb0, pages };
                MAPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
        crate::println!("ext2: the table of the mappings is full ({})", NMAP);
    }
}
/// Collect the Dirty bit from the mappings' pages (the ones intersecting [va, va+len), or from
/// all of them when len == 0) and mark the corresponding cache slots dirty.
pub fn mmap_harvest(va: u64, len: usize) -> usize {
    if MAPPED.load(core::sync::atomic::Ordering::Relaxed) == 0 { return 0; } // there are no mappings — the lock is not needed either
    let _fsg = fs_lock();
    if !active() { return 0; }
    let mut n = 0;
    // SAFETY: E1 — the lock is taken above; the mapping table is module state like the rest.
    unsafe {
        for k in 0..NMAP {
            let m = MAPS[k];
            if m.pages == 0 { continue; }
            if len != 0 && (va + len as u64 <= m.vaddr || va >= m.vaddr + (m.pages * BSZ) as u64) { continue; }
            for p in 0..m.pages {
                if !crate::paging::take_dirty(m.vaddr + (p * BSZ) as u64) { continue; }
                let slot = match map_slot(m.ino, m.fb0 + p as u64) { Some(s) => s, None => continue };
                if CTAG[slot] != 0 {
                    dcnt_del(slot);
                    if !CDIRTY[slot] { CDIRTY[slot] = true; CDIRTIME[slot] = dstamp(); }
                    COWN[slot] = m.ino; CJRN[slot] = false;
                    dcnt_add(slot);
                    n += 1;
                }
            }
        }
    }
    n
}
/// msync(addr, len, flags): collect the mapping's changes and (with MS_SYNC/MS_ASYNC)
/// send them to the disk by the same path as a file's fsync.
pub fn msync(va: u64, len: usize) -> i64 {
    let _fsg = fs_lock();
    let _rel = MayRelease::set(true); // an independent synchronisation, like an fsync
    let snap = crate::fs::WRITE_FAILED.load(core::sync::atomic::Ordering::SeqCst);
    let n = mmap_harvest(va, len);
    if n == 0 { return 0; }
    // SAFETY: E1 + E4 — under the lock, and `MayRelease::set(true)` above says releasing it
    // around the device is allowed: an msync is an independent synchronisation, like an fsync.
    unsafe {
        let mut ino = 0u32;
        for k in 0..NMAP { let m = MAPS[k]; if m.pages != 0 && va >= m.vaddr && va < m.vaddr + (m.pages * BSZ) as u64 { ino = m.ino; } }
        if JRN && JBUSY { let t0 = crate::tsc::rdtsc(); jrn_wait_free(); ph_note(8, t0); } // §8.11: like an fsync — wait for the journal, do not bypass it
        if JRN && !JBUSY { jrn_commit(if ino != 0 { ino } else { u32::MAX }); }
        else { let w = writeback(if ino != 0 { Sel::Own(ino) } else { Sel::All }, if ino != 0 { WbC::Fsync } else { WbC::Other }); if w > 0 { dev_barrier(); } }
    }
    io_err_since(snap)
}
/// Remove a mapping (munmap/an exit): collect the changes and release the pins.
pub fn munmap(va: u64, len: usize) -> bool {
    // A cheap refusal BEFORE the lock. `munmap` is called for ANY memory, while file
    // mappings usually do not exist at all; taking the common FS lock because of that means
    // waiting for a neighbour that reads from the disk. The measurement showed exactly that: a munmap of
    // 1.7 ms per call while the removal of the mappings costs tens of microseconds.
    if MAPPED.load(core::sync::atomic::Ordering::Relaxed) == 0 { return false; }
    let _fsg = fs_lock();
    if !active() { return false; }
    // SAFETY: E1 — the lock is taken above.
    let mut hit = false;
    // SAFETY: E1 — the lock is taken above; the mapping table is module state.
    unsafe {
        for k in 0..NMAP {
            let m = MAPS[k];
            if m.pages == 0 { continue; }
            if va + len.max(1) as u64 <= m.vaddr || va >= m.vaddr + (m.pages * BSZ) as u64 { continue; }
            mmap_harvest(m.vaddr, m.pages * BSZ);
            for p in 0..m.pages {
                if let Some(slot) = map_slot(m.ino, m.fb0 + p as u64) { if CPIN[slot] > 0 { CPIN[slot] -= 1; } }
            }
            MAPS[k] = MAP0;
            MAPPED.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
            hit = true;
        }
    }
    hit
}
/// How many records in the registry of the file mappings are occupied (a fast check without
/// the lock: a zero means that a `munmap`/`msync` is certainly not about us).
pub static MAPPED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Are there active file mappings (the diagnostics/a test).
/// SAFETY: E1 — under the FS lock (every caller holds it); it only counts live rows.
pub fn mmap_count() -> usize { unsafe { (*addr_of_mut!(MAPS)).iter().filter(|m| m.pages != 0).count() } }
