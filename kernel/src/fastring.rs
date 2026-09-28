//! The layout and the index arithmetic of the zero-syscall rings, and the ARP
//! exchange the cell answers by itself.
//!
//! Why a separate module. Both things are byte and index arithmetic whose mistake
//! is silent. The rings are SPSC in memory shared between ring 0 and ring 3: the
//! producer publishes a slot by advancing an index, the consumer takes it by
//! advancing another, and the only thing that keeps a live packet from being
//! overwritten is the comparison of the two — written with the wrong wrap it drops
//! frames under load and nothing says so. The ARP reply is forty-two bytes at fixed
//! offsets: one offset off and the cell answers with a plausible frame that names
//! the wrong address, which looks like a network that "sometimes does not work".
//!
//! Nothing here needs a device, a card or a core: the ring is a byte slice and a
//! pair of indices, the frame is a byte slice.
//!
//! The ring's layout (it mirrors `fastapp.c`):
//! ```text
//!   +0    u32 prod          the producer's index (it only grows, wraps at 2^32)
//!   +64   u32 cons          the consumer's index
//!   +128  the slots, SLOT_SZ each: u32 len at +0, the padding to HDR, then the payload
//! ```
//! The indices are NOT masked in memory: the distance between them is what says how
//! full the ring is, and masking happens only when a slot is addressed.

/// Slots per ring (a power of two — the mask below relies on it).
pub const SLOTS: u32 = 256;
const _: () = assert!(SLOTS.is_power_of_two());
/// The mask for addressing a slot by an index.
pub const MASK: u32 = SLOTS - 1;
/// A slot's header: `u32 len` plus the padding to the payload.
pub const HDR: usize = 16;
/// The payload per slot.
pub const FSLOT: usize = 2048;
/// A slot's size.
pub const SLOT_SZ: usize = HDR + FSLOT;
/// The producer's index lies at +0, the consumer's at +64 — separate cache lines, so
/// that the two sides do not bounce one line between cores.
pub const CONS_OFF: usize = 64;
/// Where the slots begin.
pub const SLOTS_OFF: usize = 128;
/// One ring's size.
pub const RING_SZ: usize = SLOTS_OFF + (SLOTS as usize) * SLOT_SZ;
/// The shared region: the rx ring and the tx ring.
pub const REGION_SZ: usize = 2 * RING_SZ;
/// The rx ring's offset in the region (the cell produces, the application consumes).
pub const RX_OFF: usize = 0;
/// The tx ring's offset (the application produces, the cell consumes).
pub const TX_OFF: usize = RING_SZ;
// The layout's invariants are checked AT COMPILE TIME, in the kernel's build as well as
// in the tests: the rings must not overlap, and the two indices must sit on separate
// cache lines (otherwise the producer and the consumer bounce one line between cores).
const _: () = assert!(TX_OFF >= RX_OFF + RING_SZ);
const _: () = assert!(CONS_OFF >= 64 && SLOTS_OFF >= CONS_OFF + 64);

/// The offset of the slot for the index `i` inside the ring.
#[inline]
pub fn slot_off(i: u32) -> usize { SLOTS_OFF + (i & MASK) as usize * SLOT_SZ }

/// How many slots are filled and not yet taken. A WRAPPING subtraction on purpose:
/// the indices grow for ever and go round 2^32, and `prod - cons` stays the true
/// distance across the wrap, while `prod > cons` would be false for half the ring.
#[inline]
pub fn filled(prod: u32, cons: u32) -> u32 { prod.wrapping_sub(cons) }

/// May the producer publish one more slot?
#[inline]
pub fn can_produce(prod: u32, cons: u32) -> bool { filled(prod, cons) < SLOTS }

/// Is there something for the consumer to take?
#[inline]
pub fn can_consume(prod: u32, cons: u32) -> bool { prod != cons }

/// The length the producer writes into a slot: never more than the payload fits.
#[inline]
pub fn clamp_len(len: usize) -> usize { len.min(FSLOT) }

// ── the ARP exchange the cell answers by itself ───────────────────────────────
//
// The fast path is self-sufficient: there is no stack behind it to answer an ARP
// "who has this address", and without an answer the neighbour never sends us a
// single packet. So the cell parses the request and builds the reply itself.
//
// The frame (Ethernet + ARP, 42 bytes):
// ```text
//   0..6   the destination MAC      6..12  the source MAC
//   12..14 the ethertype 0x0806     14..16 the hardware type 0x0001
//   16..18 the protocol 0x0800      18     the hardware length 6
//   19     the protocol length 4    20..22 the opcode (1 request, 2 reply)
//   22..28 the sender's MAC         28..32 the sender's IP
//   32..38 the target's MAC         38..42 the target's IP
// ```

/// The shortest frame that can carry an ARP request or reply.
pub const ARP_LEN: usize = 42;

