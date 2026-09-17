// SPDX-License-Identifier: GPL-2.0-or-later
//! B+-tree managers module.
//!
//! Re-exports the xtree (extent descriptor B+-tree) and dtree (directory B+-tree)
//! managers. Both share the common `btpage` infrastructure.

pub mod dtree;
pub mod xtree;

pub use dtree::*;
pub use xtree::*;
