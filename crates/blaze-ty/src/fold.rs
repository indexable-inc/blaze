//! Folds as linear passes: an explicit stack, no recursion, and a memo by (node, binder depth,
//! context) so a node shared in a term is walked again only if the memo, a cache that stays
//! in L1 ([`Cache`]), lost it
//! (rustc_type_ir/src/fold.rs, `TypeFolder` and `super_fold_with`; rustc recurses and guards
//! the stack with `ensure_sufficient_stack`, which grows it).
//!
//! One engine serves every fold. A [`Hook`] sees each node before its children (`pre`) and
//! answers: keep it, replace it, fold another node in its place, shift a replacement in by the
//! binders it lands under, or descend. Descending rebuilds the node from its children's results
//! and interns it only when one changed, so a pass over a term it does not change allocates
//! nothing. Every walk is skipped where the flags say the hook cannot apply.
//!
//! Cost (examples/cost.rs, this Mac, 2026-11-15, alternated with the version before under the
//! same load). Over a million list elements all changed: 36.5 ns an element, from 89; the
//! memo in L1 and the batched intern of a list's elements are both needed (81 with only the
//! first, 104 with only the second, 42 without the batch's prefetch). The floor is about one
//! independent cold hit a node (16 ns measured for lookups that do not wait on each other)
//! plus the walk's frames, about 20 ns: ratio about 1.8. A deep chain where every node
//! changes: 34 ns a node (30 to 33 before; neither the memo nor the deferral is the 2 ns, both
//! tested apart, and the cause is not found); each node's intern waits on its child's, so the
//! floor is a dependent hit (10 ns hot) plus the frames, about 15: ratio about 2.3. An
//! untouched subtree is one load of its flags (3.6 ns a field over a wide tuple).
//!
//! The engine also tracks what generalization needs: the variance of each position
//! (rustc_type_ir/src/relate.rs, `structurally_relate_tys`) and whether the node is under an
//! alias. One table of child positions ([`position`]) is shared with relate (`crate::relate`).

use core::convert::Infallible;

use blaze_own::{Alloc, Vec};

use crate::error::FoldError;
use crate::flags::Flags;
use crate::ids::{Arg, Const, Def, List, Node, TAG_CONST, TAG_LIST, TAG_REGION, TAG_TY, Ty, Universe, Variance, ix};
use crate::infer::{Table, Vars};
use crate::kind::{self, Role, ct, re, ty};
use crate::memo::{Cache, Memo};
use crate::tys::{Tys, Want};

/// The variances of an item's parameters (rustc's `variances_of`), asked only when a relation
/// or generalization passes a non-invariant position into an ADT, fn item or opaque.
pub trait VarianceOf {
    /// The variance of parameter `index` (parents first) of `def`.
    fn variance(&mut self, def: Def, index: u32) -> Variance;
}

/// Every parameter invariant: for walks where variance does not matter, and for front ends
/// that have not computed variances yet (sound: invariance relates the fewest types).
pub struct AllInvariant;

impl VarianceOf for AllInvariant {
    fn variance(&mut self, _def: Def, _index: u32) -> Variance {
        Variance::Invariant
    }
}

/// A walk's context at a node: variance in the low two bits, "under an alias" in bit 2.
pub(crate) const IN_ALIAS: u8 = 1 << 2;

#[inline(always)]
pub(crate) const fn ctx_of(v: Variance) -> u8 {
    // The code is at most 3.
    match v {
        Variance::Covariant => 0,
        Variance::Invariant => 1,
        Variance::Contravariant => 2,
        Variance::Bivariant => 3,
    }
}

#[inline(always)]
pub(crate) const fn variance_of(ctx: u8) -> Variance {
    match ctx & 3 {
        0 => Variance::Covariant,
        1 => Variance::Invariant,
        2 => Variance::Contravariant,
        _ => Variance::Bivariant,
    }
}

