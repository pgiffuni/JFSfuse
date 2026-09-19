// SPDX-License-Identifier: GPL-2.0-or-later
//! Directory B+-tree manager (dtree).
//!
//! Mirrors the kernel JFS dtree code — manages the directory entry B+-tree.
//! Maps filenames to inode numbers. Root is inline in the directory inode
//! as `dtroot_t` — a union of a header (32 bytes) and `dtslot[9]` (9*32=288 bytes).
//!
//! The dtroot header layout (32 bytes):
//!   DASD (16) | flag (1) | nextindex (1) | freecnt (1) | freelist (1)
//!   | idotdot (4) | stbl[8] (8)

use byteorder::{ByteOrder, LittleEndian};

use crate::storage::{BLOCK_SIZE, Result as StorageResult};
use crate::types::{DTENTRYSTART, DTROOTMAXSLOT, DTSLOTSIZE, DtSlot, LdtEntry};

/// A directory entry found via dtree lookup.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub inumber: u32,
    pub name: Vec<u16>,
    pub index: u32,
}

/// Directory B+-tree manager.
pub struct Dtree {
    /// Raw bytes of the dtroot (inline in inode's union area).
    data: Vec<u8>,
    /// Block number of the page (0 for inline).
    block: u64,
}

/// Offset of `nextindex` field within the 32-byte dtroot header.
const DTROOT_NEXTINDEX: usize = 17; // DASD(16) + flag(1)

/// Offset where slot[DTENTRYSTART] begins (first real entry).
const DTROOT_ENTRY_BASE: usize = DTENTRYSTART * DTSLOTSIZE; // 1 * 32 = 32

impl Dtree {
    /// Parse a dtroot from the directory inode's inline union area.
    pub fn from_inode_data(data: &[u8]) -> StorageResult<Self> {
        if data.len() < DTROOT_ENTRY_BASE {
            return Err(crate::storage::StorageError::Other(
                "insufficient data for dtroot".to_string(),
            ));
        }
        Ok(Self {
            data: data.to_vec(),
            block: 0,
        })
    }

    /// Look up a directory entry by name (UCS-2).
    pub fn lookup(&self, name: &[u16]) -> StorageResult<Option<DirEntry>> {
        let num_entries = self.next_index();
        if num_entries == 0 {
            return Ok(None);
        }

        // Read the stbl (sorted entry index table) at offset 24
        let stbl = self.read_stbl(num_entries)?;

        for &slot_idx in &stbl {
            let offset = (slot_idx as usize) * DTSLOTSIZE;
            if offset + DTSLOTSIZE > self.data.len() {
                continue;
            }
            let entry = self.parse_ldt_entry(offset)?;

            // Compare name
            let entry_name = &self.data[offset + 6..offset + 6 + (entry.name_len as usize) * 2];
            let entry_name_u16 = bytes_to_u16(entry_name);

            if entry_name_u16.as_slice() == name {
                return Ok(Some(DirEntry {
                    inumber: entry.inumber(),
                    name: entry_name_u16.clone(),
                    index: entry.index(),
                }));
            }
        }

        Ok(None)
    }

    fn parse_ldt_entry(&self, offset: usize) -> StorageResult<LdtEntry> {
        if offset + DTSLOTSIZE > self.data.len() {
            return Err(crate::storage::StorageError::Other(
                "entry out of bounds".to_string(),
            ));
        }
        let d = &self.data[offset..offset + DTSLOTSIZE];
        let mut entry = LdtEntry::default();
        entry.inumber = d[0..4].try_into().unwrap_or([0; 4]);
        entry.next = d[4] as i8;
        entry.name_len = d[5];
        for i in 0..11 {
            entry.name[i] = LittleEndian::read_u16(&d[6 + i * 2..8 + i * 2]);
        }
        entry.index = d[28..32].try_into().unwrap_or([0; 4]);
        Ok(entry)
    }

    /// Read the sorted table (stbl) which maps entry index to slot index.
    fn read_stbl(&self, num_entries: usize) -> StorageResult<Vec<u8>> {
        // stbl starts at offset 24 within dtroot header, 8 entries (1 per slot)
        let stbl_offset = 24;
        let mut stbl = Vec::with_capacity(num_entries);
        for i in 0..num_entries.min(8) {
            if stbl_offset + i >= self.data.len() {
                break;
            }
            stbl.push(self.data[stbl_offset + i]);
        }
        Ok(stbl)
    }

    fn next_index(&self) -> usize {
        if self.data.len() <= DTROOT_NEXTINDEX {
            return 0;
        }
        self.data[DTROOT_NEXTINDEX] as usize
    }

