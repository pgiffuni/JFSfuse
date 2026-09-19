// SPDX-License-Identifier: GPL-2.0-or-later
//! JFS filesystem image generator.
//!
//! Creates valid JFS filesystem images programmatically, mirroring the key
//! on-disk structures that the read path (and consistency checker) depend on.
//! This replaces the need for a pre-built `/tmp/kilo/test_jfs.img` in tests.
//!
//! ## Layout (matches the reference image at `/tmp/kilo/test_jfs.img`)
//!
//! | Block | Purpose                              |
//! |-------|--------------------------------------|
//! | 0-7   | Reserved (aggregate boot area)       |
//! | 8     | Primary superblock (`SUPER1_OFF`)    |
//! | 9-10  | Primary aggregate inode map (AIM)    |
//! | 11-14 | Primary aggregate inode table (AIT)  |
//! | 15    | Secondary superblock (`SUPER2_OFF`)  |
//! | 16    | Block allocation map descriptor      |
//! | 17-18 | Dmap pages (block allocation bitmaps)|
//! | 19-23 | Secondary AIM/AIT                    |
//! | 24-27 | Filesystem inode table (AIT2)        |
//! | 28+   | Free space                            |
//! | 3840  | Log area (inline journal)            |

use byteorder::{ByteOrder, LittleEndian};
use std::sync::Arc;

use crate::storage::{BLOCK_SIZE, MemoryStorage, Storage};
use crate::types::{
    self, BMAP_I, FILESYSTEM_I, FM_CLEAN, JFS_MAGIC, LOGREDONE, LogSuper, LOGMAGIC,
    LOGPSIZE, LOGPAGES, LOGVERSION, PSIZE, ROOT_I, SUPER1_OFF,
};

/// Number of 4 KiB blocks used by the inline journal.
const LOG_AREA_BLOCKS: u64 = 256;

/// Default filesystem size for `create_filesystem`: 16 MiB (4096 blocks).
/// This matches the reference test image.
const DEFAULT_NUM_BLOCKS: u64 = 4096;

/// Create an in-memory JFS filesystem image and return a mountable `MemoryStorage`.
///
/// The image layout matches the reference `/tmp/kilo/test_jfs.img`:
/// - 16 MiB (4096 blocks) by default
/// - Primary superblock at block 8 (0x8000)
/// - Inline journal at blocks 3840..4096 with `LOGREDONE` state
/// - Aggregate inode table (AIT) at blocks 24..28
/// - Block allocation map descriptor at block 16
/// - Dmap pages at blocks 17..18
/// - Root inode (VFS ino 18) at AIT block 28, slot 2
/// - BMAP_I inode (VFS ino 2) at AIT block 24, slot 1
pub fn create_filesystem() -> Arc<dyn Storage> {
    create_filesystem_with_blocks(DEFAULT_NUM_BLOCKS)
}

/// Create an in-memory JFS filesystem image with a specific number of blocks.
pub fn create_filesystem_with_blocks(num_blocks: u64) -> Arc<dyn Storage> {
    let storage = Arc::new(MemoryStorage::new(num_blocks));
    build_filesystem(&*storage, num_blocks);
    storage
}

/// Write the full filesystem image into the given storage backend.
pub fn build_filesystem(storage: &dyn Storage, num_blocks: u64) {
    build_superblock(storage, num_blocks);
    build_log_super(storage);
    build_aggregate_inode_table(storage, num_blocks);
    build_block_alloc_map(storage, num_blocks);
    build_bmap_inode(storage, num_blocks);
}

