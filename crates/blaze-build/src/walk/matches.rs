//! Matches by tests: the matches walk.rs's one `switch` (`match_dispatch`) does not take, a
//! scrutinee other than an integer, a `bool` or an enum (a reference, a tuple, a struct, a
//! slice), or an arm whose pattern is more than flat (a range, a tuple, a slice pattern, a
//! literal inside a variant, `&p`, `x @ p`, nested or-patterns). Each arm is a chain of tests
//! on the scrutinee's place, one block per subpattern, a failed test jumping to the next arm;
//! then its bindings, from the place again. Rules from the Reference, "Patterns":
//! - "Match expressions": "the scrutinee expression is evaluated and its result compared to the
//!   patterns in sequence"; "the first arm whose pattern matches (and whose guard, if any, is
//!   true) is taken": a failed test or a false guard falls to the next arm.
//! - "Binding modes": "Each time a reference is matched using a non-reference pattern, it will
//!   automatically dereference the value and update the default binding mode. References will
//!   set the default binding mode to `ref`"; "If a binding pattern does not explicitly have
//!   `ref`, `ref mut`, or `mut`, then it uses the default binding mode": the check's rule
//!   (blaze-typeck rules/pat.rs `step_pat`: tuple, struct, slice, path, range and literal
//!   patterns peel every reference), followed here the same way. A `&p` pattern derefs and
//!   binds by move again.
//! - "Range patterns": "An exclusive range pattern matches all values from the lower bound up
//!   to, but not including the upper bound"; "An inclusive range pattern matches all values
//!   from the lower bound up to and including the upper bound"; signed types compare signed.
//! - "Slice patterns": against `[T; N]` the elements are at fixed places; against `[T]` the
//!   length is tested (equal with no `..`, at least the fixed count with one); "the rest
//!   pattern" `x @ ..` binds the subslice between the prefix and the suffix.
//! - "Or-patterns": each alternative is tried in turn, left to right; when the alternatives
//!   bind, the alternative whose tests pass stores its bindings (into each name's first
//!   binding's slot, matches/forms.rs) right there, on its way to the pattern's pass; then
//!   `bind_tested` passes the or-pattern by.
//! - A constant (compared with its evaluated value), a float literal (by `==`) and a string
//!   literal (length and bytes) are matches/forms.rs's.
//!
//! Moves are walk/moves.rs's: a by-value binding of a part that needs drop moves it out of a
//! scrutinee the match owns (a local, by its field flags; a temporary, held to its statement's
//! end), which then drops minus what moved; anywhere else (`if let`, `let`-`else`) refused.

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::func::SlotKind;
use blaze_lir::{Block, Flags, IntCond, Op, Ty, Value};
use blaze_own::Vec;
use blaze_typeck::{Def, ExprId, List, Lit, Mutbl, Name, PNode, PTy, Pat, PatId, UnionPat};

mod alts;
mod forms;
mod irrefutable;
mod ref_bind;

use super::{Home, MatchState, Owner, PTR_BITS, Repr, Shape, Task, V, Walk, malformed, mask, unsupported};
use crate::BuildError;
use crate::layout::Variants;

/// Outcome of testing whether a union value matches a residual sub-union pattern.
enum ResidualTest {
    Never,
    Always,
    Cond(Value),
}

/// A place a pattern is matched against.
#[derive(Clone, Copy)]
pub(super) struct At {
    /// The place; of an unsized type (`[T]`, `str`), the memory of the wide pointer to it.
    at: (Value, i32),
    /// Its ground type.
    g: PTy,
    /// The default binding mode is `ref`: a reference was followed by a non-reference pattern.
    by_ref: bool,
    /// The scrutinee's value copied into the match's own slot: a `ref mut` would write a copy.
    copy: bool,
}