/// How a list's elements get their variance from the list's position.
pub(crate) const RULE_SAME: u8 = 0;
/// An item's arguments: element `i` by the item's variance of parameter `i`.
pub(crate) const RULE_ITEM: u8 = 1;
/// A fn signature: inputs contravariant, the output (last) covariant.
pub(crate) const RULE_SIG: u8 = 2;

/// The variance of a child position of a row and, for a list child, how its elements get
/// theirs. `b` says which column.
#[inline]
pub(crate) const fn position(tag: u32, k: u8, b: bool) -> (Variance, u8) {
    use Variance::{Covariant as Co, Invariant as Inv};
    // A const's children (a value's type, an unevaluated const's arguments) are invariant.
    if tag != TAG_TY {
        return (Inv, RULE_SAME);
    }
    match k {
        ty::ADT | ty::FN_DEF | ty::OPAQUE | ty::RIGID_OPAQUE => (Co, RULE_ITEM),
        ty::FN_PTR => (Co, RULE_SIG),
        ty::TUPLE | ty::RECORD | ty::UNION | ty::ARRAY | ty::SLICE | ty::PTR | ty::REF => (Co, RULE_SAME),
        ty::REF_MUT | ty::DYNAMIC => {
            if b {
                (Co, RULE_SAME)
            } else {
                (Inv, RULE_SAME)
            }
        }
        _ => (Inv, RULE_SAME),
    }
}

/// The variance of element `i` of `len` of a list in a position of variance `v`.
#[inline]
pub(crate) fn element(v: Variance, rule: u8, def: u32, i: u32, len: u32, vo: &mut dyn VarianceOf) -> Variance {
    match rule {
        RULE_ITEM if v != Variance::Invariant => v.xform(vo.variance(Def(def), i)),
        RULE_SIG if i + 1 < len => v.xform(Variance::Contravariant),
        _ => {
            if rule == RULE_ITEM {
                Variance::Invariant
            } else {
                v
            }
        }
    }
}

/// Whether a row's children are under an alias (for generalization).
#[inline(always)]
pub(crate) const fn opens_alias(tag: u32, k: u8) -> bool {
    (tag == TAG_TY && kind::is_alias(k)) || (tag == TAG_CONST && k == ct::UNEVALUATED)
}

/// What a hook says of a node.
pub(crate) enum Step {
    /// The node itself.
    Keep,
    /// This instead, as it is.
    Replace(Node),
    /// Fold this instead (a variable's value).
    Again(Node),
    /// This, with its bound variables escaping it shifted in by the count (a substituted
    /// argument landing under binders).
    ShiftIn(Node, u32),
    /// Fold the children and rebuild.
    Descend,
}

pub(crate) trait Hook {
    type Error;
    /// Whether a node of these flags at `depth` is kept as it is: tested before the memo and
    /// before `pre`, so an untouched subtree costs one load of its flags.
    #[inline(always)]
    fn skip(&self, _flags: Flags, _depth: u32) -> bool {
        false
    }
    fn pre(&mut self, tys: &mut Tys<'_>, vars: &mut Vars<'_>, n: Node, depth: u32, ctx: u8) -> Result<Step, Self::Error>;
}

const ENTER: u8 = 0;
const EXIT: u8 = 1;
const ALIAS: u8 = 2;

#[derive(Clone, Copy)]
struct Frame {
    node: Node,
    depth: u32,
    /// The rule's item, for a list.
    def: u32,
    /// Results start here, for an exit.
    base: u32,
    ctx: u8,
    op: u8,
    rule: u8,
    /// An element of a list: a rebuilt row is interned at the list's exit, with its siblings.
    in_list: bool,
}

/// A result not interned yet: its place on the result stack and its memo key.
#[derive(Clone, Copy)]
struct Pend {
    at: u32,
    /// [`NO_MEMO`] for none.
    memo: u64,
}

/// No memo key is all ones: a key's context is three bits at bit 56.
const NO_MEMO: u64 = u64::MAX;

