//! [`OverFile`]: a file standing in for a block device (D182), through bstd's file calls and so
//! the file effects every driver completes (`Effect::Read`, `Effect::WriteAt`, `Effect::Sync`,
//! `Effect::Truncate`) on its one name.
//!
//! # Its facts, and what a file system can say
//!
//! - **logical: 512.** The grain blaze-core's disk world tears every unsynced write by
//!   (`blaze_core::world::disk::SECTOR`, held by the simulation and every drive blaze targets):
//!   so a crash never tears less than a block. A file system's own block (4,096 on APFS, ext4
//!   and xfs by default) is not the unit of a tear: its write-out of a page is itself torn by
//!   sectors on a drive with no atomic 4,096-byte write.
//! - **physical: 4,096.** The page, what a write of less costs a read of on every file system
//!   blaze targets. Not asked of the file system today: bstd's `Meta` carries no block size
//!   (`st_blksize`, statx `stx_blksize`); that field is the D182 edit that would make it asked.
//! - **atomic_write: one block.** Neither file system proves more for a buffered write. APFS
//!   says nothing of atomic writes at all (no `F_*` query; its copy-on-write metadata makes a
//!   whole `pwrite` atomic against a crash only for its metadata, not its data blocks). ext4
//!   and xfs report `STATX_WRITE_ATOMIC` from Linux 6.13 for a file on a device with atomic
//!   writes, but only for `O_DIRECT` writes with `RWF_ATOMIC`; a buffered write is torn by
//!   sectors at write-back. So one 512-byte block: the drive's sector, which NVMe promises
//!   whole on power loss (AWUPF's 0, one block) and SATA drives keep in practice.
//! - **queue_depth: 1.** One call at a time through bstd's file calls; the drivers' rings
//!   are below it and not the file's to say.
//! - **volatile_cache: true.** A written page is in memory until a flush (`F_FULLFSYNC` on
//!   Apple's systems, `fsync` on Linux): the whole page cache is a volatile cache.
//!
//! # Preallocation
//!
//! The file is made to its size by `Effect::Truncate` and synced, so its length (and on APFS
//! and ext4 its extent map, sparse) is durable before any block is written. That is not
//! preallocation: a first write into a hole allocates, which is a metadata change the next
//! flush also waits for. Preallocation is `fallocate(fd, 0, 0, len)` on Linux and
//! `fcntl(F_PREALLOCATE, F_ALLOCATEALL)` then `ftruncate` on macOS (blaze-sys
//! `linux::fallocate`, `macos_block::preallocate`, written), which need an `Effect::Allocate`
//! bstd does not have (the D182 list): until then a correct, slower first write.

use blaze_core::FileName;

use super::{BlockError, BlockKind, BlockOp, BlockSize, Blocks, Device, DeviceFacts, DeviceFactsOpts, Durable, Lba};
use crate::{
    cx::Cx,
    fs::{Dir, File, ReadWrite, Unsynced},
    path::Path,
};

/// A file standing in for a block device. Linear, as its file is: finished with
/// [`OverFile::close`] or [`OverFile::close_unsynced`].
#[blaze::linear]
#[must_use = "a block device over a file is finished with close(cx) or close_unsynced()"]
pub struct OverFile {
    file: File<ReadWrite>,
    facts: DeviceFacts,
}

/// The logical block of every [`OverFile`] (its module's docs).
pub const LOGICAL: BlockSize = BlockSize::SECTOR;
/// The physical block of every [`OverFile`] (its module's docs).
pub const PHYSICAL: BlockSize = BlockSize::PAGE;

fn facts(size: Blocks) -> Result<DeviceFacts, BlockKind> {
    DeviceFacts::new(DeviceFactsOpts {
        logical: LOGICAL,
        physical: PHYSICAL,
        atomic_write: Blocks(1),
        queue_depth: core::num::NonZeroU32::MIN,
        volatile_cache: true,
        size,
    })
    .map_err(BlockKind::Facts)
}

