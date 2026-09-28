//! The routing table: which interface a packet leaves through.
//!
//! Longest prefix wins; on equal prefixes the lower metric wins; a directly
//! connected network (an interface's own address and prefix) counts as a route
//! with metric 0. Routes put in place by DHCP are marked `auto` and are the
//! only ones DHCP removes — a route the operator added outlives lease changes.
//!
//! Pure state and decisions; `netcfg.rs` owns the live table, feeds it the
//! interface states from `netsys` and pushes the result into smoltcp.

pub const MAXROUTE: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Route {
    pub used: bool,
    pub net: [u8; 4],
    pub prefix: u8,
    /// 0.0.0.0 — directly reachable (on-link).
    pub gw: [u8; 4],
    pub ifx: u8,
    pub metric: u8,
    /// Installed by DHCP (and therefore removed by it).
    pub auto: bool,
}
pub const R0: Route = Route { used: false, net: [0; 4], prefix: 0, gw: [0; 4], ifx: 0, metric: 0, auto: false };

/// What `egress` needs to know about an interface.
#[derive(Clone, Copy, Debug)]
pub struct IfState { pub up: bool, pub ip: [u8; 4], pub prefix: u8 }

pub fn mask(prefix: u8) -> u32 { if prefix == 0 { 0 } else { !0u32 << (32 - prefix.min(32)) } }
pub fn in_net(addr: [u8; 4], net: [u8; 4], prefix: u8) -> bool {
    (u32::from_be_bytes(addr) & mask(prefix)) == (u32::from_be_bytes(net) & mask(prefix))
}
/// The network part of `addr` (host bits cleared).
pub fn network(addr: [u8; 4], prefix: u8) -> [u8; 4] {
    (u32::from_be_bytes(addr) & mask(prefix)).to_be_bytes()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Added { New, Replaced, Full }

#[derive(Clone, Copy)]
pub struct Table { pub r: [Route; MAXROUTE] }

impl Default for Table {
    fn default() -> Self { Self::new() }
}

impl Table {
    pub const fn new() -> Self { Table { r: [R0; MAXROUTE] } }
    pub fn clear(&mut self) { self.r = [R0; MAXROUTE]; }

    /// Add a route. A route with the same network, prefix and interface is
    /// replaced (gateway, metric and origin updated).
    pub fn add(&mut self, net: [u8; 4], prefix: u8, gw: [u8; 4], ifx: usize, metric: u8, auto: bool) -> Added {
        let net = network(net, prefix);
        if let Some(r) = self.r.iter_mut().find(|r| r.used && r.prefix == prefix && r.net == net && r.ifx == ifx as u8) {
            r.gw = gw; r.metric = metric; r.auto = auto;
            return Added::Replaced;
        }
        match self.r.iter_mut().find(|r| !r.used) {
            Some(r) => { *r = Route { used: true, net, prefix, gw, ifx: ifx as u8, metric, auto }; Added::New }
            None => Added::Full,
        }
    }

    /// Remove routes to `net`/`prefix` on any interface. Returns how many were
    /// removed and a bit mask of the interfaces they belonged to.
    pub fn del(&mut self, net: [u8; 4], prefix: u8) -> (usize, u64) {
        let (mut n, mut touched) = (0, 0u64);
        for r in self.r.iter_mut() {
            if r.used && r.prefix == prefix && in_net(net, r.net, prefix) {
                if r.ifx < 64 { touched |= 1 << r.ifx; }
                *r = R0; n += 1;
            }
        }
        (n, touched)
    }

    /// Remove the AUTOMATIC routes of an interface (DHCP lease lost or changed).
    pub fn del_auto_of(&mut self, ifx: usize) {
        for r in self.r.iter_mut() { if r.used && r.ifx as usize == ifx && r.auto { *r = R0; } }
    }

    /// Interface for a packet to `dst`: a connected network first (longest
    /// prefix), then the table (longest prefix, lower metric on a tie).
    /// `None` — ENETUNREACH. Down interfaces are ignored.
    pub fn egress(&self, dst: [u8; 4], ifs: &[IfState]) -> Option<usize> {
        let up = |i: usize| ifs.get(i).is_some_and(|f| f.up);
        let mut best: Option<(u8, u8, usize)> = None; // (prefix, metric, interface)
        for (i, f) in ifs.iter().enumerate() {
            if !f.up || f.ip == [0; 4] || !in_net(dst, f.ip, f.prefix) { continue; }
            if best.is_none_or(|(p, _, _)| f.prefix > p) { best = Some((f.prefix, 0, i)); }
        }
        for r in self.r.iter().filter(|r| r.used && up(r.ifx as usize) && in_net(dst, r.net, r.prefix)) {
            let better = best.is_none_or(|(p, m, _)| r.prefix > p || (r.prefix == p && r.metric < m));
            if better { best = Some((r.prefix, r.metric, r.ifx as usize)); }
        }
        best.map(|(_, _, i)| i)
    }

    /// Interface of the default route (lowest metric among up interfaces),
    /// else the first interface that is up.
    pub fn default_ifx(&self, ifs: &[IfState]) -> Option<usize> {
        let up = |i: usize| ifs.get(i).is_some_and(|f| f.up);
        let mut best: Option<(u8, usize)> = None;
        for r in self.r.iter().filter(|r| r.used && r.prefix == 0 && up(r.ifx as usize)) {
            if best.is_none_or(|(m, _)| r.metric < m) { best = Some((r.metric, r.ifx as usize)); }
        }
        best.map(|(_, i)| i).or_else(|| ifs.iter().position(|f| f.up))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NET0: IfState = IfState { up: true, ip: [10, 0, 0, 2], prefix: 24 };
    const NET1: IfState = IfState { up: true, ip: [192, 168, 1, 5], prefix: 24 };

    #[test]
    fn masks_and_networks() {
        assert_eq!(mask(0), 0);
        assert_eq!(mask(24), 0xFFFF_FF00);
        assert_eq!(mask(32), 0xFFFF_FFFF);
        assert_eq!(mask(40), 0xFFFF_FFFF, "prefixes above 32 are clamped");
        assert_eq!(network([10, 1, 2, 3], 16), [10, 1, 0, 0]);
        assert!(in_net([10, 0, 0, 77], [10, 0, 0, 0], 24));
        assert!(!in_net([10, 0, 1, 77], [10, 0, 0, 0], 24));
        assert!(in_net([1, 2, 3, 4], [9, 9, 9, 9], 0), "prefix 0 matches everything");
    }

    #[test]
    fn connected_network_beats_default_route() {
        let mut t = Table::new();
        t.add([0; 4], 0, [192, 168, 1, 1], 1, 0, true);
        assert_eq!(t.egress([10, 0, 0, 9], &[NET0, NET1]), Some(0));
        assert_eq!(t.egress([8, 8, 8, 8], &[NET0, NET1]), Some(1));
    }

    #[test]
    fn longest_prefix_then_lowest_metric() {
        let mut t = Table::new();
        t.add([0; 4], 0, [10, 0, 0, 1], 0, 0, false);
        t.add([172, 16, 0, 0], 12, [192, 168, 1, 1], 1, 5, false);
        t.add([172, 16, 5, 0], 24, [10, 0, 0, 1], 0, 9, false);
        let ifs = [NET0, NET1];
        assert_eq!(t.egress([172, 16, 5, 1], &ifs), Some(0), "/24 beats /12 despite its metric");
        assert_eq!(t.egress([172, 17, 0, 1], &ifs), Some(1));
        assert_eq!(t.egress([1, 1, 1, 1], &ifs), Some(0));
        t.add([172, 16, 5, 0], 24, [192, 168, 1, 1], 1, 1, false);
        assert_eq!(t.egress([172, 16, 5, 1], &ifs), Some(1), "equal prefix: lower metric");
    }

    #[test]
    fn down_interfaces_are_skipped() {
        let mut t = Table::new();
        t.add([0; 4], 0, [192, 168, 1, 1], 1, 0, false);
        let down1 = IfState { up: false, ..NET1 };
        assert_eq!(t.egress([8, 8, 8, 8], &[NET0, down1]), None);
        assert_eq!(t.egress([192, 168, 1, 9], &[NET0, down1]), None, "its connected net too");
    }

    #[test]
    fn unaddressed_interface_has_no_connected_network() {
        let noaddr = IfState { up: true, ip: [0; 4], prefix: 0 };
        assert_eq!(Table::new().egress([8, 8, 8, 8], &[noaddr]), None);
    }

    #[test]
    fn add_replaces_same_route_and_normalises_network() {
        let mut t = Table::new();
        assert_eq!(t.add([10, 9, 8, 7], 16, [10, 0, 0, 1], 0, 3, true), Added::New);
        assert_eq!(t.r[0].net, [10, 9, 0, 0]);
        assert_eq!(t.add([10, 9, 0, 0], 16, [10, 0, 0, 254], 0, 1, false), Added::Replaced);
        assert_eq!(t.r.iter().filter(|r| r.used).count(), 1);
        assert_eq!((t.r[0].gw, t.r[0].metric, t.r[0].auto), ([10, 0, 0, 254], 1, false));
        assert_eq!(t.add([10, 9, 0, 0], 16, [192, 168, 1, 1], 1, 1, false), Added::New, "other interface: a new route");
    }

    #[test]
    fn table_full_is_reported() {
        let mut t = Table::new();
        for i in 0..MAXROUTE { assert_eq!(t.add([10, i as u8, 0, 0], 16, [0; 4], 0, 0, false), Added::New); }
        assert_eq!(t.add([11, 0, 0, 0], 8, [0; 4], 0, 0, false), Added::Full);
    }

    #[test]
    fn del_removes_on_every_interface_and_reports_them() {
        let mut t = Table::new();
        t.add([10, 9, 0, 0], 16, [0; 4], 0, 0, false);
        t.add([10, 9, 0, 0], 16, [0; 4], 2, 0, false);
        t.add([10, 8, 0, 0], 16, [0; 4], 1, 0, false);
        assert_eq!(t.del([10, 9, 3, 4], 16), (2, 0b101));
        assert_eq!(t.r.iter().filter(|r| r.used).count(), 1);
        assert_eq!(t.del([10, 9, 0, 0], 24), (0, 0), "prefix must match too");
    }

    #[test]
    fn dhcp_removes_only_its_own_routes() {
        let mut t = Table::new();
        t.add([0; 4], 0, [10, 0, 0, 1], 0, 0, true);       // from DHCP
        t.add([172, 16, 0, 0], 12, [10, 0, 0, 1], 0, 0, false); // operator
        t.add([0; 4], 0, [192, 168, 1, 1], 1, 0, true);    // DHCP on another interface
        t.del_auto_of(0);
        let left: Vec<_> = t.r.iter().filter(|r| r.used).map(|r| (r.prefix, r.ifx, r.auto)).collect();
        assert_eq!(left, [(12, 0, false), (0, 1, true)]);
    }

    #[test]
    fn default_interface_by_metric_then_first_up() {
        let mut t = Table::new();
        let ifs = [NET0, NET1];
        assert_eq!(t.default_ifx(&ifs), Some(0), "no default route: the first interface that is up");
        t.add([0; 4], 0, [10, 0, 0, 1], 0, 10, false);
        t.add([0; 4], 0, [192, 168, 1, 1], 1, 5, false);
        assert_eq!(t.default_ifx(&ifs), Some(1));
        assert_eq!(t.default_ifx(&[NET0, IfState { up: false, ..NET1 }]), Some(0));
        assert_eq!(t.default_ifx(&[]), None);
    }
}