/// One engine's stacks and memo.
///
/// A rebuilt row that is a list's element is not interned at its own exit but at the list's,
/// with its siblings', through [`Tys::intern_many`]: siblings' rows do not depend on each
/// other, so their probes' cache misses overlap. Other rows are interned at their exit: a
/// row's one or two children gain little from a batch, and deferring them measured slower on
/// a deep chain. `want` and `pend` are the rows waiting, in result-stack order, so a list's
/// elements' are a suffix.
pub(crate) struct Stacks<'a> {
    stack: Vec<Frame, Alloc<'a>>,
    out: Vec<Node, Alloc<'a>>,
    memo: Cache<'a>,
    want: Vec<Want, Alloc<'a>>,
    pend: Vec<Pend, Alloc<'a>>,
}

impl<'a> Stacks<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Stacks<'a> {
        Stacks {
            stack: Vec::new_in(alloc),
            out: Vec::new_in(alloc),
            memo: Cache::new_in(alloc),
            want: Vec::new_in(alloc),
            pend: Vec::new_in(alloc),
        }
    }

    /// Interns the waiting rows at result positions `at` and above, into their places and the
    /// memo.
    #[inline(always)]
    fn resolve_from(&mut self, tys: &mut Tys<'_>, at: u32) {
        if !self.want.is_empty() {
            self.resolve_slow(tys, at);
        }
    }

    /// [`Stacks::resolve_from`]'s batch, out of the walk's loop.
    #[inline(never)]
    fn resolve_slow(&mut self, tys: &mut Tys<'_>, at: u32) {
        let from = self.pend.partition_point(|p| p.at < at);
        if from == self.want.len() {
            return;
        }
        let Stacks { want, pend, out, memo, .. } = self;
        tys.intern_many(&want[from..], |i, node| {
            let p = pend[from + i];
            out[ix(p.at)] = node;
            if p.memo != NO_MEMO {
                memo.insert(p.memo, node.bits());
            }
        });
        want.truncate(from);
        pend.truncate(from);
    }
}

/// The engine: the stacks of a pass and a spare set for the shifts a pass asks for.
pub(crate) struct Engine<'a> {
    main: Stacks<'a>,
    spare: Stacks<'a>,
}

impl<'a> Engine<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Engine<'a> {
        Engine { main: Stacks::new_in(alloc), spare: Stacks::new_in(alloc) }
    }

    /// Folds `root` with `h`, starting in context `ctx`.
    pub(crate) fn run<H: Hook>(
        &mut self,
        tys: &mut Tys<'_>,
        vars: &mut Vars<'_>,
        vo: &mut dyn VarianceOf,
        h: &mut H,
        root: Node,
        ctx: u8,
    ) -> Result<Node, H::Error> {
        run_on(&mut self.main, Some(&mut self.spare), tys, vars, vo, h, root, ctx)
    }
}

#[inline(always)]
fn memo_key(n: Node, depth: u32, ctx: u8) -> Option<u64> {
    if depth >= 1 << 24 {
        return None;
    }
    Some(u64::from(n.bits()) | u64::from(depth) << 32 | u64::from(ctx) << 56)
}

