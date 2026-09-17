// SPDX-License-Identifier: GPL-2.0-or-later
//! Allocation maps module.
//!
//! Re-exports the block allocation map (dmap) and inode allocation map (imap).

pub mod dmap;
pub mod imap;

pub use dmap::*;
pub use imap::*;
