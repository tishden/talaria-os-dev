//! A minimal NVMe driver (PLAN-V1.1 §4.5): the admin queue + one IO pair,
//! reading/writing blocks, POLLING (without interrupts — the reactor/FS are synchronous anyway;
//! we do not switch the IEN on). The EC2 path: an EBS volume = NVMe (class 0x0108) — the disk for
//! TFS/ext2 instead of virtio-blk. The limitation: nsid=1, an LBA of 512 B.
//!
//! §8.1: LARGE TRANSFERS AND A PIPELINE. It used to be 4 KB per command (PRP1+PRP2) and
//! a wait for the completion of each: the flush of a thousand dirty ext2 blocks turned into
//! a thousand consecutive rotational delays of the disk. Now it is:
//!   - a PRP list (a page of pointers) -> up to `MAX_XFER` in one command;
//!   - up to QD-1 commands in flight, one doorbell per batch.
//! The order of the writes between the commands IN FLIGHT is undefined — the ext2
//! journal takes that into account (the barriers are put by an explicit Flush between the phases, §8.4).
//!
//! THE QUEUE DEPTH FOR DIFFERENT OWNERS (session 23). A batch is not yet a depth:
//! a batch is submitted by ONE caller and the same one waits for it entirely, so a barrier
//! (`FLUSH`, tens of milliseconds on EBS) occupied the queue for all its time, and
//! a foreign read went behind it rather than past it. In virtio that is already separated (`fs::io_one`,
//! `fs::blk_flush`), here it is the same:
//!   - "submit" and "wait for ONE'S OWN command" are separated;
//!   - a command's identifier (the CID) is a SLOT NUMBER rather than a counter: by it
//!     a completion finds its owner, and anybody at all may take a completion off;
//!   - every CID has its own page of the PRP list, living exactly until the completion.
//! The order "my writes -> my barrier" is held by the fact that everyone waits for THEIR OWN
//! completions, while the mutual exclusion of the ring is held by a short lock in `fs.rs`.

const QD: usize = 64;            // the depth of the queues (admin and IO)
const SQE: usize = 64;           // bytes per SQ entry
const CQE: usize = 16;           // bytes per CQ entry

/// The ceiling of one transfer (in bytes) in the absence of/with a large MDTS: 128 KB = 32 ext2
/// blocks of 4 KB — the same number of PRP elements, one page of the list holds 512.
const MAX_XFER: usize = 128 * 1024;
const PRP_PER_PAGE: usize = 512;

pub struct Nvme {
    regs: u64,                   // BAR0
    dstrd: u64,                  // CAP.DSTRD (the step of the doorbells)
    asq: u64, acq: u64,          // the admin SQ/CQ (phys = virt)
    iosq: u64, iocq: u64,        // IO SQ/CQ qid=1
    prp: u64,                    // the page of the PRP lists (one per command in flight)
    a_tail: u16, a_head: u16, a_phase: bool, a_cid: u16,
    io_tail: u16, io_head: u16, io_phase: bool,
    // THE TABLE OF THE COMMANDS IN FLIGHT BY CID. Bit masks rather than an array: QD = 64, that
    // is, the whole table is three machine words, and "is there a free CID",
    // "has MY completion come" cost one instruction each.
    busy: u64,                   // the CID is occupied: the command has been submitted, its owner has not taken it
    done: u64,                   // the completion has come and is waiting for its owner
    err: u64,                    // ...and the status in it was non-zero
    max_xfer: usize,             // the transfer's limit by the controller's MDTS
    /// The volatile write cache (Identify Controller, VWC). There is none — a barrier is not needed.
    vwc: bool,
    pub nsze: u64,               // the size of namespace 1 in blocks
    pub cmds: u64,               // the IO commands in total (the diagnostics of fsync)
}

impl Nvme {
    #[inline] fn r32(&self, off: u64) -> u32 { unsafe { core::ptr::read_volatile((self.regs + off) as *const u32) } }
    #[inline] fn w32(&self, off: u64, v: u32) { unsafe { core::ptr::write_volatile((self.regs + off) as *mut u32, v) } }
    #[inline] fn w64(&self, off: u64, v: u64) { unsafe { core::ptr::write_volatile((self.regs + off) as *mut u64, v) } }
    #[inline] fn doorbell_sq(&self, qid: u64) -> u64 { 0x1000 + (2 * qid) * (4 << self.dstrd) }
    #[inline] fn doorbell_cq(&self, qid: u64) -> u64 { 0x1000 + (2 * qid + 1) * (4 << self.dstrd) }

