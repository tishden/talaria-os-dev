//! The ext2 superblock: the volume's geometry, read from the 1024 bytes at the
//! offset 1024.
//!
//! Why a separate module. Everything the FS does afterwards is counted from
//! these numbers: where the group descriptors lie, which block an inode is in,
//! how many groups to walk. A volume whose superblock says "0 blocks per group"
//! is not a rare thing — that is what a half-written image, a wrong partition or
//! somebody else's file system looks like — and the arithmetic then either
//! divides by zero or wraps round and sends the FS into blocks that do not
//! exist. The refusal has to happen here, at the reading, and it can be judged
//! by tests without a disk.
//!
//! The fields (the offsets are from the start of the superblock):
//! ```text
//!    4  u32 s_blocks_count        12 u32 s_free_blocks_count
//!   16  u32 s_free_inodes_count   20 u32 s_first_data_block
//!   24  u32 s_log_block_size      32 u32 s_blocks_per_group
//!   40  u32 s_inodes_per_group    56 u16 s_magic (0xEF53)
//!   76  u32 s_rev_level           88 u16 s_inode_size (rev >= 1)
//!   96  u32 s_feature_incompat
//! ```

#[inline]
fn rd32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
#[inline]
fn rd16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }

/// The magic of an ext2/3/4 superblock.
pub const MAGIC: u16 = 0xEF53;
/// The incompatible features we can live with: `filetype` (0x2) and
/// `needs_recovery` (0x4 — the journal is replayed at the mount).
pub const FEAT_OK: u32 = 0x6;

/// The volume's geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geom {
    /// The block in bytes.
    pub block: usize,
    /// Sectors (512 B) per block.
    pub spb: usize,
    /// The volume's blocks in total.
    pub total_blocks: u32,
    /// The first block covered by the maps (1 with a block of 1 KB, 0 otherwise).
    pub first_data: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    /// The size of an inode's record (128 in rev 0).
    pub inode_size: usize,
    /// How many groups the volume has.
    pub ngroups: usize,
    /// The block of the group-descriptor table.
    pub gdt_block: u32,
    pub free_blocks: u32,
    pub free_inodes: u32,
    pub feat_incompat: u32,
}

/// Why the volume is not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    /// There is no 0xEF53 at the offset 56: this is not an ext2 superblock.
    NotExt2,
    /// The block is larger than the cache's slot.
    BlockTooBig(usize),
    /// The geometry does not add up (a zero group, a volume shorter than its first block,
    /// an inode's record that does not fit into a block).
    Geometry,
    /// Incompatible features besides `filetype`/`needs_recovery`.
    Features(u32),
}

