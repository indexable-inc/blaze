//! Step 2's store: the lowered heads of structs, enums, unions, traits, impls and fns, filled
//! on demand by a bounded work list. An item asked for is lowered from the names library's
//! text of it (`lower_signature`, one item at a time, under its crate's cfg) and its types are
//! built by [`Builder`]; a [`Why::Need`] lowers that key's head first and tries the item again.
//! A cycle of needs is [`HeadError::Cycle`]. Trait and impl members are read from the item's
//! outline, each lowered alone, a member its crate's cfg turns off left out.
//!
//! Where-clauses are built by [`super::preds`] into [`Store::preds`]; a trait's predicates and
//! fn signatures are [`Want::Members`], apart from its head. Not here yet: `where const`
//! clauses (collected, not keyed), attributes (`#[repr]`, ABI, `no_mangle`), consts, statics.

use std::alloc::Global;
use std::format;
use std::vec::Vec;

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_lower::machine::sig;
use blaze_lower::{CfgAnswer, Lowered, View, form as lf, head};
use blaze_own::Alloc;
use blaze_scope::resolve::Names;
use blaze_surface::{ItemDetail, Range};
use blaze_typeck as tc;
use blaze_typeck::{ItemKey, List, Name, PClause, PNode, PTy, PortableTys};

use super::dyn_ty::Member as DynMember;
use super::keys::{KeyClash, Keys, MemberKind, checker_of};
use super::preds::Preds;
use super::types::{Bounds, Builder, Env, Heads, ParamBound, SelfTy, Why, intern, intern_list};
use crate::resolve::Resolver;

/// The most steps one [`Store::ensure`] takes: past it the needs are a cycle.
const MOST_STEPS: usize = 1 << 16;

/// A crate's cfg as blaze-lower asks it: an atom is on when the crate's set holds it.
pub struct CrateCfg<'c>(pub &'c [(&'c str, Option<&'c str>)]);

impl blaze_lower::Cfg for CrateCfg<'_> {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        if self.0.iter().any(|&(n, v)| n == name && v == value) { CfgAnswer::Yes } else { CfgAnswer::No }
    }
}

/// Why an item's head could not be made.
#[derive(Debug)]
pub enum HeadError {
    /// The names library holds no text for the item.
    NoText(LibKey),
    /// Its lowering asked for cfg atoms the crate's set does not answer.
    Cfg(LibKey),
    /// The item is no struct, enum, union, trait, impl or fn.
    NotAHead(LibKey, &'static str),
    /// Each item needs the next's head and the last the first's.
    Cycle(Vec<LibKey, Global>),
    /// A member's key clashed.
    Key(KeyClash),
    /// The item's types: what and where in its text.
    Item(LibKey, Why),
}

impl core::fmt::Display for HeadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HeadError::NoText(k) => write!(f, "the names library holds no text for item {k:?}"),
            HeadError::Cfg(k) => write!(f, "item {k:?} asks for cfg atoms its crate's set does not answer"),
            HeadError::NotAHead(k, what) => write!(f, "item {k:?} is {what}, not a struct, enum, union, trait, impl or fn"),
            HeadError::Cycle(ks) => write!(f, "these items need each other's heads in a cycle: {ks:?}"),
            HeadError::Key(c) => write!(f, "{c}"),
            HeadError::Item(k, why) => write!(f, "item {k:?}: {why:?}"),
        }
    }
}

/// A trait's head: its generics (`Self` first), its type parameters' defaults (after `Self`),
/// its associated types and its fns by name.
pub struct TraitHead {
    pub generics: u32,
    pub defaults: Vec<Option<PTy>, Global>,
    pub assoc: Vec<Name, Global>,
    pub fns: Vec<(Name, ItemKey), Global>,
    /// The checker's flags read from the header: `auto` (which is coinductive too), `unsafe`.
    pub flags: tc::TraitFlags,
    /// Its members' dyn-compatibility facts, handed to the checker as `TraitDef.members`
    /// (the checker decides: `blaze_typeck::dyn_compat`).
    pub members: Vec<tc::dyn_compat::MemberFacts, Global>,
}

/// The heads [`Builder`] reads.
#[derive(Default)]
pub struct Maps {
    /// Each struct's, enum's and union's type parameter count and their defaults (over its
    /// own parameters, `None` where none is written).
    pub generics: HashMap<LibKey, (u32, Vec<Option<PTy>, Global>), Global>,
    pub traits: HashMap<LibKey, TraitHead, Global>,
    /// Each trait's supertraits written as bounds on `Self` (see [`Heads::trait_supers`]):
    /// made with its members ([`Want::Members`]), since a supertrait's head may need the
    /// trait's (`FromResidual<R = <Self as Try>::Residual>`).
    pub supers: HashMap<LibKey, Vec<(LibKey, List), Global>, Global>,
    /// Each type alias's generic count and aliased type ([`Heads::alias`]); `None` where its
    /// head was refused, with the why in `alias_gaps`: an alias no path uses never fails a
    /// check, and one that is used fails only the item that names it.
    pub aliases: HashMap<LibKey, Option<(u32, PTy)>, Global>,
    pub alias_gaps: HashMap<LibKey, Why, Global>,
    /// Each `usize` constant item a head's array length names, by its value
    /// ([`Why::NeedConst`]): filled by the driver that has the constant evaluator.
    pub const_lens: HashMap<LibKey, u64, Global>,
    /// Each item's const generic parameters (index, declared type), sorted by index: a
    /// struct's, enum's, union's, trait's and alias's own (made with its head, what
    /// [`Heads::const_params`] reads), an impl's own, and a fn's with its container's first
    /// (the checker's `const_params`). An item with none has an empty row once made.
    pub const_params: HashMap<LibKey, Vec<(u32, PTy), Global>, Global>,
}

impl Heads for Maps {
    fn const_len(&self, key: LibKey) -> Option<u64> {
        self.const_lens.get(&key).copied()
    }
    fn adt_generics(&self, adt: LibKey) -> Option<(u32, &[Option<PTy>])> {
        self.generics.get(&adt).map(|(g, d)| (*g, &d[..]))
    }
    fn trait_head(&self, trait_: LibKey) -> Option<(u32, &[Option<PTy>])> {
        self.traits.get(&trait_).map(|t| (t.generics, &t.defaults[..]))
    }
    fn alias(&self, alias: LibKey) -> Option<Option<(u32, PTy)>> {
        self.aliases.get(&alias).copied()
    }
    fn trait_supers(&self, trait_: LibKey) -> Option<&[(LibKey, List)]> {
        self.supers.get(&trait_).map(|s| &s[..])
    }
    fn trait_has_assoc(&self, trait_: LibKey, name: Name) -> Option<bool> {
        self.traits.get(&trait_).map(|t| t.assoc.contains(&name))
    }
    fn trait_auto(&self, trait_: LibKey) -> Option<bool> {
        self.traits.get(&trait_).map(|t| t.flags.has(tc::TraitFlags::AUTO))
    }
    fn checker(&self, key: LibKey) -> ItemKey {
        checker_of(key)
    }
    fn const_params(&self, item: LibKey) -> Option<&[(u32, PTy)]> {
        self.const_params.get(&item).map(|c| &c[..])
    }
}

