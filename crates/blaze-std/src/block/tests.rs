//! The block conformance suite (D182): one drawn programme of block operations with crashes,
//! judged by blaze-core's oracle (`blaze_core::sim::block::History`), run against
//!
//! - the simulated device ([`OverSim`]), its facts drawn: volatile cache or none, queue depth,
//!   atomic unit, block sizes, a latent sector error;
//! - [`OverFile`] in a simulated world (`Io::sim`), whose disk tears every unsynced write by
//!   512-byte sectors at a crash;
//! - [`OverFile`] on a real temporary file (a `process` test): a crash is the device closed
//!   unflushed and opened again, which a reader sees through the page cache whole.
//!
//! What a reader sees after a reopen: each atomic unit one of the versions written to it
//! since its last flush (its last, when no crash came between), a unit whose last write was
//! acknowledged durable that write exactly; a buffer not whole blocks refused; a read of the
//! block with a latent error refused naming it, until a write over it.
//!
//! The planted controls: a durable write reported complete before its flush
//! ([`BlockPlant::DurableBeforeFlush`], [`Seen::LostDurable`]); alignment not enforced
//! ([`BlockPlant::AlignmentNotEnforced`], [`Conform::AlignmentAccepted`]); a tear smaller than
//! the atomic unit accepted by the model ([`BlockModelPlant::TearsBelowAtomic`],
//! [`Seen::TornBelowAtomic`]).

use std::{alloc::System, fmt, time::Duration};

use blaze_core::sim::block::{BlockModelPlant, History, Seen, Shape, SimBlock};
use blaze_core::FileName;
use blaze_sim::{Broken, Config, Tape, check, controls};

use super::{
    Aligned, AlignedMut, Block, BlockKind, BlockPlant, BlockSize, Blocks, Device, Durable, Lba, OverFile, OverSim,
};
use crate::{
    cx::Cx,
    fs::{Dir, ReadWrite},
    io::Io,
    path::Path,
};

/// What the suite's oracle saw broken.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Conform {
    /// The device's bytes against the history ([`History::seen`]).
    Seen(Seen),
    /// A buffer `off` bytes off a block in address and `short` in length was taken as whole
    /// blocks.
    AlignmentAccepted { off: usize, short: usize },
    /// A read covering block `lba`, which has a latent error, succeeded.
    LatentMissed { lba: u64 },
}

impl fmt::Display for Conform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Conform::Seen(seen) => write!(f, "{seen}"),
            Conform::AlignmentAccepted { off, short } => {
                write!(f, "a buffer {off} bytes off a block and {short} short of one was taken as whole blocks")
            }
            Conform::LatentMissed { lba } => write!(f, "a read covering block {lba}, unreadable, succeeded"),
        }
    }
}

const STEPS: u64 = 32;

#[derive(Clone, Copy, Debug)]
enum Op {
    /// Units `first..first + count` (each taken modulo the device's), written.
    Write { first: u64, count: u64, durable: Durable, atomic: bool },
    Flush,
    /// The whole device read, unit by unit.
    Read,
    Discard { first: u64, count: u64 },
    /// A buffer `off` bytes off a block in address and `short` bytes short in length.
    Probe { off: u64, short: u64 },
    Crash,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    shape: Shape,
    ops: Vec<Op>,
}

const DURABLE: [Durable; 3] = [Durable::No, Durable::Started, Durable::Yes];

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("seed");
    let shape = Shape::drawn(|n| tape.below(n.max(1), "shape"), 64);
    let ops = (0..tape.below(STEPS, "steps"))
        .map(|_| match tape.below(10, "op") {
            0..=3 => Op::Write {
                first: tape.next_u64("first"),
                count: tape.below(4, "count"),
                durable: *tape.one_of(&DURABLE, "durable"),
                atomic: tape.coin("atomic"),
            },
            4 => Op::Flush,
            5 | 6 => Op::Read,
            7 => Op::Discard { first: tape.next_u64("first"), count: tape.below(4, "count") },
            8 => Op::Probe { off: tape.next_u64("off"), short: tape.next_u64("short") },
            _ => Op::Crash,
        })
        .collect();
    Plan { seed, shape, ops }
}

