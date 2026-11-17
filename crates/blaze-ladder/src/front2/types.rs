//! A written type (a signature's, a field's, an impl header's) to the checker's portable type,
//! every path through [`Paths::path`] (rule 30: no lookup rule here). A path to an ADT or a
//! trait whose head is not lowered yet stops with [`Why::Need`]: the driver lowers that head
//! and runs the item again (the pull-with-restart protocol; no recursion, README step 2).
//!
//! The only name rule here is the language's own: a one-segment type path the scopes do not
//! bind is a primitive type when it is one's name (`u8`, `str`), as Rust's primitive types are
//! read after every scope.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use blaze_lower::form as lf;
use blaze_lower::{BodyRange, TextAt, View};
use blaze_surface::Range;
use blaze_typeck as tc;
use blaze_typeck::{ItemKey, List, Name, PClause, PNode, PTy, PathIdx, PortableTys};

use super::source::NamesSource;
use crate::resolve::{Kind, Ns, Paths, ResolveError};

/// Why a written type has no portable type yet.
#[derive(Debug)]
pub enum Why {
    /// The head of this item (an ADT's generics, a trait's generics, defaults and associated
    /// types) is needed first: lower it, then run this again.
    Need(LibKey),
    /// A construct this build does not take yet, by its plain Rust name.
    Unsupported(&'static str, Range),
    /// The program is refused for what it says.
    Program(Box<str, Global>, Range),
    /// The names library could not resolve the path.
    Resolve(ResolveError, Range),
    /// A builtin type the names library resolved that the checker has no type for, by name
    /// (blaze's arbitrary-width integers, `i80`: blaze-typeck's `IntTy` has the standard
    /// widths only).
    NoCheckerType(Box<str, Global>, Range),
    /// `T::X` (the Reference, "Paths in types": `<T as Tr>::X` for the one trait among `T`'s
    /// bounds, supertraits included, that declares `X`): no bound declares it (the language's
    /// "associated type not found").
    AssocNotFound(Box<str, Global>, Range),
    /// `T::X` that more than one trait of `T`'s bounds declares (the language's ambiguity
    /// error), with those traits.
    AssocAmbiguous(Box<str, Global>, Vec<LibKey, Global>, Range),
    /// `dyn A + B` with two non-auto traits (the Reference, "Trait object types": "There may
    /// not be more than one non-auto trait"): both.
    TwoPrincipals(LibKey, LibKey, Range),
    /// A macro call in type position that is not core's built-in `pattern_type!`, by its path
    /// as written (type macros are expanded by the item layer, not here).
    TypeMacro(Box<str, Global>, Range),
    /// `pattern_type!(T is PAT)` refused by the checker lane's reader or builder, or its base
    /// text refused by `blaze_lower::lower_type_text`: what was said.
    PatternType(Box<str, Global>, Range),
    /// A pattern type's range end or an array length that is a constant expression other than
    /// a literal (`HALF_USIZE`, `u32::MAX`, `N * 2`): its value needs the constant evaluator,
    /// which has no world over front2's type builder yet.
    ConstPath(Box<str, Global>, Range),
    /// An array length that names the constant item `key` (`[u8; HELD]`): its value must be
    /// evaluated first ([`Heads::const_len`]); the driver that has the evaluator does so and
    /// tries the item again, as for [`Why::Need`].
    NeedConst(LibKey, Range),
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

fn program(message: std::string::String, r: Range) -> Why {
    Why::Program(message.into_boxed_str(), r)
}

/// The arguments of a struct's, enum's, union's or type alias's path `shown` taking `g`
/// parameters: the written `kids`, then each parameter not written takes its default with the
/// arguments before it put in (the Reference, "Generic parameters": `Vec<u8>` is
/// `Vec<u8, Global>`; "Type aliases": a type alias's parameters are generic parameters too).
/// The one home of default filling for a type path.
pub(super) fn with_defaults(tys: &mut PortableTys<'_>, kids: &[PTy], g: u32, defaults: &[Option<PTy>], shown: &str, r: Range) -> Result<Vec<PTy, Global>, Why> {
    if kids.len() > g as usize {
        return Err(program(format!("`{shown}` is written with {} type arguments; it takes {g}", kids.len()), r));
    }
    let mut args = Vec::with_capacity_in(g as usize, Global);
    args.extend_from_slice(kids);
    for i in kids.len()..g as usize {
        let Some(d) = defaults.get(i).copied().flatten() else {
            return Err(program(format!("`{shown}` is written with {} type arguments; it takes {g}, and parameter {i} has no default", kids.len()), r));
        };
        let d = subst(tys, d, &args)?;
        args.push(d);
    }
    Ok(args)
}

fn place(v: &View<'_>, r: Result<BodyRange, lf::BadRow>) -> Result<Range, Why> {
    Ok(v.site(r?)?)
}

/// An array length written as an integer literal (the Reference, "Integer literals": decimal,
/// `0x`, `0o`, `0b`, `_` separators, an optional suffix): its value; `None` when the text is
/// not a literal (a path or an expression). A suffix other than `usize` is the language's
/// mismatched-types error; a value past `usize` (64 bits here) its overflow error.
/// Owed: blaze-consteval's `lit::int` is the one home of this syntax; it is crate-private
/// (that crate is the coordinator's): this goes when it is exported.
fn usize_literal(text: &str, r: Range) -> Result<Option<u128>, Why> {
    let bytes = text.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_digit) {
        return Ok(None);
    }
    let (radix, digits): (u32, &[u8]) = match bytes {
        [b'0', b'x', rest @ ..] => (16, rest),
        [b'0', b'o', rest @ ..] => (8, rest),
        [b'0', b'b', rest @ ..] => (2, rest),
        _ => (10, bytes),
    };
    let mut value: u128 = 0;
    let mut end = digits.len();
    for (i, &b) in digits.iter().enumerate() {
        let d = match b {
            b'_' => continue,
            b'0'..=b'9' => u32::from(b - b'0'),
            b'a'..=b'f' if radix == 16 => u32::from(b - b'a') + 10,
            b'A'..=b'F' if radix == 16 => u32::from(b - b'A') + 10,
            // The suffix starts with a letter no digit of the radix is.
            b'i' | b'u' => {
                end = i;
                break;
            }
            _ => return Ok(None),
        };
        if d >= radix {
            return Ok(None);
        }
        value = value.checked_mul(u128::from(radix)).and_then(|v| v.checked_add(u128::from(d))).ok_or_else(|| program(format!("the array length `{text}` overflows `usize`"), r))?;
    }
    let suffix = core::str::from_utf8(&digits[end..]).unwrap_or("?");
    if !suffix.is_empty() && suffix != "usize" {
        return Err(program(format!("mismatched types: the array length `{text}` is a `{suffix}`, expected `usize`"), r));
    }
    if value > u128::from(u64::MAX) {
        return Err(program(format!("the array length `{text}` overflows `usize`"), r));
    }
    Ok(Some(value))
}

pub fn intern(tys: &mut PortableTys<'_>, n: PNode) -> Result<PTy, Why> {
    tys.intern(n).map_err(|_| program(format!("the type table is full"), Range::new(0, 0)))
}

pub fn intern_list(tys: &mut PortableTys<'_>, l: &[PTy]) -> Result<List, Why> {
    tys.intern_list(l).map_err(|_| program(format!("the type table is full"), Range::new(0, 0)))
}

/// The heads the type builder reads: an item's generics, a trait's defaults and associated
/// types, by library key; `None` when not lowered yet.
pub trait Heads {
    /// The value of the `usize` constant item `key` used as an array length, once evaluated.
    fn const_len(&self, key: LibKey) -> Option<u64>;
    /// The value of the evaluated constant `key` (a constant item, or an anonymous constant's
    /// need key) as bits in its own type's width; `None` until it is evaluated. By default
    /// what [`Heads::const_len`] holds.
    fn const_bits(&self, key: LibKey) -> Option<u128> {
        self.const_len(key).map(u128::from)
    }
    /// Takes the anonymous constant `ask` to be made a head and evaluated
    /// ([`super::anon`]); `false` where these heads have no evaluator behind them (the
    /// default), and the expression stays the head's refusal.
    fn ask_anon(&self, _ask: super::anon::Ask) -> bool {
        false
    }
    /// Takes the qualified constant leaf `ask` (`<B4 as Spec>::BITS` in a head's expression:
    /// [`super::const_qpath`]) to be selected and evaluated; `false` where these heads have
    /// no evaluator behind them (the default), and the expression is evaluated as written.
    fn ask_qpath(&self, _ask: super::const_qpath::QAsk) -> bool {
        false
    }
    /// The type of the answered qualified constant leaf under need key `need` (its bits are
    /// [`Heads::const_bits`]); `None` until it is answered.
    fn qpath_ty(&self, _need: LibKey) -> Option<PNode> {
        None
    }
    /// The inherent constant leaves asked of these heads (`Self::MAX_SIZE`:
    /// [`super::const_inherent`]); `None` where these heads have no evaluator behind them (the
    /// default), and the expression is read as written.
    fn inherent_leaves(&self) -> Option<&core::cell::RefCell<super::const_inherent::Table>> {
        None
    }
    /// Why the anonymous constant under need key `key` has no value (the evaluator's refusal,
    /// as printed), once it was tried: the head that wrote it is then refused, naming the
    /// expression and this reason, and only that head. `None`: not tried, or it has a value.
    fn anon_failed(&self, _key: LibKey) -> Option<&str> {
        None
    }
    /// The ADT's type parameter count and each one's default (over the ADT's own parameters).
    fn adt_generics(&self, adt: LibKey) -> Option<(u32, &[Option<PTy>])>;
    /// The trait's generics (`Self` included) and its defaults, after `Self`.
    fn trait_head(&self, trait_: LibKey) -> Option<(u32, &[Option<PTy>])>;
    /// Whether the trait declares an associated type `name`.
    fn trait_has_assoc(&self, trait_: LibKey, name: Name) -> Option<bool>;
    /// How many type and const parameters the trait's associated type `name` declares on
    /// itself (`type MapStorage<V>` has one; a lifetime is none): `None` when that is not on
    /// record (the default: a source of heads that keeps no such count).
    fn assoc_generics(&self, trait_: LibKey, name: Name) -> Option<u32> {
        let _ = (trait_, name);
        None
    }
    /// The const parameters of the trait's associated type `name` among its OWN type and
    /// const parameters (position counted from 0 after the trait's, lifetimes not counted;
    /// declared type), filed with the trait's members: `None` when not on record (the
    /// default: a source of heads that keeps none; or the members not made yet).
    fn assoc_const_params(&self, trait_: LibKey, name: Name) -> Option<&[(u32, PTy)]> {
        let _ = (trait_, name);
        None
    }
    /// The trait's supertraits written on its header (`: FnMut<Args>`), each with its
    /// arguments (`Self` first) over the trait's own parameters (`Self` = `Param(0)`).
    fn trait_supers(&self, trait_: LibKey) -> Option<&[(LibKey, List)]>;
    /// Whether the trait is an `auto trait` (from its own head).
    fn trait_auto(&self, trait_: LibKey) -> Option<bool>;
    /// A type alias's generic count and its aliased type over them (`Param(i)` the alias's
    /// i-th parameter): a path to it means the aliased type with its arguments substituted.
    /// `Some(None)`: its head was tried and refused (the why is in the store's `alias_gaps`).
    fn alias(&self, alias: LibKey) -> Option<Option<(u32, PTy)>>;
    /// A type alias's parameter defaults, made with its head (`None` where none is written;
    /// empty before its head is made).
    fn alias_defaults(&self, alias: LibKey) -> &[Option<PTy>];
    /// Why a type alias's head was refused (`Some(None)` from [`Heads::alias`]).
    fn alias_gap(&self, alias: LibKey) -> Option<&Why>;
    /// A trait alias's head (`trait A<T> = B<T> + C where ..;`): what a bound naming it stands
    /// for. `None` when not lowered yet.
    fn trait_alias(&self, alias: LibKey) -> Option<&TraitAliasHead>;
    /// The const generic parameters of the struct, enum, union, trait or type alias `item`
    /// (index, declared type), sorted by index; `None` when its head is not lowered yet.
    fn const_params(&self, item: LibKey) -> Option<&[(u32, PTy)]>;
    /// The checker key of a library key (filed in the key map at the start).
    fn checker(&self, key: LibKey) -> ItemKey;
    /// How many lowerings of a written bound's arguments are nested right now
    /// ([`super::written`], which bounds it: a bound read to find the trait of a path written
    /// in itself is a cycle of the program, refused by name, never a stack that runs out).
    /// `None` (the default): these heads count none, and such a path is refused by name.
    fn written_depth(&self) -> Option<&core::cell::Cell<u32>> {
        None
    }
}

/// A trait alias's head. The unstable book, `trait_alias`: "Trait aliases ... `trait Foo = Bar
/// + Baz;` ... used as a bound is equivalent to" its right-hand side; the pinned core's
/// `pub trait Thin = Pointee<Metadata = ()> + PointeeSized;` (library/core/src/ptr/metadata.rs).
/// Its clauses are over `Self` = `Param(0)` and its own parameters after it; an alias named in
/// them is already expanded (its head was made first), so a use substitutes once.
#[derive(Clone, Debug)]
pub struct TraitAliasHead {
    /// Its generics, `Self` included.
    pub generics: u32,
    /// Its bounds on `Self` and its where clauses, as clauses (trait clauses and the
    /// projections of `Name = T`); no implied `Sized` on its own parameters.
    pub clauses: Vec<PClause, Global>,
    /// Its trait clauses on `Self` by library key (what `T::X` is looked up through, as a
    /// [`ParamBound`]), with their arguments over the same parameters.
    pub traits: Vec<(LibKey, List), Global>,
}

/// The refusal of a `Self` written where the [`Env`] has no `Self` ([`SelfTy::None`]): one
/// name, so that heads.rs `impl_` can say which place it was (an impl's own self type).
pub(super) const SELF_HERE: &str = "`Self` here";

/// What `Self` is where the type is written.
#[derive(Clone, Copy, Debug)]
pub enum SelfTy {
    None,
    /// In a trait: `Self` is `Param(0)`.
    Trait(LibKey),
    /// In an impl (or an ADT's own fields): its self type, and for a trait impl once its trait
    /// is made, that trait and its arguments (`Self` first): `Self::X` not declared by the impl
    /// is `<Self as Tr>::X` for the one trait among it and its supertraits that declares `X`.
    Ty(PTy, Option<(LibKey, List)>),
}

/// A trait bound on a type parameter in scope (`T: Tr<A>`, written on the parameter or as
/// `where T: Tr<A>`): what `T::X` is looked up through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamBound {
    pub param: u32,
    pub trait_: LibKey,
    /// The trait's arguments, `Self` (the parameter) first.
    pub args: List,
}

