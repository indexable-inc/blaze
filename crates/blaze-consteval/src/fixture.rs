//! A world for the tests and `examples/cost.rs`: items declared by hand into the checker's own
//! test world (`blaze_typeck::fixture::Fx`), their bodies written as Rust text, lowered by
//! blaze-lower, converted (`to_typeck`) and checked by blaze-typeck's `check`, exactly the
//! pipeline the driver runs; then evaluated with a [`Settle`] loop over a store of records.
//!
//! A constant is filed as a fn with no inputs whose output is its type (the checker types a
//! `Def::Const` path by its item's `fn_sig` output). Layout is blaze-build's, one
//! [`Layouts`](crate::Layouts) and ground table kept for every evaluation of the world.

use std::string::String;
use std::vec::Vec as StdVec;

use blaze_ids::{ContentId, Hasher};
use blaze_lower::form::{
    ExprId as LExprId, Field as LField, Param, SegKind, Segment, SelfKind, Shape, TypeId, TypeRef, Variant as LVariant,
};
use blaze_lower::typeck::{Refused, TypeckBody, Types, to_typeck};
use blaze_lower::{Cfg, CfgAnswer, Edition, View, lower_body};
use blaze_own::Alloc;
use blaze_typeck::fixture::Fx;
use blaze_typeck::{
    Adt, AdtKind, Checked, CtorKind, Def, FloatTy, Field, Fetch, InferCtx, IntTy, ItemKey, List, Missing, Mutbl, Name,
    Needs, PNode, PTy, PortableTys, UintTy, Variant, check, ExprId, PatId, Range,
};

use crate::machine::{Answer, Codes, Need, NeedKind, Settle, value_of};
use crate::record::{ConstRecord, RecordKey};
use crate::{ConstError, ConstWorld, FnKind, FnTraitMethod, IntRepr, Intrinsic, LayoutFacts, Layouts, Limits, ReprAttrs, Typed, ValidRange};
use blaze_build::instance_key;
use blaze_typeck::PTys;

struct NoCfg;

impl Cfg for NoCfg {
    fn atom(&mut self, _: &str, _: Option<&str>) -> CfgAnswer {
        CfgAnswer::No
    }
}

/// Written types by name: the primitives and the world's non-generic ADTs; an array's length
/// is an integer literal.
struct Names<'n> {
    adts: &'n [(String, ItemKey)],
}

fn refuse(what: &'static str) -> Refused {
    Refused::Form { what, range: blaze_lower::Range::default() }
}

fn prim(name: &str) -> Option<PNode> {
    Some(match name {
        "bool" => PNode::Bool,
        "char" => PNode::Char,
        "str" => PNode::Str,
        "i8" => PNode::Int(IntTy::I8),
        "i16" => PNode::Int(IntTy::I16),
        "i32" => PNode::Int(IntTy::I32),
        "i64" => PNode::Int(IntTy::I64),
        "i128" => PNode::Int(IntTy::I128),
        "isize" => PNode::Int(IntTy::Isize),
        "u8" => PNode::Uint(UintTy::U8),
        "u16" => PNode::Uint(UintTy::U16),
        "u32" => PNode::Uint(UintTy::U32),
        "u64" => PNode::Uint(UintTy::U64),
        "u128" => PNode::Uint(UintTy::U128),
        "usize" => PNode::Uint(UintTy::Usize),
        "f32" => PNode::Float(FloatTy::F32),
        "f64" => PNode::Float(FloatTy::F64),
        "f16" => PNode::Float(FloatTy::F16),
        "f128" => PNode::Float(FloatTy::F128),
        _ => return None,
    })
}

fn int_text(s: &str) -> Option<u64> {
    let s: String = s.trim().chars().filter(|&c| c != '_').collect();
    let s = s.trim_end_matches("usize");
    s.parse().ok()
}

