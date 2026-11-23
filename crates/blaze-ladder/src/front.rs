//! The ladder's front end over the real one's pieces: each file's items from blaze-file-items'
//! outline, each item's signature and body from blaze-lower (`lower_signature`, `lower_body`,
//! `to_typeck`), and the checker's `World` answered from what they give. Its refusal is the
//! first stage of [`crate::run::refusals`], what `blaze run` stops at and prints as findings.
//!
//! What stays a stand-in until the item layer answers (the README's shortcut list): names are
//! resolved here, not by blaze-scope (a module's items, `mod` files, `use` of items and
//! variants with `{..}` groups and `as`, `crate::`, `self::` and `super::`; no globs, no
//! visibility check, no prelude), and the whole program shares one portable type table.
//!
//! # Keys
//!
//! An item's key is a hash of where it is declared and what it is, never of its position: its
//! module's path, its kind and its name (an impl: its sibling index among impls). An item of a
//! trait or an impl is keyed by its container's key and its name, a variant by its enum's key
//! and its name.
//!
//! # Generics
//!
//! A trait's `Self` is `Param(0)`, its parameters follow; a fn of a trait or an impl has its
//! container's parameters first. An item's predicates are its parameters' bounds, its where
//! clauses and the implied `Sized` of each own parameter not written `?Sized`; a trait's fn
//! has `Self: Trait<..>`; an impl's fn has its impl's. A trait path written with fewer
//! arguments than the trait has gets the parameters' defaults, `Self` and the earlier
//! arguments put in.
//!
//! Every walk is a loop over an explicit stack (no recursion), and every refusal names the
//! construct and its byte range.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::vec::Vec;

use blaze_build::layout::{IntRepr, ReprAttrs};
use blaze_build::{Abi, Intrinsic, StaticItem};
use blaze_file_items::Outline;
use blaze_hash::HashMap;
use blaze_lower::form as lf;
use blaze_lower::machine::sig;
use blaze_lower::typeck::{Refused, TypeckBody, Types, to_typeck};
use blaze_lower::{BodyRange, Cfg, CfgAnswer, Lowered, TextAt, View, head};
use blaze_own::Alloc;
use blaze_surface::tool_attrs::{self, BlazeAttr, State};
use blaze_surface::{Edition, ItemDetail, ItemIndex, Range};
use blaze_typeck as tc;
use blaze_typeck::{
    AdtKind, AssocItem, AssocKind, AssocParent, AssocTyDef, CtorKind, Def, Fetch, FnSig, Hidden, ImplHeader, ItemKey,
    LangItem, List, Mutbl, Name, PClause, PNode, PTy, PTys, PathIdx, PathRes, Predicates, PortableTys, ScopeId, Seal,
    Shape, StableHasher, TraitDef, TraitFlags, TypeAttrs, World,
};

use crate::refusal::{NOT_YET, Refusal, RefusalRule, SourceFile, syntax_name};

/// A lang item that is a fn the backend calls, never one the checker asks for: blaze-typeck's
/// [`LangItem`] names traits only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FnLang {
    /// `#[lang = "panic_bounds_check"]`: what an index past an array's end calls.
    PanicBoundsCheck,
    /// `#[lang = "drop_in_place"]`: what a drop calls; its instances are built as drop glue.
    DropInPlace,
    /// `#[lang = "panic_const_*"]`: what a failed arithmetic check calls (blaze-build
    /// walk/arith_checks.rs).
    PanicConst(blaze_build::PanicConst),
    /// `#[lang = "eh_personality"]`: the personality routine every unwind entry's CIE names
    /// (std/src/sys/personality/gcc.rs `rust_eh_personality`); reached by no call, called by
    /// the system's unwinder (crate::personality).
    EhPersonality,
    /// `#[lang = "panic_cannot_unwind"]`: what a nounwind body's terminate block calls when a
    /// panic reaches it (core/src/panicking.rs; blaze-build walk/nounwind.rs).
    PanicCannotUnwind,
}

/// The edition every file is read in (the blaze editions are 2024's syntax).
const EDITION: Edition = Edition::E2024;

/// The symbol of bstd's hosted entry, `extern "C" fn(*const HostTable) -> i32`
/// (crates/blaze-std/src/wire.rs): a program exporting it is started by the host.
pub const HOSTED_START: &str = "bstd_hosted_start";

/// Where an item is: its file and its first byte there. A refusal's range is relative to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct At {
    pub file: u32,
    pub place: u32,
}

/// Why a step refused, item-relative: made a [`Refusal`] by [`Why::at`].
#[derive(Debug)]
enum Why {
    /// A construct this build does not take yet, by its plain Rust name.
    Unsupported(&'static str, Range),
    /// A program the front end refuses for what it says (an unresolved name, a duplicate).
    Program(Box<str, Global>, Range),
}

impl Why {
    fn at(self, at: At) -> Refusal {
        match self {
            Why::Unsupported(construct, r) => Refusal {
                file: Some(at.file),
                rule: RefusalRule::Unsupported,
                place: at.place,
                start: r.start,
                end: r.end,
                secondary: None,
                construct: Some(construct),
                message: format!("{construct} {NOT_YET}").into_boxed_str(),
            },
            Why::Program(message, r) => Refusal {
                file: Some(at.file),
                rule: RefusalRule::Program,
                place: at.place,
                start: r.start,
                end: r.end,
                secondary: None,
                construct: None,
                message,
            },
        }
    }
}

impl From<lf::BadRow> for Why {
    fn from(b: lf::BadRow) -> Why {
        Why::Program(format!("blaze-lower's record has a malformed row in column `{}`", b.column).into_boxed_str(), Range::new(0, 0))
    }
}

impl From<TextAt> for Why {
    fn from(t: TextAt) -> Why {
        match t {
            TextAt::Row(b) => Why::from(b),
            t => Why::Program(format!("blaze-lower's record has a range that is no text of it: {t:?}").into_boxed_str(), Range::new(0, 0)),
        }
    }
}

/// Where `r` is in the item, for a refusal: its own range, or the macro call an expansion
/// that made it came from (rule 71: item-relative).
fn place(v: &View<'_>, r: Result<BodyRange, lf::BadRow>) -> Result<Range, Why> {
    Ok(v.site(r?)?)
}

fn program(message: std::string::String, r: Range) -> Why {
    Why::Program(message.into_boxed_str(), r)
}

/// The two namespaces an item is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Ns {
    Type,
    Value,
}

/// What a name in a module's scope is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Res {
    Mod(u32),
    Fn(u32),
    Adt(u32),
    Trait(u32),
    /// An enum's variant: the ADT and the variant's index.
    Variant(u32, u32),
}

impl Res {
    const fn ns(self) -> Ns {
        match self {
            Res::Fn(_) | Res::Variant(..) => Ns::Value,
            Res::Mod(_) | Res::Adt(_) | Res::Trait(_) => Ns::Type,
        }
    }
}

struct Module {
    parent: Option<u32>,
    file: u32,
    /// The outline entry of its `mod` item; `None` for a file's own items.
    holder: Option<ItemIndex>,
    /// Where its `mod name;` files are: "src/", "src/util/".
    dir: Box<str, Global>,
    /// Its path from the crate root: "" for the root, "util" below it.
    path: Box<str, Global>,
}

/// What `Self` is where a type is written.
#[derive(Clone, Copy, Debug)]
enum SelfTy {
    None,
    /// In a trait (by index): `Param(0)`.
    Trait(u32),
    /// In an impl or an ADT: its self type.
    Ty(PTy),
}

/// What a written type's names mean where it is written.
#[derive(Clone, Debug)]
struct Env {
    module: u32,
    /// The generic type parameters in scope by name, with their indices.
    params: Vec<(Name, u32), Global>,
    self_: SelfTy,
    /// An impl's associated types, for `Self::X`.
    self_assoc: Vec<(Name, PTy), Global>,
}

impl Env {
    fn new(module: u32) -> Env {
        Env { module, params: Vec::new_in(Global), self_: SelfTy::None, self_assoc: Vec::new_in(Global) }
    }

    fn copy_in(&self) -> Env {
        let alloc = *self.params.allocator();
        Env {
            module: self.module,
            params: self.params.to_vec_in(alloc),
            self_: self.self_,
            self_assoc: self.self_assoc.to_vec_in(alloc),
        }
    }
}

struct AdtHead {
    key: ItemKey,
    kind: AdtKind,
    generics: u32,
    /// Each variant's name and how it is built; a struct's one variant is named as the struct.
    variants: Vec<(Name, CtorKind), Global>,
}

struct TraitHead {
    key: ItemKey,
    /// `Self` included.
    generics: u32,
    assoc: Vec<Name, Global>,
    /// The default of each parameter after `Self`, in the trait's parameters.
    defaults: Vec<Option<PTy>, Global>,
    fns: Vec<(Name, ItemKey), Global>,
}

/// The names of the program: what a path means, read while signatures and bodies are resolved.
struct Names {
    modules: Vec<Module, Global>,
    scope: HashMap<(u32, Name, Ns), Res, Global>,
    adts: Vec<AdtHead, Global>,
    traits: Vec<TraitHead, Global>,
    /// The `sized` lang trait: each own parameter's implied bound.
    sized: Option<ItemKey>,
}

const CRATE: Name = Name::of("crate");
const SELF_MOD: Name = Name::of("self");
const SUPER: Name = Name::of("super");
const SELF_TY: Name = Name::of("Self");

/// The primitive types by name.
const PRIMS: [(&str, PNode); 17] = [
    ("bool", PNode::Bool),
    ("char", PNode::Char),
    ("str", PNode::Str),
    ("i8", PNode::Int(tc::IntTy::I8)),
    ("i16", PNode::Int(tc::IntTy::I16)),
    ("i32", PNode::Int(tc::IntTy::I32)),
    ("i64", PNode::Int(tc::IntTy::I64)),
    ("i128", PNode::Int(tc::IntTy::I128)),
    ("isize", PNode::Int(tc::IntTy::Isize)),
    ("u8", PNode::Uint(tc::UintTy::U8)),
    ("u16", PNode::Uint(tc::UintTy::U16)),
    ("u32", PNode::Uint(tc::UintTy::U32)),
    ("u64", PNode::Uint(tc::UintTy::U64)),
    ("u128", PNode::Uint(tc::UintTy::U128)),
    ("usize", PNode::Uint(tc::UintTy::Usize)),
    ("f32", PNode::Float(tc::FloatTy::F32)),
    ("f64", PNode::Float(tc::FloatTy::F64)),
];

fn intern(tys: &mut PortableTys<'_>, n: PNode) -> Result<PTy, Why> {
    tys.intern(n).map_err(|_| program(format!("the type table is full"), Range::new(0, 0)))
}

fn intern_list(tys: &mut PortableTys<'_>, l: &[PTy]) -> Result<List, Why> {
    tys.intern_list(l).map_err(|_| program(format!("the type table is full"), Range::new(0, 0)))
}

fn params_of(tys: &mut PortableTys<'_>, from: u32, to: u32) -> Result<Vec<PTy, Global>, Why> {
    let mut out = Vec::with_capacity_in((to - from) as usize, Global);
    for i in from..to {
        out.push(intern(tys, PNode::Param(i))?);
    }
    Ok(out)
}

/// Instantiate parameters in the same portable type table used by the checker.
fn subst(tys: &mut PortableTys<'_>, t: PTy, args: &[PTy]) -> Result<PTy, Why> {
    tys.subst(t, args).map_err(|e| {
        let message = blaze_own::text::format(tys.alloc(), format_args!("substituting type {t:?}: {e}"));
        Why::Program(message.as_str().into(), Range::new(0, 0))
    })
}

/// A segment's name, the keywords by their own.
fn seg_name(v: &View<'_>, s: &lf::Segment, r: Range) -> Result<Name, Why> {
    Ok(match (s.kind, s.name) {
        (lf::SegKind::Ident, Some(n)) => Name(v.name(n)?),
        (lf::SegKind::SelfMod, _) => SELF_MOD,
        (lf::SegKind::Super, _) => SUPER,
        (lf::SegKind::Crate, _) => CRATE,
        (lf::SegKind::SelfTy, _) => SELF_TY,
        (lf::SegKind::DollarCrate, _) => return Err(Why::Unsupported("`$crate`", r)),
        (lf::SegKind::Anchor, _) | (lf::SegKind::Ident, None) => {
            return Err(Why::Unsupported("a qualified path (`<T as Trait>::X`) in this position", r));
        }
    })
}

fn segments(v: &View<'_>, p: PathIdx) -> Result<(lf::PathRef, Vec<lf::Segment, Global>, Range), Why> {
    let r = place(v, v.path_range(p))?;
    let path = v.path(p)?;
    if path.absolute {
        return Err(Why::Unsupported("a path from the crate root (`::x`)", r));
    }
    let mut out = Vec::new_in(Global);
    for &n in v.segments(path)? {
        let s = lf::Segment::decode(n)?;
        if s.parenthesized || s.return_notation {
            return Err(Why::Unsupported("parenthesized generic arguments (`Fn(A) -> B`)", r));
        }
        out.push(s);
    }
    Ok((path, out, r))
}

/// The type ids a segment's generic arguments name, in order; a lifetime is passed over.
fn arg_types(v: &View<'_>, s: &lf::Segment, r: Range, out: &mut Vec<lf::TypeId, Global>) -> Result<(), Why> {
    if !s.has_args {
        return Ok(());
    }
    for &n in v.rows(s.args)? {
        match lf::GenericArg::decode(n)? {
            lf::GenericArg::Type(t) => out.push(t),
            lf::GenericArg::Lifetime(_) => {}
            lf::GenericArg::Const(_) => return Err(Why::Unsupported("a const generic argument", r)),
            lf::GenericArg::AssocType { .. } => {}
            _ => return Err(Why::Unsupported("an associated item constraint", r)),
        }
    }
    Ok(())
}

/// The path of the path type `t`.
fn path_of(v: &View<'_>, t: lf::TypeId) -> Result<PathIdx, Why> {
    let r = place(v, v.type_range(t))?;
    match v.ty(t)? {
        lf::TypeRef::Path(p) => Ok(p),
        _ => Err(program(format!("a trait bound that is no path"), r)),
    }
}

/// The value of an integer, byte or char literal's text (`0x2a`, `42u8`, `b'*'`, `'*'`), sign
/// dropped; `None` for what is no such literal.
pub fn int_value(text: &str) -> Option<u128> {
    let text = text.trim().trim_start_matches('-').trim();
    if let Some(quoted) = text.strip_prefix("b'").or_else(|| text.strip_prefix('\'')) {
        let inner = quoted.strip_suffix('\'')?;
        let mut out = Vec::new_in(Global);
        unescape(inner, &mut out)?;
        if text.starts_with('b') {
            return out.first().map(|&b| u128::from(b));
        }
        let c = core::str::from_utf8(&out).ok()?.chars().next()?;
        return Some(u128::from(u32::from(c)));
    }
    const SUFFIXES: [&str; 12] = ["i128", "isize", "i16", "i32", "i64", "i8", "u128", "usize", "u16", "u32", "u64", "u8"];
    let (radix, digits) = match text.get(..2) {
        Some("0x") => (16, &text[2..]),
        Some("0o") => (8, &text[2..]),
        Some("0b") => (2, &text[2..]),
        _ => (10, text),
    };
    let mut digits = digits;
    for s in SUFFIXES {
        if let Some(d) = digits.strip_suffix(s) {
            if !d.is_empty() {
                digits = d;
                break;
            }
        }
    }
    let mut value: u128 = 0;
    let mut any = false;
    for c in digits.chars().filter(|&c| c != '_') {
        let d = c.to_digit(radix)?;
        value = value.checked_mul(u128::from(radix))?.checked_add(u128::from(d))?;
        any = true;
    }
    any.then_some(value)
}

/// A quoted literal's contents unescaped into `out`; `None` for an escape outside the subset.
fn unescape(inner: &str, out: &mut Vec<u8, Global>) -> Option<()> {
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next()? {
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            '0' => out.push(0),
            '\\' => out.push(b'\\'),
            '\'' => out.push(b'\''),
            '"' => out.push(b'"'),
            'x' => {
                let hi = chars.next()?.to_digit(16)?;
                let lo = chars.next()?.to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
            }
            // `\u{7FFF}`: up to six hex digits (underscores allowed), a scalar value, as UTF-8.
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let mut v = 0u32;
                loop {
                    match chars.next()? {
                        '}' => break,
                        '_' => {}
                        d => v = v.checked_mul(16)?.checked_add(d.to_digit(16)?)?,
                    }
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(char::from_u32(v)?.encode_utf8(&mut buf).as_bytes());
            }
            '\n' => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            _ => return None,
        }
    }
    Some(())
}

