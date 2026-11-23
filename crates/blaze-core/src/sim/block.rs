//! The simulated block device (D182): one device of drawn facts, with a volatile write cache,
//! a queue whose writes complete in any order, crashes that lose or keep each atomic unit of
//! what was not flushed, and a latent sector error on read. It is the oracle of bstd's block
//! conformance suite (`blaze-std/src/block/tests.rs`) and a device the [`bstd::block::Block`]
//! grant runs over in a simulation.
//!
//! The rules it holds, each the weakest a real device keeps (NVMe 2.0 base specification,
//! sections 5.15 "Flush", 6.4 "Atomic operations"; SCSI SBC-4 for the cache bit):
//!
//! - **Alignment.** Every offset and length is a whole number of logical blocks; anything else
//!   is refused, naming the range ([`SimBlockError::Unaligned`]). Nothing is rounded.
//! - **Completion.** A write submitted is in flight until it completes; at most `queue_depth`
//!   are in flight; [`SimBlock::complete_any`] completes any one of them, so completions come
//!   in any order within the queue (a drive reorders its queue).
//! - **The cache.** With `volatile_cache`, a completed write is in the drive's cache and a
//!   crash may lose it; a flush makes every write completed before it durable (NVMe: a Flush
//!   covers commands completed before it was submitted, not those still in flight). Without
//!   it (a drive with power-loss protection, or write-through), a completed write is durable.
//!   A forced-unit-access write (`fua`, what `RWF_DSYNC` sends) is durable when it completes.
//! - **Tears.** At a crash every write not durable (completed in the cache, or in flight)
//!   lands or not by atomic units: each `atomic` logical blocks of it, aligned, all or none,
//!   independently and in any order. Never by less ([`BlockModelPlant::TearsBelowAtomic`] is
//!   the bug that does). So after a crash each atomic unit holds its durable bytes or one of
//!   the versions written to it since.
//! - **Latent errors.** One drawn block may be unreadable ([`Shape::bad`]): a read that covers
//!   it fails naming it ([`SimBlockError::Unreadable`]), as a drive reports an uncorrectable
//!   read; a write over it clears it, as a drive remaps a block on write.
//! - **Discard.** A discarded range reads as zeros (a drive that reports "read zeros after
//!   trim", RZAT); it is a change like a write, durable at the next flush.
//!
//! [`History`] is the oracle, written apart from the device so the device's own crash is
//! checked by it and not by itself: per atomic unit, the durable bytes and every version
//! written since. [`History::seen`] judges what a reader sees after a reopen.
//!
//! [`bstd::block::Block`]: ../../../blaze_std/block/struct.Block.html

use core::alloc::Allocator;
use core::fmt;

use blaze_rng::SplitMix64;

use crate::planted::{Planted, Plants};

/// The most logical blocks a simulated device has: a table has a cap (rule bounded-progress).
/// 4,096 blocks of at most 4 KiB is 16 MiB per image, two images: what a test may hold.
pub const MOST_BLOCKS: u64 = 4096;

/// The deepest queue a simulated device has: NVMe allows 65,535 per queue; a test reaches every
/// reordering of a queue of 32 within its cases.
pub const MOST_QUEUE: u32 = 32;

/// A device's shape: what bstd's `DeviceFacts` say of it, in bytes and counts, so blaze-core
/// needs nothing of bstd.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shape {
    /// The logical block: a power of two, 512 to 65,536 bytes.
    pub logical: u32,
    /// The physical block: a power-of-two multiple of `logical`, what a write of less costs a
    /// read-modify-write of.
    pub physical: u32,
    /// The atomic write unit, in logical blocks (a power of two, at least 1): what a crash never
    /// tears.
    pub atomic: u32,
    /// How many writes may be in flight at once, 1 to [`MOST_QUEUE`].
    pub queue_depth: u32,
    /// Whether a completed write may be lost at a crash until a flush.
    pub volatile_cache: bool,
    /// How many logical blocks, 1 to [`MOST_BLOCKS`].
    pub blocks: u64,
    /// The one block a read fails on, if any.
    pub bad: Option<u64>,
}

