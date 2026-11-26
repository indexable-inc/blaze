//! The flows of one body for the move and borrow check (front2 borrows.rs `borrows_of`): the
//! body's own, then each closure's and each async block's body as a flow of its own
//! (blaze-flow-lower `lower_closure`, `lower_async`), so a body holding either is checked whole
//! instead of refused (`Form::Closure`, `Form::AsyncBlock`); an `async fn`'s own body is
//! lowered with its suspensions (`lower_async_fn`) instead of refused (`Form::Coroutine`).
//!
//! The Reference, "Closure expressions" and "Capture modes": a closure's captures are borrowed
//! or moved where the closure is made (the owner's flow: each capture a loan or a move at the
//! closure expression, held in the closure value, so live as long as it is), and its body runs
//! with them as its environment (the closure's flow: the captures as arguments of their modes).
//! "Async blocks": "Async blocks capture variables from their environment using the same
//! capture modes as closures"; "Await expressions": the block is suspended at each `.await`
//! and resumed there, so a loan held across one is live through the suspension.
//!
//! The owner's flow is asked with [`NestedFacts`], which says each closure's and async block's
//! body IS checked: true only because `borrows_of` lowers and checks every one of them, in
//! expression order ([`next_nested`]); a refusal of any is the body's refusal. A nested body's
//! outlives result is a relation its owner must prove: between two positions of the closure's
//! or the future's VALUE (or `'static`) it is a requirement the owner's flow emits at the
//! expression ([`Requirements`], closures and async blocks alike); any other is filed as a
//! refusal by name, never as a finding.

use blaze_flow::{Flow, Origin};
use blaze_flow_lower::{
    AdtFacts, CallBounds, CalleeFacts, ClosureFlow, External, Facts, FieldRegion, Need, ParamOrigin, Refusal, Requirement, Shape, lower, lower_async,
    lower_async_fn, lower_closure, lower_closure_as_fn_once, lower_closure_external,
};
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;

use super::borrows::{BorrowLangs, ReadBy};

/// Which flow of a body: its own, or the body of one of its closures or async blocks (by the
/// expression that makes it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowOf {
    Owner,
    Closure(tc::ExprId),
    AsyncBlock(tc::ExprId),
}

impl FlowOf {
    /// Whether an outlives result of this flow is the owner's to prove (a nested body's). The
    /// root block of an `async fn` (`view.root` of a body with a coroutine) is not: its
    /// placeholders are the fn's signature's (blaze-flow-lower coroutine.rs `async_fn_root`),
    /// so its outlives result is the fn's own finding, as a fn body's is.
    pub fn is_nested(self, view: &tc::BodyView<'_>) -> bool {
        match self {
            FlowOf::Owner => false,
            FlowOf::AsyncBlock(e) if view.coroutine.is_some() && e == view.root => false,
            FlowOf::Closure(_) | FlowOf::AsyncBlock(_) => true,
        }
    }
}

/// The body's facts, saying that closure and async block bodies are checked on their own
/// (module doc: only `borrows_of`, which checks each, asks with these).
pub struct NestedFacts<'f, 'a> {
    facts: ReadBy<'f, 'a>,
    /// Each closure's requirements, from its own flow checked first ([`Requirements`]): two
    /// columns sorted by (closure, requirement), `needs[i]` closure `closures[i]`'s.
    closures: &'f [tc::ExprId],
    needs: &'f [Requirement],
}

impl<'f, 'a> NestedFacts<'f, 'a> {
    /// No closure's requirements known: what a caller that checks no closure first asks with.
    pub fn new(facts: ReadBy<'f, 'a>) -> NestedFacts<'f, 'a> {
        NestedFacts { facts, closures: &[], needs: &[] }
    }

    /// With the requirements of every closure checked so far.
    pub fn with_requirements(facts: ReadBy<'f, 'a>, requirements: &'f Requirements<'_>) -> NestedFacts<'f, 'a> {
        NestedFacts { facts, closures: &requirements.closures, needs: &requirements.needs }
    }
}

/// The closure requirements of one body (the NLL RFC, "Closures": the closure's creator must
/// prove the relations among the closure's external regions its body needs; an async block's
/// the same, keyed by the block's expression): each closure's and nested async block's flow is
/// checked BEFORE the flow that makes it (innermost first: [`nested_inner_first`]),
/// and each of its outlives results between two positions of the closure VALUE (or `'static`)
/// is mapped through blaze-flow-lower `requirement` into this table, which the making flow's
/// lowering emits at the closure expression (`Facts::closure_requirements`). A result that maps
/// to none (the environment's reference, the closure's signature) stays `NestedOutlives`.
pub struct Requirements<'a> {
    closures: AVec<'a, tc::ExprId>,
    needs: AVec<'a, Requirement>,
}

impl<'a> Requirements<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Requirements<'a> {
        Requirements { closures: AVec::new_in(alloc), needs: AVec::new_in(alloc) }
    }

    /// Files closure `closure`'s requirement `r` (each once, the columns kept sorted).
    pub fn add(&mut self, closure: tc::ExprId, r: Requirement) {
        let (lo, hi) = run_of(&self.closures, closure);
        let at = match self.needs[lo..hi].binary_search(&r) {
            Ok(_) => return,
            Err(i) => lo + i,
        };
        self.closures.insert(at, closure);
        self.needs.insert(at, r);
    }

    /// Closure `closure`'s requirements, in order.
    pub fn of(&self, closure: tc::ExprId) -> &[Requirement] {
        let (lo, hi) = run_of(&self.closures, closure);
        &self.needs[lo..hi]
    }
}

/// Where closure `closure`'s rows are in a sorted column of closures.
fn run_of(closures: &[tc::ExprId], closure: tc::ExprId) -> (usize, usize) {
    (closures.partition_point(|&e| e < closure), closures.partition_point(|&e| e <= closure))
}