/// The bytes of a string or byte string literal's text (`"a\n"`, `b"a"`, `r#"a"#`).
pub(crate) fn string_bytes(text: &str) -> Option<Vec<u8, Global>> {
    let t = text.strip_prefix('b').unwrap_or(text);
    let mut out = Vec::new_in(Global);
    if let Some(raw) = t.strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        let inner = raw.get(hashes + 1..raw.len().checked_sub(hashes + 1)?)?;
        out.extend_from_slice(inner.as_bytes());
        return Some(out);
    }
    unescape(t.strip_prefix('"')?.strip_suffix('"')?, &mut out)?;
    Some(out)
}

impl Names {
    fn find(&self, module: u32, name: Name, ns: Ns) -> Option<Res> {
        self.scope.get(&(module, name, ns)).copied()
    }

    /// The longest prefix of `segs` that names an item from `module`, and how many segments it
    /// took: `crate`, `self` and `super` first; a module's item in the type namespace, the last
    /// segment in `last` (then the other); an enum's variant after the enum.
    fn walk(&self, module: u32, segs: &[Name], last: Ns) -> Option<(Res, u32)> {
        let mut m = module;
        let mut i = 0usize;
        let mut res = None;
        while i < segs.len() {
            let s = segs[i];
            if i == 0 && s == CRATE {
                m = 0;
                res = Some(Res::Mod(0));
                i += 1;
                continue;
            }
            if s == SUPER && matches!(res, None | Some(Res::Mod(_))) {
                m = self.modules[m as usize].parent?;
                res = Some(Res::Mod(m));
                i += 1;
                continue;
            }
            if i == 0 && s == SELF_MOD {
                res = Some(Res::Mod(m));
                i += 1;
                continue;
            }
            let ns = if i + 1 == segs.len() { last } else { Ns::Type };
            let other = if ns == Ns::Type { Ns::Value } else { Ns::Type };
            let found = self.find(m, s, ns).or_else(|| self.find(m, s, other))?;
            i += 1;
            match found {
                Res::Mod(c) => {
                    m = c;
                    res = Some(found);
                }
                Res::Adt(a) if self.adts[a as usize].kind == AdtKind::Enum && i < segs.len() => {
                    let v = self.adts[a as usize].variants.iter().position(|(n, _)| *n == segs[i]);
                    return match v {
                        Some(v) => Some((Res::Variant(a, v as u32), i as u32 + 1)),
                        None => Some((found, i as u32)),
                    };
                }
                _ => return Some((found, i as u32)),
            }
        }
        res.map(|r| (r, i as u32))
    }

    /// The portable type of the written type `t` of `v` (text `text`) where `env` holds.
    fn ty(&self, tys: &mut PortableTys<'_>, env: &Env, v: &View<'_>,t: lf::TypeId) -> Result<PTy, Why> {
        enum S {
            Visit(lf::TypeId),
            Make(lf::TypeId),
        }
        let mut todo = Vec::new_in(Global);
        let mut done: Vec<PTy, Global> = Vec::new_in(Global);
        let mut kids: Vec<lf::TypeId, Global> = Vec::new_in(Global);
        todo.push(S::Visit(t));
        while let Some(step) = todo.pop() {
            match step {
                S::Visit(t) => {
                    todo.push(S::Make(t));
                    kids.clear();
                    self.type_children(v, t, &mut kids)?;
                    for &k in kids.iter().rev() {
                        todo.push(S::Visit(k));
                    }
                }
                S::Make(t) => {
                    kids.clear();
                    self.type_children(v, t, &mut kids)?;
                    let start = done.len() - kids.len();
                    let made = self.make(tys, env, v, t, &done[start..])?;
                    done.truncate(start);
                    done.push(made);
                }
            }
        }
        done.pop().ok_or(program(format!("an empty type"), Range::new(0, 0)))
    }

    fn type_children(&self, v: &View<'_>, t: lf::TypeId, out: &mut Vec<lf::TypeId, Global>) -> Result<(), Why> {
        let r = place(v, v.type_range(t))?;
        match v.ty(t)? {
            lf::TypeRef::Ref { inner, .. } | lf::TypeRef::Ptr { inner, .. } | lf::TypeRef::Slice(inner) => out.push(inner),
            lf::TypeRef::Array { elem, .. } => out.push(elem),
            lf::TypeRef::Tuple(l) => out.extend(v.ids(l)?.iter().map(|&i| lf::TypeId(i))),
            lf::TypeRef::Path(p) => {
                let (_, segs, r) = segments(v, p)?;
                for s in &segs {
                    if s.kind == lf::SegKind::Anchor {
                        let (Some(ty), Some(tr)) = (s.ty, s.trait_) else {
                            return Err(Why::Unsupported("a qualified path with no trait (`<T>::X`)", r));
                        };
                        out.push(ty);
                        let (_, tsegs, tr_r) = segments(v, path_of(v, tr)?)?;
                        for ts in &tsegs {
                            arg_types(v, ts, tr_r, out)?;
                        }
                    } else {
                        arg_types(v, s, r, out)?;
                    }
                }
            }
            lf::TypeRef::Never | lf::TypeRef::Infer => {}
            lf::TypeRef::FnPtr { variadic: true, .. } => return Err(Why::Unsupported("a variadic fn pointer type", r)),
            // `params` are rows of `Param`, each with its type.
            lf::TypeRef::FnPtr { params, ret, .. } => {
                for &n in v.rows(params)? {
                    out.push(lf::Param::decode(n)?.ty.ok_or(Why::Unsupported("a fn pointer parameter with no type", r))?);
                }
                out.extend(ret);
            }
            lf::TypeRef::Dyn(_) => return Err(Why::Unsupported("a trait object type (`dyn Trait`)", r)),
            lf::TypeRef::Impl(_) => return Err(Why::Unsupported("an `impl Trait` type", r)),
            lf::TypeRef::Macro(_) => return Err(Why::Unsupported("a macro call in a type", r)),
            lf::TypeRef::Pat { .. } => return Err(Why::Unsupported("a pattern type", r)),
            lf::TypeRef::Record(_) => return Err(Why::Unsupported("a record type", r)),
            lf::TypeRef::Union(_) => return Err(Why::Unsupported("a union type", r)),
            lf::TypeRef::For { .. } => return Err(Why::Unsupported("a higher-ranked type (`for<'a>`)", r)),
            lf::TypeRef::Error => return Err(Why::Unsupported("this type", r)),
        }
        Ok(())
    }

    fn make(&self, tys: &mut PortableTys<'_>, env: &Env, v: &View<'_>,t: lf::TypeId, kids: &[PTy]) -> Result<PTy, Why> {
        let r = place(v, v.type_range(t))?;
        match v.ty(t)? {
            lf::TypeRef::Ref { mutbl, .. } => intern(tys, PNode::Ref(mutbl, kids[0])),
            lf::TypeRef::Ptr { mutbl, .. } => intern(tys, PNode::Ptr(mutbl, kids[0])),
            lf::TypeRef::Slice(_) => intern(tys, PNode::Slice(kids[0])),
            lf::TypeRef::Array { len, .. } => {
                let lb = v.const_range(len)?;
                let lr = v.site(lb)?;
                let n = int_value(v.text_at(lb)?).ok_or(Why::Unsupported("an array length that is no integer literal", lr))?;
                let n = u64::try_from(n).map_err(|_| program(format!("an array length past u64"), lr))?;
                let n = tys.usize_value(n).map_err(|_| program(format!("the type table is full"), lr))?;
                intern(tys, PNode::Array(kids[0], n))
            }
            lf::TypeRef::Tuple(_) => {
                let l = intern_list(tys, kids)?;
                intern(tys, PNode::Tuple(l))
            }
            lf::TypeRef::Never => intern(tys, PNode::Never),
            lf::TypeRef::Infer => intern(tys, PNode::Infer),
            lf::TypeRef::Path(p) => self.path_ty(tys, env, v, p, kids),
            // The inputs, then the output last (unit when none is written).
            lf::TypeRef::FnPtr { ret, abi, unsafety, extern_, .. } => {
                // `extern fn` with no string is `extern "C" fn` (the Reference, "External blocks").
                let abi = match (extern_, abi) {
                    (false, _) => tc::Abi::Rust,
                    (true, None) => tc::Abi::C,
                    (true, Some(a)) => match unquote(v.text_at(a)?) {
                        "Rust" => tc::Abi::Rust,
                        "C" => tc::Abi::C,
                        "C-unwind" => tc::Abi::CUnwind,
                        "system" => tc::Abi::System,
                        "system-unwind" => tc::Abi::SystemUnwind,
                        _ => return Err(Why::Unsupported("a fn pointer of this ABI", v.site(a)?)),
                    },
                };
                let safety = if unsafety { tc::Safety::Unsafe } else { tc::Safety::Safe };
                let header = tc::FnPtrHeader { abi, safety, c_variadic: false };
                let mut l: Vec<PTy, Global> = Vec::with_capacity_in(kids.len() + 1, Global);
                l.extend_from_slice(kids);
                if ret.is_none() {
                    l.push(intern(tys, PNode::Tuple(List::EMPTY))?);
                }
                let l = intern_list(tys, &l)?;
                intern(tys, PNode::FnPtr(l, header))
            }
            _ => Err(Why::Unsupported("this type", r)),
        }
    }

    fn path_ty(&self, tys: &mut PortableTys<'_>, env: &Env, v: &View<'_>,p: PathIdx, kids: &[PTy]) -> Result<PTy, Why> {
        let (_, segs, r) = segments(v, p)?;
        let shown_at = v.text_at(v.path_range(p)?)?;
        let shown = || shown_at;
        // `<T as Trait<..>>::X`: the anchor's type, then the trait's arguments.
        if segs[0].kind == lf::SegKind::Anchor {
            let [_, last] = segs.as_slice() else {
                return Err(Why::Unsupported("a qualified path of more than one item", r));
            };
            if last.args.len != 0 {
                return Err(Why::Unsupported("arguments on a generic associated type", r));
            }
            let tr = segs[0].trait_.ok_or(Why::Unsupported("a qualified path with no trait (`<T>::X`)", r))?;
            let (ti, args) = self.trait_path(tys, env, v, path_of(v, tr)?, kids[0], &kids[1..])?;
            let name = seg_name(v, last, r)?;
            return self.alias(tys, ti, name, args, r, shown());
        }
        let first = seg_name(v, &segs[0], r)?;
        if first == SELF_TY {
            if let [_, member] = segs.as_slice() && member.args.len != 0 {
                return Err(Why::Unsupported("arguments on a generic associated type", r));
            }
            return match (segs.len(), env.self_) {
                (1, SelfTy::Trait(_)) => intern(tys, PNode::Param(0)),
                (1, SelfTy::Ty(t)) => Ok(t),
                (2, SelfTy::Trait(ti)) => {
                    let name = seg_name(v, &segs[1], r)?;
                    let g = self.traits[ti as usize].generics;
                    let args = params_of(tys, 0, g)?;
                    let args = intern_list(tys, &args)?;
                    self.alias(tys, ti, name, args, r, shown())
                }
                (2, SelfTy::Ty(_)) => {
                    let name = seg_name(v, &segs[1], r)?;
                    env.self_assoc
                        .iter()
                        .find(|(n, _)| *n == name)
                        .map(|&(_, t)| t)
                        .ok_or_else(|| program(format!("`{}` names no associated type of the impl", shown()), r))
                }
                _ => Err(Why::Unsupported("`Self` here", r)),
            };
        }
        if let Some(&(_, i)) = env.params.iter().find(|(n, _)| *n == first) {
            if segs.len() == 1 {
                return intern(tys, PNode::Param(i));
            }
            return Err(Why::Unsupported("an associated type of a type parameter (`T::X`)", r));
        }
        let mut names = Vec::with_capacity_in(segs.len(), Global);
        for s in &segs {
            names.push(seg_name(v, s, r)?);
        }
        if names.len() == 1 && self.find(env.module, first, Ns::Type).is_none() {
            if let Some((_, n)) = PRIMS.iter().find(|(p, _)| Name::of(p) == first) {
                if !kids.is_empty() {
                    return Err(program(format!("`{}` takes no type arguments", shown()), r));
                }
                return intern(tys, *n);
            }
        }
        match self.walk(env.module, &names, Ns::Type) {
            Some((Res::Adt(a), n)) if n as usize == names.len() => {
                let adt = &self.adts[a as usize];
                if kids.len() != adt.generics as usize {
                    return Err(program(format!("`{}` is written with {} type arguments; it takes {}", shown(), kids.len(), adt.generics), r));
                }
                let l = intern_list(tys, kids)?;
                intern(tys, PNode::Adt(adt.key, l))
            }
            Some(_) => Err(program(format!("`{}` is not a type", shown()), r)),
            None => Err(program(format!("the type `{}` names nothing", shown()), r)),
        }
    }

    /// `<args[0] as trait ti<args[1..]>>::name`.
    fn alias(&self, tys: &mut PortableTys<'_>, ti: u32, name: Name, args: List, r: Range, shown: &str) -> Result<PTy, Why> {
        let tr = &self.traits[ti as usize];
        if !tr.assoc.contains(&name) {
            return Err(program(format!("`{shown}` names no associated type of the trait"), r));
        }
        intern(tys, PNode::Alias { trait_: tr.key, name, args })
    }

    /// The trait path `p` with `self_ty` as `Self`: the trait and its arguments, `Self` first,
    /// the defaults of those not written put in. `given` are its written type arguments,
    /// resolved already.
    fn trait_path(&self, tys: &mut PortableTys<'_>, env: &Env, v: &View<'_>, p: PathIdx, self_ty: PTy, given: &[PTy]) -> Result<(u32, List), Why> {
        let (_, segs, r) = segments(v, p)?;
        let mut names = Vec::with_capacity_in(segs.len(), Global);
        for s in &segs {
            names.push(seg_name(v, s, r)?);
        }
        let ti = match self.walk(env.module, &names, Ns::Type) {
            Some((Res::Trait(t), n)) if n as usize == names.len() => t,
            Some(_) => return Err(program(format!("this path is not a trait"), r)),
            None => return Err(program(format!("the trait path names nothing"), r)),
        };
        let tr = &self.traits[ti as usize];
        let want = tr.generics as usize - 1;
        if given.len() > want {
            return Err(program(format!("a trait written with {} type arguments; it takes {want}", given.len()), r));
        }
        let mut args = Vec::with_capacity_in(want + 1, Global);
        args.push(self_ty);
        args.extend_from_slice(given);
        for i in given.len()..want {
            let Some(d) = tr.defaults.get(i).copied().flatten() else {
                return Err(program(format!("a trait written with {} type arguments; it takes {want}", given.len()), r));
            };
            let d = subst(tys, d, &args)?;
            args.push(d);
        }
        Ok((ti, intern_list(tys, &args)?))
    }

    /// The bound `t` (a trait path) on `self_ty` as clauses into `out`: the trait's, and a
    /// projection for each `Name = Type` argument. `Ok(None)` for `?Sized`.
    fn bound(&self, tys: &mut PortableTys<'_>, env: &Env, v: &View<'_>,b: lf::Bound, self_ty: PTy, out: &mut Vec<PClause, Global>) -> Result<Option<()>, Why> {
        let lf::Bound::Trait { ty, binder, mods } = b else { return Ok(Some(())) };
        let r = place(v, v.type_range(ty))?;
        if binder.len != 0 {
            return Err(Why::Unsupported("a higher-ranked bound (`for<'a>`)", r));
        }
        if mods.negative || mods.konst || mods.maybe_const || mods.asyncness {
            return Err(Why::Unsupported("a bound modifier", r));
        }
        let p = path_of(v, ty)?;
        let (_, segs, pr) = segments(v, p)?;
        let mut given = Vec::new_in(Global);
        let mut projections = Vec::new_in(Global);
        if let Some(last) = segs.last() {
            if last.has_args {
                for &n in v.rows(last.args)? {
                    match lf::GenericArg::decode(n)? {
                        lf::GenericArg::Type(t) => given.push(self.ty(tys, env, v, t)?),
                        lf::GenericArg::Lifetime(_) => {}
                        lf::GenericArg::AssocType { args, .. } if args.len != 0 => {
                            return Err(Why::Unsupported("arguments on a generic associated type binding", pr));
                        }
                        lf::GenericArg::AssocType { name, ty, .. } => {
                            let term = self.ty(tys, env, v, ty)?;
                            projections.push((Name(v.name(name)?), term));
                        }
                        _ => return Err(Why::Unsupported("this generic argument in a bound", pr)),
                    }
                }
            }
        }
        let (ti, args) = self.trait_path(tys, env, v, p, self_ty, &given)?;
        let key = self.traits[ti as usize].key;
        if mods.maybe {
            if Some(key) != self.sized {
                return Err(Why::Unsupported("`?Trait` for a trait that is not `Sized`", r));
            }
            return Ok(None);
        }
        out.push(PClause::Trait { trait_: key, args });
        for (name, term) in projections {
            if !self.traits[ti as usize].assoc.contains(&name) {
                return Err(program(format!("a bound names an associated type its trait does not have"), pr));
            }
            out.push(PClause::Projection { trait_: key, args, name, term });
        }
        Ok(Some(()))
    }

