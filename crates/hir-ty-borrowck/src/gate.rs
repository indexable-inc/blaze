//! What of a body may become code that runs.
//!
//! A build's output never holds code whose borrow check failed or did not run (cargo's rule: a
//! borrowck error means no binary). The only way to MIR for a build's codegen is [`checked`],
//! which gives a [`Checked`] once the borrow check of the body, and of every closure, coroutine
//! and `const` block in it, found no error; [`CheckedMir`] is made from nothing else.
//!
//! Live patching alone may have code before the check finishes (#1302, #276): [`speculate`] is
//! its entry point, and gives an [`Unchecked`], whose MIR is an [`UncheckedMir`] that no
//! [`CheckedMir`] can be made of, and a [`Pending`] check. The patch applies the code, finishes
//! the check beside it (a plane task sharing the database by reference) and rolls
//! back when [`Pending::finish`] refuses. A build's codegen takes [`CheckedMir`] and a build's
//! reuse of stored code takes a [`Checked`], so nothing the live path made can reach one by its
//! types.

use std::{
    alloc::{Allocator, Global},
    fmt,
    marker::PhantomData,
};

use base_db::Db;
use hir_def::{DefWithBodyId, ExpressionStoreOwnerId};
use hir_ty_mir::mir::{MirBody, MirLowerError, MirSpan};

use crate::{BorrowckDiagnostic, BorrowckResult, NotChecked, borrowck};

/// Proof that the borrow check of a body passed, in the database revision `'db` it was asked
/// in: an edit, which needs the database mutably, ends it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Checked<'db> {
    owner: DefWithBodyId,
    _revision: PhantomData<fn() -> &'db ()>,
}

/// Why a body's code may not run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The borrow check found these errors, rustc's, in the body or a body in it.
    Errors { owner: DefWithBodyId, errors: Vec<BorrowckDiagnostic> },
    /// The checker stopped at an error rustc reports before borrow checking.
    NotChecked { owner: DefWithBodyId, why: NotChecked },
    /// MIR lowering does not support something in the body or a body in it, so it was not
    /// checked.
    Unsupported { owner: DefWithBodyId, what: String },
    /// The MIR asked for could not be built.
    Lowering { owner: DefWithBodyId, error: blaze_mem::String<Global> },
    /// The MIR given is of another body than the one checked.
    NotOf { owner: DefWithBodyId, mir_owner: blaze_mem::String<Global> },
}

impl Refused {
    pub fn owner(&self) -> DefWithBodyId {
        match *self {
            Refused::Errors { owner, .. }
            | Refused::NotChecked { owner, .. }
            | Refused::Unsupported { owner, .. }
            | Refused::Lowering { owner, .. }
            | Refused::NotOf { owner, .. } => owner,
        }
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Errors { owner, errors } => {
                write!(f, "the borrow check of {owner:?} found {} error(s):", errors.len())?;
                for error in errors {
                    write!(f, " {}: {};", error.code, error.message)?;
                }
                Ok(())
            }
            Refused::NotChecked { owner, why } => {
                write!(f, "{owner:?} was not borrow checked: {why:?}")
            }
            Refused::Unsupported { owner, what } => {
                write!(f, "{owner:?} was not borrow checked, MIR lowering does not support {what}")
            }
            Refused::Lowering { owner, error } => {
                write!(f, "the MIR of {owner:?} could not be built: {error}")
            }
            Refused::NotOf { owner, mir_owner } => {
                write!(f, "MIR of {mir_owner} given as that of {owner:?}")
            }
        }
    }
}

impl std::error::Error for Refused {}

/// What a borrow check result lets run: errors first, as they say the most.
pub(crate) fn verdict(owner: DefWithBodyId, result: &BorrowckResult) -> Result<(), Refused> {
    let errors: Vec<_> = result.diagnostics.iter().filter(|it| it.is_error()).cloned().collect(); // alloc: a refusal's own errors; empty, so no allocation, for a passing body
    if !errors.is_empty() {
        return Err(Refused::Errors { owner, errors });
    }
    if let Some(why) = result.not_checked
        && why.refuses()
    {
        return Err(Refused::NotChecked { owner, why });
    }
    if let Some(what) = &result.skipped {
        let what = what.clone(); // clone: a refusal owns its text, on the error path
        return Err(Refused::Unsupported { owner, what });
    }
    Ok(())
}

