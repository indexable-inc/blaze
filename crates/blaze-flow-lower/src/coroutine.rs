//! An async block's body as its own flow, and its exact interior (port27/tempo-run/ASYNC.md
//! D-A2, prompt P2).
//!
//! The Reference, "Async blocks": "An async block is a variant of a block expression which
//! evaluates to a future. The final expression of the block, if present, determines the result
//! value of the future." "Capture modes": "Async blocks capture variables from their environment
//! using the same capture modes as closures." "The future type that rustc generates is roughly
//! equivalent to an enum with one variant per await point, where each variant stores the data
//! needed to resume from its corresponding point." So the body is checked as a body of its own:
//! its upvars are its arguments (the block's rows of `Checked::captures`, in row order, as
//! arguments `1..=n`: by value the captured type, by reference a reference argument, every
//! position a placeholder origin, as a closure body's captures would be), and `return` in it is
//! its own return.
//!
//! `.await` (the Reference, "Await expressions", steps 1 to 6, quoted in
//! docs/v2/plan/design/coroutines.md lines 16-27: "1. Create a future by calling
//! `IntoFuture::into_future` on the future operand. 2. Evaluate the future to a future `tmp`;
//! 3. Pin `tmp` using `Pin::new_unchecked`; 4. This pinned future is then polled by calling the
//! `Future::poll` method and passing it the current task context; 5. If the call to `poll`
//! returns `Poll::Pending`, then the future returns `Poll::Pending`, suspending its state so
//! that, when the surrounding async context is re-polled, execution returns to step 3; 6.
//! Otherwise the call to `poll` must have returned `Poll::Ready`, in which case the value
//! contained in the `Poll::Ready` variant is used as the result of the await expression itself")
//! is lowered as:
//!
//! ```text
//!     fut = Call into_future(op)              // the checker's callee row of the await
//!     goto H
//! H:  StorageLive(pin); pin = &mut fut        // BorrowKind::Mut
//!     StorageLive(r); r = Call poll(move pin); StorageDead(pin)
//!     Switch r -> [R, P]
//! P:  StorageDead(r); Suspend { resume: H, cancel: C }
//! C:  the drops a `return` here makes (fut first: the innermost temporary); _0 = const; Return
//! R:  Drop(fut) if it needs one; StorageDead(fut); r is the await's value (a temporary)
//! ```
//!
//! Three choices, each stated:
//! - `fut`'s type is the record's: the type checker decided `<T as IntoFuture>::IntoFuture` when
//!   it checked the `.await` (`Checked::awaited_of`); the lowering reads it and never solves it.
//!   No row (a future not wholly known): refused by name ([`Form::AwaitOperand`]). When it is
//!   the operand's own type, `into_future` is the identity: the library's blanket impl
//!   (core/src/future/into_future.rs:138: `impl<F: Future> IntoFuture for F { type Output =
//!   F::Output; type IntoFuture = F; fn into_future(self) -> Self::IntoFuture { self } }`) is the
//!   only impl that can name `Self` there (`type IntoFuture: Future<..>` makes `Self: Future`,
//!   and the blanket impl would then overlap any other), so `fut` is related to the operand
//!   position by position. Otherwise `into_future` is the selected impl's (the type check
//!   records it as the `.await`'s callee: blaze-typeck rules/await_impl.rs), related by that
//!   known head as any call is (await_call.rs); no callee row is refused by name.
//! - `r`, the `Poll<T>` the call returns, is given the await's output type `T`: `Poll<T>` has
//!   exactly the origin positions of `T` (core/src/task/poll.rs: `pub enum Poll<T> { Ready(T),
//!   Pending }`, no lifetime parameter of its own), and the Ready payload is all of `r` there is.
//!   `Future::poll` is `fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>`
//!   (core/src/future/future.rs:113): its result does not borrow the pin, so the pin is not related
//!   to `r`. `Self::Output` may hold some of the future's own lifetimes. For an `async fn`'s
//!   call, `r` is related from its written return type at the call (await_call.rs
//!   `awaited_shadow`). For an async block of this body, `r` and `fut` are made equal: that is
//!   the block's own contract, every capture outliving its output (`run_async`). A value with no
//!   position needs nothing. Anything else is refused by name ([`Form::AwaitValueUnrelated`],
//!   `await_unrelated`). Making `fut` and `r` equal is not what the impl states: an opaque is
//!   one position holding every capture, so the value took the arguments' borrows. The task
//!   context `cx` is the resume argument, not a local of the body, and holds no loan of it: it
//!   is not an operand.
//! - On Ready the completed future is dropped as well as its storage ended: the desugaring in
//!   the Reference's step 2 makes `tmp` a value of the await, and "When an initialized variable or
//!   temporary goes out of scope, its destructor is run" ("Destructors").
//!
//! The cancel path returns no value; the move check reads the return place at every `Return`,
//! so `_0` is set to a constant (no loan, no move) before it.

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_flow::{BorrowKind, Flow, Local, Operand, Place, Proj, StmtKind};
use blaze_typeck::{BodyView, CaptureKind, Checked, Expr, ExprId, ItemKey, Mutbl, PNode, PTy, PatId, PortableTys, Range};

