//! Which arms of a `match` rustc builds a way into.
//!
//! rustc lowers the arms of a `match` to one tree of tests (`rustc_mir_build`'s
//! `builder::matches`). An arm that an arm before it has taken every value from is still
//! built, but no edge of the tree leads to it, and its analysis of what a drop finds moved out
//! never sees what that arm binds. rustd tests the arms one after the other, where every arm
//! is on some way. This follows rustc's construction far enough to say which arms it gets
//! to, and builds nothing: the candidates are sorted by the same tests in the same order.

use blaze_ids::radix;
use blaze_mem::Frame;
use hir_def::{
    EnumVariantId, Lookup, VariantId,
    hir::{Expr, Literal, MatchArm, Pat, PatId, UnaryOp},
};
use rustc_type_ir::inherent::IntoKind;
use syntax::ast::RangeOp;

use crate::{
    infer::PatAdjust,
    mir::{
        framed::{self, Bits, FrameMap},
        lower::{MirLowerCtx, Ty, TyKind},
    },
};

/// One step from a matched value to a part of it.
#[derive(Clone, Copy, PartialEq)]
enum Step {
    Field(u32),
    Variant(EnumVariantId),
    Index(u64),
    FromEnd(u64),
    Deref,
}

/// What a pattern asks of the place it is at.
#[derive(Clone, PartialEq)] // clone: inside `Pair`
enum Ask<'t> {
    Variant(EnumVariantId),
    /// An integer or a `char`, as a number that orders the way the type does.
    Int(u128),
    Bool(bool),
    /// Both ends are in it.
    Range(u128, u128),
    /// A constant that is compared with `==`, and whether it is negated.
    Equal(&'t Literal, bool),
    Len {
        len: u64,
        or_more: bool,
    },
    Or(Vec<Vec<Pair<'t>, &'t Frame>, &'t Frame>),
}

/// A place, what a pattern asks of it, and what the pattern asks inside once that holds.
#[derive(Clone, PartialEq)] // clone: what is left after an or-pattern, in the frame, copied to each candidate that goes on to it
struct Pair<'t> {
    place: Vec<Step, &'t Frame>,
    ask: Ask<'t>,
    inside: Vec<Pair<'t>, &'t Frame>,
}

/// One way an arm can match: what is still to be tested for it.
#[cfg_attr(test, derive(Clone))] // clone: tests run the old and the new tree on the same arms
struct Candidate<'t> {
    arm: usize,
    pairs: Vec<Pair<'t>, &'t Frame>,
    guarded: bool,
    /// What an or-pattern left to be tested after its alternative has matched, in
    /// [`Tree::thens`].
    then: Option<usize>,
}

/// Candidates in the order they are matched, the first one last, so that taking the first is
/// a pop and what a test leaves stays where it is.
type Stack<'t> = Vec<Candidate<'t>, &'t Frame>;

/// Where a test sends a candidate.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Branch {
    Variant(EnumVariantId),
    Int(u128),
    Success,
    Failure,
}

/// The run over the candidates. A `bool` that goes with a list of candidates says whether
/// the block they start from can be reached.
struct Tree<'t, 'f> {
    frame: &'t Frame,
    /// What each expanded or-pattern left to be tested, shared by its alternatives.
    thens: Vec<Vec<Pair<'t>, &'t Frame>, &'t Frame>,
    reached: Bits<'f>,
    /// Whether what an or-pattern left to be tested has failed since the or-pattern was
    /// expanded: that goes to where the whole expansion falls through to.
    escaped: bool,
}