/// A shape that cannot be a device's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShapeError {
    /// `logical` is not a power of two from 512 to 65,536.
    Logical { logical: u32 },
    /// `physical` is not a power-of-two multiple of `logical`.
    Physical { logical: u32, physical: u32 },
    /// `atomic` is 0 or not a power of two, or does not divide `blocks`.
    Atomic { atomic: u32, blocks: u64 },
    /// `queue_depth` is 0 or past [`MOST_QUEUE`].
    Queue { queue_depth: u32 },
    /// `blocks` is 0 or past [`MOST_BLOCKS`].
    Blocks { blocks: u64 },
    /// `bad` is past the last block.
    Bad { bad: u64, blocks: u64 },
}

impl fmt::Display for ShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ShapeError::Logical { logical } => {
                write!(f, "a simulated block device of {logical}-byte logical blocks: not a power of two from 512 to 65536")
            }
            ShapeError::Physical { logical, physical } => write!(
                f,
                "a simulated block device of {physical}-byte physical blocks over {logical}-byte logical ones: not a power-of-two multiple"
            ),
            ShapeError::Atomic { atomic, blocks } => write!(
                f,
                "a simulated block device with an atomic unit of {atomic} blocks over {blocks}: not a power of two dividing the device"
            ),
            ShapeError::Queue { queue_depth } => {
                write!(f, "a simulated block device with a queue of {queue_depth}: not 1 to {MOST_QUEUE}")
            }
            ShapeError::Blocks { blocks } => {
                write!(f, "a simulated block device of {blocks} blocks: not 1 to {MOST_BLOCKS} (sim::block::MOST_BLOCKS)")
            }
            ShapeError::Bad { bad, blocks } => {
                write!(f, "a simulated block device of {blocks} blocks with block {bad} bad: past its end")
            }
        }
    }
}

impl std::error::Error for ShapeError {}

impl Shape {
    /// The shape, checked.
    pub fn checked(self) -> Result<Shape, ShapeError> {
        let Shape { logical, physical, atomic, queue_depth, blocks, bad, .. } = self;
        if !logical.is_power_of_two() || !(512..=65_536).contains(&logical) {
            return Err(ShapeError::Logical { logical });
        }
        if physical < logical || !physical.is_power_of_two() || physical > 65_536 {
            return Err(ShapeError::Physical { logical, physical });
        }
        if blocks == 0 || blocks > MOST_BLOCKS {
            return Err(ShapeError::Blocks { blocks });
        }
        if atomic == 0 || !atomic.is_power_of_two() || blocks % u64::from(atomic) != 0 {
            return Err(ShapeError::Atomic { atomic, blocks });
        }
        if queue_depth == 0 || queue_depth > MOST_QUEUE {
            return Err(ShapeError::Queue { queue_depth });
        }
        match bad {
            Some(bad) if bad >= blocks => Err(ShapeError::Bad { bad, blocks }),
            _ => Ok(self),
        }
    }

    /// The device's bytes.
    pub const fn bytes(&self) -> u64 {
        self.blocks * self.logical as u64
    }

    /// The atomic unit's bytes.
    pub const fn unit(&self) -> u64 {
        self.atomic as u64 * self.logical as u64
    }