impl Types for Names<'_> {
    fn ty(&mut self, v: &View<'_>, t: TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        enum Step {
            Visit(TypeId),
            FinishRef(Mutbl),
            FinishPtr(Mutbl),
            FinishTuple(usize),
            FinishArray(u64),
            FinishSlice,
            FinishFnPtr { count: usize, has_ret: bool },
        }
        let mut work = StdVec::new();
        let mut vals: StdVec<PTy> = StdVec::new();
        work.push(Step::Visit(t));
        while let Some(step) = work.pop() {
            match step {
                Step::Visit(cur) => {
                    let leaf = match v.ty(cur)? {
                        TypeRef::Path(p) => {
                            let path = v.path(p)?;
                            let segs = v.segments(path)?;
                            let last = Segment::decode(*segs.last().ok_or(refuse("an empty type path"))?)?;
                            if last.kind != SegKind::Ident {
                                return Err(refuse("a type path segment that is not a name"));
                            }
                            let name = v.name_text(last.name.ok_or(refuse("a nameless segment"))?)?;
                            match prim(name) {
                                Some(n) => n,
                                None => match self.adts.iter().find(|(n, _)| n == name) {
                                    Some(&(_, k)) => PNode::Adt(k, List::EMPTY),
                                    None => return Err(refuse("a type name the fixture does not know")),
                                },
                            }
                        }
                        TypeRef::Ref { mutbl, inner, .. } => {
                            work.push(Step::FinishRef(mutbl));
                            work.push(Step::Visit(inner));
                            continue;
                        }
                        TypeRef::Ptr { mutbl, inner } => {
                            work.push(Step::FinishPtr(mutbl));
                            work.push(Step::Visit(inner));
                            continue;
                        }
                        TypeRef::Tuple(l) => {
                            let ids = v.ids(l)?;
                            work.push(Step::FinishTuple(ids.len()));
                            for &i in ids.iter().rev() {
                                work.push(Step::Visit(TypeId(i)));
                            }
                            continue;
                        }
                        TypeRef::Array { elem, len } => {
                            let r = v.const_range(len)?;
                            let n = int_text(v.text_at(r)?).ok_or(refuse("an array length that is not a literal"))?;
                            work.push(Step::FinishArray(n));
                            work.push(Step::Visit(elem));
                            continue;
                        }
                        TypeRef::Slice(e) => {
                            work.push(Step::FinishSlice);
                            work.push(Step::Visit(e));
                            continue;
                        }
                        TypeRef::Never => PNode::Never,
                        // A safe Rust-ABI fn pointer: the inputs, then the output last (unit when none is
                        // written), as blaze-ladder's front reads one.
                        TypeRef::FnPtr { params, ret, abi: None, unsafety: false, extern_: false, variadic: false } => {
                            let rows = v.rows(params)?;
                            for &n in rows {
                                let p = Param::decode(n)?;
                                if p.ty.is_none() {
                                    return Err(refuse("a fn pointer parameter with no type"));
                                }
                            }
                            let count = rows.len() + usize::from(ret.is_some());
                            work.push(Step::FinishFnPtr { count, has_ret: ret.is_some() });
                            if let Some(r) = ret {
                                work.push(Step::Visit(r));
                            }
                            for &n in rows.iter().rev() {
                                let p = Param::decode(n)?;
                                let pt = p.ty.ok_or(refuse("a fn pointer parameter with no type"))?;
                                work.push(Step::Visit(pt));
                            }
                            continue;
                        }
                        _ => return Err(refuse("a written type the fixture does not resolve")),
                    };
                    vals.push(tys.intern(leaf)?);
                }
                Step::FinishRef(mutbl) => {
                    let inner = vals.pop().ok_or(refuse("type stack underflow"))?;
                    vals.push(tys.intern(PNode::Ref(mutbl, inner))?);
                }
                Step::FinishPtr(mutbl) => {
                    let inner = vals.pop().ok_or(refuse("type stack underflow"))?;
                    vals.push(tys.intern(PNode::Ptr(mutbl, inner))?);
                }
                Step::FinishTuple(len) => {
                    let start = vals.len().checked_sub(len).ok_or(refuse("type stack underflow"))?;
                    let slice = vals.get(start..).ok_or(refuse("type stack underflow"))?;
                    let list = tys.intern_list(slice)?;
                    vals.truncate(start);
                    vals.push(tys.intern(PNode::Tuple(list))?);
                }
                Step::FinishArray(n) => {
                    let elem = vals.pop().ok_or(refuse("type stack underflow"))?;
                    let len = tys.usize_value(n)?;
                    vals.push(tys.intern(PNode::Array(elem, len))?);
                }
                Step::FinishSlice => {
                    let elem = vals.pop().ok_or(refuse("type stack underflow"))?;
                    vals.push(tys.intern(PNode::Slice(elem))?);
                }
                Step::FinishFnPtr { count, has_ret } => {
                    if !has_ret {
                        let unit = tys.intern(PNode::Tuple(List::EMPTY))?;
                        vals.push(unit);
                    }
                    let total = count + usize::from(!has_ret);
                    let start = vals.len().checked_sub(total).ok_or(refuse("type stack underflow"))?;
                    let slice = vals.get(start..).ok_or(refuse("type stack underflow"))?;
                    let list = tys.intern_list(slice)?;
                    vals.truncate(start);
                    let header = blaze_typeck::FnPtrHeader {
                        abi: blaze_typeck::Abi::Rust,
                        safety: blaze_typeck::Safety::Safe,
                        c_variadic: false,
                    };
                    vals.push(tys.intern(PNode::FnPtr(list, header))?);
                }
            }
        }
        vals.pop().ok_or(refuse("type stack underflow"))
    }

    fn repeat_len(&mut self, v: &View<'_>, len: LExprId) -> Result<u64, Refused> {
        int_text(v.text_at(v.expr_range(len)?)?).ok_or(refuse("a repeat length that is not a literal"))
    }

    fn const_arg(&mut self, v: &View<'_>, c: blaze_lower::ConstIdx, _: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        Err(blaze_lower::typeck::no_const_arg(v, c))
    }
}

/// A checked body and what it borrows.
struct Body {
    key: ItemKey,
    text: String,
    hash: ContentId,
    body: TypeckBody<'static>,
    tys: PortableTys<'static>,
    checked: Checked<'static>,
    lits: LitTable,
}

/// A body's literals' written texts, owned; lent as [`crate::Lits`].
#[derive(Default)]
struct LitTable {
    text: String,
    exprs: (StdVec<ExprId>, StdVec<Range>),
    pats: (StdVec<PatId>, StdVec<Range>),
}

impl LitTable {
    /// Every literal of `body` read from `view`, where it was written: the item's text or an
    /// expansion's (`View::text_at`), never at the body's site ranges.
    fn of(view: &View<'_>, body: &TypeckBody<'_>) -> Result<LitTable, Skip> {
        let mut t = LitTable::default();
        let put = |text: &mut String, s: &str| {
            let start = text.len() as u32;
            text.push_str(s);
            Range { start, end: text.len() as u32 }
        };
        for (i, e) in body.exprs.iter().enumerate() {
            if let blaze_typeck::Expr::Lit(_) = e {
                let id = ExprId(i as u32);
                let s = view.expr_range(id).ok().and_then(|r| view.text_at(r).ok()).ok_or(Skip::Open)?;
                let r = put(&mut t.text, s);
                t.exprs.0.push(id);
                t.exprs.1.push(r);
            }
        }
        for (i, p) in body.pats.iter().enumerate() {
            if let blaze_typeck::Pat::Lit(_) | blaze_typeck::Pat::NegLit(_) = p {
                let id = PatId(i as u32);
                let s = view.pat_range(id).ok().and_then(|r| view.text_at(r).ok()).ok_or(Skip::Open)?;
                let r = put(&mut t.text, s);
                t.pats.0.push(id);
                t.pats.1.push(r);
            }
        }
        Ok(t)
    }

    fn lend(&self) -> crate::Lits<'_> {
        crate::Lits { text: &self.text, exprs: (&self.exprs.0, &self.exprs.1), pats: (&self.pats.0, &self.pats.1) }
    }
}

struct AdtRow {
    key: ItemKey,
    kind: AdtKind,
    variants: StdVec<Variant>,
    fields: StdVec<Field>,
}