    /// The predicates of an item's generics (`v`'s GENERICS and WHERE, own parameters from
    /// index `base`): bounds, where clauses, and each own parameter's implied `Sized`.
    fn predicates(&self, tys: &mut PortableTys<'_>, env: &Env, v: &View<'_>,base: u32, out: &mut Vec<PClause, Global>) -> Result<(), Why> {
        let mut i = base;
        for &n in v.rows(v.list(head::GENERICS))? {
            if let lf::GenericParam::Type { bounds, .. } = lf::GenericParam::decode(n)? {
                let me = intern(tys, PNode::Param(i))?;
                let mut sized = true;
                for &b in v.rows(bounds)? {
                    if self.bound(tys, env, v, lf::Bound::decode(b)?, me, out)?.is_none() {
                        sized = false;
                    }
                }
                if sized {
                    if let Some(s) = self.sized {
                        let args = intern_list(tys, &[me])?;
                        out.push(PClause::Trait { trait_: s, args });
                    }
                }
                i += 1;
            }
        }
        for &n in v.rows(v.list(head::WHERE))? {
            let pred = lf::WherePred::decode(n)?;
            // `where const { COND }` is a condition of the item, never passed over: an impl
            // under one would otherwise hold for every type (blaze-std's `Acyclic`).
            // `Front::conditions` makes its clause, each condition an item of its own.
            if let lf::WherePred::Const { .. } = pred {
                continue;
            }
            if let lf::WherePred::Type { binder, ty, bounds } = pred {
                let r = place(v, v.type_range(ty))?;
                if binder.len != 0 {
                    return Err(Why::Unsupported("a higher-ranked where clause (`for<'a>`)", r));
                }
                let me = self.ty(tys, env, v, ty)?;
                for &b in v.rows(bounds)? {
                    if self.bound(tys, env, v, lf::Bound::decode(b)?, me, out)?.is_none() {
                        return Err(Why::Unsupported("`?Sized` in a where clause", r));
                    }
                }
            }
        }
        Ok(())
    }
}

/// The generic type parameters of `v` by name, from index `base`; a const parameter is
/// refused.
fn generic_names(v: &View<'_>, base: u32, out: &mut Vec<(Name, u32), Global>) -> Result<u32, Why> {
    let mut i = base;
    let rows = v.rows(v.list(head::GENERICS))?;
    for (k, &n) in rows.iter().enumerate() {
        match lf::GenericParam::decode(n)? {
            lf::GenericParam::Lifetime { .. } => {}
            lf::GenericParam::Type { name, .. } => {
                out.push((Name(v.name(name)?), i));
                i += 1;
            }
            lf::GenericParam::Const { .. } => {
                let r = place(v, v.row_range(v.list(head::GENERICS).start + k as u32))?;
                return Err(Why::Unsupported("a const generic parameter", r));
            }
        }
    }
    Ok(i - base)
}

/// What an item's attributes say that the front end reads.
#[derive(Default)]
struct Attrs<'t> {
    lang: Option<(&'t str, Range)>,
    no_mangle: bool,
    export_name: Option<&'t str>,
    link_name: Option<&'t str>,
    intrinsic: bool,
    repr: ReprAttrs,
    thread_local: bool,
}

/// One `#[repr(..)]`'s arguments folded into `out`: `C`, `transparent`, an integer type,
/// `packed`, `packed(n)`, `align(n)`, comma separated. Any other is refused.
fn read_repr(args: &str, r: Range, out: &mut ReprAttrs) -> Result<(), Why> {
    repr_parts(args, out).map_err(|what| Why::Unsupported(what, r))
}

/// `#[repr(..)]`'s arguments (`args`, the parenthesized list) into `out`; the one reader of
/// both front ends. `Err` names the construct refused.
pub(crate) fn repr_parts(args: &str, out: &mut ReprAttrs) -> Result<(), &'static str> {
    let args = args.trim();
    let inner = args.strip_prefix('(').and_then(|a| a.strip_suffix(')')).unwrap_or(args);
    let pow2 = |n: &str| -> Result<u8, &'static str> {
        let n = int_value(n.trim()).filter(|n| n.is_power_of_two() && *n <= 1 << 29);
        n.map(|n| n.trailing_zeros() as u8).ok_or("a `repr` alignment that is no power of two")
    };
    for part in inner.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let int = |i| Some(IntRepr::Signed(i));
        let uint = |u| Some(IntRepr::Unsigned(u));
        match part {
            // The default representation written out (the Reference, "Type layout", "The `Rust`
            // representation"): it sets nothing. core writes it beside a modifier:
            // `#[repr(Rust, packed)] struct Unaligned<T>(T);` (library/core/src/ptr/mod.rs), which
            // `read_unaligned` and `write_unaligned` are made of.
            "Rust" => {}
            "C" => out.c = true,
            "transparent" => out.transparent = true,
            "packed" => out.packed = Some(0),
            "simd" => out.simd = true,
            "i8" => out.int = int(tc::IntTy::I8),
            "i16" => out.int = int(tc::IntTy::I16),
            "i32" => out.int = int(tc::IntTy::I32),
            "i64" => out.int = int(tc::IntTy::I64),
            "i128" => out.int = int(tc::IntTy::I128),
            "isize" => out.int = int(tc::IntTy::Isize),
            "u8" => out.int = uint(tc::UintTy::U8),
            "u16" => out.int = uint(tc::UintTy::U16),
            "u32" => out.int = uint(tc::UintTy::U32),
            "u64" => out.int = uint(tc::UintTy::U64),
            "u128" => out.int = uint(tc::UintTy::U128),
            "usize" => out.int = uint(tc::UintTy::Usize),
            p => match p.split_once('(') {
                Some(("packed", n)) => out.packed = Some(pow2(n.trim_end_matches(')'))?),
                Some(("align", n)) => out.align = Some(pow2(n.trim_end_matches(')'))?),
                _ => return Err("this `repr`"),
            },
        }
    }
    Ok(())
}

pub(crate) fn unquote(t: &str) -> &str {
    let t = t.trim();
    t.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(t)
}

/// The `blaze::` attributes the front end read (blaze-surface's `tool_attrs`): each is inert,
/// kept on its item's lowered record as written (the attribute row's path and token ranges).
/// What this gathers is what the commands say about them: one advice finding per use of a name
/// this build does not know, and the count of known ones nothing checks yet.
#[derive(Debug, Default)]
pub struct BlazeAttrs {
    /// Each use read, by file and its first byte in the file, sorted: an item's attributes may
    /// be read twice (a trait's for `#[lang = "sized"]`, then for its signature), and count once.
    seen: Vec<(u32, u32), Global>,
    /// Uses of known names that no stage reads, by name (an index of `tool_attrs::KNOWN`).
    unchecked: Vec<u16, Global>,
    /// An advice finding ([`RefusalRule::UnknownBlazeAttribute`]) per use of an unknown name.
    pub advice: Vec<Refusal, Global>,
    /// An error per use of a removed name (`tool_attrs::State::Removed`); none today. The
    /// front end refuses with the first once the items are read.
    removed: Vec<Refusal, Global>,
}

impl BlazeAttrs {
    /// `blaze::{name}` at `r` of the item at `at`.
    fn read(&mut self, at: At, r: Range, name: &str) {
        let Err(slot) = self.seen.binary_search(&(at.file, at.place + r.start)) else { return };
        self.seen.insert(slot, (at.file, at.place + r.start));
        match tool_attrs::classify(name) {
            BlazeAttr::Known(i) => match tool_attrs::KNOWN[usize::from(i)].state {
                State::Unchecked => self.unchecked.push(i),
                State::Checked(_) => {}
                State::Removed { why } => self.removed.push(Refusal {
                    file: Some(at.file),
                    rule: RefusalRule::Program,
                    place: at.place,
                    start: r.start,
                    end: r.end,
                    secondary: None,
                    construct: None,
                    message: format!("`blaze::{name}` was removed from blaze ({why}); delete it").into_boxed_str(),
                }),
            },
            BlazeAttr::Unknown { nearest } => {
                let message = match nearest {
                    Some(k) => format!(
                        "`blaze::{name}` is not an attribute this build of blaze knows; the nearest known is `blaze::{}`",
                        tool_attrs::KNOWN[usize::from(k)].name
                    ),
                    None => format!("`blaze::{name}` is not an attribute this build of blaze knows"),
                };
                self.advice.push(Refusal {
                    file: Some(at.file),
                    rule: RefusalRule::UnknownBlazeAttribute,
                    place: at.place,
                    start: r.start,
                    end: r.end,
                    secondary: None,
                    construct: None,
                    message: message.into_boxed_str(),
                });
            }
        }
    }

    /// How many uses of known attributes no stage of this build checks, and of how many names.
    pub fn unchecked(&self) -> (u32, u32) {
        let mut kinds = [false; tool_attrs::KNOWN.len()];
        for &i in &self.unchecked {
            kinds[usize::from(i)] = true;
        }
        (self.unchecked.len() as u32, kinds.iter().filter(|k| **k).count() as u32)
    }

    /// The one counted note a command prints after its findings, when any use is unchecked:
    /// "N attributes of M kinds are recorded but not checked by this build".
    pub fn note(&self) -> Option<Box<str, Global>> {
        let (uses, kinds) = self.unchecked();
        let s = |n: u32| if n == 1 { "" } else { "s" };
        let verb = if uses == 1 { "is" } else { "are" };
        (uses > 0).then(|| {
            format!("{uses} `blaze::` attribute{} of {kinds} kind{} {verb} recorded but not checked by this build", s(uses), s(kinds))
                .into_boxed_str()
        })
    }
}

/// The attributes of the rows `list` of `v`, of the item at `at`. Those that change nothing a
/// program does (`allow`, `inline`, `must_use`, `doc`) are passed over, and so is a tool's
/// (`rustfmt::skip`; `blaze::..` read into `blaze`); any other is refused.
fn read_attrs<'t>(v: &View<'t>, list: lf::List, at: At, blaze: &mut BlazeAttrs) -> Result<Attrs<'t>, Why> {
    let mut out = Attrs::default();
    for &n in v.rows(list)? {
        let a = v.attr(n)?;
        let path = v.text_at(a.path)?.trim();
        let r = v.site(a.path.cover(a.args).unwrap_or(a.path))?;
        let args = v.text_at(a.args)?;
        let value = || unquote(args);
        // A tool's attribute (Rust: a path of two or more segments whose first is a tool's
        // name) is inert: never a macro, never refused.
        if let Some((tool, rest)) = path.split_once("::")
            && let tool = tool.trim()
            && tool_attrs::TOOLS.contains(&tool)
        {
            if tool == tool_attrs::BLAZE {
                blaze.read(at, r, rest.trim());
            }
            continue;
        }
        match path {
            "lang" => out.lang = Some((value(), r)),
            "no_mangle" => out.no_mangle = true,
            "export_name" => out.export_name = Some(value()),
            "link_name" => out.link_name = Some(value()),
            "rustc_intrinsic" => out.intrinsic = true,
            "thread_local" => out.thread_local = true,
            "" | "doc" | "allow" | "warn" | "deny" | "expect" | "forbid" | "inline" | "cold" | "must_use" | "track_caller" => {}
            "repr" => read_repr(args, r, &mut out.repr)?,
            "derive" => return Err(Why::Unsupported("`#[derive(..)]`", r)),
            _ => return Err(Why::Unsupported("this attribute", r)),
        }
    }
    Ok(out)
}

/// A trait's lang item by its `#[lang]` name.
fn trait_lang(name: &str) -> Option<LangItem> {
    Some(match name {
        "sized" => LangItem::Sized,
        "meta_sized" => LangItem::MetaSized,
        "pointee_sized" => LangItem::PointeeSized,
        "copy" => LangItem::Copy,
        "clone" => LangItem::Clone,
        "deref" => LangItem::Deref,
        "deref_mut" => LangItem::DerefMut,
        "add" => LangItem::Add,
        "sub" => LangItem::Sub,
        "mul" => LangItem::Mul,
        "div" => LangItem::Div,
        "rem" => LangItem::Rem,
        "bitand" => LangItem::BitAnd,
        "bitor" => LangItem::BitOr,
        "bitxor" => LangItem::BitXor,
        "shl" => LangItem::Shl,
        "shr" => LangItem::Shr,
        "add_assign" => LangItem::AddAssign,
        "sub_assign" => LangItem::SubAssign,
        "mul_assign" => LangItem::MulAssign,
        "div_assign" => LangItem::DivAssign,
        "rem_assign" => LangItem::RemAssign,
        "bitand_assign" => LangItem::BitAndAssign,
        "bitor_assign" => LangItem::BitOrAssign,
        "bitxor_assign" => LangItem::BitXorAssign,
        "shl_assign" => LangItem::ShlAssign,
        "shr_assign" => LangItem::ShrAssign,
        "neg" => LangItem::Neg,
        "not" => LangItem::Not,
        "eq" => LangItem::PartialEq,
        "partial_ord" => LangItem::PartialOrd,
        "index" => LangItem::Index,
        "index_mut" => LangItem::IndexMut,
        "fn_once" => LangItem::FnOnce,
        "fn_mut" => LangItem::FnMut,
        "fn" => LangItem::Fn,
        "drop" => LangItem::Drop,
        "sync" => LangItem::Sync,
        "unpin" => LangItem::Unpin,
        "destruct" => LangItem::Destruct,
        "tuple_trait" => LangItem::Tuple,
        "unsize" => LangItem::Unsize,
        "coerce_unsized" => LangItem::CoerceUnsized,
        "dispatch_from_dyn" => LangItem::DispatchFromDyn,
        "fn_ptr_trait" => LangItem::FnPtrTrait,
        "pointee_trait" => LangItem::Pointee,
        "discriminant_kind" => LangItem::DiscriminantKind,
        "future_trait" => LangItem::Future,
        "from" => LangItem::From,
        _ => return None,
    })
}

/// Lang names rustc's `core` declares that no blaze desugaring or check names: accepted, read
/// by nothing.
const UNUSED_LANG: [&str; 3] = ["Try", "FromResidual", "Option"];

/// The key of an item from its parts (module path, kind, name, sibling index).
fn key_of(parts: &[&[u8]]) -> ItemKey {
    let half = |domain: &str| {
        let mut h = StableHasher::new(domain);
        for p in parts {
            h = h.bytes(p).byte(0xff);
        }
        h.finish()
    };
    ItemKey((u128::from(half("blaze-ladder front key hi")) << 64) | u128::from(half("blaze-ladder front key lo")))
}

/// A member key: of `container`, kind `kind`, named `name`.
fn member_key(container: ItemKey, kind: u8, name: &str) -> ItemKey {
    key_of(&[&container.0.to_le_bytes(), &[kind], name.as_bytes()])
}

/// What a fn item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnKind {
    Free,
    /// A fn of the trait at this index.
    Trait(u32),
    /// A fn of the impl at this index.
    Impl(u32),
    /// A declaration in an `extern` block: called by its symbol.
    Foreign,
    /// A tuple struct's or variant's ctor.
    Ctor,
    /// A `const` item: no inputs, its type the output, its initializer the body. A use reads
    /// its value (`Def::Const`), never calls it.
    Const,
    /// A `static` item, as a `const` row: its use is a place (`Def::Static`), its initializer's
    /// value the static's first bytes; its kind is [`Front::static_item`].
    Static,
}

