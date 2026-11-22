//! A BODY'S RUN FACTS, kept as a record (run.md, "Run and test read the engine's answer", E1).
//!
//! The Bodies stage keeps nothing of a body but its row (a key, reads, the verdict digest), and
//! `run` and `test` need three more facts of each body: whether it is clean (its class), and
//! which items its check covers (what `work::body_payload` holds for the old request). They are
//! one small record a `BodyCheck` task, put beside its row (engine_check.rs, the Bodies put), and
//! this file is the one home of its bytes (rule 30): [`write`], the one reader [`read`] that
//! checks every length and order once (rule 74), and the one rule that gives a class from a
//! task's findings' rules ([`class_of`]), which `run_ids.rs` `verdict` asks too.
//!
//! The record holds a body's CLASS, never its findings' text: the findings are in the lines
//! record alone (rule 30). Its out is the row's verdict digest, so it moves with the row and
//! invalidates nothing of its own.
//!
//! Form 3 adds THE TYPED RECORD'S KEY (engine_body_record.rs `typed_key`, the key of that
//! record's bytes): how `run` finds a clean body's checked columns. The typed record is keyed by
//! its bytes because a clean body's verdict digest does not move with every column it holds
//! (engine_body_record.rs, THE KEY); this record, keyed by its own bytes too (engine_check.rs,
//! the Bodies put), moves with it.
//!
//! ```text
//! payload  "BRF2" | form u16 = 3 | tag u8 | 0 u8 | errors u32 | rule u16 | 0 u16 | count u32 | count * key [16] | typed
//! tag      0 clean (errors 0, rule 0) | 1 errors (errors >= 1, rule: the first error's rule)
//!          | 2 untyped (errors 0, rule: the not-typed rule) | 3 typed (errors 0, rule: the
//!          borrow-not-checked rule)
//! key      a covered item's checker key, `blaze_typeck::ItemKey` as 16 big-endian bytes, as
//!          `work::body_payload` holds them; sorted, strictly ascending (one each)
//! typed    0 u8 (no typed record) | 1 u8, then the typed record's store key [32]
//! ```
//!
//! Little-endian numbers, every unused byte zero: the bytes are a function of (class, covered
//! set, typed key) alone (rule 25), so two builders of one answer write equal bytes.

use core::alloc::Allocator;

use blaze_own::Vec;

/// The payload's first four bytes: another form's are refused.
const MAGIC: [u8; 4] = *b"BRF2";

/// The form number after the magic. Form 3: the typed record's key after the covered keys.
const FORM: u16 = 3;

/// The mark after the covered keys: no typed record, or its 32-byte store key after it.
const NO_TYPED: u8 = 0;
const TYPED_AT: u8 = 1;

/// A store key's bytes.
const STORE_KEY: usize = 32;

/// The head's bytes: everything before the keys.
const HEAD: usize = 20;

/// One covered key's bytes.
const KEY: usize = 16;

const CLEAN: u8 = 0;
const ERRORS: u8 = 1;
const UNTYPED: u8 = 2;
const TYPED: u8 = 3;

/// What a body's check came to, as `run` needs it: the only class a program may call into is
/// `Clean`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Class {
    /// Checked whole, no error.
    Clean,
    /// Checked, `errors` findings that are errors (at least one, by its type: a record saying
    /// none is refused by [`read`] as [`BodyFactsError::NoErrors`]); the rule of the first.
    Errors { errors: core::num::NonZeroU32, first: u16 },
    /// Types checked whole, no error; the borrow check refused or not run: the rule of the note
    /// that says so (`BORROWS_NOT_CHECKED`). Borrows never change what a body does.
    Typed { rule: u16 },
    /// Refused, panicked, or its types not checked to the end: the rule that says so
    /// (`TYPES_NOT_CHECKED`, or `NOT_RUN` for a refusal, a panic or a body no check ran).
    Untyped { rule: u16 },
}

/// A body's run facts as [`read`] gives them: the class and the covered items' keys in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct BodyFacts<'a> {
    pub(crate) class: Class,
    /// The checker keys of the items the task covers (16 big-endian bytes each, the form
    /// `work::body_payload` holds), sorted, one each.
    pub(crate) covers: &'a [[u8; KEY]],
    /// The store key of the body's typed record (engine_body_record.rs `typed_key`): `None`
    /// when its check kept none (findings, stalled goals, not checked whole).
    pub(crate) typed: Option<blaze_drive::Key>,
}

impl BodyFacts<'_> {
    /// The covered checker keys as the old request's payload gives them (`BodyPayload::covered`).
    pub(crate) fn covered(&self) -> impl ExactSizeIterator<Item = blaze_typeck::ItemKey> + '_ {
        self.covers.iter().map(|key| blaze_typeck::ItemKey(u128::from_be_bytes(*key)))
    }
}