/// The build path: `def`'s borrow check, which must pass before any of its code is compiled
/// for a build or taken from the store into one. A verdict the item store keeps is the
/// [`borrowck`] query's own answer, so the query is the one place that looks at it.
///
/// The old cache's verdict (`check_cache::served_passed`) is never read under an item store in
/// `Mode::Trace`: its one door, `check_cache::served_body`, gives nothing there, so the verdict
/// is the [`borrowck`] query's run, which traces it whatever it is.
pub fn checked(db: &Db, def: DefWithBodyId) -> Result<Checked<'_>, Refused> {
    if !hir_ty::check_cache::served_passed(db, def) {
        verdict(def, borrowck(db, def))?;
    }
    Ok(Checked { owner: def, _revision: PhantomData })
}

/// The tag of each kind of [`Refused`] in an encoded verdict ([`encode_verdict`]).
const ERRORS: u8 = 1;
const NOT_CHECKED: u8 = 2;
const UNSUPPORTED: u8 = 3;
const LOWERING: u8 = 4;
const NOT_OF: u8 = 5;

/// A borrow check's verdict as the item store traces it (`hir_ty::body_store::offer_verdict`),
/// written over the end of `out`: what the answer of a body's borrow check step is, so a body
/// with a borrow error has a record of its errors. Deterministic, and of the body alone: no
/// `Gid`, no address, no position in the file, so no edit outside the body moves a byte.
///
/// The layout, integers little-endian:
///
/// - a pass (`None`): no bytes;
/// - else one tag byte, then by the tag:
///   - `1`, errors: a `u64` count, then for each error in the check's order: a `u64` length
///     and the code's UTF-8 (`E0499`), a span (one kind byte: 0 an expression, 1 an
///     expression's end, 2 a pattern, 3 a binding, 4 `self`, 5 unknown, 6 an inlined body's
///     span; then a `u32` index in the body's own store, 0 for kinds 4 and 5), one byte
///     `at_signature` (0 or 1), and a `u64` length and the message's UTF-8;
///   - `2`, not checked: one byte, why ([`not_checked_code`]);
///   - `3`, unsupported by MIR lowering; `4`, MIR that could not be built; `5`, MIR of another
///     body: nothing more, as their text renders types and ids with debug formatting, which
///     names ids that differ between databases.
///
/// Not in it: a pass's lints, labels, notes and suggestions (a label may name an item by its
/// id); the verdict is what refuses a body.
///
/// Floor: one pass over the errors, writing each once; a pass writes nothing.
pub fn encode_verdict<A: Allocator>(refused: Option<&Refused>, out: &mut Vec<u8, A>) {
    let Some(refused) = refused else { return };
    match refused {
        Refused::Errors { errors, .. } => {
            out.push(ERRORS);
            out.extend_from_slice(&(errors.len() as u64).to_le_bytes());
            for error in errors {
                out.extend_from_slice(&(error.code.len() as u64).to_le_bytes());
                out.extend_from_slice(error.code.as_bytes());
                let (kind, index) = span_code(error.span);
                out.push(kind);
                out.extend_from_slice(&index.to_le_bytes());
                out.push(u8::from(error.at_signature));
                out.extend_from_slice(&(error.message.len() as u64).to_le_bytes());
                out.extend_from_slice(error.message.as_bytes());
            }
        }
        Refused::NotChecked { why, .. } => {
            out.push(NOT_CHECKED);
            out.push(not_checked_code(*why));
        }
        Refused::Unsupported { .. } => out.push(UNSUPPORTED),
        Refused::Lowering { .. } => out.push(LOWERING),
        Refused::NotOf { .. } => out.push(NOT_OF),
    }
}

/// A verdict [`encode_verdict`] wrote, read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict<'a> {
    Passed,
    /// The errors, in the check's order.
    Errors(VerdictErrors<'a>),
    NotChecked(NotChecked),
    Unsupported,
    Lowering,
    NotOf,
}

/// One error of an encoded verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VerdictError<'a> {
    pub code: &'a str,
    pub span: MirSpan,
    pub at_signature: bool,
    pub message: &'a str,
}

impl VerdictError<'_> {
    /// Whether this is what `diagnostic` encodes as.
    pub fn is(&self, diagnostic: &BorrowckDiagnostic) -> bool {
        self.code == diagnostic.code
            && self.span == diagnostic.span
            && self.at_signature == diagnostic.at_signature
            && self.message == diagnostic.message
    }
}