/// A body in blaze-typeck's form, and its string literals' bytes.
pub struct BodyRow {
    pub body: TypeckBody<'static>,
    /// The bytes of each string and byte string literal, by expression, sorted.
    strings: Vec<(tc::ExprId, Box<[u8], Global>), Global>,
    /// The value of each integer and char literal, by expression, sorted.
    ints: Vec<(tc::ExprId, u128), Global>,
    /// The magnitude of each integer and char literal pattern, by pattern, sorted.
    int_pats: Vec<(tc::PatId, u128), Global>,
    /// Every literal's written text, read through the view where it was written: what the
    /// constant evaluator reads literals from (blaze-consteval's `Lits`).
    pub lits: LitTexts,
}

/// The written texts of a body's literals, each at a range of `text`, by expression and by
/// pattern, sorted (ids ascend as they are pushed).
#[derive(Default)]
pub struct LitTexts {
    pub text: std::string::String,
    pub exprs: Vec<tc::ExprId, Global>,
    pub expr_ranges: Vec<tc::Range, Global>,
    pub pats: Vec<tc::PatId, Global>,
    pub pat_ranges: Vec<tc::Range, Global>,
}

/// A fn item: its signature in the program's table, its body if it has one.
pub struct FnRow<'f> {
    pub key: ItemKey,
    pub name: Name,
    /// Its path from the crate root, for messages: "util::double", "Out::line".
    pub path: Box<str, Global>,
    pub at: At,
    /// The item's text (outer attributes included): what its body's ranges are relative to.
    pub text: &'f str,
    pub kind: FnKind,
    container: Option<ItemKey>,
    env: Env,
    parent: u32,
    own: u32,
    self_param: bool,
    inputs: Vec<PTy, Global>,
    output: PTy,
    clauses: Vec<PClause, Global>,
    pub abi: Abi,
    /// Its ABI as a type (`extern "C-unwind"` is `Abi::C` to call, its own ABI as a type) and
    /// whether it is `unsafe`: what its item reifies to.
    header: tc::FnPtrHeader,
    /// The symbol an outside fn is called by.
    pub symbol: Box<str, Global>,
    /// The symbol it is exported as (`#[no_mangle]`, `#[export_name]`).
    pub export: Option<Box<str, Global>>,
    intrinsic: Option<Intrinsic>,
    pub body: Option<BodyRow>,
    sig: Option<Lowered<'static, 'f>>,
    /// A `const fn`: the constant evaluator interprets its calls.
    pub const_fn: bool,
    /// A `where const { COND }` condition of its owner (`kind` is `Const`): the condition's
    /// range in the owner's text, lowered as an anonymous constant over the owner's generics.
    pub condition: Option<Range>,
}

struct AdtRow {
    key: ItemKey,
    kind: AdtKind,
    generics: u32,
    variants: Vec<tc::Variant, Global>,
    fields: Vec<tc::Field, Global>,
}

struct TraitRow {
    key: ItemKey,
    generics: u32,
    lang: Option<LangItem>,
    supers: Vec<PClause, Global>,
    assoc: Vec<AssocTyDef, Global>,
}

struct ImplRow {
    key: ItemKey,
    generics: u32,
    self_ty: PTy,
    trait_: Option<ItemKey>,
    trait_args: tc::ArgsAfterSelf,
    assoc: Vec<(Name, PTy), Global>,
    clauses: Vec<PClause, Global>,
    fns: Vec<u32, Global>,
}

#[derive(Clone, Copy, Debug)]
enum ItemRef {
    Fn(u32),
    Adt(u32),
    Trait(u32),
    Impl(u32),
}

/// A pending item's lowered signature, kept for the second pass.
struct Pending<'f> {
    at: At,
    text: &'f str,
    sig: Lowered<'static, 'f>,
    module: u32,
}

/// A `use` import: its module, its path, the name it binds, where it is.
struct UseImport {
    module: u32,
    at: At,
    path: Vec<Name, Global>,
    bind: Name,
    range: Range,
}

/// No `cfg` is evaluated yet: one asked for is refused by the caller.
struct NoCfg;

impl Cfg for NoCfg {
    fn atom(&mut self, _: &str, _: Option<&str>) -> CfgAnswer {
        CfgAnswer::Need
    }
}

/// A body's written types, resolved where its fn's names hold; a refusal kept for its words.
struct BodyTypes<'n> {
    names: &'n Names,
    env: &'n Env,
    failed: Option<Why>,
}

impl Types for BodyTypes<'_> {
    fn ty(&mut self, view: &View<'_>, t: lf::TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        self.names.ty(tys, self.env, view, t).map_err(|why| {
            let range = match &why {
                Why::Unsupported(_, r) | Why::Program(_, r) => *r,
            };
            self.failed = Some(why);
            Refused::Form { what: "a written type", range }
        })
    }

    /// The old front evaluates no const argument.
    fn const_arg(&mut self, view: &View<'_>, c: lf::ConstIdx, _tys: &mut PortableTys<'_>) -> Result<tc::PTy, Refused> {
        Err(blaze_lower::typeck::no_const_arg(view, c))
    }

    fn repeat_len(&mut self, view: &View<'_>, len: tc::ExprId) -> Result<u64, Refused> {
        let r = view.expr_range(len)?;
        match int_value(view.text_at(r)?).and_then(|n| u64::try_from(n).ok()) {
            Some(n) => Ok(n),
            None => Err(Refused::Form { what: "a repeat length that is no integer literal", range: view.site(r)? }),
        }
    }
}

fn lower_sig<'f>(text: &'f str, at: At, r: Range, alloc: Alloc<'static>) -> Result<Lowered<'static, 'f>, Refusal> {
    let lowered = blaze_lower::lower_signature(text, EDITION, &mut NoCfg, alloc)
        .map_err(|need| Why::Unsupported("`cfg`", need.atoms.first().copied().unwrap_or(r)).at(at))?;
    check_lowered(&lowered.view(), at)?;
    Ok(lowered)
}

/// A lowering's own refusals: what it did not lower, and the errors it found.
fn check_lowered(v: &View<'_>, at: At) -> Result<(), Refusal> {
    if let Some(u) = v.unhandled().next() {
        let construct = syntax_name(blaze_file_items::syntax_kind(u.syntax)).unwrap_or("this construct");
        return Err(Why::Unsupported(construct, v.site(u.range).map_err(|t| Why::from(t).at(at))?).at(at));
    }
    if let Some(d) = v.diags().next() {
        let d = d.map_err(|b| Why::from(b).at(at))?;
        let r = v.site(d.range).map_err(|t| Why::from(t).at(at))?;
        return Err(program(format!("{:?}", d.kind), r).at(at));
    }
    if v.cfg_out() {
        return Err(Why::Unsupported("an item `cfg` turns off", Range::new(0, 0)).at(at));
    }
    Ok(())
}

/// The program of a crate's files: its names, items, signatures and bodies, and the world
/// the checker asks.
pub struct Front<'f> {
    pub files: &'f [SourceFile<'f>],
    /// The program's one type table: signatures' and bodies' types.
    pub tys: PortableTys<'static>,
    names: Names,
    pub fns: Vec<FnRow<'f>, Global>,
    adts: Vec<AdtRow, Global>,
    traits: Vec<TraitRow, Global>,
    impls: Vec<ImplRow, Global>,
    index: HashMap<ItemKey, ItemRef, Global>,
    members: HashMap<(ItemKey, Name), u32, Global>,
    lang: Vec<(LangItem, ItemKey), Global>,
    lang_paths: Vec<(LangItem, Def), Global>,
    fn_langs: Vec<(FnLang, ItemKey), Global>,
    /// Each `static` item's kind, by its row's key, in the order collected.
    statics: Vec<(ItemKey, StaticItem), Global>,
    /// Each ADT's `#[repr(..)]`, where it is not `repr(Rust)`.
    reprs: HashMap<ItemKey, ReprAttrs, Global>,
    module_traits: Vec<Vec<ItemKey, Global>, Global>,
    buckets: HashMap<(ItemKey, Shape), Vec<ItemKey, Global>, Global>,
    blankets: HashMap<ItemKey, Vec<ItemKey, Global>, Global>,
    inherent: HashMap<(Shape, Name), Vec<AssocItem, Global>, Global>,
    /// The fn the program starts at, by index in `fns`.
    pub entry: Option<usize>,
    /// The `blaze::` attributes read; handed to the caller by [`Front::new`].
    blaze_attrs: BlazeAttrs,
    /// The program's allocator, from [`Front::new`]'s caller: its outlines, type table and
    /// lowered bodies live in it, and a `where const` condition's scratch (`run::condition_holds`).
    pub(crate) alloc: Alloc<'static>,
}

/// The shape an impl's self type is bucketed by; `None` for a blanket impl.
fn shape_of(t: PTys<'_>, p: PTy) -> Option<Shape> {
    Some(match *t.get(p)? {
        PNode::Bool => Shape::Bool,
        PNode::Char => Shape::Char,
        PNode::Int(i) => Shape::Int(i),
        PNode::Uint(u) => Shape::Uint(u),
        PNode::Float(f) => Shape::Float(f),
        PNode::Str => Shape::Str,
        PNode::Never => Shape::Never,
        PNode::Tuple(l) => Shape::Tuple(l.len),
        PNode::Record(s, l) => Shape::Record(s.0, l.len),
        PNode::Union(l) => Shape::Union(l.len),
        PNode::Ref(m, _) => Shape::Ref(m),
        PNode::Ptr(m, _) => Shape::Ptr(m),
        PNode::Array(..) => Shape::Array,
        PNode::Slice(_) => Shape::Slice,
        PNode::Pat(..) => Shape::Pat,
        PNode::Adt(k, _) => Shape::Adt(k),
        PNode::Foreign(k) => Shape::Foreign(k),
        PNode::FnDef(k, _) => Shape::FnDef(k),
        PNode::FnPtr(l, _) => Shape::FnPtr(l.len.saturating_sub(1)),
        PNode::Opaque(k, _) => Shape::Opaque(k),
        PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => Shape::Closure,
        PNode::Param(_) | PNode::Alias { .. } | PNode::Infer | PNode::Error => return None,
        PNode::ConstValue { .. } | PNode::ConstParam(_) => return None,
        PNode::Dynamic(l) => Shape::Dynamic(blaze_typeck::object_principal(t, l)),
        PNode::ExTrait(..) | PNode::ExProj { .. } | PNode::ExAuto(_) => return None,
    })
}

impl<'f> Front<'f> {
    /// The program whose crate root is `root` among `files`, or the first refusal. Either way
    /// `blaze` gets the `blaze::` attributes read up to there (all of them when it is a program).
    /// `alloc` is the caller's, handed down from its entry; the program's tables live in it.
    pub fn new(files: &'f [SourceFile<'f>], root: &str, blaze: &mut BlazeAttrs, alloc: Alloc<'static>) -> Result<Front<'f>, Refusal> {
        let mut outlines: Vec<Outline<'static>, Global> = Vec::with_capacity_in(files.len(), Global);
        for (i, f) in files.iter().enumerate() {
            let o = blaze_file_items::outline(f.text, EDITION, alloc);
            if let Some(e) = o.errors().first() {
                return Err(Refusal {
                    file: Some(i as u32),
                    rule: RefusalRule::Syntax,
                    place: 0,
                    start: e.range.start,
                    end: e.range.end,
                    secondary: None,
                    construct: None,
                    message: format!("a syntax error: {}", o.message(e)).into_boxed_str(),
                });
            }
            outlines.push(o);
        }
        let root_file = files.iter().position(|f| f.path == root).ok_or_else(|| Refusal {
            file: None,
            rule: RefusalRule::Program,
            place: 0,
            start: 0,
            end: 0,
            secondary: None,
            construct: None,
            message: format!("the crate root `{root}` is not among the files").into_boxed_str(),
        })?;
        let mut front = Front {
            files,
            tys: PortableTys::new_in(alloc),
            names: Names {
                modules: Vec::new_in(Global),
                scope: HashMap::new_in(Global),
                adts: Vec::new_in(Global),
                traits: Vec::new_in(Global),
                sized: None,
            },
            fns: Vec::new_in(Global),
            adts: Vec::new_in(Global),
            traits: Vec::new_in(Global),
            impls: Vec::new_in(Global),
            index: HashMap::new_in(Global),
            members: HashMap::new_in(Global),
            lang: Vec::new_in(Global),
            lang_paths: Vec::new_in(Global),
            fn_langs: Vec::new_in(Global),
            statics: Vec::new_in(Global),
            reprs: HashMap::new_in(Global),
            module_traits: Vec::new_in(Global),
            buckets: HashMap::new_in(Global),
            blankets: HashMap::new_in(Global),
            inherent: HashMap::new_in(Global),
            entry: None,
            blaze_attrs: BlazeAttrs::default(),
            alloc,
        };
        let filled = front.fill(&outlines, root_file as u32);
        *blaze = core::mem::take(&mut front.blaze_attrs);
        filled.map(|()| front)
    }