/// The test world (see the module).
pub struct World {
    pub fx: Fx,
    pub limits: Limits,
    names: StdVec<(String, ItemKey)>,
    adt_names: StdVec<(String, ItemKey)>,
    adts: StdVec<AdtRow>,
    reprs: StdVec<(ItemKey, PNode)>,
    discs: StdVec<((ItemKey, u32), ItemKey)>,
    kinds: StdVec<(ItemKey, FnKind)>,
    texts: StdVec<(ItemKey, String)>,
    bodies: StdVec<Body>,
    store: StdVec<ConstRecord<Alloc<'static>>>,
    /// Lowered body records attached, not yet checked: key, record bytes, item text.
    lowered: StdVec<(ItemKey, StdVec<u8>, String)>,
    anon: u32,
    /// ADTs declared with a `Drop` impl ([`World::drop_impl_on`]).
    drop_impls: StdVec<ItemKey>,
    /// The ADT standing for core's `ManuallyDrop` ([`World::manually_drop_is`]).
    manually_drop: Option<ItemKey>,
    /// The fixture's answer for `FnOnce::call_once`.
    pub(crate) fn_once: FnTraitMethod,
    /// The unit's ground table and layouts, kept between evaluations.
    unit: Option<(PortableTys<'static>, Layouts<Alloc<'static>>, Codes<Alloc<'static>>)>,
    /// The statics declared ([`ConstWorld::is_static`]).
    statics: StdVec<ItemKey>,
    /// The ADTs standing for core's `UnsafeCell` ([`World::unsafe_cell_is`]).
    cells: StdVec<ItemKey>,
}

/// A const or a static.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    Const,
    Static(Mutbl),
}

/// Why a lowered item or body did not join the world.
#[derive(Debug)]
pub enum Skip {
    /// The record does not open, or a row in it is no form of its column.
    Open,
    /// The item has generic parameters: no instantiation is named.
    Generic,
    /// A signature form this world does not declare, named.
    Signature(&'static str),
    /// `to_typeck` refused a written type or a body form.
    Convert(Refused),
    /// The name is declared twice in the world (the census's worlds are per file and flat).
    Duplicate,
    /// The checker failed.
    Check(blaze_typeck::CheckError),
    /// The checker asked the world a question it could not answer.
    WorldMissed,
    /// The checker's first diagnostic and its item-relative range.
    Diag(blaze_typeck::DiagCode, blaze_typeck::Range),
}

impl Default for World {
    fn default() -> World {
        World::new()
    }
}

fn hash_text(text: &str) -> ContentId {
    let mut h = Hasher::new("blaze-consteval fixture text");
    h.update(text.as_bytes());
    h.finish()
}

impl World {
    pub fn new() -> World {
        World {
            fx: Fx::new(),
            limits: Limits::DEFAULT,
            names: StdVec::new(),
            adt_names: StdVec::new(),
            adts: StdVec::new(),
            reprs: StdVec::new(),
            discs: StdVec::new(),
            kinds: StdVec::new(),
            texts: StdVec::new(),
            bodies: StdVec::new(),
            store: StdVec::new(),
            lowered: StdVec::new(),
            anon: 0,
            drop_impls: StdVec::new(),
            manually_drop: None,
            fn_once: FnTraitMethod::Absent,
            unit: None,
            statics: StdVec::new(),
            cells: StdVec::new(),
        }
    }

    /// ADT `adt` has a `Drop` impl (`impl Drop for ADT`; by E0367 an impl for the ADT's
    /// generic parameters, so it applies at every argument).
    pub fn drop_impl_on(&mut self, adt: ItemKey) {
        self.drop_impls.push(adt);
    }

    /// ADT `adt` is core's `ManuallyDrop` (`#[lang = "manually_drop"]`) in this world.
    pub fn manually_drop_is(&mut self, adt: ItemKey) {
        self.manually_drop = Some(adt);
    }

    /// ADT `adt` is core's `UnsafeCell` (`#[lang = "unsafe_cell"]`) in this world: its repr
    /// hides its field's niche, and a type holding it by value is not `Freeze`.
    pub fn unsafe_cell_is(&mut self, adt: ItemKey) {
        self.cells.push(adt);
    }

    /// Item `item` is `FnOnce::call_once` in this world ([`ConstWorld::fn_trait_method`]).
    pub fn fn_once_is(&mut self, item: ItemKey) {
        self.fn_once = FnTraitMethod::Found(item);
    }

    /// The item declared as `name`.
    pub fn key(&self, name: &str) -> ItemKey {
        self.names.iter().find(|(n, _)| n == name).map(|&(_, k)| k).unwrap_or_else(|| panic!("no item `{name}`"))
    }

    /// The type written `text` (`u8`, `[i32; 4]`, `&[u8]`, a declared ADT) in the world's table.
    pub fn ty(&mut self, text: &str) -> PTy {
        let alloc = blaze_own::testing::heap().alloc();
        // A written type is lowered in a body: a `let`'s.
        let probe = std::format!("const T: () = {{ let _x: {text}; }};");
        let lowered = lower_body(&probe, Edition::E2024, &mut NoCfg, &mut blaze_lower::NoModule, alloc).unwrap_or_else(|_| panic!("`{text}`: cfg"));
        let view = lowered.view();
        let mut tys = PortableTys::new_in(alloc);
        let body = to_typeck(&view, ItemKey(0), &mut tys, &mut Names { adts: &self.adt_names }, alloc)
            .unwrap_or_else(|e| panic!("`{text}`: {e:?}"));
        let t = body
            .stmts
            .iter()
            .find_map(|s| match s {
                blaze_typeck::Stmt::Let { ty, .. } => *ty,
                _ => None,
            })
            .unwrap_or_else(|| panic!("`{text}`: no written type"));
        self.import(tys.view(), t)
    }

