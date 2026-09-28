//! The notes of a core dump: `NT_PRSTATUS` (the registers) and `NT_PRPSINFO`
//! (the process's name), byte for byte as a debugger expects them.
//!
//! Why a separate module. The notes go in a core file one after another, and
//! each carries its own declared length — so a single lost byte of alignment
//! does not spoil ITS note but shifts the parsing of ALL the following ones.
//! That is not a hypothesis: the first version of the dump lost four bytes of
//! padding before `pr_flag` in `prpsinfo`, and `readelf` reported a corrupted
//! note on the NEXT one, while our own checks saw nothing. The layout is built
//! here into a buffer of a fixed size, so the tests can both compare the bytes
//! and walk the stream the way a debugger walks it.
//!
//! A note in an ELF:
//! ```text
//!   u32 namesz (5 for "CORE")   u32 descsz   u32 type
//!   the name, padded to 4       the descriptor, padded to 4
//! ```

/// The type of the note with the registers.
pub const NT_PRSTATUS: u32 = 1;
/// The type of the note with the process's name.
pub const NT_PRPSINFO: u32 = 3;

/// The descriptor of `NT_PRSTATUS` (`elf_prstatus` on x86-64).
pub const PRSTATUS_DESC: usize = 336;
/// The descriptor of `NT_PRPSINFO` (`elf_prpsinfo`).
pub const PRPSINFO_DESC: usize = 136;
/// The header of a note plus the name "CORE" padded to 4: 12 + 8.
pub const HDR: usize = 12 + 8;

/// The whole note with the registers.
pub const PRSTATUS_LEN: usize = HDR + PRSTATUS_DESC;
/// The whole note with the name.
pub const PRPSINFO_LEN: usize = HDR + PRPSINFO_DESC;

/// How many bytes the notes of a dump of `threads` threads take: one
/// `NT_PRPSINFO` for the process and an `NT_PRSTATUS` per thread.
///
/// It is counted from the same constants the builders below use — a number that
/// disagrees with the bytes gives a file that the debugger reads WRONGLY rather
/// than an error.
pub fn notes_size(threads: usize) -> u64 { (PRPSINFO_LEN + PRSTATUS_LEN * threads) as u64 }

struct Buf<'a> { b: &'a mut [u8], o: usize }

impl Buf<'_> {
    #[inline]
    fn put(&mut self, s: &[u8]) { self.b[self.o..self.o + s.len()].copy_from_slice(s); self.o += s.len(); }
    #[inline]
    fn u16(&mut self, v: u16) { self.put(&v.to_le_bytes()); }
    #[inline]
    fn u32(&mut self, v: u32) { self.put(&v.to_le_bytes()); }
    #[inline]
    fn u64(&mut self, v: u64) { self.put(&v.to_le_bytes()); }
    #[inline]
    fn zeros(&mut self, n: usize) { for _ in 0..n { self.put(&[0]); } }
    fn head(&mut self, descsz: usize, ntype: u32) {
        self.u32(5);                  // namesz: "CORE\0"
        self.u32(descsz as u32);
        self.u32(ntype);
        self.put(b"CORE\0\0\0\0");    // the name plus the padding to 4 (5 -> 8)
    }
}

/// The registers in the order of `user_regs_struct` — exactly what GDB expects
/// in an `NT_PRSTATUS`. The order is not ours and must not be changed: fields
/// swapped round give not "a format error" but a PLAUSIBLE wrong stack.
pub const NREGS: usize = 27;

