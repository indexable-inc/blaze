//! A type-checked body to LIR (crates/blaze-cli/tests/ladder/README.md, step (b)): one body in
//! blaze-typeck's form ([`BodyView`]), its check's record ([`Checked`]) and its owner's
//! signature ([`FnSig`]) to one LIR function ([`blaze_lir::Func`]), verified.
//!
//! The function it makes is the baseline tier's input: every local and every value that
//! outlives its expression lives in a LIR stack slot (`slot_addr`, `load`, `store`); `if`,
//! `match`, `loop`, `break`, `continue` and `return` are blocks and branches; a call of a fn
//! item is a `call` of function reference `place`, where `refs[place]` is the callee's item.
//! A Rust-ABI function uses the target's C convention over its lowered signature (blaze-lir's
//! `Conv`). The walk of the body uses explicit stacks: no recursion on the input.
//!
//! Anything outside the subset is [`BuildError::Unsupported`], naming the expression and the
//! construct: never a silent skip (AGENTS.md rule 3).
//!
//! # Values
//!
//! An integer is its width (`isize`/`usize` are `i64`: the target's pointers are 64 bits), a
//! `bool` an `i8` of 0 or 1, a reference or raw pointer a `ptr`; `()`, `!` and a fn item have
//! no value. A struct value is a place: a pointer and an offset into a stack slot, its fields
//! in declaration order at their natural alignment (the ladder's shortcut 8), copied field by
//! field where it is stored. Code after a diverging expression is built into blocks no edge
//! reaches, where a value a later expression needs is `uninit`.

#![feature(allocator_api)]

mod walk;

#[cfg(test)]
mod tests;

use core::alloc::Allocator;
use core::fmt;

use blaze_lir::{Func, VerifyError};
use blaze_typeck::{Adt, BodyView, Checked, ExprId, FnSig, ItemKey, PatId};

/// What the build reads from outside the body form: the structs a body's types name (their
/// fields, for layout), and the values of the body's literals, which blaze-typeck's body form
/// does not hold (its `Lit::Int` keeps the suffix, `Lit::ByteStr` the length).
///
/// The literal answers default to none, which the build refuses with
/// [`BuildError::NoLiteralValue`] naming the literal: a lowering that makes literals answers
/// them, until the body form carries the values itself.
pub trait Adts {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>>;

    /// The value of integer literal `expr` of the body of `owner`, as written (unsigned; a
    /// negative literal is a `Unary { op: Neg }` of it).
    fn int_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
        let _ = (owner, expr);
        None
    }

    /// The magnitude of integer literal pattern `pat` (`Pat::Lit` or `Pat::NegLit`) of the body
    /// of `owner`; the build negates a `NegLit`.
    fn int_pat(&self, owner: ItemKey, pat: PatId) -> Option<u128> {
        let _ = (owner, pat);
        None
    }

    /// The bytes of byte string literal `expr` of the body of `owner`.
    fn byte_str(&self, owner: ItemKey, expr: ExprId) -> Option<&[u8]> {
        let _ = (owner, expr);
        None
    }
}

/// One built function.
pub struct Built<A: Allocator> {
    /// The function, verified.
    pub func: Func<A>,
    /// By function reference place: the callee's item.
    pub refs: Vec<ItemKey, A>,
}

/// Why a body was not built.
#[derive(Debug)]
pub enum BuildError {
    /// An expression outside the subset.
    Unsupported { expr: ExprId, what: &'static str },
    /// A pattern outside the subset.
    UnsupportedPat { pat: PatId, what: &'static str },
    /// A type outside the subset, met at `expr`.
    UnsupportedType { expr: ExprId, what: &'static str },
    /// The owner's signature is outside the subset (`what` names the part).
    UnsupportedSig { what: &'static str },
    /// The body or its record is not one the checker makes (a bad index, a missing type).
    Malformed { what: &'static str },
    /// The check had diagnostics: a body with type errors is not built.
    Diagnostics { count: u32 },
    /// [`Adts`] gave no value for this literal expression.
    NoLiteralValue { expr: ExprId },
    /// [`Adts`] gave no value for this literal pattern.
    NoPatValue { pat: PatId },
    /// blaze-lir's builder refused a use of it: this crate's bug, named.
    Lir(blaze_lir::BuildError),
    /// The built function failed blaze-lir's verifier: this crate's bug, named.
    Verify(VerifyError),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Unsupported { expr, what } => write!(f, "expression {}: {what} is not built to LIR yet", expr.0),
            BuildError::UnsupportedPat { pat, what } => write!(f, "pattern {}: {what} is not built to LIR yet", pat.0),
            BuildError::UnsupportedType { expr, what } => {
                write!(f, "expression {}: a value of {what} is not built to LIR yet", expr.0)
            }
            BuildError::UnsupportedSig { what } => write!(f, "the signature: {what} is not built to LIR yet"),
            BuildError::Malformed { what } => write!(f, "a malformed body or record: {what}"),
            BuildError::Diagnostics { count } => write!(f, "the body has {count} type errors"),
            BuildError::NoLiteralValue { expr } => {
                write!(f, "expression {}: the build's `Adts` gave no value for this literal", expr.0)
            }
            BuildError::NoPatValue { pat } => {
                write!(f, "pattern {}: the build's `Adts` gave no value for this literal", pat.0)
            }
            BuildError::Lir(error) => write!(f, "blaze-lir's builder refused the function: {error}"),
            BuildError::Verify(error) => write!(f, "the LIR built does not verify: {error}"),
        }
    }
}

impl core::error::Error for BuildError {}

/// Builds `body`, checked as `checked`, of a fn whose signature is `sig`, in `alloc`.
pub fn build<A: Allocator + Clone>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    sig: &FnSig<'_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    if !checked.diags.is_empty() {
        return Err(BuildError::Diagnostics { count: u32::try_from(checked.diags.len()).unwrap_or(u32::MAX) });
    }
    walk::build(body, checked, sig, adts, alloc)
}