/// The parameters' trait bounds in scope.
#[derive(Clone, Copy, Debug)]
pub enum Bounds<'e> {
    /// The bounds themselves are being lowered: these are the ones lowered so far, so a
    /// `T::X` not found among them may be declared by a later one (a typed refusal, never
    /// the language's "not found").
    Building(&'e [ParamBound]),
    /// Every bound in scope.
    Done(&'e [ParamBound]),
}

/// Where a type is written: its module, the generic parameters in scope (name, index) and
/// their trait bounds, and `Self`, with an impl's associated types by name.
#[derive(Clone, Copy)]
pub struct Env<'e> {
    pub module: LibKey,
    pub params: &'e [(Name, u32)],
    pub bounds: Bounds<'e>,
    pub self_: SelfTy,
    pub self_assoc: &'e [(Name, PTy)],
    /// A fn's `impl Trait` types and what each stands for: in a parameter's type an anonymous
    /// type parameter `Param(i)` (the Reference, "Impl trait", "Anonymous type parameters"),
    /// in the return type an opaque type `Opaque(key, generics in scope)` ("Abstract return
    /// types"; [`return_impls`]). An `impl Trait` type not listed here is refused by name.
    pub anon: &'e [(lf::TypeId, PTy)],
    /// The const generic parameters in scope (the Reference, "Generic parameters", "Const
    /// generics"): each one's index (among `params`, which names it) and its declared type.
    /// A name in `params` whose index is here is a constant, never a type.
    pub consts: &'e [(u32, PTy)],
}

/// The checker's node for each built-in type the names library names (`Kind::Builtin(name)`,
/// from `blaze_scope::builtin_of`): a map from the library's answer, never a lookup of a
/// written name.
pub(super) const PRIMS: [(&str, PNode); 19] = [
    ("f16", PNode::Float(tc::FloatTy::F16)),
    ("f128", PNode::Float(tc::FloatTy::F128)),
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

/// A path's segments as the resolver takes them: their texts, and whether it starts at `::`.
/// A qualified (`<T as Tr>::X`) or parenthesized segment is not a plain path.
pub fn path_texts<'v>(v: &View<'v>, p: PathIdx) -> Result<(bool, Vec<&'v str, Global>, Vec<lf::Segment, Global>, Range), Why> {
    path_texts_of(v, p, false)
}

/// [`path_texts`]; `sugar`: the caller reads `Fn(A) -> B` on the last segment itself.
pub fn path_texts_of<'v>(v: &View<'v>, p: PathIdx, sugar: bool) -> Result<(bool, Vec<&'v str, Global>, Vec<lf::Segment, Global>, Range), Why> {
    let r = place(v, v.path_range(p))?;
    let path = v.path(p)?;
    let mut texts = Vec::new_in(Global);
    let mut segs = Vec::new_in(Global);
    let all = v.segments(path)?;
    for (i, &n) in all.iter().enumerate() {
        let s = lf::Segment::decode(n)?;
        let read = sugar && i + 1 == all.len() && !s.return_notation;
        if (s.parenthesized && !read) || s.return_notation {
            return Err(Why::Unsupported("parenthesized generic arguments (`Fn(A) -> B`)", r));
        }
        let text = match (s.kind, s.name) {
            (lf::SegKind::Ident, Some(n)) => v.name_text(n)?,
            (lf::SegKind::SelfMod, _) => "self",
            (lf::SegKind::Super, _) => "super",
            (lf::SegKind::Crate, _) => "crate",
            (lf::SegKind::SelfTy, _) => "Self",
            (lf::SegKind::DollarCrate, _) => return Err(Why::Unsupported("`$crate`", r)),
            (lf::SegKind::Anchor, _) | (lf::SegKind::Ident, None) => "<qualified>",
        };
        texts.push(text);
        segs.push(s);
    }
    Ok((path.absolute, texts, segs, r))
}

/// The type ids of a segment's generic arguments that are surely types, in order; a lifetime
/// is passed over. A const argument (`{ N + 1 }`, `3`) and an argument that is one plain
/// name (`N`: a type or a constant by what the name resolves to) are made by
/// [`Builder::path_args`] once the scope is known, so they are no child here.
fn arg_types(v: &View<'_>, s: &lf::Segment, r: Range, out: &mut Vec<lf::TypeId, Global>) -> Result<(), Why> {
    if !s.has_args {
        return Ok(());
    }
    for &n in v.rows(s.args)? {
        match lf::GenericArg::decode(n)? {
            lf::GenericArg::Type(t) if bare_name(v, t)?.is_some() => {}
            lf::GenericArg::Type(t) => out.push(t),
            lf::GenericArg::Lifetime(_) | lf::GenericArg::AssocType { .. } | lf::GenericArg::Const(_) => {}
            _ => return Err(Why::Unsupported("an associated item constraint", r)),
        }
    }
    Ok(())
}

