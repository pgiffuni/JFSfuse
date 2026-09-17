// SPDX-License-Identifier: GPL-2.0-or-later
//! On-disk type definitions for JFS.
//!
//! All structures are `#[repr(C, packed)]` with explicit little-endian field
//! access, preserving binary compatibility with the Linux JFS on-disk format
//! (see `fs/jfs/jfs_types.h`, `jfs_filsys.h`, `jfs_dinode.h`, `jfs_xtree.h`,
//! `jfs_dtree.h`, `jfs_imap.h`, `jfs_dmap.h`, `jfs_logmgr.h`).
//!
//! Field accessors (`length`, `address`, etc.) mirror the C macros
//! (`lengthPXD`, `addressPXD`, `lengthXAD`, `offsetXAD`) using Rust method
//! names while preserving the exact bit-level semantics.
//!
//! ## Provenance
//!
//! | Rust type | C header | On-disk size |
//! |-----------|----------|-------------|
//! | [`Pxd`] | `jfs_types.h:pxd_t` | 8 bytes |
//! | [`Dxd`] | `jfs_types.h:dxd_t` | 16 bytes |
//! | [`Timestruc`] | `jfs_types.h:timestruc_t` | 8 bytes |
//! | [`Dinode`] | `jfs_dinode.h:dinode` | 512 bytes |
//! | [`Xad`] | `jfs_xtree.h:xad_t` | 16 bytes |
//! | [`XtHeader`] | `jfs_xtree.h:xtheader` | 24 bytes |
//! | [`XtRoot`] | `jfs_xtree.h:xtroot_t` | 288 bytes |
//! | [`BtPage`] | `jfs_btree.h:btpage` | 4096 bytes |
//! | [`DtSlot`] | `jfs_dtree.h:dtslot` | 32 bytes |
//! | [`LdtEntry`] | `jfs_dtree.h:ldtentry` | 32 bytes |
//! | [`LogSuper`] | `jfs_logmgr.h:logsuper` | 208+ bytes |
//! | [`Logpage`] | `jfs_logmgr.h:logpage` | 4096 bytes |
//! | [`Lrd`] | `jfs_logmgr.h:lrd` | 36 bytes |
//! | [`Iag`] | `jfs_imap.h:iag` | 4096 bytes |
//! | [`Dmap`] | `jfs_dmap.h:dmap` | 4096 bytes |
//! | [`JfsSuperblock`] | `jfs_superblock.h:jfs_superblock` | 256 bytes |

use byteorder::{ByteOrder, LittleEndian};

// ──────────────────────────── Constants ────────────────────────────
// From jfs_filsys.h — filesystem block layout constants.

pub const PSIZE: usize = 4096;
pub const L2PSIZE: u8 = 12;
pub const PBSIZE: usize = 512;
pub const L2PBSIZE: u8 = 9;

pub const DISIZE: usize = 512;
pub const L2DISIZE: u8 = 9;
pub const INODESLOTSIZE: usize = 128;
pub const L2INODESLOTSIZE: u8 = 7;
pub const IDATASIZE: usize = 256;
pub const IXATTRSIZE: usize = 128;

// Inode allocation map (from jfs_imap.h): each IAG manages 4096 inodes
// in 128 extents of 32 inodes each.
pub const IAG_SIZE: usize = 4096;
pub const INOSPERIAG: u32 = 4096;
pub const L2INOSPERIAG: u8 = 12;
pub const INOSPEREXT: u32 = 32;
pub const L2INOSPEREXT: u8 = 5;
pub const IXSIZE: usize = DISIZE * (INOSPEREXT as usize);
pub const INOSPERPAGE: u32 = 8;
pub const L2INOSPERPAGE: u8 = 3;

// B+-tree common constants (from jfs_btree.h).
pub const MAXTREEHEIGHT: usize = 8;
pub const EXTSPERIAG: usize = 128;
pub const EXTSPERIAG_I: usize = 128;

// Log constants (from jfs_logmgr.h): log page size, max active volumes.
pub const LOGPSIZE: usize = 4096;
pub const L2LOGPSIZE: u8 = 12;
pub const LOGPAGES: usize = 16;

pub const LOGMAGIC: u32 = 0x87654321;
pub const LOGVERSION: u32 = 1;
pub const MAX_ACTIVE: usize = 128;

pub const JFS_MAGIC: &[u8; 4] = b"JFS1";
pub const JFS_VERSION: u32 = 2;

// Superblock locations (from jfs_filsys.h): fixed at block offsets 64–112.
pub const SUPER1_B: u64 = 64;
pub const AIMAP_B: u64 = SUPER1_B + 8;
pub const AITBL_B: u64 = AIMAP_B + 16;
pub const SUPER2_B: u64 = AITBL_B + 32;
pub const BMAP_B: u64 = SUPER2_B + 8;

// Superblock byte offsets within the aggregate.
pub const SUPER1_OFF: u64 = 0x8000;
pub const AIMAP_OFF: u64 = SUPER1_OFF + (PSIZE as u64);
pub const AITBL_OFF: u64 = AIMAP_OFF + (PSIZE as u64) * 2;
pub const SUPER2_OFF: u64 = AITBL_OFF + (IXSIZE as u64);
pub const BMAP_OFF: u64 = SUPER2_OFF + (PSIZE as u64);

pub const AGGR_RSVD_BLOCKS: u64 = SUPER1_B;
pub const AGGR_RSVD_BYTES: u64 = SUPER1_OFF;
pub const AGGR_INODE_TABLE_START: u64 = AITBL_OFF;

// Reserved inode numbers (from jfs_filsys.h).
pub const AGGR_RESERVED_I: u32 = 0;
pub const AGGREGATE_I: u32 = 1;
pub const BMAP_I: u32 = 2;
pub const LOG_I: u32 = 3;
pub const BADBLOCK_I: u32 = 4;
pub const FILESET_RSVD_I: u32 = 0;
pub const FILESET_EXT_I: u32 = 1;
pub const ROOT_I: u32 = 2;
pub const ACL_I: u32 = 3;
pub const FILESET_OBJECT_I: u32 = 4;
pub const FILESYSTEM_I: u32 = 16;

// Filesystem state flags (from jfs_filsys.h: FM_*).
pub const FM_CLEAN: u32 = 0x00000000;
pub const FM_MOUNT: u32 = 0x00000001;
pub const FM_DIRTY: u32 = 0x00000002;
pub const FM_LOGREDO: u32 = 0x00000004;
pub const FM_EXTENDFS: u32 = 0x00000008;

// Log superblock state values (from jfs_logmgr.h).
pub const LOGMOUNT: u32 = 0;
pub const LOGREDONE: u32 = 1;
pub const LOGWRAP: u32 = 2;
pub const LOGREADERR: u32 = 3;

