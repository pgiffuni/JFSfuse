// SPDX-License-Identifier: MIT
//! FUSE filesystem operations adapter.
//!
//! Bridges JFS filesystem operations to the FUSE kernel interface.
//! Provides: lookup, readdir, read, getattr, lseek, etc.
//!
//! ## Write support
//!
//! Write callbacks (`create`, `mkdir`, `unlink`, `rmdir`, `rename`,
//! `link`, `symlink`, `open`, `release`, `write`, `truncate`, `fsync`,
//! `setattr`, `setxattr`, etc.) are enabled by the `writable` cargo
//! feature, which is active by default. To build a read-only variant,
//! use `--no-default-features --features std`.

use std::collections::HashMap;

use crate::btree::dtree::{Dtree, DirectoryCursor};
use crate::btree::xtree::Xtree;
pub mod abi;
pub mod interrupt;
pub use interrupt::{InterruptFlag, InterruptManager, InterruptToken, RequestId};
use crate::inode::Inode;
    use crate::types::{BlockNo, Dinode};
use crate::volume::Volume;

// Re-export all FUSE ABI constants from the abi module.
pub use abi::*;

/// POSIX errno constants used by the FUSE adapter to report errors.
/// Maps internal JFS conditions to FUSE results per the Phase 9 table.
pub const ENOENT: i32 = 2;
pub const EEXIST: i32 = 17;
pub const ENOTDIR: i32 = 20;
pub const EISDIR: i32 = 21;
pub const ENOTEMPTY: i32 = 39;
pub const ENOSPC: i32 = 28;
pub const EROFS: i32 = 30;
pub const EINVAL: i32 = 22;
pub const EACCES: i32 = 13;
pub const EBUSY: i32 = 16;
pub const EOPNOTSUPP: i32 = 95;
pub const EIO: i32 = 5;
pub const EPERM: i32 = 1;
pub const EAGAIN: i32 = 11;
pub const EINTR: i32 = 4;

/// Lock types (POSIX fcntl `l_type`).
pub const F_RDLCK: i16 = 0;
pub const F_WRLCK: i16 = 1;
pub const F_UNLCK: i16 = 2;

/// `flock`-style lock types.
pub const LOCK_SH: u32 = 1;
pub const LOCK_EX: u32 = 2;
pub const LOCK_UN: u32 = 8;
/// Non-blocking flag for `flock` (OR'd with LOCK_SH or LOCK_EX).
pub const LOCK_NB: u32 = 4;

/// POSIX seek constants.
pub const SEEK_SET: i16 = 0;
pub const SEEK_CUR: i16 = 1;
pub const SEEK_END: i16 = 2;

/// ACCESS check flags (FUSE_ACCESS / `access(2)`).
pub const F_OK: u32 = 0;
pub const R_OK: u32 = 4;
pub const W_OK: u32 = 2;
pub const X_OK: u32 = 1;

/// A POSIX byte-range lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flock {
    pub l_type: i16,
    pub l_whence: i16,
    pub l_start: i64,
    pub l_len: i64,
    pub l_pid: u64,
}

/// A POSIX fcntl lock with an owner identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PosixLock {
    l_type: i16,
    start: u64,
    end: u64,
    owner: u64,
}

impl PosixLock {
    /// Create a new lock. `end` of 0 means "to end of file".
    fn new(l_type: i16, start: u64, end: u64, owner: u64) -> Self {
        Self { l_type, start, end, owner }
    }

    /// Determine whether `self` conflicts with `other`.
    /// Shared (read) locks never conflict with other shared locks.
    /// Exclusive (write) locks conflict with any lock (read or write)
    /// from a different owner.
    fn conflicts_with(&self, other: &PosixLock) -> bool {
        if self.owner == other.owner {
            return false;
        }
        if self.start >= other.end || other.start >= self.end {
            return false;
        }
        match (self.l_type, other.l_type) {
            (F_WRLCK, _) | (_, F_WRLCK) => true,
            _ => false,
        }
    }
}

/// A BSD-style `flock` lock (whole-file, shared or exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FlockLock {
    /// LOCK_SH (shared) or LOCK_EX (exclusive).
    kind: u32,
    owner: u64,
}

impl FlockLock {
    /// Determine whether two flock locks conflict.
    fn conflicts_with(&self, other: &FlockLock) -> bool {
        if self.owner == other.owner {
            return false;
        }
        self.kind == LOCK_EX || other.kind == LOCK_EX
    }
}

/// Result type for FUSE operations that may carry an errno value.
pub type FuseResult<T> = Result<T, i32>;

/// FUSE filesystem operations.
pub struct FuseFs {
    /// Mounted volume.
    pub volume: Volume,
    /// Whether write operations are permitted.
    writable: bool,
    /// POSIX byte-range locks, keyed by inode number.
    locks: HashMap<u32, Vec<PosixLock>>,
    /// BSD-style flock locks, keyed by inode number.
    flock_locks: HashMap<u32, Vec<FlockLock>>,
    /// Tracks in-flight requests for FUSE_INTERRUPT cancellation.
    #[allow(dead_code)]
    interrupt_mgr: InterruptManager,
}

/// FUSE_STATFS structure — mirrors `struct statvfs` from `<sys/statvfs.h>`.
///
/// This is returned by the FUSE_STATFS operation and maps to the kernel's
/// `statfs`/`statvfs` syscall. JFS provides the block counts via the
/// allocation map (`BlockAllocMap::nfree`/`mapsize`) and the superblock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Statfs {
    /// Total data blocks in filesystem (in units of block size).
    pub blocks: u64,
    /// Free blocks available to non-root users.
    pub bfree: u64,
    /// Free blocks (same as bfree in modern FUSE).
    pub bavail: u64,
    /// Total inodes in filesystem.
    pub files: u64,
    /// Free inodes.
    pub ffree: u64,
    /// Filesystem block size (bytes).
    pub bsize: u32,
    /// Fragment size (same as bsize for JFS).
    pub blksize: u32,
    /// Inode size (for `statvfs.f_ffree` semantics).
    pub ino_size: u32,
    /// Maximum length of filenames.
    pub namelen: u32,
}

