// SPDX-License-Identifier: BSD-2-Clause
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
use std::num::NonZeroU32;
use std::time::Duration;

use fuse3::raw::reply::{
    DirectoryEntry, DirectoryEntryPlus, ReplyAttr, ReplyBmap, ReplyCopyFileRange, ReplyCreated,
    ReplyData, ReplyEntry, ReplyInit, ReplyLSeek, ReplyOpen, ReplyPoll, ReplyStatFs, ReplyWrite,
    ReplyXAttr, ReplyDirectory, ReplyDirectoryPlus,
};
use fuse3::raw::Request;
use fuse3::{FileType, Timestamp};
use futures_util::stream::Stream;

use crate::btree::dtree::{Dtree, DirectoryCursor, DirEntry};
use crate::btree::xtree::Xtree;
use crate::storage::StorageError;
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
pub const ENXIO: i32 = 6;
pub const EPERM: i32 = 1;
pub const EBADF: i32 = 9;
pub const EAGAIN: i32 = 11;
pub const EINTR: i32 = 4;
pub const ERANGE: i32 = 34;

/// Translate a FreeBSD extattr namespace name to its internal JFS xattr name.
///
/// FreeBSD uses `user.foo` and `system.foo` as the canonical spelling;
/// JFS stores extattr names with the namespace prefix already stripped.
/// However, when interfacing through FUSE on Linux, the names arrive as
/// `user.foo` / `system.foo`. We normalize: if the name already has a
/// `user.` or `system.` prefix, strip it before passing to the volume layer.
/// If it has no recognized prefix, treat it as a `user.` attribute.
pub fn translate_xattr_name(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("user.") {
        rest.to_string()
    } else if let Some(rest) = name.strip_prefix("system.") {
        format!("system.{}", rest)
    } else if name.starts_with("system.") || name.starts_with("user.") {
        name.to_string()
    } else {
        name.to_string()
    }
}

/// Reverse of [`translate_xattr_name`]: add `user.` prefix to bare names
/// for listxattr output.
pub fn reverse_xattr_name(name: &str) -> String {
    if name.starts_with("user.") || name.starts_with("system.") {
        name.to_string()
    } else {
        format!("user.{}", name)
    }
}

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
/// Tracks per-inode state on the FUSE server side.
///
/// The kernel maintains a reference count (`nlookup`) for each inode
/// it has sent to userspace via FUSE_LOOKUP / FUSE_CREATE / FUSE_MKNOD.
/// For each such lookup, the kernel later sends FUSE_FORGET with the
/// number of references to release. Only when the count drops to zero
/// can cached data be safely evicted.
#[derive(Debug, Clone, Default)]
pub struct NodeState {
    /// Lookup reference count (incremented by LOOKUP, decremented by FORGET).
    lookup_count: u64,
}

impl NodeState {
    pub fn new() -> Self {
        Self { lookup_count: 1 }
    }

    pub fn increment(&mut self, n: u64) {
        self.lookup_count += n;
    }

    pub fn decrement(&mut self, n: u64) {
        self.lookup_count = self.lookup_count.saturating_sub(n);
    }

    pub fn is_zero(&self) -> bool {
        self.lookup_count == 0
    }
}

/// Tracks an open file handle issued by the FUSE kernel client.
#[derive(Debug, Clone)]
pub struct OpenHandle {
    /// Monotonic file-handle ID assigned by the FUSE server.
    pub fh: u64,
    /// Inode number the handle refers to.
    pub ino: u32,
    /// True if this handle was opened as a directory.
    pub is_dir: bool,
}

