//! Printing terms as Rust-like text through a small writer, iteratively: no `core::fmt`
//! machinery on the hot path and no recursion however deep the term.
//!
//! Reference for the forms: rustc_middle/src/ty/print/pretty.rs (cut to what a diagnostic
//! needs; item names come from the caller, which knows them, through [`Names`]).

use blaze_id::ItemKey;
use blaze_own::Vec;

use crate::ids::{Arg, Def, List, Mutability, Node, TAG_CONST, TAG_LIST, TAG_REGION, TAG_TY};
use crate::tys::{AliasKind, ConstKind, PredKind, RegionKind, TyKind, Tys};

/// Where text goes.
pub trait Out {
    fn str(&mut self, s: &str);
}

/// Names of items and parameters, from the caller.
pub trait Names {
    /// An item's path. By default `item_` and the first four bytes of its key in hex.
    fn item(&mut self, key: ItemKey, out: &mut dyn Out) {
        out.str("item_");
        for b in &key.0.0[..4] {
            out.str(hex(b >> 4));
            out.str(hex(b & 15));
        }
    }

    /// A type parameter's name. By default `T` and its index.
    fn ty_param(&mut self, index: u32, out: &mut dyn Out) {
        out.str("T");
        decimal(u128::from(index), out);
    }

    /// An early-bound region parameter's name, without the `'`. By default `r` and its index.
    fn region_param(&mut self, index: u32, out: &mut dyn Out) {
        out.str("r");
        decimal(u128::from(index), out);
    }

    /// A const parameter's name. By default `N` and its index.
    fn const_param(&mut self, index: u32, out: &mut dyn Out) {
        out.str("N");
        decimal(u128::from(index), out);
    }
}

/// [`Names`] with every default.
pub struct DefaultNames;

impl Names for DefaultNames {}

const DIGITS: &str = "0123456789abcdef";

fn hex(d: u8) -> &'static str {
    let i = usize::from(d & 15);
    &DIGITS[i..i + 1]
}