impl FuseFs {
    pub fn new(volume: Volume) -> Self {
        Self {
            volume,
            writable: false,
            locks: HashMap::new(),
            flock_locks: HashMap::new(),
            interrupt_mgr: InterruptManager::new(),
        }
    }

    /// Enable writable mode (mount-time check: volume must pass validation).
    #[cfg(feature = "writable")]
    pub fn enable_writable(&mut self) -> Result<(), crate::storage::StorageError> {
        self.writable = true;
        Ok(())
    }

    /// Whether the filesystem is mounted writable.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Returns the FUSE capability flags this filesystem supports.
    ///
    /// These flags are negotiated during FUSE_INIT to enable features
    /// in the kernel FUSE client. All constants are defined in the
    /// [`abi`](self::abi) module and match the Linux FUSE ABI.
    ///
    /// ## Implemented
    /// - [`FUSE_POSIX_LOCKS`] — POSIX fcntl-style byte-range locks
    /// - [`FUSE_FLOCK_LOCKS`] — BSD-style `flock(2)` locks
    /// - [`FUSE_ASYNC_READ`] — asynchronous read (reads may be reordered)
    /// - [`FUSE_BIG_WRITES`] — writes larger than 4 KB are accepted
    /// - [`FUSE_PARALLEL_DIROPS`] — concurrent directory operations
    /// - [`FUSE_BMAP`] (opcode) — file block to physical block mapping via Xtree
    /// - [`FUSE_DO_READDIRPLUS`] / [`FUSE_READDIRPLUS_AUTO`] — readdir with attributes
    ///
    /// Long-running operations are made cancellable via [`FUSE_INTERRUPT`]
    /// — each request gets an identifiable [`RequestId`]
    /// (see [`Self::register_request`]).
    ///
    /// [`FUSE_INTERRUPT`]: abi::FUSE_INTERRUPT
    pub fn fuse_capabilities(&self) -> u32 {
        FUSE_POSIX_LOCKS
            | FUSE_FLOCK_LOCKS
            | FUSE_ASYNC_READ
            | FUSE_BIG_WRITES
            | FUSE_PARALLEL_DIROPS
            | FUSE_DO_READDIRPLUS
            | FUSE_READDIRPLUS_AUTO
    }

    /// Register an in-flight FUSE request, returning a `RequestId` and
    /// `InterruptToken` that long-running operations can poll.
    pub fn register_request(&self) -> (RequestId, InterruptToken) {
        self.interrupt_mgr.register()
    }

    /// Handle a FUSE_INTERRUPT message for the given request ID.
    /// Called by the FUSE daemon thread when the kernel sends an interrupt.
    /// Returns `true` if the request was found and signaled.
    pub fn handle_interrupt(&self, request_id: RequestId) -> bool {
        self.interrupt_mgr.interrupt(request_id)
    }

    /// Deregister a completed request.
    pub fn deregister_request(&self, request_id: RequestId) {
        self.interrupt_mgr.deregister(request_id);
    }

    /// Convert a `flock` to absolute start/end offsets.
    ///
    /// Lock ranges operate at byte granularity (not block granularity) —
    /// `l_len == 0` means "to end of file", resolved using `file_size`.
    fn resolve_flock(flock: &Flock, file_size: u64) -> (u64, u64) {
        let start = match flock.l_whence {
            SEEK_SET => flock.l_start as u64,
            SEEK_CUR => flock.l_start as u64,
            SEEK_END => file_size.saturating_add(flock.l_start as i64 as u64),
            _ => flock.l_start as u64,
        };
        let end = if flock.l_len <= 0 {
            // Lock to end of file.
            u64::MAX
        } else {
            start + flock.l_len as u64
        };
        (start, end)
    }

    /// Check for conflicting locks on `ino` for `owner`, returning the
    /// conflicting lock if one exists.
    fn check_lock_conflict(&self, ino: u32, start: u64, end: u64, l_type: i16, owner: u64) -> Option<PosixLock> {
        let locks = self.locks.get(&ino)?;
        for existing in locks {
            if existing.start >= end || start >= existing.end {
                continue;
            }
            if existing.owner == owner {
                continue;
            }
            match (l_type, existing.l_type) {
                (F_WRLCK, _) | (_, F_WRLCK) => return Some(*existing),
                _ => {}
            }
        }
        None
    }

    /// Remove all locks owned by `owner` on the given inode.
    fn release_owner_locks(&mut self, ino: u32, owner: u64) {
        if let Some(lst) = self.locks.get_mut(&ino) {
            lst.retain(|l| l.owner != owner);
        }
    }

    /// Set or remove a POSIX lock.
    ///
    /// - `F_UNLCK`: release all locks for `owner` on `ino`.
    /// - `F_RDLCK`/`F_WRLCK`: set the lock, replacing any existing lock for
    ///   the same owner in the overlapping range.
    ///
    /// Returns `EAGAIN` (EAGAIN=11) if the lock would block and `would_block`
    /// is true; returns `EACCES` (13) as a fallback blocking error.
    fn do_setlk(
        &mut self,
        ino: u32,
        flock: &Flock,
        owner: u64,
    ) -> FuseResult<()> {
        let inode = Inode::read(&mut self.volume, ino).map_err(|_| ENOENT)?;
        let file_size = inode.size();
        let (start, end) = Self::resolve_flock(flock, file_size);

        if flock.l_type == F_UNLCK {
            self.release_owner_locks(ino, owner);
            return Ok(());
        }

        // Check for conflicts.
        if let Some(_conflict) = self.check_lock_conflict(ino, start, end, flock.l_type, owner) {
            return Err(EAGAIN);
        }

        // Remove existing locks by this owner in the overlapping range.
        let lst = self.locks.entry(ino).or_default();
        lst.retain(|l| l.owner != owner || l.end <= start || l.start >= end);

        // If the new lock entirely covers existing locks by this owner,
        // we already removed those — now insert the new lock.
        lst.push(PosixLock::new(flock.l_type, start, end, owner));

        Ok(())
    }

