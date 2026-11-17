//! Step 2's store: the lowered heads of structs, enums, unions, traits, impls and fns, filled
//! on demand by a bounded work list. An item asked for is lowered from the names library's
//! text of it (`lower_signature`, one item at a time, under its crate's cfg) and its types are
//! built by [`Builder`]; a [`Why::Need`] lowers that key's head first and tries the item again.
//! A cycle of needs is [`HeadError::Cycle`]. Trait and impl members are read from the item's
//! outline, each lowered alone, a member its crate's cfg turns off left out.
//!
//! Where-clauses are built by [`super::preds`] into [`Store::preds`]; a trait's predicates and
//! fn signatures are [`Want::Members`], apart from its head; an impl's fn's signature is its
//! own [`Want::MemberFn`], made when that fn is used ([`member_fn`]). Not here yet: `where const`
//! clauses (collected, not keyed), attributes (`#[repr]`, ABI, `no_mangle`), consts, statics.

use std::alloc::Global;
use std::format;
use std::vec::Vec;

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_lower::machine::sig;
use blaze_lower::{CfgAnswer, Lowered, View, form as lf, head};
use blaze_own::Alloc;
use blaze_scope::crate_roots::TextMarks;
use blaze_surface::{ItemDetail, Range};
use blaze_typeck as tc;
use blaze_typeck::{ItemKey, List, Name, PClause, PNode, PTy, PortableTys};

use super::dyn_ty::Member as DynMember;
use super::item_site::MarksLost;
use super::keys::{KeyClash, Keys, MemberKind, checker_of};
use super::preds::Preds;
use super::source::NamesSource;
use super::types::{Bounds, Builder, Env, Heads, ParamBound, SelfTy, TraitAliasHead, Why, intern, intern_list};
use crate::resolve::Paths;

mod member_fn;
pub use member_fn::{member_fn_want, member_head_want};

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
    /// `by` needs `need`, which was just made, yet its lookup still fails: a lookup asking for
    /// a want that does not make what it reads.
    NeedMade { need: Want, by: Want },
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
            // Each key whole: a key's short form is its crate's part, so two items of one
            // crate printed short read as one item needing itself.
            HeadError::Cycle(ks) => {
                write!(f, "these items need each other's heads in a cycle: [")?;
                for (i, k) in ks.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k:x}")?;
                }
                write!(f, "]")
            }
            HeadError::NeedMade { need, by } => write!(f, "{by:?} needs {need:?}, which is made, yet asks for it again (its lookup reads what that want does not make)"),
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
    /// Each trait's associated type's count of its OWN type and const parameters (`type
    /// MapStorage<V>`: one), by the trait and the name, filed with the trait's members
    /// ([`Heads::assoc_generics`]): what `<K as Key>::MapStorage<V>` is checked against.
    pub assoc_generics: HashMap<(LibKey, Name), u32, Global>,
    /// The outlives where clauses of each trait's associated type that writes one (`type
    /// Iter<'a> where Self: 'a;`), by the trait and the name: read and kept, not checked
    /// (gat.rs [`super::gat::OUTLIVES_NOT_CHECKED`]).
    pub assoc_where: HashMap<(LibKey, Name), super::gat::AssocWhere, Global>,
    /// Each trait's associated type's own const parameters (own position, declared type), by
    /// the trait and the name, filed with the trait's members ([`Heads::assoc_const_params`]):
    /// the type a constant written in `Tr<Buf<3>: Bound>` is made at (gat_bounds.rs).
    pub assoc_consts: HashMap<(LibKey, Name), Vec<(u32, PTy), Global>, Global>,
    /// Each trait's supertraits written as bounds on `Self` (see [`Heads::trait_supers`]):
    /// made with its members ([`Want::Members`]), since a supertrait's head may need the
    /// trait's (`FromResidual<R = <Self as Try>::Residual>`).
    pub supers: HashMap<LibKey, Vec<(LibKey, List), Global>, Global>,
    /// Each type alias's generic count and aliased type ([`Heads::alias`]); `None` where its
    /// head was refused, with the why in `alias_gaps`: an alias no path uses never fails a
    /// check, and one that is used fails only the item that names it.
    pub aliases: HashMap<LibKey, Option<(u32, PTy)>, Global>,
    /// Each type alias's parameter defaults (over its own parameters, `None` where none is
    /// written), made with its head: `type Result<T, E = Error>`.
    pub alias_defaults: HashMap<LibKey, Vec<Option<PTy>, Global>, Global>,
    pub alias_gaps: HashMap<LibKey, Why, Global>,
    /// Each trait alias's head ([`Heads::trait_alias`]; made by `Store::trait_alias`).
    pub trait_aliases: HashMap<LibKey, TraitAliasHead, Global>,
    /// Each `usize` constant item a head's array length names, by its value
    /// ([`Why::NeedConst`]): filled by the driver that has the constant evaluator.
    pub const_lens: HashMap<LibKey, u64, Global>,
    /// Each evaluated constant a head names, by its value's bits in its OWN type's width (an
    /// `i8` of -8 is `0xf8`; a `u128` or `i128` whole), as blaze-consteval writes them and as
    /// `PNode::ConstValue` holds them: the reader knows the type and extends by its sign.
    /// Keyed by a constant item's key or an anonymous constant's need key
    /// ([`super::anon::need_key`]); filled by `check::eval_len` beside `const_lens`, which
    /// holds only those that fit 64 bits.
    pub const_bits: HashMap<LibKey, u128, Global>,
    /// The anonymous constants the type builder asked for and no head holds yet
    /// ([`Heads::ask_anon`]): filed as heads by [`super::anon::file_asked`], which
    /// `check::eval_len` calls before it evaluates. In asking order; a repeat is skipped there.
    pub anon_asked: core::cell::RefCell<Vec<super::anon::Ask, Global>>,
    /// Each anonymous constant the evaluator refused, by its need key, with the refusal as
    /// printed ([`Heads::anon_failed`]): the head that wrote it is refused naming both.
    pub anon_failed: HashMap<LibKey, std::boxed::Box<str, Global>, Global>,
    /// The qualified constant leaves the type builder asked for (`<B4 as Spec>::BITS`:
    /// [`Heads::ask_qpath`]) and each answered one's type; answered by
    /// [`super::const_qpath::eval_asked`], which `check::eval_len` calls. A value is in
    /// `const_bits` and a refusal in `anon_failed`, under the leaf's need key.
    pub qpaths: core::cell::RefCell<super::const_qpath::Table>,
    /// The inherent constant leaves the type builder asked for (`Self::MAX_SIZE`,
    /// `InlineExtension::MAX`: [`Heads::inherent_leaves`]), each one's state and type;
    /// answered by [`super::const_inherent::eval_asked`] (`check::eval_len`) or
    /// [`super::const_inherent::first`] (the evaluator's work list). A value is in
    /// `const_bits` and a refusal in `anon_failed`, under the leaf's need key.
    pub inherents: core::cell::RefCell<super::const_inherent::Table>,
    /// Each item's const generic parameters (index, declared type), sorted by index: a
    /// struct's, enum's, union's, trait's and alias's own (made with its head, what
    /// [`Heads::const_params`] reads), an impl's own, and a fn's with its container's first
    /// (the checker's `const_params`). An item with none has an empty row once made.
    pub const_params: HashMap<LibKey, Vec<(u32, PTy), Global>, Global>,
    /// Each enum with no generic parameters whose every variant is a unit variant, by its
    /// checker key, with its variant count: what a const parameter's type may be past the
    /// integers, `char` and `bool` ([`super::const_enum`]). Filed by `Store::adt_head` with
    /// the enum's generics, so before any path to the enum has a type.
    pub fieldless_enums: HashMap<ItemKey, u32, Global>,
    /// How many lowerings of a written bound's arguments are nested right now
    /// ([`Heads::written_depth`]): zero between any two heads.
    pub written_depth: core::cell::Cell<u32>,
    /// How many constant evaluations are running now (consts.rs `Consts::value_at` counts
    /// itself in and out): zero where no evaluator is at work. `Records::consts` is `None`
    /// both while one runs and for a caller that has no evaluator at all; this tells the two
    /// apart (check.rs `make`: only the second refuses a head by name).
    pub evaluating: core::cell::Cell<u32>,
}

