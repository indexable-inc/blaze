//! Canonicalization: a term with its variables and placeholders renumbered in first-visit
//! order, so terms equal up to renaming are one canonical term, and its key as words.
//!
//! References: rustc_next_trait_solver/src/canonical/canonicalizer.rs (`Canonicalizer`: which
//! variables each mode replaces, the universes, `finalize`), rustc_type_ir/src/canonical.rs
//! (`Canonical`, `CanonicalVarKind`), and rustc_next_trait_solver/src/canonical/mod.rs
//! (instantiating a query's response from the caller's original values).
//!
//! The canonical term is in the arena (its variables are `CANONICAL` rows); its key is a
//! sequence of words written post-order with back-references to repeated nodes, items as their
//! 32-byte keys, so the key is the same in every task's arena and can key a shared cache.
//! The canonical term and its key are made in one walk ([`walk`]): a node's words are written
//! at its exit, from its canonical node, so the term is read once. Measured (examples/cost.rs,
//! this Mac, 2026-11-15, alternated with the two-walk version under the same load): a 6-node
//! goal with 3 variables, 110 to 118 ns, from 127 to 132. The floor is the walk (6 nodes, a
//! key lookup and insert each), 3 variable-map entries and 3 interns of canonical variables
//! (hot hits, about 10 ns each), about 60 ns: ratio about 1.9.
//! Deviation: type variables carry no sub-unification root (rustc's `sub_root`): blaze-ty
//! records two-variable subtyping as a goal and has no sub-unification table.

use blaze_int::Int;
use blaze_own::{Alloc, Vec};

use crate::cx::TyCx;
use crate::error::{FoldError, ResponseError};
use crate::flags::Flags;
use crate::fold::{AllInvariant, Hook, Step, VarianceOf, ctx_of};
use crate::ids::{Arg, List, Node, TAG_CONST, TAG_LIST, TAG_REGION, TAG_TY, Universe, Variance, ix};
use crate::infer::{Table, Vars};
use crate::kind::{Role, ct, re, ty};
use crate::memo::Memo;
use crate::tys::Tys;

/// What a canonicalization is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// A goal: every variable, placeholder, parameter and free region (`'static` too unless
    /// `param_env`) becomes a canonical variable of the root universe.
    Input { param_env: bool },
    /// A query's response: variables and placeholders made inside the query, their
    /// universes counted from the input's.
    Response { max_input_universe: Universe },
}

/// What a canonical variable stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CanonKind {
    Ty,
    Int,
    Float,
    Region,
    Const,
    PlaceholderTy,
    PlaceholderRegion,
    PlaceholderConst,
}

impl CanonKind {
    const fn code(self) -> u32 {
        match self {
            CanonKind::Ty => 0,
            CanonKind::Int => 1,
            CanonKind::Float => 2,
            CanonKind::Region => 3,
            CanonKind::Const => 4,
            CanonKind::PlaceholderTy => 5,
            CanonKind::PlaceholderRegion => 6,
            CanonKind::PlaceholderConst => 7,
        }
    }
}

/// One canonical variable: its kind, universe and (for a placeholder) its variable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CanonVar {
    pub kind: CanonKind,
    pub universe: Universe,
    pub var: u32,
}

/// Where a canonicalization writes, owned by the caller and reused.
pub struct CanonBuf<'b> {
    /// The canonical variables, by index.
    pub vars: Vec<CanonVar, Alloc<'b>>,
    /// What each canonical variable stood for in the caller (rustc's `orig_values`).
    pub orig: Vec<Arg, Alloc<'b>>,
    /// The key: the canonical term as words, then the variables, then the max universe.
    pub key: Vec<u32, Alloc<'b>>,
}

impl<'b> CanonBuf<'b> {
    pub fn new_in(alloc: Alloc<'b>) -> CanonBuf<'b> {
        CanonBuf { vars: Vec::new_in(alloc), orig: Vec::new_in(alloc), key: Vec::new_in(alloc) }
    }

    pub fn clear(&mut self) {
        self.vars.clear();
        self.orig.clear();
        self.key.clear();
    }

    /// The key as little-endian bytes, into `out`.
    pub fn key_bytes(&self, out: &mut Vec<u8, Alloc<'_>>) {
        for w in self.key.iter() {
            out.extend_from_slice(&w.to_le_bytes());
        }
    }
}

/// A canonical term in the arena.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Canonical {
    pub value: Node,
    pub max_universe: Universe,
    pub vars: u32,
}