pub struct FuseFs {
    /// Mounted volume.
    pub volume: Volume,
    /// Whether write operations are permitted.
    writable: bool,
    /// POSIX byte-range locks, keyed by inode number.
    locks: HashMap<u32, Vec<PosixLock>>,
    /// BSD-style flock locks, keyed by inode number.
    flock_locks: HashMap<u32, Vec<FlockLock>>,
    /// Per-inode lookup reference counts, keyed by inode number.
    node_states: HashMap<u32, NodeState>,
    /// Open file handles, keyed by FUSE file-handle ID.
    handles: HashMap<u64, OpenHandle>,
    /// Next file-handle ID to assign.
    next_fh: u64,
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
            node_states: HashMap::new(),
            handles: HashMap::new(),
            next_fh: 1,
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
    /// - [`FUSE_ASYNC_READ`] — asynchronous read (reads may be reordered)
    /// - [`FUSE_BIG_WRITES`] — writes larger than 4 KB are accepted
    /// - [`FUSE_PARALLEL_DIROPS`] — concurrent directory operations
    /// - [`FUSE_BMAP`] (opcode) — file block to physical block mapping via Xtree
    /// - [`FUSE_DO_READDIRPLUS`] / [`FUSE_READDIRPLUS_AUTO`] — readdir with attributes
    ///
    /// ## Not requested (incomplete)
    /// - [`FUSE_POSIX_LOCKS`] — `FuseFs` has `setlk`/`getlk`/`setlkw` methods, but
    ///   the `file-lock` feature is not enabled in `fuse3`, so `GETLK`/`SETLK`/`SETLKW`
    ///   are not dispatched at the FUSE protocol level. The in-memory lock table is
    ///   dead code until the feature is enabled.
    /// - [`FUSE_FLOCK_LOCKS`] — `FuseFs` has a `flock` method, but `fuse3` 0.7 does
    ///   not expose a `flock` callback in the `Filesystem` trait. Same dead-code caveat.
    ///
    /// ## Partially implemented (see FuseFs documentation)
    /// - [`FUSE_SETLKW`] semantics: conflicts return `EAGAIN` instead of blocking.
    ///
    /// ## Not requested
    /// - [`FUSE_DEFAULT_PERMISSIONS`] — access checks are performed in the
    ///   FUSE server via `ACCESS`, not delegated to the kernel.
    ///
    /// Long-running operations are made cancellable via [`FUSE_INTERRUPT`]
    /// — each request gets an identifiable [`RequestId`]
    /// (see [`Self::register_request`]).
    ///
    /// [`FUSE_INTERRUPT`]: abi::FUSE_INTERRUPT
    pub fn fuse_capabilities(&self) -> u32 {
        FUSE_ASYNC_READ
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

    /// Register an in-flight FUSE request using a specific request ID
    /// (the `unique` field from the FUSE header). This allows FUSE_INTERRUPT
    /// messages, which carry the same `unique` ID, to be routed to the correct
    /// interrupt token.
    pub fn register_request_at(&self, request_id: RequestId) -> (RequestId, InterruptToken) {
        self.interrupt_mgr.register_at(request_id)
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
            "." => {
                self.add_node_state(parent_ino);
                Some(parent_ino)
            }
            ".." => {
                let parent = Inode::read(&mut self.volume, parent_ino).ok()?;
                if !parent.is_dir() {
                    return None;
                }
                let dtroot = parent.dtroot_bytes();
                if dtroot.len() >= 24 {
                    let ino = u32::from_le_bytes(
                        dtroot[20..24].try_into().unwrap(),
                    );
                    self.add_node_state(ino);
                    Some(ino)
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
                self.add_node_state(entry.inumber);
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
    pub fn readdir(&mut self, ino: u32, offset: u64) -> Option<Vec<(String, u32, u8, u64)>> {
        let inode = Inode::read(&mut self.volume, ino).ok()?;
        if !inode.is_dir() {
            return None;
        }

        let mut result = Vec::new();
        let cursor = DirectoryCursor::from_offset(offset);

        // `.` entry — points to self (always emitted; FUSE may skip if offset > 0)
        if !cursor.is_entry() {
            result.push((".".to_string(), ino, 0x04, DirectoryCursor::DOT_OFFSET));
        }

        // `..` entry — parent from dtroot header
        let dtroot = inode.dtroot_bytes();
        if dtroot.len() >= 24 {
            let parent_ino = u32::from_le_bytes(dtroot[20..24].try_into().unwrap());
            if cursor.offset <= DirectoryCursor::DOTDOT_OFFSET {
                result.push(("..".to_string(), parent_ino, 0x04, DirectoryCursor::DOTDOT_OFFSET));
            }
        }

        // Real entries from the dtree, starting from the cursor's position
        let start_idx = cursor.entry_index().unwrap_or(0);
        let entries: Option<Vec<DirEntry>> = if let Ok(dtree) = Dtree::from_inode_data(inode.dtroot_bytes()) {
            if let Ok(entries) = dtree.entries() {
                Some(entries.into_iter().skip(start_idx).collect::<Vec<_>>())
            } else {
                None
            }
        } else {
            None
        };

        if let Some(entries) = entries {
            for (idx, e) in entries.iter().enumerate() {
                let name = String::from_utf16_lossy(&e.name)
                    .trim_end_matches('\0')
                    .to_string();
                let file_type = self.infer_file_type(e.inumber);
                let cookie = DirectoryCursor::ENTRY_BASE + start_idx as u64 + idx as u64;
                result.push((name, e.inumber, file_type, cookie));
            }
        }

        Some(result)
    }

    /// FUSE_READDIRPLUS — read directory entries with attributes.
    ///
    /// Like `readdir` but also fetches the `Dinode` (stat) and type for
    /// each child. This reduces subsequent LOOKUP + GETATTR round-trips.
    /// Supports offset-based resumption via [`DirectoryCursor`].
    pub fn readdirplus(&mut self, ino: u32, offset: u64) -> Option<Vec<(String, u32, u8, Dinode, u64)>> {
        let inode = Inode::read(&mut self.volume, ino).ok()?;
        if !inode.is_dir() {
            return None;
        }

        let mut result = Vec::new();
        let cursor = DirectoryCursor::from_offset(offset);
        let dtroot = inode.dtroot_bytes();

        // `.` entry
        if !cursor.is_entry() {
            self.add_node_state(ino);
            if let Some(attr) = self.getattr(ino) {
                result.push((".".to_string(), ino, 0x04, attr, DirectoryCursor::DOT_OFFSET));
            }
        }

        // `..` entry
        if dtroot.len() >= 24 {
            let parent_ino = u32::from_le_bytes(dtroot[20..24].try_into().unwrap());
            if cursor.offset <= DirectoryCursor::DOTDOT_OFFSET {
                self.add_node_state(parent_ino);
                if let Some(attr) = self.getattr(parent_ino) {
                    result.push(("..".to_string(), parent_ino, 0x04, attr, DirectoryCursor::DOTDOT_OFFSET));
                }
            }
        }

        // Real entries
        let start_idx = cursor.entry_index().unwrap_or(0);
        let entries: Option<Vec<DirEntry>> = if let Ok(dtree) = Dtree::from_inode_data(inode.dtroot_bytes()) {
            if let Ok(entries) = dtree.entries() {
                Some(entries.into_iter().skip(start_idx).collect::<Vec<_>>())
            } else {
                None
            }
        } else {
            None
        };

        if let Some(entries) = entries {
            for (idx, e) in entries.iter().enumerate() {
                let name = String::from_utf16_lossy(&e.name)
                    .trim_end_matches('\0')
                    .to_string();
                let file_type = self.infer_file_type(e.inumber);
                let cookie = DirectoryCursor::ENTRY_BASE + start_idx as u64 + idx as u64;
                self.add_node_state(e.inumber);
                if let Some(attr) = self.getattr(e.inumber) {
                    result.push((name, e.inumber, file_type, attr, cookie));
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
    ) -> Result<Vec<(String, u32, u8, u64)>, i32> {
        let inode = Inode::read(&mut self.volume, ino).map_err(|_| ENOENT)?;
        if !inode.is_dir() {
            return Err(ENOTDIR);
        }

        let mut result = Vec::new();
        let cursor = DirectoryCursor::from_offset(offset);

        if !cursor.is_entry() {
            result.push((".".to_string(), ino, 0x04, DirectoryCursor::DOT_OFFSET));
        }

        let dtroot = inode.dtroot_bytes();
        if dtroot.len() >= 24 {
            let parent_ino = u32::from_le_bytes(dtroot[20..24].try_into().unwrap());
            if cursor.offset <= DirectoryCursor::DOTDOT_OFFSET {
                result.push(("..".to_string(), parent_ino, 0x04, DirectoryCursor::DOTDOT_OFFSET));
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
                    let cookie = DirectoryCursor::ENTRY_BASE + start_idx as u64 + i as u64;
                    result.push((name, e.inumber, file_type, cookie));
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
    /// - Symlinks: ACCESS always passes (symlink mode is 0777)
    /// - For directories, X_OK means "search" (traverse) permission
    /// - For regular files, X_OK means execute permission
    ///
    /// Returns `EACCES` if access is denied, `ENOENT` if inode doesn't exist.
    pub fn access(&mut self, ino: u32, mask: u32, uid: u32, gid: u32) -> FuseResult<()> {
        let dinode = self.getattr(ino).ok_or(ENOENT)?;
        let mode = dinode.mode();

        // Root (uid 0) bypasses permission checks.
        if uid == 0 {
            return Ok(());
        }

        // Symlinks always allow access (their mode is 0777).
        if dinode.is_symlink() {
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
            return Err(EACCES);
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

        // At or past EOF: no data or hole to report.
        if offset >= size {
            return None;
        }

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

                // No data extent at or after offset.
                None
            }
            SEEK_HOLE => {
                let xtree = Xtree::from_inode_data(inode.xtroot_bytes()).ok()?;

                for ext in xtree.iter_extents() {
                    let ext_start = ext.offset as u64 * block;
                    let ext_end = std::cmp::min(ext_start + ext.length as u64 * block, size);

                    if ext_start > offset {
                        return Some(offset);
                    }

                    if offset < ext_end {
                        return Some(ext_end);
                    }
                }

                // No more extents — the hole starts at EOF.
                Some(size)
            }
            _ => None,
        }
    }
}

impl FuseFs {
    /// Convert a `Dinode` to a `fuse3::FileAttr`.
    pub fn dinode_to_fileattr(ino: u32, dinode: &Dinode) -> fuse3::raw::reply::FileAttr {
        let mode = dinode.mode();
        let kind = match mode & 0xf000 {
            0x1000 => FileType::NamedPipe,
            0x2000 => FileType::CharDevice,
            0x4000 => FileType::Directory,
            0x6000 => FileType::BlockDevice,
            0x8000 => FileType::RegularFile,
            0xa000 => FileType::Symlink,
            0xc000 => FileType::Socket,
            _ => FileType::RegularFile,
        };
        let block_size = crate::storage::BLOCK_SIZE as u32;
        fuse3::raw::reply::FileAttr {
            ino: ino as u64,
            size: dinode.size_val(),
            blocks: dinode.nblocks(),
            atime: Timestamp::new(dinode.di_atime.seconds() as i64, dinode.di_atime.nanoseconds()),
            mtime: Timestamp::new(dinode.di_mtime.seconds() as i64, dinode.di_mtime.nanoseconds()),
            ctime: Timestamp::new(dinode.di_ctime.seconds() as i64, dinode.di_ctime.nanoseconds()),
            kind,
            perm: (mode & 0x1fff) as u16,
            nlink: dinode.nlink(),
            uid: dinode.uid(),
            gid: dinode.gid(),
            rdev: 0,
            blksize: block_size,
        }
    }

    /// Allocate a new file-handle ID and register an `OpenHandle`.
    pub fn open_handle(&mut self, ino: u32, is_dir: bool) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.handles.insert(fh, OpenHandle { fh, ino, is_dir });
        fh
    }

    /// Remove a file-handle ID from the handle table.
    pub fn release_handle(&mut self, fh: u64) -> Option<OpenHandle> {
        self.handles.remove(&fh)
    }

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
        let ino = self.volume.create_file(parent_ino, name).ok()?;
        self.add_node_state(ino);
        Some(ino)
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
        const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
        const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
        const FALLOC_FL_PUNCH_KEEP: u32 = FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE;
        const FALLOC_FL_COLLAPSE_RANGE: u32 = 0x08;
        const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
        const FALLOC_FL_INSERT_RANGE: u32 = 0x20;
        if mode & (FALLOC_FL_COLLAPSE_RANGE | FALLOC_FL_ZERO_RANGE | FALLOC_FL_INSERT_RANGE) != 0 {
            return Err(EOPNOTSUPP);
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
    /// FUSE_COPY_FILE_RANGE — copy data between files.
    ///
    /// When the `FUSE_COPY_FILE_RANGE_MOVE` flag is set and the source and
    /// destination are different files, this method first attempts an
    /// extent-reference transfer: it "steals" extents from the source xtree
    /// and inserts them into the destination xtree, avoiding an actual data
    /// copy (write-in-place semantics).
    ///
    /// For non-MOVE copies or when the optimized path cannot apply (sparse
    /// source region, non-block-aligned offsets, xtroot full), it falls back
    /// to a read → write cycle. For MOVE, the source range is punched (freed)
    /// after a successful copy.
    ///
    /// The full requested `len` bytes are copied via internal looping; a
    /// single return value always reflects the total bytes transferred.
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
        const CHUNK_SIZE: usize = 65536;

        if !self.writable {
            return Err(EROFS);
        }

        if src_ino == dst_ino && src_offset == dst_offset {
            return Err(EINVAL);
        }

        let is_move = flags & FUSE_COPY_FILE_RANGE_MOVE != 0;
        let mut total_copied = 0usize;
        let mut cur_src = src_offset;
        let mut cur_dst = dst_offset;

        while total_copied < len {
            let remaining = len - total_copied;
            let chunk = std::cmp::min(remaining, CHUNK_SIZE);

            let copied = if is_move && src_ino != dst_ino {
                match self
                    .volume
                    .copy_file_range_extent(src_ino, cur_src, dst_ino, cur_dst, chunk)
                {
                    Ok(Some(n)) => n,
                    _ => {
                        // Optimized path not applicable or failed —
                        // fall through to read → write.
                        let data = self.read(src_ino, cur_src, chunk).ok_or(EIO)?;
                        let written = self.write(dst_ino, cur_dst, &data).unwrap_or(0);

                        if written > 0 && is_move {
                            let _ = self.volume.fallocate(
                                src_ino,
                                cur_src,
                                written as u64,
                                FALLOC_FL_PUNCH_KEEP,
                            );
                        }

                        written
                    }
                }
            } else {
                let data = self.read(src_ino, cur_src, chunk).ok_or(EIO)?;
                let written = self.write(dst_ino, cur_dst, &data).unwrap_or(0);
                written
            };

            if copied == 0 {
                break;
            }

            total_copied += copied;
            cur_src += copied as u64;
            cur_dst += copied as u64;
        }

        if total_copied == 0 {
            return Err(EIO);
        }

        Ok(total_copied)
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
        self.volume.mkdir(parent_ino, name)
            .map(|ino| {
                self.add_node_state(ino);
                ino
            })
            .map_err(|e| {
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

    /// Open a file or directory.
    ///
    /// Validates that the inode exists and increments the open-handle count
    /// for open-unlinked semantics (inode is retained while open handles exist).
    /// Available in both read-only and writable builds.
    pub fn open(&mut self, ino: u32) -> FuseResult<()> {
        let result = self.volume.open_file(ino);
        match result {
            Ok(_) => Ok(()),
            Err(_) => Err(ENOENT),
        }
    }

    /// Release an open file handle.
    ///
    /// Decrements the open-handle count. If the inode's nlink dropped to 0
    /// (via unlink while open) and this was the last handle, the inode's
    /// data blocks are freed. Available in both read-only and writable builds;
    /// in read-only mode, no reclamation is needed.
    pub fn release(&mut self, ino: u32) -> FuseResult<()> {
        #[cfg(feature = "writable")]
        {
            self.volume.release_file(ino).map_err(|_| EIO)
        }
        #[cfg(not(feature = "writable"))]
        {
            let _ = ino;
            Ok(())
        }
    }

    /// Returns the current lookup reference count for an inode (for
    /// diagnostics/testing). Returns 0 if the inode is not tracked.
    pub fn lookup_count(&self, ino: u32) -> u64 {
        self.node_states.get(&ino).map(|s| s.lookup_count).unwrap_or(0)
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
        let should_evict = self.decrement_node_state(ino, nlookup);
        if should_evict {
            self.volume.page_cache.evict(ino);
        }
    }

    /// FUSE_BATCH_FORGET — decrement refcounts on multiple inodes at once.
    ///
    /// `entries` is a list of (inode, nlookup) pairs. Each inode's lookup
    /// count is decremented by its `nlookup`; cached pages are evicted only
    /// for inodes whose count reaches zero.
    pub fn batch_forget(&mut self, entries: &[(u32, u64)]) {
        for &(ino, nlookup) in entries {
            let should_evict = self.decrement_node_state(ino, nlookup);
            if should_evict {
                self.volume.page_cache.evict(ino);
            }
        }
    }

    /// Increment the lookup reference count for an inode (called on LOOKUP/CREATE/MKNOD).
    pub fn add_node_state(&mut self, ino: u32) {
        self.node_states
            .entry(ino)
            .and_modify(|s| s.increment(1))
            .or_insert_with(NodeState::new);
    }

    /// Decrement the lookup reference count by `nlookup`.
    /// Returns `true` if the count reached zero (page cache can be evicted).
    fn decrement_node_state(&mut self, ino: u32, nlookup: u64) -> bool {
        let should_evict = if let Some(state) = self.node_states.get_mut(&ino) {
            state.decrement(nlookup);
            state.is_zero()
        } else {
            false
        };
        if should_evict {
            self.node_states.remove(&ino);
        }
        should_evict
    }

    /// Set file attributes (writable builds only).
    ///
    /// Supports chmod (mode), chown (uid/gid), utimens (atime/mtime),
    /// and truncate (size). Unspecified attributes are left unchanged.
    #[cfg(feature = "writable")]
    pub fn setattr(
        &mut self,
        ino: u32,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: Option<u64>,
        mtime: Option<u64>,
        size: Option<u64>,
    ) -> FuseResult<()> {
        if !self.writable {
            return Err(EROFS);
        }
        if let Some(new_size) = size {
            self.truncate(ino, new_size).ok_or(EIO)?;
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
        let name = translate_xattr_name(name);
        self.volume.setxattr(ino, &name, value, flags).map_err(|e| storage_error_to_errno(&e))
    }

    /// Get an extended attribute value (writable builds only).
    ///
    /// Returns `Ok(None)` if the attribute does not exist.
    #[cfg(feature = "writable")]
    pub fn getxattr(&mut self, ino: u32, name: &str) -> FuseResult<Option<Vec<u8>>> {
        if !self.writable {
            return Err(EROFS);
        }
        let name = translate_xattr_name(name);
        self.volume.getxattr(ino, &name).map_err(|e| storage_error_to_errno(&e))
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
        self.volume.symlink(parent_ino, name, target)
            .map(|ino| {
                self.add_node_state(ino);
                ino
            })
            .map_err(|e| storage_error_to_errno(&e))
    }

    /// Create a special file (FIFO, char/block device, socket) in a directory.
    ///
    /// Unlike `create` (which always creates regular files), `mknod` supports
    /// special file types via the mode's file-type bits.
    #[cfg(feature = "writable")]
    pub fn mknod(&mut self, parent_ino: u32, name: &str, mode: u32, rdev: u64) -> FuseResult<u32> {
        if !self.writable {
            return Err(EROFS);
        }
        self.volume
            .mknod(parent_ino, name, mode, rdev)
            .map(|ino| {
                self.add_node_state(ino);
                ino
            })
            .map_err(|e| storage_error_to_errno(&e))
    }

    /// Read the target of a symbolic link.
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
        let name = translate_xattr_name(name);
        self.volume.removexattr(ino, &name).map_err(|e| storage_error_to_errno(&e)).and_then(|ok| {
            if ok {
                Ok(())
            } else {
                Err(ENODATA)
            }
        })
    }

    /// List xattr names (writable builds only).
    #[cfg(feature = "writable")]
    pub fn listxattr(&mut self, ino: u32) -> FuseResult<Vec<String>> {
        if !self.writable {
            return Err(EROFS);
        }
        let names = self.volume.listxattr(ino).map_err(|e| storage_error_to_errno(&e))?;
        Ok(names.into_iter().map(|n| reverse_xattr_name(&n)).collect())
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
        self.volume.link_file(parent_ino, name, target_ino).map(|_| ()).map_err(|e| storage_error_to_errno(&e))
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
    /// **Note:** This implementation is **not semantically complete**. When a
    /// conflicting lock exists, it returns `EAGAIN` instead of blocking the
    /// request. In a kernel FUSE context the filesystem would hold the
    /// request open until the lock becomes available; this in-memory
    /// implementation returns `EAGAIN` so the caller can retry. Properly
    /// implementing blocking semantics would require per-file request
    /// queues with condition variables — out of scope for this phase.
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
    ///
    /// **Note:** This implements an in-memory lock table specific to this
    /// FUSE server. FreeBSD's native FUSE uses vnode locking as a fallback
    /// for `flock` when the FUSE_FLOCK_LOCKS capability is not negotiated;
    /// in this implementation all `flock` semantics are handled by the
    /// filesystem itself via the `FUSE_FLOCK_LOCKS` capability. This is
    /// acceptable for a userland implementation but not fully FreeBSD-equivalent.
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

/// Wrapper around `FuseFs` that implements the fuse3 `Filesystem` trait.
///
/// The trait methods take `&self`, so the inner `FuseFs` is protected by a
/// `Mutex`. All operations are dispatched through `FuseFs`'s existing
/// `&mut self` methods via the lock.
pub struct Fuse3Fs {
    /// Shared, mutex-protected filesystem state.
    inner: std::sync::Mutex<FuseFs>,
}

/// Convert an i32 errno to a `fuse3::Errno`.
pub fn errno_to_fuse3(code: i32) -> fuse3::Errno {
    fuse3::Errno::from(code)
}

/// FreeBSD-compatible errno constants.
///
/// These use Linux errno values because the FUSE protocol requires them.
/// On FreeBSD, `ENOSYS` is preferred over `EOPNOTSUPP` for filesystem
/// operations; we normalize via [`errno_to_fuse3`] at the call site.
pub const ENODATA: i32 = 61;
pub const E2BIG: i32 = 7;
pub const ENAMETOOLONG: i32 = 36;

/// Map a `StorageError` to a FUSE errno code.
///
/// This replaces the old string-matching approach with proper variant checks.
/// On FreeBSD, `ENOSYS` (78) is preferred over `EOPNOTSUPP` (95) for
/// unsupported operations — the FUSE daemon normalizes both to the Linux
/// errno value as the FUSE protocol requires.
pub fn storage_error_to_errno(e: &StorageError) -> i32 {
    use StorageError::*;
    match e {
        XattrNotFound => ENODATA,
        XattrAlreadyExists | AlreadyExists => EEXIST,
        XattrNameTooLong => ENAMETOOLONG,
        XattrValueTooLarge | XattrDataTooLarge => E2BIG,
        XattrBufferTooSmall { .. } => ERANGE,
        NoFreeInode => ENOSPC,
        InvalidName => EINVAL,
        NotADirectory => ENOTDIR,
        DirectoryNotEmpty => ENOTEMPTY,
        NotFound => ENOENT,
        InvalidFileType | CannotLinkDir => EPERM,
        NotSupported => EOPNOTSUPP,
        Interrupted => EINTR,
        FaultInjection => EIO,
        _ => EIO,
    }
}

/// FreeBSD-compatible errno for unsupported operations.
///
/// On FreeBSD, `ENOSYS` (78) is preferred over `EOPNOTSUPP` (95) for
/// filesystem-level "not supported" conditions. Both are valid; callers
/// targeting FreeBSD should use this function.
#[cfg(target_os = "freebsd")]
pub fn unsupported_errno() -> i32 {
    78 // ENOSYS
}

impl Fuse3Fs {
    /// Create a new `Fuse3Fs` wrapping an initialized (and writable-enabled) `FuseFs`.
    pub fn new(fs: FuseFs) -> Self {
        Self {
            inner: std::sync::Mutex::new(fs),
        }
    }

    /// Borrow the inner `FuseFs` for a synchronous operation.
    fn with_inner<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut FuseFs) -> R,
    {
        let mut guard = self.inner.lock().unwrap();
        f(&mut guard)
    }
}

impl fuse3::raw::Filesystem for Fuse3Fs {
    type DirEntryStream<'a>
        = futures_util::stream::Iter<std::vec::IntoIter<std::result::Result<DirectoryEntry, fuse3::Errno>>>
    where
        Self: 'a;

    type DirEntryPlusStream<'a>
        = futures_util::stream::Iter<std::vec::IntoIter<std::result::Result<DirectoryEntryPlus, fuse3::Errno>>>
    where
        Self: 'a;

    fn init(&self, _req: Request) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyInit>> + Send + '_>> {
        Box::pin(async {
            Ok(ReplyInit {
                max_write: NonZeroU32::new(1 << 20).unwrap(),
            })
        })
    }

    fn destroy(&self, _req: Request) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        #[cfg(feature = "writable")]
        {
            self.with_inner(|fs| {
                if fs.is_writable() {
                    let _ = fs.volume.umount();
                }
            })
        }
        #[cfg(not(feature = "writable"))]
        {
            let _ = req;
        }
        Box::pin(async {})
    }

    fn lookup(&self, req: Request, parent: fuse3::Inode, name: &std::ffi::OsStr) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyEntry>> + Send + '_>> {
        let name_str = name.to_string_lossy().to_string();
        let ttl = Duration::from_secs(5);

        Box::pin(async move {
            self.with_inner(|fs| {
                let ino = fs.lookup(parent as u32, &name_str).ok_or(ENOENT)?;
                let dinode = fs.getattr(ino).ok_or(ENOENT)?;
                let attr = Self::dinode_attr(fs, ino, &dinode);
                Ok(ReplyEntry {
                    ttl,
                    attr,
                    generation: dinode.generation() as u64,
                })
            })
        })
    }

    fn forget(&self, _req: Request, inode: fuse3::Inode, nlookup: u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let parent = inode as u32;
        let n = nlookup;
        Box::pin(async move {
            self.with_inner(|fs| fs.forget(parent, n));
        })
    }

    fn batch_forget(&self, _req: Request, inodes: &[fuse3::Inode]) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let inodes_vec: Vec<u32> = inodes.iter().map(|&i| i as u32).collect();
        Box::pin(async move {
            self.with_inner(|fs| {
                for inode in &inodes_vec {
                    fs.forget(*inode, 1);
                }
            })
        })
    }

    fn getattr(
        &self,
        _req: Request,
        inode: fuse3::Inode,
        _fh: Option<u64>,
        _flags: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyAttr>> + Send + '_>> {
        let ino = inode as u32;
        let ttl = Duration::from_secs(5);
        Box::pin(async move {
            self.with_inner(|fs| {
                let dinode = fs.getattr(ino).ok_or(ENOENT)?;
                let attr = Self::dinode_attr(fs, ino, &dinode);
                Ok(ReplyAttr { ttl, attr })
            })
        })
    }

    fn setattr(
        &self,
        req: Request,
        inode: fuse3::Inode,
        _fh: Option<u64>,
        set_attr: fuse3::SetAttr,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyAttr>> + Send + '_>> {
        let ino = inode as u32;
        let ttl = Duration::from_secs(5);
         let uid = set_attr.uid;
        let gid = set_attr.gid;
        let mode = set_attr.mode.map(|m| m as u32);
        let atime = set_attr.atime.map(|t| t.sec as u64);
        let mtime = set_attr.mtime.map(|t| t.sec as u64);
        let size = set_attr.size;
        let _ = req;

        Box::pin(async move {
            self.with_inner(|fs| {
                match fs.setattr(
                    ino,
                    mode,
                    uid,
                    gid,
                    atime,
                    mtime,
                    size,
                ) {
                    Ok(()) => {
                        let dinode = fs.getattr(ino).ok_or(ENOENT)?;
                        let attr = Self::dinode_attr(fs, ino, &dinode);
                        Ok(ReplyAttr { ttl, attr })
                    }
                    Err(e) => Err(e.into()),
                }
            })
        })
    }

    fn readlink(&self, _req: Request, inode: fuse3::Inode) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyData>> + Send + '_>> {
        let ino = inode as u32;
        Box::pin(async move {
            self.with_inner(|fs| {
                let data = fs.readlink(ino).map_err(|e| {
                    let code: i32 = e;
                    errno_to_fuse3(code)
                })?;
                let bytes = if data.len() > 128 {
                    data[..128].to_vec()
                } else {
                    data
                };
                Ok(ReplyData { data: bytes.into() })
            })
        })
    }

    fn symlink(
        &self,
        _req: Request,
        parent: fuse3::Inode,
        name: &std::ffi::OsStr,
        link: &std::ffi::OsStr,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyEntry>> + Send + '_>> {
        let parent_ino = parent as u32;
        let name_str = name.to_string_lossy().to_string();
        let link_str = link.to_string_lossy().to_string();
        let ttl = Duration::from_secs(5);

        Box::pin(async move {
            self.with_inner(|fs| {
                let ino = fs.symlink(parent_ino, &name_str, &link_str).map_err(|e| {
                    let code: i32 = e;
                    errno_to_fuse3(code)
                })?;
                let dinode = fs.getattr(ino).ok_or(ENOENT)?;
                let attr = Self::dinode_attr(fs, ino, &dinode);
                Ok(ReplyEntry {
                    ttl,
                    attr,
                    generation: dinode.generation() as u64,
                })
            })
        })
    }

    fn mknod(
        &self,
        req: Request,
        parent: fuse3::Inode,
        name: &std::ffi::OsStr,
        mode: u32,
        rdev: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyEntry>> + Send + '_>> {
        let parent_ino = parent as u32;
        let name_str = name.to_string_lossy().to_string();
        let ttl = Duration::from_secs(5);

        Box::pin(async move {
            let _ = req;
            self.with_inner(|fs| {
                let ino = fs.mknod(parent_ino, &name_str, mode, rdev as u64).map_err(|e| {
                    let code: i32 = e;
                    errno_to_fuse3(code)
                })?;
                let dinode = fs.getattr(ino).ok_or(ENOENT)?;
                let attr = Self::dinode_attr(fs, ino, &dinode);
                Ok(ReplyEntry {
                    ttl,
                    attr,
                    generation: dinode.generation() as u64,
                })
            })
        })
    }

    fn mkdir(
        &self,
        req: Request,
        parent: fuse3::Inode,
        name: &std::ffi::OsStr,
        mode: u32,
        _umask: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyEntry>> + Send + '_>> {
        let parent_ino = parent as u32;
        let name_str = name.to_string_lossy().to_string();
        let ttl = Duration::from_secs(5);

        Box::pin(async move {
            let _ = req;
            let _ = _umask;
            self.with_inner(|fs| {
                let ino = fs.mkdir(parent_ino, &name_str, mode).map_err(|e| {
                    let code: i32 = e;
                    errno_to_fuse3(code)
                })?;
                let dinode = fs.getattr(ino).ok_or(ENOENT)?;
                let attr = Self::dinode_attr(fs, ino, &dinode);
                Ok(ReplyEntry {
                    ttl,
                    attr,
                    generation: dinode.generation() as u64,
                })
            })
        })
    }

    fn unlink(&self, req: Request, parent: fuse3::Inode, name: &std::ffi::OsStr) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        let parent_ino = parent as u32;
        let name_str = name.to_string_lossy().to_string();
        let _ = req;

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    if fs.unlink(parent_ino, &name_str).unwrap_or(false) {
                        Ok(())
                    } else {
                        Err(ENOENT.into())
                    }
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (parent_ino, name_str);
                    Err(EROFS.into())
                }
            })
        })
    }

    fn rmdir(&self, req: Request, parent: fuse3::Inode, name: &std::ffi::OsStr) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        let parent_ino = parent as u32;
        let name_str = name.to_string_lossy().to_string();
        let _ = req;

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    fs.rmdir(parent_ino, &name_str).map_err(|e| {
                        let code: i32 = e;
                        errno_to_fuse3(code)
                    })
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (parent_ino, name_str);
                    Err(EROFS.into())
                }
            })
        })
    }

    fn rename(
        &self,
        req: Request,
        parent: fuse3::Inode,
        name: &std::ffi::OsStr,
        new_parent: fuse3::Inode,
        new_name: &std::ffi::OsStr,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        let old_parent = parent as u32;
        let old_name = name.to_string_lossy().to_string();
        let new_p = new_parent as u32;
        let new_name = new_name.to_string_lossy().to_string();
        let _ = req;

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    fs.rename(old_parent, &old_name, new_p, &new_name).map_err(|e| {
                        let code: i32 = e;
                        errno_to_fuse3(code)
                    })
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (old_parent, old_name, new_p, new_name);
                    Err(EROFS.into())
                }
            })
        })
    }

    fn link(
        &self,
        req: Request,
        inode: fuse3::Inode,
        new_parent: fuse3::Inode,
        new_name: &std::ffi::OsStr,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyEntry>> + Send + '_>> {
        let target_ino = inode as u32;
        let parent_ino = new_parent as u32;
        let name_str = new_name.to_string_lossy().to_string();
        let ttl = Duration::from_secs(5);
        let _ = req;

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                fs.link(parent_ino, &name_str, target_ino).map_err(|e| {
                    let code: i32 = e;
                    errno_to_fuse3(code)
                })?;
                let dinode = fs.getattr(target_ino).ok_or(ENOENT)?;
                let attr = Self::dinode_attr(fs, target_ino, &dinode);
                Ok(ReplyEntry {
                    ttl,
                    attr,
                    generation: dinode.generation() as u64,
                })
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (parent_ino, name_str, target_ino);
                    Err(EROFS.into())
                }
            })
        })
    }

    fn open(&self, req: Request, inode: fuse3::Inode, flags: u32) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyOpen>> + Send + '_>> {
        let ino = inode as u32;
        let _ = req;

         Box::pin(async move {
            self.with_inner(|fs| {
                fs.open(ino).map_err(|e| {
                    let code: i32 = e;
                    errno_to_fuse3(code)
                })?;
                let fh = fs.open_handle(ino, false);
                let _ = flags;
                Ok(ReplyOpen { fh, flags: 0 })
            })
        })
    }

    fn read(
        &self,
        req: Request,
        inode: fuse3::Inode,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyData>> + Send + '_>> {
        let ino = inode as u32;
        let req_id = req.unique;

        Box::pin(async move {
            self.with_inner(|fs| {
                Self::validate_handle_static(&fs.handles, fh, ino, false)
                    .map_err(|e| errno_to_fuse3(e))?;
                let (_id, token) = fs.register_request_at(req_id);
                let result = fs.read_interruptible(ino, offset, size as usize, &token);
                fs.deregister_request(_id);
                result.map(|data| ReplyData { data: data.into() }).map_err(|e| {
                    errno_to_fuse3(e)
                })
            })
        })
    }

    fn write(
        &self,
        req: Request,
        inode: fuse3::Inode,
        fh: u64,
        offset: u64,
        data: &[u8],
        write_flags: u32,
        flags: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyWrite>> + Send + '_>> {
        let ino = inode as u32;
        let data_vec = data.to_vec();
        let req_id = req.unique;
        let _ = flags;

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    Self::validate_handle_static(&fs.handles, fh, ino, false)
                        .map_err(errno_to_fuse3)?;
                    let (_id, token) = fs.register_request_at(req_id);
                    let written = fs.write_interruptible(ino, offset, &data_vec, &token);
                    fs.deregister_request(_id);
                    match written {
                        Ok(n) => {
                            if write_flags & 1 != 0 {
                                fs.flush(ino).ok_or_else(|| errno_to_fuse3(EIO))?;
                            }
                            Ok(ReplyWrite { written: n as u32 })
                        }
                        Err(e) => Err(errno_to_fuse3(e)),
                    }
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (ino, offset, data_vec);
                    Err(EROFS.into())
                }
            })
        })
    }

    fn statfs(&self, _req: Request, _inode: fuse3::Inode) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyStatFs>> + Send + '_>> {
        Box::pin(async {
            self.with_inner(|fs| {
                let stat = fs.statfs(0);
                Ok(ReplyStatFs {
                    blocks: stat.blocks,
                    bfree: stat.bfree,
                    bavail: stat.bavail,
                    files: stat.files,
                    ffree: stat.ffree,
                    bsize: stat.bsize,
                    namelen: stat.namelen,
                    frsize: stat.blksize,
                })
            })
        })
    }

    fn release(
        &self,
        _req: Request,
        inode: fuse3::Inode,
        fh: u64,
        _flags: u32,
        lock_owner: u64,
        flush: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        let ino = inode as u32;
        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    if flush {
                        fs.flush(ino).ok_or(EIO)?;
                    }
                    let _ = fs.release_handle(fh);
                    fs.release_locks(ino, lock_owner);
                    fs.release(ino).map_err(errno_to_fuse3)?;
                    Ok(())
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (flush, lock_owner);
                    let _ = fs.release_handle(fh);
                    fs.release(ino)?;
                    Ok(())
                }
            })
        })
    }

    fn fsync(&self, _req: Request, inode: fuse3::Inode, _fh: u64, datasync: bool) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        let ino = inode as u32;
        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    if datasync {
                        fs.flush(ino).ok_or(EIO)?;
                    } else {
                        fs.sync_fs().map_err(errno_to_fuse3)?;
                        fs.flush(ino).ok_or(EIO)?;
                    }
                    Ok(())
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = ino;
                    Ok(())
                }
            })
        })
    }

    fn setxattr(
        &self,
        req: Request,
        inode: fuse3::Inode,
        name: &std::ffi::OsStr,
        value: &[u8],
        flags: u32,
        _position: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        let ino = inode as u32;
        let name_str = name.to_string_lossy().to_string();
        let value_vec = value.to_vec();
        let _ = req;

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    fs.setxattr(ino, &name_str, &value_vec, flags).map_err(|e| {
                        let code: i32 = e;
                        errno_to_fuse3(code)
                    })
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (ino, name_str, value_vec, flags);
                    Err(EROFS.into())
                }
            })
        })
    }

    fn getxattr(
        &self,
        _req: Request,
        inode: fuse3::Inode,
        name: &std::ffi::OsStr,
        size: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyXAttr>> + Send + '_>> {
         let ino = inode as u32;
        let name_str = name.to_string_lossy().to_string();

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    let result = fs.getxattr(ino, &name_str).map_err(|e| {
                        let code: i32 = e;
                        errno_to_fuse3(code)
                    })?;
                    match result {
                        Some(data) => {
                            if size == 0 {
                                Ok(ReplyXAttr::Size(data.len() as u32))
                            } else if (data.len() as u32) <= size {
                                Ok(ReplyXAttr::Data(data.into()))
                            } else {
                                Err(fuse3::Errno::from(ERANGE))
                            }
                        }
                        None => Err(fuse3::Errno::from(ENODATA)),
                    }
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (ino, name_str, size);
                    Err(EROFS.into())
                }
            })
        })
    }

    fn listxattr(&self, _req: Request, inode: fuse3::Inode, size: u32) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyXAttr>> + Send + '_>> {
        let ino = inode as u32;

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    let names = fs.listxattr(ino).map_err(|e| {
                        let code: i32 = e;
                        errno_to_fuse3(code)
                    })?;
                    let mut buf = Vec::new();
                    for name in &names {
                        let bytes = name.as_bytes();
                        buf.extend_from_slice(bytes);
                        buf.push(0);
                    }
                    if size == 0 {
                        Ok(ReplyXAttr::Size(buf.len() as u32))
                    } else if buf.len() as u32 <= size {
                        Ok(ReplyXAttr::Data(buf.into()))
                    } else {
                        Err(fuse3::Errno::from(ERANGE))
                    }
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (ino, size);
                    Err(EROFS.into())
                }
            })
        })
    }

    fn removexattr(&self, req: Request, inode: fuse3::Inode, name: &std::ffi::OsStr) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        let ino = inode as u32;
        let name_str = name.to_string_lossy().to_string();
        let _ = req;

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    fs.removexattr(ino, &name_str).map_err(|e| {
                        let code: i32 = e;
                        errno_to_fuse3(code)
                    })
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (ino, name_str);
                    Err(EROFS.into())
                }
            })
        })
    }

    fn flush(&self, _req: Request, inode: fuse3::Inode, _fh: u64, _lock_owner: u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        let ino = inode as u32;

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    fs.flush(ino).ok_or(EIO)?;
                    Ok(())
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = ino;
                    Ok(())
                }
            })
        })
    }

    fn opendir(&self, req: Request, inode: fuse3::Inode, flags: u32) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyOpen>> + Send + '_>> {
        let ino = inode as u32;
        let _ = (req, flags);

        Box::pin(async move {
            self.with_inner(|fs| {
                fs.open(ino).map_err(|e| {
                    let code: i32 = e;
                    errno_to_fuse3(code)
                })?;
                let fh = fs.open_handle(ino, true);
                Ok(ReplyOpen { fh, flags: 0 })
            })
        })
    }

    fn readdir<'a>(
        &'a self,
        req: Request,
        parent: fuse3::Inode,
        fh: u64,
        offset: i64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = fuse3::Result<ReplyDirectory<Self::DirEntryStream<'a>>>,
                > + Send + 'a,
            >,
    > {
        let parent_ino = parent as u32;
        let off = offset as u64;
        let req_id = req.unique;

        Box::pin(async move {
            let entries: Vec<(String, u32, u8, u64)> = self.with_inner(|fs| -> Result<_, fuse3::Errno> {
                Self::validate_handle_static(&fs.handles, fh, parent_ino, true)
                    .map_err(errno_to_fuse3)?;
                let (_id, token) = fs.register_request_at(req_id);
                let result = fs.readdir_interruptible(parent_ino, off, &token);
                fs.deregister_request(_id);
                result.map_err(errno_to_fuse3)
            })?;

            let stream_items: Vec<std::result::Result<DirectoryEntry, fuse3::Errno>> = entries
                .into_iter()
                .map(|(name, ino, kind, cookie)| {
                    let file_type = Self::fuse_file_type(kind);
                    let entry = DirectoryEntry {
                        inode: ino as u64,
                        kind: file_type,
                        name: std::ffi::OsString::from(name),
                        offset: cookie as i64,
                    };
                    Ok(entry)
                })
                .collect();

            Ok(ReplyDirectory {
                entries: futures_util::stream::iter(stream_items),
            })
        })
    }

    fn releasedir(&self, _req: Request, inode: fuse3::Inode, fh: u64, _flags: u32) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        let ino = inode as u32;
        Box::pin(async move {
            self.with_inner(|fs| -> FuseResult<()> {
                let _ = fs.release_handle(fh);
                fs.release(ino)?;
                Ok(())
            })?;
            Ok(())
        })
    }

    fn fsyncdir(&self, _req: Request, _inode: fuse3::Inode, _fh: u64, _datasync: bool) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    fs.sync_fs().map_err(|e| errno_to_fuse3(e))?;
                    Ok(())
                }
                #[cfg(not(feature = "writable"))]
                {
                    Ok(())
                }
            })
        })
    }

    fn access(&self, req: Request, inode: fuse3::Inode, mask: u32) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        let ino = inode as u32;
        let uid = req.uid;
        let gid = req.gid;

        Box::pin(async move {
            self.with_inner(|fs| {
                match fs.access(ino, mask, uid, gid) {
                    Ok(()) => Ok(()),
                    Err(e) => Err(fuse3::Errno::from(e)),
                }
            })
        })
    }

    fn create(
        &self,
        req: Request,
        parent: fuse3::Inode,
        name: &std::ffi::OsStr,
        mode: u32,
        _flags: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyCreated>> + Send + '_>> {
        let parent_ino = parent as u32;
        let name_str = name.to_string_lossy().to_string();
        let ttl = Duration::from_secs(5);
        let _ = req;

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    let ino = fs.create(parent_ino, &name_str, mode).ok_or(EIO)?;
                    let dinode = fs.getattr(ino).ok_or(ENOENT)?;
                    let attr = Self::dinode_attr(fs, ino, &dinode);
                    fs.open(ino).map_err(|e| errno_to_fuse3(e))?;
                    let fh = fs.open_handle(ino, false);
                    Ok(ReplyCreated {
                        ttl,
                        attr,
                        generation: dinode.generation() as u64,
                        fh,
                        flags: 0,
                    })
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (parent_ino, name_str, mode);
                    Err(EROFS.into())
                }
            })
        })
    }

    fn interrupt(&self, _req: Request, unique: u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        let req_id: RequestId = unique;
        Box::pin(async move {
            self.with_inner(|fs| {
                fs.handle_interrupt(req_id);
            });
            Ok(())
        })
    }

    fn bmap(&self, _req: Request, inode: fuse3::Inode, blocksize: u32, idx: u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<fuse3::raw::reply::ReplyBmap>> + Send + '_>> {
        let ino = inode as u32;
        Box::pin(async move {
            self.with_inner(|fs| {
                let fs_block_size = crate::storage::BLOCK_SIZE as u32;
                if blocksize != 0 && blocksize != fs_block_size {
                    return Err(EINVAL.into());
                }
                let (block, _nblocks) = fs.bmap(ino, idx).map_err(|e| {
                    let code: i32 = e;
                    errno_to_fuse3(code)
                })?;
                Ok(fuse3::raw::reply::ReplyBmap { block })
            })
        })
    }

    fn poll(
        &self,
        _req: Request,
        _inode: fuse3::Inode,
        _fh: u64,
        _kh: Option<u64>,
        _flags: u32,
        _events: u32,
        _notify: &fuse3::notify::Notify,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<fuse3::raw::reply::ReplyPoll>> + Send + '_>> {
        Box::pin(async { Err(fuse3::Errno::from(EOPNOTSUPP)) })
    }

    fn fallocate(
        &self,
        req: Request,
        inode: fuse3::Inode,
        fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        let ino = inode as u32;
        let _ = req;

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    Self::validate_handle_static(&fs.handles, fh, ino, false)
                        .map_err(errno_to_fuse3)?;
                    fs.fallocate(ino, offset, length, mode).map_err(|e| {
                        let code: i32 = e;
                        errno_to_fuse3(code)
                    })?;
                    Ok(())
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (ino, offset, length, mode);
                    Err(EROFS.into())
                }
            })
        })
    }

    fn readdirplus<'a>(
        &'a self,
        req: Request,
        parent: fuse3::Inode,
        fh: u64,
        offset: u64,
        lock_owner: u64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = fuse3::Result<ReplyDirectoryPlus<Self::DirEntryPlusStream<'a>>>,
                > + Send + 'a,
            >,
    > {
        let parent_ino = parent as u32;
        let _ = lock_owner;
        let req_id = req.unique;
        let ttl = Duration::from_secs(5);

         Box::pin(async move {
            let entries: Vec<(String, u32, u8, Dinode)> = self.with_inner(|fs| -> Result<_, fuse3::Errno> {
                Self::validate_handle_static(&fs.handles, fh, parent_ino, true)
                    .map_err(errno_to_fuse3)?;
                let (_id, token) = fs.register_request_at(req_id);
                let basic_entries = fs.readdir_interruptible(parent_ino, offset, &token);
                fs.deregister_request(_id);
                let basic_entries: Vec<(String, u32, u8, u64)> = basic_entries.map_err(errno_to_fuse3)?;
                let mut full_entries = Vec::new();
                for (name, ino, kind, cookie) in basic_entries {
                    if let Some(dinode) = fs.getattr(ino) {
                        full_entries.push((name, ino, kind, dinode));
                    }
                }
                Ok(full_entries)
            })?;
            let attr_ttl = Duration::from_secs(5);

            let stream_items: Vec<std::result::Result<DirectoryEntryPlus, fuse3::Errno>> = entries
                .into_iter()
                .map(|(name, ino, kind, dinode)| {
                    let file_type = match kind {
                        0x04 => FileType::Directory,
                        0x08 => FileType::RegularFile,
                        0x0A | 0xA0 => FileType::Symlink,
                        _ => FileType::RegularFile,
                    };
                    let attr = Self::dinode_attr_from(&dinode, ino);
                    let entry = DirectoryEntryPlus {
                        inode: ino as u64,
                        generation: dinode.generation() as u64,
                        kind: file_type,
                        name: std::ffi::OsString::from(name),
                        offset: 0i64,
                        attr,
                        entry_ttl: ttl,
                        attr_ttl,
                    };
                    Ok(entry)
                })
                .collect();

            Ok(ReplyDirectoryPlus {
                entries: futures_util::stream::iter(stream_items),
            })
        })
    }

    fn rename2(
        &self,
        req: Request,
        parent: fuse3::Inode,
        name: &std::ffi::OsStr,
        new_parent: fuse3::Inode,
        new_name: &std::ffi::OsStr,
        flags: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<()>> + Send + '_>> {
        const RENAME_NOREPLACE: u32 = 1 << 0;
        const RENAME_EXCHANGE: u32 = 1 << 1;

        let old_parent = parent as u32;
        let old_name = name.to_string_lossy().to_string();
        let new_p = new_parent as u32;
        let new_name = new_name.to_string_lossy().to_string();
        let _ = req;

        Box::pin(async move {
            if flags & RENAME_EXCHANGE != 0 {
                return Err(EOPNOTSUPP.into());
            }
            if flags & RENAME_NOREPLACE != 0 {
                return Err(EOPNOTSUPP.into());
            }
            if flags != 0 {
                return Err(EINVAL.into());
            }
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    fs.rename(old_parent, &old_name, new_p, &new_name).map_err(|e| {
                        let code: i32 = e;
                        errno_to_fuse3(code)
                    })
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (old_parent, old_name, new_p, new_name);
                    Err(EROFS.into())
                }
            })
        })
    }

    fn lseek(
        &self,
        req: Request,
        inode: fuse3::Inode,
        fh: u64,
        offset: u64,
        whence: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyLSeek>> + Send + '_>> {
        let ino = inode as u32;
        let _ = req;

        Box::pin(async move {
            self.with_inner(|fs| {
                Self::validate_handle_static(&fs.handles, fh, ino, false)
                    .map_err(errno_to_fuse3)?;
                const SEEK_DATA: u32 = 3;
                const SEEK_HOLE: u32 = 4;
                if whence != SEEK_DATA && whence != SEEK_HOLE {
                    return Err(EINVAL.into());
                }
                match fs.lseek_data_or_hole(ino, offset, whence as i32) {
                    Some(result) => Ok(ReplyLSeek { offset: result }),
                    None => Err(ENXIO.into()),
                }
            })
        })
    }

    fn copy_file_range(
        &self,
        req: Request,
        inode: fuse3::Inode,
        fh_in: u64,
        off_in: u64,
        inode_out: fuse3::Inode,
        fh_out: u64,
        off_out: u64,
        length: u64,
        flags: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = fuse3::Result<ReplyCopyFileRange>> + Send + '_>> {
         let src_ino = inode as u32;
        let dst_ino = inode_out as u32;
        let _ = req;

        Box::pin(async move {
            self.with_inner(|fs| {
                #[cfg(feature = "writable")]
                {
                    if src_ino == dst_ino && off_in == off_out {
                        return Err(EINVAL.into());
                    }
                    // FreeBSD sends flags=0. Reject all unknown flag
                    // combinations rather than inventing behavior.
                    if flags != 0 {
                        return Err(EOPNOTSUPP.into());
                    }
                    Self::validate_handle_static(&fs.handles, fh_in, src_ino, false)
                        .map_err(errno_to_fuse3)?;
                    Self::validate_handle_static(&fs.handles, fh_out, dst_ino, false)
                        .map_err(errno_to_fuse3)?;
                    let copied = fs.copy_file_range(
                        src_ino,
                        off_in,
                        dst_ino,
                        off_out,
                        length as usize,
                        flags as u32,
                    ).map_err(|e| {
                        let code: i32 = e;
                        errno_to_fuse3(code)
                    })?;
                    Ok(ReplyCopyFileRange { copied: copied as u64 })
                }
                #[cfg(not(feature = "writable"))]
                {
                    let _ = (src_ino, fh_in, off_in, dst_ino, fh_out, off_out, length, flags);
                    Err(EROFS.into())
                }
            })
        })
    }
}

