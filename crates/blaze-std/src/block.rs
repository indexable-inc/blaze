//! The block capability (D182): a device of logical blocks, read and written in place by
//! whole blocks, that the store and any other capability can sit on in place of a folder of
//! files. What it costs a file system per operation (an open, a path walk, a name's journal)
//! a block device does not charge: on a one-function edit on tempo the opens alone were 9.5%
//! of the command's on-CPU time (2026-11-20).
//!
//! # The types
//!
//! - [`BlockSize`]: a logical (or physical) block, a power of two from 512 to 65,536 bytes.
//! - [`Lba`]: a block's number; [`Blocks`]: a count of blocks. Neither is a byte offset, so
//!   no byte offset reaches a device unconverted.
//! - [`Aligned`] and [`AlignedMut`]: a buffer whose address and length are whole multiples of
//!   a block, by construction: made only by [`Aligned::new`] (which checks), or from a
//!   task's frame ([`AlignedMut::in_frame`], which asks the frame for that alignment).
//! - [`DeviceFacts`]: the device as it says it is, read once when it is opened.
//! - [`Durable`]: bstd's file meaning, the same three values. `No`: nothing asked of the
//!   device; `Started`: the write handed to the device and its write-back begun, waited for by
//!   nothing; `Yes`: once the call returns a crash keeps the write (`RWF_DSYNC` on a raw
//!   device, a write then a flush on a file).
//! - [`BlockError`]: the device and the range, the operation, and why.
//!
//! # The seam
//!
//! [`Block<D, P>`] is the grant a capability holds; `D: Device` is the one seam (rule 78):
//! [`OverFile`] (a preallocated file standing in for a device), [`OverSim`] (blaze-core's
//! simulated device, `blaze_core::sim::block`) and [`RawDevice`] (a Linux block device the
//! grant names by its `/dev` path). The file and raw devices complete the same effects the
//! file drivers do (`Effect::Read`, `Effect::WriteAt`, `Effect::Sync`), on the device's one
//! name; nothing here makes a system call.
//!
//! # Plants
//!
//! [`BlockPlant::DurableBeforeFlush`]: a durable write reported complete before it is.
//! [`BlockPlant::AlignmentNotEnforced`]: a buffer not whole blocks accepted. Both are caught by
//! the conformance suite (`block/tests.rs`), with blaze-core's own plant of a tear below the
//! atomic unit.

pub mod over_file;
pub mod over_sim;
pub mod raw;
#[cfg(test)]
mod tests;

use core::alloc::Layout;
use core::fmt;
use core::num::NonZeroU32;
use core::ptr::NonNull;

use blaze_core::{DirName, FileName};
use blaze_core::planted::{Planted, Plants};
use blaze_mem::Frame;

use crate::{Stopped, cx::Cx, fs::FsError};

pub use crate::fs::Durable;
pub use self::{over_file::OverFile, over_sim::OverSim, raw::RawDevice};

/// A block's size in bytes: a power of two from 512 to 65,536.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct BlockSize(NonZeroU32);

impl BlockSize {
    /// 512 bytes: a drive's sector, the grain blaze-core's disk world tears every unsynced
    /// write by (`blaze_core::sim::SECTOR`).
    pub const SECTOR: BlockSize = BlockSize(NonZeroU32::new(512).unwrap());
    /// 4,096 bytes: a page, APFS's and ext4's and xfs's default block.
    pub const PAGE: BlockSize = BlockSize(NonZeroU32::new(4096).unwrap());

    /// `bytes` as a block size, if it is one.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub const fn new(bytes: u32) -> Option<BlockSize> {
        match NonZeroU32::new(bytes) {
            Some(n) if bytes.is_power_of_two() && bytes >= 512 && bytes <= 65_536 => Some(BlockSize(n)),
            _ => None,
        }
    }

    /// Its bytes.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// Its bytes, as an offset's type.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub const fn bytes(self) -> u64 {
        self.0.get() as u64
    }

    /// Whether `other` is a whole multiple of it.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub const fn divides(self, other: BlockSize) -> bool {
        other.get() % self.get() == 0
    }
}

/// A logical block's number on its device.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Lba(pub u64);

/// A count of logical blocks.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Blocks(pub u64);

impl Lba {
    /// Its first byte's offset, on a device of `size` blocks; `None` past `u64`.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub const fn offset(self, size: BlockSize) -> Option<u64> {
        self.0.checked_mul(size.bytes())
    }

    /// The block `n` after it; `None` past `u64`.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub const fn plus(self, n: Blocks) -> Option<Lba> {
        match self.0.checked_add(n.0) {
            Some(at) => Some(Lba(at)),
            None => None,
        }
    }
}

