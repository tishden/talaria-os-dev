//! On-disk layouts the kernel reads before any file system is mounted: the
//! data partition (GPT by name, GPT second entry, MBR second entry) and the
//! read-only TFS1/TFS2 directory, plus path normalisation for TFS lookups.
//!
//! Pure parsing over sector buffers; `fs.rs` supplies the sectors. The formats
//! are written by `kernel/mkfs.py` (TFS1), `kernel/mkfs2.py` (TFS2) and
//! `tools/mk-ami.sh` / `tools/mk-bios.sh` (partition tables).

pub const SECTOR: usize = 512;
/// Longest TFS2 path, including the terminating NUL slot.
pub const NAMELEN: usize = 84;

/// Where the data partition was found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PartFrom {
    /// GPT entry named "talaria-data" (hybrid images: BIOS boot, ESP, data).
    GptNamed,
    /// GPT without a named entry: the second entry.
    GptSecond,
    /// MBR (BIOS images): the second entry — the first holds GRUB's /boot.
    Mbr,
}

/// First LBA of the data partition, or `None` for a bare file system at LBA 0.
/// `read(lba, buf)` reads one sector and returns false on an I/O error.
pub fn data_partition<F: FnMut(usize, &mut [u8; SECTOR]) -> bool>(mut read: F) -> Option<(usize, PartFrom)> {
    let mut s = [0u8; SECTOR];
    if read(1, &mut s) && &s[..8] == b"EFI PART" {
        let pe_lba = u64::from_le_bytes(s[72..80].try_into().unwrap()) as usize;
        let n = u32::from_le_bytes(s[80..84].try_into().unwrap()) as usize;
        let esz = u32::from_le_bytes(s[84..88].try_into().unwrap()) as usize;
        if !(128..=SECTOR).contains(&esz) { return None; }
        let mut second = 0u64;
        for i in 0..n.min(32) {
            let (sec, off) = (pe_lba + i * esz / SECTOR, (i * esz) % SECTOR);
            if !read(sec, &mut s) { break; }
            let first = u64::from_le_bytes(s[off + 32..off + 40].try_into().unwrap());
            if first == 0 { continue; }
            if i == 1 { second = first; }
            // the name: UTF-16LE, 36 characters at offset 56; ASCII is enough here
            let named = (0..12).all(|k| s[off + 56 + k * 2] == b"talaria-data"[k] && s[off + 57 + k * 2] == 0);
            if named { return Some((first as usize, PartFrom::GptNamed)); }
        }
        return if second != 0 { Some((second as usize, PartFrom::GptSecond)) } else { None };
    }
    if read(0, &mut s) && s[510] == 0x55 && s[511] == 0xAA {
        let e = 446 + 16;
        let typ = s[e + 4];
        let first = u32::from_le_bytes(s[e + 8..e + 12].try_into().unwrap());
        if typ != 0 && first != 0 { return Some((first as usize, PartFrom::Mbr)); }
    }
    None
}

/// Directory format of a TFS volume.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tfs {
    /// TFS1: up to 16 flat names of ≤ 24 bytes, directory in sector 0.
    V1,
    /// TFS2: paths ≤ 83 bytes, directories, the directory spans `dirsec` sectors.
    V2 { dirsec: usize },
}

pub const TFS1_MAGIC: u32 = 0x3153_4654; // "TFS1"
pub const TFS2_MAGIC: u32 = 0x3253_4654; // "TFS2"
pub const TFS1_MAX: usize = 16;

/// Header of a TFS volume from its first sector: the format and the number of
/// entries it claims. `None` — not TFS (the caller tries ext2).
pub fn tfs_header(s0: &[u8]) -> Option<(Tfs, usize)> {
    let magic = u32::from_le_bytes(s0[0..4].try_into().unwrap());
    let count = u32::from_le_bytes(s0[4..8].try_into().unwrap()) as usize;
    match magic {
        TFS1_MAGIC => Some((Tfs::V1, count.min(TFS1_MAX))),
        TFS2_MAGIC => {
            let dirsec = (u32::from_le_bytes(s0[8..12].try_into().unwrap()) as usize).clamp(1, 32);
            Some((Tfs::V2 { dirsec }, count))
        }
        _ => None,
    }
}

/// One directory entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry { pub name: [u8; NAMELEN], pub lba: u32, pub len: u32, pub dir: bool }
pub const E0: Entry = Entry { name: [0; NAMELEN], lba: 0, len: 0, dir: false };

impl Entry {
    /// The name without its NUL padding.
    pub fn name(&self) -> &[u8] {
        let n = self.name.iter().position(|&c| c == 0).unwrap_or(NAMELEN);
        &self.name[..n]
    }
}

