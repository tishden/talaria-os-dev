//! The trading pipeline: the strategy+risk stage builds an order packet in place in the
//! buffer of the received market data (headers reflected + the order in the payload), and the
//! crypto stage encrypts the payload in place (AES-GCM). The buffer is reused — zero-copy.
pub const ORDER_OFF: usize = 42;   // eth(14)+ip(20)+udp(8)
pub const ORDER_LEN: usize = 48;   // a compact binary order
pub const TAG: usize = 16;
pub const FRAME_LEN: usize = ORDER_OFF + ORDER_LEN + TAG; // 106

/// Stage B (strategy+risk): from the received market data (frame,rx_len) build an
/// order packet and return the resulting frame length (payload = ORDER_LEN+TAG), or None
/// (risk/strategy rejected it). The headers are reflected — the reply goes back to the sender.
pub fn strategy_risk(f: &mut [u8], rx_len: usize, seq: u64) -> Option<usize> {
    if rx_len < ORDER_OFF { return None; }
    // only UDP to port 7777 (market data); anything else (ARP and so on) is not our input
    if u16::from_be_bytes([f[12], f[13]]) != 0x0800 { return None; } // not IPv4
    if f[23] != 17 { return None; }                                  // not UDP
    if u16::from_be_bytes([f[36], f[37]]) != 7777 { return None; }   // not our port
    // strategy: a simple signal — we trade on the parity of the "price" (the first payload byte)
    let signal = f.get(ORDER_OFF).copied().unwrap_or(0);
    // risk: a rate-limit demo — everything passes (a real limit goes by position/frequency)
    let _ = signal;

    // reflect the headers (the reply goes to the sender)
    for i in 0..6 { f.swap(i, 6 + i); }              // eth src<->dst
    for i in 0..4 { f.swap(26 + i, 30 + i); }        // ip src<->dst
    for i in 0..2 { f.swap(34 + i, 36 + i); }        // udp src<->dst port

    // build the (binary) order: magic, seq, side, qty, price, padding
    let o = &mut f[ORDER_OFF..ORDER_OFF + ORDER_LEN];
    o.fill(0);
    o[0..4].copy_from_slice(b"ORD1");
    o[4..12].copy_from_slice(&seq.to_be_bytes());
    o[12] = if signal & 1 == 0 { b'B' } else { b'S' }; // side
    o[13..17].copy_from_slice(&100u32.to_be_bytes());  // qty
    o[17..25].copy_from_slice(&(10000u64 + seq % 100).to_be_bytes()); // price

    // lengths: payload = ORDER_LEN + TAG (the tag is added by the crypto stage)
    let ip_total = (20 + 8 + ORDER_LEN + TAG) as u16;
    f[16..18].copy_from_slice(&ip_total.to_be_bytes());
    let udp_len = (8 + ORDER_LEN + TAG) as u16;
    f[38..40].copy_from_slice(&udp_len.to_be_bytes());
    f[40] = 0; f[41] = 0; // udp checksum = 0 (optional for IPv4)
    fix_ip_checksum(f);
    Some(FRAME_LEN)
}

/// Recompute the IP header checksum (20 bytes from offset 14).
pub fn fix_ip_checksum(f: &mut [u8]) {
    f[24] = 0; f[25] = 0;
    let mut sum = 0u32;
    for i in 0..10 { sum += u16::from_be_bytes([f[14 + 2*i], f[14 + 2*i + 1]]) as u32; }
    while sum >> 16 != 0 { sum = (sum & 0xFFFF) + (sum >> 16); }
    let c = !(sum as u16);
    f[24..26].copy_from_slice(&c.to_be_bytes());
}