/// A struct, enum or union: the checker's `Adt` parts.
pub struct AdtRow {
    pub kind: tc::AdtKind,
    pub generics: u32,
    pub variants: Vec<tc::Variant, Global>,
    pub fields: Vec<tc::Field, Global>,
    /// Its `#[repr(..)]`: what layout reads.
    pub repr: blaze_build::layout::ReprAttrs,
    /// A `rustc_layout_scalar_valid_range_*` attribute narrows its valid range.
    pub narrowed: bool,
    /// An enum's discriminant by variant, explicit or implicit, in its discriminant type
    /// ([`discr_ty`]); empty for a struct or union. Layout's `discriminant` answers from it.
    pub discrs: Vec<i128, Global>,
}

/// An enum's discriminant type (the Reference, "Enumerations", "Discriminants": "Under the
/// default representation, the discriminant is interpreted as an `isize` value"; a primitive
/// representation `#[repr(u8)]` makes it that integer type), as the checker's `DiscrRepr`.
pub fn discr_ty(repr: blaze_build::layout::ReprAttrs) -> tc::DiscrRepr {
    match repr.int {
        Some(blaze_build::layout::IntRepr::Signed(i)) => tc::DiscrRepr::Int(i),
        Some(blaze_build::layout::IntRepr::Unsigned(u)) => tc::DiscrRepr::Uint(u),
        None => tc::DiscrRepr::Int(tc::IntTy::Isize),
    }
}

/// Each variant's discriminant (the Reference, "Enumerations", "Discriminants"): a written
/// one is a constant expression of the discriminant type ([`Builder::const_term`]: a literal or
/// a constant item; another expression is [`Why::ConstPath`] naming it); an implicit one is
/// the previous variant's plus one (the first's is 0). Two variants with one value are the
/// language's error (E0081), as is one past the type's range.
fn discriminants(b: &Builder<'_, '_, Maps>, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, repr: blaze_build::layout::ReprAttrs, written: &[(Option<lf::ConstIdx>, Range)]) -> Result<Vec<i128, Global>, Why> {
    let mut out: Vec<i128, Global> = Vec::with_capacity_in(written.len(), Global);
    if written.is_empty() {
        return Ok(out);
    }
    let (node, signed) = match discr_ty(repr) {
        tc::DiscrRepr::Int(i) => (PNode::Int(i), true),
        tc::DiscrRepr::Uint(u) => (PNode::Uint(u), false),
        tc::DiscrRepr::Unread => return Err(Why::Program("an enum with no discriminant type".into(), written[0].1)),
    };
    let ty = intern(tys, node)?;
    let Some(&(shown, _)) = super::types::PRIMS.iter().find(|(_, n)| *n == node) else {
        return Err(Why::Program("a discriminant type with no name".into(), written[0].1));
    };
    let bits: u32 = match shown {
        "i8" | "u8" => 8,
        "i16" | "u16" => 16,
        "i32" | "u32" => 32,
        "i128" | "u128" => 128,
        _ => 64,
    };
    let (least, most): (i128, i128) = match (signed, bits) {
        (true, 128) => (i128::MIN, i128::MAX),
        (true, w) => (-(1i128 << (w - 1)), (1i128 << (w - 1)) - 1),
        // A `u128` past `i128::MAX` is refused below by name (`i128` is layout's question).
        (false, 128) => (0, i128::MAX),
        (false, w) => (0, (1i128 << w) - 1),
    };
    for (k, &(c, r)) in written.iter().enumerate() {
        let value = match c {
            Some(c) => {
                let at = v.const_range(c)?;
                let cr = v.site(at)?;
                let made = b.const_term(tys, env, v.text_at(at)?, cr, &|_| Ok(ty))?;
                let Some(&PNode::ConstValue { bits: raw, .. }) = tys.view().nodes.get(made.0 as usize) else {
                    return Err(Why::ConstPath(Box::from(v.text_at(at)?.trim()), cr));
                };
                // The bits are the value's in the type's width: sign-extend a signed one.
                if signed && bits < 128 && raw >> (bits - 1) & 1 == 1 {
                    (raw | !((1u128 << bits) - 1)) as i128
                } else {
                    i128::try_from(raw).map_err(|_| Why::Unsupported("a `u128` discriminant past `i128::MAX`", cr))?
                }
            }
            None => match out.last() {
                None => 0,
                Some(&prev) if prev < most => prev + 1,
                Some(_) => return Err(Why::Program(format!("enum discriminant overflowed: the variant after the value {most} has no value of `{shown}` (E0370)").into_boxed_str(), r)),
            },
        };
        if value < least || value > most {
            return Err(Why::Program(format!("the discriminant {value} is not a `{shown}`").into_boxed_str(), r));
        }
        if let Some(j) = out.iter().position(|&o| o == value) {
            return Err(Why::Program(format!("discriminant value `{value}` assigned more than once (E0081): variants {j} and {k}").into_boxed_str(), r));
        }
        out.push(value);
    }
    Ok(out)
}

/// A fn's signature: generics `0..parent` are its trait's or impl's, `parent..parent + own` its
/// own; `text` is the item's text its body is lowered from.
pub struct FnHead<'n> {
    pub lib: LibKey,
    pub text: &'n str,
    pub container: Option<ItemKey>,
    pub parent: u32,
    pub own: u32,
    /// The first `declared` of the `own` parameters are written in `<..>`; the rest are the
    /// anonymous parameters of its `impl Trait` arguments (`own - anon.len()`).
    pub declared: u32,
    pub self_param: bool,
    /// Its ABI and safety: the header of the fn pointer it reifies to.
    pub header: tc::FnPtrHeader,
    pub inputs: Vec<PTy, Global>,
    pub output: PTy,
    pub has_body: bool,
    /// The names its body's types are read with, as its signature was: the module, every
    /// generic parameter in scope (the container's first, then its own), `Self` and the
    /// container's associated types. One home: a body is lowered in the env its head was.
    pub module: LibKey,
    pub params: Vec<(Name, u32), Global>,
    /// Which of those are const parameters, with their types ([`Env::consts`]).
    pub consts: Vec<(u32, PTy), Global>,
    /// The trait bounds on those parameters (the container's, then its own).
    pub bounds: Vec<ParamBound, Global>,
    pub self_: SelfTy,
    pub self_assoc: Vec<(Name, PTy), Global>,
}