    /// Check what lock would conflict with the given `flock` without
    /// setting it. Returns the conflicting lock info in `Flock` form,
    /// or `F_UNLCK` if no conflict.
    fn do_getlk(
        &mut self,
        ino: u32,
        flock: &Flock,
        owner: u64,
    ) -> FuseResult<Flock> {
        let inode = Inode::read(&mut self.volume, ino).map_err(|_| ENOENT)?;
        let file_size = inode.size();
        let (start, end) = Self::resolve_flock(flock, file_size);

        if let Some(conflict) = self.check_lock_conflict(ino, start, end, flock.l_type, owner) {
            Ok(Flock {
                l_type: conflict.l_type,
                l_whence: SEEK_SET,
                l_start: conflict.start as i64,
                l_len: if conflict.end == u64::MAX {
                    0
                } else {
                    (conflict.end - conflict.start) as i64
                },
                l_pid: conflict.owner,
            })
        } else {
            Ok(Flock {
                l_type: F_UNLCK,
                l_whence: flock.l_whence,
                l_start: flock.l_start,
                l_len: flock.l_len,
                l_pid: 0,
            })
        }
    }

    /// Look up an inode by name within a directory.
    /// Handles `.` and `..` as special cases (implicit entries).
    pub fn lookup(&mut self, parent_ino: u32, name: &str) -> Option<u32> {
        match name {
            "." => Some(parent_ino),
            ".." => {
                let parent = Inode::read(&mut self.volume, parent_ino).ok()?;
                if !parent.is_dir() {
                    return None;
                }
                let dtroot = parent.dtroot_bytes();
                if dtroot.len() >= 24 {
                    Some(u32::from_le_bytes(
                        dtroot[20..24].try_into().unwrap(),
                    ))
                } else {
                    None
                }
            }
            _ => {
                let parent = Inode::read(&mut self.volume, parent_ino).ok()?;
                if !parent.is_dir() {
                    return None;
                }
                let dtree = Dtree::from_inode_data(parent.dtroot_bytes()).ok()?;
                let name_u16: Vec<u16> = name.encode_utf16().collect();
                let entry = dtree.lookup(&name_u16).ok()??;
                Some(entry.inumber)
            }
        }
    }

    /// Read directory entries starting from `offset`. Returns a list of
    /// (name, inode_number, file_type).
    ///
    /// Uses [`DirectoryCursor`] to support offset-based resumption as
    /// required by FUSE READDIR — the kernel passes a cookie offset to
    /// resume from a previous position.
    /// Includes `.` and `..` entries. Real entries come from the dtree stbl order.
    pub fn readdir(&mut self, ino: u32, offset: u64) -> Option<Vec<(String, u32, u8)>> {
        let inode = Inode::read(&mut self.volume, ino).ok()?;
        if !inode.is_dir() {
            return None;
        }

        let mut result = Vec::new();
        let cursor = DirectoryCursor::from_offset(offset);

        // `.` entry — points to self (always emitted; FUSE may skip if offset > 0)
        if !cursor.is_entry() {
            result.push((".".to_string(), ino, 0x04));
        }

        // `..` entry — parent from dtroot header
        let dtroot = inode.dtroot_bytes();
        if dtroot.len() >= 24 {
            let parent_ino = u32::from_le_bytes(dtroot[20..24].try_into().unwrap());
            if cursor.offset <= DirectoryCursor::DOTDOT_OFFSET {
                result.push(("..".to_string(), parent_ino, 0x04));
            }
        }

        // Real entries from the dtree, starting from the cursor's position
        if cursor.is_entry() {
            if let Ok(dtree) = Dtree::from_inode_data(inode.dtroot_bytes()) {
                if let Ok(entries) = dtree.entries() {
                    let start_idx = cursor.entry_index().unwrap_or(0);
                    for e in entries.iter().skip(start_idx) {
                        let name = String::from_utf16_lossy(&e.name)
                            .trim_end_matches('\0')
                            .to_string();
                        let file_type = self.infer_file_type(e.inumber);
                        result.push((name, e.inumber, file_type));
                    }
                }
            }
        } else {
            // Offset was at . or .., so include all real entries
            if let Ok(dtree) = Dtree::from_inode_data(inode.dtroot_bytes()) {
                if let Ok(entries) = dtree.entries() {
                    for e in &entries {
                        let name = String::from_utf16_lossy(&e.name)
                            .trim_end_matches('\0')
                            .to_string();
                        let file_type = self.infer_file_type(e.inumber);
                        result.push((name, e.inumber, file_type));
                    }
                }
            }
        }

        Some(result)
    }