impl Blocks {
    /// Their bytes, at `size`; `None` past `u64`.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub const fn bytes(self, size: BlockSize) -> Option<u64> {
        self.0.checked_mul(size.bytes())
    }
}

/// A range of blocks: what every error names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Range {
    pub at: Lba,
    pub len: Blocks,
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blocks {}..{}", self.at.0, self.at.0.saturating_add(self.len.0))
    }
}

/// A buffer not whole blocks: what [`Aligned::new`] and [`AlignedMut::new`] refuse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Misaligned {
    pub addr: usize,
    pub len: usize,
    pub size: BlockSize,
}

impl fmt::Display for Misaligned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a block buffer of {} bytes at {:#x}: not whole {}-byte blocks, in address and length (bstd::block, D182)",
            self.len,
            self.addr,
            self.size.get()
        )
    }
}

impl core::error::Error for Misaligned {}

/// The bugs a control can plant in the block grant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockPlant {
    /// The grant as it is.
    None,
    /// A `Durable::Yes` write handed to its device as `Durable::No`, and reported done: a
    /// durable write reported complete before its flush.
    DurableBeforeFlush,
    /// A buffer that is not whole blocks accepted as one.
    AlignmentNotEnforced,
}

impl Plants for BlockPlant {}

fn whole(addr: usize, len: usize, size: BlockSize, plant: impl Planted<BlockPlant>) -> Result<(), Misaligned> {
    let n = size.get() as usize;
    match (addr % n == 0 && len % n == 0 && len > 0) || plant.has(BlockPlant::AlignmentNotEnforced) {
        true => Ok(()),
        false => Err(Misaligned { addr, len, size }),
    }
}

/// Bytes to write: their address and length whole blocks of `size`, by construction.
#[derive(Clone, Copy, Debug)]
pub struct Aligned<'a> {
    bytes: &'a [u8],
    size: BlockSize,
}

impl<'a> Aligned<'a> {
    /// `bytes`, if their address and length are whole blocks of `size` and not empty.
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub fn new(bytes: &'a [u8], size: BlockSize) -> Result<Aligned<'a>, Misaligned> {
        Aligned::planted(bytes, size, blaze_core::planted::NoPlant)
    }

    /// [`Aligned::new`] through the plant seam.
    pub fn planted(bytes: &'a [u8], size: BlockSize, plant: impl Planted<BlockPlant>) -> Result<Aligned<'a>, Misaligned> {
        whole(bytes.as_ptr().addr(), bytes.len(), size, plant)?;
        Ok(Aligned { bytes, size })
    }

    /// The bytes.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The block they are whole multiples of.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub fn size(&self) -> BlockSize {
        self.size
    }

    /// How many blocks of `of` they are (`of` divides their size).
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub fn blocks(&self, of: BlockSize) -> Blocks {
        Blocks(self.bytes.len() as u64 / of.bytes())
    }
}

/// Room to read into: its address and length whole blocks of `size`, by construction.
#[derive(Debug)]
pub struct AlignedMut<'a> {
    bytes: &'a mut [u8],
    size: BlockSize,
}

impl<'a> AlignedMut<'a> {
    /// `bytes`, if their address and length are whole blocks of `size` and not empty.
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub fn new(bytes: &'a mut [u8], size: BlockSize) -> Result<AlignedMut<'a>, Misaligned> {
        AlignedMut::planted(bytes, size, blaze_core::planted::NoPlant)
    }

    /// [`AlignedMut::new`] through the plant seam.
    pub fn planted(
        bytes: &'a mut [u8],
        size: BlockSize,
        plant: impl Planted<BlockPlant>,
    ) -> Result<AlignedMut<'a>, Misaligned> {
        whole(bytes.as_ptr().addr(), bytes.len(), size, plant)?;
        Ok(AlignedMut { bytes, size })
    }