    /// `t` of table `from` interned into the world's table.
    fn import(&mut self, from: blaze_typeck::PTys<'_>, t: PTy) -> PTy {
        enum Step {
            Visit(PTy),
            Finish { node: PNode, list_len: usize },
        }
        let mut work = StdVec::new();
        let mut vals = StdVec::new();
        work.push(Step::Visit(t));
        while let Some(step) = work.pop() {
            match step {
                Step::Visit(cur) => {
                    let n = from.node(cur).expect("node");
                    match n {
                        PNode::Ref(_, i) | PNode::Ptr(_, i) | PNode::Slice(i) | PNode::ConstValue { ty: i, .. } => {
                            work.push(Step::Finish { node: n, list_len: 0 });
                            work.push(Step::Visit(i));
                        }
                        PNode::Array(i, k) => {
                            work.push(Step::Finish { node: n, list_len: 0 });
                            work.push(Step::Visit(k));
                            work.push(Step::Visit(i));
                        }
                        // The inputs and output, each imported: the list indexes `from`, not the world.
                        PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::FnPtr(l, _) => {
                            let ts = from.list(l).expect("list");
                            work.push(Step::Finish { node: n, list_len: ts.len() });
                            for &x in ts.iter().rev() {
                                work.push(Step::Visit(x));
                            }
                        }
                        other => {
                            let interned = self.fx.ty(other);
                            vals.push(interned);
                        }
                    }
                }
                Step::Finish { node, list_len } => {
                    let built = match node {
                        PNode::Ref(m, _) => {
                            let Some(i) = vals.pop() else { return t };
                            PNode::Ref(m, i)
                        }
                        PNode::Ptr(m, _) => {
                            let Some(i) = vals.pop() else { return t };
                            PNode::Ptr(m, i)
                        }
                        PNode::Slice(_) => {
                            let Some(i) = vals.pop() else { return t };
                            PNode::Slice(i)
                        }
                        PNode::ConstValue { bits, .. } => {
                            let Some(ty) = vals.pop() else { return t };
                            PNode::ConstValue { ty, bits }
                        }
                        PNode::Array(_, _) => {
                            let (Some(k), Some(i)) = (vals.pop(), vals.pop()) else { return t };
                            PNode::Array(i, k)
                        }
                        PNode::Tuple(_) | PNode::Record(_, _) | PNode::Union(_) | PNode::FnPtr(_, _) => {
                            let Some(start) = vals.len().checked_sub(list_len) else { return t };
                            let Some(slice) = vals.get(start..) else { return t };
                            let list = self.fx.list(slice);
                            vals.truncate(start);
                            match node {
                                PNode::Record(s, _) => PNode::Record(s, list),
                                PNode::Union(_) => PNode::Union(list),
                                PNode::FnPtr(_, header) => PNode::FnPtr(list, header),
                                _ => PNode::Tuple(list),
                            }
                        }
                        other => other,
                    };
                    let interned = self.fx.ty(built);
                    vals.push(interned);
                }
            }
        }
        vals.pop().unwrap_or(t)
    }

    fn declare(&mut self, name: &str, k: ItemKey) {
        self.names.push((String::from(name), k));
    }

    /// `const NAME: TY = BODY;`
    pub fn konst(&mut self, name: &str, ty: &str, body: &str) -> ItemKey {
        let t = self.ty(ty);
        let k = self.fx.func(&std::format!("const {name}"), 0, &[], t, StdVec::new());
        self.fx.use_path(name, Def::Const(k));
        self.declare(name, k);
        self.kinds.push((k, FnKind::NotConst));
        self.texts.push((k, std::format!("const {name}: {ty} = {body};")));
        k
    }

    /// `const NAME: TY = BODY;` as a member of an `impl<const PARAM: PARAM_TY>`: the constant
    /// has one generic parameter, a const one, at index 0, which `PARAM` in the body names
    /// (`const LAP: usize = CAP + 1;` of `impl<T, const CAP: usize>`, with the type parameter
    /// left out). Evaluated at an argument with [`World::eval_at`].
    pub fn generic_konst(&mut self, name: &str, ty: &str, param: (&str, &str), body: &str) -> ItemKey {
        let t = self.ty(ty);
        let pt = self.ty(param.1);
        let k = self.fx.func(&std::format!("const {name}"), 1, &[], t, StdVec::new());
        self.fx.const_param(k, 0, pt);
        self.fx.use_path(param.0, Def::TyParam(0));
        self.fx.use_path(name, Def::Const(k));
        self.declare(name, k);
        self.kinds.push((k, FnKind::NotConst));
        self.texts.push((k, std::format!("const {name}: {ty} = {body};")));
        k
    }

    /// `static NAME: TY = BODY;`
    pub fn stat(&mut self, name: &str, ty: &str, body: &str) -> ItemKey {
        let t = self.ty(ty);
        let k = self.fx.func(&std::format!("static {name}"), 0, &[], t, StdVec::new());
        self.fx.use_path(name, Def::Static(k, Mutbl::Not));
        self.declare(name, k);
        self.statics.push(k);
        self.kinds.push((k, FnKind::NotConst));
        self.texts.push((k, std::format!("static {name}: {ty} = {body};")));
        k
    }

    /// `static mut NAME: TY = BODY;`
    pub fn stat_mut(&mut self, name: &str, ty: &str, body: &str) -> ItemKey {
        let t = self.ty(ty);
        let k = self.fx.func(&std::format!("static mut {name}"), 0, &[], t, StdVec::new());
        self.fx.use_path(name, Def::Static(k, Mutbl::Mut));
        self.declare(name, k);
        self.statics.push(k);
        self.kinds.push((k, FnKind::NotConst));
        self.texts.push((k, std::format!("static mut {name}: {ty} = {body};")));
        k
    }

    /// `const fn NAME(PARAMS) -> OUT BODY`, `params` as `(name, type)`.
    pub fn const_fn(&mut self, name: &str, params: &[(&str, &str)], out: &str, body: &str) -> ItemKey {
        self.any_fn(name, params, out, body, FnKind::Const)
    }

    /// `fn NAME(PARAMS) -> OUT BODY`: not callable in a constant.
    pub fn plain_fn(&mut self, name: &str, params: &[(&str, &str)], out: &str, body: &str) -> ItemKey {
        self.any_fn(name, params, out, body, FnKind::NotConst)
    }

