// SPDX-License-Identifier: GPL-2.0-or-later
//! FUSE protocol constants — capability flags and opcodes.
//!
//! These constants match the Linux FUSE ABI as defined in
//! `include/uapi/linux/fuse.h`. Capability flags are negotiated
//! during FUSE_INIT (the `flags` field of `fuse_init_in`).
//!
//! Opcodes identify the type of a FUSE request.

//! Capability flags (negotiated in FUSE_INIT `flags` field, bits 0-31).

/// Asynchronous read requests — reads may be reordered by the kernel.
pub const FUSE_ASYNC_READ: u32 = 1 << 0;
/// Remote locking for POSIX file locks (fcntl-style).
pub const FUSE_POSIX_LOCKS: u32 = 1 << 1;
/// Kernel sends a file handle for fstat, etc. (not yet supported by jfsfuse).
pub const FUSE_FILE_OPS: u32 = 1 << 2;
/// Handles the O_TRUNC open flag in the filesystem.
pub const FUSE_ATOMIC_O_TRUNC: u32 = 1 << 3;
/// Filesystem handles lookups of "." and "..".
pub const FUSE_EXPORT_SUPPORT: u32 = 1 << 4;
/// Filesystem can handle write size larger than 4 kB.
pub const FUSE_BIG_WRITES: u32 = 1 << 5;
/// Don't apply umask to file mode on create operations.
pub const FUSE_DONT_MASK: u32 = 1 << 6;
/// Kernel supports `splice()` write on the FUSE device.
pub const FUSE_SPLICE_WRITE: u32 = 1 << 7;
/// Kernel supports `splice()` move on the FUSE device.
pub const FUSE_SPLICE_MOVE: u32 = 1 << 8;
/// Kernel supports `splice()` read on the FUSE device.
pub const FUSE_SPLICE_READ: u32 = 1 << 9;
/// Remote locking for BSD-style file locks (`flock(2)`).
pub const FUSE_FLOCK_LOCKS: u32 = 1 << 10;
/// Kernel supports `ioctl` on directories.
pub const FUSE_HAS_IOCTL_DIR: u32 = 1 << 11;
/// Kernel automatically invalidates cached data.
pub const FUSE_AUTO_INVAL_DATA: u32 = 1 << 12;
/// Kernel sends READDIRPLUS.
pub const FUSE_DO_READDIRPLUS: u32 = 1 << 13;
/// Adaptive readdirplus.
pub const FUSE_READDIRPLUS_AUTO: u32 = 1 << 14;
/// Supports async direct I/O.
pub const FUSE_ASYNC_DIO: u32 = 1 << 15;
/// Filesystem supports `bmap()` (via FUSE_BMAP opcode; no separate capability
/// flag in the Linux FUSE ABI — support is implied if the daemon responds).
pub const FUSE_BMAP_SUPPORT: u32 = 0; // feature-gating only, not an ABI flag
/// Filesystem supports writeback caching.
pub const FUSE_WRITEBACK_CACHE: u32 = 1 << 16;
/// Supports parallel directory operations.
pub const FUSE_PARALLEL_DIROPS: u32 = 1 << 18;
/// Kill privileges on write (v1).
pub const FUSE_HANDLE_KILLPRIV: u32 = 1 << 27;
/// Kill privileges on write (v2).
pub const FUSE_HANDLE_KILLPRIV_V2: u32 = 1 << 28;

/// Capability flags in bits 32-63 (shifted into `flags2` by the kernel).
/// These are stored as `1ULL << N` in the kernel header.
pub const FUSE_SECURITY_CTX: u64 = 1u64 << 32;
pub const FUSE_HAS_INODE_DAX: u64 = 1u64 << 33;

/// FUSE opcodes (request types).
pub const FUSE_LOOKUP: u32 = 1;
pub const FUSE_FORGET: u32 = 2;
pub const FUSE_GETATTR: u32 = 3;
pub const FUSE_SETATTR: u32 = 4;
pub const FUSE_READLINK: u32 = 5;
pub const FUSE_SYMLINK: u32 = 6;
pub const FUSE_MKNOD: u32 = 8;
pub const FUSE_MKDIR: u32 = 9;
pub const FUSE_UNLINK: u32 = 10;
pub const FUSE_RMDIR: u32 = 11;
pub const FUSE_RENAME: u32 = 12;
pub const FUSE_LINK: u32 = 13;
pub const FUSE_OPEN: u32 = 14;
pub const FUSE_READ: u32 = 15;
pub const FUSE_WRITE: u32 = 16;
pub const FUSE_STATFS: u32 = 17;
pub const FUSE_RELEASE: u32 = 18;
pub const FUSE_FSYNC: u32 = 20;
pub const FUSE_SETXATTR: u32 = 21;
pub const FUSE_GETXATTR: u32 = 22;
pub const FUSE_LISTXATTR: u32 = 23;
pub const FUSE_REMOVEXATTR: u32 = 24;
pub const FUSE_FLUSH: u32 = 25;
pub const FUSE_INIT: u32 = 26;
pub const FUSE_OPENDIR: u32 = 27;
pub const FUSE_READDIR: u32 = 28;
pub const FUSE_RELEASEDIR: u32 = 29;
pub const FUSE_FSYNCDIR: u32 = 30;
pub const FUSE_GETLK: u32 = 31;
pub const FUSE_SETLK: u32 = 32;
pub const FUSE_SETLKW: u32 = 33;
pub const FUSE_ACCESS: u32 = 34;
pub const FUSE_CREATE: u32 = 35;
pub const FUSE_INTERRUPT: u32 = 36;
pub const FUSE_BMAP: u32 = 37;
pub const FUSE_DESTROY: u32 = 38;
pub const FUSE_IOCTL: u32 = 39;
pub const FUSE_POLL: u32 = 40;
pub const FUSE_NOTIFY_REPLY: u32 = 41;
pub const FUSE_BATCH_FORGET: u32 = 42;
pub const FUSE_FALLOCATE: u32 = 43;
pub const FUSE_READDIRPLUS: u32 = 44;
pub const FUSE_RENAME2: u32 = 45;
pub const FUSE_LSEEK: u32 = 46;
pub const FUSE_COPY_FILE_RANGE: u32 = 47;
pub const FUSE_SETUPMAPPING: u32 = 48;
pub const FUSE_REMOVEMAPPING: u32 = 49;
pub const FUSE_SYNCFS: u32 = 50;
pub const FUSE_TMPFILE: u32 = 51;
pub const FUSE_STATX: u32 = 52;

/// Lock types for `access(2)` / `FUSE_ACCESS`.
pub const F_OK: u32 = 0;
pub const R_OK: u32 = 4;
pub const W_OK: u32 = 2;
pub const X_OK: u32 = 1;
