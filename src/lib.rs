// SPDX-License-Identifier: BSD-2-Clause
//! jfsfuse — a Rust reimplementation of the Linux JFS filesystem, exposed over FUSE.
//!
//! The architecture mirrors the kernel JFS layout:
//! - [`types`] — on-disk structure definitions (pxd_t, dinode, lrd, logsuper, etc.)
//! - [`storage`] — block I/O abstraction + page cache (metapage replacement)
//! - [`journal`] — journal crash recovery (logredo) and log I/O
//! - [`alloc`] — block and inode allocation maps (dmap, imap)
//! - [`btree`] — B+-tree managers (xtree extents, dtree directories)
//! - [`inode`] — on-disk inode access
//! - [`volume`] — volume mount, superblock parsing
//! - [`transaction`] — transaction manager for journaled write transactions
//! - [`fuse`] — FUSE filesystem operations adapter

pub mod mkfs;
pub mod alloc;
pub mod btree;
pub mod fuse;
pub mod inode;
pub mod journal;
pub mod storage;
pub mod transaction;
pub mod types;
pub mod volume;