    fn any_fn(&mut self, name: &str, params: &[(&str, &str)], out: &str, body: &str, kind: FnKind) -> ItemKey {
        let inputs: StdVec<PTy> = params.iter().map(|&(_, t)| self.ty(t)).collect();
        let o = self.ty(out);
        let k = self.fx.func(name, 0, &inputs, o, StdVec::new());
        self.declare(name, k);
        self.kinds.push((k, kind));
        let ps: StdVec<String> = params.iter().map(|&(n, t)| std::format!("{n}: {t}")).collect();
        let c = if kind == FnKind::Const { "const " } else { "" };
        self.texts.push((k, std::format!("{c}fn {name}({}) -> {out} {body}", ps.join(", "))));
        k
    }

    /// `const fn NAME(PARAMS) -> OUT BODY` whose kind the world does not hold yet
    /// (`FnKind::Missing(fact)`): a call of it is refused as `Refusal::FnFact`.
    pub fn unread_fn(&mut self, name: &str, params: &[(&str, &str)], out: &str, body: &str, fact: crate::Fact) -> ItemKey {
        let k = self.any_fn(name, params, out, body, FnKind::Const);
        self.set_kind(name, FnKind::Missing(fact));
        k
    }

    /// The world now answers `name`'s kind as `kind` (a fact read after a first evaluation).
    pub fn set_kind(&mut self, name: &str, kind: FnKind) {
        let k = self.key(name);
        match self.kinds.iter_mut().find(|(x, _)| *x == k) {
            Some(row) => row.1 = kind,
            None => self.kinds.push((k, kind)),
        }
    }

    /// An intrinsic `name` with `own` type parameters (`size_of::<T>()`), its inputs and output.
    pub fn intrinsic(&mut self, name: &str, own: u32, inputs: &[PTy], out: PTy, what: Intrinsic) -> ItemKey {
        let k = self.fx.func(name, own, inputs, out, StdVec::new());
        self.declare(name, k);
        self.kinds.push((k, FnKind::Intrinsic(what)));
        k
    }

    /// A `#[rustc_intrinsic]` fn `name` the table does not hold (`FnKind::UnknownIntrinsic`).
    pub fn unknown_intrinsic(&mut self, name: &str, own: u32, inputs: &[PTy], out: PTy) -> ItemKey {
        let k = self.fx.func(name, own, inputs, out, StdVec::new());
        self.declare(name, k);
        self.kinds.push((k, FnKind::UnknownIntrinsic));
        k
    }

    /// A panic entry `name(msg: &str) -> !` (core's `panic_display` and kin, which `panic!`
    /// expands to).
    pub fn panic_fn(&mut self, name: &str, arg: &str) -> ItemKey {
        let a = self.ty(arg);
        let never = self.fx.ty(PNode::Never);
        let k = self.fx.func(name, 0, &[a], never, StdVec::new());
        self.declare(name, k);
        self.kinds.push((k, FnKind::Panic));
        k
    }

    /// `struct NAME { fields }`, fields as `(name, type)`.
    pub fn strukt(&mut self, name: &str, fields: &[(&str, &str)]) -> ItemKey {
        let tys: StdVec<(&str, PTy)> = fields.iter().map(|&(n, t)| (n, self.ty(t))).collect();
        self.strukt_typed(name, &tys)
    }

    fn strukt_typed(&mut self, name: &str, tys: &[(&str, PTy)]) -> ItemKey {
        let k = self.fx.strukt(name, 0, tys);
        let fs: StdVec<Field> = tys.iter().map(|&(n, ty)| Field { name: Name::of(n), ty }).collect();
        let v = Variant { key: k, name: Name::of(name), ctor: CtorKind::Braces, fields: List { start: 0, len: fs.len() as u32 } };
        self.adts.push(AdtRow { key: k, kind: AdtKind::Struct, variants: std::vec![v], fields: fs });
        self.adt_names.push((String::from(name), k));
        self.declare(name, k);
        k
    }

    /// `#[repr(REPR)] enum NAME { V(fields) = DISC, .. }`: each variant `(name, field types,
    /// explicit discriminant text)`.
    pub fn enum_(&mut self, name: &str, repr: Option<&str>, variants: &[(&str, &[&str], Option<&str>)]) -> ItemKey {
        let mut tys: StdVec<StdVec<PTy>> = StdVec::new();
        for (_, fs, _) in variants {
            tys.push(fs.iter().map(|t| self.ty(t)).collect());
        }
        let shaped: StdVec<(&str, StdVec<PTy>)> = variants.iter().zip(tys).map(|(&(n, _, _), t)| (n, t)).collect();
        let repr_node = repr.map(|r| prim(r).unwrap_or_else(|| panic!("repr `{r}`")));
        let (k, _) = self.enum_typed(name, repr_node, &shaped);
        for (i, &(vname, _, disc)) in variants.iter().enumerate() {
            if let Some(d) = disc {
                let rt = repr.unwrap_or("isize");
                let dk = self.konst(&std::format!("{name}_{vname}_DISC"), rt, d);
                self.discs.push(((k, i as u32), dk));
            }
        }
        k
    }

    /// An enum of variants `(name, field types)` (tuple variants), its `repr` integer.
    fn enum_typed(&mut self, name: &str, repr: Option<PNode>, variants: &[(&str, StdVec<PTy>)]) -> (ItemKey, StdVec<ItemKey>) {
        let shaped: StdVec<(&str, &[PTy])> = variants.iter().map(|(n, t)| (*n, t.as_slice())).collect();
        let (k, vkeys) = self.fx.enum_(name, 0, &shaped);
        let mut vs = StdVec::new();
        let mut fields = StdVec::new();
        for (i, (vname, ts)) in variants.iter().enumerate() {
            let start = fields.len() as u32;
            for (j, &ty) in ts.iter().enumerate() {
                fields.push(Field { name: Name::of_index(j as u32), ty });
            }
            let ctor = if ts.is_empty() { CtorKind::Unit } else { CtorKind::Tuple };
            vs.push(Variant { key: vkeys[i], name: Name::of(vname), ctor, fields: List { start, len: ts.len() as u32 } });
            if !ts.is_empty() {
                self.kinds.push((vkeys[i], FnKind::Ctor { adt: k, variant: i as u32 }));
            }
        }
        if let Some(r) = repr {
            self.reprs.push((k, r));
        }
        self.adts.push(AdtRow { key: k, kind: AdtKind::Enum, variants: vs, fields });
        self.adt_names.push((String::from(name), k));
        self.declare(name, k);
        (k, vkeys)
    }