    /// Every stage of the front end after the outlines, in order.
    fn fill(&mut self, outlines: &[Outline<'static>], root_file: u32) -> Result<(), Refusal> {
        self.modules(outlines, root_file)?;
        let pending = self.collect(outlines)?;
        self.signatures(pending)?;
        if let Some(first) = self.blaze_attrs.removed.first() {
            return Err(first.clone());
        }
        self.bodies()?;
        self.index_impls();
        self.entry = self.find_entry()?;
        Ok(())
    }

    /// The module tree from the root: inline modules and `mod name;` files.
    fn modules(&mut self, outlines: &[Outline<'static>], root: u32) -> Result<(), Refusal> {
        let root_path = self.files[root as usize].path;
        let dir = root_path.rfind('/').map_or("", |i| &root_path[..=i]);
        self.names.modules.push(Module { parent: None, file: root, holder: None, dir: dir.into(), path: "".into() });
        let mut used = Vec::new_in(Global);
        used.push(root);
        let mut m = 0usize;
        while m < self.names.modules.len() {
            let (file, holder) = (self.names.modules[m].file, self.names.modules[m].holder);
            let text = self.files[file as usize].text;
            let o = &outlines[file as usize];
            for (idx, item) in o.children(holder) {
                let ItemDetail::Mod { inline, path } = item.detail else { continue };
                let at = At { file, place: item.range.start };
                let name_r = item.name.unwrap_or_default();
                let name = item.text(text, name_r);
                let name = name.strip_prefix("r#").unwrap_or(name);
                if path.is_some() {
                    return Err(Why::Unsupported("`#[path]` on a module", name_r).at(at));
                }
                let parent = &self.names.modules[m];
                let dir: Box<str, Global> = format!("{}{name}/", parent.dir).into_boxed_str();
                let mpath: Box<str, Global> =
                    if parent.path.is_empty() { name.into() } else { format!("{}::{name}", parent.path).into_boxed_str() };
                let child = if inline {
                    Module { parent: Some(m as u32), file, holder: Some(idx), dir, path: mpath }
                } else {
                    let flat = format!("{}{name}.rs", parent.dir);
                    let nested = format!("{}{name}/mod.rs", parent.dir);
                    let a = self.files.iter().position(|f| f.path == flat);
                    let b = self.files.iter().position(|f| f.path == nested);
                    let f = match (a, b) {
                        (Some(f), None) | (None, Some(f)) => f as u32,
                        (None, None) => return Err(program(format!("`mod {name};` has no file `{flat}` or `{nested}`"), name_r).at(at)),
                        (Some(_), Some(_)) => return Err(program(format!("`mod {name};` has both `{flat}` and `{nested}`"), name_r).at(at)),
                    };
                    if used.contains(&f) {
                        return Err(program(format!("`mod {name};` names a file already a module"), name_r).at(at));
                    }
                    used.push(f);
                    Module { parent: Some(m as u32), file: f, holder: None, dir, path: mpath }
                };
                let c = self.names.modules.len() as u32;
                self.names.modules.push(child);
                self.bind(m as u32, Name::of(name), Res::Mod(c), at, name_r, name)?;
            }
            m += 1;
        }
        self.module_traits = (0..self.names.modules.len()).map(|_| Vec::new_in(Global)).collect();
        Ok(())
    }

    fn bind(&mut self, module: u32, name: Name, res: Res, at: At, r: Range, shown: &str) -> Result<(), Refusal> {
        if self.names.scope.insert((module, name, res.ns()), res).is_some() {
            return Err(program(format!("a second item named `{shown}` in one module"), r).at(at));
        }
        if let Res::Trait(t) = res {
            let key = self.names.traits[t as usize].key;
            self.module_traits[module as usize].push(key);
        }
        Ok(())
    }

    fn item_path(&self, module: u32, name: &str) -> Box<str, Global> {
        let m = &self.names.modules[module as usize].path;
        if m.is_empty() { name.into() } else { format!("{m}::{name}").into_boxed_str() }
    }

    /// Every item of every module: its key, its name bound, its signature lowered.
    fn collect(&mut self, outlines: &[Outline<'static>]) -> Result<Collected<'f>, Refusal> {
        let alloc = self.alloc;
        let mut c = Collected {
            adts: Vec::new_in(Global),
            traits: Vec::new_in(Global),
            impls: Vec::new_in(Global),
            uses: Vec::new_in(Global),
        };
        for m in 0..self.names.modules.len() as u32 {
            let (file, holder) = (self.names.modules[m as usize].file, self.names.modules[m as usize].holder);
            let text = self.files[file as usize].text;
            let o = &outlines[file as usize];
            let mut impls_seen = 0u32;
            for (idx, item) in o.children(holder) {
                let at = At { file, place: item.range.start };
                let item_text = item.range.of(text);
                let name_r = item.name.unwrap_or_default();
                let name = item.name.map(|r| item.text(text, r)).map(|n| n.strip_prefix("r#").unwrap_or(n));
                let whole = Range::new(0, item.range.end - item.range.start);
                let mpath = self.names.modules[m as usize].path.clone();
                let kind_key = |kind: u8, n: &str, d: u32| key_of(&[mpath.as_bytes(), &[kind], n.as_bytes(), &d.to_le_bytes()]);
                match item.detail {
                    ItemDetail::Mod { .. } => {}
                    ItemDetail::Fn { .. } => {
                        let name = name.unwrap_or("");
                        let key = kind_key(1, name, item.disambiguator);
                        let sig = lower_sig(item_text, at, whole, alloc)?;
                        let row = self.fn_row(key, name, self.item_path(m, name), at, item_text, FnKind::Free, None, Env::new(m), 0, sig)?;
                        let i = self.fns.len() as u32;
                        self.fns.push(row);
                        self.bind(m, Name::of(name), Res::Fn(i), at, name_r, name)?;
                    }
                    ItemDetail::Const { .. } => {
                        let name = name.unwrap_or("");
                        let key = kind_key(6, name, item.disambiguator);
                        let sig = lower_sig(item_text, at, whole, alloc)?;
                        let row = self.fn_row(key, name, self.item_path(m, name), at, item_text, FnKind::Const, None, Env::new(m), 0, sig)?;
                        let i = self.fns.len() as u32;
                        self.fns.push(row);
                        self.bind(m, Name::of(name), Res::Fn(i), at, name_r, name)?;
                    }
                    ItemDetail::Static { .. } => {
                        let name = name.unwrap_or("");
                        let key = kind_key(8, name, item.disambiguator);
                        let sig = lower_sig(item_text, at, whole, alloc)?;
                        let row = self.fn_row(key, name, self.item_path(m, name), at, item_text, FnKind::Static, None, Env::new(m), 0, sig)?;
                        let i = self.fns.len() as u32;
                        self.fns.push(row);
                        self.bind(m, Name::of(name), Res::Fn(i), at, name_r, name)?;
                    }
                    ItemDetail::Struct | ItemDetail::Enum => {
                        let name = name.unwrap_or("");
                        let key = kind_key(2, name, item.disambiguator);
                        let sig = lower_sig(item_text, at, whole, alloc)?;
                        let a = self.adt_head(key, name, &sig.view(), at)?;
                        let i = self.names.adts.len() as u32;
                        self.names.adts.push(a);
                        self.bind(m, Name::of(name), Res::Adt(i), at, name_r, name)?;
                        c.adts.push(Pending { at, text: item_text, sig, module: m });
                    }
                    ItemDetail::Trait => {
                        let name = name.unwrap_or("");
                        let key = kind_key(3, name, item.disambiguator);
                        let sig = lower_sig(item_text, at, whole, alloc)?;
                        let v = sig.view();
                        if v.head[head::FLAGS] & (sig::AUTO | sig::TRAIT_ALIAS) != 0 {
                            return Err(Why::Unsupported("an auto trait or a trait alias", whole).at(at));
                        }
                        // `Sized` is known before any signature is read: each parameter's
                        // implied bound names it.
                        if let Some(("sized", _)) = read_attrs(&v, v.list(head::ATTRS), at, &mut self.blaze_attrs).map_err(|w| w.at(at))?.lang {
                            self.names.sized = Some(key);
                        }
                        let mut env = Env::new(m);
                        let ti = self.names.traits.len() as u32;
                        env.self_ = SelfTy::Trait(ti);
                        let own = generic_names(&v, 1, &mut env.params).map_err(|w| w.at(at))?;
                        let mut head_ = TraitHead {
                            key,
                            generics: own + 1,
                            assoc: Vec::new_in(Global),
                            defaults: Vec::new_in(Global),
                            fns: Vec::new_in(Global),
                        };
                        let mut members = Vec::new_in(Global);
                        for (_, child) in o.children(Some(idx)) {
                            members.push(*child);
                        }
                        self.names.traits.push(TraitHead { key, generics: own + 1, assoc: Vec::new_in(Global), defaults: Vec::new_in(Global), fns: Vec::new_in(Global) });
                        for child in &members {
                            let cat = At { file, place: child.range.start };
                            let ctext = child.range.of(text);
                            let cname = child.name.map(|r| child.text(text, r)).unwrap_or("");
                            let cwhole = Range::new(0, child.range.end - child.range.start);
                            match child.detail {
                                ItemDetail::Fn { .. } => {
                                    let ckey = member_key(key, 1, cname);
                                    let csig = lower_sig(ctext, cat, cwhole, alloc)?;
                                    let path = self.item_path(m, &format!("{name}::{cname}"));
                                    let row = self.fn_row(ckey, cname, path, cat, ctext, FnKind::Trait(ti), Some(key), env.copy_in(), own + 1, csig)?;
                                    head_.fns.push((Name::of(cname), ckey));
                                    let i = self.fns.len() as u32;
                                    self.fns.push(row);
                                    self.members.insert((key, Name::of(cname)), i);
                                }
                                ItemDetail::TypeAlias => {
                                    let csig = lower_sig(ctext, cat, cwhole, alloc)?;
                                    let cv = csig.view();
                                    if cv.head_opt(head::TY).is_some() {
                                        return Err(Why::Unsupported("an associated type's default", cwhole).at(cat));
                                    }
                                    if cv.list(head::GENERICS).len != 0 {
                                        return Err(Why::Unsupported("a generic associated type", cwhole).at(cat));
                                    }
                                    head_.assoc.push(Name::of(cname));
                                }
                                _ => return Err(Why::Unsupported(detail_name(child.detail), cwhole).at(cat)),
                            }
                        }
                        self.names.traits[ti as usize] = head_;
                        self.bind(m, Name::of(name), Res::Trait(ti), at, name_r, name)?;
                        c.traits.push((Pending { at, text: item_text, sig, module: m }, env));
                    }
                    ItemDetail::Impl => {
                        let key = kind_key(4, "", impls_seen);
                        impls_seen += 1;
                        let sig = lower_sig(item_text, at, whole, alloc)?;
                        let v = sig.view();
                        if v.head[head::FLAGS] & sig::NEGATIVE != 0 {
                            return Err(Why::Unsupported("a negative impl", whole).at(at));
                        }
                        let mut env = Env::new(m);
                        let generics = generic_names(&v, 0, &mut env.params).map_err(|w| w.at(at))?;
                        let ii = c.impls.len() as u32;
                        let mut fns = Vec::new_in(Global);
                        let mut assoc = Vec::new_in(Global);
                        let mut members = Vec::new_in(Global);
                        for (_, child) in o.children(Some(idx)) {
                            members.push(*child);
                        }
                        for child in &members {
                            let cat = At { file, place: child.range.start };
                            let ctext = child.range.of(text);
                            let cname = child.name.map(|r| child.text(text, r)).unwrap_or("");
                            let cwhole = Range::new(0, child.range.end - child.range.start);
                            match child.detail {
                                ItemDetail::Fn { .. } => {
                                    let ckey = member_key(key, 1, cname);
                                    let csig = lower_sig(ctext, cat, cwhole, alloc)?;
                                    let row = self.fn_row(ckey, cname, cname.into(), cat, ctext, FnKind::Impl(ii), Some(key), env.copy_in(), generics, csig)?;
                                    let i = self.fns.len() as u32;
                                    fns.push(i);
                                    self.fns.push(row);
                                    self.members.insert((key, Name::of(cname)), i);
                                }
                                ItemDetail::TypeAlias => {
                                    let csig = lower_sig(ctext, cat, cwhole, alloc)?;
                                    assoc.push((Name::of(cname), Pending { at: cat, text: ctext, sig: csig, module: m }));
                                }
                                _ => return Err(Why::Unsupported(detail_name(child.detail), cwhole).at(cat)),
                            }
                        }
                        c.impls.push(PendingImpl { key, generics, env, item: Pending { at, text: item_text, sig, module: m }, fns, assoc });
                    }
                    ItemDetail::ExternBlock { abi } => {
                        let abi_text = abi.map(|r| item.text(text, r)).unwrap_or("extern");
                        let abi = match abi_text.trim_start_matches("extern").trim() {
                            "" | "\"C\"" => Abi::C,
                            _ => return Err(Why::Unsupported("an extern block of another ABI than \"C\"", abi.unwrap_or(whole)).at(at)),
                        };
                        let mut members = Vec::new_in(Global);
                        for (_, child) in o.children(Some(idx)) {
                            members.push(*child);
                        }
                        for child in &members {
                            let cat = At { file, place: child.range.start };
                            let ctext = child.range.of(text);
                            let cname = child.name.map(|r| child.text(text, r)).unwrap_or("");
                            let cname_r = child.name.unwrap_or_default();
                            let cwhole = Range::new(0, child.range.end - child.range.start);
                            let ItemDetail::Fn { .. } = child.detail else {
                                return Err(Why::Unsupported(detail_name(child.detail), cwhole).at(cat));
                            };
                            let ckey = kind_key(5, cname, child.disambiguator);
                            let csig = lower_sig(ctext, cat, cwhole, alloc)?;
                            let mut row = self.fn_row(ckey, cname, self.item_path(m, cname), cat, ctext, FnKind::Foreign, None, Env::new(m), 0, csig)?;
                            row.abi = abi;
                            let i = self.fns.len() as u32;
                            self.fns.push(row);
                            self.bind(m, Name::of(cname), Res::Fn(i), cat, cname_r, cname)?;
                        }
                    }
                    ItemDetail::Use { tree } => {
                        let tree = tree.ok_or(Why::Unsupported("a `use` with no tree", whole)).map_err(|w| w.at(at))?;
                        parse_use(item.text(text, tree), tree, m, at, &mut c.uses)?;
                    }
                    other => return Err(Why::Unsupported(detail_name(other), whole).at(at)),
                }
            }
        }
        self.uses(&c.uses)?;
        Ok(c)
    }

    /// A fn's row from its signature, its types not resolved yet; its attributes read.
    #[expect(clippy::too_many_arguments, reason = "a row's fields, each from its own source")]
    fn fn_row(
        &mut self,
        key: ItemKey,
        name: &str,
        path: Box<str, Global>,
        at: At,
        text: &'f str,
        kind: FnKind,
        container: Option<ItemKey>,
        mut env: Env,
        parent: u32,
        sig: Lowered<'static, 'f>,
    ) -> Result<FnRow<'f>, Refusal> {
        let v = sig.view();
        let whole = Range::new(0, text.len() as u32);
        let flags = v.head[head::FLAGS];
        if flags & sig::ASYNC != 0 {
            return Err(Why::Unsupported("an `async fn`", whole).at(at));
        }
        if flags & (sig::GEN | sig::VARIADIC) != 0 {
            return Err(Why::Unsupported("a `gen fn` or a variadic fn", whole).at(at));
        }
        let own = generic_names(&v, parent, &mut env.params).map_err(|w| w.at(at))?;
        let attrs = read_attrs(&v, v.list(head::ATTRS), at, &mut self.blaze_attrs).map_err(|w| w.at(at))?;
        let (abi, type_abi) = match v.head[head::ABI] {
            u32::MAX => (Abi::Rust, tc::Abi::Rust),
            x if x == u32::MAX - 1 => (Abi::C, tc::Abi::C),
            _ => match v.abi().map(|r| v.text_at(r).map(unquote)) {
                Some(Ok("C")) => (Abi::C, tc::Abi::C),
                // The C convention; unwinding out of it is allowed, and it is its own type.
                Some(Ok("C-unwind")) => (Abi::C, tc::Abi::CUnwind),
                Some(Ok("Rust")) => (Abi::Rust, tc::Abi::Rust),
                _ => return Err(Why::Unsupported("an ABI other than \"C\", \"C-unwind\" and \"Rust\"", whole).at(at)),
            },
        };
        let safety = if flags & sig::UNSAFE != 0 { tc::Safety::Unsafe } else { tc::Safety::Safe };
        let header = tc::FnPtrHeader { abi: type_abi, safety, c_variadic: false };
        let export = match (attrs.export_name, attrs.no_mangle) {
            (Some(e), _) => Some(e.into()),
            (None, true) => Some(name.into()),
            (None, false) => None,
        };
        if kind == FnKind::Static {
            self.statics.push((key, StaticItem { thread_local: attrs.thread_local, mutable: flags & sig::MUT != 0 }));
        } else if attrs.thread_local {
            return Err(Why::Unsupported("`#[thread_local]` on other than a `static`", whole).at(at));
        }
        let mut intrinsic = None;
        if attrs.intrinsic {
            // Every intrinsic the build's table names; the walk lowers each.
            let which = blaze_build::intrinsics::Which::named(name).map_err(|_| Why::Unsupported("this compiler intrinsic", whole).at(at))?;
            intrinsic = Some(Intrinsic::Table(which));
        }
        if let Some((lang, r)) = attrs.lang {
            match lang {
                _ if let Some(l) = FnLang::named(lang) => self.fn_langs.push((l, key)),
                "branch" => self.lang_paths.push((LangItem::TryTraitBranch, Def::Fn(key))),
                "from_residual" => self.lang_paths.push((LangItem::TryTraitFromResidual, Def::Fn(key))),
                "from_output" => self.lang_paths.push((LangItem::TryTraitFromOutput, Def::Fn(key))),
                "into_iter" => self.lang_paths.push((LangItem::IntoIterIntoIter, Def::Fn(key))),
                "next" => self.lang_paths.push((LangItem::IteratorNext, Def::Fn(key))),
                l if UNUSED_LANG.contains(&l) => {}
                _ => return Err(Why::Unsupported("this `#[lang]` item", r).at(at)),
            }
        }
        let unit = intern(&mut self.tys, PNode::Tuple(List::EMPTY)).map_err(|w| w.at(at))?;
        Ok(FnRow {
            key,
            name: Name::of(name),
            path,
            at,
            text,
            kind,
            container,
            env,
            parent,
            own,
            self_param: false,
            inputs: Vec::new_in(Global),
            output: unit,
            clauses: Vec::new_in(Global),
            abi,
            header,
            symbol: attrs.link_name.unwrap_or(name).into(),
            export,
            intrinsic,
            body: None,
            sig: Some(sig),
            const_fn: flags & sig::CONST != 0,
            condition: None,
        })
    }

    /// An ADT's names: its kind, generics and variants.
    fn adt_head(&mut self, key: ItemKey, name: &str, v: &View<'_>, at: At) -> Result<AdtHead, Refusal> {
        let mut params = Vec::new_in(Global);
        let generics = generic_names(v, 0, &mut params).map_err(|w| w.at(at))?;
        let attrs = read_attrs(v, v.list(head::ATTRS), at, &mut self.blaze_attrs).map_err(|w| w.at(at))?;
        if attrs.repr != ReprAttrs::RUST {
            self.reprs.insert(key, attrs.repr);
        }
        let mut variants = Vec::new_in(Global);
        let syntax = blaze_file_items::syntax_kind(v.head[head::SYNTAX] as u16);
        let kind = if syntax == parser::SyntaxKind::ENUM { AdtKind::Enum } else { AdtKind::Struct };
        if let Some((lang, r)) = attrs.lang {
            let l = match lang {
                "Range" => LangItem::Range,
                "RangeFrom" => LangItem::RangeFrom,
                "RangeTo" => LangItem::RangeTo,
                "RangeFull" => LangItem::RangeFull,
                "RangeToInclusive" => LangItem::RangeToInclusive,
                // Its field is never dropped: the build asks for it by `Adts::lang_adt`.
                "manually_drop" => LangItem::ManuallyDrop,
                l if UNUSED_LANG.contains(&l) => LangItem::Sized,
                _ => return Err(Why::Unsupported("this `#[lang]` item", r).at(at)),
            };
            if l == LangItem::ManuallyDrop {
                self.lang.push((l, key));
            } else if l != LangItem::Sized {
                self.lang_paths.push((l, Def::Adt(key)));
            }
        }
        match kind {
            AdtKind::Enum => {
                for (i, &n) in v.rows(v.list(head::FIELDS)).map_err(|b| Why::from(b).at(at))?.iter().enumerate() {
                    let var = lf::Variant::decode(n).map_err(|b| Why::from(b).at(at))?;
                    let r = place(v, v.row_range(v.list(head::FIELDS).start + i as u32)).map_err(|w| w.at(at))?;
                    if var.discriminant.is_some() {
                        return Err(Why::Unsupported("an explicit discriminant", r).at(at));
                    }
                    let ctor = match var.shape {
                        lf::Shape::Record => CtorKind::Braces,
                        lf::Shape::Tuple => CtorKind::Tuple,
                        lf::Shape::Unit => CtorKind::Unit,
                    };
                    let vattrs = read_attrs(v, var.attrs, at, &mut self.blaze_attrs).map_err(|w| w.at(at))?;
                    if let Some((lang, lr)) = vattrs.lang {
                        let def = Def::Ctor { adt: key, variant: i as u32, kind: ctor };
                        match lang {
                            "Continue" => self.lang_paths.push((LangItem::ControlFlowContinue, def)),
                            "Break" => self.lang_paths.push((LangItem::ControlFlowBreak, def)),
                            "Some" => self.lang_paths.push((LangItem::OptionSome, def)),
                            "None" => self.lang_paths.push((LangItem::OptionNone, def)),
                            "Ok" | "Err" => {}
                            _ => return Err(Why::Unsupported("this `#[lang]` item", lr).at(at)),
                        }
                    }
                    variants.push((Name(v.name(var.name).map_err(|b| Why::from(b).at(at))?), ctor));
                }
            }
            _ => {
                let ctor = match v.head[head::SHAPE] {
                    0 => CtorKind::Braces,
                    1 => CtorKind::Tuple,
                    _ => CtorKind::Unit,
                };
                variants.push((Name::of(name), ctor));
            }
        }
        Ok(AdtHead { key, kind, generics, variants })
    }

    /// Each `use` bound in its module, in passes until every one names its item.
    fn uses(&mut self, uses: &[UseImport]) -> Result<(), Refusal> {
        let mut done = Vec::with_capacity_in(uses.len(), Global);
        done.resize(uses.len(), false);
        loop {
            let mut progress = false;
            for (i, u) in uses.iter().enumerate() {
                if done[i] {
                    continue;
                }
                // A path names an item in the type namespace and one in the value namespace
                // (a unit struct is both): each found is bound.
                let mut found = false;
                for ns in [Ns::Type, Ns::Value] {
                    if let Some((res, n)) = self.names.walk(u.module, &u.path, ns) {
                        if n as usize == u.path.len() && res.ns() == ns && self.names.find(u.module, u.bind, ns) != Some(res) {
                            let shown = format!("{:#x}", u.bind.0);
                            self.bind(u.module, u.bind, res, u.at, u.range, &shown)?;
                            found = true;
                        } else if n as usize == u.path.len() && self.names.find(u.module, u.bind, res.ns()) == Some(res) {
                            found = true;
                        }
                    }
                }
                if found {
                    done[i] = true;
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }
        if let Some(i) = done.iter().position(|d| !d) {
            let u = &uses[i];
            return Err(program(format!("this `use` names nothing"), u.range).at(u.at));
        }
        Ok(())
    }
}

struct PendingImpl<'f> {
    key: ItemKey,
    generics: u32,
    env: Env,
    item: Pending<'f>,
    fns: Vec<u32, Global>,
    assoc: Vec<(Name, Pending<'f>), Global>,
}

struct Collected<'f> {
    adts: Vec<Pending<'f>, Global>,
    traits: Vec<(Pending<'f>, Env), Global>,
    impls: Vec<PendingImpl<'f>, Global>,
    uses: Vec<UseImport, Global>,
}

/// The plain name of an item kind the front end refuses.
pub(crate) fn detail_name(d: ItemDetail) -> &'static str {
    match d {
        ItemDetail::Union => "a union",
        ItemDetail::TraitAlias => "a trait alias",
        ItemDetail::Const { .. } => "a `const` item",
        ItemDetail::Static { .. } => "a `static` item",
        ItemDetail::TypeAlias => "a type alias",
        ItemDetail::ExternCrate { .. } => "`extern crate`",
        ItemDetail::MacroRules { .. } => "`macro_rules!`",
        ItemDetail::MacroDef => "a `macro` definition",
        ItemDetail::MacroCall { .. } => "a macro call in item position",
        ItemDetail::Impl => "an impl",
        _ => "this item",
    }
}

/// The imports of the use tree `tree` (its text, at `range` of the item at `at`), with `{..}`
/// groups, `self` in a group and `as`; a glob is refused.
fn parse_use(tree: &str, range: Range, module: u32, at: At, out: &mut Vec<UseImport, Global>) -> Result<(), Refusal> {
    let mut path: Vec<&str, Global> = Vec::new_in(Global);
    let mut groups: Vec<usize, Global> = Vec::new_in(Global);
    let mut alias: Option<&str> = None;
    let mut leaf = false;
    let mut rest = tree;
    let emit = |path: &[&str], alias: Option<&str>, out: &mut Vec<UseImport, Global>| -> Result<(), Refusal> {
        let (segs, last) = match path.split_last() {
            Some((&"self", prefix)) => (prefix, prefix.last().copied()),
            Some((&l, _)) => (path, Some(l)),
            None => (path, None),
        };
        let Some(last) = alias.or(last) else { return Err(program(format!("an empty `use`"), range).at(at)) };
        if last == "_" {
            return Err(Why::Unsupported("`use .. as _`", range).at(at));
        }
        let mut names = Vec::with_capacity_in(segs.len(), Global);
        names.extend(segs.iter().map(|s| Name::of(s.strip_prefix("r#").unwrap_or(s))));
        out.push(UseImport { module, at, path: names, bind: Name::of(last.strip_prefix("r#").unwrap_or(last)), range });
        Ok(())
    };
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some(r) = rest.strip_prefix("::") {
            rest = r;
        } else if let Some(r) = rest.strip_prefix('{') {
            groups.push(path.len());
            rest = r;
        } else if rest.starts_with(',') || rest.starts_with('}') {
            if leaf {
                emit(&path, alias.take(), out)?;
            }
            leaf = false;
            let top = groups.last().copied().unwrap_or(0);
            if rest.starts_with('}') {
                groups.pop();
            }
            path.truncate(top);
            rest = &rest[1..];
        } else if rest.starts_with('*') {
            return Err(Why::Unsupported("a glob import (`use a::*`)", range).at(at));
        } else {
            let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '#')).unwrap_or(rest.len());
            if end == 0 {
                return Err(program(format!("a `use` this front end cannot read"), range).at(at));
            }
            let word = &rest[..end];
            rest = &rest[end..];
            if word == "as" {
                rest = rest.trim_start();
                let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '#')).unwrap_or(rest.len());
                alias = Some(&rest[..end]);
                rest = &rest[end..];
            } else {
                path.push(word);
                leaf = true;
            }
        }
    }
    if leaf {
        emit(&path, alias.take(), out)?;
    }
    Ok(())
}