    /// `blocks` blocks of `size` from `frame`, zeroed: the frame is asked for the block's
    /// alignment (`Frame::try_alloc_layout` aligns its cursor up to `Layout::align`, wasting at
    /// most `size - 1` bytes), so the buffer is whole blocks in address as in length. Freed
    /// with the frame. `Stopped::OutOfMemory` past the frame's limit, naming `arena`.
    #[expect(unsafe_code, reason = "a frame's fresh block zeroed and lent as a slice for the frame's life, with its SAFETY")]
    pub fn in_frame(
        frame: &'a Frame,
        blocks: Blocks,
        size: BlockSize,
        arena: &'static str,
    ) -> Result<AlignedMut<'a>, Stopped> {
        let asked = blocks.bytes(size).and_then(|n| usize::try_from(n).ok()).filter(|&n| n > 0);
        let Some(len) = asked else {
            return Err(Stopped::OutOfMemory { arena, asked: usize::MAX });
        };
        let layout = Layout::from_size_align(len, size.get() as usize)
            .map_err(|_| Stopped::OutOfMemory { arena, asked: len })?;
        let at: NonNull<u8> = frame.try_alloc_layout(layout).map_err(|_| Stopped::OutOfMemory { arena, asked: len })?;
        // SAFETY: `at` is `len` bytes the frame handed out for `'a` (the frame's borrow), to this
        // call alone, aligned to `size` (the layout's alignment); they are written to zeros
        // before a slice of them exists, so every byte of the slice is initialized.
        let bytes = unsafe {
            core::ptr::write_bytes(at.as_ptr(), 0, len);
            core::slice::from_raw_parts_mut(at.as_ptr(), len)
        };
        Ok(AlignedMut { bytes, size })
    }

    /// The bytes.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub fn bytes(&self) -> &[u8] {
        self.bytes
    }

    /// The bytes, to fill.
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.bytes
    }

    /// The same room, to write from.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub fn as_aligned(&self) -> Aligned<'_> {
        Aligned { bytes: self.bytes, size: self.size }
    }

    /// The block it is whole multiples of.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub fn size(&self) -> BlockSize {
        self.size
    }

    /// How many blocks of `of` it holds.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub fn blocks(&self, of: BlockSize) -> Blocks {
        Blocks(self.bytes.len() as u64 / of.bytes())
    }
}

/// What a device says of itself, read once at its opening.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeviceFacts {
    /// The smallest unit it reads and writes.
    pub logical: BlockSize,
    /// The unit it writes without a read-modify-write: a write of less costs a read.
    pub physical: BlockSize,
    /// The most blocks one aligned write of which a crash never tears: at least one.
    pub atomic_write: Blocks,
    /// How many operations it takes in flight at once.
    pub queue_depth: NonZeroU32,
    /// Whether a completed write may be lost at a crash until a flush.
    pub volatile_cache: bool,
    /// Its size.
    pub size: Blocks,
}

/// What [`DeviceFacts::new`] is told.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeviceFactsOpts {
    pub logical: BlockSize,
    pub physical: BlockSize,
    pub atomic_write: Blocks,
    pub queue_depth: NonZeroU32,
    pub volatile_cache: bool,
    pub size: Blocks,
}

/// Facts that cannot be a device's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FactsError {
    /// A logical block a device said that is no [`BlockSize`].
    Logical { bytes: u32 },
    /// The physical block is not a whole multiple of the logical.
    Physical { logical: BlockSize, physical: BlockSize },
    /// The atomic unit is no blocks, or not a power of two.
    Atomic { atomic_write: Blocks },
    /// The device has no blocks, or more bytes than a `u64` counts.
    Size { size: Blocks, logical: BlockSize },
}

impl fmt::Display for FactsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            FactsError::Logical { bytes } => {
                write!(f, "a block device of {bytes}-byte logical blocks: not a power of two from 512 to 65536")
            }
            FactsError::Physical { logical, physical } => write!(
                f,
                "a block device of {}-byte physical blocks over {}-byte logical ones: not a whole multiple",
                physical.get(),
                logical.get()
            ),
            FactsError::Atomic { atomic_write } => {
                write!(f, "a block device whose atomic write is {} blocks: not a power of two, at least 1", atomic_write.0)
            }
            FactsError::Size { size, logical } => {
                write!(f, "a block device of {} blocks of {} bytes: none, or past a u64 of bytes", size.0, logical.get())
            }
        }
    }
}

impl core::error::Error for FactsError {}

impl DeviceFacts {
    /// The facts, checked.
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub fn new(opts: DeviceFactsOpts) -> Result<DeviceFacts, FactsError> {
        let DeviceFactsOpts { logical, physical, atomic_write, queue_depth, volatile_cache, size } = opts;
        if !logical.divides(physical) {
            return Err(FactsError::Physical { logical, physical });
        }
        if atomic_write.0 == 0 || !atomic_write.0.is_power_of_two() {
            return Err(FactsError::Atomic { atomic_write });
        }
        if size.0 == 0 || size.bytes(logical).is_none() {
            return Err(FactsError::Size { size, logical });
        }
        Ok(DeviceFacts { logical, physical, atomic_write, queue_depth, volatile_cache, size })
    }