struct Canonicalize<'m, 'a, 'b> {
    mode: Mode,
    map: &'m mut Memo<'a>,
    buf: &'m mut CanonBuf<'b>,
}

impl Canonicalize<'_, '_, '_> {
    /// The canonical variable of `orig`, made if new.
    fn var(&mut self, tys: &mut Tys<'_>, orig: Node, kind: CanonKind, universe: Universe, var: Option<u32>) -> Step {
        let i = match self.map.get(u64::from(orig.bits())) {
            Some(i) => i,
            None => {
                let Some(i) = crate::ids::row(self.buf.vars.len()) else {
                    panic!("blaze-ty: a canonical term has too many variables (rule 18)")
                };
                self.buf.vars.push(CanonVar { kind, universe, var: var.unwrap_or(i) });
                if let Some(a) = orig.arg() {
                    self.buf.orig.push(a);
                }
                self.map.insert(u64::from(orig.bits()), i);
                i
            }
        };
        let x = match orig.tag() {
            TAG_TY => tys.canonical_ty(i).node(),
            TAG_REGION => tys.re_canonical(i).node(),
            _ => tys.const_canonical(i).node(),
        };
        Step::Replace(x)
    }

    fn input(&self) -> bool {
        matches!(self.mode, Mode::Input { .. })
    }
}

/// What canonicalization may replace: a node of none of these flags is its own canonical form.
const NEEDS: u16 = Flags::INFER | Flags::PLACEHOLDER | Flags::PARAM | Flags::FREE_REGIONS;