    /// Lowers, converts and checks every body declared; panics naming the first that fails.
    pub fn build(&mut self) {
        if let Some((key, why)) = self.try_build().into_iter().next() {
            panic!("body {:#x}: {why:?}", key.0);
        }
    }

    /// Converts and checks every body declared or attached: the bodies that failed, each with
    /// its cause; the rest join the world.
    pub fn try_build(&mut self) -> StdVec<(ItemKey, Skip)> {
        self.fx.finish();
        let alloc = blaze_own::testing::heap().alloc();
        let mut failed = StdVec::new();
        for (key, text) in core::mem::take(&mut self.texts) {
            let lowered = lower_body(&text, Edition::E2024, &mut NoCfg, &mut blaze_lower::NoModule, alloc).unwrap_or_else(|_| panic!("{text}: cfg"));
            if let Err(why) = self.check_body(key, &lowered.view()) {
                failed.push((key, why));
            }
        }
        for (key, bytes, text) in core::mem::take(&mut self.lowered) {
            let Ok(view) = blaze_lower::open(&bytes, &text) else {
                failed.push((key, Skip::Open));
                continue;
            };
            if let Err(why) = self.check_body(key, &view) {
                failed.push((key, why));
            }
        }
        failed
    }

    fn check_body(&mut self, key: ItemKey, view: &View<'_>) -> Result<(), Skip> {
        let text = String::from(view.item());
        let alloc = blaze_own::testing::heap().alloc();
        let mut tys = PortableTys::new_in(alloc);
        let body = to_typeck(view, key, &mut tys, &mut Names { adts: &self.adt_names }, alloc).map_err(Skip::Convert)?;
        let lits = LitTable::of(view, &body)?;
        let checked = {
            let bv = body.view(tys.view());
            let mut cx = InferCtx::new_in(alloc);
            match check(&bv, &self.fx, &mut cx, &Needs::new_in(alloc), alloc) {
                Ok(blaze_typeck::Outcome::Done(c)) => c,
                Ok(blaze_typeck::Outcome::Need(_)) => return Err(Skip::WorldMissed),
                Err(e) => return Err(Skip::Check(e)),
            }
        };
        if let Some(d) = checked.diags.first() {
            return Err(Skip::Diag(d.code, d.at));
        }
        self.bodies.push(Body { key, hash: hash_text(&text), text, body, tys, checked, lits });
        Ok(())
    }

    /// A type written in a lowered record, in the world's table.
    fn written(&mut self, v: &View<'_>, t: TypeId) -> Result<PTy, Skip> {
        let mut tys = PortableTys::new_in(blaze_own::testing::heap().alloc());
        let p = Names { adts: &self.adt_names }.ty(v, t, &mut tys).map_err(Skip::Convert)?;
        Ok(self.import(tys.view(), p))
    }

    fn fresh(&self, name: &str) -> Result<(), Skip> {
        if self.names.iter().any(|(n, _)| n == name) { Err(Skip::Duplicate) } else { Ok(()) }
    }

    /// Declares the const or static whose lowered signature is `sig`: its value is attached
    /// later ([`World::attach`]).
    pub fn declare_value(&mut self, value: Value, name: &str, sig: &View<'_>) -> Result<ItemKey, Skip> {
        if sig.list(blaze_lower::head::GENERICS).len != 0 {
            return Err(Skip::Generic);
        }
        // `const _` (the Reference, "Constant items": "Unnamed constant") names nothing: it is
        // never a duplicate and no path reaches it. The outline gives it no name ("").
        let named = !matches!(name, "_" | "");
        if named {
            self.fresh(name)?;
        }
        let t = sig.head_opt(blaze_lower::head::TY).ok_or(Skip::Signature("a const or static with no written type"))?;
        let t = self.written(sig, TypeId(t))?;
        let (label, def) = match value {
            Value::Const => ("const", None),
            Value::Static(m) => ("static", Some(m)),
        };
        let k = self.fx.func(&std::format!("{label} {name}"), 0, &[], t, StdVec::new());
        if def.is_some() {
            self.statics.push(k);
        }
        if named {
            self.fx.use_path(name, def.map_or(Def::Const(k), |m| Def::Static(k, m)));
            self.declare(name, k);
        }
        self.kinds.push((k, FnKind::NotConst));
        Ok(k)
    }

    /// An anonymous constant of type `ty` (an array length: `usize`), its value attached later.
    pub fn declare_anon(&mut self, ty: PNode) -> ItemKey {
        let t = self.fx.ty(ty);
        let n = self.anon;
        self.anon += 1;
        let k = self.fx.func(&std::format!("anon const {n}"), 0, &[], t, StdVec::new());
        self.kinds.push((k, FnKind::NotConst));
        k
    }

    /// Declares the fn whose lowered signature is `sig`; `konst`: a `const fn`.
    pub fn declare_fn(&mut self, name: &str, sig: &View<'_>) -> Result<ItemKey, Skip> {
        if sig.list(blaze_lower::head::GENERICS).len != 0 {
            return Err(Skip::Generic);
        }
        self.fresh(name)?;
        let mut inputs = StdVec::new();
        for &row in sig.rows(sig.list(blaze_lower::head::PARAMS)).map_err(|_| Skip::Open)? {
            let p = Param::decode(row).map_err(|_| Skip::Open)?;
            if p.self_kind != SelfKind::No {
                return Err(Skip::Signature("a `self` parameter"));
            }
            let t = p.ty.ok_or(Skip::Signature("a parameter with no type"))?;
            inputs.push(self.written(sig, t)?);
        }
        let out = match sig.head_opt(blaze_lower::head::TY) {
            Some(t) => self.written(sig, TypeId(t))?,
            None => {
                let unit = self.fx.list(&[]);
                self.fx.ty(PNode::Tuple(unit))
            }
        };
        let konst = sig.head[blaze_lower::head::FLAGS] & blaze_lower::machine::sig::CONST != 0;
        let k = self.fx.func(name, 0, &inputs, out, StdVec::new());
        self.declare(name, k);
        self.kinds.push((k, if konst { FnKind::Const } else { FnKind::NotConst }));
        Ok(k)
    }

