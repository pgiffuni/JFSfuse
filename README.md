# jfsfuse

A Rust reimplementation of the Linux JFS (Journaled File System) exposed over FUSE.

This project re-implements the on-disk format, B+-tree managers, journal recovery,
and allocation maps of IBM's JFS filesystem entirely in Rust, providing read-only
access through the FUSE (Filesystem in Userspace) interface.

## Building

### Prerequisites

- Rust toolchain (edition 2024 or later)
- FUSE runtime (installed on the host)
- Kernel headers for FUSE development (optional, only if building the FUSE backend)

### Build

```sh
cargo build
```

### Run tests

```sh
cargo test
```

### Build the mount binary

```sh
cargo build --bin jfsfuse-mount
```

### Mount a JFS filesystem image

```sh
cargo build --bin jfsfuse-mount
sudo ./target/debug/jfsfuse-mount /path/to/jfs.img /mnt/jfs
```

## Structure

```
fusejfs/
├── Cargo.toml          # Package manifest (crate name: jfsfuse)
├── LICENSE-MIT         # MIT license text
├── LICENSE-GPL         # GNU GPL v2 (or later) license text
├── README.md           # This file
├── src/                # Rust source tree
│   ├── lib.rs          # Crate root: declares all modules
│   ├── types.rs        # On-disk structure definitions (pxd_t, dinode, xtroot, etc.)
│   ├── storage/        # Block I/O abstraction + page cache (replaces kernel metapage)
│   │   └── mod.rs
│   ├── journal/        # Journal subsystem: log I/O and crash recovery (logredo)
│   │   ├── mod.rs
│   │   ├── logmgr.rs   # Log manager (mirrors jfs_logmgr.c/h)
│   │   └── recovery.rs # Crash recovery / logredo
│   ├── alloc/          # Allocation maps: block allocation map and inode allocation map
│   │   ├── mod.rs
│   │   ├── dmap.rs     # Block allocation map (mirrors jfs_dmap.c/h)
│   │   └── imap.rs     # Inode allocation map (mirrors jfs_imap.c/h)
│   ├── btree/          # B+-tree managers: xtree and dtree
│   │   ├── mod.rs
│   │   ├── dtree.rs    # Directory B+-tree (mirrors jfs_dtree.c/h)
│   │   └── xtree.rs    # Extent B+-tree (mirrors jfs_xtree.c/h)
│   ├── inode.rs        # Runtime inode access (mirrors jfs_incore.h / jfs_inode.c)
│   ├── volume.rs       # Volume mount: superblock parsing, journal recovery
│   ├── fuse/           # FUSE filesystem operations adapter
│   │   └── mod.rs
│   └── bin/
│       └── mount.rs    # CLI binary entry point (jfsfuse-mount)
├── src-mining/         # Reference copies of original Linux JFS kernel source
│                       # (GPL-2.0-or-later) used for provenance and algorithm tracing
└── target/             # Cargo build output (git-ignored)
```

### Module Overview