    /// Return the number of directory entries (for readdir).
    pub fn len_entries(&self) -> usize {
        self.next_index()
    }

    /// Iterate over all directory entries.
    pub fn entries(&self) -> StorageResult<Vec<DirEntry>> {
        let mut result = Vec::new();
        let num_entries = self.next_index();
        let stbl = self.read_stbl(num_entries)?;

        for &slot_idx in &stbl {
            let offset = (slot_idx as usize) * DTSLOTSIZE;
            if offset + DTSLOTSIZE > self.data.len() {
                continue;
            }
            let entry = self.parse_ldt_entry(offset)?;
            let name_bytes = &self.data[offset + 6..offset + 6 + (entry.name_len as usize) * 2];
            let name = bytes_to_u16(name_bytes);
            result.push(DirEntry {
                inumber: entry.inumber(),
                name,
                index: entry.index(),
            });
        }

        Ok(result)
    }

    /// Insert a directory entry. Returns true if the entry was added.
    ///
    /// Uses the free-list within the dtroot to find a free slot. If no free
    /// slot is available and the inline table is full, returns false
    /// (external pages not yet supported).
    pub fn insert(&mut self, name: &[u16], ino: u32, index: u32) -> StorageResult<bool> {
        if self.lookup(name)?.is_some() {
            return Ok(false);
        }

        let num_entries = self.next_index();
        if num_entries >= DTROOTMAXSLOT - 1 {
            return Ok(false);
        }

        let free_slot = self.alloc_slot(num_entries)?;
        if free_slot >= DTROOTMAXSLOT {
            return Ok(false);
        }

        let offset = free_slot * DTSLOTSIZE;
        self.write_ldt_entry(offset, ino, name, index)?;

        let new_next = num_entries + 1;
        self.set_stbl_entry(new_next - 1, free_slot as u8)?;
        self.set_nextindex(new_next);

        Ok(true)
    }

    /// Remove a directory entry by name. Returns the inode number if found.
    pub fn remove(&mut self, name: &[u16]) -> StorageResult<Option<u32>> {
        let num_entries = self.next_index();
        let stbl = self.read_stbl(num_entries)?;

        for (stbl_idx, &slot_idx) in stbl.iter().enumerate() {
            let offset = (slot_idx as usize) * DTSLOTSIZE;
            if offset + DTSLOTSIZE > self.data.len() {
                continue;
            }
            let entry = self.parse_ldt_entry(offset)?;
            let entry_name = &self.data[offset + 6..offset + 6 + (entry.name_len as usize) * 2];
            let entry_name_u16 = bytes_to_u16(entry_name);

            if entry_name_u16.as_slice() == name {
                let ino = entry.inumber();

                for j in stbl_idx..num_entries - 1 {
                    let next_slot = self.read_stbl_entry(j + 1)?;
                    self.set_stbl_entry(j, next_slot);
                }
                self.set_stbl_entry(num_entries - 1, 0);

                self.set_nextindex(num_entries - 1);

                let slot_offset = (slot_idx as usize) * DTSLOTSIZE;
                if slot_offset + DTSLOTSIZE <= self.data.len() {
                    for byte in &mut self.data[slot_offset..slot_offset + DTSLOTSIZE] {
                        *byte = 0;
                    }
                }

                return Ok(Some(ino));
            }
        }

        Ok(None)
    }

    /// Return the serialized dtroot bytes.
    pub fn to_bytes(&self) -> &[u8] {
        &self.data
    }

    fn alloc_slot(&mut self, num_entries: usize) -> StorageResult<usize> {
        let freecnt = if self.data.len() > 18 {
            self.data[18] as usize
        } else {
            0
        };
        let freelist = if self.data.len() > 19 {
            self.data[19] as usize
        } else {
            0
        };

        if freecnt > 0 && freelist != 0 {
            let slot = freelist;
            let next = if slot * DTSLOTSIZE + 4 < self.data.len() {
                self.data[slot * DTSLOTSIZE + 4] as usize
            } else {
                0
            };
            self.data[18] = (freecnt - 1) as u8;
            self.data[19] = next as u8;
            return Ok(slot);
        }

        let next_free = (num_entries + DTENTRYSTART).min(DTROOTMAXSLOT - 1);
        Ok(next_free)
    }