use crate::identity::{Identity, Role};
use crate::{At, AwaitUnrelated, BodyOut, External, Facts, Form, Lower, MOST_POSITIONS, Refusal, Region, Task, TyD, range_of};

/// An async block's body lowered as its own flow.
pub struct LoweredAsync<A: Allocator + Copy> {
    pub flow: Flow<A>,
    /// Per local of `flow`, its type in `ck.types` (`PTy`).
    pub local_ty: Vec<PTy, A>,
    /// Per suspension, in statement order, the `Expr::Await` it came from.
    pub point_of: Vec<ExprId, A>,
    /// Per local of `flow`: `Some(m)` when the local is a reference `&m T` to its `local_ty`
    /// that the record does not name (a capture's reference argument, an `.await`'s pin, a
    /// guard's fake borrow, an overloaded deref's result); `None` when `local_ty` is its type.
    pub local_wrap: Vec<Option<Mutbl>, A>,
    /// Per local of `flow`: what it stands for (identity.rs), the ONE table of identities the
    /// builder looks the locals it keeps across a suspension up in (ASYNC.md P8, async-P8-4).
    pub identity: Vec<crate::identity::Identity, A>,
    /// The block's item-relative range (what a refusal of [`interior`] names).
    pub range: Range,
    /// What each placeholder of `flow` stands for in the flow that makes the block (lane B14,
    /// the one mechanism of closure.rs [`crate::External`]): a capture's positions are the
    /// future VALUE's (`External::Capture`, laid as `Lower::npos` of `PNode::Future` lays
    /// them: each capture in row order, a capture by reference its reference first), the
    /// output's are `External::Sig` (the future's type holds no position for them), and
    /// `'static` is `External::Static`. An outlives result between two of the value's
    /// positions (or `'static`) is then a requirement the maker proves at the block
    /// expression ([`crate::requirement`], `Facts::closure_requirements` keyed by the block's
    /// expression), as a closure's is. The root block of an `async fn` has its placeholders
    /// laid by the signature (`async_fn_root`): its rows are made the same way and never read
    /// (its outlives results are the fn's own findings).
    pub externals: Vec<(blaze_flow::Origin, External), A>,
    /// The first stricter relation this flow used ([`crate::Lowered::coarse`]): the body is
    /// refused by it only if it has a finding.
    pub coarse: Option<Refusal>,
}

/// The body of async block `block` (an `Expr::AsyncBlock` of `body`) as its own flow.
pub fn lower_async<A: Allocator + Copy, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    block: ExprId,
    facts: &F,
    alloc: A,
) -> Result<LoweredAsync<A>, Refusal> {
    lower_async_with::<A, F, crate::one_call::Production>(body, ck, block, facts, false, alloc, &mut None)
}

/// [`lower_async`] under the call rule `R`; with `logged` (the oracle's comparing lowering,
/// lib.rs `lower_with`) each call's record into `log`, kept on a refusal too.
pub(crate) fn lower_async_with<A: Allocator + Copy, F: Facts, R: crate::one_call::CallRule>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    block: ExprId,
    facts: &F,
    logged: bool,
    alloc: A,
    log: &mut Option<crate::builder::Log<A>>,
) -> Result<LoweredAsync<A>, Refusal> {
    let range = range_of(body, block);
    let inner = match body.exprs.get(block.ix()) {
        Some(Expr::AsyncBlock { body: inner, .. }) => *inner,
        _ => return Err(Refusal { form: Form::Malformed, range }),
    };
    // Only expressions evaluated by this block contribute suspension points. Evaluating a
    // closure or nested async block constructs its captures; its body has a flow of its own.
    let mut l: blaze_own::Box<Lower<'_, '_, F, A, R>, A> = Lower::new(body, ck, facts, alloc);
    l.in_nested = true;
    if logged {
        l.b.start_log(alloc);
        l.per_call = true;
    }
    l.async_root = Some(block);
    let mut externals: Vec<(blaze_flow::Origin, External), A> = Vec::new_in(alloc);
    let ran = l.run_async(block, inner, &mut externals);
    let ran = l.finish_held(ran);
    *log = l.b.take_log();
    ran?;
    // `'static`, made when the walk first met it (`Lower::universal`), as closure.rs does.
    if let Some(&(_, st)) = l.uni.iter().find(|(r, _)| *r == Region::Static) {
        externals.push((st, External::Static));
    }
    let b = core::mem::replace(&mut l.b, crate::builder::Builder::new(crate::FlowBuilder::new_in(1, 0, alloc)));
    let flow = b.finish().map_err(|_| Refusal { form: Form::Builder, range })?;
    if l.local_ty.len() != flow.locals as usize {
        return Err(Refusal { form: Form::Malformed, range });
    }
    let mut local_ty = Vec::new_in(alloc);
    let mut local_wrap = Vec::new_in(alloc);
    for t in l.local_ty.iter() {
        local_ty.push(t.ty);
        local_wrap.push(t.wrap);
    }
    let suspends = flow.stmts.iter().filter(|s| matches!(s.kind, StmtKind::Suspend { .. })).count();
    if suspends != l.points.len() {
        return Err(Refusal { form: Form::Malformed, range });
    }
    if l.identity.len() != flow.locals as usize {
        return Err(Refusal { form: Form::Malformed, range });
    }
    let point_of = core::mem::replace(&mut l.points, Vec::new_in(alloc));
    let identity = core::mem::replace(&mut l.identity, Vec::new_in(alloc));
    Ok(LoweredAsync { flow, local_ty, point_of, local_wrap, identity, range, externals, coarse: l.coarse.get() })
}