/// Why bytes are not a facts record, or why a record cannot be written: each names what.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BodyFactsError {
    /// Shorter than the head.
    Short { len: usize },
    /// Another form's magic.
    Magic { found: [u8; 4] },
    /// Another form number.
    Form { found: u16 },
    /// A class tag that is none of the three.
    Tag { tag: u8 },
    /// A field the class does not use is not zero.
    Fields { tag: u8 },
    /// An errors class whose error count is zero: no verdict (finding 13).
    NoErrors,
    /// A byte that must be zero is not, at this offset.
    Reserved { at: usize },
    /// The head's key count and the bytes disagree.
    Size { count: u32, len: usize },
    /// The keys are not sorted one each, at this key (the later of the two).
    Unsorted { at: usize },
    /// More keys than the count's 32 bits hold (a write).
    TooMany { count: usize },
    /// The mark after the covered keys is neither "no typed record" nor "its key follows".
    TypedMark { mark: u8 },
}

impl core::fmt::Display for BodyFactsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BodyFactsError::Short { len } => write!(f, "a body's run facts of {len} bytes, shorter than their {HEAD}-byte head (engine_body_facts.rs `read`)"),
            BodyFactsError::Magic { found } => write!(f, "a body's run facts of form {found:?}, not {MAGIC:?} (engine_body_facts.rs `read`)"),
            BodyFactsError::Form { found } => write!(f, "a body's run facts of form number {found}, not {FORM} (engine_body_facts.rs `read`)"),
            BodyFactsError::Tag { tag } => write!(f, "a body's run facts of class tag {tag}, none of 0 (clean), 1 (errors), 2 (untyped), 3 (typed) (engine_body_facts.rs `read`)"),
            BodyFactsError::Fields { tag } => write!(f, "a body's run facts of class tag {tag} whose error count or rule is not what the class holds (engine_body_facts.rs `read`)"),
            BodyFactsError::NoErrors => f.write_str("a body's run facts of class tag 1 (errors) with an error count of zero (engine_body_facts.rs `read`)"),
            BodyFactsError::Reserved { at } => write!(f, "a body's run facts whose byte {at} is not zero (engine_body_facts.rs `read`)"),
            BodyFactsError::Size { count, len } => write!(f, "a body's run facts naming {count} covered keys in {len} bytes (engine_body_facts.rs `read`)"),
            BodyFactsError::Unsorted { at } => write!(f, "a body's run facts whose covered keys are not sorted one each at key {at} (engine_body_facts.rs)"),
            BodyFactsError::TooMany { count } => write!(f, "a body covering {count} items, more than a record's 32-bit count holds (engine_body_facts.rs `write`)"),
            BodyFactsError::TypedMark { mark } => write!(f, "a body's run facts whose typed-record mark is {mark}, neither {NO_TYPED} (none) nor {TYPED_AT} (its key follows) (engine_body_facts.rs `read`)"),
        }
    }
}

/// Why the Bodies put could not make a task's facts: each names the body in the caller's error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MakeError {
    /// The task was entered and the Bodies stage holds no result for it.
    NotRan,
    /// The merged answer holds no row for it.
    NoRow,
    /// Its bytes cannot be written.
    Bytes(BodyFactsError),
}

impl core::fmt::Display for MakeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MakeError::NotRan => f.write_str("the task was entered and the Bodies stage has no result for it, so no run facts can be made (engine_bodies.rs `write_facts`)"),
            MakeError::NoRow => f.write_str("the task was entered and the merged answer has no row for it (engine_check.rs, the Bodies put)"),
            MakeError::Bytes(error) => write!(f, "{error}"),
        }
    }
}

/// Whether a finding of `rule` says the body's types were not checked whole: its types were not
/// checked to the end, or it was not run. Such a body is `Untyped` whatever else it has.
pub(crate) fn is_untyped(rule: u16) -> bool {
    rule == crate::body_types::TYPES_NOT_CHECKED || rule == crate::work::NOT_RUN
}

/// The class the rules of a task's findings give: a rule of the not-typed kind ([`is_untyped`])
/// gives `Untyped`, at the first such; else the rules `crate::every_rule` calls errors are
/// counted, and the first is kept (`Errors`); else `BORROWS_NOT_CHECKED` (types whole, no error,
/// the borrow check refused or not run) gives `Typed`; none is `Clean`. Whether a `Typed` body
/// runs is the reader's (borrows are notes until `BORROWS_ARE_ERRORS`), never this record's.
pub(crate) fn class_of(rules: impl IntoIterator<Item = u16>) -> Class {
    let mut errors = 0u32;
    let mut first = 0u16;
    let mut typed = false;
    for rule in rules {
        if is_untyped(rule) {
            return Class::Untyped { rule };
        }
        if rule == crate::body_types::BORROWS_NOT_CHECKED {
            typed = true;
        }
        if crate::every_rule(rule) {
            if errors == 0 {
                first = rule;
            }
            errors = errors.saturating_add(1);
        }
    }
    if let Some(errors) = core::num::NonZeroU32::new(errors) {
        Class::Errors { errors, first }
    } else if typed {
        Class::Typed { rule: crate::body_types::BORROWS_NOT_CHECKED }
    } else {
        Class::Clean
    }
}

