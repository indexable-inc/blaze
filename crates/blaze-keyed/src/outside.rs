//! The one form of every [`Trace::Outside`](crate::kind::Trace::Outside) kind: a record whose
//! validity is its key and the answers, now, of what its run asked outside (environment
//! variables, paths, a span's position, a request's files). A certificate, a script run, an
//! expansion and a test run are all this shape.
//!
//! An outside answer can change while the key stays equal, so one record per key cannot hold it
//! (a new answer would be other bytes under a filed key, [`Put::Other`], and an undone edit
//! could not find its old value). Two levels, every record immutable:
//!
//! - Shapes: at [`shape_slot`]`(key, i)`, for `i` from 0 up to [`MAX_SHAPES`], a record holding
//!   the bytes of one list of asks a run of the key made (the asks themselves, in the kind's
//!   own encoding: a fact id cannot be turned back into its question). Slots fill from 0; the
//!   first empty one ends the list.
//! - Values: at the address [`address`]`(key, shape, answers)`, the value, with the read list
//!   as facts for explain. A changed answer is a new address; an undone one finds its old value.
//!
//! [`Lookup`] finds a valid value (per slot: read the shape, the host answers its asks now, read
//! the address); a hit by address is a [`Holds`] like any other, made by the key check of
//! [`proof::hit`] on the address. [`settle`] checks that a run's answers did not move while it
//! ran; [`Filer`] files a settled run. All three are sans-IO: the caller reads, answers and puts.

#![blaze::sans_io]

use core::fmt;

use blaze_ids::{ContentId, Hasher};

use crate::{
    kind::{Key, Kind},
    proof::{self, Checks, Holds, Snapshot},
    record::{self, Fact, RecordError, Traced},
    store::Put,
};

/// The most shapes one key may have. A run reads the same things every time, whatever is in
/// them, so a key has one shape, or a few where a read decides what else is read; past this,
/// lookup would be a search.
///
/// At the cap a filing ends in [`OutsideError::TooManyShapes`], and what that costs is the
/// caller's: the request driver ([`crate::driver`]) has filed every record and mark by then, so
/// it ends [`Built`](crate::driver::Outcome::Built) with
/// [`Filed::Full`](crate::driver::Filed::Full): nothing certifies the request and the next one
/// settles each unit by its mark, a rebuild of nothing (before read-addressing it failed the
/// request after filing everything). Expansions and script runs (blaze-expansion) refuse.
pub const MAX_SHAPES: u32 = 64;

/// The key the `slot`th shape of `key` is filed under.
pub fn shape_slot<K: Kind>(key: Key<K>, slot: u32) -> Key<K> {
    let mut hasher = Hasher::new("blaze-keyed shape slot 1");
    hasher.update(K::NAME.as_bytes()).id(&key.id()).update(&slot.to_le_bytes());
    Key::from_id(hasher.finish())
}

/// Hashes an address as answers arrive, so a lookup allocates nothing.
pub struct AddressHasher {
    hasher: Hasher,
}

impl AddressHasher {
    pub fn new<K: Kind>(key: Key<K>, shape: &[u8]) -> AddressHasher {
        let mut hasher = Hasher::new("blaze-keyed address 1");
        hasher.update(K::NAME.as_bytes()).id(&key.id()).part(shape);
        AddressHasher { hasher }
    }

    pub fn answer(&mut self, answer: &ContentId) {
        self.hasher.id(answer);
    }

    pub fn finish<K: Kind>(&self) -> Key<K> {
        Key::from_id(self.hasher.finish())
    }
}

/// The address of the value of `key` whose run asked `shape` and got `answers`, in order.
pub fn address<K: Kind>(key: Key<K>, shape: &[u8], answers: &[ContentId]) -> Key<K> {
    let mut hasher = AddressHasher::new(key, shape);
    answers.iter().for_each(|answer| hasher.answer(answer));
    hasher.finish()
}