/// How many of a segment's generic arguments are types or constants (a lifetime is erased; an
/// associated item constraint is no argument): how many entries the segment has in
/// [`Builder::path_args`]' answer.
fn written_type_args(v: &View<'_>, s: &lf::Segment) -> Result<usize, Why> {
    if !s.has_args {
        return Ok(0);
    }
    let mut n = 0usize;
    for &row in v.rows(s.args)? {
        if matches!(lf::GenericArg::decode(row)?, lf::GenericArg::Type(_) | lf::GenericArg::Const(_)) {
            n += 1;
        }
    }
    Ok(n)
}

/// The name of a type argument written as one plain identifier with no arguments (`N`, `T`,
/// `u8`): the Reference, "Generic parameters": such an argument "is resolved as a type if it
/// resolves to a type in the type namespace, and as a const otherwise".
fn bare_name<'v>(v: &View<'v>, t: lf::TypeId) -> Result<Option<&'v str>, Why> {
    let lf::TypeRef::Path(p) = v.ty(t)? else { return Ok(None) };
    let path = v.path(p)?;
    let [n] = v.segments(path)? else { return Ok(None) };
    let s = lf::Segment::decode(*n)?;
    if path.absolute || s.has_args || s.parenthesized || s.kind != lf::SegKind::Ident {
        return Ok(None);
    }
    match s.name {
        Some(name) => Ok(Some(v.name_text(name)?)),
        None => Ok(None),
    }
}

/// A const argument's value as the checker holds it (`PNode::ConstValue`), from the literal
/// `text` of type `want` (the Reference, "Literal expressions"): `true`, `false`, or an integer
/// literal, `-` before it for a signed type; its bits are the value's in `want`'s width, as
/// blaze-consteval writes them. `None`: the text is no such literal.
fn literal_value(tys: &mut PortableTys<'_>, text: &str, want: PTy, r: Range) -> Result<Option<PTy>, Why> {
    let node = *tys.view().get(want).ok_or(program(format!("a const parameter's type is past the type table"), r))?;
    let signed = match node {
        PNode::Bool => {
            let b = match text {
                "true" => 1,
                "false" => 0,
                _ => return Ok(None),
            };
            return Ok(Some(intern(tys, PNode::ConstValue { ty: want, bits: b })?));
        }
        PNode::Int(_) => true,
        PNode::Uint(_) => false,
        _ => return Ok(None),
    };
    let Some(&(name, _)) = PRIMS.iter().find(|(_, n)| *n == node) else { return Ok(None) };
    let bits = int_bits(name);
    let (neg, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text),
    };
    let Some((value, suffix)) = int_literal(digits) else { return Ok(None) };
    if !suffix.is_empty() && suffix != name {
        return Err(program(format!("mismatched types: the const argument `{text}` is a `{suffix}`, expected `{name}`"), r));
    }
    if neg && !signed {
        return Err(program(format!("the const argument `{text}` is negative; its type `{name}` is unsigned"), r));
    }
    let mask = if bits == 128 { u128::MAX } else { (1u128 << bits) - 1 };
    let most = if signed { mask >> 1 } else { mask };
    let fits = value <= most || (neg && value == most + 1);
    if !fits {
        return Err(program(format!("the const argument `{text}` overflows `{name}`"), r));
    }
    let bits = if neg { value.wrapping_neg() & mask } else { value };
    Ok(Some(intern(tys, PNode::ConstValue { ty: want, bits })?))
}

/// An integer type's width by its name; `isize` and `usize` are 64 bits here, as
/// [`usize_literal`] takes them.
fn int_bits(name: &str) -> u32 {
    match name {
        "i8" | "u8" => 8,
        "i16" | "u16" => 16,
        "i32" | "u32" => 32,
        "i128" | "u128" => 128,
        _ => 64,
    }
}

/// An unsigned integer literal's value and its suffix (the Reference, "Integer literals"):
/// `None` when `text` is no integer literal or its value passes `u128`.
fn int_literal(text: &str) -> Option<(u128, &str)> {
    let bytes = text.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let (radix, start): (u32, usize) = match bytes {
        [b'0', b'x', ..] => (16, 2),
        [b'0', b'o', ..] => (8, 2),
        [b'0', b'b', ..] => (2, 2),
        _ => (10, 0),
    };
    let mut value: u128 = 0;
    let mut end = bytes.len();
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        let d = match b {
            b'_' => continue,
            b'0'..=b'9' => u32::from(b - b'0'),
            b'a'..=b'f' if radix == 16 => u32::from(b - b'a') + 10,
            b'A'..=b'F' if radix == 16 => u32::from(b - b'A') + 10,
            b'i' | b'u' => {
                end = i;
                break;
            }
            _ => return None,
        };
        if d >= radix {
            return None;
        }
        value = value.checked_mul(u128::from(radix))?.checked_add(u128::from(d))?;
    }
    Some((value, &text[end..]))
}

/// The type builder of one item: its resolver, over the names source `S`
/// ([`NamesSource`]: the solved names or each crate's records), and the heads lowered so far.
pub struct Builder<'b, H: Heads, S> {
    pub resolver: Paths<S>,
    pub heads: &'b H,
}

impl<'b, 'n, H: Heads, S: NamesSource<'n>> Builder<'b, H, S> {
    /// The portable type of `t`, by a work list (no recursion).
    pub fn ty(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, t: lf::TypeId) -> Result<PTy, Why> {
        enum S {
            Visit(lf::TypeId),
            Make(lf::TypeId),
        }
        let mut todo = Vec::new_in(Global);
        let mut done: Vec<PTy, Global> = Vec::new_in(Global);
        let mut kids: Vec<lf::TypeId, Global> = Vec::new_in(Global);
        todo.push(S::Visit(t));
        while let Some(step) = todo.pop() {
            kids.clear();
            match step {
                S::Visit(t) => {
                    todo.push(S::Make(t));
                    children(v, t, &mut kids)?;
                    todo.extend(kids.iter().rev().map(|&k| S::Visit(k)));
                }
                S::Make(t) => {
                    children(v, t, &mut kids)?;
                    let start = done.len() - kids.len();
                    let made = self.make(tys, env, v, t, &done[start..])?;
                    done.truncate(start);
                    done.push(made);
                }
            }
        }
        done.pop().ok_or(program(format!("an empty type"), Range::new(0, 0)))
    }

    /// An array length written as a plain path (`HELD`, `crate::out::HELD`): the value of the
    /// constant item the names library resolves it to in the value namespace, once evaluated
    /// ([`Why::NeedConst`] until then). Anything else (a type-relative `u32::MAX`, a const
    /// parameter, an expression) is [`Why::ConstPath`] naming the text.
    /// An array length's value from its written text (not `_`): an integer literal, or a
    /// constant's value at `usize` ([`Builder::end_value`]).
    pub(super) fn array_len(&self, env: &Env<'_>, text: &str, r: Range) -> Result<u128, Why> {
        self.end_value(env, text, r, Some(PNode::Uint(tc::UintTy::Usize)))
    }

    /// Whether `text` is one token (a literal with its suffix, a name): an expression
    /// (`1usize << 0`) is never read as a literal with a wrong suffix.
    fn one_token(text: &str) -> bool {
        !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }

    /// The value of a constant written where a head needs an unsigned value of type `ty` (an
    /// array length, at `usize`; a pattern type's range end, at its base type): an integer
    /// literal, or [`Builder::const_value_bits`]. `ty` is `None` where the type is not known:
    /// then only a literal or a constant item's path has a value.
    pub(super) fn end_value(&self, env: &Env<'_>, text: &str, r: Range, ty: Option<PNode>) -> Result<u128, Why> {
        let text = text.trim();
        let literal = if Self::one_token(text) { usize_literal(text, r)? } else { None };
        match literal {
            Some(bits) => Ok(bits),
            None => {
                let bits = self.const_value_bits(env, text, r, ty)?;
                if ty == Some(PNode::Uint(tc::UintTy::Usize)) && bits > u128::from(u64::MAX) {
                    return Err(program(format!("the constant `{text}` is past `usize` (64 bits here)"), r));
                }
                Ok(bits)
            }
        }
    }

