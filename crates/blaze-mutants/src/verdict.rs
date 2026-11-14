//! Verdicts, and the record a final one is filed as.
//!
//! **The key** ([`MutantVerdict`], a core record kind): the function's body key, the site's
//! index in that body, the mutation's bytes, the operators' version, and the keys of every test
//! whose reach includes the function, sorted. All of it is known before anything runs. A test key
//! already covers every instance the test reaches by its instance key, the mutated function's
//! included; the body key makes the function explicit. So an unchanged function with unchanged
//! tests is never mutated again (decision 18, verify once): the key is equal, and an equal key is
//! the verdict with no other check. A changed test, or a new test reaching the function, moves
//! the key.
//!
//! **The value** ([`VerdictRecord`]): killed by which test and how, or survived with the tests
//! that ran and passed. Only those two are filed. Not reached and equivalent are decided
//! statically, for nothing, every time; not run is no verdict. A record's bytes are a function of
//! its key's inputs (rule 25: tests run in an order decided by the reach, not by recorded
//! times), with one exception said by [`Verdict::machine_dependent`]: a kill by timeout depends
//! on the machine's speed, so a shared cache's push refuses it.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::fmt;

use blaze_ids::ContentId;
use blaze_keyed::kind::{Grain, Keep, Key, KeyHasher, Kind, Layout, Tag, Trace};
use blaze_keyed::record::Opaque;
use blaze_program::key::BodyKey;
use blaze_program::record::TestKey;

use crate::Plant;
use crate::arid::EquivRule;

/// The version of what the operators do and of the record's layout: a change to either bumps it,
/// and every verdict is made again.
pub const MUTANTS_VERSION: u16 = 1;

/// The core record kind of a final verdict. Its tag is the next free one after
/// `blaze_keyed::kind::TAGS` (11 kinds); it is to be listed there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MutantVerdict;

/// What a verdict's key is made of.
pub struct VerdictInputs<'a> {
    pub body: BodyKey,
    pub site: u32,
    /// [`crate::operator::Mutation::encode`]'s bytes.
    pub mutation: &'a [u8],
    /// The keys of every test whose reach includes the function, sorted, each once.
    pub tests: &'a [TestKey],
    pub plant: Plant,
}

impl Kind for MutantVerdict {
    const TAG: Tag = Tag(12);
    const NAME: &'static str = "MutantVerdict";
    const DOMAIN: &'static str = "blaze-keyed MutantVerdict key";
    const VERSION: u16 = MUTANTS_VERSION;
    const LAYOUT: Layout = Layout::Flat;
    const GRAIN: Grain = Grain::Run;
    const TRACE: Trace = Trace::Key;
    const KEEP: Keep = Keep::Disk;
    type Root = Opaque;
    type Inputs<'a> = VerdictInputs<'a>;

    fn key(inputs: &VerdictInputs<'_>) -> Key<MutantVerdict> {
        let mut hasher = KeyHasher::of::<MutantVerdict>();
        hasher.id(&inputs.body.0).u64(u64::from(inputs.site)).bytes(inputs.mutation);
        let tests: &[TestKey] = match inputs.plant {
            Plant::VerdictKeyWithoutTests => &[],
            _ => inputs.tests,
        };
        hasher.u64(tests.len() as u64);
        for test in tests {
            hasher.id(&test.0);
        }
        hasher.finish()
    }
}

/// A verdict's key.
pub type VerdictKey = Key<MutantVerdict>;

/// How a test killed a mutant.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum KilledBy {
    /// It failed: an assertion, a panic, a nonzero exit.
    Failed,
    /// Its process died (a signal, an abort).
    Crashed,
    /// It ran past its timeout, derived from its recorded time (rule bounded-progress: a hang is a
    /// kill, never a wait).
    Timeout,
}

/// Why a mutant has no verdict from a run.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum NotRun {
    /// The budget did not reach it: never started.
    Budget,
    /// Started, and the session's deadline came first.
    Expired,
}