/// Every nested flow of the body whose outlives results are its maker's to prove: each closure
/// (an actual `async ||` is refused by `lower_closure_external`) and each async block but an
/// `async fn`'s root ([`FlowOf::is_nested`]), innermost first: by strict range containment,
/// then by range length and expression. What [`Requirements`] is filled in the order of, so
/// every maker is lowered with the requirements of what it makes (lane B14: async blocks in the
/// one mechanism of closures, rule 30; a closure inside an async block, or an async block inside
/// a closure, either way).
pub fn nested_inner_first<'a>(view: &tc::BodyView<'_>, alloc: Alloc<'a>) -> AVec<'a, FlowOf> {
    // Span length is normally enough, but macro expansion can give two nested expressions
    // overlapping ranges.  The owner must still be lowered after every body it contains, so
    // order by strict containment depth first and use length only as the stable tie-breaker.
    let mut out: AVec<'a, (u32, u32, u32, tc::ExprId)> = AVec::new_in(alloc);
    for (i, x) in view.exprs.iter().enumerate() {
        let e = tc::ExprId(i as u32);
        let nested = match x {
            tc::Expr::Closure { .. } => true,
            tc::Expr::AsyncBlock { .. } => FlowOf::AsyncBlock(e).is_nested(view),
            _ => false,
        };
        if nested {
            let range = view.expr_ranges.get(i).copied().unwrap_or_default();
            out.push((range.start, range.end, range.end.saturating_sub(range.start), e));
        }
    }
    let mut depths: AVec<'a, u32> = AVec::new_in(alloc);
    for i in 0..out.len() {
        let (start, end, _, _) = out[i];
        let depth = out
            .iter()
            .filter(|&&(outer_start, outer_end, _, _)| {
                (outer_start < start || outer_end > end) && outer_start <= start && end <= outer_end
            })
            .count() as u32;
        depths.push(depth);
    }
    for (row, &depth) in out.iter_mut().zip(depths.iter()) {
        row.0 = depth;
    }
    out.sort_unstable_by_key(|&(depth, _, len, e)| (u32::MAX - depth, len, core::cmp::Reverse(e)));
    let mut flows: AVec<'a, FlowOf> = AVec::new_in(alloc);
    for &(_, _, _, e) in out.iter() {
        flows.push(match view.exprs.get(e.ix()) {
            Some(tc::Expr::AsyncBlock { .. }) => FlowOf::AsyncBlock(e),
            _ => FlowOf::Closure(e),
        });
    }
    flows
}

/// A closure's flow with what each of its placeholders stands for in its owner
/// (blaze-flow-lower `lower_closure_external`): its requirements are mapped through these.
pub fn closure_flow_of<'a>(
    view: &tc::BodyView<'_>,
    done: &tc::Checked<'_>,
    closure: tc::ExprId,
    facts: &NestedFacts<'_, '_>,
    alloc: Alloc<'a>,
) -> Result<ClosureFlow<Alloc<'a>>, Refusal> {
    lower_closure_external(view, done, closure, facts, alloc)
}

/// [`closure_flow_of`] lowered as `FnOnce::call_once(self)` when [`classify_closure_env_use`]
/// proves the enclosing consumer only ever invokes the closure once (e.g. `scope.may_overlap`
/// taking `impl FnOnce(&Cx) -> Result<T, E> + Send`), so `ByValue` captures like `bufs_chunk:
/// &mut [u8]` are owned locals instead of borrowed through `External::Env`.
pub fn closure_flow_of_as_fn_once<'a>(
    view: &tc::BodyView<'_>,
    done: &tc::Checked<'_>,
    closure: tc::ExprId,
    facts: &NestedFacts<'_, '_>,
    alloc: Alloc<'a>,
) -> Result<ClosureFlow<Alloc<'a>>, Refusal> {
    lower_closure_as_fn_once(view, done, closure, facts, alloc)
}