    /// The value of the constant expression `text` of type `ty`, as bits in `ty`'s width (the
    /// Reference, "Constant evaluation", "Const context"). A plain path the names library
    /// resolves to a constant item is that item's value ([`Heads::const_bits`];
    /// [`Why::NeedConst`] naming it until the driver's `eval_len` has evaluated it). Any other
    /// expression (`u32::MAX`, `1 << 0`, `A + 1`, `size_of::<u64>()`, `<B4 as
    /// Specifier>::BITS + 1`) is an ANONYMOUS constant ([`super::anon`]): asked of the heads
    /// ([`Heads::ask_anon`]) and needed under its own key the same way, evaluated by the same
    /// machine as a constant item's body. Not evaluated at the head, and refused by name:
    /// an expression naming a generic parameter in scope or `Self` ("a constant expression
    /// over a generic parameter": blaze-typeck's type form has no node for an unevaluated
    /// expression), and one whose type is unknown or is not `bool`, `char` or an integer
    /// ([`Why::ConstPath`] naming the text).
    fn const_value_bits(&self, env: &Env<'_>, text: &str, r: Range, ty: Option<PNode>) -> Result<u128, Why> {
        let text = text.trim();
        let other = || Why::ConstPath(Box::from(text), r);
        let (abs, rest) = match text.strip_prefix("::") {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let mut segs: Vec<&str, Global> = Vec::new_in(Global);
        let mut plain = true;
        for s in rest.split("::") {
            let s = s.trim();
            let ident = s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && s.chars().all(|c| c.is_alphanumeric() || c == '_');
            if !ident {
                plain = false;
                break;
            }
            segs.push(s);
        }
        if super::anon::mentions(text, env.params) {
            return Err(Why::Unsupported("a constant expression over a generic parameter", r));
        }
        // Each qualified leaf `<T as Trait>::NAME` is answered by selection first
        // ([`super::const_qpath`]) and the expression is evaluated with its value in its
        // place: a checked body's constant path holds no `Self` to select an impl by. A leaf
        // refused there refuses this head, naming the leaf and the reason.
        let resolve_ty = |abs: bool, segs: &[&str]| self.resolver.path(env.module, abs, segs, Ns::Type);
        let with_values = match super::const_qpath::substitute(self.heads, &resolve_ty, text, blaze_own::Alloc::global()) {
            Ok(done) => done,
            Err(super::const_qpath::Stop::Need(need)) => return Err(Why::NeedConst(need, r)),
            Err(super::const_qpath::Stop::Failed { start, end, why }) => {
                let leaf = text.get(start..end).unwrap_or(text);
                return Err(Why::ConstPath(format!("{leaf}: {why}").into_boxed_str(), r));
            }
        };
        let text = match &with_values {
            Some(done) => done.as_str(),
            None => text,
        };
        if plain {
            let found = self.resolver.path(env.module, abs, &segs, Ns::Value).map_err(|e| Why::Resolve(e, r))?;
            if found.kind == Kind::Const && found.consumed as usize == segs.len() {
                return self.heads.const_bits(found.key).ok_or(Why::NeedConst(found.key, r));
            }
        }
        let Some(ask) = ty.and_then(|ty| super::anon::Ask::of(env.module, ty, text)) else { return Err(other()) };
        if let Some(bits) = self.heads.const_bits(ask.need) {
            return Ok(bits);
        }
        // Tried and refused by the evaluator (`size_of::<u8>()` where a call it needs does not
        // check): the head is refused by the expression's name with the evaluator's reason, as
        // an expression was before there was an evaluator behind the heads; never asked again.
        if let Some(why) = self.heads.anon_failed(ask.need) {
            return Err(Why::ConstPath(format!("{text}: {why}").into_boxed_str(), r));
        }
        let need = ask.need;
        if self.heads.ask_anon(ask) { Err(Why::NeedConst(need, r)) } else { Err(other()) }
    }

    /// A constant written as a generic argument, an array length or a const parameter's
    /// default (the Reference, "Generic parameters", "Const generics"): a block `{ e }` is read
    /// as `e`; `_` is a hole; a const parameter in scope is `ConstParam`; a literal or a
    /// constant item's value is a `ConstValue` of the parameter's type `want()`. Any other
    /// expression (`N + 1`, `size_of::<T>()`) needs the constant evaluator to run over a
    /// generic expression: [`Why::ConstPath`] naming its text.
    pub(super) fn const_term(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, text: &str, r: Range, want: &dyn Fn(&mut PortableTys<'_>) -> Result<PTy, Why>) -> Result<PTy, Why> {
        let mut text = text.trim();
        if let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
            text = inner.trim();
        }
        if text == "_" {
            return intern(tys, PNode::Infer);
        }
        if let Some(&(_, i)) = env.params.iter().find(|(n, _)| *n == Name::of(text)) {
            if env.consts.iter().any(|(c, _)| *c == i) {
                return intern(tys, PNode::ConstParam(i));
            }
            return Err(program(format!("`{text}` is a type parameter; a constant is expected here"), r));
        }
        let ty = want(tys)?;
        // A parameter whose type has no value node (a reference, tuple, array or generic ADT:
        // `R<{ "k" }, T>`) takes no value here: refused by name, never read as bits (a
        // constant item's evaluated bits of a `&str` are an address, not its value).
        if !super::const_param_ty::value_has_node(tys.view().get(ty)) {
            return Err(Why::Unsupported(super::const_param_ty::NO_VALUE_NODE, r));
        }
        // A parameter of a fieldless enum type takes a path to a unit variant, whose value is
        // that variant under the enum's key (const_enum.rs); nothing below reads an enum.
        match super::const_enum::term(tys, &|abs, segs| self.resolver.path(env.module, abs, segs, Ns::Value), ty, text, r)? {
            super::const_enum::Term::Is(value) => return Ok(value),
            super::const_enum::Term::OtherEnum(_) => {
                return Err(program(format!("mismatched types: the const argument `{text}` is a variant of another enum than its parameter's type"), r));
            }
            super::const_enum::Term::NotEnum => {}
        }
        // An inherent associated constant leaf (`Self::MAX_SIZE`, `InlineExtension::MAX`,
        // `<W>::N`) is the impl member constant's value, evaluated by its own key
        // (front2/const_inherent.rs), never through the type's row, whose fields may hold this
        // very length; the expression goes on with each leaf's value in its place.
        let inherent_resolve = |abs: bool, segs: &[&str]| self.resolver.path(env.module, abs, segs, Ns::Type);
        let self_adt = super::const_inherent::ground_self_adt(tys.view(), env);
        // The substituted text is kept in the allocator of the type table this call holds.
        let substituted = match super::const_inherent::substitute(self.heads, &inherent_resolve, self_adt, text, tys.alloc()) {
            Ok(done) => done,
            Err(super::const_inherent::Stop::Need(need)) => return Err(Why::NeedConst(need, r)),
            Err(super::const_inherent::Stop::Failed { start, end, need }) => {
                let leaf = text.get(start..end).unwrap_or(text);
                // The typed refusal, printed into the head's `Why` as every head refusal is.
                let shown = match self.heads.inherent_leaves() {
                    Some(table) => super::const_inherent::refusal(table, need, |why| match why {
                        Some(why) => format!("{leaf}: {why}"),
                        None => format!("{leaf}: refused, and its refusal is not in the table"),
                    }),
                    None => format!("{leaf}: refused where these heads keep no inherent leaves"),
                };
                return Err(Why::ConstPath(shown.into_boxed_str(), r));
            }
            Err(super::const_inherent::Stop::Cycle { start, end }) => {
                let leaf = text.get(start..end).unwrap_or(text);
                return Err(Why::ConstPath(format!("{leaf}: the inherent constant is needed while it is being evaluated (a constant that needs itself)").into_boxed_str(), r));
            }
            Err(super::const_inherent::Stop::Full) => {
                return Err(Why::Unsupported("more inherent constant leaves than front2/const_inherent.rs keeps (rule 18)", r));
            }
        };
        if let Some(done) = &substituted {
            text = done.as_str();
        }
        // A literal is one token, `-` before it for a signed one: `1usize << 0` is an
        // expression, never a literal with the suffix `usize << 0`.
        let digits = text.strip_prefix('-').map_or(text, str::trim_start);
        if Self::one_token(digits) {
            if let Some(value) = literal_value(tys, text, ty, r)? {
                return Ok(value);
            }
        }
        // Any other constant: a constant item's value or an anonymous constant's, in `ty`'s
        // width (`ty`'s node says how the anonymous one is typed; an unknown type, a body's
        // `_`, takes only a constant item's path).
        let node = tys.view().get(ty).copied();
        // An expression that names `Self` where `Self` is a ground type has a value here
        // (`[u8; size_of::<Self>()]` in `impl u16`: front2/const_open.rs, state one): its
        // anonymous constant is filed with that `Self` in scope. Every other expression is
        // read as before; one over a generic parameter is refused there by name.
        let bits = match super::const_open::ground_self(tys.view(), env, text) {
            Some((self_ty, self_node)) => super::anon::self_value(self.heads, env.module, node, text, self_ty, self_node, r)?,
            None => self.const_value_bits(env, text, r, node)?,
        };
        intern(tys, PNode::ConstValue { ty, bits })
    }

    /// The declared type of the const parameter that argument `index` of the path `texts`
    /// fills (its item's head, [`Heads::const_params`]; a trait's arguments follow `Self`).
    fn const_want(&self, env: &Env<'_>, abs: bool, texts: &[&str], index: u32, r: Range) -> Result<PTy, Why> {
        let found = self.resolver.path(env.module, abs, texts, Ns::Type).map_err(|e| Why::Resolve(e, r))?;
        if found.consumed as usize != texts.len() {
            return Err(Why::Unsupported("a const argument on an associated item's path", r));
        }
        let at = if found.kind == Kind::Trait { index + 1 } else { index };
        let consts = self.heads.const_params(found.key).ok_or(Why::Need(found.key))?;
        match consts.iter().find(|(i, _)| *i == at) {
            Some(&(_, t)) => Ok(t),
            None => Err(program(format!("argument {} of `{}` is a constant; that parameter is not a const parameter", index + 1, texts.join("::")), r)),
        }
    }

    /// A path's generic arguments in written order: each type argument from `kids` (as
    /// [`children`] listed them: a qualified path's self type first), each const argument
    /// and each one-name argument (`N`: a type if the name is a type parameter or resolves in
    /// the type namespace, else a constant; the Reference, "Generic parameters") made here.
    pub(super) fn path_args(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, p: PathIdx, kids: &[PTy]) -> Result<Vec<PTy, Global>, Why> {
        let r = place(v, v.path_range(p))?;
        let path = v.path(p)?;
        let mut out = Vec::with_capacity_in(kids.len(), Global);
        let mut next = kids.iter().copied();
        // Each segment whose arguments are listed, with the path its arguments are for.
        let mut work: Vec<(lf::Segment, bool, Vec<&str, Global>), Global> = Vec::new_in(Global);
        let (abs, texts, segs, _) = path_texts_of(v, p, true)?;
        for (k, s) in segs.iter().enumerate() {
            if s.kind == lf::SegKind::Anchor {
                out.push(next.next().ok_or(program(format!("a qualified path's self type is missing"), r))?);
                let Some(tr) = s.trait_ else { return Err(Why::Unsupported("a qualified path with no trait (`<T>::X`)", r)) };
                let lf::TypeRef::Path(tp) = v.ty(tr)? else { return Err(program(format!("a trait that is no path"), r)) };
                let (tabs, ttexts, tsegs, _) = path_texts(v, tp)?;
                for ts in tsegs {
                    work.push((ts, tabs, ttexts.clone()));
                }
            } else {
                let mut upto = Vec::new_in(Global);
                upto.extend_from_slice(&texts[..=k]);
                work.push((*s, abs, upto));
            }
            // Arguments are lowered segment by segment, in the order `children` lists them.
            for (s, sabs, stexts) in work.drain(..) {
                if !s.has_args {
                    continue;
                }
                let mut index = 0u32;
                for &n in v.rows(s.args)? {
                    let made = match lf::GenericArg::decode(n)? {
                        lf::GenericArg::Type(t) => match bare_name(v, t)? {
                            None => next.next().ok_or(program(format!("a type argument is missing"), r))?,
                            Some(name) => {
                                let param = env.params.iter().find(|(n, _)| *n == Name::of(name)).map(|&(_, i)| i);
                                let is_const = param.is_some_and(|i| env.consts.iter().any(|(c, _)| *c == i));
                                // A type if a type parameter, or the names library's answer in
                                // the type namespace is a type (a constant item answers there
                                // too, as the library files it: then it is a constant).
                                let names_a_type = || match self.resolver.path(env.module, false, &[name], Ns::Type) {
                                    Ok(f) => f.consumed == 1 && matches!(f.kind, Kind::Struct | Kind::Enum | Kind::Union | Kind::TypeAlias | Kind::Builtin(_) | Kind::WideInt { .. }),
                                    Err(_) => false,
                                };
                                let is_type = !is_const && (param.is_some() || names_a_type());
                                let lf::TypeRef::Path(tp) = v.ty(t)? else { return Err(program(format!("a one-name argument that is no path"), r)) };
                                if is_type {
                                    self.path_ty(tys, env, v, tp, &[])?
                                } else {
                                    let ar = place(v, v.type_range(t))?;
                                    // A name that is no parameter, no type and no value here
                                    // names nothing: refused as unresolved, never guessed to be
                                    // a constant (zerocopy's `HasField<Client, value, ..>`, whose
                                    // `value` is an enum of the impl's own block, was refused
                                    // "argument 2 of `HasField` is a constant").
                                    if param.is_none() && name != "true" && name != "false" {
                                        if let Err(e) = self.resolver.path(env.module, false, &[name], Ns::Value) {
                                            return Err(Why::Resolve(e, ar));
                                        }
                                    }
                                    let want = |_: &mut PortableTys<'_>| self.const_want(env, sabs, &stexts, index, ar);
                                    self.const_term(tys, env, name, ar, &want)?
                                }
                            }
                        },
                        lf::GenericArg::Const(c) => {
                            let at = v.const_range(c)?;
                            let ar = v.site(at)?;
                            let text = v.text_at(at)?;
                            let want = |_: &mut PortableTys<'_>| self.const_want(env, sabs, &stexts, index, ar);
                            self.const_term(tys, env, text, ar, &want)?
                        }
                        _ => continue,
                    };
                    out.push(made);
                    index += 1;
                }
            }
        }
        if next.next().is_some() {
            return Err(program(format!("a type argument is left over"), r));
        }
        Ok(out)
    }

    fn make(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, t: lf::TypeId, kids: &[PTy]) -> Result<PTy, Why> {
        let r = place(v, v.type_range(t))?;
        match v.ty(t)? {
            lf::TypeRef::Ref { mutbl, .. } => intern(tys, PNode::Ref(mutbl, kids[0])),
            lf::TypeRef::Ptr { mutbl, .. } => intern(tys, PNode::Ptr(mutbl, kids[0])),
            lf::TypeRef::Slice(_) => intern(tys, PNode::Slice(kids[0])),
            lf::TypeRef::Tuple(_) => {
                let l = intern_list(tys, kids)?;
                intern(tys, PNode::Tuple(l))
            }
            lf::TypeRef::Never => intern(tys, PNode::Never),
            lf::TypeRef::Infer => intern(tys, PNode::Infer),
            lf::TypeRef::Path(p) => {
                let args = self.path_args(tys, env, v, p, kids)?;
                self.path_ty(tys, env, v, p, &args)
            }
            lf::TypeRef::Dyn(bounds) => self.dyn_ty(tys, env, v, bounds, kids, r),
            // `[T; N]` (the Reference, "Array types": the length is a constant expression of
            // type `usize`): an integer literal is its value; `_` is a hole, as a `_` type is
            // (refused where a signature cannot have one, as that is); any other expression (a
            // constant's path, arithmetic) needs the evaluator over front2's records and is
            // refused by its text ([`Why::ConstPath`]).
            lf::TypeRef::Array { len, .. } => {
                let at = v.const_range(len)?;
                let lr = v.site(at)?;
                let text = v.text_at(at)?.trim();
                // The length is a `usize` (the Reference, "Array types"); a const parameter
                // (`[T; N]`) is its `ConstParam`; a literal is read with `usize`'s rules.
                let usize_ty = intern(tys, PNode::Uint(tc::UintTy::Usize))?;
                let n = match usize_literal(text, lr)? {
                    Some(bits) => intern(tys, PNode::ConstValue { ty: usize_ty, bits })?,
                    None => self.const_term(tys, env, text, lr, &|_| Ok(usize_ty))?,
                };
                intern(tys, PNode::Array(kids[0], n))
            }
            // `[unsafe] [extern "abi"] fn(A, B) -> R`: the inputs, then the output (`()` when
            // none is written), as `children` listed them; a C-variadic one is refused.
            lf::TypeRef::FnPtr { ret, abi, unsafety, extern_, variadic, .. } => {
                if variadic {
                    return Err(Why::Unsupported("a C-variadic fn pointer type", r));
                }
                let abi = match (abi, extern_) {
                    (None, false) => tc::Abi::Rust,
                    (None, true) => tc::Abi::C,
                    (Some(at), _) => fn_ptr_abi(v.text_at(at)?, r)?,
                };
                let safety = if unsafety { tc::Safety::Unsafe } else { tc::Safety::Safe };
                let mut l = Vec::with_capacity_in(kids.len() + 1, Global);
                l.extend_from_slice(kids);
                if ret.is_none() {
                    let unit = intern_list(tys, &[])?;
                    l.push(intern(tys, PNode::Tuple(unit))?);
                }
                let l = intern_list(tys, &l)?;
                intern(tys, PNode::FnPtr(l, tc::FnPtrHeader { abi, safety }))
            }
            lf::TypeRef::Macro(_) => self.macro_ty(tys, env, v, t, r),
            lf::TypeRef::Pat { pat, .. } => self.pat_ty(tys, env, v, kids[0], pat, r),
            // `for<'a> fn(&'a T)`: the binder's lifetimes are erased ([`super::preds::lifetimes_only`],
            // checked where `children` lists the inner type), so the type is its inner type.
            lf::TypeRef::For { .. } => Ok(kids[0]),
            lf::TypeRef::Impl(_) => match env.anon.iter().find(|(at, _)| *at == t) {
                Some(&(_, ty)) => Ok(ty),
                // Neither in a fn's parameter types nor in its return type (a field, a `let`,
                // a type alias, an `impl Trait` inside a fn pointer): refused by name.
                None => Err(Why::Unsupported("an `impl Trait` type outside a fn's parameters and return type", r)),
            },
            _ => Err(Why::Unsupported("this type", r)),
        }
    }

    /// A path type: `Self`, a generic parameter, an ADT, a primitive, or `<T as Tr>::X`.
    fn path_ty(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, p: PathIdx, kids: &[PTy]) -> Result<PTy, Why> {
        let (abs, texts, segs, r) = path_texts(v, p)?;
        let shown = v.text_at(v.path_range(p)?)?;
        if segs[0].kind == lf::SegKind::Anchor {
            let [_, last] = segs.as_slice() else {
                return Err(Why::Unsupported("a qualified path of more than one item", r));
            };
            let tr = segs[0].trait_.ok_or(Why::Unsupported("a qualified path with no trait (`<T>::X`)", r))?;
            let lf::TypeRef::Path(tp) = v.ty(tr)? else { return Err(program(format!("a trait that is no path"), r)) };
            // `kids` are the self type, then the arguments written on the trait's path, then
            // those written on the associated type's own segment (`path_args`' order). The
            // last are the associated type's (the Reference, "Associated Items", "Associated
            // Types": "Associated types may include generic parameters and where clauses;
            // these are often referred to as generic associated types"), never the trait's:
            // `<K as Key>::MapStorage<V>` gives `Key` no argument.
            let own = written_type_args(v, last)?;
            let Some(split) = kids.len().checked_sub(own).filter(|s| *s >= 1) else {
                return Err(program(format!("`{shown}` has fewer arguments than its last segment writes"), r));
            };
            let (trait_, args) = self.trait_path(tys, env, v, tp, kids[0], &kids[1..split])?;
            let Some(n) = last.name else { return Err(Why::Unsupported("this qualified path", r)) };
            return self.assoc_with_args(tys, trait_, Name(v.name(n)?), args, &kids[split..], r, shown);
        }
        if !abs && texts[0] == "Self" {
            return match (texts.len(), env.self_) {
                (1, SelfTy::Trait(_)) => intern(tys, PNode::Param(0)),
                (1, SelfTy::Ty(t, _)) => Ok(t),
                (2, SelfTy::Trait(tr)) => {
                    let (g, _) = self.heads.trait_head(tr).ok_or(Why::Need(tr))?;
                    // The trait's supertraits are filed when its own bounds are lowered
                    // (heads.rs `trait_members`); until then `v` IS its header, and a name the
                    // trait does not declare is found through the bounds written there
                    // ([`super::written`]): asking for the filed supertraits here is the trait's
                    // members needing themselves (reth's `trait Call: LoadState<Error:
                    // From<Self::Evm>> + ..`, the cycle that stopped tempo's contracts).
                    if self.heads.trait_supers(tr).is_none() && !self.heads.trait_has_assoc(tr, Name::of(texts[1])).ok_or(Why::Need(tr))? {
                        return self.written_self_assoc(tys, env, v, Name::of(texts[1]), r, shown);
                    }
                    let mut args = Vec::with_capacity_in(g as usize, Global);
                    for i in 0..g {
                        args.push(intern(tys, PNode::Param(i))?);
                    }
                    let args = intern_list(tys, &args)?;
                    self.self_assoc(tys, tr, args, Name::of(texts[1]), r, shown)
                }
                // The impl's own associated type; else, in a trait impl, the one its trait or a
                // supertrait declares (the Reference, "Paths", "`Self`" and "Paths in types":
                // core's `impl DoubleEndedIterator for X { fn rfold(..) -> Self::Item }`, where
                // `Item` is `Iterator`'s, given by another impl).
                (2, SelfTy::Ty(_, tr)) => match (env.self_assoc.iter().find(|(n, _)| *n == Name::of(texts[1])), tr) {
                    (Some(&(_, t)), _) => Ok(t),
                    (None, Some((tr, args))) => self.self_assoc(tys, tr, args, Name::of(texts[1]), r, shown),
                    (None, None) => Err(program(format!("`{shown}` names no associated type of the impl"), r)),
                },
                _ => Err(Why::Unsupported(SELF_HERE, r)),
            };
        }
        if !abs && texts.len() == 1 {
            if let Some(&(_, i)) = env.params.iter().find(|(n, _)| *n == Name::of(texts[0])) {
                // A const parameter is no type (its sort is checked here, never a `Param` the
                // solver meets as a malformed term). A bound's argument in a position its trait
                // declares const never comes here (const_args_in_bounds.rs makes it a
                // constant); what does is a const parameter where a type is declared, or one in
                // a trait object's or an `impl Trait`'s bound, whose trait is not known to the
                // walk yet.
                if env.consts.iter().any(|(c, _)| *c == i) {
                    return Err(Why::Unsupported("a const parameter in a type position (a bound's const argument `Tr<N>`)", r));
                }
                return intern(tys, PNode::Param(i));
            }
        }
        // `T::X` with `T` a type parameter: through `T`'s bounds (a parameter shadows an item
        // of its name, so this comes before the names library).
        if !abs && texts.len() == 2 {
            if let Some(&(_, i)) = env.params.iter().find(|(n, _)| *n == Name::of(texts[0])) {
                // `T::X<A>` is `<T as Tr>::X<A>` (the Reference, "Paths", "Paths in types";
                // "Associated Items", generic associated types): `kids` are the arguments
                // written on `X`'s own segment, the parameter's segment taking none.
                if segs[0].has_args {
                    return Err(Why::Unsupported("generic arguments on a type parameter (`T<A>::X`)", r));
                }
                return self.param_assoc(tys, env, v, i, Name::of(texts[1]), kids, r, shown);
            }
        }
        let found = match self.resolver.path(env.module, abs, &texts, Ns::Type) {
            Ok(f) => f,
            // A built-in type is the names library's answer (`Kind::Builtin`): front2 has no
            // lookup rule of its own for one.
            Err(e) => return Err(Why::Resolve(e, r)),
        };
        if found.consumed as usize != texts.len() {
            return Err(Why::Unsupported("an associated type of a type (`T::X`)", r));
        }
        match found.kind {
            Kind::Struct | Kind::Enum | Kind::Union => {
                let (g, defaults) = self.heads.adt_generics(found.key).ok_or(Why::Need(found.key))?;
                let args = with_defaults(tys, kids, g, defaults, shown, r)?;
                let l = intern_list(tys, &args)?;
                intern(tys, PNode::Adt(self.heads.checker(found.key), l))
            }
            // The Reference, "Type aliases": the path means the aliased type; a generic
            // alias's arguments are substituted for its parameters, those left out taking
            // their defaults as a struct's do (`alloy_sol_types::Result<Self>` is
            // `core::result::Result<Self, Error>` for `type Result<T, E = Error>`).
            Kind::TypeAlias => {
                let made = self.heads.alias(found.key).ok_or(Why::Need(found.key))?;
                // The alias's own refusal is named, so a census line shows the real cause.
                let Some((g, t)) = made else {
                    let why = self.heads.alias_gap(found.key).map_or_else(|| format!("(no why kept)"), |w| format!("{w:?}"));
                    return Err(program(format!("the type alias `{shown}` is refused: {why}"), r));
                };
                let args = with_defaults(tys, kids, g, self.heads.alias_defaults(found.key), shown, r)?;
                subst(tys, t, &args)
            }
            Kind::Builtin(name) => {
                let Some((_, n)) = PRIMS.iter().find(|(p, _)| *p == name) else {
                    return Err(Why::NoCheckerType(Box::from(name), r));
                };
                if !kids.is_empty() {
                    return Err(program(format!("`{shown}` takes no type arguments"), r));
                }
                intern(tys, *n)
            }
            // blaze-typeck's `IntTy` has the standard widths only.
            Kind::WideInt { signed, bits } => Err(Why::NoCheckerType(format!("{}{bits}", if signed { "i" } else { "u" }).into_boxed_str(), r)),
            _ => Err(program(format!("`{shown}` is not a type"), r)),
        }
    }

    /// `Self::name` in the trait `tr` (its arguments `args`): the trait's own associated type,
    /// else the one supertrait (searched through the supertraits' supertraits, by a work list)
    /// that declares it, its arguments substituted; two that declare it are ambiguous.
    fn self_assoc(&self, tys: &mut PortableTys<'_>, tr: LibKey, args: List, name: Name, r: Range, shown: &str) -> Result<PTy, Why> {
        match self.assoc_owner(tys, tr, args, name, r, shown)? {
            Some((t, a)) => self.alias(tys, t, name, a, r, shown),
            None => Err(program(format!("`{shown}` names no associated type of the trait or its supertraits"), r)),
        }
    }

    /// `T::name<own..>` with `T` the parameter `param`: `<T as Tr>::name<own..>` for the one
    /// trait `Tr` among `T`'s bounds in `env`, supertraits included, that declares `name`, the
    /// arguments `own` written on `name`'s segment being the generic associated type's own
    /// ([`Self::assoc_with_args`]; none is [`Self::alias`]). While the bounds are themselves
    /// being lowered (`Bounds::Building`) and none lowered so far declares it, the bounds of `T`
    /// as written in the header `v` are read ([`super::written`]: `T: Tr<T::X> + Declares`, a
    /// bound naming `X` before the bound that declares it); with own arguments that case is
    /// refused by name.
    #[expect(clippy::too_many_arguments, reason = "the path, where it is written, and the parameter")]
    fn param_assoc(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, param: u32, name: Name, own: &[PTy], r: Range, shown: &str) -> Result<PTy, Why> {
        let (bounds, done) = match env.bounds {
            Bounds::Building(b) => (b, false),
            Bounds::Done(b) => (b, true),
        };
        let mut found: Vec<(LibKey, List), Global> = Vec::new_in(Global);
        for b in bounds.iter().filter(|b| b.param == param) {
            if let Some(owner) = self.assoc_owner(tys, b.trait_, b.args, name, r, shown)? {
                if !found.contains(&owner) {
                    found.push(owner);
                }
            }
        }
        match found.as_slice() {
            &[(t, a)] => self.assoc_with_args(tys, t, name, a, own, r, shown),
            [] if !done && !own.is_empty() => Err(Why::Unsupported("`T::X<A>` in a bound written before the bound of `T` that declares `X`", r)),
            [] if !done => match self.written_param_assoc(tys, env, v, param, name, r, shown)? {
                Some(t) => Ok(t),
                None => Err(Why::Unsupported("`T::X` in a bound written before the bound of `T` that declares `X`", r)),
            },
            [] => Err(Why::AssocNotFound(Box::from(shown), r)),
            many => {
                let mut traits = Vec::with_capacity_in(many.len(), Global);
                traits.extend(many.iter().map(|&(t, _)| t));
                Err(Why::AssocAmbiguous(Box::from(shown), traits, r))
            }
        }
    }

    /// The trait declaring the associated type `name` among `tr` (its arguments `args`) and its
    /// supertraits (searched through the supertraits' supertraits, by a work list), with its
    /// arguments substituted; `None` when none does, an error when two do.
    pub(super) fn assoc_owner(&self, tys: &mut PortableTys<'_>, tr: LibKey, args: List, name: Name, r: Range, shown: &str) -> Result<Option<(LibKey, List)>, Why> {
        const MOST_SUPERS: usize = 256;
        let mut todo = Vec::new_in(Global);
        todo.push((tr, args));
        let mut found: Option<(LibKey, List)> = None;
        let mut seen = Vec::new_in(Global);
        for _ in 0..MOST_SUPERS {
            let Some((t, a)) = todo.pop() else { break };
            if seen.contains(&(t, a)) {
                continue;
            }
            seen.push((t, a));
            if self.heads.trait_has_assoc(t, name).ok_or(Why::Need(t))? {
                match found {
                    Some(prev) if prev != (t, a) => return Err(program(format!("`{shown}` is ambiguous: two supertraits declare it"), r)),
                    _ => found = Some((t, a)),
                }
                continue;
            }
            let given = tys.view().list(a).map_err(|_| program(format!("a list outside the table"), r))?;
            let mut given_v = Vec::with_capacity_in(given.len(), Global);
            given_v.extend_from_slice(given);
            for &(st, sargs) in self.heads.trait_supers(t).ok_or(Why::Need(t))? {
                let written = tys.view().list(sargs).map_err(|_| program(format!("a list outside the table"), r))?;
                let mut written_v = Vec::with_capacity_in(written.len(), Global);
                written_v.extend_from_slice(written);
                let mut out = Vec::with_capacity_in(written_v.len(), Global);
                for &w in &written_v {
                    out.push(subst(tys, w, &given_v)?);
                }
                todo.push((st, intern_list(tys, &out)?));
            }
        }
        if !todo.is_empty() {
            return Err(Why::Unsupported("a supertrait tree past 256 traits", r));
        }
        Ok(found)
    }

    /// `<args[0] as trait_<args[1..]>>::name`.
    fn alias(&self, tys: &mut PortableTys<'_>, trait_: LibKey, name: Name, args: List, r: Range, shown: &str) -> Result<PTy, Why> {
        if !self.heads.trait_has_assoc(trait_, name).ok_or(Why::Need(trait_))? {
            return Err(program(format!("`{shown}` names no associated type of the trait"), r));
        }
        intern(tys, PNode::Alias { trait_: self.heads.checker(trait_), name, args })
    }

    /// `<args[0] as trait_<args[1..]>>::name<own..>`: the associated type with the arguments
    /// written on its OWN segment. None written is [`Self::alias`]. Written ones are checked
    /// against the associated type's declared type and const parameters
    /// ([`Heads::assoc_generics`]; a count not on record is refused by name, never taken as
    /// zero), and follow the trait's arguments in the projection's list: the solver's form
    /// for a generic associated type (blaze-solve alias.rs: the arguments from the trait's
    /// generic count on are the associated type's own).
    #[expect(clippy::too_many_arguments, reason = "the projection, its own arguments, and where it is written")]
    fn assoc_with_args(&self, tys: &mut PortableTys<'_>, trait_: LibKey, name: Name, args: List, own: &[PTy], r: Range, shown: &str) -> Result<PTy, Why> {
        if own.is_empty() {
            return self.alias(tys, trait_, name, args, r, shown);
        }
        if !self.heads.trait_has_assoc(trait_, name).ok_or(Why::Need(trait_))? {
            return Err(program(format!("`{shown}` names no associated type of the trait"), r));
        }
        let Some(declared) = self.heads.assoc_generics(trait_, name) else {
            return Err(Why::Unsupported("type arguments on an associated type whose own parameters are not on record", r));
        };
        if declared as usize != own.len() {
            return Err(program(format!("the associated type of `{shown}` is written with {} type arguments; it takes {declared}", own.len()), r));
        }
        let of_trait = tys.view().list(args).map_err(|_| program(format!("`{shown}`: the trait's arguments are past the type table"), r))?;
        let mut all = Vec::with_capacity_in(of_trait.len() + own.len(), Global);
        all.extend_from_slice(of_trait);
        all.extend_from_slice(own);
        let all = intern_list(tys, &all)?;
        intern(tys, PNode::Alias { trait_: self.heads.checker(trait_), name, args: all })
    }

    /// The trait path `p` with `self_ty` as `Self`: the trait and its arguments, `Self` first,
    /// the defaults of those not written put in (substituted by the arguments before them).
    /// The trait a written type names (an impl's trait, a bound's), its arguments built here:
    /// `self_ty` first, then the written ones, then the defaults.
    pub fn trait_ref(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, t: lf::TypeId, self_ty: PTy) -> Result<(LibKey, List), Why> {
        let r = place(v, v.type_range(t))?;
        let lf::TypeRef::Path(p) = v.ty(t)? else { return Err(program(format!("a trait that is no path"), r)) };
        let mut written = Vec::new_in(Global);
        children(v, t, &mut written)?;
        let mut given = Vec::with_capacity_in(written.len(), Global);
        for &w in &written {
            given.push(self.ty(tys, env, v, w)?);
        }
        let given = self.path_args(tys, env, v, p, &given)?;
        self.trait_path(tys, env, v, p, self_ty, &given)
    }

    pub fn trait_path(
        &self,
        tys: &mut PortableTys<'_>,
        env: &Env<'_>,
        v: &View<'_>,
        p: PathIdx,
        self_ty: PTy,
        given: &[PTy],
    ) -> Result<(LibKey, List), Why> {
        let (abs, texts, _, r) = path_texts(v, p)?;
        self.trait_at(tys, env, abs, &texts, r, self_ty, given)
    }

    /// [`Self::trait_path`] over the path's texts.
    pub(super) fn trait_at(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, abs: bool, texts: &[&str], r: Range, self_ty: PTy, given: &[PTy]) -> Result<(LibKey, List), Why> {
        match self.bound_target(env, abs, texts, r)? {
            Bounded::Trait(key) => self.trait_args(tys, key, r, self_ty, given),
            Bounded::Alias(_) => Err(program(format!("`{}` is a trait alias, which stands for bounds, not one trait (an impl's trait, a `T::X` bound)", texts.join("::")), r)),
        }
    }

    /// What a bound's path names, by the names library: a trait or a trait alias.
    pub(super) fn bound_target(&self, env: &Env<'_>, abs: bool, texts: &[&str], r: Range) -> Result<Bounded, Why> {
        let found = self.resolver.path(env.module, abs, texts, Ns::Type).map_err(|e| Why::Resolve(e, r))?;
        match found.kind {
            _ if found.consumed as usize != texts.len() => Err(program(format!("this path is not a trait"), r)),
            Kind::Trait => Ok(Bounded::Trait(found.key)),
            Kind::TraitAlias => Ok(Bounded::Alias(found.key)),
            _ => Err(program(format!("this path is not a trait"), r)),
        }
    }

    /// The trait alias `alias` as a bound on `self_ty` with the written arguments `given`: its
    /// clauses with `Self` and its parameters substituted (the unstable book, `trait_alias`:
    /// a bound naming it "is equivalent to" its right-hand side), into `out`.
    pub(super) fn alias_clauses(&self, tys: &mut PortableTys<'_>, alias: LibKey, r: Range, self_ty: PTy, given: &[PTy], out: &mut Vec<PClause, Global>) -> Result<(), Why> {
        let head = self.heads.trait_alias(alias).ok_or(Why::Need(alias))?;
        let want = head.generics as usize - 1;
        if given.len() != want {
            return Err(program(format!("a trait alias written with {} type arguments; it takes {want}", given.len()), r));
        }
        let mut args = Vec::with_capacity_in(want + 1, Global);
        args.push(self_ty);
        args.extend_from_slice(given);
        for &c in &head.clauses {
            out.push(subst_clause(tys, c, &args)?);
        }
        Ok(())
    }

    /// The trait `key` with `self_ty` as `Self` and the written arguments `given`, the
    /// defaults of those not written put in.
    pub(super) fn trait_args(&self, tys: &mut PortableTys<'_>, key: LibKey, r: Range, self_ty: PTy, given: &[PTy]) -> Result<(LibKey, List), Why> {
        let (g, defaults) = self.heads.trait_head(key).ok_or(Why::Need(key))?;
        let want = g as usize - 1;
        if given.len() > want {
            return Err(program(format!("a trait written with {} type arguments; it takes {want}", given.len()), r));
        }
        let mut args = Vec::with_capacity_in(want + 1, Global);
        args.push(self_ty);
        args.extend_from_slice(given);
        for i in given.len()..want {
            let Some(d) = defaults.get(i).copied().flatten() else {
                return Err(program(format!("a trait written with {} type arguments; it takes {want}", given.len()), r));
            };
            let d = subst(tys, d, &args)?;
            args.push(d);
        }
        Ok((key, intern_list(tys, &args)?))
    }
}

/// What a bound's path names ([`Builder::bound_target`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bounded {
    Trait(LibKey),
    Alias(LibKey),
}