impl<'f> Front<'f> {
    /// Every signature's types, in the order the later ones read the earlier: trait
    /// parameters' defaults, ADTs, impls (self type, trait, associated types), traits'
    /// supertraits, then fns.
    fn signatures(&mut self, c: Collected<'f>) -> Result<(), Refusal> {
        // Trait parameters' defaults.
        for (p, env) in &c.traits {
            let v = p.sig.view();
            let mut defaults = Vec::new_in(Global);
            for &n in v.rows(v.list(head::GENERICS)).map_err(|b| Why::from(b).at(p.at))? {
                if let lf::GenericParam::Type { default, .. } = lf::GenericParam::decode(n).map_err(|b| Why::from(b).at(p.at))? {
                    let d = match default {
                        Some(t) => Some(self.names.ty(&mut self.tys, env, &v, t).map_err(|w| w.at(p.at))?),
                        None => None,
                    };
                    defaults.push(d);
                }
            }
            let SelfTy::Trait(ti) = env.self_ else { continue };
            self.names.traits[ti as usize].defaults = defaults;
        }
        // ADTs.
        for (a, p) in c.adts.iter().enumerate() {
            self.adt_sig(a as u32, p)?;
        }
        // Impls.
        for imp in &c.impls {
            self.impl_(imp)?;
        }
        // Traits' supertraits and where clauses on `Self`.
        for (p, env) in &c.traits {
            let SelfTy::Trait(ti) = env.self_ else { continue };
            let v = p.sig.view();
            let me = intern(&mut self.tys, PNode::Param(0)).map_err(|w| w.at(p.at))?;
            let mut supers = Vec::new_in(Global);
            for &b in v.rows(v.list(head::BOUNDS)).map_err(|b| Why::from(b).at(p.at))? {
                let b = lf::Bound::decode(b).map_err(|b| Why::from(b).at(p.at))?;
                self.names.bound(&mut self.tys, env, &v, b, me, &mut supers).map_err(|w| w.at(p.at))?;
            }
            for &n in v.rows(v.list(head::GENERICS)).map_err(|b| Why::from(b).at(p.at))? {
                if let lf::GenericParam::Type { bounds, .. } = lf::GenericParam::decode(n).map_err(|b| Why::from(b).at(p.at))? {
                    if bounds.len != 0 {
                        return Err(Why::Unsupported("a bound on a trait's parameter", Range::new(0, 0)).at(p.at));
                    }
                }
            }
            if v.list(head::WHERE).len != 0 {
                return Err(Why::Unsupported("a where clause on a trait", Range::new(0, 0)).at(p.at));
            }
            let attrs = read_attrs(&v, v.list(head::ATTRS), p.at, &mut self.blaze_attrs).map_err(|w| w.at(p.at))?;
            let head_ = &self.names.traits[ti as usize];
            let mut lang = None;
            if let Some((l, r)) = attrs.lang {
                match trait_lang(l) {
                    Some(l) => lang = Some(l),
                    None if UNUSED_LANG.contains(&l) => {}
                    None => return Err(Why::Unsupported("this `#[lang]` item", r).at(p.at)),
                }
            }
            let mut assoc = Vec::with_capacity_in(head_.assoc.len(), Global);
            assoc.extend(
                head_.assoc.iter().map(|&name| AssocTyDef { name, item: ItemKey(tc::infer::assoc_def(head_.key, name)), default: None }),
            );
            let key = head_.key;
            self.index.insert(key, ItemRef::Trait(self.traits.len() as u32));
            self.traits.push(TraitRow { key, generics: head_.generics, lang, supers, assoc });
        }
        // Fns.
        for i in 0..self.fns.len() {
            self.fn_sig(i)?;
        }
        Ok(())
    }

    fn adt_sig(&mut self, a: u32, p: &Pending<'f>) -> Result<(), Refusal> {
        let v = p.sig.view();
        let at = p.at;
        let bad = |b: lf::BadRow| Why::from(b).at(at);
        let (key, kind, generics) = {
            let h = &self.names.adts[a as usize];
            (h.key, h.kind, h.generics)
        };
        let mut env = Env::new(p.module);
        generic_names(&v, 0, &mut env.params).map_err(|w| w.at(at))?;
        let params = params_of(&mut self.tys, 0, generics).map_err(|w| w.at(at))?;
        let pl = intern_list(&mut self.tys, &params).map_err(|w| w.at(at))?;
        let self_ty = intern(&mut self.tys, PNode::Adt(key, pl)).map_err(|w| w.at(at))?;
        env.self_ = SelfTy::Ty(self_ty);
        let mut variants = Vec::new_in(Global);
        let mut fields = Vec::new_in(Global);
        let mut sized = Vec::new_in(Global);
        if let Some(s) = self.names.sized {
            for &p in &params {
                let args = intern_list(&mut self.tys, &[p]).map_err(|w| w.at(at))?;
                sized.push(PClause::Trait { trait_: s, args });
            }
        }
        // Each variant's fields: (name, shape, field rows); a struct's one variant is itself.
        let mut shapes = Vec::new_in(Global);
        match kind {
            AdtKind::Enum => {
                for &n in v.rows(v.list(head::FIELDS)).map_err(bad)? {
                    let var = lf::Variant::decode(n).map_err(bad)?;
                    shapes.push((var.name, var.fields));
                }
            }
            _ => shapes.push((lf::NameIdx(u32::MAX), v.list(head::FIELDS))),
        }
        for (i, &(vname, rows)) in shapes.iter().enumerate() {
            let (name, ctor) = self.names.adts[a as usize].variants[i];
            let vkey = match kind {
                AdtKind::Enum => member_key(key, 6, &format!("{:#x}", name.0)),
                _ => member_key(key, 7, "ctor"),
            };
            let _ = vname;
            let start = fields.len() as u32;
            let mut tys_ = Vec::new_in(Global);
            for (j, &n) in v.rows(rows).map_err(bad)?.iter().enumerate() {
                let f = lf::Field::decode(n).map_err(bad)?;
                let ty = self.names.ty(&mut self.tys, &env, &v, f.ty).map_err(|w| w.at(at))?;
                let fname = match f.name {
                    Some(n) => Name(v.name(n).map_err(bad)?),
                    None => Name::of_index(j as u32),
                };
                fields.push(tc::Field { name: fname, ty });
                tys_.push(ty);
            }
            variants.push(tc::Variant { key: vkey, name, ctor, fields: List { start, len: fields.len() as u32 - start } });
            if ctor == CtorKind::Tuple {
                let path = self.item_path(p.module, "(ctor)");
                let i = self.fns.len() as u32;
                self.fns.push(FnRow {
                    key: vkey,
                    name,
                    path,
                    at,
                    text: p.text,
                    kind: FnKind::Ctor,
                    container: None,
                    env: env.copy_in(),
                    parent: 0,
                    own: generics,
                    self_param: false,
                    inputs: tys_,
                    output: self_ty,
                    clauses: sized.to_vec_in(*sized.allocator()),
                    abi: Abi::Rust,
                    header: tc::FnPtrHeader::RUST,
                    symbol: "".into(),
                    export: None,
                    intrinsic: None,
                    body: None,
                    sig: None,
                    const_fn: false,
                    condition: None,
                });
                self.index.insert(vkey, ItemRef::Fn(i));
            }
        }
        self.index.insert(key, ItemRef::Adt(self.adts.len() as u32));
        self.adts.push(AdtRow { key, kind, generics, variants, fields });
        Ok(())
    }

