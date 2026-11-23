//! [`Facts`], the one method a world answers, and [`Reads`], the sealed recorder that is the
//! only way to ask it, as blaze-typeck's `World` and `Reads` are for the checker (world.rs).
//!
//! `Facts::ask` takes a [`Seal`], which only this module makes: a world can write its answer
//! and cannot be asked except through `Reads`, so a fact the evaluator or the builder used is
//! in the read list or was never read. The read list is the input half of the key of what the
//! run makes (rule 21: a key covers every input).

use blaze_own::{Alloc, Vec};
use blaze_typeck::StableHasher;

use crate::answer::{Answer, Vocab};
use crate::digest::Digester;
use crate::fact::{Fact, FactKind};

/// The proof that a call of [`Facts::ask`] is made by [`Reads`]. Its field is private to this
/// module, so nothing else can make one.
pub struct Seal {
    _private: (),
}

/// Everything the evaluator and the builder may learn from the front end. Implemented by each
/// world (blaze-ladder's front ends, the crates' test worlds) as ONE exhaustive `match` over
/// [`Fact`]: a question it cannot answer is an [`Answer::Unknown`] it writes on purpose, a
/// record not made yet [`Answer::Missing`]. It must be a function of the records: two asks of
/// one question in one run give one answer (`Reads` refuses two).
pub trait Facts {
    type Vocab: Vocab;
    fn ask<'w>(&'w self, seal: Seal, fact: Fact<'_>) -> Answer<'w, Self::Vocab>;
}

/// A question as a read list names it: its kind and the digest of its arguments (ground
/// arguments by content). Sorted by kind, then digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FactKey {
    pub kind: FactKind,
    pub args: u64,
}

/// One row of a read list: a question and the digest of the answer it was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Read {
    pub fact: FactKey,
    pub answer: u64,
}

/// An ask that gave no answer to use, named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactError {
    /// The world does not know `fact`, and said why.
    Unknown { fact: FactKey, why: &'static str },
    /// The record that answers `fact` is not made yet: a hole, kept in [`Reads::holes`]; the
    /// run makes nothing and is asked again once the record exists.
    Missing { fact: FactKey },
    /// The world answered `asked` with an answer to `got`: the world's bug.
    WrongAnswer { asked: FactKind, got: FactKind },
    /// Two asks of `fact` in one run were given two answers: the world is not a function of
    /// its records.
    Unstable { fact: FactKey },
    /// A question's or an answer's terms break their table's form (a child after its
    /// parent, a handle past the table).
    Malformed { what: &'static str },
}

/// The recorder of one run: owns the world's borrow, the read list and the holes. The only
/// caller of [`Facts::ask`].
pub struct Reads<'w, 'a, F: Facts> {
    facts: &'w F,
    /// Sorted by question, one row each.
    rows: Vec<Read, Alloc<'a>>,
    /// Each question the world had no record for yet, sorted, one each.
    holes: Vec<FactKey, Alloc<'a>>,
    digester: Digester<'a>,
}

impl<'w, 'a, F: Facts> Reads<'w, 'a, F> {
    /// The recorder for one run over `facts`, its rows and memo in `alloc`.
    pub fn new_in(facts: &'w F, alloc: Alloc<'a>) -> Reads<'w, 'a, F> {
        Reads { facts, rows: Vec::new_in(alloc), holes: Vec::new_in(alloc), digester: Digester::new_in(alloc) }
    }

    /// Asks `fact`, records the question and the digest of its answer, and hands the answer
    /// on. An `Unknown` or `Missing` answer is an error naming the question and is not a read
    /// (a hole is kept in [`Reads::holes`]); a second ask of one question adds no row.
    pub fn ask(&mut self, fact: Fact<'_>) -> Result<Answer<'w, F::Vocab>, FactError> {
        let key = FactKey { kind: fact.kind(), args: self.digester.fact(&fact)? };
        let answer = self.facts.ask(Seal { _private: () }, fact);
        match &answer {
            Answer::Unknown(u) => return Err(FactError::Unknown { fact: key, why: u.why }),
            Answer::Missing => {
                if let Err(at) = self.holes.binary_search(&key) {
                    self.holes.insert(at, key);
                }
                return Err(FactError::Missing { fact: key });
            }
            _ => {}
        }
        match answer.kind() {
            Some(got) if got != key.kind => return Err(FactError::WrongAnswer { asked: key.kind, got }),
            _ => {}
        }
        let digest = self.digester.answer(&answer)?;
        match self.rows.binary_search_by(|r| r.fact.cmp(&key)) {
            Ok(at) => {
                if self.rows.get(at).is_some_and(|r| r.answer != digest) {
                    return Err(FactError::Unstable { fact: key });
                }
            }
            Err(at) => self.rows.insert(at, Read { fact: key, answer: digest }),
        }
        Ok(answer)
    }

    /// The reads so far, sorted by question.
    pub fn reads(&self) -> &[Read] {
        &self.rows
    }

    /// The holes so far, sorted: the run made nothing if any.
    pub fn holes(&self) -> &[FactKey] {
        &self.holes
    }

    /// The read list of the run.
    pub fn finish(self) -> ReadList<'a> {
        ReadList { rows: self.rows }
    }
}

/// What a run read: each question and the digest of its answer, sorted by question, one row
/// each. Its [`ReadList::digest`] is what a key holds of the facts the run used; a later run
/// is valid by it when each row's question gives today the digest it recorded.
pub struct ReadList<'a> {
    rows: Vec<Read, Alloc<'a>>,
}

impl ReadList<'_> {
    pub fn rows(&self) -> &[Read] {
        &self.rows
    }

    /// The digest recorded for `fact`, if it was read.
    pub fn get(&self, fact: FactKey) -> Option<u64> {
        self.rows.binary_search_by(|r| r.fact.cmp(&fact)).ok().and_then(|at| self.rows.get(at)).map(|r| r.answer)
    }

    /// The digest of the whole list: its rows in order. Changes when an answer read changes,
    /// and not when a fact no ask reached changes.
    pub fn digest(&self) -> u64 {
        let mut h = StableHasher::new("blaze-facts read list");
        h.u64(self.rows.len() as u64);
        for r in self.rows.iter() {
            h.tag(r.fact.kind as u8);
            h.u64(r.fact.args);
            h.u64(r.answer);
        }
        h.finish()
    }
}