impl<'t> Tree<'t, '_> {
    /// Matches the candidates in order, and says whether none of them matching can happen.
    /// Or-patterns are expanded before any test, as soon as any candidate starts with one,
    /// so that the alternatives are sorted by the tests of the arms before them.
    fn run(&mut self, mut start: bool, mut candidates: Stack<'t>) -> bool {
        // The lowest place of a candidate that starts with an or-pattern. A candidate below the
        // top is not changed while it waits, so one is left while this is below the length.
        let ors = candidates.iter().position(starts_with_or);
        loop {
            let Some(first) = candidates.last() else { return start };
            match first.pairs.first() {
                None => {
                    let first = candidates.pop().expect("the first candidate");
                    start = self.matched(start, first);
                }
                Some(_) if ors.is_some_and(|it| it < candidates.len()) => {
                    start = self.expand(start, &mut candidates)
                }
                Some(_) => start = self.test(start, &mut candidates),
            }
        }
    }

    /// A candidate with nothing left to test. Says whether the candidates after it can
    /// still be reached: only through a guard.
    fn matched(&mut self, start: bool, candidate: Candidate<'t>) -> bool {
        let Candidate { arm, guarded, then, .. } = candidate;
        let Some(then) = then else {
            if start {
                self.reached.insert(arm);
            }
            return start && guarded;
        };
        let pairs = self.thens[then].to_vec_in(self.frame);
        let mut rest = Vec::with_capacity_in(1, self.frame);
        rest.push(Candidate { arm, pairs, guarded, then: None });
        let fell = self.run(start, rest);
        if guarded {
            return fell;
        }
        self.escaped |= fell;
        false
    }

    /// Expands the or-patterns that candidates start with into a candidate for each
    /// alternative. The candidates after one that has more to test after its or-pattern are
    /// left in `candidates` for after.
    fn expand(&mut self, start: bool, candidates: &mut Stack<'t>) -> bool {
        let last = candidates.iter().rposition(|it| starts_with_or(it) && it.pairs.len() > 1);
        let taken = candidates.split_off(last.unwrap_or(0));
        let mut expanded = Vec::with_capacity_in(taken.len(), self.frame);
        for mut candidate in taken.into_iter().rev() {
            if !starts_with_or(&candidate) {
                expanded.push(candidate);
                continue;
            }
            let Ask::Or(alternatives) = candidate.pairs.remove(0).ask else { continue };
            let mut left = Vec::new_in(self.frame);
            left.append(&mut candidate.pairs);
            if let Some(after) = candidate.then {
                left.extend_from_slice(&self.thens[after]);
            }
            let then = (!left.is_empty()).then(|| {
                self.thens.push(left);
                self.thens.len() - 1
            });
            expanded.extend(alternatives.into_iter().map(|pairs| Candidate {
                arm: candidate.arm,
                pairs,
                guarded: candidate.guarded,
                then,
            }));
        }
        expanded.reverse();
        let outer = std::mem::take(&mut self.escaped);
        let fell = self.run(start, expanded);
        let escaped = std::mem::replace(&mut self.escaped, outer);
        fell || escaped
    }

    /// Makes the test the first candidate asks for first, sorts the candidates that the
    /// outcome says something about into its branches, and matches each branch. The
    /// candidates from the first one it says nothing about are left in `candidates` for after.
    fn test(&mut self, start: bool, candidates: &mut Stack<'t>) -> bool {
        let Some(first) = candidates.last() else { return start };
        // The first candidate is sorted by the pair the test is made of.
        let Some(branch) = own_branch(&first.pairs[0].ask) else { return start };
        let mut first = candidates.pop().expect("the first candidate");
        let Pair { place, ask, inside } = first.pairs.remove(0);
        first.pairs.extend(inside);
        radix::sort_by_key(&mut first.pairs, or_last);
        let mut branches = Branches::new_in(self.frame);
        branches.add(branch, first, &place, &ask);
        while let Some(mut candidate) = candidates.pop() {
            let Some(branch) = sort(&place, &ask, &mut candidate, &branches) else {
                candidates.push(candidate);
                break;
            };
            branches.add(branch, candidate, &place, &ask);
        }
        // A switch always has a target for the values none of the candidates named, and
        // the test of a `bool` has one for the value the first candidate did not ask for.
        let mut fell = start
            && match ask {
                Ask::Bool(_) => !(branches.success && branches.failure),
                _ => !branches.failure,
            };
        for (_, mut sorted) in branches.list {
            sorted.reverse();
            fell |= self.run(start, sorted);
        }
        fell
    }
}