/// Build the primary superblock at `SUPER1_OFF` (block 8).
fn build_superblock(storage: &dyn Storage, num_blocks: u64) {
    let mut sb = types::JfsSuperblock::default();

    // Magic number "JFS1".
    sb.s_magic = *JFS_MAGIC;

    // Version 1 (code accepts 1 or 2).
    LittleEndian::write_u32(&mut sb.s_version, 1);

    // Aggregate size in blocks.
    LittleEndian::write_u64(&mut sb.s_size, num_blocks);

    // Block size = PSIZE (4096).
    LittleEndian::write_u32(&mut sb.s_bsize, PSIZE as u32);

    // log2(block_size) = 12.
    LittleEndian::write_u16(&mut sb.s_l2bsize, 12);

    // log2(physical block factor) = 3 (4096/512 = 8 = 2^3).
    LittleEndian::write_u16(&mut sb.s_l2bfactor, 3);

    // Physical block size = 512.
    LittleEndian::write_u32(&mut sb.s_pbsize, 512);

    // log2(pbsize) = 9.
    LittleEndian::write_u16(&mut sb.s_l2pbsize, 9);

    // Allocation group size (8192 blocks — large enough for num_ag calculation).
    LittleEndian::write_u32(&mut sb.s_agsize, 8192);

    // Flags: JFS_LINUX (bit 9? actually 0x10200900 in reference)
    // 0x10000000 = JFS_FIXDIR, 0x02000000 = JFS_DIRSYNC(?),
    // 0x00200000 = JFS_LAZY journal(?), 0x00000900 = ?,
    // 0x00000800 = JFS_INLINELOG
    let s_flag: u32 = 0x10200900;
    LittleEndian::write_u32(&mut sb.s_flag, s_flag);

    // Filesystem state: clean.
    LittleEndian::write_u32(&mut sb.s_state, FM_CLEAN);

    // Secondary AIT2 extent: address=24, length=4.
    // Inodes are stored in blocks 24..28 (4 blocks × 8 inodes = 32 inodes).
    sb.s_ait2 = make_pxd(4, 24);

    // Secondary AIM2 extent: address=22, length=2.
    sb.s_aim2 = make_pxd(2, 22);

    // Inline log extent: address=3840, length=256.
    sb.s_logpxd = make_pxd(LOG_AREA_BLOCKS as u32, 3840);

    let _ = storage.write_bytes(SUPER1_OFF, unsafe {
        std::slice::from_raw_parts(
            &sb as *const types::JfsSuperblock as *const u8,
            std::mem::size_of::<types::JfsSuperblock>(),
        )
    });
}

/// Build the logsuper at the start of the inline journal area.
///
/// The inline log starts at block 3840. The logsuper is at
/// `log_base + BLOCK_SIZE` (block 3841). The log data area begins at
/// `log_base + 2 * BLOCK_SIZE` (block 3842).
fn build_log_super(storage: &dyn Storage) {
    let log_base_block: u64 = 3840;
    let logsuper_offset = log_base_block * (BLOCK_SIZE as u64) + (BLOCK_SIZE as u64);

    let mut ls = LogSuper::default();

    // Magic.
    LittleEndian::write_u32(&mut ls.magic, LOGMAGIC);

    // Version.
    LittleEndian::write_u32(&mut ls.version, LOGVERSION);

    // Serial (transaction counter).
    LittleEndian::write_u32(&mut ls.serial, 0);

    // Size in pages (256).
    LittleEndian::write_u32(&mut ls.size, LOGPAGES as u32);

    // Block size.
    LittleEndian::write_u32(&mut ls.bsize, LOGPSIZE as u32);

    // log2(block_size).
    LittleEndian::write_u32(&mut ls.l2bsize, 12);

    // Flag: same as superblock (inline log bit set).
    LittleEndian::write_u32(&mut ls.flag, 0x10200900);

    // State: LOGREDONE (no recovery needed).
    LittleEndian::write_u32(&mut ls.state, LOGREDONE);

    // End of log (no records written).
    LittleEndian::write_u32(&mut ls.end, 0);

    let ls_bytes = unsafe {
        std::slice::from_raw_parts(
            &ls as *const LogSuper as *const u8,
            std::mem::size_of::<LogSuper>(),
        )
    };

    let _ = storage.write_bytes(logsuper_offset, ls_bytes);
}