impl Fuse3Fs {
    /// Build a `FileAttr` from an inode's dinode and number.
    fn dinode_attr(fs: &mut FuseFs, ino: u32, dinode: &Dinode) -> fuse3::raw::reply::FileAttr {
        FuseFs::dinode_to_fileattr(ino, dinode)
    }

    /// Build a `FileAttr` from a raw `Dinode` without needing a `FuseFs` reference.
    fn dinode_attr_from(dinode: &Dinode, ino: u32) -> fuse3::raw::reply::FileAttr {
        FuseFs::dinode_to_fileattr(ino, dinode)
    }

    /// Map a FUSE DT_* type byte to a `fuse3::FileType`.
    fn fuse_file_type(dt: u8) -> FileType {
        match dt {
            0x04 => FileType::Directory,
            0x08 => FileType::RegularFile,
            0x0A | 0xA0 => FileType::Symlink,
            0x01 => FileType::NamedPipe,
            0x02 => FileType::CharDevice,
            0x06 => FileType::BlockDevice,
            0x0C | 0x0E => FileType::Socket,
            _ => FileType::RegularFile,
        }
    }

    /// Validate that `fh` refers to `expected_ino` and is not a directory
    /// (when `expect_file` is true) or is a directory (when false).
    /// Returns `EBADF` for unknown handles, `EISDIR`/`ENOTDIR` for type mismatch.
    fn validate_handle(&self, fh: u64, expected_ino: u32, expect_file: bool) -> FuseResult<()> {
        let handles = self.with_inner(|fs| fs.handles.get(&fh).cloned());
        match handles {
            Some(h) if h.ino == expected_ino => {
                if expect_file && h.is_dir {
                    Err(EISDIR)
                } else if !expect_file && !h.is_dir {
                    Err(ENOTDIR)
                } else {
                    Ok(())
                }
            }
            Some(_) => Err(EBADF),
            None => Err(EBADF),
        }
    }