    /// A shape drawn by `below(n)` (a number under `n`): every field over its whole range, the
    /// device at most `most_blocks` (a test's own cap, at most [`MOST_BLOCKS`]). What the D182
    /// limits were sized by: see `docs/v2/plan/design/store-next.md`, "D182".
    pub fn drawn(mut below: impl FnMut(u64) -> u64, most_blocks: u64) -> Shape {
        let logical = 512u32 << below(4); // 512 to 4096: the sizes devices ship.
        let physical = logical << below(4); // up to 8x: 512e drives are 8 (512 over 4096).
        let atomic = 1u32 << below(4); // 1 to 8 blocks: NVMe AWUPF is often 0 (one block).
        let queue_depth = 1 + below(u64::from(MOST_QUEUE)) as u32;
        let volatile_cache = below(4) != 0; // most consumer drives have one.
        let most_units = (most_blocks.min(MOST_BLOCKS) / u64::from(atomic)).max(1);
        let blocks = (1 + below(most_units)) * u64::from(atomic);
        let bad = match below(4) {
            0 => Some(below(blocks)),
            _ => None,
        };
        Shape { logical, physical, atomic, queue_depth, volatile_cache, blocks, bad }
    }
}

/// The bugs a control can plant in the simulated device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockModelPlant {
    /// The device as it is.
    None,
    /// A crash tears a write by halves of the atomic unit: what a model that accepted a tear
    /// smaller than the atomic unit would let a reader see. [`History::seen`] names it
    /// [`Seen::TornBelowAtomic`].
    TearsBelowAtomic,
}

impl Plants for BlockModelPlant {}

/// A write handed to the device and not yet completed: its ticket.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ticket(u64);

/// Why the simulated device refused an operation. Each names the range in blocks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimBlockError {
    /// An offset or length that is not a whole number of logical blocks.
    Unaligned { at: u64, len: u64, logical: u32 },
    /// A range past the device's end.
    Past { at: u64, len: u64, bytes: u64 },
    /// The queue holds `queue_depth` writes in flight already.
    QueueFull { queue_depth: u32 },
    /// A read covering the block with a latent error.
    Unreadable { lba: u64 },
    /// A ticket the device does not hold (completed already, or lost at a crash).
    NoTicket { ticket: Ticket },
}

impl fmt::Display for SimBlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SimBlockError::Unaligned { at, len, logical } => {
                write!(f, "simulated block device: bytes {at}..{} are not whole {logical}-byte blocks", at + len)
            }
            SimBlockError::Past { at, len, bytes } => {
                write!(f, "simulated block device: bytes {at}..{} past its {bytes}", at + len)
            }
            SimBlockError::QueueFull { queue_depth } => {
                write!(f, "simulated block device: {queue_depth} writes in flight, its whole queue")
            }
            SimBlockError::Unreadable { lba } => write!(f, "simulated block device: block {lba} is unreadable"),
            SimBlockError::NoTicket { ticket } => write!(f, "simulated block device: no write in flight as {ticket:?}"),
        }
    }
}

impl std::error::Error for SimBlockError {}

/// One write the device holds, in flight or completed into its cache.
struct Held<A: Allocator> {
    ticket: Ticket,
    at: u64,
    bytes: Vec<u8, A>,
    fua: bool,
    /// Its place in completion order, from 1; [`IN_FLIGHT`] while in flight.
    seq: u64,
}

/// The `seq` of a write not yet completed: ordered against nothing, so a crash may land it over
/// anything.
const IN_FLIGHT: u64 = u64::MAX;

/// The simulated device. Its memory is `alloc`'s, named by its maker (rule bump-allocate):
/// two images of the device and the writes held.
pub struct SimBlock<A: Allocator + Clone, P: Planted<BlockModelPlant>> {
    shape: Shape,
    /// What survives a crash.
    durable: Vec<u8, A>,
    /// For each logical block, the completion `seq` of the write its durable bytes are: a
    /// cached write completed before it never lands over it at a crash (a drive's cache holds
    /// one copy of a block, the newest).
    durable_seq: Vec<u64, A>,
    /// Writes completed so far: the last `seq` given.
    done: u64,
    /// What a read sees: every completed write applied.
    seen: Vec<u8, A>,
    /// Completed writes not yet durable, oldest first.
    cached: Vec<Held<A>, A>,
    /// Submitted writes not yet completed.
    flight: Vec<Held<A>, A>,
    bad: Option<u64>,
    next: u64,
    rng: SplitMix64,
    alloc: A,
    plant: P,
}