/// Up to how many branches of a test one is found by a scan rather than through the index.
const LINEAR: usize = 8;

/// The branches of one test, in the order a candidate was first sorted into each, which is
/// the order they are matched in, and what `sort` asks of them.
struct Branches<'t> {
    /// Each branch's candidates in the order they were sorted in.
    list: Vec<(Branch, Vec<Candidate<'t>, &'t Frame>), &'t Frame>,
    /// By branch, where it is in `list`, once there are more than `LINEAR`.
    index: FrameMap<'t, Branch, u32>,
    /// The values of the `Int` branches, ascending.
    ints: Vec<u128, &'t Frame>,
    /// What the ranges at the tested place of the candidates in the `Failure` branch of an
    /// `Int` test cover: disjoint ranges, ascending.
    failed: Vec<(u128, u128), &'t Frame>,
    success: bool,
    failure: bool,
}

impl<'t> Branches<'t> {
    fn new_in(frame: &'t Frame) -> Self {
        Branches {
            list: Vec::new_in(frame),
            index: framed::map_in(frame),
            ints: Vec::new_in(frame),
            failed: Vec::new_in(frame),
            success: false,
            failure: false,
        }
    }

    /// Puts `candidate` in `branch` of the test `ask` makes of `place`.
    fn add(&mut self, branch: Branch, candidate: Candidate<'t>, place: &[Step], ask: &Ask<'t>) {
        if branch == Branch::Failure && matches!(ask, Ask::Int(_)) {
            for pair in &candidate.pairs {
                if let Ask::Range(lo, hi) = pair.ask
                    && pair.place == place
                {
                    self.fail_range(lo, hi);
                }
            }
        }
        let found = match self.list.len() <= LINEAR {
            true => self.list.iter().position(|(it, _)| *it == branch),
            false => self.index.get(&branch).map(|&it| it as usize),
        };
        if let Some(at) = found {
            self.list[at].1.push(candidate);
            return;
        }
        match branch {
            Branch::Int(value) => {
                let at = self.ints.partition_point(|&it| it < value);
                self.ints.insert(at, value);
            }
            Branch::Success => self.success = true,
            Branch::Failure => self.failure = true,
            Branch::Variant(_) => (),
        }
        let mut sorted = Vec::with_capacity_in(1, *self.list.allocator());
        sorted.push(candidate);
        self.list.push((branch, sorted));
        let len = self.list.len();
        if len == LINEAR + 1 {
            let list = &self.list;
            self.index.extend(list.iter().enumerate().map(|(at, (it, _))| (*it, at as u32)));
        } else if len > LINEAR + 1 {
            self.index.insert(branch, (len - 1) as u32);
        }
    }

    fn fail_range(&mut self, mut lo: u128, mut hi: u128) {
        let from = self.failed.partition_point(|&(_, end)| end < lo);
        let to = self.failed.partition_point(|&(start, _)| start <= hi);
        if from < to {
            lo = lo.min(self.failed[from].0);
            hi = hi.max(self.failed[to - 1].1);
        }
        self.failed.drain(from..to);
        self.failed.insert(from, (lo, hi));
    }

    /// Whether a range of a candidate in the `Failure` branch of an `Int` test has `value`.
    fn failed_covers(&self, value: u128) -> bool {
        let at = self.failed.partition_point(|&(_, end)| end < value);
        self.failed.get(at).is_some_and(|&(start, _)| start <= value)
    }

    /// Whether the value of an `Int` branch is in `lo..=hi`.
    fn names_int_in(&self, lo: u128, hi: u128) -> bool {
        let at = self.ints.partition_point(|&it| it < lo);
        self.ints.get(at).is_some_and(|&it| it <= hi)
    }
}

/// Whether the first thing left to test for `candidate` is an or-pattern.
fn starts_with_or(candidate: &Candidate<'_>) -> bool {
    matches!(candidate.pairs.first(), Some(Pair { ask: Ask::Or(_), .. }))
}