/// An async block's WITNESS (typeck.md "Witness records", W1): what its state stores, region-
/// erased: the captures (arguments `1..=args`, typed by their mode), the locals saved at each
/// suspension (`blaze_flow::saved`) and the awaitees among them. Made by [`lower_async_witness`]
/// under one_call.rs `Erased`, which relates no region, so a block whose regions the borrow
/// check's lowering refuses still has one.
///
/// It gives no `Flow` out, by construction (rule 76): its flow's origins are laid but never
/// related, so a borrow check over it would pass what it must refuse. Only the facts a witness
/// is read for leave it: [`AsyncWitness::interior_parts`], [`AsyncWitness::state_recipe`],
/// [`AsyncWitness::saved`] and the per-local columns.
pub struct AsyncWitness<A: Allocator + Copy> {
    lowered: LoweredAsync<A>,
}

impl<A: Allocator + Copy> AsyncWitness<A> {
    /// Per local, its type in `ck.types` ([`LoweredAsync::local_ty`]).
    pub fn local_ty(&self) -> &[PTy] {
        &self.lowered.local_ty
    }
    /// Per local, the reference the record does not name ([`LoweredAsync::local_wrap`]).
    pub fn local_wrap(&self) -> &[Option<Mutbl>] {
        &self.lowered.local_wrap
    }
    /// Per local, what it stands for ([`LoweredAsync::identity`]).
    pub fn identity(&self) -> &[Identity] {
        &self.lowered.identity
    }
    /// Per suspension, in statement order, its `.await` ([`LoweredAsync::point_of`]).
    pub fn point_of(&self) -> &[ExprId] {
        &self.lowered.point_of
    }
    /// The block's item-relative range.
    pub fn range(&self) -> Range {
        self.lowered.range
    }
    /// The number of locals, the return place and the captures included.
    pub fn locals(&self) -> u32 {
        self.lowered.flow.locals
    }
    /// The number of captures (locals `1..=args`).
    pub fn args(&self) -> u32 {
        self.lowered.flow.args
    }
    /// The locals saved at some suspension: `blaze_flow::saved`, the one rule of what a state
    /// stores. Past its bound, refused by name (`Form::Bound`).
    pub fn saved(&self, alloc: A) -> Result<blaze_flow::Saved<A>, Refusal> {
        blaze_flow::suspend::saved(&self.lowered.flow, alloc).map_err(|_| Refusal { form: Form::Bound, range: self.lowered.range })
    }
    /// The exact interior's elements ([`interior_parts`] over the witness).
    pub fn interior_parts(&self, ck: &Checked<'_>, block: ExprId, alloc: A) -> Result<Vec<InteriorPart, A>, Refusal> {
        parts_with(&self.lowered, ck, block, false, alloc)
    }
    /// The state recipe (state_recipe.rs over the witness).
    pub fn state_recipe<B: Allocator + Clone>(&self, body: &BodyView<'_>, alloc: B) -> Result<crate::state_recipe::StateRecipe<B>, Refusal> {
        crate::state_recipe::state_recipe(body, &self.lowered, alloc)
    }
    /// The witness's lowering, for the oracle's differential (oracle.rs `witness_differs`)
    /// alone: never outside a test (the type's doc).
    #[cfg(test)]
    pub(crate) fn lowered_for_oracle(&self) -> &LoweredAsync<A> {
        &self.lowered
    }
}

/// The WITNESS of async block `block` (an `Expr::AsyncBlock` of `body`, or an `async fn`'s
/// root block): its body lowered under one_call.rs `Erased`, relating no region and reading no
/// head's regions. What it still refuses is a FORM it does not lower or a type the layout does
/// not know (`InlineAsm`, `UnknownAdt`, `TypeNode`, `Bound`, `Malformed`), each by name; never a
/// region reason (`CallHiddenInput`, `CallOutlivesUnread`, `RegionsUnread`, a `OneRule` count,
/// `ClosureSigUnaligned`). The borrow check keeps [`lower_async`].
pub fn lower_async_witness<A: Allocator + Copy, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    block: ExprId,
    facts: &F,
    alloc: A,
) -> Result<AsyncWitness<A>, Refusal> {
    let lowered = lower_async_with::<A, F, crate::one_call::Erased>(body, ck, block, facts, false, alloc, &mut None)?;
    Ok(AsyncWitness { lowered })
}

/// A planted bug for the tests' controls.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InteriorPlant {
    None,
    /// Every local but the return place and the arguments is taken as saved: the moved-local
    /// rule (a local moved out before every suspension is not saved) is left out.
    EveryLocal,
}

