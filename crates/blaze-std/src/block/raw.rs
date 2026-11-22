//! [`RawDevice`]: a Linux block device the grant names by its `/dev` path (D182), opened by the
//! grant in a world rooted at the path's folder, never discovered here: the grant's scope
//! holds the path, and nothing in bstd lists `/dev` or guesses a device.
//!
//! # Its facts
//!
//! [`RawFacts`] is what the device says, each a value or not said; [`RawFacts::facts`] takes
//! the conservative value for what is not said. The calls behind them are blaze-sys's
//! `linux::block` (written, D182), whose table names each call's kernel:
//!
//! - logical: `BLKSSZGET` (2.6); physical: `BLKPBSZGET` (2.6.32), else the logical block;
//!   size: `BLKGETSIZE64` (2.6).
//! - atomic write: `statx(STATX_WRITE_ATOMIC)`'s `stx_atomic_write_unit_max` (6.11), or sysfs
//!   `queue/atomic_write_unit_max_bytes` (6.11). Only an `O_DIRECT` write with `RWF_ATOMIC`
//!   is held to it, so a buffered device's facts say one block whatever the device says.
//! - queue depth: sysfs `queue/nr_requests`, else 1.
//! - volatile cache: sysfs `queue/write_cache` (4.7) `write through` says none; anything else,
//!   or nothing said, a volatile cache.
//!
//! bstd makes no system call: the facts reach it as an effect's answer (`Effect::BlockFacts`,
//! the D182 list), which blaze-core's drivers answer from those calls. Until it exists the
//! grant has no way to read them and [`RawDevice::open`] is handed them by its caller.
//!
//! # Writes
//!
//! Buffered by default ([`Cache::Buffered`]): through the page cache of the block device, as
//! a file's are; `Durable::Yes` is the write and then the device's flush (`Effect::Sync`,
//! `fsync` of a block device, which sends a cache flush). With `RWF_DSYNC` on the write
//! (`pwritev2`, 4.7) that is one call and, on a device with forced unit access, no flush of the
//! whole cache: the write effect does not carry the flag yet (the D182 list). [`Cache::Direct`]
//! (`O_DIRECT`, around the page cache) is the named option: bstd reaches it through
//! `Effect::ReadInto` and `Effect::WriteFrom` with `Cache::Bypassed` over registered spans,
//! which bstd does not expose yet, so it is refused by name today, never served buffered.
//!
//! # macOS
//!
//! The facts query only: blaze-sys `macos_block::disk_facts` (`DKIOCGETBLOCKSIZE`,
//! `DKIOCGETPHYSICALBLOCKSIZE`, `DKIOCGETBLOCKCOUNT`, `DKIOCGETFEATURES`) on `/dev/rdiskN`.
//! macOS says no atomic unit and no queue depth: one block, a queue of 1, a volatile cache.
//!
//! # NVMe passthrough (design, not built)
//!
//! Past the block layer, `IORING_OP_URING_CMD` on the NVMe generic character device
//! (`/dev/ng0n1`, Linux 5.19) hands a submission entry straight to the controller: a write,
//! read, flush or dataset-management (discard) command with blaze's own LBA, length and
//! forced-unit-access bit, completed on the ring with no bio, no request merge and no page
//! cache, about 1 µs a command less than `O_DIRECT` through the block layer (the io_uring
//! passthrough paper, FAST '24). Its facts come from Identify Namespace (`NVME_IOCTL_ADMIN_CMD`
//! opcode 0x06): LBA format (the logical block), NAWUPF/AWUPF (the atomic unit on power
//! loss, 0's based), and VWC from Identify Controller (the volatile cache). It would be a
//! fourth [`super::Device`], `Passthrough`, behind the same grant and the same conformance
//! suite; it needs an `Effect::NvmeCommand` carrying the 64-byte command and a registered span,
//! and a grant whose scope names the generic device. Not before the store on [`RawDevice`]
//! is measured on a real NVMe device: the saving is per command, and the store's commands are
//! few and large.

use core::num::NonZeroU32;

use blaze_core::FileName;

use super::{BlockError, BlockKind, BlockOp, BlockSize, Blocks, Device, DeviceFacts, DeviceFactsOpts, Durable, Lba};
use crate::{
    cx::Cx,
    fs::{File, ReadWrite, Unsynced},
};

/// Whether a device's reads and writes go through the page cache.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cache {
    /// Through it: the default.
    Buffered,
    /// Around it (`O_DIRECT`): buffers and offsets aligned to the logical block.
    Direct,
}

/// What a device says of itself; `None` where it says nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawFacts {
    /// `BLKSSZGET`.
    pub logical: u32,
    /// `BLKPBSZGET`.
    pub physical: Option<u32>,
    /// `BLKGETSIZE64`, in bytes.
    pub bytes: u64,
    /// `stx_atomic_write_unit_max` or sysfs `atomic_write_unit_max_bytes`, in bytes.
    pub atomic_max: Option<u32>,
    /// sysfs `nr_requests`.
    pub queue: Option<u32>,
    /// sysfs `write_cache`: `Some(false)` for `write through`.
    pub write_back: Option<bool>,
}

