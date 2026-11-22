//! The questions a certificate's choice rested on (certificates.md, "The questions"): each a
//! solver question with the digest of the answer the solve saw. They are the negative half of
//! the claim the checker's module doc states: the derivation proves its steps apply; the
//! questions say nothing else could have been chosen (the impl buckets that were searched, the
//! lang items, the specialization answers, the trait flags that decided candidate assembly).
//!
//! Content the check makes again is left out: an impl header or an ADT the checker itself
//! read is proven by the check, so an edit to a where-clause or a field type costs one linear
//! check, not a solve. Everything else of the solve's reads is kept, sorted and deduplicated.

use blaze_int::idx;
use blaze_own::{Alloc, Vec};
use blaze_solve::error::Stop;
use blaze_solve::facts::{Ask, Facts, Question, Read};

use crate::check::Checked;

/// The cap on one certificate's questions (rule 18).
pub const MAX_QUESTIONS: u32 = 1 << 16;

/// Why a question list cannot be made or asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandError {
    /// More questions than [`MAX_QUESTIONS`].
    TooMany { n: usize, cap: u32 },
    /// One question recorded with two digests: the solve read a world that moved under it.
    Conflict { question: Question, a: u64, b: u64 },
    /// A `ConstHolds` question names terms of the solve's arena, not portable keys: it cannot be
    /// asked of a world alone (certificates.md, "What is out").
    ArenaQuestion { question: Question },
    /// The world has no answer today (a definition removed): the certificate cannot stand, and
    /// the caller solves again.
    Need { question: Question },
    /// The world's own failure while answering.
    Solve { question: Question, error: blaze_solve::SolveError },
}

impl core::fmt::Display for StandError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "blaze-cert: the questions cannot be asked: {self:?}")
    }
}

impl core::error::Error for StandError {}

/// A certificate's questions with their digests, sorted by question, each once.
#[derive(Debug)]
pub struct Questions<'a> {
    reads: Vec<Read, Alloc<'a>>,
}

impl<'a> Questions<'a> {
    /// The questions of a solve whose response `checked` certifies: the solve's reads minus the
    /// impl headers and ADTs the check read itself.
    pub fn from_solve(solve_reads: &[Read], checked: &Checked<'_, '_>, alloc: Alloc<'a>) -> Result<Questions<'a>, StandError> {
        let mut made: Vec<Question, Alloc<'a>> = Vec::new_in(alloc);
        for r in checked.reads() {
            if matches!(r.question, Question::ImplHeader(_) | Question::Adt(_)) {
                made.push(r.question);
            }
        }
        made.sort_unstable();
        made.dedup();
        let mut reads: Vec<Read, Alloc<'a>> = Vec::new_in(alloc);
        for r in solve_reads {
            if made.binary_search(&r.question).is_err() {
                reads.push(*r);
            }
        }
        Questions::from_reads(reads)
    }

    /// The list from recorded reads as they are (a fetched certificate's list, a test's).
    pub fn from_reads(mut reads: Vec<Read, Alloc<'a>>) -> Result<Questions<'a>, StandError> {
        reads.sort_unstable();
        reads.dedup();
        if reads.len() > idx(MAX_QUESTIONS) {
            return Err(StandError::TooMany { n: reads.len(), cap: MAX_QUESTIONS });
        }
        for w in reads.windows(2) {
            if w[0].question == w[1].question {
                return Err(StandError::Conflict { question: w[0].question, a: w[0].digest, b: w[1].digest });
            }
        }
        Ok(Questions { reads })
    }

    pub fn reads(&self) -> &[Read] {
        &self.reads
    }

    /// The first question (in sorted order) whose answer today has another digest, or `None`
    /// when every one stands. One digest compare per question; no term is walked.
    pub fn moved<F: Facts>(&self, facts: &F) -> Result<Option<Question>, StandError> {
        let ask = Ask::new(facts);
        // One answer per question, so a scratch of one: the ask notes it, we read it back.
        let mut buf = [Read { question: Question::IsLocal(blaze_solve::Def(0)), digest: 0 }];
        for r in self.reads.iter() {
            let mut log = One { got: &mut buf, set: false };
            let q = r.question;
            let asked = match q {
                Question::TraitDef(d) => ask.trait_def(&mut log, d).map(drop),
                Question::Impls(t, shape) => ask.impls(&mut log, t, shape).map(drop),
                Question::ImplHeader(d) => ask.impl_header(&mut log, d).map(drop),
                Question::Adt(d) => ask.adt(&mut log, d).map(drop),
                Question::FnSig(d) => ask.fn_sig(&mut log, d).map(drop),
                Question::Assoc(d) => ask.assoc(&mut log, d).map(drop),
                Question::Opaque(d) => ask.opaque(&mut log, d).map(drop),
                Question::OpaqueHidden(d) => ask.opaque_hidden(&mut log, d).map(drop),
                Question::Lang(l) => ask.lang(&mut log, l).map(drop),
                Question::IsLocal(d) => ask.is_local(&mut log, d).map(drop),
                Question::Specializes(a, b) => ask.specializes(&mut log, a, b).map(drop),
                Question::ConstHolds(..) => return Err(StandError::ArenaQuestion { question: q }),
            };
            match asked {
                Ok(()) => {}
                Err(Stop::Need(question)) => return Err(StandError::Need { question }),
                Err(Stop::Error(error)) => return Err(StandError::Solve { question: q, error }),
            }
            let set = log.set;
            if !set || buf[0].question != q || buf[0].digest != r.digest {
                return Ok(Some(q));
            }
        }
        Ok(None)
    }

    /// Whether every question answers as recorded.
    pub fn stands<F: Facts>(&self, facts: &F) -> Result<bool, StandError> {
        Ok(self.moved(facts)?.is_none())
    }
}

/// A log that holds the one read an ask notes.
struct One<'b> {
    got: &'b mut [Read; 1],
    set: bool,
}

impl blaze_solve::facts::Log for One<'_> {
    fn note(&mut self, r: Read) -> Result<(), blaze_solve::SolveError> {
        self.got[0] = r;
        self.set = true;
        Ok(())
    }
}
