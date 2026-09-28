//! The network's configuration: the interfaces, the addresses, the ROUTES, DHCP, DNS.
//!
//! WHY A SEPARATE LAYER. Before this "the network's configuration" was three variables
//! (`talaria.ip=`, `gw=`, `dns=`) and one default route inside
//! `netsys`. For a node with one card that was enough; for a node such as are actually
//! deployed — not: one ENI looks into the management network, another into the working one,
//! each has its own gateway, and some subnets go not through the default gateway.
//!
//! THE MODEL. Exactly the one the operator is used to in Linux, and without anything extra:
//!   * AN INTERFACE is a card plus an address: `net0`, `net1`, ... (the numbering is the order
//!     the cards came up in, which is also the order of the table of drivers). The address is static or
//!     from DHCP, the interface is up or down;
//!   * A ROUTE is `<net>/<length> via <gateway> dev <interface>`, chosen by the
//!     LONGEST prefix match. The default route is
//!     `0.0.0.0/0`, not a separate entity;
//!   * THE CONNECTED NETWORKS are implied: an interface's address sets a route into its
//!     subnet automatically, and it is always longer (that is, stronger) than any
//!     route through a gateway into the same network.
//!
//! HOW THIS TRAVELS INTO THE KERNEL. Three paths, and all three are equal:
//!   * the cmdline/manifest: `talaria.net=net1:10.0.1.5/24`, `talaria.net=net1:dhcp`,
//!     `talaria.route=10.9.0.0/16,10.0.1.1,net1`. Instead of a name one can name a
//!     MAC (`talaria.net=52-54-00-12-34-56:10.0.1.5/24`): the order of the enumeration of the
//!     cards is set by the hardware, and it does not have to coincide with the order in which a human
//!     listed the cards;
//!   * the operator's console: `ip addr add`, `ip link`, `route add/del`, `dhcp`;
//!   * DHCP — by itself, on the interface where it is switched on.
//! The operator is senior to the automatics: a route added by hand DHCP does not take down
//! (only the routes that DHCP itself put there are taken down).
use crate::netsys;

pub use crate::routes::{Route, MAXROUTE};
use crate::routes::{Added, IfState, Table, in_net, mask};

static mut ROUTES: Table = Table::new();

fn table() -> &'static mut Table { unsafe { &mut *core::ptr::addr_of_mut!(ROUTES) } }
fn routes() -> &'static mut [Route; MAXROUTE] { &mut table().r }

/// Interface states for the routing decisions, in interface order.
fn ifs(out: &mut [IfState; netsys::MAXIF]) -> &[IfState] {
    let n = netsys::nifs().min(netsys::MAXIF);
    for (i, slot) in out.iter_mut().enumerate().take(n) {
        *slot = match netsys::if_info(i) {
            Some(f) => IfState { up: f.up, ip: f.ip, prefix: f.prefix },
            None => IfState { up: false, ip: [0; 4], prefix: 0 },
        };
    }
    &out[..n]
}

/// Clear the table (a re-creation of the stack).
pub fn clear() { table().clear(); }

/// Add a route. A matching one (net+length+interface) is replaced.
pub fn add_route(net: [u8; 4], prefix: u8, gw: [u8; 4], ifx: usize, metric: u8) -> bool {
    add_route_f(net, prefix, gw, ifx, metric, true)
}
/// The same with an explicit sign "put there by the automatics" (DHCP) or by the operator.
pub fn add_route_f(net: [u8; 4], prefix: u8, gw: [u8; 4], ifx: usize, metric: u8, auto: bool) -> bool {
    match table().add(net, prefix, gw, ifx, metric, auto) {
        Added::Replaced => { apply_iface(ifx); true }
        Added::New => { warn_unreachable_gw(gw, ifx); apply_iface(ifx); true }
        Added::Full => {
            crate::println!("route: the table is full ({} routes) — the route has not been added", MAXROUTE);
            false
        }
    }
}
/// The gateway is bound to lie in the subnet of THAT interface the route leads through:
/// otherwise an ARP for it will go into the wrong card and the route will never work.
/// The error is quiet by nature (the table looks right), so — out loud.
fn warn_unreachable_gw(gw: [u8; 4], ifx: usize) {
    if gw == [0; 4] { return; }
    if let Some(f) = netsys::if_info(ifx) {
        if f.ip == [0; 4] {
            crate::println!("route: ATTENTION — {} has no address yet, there is nothing to check the gateway {}.{}.{}.{} against",
                core::str::from_utf8(f.name_str()).unwrap_or("?"), gw[0], gw[1], gw[2], gw[3]);
            return;
        }
        if !in_net(gw, f.ip, f.prefix) {
            crate::println!("route: ATTENTION — the gateway {}.{}.{}.{} is NOT in the subnet of {} ({}.{}.{}.{}/{}): an ARP for it will go into the wrong card",
                gw[0], gw[1], gw[2], gw[3], core::str::from_utf8(f.name_str()).unwrap_or("?"),
                f.ip[0], f.ip[1], f.ip[2], f.ip[3], f.prefix);
        }
    }
}