// Log record types (lrd.type, from jfs_logmgr.h).
pub const LOG_COMMIT: u16 = 0x8000;
pub const LOG_SYNCPT: u16 = 0x4000;
pub const LOG_MOUNT: u16 = 0x2000;
pub const LOG_REDOPAGE: u16 = 0x0800;
pub const LOG_NOREDOPAGE: u16 = 0x0080;
pub const LOG_NOREDOINOEXT: u16 = 0x0040;
pub const LOG_UPDATEMAP: u16 = 0x0008;
pub const LOG_NOREDOFILE: u16 = 0x0001;

// REDOPAGE data type flags (from jfs_logmgr.h).
pub const LOG_INODE: u16 = 0x0001;
pub const LOG_XTREE: u16 = 0x0002;
pub const LOG_DTREE: u16 = 0x0004;
pub const LOG_BTROOT: u16 = 0x0010;
pub const LOG_EA: u16 = 0x0020;
pub const LOG_ACL: u16 = 0x0040;
pub const LOG_DATA: u16 = 0x0080;
pub const LOG_NEW: u16 = 0x0100;
pub const LOG_EXTEND: u16 = 0x0200;
pub const LOG_RELOCATE: u16 = 0x0400;
pub const LOG_DIR_XTREE: u16 = 0x0800;

// Mode extended bits (from jfs_dinode.h: high 16 bits of di_mode).
pub const IFJOURNAL: u32 = 0x00010000;
pub const ISPARSE: u32 = 0x00020000;
pub const INLINEEA: u32 = 0x00040000;
pub const ISWAPFILE: u32 = 0x00800000;

/// Block number type — in filesystem blocks (4096-byte units), not physical blocks.
pub type BlockNo = u64;

/// Block length type — count in filesystem blocks.
pub type BlockLength = u64;

/// Number of bytes per filesystem block (PSIZE from jfs_filsys.h).
pub const BLOCK_SIZE: usize = PSIZE;

// ──────────────────────── pxd_t: physical extent descriptor ────────────────────────
// From jfs_types.h. Encodes a 40-bit block address and a 24-bit length in 8 bytes.
// The high 8 bits of `len_addr` hold the upper byte of the address; the low
// 24 bits hold the length. `addr2` holds the low 32 bits of the address.
// Max extent: 2^24 - 1 blocks (≈64 GB at 4 KB). Max volume: 2^40 blocks (≈4 PiB).

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Pxd {
    pub len_addr: [u8; 4],
    pub addr2: [u8; 4],
}

impl Pxd {
    pub fn new() -> Self {
        Self::default()
    }

    /// Extent length in filesystem blocks (low 24 bits of `len_addr`).
    pub fn length(&self) -> u32 {
        let l = LittleEndian::read_u32(&self.len_addr);
        l & 0xffffff
    }

    pub fn set_length(&mut self, len: u32) {
        let l = LittleEndian::read_u32(&self.len_addr);
        let masked = l & !0xffffff;
        let new_val = masked | (len & 0xffffff);
        LittleEndian::write_u32(&mut self.len_addr, new_val);
    }

    /// Physical block address (40-bit, zero-extended to u64).
    pub fn address(&self) -> u64 {
        let l = LittleEndian::read_u32(&self.len_addr) as u64;
        let high = (l & !0xffffff) << 8;
        let low = LittleEndian::read_u32(&self.addr2) as u64;
        high + low
    }

    pub fn set_address(&mut self, addr: u64) {
        let high_addr = (addr >> 32) as u32;
        let low_addr = addr as u32;
        let l = LittleEndian::read_u32(&self.len_addr);
        let reserved = l & 0xffffff;
        let new_len_addr = reserved | ((high_addr << 24) & !0xffffff);
        LittleEndian::write_u32(&mut self.len_addr, new_len_addr);
        LittleEndian::write_u32(&mut self.addr2, low_addr);
    }

    /// Serialize to the 8-byte on-disk representation.
    pub fn into_bytes(self) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        bytes[..4].copy_from_slice(&self.len_addr);
        bytes[4..].copy_from_slice(&self.addr2);
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut p = Self::default();
        if bytes.len() >= 8 {
            p.len_addr.copy_from_slice(&bytes[..4]);
            p.addr2.copy_from_slice(&bytes[4..8]);
        }
        p
    }
}

// ──────────────────────── dxd_t: data extent descriptor ────────────────────────
// From jfs_types.h. Used for ACL, extended attribute, and symlink descriptors.
// 16 bytes: 1-byte flag, 3-byte reserved, 4-byte size (LE), then an embedded Pxd.

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct Dxd {
    pub flag: u8,
    pub rsrvd: [u8; 3],
    pub size: [u8; 4],
    pub loc: Pxd,
}

bitflags::bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct DxdFlag: u8 {
        const DXD_INDEX   = 0x80;
        const DXD_INLINE  = 0x40;
        const DXD_EXTENT  = 0x20;
        const DXD_FILE    = 0x10;
        const DXD_CORRUPT = 0x08;
    }
}

impl Dxd {
    pub fn length(&self) -> u32 {
        LittleEndian::read_u32(&self.size)
    }

    pub fn set_length(&mut self, len: u32) {
        LittleEndian::write_u32(&mut self.size, len);
    }

    pub fn loc_length(&self) -> u32 {
        self.loc.length()
    }

    pub fn loc_address(&self) -> u64 {
        self.loc.address()
    }

    pub fn set_loc(&mut self, length: u32, address: u64) {
        self.loc = Pxd::default();
        self.loc.set_length(length);
        self.loc.set_address(address);
    }
}

// ──────────────────────── xad_t: xtree extent descriptor ────────────────────────
// From jfs_xtree.h. Each entry maps a logical file offset to a physical extent.
// 16 bytes: 1-byte flag, 2-byte reserved, 1-byte off1 + 4-byte off2 (LE) for a
// split 40-bit extent offset, then a Pxd for the physical location.
// The offset is split across off1 (high 8 bits) and off2 (low 32 bits).

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct Xad {
    pub flag: u8,
    pub rsvrd: [u8; 2],
    pub off1: u8,
    pub off2: [u8; 4],
    pub loc: Pxd,
}

bitflags::bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct XadFlag: u8 {
        const XAD_NEW        = 0x01;
        const XAD_EXTENDED   = 0x02;
        const XAD_COMPRESSED = 0x04;
        const XAD_NOTRECORDED = 0x08;
        const XAD_COW        = 0x10;
    }
}

impl Xad {
    /// Logical file offset in fsblocks (40-bit, from the split off1/off2 fields).
    pub fn offset(&self) -> i64 {
        let high = (self.off1 as u64) << 32;
        let low = LittleEndian::read_u32(&self.off2);
        (high | low as u64) as i64
    }

