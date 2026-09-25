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

#![allow(
    clippy::manual_div_ceil,
    clippy::manual_is_multiple_of,
    clippy::unnecessary_cast,
    clippy::collapsible_if,
    clippy::needless_range_loop,
    clippy::redundant_closure,
    clippy::manual_repeat_n,
    clippy::redundant_slicing,
    clippy::field_reassign_with_default,
    clippy::identity_op,
    clippy::unnecessary_min_or_max,
    clippy::manual_checked_ops,
    clippy::manual_range_contains,
    clippy::if_same_then_else,
    clippy::let_and_return,
    clippy::manual_inspect,
    clippy::unwrap_or_default,
    clippy::while_let_loop,
    clippy::needless_return,
    clippy::let_unit_value,
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::chunks_exact_to_as_chunks,
    clippy::derivable_impls,
    clippy::match_like_matches_macro,
    clippy::single_match,
    clippy::clone_on_copy,
    clippy::io_other_error,
    clippy::needless_borrows_for_generic_args,
    clippy::items_after_test_module,
    clippy::empty_line_after_doc_comments,
    clippy::vec_init_then_push,
    dead_code,
    unused_mut,
    unused_variables,
    unused_must_use
)]

pub mod alloc;
pub mod btree;
pub mod fuse;
pub mod inode;
pub mod journal;
pub mod mkfs;
pub mod storage;
pub mod transaction;
pub mod types;
pub mod volume;