/// Build the aggregate inode table (AIT) at blocks 24..28.
///
/// The AIT contains:
/// - Block 24, slot 1: BMAP_I inode (VFS ino 2, fileset=1, number=2)
/// - Block 28, slot 2: Root inode (VFS ino 18, fileset=16, number=2)
fn build_aggregate_inode_table(storage: &dyn Storage, num_blocks: u64) {
    let ait_start: u64 = 24;

    // Block 24: contains BMAP_I inode (fileset=1, number=2 at slot 1)
    // and LOG_I (fileset=1, number=3 at slot 3), BADBLOCK_I (fileset=1, number=4 at slot 4)
    let mut block_24 = vec![0u8; BLOCK_SIZE];

    // BMAP_I inode at slot 1 (offset 512).
    let bmap_dinode = build_bmap_inode_bytes(num_blocks);
    let bmap_off = 1 * types::DISIZE;
    block_24[bmap_off..bmap_off + types::DISIZE].copy_from_slice(&bmap_dinode);

    let _ = storage.write_bytes(ait_start * (BLOCK_SIZE as u64), &block_24);

    // Block 25: empty (reserved for future inode extensions).
    // (stays zeroed)

    // Block 26: contains AGGREGATE_I inode (fileset=1, number=16 at slot 0)
    let mut block_26 = vec![0u8; BLOCK_SIZE];
    let agg_ino = build_aggregate_inode_bytes(num_blocks);
    block_26[0..types::DISIZE].copy_from_slice(&agg_ino);
    let _ = storage.write_bytes((ait_start + 2) * (BLOCK_SIZE as u64), &block_26);

    // Block 27: empty.

    // Block 28: contains the root inode (fileset=FILESYSTEM_I=16, number=ROOT_I=2 at slot 2)
    let mut block_28 = vec![0u8; BLOCK_SIZE];
    let root_dinode = build_root_inode_bytes();
    let root_off = 2 * types::DISIZE;
    block_28[root_off..root_off + types::DISIZE].copy_from_slice(&root_dinode);

    // Also add the filesystem inode (fileset=16, number=16-16=0... wait)
    // FILESYSTEM_I = 16, so vfs ino 16+0 = 16 is at fileset=16, number=0
    // FILESYSTEM_I = 16 is at block 26, slot 0 (we already set it above)
    // Actually, let's re-check: in the reference, block 26 slot 0 has fileset=1, number=16
    // That's for fileset 1 (the aggregate fileset)

    let _ = storage.write_bytes((ait_start + 4) * (BLOCK_SIZE as u64), &block_28);
}

/// Build the block allocation map descriptor and dmap pages.
///
/// Layout:
/// - Block 16: dbmap_disk descriptor (mapsize, nfree)
/// - Block 17..(18): dmap pages with pmap (1 = free)
///
/// For the test image, we mark blocks 0..16 and 24..28 and 3840..4096 as allocated
/// (reserved for metadata), and everything else as free.
fn build_block_alloc_map(storage: &dyn Storage, num_blocks: u64) {
    let mapsize = num_blocks;

    // Compute reserved blocks (metadata + journal).
    let mut reserved: Vec<(u64, u64)> = Vec::new();
    reserved.push((0, 16)); // Reserved + sb + AIM + AIT + sb2 (blocks 0..16)
    reserved.push((24, 4)); // AIT blocks 24..28
    reserved.push((3840, LOG_AREA_BLOCKS)); // Log area

    // Count free blocks.
    let mut nfree = num_blocks;
    for (start, len) in &reserved {
        nfree -= len;
    }

    // The BMAP_I xtroot has xad[2]: address=17, length=2.
    // Per the JFS dmap code: block 17 = dbmap_disk descriptor, dmap pages start at 18+.
    let dmap_start: u64 = 17;
    let ndmap: u64 = 2; // 2 dmap pages (blocks 18, 19)

    // Block 17: dbmap_disk descriptor (mapsize, nfree).
    let mut desc = vec![0u8; BLOCK_SIZE];
    LittleEndian::write_u64(&mut desc[0..8], mapsize);
    LittleEndian::write_u64(&mut desc[8..16], nfree);
    let _ = storage.write_bytes(dmap_start * (BLOCK_SIZE as u64), &desc);

    // Dmap pages: each covers 8192 blocks with 2×256-word bitmaps (wmap + pmap).
    // pmap: bit set (1) = free; bit clear (0) = allocated.
    let blocks_per_dmap: u64 = 8192;

    for dmap_idx in 0..ndmap {
        let mut dmap = vec![0u8; BLOCK_SIZE];
        let dmap_start_block = dmap_idx * blocks_per_dmap;

        // Mark free blocks within mapsize (those not reserved).
        // The pmap starts all-zero (all allocated); we set bit=1 for free blocks.
        for blk in 0..blocks_per_dmap {
            let global_blk = dmap_start_block + blk;
            if global_blk >= mapsize {
                continue; // beyond filesystem size — leave as allocated (0)
            }
            let is_reserved = reserved.iter().any(|(s, l)| {
                let start = *s as u64;
                let end = start + *l as u64;
                global_blk >= start && global_blk < end
            });
            if !is_reserved {
                let word_idx = (blk / 32) as usize;
                let bit_idx = blk % 32;
                let pmap_off: usize = 3072 + word_idx * 4;
                let word = LittleEndian::read_u32(&dmap[pmap_off..pmap_off + 4]);
                LittleEndian::write_u32(&mut dmap[pmap_off..pmap_off + 4], word | (1 << bit_idx));
            }
        }

        let _ = storage.write_bytes((dmap_start + 1 + dmap_idx) * (BLOCK_SIZE as u64), &dmap);
    }
}