/// A device the suite runs on, and how its power is lost.
trait Rig {
    type D: Device;
    fn block(&mut self) -> &mut Block<Self::D, BlockPlant>;
    /// The power lost, and the device opened again.
    fn crash(&mut self, cx: &Cx) -> Result<(), Broken<Conform>>;
    /// Whether discard is built on it.
    fn discards(&self) -> bool;
    /// The block with a latent error at the start.
    fn bad(&self) -> Option<u64>;
    /// The device finished.
    fn finish(self, cx: &Cx);
}

fn harness(what: &str, error: impl fmt::Display) -> Broken<Conform> {
    Broken::Harness(format!("{what}: {error}"))
}

/// `len` bytes of a buffer aligned to `size`, owned.
struct Room {
    raw: Vec<u8>,
    at: usize,
    len: usize,
}

impl Room {
    fn new(len: usize, size: BlockSize) -> Room {
        let n = size.get() as usize;
        let raw = vec![0u8; len + 2 * n];
        let addr = raw.as_ptr().addr();
        let at = addr.next_multiple_of(n) - addr;
        Room { raw, at, len }
    }

    fn bytes(&mut self) -> &mut [u8] {
        &mut self.raw[self.at..self.at + self.len]
    }
}

/// The whole device read unit by unit, and the unit a latent error kept unread.
fn image<R: Rig>(rig: &mut R, cx: &Cx, unit_blocks: u64, bad: Option<u64>) -> Result<(Vec<u8>, Option<u64>), Broken<Conform>> {
    let facts = rig.block().facts();
    let unit = (unit_blocks * facts.logical.bytes()) as usize;
    let units = facts.size.0 / unit_blocks;
    let mut image = Vec::with_capacity(units as usize * unit);
    let mut unread = None;
    for u in 0..units {
        let mut room = Room::new(unit, facts.logical);
        let into = AlignedMut::new(room.bytes(), facts.logical).map_err(|e| harness("a unit's room", e))?;
        let covers = bad.filter(|&lba| lba / unit_blocks == u);
        match (rig.block().read(cx, Lba(u * unit_blocks), into), covers) {
            (Ok(()), None) => image.extend_from_slice(room.bytes()),
            (Ok(()), Some(lba)) => return Err(Broken::Oracle(Conform::LatentMissed { lba })),
            (Err(error), Some(lba)) if matches!(error.kind, BlockKind::Unreadable { lba: Lba(got) } if got == lba) => {
                unread = Some(u);
                image.extend(core::iter::repeat_n(0, unit));
            }
            (Err(error), _) => return Err(harness("a unit's read", error)),
        }
    }
    Ok((image, unread))
}

/// The programme on `rig`, judged; `plant` is the grant's (the model's is the rig's own).
fn run<R: Rig>(plan: &Plan, mut rig: R, cx: &Cx, plant: BlockPlant) -> Result<(), Broken<Conform>> {
    let judged = programme(plan, &mut rig, cx, plant);
    rig.finish(cx);
    judged
}

