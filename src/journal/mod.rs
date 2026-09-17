// SPDX-License-Identifier: GPL-2.0-or-later
//! Journal subsystem — log I/O and crash recovery.
//!
//! Mirrors the kernel `jfs_logmgr.c` (log I/O) and the jfsutils `logredo.c`
//! (crash recovery). In FUSE, logredo runs once at mount time before any
//! client I/O is accepted.
//!
//! ## Write-path design
//!
//! The journal is the central dependency for safe mutation. Write support
//! will extend this module with:
//!
//! 1. Log-page allocation.
//! 2. Log-record serialization (transaction begin/update/commit records).
//! 3. Redo information for metadata blocks.
//! 4. Log wrapping and checkpointing.
//! 5. Journal flush and durability barriers.
//! 6. Recovery of committed but uncheckpointed transactions.
//! 7. Discarding or rolling back incomplete transactions.
//!
//! The existing backward-replay recovery (`JournalRecovery`) should be
//! extended rather than replaced. The first writable milestone supports
//! only metadata updates, allocation-map updates, inode updates, and
//! directory-tree updates — file-data journaling is deferred in favor of
//! ordered data writes.
//!
//! **Consistency model (ordered data):**
//! 1. allocate and initialize data blocks;
//! 2. flush data blocks;
//! 3. journal metadata describing those blocks;
//! 4. commit metadata transaction;
//! 5. expose the new file state.

pub mod logmgr;
pub mod recovery;

pub use logmgr::*;
pub use recovery::*;