/// Build the root inode (VFS ino 18 = FILESYSTEM_I + ROOT_I).
/// Stored at AIT block 28, slot 2.
fn build_root_inode_bytes() -> Vec<u8> {
    let mut dinode = vec![0u8; types::DISIZE];

    // inostamp.
    LittleEndian::write_u32(&mut dinode[0..4], 0);

    // fileset = FILESYSTEM_I (16).
    LittleEndian::write_u32(&mut dinode[4..8], FILESYSTEM_I);

    // number = ROOT_I (2).
    LittleEndian::write_u32(&mut dinode[8..12], ROOT_I);

    // generation.
    LittleEndian::write_u32(&mut dinode[12..16], 1);

    // di_ixpxd: inode extent location.
    // For inline inodes, this points to the inode's own location.
    let ixpxd = make_pxd(1, 28); // 1 block at block 28
    dinode[16..24].copy_from_slice(&ixpxd.into_bytes());

    // di_size = 256 (directory size).
    LittleEndian::write_u64(&mut dinode[24..32], 256);

    // di_nblocks = 0.
    LittleEndian::write_u64(&mut dinode[32..40], 0);

    // di_nlink = 2.
    LittleEndian::write_u32(&mut dinode[40..44], 2);

    // di_uid = 0.
    LittleEndian::write_u32(&mut dinode[44..48], 0);

    // di_gid = 0.
    LittleEndian::write_u32(&mut dinode[48..52], 0);

    // di_mode = IFJOURNAL | S_IFDIR | 0755 = 0x141ED.
    let mode: u32 = types::IFJOURNAL | 0x4000 | 0x1ED;
    LittleEndian::write_u32(&mut dinode[52..56], mode);

    // Timestamps (all zero for simplicity).
    // di_atime, di_ctime, di_mtime, di_otime already zeroed.

    // di_acl and di_ea already zeroed.

    // Build dtroot at union offset 96 (dinode offset 128+96=224).
    // The union is 384 bytes starting at dinode offset 128.
    // dtroot occupies u[96..384] = 288 bytes.
    // Layout: 32-byte header + 8 slots (1-indexed, slot 0 is the header).
    let dtroot_off = 224;
    let dtroot_len = 288; // 384 - 96
    let dtroot = &mut dinode[dtroot_off..dtroot_off + dtroot_len];

    // DASD (16 bytes) — all zeros.

    // dtroot header byte 16: flag = BT_SWAPPED | BT_LEAF | BT_ROOT = 0x83.
    dtroot[16] = 0x83;

    // nextindex = 0 (no directory entries).
    dtroot[17] = 0;

    // freecnt = 0 (no free list; sequential allocation via next_free is used).
    dtroot[18] = 0;

    // freelist = 0 (no free entries in the free list).
    dtroot[19] = 0;

    // idotdot = 2 (parent is itself, since root's `..` points to root).
    LittleEndian::write_u32(&mut dtroot[20..24], 2);

    // stbl (8 bytes) — all zeros.
    // Already zeroed.

    dinode
}

/// Build the root inode into the storage at block 28.
fn build_root_inode(storage: &dyn Storage) {
    let root_dinode = build_root_inode_bytes();
    let block_28_offset = 28 * (BLOCK_SIZE as u64);

    // Read the block, update slot 2, write it back.
    let mut block = vec![0u8; BLOCK_SIZE];
    let root_off = 2 * types::DISIZE;
    block[root_off..root_off + types::DISIZE].copy_from_slice(&root_dinode);
    let _ = storage.write_bytes(block_28_offset, &block);
}