/// The exact interior: the sorted, deduplicated types of the saved locals
/// (`blaze_flow::suspend::saved`) and of the captures by kind (by value `T`, by reference
/// `&T`/`&mut T`, made in `out`).
///
/// `out` holds `ck.types` as its prefix (a `fork_in` of it, or the table `ck.types` itself
/// extends): every `PTy` of the record is the same type of `out`. A type of the record that
/// `out` does not hold as the record does is refused by name ([`Form::InteriorTable`]). The
/// Reference's rule (typeck's rules/coroutine.rs doc lines 11-12): "Its auto traits (`Send`,
/// `Sync`) are its interior's: its captures and every value live across an `.await`."
pub fn interior<A: Allocator + Copy>(
    lowered: &LoweredAsync<A>,
    ck: &Checked<'_>,
    block: ExprId,
    out: &mut PortableTys<'_>,
    alloc: A,
) -> Result<PTy, Refusal> {
    interior_with(lowered, ck, block, out, false, alloc)
}

#[cfg(test)]
pub(crate) fn interior_planted<A: Allocator + Copy>(
    lowered: &LoweredAsync<A>,
    ck: &Checked<'_>,
    block: ExprId,
    out: &mut PortableTys<'_>,
    plant: InteriorPlant,
    alloc: A,
) -> Result<PTy, Refusal> {
    interior_with(lowered, ck, block, out, plant == InteriorPlant::EveryLocal, alloc)
}

/// One element of an exact interior ([`interior_parts`]): a type of the block's record
/// (`Checked::types`), behind a reference `&m` when `wrap` is `Some(m)` (a capture by reference,
/// or a local the record does not type as itself: [`LoweredAsync::local_wrap`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InteriorPart {
    pub ty: PTy,
    pub wrap: Option<Mutbl>,
}

/// The exact interior's elements, sorted by (record type, wrap) and deduplicated: the saved
/// locals (`blaze_flow::suspend::saved`, the one rule of what a state stores) and the captures
/// by kind. [`interior`] interns the same elements into a table extending the record's; a caller
/// with a table of its own (front2 opaque_auto.rs) copies each `ty` and makes the wraps there.
/// A local or capture whose type the record does not hold is refused by name
/// ([`Form::Malformed`]).
pub fn interior_parts<A: Allocator + Copy>(lowered: &LoweredAsync<A>, ck: &Checked<'_>, block: ExprId, alloc: A) -> Result<Vec<InteriorPart, A>, Refusal> {
    parts_with(lowered, ck, block, false, alloc)
}

fn parts_with<A: Allocator + Copy>(
    lowered: &LoweredAsync<A>,
    ck: &Checked<'_>,
    block: ExprId,
    every_local: bool,
    alloc: A,
) -> Result<Vec<InteriorPart, A>, Refusal> {
    let range = lowered.range;
    let refuse = |form: Form| Refusal { form, range };
    let flow = &lowered.flow;
    if lowered.local_ty.len() != flow.locals as usize || lowered.local_wrap.len() != flow.locals as usize {
        return Err(refuse(Form::Malformed));
    }
    let saved = blaze_flow::suspend::saved(flow, alloc).map_err(|_| refuse(Form::Bound))?;
    let record = ck.types.view();
    let mut parts: Vec<InteriorPart, A> = Vec::new_in(alloc);
    // The saved locals; the arguments are the captures, taken below by their kind.
    for l in flow.args + 1..flow.locals {
        if !every_local && !saved.is_saved(Local(l)) {
            continue;
        }
        let ty = lowered.local_ty[l as usize];
        record.get(ty).ok_or(refuse(Form::Malformed))?;
        parts.push(InteriorPart { ty, wrap: lowered.local_wrap[l as usize] });
    }
    // The captures: the captured PLACES' types by how each field holds its place (the
    // Reference, "Async blocks": captures as closures do; RFC 2229), the auto-trait answer
    // rustc gives. Read under `Places` whatever `ActiveEnv` is: the saved locals above are the
    // same under either layout (each is a block local; none is a capture argument), so the
    // interior is the places' before the build lays them (blaze-typeck env_fields.rs). An
    // owner the place analysis did not decide gives its whole-binding rows.
    for f in ck.env_fields_by::<blaze_typeck::Places>(block) {
        let ty = f.place_ty(ck).ok_or(refuse(Form::Malformed))?;
        record.get(ty).ok_or(refuse(Form::Malformed))?;
        let wrap = match f.how {
            CaptureKind::ByValue => None,
            CaptureKind::ByRef => Some(Mutbl::Not),
            CaptureKind::ByMutRef => Some(Mutbl::Mut),
        };
        parts.push(InteriorPart { ty, wrap });
    }
    // A binding the layout still moves whole beside its places (blaze-typeck env_fields.rs
    // `moved_whole_beside_places`): what the state holds until the stage-2 switch.
    for p in ck.moved_whole_beside_places(block) {
        let ty = *ck.local_ty.get(p.ix()).ok_or(refuse(Form::Malformed))?;
        record.get(ty).ok_or(refuse(Form::Malformed))?;
        parts.push(InteriorPart { ty, wrap: None });
    }
    parts.sort_unstable();
    let mut unique: Vec<InteriorPart, A> = Vec::new_in(alloc);
    for &p in parts.iter() {
        if unique.last() != Some(&p) {
            unique.push(p);
        }
    }
    Ok(unique)
}