    pub fn set_offset(&mut self, offset: i64) {
        let val = offset as u64;
        self.off1 = ((val >> 32) & 0xff) as u8;
        LittleEndian::write_u32(&mut self.off2, (val & 0xffffffff) as u32);
    }

    pub fn length(&self) -> u32 {
        self.loc.length()
    }

    pub fn set_length(&mut self, len: u32) {
        self.loc.set_length(len);
    }

    pub fn address(&self) -> u64 {
        self.loc.address()
    }

    pub fn set_address(&mut self, addr: u64) {
        self.loc.set_address(addr);
    }
}

// ──────────────────────── pxdlist ────────────────────────
// From jfs_types.h. A small array of up to MAXTREEHEIGHT (8) physical extents.

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct PxdList {
    pub maxnpxd: i16,
    pub npxd: i16,
    pub pxd: [Pxd; MAXTREEHEIGHT],
}

// ──────────────────────── timestruc_t ────────────────────────
// From jfs_types.h. On-disk timestamp: little-endian seconds + nanoseconds.
// Differs from Linux's timespec in using `__le32` for portability.

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct Timestruc {
    pub tv_sec: [u8; 4],
    pub tv_nsec: [u8; 4],
}

impl Timestruc {
    pub fn seconds(&self) -> u32 {
        LittleEndian::read_u32(&self.tv_sec)
    }

    pub fn set_seconds(&mut self, sec: u32) {
        LittleEndian::write_u32(&mut self.tv_sec, sec);
    }

    pub fn nanoseconds(&self) -> u32 {
        LittleEndian::read_u32(&self.tv_nsec)
    }

    pub fn set_nanoseconds(&mut self, nsec: u32) {
        LittleEndian::write_u32(&mut self.tv_nsec, nsec);
    }
}

// ──────────────────────── dinode ────────────────────────
// From jfs_dinode.h. The on-disk inode is always 512 bytes (DISIZE).
// Layout: 128-byte base area, then a 384-byte union that varies by file type:
//   - Directories: dtroot (B+-tree root, 9 slots × 32 bytes = 288 bytes)
//   - Regular files: xtroot (extent B+-tree root, 18 xad slots × 16 bytes = 288 bytes)
//   - Symlinks (<128 bytes): inline fast symlink data in the _special union area
// The `u` field is the raw union bytes; callers interpret it based on `is_dir()` /
// `is_regular()` / `is_symlink()`.

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Dinode {
    pub di_inostamp: [u8; 4],
    pub di_fileset: [u8; 4],
    pub di_number: [u8; 4],
    pub di_generation: [u8; 4],
    pub di_ixpxd: Pxd,
    pub di_size: [u8; 8],
    pub di_nblocks: [u8; 8],
    pub di_nlink: [u8; 4],
    pub di_uid: [u8; 4],
    pub di_gid: [u8; 4],
    pub di_mode: [u8; 4],
    pub di_atime: Timestruc,
    pub di_ctime: Timestruc,
    pub di_mtime: Timestruc,
    pub di_otime: Timestruc,
    pub di_acl: Dxd,
    pub di_ea: Dxd,
    pub di_next_index: [u8; 4],
    pub di_acltype: [u8; 4],
    pub u: [u8; 384],
}

impl Default for Dinode {
    fn default() -> Self {
        Self {
            di_inostamp: [0; 4],
            di_fileset: [0; 4],
            di_number: [0; 4],
            di_generation: [0; 4],
            di_ixpxd: Pxd::default(),
            di_size: [0; 8],
            di_nblocks: [0; 8],
            di_nlink: [0; 4],
            di_uid: [0; 4],
            di_gid: [0; 4],
            di_mode: [0; 4],
            di_atime: Timestruc::default(),
            di_ctime: Timestruc::default(),
            di_mtime: Timestruc::default(),
            di_otime: Timestruc::default(),
            di_acl: Dxd::default(),
            di_ea: Dxd::default(),
            di_next_index: [0; 4],
            di_acltype: [0; 4],
            u: [0; 384],
        }
    }
}

impl Dinode {
    /// On-disk size of a dinode (always 512 bytes, DISIZE).
    pub fn size() -> usize {
        std::mem::size_of::<Dinode>()
    }

    pub fn inostamp(&self) -> u32 {
        LittleEndian::read_u32(&self.di_inostamp)
    }

    pub fn fileset(&self) -> u32 {
        LittleEndian::read_u32(&self.di_fileset)
    }

    pub fn number(&self) -> u32 {
        LittleEndian::read_u32(&self.di_number)
    }

    pub fn generation(&self) -> u32 {
        LittleEndian::read_u32(&self.di_generation)
    }

    pub fn size_val(&self) -> u64 {
        LittleEndian::read_u64(&self.di_size)
    }

    pub fn nblocks(&self) -> u64 {
        LittleEndian::read_u64(&self.di_nblocks)
    }

    pub fn nlink(&self) -> u32 {
        LittleEndian::read_u32(&self.di_nlink)
    }

    pub fn uid(&self) -> u32 {
        LittleEndian::read_u32(&self.di_uid)
    }

    pub fn gid(&self) -> u32 {
        LittleEndian::read_u32(&self.di_gid)
    }

    pub fn mode(&self) -> u32 {
        LittleEndian::read_u32(&self.di_mode)
    }

    /// Whether this inode is a directory (S_IFDIR = 0x4000).
    pub fn is_dir(&self) -> bool {
        (self.mode() & 0xf000) == 0x4000
    }

    /// Whether this inode is a regular file (S_IFREG = 0x8000).
    pub fn is_regular(&self) -> bool {
        (self.mode() & 0xf000) == 0x8000
    }

    /// Whether this inode is a symlink (S_IFLNK = 0xa000).
    pub fn is_symlink(&self) -> bool {
        (self.mode() & 0xf000) == 0xa000
    }

    /// Xtree root bytes (xtroot_t) — at offset 96 within the 384-byte union.
    /// Valid for regular files and directories with inline xtree.
    pub fn xtroot_bytes(&self) -> &[u8] {
        &self.u[96..]
    }

    /// Dtree root bytes (dtroot_t) — the full 384-byte union area.
    /// Valid for directories.
    pub fn dtroot_bytes(&self) -> &[u8] {
        &self.u
    }