/// Entry `i` of the directory held in `dir` (the directory sectors, read
/// contiguously). `None` — the entry lies beyond the directory.
pub fn tfs_entry(dir: &[u8], kind: Tfs, i: usize) -> Option<Entry> {
    let u32at = |o: usize| u32::from_le_bytes(dir[o..o + 4].try_into().unwrap());
    let mut e = E0;
    match kind {
        Tfs::V1 => {
            let o = 8 + i * 32;
            if i >= TFS1_MAX || o + 32 > dir.len().min(SECTOR) { return None; }
            e.name[..24].copy_from_slice(&dir[o..o + 24]);
            e.lba = u32at(o + 24);
            e.len = u32at(o + 28);
        }
        Tfs::V2 { dirsec } => {
            let o = 16 + i * 96;
            if o + 96 > (dirsec * SECTOR).min(dir.len()) { return None; }
            e.name.copy_from_slice(&dir[o..o + NAMELEN]);
            e.lba = u32at(o + 84);
            e.len = u32at(o + 88);
            e.dir = dir[o + 92] & 1 != 0;
        }
    }
    Some(e)
}

/// Normalise a path for TFS lookup: drop leading and repeated '/', "./"
/// components and a trailing '/'; the path ends at its first NUL. Returns the
/// length written to `out`.
pub fn normalize(path: &[u8], out: &mut [u8; NAMELEN]) -> usize {
    let mut n = 0; let mut i = 0;
    let plen = path.iter().position(|&c| c == 0).unwrap_or(path.len());
    let p = &path[..plen];
    while i < p.len() && n < NAMELEN {
        if p[i] == b'/' { i += 1; continue; }
        if p[i] == b'.' && (i + 1 == p.len() || p[i + 1] == b'/') { i += 1; continue; }
        while i < p.len() && p[i] != b'/' && n < NAMELEN { out[n] = p[i]; n += 1; i += 1; }
        if i < p.len() && n < NAMELEN { out[n] = b'/'; n += 1; }
    }
    while n > 0 && out[n - 1] == b'/' { n -= 1; }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn norm(p: &str) -> String {
        let mut out = [0u8; NAMELEN];
        let n = normalize(p.as_bytes(), &mut out);
        String::from_utf8(out[..n].to_vec()).unwrap()
    }

    #[test]
    fn normalize_paths() {
        assert_eq!(norm("/etc/hosts"), "etc/hosts");
        assert_eq!(norm("//etc///hosts/"), "etc/hosts");
        assert_eq!(norm("./etc/./hosts"), "etc/hosts");
        assert_eq!(norm("/"), "");
        assert_eq!(norm(""), "");
        assert_eq!(norm("a\0garbage"), "a");
        assert_eq!(norm(".hidden/x"), ".hidden/x", "a dot file is not a ./ component");
        assert_eq!(norm("../x"), "../x", "parent references are kept as is");
    }

    #[test]
    fn normalize_never_overflows() {
        let long = "d/".repeat(200);
        let mut out = [0u8; NAMELEN];
        assert!(normalize(long.as_bytes(), &mut out) <= NAMELEN);
    }

    /// A disk as a map of sectors; unknown sectors read as zeros, `bad` ones fail.
    struct Disk { s: HashMap<usize, [u8; SECTOR]>, bad: Vec<usize> }
    impl Disk {
        fn new() -> Self { Disk { s: HashMap::new(), bad: vec![] } }
        fn sec(&mut self, lba: usize) -> &mut [u8; SECTOR] { self.s.entry(lba).or_insert([0; SECTOR]) }
        fn read(&self, lba: usize, b: &mut [u8; SECTOR]) -> bool {
            if self.bad.contains(&lba) { return false; }
            *b = *self.s.get(&lba).unwrap_or(&[0; SECTOR]);
            true
        }
    }

    fn gpt(entries: &[(u64, &str)]) -> Disk {
        let mut d = Disk::new();
        let h = d.sec(1);
        h[..8].copy_from_slice(b"EFI PART");
        h[72..80].copy_from_slice(&2u64.to_le_bytes());       // entry array at LBA 2
        h[80..84].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        for (i, (first, name)) in entries.iter().enumerate() {
            let (sec, off) = (2 + i * 128 / SECTOR, (i * 128) % SECTOR);
            let s = d.sec(sec);
            s[off + 32..off + 40].copy_from_slice(&first.to_le_bytes());
            for (k, c) in name.bytes().enumerate() { s[off + 56 + 2 * k] = c; }
        }
        d
    }

    #[test]
    fn gpt_named_data_partition_wins_over_position() {
        let d = gpt(&[(34, "bios"), (2048, "EFI"), (8192, "talaria-data")]);
        assert_eq!(data_partition(|l, b| d.read(l, b)), Some((8192, PartFrom::GptNamed)));
    }

    #[test]
    fn gpt_without_name_falls_back_to_the_second_entry() {
        let d = gpt(&[(2048, "EFI"), (4096, "data")]);
        assert_eq!(data_partition(|l, b| d.read(l, b)), Some((4096, PartFrom::GptSecond)));
    }

    #[test]
    fn gpt_name_must_match_exactly_in_utf16() {
        let d = gpt(&[(2048, "EFI"), (4096, "talaria-datX")]);
        assert_eq!(data_partition(|l, b| d.read(l, b)).map(|x| x.1), Some(PartFrom::GptSecond));
    }

    #[test]
    fn gpt_with_bad_entry_size_is_not_trusted() {
        let mut d = gpt(&[(2048, "EFI"), (4096, "talaria-data")]);
        d.sec(1)[84..88].copy_from_slice(&64u32.to_le_bytes());
        assert_eq!(data_partition(|l, b| d.read(l, b)), None);
    }

    #[test]
    fn gpt_entry_sector_read_error_stops_the_scan() {
        let mut d = gpt(&[(2048, "EFI"), (4096, "talaria-data")]);
        d.bad.push(2);
        assert_eq!(data_partition(|l, b| d.read(l, b)), None);
    }

    #[test]
    fn mbr_second_entry() {
        let mut d = Disk::new();
        let s = d.sec(0);
        s[510] = 0x55; s[511] = 0xAA;
        let e = 446 + 16;
        s[e + 4] = 0x83;
        s[e + 8..e + 12].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert_eq!(data_partition(|l, b| d.read(l, b)), Some((1_000_000, PartFrom::Mbr)));
        d.sec(0)[e + 4] = 0; // empty entry type
        assert_eq!(data_partition(|l, b| d.read(l, b)), None);
    }

    #[test]
    fn bare_volume_has_no_partition() {
        let d = Disk::new();
        assert_eq!(data_partition(|l, b| d.read(l, b)), None);
    }

    #[test]
    fn tfs1_directory() {
        let mut s = vec![0u8; SECTOR];
        s[..4].copy_from_slice(&TFS1_MAGIC.to_le_bytes());
        s[4..8].copy_from_slice(&40u32.to_le_bytes()); // claims more than fits
        s[8..8 + 9].copy_from_slice(b"hello.txt");
        s[8 + 24..8 + 28].copy_from_slice(&7u32.to_le_bytes());
        s[8 + 28..8 + 32].copy_from_slice(&13u32.to_le_bytes());
        let (kind, n) = tfs_header(&s).unwrap();
        assert_eq!((kind, n), (Tfs::V1, TFS1_MAX));
        let e = tfs_entry(&s, kind, 0).unwrap();
        assert_eq!((e.name(), e.lba, e.len, e.dir), (&b"hello.txt"[..], 7, 13, false));
        assert!(tfs_entry(&s, kind, TFS1_MAX).is_none());
    }

    #[test]
    fn tfs2_directory_spans_sectors_and_marks_directories() {
        let dirsec = 2;
        let mut d = vec![0u8; dirsec * SECTOR];
        d[..4].copy_from_slice(&TFS2_MAGIC.to_le_bytes());
        d[4..8].copy_from_slice(&10u32.to_le_bytes());
        d[8..12].copy_from_slice(&(dirsec as u32).to_le_bytes());
        let put = |d: &mut Vec<u8>, i: usize, name: &str, lba: u32, len: u32, dir: bool| {
            let o = 16 + i * 96;
            d[o..o + name.len()].copy_from_slice(name.as_bytes());
            d[o + 84..o + 88].copy_from_slice(&lba.to_le_bytes());
            d[o + 88..o + 92].copy_from_slice(&len.to_le_bytes());
            d[o + 92] = dir as u8;
        };
        put(&mut d, 0, "cfg", 0, 0, true);
        put(&mut d, 5, "cfg/app.ini", 9, 100, false); // entry 5 lies in the second sector
        let (kind, n) = tfs_header(&d).unwrap();
        assert_eq!((kind, n), (Tfs::V2 { dirsec }, 10));
        assert!(tfs_entry(&d, kind, 0).unwrap().dir);
        let e = tfs_entry(&d, kind, 5).unwrap();
        assert_eq!((e.name(), e.lba, e.len, e.dir), (&b"cfg/app.ini"[..], 9, 100, false));
        // 16 + 10 * 96 > 1024: entries beyond the directory sectors do not exist
        assert!(tfs_entry(&d, kind, 10).is_none());
    }

    #[test]
    fn tfs2_directory_size_is_clamped() {
        let mut s = vec![0u8; SECTOR];
        s[..4].copy_from_slice(&TFS2_MAGIC.to_le_bytes());
        s[8..12].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(tfs_header(&s).unwrap().0, Tfs::V2 { dirsec: 32 });
        s[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(tfs_header(&s).unwrap().0, Tfs::V2 { dirsec: 1 });
    }

    #[test]
    fn unknown_magic_is_not_tfs() {
        assert!(tfs_header(&[0u8; SECTOR]).is_none());
    }
}