#[expect(clippy::too_many_arguments, reason = "the engine's one entry: its stacks, the arena and the hook's inputs")]
fn run_on<H: Hook>(
    st: &mut Stacks<'_>,
    mut spare: Option<&mut Stacks<'_>>,
    tys: &mut Tys<'_>,
    vars: &mut Vars<'_>,
    vo: &mut dyn VarianceOf,
    h: &mut H,
    root: Node,
    ctx: u8,
) -> Result<Node, H::Error> {
    st.stack.clear();
    st.out.clear();
    st.memo.clear();
    st.want.clear();
    st.pend.clear();
    st.stack.push(Frame { node: root, depth: 0, def: 0, base: 0, ctx, op: ENTER, rule: RULE_SAME, in_list: false });
    while let Some(f) = st.stack.pop() {
        match f.op {
            ENTER => {
                if h.skip(tys.node_flags(f.node), f.depth) {
                    st.out.push(f.node);
                    continue;
                }
                let key = if f.rule == RULE_SAME { memo_key(f.node, f.depth, f.ctx) } else { None };
                if let Some(k) = key
                    && let Some(r) = st.memo.get(k)
                {
                    st.out.push(Node(r));
                    continue;
                }
                match h.pre(tys, vars, f.node, f.depth, f.ctx)? {
                    Step::Keep => st.out.push(f.node),
                    Step::Replace(n) => {
                        if let Some(k) = key {
                            st.memo.insert(k, n.bits());
                        }
                        st.out.push(n);
                    }
                    Step::ShiftIn(n, by) => {
                        let r = match spare.as_deref_mut() {
                            Some(sp) => shift_on(sp, tys, vars, n, by),
                            None => shift_on(&mut Stacks::new_in(tys.allocator()), tys, vars, n, by),
                        };
                        if let Some(k) = key {
                            st.memo.insert(k, r.bits());
                        }
                        st.out.push(r);
                    }
                    Step::Again(n) => {
                        st.stack.push(Frame { op: ALIAS, ..f });
                        st.stack.push(Frame { node: n, op: ENTER, rule: RULE_SAME, def: 0, ..f });
                    }
                    Step::Descend => descend(st, tys, vo, f),
                }
            }
            EXIT => {
                let base = ix(f.base);
                let n = f.node;
                let key = if f.rule == RULE_SAME { memo_key(n, f.depth, f.ctx) } else { None };
                let result = if n.tag() == TAG_LIST {
                    // Only a list's children wait to be interned.
                    st.resolve_from(tys, f.base);
                    let l = n.list();
                    let same = {
                        let old = tys.list(l);
                        let new = &st.out[base..];
                        old.len() == new.len() && old.iter().zip(new.iter()).all(|(o, r)| o.bits() == r.bits())
                    };
                    if same {
                        n
                    } else {
                        let m = tys.list_mark();
                        for r in st.out[base..].iter() {
                            tys.list_push(Arg(r.bits()));
                        }
                        tys.list_finish(m).node()
                    }
                } else {
                    let (k, a, b) = tys.raw(n);
                    let d = tys.desc(n);
                    let mut i = base;
                    let na = if d.a == Role::Child {
                        i += 1;
                        st.out[i - 1].bits()
                    } else {
                        a
                    };
                    let nb = if d.b == Role::Child { st.out[i].bits() } else { b };
                    if na == a && nb == b {
                        n
                    } else if !f.in_list {
                        tys.intern_node(n.tag(), k, na, nb)
                    } else {
                        // Interned at the list's exit, with its siblings; `n` holds the place.
                        st.out.truncate(base);
                        st.out.push(n);
                        st.want.push(Want { tag: n.tag(), kind: k, a: na, b: nb });
                        st.pend.push(Pend { at: f.base, memo: key.unwrap_or(NO_MEMO) });
                        continue;
                    }
                };
                st.out.truncate(base);
                st.out.push(result);
                if let Some(k) = key {
                    st.memo.insert(k, result.bits());
                }
            }
            _ => {
                // The result of the node folded in `f.node`'s place is `f.node`'s.
                if let Some(last) = st.out.len().checked_sub(1).and_then(crate::ids::row) {
                    st.resolve_from(tys, last);
                }
                if let (Some(k), Some(r)) = (memo_key(f.node, f.depth, f.ctx), st.out.last()) {
                    st.memo.insert(k, r.bits());
                }
            }
        }
    }
    st.resolve_from(tys, 0);
    match st.out.pop() {
        Some(r) => Ok(r),
        None => Ok(root),
    }
}