impl<A: Allocator + Clone, P: Planted<BlockModelPlant>> SimBlock<A, P> {
    /// A device of `shape`, all zeros, its choices (completion order, what a crash keeps) drawn
    /// from `seed`.
    pub fn new(shape: Shape, seed: u64, alloc: A, plant: P) -> Result<SimBlock<A, P>, ShapeError> {
        let shape = shape.checked()?;
        let len = shape.bytes() as usize;
        let mut durable = Vec::with_capacity_in(len, alloc.clone());
        durable.resize(len, 0);
        let mut seen = Vec::with_capacity_in(len, alloc.clone());
        seen.resize(len, 0);
        let mut durable_seq = Vec::with_capacity_in(shape.blocks as usize, alloc.clone());
        durable_seq.resize(shape.blocks as usize, 0);
        Ok(SimBlock {
            shape,
            durable,
            durable_seq,
            done: 0,
            seen,
            cached: Vec::new_in(alloc.clone()),
            flight: Vec::with_capacity_in(shape.queue_depth as usize, alloc.clone()),
            bad: shape.bad,
            next: 0,
            rng: SplitMix64::new(seed),
            alloc,
            plant,
        })
    }

    /// Its shape.
    pub fn shape(&self) -> Shape {
        self.shape
    }

    /// How many writes are in flight.
    pub fn in_flight(&self) -> usize {
        self.flight.len()
    }

    fn range(&self, at: u64, len: u64) -> Result<(), SimBlockError> {
        let logical = self.shape.logical;
        if at % u64::from(logical) != 0 || len % u64::from(logical) != 0 {
            return Err(SimBlockError::Unaligned { at, len, logical });
        }
        match at.checked_add(len) {
            Some(end) if end <= self.shape.bytes() => Ok(()),
            _ => Err(SimBlockError::Past { at, len, bytes: self.shape.bytes() }),
        }
    }

    /// `bytes` handed to the device at byte `at`, in flight: [`SimBlock::complete_any`] or
    /// [`SimBlock::complete`] completes it. `fua`: durable once it completes.
    pub fn submit(&mut self, at: u64, bytes: &[u8], fua: bool) -> Result<Ticket, SimBlockError> {
        self.range(at, bytes.len() as u64)?;
        if self.flight.len() >= self.shape.queue_depth as usize {
            return Err(SimBlockError::QueueFull { queue_depth: self.shape.queue_depth });
        }
        let ticket = Ticket(self.next);
        self.next += 1;
        let mut held = Vec::with_capacity_in(bytes.len(), self.alloc.clone());
        held.extend_from_slice(bytes);
        self.flight.push(Held { ticket, at, bytes: held, fua, seq: IN_FLIGHT });
        Ok(ticket)
    }

    /// Any one write in flight completed, the device's choice: which, or `None` with none in
    /// flight.
    pub fn complete_any(&mut self) -> Option<Ticket> {
        if self.flight.is_empty() {
            return None;
        }
        let at = self.rng.below(self.flight.len() as u64) as usize;
        let held = self.flight.swap_remove(at);
        let ticket = held.ticket;
        self.landed(held);
        Some(ticket)
    }

    /// The write `ticket` completed.
    pub fn complete(&mut self, ticket: Ticket) -> Result<(), SimBlockError> {
        let Some(at) = self.flight.iter().position(|held| held.ticket == ticket) else {
            return Err(SimBlockError::NoTicket { ticket });
        };
        let held = self.flight.swap_remove(at);
        self.landed(held);
        Ok(())
    }