    fn wait_rdy(&self, want: bool) -> bool {
        let hz = crate::tsc::calibrate_cached();
        let t0 = crate::tsc::rdtsc();
        loop {
            if (self.r32(0x1C) & 1 != 0) == want { return true; }
            if crate::tsc::rdtsc() - t0 > 2 * hz { return false; } // 2 c
            core::hint::spin_loop();
        }
    }

    /// Send a command into the ADMIN queue and wait for its completion (polling the CQ by
    /// the phase bit). cmd is 16 dwords; the cid is filled in here. 0 = OK.
    ///
    /// Admin only: the IO queue has a depth, and waiting there for "the first completion at
    /// the head" is impossible — it may be someone else's. IO goes through `push_*`/`reap`.
    fn submit(&mut self, cmd: &mut [u32; 16]) -> u16 {
        let (sq, cq, qid) = (self.asq, self.acq, 0u64);
        let (tail, head, phase, cid) = {
            self.a_cid = self.a_cid.wrapping_add(1);
            (self.a_tail, self.a_head, self.a_phase, self.a_cid)
        };
        cmd[0] = (cmd[0] & 0xFFFF) | ((cid as u32) << 16);
        unsafe {
            core::ptr::copy_nonoverlapping(cmd.as_ptr() as *const u8, (sq + (tail as u64) * SQE as u64) as *mut u8, SQE);
        }
        crate::arch::barrier::dma_wmb();   // the command is in the queue's memory before the doorbell says so
        let new_tail = (tail + 1) % QD as u16;
        self.w32(self.doorbell_sq(qid), new_tail as u32);
        // wait for the completion (one command at a time — it will come first)
        let hz = crate::tsc::calibrate_cached();
        let t0 = crate::tsc::rdtsc();
        let status = loop {
            let ce = (cq + (head as u64) * CQE as u64) as *const u32;
            let dw3 = unsafe { core::ptr::read_volatile(ce.add(3)) };
            if ((dw3 >> 16) & 1 != 0) == phase { crate::arch::barrier::dma_rmb(); break (dw3 >> 17) as u16; }
            if crate::tsc::rdtsc() - t0 > 2 * hz { return 0xFFFF; } // a timeout
            core::hint::spin_loop();
        };
        let new_head = (head + 1) % QD as u16;
        let new_phase = if new_head == 0 { !phase } else { phase };
        self.w32(self.doorbell_cq(qid), new_head as u32);
        self.a_tail = new_tail; self.a_head = new_head; self.a_phase = new_phase;
        status
    }

    /// Assemble a read/write command with PRP1/PRP2/a PRP list. prp_slot is the number
    /// of the list's page (one per command in flight).
    fn build_rw(&mut self, write: bool, lba: u64, nblk: u16, buf: u64, prp_slot: usize, fua: bool) -> [u32; 16] {
        let mut cmd = [0u32; 16];
        cmd[0] = if write { 0x01 } else { 0x02 };
        cmd[1] = 1; // nsid
        cmd[6] = buf as u32; cmd[7] = (buf >> 32) as u32; // PRP1 (it may be with an offset in the page)
        let bytes = nblk as usize * 512;
        let first_page_rest = 4096 - (buf & 0xFFF) as usize;
        if bytes > first_page_rest {
            let p2 = (buf & !0xFFF) + 4096;              // the second page of the transfer
            let rest = bytes - first_page_rest;
            if rest <= 4096 {                             // PRP2 = the page itself
                cmd[8] = p2 as u32; cmd[9] = (p2 >> 32) as u32;
            } else {                                      // PRP2 = the list of the remaining pages
                let list = self.prp + (prp_slot as u64) * 4096;
                let npages = rest.div_ceil(4096);
                for i in 0..npages.min(PRP_PER_PAGE) {
                    let a = p2 + (i as u64) * 4096;
                    unsafe { core::ptr::write_volatile((list + (i as u64) * 8) as *mut u64, a); }
                }
                cmd[8] = list as u32; cmd[9] = (list >> 32) as u32;
            }
        }
        cmd[10] = lba as u32; cmd[11] = (lba >> 32) as u32;
        // FUA (CDW12 bit 30): a write is considered completed only when it is
        // ON THE MEDIUM. One such write replaces the pair "a write + a Flush" —
        // it is exactly by this that jbd2 gets by with one barrier per transaction instead of two.
        cmd[12] = (nblk - 1) as u32 | if fua { 1 << 30 } else { 0 };
        cmd
    }

