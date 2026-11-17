//! What the check decided, at each call, for every projection the callee's signature names
//! (piece impl-per-call): the impl that gives `<T as Tr>::Out` its value there, or why none
//! does. Kept in the record ([`crate::Checked::selected`]) so the borrow check reads the
//! selected impl's `type Out = ..;` text at the call (blaze-flow-lower impl_call.rs) instead of
//! refusing the call (`Form::CallHiddenInput`) or selecting again itself (rule 77).
//!
//! Where a row comes from: lowering a callee's signature at a call (`Lower::Args` with
//! `Cause::Expr(call)`: rules/call.rs, rules/method.rs) turns each written projection into a
//! fresh variable and a projection obligation (machine.rs `lower_at`, the `PNode::Alias` arm);
//! the same arm notes `(call, trait, name, args)`. At writeback the arguments are resolved and
//! exported, and when they are ground the impl is chosen by the one selection home for ground
//! arguments, `instance::select_in` (the selection the code builder's `resolve_instance` makes),
//! over the BODY's own `Reads`: every world fact it consults is on the body's read list (rule
//! 21), and an impl's facts were already read by the solver when it normalized the obligation.
//!
//! The Reference, "Implementations", "Trait implementations": at a call with type arguments
//! fixed, "the" implementation of `Tr` for `T` is the one that applies (coherence makes it
//! unique; `min_specialization` picks the most specific: instance.rs); "Associated Items",
//! "Associated types": its `type Out = U;` defines `<T as Tr>::Out` as `U`.

use crate::ids::{ExprId, ItemKey, List, Name};

/// How one projection of a callee's signature was decided at one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selected {
    /// The impl `imp` at its arguments `args` (a list of the record's `types`): its own
    /// `type Name` member is the projection's value at this call.
    Impl { imp: ItemKey, args: List },
    /// The projection's arguments name the body's own generics (or an alias or opaque type):
    /// a where-clause of the body's environment or a generic impl decides it per instance, not
    /// the call. The borrow check keeps refusing such a call by name.
    Generic,
    /// The arguments are ground and no impl holds: the solver proved the projection by a
    /// builtin rule or a global where-clause of the body; no impl text gives its value.
    NotImpl,
    /// Selecting needed a world fact the check had not read (`ResolveError::Missing`); not
    /// decided here, never guessed: the borrow check refuses the call by name.
    Unanswered,
}

/// One projection of a callee's signature at one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Projected {
    /// The call or method call.
    pub expr: ExprId,
    /// Its ordinal among this call's rows, in the order the signature was lowered (the inputs
    /// left to right, then the output; within one written type, as `lower_at` meets it): the
    /// order the borrow check's reader numbers a signature's hidden positions (`Region::Hidden`).
    pub at: u32,
    pub trait_: ItemKey,
    pub name: Name,
    /// `[Self, trait args..]` at the call, a list of the record's `types`.
    pub args: List,
    pub selected: Selected,
}

/// The most rows one body keeps (rule 18): past it the check stops with `Limit::Solver`.
pub(crate) const MOST_PROJECTED: usize = 1 << 16;

/// The most nodes the groundness walk (rules/writeback/selected_rows.rs) visits in one row's
/// arguments (rule 18): past it the check stops with `Limit::Solver`.
pub(crate) const MOST_NODES: u32 = 1 << 16;