/// The shape record at `slot`.
pub fn write_shape<K: Kind<Root = Traced>>(slot: &Key<K>, shape: &[u8]) -> Vec<u8> {
    record::write_traced(slot, &[], &[], shape)
}

/// The value record at `address`, with its read list (`reads`, for explain) and `payload`.
pub fn write_value<K: Kind<Root = Traced>>(address: &Key<K>, reads: &[Fact], payload: &[u8]) -> Vec<u8> {
    record::write_traced(address, reads, &[], payload)
}

/// The shape bytes of the shape record `record` filed at `slot`.
pub fn shape_of<'b, K: Kind<Root = Traced>>(record: &'b [u8], slot: &Key<K>) -> Result<&'b [u8], RecordError> {
    record::open::<K>(record, slot)?.payload.follow(record).map_err(RecordError::Format)
}

/// Why an outside lookup or filing stopped, naming the key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OutsideError {
    /// A shape or value record that does not open under the key it is filed at.
    Record { key: ContentId, why: RecordError },
    /// Every one of the [`MAX_SHAPES`] slots of `key` holds another shape.
    TooManyShapes { key: ContentId },
    /// Other bytes at `address`: an equal key and equal answers gave another value, so the key
    /// misses an input (rule 21).
    TwoValues { key: ContentId, address: ContentId },
    /// The caller gave an answer the machine did not ask for.
    Misused(&'static str),
}

impl fmt::Display for OutsideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutsideError::Record { key, why } => write!(f, "the outside record at {key:?}: {why}"),
            OutsideError::TooManyShapes { key } => write!(
                f,
                "the key {key:?} has more than {MAX_SHAPES} read lists: what it reads depends on what it \
                 read past what a lookup can verify (rule bounded-progress)"
            ),
            OutsideError::TwoValues { key, address } => write!(
                f,
                "another value at address {address:?} of key {key:?} with the same answers: the key \
                 misses an input (rule keys-cover-every-input)"
            ),
            OutsideError::Misused(what) => write!(f, "the outside machine was misused: {what}"),
        }
    }
}

/// What a [`Lookup`] or a [`Filer`] wants next.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Want<K> {
    /// Read the record at this shape slot, and give it back.
    ReadShape(Key<K>),
    /// Read the record at this address, and give it back.
    ReadValue(Key<K>),
    /// The answer now of every ask of the shape just given, in its order ([`Lookup::answers`]).
    Answers,
    /// File the shape record ([`write_shape`]) at this slot, and give the put back.
    PutShape(Key<K>),
    /// File the value record at this address, and give the put back.
    PutValue(Key<K>),
    Done,
}

/// What a lookup found.
pub enum Found<'s, K> {
    /// The value at the address the answers now make: valid in this snapshot.
    Hit(Holds<'s, K>),
    Miss,
}

impl<K> Clone for Found<'_, K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for Found<'_, K> {}

#[derive(Clone, Copy)]
enum Stage {
    Shape(u32),
    Answers(u32),
    Value(u32),
    Done,
}

/// Looks for a valid value of `key`. Each shape's answers are asked once; each address is read
/// once; nothing is checked after a hit. On a miss it remembers each shape it read (by digest),
/// so filing the run ([`Lookup::filer`]) reads no slot again.
pub struct Lookup<'s, K: Kind> {
    snap: Snapshot<'s>,
    key: Key<K>,
    stage: Stage,
    address: Option<AddressHasher>,
    found: Option<Found<'s, K>>,
    /// The digests of the shapes at slots `0..filled`.
    seen: [ContentId; MAX_SHAPES as usize],
    filled: u32,
}