/// A tuple struct's or tuple variant's constructor fn (the Reference, "Struct expressions",
/// "Enumerated types"): its name in the value namespace is a fn from its fields, in order, to
/// the type, generic over the struct's or enum's parameters (`Ok::<T, E>: fn(T) -> Result<T,
/// E>`); its where-clauses are the type's. A unit struct or variant is a constant of the type
/// and a braced one has no value-namespace name: neither has a row.
pub struct CtorSig {
    pub adt: ItemKey,
    pub generics: u32,
    pub inputs: Vec<PTy, Global>,
    pub output: PTy,
}

/// An impl's header: its self type, its trait and arguments, its associated types and fns.
pub struct ImplHead {
    pub generics: u32,
    pub self_ty: PTy,
    pub trait_: Option<ItemKey>,
    pub trait_args: tc::ArgsAfterSelf,
    pub assoc: Vec<(Name, PTy), Global>,
    pub fns: Vec<(Name, ItemKey), Global>,
    /// Its associated constants (the Reference, "Associated constants"), each with a row in
    /// `Store::fns` as the checker reads one: no inputs, its type the output, no body.
    pub consts: Vec<(Name, ItemKey), Global>,
    /// `impl !Trait for T` is negative (the Reference does not list negative impls: rustc's
    /// `negative_impls`, which core and alloc use); `#[rustc_reservation_impl]` reserves.
    pub polarity: tc::ImplPolarity,
}

/// What [`Store::ensure`] is asked to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// An ADT's generic count or a trait's head: what [`Builder`] reads.
    Head(LibKey),
    Adt(LibKey),
    Impl(LibKey),
    Fn(LibKey),
    /// A trait's predicates (supertraits, where clauses) and its fns' signatures: apart from
    /// its head, which they may need (`where Self::Item: Clone`).
    Members(LibKey),
}

impl Want {
    fn key(self) -> LibKey {
        match self {
            Want::Head(k) | Want::Adt(k) | Want::Impl(k) | Want::Fn(k) | Want::Members(k) => k,
        }
    }
}

/// The lowered heads, by key, and the type table they are in.
pub struct Store<'n> {
    pub tys: PortableTys<'static>,
    pub maps: Maps,
    pub adts: HashMap<ItemKey, AdtRow, Global>,
    pub impls: HashMap<ItemKey, ImplHead, Global>,
    pub fns: HashMap<ItemKey, FnHead<'n>, Global>,
    /// Each ADT's, trait's, impl's and fn's where-clauses, own and inherited (a fn's from its
    /// trait or impl), with its `where const` conditions.
    pub preds: HashMap<ItemKey, Preds, Global>,
    /// The `sized` lang trait: each own type parameter's implied bound.
    pub sized: Option<LibKey>,
    /// Each trait's parts of the checker's `TraitDef` that need its members lowered.
    pub trait_defs: HashMap<ItemKey, TraitExtra, Global>,
    /// The member fns whose signature could not be made, by why: their container's head is
    /// made without them, and a body that uses one gets [`super::world_items::Gap::FnHead`]
    /// (a trait's head does not depend on its fns' signatures: `FnMut` and `extern "rust-call"`).
    pub fn_gaps: HashMap<ItemKey, Why, Global>,
    /// Each tuple struct's and tuple variant's constructor fn, by the variant's key: made with
    /// its ADT's head.
    pub ctors: HashMap<ItemKey, CtorSig, Global>,
    /// Each static whose head is made (its type is its [`FnHead`] of no inputs, as a const's),
    /// with its mutability (`static mut`): the checker's `Def::Static`.
    pub statics: HashMap<ItemKey, tc::Mutbl, Global>,
}

/// A trait's clauses on `Self` and its associated types with their keys and defaults (in the
/// store's table); made with its members ([`Want::Members`]).
pub struct TraitExtra {
    pub supertraits: Vec<PClause, Global>,
    pub assoc: Vec<tc::AssocTyDef, Global>,
}

/// The step's result: done, or the head it needs first.
enum Step {
    Done,
    Need(LibKey),
}

fn cycle(stack: &[Want]) -> Vec<LibKey, Global> {
    let mut out = Vec::with_capacity_in(stack.len(), Global);
    out.extend(stack.iter().map(|w| w.key()));
    out
}

fn bad(key: LibKey) -> impl Fn(Why) -> HeadError {
    move |why| HeadError::Item(key, why)
}


/// The fn's anonymous type parameters (the Reference, "Impl trait", "Anonymous type
/// parameters"): each `impl Trait` in a parameter's type, numbered from `base` (after the
/// declared generics) in order of appearance in the text, an outer one before those in its
/// bounds' type arguments (`impl Into<impl Show>`: the `Into` one first). One inside an
/// associated item constraint (`impl Iterator<Item = impl Show>`) is not walked to, so it stays
/// refused where it is made. A work list, no recursion.
pub(super) fn anon_params(v: &View<'_>, base: u32, out: &mut Vec<(lf::TypeId, u32), Global>) -> Result<(), Why> {
    let mut todo: Vec<lf::TypeId, Global> = Vec::new_in(Global);
    let mut kids: Vec<lf::TypeId, Global> = Vec::new_in(Global);
    for &n in v.rows(v.list(head::PARAMS))?.iter().rev() {
        todo.extend(lf::Param::decode(n)?.ty);
    }
    while let Some(t) = todo.pop() {
        kids.clear();
        if let lf::TypeRef::Impl(bounds) = v.ty(t)? {
            out.push((t, base + out.len() as u32));
            for &b in v.rows(bounds)? {
                if let lf::Bound::Trait { ty, .. } = lf::Bound::decode(b)? {
                    kids.push(ty);
                }
            }
        } else {
            super::types::children(v, t, &mut kids)?;
        }
        todo.extend(kids.iter().rev());
    }
    Ok(())
}

/// What `param_names` does with a const generic parameter.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Consts {
    /// Leave its name out (its index is still taken): the caller asks only which names are
    /// type parameters (filing an impl by its header, where a const parameter is never a type).
    Skip,
    /// Name it with its index, as a type parameter is ([`own_generics`] says which are const).
    Keep,
}

/// The generic type parameters of `v`'s item from index `base`, by name; their count.
pub(super) fn param_names(v: &View<'_>, base: u32, out: &mut Vec<(Name, u32), Global>, consts: Consts) -> Result<u32, Why> {
    let mut i = base;
    let list = v.list(head::GENERICS);
    for &n in v.rows(list)? {
        match lf::GenericParam::decode(n)? {
            lf::GenericParam::Lifetime { .. } => {}
            lf::GenericParam::Type { name, .. } => {
                out.push((Name(v.name(name)?), i));
                i += 1;
            }
            lf::GenericParam::Const { name, .. } => {
                if consts == Consts::Keep {
                    out.push((Name(v.name(name)?), i));
                }
                i += 1;
            }
        }
    }
    Ok(i - base)
}