impl Facts for NestedFacts<'_, '_> {
    fn adt(&self, adt: tc::ItemKey) -> Option<AdtFacts> {
        self.facts.adt(adt)
    }
    fn callee(&self, item: tc::ItemKey) -> Option<CalleeFacts> {
        self.facts.callee(item)
    }
    // The head records' regions (D148), forwarded: a callee's signature (`Facts::sig`'s one
    // source) and the clause columns a closure's signature is read from (closure.rs
    // `sig_slots`) are the owner's facts. Without it a nested flow read `NoRegions` and every
    // closure a clause expected was refused `ClosureClauseUnread`.
    fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts {
        self.facts.regions()
    }
    fn region_tys(&self) -> Option<tc::PTys<'_>> {
        self.facts.region_tys()
    }
    fn fn_binders(&self) -> bool {
        self.facts.fn_binders()
    }
    fn closure_bodies_checked(&self) -> bool {
        true
    }
    fn async_bodies_checked(&self) -> bool {
        true
    }
    fn is_future(&self, ty: tc::PTy) -> Option<bool> {
        self.facts.is_future(ty)
    }
    fn field(&self, adt: tc::ItemKey, variant: u32, name: tc::Name) -> Option<u32> {
        self.facts.field(adt, variant, name)
    }
    fn field_count(&self, adt: tc::ItemKey, variant: u32) -> Option<u32> {
        self.facts.field_count(adt, variant)
    }
    fn field_param(&self, adt: tc::ItemKey, variant: u32, field: u32) -> Option<u32> {
        self.facts.field_param(adt, variant, field)
    }
    fn field_lead(&self, adt: tc::ItemKey, variant: u32, field: u32) -> Option<blaze_flow_lower::FieldLead> {
        self.facts.field_lead(adt, variant, field)
    }
    fn param_copy(&self, index: u32) -> bool {
        self.facts.param_copy(index)
    }
    fn alias_copy(&self, alias: tc::PTy) -> bool {
        self.facts.alias_copy(alias)
    }
    fn shape(&self, item: tc::ItemKey) -> Option<Shape<'_>> {
        self.facts.shape(item)
    }
    fn call_bounds(&self, item: tc::ItemKey) -> Option<CallBounds<'_>> {
        self.facts.call_bounds(item)
    }
    fn object_trait_lifetimes(&self, trait_: tc::ItemKey) -> Option<bool> {
        self.facts.object_trait_lifetimes(trait_)
    }
    // The one call rule's counts (one_call.rs `FactsArity`, `CallSel::impl_at`), forwarded:
    // every flow the command and the census lower is lowered through these facts, and the
    // trait's default answers `None`. Unforwarded, every alias and trait reference the one
    // rule counted was refused `Count` and every selected impl with an associated value
    // `HeadUnread`, though the gather had filed them all (census wave-1119: 30,061 and 758
    // `Count`, "the read answers: the gather did not file it"; 9,512 `HeadUnread`).
    fn trait_lifetimes(&self, trait_: tc::ItemKey) -> Option<u32> {
        self.facts.trait_lifetimes(trait_)
    }
    fn alias_lifetimes(&self, trait_: tc::ItemKey, name: tc::Name) -> Option<u32> {
        self.facts.alias_lifetimes(trait_, name)
    }
    // Forwarded for the same reason: unforwarded, every `F: [const] Destruct` of a closure
    // argument (`Option::map`) asked a selection typeck files no row for, refused `Unanswered`.
    fn trait_relates_nothing(&self, trait_: tc::ItemKey) -> bool {
        self.facts.trait_relates_nothing(trait_)
    }
    fn is_auto_trait(&self, trait_: tc::ItemKey) -> bool {
        self.facts.is_auto_trait(trait_)
    }
    fn is_try_as_dyn_trait(&self, trait_: tc::ItemKey) -> bool {
        self.facts.is_try_as_dyn_trait(trait_)
    }
    fn is_fn_trait(&self, trait_: tc::ItemKey) -> bool {
        self.facts.is_fn_trait(trait_)
    }
    fn is_future_trait(&self, trait_: tc::ItemKey) -> bool {
        self.facts.is_future_trait(trait_)
    }
    fn is_into_future_trait(&self, trait_: tc::ItemKey) -> bool {
        self.facts.is_into_future_trait(trait_)
    }
    fn assoc_item(&self, trait_: tc::ItemKey, name: tc::Name) -> Option<tc::ItemKey> {
        self.facts.assoc_item(trait_, name)
    }
    fn owner_trait_impl(&self, trait_: tc::ItemKey) -> Option<blaze_flow_lower::OwnerTraitImpl> {
        self.facts.owner_trait_impl(trait_)
    }
    fn deref_impl(&self, adt: tc::ItemKey) -> Option<tc::ItemKey> {
        self.facts.deref_impl(adt)
    }
    fn is_zero_slot_builtin_assoc(&self, trait_: tc::ItemKey, name: tc::Name) -> bool {
        self.facts.is_zero_slot_builtin_assoc(trait_, name)
    }
    fn rpitit_opaque(&self, opaque: tc::ItemKey) -> Option<blaze_flow_lower::RpititOpaque> {
        self.facts.rpitit_opaque(opaque)
    }
    fn impl_fn(&self, imp: tc::ItemKey, fn_name: tc::Name) -> Option<tc::ItemKey> {
        self.facts.impl_fn(imp, fn_name)
    }
    // Every fact the body's facts answer, forwarded (a nested flow asks what the owner's does).
    fn ties(&self, item: tc::ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
        self.facts.ties(item)
    }
    fn adt_variances(&self, adt: tc::ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
        self.facts.adt_variances(adt)
    }
    // The owner's implied bounds and kind, forwarded: the owner's own flow is lowered through
    // these facts too (borrows.rs `flow_of(.., &nested, ..)`). A closure's or async block's
    // flow does not read them (its signature is its own; its temporaries are not the item's).
    fn implied_bounds(&self) -> Option<(&[(u32, tc::regions::Region)], &[(tc::regions::Region, tc::regions::Region)])> {
        self.facts.implied_bounds()
    }
    fn constant_initializer(&self) -> bool {
        self.facts.constant_initializer()
    }
    fn field_regions(&self, adt: tc::ItemKey, variant: u32, field: u32) -> Option<&[FieldRegion]> {
        self.facts.field_regions(adt, variant, field)
    }
    fn copy_args(&self, adt: tc::ItemKey) -> Option<&[bool]> {
        self.facts.copy_args(adt)
    }
    fn drop_args(&self, adt: tc::ItemKey) -> Option<&[bool]> {
        self.facts.drop_args(adt)
    }
    fn inst_needs_drop(&self, ty: tc::PTy) -> Option<bool> {
        self.facts.inst_needs_drop(ty)
    }
    fn drop_strict(&self, adt: tc::ItemKey) -> Option<&[bool]> {
        self.facts.drop_strict(adt)
    }
    fn drop_lifetimes(&self, adt: tc::ItemKey) -> Option<&[bool]> {
        self.facts.drop_lifetimes(adt)
    }
    fn closure_requirements(&self, closure: tc::ExprId) -> &[Requirement] {
        let (lo, hi) = run_of(self.closures, closure);
        self.needs.get(lo..hi).unwrap_or(&[])
    }
}

/// The item-relative range of the closure or async block `of` is the flow of, when it is a
/// nested one (`FlowOf::is_nested`); `None` for the owner's flow: where a refusal of that flow
/// is (borrows.rs `BorrowVerdict::within`).
pub fn nested_range(view: &tc::BodyView<'_>, of: FlowOf) -> Option<(u32, u32)> {
    match of {
        FlowOf::Closure(e) | FlowOf::AsyncBlock(e) if of.is_nested(view) => view.expr_ranges.get(e.ix()).map(|r| (r.start, r.end)),
        _ => None,
    }
}

/// The flow after `at` in the body's order: the owner's first, then each closure and async block
/// in expression order. `None` after the last. One pass over the expressions in all.
pub fn next_nested(view: &tc::BodyView<'_>, at: FlowOf) -> Option<FlowOf> {
    let from = match at {
        FlowOf::Owner => 0,
        FlowOf::Closure(e) | FlowOf::AsyncBlock(e) => e.ix() + 1,
    };
    for i in from..view.exprs.len() {
        let e = tc::ExprId(i as u32);
        match view.exprs[i] {
            tc::Expr::Closure { .. } => return Some(FlowOf::Closure(e)),
            tc::Expr::AsyncBlock { .. } => return Some(FlowOf::AsyncBlock(e)),
            _ => {}
        }
    }
    None
}

/// Lowers flow `of` of the body: the owner's (an `async fn`'s with its suspensions), a closure's
/// or an async block's.
pub fn flow_of<'a>(
    of: FlowOf,
    view: &tc::BodyView<'_>,
    done: &tc::Checked<'_>,
    facts: &NestedFacts<'_, '_>,
    alloc: Alloc<'a>,
) -> Result<(Flow<Alloc<'a>>, AVec<'a, ParamOrigin>, Option<Refusal>), Refusal> {
    // With each flow, its placeholders for the body's type parameters (blaze-flow-lower
    // param_outlives.rs): an `UnprovenOutlives` from one is E0310 or a refusal by name. An async
    // block's lowering makes none. And the first stricter relation it used
    // (blaze-flow-lower `Lowered::coarse`): the body is refused by it only on a finding.
    match of {
        FlowOf::Owner if view.coroutine.is_some() => lower_async_fn(view, done, facts, alloc).map(|l| (l.flow, l.params, l.coarse)),
        FlowOf::Owner => lower(view, done, facts, alloc).map(|l| (l.flow, l.params, l.coarse)),
        FlowOf::Closure(e) => lower_closure(view, done, e, facts, alloc).map(|l| (l.flow, l.params, l.coarse)),
        FlowOf::AsyncBlock(e) => lower_async(view, done, e, facts, alloc).map(|l| (l.flow, AVec::new_in(alloc), l.coarse)),
    }
}

