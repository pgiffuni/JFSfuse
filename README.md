# JFS for fuse

A Rust reimplementation of IBM's JFS (Journaled File System) exposed over FUSE.

This project re-implements the on-disk format, B+-tree managers, journal recovery,
and allocation maps of IBM's [JFS filesystem](https://jfs.sourceforge.net/) entirely
in Rust, providing **read-write** access through the FUSE (Filesystem in Userspace)
interface with full journaling support.

## Building

### Prerequisites

- Rust toolchain (edition 2024 or later)
- FUSE runtime (installed on the host)
- Kernel headers for FUSE development (optional, only if building the FUSE backend)

### Build

```sh
cargo build
cargo build --features writable          # enable writable support
```

### Run tests

```sh
cargo test --features writable
```

### Binaries

- `jfsfuse-mount` — FUSE filesystem mount utility
- `jfsck` — JFS consistency checker and diagnostic tool
- `newjfs` — JFS filesystem image creator

```sh
cargo build --bin jfsck --features writable
cargo build --bin newjfs --features writable
```

## Files

```
fusejfs/
├── Cargo.toml          # Package manifest (crate name: jfsfuse)
├── LICENSE-MIT         # MIT license text
├── LICENSE-GPL         # GNU GPL v2 (or later) license text
├── README.md           # This file
├── src/                # Rust source tree
│   ├── lib.rs          # Crate root: declares all modules
│   ├── types.rs        # On-disk type definitions (pxd_t, dinode, xtroot, etc.)
│   ├── storage/        # Block I/O abstraction + page cache (replaces kernel metapage)
│   │   └── mod.rs
│   ├── journal/        # Journal subsystem: log I/O and crash recovery (logredo)
│   │   ├── mod.rs
│   │   ├── logmgr.rs   # Log manager (mirrors kernel JFS logmgr)
│   │   └── recovery.rs # Crash recovery / logredo
│   ├── alloc/          # Allocation maps: block allocation map and inode allocation map
│   │   ├── mod.rs
│   │   ├── dmap.rs     # Block allocation map (mirrors kernel JFS dmap)
│   │   └── imap.rs     # Inode allocation map (mirrors kernel JFS imap)
│   ├── btree/          # B+-tree managers: xtree and dtree
│   │   ├── mod.rs
│   │   ├── dtree.rs    # Directory B+-tree (mirrors kernel JFS dtree)
│   │   └── xtree.rs    # Extent B+-tree (mirrors kernel JFS xtree)
│   ├── inode.rs        # Runtime inode access (mirrors kernel JFS inode code)
│   ├── volume.rs       # Volume mount: superblock parsing, journal recovery
│   ├── fuse/           # FUSE filesystem operations adapter
│   │   └── mod.rs
│   └── bin/
│       ├── mount.rs    # CLI binary entry point (jfsfuse-mount)
│       ├── jfsck.rs    # Consistency checker CLI (jfsck)
│       └── newjfs.rs  # Filesystem image creator CLI (newjfs)
├── tests/              # Integration tests (organized by phase)
└── target/             # Cargo build output (git-ignored)
```

## Module Overview