/// The errors of an encoded verdict, read in place: [`decode_verdict`] read each once, so this
/// is the proof that every one reads, and iterating them reads the bytes again with nothing
/// left to refuse. Nothing is allocated for them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct VerdictErrors<'a> {
    /// At the first error.
    at: Cursor<'a>,
    len: usize,
}

impl<'a> VerdictErrors<'a> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The errors, in the check's order.
    pub fn iter(&self) -> VerdictErrorsIter<'a> {
        VerdictErrorsIter { at: self.at, left: self.len }
    }
}

impl fmt::Debug for VerdictErrors<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> IntoIterator for VerdictErrors<'a> {
    type Item = VerdictError<'a>;
    type IntoIter = VerdictErrorsIter<'a>;

    fn into_iter(self) -> VerdictErrorsIter<'a> {
        self.iter()
    }
}

/// The errors of a [`VerdictErrors`], each read in place as it is reached.
#[derive(Clone)]
pub struct VerdictErrorsIter<'a> {
    at: Cursor<'a>,
    left: usize,
}

impl<'a> Iterator for VerdictErrorsIter<'a> {
    type Item = VerdictError<'a>;

    fn next(&mut self) -> Option<VerdictError<'a>> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        Some(self.at.error().expect("decode_verdict read each error of the verdict, so it reads again"))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for VerdictErrorsIter<'_> {}

/// Why bytes are no verdict [`encode_verdict`] wrote: what does not read, and at which byte of
/// the verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VerdictDecodeError {
    /// More errors than the bytes after the count could hold.
    Count { count: u64, len: usize },
    /// A span's kind byte that names no kind.
    Span { kind: u8, at: usize },
    /// An `at_signature` byte that is neither 0 nor 1.
    AtSignature(u8),
    /// A not-checked byte that names no reason.
    Why(u8),
    /// A tag that names no kind of refusal.
    Tag(u8),
    /// Bytes after the verdict's end.
    PastEnd(usize),
    /// The verdict ends at byte `at` where `more` bytes are read.
    Ends { at: usize, more: usize },
    /// A text longer than the bytes left.
    TextLen { len: u64, at: usize },
    /// A text that is not UTF-8.
    NotUtf8 { at: usize },
}

impl fmt::Display for VerdictDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            VerdictDecodeError::Count { count, len } => {
                write!(f, "an encoded verdict of {count} errors in {len} bytes")
            }
            VerdictDecodeError::Span { kind, at } => {
                write!(f, "an encoded verdict's span of kind {kind} at byte {at}")
            }
            VerdictDecodeError::AtSignature(other) => {
                write!(f, "an encoded verdict's `at_signature` of {other}")
            }
            VerdictDecodeError::Why(code) => write!(f, "an encoded verdict's why of {code}"),
            VerdictDecodeError::Tag(other) => write!(f, "an encoded verdict's tag of {other}"),
            VerdictDecodeError::PastEnd(len) => {
                write!(f, "an encoded verdict with {len} bytes past its end")
            }
            VerdictDecodeError::Ends { at, more } => {
                write!(f, "an encoded verdict ends at byte {at} where {more} more are read")
            }
            VerdictDecodeError::TextLen { len, at } => {
                write!(f, "an encoded verdict's text of {len} bytes at byte {at}")
            }
            VerdictDecodeError::NotUtf8 { at } => {
                write!(f, "an encoded verdict's text at byte {at} is not UTF-8")
            }
        }
    }
}

impl std::error::Error for VerdictDecodeError {}

