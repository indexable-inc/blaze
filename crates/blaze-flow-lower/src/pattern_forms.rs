//! Three pattern forms the binding walk (`Lower::bind_walk`, lib.rs) lowers soundly.
//!
//! # Match guards
//!
//! The Reference, "Match expressions", "Match guards" ([expr.match.guard.value]): "Moreover, by
//! holding a shared reference while evaluating the guard, mutation inside guards is also
//! prevented. [..] a guard expression that tries to move out of [a by-value binding] is an
//! error." rustc's lowering (MIR building, `bind_matched_candidate_for_guard`): before the guard
//! each by-value binding `x` is bound as `ref x` (a shared borrow of its place), the guard reads
//! `*ref_x` wherever it names `x`, and only after the guard holds is `x` bound by value (moved
//! or copied) for the arm's body. The scrutinee is also held by fake borrows over the guard
//! (`Task::FakeBorrow`), so the guard cannot write it.
//!
//! So a guarded arm binding a non-`Copy` value by value lowers as:
//! - [`BindPass::GuardRefs`] before the guard: each by-reference binding is made as usual; each
//!   by-value one gets a temporary `&T` borrowing its place, and the guard's uses of the binding
//!   resolve to `*tmp` (through `upvar_places`, which `Expr::Local` consults first). A move in
//!   the guard is then a move out of a deref: E0507 (`MoveOut::Deref`), as rustc reports.
//! - [`BindPass::GuardMoves`] once the guard held: the redirection is dropped and each by-value
//!   binding is moved or copied from its place, the body's uses naming that local.
//!
//! The control `Plant::GuardBindsByValue` binds by value before the guard (as an arm without a
//! guard is): a guard that moves its binding is then accepted (pattern_forms_tests.rs).
//!
//! # Or-patterns
//!
//! The Reference, "Patterns", "Or-patterns": every alternative binds the same names, at the
//! same types and binding modes, and a name is ONE variable whichever alternative matched. So
//! the alternatives (when any binds) are reached through a switch on the place, each binds in
//! its own block into the locals of its names' FIRST bindings (pre-order, as the code walk's
//! `first_binds`: blaze-build walk/matches/forms.rs), and all meet at one join block where the
//! rest of the pattern goes on. A use of the name after the pattern reads that one local; a
//! move in any alternative's path moves it. In a guarded arm that moves a binding the
//! alternatives are refused by name (`Form::OrPatternBinding`): the guard would run once per
//! alternative (blaze-build walk/matches/alts.rs), which this lowering does not model.
//!
//! # Slice patterns
//!
//! The Reference, "Slice patterns": against `[T; N]` or `[T]`, each element before the `..` is
//! the element at its index from the start, each after it the element at its index from the
//! end, and `rest @ ..` binds the subslice between. Projected as rustc's MIR does
//! (`ProjectionElem::ConstantIndex { offset, min_length, from_end }` and
//! `ProjectionElem::Subslice { from, to, from_end }`), so distinct elements are disjoint places
//! (blaze-flow loans.rs `overlap`, rustc's `place_projection_conflict`): `[a, b]` of a
//! `&mut [T; 2]` holds two loans that do not conflict, while a borrow of the whole slice
//! overlaps each element (a prefix). An element of an unsized `[T]` is projected marked as one
//! (`PlaceMark::SliceElement`, its own row fact `PlaceRow::of_slice`, never the `Drop` field
//! mark), so a move out of it is refused (E0508; blaze-flow moves.rs `move_out`).

use blaze_flow::{Block, BorrowKind, Local, Place, Proj, StmtKind};
use blaze_own::Vec;
use blaze_typeck::{ExprId, List, Mutbl, Name, PNode, Pat, PatId};
use core::alloc::Allocator;

use crate::identity::Identity;
use crate::{Facts, Form, Lower, Plant, Refusal, TyD};

/// One pass of the binding walk over a pattern.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BindPass {
    /// Every binding, by its mode: a `let`, a parameter, an arm with no moving guard.
    Whole,
    /// A guarded arm before its guard `ExprId`: by-reference bindings made, each by-value one a
    /// shared reference to its place that the guard reads through.
    GuardRefs(ExprId),
    /// The same arm after its guard held: only the by-value bindings, moved or copied.
    GuardMoves,
}