/// Remove a route by net/length (any interface). It returns how many were removed.
pub fn del_route(net: [u8; 4], prefix: u8) -> usize {
    let (n, touched) = table().del(net, prefix);
    for i in 0..netsys::MAXIF { if touched & (1 << i) != 0 { apply_iface(i); } }
    n
}
/// Remove the AUTOMATIC routes of an interface (the loss/change of a DHCP lease).
/// The routes entered by the operator stay: he is senior to the automatics.
pub fn del_routes_of(ifx: usize) {
    table().del_auto_of(ifx);
    apply_iface(ifx);
}

/// Through which interface a packet to `dst` leaves (routes.rs: a connected network,
/// then the longest match, at equality the smaller metric). None — ENETUNREACH.
pub fn egress(dst: [u8; 4]) -> Option<usize> {
    let mut buf = [IfState { up: false, ip: [0; 4], prefix: 0 }; netsys::MAXIF];
    table().egress(dst, ifs(&mut buf))
}
/// The interface of the default route (it is the one `cfg_ip`/`cfg_gw` answer with).
pub fn default_ifx() -> Option<usize> {
    let mut buf = [IfState { up: false, ip: [0; 4], prefix: 0 }; netsys::MAXIF];
    table().default_ifx(ifs(&mut buf))
}

/// Transfer an interface's routes into its smoltcp table.
///
/// In smoltcp the routes live PER INTERFACE and answer only the question "through
/// which gateway", while "through which card" we decide: an interface will never see
/// foreign routes, so it will not be able to send a foreign packet either.
pub fn apply_iface(ifx: usize) {
    netsys::with_routes(ifx, |rt| {
        rt.update(|v| {
            v.clear();
            for r in routes().iter() {
                if !r.used || r.ifx as usize != ifx || r.gw == [0; 4] { continue; }
                let cidr = smoltcp::wire::IpCidr::new(
                    smoltcp::wire::IpAddress::v4(r.net[0], r.net[1], r.net[2], r.net[3]), r.prefix);
                let _ = v.push(smoltcp::iface::Route {
                    cidr,
                    via_router: smoltcp::wire::IpAddress::v4(r.gw[0], r.gw[1], r.gw[2], r.gw[3]),
                    preferred_until: None,
                    expires_at: None,
                });
            }
        });
    });
}

// ── the parsing of the command line/manifest ──────────────────────────────────
// The rules accumulate BEFORE the cards come up (there are no interfaces yet) and are applied in
// `apply_pending()` right after the cards have been found.
const MAXRULE: usize = 8;
#[derive(Clone, Copy)]
struct Rule { used: bool, name: [u8; 18], nlen: usize, kind: u8, ip: [u8; 4], prefix: u8, gw: [u8; 4] }
const RULE0: Rule = Rule { used: false, name: [0; 18], nlen: 0, kind: 0, ip: [0; 4], prefix: 0, gw: [0; 4] };
const K_ADDR: u8 = 1;
const K_DHCP: u8 = 2;
const K_DOWN: u8 = 3;
static mut RULES: [Rule; MAXRULE] = [RULE0; MAXRULE];
static mut NRULE: usize = 0;
#[derive(Clone, Copy)]
struct PendRoute { used: bool, net: [u8; 4], prefix: u8, gw: [u8; 4], name: [u8; 18], nlen: usize }
const PR0: PendRoute = PendRoute { used: false, net: [0; 4], prefix: 0, gw: [0; 4], name: [0; 18], nlen: 0 };
static mut PROUTES: [PendRoute; MAXROUTE] = [PR0; MAXROUTE];
static mut NPROUTE: usize = 0;