| Module | Responsibility |
|--------|---------------|
| `types` | On-disk type definitions for all JFS structures (pxd_t, dxd_t, dinode, xad_t, xtroot, dtroot, logsuper, logpage, lrd, jfs_superblock, iag, dmap). All constants and field accessors mirror the corresponding C headers. |
| `storage` | Block-level I/O (`Storage` trait, `FileStorage`, `NullStorage`), a metadata page cache (`PageCache` replacing the kernel's `metapage`/`address_space`), and a raw buffer pool for journal operations. |
| `journal` | Journal/log management and crash recovery. `LogManager` handles log page I/O and logsuper read/write; `JournalRecovery` implements backward-replay logredo to restore filesystem consistency after an unclean shutdown. |
| `alloc` | Block allocation map (`dmap`) and inode allocation map (`imap`) — the buddy allocator and IAG structures for managing free blocks and inode numbers. |
| `btree` | B+-tree managers: `xtree` (extent descriptor tree mapping file offsets to disk blocks) and `dtree` (directory entry tree mapping filenames to inode numbers). |
| `inode` | Runtime inode representation bridging on-disk `dinode` structures to filesystem operations. |
| `volume` | Volume mount logic: superblock validation, inline log detection, journal recovery orchestration, and allocation map initialization. |
| `fuse` | FUSE adapter: bridges JFS filesystem operations to the FUSE kernel interface. |

## Licensing

This project contains code with two different licenses:

- **Files directly derived from the Linux JFS kernel source** (`src-mining/` and the
  Rust modules that mirror JFS on-disk structures and algorithms — `types.rs`,
  `journal/`, `alloc/`, `btree/`, `inode.rs`, `volume.rs`) are licensed under
  **GPL-2.0-or-later**, matching the original Linux kernel JFS code.

- **Original Rust code** — the storage abstraction layer, FUSE adapter, binary entry
  point, and crate root (`lib.rs`, `storage/mod.rs`, `fuse/mod.rs`, `bin/mount.rs`) —
  are licensed under **MIT**.

Each source file carries an `SPDX-License-Identifier` header identifying its license.

The package as a whole is distributed under `MIT OR GPL-2.0-or-later`; see
`LICENSE-MIT` and `LICENSE-GPL` for the full license text.

---

## Write Support

The read-only implementation is the foundation. Write support adds a transaction
layer beneath FUSE so that all mutation logic lives in the filesystem core, not
in the FUSE adapter.

### Recommended implementation order

#### Phase 0 — Establish a writable baseline

Before changing behavior:

* Create a `writable` feature or configuration path.

* Keep read-only mounting available.

* Refuse writable mounts unless the volume and journal pass validation.

* Add an explicit `--read-only` option even if read-only remains the default.

* Add a mount-time dirty-state check.

* Ensure all write operations are disabled when the volume is mounted read-only.

* Add a test image containing:

  * regular files;

  * empty and nonempty directories;

  * long names;

  * hard links;

  * symbolic links;

  * sparse files;

  * extended attributes;

  * a populated journal.

Do not initially permit concurrent writers. Start with a single filesystem-wide
write lock and later refine locking.

#### Phase 1 — Introduce a transaction abstraction

Create a core transaction API, independent of FUSE. Conceptually it should provide:

* begin transaction;

* acquire metadata pages;

* mark metadata pages dirty;

* allocate blocks;

* free blocks;

* update inode;

* update allocation maps;

* append log records;

* commit;

* abort;

* recover incomplete transactions.

The transaction object should own the write set and prevent callers from
modifying metadata without journal participation.

A useful internal separation is:

```
Filesystem
 ├── Volume
 ├── Storage
 ├── MetadataPageCache
 ├── TransactionManager
 ├── Journal
 ├── AllocationManager
 ├── InodeManager
 ├── DirectoryManager
 └── BTree managers
```

The critical rule is:

> No code outside the transaction and metadata layers may write directly to the
> storage backend.

The current `Storage` abstraction and metadata-page cache should be retained,
but write support must add dirty-page tracking, ordering, rollback handling, and
durable flush operations.

#### Phase 2 — Make the journal writable

The journal is the central dependency for safe mutation.

Implement, in this order:

1. Log-page allocation.
2. Log-record serialization.
3. Transaction identifiers.
4. Begin and commit records.
5. Update records.
6. Redo information for metadata blocks.
7. Transaction end records.
8. Log wrapping.
9. Checkpointing.
10. Journal flush and durability barriers.
11. Recovery of committed but uncheckpointed transactions.
12. Discarding or rolling back incomplete transactions.

Do not begin with a completely new logging design. First reproduce the Linux JFS
log record layout and ordering rules faithfully. The existing backward-replay
recovery implementation should be extended rather than replaced.

The first writable journal milestone should support only:

* metadata updates;
* allocation-map updates;
* inode updates;
* directory-tree updates.

File-data journaling can initially be avoided if the implementation uses an
ordered write policy:

1. allocate and initialize data blocks;
2. flush data blocks;
3. journal metadata describing those blocks;
4. commit metadata transaction;
5. expose the new file state.

This is simpler than journaling every file-data block, but it must be documented
as an explicit consistency model.

#### Phase 3 — Writable block and metadata cache

Extend the cache with:

* shared read access;
* exclusive write access;
* dirty state;
* pin count;
* transaction ownership;
* generation or sequence number;
* flush state;
* checksum or diagnostic metadata where appropriate;
* invalidation after abort;
* writeback ordering.

Use a filesystem-wide write lock initially. Avoid premature fine-grained locking
until the mutation paths are correct.

Required invariants:

* A dirty page belongs to one active transaction.
* A page cannot be evicted while pinned.
* A transaction cannot commit before all required log records are durable.
* Metadata must not reach stable storage before the corresponding log record.
* Aborted transactions must not leave dirty pages visible.
* The cache must not return stale metadata after recovery.

#### Phase 4 — Allocation-map writes

The allocation maps must be writable before file creation can be correct.

##### Dmap

Implement:

* block allocation;
* block freeing;
* buddy-tree updates;
* summary-bit updates;
* free-space accounting;
* allocation from a preferred range;
* rollback of allocations;
* consistency validation.

Test:

* single-block allocation;
* contiguous allocation;
* allocation spanning dmap boundaries;
* freeing and reallocating;
* exhaustion;
* rollback after journal failure.

##### Imap

Implement:

* inode allocation;
* inode freeing;
* IAG updates;
* inode allocation-group summaries;
* inode extent updates;
* inode reuse protection;
* rollback of inode allocation.

Do not reuse an inode immediately after unlink unless the transaction and
journal semantics guarantee that stale directory entries and cached inode
references cannot observe the reused inode.

#### Phase 5 — Writable xtree extent management

Regular-file writes require extent-tree mutation.

Implement separately:

1. Insert a new extent.
2. Extend the last extent.
3. Split an extent.
4. Merge adjacent compatible extents.
5. Remove an extent.
6. Split around a hole.
7. Support sparse-file holes.
8. Update extent-tree root and internal nodes.
9. Allocate and free xtree nodes.
10. Handle tree growth and shrinkage.
11. Validate ordering and non-overlap.

Every operation must maintain:

* logical extent ordering;
* physical extent validity;
* non-overlapping logical ranges;
* correct subtree limits;
* correct root metadata;
* correct inode size;
* correct block count;
* allocation-map agreement.

Start with writes that append to the end of an existing file. Then add
overwrites within existing extents, followed by writes into holes and finally
truncation.

#### Phase 6 — Regular-file write path

Implement the core API before connecting FUSE:

```
read_at(inode, offset, length)
write_at(inode, offset, data)
allocate_range(inode, offset, length)
punch_hole(inode, offset, length)
truncate(inode, new_size)
fsync(inode)
```

The write algorithm should be:

1. Validate inode type and permissions.
2. Validate offset and length.
3. Begin transaction.
4. Resolve the logical block range.
5. Allocate missing physical blocks.
6. Read-modify-write partial blocks.
7. Write full data blocks.
8. Update xtree extents.
9. Update inode size and block count.
10. Update timestamps.
11. Flush data blocks according to the chosen ordering policy.
12. Log metadata changes.
13. Commit.
14. Return the number of bytes written.

Do not update `mtime` or `ctime` until the transaction is going to commit, or
ensure that those updates are rolled back correctly.

#### Phase 7 — Truncate and hole punching

Implement truncation as its own subsystem because it combines extent-tree and
allocation-map changes.

For shrinking:

1. Identify extents beyond the new EOF.
2. Free complete trailing extents.
3. Split the final extent if necessary.
4. Free the physical tail.
5. Update the xtree.
6. Update inode size and block count.
7. Commit as one transaction.

For growing:

* do not allocate blocks merely because the file size grows;
* represent the new range as a hole;
* update inode size only.

For `fallocate`-style allocation, use a separate API. Do not overload ordinary
file growth semantics.

#### Phase 8 — Directory mutation

The dtree must support mutation before FUSE can expose normal filesystem
operations.

Implement:

* insert directory entry;
* remove directory entry;
* lookup after mutation;
* rename within one directory;
* rename across directories;
* directory growth;
* directory shrinkage;
* empty-directory validation;
* `.` and `..` handling;
* directory link-count updates;
* name collision detection;
* name encoding and validation.

Directory insertion should be transactional with inode allocation when
implementing `create` or `mkdir`.

For a new regular file:

1. Allocate inode.
2. Initialize inode.
3. Insert directory entry.
4. Update parent directory metadata.
5. Update parent timestamps.
6. Commit.

For `mkdir`:

1. Allocate inode.
2. Initialize directory inode.
3. Create `.` and `..`.
4. Insert child in parent.
5. Increment parent link count.
6. Commit.

For `rmdir`:

1. Verify the target is a directory.
2. Verify it contains no entries other than `.` and `..`.
3. Remove the parent entry.
4. Remove `.` and `..`.
5. Decrement parent link count.
6. Free the directory inode.
7. Commit.

#### Phase 9 — FUSE mutation operations

Only after the core operations work independently should the FUSE adapter
implement:

* `create`, `mkdir`, `unlink`, `rmdir`, `rename`, `link`, `symlink`,
* `open`, `release`, `write`, `truncate`, `fsync`, `flush`, `setattr`,
* `chmod`, `chown`, `utimens`, `setxattr`, `getxattr`, `listxattr`,
  `removexattr`.

Map internal errors carefully:

| Internal condition | FUSE result |
| --- | --- |
| Missing inode or name | `ENOENT` |
| Existing name | `EEXIST` |
| Wrong inode type | `ENOTDIR` or `EISDIR` |
| Nonempty directory | `ENOTEMPTY` |
| No free blocks/inodes | `ENOSPC` |
| Read-only volume | `EROFS` |
| Invalid name | `EINVAL` |
| Permission failure | `EACCES` |
| Busy or open target | `EBUSY` where applicable |
| Unsupported feature | `EOPNOTSUPP` |

Do not let FUSE callbacks manipulate raw on-disk structures directly.

#### Phase 10 — Links and open-unlinked semantics

Implement hard links only after unlink and inode reference counting are correct.

Required behavior:

* increment inode link count when adding a hard link;
* decrement it on unlink;
* retain inode data while link count is zero but open handles remain;
* free blocks only after the last open reference closes;
* prevent hard links to directories unless explicitly supported;
* update parent directory timestamps.

The runtime inode object will need a distinction between:

* directory link count;
* open-handle count;
* on-disk inode lifetime;
* pending deletion state.

#### Phase 11 — Symbolic links

Implement:

* short symlinks stored inline if JFS supports that representation;
* long symlinks using allocated data blocks;
* symlink read;
* symlink creation;
* truncation prohibition;
* correct inode mode and size;
* transactionally allocated symlink data.

Keep symlink resolution policy in the VFS/FUSE layer, while storage and inode
code only expose the symlink payload.

#### Phase 12 — Metadata and extended attributes

Implement ordinary metadata changes first:

* mode; owner; group; size; atime; mtime; ctime; link count; flags.

Then implement xattrs using the existing JFS xattr representation, preserving
Linux JFS semantics and provenance. Do not invent a new xattr format.

Required xattr operations:

* set; replace; create-only; remove; list; size-query; namespace
  validation; maximum size validation; transaction rollback.

ACLs should be treated as a later layer over xattrs unless the current JFS
implementation already has complete ACL support.

#### Phase 13 — Mount safety and recovery

Writable mounting must perform stronger validation than read-only mounting:

1. Read and validate the superblock.
2. Identify the journal.
3. Check journal state.
4. Replay committed transactions.
5. Detect incomplete transactions.
6. Rebuild or validate allocation summaries.
7. Validate root inode.
8. Validate root directory.
9. Refuse mounting if recovery cannot establish a consistent state.
10. Mark the filesystem dirty before allowing writes.
11. Mark it clean only after a clean unmount and journal checkpoint.

A crash test must cover:

* crash before data flush;
* crash after data flush but before metadata commit;
* crash after log commit;
* crash during allocation-map update;
* crash during directory insertion;
* crash during rename;
* crash during unlink;
* crash during truncate;
* crash during journal wraparound.

#### Phase 14 — Consistency checker

Add a standalone checker before declaring write support complete. It should
verify:

* every allocated block is referenced appropriately;
* no block is allocated twice;
* free-space maps agree with extent trees;
* inode allocation maps agree with inode reachability;
* directory entries point to valid inodes;
* link counts match directory references;
* `.` and `..` are correct;
* directory names are unique;
* extent trees are ordered and non-overlapping;
* inode sizes agree with extent mappings;
* journal state is valid;
* orphaned inodes are handled;
* xattr storage is valid.

The checker should support:

```
jfsck --read-only image
jfsck --repair image
jfsck --replay image
jfsck --dump-inode image ino
jfsck --dump-xtree image ino
jfsck --dump-dtree image ino
jfsck --dump-journal image
```

Do not enable automatic repair until the checker can produce a complete
diagnostic report.

### Suggested milestones

**Milestone A — Writable metadata prototype:** journal transactions;
allocation-map updates; inode updates; `fsync`; no directory mutation yet.

**Milestone B — Create and write:** `create`, `open`, `write`,
read-after-write, `truncate`, `fsync`, `unlink`. Single writer lock.

**Milestone C — Directory operations:** `mkdir`, `rmdir`, `rename`, hard
links, symbolic links.

**Milestone D — Full metadata:** `chmod`, `chown`, timestamps, xattrs, ACLs
if feasible.

**Milestone E — Crash safety and concurrency:** journal replay; clean/dirty
mount state; crash testing; concurrent readers; serialized or carefully
locked writers; consistency checker.

### Testing strategy

**Differential tests:** Create the same filesystem image with Linux JFS tools,
then compare directory listings, inode metadata, file contents, extents,
allocation counts, xattrs, rename and unlink behavior.

**Property tests:** Generate sequences of create, write, truncate, mkdir,
rename, link, unlink, rmdir. After every sequence, verify the consistency
checker.

**Crash tests:** Run each mutation under an injected crash point
(`after_allocate`, `after_data_write`, `after_log_record`, `after_log_flush`,
`after_metadata_write`, `after_commit`, `before_checkpoint`, `after_checkpoint`).
Remount the image and verify that the result is either the complete committed
operation or the previous consistent state. Never accept an image that is merely
mountable if its allocation maps, link counts, or directory trees are
inconsistent.