/// How a closure expression is consumed in its enclosing body, for deciding whether a
/// `ByMutRef` capture's per-call environment reference (`External::Env`) is actually a
/// `FnOnce`-consumed capture position (`External::Capture(pos)`) or a genuinely non-escaping
/// `Fn`/`FnMut` environment borrow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosureEnvUse {
    /// Proven consumed at most once via `FnOnce` (never `Fn` or `FnMut`): passed directly as a
    /// by-value argument to a callee whose clauses on that parameter require `FnOnce` and no
    /// `Fn`/`FnMut`, owned-coerced to `dyn FnOnce`, or invoked immediately as an rvalue call.
    FnOnceOnly,
    /// Proven used where `Fn` or `FnMut` is required: a mutable reference reborrowed through
    /// `&'env mut self` cannot escape into an owner region (rustc: captured variable cannot
    /// escape `FnMut` closure body).
    FnOrFnMut,
    /// Usage could not be classified with certainty: stay conservative (`NestedOutlives`).
    Unknown,
}

fn unwrap_parens(view: &tc::BodyView<'_>, mut cur: tc::ExprId) -> tc::ExprId {
    for _ in 0..16 {
        match view.exprs.get(cur.ix()) {
            Some(&tc::Expr::Paren(inner)) => cur = inner,
            _ => break,
        }
    }
    cur
}

fn adjust_range(done: &tc::Checked<'_>, e: tc::ExprId) -> (usize, usize) {
    let col = done.adjusts.column::<0>();
    let lo = col.partition_point(|x| *x < e);
    let hi = col.partition_point(|x| *x <= e);
    (lo, hi)
}

fn unadjusted_to_closure(view: &tc::BodyView<'_>, done: &tc::Checked<'_>, mut cur: tc::ExprId, closure: tc::ExprId) -> bool {
    for _ in 0..16 {
        let (lo, hi) = adjust_range(done, cur);
        if lo != hi {
            return false;
        }
        if cur == closure {
            return true;
        }
        match view.exprs.get(cur.ix()) {
            Some(&tc::Expr::Paren(inner)) => cur = inner,
            _ => return false,
        }
    }
    false
}

fn arg_contains_closure(view: &tc::BodyView<'_>, done: &tc::Checked<'_>, args: tc::List, closure: tc::ExprId) -> bool {
    let Some(es) = view.expr_lists.get(args.range()) else {
        return false;
    };
    es.iter().any(|&a| unadjusted_to_closure(view, done, a, closure))
}

fn peel_to_dyn(tys: tc::PTys<'_>, mut ty: tc::PTy, facts: &ReadBy<'_, '_>) -> Option<tc::PTy> {
    for _ in 0..8 {
        match tys.node(ty).ok()? {
            tc::PNode::Dynamic(_) => return Some(ty),
            tc::PNode::Ref(_, inner) | tc::PNode::Ptr(_, inner) => ty = inner,
            tc::PNode::Adt(k, args) if facts.adt(k).is_some_and(|a| a.is_box) => {
                ty = *tys.list(args).ok()?.first()?;
            }
            _ => return None,
        }
    }
    None
}