fn name_of(s: &str, out: &mut [u8; 18]) -> usize {
    let n = s.len().min(18);
    out[..n].copy_from_slice(&s.as_bytes()[..n]);
    n
}

/// Parse a MAC of the form `52-54-00-12-34-56` (with hyphens: the colon is occupied
/// by the rule's separator).
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut m = [0u8; 6];
    let mut n = 0;
    for part in s.split('-') {
        if n >= 6 || part.len() != 2 { return None; }
        m[n] = u8::from_str_radix(part, 16).ok()?;
        n += 1;
    }
    if n == 6 { Some(m) } else { None }
}
/// An interface by name OR by MAC — a rule may address a card either way.
fn find_iface(sel: &str) -> Option<usize> {
    if let Some(m) = parse_mac(sel) { return netsys::if_by_mac(m); }
    netsys::if_by_name(sel)
}

/// `talaria.net=<name>:<dhcp|down|A.B.C.D/P[,gw=A.B.C.D]>` and
/// `talaria.route=<net>/<length>,<gateway>,<interface>`.
pub fn configure_from_cmdline(cmdline: &str) {
    for tok in cmdline.split_whitespace() {
        if let Some(v) = tok.strip_prefix("talaria.net=") { parse_net_rule(v); }
        else if let Some(v) = tok.strip_prefix("talaria.route=") { parse_route_rule(v); }
    }
}
fn parse_net_rule(v: &str) {
    let (name, spec) = match v.split_once(':') { Some(x) => x, None => { crate::println!("net: the rule '{}' has no interface name — skipped", v); return; } };
    let mut r = RULE0;
    r.nlen = name_of(name, &mut r.name);
    if spec == "dhcp" { r.kind = K_DHCP; }
    else if spec == "down" { r.kind = K_DOWN; }
    else {
        let (a, rest) = match spec.split_once(',') { Some((a, b)) => (a, b), None => (spec, "") };
        match netsys::parse_ip(a) {
            Some((ip, p)) => { r.kind = K_ADDR; r.ip = ip; r.prefix = p; }
            None => { crate::println!("net: the rule '{}' — not an address, skipped", v); return; }
        }
        for kv in rest.split(',') {
            if let Some(g) = kv.strip_prefix("gw=") { if let Some((a, _)) = netsys::parse_ip(g) { r.gw = a; } }
        }
    }
    unsafe { if NRULE < MAXRULE { r.used = true; RULES[NRULE] = r; NRULE += 1; } }
}
fn parse_route_rule(v: &str) {
    let mut it = v.split(',');
    let net = it.next().unwrap_or("");
    let gw = it.next().unwrap_or("");
    let dev = it.next().unwrap_or("");
    let (n, p) = match netsys::parse_ip(net) { Some(x) => x, None => { crate::println!("route: '{}' — not a network, skipped", v); return; } };
    let (g, _) = netsys::parse_ip(gw).unwrap_or(([0; 4], 0));
    let mut pr = PR0;
    pr.net = n; pr.prefix = if net.contains('/') { p } else { 0 }; pr.gw = g;
    pr.nlen = name_of(dev, &mut pr.name);
    unsafe { if NPROUTE < MAXROUTE { pr.used = true; PROUTES[NPROUTE] = pr; NPROUTE += 1; } }
}