    /// Take a free CID. None — all the QD-1 are taken (submitting deeper is impossible:
    /// the SQ must stay non-full, otherwise the tail would catch up with the head).
    fn cid_alloc(&mut self) -> Option<u16> {
        let free = !(self.busy | self.done);
        if free == 0 { return None; }
        if self.busy.count_ones() as usize >= QD - 1 { return None; }
        let cid = free.trailing_zeros() as u16;
        self.busy |= 1u64 << cid;
        self.err &= !(1u64 << cid);
        Some(cid)
    }
    /// Put a ready command into the SQ under an already taken CID (without a doorbell).
    fn sq_write(&mut self, cid: u16, cmd: &mut [u32; 16]) {
        cmd[0] = (cmd[0] & 0xFFFF) | ((cid as u32) << 16);
        unsafe {
            core::ptr::copy_nonoverlapping(cmd.as_ptr() as *const u8,
                (self.iosq + (self.io_tail as u64) * SQE as u64) as *mut u8, SQE);
        }
        self.io_tail = (self.io_tail + 1) % QD as u16;
        self.cmds += 1;
    }

    /// SUBMIT a read/write without waiting: it returns the CID by which the owner will later
    /// recognise its own completion. None — there are no free CIDs (ring and wait).
    /// `nblk` must not exceed `max_blocks()` — it is the caller that is bound to cut it.
    pub fn push_rw(&mut self, write: bool, lba: u64, nblk: u16, buf: u64) -> Option<u16> {
        self.push_rw_ex(write, lba, nblk, buf, false)
    }
    /// The same with FUA: the completion of such a write means "the data is on the medium".
    pub fn push_rw_ex(&mut self, write: bool, lba: u64, nblk: u16, buf: u64, fua: bool) -> Option<u16> {
        if nblk == 0 { return None; }
        let cid = self.cid_alloc()?;
        // The page of the PRP list is EACH CID'S OWN and lives exactly as long:
        // the list is needed by the device until the end of the transfer, not until the write into the SQ.
        let mut cmd = self.build_rw(write, lba, nblk, buf, cid as usize, fua);
        self.sq_write(cid, &mut cmd);
        Some(cid)
    }
    /// SUBMIT a barrier (a Flush nsid=1) without waiting. It is for this that all this was done: while
    /// the barrier flies, the ring stays free for foreign reads.
    pub fn push_flush(&mut self) -> Option<u16> {
        let cid = self.cid_alloc()?;
        let mut cmd = [0u32; 16];
        cmd[0] = 0x00; cmd[1] = 1;
        self.sq_write(cid, &mut cmd);
        Some(cid)
    }
    /// A doorbell into the SQ doorbell — one MMIO write for the whole batch.
    pub fn kick(&mut self) { crate::arch::barrier::dma_wmb(); self.w32(self.doorbell_sq(1), self.io_tail as u32); }

    /// Take off ALL the ready completions (by whomever they were submitted) and lay them out
    /// by CID. It returns the number taken off.
    pub fn reap(&mut self) -> u16 {
        let mut n = 0;
        loop {
            let ce = (self.iocq + (self.io_head as u64) * CQE as u64) as *const u32;
            let dw3 = unsafe { core::ptr::read_volatile(ce.add(3)) };
            if ((dw3 >> 16) & 1 != 0) != self.io_phase { break; }
            let cid = (dw3 & 0xFFFF) as u16;
            if (cid as usize) < QD {
                let bit = 1u64 << cid;
                // We do not resurrect the completion of a CID abandoned by a timeout: its busy
                // bit will not be cleared, and the slot honestly drops out of circulation.
                if self.busy & bit != 0 {
                    self.done |= bit;
                    if (dw3 >> 17) as u16 != 0 { self.err |= bit; }
                }
            }
            self.io_head = (self.io_head + 1) % QD as u16;
            if self.io_head == 0 { self.io_phase = !self.io_phase; }
            n += 1;
        }
        // The payload of a completed read is loaded AFTER its completion entry was.
        if n > 0 { crate::arch::barrier::dma_rmb(); self.w32(self.doorbell_cq(1), self.io_head as u32); }
        n
    }
    /// Take ONE'S OWN completion. None — it has not come yet. Some(true) — the status is 0.
    pub fn take_done(&mut self, cid: u16) -> Option<bool> {
        let bit = 1u64 << cid;
        if self.done & bit == 0 { return None; }
        self.done &= !bit;
        self.busy &= !bit;
        let ok = self.err & bit == 0;
        self.err &= !bit;
        Some(ok)
    }
    /// Abandon a CID without having waited (a timeout): the slot drops out forever, because
    /// the device may still write into its buffer, and the page of the PRP list is its.
    pub fn abandon(&mut self, cid: u16) { self.done &= !(1u64 << cid); }
    /// How many commands are in flight (the diagnostics of the depth).
    pub fn inflight(&self) -> u32 { self.busy.count_ones() }
    /// Whether the controller has a VOLATILE write cache (VWC). It has not — a Flush is not needed.
    pub fn has_write_cache(&self) -> bool { self.vwc }
    /// The ceiling of ONE command in 512-blocks (the controller's MDTS).
    ///
    /// It is the caller that is bound to cut it, and that is a measurement, not caution: while the files
    /// lay scattered, the ranges were short and the limit did not surface; after
    /// contiguous runs of blocks the very first fsync of 512 blocks would have gone in one
    /// command of 2 MB — a controller rejects such a one both on a LIVE EBS and on
    /// QEMU-NVMe alike. At the same time the PRP list fits into its own page.
    pub fn max_blocks(&self) -> u16 {
        // The second ceiling is ITS OWN page of the PRP list: it holds PRP_PER_PAGE
        // addresses of pages, plus the first page goes into PRP1.
        let by_prp = ((PRP_PER_PAGE + 1) * 4096 / 512) as u64;
        ((self.max_xfer / 512).max(1) as u64).min(by_prp).min(u16::MAX as u64) as u16
    }