impl Canonicalize<'_, '_, '_> {
    /// What becomes of `n`, a node of [`NEEDS`]'s flags: `Keep` (as it is, not descended),
    /// `Replace` (a leaf: a canonical variable or a variable's integer or float value),
    /// `Again` (a variable's value, canonicalized in its place) or `Descend`.
    fn pre(&mut self, tys: &mut Tys<'_>, vars: &mut Vars<'_>, n: Node) -> Result<Step, FoldError> {
        let (k, a, b) = tys.raw(n);
        let input = self.input();
        let root = Universe::ROOT;
        let mode = self.mode;
        let shifted = move |u: Universe| match mode {
            Mode::Input { .. } => root,
            Mode::Response { max_input_universe } => Universe(u.0.saturating_sub(max_input_universe.0)),
        };
        let step = match (n.tag(), k) {
            (TAG_TY, ty::TY_VAR) => match vars.value(Table::Ty, a) {
                Some(t) => Step::Again(crate::ids::Ty(t).node()),
                None => {
                    let r = vars.root(Table::Ty, a);
                    let rn = tys.ty_var(r).node();
                    let u = shifted(vars.universe_of(Table::Ty, r));
                    self.var(tys, rn, CanonKind::Ty, u, None)
                }
            },
            (TAG_TY, ty::INT_VAR) => match vars.value(Table::Int, a) {
                Some(t) => Step::Replace(crate::ids::Ty(t).node()),
                None => {
                    let rn = tys.int_var(vars.root(Table::Int, a)).node();
                    self.var(tys, rn, CanonKind::Int, root, None)
                }
            },
            (TAG_TY, ty::FLOAT_VAR) => match vars.value(Table::Float, a) {
                Some(t) => Step::Replace(crate::ids::Ty(t).node()),
                None => {
                    let rn = tys.float_var(vars.root(Table::Float, a)).node();
                    self.var(tys, rn, CanonKind::Float, root, None)
                }
            },
            (TAG_TY, ty::PLACEHOLDER) => {
                if input {
                    self.var(tys, n, CanonKind::PlaceholderTy, root, None)
                } else {
                    self.var(tys, n, CanonKind::PlaceholderTy, shifted(Universe(a)), Some(b))
                }
            }
            (TAG_TY, ty::PARAM) => {
                if !input {
                    return Err(FoldError::InResponse { node: n });
                }
                self.var(tys, n, CanonKind::PlaceholderTy, root, None)
            }
            (TAG_REGION, _) => match k {
                re::BOUND => Step::Keep,
                re::STATIC => match self.mode {
                    Mode::Input { param_env: false } => self.var(tys, n, CanonKind::PlaceholderRegion, root, None),
                    _ => Step::Keep,
                },
                re::ERASED | re::ERROR => {
                    if input {
                        self.var(tys, n, CanonKind::PlaceholderRegion, root, None)
                    } else {
                        Step::Keep
                    }
                }
                re::EARLY_PARAM | re::LATE_PARAM => {
                    if !input {
                        return Err(FoldError::InResponse { node: n });
                    }
                    self.var(tys, n, CanonKind::PlaceholderRegion, root, None)
                }
                re::PLACEHOLDER => {
                    if input {
                        self.var(tys, n, CanonKind::PlaceholderRegion, root, None)
                    } else {
                        if let Mode::Response { max_input_universe } = self.mode
                            && a != 0
                            && max_input_universe.can_name(Universe(a))
                        {
                            return Err(FoldError::InResponse { node: n });
                        }
                        self.var(tys, n, CanonKind::PlaceholderRegion, shifted(Universe(a)), Some(b))
                    }
                }
                re::VAR => {
                    if input {
                        self.var(tys, n, CanonKind::PlaceholderRegion, root, None)
                    } else {
                        let u = shifted(vars.region_universe(a));
                        self.var(tys, n, CanonKind::Region, u, None)
                    }
                }
                _ => Step::Keep,
            },
            (TAG_CONST, ct::VAR) => match vars.value(Table::Const, a) {
                Some(c) => Step::Again(crate::ids::Const(c).node()),
                None => {
                    let r = vars.root(Table::Const, a);
                    let rn = tys.const_var(r).node();
                    let u = shifted(vars.universe_of(Table::Const, r));
                    self.var(tys, rn, CanonKind::Const, u, None)
                }
            },
            (TAG_CONST, ct::PLACEHOLDER) => {
                if input {
                    self.var(tys, n, CanonKind::PlaceholderConst, root, None)
                } else {
                    self.var(tys, n, CanonKind::PlaceholderConst, shifted(Universe(a)), Some(b))
                }
            }
            (TAG_CONST, ct::PARAM) => {
                if !input {
                    return Err(FoldError::InResponse { node: n });
                }
                self.var(tys, n, CanonKind::PlaceholderConst, root, None)
            }
            _ => Step::Descend,
        };
        Ok(step)
    }
}

/// Canonical variables to values (rustc's `instantiate_canonical`'s substitution).
struct Values {
    values: List,
}

impl Hook for Values {
    type Error = FoldError;

    #[inline(always)]
    fn skip(&self, f: Flags, _: u32) -> bool {
        !f.has(Flags::CANONICAL)
    }

    fn pre(&mut self, tys: &mut Tys<'_>, _: &mut Vars<'_>, n: Node, depth: u32, _: u8) -> Result<Step, FoldError> {
        if !tys.node_flags(n).has(Flags::CANONICAL) {
            return Ok(Step::Keep);
        }
        let (k, i, _) = tys.raw(n);
        let canon = match n.tag() {
            TAG_TY => k == ty::CANONICAL,
            TAG_REGION => k == re::CANONICAL,
            TAG_CONST => k == ct::CANONICAL,
            _ => false,
        };
        if !canon {
            return Ok(Step::Descend);
        }
        let Some(v) = tys.list_get(self.values, i) else {
            return Err(FoldError::CanonicalOutOfRange { node: n, values: tys.list_len(self.values) });
        };
        if v.node().tag() != n.tag() {
            return Err(FoldError::ParamClass { param: n });
        }
        let x = v.node();
        if depth > 0 && tys.node_flags(x).escapes(0) { Ok(Step::ShiftIn(x, depth)) } else { Ok(Step::Replace(x)) }
    }
}

const KEY_BACKREF: u32 = 1 << 31;
const KEY_LIST: u32 = 3 << 8;