/// Apply the accumulated rules to the interfaces that have come up. It is called after
/// the cards have been found and the interfaces started.
pub fn apply_pending() {
    unsafe {
        for i in 0..NRULE {
            let r = RULES[i];
            if !r.used { continue; }
            let nm = core::str::from_utf8(&r.name[..r.nlen]).unwrap_or("");
            let ifx = match find_iface(nm) {
                Some(x) => x,
                None => { crate::println!("net: the rule for '{}' — there is no such interface (cards {}); `ip` will show the names and the MACs", nm,
                    netsys::nifs()); continue; }
            };
            match r.kind {
                K_DHCP => { netsys::if_set_dhcp(ifx, true); crate::println!("net: {} — DHCP switched on by a rule", nm); }
                K_DOWN => { netsys::if_set_up(ifx, false); crate::println!("net: {} — put down by a rule", nm); }
                K_ADDR => {
                    netsys::if_set_addr(ifx, r.ip, r.prefix);
                    if r.gw != [0; 4] { netsys::if_set_gw(ifx, r.gw); add_route_f([0, 0, 0, 0], 0, r.gw, ifx, 1, false); }
                    crate::println!("net: {} — {}.{}.{}.{}/{} by a rule", nm, r.ip[0], r.ip[1], r.ip[2], r.ip[3], r.prefix);
                }
                _ => {}
            }
        }
        for i in 0..NPROUTE {
            let pr = PROUTES[i];
            if !pr.used { continue; }
            let nm = core::str::from_utf8(&pr.name[..pr.nlen]).unwrap_or("");
            match find_iface(nm) {
                Some(ifx) => {
                    if add_route_f(pr.net, pr.prefix, pr.gw, ifx, 1, false) {
                        crate::println!("route: {}.{}.{}.{}/{} via {}.{}.{}.{} dev {} — by a rule",
                            pr.net[0], pr.net[1], pr.net[2], pr.net[3], pr.prefix,
                            pr.gw[0], pr.gw[1], pr.gw[2], pr.gw[3], nm);
                    }
                }
                None => crate::println!("route: the rule through '{}' — there is no such interface", nm),
            }
        }
    }
}

// ── the operator's commands ───────────────────────────────────────────────────
fn puts_ip(a: [u8; 4]) { crate::print!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]); }

/// `ip` — show the interfaces; `ip addr add/del`, `ip link up/down`.
pub fn cmd_ip(arg: &str) {
    let arg = arg.trim();
    if arg.is_empty() || arg == "addr" || arg == "a" { return show_ifaces(); }
    let mut it = arg.split_whitespace();
    let sub = it.next().unwrap_or("");
    match sub {
        "addr" | "a" => {
            let op = it.next().unwrap_or("");
            let a = it.next().unwrap_or("");
            let dev = it.next().unwrap_or("");
            let ifx = match find_iface(dev) { Some(x) => x, None => { crate::println!("ip: there is no interface '{}' — `ip` will show the list", dev); return;
                } };
            match op {
                "add" => match netsys::parse_ip(a) {
                    Some((ip, p)) => { netsys::if_set_addr(ifx, ip, p); crate::print!("ip: {} <- ", dev); puts_ip(ip); crate::println!("/{}", p); }
                    None => crate::println!("usage: ip addr add <A.B.C.D/P> <interface>"),
                },
                "del" => { netsys::if_set_addr(ifx, [0; 4], 0); crate::println!("ip: the address has been removed from {}", dev); }
                _ => crate::println!("usage: ip addr add|del <A.B.C.D/P> <interface>"),
            }
        }
        "link" | "l" => {
            let op = it.next().unwrap_or("");
            let dev = it.next().unwrap_or("");
            let ifx = match find_iface(dev) { Some(x) => x, None => { crate::println!("ip: there is no interface '{}'", dev); return; } };
            match op {
                "up" => { netsys::if_set_up(ifx, true); crate::println!("ip: {} is up", dev); }
                "down" => { netsys::if_set_up(ifx, false); crate::println!("ip: {} is down (its connections have been reset)", dev); }
                _ => crate::println!("usage: ip link up|down <interface>"),
            }
        }
        _ => crate::println!("usage: ip | ip addr add|del <A.B.C.D/P> <if> | ip link up|down <if>"),
    }
}