    /// Its bytes.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    pub fn bytes(&self) -> u64 {
        // Checked at `new`.
        self.size.0 * self.logical.bytes()
    }
}

/// Which operation an error is about.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BlockOp {
    Open,
    Read,
    Write,
    Flush,
    Discard,
}

/// Why a block operation failed.
#[derive(Clone, Debug)]
pub enum BlockKind {
    /// The range runs past the device's end.
    Past { size: Blocks },
    /// The buffer's block is not a whole multiple of the device's logical block.
    OtherBlock { buffer: BlockSize, logical: BlockSize },
    /// An atomic write longer than the device's atomic unit, or not aligned to it.
    NotAtomic { atomic_write: Blocks },
    /// The device could not read this block (a latent sector error).
    Unreadable { lba: Lba },
    /// The read or write moved fewer bytes than asked.
    Short { moved: u64 },
    /// The file standing in for the device: its error.
    Fs(FsError),
    /// The simulated device's refusal.
    Sim(blaze_core::sim::block::SimBlockError),
    /// The simulated device's shape is no device's facts.
    Shape(blaze_core::sim::block::ShapeError),
    /// The facts the device gave cannot be a device's.
    Facts(FactsError),
    /// What the operation needs is not built yet: the effect or call it names
    /// (docs/v2/plan/design/store-next.md, "D182").
    Unbuilt { needs: &'static str },
    /// The context stopped the wait.
    Stopped(Stopped),
}

/// A block operation that failed: the device, the range, the operation and why.
#[derive(Clone, Debug)]
pub struct BlockError {
    /// The device's name, under its driver's root (a file's error at its opening names what
    /// the file call named).
    pub device: DirName,
    pub range: Option<Range>,
    pub op: BlockOp,
    pub kind: BlockKind,
}

impl BlockError {
    /// A file call's failure while `op` made a device of the file: named as the call named it.
    pub fn of_fs(op: BlockOp, error: FsError) -> BlockError {
        // clone: the error's name, a refcount.
        BlockError { device: error.path.clone(), range: None, op, kind: BlockKind::Fs(error) }
    }
}

impl fmt::Display for BlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "block device {}: {:?}", self.device, self.op)?;
        if let Some(range) = self.range {
            write!(f, " of {range}")?;
        }
        match &self.kind {
            BlockKind::Past { size } => write!(f, ": past its end at block {}", size.0),
            BlockKind::OtherBlock { buffer, logical } => write!(
                f,
                ": a buffer of {}-byte blocks on a device of {}-byte ones",
                buffer.get(),
                logical.get()
            ),
            BlockKind::NotAtomic { atomic_write } => {
                write!(f, ": an atomic write must be within one aligned unit of {} blocks", atomic_write.0)
            }
            BlockKind::Unreadable { lba } => write!(f, ": block {} is unreadable", lba.0),
            BlockKind::Short { moved } => write!(f, ": only {moved} bytes moved"),
            BlockKind::Fs(error) => write!(f, ": {error}"),
            BlockKind::Sim(error) => write!(f, ": {error}"),
            BlockKind::Shape(error) => write!(f, ": {error}"),
            BlockKind::Facts(error) => write!(f, ": {error}"),
            BlockKind::Unbuilt { needs } => write!(f, ": needs {needs}, which is not built (D182)"),
            BlockKind::Stopped(stopped) => write!(f, ": {stopped}"),
        }
    }
}

impl core::error::Error for BlockError {}

/// A device under the grant: the seam (rule 78) its three makings implement. Ranges are
/// checked by [`Block`] before they reach it; it answers with the [`BlockKind`] alone and
/// [`Block`] names the device and the range.
pub trait Device {
    /// Its facts, as read at its opening.
    fn facts(&self) -> DeviceFacts;
    /// Its name, for errors.
    fn name(&self) -> &FileName;
    /// `into` filled from `at`.
    fn read(&mut self, cx: &Cx, at: Lba, into: &mut [u8]) -> Result<(), BlockKind>;
    /// `bytes` written at `at` as durably as `durable` says; `atomic`: the caller asked that a
    /// crash not tear it (checked against the facts by [`Block`]).
    fn write(&mut self, cx: &Cx, at: Lba, bytes: &[u8], durable: Durable, atomic: bool) -> Result<(), BlockKind>;
    /// Every write that completed before this durable once it returns.
    fn flush(&mut self, cx: &Cx) -> Result<(), BlockKind>;
    /// `len` blocks at `at` given back to the device.
    fn discard(&mut self, cx: &Cx, at: Lba, len: Blocks) -> Result<(), BlockKind>;
}