fn interior_with<A: Allocator + Copy>(
    lowered: &LoweredAsync<A>,
    ck: &Checked<'_>,
    block: ExprId,
    out: &mut PortableTys<'_>,
    every_local: bool,
    alloc: A,
) -> Result<PTy, Refusal> {
    let range = lowered.range;
    let refuse = |form: Form| Refusal { form, range };
    let parts = parts_with(lowered, ck, block, every_local, alloc)?;
    let record = ck.types.view();
    if out.view().len() < record.len() {
        return Err(refuse(Form::InteriorTable));
    }
    let mut tys: Vec<PTy, A> = Vec::new_in(alloc);
    for p in parts.iter() {
        same_in(record.get(p.ty), out.view().get(p.ty)).ok_or(refuse(Form::InteriorTable))?;
        let t = match p.wrap {
            Some(m) => out.intern(PNode::Ref(m, p.ty)).map_err(|_| refuse(Form::Bound))?,
            None => p.ty,
        };
        tys.push(t);
    }
    tys.sort_unstable();
    let mut unique: Vec<PTy, A> = Vec::new_in(alloc);
    for &t in tys.iter() {
        if unique.last() != Some(&t) {
            unique.push(t);
        }
    }
    let list = out.intern_list(&unique).map_err(|_| refuse(Form::Bound))?;
    out.intern(PNode::Tuple(list)).map_err(|_| refuse(Form::Bound))
}

/// `Some(())` when the record's node and `out`'s at one index are the same node.
fn same_in(record: Option<&PNode>, out: Option<&PNode>) -> Option<()> {
    match (record, out) {
        (Some(a), Some(b)) if a == b => Some(()),
        _ => None,
    }
}

impl<F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'_, '_, F, A, R> {
    /// The walk of an async block's body `inner`: as `run`, with the block's captures as the
    /// arguments and its output as the return place. `externals` receives what each capture's
    /// and the output's placeholder stands for in the block's maker ([`LoweredAsync::externals`]).
    fn run_async(&mut self, block: ExprId, inner: ExprId, externals: &mut Vec<(blaze_flow::Origin, External), A>) -> Result<(), Refusal> {
        let ck = self.ck;
        // The block's captures, in row order: argument `1 + k` is row `k`.
        let mut rows: Vec<(PatId, CaptureKind), A> = Vec::new_in(self.alloc);
        {
            let firsts = ck.captures.column::<0>();
            let pats = ck.captures.column::<1>();
            let kinds = ck.captures.column::<2>();
            for i in 0..firsts.len() {
                if firsts[i] == block {
                    rows.push((pats[i], kinds[i]));
                }
            }
        }
        let nu = rows.len() as u32;
        self.b.replace(blaze_flow::FlowBuilder::new_in(1 + nu, nu, self.alloc));
        self.nargs = nu;
        let entry = self.b.block();
        self.ret_block = self.b.block();
        self.b.start(entry);
        self.scope_marks.push(0);
        self.scope_temps.push((self.temps.len() as u32, self.temp_marks.len() as u32));
        // The next position of the future value (`External::Capture`): a capture by reference
        // is its reference then its variable's type's positions, one by value its type's, the
        // order `Lower::npos` of `PNode::Future` counts and `closure_value` lays them in. An
        // async block is polled through `Pin<&mut Self>` and owns its captures, so no capture
        // is reached through a per-call environment reference (closure.rs `External::Env`).
        let mut value_at = 0u32;
        for k in 0..rows.len() {
            let (p, kind) = rows[k];
            let t = match ck.local_ty.get(p.ix()) {
                Some(&t) => t,
                None => return self.refuse_pat(Form::Malformed, p),
            };
            let l = Local(1 + k as u32);
            let first = self.origs.len() as u32;
            match kind {
                CaptureKind::ByValue => {
                    self.new_local_with(TyD::of(t), true, Some(l), block)?;
                    if let Some(slot) = self.pat_local.get_mut(p.ix()) {
                        *slot = l.0;
                    } else {
                        return self.refuse_pat(Form::Malformed, p);
                    }
                }
                CaptureKind::ByRef | CaptureKind::ByMutRef => {
                    let m = if kind == CaptureKind::ByRef { Mutbl::Not } else { Mutbl::Mut };
                    let rt = TyD { wrap: Some(m), ty: t };
                    self.new_local_with(rt, true, Some(l), block)?;
                    let at = self.place_of(l);
                    let place = self.project(at, rt, Proj::Deref, TyD::of(t), false, block)?;
                    self.upvar_places.push((p, place, TyD::of(t)));
                }
            }
            let made = self.origs.len() as u32;
            for o in first..made {
                externals.push((self.origs[o as usize], External::Capture(value_at + (o - first))));
            }
            value_at += made - first;
            self.vars.push(l);
        }
        // The return place: the block's output, its positions placeholders; every capture's
        // placeholder is known to outlive every one of them (the output may hold what a capture
        // by reference points to; a loan of the block's own locals or by-value captures is not
        // known to outlive it and is refused, as a returned reference to a local is).
        let out = match self.node(self.expr_ty(block)?, block)? {
            PNode::Future(fe, out, _) if fe == block => out,
            _ => return self.refuse(Form::TypeNode, block),
        };
        let upvar_origins = self.origs.len();
        self.new_local_with(TyD::of(out), true, Some(Local(0)), block)?;
        let all = self.origs.len();
        // The output's positions: no position of the future value holds them (`npos` of
        // `PNode::Future` counts the captures only), so a requirement naming one cannot be
        // stated at the block expression ([`crate::requirement`] gives `None`: refused by name).
        for o in upvar_origins..all {
            externals.push((self.origs[o], External::Sig));
        }
        if (all - upvar_origins) as u64 * upvar_origins as u64 > u64::from(MOST_POSITIONS) {
            return self.refuse(Form::Bound, block);
        }
        if block == self.body.root && self.body.coroutine.is_some() {
            // An `async fn`'s root block: its captures are the fn's parameters and its value
            // the written return type, both the signature's (`async_fn_root`).
            self.async_fn_root(&rows, block)?;
        } else {
            for u in 0..upvar_origins {
                for o in upvar_origins..all {
                    self.b.known(self.origs[u], self.origs[o]);
                }
            }
        }
        self.tasks.push(Task::RootEnd(inner));
        self.tasks.push(Task::Eval(inner, false));
        let body = self.body;
        let fuel = 32 * (body.exprs.len() + body.pats.len() + body.stmts.len()) as u64 + 256;
        let mut steps = 0u64;
        while let Some(t) = self.tasks.pop() {
            steps += 1;
            if steps > fuel {
                return self.refuse(Form::Bound, block);
            }
            self.step(t)?;
        }
        Ok(())
    }