    fn write_ldt_entry(
        &mut self,
        offset: usize,
        ino: u32,
        name: &[u16],
        index: u32,
    ) -> StorageResult<()> {
        if offset + DTSLOTSIZE > self.data.len() {
            return Err(crate::storage::StorageError::Other(
                "slot offset out of bounds".to_string(),
            ));
        }
        let d = &mut self.data[offset..offset + DTSLOTSIZE];
        LittleEndian::write_u32(&mut d[0..4], ino);
        d[5] = name.len().min(11) as u8;
        for (i, &ch) in name.iter().take(11).enumerate() {
            LittleEndian::write_u16(&mut d[6 + i * 2..8 + i * 2], ch);
        }
        LittleEndian::write_u32(&mut d[28..32], index);
        Ok(())
    }

    fn set_stbl_entry(&mut self, idx: usize, slot: u8) -> StorageResult<()> {
        let stbl_offset = 24 + idx;
        if stbl_offset >= self.data.len() {
            return Err(crate::storage::StorageError::Other(
                "stbl offset out of bounds".to_string(),
            ));
        }
        self.data[stbl_offset] = slot;
        Ok(())
    }

    fn read_stbl_entry(&self, idx: usize) -> StorageResult<u8> {
        let stbl_offset = 24 + idx;
        if stbl_offset >= self.data.len() {
            return Err(crate::storage::StorageError::Other(
                "stbl offset out of bounds".to_string(),
            ));
        }
        Ok(self.data[stbl_offset])
    }

    fn set_nextindex(&mut self, val: usize) {
        if self.data.len() > DTROOT_NEXTINDEX {
            self.data[DTROOT_NEXTINDEX] = val as u8;
        }
    }
}

fn bytes_to_u16(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|chunk| LittleEndian::read_u16(chunk))
        .collect()
}

impl LdtEntry {
    pub fn inumber(&self) -> u32 {
        LittleEndian::read_u32(&self.inumber)
    }

    pub fn index(&self) -> u32 {
        LittleEndian::read_u32(&self.index)
    }
}

// Re-export types needed externally
pub use crate::types::{BtFlag, BtPage};

#[cfg(test)]
mod tests {
    use super::*;

    fn make_dtroot(num_entries: usize) -> Vec<u8> {
        // dtroot is 9 * 32 = 288 bytes
        let mut data = vec![0u8; DTROOTMAXSLOT * DTSLOTSIZE];

        // Set flag at offset 16
        data[16] = 0x01;

        // Set nextindex at offset 17
        data[17] = num_entries as u8;

        // Set stbl at offset 24: each stbl entry points to a slot index
        for i in 0..num_entries {
            data[24 + i] = (DTENTRYSTART + i) as u8;
        }

        data
    }

    fn make_ldt_entry(data: &mut [u8], offset: usize, ino: u32, name: &[u16], index: u32) {
        LittleEndian::write_u32(&mut data[offset..offset + 4], ino);
        data[offset + 5] = name.len() as u8;
        for (i, &ch) in name.iter().enumerate() {
            LittleEndian::write_u16(&mut data[offset + 6 + i * 2..offset + 8 + i * 2], ch);
        }
        LittleEndian::write_u32(&mut data[offset + 28..offset + 32], index);
    }