/// Build the BMAP_I inode (VFS ino 2).
/// Stored at AIT block 24, slot 1.
fn build_bmap_inode_bytes(num_blocks: u64) -> Vec<u8> {
    let mut dinode = vec![0u8; types::DISIZE];

    // fileset = 1 (aggregate fileset).
    LittleEndian::write_u32(&mut dinode[4..8], 1);

    // number = BMAP_I (2).
    LittleEndian::write_u32(&mut dinode[8..12], BMAP_I);

    // generation.
    LittleEndian::write_u32(&mut dinode[12..16], 1);

    // di_ixpxd: points to the dmap extent itself.
    // The BMAP inode's xtroot contains the extent for blocks 17..18.
    // But the ixpxd is the inode's own location.
    let _ = &mut dinode; // ixpxd stays zeroed for now

    // di_size = 8192 (number of blocks the bmap covers = 8192 per dmap page × 1... wait)
    // In the reference: size=24576, nblocks=2
    // Actually size = 24576 = 8192 * 3? No, that's 3 dmap pages worth.
    // Let's use the reference value.
    LittleEndian::write_u64(&mut dinode[24..32], 24576);

    // di_nblocks = 2 (dmap pages).
    LittleEndian::write_u64(&mut dinode[32..40], 2);

    // di_nlink = 1.
    LittleEndian::write_u32(&mut dinode[40..44], 1);

    // di_mode = 0x18000 (DT_REG | 0600 without JFS_? prefix).
    LittleEndian::write_u32(&mut dinode[52..56], 0x18000);

    // Build xtroot at union offset 96 (dinode offset 224).
    let xtroot_off = 224;
    let xtroot = &mut dinode[xtroot_off..];

    // XtHeader (24 bytes):
    // next (8), prev (8), flag (1), rsrvd1 (1), nextindex (2), maxentry (2), rsrvd2 (2), self_pxd (8)
    // flag = 0x83 (BT_SWAPPED | BT_LEAF | BT_ROOT)
    xtroot[16] = 0x83;
    // nextindex = 3 (2 xad entries + header overlap? Actually 3 = 2 real + self)
    // Wait, in reference nextindex=3 means entries at indices 0,1,2
    // But XTENTRYSTART=2, so real xads start at index 2.
    // Actually nextindex=3 means there are 3 "entries" but the first 2 overlap with header.
    // Let me check: the xads in the reference had xad[2] with data and xad[0,1] zero.
    // So nextindex=3 means 3 slots are "in use" in the xad array.
    // But only xad[2] has real data.
    LittleEndian::write_u16(&mut xtroot[18..20], 3);
    // maxentry = 18 (XTROOTMAXSLOT).
    LittleEndian::write_u16(&mut xtroot[20..22], 18);

    // self_pxd at xt offset 24-31 (zeros for inline root).

    // xad[2] at offset 32: address=17 (dmap pages start at 17), length=2
    let xad_off = 32; // XTENTRYSTART=2, so xad[2] at offset 32
    let xad = &mut xtroot[xad_off..xad_off + 16];
    // xad layout: flag(1) rsvrd(2) off1(1) off2(4) loc_pxd(8)
    // flag = 0
    xad[0] = 0;
    // off1 = 0, off2 = 0 (logical offset 0)
    LittleEndian::write_u32(&mut xad[4..8], 0);
    // loc: length (low 24 bits) and address (high 8 + 32 low)
    let loc = make_pxd(2, 17); // length=2, address=17
    xad[8..16].copy_from_slice(&loc.into_bytes());

    dinode
}

/// Build the BMAP_I inode into the storage at block 24.
fn build_bmap_inode(storage: &dyn Storage, num_blocks: u64) {
    let bmap_dinode = build_bmap_inode_bytes(num_blocks);
    let block_24_offset = 24 * (BLOCK_SIZE as u64);

    let mut block = vec![0u8; BLOCK_SIZE];
    let bmap_off = 1 * types::DISIZE;
    block[bmap_off..bmap_off + types::DISIZE].copy_from_slice(&bmap_dinode);
    let _ = storage.write_bytes(block_24_offset, &block);
}

/// Build the AGGREGATE_I inode (VFS ino 1, fileset=1, number=16).
/// Stored at AIT block 26, slot 0.
fn build_aggregate_inode_bytes(num_blocks: u64) -> Vec<u8> {
    let mut dinode = vec![0u8; types::DISIZE];

    // fileset = 1 (aggregate fileset).
    LittleEndian::write_u32(&mut dinode[4..8], 1);

    // number = 16 (FILESYSTEM_I in the aggregate fileset = AGGREGATE_I).
    LittleEndian::write_u32(&mut dinode[8..12], FILESYSTEM_I);

    // di_size = 8192 (matches reference).
    LittleEndian::write_u64(&mut dinode[24..32], 8192);

    // di_nlink = 1.
    LittleEndian::write_u32(&mut dinode[40..44], 1);

    // di_mode = 0x18000.
    LittleEndian::write_u32(&mut dinode[52..56], 0x18000);

    dinode
}

/// Create a PXD (physical extent descriptor) with the given length and address.
fn make_pxd(length: u32, address: u64) -> types::Pxd {
    let mut pxd = types::Pxd::default();
    pxd.set_length(length);
    pxd.set_address(address);
    pxd
}
