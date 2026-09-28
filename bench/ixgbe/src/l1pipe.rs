//! Stage B of the pipeline on a full smoltcp L1 stack. The Device works on top of
//! the SPSC rings: RX packets arrive from a2b (a buffer of the 1G arena, zero-copy reads),
//! TX replies are written into a buffer from txbuf and leave through b2a on stage A's TX.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage};
use smoltcp::phy::{Checksum, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::udp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

use crate::spsc::Spsc;

type R = Arc<Spsc>;
type B = Arc<Vec<usize>>;

pub struct RingDev { pub a2b: R, pub b2a: R, pub txbuf: R, pub bufs: B }

pub struct RxTok { b2a: R, bufs: B, b: u32, len: usize }
pub struct TxTok { b2a: R, txbuf: R, bufs: B }

impl RxToken for RxTok {
    fn consume<Rr, F: FnOnce(&[u8]) -> Rr>(self, f: F) -> Rr {
        let ptr = self.bufs[self.b as usize] as *const u8;
        let r = f(unsafe { std::slice::from_raw_parts(ptr, self.len) });
        while !self.b2a.push(self.b as u64) { std::hint::spin_loop(); } // len=0 -> free
        r
    }
}
impl TxToken for TxTok {
    fn consume<Rr, F: FnOnce(&mut [u8]) -> Rr>(self, len: usize, f: F) -> Rr {
        // take a reply buffer from txbuf (stage A refills it)
        let tb = loop {
            if let Some(v) = self.txbuf.pop() { break (v & 0xffff_ffff) as u32; }
            std::hint::spin_loop();
        };
        let ptr = self.bufs[tb as usize] as *mut u8;
        let r = f(unsafe { std::slice::from_raw_parts_mut(ptr, len) });
        while !self.b2a.push(((len as u64) << 32) | tb as u64) { std::hint::spin_loop(); } // TX
        r
    }
}

impl Device for RingDev {
    type RxToken<'a> = RxTok where Self: 'a;
    type TxToken<'a> = TxTok where Self: 'a;
    fn receive(&mut self, _t: Instant) -> Option<(RxTok, TxTok)> {
        let v = self.a2b.pop()?;
        let b = (v & 0xffff_ffff) as u32;
        let len = (v >> 32) as usize;
        Some((RxTok { b2a: self.b2a.clone(), bufs: self.bufs.clone(), b, len },
              TxTok { b2a: self.b2a.clone(), txbuf: self.txbuf.clone(), bufs: self.bufs.clone() }))
    }
    fn transmit(&mut self, _t: Instant) -> Option<TxTok> {
        Some(TxTok { b2a: self.b2a.clone(), txbuf: self.txbuf.clone(), bufs: self.bufs.clone() })
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.max_transmission_unit = 1514;
        c.max_burst_size = Some(1);
        c.medium = Medium::Ethernet;
        c.checksum.udp = Checksum::None;
        c
    }
}

fn rdtsc() -> u64 { unsafe { std::arch::x86_64::_rdtsc() } }
fn now(hz: u64) -> Instant {
    let t = rdtsc();
    Instant::from_micros(((t / hz) * 1_000_000 + (t % hz) * 1_000_000 / hz) as i64)
}

/// The stage B reactor: smoltcp UDP echo :7777 on top of the rings. Spins until stop.
pub fn run(a2b: R, b2a: R, txbuf: R, bufs: B, mac: [u8;6], hz: u64, stop: Arc<AtomicBool>) {
    let mut dev = RingDev { a2b, b2a, txbuf, bufs };
    let cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    let mut iface = Interface::new(cfg, &mut dev, now(hz));
    iface.update_ip_addrs(|a| { a.push(IpCidr::new(IpAddress::v4(10,0,0,2), 24)).unwrap(); });

    let mut storage = [SocketStorage::EMPTY; 2];
    let mut sockets = SocketSet::new(&mut storage[..]);
    let mut rx_meta = [udp::PacketMetadata::EMPTY; 32];
    let mut tx_meta = [udp::PacketMetadata::EMPTY; 32];
    let mut rx_pl = vec![0u8; 65536];
    let mut tx_pl = vec![0u8; 65536];
    let sock = udp::Socket::new(
        udp::PacketBuffer::new(&mut rx_meta[..], &mut rx_pl[..]),
        udp::PacketBuffer::new(&mut tx_meta[..], &mut tx_pl[..]));
    let handle = sockets.add(sock);
    sockets.get_mut::<udp::Socket>(handle).bind(7777).unwrap();

    let mut echo = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        let t = now(hz);
        iface.poll(t, &mut dev, &mut sockets);
        let s = sockets.get_mut::<udp::Socket>(handle);
        while let Ok((data, meta)) = s.recv() {
            let n = data.len().min(echo.len());
            echo[..n].copy_from_slice(&data[..n]);
            let _ = s.send_slice(&echo[..n], meta);
        }
        iface.poll(t, &mut dev, &mut sockets);
    }
}
