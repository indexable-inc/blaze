//! THE READ SIDE OF A BODY'S TYPED RECORD (D159 "one body record", D163 part 2): what `blaze
//! run` reads from the check of a body so it builds the body without checking it again.
//!
//! Run needs, per body, the check's answer: blaze-typeck `Checked`, over the run's own
//! lowering of the body (the lowering is deterministic from the body's text and the heads it
//! names, so its expression, pattern and local ids are the check's). `Checked` is
//! self-contained: its types are its own `PortableTys`, every item in it is a key from text.
//! The columns the make reads: `types`, `expr_ty`, `pat_ty`, `local_ty`, `adjusts`,
//! `callees`, `expr_defs`, `const_args`, `pat_const_args`, `pat_defs`, `fields`, `captures`,
//! `closures`, `hidden`, `promoted`, `coroutine_witnesses`, `awaited`, `selected`,
//! `fru_fields`, `default_fields`, `index_mut`, `needs`. Not read by a run (a body that runs
//! has none, or they are the check's own): `diags`, `suggests`, `stalled`, `findings`,
//! `undecided`, `reads`. The record's form is blaze-typeck checked_record.rs (R1: `encode` of
//! a clean check, `decode` back to `Checked`). The check keeps one per clean body under
//! blaze-cli engine_body_record.rs `typed_key(check, out)` (put by engine_bodies.rs, the Bodies
//! stage); the command's reader is that file's `StoreTyped`.
//!
//! BODY-LOCAL ITEMS. The record is per BODY OWNER, keyed by the owner's checker key; the rule
//! for what is local to a body:
//!
//! - A CLOSURE or an ASYNC BLOCK (an async closure's coroutine too) has no key and no record
//!   of its own. Its identity is `(owner, ordinal)` (blaze-typeck coroutine_id.rs, typeck.md
//!   "A closure's identity"), and its columns (its expressions' types, its captures, its
//!   kind and signature in `closures`, its witnesses in `coroutine_witnesses`) are rows of
//!   its OWNER's `Checked`: one check of the owner's body checks every closure and block in
//!   it. A run makes a closure's or coroutine's instance at its owner's row (run/make_one.rs
//!   `owners_named`), so it asks [`TypedRecords::typed`] for the owner, never for the local;
//!   there is no local key to ask with. WHEN THE OWNER'S RECORD IS READ, the reader files its
//!   coroutines and closures in the store exactly as a clean check of the owner files them
//!   (fill.rs `file_read_check`: opaque_auto.rs `file` into `Store::hidden`, the answer to
//!   `Question::Coroutine(owner, n)`; foreign_closures.rs `ClosureRows::file` into
//!   `Store::closures`, the answer to `Question::Closure(owner, n)`), from the record's
//!   columns and the run's lowering. A make that asks about a coroutine type (its `Drop`
//!   impl, its layout, an auto trait) meets the same answer on both arms. That was the
//!   "async awaiting another" rung's refusal: "the world has no answer to Coroutine(..)".
//! - A FN (or const, static) DECLARED IN A BODY is an item with a key of its own, derived from
//!   its enclosing body's by text (front2 nested_exports.rs `local_key` under `body_key`,
//!   `LibKey::INNER` set in byte 8): the key names its parent by that derivation, and the
//!   parent is found by deriving it again over the crate's text (lazy/locate.rs
//!   `locate_local`). Its own body is its own record, under its own key. `typed(local)` gives
//!   the local fn's own columns; the reader files the key first by LOWERING the bodies
//!   around it, outermost first (never by checking them: the lowering is what files a body's
//!   locals, locals.rs `Locals::keep`). A local whose enclosing body has no record still
//!   reads (its own record is what is read). A local whose enclosing body does not LOWER is
//!   that body's refusal ([`TypedGap::Refused`] in the double, `LazyRefusal::Lower` naming
//!   the enclosing body and the local in the reader).
//! - EVERY ITEM A BODY'S COLUMNS NAME (a callee, a fn named as a value, an ADT and its ctors)
//!   is filed and its head made by the READER from the columns (fill.rs `check_one_key_by`,
//!   `callee_row`), never left to a check's side effects: a check files what it asks as it
//!   goes (an impl's head files its members), and a body read from its record was not
//!   checked here. That was the async rung's refusal: `main`'s callee, a member of an impl
//!   (its key's inner bit clear: not a body-local item), had no row path in the reader.
//!
//! [`Rechecked`] follows the same rule with no extra step: its own program's `check_one_key`
//! of a local fn files it by the same lowering of its parents, and is asked only for owners
//! and local fns.
//!
//! [`TypedRecords`] is the interface [`super::LazyProgram::check_one_typed`] reads through: a
//! body's columns by its checker key, or a typed gap naming the body (rule 3: no second check
//! when the record is not there). [`Rechecked`] is the test double, filled from today's second
//! check: its own lazy program over the same names records checks the body and hands the
//! columns over, so the oracle (run/make_one.rs `walk_records`) compares a run made from a
//! record against one made by checking, entry byte for entry byte.

