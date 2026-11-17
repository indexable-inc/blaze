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
//!    One case the closure walk does not see is decided here: a block that is not `move`
//!    holding a `move` closure or an `async move` block that names an outer variable. The inner
//!    one uses that variable by value, so the outer block captures it by value unless its type
//!    is `Copy` (the Reference, "Closure types", "Capture modes": a place used by value is
//!    captured "by move" unless it is `Copy`, when a shared borrow does).
//!
//! 2. AN AUTO TRAIT THAT FAILED ON THE SUPERSET. rules/coroutine.rs fixes a future's interior
//!    as a superset of the Reference's (every type the body names; liveness across an `.await`
//!    is not computed). A goal that HOLDS on the superset holds on the exact interior. A goal
//!    that FAILS on it is known to fail only when a value the future always holds fails it: a
//!    capture by value of an `async move` block, which is part of the state whatever is live.
//!    Any other failure is not an answer: it is the typed gap [`INTERIOR_GAP`], never E0277
//!    (the Reference accepts `async { let r = Rc::new(1); drop(r); f().await }` as `Send`).
//!    Needed to close the gap: the locals and temporaries whose storage is live at some
//!    `.await` (a local from its binding to its scope's end less what was moved out, a
//!    temporary to its statement's end), which is the flow layer's liveness over the body.

use blaze_own::{Alloc, Vec};

use super::closure::USE_MOVE;
use crate::body::{Capture, Expr};
use crate::columns::CaptureKind;
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ExprId, ItemKey, LangItem, PatId};
use crate::machine::Machine;
use crate::ty::{Goal, Ty, TyView, TypeCtx};
use crate::world::{TraitFlags, World};

/// The gap of an auto trait of an async block's future that failed on the superset interior
/// (the module doc, 2).
pub const INTERIOR_GAP: &str = "an auto trait (`Send`, `Sync`) of an async block's future that fails only on a value not known to be live across an `.await` (liveness across `.await` is not computed)";

