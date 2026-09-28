//! A minimal Intel 82599 driver (one RX and one TX queue) on top of VFIO,
//! noiommu. The initialisation sequence is ported from ixy.rs (MIT),
//! §§4.6.3–4.6.8 of the datasheet. Exactly the same ring logic that goes into the cell.

use crate::dma::HugeRegion;
use crate::vfio::Bar;
use std::time::{Duration, Instant};

// --- registers (BAR0) ---
const CTRL: usize = 0x00000;
const CTRL_EXT: usize = 0x00018;
const EIMC: usize = 0x00888;
const EEC: usize = 0x10010;
const RDRXCTL: usize = 0x02F00;
const AUTOC: usize = 0x042A0;
const LINKS: usize = 0x042A4;
const RXCTRL: usize = 0x03000;
const HLREG0: usize = 0x04240;
const FCTRL: usize = 0x05080;
const RTTDCS: usize = 0x04900;
const DTXMXSZRQ: usize = 0x08100;
const DMATXCTL: usize = 0x04A80;

const fn rxpbsize(i: usize) -> usize { 0x03C00 + i * 4 }
const fn txpbsize(i: usize) -> usize { 0x0CC00 + i * 4 }
const fn rdbal(i: usize) -> usize { 0x01000 + i * 0x40 }
const fn rdbah(i: usize) -> usize { 0x01004 + i * 0x40 }
const fn rdlen(i: usize) -> usize { 0x01008 + i * 0x40 }
const fn rdh(i: usize) -> usize { 0x01010 + i * 0x40 }
const fn rdt(i: usize) -> usize { 0x01018 + i * 0x40 }
const fn rxdctl(i: usize) -> usize { 0x01028 + i * 0x40 }
const fn srrctl(i: usize) -> usize { 0x02100 + i * 4 } // i<=15
const fn dca_rxctrl(i: usize) -> usize { 0x02200 + i * 4 } // i<=15
const fn tdbal(i: usize) -> usize { 0x06000 + i * 0x40 }
const fn tdbah(i: usize) -> usize { 0x06004 + i * 0x40 }
const fn tdlen(i: usize) -> usize { 0x06008 + i * 0x40 }
const fn tdh(i: usize) -> usize { 0x06010 + i * 0x40 }
const fn tdt(i: usize) -> usize { 0x06018 + i * 0x40 }
const fn txdctl(i: usize) -> usize { 0x06028 + i * 0x40 }
const fn ral(i: usize) -> usize { 0x05400 + i * 8 }
const fn rah(i: usize) -> usize { 0x05404 + i * 8 }

// --- bit fields ---
const CTRL_RST_MASK: u32 = 0x04000000 | 0x00000008; // RST | LNK_RST
const EEC_ARD: u32 = 0x0000_0200;
const RDRXCTL_DMAIDONE: u32 = 0x0000_0008;
const RDRXCTL_CRCSTRIP: u32 = 0x0000_0002;
const RDRXCTL_RSCFRSTSIZE: u32 = 0x003E_0000;
const RDRXCTL_RSCACKC: u32 = 0x0200_0000;
const RDRXCTL_FCOE_WRFIX: u32 = 0x0400_0000;
const AUTOC_LMS_MASK: u32 = 0x7 << 13;
const AUTOC_LMS_10G_SERIAL: u32 = 0x3 << 13;
const AUTOC_10G_PMA_PMD_MASK: u32 = 0x0000_0180;
const AUTOC_AN_RESTART: u32 = 0x0000_1000;
const RXCTRL_RXEN: u32 = 0x1;
const RXPBSIZE_128KB: u32 = 0x0002_0000;
const TXPBSIZE_40KB: u32 = 0x0000_A000;
const HLREG0_RXCRCSTRP: u32 = 0x0000_0002;
const HLREG0_TXCRCEN: u32 = 0x0000_0001;
const HLREG0_TXPADEN: u32 = 0x0000_0400;
const FCTRL_BAM: u32 = 0x0000_0400;
const FCTRL_MPE: u32 = 0x0000_0100;
const FCTRL_UPE: u32 = 0x0000_0200;
const SRRCTL_DESCTYPE_MASK: u32 = 0x0E00_0000;
const SRRCTL_DESCTYPE_ADV_ONEBUF: u32 = 0x0200_0000;
const SRRCTL_DROP_EN: u32 = 0x1000_0000;
const CTRL_EXT_NS_DIS: u32 = 0x0001_0000;
const RTTDCS_ARBDIS: u32 = 0x0000_0040;
const DMATXCTL_TE: u32 = 0x1;
const RXDCTL_ENABLE: u32 = 0x0200_0000;
const TXDCTL_ENABLE: u32 = 0x0200_0000;
const LINKS_UP: u32 = 0x4000_0000;
const RXD_STAT_DD: u32 = 0x01;
const RXD_STAT_EOP: u32 = 0x02;
const TXD_CMD_EOP: u32 = 0x0100_0000;
const TXD_CMD_RS: u32 = 0x0800_0000;
const TXD_CMD_IFCS: u32 = 0x0200_0000;
const TXD_CMD_DEXT: u32 = 0x2000_0000;
const ADVTXD_DTYP_DATA: u32 = 0x0030_0000;
const ADVTXD_PAYLEN_SHIFT: u32 = 14;