    /// FUSE_READDIRPLUS — read directory entries with attributes.
    ///
    /// Like `readdir` but also fetches the `Dinode` (stat) and type for
    /// each child. This reduces subsequent LOOKUP + GETATTR round-trips.
    /// Supports offset-based resumption via [`DirectoryCursor`].
    pub fn readdirplus(&mut self, ino: u32, offset: u64) -> Option<Vec<(String, u32, u8, Dinode)>> {
        let inode = Inode::read(&mut self.volume, ino).ok()?;
        if !inode.is_dir() {
            return None;
        }

        let mut result = Vec::new();
        let cursor = DirectoryCursor::from_offset(offset);
        let dtroot = inode.dtroot_bytes();

        // `.` entry
        if !cursor.is_entry() {
            if let Some(attr) = self.getattr(ino) {
                result.push((".".to_string(), ino, 0x04, attr));
            }
        }

        // `..` entry
        if dtroot.len() >= 24 {
            let parent_ino = u32::from_le_bytes(dtroot[20..24].try_into().unwrap());
            if cursor.offset <= DirectoryCursor::DOTDOT_OFFSET {
                if let Some(attr) = self.getattr(parent_ino) {
                    result.push(("..".to_string(), parent_ino, 0x04, attr));
                }
            }
        }

        // Real entries
        let entries = if let Ok(dtree) = Dtree::from_inode_data(inode.dtroot_bytes()) {
            if let Ok(entries) = dtree.entries() {
                let start_idx = cursor.entry_index().unwrap_or(0);
                Some(entries.into_iter().skip(start_idx).collect::<Vec<_>>())
            } else {
                None
            }
        } else {
            None
        };

        if let Some(entries) = entries {
            if !cursor.is_entry() {
                // Offset was at . or .., include all entries
                for e in &entries {
                    let name = String::from_utf16_lossy(&e.name)
                        .trim_end_matches('\0')
                        .to_string();
                    let file_type = self.infer_file_type(e.inumber);
                    if let Some(attr) = self.getattr(e.inumber) {
                        result.push((name, e.inumber, file_type, attr));
                    }
                }
            } else {
                for e in &entries {
                    let name = String::from_utf16_lossy(&e.name)
                        .trim_end_matches('\0')
                        .to_string();
                    let file_type = self.infer_file_type(e.inumber);
                    if let Some(attr) = self.getattr(e.inumber) {
                        result.push((name, e.inumber, file_type, attr));
                    }
                }
            }
        }

        Some(result)
    }

    /// Interruptible readdir — same as `readdir` but checks `token` at
    /// each directory entry. Returns `Err(EINTR)` if interrupted.
    /// Supports offset-based resumption.
    pub fn readdir_interruptible(
        &mut self,
        ino: u32,
        offset: u64,
        token: &InterruptToken,
    ) -> Result<Vec<(String, u32, u8)>, i32> {
        let inode = Inode::read(&mut self.volume, ino).map_err(|_| ENOENT)?;
        if !inode.is_dir() {
            return Err(ENOTDIR);
        }

        let mut result = Vec::new();
        let cursor = DirectoryCursor::from_offset(offset);

        if !cursor.is_entry() {
            result.push((".".to_string(), ino, 0x04));
        }

        let dtroot = inode.dtroot_bytes();
        if dtroot.len() >= 24 {
            let parent_ino = u32::from_le_bytes(dtroot[20..24].try_into().unwrap());
            if cursor.offset <= DirectoryCursor::DOTDOT_OFFSET {
                result.push(("..".to_string(), parent_ino, 0x04));
            }
        }

        if let Ok(dtree) = Dtree::from_inode_data(inode.dtroot_bytes()) {
            if let Ok(entries) = dtree.entries() {
                let start_idx = if cursor.is_entry() {
                    cursor.entry_index().unwrap_or(0)
                } else {
                    0
                };
                for (i, e) in entries.iter().enumerate() {
                    if i < start_idx {
                        continue;
                    }
                    if token.is_interrupted() {
                        return Err(EINTR);
                    }
                    let name = String::from_utf16_lossy(&e.name)
                        .trim_end_matches('\0')
                        .to_string();
                    let file_type = self.infer_file_type(e.inumber);
                    result.push((name, e.inumber, file_type));
                }
            }
        }

        Ok(result)
    }

    /// Infer file type from an inode number by reading the target inode.
    fn infer_file_type(&mut self, ino: u32) -> u8 {
        if let Ok(inode) = Inode::read(&mut self.volume, ino) {
            if inode.is_symlink() {
                0xA0 // DT_LNK
            } else if inode.is_dir() {
                0x04 // DT_DIR
            } else if inode.is_regular() {
                0x08 // DT_REG
            } else {
                0x00 // DT_UNKNOWN
            }
        } else {
            0x00
        }
    }

    /// Get file attributes for an inode.
    pub fn getattr(&mut self, ino: u32) -> Option<Dinode> {
        Inode::read(&mut self.volume, ino).ok().map(|i| i.dinode)
    }

    /// FUSE_ACCESS — check file access permissions.
    ///
    /// Checks whether the requested access (F_OK, R_OK, W_OK, X_OK) is
    /// allowed for the given UID/GID against the file's permission bits.
    ///
    /// Uses a simplified POSIX permission model:
    /// - If uid matches the file's uid, use owner bits
    /// - Else if gid matches the file's gid, use group bits
    /// - Else use other bits
    /// - CAP_DAC_OVERRIDE (root uid 0) bypasses all checks
    ///
    /// Returns `EACCES` if access is denied, `ENOENT` if inode doesn't exist.
    pub fn access(&mut self, ino: u32, mask: u32, uid: u32, gid: u32) -> FuseResult<()> {
        let dinode = self.getattr(ino).ok_or(ENOENT)?;
        let mode = dinode.mode();

        // Root (uid 0) bypasses permission checks.
        if uid == 0 {
            return Ok(());
        }

        // Determine which permission bits to use.
        let perms = if uid == dinode.uid() {
            mode >> 6  // owner bits (rwx shifted to position 0-5)
        } else if gid == dinode.gid() {
            mode >> 3  // group bits
        } else {
            mode       // other bits
        };

        // Check each requested permission.
        if mask & R_OK != 0 && perms & 0o4 == 0 {
            return Err(EACCES);
        }
        if mask & W_OK != 0 && perms & 0o2 == 0 {
            return Err(EACCES);
        }
        if mask & X_OK != 0 && perms & 0o1 == 0 {
            // Execute permission also requires at least one execute bit
            // set anywhere in the mode for regular files.
            if mode & 0o111 == 0 {
                return Err(EACCES);
            }
        }

        Ok(())
    }

