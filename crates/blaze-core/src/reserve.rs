//! Writing a large record once, in place, a sans-IO core over [`Effect::MapWrite`]: the record
//! is written straight into the file that will hold it, never whole in memory
//! (docs/v2/plan/design/fetch.md, "Writing a record in place").
//!
//! - [`Reserve::new`] asks for a scratch file of `room` bytes mapped read-write, named
//!   `<key>.part-<tag>`: beside the key, in its directory, by construction;
//! - [`Reserve::finish`] takes the mapping back (dropping it unmaps), then asks a cut to the
//!   written length, a sync of the file, and a synced [`Effect::Rename`] to the key, which
//!   syncs the one directory both names are in: the two syncs a `Put` makes. A scratch file in
//!   another directory (a `tmp/`) made the rename sync both directories, a third sync that
//!   cost a reservation 1.45 times a `Put` of the same bytes (`examples/reserve_cost.rs`,
//!   2026-11-16, 4 KiB to 2 MiB);
//! - [`Reserve::abandon`] drops the mapping and removes the scratch file: nothing is visible
//!   under the key, and the space is returned.
//!
//! The promise, proved by the crash simulation below at every point: after a crash the key
//! names either nothing or the whole record as written. The scratch file's making is not
//! durable until its directory is synced, so after a crash one may be there or not: a sweep
//! removes every `.part-` name it finds and relies on none being there.
//!
//! One effect is in flight at a time, all under the one tag the caller gives.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::fmt;

use blaze_own::Share;

use crate::{Done, Effect, FileName, IoError, MappingWrite, Tag};

/// What a reservation's scratch name adds to its key's: `<key>.part-<tag>`.
pub const PART: &str = ".part-";

/// One reservation's protocol. The caller submits each [`Effect`] it hands back and gives the
/// completion to [`Reserve::answer`].
#[derive(Debug)]
pub struct Reserve {
    tmp: FileName,
    key: FileName,
    room: u64,
    tag: Tag,
    step: Step,
    #[cfg(test)]
    plant: Option<Plant>,
}

/// Where the protocol is: what was last asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// The scratch file asked for and mapped ([`Effect::MapWrite`]).
    Mapping,
    /// The mapping is the caller's, to write and then [`Reserve::finish`] or
    /// [`Reserve::abandon`].
    Mapped,
    /// Cut to the written length ([`Effect::Truncate`]).
    Cutting,
    /// The bytes synced ([`Effect::Sync`]).
    Syncing,
    /// Renamed to the key, its directory synced ([`Effect::Rename`]).
    Renaming,
    /// The record is under its key, durable.
    Stored,
    /// The scratch file removed ([`Effect::Remove`]).
    Removing,
    /// Nothing is under the key, and the scratch file is gone.
    Abandoned,
}

/// What the caller does next.
#[derive(Debug)]
pub enum Next {
    /// Submit this effect, then answer it.
    Ask(Effect),
    /// The file's bytes: write the record into them, then finish or abandon.
    Write(MappingWrite),
    /// The record is under its key, durable.
    Stored,
    /// The reservation is undone.
    Abandoned,
}

/// Why a reservation failed: the file, the step, and what went wrong. After any of these the
/// caller asks [`Reserve::abandon`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ReserveError {
    pub file: FileName,
    pub step: Step,
    pub why: Why,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Why {
    /// The driver failed the effect.
    Failed(IoError),
    /// The driver answered with something that is not this step's completion.
    Answer(&'static str),
    /// The record is longer than the room reserved.
    TooLong { len: u64, room: u64 },
    /// Asked out of order: `finish` before the mapping came, an answer with nothing asked.
    NotNow,
}

impl fmt::Display for ReserveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { file, step, why } = self;
        match why {
            Why::Failed(error) => write!(f, "{file}: reserving a record, at {step:?}: {error}"),
            Why::Answer(got) => {
                write!(f, "{file}: reserving a record, at {step:?}: answered {got}")
            }
            Why::TooLong { len, room } => write!(
                f,
                "{file}: a record of {len} bytes in a reservation of {room}: a record is reserved \
                 at its bound before it is written"
            ),
            Why::NotNow => write!(f, "{file}: reserving a record: asked out of order at {step:?}"),
        }
    }
}