    /// Declares the struct or enum whose lowered signature is `sig`: for an enum, each
    /// explicit discriminant's range in the item's text and the constant declared for it, whose
    /// value is attached later.
    pub fn declare_adt(&mut self, name: &str, is_enum: bool, sig: &View<'_>) -> Result<StdVec<(blaze_lower::Range, ItemKey)>, Skip> {
        if sig.list(blaze_lower::head::GENERICS).len != 0 {
            return Err(Skip::Generic);
        }
        self.fresh(name)?;
        let bad = |_| Skip::Open;
        let rows = sig.rows(sig.list(blaze_lower::head::FIELDS)).map_err(bad)?;
        if !is_enum {
            let mut fs: StdVec<(String, PTy)> = StdVec::new();
            for (i, &row) in rows.iter().enumerate() {
                let f = LField::decode(row).map_err(bad)?;
                let n = match f.name {
                    Some(n) => String::from(sig.name_text(n).map_err(|_| Skip::Open)?),
                    None => std::format!("{i}"),
                };
                fs.push((n, self.written(sig, f.ty)?));
            }
            let fs: StdVec<(&str, PTy)> = fs.iter().map(|(n, t)| (n.as_str(), *t)).collect();
            self.strukt_typed(name, &fs);
            return Ok(StdVec::new());
        }
        let repr = match sig.head_opt(blaze_lower::head::REPR) {
            None => None,
            Some(r) => {
                let a = sig.attr(sig.row(blaze_lower::form::RowIdx(r)).map_err(bad)?).map_err(bad)?;
                let args = sig.text_at(a.args).map_err(|_| Skip::Open)?;
                let inner = args.trim().trim_start_matches('(').trim_end_matches(')').trim();
                Some(prim(inner).ok_or(Skip::Signature("a `repr` that is not one integer type"))?)
            }
        };
        let mut variants: StdVec<(&str, StdVec<PTy>)> = StdVec::new();
        let mut discs = StdVec::new();
        for &row in rows {
            let v = LVariant::decode(row).map_err(bad)?;
            if v.shape == Shape::Record {
                return Err(Skip::Signature("a variant with named fields"));
            }
            let mut ts = StdVec::new();
            for &f in sig.rows(v.fields).map_err(bad)? {
                ts.push(self.written(sig, LField::decode(f).map_err(bad)?.ty)?);
            }
            variants.push((sig.name_text(v.name).map_err(|_| Skip::Open)?, ts));
            discs.push(v.discriminant.map(|c| sig.const_range(c)));
        }
        let (k, _) = self.enum_typed(name, repr, &variants);
        let disc_ty = repr.unwrap_or(PNode::Int(IntTy::Isize));
        let mut out = StdVec::new();
        for (i, d) in discs.into_iter().enumerate() {
            let Some(r) = d else { continue };
            // The range is given to `lower_anon_const` with the item's text: one of it, never of
            // an expansion (a signature expands nothing in place).
            let r = sig.item_relative(r.map_err(bad)?).ok_or(Skip::Signature("a discriminant made by a macro expansion"))?;
            let dk = self.declare_anon(disc_ty);
            self.discs.push(((k, i as u32), dk));
            out.push((r, dk));
        }
        Ok(out)
    }

    /// Attaches the lowered body record `bytes` of item text `text` as `key`'s value, checked
    /// by the next [`World::try_build`].
    pub fn attach(&mut self, key: ItemKey, bytes: &[u8], text: &str) {
        self.lowered.push((key, bytes.to_vec(), String::from(text)));
    }

    /// Evaluates `name` and everything it needs, storing each record: its record, or the
    /// refusal.
    pub fn eval(&mut self, name: &str) -> Result<&ConstRecord<Alloc<'static>>, ConstError<'static>> {
        let item = self.key(name);
        self.eval_item(item)
    }

    pub fn eval_item(&mut self, item: ItemKey) -> Result<&ConstRecord<Alloc<'static>>, ConstError<'static>> {
        self.eval_item_at(item, &[])
    }

    /// Evaluates `name`, a constant with const generic parameters of type `usize` only
    /// ([`World::generic_konst`]), at the arguments `usizes`, in order.
    pub fn eval_at(&mut self, name: &str, usizes: &[u64]) -> Result<&ConstRecord<Alloc<'static>>, ConstError<'static>> {
        let item = self.key(name);
        self.eval_item_at(item, usizes)
    }

    fn eval_item_at(&mut self, item: ItemKey, usizes: &[u64]) -> Result<&ConstRecord<Alloc<'static>>, ConstError<'static>> {
        let alloc = blaze_own::testing::heap().alloc();
        // The unit's ground table and layouts: one for every evaluation of the world.
        let (mut ground, mut layout, mut codes) =
            self.unit.take().unwrap_or_else(|| (PortableTys::new_in(alloc), Layouts::new_in(alloc), Codes::new_in(alloc)));
        let values: StdVec<PTy> = usizes.iter().map(|&n| ground.usize_value(n).expect("a const argument in the ground table")).collect();
        let args = ground.intern_list(&values).expect("the arguments in the ground table");
        let mut settle = Settle::new(Need { kind: NeedKind::Value, item, args }, 64, alloc);
        let mut last = None;
        while let Some(n) = settle.next() {
            let answer = value_of(&*self, &mut layout, &mut codes, n.item, n.args, &mut ground, self.limits, alloc);
            let step = match answer {
                Ok(Answer::Done(rec)) => {
                    let key = rec.key;
                    if !self.store.iter().any(|r| r.key == key) {
                        self.store.push(rec);
                    }
                    last = Some(key);
                    settle.done();
                    Ok(())
                }
                Ok(Answer::Need(m)) => settle.need(m),
                Err(e) => Err(e),
            };
            if let Err(e) = step {
                self.unit = Some((ground, layout, codes));
                return Err(e);
            }
        }
        self.unit = Some((ground, layout, codes));
        let key = last.expect("settled");
        Ok(self.store.iter().find(|r| r.key == key).expect("stored"))
    }