/// The most type nodes read to find a future inside a type (rule 18). A type larger than this
/// is answered as holding one: the gap, never a wrong "no".
const MOST_TYPE_NODES: usize = 65_536;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// Whether the closure or async block `e` is written `move`: every place it names is then
    /// captured by value.
    pub(crate) fn captures_by_move(&self, e: ExprId) -> Result<bool, Stop> {
        Ok(match self.body.expr(e)? {
            Expr::Closure { is_move, .. } => is_move,
            Expr::AsyncBlock { capture, .. } => capture == Capture::Move,
            _ => false,
        })
    }

    /// The captures of every async block of the body, as rows of `captures` (the module doc,
    /// 1). After `analyze_closures`, whose rows of the closures it holds stay as they are.
    pub(crate) fn analyze_coroutine_captures(&mut self) -> Result<(), Stop> {
        if self.futures.is_empty() {
            return Ok(());
        }
        let mut bound: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(self.body.pats.len(), self.alloc);
        let mut exprs: Vec<(ExprId, u8), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut pats: Vec<PatId, Alloc<'a>> = Vec::new_in(self.alloc);
        // Each (block that is not `move`, `move` closure or `async move` block inside it).
        let mut nested: Vec<(ExprId, ExprId), Alloc<'a>> = Vec::new_in(self.alloc);
        let bound_steps = self.body.exprs.len() + self.body.stmts.len() + 1;
        // The walk also files each use's `Fn*` kind, which is a closure's alone.
        let uses_before = self.capture_uses.len();
        for fi in 0..self.futures.len() {
            let block = self.futures[fi].expr;
            // An async closure's future is its closure's: `analyze_closures` found its rows.
            let Expr::AsyncBlock { body, capture } = self.body.expr(block)? else { continue };
            bound.clear();
            bound.resize(self.body.pats.len(), false);
            exprs.clear();
            // The block's value is its body's: taken by value, as a closure's result is.
            exprs.push((body, USE_MOVE));
            let mut steps = 0usize;
            while let Some((x, usage)) = exprs.pop() {
                steps += 1;
                if steps > bound_steps {
                    return Err(CheckError::Limit(Limit::Steps).into());
                }
                if capture == Capture::Ref && self.captures_by_move(x)? {
                    nested.push((block, x));
                }
                self.visit_for_captures(block, x, usage, &mut exprs, &mut pats, &mut bound)?;
            }
        }
        self.capture_uses.truncate(uses_before);
        // One row per (block, local), the strongest use kept (rule 25: sorted), as
        // `analyze_closures` leaves the closures' rows.
        self.captures.sort_unstable_by_key(|&(c, p, k)| (c, p, core::cmp::Reverse(k as u8)));
        self.captures.dedup_by_key(|&mut (c, p, _)| (c, p));
        // What an inner `move` holds by value, the outer block holds by value too unless it
        // is `Copy`. An inner block's rows are whole by now, whatever the order met.
        for ni in 0..nested.len() {
            let (outer, inner) = nested[ni];
            for i in 0..self.captures.len() {
                let (c, p, _) = self.captures[i];
                if c != inner {
                    continue;
                }
                let Some(at) = self.captures.iter().position(|&(oc, op, ok)| oc == outer && op == p && ok != CaptureKind::ByValue) else {
                    continue;
                };
                if !self.local_may_be_copy(p)? {
                    self.captures[at].2 = CaptureKind::ByValue;
                }
            }
        }
        Ok(())
    }

    /// Whether a use by value at `x`, in an async block that is not `move`, moves the local it
    /// names (rules/closure.rs `moved_out`'s rule). In a world with no `Copy` lang item no
    /// type is `Copy`, so the use moves (the Reference's case for a place that is not `Copy`),
    /// and no finding is made: a closure's walk names the missing item, a block's does not
    /// turn a clean body into a finding (an `async fn`'s root block is in every async body).
    pub(crate) fn block_moved_out(&mut self, x: ExprId) -> Result<bool, Stop> {
        if self.lang(LangItem::Copy)?.is_none() {
            return Ok(true);
        }
        self.moved_out(x)
    }

    /// Whether the binding `p`'s type may be `Copy`. With no `Copy` lang item nothing is
    /// known `Copy`: a use by value moves (rules/closure.rs `moved_out` decides the same).
    fn local_may_be_copy(&mut self, p: PatId) -> Result<bool, Stop> {
        let Some(copy) = self.lang(LangItem::Copy)? else { return Ok(false) };
        let t = self.ty.resolve(self.local_ty[p.ix()])?;
        let args = self.mk_list(&[t])?;
        Ok(self.ty.may_hold(Goal::Trait { trait_: copy, args }, &mut self.reads)?)
    }

    /// For a goal that FAILED: `Some` gap when it is an auto trait of a type holding an async
    /// block's future and the failure is not known to be the exact interior's (the module doc,
    /// 2); `None` when the failure stands as E0277.
    pub(crate) fn coroutine_interior_gap(&mut self, goal: Goal) -> Result<Option<CheckError>, Stop> {
        let Goal::Trait { trait_, args } = goal else { return Ok(None) };
        if self.ty.list_len(args) == 0 {
            return Ok(None);
        }
        let auto = match self.reads.trait_def(trait_)? {
            Some(def) => def.flags.0 & TraitFlags::AUTO != 0,
            None => false,
        };
        if !auto {
            return Ok(None);
        }
        let this = self.ty.resolve(self.ty.list_get(args, 0))?;
        if !self.reaches_future(this)? {
            return Ok(None);
        }
        if let TyView::Future(block, _, _) = self.view(this)
            && self.moved_capture_fails(block, trait_)?
        {
            return Ok(None);
        }
        Ok(Some(CheckError::Unported(INTERIOR_GAP)))
    }

    /// Whether `t` is, or holds anywhere inside, an async block's future.
    fn reaches_future(&mut self, t: Ty) -> Result<bool, Stop> {
        let mut work: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        work.push(t);
        let mut steps = 0usize;
        while let Some(x) = work.pop() {
            steps += 1;
            if steps > MOST_TYPE_NODES {
                return Ok(true);
            }
            let x = self.ty.shallow_resolve(x);
            let v = self.view(x);
            if matches!(v, TyView::Future(..)) {
                return Ok(true);
            }
            self.ty_children(v, &mut work);
        }
        Ok(false)
    }

    /// Whether `block`, an `async move` block of this body, captures by value a variable whose
    /// type does not implement `trait_` and holds no future itself: a value the state always
    /// holds, so the future does not implement the auto trait whatever is live.
    pub(super) fn moved_capture_fails(&mut self, block: ExprId, trait_: ItemKey) -> Result<bool, Stop> {
        // A future of another body (a hidden type's) has no rows here.
        if !self.futures.iter().any(|f| f.expr == block) {
            return Ok(false);
        }
        let Expr::AsyncBlock { capture: Capture::Move, .. } = self.body.expr(block)? else { return Ok(false) };
        for i in 0..self.captures.len() {
            let (c, p, kind) = self.captures[i];
            if c != block || kind != CaptureKind::ByValue {
                continue;
            }
            let t = self.ty.resolve(self.local_ty[p.ix()])?;
            if self.reaches_future(t)? {
                continue;
            }
            let args = self.mk_list(&[t])?;
            if !self.ty.may_hold(Goal::Trait { trait_, args }, &mut self.reads)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
