// SPDX-License-Identifier: GPL-2.0-or-later
//! On-disk inode access — mirrors `jfs_incore.h` / `jfs_inode.c`.
//!
//! Provides read/write access to disk inodes (dinode structures) and
//! translates between on-disk and runtime representations.

use crate::storage::{BLOCK_SIZE, Result as StorageResult, Storage};
use crate::types::{Dinode, INOSPERIAG, INOSPERPAGE, L2INOSPERPAGE, PSIZE};

/// Runtime inode representation.
///
/// This replaces `struct jfs_inode_info` / `struct inode` from the kernel.
/// In Rust, we don't have VFS integration, so this is a self-contained
/// structure holding both on-disk fields and runtime caches.
pub struct Inode {
    /// On-disk inode data.
    pub dinode: Dinode,
    /// Inode number.
    pub ino: u32,
    /// Block number containing this inode (in the aggregate).
    pub page_block: u64,
    /// Offset within the page.
    pub page_offset: usize,
}

impl Inode {
    /// Read an inode by number from the filesystem.
    ///
    /// Maps the inode number to its location on disk via the IAG/imap,
    /// then reads the dinode structure.
    pub fn read(volume: &mut crate::volume::Volume, ino: u32) -> StorageResult<Self> {
        // Calculate IAG number and extent index
        let iag_num = ino >> 12; // INOSPERIAG = 4096, L2INOSPERIAG = 12
        let ext_idx = (ino % INOSPERIAG) >> 5; // INOSPEREXT = 32, L2INOSPEREXT = 5

        // For the aggregate inode table, we need to walk the imap.
        // Simplified: compute block address directly for common reserved inodes.
        let block = Self::inode_block(volume, ino)?;

        let data = volume.read_page(block)?;

        let offset = Self::inode_offset_in_page(ino);

        let mut dinode = Dinode::default();
        let dinode_bytes = &data[offset..offset + std::mem::size_of::<Dinode>()];
        Self::parse_dinode(dinode_bytes, &mut dinode)?;

        Ok(Self {
            dinode,
            ino,
            page_block: block,
            page_offset: offset,
        })
    }

    /// Compute the block number containing a given inode.
    fn inode_block(volume: &crate::volume::Volume, ino: u32) -> StorageResult<u64> {
        use crate::types::AGGR_INODE_TABLE_START;

        let bytes_per_ino = std::mem::size_of::<Dinode>();
        let inos_per_page = (PSIZE / bytes_per_ino) as u32;

        // For reserved inodes, they are in the inline inode table at AITBL_OFF
        // The inode table starts at AGGR_INODE_TABLE_START
        let byte_offset =
            (AGGR_INODE_TABLE_START + (ino as u64) * (bytes_per_ino as u64)) % (PSIZE as u64);
        let block =
            (AGGR_INODE_TABLE_START + (ino as u64) * (bytes_per_ino as u64)) / (PSIZE as u64);

        let _ = byte_offset;
        let _ = inos_per_page;
        Ok(block)
    }

    /// Compute offset of an inode within its page.
    fn inode_offset_in_page(ino: u32) -> usize {
        let page_ino_offset = ino % (INOSPERPAGE);
        (page_ino_offset as usize) * (PSIZE / INOSPERPAGE as usize)
    }

