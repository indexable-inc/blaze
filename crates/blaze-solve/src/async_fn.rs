//! The builtin `AsyncFn`, `AsyncFnMut` and `AsyncFnOnce` impls of a callable type, and the
//! normalization of their three associated types (docs: port27/tempo-run/ASYNC.md D-A9, prompt
//! P9): the reference's `consider_builtin_async_fn_trait_candidates` (trait_goals.rs and
//! normalizes_to.rs) over `extract_tupled_inputs_and_output_from_async_callable`
//! (structural_traits.rs), for fn items, fn pointers and closures.
//!
//! The pinned library declares the traits (library/core/src/ops/async_function.rs):
//!
//! ```text
//! #[lang = "async_fn"]
//! pub trait AsyncFn<Args: Tuple>: AsyncFnMut<Args> {
//!     extern "rust-call" fn async_call(&self, args: Args) -> Self::CallRefFuture<'_>;
//! }
//! #[lang = "async_fn_mut"]
//! pub trait AsyncFnMut<Args: Tuple>: AsyncFnOnce<Args> {
//!     #[lang = "call_ref_future"]
//!     type CallRefFuture<'a>: Future<Output = Self::Output> where Self: 'a;
//!     extern "rust-call" fn async_call_mut(&mut self, args: Args) -> Self::CallRefFuture<'_>;
//! }
//! #[lang = "async_fn_once"]
//! pub trait AsyncFnOnce<Args: Tuple> {
//!     #[lang = "call_once_future"]
//!     type CallOnceFuture: Future<Output = Self::Output>;
//!     #[lang = "async_fn_once_output"]
//!     type Output;
//!     extern "rust-call" fn async_call_once(self, args: Args) -> Self::CallOnceFuture;
//! }
//! ```
//!
//! The rule (the closure-kind rule, the `Fn*` traits' sibling in builtin.rs
//! `assemble_fn_trait`): a callable type whose kind extends the goal's kind (a fn item or fn
//! pointer is `Fn`; a closure's kind is the one its captures' uses settle) and whose output is a
//! `Future` implements the async trait of that kind; its `CallOnceFuture` and `CallRefFuture` are
//! that output, its `Output` the output's `Future::Output`. An `async` closure is such a closure:
//! the checker types it as a closure whose output is its body's future (blaze-typeck
//! rules/closure.rs `open_async_closure`), so the one rule covers `async || ..` and
//! `|| async { .. }` alike, as the reference's does for a closure returning a future.
//!
//! Deviation, stated: the reference's `CoroutineClosure` (a true async closure) may LEND its
//! captures to the future it returns (the future borrows the closure through
//! `CallRefFuture<'a>`). Here the future of an async closure is one type for every call (its
//! captures are the closure's, `Future(e, ..)`), and lifetimes are not part of the solver's
//! terms, so the lending form and the non-lending form are the same term; borrowck of the
//! call (blaze-flow) is what keeps two lent futures from being live at once.

use blaze_own::{Alloc, Vec};

use crate::assembly::{BuiltinSource, Cand, Cx, Rule as CandRule, Step};
use crate::builtin::{self, Callable, ClosureKind, Rule};
use crate::error::Stop;
use crate::facts::Facts;
use crate::term::{AliasKind, Certainty, GoalSource, Lang, List, Polarity, Rigid, Ty, TyKind};
use crate::trait_goals::TraitGoal;
use crate::tys::Tys;

/// One of the three associated types the async traits declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Assoc {
    /// `AsyncFnOnce::Output` (`async_fn_once_output`): the future's `Future::Output`.
    Output,
    /// `AsyncFnOnce::CallOnceFuture` (`call_once_future`): the callable's output.
    CallOnceFuture,
    /// `AsyncFnMut::CallRefFuture<'a>` (`call_ref_future`): the callable's output.
    CallRefFuture,
}

impl Assoc {
    /// Every associated type, in declaration order (the order candidates are tried in).
    pub const ALL: [Assoc; 3] = [Assoc::Output, Assoc::CallOnceFuture, Assoc::CallRefFuture];

    /// The lang item naming this associated type.
    pub fn lang(self) -> Lang {
        match self {
            Assoc::Output => Lang::AsyncFnOnceOutput,
            Assoc::CallOnceFuture => Lang::CallOnceFuture,
            Assoc::CallRefFuture => Lang::CallRefFuture,
        }
    }

    /// The kind of the trait that declares this associated type (the reference's
    /// `async_fn_trait_kind_from_def_id` of the item's trait): `CallRefFuture` is
    /// `AsyncFnMut`'s, so the callable must be at least `FnMut`; the other two are
    /// `AsyncFnOnce`'s.
    pub fn trait_kind(self) -> ClosureKind {
        match self {
            Assoc::Output | Assoc::CallOnceFuture => ClosureKind::FnOnce,
            Assoc::CallRefFuture => ClosureKind::FnMut,
        }
    }
}

