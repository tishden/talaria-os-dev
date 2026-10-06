//! A loopback NIC: a `NetDev` without hardware — everything sent to transmit comes back
//! as a receive.
//!
//! WHY IT IS IN THE KERNEL. The claim "a new NIC is plugged in with one file and one
//! line in a table" must be PROVED rather than declared. This driver is
//! plugged in exactly that way and no other, and the gate `talaria.test=nicdev` stands on it:
//! if the `NetDev` trait turns out to be insufficient (and the shared `NicQ` used to hold
//! things a NIC may not have at all — virtqueue tokens, a virtio
//! header before the frame), that will come out here rather than on live hardware, which
//! the local regression cannot reach.
//!
//! A side benefit: running the datapath without QEMU networking, a tap and a stand neighbour.
//!
//! It is not picked up by itself (`Bind::OnDemand`) — only by `talaria.nic=loop`.

use crate::netdev::{Bind, Driver, NetDev, FRAME, MAXTX};
use crate::pci::PciDev;

/// How many frames we keep "on the wire". More are not needed: the loop returns a frame
/// in the same pass.
const NRING: usize = 64;

pub struct LoopNic {
    mac: [u8; 6],
    rx_addr: [u64; NRING],    // a receive slot -> the address of the posted buffer
    rx_ready: [bool; NRING],  // the buffer is posted and free for a frame
    done: [(u16, u16); NRING], // ready frames: (slot, length)
    rd: usize,
    wr: usize,
    tx_busy: [bool; MAXTX],
    next_rx: usize,
    doorbells: u64,
    pending: bool,
    /// Frames dropped: no receive buffers were left. We count rather than stay silent —
    /// "the loop quietly swallowed a frame" would look like a defect of the stack above.
    pub dropped: u64,
    pub looped: u64,
}

impl NetDev for LoopNic {
    fn name(&self) -> &'static str { "loop" }
    fn mac(&self) -> [u8; 6] { self.mac }
    fn rx_capacity(&self, want: usize) -> usize { want.min(NRING) }

    fn rx_post(&mut self, slot: usize, addr: u64) {
        if slot >= NRING { return; }
        self.rx_addr[slot] = addr;
        self.rx_ready[slot] = true;
    }

    fn rx_poll(&mut self) -> Option<(usize, usize, usize)> {
        if self.rd == self.wr { return None; }
        let (slot, len) = self.done[self.rd % NRING];
        self.rd = self.rd.wrapping_add(1);
        Some((slot as usize, 0, len as usize))
    }

    fn tx_alloc(&mut self, ntx: usize) -> Option<usize> {
        self.tx_busy[..ntx.min(MAXTX)].iter().position(|&b| !b)
    }

    fn tx_post(&mut self, slot: usize, addr: u64, n: usize) {
        if slot < MAXTX { self.tx_busy[slot] = true; }
        self.pending = true;
        // find a posted receive buffer (a round-robin walk — otherwise the very first slot
        // would hold the whole loop)
        let mut found = None;
        for k in 0..NRING {
            let i = (self.next_rx + k) % NRING;
            if self.rx_ready[i] { found = Some(i); break; }
        }
        let dst = match found {
            Some(i) => { self.next_rx = (i + 1) % NRING; self.rx_ready[i] = false; i }
            None => { self.dropped += 1; return; }
        };
        let n = n.min(FRAME);
        unsafe {
            core::ptr::copy_nonoverlapping(crate::mem::vptr::<u8>(addr), crate::mem::vptr::<u8>(self.rx_addr[dst]), n);
        }
        self.done[self.wr % NRING] = (dst as u16, n as u16);
        self.wr = self.wr.wrapping_add(1);
        self.looped += 1;
    }

    /// In a loop a transmit completes instantly: the slot is free at once.
    fn tx_reap(&mut self) {
        for b in self.tx_busy.iter_mut() { *b = false; }
    }

    fn flush(&mut self) -> bool {
        self.doorbells += 1;
        let had = self.pending;
        self.pending = false;
        had
    }

    fn doorbells(&self) -> Option<u64> { Some(self.doorbells) }
}

// ── binding ─────────────────────────────────────────────────────────────────
static mut SLOT: Option<LoopNic> = None;

fn probe(_: Option<&PciDev>, inst: usize) -> Option<&'static mut dyn NetDev> {
    if inst != 0 { return None; } // there is exactly one loopback NIC
    let nic = LoopNic {
        // a locally administered address (bit 1 of the first byte): it never leaves
        // for the network and cannot clash with a real NIC
        mac: [0x02, 0x54, 0x41, 0x4c, 0x00, 0x01],
        rx_addr: [0; NRING], rx_ready: [false; NRING],
        done: [(0, 0); NRING], rd: 0, wr: 0,
        tx_busy: [false; MAXTX], next_rx: 0,
        doorbells: 0, pending: false, dropped: 0, looped: 0,
    };
    unsafe {
        let p = core::ptr::addr_of_mut!(SLOT);
        core::ptr::write(p, Some(nic));
        Some((*p).as_mut().unwrap() as &mut dyn NetDev)
    }
}

pub static DRIVER: Driver = Driver {
    name: "loop",
    bind: &[Bind::OnDemand],
    probe,
};

/// The loop's counters (for the gate): (looped back, dropped).
pub fn counters() -> (u64, u64) {
    unsafe {
        match (*core::ptr::addr_of!(SLOT)).as_ref() {
            Some(n) => (n.looped, n.dropped),
            None => (0, 0),
        }
    }
}