use blaze_own::Alloc;
use blaze_typeck::checked_record::{self, CheckedRecordError};
use blaze_typeck::{Checked, ItemKey};

use super::{LazyProgram, LazyRefusal, PositionSource};

/// A body's checked columns as the check kept them (the module doc). One question per body:
/// [`super::LazyProgram`] asks each key once.
pub trait TypedRecords {
    /// The checked columns of the body of fn `key` (its checker key), or why there are none.
    fn typed(&mut self, key: ItemKey) -> Result<Checked<'static>, TypedGap>;
}

/// A KIND OF BODY THE RUN CHECKS ITSELF (the typing seam's third arm, run/make_one.rs `ByKind`):
/// `check` keeps no typed record for a body of this kind yet, so the run's one check of it is
/// the only check it gets. Each is named and counted in the run's closing line, and never a
/// refusal: a body of a kind `check` keeps records for is read from its record, and a gap there
/// is a refusal. A kind moves to [`BodySource::Read`] once `check` keeps its record (front2
/// tables/task.rs), and then its variant here goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunChecked {
    /// A toolchain crate's body: `check` checks none (blaze-cli run_ids.rs grains).
    Toolchain = 0,
    /// A locked dependency's body: likewise, until the run's check keeps a record under its key.
    Dependency = 1,
    /// A fn declared in a workspace body (its key's inner bit, [`is_local`]): `check` checks it
    /// under its root, and keeps no record under its own key yet.
    Local = 2,
    /// A workspace body `check` ran and kept no typed record of. Its facts name none: stalled
    /// goals decided after the record was written (F18), or columns the record refused.
    Unrecorded = 3,
}

impl RunChecked {
    /// Every kind, in index order ([`RunChecked::index`]).
    pub const ALL: [RunChecked; 4] = [RunChecked::Toolchain, RunChecked::Dependency, RunChecked::Local, RunChecked::Unrecorded];

    pub fn index(self) -> usize {
        self as usize
    }

    /// The kind as the run's closing line names it.
    pub fn name(self) -> &'static str {
        match self {
            RunChecked::Toolchain => "toolchain",
            RunChecked::Dependency => "dependency",
            RunChecked::Local => "local fn",
            RunChecked::Unrecorded => "no record kept",
        }
    }
}

/// Where one body's checked columns come from: the record `check` kept, read, or the run's own
/// check, for a body of a kind `check` keeps none for ([`RunChecked`]).
pub enum BodySource {
    Read(Checked<'static>),
    Run(RunChecked),
}

/// The run's records of checked bodies, kind by kind (run/make_one.rs `ByKind`): for each body,
/// its columns read from its record, or the kind the run checks itself. A body of a kind that
/// has records but whose record cannot be read is a [`TypedGap`], which the run refuses by name
/// (rule 3: never a quiet check in its place).
pub trait KeptBodies {
    fn source(&mut self, key: ItemKey) -> Result<BodySource, TypedGap>;
}

/// Whether checker key `key` is a fn declared in a body: the inner bit of its library key
/// (byte 8, `LibKey::INNER`; front2 nested_exports.rs sets it), which a checker key keeps in its
/// first 16 bytes (front2 keys.rs `checker_of`).
pub fn is_local(key: ItemKey) -> bool {
    key.0.to_be_bytes()[8] & blaze_id::ItemKey::INNER != 0
}

/// One body's columns, read before the lowering by [`KeptBodies::source`], handed to
/// `check_one_typed` for that body alone: a second ask, or an ask for another key, is a gap
/// named by its key.
pub(crate) struct Given {
    pub(crate) key: ItemKey,
    pub(crate) got: Option<Result<Checked<'static>, TypedGap>>,
}

impl TypedRecords for Given {
    fn typed(&mut self, key: ItemKey) -> Result<Checked<'static>, TypedGap> {
        if key != self.key {
            return Err(TypedGap::NoBody { key });
        }
        self.got.take().unwrap_or(Err(TypedGap::NoRecord { key }))
    }
}

