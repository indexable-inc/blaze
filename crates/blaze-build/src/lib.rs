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

#![feature(allocator_api)]

use core::alloc::Allocator;
use core::fmt;

use blaze_lir::{Func, VerifyError};
use blaze_typeck::{Adt, BodyView, Checked, ExprId, FnSig, ItemKey, PatId};

/// The structs a body's types name: their fields, for layout.
pub trait Adts {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>>;
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
    /// The body or its record is not one the checker makes (a bad index, a missing type).
    Malformed { what: &'static str },
    /// The check had diagnostics: a body with type errors is not built.
    Diagnostics { count: u32 },
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
            BuildError::Malformed { what } => write!(f, "a malformed body or record: {what}"),
            BuildError::Diagnostics { count } => write!(f, "the body has {count} type errors"),
            BuildError::Verify(error) => write!(f, "the LIR built does not verify: {error:?}"),
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
    let _ = (body, checked, sig, adts, alloc);
    Err(BuildError::Unsupported { expr: ExprId(0), what: "every body (the builder is being written)" })
}