#[cfg(not(feature = "bigring"))]
const NUM_RX: usize = 512;
#[cfg(not(feature = "bigring"))]
const NUM_TX: usize = 512;
// bigring: an RX set of 16384*2KB=32MB > L3 -> leaky DMA (RX reads bypass L3)
#[cfg(feature = "bigring")]
const NUM_RX: usize = 16384;
#[cfg(feature = "bigring")]
const NUM_TX: usize = 512;
const BUF: usize = 2048;
const POOL: usize = NUM_RX + NUM_TX + 512; // headroom for the pipeline rings

// advanced descriptors are 16 bytes — we work with the fields directly by offset
#[repr(C, align(16))]
struct Desc {
    lo: u64,
    hi: u64,
}

pub struct Ixgbe {
    base: *mut u8,
    rx: *mut Desc,
    tx: *mut Desc,
    // the buffer pool; descriptors refer to a buffer by index
    buf_virt: [*mut u8; POOL],
    buf_phys: [usize; POOL],
    free: Vec<usize>,
    rx_bidx: [usize; NUM_RX],
    tx_bidx: [usize; NUM_TX],
    rx_index: usize,
    tx_index: usize,
    tx_clean: usize,
}

impl Ixgbe {
    unsafe fn r(&self, off: usize) -> u32 {
        std::ptr::read_volatile(self.base.add(off) as *const u32)
    }
    unsafe fn w(&self, off: usize, v: u32) {
        std::ptr::write_volatile(self.base.add(off) as *mut u32, v);
    }
    unsafe fn set(&self, off: usize, f: u32) { self.w(off, self.r(off) | f); }
    unsafe fn clr(&self, off: usize, f: u32) { self.w(off, self.r(off) & !f); }
    unsafe fn wait_set(&self, off: usize, f: u32) {
        while self.r(off) & f != f { std::hint::spin_loop(); }
    }
    unsafe fn wait_clr(&self, off: usize, f: u32) {
        while self.r(off) & f != 0 { std::hint::spin_loop(); }
    }

    pub fn init(bar: &Bar, mem: &mut HugeRegion) -> Self {
        let base = bar.ptr;
        // lay the rings and buffers out in the hugepage
        let (rx_v, rx_p) = mem.bump(NUM_RX * 16, 4096);
        let (tx_v, tx_p) = mem.bump(NUM_TX * 16, 4096);
        let mut dev = Ixgbe {
            base,
            rx: rx_v as *mut Desc,
            tx: tx_v as *mut Desc,
            buf_virt: [std::ptr::null_mut(); POOL],
            buf_phys: [0; POOL],
            free: Vec::with_capacity(POOL),
            rx_bidx: [0; NUM_RX],
            tx_bidx: [usize::MAX; NUM_TX],
            rx_index: 0,
            tx_index: 0,
            tx_clean: 0,
        };
        unsafe {
            std::ptr::write_bytes(rx_v, 0xff, NUM_RX * 16);
            std::ptr::write_bytes(tx_v, 0xff, NUM_TX * 16);
        }
        // buffer pool: the first NUM_RX are pinned to the rx descriptors, the rest is the free list
        for b in 0..POOL {
            let (v, p) = mem.bump(BUF, 64);
            dev.buf_virt[b] = v; dev.buf_phys[b] = p;
        }
        for i in 0..NUM_RX { dev.rx_bidx[i] = i; }
        for b in NUM_RX..POOL { dev.free.push(b); }

        unsafe { dev.reset_and_init(rx_p, tx_p); }
        dev
    }

