// SPDX-License-Identifier: GPL-2.0-or-later
//! Extent descriptor B+-tree manager (xtree).
//!
//! Mirrors the kernel JFS xtree code — manages the extent allocation
//! descriptor B+-tree. Maps file offsets (in fsblocks) to disk block addresses.
//!
//! Root can be inline in the inode (`xtroot_t`) or in external pages (`xtpage_t`).

use crate::storage::{BLOCK_SIZE, Result as StorageResult, Storage};
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
        Ok(Self { root })
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

    /// Iterate over all extent entries in the xtree root (inline only).
    pub fn iter_extents(&self) -> impl Iterator<Item = Extent> + '_ {
        let next_idx = self.root.header.nextindex() as usize;
        (XTENTRYSTART..next_idx).filter_map(move |i| {
            if i >= XTROOTMAXSLOT {
                return None;
            }
            let xad = &self.root.xad[i];
            let offset = xad.offset() as i64;
            let length = xad.length();
            let address = xad.address();
            if length == 0 {
                return None;
            }
            Some(Extent {
                offset,
                length,
                address,
                flags: XadFlag::from_bits_truncate(xad.flag),
            })
        })
    }

    /// Serialize the XtRoot back to its on-disk byte representation.
    ///
    /// The on-disk format has the xtheader (32 bytes) at the start, with
    /// xad entries beginning at offset 32 (overlapping the first 2 slots
    /// that are part of the header). Returns 288 bytes matching
    /// `Dinode::xtroot_bytes()` length.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 288];

        // Header (32 bytes on-disk, but XtHeader is 24 bytes — the extra
        // 8 bytes come from alignment in the packed layout).
        buf[0..8].copy_from_slice(&self.root.header.next);
        buf[8..16].copy_from_slice(&self.root.header.prev);
        buf[16] = self.root.header.flag;
        buf[17] = self.root.header.rsrvd1;
        buf[18..20].copy_from_slice(&self.root.header.nextindex);
        buf[20..22].copy_from_slice(&self.root.header.maxentry);
        buf[22..24].copy_from_slice(&self.root.header.rsrvd2);
        buf[24..28].copy_from_slice(&self.root.header.self_pxd.len_addr);
        buf[28..32].copy_from_slice(&self.root.header.self_pxd.addr2);

        // Xad entries starting at offset 32 in the data, stored at indices
        // XTENTRYSTART..XTROOTMAXSLOT in the xad array.
        let xad_base = 32;
        for i in 0..XTROOTMAXSLOT.saturating_sub(XTENTRYSTART) {
            let idx = XTENTRYSTART + i;
            if idx >= XTROOTMAXSLOT {
                break;
            }
            let xad = &self.root.xad[idx];
            let base = xad_base + i * 16;
            if base + 16 > buf.len() {
                break;
            }
            buf[base] = xad.flag;
            buf[base + 1..base + 3].copy_from_slice(&xad.rsvrd);
            buf[base + 3] = xad.off1;
            buf[base + 4..base + 8].copy_from_slice(&xad.off2);
            buf[base + 8..base + 12].copy_from_slice(&xad.loc.len_addr);
            buf[base + 12..base + 16].copy_from_slice(&xad.loc.addr2);
        }

        buf
    }

    /// Insert a new extent into the xtroot.
    ///
    /// If the last extent is contiguous (same physical address range),
    /// it is extended in place. Otherwise, a new xad entry is appended.
    /// Returns true if the extent was inserted/extended, false if the
    /// xtroot is full and needs an external page (not yet implemented).
    pub fn insert_extent(&mut self, logical_offset: i64, length: u32, physical_addr: u64) -> bool {
        let next_idx = self.root.header.nextindex();

        // Check if we can extend the last extent (contiguous physical blocks).
        if next_idx > XTENTRYSTART as u16 {
            let last_idx = (next_idx - 1) as usize;
            if last_idx >= XTROOTMAXSLOT {
                return false;
            }
            let last = &self.root.xad[last_idx];
            let last_offset = last.offset();
            let last_length = last.length();
            let last_addr = last.address();

            // Check if this is a contiguous extension of the last extent.
            if last_offset as i64 + last_length as i64 == logical_offset
                && last_addr + last_length as u64 == physical_addr
            {
                self.root.xad[last_idx].set_length(last_length + length);
                return true;
            }
        }

        // Need a new slot.
        if next_idx as usize >= XTROOTMAXSLOT {
            return false;
        }

        // For an empty or header-overlap xtroot, the first real entry goes at
        // XTENTRYSTART. The on-disk xad array at offset 32 corresponds to
        // xad[XTENTRYSTART] in the in-memory array.
        let idx = if next_idx as usize <= XTENTRYSTART {
            XTENTRYSTART
        } else {
            next_idx as usize
        };
        let xad = &mut self.root.xad[idx];
        xad.set_offset(logical_offset);
        xad.set_length(length);
        xad.set_address(physical_addr);
        xad.set_flag(XadFlag::XAD_NEW);

        self.root.header.set_nextindex(idx as u16 + 1);
        true
    }

    /// Replace an extent at the given index.
    pub fn set_extent(&mut self, idx: usize, logical_offset: i64, length: u32, physical_addr: u64) -> bool {
        if idx < XTENTRYSTART || idx >= XTROOTMAXSLOT {
            return false;
        }
        let xad = &mut self.root.xad[idx];
        xad.set_offset(logical_offset);
        xad.set_length(length);
        xad.set_address(physical_addr);
        xad.set_flag(XadFlag::XAD_NEW);
        true
    }

    /// Truncate extents to fit within `new_eof_fsb`.
    ///
    /// Returns a list of (physical_address, block_count) pairs for extents
    /// that were freed (beyond the truncation point). The caller is responsible
    /// for actually freeing those blocks via the allocator.
    ///
    /// Extent entries beyond `new_eof_fsb` are removed from the xtree. If the
    /// last remaining extent straddles the new EOF, its length is reduced.
    pub fn truncate_extents(&mut self, new_eof_fsb: i64) -> Vec<(u64, u32)> {
        let next_idx = self.root.header.nextindex() as usize;

        // Collect extent data first to avoid borrow conflicts during modification.
        let mut kept: Vec<Xad> = Vec::new();
        let mut freed = Vec::new();

        for i in XTENTRYSTART..next_idx {
            if i >= XTROOTMAXSLOT {
                break;
            }
            let xad = &self.root.xad[i];
            let ext_offset = xad.offset() as i64;
            let ext_length = xad.length() as u32;
            let ext_addr = xad.address();

            if ext_length == 0 {
                continue;
            }

            if ext_offset >= new_eof_fsb {
                // Entirely beyond EOF — free and remove.
                freed.push((ext_addr, ext_length));
            } else if ext_offset + ext_length as i64 > new_eof_fsb {
                // Straddles EOF — split: keep partial, free the tail.
                let keep_len = (new_eof_fsb - ext_offset) as u32;
                let free_len = (ext_offset + ext_length as i64 - new_eof_fsb) as u32;
                let free_addr = ext_addr + keep_len as u64;
                freed.push((free_addr, free_len));

                let mut xad_copy = *xad;
                xad_copy.set_length(keep_len);
                kept.push(xad_copy);
            } else {
                // Entirely before EOF — keep as-is.
                kept.push(*xad);
            }
        }

        // Write kept extents back.
        for (write_idx, xad) in kept.iter().enumerate() {
            let idx = XTENTRYSTART + write_idx;
            if idx < XTROOTMAXSLOT {
                self.root.xad[idx] = *xad;
            }
        }
        let new_next = XTENTRYSTART + kept.len();
        // Clear any leftover entries
        for idx in new_next..next_idx.min(XTROOTMAXSLOT) {
            self.root.xad[idx] = Xad::default();
        }
        self.root.header.set_nextindex(new_next as u16);
        freed
    }

    /// Remove extents within the range `[start_fsb, end_fsb)`.
    ///
    /// Extents entirely within the range are freed and removed. Extents
    /// straddling the boundaries are split: the portion outside the range
    /// is kept, the portion inside is freed.
    ///
    /// Returns a list of `(physical_address, block_count)` pairs for the
    /// freed blocks.
    pub fn punch_extents(&mut self, start_fsb: i64, end_fsb: i64) -> Vec<(u64, u32)> {
        let next_idx = self.root.header.nextindex() as usize;
        let mut kept: Vec<Xad> = Vec::new();
        let mut freed = Vec::new();

        for i in XTENTRYSTART..next_idx {
            if i >= XTROOTMAXSLOT {
                break;
            }
            let xad = &self.root.xad[i];
            let ext_offset = xad.offset() as i64;
            let ext_length = xad.length() as u32;
            let ext_addr = xad.address();

            if ext_length == 0 {
                continue;
            }

            let ext_end = ext_offset + ext_length as i64;

            if ext_end <= start_fsb {
                // Entirely before the punch range — keep as-is.
                kept.push(*xad);
            } else if ext_offset >= end_fsb {
                // Entirely after the punch range — keep as-is.
                kept.push(*xad);
            } else if ext_offset >= start_fsb && ext_end <= end_fsb {
                // Entirely within the punch range — free and remove.
                freed.push((ext_addr, ext_length));
            } else if ext_offset < start_fsb && ext_end > end_fsb {
                // Spans the entire punch range — split into two.
                let left_len = (start_fsb - ext_offset) as u32;
                let right_len = (ext_end - end_fsb) as u32;
                let right_addr = ext_addr + left_len as u64;

                freed.push((ext_addr + left_len as u64, (end_fsb - start_fsb) as u32));

                let mut left = *xad;
                left.set_length(left_len);
                kept.push(left);

                let mut right = *xad;
                right.set_offset(end_fsb);
                right.set_length(right_len);
                right.set_address(right_addr);
                kept.push(right);
            } else if ext_offset < start_fsb {
                // Starts before punch range, extends into it — keep left part.
                let keep_len = (start_fsb - ext_offset) as u32;
                let free_len = ext_length - keep_len;
                freed.push((ext_addr + keep_len as u64, free_len));

                let mut xad_copy = *xad;
                xad_copy.set_length(keep_len);
                kept.push(xad_copy);
            } else {
                // Starts within punch range, extends past it — keep right part.
                let keep_len = (ext_end - end_fsb) as u32;
                let free_len = (end_fsb - ext_offset) as u32;
                freed.push((ext_addr, free_len));

                let mut xad_copy = *xad;
                xad_copy.set_offset(end_fsb);
                xad_copy.set_length(keep_len);
                xad_copy.set_address(ext_addr + free_len as u64);
                kept.push(xad_copy);
            }
        }

        // Write kept extents back.
        for (write_idx, xad) in kept.iter().enumerate() {
            let idx = XTENTRYSTART + write_idx;
            if idx < XTROOTMAXSLOT {
                self.root.xad[idx] = *xad;
            }
        }
        let new_next = XTENTRYSTART + kept.len();
        for idx in new_next..next_idx.min(XTROOTMAXSLOT) {
            self.root.xad[idx] = Xad::default();
        }
        self.root.header.set_nextindex(new_next as u16);
        freed
    }


    /// order with no overlapping ranges.
    ///
    /// Returns an error string if the invariant is violated.
    pub fn validate(&self) -> Result<(), String> {
        let next_idx = self.root.header.nextindex() as usize;
        let mut prev_end: i64 = 0;

        for i in XTENTRYSTART..next_idx.min(XTROOTMAXSLOT) {
            let xad = &self.root.xad[i];
            let length = xad.length();
            if length == 0 {
                continue;
            }
            let offset = xad.offset() as i64;
            let end = offset + length as i64;

            if offset < prev_end {
                return Err(format!(
                    "extent at index {} overlaps previous (offset={}, prev_end={})",
                    i, offset, prev_end
                ));
            }
            prev_end = end;
        }

        Ok(())
    }

    /// Split an extent at the given logical block offset.
    ///
    /// If the extent straddling `split_at` is found, it is divided into two
    /// extents: one covering `[original_offset, split_at)` and another
    /// covering `[split_at, original_end)`. Returns `true` if a split occurred.
    ///
    /// If `split_at` falls exactly on an extent boundary, no split is needed.
    pub fn split_extent(&mut self, split_at: i64) -> bool {
        if split_at <= 0 {
            return false;
        }

        let next_idx = self.root.header.nextindex() as usize;
        let mut to_split: Option<usize> = None;

        for i in XTENTRYSTART..next_idx.min(XTROOTMAXSLOT) {
            let xad = &self.root.xad[i];
            let offset = xad.offset() as i64;
            let length = xad.length() as i32 as i64;

            if offset < split_at && offset + length > split_at {
                to_split = Some(i);
                break;
            }
        }

        let idx = match to_split {
            Some(i) => i,
            None => return false,
        };

        let xad = &self.root.xad[idx];
        let original_offset = xad.offset();
        let original_length = xad.length();
        let address = xad.address();
        let keep_len = (split_at - original_offset as i64) as u32;
        let new_len = original_length - keep_len;
        let new_addr = address + keep_len as u64;
        let new_offset = split_at;

        // Check if there's room for a new extent.
        let new_next = next_idx + 1;
        if new_next > XTROOTMAXSLOT {
            return false;
        }

        // Truncate the existing extent to the kept portion.
        self.root.xad[idx].set_length(keep_len);

        // Insert the new extent at the end (direct placement, no merging).
        let new_idx = next_idx;
        if new_idx < XTROOTMAXSLOT {
            let xad = &mut self.root.xad[new_idx];
            xad.set_offset(new_offset);
            xad.set_length(new_len);
            xad.set_address(new_addr);
            xad.flag = 0;
        }
        self.root.header.set_nextindex(new_next as u16);

        true
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

    #[test]
    fn test_xtree_to_bytes_roundtrip() {
        let mut data = vec![0u8; 288];
        // Set up an xtroot with one extent.
        data[16] = 0x01; // flag: BT_ROOT
        data[18..20].copy_from_slice(&3u16.to_le_bytes()); // nextindex = 3
        data[20..22].copy_from_slice(&18u16.to_le_bytes()); // maxentry = 18

        // First xad at offset 32 (index XTENTRYSTART=2)
        // offset = 0, length = 10, address = 100
        data[32] = 0; // flag
        data[40..44].copy_from_slice(&10u32.to_le_bytes()); // loc.len_addr (length=10)
        data[44..48].copy_from_slice(&100u32.to_le_bytes()); // loc.addr2 (address=100)

        let xt = Xtree::from_inode_data(&data).unwrap();
        let bytes = xt.to_bytes();

        // The serialized bytes should match the original (for this simple case).
        assert_eq!(bytes.len(), 288);
        assert_eq!(bytes[16], 0x01);
        assert_eq!(bytes[18..20], 3u16.to_le_bytes());
        assert_eq!(bytes[20..22], 18u16.to_le_bytes());
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 10);
        assert_eq!(u32::from_le_bytes(bytes[44..48].try_into().unwrap()), 100);

        // Round-trip: parse the serialized bytes back.
        let xt2 = Xtree::from_inode_data(&bytes).unwrap();
        assert_eq!(xt2.next_index(), xt.next_index());
    }

    #[test]
    fn test_xtree_insert_extent() {
        let mut data = vec![0u8; 288];
        data[16] = 0x01; // BT_ROOT
        data[18..20].copy_from_slice(&2u16.to_le_bytes()); // nextindex = 2 (no entries yet)
        data[20..22].copy_from_slice(&18u16.to_le_bytes()); // maxentry = 18

        let mut xt = Xtree::from_inode_data(&data).unwrap();
        assert_eq!(xt.next_index(), 2);

        // Insert first extent: logical offset 0, length 5, physical addr 1000.
        let result = xt.insert_extent(0, 5, 1000);
        assert!(result);
        assert_eq!(xt.next_index(), 3);

        // Verify lookup finds it.
        let ext = xt.lookup(0).unwrap().unwrap();
        assert_eq!(ext.offset, 0);
        assert_eq!(ext.length, 5);
        assert_eq!(ext.address, 1000);

        // Insert second extent: contiguous with first (same physical address).
        let result = xt.insert_extent(5, 3, 1005);
        assert!(result);
        assert_eq!(xt.next_index(), 3); // Should have extended, not added new

        // Verify the first extent was extended.
        let ext = xt.lookup(0).unwrap().unwrap();
        assert_eq!(ext.length, 8);

        // Insert non-contiguous extent.
        let result = xt.insert_extent(10, 2, 2000);
        assert!(result);
        assert_eq!(xt.next_index(), 4);

        // Verify lookup by offset.
        let ext = xt.lookup(10).unwrap().unwrap();
        assert_eq!(ext.offset, 10);
        assert_eq!(ext.length, 2);
        assert_eq!(ext.address, 2000);
    }

    #[test]
    fn test_xtree_set_extent() {
        let mut data = vec![0u8; 288];
        data[16] = 0x01;
        data[18..20].copy_from_slice(&4u16.to_le_bytes()); // nextindex = 4 (2 entries)
        data[20..22].copy_from_slice(&18u16.to_le_bytes());

        // First xad at index 2
        data[32] = 0;
        data[40..44].copy_from_slice(&10u32.to_le_bytes()); // length=10
        data[44..48].copy_from_slice(&100u32.to_le_bytes()); // address=100

        let mut xt = Xtree::from_inode_data(&data).unwrap();

        // Replace extent at index 2.
        let result = xt.set_extent(2, 0, 20, 200);
        assert!(result);

        let ext = xt.lookup(0).unwrap().unwrap();
        assert_eq!(ext.length, 20);
        assert_eq!(ext.address, 200);
    }

    #[test]
    fn test_xtree_truncate_no_split() {
        // Single extent: offset=0, length=10, address=100
        // Truncate to new_eof_fsb=5 → keep 5 blocks, free 5 blocks
        let mut data = vec![0u8; 288];
        data[16] = 0x01; // BT_ROOT
        data[18..20].copy_from_slice(&3u16.to_le_bytes()); // nextindex = 3
        data[20..22].copy_from_slice(&18u16.to_le_bytes()); // maxentry = 18
        data[32] = 0; // flag
        data[40..44].copy_from_slice(&10u32.to_le_bytes()); // length=10
        data[44..48].copy_from_slice(&100u32.to_le_bytes()); // address=100

        let mut xt = Xtree::from_inode_data(&data).unwrap();
        let freed = xt.truncate_extents(5);

        assert_eq!(freed.len(), 1, "should free 1 extent");
        assert_eq!(freed[0].0, 105, "freed blocks should start at addr 105");
        assert_eq!(freed[0].1, 5, "freed block count should be 5");

        let ext = xt.lookup(0).unwrap().unwrap();
        assert_eq!(ext.length, 5, "kept extent should be 5 blocks");
        assert_eq!(ext.address, 100, "kept extent should keep original address");
        assert_eq!(xt.next_index(), 3, "nextindex should be 3 (XTENTRYSTART + 1)");
    }

    #[test]
    fn test_xtree_truncate_frees_trailing_extent() {
        // Two extents: [0..10] addr=100, [10..20] addr=200
        // Truncate to new_eof_fsb=10 keeps first, frees second entirely
        let mut data = vec![0u8; 288];
        data[16] = 0x01; // BT_ROOT
        data[18..20].copy_from_slice(&2u16.to_le_bytes()); // nextindex = 2
        data[20..22].copy_from_slice(&18u16.to_le_bytes()); // maxentry = 18

        let mut xt = Xtree::from_inode_data(&data).unwrap();
        xt.insert_extent(0, 10, 100);
        xt.insert_extent(10, 10, 200);

        let freed = xt.truncate_extents(10);

        assert_eq!(freed.len(), 1, "should free 1 trailing extent");
        assert_eq!(freed[0].0, 200, "freed at address 200");
        assert_eq!(freed[0].1, 10, "freed 10 blocks");

        // Only the first extent should remain.
        let ext = xt.lookup(0).unwrap().unwrap();
        assert_eq!(ext.length, 10, "first extent intact");
        assert_eq!(ext.address, 100);

        // Second extent should be gone.
        let none = xt.lookup(10).unwrap();
        assert!(none.is_none(), "second extent should be removed");
        assert_eq!(xt.next_index(), 3, "should have 1 entry after truncate");
    }

    fn test_xtree_truncate_grow_no_change() {
        // Growing: truncate to a larger size should be a no-op on extents.
        let mut data = vec![0u8; 288];
        data[16] = 0x01; // BT_ROOT
        data[18..20].copy_from_slice(&3u16.to_le_bytes()); // nextindex = 3
        data[20..22].copy_from_slice(&18u16.to_le_bytes()); // maxentry = 18
        data[40..44].copy_from_slice(&10u32.to_le_bytes()); // length=10
        data[44..48].copy_from_slice(&100u32.to_le_bytes()); // address=100

        let mut xt = Xtree::from_inode_data(&data).unwrap();
        let freed = xt.truncate_extents(20);

        assert!(freed.is_empty(), "growing should not free any extents");
        assert_eq!(xt.next_index(), 3, "extent count should not change");
    }

    #[test]
    fn test_xtree_validate_no_overlap() {
        let mut data = vec![0u8; 288];
        data[16] = 0x01;
        data[18..20].copy_from_slice(&4u16.to_le_bytes()); // nextindex = 4 (2 entries)
        data[20..22].copy_from_slice(&18u16.to_le_bytes());

        // xad[2] at byte offset 32: offset=0 (off1=0, off2=0), length=10, address=100
        // Layout: flag(1) rsvrd(2) off1(1) off2(4) loc.len_addr(4) loc.addr2(4)
        data[40..44].copy_from_slice(&10u32.to_le_bytes()); // loc.len_addr = length 10
        data[44..48].copy_from_slice(&100u32.to_le_bytes()); // loc.addr2 = address 100

        // xad[3] at byte offset 48: offset=10, length=5, address=200
        data[51] = 0; // off1 (high byte of offset) = 0
        data[52..56].copy_from_slice(&10u32.to_le_bytes()); // off2 = offset 10
        data[56..60].copy_from_slice(&5u32.to_le_bytes()); // loc.len_addr = length 5
        data[60..64].copy_from_slice(&200u32.to_le_bytes()); // loc.addr2 = address 200

        let xt = Xtree::from_inode_data(&data).unwrap();
        assert!(xt.validate().is_ok(), "non-overlapping extents should validate");
    }

    #[test]
    fn test_xtree_split_extent() {
        let mut data = vec![0u8; 288];
        data[16] = 0x01;
        data[18..20].copy_from_slice(&2u16.to_le_bytes());
        data[20..22].copy_from_slice(&18u16.to_le_bytes());

        let mut xt = Xtree::from_inode_data(&data).unwrap();
        xt.insert_extent(0, 10, 100);

        // Split at offset 5: [0..5] addr=100 + [5..10] addr=105
        assert!(xt.split_extent(5), "should split at offset 5");

        // Should have 2 extents now.
        assert_eq!(xt.next_index(), 4, "should have 2 extents after split");

        // Verify first part.
        let ext1 = xt.lookup(0).unwrap().unwrap();
        assert_eq!(ext1.length, 5, "first half should be 5 blocks");
        assert_eq!(ext1.address, 100, "first half keeps original address");

        // Verify second part.
        let ext2 = xt.lookup(5).unwrap().unwrap();
        assert_eq!(ext2.length, 5, "second half should be 5 blocks");
        assert_eq!(ext2.address, 105, "second half address should be 105");
    }

    #[test]
    fn test_xtree_split_on_boundary_noop() {
        let mut data = vec![0u8; 288];
        data[16] = 0x01;
        data[18..20].copy_from_slice(&2u16.to_le_bytes());
        data[20..22].copy_from_slice(&18u16.to_le_bytes());

        let mut xt = Xtree::from_inode_data(&data).unwrap();
        xt.insert_extent(0, 10, 100);

        // Split at boundary (offset 10) — no split needed.
        let result = xt.split_extent(10);
        assert!(!result, "split at boundary should be a no-op");
        assert_eq!(xt.next_index(), 3);
    }
}