/// The walk's memo holds three maps, told apart by the high word of the key: a variable's
/// root to its canonical index (`Canonicalize::var`, high word 0), an input node to its
/// canonical node (only where they differ), and a canonical node to its post-order number in
/// the key (the back-reference target).
const FOLDED: u64 = 1 << 32;
const KEYED: u64 = 2 << 32;

const ENTER: u8 = 0;
const EXIT: u8 = 1;
/// After a variable's value: the variable's canonical node is the value's.
const ALIAS: u8 = 2;

/// One frame of canonicalization's walk.
#[derive(Clone, Copy)]
pub(crate) struct CanonFrame {
    node: Node,
    /// For an exit: where the node's children's results start on the result stack.
    base: u32,
    /// For an exit: where the node's key words start.
    key_at: u32,
    op: u8,
    /// The node is its own canonical form (no [`NEEDS`] flag, or kept): its subtree is only
    /// written to the key.
    plain: bool,
}

/// The canonical form of `root` and its key, in one post-order walk: each node's canonical
/// node is made at its exit and written to the key there, so the key is the canonical term's
/// (a node equal to one written before, by the canonical node, is one back-reference). Where
/// a node's canonical node was written before, the words its children wrote are cut back to
/// that one back-reference: they were all back-references, since the earlier node's subtree
/// was written first.
fn walk(
    tys: &mut Tys<'_>,
    vars: &mut Vars<'_>,
    stack: &mut Vec<CanonFrame, Alloc<'_>>,
    out: &mut Vec<Node, Alloc<'_>>,
    h: &mut Canonicalize<'_, '_, '_>,
    root: Node,
) -> Result<Node, FoldError> {
    stack.clear();
    out.clear();
    stack.push(CanonFrame { node: root, base: 0, key_at: 0, op: ENTER, plain: false });
    let mut count: u32 = 0;
    while let Some(f) = stack.pop() {
        let n = f.node;
        match f.op {
            ENTER => {
                let plain = f.plain || !tys.node_flags(n).has(NEEDS);
                let cand = if plain { n } else { h.map.get(FOLDED | u64::from(n.bits())).map_or(n, Node) };
                if let Some(at) = h.map.get(KEYED | u64::from(cand.bits())) {
                    out.push(cand);
                    h.buf.key.push(KEY_BACKREF | at);
                    continue;
                }
                let plain = if plain {
                    true
                } else {
                    match h.pre(tys, vars, n)? {
                        Step::Keep => true,
                        Step::Descend => false,
                        Step::Replace(x) => {
                            h.map.insert(FOLDED | u64::from(n.bits()), x.bits());
                            stack.push(CanonFrame { node: x, plain: true, ..f });
                            continue;
                        }
                        Step::Again(v) => {
                            stack.push(CanonFrame { op: ALIAS, ..f });
                            stack.push(CanonFrame { node: v, ..f });
                            continue;
                        }
                        Step::ShiftIn(..) => unreachable!("canonicalization shifts nothing"),
                    }
                };
                let (Some(base), Some(key_at)) = (crate::ids::row(out.len()), crate::ids::row(h.buf.key.len())) else {
                    panic!("blaze-ty: a canonical term is too large (rule 18)")
                };
                stack.push(CanonFrame { node: n, base, key_at, op: EXIT, plain });
                if n.tag() == TAG_LIST {
                    for el in tys.list(n.list()).iter().rev() {
                        stack.push(CanonFrame { node: el.node(), base: 0, key_at: 0, op: ENTER, plain });
                    }
                } else {
                    let (_, a, b) = tys.raw(n);
                    let d = tys.desc(n);
                    if d.b == Role::Child {
                        stack.push(CanonFrame { node: Node(b), base: 0, key_at: 0, op: ENTER, plain });
                    }
                    if d.a == Role::Child {
                        stack.push(CanonFrame { node: Node(a), base: 0, key_at: 0, op: ENTER, plain });
                    }
                }
            }
            EXIT => {
                let base = ix(f.base);
                let r = if f.plain { n } else { rebuild(tys, n, &out[base..]) };
                out.truncate(base);
                out.push(r);
                if r != n {
                    h.map.insert(FOLDED | u64::from(n.bits()), r.bits());
                }
                if let Some(at) = h.map.get(KEYED | u64::from(r.bits())) {
                    h.buf.key.truncate(ix(f.key_at));
                    h.buf.key.push(KEY_BACKREF | at);
                } else {
                    write_node(tys, r, &mut h.buf.key);
                    h.map.insert(KEYED | u64::from(r.bits()), count);
                    count += 1;
                }
            }
            _ => {
                if let Some(r) = out.last() {
                    h.map.insert(FOLDED | u64::from(n.bits()), r.bits());
                }
            }
        }
    }
    Ok(out.pop().unwrap_or(root))
}