/// The clause `c` with each `Param(i)` replaced by `args[i]` ([`subst`] over its parts).
pub fn subst_clause(tys: &mut PortableTys<'_>, c: PClause, args: &[PTy]) -> Result<PClause, Why> {
    let list = |tys: &mut PortableTys<'_>, l: List| -> Result<List, Why> {
        let old = tys.view().list(l).map_err(|_| program(format!("a clause refers past the type table"), Range::new(0, 0)))?.to_vec_in(Global);
        let mut new = Vec::with_capacity_in(old.len(), Global);
        for t in old {
            new.push(subst(tys, t, args)?);
        }
        intern_list(tys, &new)
    };
    Ok(match c {
        PClause::Trait { trait_, args: l } => PClause::Trait { trait_, args: list(tys, l)? },
        PClause::Projection { trait_, args: l, name, term } => {
            let l = list(tys, l)?;
            PClause::Projection { trait_, args: l, name, term: subst(tys, term, args)? }
        }
        PClause::Const { cond, args: l } => PClause::Const { cond, args: list(tys, l)? },
    })
}

/// `t` with each `Param(i)` replaced by `args[i]`, by a work list. Moved here from front.rs,
/// whose copy goes at the switch (its one home after that; blaze-typeck's `PortableTys` is the
/// better one, its lane's to take).
pub fn subst(tys: &mut PortableTys<'_>, t: PTy, args: &[PTy]) -> Result<PTy, Why> {
    enum S {
        Visit(PTy),
        Make(PTy),
    }
    let bad = || program(format!("a type refers past the type table"), Range::new(0, 0));
    let mut todo = Vec::new_in(Global);
    let mut done: Vec<PTy, Global> = Vec::new_in(Global);
    let mut kids: Vec<PTy, Global> = Vec::new_in(Global);
    todo.push(S::Visit(t));
    while let Some(step) = todo.pop() {
        kids.clear();
        match step {
            S::Visit(t) => {
                todo.push(S::Make(t));
                children_of(tys.view(), t, &mut kids).ok_or_else(bad)?;
                todo.extend(kids.iter().rev().map(|&k| S::Visit(k)));
            }
            S::Make(t) => {
                children_of(tys.view(), t, &mut kids).ok_or_else(bad)?;
                let start = done.len() - kids.len();
                let node = *tys.view().get(t).ok_or_else(bad)?;
                let new: Vec<PTy, Global> = done.split_off(start);
                let made = match node {
                    PNode::Param(i) | PNode::ConstParam(i) => *args.get(i as usize).ok_or_else(|| program(format!("a generic parameter past the arguments"), Range::new(0, 0)))?,
                    PNode::Ref(m, _) => intern(tys, PNode::Ref(m, new[0]))?,
                    PNode::Ptr(m, _) => intern(tys, PNode::Ptr(m, new[0]))?,
                    PNode::Slice(_) => intern(tys, PNode::Slice(new[0]))?,
                    PNode::Array(..) => intern(tys, PNode::Array(new[0], new[1]))?,
                    PNode::Closure { owner, ordinal, .. } => {
                        let l = intern_list(tys, &new)?;
                        intern(tys, PNode::Closure { owner, ordinal, args: l })?
                    }
                    PNode::Future(e, _, _) => intern(tys, PNode::Future(e, new[0], new[1]))?,
                    PNode::Tuple(_) => {
                        let l = intern_list(tys, &new)?;
                        intern(tys, PNode::Tuple(l))?
                    }
                    PNode::Adt(k, _) => {
                        let l = intern_list(tys, &new)?;
                        intern(tys, PNode::Adt(k, l))?
                    }
                    PNode::FnDef(k, _) => {
                        let l = intern_list(tys, &new)?;
                        intern(tys, PNode::FnDef(k, l))?
                    }
                    PNode::FnPtr(_, h) => {
                        let l = intern_list(tys, &new)?;
                        intern(tys, PNode::FnPtr(l, h))?
                    }
                    PNode::Opaque(k, _) => {
                        let l = intern_list(tys, &new)?;
                        intern(tys, PNode::Opaque(k, l))?
                    }
                    PNode::Alias { trait_, name, .. } => {
                        let l = intern_list(tys, &new)?;
                        intern(tys, PNode::Alias { trait_, name, args: l })?
                    }
                    _ => t,
                };
                done.push(made);
            }
        }
    }
    done.pop().ok_or_else(bad)
}