/// Pushes `f`'s exit and its children's entries, last child first.
#[inline]
fn descend(st: &mut Stacks<'_>, tys: &Tys<'_>, vo: &mut dyn VarianceOf, f: Frame) {
    let n = f.node;
    let Some(base) = crate::ids::row(st.out.len()) else { panic!("blaze-ty: a fold's result stack is full (rule 18)") };
    st.stack.push(Frame { op: EXIT, base, ..f });
    let v = variance_of(f.ctx);
    let alias = f.ctx & IN_ALIAS;
    if n.tag() == TAG_LIST {
        let len = tys.list_len(n.list());
        let items = tys.list(n.list());
        let mut i = len;
        for el in items.iter().rev() {
            i -= 1;
            let ev = element(v, f.rule, f.def, i, len, vo);
            st.stack.push(Frame {
                node: el.node(),
                depth: f.depth,
                def: 0,
                base: 0,
                ctx: ctx_of(ev) | alias,
                op: ENTER,
                rule: RULE_SAME,
                in_list: true,
            });
        }
        return;
    }
    let (k, a, b) = tys.raw(n);
    let d = tys.desc(n);
    let depth = if d.binder { f.depth + 1 } else { f.depth };
    let alias = if opens_alias(n.tag(), k) { IN_ALIAS } else { alias };
    let def = if d.a == Role::Def { a } else { 0 };
    if d.b == Role::Child {
        let (pv, rule) = position(n.tag(), k, true);
        let ctx = ctx_of(v.xform(pv)) | alias;
        st.stack.push(Frame { node: Node(b), depth, def, base: 0, ctx, op: ENTER, rule, in_list: false });
    }
    if d.a == Role::Child {
        let (pv, rule) = position(n.tag(), k, false);
        let ctx = ctx_of(v.xform(pv)) | alias;
        st.stack.push(Frame { node: Node(a), depth, def, base: 0, ctx, op: ENTER, rule, in_list: false });
    }
}

/// Shifts the bound variables escaping `n` in (or out) by `by` binders
/// (rustc_type_ir/src/fold.rs, `Shifter`).
pub(crate) struct Shift {
    pub(crate) by: u32,
    pub(crate) out: bool,
}

/// Where a walk at `depth` is at `n`, for a hook on bound variables.
enum At {
    /// Nothing inside escapes `depth`, or `n` is a bound variable of a binder inside.
    Keep,
    /// Something inside may escape.
    Descend,
    /// A bound variable escaping `depth`: kind, De Bruijn index, variable.
    Bound(u8, u32, u32),
}

#[inline]
fn bound_at(tys: &Tys<'_>, n: Node, depth: u32) -> At {
    if !tys.node_flags(n).escapes(depth) {
        return At::Keep;
    }
    let (k, d, var) = tys.raw(n);
    let bound = match n.tag() {
        TAG_TY => k == ty::BOUND,
        TAG_REGION => k == re::BOUND,
        TAG_CONST => k == ct::BOUND,
        _ => false,
    };
    match (bound, d >= depth) {
        (false, _) => At::Descend,
        (true, false) => At::Keep,
        (true, true) => At::Bound(k, d, var),
    }
}

impl Hook for Shift {
    type Error = FoldError;

    #[inline(always)]
    fn skip(&self, f: Flags, depth: u32) -> bool {
        !f.escapes(depth)
    }

    #[inline]
    fn pre(&mut self, tys: &mut Tys<'_>, _: &mut Vars<'_>, n: Node, depth: u32, _: u8) -> Result<Step, FoldError> {
        match bound_at(tys, n, depth) {
            At::Keep => Ok(Step::Keep),
            At::Descend => Ok(Step::Descend),
            At::Bound(k, d, var) => {
                let nd = if self.out {
                    match d.checked_sub(self.by) {
                        Some(x) => x,
                        None => return Err(FoldError::ShiftUnderflow { node: n }),
                    }
                } else {
                    d.saturating_add(self.by)
                };
                Ok(Step::Replace(tys.intern_node(n.tag(), k, nd, var)))
            }
        }
    }
}

/// Shifting in, which cannot fail.
pub(crate) struct ShiftIn(pub(crate) u32);

impl Hook for ShiftIn {
    type Error = Infallible;

    #[inline(always)]
    fn skip(&self, f: Flags, depth: u32) -> bool {
        !f.escapes(depth)
    }