impl RawFacts {
    /// The device's facts for `cache`, each unsaid one the conservative value (the module's
    /// docs): never one promising more than the device said.
    pub fn facts(self, cache: Cache) -> Result<DeviceFacts, BlockKind> {
        let logical =
            BlockSize::new(self.logical).ok_or(BlockKind::Facts(super::FactsError::Logical { bytes: self.logical }))?;
        let physical = match self.physical.and_then(BlockSize::new) {
            Some(physical) if logical.divides(physical) => physical,
            _ => logical,
        };
        let atomic_write = match (cache, self.atomic_max) {
            // Only `O_DIRECT` with `RWF_ATOMIC` is held to the device's unit: the largest power
            // of two of whole blocks within it.
            (Cache::Direct, Some(max)) if max >= logical.get() => {
                let blocks = u64::from(max / logical.get());
                Blocks(1u64 << (63 - blocks.leading_zeros()))
            }
            _ => Blocks(1),
        };
        DeviceFacts::new(DeviceFactsOpts {
            logical,
            physical,
            atomic_write,
            queue_depth: self.queue.and_then(NonZeroU32::new).unwrap_or(NonZeroU32::MIN),
            volatile_cache: self.write_back != Some(false),
            size: Blocks(self.bytes / logical.bytes()),
        })
        .map_err(BlockKind::Facts)
    }
}

/// A raw block device. Linear, as its file is.
#[blaze::linear]
#[must_use = "a raw block device is finished with close(cx) or close_unsynced()"]
pub struct RawDevice {
    file: File<ReadWrite>,
    facts: DeviceFacts,
}

impl RawDevice {
    /// The device `file` (the grant's, opened from its `/dev` path) with the facts it said,
    /// its writes through `cache`. [`Cache::Direct`] is refused by name (the module's docs).
    pub fn open(file: File<ReadWrite>, said: RawFacts, cache: Cache) -> Result<RawDevice, BlockError> {
        let made = match cache {
            Cache::Buffered => said.facts(cache),
            Cache::Direct => Err(BlockKind::Unbuilt {
                needs: "bstd reads and writes over registered spans (Effect::ReadInto, Effect::WriteFrom, Cache::Bypassed)",
            }),
        };
        match made {
            Ok(facts) => Ok(RawDevice { file, facts }),
            Err(kind) => {
                // clone: the error's name, a refcount.
                let device = blaze_core::DirName::Named(file.name().clone());
                let _written = file.close_unsynced(Unsynced::No);
                Err(BlockError { device, range: None, op: BlockOp::Open, kind })
            }
        }
    }

    /// The device finished, every write flushed.
    #[blaze::consumes]
    pub fn close(self, cx: &Cx) -> Result<(), BlockKind> {
        let RawDevice { file, facts: _ } = self;
        file.close_synced(cx).map(|_| ()).map_err(BlockKind::Fs)
    }

    /// The device finished with nothing flushed.
    #[blaze::consumes]
    pub fn close_unsynced(self) {
        let RawDevice { file, facts: _ } = self;
        let _written = file.close_unsynced(Unsynced::No);
    }
}

impl Device for RawDevice {
    fn facts(&self) -> DeviceFacts {
        self.facts
    }

    fn name(&self) -> &FileName {
        self.file.name()
    }

    fn read(&mut self, cx: &Cx, at: Lba, into: &mut [u8]) -> Result<(), BlockKind> {
        let offset = at.0 * self.facts.logical.bytes();
        let asked = into.len();
        match self.file.read_into(into, offset).wait(cx) {
            Ok(n) if n == asked => Ok(()),
            Ok(n) => Err(BlockKind::Short { moved: n as u64 }),
            Err(error) => Err(BlockKind::Fs(error)),
        }
    }

    fn write(&mut self, cx: &Cx, at: Lba, bytes: &[u8], durable: Durable, _atomic: bool) -> Result<(), BlockKind> {
        // Buffered: the facts say one block atomic, which every whole-block write is.
        let offset = at.0 * self.facts.logical.bytes();
        self.file.write_at(cx, offset, bytes, durable).map_err(BlockKind::Fs)
    }

    fn flush(&mut self, cx: &Cx) -> Result<(), BlockKind> {
        self.file.flush(cx).map_err(BlockKind::Fs)
    }

    fn discard(&mut self, _cx: &Cx, _at: Lba, _len: Blocks) -> Result<(), BlockKind> {
        Err(BlockKind::Unbuilt { needs: "Effect::Discard: ioctl(BLKDISCARD) on a block device" })
    }
}