/// A fn's written ABI: none is Rust's, a bare `extern` is C's; any other than "C", "C-unwind",
/// "system", "system-unwind" and "Rust" is refused.
fn abi_of(v: &View<'_>, whole: Range) -> Result<tc::Abi, Why> {
    if v.head[head::ABI] == u32::MAX {
        return Ok(tc::Abi::Rust);
    }
    let Some(r) = v.abi() else { return Ok(tc::Abi::C) };
    super::types::fn_ptr_abi(v.text_at(r)?, whole)
}

/// `Param(from)..Param(to)` as one list, a const parameter's as `ConstParam`.
fn params(tys: &mut PortableTys<'_>, from: u32, to: u32, consts: &[(u32, PTy)]) -> Result<List, Why> {
    let mut l = Vec::with_capacity_in((to - from) as usize, Global);
    for i in from..to {
        let n = if consts.iter().any(|(c, _)| *c == i) { PNode::ConstParam(i) } else { PNode::Param(i) };
        l.push(intern(tys, n)?);
    }
    intern_list(tys, &l)
}

/// The generic parameters of `v`'s item from index `base` (the Reference, "Generic
/// parameters"): each type and const parameter by name into `ps`, and each const parameter's
/// index and declared type into `consts` (its type is read in the item's module with no
/// parameters in scope: the Reference, "Const generics": "The only allowed types of const
/// parameters are `u8` .. `usize`, `char` and `bool`", none of which names a parameter).
/// Their count.
pub(super) fn own_generics(b: &Builder<'_, '_, Maps>, tys: &mut PortableTys<'_>, module: LibKey, v: &View<'_>, base: u32, ps: &mut Vec<(Name, u32), Global>, consts: &mut Vec<(u32, PTy), Global>) -> Result<u32, Why> {
    let n = param_names(v, base, ps, Consts::Keep)?;
    let env = Env { module, params: &[], bounds: Bounds::Done(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &[] };
    let mut i = base;
    let list = v.list(head::GENERICS);
    for (k, &row) in v.rows(list)?.iter().enumerate() {
        match lf::GenericParam::decode(row)? {
            lf::GenericParam::Lifetime { .. } => {}
            lf::GenericParam::Type { .. } => i += 1,
            lf::GenericParam::Const { ty, .. } => {
                let t = b.ty(tys, &env, v, ty)?;
                match tys.view().nodes.get(t.0 as usize) {
                    Some(PNode::Int(_) | PNode::Uint(_) | PNode::Bool | PNode::Char) => {}
                    _ => {
                        let r = v.site(v.row_range(list.start + k as u32)?)?;
                        return Err(Why::Unsupported("a const parameter whose type is not an integer, `char` or `bool` (`adt_const_params`)", r));
                    }
                }
                consts.push((i, t));
                i += 1;
            }
        }
    }
    Ok(n)
}

/// Each own generic parameter's default from index `base`, in order (the Reference, "Generic
/// parameters": a type parameter's is a type, a const parameter's a constant of its type), read
/// in `env` (the parameters before it in scope); `None` where none is written.
fn own_defaults(b: &Builder<'_, '_, Maps>, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, base: u32, out: &mut Vec<Option<PTy>, Global>) -> Result<(), Why> {
    let mut i = base;
    for &row in v.rows(v.list(head::GENERICS))? {
        match lf::GenericParam::decode(row)? {
            lf::GenericParam::Lifetime { .. } => continue,
            lf::GenericParam::Type { default, .. } => out.push(match default {
                Some(t) => Some(b.ty(tys, env, v, t)?),
                None => None,
            }),
            lf::GenericParam::Const { default, .. } => out.push(match default {
                Some(c) => {
                    let at = v.const_range(c)?;
                    let r = v.site(at)?;
                    let want = env.consts.iter().find(|(c, _)| *c == i).map(|&(_, t)| t);
                    let want = want.ok_or(Why::Program("a const parameter with no type".into(), r))?;
                    Some(b.const_term(tys, env, v.text_at(at)?, r, &|_| Ok(want))?)
                }
                None => None,
            }),
        }
        i += 1;
    }
    Ok(())
}

/// One item's signature, lowered from `text` under its crate's cfg.
pub(super) fn lower<'n>(names: &'n Names<'n>, key: LibKey, text: &'n str) -> Result<Lowered<'static, 'n>, HeadError> {
    let krate = names.crate_of(key).ok_or(HeadError::NoText(key))?;
    let mut cfg = CrateCfg(names.krate(krate).cfg.atoms());
    blaze_lower::lower_signature(text, names.edition(krate), &mut cfg, Alloc::global()).map_err(|_| HeadError::Cfg(key))
}

/// A member of a trait or an impl: its name, kind and text, as the names library lists it.
struct Member<'n> {
    name: &'n str,
    detail: ItemDetail,
    text: &'n str,
}

/// The members of trait or impl `key` from `Names::members`: those written in its body and
/// those a macro call there expands to (core's `impl u32 { uint_impl! { .. } }`), one list.
fn members<'n>(names: &'n Names<'n>, key: LibKey) -> Result<Vec<Member<'n>, Global>, HeadError> {
    let krate = names.crate_of(key).ok_or(HeadError::NoText(key))?;
    names.krate(krate).tables.texts.get(&key).ok_or(HeadError::NoText(key))?;
    let mut out = Vec::new_in(Global);
    // A container with no member has no row in the library's members table.
    for m in names.members(key).unwrap_or(&[]) {
        // The call itself: its expansion's members are listed beside it (`written_by`).
        if matches!(m.detail, ItemDetail::MacroCall { .. }) {
            continue;
        }
        out.push(Member { name: m.name.unwrap_or(""), detail: m.detail, text: m.text });
    }
    Ok(out)
}