| Module | Responsibility |
|--------|---------------|
| `types` | On-disk type definitions for all JFS structures (pxd_t, dxd_t, dinode, xad_t, xtroot, dtroot, logsuper, logpage, lrd, jfs_superblock, iag, dmap). All constants and field accessors mirror the corresponding kernel JFS headers. |
| `storage` | Block-level I/O (`Storage` trait, `FileStorage`, `NullStorage`, `MemoryStorage`), a metadata page cache (`PageCache` replacing the kernel's `metapage`/`address_space`), and a raw buffer pool for journal operations. Includes `FaultInjector` for crash-safety testing. |
| `journal` | Journal/log management and crash recovery. `LogManager` handles log page I/O and logsuper read/write; `JournalRecovery` implements backward-replay logredo to restore filesystem consistency after an unclean shutdown. |
| `alloc` | Block allocation map (`dmap`) and inode allocation map (`imap`) — the buddy allocator and IAG structures for managing free blocks and inode numbers. |
| `btree` | B+-tree managers: `xtree` (extent descriptor tree mapping file offsets to disk blocks) and `dtree` (directory entry tree mapping filenames to inode numbers). |
| `inode` | Runtime inode representation bridging on-disk `dinode` structures to filesystem operations. |
| `volume` | Volume mount logic: superblock validation, inline log detection, journal recovery orchestration, allocation map initialization, consistency checking, and dirty-bit management. |
| `fuse` | FUSE adapter: bridges JFS filesystem operations to the FUSE kernel interface. Read-only and writable modes with errno mapping. |
| `transaction` | Transaction manager for journaled write transactions (ordered-data model). |

## Features

### Read-only
Available without feature flags. Supports `lookup`, `readdir`, `read`, `getattr`.

### Writable (`--features writable`)
Full journaled write support with ordered-data semantics:

| Operation | Description |
|-----------|-------------|
| `lookup` / `readdir` | Name→inode resolution and directory listing (read-only) |
| `getattr` | Get inode attributes (read-only) |
| `open` / `release` | Open/close file handles (read-only: existence check; writable: open-handle tracking) |
| `read` | Read file data via xtree extent mapping |
| `create` / `mkdir` | Create files and directories with inline dtree initialization |
| `unlink` / `rmdir` | Remove files and directories with link-count management |
| `write` / `truncate` | Ordered-data writes with block allocation |
| `link` | Hard links (directories rejected) |
| `symlink` / `readlink` | Inline symbolic links (paths < 128 bytes) |
| `setattr` | chmod, chown, utimens |
| `setxattr` / `getxattr` / `listxattr` / `removexattr` | Extended attributes (inline TLV storage) |
| `rename` | Reserved (EOPNOTSUPP) |
| `lseek` (SEEK_DATA/SEEK_HOLE) | Find next data extent or hole from a given offset |
| `fallocate` | Pre-allocate space (mode 0) or punch holes (FALLOC_FL_PUNCH_HOLE \| FALLOC_FL_KEEP_SIZE) |

### Crash safety
- Ordered-data journaling: data blocks flushed before metadata journal commit
- Mount-time dirty-bit detection: `FM_DIRTY` set on writable mount, `FM_CLEAN` on clean umount
- Journal replay: `LOGWRAP` state triggers logredo recovery
- Open-unlinked semantics: inodes persist while open handles exist after unlink
- `FaultInjector` in storage layer for crash-injection testing

## Licensing

This project contains code with two different licenses:

- **Files mirroring the Linux JFS kernel algorithms and on-disk structures** (`types.rs`,
  `journal/`, `alloc/`, `btree/`, `inode.rs`, `volume.rs`, `transaction.rs`, and all
  test files) are licensed under **GPL-2.0-or-later**, matching the original Linux
  kernel JFS code.

- **Original Rust code** — the storage abstraction layer, FUSE adapter, binaries,
  and crate root (`lib.rs`, `storage/mod.rs`, `fuse/mod.rs`, `bin/mount.rs`,
  `bin/jfsck.rs`, `bin/newjfs.rs`) — are licensed under **MIT**.

Each source file carries an `SPDX-License-Identifier` header identifying its license.
The package as a whole is distributed under `GPL-2.0-or-later` and should IBM or
a future copyright holder relax the licensing, we can follow.

## jfsck — Consistency Checker

`jfsck` is a standalone tool for checking and repairing JFS filesystems,

### Commands

| Command | Description |
|---------|-------------|
| `jfsck --read-only <image>` | Check filesystem consistency without mounting for write. Reports errors/warnings found in superblock, journal, root inode, directory entries, extent trees, and xattr metadata. |
| `jfsck --repair <image>` | Run a consistency check, then apply safe repairs when the filesystem is structurally consistent. Currently clears a stale `FM_DIRTY` superblock state after journal replay. Requires `--features writable`. |
| `jfsck --replay <image>` | Mount the filesystem and replay the journal if needed. Use after an unclean shutdown. Requires `--features writable`. |
| `jfsck --dump-inode <image> <ino>` | Print detailed metadata for a single inode: mode, size, nlink, uid/gid, and xtree/dtree layout. |
| `jfsck --dump-xtree <image> <ino>` | Show the extent tree for a regular file or directory, including logical offset, length, and physical address for each extent. |
| `jfsck --dump-dtree <image> <ino>` | List all directory entries in a directory inode, showing name and inode number. |
| `jfsck --dump-journal <image>` | Show the log superblock: magic, version, state (LOGMOUNT/LOGREDONE/LOGWRAP), end pointer, and page geometry. |

### Example

```sh
# Create a filesystem image (optional: specify block count, default 4096 = 16 MiB)
newjfs /tmp/myfs.jfs
newjfs /tmp/myfs.jfs 8192       # 32 MiB

# Check filesystem
jfsck --read-only /tmp/myfs.jfs

# Dump a specific inode
jfsck --dump-inode /tmp/myfs.jfs 18

# Show journal state
jfsck --dump-journal /tmp/myfs.jfs
```

## Architecture

### Write path (ordered-data journaling)

```
write_at(inode, offset, data)
  1. Begin transaction
  2. Allocate blocks for sparse regions
  3. Write data blocks to physical locations
  4. Flush data blocks (flush_data)
  5. Update inode xtree + size (mark pages dirty)
  6. Append log records (journal metadata)
  7. Commit transaction: flush journal → flush metadata
```

### Open-unlinked semantics

When a file is unlinked while open, the inode persists in the inode table.
The link count is decremented but the inode is not freed until the last open
handle is released. This is tracked via `Volume::open_handles`, a map of
inode page blocks to handle counts.

### Journal recovery

On mount with `LOGWRAP` state, the `JournalRecovery` module replays the log
backward, applying `LOG_REDOPAGE` records to restore metadata pages, then
writes `LOGREDONE` to mark the journal as clean.