/// An item of the binding walk's work list.
#[derive(Clone, Copy)]
pub(crate) enum BindItem {
    /// A pattern against a place of a type, under a default binding mode (0 move, 1 `ref`,
    /// 2 `ref mut`).
    Pat(PatId, Place, TyD, u8),
    /// Start an or-pattern alternative's block, or the alternatives' join.
    Start(Block),
    /// An alternative's end: go to the join (the span is the or-pattern's).
    Goto(Block, PatId),
}

impl<F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'_, '_, F, A, R> {
    /// Each name `root` binds with its first binding pattern in source order (pre-order, left to
    /// right): the variable every or-alternative's binding of that name binds.
    pub(crate) fn first_binds(&self, root: PatId, out: &mut Vec<(Name, PatId), A>) -> Result<(), Refusal> {
        let mut todo: Vec<PatId, A> = Vec::new_in(self.alloc);
        let mut subs: Vec<PatId, A> = Vec::new_in(self.alloc);
        todo.push(root);
        let mut steps = 0usize;
        while let Some(q) = todo.pop() {
            steps += 1;
            if steps > self.body.pats.len() + 1 {
                return self.refuse_pat(Form::Bound, root);
            }
            let pat = match self.body.pats.get(q.ix()) {
                Some(x) => *x,
                None => return self.refuse_pat(Form::Malformed, q),
            };
            if let Pat::Bind { name, .. } = pat
                && !out.iter().any(|&(n, _)| n == name)
            {
                out.push((name, q));
            }
            subs.clear();
            self.sub_pats(pat, &mut subs)?;
            // Last first, so they pop in source order.
            for &s in subs.iter().rev() {
                todo.push(s);
            }
        }
        Ok(())
    }

    /// The local binding `p` (named `name`, of type `lt`) binds: its name's first binding's
    /// when an earlier or-alternative made it ([`Self::first_binds`]), else a new one.
    pub(crate) fn binding_local(&mut self, p: PatId, name: Name, lt: TyD, alias: &[(Name, PatId)]) -> Result<Local, Refusal> {
        let first = alias.iter().find(|&&(n, _)| n == name).map_or(p, |&(_, f)| f);
        if first != p
            && let Some(&l) = self.pat_local.get(first.ix())
            && l != u32::MAX
        {
            // The alternatives agree on the type (rules/pat.rs checks each against the scrutinee
            // and unifies a name's bindings); a disagreement here is a malformed table.
            if self.local_ty.get(l as usize).map(|t| t.ty) != Some(lt.ty) {
                return self.refuse_pat(Form::OrPatternBinding, p);
            }
            self.pat_local[p.ix()] = l;
            return Ok(Local(l));
        }
        let root = self.body.root;
        let l = self.new_local_with(lt, false, None, root)?;
        self.name_local(l, Identity::Binding(p));
        self.pat_local[p.ix()] = l.0;
        self.vars.push(l);
        Ok(l)
    }

    /// Before guard `g`: the by-value binding `p` (of type `lt`, its place `place` of type `ty`)
    /// as a temporary `&lt` borrowing the place, and the guard's uses of `p` as `*tmp`.
    pub(crate) fn guard_ref_binding(&mut self, p: PatId, place: Place, ty: TyD, lt: TyD, g: ExprId) -> Result<(), Refusal> {
        let root = self.body.root;
        if ty.wrap.is_some() || ty.ty != lt.ty {
            return self.refuse_pat(Form::BindingMode, p);
        }
        let rt = TyD { wrap: Some(Mutbl::Not), ty: lt.ty };
        let r = self.new_local_with(rt, false, None, root)?;
        // Named as the binding: the reference is dead before the by-value binding is made
        // (`GuardMoves`), so no suspension holds both under one identity (`Form::IdentityTwice`).
        self.name_local(r, Identity::Binding(p));
        // Live for the guard: its span is the guard's.
        self.b.stmt(StmtKind::StorageLive(r), self.span(g));
        self.vars.push(r);
        self.borrow(place, ty, BorrowKind::Shared, r, root)?;
        let rp = self.place_of(r);
        let star = self.project(rp, rt, Proj::Deref, lt, false, root)?;
        self.upvar_places.retain(|u| u.0 != p);
        self.upvar_places.push((p, star, lt));
        Ok(())
    }

