// SPDX-License-Identifier: MIT
//! jfsck — JFS consistency checker.
//!
//! Usage:
//!   jfsck --read-only <image>
//!   jfsck --repair <image>
//!   jfsck --replay <image>
//!   jfsck --dump-inode <image> <ino>
//!   jfsck --dump-xtree <image> <ino>
//!   jfsck --dump-dtree <image> <ino>
//!   jfsck --dump-journal <image>
//!
//! The `--repair` flag is reserved and not yet enabled — it will exit
//! with an error message until the checker can produce a complete
//! diagnostic report.

use std::env;
use std::process;

use jfsfuse::storage::{BLOCK_SIZE, FileStorage, Storage};
use jfsfuse::volume::Volume;

const USAGE: &str = "usage: jfsck --read-only <image>\n\
                     jfsck --repair <image>\n\
                     jfsck --replay <image>\n\
                     jfsck --dump-inode <image> <ino>\n\
                     jfsck --dump-xtree <image> <ino>\n\
                     jfsck --dump-dtree <image> <ino>\n\
                     jfsck --dump-journal <image>";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("{}", USAGE);
        process::exit(1);
    }

    let mode = &args[1];
    let image = &args[2];

    match mode.as_str() {
        "--read-only" => cmd_read_only(image),
        "--repair" => cmd_repair(image),
        "--replay" => cmd_replay(image),
        "--dump-inode" => {
            if args.len() < 4 {
                eprintln!("usage: jfsck --dump-inode <image> <ino>");
                process::exit(1);
            }
            let ino: u32 = args[3].parse().map_err(|_| "invalid inode number")?;
            cmd_dump_inode(image, ino)
        }
        "--dump-xtree" => {
            if args.len() < 4 {
                eprintln!("usage: jfsck --dump-xtree <image> <ino>");
                process::exit(1);
            }
            let ino: u32 = args[3].parse().map_err(|_| "invalid inode number")?;
            cmd_dump_xtree(image, ino)
        }
        "--dump-dtree" => {
            if args.len() < 4 {
                eprintln!("usage: jfsck --dump-dtree <image> <ino>");
                process::exit(1);
            }
            let ino: u32 = args[3].parse().map_err(|_| "invalid inode number")?;
            cmd_dump_dtree(image, ino)
        }
        "--dump-journal" => cmd_dump_journal(image),
        _ => {
            eprintln!("error: unknown mode '{}'\n{}", mode, USAGE);
            process::exit(1);
        }
    }
}

fn cmd_read_only(image: &str) -> Result<(), Box<dyn std::error::Error>> {
    let storage = FileStorage::open_readonly(std::path::Path::new(image))?;
    let vol = Volume::open_from_storage(std::sync::Arc::new(storage))?;

    let mut vol = vol;
    let report = vol.check_consistent()?;

    let mut errors = 0;
    let mut warnings = 0;
    for issue in &report.issues {
        let prefix = match issue.level {
            2 => {
                errors += 1;
                "ERROR"
            }
            1 => {
                warnings += 1;
                "WARNING"
            }
            _ => {
                "INFO"
            }
        };
        let location = match (&issue.ino, &issue.block) {
            (Some(ino), _) => format!(" inode {}", ino),
            (_, Some(block)) => format!(" block {}", block),
            _ => String::new(),
        };
        eprintln!("{}:{} {}", prefix, location, issue.message);
    }

    if errors > 0 {
        eprintln!("\n{} errors, {} warnings", errors, warnings);
        process::exit(1);
    } else if warnings > 0 {
        eprintln!("\n{} warnings", warnings);
    } else {
        println!("Filesystem is clean.");
    }

    Ok(())
}

#[cfg(feature = "writable")]
fn cmd_repair(image: &str) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("error: --repair is not yet supported. Run with --read-only for a diagnostic report.");
    process::exit(1);
}

#[cfg(not(feature = "writable"))]
fn cmd_repair(_image: &str) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("error: --repair requires the 'writable' feature.");
    process::exit(1);
}

#[cfg(feature = "writable")]
fn cmd_replay(image: &str) -> Result<(), Box<dyn std::error::Error>> {
    let storage = std::sync::Arc::new(FileStorage::open(std::path::Path::new(image))?);
    let _vol = Volume::open_from_storage(storage)?;
    println!("Journal replayed successfully.");
    Ok(())
}

#[cfg(not(feature = "writable"))]
fn cmd_replay(_image: &str) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("error: --replay requires the 'writable' feature.");
    process::exit(1);
}