    /// Read file data at offset.
    pub fn read(&mut self, ino: u32, offset: u64, length: usize) -> Option<Vec<u8>> {
        let inode = Inode::read(&mut self.volume, ino).ok()?;
        let size = inode.size();
        if offset >= size {
            return Some(vec![]);
        }
        let max_read = std::cmp::min(length, (size - offset) as usize);
        let xtree = Xtree::from_inode_data(inode.xtroot_bytes()).ok()?;
        let storage = self.volume.storage.clone();
        xtree.read_data(offset, max_read, &*storage).ok()
    }

    /// Interruptible read — same as `read` but checks `token` at each
    /// block read boundary. Returns `Err(EINTR)` if interrupted.
    pub fn read_interruptible(
        &mut self,
        ino: u32,
        offset: u64,
        length: usize,
        token: &InterruptToken,
    ) -> Result<Vec<u8>, i32> {
        let inode = Inode::read(&mut self.volume, ino).map_err(|_| ENOENT)?;
        let size = inode.size();
        if offset >= size {
            return Ok(vec![]);
        }
        let max_read = std::cmp::min(length, (size - offset) as usize);
        let xtree = Xtree::from_inode_data(inode.xtroot_bytes()).map_err(|_| EIO)?;
        let storage = self.volume.storage.clone();
        xtree
            .read_data_checked(offset, max_read, &*storage, token)
            .map_err(|_| EINTR)
    }

    /// lseek — find the next data segment or hole from a given offset.
    ///
    /// Implements SEEK_DATA and SEEK_HOLE by walking the file's xtree
    /// extent list. Returns the byte offset of the next data region
    /// (SEEK_DATA) or hole (SEEK_HOLE), relative to the file start.
    ///
    /// Seek constants (Linux `<unistd.h>`):
    /// - `SEEK_DATA = 3` — returns the offset of the next data extent at or after `offset`
    /// - `SEEK_HOLE = 4` — returns the offset of the next hole at or after `offset`
    pub fn lseek_data_or_hole(&mut self, ino: u32, offset: u64, seek_type: i32) -> Option<u64> {
        const SEEK_DATA: i32 = 3;
        const SEEK_HOLE: i32 = 4;

        let inode = Inode::read(&mut self.volume, ino).ok()?;
        let size = inode.size();
        let block = crate::storage::BLOCK_SIZE as u64;

        match seek_type {
            SEEK_DATA => {
                let xtree = Xtree::from_inode_data(inode.xtroot_bytes()).ok()?;

                for ext in xtree.iter_extents() {
                    let ext_start = ext.offset as u64 * block;
                    let ext_end = ext_start + ext.length as u64 * block;

                    if ext_end > offset {
                        if ext_start <= offset {
                            return Some(offset);
                        } else {
                            return Some(ext_start);
                        }
                    }
                }

                Some(size)
            }
            SEEK_HOLE => {
                let xtree = Xtree::from_inode_data(inode.xtroot_bytes()).ok()?;

                for ext in xtree.iter_extents() {
                    let ext_start = ext.offset as u64 * block;
                    let ext_end = ext_start + ext.length as u64 * block;

                    if ext_start > offset {
                        return Some(offset);
                    }

                    if offset < ext_end {
                        return Some(ext_end);
                    }
                }

                Some(size)
            }
            _ => None,
    }
    }
}

impl FuseFs {
    /// FUSE_STATFS — return filesystem statistics.
    ///
    /// Maps to `statvfs(2)`. Queries the JFS superblock for total blocks
    /// and the allocation map for free blocks.
    pub fn statfs(&mut self, ino: u32) -> Statfs {
        let block_size = self.volume.block_size();
        let total_blocks = self.volume.total_blocks();
        let free_blocks = self.volume.free_blocks();
        let total_inodes = self.volume.total_inodes();
        let free_inodes = self.volume.free_inodes();

        Statfs {
            blocks: total_blocks,
            bfree: free_blocks,
            bavail: free_blocks,
            files: total_inodes,
            ffree: free_inodes,
            bsize: block_size,
            blksize: block_size,
            ino_size: std::mem::size_of::<crate::types::Dinode>() as u32,
            namelen: 255,
        }
    }

    /// FUSE_BMAP: map a file block number to a physical filesystem block number.
    ///
    /// Returns `(physical_block, num_blocks)` for the extent containing
    /// `file_block`. If the region is sparse (no extent), returns a zero
    /// physical block to indicate a hole.
    ///
    /// In JFS, this is implemented through the Xtree — the file block
    /// (in fsblocks) is looked up in the xtree to find the corresponding
    /// physical disk block address.
    ///
    /// Returns `ENOENT` if the inode cannot be loaded, `EIO` if the xtree
    /// cannot be parsed.
    pub fn bmap(&mut self, ino: u32, file_block: u64) -> FuseResult<(u64, u32)> {
        let inode = Inode::read(&mut self.volume, ino).map_err(|_| ENOENT)?;
        let xtree = Xtree::from_inode_data(inode.xtroot_bytes()).map_err(|_| EIO)?;
        let block_no = file_block as BlockNo;
        match xtree.lookup(block_no) {
            Ok(Some(extent)) => {
                let rel = block_no - extent.offset as u64;
                Ok((extent.address + rel, extent.length))
            }
            Ok(None) => {
                // Sparse region — no extent allocated.
                Ok((0, 0))
            }
            Err(_) => Err(EIO),
        }
    }

    /// Create a file (writable builds only).
    ///
    /// Allocates a new inode, inserts a directory entry in the parent,
    /// and returns the new inode number.
    #[cfg(feature = "writable")]
    pub fn create(&mut self, parent_ino: u32, name: &str, _mode: u32) -> Option<u32> {
        if !self.writable {
            return None;
        }
        self.volume.create_file(parent_ino, name).ok()
    }