/// The key that puts a candidate's or-patterns after the rest of what it asks.
fn or_last(pair: &Pair<'_>) -> u8 {
    u8::from(matches!(pair.ask, Ask::Or(_)))
}

/// The branch of the test `ask` makes that the candidate `ask` is the first pair of goes
/// in, as [`sort`] finds it: the test answers it. An or-pattern is expanded, not tested.
fn own_branch(ask: &Ask<'_>) -> Option<Branch> {
    Some(match *ask {
        Ask::Variant(variant) => Branch::Variant(variant),
        Ask::Int(value) => Branch::Int(value),
        Ask::Bool(value) => {
            if value {
                Branch::Success
            } else {
                Branch::Failure
            }
        }
        Ask::Range(..) | Ask::Equal(..) | Ask::Len { .. } => Branch::Success,
        Ask::Or(_) => return None,
    })
}

/// The branch of the test `ask` makes of `place` that `candidate` belongs in, if the outcome
/// decides anything for it. A candidate the test answers a pair of loses the pair and gets
/// what was inside it, with the or-patterns last.
fn sort<'t>(
    place: &[Step],
    ask: &Ask<'t>,
    candidate: &mut Candidate<'t>,
    branches: &Branches<'t>,
) -> Option<Branch> {
    let at = candidate.pairs.iter().position(|it| it.place == place)?;
    let (branch, answered) = match (ask, &candidate.pairs[at].ask) {
        (Ask::Variant(_), &Ask::Variant(variant)) => (Branch::Variant(variant), true),
        (Ask::Int(_), &Ask::Int(value)) => {
            // A range of an earlier candidate that has the value would be tested first.
            if branches.failed_covers(value) {
                return None;
            }
            (Branch::Int(value), true)
        }
        (Ask::Int(_), &Ask::Range(lo, hi)) => {
            if branches.names_int_in(lo, hi) {
                return None;
            }
            (Branch::Failure, false)
        }
        (Ask::Bool(_), &Ask::Bool(value)) => {
            (if value { Branch::Success } else { Branch::Failure }, true)
        }
        (Ask::Range(lo, hi), &Ask::Range(other_lo, other_hi)) => {
            if (lo, hi) == (&other_lo, &other_hi) {
                (Branch::Success, true)
            } else if other_hi < *lo || *hi < other_lo {
                (Branch::Failure, false)
            } else {
                return None;
            }
        }
        (Ask::Range(lo, hi), &Ask::Int(value)) if value < *lo || *hi < value => {
            (Branch::Failure, false)
        }
        (Ask::Equal(..), other @ Ask::Equal(..)) if ask == other => (Branch::Success, true),
        (&Ask::Len { len: tested, or_more: false }, &Ask::Len { len, or_more }) => {
            match (tested.cmp(&len), or_more) {
                (std::cmp::Ordering::Equal, false) => (Branch::Success, true),
                (std::cmp::Ordering::Less, _) | (std::cmp::Ordering::Greater, false) => {
                    (Branch::Failure, false)
                }
                _ => return None,
            }
        }
        (&Ask::Len { len: tested, or_more: true }, &Ask::Len { len, or_more }) => {
            match (tested.cmp(&len), or_more) {
                (std::cmp::Ordering::Equal, true) => (Branch::Success, true),
                (std::cmp::Ordering::Greater, true) => return None,
                _ => (Branch::Failure, false),
            }
        }
        _ => return None,
    };
    if answered {
        let pair = candidate.pairs.remove(at);
        candidate.pairs.extend(pair.inside);
        radix::sort_by_key(&mut candidate.pairs, or_last);
    }
    Some(branch)
}