    /// Constant `name`'s record as code the builder emits reads it back (blaze-build's
    /// `read_back`): its record placed in memory, every scalar leaf of `ty` (a type of this
    /// world's table) loaded through the unit's `Layouts`, depth first.
    #[cfg(test)]
    pub fn read_back(&mut self, name: &str, ty: PTy) -> Result<StdVec<u64>, blaze_build::read_back::ReadBackError> {
        let alloc = blaze_own::testing::heap().alloc();
        let key = self.eval(name).unwrap_or_else(|e| panic!("{name}: {e}")).key;
        let (mut ground, mut layouts, codes) = self.unit.take().expect("an evaluation made the unit");
        let g = blaze_build::ground(self.fx.t.view(), ty, &[], &mut ground, alloc, &*self).expect("a ground type");
        let got = {
            let rec = self.store.iter().find(|r| r.key == key).expect("stored");
            let mut cx = blaze_build::layout::LayoutCx { ground: &mut ground, facts: &*self, scratch: alloc };
            blaze_build::read_back::read_back(g, &rec.data, &mut layouts, &mut cx)
        };
        self.unit = Some((ground, layouts, codes));
        got.map(|v| v.into_iter().collect())
    }

    /// Forgets every record made (the next evaluation makes them again).
    pub fn clear_records(&mut self) {
        self.store.clear();
        // The layouts' proofs rest on records: a new unit.
        self.unit = None;
    }

    /// The records made so far.
    pub fn records(&self) -> usize {
        self.store.len()
    }
}

/// The root's bytes as a little-endian integer (at most 16 bytes).
pub fn int_of(r: &ConstRecord<Alloc<'_>>) -> u128 {
    let mut v = 0u128;
    for (k, &b) in r.data.allocs[0].bytes.iter().enumerate().take(16) {
        v |= u128::from(b) << (8 * k);
    }
    v
}

impl LayoutFacts for World {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.adts.iter().find(|a| a.key == key).map(|a| Adt {
            kind: a.kind,
            generics: 0,
            variants: &a.variants,
            fields: &a.fields,
            types: self.fx.t.view(),
        })
    }

    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, blaze_build::layout::Missing> {
        let int = self.reprs.iter().find(|(k, _)| *k == key).map(|&(_, n)| match n {
            PNode::Int(i) => IntRepr::Signed(i),
            PNode::Uint(u) => IntRepr::Unsigned(u),
            _ => panic!("a repr that is not an integer"),
        });
        Ok(ReprAttrs { int, hide_niche: self.cells.contains(&key), ..ReprAttrs::RUST })
    }

    /// From the explicit discriminant's record (the machine makes it a need before it asks).
    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, blaze_build::layout::Missing> {
        let Some(d) = self.explicit_discriminant(key, variant) else { return Ok(None) };
        // The value read is the discriminant's: its item is the need.
        let missing = blaze_build::layout::Missing { item: d };
        let ik = instance_key(self.code_id(d), List::EMPTY, PTys::EMPTY, blaze_own::testing::heap().alloc()).map_err(|_| missing)?;
        let r = self.store.iter().find(|r| r.key == RecordKey::of(ik)).ok_or(missing)?;
        let bits = 8 * r.data.allocs[0].bytes.len() as u32;
        let v = int_of(r);
        let signed = !matches!(self.repr(key)?.int, Some(IntRepr::Unsigned(_)));
        Ok(Some(if signed && bits < 128 { ((v << (128 - bits)) as i128) >> (128 - bits) } else { v as i128 }))
    }

    fn valid_range(&self, _: ItemKey) -> Result<ValidRange, blaze_build::layout::Missing> {
        Ok(ValidRange::default())
    }
}

impl ConstWorld for World {
    fn body(&self, item: ItemKey) -> Fetch<Option<Typed<'_>>> {
        Ok(self.bodies.iter().find(|b| b.key == item).map(|b| Typed::new(b.body.view(b.tys.view()), &b.checked, &b.text, b.lits.lend(), b.hash)))
    }

    fn fn_kind(&self, item: ItemKey) -> FnKind {
        self.kinds.iter().find(|(k, _)| *k == item).map_or(FnKind::NotConst, |&(_, f)| f)
    }

    fn code_id(&self, item: ItemKey) -> ContentId {
        let mut h = Hasher::new("blaze-consteval fixture item");
        h.update(&item.0.to_le_bytes());
        if let Some(b) = self.bodies.iter().find(|b| b.key == item) {
            h.id(&b.hash);
        }
        h.finish()
    }

    fn is_static(&self, item: ItemKey) -> bool {
        self.statics.contains(&item)
    }

    fn record(&self, key: RecordKey) -> Fetch<&ConstRecord<Alloc<'_>>> {
        self.store.iter().find(|r| r.key == key).ok_or(Missing)
    }

    fn explicit_discriminant(&self, adt: ItemKey, variant: u32) -> Option<ItemKey> {
        self.discs.iter().find(|(k, _)| *k == (adt, variant)).map(|&(_, d)| d)
    }

    /// The fixture builds no `drop` fn: the ADT's own key stands for it (only whether an impl
    /// exists is read).
    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Option<Result<Option<ItemKey>, blaze_build::BuildError>> {
        let PNode::Adt(k, _) = ground.view().node(ty).ok()? else {
            return Some(Err(blaze_build::BuildError::Malformed { what: "a `Drop` impl asked of a type that is not an ADT" }));
        };
        Some(Ok(self.drop_impls.contains(&k).then_some(k)))
    }

    fn manually_drop(&self) -> Option<Option<ItemKey>> {
        Some(self.manually_drop)
    }

    fn fn_trait_method(&self, which: blaze_build::FnTrait) -> FnTraitMethod {
        match which {
            blaze_build::FnTrait::FnOnce => self.fn_once,
            blaze_build::FnTrait::Fn | blaze_build::FnTrait::FnMut => FnTraitMethod::Absent,
        }
    }
}