/// A test still to emit: a subpattern, its place, its block, where it goes on a pass and on a
/// failure.
type Test = (PatId, At, Block, Block, Block);

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether the switch takes the match on `scrutinee` (held as `srepr`) with arms `arms`:
    /// an integer, a `bool` or an enum (through references, with variant patterns), and each
    /// arm's pattern flat (a literal, a path, `_`, a plain binding, an or of literals and
    /// paths, a variant of plain bindings and `_`).
    pub(super) fn switch_takes(&mut self, scrutinee: ExprId, arms: List, srepr: Repr) -> Result<bool, BuildError> {
        let list = self.body.arms.get(arms.range()).ok_or(malformed("a match's arms past `arms`"))?;
        let mut variants = false;
        for arm in list {
            if !self.flat(arm.pat)? {
                return Ok(false);
            }
            // A guard runs once per alternative that matches (matches/alts.rs; two
            // alternatives may be one value: `1 | 1 if g`): by tests, one chain each.
            let by_tests = arm.guard.is_some() && matches!(self.pat(arm.pat)?, Pat::Or { .. });
            #[cfg(test)]
            let by_tests = by_tests && self.adts.planted() != crate::Planted::GuardToNextArm;
            if by_tests {
                return Ok(false);
            }
            variants |= matches!(self.pat(arm.pat)?, Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Path(_) | Pat::Or { .. });
        }
        let t = self.adjusted_ty(scrutinee)?;
        let mut g = self.ground_ty(t).map_err(|error| Self::ty_err(scrutinee, error))?;
        Ok(match srepr {
            Repr::Int { .. } | Repr::Bool => true,
            Repr::Float(_) => false,
            Repr::Ptr => {
                let mut depth = 0u32;
                while let PNode::Ref(_, inner) = self.ground_node(g).map_err(|error| Self::ty_err(scrutinee, error))? {
                    g = inner;
                    depth += 1;
                }
                depth > 0 && variants && self.is_agg(g).map_err(|error| Self::ty_err(scrutinee, error))? && self.is_enum(scrutinee, g)?
            }
            Repr::Agg(_) | Repr::None => self.is_enum(scrutinee, g)?,
        })
    }

    /// `g` is an enum (or multi-member tagged union) laid out as one.
    fn is_enum(&mut self, scrutinee: ExprId, g: PTy) -> Result<bool, BuildError> {
        let is_enum_node = match self.ground_node(g).map_err(|error| Self::ty_err(scrutinee, error))? {
            PNode::Adt(..) => true,
            PNode::Union(l) => l.len != 1,
            _ => false,
        };
        if !is_enum_node {
            return Ok(false);
        }
        let l = self.layout(g).map_err(|error| Self::ty_err(scrutinee, error))?;
        Ok(matches!(self.layouts[l as usize].shape, Shape::Enum { .. }))
    }

    /// A pattern the switch takes.
    fn flat(&self, p: PatId) -> Result<bool, BuildError> {
        let plain = |w: &Self, q: PatId| -> Result<bool, BuildError> {
            Ok(match w.pat(q)? {
                Pat::Wild => true,
                Pat::Bind { ann, sub: None, .. } => ann.by_ref.is_none() && w.checked.union_pat_of(q).is_none(),
                _ => false,
            })
        };
        let lit = |l: Lit| matches!(l, Lit::Int(_) | Lit::Char | Lit::Bool(_));
        // A path to a struct or a variant; a constant is compared by value, by tests (the
        // switch's cases are variants' discriminants and literals only).
        let variant = |w: &Self, q: PatId| Self::def_variant(w.pat_def(q)).is_some();
        Ok(match self.pat(p)? {
            Pat::Wild => true,
            Pat::Path(_) => variant(self, p),
            Pat::Bind { ann, sub: None, .. } => ann.by_ref.is_none() && self.checked.union_pat_of(p).is_none(),
            Pat::Lit(l) | Pat::NegLit(l) => lit(l),
            Pat::Or { alts } => {
                let list = self.body.pat_lists.get(alts.range()).ok_or(malformed("an or-pattern past `pat_lists`"))?;
                let mut all = true;
                for &alt in list {
                    all &= match self.pat(alt)? {
                        Pat::Path(_) => variant(self, alt),
                        Pat::Lit(l) | Pat::NegLit(l) => lit(l),
                        _ => false,
                    };
                }
                all
            }
            Pat::TupleStruct { elems, .. } => {
                let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                let mut all = true;
                for &q in subs {
                    all &= plain(self, q)?;
                }
                all
            }
            Pat::Struct { fields, .. } => {
                let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                let mut all = true;
                for fp in fps {
                    all &= plain(self, fp.pat)?;
                }
                all
            }
            _ => false,
        })
    }

    /// Starts match `e` by tests: the scrutinee's place (a scalar copied to a slot), every arm
    /// tested in turn (`arm_tested`).
    pub(super) fn match_tested(&mut self, e: ExprId, scrutinee: ExprId, arms: List, s: V, srepr: Repr) -> Result<(), BuildError> {
        // walk/moves.rs: a scrutinee that needs drop is owned (a local by its field flags, a
        // temporary held to its statement's end), so arms may move parts out of it.
        let m_next = self.matches.len() as u32;
        let at = self.owned_place(m_next, e, scrutinee, s, srepr)?;
        let (at, g, copy) = (at.at, at.g, at.copy);
        let list = self.body.arms.get(arms.range()).ok_or(malformed("a match's arms past `arms`"))?;
        let repr = self.expr_repr(e)?;
        let first = self.arm_blocks.len() as u32;
        for _ in 0..=list.len() {
            let block = self.create();
            self.arm_blocks.push(block);
        }
        let join = self.create();
        let n = list.len() as u32;
        let to = self.arm_blocks[first as usize];
        self.jump(to, &[]);
        let tested = Some(At { at, g, by_ref: false, copy });
        self.matches.push(MatchState {
            expr: e,
            scrutinee: at.0,
            width: 0,
            of_enum: None,
            join,
            repr,
            first,
            arms: n,
            guarded: true,
            by_ref: false,
            owner: Owner::None,
            tested,
        });
        let m = (self.matches.len() - 1) as u32;
        self.work.push(Task::MatchEnd);
        for i in (0..n).rev() {
            self.work.push(Task::Arm { m, i });
        }
        Ok(())
    }

    /// The place match `m` (expression `e`: a `match`, or a `let`-`else` initializer) tests
    /// its scrutinee at, the scrutinee OWNED (walk/moves.rs `own_scrutinee`): a local by its
    /// field flags, a temporary that needs drop held in its temporary scope with a word of its
    /// own (walk/temps.rs `hold_temp`), so the bindings (`move_arm`) may move parts out of it
    /// and the rest drops at that scope's end, once.
    pub(super) fn owned_place(&mut self, m: u32, e: ExprId, scrutinee: ExprId, s: V, srepr: Repr) -> Result<At, BuildError> {
        match self.own_scrutinee(m, e, scrutinee, s, srepr)? {
            Some((p, off, g)) => Ok(At { at: (p, off), g, by_ref: false, copy: false }),
            None => self.tested_place(scrutinee, s, srepr, "a scrutinee temporary that needs drop, not owned (its drop point is not built)"),
        }
    }

    /// The place scrutinee `scrutinee` (its value `s`, held as `srepr`) is tested at: its own
    /// place, or a scalar copied to a slot. A temporary that needs drop is refused with `what`
    /// (a local's place stays its local's, dropped where the local is; a temporary would need
    /// its drop point here). Also walk/lets.rs's, for `if let`, `while let` and `let`-`else`.
    pub(super) fn tested_place(&mut self, scrutinee: ExprId, s: V, srepr: Repr, what: &'static str) -> Result<At, BuildError> {
        let t = self.adjusted_ty(scrutinee)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(scrutinee, error))?;
        if self.needs_drop(g).map_err(|error| Self::ty_err(scrutinee, error))? {
            // A local, or a field of one it owns (walk/moves/field_scrutinee.rs): the local's
            // place, dropped where the local is.
            let local = (self.local_of(scrutinee)?.is_some() && self.adjust_rows(scrutinee).is_empty())
                || self.owned_field_scrutinee(scrutinee)?.is_some();
            // A place behind a pointer is tested where it is and owned by no one here
            // (walk/moves.rs `scrutinee_in_place`; a by-value binding of a part that needs
            // drop is refused by `move_out`).
            if !local && !self.scrutinee_in_place(scrutinee)? {
                return Err(unsupported(scrutinee, what));
            }
        }
        let (at, copy) = match s {
            V::Place(p, off) | V::Unsized(p, off) => ((p, off), false),
            V::Scalar(_) => {
                let (size, align) = self.size_align(srepr);
                let slot = self.b.slot(size, align, SlotKind::Temp);
                let to = (self.b.slot_addr(slot), 0);
                self.store(s, srepr, to)?;
                (to, true)
            }
            V::None if srepr == Repr::None => ((self.b.uninit(Ty::PTR), 0), false),
            V::None => (self.place(s, srepr)?, false),
        };
        Ok(At { at, g, by_ref: false, copy })
    }

    /// Arm `i` of match `m` by tests: its pattern's tests from `block`, a failure to `next`;
    /// its bindings; then its guard and body as walk.rs `arm` pushes them.
    pub(super) fn arm_tested(&mut self, m: u32, i: u32, arm: blaze_typeck::Arm, at: At, block: Block, next: Block) -> Result<(), BuildError> {
        self.begin(block);
        let pass = self.create();
        // matches/alts.rs: a guard runs once per alternative that matches, so a guarded arm
        // with or-patterns is one chain of tests per expansion.
        let expand = arm.guard.is_some() && self.has_or(arm.pat)?;
        #[cfg(test)]
        let expand = expand && self.adts.planted() != crate::Planted::GuardToNextArm;
        // walk/moves.rs: a by-value binding of a part that needs drop moves it out; with a
        // guard, once the guard passed (`Pending`, applied by `Task::ArmMoves`). Set before the
        // tests: an or-pattern's alternative binds (and moves) on its own pass, inside them.
        let from = self.pending_mark();
        self.move_arm(m, arm.pat, arm.guard.is_some());
        let tested = if expand {
            self.test_alts(arm.pat, at, pass, next).map(Some)
        } else {
            self.test_pat(arm.pat, at, pass, next).map(|()| None)
        };
        let expanded = match tested {
            Ok(x) => x,
            Err(error) => {
                self.move_arm_end();
                return Err(error);
            }
        };
        self.begin(pass);
        self.scope_marks.push(self.scope_pats.len() as u32);
        let bound = self.bind_tested(arm.pat, at);
        self.move_arm_end();
        bound?;
        self.work.push(Task::ArmEnd { m });
        self.work.push(Task::ScopeEnd(arm.body));
        // An `if let` guard (walk/lets.rs `let_guard`): its links, each failure to `next`.
        if let Some(g) = arm.guard
            && self.cond_has_let(g)?
        {
            if expanded.is_some() {
                return Err(unsupported(g, "an `if let` guard on an arm whose or-pattern binds (a guard per alternative)"));
            }
            return self.let_guard(g, arm.body, next);
        }
        self.work.push(Task::Eval(arm.body));
        if let Some(g) = arm.guard {
            self.work.push(Task::ArmMoves { from });
            self.work.push(match expanded {
                Some((word, chains, n)) => Task::GuardAlts { word, chains, n },
                None => Task::Guard { m, i },
            });
            self.work.push(Task::Eval(g));
        }
        Ok(())
    }

    /// The tests of pattern `root` at `at`, from the current block: to `ok` when it matches,
    /// else to `fail`. A work list of subpatterns, each in its own block, reached only once
    /// the tests before it passed (a variant's fields after its discriminant, a slice's
    /// elements after its length), so no load reads past what the value holds. The bindings
    /// of an or-pattern whose alternatives bind are stored here, on the matching
    /// alternative's pass; `bind_tested` stores the rest.
    pub(super) fn test_pat(&mut self, root: PatId, at: At, ok: Block, fail: Block) -> Result<(), BuildError> {
        self.test_pat_chosen(root, at, ok, fail, None)
    }

    /// `test_pat`; with `choice` (one expansion of a guarded arm: matches/alts.rs), each
    /// or-pattern tests only the alternative `choice` takes for it, so the chain is the tests
    /// of ONE or-free pattern.
    fn test_pat_chosen(&mut self, root: PatId, at: At, ok: Block, fail: Block, choice: Option<&[alts::Choice]>) -> Result<(), BuildError> {
        let mut todo: Vec<Test, A> = Vec::new_in(self.alloc);
        let mut parts: Vec<(PatId, At), A> = Vec::new_in(self.alloc);
        // Each name's first binding in `root`, filled at the first or-pattern that binds.
        let mut alias: Vec<(Name, PatId), A> = Vec::new_in(self.alloc);
        let entry = self.create();
        self.jump(entry, &[]);
        todo.push((root, at, entry, ok, fail));
        // The work list emits a block before the test that jumps to it, so `begin` cannot
        // read its liveness from an edge yet: each block of the chain is live as its arm is.
        let live = self.live;
        while let Some((p0, at0, entry, ok, fail)) = todo.pop() {
            self.begin(entry);
            self.live = live;
            let (p, mut at) = self.through(p0, at0)?;
            let pat = self.pat(p)?;
            // A path pattern of a reference type (a constant `const TOKEN: &str`) follows no
            // reference, as the check typed it (blaze-typeck rules/pat.rs `step_pat`, the
            // Reference, "Binding modes": "Non-reference patterns include all patterns except
            // bindings, wildcard patterns (`_`), const patterns of reference types, and reference
            // patterns"; rustc_hir_typeck pat.rs `ResolvedPat::adjust_mode`: a constant is
            // `AdjustMode::Pass`, "These constants can be of a reference type"). The scrutinee is
            // matched as it is. Peeled, serde_json 1.0.150's `match name { crate::number::TOKEN
            // => .., _ => .. }` on `name: &str` (value/ser.rs:276, t1800) was compared at `str`
            // itself: "pattern 3: str is not built to LIR yet".
            if Self::peels(pat) && !(matches!(pat, Pat::Path(_)) && self.pat_is_ref(p)?) {
                at = self.peel(p, at)?;
            }
            match pat {
                Pat::Wild => self.jump(ok, &[]),
                Pat::Bind { ann, sub, .. } => match self.checked.union_pat_of(p) {
                    None => self.jump(ok, &[]),
                    Some(UnionPat::Variant(v)) => {
                        let peeled = self.peel(p, at)?;
                        let test = self.union_variant_test(p, peeled, v)?;
                        match (test, sub) {
                            (None, None) => self.jump(ok, &[]),
                            (Some(c), None) => self.br(c, (ok, &[]), (fail, &[])),
                            (None, Some(s)) => {
                                let payload = self.union_variant_at(p, peeled, v)?;
                                let mid = self.create();
                                self.jump(mid, &[]);
                                todo.push((s, payload, mid, ok, fail));
                            }
                            (Some(c), Some(s)) => {
                                let payload = self.union_variant_at(p, peeled, v)?;
                                let mid = self.create();
                                self.br(c, (mid, &[]), (fail, &[]));
                                todo.push((s, payload, mid, ok, fail));
                            }
                        }
                    }
                    Some(UnionPat::Residual) => {
                        let peeled = self.peel(p, at)?;
                        let by_ref = ann.by_ref.is_some() || (!ann.mutable && peeled.by_ref);
                        let dst_g = self.union_residual_ty(p, by_ref)?;
                        match (self.union_residual_test(p, peeled, dst_g)?, sub) {
                            (ResidualTest::Never, _) => self.jump(fail, &[]),
                            (ResidualTest::Always, None) => self.jump(ok, &[]),
                            (ResidualTest::Cond(c), None) => self.br(c, (ok, &[]), (fail, &[])),
                            (ResidualTest::Always, Some(s)) => {
                                let dst_at = self.union_residual_narrow(p, peeled, dst_g)?;
                                let mid = self.create();
                                self.jump(mid, &[]);
                                todo.push((s, dst_at, mid, ok, fail));
                            }
                            (ResidualTest::Cond(c), Some(s)) => {
                                let mid = self.create();
                                self.br(c, (mid, &[]), (fail, &[]));
                                self.begin(mid);
                                self.live = live;
                                let dst_at = self.union_residual_narrow(p, peeled, dst_g)?;
                                let next_b = self.create();
                                self.jump(next_b, &[]);
                                todo.push((s, dst_at, next_b, ok, fail));
                            }
                        }
                    }
                },
                // matches/forms.rs: a float by `==`, a string by length and bytes.
                Pat::Lit(Lit::Float(_)) | Pat::NegLit(Lit::Float(_)) => {
                    let c = self.float_test(p, at)?;
                    self.br(c, (ok, &[]), (fail, &[]));
                }
                Pat::Lit(Lit::Str | Lit::ByteStr(_)) => self.str_test(p, at, ok, fail)?,
                Pat::Lit(_) | Pat::NegLit(_) => {
                    let (x, ty, _) = self.scalar_at(p, at)?;
                    let v = self.lit_bits(p, ty.bits(PTR_BITS))?;
                    let k = self.b.iconst(ty, v);
                    let c = self.b.icmp(IntCond::Eq, x, k);
                    self.br(c, (ok, &[]), (fail, &[]));
                }
                Pat::Range { lo, hi, inclusive } => {
                    let (x, ty, signed) = self.scalar_at(p, at)?;
                    let width = ty.bits(PTR_BITS);
                    let mut c = None;
                    if let Some(lo) = lo {
                        let v = self.lit_bits(lo, width)?;
                        let k = self.b.iconst(ty, v);
                        c = Some(self.b.icmp(if signed { IntCond::Sge } else { IntCond::Uge }, x, k));
                    }
                    if let Some(hi) = hi {
                        let v = self.lit_bits(hi, width)?;
                        let k = self.b.iconst(ty, v);
                        let cond = match (inclusive, signed) {
                            (true, true) => IntCond::Sle,
                            (true, false) => IntCond::Ule,
                            (false, true) => IntCond::Slt,
                            (false, false) => IntCond::Ult,
                        };
                        let hit = self.b.icmp(cond, x, k);
                        c = Some(match c {
                            Some(prev) => self.b.binary(Op::And, Flags::NONE, prev, hit),
                            None => hit,
                        });
                    }
                    let c = c.ok_or(malformed("a range pattern of no ends"))?;
                    self.br(c, (ok, &[]), (fail, &[]));
                }
                // A constant by its value (matches/forms.rs), else a struct or a variant.
                Pat::Path(_) => {
                    if !self.const_test(p, at, ok, fail)? {
                        match self.variant_test(p, at)? {
                            Some(c) => self.br(c, (ok, &[]), (fail, &[])),
                            None => self.jump(ok, &[]),
                        }
                    }
                }
                Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Record { .. } | Pat::Tuple { .. } | Pat::Slice { .. } => {
                    let c = match pat {
                        Pat::TupleStruct { .. } | Pat::Struct { .. } => self.variant_test(p, at)?,
                        Pat::Slice { elems, rest } => self.length_test(p, at, elems.len, rest.is_some())?,
                        _ => None,
                    };
                    if let Some(c) = c {
                        let mid = self.create();
                        self.br(c, (mid, &[]), (fail, &[]));
                        self.begin(mid);
                    }
                    parts.clear();
                    self.parts(p, at, &mut parts)?;
                    if parts.is_empty() {
                        self.jump(ok, &[]);
                        continue;
                    }
                    let start = self.create();
                    self.jump(start, &[]);
                    let mut from = start;
                    let n = parts.len();
                    for (j, &(q, a)) in parts.iter().enumerate() {
                        let to = if j + 1 == n { ok } else { self.create() };
                        todo.push((q, a, from, to, fail));
                        from = to;
                    }
                }
                Pat::Or { alts } => {
                    let list = self.body.pat_lists.get(alts.range()).ok_or(malformed("an or-pattern past `pat_lists`"))?;
                    if list.is_empty() {
                        return Err(malformed("an or-pattern of no alternatives"));
                    }
                    // Alternatives that bind: each, once its tests pass, stores its bindings
                    // into its names' first bindings' slots in a block of its own, then goes
                    // on to `ok` (the Reference, "Or-patterns"; matches/forms.rs).
                    let binds = self.binds_any(p)?;
                    if binds && alias.is_empty() {
                        self.first_binds(root, &mut alias)?;
                    }
                    // One expansion of a guarded arm: only the alternative it takes here.
                    let only = match choice {
                        Some(c) => {
                            let &(_, j, _) = c.iter().find(|&&(q, ..)| q == p).ok_or(malformed("an or-pattern its arm's expansion did not choose for"))?;
                            Some(j as usize)
                        }
                        None => None,
                    };
                    let mut from = self.create();
                    self.jump(from, &[]);
                    for (j, &alt) in list.iter().enumerate() {
                        if only.is_some_and(|taken| taken != j) {
                            continue;
                        }
                        let otherwise = if only.is_some() || j + 1 == list.len() { fail } else { self.create() };
                        let pass = if binds {
                            let bind = self.create();
                            self.begin(bind);
                            self.live = live;
                            self.bind_with(alt, at, &alias)?;
                            self.jump(ok, &[]);
                            bind
                        } else {
                            ok
                        };
                        todo.push((alt, at, from, pass, otherwise));
                        from = otherwise;
                    }
                }
                _ => {
                    return Err(BuildError::UnsupportedPat {
                        pat: p,
                        what: "a pattern other than a literal, a range, a path, a tuple, a struct, a variant, a slice, `&`, an or, a binding or `_`",
                    });
                }
            }
        }
        Ok(())
    }

    /// The bindings of pattern `root` (which matched) at `at`: each by its binding mode, a
    /// reference to its part or a copy of it, a by-value one owned in declaration order. An
    /// or-pattern whose alternatives bind was bound by `test_pat`, on the pass of the
    /// alternative that matched.
    pub(super) fn bind_tested(&mut self, root: PatId, at: At) -> Result<(), BuildError> {
        self.bind_with(root, at, &[])
    }

    /// `bind_tested`, each binding stored into the slot of its name's first binding in
    /// `alias` (an or-alternative's: matches/forms.rs `first_binds`), else its own.
    fn bind_with(&mut self, root: PatId, at: At, alias: &[(Name, PatId)]) -> Result<(), BuildError> {
        let mut todo: Vec<(PatId, At), A> = Vec::new_in(self.alloc);
        let mut parts: Vec<(PatId, At), A> = Vec::new_in(self.alloc);
        // An or-alternative's bindings (matches/forms.rs `first_binds`): bound on the
        // alternative's own pass, where its moves clear their bits; the first bindings own
        // their parts once, at the arm's pass (the `Pat::Or` row below).
        let alt = !alias.is_empty();
        todo.push((root, at));
        while let Some((p, at)) = todo.pop() {
            let pat = self.pat(p)?;
            match pat {
                Pat::Bind { ann, name, sub, .. } => {
                    let at = match self.checked.union_pat_of(p) {
                        None => at,
                        Some(UnionPat::Variant(v)) => {
                            let peeled = self.peel(p, at)?;
                            self.union_variant_at(p, peeled, v)?
                        }
                        Some(UnionPat::Residual) => {
                            let peeled = self.peel(p, at)?;
                            let by_ref = ann.by_ref.is_some() || (!ann.mutable && peeled.by_ref);
                            let dst_g = self.union_residual_ty(p, by_ref)?;
                            self.union_residual_narrow(p, peeled, dst_g)?
                        }
                    };
                    if at.copy && ann.by_ref == Some(Mutbl::Mut) {
                        return Err(BuildError::UnsupportedPat { pat: p, what: "a `ref mut` binding of a scrutinee held as a value (a copy)" });
                    }
                    // rules/pat.rs `pat_rule`: an explicit `ref`, else `mut` binds by move,
                    // else the default binding mode.
                    let by_ref = ann.by_ref.is_some() || (!ann.mutable && at.by_ref);
                    let mut moved = false;
                    let v = if by_ref {
                        match self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? {
                            // The wide pointer itself.
                            PNode::Slice(_) | PNode::Str => V::Place(at.at.0, at.at.1),
                            _ => V::Scalar(self.address(at.at)),
                        }
                    } else {
                        if self.needs_drop(at.g).map_err(|error| Self::pat_ty_err(p, error))? {
                            if alt && self.holding_moves() {
                                // A guarded arm runs its guard per alternative (matches/alts.rs);
                                // the held moves of one would be applied for all.
                                return Err(BuildError::UnsupportedPat {
                                    pat: p,
                                    what: "a by-value binding of a part that needs drop in an or-pattern of a guarded arm",
                                });
                            }
                            // walk/moves.rs: the part moves out of a scrutinee the match owns
                            // (its bit cleared); anywhere else refused by name.
                            self.move_out(p, at.g, alt)?;
                            moved = true;
                        }
                        let r = self.repr(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
                        self.load(r, at.at)
                    };
                    let target = Self::bind_target(alias, name, p);
                    self.bind_slot(target, v)?;
                    if moved && alt {
                        self.alt_moved(target);
                    } else if moved {
                        // The binding owns the part: dropped at its arm's end unless moved on;
                        // in a guarded arm, once its guard passed (walk/moves.rs `Pending`).
                        let hold = self.holding_moves();
                        self.arm_effect(super::moves::Pending::Own(target), hold)?;
                    } else if !by_ref {
                        self.own_kept(target, true);
                    }
                    if let Some(s) = sub {
                        todo.push((s, at));
                    }
                }
                Pat::Ref { inner, .. } => {
                    let PNode::Ref(_, to) = self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? else {
                        return Err(malformed("a `&` pattern over other than a reference"));
                    };
                    let mut a = self.deref_at(p, at, to)?;
                    a.by_ref = false;
                    todo.push((inner, a));
                }
                // Bound by `test_pat` on the pass of the alternative that matched; at the arm's
                // pass (not inside another alternative), each name whose binding moved its part
                // owns it, once, by its first binding (every alternative stored there): in
                // declaration order, as the bindings around it are owned.
                Pat::Or { .. } if !alt => {
                    let mut names: Vec<(Name, PatId), A> = Vec::new_in(self.alloc);
                    self.first_binds(p, &mut names)?;
                    for &(_, first) in names.iter() {
                        if self.take_alt_moved(first) {
                            let hold = self.holding_moves();
                            self.arm_effect(super::moves::Pending::Own(first), hold)?;
                        }
                    }
                }
                Pat::Or { .. } => {}
                Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Record { .. } | Pat::Slice { .. } => {
                    let a = self.peel(p, at)?;
                    parts.clear();
                    self.parts(p, a, &mut parts)?;
                    // Pushed last first, so they pop as written: each binding is owned (joins
                    // `scope_pats`) in declaration order, pre-order and left to right as walk.rs
                    // `own_pat` does, and the scope drops them in reverse ("Destructors": a
                    // pattern's variables drop in reverse order of declaration; the switch's
                    // walk.rs `bind_variant` and `own_pat` give the same order).
                    for &x in parts.iter().rev() {
                        todo.push(x);
                    }
                }
                // A literal, a range, a path, `_`: nothing bound.
                _ => {}
            }
        }
        Ok(())
    }

    /// Stores `v` into binding `p`'s slot, made on first use.
    fn bind_slot(&mut self, p: PatId, v: V) -> Result<(), BuildError> {
        let (home, repr) = match self.slots[p.ix()] {
            Some(found) => found,
            None => {
                let repr = self.local_repr(p)?;
                let (size, align) = self.size_align(repr);
                let slot = self.b.slot(size, align, SlotKind::Named);
                self.slots[p.ix()] = Some((Home::Slot(slot), repr));
                (Home::Slot(slot), repr)
            }
        };
        let to = self.home(home);
        self.store(v, repr, to)
    }

    /// Follows `x @ sub` (its test is `sub`'s) and `&inner` from `p` at `at`.
    fn through(&mut self, mut p: PatId, mut at: At) -> Result<(PatId, At), BuildError> {
        loop {
            match self.pat(p)? {
                Pat::Bind { sub: Some(s), .. } if self.checked.union_pat_of(p).is_none() => p = s,
                Pat::Ref { inner, .. } => {
                    let PNode::Ref(_, to) = self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? else {
                        return Err(malformed("a `&` pattern over other than a reference"));
                    };
                    at = self.deref_at(p, at, to)?;
                    at.by_ref = false;
                    p = inner;
                }
                _ => return Ok((p, at)),
            }
        }
    }

    /// The patterns that follow references (rules/pat.rs `step_pat`'s `peel`).
    fn peels(pat: Pat) -> bool {
        match pat {
            Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Record { .. } | Pat::Slice { .. } | Pat::Path(_) | Pat::Range { .. } => true,
            Pat::Lit(l) | Pat::NegLit(l) => !matches!(l, Lit::Str | Lit::ByteStr(_) | Lit::CStr),
            _ => false,
        }
    }

    /// Whether pattern `p`'s own type, as the check recorded it, is a reference.
    fn pat_is_ref(&mut self, p: PatId) -> Result<bool, BuildError> {
        let t = *self.checked.pat_ty.get(p.ix()).ok_or(malformed("a pattern without a type in the record"))?;
        let g = self.ground_ty(t).map_err(|error| Self::pat_ty_err(p, error))?;
        Ok(matches!(self.ground_node(g).map_err(|error| Self::pat_ty_err(p, error))?, PNode::Ref(..)))
    }

    /// Follows every reference at `at`; the default binding mode becomes `ref`.
    fn peel(&mut self, p: PatId, mut at: At) -> Result<At, BuildError> {
        while let PNode::Ref(_, inner) = self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? {
            at = self.deref_at(p, at, inner)?;
            at.by_ref = true;
        }
        Ok(at)
    }

    /// The place the reference at `at` points to, of type `inner`: an unsized one is held as
    /// the wide pointer's own memory.
    fn deref_at(&mut self, p: PatId, at: At, inner: PTy) -> Result<At, BuildError> {
        let to = match self.ground_node(inner).map_err(|error| Self::pat_ty_err(p, error))? {
            PNode::Slice(_) | PNode::Str => at.at,
            _ => (self.b.load(Ty::PTR, Flags::NONE, at.at.0, at.at.1, MemAttrs::aligned(3)), 0),
        };
        Ok(At { at: to, g: inner, by_ref: at.by_ref, copy: false })
    }

    /// The scalar at `at` for a literal or range test: its value, LIR type and signedness.
    fn scalar_at(&mut self, p: PatId, at: At) -> Result<(Value, Ty, bool), BuildError> {
        let repr = self.repr(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
        let (ty, signed) = match repr {
            Repr::Int { ty, signed } => (ty, signed),
            Repr::Bool => (Ty::I8, false),
            _ => {
                return Err(BuildError::UnsupportedPat {
                    pat: p,
                    what: "a literal or range pattern over other than an integer, a `char` or a `bool` (a float, a string)",
                });
            }
        };
        let attrs = self.mem(repr);
        Ok((self.b.load(ty, Flags::NONE, at.at.0, at.at.1, attrs), ty, signed))
    }

    /// The bits literal pattern `p` (a range's end too) stands for at `width`.
    fn lit_bits(&mut self, p: PatId, width: u32) -> Result<u128, BuildError> {
        match self.pat(p)? {
            Pat::Lit(Lit::Bool(b)) => Ok(u128::from(b)),
            Pat::Lit(Lit::Int(_) | Lit::Char) | Pat::NegLit(Lit::Int(_)) => {
                let v = self.adts.int_pat(self.body.owner, p).ok_or(BuildError::NoPatValue { pat: p })?;
                if mask(v, width) != v {
                    return Err(BuildError::UnsupportedPat { pat: p, what: "an integer literal past its type's width" });
                }
                Ok(match self.pat(p)? {
                    Pat::NegLit(_) => mask(0u128.wrapping_sub(v), width),
                    _ => v,
                })
            }
            // A range's end named by a constant: its evaluated value (matches/forms.rs).
            Pat::Path(_) => match self.pat_def(p) {
                Some(Def::Const(item)) => self.const_bits(p, item, width),
                _ => Err(BuildError::UnsupportedPat { pat: p, what: "a range's end named by other than a constant" }),
            },
            _ => Err(BuildError::UnsupportedPat { pat: p, what: "a literal pattern other than an integer, a `char` or a `bool`" }),
        }
    }

    /// The test that the value at `at` is variant pattern `p`'s variant: none for a struct.
    fn variant_test(&mut self, p: PatId, at: At) -> Result<Option<Value>, BuildError> {
        let v = Self::def_variant(self.pat_def(p)).ok_or(BuildError::UnsupportedPat {
            pat: p,
            what: "a path pattern to other than a struct or a variant (a constant)",
        })?;
        let l = self.layout(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
        let Shape::Enum { id, .. } = self.layouts[l as usize].shape else { return Ok(None) };
        let d = self.read_discriminant(id, at.at)?;
        let case = self.discriminant_case(id, v)?;
        let k = self.b.iconst(Ty::I64, case);
        Ok(Some(self.b.icmp(IntCond::Eq, d, k)))
    }

    /// The length test of a slice pattern of `n` elements (one of them `..` when `rest`) over
    /// a slice; none over an array (the check fixed its length).
    fn length_test(&mut self, p: PatId, at: At, n: u32, rest: bool) -> Result<Option<Value>, BuildError> {
        let PNode::Slice(_) = self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? else { return Ok(None) };
        let len = self.b.load(Ty::I64, Flags::NONE, at.at.0, at.at.1 + 8, MemAttrs::aligned(3));
        let fixed = n - u32::from(rest);
        let k = self.b.iconst(Ty::I64, u128::from(fixed));
        Ok(Some(self.b.icmp(if rest { IntCond::Uge } else { IntCond::Eq }, len, k)))
    }

    /// The subpatterns of `p` (a tuple, a struct or variant, a slice or array pattern) at
    /// `at` (references followed), each with its part's place, into `out`.
    fn parts(&mut self, p: PatId, at: At, out: &mut Vec<(PatId, At), A>) -> Result<(), BuildError> {
        let part = |at: At, off: i64, g: PTy| -> Result<At, BuildError> {
            let off = i32::try_from(i64::from(at.at.1) + off).map_err(|_| BuildError::UnsupportedPat { pat: p, what: "a part past an `i32` offset" })?;
            Ok(At { at: (at.at.0, off), g, by_ref: at.by_ref, copy: at.copy })
        };
        match self.pat(p)? {
            Pat::Tuple { elems, rest } | Pat::TupleStruct { elems, rest, .. } => {
                let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                if subs.is_empty() {
                    return Ok(());
                }
                let l = self.layout(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
                let (first, len) = match self.pat(p)? {
                    Pat::TupleStruct { .. } => {
                        let variant = Self::def_variant(self.pat_def(p)).ok_or(malformed("a variant pattern the check did not resolve"))?;
                        let (first, len, _) = self.variant_rows(l, variant)?;
                        (first, len)
                    }
                    _ => match self.layouts[l as usize].shape {
                        Shape::Fields { first, len } => (first, len),
                        _ => return Err(malformed("a tuple pattern over other than a tuple")),
                    },
                };
                let n = len as usize;
                let fits = match rest {
                    Some(_) => subs.len() <= n,
                    None => subs.len() == n,
                };
                if !fits {
                    return Err(malformed("a tuple or variant pattern of another arity than its type"));
                }
                for (j, &sub) in subs.iter().enumerate() {
                    let k = match rest {
                        Some(r) if j >= r as usize => n - (subs.len() - j),
                        _ => j,
                    };
                    let row = first as usize + k;
                    let off = self.layout_fields[row].1;
                    let g = self.layout_field_tys[row];
                    out.push((sub, part(at, i64::from(off), g)?));
                }
            }
            Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                if fps.is_empty() {
                    return Ok(());
                }
                let l = self.layout(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
                let variant = if matches!(self.pat(p)?, Pat::Record { .. }) {
                    0
                } else {
                    Self::def_variant(self.pat_def(p)).ok_or(malformed("a variant pattern the check did not resolve"))?
                };
                let (first, len, _) = self.variant_rows(l, variant)?;
                for fp in fps {
                    let row = (first..first + len)
                        .find(|&i| self.layout_fields[i as usize].0 == fp.name)
                        .ok_or(malformed("a struct pattern naming a field its variant lacks"))?;
                    let off = self.layout_fields[row as usize].1;
                    let g = self.layout_field_tys[row as usize];
                    out.push((fp.pat, part(at, i64::from(off), g)?));
                }
            }
            Pat::Slice { elems, rest } => {
                let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                let (elem, array) = match self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? {
                    PNode::Array(elem, n) => (elem, Some(self.ground_len(n).map_err(|error| Self::pat_ty_err(p, error))?)),
                    PNode::Slice(elem) => (elem, None),
                    _ => return Err(malformed("a slice pattern over other than an array or a slice")),
                };
                let er = self.repr(elem).map_err(|error| Self::pat_ty_err(p, error))?;
                let stride = i64::from(self.size_align(er).0);
                let count = subs.len() as i64;
                match array {
                    Some(n) => {
                        let n = i64::try_from(n).map_err(|_| malformed("an array of 2^63 elements or more"))?;
                        for (j, &sub) in subs.iter().enumerate() {
                            let j = j as i64;
                            let (k, g) = match rest {
                                Some(r) if j == i64::from(r) => (j, self.rest_ty(sub)?),
                                Some(r) if j > i64::from(r) => (n - (count - j), elem),
                                _ => (j, elem),
                            };
                            out.push((sub, part(at, k * stride, g)?));
                        }
                    }
                    None => {
                        let data = self.b.load(Ty::PTR, Flags::NONE, at.at.0, at.at.1, MemAttrs::aligned(3));
                        let len = self.b.load(Ty::I64, Flags::NONE, at.at.0, at.at.1 + 8, MemAttrs::aligned(3));
                        let base = At { at: (data, 0), g: elem, by_ref: at.by_ref, copy: false };
                        for (j, &sub) in subs.iter().enumerate() {
                            let j = j as i64;
                            match rest {
                                // The subslice: a wide pointer of its own, past the prefix,
                                // `len - (count - 1)` long.
                                Some(r) if j == i64::from(r) => {
                                    let w = self.b.slot(16, 3, SlotKind::Temp);
                                    let wa = self.b.slot_addr(w);
                                    let off = self.b.iconst(Ty::I64, (j * stride) as u128);
                                    let from = self.b.ptr_add(Flags::NONE, data, off);
                                    self.b.store(Flags::NONE, from, wa, 0, MemAttrs::aligned(3));
                                    let fixed = self.b.iconst(Ty::I64, (count - 1) as u128);
                                    let left = self.b.binary(Op::Sub, Flags::NONE, len, fixed);
                                    self.b.store(Flags::NONE, left, wa, 8, MemAttrs::aligned(3));
                                    let g = self.rest_ty(sub)?;
                                    out.push((sub, At { at: (wa, 0), g, by_ref: at.by_ref, copy: false }));
                                }
                                // The suffix: `len - (count - j)` from the start.
                                Some(r) if j > i64::from(r) => {
                                    let back = self.b.iconst(Ty::I64, (count - j) as u128);
                                    let k = self.b.binary(Op::Sub, Flags::NONE, len, back);
                                    let s = self.b.iconst(Ty::I64, stride as u128);
                                    let off = self.b.binary(Op::Mul, Flags::NONE, k, s);
                                    let ptr = self.b.ptr_add(Flags::NONE, data, off);
                                    out.push((sub, At { at: (ptr, 0), g: elem, by_ref: at.by_ref, copy: false }));
                                }
                                _ => out.push((sub, part(base, j * stride, elem)?)),
                            }
                        }
                    }
                }
            }
            _ => return Err(malformed("the parts of a pattern that has none")),
        }
        Ok(())
    }

    /// The type of a slice pattern's `..` element (`[T]`, or `[T; N - k]` over an array): the
    /// check's type of it.
    fn rest_ty(&mut self, sub: PatId) -> Result<PTy, BuildError> {
        let t = *self.checked.pat_ty.get(sub.ix()).ok_or(malformed("a pattern without a type in the record"))?;
        self.ground_ty(t).map_err(|error| Self::pat_ty_err(sub, error))
    }

    /// Some binding inside pattern `p`, by a work list.
    fn binds_any(&self, p: PatId) -> Result<bool, BuildError> {
        let mut todo: Vec<PatId, A> = Vec::new_in(self.alloc);
        todo.push(p);
        while let Some(q) = todo.pop() {
            match self.pat(q)? {
                Pat::Bind { .. } => return Ok(true),
                Pat::Ref { inner, .. } => todo.push(inner),
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } | Pat::Or { alts: elems } => {
                    let subs = self.body.pat_lists.get(elems.range()).ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                    for &s in subs {
                        todo.push(s);
                    }
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    let fps = self.body.field_pats.get(fields.range()).ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    for fp in fps {
                        todo.push(fp.pat);
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// The test that a union value at `at` (references peeled) holds variant `v`: `None` when it
    /// always holds `v` (single-arm transparent union or single-present-variant layout).
    fn union_variant_test(&mut self, p: PatId, at: At, v: u32) -> Result<Option<Value>, BuildError> {
        let PNode::Union(ul) = self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? else {
            return Err(malformed("a union variant pattern over other than a structural union"));
        };
        if ul.len == 1 {
            if v != 0 {
                return Err(malformed("a single-member union pattern at a non-zero variant"));
            }
            return Ok(None);
        }
        let l = self.layout(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
        let l_idx = usize::try_from(l).map_err(|_| malformed("a layout index past `usize`"))?;
        let Some(&Shape::Enum { id, .. }) = self.layouts.get(l_idx).map(|row| &row.shape) else {
            return Err(malformed("a multi-member union not laid out as an enum"));
        };
        match self.ls.get(id).variants {
            Variants::Single { index } if index == v => Ok(None),
            Variants::Single { .. } => {
                let z = self.b.iconst(Ty::I8, 0);
                let o = self.b.iconst(Ty::I8, 1);
                Ok(Some(self.b.icmp(IntCond::Eq, z, o)))
            }
            Variants::Multiple { .. } => {
                let d = self.read_discriminant(id, at.at)?;
                let case = self.discriminant_case(id, v)?;
                let k = self.b.iconst(Ty::I64, case);
                Ok(Some(self.b.icmp(IntCond::Eq, d, k)))
            }
        }
    }

    /// Projects union place `at` (references peeled) to the payload of member `v`.
    fn union_variant_at(&mut self, p: PatId, at: At, v: u32) -> Result<At, BuildError> {
        let PNode::Union(ul) = self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? else {
            return Err(malformed("a union variant pattern over other than a structural union"));
        };
        if ul.len == 1 {
            if v != 0 {
                return Err(malformed("a single-member union pattern at a non-zero variant"));
            }
            let only = self
                .ground
                .view()
                .list(ul)
                .ok()
                .and_then(|xs| xs.first().copied())
                .ok_or(malformed("a union list past the ground table"))?;
            return Ok(At { at: at.at, g: only, by_ref: at.by_ref, copy: at.copy });
        }
        let l = self.layout(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
        let (first, len, _) = self.variant_rows(l, v)?;
        if len != 1 {
            return Err(malformed("a union variant without exactly one payload field"));
        }
        let first_idx = usize::try_from(first).map_err(|_| malformed("a union field index past `usize`"))?;
        let off = self
            .layout_fields
            .get(first_idx)
            .ok_or(malformed("a union variant's field past `layout_fields`"))?
            .1;
        let off_i32 = i32::try_from(i64::from(at.at.1) + i64::from(off))
            .map_err(|_| BuildError::UnsupportedPat { pat: p, what: "a part past an `i32` offset" })?;
        let member_g = *self
            .layout_field_tys
            .get(first_idx)
            .ok_or(malformed("a union variant's field type past `layout_field_tys`"))?;
        Ok(At { at: (at.at.0, off_i32), g: member_g, by_ref: at.by_ref, copy: at.copy })
    }

    /// The narrowed sub-union ground type of a `UnionPat::Residual` binding pattern `p`.
    fn union_residual_ty(&mut self, p: PatId, by_ref: bool) -> Result<PTy, BuildError> {
        let lt = *self.checked.local_ty.get(p.ix()).ok_or(malformed("a binding without a type in the record"))?;
        let lg = self.ground_ty(lt).map_err(|error| Self::pat_ty_err(p, error))?;
        if by_ref
            && let PNode::Ref(_, inner) = self.ground_node(lg).map_err(|error| Self::pat_ty_err(p, error))?
        {
            return Ok(inner);
        }
        Ok(lg)
    }

    /// Tests whether union value `at` holds any variant whose ground type appears in `dst_g`.
    fn union_residual_test(&mut self, p: PatId, at: At, dst_g: PTy) -> Result<ResidualTest, BuildError> {
        let PNode::Union(src_l) = self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? else {
            return Err(malformed("a union residual pattern over other than a structural union"));
        };
        let PNode::Union(dst_l) = self.ground_node(dst_g).map_err(|error| Self::pat_ty_err(p, error))? else {
            return Err(malformed("a union residual binding whose type is not a structural union"));
        };
        let mut src_members: Vec<PTy, A> = Vec::new_in(self.alloc);
        src_members.extend_from_slice(
            self.ground
                .view()
                .list(src_l)
                .map_err(|_| malformed("a source union's members past the ground table"))?,
        );
        let mut dst_members: Vec<PTy, A> = Vec::new_in(self.alloc);
        dst_members.extend_from_slice(
            self.ground
                .view()
                .list(dst_l)
                .map_err(|_| malformed("a target union's members past the ground table"))?,
        );
        if let [only] = src_members.as_slice() {
            return Ok(if dst_members.contains(only) {
                ResidualTest::Always
            } else {
                ResidualTest::Never
            });
        }
        let l = self.layout(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
        let l_idx = usize::try_from(l).map_err(|_| malformed("a layout index past `usize`"))?;
        let Some(&Shape::Enum { id, .. }) = self.layouts.get(l_idx).map(|row| &row.shape) else {
            return Err(malformed("a multi-member union not laid out as an enum"));
        };
        match self.ls.get(id).variants {
            Variants::Single { index } => {
                let idx = usize::try_from(index).ok();
                Ok(if idx.and_then(|i| src_members.get(i)).is_some_and(|sm| dst_members.contains(sm)) {
                    ResidualTest::Always
                } else {
                    ResidualTest::Never
                })
            }
            Variants::Multiple { .. } => {
                let d = self.read_discriminant(id, at.at)?;
                let mut cond: Option<Value> = None;
                for (i_idx, &sm) in src_members.iter().enumerate() {
                    if !dst_members.contains(&sm) {
                        continue;
                    }
                    let i = u32::try_from(i_idx).map_err(|_| malformed("a union variant index past `u32`"))?;
                    let case = self.discriminant_case(id, i)?;
                    let k = self.b.iconst(Ty::I64, case);
                    let eq = self.b.icmp(IntCond::Eq, d, k);
                    cond = Some(match cond {
                        Some(prev) => self.b.binary(Op::Or, Flags::NONE, prev, eq),
                        None => eq,
                    });
                }
                Ok(match cond {
                    Some(c) => ResidualTest::Cond(c),
                    None => ResidualTest::Never,
                })
            }
        }
    }

    /// Materializes the narrowed sub-union `dst_g` from source union `at` into a temp slot.
    fn union_residual_narrow(&mut self, p: PatId, at: At, dst_g: PTy) -> Result<At, BuildError> {
        let PNode::Union(src_l) = self.ground_node(at.g).map_err(|error| Self::pat_ty_err(p, error))? else {
            return Err(malformed("a union residual pattern over other than a structural union"));
        };
        let PNode::Union(dst_l) = self.ground_node(dst_g).map_err(|error| Self::pat_ty_err(p, error))? else {
            return Err(malformed("a union residual binding whose type is not a structural union"));
        };
        let mut src_members: Vec<PTy, A> = Vec::new_in(self.alloc);
        src_members.extend_from_slice(
            self.ground
                .view()
                .list(src_l)
                .map_err(|_| malformed("a source union's members past the ground table"))?,
        );
        let mut dst_members: Vec<PTy, A> = Vec::new_in(self.alloc);
        dst_members.extend_from_slice(
            self.ground
                .view()
                .list(dst_l)
                .map_err(|_| malformed("a target union's members past the ground table"))?,
        );
        let dst_repr = self.repr(dst_g).map_err(|error| Self::pat_ty_err(p, error))?;
        if dst_repr == Repr::None {
            return Ok(At { at: (self.b.uninit(Ty::PTR), 0), g: dst_g, by_ref: at.by_ref, copy: true });
        }
        let Repr::Agg(dst_layout) = dst_repr else {
            return Err(malformed("a narrowed sub-union not held as an aggregate"));
        };
        let dst_at = self.temp(dst_repr);
        if self.live {
            let src_layout = self.layout(at.g).map_err(|error| Self::pat_ty_err(p, error))?;
            self.remap_union(None, src_layout, at.at, &src_members, dst_layout, dst_at, &dst_members)?;
        }
        Ok(At { at: dst_at, g: dst_g, by_ref: at.by_ref, copy: true })
    }
}