impl OverFile {
    /// A new device of `size` blocks at `path` beneath `dir`, all zeros: the file made, cut to
    /// its length and synced, and its name synced, so it survives a crash whole before this
    /// returns. A file there is cut to nothing first.
    pub fn create(cx: &Cx, dir: &Dir<ReadWrite>, path: &Path, size: Blocks) -> Result<OverFile, BlockError> {
        let file = dir.create(cx, path).map_err(|error| BlockError::of_fs(BlockOp::Open, error))?;
        let made = facts(size).and_then(|facts| {
            file.set_len_synced(cx, facts.bytes()).map_err(BlockKind::Fs)?;
            dir.sync_names(cx).map_err(BlockKind::Fs)?;
            Ok(facts)
        });
        OverFile::made(file, made)
    }

    /// The device at `path` beneath `dir`, as long as it is: a whole number of blocks.
    pub fn open(cx: &Cx, dir: &Dir<ReadWrite>, path: &Path) -> Result<OverFile, BlockError> {
        let file = dir.open(cx, path).map_err(|error| BlockError::of_fs(BlockOp::Open, error))?;
        let made = file.len(cx).map_err(BlockKind::Fs).and_then(|len| match len % LOGICAL.bytes() {
            0 => facts(Blocks(len / LOGICAL.bytes())),
            // A length that is not whole blocks is no device of this block size.
            _ => Err(BlockKind::Short { moved: len }),
        });
        OverFile::made(file, made)
    }

    /// `file` as a device of `made`, or finished unsynced and the error naming it.
    fn made(file: File<ReadWrite>, made: Result<DeviceFacts, BlockKind>) -> Result<OverFile, BlockError> {
        match made {
            Ok(facts) => Ok(OverFile { file, facts }),
            Err(kind) => {
                // clone: the error's name, a refcount.
                let device = blaze_core::DirName::Named(file.name().clone());
                // Nothing of a device that failed its making is worth a flush.
                let _written = file.close_unsynced(Unsynced::No);
                Err(BlockError { device, range: None, op: BlockOp::Open, kind })
            }
        }
    }

    /// The device finished, every write flushed.
    #[blaze::consumes]
    pub fn close(self, cx: &Cx) -> Result<(), BlockKind> {
        let OverFile { file, facts: _ } = self;
        file.close_synced(cx).map(|_| ()).map_err(BlockKind::Fs)
    }

    /// The device finished with nothing flushed: what a crash keeps of its writes since the
    /// last flush is the device's.
    #[blaze::consumes]
    pub fn close_unsynced(self) {
        let OverFile { file, facts: _ } = self;
        let _written = file.close_unsynced(Unsynced::No);
    }
}

impl Device for OverFile {
    fn facts(&self) -> DeviceFacts {
        self.facts
    }

    fn name(&self) -> &FileName {
        self.file.name()
    }

    fn read(&mut self, cx: &Cx, at: Lba, into: &mut [u8]) -> Result<(), BlockKind> {
        // Checked by `Block::within`: the range is on the device, whose bytes fit a `u64`.
        let offset = at.0 * LOGICAL.bytes();
        let asked = into.len();
        match self.file.read_into(into, offset).wait(cx) {
            Ok(n) if n == asked => Ok(()),
            Ok(n) => Err(BlockKind::Short { moved: n as u64 }),
            Err(error) => Err(BlockKind::Fs(error)),
        }
    }

    fn write(&mut self, cx: &Cx, at: Lba, bytes: &[u8], durable: Durable, _atomic: bool) -> Result<(), BlockKind> {
        // An atomic write here is one block (the facts), which every write of a whole block
        // is: nothing more to ask.
        let offset = at.0 * LOGICAL.bytes();
        self.file.write_at(cx, offset, bytes, durable).map_err(BlockKind::Fs)
    }

    fn flush(&mut self, cx: &Cx) -> Result<(), BlockKind> {
        self.file.flush(cx).map_err(BlockKind::Fs)
    }

    fn discard(&mut self, _cx: &Cx, _at: Lba, _len: Blocks) -> Result<(), BlockKind> {
        // Never zeros written in its place (rule no-fallback): a hole punched is the
        // operation, and bstd has no effect for it.
        Err(BlockKind::Unbuilt {
            needs: "Effect::Discard: fallocate(PUNCH_HOLE | KEEP_SIZE) on Linux, fcntl(F_PUNCHHOLE) on macOS",
        })
    }
}