/// Build the note `NT_PRSTATUS`.
pub fn prstatus(sig: u32, pid: i32, regs: &[u64; NREGS]) -> [u8; PRSTATUS_LEN] {
    let mut out = [0u8; PRSTATUS_LEN];
    let mut w = Buf { b: &mut out, o: 0 };
    w.head(PRSTATUS_DESC, NT_PRSTATUS);
    w.u32(sig); w.u32(0); w.u32(0);          // si_signo, si_code, si_errno
    w.u16(sig as u16); w.u16(0);             // pr_cursig + the padding
    w.u64(0); w.u64(0);                      // pr_sigpend, pr_sighold
    w.u32(pid as u32); w.u32(0); w.u32(0); w.u32(0); // pid/ppid/pgrp/sid
    for _ in 0..8 { w.u64(0); }              // four timevals
    for r in regs.iter() { w.u64(*r); }
    w.u32(0);                                // pr_fpvalid
    w.u32(0);                                // the trailing alignment of the structure to 8
    debug_assert_eq!(w.o, PRSTATUS_LEN);
    out
}

/// Build the note `NT_PRPSINFO`. The name is cut to 15 bytes plus a zero — that
/// is the field's size, and a longer one would eat the next field.
pub fn prpsinfo(pid: i32, name: &[u8]) -> [u8; PRPSINFO_LEN] {
    let mut out = [0u8; PRPSINFO_LEN];
    let mut w = Buf { b: &mut out, o: 0 };
    w.head(PRPSINFO_DESC, NT_PRPSINFO);
    w.zeros(4);                              // state, sname, zomb, nice
    w.zeros(4);                              // the alignment to 8 before pr_flag
    w.u64(0);                                // pr_flag
    w.u32(0); w.u32(0);                      // uid, gid
    w.u32(pid as u32); w.u32(0); w.u32(0); w.u32(0); // pid/ppid/pgrp/sid
    let n = name.len().min(15);
    let mut fname = [0u8; 16];
    fname[..n].copy_from_slice(&name[..n]);
    w.put(&fname);
    let mut args = [0u8; 80];
    args[..n].copy_from_slice(&name[..n]);
    w.put(&args);
    debug_assert_eq!(w.o, PRPSINFO_LEN);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rd32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
    fn rd64(b: &[u8], o: usize) -> u64 {
        u64::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7]])
    }

    fn regs() -> [u64; NREGS] {
        let mut r = [0u64; NREGS];
        for (i, v) in r.iter_mut().enumerate() { *v = 0x1000 + i as u64; }
        r
    }

    /// A walk over a stream of notes the way a debugger does it: the header, the
    /// name padded to 4, the descriptor padded to 4. It returns the types it met,
    /// or `None` if the stream does not end exactly at the buffer's end.
    fn walk(b: &[u8]) -> Option<Vec<(u32, usize)>> {
        let mut o = 0usize;
        let mut out = Vec::new();
        while o < b.len() {
            if o + 12 > b.len() { return None; }
            let namesz = rd32(b, o) as usize;
            let descsz = rd32(b, o + 4) as usize;
            let ntype = rd32(b, o + 8);
            let name_pad = namesz.div_ceil(4) * 4;
            let desc_pad = descsz.div_ceil(4) * 4;
            let end = o.checked_add(12 + name_pad + desc_pad)?;
            if end > b.len() { return None; }
            if &b[o + 12..o + 12 + namesz] != b"CORE\0" { return None; }
            out.push((ntype, descsz));
            o = end;
        }
        Some(out)
    }

    #[test]
    fn the_notes_have_exactly_the_length_the_size_promises() {
        assert_eq!(prstatus(11, 1, &regs()).len(), PRSTATUS_LEN);
        assert_eq!(prpsinfo(1, b"mdpipe").len(), PRPSINFO_LEN);
        for threads in 0..8 {
            assert_eq!(notes_size(threads), (PRPSINFO_LEN + threads * PRSTATUS_LEN) as u64);
        }
        assert_eq!(PRSTATUS_LEN % 4, 0);
        assert_eq!(PRPSINFO_LEN % 4, 0);
    }

    #[test]
    fn a_stream_of_notes_is_walked_to_its_very_end() {
        // exactly what a dump of four threads carries
        let mut v = prpsinfo(1, b"mdpipe").to_vec();
        for t in 0..4 { v.extend_from_slice(&prstatus(11, t + 1, &regs())); }
        assert_eq!(v.len(), notes_size(4) as usize);
        let seen = walk(&v).expect("the stream must end exactly at the buffer's end");
        assert_eq!(seen.len(), 5);
        assert_eq!(seen[0], (NT_PRPSINFO, PRPSINFO_DESC));
        assert!(seen[1..].iter().all(|&(t, d)| t == NT_PRSTATUS && d == PRSTATUS_DESC));
    }

    /// The very defect the module was written against: a lost byte of padding is
    /// not seen in its own note — it is seen in the NEXT one.
    #[test]
    fn a_lost_byte_of_padding_breaks_the_stream_and_the_walk_says_so() {
        let mut v = prpsinfo(1, b"mdpipe").to_vec();
        v.extend_from_slice(&prstatus(11, 1, &regs()));
        assert!(walk(&v).is_some());
        let mut broken = v.clone();
        broken.remove(PRPSINFO_LEN - 1);          // one byte less in the first note
        assert!(walk(&broken).is_none(), "the walk must not reach the end of such a stream");
    }

    #[test]
    fn the_header_of_a_note_names_core_and_the_type() {
        let n = prstatus(11, 7, &regs());
        assert_eq!(rd32(&n, 0), 5, "namesz for \"CORE\\0\"");
        assert_eq!(rd32(&n, 4), PRSTATUS_DESC as u32);
        assert_eq!(rd32(&n, 8), NT_PRSTATUS);
        assert_eq!(&n[12..17], b"CORE\0");
        assert_eq!(&n[17..20], &[0, 0, 0], "the name is padded to 4 with zeroes");
    }

    #[test]
    fn the_registers_lie_in_the_order_gdb_reads_them() {
        let r = regs();
        let n = prstatus(11, 7, &r);
        // the descriptor starts at HDR; the registers come after 112 bytes of the
        // fields before them (the signal, the masks, the pids, the timevals)
        let base = HDR + 112;
        for (i, v) in r.iter().enumerate() {
            assert_eq!(rd64(&n, base + i * 8), *v, "the register {} has moved", i);
        }
        assert_eq!(rd64(&n, base + NREGS * 8), 0, "after the registers comes pr_fpvalid, not a register");
    }

    #[test]
    fn the_signal_and_the_pid_are_where_they_are_looked_for() {
        let n = prstatus(9, 1234, &regs());
        assert_eq!(rd32(&n, HDR), 9, "si_signo");
        assert_eq!(u16::from_le_bytes([n[HDR + 12], n[HDR + 13]]), 9, "pr_cursig");
        assert_eq!(rd32(&n, HDR + 32), 1234, "pr_pid");
    }

    #[test]
    fn the_process_name_is_cut_to_the_field_rather_than_eating_the_next_one() {
        let long = b"a-very-long-name-of-a-workload";
        let n = prpsinfo(1, long);
        // the descriptor: 4 (state..nice) + 4 (padding) + 8 (pr_flag) + 8 (uid, gid) + 16 (the pids)
        let fname = HDR + 40;
        assert_eq!(&n[fname..fname + 15], &long[..15]);
        assert_eq!(n[fname + 15], 0, "the name's field ends with a zero");
        // pr_psargs comes right after and starts with the same name
        assert_eq!(&n[fname + 16..fname + 16 + 15], &long[..15]);
        assert_eq!(n[n.len() - 1], 0, "the tail of pr_psargs is zeroes");
        assert!(walk(&n).is_some());
    }

    #[test]
    fn an_empty_name_and_an_empty_dump_are_still_valid_notes() {
        let n = prpsinfo(0, b"");
        assert!(walk(&n).is_some());
        assert_eq!(n[HDR + 40], 0);
        let v = prpsinfo(1, b"x").to_vec();
        assert_eq!(v.len(), notes_size(0) as usize, "a dump without threads is one note");
    }
}