impl<'a, 'f, 'db> MirLowerCtx<'a, 'f, 'db> {
    /// For each arm, whether rustc's tree of tests has a way into it. Every arm, where a
    /// pattern has something in it that is not followed here.
    pub(super) fn reached_arms(&self, arms: &[MatchArm]) -> Bits<'f> {
        let frame = self.frame;
        let mut reached = framed::bits_in(arms.len(), frame);
        let mut candidates = Vec::with_capacity_in(arms.len(), frame);
        for (arm, it) in arms.iter().enumerate() {
            let mut pairs = Vec::new_in(frame);
            if self.pairs_of(it.pat, Vec::new_in(frame), &mut pairs).is_none() {
                reached.insert_range(0..arms.len());
                return reached;
            }
            radix::sort_by_key(&mut pairs, or_last);
            candidates.push(Candidate { arm, pairs, guarded: it.guard.is_some(), then: None });
        }
        candidates.reverse();
        let mut tree = Tree { frame, thens: Vec::new_in(frame), reached, escaped: false };
        tree.run(true, candidates);
        tree.reached
    }

    /// Adds what `pattern` asks of the value at `place` to `pairs`. `None` when the pattern
    /// has a part whose test is not known here.
    fn pairs_of<'t>(
        &self,
        pattern: PatId,
        mut place: Vec<Step, &'t Frame>,
        pairs: &mut Vec<Pair<'t>, &'t Frame>,
    ) -> Option<()>
    where
        'a: 't,
    {
        let frame = *place.allocator();
        // A deref through a smart pointer that is not a `Box` is a call into a temporary, a
        // test of its own that is not followed here.
        let adjustments = self.infer.pat_adjustments.get(&pattern).unwrap_or_default();
        for adjustment in adjustments {
            if adjustment.kind == PatAdjust::OverloadedDeref && !adjustment.source.as_ref().is_box()
            {
                return None;
            }
            place.push(Step::Deref);
        }
        let ty = self.infer.pat_ty(pattern);
        match self.store.pat(pattern) {
            Pat::Wild | Pat::Bind { subpat: None, .. } => (),
            Pat::Bind { subpat: Some(inner), .. } => self.pairs_of(inner, place, pairs)?,
            Pat::Ref { pat: inner, .. } | Pat::Box { inner } => {
                place.push(Step::Deref);
                self.pairs_of(inner, place, pairs)?
            }
            Pat::Deref { inner } if ty.is_box() => {
                place.push(Step::Deref);
                self.pairs_of(inner, place, pairs)?
            }
            Pat::Tuple { args, ellipsis } => {
                let TyKind::Tuple(fields) = ty.kind() else { return None };
                self.pairs_of_fields(args, ellipsis, fields.len(), &place, pairs)?
            }
            Pat::Or(alternatives) => {
                let mut each = Vec::with_capacity_in(alternatives.len(), frame);
                for &it in alternatives.iter() {
                    let mut pairs = Vec::new_in(frame);
                    self.pairs_of(it, place.to_vec_in(frame), &mut pairs)?;
                    radix::sort_by_key(&mut pairs, or_last);
                    each.push(pairs);
                }
                pairs.push(Pair { place, ask: Ask::Or(each), inside: Vec::new_in(frame) })
            }
            Pat::Lit(literal) => {
                let ask = self.constant_ask(ty, literal)?;
                pairs.push(Pair { place, ask, inside: Vec::new_in(frame) })
            }
            Pat::Range { start, end, range_type } => {
                let (least, most, bounded) = int_bounds(ty)?;
                let end_of = |end: &Option<_>, missing| match end {
                    Some(it) => match self.constant_ask(ty, *it)? {
                        Ask::Int(value) => Some(value),
                        _ => None,
                    },
                    None => Some(missing),
                };
                let (lo, hi) = (end_of(&start, least)?, end_of(&end, most)?);
                let hi = match (range_type, end) {
                    (RangeOp::Exclusive, Some(_)) => hi.checked_sub(1)?,
                    _ => hi,
                };
                let ask = match lo.cmp(&hi) {
                    std::cmp::Ordering::Greater => return None,
                    std::cmp::Ordering::Equal => Ask::Int(lo),
                    // A range of every value of the type asks nothing.
                    std::cmp::Ordering::Less if bounded && (lo, hi) == (least, most) => {
                        return Some(());
                    }
                    std::cmp::Ordering::Less => Ask::Range(lo, hi),
                };
                pairs.push(Pair { place, ask, inside: Vec::new_in(frame) })
            }
            Pat::Slice { prefix, slice, suffix } => {
                if !slice.is_none_or(|it| self.is_rest(it)) {
                    return None;
                }
                let mut inside = Vec::new_in(frame);
                for (i, &element) in prefix.iter().enumerate() {
                    let mut place = place.to_vec_in(frame);
                    place.push(Step::Index(i as u64));
                    self.pairs_of(element, place, &mut inside)?;
                }
                let named = (prefix.len() + suffix.len()) as u64;
                let len = match ty.kind() {
                    TyKind::Array(_, len) => {
                        Some(u64::try_from(crate::consteval::try_const_usize(self.db, len)?).ok()?)
                    }
                    TyKind::Slice(_) => None,
                    _ => return None,
                };
                for (i, &element) in suffix.iter().enumerate() {
                    let mut place = place.to_vec_in(frame);
                    place.push(match len {
                        Some(len) => Step::Index(len.checked_sub(suffix.len() as u64)? + i as u64),
                        None => Step::FromEnd((suffix.len() - 1 - i) as u64),
                    });
                    self.pairs_of(element, place, &mut inside)?;
                }
                // An array has its length, and `[..]` is any slice.
                if len.is_some() || (named == 0 && slice.is_some()) {
                    pairs.extend(inside);
                } else {
                    let ask = Ask::Len { len: named, or_more: slice.is_some() };
                    pairs.push(Pair { place, ask, inside });
                }
            }
            Pat::Path(_) => {
                let variant = self.infer.variant_resolution_for_pat(pattern)?;
                self.pairs_of_variant(variant, Vec::new_in(frame), place, pairs)
            }
            Pat::TupleStruct { args, ellipsis, .. } => {
                let variant = self.infer.variant_resolution_for_pat(pattern)?;
                let fields = match variant {
                    VariantId::EnumVariantId(it) => it.fields(self.db),
                    VariantId::StructId(it) => it.fields(self.db),
                    VariantId::UnionId(_) => return None,
                };
                let mut inside = Vec::new_in(frame);
                let within = variant_place(variant, &place, frame);
                self.pairs_of_fields(args, ellipsis, fields.fields().len(), &within, &mut inside)?;
                self.pairs_of_variant(variant, inside, place, pairs)
            }
            Pat::Record { args, .. } => {
                let variant = self.infer.variant_resolution_for_pat(pattern)?;
                let fields = match variant {
                    VariantId::EnumVariantId(it) => it.fields(self.db),
                    VariantId::StructId(it) => it.fields(self.db),
                    VariantId::UnionId(_) => return None,
                };
                let mut inside = Vec::new_in(frame);
                for field in args.iter() {
                    let mut within = variant_place(variant, &place, frame);
                    within.push(Step::Field(u32::from(fields.field(&field.name)?.into_raw())));
                    self.pairs_of(field.pat, within, &mut inside)?;
                }
                self.pairs_of_variant(variant, inside, place, pairs)
            }
            Pat::Missing | Pat::Rest | Pat::NotNull | Pat::Deref { .. } | Pat::Expr(_) => {
                return None;
            }
        }
        Some(())
    }

    /// Whether the pattern for the middle of a slice asks nothing of it: `..` or `name @ ..`.
    fn is_rest(&self, pattern: PatId) -> bool {
        match self.store.pat(pattern) {
            Pat::Wild | Pat::Rest | Pat::Bind { subpat: None, .. } => true,
            Pat::Bind { subpat: Some(inner), .. } => self.is_rest(inner),
            _ => false,
        }
    }

    /// The patterns of the fields of a tuple or a tuple struct of `arity` fields, where
    /// `ellipsis` says how many of them come before a `..`.
    fn pairs_of_fields<'t>(
        &self,
        args: &[PatId],
        ellipsis: Option<u32>,
        arity: usize,
        place: &[Step],
        pairs: &mut Vec<Pair<'t>, &'t Frame>,
    ) -> Option<()>
    where
        'a: 't,
    {
        let frame = *pairs.allocator();
        let before = ellipsis.map_or(args.len(), |it| it as usize);
        for (i, &arg) in args.iter().enumerate() {
            let field = if i < before { i } else { arity.checked_sub(args.len() - i)? };
            let mut place = place.to_vec_in(frame);
            place.push(Step::Field(field as u32));
            self.pairs_of(arg, place, pairs)?;
        }
        Some(())
    }

    /// A pattern that names a variant: the variant is asked for when the type has another,
    /// and what the fields ask comes after that.
    fn pairs_of_variant<'t>(
        &self,
        variant: VariantId,
        inside: Vec<Pair<'t>, &'t Frame>,
        place: Vec<Step, &'t Frame>,
        pairs: &mut Vec<Pair<'t>, &'t Frame>,
    ) {
        match variant {
            VariantId::EnumVariantId(it)
                if it.lookup(self.db).parent.enum_variants(self.db).variants.len() > 1 =>
            {
                pairs.push(Pair { place, ask: Ask::Variant(it), inside })
            }
            _ => pairs.extend(inside),
        }
    }

    /// What a literal in a pattern of type `ty` asks.
    fn constant_ask<'t>(&self, ty: Ty<'db>, literal: hir_def::hir::ExprId) -> Option<Ask<'t>>
    where
        'a: 't,
    {
        let store = self.store;
        // A negated literal is one `Literal::negate` takes: an integer or a float.
        let (literal, negated) = match store.expr(literal) {
            Expr::Literal(it) => (it, false),
            Expr::UnaryOp { expr, op: UnaryOp::Neg } => match store.expr(expr) {
                Expr::Literal(it @ (Literal::Int(..) | Literal::Float(..))) => (it, true),
                _ => return None,
            },
            _ => return None,
        };
        let signed = matches!(ty.kind(), TyKind::Int(_));
        let ordered = |value: u128| if signed { value ^ (1 << 127) } else { value };
        Some(match *literal {
            Literal::Bool(it) => Ask::Bool(it),
            Literal::Char(it) => Ask::Int(u128::from(it)),
            Literal::Int(it, _) => Ask::Int(ordered((if negated { -it } else { it }) as u128)),
            Literal::Uint(it, _) => Ask::Int(ordered(it)),
            _ => Ask::Equal(literal, negated),
        })
    }
}