    /// Parse a raw 512-byte dinode from disk into the struct fields.
    /// Mirrors `copy_from_dinode()` in jfs_imap.c.
    pub fn parse(data: &[u8], dinode: &mut Dinode) -> std::result::Result<(), String> {
        if data.len() < std::mem::size_of::<Dinode>() {
            return Err("insufficient data for dinode".to_string());
        }
        dinode.di_inostamp = data[0..4].try_into().unwrap_or([0; 4]);
        dinode.di_fileset = data[4..8].try_into().unwrap_or([0; 4]);
        dinode.di_number = data[8..12].try_into().unwrap_or([0; 4]);
        dinode.di_generation = data[12..16].try_into().unwrap_or([0; 4]);
        dinode.di_ixpxd = Pxd::from_bytes(&data[16..24]);
        dinode.di_size = data[24..32].try_into().unwrap_or([0; 8]);
        dinode.di_nblocks = data[32..40].try_into().unwrap_or([0; 8]);
        dinode.di_nlink = data[40..44].try_into().unwrap_or([0; 4]);
        dinode.di_uid = data[44..48].try_into().unwrap_or([0; 4]);
        dinode.di_gid = data[48..52].try_into().unwrap_or([0; 4]);
        dinode.di_mode = data[52..56].try_into().unwrap_or([0; 4]);

        dinode.di_atime = Timestruc {
            tv_sec: data[56..60].try_into().unwrap_or([0; 4]),
            tv_nsec: data[60..64].try_into().unwrap_or([0; 4]),
        };
        dinode.di_ctime = Timestruc {
            tv_sec: data[64..68].try_into().unwrap_or([0; 4]),
            tv_nsec: data[68..72].try_into().unwrap_or([0; 4]),
        };
        dinode.di_mtime = Timestruc {
            tv_sec: data[72..76].try_into().unwrap_or([0; 4]),
            tv_nsec: data[76..80].try_into().unwrap_or([0; 4]),
        };
        dinode.di_otime = Timestruc {
            tv_sec: data[80..84].try_into().unwrap_or([0; 4]),
            tv_nsec: data[84..88].try_into().unwrap_or([0; 4]),
        };

        dinode.di_acl = Dxd {
            flag: data[88],
            rsrvd: [data[89], data[90], data[91]],
            size: data[92..96].try_into().unwrap_or([0; 4]),
            loc: Pxd::from_bytes(&data[96..104]),
        };
        dinode.di_ea = Dxd {
            flag: data[104],
            rsrvd: [data[105], data[106], data[107]],
            size: data[108..112].try_into().unwrap_or([0; 4]),
            loc: Pxd::from_bytes(&data[112..120]),
        };

        dinode.di_next_index = data[120..124].try_into().unwrap_or([0; 4]);
        dinode.di_acltype = data[124..128].try_into().unwrap_or([0; 4]);
        dinode.u = data[128..512].try_into().unwrap_or([0; 384]);

        Ok(())
    }
}

// ──────────────────────── xtree structures ────────────────────────
// From jfs_xtree.h. The extent B+-tree maps logical file offsets (in fsblocks)
// to physical disk block addresses. Max extent length: 2^24−1 blocks (MAXXLEN).

/// Maximum extent length (24-bit field limit from pxd_t).
pub const MAXXLEN: u32 = (1 << 24) - 1;
pub const XTSLOTSIZE: usize = 16;
pub const L2XTSLOTSIZE: u8 = 4;
/// Initial slots in a root xtree for directories (vs regular files).
pub const XTROOTINITSLOT_DIR: usize = 6;
/// Initial slots in a root xtree for regular files.
pub const XTROOTINITSLOT: usize = 10;
/// Maximum xad entries in an inline xtroot.
pub const XTROOTMAXSLOT: usize = 18;
/// Maximum xad entries per external xtree page.
pub const XTPAGEMAXSLOT: usize = 256;
/// First xad slot index that holds real data (first 2 overlap the header).
pub const XTENTRYSTART: usize = 2;

/// B+-tree page header for both xtree root and pages (jfs_xtree.h:xtheader).
/// 24 bytes. `next`/`prev` are sibling pointers; `nextindex` is the entry
/// count in use; `maxentry` is the slot capacity; `self` (pxd) is this page's
/// own disk extent.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct XtHeader {
    pub next: [u8; 8],
    pub prev: [u8; 8],
    pub flag: u8,
    pub rsrvd1: u8,
    pub nextindex: [u8; 2],
    pub maxentry: [u8; 2],
    pub rsrvd2: [u8; 2],
    pub self_pxd: Pxd,
}

impl XtHeader {
    pub fn set_next(&mut self, val: u64) {
        LittleEndian::write_u64(&mut self.next, val);
    }

    pub fn prev(&self) -> u64 {
        LittleEndian::read_u64(&self.prev)
    }

    pub fn set_prev(&mut self, val: u64) {
        LittleEndian::write_u64(&mut self.prev, val);
    }

    /// Number of entries currently in use (next free slot index).
    pub fn nextindex(&self) -> u16 {
        LittleEndian::read_u16(&self.nextindex)
    }

    pub fn set_nextindex(&mut self, val: u16) {
        LittleEndian::write_u16(&mut self.nextindex, val);
    }

    /// Maximum number of entries this page can hold.
    pub fn maxentry(&self) -> u16 {
        LittleEndian::read_u16(&self.maxentry)
    }

    pub fn set_maxentry(&mut self, val: u16) {
        LittleEndian::write_u16(&mut self.maxentry, val);
    }
}

/// Extent B+-tree root (xtroot_t) — stored inline in the inode's union area.
/// From jfs_xtree.h. A union of xtheader + xad array. For BT_ROOT pages,
/// `bt == 0` indicates the root is in the inode (no disk I/O needed).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct XtRoot {
    pub header: XtHeader,
    pub xad: [Xad; XTROOTMAXSLOT],
}

impl Default for XtRoot {
    fn default() -> Self {
        Self {
            header: XtHeader::default(),
            xad: [Xad::default(); XTROOTMAXSLOT],
        }
    }
}

impl XtRoot {
    pub fn is_leaf(&self) -> bool {
        self.header.flag & 0x02 != 0
    }

    pub fn is_root(&self) -> bool {
        self.header.flag & 0x01 != 0
    }

    pub fn next_index(&self) -> usize {
        self.header.nextindex() as usize
    }

    /// Get the xad entry at index `idx`, if in range. Real entries start at
    /// `XTENTRYSTART` (2) since the first two slots overlap the header.
    pub fn entry(&self, idx: usize) -> Option<&Xad> {
        if idx < XTROOTMAXSLOT && idx < self.next_index() {
            Some(&self.xad[idx])
        } else {
            None
        }
    }
}

// ──────────────────────── dtree structures ────────────────────────
// From jfs_dtree.h. The directory B+-tree maps filenames (UCS-2) to inode
// numbers. Uses 32-byte fixed slots; variable-length names may span multiple
// linked slots. Entries are kept sorted via a 1-byte-per-entry index table
// (stbl) for binary search.

/// Size of a directory page slot (dtslot, idtentry, ldtentry).
pub const DATASLOTSIZE: usize = 16;
pub const L2DATASLOTSIZE: u8 = 4;
/// Directory slot size (dtslot = 32 bytes).
pub const DTSLOTSIZE: usize = 32;
pub const L2DTSLOTSIZE: u8 = 5;
/// Header size within a slot (next + cnt = 2 bytes).
pub const DTSLOTHDRSIZE: usize = 2;
/// Bytes available for data in a slot (32 − 2 = 30).
pub const DTSLOTDATASIZE: usize = 30;
/// Number of u16 wchar units in a slot's data area.
pub const DTSLOTDATALEN: usize = 15;