    #[test]
    fn test_dtree_lookup() {
        let mut data = make_dtroot(1);

        // First entry at slot 1 (offset 32)
        let name: Vec<u16> = "abc".encode_utf16().collect();
        make_ldt_entry(&mut data, 32, 42, &name, 0);

        let dtree = Dtree::from_inode_data(&data).unwrap();

        // Lookup "abc"
        let entry = dtree.lookup(&name).unwrap();
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.inumber, 42);
        assert_eq!(entry.index, 0);
    }

    #[test]
    fn test_dtree_lookup_not_found() {
        let mut data = make_dtroot(1);
        let name: Vec<u16> = "abc".encode_utf16().collect();
        make_ldt_entry(&mut data, 32, 42, &name, 0);

        let dtree = Dtree::from_inode_data(&data).unwrap();

        let not_found: Vec<u16> = "xyz".encode_utf16().collect();
        let entry = dtree.lookup(&not_found).unwrap();
        assert!(entry.is_none());
    }

    #[test]
    fn test_dtree_empty() {
        let data = make_dtroot(0);
        let dtree = Dtree::from_inode_data(&data).unwrap();
        assert_eq!(dtree.len_entries(), 0);
        assert_eq!(dtree.entries().unwrap().len(), 0);
    }

    #[test]
    fn test_dtree_entries() {
        let mut data = make_dtroot(3);

        make_ldt_entry(
            &mut data,
            32,
            10,
            &"a".encode_utf16().collect::<Vec<u16>>(),
            0,
        );
        make_ldt_entry(
            &mut data,
            64,
            20,
            &"bb".encode_utf16().collect::<Vec<u16>>(),
            1,
        );
        make_ldt_entry(
            &mut data,
            96,
            30,
            &"ccc".encode_utf16().collect::<Vec<u16>>(),
            2,
        );

        let dtree = Dtree::from_inode_data(&data).unwrap();
        assert_eq!(dtree.len_entries(), 3);

        let entries = dtree.entries().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].inumber, 10);
        assert_eq!(entries[1].inumber, 20);
        assert_eq!(entries[2].inumber, 30);
    }

    #[test]
    fn test_dtree_too_short() {
        let data = vec![0u8; 10];
        let result = Dtree::from_inode_data(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_dtree_insert_entry() {
        let mut data = make_dtroot(0);
        let mut dtree = Dtree::from_inode_data(&data).unwrap();

        let name: Vec<u16> = "hello".encode_utf16().collect();
        let result = dtree.insert(&name, 42, 0).unwrap();
        assert!(result, "insert should succeed");

        assert_eq!(dtree.len_entries(), 1);
        let entry = dtree.lookup(&name).unwrap().unwrap();
        assert_eq!(entry.inumber, 42);
    }

    #[test]
    fn test_dtree_insert_collision() {
        let mut data = make_dtroot(1);
        let name: Vec<u16> = "abc".encode_utf16().collect();
        make_ldt_entry(&mut data, 32, 42, &name, 0);

        let mut dtree = Dtree::from_inode_data(&data).unwrap();
        let result = dtree.insert(&name, 99, 1).unwrap();
        assert!(!result, "insert of existing name should fail");

        let entry = dtree.lookup(&name).unwrap().unwrap();
        assert_eq!(entry.inumber, 42, "original entry should remain");
    }

    #[test]
    fn test_dtree_remove_entry() {
        let mut data = make_dtroot(1);
        let name: Vec<u16> = "abc".encode_utf16().collect();
        make_ldt_entry(&mut data, 32, 42, &name, 0);

        let mut dtree = Dtree::from_inode_data(&data).unwrap();
        let removed = dtree.remove(&name).unwrap();
        assert_eq!(removed, Some(42));
        assert_eq!(dtree.len_entries(), 0);

        let entry = dtree.lookup(&name).unwrap();
        assert!(entry.is_none(), "entry should be gone");
    }

    #[test]
    fn test_dtree_remove_not_found() {
        let data = make_dtroot(0);
        let mut dtree = Dtree::from_inode_data(&data).unwrap();
        let name: Vec<u16> = "xyz".encode_utf16().collect();
        let removed = dtree.remove(&name).unwrap();
        assert_eq!(removed, None);
    }

    #[test]
    fn test_dtree_insert_multiple() {
        let mut data = make_dtroot(0);
        let mut dtree = Dtree::from_inode_data(&data).unwrap();

        let name1: Vec<u16> = "file1".encode_utf16().collect();
        let name2: Vec<u16> = "file2".encode_utf16().collect();
        let name3: Vec<u16> = "file3".encode_utf16().collect();

        assert!(dtree.insert(&name1, 10, 0).unwrap());
        assert!(dtree.insert(&name2, 20, 1).unwrap());
        assert!(dtree.insert(&name3, 30, 2).unwrap());

        assert_eq!(dtree.len_entries(), 3);

        let entries = dtree.entries().unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn test_dtree_insert_and_remove_roundtrip() {
        let mut data = make_dtroot(0);
        let mut dtree = Dtree::from_inode_data(&data).unwrap();

        let name: Vec<u16> = "test".encode_utf16().collect();

        // Insert, then remove, then verify empty.
        dtree.insert(&name, 42, 0).unwrap();
        assert_eq!(dtree.len_entries(), 1);

        dtree.remove(&name).unwrap();
        assert_eq!(dtree.len_entries(), 0);

        // Should be able to re-insert at the same slot.
        assert!(dtree.insert(&name, 42, 0).unwrap());
        assert_eq!(dtree.len_entries(), 1);

        let entry = dtree.lookup(&name).unwrap().unwrap();
        assert_eq!(entry.inumber, 42);
    }

    #[test]
    fn test_dtree_to_bytes_roundtrip() {
        let mut data = make_dtroot(0);
        let mut dtree = Dtree::from_inode_data(&data).unwrap();

        let name: Vec<u16> = "abc".encode_utf16().collect();
        dtree.insert(&name, 42, 0).unwrap();

        // Serialize and re-parse.
        let bytes = dtree.to_bytes().to_vec();
        let dtree2 = Dtree::from_inode_data(&bytes).unwrap();

        assert_eq!(dtree2.len_entries(), 1);
        let entry = dtree2.lookup(&name).unwrap().unwrap();
        assert_eq!(entry.inumber, 42);
    }
}