/// Is this frame an ARP REQUEST for `my_ip`? Everything is checked that the answer
/// depends on: the length, the ethertype, the opcode and the target's address — an
/// answer to somebody else's request poisons a neighbour's table with our MAC.
pub fn is_arp_request_for(frame: &[u8], my_ip: [u8; 4]) -> bool {
    frame.len() >= ARP_LEN
        && frame[12] == 0x08 && frame[13] == 0x06   // the ethertype ARP
        && frame[20] == 0x00 && frame[21] == 0x01   // the opcode: a request
        && frame[38..42] == my_ip                   // ...for our address
}

/// The sender of an ARP request: `(its MAC, its IP)` — the addresses the reply goes to.
pub fn arp_sender(frame: &[u8]) -> ([u8; 6], [u8; 4]) {
    let mut mac = [0u8; 6];
    let mut ip = [0u8; 4];
    mac.copy_from_slice(&frame[22..28]);
    ip.copy_from_slice(&frame[28..32]);
    (mac, ip)
}

/// Build the ARP REPLY into `out` (at least `ARP_LEN` bytes): from `my_mac`/`my_ip`
/// to the sender of the request. The reply's roles are the mirror of the request's —
/// getting that backwards is the mistake that makes a neighbour learn its own
/// address from us.
pub fn build_arp_reply(out: &mut [u8], my_mac: [u8; 6], my_ip: [u8; 4], to_mac: [u8; 6], to_ip: [u8; 4]) {
    let t = &mut out[..ARP_LEN];
    t[0..6].copy_from_slice(&to_mac);                       // to the one who asked
    t[6..12].copy_from_slice(&my_mac);
    t[12] = 0x08; t[13] = 0x06;                             // ARP
    t[14] = 0; t[15] = 1;                                   // Ethernet
    t[16] = 0x08; t[17] = 0;                                // IPv4
    t[18] = 6; t[19] = 4;                                   // the address lengths
    t[20] = 0; t[21] = 2;                                   // the opcode: a reply
    t[22..28].copy_from_slice(&my_mac);                     // the sender: we
    t[28..32].copy_from_slice(&my_ip);
    t[32..38].copy_from_slice(&to_mac);                     // the target: the one who asked
    t[38..42].copy_from_slice(&to_ip);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MY_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const MY_IP: [u8; 4] = [10, 0, 0, 2];
    const PEER_MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
    const PEER_IP: [u8; 4] = [10, 0, 0, 1];

    fn arp_request(target: [u8; 4]) -> [u8; ARP_LEN] {
        let mut f = [0u8; ARP_LEN];
        f[0..6].copy_from_slice(&[0xFF; 6]);                // a broadcast
        f[6..12].copy_from_slice(&PEER_MAC);
        f[12] = 0x08; f[13] = 0x06;
        f[14] = 0; f[15] = 1; f[16] = 0x08; f[17] = 0; f[18] = 6; f[19] = 4;
        f[20] = 0; f[21] = 1;                               // a request
        f[22..28].copy_from_slice(&PEER_MAC);
        f[28..32].copy_from_slice(&PEER_IP);
        f[38..42].copy_from_slice(&target);
        f
    }

    #[test]
    fn the_layout_adds_up_and_the_slots_do_not_overlap() {
        assert_eq!(SLOT_SZ, HDR + FSLOT);
        assert_eq!(RING_SZ, SLOTS_OFF + SLOTS as usize * SLOT_SZ);
        assert_eq!(REGION_SZ, 2 * RING_SZ);
        for i in 0..SLOTS {
            let a = slot_off(i);
            assert!(a >= SLOTS_OFF, "the slot {} climbed onto the indices", i);
            assert!(a + SLOT_SZ <= RING_SZ, "the slot {} left the ring", i);
            if i > 0 { assert_eq!(a - slot_off(i - 1), SLOT_SZ, "the slots are not adjacent"); }
        }
    }

    #[test]
    fn a_slot_is_addressed_by_the_index_modulo_the_ring() {
        assert_eq!(slot_off(0), slot_off(SLOTS));
        assert_eq!(slot_off(1), slot_off(SLOTS + 1));
        assert_eq!(slot_off(MASK), slot_off(u32::MAX & MASK | (u32::MAX & !MASK)));
        // the last index before the wrap of the counter addresses a lawful slot
        assert!(slot_off(u32::MAX) + SLOT_SZ <= RING_SZ);
    }

    /// The comparison that keeps a live packet from being overwritten. It must hold
    /// across the wrap of the 32-bit counters — that is why the subtraction wraps.
    #[test]
    fn the_fullness_is_counted_across_the_wrap_of_the_counters() {
        assert_eq!(filled(0, 0), 0);
        assert_eq!(filled(5, 0), 5);
        assert_eq!(filled(SLOTS, 0), SLOTS);
        // the producer is just before the wrap, the consumer just after it
        assert_eq!(filled(3, u32::MAX - 1), 5);
        assert_eq!(filled(0, u32::MAX), 1);
        // a naive comparison would call the ring EMPTY right here
        let (prod, cons) = (0u32, u32::MAX);
        assert!(prod < cons, "naively the producer looks to be behind the consumer");
        assert!(can_consume(prod, cons), "there IS a packet: the producer has wrapped");
        assert_eq!(filled(prod, cons), 1);
    }

    #[test]
    fn a_full_ring_refuses_and_an_empty_one_gives_nothing() {
        assert!(can_produce(0, 0));
        assert!(!can_consume(0, 0), "an empty ring");
        assert!(can_produce(SLOTS - 1, 0));
        assert!(!can_produce(SLOTS, 0), "the ring is full — the packet must be dropped, not overwritten");
        assert!(!can_produce(SLOTS + 1, 0), "even fuller");
        assert!(can_consume(SLOTS, 0));
        // across the wrap the ceiling is the same
        assert!(!can_produce(SLOTS - 1, u32::MAX));
        assert!(can_produce(SLOTS - 2, u32::MAX));
    }

    /// A full circle of the ring: what the producer put in, the consumer takes out in
    /// the same order, and nothing is lost over several wraps of the counters.
    #[test]
    fn everything_produced_is_consumed_in_order_over_several_wraps() {
        let mut prod = u32::MAX - 3;   // start just before the wrap
        let mut cons = prod;
        let mut sent = 0u64;
        let mut got = 0u64;
        for round in 0..(SLOTS as u64 * 5) {
            // the producer fills up to the ceiling
            while can_produce(prod, cons) && sent - got < 3 {
                prod = prod.wrapping_add(1); sent += 1;
            }
            assert!(filled(prod, cons) <= SLOTS, "the round {} overfilled the ring", round);
            // the consumer takes one
            if can_consume(prod, cons) { cons = cons.wrapping_add(1); got += 1; }
        }
        while can_consume(prod, cons) { cons = cons.wrapping_add(1); got += 1; }
        assert_eq!(sent, got, "packets were lost in the ring");
    }

    #[test]
    fn a_length_never_exceeds_the_payload() {
        assert_eq!(clamp_len(0), 0);
        assert_eq!(clamp_len(FSLOT - 1), FSLOT - 1);
        assert_eq!(clamp_len(FSLOT), FSLOT);
        assert_eq!(clamp_len(FSLOT + 1), FSLOT, "a jumbo frame is cut, not written past the slot");
        assert_eq!(clamp_len(usize::MAX), FSLOT);
    }

    #[test]
    fn an_arp_request_for_us_is_recognised() {
        let f = arp_request(MY_IP);
        assert!(is_arp_request_for(&f, MY_IP));
        assert_eq!(arp_sender(&f), (PEER_MAC, PEER_IP));
    }

    #[test]
    fn a_request_for_somebody_else_is_not_answered() {
        let f = arp_request([10, 0, 0, 7]);
        assert!(!is_arp_request_for(&f, MY_IP), "answering it would poison the neighbour's table");
    }

    #[test]
    fn a_reply_a_short_frame_and_a_foreign_ethertype_are_not_requests() {
        let mut reply = arp_request(MY_IP);
        reply[21] = 2;                                  // the opcode: a reply
        assert!(!is_arp_request_for(&reply, MY_IP));
        let mut ip = arp_request(MY_IP);
        ip[12] = 0x08; ip[13] = 0x00;                   // IPv4, not ARP
        assert!(!is_arp_request_for(&ip, MY_IP));
        let f = arp_request(MY_IP);
        assert!(!is_arp_request_for(&f[..ARP_LEN - 1], MY_IP), "a frame shorter than the header");
        assert!(!is_arp_request_for(&[], MY_IP));
    }

    /// The reply must be the MIRROR of the request: our address as the sender, the
    /// asker's as the target. Swap them and the neighbour learns its own address from
    /// us — the table looks plausible and the traffic goes nowhere.
    #[test]
    fn the_reply_mirrors_the_request() {
        let req = arp_request(MY_IP);
        let (to_mac, to_ip) = arp_sender(&req);
        let mut out = [0xEEu8; 64];
        build_arp_reply(&mut out, MY_MAC, MY_IP, to_mac, to_ip);
        assert_eq!(&out[0..6], &PEER_MAC, "it goes to the one who asked");
        assert_eq!(&out[6..12], &MY_MAC);
        assert_eq!(&out[12..14], &[0x08, 0x06]);
        assert_eq!(&out[20..22], &[0x00, 0x02], "the opcode: a reply");
        assert_eq!(&out[22..28], &MY_MAC, "the sender is us");
        assert_eq!(&out[28..32], &MY_IP);
        assert_eq!(&out[32..38], &PEER_MAC, "the target is the one who asked");
        assert_eq!(&out[38..42], &PEER_IP);
        assert_eq!(out[ARP_LEN], 0xEE, "nothing is written past the frame");
        // and such a reply is not taken for a request
        assert!(!is_arp_request_for(&out[..ARP_LEN], PEER_IP));
    }

    #[test]
    fn a_reply_is_built_the_same_from_a_request_we_parsed_ourselves() {
        // a round trip: the request -> the reply -> the parse of the reply's fields
        let req = arp_request(MY_IP);
        let (mac, ip) = arp_sender(&req);
        let mut out = [0u8; ARP_LEN];
        build_arp_reply(&mut out, MY_MAC, MY_IP, mac, ip);
        let (rmac, rip) = arp_sender(&out);
        assert_eq!((rmac, rip), (MY_MAC, MY_IP), "the reply's sender is us");
    }
}