/// `n` with its children's results `kids`, interned only if one changed.
#[inline]
fn rebuild(tys: &mut Tys<'_>, n: Node, kids: &[Node]) -> Node {
    if n.tag() == TAG_LIST {
        let l = n.list();
        let same = {
            let old = tys.list(l);
            old.len() == kids.len() && old.iter().zip(kids.iter()).all(|(o, r)| o.bits() == r.bits())
        };
        if same {
            return n;
        }
        let m = tys.list_mark();
        for r in kids {
            tys.list_push(Arg(r.bits()));
        }
        return tys.list_finish(m).node();
    }
    let (k, a, b) = tys.raw(n);
    let d = tys.desc(n);
    let mut i = 0;
    let na = if d.a == Role::Child {
        i += 1;
        kids[0].bits()
    } else {
        a
    };
    let nb = if d.b == Role::Child { kids[i].bits() } else { b };
    if na == a && nb == b { n } else { tys.intern_node(n.tag(), k, na, nb) }
}

/// A node's own words in the key, after its children's: a header (class and kind), its
/// numbers, its item as eight words of its key, its scalar as four words.
#[inline]
fn write_node(tys: &Tys<'_>, n: Node, out: &mut Vec<u32, Alloc<'_>>) {
    if n.tag() == TAG_LIST {
        out.push(KEY_LIST);
        out.push(tys.list_len(n.list()));
        return;
    }
    let (k, a, b) = tys.raw(n);
    let d = tys.desc(n);
    out.push(n.tag() << 8 | u32::from(k));
    for (role, x) in [(d.a, a), (d.b, b)] {
        match role {
            Role::Raw => out.push(x),
            Role::Def => {
                let key = tys.item_key(crate::ids::Def(x));
                for w in key.0.0.chunks_exact(4) {
                    out.push(u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
                }
            }
            Role::Scalar => {
                let v = tys.scalar_value(x);
                for s in 0..4u32 {
                    out.push((v >> (32 * s)).truncate_to::<u32>());
                }
            }
            Role::None | Role::Child => {}
        }
    }
}

impl<'a> TyCx<'a> {
    /// Canonicalizes `value` into `buf` (cleared first): the canonical term, its variables in
    /// first-visit order, the caller's values of them, and the key, in one walk.
    pub fn canonicalize(&mut self, value: Node, mode: Mode, buf: &mut CanonBuf<'_>) -> Result<Canonical, FoldError> {
        buf.clear();
        self.canon_map.clear();
        let mut h = Canonicalize { mode, map: &mut self.canon_map, buf };
        let c = walk(&mut self.tys, &mut self.vars, &mut self.canon_stack, &mut self.canon_out, &mut h, value)?;
        let max_universe = match mode {
            Mode::Input { .. } => Universe::ROOT,
            Mode::Response { .. } => buf.vars.iter().map(|v| v.universe).max().unwrap_or(Universe::ROOT),
        };
        let Some(count) = crate::ids::row(buf.vars.len()) else {
            panic!("blaze-ty: a canonical term has too many variables (rule 18)")
        };
        for v in buf.vars.iter() {
            buf.key.push(v.kind.code() << 24 | (v.universe.0 & 0x00ff_ffff));
            buf.key.push(v.var);
        }
        buf.key.push(max_universe.0);
        buf.key.push(count);
        Ok(Canonical { value: c, max_universe, vars: count })
    }

    /// The canonical term with its variables replaced by `values` (one per variable, of its
    /// class).
    pub fn substitute_canonical(&mut self, value: Node, values: List) -> Result<Node, FoldError> {
        if !self.tys.node_flags(value).has(Flags::CANONICAL) {
            return Ok(value);
        }
        let mut h = Values { values };
        self.eng.run(&mut self.tys, &mut self.vars, &mut AllInvariant, &mut h, value, ctx_of(Variance::Invariant))
    }

    /// Instantiates a canonical term with fresh variables and placeholders: one new universe
    /// per canonical universe above the root, which maps to the current one
    /// (`instantiate_canonical`). The values made are left in `values`.
    pub fn instantiate_canonical(
        &mut self,
        c: Canonical,
        vars: &[CanonVar],
        values: &mut Vec<Arg, Alloc<'_>>,
    ) -> Result<Node, FoldError> {
        let base = self.vars.universe();
        for _ in 0..c.max_universe.0 {
            self.vars.create_next_universe();
        }
        values.clear();
        for v in vars {
            let u = Universe(base.0 + v.universe.0);
            values.push(self.fresh_for(*v, u));
        }
        let l = self.tys.mk_list(values);
        self.substitute_canonical(c.value, l)
    }

    fn fresh_for(&mut self, v: CanonVar, u: Universe) -> Arg {
        match v.kind {
            CanonKind::Ty => self.next_ty_var_in(u).arg(),
            CanonKind::Int => self.next_int_var().arg(),
            CanonKind::Float => self.next_float_var().arg(),
            CanonKind::Region => self.next_region_var_in(u).arg(),
            CanonKind::Const => self.next_const_var_in(u).arg(),
            CanonKind::PlaceholderTy => self.tys.placeholder(u, v.var).arg(),
            CanonKind::PlaceholderRegion => self.tys.re_placeholder(u, v.var).arg(),
            CanonKind::PlaceholderConst => self.tys.const_placeholder(u, v.var).arg(),
        }
    }

    /// Applies a canonical response to the caller's original values
    /// (rustc_next_trait_solver/src/canonical/mod.rs, `instantiate_and_apply_query_response`):
    /// `c.value` is a list of the values the query gave the input's variables, aligned with
    /// `orig`. A response variable that is one of those values unchanged, of the root
    /// universe, takes the caller's value; the others are fresh. Then each original value is
    /// equated with its instantiated response.
    pub fn instantiate_response(
        &mut self,
        c: Canonical,
        vars: &[CanonVar],
        orig: &[Arg],
        values: &mut Vec<Arg, Alloc<'_>>,
        vo: &mut dyn VarianceOf,
    ) -> Result<(), ResponseError> {
        if c.value.tag() != TAG_LIST {
            return Err(ResponseError::NotAList { node: c.value });
        }
        let given = c.value.list();
        let n = self.tys.list_len(given);
        if ix(n) != orig.len() {
            return Err(ResponseError::Arity { response: n, orig: orig.len() });
        }
        values.clear();
        // `Arg(u32::MAX)` is never an argument: the marker of a value not yet chosen.
        let unset = Arg(u32::MAX);
        values.resize(vars.len(), unset);
        for (i, o) in (0u32..).zip(orig.iter()) {
            let Some(g) = self.tys.list_get(given, i) else { break };
            let (k, j, _) = self.tys.raw(g.node());
            let canon = match g.node().tag() {
                TAG_TY => k == ty::CANONICAL,
                TAG_REGION => k == re::CANONICAL,
                TAG_CONST => k == ct::CANONICAL,
                _ => false,
            };
            if canon
                && let Some(var) = vars.get(ix(j))
                && var.universe == Universe::ROOT
                && values[ix(j)] == unset
            {
                values[ix(j)] = *o;
            }
        }
        let base = self.vars.universe();
        for _ in 0..c.max_universe.0 {
            self.vars.create_next_universe();
        }
        for (i, v) in vars.iter().enumerate() {
            if values[i] == unset {
                values[i] = self.fresh_for(*v, Universe(base.0 + v.universe.0));
            }
        }
        let l = self.tys.mk_list(values);
        let inst = self.substitute_canonical(c.value, l).map_err(ResponseError::Fold)?;
        let got = inst.list();
        for (i, o) in (0u32..).zip(orig.iter()) {
            let Some(r) = self.tys.list_get(got, i) else { break };
            self.eq(*o, r, vo).map_err(ResponseError::Relate)?;
        }
        Ok(())
    }
}