    unsafe fn reset_and_init(&mut self, rx_phys: usize, tx_phys: usize) {
        // 4.6.3.1 — disable interrupts
        self.w(EIMC, 0x7FFF_FFFF);
        // 4.6.3.2 — global reset
        self.w(CTRL, CTRL_RST_MASK);
        self.wait_clr(CTRL, CTRL_RST_MASK);
        std::thread::sleep(Duration::from_millis(10));
        self.w(EIMC, 0x7FFF_FFFF);

        // 4.6.3 — EEPROM auto-read + DMA init done
        self.wait_set(EEC, EEC_ARD);
        self.wait_set(RDRXCTL, RDRXCTL_DMAIDONE);

        // 4.6.4 — link (autoneg 10G serial)
        self.w(AUTOC, (self.r(AUTOC) & !AUTOC_LMS_MASK) | AUTOC_LMS_10G_SERIAL);
        self.w(AUTOC, self.r(AUTOC) & !AUTOC_10G_PMA_PMD_MASK);
        self.set(AUTOC, AUTOC_AN_RESTART);

        // 4.6.7 — RX
        self.clr(RXCTRL, RXCTRL_RXEN);
        self.w(rxpbsize(0), RXPBSIZE_128KB);
        for i in 1..8 { self.w(rxpbsize(i), 0); }
        self.set(HLREG0, HLREG0_RXCRCSTRP);
        self.set(RDRXCTL, RDRXCTL_CRCSTRIP);
        let rdr = self.r(RDRXCTL);
        self.w(RDRXCTL, (rdr & !RDRXCTL_RSCFRSTSIZE) | RDRXCTL_RSCACKC | RDRXCTL_FCOE_WRFIX);
        self.set(FCTRL, FCTRL_BAM);
        // queue 0
        self.w(srrctl(0), (self.r(srrctl(0)) & !SRRCTL_DESCTYPE_MASK) | SRRCTL_DESCTYPE_ADV_ONEBUF);
        self.set(srrctl(0), SRRCTL_DROP_EN);
        self.w(rdbal(0), (rx_phys & 0xffff_ffff) as u32);
        self.w(rdbah(0), (rx_phys >> 32) as u32);
        self.w(rdlen(0), (NUM_RX * 16) as u32);
        self.w(rdh(0), 0);
        self.w(rdt(0), 0);
        self.set(CTRL_EXT, CTRL_EXT_NS_DIS);
        self.clr(dca_rxctrl(0), 1 << 12);
        // promisc
        self.set(FCTRL, FCTRL_MPE | FCTRL_UPE);
        self.set(RXCTRL, RXCTRL_RXEN);

        // 4.6.8 — TX
        self.set(HLREG0, HLREG0_TXCRCEN | HLREG0_TXPADEN);
        self.w(txpbsize(0), TXPBSIZE_40KB);
        for i in 1..8 { self.w(txpbsize(i), 0); }
        self.w(DTXMXSZRQ, 0xfff);
        self.clr(RTTDCS, RTTDCS_ARBDIS);
        self.w(tdbal(0), (tx_phys & 0xffff_ffff) as u32);
        self.w(tdbah(0), (tx_phys >> 32) as u32);
        self.w(tdlen(0), (NUM_TX * 16) as u32);
        let mut td = self.r(txdctl(0));
        td &= !(0x7F | (0x7F << 8) | (0x7F << 16));
        td |= 36 | (8 << 8) | (4 << 16);
        self.w(txdctl(0), td);
        self.w(DMATXCTL, DMATXCTL_TE);

        // start rx queue: fill the descriptors with buffers from the pool
        for i in 0..NUM_RX {
            let d = self.rx.add(i);
            std::ptr::write_volatile(&mut (*d).lo, self.buf_phys[self.rx_bidx[i]] as u64);
            std::ptr::write_volatile(&mut (*d).hi, 0);
        }
        self.set(rxdctl(0), RXDCTL_ENABLE);
        self.wait_set(rxdctl(0), RXDCTL_ENABLE);
        self.w(rdh(0), 0);
        self.w(rdt(0), (NUM_RX - 1) as u32);

        // start tx queue
        self.w(tdh(0), 0);
        self.w(tdt(0), 0);
        self.set(txdctl(0), TXDCTL_ENABLE);
        self.wait_set(txdctl(0), TXDCTL_ENABLE);
    }

