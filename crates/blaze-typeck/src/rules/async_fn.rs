//! What an async block's future HOLDS, for the code builder and for its auto traits
//! (docs/v2/plan/design/coroutines.md, "Front-end writer 6's notes"). The typing rules of
//! `async` and `.await` are rules/coroutine.rs's, rules/async_fmt.rs's and
//! rules/opaque_def.rs's; this file adds the two facts they left owed.
//!
//! 1. CAPTURE ROWS. The Reference, "Async blocks", "Capture modes": "Async blocks capture
//!    variables from their environment using the same capture modes as closures. Like
//!    closures, when written `async { .. }` the capture mode for each variable will be inferred
//!    from the content of the block. `async move { .. }` blocks however will move all
//!    referenced variables into the resulting future." So an async block's captures are found
//!    by the closure rule's own walk (rules/closure.rs `visit_for_captures`: one concept, one
//!    home) and written to the same table (`Checked::captures`, the block's expression in the
//!    first column): the builder's `coroutine_facts` reads the block's upvars from those rows.
//!    An `async fn`'s parameters are captured by its root `async move` block (the Reference,
//!    "Async functions"), so without these rows no `async fn` with a parameter builds.
//!
//!    At construction the future holds its proven captures and, for an
//!    async closure call, the arguments of the call.
//!
//! 2. An auto trait that fails on a provisional interior needs the flow witness. The
//!    Reference describes a future as storing "the data needed to resume from its
//!    corresponding point". A local moved before every suspension contributes no storage.
//!    The flow layer computes this from the same saved locals it uses for the state layout.

use blaze_own::{Alloc, Vec};

use crate::body::{Capture, Expr};
use crate::diag::At;
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, PatId};
use crate::machine::{Lower, Machine};
use crate::ty::{Cause, Goal, Obligation, Ty, TyView, TypeCtx};
use crate::world::{TraitFlags, World};

/// The gap of an auto trait of an async block's future that failed on the superset interior
/// (the module doc, 2).
pub const INTERIOR_GAP: &str = "an auto-trait obligation on an async future whose exact capture and suspension witness is unavailable";
pub const EXTERNAL_WITNESS_GAP: &str = "the auto trait of another fn's future needs its witness record";

/// The most type nodes read to find a future inside a type (rule 18).
const MOST_TYPE_NODES: usize = 65_536;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Witness {
    Fixed,
    /// A proven initial capture independently fails the goal.
    FailedCapture,
    LocalFlow,
    External,
    /// Another fn's future whose record holds only its check's superset interior
    /// ([`crate::InteriorIs::Superset`]: its defining body's flow was refused), reached
    /// directly or through a revealed opaque. A goal that failed (or was left open) on it is
    /// the gap [`EXTERNAL_WITNESS_GAP`], never E0277: what the state stores may meet it.
    /// It holds the owner and ordinal of the FIRST such record reached, which the gap names
    /// (`CheckError::ForeignWitness`, rule 19).
    ForeignSuperset(ItemKey, u32),
}