/// `v` in decimal.
pub fn decimal(mut v: u128, out: &mut dyn Out) {
    let mut buf = [0u8; 39];
    let mut n = buf.len();
    loop {
        n -= 1;
        // `v % 10` is under 10.
        buf[n] = b'0' + blaze_int::Int::truncate_to::<u8>(v % 10);
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for &d in &buf[n..] {
        out.str(hex(d - b'0'));
    }
}

#[derive(Clone, Copy)]
enum Item {
    Node(Node),
    Text(&'static str),
    Num(u128),
    Def(Def),
    TyParam(u32),
    RegionParam(u32),
    ConstParam(u32),
}

struct Printer<'t, 'a> {
    tys: &'t Tys<'a>,
    stack: Vec<Item, blaze_own::Alloc<'a>>,
}

impl Printer<'_, '_> {
    fn push(&mut self, i: Item) {
        self.stack.push(i);
    }

    /// Pushes, to come out in this order, `items` (pushed in reverse).
    fn seq(&mut self, items: &[Item]) {
        for &i in items.iter().rev() {
            self.stack.push(i);
        }
    }

    /// Pushes a list's elements `from..to` with `sep` between them.
    fn list(&mut self, l: List, from: u32, to: u32, sep: &'static str) {
        let mut i = to;
        while i > from {
            i -= 1;
            if let Some(a) = self.tys.list_get(l, i) {
                self.push(Item::Node(a.node()));
            }
            if i > from {
                self.push(Item::Text(sep));
            }
        }
    }

    /// `<args>` if there are any.
    fn args(&mut self, l: List, from: u32) {
        let len = self.tys.list_len(l);
        if len > from {
            self.push(Item::Text(">"));
            self.list(l, from, len, ", ");
            self.push(Item::Text("<"));
        }
    }

    /// `name<args>`, pushed so it comes out next.
    fn path(&mut self, d: Def, l: List, from: u32) {
        self.args(l, from);
        self.push(Item::Def(d));
    }

    fn ty(&mut self, n: Node) {
        let t = n.ty();
        use Item::{Node as N, Num, Text};
        match self.tys.ty_kind(t) {
            TyKind::Bool => self.push(Text("bool")),
            TyKind::Char => self.push(Text("char")),
            TyKind::Int(i) => self.push(Text(["isize", "i8", "i16", "i32", "i64", "i128"][crate::ids::ix(i.code())])),
            TyKind::Uint(u) => self.push(Text(["usize", "u8", "u16", "u32", "u64", "u128"][crate::ids::ix(u.code())])),
            TyKind::Float(f) => self.push(Text(["f16", "f32", "f64", "f128"][crate::ids::ix(f.code())])),
            TyKind::Str => self.push(Text("str")),
            TyKind::Never => self.push(Text("!")),
            TyKind::Adt(d, l) | TyKind::FnDef(d, l) => self.path(d, l, 0),
            TyKind::Foreign(d) => self.push(Item::Def(d)),
            TyKind::Array(e, c) => self.seq(&[Text("["), N(e.node()), Text("; "), N(c.node()), Text("]")]),
            TyKind::Slice(e) => self.seq(&[Text("["), N(e.node()), Text("]")]),
            TyKind::Pat(b, l) => self.seq(&[Text("pattern_type!("), N(b.node()), Text(" is "), N(l.node()), Text(")")]),
            TyKind::RawPtr(e, m) => {
                let p = if m == Mutability::Mut { "*mut " } else { "*const " };
                self.seq(&[Text(p), N(e.node())]);
            }
            TyKind::Ref(r, e, m) => {
                let mt = if m == Mutability::Mut { " mut " } else { " " };
                self.seq(&[Text("&"), N(r.node()), Text(mt), N(e.node())]);
            }
            TyKind::FnPtr(sig, h) => {
                if let Some(effs) = h.declared_effects() {
                    self.push(Text(effs.canonical_label()));
                    self.push(Text(" "));
                }
                let len = self.tys.list_len(sig);
                let out = len.saturating_sub(1);
                if let Some(o) = self.tys.list_get(sig, out) {
                    self.push(N(o.node()));
                }
                self.push(Text(") -> "));
                self.list(sig, 0, out, ", ");
                self.push(Text(if h.is_unsafe() { "unsafe fn(" } else { "fn(" }));
            }
            TyKind::Dynamic(preds, r) => {
                self.seq(&[N(r.node())]);
                self.push(Text(" + "));
                let len = self.tys.list_len(preds);
                self.list(preds, 0, len, " + ");
                self.push(Text("dyn "));
            }
            TyKind::Closure(d, l) => self.wrapped("{closure ", d, l),
            TyKind::CoroutineClosure(d, l) => self.wrapped("{async closure ", d, l),
            TyKind::Coroutine(d, l) => self.wrapped("{coroutine ", d, l),
            TyKind::CoroutineWitness(d, l) => self.wrapped("{witness ", d, l),
            TyKind::Tuple(l) => {
                let len = self.tys.list_len(l);
                self.push(Text(if len == 1 { ",)" } else { ")" }));
                self.list(l, 0, len, ", ");
                self.push(Text("("));
            }
            TyKind::Record(_, l) => {
                let len = self.tys.list_len(l);
                self.push(Text("}"));
                self.list(l, 0, len, ", ");
                self.push(Text("{"));
            }
            TyKind::Union(l) => {
                let len = self.tys.list_len(l);
                self.list(l, 0, len, " | ");
            }
            // Rigid aliases print as the others: rigidness is the solver's knowledge.
            TyKind::Alias { kind, rigid: _, def, args } => {
                match kind {
                    AliasKind::Projection | AliasKind::Inherent => {
                        // `<Self as ..>::Item<rest>`: the item's path, its arguments, `Self` first.
                        self.push(Text(">"));
                        self.args(args, 0);
                        self.push(Item::Def(def));
                        self.push(Text("<"));
                    }
                    AliasKind::Opaque => {
                        self.push(Text("}"));
                        self.path(def, args, 0);
                        self.push(Text("{opaque "));
                    }
                    AliasKind::Free => self.path(def, args, 0),
                }
            }
            TyKind::Param(i) => self.push(Item::TyParam(i)),
            TyKind::Bound(d, v) => self.seq(&[Text("^"), Num(u128::from(d)), Text("_"), Num(u128::from(v))]),
            TyKind::Canonical(v) => self.seq(&[Text("^c"), Num(u128::from(v))]),
            TyKind::Placeholder(u, v) => self.seq(&[Text("!"), Num(u128::from(u.0)), Text("_"), Num(u128::from(v))]),
            TyKind::TyVar(v) => self.seq(&[Text("?"), Num(u128::from(v)), Text("t")]),
            TyKind::IntVar(v) => self.seq(&[Text("?"), Num(u128::from(v)), Text("i")]),
            TyKind::FloatVar(v) => self.seq(&[Text("?"), Num(u128::from(v)), Text("f")]),
            TyKind::Error => self.push(Text("{type error}")),
            TyKind::NotAType => self.pred(t),
        }
    }

    fn wrapped(&mut self, open: &'static str, d: Def, l: List) {
        self.push(Item::Text("}"));
        self.path(d, l, 0);
        self.push(Item::Text(open));
    }

    fn pred(&mut self, t: crate::ids::Ty) {
        use Item::{Node as N, Text};
        let Some(p) = self.tys.as_pred(t) else { return };
        let a = |x: Arg| N(x.node());
        match self.tys.pred_kind(p) {
            PredKind::Trait(d, l) | PredKind::NotTrait(d, l) => {
                let neg = matches!(self.tys.pred_kind(p), PredKind::NotTrait(..));
                self.path(d, l, 1);
                if neg {
                    self.push(Text("!"));
                }
                self.push(Text(": "));
                if let Some(s) = self.tys.list_get(l, 0) {
                    self.push(a(s));
                }
            }
            PredKind::Projection(x, y) | PredKind::NormalizesTo(x, y) | PredKind::AliasEq(x, y) => {
                self.seq(&[a(x), Text(" == "), a(y)]);
            }
            PredKind::RegionOutlives(x, y) => self.seq(&[N(x.node()), Text(": "), N(y.node())]),
            PredKind::TypeOutlives(x, y) => self.seq(&[N(x.node()), Text(": "), N(y.node())]),
            PredKind::WellFormed(x) => self.seq(&[Text("wf("), a(x), Text(")")]),
            PredKind::ConstHasType(c, x) => self.seq(&[N(c.node()), Text(": "), N(x.node())]),
            PredKind::ConstEvaluatable(c) => self.seq(&[Text("evaluatable("), N(c.node()), Text(")")]),
            PredKind::Subtype(x, y) | PredKind::AliasSub(x, y) => self.seq(&[N(x.node()), Text(" <: "), N(y.node())]),
            PredKind::Coerce(x, y) => self.seq(&[N(x.node()), Text(" -> "), N(y.node())]),
            PredKind::DynCompatible(d) => self.seq(&[Text("dyn-compatible("), Item::Def(d), Text(")")]),
            PredKind::ConstEquate(x, y) => self.seq(&[N(x.node()), Text(" == "), N(y.node())]),
            PredKind::Ambiguous => self.push(Text("{ambiguous}")),
            PredKind::ExTrait(d, l) => self.path(d, l, 0),
            PredKind::ExProjection(d, l) => {
                let len = self.tys.list_len(l);
                let last = len.saturating_sub(1);
                if let Some(term) = self.tys.list_get(l, last) {
                    self.push(a(term));
                }
                self.push(Text(" = "));
                if last > 0 {
                    self.push(Text(">"));
                    self.list(l, 0, last, ", ");
                    self.push(Text("<"));
                }
                self.push(Item::Def(d));
            }
            PredKind::ExAuto(d) => self.push(Item::Def(d)),
            PredKind::NotAPred => {}
        }
    }

    fn region(&mut self, n: Node) {
        use Item::{Num, Text};
        match self.tys.region_kind(n.region()) {
            RegionKind::EarlyParam(i) => self.seq(&[Text("'"), Item::RegionParam(i)]),
            RegionKind::Bound(d, v) => self.seq(&[Text("'^"), Num(u128::from(d)), Text("_"), Num(u128::from(v))]),
            RegionKind::LateParam(d, v) => self.seq(&[Text("'{"), Item::Def(d), Text(" "), Num(u128::from(v)), Text("}")]),
            RegionKind::Static => self.push(Text("'static")),
            RegionKind::Var(v) => self.seq(&[Text("'?"), Num(u128::from(v))]),
            RegionKind::Placeholder(u, v) => {
                self.seq(&[Text("'!"), Num(u128::from(u.0)), Text("_"), Num(u128::from(v))]);
            }
            RegionKind::Erased => self.push(Text("'_")),
            RegionKind::Error => self.push(Text("'{region error}")),
            RegionKind::Canonical(v) => self.seq(&[Text("'^c"), Num(u128::from(v))]),
        }
    }

    fn konst(&mut self, n: Node) {
        use Item::{Num, Text};
        match self.tys.const_kind(n.konst()) {
            ConstKind::Param(i) => self.push(Item::ConstParam(i)),
            ConstKind::Var(v) => self.seq(&[Text("?"), Num(u128::from(v)), Text("c")]),
            ConstKind::Bound(d, v) => self.seq(&[Text("^"), Num(u128::from(d)), Text("_"), Num(u128::from(v))]),
            ConstKind::Placeholder(u, v) => self.seq(&[Text("!"), Num(u128::from(u.0)), Text("_"), Num(u128::from(v))]),
            ConstKind::Value(_, v) => self.push(Num(v)),
            ConstKind::Unevaluated(d, l) => self.wrapped("{const ", d, l),
            ConstKind::Error => self.push(Text("{const error}")),
            ConstKind::Canonical(v) => self.seq(&[Text("^c"), Num(u128::from(v))]),
        }
    }
}

/// Prints `n` (a type, region, const, predicate or list) to `out`.
pub fn print(tys: &Tys<'_>, n: Node, names: &mut dyn Names, out: &mut dyn Out) {
    let mut p = Printer { tys, stack: Vec::new_in(tys.allocator()) };
    p.push(Item::Node(n));
    while let Some(i) = p.stack.pop() {
        match i {
            Item::Text(s) => out.str(s),
            Item::Num(v) => decimal(v, out),
            Item::Def(d) => names.item(tys.item_key(d), out),
            Item::TyParam(i) => names.ty_param(i, out),
            Item::RegionParam(i) => names.region_param(i, out),
            Item::ConstParam(i) => names.const_param(i, out),
            Item::Node(n) => match n.tag() {
                TAG_TY => p.ty(n),
                TAG_REGION => p.region(n),
                TAG_CONST => p.konst(n),
                _ => {
                    debug_assert!(n.tag() == TAG_LIST);
                    let l = n.list();
                    let len = tys.list_len(l);
                    p.push(Item::Text("]"));
                    p.list(l, 0, len, ", ");
                    p.push(Item::Text("["));
                }
            },
        }
    }
}
