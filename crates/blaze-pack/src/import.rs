//! A pack into a store.
//!
//! [`Import`] is the order of an import and its checks, as a state machine its driver steps:
//! for each record of the manifest, in order, ask the store whether it keeps the key
//! ([`Step::Look`]); one it keeps with the same content is left as it is, one it keeps with
//! other content refuses the import naming the key; one it lacks is read from the body
//! ([`Step::Read`]), checked ([`Import::read`]) and put. Then the pin ([`Step::Pin`]), then
//! one flush ([`Step::Flush`]), then done.
//!
//! **What it promises** (the invariants `src/tests.rs` checks under simulation):
//! - *Nothing unchecked is put.* The only way to a [`Verified`], which is all a
//!   [`Store::put`] takes, is [`Import::read`]: the length against the manifest's, then the
//!   bytes against the manifest's content hash. A record that fails either stops the import
//!   naming the record; nothing after it is read.
//! - *Kinds first.* A record of a kind the import was not told to admit refuses the whole
//!   pack as it is made ([`Import::new`]), before the first put.
//! - *Pinned before durable.* The pin of every key of the manifest is put after the last
//!   record and before the one flush, so the commit that makes the records durable makes
//!   their pin durable: no collection that can see the records can run without their pin.
//! - *An import can be done again.* It holds no state outside the store: after a crash, a
//!   truncated body or a refused record, the same import started over leaves what the store
//!   kept as it is and puts the rest.
//!
//! [`run`] is the driver's loop, over a [`Store`] and a [`Body`] it is given: the same loop
//! runs the real store (`blaze store import`) and the simulation.

use alloc::{
    format,
    string::{String, ToString as _},
    vec::Vec,
};

use crate::{Hashes, Id, Manifest, Record, Refused};

/// What the driver does next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// Ask the store for the content hash it keeps under record `index`'s key; then
    /// [`Import::looked`].
    Look { index: u32 },
    /// Read `len` bytes of the body from `at`; then [`Import::read`], put what it gives, then
    /// [`Import::put`].
    Read { index: u32, at: u64, len: u64 },
    /// Pin every key of the manifest under its pin; then [`Import::pinned`].
    Pin,
    /// Make what was put durable; then [`Import::flushed`].
    Flush,
    Done,
}

/// A record whose bytes were checked against its manifest: what a store is given to keep.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Verified<'a> {
    key: Id,
    kind: u8,
    content: Id,
    bytes: &'a [u8],
}

impl<'a> Verified<'a> {
    pub fn key(&self) -> Id {
        self.key
    }

    pub fn kind(&self) -> u8 {
        self.kind
    }