    /// The place and type an `Expr::Local` of captured pattern `p` reads in an async block's
    /// body when the block captures it by reference: `*arg`.
    pub(crate) fn upvar_place(&self, p: PatId) -> Option<(Place, TyD)> {
        self.upvar_places.iter().find(|u| u.0 == p).map(|u| (u.1, u.2))
    }

    /// `.await` at `e`, its operand's value on the stack (the module doc's lowering).
    pub(crate) fn await_done(&mut self, e: ExprId) -> Result<(), Refusal> {
        let v = self.pop(e)?;
        if self.awaiting.last().is_some_and(|&(_, a)| a == e) {
            self.awaiting.pop();
        }
        // The awaited value's own positions, when the operand is a call related through its
        // written return type (await_call.rs `awaited_shadow`); else why not.
        let shadow = match (self.awaited.last(), v.at) {
            (Some(&(p, w)), At::Place(q)) if p == q => {
                self.awaited.pop();
                w
            }
            _ => Err((AwaitUnrelated::NotACall, None)),
        };
        if self.async_root.is_none() {
            return self.refuse(Form::Await, e);
        }
        if self.callee_of(e).is_none() {
            return self.refuse(Form::Malformed, e);
        }
        if v.ty.wrap.is_some() {
            return self.refuse(Form::AwaitOperand, e);
        }
        // The future `tmp` holds across the suspension, `<T as IntoFuture>::IntoFuture`, as the
        // type checker decided it at this `.await` (module doc): read, never solved here.
        let Some(held) = self.ck.awaited_of(e) else {
            return self.refuse(Form::AwaitOperand, e);
        };
        let span = self.span(e);
        let ft = TyD::of(held);
        // Step 1: `fut = into_future(op)`.
        let fut = self.temp(ft, e)?;
        self.name_local(fut, Identity::Temp(e, Role::Awaitee));
        let fp = self.place_of(fut);
        // The awaited value's positions live while the future does: to the poll.
        if let Ok(w) = shadow {
            let (start, n) = self.local_origs[w.0 as usize];
            for k in start..start + n {
                let o = self.origs[k as usize];
                self.b.mentions(fut.0, o);
            }
        }
        let a = self.operand(v, e)?;
        self.b.call(&[a], fp, span);
        if held == v.ty.ty {
            // The blanket impl's identity (module doc): position by position.
            if let At::Place(p) = v.at {
                self.relate(p, fp, ft, e)?;
            }
        } else {
            // The selected impl's `into_future(self) -> Self::IntoFuture`, by its head.
            self.relate_into_future(v, fp, ft, e)?;
        }
        let (head, ready, pending, cancel) = (self.b.block(), self.b.block(), self.b.block(), self.b.block());
        self.b.goto(head, span);
        // Steps 3 and 4: pin, poll.
        self.b.start(head);
        let pin = self.new_local_with(TyD { wrap: Some(Mutbl::Mut), ty: held }, false, None, e)?;
        self.name_local(pin, Identity::Temp(e, Role::Pin));
        self.b.stmt(StmtKind::StorageLive(pin), span);
        self.borrow(fp, ft, BorrowKind::Mut, pin, e)?;
        let out = TyD::of(self.expr_ty(e)?);
        let r = self.new_local_with(out, false, None, e)?;
        self.name_local(r, Identity::Temp(e, Role::Result));
        self.b.stmt(StmtKind::StorageLive(r), span);
        let rp = self.place_of(r);
        let pp = self.place_of(pin);
        self.b.call(&[Operand::Move(pp)], rp, span);
        // The result is `Future::Output`. Of an `async fn`'s call it is the written return
        // type at the call's instantiation, made at the call (`awaited_shadow`): related from
        // those positions, never from the future's, which hold every borrow it captured.
        match shadow {
            Ok(w) => {
                let wp = self.place_of(w);
                self.relate(wp, rp, out, e)?;
            }
            Err(why) => self.await_unrelated(why, held, v.ty.ty, fp, rp, e)?,
        }
        self.b.stmt(StmtKind::StorageDead(pin), span);
        self.b.switch(rp, &[ready, pending], span);
        // Step 5: Pending suspends; polled again, back to the head.
        self.b.start(pending);
        self.b.stmt(StmtKind::StorageDead(r), span);
        self.b.suspend(head, cancel);
        self.points.push(e);
        // Dropped while parked here: what a `return` here drops, `fut` first.
        self.b.start(cancel);
        self.exit_drops(0, 0, e)?;
        let ret = self.place_of(Local(0));
        self.b.assign(ret, Operand::Const, span);
        self.b.ret(span);
        // Step 6: Ready; the future is done with, `r` is the await's value.
        self.b.start(ready);
        if self.temps.last() != Some(&fut) {
            return self.refuse(Form::Malformed, e);
        }
        self.temps.pop();
        self.drop_local(fut, e)?;
        self.temps.push(r);
        self.push_place(rp, out);
        Ok(())
    }