fn programme<R: Rig>(plan: &Plan, rig: &mut R, cx: &Cx, plant: BlockPlant) -> Result<(), Broken<Conform>> {
    let facts = rig.block().facts();
    let logical = facts.logical;
    let unit_blocks = facts.atomic_write.0;
    let unit = unit_blocks * logical.bytes();
    let units = facts.size.0 / unit_blocks;
    let mut history = History::new(facts.bytes(), unit, System);
    let mut bad = rig.bad();
    let mut stamp: u8 = 0;
    let clears = |bad: &mut Option<u64>, first: u64, count: u64| {
        if bad.is_some_and(|lba| (first * unit_blocks..(first + count) * unit_blocks).contains(&lba)) {
            *bad = None;
        }
    };
    for op in plan.ops.iter().copied() {
        match op {
            Op::Write { first, count, durable, atomic } => {
                let first = first % units;
                let count = match atomic {
                    true => 1,
                    false => 1 + count % (units - first).min(4),
                };
                let mut room = Room::new((count * unit) as usize, logical);
                // Each byte its own stamp, so a tear is told from a version.
                for byte in room.bytes() {
                    stamp = stamp.wrapping_add(1) | 1;
                    *byte = stamp;
                }
                let from = Aligned::new(room.bytes(), logical).map_err(|e| harness("a write's room", e))?;
                let at = Lba(first * unit_blocks);
                let wrote = match atomic {
                    true => rig.block().write_atomic(cx, at, from, durable),
                    false => rig.block().write(cx, at, from, durable),
                };
                wrote.map_err(|e| harness("a write", e))?;
                history
                    .write(first * unit, room.bytes(), durable == Durable::Yes)
                    .map_err(|e| harness("the oracle's write", format!("{e:?}")))?;
                clears(&mut bad, first, count);
            }
            Op::Flush => {
                rig.block().flush(cx).map_err(|e| harness("a flush", e))?;
                history.flush();
            }
            Op::Read => {
                let (image, unread) = image(rig, cx, unit_blocks, bad)?;
                history.seen_but(&image, false, unread).map_err(|seen| Broken::Oracle(Conform::Seen(seen)))?;
            }
            Op::Discard { first, count } => {
                let first = first % units;
                let count = 1 + count % (units - first).min(4);
                let discarded = rig.block().discard(cx, Lba(first * unit_blocks), Blocks(count * unit_blocks));
                match (discarded, rig.discards()) {
                    (Ok(()), true) => {
                        let zeros = vec![0u8; (count * unit) as usize];
                        history.write(first * unit, &zeros, false).map_err(|e| harness("the oracle's discard", format!("{e:?}")))?;
                        clears(&mut bad, first, count);
                    }
                    (Err(error), false) if matches!(error.kind, BlockKind::Unbuilt { .. }) => {}
                    (Ok(()), false) => return Err(Broken::Harness("a discard not built succeeded".to_owned())),
                    (Err(error), _) => return Err(harness("a discard", error)),
                }
            }
            Op::Probe { off, short } => {
                let n = logical.get() as u64;
                let (off, short) = match (off % n, short % n) {
                    (0, 0) => (1, 0),
                    pair => pair,
                };
                let mut room = Room::new(2 * n as usize, logical);
                let bytes = room.bytes();
                let probe = &bytes[off as usize..off as usize + n as usize - short as usize];
                if Aligned::planted(probe, logical, plant).is_ok() {
                    return Err(Broken::Oracle(Conform::AlignmentAccepted { off: off as usize, short: short as usize }));
                }
            }
            Op::Crash => {
                rig.crash(cx)?;
                let (image, unread) = image(rig, cx, unit_blocks, bad)?;
                history.seen_but(&image, true, unread).map_err(|seen| Broken::Oracle(Conform::Seen(seen)))?;
            }
        }
    }
    Ok(())
}

/// The simulated device.
struct SimRig {
    block: Block<OverSim<System, BlockModelPlant>, BlockPlant>,
    bad: Option<u64>,
}

impl SimRig {
    fn new(plan: &Plan, model: BlockModelPlant, plant: BlockPlant) -> Result<SimRig, Broken<Conform>> {
        let device = SimBlock::new(plan.shape, plan.seed, System, model).map_err(|e| harness("the device", e))?;
        let name = FileName::new("sim-block").ok_or_else(|| Broken::Harness("a plain name".to_owned()))?;
        let device = OverSim::new(device, name).map_err(|e| harness("the device's facts", format!("{e:?}")))?;
        Ok(SimRig { block: Block::new(device, plant), bad: plan.shape.bad })
    }
}