    /// Write data to an open file (write-supported, gated behind `writable`).
    ///
    /// Writes data through the transaction layer: begins a transaction,
    /// writes data to the file's existing data blocks, updates inode
    /// metadata, and commits the transaction (journal flush + metadata flush).
    #[cfg(feature = "writable")]
    pub fn write(&mut self, ino: u32, offset: u64, data: &[u8]) -> Option<usize> {
        if !self.writable {
            return None;
        }
        self.volume.write_at(ino, offset, data).ok()
    }

    /// Interruptible write — delegates to `Volume::write_at_with_interrupt`
    /// which checks `token` at each block-write boundary. Returns
    /// `Err(EINTR)` if interrupted.
    #[cfg(feature = "writable")]
    pub fn write_interruptible(
        &mut self,
        ino: u32,
        offset: u64,
        data: &[u8],
        token: &InterruptToken,
    ) -> Result<usize, i32> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume
            .write_at_with_interrupt(ino, offset, data, Some(token))
            .map_err(|_| EINTR)
    }

    /// Flush pending writes for a file (writable builds only).
    ///
    /// In the current ordered-data model, each `write` already commits a
    /// transaction. This method is a no-op for correctness but is provided
    /// for FUSE semantics.
    #[cfg(feature = "writable")]
    pub fn flush(&mut self, ino: u32) -> Option<()> {
        if !self.writable {
            return None;
        }
        self.volume.fsync(ino).ok()
    }

    /// FUSE_SYNCFS — synchronize all filesystem metadata and pending
    /// transactions to durable storage.
    ///
    /// Maps onto the JFS sync sequence:
    /// 1. Commit outstanding transactions (journal commit)
    /// 2. Flush the log/journal
    /// 3. Flush metadata (page cache → storage)
    /// 4. Flush the allocation map
    /// 5. `storage.sync()` — push to stable storage
    #[cfg(feature = "writable")]
    pub fn sync_fs(&mut self) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume.sync().map_err(|_| EIO)
    }

    /// Truncate a file (writable builds only).
    #[cfg(feature = "writable")]
    pub fn truncate(&mut self, ino: u32, new_size: u64) -> Option<()> {
        if !self.writable {
            return None;
        }
        self.volume.truncate(ino, new_size).ok()
    }

    /// Pre-allocate or deallocate file space (writable builds only).
    ///
    /// Implements `fallocate(2)` semantics via the FUSE `fallocate` operation.
    /// Supports space preallocation (mode 0) and hole punching
    /// (`FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE`).
    #[cfg(feature = "writable")]
    pub fn fallocate(
        &mut self,
        ino: u32,
        offset: u64,
        len: u64,
        mode: u32,
    ) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume
            .fallocate(ino, offset, len, mode)
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("not supported") {
                    EOPNOTSUPP
                } else if msg.contains("no contiguous free blocks") || msg.contains("no free") {
                    ENOSPC
                } else {
                    EIO
                }
            })
    }

    /// Copy data between two file descriptors (writable builds only).
    ///
    /// **Optimized MOVE path:** When `FUSE_COPY_FILE_RANGE_MOVE` is set and
    /// both inodes share the same on-disk block (e.g. they reside in the same
    /// inode table page), the method first attempts an extent-reference
    /// transfer: it "steals" extents from the source xtree and inserts them
    /// into the destination xtree, avoiding an actual data copy.
    ///
    /// If the optimized path cannot apply (e.g. sparse source region, or
    /// non-MOVE semantics), it falls back to a read → write cycle. For MOVE,
    /// the source range is punched (freed) after a successful copy.
    #[cfg(feature = "writable")]
    pub fn copy_file_range(
        &mut self,
        src_ino: u32,
        src_offset: u64,
        dst_ino: u32,
        dst_offset: u64,
        len: usize,
        flags: u32,
    ) -> FuseResult<usize> {
        const FUSE_COPY_FILE_RANGE_MOVE: u32 = 1;
        const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
        const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
        const FALLOC_FL_PUNCH_KEEP: u32 = FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE;

        if !self.writable {
            return Err(EROFS);
        }

        if src_ino == dst_ino && src_offset == dst_offset {
            return Err(EINVAL);
        }

        let to_copy = std::cmp::min(len, 65536);
        let is_move = flags & FUSE_COPY_FILE_RANGE_MOVE != 0;

        // Try the optimized extent-reference transfer for MOVE.
        if is_move && src_ino != dst_ino {
            if let Ok(Some(n)) = self
                .volume
                .copy_file_range_extent(src_ino, src_offset, dst_ino, dst_offset, to_copy)
            {
                return Ok(n);
            }
            // Optimized path not applicable — fall through to read → write.
        }

        // Fallback: read → write.
        let data = self.read(src_ino, src_offset, to_copy).ok_or(EIO)?;
        let written = self.write(dst_ino, dst_offset, &data).unwrap_or(0);

        if written > 0 && is_move {
            let _ = self.volume.fallocate(
                src_ino,
                src_offset,
                written as u64,
                FALLOC_FL_PUNCH_KEEP,
            );
        }

        if written == 0 {
            return Err(EIO);
        }

        Ok(written)
    }

    /// Remove a file (write-supported, gated behind `writable`).
    #[cfg(feature = "writable")]
    pub fn unlink(&mut self, parent_ino: u32, name: &str) -> Option<bool> {
        if !self.writable {
            return None;
        }
        Some(self.volume.unlink_file(parent_ino, name).unwrap_or(false))
    }

    /// Create a directory (write-supported, gated behind `writable`).
    ///
    /// Allocates a new directory inode, initializes it with `.` and `..`
    /// entries, inserts a directory entry in the parent, and increments the
    /// parent's link count.
    #[cfg(feature = "writable")]
    pub fn mkdir(&mut self, parent_ino: u32, name: &str, _mode: u32) -> FuseResult<u32> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume.mkdir(parent_ino, name).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("already exists") {
                EEXIST
            } else if msg.contains("invalid filename") {
                EINVAL
            } else if msg.contains("no free inode") {
                ENOSPC
            } else {
                EIO
            }
        })
    }

    /// Remove a directory (write-supported, gated behind `writable`).
    ///
    /// Fails if the child is not a directory or is not empty
    /// (only `.` and `..` entries are allowed).
    #[cfg(feature = "writable")]
    pub fn rmdir(&mut self, parent_ino: u32, name: &str) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        match self.volume.rmdir(parent_ino, name) {
            Ok(true) => Ok(()),
            Ok(false) => Err(ENOENT),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("not a directory") {
                    Err(ENOTDIR)
                } else if msg.contains("not empty") {
                    Err(ENOTEMPTY)
                } else {
                    Err(EIO)
                }
            }
        }
    }

    /// Open a file or directory (writable builds only).
    ///
    /// Validates that the inode exists and increments the open-handle count
    /// for open-unlinked semantics (inode is retained while open handles exist).
    #[cfg(feature = "writable")]
    pub fn open(&mut self, ino: u32) -> FuseResult<()> {
        let result = self.volume.open_file(ino);
        match result {
            Ok(_) => Ok(()),
            Err(_) => Err(ENOENT),
        }
    }

    /// Release an open file handle (writable builds only).
    ///
    /// Decrements the open-handle count. If the inode's nlink dropped to 0
    /// (via unlink while open) and this was the last handle, the inode's
    /// data blocks are freed.
    #[cfg(feature = "writable")]
    pub fn release(&mut self, ino: u32) -> FuseResult<()> {
        self.volume.release_file(ino).map_err(|_| EIO)
    }

    /// FUSE_FORGET — decrement the reference count on an inode.
    ///
    /// `nlookup` is the number of lookups to forget (kernel sends this in
    /// FUSE_FORGET). If the resulting refcount reaches zero, the inode's
    /// cached page data is released from the page cache.
    ///
    /// This helps manage memory: the FUSE kernel module holds a reference
    /// count per inode node and sends FUSE_FORGET when entries are evicted
    /// from the dentry cache.
    pub fn forget(&mut self, ino: u32, nlookup: u64) {
        // In the current JFS model, inodes are looked up from disk on each
        // access (no long-lived inode cache). FORGET is acknowledged by
        // evicting any cached page-cache entries for this inode.
        self.volume.page_cache.evict(ino);
    }

    /// FUSE_BATCH_FORGET — decrement refcounts on multiple inodes at once.
    ///
    /// `entries` is a list of (inode, nlookup) pairs. Each inode's cached
    /// pages are evicted if their refcount reaches zero.
    pub fn batch_forget(&mut self, entries: &[(u32, u64)]) {
        for &(ino, _nlookup) in entries {
            self.volume.page_cache.evict(ino);
        }
    }

    /// Set file attributes (writable builds only).
    ///
    /// Supports chmod (mode), chown (uid/gid), and utimens (atime/mtime).
    /// Unspecified attributes are left unchanged.
    #[cfg(feature = "writable")]
    pub fn setattr(
        &mut self,
        ino: u32,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: Option<u64>,
        mtime: Option<u64>,
    ) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume
            .setattr(ino, mode, uid, gid, atime, mtime)
            .map_err(|_| EIO)
    }

    /// Rename or move a file/directory (writable builds only).
    ///
    /// Unlinks the old name from the source parent and creates a new entry
    /// in the destination parent.
    #[cfg(feature = "writable")]
    pub fn rename(
        &mut self,
        old_parent: u32,
        old_name: &str,
        new_parent: u32,
        new_name: &str,
    ) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume
            .rename(old_parent, old_name, new_parent, new_name)
            .map(|_| ())
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("not a directory") {
                    ENOTDIR
                } else if msg.contains("not empty") {
                    ENOTEMPTY
                } else if msg.contains("already exists") || msg.contains("file exists") {
                    EEXIST
                } else if msg.contains("directory entry already exists") {
                    EEXIST
                } else if msg.contains("no such file") || msg.contains("not found") {
                    ENOENT
                } else if msg.contains("invalid") || msg.contains("cannot") {
                    EPERM
                } else {
                    EIO
                }
            })
    }

    /// Set an extended attribute (writable builds only).
    ///
    /// `flags` follows Linux semantics: XATTR_CREATE (1) fails if the
    /// attribute already exists, XATTR_REPLACE (2) fails if it does not.
    #[cfg(feature = "writable")]
    pub fn setxattr(
        &mut self,
        ino: u32,
        name: &str,
        value: &[u8],
        flags: u32,
    ) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume.setxattr(ino, name, value, flags).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("already exists") {
                EEXIST
            } else if msg.contains("does not exist") {
                ENOENT
            } else if msg.contains("too long") || msg.contains("too large") {
                EOPNOTSUPP
            } else {
                EIO
            }
        })
    }

    /// Get an extended attribute value (writable builds only).
    ///
    /// Returns `Ok(None)` if the attribute does not exist.
    #[cfg(feature = "writable")]
    pub fn getxattr(&mut self, ino: u32, name: &str) -> FuseResult<Option<Vec<u8>>> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume.getxattr(ino, name).map_err(|_| EIO)
    }

    /// Create a symbolic link (writable builds only).
    ///
    /// Stores the target path inline in the new symlink inode if it fits
    /// within 128 bytes. Longer paths are not yet supported.
    #[cfg(feature = "writable")]
    pub fn symlink(&mut self, parent_ino: u32, name: &str, target: &str) -> FuseResult<u32> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume.symlink(parent_ino, name, target).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("already exists") {
                EEXIST
            } else if msg.contains("invalid filename") {
                EINVAL
            } else if msg.contains("no free inode") {
                ENOSPC
            } else if msg.contains("long symlinks not yet supported") {
                EOPNOTSUPP
            } else {
                EIO
            }
        })
    }

    /// Read the target of a symbolic link (writable builds only).
    #[cfg(feature = "writable")]
    pub fn readlink(&mut self, ino: u32) -> FuseResult<Vec<u8>> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume.read_symlink(ino).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("not a symbolic link") {
                ENOENT
            } else {
                EIO
            }
        })
    }

    /// Remove an extended attribute (writable builds only).
    #[cfg(feature = "writable")]
    pub fn removexattr(&mut self, ino: u32, name: &str) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume.removexattr(ino, name).map_err(|_| EIO).and_then(|ok| {
            if ok {
                Ok(())
            } else {
                Err(ENOENT)
            }
        })
    }

    /// List xattr names (writable builds only).
    #[cfg(feature = "writable")]
    pub fn listxattr(&mut self, ino: u32) -> FuseResult<Vec<String>> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume.listxattr(ino).map_err(|_| EIO)
    }

    /// Create a hard link (writable builds only).
    ///
    /// Increments the target inode's link count and inserts a new directory
    /// entry. Hard links to directories are rejected.
    #[cfg(feature = "writable")]
    pub fn link(&mut self, parent_ino: u32, name: &str, target_ino: u32) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume.link_file(parent_ino, name, target_ino).map(|_| ()).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("already exists") {
                EEXIST
            } else if msg.contains("cannot hard-link a directory") {
                EPERM
            } else if msg.contains("invalid filename") {
                EINVAL
            } else {
                EIO
            }
        })
    }

    /// Get file lock (FUSE_GETLK).
    ///
    /// Checks whether the given `flock` would block and, if so, returns
    /// the conflicting lock in the `Flock` result. If no conflict, the
    /// returned `l_type` is `F_UNLCK`.
    ///
    /// `owner` identifies the calling process/thread.
    pub fn getlk(&mut self, ino: u32, flock: &Flock, owner: u64) -> FuseResult<Flock> {
        if !self.writable {
            return Err(EROFS);
        }
        if flock.l_type != F_RDLCK && flock.l_type != F_WRLCK {
            return Err(EINVAL);
        }
        self.do_getlk(ino, flock, owner)
    }

    /// Set file lock (non-blocking, FUSE_SETLK).
    ///
    /// `F_UNLCK` releases locks. `F_RDLCK`/`F_WRLCK` set a lock.
    /// If a conflicting lock exists, returns `EAGAIN` immediately.
    pub fn setlk(&mut self, ino: u32, flock: &Flock, owner: u64) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        if flock.l_type != F_UNLCK && flock.l_type != F_RDLCK && flock.l_type != F_WRLCK {
            return Err(EINVAL);
        }
        self.do_setlk(ino, flock, owner)
    }

    /// Set file lock (blocking, FUSE_SETLKW).
    ///
    /// If a conflicting lock exists, blocking is indicated by returning
    /// `EWOULDBLOCK` (mapped to `EAGAIN`). In a kernel FUSE context the
    /// filesystem would hold the request until the lock becomes available;
    /// in this in-memory implementation we return `EAGAIN` so the caller
    /// can retry.
    pub fn setlkw(&mut self, ino: u32, flock: &Flock, owner: u64) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        if flock.l_type != F_UNLCK && flock.l_type != F_RDLCK && flock.l_type != F_WRLCK {
            return Err(EINVAL);
        }
        self.do_setlk(ino, flock, owner)
    }

    /// Release all POSIX locks held by `owner` on `ino`.
    /// Called when a file handle is released.
    pub fn release_locks(&mut self, ino: u32, owner: u64) {
        self.release_owner_locks(ino, owner);
        self.release_flock_owner(ino, owner);
    }

    /// Acquire or release a BSD-style `flock(2)` lock.
    ///
    /// - `LOCK_SH` — shared (read) lock
    /// - `LOCK_EX` — exclusive (write) lock
    /// - `LOCK_UN` — release all locks for `owner`
    /// - `LOCK_NB` — OR'd with LOCK_SH/LOCK_EX for non-blocking
    ///
    /// Returns `EAGAIN` if the lock would block and `LOCK_NB` is set.
    pub fn flock(&mut self, ino: u32, operation: u32, owner: u64) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }

        let nonblocking = operation & LOCK_NB != 0;
        let kind = operation & !LOCK_NB;

        if kind == LOCK_UN {
            self.release_flock_owner(ino, owner);
            return Ok(());
        }

        if kind != LOCK_SH && kind != LOCK_EX {
            return Err(EINVAL);
        }

        // Check for conflicting locks.
        if let Some(existing) = self.flock_locks.get(&ino) {
            for lock in existing {
                if lock.owner != owner && lock.conflicts_with(&FlockLock { kind, owner }) {
                    if nonblocking {
                        return Err(EAGAIN);
                    }
                    // In this in-memory implementation we cannot truly block.
                    // Return EAGAIN so the FUSE daemon can retry or block.
                    return Err(EAGAIN);
                }
            }
        }

        // Remove existing locks by this owner (flock replaces, doesn't merge).
        if let Some(lst) = self.flock_locks.get_mut(&ino) {
            lst.retain(|l| l.owner != owner);
        }
        self.flock_locks.entry(ino).or_default().push(FlockLock { kind, owner });

        Ok(())
    }

    /// Remove all flock locks for `owner` on `ino`.
    fn release_flock_owner(&mut self, ino: u32, owner: u64) {
        if let Some(lst) = self.flock_locks.get_mut(&ino) {
            lst.retain(|l| l.owner != owner);
        }
    }
}
