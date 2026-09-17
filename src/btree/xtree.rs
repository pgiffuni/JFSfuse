// SPDX-License-Identifier: GPL-2.0-or-later
//! Extent descriptor B+-tree manager (xtree).
//!
//! Mirrors `jfs_xtree.c` / `jfs_xtree.h` — manages the extent allocation
//! descriptor B+-tree. Maps file offsets (in fsblocks) to disk block addresses.
//!
//! Root can be inline in the inode (`xtroot_t`) or in external pages (`xtpage_t`).

use crate::storage::{BLOCK_SIZE, NullStorage, Result as StorageResult, Storage};
use crate::types::{BlockLength, BlockNo, Pxd, XTENTRYSTART, XTROOTMAXSLOT, Xad, XadFlag, XtRoot};

/// An extent in the file's block map.
#[derive(Debug, Clone, Copy)]
pub struct Extent {
    pub offset: i64,
    pub length: u32,
    pub address: u64,
    pub flags: XadFlag,
}

/// Xtree (extent B+-tree) manager.
pub struct Xtree {
    /// Root is inline in the inode (xtroot_t).
    root: XtRoot,
    /// Storage backend for external pages.
    storage: Box<dyn Storage>,
    /// Block number of the inode (for page I/O).
    inode_block: u64,
}

impl Xtree {
    /// Parse an xtree root from inode data bytes.
    pub fn from_inode_data(data: &[u8]) -> StorageResult<Self> {
        if data.len() < 96 {
            return Err(crate::storage::StorageError::Other(
                "insufficient data for xtroot".to_string(),
            ));
        }

        let root = Self::parse_xtroot(&data[..])?;
        let storage: Box<dyn Storage> = Box::new(crate::storage::NullStorage);
        Ok(Self {
            root,
            storage,
            inode_block: 0,
        })
    }

    fn parse_xtroot(data: &[u8]) -> StorageResult<XtRoot> {
        if data.len() < 20 {
            return Err(crate::storage::StorageError::Other(
                "insufficient data for xtroot header".to_string(),
            ));
        }

        let mut root = XtRoot::default();

        root.header.next = data[0..8].try_into().unwrap_or([0; 8]);
        root.header.prev = data[8..16].try_into().unwrap_or([0; 8]);
        root.header.flag = data[16];
        root.header.rsrvd1 = data[17];
        root.header.nextindex = data[18..20].try_into().unwrap_or([0; 2]);
        root.header.maxentry = data[20..22].try_into().unwrap_or([0; 2]);
        root.header.rsrvd2 = data[22..24].try_into().unwrap_or([0; 2]);
        root.header.self_pxd = crate::types::Pxd::from_bytes(&data[24..32]);

        // Read xad entries starting at offset 32 (after xtheader).
        // In the JFS on-disk format, the first XTENTRYSTART xad slots overlap
        // with the header (it's a union). Real entries start at xad[XTENTRYSTART]
        // which is at byte offset 32 on disk. We store them at their correct
        // JFS index in the xad array.
        let xad_base = 32; // sizeof(xtheader) = 32 bytes
        for i in 0..XTROOTMAXSLOT.saturating_sub(XTENTRYSTART) {
            let base = xad_base + i * 16;
            if base + 16 > data.len() {
                break;
            }
            root.xad[XTENTRYSTART + i] = Self::parse_xad(&data[base..base + 16])?;
        }

        Ok(root)
    }

    fn parse_xad(data: &[u8]) -> StorageResult<Xad> {
        if data.len() < 16 {
            return Err(crate::storage::StorageError::Other(
                "insufficient data for xad".to_string(),
            ));
        }
        let mut xad = Xad::default();
        xad.flag = data[0];
        xad.rsvrd = [data[1], data[2]];
        xad.off1 = data[3];
        xad.off2 = data[4..8].try_into().unwrap_or([0; 4]);
        xad.loc = crate::types::Pxd::from_bytes(&data[8..16]);
        Ok(xad)
    }

    /// Look up the extent containing a given file offset (in fsblocks).
    /// Returns the matching extent entry.
    ///
    /// In JFS, `nextindex` counts entries from XTENTRYSTART, so real entries
    /// are at xad[XTENTRYSTART..nextindex]. The first XTENTRYSTART xad slots
    /// overlap with the header.
    pub fn lookup(&self, offset: BlockNo) -> StorageResult<Option<Extent>> {
        let next_index = self.root.header.nextindex();

        for i in XTENTRYSTART..next_index as usize {
            if i >= XTROOTMAXSLOT {
                break;
            }
            let xad = &self.root.xad[i];
            let xad_offset = xad.offset() as u64;
            let xad_length = xad.length() as u64;
            let xad_addr = xad.address();

            if offset >= xad_offset && offset < xad_offset + xad_length {
                return Ok(Some(Extent {
                    offset: xad_offset as i64,
                    length: xad_length as u32,
                    address: xad_addr,
                    flags: XadFlag::from_bits_truncate(xad.flag),
                }));
            }
        }

        Ok(None)
    }