impl std::error::Error for ReserveError {}

/// The planted bug the crash simulation must catch: the rename asked before the bytes are
/// synced, so a crash after it can leave a torn record under the key.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    RenameBeforeSync,
}

impl Reserve {
    /// A reservation of `room` bytes to become `key`, under `tag` (unique among the caller's
    /// reservations in flight, as it names the scratch file): the machine, and the effect to
    /// submit first.
    pub fn new(key: FileName, room: u64, tag: Tag) -> (Reserve, Effect) {
        // alloc: the scratch file's name, once per reservation.
        let tmp = FileName::new(&format!("{key}{PART}{}", tag.0)).expect("a key's name with a suffix is a name");
        // clone: a refcount, the name the effect carries.
        let ask = Effect::MapWrite { file: tmp.share(), len: room, tag };
        let reserve = Reserve {
            tmp,
            key,
            room,
            tag,
            step: Step::Mapping,
            #[cfg(test)]
            plant: None,
        };
        (reserve, ask)
    }

    #[cfg(test)]
    pub(crate) fn plant(&mut self, plant: Plant) {
        self.plant = Some(plant);
    }

    pub fn step(&self) -> Step {
        self.step
    }

    pub fn room(&self) -> u64 {
        self.room
    }

    fn error(&self, why: Why) -> ReserveError {
        // clone: a refcount, the name the error carries.
        ReserveError { file: self.tmp.share(), step: self.step, why }
    }

    /// The completion of the effect last asked.
    pub fn answer(&mut self, done: Done) -> Result<Next, ReserveError> {
        if let Done::Failed(error) = done {
            return Err(self.error(Why::Failed(error)));
        }
        let planted = self.planted();
        match (self.step, done) {
            (Step::Mapping, Done::MappedWrite(mapping)) => {
                if mapping.len() as u64 != self.room {
                    return Err(self.error(Why::Answer("a mapping of another length")));
                }
                self.step = Step::Mapped;
                Ok(Next::Write(mapping))
            }
            // The plant: cut, renamed (synced) to the key, and only then the bytes synced.
            (Step::Cutting, Done::Truncated) if planted => {
                self.step = Step::Renaming;
                // clone: refcounts, the names the effect carries.
                Ok(Next::Ask(Effect::Rename { from: self.tmp.share(), to: self.key.share(), tag: self.tag }))
            }
            (Step::Renaming, Done::Renamed) if planted => {
                self.step = Step::Syncing;
                // clone: a refcount, the name the effect carries.
                Ok(Next::Ask(Effect::Sync { file: self.key.share(), tag: self.tag }))
            }
            (Step::Syncing, Done::Synced) if planted => {
                self.step = Step::Stored;
                Ok(Next::Stored)
            }
            (Step::Cutting, Done::Truncated) => {
                self.step = Step::Syncing;
                // clone: a refcount, the name the effect carries.
                Ok(Next::Ask(Effect::Sync { file: self.tmp.share(), tag: self.tag }))
            }
            (Step::Syncing, Done::Synced) => {
                self.step = Step::Renaming;
                // clone: refcounts, the names the effect carries.
                Ok(Next::Ask(Effect::Rename { from: self.tmp.share(), to: self.key.share(), tag: self.tag }))
            }
            (Step::Renaming, Done::Renamed) => {
                self.step = Step::Stored;
                Ok(Next::Stored)
            }
            (Step::Removing, Done::Removed) => {
                self.step = Step::Abandoned;
                Ok(Next::Abandoned)
            }
            (Step::Mapped | Step::Stored | Step::Abandoned, _) => Err(self.error(Why::NotNow)),
            (Step::Mapping | Step::Cutting | Step::Syncing | Step::Renaming | Step::Removing, _) => {
                Err(self.error(Why::Answer("another effect's completion")))
            }
        }
    }