/// First slot index for entries in a dtroot (slot[0] is the header).
pub const DTENTRYSTART: usize = 1;
/// Maximum slots in a dtroot page.
pub const DTROOTMAXSLOT: usize = 9;

/// Directory slot (dtslot) — 32 bytes. `next` links multi-slot entries;
/// `cnt` is the slot count used by this entry. `name` holds 15 UCS-2 chars.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct DtSlot {
    pub next: i8,
    pub cnt: i8,
    pub name: [u16; DTSLOTDATALEN],
}

/// Internal node entry (idtentry) — 32 bytes: child pxd + name prefix.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct IdtEntry {
    pub xd: Pxd,
    pub next: i8,
    pub name_len: u8,
    pub name: [u16; 11],
}

/// Leaf node entry (ldtentry) — 32 bytes: inode number + name + dir_table index.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct LdtEntry {
    pub inumber: [u8; 4],
    pub next: i8,
    pub name_len: u8,
    pub name: [u16; 11],
    pub index: [u8; 4],
}

// ──────────────────────── B+-tree common ────────────────────────
// From jfs_btree.h. All B+-trees (xtree, dtree) use a common page format:
// 16-byte header (next/prev sibling, flag, self-address) + 4064-byte entry area.

pub const BTPAGE_ENTRY_SIZE: usize = 4064;

/// Common B+-tree page header (jfs_btree.h:btpage).
/// 32 bytes: sibling links, flag (type/root/leaf/etc.), and self block address.
/// The `entry` array holds type-specific slot data (xad, dtslot, etc.).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct BtPage {
    pub next: [u8; 8],
    pub prev: [u8; 8],
    pub flag: u8,
    pub rsrvd: [u8; 7],
    pub self_addr: [u8; 8],
    pub entry: [u8; BTPAGE_ENTRY_SIZE],
}

impl Default for BtPage {
    fn default() -> Self {
        Self {
            next: [0; 8],
            prev: [0; 8],
            flag: 0,
            rsrvd: [0; 7],
            self_addr: [0; 8],
            entry: [0; BTPAGE_ENTRY_SIZE],
        }
    }
}

/// B+-tree page flags (jfs_btree.h). `BT_TYPE` (0x07) masks the page type.
bitflags::bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct BtFlag: u8 {
        const BT_TYPE      = 0x07;
        const BT_ROOT      = 0x01;
        const BT_LEAF      = 0x02;
        const BT_INTERNAL  = 0x04;
        const BT_RIGHTMOST = 0x10;
        const BT_LEFTMOST  = 0x20;
        const BT_SWAPPED   = 0x80;
    }
}

// ──────────────────────── logsuper ────────────────────────
// From jfs_logmgr.h. Located at disk block 1 (after the unused block 0).
// The log superblock records the log's magic, size, state, and active volumes.

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogSuper {
    pub magic: [u8; 4],
    pub version: [u8; 4],
    pub serial: [u8; 4],
    pub size: [u8; 4],
    pub bsize: [u8; 4],
    pub l2bsize: [u8; 4],
    pub flag: [u8; 4],
    pub state: [u8; 4],
    pub end: [u8; 4],
    pub uuid: [u8; 16],
    pub label: [u8; 16],
    pub active: [ActiveEntry; MAX_ACTIVE],
}

/// One entry in the logsuper active-volume table (jfs_logmgr.h).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ActiveEntry {
    pub uuid: [u8; 16],
}

impl Default for LogSuper {
    fn default() -> Self {
        Self {
            magic: [0; 4],
            version: [0; 4],
            serial: [0; 4],
            size: [0; 4],
            bsize: [0; 4],
            l2bsize: [0; 4],
            flag: [0; 4],
            state: [0; 4],
            end: [0; 4],
            uuid: [0; 16],
            label: [0; 16],
            active: [ActiveEntry::default(); MAX_ACTIVE],
        }
    }
}

impl LogSuper {
    pub fn magic_val(&self) -> u32 {
        LittleEndian::read_u32(&self.magic)
    }

    /// Validate magic against LOGMAGIC (0x87654321).
    pub fn is_valid(&self) -> bool {
        self.magic_val() == LOGMAGIC
    }

    /// Log format version (currently always 1 = LOGVERSION).
    pub fn version(&self) -> u32 {
        LittleEndian::read_u32(&self.version)
    }

    /// Total log size in pages.
    pub fn page_size(&self) -> u32 {
        LittleEndian::read_u32(&self.size)
    }

    /// Log block size in bytes (usually 4096).
    pub fn block_size(&self) -> u32 {
        LittleEndian::read_u32(&self.bsize)
    }

    /// Log superblock state: LOGMOUNT / LOGREDONE / LOGWRAP / LOGREADERR.
    pub fn state(&self) -> u32 {
        LittleEndian::read_u32(&self.state)
    }

    /// End-of-log byte offset within the log data area (last committed record).
    pub fn end(&self) -> u32 {
        LittleEndian::read_u32(&self.end)
    }

    /// Log flags (e.g., JFS_INLINELOG = 0x200, bit 9).
    pub fn flag(&self) -> u32 {
        LittleEndian::read_u32(&self.flag)
    }

    pub fn l2bsize_val(&self) -> u32 {
        LittleEndian::read_u32(&self.l2bsize)
    }

    pub fn set_state(&mut self, val: u32) {
        LittleEndian::write_u32(&mut self.state, val);
    }

    pub fn set_end(&mut self, val: u32) {
        LittleEndian::write_u32(&mut self.end, val);
    }
}

// ──────────────────────── logpage ────────────────────────
// From jfs_logmgr.h. 4096-byte log page: header (8 bytes), data array,
// and trailer (4 bytes). XOR integrity: XOR all 32-bit words in `data`;
// the upper 16 bits go in the header `eor`, the lower 16 bits in the trailer
// `eor`. The trailer `page` field must match the header `page` field.

pub const LOG_PAGE_DATA_WORDS: usize = LOGPSIZE / 4 - 4;

/// Log page with XOR integrity check (jfs_logmgr.h:logpage).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Logpage {
    pub h: LogpageHeader,
    pub data: [[u8; 4]; LOG_PAGE_DATA_WORDS],
    pub t: LogpageTrailer,
}

impl Default for Logpage {
    fn default() -> Self {
        Self {
            h: LogpageHeader::default(),
            data: [[0u8; 4]; LOG_PAGE_DATA_WORDS],
            t: LogpageTrailer::default(),
        }
    }
}