/// Why a body's typed record was not read: each names the body (rule 19).
#[derive(Debug)]
pub enum TypedGap {
    /// No typed record of the body is kept: no body row, no out today, or nothing under its key
    /// (a body that is not clean has none, and the run gate refuses it before this is asked).
    NoRecord { key: ItemKey },
    /// The record source holds no body for `key`, which the run lowered one for.
    NoBody { key: ItemKey },
    /// The test double's own check of the body refused it ([`Rechecked`]).
    Refused { key: ItemKey, why: Box<LazyRefusal> },
    /// The body's record was not written or not read back (blaze-typeck checked_record.rs).
    Record { key: ItemKey, why: CheckedRecordError },
    /// The record source failed to read the body's record (the command's store: blaze-cli
    /// engine_body_record.rs `TypedReadError`, which names the body and the store key).
    Read { key: ItemKey, why: Box<dyn core::error::Error> },
}

impl core::fmt::Display for TypedGap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TypedGap::NoRecord { key } => write!(f, "no typed record of the body {:032x} is kept (front2/program/lazy/typed.rs, D163)", key.0),
            TypedGap::NoBody { key } => write!(f, "the typed records hold no body for {:032x}, which the run lowered", key.0),
            TypedGap::Refused { key, why } => write!(f, "the second check of the body {:032x} refused: {why}", key.0),
            TypedGap::Record { key, why } => write!(f, "the typed record of the body {:032x}: {why} (blaze-typeck checked_record.rs)", key.0),
            TypedGap::Read { key, why } => write!(f, "the typed record of the body {:032x} was not read: {why}", key.0),
        }
    }
}

impl core::error::Error for TypedGap {}

/// THE TEST DOUBLE of the typed records: today's second check, in a lazy program of its own over
/// the same names, each body checked once when it is asked and its columns handed over. What a
/// typed record will hold, until the check keeps one (the module doc).
pub struct Rechecked<'n, S: PositionSource<'n>> {
    program: LazyProgram<'n, S>,
    alloc: Alloc<'static>,
}

impl<'n, S: PositionSource<'n>> Rechecked<'n, S> {
    /// The double over `program`: a lazy program no body was checked in yet, its start decided
    /// as the run's is. Each body's record is written and read back in `alloc`.
    pub fn new(program: LazyProgram<'n, S>, alloc: Alloc<'static>) -> Rechecked<'n, S> {
        Rechecked { program, alloc }
    }
}

impl<'n, S: PositionSource<'n>> TypedRecords for Rechecked<'n, S> {
    /// The body checked once in the double's program, then THROUGH ITS RECORD: written
    /// (blaze-typeck checked_record.rs `encode`) and read back (`decode`), so what the run
    /// gets is only what the record holds, and the oracle tests the record form too.
    fn typed(&mut self, key: ItemKey) -> Result<Checked<'static>, TypedGap> {
        let checked = match self.program.check_one_key(key) {
            Ok(Some((_, checked))) => checked,
            Ok(None) => return Err(TypedGap::NoBody { key }),
            Err(why) => return Err(TypedGap::Refused { key, why: Box::new(why) }),
        };
        let bytes = checked_record::encode(&checked, self.alloc).map_err(|why| TypedGap::Record { key, why })?;
        checked_record::decode(&bytes, self.alloc).map_err(|why| TypedGap::Record { key, why })
    }
}

/// The double as the run's records: every body read from its record (a record of every kind).
impl<'n, S: PositionSource<'n>> KeptBodies for Rechecked<'n, S> {
    fn source(&mut self, key: ItemKey) -> Result<BodySource, TypedGap> {
        self.typed(key).map(BodySource::Read)
    }
}