/// Classifies how `closure` is used in its enclosing body (`view`/`done`), reading the callee's
/// clauses from `store` so a `ByMutRef` capture of an inline `FnOnce`-only closure (such as
/// `peer.and_then(|peer| self.ends.get_mut(peer))`) can soundly promote its `External::Env`
/// origin to its owner capture position (`External::Capture(pos)`), while an `Fn`/`FnMut`
/// closure stays non-escaping (E0521).
pub fn classify_closure_env_use<'a>(
    closure: tc::ExprId,
    view: &tc::BodyView<'_>,
    done: &tc::Checked<'_>,
    store: &super::heads::Store<'_>,
    langs: &BorrowLangs,
    facts: &ReadBy<'_, '_>,
    alloc: Alloc<'a>,
) -> ClosureEnvUse {
    let Some(row) = done.closures.column::<0>().iter().position(|&c| c == closure) else {
        return ClosureEnvUse::Unknown;
    };
    let Some(&sig_from) = done.closures.column::<4>().get(row) else {
        return ClosureEnvUse::Unknown;
    };
    let is_fn_or_mut = |tr: tc::ItemKey| Some(tr) == langs.fn_traits[0] || Some(tr) == langs.fn_traits[1];
    let is_fn_once = |tr: tc::ItemKey| Some(tr) == langs.fn_traits[2];
    match sig_from {
        tc::SigFrom::Clause { expr, nth, .. } => {
            let call_expr = match view.exprs.get(expr.ix()) {
                Some(tc::Expr::MethodCall { args, .. } | tc::Expr::Call { args, .. })
                    if arg_contains_closure(view, done, *args, closure) =>
                {
                    Some(expr)
                }
                _ => view.exprs.iter().enumerate().find_map(|(ix, x)| match *x {
                    tc::Expr::Call { callee, args }
                        if unwrap_parens(view, callee) == expr && arg_contains_closure(view, done, args, closure) =>
                    {
                        Some(tc::ExprId(ix as u32))
                    }
                    _ => None,
                }),
            };
            let Some(call_expr) = call_expr else {
                return ClosureEnvUse::Unknown;
            };
            let callee_of = |e: tc::ExprId| {
                done.callees
                    .column::<0>()
                    .iter()
                    .position(|&x| x == e)
                    .and_then(|pos| done.callees.column::<1>().get(pos).copied())
            };
            let def_fn_of = |e: tc::ExprId| {
                done.expr_defs
                    .column::<0>()
                    .iter()
                    .position(|&x| x == e)
                    .and_then(|pos| match done.expr_defs.column::<1>().get(pos).copied() {
                        Some(tc::Def::Fn(k)) => Some(k),
                        _ => None,
                    })
            };
            let Some(callee_key) = callee_of(expr).or_else(|| callee_of(call_expr)).or_else(|| def_fn_of(expr)) else {
                return ClosureEnvUse::Unknown;
            };
            let Some(preds) = store.preds.get(&callee_key) else {
                return ClosureEnvUse::Unknown;
            };
            let tys = store.tys.view();
            let Some(&tc::PClause::Trait { trait_, args }) = preds.clauses.get(nth as usize) else {
                return ClosureEnvUse::Unknown;
            };
            if is_fn_or_mut(trait_) {
                return ClosureEnvUse::FnOrFnMut;
            }
            if !is_fn_once(trait_) {
                return ClosureEnvUse::Unknown;
            }
            let Ok(list) = tys.list(args) else {
                return ClosureEnvUse::Unknown;
            };
            let Some(&first) = list.first() else {
                return ClosureEnvUse::Unknown;
            };
            let Ok(tc::PNode::Param(param_idx)) = tys.node(first) else {
                return ClosureEnvUse::Unknown;
            };
            // Verify EVERY clause on `Param(param_idx)` in `callee_key` (including inherited
            // impl/trait clauses): none may require `Fn` or `FnMut`, directly or via supertraits.
            let mut visited: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
            let mut stack: AVec<'a, tc::ItemKey> = AVec::new_in(alloc);
            for clause in preds.clauses.iter() {
                let tc::PClause::Trait { trait_: other_trait, args: other_args } = *clause else {
                    continue;
                };
                let Ok(olist) = tys.list(other_args) else {
                    return ClosureEnvUse::Unknown;
                };
                let Some(&ofirst) = olist.first() else {
                    continue;
                };
                if !matches!(tys.node(ofirst), Ok(tc::PNode::Param(p)) if p == param_idx) {
                    continue;
                }
                if is_fn_or_mut(other_trait) {
                    return ClosureEnvUse::FnOrFnMut;
                }
                if is_fn_once(other_trait) || facts.trait_relates_nothing(other_trait) || facts.is_auto_trait(other_trait) {
                    continue;
                }
                visited.clear();
                stack.clear();
                stack.push(other_trait);
                while let Some(t) = stack.pop() {
                    if is_fn_or_mut(t) {
                        return ClosureEnvUse::FnOrFnMut;
                    }
                    if is_fn_once(t) || facts.trait_relates_nothing(t) || facts.is_auto_trait(t) {
                        continue;
                    }
                    if visited.contains(&t) {
                        continue;
                    }
                    if visited.len() >= 64 {
                        return ClosureEnvUse::Unknown;
                    }
                    visited.push(t);
                    let Some(def) = store.trait_defs.get(&t) else {
                        return ClosureEnvUse::Unknown;
                    };
                    for sc in def.supertraits.iter() {
                        if let tc::PClause::Trait { trait_: sup, args: sargs } = *sc
                            && tys.list(sargs).ok().and_then(|l| l.first().copied()).is_some_and(|f| matches!(tys.node(f), Ok(tc::PNode::Param(0))))
                        {
                            stack.push(sup);
                        }
                    }
                }
            }
            ClosureEnvUse::FnOnceOnly
        }
        tc::SigFrom::Object => {
            let mut cur = closure;
            let mut behind_ref = false;
            let tys = done.types.view();
            for _ in 0..8 {
                let (lo, hi) = adjust_range(done, cur);
                let adj_ty = if hi > lo {
                    done.adjusts.column::<2>().get(hi - 1).copied()
                } else {
                    done.expr_ty.get(cur.ix()).copied()
                };
                if let Some(ty) = adj_ty
                    && let Some(dyn_ty) = peel_to_dyn(tys, ty, facts)
                    && let Ok(tc::PNode::Dynamic(preds_list)) = tys.node(dyn_ty)
                    && let Ok(preds) = tys.list(preds_list)
                {
                    for &p in preds {
                        if let Ok(tc::PNode::ExTrait(tr, _)) = tys.node(p) {
                            if is_fn_or_mut(tr) {
                                return ClosureEnvUse::FnOrFnMut;
                            }
                            if is_fn_once(tr) && !behind_ref {
                                return ClosureEnvUse::FnOnceOnly;
                            }
                        }
                    }
                    return ClosureEnvUse::Unknown;
                }
                let parent = view.exprs.iter().enumerate().find_map(|(ix, expr)| {
                    let pe = tc::ExprId(ix as u32);
                    match *expr {
                        tc::Expr::Paren(inner) | tc::Expr::Cast { operand: inner, .. } if inner == cur => Some((pe, false)),
                        tc::Expr::Ref { operand: inner, .. } if inner == cur => Some((pe, true)),
                        tc::Expr::Call { args, .. } => {
                            let es = view.expr_lists.get(args.range())?;
                            if es.len() == 1
                                && es[0] == cur
                                && let Some(&pty) = done.expr_ty.get(pe.ix())
                                && let Ok(tc::PNode::Adt(k, _)) = tys.node(pty)
                                && facts.adt(k).is_some_and(|a| a.is_box)
                            {
                                Some((pe, false))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                });
                let Some((pe, is_ref)) = parent else {
                    break;
                };
                behind_ref |= is_ref;
                cur = pe;
            }
            ClosureEnvUse::Unknown
        }
        tc::SigFrom::Written | tc::SigFrom::Inferred => {
            let called_immediately = view
                .exprs
                .iter()
                .any(|expr| matches!(*expr, tc::Expr::Call { callee, .. } if unadjusted_to_closure(view, done, callee, closure)));
            if called_immediately { ClosureEnvUse::FnOnceOnly } else { ClosureEnvUse::Unknown }
        }
        tc::SigFrom::FnPtr => ClosureEnvUse::Unknown,
    }
}

/// Core alignment step shared by [`prepare_closure_env`] and its unit tests: given the ordered
/// capture modes `env_caps` of every capture that `closure.rs` `field_through` marked `env = true`
/// (`ByValue` or `ByMutRef` in a non-`FnOnce` closure), validates the capture prefix of
/// `externals`, promotes `ByMutRef` `External::Env` origins to `External::Capture(pos)` when
/// `usage == ClosureEnvUse::FnOnceOnly`, and records non-escaping `External::Env` origins (every
/// `ByValue` capture, plus `ByMutRef` captures under `ClosureEnvUse::FnOrFnMut`) into
/// `forbid_env_escape`. Returns `false` (leaving both outputs unchanged) if the prefix does not
/// align.
pub(crate) fn apply_closure_env<'a>(
    env_caps: &[tc::CaptureKind],
    usage: ClosureEnvUse,
    externals: &mut [(Origin, External)],
    forbid_env_escape: &mut AVec<'a, Origin>,
    alloc: Alloc<'a>,
) -> bool {
    if env_caps.is_empty() {
        return true;
    }
    let mut env_idx = 0usize;
    let mut next_cap = 0u32;
    let mut promotions: AVec<'a, (usize, u32)> = AVec::new_in(alloc);
    let mut forbidden: AVec<'a, Origin> = AVec::new_in(alloc);
    for (idx, &(orig, ext)) in externals.iter().enumerate() {
        match ext {
            External::Capture(pos) => {
                if pos != next_cap {
                    return false;
                }
                let Some(next) = next_cap.checked_add(1) else {
                    return false;
                };
                next_cap = next;
            }
            External::Env => {
                let Some(&cap) = env_caps.get(env_idx) else {
                    return false;
                };
                env_idx += 1;
                match cap {
                    tc::CaptureKind::ByMutRef => {
                        let pos = next_cap;
                        let Some(next) = next_cap.checked_add(1) else {
                            return false;
                        };
                        next_cap = next;
                        match usage {
                            ClosureEnvUse::FnOnceOnly => promotions.push((idx, pos)),
                            ClosureEnvUse::FnOrFnMut => forbidden.push(orig),
                            ClosureEnvUse::Unknown => {}
                        }
                    }
                    tc::CaptureKind::ByValue => {
                        forbidden.push(orig);
                    }
                    tc::CaptureKind::ByRef => return false,
                }
            }
            _ => break,
        }
    }
    if env_idx != env_caps.len() {
        return false;
    }
    for &(idx, pos) in promotions.iter() {
        externals[idx].1 = External::Capture(pos);
    }
    for &orig in forbidden.iter() {
        forbid_env_escape.push(orig);
    }
    true
}

/// Rewrites `ByMutRef` `External::Env` entries in `externals` to `External::Capture(pos)` when
/// `closure` is proven `FnOnceOnly`, and records truly non-escaping `External::Env` origins
/// (`ByValue` captures, or `ByMutRef` captures of `FnOrFnMut` closures) into `forbid_env_escape`.
pub fn prepare_closure_env<'a>(
    closure: tc::ExprId,
    done: &tc::Checked<'_>,
    usage: ClosureEnvUse,
    externals: &mut [(Origin, External)],
    forbid_env_escape: &mut AVec<'a, Origin>,
    alloc: Alloc<'a>,
) {
    let Some(row) = done.closures.column::<0>().iter().position(|&c| c == closure) else {
        return;
    };
    let Some(&kind) = done.closures.column::<1>().get(row) else {
        return;
    };
    if kind == tc::ClosureKind::FnOnce {
        return;
    }
    let mut env_caps: AVec<'a, tc::CaptureKind> = AVec::new_in(alloc);
    for f in done.env_fields_by::<tc::Places>(closure) {
        if matches!(f.how, tc::CaptureKind::ByValue | tc::CaptureKind::ByMutRef) {
            env_caps.push(f.how);
        }
    }
    let _ = apply_closure_env(&env_caps, usage, externals, forbid_env_escape, alloc);
}

/// Whether an outlives result `from: into` of a closure's flow is an illegal escape of a
/// per-call environment reference (`External::Env` -> an owner region): either a reference
/// borrowed from a `ByValue` capture owned by the closure (dangling even for `FnOnce`), or a
/// `&mut` capture reborrowed through `&'env mut self` in a closure used as `Fn`/`FnMut`
/// (rustc: "captured variable cannot escape `FnMut` closure body").
pub fn env_escapes_closure<'a>(
    externals: &[(Origin, External)],
    known: &[(Origin, Origin)],
    forbid_env_escape: &[Origin],
    from: Origin,
    into: Origin,
    alloc: Alloc<'a>,
) -> bool {
    let end = |o: Origin| externals.iter().find(|(x, _)| *x == o).map(|&(_, w)| w);
    let owner_names = |w: Option<External>| {
        matches!(
            w,
            Some(
                External::Capture(_)
                    | External::SigOwner(_)
                    | External::Static
                    | External::OwnerParam(_)
                    | External::Param(_)
                    | External::DynPlaceholder(_)
            )
        )
    };
    if !forbid_env_escape.contains(&from) || end(from) != Some(External::Env) || !owner_names(end(into)) {
        return false;
    }
    let mut reached: AVec<'a, Origin> = AVec::new_in(alloc);
    reached.push(from);
    let mut i = 0usize;
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

fn owner_need_of(ext: External) -> Option<Need> {
    match ext {
        External::Capture(j) => Some(Need::Capture(j)),
        External::Static => Some(Need::Static),
        External::SigOwner(k) => Some(Need::Sig(k)),
        External::OwnerParam(i) => Some(Need::OwnerParam(i)),
        External::Param(u) => Some(Need::Param(u)),
        External::DynPlaceholder(ord) => Some(Need::DynPlaceholder(ord)),
        External::Env | External::Sig | External::Late => None,
    }
}

/// Whether `start` reaches `target` in the directed `known` outlives graph (`a: b` as `a -> b`).
fn reaches_in<'a>(known: &[(Origin, Origin)], start: Origin, target: Origin, alloc: Alloc<'a>) -> bool {
    if start == target {
        return true;
    }
    let mut visited: AVec<'a, Origin> = AVec::new_in(alloc);
    visited.push(start);
    let mut i = 0usize;
    while i < visited.len() {
        let cur = visited[i];
        i += 1;
        for &(a, b) in known {
            if a == cur {
                if b == target {
                    return true;
                }
                if !visited.contains(&b) {
                    visited.push(b);
                }
            }
        }
    }
    false
}