/// Where the fields of a variant are, from the place of the value.
fn variant_place<'t>(variant: VariantId, place: &[Step], frame: &'t Frame) -> Vec<Step, &'t Frame> {
    let mut place = place.to_vec_in(frame);
    if let VariantId::EnumVariantId(it) = variant {
        place.push(Step::Variant(it));
    }
    place
}

/// The least and the greatest value of an integer type or `char`, as `Ask::Int` has them,
/// and whether a range of all of them is known to be one: not for `usize` and `isize`.
fn int_bounds(ty: Ty<'_>) -> Option<(u128, u128, bool)> {
    use rustc_type_ir::{IntTy, UintTy};
    let unsigned = |bits: u32| (0, u128::MAX >> (128 - bits));
    let signed = |bits: u32| {
        let half = 1u128 << (bits - 1);
        ((1 << 127) - half, (1 << 127) + (half - 1))
    };
    let ((least, most), bounded) = match ty.kind() {
        TyKind::Char => ((0, u128::from(char::MAX)), true),
        TyKind::Uint(UintTy::Usize) => (unsigned(64), false),
        TyKind::Int(IntTy::Isize) => (signed(64), false),
        TyKind::Uint(it) => (unsigned(it.bit_width()? as u32), true),
        TyKind::Int(it) => (signed(it.bit_width()? as u32), true),
        _ => return None,
    };
    Some((least, most, bounded))
}

#[cfg(test)]
mod tests;