    /// An or-pattern `p` with alternatives `alts` against `place`: when any alternative binds, a
    /// switch on the place to one block per alternative, each binding it and going to a join
    /// where the walk goes on (the module doc).
    #[expect(clippy::too_many_arguments, reason = "one pattern's frame")]
    pub(crate) fn or_alternatives(
        &mut self,
        p: PatId,
        alts: List,
        place: Place,
        ty: TyD,
        mode: u8,
        pass: BindPass,
        stack: &mut Vec<BindItem, A>,
    ) -> Result<(), Refusal> {
        let body = self.body;
        let list = body.pat_lists.get(alts.range()).unwrap_or(&[]);
        let mut any = false;
        for &a in list {
            if self.binds(a)? {
                any = true;
                break;
            }
        }
        if !any {
            return Ok(());
        }
        if pass != BindPass::Whole {
            return self.refuse_pat(Form::OrPatternBinding, p);
        }
        let mut blocks: Vec<Block, A> = Vec::new_in(self.alloc);
        for _ in list {
            blocks.push(self.b.block());
        }
        let join = self.b.block();
        self.b.switch(place, &blocks, self.pat_span(p));
        // The work list pops last first: alternative 1's start, its pattern, its goto, then
        // alternative 2's, .., then the join.
        stack.push(BindItem::Start(join));
        for (i, &a) in list.iter().enumerate().rev() {
            stack.push(BindItem::Goto(join, p));
            stack.push(BindItem::Pat(a, place, ty, mode));
            stack.push(BindItem::Start(blocks[i]));
        }
        Ok(())
    }

    /// A slice pattern `p` (elements `elems`, the `..` at index `rest`) against `place` of the
    /// array or slice type `ty`: each binding element its own constant projection (the module
    /// doc).
    #[expect(clippy::too_many_arguments, reason = "one pattern's frame")]
    pub(crate) fn slice_elements(
        &mut self,
        p: PatId,
        elems: List,
        rest: Option<u32>,
        place: Place,
        ty: TyD,
        mode: u8,
        stack: &mut Vec<BindItem, A>,
    ) -> Result<(), Refusal> {
        let root = self.body.root;
        let (elem, unsized_) = match (ty.wrap, self.node(ty.ty, root)?) {
            (None, PNode::Array(t, _)) => (t, false),
            (None, PNode::Slice(t)) => (t, true),
            _ => return self.refuse_pat(Form::Malformed, p),
        };
        let body = self.body;
        let es = body.pat_lists.get(elems.range()).unwrap_or(&[]);
        let Ok(n) = u32::try_from(es.len()) else {
            return self.refuse_pat(Form::Bound, p);
        };
        let fixed = n - u32::from(rest.is_some());
        if rest.is_some_and(|r| r >= n) {
            return self.refuse_pat(Form::Malformed, p);
        }
        for (j, &sp) in es.iter().enumerate() {
            if !self.binds(sp)? {
                continue;
            }
            let j = j as u32;
            let (proj, t) = match rest {
                Some(r) if j == r => {
                    // `rest @ ..`: its type is the checker's (`[T; N - k]` or `[T]`).
                    let own = match self.ck.pat_ty.get(sp.ix()) {
                        Some(&t) => t,
                        None => return self.refuse_pat(Form::Malformed, sp),
                    };
                    (Proj::Subslice { from: r, to: n - 1 - r, from_end: true }, TyD::of(own))
                }
                // After the `..`: the last element is 1 from the end.
                Some(r) if j > r => (Proj::ConstantIndex { offset: n - j, min_length: fixed, from_end: true }, TyD::of(elem)),
                _ => (Proj::ConstantIndex { offset: j, min_length: fixed, from_end: false }, TyD::of(elem)),
            };
            let proj = if self.plant == Plant::SliceElementsOverlap { Proj::Index } else { proj };
            // An element of an unsized `[T]` is its own fact (blaze-flow `PlaceRow::of_slice`),
            // never the `Drop` field mark.
            let mark = if unsized_ { blaze_flow::PlaceMark::SliceElement } else { blaze_flow::PlaceMark::None };
            let q = self.project_mark(place, ty, proj, t, mark, root)?;
            stack.push(BindItem::Pat(sp, q, t, mode));
        }
        Ok(())
    }
}