    pub fn mac(&self) -> [u8; 6] {
        unsafe {
            let l = self.r(ral(0));
            let h = self.r(rah(0));
            [l as u8, (l >> 8) as u8, (l >> 16) as u8, (l >> 24) as u8, h as u8, (h >> 8) as u8]
        }
    }

    pub fn link_up(&self) -> bool {
        unsafe { self.r(LINKS) & LINKS_UP != 0 }
    }

    pub fn wait_link(&self, timeout: Duration) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            if self.link_up() { return true; }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Virtual addresses of all pool buffers (immutable after init) — for the second shard.
    pub fn buf_addrs(&self) -> Vec<usize> {
        self.buf_virt.iter().map(|&p| p as usize).collect()
    }

    /// RX: receive a packet, return (buf_idx, len), refill the RX descriptor with a fresh
    /// buffer. The buffer passes into the caller's (the pipeline's) ownership. None means no
    /// packet OR no free buffer (backpressure, retry later).
    pub fn rx_poll(&mut self) -> Option<(u32, usize)> {
        let i = self.rx_index;
        let d = unsafe { self.rx.add(i) };
        let status = unsafe { std::ptr::read_volatile(&(*d).hi) } as u32;
        if status & RXD_STAT_DD == 0 { return None; }
        unsafe {
            let ni = (i + 1) % NUM_RX;
            core::arch::x86_64::_mm_prefetch(self.rx.add(ni) as *const i8, core::arch::x86_64::_MM_HINT_T0);
            core::arch::x86_64::_mm_prefetch(self.buf_virt[self.rx_bidx[ni]] as *const i8, core::arch::x86_64::_MM_HINT_T0);
        }
        let len = (unsafe { std::ptr::read_volatile(&(*d).hi) } >> 32) as u16 as usize;
        let b = self.rx_bidx[i];
        unsafe { self.clean_tx(); }
        let nb = match self.free.pop() { Some(nb) => nb, None => return None };
        self.rx_bidx[i] = nb;
        unsafe {
            std::ptr::write_volatile(&mut (*d).lo, self.buf_phys[nb] as u64);
            std::ptr::write_volatile(&mut (*d).hi, 0);
            self.w(rdt(0), i as u32);
        }
        self.rx_index = (i + 1) % NUM_RX;
        Some((b as u32, len))
    }

    /// TX: send buffer b (len bytes) as is (zero-copy). The buffer returns to the
    /// free list once the transmission completes (clean_tx).
    pub fn tx_submit(&mut self, b: u32, len: usize) {
        unsafe { self.clean_tx(); }
        let ti = self.tx_index;
        let bb = b as usize;
        unsafe {
            let td = self.tx.add(ti);
            std::ptr::write_volatile(&mut (*td).lo, self.buf_phys[bb] as u64);
            let cmd = TXD_CMD_EOP | TXD_CMD_RS | TXD_CMD_IFCS | TXD_CMD_DEXT
                | ADVTXD_DTYP_DATA | len as u32;
            let olinfo = (len as u32) << ADVTXD_PAYLEN_SHIFT;
            std::ptr::write_volatile(&mut (*td).hi, ((olinfo as u64) << 32) | cmd as u64);
        }
        self.tx_bidx[ti] = bb;
        self.tx_index = (ti + 1) % NUM_TX;
        unsafe { self.w(tdt(0), self.tx_index as u32); }
    }

    /// Return buffer b to the free list without sending it (a dropped packet).
    pub fn free_buf(&mut self, b: u32) { self.free.push(b as usize); }