    #[inline]
    fn pre(&mut self, tys: &mut Tys<'_>, _: &mut Vars<'_>, n: Node, depth: u32, _: u8) -> Result<Step, Infallible> {
        Ok(match bound_at(tys, n, depth) {
            At::Keep => Step::Keep,
            At::Descend => Step::Descend,
            At::Bound(k, d, var) => Step::Replace(tys.intern_node(n.tag(), k, d.saturating_add(self.0), var)),
        })
    }
}

fn shift_on(st: &mut Stacks<'_>, tys: &mut Tys<'_>, vars: &mut Vars<'_>, n: Node, by: u32) -> Node {
    if by == 0 || !tys.node_flags(n).escapes(0) {
        return n;
    }
    match run_on(st, None, tys, vars, &mut AllInvariant, &mut ShiftIn(by), n, ctx_of(Variance::Invariant)) {
        Ok(r) => r,
        Err(never) => match never {},
    }
}

/// The replacement of an argument landing `depth` binders down.
#[inline]
fn landing(tys: &Tys<'_>, x: Node, depth: u32) -> Step {
    if depth > 0 && tys.node_flags(x).escapes(0) { Step::ShiftIn(x, depth) } else { Step::Replace(x) }
}

/// Generic parameters to arguments (rustc_type_ir/src/binder.rs, `ArgFolder`).
pub(crate) struct Subst {
    pub(crate) args: List,
}

impl Hook for Subst {
    type Error = FoldError;

    #[inline(always)]
    fn skip(&self, f: Flags, _: u32) -> bool {
        !f.has(Flags::PARAM)
    }

    #[inline]
    fn pre(&mut self, tys: &mut Tys<'_>, _: &mut Vars<'_>, n: Node, depth: u32, _: u8) -> Result<Step, FoldError> {
        if !tys.node_flags(n).has(Flags::PARAM) {
            return Ok(Step::Keep);
        }
        let (k, a, _) = tys.raw(n);
        let param = match n.tag() {
            TAG_TY => k == ty::PARAM,
            TAG_REGION => k == re::EARLY_PARAM,
            TAG_CONST => k == ct::PARAM,
            _ => false,
        };
        if !param {
            return Ok(Step::Descend);
        }
        let Some(arg) = tys.list_get(self.args, a) else {
            return Err(FoldError::ParamOutOfRange { param: n, args: tys.list_len(self.args) });
        };
        if arg.node().tag() != n.tag() {
            return Err(FoldError::ParamClass { param: n });
        }
        Ok(landing(tys, arg.node(), depth))
    }
}

/// What a binder's variables become.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoundTo {
    /// Placeholders of this universe (rustc's `enter_forall`).
    Placeholders(Universe),
    /// New variables of this universe (`instantiate_binder_with_fresh_vars`).
    Fresh(Universe),
    /// These arguments, by the variable's index (`instantiate`).
    Args(List),
}

/// A binder's variables to placeholders, variables or arguments
/// (rustc_type_ir/src/fold.rs, `BoundVarReplacer`). The variables bound by the binder just
/// outside the root are those of De Bruijn index equal to the depth; variables of outer
/// binders are left as they are, as rustc does (its callers instantiate outermost binders).
pub(crate) struct ReplaceBound<'m, 'a> {
    pub(crate) to: BoundTo,
    /// Fresh variables made so far, by the bound variable's index.
    pub(crate) made: &'m mut Memo<'a>,
}

impl Hook for ReplaceBound<'_, '_> {
    type Error = FoldError;

    #[inline(always)]
    fn skip(&self, f: Flags, depth: u32) -> bool {
        !f.escapes(depth)
    }