/// Log page header (jfs_logmgr.h). 8 bytes: page number + end-of-record offset.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct LogpageHeader {
    pub page: [u8; 4],
    pub rsrvd: [u8; 2],
    pub eor: [u8; 2],
}

/// Log page trailer (jfs_logmgr.h). Mirrors header for XOR validation.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct LogpageTrailer {
    pub page: [u8; 4],
    pub rsrvd: [u8; 2],
    pub eor: [u8; 2],
}

impl Logpage {
    pub fn h_page(&self) -> u32 {
        LittleEndian::read_u32(&self.h.page)
    }

    pub fn set_h_page(&mut self, val: u32) {
        LittleEndian::write_u32(&mut self.h.page, val);
    }

    pub fn h_eor(&self) -> u16 {
        LittleEndian::read_u16(&self.h.eor)
    }

    pub fn set_h_eor(&mut self, val: u16) {
        LittleEndian::write_u16(&mut self.h.eor, val);
    }

    pub fn t_page(&self) -> u32 {
        LittleEndian::read_u32(&self.t.page)
    }

    pub fn t_eor(&self) -> u16 {
        LittleEndian::read_u16(&self.t.eor)
    }

    pub fn data_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.data.as_ptr() as *const u8, LOGPSIZE - 16) }
    }
}

// ──────────────────────── lrd (log record descriptor) ────────────────────────
// From jfs_logmgr.h. Each log record is `[data][lrd]` (36 bytes). The `type`
// field selects the interpretation: LOG_COMMIT, LOG_REDOPAGE, LOG_NOREDOPAGE,
// LOG_NOREDOINOEXT, LOG_UPDATEMAP, LOG_SYNCPT, LOG_MOUNT. `backchain` links
// records within a transaction (0 = last record). The union fields
// (redopage_*) are only meaningful for REDOPAGE/NOREDOPAGE types.

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Lrd {
    pub logtid: [u8; 4],
    pub backchain: [u8; 4],
    pub r#type: [u8; 2],
    pub length: [u8; 2],
    pub aggregate: [u8; 4],
    pub redopage_fileset: [u8; 4],
    pub redopage_inode: [u8; 4],
    pub redopage_type: [u8; 2],
    pub redopage_l2linesize: [u8; 2],
    pub redopage_pxd: Pxd,
}

impl Default for Lrd {
    fn default() -> Self {
        Self {
            logtid: [0; 4],
            backchain: [0; 4],
            r#type: [0; 2],
            length: [0; 2],
            aggregate: [0; 4],
            redopage_fileset: [0; 4],
            redopage_inode: [0; 4],
            redopage_type: [0; 2],
            redopage_l2linesize: [0; 2],
            redopage_pxd: Pxd::default(),
        }
    }
}

impl Lrd {
    /// Transaction ID (logtid) this record belongs to.
    pub fn logtid(&self) -> u32 {
        LittleEndian::read_u32(&self.logtid)
    }

    /// Backchain pointer; 0 means this is the last record in the transaction.
    pub fn backchain(&self) -> u32 {
        LittleEndian::read_u32(&self.backchain)
    }

    /// Record type (LOG_COMMIT, LOG_REDOPAGE, etc.).
    pub fn r#type(&self) -> u16 {
        LittleEndian::read_u16(&self.r#type)
    }

    /// Length of the data payload that follows this LRD (in bytes).
    pub fn length(&self) -> u16 {
        LittleEndian::read_u16(&self.length)
    }

    /// Aggregating filesystem number for multi-volume logs.
    pub fn aggregate(&self) -> u32 {
        LittleEndian::read_u32(&self.aggregate)
    }

    /// Fileset number (from redopage union).
    pub fn fileset(&self) -> u32 {
        LittleEndian::read_u32(&self.redopage_fileset)
    }

    /// Target inode number (from redopage union).
    pub fn inode(&self) -> u32 {
        LittleEndian::read_u32(&self.redopage_inode)
    }

    /// Page type for REDOPAGE records (LOG_INODE, LOG_XTREE, LOG_DTREE, etc.).
    pub fn redopage_type(&self) -> u16 {
        LittleEndian::read_u16(&self.redopage_type)
    }

    pub fn redopage_l2linesize(&self) -> u16 {
        LittleEndian::read_u16(&self.redopage_l2linesize)
    }

    /// Target page location (pxd) for REDOPAGE/NOREDOPAGE records.
    pub fn redopage_pxd(&self) -> &Pxd {
        &self.redopage_pxd
    }

    /// Sync point address (reused redopage_fileset field for LOG_SYNCPT).
    pub fn syncpt_sync(&self) -> u32 {
        LittleEndian::read_u32(&self.redopage_fileset)
    }

    pub fn is_commit(&self) -> bool {
        self.r#type() & LOG_COMMIT != 0
    }

    pub fn is_syncpt(&self) -> bool {
        self.r#type() & LOG_SYNCPT != 0
    }

    pub fn is_redopage(&self) -> bool {
        self.r#type() & LOG_REDOPAGE != 0
    }

    pub fn is_noredopage(&self) -> bool {
        self.r#type() & LOG_NOREDOPAGE != 0
    }

    pub fn is_updatemap(&self) -> bool {
        self.r#type() & LOG_UPDATEMAP != 0
    }

    pub fn is_noredoinoext(&self) -> bool {
        self.r#type() & LOG_NOREDOINOEXT != 0
    }

    pub fn is_mount(&self) -> bool {
        self.r#type() & LOG_MOUNT != 0
    }
}

// ──────────────────────── jfs_superblock ────────────────────────
// From jfs_superblock.h. The aggregate superblock at fixed offset 0x8000.
// Magic "JFS1", version 2, block size must be PSIZE (4096). The superblock
// also carries inline log extent (s_logpxd), fsck work extent (s_fsckpxd),
// and secondary AIM/AIT extents for aggregate inode table.

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct JfsSuperblock {
    pub s_magic: [u8; 4],
    pub s_version: [u8; 4],
    pub s_size: [u8; 8],
    pub s_bsize: [u8; 4],
    pub s_l2bsize: [u8; 2],
    pub s_l2bfactor: [u8; 2],
    pub s_pbsize: [u8; 4],
    pub s_l2pbsize: [u8; 2],
    pub pad: [u8; 2],
    pub s_agsize: [u8; 4],
    pub s_flag: [u8; 4],
    pub s_state: [u8; 4],
    pub s_compress: [u8; 4],
    pub s_ait2: Pxd,
    pub s_aim2: Pxd,
    pub s_logdev: [u8; 4],
    pub s_logserial: [u8; 4],
    pub s_logpxd: Pxd,
    pub s_fsckpxd: Pxd,
    pub s_time: Timestruc,
    pub s_fsckloglen: [u8; 4],
    pub s_fscklog: u8,
    pub s_fpack: [u8; 11],
    pub s_xsize: [u8; 8],
    pub s_xfsckpxd: Pxd,
    pub s_xlogpxd: Pxd,
    pub s_uuid: [u8; 16],
    pub s_label: [u8; 16],
    pub s_loguuid: [u8; 16],
}