impl Rig for SimRig {
    type D = OverSim<System, BlockModelPlant>;

    fn block(&mut self) -> &mut Block<Self::D, BlockPlant> {
        &mut self.block
    }

    fn crash(&mut self, _cx: &Cx) -> Result<(), Broken<Conform>> {
        self.block.device_mut().device().crash();
        Ok(())
    }

    fn discards(&self) -> bool {
        true
    }

    fn bad(&self) -> Option<u64> {
        self.bad
    }

    fn finish(self, _cx: &Cx) {}
}

/// [`OverFile`] in a world of files, simulated or real.
struct FileRig {
    io: Io,
    dir: Dir<ReadWrite>,
    block: Option<Block<OverFile, BlockPlant>>,
    plant: BlockPlant,
    /// Whether the world is simulated: a crash loses the power; on a real one it only closes.
    simulated: bool,
}

const DEVICE: &str = "device";
/// 64 blocks of 512: the size of the simulated devices' largest.
const FILE_BLOCKS: Blocks = Blocks(64);

impl FileRig {
    fn new(io: Io, cx: &Cx, plant: BlockPlant, simulated: bool) -> Result<FileRig, Broken<Conform>> {
        let fs = io.need_fs().map_err(|e| harness("the world's files", e))?;
        let at = Path::new("blocks").map_err(|e| harness("a plain path", e))?;
        let dir = fs.open_dir(cx, &at).map_err(|e| harness("the folder", e))?;
        let name = Path::new(DEVICE).map_err(|e| harness("a plain path", e))?;
        let device = OverFile::create(cx, &dir, &name, FILE_BLOCKS).map_err(|e| harness("the device's making", e))?;
        Ok(FileRig { io, dir, block: Some(Block::new(device, plant)), plant, simulated })
    }
}

impl Rig for FileRig {
    type D = OverFile;

    fn block(&mut self) -> &mut Block<OverFile, BlockPlant> {
        // Some between calls: `crash` puts the device back before it returns.
        self.block.as_mut().expect("the rig holds its device between calls")
    }

    fn crash(&mut self, cx: &Cx) -> Result<(), Broken<Conform>> {
        if self.simulated && !self.io.crash() {
            return Err(Broken::Harness("the world is not simulated".to_owned()));
        }
        if let Some(block) = self.block.take() {
            block.into_device().close_unsynced();
        }
        let name = Path::new(DEVICE).map_err(|e| harness("a plain path", e))?;
        let device = OverFile::open(cx, &self.dir, &name).map_err(|e| harness("the device's reopening", e))?;
        self.block = Some(Block::new(device, self.plant));
        Ok(())
    }

    fn discards(&self) -> bool {
        false
    }

    fn bad(&self) -> Option<u64> {
        None
    }

    fn finish(mut self, cx: &Cx) {
        if let Some(block) = self.block.take() {
            // A failed close leaves only a scratch file; the run's verdict is the programme's.
            let _closed = block.into_device().close(cx);
        }
    }
}

const BUDGET: Duration = Duration::from_secs(120);
const FRAME: usize = 1 << 24;

fn root(io: &Io) -> Result<Cx, Broken<Conform>> {
    io.cx(BUDGET, FRAME).map_err(|e| harness("the root frame", e))
}

fn on_sim(plan: &Plan, model: BlockModelPlant, plant: BlockPlant) -> Result<(), Broken<Conform>> {
    let io = Io::sim(plan.seed);
    let cx = root(&io)?;
    run(plan, SimRig::new(plan, model, plant)?, &cx, plant)
}

fn on_sim_file(plan: &Plan, plant: BlockPlant) -> Result<(), Broken<Conform>> {
    let io = Io::sim(plan.seed);
    let cx = root(&io)?;
    // clone: the world, a refcount.
    let rig = FileRig::new(io.clone(), &cx, plant, true)?;
    run(plan, rig, &cx, plant)
}

