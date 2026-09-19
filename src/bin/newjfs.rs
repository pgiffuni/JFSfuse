// SPDX-License-Identifier: MIT
//! newjfs — Create a JFS filesystem image.
//!
//! Usage:
//!   newjfs `image` \[size_in_blocks\]
//!
//! Creates a JFS filesystem image file at ``image`` with the given number
//! of 4 KiB blocks (default: 4096, i.e. 16 MiB).

use std::env;
use std::fs::OpenOptions;
use std::process;

use jfsfuse::storage::{BLOCK_SIZE, FileStorage, Storage};
use jfsfuse::types;

const DEFAULT_NUM_BLOCKS: u64 = 4096;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: newjfs `image` [size_in_blocks]");
        eprintln!();
        eprintln!("Create a JFS filesystem image.");
        eprintln!();
        eprintln!("Arguments:");
        eprintln!("  image            Path to the output image file (will be created or truncated).");
        eprintln!("  size_in_blocks   Number of 4 KiB blocks (default: {} = 16 MiB)", DEFAULT_NUM_BLOCKS);
        process::exit(1);
    }

    let image_path = &args[1];
    let num_blocks: u64 = if args.len() >= 3 {
        args[2].parse().map_err(|_| "invalid block count")?
    } else {
        DEFAULT_NUM_BLOCKS
    };

    if num_blocks < 64 {
        eprintln!("error: filesystem must be at least 64 blocks");
        process::exit(1);
    }

    let total_size = num_blocks * (BLOCK_SIZE as u64);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(image_path)?;
    file.set_len(total_size)?;

    let storage = FileStorage::open(std::path::Path::new(image_path))?;
    jfsfuse::mkfs::build_filesystem(&storage, num_blocks);

    let blocks = storage.size_blocks();
    let free = types::FM_CLEAN;
    eprintln!(
        "newjfs: created JFS image '{}' ({} blocks, {} bytes, state={:#010x})",
        image_path, blocks, total_size, free
    );

    Ok(())
}