    /// One admin command with no data (Get Features: the number of queues) — the admin queue's
    /// completion always raises the device's vector 0, so this is how a gate fires an
    /// interrupt on demand (`talaria.test=its`). True — the command completed without an error.
    // The caller is the arm64 bring-up (a device tree, an ITS); the x86 build has none.
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    pub fn admin_ping(&mut self) -> bool {
        let mut c = [0u32; 16];
        c[0] = 0x0A; c[10] = 0x07;
        self.submit(&mut c) == 0
    }
    /// A SYNCHRONOUS read BEFORE the device is published (`open_nvme_boot`): the recognition of
    /// the disk goes earlier than the ring's lock appears, and there are no other owners of the queue
    /// at that moment by construction.
    pub fn read_blocks_boot(&mut self, lba: usize, buf: &mut [u8]) -> bool {
        let maxb = self.max_blocks();
        let mut done = 0usize;
        while done < buf.len() {
            let nb = (((buf.len() - done) / 512).max(1) as u16).min(maxb);
            let Some(cid) = self.push_rw(false, (lba + done / 512) as u64, nb, buf.as_ptr() as u64 + done as u64) else { return false };
            self.kick();
            let hz = crate::tsc::calibrate_cached();
            let t0 = crate::tsc::rdtsc();
            loop {
                self.reap();
                if let Some(ok) = self.take_done(cid) { if !ok { return false; } break; }
                if crate::tsc::rdtsc().wrapping_sub(t0) > 5 * hz { self.abandon(cid); return false; }
                core::hint::spin_loop();
            }
            done += nb as usize * 512;
        }
        true
    }
}

