//! An in-place frame reflector (ARP responder, ICMP echo, UDP echo :7777).
//! The logic is identical to kernel/src/net.rs — checksum-neutral swaps.
pub const OUR_IP: [u8; 4] = [10, 0, 0, 2];

pub fn reflect(f: &mut [u8], our_mac: &[u8; 6]) -> Option<usize> {
    let len = f.len();
    if len < 14 { return None; }
    match u16::from_be_bytes([f[12], f[13]]) {
        0x0806 => arp(f, len, our_mac),
        0x0800 => ipv4(f, len),
        _ => None,
    }
}

fn arp(f: &mut [u8], len: usize, mac: &[u8; 6]) -> Option<usize> {
    if len < 42 { return None; }
    if u16::from_be_bytes([f[20], f[21]]) != 1 { return None; }
    if f[38..42] != OUR_IP { return None; }
    let req: [u8; 6] = f[6..12].try_into().unwrap();
    f[0..6].copy_from_slice(&req);
    f[6..12].copy_from_slice(mac);
    f[20] = 0; f[21] = 2;
    let spa: [u8; 4] = f[28..32].try_into().unwrap();
    f[22..28].copy_from_slice(mac);
    f[28..32].copy_from_slice(&OUR_IP);
    f[32..38].copy_from_slice(&req);
    f[38..42].copy_from_slice(&spa);
    Some(42)
}

fn ipv4(f: &mut [u8], len: usize) -> Option<usize> {
    if len < 34 { return None; }
    let ihl = ((f[14] & 0x0F) as usize) * 4;
    if ihl < 20 || len < 14 + ihl + 8 { return None; }
    if f[30..34] != OUR_IP { return None; }
    let l4 = 14 + ihl;
    match f[23] {
        17 => {
            if u16::from_be_bytes([f[l4 + 2], f[l4 + 3]]) != 7777 { return None; }
            swap_eth(f); swap_ip(f);
            for i in 0..2 { f.swap(l4 + i, l4 + 2 + i); }
            Some(len)
        }
        1 => {
            if f[l4] != 8 { return None; }
            swap_eth(f); swap_ip(f);
            f[l4] = 0;
            let c = u16::from_be_bytes([f[l4 + 2], f[l4 + 3]]);
            f[l4 + 2..l4 + 4].copy_from_slice(&csum_update(c, 0x0800, 0).to_be_bytes());
            Some(len)
        }
        _ => None,
    }
}

fn swap_eth(f: &mut [u8]) { for i in 0..6 { f.swap(i, 6 + i); } }
fn swap_ip(f: &mut [u8]) { for i in 0..4 { f.swap(26 + i, 30 + i); } }
fn csum_update(hc: u16, m_old: u16, m_new: u16) -> u16 {
    let mut s = (!hc as u32) + (!m_old as u32 & 0xFFFF) + m_new as u32;
    while s >> 16 != 0 { s = (s & 0xFFFF) + (s >> 16); }
    !(s as u16)
}
