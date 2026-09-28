//! The headers of an ELF64 binary: the file header and the program headers.
//!
//! Why a separate module. The kernel reads these bytes from a file the workload
//! brought — an image on the volume, a binary handed to `execve`. Until now
//! every loader decoded the offsets itself and indexed the buffer without
//! looking: a truncated binary (or one with `e_phnum` of 65535) does not give a
//! bad workload, it gives a panic inside the kernel, on a path that a user
//! reaches with a plain `open`. The decoding lives here, returns `None` instead
//! of indexing past the end, and is judged by tests rather than by a boot.
//!
//! The offsets (ELF64, little-endian):
//! ```text
//!   file header:  0 magic 7F 'E' 'L' 'F'   4 class(2=64)   5 data(1=LE)
//!                16 u16 e_type            18 u16 e_machine (0x3E = x86-64)
//!                24 u64 e_entry           32 u64 e_phoff
//!                54 u16 e_phentsize       56 u16 e_phnum
//!   program hdr:  0 u32 p_type             4 u32 p_flags
//!                 8 u64 p_offset          16 u64 p_vaddr
//!                32 u64 p_filesz          40 u64 p_memsz   48 u64 p_align
//! ```

/// A segment to be loaded into memory.
pub const PT_LOAD: u32 = 1;
/// The path of the interpreter (a dynamic binary).
pub const PT_INTERP: u32 = 3;
/// `p_flags`: the segment is executable.
pub const PF_X: u32 = 1;
/// `p_flags`: the segment is writable.
pub const PF_W: u32 = 2;
/// The smallest program header of an ELF64.
pub const PHENT: usize = 56;
/// The file header of an ELF64.
pub const EHDR: usize = 64;

/// As many program headers as a loader is willing to read: a real binary has
/// units of them, and the number comes from the file, so it needs a ceiling.
pub const PHNUM_MAX: usize = 64;

/// The file header, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hdr {
    /// `e_type`: 2 = ET_EXEC (a fixed address), 3 = ET_DYN (a PIE).
    pub etype: u16,
    pub entry: u64,
    pub phoff: u64,
    pub phentsize: usize,
    pub phnum: usize,
}

impl Hdr {
    /// Is this a PIE (it may be loaded at any base)?
    #[inline]
    pub fn is_pie(&self) -> bool { self.etype == 3 }
}

/// A program header, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ph {
    pub ptype: u32,
    pub flags: u32,
    pub off: u64,
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

impl Ph {
    /// Is the segment to be loaded into memory?
    #[inline]
    pub fn is_load(&self) -> bool { self.ptype == PT_LOAD }
    /// The rights of the segment's pages: `(writable, executable)`.
    #[inline]
    pub fn rights(&self) -> (bool, bool) { (self.flags & PF_W != 0, self.flags & PF_X != 0) }
    /// Does the segment's file part lie inside a file of `len` bytes?
    #[inline]
    pub fn fits(&self, len: usize) -> bool {
        match self.off.checked_add(self.filesz) {
            Some(end) => end <= len as u64,
            None => false,
        }
    }
}

#[inline]
fn rd16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
#[inline]
fn rd32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
#[inline]
fn rd64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7]])
}

/// Decode the file header. `None` — this is not an ELF64 for x86-64, or its
/// header does not add up.
///
/// `b` is the beginning of the file: 64 bytes are enough.
pub fn header(b: &[u8]) -> Option<Hdr> {
    if b.len() < EHDR { return None; }
    if b[0..4] != [0x7F, b'E', b'L', b'F'] { return None; }
    if b[4] != 2 || b[5] != 1 { return None; }          // ELF64, little-endian
    if rd16(b, 18) != 0x3E { return None; }             // x86-64
    let phentsize = rd16(b, 54) as usize;
    let phnum = rd16(b, 56) as usize;
    if phnum != 0 && (phentsize < PHENT || phnum > PHNUM_MAX) { return None; }
    let phoff = rd64(b, 32);
    // The table must lie inside a file addressable at all.
    phoff.checked_add((phnum * phentsize) as u64)?;
    Some(Hdr { etype: rd16(b, 16), entry: rd64(b, 24), phoff, phentsize, phnum })
}

/// Decode the `i`-th program header out of the table's buffer. `None` — the
/// buffer is shorter than the header.
pub fn phdr(tab: &[u8], i: usize, phentsize: usize) -> Option<Ph> {
    let o = i.checked_mul(phentsize)?;
    let p = tab.get(o..o.checked_add(PHENT)?)?;
    Some(Ph {
        ptype: rd32(p, 0),
        flags: rd32(p, 4),
        off: rd64(p, 8),
        vaddr: rd64(p, 16),
        filesz: rd64(p, 32),
        memsz: rd64(p, 40),
        align: rd64(p, 48),
    })
}

/// The program headers of a table's buffer. The walk ends where the buffer does:
/// a binary that promises more headers than it carries gives out what it really has.
pub fn phdrs(tab: &[u8], phentsize: usize, phnum: usize) -> impl Iterator<Item = Ph> + '_ {
    (0..phnum).map_while(move |i| phdr(tab, i, phentsize))
}