    /// The poll's result `rp` of the `.await` at `e`, whose value no written return type
    /// relates (`why`, with the awaited call's head when it has one). `held` is the future's
    /// type; `operand` is the operand's type; `fp` is the future.
    ///
    /// - An async block of this body awaited as its own future (`PNode::Future`, the identity
    ///   `IntoFuture`): its value is related to the future's positions, which are exactly the
    ///   block's captures. That is the block's own contract: `run_async` checked its body with
    ///   every capture outliving every position of its output.
    /// - A value with no position: nothing to relate.
    /// - Anything else is made equal to the future's positions, a STRICTER relation than
    ///   Rust's (every lifetime of the `Output` is one of the future type's or `'static`, E0207
    ///   and "Capturing"), noted as [`Form::AwaitValueUnrelated`] ([`crate::Lowered::coarse`]):
    ///   a body clean under it stands checked; one with a finding is refused by that name, its
    ///   findings dropped. An opaque future is one position holding every capture, so the value
    ///   took the arguments' borrows (tempo tip20_channel_reserve.rs:229, 1,386 rows at one site
    ///   in c2350wave75): such an artifact is now that refusal, never a finding (rule 3).
    ///
    /// The witness lowering (one_call.rs `Erased`) relates as before: it reads no region.
    fn await_unrelated(&mut self, why: (AwaitUnrelated, Option<ItemKey>), held: PTy, operand: PTy, fp: Place, rp: Place, e: ExprId) -> Result<(), Refusal> {
        if !R::RELATES {
            self.unify(&[fp, rp]);
            return Ok(());
        }
        if held == operand && matches!(self.node(held, e)?, PNode::Future(..)) {
            // The block's body proved each capture outlives each position of its value; equal
            // both ways (and the captures to each other) is stricter: noted.
            let (f, r) = (self.info(fp).len, self.info(rp).len);
            if r > 0 && f + r > 2 {
                self.coarse_step(Form::Unified { why: crate::Unified::AsyncBlockOutput }, e);
            }
            self.unify(&[fp, rp]);
            return Ok(());
        }
        if self.info(rp).len == 0 {
            return Ok(());
        }
        let (why, callee) = why;
        let why = if held != operand { AwaitUnrelated::NotIdentity } else { why };
        self.coarse_step(Form::AwaitValueUnrelated { why, callee }, e);
        self.unify(&[fp, rp]);
        Ok(())
    }
}