    fn landed(&mut self, mut held: Held<A>) {
        self.done += 1;
        held.seq = self.done;
        let (at, end) = (held.at as usize, held.at as usize + held.bytes.len());
        self.seen[at..end].copy_from_slice(&held.bytes);
        if let Some(bad) = self.bad {
            let logical = u64::from(self.shape.logical);
            if (held.at / logical..(held.at + held.bytes.len() as u64) / logical).contains(&bad) {
                self.bad = None;
            }
        }
        match held.fua || !self.shape.volatile_cache {
            true => self.made_durable(&held, at, end),
            false => self.cached.push(held),
        }
    }

    /// `held`'s bytes `at..end` of the device on the media, wherever they are newer than what
    /// is there.
    fn made_durable(&mut self, held: &Held<A>, at: usize, end: usize) {
        let logical = self.shape.logical as usize;
        for block in at / logical..end / logical {
            if held.seq <= self.durable_seq[block] && held.seq != IN_FLIGHT {
                continue;
            }
            let from = block * logical - held.at as usize;
            self.durable[block * logical..(block + 1) * logical].copy_from_slice(&held.bytes[from..from + logical]);
            if held.seq != IN_FLIGHT {
                self.durable_seq[block] = held.seq;
            }
        }
    }

    /// `bytes` written at `at` and completed: a submit and its own completion.
    pub fn write(&mut self, at: u64, bytes: &[u8], fua: bool) -> Result<(), SimBlockError> {
        let ticket = self.submit(at, bytes, fua)?;
        self.complete(ticket)
    }

    /// `len` zero bytes at `at`, as a write that is not forced: a discard reads as zeros
    /// (RZAT) and is durable at the next flush.
    pub fn discard(&mut self, at: u64, len: u64) -> Result<(), SimBlockError> {
        self.range(at, len)?;
        let mut zeros = Vec::with_capacity_in(len as usize, self.alloc.clone());
        zeros.resize(len as usize, 0);
        let ticket = Ticket(self.next);
        self.next += 1;
        self.landed(Held { ticket, at, bytes: zeros, fua: false, seq: IN_FLIGHT });
        Ok(())
    }

    /// Every write completed before this made durable. Writes in flight are not covered.
    pub fn flush(&mut self) {
        let cached = core::mem::replace(&mut self.cached, Vec::new_in(self.alloc.clone()));
        for held in cached.iter() {
            let (at, end) = (held.at as usize, held.at as usize + held.bytes.len());
            self.made_durable(held, at, end);
        }
    }

    /// `into.len()` bytes at `at` into `into`, as the device holds them now.
    pub fn read(&self, at: u64, into: &mut [u8]) -> Result<(), SimBlockError> {
        self.range(at, into.len() as u64)?;
        let logical = u64::from(self.shape.logical);
        if let Some(bad) = self.bad
            && (at / logical..(at + into.len() as u64) / logical).contains(&bad)
        {
            return Err(SimBlockError::Unreadable { lba: bad });
        }
        into.copy_from_slice(&self.seen[at as usize..at as usize + into.len()]);
        Ok(())
    }

    /// The power lost: every write not durable lands or not by atomic units, each unit on its
    /// own, cached writes oldest first and then those in flight in an order drawn; nothing is
    /// in flight or cached after, and a read sees what is durable.
    pub fn crash(&mut self) {
        let unit = match self.plant.has(BlockModelPlant::TearsBelowAtomic) {
            // The planted bug: a tear by half the unit (at least a byte).
            true => (self.shape.unit() / 2).max(1) as usize,
            false => self.shape.unit() as usize,
        };
        // In flight: any order (a drive reorders its queue).
        let mut flight = core::mem::replace(&mut self.flight, Vec::new_in(self.alloc.clone()));
        let mut order = Vec::with_capacity_in(flight.len(), self.alloc.clone());
        while !flight.is_empty() {
            let at = self.rng.below(flight.len() as u64) as usize;
            order.push(flight.swap_remove(at));
        }
        let cached = core::mem::replace(&mut self.cached, Vec::new_in(self.alloc.clone()));
        for held in cached.iter().chain(order.iter()) {
            // Writes are whole logical blocks; a unit of the write is the part of it inside one
            // aligned unit of the device, landed whole or not at all. A cached write lands only
            // over blocks it is newer than; one in flight over any.
            let mut at = held.at as usize;
            let end = held.at as usize + held.bytes.len();
            while at < end {
                let stop = ((at / unit + 1) * unit).min(end);
                if self.rng.below(2) == 0 {
                    if unit >= self.shape.logical as usize {
                        self.made_durable(held, at, stop);
                    } else {
                        // Only the planted tear is finer than a block.
                        let from = at - held.at as usize;
                        self.durable[at..stop].copy_from_slice(&held.bytes[from..from + (stop - at)]);
                    }
                }
                at = stop;
            }
        }
        self.seen.copy_from_slice(&self.durable);
    }