blaze_sim::plants! {
    /// The plants the suite controls on the simulated device.
    enum DevicePlant breaks Conform {
        None,
        DurableBeforeFlush => Conform::Seen(Seen::LostDurable { .. }),
        AlignmentNotEnforced => Conform::AlignmentAccepted { .. },
        TearsBelowAtomic => Conform::Seen(Seen::TornBelowAtomic { .. }),
    }
}

impl DevicePlant {
    fn split(self) -> (BlockModelPlant, BlockPlant) {
        match self {
            DevicePlant::None => (BlockModelPlant::None, BlockPlant::None),
            DevicePlant::DurableBeforeFlush => (BlockModelPlant::None, BlockPlant::DurableBeforeFlush),
            DevicePlant::AlignmentNotEnforced => (BlockModelPlant::None, BlockPlant::AlignmentNotEnforced),
            DevicePlant::TearsBelowAtomic => (BlockModelPlant::TearsBelowAtomic, BlockPlant::None),
        }
    }
}

blaze_sim::plants! {
    /// The plants the suite controls on a file in a simulated world (the model's tear is the
    /// simulated device's alone).
    enum FilePlant breaks Conform {
        None,
        DurableBeforeFlush => Conform::Seen(Seen::LostDurable { .. }),
        AlignmentNotEnforced => Conform::AlignmentAccepted { .. },
    }
}

impl FilePlant {
    fn grant(self) -> BlockPlant {
        match self {
            FilePlant::None => BlockPlant::None,
            FilePlant::DurableBeforeFlush => BlockPlant::DurableBeforeFlush,
            FilePlant::AlignmentNotEnforced => BlockPlant::AlignmentNotEnforced,
        }
    }
}

blaze_sim::kind_test! {
    /// The programme on the simulated device: what a reader sees is what its facts promise.
    simulation fn the_simulated_device_conforms() {
        check(Config::cases(300), |tape: &Tape| on_sim(&plan(tape), BlockModelPlant::None, BlockPlant::None));
    }
}

blaze_sim::kind_test! {
    /// Each plant on the simulated device caught by the invariant it names.
    simulation fn every_device_plant_is_caught() {
        controls(Config::cases(300), |tape: &Tape, plant: DevicePlant| {
            let (model, grant) = plant.split();
            on_sim(&plan(tape), model, grant)
        });
    }
}

blaze_sim::kind_test! {
    /// The programme on a file in a simulated world, crashes tearing by sectors.
    simulation fn a_file_device_conforms_through_crashes() {
        check(Config::cases(200), |tape: &Tape| on_sim_file(&plan(tape), BlockPlant::None));
    }
}

blaze_sim::kind_test! {
    /// Each grant plant on a file in a simulated world caught by the invariant it names.
    simulation fn every_file_plant_is_caught() {
        controls(Config::cases(200), |tape: &Tape, plant: FilePlant| on_sim_file(&plan(tape), plant.grant()));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
blaze_sim::kind_test! {
    /// The programme on a real temporary file: a reopen after a close unflushed sees what the
    /// oracle allows (on a real disk with the power kept, every write).
    process fn a_real_file_device_conforms() {
        let dir = blaze_core::temp::TempDir::for_test("bstd-block-");
        let io = Io::native(crate::io::NativeWorld {
            root: &dir,
            threads: 2,
            env: crate::env::Vars::given(Vec::new()),
            args: crate::args::ArgList::given(Vec::new()),
            host_name: alloc::sync::Arc::from(""),
            seed: Some(7),
            above: 0,
            limits: crate::io::Limits::USUAL,
        })
        .expect("a native world over a test's directory");
        check(Config::cases(16), |tape: &Tape| {
            let plan = plan(tape);
            let cx = root(&io)?;
            // clone: the world, a refcount.
            let rig = FileRig::new(io.clone(), &cx, BlockPlant::None, false)?;
            run(&plan, rig, &cx, BlockPlant::None)
        });
    }
}