    /// Static version of validate_handle for use within `with_inner`.
    fn validate_handle_static(handles: &HashMap<u64, OpenHandle>, fh: u64, expected_ino: u32, expect_file: bool) -> FuseResult<()> {
        match handles.get(&fh) {
            Some(h) if h.ino == expected_ino => {
                if expect_file && h.is_dir {
                    Err(EISDIR)
                } else if !expect_file && !h.is_dir {
                    Err(ENOTDIR)
                } else {
                    Ok(())
                }
            }
            Some(_) => Err(EBADF),
            None => Err(EBADF),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StorageError;

    #[test]
    fn test_xattr_errno_mapping() {
        assert_eq!(storage_error_to_errno(&StorageError::XattrNotFound), ENODATA);
        assert_eq!(storage_error_to_errno(&StorageError::XattrAlreadyExists), EEXIST);
        assert_eq!(storage_error_to_errno(&StorageError::XattrNameTooLong), ENAMETOOLONG);
        assert_eq!(storage_error_to_errno(&StorageError::XattrValueTooLarge), E2BIG);
        assert_eq!(
            storage_error_to_errno(&StorageError::XattrDataTooLarge),
            E2BIG
        );
        assert_eq!(
            storage_error_to_errno(&StorageError::XattrBufferTooSmall { needed: 128 }),
            ERANGE
        );
    }

    #[test]
    fn test_fs_errno_mapping() {
        assert_eq!(storage_error_to_errno(&StorageError::AlreadyExists), EEXIST);
        assert_eq!(storage_error_to_errno(&StorageError::InvalidName), EINVAL);
        assert_eq!(storage_error_to_errno(&StorageError::NoFreeInode), ENOSPC);
        assert_eq!(storage_error_to_errno(&StorageError::NotADirectory), ENOTDIR);
        assert_eq!(storage_error_to_errno(&StorageError::DirectoryNotEmpty), ENOTEMPTY);
        assert_eq!(storage_error_to_errno(&StorageError::NotFound), ENOENT);
        assert_eq!(storage_error_to_errno(&StorageError::InvalidFileType), EPERM);
        assert_eq!(storage_error_to_errno(&StorageError::NotSupported), EOPNOTSUPP);
        assert_eq!(storage_error_to_errno(&StorageError::CannotLinkDir), EPERM);
        assert_eq!(storage_error_to_errno(&StorageError::Interrupted), EINTR);
    }

    #[test]
    fn test_fallback_errno_mapping() {
        assert_eq!(storage_error_to_errno(&StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            "test"
        ))), EIO);
        assert_eq!(storage_error_to_errno(&StorageError::Other("something".to_string())), EIO);
    }

    #[test]
    fn test_translate_xattr_name() {
        assert_eq!(translate_xattr_name("user.foo"), "foo");
        assert_eq!(translate_xattr_name("system.bar"), "system.bar");
        assert_eq!(translate_xattr_name("bare"), "bare");
    }

    #[test]
    fn test_reverse_xattr_name() {
        assert_eq!(reverse_xattr_name("foo"), "user.foo");
        assert_eq!(reverse_xattr_name("user.foo"), "user.foo");
        assert_eq!(reverse_xattr_name("system.bar"), "system.bar");
    }
}