    /// What survives a crash now: the durable image.
    pub fn durable(&self) -> &[u8] {
        &self.durable
    }
}

/// What a reader saw that the oracle does not allow: the invariant broken, by atomic unit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seen {
    /// A read with no crash between gave other bytes than the last written.
    ReadDiffers { unit: u64 },
    /// After a crash, a unit whose every write was acknowledged durable holds other bytes:
    /// a durable write reported complete before it was.
    LostDurable { unit: u64 },
    /// After a crash, a unit holds no version whole, but each of its halves is some version's:
    /// a tear smaller than the atomic unit.
    TornBelowAtomic { unit: u64 },
    /// After a crash, a unit holds bytes no write gave it.
    NotWritten { unit: u64 },
}

impl fmt::Display for Seen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Seen::ReadDiffers { unit } => write!(f, "atomic unit {unit} reads other bytes than were last written to it"),
            Seen::LostDurable { unit } => {
                write!(f, "after a crash atomic unit {unit} lost a write acknowledged durable")
            }
            Seen::TornBelowAtomic { unit } => {
                write!(f, "after a crash atomic unit {unit} is torn below the atomic write unit")
            }
            Seen::NotWritten { unit } => write!(f, "after a crash atomic unit {unit} holds bytes no write gave it"),
        }
    }
}

/// The oracle: per atomic unit, its durable bytes and each version written since, oldest
/// first. Writes it is told of are whole atomic units (a write of part of a unit is not a
/// version of the unit and the oracle refuses it: [`History::write`]).
pub struct History<A: Allocator + Copy> {
    unit: usize,
    /// For each unit, its versions: the first durable, the rest since.
    units: Vec<Vec<Vec<u8, A>, A>, A>,
    alloc: A,
}

/// A write the oracle cannot judge: not whole atomic units of its device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotWholeUnits {
    pub at: u64,
    pub len: u64,
    pub unit: u64,
}

impl<A: Allocator + Copy> History<A> {
    /// The oracle of a device of `bytes` bytes, all zeros, torn at most by `unit` bytes.
    pub fn new(bytes: u64, unit: u64, alloc: A) -> History<A> {
        let count = (bytes / unit) as usize;
        let mut units = Vec::with_capacity_in(count, alloc);
        for _ in 0..count {
            let mut zero = Vec::with_capacity_in(unit as usize, alloc);
            zero.resize(unit as usize, 0);
            let mut versions = Vec::new_in(alloc);
            versions.push(zero);
            units.push(versions);
        }
        History { unit: unit as usize, units, alloc }
    }

    /// `bytes` written at `at`; `durable`: acknowledged durable, so its units have no other
    /// version a crash may leave.
    pub fn write(&mut self, at: u64, bytes: &[u8], durable: bool) -> Result<(), NotWholeUnits> {
        let unit = self.unit as u64;
        if at % unit != 0 || bytes.len() as u64 % unit != 0 {
            return Err(NotWholeUnits { at, len: bytes.len() as u64, unit });
        }
        for (n, chunk) in bytes.chunks(self.unit).enumerate() {
            let versions = &mut self.units[(at / unit) as usize + n];
            let mut version = Vec::with_capacity_in(self.unit, self.alloc);
            version.extend_from_slice(chunk);
            if durable {
                versions.clear();
            }
            versions.push(version);
        }
        Ok(())
    }