impl Default for JfsSuperblock {
    fn default() -> Self {
        Self {
            s_magic: [0; 4],
            s_version: [0; 4],
            s_size: [0; 8],
            s_bsize: [0; 4],
            s_l2bsize: [0; 2],
            s_l2bfactor: [0; 2],
            s_pbsize: [0; 4],
            s_l2pbsize: [0; 2],
            pad: [0; 2],
            s_agsize: [0; 4],
            s_flag: [0; 4],
            s_state: [0; 4],
            s_compress: [0; 4],
            s_ait2: Pxd::default(),
            s_aim2: Pxd::default(),
            s_logdev: [0; 4],
            s_logserial: [0; 4],
            s_logpxd: Pxd::default(),
            s_fsckpxd: Pxd::default(),
            s_time: Timestruc::default(),
            s_fsckloglen: [0; 4],
            s_fscklog: 0,
            s_fpack: [0; 11],
            s_xsize: [0; 8],
            s_xfsckpxd: Pxd::default(),
            s_xlogpxd: Pxd::default(),
            s_uuid: [0; 16],
            s_label: [0; 16],
            s_loguuid: [0; 16],
        }
    }
}

impl JfsSuperblock {
    /// Validate magic number (must be "JFS1").
    pub fn is_valid_magic(&self) -> bool {
        &self.s_magic == JFS_MAGIC
    }

    /// Filesystem version (currently 2 = JFS_VERSION).
    pub fn version(&self) -> u32 {
        LittleEndian::read_u32(&self.s_version)
    }

    /// Aggregate block size in bytes (must be PSIZE = 4096).
    pub fn block_size(&self) -> u32 {
        LittleEndian::read_u32(&self.s_bsize)
    }

    /// log2 of block size (L2PSIZE = 12).
    pub fn l2bsize(&self) -> u16 {
        LittleEndian::read_u16(&self.s_l2bsize)
    }

    /// log2 of physical block size (PBSIZE = 512).
    pub fn log2_pbsize(&self) -> u16 {
        LittleEndian::read_u16(&self.s_l2pbsize)
    }

    /// Allocation group size in blocks.
    pub fn agsize(&self) -> u32 {
        LittleEndian::read_u32(&self.s_agsize)
    }

    /// Filesystem flags (e.g., JFS_INLINELOG = 0x200, JFS_LINUX = bit 9).
    pub fn flag(&self) -> u32 {
        LittleEndian::read_u32(&self.s_flag)
    }

    /// Filesystem state (FM_CLEAN, FM_DIRTY, FM_LOGREDO, etc.).
    pub fn state(&self) -> u32 {
        LittleEndian::read_u32(&self.s_state)
    }

    pub fn set_state(&mut self, val: u32) {
        LittleEndian::write_u32(&mut self.s_state, val);
    }

    /// Whether the journal is stored inline within the filesystem.
    pub fn has_inline_log(&self) -> bool {
        self.flag() & 0x00000800 != 0
    }

    /// Total aggregate size in blocks.
    pub fn aggregate_size(&self) -> u64 {
        LittleEndian::read_u64(&self.s_size)
    }

    /// Inline journal extent (s_logpxd).
    pub fn inline_log_pxd(&self) -> &Pxd {
        &self.s_logpxd
    }

    /// FSCK work extent (s_fsckpxd).
    pub fn fsck_pxd(&self) -> &Pxd {
        &self.s_fsckpxd
    }

    /// Secondary aggregate inode map extent (s_aim2).
    pub fn aggregate_inode_map_pxd(&self) -> &Pxd {
        &self.s_aim2
    }

    /// Block allocation map extent (s_ait2).
    pub fn block_alloc_map_pxd(&self) -> &Pxd {
        &self.s_ait2
    }
}

// ──────────────────────── IAG (Inode Allocation Group) ────────────────────────
// From jfs_imap.h. Each IAG is a 4096-byte page managing 4096 inodes
// (INOSPERIAG) arranged as 128 extents (EXTSPERIAG) of 32 inodes each.
// Contains working/persistent allocation bitmaps and per-extent pxd descriptors
// pointing to the actual disk inode pages.

/// On-disk IAG (Inode Allocation Group) page — 4096 bytes.
/// From jfs_imap.h. Manages 128 inode extents (EXTSPERIAG = 128), each
/// covering 32 inodes. `wmap` is the working (transient) bitmap, `pmap`
/// is the persistent (committed) bitmap, and `inoext` holds the pxd
/// descriptors for each extent's disk page.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Iag {
    pub wmap: [[u8; 4]; EXTSPERIAG],
    pub pmap: [[u8; 4]; EXTSPERIAG],
    pub inoext: [Pxd; EXTSPERIAG],
}

impl Default for Iag {
    fn default() -> Self {
        Self {
            wmap: [[0u8; 4]; EXTSPERIAG],
            pmap: [[0u8; 4]; EXTSPERIAG],
            inoext: [Pxd::default(); EXTSPERIAG],
        }
    }
}

/// On-disk dmap (block allocation map page) — 4096 bytes.
/// From jfs_dmap.h. Manages 8192 blocks (BPERDMAP) as two 1024-word
/// bitmaps: `wmap` (working, transient) and `pmap` (persistent, committed).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Dmap {
    pub wmap: [[u8; 4]; 8192 / 4],
    pub pmap: [[u8; 4]; 8192 / 4],
}

impl Default for Dmap {
    fn default() -> Self {
        Self {
            wmap: [[0u8; 4]; 8192 / 4],
            pmap: [[0u8; 4]; 8192 / 4],
        }
    }
}

// ──────────────────────── Utility functions ────────────────────────
// Miscellaneous helpers shared across modules.

/// Compute LSN difference accounting for log wrapping (logdiff macro).
/// Returns `lsn - syncpt`, wrapping around modulo `logsize` if negative.
pub fn logdiff(syncpt: i64, lsn: i64, logsize: i64) -> i64 {
    let mut diff = lsn - syncpt;
    if diff < 0 {
        diff += logsize;
    }
    diff
}

