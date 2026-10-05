//! A reference for the wall clock over the network: SNTP (RFC 4330) to one server — on EC2 the
//! Amazon Time Sync Service at 169.254.169.123, reachable from every instance without a route
//! out. The servo (`clock::Servo`) takes any source through `wallclock::feed_ref`; until this
//! module nothing fed it in production except kvmclock on x86 under KVM: a Graviton (no PHC on
//! Nitro's ENA, no paravirtual clock on arm64 KVM) and every metal machine drifted by the quartz's
//! 10-50 ppm from the firmware's whole seconds (docs/ARM64-PLAN.md §4.9, §10.11).
//!
//! The sample. A query carries our own counter value `t1` in its transmit field; a server echoes
//! that field verbatim as the reply's originate one, so a reply names the query it answers and
//! nothing about the outstanding queries is kept. With the server's receive and transmit times
//! `T2`, `T3` and our counter `t4` at the reply, the server's clock read `(T2 + T3) / 2` at our
//! counter's `(t1 + t4) / 2`: the classic offset, written as the pair the servo wants. The error
//! of that pairing is at most half the path's asymmetry, bounded by half the round trip — so a
//! poll is a BURST of queries and only the one with the shortest round trip is fed (a queue on
//! the way delays one direction and lengthens the round trip with it).
//!
//! Pure arithmetic here (the host tests drive it); the socket and the schedule are netsys's
//! (`netsys::ntp_process`), which runs in the stack's pass like the DHCP client.

/// Seconds from the NTP era (1900-01-01) to the unix epoch.
pub const NTP_UNIX_DELTA: u64 = 2_208_988_800;
/// The queries in one poll; the shortest round trip of them is the sample.
pub const BURST: u32 = 4;

/// An NTP 32.32 timestamp to unix nanoseconds (0 for a time before 1970: not a clock).
pub fn ntp_to_unix_ns(ts: u64) -> u64 {
    let sec = ts >> 32;
    if sec < NTP_UNIX_DELTA { return 0; }
    let frac = ts & 0xFFFF_FFFF;
    (sec - NTP_UNIX_DELTA) * 1_000_000_000 + ((frac * 1_000_000_000) >> 32)
}

/// A client query (48 bytes): version 4, mode 3; `cookie` in the transmit field.
pub fn request(cookie: u64) -> [u8; 48] {
    let mut p = [0u8; 48];
    p[0] = (4 << 3) | 3;
    p[40..48].copy_from_slice(&cookie.to_be_bytes());
    p
}

/// What a reply says, once it is judged usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    /// Our transmit field, echoed (the query's counter value).
    pub cookie: u64,
    /// The server's receive and transmit times, unix ns.
    pub t2: u64,
    pub t3: u64,
    pub stratum: u8,
}

/// Parse a server's reply. `None` for anything that is not a usable time: a short packet, a mode
/// other than server (4) or broadcast (5), a kiss-of-death (stratum 0) or an unsynchronised server
/// (leap indicator 3), a zero or reversed timestamp pair.
pub fn parse(p: &[u8]) -> Option<Reply> {
    if p.len() < 48 { return None; }
    let (li, mode, stratum) = (p[0] >> 6, p[0] & 7, p[1]);
    if mode != 4 && mode != 5 { return None; }
    if stratum == 0 || stratum >= 16 || li == 3 { return None; }
    let be = |o: usize| u64::from_be_bytes([p[o], p[o + 1], p[o + 2], p[o + 3], p[o + 4], p[o + 5], p[o + 6], p[o + 7]]);
    let (t2, t3) = (ntp_to_unix_ns(be(32)), ntp_to_unix_ns(be(40)));
    if t2 == 0 || t3 < t2 { return None; }
    Some(Reply { cookie: be(24), t2, t3, stratum })
}

/// One reply turned into the servo's pair, with the round trip that bounds its error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// The server's clock at `at_tsc`, unix ns.
    pub ref_ns: u64,
    /// Our counter at the round trip's midpoint.
    pub at_tsc: u64,
    /// The round trip without the server's own hold time, ns.
    pub rtt_ns: u64,
}

/// The sample of a reply received at counter `t4` (the query left at `r.cookie`); `hz` is the
/// counter's frequency. `None` when the counter says the reply came before its query (not ours).
pub fn sample(r: &Reply, t4: u64, hz: u64) -> Option<Sample> {
    let t1 = r.cookie;
    if t4 < t1 || hz == 0 { return None; }
    let d = t4 - t1;
    let hold = r.t3 - r.t2;
    let rtt = crate::clock::cycles_to_ns(d, hz).saturating_sub(hold);
    Some(Sample { ref_ns: r.t2 + hold / 2, at_tsc: t1 + d / 2, rtt_ns: rtt })
}

/// A poll in progress: the queries sent, the replies taken, the best sample so far.
#[derive(Clone, Copy, Debug)]
pub struct Burst {
    pub sent: u32,
    pub got: u32,
    pub best: Option<Sample>,
}

impl Burst {
    pub const fn new() -> Burst { Burst { sent: 0, got: 0, best: None } }
    /// Take a sample; the shortest round trip wins.
    pub fn add(&mut self, s: Sample) {
        self.got += 1;
        if self.best.is_none_or(|b| s.rtt_ns < b.rtt_ns) { self.best = Some(s); }
    }
}

impl Default for Burst {
    fn default() -> Self { Burst::new() }
}