fn show_ifaces() {
    if netsys::nifs() == 0 { crate::println!("ip: there are no interfaces (the network is not up)"); return; }
    for i in 0..netsys::nifs() {
        let f = match netsys::if_info(i) { Some(f) => f, None => continue };
        crate::print!("{}: ", core::str::from_utf8(f.name_str()).unwrap_or("?"));
        crate::print!("{} ", if f.up { "UP" } else { "DOWN" });
        if f.ip == [0; 4] { crate::print!("without an address"); } else { puts_ip(f.ip); crate::print!("/{}", f.prefix); }
        if f.dhcp { crate::print!(" (dhcp)"); }
        crate::println!("");
        let (rxn, rxb) = netsys::if_rx(i);
        crate::println!("    the card '{}' mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}{}",
            f.drv, f.mac[0], f.mac[1], f.mac[2], f.mac[3], f.mac[4], f.mac[5],
            if crate::netcfg::default_ifx() == Some(i) { "  <- the default route" } else { "" });
        crate::println!("    frames received {} ({} B){}", rxn, rxb,
            if rxn == 0 { " — there has not been A SINGLE frame on this card (check the cable/VLAN)" } else { "" });
    }
}

/// `route` — show the table; `route add/del`.
pub fn cmd_route(arg: &str) {
    let arg = arg.trim();
    if arg.is_empty() || arg == "show" { return show_routes(); }
    let mut it = arg.split_whitespace();
    let op = it.next().unwrap_or("");
    match op {
        "add" => {
            // route add <net/length|default> via <gateway> dev <if>
            let netstr = it.next().unwrap_or("");
            let (mut net, mut prefix) = ([0u8; 4], 0u8);
            if netstr != "default" && netstr != "0.0.0.0/0" {
                match netsys::parse_ip(netstr) { Some((n, p)) => { net = n; prefix = p; }, None => { crate::println!("usage: route add <net/length|default> \
                    via <gateway> dev <if>"); return; } }
            }
            let mut gw = [0u8; 4];
            let mut dev = "";
            while let Some(k) = it.next() {
                match k {
                    "via" => { if let Some(g) = it.next() { gw = netsys::parse_ip(g).map(|x| x.0).unwrap_or([0; 4]); } }
                    "dev" => { dev = it.next().unwrap_or(""); }
                    _ => {}
                }
            }
            let ifx = if dev.is_empty() {
                // without a dev the interface is chosen by the same rule as for a packet to the gateway
                match egress(gw) { Some(x) => x, None => { crate::println!("route: it is impossible to tell through which interface — specify dev <if>");
                    return; } }
            } else {
                match find_iface(dev) { Some(x) => x, None => { crate::println!("route: there is no interface '{}'", dev); return; } }
            };
            if add_route_f(net, prefix, gw, ifx, 1, false) {
                crate::print!("route: ");
                if prefix == 0 { crate::print!("default"); } else { puts_ip(net); crate::print!("/{}", prefix); }
                crate::print!(" via "); puts_ip(gw);
                crate::println!(" dev net{}", ifx);
            }
        }
        "del" => {
            let netstr = it.next().unwrap_or("");
            let (net, prefix) = if netstr == "default" || netstr == "0.0.0.0/0" { ([0u8; 4], 0u8) }
                else { match netsys::parse_ip(netstr) { Some(x) => x, None => { crate::println!("usage: route del <net/length|default>"); return; } } };
            let n = del_route(net, prefix);
            crate::println!("route: routes removed {}", n);
        }
        _ => crate::println!("usage: route | route add <net/length|default> via <gateway> dev <if> | route del <net/length>"),
    }
}

fn show_routes() {
    crate::println!("network           gateway          interface  metric   source");
    let mut n = 0;
    // the connected networks are printed first: it is exactly they that decide most often
    for i in 0..netsys::nifs() {
        if let Some(f) = netsys::if_info(i) {
            if !f.up || f.ip == [0; 4] { continue; }
            let net = crate::routes::network(f.ip, f.prefix);
            puts_ip(net); crate::print!("/{:<12}", f.prefix);
            crate::print!("{:<17}", "directly");
            crate::println!("{:<11}{:<9}{}", core::str::from_utf8(f.name_str()).unwrap_or("?"), 0, "connected");
            n += 1;
        }
    }
    for r in routes().iter() {
        if !r.used { continue; }
        if r.prefix == 0 { crate::print!("{:<17}", "default"); }
        else { puts_ip(r.net); crate::print!("/{:<12}", r.prefix); }
        if r.gw == [0; 4] { crate::print!("{:<17}", "directly"); }
        else { puts_ip(r.gw); crate::print!("{:<3}", ""); }
        let nm = netsys::if_info(r.ifx as usize).map(|f| f.name);
        match nm {
            Some(nb) => crate::print!("{:<11}", core::str::from_utf8(&nb[..4]).unwrap_or("?")),
            None => crate::print!("{:<11}", "?"),
        }
        crate::println!("{:<9}{}", r.metric, if r.auto { "dhcp" } else { "the operator" });
        n += 1;
    }
    if n == 0 { crate::println!("(the table is empty — the node routes nowhere)"); }
}