    /// Everything written before made durable.
    pub fn flush(&mut self) {
        for versions in self.units.iter_mut() {
            // Keep the newest version alone. Not `drain(..last)`: on the pinned nightly `Vec::drain`
            // asks more of the allocator than a handed `A` gives; the newest is moved to the front.
            let last = versions.len() - 1;
            versions.swap(0, last);
            versions.truncate(1);
        }
    }

    /// That `image` (the whole device, as a read gives it) is what a reader may see: after a
    /// crash (`crashed`) each unit holds one of its versions whole; with none, each holds its
    /// last. A crash's image becomes the durable state.
    pub fn seen(&mut self, image: &[u8], crashed: bool) -> Result<(), Seen> {
        self.seen_but(image, crashed, None)
    }

    /// As [`History::seen`], unit `unreadable` not judged (a latent error kept the reader from
    /// it; its bytes in `image` are anything): it keeps every version it had.
    pub fn seen_but(&mut self, image: &[u8], crashed: bool, unreadable: Option<u64>) -> Result<(), Seen> {
        for (n, (versions, got)) in self.units.iter_mut().zip(image.chunks(self.unit)).enumerate() {
            let unit = n as u64;
            if unreadable == Some(unit) {
                continue;
            }
            let last = &versions[versions.len() - 1];
            if !crashed {
                if last.as_slice() != got {
                    return Err(Seen::ReadDiffers { unit });
                }
                continue;
            }
            if versions.iter().any(|version| version.as_slice() == got) {
                let mut kept = Vec::with_capacity_in(self.unit, self.alloc);
                kept.extend_from_slice(got);
                versions.clear();
                versions.push(kept);
                continue;
            }
            if versions.len() == 1 {
                return Err(Seen::LostDurable { unit });
            }
            let half = self.unit / 2;
            let torn = half > 0
                && got.chunks(half).enumerate().all(|(h, part)| {
                    versions.iter().any(|version| &version[h * half..h * half + part.len()] == part)
                });
            return Err(match torn {
                true => Seen::TornBelowAtomic { unit },
                false => Seen::NotWritten { unit },
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! The device against its oracle: drawn shapes, a queue completed in a drawn order,
    //! flushes and crashes; and the control of [`BlockModelPlant::TearsBelowAtomic`].

    use std::alloc::System;

    use blaze_sim::{Broken, Config, Tape, check, controls};

    use super::*;

    blaze_sim::plants! {
        impl BlockModelPlant breaks Seen {
            None,
            TearsBelowAtomic => Seen::TornBelowAtomic { .. },
        }
    }

    /// One drawn programme: writes of whole units, some in flight together, flushes, crashes.
    fn programme(tape: &Tape, plant: BlockModelPlant) -> Result<(), Broken<Seen>> {
        let shape = Shape { bad: None, ..Shape::drawn(|n| tape.below(n.max(1), "shape"), 64) };
        let mut device = SimBlock::new(shape, tape.next_u64("device seed"), System, plant)
            .map_err(|e| Broken::Harness(e.to_string()))?;
        let unit = shape.unit();
        let units = shape.blocks / u64::from(shape.atomic);
        let mut history = History::new(shape.bytes(), unit, System);
        let mut busy = vec![false; units as usize];
        let mut stamp: u8 = 0;
        for _ in 0..tape.below(40, "steps") {
            match tape.below(6, "op") {
                0..=2 => {
                    let first = tape.below(units, "first unit");
                    let count = 1 + tape.below((units - first).min(4), "units");
                    let fua = tape.coin("fua");
                    // Bytes differing by byte, so a tear is told from a version.
                    let bytes: Vec<u8> = (0..count * unit)
                        .map(|_| {
                            stamp = stamp.wrapping_add(1);
                            stamp
                        })
                        .collect();
                    // In flight together up to the queue, completed in the device's order; two
                    // writes of one unit are never in flight together (their order is then the
                    // device's, and no reader may rely on either), nor a forced write with any.
                    let range = first..first + count;
                    if device.in_flight() as u32 >= shape.queue_depth || fua || range.clone().any(|u| busy[u as usize]) {
                        while device.complete_any().is_some() {}
                        busy.iter_mut().for_each(|b| *b = false);
                    }
                    range.for_each(|u| busy[u as usize] = true);
                    device.submit(first * unit, &bytes, fua).map_err(|e| Broken::Harness(e.to_string()))?;
                    // A write not yet acknowledged is no promise: told the oracle as not durable,
                    // and as durable only once it completes, below.
                    history.write(first * unit, &bytes, false).map_err(|e| Broken::Harness(format!("{e:?}")))?;
                    if tape.coin("wait for it") {
                        while device.complete_any().is_some() {}
                        busy.iter_mut().for_each(|b| *b = false);
                        if fua {
                            history.write(first * unit, &bytes, true).map_err(|e| Broken::Harness(format!("{e:?}")))?;
                        }
                    }
                }
                3 => {
                    while device.complete_any().is_some() {}
                    busy.iter_mut().for_each(|b| *b = false);
                    device.flush();
                    history.flush();
                }
                4 => {
                    while device.complete_any().is_some() {}
                    busy.iter_mut().for_each(|b| *b = false);
                    let mut image = vec![0u8; shape.bytes() as usize];
                    device.read(0, &mut image).map_err(|e| Broken::Harness(e.to_string()))?;
                    history.seen(&image, false).map_err(Broken::Oracle)?;
                }
                _ => {
                    device.crash();
                    busy.iter_mut().for_each(|b| *b = false);
                    let mut image = vec![0u8; shape.bytes() as usize];
                    device.read(0, &mut image).map_err(|e| Broken::Harness(e.to_string()))?;
                    history.seen(&image, true).map_err(Broken::Oracle)?;
                }
            }
        }
        Ok(())
    }

    blaze_sim::kind_test! {
        /// After any crash each atomic unit holds a version written to it whole, and a read with
        /// no crash between gives the last write, whatever the shape and completion order.
        simulation fn the_simulated_device_keeps_what_its_facts_promise() {
            check(Config::cases(400), |tape: &Tape| programme(tape, BlockModelPlant::None));
        }
    }

    blaze_sim::kind_test! {
        /// The oracle catches a device that tears below its atomic unit.
        simulation fn a_tear_below_the_atomic_unit_is_caught() {
            controls(Config::cases(400), programme);
        }
    }

    blaze_sim::kind_test! {
        /// A latent error fails the read that covers it, naming the block, and a write over it
        /// clears it.
        unit fn a_latent_sector_error_fails_its_read_until_written() {
            let shape = Shape {
                logical: 512,
                physical: 4096,
                atomic: 1,
                queue_depth: 4,
                volatile_cache: true,
                blocks: 8,
                bad: Some(5),
            };
            let mut device = SimBlock::new(shape, 7, System, BlockModelPlant::None).expect("a shape of its range");
            let mut into = [0u8; 1024];
            assert_eq!(device.read(4 * 512, &mut into), Err(SimBlockError::Unreadable { lba: 5 }));
            assert_eq!(device.read(0, &mut into), Ok(()));
            device.write(5 * 512, &[9u8; 512], false).expect("a write of one block");
            assert_eq!(device.read(4 * 512, &mut into), Ok(()));
            assert_eq!(&into[512..], &[9u8; 512]);
            assert_eq!(device.read(1, &mut into[..512]), Err(SimBlockError::Unaligned { at: 1, len: 512, logical: 512 }));
        }
    }
}