impl<'b, F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'b, '_, F, A, R> {
    /// The root block of an `async fn`, its captures (`rows`, arguments `1..`) and return place
    /// (its value) laid: the Reference, "Async functions": `async fn f(x: &u8) -> T` is "roughly
    /// equivalent to" `fn f<'a>(x: &'a u8) -> impl Future<Output = T> + 'a`, its body an `async
    /// move` block that captures the parameters and whose value is the future's `Output`. So the
    /// body's value must be a `T` as written, its regions the signature's (RFC 2394), exactly as
    /// a fn body's returned value is (`Lower::signature`): each capture's positions are made
    /// equal to its parameter's regions, the value's to the written return type's
    /// (`BodyOut::Async`), and what is known among them is the signature's (its declared bounds,
    /// `'static`, the implied bounds of its types). An async block's own rule, every capture
    /// outliving the output, is not this: it let `async fn f(x: &u8) -> &'static u8 { x }`
    /// through. A capture that is not a whole parameter by value, or a signature not read, is
    /// refused by name; with no lifetime part, a value with no position needs nothing.
    fn async_fn_root(&mut self, rows: &[(PatId, CaptureKind)], block: ExprId) -> Result<(), Refusal> {
        // The witness lowering (one_call.rs `Erased`) reads no signature: nothing below is
        // anything but regions.
        if !R::RELATES {
            return Ok(());
        }
        let body = self.body;
        let facts: &'b F = self.facts;
        let (out_start, out_n) = self.local_origs[0];
        let sig = match facts.sig(body.owner) {
            Ok(sig) => sig,
            Err(_) if out_n == 0 => return Ok(()),
            Err(unread) => return self.refuse(Form::RegionsUnread { item: unread.item, why: unread.why }, block),
        };
        // The written return type's column (D148: the head's `written_output`), laid as the
        // flow's positions of its own type: a position the flow gives an opaque type or a
        // projection, or a count that differs, is refused by name, never read as another's.
        let Some(written_col) = sig.regions().written_output else {
            return self.refuse(Form::AsyncOutputUnread, block);
        };
        let BodyOut::Async(_) = sig.body() else {
            return self.refuse(Form::SigUnaligned, block);
        };
        if !sig.well_formed() || sig.inputs() as usize != body.params.len() {
            return self.refuse(Form::SigUnaligned, block);
        }
        let mut laid: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        // The written return type's column over the block's value's own type (`lay_over`); a
        // hidden position is a placeholder of its own below.
        let mut written: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        let out_ty = self.local_ty[0].ty;
        if !self.lay_over(written_col.ty(), out_ty, written_col.as_slice(), &mut written, block)? {
            return self.refuse(Form::SigUnaligned, block);
        }
        if written.len() != out_n as usize {
            return self.refuse(Form::SigPositionsUnaligned { positions: out_n as usize, regions: written.len(), site: crate::SigSite::AsyncOutput }, block);
        }
        // Each capture: its parameter's regions, position by position (input `i`'s child of
        // the column, laid over the declared input type).
        for (k, &(p, kind)) in rows.iter().enumerate() {
            let Some(i) = body.params.iter().position(|&q| q == p) else {
                return self.refuse_pat(Form::AsyncFnCapture, p);
            };
            if kind != CaptureKind::ByValue {
                return self.refuse_pat(Form::AsyncFnCapture, p);
            }
            let (start, n) = self.local_origs[1 + k];
            let Some(slots) = sig.input(i as u32) else {
                return self.refuse(Form::SigUnaligned, block);
            };
            let declared = self.sig_declared(sig, i as u32, block)?;
            // Laid over the capture's own type in this flow (`lay_over`: a declared projection
            // the body normalized is hidden positions, not a count that differs).
            let capture_ty = self.local_ty[1 + k].ty;
            let input = Some(i as u32);
            if !self.lay_over(declared, capture_ty, slots, &mut laid, block)? {
                return self.refuse(Form::SigPositionsUnaligned { positions: n as usize, regions: slots.len(), site: crate::SigSite::Unlaid { input } }, block);
            }
            if laid.len() != n as usize {
                return self.refuse(Form::SigPositionsUnaligned { positions: n as usize, regions: laid.len(), site: crate::SigSite::Count { input } }, block);
            }
            for j in 0..n as usize {
                // A hidden position (an opaque, a projection, or inside a normalization of
                // one: `lay_over`) is a placeholder of its own, as `Lower::signature` lays it.
                let u = match laid.get(j) {
                    Some(&Some(r)) => self.universal(r),
                    Some(&None) => self.hidden_placeholder(),
                    None => return self.refuse(Form::SigUnaligned, block),
                };
                let o = self.origs[start as usize + j];
                self.b.known(o, u);
                self.b.known(u, o);
            }
        }
        // The value: the written return type's regions.
        for (j, &r) in written.iter().enumerate() {
            let u = match r {
                Some(r) => self.universal(r),
                None => self.hidden_placeholder(),
            };
            let o = self.origs[out_start as usize + j];
            self.b.known(o, u);
            self.b.known(u, o);
        }
        // The implied bounds of the captures' and the value's types (`&'a T`: `T: 'a`).
        let mut ranges: Vec<(u32, u32), A> = Vec::new_in(self.alloc);
        for l in 0..=rows.len() {
            let (start, _) = self.local_origs[l];
            let t = self.local_ty[l];
            self.ref_ranges(t, block, &mut ranges)?;
            for j in 0..ranges.len() {
                let (at, inner) = ranges[j];
                let outer = self.origs[(start + at) as usize];
                for k in 1..=inner {
                    let o = self.origs[(start + at + k) as usize];
                    if o != outer {
                        self.b.known(o, outer);
                    }
                }
            }
        }
        // The declared bounds.
        for &(a, b) in sig.bounds() {
            let (oa, ob) = (self.universal(a), self.universal(b));
            self.b.known(oa, ob);
        }
        Ok(())
    }
}
