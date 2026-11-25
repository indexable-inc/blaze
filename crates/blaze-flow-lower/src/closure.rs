//! A closure's body as its own flow, and an `async fn`'s body with its suspensions (the move and
//! borrow check's two nested forms: wiring/blaze-ladder/borrows-closures-async-1.md).
//!
//! The Reference, "Closure expressions": "A closure expression ... captures variables from the
//! environment"; "Capture modes": "The compiler prefers to capture a closed-over variable by
//! immutable borrow, followed by unique immutable borrow, by mutable borrow, and finally by
//! move"; "Closure types": the closure is "roughly" a struct of its captures, and "Call traits
//! and coercions": an `Fn` closure's body reads its environment through `&self`, an `FnMut`'s
//! through `&mut self`, an `FnOnce`'s owns it (`self`). The owner's side (each capture a borrow
//! or a move at the closure expression, held in the closure value's own positions, so live as
//! long as the value) is the lowering's `closure_value`; this module lowers the BODY, the type
//! checker's capture rows (`Checked::captures`) and the closure's settled kind
//! (`Checked::closures`) deciding each capture's argument:
//!
//! - by reference (`ByRef`, `ByMutRef`): an argument `&T` / `&mut T`, and the captured variable
//!   is the place `*arg` (the same form an async block's body takes, coroutine.rs `run_async`);
//! - by value in an `FnOnce` closure: an argument `T`, the variable itself, moved and dropped as
//!   the body's own;
//! - by value in an `Fn` or `FnMut` closure: an argument `&T` or `&mut T` to the environment's
//!   field, the variable the place `*arg`: a move out of it is a move out of a place behind a
//!   reference (E0507, "cannot move out of a captured variable in an `Fn(Mut)` closure"), and it
//!   is not dropped by a call.
//!
//! The closure's parameters follow the captures as arguments; its return place is the
//! signature's output, the body's value assigned to it. Every position is a placeholder. Known
//! among them: each argument's implied bounds (`&'a T` is well formed only when `T: 'a`, the
//! Reference, "Lifetime bounds"), and that two positions the expecting clause binds to one
//! variable are one region. A relation the body needs beyond these is the owner's to prove (its
//! closure requirements, the NLL RFC, "Closures"): [`lower_closure_external`] says what each
//! placeholder is in the owner ([`External`]); an outlives result between two of the closure
//! VALUE's positions (or `'static`) becomes a [`Requirement`] the owner's flow emits at the
//! closure expression ([`crate::Facts::closure_requirements`]); one that names the
//! environment's reference, a region the expecting clause binds, or a signature not read
//! ([`requirement`] gives `None`) is still filed as a refusal, never as a finding.
//!
//! THE SIGNATURE'S REGIONS (D148, docs/v2/plan/design/flow.md "Lifetimes are produced once and
//! carried beside the types", piece L5). The type check records WHERE a closure's signature was
//! expected from (`Checked::closures` column 4, `SigFrom`), never a copy of regions; this module
//! reads the region column of the expecting clause (`Lower::sig_slots`):
//!
//! - a slot the clause's `for<..>` binds (`Late`, `Fn(&T)`'s elided input and its output) is a
//!   UNIVERSAL region of the closure's body ([`SigSlot::Late`], [`External::Late`]): the body
//!   must hold for every choice of it, and a relation it needs on it is refused, never pushed
//!   on the owner (rustc reports those inside the closure: "borrowed data escapes");
//! - every other position of a clause's part (under a type parameter of the clause, a
//!   lifetime parameter of the callee, `'static`), and the output of a signature no clause
//!   expected, is the OWNER's: a position of the closure VALUE after its captures'
//!   ([`SigSlot::Owner`], [`External::SigOwner`], [`Need::Sig`]), one region per closure value,
//!   as rustc's closure type carries its signature's free regions in its substs. The owner ties
//!   them where it uses the value: a direct call relates each argument and the result with them
//!   (`Lower::closure_call_sig`, callee_value.rs); a generic call that takes the closure as a
//!   type argument relates them with the call's other type arguments and the regions the
//!   callee's clauses write (`Lower::tie_closure_sigs`, generic_call.rs), a superset of the
//!   clause's own relation (more demanded, never less) since the column holds no type;
//! - the inputs of a signature no clause expected (written, inferred, or a trait object's or a
//!   fn pointer's, not read yet) are universal, each a region of its own: a written elided
//!   `&T` is bound per call (rustc: late-bound), and the type check does not hold which
//!   inferred region a call this body does not see may choose.
//!
//! A requirement on an owner position holds only where every call binds the position to the
//! value's region. Where a call may not (a callee with a binder clause the closure was not
//! expected by, a coercion to a trait object or a fn pointer, an opaque return type) a closure
//! with such a requirement is refused by name (`Form::ClosureSigHigherRanked`,
//! `Form::ClosureSigErased`), never accepted.
//!
//! A by-reference capture is a borrow made at the closure expression whose reference is a
//! position of the closure value (lib.rs `closure_value`), so the loan is live wherever the
//! VALUE is: `let read = || v; v = 1; read();` is E0506 at `v = 1` (the call reads the value).
//!
//! An `async fn` (the Reference, "Async functions": "an async fn ... returns a future"; its body
//! runs when the future is polled, its parameters moved into it): the fn's body lowered as a fn
//! body, `.await` allowed in it (coroutine.rs `await_done`: each a suspension whose loans stay
//! live through it), the return place the written output.
//!
//! No recursion: the walk is the lowering's task stack.

use blaze_flow::{FlowBuilder, Local, Origin, Place, Proj};
use blaze_own::Vec;
use blaze_typeck::{BodyView, CaptureKind, Checked, ClosureKind, Expr, ExprId, ItemKey, Mutbl, PNode, PTy, Pat, PatId, SigFrom};
use core::alloc::Allocator;

use crate::{At, ClosureSigWhy, Facts, Form, Lower, Lowered, MOST_POSITIONS, Refusal, Region, Task, TyD, Val, range_of};

/// The body of closure `closure` (an `Expr::Closure` of `body`, not `async`) as its own flow:
/// its captures, then its parameters, as the arguments (the module doc).
pub fn lower_closure<A: Allocator + Copy, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    closure: ExprId,
    facts: &F,
    alloc: A,
) -> Result<Lowered<A>, Refusal> {
    lower_closure_external(body, ck, closure, facts, alloc).map(|c| c.lowered)
}

/// What a placeholder of a closure's or an async block's flow stands for in the flow that makes
/// it (its OWNER: the fn body, or the closure or async block whose body holds the expression).
/// One table for both (lane B14): an async block's captures are laid as a closure's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum External {
    /// Position `j` of the closure VALUE, counted as the owner lays it (`Lower::npos` of the
    /// closure type: each capture in row order, a capture by reference its reference first,
    /// then the captured variable's type's positions).
    Capture(u32),
    /// `'static`.
    Static,
    /// The environment's own reference (`&'env self` of an `Fn`/`FnMut`, and the reference an
    /// `FnMut` reaches a `&mut` capture through): bound per call, no position of the value.
    Env,
    /// For an async block (coroutine.rs `LoweredAsync::externals`), a position of its output:
    /// the future's type holds no position for it (see [`requirement`]).
    Sig,
    /// Signature position `k` the OWNER binds ([`SigSlot::Owner`]): position `k` of the
    /// closure value after its captures' (`Lower::npos` of the closure type), so a relation the
    /// body needs on it is the owner's to prove ([`Need::Sig`]).
    SigOwner(u32),
    /// A signature position no owner region stands for ([`SigSlot::Late`]): a region the
    /// expecting clause's `for<..>` binds, or one every call may choose. A universal region of
    /// the body: a relation the body needs on it is refused, never pushed on the owner.
    Late,
    /// A universal lifetime parameter `Region::Param(i)` of the enclosing fn (used when a
    /// closure body proves a callee bound through an enclosing `where`-clause naming `'a`).
    OwnerParam(u32),
    /// A type parameter outlives placeholder `ρ_P` of the enclosing fn (`param_origs`).
    Param(u32),
    /// The `ordinal`-th canonical higher-ranked `dyn for<..>` placeholder (`dyn_bound_placeholders`).
    DynPlaceholder(u32),
}

/// What one position of a closure's signature (its inputs' positions, then its output's, as
/// `Lower::npos` lays each) is (`Lower::sig_slots`, the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigSlot {
    /// The owner's: position `k` among the value's signature positions.
    Owner(u32),
    /// Universal: `Some((binder, var))` a variable of the expecting clause's binder (two
    /// positions of one variable are one region), `None` a region of its own.
    Late(Option<(u32, u32)>),
    /// Explicitly written `'static` on a closure parameter (`|lock: &'static str|`).
    Static,
    /// Bound by an inner higher-ranked binder inside a parameter or output type (such as
    /// `dyn for<'d> FnMut(&'d dyn Display)` or `for<'d> fn(&'d T)`), NOT by the closure's own
    /// call binder: laid as an existential origin in the closure body, like `universal(Late)` in
    /// a top-level fn.
    InnerBound(u32, u32),
}

/// A closure's flow and what each of its placeholders stands for in the owner.
pub struct ClosureFlow<A: Allocator + Copy> {
    pub lowered: Lowered<A>,
    /// `(placeholder, what it stands for)`, one row per placeholder the walk made.
    pub externals: Vec<(blaze_flow::Origin, External), A>,
}

/// One end of a [`Requirement`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Need {
    /// Position `j` of the closure value ([`External::Capture`]).
    Capture(u32),
    Static,
    /// Signature position `k` of the closure value, after its captures' ([`External::SigOwner`]).
    Sig(u32),
    /// Universal lifetime parameter `Region::Param(i)` of the enclosing fn ([`External::OwnerParam`]).
    OwnerParam(u32),
    /// Type parameter outlives placeholder `ρ_P` of the enclosing fn ([`External::Param`]).
    Param(u32),
    /// Canonical higher-ranked `dyn for<..>` placeholder `ordinal` ([`External::DynPlaceholder`]).
    DynPlaceholder(u32),
}

impl Need {
    /// Whether this end is a signature position ([`Need::Sig`]): a requirement on one holds
    /// only where every call binds it to the value's region (closure.rs `sig_bound_here`).
    pub fn is_sig(self) -> bool {
        matches!(self, Need::Sig(_))
    }
}

/// A closure requirement (the NLL RFC, "Closures": "the closure's creator ... must prove" the
/// relations among the closure's external regions that the closure body needs and its own
/// signature does not give): `from` outlives `into`, both regions of the closure VALUE's type
/// (or `'static`), proven where the closure is made. The owner's lowering emits it as
/// `subset(from, into)` at the closure expression ([`crate::Facts::closure_requirements`]): a
/// requirement the owner cannot prove is the owner's finding, at the closure's range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Requirement {
    pub from: Need,
    pub into: Need,
}

/// The requirement an outlives result of the closure's flow (`UnprovenOutlives { from, into }`:
/// `from: into` needed between two placeholders, not known) puts on its owner, or `None` when it
/// cannot be stated there: an end is the environment's reference (bound per call), a universal
/// signature region ([`External::Late`]: the body must hold for every choice of it), an async
/// block's output ([`External::Sig`]), or a placeholder this flow did not name. `None` stays
/// `NestedOutlives`: not checked, never accepted.
pub fn requirement(externals: &[(blaze_flow::Origin, External)], from: blaze_flow::Origin, into: blaze_flow::Origin) -> Option<Requirement> {
    let end = |o: blaze_flow::Origin| match externals.iter().find(|(x, _)| *x == o).map(|&(_, w)| w) {
        Some(External::Capture(j)) => Some(Need::Capture(j)),
        Some(External::Static) => Some(Need::Static),
        Some(External::SigOwner(k)) => Some(Need::Sig(k)),
        Some(External::OwnerParam(i)) => Some(Need::OwnerParam(i)),
        Some(External::Param(u)) => Some(Need::Param(u)),
        Some(External::DynPlaceholder(ord)) => Some(Need::DynPlaceholder(ord)),
        Some(External::Env | External::Sig | External::Late) | None => None,
    };
    Some(Requirement { from: end(from)?, into: end(into)? })
}