    /// Take a free buffer from the pool (for stage B's TX replies).
    pub fn alloc_buf(&mut self) -> Option<u32> { self.free.pop().map(|x| x as u32) }

    /// Clean up completed TX descriptors, returning their buffers to the free list.
    unsafe fn clean_tx(&mut self) {
        const DD: u32 = 0x1;
        while self.tx_clean != self.tx_index {
            let d = self.tx.add(self.tx_clean);
            let status = (std::ptr::read_volatile(&(*d).hi) >> 32) as u32;
            if status & DD == 0 { break; }
            let b = self.tx_bidx[self.tx_clean];
            if b != usize::MAX { self.free.push(b); self.tx_bidx[self.tx_clean] = usize::MAX; }
            self.tx_clean = (self.tx_clean + 1) % NUM_TX;
        }
    }

    /// A zero-copy reflector: the buffer with the received packet is handed to the TX ring as is,
    /// and the RX descriptor is refilled with a fresh buffer from the pool. There is no copy.
    pub fn poll_echo<F: FnMut(&mut [u8]) -> Option<usize>>(&mut self, mut f: F) -> bool {
        let i = self.rx_index;
        let d = unsafe { self.rx.add(i) };
        let status = unsafe { std::ptr::read_volatile(&(*d).hi) } as u32;
        if status & RXD_STAT_DD == 0 { return false; }
        // prefetch the next descriptor and its buffer — hide DRAM behind the processing
        unsafe {
            let ni = (i + 1) % NUM_RX;
            core::arch::x86_64::_mm_prefetch(self.rx.add(ni) as *const i8, core::arch::x86_64::_MM_HINT_T0);
            core::arch::x86_64::_mm_prefetch(self.buf_virt[self.rx_bidx[ni]] as *const i8, core::arch::x86_64::_MM_HINT_T0);
        }
        assert!(status & RXD_STAT_EOP != 0, "jumbo/unassembled frame");
        let len = (unsafe { std::ptr::read_volatile(&(*d).hi) } >> 32) as u16 as usize;

        let b = self.rx_bidx[i];
        let out = f(unsafe { std::slice::from_raw_parts_mut(self.buf_virt[b], len) });

        if let Some(tlen) = out {
            unsafe { self.clean_tx(); }
            match self.free.pop() {
                Some(nb) => {
                    // hand buffer b to TX as is
                    let ti = self.tx_index;
                    unsafe {
                        let td = self.tx.add(ti);
                        std::ptr::write_volatile(&mut (*td).lo, self.buf_phys[b] as u64);
                        let cmd = TXD_CMD_EOP | TXD_CMD_RS | TXD_CMD_IFCS | TXD_CMD_DEXT
                            | ADVTXD_DTYP_DATA | tlen as u32;
                        let olinfo = (tlen as u32) << ADVTXD_PAYLEN_SHIFT;
                        std::ptr::write_volatile(&mut (*td).hi, ((olinfo as u64) << 32) | cmd as u64);
                    }
                    self.tx_bidx[ti] = b;
                    self.tx_index = (ti + 1) % NUM_TX;
                    // refill RX with the fresh buffer nb
                    self.rx_bidx[i] = nb;
                    unsafe {
                        std::ptr::write_volatile(&mut (*d).lo, self.buf_phys[nb] as u64);
                        std::ptr::write_volatile(&mut (*d).hi, 0);
                        self.w(tdt(0), self.tx_index as u32);
                        self.w(rdt(0), i as u32);
                    }
                }
                None => {
                    // no free buffer — drop it, the RX descriptor keeps the same buffer
                    unsafe {
                        std::ptr::write_volatile(&mut (*d).lo, self.buf_phys[b] as u64);
                        std::ptr::write_volatile(&mut (*d).hi, 0);
                        self.w(rdt(0), i as u32);
                    }
                }
            }
        } else {
            // drop: return the RX descriptor with the same buffer
            unsafe {
                std::ptr::write_volatile(&mut (*d).lo, self.buf_phys[b] as u64);
                std::ptr::write_volatile(&mut (*d).hi, 0);
                self.w(rdt(0), i as u32);
            }
        }
        self.rx_index = (i + 1) % NUM_RX;
        true
    }

}