impl<'n> Store<'n> {
    /// An empty store; `sized` is the program's `#[lang = "sized"]` trait (none in a
    /// `no_core` rung that declares none).
    pub fn new(sized: Option<LibKey>) -> Store<'n> {
        Store {
            tys: PortableTys::new_in(Alloc::global()),
            maps: Maps::default(),
            adts: HashMap::new_in(Global),
            impls: HashMap::new_in(Global),
            fns: HashMap::new_in(Global),
            preds: HashMap::new_in(Global),
            sized,
            trait_defs: HashMap::new_in(Global),
            fn_gaps: HashMap::new_in(Global),
            ctors: HashMap::new_in(Global),
            statics: HashMap::new_in(Global),
        }
    }

    /// What a [`Why::Need`] of `k` asks for: its head, or, for a trait whose head is made,
    /// its members (where its supertraits are filed).
    pub fn need(&self, k: LibKey) -> Want {
        if self.maps.traits.contains_key(&k) { Want::Members(k) } else { Want::Head(k) }
    }

    /// Makes `want` and every head it needs, by a work list.
    pub fn ensure(&mut self, names: &'n Names<'n>, keys: &mut Keys, want: Want) -> Result<(), HeadError> {
        let mut stack = Vec::new_in(Global);
        stack.push(want);
        for _ in 0..MOST_STEPS {
            let Some(&top) = stack.last() else { return Ok(()) };
            match self.step(names, keys, top)? {
                Step::Done => {
                    stack.pop();
                }
                Step::Need(k) => {
                    let need = self.need(k);
                    if stack.iter().any(|w| *w == need) {
                        return Err(HeadError::Cycle(cycle(&stack)));
                    }
                    stack.push(need);
                }
            }
        }
        Err(HeadError::Cycle(cycle(&stack)))
    }

    fn step(&mut self, names: &'n Names<'n>, keys: &mut Keys, want: Want) -> Result<Step, HeadError> {
        let key = want.key();
        let ck = checker_of(key);
        let done = match want {
            Want::Head(k) => self.maps.generics.contains_key(&k) || self.maps.traits.contains_key(&k) || self.maps.aliases.contains_key(&k),
            Want::Adt(_) => self.adts.contains_key(&ck),
            Want::Impl(_) => self.impls.contains_key(&ck),
            Want::Fn(_) => self.fns.contains_key(&ck),
            Want::Members(_) => self.preds.contains_key(&ck),
        };
        if done {
            return Ok(Step::Done);
        }
        let resolver = Resolver { names };
        let detail = resolver.detail(key).ok_or(HeadError::NoText(key))?;
        let text = names.item_text(key).ok_or(HeadError::NoText(key))?.text;
        let module = names.module_of(key).ok_or(HeadError::NoText(key))?;
        let low = lower(names, key, text)?;
        let v = low.view();
        let got = match (want, detail) {
            (Want::Head(_), ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union) => {
                self.adt_head(names, key, module, &v)
            }
            (Want::Head(_), ItemDetail::Trait) => self.trait_head(names, keys, key, module, &v, text),
            (Want::Head(_), ItemDetail::TypeAlias) => match self.alias(names, key, module, &v) {
                Err(why @ (Why::Need(_) | Why::NeedConst(..))) => Err(why),
                Err(why) => {
                    self.maps.aliases.insert(key, None);
                    self.maps.alias_gaps.insert(key, why);
                    Ok(())
                }
                Ok(()) => Ok(()),
            },
            (Want::Adt(_), ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union) => {
                if !self.maps.generics.contains_key(&key) {
                    return Ok(Step::Need(key));
                }
                self.adt(names, keys, key, module, &v, detail)
            }
            (Want::Members(_), ItemDetail::Trait) => {
                if !self.maps.traits.contains_key(&key) {
                    return Ok(Step::Need(key));
                }
                self.trait_members(names, keys, key, module, &v)
            }
            (Want::Impl(_), ItemDetail::Impl) => self.impl_(names, keys, key, module, &v),
            // A free constant's head is its type as a fn of no inputs (the checker reads a
            // `Def::Const` through `fn_sig`); its body is lowered by `lower_fn` like a fn's.
            (Want::Fn(_), ItemDetail::Fn { .. } | ItemDetail::Const { .. }) => {
                let env = Env { module, params: &[], bounds: Bounds::Done(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &[] };
                self.fn_head(names, key, ck, None, &env, 0, &[], &v, text)
            }
            // A static's head is its type, as a const's, and its mutability (the Reference,
            // "Static items": "Mutable statics", `static mut`).
            (Want::Fn(_), ItemDetail::Static { .. }) => {
                let env = Env { module, params: &[], bounds: Bounds::Done(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &[] };
                let made = self.fn_head(names, key, ck, None, &env, 0, &[], &v, text);
                if made.is_ok() {
                    let m = if v.head[head::FLAGS] & sig::MUT != 0 { tc::Mutbl::Mut } else { tc::Mutbl::Not };
                    self.statics.insert(ck, m);
                }
                made
            }
            (_, d) => return Err(HeadError::NotAHead(key, crate::front::detail_name(d))),
        };
        match got {
            Ok(()) => Ok(Step::Done),
            Err(Why::Need(k)) => Ok(Step::Need(k)),
            Err(why) => Err(HeadError::Item(key, why)),
        }
    }

    /// A free type alias's head: its generics and the aliased type, read in the alias's own
    /// module with its own parameters (a bound on an alias's parameter binds nothing).
    fn alias(&mut self, names: &'n Names<'n>, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let b = Builder { resolver: Resolver { names }, heads: &self.maps };
        let generics = own_generics(&b, &mut self.tys, module, v, 0, &mut ps, &mut cs)?;
        let t = v.head_opt(head::TY).ok_or(Why::Unsupported("a type alias with no type (`type T;`)", Range::new(0, 0)))?;
        // A bound on an alias's parameter is not checked where the alias is used, but it is
        // what `T::X` in the aliased type is read through (core's `ChangeOutputType`).
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &cs };
        let mut bounds = Vec::new_in(Global);
        b.param_bounds(&mut self.tys, &env, v, 0, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        let ty = b.ty(&mut self.tys, &env, v, lf::TypeId(t))?;
        self.maps.aliases.insert(key, Some((generics, ty)));
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    /// A struct's, enum's or union's head: its type parameter count and each one's default,
    /// lowered over the parameters before it (`struct Vec<T, A: Allocator = Global>`).
    fn adt_head(&mut self, names: &'n Names<'n>, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let b = Builder { resolver: Resolver { names }, heads: &self.maps };
        let g = own_generics(&b, &mut self.tys, module, v, 0, &mut ps, &mut cs)?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &cs };
        let mut defaults = Vec::new_in(Global);
        own_defaults(&b, &mut self.tys, &env, v, 0, &mut defaults)?;
        self.maps.generics.insert(key, (g, defaults));
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    fn trait_head(&mut self, names: &'n Names<'n>, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>, _text: &'n str) -> Result<(), Why> {
        let whole = Range::new(0, 0);
        let written = v.head[head::FLAGS];
        if written & sig::TRAIT_ALIAS != 0 {
            return Err(Why::Unsupported("a trait alias", whole));
        }
        let mut flags = 0;
        if written & sig::AUTO != 0 {
            flags |= tc::TraitFlags::AUTO | tc::TraitFlags::COINDUCTIVE;
        }
        if written & sig::UNSAFE != 0 {
            flags |= tc::TraitFlags::UNSAFE;
        }
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let b = Builder { resolver: Resolver { names }, heads: &self.maps };
        let own = own_generics(&b, &mut self.tys, module, v, 1, &mut ps, &mut cs)?;
        // The trait's bounds are lowered after its head (they may name the trait): a default
        // written with `T::X` is refused as not yet readable, never as not found.
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::Trait(key), self_assoc: &[], anon: &[], consts: &cs };
        let mut defaults = Vec::new_in(Global);
        own_defaults(&b, &mut self.tys, &env, v, 1, &mut defaults)?;
        let ck = checker_of(key);
        let mut assoc = Vec::new_in(Global);
        let mut fns = Vec::new_in(Global);
        let ms = members(names, key).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
        let mut members = Vec::new_in(Global);
        for m in &ms {
            let low = lower(names, key, m.text).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
            if low.view().cfg_out() {
                continue;
            }
            let kind = match m.detail {
                ItemDetail::Fn { .. } => {
                    let mk = keys.member(ck, MemberKind::Fn, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    fns.push((Name::of(m.name), mk));
                    DynMember::Fn
                }
                ItemDetail::TypeAlias => {
                    assoc.push(Name::of(m.name));
                    DynMember::Type
                }
                ItemDetail::Const { .. } => DynMember::Const,
                d => return Err(Why::Unsupported(crate::front::detail_name(d), whole)),
            };
            let shape = super::dyn_ty::member(&b.resolver, module, self.sized, kind, &low.view())?;
            members.push(tc::dyn_compat::MemberFacts { name: Name::of(m.name), shape });
        }
        self.maps.traits.insert(key, TraitHead { generics: own + 1, defaults, assoc, fns, flags: tc::TraitFlags(flags), members });
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    /// A trait's predicates (its supertraits and where clauses on its generics, `Self` =
    /// `Param(0)`) and its fns' signatures, each fn's clauses its trait's plus
    /// `Self: Trait<params>`.
    fn trait_members(&mut self, names: &'n Names<'n>, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let whole = Range::new(0, 0);
        let ck = checker_of(key);
        let mut ps = Vec::new_in(Global);
        let own = param_names(v, 1, &mut ps, Consts::Keep)?;
        let cs = self.maps.const_params.get(&key).cloned().ok_or(Why::Need(key))?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::Trait(key), self_assoc: &[], anon: &[], consts: &cs };
        let b = Builder { resolver: Resolver { names }, heads: &self.maps };
        let mut bounds = Vec::new_in(Global);
        b.param_bounds(&mut self.tys, &env, v, 1, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        let me = intern(&mut self.tys, PNode::Param(0))?;
        let mut preds = Preds::new();
        b.predicates(&mut self.tys, &env, v, 1, Some(me), self.sized, &mut preds)?;
        let mut inherited = Vec::with_capacity_in(preds.clauses.len() + 1, Global);
        inherited.push(PClause::Trait { trait_: ck, args: params(&mut self.tys, 0, own + 1, &cs)? });
        inherited.extend_from_slice(&preds.clauses);
        // What elaborating `T: Trait` adds: the clauses on `Self` (supertraits, `where Self: ..`).
        let mut supertraits = Vec::new_in(Global);
        for c in preds.clauses.iter() {
            let on_self = match *c {
                PClause::Trait { args, .. } | PClause::Projection { args, .. } => self.tys.view().list(args).map_err(|_| Why::Unsupported("a clause list outside the store's table", whole))?.first() == Some(&me),
                PClause::Const { .. } => false,
            };
            if on_self {
                supertraits.push(*c);
            }
        }
        // Before the members' signatures: `Self::Output` in `Fn::call` is a supertrait's.
        let mut supers = Vec::new_in(Global);
        for c in supertraits.iter() {
            if let PClause::Trait { trait_, args } = *c {
                let lib = keys.library(trait_).ok_or(Why::Program(format!("the supertrait {trait_:?} has no library key").into_boxed_str(), whole))?;
                supers.push((lib, args));
            }
        }
        self.maps.supers.insert(key, supers);
        let mut assoc = Vec::new_in(Global);
        for m in &members(names, key).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))? {
            let low = lower(names, key, m.text).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
            let mv = low.view();
            if mv.cfg_out() {
                continue;
            }
            match m.detail {
                ItemDetail::Fn { .. } => {
                    let mk = keys.member(ck, MemberKind::Fn, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    self.member_fn_head(names, key, mk, Some(ck), &env, own + 1, &inherited, &mv, m.text)?;
                }
                ItemDetail::TypeAlias => {
                    let item = keys.member(ck, MemberKind::Type, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    let b = Builder { resolver: Resolver { names }, heads: &self.maps };
                    let default = match mv.head_opt(head::TY) {
                        Some(t) => Some(b.ty(&mut self.tys, &env, &mv, lf::TypeId(t))?),
                        None => None,
                    };
                    assoc.push(tc::AssocTyDef { name: Name::of(m.name), item, default });
                }
                _ => {}
            }
        }
        self.trait_defs.insert(ck, TraitExtra { supertraits, assoc });
        self.preds.insert(ck, preds);
        Ok(())
    }

    #[expect(clippy::too_many_arguments, reason = "the item's parts, each read once by the caller")]
    fn adt(&mut self, names: &'n Names<'n>, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>, detail: ItemDetail) -> Result<(), Why> {
        let ck = checker_of(key);
        let mut ps = Vec::new_in(Global);
        let generics = param_names(v, 0, &mut ps, Consts::Keep)?;
        let cs = self.maps.const_params.get(&key).cloned().ok_or(Why::Need(key))?;
        let pl = params(&mut self.tys, 0, generics, &cs)?;
        let self_ty = intern(&mut self.tys, PNode::Adt(ck, pl))?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::Ty(self_ty, None), self_assoc: &[], anon: &[], consts: &cs };
        let b = Builder { resolver: Resolver { names }, heads: &self.maps };
        let mut bounds = Vec::new_in(Global);
        b.param_bounds(&mut self.tys, &env, v, 0, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        let kind = match detail {
            ItemDetail::Enum => tc::AdtKind::Enum,
            ItemDetail::Union => tc::AdtKind::Union,
            _ => tc::AdtKind::Struct,
        };
        let ctor_of = |s: lf::Shape| match s {
            lf::Shape::Record => tc::CtorKind::Braces,
            lf::Shape::Tuple => tc::CtorKind::Tuple,
            lf::Shape::Unit => tc::CtorKind::Unit,
        };
        // Each variant: its key, name, ctor and field rows; a struct's one variant is itself.
        let mut shapes = Vec::new_in(Global);
        // Each variant's written discriminant (`A = 1`) and the variant's range.
        let mut written = Vec::new_in(Global);
        if kind == tc::AdtKind::Enum {
            let krate = names.crate_of(key).ok_or(Why::Need(key))?;
            let lib_vs = names.krate(krate).tables.enums.get(&key);
            for (i, &n) in v.rows(v.list(head::FIELDS))?.iter().enumerate() {
                let var = lf::Variant::decode(n)?;
                let r = v.site(v.row_range(v.list(head::FIELDS).start + i as u32)?)?;
                written.push((var.discriminant, r));
                let name = v.name_text(var.name)?;
                let lk = lib_vs.and_then(|vs| vs.iter().find(|x| x.name == name)).map(|x| x.key);
                let lk = lk.ok_or_else(|| Why::Program(format!("variant `{name}` is not in the names library").into_boxed_str(), r))?;
                shapes.push((checker_of(lk), Name::of(name), ctor_of(var.shape), var.fields));
            }
        } else {
            let ctor = match v.head[head::SHAPE] {
                0 => tc::CtorKind::Braces,
                1 => tc::CtorKind::Tuple,
                _ => tc::CtorKind::Unit,
            };
            let name = names.item_text(key).and_then(|t| t.name).unwrap_or("");
            let vk = keys.member(ck, MemberKind::Ctor, "").map_err(|c| Why::Program(format!("{c}").into_boxed_str(), Range::new(0, 0)))?;
            shapes.push((vk, Name::of(name), ctor, v.list(head::FIELDS)));
        }
        let mut variants = Vec::new_in(Global);
        let mut fields = Vec::new_in(Global);
        for &(vk, name, ctor, rows) in &shapes {
            let start = fields.len() as u32;
            for (j, &n) in v.rows(rows)?.iter().enumerate() {
                let f = lf::Field::decode(n)?;
                let ty = b.ty(&mut self.tys, &env, v, f.ty)?;
                let fname = match f.name {
                    Some(n) => Name(v.name(n)?),
                    None => Name::of_index(j as u32),
                };
                fields.push(tc::Field { name: fname, ty });
            }
            variants.push(tc::Variant { key: vk, name, ctor, fields: List { start, len: fields.len() as u32 - start } });
            if ctor == tc::CtorKind::Tuple {
                let mut inputs = Vec::with_capacity_in(fields.len() - start as usize, Global);
                inputs.extend(fields[start as usize..].iter().map(|f| f.ty));
                self.ctors.insert(vk, CtorSig { adt: ck, generics, inputs, output: self_ty });
            }
        }
        let mut preds = Preds::new();
        b.predicates(&mut self.tys, &env, v, 0, None, self.sized, &mut preds)?;
        self.preds.insert(ck, preds);
        // Its `#[repr(..)]`, read by the reader the old front reads with, and whether a
        // `rustc_layout_scalar_valid_range_*` narrows its range (the program refuses to lay
        // out such a type until the back half's `valid_range` reads it).
        let mut repr = blaze_build::layout::ReprAttrs::RUST;
        let mut narrowed = false;
        let attrs = v.list(head::ATTRS);
        for (i, &n) in v.rows(attrs)?.iter().enumerate() {
            let a = v.attr(n)?;
            let path = v.text_at(a.path)?.trim();
            if path == "repr" {
                let r = v.site(v.row_range(attrs.start + i as u32)?)?;
                crate::front::repr_parts(v.text_at(a.args)?, &mut repr).map_err(|what| Why::Unsupported(what, r))?;
            }
            narrowed |= path.starts_with("rustc_layout_scalar_valid_range");
        }
        let discrs = discriminants(&b, &mut self.tys, &env, v, repr, &written)?;
        self.adts.insert(ck, AdtRow { kind, generics, variants, fields, repr, narrowed, discrs });
        Ok(())
    }

    fn impl_(&mut self, names: &'n Names<'n>, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let whole = Range::new(0, 0);
        let mut polarity = if v.head[head::FLAGS] & sig::NEGATIVE != 0 { tc::ImplPolarity::Negative } else { tc::ImplPolarity::Positive };
        for &n in v.rows(v.list(head::ATTRS))? {
            if v.text_at(v.attr(n)?.path)?.trim() == "rustc_reservation_impl" {
                polarity = tc::ImplPolarity::Reservation;
            }
        }
        let ck = checker_of(key);
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let b = Builder { resolver: Resolver { names }, heads: &self.maps };
        let generics = own_generics(&b, &mut self.tys, module, v, 0, &mut ps, &mut cs)?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &cs };
        let mut bounds = Vec::new_in(Global);
        b.param_bounds(&mut self.tys, &env, v, 0, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        let self_ty = b.ty(&mut self.tys, &env, v, lf::TypeId(v.head[head::SELF_TY]))?;
        let env = Env { self_: SelfTy::Ty(self_ty, None), ..env };
        let (trait_, trait_args, written) = match v.head_opt(head::TRAIT) {
            Some(t) => {
                // `trait_ref` gives a clause's arguments, `Self` first; the checker's
                // `ImplHeader::trait_args` are those AFTER `Self` (the self type is `self_ty`).
                // With `Self` left in, no goal unified with the header (`Result<Out, Missing>:
                // Try` failed against `impl<T, E> Try for Result<T, E>`).
                let (tk, args) = b.trait_ref(&mut self.tys, &env, v, lf::TypeId(t), self_ty)?;
                let rest = tc::ArgsAfterSelf::split(self.tys.view(), args, self_ty).map_err(|e| {
                    Why::Program(format!("the impl's trait reference does not start with its self type {self_ty:?}: {e:?}").into_boxed_str(), whole)
                })?;
                (Some(checker_of(tk)), rest, Some((tk, args)))
            }
            None => (None, tc::ArgsAfterSelf::NONE, None),
        };
        // The members' `Self::X` reads the impl's trait (types.rs `SelfTy::Ty`).
        let env = Env { self_: SelfTy::Ty(self_ty, written), ..env };
        let ms = members(names, key).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
        let mut assoc = Vec::new_in(Global);
        let mut fns = Vec::new_in(Global);
        let mut fn_texts = Vec::new_in(Global);
        let mut const_texts = Vec::new_in(Global);
        for m in &ms {
            let low = lower(names, key, m.text).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
            let mv = low.view();
            if mv.cfg_out() {
                continue;
            }
            match m.detail {
                ItemDetail::Fn { .. } => {
                    let mk = keys.member(ck, MemberKind::Fn, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    fns.push((Name::of(m.name), mk));
                    fn_texts.push((mk, m.text));
                }
                ItemDetail::TypeAlias => {
                    let t = mv.head_opt(head::TY).ok_or(Why::Unsupported("an associated type with no type", whole))?;
                    assoc.push((Name::of(m.name), b.ty(&mut self.tys, &env, &mv, lf::TypeId(t))?));
                }
                ItemDetail::Const { .. } => {
                    let mk = keys.member(ck, MemberKind::Const, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    const_texts.push((Name::of(m.name), mk, m.text));
                }
                d => return Err(Why::Unsupported(crate::front::detail_name(d), whole)),
            }
        }
        let env = Env { self_assoc: &assoc, ..env };
        let mut preds = Preds::new();
        b.predicates(&mut self.tys, &env, v, 0, None, self.sized, &mut preds)?;
        let mut consts = Vec::with_capacity_in(const_texts.len(), Global);
        for &(name, mk, text) in &const_texts {
            let low = lower(names, key, text).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
            let cv = low.view();
            let at = Range::new(0, text.len() as u32);
            let t = cv.head_opt(head::TY).ok_or(Why::Unsupported("an associated constant with no type", at))?;
            let output = b.ty(&mut self.tys, &env, &cv, lf::TypeId(t))?;
            let mut own_bounds = Vec::new_in(Global);
            own_bounds.extend_from_slice(&bounds);
            let mut self_assoc = Vec::with_capacity_in(assoc.len(), Global);
            self_assoc.extend_from_slice(&assoc);
            let header = tc::FnPtrHeader { abi: tc::Abi::Rust, safety: tc::Safety::Safe };
            let mut cp = Preds::new();
            cp.clauses.extend_from_slice(&preds.clauses);
            self.preds.insert(mk, cp);
            let row = FnHead { lib: key, text, container: Some(ck), parent: generics, own: 0, declared: 0, self_param: false, header, inputs: Vec::new_in(Global), output, has_body: false, module, params: ps.clone(), consts: cs.clone(), bounds: own_bounds, self_: env.self_, self_assoc };
            self.fns.insert(mk, row);
            consts.push((name, mk));
        }
        for &(mk, text) in &fn_texts {
            let low = lower(names, key, text).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
            self.member_fn_head(names, key, mk, Some(ck), &env, generics, &preds.clauses, &low.view(), text)?;
        }
        self.preds.insert(ck, preds);
        self.impls.insert(ck, ImplHead { generics, self_ty, trait_, trait_args, assoc, fns, consts, polarity });
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    /// A member fn's head: as [`Store::fn_head`], but a refusal other than a need is filed in
    /// `fn_gaps` and the container's head goes on (a need is passed up: the store makes the
    /// head it names and makes the container again).
    #[expect(clippy::too_many_arguments, reason = "the fn's parts and its container's")]
    fn member_fn_head(&mut self, names: &'n Names<'n>, lib: LibKey, key: ItemKey, container: Option<ItemKey>, outer: &Env<'_>, parent: u32, inherited: &[PClause], v: &View<'_>, text: &'n str) -> Result<(), Why> {
        match self.fn_head(names, lib, key, container, outer, parent, inherited, v, text) {
            Err(why @ (Why::Need(_) | Why::NeedConst(..))) => Err(why),
            Err(why) => {
                self.fn_gaps.insert(key, why);
                Ok(())
            }
            Ok(()) => {
                self.fn_gaps.remove(&key);
                Ok(())
            }
        }
    }

    /// A fn's signature and clauses: `env` is its container's (its params `0..parent`),
    /// `inherited` its container's clauses, `v` its own text's.
    #[expect(clippy::too_many_arguments, reason = "the fn's parts and its container's")]
    fn fn_head(&mut self, names: &'n Names<'n>, lib: LibKey, key: ItemKey, container: Option<ItemKey>, outer: &Env<'_>, parent: u32, inherited: &[PClause], v: &View<'_>, text: &'n str) -> Result<(), Why> {
        let mut ps = Vec::new_in(Global);
        ps.extend_from_slice(outer.params);
        let mut cs = Vec::new_in(Global);
        cs.extend_from_slice(outer.consts);
        let declared = {
            let b = Builder { resolver: Resolver { names }, heads: &self.maps };
            own_generics(&b, &mut self.tys, outer.module, v, parent, &mut ps, &mut cs)?
        };
        // Each `impl Trait` parameter type is one more own type parameter, after the declared
        // ones; it has no name, so it is in `anon`, never in `ps`.
        let mut anon = Vec::new_in(Global);
        anon_params(v, parent + declared, &mut anon)?;
        let own = declared + anon.len() as u32;
        let mut bounds = Vec::new_in(Global);
        let Bounds::Done(outer_bounds) = outer.bounds else {
            return Err(Why::Program("a fn's container's bounds were not lowered before the fn".into(), Range::new(0, 0)));
        };
        bounds.extend_from_slice(outer_bounds);
        let b = Builder { resolver: Resolver { names }, heads: &self.maps };
        let env = Env { module: outer.module, params: &ps, bounds: Bounds::Building(&[]), self_: outer.self_, self_assoc: outer.self_assoc, anon: &anon, consts: &cs };
        b.param_bounds(&mut self.tys, &env, v, parent, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        let self_ty = match env.self_ {
            SelfTy::Trait(_) => Some(intern(&mut self.tys, PNode::Param(0))?),
            SelfTy::Ty(t, _) => Some(t),
            SelfTy::None => None,
        };
        let mut inputs = Vec::new_in(Global);
        let mut self_param = false;
        let list = v.list(head::PARAMS);
        for (k, &n) in v.rows(list)?.iter().enumerate() {
            let prm = lf::Param::decode(n)?;
            let r = v.site(v.row_range(list.start + k as u32)?)?;
            let ty = match (prm.self_kind, self_ty) {
                (lf::SelfKind::No | lf::SelfKind::Typed, _) => b.ty(&mut self.tys, &env, v, prm.ty.ok_or(Why::Unsupported("a parameter with no type", r))?)?,
                (lf::SelfKind::Value, Some(s)) => s,
                (lf::SelfKind::Ref(m), Some(s)) => intern(&mut self.tys, PNode::Ref(m, s))?,
                (_, None) => return Err(Why::Program("a `self` parameter outside a trait or an impl".into(), r)),
            };
            self_param |= k == 0 && prm.self_kind != lf::SelfKind::No;
            inputs.push(ty);
        }
        let output = match v.head_opt(head::TY) {
            Some(t) => b.ty(&mut self.tys, &env, v, lf::TypeId(t))?,
            None => intern(&mut self.tys, PNode::Tuple(List::EMPTY))?,
        };
        let flags = v.head[head::FLAGS];
        let whole = Range::new(0, text.len() as u32);
        if flags & (sig::ASYNC | sig::GEN | sig::VARIADIC) != 0 {
            return Err(Why::Unsupported("an `async fn`, a `gen fn` or a variadic fn", whole));
        }
        let header = tc::FnPtrHeader { abi: abi_of(v, whole)?, safety: if flags & sig::UNSAFE != 0 { tc::Safety::Unsafe } else { tc::Safety::Safe } };
        let has_body = flags & sig::HAS_BODY != 0;
        let mut preds = Preds::new();
        preds.clauses.extend_from_slice(inherited);
        b.predicates(&mut self.tys, &env, v, parent, None, self.sized, &mut preds)?;
        b.anon_predicates(&mut self.tys, &env, v, self.sized, &mut preds)?;
        self.preds.insert(key, preds);
        let mut self_assoc = Vec::with_capacity_in(env.self_assoc.len(), Global);
        self_assoc.extend_from_slice(env.self_assoc);
        let (module, self_) = (env.module, env.self_);
        self.maps.const_params.insert(lib, cs.clone());
        self.fns.insert(key, FnHead { lib, text, container, parent, own, declared, self_param, header, inputs, output, has_body, module, params: ps, consts: cs, bounds, self_, self_assoc });
        Ok(())
    }
}