    fn pre(&mut self, tys: &mut Tys<'_>, vars: &mut Vars<'_>, n: Node, depth: u32, _: u8) -> Result<Step, FoldError> {
        if !tys.node_flags(n).escapes(depth) {
            return Ok(Step::Keep);
        }
        let (k, d, var) = tys.raw(n);
        let tag = n.tag();
        let bound = match tag {
            TAG_TY => k == ty::BOUND,
            TAG_REGION => k == re::BOUND,
            TAG_CONST => k == ct::BOUND,
            _ => false,
        };
        if !bound {
            return Ok(Step::Descend);
        }
        if d != depth {
            return Ok(Step::Keep);
        }
        let x = match self.to {
            BoundTo::Placeholders(u) => match tag {
                TAG_TY => tys.placeholder(u, var).node(),
                TAG_REGION => tys.re_placeholder(u, var).node(),
                _ => tys.const_placeholder(u, var).node(),
            },
            BoundTo::Fresh(u) => {
                let key = u64::from(var) | u64::from(tag) << 32;
                match self.made.get(key) {
                    Some(bits) => Node(bits),
                    None => {
                        let x = match tag {
                            TAG_TY => {
                                let v = vars.new_var(Table::Ty, u);
                                tys.ty_var(v).node()
                            }
                            TAG_REGION => {
                                let v = vars.new_region_var(u);
                                tys.re_var(v).node()
                            }
                            _ => {
                                let v = vars.new_var(Table::Const, u);
                                tys.const_var(v).node()
                            }
                        };
                        self.made.insert(key, x.bits());
                        x
                    }
                }
            }
            BoundTo::Args(l) => {
                let Some(arg) = tys.list_get(l, var) else {
                    return Err(FoldError::ParamOutOfRange { param: n, args: tys.list_len(l) });
                };
                if arg.node().tag() != tag {
                    return Err(FoldError::ParamClass { param: n });
                }
                return Ok(landing(tys, arg.node(), depth));
            }
        };
        Ok(Step::Replace(x))
    }
}

/// Variables to their values, everywhere (rustc's `resolve_vars_if_possible`, here complete:
/// unbound variables become their class's root, so equal classes are equal ids).
pub(crate) struct Resolve;

impl Hook for Resolve {
    type Error = Infallible;

    #[inline(always)]
    fn skip(&self, f: Flags, _: u32) -> bool {
        !f.has(Flags::TY_INFER | Flags::CT_INFER)
    }

    #[inline]
    fn pre(&mut self, tys: &mut Tys<'_>, vars: &mut Vars<'_>, n: Node, _: u32, _: u8) -> Result<Step, Infallible> {
        if !tys.node_flags(n).has(Flags::TY_INFER | Flags::CT_INFER) {
            return Ok(Step::Keep);
        }
        let (k, v, _) = tys.raw(n);
        let step = match (n.tag(), k) {
            (TAG_TY, ty::TY_VAR) => match vars.value(Table::Ty, v) {
                Some(t) => Step::Again(Ty(t).node()),
                None => root_var(tys, vars, Table::Ty, v),
            },
            (TAG_TY, ty::INT_VAR) => match vars.value(Table::Int, v) {
                Some(t) => Step::Replace(Ty(t).node()),
                None => root_var(tys, vars, Table::Int, v),
            },
            (TAG_TY, ty::FLOAT_VAR) => match vars.value(Table::Float, v) {
                Some(t) => Step::Replace(Ty(t).node()),
                None => root_var(tys, vars, Table::Float, v),
            },
            (TAG_CONST, ct::VAR) => match vars.value(Table::Const, v) {
                Some(c) => Step::Again(Const(c).node()),
                None => root_var(tys, vars, Table::Const, v),
            },
            _ => Step::Descend,
        };
        Ok(step)
    }
}

/// The variable node of `v`'s root, or `Keep` when `v` is the root.
pub(crate) fn root_var(tys: &mut Tys<'_>, vars: &Vars<'_>, t: Table, v: u32) -> Step {
    let r = vars.root(t, v);
    if r == v {
        return Step::Keep;
    }
    Step::Replace(var_node(tys, t, r))
}

/// The node of variable `v` of table `t`.
pub(crate) fn var_node(tys: &mut Tys<'_>, t: Table, v: u32) -> Node {
    match t {
        Table::Ty => tys.ty_var(v).node(),
        Table::Int => tys.int_var(v).node(),
        Table::Float => tys.float_var(v).node(),
        Table::Const => tys.const_var(v).node(),
    }
}