/// Computes the tightest (minimal) owner-side upper bound `U` of late-bound region `late` in
/// `known` (`U: late`, i.e., `U ->* late`):
///
/// - Collects every non-`'static` owner-side origin `U` that reaches `late` in `known` (reverse
///   BFS from `late` along `(a, b)` edges `b -> a`).
/// - If none exists, `'static` is the unique non-local upper bound (`∀ 'a. R: 'a ⇔ R: 'static`):
///   returns `Some(Need::Static)`.
/// - Otherwise keeps only minimal candidates (no non-equivalent candidate `V` with `U ->* V`,
///   meaning `V` is strictly smaller than `U`). If every minimal candidate belongs to one
///   equivalence class (same `Need` or mutually reachable in `known`), returns its `Need`. If
///   two incomparable minimal upper bounds exist, returns `None` (conservative).
fn non_local_upper_bound<'a>(
    externals: &[(Origin, External)],
    known: &[(Origin, Origin)],
    late: Origin,
    alloc: Alloc<'a>,
) -> Option<Need> {
    let end = |o: Origin| externals.iter().find(|(x, _)| *x == o).map(|&(_, w)| w);
    let mut reached: AVec<'a, Origin> = AVec::new_in(alloc);
    reached.push(late);
    let mut i = 0usize;
    while i < reached.len() {
        let cur = reached[i];
        i += 1;
        for &(a, b) in known {
            if b == cur && !reached.contains(&a) {
                reached.push(a);
            }
        }
    }
    let mut candidates: AVec<'a, (Origin, Need)> = AVec::new_in(alloc);
    for &orig in reached.iter() {
        if let Some(ext) = end(orig)
            && let Some(need) = owner_need_of(ext)
            && need != Need::Static
        {
            candidates.push((orig, need));
        }
    }
    if candidates.is_empty() {
        return Some(Need::Static);
    }
    let mut minimal: AVec<'a, (Origin, Need)> = AVec::new_in(alloc);
    for &(u, u_need) in candidates.iter() {
        let dominated = candidates.iter().any(|&(v, _)| {
            v != u && reaches_in(known, u, v, alloc) && !reaches_in(known, v, u, alloc)
        });
        if !dominated {
            minimal.push((u, u_need));
        }
    }
    let &(first_orig, first_need) = minimal.first()?;
    let unique = minimal.iter().all(|&(m_orig, m_need)| {
        m_need == first_need
            || (reaches_in(known, first_orig, m_orig, alloc) && reaches_in(known, m_orig, first_orig, alloc))
    });
    unique.then_some(first_need)
}