/// Reads a verdict [`encode_verdict`] wrote, in place; an error naming what does not read and
/// where. Floor: one pass over the bytes, nothing allocated.
pub fn decode_verdict(bytes: &[u8]) -> Result<Verdict<'_>, VerdictDecodeError> {
    let Some((&tag, rest)) = bytes.split_first() else { return Ok(Verdict::Passed) };
    let mut at = Cursor { bytes: rest, at: 1 };
    let verdict = match tag {
        ERRORS => {
            let count = at.u64()?;
            // Each error is at least 22 bytes: a bound on the count before it is read.
            let bound = rest.len() / 22;
            if count > bound as u64 {
                return Err(VerdictDecodeError::Count { count, len: rest.len() });
            }
            let errors = VerdictErrors { at, len: count as usize };
            for _ in 0..count {
                at.error()?;
            }
            Verdict::Errors(errors)
        }
        NOT_CHECKED => {
            let code = at.byte()?;
            Verdict::NotChecked(not_checked_of(code).ok_or(VerdictDecodeError::Why(code))?)
        }
        UNSUPPORTED => Verdict::Unsupported,
        LOWERING => Verdict::Lowering,
        NOT_OF => Verdict::NotOf,
        other => return Err(VerdictDecodeError::Tag(other)),
    };
    if !at.bytes.is_empty() {
        return Err(VerdictDecodeError::PastEnd(at.bytes.len()));
    }
    Ok(verdict)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cursor<'a> {
    bytes: &'a [u8],
    /// Where `bytes` starts in the verdict, for an error.
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], VerdictDecodeError> {
        if self.bytes.len() < n {
            return Err(VerdictDecodeError::Ends { at: self.at, more: n });
        }
        let (taken, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        self.at += n;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8, VerdictDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, VerdictDecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn u64(&mut self) -> Result<u64, VerdictDecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn text(&mut self) -> Result<&'a str, VerdictDecodeError> {
        let len = self.u64()?;
        if len > self.bytes.len() as u64 {
            return Err(VerdictDecodeError::TextLen { len, at: self.at });
        }
        let at = self.at;
        std::str::from_utf8(self.take(len as usize)?).map_err(|_| VerdictDecodeError::NotUtf8 { at })
    }

    /// One error of an errors verdict.
    fn error(&mut self) -> Result<VerdictError<'a>, VerdictDecodeError> {
        let code = self.text()?;
        let kind = self.byte()?;
        let index = self.u32()?;
        let span = span_of(kind, index).ok_or(VerdictDecodeError::Span { kind, at: self.at })?;
        let at_signature = match self.byte()? {
            0 => false,
            1 => true,
            other => return Err(VerdictDecodeError::AtSignature(other)),
        };
        let message = self.text()?;
        Ok(VerdictError { code, span, at_signature, message })
    }
}

/// A span's kind byte and index in an encoded verdict.
fn span_code(span: MirSpan) -> (u8, u32) {
    match span {
        MirSpan::ExprId(it) => (0, it.into_raw().into_u32()),
        MirSpan::ExprEnd(it) => (1, it.into_raw().into_u32()),
        MirSpan::PatId(it) => (2, it.into_raw().into_u32()),
        MirSpan::BindingId(it) => (3, it.into_raw().into_u32()),
        MirSpan::SelfParam => (4, 0),
        MirSpan::Unknown => (5, 0),
        MirSpan::Inlined(n) => (6, n),
    }
}

fn span_of(kind: u8, index: u32) -> Option<MirSpan> {
    let raw = la_arena::RawIdx::from_u32(index);
    Some(match kind {
        0 => MirSpan::ExprId(la_arena::Idx::from_raw(raw)),
        1 => MirSpan::ExprEnd(la_arena::Idx::from_raw(raw)),
        2 => MirSpan::PatId(la_arena::Idx::from_raw(raw)),
        3 => MirSpan::BindingId(la_arena::Idx::from_raw(raw)),
        4 => MirSpan::SelfParam,
        5 => MirSpan::Unknown,
        6 => MirSpan::Inlined(index),
        _ => return None,
    })
}

/// Why a body was not checked, as an encoded verdict's byte: a fixed table, never the enum's
/// discriminant, so reordering [`NotChecked`] moves no byte.
pub fn not_checked_code(why: NotChecked) -> u8 {
    match why {
        NotChecked::Off => 0,
        NotChecked::TypeErrors => 1,
        NotChecked::LoweringErrors => 2,
        NotChecked::ExpansionErrors => 3,
        NotChecked::MatchErrors => 4,
        NotChecked::TailCallErrors => 5,
        NotChecked::UnsizedStatic => 6,
        NotChecked::DropInConst => 7,
        NotChecked::Unproven => 8,
    }
}

fn not_checked_of(code: u8) -> Option<NotChecked> {
    Some(match code {
        0 => NotChecked::Off,
        1 => NotChecked::TypeErrors,
        2 => NotChecked::LoweringErrors,
        3 => NotChecked::ExpansionErrors,
        4 => NotChecked::MatchErrors,
        5 => NotChecked::TailCallErrors,
        6 => NotChecked::UnsizedStatic,
        7 => NotChecked::DropInConst,
        8 => NotChecked::Unproven,
        _ => return None,
    })
}