    /// Read `length` blocks starting at file offset `offset`.
    /// Returns the logical block addresses on disk.
    pub fn map_blocks(
        &self,
        offset: BlockNo,
        length: BlockLength,
    ) -> StorageResult<Vec<(BlockNo, BlockLength)>> {
        let mut result = Vec::new();
        let mut current = offset;
        let end = offset + length;

        while current < end {
            if let Some(extent) = self.lookup(current)? {
                let rel = current - extent.offset as u64;
                let remaining = end - current;
                let take = std::cmp::min(extent.length as u64 - rel, remaining) as BlockLength;
                result.push((extent.address + rel, take));
                current += take as u64;
            } else {
                // Sparse region
                result.push((0, (end - current) as BlockLength));
                break;
            }
        }

        Ok(result)
    }

    /// Read file data for the given offset and length.
    pub fn read_data(
        &self,
        offset: u64,
        length: usize,
        storage: &dyn Storage,
    ) -> StorageResult<Vec<u8>> {
        let fsb_offset = offset / (BLOCK_SIZE as u64);
        let byte_offset = offset % (BLOCK_SIZE as u64);
        let fsb_count = ((length + byte_offset as usize + BLOCK_SIZE - 1) / BLOCK_SIZE) as u64;

        let extents = self.map_blocks(fsb_offset, fsb_count)?;
        let mut result = Vec::with_capacity(length);
        let mut remaining = length;
        let mut buf_offset = byte_offset as usize;

        for (block_addr, block_count) in extents {
            if block_addr == 0 {
                // Sparse: fill with zeros
                let zeros = std::cmp::min(block_count as usize * BLOCK_SIZE, remaining);
                result.extend(std::iter::repeat(0u8).take(zeros));
                remaining = remaining.saturating_sub(zeros);
                buf_offset = 0;
                if remaining == 0 {
                    break;
                }
                continue;
            }

            for blk in 0..block_count as u64 {
                if remaining == 0 {
                    break;
                }
                let data = storage.read_block(block_addr + blk)?;
                let to_copy = std::cmp::min(BLOCK_SIZE - buf_offset, remaining);
                result.extend_from_slice(&data[buf_offset..buf_offset + to_copy]);
                remaining -= to_copy;
                buf_offset = 0;
            }
        }

        result.truncate(length);
        Ok(result)
    }

    pub fn next_index(&self) -> usize {
        self.root.header.nextindex() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xtree_parse() {
        let mut data = vec![0u8; 288];

        // Set xtheader (32 bytes)
        data[0..8].copy_from_slice(&0xFFFFFFFFFFFFFFFFu64.to_le_bytes()); // next
        data[8..16].copy_from_slice(&0xFFFFFFFFFFFFFFFFu64.to_le_bytes()); // prev
        data[16] = 0x01; // flag: BT_ROOT
        data[17] = 0; // rsrvd1
        data[18..20].copy_from_slice(&3u16.to_le_bytes()); // nextindex = 3 (1 entry at index 2 = XTENTRYSTART)
        data[20..22].copy_from_slice(&18u16.to_le_bytes()); // maxentry = 18

        // First xad at offset 32 (xtroot->xad[XTENTRYSTART=2])
        // xad layout: flag(1) rsvrd(2) off1(1) off2(4) loc.len_addr(4) loc.addr2(4) = 16 bytes
        // Total: 12 bytes header + 4 bytes padding = 16 bytes
        // On-disk: data[32]=flag, data[36..40]=off2, data[40..44]=loc.len_addr, data[44..48]=loc.addr2
        data[32] = 0; // flag
        // off2 = 0 (length/offset don't overlap with loc)
        data[40..44].copy_from_slice(&10u32.to_le_bytes()); // loc.len_addr (length=10)
        data[44..48].copy_from_slice(&100u32.to_le_bytes()); // loc.addr2 (low address)

        let xt = Xtree::from_inode_data(&data).unwrap();
        assert_eq!(xt.next_index(), 3);

        // Lookup at offset 5 should find first extent
        let extent = xt.lookup(5).unwrap();
        assert!(extent.is_some());
        let ext = extent.unwrap();
        assert_eq!(ext.length, 10);
        assert_eq!(ext.address, 100);
    }

    #[test]
    fn test_xtree_parse_minimal() {
        let mut data = vec![0u8; 320];
        data[16] = 0x01; // BT_ROOT
        data[18..20].copy_from_slice(&3u16.to_le_bytes()); // nextindex = 3
        data[20..22].copy_from_slice(&18u16.to_le_bytes()); // maxentry = 18

        let xt = Xtree::from_inode_data(&data).unwrap();
        assert_eq!(xt.next_index(), 3);
    }

    #[test]
    fn test_xtree_parse_too_short() {
        let data = vec![0u8; 30];
        let result = Xtree::from_inode_data(&data);
        assert!(result.is_err());
    }
}