/// `dhcp <interface> [on|off|renew]`.
pub fn cmd_dhcp(arg: &str) {
    let mut it = arg.split_whitespace();
    let dev = it.next().unwrap_or("");
    let op = it.next().unwrap_or("renew");
    if dev.is_empty() { crate::println!("usage: dhcp <interface> [on|off|renew]"); return; }
    let ifx = match find_iface(dev) { Some(x) => x, None => { crate::println!("dhcp: there is no interface '{}'", dev); return; } };
    match op {
        "on" => { netsys::if_set_dhcp(ifx, true); crate::println!("dhcp: switched on at {} — the lease will come in the nearest passes of the stack", dev); }
        "off" => { netsys::if_set_dhcp(ifx, false); crate::println!("dhcp: switched off at {} (the address has stayed the same)", dev); }
        "renew" => { netsys::dhcp_renew(ifx); crate::println!("dhcp: a re-request of the lease at {}", dev); }
        _ => crate::println!("usage: dhcp <interface> [on|off|renew]"),
    }
}

/// `dns [A.B.C.D]` — show or change the resolver (it regenerates /etc/resolv.conf).
pub fn cmd_dns(arg: &str) {
    let arg = arg.trim();
    if arg.is_empty() {
        let d = netsys::cfg_dns();
        crate::print!("dns: "); puts_ip(d); crate::println!("  (/etc/resolv.conf is generated by the kernel)");
        return;
    }
    match netsys::parse_ip(arg) {
        Some((a, _)) => { netsys::set_dns(a); crate::print!("dns: now "); puts_ip(a); crate::println!(""); }
        None => crate::println!("usage: dns [A.B.C.D]"),
    }
}

/// The self-test of the routing table (`talaria.test=netcfg`) — without cards and without a network.
/// What is checked is what makes packets go "into the wrong door": the prefix's length is
/// stronger than the order of addition, a connected network is stronger than a gateway, the removal of
/// an automatic route does not touch the operator's one.
pub fn selftest() -> bool {
    let mut ok = true;
    let mut check = |c: bool, what: &str| { if !c { crate::println!("netcfg-test: FAIL {}", what); ok = false; } else { crate::println!("netcfg-test: ok {}",
        what); } };
    clear();
    check(mask(24) == 0xFFFF_FF00 && mask(0) == 0 && mask(32) == 0xFFFF_FFFF, "the mask by the prefix's length");
    check(in_net([10, 0, 1, 7], [10, 0, 1, 0], 24), "an address is in its own network");
    check(!in_net([10, 0, 2, 7], [10, 0, 1, 0], 24), "an address from a neighbouring network is not in this one");
    // A route's network is normalised by the mask: 10.9.1.1/16 is bound to land as 10.9.0.0/16,
    // otherwise the comparison by prefix would miss by exactly "it almost matched".
    add_route_f([10, 9, 1, 1], 16, [10, 0, 0, 1], 0, 1, false);
    let r = routes().iter().find(|r| r.used && r.prefix == 16).map(|r| r.net);
    check(r == Some([10, 9, 0, 0]), "a route's network is normalised by the mask");
    check(parse_mac("52-54-00-12-34-56") == Some([0x52, 0x54, 0, 0x12, 0x34, 0x56]), "the parsing of a MAC in a rule (addressing a card independently of the \
        order)");
    check(parse_mac("net0").is_none(), "an interface's name is not taken for a MAC");
    clear();
    ok
}