    /// Parse a dinode from raw bytes.
    fn parse_dinode(data: &[u8], dinode: &mut Dinode) -> StorageResult<()> {
        if data.len() < std::mem::size_of::<Dinode>() {
            return Err(crate::storage::StorageError::Other(
                "insufficient data for dinode".to_string(),
            ));
        }
        dinode.di_inostamp = data[0..4].try_into().unwrap_or([0; 4]);
        dinode.di_fileset = data[4..8].try_into().unwrap_or([0; 4]);
        dinode.di_number = data[8..12].try_into().unwrap_or([0; 4]);
        dinode.di_generation = data[12..16].try_into().unwrap_or([0; 4]);
        dinode.di_ixpxd = crate::types::Pxd::from_bytes(&data[16..24]);
        dinode.di_size = data[24..32].try_into().unwrap_or([0; 8]);
        dinode.di_nblocks = data[32..40].try_into().unwrap_or([0; 8]);
        dinode.di_nlink = data[40..44].try_into().unwrap_or([0; 4]);
        dinode.di_uid = data[44..48].try_into().unwrap_or([0; 4]);
        dinode.di_gid = data[48..52].try_into().unwrap_or([0; 4]);
        dinode.di_mode = data[52..56].try_into().unwrap_or([0; 4]);

        // timestamps and xattr/ea descriptors
        dinode.di_atime = crate::types::Timestruc {
            tv_sec: data[56..60].try_into().unwrap_or([0; 4]),
            tv_nsec: data[60..64].try_into().unwrap_or([0; 4]),
        };
        dinode.di_ctime = crate::types::Timestruc {
            tv_sec: data[64..68].try_into().unwrap_or([0; 4]),
            tv_nsec: data[68..72].try_into().unwrap_or([0; 4]),
        };
        dinode.di_mtime = crate::types::Timestruc {
            tv_sec: data[72..76].try_into().unwrap_or([0; 4]),
            tv_nsec: data[76..80].try_into().unwrap_or([0; 4]),
        };
        dinode.di_otime = crate::types::Timestruc {
            tv_sec: data[80..84].try_into().unwrap_or([0; 4]),
            tv_nsec: data[84..88].try_into().unwrap_or([0; 4]),
        };

        // di_acl (16) + di_ea (16) at offset 88
        dinode.di_acl = crate::types::Dxd {
            flag: data[88],
            rsrvd: [data[89], data[90], data[91]],
            size: [data[92], data[93], data[94], data[95]],
            loc: crate::types::Pxd::from_bytes(&data[96..104]),
        };
        dinode.di_ea = crate::types::Dxd {
            flag: data[104],
            rsrvd: [data[105], data[106], data[107]],
            size: [data[108], data[109], data[110], data[111]],
            loc: crate::types::Pxd::from_bytes(&data[112..120]),
        };

        dinode.di_next_index = data[120..124].try_into().unwrap_or([0; 4]);
        dinode.di_acltype = data[124..128].try_into().unwrap_or([0; 4]);

        // Union area starts at offset 128 (96 bytes base + 32 more)
        // Actually di_base is 128 bytes, then the union is 384 bytes
        // The union starts at offset 128
        dinode.u = data[128..512].try_into().unwrap_or([0; 384]);

        Ok(())
    }

    /// Get the xtree root for this inode (for regular files/directories).
    pub fn xtroot_bytes(&self) -> &[u8] {
        self.dinode.xtroot_bytes()
    }

    /// Get the dtroot bytes (for directories).
    pub fn dtroot_bytes(&self) -> &[u8] {
        self.dinode.dtroot_bytes()
    }

    /// Get the file size.
    pub fn size(&self) -> u64 {
        self.dinode.size_val()
    }

    /// Get file type / mode.
    pub fn mode(&self) -> u32 {
        self.dinode.mode()
    }

    /// Is this a directory?
    pub fn is_dir(&self) -> bool {
        self.dinode.is_dir()
    }

    /// Is this a regular file?
    pub fn is_regular(&self) -> bool {
        self.dinode.is_regular()
    }

    /// Is this a symlink?
    pub fn is_symlink(&self) -> bool {
        self.dinode.is_symlink()
    }

    /// Get the inline fast symlink data if this is a symlink.
    pub fn fast_symlink(&self) -> Option<&[u8]> {
        if !self.is_symlink() {
            return None;
        }
        // For symlinks, the inline data is in the _special._u union area
        // The _fastsymlink is 128 bytes at offset 192 within the union
        let union = &self.dinode.u;
        if union.len() >= 192 + 128 {
            Some(&union[192..192 + 128])
        } else {
            None
        }
    }
}