/// A written ABI string (`"C"`, with its quotes): the one reading of it, for fn items
/// (`heads::abi_of`) and fn pointer types.
pub(super) fn fn_ptr_abi(written: &str, r: Range) -> Result<tc::Abi, Why> {
    let name = written.trim().strip_prefix('"').and_then(|t| t.strip_suffix('"')).ok_or(Why::Unsupported("an ABI that is no plain string", r))?;
    match name {
        "C" => Ok(tc::Abi::C),
        "C-unwind" => Ok(tc::Abi::CUnwind),
        "system" => Ok(tc::Abi::System),
        "system-unwind" => Ok(tc::Abi::SystemUnwind),
        "Rust" => Ok(tc::Abi::Rust),
        "rust-call" => Ok(tc::Abi::RustCall),
        // A body's `extern "llvm-intrinsic"` block: its declarations' headers (heads.rs, a
        // local foreign fn); the builder calls them by their `link_name` (front2/llvm_intrinsic.rs).
        "llvm-intrinsic" => Ok(tc::Abi::LlvmIntrinsic),
        _ => Err(Why::Unsupported("an ABI other than \"C\", \"C-unwind\", \"system\" and \"Rust\"", r)),
    }
}

/// The types `t` is made of, in order; `None` past the table.
pub fn children_of(t: blaze_typeck::PTys<'_>, ty: PTy, out: &mut Vec<PTy, Global>) -> Option<()> {
    match *t.get(ty)? {
        PNode::Ref(_, i) | PNode::Ptr(_, i) | PNode::Slice(i) => out.push(i),
        PNode::Closure { args, .. } => out.extend_from_slice(t.list(args).ok()?),
        PNode::Future(_, o, i) | PNode::Array(o, i) => {
            out.push(o);
            out.push(i);
        }
        PNode::Tuple(l)
        | PNode::Adt(_, l)
        | PNode::FnDef(_, l)
        | PNode::FnPtr(l, _)
        | PNode::Opaque(_, l)
        | PNode::Alias { args: l, .. } => out.extend_from_slice(t.list(l).ok()?),
        _ => {}
    }
    Some(())
}