impl<'s, K: Kind<Root = Traced>> Lookup<'s, K> {
    pub fn new(snap: Snapshot<'s>, key: Key<K>) -> Lookup<'s, K> {
        Lookup {
            snap,
            key,
            stage: Stage::Shape(0),
            address: None,
            found: None,
            seen: [ContentId([0; 32]); MAX_SHAPES as usize],
            filled: 0,
        }
    }

    /// After a miss, the filing of a run of the same key in the same snapshot, from what the
    /// lookup read: its shape's slot if one holds it, else the first empty slot; no slot is read
    /// again.
    pub fn filer(&self, settled: Settled<K>, shape: &[u8]) -> Result<Filer<K>, OutsideError> {
        if !matches!(self.found, Some(Found::Miss)) || settled.key != self.key {
            return Err(OutsideError::Misused("a filing from a lookup that did not miss on its key"));
        }
        let digest = record::digest(shape);
        let stage = match self.seen[..self.filled as usize].iter().position(|seen| *seen == digest) {
            Some(_) => FileStage::PutValue,
            None if self.filled < MAX_SHAPES => FileStage::PutShape(self.filled),
            None => return Err(OutsideError::TooManyShapes { key: self.key.id() }),
        };
        Ok(Filer { settled, stage })
    }

    pub fn key(&self) -> Key<K> {
        self.key
    }

    pub fn want(&self) -> Want<K> {
        match self.stage {
            Stage::Shape(slot) => Want::ReadShape(shape_slot(self.key, slot)),
            Stage::Answers(_) => Want::Answers,
            Stage::Value(_) => match &self.address {
                Some(address) => Want::ReadValue(address.finish()),
                None => Want::Done,
            },
            Stage::Done => Want::Done,
        }
    }

    /// The record at the shape slot asked: its shape, handed back for the caller to answer, or
    /// `None` when the slot is empty (a miss).
    pub fn shape<'b>(&mut self, record: Option<&'b [u8]>, checks: &mut Checks) -> Result<Option<&'b [u8]>, OutsideError> {
        let Stage::Shape(slot) = self.stage else {
            return Err(OutsideError::Misused("a shape given to a lookup that asked none"));
        };
        checks.key_lookups += 1;
        let Some(record) = record else {
            self.end(Found::Miss);
            return Ok(None);
        };
        let at = shape_slot(self.key, slot);
        let shape = shape_of(record, &at).map_err(|why| OutsideError::Record { key: at.id(), why })?;
        self.seen[slot as usize] = record::digest(shape);
        self.filled = slot + 1;
        self.address = Some(AddressHasher::new(self.key, shape));
        self.stage = Stage::Answers(slot);
        Ok(Some(shape))
    }

    /// The answers now of the shape's asks, in order: the shape's one check.
    pub fn answers(&mut self, now: &[ContentId], checks: &mut Checks) -> Result<(), OutsideError> {
        let (Stage::Answers(slot), Some(address)) = (self.stage, self.address.as_mut()) else {
            return Err(OutsideError::Misused("answers given to a lookup that asked none"));
        };
        checks.read_lists += 1;
        checks.facts_compared += now.len() as u32;
        now.iter().for_each(|answer| address.answer(answer));
        self.stage = Stage::Value(slot);
        Ok(())
    }

    /// The record at the address the answers made, if any.
    pub fn value(&mut self, record: Option<&[u8]>, checks: &mut Checks) -> Result<(), OutsideError> {
        let (Stage::Value(slot), Some(address)) = (self.stage, self.address.as_ref()) else {
            return Err(OutsideError::Misused("a value given to a lookup that asked none"));
        };
        let address = address.finish::<K>();
        checks.key_lookups += 1;
        match record {
            Some(bytes) => {
                let hit = proof::hit(self.snap, address, bytes)
                    .map_err(|why| OutsideError::Record { key: address.id(), why })?;
                self.end(Found::Hit(hit.holds()));
            }
            None if slot + 1 == MAX_SHAPES => self.end(Found::Miss),
            None => {
                self.address = None;
                self.stage = Stage::Shape(slot + 1);
            }
        }
        Ok(())
    }

    fn end(&mut self, found: Found<'s, K>) {
        self.stage = Stage::Done;
        self.address = None;
        self.found = Some(found);
    }

    /// What it found, once [`Want::Done`].
    pub fn found(&self) -> Option<Found<'s, K>> {
        self.found
    }
}