/// Whether an outlives result `from: into` of a closure's flow is rustc's E0521, "borrowed data
/// escapes outside of closure". `from` must be a region the expecting clause's `for<..>` binds
/// ([`External::Late`]): it is chosen per call, so it may be shorter than any region the owner
/// names. `into` must be a region the owner names: a capture's, a signature position the owner
/// binds, or `'static`. rustc (region_infer `try_propagate_universal_region_error`) moves such a
/// result to the owner only through a non-local region the local one is known to outlive
/// (`non_local_lower_bound`); with none, it is an error in the closure. `known` is the flow's
/// stated relations (`Flow::known`, `(a, b)` for `a: b`). A `from` known to outlive some owner
/// region is not this: that move is not made here, and the caller refuses it by name. The walk
/// visits each origin of `known` at most once.
///
/// Example: `fn on<F: FnMut(&dyn E)>(f: F)` and `on(|e| { keep = Some(e); })` with a captured
/// `keep: Option<&dyn E>`: `e`'s region (Late) must outlive the capture's. Nothing states it, so
/// this is E0521 (tempo's shape: front2_closure_sigs.rs `user_keeps`).
pub fn escapes_closure<A: Allocator>(
    externals: &[(Origin, External)],
    known: &[(Origin, Origin)],
    from: Origin,
    into: Origin,
    alloc: A,
) -> bool {
    let end = |o: Origin| externals.iter().find(|(x, _)| *x == o).map(|&(_, w)| w);
    let owner_names = |w: Option<External>| {
        matches!(w, Some(External::Capture(_) | External::SigOwner(_) | External::Static | External::OwnerParam(_) | External::Param(_) | External::DynPlaceholder(_)))
    };
    if end(from) != Some(External::Late) || !owner_names(end(into)) {
        return false;
    }
    let mut reached: Vec<Origin, A> = Vec::new_in(alloc);
    reached.push(from);
    let mut i = 0;
    while i < reached.len() {
        let cur = reached[i];
        i += 1;
        for &(a, b) in known {
            if a == cur && !reached.contains(&b) {
                if owner_names(end(b)) {
                    return false;
                }
                reached.push(b);
            }
        }
    }
    true
}

/// [`lower_closure`], with what each placeholder of the flow stands for in the owner
/// ([`ClosureFlow::externals`]), so the flow's outlives results among its captures become the
/// owner's requirements ([`requirement`]).
pub fn lower_closure_external<A: Allocator + Copy, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    closure: ExprId,
    facts: &F,
    alloc: A,
) -> Result<ClosureFlow<A>, Refusal> {
    lower_closure_with::<A, F, crate::one_call::Production>(body, ck, closure, facts, false, alloc, &mut None)
}

/// [`lower_closure_external`] under the call rule `R`; with `logged` (the oracle's comparing
/// lowering, lib.rs `lower_with`) each call's record into `log`, kept on a refusal too.
pub(crate) fn lower_closure_with<A: Allocator + Copy, F: Facts, R: crate::one_call::CallRule>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    closure: ExprId,
    facts: &F,
    logged: bool,
    alloc: A,
    log: &mut Option<crate::builder::Log<A>>,
) -> Result<ClosureFlow<A>, Refusal> {
    let range = range_of(body, closure);
    let (params, inner) = match body.exprs.get(closure.ix()) {
        Some(Expr::Closure { is_async: true, .. }) => return Err(Refusal { form: Form::AsyncClosure, range }),
        Some(Expr::Closure { params, body: inner, .. }) => (*params, *inner),
        _ => return Err(Refusal { form: Form::Malformed, range }),
    };
    let params: &[PatId] = match body.pat_lists.get(params.range()) {
        Some(p) => p,
        None => return Err(Refusal { form: Form::Malformed, range }),
    };
    let row = ck.closures.column::<0>().iter().position(|&c| c == closure);
    let kind = match row.and_then(|i| ck.closures.column::<1>().get(i)) {
        Some(&k) => k,
        None => return Err(Refusal { form: Form::Malformed, range }),
    };
    // The closure's signature as the type check settled it (`Checked::closures`: a fn pointer
    // type, its inputs then its output; typeck rules/closure.rs `expr_closure`): each
    // parameter's type BEFORE its pattern's peels. `pat_ty` of a parameter pattern is the type
    // after them (`|(it, _)|` met by `&&(K, V)` records `(K, V)`), so a flow local of that
    // type peels nothing and every binding below it was refused `BindingMode` (census 1118).
    let sig = match row.and_then(|i| ck.closures.column::<2>().get(i)) {
        Some(&s) => s,
        None => return Err(Refusal { form: Form::Malformed, range }),
    };
    let tys = ck.types.view();
    let (inputs, output): (&[PTy], PTy) = match tys.node(sig) {
        Ok(PNode::FnPtr(l, _)) => match tys.list(l) {
            Ok(all) if all.len() == params.len() + 1 => (&all[..params.len()], all[params.len()]),
            _ => return Err(Refusal { form: Form::Malformed, range }),
        },
        _ => return Err(Refusal { form: Form::Malformed, range }),
    };
    let Some(row) = row else { return Err(Refusal { form: Form::Malformed, range }) };
    let mut l: blaze_own::Box<Lower<'_, '_, F, A, R>, A> = Lower::new(body, ck, facts, alloc);
    l.in_nested = true;
    if logged {
        l.b.start_log(alloc);
        l.per_call = true;
    }
    let mut externals: Vec<(Origin, External), A> = Vec::new_in(alloc);
    let ran = l.run_closure(closure, row, inner, params, (inputs, output), kind, &mut externals);
    let ran = l.finish_held(ran);
    *log = l.b.take_log();
    ran?;
    // `'static` and any universal lifetime parameters of the enclosing fn met during the walk
    // (`Lower::universal`).
    for &(r, o) in l.uni.iter() {
        match r {
            Region::Static => externals.push((o, External::Static)),
            Region::Param(i) => externals.push((o, External::OwnerParam(i))),
            Region::Late { .. } => {}
        }
    }
    for p in l.param_origs.iter() {
        externals.push((p.origin, External::Param(p.param)));
    }
    for (idx, &o) in l.dyn_bound_placeholders.iter().enumerate() {
        if let Ok(ord) = u32::try_from(idx) {
            externals.push((o, External::DynPlaceholder(ord)));
        }
    }
    let stats = l.stats;
    let b = core::mem::replace(&mut l.b, crate::builder::Builder::new(crate::FlowBuilder::new_in(1, 0, alloc)));
    let flow = b.finish().map_err(|_| Refusal { form: Form::Builder, range })?;
    let params = core::mem::replace(&mut l.param_origs, Vec::new_in(alloc));
    Ok(ClosureFlow { lowered: Lowered { flow, stats, params, coarse: l.coarse.get() }, externals })
}

/// The body of an `async fn` (`BodyView::coroutine` is `Some`) as its own flow, its `.await`s
/// suspensions (the module doc). A body that is not an `async fn`'s is refused as malformed:
/// [`crate::lower`] takes it.
pub fn lower_async_fn<A: Allocator + Copy, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    alloc: A,
) -> Result<Lowered<A>, Refusal> {
    lower_async_fn_with::<A, F, crate::one_call::Production>(body, ck, facts, false, alloc, &mut None)
}

/// [`lower_async_fn`] under the call rule `R`, logged as [`lower_closure_with`].
pub(crate) fn lower_async_fn_with<A: Allocator + Copy, F: Facts, R: crate::one_call::CallRule>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    logged: bool,
    alloc: A,
    log: &mut Option<crate::builder::Log<A>>,
) -> Result<Lowered<A>, Refusal> {
    let range = range_of(body, body.root);
    if body.coroutine.is_none() {
        return Err(Refusal { form: Form::Malformed, range });
    }
    let mut l: blaze_own::Box<Lower<'_, '_, F, A, R>, A> = Lower::new(body, ck, facts, alloc);
    if logged {
        l.b.start_log(alloc);
        l.per_call = true;
    }
    l.async_root = Some(body.root);
    let ran = l.run();
    let ran = l.finish_held(ran);
    *log = l.b.take_log();
    ran?;
    let suspends = l.points.len();
    let stats = l.stats;
    let b = core::mem::replace(&mut l.b, crate::builder::Builder::new(crate::FlowBuilder::new_in(1, 0, alloc)));
    let flow = b.finish().map_err(|_| Refusal { form: Form::Builder, range })?;
    let counted = flow.stmts.iter().filter(|s| matches!(s.kind, blaze_flow::StmtKind::Suspend { .. })).count();
    if counted != suspends {
        return Err(Refusal { form: Form::Malformed, range });
    }
    let params = core::mem::replace(&mut l.param_origs, Vec::new_in(alloc));
    Ok(Lowered { flow, stats, params, coarse: l.coarse.get() })
}