// ──────────────────────── Tests ────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use byteorder::{ByteOrder, LittleEndian};

    #[test]
    fn test_pxd_length() {
        let mut p = Pxd::default();
        p.set_length(42);
        assert_eq!(p.length(), 42);

        p.set_length(0x123456);
        assert_eq!(p.length(), 0x123456);

        p.set_length(0xFFFFFF);
        assert_eq!(p.length(), 0xFFFFFF);

        p.set_length(0x1000000);
        assert_eq!(p.length(), 0x000000);
    }

    #[test]
    fn test_pxd_address() {
        // PXD stores 40-bit addresses: 8 bits in high byte of len_addr,
        // 32 bits in addr2. Full 64-bit values are truncated.
        let mut p = Pxd::default();
        p.set_address(0x00000078_9ABCDEF0);
        assert_eq!(p.address(), 0x00000078_9ABCDEF0);

        p.set_address(0x00000001_00000002);
        assert_eq!(p.address(), 0x00000001_00000002);

        p.set_address(0x00000003);
        assert_eq!(p.address() & 0xFFFFFF, 3);
        assert_eq!(p.length(), 0);
    }

    #[test]
    fn test_pxd_set_length_and_address() {
        let mut p = Pxd::default();
        p.set_length(0x123);
        p.set_address(0xFFFF);
        assert_eq!(p.length(), 0x123);
        assert_eq!(p.address(), 0xFFFF);
    }

    #[test]
    fn test_pxd_into_from_bytes() {
        let mut p = Pxd::default();
        p.set_length(100);
        p.set_address(200);

        let bytes = p.into_bytes();
        assert_eq!(bytes.len(), 8);

        let p2 = Pxd::from_bytes(&bytes);
        assert_eq!(p2.length(), 100);
        assert_eq!(p2.address(), 200);
    }

    #[test]
    fn test_dinode_size() {
        assert_eq!(std::mem::size_of::<Dinode>(), 512);
    }

    #[test]
    fn test_dinode_mode() {
        let mut d = Dinode::default();
        // 0x4000 = S_IFDIR, little-endian: bytes [0x00, 0x40, 0x00, 0x00]
        d.di_mode = [0x00, 0x40, 0x00, 0x00];
        assert!(d.is_dir());
        assert!(!d.is_regular());

        // 0x8000 = S_IFREG
        d.di_mode = [0x00, 0x80, 0x00, 0x00];
        assert!(d.is_regular());

        // 0xa000 = S_IFLNK
        d.di_mode = [0x00, 0xa0, 0x00, 0x00];
        assert!(d.is_symlink());
    }

    #[test]
    fn test_dinode_parse() {
        let mut data = vec![0u8; 512];
        LittleEndian::write_u32(&mut data[8..12], 42);
        LittleEndian::write_u64(&mut data[24..32], 1024);
        LittleEndian::write_u32(&mut data[52..56], 0x81a4);
        LittleEndian::write_u32(&mut data[40..44], 1);
        LittleEndian::write_u32(&mut data[44..48], 1000);

        let mut dinode = Dinode::default();
        Dinode::parse(&data, &mut dinode).unwrap();

        assert_eq!(dinode.number(), 42);
        assert_eq!(dinode.size_val(), 1024);
        assert!(dinode.is_regular());
        assert_eq!(dinode.nlink(), 1);
        assert_eq!(dinode.uid(), 1000);
    }

    #[test]
    fn test_xad_offset() {
        let mut xad = Xad::default();
        xad.set_offset(0x100000000);
        assert_eq!(xad.offset(), 0x100000000);

        xad.set_offset(42);
        assert_eq!(xad.offset(), 42);
    }

    #[test]
    fn test_xad_length_address() {
        let mut xad = Xad::default();
        xad.set_length(100);
        xad.set_address(200);
        assert_eq!(xad.length(), 100);
        assert_eq!(xad.address(), 200);
    }

    #[test]
    fn test_xad_size() {
        assert_eq!(std::mem::size_of::<Xad>(), 16);
    }

    #[test]
    fn test_lrd_type_flags() {
        let mut lrd = Lrd::default();

        // Set LRD-level type to LOG_COMMIT (0x8000)
        lrd.r#type = [0x00, 0x80];
        assert!(lrd.is_commit());
        assert!(!lrd.is_syncpt());
        assert!(!lrd.is_redopage());
        assert!(!lrd.is_noredopage());
        assert!(!lrd.is_updatemap());

        // Set LRD-level type to LOG_REDOPAGE (0x0800)
        lrd.r#type = [0x00, 0x08];
        assert!(!lrd.is_commit());
        assert!(lrd.is_redopage());

        // Set LRD-level type to LOG_REDOPAGE | LOG_BTROOT | LOG_XTREE
        lrd.r#type = [0x00, 0x08]; // LOG_REDOPAGE
        assert!(lrd.is_redopage());

        // REDOPAGE data type is in the union's redopage.type field
        lrd.redopage_type = [LOG_INODE as u8, (LOG_INODE >> 8) as u8];
        assert_eq!(lrd.redopage_type(), LOG_INODE);

        lrd.redopage_type = [LOG_BTROOT as u8, (LOG_BTROOT >> 8) as u8];
        assert_eq!(lrd.redopage_type(), LOG_BTROOT);
    }

    #[test]
    fn test_logsuper_validate() {
        let mut ls = LogSuper::default();
        assert!(!ls.is_valid());

        ls.magic = {
            let mut buf = [0u8; 4];
            LittleEndian::write_u32(&mut buf, LOGMAGIC);
            buf
        };
        assert!(ls.is_valid());
        assert_eq!(ls.magic_val(), LOGMAGIC);
    }

    #[test]
    fn test_logdiff_wrap() {
        assert_eq!(logdiff(0, 100, 1000), 100);
        assert_eq!(logdiff(900, 100, 1000), 200);
    }

    #[test]
    fn test_btree_flags() {
        let f = BtFlag::BT_ROOT | BtFlag::BT_LEAF;
        assert!(f.contains(BtFlag::BT_ROOT));
        assert!(f.contains(BtFlag::BT_LEAF));
        assert!(!f.contains(BtFlag::BT_INTERNAL));
    }

    #[test]
    fn test_xad_flags() {
        let f = XadFlag::XAD_NEW | XadFlag::XAD_EXTENDED;
        assert!(f.contains(XadFlag::XAD_NEW));
        assert!(f.contains(XadFlag::XAD_EXTENDED));
        assert!(!f.contains(XadFlag::XAD_COW));
    }

    #[test]
    fn test_dxd_flags() {
        let f = DxdFlag::DXD_INDEX | DxdFlag::DXD_INLINE;
        assert!(f.contains(DxdFlag::DXD_INDEX));
        assert!(f.contains(DxdFlag::DXD_INLINE));
        assert!(!f.contains(DxdFlag::DXD_EXTENT));
    }

    #[test]
    fn test_timestruc() {
        let mut ts = Timestruc::default();
        ts.set_seconds(1000);
        ts.set_nanoseconds(500);
        assert_eq!(ts.seconds(), 1000);
        assert_eq!(ts.nanoseconds(), 500);
    }

    #[test]
    fn test_xtroot_default() {
        let root = XtRoot::default();
        assert_eq!(root.next_index(), 0);
        assert!(!root.is_root());
        assert!(!root.is_leaf());
    }
}