fn mir_of<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    body: Result<&'db MirBody<'db>, MirLowerError<'db>>,
) -> Result<&'db MirBody<'db>, Refused> {
    let body = body.map_err(|error| {
        // Rendered with `db` attached: the error's types name themselves only through it.
        let error = blaze_memo::attach(db, || blaze_mem::format_in!(Global, "{error:?}"));
        Refused::Lowering { owner: def, error }
    })?;
    // MIR of `def`'s body or of a closure, coroutine or `const` block in it, which its borrow
    // check covers.
    if body.owner.expression_store_owner(db) != ExpressionStoreOwnerId::Body(def) {
        let mir_owner = blaze_mem::format_in!(Global, "{:?}", body.owner);
        return Err(Refused::NotOf { owner: def, mir_owner });
    }
    Ok(body)
}

impl<'db> Checked<'db> {
    pub fn owner(self) -> DefWithBodyId {
        self.owner
    }

    /// `body`, any MIR of the checked body or of a closure, coroutine or `const` block in it
    /// (built, runtime, optimized, monomorphized), for a build's codegen.
    pub fn mir(
        self,
        db: &'db Db,
        body: Result<&'db MirBody<'db>, MirLowerError<'db>>,
    ) -> Result<CheckedMir<'db>, Refused> {
        Ok(CheckedMir { owner: self, body: mir_of(db, self.owner, body)? })
    }
}

/// MIR whose borrow check passed: what a build's codegen compiles.
#[derive(Clone, Copy, Debug)]
pub struct CheckedMir<'db> {
    owner: Checked<'db>,
    body: &'db MirBody<'db>,
}

impl<'db> CheckedMir<'db> {
    pub fn checked(self) -> Checked<'db> {
        self.owner
    }

    pub fn body(self) -> &'db MirBody<'db> {
        self.body
    }
}

impl<'db> std::ops::Deref for CheckedMir<'db> {
    type Target = MirBody<'db>;

    fn deref(&self) -> &MirBody<'db> {
        self.body
    }
}

/// The live-patch path's entry point: `def`'s code may be compiled through the [`Unchecked`]
/// now, and the patch that runs it must be rolled back unless the [`Pending`] check finishes
/// without error. Never for a build's output.
pub fn speculate(def: DefWithBodyId) -> (Unchecked, Pending) {
    (Unchecked { owner: def }, Pending { owner: def })
}

/// A body whose code the live-patch path compiles before its borrow check finishes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unchecked {
    owner: DefWithBodyId,
}

impl Unchecked {
    pub fn owner(self) -> DefWithBodyId {
        self.owner
    }

    /// `body`, any MIR of the body or of a closure, coroutine or `const` block in it, for the
    /// live patch's codegen.
    pub fn mir<'db>(
        self,
        db: &'db Db,
        body: Result<&'db MirBody<'db>, MirLowerError<'db>>,
    ) -> Result<UncheckedMir<'db>, Refused> {
        Ok(UncheckedMir { owner: self, body: mir_of(db, self.owner, body)? })
    }
}

/// MIR whose borrow check has not finished: what the live patch's codegen compiles.
#[derive(Clone, Copy, Debug)]
pub struct UncheckedMir<'db> {
    owner: Unchecked,
    body: &'db MirBody<'db>,
}

impl<'db> UncheckedMir<'db> {
    pub fn unchecked(self) -> Unchecked {
        self.owner
    }

    pub fn body(self) -> &'db MirBody<'db> {
        self.body
    }
}

impl<'db> std::ops::Deref for UncheckedMir<'db> {
    type Target = MirBody<'db>;

    fn deref(&self) -> &MirBody<'db> {
        self.body
    }
}

/// The borrow check a live patch waits on. It names no revision of the database, so it can be
/// sent to the task that finishes it.
#[derive(PartialEq, Eq, Debug)]
#[must_use = "a live patch is rolled back unless its borrow check finishes without error"]
/// Linear (`hir-ty-linear`): a `Pending` dropped without [`Pending::finish`] is a patch whose
/// rollback is never decided, so dropping one is a compile error under blaze, not a warning.
#[blaze::linear]
pub struct Pending {
    owner: DefWithBodyId,
}

impl Pending {
    pub fn owner(&self) -> DefWithBodyId {
        self.owner
    }

    /// Runs the borrow check, or takes its result if it ran: `Err` means the patch must be
    /// rolled back.
    #[blaze::consumes]
    pub fn finish(self, db: &Db) -> Result<Checked<'_>, Refused> {
        checked(db, self.owner)
    }
}