/// Each `impl Trait` written in a fn's return type `ret` (the Reference, "Impl trait",
/// "Abstract return types": each is its own opaque type), in the order that numbers them:
/// preorder, left to right, an `impl` before the `impl`s in its bounds' arguments and
/// associated type bindings (`impl Iterator<Item = impl Show>`: the `Iterator` one is 0, the
/// `Show` one 1). The index is a function of the text alone, never of the order heads are
/// built in. A work list, no recursion.
///
/// Returns whether a type macro is written anywhere in `ret` (at any depth, in an `impl`'s
/// bounds' arguments too): its `impl`s are then not known from `ret`'s text, and the caller
/// expands `ret` first and numbers the expansion's ([`super::type_expand::Expanded::impls`];
/// the Reference, "Impl trait": an `impl Trait` a macro writes in the return type is one of the
/// fn's opaques like a written one).
pub(super) fn return_impls(v: &View<'_>, ret: lf::TypeId, out: &mut Vec<lf::TypeId, Global>) -> Result<bool, Why> {
    let mut todo: Vec<lf::TypeId, Global> = Vec::new_in(Global);
    let mut kids: Vec<lf::TypeId, Global> = Vec::new_in(Global);
    let mut macros = false;
    todo.push(ret);
    while let Some(t) = todo.pop() {
        kids.clear();
        match v.ty(t)? {
            lf::TypeRef::Impl(bounds) => {
                out.push(t);
                for &b in v.rows(bounds)? {
                    if let lf::Bound::Trait { ty, .. } = lf::Bound::decode(b)? {
                        bound_arg_types(v, ty, &mut kids)?;
                    }
                }
            }
            lf::TypeRef::Macro(_) => macros = true,
            _ => children(v, t, &mut kids)?,
        }
        todo.extend(kids.iter().rev());
    }
    Ok(macros)
}

