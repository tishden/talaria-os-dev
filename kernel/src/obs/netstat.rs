//! `netstat` — the whole network: the interfaces, the addresses, the routes, the sockets and WHO holds them.
//!
//! ## Why one command rather than five
//!
//! A network question almost never sounds like "show me the routes". It sounds like
//! "why does port 9000 not answer": the NIC came up with the wrong driver, the address
//! arrived on the wrong interface, the default route went into the management NIC,
//! there is no listener at all, or there is one but on another interface. The answer to that is
//! ONE screen where all five layers are visible at once; split across five commands, they
//! require keeping the links between them in one's head.
//!
//! ## What is here that `ip`/`route` do not have
//!
//! * the NIC's counters (frames, bytes, transmit drops) — separately from the counters
//!   of the sockets: "the NIC receives but the connection is silent" and "the NIC does not receive" are
//!   different diagnoses, and the socket's traffic cannot tell them apart;
//! * THE OWNING THREAD of every socket: on a machine with one application the question "who
//!   listens on the port" is the question "which of its threads";
//! * binding a socket to an INTERFACE: `listen(0.0.0.0)` expands here into
//!   a listener on every interface, and knowing which one a connection arrived on
//!   is half the analysis.
use super::{bytes, comm};

/// Printing an address `A.B.C.D:port` (a zero address is `*`).
fn addr(a: [u8; 4], p: u16) -> super::S<24> {
    use core::fmt::Write;
    let mut s = super::S::new();
    if a == [0, 0, 0, 0] { let _ = write!(s, "*:{}", p); }
    else { let _ = write!(s, "{}.{}.{}.{}:{}", a[0], a[1], a[2], a[3], p); }
    s
}
fn owner(tid: u8) -> super::S<24> {
    use core::fmt::Write;
    let mut s = super::S::new();
    if tid == 255 { let _ = write!(s, "kernel"); }
    else { let _ = write!(s, "{}/{}", tid, comm(tid as usize)); }
    s
}

/// The interfaces: the address, the state, the driver, the NIC's counters.
pub fn ifaces() {
    let n = crate::netsys::nifs();
    if n == 0 { crate::println!("netstat: the network stack is not up"); return; }
    crate::println!("INTERFACES");
    crate::println!("  {:<6} {:<18} {:<4} {:<9} {:<17} {:>9} {:>9} {:>9} {:>9} {:>6}",
        "name", "address", "state", "driver", "MAC", "rx frames", "rx bytes", "tx frames", "tx bytes", "drops");
    for i in 0..n {
        let Some(f) = crate::netsys::if_info(i) else { continue };
        let (rn, rb, tn, tb, dr) = crate::netsys::if_counters(i);
        let mut nm = super::S::<8>::new();
        {   use core::fmt::Write;
            for &b in f.name_str() { let _ = write!(nm, "{}", b as char); } }
        let mut ip = super::S::<20>::new();
        {   use core::fmt::Write;
            let _ = write!(ip, "{}.{}.{}.{}/{}", f.ip[0], f.ip[1], f.ip[2], f.ip[3], f.prefix); }
        let mut mac = super::S::<18>::new();
        {   use core::fmt::Write;
            for (k, b) in f.mac.iter().enumerate() { let _ = write!(mac, "{}{:02x}", if k == 0 { "" } else { ":" }, b); } }
        crate::println!("  {:<6} {:<18} {:<4} {:<9} {:<17} {:>9} {:>9} {:>9} {:>9} {:>6}",
            nm, ip, if f.up { "up" } else { "down" }, crate::netsys::if_driver(i), mac,
            rn, bytes(rb), tn, bytes(tb), dr);
        if f.dhcp || f.gw != [0; 4] {
            crate::println!("         gateway {}.{}.{}.{}{}", f.gw[0], f.gw[1], f.gw[2], f.gw[3],
                if f.dhcp { ", the address was obtained by DHCP" } else { "" });
        }
    }
    let d = crate::netsys::cfg_dns();
    crate::println!("  DNS {}.{}.{}.{}", d[0], d[1], d[2], d[3]);
}

/// The sockets: listeners, connections, UDP — with the owner and the interface.
pub fn sockets() {
    if !crate::netsys::ready() { return; }
    crate::println!("");
    crate::println!("SOCKETS");
    crate::println!("  {:<4} {:<5} {:<22} {:<22} {:<10} {:>8} {:>8} {:<4} {}",
        "fd", "proto", "local", "remote", "state", "rx", "tx", "if", "owner");
    let mut any = false;
    // The listeners come first: "who holds the port" is asked most often.
    for pass in [1u8, 3, 2] {
        for i in 0..crate::netsys::NSOCK {
            let Some(r) = crate::netsys::sock_row(i) else { continue };
            if r.kind != pass { continue; }
            any = true;
            crate::println!("  {:<4} {:<5} {:<22} {:<22} {:<10} {:>8} {:>8} net{:<1} {}",
                r.fd, if r.kind == 1 { "tcp*" } else { "tcp" },
                addr([0, 0, 0, 0], r.lport), addr(r.raddr, r.rport),
                if r.kind == 3 { "BACKLOG" } else { r.state },
                bytes(r.rx), bytes(r.tx), r.ifx, owner(r.tid));
        }
    }
    for i in 0..crate::netsys::NUDP {
        let Some((fd, port, ifx, tid)) = crate::netsys::udp_row(i) else { continue };
        any = true;
        crate::println!("  {:<4} {:<5} {:<22} {:<22} {:<10} {:>8} {:>8} net{:<1} {}",
            fd, "udp", addr([0, 0, 0, 0], port), addr([0, 0, 0, 0], 0), "-", "-", "-", ifx, owner(tid));
    }
    if !any { crate::println!("  (there are no open sockets)"); }
}

/// The datapath's profile, if it is on (`talaria.netprof=`): where exactly the time goes.
fn datapath() {
    let (rx, tx, live) = crate::netsys::totals();
    crate::println!("");
    crate::println!("DATAPATH  live connections {}, in total rx {} tx {}", live, bytes(rx), bytes(tx));
    if crate::netprof::on() { crate::netprof::dump(); }
    else { crate::println!("  the stage profile is off — `talaria.netprof=<N>` on the kernel command line"); }
    if crate::telemetry::pkts() != 0 { crate::telemetry::dump(); }
}

/// `netstat [-i|-s|-r|-d]` — without arguments, everything at once.
pub fn cmd(arg: &str) {
    let all = arg.is_empty();
    if all || arg.contains("-i") { ifaces(); }
    if all || arg.contains("-r") {
        crate::println!("");
        crate::println!("ROUTES");
        crate::netcfg::cmd_route("");
    }
    if all || arg.contains("-s") { sockets(); }
    if all || arg.contains("-d") { datapath(); }
}