/// A run whose answers held still: where it is filed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Settled<K> {
    pub key: Key<K>,
    pub address: Key<K>,
}

/// An answer that moved while the run ran: nothing is filed; the next request runs it against
/// what is there then (an edit is a new input; nothing is retried here).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Raced {
    pub at: u32,
}

/// A run of `key` that asked `shape` and got `then`, settled against the answers `now` after it
/// ended: equal, and it is filed at the address they make.
pub fn settle<K: Kind>(key: Key<K>, shape: &[u8], then: &[ContentId], now: &[ContentId], checks: &mut Checks) -> Result<Settled<K>, Raced> {
    checks.read_lists += 1;
    for at in 0..then.len().max(now.len()) {
        checks.facts_compared += 1;
        if then.get(at) != now.get(at) {
            return Err(Raced { at: at as u32 });
        }
    }
    Ok(Settled { key, address: address(key, shape, then) })
}

#[derive(Clone, Copy)]
enum FileStage {
    Read(u32),
    PutShape(u32),
    PutValue,
    Done,
}

/// Files a settled run: its shape at the first slot that holds it or is free (a slot another
/// process filed with another shape meanwhile is taken, and the next is tried; never the work
/// again), then its value at its address.
pub struct Filer<K> {
    settled: Settled<K>,
    stage: FileStage,
}

impl<K: Kind<Root = Traced>> Filer<K> {
    pub fn new(settled: Settled<K>) -> Filer<K> {
        Filer { settled, stage: FileStage::Read(0) }
    }

    pub fn want(&self) -> Want<K> {
        match self.stage {
            FileStage::Read(slot) => Want::ReadShape(shape_slot(self.settled.key, slot)),
            FileStage::PutShape(slot) => Want::PutShape(shape_slot(self.settled.key, slot)),
            FileStage::PutValue => Want::PutValue(self.settled.address),
            FileStage::Done => Want::Done,
        }
    }

    fn next_slot(&mut self, slot: u32) -> Result<(), OutsideError> {
        if slot + 1 == MAX_SHAPES {
            return Err(OutsideError::TooManyShapes { key: self.settled.key.id() });
        }
        self.stage = FileStage::Read(slot + 1);
        Ok(())
    }

    /// The record at the slot asked; `shape` is the run's.
    pub fn read(&mut self, record: Option<&[u8]>, shape: &[u8], checks: &mut Checks) -> Result<(), OutsideError> {
        let FileStage::Read(slot) = self.stage else {
            return Err(OutsideError::Misused("a read given to a filer that asked none"));
        };
        checks.key_lookups += 1;
        let at = shape_slot(self.settled.key, slot);
        match record {
            None => {
                self.stage = FileStage::PutShape(slot);
                Ok(())
            }
            Some(record) => {
                let filed = shape_of(record, &at).map_err(|why| OutsideError::Record { key: at.id(), why })?;
                if filed == shape {
                    self.stage = FileStage::PutValue;
                    Ok(())
                } else {
                    self.next_slot(slot)
                }
            }
        }
    }

    /// The put of the shape or the value asked.
    pub fn put(&mut self, put: Put) -> Result<(), OutsideError> {
        match (self.stage, put) {
            (FileStage::PutShape(_), Put::Written | Put::Same) => self.stage = FileStage::PutValue,
            (FileStage::PutShape(slot), Put::Other) => return self.next_slot(slot),
            (FileStage::PutValue, Put::Written | Put::Same) => self.stage = FileStage::Done,
            (FileStage::PutValue, Put::Other) => {
                return Err(OutsideError::TwoValues { key: self.settled.key.id(), address: self.settled.address.id() });
            }
            _ => return Err(OutsideError::Misused("a put given to a filer that asked none")),
        }
        Ok(())
    }
}