/// `talaria.ntp=` — `aws` (169.254.169.123) or `a.b.c.d`, optionally `,<poll interval s>`
/// (the default 16, chrony's minimum poll). `None` for anything else.
pub fn parse_knob(v: &str) -> Option<([u8; 4], u32)> {
    let (host, iv) = match v.split_once(',') { Some((h, i)) => (h, i.parse::<u32>().ok()?), None => (v, 16) };
    if iv == 0 { return None; }
    if host == "aws" { return Some(([169, 254, 169, 123], iv)); }
    let mut ip = [0u8; 4];
    let mut n = 0;
    for part in host.split('.') {
        if n == 4 { return None; }
        ip[n] = part.parse::<u8>().ok()?;
        n += 1;
    }
    if n != 4 { return None; }
    Some((ip, iv))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HZ: u64 = 1_050_000_000; // a Graviton's counter

    fn ntp_ts(unix_ns: u64) -> u64 {
        let sec = unix_ns / 1_000_000_000 + NTP_UNIX_DELTA;
        let frac = ((unix_ns % 1_000_000_000) << 32) / 1_000_000_000;
        (sec << 32) | frac
    }

    fn reply(cookie: u64, t2: u64, t3: u64) -> [u8; 48] {
        let mut p = [0u8; 48];
        p[0] = (4 << 3) | 4;
        p[1] = 2;
        p[24..32].copy_from_slice(&cookie.to_be_bytes());
        p[32..40].copy_from_slice(&ntp_ts(t2).to_be_bytes());
        p[40..48].copy_from_slice(&ntp_ts(t3).to_be_bytes());
        p
    }

    #[test]
    fn the_era_and_the_fraction() {
        assert_eq!(ntp_to_unix_ns(NTP_UNIX_DELTA << 32), 0);
        assert_eq!(ntp_to_unix_ns(((NTP_UNIX_DELTA + 1) << 32) | 0x8000_0000), 1_500_000_000);
        assert_eq!(ntp_to_unix_ns(5 << 32), 0, "before 1970 is not a clock");
        let t = 1_791_097_426_123_456_789u64;
        assert!(ntp_to_unix_ns(ntp_ts(t)).abs_diff(t) <= 1);
    }

    #[test]
    fn a_query_carries_its_cookie() {
        let q = request(0x1122_3344_5566_7788);
        assert_eq!(q[0], 0x23);
        assert_eq!(&q[40..48], &0x1122_3344_5566_7788u64.to_be_bytes());
    }

    #[test]
    fn a_reply_names_its_query_and_unusable_ones_are_refused() {
        let r = parse(&reply(42, 10_000_000_000, 10_000_020_000)).unwrap();
        assert_eq!(r.cookie, 42);
        assert!(r.t3 - r.t2 >= 19_999 && r.t3 - r.t2 <= 20_001);
        let mut kod = reply(42, 10_000_000_000, 10_000_000_000); kod[1] = 0;
        assert!(parse(&kod).is_none(), "a kiss-of-death is not a time");
        let mut unsync = reply(42, 10_000_000_000, 10_000_000_000); unsync[0] |= 3 << 6;
        assert!(parse(&unsync).is_none(), "an unsynchronised server is not a time");
        let mut client = reply(42, 10_000_000_000, 10_000_000_000); client[0] = 0x23;
        assert!(parse(&client).is_none(), "our own query reflected is not a reply");
        assert!(parse(&reply(42, 10_000_020_000, 10_000_000_000)).is_none(), "a transmit before the receive");
        assert!(parse(&[0u8; 47]).is_none());
    }

    #[test]
    fn the_sample_is_the_midpoint_pair() {
        // Our counter: t1 at 100 s; the path 30 us each way, the server holds 10 us; the
        // server's clock is 2.5 s ahead of ours. The midpoint pair must say exactly that.
        let t1 = 100 * HZ;
        let our_t1_ns = 100_000_000_000u64;
        let off = 2_500_000_000u64;
        let t2 = our_t1_ns + 30_000 + off;
        let t3 = t2 + 10_000;
        let t4 = t1 + 70_000 * HZ / 1_000_000_000; // 70 us later (73500 ticks, exact)
        let s = sample(&parse(&reply(t1, t2, t3)).unwrap(), t4, HZ).unwrap();
        assert!(s.rtt_ns.abs_diff(60_000) <= 2, "rtt {}", s.rtt_ns);
        let our_mid = crate::clock::cycles_to_ns(s.at_tsc, HZ);
        assert!((s.ref_ns - our_mid).abs_diff(off) <= 2, "offset {}", s.ref_ns - our_mid);
        assert!(sample(&parse(&reply(t1, t2, t3)).unwrap(), t1 - 1, HZ).is_none());
    }

    #[test]
    fn a_burst_keeps_the_shortest_round_trip() {
        let mut b = Burst::new();
        for (rtt, at) in [(90_000u64, 1u64), (40_000, 2), (55_000, 3), (40_001, 4)] {
            b.add(Sample { ref_ns: at, at_tsc: at, rtt_ns: rtt });
        }
        assert_eq!(b.got, 4);
        assert_eq!(b.best.unwrap().at_tsc, 2);
    }

    #[test]
    fn the_knob() {
        assert_eq!(parse_knob("aws"), Some(([169, 254, 169, 123], 16)));
        assert_eq!(parse_knob("10.0.0.1,1"), Some(([10, 0, 0, 1], 1)));
        assert_eq!(parse_knob("10.0.0.1,0"), None);
        assert_eq!(parse_knob("10.0.0"), None);
        assert_eq!(parse_knob("10.0.0.1.5"), None);
        assert_eq!(parse_knob("pool.ntp.org"), None);
    }
}