impl Heads for Maps {
    fn written_depth(&self) -> Option<&core::cell::Cell<u32>> {
        Some(&self.written_depth)
    }
    fn const_len(&self, key: LibKey) -> Option<u64> {
        self.const_lens.get(&key).copied()
    }
    fn const_bits(&self, key: LibKey) -> Option<u128> {
        self.const_bits.get(&key).copied().or_else(|| self.const_lens.get(&key).map(|&v| u128::from(v)))
    }
    fn anon_failed(&self, key: LibKey) -> Option<&str> {
        self.anon_failed.get(&key).map(|why| &**why)
    }
    /// Kept for `check::eval_len` to file; one asked twice before then is kept once.
    fn ask_anon(&self, ask: super::anon::Ask) -> bool {
        let mut asked = self.anon_asked.borrow_mut();
        if !asked.iter().any(|a| a.need == ask.need) {
            asked.push(ask);
        }
        true
    }
    /// Kept for `check::eval_len` to answer ([`super::const_qpath::eval_asked`]).
    fn ask_qpath(&self, ask: super::const_qpath::QAsk) -> bool {
        self.qpaths.borrow_mut().ask(ask);
        true
    }
    fn qpath_ty(&self, need: LibKey) -> Option<PNode> {
        self.qpaths.borrow().ty(need)
    }
    fn inherent_leaves(&self) -> Option<&core::cell::RefCell<super::const_inherent::Table>> {
        Some(&self.inherents)
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
    fn alias_defaults(&self, alias: LibKey) -> &[Option<PTy>] {
        self.alias_defaults.get(&alias).map_or(&[], |d| &d[..])
    }
    fn alias_gap(&self, alias: LibKey) -> Option<&Why> {
        self.alias_gaps.get(&alias)
    }
    fn trait_alias(&self, alias: LibKey) -> Option<&TraitAliasHead> {
        self.trait_aliases.get(&alias)
    }
    fn trait_supers(&self, trait_: LibKey) -> Option<&[(LibKey, List)]> {
        self.supers.get(&trait_).map(|s| &s[..])
    }
    fn trait_has_assoc(&self, trait_: LibKey, name: Name) -> Option<bool> {
        self.traits.get(&trait_).map(|t| t.assoc.contains(&name))
    }
    fn assoc_generics(&self, trait_: LibKey, name: Name) -> Option<u32> {
        self.assoc_generics.get(&(trait_, name)).copied()
    }
    fn assoc_const_params(&self, trait_: LibKey, name: Name) -> Option<&[(u32, PTy)]> {
        self.assoc_consts.get(&(trait_, name)).map(|c| &c[..])
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

/// Whether a field default (`a: u8 = 1`, RFC 3681 `default_field_values`) may be written on
/// a field of `adt`'s variant of constructor `ctor`, at `r`:
/// - only on a named field of a struct or an enum's struct variant: a tuple struct's or tuple
///   variant's field and a union's field are refused, each by name (rustc's "default fields
///   are not supported in tuple structs" and "unions cannot have default field values");
/// - in a blaze edition (the crate's `std` is bstd) the feature is on unasked (D126); in a
///   Rust edition it needs `#![feature(default_field_values)]` at the crate root (the crate
///   root record's `features`, `NamesSource::has_feature`), refused naming the feature
///   without it.
pub(crate) fn field_default_allowed<'n, S: NamesSource<'n>>(names: S, adt: LibKey, kind: tc::AdtKind, ctor: tc::CtorKind, r: Range) -> Result<(), Why> {
    if kind == tc::AdtKind::Union {
        return Err(Why::Program(Box::from("a union's field may not have a default value (RFC 3681: unions cannot have default field values)"), r));
    }
    if ctor != tc::CtorKind::Braces {
        return Err(Why::Program(Box::from("a tuple struct's or tuple variant's field may not have a default value (RFC 3681: only a named field has one)"), r));
    }
    let Some(krate) = names.crate_of(adt) else {
        return Err(Why::Program(Box::from("a field default: the item's crate is not answered"), r));
    };
    match (names.std_is(krate), names.has_feature(krate, "default_field_values")) {
        (Some(blaze_unit_graph::StdIs::Bstd(_)), _) | (Some(blaze_unit_graph::StdIs::Rust), Some(true)) => Ok(()),
        (Some(blaze_unit_graph::StdIs::Rust), Some(false)) => Err(Why::Program(
            Box::from("a field default needs `#![feature(default_field_values)]` at the crate root in a Rust edition (on unasked in a blaze edition)"),
            r,
        )),
        (Some(blaze_unit_graph::StdIs::Rust), None) => Err(Why::Program(Box::from("a field default: the crate root's features are not answered"), r)),
        (None, _) => Err(Why::Program(Box::from("a field default: the crate's edition (which `std` it names) is not answered"), r)),
    }
}

/// An ADT head's own syntax errors (its lowering's `Syntax` findings) refuse it at their place
/// (rule 3: a head is never made from text that does not parse). The parser takes `= value`
/// only after a NAMED field (RFC 3681), so a tuple field's `u8 = 1` is a syntax error whose
/// rest begins with `=`: refused by name as [`field_default_allowed`] refuses one.
fn adt_syntax(v: &View<'_>) -> Result<(), Why> {
    let Some(d) = v.diags().next() else { return Ok(()) };
    let d = d?;
    let r = v.site(d.range)?;
    // The field the error is in: from the `(` or `,` before it to the `,` or `)` after it (the
    // parser stops at the `=` or past its value, by how it recovers).
    let text = v.item();
    let at = (r.start as usize).min(text.len());
    let from = text.get(..at).and_then(|t| t.rfind(['(', ','])).map_or(0, |i| i + 1);
    let to = text.get(at..).and_then(|t| t.find([',', ')'])).map_or(text.len(), |i| at + i);
    if let Some(eq) = text.get(from..to).and_then(|piece| piece.find('=')) {
        let start = (from + eq) as u32;
        return Err(Why::Program(
            Box::from("a tuple struct's or tuple variant's field may not have a default value (RFC 3681: only a named field has one)"),
            Range::new(start, to as u32),
        ));
    }
    let near = text.get(from..to).unwrap_or("");
    Err(Why::Program(format!("the item's text does not parse here ({:?}, in `{near}`)", d.kind).into_boxed_str(), r))
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
    /// Marked `#[blaze::args]` (rule 83, D126): an options struct, a call's named arguments and
    /// nothing else. The world answers it as [`tc::TypeAttrs::ARGS`] (world_items.rs `attrs`),
    /// which the body check's `args-inline` reads (blaze-typeck rules/args_inline.rs); the head
    /// record keeps it, so an ADT of any crate is known.
    pub args: bool,
}

/// Whether an attribute's path, as written, is `blaze::args` (spaces around `::` allowed).
fn is_args_attr(path: &str) -> bool {
    path.split_once("::").is_some_and(|(tool, rest)| tool.trim() == "blaze" && rest.trim() == "args")
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
fn discriminants<'n, S: NamesSource<'n>>(b: &Builder<'_, Maps, S>, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, repr: blaze_build::layout::ReprAttrs, written: &[(Option<lf::ConstIdx>, Range)]) -> Result<Vec<i128, Global>, Why> {
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
    // A written discriminant that is a constant not evaluated yet (a constant item, or an
    // expression `1 << 0`: an anonymous constant, front2/anon.rs) is a need; EVERY written one
    // is asked before the first need is returned, so the driver evaluates them in one round.
    let mut need: Option<Why> = None;
    for (k, &(c, r)) in written.iter().enumerate() {
        let value = match c {
            Some(c) => {
                let at = v.const_range(c)?;
                let cr = v.site(at)?;
                let made = match b.const_term(tys, env, v.text_at(at)?, cr, &|_| Ok(ty)) {
                    Ok(made) => made,
                    Err(why @ Why::NeedConst(..)) => {
                        need.get_or_insert(why);
                        continue;
                    }
                    Err(why) => return Err(why),
                };
                if need.is_some() {
                    continue;
                }
                let Some(&PNode::ConstValue { bits: raw, .. }) = tys.view().get(made) else {
                    return Err(Why::ConstPath(Box::from(v.text_at(at)?.trim()), cr));
                };
                // The bits are the value's in the type's width: sign-extend a signed one.
                if signed && bits == 128 {
                    raw as i128
                } else if signed && raw >> (bits - 1) & 1 == 1 {
                    (raw | !((1u128 << bits) - 1)) as i128
                } else {
                    i128::try_from(raw).map_err(|_| Why::Unsupported("a `u128` discriminant past `i128::MAX`", cr))?
                }
            }
            // After a need the values so far are not all known: nothing is judged this pass.
            None if need.is_some() => continue,
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
    if let Some(why) = need {
        return Err(why);
    }
    Ok(out)
}

/// `#[rustc_legacy_const_generics(..)]`'s arguments (`args`, the parenthesized list): the
/// unsuffixed decimal positions, comma separated, appended to `out` in written order (the
/// checker binds them to the own const parameters in that order). Any other form is refused.
fn legacy_positions(args: &str, r: Range, out: &mut Vec<u32, Global>) -> Result<(), Why> {
    let inner = args.trim().strip_prefix('(').and_then(|a| a.strip_suffix(')')).ok_or(Why::Unsupported("a `rustc_legacy_const_generics` attribute without a parenthesized list", r))?;
    for part in inner.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Why::Unsupported("a `rustc_legacy_const_generics` position that is not an unsuffixed decimal integer", r));
        }
        let k = part.parse::<u32>().map_err(|_| Why::Unsupported("a `rustc_legacy_const_generics` position past `u32::MAX`", r))?;
        out.push(k);
    }
    Ok(())
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
    /// A C-variadic foreign fn (`...` last): `inputs` are the declared parameters only.
    pub c_variadic: bool,
    /// `#[rustc_legacy_const_generics(i, ..)]`'s positions, as written: the call argument at
    /// each is the next own const generic argument (blaze-typeck rules/legacy_const.rs). Empty
    /// for every other fn.
    pub legacy_const_generics: Vec<u32, Global>,
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
    /// Its fns by name and key. No signature is made with the head: each is its own
    /// [`Want::MemberFn`], made when the fn is used (work at item grain).
    pub fns: Vec<(Name, ItemKey), Global>,
    /// For each of `fns`, in its order: whether its first parameter is `self`, read from its
    /// text ([`member_fn::takes_self`]): what answering a name needs, without a signature.
    pub fn_self: Vec<bool, Global>,
    /// Its associated constants (the Reference, "Associated constants"), each with a row in
    /// `Store::fns` as the checker reads one: no inputs, its type the output, and its body
    /// where its value is written (`has_body`: what the constant evaluator lowers).
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
    /// The signature of fn `.1` of impl `.0`: its own want, made after the impl's head and
    /// apart from it ([`member_fn`]), so naming a member never makes a signature.
    MemberFn(LibKey, ItemKey),
}

impl Want {
    /// The library key it is made from: an impl's fn's is its impl's.
    pub fn key(self) -> LibKey {
        match self {
            Want::Head(k) | Want::Adt(k) | Want::Impl(k) | Want::Fn(k) | Want::Members(k) | Want::MemberFn(k, _) => k,
        }
    }
}

/// The lowered heads, by key, and the type table they are in.
pub struct Store<'n> {
    pub tys: PortableTys<'static>,
    pub maps: Maps,
    pub adts: HashMap<ItemKey, AdtRow, Global>,
    /// Each ADT's field defaults (`struct S { a: u8 = 1 }`, RFC 3681), made with its head in
    /// `adts`: one per field of `AdtRow::fields`, the key of the default's head in `fns` (an
    /// anonymous constant of the field's type over the ADT's generics), or `None`; empty when
    /// no field has one. A head record carries them (head_record.rs, form 8); a row filed
    /// without its entry is the world's gap `field_defaults`, never "no defaults".
    pub field_defaults: HashMap<ItemKey, Vec<Option<ItemKey>, Global>, Global>,
    pub impls: HashMap<ItemKey, ImplHead, Global>,
    pub fns: HashMap<ItemKey, FnHead<'n>, Global>,
    /// Each ADT's, trait's, impl's and fn's where-clauses, own and inherited (a fn's from its
    /// trait or impl), with its `where const` conditions.
    pub preds: HashMap<ItemKey, Preds, Global>,
    /// The `sized` lang trait: each own type parameter's implied bound.
    pub sized: Option<LibKey>,
    /// The library's `Box`, `Rc`, `Arc` and `Pin`, a trait method's typed receiver is read
    /// against for dyn compatibility ([`super::receiver::Pointers::find`]; set by the maker of
    /// the store: [`Store::new`] leaves them not looked up, so every such receiver is a gap).
    pub pointers: super::receiver::Pointers,
    /// Each trait's parts of the checker's `TraitDef` that need its members lowered.
    pub trait_defs: HashMap<ItemKey, TraitExtra, Global>,
    /// The member fns whose signature could not be made, by why: their container's head is
    /// made without them, and a body that uses one gets [`super::world_items::Gap::FnHead`]
    /// (a trait's head does not depend on its fns' signatures: `FnMut` and `extern "rust-call"`).
    pub fn_gaps: HashMap<ItemKey, Why, Global>,
    /// Each impl's frame its fns' signatures are made in ([`member_fn::MemberFrame`]), kept
    /// when its head is made, by the impl's checker key.
    member_frames: HashMap<ItemKey, member_fn::MemberFrame, Global>,
    /// Each tuple struct's and tuple variant's constructor fn, by the variant's key: made with
    /// its ADT's head.
    pub ctors: HashMap<ItemKey, CtorSig, Global>,
    /// Each static whose head is made (its type is its [`FnHead`] of no inputs, as a const's),
    /// with its mutability (`static mut`): the checker's `Def::Static`.
    pub statics: HashMap<ItemKey, tc::Mutbl, Global>,
    /// Each opaque type a fn's head made (an `impl Trait` in its return type, an `async fn`'s
    /// `impl Future`), by its key ([`super::keys::opaque_key`]): its fn and its index. Its
    /// bounds are in `preds` under the same key, with the opaque type as `Self`.
    pub opaques: HashMap<ItemKey, (ItemKey, u32), Global>,
    /// The hidden types of the opaques defined by the bodies checked clean in this command,
    /// and the running check's leak list ([`super::opaque_auto`]).
    pub hidden: super::opaque_auto::HiddenTable<'static>,
    /// The items declared inside the bodies lowered so far, kept in the command's frame: a
    /// local item's head is made from its row ([`super::locals`]).
    pub locals: super::locals::Locals<'n>,
    /// Each head's lifetime part, made when first asked for ([`super::lifetimes`]:
    /// `Store::fn_life`, `Store::adt_life`, `Store::container_life`): what the erased types
    /// above leave out and the borrow check reads.
    pub lives: super::lifetimes::Lives,
    /// The run's sources a body's macro calls expand with ([`super::body_host::BodyHost`]:
    /// the crate's environment, its texts, the proc-macro runner), the same item-level
    /// expansion reads. `None` (as [`Store::new`] makes it): every `env!`, include and proc
    /// macro in a body is refused by name. The command sets it once, before any body.
    pub sources: Option<&'n dyn super::body_host::BodySources>,
    /// Each body checked clean in this run: its read list's digest, by owner, filed where the
    /// check finished (front2 body_reads.rs); the list itself is dropped there.
    pub reads: super::body_reads::Digests,
    /// Each closure of each body checked clean in this run (its kind and signature, in `tys`),
    /// filed beside `reads`: what the world answers a closure of ANOTHER body from
    /// ([`super::foreign_closures`]).
    pub closures: super::foreign_closures::ClosureRows<Global>,
}

/// A trait's clauses on `Self` and its associated types with their keys and defaults (in the
/// store's table); made with its members ([`Want::Members`]).
pub struct TraitExtra {
    pub supertraits: Vec<PClause, Global>,
    pub assoc: Vec<tc::AssocTyDef, Global>,
    /// Its associated constants, declared or defaulted (the Reference, "Associated
    /// Constants"), in the order written: each with a row in `Store::fns` as an impl's has
    /// ([`ImplHead::consts`]); the row's `has_body` says a default is written.
    pub consts: Vec<(Name, ItemKey), Global>,
}

/// The step's result: done, the head it needs first, or the want it needs first (an impl's
/// head before one of its fns' signatures).
enum Step {
    Done,
    Need(LibKey),
    First(Want),
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
/// bounds' type arguments (`impl Into<impl Show>`: the `Into` one first), and then those in its
/// bounds' associated type bindings (`impl IntoIterator<Item = impl Into<u64>>`: the
/// `IntoIterator` one, then the `Into<u64>` one; clap's `ArgGroup::args(self, ns: impl
/// IntoIterator<Item = impl Into<Id>>)`). The Reference, "Impl trait": "`impl Trait` can only
/// appear as a parameter or return type of a non-extern function": the inner one is in the
/// parameter's type, so it is an anonymous type parameter too; its bounds are filed by
/// `Preds::impl_bounds` from this list and the binding lowers to its `Param(i)` through
/// `env.anon`. A binding's type follows the path's type arguments, which is text order: a
/// binding is written after every argument of its segment, and only on a trait path's last
/// segment. Not walked to, so refused where made ("an `impl Trait` type outside a fn's
/// parameters and return type"): an `impl` in a parenthesized `Fn(..) -> ..` bound, in a
/// binding's own arguments (`Out<impl X> = Y`) or under an associated type bound
/// (`Item: Into<impl X>`). Note: `impl Into<impl Show>` is accepted here and refused by rustc
/// (E0666); left as it is. A work list, no recursion.
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
                    // The path's type arguments, then its bindings' types; an associated type
                    // bound (`Error: Into<E>`) holds no type of the path: its bounds are
                    // clauses lowered by `Builder::bound` (bound_kids.rs). A bound that is no
                    // path is walked as a type.
                    if !super::bound_kids::bound_path_kids(v, ty, &mut kids)? {
                        kids.push(ty);
                    }
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
/// parameters in scope: a const parameter's type may not name one, E0770). The type is any
/// the pinned core's `ConstParamTy_` impls can cover (integers, `char`, `bool`, a struct or
/// enum, tuples and arrays of those, `&T`, `&str`, `&[T]`: [`super::const_param_ty::admits`],
/// which refuses every other type by name). Their count.
pub(super) fn own_generics<'n, S: NamesSource<'n>>(b: &Builder<'_, Maps, S>, tys: &mut PortableTys<'_>, module: LibKey, v: &View<'_>, base: u32, ps: &mut Vec<(Name, u32), Global>, consts: &mut Vec<(u32, PTy), Global>) -> Result<u32, Why> {
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
                // By the type's shape (const_param_ty.rs): `const ASSUME: Assume` (a struct,
                // core's `TransmuteFrom`), `const ORD: AtomicOrdering` (a fieldless enum),
                // `const FIELDS: &'static str`. A VALUE of a non-scalar type is refused where it
                // is read (types.rs `const_term`), never here.
                let scratch = tys.alloc();
                if let Err(no) = super::const_param_ty::admits(tys, t, scratch) {
                    let r = v.site(v.row_range(list.start + k as u32)?)?;
                    return Err(Why::Unsupported(no.text(), r));
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
fn own_defaults<'n, S: NamesSource<'n>>(b: &Builder<'_, Maps, S>, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, base: u32, out: &mut Vec<Option<PTy>, Global>) -> Result<(), Why> {
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
pub(super) fn lower<'n, S: super::source::NamesSource<'n>>(names: S, key: LibKey, text: &'n str) -> Result<Lowered<'static, 'n>, HeadError> {
    let krate = names.crate_of(key).ok_or(HeadError::NoText(key))?;
    // Over records a crate with no items record has no atoms and no edition: the source filed
    // why (`source::SourceError::NoRecord`), and the item has no text to lower.
    let mut cfg = CrateCfg(names.cfg_atoms(krate).ok_or(HeadError::NoText(key))?);
    let edition = names.edition_of(krate).ok_or(HeadError::NoText(key))?;
    blaze_lower::lower_signature(text, edition, &mut cfg, Alloc::global()).map_err(|_| HeadError::Cfg(key))
}

/// A member of a trait or an impl: its name, kind and text, as the names library lists it.
struct Member<'n> {
    name: &'n str,
    detail: ItemDetail,
    text: &'n str,
    /// The macro call in the container's body whose expansion it is in (its tokens' marks are
    /// that expansion's: front2 item_site.rs `member_marks`).
    written_by: Option<LibKey>,
}

/// The members of trait or impl `key` from `Names::members`: those written in its body and
/// those a macro call there expands to (core's `impl u32 { uint_impl! { .. } }`), one list.
fn members<'n, S: super::source::NamesSource<'n>>(names: S, key: LibKey) -> Result<Vec<Member<'n>, Global>, HeadError> {
    names.crate_of(key).ok_or(HeadError::NoText(key))?;
    if !names.has_text(key) {
        return Err(HeadError::NoText(key));
    }
    let mut out = Vec::new_in(Global);
    // A member of a kind whose detail the source cannot give (over records: one that holds
    // ranges of its text; the source filed the gap).
    let mut no_detail = false;
    // A container with no member has no row in the library's members table.
    names.each_member(key, &mut |m| {
        // The call itself: its expansion's members are listed beside it (`written_by`).
        if m.kind == blaze_surface::ItemKind::MacroCall {
            return;
        }
        match m.detail {
            Some(detail) => out.push(Member { name: m.name.unwrap_or(""), detail, text: m.text, written_by: m.written_by }),
            None => {
                names.gap("a member's outline detail beyond its kind (heads.rs: members)", Some(m.key));
                no_detail = true;
            }
        }
    });
    if no_detail {
        return Err(HeadError::NoText(key));
    }
    Ok(out)
}

impl<'n> Store<'n> {
    /// An empty store; `sized` is the program's `#[lang = "sized"]` trait (none in a
    /// `no_core` rung that declares none).
    /// `frame` is the command's ([`super::Front2::frame`]), where bodies' local items are kept.
    /// `alloc` is the check's allocator, handed down from the program's entry: the tables are
    /// in it, and `tys.alloc()` gives it to everything the store keeps for the check.
    pub fn new(sized: Option<LibKey>, frame: &'n blaze_mem::Frame, alloc: Alloc<'static>) -> Store<'n> {
        Store {
            tys: PortableTys::new_in(alloc),
            maps: Maps::default(),
            adts: HashMap::new_in(Global),
            field_defaults: HashMap::new_in(Global),
            impls: HashMap::new_in(Global),
            fns: HashMap::new_in(Global),
            preds: HashMap::new_in(Global),
            sized,
            pointers: super::receiver::Pointers::NOT_LOOKED_UP,
            trait_defs: HashMap::new_in(Global),
            fn_gaps: HashMap::new_in(Global),
            member_frames: HashMap::new_in(Global),
            ctors: HashMap::new_in(Global),
            statics: HashMap::new_in(Global),
            opaques: HashMap::new_in(Global),
            hidden: super::opaque_auto::HiddenTable::new_in(alloc),
            locals: super::locals::Locals::new(frame),
            lives: super::lifetimes::Lives::new_in(alloc),
            sources: None,
            reads: super::body_reads::Digests::new(),
            closures: super::foreign_closures::ClosureRows::new_in(Global),
        }
    }

    /// What a [`Why::Need`] of `k` asks for: its head, or, for a trait whose head is made,
    /// its members (where its supertraits are filed).
    pub fn need(&self, k: LibKey) -> Want {
        if self.maps.traits.contains_key(&k) { Want::Members(k) } else { Want::Head(k) }
    }

    /// The marks the tokens of `key`'s text `text` keep, which its head's paths resolve by
    /// (front2 item_site.rs, token-mark-F1). An item declared in a body keeps its enclosing
    /// body's marks when a macro wrote that body (locals.rs `LocalHead::marks`: bstd's `main!`
    /// writes `static MANIFEST: $crate::grant::Manifest` in the user's `main`), read at its
    /// block's module.
    fn marks<S: NamesSource<'n>>(&self, names: S, key: LibKey, text: &str) -> Result<Option<TextMarks<'n>>, Why> {
        if let Some(local) = self.locals.head(checker_of(key)) {
            return Ok(local.marks);
        }
        super::item_site::item_marks(names, key, text).map_err(MarksLost::why)
    }

    /// Makes `want` and every head it needs, by a work list.
    pub fn ensure<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, want: Want) -> Result<(), HeadError> {
        let mut stack = Vec::new_in(Global);
        stack.push(want);
        // The want made by the step before: asked again at once, it was made and its lookup
        // still fails (no progress), which is named, never looped until `MOST_STEPS`.
        let mut just_made = None;
        for _ in 0..MOST_STEPS {
            let Some(&top) = stack.last() else { return Ok(()) };
            let need = match self.step(names, keys, top)? {
                Step::Done => {
                    just_made = stack.pop();
                    continue;
                }
                Step::Need(k) => self.need(k),
                Step::First(w) => w,
            };
            if just_made == Some(need) {
                return Err(HeadError::NeedMade { need, by: top });
            }
            if stack.iter().any(|w| *w == need) {
                return Err(HeadError::Cycle(cycle(&stack)));
            }
            just_made = None;
            stack.push(need);
        }
        Err(HeadError::Cycle(cycle(&stack)))
    }

    fn step<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, want: Want) -> Result<Step, HeadError> {
        let key = want.key();
        let ck = checker_of(key);
        // The one test the world's answers share (front2 by_origin.rs `Store::made`).
        let done = self.made(want);
        if done {
            return Ok(Step::Done);
        }
        // An impl's fn: its impl's head first (its frame, associated types and clauses), then
        // its own signature alone.
        if let Want::MemberFn(impl_, f) = want {
            if !self.impls.contains_key(&ck) {
                return Ok(Step::First(Want::Impl(impl_)));
            }
            return match self.member_fn(names, keys, impl_, f) {
                Ok(()) => Ok(Step::Done),
                Err(Why::Need(k)) => Ok(Step::Need(k)),
                Err(why) => Err(HeadError::Item(impl_, why)),
            };
        }
        let resolver = Paths { names };
        // An item declared in a body: its row's text and its block's module (`Locals::head_of`).
        let (detail, text, module) = match self.locals.head_of(key) {
            Some(local) => local?,
            None => {
                let detail = resolver.detail(key).ok_or(HeadError::NoText(key))?;
                // One row over records: the text and the module its names are read in.
                let it = names.text_of(key).ok_or(HeadError::NoText(key))?;
                (detail, it.text, it.module)
            }
        };
        let low = lower(names, key, text)?;
        let v = low.view();
        let got = match (want, detail) {
            (Want::Head(_), ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union) => {
                self.adt_head(names, key, module, &v)
            }
            (Want::Head(_), ItemDetail::Trait) => self.trait_head(names, keys, key, module, &v, text),
            (Want::Head(_), ItemDetail::TraitAlias) => self.trait_alias(names, keys, key, module, &v),
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
                let marks = self.marks(names, key, text).map_err(|why| HeadError::Item(key, why))?;
                self.fn_head(names, key, ck, None, &env, 0, &[], &v, text, marks)
            }
            // A static's head is its type, as a const's, and its mutability (the Reference,
            // "Static items": "Mutable statics", `static mut`).
            (Want::Fn(_), ItemDetail::Static { .. }) => {
                let env = Env { module, params: &[], bounds: Bounds::Done(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &[] };
                let marks = self.marks(names, key, text).map_err(|why| HeadError::Item(key, why))?;
                let made = self.fn_head(names, key, ck, None, &env, 0, &[], &v, text, marks);
                if made.is_ok() {
                    let m = if v.head[head::FLAGS] & sig::MUT != 0 { tc::Mutbl::Mut } else { tc::Mutbl::Not };
                    self.statics.insert(ck, m);
                }
                made
            }
            // Rule 19: every detail named (the shared names say "this item" for the kinds the
            // old front end made, which here are a head asked for under another want).
            (w, d) => {
                let what = match (w, d) {
                    (Want::Head(_), ItemDetail::Fn { .. }) => "a fn (asked for as a type's or trait's head: a `Why::Need` of a fn's key)",
                    (Want::Adt(_), ItemDetail::Fn { .. }) => "a fn (asked for as an ADT)",
                    (Want::Impl(_), ItemDetail::Fn { .. }) => "a fn (asked for as an impl)",
                    (Want::Members(_), ItemDetail::Fn { .. }) => "a fn (asked for as a trait's members)",
                    (Want::MemberFn(..), ItemDetail::Fn { .. }) => "a fn (asked for as an impl's member fn)",
                    (_, d) => match d {
                    ItemDetail::Struct => "a struct (asked for as other than an ADT)",
                    ItemDetail::Enum => "an enum (asked for as other than an ADT)",
                    ItemDetail::Trait => "a trait (asked for as other than a trait)",
                    ItemDetail::Fn { .. } => "a fn (asked for as other than a fn)",
                    ItemDetail::Mod { .. } => "a module",
                    ItemDetail::Use { .. } => "a `use`",
                    ItemDetail::ExternBlock { .. } => "an `extern` block",
                    d => crate::front::detail_name(d),
                    },
                };
                return Err(HeadError::NotAHead(key, what));
            }
        };
        match got {
            Ok(()) => Ok(Step::Done),
            Err(Why::Need(k)) => Ok(Step::Need(k)),
            Err(why) => Err(HeadError::Item(key, why)),
        }
    }

    /// A trait alias's head (`trait A<T> = B<T> + C where ..;`): its generics (`Self` first)
    /// and the clauses it stands for, read in its own module as a trait's supertraits and where
    /// clauses are, with `Self` = `Param(0)`. An alias named in them is made first
    /// ([`Why::Need`]), so its clauses arrive expanded; aliases naming each other in a cycle
    /// are [`Store::ensure`]'s `HeadError::Cycle`.
    fn trait_alias<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let whole = Range::new(0, 0);
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        let own = own_generics(&b, &mut self.tys, module, v, 1, &mut ps, &mut cs)?;
        let me = intern(&mut self.tys, PNode::Param(0))?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::Ty(me, None), self_assoc: &[], anon: &[], consts: &cs };
        let mut bounds = Vec::new_in(Global);
        b.param_bounds(&mut self.tys, &env, v, 1, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        let mut preds = Preds::new();
        // `sized` is not given: an alias adds no implied `Sized` on its own parameters (a use
        // stands for what is written).
        b.predicates(&mut self.tys, &env, v, 1, Some(me), None, &mut preds)?;
        if !preds.conds.is_empty() {
            return Err(Why::Unsupported("`where const` in a trait alias", whole));
        }
        let mut traits = Vec::new_in(Global);
        for c in preds.clauses.iter() {
            let PClause::Trait { trait_, args } = *c else { continue };
            let first = self.tys.view().list(args).map_err(|_| Why::Unsupported("a clause list outside the store's table", whole))?.first().copied();
            if first == Some(me) {
                let lib = keys.library_in(names, trait_).ok_or(Why::Program(format!("the trait {trait_:?} has no library key").into_boxed_str(), whole))?;
                traits.push((lib, args));
            }
        }
        self.maps.trait_aliases.insert(key, TraitAliasHead { generics: own + 1, clauses: preds.clauses, traits });
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    /// A free type alias's head: its generics and the aliased type, read in the alias's own
    /// module with its own parameters (a bound on an alias's parameter binds nothing).
    fn alias<S: NamesSource<'n>>(&mut self, names: S, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        let generics = own_generics(&b, &mut self.tys, module, v, 0, &mut ps, &mut cs)?;
        // A free `type T;` with no type is an extern type (the Reference has a free alias
        // without a type as an error; core's `unsafe extern "C" { pub type Code; }`, the
        // pointee of `FnPtr::as_ptr`'s `NonNull<Code>`). It is its own unsized nominal type,
        // which the checker's portable form has no node for (`PNode` lacks `Foreign`, though
        // blaze-ty's `TyKind::Foreign` and the solver's `Shape::Foreign` exist).
        let t = v.head_opt(head::TY).ok_or(Why::Unsupported("an extern type (`extern { type T; }`): the checker's portable type has no foreign node", Range::new(0, 0)))?;
        // A bound on an alias's parameter is not checked where the alias is used, but it is
        // what `T::X` in the aliased type is read through (core's `ChangeOutputType`).
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &cs };
        let mut bounds = Vec::new_in(Global);
        b.param_bounds(&mut self.tys, &env, v, 0, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        // Its parameters' defaults (`type Result<T, E = Error>`): a path leaving them out takes
        // them, as a struct's (types.rs `with_defaults`).
        let mut defaults = Vec::new_in(Global);
        own_defaults(&b, &mut self.tys, &env, v, 0, &mut defaults)?;
        let ty = b.ty(&mut self.tys, &env, v, lf::TypeId(t))?;
        self.maps.alias_defaults.insert(key, defaults);
        self.maps.aliases.insert(key, Some((generics, ty)));
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    /// A struct's, enum's or union's head: its type parameter count and each one's default,
    /// lowered over the parameters before it (`struct Vec<T, A: Allocator = Global>`).
    fn adt_head<S: NamesSource<'n>>(&mut self, names: S, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        let g = own_generics(&b, &mut self.tys, module, v, 0, &mut ps, &mut cs)?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &cs };
        let mut defaults = Vec::new_in(Global);
        own_defaults(&b, &mut self.tys, &env, v, 0, &mut defaults)?;
        // A fieldless enum is filed before its generics are: a path to it has a type only
        // once `generics` holds it, and `own_generics` then finds it here.
        if let Some(variants) = super::const_enum::fieldless(names, key, v, g)? {
            self.maps.fieldless_enums.insert(checker_of(key), variants);
        }
        self.maps.generics.insert(key, (g, defaults));
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    fn trait_head<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>, _text: &'n str) -> Result<(), Why> {
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
        // `#[rustc_coinductive]` (core's `Sized`, `MetaSized`, `PointeeSized`): a cycle through
        // the trait's own steps holds (library/core/src/marker.rs: "`Sized` being coinductive,
        // despite having supertraits, is okay as there are no user-written impls").
        for &n in v.rows(v.list(head::ATTRS))? {
            if v.text_at(v.attr(n)?.path)?.trim() == "rustc_coinductive" {
                flags |= tc::TraitFlags::COINDUCTIVE;
            }
        }
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        let own = own_generics(&b, &mut self.tys, module, v, 1, &mut ps, &mut cs)?;
        // The trait's bounds are lowered after its head (they may name the trait): a default
        // written with `T::X` is refused as not yet readable, never as not found.
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::Trait(key), self_assoc: &[], anon: &[], consts: &cs };
        let mut defaults = Vec::new_in(Global);
        own_defaults(&b, &mut self.tys, &env, v, 1, &mut defaults)?;
        let ck = checker_of(key);
        let mut assoc = Vec::new_in(Global);
        let mut assoc_own: Vec<(Name, u32), Global> = Vec::new_in(Global);
        let mut assoc_where: Vec<(Name, super::gat::AssocWhere), Global> = Vec::new_in(Global);
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
                    // Its own type and const parameters, counted (a generic associated type:
                    // `type MapStorage<V>` has one; a lifetime is none): what arguments on
                    // its segment are checked against ([`Heads::assoc_generics`]). Filed with
                    // the head, as its name is: a path may name it before the members exist.
                    let mv = low.view();
                    let mut own_params = 0u32;
                    for &row in mv.rows(mv.list(head::GENERICS))? {
                        if !matches!(lf::GenericParam::decode(row)?, lf::GenericParam::Lifetime { .. }) {
                            own_params += 1;
                        }
                    }
                    assoc_own.push((Name::of(m.name), own_params));
                    // Its where clauses: an outlives clause is kept with the head, any
                    // other is refused by name (gat.rs).
                    let kept = super::gat::member_where(&mv, m.name)?;
                    if !kept.is_empty() {
                        assoc_where.push((Name::of(m.name), kept));
                    }
                    DynMember::Type
                }
                ItemDetail::Const { .. } => DynMember::Const,
                d => return Err(Why::Unsupported(crate::front::detail_name(d), whole)),
            };
            let shape = super::dyn_ty::member(&b.resolver, module, self.sized, &self.pointers, kind, &low.view())?;
            members.push(tc::dyn_compat::MemberFacts { name: Name::of(m.name), shape });
        }
        for (name, own_params) in assoc_own {
            self.maps.assoc_generics.insert((key, name), own_params);
        }
        for (name, kept) in assoc_where {
            self.maps.assoc_where.insert((key, name), kept);
        }
        self.maps.traits.insert(key, TraitHead { generics: own + 1, defaults, assoc, fns, flags: tc::TraitFlags(flags), members });
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    /// A trait's predicates (its supertraits and where clauses on its generics, `Self` =
    /// `Param(0)`) and its fns' signatures, each fn's clauses its trait's plus
    /// `Self: Trait<params>`.
    fn trait_members<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let whole = Range::new(0, 0);
        let ck = checker_of(key);
        let mut ps = Vec::new_in(Global);
        let own = param_names(v, 1, &mut ps, Consts::Keep)?;
        let cs = self.maps.const_params.get(&key).cloned().ok_or(Why::Need(key))?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::Trait(key), self_assoc: &[], anon: &[], consts: &cs };
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
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
                let lib = keys.library_in(names, trait_).ok_or(Why::Program(format!("the supertrait {trait_:?} has no library key").into_boxed_str(), whole))?;
                supers.push((lib, args));
            }
        }
        // A trait's `Self` is `MetaSized` unless its header states a sizedness (self_size.rs;
        // core's `Write::write_fmt` needs it for its block impl's `W: ?Sized` at `W = Self`): a
        // clause of every member's env, as `Self: Trait` is; not a supertrait (vtables).
        if let Some(meta) = super::self_size::implied_self_size(&self.maps, key, &supers, self.sized)? {
            let self_list = intern_list(&mut self.tys, &[me])?;
            inherited.push(PClause::Trait { trait_: self.maps.checker(meta), args: self_list });
        }
        self.maps.supers.insert(key, supers);
        let mut assoc = Vec::new_in(Global);
        let mut consts: Vec<(Name, ItemKey), Global> = Vec::new_in(Global);
        // Each associated type's own const parameters (own position, type): filed after the
        // loop into `Maps::assoc_consts`.
        let mut assoc_consts: Vec<(Name, Vec<(u32, PTy), Global>), Global> = Vec::new_in(Global);
        for m in &members(names, key).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))? {
            let low = lower(names, key, m.text).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
            let mv = low.view();
            if mv.cfg_out() {
                continue;
            }
            match m.detail {
                ItemDetail::Fn { .. } => {
                    let mk = keys.member(ck, MemberKind::Fn, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    let marks = super::item_site::member_marks(names, key, m.written_by, m.text).map_err(MarksLost::why)?;
                    self.member_fn_head(names, key, mk, Some(ck), &env, own + 1, &inherited, &mv, m.text, marks)?;
                }
                ItemDetail::TypeAlias => {
                    let item = keys.member(ck, MemberKind::Type, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    let marks = super::item_site::member_marks(names, key, m.written_by, m.text).map_err(MarksLost::why)?;
                    let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
                    let default = match mv.head_opt(head::TY) {
                        Some(t) => Some(b.ty(&mut self.tys, &env, &mv, lf::TypeId(t))?),
                        None => None,
                    };
                    // Its item bounds (`type A: B;` and the implied `Sized`), filed as the
                    // predicates of its own key with `Self` the alias `<Self as Trait<params>>::A`
                    // ([`Builder::assoc_ty_bounds`]): what the checker gives the solver as the
                    // alias's bounds. As a member fn's signature ([`Store::member_fn_head`]): a
                    // need is passed up; any other refusal is filed in `fn_gaps` under the
                    // type's key and the trait's head goes on: a body whose solve reads the
                    // bounds then gets the gap, by name (`BodyWorld::predicates`).
                    // A generic associated type (`type Out<T>: B;`): its own type and const
                    // parameters follow the trait's, in the alias's list and in scope for
                    // its bounds (gat.rs; the checker's `AssocGenerics`).
                    let mut own_ps = Vec::new_in(Global);
                    own_ps.extend_from_slice(&ps);
                    let mut own_cs = Vec::new_in(Global);
                    own_cs.extend_from_slice(&cs);
                    let of_type = match own_generics(&b, &mut self.tys, module, &mv, own + 1, &mut own_ps, &mut own_cs) {
                        Ok(n) => n,
                        Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                        Err(why) => {
                            self.preds.remove(&item);
                            self.fn_gaps.insert(item, why);
                            assoc.push(tc::AssocTyDef { name: Name::of(m.name), item, default });
                            continue;
                        }
                    };
                    // Its own const parameters: those of `own_cs` past the trait's (indices
                    // `own + 1 ..`), at their position among its own parameters.
                    let mut mine = Vec::new_in(Global);
                    for &(i, t) in own_cs.iter().filter(|(i, _)| *i > own) {
                        mine.push((i - own - 1, t));
                    }
                    assoc_consts.push((Name::of(m.name), mine));
                    let own_env = Env { params: &own_ps, consts: &own_cs, ..env };
                    let alias_args = params(&mut self.tys, 0, own + 1 + of_type, &own_cs)?;
                    let alias = intern(&mut self.tys, PNode::Alias { trait_: ck, name: Name::of(m.name), args: alias_args })?;
                    let mut ap = Preds::new();
                    match b.assoc_ty_bounds(&mut self.tys, &own_env, &mv, alias, self.sized, &mut ap) {
                        Ok(()) => {
                            self.fn_gaps.remove(&item);
                            self.preds.insert(item, ap);
                        }
                        Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                        Err(why) => {
                            self.preds.remove(&item);
                            self.fn_gaps.insert(item, why);
                        }
                    }
                    assoc.push(tc::AssocTyDef { name: Name::of(m.name), item, default });
                }
                // A trait's associated constant (the Reference, "Associated Items",
                // "Associated Constants": "An associated constant declaration declares a
                // signature for associated constant definitions. It is written as `const`,
                // then an identifier, then `:`, then a type, finished by a `;`"; "Associated
                // constants definitions define a constant associated with a type. It is
                // written the same as a constant item"): a row as an impl's constant has
                // ([`ImplHead::consts`]): no inputs, its type the output, generic over the
                // trait's parameters (`Self` = `Param(0)`), its clauses the trait's plus
                // `Self: Trait<params>`. `has_body` says whether a DEFAULT is written
                // (`const N: usize = 4;`), read from the text as the impl side reads it.
                ItemDetail::Const { .. } => {
                    let mk = keys.member(ck, MemberKind::Const, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    let at = Range::new(0, m.text.len() as u32);
                    let t = mv.head_opt(head::TY).ok_or(Why::Unsupported("an associated constant with no type", at))?;
                    consts.push((Name::of(m.name), mk));
                    let marks = super::item_site::member_marks(names, key, m.written_by, m.text).map_err(MarksLost::why)?;
                    let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
                    // As a member fn's signature ([`Store::member_fn_head`]): a need is passed
                    // up; any other refusal of the constant's type is filed in `fn_gaps` and
                    // the trait's head goes on (core's `SizedTypeProperties` has six
                    // constants: one unread type must not hide `SIZE`); a body that names the
                    // constant then gets the gap, by name.
                    let output = match b.ty(&mut self.tys, &env, &mv, lf::TypeId(t)) {
                        Ok(output) => output,
                        Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                        Err(why) => {
                            self.fn_gaps.insert(mk, why);
                            continue;
                        }
                    };
                    self.fn_gaps.remove(&mk);
                    let mut own_bounds = Vec::with_capacity_in(bounds.len(), Global);
                    own_bounds.extend_from_slice(&bounds);
                    let mut cp = Preds::new();
                    cp.clauses.extend_from_slice(&inherited);
                    self.preds.insert(mk, cp);
                    let header = tc::FnPtrHeader::RUST;
                    let has_body = mv.head[head::FLAGS] & sig::HAS_BODY != 0;
                    let row = FnHead { lib: key, text: m.text, container: Some(ck), parent: own + 1, own: 0, declared: 0, self_param: false, header, c_variadic: false, legacy_const_generics: Vec::new_in(Global), inputs: Vec::new_in(Global), output, has_body, module, params: ps.clone(), consts: cs.clone(), bounds: own_bounds, self_: env.self_, self_assoc: Vec::new_in(Global) };
                    self.fns.insert(mk, row);
                }
                _ => {}
            }
        }
        for (name, mine) in assoc_consts {
            self.maps.assoc_consts.insert((key, name), mine);
        }
        self.trait_defs.insert(ck, TraitExtra { supertraits, assoc, consts });
        self.preds.insert(ck, preds);
        Ok(())
    }

    #[expect(clippy::too_many_arguments, reason = "the item's parts, each read once by the caller")]
    fn adt<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>, detail: ItemDetail) -> Result<(), Why> {
        let ck = checker_of(key);
        adt_syntax(v)?;
        let mut ps = Vec::new_in(Global);
        let generics = param_names(v, 0, &mut ps, Consts::Keep)?;
        let cs = self.maps.const_params.get(&key).cloned().ok_or(Why::Need(key))?;
        let pl = params(&mut self.tys, 0, generics, &cs)?;
        let self_ty = intern(&mut self.tys, PNode::Adt(ck, pl))?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::Ty(self_ty, None), self_assoc: &[], anon: &[], consts: &cs };
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
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
            names.crate_of(key).ok_or(Why::Need(key))?;
            let lib_vs = names.variants(key);
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
            let name = names.name_of(key).unwrap_or("");
            let vk = keys.member(ck, MemberKind::Ctor, "").map_err(|c| Why::Program(format!("{c}").into_boxed_str(), Range::new(0, 0)))?;
            shapes.push((vk, Name::of(name), ctor, v.list(head::FIELDS)));
        }
        let mut variants = Vec::new_in(Global);
        let mut fields = Vec::new_in(Global);
        // Each field's default (RFC 3681), one per field, and the text of each written one:
        // filed as an anonymous constant's head below, once the ADT's clauses are made.
        let mut defaults: Vec<Option<ItemKey>, Global> = Vec::new_in(Global);
        let mut default_heads: Vec<(ItemKey, &'n str, PTy), Global> = Vec::new_in(Global);
        for (vi, &(vk, name, ctor, rows)) in shapes.iter().enumerate() {
            let start = fields.len() as u32;
            for (j, &n) in v.rows(rows)?.iter().enumerate() {
                let f = lf::Field::decode(n)?;
                let ty = b.ty(&mut self.tys, &env, v, f.ty)?;
                let fname = match f.name {
                    Some(n) => Name(v.name(n)?),
                    None => Name::of_index(j as u32),
                };
                fields.push(tc::Field { name: fname, ty });
                let Some(c) = f.default else {
                    defaults.push(None);
                    continue;
                };
                let at = v.const_range(c)?;
                let r = v.site(at)?;
                field_default_allowed(names, key, kind, ctor, r)?;
                let ty_text = v.text_at(v.type_range(f.ty)?)?.trim();
                let value_text = v.text_at(at)?.trim();
                // Keyed by the ADT, the field's place and the default's text (rule 21: the
                // text is the value's input; the ADT's generics are its head's).
                let dk = keys
                    .member(ck, MemberKind::Anon, &format!("field default {vi}.{j}: {ty_text} = {value_text}"))
                    .map_err(|c| Why::Program(format!("{c}").into_boxed_str(), r))?;
                let text: &'n str = self.locals.keep_text(&format!("const _: {ty_text} = {value_text};"));
                defaults.push(Some(dk));
                default_heads.push((dk, text, ty));
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
        // Each field default (RFC 3681): an anonymous constant of the field's type in a const
        // context, generic over the ADT's parameters with its clauses (it may name `N` or
        // `T`), evaluated at each use's ground arguments (the struct expression's type's).
        for &(dk, text, output) in &default_heads {
            let mut dp = Preds::new();
            dp.clauses.extend_from_slice(&preds.clauses);
            self.preds.insert(dk, dp);
            let mut own_bounds = Vec::with_capacity_in(bounds.len(), Global);
            own_bounds.extend_from_slice(&bounds);
            let row = FnHead {
                lib: key,
                text,
                container: None,
                parent: 0,
                own: generics,
                declared: 0,
                self_param: false,
                header: tc::FnPtrHeader::RUST,
                c_variadic: false,
                legacy_const_generics: Vec::new_in(Global),
                inputs: Vec::new_in(Global),
                output,
                has_body: true,
                module,
                params: ps.clone(),
                consts: cs.clone(),
                bounds: own_bounds,
                self_: SelfTy::Ty(self_ty, None),
                self_assoc: Vec::new_in(Global),
            };
            self.fns.insert(dk, row);
        }
        if defaults.iter().all(Option::is_none) {
            defaults.clear();
        }
        self.field_defaults.insert(ck, defaults);
        self.preds.insert(ck, preds);
        // Its `#[repr(..)]`, read by the reader the old front reads with, and whether a
        // `rustc_layout_scalar_valid_range_*` narrows its range (the program refuses to lay
        // out such a type until the back half's `valid_range` reads it).
        let mut repr = blaze_build::layout::ReprAttrs::RUST;
        let mut narrowed = false;
        let mut args = false;
        let attrs = v.list(head::ATTRS);
        for (i, &n) in v.rows(attrs)?.iter().enumerate() {
            let a = v.attr(n)?;
            let path = v.text_at(a.path)?.trim();
            if path == "repr" {
                let r = v.site(v.row_range(attrs.start + i as u32)?)?;
                crate::front::repr_parts(v.text_at(a.args)?, &mut repr).map_err(|what| Why::Unsupported(what, r))?;
            }
            narrowed |= path.starts_with("rustc_layout_scalar_valid_range");
            args |= is_args_attr(path);
        }
        let discrs = discriminants(&b, &mut self.tys, &env, v, repr, &written)?;
        self.adts.insert(ck, AdtRow { kind, generics, variants, fields, repr, narrowed, discrs, args });
        Ok(())
    }

    fn impl_<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
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
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        let generics = own_generics(&b, &mut self.tys, module, v, 0, &mut ps, &mut cs)?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &cs };
        // The self type FIRST, then the bounds with `Self` known: the Reference, "Paths",
        // "`Self`": "In an implementation, it refers to the type being implemented", the
        // impl's where clauses included (ruint's `impl<.., T> UintTryTo<T> for Uint<BITS,
        // LIMBS> where T: for<'a> TryFrom<&'a Self>`). A `T::X` in the self type is read
        // through the bounds of `T` as written in this header (`Bounds::Building`:
        // [`super::written`], by name), never through the lowered list, which needs `Self`.
        // The self type itself naming `Self` has nothing to refer to: refused by that name.
        let self_ty = b.ty(&mut self.tys, &env, v, lf::TypeId(v.head[head::SELF_TY])).map_err(|why| match why {
            Why::Unsupported(super::types::SELF_HERE, r) => Why::Unsupported("an impl's self type naming `Self`", r),
            other => other,
        })?;
        let env = Env { self_: SelfTy::Ty(self_ty, None), ..env };
        let mut bounds = Vec::new_in(Global);
        b.param_bounds(&mut self.tys, &env, v, 0, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
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
        // Each fn's `self` from its text ([`member_fn::takes_self`]): its signature is its own
        // want ([`Want::MemberFn`]), made when it is used, never with this head (brief 14: a
        // signature naming `Self::BYTES` of a sibling impl made every lookup of a name in the
        // type need that constant).
        let mut fn_self = Vec::new_in(Global);
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
                    fn_self.push(member_fn::takes_self(&mv)?);
                }
                ItemDetail::TypeAlias => {
                    let t = mv.head_opt(head::TY).ok_or(Why::Unsupported("an associated type with no type", whole))?;
                    // The member's text is read with its own tokens' marks (item_site.rs).
                    let marks = super::item_site::member_marks(names, key, m.written_by, m.text).map_err(MarksLost::why)?;
                    let mb = Builder { resolver: Paths { names }, heads: &self.maps, marks };
                    // `type Out<X> = ..;` (a generic associated type's value): its own type
                    // and const parameters are in scope after the impl's, at the indices
                    // `generics..` (the checker's `ImplAssoc`: "over the impl's generics then
                    // the item's own"; a lifetime is no parameter of a portable type). Its
                    // where clauses are read and refused or kept by gat.rs.
                    let value = if mv.rows(mv.list(head::GENERICS))?.is_empty() {
                        mb.ty(&mut self.tys, &env, &mv, lf::TypeId(t))?
                    } else {
                        let mut own_ps = Vec::new_in(Global);
                        own_ps.extend_from_slice(&ps);
                        let mut own_cs = Vec::new_in(Global);
                        own_cs.extend_from_slice(&cs);
                        own_generics(&mb, &mut self.tys, module, &mv, generics, &mut own_ps, &mut own_cs)?;
                        let own_env = Env { params: &own_ps, consts: &own_cs, ..env };
                        mb.ty(&mut self.tys, &own_env, &mv, lf::TypeId(t))?
                    };
                    super::gat::member_where(&mv, m.name)?;
                    assoc.push((Name::of(m.name), value));
                }
                ItemDetail::Const { .. } => {
                    let mk = keys.member(ck, MemberKind::Const, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    const_texts.push((Name::of(m.name), mk, m.text, m.written_by));
                }
                d => return Err(Why::Unsupported(crate::front::detail_name(d), whole)),
            }
        }
        let env = Env { self_assoc: &assoc, ..env };
        let mut preds = Preds::new();
        b.predicates(&mut self.tys, &env, v, 0, None, self.sized, &mut preds)?;
        let mut consts = Vec::with_capacity_in(const_texts.len(), Global);
        for &(name, mk, text, written_by) in &const_texts {
            let low = lower(names, key, text).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
            let cv = low.view();
            let at = Range::new(0, text.len() as u32);
            let t = cv.head_opt(head::TY).ok_or(Why::Unsupported("an associated constant with no type", at))?;
            let marks = super::item_site::member_marks(names, key, written_by, text).map_err(MarksLost::why)?;
            let cb = Builder { resolver: Paths { names }, heads: &self.maps, marks };
            let output = cb.ty(&mut self.tys, &env, &cv, lf::TypeId(t))?;
            let mut own_bounds = Vec::new_in(Global);
            own_bounds.extend_from_slice(&bounds);
            let mut self_assoc = Vec::with_capacity_in(assoc.len(), Global);
            self_assoc.extend_from_slice(&assoc);
            let header = tc::FnPtrHeader::RUST;
            let mut cp = Preds::new();
            cp.clauses.extend_from_slice(&preds.clauses);
            self.preds.insert(mk, cp);
            // An impl's constant has its value written (`pub const MAX: Self = !0;` inside
            // `impl usize`): its body is lowered by `lower_member` like a member fn's, which
            // is what the constant evaluator reads when another constant names it
            // (`usize::MAX >> 1`). Read from the text, never assumed.
            let has_body = cv.head[head::FLAGS] & sig::HAS_BODY != 0;
            let row = FnHead { lib: key, text, container: Some(ck), parent: generics, own: 0, declared: 0, self_param: false, header, c_variadic: false, legacy_const_generics: Vec::new_in(Global), inputs: Vec::new_in(Global), output, has_body, module, params: ps.clone(), consts: cs.clone(), bounds: own_bounds, self_: env.self_, self_assoc };
            self.fns.insert(mk, row);
            consts.push((name, mk));
        }
        // The frame its fns' signatures are made in, each by its own want (`Store::member_fn`).
        let mut frame_bounds = Vec::with_capacity_in(bounds.len(), Global);
        frame_bounds.extend_from_slice(&bounds);
        self.member_frames.insert(ck, member_fn::MemberFrame { module, generics, params: ps.clone(), consts: cs.clone(), bounds: frame_bounds, self_: env.self_ });
        self.preds.insert(ck, preds);
        self.impls.insert(ck, ImplHead { generics, self_ty, trait_, trait_args, assoc, fns, fn_self, consts, polarity });
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    /// A member fn's head: as [`Store::fn_head`], but a refusal other than a need is filed in
    /// `fn_gaps` and the container's head goes on (a need is passed up: the store makes the
    /// head it names and makes the container again).
    #[expect(clippy::too_many_arguments, reason = "the fn's parts and its container's")]
    fn member_fn_head<S: NamesSource<'n>>(&mut self, names: S, lib: LibKey, key: ItemKey, container: Option<ItemKey>, outer: &Env<'_>, parent: u32, inherited: &[PClause], v: &View<'_>, text: &'n str, marks: Option<TextMarks<'n>>) -> Result<(), Why> {
        match self.fn_head(names, lib, key, container, outer, parent, inherited, v, text, marks) {
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
    /// `inherited` its container's clauses, `v` its own text's, `marks` its text's tokens'
    /// (front2 item_site.rs).
    #[expect(clippy::too_many_arguments, reason = "the fn's parts and its container's")]
    fn fn_head<S: NamesSource<'n>>(&mut self, names: S, lib: LibKey, key: ItemKey, container: Option<ItemKey>, outer: &Env<'_>, parent: u32, inherited: &[PClause], v: &View<'_>, text: &'n str, marks: Option<TextMarks<'n>>) -> Result<(), Why> {
        let mut ps = Vec::new_in(Global);
        ps.extend_from_slice(outer.params);
        let mut cs = Vec::new_in(Global);
        cs.extend_from_slice(outer.consts);
        let declared = {
            let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
            own_generics(&b, &mut self.tys, outer.module, v, parent, &mut ps, &mut cs)?
        };
        // Each `impl Trait` parameter type is one more own type parameter, after the declared
        // ones; it has no name, so it is in `anon`, never in `ps`.
        let mut anon_at = Vec::new_in(Global);
        anon_params(v, parent + declared, &mut anon_at)?;
        let own = declared + anon_at.len() as u32;
        let mut anon: Vec<(lf::TypeId, PTy), Global> = Vec::with_capacity_in(anon_at.len(), Global);
        for &(t, i) in &anon_at {
            anon.push((t, intern(&mut self.tys, PNode::Param(i))?));
        }
        let params_end = anon.len();
        // Each `impl Trait` in the return type is an opaque type (the Reference, "Impl trait",
        // "Abstract return types": the caller sees only its bounds), keyed by the fn's key and
        // its index ([`super::keys::opaque_key`], [`super::types::return_impls`]'s order; an
        // `async fn`'s `impl Future` is index 0 and the written ones follow). It captures every
        // generic parameter in scope, the fn's and its container's, `impl Trait` parameters
        // too ("Capturing", 2024 edition: "all in-scope generic parameters"; in earlier
        // editions all type and const parameters and only the named lifetimes: lifetimes are
        // erased here, so both editions give the same list): its args are their identity.
        let flags = v.head[head::FLAGS];
        let is_async = flags & sig::ASYNC != 0;
        let mut ident = Vec::with_capacity_in((parent + own) as usize, Global);
        for i in 0..parent + own {
            let node = if cs.iter().any(|&(c, _)| c == i) { PNode::ConstParam(i) } else { PNode::Param(i) };
            ident.push(intern(&mut self.tys, node)?);
        }
        let ident = intern_list(&mut self.tys, &ident)?;
        // The return type as the Reference reads it, after macro expansion: one that writes a
        // type macro anywhere (`-> fut!(u8)`, `-> Option<it!(u8)>`, alloy's `-> impl_future!(<Output
        // = T>)`) is lowered again from its own text with every macro expanded
        // ([`super::type_expand`]) and its opaques are the expansion's, numbered in the same
        // preorder; one with no macro is read in place. An `async fn`'s written return type is
        // walked the same way (`-> eyre::Result<impl Provider<N>>`: opaque 1, after the future's 0).
        let written_ret = v.head_opt(head::TY).map(lf::TypeId);
        let mut rets = Vec::new_in(Global);
        let has_macro = match written_ret {
            Some(t) => super::types::return_impls(v, t, &mut rets)?,
            None => false,
        };
        let expanded = match written_ret {
            Some(t) if has_macro => {
                rets.clear();
                let r = v.site(v.type_range(t)?)?;
                let ret_text = v.text_at(v.type_range(t)?)?;
                let x = super::type_expand::expand_type_text(names, outer.module, ret_text, r, self.tys.alloc())?;
                x.impls(ret_text, r, &mut rets)?;
                Some(x)
            }
            _ => None,
        };
        // `rv` is the view the return type and its opaques' ids are in: the expansion's, or
        // the item's own. The return opaques are in their own list (`opaques`), never in
        // `anon` with the parameters' `impl`s: two views' type ids are not comparable.
        let (rv, ret) = match &expanded {
            Some(x) => (x.view(), Some(x.root)),
            None => (*v, written_ret),
        };
        let first = u32::from(is_async);
        let mut opaques: Vec<(lf::TypeId, PTy), Global> = Vec::with_capacity_in(rets.len(), Global);
        for (j, &t) in rets.iter().enumerate() {
            let k = super::keys::opaque_key(key, first + j as u32);
            opaques.push((t, intern(&mut self.tys, PNode::Opaque(k, ident))?));
        }
        let mut bounds = Vec::new_in(Global);
        let Bounds::Done(outer_bounds) = outer.bounds else {
            return Err(Why::Program("a fn's container's bounds were not lowered before the fn".into(), Range::new(0, 0)));
        };
        bounds.extend_from_slice(outer_bounds);
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        // The return type's builder: an expanded return type is a text of its own, read at the
        // fn's module; the item's marks are not its tokens'.
        let rb = Builder { resolver: Paths { names }, heads: &self.maps, marks: if expanded.is_some() { None } else { marks } };
        let env = Env { module: outer.module, params: &ps, bounds: Bounds::Building(&[]), self_: outer.self_, self_assoc: outer.self_assoc, anon: &anon, consts: &cs };
        b.param_bounds(&mut self.tys, &env, v, parent, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        // The return type's env: its opaques by their ids in `rv`; the parameters' `impl`s are
        // not in it (none is written in a return type).
        let ret_env = Env { anon: &opaques, ..env };
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
            // `...` (last, blaze-lower's grammar): no input; `c_variadic` below records it.
            if prm.variadic {
                continue;
            }
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
        let written = match ret {
            Some(t) => rb.ty(&mut self.tys, &ret_env, &rv, t)?,
            None => intern(&mut self.tys, PNode::Tuple(List::EMPTY))?,
        };
        let whole = Range::new(0, text.len() as u32);
        if flags & sig::GEN != 0 {
            return Err(Why::Unsupported("a `gen fn`", whole));
        }
        let c_variadic = flags & sig::VARIADIC != 0;
        if c_variadic && flags & sig::HAS_BODY != 0 {
            return Err(Why::Unsupported("a C-variadic fn definition (`unsafe extern \"C\" fn f(x: i32, mut args: ...)`: the unstable `c_variadic` feature)", whole));
        }
        // Each opaque's bounds as written, with the opaque type as `Self`, are its own
        // predicates (the checker's `Predicates(k)`); the written ones' bounds are made after
        // the fn's own bounds are known (a bound may name `T::Item`).
        for (j, &(t, me)) in opaques.iter().enumerate() {
            let k = super::keys::opaque_key(key, first + j as u32);
            let mut op = Preds::new();
            rb.impl_bounds(&mut self.tys, &ret_env, &rv, &[(t, me)], self.sized, &mut op)?;
            self.preds.insert(k, op);
            self.opaques.insert(k, (key, first + j as u32));
        }
        // An `async fn f(..) -> T` is `fn f(..) -> impl Future<Output = T>` (the Reference,
        // "Async functions": "async fn example(x: &str) -> usize { x.len() }" is "roughly
        // equivalent to" "fn example<'a>(x: &'a str) -> impl Future<Output = usize> + 'a"):
        // its output is opaque 0 with the bounds `Future`, `Output = T` and `Sized`. Its body
        // is an async block (a coroutine): the checker's and the code builder's, not the head's.
        let output = if is_async {
            let fut = super::langs::future_trait(Paths { names }, self.sized.ok_or(Why::Unsupported("an `async fn` in a program with no `Sized` (no core)", whole))?)
                .map_err(|e| Why::Program(format!("the `Future` trait of an `async fn`: {e}").into_boxed_str(), whole))?
                .ok_or(Why::Unsupported("an `async fn` in a program whose core has no `future::Future`", whole))?;
            let k = super::keys::opaque_key(key, 0);
            let me = intern(&mut self.tys, PNode::Opaque(k, ident))?;
            let args = intern_list(&mut self.tys, &[me])?;
            let mut op = Preds::new();
            op.clauses.push(PClause::Trait { trait_: checker_of(fut), args });
            op.clauses.push(PClause::Projection { trait_: checker_of(fut), args, name: Name::of("Output"), term: written });
            if let Some(s) = self.sized {
                op.clauses.push(PClause::Trait { trait_: checker_of(s), args });
            }
            self.preds.insert(k, op);
            self.opaques.insert(k, (key, 0));
            me
        } else {
            written
        };
        let header = tc::FnPtrHeader { abi: abi_of(v, whole)?, safety: if flags & sig::UNSAFE != 0 { tc::Safety::Unsafe } else { tc::Safety::Safe }, c_variadic: false };
        // A fn an `extern` block in a body declares ([`super::foreign_locals`]): its own text
        // carries no ABI, its block's does, and it "is implicitly unsafe unless the safe
        // function qualifier is present" (the Reference, "External blocks").
        let header = match self.locals.foreign(key) {
            Some(f) if f.kind == super::foreign_locals::ForeignKind::Fn => tc::FnPtrHeader { abi: super::types::fn_ptr_abi(f.abi, whole)?, safety: if f.safe { tc::Safety::Safe } else { tc::Safety::Unsafe }, c_variadic: false },
            // A fn a MODULE-LEVEL `extern` block declares: its block's ABI, and `unsafe` unless
            // written `safe` (super::foreign_head). Read as its own head before, it was a safe
            // Rust fn: aws-lc-rs's `EVP_aead_chacha20_poly1305` was no `unsafe extern "C" fn`
            // pointer (E0308) and libc's variadic `open` was refused as a `...` on a Rust fn.
            _ => super::foreign_head::module_foreign_header(names, lib, container.is_some(), flags, header, whole)?,
        };
        if c_variadic && !matches!(header.abi, tc::Abi::C | tc::Abi::CUnwind | tc::Abi::System | tc::Abi::SystemUnwind) {
            return Err(Why::Unsupported("`...` on a fn of an ABI other than \"C\" or \"system\" (the Reference, [items.extern.variadic.conventions])", whole));
        }
        let has_body = flags & sig::HAS_BODY != 0;
        let mut preds = Preds::new();
        preds.clauses.extend_from_slice(inherited);
        b.predicates(&mut self.tys, &env, v, parent, None, self.sized, &mut preds)?;
        b.impl_bounds(&mut self.tys, &env, v, &anon[..params_end], self.sized, &mut preds)?;
        self.preds.insert(key, preds);
        let mut self_assoc = Vec::with_capacity_in(env.self_assoc.len(), Global);
        self_assoc.extend_from_slice(env.self_assoc);
        let (module, self_) = (env.module, env.self_);
        // A free fn's const parameters are filed under its own key. A member's `lib` is its
        // CONTAINER's key (a member has no library key): filing there replaced the trait's or
        // impl's row with the container's plus this member's, so after `Iterator::next_chunk<const
        // N: usize>` the trait's index 1 read as a const parameter, and `sum<S>`'s `S` (index 1
        // too) was refused "a const parameter in a type position" when the trait's members
        // were made again. A member's own are in its `FnHead::consts`.
        if container.is_none() {
            self.maps.const_params.insert(lib, cs.clone());
        }
        // `#[rustc_legacy_const_generics(1)]` (stdarch's `vget_lane_u64(v, 0)`): the positions
        // of the call arguments that are the fn's own const generic arguments.
        let mut legacy_const_generics = Vec::new_in(Global);
        let attrs = v.list(head::ATTRS);
        for (i, &n) in v.rows(attrs)?.iter().enumerate() {
            let a = v.attr(n)?;
            if v.text_at(a.path)?.trim() == "rustc_legacy_const_generics" {
                let r = v.site(v.row_range(attrs.start + i as u32)?)?;
                legacy_positions(v.text_at(a.args)?, r, &mut legacy_const_generics)?;
            }
        }
        self.fns.insert(key, FnHead { lib, text, container, parent, own, declared, self_param, header, c_variadic, legacy_const_generics, inputs, output, has_body, module, params: ps, consts: cs, bounds, self_, self_assoc });
        Ok(())
    }
}
