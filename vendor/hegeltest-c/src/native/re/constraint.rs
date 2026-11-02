//! A regex as the constraint on a string choice (see
//! [`StringChoice`](crate::native::core::StringChoice)): a string drawn
//! from a pattern (`from_regex`) is recorded as one choice whose kind
//! carries the pattern, and a replay checks the stored string against it
//! with the [`Matcher`] rather than re-running the draw. The constraint
//! compares and hashes by an identity hashed from the pattern, its flags,
//! whether it must match the whole string and the alphabet it was drawn
//! over, so that two runs compiling the same pattern afresh agree on the
//! kind.

use super::constants::{MAXREPEAT, SRE_FLAG_DOTALL};
use super::matcher::Matcher;
use super::parser::{OpCode, ParsedPattern, SubPattern};
use super::vm::Program;
use crate::native::core::Drawn;
use crate::native::intervalsets::IntervalSet;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;
use core::hash::{Hash, Hasher};
use once_cell::race::OnceBox;
use rustc_hash::FxHasher;

pub struct RegexConstraint {
    parsed: ParsedPattern,
    fullmatch: bool,
    /// The pattern as a [`Program`] compiled for recognition alone (see
    /// [`Program::compile_recognizer`]), which checks a string without
    /// the matcher's step and depth limits, when the program can express
    /// it: the pattern itself under `fullmatch`, and otherwise the
    /// pattern after a lazy run of any characters, which matches at the
    /// start exactly when the pattern matches somewhere.
    program: Option<Program>,
    identity: u64,
    /// The simplest string of the pattern, set once by the generator at
    /// the first draw (see [`simplest_or_init`](Self::simplest_or_init)).
    simplest: OnceBox<Vec<u32>>,
    drawn: Drawn,
}

impl RegexConstraint {
    pub fn new(
        pattern: &str,
        parsed: ParsedPattern,
        fullmatch: bool,
        alphabet: Option<&IntervalSet>,
    ) -> RegexConstraint {
        let mut hasher = FxHasher::default();
        pattern.hash(&mut hasher);
        parsed.flags.hash(&mut hasher);
        fullmatch.hash(&mut hasher);
        alphabet.map(|a| &a.intervals).hash(&mut hasher);
        let program = if fullmatch {
            Program::compile_recognizer(&parsed)
        } else {
            Program::compile_recognizer(&anywhere(&parsed))
        }
        .ok();
        RegexConstraint {
            parsed,
            fullmatch,
            program,
            identity: hasher.finish(),
            simplest: OnceBox::new(),
            drawn: Drawn::default(),
        }
    }

    /// Whether `text` is one of the pattern's strings: matched whole by
    /// it, or, when the pattern need not match the whole string,
    /// matched somewhere in it, as CPython's `fullmatch` and `search`
    /// decide. A string the generator drew is one (see [`Drawn`]); of any
    /// other, a codepoint that is not a Unicode scalar value is in no
    /// pattern, and, for a pattern the [`Program`] cannot express, a
    /// match the matcher cannot finish within its limits is no match.
    pub fn accepts(&self, text: &[u32]) -> bool {
        if self.drawn.contains(text) {
            return true;
        }
        let Some(chars) = text
            .iter()
            .map(|&cp| char::from_u32(cp))
            .collect::<Option<Vec<char>>>()
        else {
            return false;
        };
        match &self.program {
            Some(program) => {
                let end = self.fullmatch.then_some(chars.len());
                program.scan(&chars, 0, end).0.is_some()
            }
            None => matches(&self.parsed, self.fullmatch, &chars),
        }
    }

    /// The simplest string of the pattern, computing it with `init` when
    /// it is not set yet. Once set it is never computed again.
    pub fn simplest_or_init<E>(
        &self,
        init: impl FnOnce() -> Result<Vec<u32>, E>,
    ) -> Result<&[u32], E> {
        self.simplest
            .get_or_try_init(|| init().map(Box::new))
            .map(|simplest| simplest.as_slice())
    }

    /// The simplest string of the pattern, once the generator has set it.
    pub fn simplest(&self) -> Option<&[u32]> {
        self.simplest.get().map(|simplest| simplest.as_slice())
    }

    pub fn identity(&self) -> u64 {
        self.identity
    }

    /// Note `text` as a string the generator drew (see [`Drawn`]).
    pub fn note_drawn(&self, text: &[u32]) {
        self.drawn.note(text);
    }

    /// Whether the pattern must match the whole string.
    pub fn fullmatch(&self) -> bool {
        self.fullmatch
    }
}

impl PartialEq for RegexConstraint {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}

impl Eq for RegexConstraint {}

impl Hash for RegexConstraint {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.identity.hash(state);
    }
}

/// `parsed` after a lazy run of any characters: a pattern matching at
/// the start of a text exactly when `parsed` matches somewhere in it.
fn anywhere(parsed: &ParsedPattern) -> ParsedPattern {
    let any = OpCode::Subpattern {
        group: None,
        add_flags: SRE_FLAG_DOTALL,
        del_flags: 0,
        p: SubPattern {
            data: alloc::vec![OpCode::Any],
        },
    };
    let skip = OpCode::MinRepeat {
        min: 0,
        max: MAXREPEAT,
        item: SubPattern {
            data: alloc::vec![any],
        },
    };
    let inner = OpCode::Subpattern {
        group: None,
        add_flags: 0,
        del_flags: 0,
        p: parsed.pattern.clone(),
    };
    ParsedPattern {
        pattern: SubPattern {
            data: alloc::vec![skip, inner],
        },
        flags: parsed.flags,
        group_refs: parsed.group_refs,
    }
}

/// Whether `chars` is one of `parsed`'s strings: matched whole by it
/// when `fullmatch`, and otherwise matched somewhere in it, as CPython's
/// `fullmatch` and `search` decide. A match the matcher cannot finish
/// within its limits is no match.
pub(crate) fn matches(parsed: &ParsedPattern, fullmatch: bool, chars: &[char]) -> bool {
    let matcher = Matcher::new(parsed);
    if fullmatch {
        return matches!(matcher.fullmatch(chars), Ok(true));
    }
    (0..=chars.len()).any(|start| matches!(matcher.match_at_touch(chars, start), Ok((Some(_), _))))
}

impl fmt::Debug for RegexConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegexConstraint")
            .field("identity", &self.identity)
            .field("fullmatch", &self.fullmatch)
            .finish()
    }
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/re_constraint_tests.rs"]
mod tests;