/// Computes the loosest (maximal) owner-side lower bound `L` of late-bound region `late` in
/// `known` (`late: L`, i.e., `late ->* L`):
///
/// - Collects every owner-side origin `L` reachable from `late` in `known` (forward BFS).
/// - If none exists, returns `None` (no owner-side proof can establish `late: into`; for an
///   owner-side `into`, `escapes_closure` reports E0521).
/// - If `Need::Static` is reachable (`late: 'static`), `'static` is maximal: returns `Some(Need::Static)`.
/// - Otherwise keeps only maximal candidates (no non-equivalent candidate `V` with `V ->* l`,
///   meaning `V` is strictly larger than `l`). Returns the unique maximal equivalence class's
///   `Need`, or `None` if two incomparable maximal lower bounds exist.
fn non_local_lower_bound<'a>(
    externals: &[(Origin, External)],
    known: &[(Origin, Origin)],
    late: Origin,
    alloc: Alloc<'a>,
) -> Option<Need> {
    let end = |o: Origin| externals.iter().find(|(x, _)| *x == o).map(|&(_, w)| w);
    let mut reached: AVec<'a, Origin> = AVec::new_in(alloc);
    reached.push(late);
    let mut i = 0usize;
    while i < reached.len() {
        let cur = reached[i];
        i += 1;
        for &(a, b) in known {
            if a == cur && !reached.contains(&b) {
                reached.push(b);
            }
        }
    }
    let mut candidates: AVec<'a, (Origin, Need)> = AVec::new_in(alloc);
    for &orig in reached.iter() {
        if let Some(ext) = end(orig)
            && let Some(need) = owner_need_of(ext)
        {
            if need == Need::Static {
                return Some(Need::Static);
            }
            candidates.push((orig, need));
        }
    }
    if candidates.is_empty() {
        return None;
    }
    let mut maximal: AVec<'a, (Origin, Need)> = AVec::new_in(alloc);
    for &(l, l_need) in candidates.iter() {
        let dominated = candidates.iter().any(|&(v, _)| {
            v != l && reaches_in(known, v, l, alloc) && !reaches_in(known, l, v, alloc)
        });
        if !dominated {
            maximal.push((l, l_need));
        }
    }
    let &(first_orig, first_need) = maximal.first()?;
    let unique = maximal.iter().all(|&(m_orig, m_need)| {
        m_need == first_need
            || (reaches_in(known, first_orig, m_orig, alloc) && reaches_in(known, m_orig, first_orig, alloc))
    });
    unique.then_some(first_need)
}

/// Maps a nested flow's unproven outlives constraint `from: into` to an owner-side
/// [`Requirement`], extending `blaze_flow_lower::requirement` with NLL closure region
/// propagation through late-bound (`External::Late`) regions:
///
/// - Owner -> Owner: direct `Requirement` (identical to `blaze_flow_lower::requirement`).
/// - Owner -> Late: propagates `into` upward to its tightest owner-side upper bound `U` in
///   `known` (`U: into`, e.g. `'env: 'scope` from `&'scope Scope<'scope, 'env>` in
///   `std::thread::scope`), or `'static` when no non-`'static` upper bound exists
///   (`∀ 'a. R: 'a ⇔ R: 'static`). Sound because `from: U` and `U: into` imply `from: into`.
/// - Late -> Owner / Late -> Late: propagates `from` downward to its loosest owner-side lower
///   bound `L` in `known` (`from: L`), returning `None` when none exists (so `escapes_closure`
///   reports E0521 for `Late -> Owner`, and unpropagatable `Late -> Late` stays `NestedOutlives`).
pub fn propagate_nested_requirement<'a>(
    externals: &[(Origin, External)],
    known: &[(Origin, Origin)],
    from: Origin,
    into: Origin,
    alloc: Alloc<'a>,
) -> Option<Requirement> {
    if let Some(direct) = blaze_flow_lower::requirement(externals, from, into) {
        return Some(direct);
    }
    let end = |o: Origin| externals.iter().find(|(x, _)| *x == o).map(|&(_, w)| w);
    let from_need = match end(from)? {
        External::Late => non_local_lower_bound(externals, known, from, alloc)?,
        ext => owner_need_of(ext)?,
    };
    let into_need = match end(into)? {
        External::Late => non_local_upper_bound(externals, known, into, alloc)?,
        ext => owner_need_of(ext)?,
    };
    Some(Requirement { from: from_need, into: into_need })
}

#[cfg(test)]
mod env_tests {
    use super::*;