/// The closure kind of an async trait's lang item; `None` for every other lang item.
pub fn goal_kind(l: Lang) -> Option<ClosureKind> {
    match l {
        Lang::AsyncFn => Some(ClosureKind::Fn),
        Lang::AsyncFnMut => Some(ClosureKind::FnMut),
        Lang::AsyncFnOnce => Some(ClosureKind::FnOnce),
        _ => None,
    }
}

/// `consider_builtin_async_fn_trait_candidates` (trait_goals.rs): a callable type of a kind
/// extending `goal_kind` has one candidate (its step: `step_async_fn_trait`); an unknown
/// closure kind is a forced ambiguity unless the goal is `AsyncFnOnce` (builtin.rs `callable`);
/// a type that is not callable has none. `c` is `Tuple([inputs, output])`.
pub(crate) fn assemble_async_fn_trait<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    g: TraitGoal,
    self_ty: Ty,
    goal_kind: ClosureKind,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<(), Stop> {
    if g.polarity != Polarity::Positive {
        return Ok(());
    }
    match builtin::callable(cx, self_ty, goal_kind)? {
        Callable::No => {}
        Callable::Ambiguous => out.push(builtin::certain(Certainty::Ambiguous)),
        Callable::Sig(inputs, output) => {
            let packed = builtin::pack_args(cx, g.args, self_ty)?;
            let io = cx.mk_list(&[inputs, output])?;
            let c = cx.mk(TyKind::Tuple(io))?;
            let row = builtin::seeded(
                builtin::cand(BuiltinSource::Misc, CandRule::Builtin(Rule::AsyncFnTrait(goal_kind))),
                self_ty,
                packed,
                g.trait_,
            );
            out.push(Cand { c, ..row });
        }
    }
    Ok(())
}

/// The step of an `AsyncFnTrait` candidate: the goal's `Args` equal the callable's tupled
/// inputs (`match_assumption`), and the output is a `Future` (the nested goal the reference's
/// `extract_tupled_inputs_and_output_from_async_callable` returns for a fn item, a fn pointer
/// and a closure, added as `ImplWhereBound`).
pub(crate) fn step_async_fn_trait<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand) -> Result<Step, Stop> {
    let io = builtin::tuple_list(cx, cand.c)?;
    let args = builtin::tuple_list(cx, cand.b)?;
    if io.len != 2 || args.len != 2 {
        return Err(builtin::malformed());
    }
    let (inputs, output) = (cx.arg(io, 0), cx.arg(io, 1));
    if !cx.eq(cx.arg(args, 1), inputs)? {
        return Ok(Step::No);
    }
    let future = builtin::require_lang(cx, Lang::Future)?;
    let p = builtin::trait_pred(cx, future, &[output])?;
    cx.add(GoalSource::ImplWhereBound, p);
    Ok(Step::Done(Certainty::Yes))
}

/// `consider_builtin_async_fn_trait_candidates` for a `NormalizesTo` goal of one of the three
/// associated types (normalizes_to.rs): the callable's tupled inputs and output at the kind of
/// the item's trait, the goal's `Args` (its argument after `Self`) equated with the inputs, the
/// term equated with the value, and the output's `Future` bound required (`ImplWhereBound`).
/// `CallOnceFuture` and `CallRefFuture` are the output; `Output` is the output's
/// `Future::Output`: an async coroutine's own output (its `args[0]`, builtin.rs
/// `normalize_future_output`), else the projection `<output as Future>::Output`, which relating
/// the term normalizes.
pub(crate) fn normalize<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    self_ty: Ty,
    goal_args: List,
    term: Ty,
    which: Assoc,
) -> Result<Step, Stop> {
    let (inputs, output) = match builtin::callable(cx, self_ty, which.trait_kind())? {
        Callable::No => return Ok(Step::No),
        Callable::Ambiguous => return Ok(Step::Done(Certainty::Ambiguous)),
        Callable::Sig(i, o) => (i, o),
    };
    // `Self`, `Args`, and for `CallRefFuture<'a>` its lifetime where the terms keep one.
    if goal_args.len < 2 {
        return Err(builtin::malformed());
    }
    if !cx.eq(cx.arg(goal_args, 1), inputs)? {
        return Ok(Step::No);
    }
    let value = match which {
        Assoc::CallOnceFuture | Assoc::CallRefFuture => output,
        Assoc::Output => match cx.kind(output) {
            TyKind::Coroutine(_, a) => {
                if a.len < 2 {
                    return Err(builtin::malformed());
                }
                cx.arg(a, 0)
            }
            _ => {
                let fo = builtin::require_lang(cx, Lang::FutureOutput)?;
                let l = cx.mk_list(&[output])?;
                cx.mk(TyKind::Alias(Rigid::No, AliasKind::Projection, fo, l))?
            }
        },
    };
    if !cx.eq(term, value)? {
        return Ok(Step::No);
    }
    let future = builtin::require_lang(cx, Lang::Future)?;
    let p = builtin::trait_pred(cx, future, &[output])?;
    cx.add(GoalSource::ImplWhereBound, p);
    Ok(Step::Done(Certainty::Yes))
}