/// The facts record's payload for (`class`, `covers`, `typed`) onto `into`: `covers` sorted, one
/// each (checked here: a refusal writes nothing). The bytes are a function of the three (rule 25).
pub(crate) fn write<A: Allocator>(class: Class, covers: &[[u8; KEY]], typed: Option<blaze_drive::Key>, into: &mut Vec<u8, A>) -> Result<(), BodyFactsError> {
    let count = u32::try_from(covers.len()).map_err(|_| BodyFactsError::TooMany { count: covers.len() })?;
    if let Some(at) = (1..covers.len()).find(|&at| covers[at - 1] >= covers[at]) {
        return Err(BodyFactsError::Unsorted { at });
    }
    let (tag, errors, rule) = match class {
        Class::Clean => (CLEAN, 0, 0),
        Class::Errors { errors, first } => (ERRORS, errors.get(), first),
        Class::Untyped { rule } => (UNTYPED, 0, rule),
        Class::Typed { rule } => (TYPED, 0, rule),
    };
    into.reserve(HEAD + covers.len() * KEY + 1 + STORE_KEY);
    into.extend_from_slice(&MAGIC);
    into.extend_from_slice(&FORM.to_le_bytes());
    into.push(tag);
    into.push(0);
    into.extend_from_slice(&errors.to_le_bytes());
    into.extend_from_slice(&rule.to_le_bytes());
    into.extend_from_slice(&[0, 0]);
    into.extend_from_slice(&count.to_le_bytes());
    for key in covers {
        into.extend_from_slice(key);
    }
    match typed {
        Some(key) => {
            into.push(TYPED_AT);
            into.extend_from_slice(&key.0.0);
        }
        None => into.push(NO_TYPED),
    }
    Ok(())
}

/// `bytes` as a body's run facts, every length and order checked here, once (rule 74), the keys
/// read in place.
pub(crate) fn read(bytes: &[u8]) -> Result<BodyFacts<'_>, BodyFactsError> {
    let Some((head, tail)) = bytes.split_at_checked(HEAD) else { return Err(BodyFactsError::Short { len: bytes.len() }) };
    let magic = [head[0], head[1], head[2], head[3]];
    if magic != MAGIC {
        return Err(BodyFactsError::Magic { found: magic });
    }
    let form = u16::from_le_bytes([head[4], head[5]]);
    if form != FORM {
        return Err(BodyFactsError::Form { found: form });
    }
    for at in [7, 14, 15] {
        if head[at] != 0 {
            return Err(BodyFactsError::Reserved { at });
        }
    }
    let tag = head[6];
    let errors = u32::from_le_bytes([head[8], head[9], head[10], head[11]]);
    let rule = u16::from_le_bytes([head[12], head[13]]);
    let count = u32::from_le_bytes([head[16], head[17], head[18], head[19]]);
    let class = match tag {
        CLEAN if errors == 0 && rule == 0 => Class::Clean,
        ERRORS => Class::Errors { errors: core::num::NonZeroU32::new(errors).ok_or(BodyFactsError::NoErrors)?, first: rule },
        UNTYPED if errors == 0 => Class::Untyped { rule },
        TYPED if errors == 0 => Class::Typed { rule },
        CLEAN | ERRORS | UNTYPED | TYPED => return Err(BodyFactsError::Fields { tag }),
        _ => return Err(BodyFactsError::Tag { tag }),
    };
    let size = BodyFactsError::Size { count, len: bytes.len() };
    let Some((keys, after)) = (count as usize).checked_mul(KEY).and_then(|keys| tail.split_at_checked(keys)) else { return Err(size) };
    let typed = match after {
        [NO_TYPED] => None,
        [TYPED_AT, key @ ..] if key.len() == STORE_KEY => {
            let mut id = [0u8; STORE_KEY];
            id.copy_from_slice(key);
            Some(blaze_drive::Key(blaze_ids::ContentId(id)))
        }
        [] | [NO_TYPED | TYPED_AT, ..] => return Err(size),
        [mark, ..] => return Err(BodyFactsError::TypedMark { mark: *mark }),
    };
    let (covers, _) = keys.as_chunks::<KEY>();
    if let Some(at) = (1..covers.len()).find(|&at| covers[at - 1] >= covers[at]) {
        return Err(BodyFactsError::Unsorted { at });
    }
    Ok(BodyFacts { class, covers, typed })
}