/// Read the geometry out of a superblock. `max_block` is the largest block the
/// cache can hold.
pub fn parse(sb: &[u8], max_block: usize) -> Result<Geom, Bad> {
    if sb.len() < 100 || rd16(sb, 56) != MAGIC { return Err(Bad::NotExt2); }
    let log = rd32(sb, 24);
    if log > 16 { return Err(Bad::Geometry); }
    let block = 1024usize << log;
    if block > max_block { return Err(Bad::BlockTooBig(block)); }

    let total_blocks = rd32(sb, 4);
    let first_data = rd32(sb, 20);
    let blocks_per_group = rd32(sb, 32);
    let inodes_per_group = rd32(sb, 40);
    let rev = rd32(sb, 76);
    let inode_size = if rev >= 1 { rd16(sb, 88) as usize } else { 128 };

    // The checks that the arithmetic below relies on. Without them a corrupt
    // superblock is not a refusal but a division by zero or a wrap-round.
    if blocks_per_group == 0 || inodes_per_group == 0 { return Err(Bad::Geometry); }
    if total_blocks <= first_data { return Err(Bad::Geometry); }
    if inode_size < 128 || inode_size > block || !inode_size.is_power_of_two() { return Err(Bad::Geometry); }

    let feat_incompat = rd32(sb, 96);
    if feat_incompat & !FEAT_OK != 0 { return Err(Bad::Features(feat_incompat)); }

    Ok(Geom {
        block,
        spb: block / 512,
        total_blocks,
        first_data,
        blocks_per_group,
        inodes_per_group,
        inode_size,
        ngroups: (total_blocks - first_data).div_ceil(blocks_per_group) as usize,
        gdt_block: first_data + 1,
        free_blocks: rd32(sb, 12),
        free_inodes: rd32(sb, 16),
        feat_incompat,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wr32(b: &mut [u8], o: usize, v: u32) { b[o..o + 4].copy_from_slice(&v.to_le_bytes()); }
    fn wr16(b: &mut [u8], o: usize, v: u16) { b[o..o + 2].copy_from_slice(&v.to_le_bytes()); }

    /// A superblock of a volume as `mkfs.ext2` lays it out: `blocks` blocks of
    /// `1024 << log` bytes.
    fn sb(blocks: u32, log: u32, bpg: u32, ipg: u32, rev: u32, isz: u16) -> [u8; 1024] {
        let mut b = [0u8; 1024];
        wr16(&mut b, 56, MAGIC);
        wr32(&mut b, 4, blocks);
        wr32(&mut b, 12, blocks / 2);            // free blocks
        wr32(&mut b, 16, 1000);                  // free inodes
        wr32(&mut b, 20, if log == 0 { 1 } else { 0 });
        wr32(&mut b, 24, log);
        wr32(&mut b, 32, bpg);
        wr32(&mut b, 40, ipg);
        wr32(&mut b, 76, rev);
        wr16(&mut b, 88, isz);
        wr32(&mut b, 96, 0x2);                   // filetype
        b
    }

    /// The usual volume of the regression: 64 MB, a block of 1 KB, rev 1.
    fn ok_sb() -> [u8; 1024] { sb(65536, 0, 8192, 2048, 1, 256) }

    #[test]
    fn a_healthy_volume_is_read_field_by_field() {
        let g = parse(&ok_sb(), 4096).expect("a healthy superblock");
        assert_eq!(g.block, 1024);
        assert_eq!(g.spb, 2);
        assert_eq!(g.total_blocks, 65536);
        assert_eq!(g.first_data, 1, "with a block of 1 KB the maps start at the block 1");
        assert_eq!(g.gdt_block, 2);
        assert_eq!(g.blocks_per_group, 8192);
        assert_eq!(g.inodes_per_group, 2048);
        assert_eq!(g.inode_size, 256);
        assert_eq!(g.ngroups, 8, "(65536 - 1) / 8192, rounded up");
        assert_eq!(g.free_blocks, 32768);
        assert_eq!(g.free_inodes, 1000);
    }

    #[test]
    fn a_block_of_four_kilobytes_starts_the_maps_at_zero() {
        let g = parse(&sb(16384, 2, 32768, 8192, 1, 256), 4096).expect("4K");
        assert_eq!(g.block, 4096);
        assert_eq!(g.spb, 8);
        assert_eq!(g.first_data, 0);
        assert_eq!(g.gdt_block, 1);
        assert_eq!(g.ngroups, 1);
    }

    #[test]
    fn the_groups_are_rounded_up_rather_than_down() {
        // one block over two whole groups is still a third group
        let g = parse(&sb(2 * 8192 + 1, 2, 8192, 2048, 1, 256), 4096).unwrap();
        assert_eq!(g.ngroups, 3);
        // exactly two groups are two
        let g2 = parse(&sb(2 * 8192, 2, 8192, 2048, 1, 256), 4096).unwrap();
        assert_eq!(g2.ngroups, 2);
    }

    #[test]
    fn a_revision_zero_volume_has_an_inode_of_128_bytes() {
        // rev 0: the field s_inode_size does not exist, whatever lies there
        let g = parse(&sb(65536, 0, 8192, 2048, 0, 9999), 4096).unwrap();
        assert_eq!(g.inode_size, 128);
    }

    #[test]
    fn a_foreign_superblock_is_not_taken() {
        let mut b = ok_sb();
        wr16(&mut b, 56, 0x1234);
        assert_eq!(parse(&b, 4096), Err(Bad::NotExt2));
        assert_eq!(parse(&[0u8; 1024], 4096), Err(Bad::NotExt2));
        assert_eq!(parse(&[], 4096), Err(Bad::NotExt2), "a short buffer is not a superblock");
    }

    #[test]
    fn a_block_larger_than_the_cache_slot_is_refused_by_its_size() {
        let b = sb(16384, 3, 32768, 8192, 1, 256);   // 8 KB
        assert_eq!(parse(&b, 4096), Err(Bad::BlockTooBig(8192)));
        assert!(parse(&b, 8192).is_ok(), "the same volume is taken by a cache that holds it");
    }

    #[test]
    fn an_impossible_block_size_is_a_geometry_error_rather_than_an_overflow() {
        let b = sb(16384, 40, 32768, 8192, 1, 256);  // 1024 << 40 does not fit into usize
        assert_eq!(parse(&b, 4096), Err(Bad::Geometry));
    }

    #[test]
    fn a_zero_group_is_refused_instead_of_dividing_by_zero() {
        assert_eq!(parse(&sb(65536, 0, 0, 2048, 1, 256), 4096), Err(Bad::Geometry));
        assert_eq!(parse(&sb(65536, 0, 8192, 0, 1, 256), 4096), Err(Bad::Geometry));
    }

    #[test]
    fn a_volume_shorter_than_its_first_block_is_refused_instead_of_wrapping_round() {
        // s_blocks_count = 1 with s_first_data_block = 1: there is not a single block of data
        assert_eq!(parse(&sb(1, 0, 8192, 2048, 1, 256), 4096), Err(Bad::Geometry));
        assert_eq!(parse(&sb(0, 0, 8192, 2048, 1, 256), 4096), Err(Bad::Geometry));
    }

    #[test]
    fn an_inode_record_that_does_not_fit_a_block_is_refused() {
        assert_eq!(parse(&sb(65536, 0, 8192, 2048, 1, 2048), 4096), Err(Bad::Geometry),
                   "an inode of 2 KB does not fit into a block of 1 KB");
        assert_eq!(parse(&sb(65536, 0, 8192, 2048, 1, 64), 4096), Err(Bad::Geometry),
                   "an inode shorter than the ext2 record");
        assert_eq!(parse(&sb(65536, 0, 8192, 2048, 1, 300), 4096), Err(Bad::Geometry),
                   "the record's size is a power of two");
    }

    #[test]
    fn the_features_we_cannot_honour_are_named_in_the_refusal() {
        let mut b = ok_sb();
        wr32(&mut b, 96, 0x40);                  // meta_bg
        assert_eq!(parse(&b, 4096), Err(Bad::Features(0x40)));
        wr32(&mut b, 96, 0x2 | 0x4);             // filetype + needs_recovery
        assert!(parse(&b, 4096).is_ok(), "a journal awaiting a replay is not a refusal");
    }

    /// Everything the FS counts afterwards must hold together: the groups cover
    /// the volume, and the last group is not empty.
    #[test]
    fn the_groups_cover_the_volume_exactly() {
        for &(blocks, log, bpg) in &[(65536u32, 0u32, 8192u32), (16384, 2, 32768), (20000, 2, 8192), (8193, 0, 8192)] {
            let g = parse(&sb(blocks, log, bpg, 2048, 1, 256), 4096).unwrap();
            let covered = g.total_blocks - g.first_data;
            let last = crate::ext2bmap::group_bits(g.total_blocks, g.first_data, g.blocks_per_group, g.ngroups - 1);
            assert!(last > 0, "the last group of the volume {} is empty", blocks);
            assert_eq!((g.ngroups - 1) as u32 * g.blocks_per_group + last as u32, covered,
                       "the groups do not cover the volume {}", blocks);
        }
    }
}