/// The types written in a bound's trait path's arguments: type arguments, associated type
/// bindings (`Item = T`) and a parenthesized `Fn(A) -> B`'s inputs and output, in order.
fn bound_arg_types(v: &View<'_>, ty: lf::TypeId, out: &mut Vec<lf::TypeId, Global>) -> Result<(), Why> {
    let lf::TypeRef::Path(p) = v.ty(ty)? else { return Ok(()) };
    for &n in v.segments(v.path(p)?)? {
        let s = lf::Segment::decode(n)?;
        if !s.has_args && !s.parenthesized {
            continue;
        }
        for &a in v.rows(s.args)? {
            match lf::GenericArg::decode(a)? {
                lf::GenericArg::Type(t) | lf::GenericArg::AssocType { ty: t, .. } => out.push(t),
                _ => {}
            }
        }
        if s.parenthesized {
            out.extend(s.ty);
        }
    }
    Ok(())
}

/// The written types a type is made of, in order (the work list's children).
pub(super) fn children(v: &View<'_>, t: lf::TypeId, out: &mut Vec<lf::TypeId, Global>) -> Result<(), Why> {
    let r = place(v, v.type_range(t))?;
    match v.ty(t)? {
        lf::TypeRef::Ref { inner, .. } | lf::TypeRef::Ptr { inner, .. } | lf::TypeRef::Slice(inner) => out.push(inner),
        lf::TypeRef::Array { elem, .. } => out.push(elem),
        lf::TypeRef::Tuple(l) => out.extend(v.ids(l)?.iter().map(|&i| lf::TypeId(i))),
        lf::TypeRef::Path(p) => {
            let path = v.path(p)?;
            for &n in v.segments(path)? {
                let s = lf::Segment::decode(n)?;
                if s.kind == lf::SegKind::Anchor {
                    let (Some(ty), Some(tr)) = (s.ty, s.trait_) else {
                        return Err(Why::Unsupported("a qualified path with no trait (`<T>::X`)", r));
                    };
                    out.push(ty);
                    let lf::TypeRef::Path(tp) = v.ty(tr)? else { return Err(program(format!("a trait that is no path"), r)) };
                    for &tn in v.segments(v.path(tp)?)? {
                        arg_types(v, &lf::Segment::decode(tn)?, r, out)?;
                    }
                } else {
                    arg_types(v, &s, r, out)?;
                }
            }
        }
        lf::TypeRef::Never | lf::TypeRef::Infer => {}
        lf::TypeRef::FnPtr { params, ret, .. } => {
            for &n in v.rows(params)? {
                let p = lf::Param::decode(n)?;
                out.push(p.ty.ok_or(Why::Unsupported("a fn pointer parameter with no type", r))?);
            }
            out.extend(ret);
        }
        lf::TypeRef::Dyn(bounds) => {
            for &b in v.rows(bounds)? {
                if let Some((_, seg)) = super::dyn_ty::dyn_bound(v, lf::Bound::decode(b)?, r)? {
                    super::dyn_ty::dyn_arg_types(v, &seg, r, out)?;
                }
            }
        }
        // An anonymous type parameter is made of nothing: its bounds are its fn's predicates
        // ([`anon_params`]); any other `impl Trait` is refused where it is made.
        lf::TypeRef::Impl(_) => {}
        // A signature's type macro is expanded from its own text ([`super::pattern_ty`]).
        lf::TypeRef::Macro(_) => {}
        lf::TypeRef::Pat { base, .. } => out.push(base),
        lf::TypeRef::For { binder, inner } => {
            super::preds::lifetimes_only(v, binder, r)?;
            out.push(inner);
        }
        lf::TypeRef::Error => return Err(Why::Unsupported("this type", r)),
    }
    Ok(())
}