/// The span `[lo, hi)` of the addresses of the PT_LOAD segments — the region the
/// image occupies in memory. `None` — there is not a single such segment.
pub fn load_span(tab: &[u8], phentsize: usize, phnum: usize) -> Option<(u64, u64)> {
    let (mut lo, mut hi) = (u64::MAX, 0u64);
    for p in phdrs(tab, phentsize, phnum).filter(|p| p.is_load()) {
        lo = lo.min(p.vaddr);
        hi = hi.max(p.vaddr.saturating_add(p.memsz));
    }
    if hi > lo { Some((lo, hi)) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wr16(b: &mut [u8], o: usize, v: u16) { b[o..o + 2].copy_from_slice(&v.to_le_bytes()); }
    fn wr32(b: &mut [u8], o: usize, v: u32) { b[o..o + 4].copy_from_slice(&v.to_le_bytes()); }
    fn wr64(b: &mut [u8], o: usize, v: u64) { b[o..o + 8].copy_from_slice(&v.to_le_bytes()); }

    /// A file header as `ld` writes it: ET_EXEC, x86-64, `phnum` headers right
    /// after the header itself.
    fn ehdr(etype: u16, entry: u64, phnum: usize) -> [u8; EHDR] {
        let mut b = [0u8; EHDR];
        b[0..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
        b[4] = 2; b[5] = 1;
        wr16(&mut b, 16, etype);
        wr16(&mut b, 18, 0x3E);
        wr64(&mut b, 24, entry);
        wr64(&mut b, 32, EHDR as u64);
        wr16(&mut b, 54, PHENT as u16);
        wr16(&mut b, 56, phnum as u16);
        b
    }

    fn ph(ptype: u32, flags: u32, off: u64, vaddr: u64, filesz: u64, memsz: u64) -> [u8; PHENT] {
        let mut p = [0u8; PHENT];
        wr32(&mut p, 0, ptype);
        wr32(&mut p, 4, flags);
        wr64(&mut p, 8, off);
        wr64(&mut p, 16, vaddr);
        wr64(&mut p, 32, filesz);
        wr64(&mut p, 40, memsz);
        wr64(&mut p, 48, 4096);
        p
    }

    /// A whole little binary: the header, the table, and bytes up to `len`.
    fn image(etype: u16, entry: u64, segs: &[[u8; PHENT]], len: usize) -> Vec<u8> {
        let mut v = ehdr(etype, entry, segs.len()).to_vec();
        for s in segs { v.extend_from_slice(s); }
        v.resize(len.max(v.len()), 0);
        v
    }

    #[test]
    fn a_static_binary_is_read_field_by_field() {
        let text = ph(PT_LOAD, PF_X, 0, 0x10000000, 0x1000, 0x1000);
        let data = ph(PT_LOAD, PF_W, 0x1000, 0x10001000, 0x200, 0x800);
        let v = image(2, 0x10000123, &[text, data], 0x2000);
        let h = header(&v).expect("a healthy header");
        assert_eq!(h.etype, 2);
        assert!(!h.is_pie());
        assert_eq!(h.entry, 0x10000123);
        assert_eq!(h.phoff, EHDR as u64);
        assert_eq!(h.phentsize, PHENT);
        assert_eq!(h.phnum, 2);

        let tab = &v[h.phoff as usize..];
        let ps: Vec<Ph> = phdrs(tab, h.phentsize, h.phnum).collect();
        assert_eq!(ps.len(), 2);
        assert!(ps[0].is_load());
        assert_eq!(ps[0].rights(), (false, true), "the text is executable and not writable");
        assert_eq!(ps[1].rights(), (true, false), "the data is writable and not executable");
        assert_eq!(ps[1].memsz - ps[1].filesz, 0x600, "the .bss is the tail of memsz");
        assert!(ps.iter().all(|p| p.fits(v.len())));
    }

    #[test]
    fn a_pie_says_so_by_its_type() {
        let v = image(3, 0x1000, &[ph(PT_LOAD, PF_X, 0, 0, 0x100, 0x100)], 0x1000);
        assert!(header(&v).unwrap().is_pie());
    }

    #[test]
    fn anything_that_is_not_an_elf64_for_x86_64_is_refused() {
        let good = image(2, 1, &[ph(PT_LOAD, PF_X, 0, 0x1000, 0x10, 0x10)], 0x1000);
        assert!(header(&good).is_some());

        let mut bad = good.clone(); bad[1] = b'X';                    // the magic
        assert_eq!(header(&bad), None);
        let mut bad = good.clone(); bad[4] = 1;                       // a 32-bit ELF
        assert_eq!(header(&bad), None);
        let mut bad = good.clone(); bad[5] = 2;                       // big-endian
        assert_eq!(header(&bad), None);
        let mut bad = good.clone(); wr16(&mut bad, 18, 0xB7);         // aarch64
        assert_eq!(header(&bad), None);
    }

    #[test]
    fn a_file_shorter_than_the_header_is_refused_rather_than_indexed() {
        let v = image(2, 1, &[ph(PT_LOAD, PF_X, 0, 0x1000, 0x10, 0x10)], 0x1000);
        for len in 0..EHDR {
            assert_eq!(header(&v[..len]), None, "a header of {} bytes has been taken", len);
        }
        assert!(header(&v[..EHDR]).is_some(), "64 bytes are enough for the header itself");
    }

    #[test]
    fn an_impossible_program_header_table_is_refused() {
        let mut v = image(2, 1, &[ph(PT_LOAD, PF_X, 0, 0x1000, 0x10, 0x10)], 0x1000);
        wr16(&mut v, 54, 40);                     // phentsize of a 32-bit ELF
        assert_eq!(header(&v), None);

        let mut v = image(2, 1, &[ph(PT_LOAD, PF_X, 0, 0x1000, 0x10, 0x10)], 0x1000);
        wr16(&mut v, 56, 65535);                  // more headers than any binary has
        assert_eq!(header(&v), None);

        let mut v = image(2, 1, &[ph(PT_LOAD, PF_X, 0, 0x1000, 0x10, 0x10)], 0x1000);
        wr64(&mut v, 32, u64::MAX - 8);           // the table is beyond the end of the numbers
        assert_eq!(header(&v), None);
    }

    #[test]
    fn a_binary_without_program_headers_is_still_a_header() {
        let v = image(2, 0x400000, &[], EHDR);
        let h = header(&v).expect("phnum = 0 is lawful");
        assert_eq!(h.phnum, 0);
        assert_eq!(phdrs(&v[EHDR..], h.phentsize, h.phnum).count(), 0);
        assert_eq!(load_span(&v[EHDR..], h.phentsize, h.phnum), None);
    }

    #[test]
    fn a_table_cut_short_gives_out_what_is_really_there() {
        let a = ph(PT_LOAD, PF_X, 0, 0x1000, 0x10, 0x10);
        let b = ph(PT_LOAD, PF_W, 0x1000, 0x2000, 0x10, 0x10);
        let v = image(2, 1, &[a, b], 0x3000);
        let tab = &v[EHDR..EHDR + PHENT + 8];     // the second header is cut in the middle
        assert_eq!(phdrs(tab, PHENT, 2).count(), 1, "a torn header is not given out");
        assert_eq!(phdr(tab, 1, PHENT), None);
        assert_eq!(phdr(&v[EHDR..], 1, PHENT).map(|p| p.vaddr), Some(0x2000));
    }

    #[test]
    fn a_segment_that_promises_more_than_the_file_holds_is_seen() {
        let v = image(2, 1, &[ph(PT_LOAD, PF_X, 0x100, 0x1000, 0x4000, 0x4000)], 0x1000);
        let p = phdr(&v[EHDR..], 0, PHENT).unwrap();
        assert!(!p.fits(v.len()), "the file part goes beyond the file");
        let ok = ph(PT_LOAD, PF_X, 0x100, 0x1000, 0x100, 0x4000);
        assert!(phdr(&ok, 0, PHENT).unwrap().fits(0x1000), "the memsz tail beyond the file is the .bss, that is lawful");
        // an offset at the end of the numbers must not wrap round into "it fits"
        let mut huge = ph(PT_LOAD, PF_X, u64::MAX - 4, 0x1000, 0x100, 0x100);
        wr64(&mut huge, 32, 0x100);
        assert!(!phdr(&huge, 0, PHENT).unwrap().fits(0x1000));
    }

    #[test]
    fn the_span_of_the_image_covers_the_load_segments_only() {
        let text = ph(PT_LOAD, PF_X, 0, 0x10000000, 0x1000, 0x1000);
        let data = ph(PT_LOAD, PF_W, 0x1000, 0x10002000, 0x200, 0x1800);
        let interp = ph(PT_INTERP, 0, 0x200, 0x99999999, 0x20, 0x20);
        let tls = ph(7, 0, 0x1000, 0x10002000, 0x10, 0x40);   // PT_TLS
        let v = image(2, 1, &[interp, text, tls, data], 0x4000);
        let h = header(&v).unwrap();
        let tab = &v[h.phoff as usize..];
        assert_eq!(load_span(tab, h.phentsize, h.phnum), Some((0x10000000, 0x10003800)));
        assert_eq!(phdrs(tab, h.phentsize, h.phnum).filter(|p| p.is_load()).count(), 2);
        assert_eq!(phdrs(tab, h.phentsize, h.phnum).filter(|p| p.ptype == PT_INTERP).count(), 1);
    }

    #[test]
    fn a_span_does_not_wrap_round_on_an_impossible_segment() {
        let s = ph(PT_LOAD, PF_W, 0, u64::MAX - 0x100, 0x10, 0x1000);
        let v = image(2, 1, &[s], 0x1000);
        let (lo, hi) = load_span(&v[EHDR..], PHENT, 1).unwrap();
        assert_eq!(lo, u64::MAX - 0x100);
        assert_eq!(hi, u64::MAX, "the end is clamped rather than wrapped to zero");
    }
}