    #[cfg(test)]
    fn planted(&self) -> bool {
        self.plant == Some(Plant::RenameBeforeSync)
    }

    #[cfg(not(test))]
    fn planted(&self) -> bool {
        false
    }

    /// The record is the first `len` bytes of `mapping`: it is dropped (unmapped, its bytes in
    /// the page cache), and the effect that begins making them the key's is handed back.
    pub fn finish(&mut self, mapping: MappingWrite, len: u64) -> Result<Effect, ReserveError> {
        if self.step != Step::Mapped {
            return Err(self.error(Why::NotNow));
        }
        if len > self.room {
            return Err(self.error(Why::TooLong { len, room: self.room }));
        }
        drop(mapping);
        self.step = Step::Cutting;
        // clone: a refcount, the name the effect carries.
        Ok(Effect::Truncate { file: self.tmp.share(), len, tag: self.tag })
    }

    /// Undo the reservation, at any step before the record is stored: the mapping (if the
    /// caller holds it) is dropped and the scratch file removed. `None` once stored or
    /// abandoned, or while an effect is in flight (answer it first).
    pub fn abandon(&mut self, mapping: Option<MappingWrite>) -> Option<Effect> {
        drop(mapping);
        match self.step {
            Step::Stored | Step::Abandoned | Step::Removing => None,
            // A failed rename leaves the scratch name as it was: that is the one to remove.
            Step::Mapping | Step::Mapped | Step::Cutting | Step::Syncing | Step::Renaming => {
                self.step = Step::Removing;
                // clone: a refcount, the name the effect carries.
                Some(Effect::Remove { file: self.tmp.share(), tag: self.tag })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Completion, run_until_idle,
        sim::{Faults, Sim},
    };

    fn name(it: &str) -> FileName {
        FileName::new(it).expect("a test's name")
    }

    /// One effect run to its completion in `driver`.
    fn ask(driver: &mut impl crate::Driver, effect: Effect) -> Done {
        let mut effects = vec![effect];
        let mut got = None;
        run_until_idle(driver, &mut effects, |Completion { done, .. }, _| got = Some(done))
            .expect("the effect ends");
        got.expect("one completion")
    }

    /// The whole protocol on this machine's driver: the record is under its key, the
    /// scratch file is gone, and the bytes read back are those written.
    #[cfg(unix)]
    #[test]
    fn a_record_written_in_place_is_stored_on_the_native_driver() {
        let root = crate::temp::TempDir::for_test("blaze-core-reserve-");
        std::fs::create_dir_all(root.path().join("records")).expect("records/");
        let mut driver = crate::native(root.path(), 2).expect("the native driver");
        let record: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let (mut reserve, first) = Reserve::new(name("records/k9"), 1 << 20, Tag(3));
        let mut next = ask(&mut driver, first);
        loop {
            next = match reserve.answer(next) {
                Ok(Next::Write(mut mapping)) => {
                    mapping.bytes_mut()[..record.len()].copy_from_slice(&record);
                    let effect = reserve.finish(mapping, record.len() as u64).expect("finish");
                    ask(&mut driver, effect)
                }
                Ok(Next::Ask(effect)) => ask(&mut driver, effect),
                Ok(Next::Stored) => break,
                Ok(Next::Abandoned) => panic!("abandoned"),
                Err(error) => panic!("{error}"),
            };
        }
        assert!(!root.path().join("records/k9.part-3").exists(), "the scratch file is left");
        let stored = std::fs::read(root.path().join("records/k9")).expect("the record");
        assert!(stored == record, "{} bytes stored, {} written", stored.len(), record.len());
    }

    /// How one run went: whether the protocol reached `Stored`.
    struct Ran {
        stored: bool,
        record: Vec<u8>,
        left: Option<Vec<u8>>,
    }

    /// A reservation of a drawn room, written with a drawn record, crashed after a drawn
    /// number of effects (or never), the faults of `seed` failing effects at random; then what
    /// the key holds.
    fn run(seed: u64, plant: Option<Plant>) -> Ran {
        let fail_one_in = if seed % 3 == 0 { 0 } else { 6 };
        let mut sim = Sim::new(seed, Faults { fail_one_in, ..Faults::default() });
        let key = name("records/k");
        // Rooms around a sector and a page, and up to a few of them.
        let room = 1 + sim.rng().below(40_000);
        let len = sim.rng().below(room + 1);
        let fill = sim.rng().below(255) as u8 + 1;
        let record: Vec<u8> = (0..len).map(|i| fill.wrapping_add(i as u8)).collect();
        let crash_after = sim.rng().below(8);
        let (mut reserve, first) = Reserve::new(key.share(), room, Tag(7));
        if let Some(plant) = plant {
            reserve.plant(plant);
        }
        let mut asked = 0;
        let mut next = Some(first);
        let mut stored = false;
        while let Some(effect) = next.take() {
            if asked == crash_after {
                break;
            }
            asked += 1;
            let done = ask(&mut sim, effect);
            next = match reserve.answer(done) {
                Ok(Next::Ask(effect)) => Some(effect),
                Ok(Next::Write(mut mapping)) => {
                    mapping.bytes_mut()[..len as usize].copy_from_slice(&record);
                    match reserve.finish(mapping, len) {
                        Ok(effect) => Some(effect),
                        Err(error) => panic!("{error}"),
                    }
                }
                Ok(Next::Stored) => {
                    stored = true;
                    None
                }
                Ok(Next::Abandoned) => None,
                Err(_) => reserve.abandon(None),
            };
        }
        sim.crash();
        let left = sim.exists(&key).then(|| sim.contents(&key));
        Ran { stored, record, left }
    }

    #[test]
    fn a_crash_at_any_point_leaves_no_record_or_the_whole_one() {
        let (mut stored, mut whole_after_crash) = (0, 0);
        for seed in 0..2000 {
            let Ran { stored: done, record, left } = run(seed, None);
            stored += u32::from(done);
            match left {
                None => assert!(!done, "seed {seed}: stored, and gone after the crash"),
                Some(left) => {
                    assert!(left == record, "seed {seed}: the key holds {} bytes, not the {} written", left.len(), record.len());
                    whole_after_crash += 1;
                }
            }
        }
        // The cases ran: some reservations were stored, some crashed before.
        assert!(stored > 100 && whole_after_crash > 100, "stored {stored}, whole {whole_after_crash}");
    }

    #[test]
    fn the_planted_rename_before_sync_is_caught() {
        let torn = (0..2000)
            .filter(|&seed| {
                let Ran { record, left, .. } = run(seed, Some(Plant::RenameBeforeSync));
                left.is_some_and(|left| left != record)
            })
            .count();
        assert!(torn > 0, "no crash tore the record: the simulation does not see the plant");
    }

    #[test]
    fn a_record_longer_than_its_room_is_refused() {
        let mut sim = Sim::new(1, Faults::default());
        let (mut reserve, first) = Reserve::new(name("records/k2"), 10, Tag(1));
        let done = ask(&mut sim, first);
        let Ok(Next::Write(mapping)) = reserve.answer(done) else { panic!("no mapping") };
        let refused = reserve.finish(mapping, 11).expect_err("too long");
        assert_eq!(refused.why, Why::TooLong { len: 11, room: 10 });
    }

    #[test]
    fn an_abandoned_reservation_leaves_nothing() {
        let mut sim = Sim::new(2, Faults::default());
        let tmp = name("records/k3.part-1");
        let (mut reserve, first) = Reserve::new(name("records/k3"), 4096, Tag(1));
        let done = ask(&mut sim, first);
        let Ok(Next::Write(mut mapping)) = reserve.answer(done) else { panic!("no mapping") };
        mapping.bytes_mut().fill(9);
        let remove = reserve.abandon(Some(mapping)).expect("a remove");
        let done = ask(&mut sim, remove);
        assert!(matches!(reserve.answer(done), Ok(Next::Abandoned)));
        assert!(!sim.exists(&tmp) && !sim.exists(&name("records/k3")));
    }
}