    /// The `where const { COND }` conditions of item `owner` (generics `0..count`, read in
    /// `env`): one clause each, `PClause::Const` over the owner's own parameters, and one item
    /// each, the condition lowered as an anonymous constant of the owner's `text`, of type
    /// `bool`, under the owner's clauses (its own condition among them, as a body assumes its
    /// item's where-clauses).
    #[expect(clippy::too_many_arguments, reason = "the owner's parts, each from its own pass")]
    fn conditions(
        &mut self,
        owner: ItemKey,
        path: Box<str, Global>,
        at: At,
        text: &'f str,
        v: &View<'_>,
        env: &Env,
        count: u32,
        clauses: &mut Vec<PClause, Global>,
    ) -> Result<(), Refusal> {
        let mut made = Vec::new_in(Global);
        let rows = v.rows(v.list(head::WHERE)).map_err(|b| Why::from(b).at(at))?;
        for (k, &n) in rows.iter().enumerate() {
            let lf::WherePred::Const { cond } = lf::WherePred::decode(n).map_err(|b| Why::from(b).at(at))? else { continue };
            let range = place(v, v.const_range(cond)).map_err(|w| w.at(at))?;
            // The clause's range is the whole `const { COND }`; its value is the block.
            let written = range.of(text);
            let Some(open) = written.strip_prefix("const").and_then(|rest| rest.find('{').map(|o| o + 5)) else {
                return Err(program(format!("a `where const` condition that is no `const {{ .. }}` block"), range).at(at));
            };
            let range = Range::new(range.start + open as u32, range.end);
            let key = key_of(&[&owner.0.to_le_bytes(), b"where const", &(k as u32).to_le_bytes()]);
            let ps = params_of(&mut self.tys, 0, count).map_err(|w| w.at(at))?;
            let args = intern_list(&mut self.tys, &ps).map_err(|w| w.at(at))?;
            clauses.push(PClause::Const { cond: key, args });
            made.push((key, range));
        }
        if made.is_empty() {
            return Ok(());
        }
        let bool_ = intern(&mut self.tys, PNode::Bool).map_err(|w| w.at(at))?;
        for (key, range) in made {
            let i = self.fns.len() as u32;
            self.fns.push(FnRow {
                key,
                name: Name::of("{where const}"),
                path: path.clone(),
                at,
                text,
                kind: FnKind::Const,
                container: None,
                env: env.copy_in(),
                parent: count,
                own: 0,
                self_param: false,
                inputs: Vec::new_in(Global),
                output: bool_,
                clauses: clauses.to_vec_in(*clauses.allocator()),
                abi: Abi::Rust,
                header: tc::FnPtrHeader::RUST,
                symbol: "".into(),
                export: None,
                intrinsic: None,
                body: None,
                sig: None,
                const_fn: false,
                condition: Some(range),
            });
            self.index.insert(key, ItemRef::Fn(i));
        }
        Ok(())
    }

    fn impl_(&mut self, imp: &PendingImpl<'f>) -> Result<(), Refusal> {
        let p = &imp.item;
        let at = p.at;
        let v = p.sig.view();
        let mut env = imp.env.copy_in();
        let self_t = v.head_opt(head::SELF_TY).ok_or(Why::Unsupported("an impl with no self type", Range::new(0, 0))).map_err(|w| w.at(at))?;
        let self_ty = self.names.ty(&mut self.tys, &env, &v, lf::TypeId(self_t)).map_err(|w| w.at(at))?;
        env.self_ = SelfTy::Ty(self_ty);
        let mut clauses = Vec::new_in(Global);
        let (trait_, trait_args) = match v.head_opt(head::TRAIT) {
            Some(t) => {
                let t = lf::TypeId(t);
                let mut cl = Vec::new_in(Global);
                let b = lf::Bound::Trait { ty: t, binder: List::EMPTY, mods: lf::BoundMods::default() };
                self.names.bound(&mut self.tys, &env, &v, b, self_ty, &mut cl).map_err(|w| w.at(at))?;
                let Some(PClause::Trait { trait_, args }) = cl.first().copied() else {
                    return Err(program(format!("an impl's trait is no trait"), Range::new(0, 0)).at(at));
                };
                if cl.len() != 1 {
                    return Err(Why::Unsupported("an associated type constraint on an impl's trait", Range::new(0, 0)).at(at));
                }
                let rest = tc::ArgsAfterSelf::split(self.tys.view(), args, self_ty)
                    .map_err(|e| program(format!("an impl's trait reference does not start with its self type: {e:?}"), Range::new(0, 0)).at(at))?;
                (Some(trait_), rest)
            }
            None => (None, tc::ArgsAfterSelf::NONE),
        };
        self.names.predicates(&mut self.tys, &env, &v, 0, &mut clauses).map_err(|w| w.at(at))?;
        let path = self.item_path(p.module, "{impl}::{where const}");
        self.conditions(imp.key, path, at, p.text, &v, &env, imp.generics, &mut clauses)?;
        let mut assoc = Vec::new_in(Global);
        for (name, ap) in &imp.assoc {
            let av = ap.sig.view();
            let t = av.head_opt(head::TY).ok_or(Why::Unsupported("an associated type with no type", Range::new(0, 0))).map_err(|w| w.at(ap.at))?;
            let t = self.names.ty(&mut self.tys, &env, &av, lf::TypeId(t)).map_err(|w| w.at(ap.at))?;
            assoc.push((*name, t));
        }
        let alloc = *assoc.allocator();
        env.self_assoc = assoc.to_vec_in(alloc);
        for &f in &imp.fns {
            let row = &mut self.fns[f as usize];
            row.env.self_ = SelfTy::Ty(self_ty);
            row.env.self_assoc = assoc.to_vec_in(alloc);
            let name = &*self.names.modules[p.module as usize].path;
            let shown = format!("{}<{}>::{}", if name.is_empty() { "" } else { "::" }, imp.generics, "");
            let _ = shown;
            let self_text = v.text_at(v.type_range(lf::TypeId(self_t)).map_err(|b| Why::from(b).at(at))?).map_err(|t| Why::from(t).at(at))?;
            let full = format!("{self_text}::{}", self.fns[f as usize].path);
            self.fns[f as usize].path = self.item_path(p.module, &full);
        }
        self.index.insert(imp.key, ItemRef::Impl(self.impls.len() as u32));
        self.impls.push(ImplRow {
            key: imp.key,
            generics: imp.generics,
            self_ty,
            trait_,
            trait_args,
            assoc,
            clauses,
            fns: imp.fns.to_vec_in(*imp.fns.allocator()),
        });
        Ok(())
    }

    /// Fn `i`'s parameters, output and predicates.
    fn fn_sig(&mut self, i: usize) -> Result<(), Refusal> {
        let Some(sig) = self.fns[i].sig.take() else { return Ok(()) };
        let v = sig.view();
        let row = &self.fns[i];
        let (at, text, parent, kind) = (row.at, row.text, row.parent, row.kind);
        let mut env = row.env.copy_in();
        let mut clauses = Vec::new_in(Global);
        match kind {
            FnKind::Trait(ti) => {
                let tr = &self.names.traits[ti as usize];
                let (key, g) = (tr.key, tr.generics);
                let ps = params_of(&mut self.tys, 0, g).map_err(|w| w.at(at))?;
                let args = intern_list(&mut self.tys, &ps).map_err(|w| w.at(at))?;
                clauses.push(PClause::Trait { trait_: key, args });
            }
            FnKind::Impl(ii) => {
                clauses.extend_from_slice(&self.impls[ii as usize].clauses);
                env.self_ = SelfTy::Ty(self.impls[ii as usize].self_ty);
            }
            _ => {}
        }
        self.names.predicates(&mut self.tys, &env, &v, parent, &mut clauses).map_err(|w| w.at(at))?;
        let (owner, count) = (self.fns[i].key, parent + self.fns[i].own);
        let path = format!("{}::{{where const}}", self.fns[i].path).into_boxed_str();
        self.conditions(owner, path, at, text, &v, &env, count, &mut clauses)?;
        let self_ty = match env.self_ {
            SelfTy::Trait(_) => Some(intern(&mut self.tys, PNode::Param(0)).map_err(|w| w.at(at))?),
            SelfTy::Ty(t) => Some(t),
            SelfTy::None => None,
        };
        let mut inputs = Vec::new_in(Global);
        let mut self_param = false;
        let rows = v.rows(v.list(head::PARAMS)).map_err(|b| Why::from(b).at(at))?;
        for (k, &n) in rows.iter().enumerate() {
            let prm = lf::Param::decode(n).map_err(|b| Why::from(b).at(at))?;
            let r = place(&v, v.row_range(v.list(head::PARAMS).start + k as u32)).map_err(|w| w.at(at))?;
            let ty = match (prm.self_kind, self_ty) {
                (lf::SelfKind::No, _) | (lf::SelfKind::Typed, _) => {
                    let t = prm.ty.ok_or(Why::Unsupported("a parameter with no type", r)).map_err(|w| w.at(at))?;
                    self.names.ty(&mut self.tys, &env, &v, t).map_err(|w| w.at(at))?
                }
                (lf::SelfKind::Value, Some(s)) => s,
                (lf::SelfKind::Ref(m), Some(s)) => intern(&mut self.tys, PNode::Ref(m, s)).map_err(|w| w.at(at))?,
                (_, None) => return Err(program(format!("a `self` parameter outside a trait or an impl"), r).at(at)),
            };
            if k == 0 && prm.self_kind != lf::SelfKind::No {
                self_param = true;
            }
            inputs.push(ty);
        }
        let output = match v.head_opt(head::TY) {
            Some(t) => self.names.ty(&mut self.tys, &env, &v, lf::TypeId(t)).map_err(|w| w.at(at))?,
            None => intern(&mut self.tys, PNode::Tuple(List::EMPTY)).map_err(|w| w.at(at))?,
        };
        let has_body = v.head[head::FLAGS] & sig::HAS_BODY != 0;
        let row = &mut self.fns[i];
        row.env = env;
        row.inputs = inputs;
        row.output = output;
        row.clauses = clauses;
        row.self_param = self_param;
        if !has_body && matches!(kind, FnKind::Free | FnKind::Impl(_) | FnKind::Const | FnKind::Static) && row.intrinsic.is_none() {
            return Err(program(format!("fn `{}` has no body", row.path), Range::new(0, 0)).at(at));
        }
        // Kept for the body pass: whether it has one.
        row.sig = None;
        if has_body {
            row.body = None;
            row.own += 0;
            self.index.insert(row.key, ItemRef::Fn(i as u32));
            self.pending_bodies_mark(i);
        } else {
            self.index.insert(row.key, ItemRef::Fn(i as u32));
        }
        Ok(())
    }

    fn pending_bodies_mark(&mut self, i: usize) {
        // A fn with a body has its symbol cleared: it is called by its slot, never by name.
        self.fns[i].symbol = "".into();
    }

    /// Every fn's body: lowered, converted, its string literals' bytes kept.
    fn bodies(&mut self) -> Result<(), Refusal> {
        for i in 0..self.fns.len() {
            let row = &self.fns[i];
            if !row.symbol.is_empty() || matches!(row.kind, FnKind::Ctor | FnKind::Foreign) || row.intrinsic.is_some() {
                continue;
            }
            let (at, text, key) = (row.at, row.text, row.key);
            let whole = Range::new(0, text.len() as u32);
            // OWED: this front end's module answers (its `resolve_path` lives on the check's
            // world); until then an identifier pattern here binds (front2 answers: body.rs
            // `ModuleValues`).
            let lowered = match row.condition {
                Some(r) => blaze_lower::lower_anon_const(text, r, EDITION, &mut NoCfg, &mut blaze_lower::NoModule, self.alloc),
                None => blaze_lower::lower_body(text, EDITION, &mut NoCfg, &mut blaze_lower::NoModule, self.alloc),
            }
            .map_err(|need| Why::Unsupported("`cfg`", need.atoms.first().copied().unwrap_or(whole)).at(at))?;
            let view = lowered.view();
            check_lowered(&view, at)?;
            let mut types = BodyTypes { names: &self.names, env: &self.fns[i].env, failed: None };
            let body = match to_typeck(&view, key, &mut self.tys, &mut types, self.alloc) {
                Ok(b) => b,
                Err(Refused::Form { what, range }) => {
                    return Err(match types.failed.take() {
                        Some(why) => why.at(at),
                        None => Why::Unsupported(what, range).at(at),
                    });
                }
                Err(Refused::NoBody) => continue,
                Err(Refused::Bad(b)) => return Err(Why::from(b).at(at)),
                Err(Refused::Text(t)) => return Err(Why::from(t).at(at)),
                Err(Refused::Check(e)) => return Err(program(format!("the checker's table refused: {e:?}"), whole).at(at)),
            };
            // Every literal's value is read here, while the record is alive, through the view:
            // `to_typeck` keeps the view's expression and pattern ids, and `View::text_at` reads
            // a range inside an expansion as surely as one of the item's own.
            let text_of = |r: Result<BodyRange, lf::BadRow>| -> Result<&str, Refusal> {
                view.text_at(r.map_err(|b| Why::from(b).at(at))?).map_err(|t| Why::from(t).at(at))
            };
            let mut strings = Vec::new_in(Global);
            let mut ints = Vec::new_in(Global);
            let mut lits = LitTexts::default();
            for (e, x) in body.exprs.iter().enumerate() {
                let id = tc::ExprId(e as u32);
                let r = body.expr_ranges[e];
                let refuse = |what| Why::Unsupported(what, Range::new(r.start, r.end)).at(at);
                if let tc::Expr::Lit(_) = x {
                    let start = lits.text.len() as u32;
                    lits.text.push_str(text_of(view.expr_range(id))?);
                    lits.exprs.push(id);
                    lits.expr_ranges.push(tc::Range { start, end: lits.text.len() as u32 });
                }
                match x {
                    tc::Expr::Lit(tc::Lit::Str | tc::Lit::ByteStr(_)) => {
                        let bytes = string_bytes(text_of(view.expr_range(id))?).ok_or_else(|| refuse("this escape in a string literal"))?;
                        strings.push((id, bytes.into_boxed_slice()));
                    }
                    tc::Expr::Lit(tc::Lit::Int(_) | tc::Lit::Char) => {
                        let v = int_value(text_of(view.expr_range(id))?).ok_or_else(|| refuse("this integer or char literal"))?;
                        ints.push((id, v));
                    }
                    _ => {}
                }
            }
            let mut int_pats = Vec::new_in(Global);
            for (p, x) in body.pats.iter().enumerate() {
                if let tc::Pat::Lit(_) | tc::Pat::NegLit(_) = x {
                    let start = lits.text.len() as u32;
                    lits.text.push_str(text_of(view.pat_range(tc::PatId(p as u32)))?);
                    lits.pats.push(tc::PatId(p as u32));
                    lits.pat_ranges.push(tc::Range { start, end: lits.text.len() as u32 });
                }
                if let tc::Pat::Lit(tc::Lit::Int(_) | tc::Lit::Char) | tc::Pat::NegLit(tc::Lit::Int(_) | tc::Lit::Char) = x {
                    let r = body.pat_ranges[p];
                    let v = int_value(text_of(view.pat_range(tc::PatId(p as u32)))?)
                        .ok_or_else(|| Why::Unsupported("this literal pattern", Range::new(r.start, r.end)).at(at))?;
                    int_pats.push((tc::PatId(p as u32), v));
                }
            }
            self.fns[i].body = Some(BodyRow { body, strings, ints, int_pats, lits });
        }
        Ok(())
    }

    /// The impl buckets, blanket impls and inherent index; the lang items of the traits.
    fn index_impls(&mut self) {
        for t in &self.traits {
            if let Some(l) = t.lang {
                self.lang.push((l, t.key));
            }
        }
        let view = self.tys.view();
        let mut rows = Vec::new_in(Global);
        for (i, imp) in self.impls.iter().enumerate() {
            rows.push((imp.key, i, imp.trait_, shape_of(view, imp.self_ty)));
        }
        rows.sort_by_key(|r| r.0);
        for &(k, i, tr, shape) in &rows {
            match (tr, shape) {
                (Some(tr), None) => self.blankets.entry(tr).or_insert_with(|| Vec::new_in(Global)).push(k),
                (Some(tr), Some(s)) => {
                    self.buckets.entry((tr, s)).or_insert_with(|| Vec::new_in(Global)).push(k);
                    let any = match s {
                        Shape::Int(_) | Shape::Uint(_) => Some(Shape::AnyInt),
                        Shape::Float(_) => Some(Shape::AnyFloat),
                        _ => None,
                    };
                    if let Some(a) = any {
                        self.buckets.entry((tr, a)).or_insert_with(|| Vec::new_in(Global)).push(k);
                    }
                }
                (None, Some(s)) => {
                    for &f in &self.impls[i].fns {
                        let f = &self.fns[f as usize];
                        let kind = AssocKind::Fn { self_param: f.self_param };
                        self.inherent.entry((s, f.name)).or_insert_with(|| Vec::new_in(Global)).push(AssocItem { container: k, item: f.key, kind });
                    }
                }
                (None, None) => {}
            }
        }
        for (&(tr, _), v) in self.buckets.iter_mut() {
            if let Some(b) = self.blankets.get(&tr) {
                v.extend_from_slice(b);
            }
        }
    }