impl<F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'_, '_, F, A, R> {
    /// The walk of closure `closure`'s body `inner`: as `run`, with the captures then the
    /// parameters as the arguments and the signature's output as the return place.
    /// `row`: the closure's row of `Checked::closures`; `sig`: the parameters' types and the
    /// output from the closure's signature; `externals` receives what each capture's,
    /// parameter's and return place's placeholder stands for in the owner ([`External`]).
    #[allow(clippy::too_many_arguments)]
    fn run_closure(
        &mut self,
        closure: ExprId,
        row: usize,
        inner: ExprId,
        params: &[PatId],
        sig: (&[PTy], PTy),
        kind: ClosureKind,
        externals: &mut Vec<(Origin, External), A>,
    ) -> Result<(), Refusal> {
        let (inputs, output) = sig;
        let ck = self.ck;
        self.env_now.clear();
        self.field_places.clear();
        for f in ck.env_fields_by::<blaze_typeck::Places>(closure) {
            self.env_now.push(f);
        }
        let nu = to_u32(self.env_now.len());
        let np = to_u32(params.len());
        self.b.replace(FlowBuilder::new_in(1 + nu + np, nu + np, self.alloc));
        self.nargs = nu + np;
        let entry = self.b.block();
        self.ret_block = self.b.block();
        self.b.start(entry);
        self.scope_marks.push(0);
        self.scope_temps.push((to_u32(self.temps.len()), to_u32(self.temp_marks.len())));
        // The next position of the closure value (`External::Capture`).
        let mut value_at = 0u32;
        for k in 0..self.env_now.len() {
            let f = self.env_now[k];
            let cap = f.how;
            let l = Local(1 + to_u32(k));
            let (through, env) = Self::field_through(cap, kind);
            let first = to_u32(self.origs.len());
            if f.is_whole() {
                let p = f.base;
                let t = match ck.local_ty.get(p.ix()) {
                    Some(&t) => t,
                    None => return self.refuse_pat(Form::Malformed, p),
                };
                match through {
                    None => {
                        self.new_local_with(TyD::of(t), true, Some(l), closure)?;
                        match self.pat_local.get_mut(p.ix()) {
                            Some(slot) => *slot = l.0,
                            None => return self.refuse_pat(Form::Malformed, p),
                        }
                    }
                    Some(m) => {
                        let rt = TyD { wrap: Some(m), ty: t };
                        self.new_local_with(rt, true, Some(l), closure)?;
                        let at = self.place_of(l);
                        let place = self.project(at, rt, Proj::Deref, TyD::of(t), false, closure)?;
                        self.upvar_places.push((p, place, TyD::of(t)));
                    }
                }
            } else {
                self.place_upvar(closure, to_u32(k), f, through)?;
            }
            let made = to_u32(self.origs.len());
            // A by-value capture reached through `&'env self` has the environment's reference
            // first and no position of the value for it; a `&mut` capture of an `FnMut` has its
            // reference at the value's position, but reaches it only for `'env`: neither is a
            // region the owner can state a requirement on.
            let by_value_env = env && cap == CaptureKind::ByValue;
            for o in first..made {
                let what = if env && o == first {
                    External::Env
                } else {
                    External::Capture(value_at + (o - first) - u32::from(by_value_env))
                };
                externals.push((self.origs[to_usize(o)], what));
            }
            value_at += (made - first) - u32::from(by_value_env);
            self.vars.push(l);
        }
        let upvar_origins = self.origs.len();
        // What each signature position is (the module doc, THE SIGNATURE'S REGIONS). Computed
        // before creating parameter and return locals so inner higher-ranked binder slots
        // (`SigSlot::InnerBound`, e.g. `dyn for<'d> FnMut(&'d dyn Display)`) receive existential
        // origins instead of rigid placeholders.
        let mut slots: Vec<SigSlot, A> = Vec::new_in(self.alloc);
        self.sig_slots(row, closure, &mut slots)?;
        let mut expected_sig_npos = 0usize;
        for &t in inputs {
            expected_sig_npos += to_usize(self.npos(t, closure)?);
        }
        expected_sig_npos += to_usize(self.npos(output, closure)?);
        if slots.len() != expected_sig_npos {
            return self.refuse(Form::SigPositionsUnaligned { positions: expected_sig_npos, regions: slots.len(), site: crate::SigSite::ClosureSlots }, closure);
        }
        if to_u64(slots.len()) * to_u64(slots.len()) > u64::from(MOST_POSITIONS) {
            return self.refuse(Form::Bound, closure);
        }
        let mut inner_bound: Vec<((u32, u32), Origin), A> = Vec::new_in(self.alloc);
        let mut slot_at = 0usize;
        // The parameters: their types in the closure's signature (before their patterns'
        // peels; `lower_closure_external`), each non-`InnerBound` position a placeholder.
        for (i, &p) in params.iter().enumerate() {
            let t = match inputs.get(i) {
                Some(&t) => TyD::of(t),
                None => return self.refuse_pat(Form::Malformed, p),
            };
            let n = to_usize(self.npos_d(t, closure)?);
            let l = Local(1 + nu + to_u32(i));
            let Some(param_slots) = slots.get(slot_at..slot_at + n) else {
                return self.refuse(Form::SigPositionsUnaligned { positions: slot_at + n, regions: slots.len(), site: crate::SigSite::ClosureSlots }, closure);
            };
            self.new_closure_sig_local(t, l, param_slots, &mut inner_bound, closure)?;
            slot_at += n;
            self.vars.push(l);
        }
        // The return place: the signature's output (the body's value is assigned to it at
        // `RootEnd`, related by `assign_val`; a coercion of the value to the output is the
        // relation's, all positions made equal where the shapes differ). Its positions are the
        // signature's after the inputs', as `sig_slots` lays them.
        let out_n = to_usize(self.npos(output, closure)?);
        let Some(out_slots) = slots.get(slot_at..slot_at + out_n) else {
            return self.refuse(Form::SigPositionsUnaligned { positions: slot_at + out_n, regions: slots.len(), site: crate::SigSite::ClosureSlots }, closure);
        };
        self.new_closure_sig_local(TyD::of(output), Local(0), out_slots, &mut inner_bound, closure)?;
        let all = self.origs.len();
        if slots.len() != all - upvar_origins {
            return self.refuse(Form::SigPositionsUnaligned { positions: all - upvar_origins, regions: slots.len(), site: crate::SigSite::ClosureSlots }, closure);
        }
        for (j, &s) in slots.iter().enumerate() {
            let o = self.origs[upvar_origins + j];
            match s {
                SigSlot::Owner(k) => externals.push((o, External::SigOwner(k))),
                SigSlot::Late(_) => externals.push((o, External::Late)),
                SigSlot::Static => externals.push((o, External::Static)),
                SigSlot::InnerBound(..) => {}
            }
            if s == SigSlot::Static {
                let st = self.universal(Region::Static);
                self.b.known(o, st);
                self.b.known(st, o);
            }
            // Two positions of one variable of the clause's or fn pointer's binder are one region.
            if let SigSlot::Late(Some(var)) = s {
                for (i, &t) in slots[..j].iter().enumerate() {
                    if t == SigSlot::Late(Some(var)) {
                        let p = self.origs[upvar_origins + i];
                        self.b.known(o, p);
                        self.b.known(p, o);
                    }
                }
            }
        }
        // Each argument's and the return place's implied bounds (`&'r T` implies both inner
        // lifetimes outlive `'r` and inner type parameters `P: 'r`, RFC 1214). Skip existential
        // inner-binder positions (`SigSlot::InnerBound`): they are bound inside `dyn`/`fn`, not
        // free lifetimes of the enclosing reference.
        let is_inner_bound = |orig_idx: usize| -> bool {
            orig_idx >= upvar_origins && matches!(slots.get(orig_idx - upvar_origins), Some(SigSlot::InnerBound(..)))
        };
        let mut ranges: Vec<(u32, u32), A> = Vec::new_in(self.alloc);
        for l in 0..=self.nargs {
            let (start, _) = self.local_origs[to_usize(l)];
            let t = self.local_ty[to_usize(l)];
            self.ref_ranges(t, closure, &mut ranges)?;
            for j in 0..ranges.len() {
                let (at, inner_n) = ranges[j];
                let outer_idx = to_usize(start + at);
                if is_inner_bound(outer_idx) {
                    continue;
                }
                let Some(&outer) = self.origs.get(outer_idx) else {
                    continue;
                };
                for k in 1..=inner_n {
                    let o_idx = to_usize(start + at + k);
                    if is_inner_bound(o_idx) {
                        continue;
                    }
                    let Some(&o) = self.origs.get(o_idx) else {
                        continue;
                    };
                    if o != outer {
                        self.b.known(o, outer);
                    }
                }
            }
            self.ref_param_implied(t, start, closure)?;
        }
        // The parameters' patterns, bound as a fn body's are.
        for (i, &p) in params.iter().enumerate() {
            let l = Local(1 + nu + to_u32(i));
            let ty = self.local_ty[to_usize(l.0)];
            let pl = self.place_of(l);
            match self.body.pats.get(p.ix()) {
                Some(Pat::Bind { ann, sub: None, .. }) if ann.by_ref.is_none() => {
                    self.pat_local[p.ix()] = l.0;
                }
                _ => self.bind_pattern(p, Val { at: At::Place(pl), ty })?,
            }
        }
        self.tasks.push(Task::RootEnd(inner));
        self.tasks.push(Task::Eval(inner, false));
        let body = self.body;
        let fuel = 32 * to_u64(body.exprs.len() + body.pats.len() + body.stmts.len()) + 256;
        let mut steps = 0u64;
        while let Some(t) = self.tasks.pop() {
            steps += 1;
            if steps > fuel {
                return self.refuse(Form::Bound, closure);
            }
            self.step(t)?;
        }
        Ok(())
    }

    /// Creates a closure parameter or return local whose origins follow `sig_slots`: rigid
    /// placeholders for closure-bound / owner / `'static` slots, and shared existential origins
    /// for inner higher-ranked binder slots (`SigSlot::InnerBound`).
    fn new_closure_sig_local(
        &mut self,
        t: TyD,
        l: Local,
        sig_slots: &[SigSlot],
        inner_bound: &mut Vec<((u32, u32), Origin), A>,
        e: ExprId,
    ) -> Result<(), Refusal> {
        let n = self.npos_d(t, e)?;
        let mut live_mask: Vec<crate::OriginLive, A> = Vec::new_in(self.alloc);
        if n > 0 {
            self.origin_live_mask(t, &mut live_mask, e)?;
        }
        let start = to_u32(self.origs.len());
        for k in 0..to_usize(n) {
            let o = match sig_slots.get(k).copied() {
                Some(SigSlot::InnerBound(binder, var)) => {
                    if let Some(&(_, prev)) = inner_bound.iter().find(|&&(key, _)| key == (binder, var)) {
                        prev
                    } else {
                        let fresh = self.b.origin(false);
                        inner_bound.push(((binder, var), fresh));
                        fresh
                    }
                }
                _ => self.b.origin(true),
            };
            match live_mask.get(k).copied().unwrap_or(crate::OriginLive::Full) {
                crate::OriginLive::Full => self.b.mentions(l.0, o),
                crate::OriginLive::NoDrop => self.b.mentions_no_drop(l.0, o),
                crate::OriginLive::None => {}
            }
            self.origs.push(o);
        }
        self.set_local(l, t, start, n);
        let identity = match l {
            Local(0) => crate::Identity::Return,
            a => crate::Identity::Arg(a.0),
        };
        self.name_local(l, identity);
        Ok(())
    }
}

/// Whether a slot of a clause's column is one its `for<..>` binds.
fn is_late(r: &Region) -> bool {
    matches!(r, Region::Late { .. })
}

/// A type node's kind, for a refusal that names two nodes ([`ClosureSigWhy::Shape`]).
pub(crate) fn node_kind(n: PNode) -> &'static str {
    match n {
        PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str | PNode::Never => "scalar",
        PNode::Tuple(_) => "tuple",
        PNode::Record(..) => "record",
        PNode::Union(_) => "union",
        PNode::Ref(..) => "ref",
        PNode::Ptr(..) => "ptr",
        PNode::Array(..) => "array",
        PNode::Slice(_) => "slice",
        PNode::Pat(..) => "pattern type",
        PNode::Adt(..) => "adt",
        PNode::Foreign(_) => "extern type",
        PNode::FnDef(..) => "fn item",
        PNode::FnPtr(..) => "fn ptr",
        PNode::Closure { .. } => "closure",
        PNode::Future(..) | PNode::Coroutine { .. } => "future",
        PNode::Param(_) => "param",
        PNode::Alias { .. } => "projection",
        PNode::Opaque(..) => "opaque",
        PNode::Dynamic(_) => "dyn",
        PNode::ConstValue { .. } | PNode::ConstParam(_) => "const",
        PNode::Infer => "infer",
        PNode::Error => "error",
        PNode::ExTrait(..) | PNode::ExProj { .. } | PNode::ExAuto(_) => "clause",
    }
}