fn cmd_dump_inode(image: &str, ino: u32) -> Result<(), Box<dyn std::error::Error>> {
    let storage = std::sync::Arc::new(FileStorage::open_readonly(std::path::Path::new(image))?);
    let mut vol = Volume::open_from_storage(storage)?;

    let inode = jfsfuse::inode::Inode::read(&mut vol, ino)?;
    println!("Inode {}:", ino);
    println!("  mode:   {:#010x}", inode.mode());
    println!("  size:   {}", inode.size());
    println!("  nlink:  {}", inode.dinode.nlink());
    println!("  uid:    {}", inode.dinode.uid());
    println!("  gid:    {}", inode.dinode.gid());
    println!("  nblock: {}", inode.dinode.nblocks());
    println!("  is_dir: {}", inode.is_dir());
    println!("  is_reg: {}", inode.is_regular());
    println!("  is_lnk: {}", inode.is_symlink());

    if inode.is_regular() || inode.is_dir() {
        let xt = jfsfuse::btree::xtree::Xtree::from_inode_data(inode.xtroot_bytes())?;
        println!("  xtree next_index: {}", xt.next_index());
        for ext in xt.iter_extents() {
            println!("    extent: off={} len={} addr={}", ext.offset, ext.length, ext.address);
        }
    }

    if inode.is_dir() {
        let dt = jfsfuse::btree::dtree::Dtree::from_inode_data(inode.dtroot_bytes())?;
        println!("  dtree entries: {}", dt.len_entries());
    }

    if inode.is_symlink() {
        let target = inode.dinode.u[..inode.size() as usize].to_vec();
        println!("  symlink target: {}", String::from_utf8_lossy(&target));
    }

    Ok(())
}

fn cmd_dump_xtree(image: &str, ino: u32) -> Result<(), Box<dyn std::error::Error>> {
    let storage = std::sync::Arc::new(FileStorage::open_readonly(std::path::Path::new(image))?);
    let mut vol = Volume::open_from_storage(storage)?;

    let inode = jfsfuse::inode::Inode::read(&mut vol, ino)?;
    let xt = jfsfuse::btree::xtree::Xtree::from_inode_data(inode.xtroot_bytes())?;

    println!("Xtree for inode {}:", ino);
    println!("  next_index: {}", xt.next_index());

    match xt.validate() {
        Ok(()) => println!("  validation: OK"),
        Err(e) => println!("  validation: ERROR: {}", e),
    }

    for ext in xt.iter_extents() {
        println!(
            "  extent: logical_off={} length={} physical_addr={:#010x}",
            ext.offset, ext.length, ext.address
        );
    }

    Ok(())
}

fn cmd_dump_dtree(image: &str, ino: u32) -> Result<(), Box<dyn std::error::Error>> {
    let storage = std::sync::Arc::new(FileStorage::open_readonly(std::path::Path::new(image))?);
    let mut vol = Volume::open_from_storage(storage)?;

    let inode = jfsfuse::inode::Inode::read(&mut vol, ino)?;
    let dt = jfsfuse::btree::dtree::Dtree::from_inode_data(inode.dtroot_bytes())?;

    println!("Dtree for inode {}:", ino);
    println!("  entries: {}", dt.len_entries());

    for entry in dt.entries()? {
        let name = String::from_utf16_lossy(&entry.name).trim_end_matches('\0').to_string();
        println!("    name={} ino={}", name, entry.inumber);
    }

    Ok(())
}

#[cfg(feature = "writable")]
fn cmd_dump_journal(image: &str) -> Result<(), Box<dyn std::error::Error>> {
    let storage = std::sync::Arc::new(FileStorage::open_readonly(std::path::Path::new(image))?);
    let sb = Volume::read_super(&*storage)?;

    if !sb.has_inline_log() {
        println!("No inline log found.");
        return Ok(());
    }

    let log_base = sb.inline_log_pxd().address() * (BLOCK_SIZE as u64);
    let ls = jfsfuse::journal::LogManager::read_super(&*storage, log_base)?;

    println!("Log superblock:");
    println!("  magic:   0x{:08x}", ls.magic_val());
    println!("  version: {}", ls.version());
    println!("  state:   {}", ls.state());
    println!("  end:     {}", ls.end());
    println!("  pages:   {}", ls.page_size());
    println!("  bsize:   {}", ls.block_size());

    let state_name = match ls.state() {
        jfsfuse::types::LOGMOUNT => "LOGMOUNT",
        jfsfuse::types::LOGREDONE => "LOGREDONE (clean)",
        jfsfuse::types::LOGWRAP => "LOGWRAP (needs recovery)",
        jfsfuse::types::LOGREADERR => "LOGREADERR",
        _ => "unknown",
    };
    println!("  state_name: {}", state_name);

    Ok(())
}

#[cfg(not(feature = "writable"))]
fn cmd_dump_journal(image: &str) -> Result<(), Box<dyn std::error::Error>> {
    let storage = std::sync::Arc::new(FileStorage::open_readonly(std::path::Path::new(image))?);
    let sb = Volume::read_super(&*storage)?;

    if !sb.has_inline_log() {
        println!("No inline log found.");
        return Ok(());
    }

    let log_base = sb.inline_log_pxd().address() * (BLOCK_SIZE as u64);
    let ls = jfsfuse::journal::LogManager::read_super(&*storage, log_base)?;

    println!("Log superblock:");
    println!("  magic:   0x{:08x}", ls.magic_val());
    println!("  version: {}", ls.version());
    println!("  state:   {}", ls.state());
    println!("  end:     {}", ls.end());
    println!("  pages:   {}", ls.page_size());
    println!("  bsize:   {}", ls.block_size());

    let state_name = match ls.state() {
        jfsfuse::types::LOGMOUNT => "LOGMOUNT",
        jfsfuse::types::LOGREDONE => "LOGREDONE (clean)",
        jfsfuse::types::LOGWRAP => "LOGWRAP (needs recovery)",
        jfsfuse::types::LOGREADERR => "LOGREADERR",
        _ => "unknown",
    };
    println!("  state_name: {}", state_name);

    Ok(())
}