/// What became of a mutant.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Verdict {
    Killed { test: TestKey, how: KilledBy },
    /// Every reaching test ran and passed: the tests, by key, sorted.
    Survived { tests: Vec<TestKey, Global> },
    /// No test's reach includes the function: decided without a run.
    NotReached,
    Equivalent(EquivRule),
    NotRun(NotRun),
}

impl Verdict {
    /// Whether it is filed as a record: only what a run decided, completely.
    pub fn filed(&self) -> bool {
        matches!(self, Verdict::Killed { .. } | Verdict::Survived { .. })
    }

    /// Whether its bytes may differ between two machines for one key: a kill by timeout only.
    pub fn machine_dependent(&self) -> bool {
        matches!(self, Verdict::Killed { how: KilledBy::Timeout, .. })
    }
}

/// The payload of a [`MutantVerdict`] record (its `Opaque` root's payload).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VerdictRecord {
    pub verdict: Verdict,
}

/// Why bytes are not a verdict record: the byte offset and what was wrong there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodeError {
    pub at: usize,
    pub what: &'static str,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a {} record, at byte {}: {}", MutantVerdict::NAME, self.at, self.what)
    }
}

impl VerdictRecord {
    /// Its bytes: the verdict's tag, then its fields. Only a filed verdict has a record.
    pub fn encode(&self) -> Option<Vec<u8, Global>> {
        let mut out = Vec::new_in(Global);
        match &self.verdict {
            Verdict::Killed { test, how } => {
                out.push(1);
                out.extend_from_slice(&test.0.0);
                out.push(match how {
                    KilledBy::Failed => 1,
                    KilledBy::Crashed => 2,
                    KilledBy::Timeout => 3,
                });
            }
            Verdict::Survived { tests } => {
                out.push(2);
                out.extend_from_slice(&(tests.len() as u32).to_le_bytes());
                for test in tests {
                    out.extend_from_slice(&test.0.0);
                }
            }
            Verdict::NotReached | Verdict::Equivalent(_) | Verdict::NotRun(_) => return None,
        }
        Some(out)
    }

    /// The record `bytes` hold, exactly: bytes after it are refused.
    pub fn decode(bytes: &[u8]) -> Result<VerdictRecord, DecodeError> {
        let mut input = Reader { bytes, at: 0 };
        let verdict = match input.u8()? {
            1 => {
                let test = TestKey(input.id()?);
                let how = match input.u8()? {
                    1 => KilledBy::Failed,
                    2 => KilledBy::Crashed,
                    3 => KilledBy::Timeout,
                    _ => return Err(input.error("a kill of a kind no kill has")),
                };
                Verdict::Killed { test, how }
            }
            2 => {
                let count = input.u32()? as usize;
                if count > (bytes.len() - input.at) / 32 {
                    return Err(input.error("more tests than the bytes hold"));
                }
                let mut tests = Vec::with_capacity_in(count, Global);
                for _ in 0..count {
                    tests.push(TestKey(input.id()?));
                }
                if !tests.windows(2).all(|pair| pair[0] < pair[1]) {
                    return Err(input.error("a survivor's tests out of order or repeated"));
                }
                Verdict::Survived { tests }
            }
            _ => return Err(input.error("a verdict of a tag no filed verdict has")),
        };
        if input.at != bytes.len() {
            return Err(input.error("bytes after the record"));
        }
        Ok(VerdictRecord { verdict })
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn error(&self, what: &'static str) -> DecodeError {
        DecodeError { at: self.at, what }
    }

    fn take(&mut self, len: usize) -> Result<&[u8], DecodeError> {
        let end = self.at.checked_add(len).filter(|end| *end <= self.bytes.len());
        let Some(end) = end else { return Err(self.error("the record ends early")) };
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let raw = self.take(4)?;
        Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    fn id(&mut self) -> Result<ContentId, DecodeError> {
        let raw = self.take(32)?;
        let mut id = [0u8; 32];
        id.copy_from_slice(raw);
        Ok(ContentId(id))
    }
}