    /// Mixed captures (`ByRef`, `ByValue` with 0 and 1 inner regions, `ByMutRef` with 0 and 1
    /// inner regions): under `FnOnceOnly`, ONLY the `ByMutRef` `Env` origins promote to their
    /// exact closure-value capture slots (`Capture(3)` and `Capture(4)`), while `ByValue` `Env`
    /// origins stay `External::Env` and are recorded in `forbid_env_escape`.
    /// Under `FnOrFnMut`, NO `Env` promotes and ALL `Env` origins are recorded in
    /// `forbid_env_escape`. Under `Unknown`, `ByMutRef` stays conservative (not promoted, not
    /// forbidden) while `ByValue` is still forbidden.
    #[test]
    fn apply_closure_env_promotes_only_fnonce_by_mut_ref_and_forbids_by_value_and_fn_mut() {
        let alloc = Alloc::global();
        let env_caps = [
            tc::CaptureKind::ByValue,  // o2: 0 inner regions -> no value slot
            tc::CaptureKind::ByValue,  // o3: 1 inner region (o4 = Capture(2)) -> no value slot
            tc::CaptureKind::ByMutRef, // o5: 0 inner regions -> value slot Capture(3)
            tc::CaptureKind::ByMutRef, // o6: 1 inner region (o7 = Capture(5)) -> value slot Capture(4)
        ];
        let base = [
            (Origin(0), External::Capture(0)),
            (Origin(1), External::Capture(1)),
            (Origin(2), External::Env),
            (Origin(3), External::Env),
            (Origin(4), External::Capture(2)),
            (Origin(5), External::Env),
            (Origin(6), External::Env),
            (Origin(7), External::Capture(5)),
            (Origin(8), External::SigOwner(0)),
        ];

        // FnOnceOnly: o5 -> Capture(3), o6 -> Capture(4); o2 and o3 stay Env and are forbidden.
        let mut ext = base;
        let mut forbid = AVec::new_in(alloc);
        assert!(apply_closure_env(&env_caps, ClosureEnvUse::FnOnceOnly, &mut ext, &mut forbid, alloc));
        assert_eq!(ext[2].1, External::Env);
        assert_eq!(ext[3].1, External::Env);
        assert_eq!(ext[5].1, External::Capture(3));
        assert_eq!(ext[6].1, External::Capture(4));
        assert_eq!(forbid.as_slice(), &[Origin(2), Origin(3)]);
        assert!(env_escapes_closure(&ext, &[], &forbid, Origin(2), Origin(8), alloc));
        assert!(!env_escapes_closure(&ext, &[], &forbid, Origin(5), Origin(8), alloc));

        // FnOrFnMut: nothing promoted; all 4 Env origins forbidden.
        let mut ext = base;
        let mut forbid = AVec::new_in(alloc);
        assert!(apply_closure_env(&env_caps, ClosureEnvUse::FnOrFnMut, &mut ext, &mut forbid, alloc));
        assert_eq!(ext[5].1, External::Env);
        assert_eq!(ext[6].1, External::Env);
        assert_eq!(forbid.as_slice(), &[Origin(2), Origin(3), Origin(5), Origin(6)]);
        assert!(env_escapes_closure(&ext, &[], &forbid, Origin(5), Origin(8), alloc));

        // Unknown: ByMutRef stays unpromoted and unforbidden (conservative NestedOutlives); ByValue still forbidden.
        let mut ext = base;
        let mut forbid = AVec::new_in(alloc);
        assert!(apply_closure_env(&env_caps, ClosureEnvUse::Unknown, &mut ext, &mut forbid, alloc));
        assert_eq!(ext[5].1, External::Env);
        assert_eq!(forbid.as_slice(), &[Origin(2), Origin(3)]);
        assert!(!env_escapes_closure(&ext, &[], &forbid, Origin(5), Origin(8), alloc));
        assert!(env_escapes_closure(&ext, &[], &forbid, Origin(2), Origin(8), alloc));

        // Misaligned prefix fails atomically with no mutation.
        let mut bad = base;
        bad[7].1 = External::Capture(99);
        let mut forbid = AVec::new_in(alloc);
        assert!(!apply_closure_env(&env_caps, ClosureEnvUse::FnOnceOnly, &mut bad, &mut forbid, alloc));
        assert_eq!(bad[5].1, External::Env);
        assert!(forbid.is_empty());
    }

    #[test]
    fn propagate_nested_requirement_handles_late_upper_and_lower_bounds_soundly() {
        let alloc = Alloc::global();
        let ext = [
            (Origin(0), External::Capture(0)),
            (Origin(1), External::Param(0)),
            (Origin(2), External::SigOwner(0)), // 'env
            (Origin(3), External::Late),        // 'scope
            (Origin(4), External::Late),        // 'other_late (unbounded)
            (Origin(5), External::OwnerParam(0)),
            (Origin(6), External::Env),
            (Origin(7), External::Static),
        ];

        // 1. std::thread::scope shape: `'env: 'scope` in `known`.
        // Capture(0): 'scope -> Capture(0): 'env; Param(0): 'scope -> Param(0): 'env.
        let scope_known = [(Origin(2), Origin(3))];
        assert_eq!(
            propagate_nested_requirement(&ext, &scope_known, Origin(0), Origin(3), alloc),
            Some(Requirement { from: Need::Capture(0), into: Need::Sig(0) }),
        );
        assert_eq!(
            propagate_nested_requirement(&ext, &scope_known, Origin(1), Origin(3), alloc),
            Some(Requirement { from: Need::Param(0), into: Need::Sig(0) }),
        );

        // 2. Domination: `'owner: 'env: 'scope` selects minimal `'env`, not `'owner` or `'static`.
        let dom_known = [(Origin(7), Origin(5)), (Origin(5), Origin(2)), (Origin(2), Origin(3))];
        assert_eq!(
            propagate_nested_requirement(&ext, &dom_known, Origin(0), Origin(3), alloc),
            Some(Requirement { from: Need::Capture(0), into: Need::Sig(0) }),
        );

        // 3. Unbounded late region (`for<'a>` with no owner upper bound):
        // `Capture(0): 'a` for all `'a` requires `Capture(0): 'static` (never silently dropped!).
        assert_eq!(
            propagate_nested_requirement(&ext, &scope_known, Origin(0), Origin(4), alloc),
            Some(Requirement { from: Need::Capture(0), into: Need::Static }),
        );
        assert_eq!(
            propagate_nested_requirement(&ext, &scope_known, Origin(1), Origin(4), alloc),
            Some(Requirement { from: Need::Param(0), into: Need::Static }),
        );

        // 4. Ambiguous incomparable upper bounds (`'env: 'scope` AND `'owner: 'scope`, unrelated):
        // stays `None` (conservative `NestedOutlives`).
        let ambig_known = [(Origin(2), Origin(3)), (Origin(5), Origin(3))];
        assert_eq!(propagate_nested_requirement(&ext, &ambig_known, Origin(0), Origin(3), alloc), None);

        // 5. NEGATIVE: `Late -> Owner` with no lower bound returns `None` (so `escapes_closure` emits E0521).
        assert_eq!(propagate_nested_requirement(&ext, &scope_known, Origin(3), Origin(0), alloc), None);
        assert!(blaze_flow_lower::escapes_closure(&ext, &scope_known, Origin(3), Origin(0), alloc));

        // 6. NEGATIVE: `Late -> Late` with no lower bound on `from` returns `None` (never accepted).
        assert_eq!(propagate_nested_requirement(&ext, &scope_known, Origin(4), Origin(3), alloc), None);
        assert_eq!(propagate_nested_requirement(&ext, &scope_known, Origin(3), Origin(4), alloc), None);

        // 7. NEGATIVE: `Env -> Late` is never promoted when `Env` is not an owner capture.
        assert_eq!(propagate_nested_requirement(&ext, &scope_known, Origin(6), Origin(3), alloc), None);

        // 8. Symmetric lower bound: `'late: 'env` allows `'late: Capture(0)` -> `'env: Capture(0)`.
        let lower_known = [(Origin(4), Origin(2))];
        assert_eq!(
            propagate_nested_requirement(&ext, &lower_known, Origin(4), Origin(0), alloc),
            Some(Requirement { from: Need::Sig(0), into: Need::Capture(0) }),
        );
    }
}