/// The block grant: a device, its facts, and the plant seam.
pub struct Block<D: Device, P: Planted<BlockPlant>> {
    device: D,
    facts: DeviceFacts,
    plant: P,
}

impl<D: Device, P: Planted<BlockPlant>> Block<D, P> {
    /// The grant over `device`.
    pub fn new(device: D, plant: P) -> Block<D, P> {
        let facts = device.facts();
        Block { device, facts, plant }
    }

    /// The device's facts.
    pub fn facts(&self) -> DeviceFacts {
        self.facts
    }

    /// The device itself, given back.
    pub fn into_device(self) -> D {
        self.device
    }

    /// The device itself, for what is not the grant's to do (a simulation's crash).
    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }

    fn error(&self, op: BlockOp, range: Option<Range>, kind: BlockKind) -> BlockError {
        // clone: the error's name, a refcount.
        BlockError { device: DirName::Named(self.device.name().clone()), range, op, kind }
    }

    /// That `len` blocks at `at` are on the device, in a buffer of `buffer`-byte blocks.
    fn within(&self, op: BlockOp, range: Range, buffer: Option<BlockSize>) -> Result<(), BlockError> {
        if let Some(buffer) = buffer
            && !self.facts.logical.divides(buffer)
        {
            return Err(self.error(op, Some(range), BlockKind::OtherBlock { buffer, logical: self.facts.logical }));
        }
        match range.at.plus(range.len) {
            Some(end) if end.0 <= self.facts.size.0 => Ok(()),
            _ => Err(self.error(op, Some(range), BlockKind::Past { size: self.facts.size })),
        }
    }

    /// `into` filled from block `at`.
    pub fn read(&mut self, cx: &Cx, at: Lba, mut into: AlignedMut<'_>) -> Result<(), BlockError> {
        let range = Range { at, len: into.blocks(self.facts.logical) };
        self.within(BlockOp::Read, range, Some(into.size()))?;
        self.device.read(cx, at, into.bytes_mut()).map_err(|kind| self.error(BlockOp::Read, Some(range), kind))
    }

    /// `from` written at block `at`, as durably as `durable` says. A crash may tear it at
    /// any atomic unit ([`DeviceFacts::atomic_write`]) it spans.
    pub fn write(&mut self, cx: &Cx, at: Lba, from: Aligned<'_>, durable: Durable) -> Result<(), BlockError> {
        self.written(cx, at, from, durable, false)
    }

    /// `from` written at block `at` whole or not at all through any crash: it must lie within
    /// one aligned atomic unit of the device, or it is refused, naming the unit.
    pub fn write_atomic(&mut self, cx: &Cx, at: Lba, from: Aligned<'_>, durable: Durable) -> Result<(), BlockError> {
        let len = from.blocks(self.facts.logical);
        let unit = self.facts.atomic_write.0;
        if at.0 / unit != (at.0 + len.0.max(1) - 1) / unit {
            let range = Range { at, len };
            return Err(self.error(BlockOp::Write, Some(range), BlockKind::NotAtomic { atomic_write: self.facts.atomic_write }));
        }
        self.written(cx, at, from, durable, true)
    }

    fn written(&mut self, cx: &Cx, at: Lba, from: Aligned<'_>, durable: Durable, atomic: bool) -> Result<(), BlockError> {
        let range = Range { at, len: from.blocks(self.facts.logical) };
        self.within(BlockOp::Write, range, Some(from.size()))?;
        let told = match durable == Durable::Yes && self.plant.has(BlockPlant::DurableBeforeFlush) {
            true => Durable::No,
            false => durable,
        };
        self.device
            .write(cx, at, from.bytes(), told, atomic)
            .map_err(|kind| self.error(BlockOp::Write, Some(range), kind))
    }

    /// Every write that returned before this is durable once it returns.
    pub fn flush(&mut self, cx: &Cx) -> Result<(), BlockError> {
        self.device.flush(cx).map_err(|kind| self.error(BlockOp::Flush, None, kind))
    }

    /// `len` blocks at `at` given back: what a read of them gives after is the device's (zeros
    /// on the simulated one). Durable at the next flush.
    pub fn discard(&mut self, cx: &Cx, at: Lba, len: Blocks) -> Result<(), BlockError> {
        let range = Range { at, len };
        self.within(BlockOp::Discard, range, None)?;
        self.device.discard(cx, at, len).map_err(|kind| self.error(BlockOp::Discard, Some(range), kind))
    }
}