/// Find an NVMe (class 0x0108) on the PCIe and initialise it: the admin pair, an IO pair
/// qid=1, Identify Namespace (nsze). None — there is no device/an error.
/// Bring up a SPECIFIC (the n-th) NVMe controller from the PCI table.
pub fn probe_nth(n: usize) -> Option<Nvme> {
    let d = match crate::pci::find_class_nth(0x010802, n) {
        Some(d) => d,
        None => { if n == 0 { crate::println!("nvme: a controller (class 0x0108) was not found in the PCI table"); } return None; }
    };
    let (bar0, _sz) = crate::pci::bar(d.bus, d.dev, d.func, 0);
    if bar0 == 0 { crate::println!("nvme: BAR0 is unavailable"); return None; }
    // the queues and the Identify buffer: physically contiguous pages (identity: phys==virt)
    let asq = crate::mem::alloc_contig(1)?; let acq = crate::mem::alloc_contig(1)?;
    let iosq = crate::mem::alloc_contig(1)?; let iocq = crate::mem::alloc_contig(1)?;
    let idbuf = crate::mem::alloc_contig(1)?;
    // §8.1: a page of the PRP list per EVERY CID (the commands go in a pipeline, and
    // the list is needed by the device until the end of the transfer — otherwise the next one would overwrite it)
    let prp = crate::mem::alloc_contig(QD as u64)?;
    for p in [asq, acq, iosq, iocq, idbuf] { unsafe { core::ptr::write_bytes(p as *mut u8, 0, 4096) }; }
    unsafe { core::ptr::write_bytes(prp as *mut u8, 0, QD * 4096) };
    let cap = unsafe { core::ptr::read_volatile(bar0 as *const u64) };
    let mut n = Nvme {
        regs: bar0, dstrd: (cap >> 32) & 0xF,
        asq, acq, iosq, iocq, prp,
        a_tail: 0, a_head: 0, a_phase: true, a_cid: 0,
        io_tail: 0, io_head: 0, io_phase: true,
        busy: 0, done: 0, err: 0, max_xfer: MAX_XFER, vwc: true,
        nsze: 0, cmds: 0,
    };
    // the reset: EN=0, wait for RDY=0; set up the admin pair; EN=1 (IOSQES=6, IOCQES=4, MPS=0=4K)
    n.w32(0x14, 0);
    if !n.wait_rdy(false) { crate::println!("nvme: the controller did not reset"); return None; }
    n.w32(0x24, ((QD as u32 - 1) << 16) | (QD as u32 - 1)); // AQA
    n.w64(0x28, asq); n.w64(0x30, acq);
    n.w32(0x14, (6 << 16) | (4 << 20) | 1); // CC: IOSQES|IOCQES|EN
    if !n.wait_rdy(true) { crate::println!("nvme: the controller did not come up"); return None; }
    // the IO CQ qid=1 (op 0x05, without interrupts), then the IO SQ qid=1 (op 0x01, cqid=1)
    let mut c = [0u32; 16];
    c[0] = 0x05; c[6] = iocq as u32; c[7] = (iocq >> 32) as u32;
    c[10] = ((QD as u32 - 1) << 16) | 1; c[11] = 1; // PC=1
    if n.submit(&mut c) != 0 { crate::println!("nvme: create IO CQ fail"); return None; }
    let mut c = [0u32; 16];
    c[0] = 0x01; c[6] = iosq as u32; c[7] = (iosq >> 32) as u32;
    c[10] = ((QD as u32 - 1) << 16) | 1; c[11] = (1 << 16) | 1; // cqid=1, PC=1
    if n.submit(&mut c) != 0 { crate::println!("nvme: create IO SQ fail"); return None; }
    // Identify Controller (CNS=1): the MDTS is the transfer's ceiling in units of MPSMIN (4 KB)
    let mut c = [0u32; 16];
    c[0] = 0x06; c[6] = idbuf as u32; c[7] = (idbuf >> 32) as u32; c[10] = 1;
    if n.submit(&mut c) == 0 {
        let mdts = unsafe { core::ptr::read_volatile((idbuf + 77) as *const u8) };
        if mdts > 0 && mdts < 20 {
            n.max_xfer = MAX_XFER.min(4096usize << mdts);
        }
        // VWC (byte 525, bit 0) — WHETHER THE CONTROLLER HAS A VOLATILE WRITE CACHE.
        // If it has none, a write is durable at the moment the command completes, and
        // the Flush command does NOTHING — while on EBS it costs milliseconds. Linux
        // asks exactly this (`blk_queue_write_cache`) and on such a device
        // issues no barriers at all; we issued them always.
        n.vwc = unsafe { core::ptr::read_volatile((idbuf + 525) as *const u8) } & 1 != 0;
    }
    // Identify Namespace 1 (CNS=0): the nsze + a check that the LBA is 512
    let mut c = [0u32; 16];
    c[0] = 0x06; c[1] = 1; c[6] = idbuf as u32; c[7] = (idbuf >> 32) as u32; c[10] = 0;
    if n.submit(&mut c) != 0 { crate::println!("nvme: identify ns fail"); return None; }
    n.nsze = unsafe { core::ptr::read_volatile(idbuf as *const u64) };
    let flbas = unsafe { core::ptr::read_volatile((idbuf + 26) as *const u8) } & 0xF;
    let lbads = unsafe { core::ptr::read_volatile((idbuf + 128 + 16 * flbas as u64 + 2) as *const u8) };
    crate::mem::free_contig(idbuf, 1);
    if lbads != 9 { crate::println!("nvme: an LBA of {} bytes is not supported (512 is needed)", 1u32 << lbads); return None; }
    crate::println!("nvme: {:02x}:{:02x}.{} BAR0={:#x} nsze={} blocks ({} MB), an IO pair qid=1 of depth {}, a transfer of up to {} KB, polling; the volatile \
        write cache is {}",
        d.bus, d.dev, d.func, bar0, n.nsze, n.nsze * 512 / 1048576, QD, n.max_xfer / 1024,
        if n.vwc { "THERE (a barrier is mandatory)" } else { "ABSENT (a write is durable on completion — a barrier is not needed)" });
    Some(n)
}