    /// The fn exported as `bstd_hosted_start` (the hosted world's entry, crates/blaze-std
    /// src/guest.rs), else the fn exported as `main`, else the crate root's `fn main`.
    fn find_entry(&self) -> Result<Option<usize>, Refusal> {
        for symbol in [HOSTED_START, "main"] {
            let mut found = None;
            for (i, f) in self.fns.iter().enumerate() {
                if f.export.as_deref() == Some(symbol) {
                    if found.is_some() {
                        return Err(program(format!("a second fn exported as `{symbol}`"), Range::new(0, 0)).at(f.at));
                    }
                    found = Some(i);
                }
            }
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(match self.names.find(0, Name::of("main"), Ns::Value) {
            Some(Res::Fn(i)) => Some(i as usize),
            _ => None,
        })
    }

    /// Fn `i`'s signature as the checker reads it.
    pub fn sig(&self, i: usize) -> FnSig<'_> {
        let f = &self.fns[i];
        FnSig {
            parent_count: f.parent,
            own_count: f.own,
            // This front makes no anonymous parameters: every own one is declared.
            declared_count: f.own,
            self_param: f.self_param,
            header: f.header,
            c_variadic: false,
            legacy_const_generics: &[],
            inputs: &f.inputs,
            output: f.output,
            types: self.tys.view(),
        }
    }

    /// The index of fn `key` in [`Front::fns`].
    pub fn fn_index(&self, key: ItemKey) -> Option<usize> {
        match self.index.get(&key) {
            Some(&ItemRef::Fn(i)) => Some(i as usize),
            _ => None,
        }
    }

    /// The fn of lang item `l`.
    pub fn fn_lang(&self, l: FnLang) -> Option<ItemKey> {
        self.fn_langs.iter().find(|(x, _)| *x == l).map(|&(_, k)| k)
    }

    /// The `Drop` trait's `drop` fn, if the program has the `drop` lang trait.
    pub fn drop_trait(&self) -> Option<(ItemKey, Option<ItemKey>)> {
        let &(_, tr) = self.lang.iter().find(|(l, _)| *l == LangItem::Drop)?;
        let f = self.members.get(&(tr, Name::of("drop"))).map(|&i| self.fns[i as usize].key);
        Some((tr, f))
    }

    /// The item declared `#[lang]` as `item` (a lang trait, or `ManuallyDrop`).
    pub fn lang_item(&self, item: LangItem) -> Option<ItemKey> {
        self.lang.iter().find(|(l, _)| *l == item).map(|&(_, k)| k)
    }

    /// The fn `name` of the lang trait `trait_` (`Deref::deref`, `DerefMut::deref_mut`).
    pub fn lang_trait_fn(&self, trait_: LangItem, name: &str) -> Option<ItemKey> {
        let &(_, tr) = self.lang.iter().find(|(l, _)| *l == trait_)?;
        self.members.get(&(tr, Name::of(name))).map(|&i| self.fns[i as usize].key)
    }

    /// The world fn `i`'s body is checked in.
    pub fn world(&self, i: usize) -> FrontWorld<'_, 'f> {
        FrontWorld { front: self, f: i, nested: false }
    }

    /// The world a body is checked in while a `where const` condition is evaluated: it reads
    /// no other condition, so an evaluation never nests another (rule 18).
    pub(crate) fn nested_world(&self, i: usize) -> FrontWorld<'_, 'f> {
        FrontWorld { front: self, f: i, nested: true }
    }

    /// The program's ADT `key`.
    pub fn adt(&self, key: ItemKey) -> Option<tc::Adt<'_>> {
        let Some(&ItemRef::Adt(a)) = self.index.get(&key) else { return None };
        let a = &self.adts[a as usize];
        Some(tc::Adt { kind: a.kind, generics: a.generics, variants: &a.variants, fields: &a.fields, types: self.tys.view() })
    }

    pub fn int_lit(&self, owner: ItemKey, e: tc::ExprId) -> Option<u128> {
        let s = &self.fns[self.fn_index(owner)?].body.as_ref()?.ints;
        s.binary_search_by_key(&e.0, |(x, _)| x.0).ok().map(|i| s[i].1)
    }

    pub fn int_pat(&self, owner: ItemKey, p: tc::PatId) -> Option<u128> {
        let s = &self.fns[self.fn_index(owner)?].body.as_ref()?.int_pats;
        s.binary_search_by_key(&p.0, |(x, _)| x.0).ok().map(|i| s[i].1)
    }

    pub fn string(&self, owner: ItemKey, e: tc::ExprId) -> Option<&[u8]> {
        let f = &self.fns[self.fn_index(owner)?];
        let s = &f.body.as_ref()?.strings;
        s.binary_search_by_key(&e.0, |(x, _)| x.0).ok().map(|i| &*s[i].1)
    }

    /// ADT `key`'s `#[repr(..)]`: what layout reads (`LayoutFacts::repr`).
    pub fn repr(&self, key: ItemKey) -> ReprAttrs {
        self.reprs.get(&key).copied().unwrap_or(ReprAttrs::RUST)
    }

    pub fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
        self.fns.get(self.fn_index(item)?)?.intrinsic
    }

    /// The code identity of fn `i`: its key and its text.
    /// The kind of `static` item `key`.
    pub fn static_item(&self, key: ItemKey) -> Option<StaticItem> {
        self.statics.iter().find(|(k, _)| *k == key).map(|&(_, s)| s)
    }

    /// The ADT and variant index whose tuple constructor is fn `key`.
    pub fn ctor_of(&self, key: ItemKey) -> Option<(ItemKey, u32)> {
        self.adts.iter().find_map(|a| a.variants.iter().position(|v| v.key == key).map(|n| (a.key, n as u32)))
    }

    pub fn code_of(&self, i: usize) -> blaze_ids::ContentId {
        let f = &self.fns[i];
        blaze_ids::Hasher::new("blaze-ladder code identity 2").part(&f.key.0.to_le_bytes()).part(f.text.as_bytes()).finish()
    }
}

/// The checker's world for one body: the program's, with the body's fn's names in scope.
pub struct FrontWorld<'a, 'f> {
    front: &'a Front<'f>,
    f: usize,
    /// Checking a body a condition's evaluation reads: `const_holds` answers `Missing`.
    nested: bool,
}

impl FrontWorld<'_, '_> {
    fn trait_row(&self, k: ItemKey) -> Option<&TraitRow> {
        match self.front.index.get(&k) {
            Some(&ItemRef::Trait(t)) => self.front.traits.get(t as usize),
            _ => None,
        }
    }

    fn impl_row(&self, k: ItemKey) -> Option<&ImplRow> {
        match self.front.index.get(&k) {
            Some(&ItemRef::Impl(i)) => self.front.impls.get(i as usize),
            _ => None,
        }
    }

    fn fn_row(&self, k: ItemKey) -> Option<&FnRow<'_>> {
        self.front.fn_index(k).map(|i| &self.front.fns[i])
    }
}

impl World for FrontWorld<'_, '_> {
    fn fn_sig(&self, _: Seal, item: ItemKey) -> Fetch<Option<FnSig<'_>>> {
        Ok(self.front.fn_index(item).map(|i| self.front.sig(i)))
    }

    fn predicates(&self, _: Seal, item: ItemKey) -> Fetch<Predicates<'_>> {
        let clauses: &[PClause] = if let Some(f) = self.fn_row(item) {
            &f.clauses
        } else if let Some(i) = self.impl_row(item) {
            &i.clauses
        } else {
            &[]
        };
        Ok(Predicates { clauses, types: self.front.tys.view() })
    }

    fn adt(&self, _: Seal, item: ItemKey) -> Fetch<Option<tc::Adt<'_>>> {
        Ok(self.front.adt(item))
    }

    fn impls_of(&self, _: Seal, trait_: ItemKey, shape: Shape) -> Fetch<&[ItemKey]> {
        if let Some(v) = self.front.buckets.get(&(trait_, shape)) {
            return Ok(v);
        }
        Ok(self.front.blankets.get(&trait_).map_or(&[] as &[ItemKey], |v| v.as_slice()))
    }

    fn impl_header(&self, _: Seal, item: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        Ok(self.impl_row(item).map(|i| ImplHeader {
                    generics: i.generics,
                    self_ty: i.self_ty,
                    trait_: i.trait_,
                    trait_args: i.trait_args,
                    assoc_types: &i.assoc,
                    types: self.front.tys.view(),
                    polarity: blaze_typeck::ImplPolarity::Positive,
                }))
    }

    fn inherent(&self, _: Seal, shape: Shape, name: Name) -> Fetch<&[AssocItem]> {
        Ok(self.front.inherent.get(&(shape, name)).map_or(&[] as &[AssocItem], |v| v.as_slice()))
    }

    fn assoc_item(&self, _: Seal, container: ItemKey, name: Name) -> Fetch<Option<AssocItem>> {
        if let Some(&f) = self.front.members.get(&(container, name)) {
            let f = &self.front.fns[f as usize];
            return Ok(Some(AssocItem { container, item: f.key, kind: AssocKind::Fn { self_param: f.self_param } }));
        }
        if let Some(t) = self.trait_row(container) {
            if let Some(a) = t.assoc.iter().find(|a| a.name == name) {
                return Ok(Some(AssocItem { container, item: a.item, kind: AssocKind::Type }));
            }
        }
        Ok(None)
    }

    fn traits_in_scope(&self, _: Seal, _: ScopeId) -> Fetch<&[ItemKey]> {
        let m = self.front.fns[self.f].env.module;
        Ok(self.front.module_traits.get(m as usize).map_or(&[] as &[ItemKey], |v| v.as_slice()))
    }

    /// The old front's names walk whole paths and do not say which segment failed: a miss
    /// past `Self` and the parameters is [`tc::PathMiss::Whole`].
    fn resolve_path(&self, _: Seal, _: ScopeId, _: PathIdx, segments: &[Name]) -> Fetch<tc::PathAnswer> {
        let names = &self.front.names;
        let env = &self.front.fns[self.f].env;
        let Some(&first) = segments.first() else { return Ok(Err(tc::PathMiss::Whole)) };
        if first == SELF_TY {
            return Ok(match env.self_ {
                SelfTy::Trait(_) => Ok(PathRes { def: Def::TyParam(0), consumed: 1 }),
                SelfTy::Ty(t) => match self.front.tys.view().get(t) {
                    Some(PNode::Adt(k, _)) => Ok(PathRes { def: Def::Adt(*k), consumed: 1 }),
                    _ => Err(tc::PathMiss::FIRST),
                },
                SelfTy::None => Err(tc::PathMiss::FIRST),
            });
        }
        if let Some(&(_, i)) = env.params.iter().find(|(n, _)| *n == first) {
            return Ok(Ok(PathRes { def: Def::TyParam(i), consumed: 1 }));
        }
        let Some((res, consumed)) = names.walk(env.module, segments, Ns::Value) else { return Ok(Err(tc::PathMiss::Whole)) };
        let all = consumed as usize == segments.len();
        let def = match res {
            Res::Fn(i) => {
                let row = &self.front.fns[i as usize];
                match row.kind {
                    FnKind::Const => Def::Const(row.key),
                    FnKind::Static => match self.front.static_item(row.key) {
                        Some(s) if s.mutable => Def::Static(row.key, Mutbl::Mut),
                        _ => Def::Static(row.key, Mutbl::Not),
                    },
                    _ => Def::Fn(row.key),
                }
            }
            Res::Adt(a) => {
                let h = &names.adts[a as usize];
                match (all, h.kind, h.variants.first().map(|v| v.1)) {
                    (true, AdtKind::Struct, Some(CtorKind::Unit)) => Def::Ctor { adt: h.key, variant: 0, kind: CtorKind::Unit },
                    (true, AdtKind::Struct, Some(CtorKind::Tuple)) => Def::Ctor { adt: h.key, variant: 0, kind: CtorKind::Tuple },
                    _ => Def::Adt(h.key),
                }
            }
            Res::Variant(a, v) => {
                let h = &names.adts[a as usize];
                Def::Ctor { adt: h.key, variant: v, kind: h.variants[v as usize].1 }
            }
            // `Trait::f`: the trait's fn, as rustc's resolver resolves it.
            Res::Trait(t) => {
                let tr = &names.traits[t as usize];
                if let Some(&next) = segments.get(consumed as usize) {
                    if let Some(&(_, f)) = tr.fns.iter().find(|(n, _)| *n == next) {
                        return Ok(Ok(PathRes { def: Def::Fn(f), consumed: consumed + 1 }));
                    }
                }
                Def::Trait(tr.key)
            }
            Res::Mod(_) => return Ok(Err(tc::PathMiss::Whole)),
        };
        Ok(Ok(PathRes { def, consumed }))
    }

    fn lang_item(&self, _: Seal, item: LangItem) -> Fetch<Option<ItemKey>> {
        Ok(self.front.lang.iter().find(|(l, _)| *l == item).map(|&(_, k)| k))
    }

    fn lang_path(&self, _: Seal, item: LangItem) -> Fetch<Option<Def>> {
        Ok(self.front.lang_paths.iter().find(|(l, _)| *l == item).map(|&(_, d)| d))
    }

    fn const_value(&self, _: Seal, _: ItemKey) -> Fetch<Option<u64>> {
        Ok(None)
    }

    fn opaque_hidden(&self, _: Seal, _: ItemKey) -> Fetch<Option<Hidden<'_>>> {
        Ok(None)
    }
    /// The old front end keeps no checked records: a closure of another body is not known.
    fn closure(&self, _: Seal, _: ItemKey, _: u32) -> Fetch<Option<blaze_typeck::ClosureFacts<'_>>> {
        Ok(None)
    }
    fn attrs(&self, _: Seal, _: ItemKey) -> Fetch<TypeAttrs> {
        Ok(TypeAttrs(0))
    }

    fn trait_def(&self, _: Seal, trait_: ItemKey) -> Fetch<Option<TraitDef<'_>>> {
        Ok(self.trait_row(trait_).map(|t| TraitDef {
            generics: t.generics,
            flags: TraitFlags(0),
            lang: t.lang,
            supertraits: &t.supers,
            assoc_types: &t.assoc,
            types: self.front.tys.view(),
            // The old front end does not read members: a `dyn` of its traits is the checker's
            // named gap (`CheckError::DynUndecided`); front2 hands them.
            members: blaze_typeck::TraitMembers::NotKnown,
            defaults: blaze_typeck::TraitDefaults::NotKnown,
        }))
    }

    fn assoc_parent(&self, _: Seal, item: ItemKey) -> Fetch<Option<AssocParent>> {
        Ok(self.fn_row(item).and_then(|f| {
            f.container.map(|c| AssocParent { container: c, name: f.name, kind: AssocKind::Fn { self_param: f.self_param } })
        }))
    }

    /// The condition evaluated at `args` by blaze-consteval (`run::condition_holds`).
    fn const_holds(&self, _: Seal, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> Fetch<tc::ConstHolds> {
        if self.nested {
            return Err(tc::Missing);
        }
        Ok(crate::run::condition_holds(self.front, cond, args, types))
    }

    /// None: `generic_names` refuses every item with a const generic parameter, so an item
    /// this front end answers for has none.
    fn const_params(&self, _: Seal, _: ItemKey) -> Fetch<tc::ConstParams<'_>> {
        Ok(tc::ConstParams { params: &[], types: self.front.tys.view() })
    }
}

#[cfg(test)]
mod tests {
    use super::int_value;

    #[test]
    fn literal_values() {
        assert_eq!(int_value("42"), Some(42));
        assert_eq!(int_value("0x2a"), Some(42));
        assert_eq!(int_value("42u8"), Some(42));
        assert_eq!(int_value("1_000i64"), Some(1000));
        assert_eq!(int_value("b'*'"), Some(42));
        assert_eq!(int_value("b'\\n'"), Some(10));
        assert_eq!(int_value("- 7"), Some(7));
        assert_eq!(int_value("x"), None);
    }

    /// `repr(Rust)` is the default written out and sets nothing; core's `Unaligned` writes it
    /// beside `packed`. A word that is no representation is still refused.
    #[test]
    fn repr_rust_is_the_default_written_out() {
        use super::{ReprAttrs, repr_parts};
        let mut packed = ReprAttrs::default();
        assert_eq!(repr_parts("(Rust, packed)", &mut packed), Ok(()));
        assert_eq!(packed, ReprAttrs { packed: Some(0), ..ReprAttrs::default() });
        let mut plain = ReprAttrs::default();
        assert_eq!(repr_parts("(Rust)", &mut plain), Ok(()));
        assert_eq!(plain, ReprAttrs::default());
        assert_eq!(repr_parts("(rust)", &mut ReprAttrs::default()), Err("this `repr`"));
    }
}