impl<'b, 'c, F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'b, 'c, F, A, R> {
    /// What each position of the signature of closure row `row` (`Checked::closures`) is, into
    /// `out`: its inputs' positions, then its output's, each as `npos` counts it (the module
    /// doc, THE SIGNATURE'S REGIONS).
    ///
    /// - Expected by a clause (`SigFrom::Clause`): the inputs are aligned with the trait
    ///   clause's `Args` (its column's child 1), the output with the projection clause's term
    ///   (child 2). A part whose slots hold no binder region is the owner's whole (every region
    ///   of it is the callee's parameter, `'static`, or a type parameter's instance, which the
    ///   call fixes). A part with a binder region is aligned one slot per position, which holds
    ///   only when no type parameter of the clause holds a position of it (then the counts are
    ///   equal); else refused by name (`Form::ClosureSigUnaligned`), never guessed. An output
    ///   no projection clause of the use names is the owner's.
    /// - Otherwise (written, inferred, expected by a trait object or a fn pointer): every
    ///   input position universal, each a region of its own (a written elided `&T` is one
    ///   bound per call, and a call this body does not see may choose any region for one the
    ///   type check inferred), every output position the owner's.
    pub(crate) fn sig_slots(&self, row: usize, e: ExprId, out: &mut Vec<SigSlot, A>) -> Result<(), Refusal> {
        out.clear();
        let ck: &'c Checked<'c> = self.ck;
        let (Some(&sig), Some(&from)) = (ck.closures.column::<2>().get(row), ck.closures.column::<4>().get(row)) else {
            return self.refuse(Form::Malformed, e);
        };
        let all: &'c [PTy] = match self.node(sig, e)? {
            PNode::FnPtr(l, _) => self.list(l, e)?,
            _ => return self.refuse(Form::Malformed, e),
        };
        let Some((&output, inputs)) = all.split_last() else { return self.refuse(Form::Malformed, e) };
        let mut nin = 0u32;
        for &t in inputs {
            nin += self.npos(t, e)?;
        }
        let nout = self.npos(output, e)?;
        let facts: &'b F = self.facts;
        let arity = crate::one_call::FactsArity { facts };
        let heads = blaze_borrow::Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        // Each part: the clause's or expecting fn-pointer's slots over it, and its declared
        // type(s) in the heads' table.
        type InPart<'s> = Option<(&'s [Region], Option<PTy>, Option<&'s [PTy]>)>;
        type OutPart<'s> = Option<(&'s [Region], Option<PTy>)>;
        let mut outer_binder = 0u32;
        let mut static_as_static = false;
        let (ins, outs): (InPart<'_>, OutPart<'_>) = match from {
            SigFrom::Clause { expr, nth, output } => {
                let item = self.clause_owner(expr, nth, e)?;
                let clauses = match facts.regions().clauses(item) {
                    Ok(c) => c,
                    Err(u) => return self.refuse(Form::RegionsUnread { item: u.item, why: u.why }, e),
                };
                let part = |n: u32, child: u32| {
                    let c = clauses.iter().find(|c| c.nth == n)?;
                    let slots = c.col.child(child)?;
                    let declared = blaze_borrow::RegionTypes::kids(&heads, c.col.ty()).ok().and_then(|k| k.get(child));
                    Some((slots, declared))
                };
                let Some((in_s, in_d)) = part(nth, 1) else { return self.refuse(Form::ClosureClauseUnread { nth }, e) };
                let outs = match output {
                    Some(o) => match part(o, 2) {
                        Some(s) => Some(s),
                        None => return self.refuse(Form::ClosureClauseUnread { nth: o }, e),
                    },
                    None => None,
                };
                (Some((in_s, in_d, None)), outs)
            }
            SigFrom::FnPtr => {
                if let Some((in_s, decl_ins, out_s, decl_out, binder)) =
                    ck.closures.column::<0>().get(row).copied().and_then(|ce| self.fn_ptr_expectation(ce, inputs.len(), &heads, e))
                {
                    outer_binder = binder;
                    static_as_static = true;
                    (Some((in_s, None, Some(decl_ins))), Some((out_s, Some(decl_out))))
                } else {
                    (None, None)
                }
            }
            SigFrom::Object => {
                if let Some((in_s, decl_tup, out_part, binder)) =
                    ck.closures.column::<0>().get(row).copied().and_then(|ce| self.object_expectation(ce, inputs.len(), &heads, e))
                {
                    outer_binder = binder;
                    static_as_static = true;
                    (Some((in_s, Some(decl_tup), None)), out_part)
                } else {
                    (None, None)
                }
            }
            SigFrom::Written | SigFrom::Inferred => (None, None),
        };
        let mut owner = 0u32;
        let mut pairs: Vec<(PTy, PTy), A> = Vec::new_in(self.alloc);
        match ins {
            Some((s, declared_tuple, declared_list)) => {
                // The declared `Args` tuple's elements (or expecting `fn` pointer's input types)
                // beside the closure's inputs.
                pairs.clear();
                if let Some(ds) = declared_list
                    && ds.len() == inputs.len()
                {
                    for (&dk, &ik) in ds.iter().zip(inputs.iter()) {
                        pairs.push((dk, ik));
                    }
                } else if let Some(d) = declared_tuple
                    && let Ok(PNode::Tuple(_)) = blaze_borrow::RegionTypes::node(&heads, d)
                    && let Ok(blaze_borrow::Kids::List(ds)) = blaze_borrow::RegionTypes::kids(&heads, d)
                    && ds.len() == inputs.len()
                {
                    for (&dk, &ik) in ds.iter().zip(inputs.iter()) {
                        pairs.push((dk, ik));
                    }
                }
                self.lay_sig_part(s, nin, 0, outer_binder, static_as_static, &pairs, &heads, &mut owner, out, e)?
            }
            None => {
                let ce = ck.closures.column::<0>().get(row).copied();
                let written_tys = ce.and_then(|ce| match self.body.exprs.get(ce.ix()) {
                    Some(Expr::Closure { param_tys, .. }) => self.body.opt_types.get(param_tys.range()),
                    _ => None,
                });
                for (pi, &t) in inputs.iter().enumerate() {
                    let param_written = written_tys.and_then(|l| l.get(pi)).is_some_and(Option::is_some);
                    self.lay_written_param(ce, to_u32(pi), t, param_written, from, &mut owner, out, e)?;
                }
            }
        }
        match outs {
            Some((s, declared)) => {
                pairs.clear();
                if let Some(d) = declared {
                    pairs.push((d, output));
                }
                self.lay_sig_part(s, nout, 1, outer_binder, static_as_static, &pairs, &heads, &mut owner, out, e)?
            }
            None => {
                for _ in 0..nout {
                    out.push(SigSlot::Owner(owner));
                    owner += 1;
                }
            }
        }
        Ok(())
    }

    /// Resolves the expecting `fn(...) -> ...` pointer type and its region column for a closure
    /// coerced to a `fn` pointer (`SigFrom::FnPtr`): struct field initializer (`Check { run: |..| .. }`),
    /// tuple constructor argument, or direct/method call argument.
    ///
    /// Returns `(input_slots, declared_inputs, output_slots, declared_output, fn_ptr_binder)` in
    /// the heads' table when the expecting site declares a `PNode::FnPtr` of matching arity.
    fn fn_ptr_expectation<'h>(
        &self,
        ce: ExprId,
        n_inputs: usize,
        heads: &'h blaze_borrow::Columns<'h>,
        e: ExprId,
    ) -> Option<(&'b [Region], &'h [PTy], &'b [Region], PTy, u32)> {
        use blaze_borrow::RegionTypes;
        use blaze_typeck::Def;

        let unwrap_parens = |mut cur: ExprId| -> ExprId {
            for _ in 0..16 {
                match self.body.exprs.get(cur.ix()) {
                    Some(&Expr::Paren(inner)) => cur = inner,
                    _ => break,
                }
            }
            cur
        };

        let facts: &'b F = self.facts;
        let mut found: Option<(PTy, &'b [Region], u32)> = None;
        for (ix, expr) in self.body.exprs.iter().enumerate() {
            let pe = ExprId(to_u32(ix));
            match *expr {
                Expr::Struct { fields, .. } => {
                    let Some(fs) = self.body.field_inits.get(fields.range()) else { continue };
                    let Some(f) = fs.iter().find(|f| unwrap_parens(f.expr) == ce) else { continue };
                    let (adt, variant) = match self.expr_def(pe) {
                        Some(Def::Ctor { adt, variant, .. }) => (adt, variant),
                        Some(Def::Adt(adt)) if self.facts.adt(adt).is_some_and(|a| !a.is_enum) => (adt, 0),
                        _ => continue,
                    };
                    let Some(fi) = self.facts.field(adt, variant, f.name) else { continue };
                    if let Ok(col) = facts.regions().field(adt, variant, fi) {
                        found = Some((col.ty(), col.as_slice(), 0));
                        break;
                    }
                }
                Expr::Call { callee, args } => {
                    let Some(es) = self.body.expr_lists.get(args.range()) else { continue };
                    let Some(arg_i) = es.iter().position(|&a| unwrap_parens(a) == ce) else { continue };
                    match self.expr_def(callee) {
                        Some(Def::Ctor { adt, variant, .. }) => {
                            if let Ok(col) = facts.regions().field(adt, variant, to_u32(arg_i)) {
                                found = Some((col.ty(), col.as_slice(), 0));
                                break;
                            }
                        }
                        Some(Def::Fn(item)) => {
                            if let Ok(sig) = facts.sig(item)
                                && let Some(in_slots) = sig.input(to_u32(arg_i))
                                && let Ok(in_ty) = self.sig_declared(sig, to_u32(arg_i), e)
                            {
                                let binder = self.fn_sig_param_binder(sig, to_u32(arg_i), heads);
                                found = Some((in_ty, in_slots, binder));
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                Expr::MethodCall { args, .. } => {
                    let Some(es) = self.body.expr_lists.get(args.range()) else { continue };
                    let Some(arg_i) = es.iter().position(|&a| unwrap_parens(a) == ce) else { continue };
                    let param_i = to_u32(arg_i + 1);
                    if let Some(pos) = self.ck.callees.column::<0>().iter().position(|&x| x == pe)
                        && let Some(&item) = self.ck.callees.column::<1>().get(pos)
                        && let Ok(sig) = facts.sig(item)
                        && let Some(in_slots) = sig.input(param_i)
                        && let Ok(in_ty) = self.sig_declared(sig, param_i, e)
                    {
                        let binder = self.fn_sig_param_binder(sig, param_i, heads);
                        found = Some((in_ty, in_slots, binder));
                        break;
                    }
                }
                _ => {}
            }
        }
        let (fn_ty, fn_slots, outer_binder) = found?;
        let Ok(PNode::FnPtr(_, _)) = heads.node(fn_ty) else { return None };
        let Ok(blaze_borrow::Kids::List(fn_kids)) = heads.kids(fn_ty) else { return None };
        let (in_kids, rest_kids) = fn_kids.split_at_checked(n_inputs)?;
        let (&out_kid, tail_kids) = rest_kids.split_first()?;
        if !tail_kids.is_empty() {
            return None;
        }
        let mut in_len = 0usize;
        for &k in in_kids {
            let Ok(c) = heads.count(k) else { return None };
            in_len += c.len();
        }
        let Ok(out_c) = heads.count(out_kid) else { return None };
        if in_len + out_c.len() != fn_slots.len() {
            return None;
        }
        let (in_slots, out_slots) = fn_slots.split_at_checked(in_len)?;
        Some((in_slots, in_kids, out_slots, out_kid, outer_binder))
    }

    /// Resolves the expecting `dyn Fn*(Args) -> Output` trait object type and its region column
    /// for a closure coerced to a trait object (`SigFrom::Object`, e.g. `Box::new(move |data: &[u8]| ...)`
    /// coerced to `Box<dyn for<'a> Fn(&'a [u8]) -> Option<Decoded<'a>>>`).
    ///
    /// Returns `(input_slots, declared_args_tuple, output_part, dyn_binder)` in the heads' table
    /// when a unique matching `PNode::Dynamic` with `Fn*` predicates is found in the expecting
    /// site or enclosing owner's signature.
    #[allow(clippy::type_complexity)]
    fn object_expectation<'h>(
        &self,
        ce: ExprId,
        n_inputs: usize,
        heads: &'h blaze_borrow::Columns<'h>,
        e: ExprId,
    ) -> Option<(&'b [Region], PTy, Option<(&'b [Region], Option<PTy>)>, u32)> {
        use blaze_borrow::RegionTypes;
        use blaze_typeck::Def;

        let facts: &'b F = self.facts;
        // Walk up from `ce` through transparent wrappers (`Paren`, `&`, `Box::new`, `Cast`)
        // to find the coerced erased `PNode::Dynamic` target type.
        let mut cur = ce;
        let mut want_dyn: Option<PTy> = None;
        for _ in 0..8 {
            if let Ok(adj) = self.adjusted_ty(cur)
                && let Ok(Some(d)) = self.peel_body_dyn(adj, e)
            {
                want_dyn = Some(d);
                break;
            }
            // Find the parent expression that wraps `cur`.
            let mut next = None;
            for (ix, expr) in self.body.exprs.iter().enumerate() {
                let pe = ExprId(to_u32(ix));
                match *expr {
                    Expr::Paren(inner) | Expr::Ref { operand: inner, .. } | Expr::Cast { operand: inner, .. } if inner == cur => {
                        next = Some(pe);
                        break;
                    }
                    Expr::Call { args, .. } => {
                        if let Some(es) = self.body.expr_lists.get(args.range())
                            && es.len() == 1
                            && es[0] == cur
                            && let Ok(pty) = self.expr_ty(pe)
                            && let Ok(PNode::Adt(k, _)) = self.node(pty, e)
                            && facts.adt(k).is_some_and(|a| a.is_box)
                        {
                            next = Some(pe);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(parent) = next else { break };
            cur = parent;
        }
        let want_dyn = want_dyn?;
        let target_expr = cur;

        let mut found: Option<(PTy, &'b [Region], u32)> = None;

        // 1. Direct expecting site (struct field, tuple ctor, direct fn or method call arg).
        for (ix, expr) in self.body.exprs.iter().enumerate() {
            let pe = ExprId(to_u32(ix));
            match *expr {
                Expr::Struct { fields, .. } => {
                    let Some(fs) = self.body.field_inits.get(fields.range()) else { continue };
                    let Some(f) = fs.iter().find(|f| f.expr == target_expr) else { continue };
                    let (adt, variant) = match self.expr_def(pe) {
                        Some(Def::Ctor { adt, variant, .. }) => (adt, variant),
                        Some(Def::Adt(adt)) if facts.adt(adt).is_some_and(|a| !a.is_enum) => (adt, 0),
                        _ => continue,
                    };
                    let Some(fi) = facts.field(adt, variant, f.name) else { continue };
                    if let Ok(col) = facts.regions().field(adt, variant, fi)
                        && let Ok(Some(m)) = self.find_unique_dyn_with_binder(col.ty(), col.as_slice(), 0, heads, want_dyn, e)
                    {
                        found = Some(m);
                        break;
                    }
                }
                Expr::Call { callee, args } => {
                    let Some(es) = self.body.expr_lists.get(args.range()) else { continue };
                    let Some(arg_i) = es.iter().position(|&a| a == target_expr) else { continue };
                    match self.expr_def(callee) {
                        Some(Def::Ctor { adt, variant, .. }) => {
                            if let Ok(col) = facts.regions().field(adt, variant, to_u32(arg_i))
                                && let Ok(Some(m)) = self.find_unique_dyn_with_binder(col.ty(), col.as_slice(), 0, heads, want_dyn, e)
                            {
                                found = Some(m);
                                break;
                            }
                        }
                        Some(Def::Fn(item)) => {
                            if let Ok(sig) = facts.sig(item)
                                && let Some(in_slots) = sig.input(to_u32(arg_i))
                                && let Ok(in_ty) = self.sig_declared(sig, to_u32(arg_i), e)
                            {
                                let base = self.fn_sig_param_binder(sig, to_u32(arg_i), heads);
                                if let Ok(Some(m)) = self.find_unique_dyn_with_binder(in_ty, in_slots, base, heads, want_dyn, e) {
                                    found = Some(m);
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Expr::MethodCall { args, .. } => {
                    let Some(es) = self.body.expr_lists.get(args.range()) else { continue };
                    let Some(arg_i) = es.iter().position(|&a| a == target_expr) else { continue };
                    let param_i = to_u32(arg_i + 1);
                    if let Some(pos) = self.ck.callees.column::<0>().iter().position(|&x| x == pe)
                        && let Some(&item) = self.ck.callees.column::<1>().get(pos)
                        && let Ok(sig) = facts.sig(item)
                        && let Some(in_slots) = sig.input(param_i)
                        && let Ok(in_ty) = self.sig_declared(sig, param_i, e)
                    {
                        let base = self.fn_sig_param_binder(sig, param_i, heads);
                        if let Ok(Some(m)) = self.find_unique_dyn_with_binder(in_ty, in_slots, base, heads, want_dyn, e) {
                            found = Some(m);
                            break;
                        }
                    }
                }
                _ => {}
            }
        }

        // 2. When the immediate call is generic (e.g. `HashMap::insert(k, Box::new(closure))` into
        //    a parameter `registry: &mut HashMap<K, Box<dyn for<'a> Fn(&'a [u8]) -> ...>>`),
        //    search the enclosing fn's own declared signature for a unique matching `PNode::Dynamic`.
        if found.is_none()
            && let Ok(own_sig) = facts.sig(self.body.owner)
            && let Ok(blaze_borrow::Kids::List(sig_kids)) = heads.kids(own_sig.regions().col.ty())
        {
            let mut ambiguous = false;
            for (ki, &kty) in sig_kids.iter().enumerate() {
                let Some(kslots) = own_sig.regions().col.child(to_u32(ki)) else { continue };
                let base = self.fn_sig_param_binder(own_sig, to_u32(ki), heads);
                let Ok(cand) = self.find_unique_dyn_with_binder(kty, kslots, base, heads, want_dyn, e) else {
                    ambiguous = true;
                    break;
                };
                if let Some((dt, ds, db)) = cand {
                    match found {
                        None => found = Some((dt, ds, db)),
                        Some((_, prev_s, prev_b)) if prev_b == db && prev_s == ds => {}
                        Some(_) => {
                            ambiguous = true;
                            break;
                        }
                    }
                }
            }
            if ambiguous {
                found = None;
            }
        }

        let (dyn_ty, dyn_slots, dyn_binder) = found?;
        let Ok(PNode::Dynamic(preds)) = heads.node(dyn_ty) else { return None };
        let Some(mut rest) = dyn_slots.get(1..) else { return None };
        let out_name = blaze_typeck::Name::of("Output");
        let mut args_match: Option<(PTy, &'b [Region])> = None;
        let mut out_match: Option<(&'b [Region], Option<PTy>)> = None;
        for &bound in self.head_list(preds, e).ok()? {
            let Ok(bc) = heads.count(bound) else { return None };
            let Some((bslots, after)) = rest.split_at_checked(bc.len()) else { return None };
            rest = after;
            match heads.node(bound) {
                Ok(PNode::ExTrait(tr, _)) if facts.is_fn_trait(tr) => {
                    let Ok(kids) = heads.kids(bound) else { continue };
                    let Some(tup) = kids.get(0) else { continue };
                    let Ok(PNode::Tuple(_)) = heads.node(tup) else { continue };
                    let Ok(tkids) = heads.kids(tup) else { continue };
                    if to_usize(tkids.len()) != n_inputs {
                        continue;
                    }
                    let Ok(tc) = heads.count(tup) else { return None };
                    if tc.len() != bslots.len() {
                        return None;
                    }
                    args_match = Some((tup, bslots));
                }
                Ok(PNode::ExProj { trait_: tr, name, term, .. }) if facts.is_fn_trait(tr) && name == out_name => {
                    let Ok(tc) = heads.count(term) else { return None };
                    let tn = tc.len();
                    let Some(oslots) = bslots.get(bslots.len().saturating_sub(tn)..) else { return None };
                    if oslots.len() != tn {
                        return None;
                    }
                    out_match = Some((oslots, Some(term)));
                }
                _ => {}
            }
        }
        if !rest.is_empty() {
            return None;
        }
        let (args_tup, args_slots) = args_match?;
        Some((args_slots, args_tup, out_match, dyn_binder))
    }

    /// Searches a head-table type `(root, slots)` in exact pre-order (tracking binder indices
    /// from `base_binder`) for a unique `PNode::Dynamic` matching `want_dyn`.
    #[allow(clippy::type_complexity)]
    fn find_unique_dyn_with_binder(
        &self,
        root: PTy,
        slots: &'b [Region],
        base_binder: u32,
        heads: &blaze_borrow::Columns<'_>,
        want_dyn: PTy,
        e: ExprId,
    ) -> Result<Option<(PTy, &'b [Region], u32)>, Refusal> {
        use blaze_borrow::RegionTypes;

        let want_slots = self.npos(want_dyn, e)?;
        let PNode::Dynamic(want_preds) = self.node(want_dyn, e)? else { return Ok(None) };
        let want_list = self.list(want_preds, e)?;
        let mut stack: Vec<(PTy, &'b [Region], bool), A> = Vec::new_in(self.alloc);
        stack.push((root, slots, false));
        let mut found: Option<(PTy, &'b [Region], u32)> = None;
        let mut next_binder = base_binder;
        let mut steps = 0u32;
        while let Some((cur, cur_slots, under_fn_ptr)) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return Ok(None);
            }
            let cur_node = self.head_node(cur, e).ok();
            let mut child_under_fn_ptr = under_fn_ptr;
            if let Some(PNode::Dynamic(h_preds)) = cur_node {
                let my_binder = next_binder;
                next_binder += 1;
                if !under_fn_ptr
                    && usize::try_from(want_slots).ok() == Some(cur_slots.len())
                    && self.dyn_preds_match(h_preds, want_list, e)?
                {
                    match found {
                        None => found = Some((cur, cur_slots, my_binder)),
                        Some((_, prev_slots, prev_b)) if prev_b == my_binder && prev_slots == cur_slots => {}
                        Some(_) => return Ok(None),
                    }
                }
            } else if matches!(cur_node, Some(PNode::FnPtr(..))) {
                next_binder += 1;
                child_under_fn_ptr = true;
            }
            let Ok(own) = heads.own(cur) else { continue };
            let Ok(kids) = heads.kids(cur) else { continue };
            let Some((_, mut rest)) = cur_slots.split_at_checked(own.len()) else { continue };
            // Collect children first, then push in reverse so stack pop visits in exact pre-order.
            let mut kslices: Vec<(PTy, &'b [Region]), A> = Vec::new_in(self.alloc);
            for k in 0..kids.len() {
                let Some(kid) = kids.get(k) else { break };
                let Ok(kc) = heads.count(kid) else { break };
                let Some((kslots, next_rest)) = rest.split_at_checked(kc.len()) else { break };
                rest = next_rest;
                kslices.push((kid, kslots));
            }
            for &(kid, kslots) in kslices.iter().rev() {
                stack.push((kid, kslots, child_under_fn_ptr));
            }
        }
        Ok(found)
    }

    /// Pre-order binder index of parameter `param_i`'s root node in `sig` (`sig`'s own root
    /// `FnPtr` is binder 0; each preceding `FnPtr`/`Dynamic` in inputs `0..param_i` increments it).
    fn fn_sig_param_binder(&self, sig: crate::SigLife<'_>, param_i: u32, heads: &blaze_borrow::Columns<'_>) -> u32 {
        use blaze_borrow::{Kids, RegionTypes};
        let mut binders = 1u32;
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc);
        let Ok(Kids::List(kids)) = heads.kids(sig.regions().col.ty()) else { return binders };
        for i in 0..to_usize(param_i) {
            let Some(&k) = kids.get(i) else { break };
            stack.push(k);
            let mut steps = 0u32;
            while let Some(t) = stack.pop() {
                steps += 1;
                if steps > MOST_POSITIONS {
                    break;
                }
                let Ok(n) = heads.node(t) else { break };
                if matches!(n, PNode::FnPtr(..) | PNode::Dynamic(_)) {
                    binders += 1;
                }
                if let Ok(ks) = heads.kids(t) {
                    for ci in (0..ks.len()).rev() {
                        if let Some(child) = ks.get(ci) {
                            stack.push(child);
                        }
                    }
                }
            }
        }
        binders
    }

    /// Lays `SigSlot`s for one closure parameter `root` when no expecting clause supplies its
    /// signature regions, walking `root` in exact `npos` pre-order while tracking the structural
    /// child-index path to look up explicitly written `'static` and named enclosing lifetimes in
    /// `BodyView::closure_param_regions`.
    #[expect(clippy::too_many_arguments, reason = "closure id, param index, type, written flag, source, owner counter, output, and expr")]
    fn lay_written_param(
        &self,
        ce: Option<ExprId>,
        param: u32,
        root: PTy,
        param_written: bool,
        from: SigFrom,
        owner: &mut u32,
        out: &mut Vec<SigSlot, A>,
        e: ExprId,
    ) -> Result<(), Refusal> {
        let inferred_unannotated = from == SigFrom::Inferred && !param_written;
        let push_default = |n: u32, owner: &mut u32, out: &mut Vec<SigSlot, A>| {
            for _ in 0..n {
                if inferred_unannotated {
                    out.push(SigSlot::Owner(*owner));
                    *owner += 1;
                } else {
                    out.push(SigSlot::Late(None));
                }
            }
        };
        let push_written = |path: u64, own: u32, owner: &mut u32, out: &mut Vec<SigSlot, A>| {
            let written = ce.and_then(|c| self.body.closure_param_region(c, param, path, own));
            match written {
                Some(blaze_typeck::WrittenRegion::Static) => out.push(SigSlot::Static),
                Some(blaze_typeck::WrittenRegion::Named) => {
                    out.push(SigSlot::Owner(*owner));
                    *owner += 1;
                }
                None if inferred_unannotated => {
                    out.push(SigSlot::Owner(*owner));
                    *owner += 1;
                }
                None => out.push(SigSlot::Late(None)),
            }
        };
        let mut stack: Vec<(PTy, Option<u64>), A> = Vec::new_in(self.alloc);
        stack.push((root, Some(blaze_typeck::ClosureParamRegion::ROOT)));
        let mut steps = 0u32;
        while let Some((t, path)) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            let Some(p) = path else {
                push_default(self.npos(t, e)?, owner, out);
                continue;
            };
            match self.node(t, e)? {
                PNode::Ref(_, inner) => {
                    push_written(p, 0, owner, out);
                    stack.push((inner, blaze_typeck::ClosureParamRegion::push_step(p, 0)));
                }
                PNode::Ptr(_, inner) | PNode::Slice(inner) | PNode::Array(inner, _) | PNode::Pat(inner, _) => {
                    stack.push((inner, blaze_typeck::ClosureParamRegion::push_step(p, 0)));
                }
                PNode::Tuple(l) => {
                    for (ci, &child) in self.list(l, e)?.iter().enumerate().rev() {
                        stack.push((child, blaze_typeck::ClosureParamRegion::push_step(p, to_u32(ci))));
                    }
                }
                PNode::Adt(k, l) => {
                    let nl = self.adt_facts(k, e)?.lifetimes;
                    for lt in 0..nl {
                        push_written(p, lt, owner, out);
                    }
                    for (ci, &child) in self.list(l, e)?.iter().enumerate().rev() {
                        stack.push((child, blaze_typeck::ClosureParamRegion::push_step(p, to_u32(ci))));
                    }
                }
                PNode::Dynamic(_) => {
                    // Slot 0 is the object lifetime (`+ 'o`); slots 1.. are existential
                    // predicate slots bound by `dyn`'s own inner `for<..>` binder (e.g.
                    // `|tx: &dyn Fn(&Fixture) -> AASigned|`), so under an explicit annotation
                    // they receive `InnerBound` (existential in the closure body) rather than
                    // `Late(None)` (rigid universal).
                    let total = self.npos(t, e)?;
                    if total > 0 {
                        push_written(p, 0, owner, out);
                        for slot_idx in 1..total {
                            if inferred_unannotated {
                                out.push(SigSlot::Owner(*owner));
                                *owner += 1;
                            } else {
                                out.push(SigSlot::InnerBound(u32::MAX - 1, slot_idx));
                            }
                        }
                    }
                }
                PNode::FnPtr(..) => {
                    let total = self.npos(t, e)?;
                    for slot_idx in 0..total {
                        if inferred_unannotated {
                            out.push(SigSlot::Owner(*owner));
                            *owner += 1;
                        } else {
                            out.push(SigSlot::InnerBound(u32::MAX - 2, slot_idx));
                        }
                    }
                }
                _ => {
                    push_default(self.npos(t, e)?, owner, out);
                }
            }
        }
        Ok(())
    }

    fn region_sig_slot(r: Region, outer_binder: u32, static_as_static: bool, owner: &mut u32) -> SigSlot {
        match r {
            Region::Late { binder, var } if binder == outer_binder => SigSlot::Late(Some((binder, var))),
            Region::Late { binder, var } => SigSlot::InnerBound(binder, var),
            Region::Static if static_as_static => SigSlot::Static,
            _ => {
                *owner += 1;
                SigSlot::Owner(*owner - 1)
            }
        }
    }

    /// One part (`part` 0 the inputs, 1 the output) of `Lower::sig_slots` from the clause's
    /// slots `s` over the part's `n` positions; `pairs`: the part's declared types (the
    /// clause's, in `heads`' table) beside the closure's own (the body's), when read.
    #[expect(clippy::too_many_arguments, reason = "the part, its declared and actual types, outer binder, static flag, and the owner count")]
    fn lay_sig_part(
        &self,
        s: &[Region],
        n: u32,
        part: u32,
        outer_binder: u32,
        static_as_static: bool,
        pairs: &[(PTy, PTy)],
        heads: &blaze_borrow::Columns<'_>,
        owner: &mut u32,
        out: &mut Vec<SigSlot, A>,
        e: ExprId,
    ) -> Result<(), Refusal> {
        if !s.iter().any(is_late) && !(static_as_static && s.iter().any(|r| matches!(r, Region::Static))) {
            for _ in 0..n {
                out.push(SigSlot::Owner(*owner));
                *owner += 1;
            }
            return Ok(());
        }
        if to_u32(s.len()) != n {
            // The counts part where a type parameter or a projection of the clause stands for
            // a type with positions (`P: FnMut(&Self::Item) -> bool` with `Item = (&'a A, &'b
            // B)`: one slot, three positions). Laid by walking the declared part beside the
            // closure's own; never guessed.
            return self.lay_sig_typed(s, n, part, outer_binder, static_as_static, pairs, heads, owner, out, e);
        }
        for &r in s {
            out.push(Self::region_sig_slot(r, outer_binder, static_as_static, owner));
        }
        Ok(())
    }

    /// A clause part whose slots are not one per position (`Lower::lay_sig_part`): its
    /// declared types `pairs` (the clause's, in `heads`' table) walked beside the closure's
    /// (the body's), both in pre-order (a node's own slots or positions, then its children's,
    /// as `npos` and the column lay them). Where the declared type writes the structure (a
    /// reference, an ADT, a tuple, a slice, an array, a pointer), each own slot is the
    /// position's: a binder's region (`Late`) or the owner's. Where it writes a type
    /// parameter, a projection or an opaque type, the call fixes what stands there (its
    /// instance or its normalization: no binder of the clause reaches into it, the Reference,
    /// "Higher-ranked trait bounds": the binder binds lifetimes the bound writes), so every
    /// position under it is the owner's, and its own slots (a projection's trait lifetimes)
    /// must hold no binder region. Anything else, or two types that do not walk in step:
    /// refused by name (`ClosureSigUnaligned`), never guessed.
    #[expect(clippy::too_many_arguments, reason = "lay_sig_part's parts")]
    fn lay_sig_typed(
        &self,
        s: &[Region],
        n: u32,
        part: u32,
        outer_binder: u32,
        static_as_static: bool,
        pairs: &[(PTy, PTy)],
        heads: &blaze_borrow::Columns<'_>,
        owner: &mut u32,
        out: &mut Vec<SigSlot, A>,
        e: ExprId,
    ) -> Result<(), Refusal> {
        use blaze_borrow::{Kids, RegionTypes};
        let unaligned = |why| Form::ClosureSigUnaligned { part, why };
        if pairs.is_empty() {
            return self.refuse(unaligned(ClosureSigWhy::NoDeclared), e);
        }
        let first = out.len();
        let mut at = 0usize;
        let mut stack: Vec<(PTy, PTy), A> = Vec::new_in(self.alloc);
        for &p in pairs.iter().rev() {
            stack.push(p);
        }
        // One own slot of the declared type, as the position of the closure's at the same place.
        let take = |at: &mut usize, owner: &mut u32, out: &mut Vec<SigSlot, A>| -> bool {
            let Some(&r) = s.get(*at) else { return false };
            *at += 1;
            out.push(Self::region_sig_slot(r, outer_binder, static_as_static, owner));
            true
        };
        let mut steps = 0u32;
        while let Some((d, a)) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            let Ok(dn) = heads.node(d) else { return self.refuse(unaligned(ClosureSigWhy::ColumnUnread), e) };
            let an = self.node(a, e)?;
            match (dn, an) {
                (PNode::Param(_) | PNode::Alias { .. } | PNode::Opaque(..), _) => {
                    let Ok(c) = heads.count(d) else { return self.refuse(unaligned(ClosureSigWhy::ColumnUnread), e) };
                    let Some(under) = s.get(at..at + c.len()) else { return self.refuse(unaligned(ClosureSigWhy::SlotsShort), e) };
                    if under.iter().any(is_late) {
                        return self.refuse(unaligned(ClosureSigWhy::BinderHidden), e);
                    }
                    at += c.len();
                    for _ in 0..self.npos(a, e)? {
                        out.push(SigSlot::Owner(*owner));
                        *owner += 1;
                    }
                }
                // A trait object: under `grain::Slots`, its object lifetime followed by its
                // predicates' slots match `heads.count(d)` one-to-one; under `grain::Positions`,
                // only the object lifetime has a position in the flow.
                (PNode::Dynamic(_), PNode::Dynamic(_)) => {
                    let Ok(c) = heads.count(d) else { return self.refuse(unaligned(ClosureSigWhy::ColumnUnread), e) };
                    let Some(under) = s.get(at..at + c.len()) else { return self.refuse(unaligned(ClosureSigWhy::SlotsShort), e) };
                    let Some((_, predicates)) = under.split_first() else { return self.refuse(unaligned(ClosureSigWhy::SlotsShort), e) };
                    let an = self.npos(a, e)?;
                    if to_usize(an) == c.len() {
                        for _ in 0..c.len() {
                            if !take(&mut at, &mut *owner, &mut *out) {
                                return self.refuse(unaligned(ClosureSigWhy::SlotsShort), e);
                            }
                        }
                    } else {
                        if predicates.iter().any(|r| matches!(r, Region::Late { binder, .. } if *binder == outer_binder)) {
                            return self.refuse(unaligned(ClosureSigWhy::BinderHidden), e);
                        }
                        if !take(&mut at, &mut *owner, &mut *out) {
                            return self.refuse(unaligned(ClosureSigWhy::SlotsShort), e);
                        }
                        at += predicates.len();
                    }
                }
                (PNode::Ref(dm, di), PNode::Ref(am, ai)) if dm == am => {
                    if !take(&mut at, &mut *owner, &mut *out) {
                        return self.refuse(unaligned(ClosureSigWhy::SlotsShort), e);
                    }
                    stack.push((di, ai));
                }
                (PNode::Ptr(dm, di), PNode::Ptr(am, ai)) if dm == am => stack.push((di, ai)),
                (PNode::Slice(di), PNode::Slice(ai)) | (PNode::Array(di, _), PNode::Array(ai, _)) => stack.push((di, ai)),
                (PNode::Tuple(_), PNode::Tuple(al))
                | (PNode::Record(..), PNode::Record(_, al))
                | (PNode::Union(_), PNode::Union(al))
                | (PNode::Adt(..), PNode::Adt(_, al)) => {
                    if let (PNode::Adt(dk, _), PNode::Adt(ak, _)) = (dn, an)
                        && dk != ak
                    {
                        return self.refuse(unaligned(ClosureSigWhy::AdtDiffers), e);
                    }
                    if let (PNode::Record(ds, _), PNode::Record(as_, _)) = (dn, an)
                        && ds != as_
                    {
                        return self.refuse(unaligned(ClosureSigWhy::AdtDiffers), e);
                    }
                    let (Ok(own), Ok(Kids::List(ds))) = (heads.own(d), heads.kids(d)) else { return self.refuse(unaligned(ClosureSigWhy::ColumnUnread), e) };
                    let as_ = self.list(al, e)?;
                    if ds.len() != as_.len() {
                        return self.refuse(unaligned(ClosureSigWhy::AdtDiffers), e);
                    }
                    if let PNode::Adt(ak, _) = an
                        && self.adt_facts(ak, e)?.lifetimes != own.get()
                    {
                        return self.refuse(unaligned(ClosureSigWhy::AdtDiffers), e);
                    }
                    for _ in 0..own.get() {
                        if !take(&mut at, &mut *owner, &mut *out) {
                            return self.refuse(unaligned(ClosureSigWhy::SlotsShort), e);
                        }
                    }
                    for k in (0..ds.len()).rev() {
                        stack.push((ds[k], as_[k]));
                    }
                }
                (
                    PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str | PNode::Never | PNode::ConstValue { .. } | PNode::ConstParam(_),
                    _,
                ) => {
                    if self.npos(a, e)? != 0 {
                        return self.refuse(unaligned(ClosureSigWhy::ScalarHolds), e);
                    }
                }
                _ => return self.refuse(unaligned(ClosureSigWhy::Shape { declared: node_kind(dn), closure: node_kind(an) }), e),
            }
        }
        if at != s.len() || to_u32(out.len() - first) != n {
            return self.refuse(unaligned(ClosureSigWhy::CountsDiffer), e);
        }
        Ok(())
    }

    /// The head whose clause `nth` expected a closure's signature, by the use the type check
    /// filed the clause at (`Cause::Clause { expr, .. }`): a method call's own callee row, else
    /// the fn a value path names.
    fn clause_owner(&self, expr: ExprId, nth: u32, e: ExprId) -> Result<ItemKey, Refusal> {
        if let Some((k, _)) = self.callee_of(expr) {
            return Ok(k);
        }
        match self.expr_def(expr) {
            Some(blaze_typeck::Def::Fn(k)) => Ok(k),
            _ => self.refuse(Form::ClosureClauseUnread { nth }, e),
        }
    }

    /// How many positions of closure row `row`'s VALUE are its signature's (the owner's
    /// positions of `Lower::sig_slots`), after its captures'.
    pub(crate) fn owner_sig_count(&self, row: usize, e: ExprId) -> Result<u32, Refusal> {
        // The witness lowering (one_call.rs `Erased`) reads no clause: the value is laid as its
        // captures alone, the same count at every caller (`npos`, `closure_value`).
        if !R::RELATES {
            return Ok(0);
        }
        let mut slots: Vec<SigSlot, A> = Vec::new_in(self.alloc);
        self.sig_slots(row, e, &mut slots)?;
        Ok(to_u32(slots.iter().filter(|s| matches!(s, SigSlot::Owner(_))).count()))
    }

    /// Whether closure row `row`'s body needs a relation on one of its owner signature
    /// positions (a requirement with a [`Need::Sig`] end): it holds only where every call
    /// binds those positions to the value's.
    fn sig_needs(&self, row: usize, e: ExprId) -> Result<bool, Refusal> {
        let Some(&ce) = self.ck.closures.column::<0>().get(row) else { return self.refuse(Form::Malformed, e) };
        let facts: &'b F = self.facts;
        Ok(facts.closure_requirements(ce).iter().any(|r| r.from.is_sig() || r.into.is_sig()))
    }

    /// Whether closure row `row`'s signature was expected by a clause of the call `e` itself
    /// (its method call, or the path its call names).
    pub(crate) fn sig_from_call(&self, row: usize, e: ExprId) -> Result<bool, Refusal> {
        let Some(&from) = self.ck.closures.column::<4>().get(row) else { return self.refuse(Form::Malformed, e) };
        let SigFrom::Clause { expr, .. } = from else { return Ok(false) };
        Ok(expr == e || matches!(self.body.exprs.get(e.ix()), Some(Expr::Call { callee, .. }) if *callee == expr))
    }

    /// A DIRECT call of the closure value laid at `cp` (`t` its type; its first `skip`
    /// positions the reference `call_done` made to it), with `args` and result `dest`
    /// (callee_value.rs): each argument's positions related both ways with its input's
    /// signature positions, the result's with the output's. An owner position is the value's
    /// own; a binder variable is one fresh origin per call; a universal region of its own
    /// relates nothing (the body proved what it returns for every choice of it).
    pub(crate) fn closure_call_sig(&mut self, t: PTy, cp: Place, skip: u32, args: &[Val], dest: Place, e: ExprId) -> Result<(), Refusal> {
        let PNode::Closure { owner, ordinal, .. } = self.node(t, e)? else { return self.refuse(Form::Malformed, e) };
        if owner != self.body.owner {
            return self.refuse(Form::TypeNode, e);
        }
        let row = to_usize(ordinal);
        let ck: &'c Checked<'c> = self.ck;
        let Some(&sig) = ck.closures.column::<2>().get(row) else { return self.refuse(Form::Malformed, e) };
        let all: &'c [PTy] = match self.node(sig, e)? {
            PNode::FnPtr(l, _) => self.list(l, e)?,
            _ => return self.refuse(Form::Malformed, e),
        };
        let Some((&output, inputs)) = all.split_last() else { return self.refuse(Form::Malformed, e) };
        if inputs.len() != args.len() {
            return self.refuse(Form::ClosureCallUnaligned { arg: to_u32(args.len()) }, e);
        }
        let mut slots: Vec<SigSlot, A> = Vec::new_in(self.alloc);
        self.sig_slots(row, e, &mut slots)?;
        let nsig = to_u32(slots.iter().filter(|s| matches!(s, SigSlot::Owner(_))).count());
        let ci = self.info(cp);
        if !ci.exact || ci.len < skip + nsig {
            return self.refuse(Form::Malformed, e);
        }
        // The owner positions are the value's last (`closure_value`).
        let base = ci.start + ci.len - nsig;
        let mut vars: Vec<((u32, u32), Origin), A> = Vec::new_in(self.alloc);
        let mut shared = false;
        let mut at = 0usize;
        for i in 0..=args.len() {
            let (place, n) = if i < args.len() {
                let n = to_usize(self.npos(inputs[i], e)?);
                match args[i].at {
                    At::Place(p) => (Some(p), n),
                    At::Const => (None, n),
                }
            } else {
                (Some(dest), to_usize(self.npos(output, e)?))
            };
            let Some(part) = slots.get(at..at + n) else { return self.refuse(Form::Malformed, e) };
            let binds = part.iter().any(|s| !matches!(s, SigSlot::Late(None)));
            if let (Some(p), true) = (place, binds) {
                let s = self.info(p);
                if !s.exact || to_usize(s.len) != n {
                    return self.refuse(Form::ClosureCallUnaligned { arg: to_u32(i) }, e);
                }
                for j in 0..n {
                    let target = match slots[at + j] {
                        SigSlot::Owner(k) => self.origs[to_usize(base + k)],
                        SigSlot::Static => self.universal(Region::Static),
                        SigSlot::Late(Some(v)) => match vars.iter().find(|(w, _)| *w == v) {
                            Some(&(_, o)) => {
                                shared = true;
                                o
                            }
                            None => {
                                let o = self.b.origin(false);
                                vars.push((v, o));
                                o
                            }
                        },
                        SigSlot::Late(None) | SigSlot::InnerBound(..) => continue,
                    };
                    let here = self.origs[to_usize(s.start + to_u32(j))];
                    self.b.subset(here, target);
                    self.b.subset(target, here);
                }
            }
            at += n;
        }
        if at != slots.len() {
            return self.refuse(Form::Malformed, e);
        }
        // A binder variable tied both ways to two or more positions makes them equal; Rust's
        // call relates each argument into it and it into the result, one way: noted.
        if shared {
            self.coarse_step(Form::Unified { why: crate::Unified::ClosureCallSig }, e);
        }
        Ok(())
    }

    /// A call `e` of `callee` by its generic signature (generic_call.rs) with type arguments
    /// `args`: each closure of this body among them has its owner signature positions
    /// related, both ways, with every position of every other type argument and with the
    /// call's instance of every lifetime parameter the callee's clauses write. The callee
    /// calls the closure only with values its clauses describe, built from those (the column
    /// holds no type to say which): a superset of the clause's own relation, more demanded,
    /// never less. `origin(p, k)`: the call's origin of position `k` of type argument `p`.
    ///
    /// A closure whose signature was expected elsewhere and whose body needs a relation on an
    /// owner position is refused when a clause of the callee binds a region
    /// (`Form::ClosureSigHigherRanked`): the callee may then call it with a region of its own
    /// choosing, which no owner region stands for.
    pub(crate) fn tie_closure_sigs(
        &mut self,
        callee: ItemKey,
        args: &[PTy],
        inst: &mut Vec<(Region, Origin), A>,
        mut origin: impl FnMut(&mut Self, u32, u32) -> Option<Origin>,
        e: ExprId,
    ) -> Result<(), Refusal> {
        let mut params: Option<Vec<Region, A>> = None;
        for p in 0..args.len() {
            let PNode::Closure { owner, ordinal, .. } = self.node(args[p], e)? else { continue };
            if owner != self.body.owner {
                continue;
            }
            let row = to_usize(ordinal);
            let nsig = self.owner_sig_count(row, e)?;
            if nsig == 0 {
                continue;
            }
            let Some(ncap) = self.npos(args[p], e)?.checked_sub(nsig) else { return self.refuse(Form::Malformed, e) };
            // The callee's clause regions, read once a call: the lifetime parameters they
            // write, and whether any is bound by a binder.
            let written = match params.take() {
                Some(w) => w,
                None => {
                    let facts: &'b F = self.facts;
                    let clauses = match facts.regions().clauses(callee) {
                        Ok(c) => c,
                        Err(u) => return self.refuse(Form::RegionsUnread { item: u.item, why: u.why }, e),
                    };
                    let mut w: Vec<Region, A> = Vec::new_in(self.alloc);
                    let mut binds = false;
                    for c in clauses {
                        for &r in c.col.as_slice() {
                            binds |= is_late(&r);
                            if matches!(r, Region::Param(_)) && !w.contains(&r) {
                                w.push(r);
                            }
                        }
                    }
                    // A binder region is kept as the list's marker (`Late` is never instanced).
                    if binds {
                        w.push(Region::Late { binder: 0, var: 0 });
                    }
                    w
                }
            };
            let binds = written.iter().any(is_late);
            if binds && !self.sig_from_call(row, e)? && self.sig_needs(row, e)? {
                return self.refuse(Form::ClosureSigHigherRanked { callee }, e);
            }
            if self.tie_closure_sig_exact(callee, to_u32(p), row, ncap, nsig, args, inst, &mut origin, e)? {
                params = Some(written);
                continue;
            }
            // Each owner position is tied both ways to every position below: two or more make
            // them all equal, a superset of the clause's relation: noted (not refused).
            let mut ties = 0u32;
            for q in 0..args.len() {
                if q != p {
                    ties = ties.saturating_add(self.npos(args[q], e)?);
                }
            }
            ties = ties.saturating_add(to_u32(written.iter().filter(|r| !is_late(*r)).count()));
            if ties > 1 {
                self.coarse_step(Form::Unified { why: crate::Unified::ClosureSigTies }, e);
            }
            for k in 0..nsig {
                let Some(o) = origin(self, to_u32(p), ncap + k) else { return self.refuse(Form::Malformed, e) };
                for q in 0..args.len() {
                    if q == p {
                        continue;
                    }
                    let n = self.npos(args[q], e)?;
                    for j in 0..n {
                        let Some(a) = origin(self, to_u32(q), j) else { return self.refuse(Form::Malformed, e) };
                        self.b.subset(a, o);
                        self.b.subset(o, a);
                    }
                }
                for &r in written.iter() {
                    if is_late(&r) {
                        continue;
                    }
                    let x = self.instance(inst, r);
                    self.b.subset(x, o);
                    self.b.subset(o, x);
                }
            }
            params = Some(written);
        }
        Ok(())
    }

    /// Attempts to tie closure `args[p]`'s owner signature slots (`0..nsig`) directly against the
    /// callee's `Fn*` and `Output` clauses for `Param(p)` (e.g. `Option::map<T, U, F: FnOnce(T) -> U>`
    /// ties `T` only to the closure input and `U` only to its output, never equating `T` and `U`).
    /// Returns `Ok(true)` when every owner slot was matched precisely; `Ok(false)` to fall back to
    /// the conservative all-arguments tie.
    #[expect(clippy::too_many_arguments, reason = "callee, closure parameter index/row/counts, call args, instances and origin lookup")]
    fn tie_closure_sig_exact(
        &mut self,
        callee: ItemKey,
        p: u32,
        row: usize,
        ncap: u32,
        nsig: u32,
        args: &[PTy],
        inst: &mut Vec<(Region, Origin), A>,
        origin: &mut impl FnMut(&mut Self, u32, u32) -> Option<Origin>,
        e: ExprId,
    ) -> Result<bool, Refusal> {
        let facts: &'b F = self.facts;
        let Ok(clauses) = facts.regions().clauses(callee) else {
            return Ok(false);
        };
        let ck: &'c Checked<'c> = self.ck;
        let Some(&sig_ty) = ck.closures.column::<2>().get(row) else {
            return Ok(false);
        };
        let all: &'c [PTy] = match self.node(sig_ty, e)? {
            PNode::FnPtr(l, _) => self.list(l, e)?,
            _ => return Ok(false),
        };
        let Some((&act_out, act_ins)) = all.split_last() else {
            return Ok(false);
        };
        let mut cslots: Vec<SigSlot, A> = Vec::new_in(self.alloc);
        self.sig_slots(row, e, &mut cslots)?;
        let mut nin = 0u32;
        for &t in act_ins {
            nin += self.npos(t, e)?;
        }
        let nout = self.npos(act_out, e)?;
        if cslots.len() != to_usize(nin + nout) {
            return Ok(false);
        }
        let arity = crate::one_call::FactsArity { facts };
        let heads = blaze_borrow::Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        let out_name = blaze_typeck::Name::of("Output");
        let mut args_clause: Option<(PTy, &[Region])> = None;
        let mut out_clause: Option<(PTy, &[Region])> = None;
        for c in clauses {
            let ct = c.col.ty();
            match blaze_borrow::RegionTypes::node(&heads, ct) {
                Ok(PNode::ExTrait(tr, _)) if facts.is_fn_trait(tr) => {
                    let Ok(kids) = blaze_borrow::RegionTypes::kids(&heads, ct) else { continue };
                    let (Some(subj), Some(tup)) = (kids.get(0), kids.get(1)) else { continue };
                    if !matches!(blaze_borrow::RegionTypes::node(&heads, subj), Ok(PNode::Param(q)) if q == p) {
                        continue;
                    }
                    let Ok(PNode::Tuple(_)) = blaze_borrow::RegionTypes::node(&heads, tup) else { continue };
                    let Ok(tkids) = blaze_borrow::RegionTypes::kids(&heads, tup) else { continue };
                    if to_usize(tkids.len()) != act_ins.len() {
                        continue;
                    }
                    if let Some(tslots) = c.col.child(1) {
                        args_clause = Some((tup, tslots));
                    }
                }
                Ok(PNode::ExProj { trait_: tr, name, term, .. }) if facts.is_fn_trait(tr) && name == out_name => {
                    let Ok(kids) = blaze_borrow::RegionTypes::kids(&heads, ct) else { continue };
                    let (Some(subj), Some(tup)) = (kids.get(0), kids.get(1)) else { continue };
                    if !matches!(blaze_borrow::RegionTypes::node(&heads, subj), Ok(PNode::Param(q)) if q == p) {
                        continue;
                    }
                    let Ok(PNode::Tuple(_)) = blaze_borrow::RegionTypes::node(&heads, tup) else { continue };
                    let Ok(tkids) = blaze_borrow::RegionTypes::kids(&heads, tup) else { continue };
                    if to_usize(tkids.len()) != act_ins.len() {
                        continue;
                    }
                    if let Some(oslots) = c.col.child(2) {
                        out_clause = Some((term, oslots));
                    }
                }
                _ => {}
            }
        }
        let Some((decl_tup, decl_tup_slots)) = args_clause else {
            return Ok(false);
        };
        let mut work: Vec<(PTy, &[Region], PTy, usize, usize), A> = Vec::new_in(self.alloc);
        // Push output first (so inputs pop first; order does not affect constraints).
        if nout > 0 {
            let Some((decl_out, decl_out_slots)) = out_clause else {
                return Ok(false);
            };
            work.push((decl_out, decl_out_slots, act_out, to_usize(nin), to_usize(nin + nout)));
        }
        // Push each input pair from `ArgsTuple` alongside `act_ins`.
        let Ok(tkids) = blaze_borrow::RegionTypes::kids(&heads, decl_tup) else {
            return Ok(false);
        };
        let mut d_off = 0usize;
        let mut c_off = 0usize;
        for (i, &act_in) in act_ins.iter().enumerate() {
            let Some(dk) = tkids.get(to_u32(i)) else { return Ok(false) };
            let Ok(dc) = blaze_borrow::RegionTypes::count(&heads, dk) else { return Ok(false) };
            let dn = to_usize(dc.get());
            let Some(dslice) = decl_tup_slots.get(d_off..d_off + dn) else { return Ok(false) };
            d_off += dn;
            let cn = to_usize(self.npos(act_in, e)?);
            work.push((dk, dslice, act_in, c_off, c_off + cn));
            c_off += cn;
        }
        if d_off != decl_tup_slots.len() || c_off != to_usize(nin) {
            return Ok(false);
        }
        // Collect `(owner_k, target_origin)` ties.
        let mut pairs: Vec<(u32, Origin), A> = Vec::new_in(self.alloc);
        let mut seen: Vec<u32, A> = Vec::new_in(self.alloc);
        seen.resize(to_usize(nsig), 0u32);
        let mut steps = 0u32;
        while let Some((decl, dslots, act, clo, chi)) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return Ok(false);
            }
            let slice = &cslots[clo..chi];
            if !slice.iter().any(|s| matches!(s, SigSlot::Owner(_))) {
                continue;
            }
            let Ok(dnode) = blaze_borrow::RegionTypes::node(&heads, decl) else {
                return Ok(false);
            };
            match (dnode, self.node(act, e)?) {
                (PNode::Param(q), _) => {
                    let Some(&arg_ty) = args.get(to_usize(q)) else { return Ok(false) };
                    let n = self.npos(arg_ty, e)?;
                    if to_usize(n) != slice.len() {
                        return Ok(false);
                    }
                    for (j, &ss) in slice.iter().enumerate() {
                        if let SigSlot::Owner(k) = ss {
                            if k >= nsig {
                                return Ok(false);
                            }
                            let Some(t) = origin(self, q, to_u32(j)) else { return Ok(false) };
                            pairs.push((k, t));
                            seen[to_usize(k)] = seen[to_usize(k)].saturating_add(1);
                        }
                    }
                }
                (PNode::Ref(_, di), PNode::Ref(_, ai)) => {
                    let Some(&r0) = dslots.first() else { return Ok(false) };
                    if let Some(&SigSlot::Owner(k)) = slice.first() {
                        if k >= nsig {
                            return Ok(false);
                        }
                        let t = match r0 {
                            Region::Param(_) => self.instance(inst, r0),
                            Region::Static => self.universal(Region::Static),
                            Region::Late { .. } => return Ok(false),
                        };
                        pairs.push((k, t));
                        seen[to_usize(k)] = seen[to_usize(k)].saturating_add(1);
                    }
                    work.push((di, &dslots[1..], ai, clo + 1, chi));
                }
                (PNode::Ptr(_, di), PNode::Ptr(_, ai))
                | (PNode::Slice(di), PNode::Slice(ai))
                | (PNode::Array(di, _), PNode::Array(ai, _))
                | (PNode::Pat(di, _), PNode::Pat(ai, _)) => {
                    work.push((di, dslots, ai, clo, chi));
                }
                (PNode::Tuple(_), PNode::Tuple(al)) => {
                    let Ok(dkids) = blaze_borrow::RegionTypes::kids(&heads, decl) else { return Ok(false) };
                    let alist = self.list(al, e)?;
                    if to_usize(dkids.len()) != alist.len() {
                        return Ok(false);
                    }
                    let mut doff = 0usize;
                    let mut coff = clo;
                    for (i, &ak) in alist.iter().enumerate() {
                        let Some(dk) = dkids.get(to_u32(i)) else { return Ok(false) };
                        let Ok(dc) = blaze_borrow::RegionTypes::count(&heads, dk) else { return Ok(false) };
                        let dn = to_usize(dc.get());
                        let Some(ds) = dslots.get(doff..doff + dn) else { return Ok(false) };
                        doff += dn;
                        let cn = to_usize(self.npos(ak, e)?);
                        work.push((dk, ds, ak, coff, coff + cn));
                        coff += cn;
                    }
                    if doff != dslots.len() || coff != chi {
                        return Ok(false);
                    }
                }
                (PNode::Adt(dk, _), PNode::Adt(ak, al)) if dk == ak => {
                    let lts = to_usize(self.adt_facts(ak, e)?.lifetimes);
                    if dslots.len() < lts || slice.len() < lts {
                        return Ok(false);
                    }
                    for i in 0..lts {
                        if let SigSlot::Owner(k) = slice[i] {
                            if k >= nsig {
                                return Ok(false);
                            }
                            let t = match dslots[i] {
                                Region::Param(_) => self.instance(inst, dslots[i]),
                                Region::Static => self.universal(Region::Static),
                                Region::Late { .. } => return Ok(false),
                            };
                            pairs.push((k, t));
                            seen[to_usize(k)] = seen[to_usize(k)].saturating_add(1);
                        }
                    }
                    let Ok(dkids) = blaze_borrow::RegionTypes::kids(&heads, decl) else { return Ok(false) };
                    let alist = self.list(al, e)?;
                    if to_usize(dkids.len()) != alist.len() {
                        return Ok(false);
                    }
                    let mut doff = lts;
                    let mut coff = clo + lts;
                    for (i, &ak_ty) in alist.iter().enumerate() {
                        let Some(dk_ty) = dkids.get(to_u32(i)) else { return Ok(false) };
                        let Ok(dc) = blaze_borrow::RegionTypes::count(&heads, dk_ty) else { return Ok(false) };
                        let dn = to_usize(dc.get());
                        let Some(ds) = dslots.get(doff..doff + dn) else { return Ok(false) };
                        doff += dn;
                        let cn = to_usize(self.npos(ak_ty, e)?);
                        work.push((dk_ty, ds, ak_ty, coff, coff + cn));
                        coff += cn;
                    }
                    if doff != dslots.len() || coff != chi {
                        return Ok(false);
                    }
                }
                (PNode::Alias { .. } | PNode::Opaque(..), _) => {
                    // Collect the callee type parameters inside the projection/opaque's args
                    // (e.g. `Self` in `Self::Item`), plus any non-late regions in `dslots`.
                    let mut qparams: Vec<u32, A> = Vec::new_in(self.alloc);
                    let mut pstack: Vec<PTy, A> = Vec::new_in(self.alloc);
                    if let Ok(dkids) = blaze_borrow::RegionTypes::kids(&heads, decl) {
                        for j in 0..dkids.len() {
                            if let Some(kid) = dkids.get(j) {
                                pstack.push(kid);
                            }
                        }
                    }
                    let mut psteps = 0u32;
                    while let Some(x) = pstack.pop() {
                        psteps += 1;
                        if psteps > MOST_POSITIONS {
                            return Ok(false);
                        }
                        match blaze_borrow::RegionTypes::node(&heads, x) {
                            Ok(PNode::Param(q)) => {
                                if !qparams.contains(&q) {
                                    qparams.push(q);
                                }
                            }
                            Ok(_) => {
                                if let Ok(ks) = blaze_borrow::RegionTypes::kids(&heads, x) {
                                    for j in 0..ks.len() {
                                        if let Some(kid) = ks.get(j) {
                                            pstack.push(kid);
                                        }
                                    }
                                }
                            }
                            Err(_) => return Ok(false),
                        }
                    }
                    for &ss in slice {
                        let SigSlot::Owner(k) = ss else { continue };
                        if k >= nsig {
                            return Ok(false);
                        }
                        for &q in &qparams {
                            let Some(&arg_ty) = args.get(to_usize(q)) else { return Ok(false) };
                            let n = self.npos(arg_ty, e)?;
                            for j in 0..n {
                                let Some(t) = origin(self, q, j) else { return Ok(false) };
                                pairs.push((k, t));
                                seen[to_usize(k)] = seen[to_usize(k)].saturating_add(1);
                            }
                        }
                        for &r in dslots {
                            if is_late(&r) {
                                continue;
                            }
                            let t = if r == Region::Static { self.universal(Region::Static) } else { self.instance(inst, r) };
                            pairs.push((k, t));
                            seen[to_usize(k)] = seen[to_usize(k)].saturating_add(1);
                        }
                    }
                }
                _ => return Ok(false),
            }
        }
        // Only commit when every owner signature slot `0..nsig` was tied to at least one target.
        if seen.iter().any(|&c| c == 0) {
            return Ok(false);
        }
        if seen.iter().any(|&c| c > 1) {
            self.coarse_step(Form::Unified { why: crate::Unified::ClosureSigTies }, e);
        }
        for &(k, t) in &pairs {
            let Some(o) = origin(self, p, ncap + k) else { return self.refuse(Form::Malformed, e) };
            self.b.subset(t, o);
            self.b.subset(o, t);
        }
        Ok(true)
    }

    /// Refuses a value of type `t` holding a closure of this body whose body needs a relation
    /// on an owner signature position, where the value is made into another type (a coercion
    /// to a trait object or a fn pointer, an opaque return type): its calls are then the
    /// other type's, whose signature may bind a region per call that no owner region stands
    /// for (`Form::ClosureSigErased`). A bounded walk of `t`'s parts.
    pub(crate) fn closure_sig_kept(&self, t: PTy, e: ExprId) -> Result<(), Refusal> {
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc);
        stack.push(t);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
                PNode::Closure { owner, ordinal, .. } if owner == self.body.owner => {
                    if self.sig_needs(to_usize(ordinal), e)? {
                        return self.refuse(Form::ClosureSigErased, e);
                    }
                }
                PNode::Ref(_, i) | PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::Adt(_, l) => stack.extend_from_slice(self.list(l, e)?),
                _ => {}
            }
        }
        Ok(())
    }
}

#[inline]
fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[inline]
fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[inline]
fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
