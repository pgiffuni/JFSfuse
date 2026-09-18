// SPDX-License-Identifier: GPL-2.0-or-later
//! On-disk inode access — mirrors `jfs_incore.h` / `jfs_inode.c`.
//!
//! Provides read/write access to disk inodes (dinode structures) and
//! translates between on-disk and runtime representations.

use byteorder::{ByteOrder, LittleEndian};

use crate::storage::{Result as StorageResult, StorageError};
use crate::types::{Dinode, DISIZE, FILESYSTEM_I, INOSPERPAGE};

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
        let (block, offset) = Self::find_inode_page(volume, ino)?;
        let data = volume.read_page(block)?;

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

    /// Find the (block, offset) of an inode by scanning the aggregate inode
    /// table. Returns the physical disk location of the dinode.
    ///
    /// The scan starts at the `s_ait2` PXD address and extends through
    /// `s_ait2.length()` blocks, plus additional blocks that may belong
    /// to different filesets' inode tables.
    fn find_inode_page(
        volume: &crate::volume::Volume,
        ino: u32,
    ) -> StorageResult<(u64, usize)> {
        // Map VFS inode number to (fileset, number) pair.
        let (target_fs, target_num) = if ino >= FILESYSTEM_I {
            (FILESYSTEM_I, ino - FILESYSTEM_I)
        } else {
            (1, ino)
        };

        // The aggregate inode table starts at the s_ait2 PXD address.
        let pxd = &volume.sb.s_ait2;
        let table_start = pxd.address();
        let table_len = pxd.length() as u64;

        // Scan the AIT extent plus additional blocks for fileset-table entries.
        // The root inode and other filesystem inodes may be stored in blocks
        // immediately following the AIT2 extent.
        let max_scan = table_len + 32;

        for block_offset in 0..max_scan {
            let block_num = table_start + block_offset;
            if block_num >= volume.agg_size {
                continue;
            }
            let data = volume.read_page(block_num)?;

            for i in 0..INOSPERPAGE {
                let off = (i as usize) * DISIZE;
                let dinode_bytes = &data[off..off + DISIZE];
                let fs = LittleEndian::read_u32(&dinode_bytes[4..8]);
                let num = LittleEndian::read_u32(&dinode_bytes[8..12]);

                if fs == target_fs && num == target_num {
                    return Ok((block_num, off));
                }
            }
        }

        Err(StorageError::Other(format!(
            "inode {} (fileset={}, number={}) not found in inode table",
            ino, target_fs, target_num
        )))
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