/// Where [`superset_record`] starts: a type of a world record's table (a revealed opaque's
/// hidden type), or one coroutine record by its owner and ordinal.
#[derive(Clone, Copy)]
pub(crate) enum SupersetFrom<'w> {
    Type(crate::portable::PTy, crate::portable::PTys<'w>),
    /// The concrete body records the solver imports to construct this hidden term.
    /// A nested opaque stays opaque and supplies its own facts when selection needs it.
    Import(crate::world::Hidden<'w>),
    Record(ItemKey, u32),
}

/// The first coroutine record reached from `from` whose interior is a superset
/// ([`crate::InteriorIs::Superset`]), with its owner and ordinal: through a revealed opaque's
/// hidden type, an exact foreign coroutine's own interior (an `async fn` that awaits another
/// stores that one's future), and every type's children, generic arguments included (an
/// argument may stand for a stored `Param`). `None`: every coroutine reached is exact, or this
/// reader sees no hidden type behind the opaques reached. Bounded by [`MOST_TYPE_NODES`]
/// (rule 18); each distinct node is walked once. Every type walked is of a world
/// record's table, so a bare block id (`PNode::Future`) is malformed there.
pub(crate) fn superset_record<'w, W: World>(
    reads: &mut crate::world::Reads<'w, '_, W>,
    from: SupersetFrom<'w>,
    alloc: Alloc<'_>,
) -> Result<Option<(ItemKey, u32)>, Stop> {
    use crate::portable::{PNode, PTy, PTys};
    let mut work: Vec<(PTy, PTys<'w>), Alloc<'_>> = Vec::new_in(alloc);
    let mut kids: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    let mut seen = blaze_hash::HashSet::new_in(alloc);
    let importing = matches!(from, SupersetFrom::Import(_));
    match from {
        SupersetFrom::Type(t, types) => work.push((t, types)),
        SupersetFrom::Import(h) => work.push((h.ty, h.types)),
        SupersetFrom::Record(owner, ordinal) => {
            if let Some(f) = reads.coroutine_record(owner, ordinal)? {
                if f.is == crate::world::InteriorIs::Superset {
                    return Ok(Some((owner, ordinal)));
                }
                work.push((f.interior, f.types));
            }
        }
    }
    while let Some((t, table)) = work.pop() {
        if !seen.insert((table.identity(), t)) {
            continue;
        }
        if seen.len() > MOST_TYPE_NODES {
            return Err(CheckError::Limit(Limit::Steps).into());
        }
        let node = table.node(t)?;
        match node {
            PNode::Opaque(k, _) if !importing => {
                if let Some(h) = reads.opaque_record(k)? {
                    work.push((h.ty, h.types));
                }
            }
            PNode::Coroutine { owner, ordinal, .. } => {
                if let Some(f) = reads.coroutine_record(owner, ordinal)? {
                    if f.is == crate::world::InteriorIs::Superset {
                        return Ok(Some((owner, ordinal)));
                    }
                    work.push((f.interior, f.types));
                    if importing {
                        work.push((f.output, f.types));
                    }
                }
            }
            PNode::Closure { owner, ordinal, .. } => {
                if let Some((f, env)) = reads.closure(owner, ordinal)? {
                    if let Some(env) = env {
                        work.push((env, f.types));
                    }
                    if importing {
                        work.push((f.sig, f.types));
                    }
                }
            }
            // A record's nested future is its owner's coroutine by identity (front2
            // opaque_auto.rs `copy_reached`); a bare block id outside its body names nothing.
            PNode::Future(block, ..) if !importing => {
                return Err(CheckError::Malformed(crate::error::Malformed::FutureOutsideOwner { block, reader: None }).into());
            }
            PNode::FnDef(..) | PNode::FnPtr(..) | PNode::Dynamic(_) if !importing => continue,
            _ => {}
        }
        kids.clear();
        table.children_of(node, &mut kids)?;
        for &c in kids.iter() {
            work.push((c, table));
        }
    }
    Ok(None)
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// A direct auto-trait goal can stay ambiguous after the opaque's definition was
    /// supplied. For a returned future, the type that still needs revealing can be the
    /// future of another async fn stored while an await is pending. Follow the actual
    /// instantiated hidden type and the structural auto-trait constituents. Only an
    /// opaque whose own obligation is ambiguous may ask for another defining body.
    pub(crate) fn remaining_opaque_witness(&mut self, gap: CheckError, obligation: Obligation, at: At) -> Result<CheckError, Stop> {
        let CheckError::OpaqueHiddenUnknown { opaque, trait_ } = gap else { return Ok(gap) };
        let Some(hidden) = self.reads.opaque_record(opaque)? else { return Ok(gap) };
        let at = if matches!(at, At::Body) { At::Expr(self.body.root) } else { at };
        let range = self.range_of(at);
        if let Some((item, ordinal)) = superset_record(&mut self.reads, SupersetFrom::Import(hidden), self.alloc)? {
            return Ok(CheckError::ForeignWitness { gap: EXTERNAL_WITNESS_GAP, item, ordinal: Some(ordinal), at: range });
        }
        let Goal::Trait { args, .. } = obligation.goal else { return Ok(gap) };
        let root = self.ty.list_get(args, 0);
        let TyView::Opaque(key, subst) = self.view(root) else { return Ok(gap) };
        if key != opaque {
            return Ok(gap);
        }
        let send = self.lang(LangItem::Send)?;
        let sync = self.lang(LangItem::Sync)?;
        if Some(trait_) != send && Some(trait_) != sync {
            return Ok(CheckError::OpaqueAutoUndecided { opaque, trait_, at: range, constituent: None });
        }
        self.probe(|m| {
            let first = m.lower(hidden.types, hidden.ty, Lower::Args(subst), obligation.cause)?;
            let mut work = Vec::new_in(m.alloc);
            let mut seen = blaze_hash::HashSet::new_in(m.alloc);
            let mut unresolved = None;
            work.push((first, trait_));
            while let Some((ty, required)) = work.pop() {
                let ty = m.ty.shallow_resolve(ty);
                if !seen.insert((ty, required)) {
                    continue;
                }
                if seen.len() > MOST_TYPE_NODES {
                    return Err(CheckError::Limit(Limit::Steps).into());
                }
                match m.view(ty) {
                    TyView::Opaque(child, args) => {
                        if !m.ambiguous_constituent(ty, required, obligation.cause)? {
                            continue;
                        }
                        let Some(h) = m.reads.opaque_record(child)? else {
                            return Ok(CheckError::OpaqueHiddenUnknown { opaque: child, trait_: required });
                        };
                        if let Some((item, ordinal)) = superset_record(&mut m.reads, SupersetFrom::Import(h), m.alloc)? {
                            return Ok(CheckError::ForeignWitness { gap: EXTERNAL_WITNESS_GAP, item, ordinal: Some(ordinal), at: range });
                        }
                        work.push((m.lower(h.types, h.ty, Lower::Args(args), obligation.cause)?, required));
                    }
                    TyView::Future(_, _, interior) | TyView::CoroutineOf { interior, .. } => work.push((interior, required)),
                    view @ (TyView::Closure(..) | TyView::ClosureOf { .. }) => {
                        if let Some((_, Some(env))) = m.closure_witness(view) {
                            work.push((env, required));
                        }
                    }
                    TyView::Tuple(fields) => {
                        for i in (0..m.ty.list_len(fields)).rev() {
                            work.push((m.ty.list_get(fields, i), required));
                        }
                    }
                    TyView::Slice(element) | TyView::Array(element, _) => work.push((element, required)),
                    TyView::Ref(Mutbl::Not, target) if send == Some(required) => {
                        let sync = sync.ok_or(CheckError::Malformed(crate::error::Malformed::NoRecord(crate::world::Question::Lang(LangItem::Sync))))?;
                        work.push((target, sync));
                    }
                    TyView::Ref(_, target) => work.push((target, required)),
                    TyView::Adt(item, _) | TyView::Alias { trait_: item, .. } => {
                        if unresolved.is_none() && m.ambiguous_constituent(ty, required, obligation.cause)? {
                            unresolved = Some((item, required));
                        }
                    }
                    _ => {}
                }
            }
            Ok(CheckError::OpaqueAutoUndecided { opaque, trait_, at: range, constituent: unresolved })
        })
    }


    /// Probe one stored value, even when the caller is partway through reporting a different
    /// list of errors. Rolling back restores both that list and its reader's position.
    fn ambiguous_constituent(&mut self, ty: Ty, trait_: ItemKey, cause: Cause) -> Result<bool, Stop> {
        let args = self.mk_list(&[ty])?;
        let obligation = Obligation { goal: Goal::Trait { trait_, args }, cause };
        crate::stalled::ambiguous_interior_goal(self.ty, &mut self.reads, obligation)
    }

    /// Whether the closure or async block `e` is written `move`: every place it names is then
    /// captured by value.
    pub(crate) fn captures_by_move(&self, e: ExprId) -> Result<bool, Stop> {
        Ok(match self.body.expr(e)? {
            Expr::Closure { is_move, .. } => is_move,
            Expr::AsyncBlock { capture, .. } => capture == Capture::Move,
            _ => false,
        })
    }

    /// Record what the future holds before its first poll. Capture analysis has
    /// already combined the enclosing and nested owners' uses.
    pub(crate) fn analyze_coroutine_captures(&mut self) -> Result<(), Stop> {
        let mut held = Vec::new_in(self.alloc);
        for i in 0..self.futures.len() {
            let f = self.futures[i];
            held.clear();
            if let Expr::Closure { params, .. } = self.body.expr(f.expr)? {
                // Every argument is moved into the call future, even if unused.
                for &p in self.body.pat_list(params)? {
                    held.push(self.pat_ty[p.ix()]);
                }
            }
            if let Some(env) = f.initial {
                // The captured places' environment (capture_places.rs), filed by
                // rules/closure.rs `analyze_closures` where the places were decided.
                let TyView::Tuple(env) = self.view(env) else {
                    return Err(CheckError::Malformed(crate::error::Malformed::Expr(f.expr)).into());
                };
                for k in 0..self.ty.list_len(env) {
                    held.push(self.ty.list_get(env, k));
                }
            } else if f.witness != crate::CoroutineWitness::NeedsCapturePlaces {
                for row in self.capture_rows(f.expr) {
                    held.push(self.stored_capture(row)?);
                }
            }
            if f.witness == crate::CoroutineWitness::Complete || !held.is_empty() {
                let initial = self.witness_tuple(&mut held)?;
                self.futures[i].initial = Some(initial);
                if f.witness == crate::CoroutineWitness::Complete {
                    self.demand_eq(At::Expr(f.expr), f.interior, initial)?;
                }
            }
            if f.witness != crate::CoroutineWitness::Complete {
                // analyze_futures writes the types it found here. Keep the solver's
                // variable open until the precision of that tuple has been established.
                self.pending_witnesses.push((i, f.interior));
                self.futures[i].interior = self.fresh()?;
                // Discarding the result of a sole await adds no suspended state.
                // Let the ordinary forwarding proof inspect its operand in either form.
                if let Expr::Block { stmts, tail: None, .. } = self.body.expr(f.body)?
                    && let &[stmt] = self.body.stmt_list(stmts)?
                    && let crate::body::Stmt::Expr { expr, .. } = self.body.stmt(stmt)?
                    && matches!(self.body.expr(expr)?, Expr::Await(_))
                {
                    self.futures[i].body = expr;
                }
            }
        }
        Ok(())
    }

    /// A by-value use in an async block consumes a place unless it is Copy.
    pub(crate) fn block_moved_out(&mut self, x: ExprId) -> Result<bool, Stop> {
        if self.lang(LangItem::Copy)?.is_none() {
            return Ok(true);
        }
        self.moved_out(x)
    }

    /// Whether the binding `p`'s type may be `Copy`. With no `Copy` lang item nothing is
    /// known `Copy`: a use by value moves (rules/closure.rs `moved_out` decides the same).
    pub(super) fn local_may_be_copy(&mut self, p: PatId) -> Result<bool, Stop> {
        let Some(copy) = self.lang(LangItem::Copy)? else { return Ok(false) };
        let t = self.ty.resolve(self.local_ty[p.ix()])?;
        let args = self.mk_list(&[t])?;
        Ok(self.ty.may_hold(Goal::Trait { trait_: copy, args }, &mut self.reads)?)
    }

    /// The ownership of a nested environment depends on a decided Copy goal.
    /// An open goal cannot establish a borrow for a value that might move.
    pub(super) fn copied_into_child(&mut self, p: PatId, child: ExprId) -> Result<bool, Stop> {
        let Some(copy) = self.lang(LangItem::Copy)? else { return Ok(false) };
        let t = self.ty.resolve(self.local_ty[p.ix()])?;
        let args = self.mk_list(&[t])?;
        let goal = Goal::Trait { trait_: copy, args };
        if !self.ty.may_hold(goal, &mut self.reads)? {
            return Ok(false);
        }
        if crate::stalled::ambiguous_interior_goal(self.ty, &mut self.reads, Obligation { goal, cause: Cause::Expr(child) })? {
            return Err(CheckError::FutureWitness {
                reason: "the capture mode for a value transferred to a nested closure or async block needs its Copy obligation",
                at: self.body.expr_range(child),
            }.into());
        }
        Ok(true)
    }

    /// For a goal that FAILED: whether it is an auto trait of a type holding an async
    /// block's future and the failure is not known to be the exact interior's (the module doc,
    /// 2).
    pub(crate) fn coroutine_interior_gap(&mut self, goal: Goal) -> Result<Witness, Stop> {
        let Goal::Trait { trait_, args } = goal else { return Ok(Witness::Fixed) };
        if self.ty.list_len(args) == 0 {
            return Ok(Witness::Fixed);
        }
        let auto = match self.reads.trait_def(trait_)? {
            Some(def) => def.flags.0 & TraitFlags::AUTO != 0,
            None => false,
        };
        if !auto {
            return Ok(Witness::Fixed);
        }
        let this = self.ty.resolve(self.ty.list_get(args, 0))?;
        // The solver rejects Unpin for an async coroutine independently of its
        // fields (blaze-solve structural.rs). No liveness information can change it.
        if matches!(self.view(this), TyView::Future(..) | TyView::CoroutineOf { .. })
            && self.lang(LangItem::Unpin)? == Some(trait_)
        {
            return Ok(Witness::Fixed);
        }
        if let TyView::Future(e, _, _) = self.view(this)
            && (self.lang(LangItem::Send)? == Some(trait_) || self.lang(LangItem::Sync)? == Some(trait_))
            && self.initial_state_fails(e, trait_)?
        {
            return Ok(Witness::FailedCapture);
        }
        let witness = self.witness_source(this)?;
        if witness == Witness::External && !self.reaches_future(this)? {
            return Ok(Witness::Fixed);
        }
        Ok(witness)
    }

    /// Whether `t` is, or holds anywhere inside, an async block's future.
    fn reaches_future(&mut self, t: Ty) -> Result<bool, Stop> {
        let mut work: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        work.push(t);
        let mut steps = 0usize;
        while let Some(x) = work.pop() {
            steps += 1;
            if steps > MOST_TYPE_NODES {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            let x = self.ty.shallow_resolve(x);
            let v = self.view(x);
            if matches!(v, TyView::Future(..) | TyView::CoroutineOf { .. }) {
                return Ok(true);
            }
            self.ty_children(v, &mut work);
        }
        Ok(false)
    }

    pub(super) fn closure_witness(&self, view: TyView) -> Option<(Ty, Option<Ty>)> {
        match view {
            TyView::Closure(e, sig) => Some((sig, self.closure_kinds.iter().find(|r| r.0 == e).and_then(|r| r.2))),
            TyView::ClosureOf { sig, upvars, .. } => Some((sig, upvars)),
            _ => None,
        }
    }

    /// Which stored types can change a failed goal when local flow replaces its
    /// provisional tuples, or when another fn's record turns exact: a nominal coroutine (or
    /// an opaque revealed to one) whose record is a superset is [`Witness::ForeignSuperset`].
    /// The strongest wins: `LocalFlow` returns at once, `ForeignSuperset` is never lowered to
    /// `External`, `External` never to `Fixed`.
    pub(super) fn witness_source(&mut self, source: Ty) -> Result<Witness, Stop> {
        self.probe(|m| m.stored_witness_source(source))
    }

    fn stored_witness_source(&mut self, source: Ty) -> Result<Witness, Stop> {
        let mut work = Vec::new_in(self.alloc);
        let mut children = Vec::new_in(self.alloc);
        let mut seen = blaze_hash::HashSet::new_in(self.alloc);
        let mut witness = Witness::Fixed;
        work.push((source, false));
        while let Some((t, may_project)) = work.pop() {
            let t = self.ty.shallow_resolve(t);
            if !seen.insert((t, may_project)) {
                continue;
            }
            if seen.len() > MOST_TYPE_NODES {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            let view = self.view(t);
            if let Some((sig, env)) = self.closure_witness(view) {
                if let Some(env) = env {
                    work.push((env, false));
                }
                // Buffered<Map<_, F>> also stores F::Output. Replaying the
                // entire failed ADT goal determines whether this future mattered.
                if may_project && let TyView::FnPtr(io, _) = self.view(sig) {
                    let last = self.ty.list_len(io).checked_sub(1).ok_or(CheckError::Malformed(
                        crate::error::Malformed::Unreadable { node: sig.0, what: crate::error::Unreadable::ClosureArgs },
                    ))?;
                    work.push((self.ty.list_get(io, last), false));
                }
                continue;
            }
            match view {
                TyView::FnDef(..) | TyView::FnPtr(..) | TyView::Dynamic(_) => continue,
                TyView::Opaque(key, args) => {
                    // Inspect the producer's precision even when selection could only
                    // use the opaque's declared bounds.
                    match self.reads.opaque_record(key)? {
                        Some(hidden) => {
                            if let Some((owner, ordinal)) = superset_record(&mut self.reads, SupersetFrom::Type(hidden.ty, hidden.types), self.alloc)? {
                                if !matches!(witness, Witness::ForeignSuperset(..)) {
                                    witness = Witness::ForeignSuperset(owner, ordinal);
                                }
                            } else {
                                let ty = self.lower(hidden.types, hidden.ty, Lower::Args(args), Cause::Body)?;
                                work.push((ty, may_project));
                            }
                        }
                        // Not revealed: an outside witness, unless a superset gap was found already.
                        None => {
                            if witness == Witness::Fixed {
                                witness = Witness::External;
                            }
                        }
                    }
                    continue;
                }
                TyView::CoroutineOf { owner, ordinal, interior, .. } => {
                    // A nominal coroutine imported its producer's record: exact, or the
                    // superset of a producer whose flow was refused (`InteriorIs`).
                    match self.reads.coroutine_record(owner, ordinal)? {
                        Some(f) if f.is == crate::world::InteriorIs::Superset => {
                            if !matches!(witness, Witness::ForeignSuperset(..)) {
                                witness = Witness::ForeignSuperset(owner, ordinal);
                            }
                        }
                        _ => work.push((interior, false)),
                    }
                    continue;
                }
                TyView::Future(e, _, interior) => {
                    match self.futures.iter().find(|f| f.expr == e) {
                        Some(f) if f.witness != crate::CoroutineWitness::Complete => return Ok(Witness::LocalFlow),
                        Some(_) => work.push((interior, false)),
                        None if witness == Witness::Fixed => witness = Witness::External,
                        None => {}
                    }
                    // Only stored values determine the coroutine's auto traits.
                    continue;
                }
                _ => {}
            }
            children.clear();
            self.ty_children(view, &mut children);
            let may_project = may_project || matches!(view, TyView::Adt(..) | TyView::Alias { .. });
            work.extend(children.iter().map(|&child| (child, may_project)));
        }
        Ok(witness)
    }
}