    pub fn content(&self) -> Id {
        self.content
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

/// What an import did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Summary {
    /// Records put.
    pub put: u32,
    /// Records the store kept already, left as they were.
    pub present: u32,
    /// Bytes of the records put.
    pub bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Look,
    Read,
    /// A record was checked and handed out; its put is awaited.
    Put,
    Pin,
    Flush,
    Done,
}

pub struct Import {
    manifest: Manifest,
    index: u32,
    phase: Phase,
    summary: Summary,
    /// Planted bugs, for the simulation's controls: records put unchecked; no pin.
    #[cfg(test)]
    pub(crate) plant_unchecked: bool,
    #[cfg(test)]
    pub(crate) plant_no_pin: bool,
}

impl Import {
    /// The import of `manifest`, admitting records of the kinds `admit` (the store's kind
    /// codes): a record of any other kind refuses the pack here.
    pub fn new(manifest: Manifest, admit: &[u8]) -> Result<Import, Refused> {
        if let Some(record) = manifest.records().iter().find(|it| !admit.contains(&it.kind)) {
            return Err(Refused::Kind { key: record.key, kind: record.kind });
        }
        let phase = match manifest.records().is_empty() {
            true => Phase::Pin,
            false => Phase::Look,
        };
        Ok(Import {
            manifest,
            index: 0,
            phase,
            summary: Summary::default(),
            #[cfg(test)]
            plant_unchecked: false,
            #[cfg(test)]
            plant_no_pin: false,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn summary(&self) -> Summary {
        self.summary
    }

    fn record(&self) -> Record {
        self.manifest.records()[self.index as usize]
    }

    pub fn step(&self) -> Step {
        match self.phase {
            Phase::Look => Step::Look { index: self.index },
            Phase::Read | Phase::Put => {
                Step::Read { index: self.index, at: self.manifest.at(self.index), len: self.record().len }
            }
            Phase::Pin => Step::Pin,
            Phase::Flush => Step::Flush,
            Phase::Done => Step::Done,
        }
    }

    fn advance(&mut self) {
        self.index += 1;
        self.phase = match (self.index as usize) < self.manifest.records().len() {
            true => Phase::Look,
            false => Phase::Pin,
        };
        #[cfg(test)]
        if self.phase == Phase::Pin && self.plant_no_pin {
            self.phase = Phase::Flush;
        }
    }

    /// The store's answer to [`Step::Look`]: the content hash it keeps under the key, or none.
    pub fn looked(&mut self, kept: Option<Id>) -> Result<(), Refused> {
        if self.phase != Phase::Look {
            return Err(Refused::Order { what: "`looked` where no look was asked" });
        }
        let record = self.record();
        match kept {
            None => self.phase = Phase::Read,
            Some(kept) if kept == record.content => {
                self.summary.present += 1;
                self.advance();
            }
            Some(kept) => {
                return Err(Refused::Differs { key: record.key, kept, given: record.content });
            }
        }
        Ok(())
    }

    /// The bytes read for [`Step::Read`], checked: the record to put, or why it is refused.
    /// Fewer bytes than asked is a body that ended ([`Refused::Truncated`]).
    pub fn read<'b>(&mut self, bytes: &'b [u8], hashes: &impl Hashes) -> Result<Verified<'b>, Refused> {
        if self.phase != Phase::Read {
            return Err(Refused::Order { what: "`read` where no read was asked" });
        }
        let record = self.record();
        let got = bytes.len() as u64;
        if got < record.len {
            return Err(Refused::Truncated {
                key: record.key,
                index: self.index,
                at: self.manifest.at(self.index),
                want: record.len,
                got,
            });
        }
        if got > record.len {
            return Err(Refused::Length { key: record.key, index: self.index, want: record.len, got });
        }
        #[cfg(test)]
        let unchecked = self.plant_unchecked;
        #[cfg(not(test))]
        let unchecked = false;
        if !unchecked {
            let content = hashes.content(bytes);
            if content != record.content {
                return Err(Refused::Record {
                    key: record.key,
                    index: self.index,
                    want: record.content,
                    got: content,
                });
            }
        }
        self.phase = Phase::Put;
        Ok(Verified { key: record.key, kind: record.kind, content: record.content, bytes })
    }

    /// The record [`Import::read`] gave was put.
    pub fn put(&mut self) -> Result<(), Refused> {
        if self.phase != Phase::Put {
            return Err(Refused::Order { what: "`put` where no record was handed out" });
        }
        self.summary.put += 1;
        self.summary.bytes += self.record().len;
        self.advance();
        Ok(())
    }

    pub fn pinned(&mut self) -> Result<(), Refused> {
        if self.phase != Phase::Pin {
            return Err(Refused::Order { what: "`pinned` where no pin was asked" });
        }
        self.phase = Phase::Flush;
        Ok(())
    }

    pub fn flushed(&mut self) -> Result<(), Refused> {
        if self.phase != Phase::Flush {
            return Err(Refused::Order { what: "`flushed` where no flush was asked" });
        }
        self.phase = Phase::Done;
        Ok(())
    }
}

/// The store an import puts into, as its driver reaches it.
pub trait Store {
    /// The content hash kept under `key`, or none. A store that cannot be read is the error.
    fn look(&mut self, key: &Id) -> Result<Option<Id>, String>;
    /// Keeps `record`. It may be read from now on, and is durable after the next
    /// [`Store::flush`].
    fn put(&mut self, record: Verified<'_>) -> Result<(), String>;
    /// Adds `keys` to the pin `name`, so the store's collector keeps them from now on;
    /// durable with the same flush as the records put before it.
    fn pin(&mut self, name: &str, keys: &[Id]) -> Result<(), String>;
    fn flush(&mut self) -> Result<(), String>;
}

/// A pack's body, as its driver reads it.
pub trait Body {
    /// Reads `len` bytes from `at` into `into`, emptied first: fewer when the body ends
    /// before them. A read that fails is the error.
    fn read(&mut self, at: u64, len: u64, into: &mut Vec<u8>) -> Result<(), String>;
}

fn driver(what: &str, key: Option<&Id>, why: String) -> Refused {
    Refused::Driver {
        // alloc: an error's text
        what: match key {
            Some(key) => format!("{what} (key {key}): {why}"),
            None => format!("{what}: {why}"),
        },
    }
}

/// Steps `import` to its end over `store` and `body`.
pub fn run(
    import: &mut Import,
    store: &mut impl Store,
    body: &mut impl Body,
    hashes: &impl Hashes,
) -> Result<Summary, Refused> {
    // alloc: one buffer an import, as long as its longest record
    let mut bytes = Vec::new();
    // Every step moves the import on or ends it, so this is at most three steps a record
    // and three more: the bound is stated, and a machine that stands still is an error.
    let most = 3 * import.manifest().records().len() as u64 + 3;
    for _ in 0..=most {
        match import.step() {
            Step::Look { index } => {
                let key = import.manifest().records()[index as usize].key;
                let kept = store
                    .look(&key)
                    .map_err(|why| driver("the store could not be asked for a record", Some(&key), why))?;
                import.looked(kept)?;
            }
            Step::Read { index, at, len } => {
                let key = import.manifest().records()[index as usize].key;
                body.read(at, len, &mut bytes)
                    .map_err(|why| driver("the pack's body could not be read", Some(&key), why))?;
                let record = import.read(&bytes, hashes)?;
                store
                    .put(record)
                    .map_err(|why| driver("the store did not keep a record", Some(&key), why))?;
                import.put()?;
            }
            Step::Pin => {
                // alloc: the pin's keys, once an import
                let keys: Vec<Id> = import.manifest().records().iter().map(|it| it.key).collect();
                let name = import.manifest().pin().to_string();
                store
                    .pin(&name, &keys)
                    .map_err(|why| driver("the store did not pin the pack's records", None, why))?;
                import.pinned()?;
            }
            Step::Flush => {
                store
                    .flush()
                    .map_err(|why| driver("the store did not make the import durable", None, why))?;
                import.flushed()?;
            }
            Step::Done => return Ok(import.summary()),
        }
    }
    Err(Refused::Order { what: "the import did not end within its steps" })
}
