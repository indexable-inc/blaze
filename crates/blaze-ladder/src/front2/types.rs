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
use blaze_own::Alloc;
use blaze_typeck as tc;
use blaze_typeck::regions::{Region, RegionError, each_slot_node};
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
    /// [`Why::Unsupported`] of the head of an item declared in a body, with that item named
    /// (rule 19): its kind, its name as written and the body it is declared in (heads.rs
    /// `Store::named_local`), since its key names nothing a person can find. The construct
    /// first, so a census groups it with [`Why::Unsupported`]'s class.
    UnsupportedIn(&'static str, Box<str, Global>, Range),
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
    /// The head's lifetimes were refused (D148): the language's error by its code, or a
    /// construct by name, with its range inside the refusal (heads_regions.rs).
    Regions(super::heads_regions::RegionsRefusal),
}

/// Why a type alias's head was refused ([`Heads::alias_gap`]): the refusal's text, the same
/// whether the head was lowered here (its `Why`'s `Debug`, the `Why` kept beside it) or filed
/// from a frozen head record (head_record.rs `K_ALIAS_REFUSED`, which holds the text only). Its
/// `Debug` is that text, so a message naming the refusal reads alike either way.
pub struct AliasGap {
    pub text: Box<str, Global>,
    /// The builder's own `Why`; `None` for a refusal filed from a record.
    pub why: Option<Why>,
}

impl AliasGap {
    /// A refusal made here.
    pub fn made(why: Why) -> AliasGap {
        AliasGap { text: format!("{why:?}").into_boxed_str(), why: Some(why) }
    }
}

impl core::fmt::Debug for AliasGap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.text)
    }
}

/// One region slot of a type alias's type, laid by the alias's head over its own lifetimes
/// (D148), in `each_slot_node`'s order over the aliased type ([`Heads::alias_slots`]): a use
/// `R<'q>` puts `'q` where the alias wrote `'x`, its expansion's slots substituted (the
/// Reference, "Type aliases").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AliasSlot {
    /// The alias's `i`-th declared lifetime parameter.
    Lifetime(u32),
    Static,
    /// Bound inside the aliased type (`for<'a> fn(&'a S)`): `binder` the binding node's index
    /// among the aliased type's binder nodes in pre-order.
    Late { binder: u32, var: u32 },
    /// A trait object's elided default lifetime (`type H = dyn Fn(..) + Send + Sync`), resolved
    /// by the use site's context (RFC 599: `'static` in `Arc<H>`, `'a` in `&'a H`).
    ObjectDefault,
}

/// A type alias's laid regions ([`Heads::alias_slots`]): its declared lifetime count and its
/// type's slots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AliasRegions {
    pub lifetimes: u32,
    pub slots: blaze_own::Vec<AliasSlot, Alloc<'static>>,
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
pub(super) fn with_defaults<'a>(tys: &mut PortableTys<'a>, kids: &[PTy], g: u32, defaults: &[Option<PTy>], shown: &str, r: Range) -> Result<blaze_own::Vec<PTy, Alloc<'a>>, Why> {
    if kids.len() > g as usize {
        return Err(program(format!("`{shown}` is written with {} type arguments; it takes {g}", kids.len()), r));
    }
    let mut args = blaze_own::Vec::with_capacity_in(g as usize, tys.alloc());
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

fn table_full() -> Why {
    program(format!("the type table is full"), Range::new(0, 0))
}

pub fn intern(tys: &mut PortableTys<'_>, n: PNode) -> Result<PTy, Why> {
    tys.intern(n).map_err(|_| table_full())
}

pub fn intern_list(tys: &mut PortableTys<'_>, l: &[PTy]) -> Result<List, Why> {
    tys.intern_list(l).map_err(|_| table_full())
}

/// A supertrait associated-type equality on `Self` (`trait Sub: Sup<Item = T>`),
/// over the declaring trait's own parameters (`Self` = `Param(0)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SuperProj {
    pub trait_: ItemKey,
    pub name: Name,
    pub args: List,
    pub term: PTy,
}

/// One lifetime argument written on a trait's supertrait bound (`trait Sub<'x>: Super<'x>`),
/// used by RFC 1156 default trait object lifetime resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuperLifetime {
    /// `'static`.
    Static,
    /// The `i`-th declared lifetime parameter of the subtrait (`0..declared_lifetimes`).
    Param(u32),
    /// A non-substitutable lifetime (e.g. bound by `for<'a>` on the supertrait clause, or elided).
    Other,
}

/// A trait's direct `Self: Super<'a, ..>` supertrait lifetime edges, packed into two flat
/// vectors: edge `k` targets `edges[k].0` with lifetime arguments `args[edges[k].1..edges[k].2]`,
/// plus any direct `Self: 'r` bounds inherited through trait alias supertraits
/// (`trait LtAlias<'a> = 'a; trait Sub<'x>: LtAlias<'x> {}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuperLifetimes {
    pub edges: blaze_own::Vec<(LibKey, u32, u32), Alloc<'static>>,
    pub args: blaze_own::Vec<SuperLifetime, Alloc<'static>>,
    pub alias_outlives: blaze_own::Vec<SuperLifetime, Alloc<'static>>,
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
    /// The evaluated primitive type (`bool`, `char`, `Int`, `Uint`) of the evaluated constant
    /// `key`, when known.
    fn const_ty(&self, _key: LibKey) -> Option<PNode> {
        None
    }
    /// Takes the anonymous constant `ask` to be made a head and evaluated
    /// ([`super::anon`]); `false` where these heads have no evaluator behind them (the
    /// default), and the expression stays the head's refusal.
    fn ask_anon(&self, _ask: super::anon::Ask) -> bool {
        false
    }
    /// The allocator an asked anonymous constant's token marks are copied into
    /// ([`super::anon::Ask::carry_marks`]); `None` where these heads have no evaluator behind
    /// them (the default: nothing is asked).
    fn mark_alloc(&self) -> Option<Alloc<'static>> {
        None
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
    /// The trait's supertrait associated-type equalities on `Self` (`: Sup<Item = T>`),
    /// filed with its members alongside [`Heads::trait_supers`].
    fn trait_super_projs(&self, trait_: LibKey) -> Option<&[SuperProj]>;
    /// Region slots of a supertrait associated-type equality on `Self` (`<Self as sup_trait>::name = term`)
    /// declared on `sub_trait`, numbered over `sub_trait`'s own lifetime parameters (`Region::Param(i)`).
    fn trait_super_proj_regions(&self, sub_trait: ItemKey, sup_trait: ItemKey, name: Name) -> Option<&[Region]> {
        let _ = (sub_trait, sup_trait, name);
        None
    }
    /// Whether the trait's full members (`Want::Members`, Layer 3 — not merely Layer 2
    /// `supers_only`) have been filed, including `assoc_const_params`.
    fn trait_members_made(&self, trait_: LibKey) -> bool {
        self.trait_supers(trait_).is_some()
    }
    /// Whether the trait is an `auto trait` (from its own head).
    fn trait_auto(&self, trait_: LibKey) -> Option<bool>;
    /// Whether the trait declares a `Self: 'static` bound (e.g. `core::any::Any`).
    fn trait_self_static(&self, trait_: LibKey) -> Option<bool>;
    /// The declared lifetime parameter indices `i` (`0..declared_lifetimes`) for which the trait
    /// directly declares `Self: 'lt_i` (in its supertrait bounds or `where Self: 'lt_i`), sorted.
    fn trait_self_outlives_params(&self, trait_: LibKey) -> Option<&[u32]>;
    /// The trait's direct `Self: Super<..>` supertrait lifetime edges, filed alongside
    /// [`Heads::trait_supers`].
    fn trait_super_lifetimes(&self, trait_: LibKey) -> Option<&SuperLifetimes>;
    /// A type alias's generic count and its aliased type over them (`Param(i)` the alias's
    /// i-th parameter): a path to it means the aliased type with its arguments substituted.
    /// `Some(None)`: its head was tried and refused (the why is in the store's `alias_gaps`).
    fn alias(&self, alias: LibKey) -> Option<Option<(u32, PTy)>>;
    /// A type alias's parameter defaults, made with its head (`None` where none is written;
    /// empty before its head is made).
    fn alias_defaults(&self, alias: LibKey) -> &[Option<PTy>];
    /// A type alias's type laid over its own lifetimes ([`AliasRegions`]), made with its head;
    /// `None` where its head laid none (its walk refused them, or its type holds an elided
    /// lifetime, a trait object's default or an undeclared name): a use whose expansion holds a
    /// lifetime is then refused by name.
    fn alias_slots(&self, alias: LibKey) -> Option<&AliasRegions>;
    /// Why a type alias's head was refused (`Some(None)` from [`Heads::alias`]).
    fn alias_gap(&self, alias: LibKey) -> Option<&AliasGap>;
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
    /// How many lifetime parameters the BODY-LOCAL ADT `adt` (a checker key; an item declared
    /// in a body, which the names world does not hold) declares, filed when its body made its
    /// head. Every other item's count is the names' ([`declared_lifetimes`], D155): never read
    /// here. `None`: not filed.
    fn adt_lifetimes(&self, adt: ItemKey) -> Option<u32>;
    /// How many lifetime parameters the body-local trait `trait_` declares, as `adt_lifetimes`.
    fn trait_lifetimes(&self, trait_: ItemKey) -> Option<u32>;
    /// How many lifetime parameters the body-local trait's associated type `name` declares on
    /// itself, as `adt_lifetimes`.
    fn assoc_lifetimes(&self, trait_: ItemKey, name: Name) -> Option<u32>;
    /// How many lifetimes the opaque type `opaque` captures (filed by its fn's head).
    fn opaque_lifetimes(&self, opaque: ItemKey) -> Option<u32>;
    /// Records the source spelling `text` of structural record field `name` so record type spread
    /// (`{ ..Base, extra: T }`) sorts merged fields alphabetically by source spelling.
    fn note_record_field_name(&self, _name: Name, _text: &str) {}
    /// The recorded source spelling of structural record field `name`, if known.
    fn record_field_name(&self, _name: Name) -> Option<&str> {
        None
    }
}

/// The one region counter's facts (`regions::Arity`, D148): each DECLARED lifetime count
/// (an ADT's, a trait's, an associated type's) from the names (`names`, D155:
/// [`declared_lifetimes`]), the same in every store whatever heads it made; an opaque's
/// captures from the fn being made (`opaques`, not filed until its head is), then as the
/// heads filed them. An unknown fact is `Count::NotKnown`, never a guessed count (rule 3).
pub struct HeadsArity<'h, H: Heads + ?Sized, S> {
    pub heads: &'h H,
    pub opaques: &'h [(ItemKey, u32)],
    pub names: S,
}

impl<'n, H: Heads + ?Sized, S: NamesSource<'n>> tc::regions::Arity for HeadsArity<'_, H, S> {
    fn adt_lifetimes(&self, adt: ItemKey) -> tc::regions::Count {
        known(declared_lifetimes(self.heads, self.names, adt, Declared::Adt))
    }
    fn opaque_captures(&self, opaque: ItemKey) -> tc::regions::Count {
        match self.opaques.iter().find(|(k, _)| *k == opaque) {
            Some(&(_, n)) => tc::regions::Count::Known(n),
            None => known(self.heads.opaque_lifetimes(opaque)),
        }
    }
    fn alias_lifetimes(&self, trait_: ItemKey, name: Name) -> tc::regions::Count {
        let own = declared_lifetimes(self.heads, self.names, trait_, Declared::Trait);
        match (own, declared_lifetimes(self.heads, self.names, trait_, Declared::Assoc(name))) {
            (Some(t), Some(a)) => t.checked_add(a).map_or(tc::regions::Count::NotKnown, tc::regions::Count::Known),
            _ => tc::regions::Count::NotKnown,
        }
    }
    fn trait_lifetimes(&self, trait_: ItemKey) -> tc::regions::Count {
        known(declared_lifetimes(self.heads, self.names, trait_, Declared::Trait))
    }
}

/// Whose declared lifetimes [`declared_lifetimes`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Declared {
    /// A struct's, enum's or union's.
    Adt,
    /// A trait's.
    Trait,
    /// The named associated type's own (a GAT's) of the trait asked.
    Assoc(Name),
}

/// THE ONE READ of a declared lifetime count (D155; design/states.md item 30): a fact of the
/// item's own text, whose one home is its crate's items record. `item` is a checker key: the
/// names find its library key (`NamesSource::key_beginning`, the solved names' index or one
/// search of its crate's records) and answer from that crate's record
/// (`NamesSource::declared_lifetimes`, `assoc_lifetimes`), in any store, whatever heads were
/// made. A key the names world holds no item for is a body-local item's (declared in a body,
/// keyed by its body: front2 locals.rs): its count is the one its body filed with its head
/// (`Heads::adt_lifetimes`), a body's locals being made before any type of the body names
/// them. Which of the two answers is decided by whose key it is, never by a failed ask.
pub fn declared_lifetimes<'n, H: Heads + ?Sized, S: NamesSource<'n>>(heads: &H, names: S, item: ItemKey, what: Declared) -> Option<u32> {
    match (names.key_beginning(item.0.to_be_bytes()), what) {
        (Some(lib), Declared::Adt | Declared::Trait) => names.declared_lifetimes(lib),
        (Some(lib), Declared::Assoc(name)) => names.assoc_lifetimes(lib, name),
        (None, Declared::Adt) => heads.adt_lifetimes(item),
        (None, Declared::Trait) => heads.trait_lifetimes(item),
        (None, Declared::Assoc(name)) => heads.assoc_lifetimes(item, name),
    }
}

fn known(n: Option<u32>) -> tc::regions::Count {
    n.map_or(tc::regions::Count::NotKnown, tc::regions::Count::Known)
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
    pub clauses: blaze_own::Vec<PClause, Alloc<'static>>,
    /// Its trait clauses on `Self` by library key (what `T::X` is looked up through, as a
    /// [`ParamBound`]), with their arguments over the same parameters.
    pub traits: blaze_own::Vec<(LibKey, List), Alloc<'static>>,
    /// Expanded `Self: Tr<'a, ..>` lifetime edges (and direct `Self: 'a` bounds) over the
    /// alias's own declared lifetimes, with nested trait aliases already substituted.
    pub super_lifetimes: SuperLifetimes,
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
    /// The body whose blocks may bind the first segment of a written path
    /// ([`super::scope_table::written`]). `None` for a signature or other item head.
    pub body: Option<super::block_type_uses::BodyScope<'e>>,
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

/// Inline-backed small buffer for path segments and texts (nearly all Rust paths have `<= 8`
/// segments), avoiding two heap allocations per path resolution.
#[derive(Clone, Debug)]
pub struct SmallPath<T: Copy, const N: usize = 8> {
    inline: [T; N],
    len: usize,
    spill: Option<Vec<T, Global>>,
}

impl<T: Copy, const N: usize> SmallPath<T, N> {
    #[inline]
    fn new(fill: T) -> Self {
        Self { inline: [fill; N], len: 0, spill: None }
    }

    #[inline]
    fn push(&mut self, value: T) {
        if let Some(v) = &mut self.spill {
            v.push(value);
        } else if let Some(slot) = self.inline.get_mut(self.len) {
            *slot = value;
            self.len += 1;
        } else {
            let mut v = Vec::with_capacity_in(N * 2, Global);
            v.extend_from_slice(&self.inline);
            v.push(value);
            self.spill = Some(v);
        }
    }
    #[inline]
    fn pop(&mut self) -> Option<T> {
        if let Some(v) = &mut self.spill {
            v.pop()
        } else if let Some(next_len) = self.len.checked_sub(1)
            && let Some(&val) = self.inline.get(next_len)
        {
            self.len = next_len;
            Some(val)
        } else {
            None
        }
    }

    #[inline]
    fn clear(&mut self) {
        if let Some(v) = &mut self.spill {
            v.clear();
        } else {
            self.len = 0;
        }
    }

    #[inline]
    fn truncate(&mut self, len: usize) {
        if let Some(v) = &mut self.spill {
            v.truncate(len);
        } else if len < self.len {
            self.len = len;
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[T] {
        match &self.spill {
            Some(v) => v.as_slice(),
            None => self.inline.get(..self.len).unwrap_or(&[]),
        }
    }
}

impl<T: Copy, const N: usize> core::ops::Deref for SmallPath<T, N> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

const EMPTY_SEGMENT: lf::Segment = lf::Segment {
    kind: lf::SegKind::Ident,
    name: None,
    args: tc::List { start: 0, len: 0 },
    has_args: false,
    parenthesized: false,
    return_notation: false,
    ty: None,
    trait_: None,
};

/// A path's segments as the resolver takes them: their texts, and whether it starts at `::`.
/// A qualified (`<T as Tr>::X`) or parenthesized segment is not a plain path.
pub fn path_texts<'v>(v: &View<'v>, p: PathIdx) -> Result<(bool, SmallPath<&'v str>, SmallPath<lf::Segment>, Range), Why> {
    path_texts_of(v, p, false)
}

/// [`path_texts`]; `sugar`: the caller reads `Fn(A) -> B` on the last segment itself.
pub fn path_texts_of<'v>(v: &View<'v>, p: PathIdx, sugar: bool) -> Result<(bool, SmallPath<&'v str>, SmallPath<lf::Segment>, Range), Why> {
    let r = place(v, v.path_range(p))?;
    let path = v.path(p)?;
    let mut texts = SmallPath::new("");
    let mut segs = SmallPath::new(EMPTY_SEGMENT);
    let all = v.segments(path)?;
    for (i, &n) in all.iter().enumerate() {
        let s = lf::Segment::decode(n)?;
        let read = sugar && i + 1 == all.len() && !s.return_notation;
        if (s.parenthesized && !read) || s.return_notation {
            return Err(Why::Unsupported("parenthesized generic arguments (`Fn(A) -> B`)", r));
        }
        // A raw identifier's segment is its name without `r#` (`impl<T> Tr for r#return<T>`),
        // as the names library files it (blaze-scope lower.rs `unraw`); the raw text stayed a
        // segment no item is named, "a path did not resolve".
        let text = match (s.kind, s.name) {
            (lf::SegKind::Ident, Some(n)) => blaze_scope::task::unraw(v.name_text(n)?),
            (lf::SegKind::SelfMod, _) => "self",
            (lf::SegKind::Super, _) => "super",
            (lf::SegKind::Crate, _) => "crate",
            (lf::SegKind::SelfTy, _) => "Self",
            (lf::SegKind::DollarCrate, _) => "$crate",
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
fn arg_types_into(v: &View<'_>, s: &lf::Segment, r: Range, out: &mut impl FnMut(lf::TypeId)) -> Result<(), Why> {
    if !s.has_args {
        return Ok(());
    }
    for &n in v.rows(s.args)? {
        match lf::GenericArg::decode(n)? {
            lf::GenericArg::Type(t) if !s.parenthesized && bare_name(v, t)?.is_some() => {}
            lf::GenericArg::Type(t) => out(t),
            lf::GenericArg::AssocType { args, ty, .. } if !s.parenthesized && args.len == 0 => out(ty),
            lf::GenericArg::Lifetime(_) | lf::GenericArg::AssocType { .. } | lf::GenericArg::Const(_) => {}
            _ => return Err(Why::Unsupported("an associated item constraint", r)),
        }
    }
    if s.parenthesized
        && let Some(ret) = s.ty
    {
        out(ret);
    }
    Ok(())
}

/// How many of a segment's generic arguments are types or constants (a lifetime is erased; an
/// associated item constraint is no argument): how many entries the segment has in
/// [`Builder::path_args`]' answer.
pub(super) fn written_type_args(v: &View<'_>, s: &lf::Segment) -> Result<usize, Why> {
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

/// Reads the target pointer width (`32` or `64`) from `module`'s crate cfg atoms, refusing with
/// a named error when the crate or its `target_pointer_width` atom is missing rather than
/// assuming 64-bit.
fn target_pointer_width<'n, S: NamesSource<'n>>(names: S, module: LibKey, r: Range) -> Result<u32, Why> {
    let krate = names
        .crate_of(module)
        .ok_or(Why::Unsupported("a pointer-sized constant literal whose module's crate is not read", r))?;
    let atoms = names
        .cfg_atoms(krate)
        .ok_or(Why::Unsupported("a pointer-sized constant literal whose crate cfg is not read", r))?;
    atoms
        .iter()
        .find(|(n, _)| *n == "target_pointer_width")
        .and_then(|(_, w)| w.and_then(|w| w.parse::<u32>().ok()))
        .filter(|&b| b == 32 || b == 64)
        .ok_or(Why::Unsupported("a pointer-sized constant literal whose crate cfg has no valid `target_pointer_width`", r))
}

/// A const argument's value as the checker holds it (`PNode::ConstValue`), from the literal
/// `text` of type `want` (the Reference, "Literal expressions"): `true`, `false`, or an integer
/// literal, `-` before it for a signed type; its bits are the value's in `want`'s width, as
/// blaze-consteval writes them. `None`: the text is no such literal.
fn literal_value<'n, S: NamesSource<'n>>(
    tys: &mut PortableTys<'_>,
    text: &str,
    want: PTy,
    names: S,
    module: LibKey,
    r: Range,
) -> Result<Option<PTy>, Why> {
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
    let bits = match name {
        "isize" | "usize" => match target_pointer_width(names, module, r) {
            Ok(b) => b,
            Err(err) => {
                // Never silently assume 64-bit: if the crate's cfg was not read at all, or if
                // the literal is negative (width-dependent two's complement) or exceeds 32-bit
                // range, refuse with the named missing-pointer-width error. Only a non-negative
                // literal that fits in 32 bits under an already-read empty synthetic cfg has
                // identical validity and bit pattern across 32-bit and 64-bit targets.
                let cfg_read = names.crate_of(module).and_then(|k| names.cfg_atoms(k)).is_some();
                let fits_32 = !neg
                    && if signed {
                        value <= u128::from(i32::MAX.cast_unsigned())
                    } else {
                        value <= u128::from(u32::MAX)
                    };
                if cfg_read && fits_32 { 32 } else { return Err(err) }
            }
        },
        _ => int_bits(name),
    };
    let mask = if bits == 128 { u128::MAX } else { (1u128 << bits) - 1 };
    let most = if signed { mask >> 1 } else { mask };
    let fits = value <= most || (neg && value == most + 1);
    if !fits {
        return Err(program(format!("the const argument `{text}` overflows `{name}`"), r));
    }
    let bits = if neg { value.wrapping_neg() & mask } else { value };
    Ok(Some(intern(tys, PNode::ConstValue { ty: want, bits })?))
}

/// A fixed-width integer type's width by its name (`isize`/`usize` are read via
/// [`target_pointer_width`]).
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

/// Expansion provenance for a type lowered by `type_expand::expand_type_text` (`pattern_ty.rs`
/// `macro_ty` and `heads.rs` `fn_head` return-type expansion): lets [`Builder::head_owner_of`]
/// connect item-declaring const blocks inside the expansion back to the enclosing item's
/// `blaze-scope` expansion block modules (`__blaze_block_x<tag(call)>_<ord>`).
#[derive(Clone, Copy)]
pub(super) struct ExpandedOwner<'b> {
    pub(super) item: LibKey,
    pub(super) owner_text: &'b str,
    pub(super) base_offset: u32,
    pub(super) view: View<'b>,
}

/// The type builder of one item: its resolver, over the names source `S`
/// ([`NamesSource`]: the solved names or each crate's records), and the heads lowered so far.
pub struct Builder<'b, H: Heads, S> {
    pub resolver: Paths<S>,
    pub heads: &'b H,
    /// The marks of the tokens of the view's **non-expansion** text (front2 item_site.rs,
    /// token-mark-F1): `None` for a text written in a file, or one that keeps no column.
    /// Invariant: when `Some(m)`, every non-expansion slice read from the view is inside `m.text`
    /// (never the separate `view.expanded` buffer; those marks live in `expanded_marks`).
    pub marks: Option<blaze_scope::crate_roots::TextMarks<'b>>,
    /// The enclosing library item and the exact scanned text (`impl_header` for an `impl`, whole
    /// text for a `fn`/`const`/`static` with `body_holds_items`) that `blaze-scope` built block
    /// modules for: carried onto head-level anonymous constants (`anon::HeadOwner`) so blocks
    /// declared inside them resolve to their `blaze-scope` block modules.
    pub owner: Option<(LibKey, &'b str)>,
    /// The checker key of the item/member/body whose written types this builder lowers, used to
    /// scope structural-record field default tags independently of whether the item holds const
    /// blocks (`owner`).
    pub owner_key: Option<ItemKey>,
    /// Present when this builder walks an expanded type's view (`pattern_ty.rs` / `heads.rs`):
    /// maps const blocks inside macro expansions back to the enclosing head call's expansion
    /// block modules.
    pub(super) expanded: Option<ExpandedOwner<'b>>,
    /// Token marks over the expansion buffer (`view.expanded_text()`) when walking an expanded
    /// type's view (`pattern_ty.rs` / `heads.rs`), kept separate from `marks` so non-expansion
    /// paths never query the expansion buffer's column, and preserved even when `expanded` is
    /// `None`.
    pub(super) expanded_marks: Option<blaze_scope::crate_roots::TextMarks<'b>>,
}

impl<'b, 'n, H: Heads, S: NamesSource<'n>> Builder<'b, H, S> {
    /// Selects the token-mark column whose text holds `slice` (`self.marks` for the view's
    /// non-expansion text or `self.expanded_marks` for its expansion buffer), returning `None`
    /// when neither column covers `slice`.
    pub(super) fn marks_for(&self, slice: &str) -> Option<blaze_scope::crate_roots::TextMarks<'b>> {
        if let Some(m) = self.marks
            && blaze_scope::expansion_origins::offset_in(m.text, slice).is_some()
        {
            return Some(m);
        }
        if let Some(em) = self.expanded_marks
            && blaze_scope::expansion_origins::offset_in(em.text, slice).is_some()
        {
            return Some(em);
        }
        None
    }

    /// Resolves the `blaze-scope` `MacroCall` key and outermost owner-text call offset for
    /// expansion `target_exp` inside `exp.view`.
    fn resolve_expansion_call(&self, exp: ExpandedOwner<'_>, target_exp: u32) -> Option<super::anon::HeadExpansion> {
        let names = self.resolver.names;
        let krate = names
            .crate_of(exp.item)
            .or_else(|| names.member_owner(exp.item).and_then(|(c, _)| names.crate_of(c)))?;
        let crate_key = names.crate_key(krate)?;
        blaze_mem::frame::task(blaze_mem::Tag::Parse, |frame| {
            let scratch = blaze_own::Erased::new(frame);
            let alloc = scratch.handle();
            let total = exp.view.expansion_keys.len();
            let mut chain: blaze_own::AVec<'_, u32> = blaze_own::AVec::new_in(alloc);
            let mut cur = target_exp;
            for _ in 0..=total {
                chain.push(cur);
                match exp.view.expansion_call_at(cur).ok()? {
                    blaze_lower::scope_origin::CallAt::Item { .. } => break,
                    blaze_lower::scope_origin::CallAt::Expansion { expansion: writer, .. } => cur = writer,
                }
            }
            chain.reverse();
            let root_exp = *chain.first()?;
            let blaze_lower::scope_origin::CallAt::Item { start: root_start } = exp.view.expansion_call_at(root_exp).ok()? else {
                return None;
            };
            let root_call_offset = exp.base_offset.checked_add(root_start)?;
            let owner_nested = blaze_file_items::nested::scan(exp.owner_text, alloc);
            if owner_nested.end != blaze_file_items::nested::NestedEnd::Whole {
                return None;
            }
            let owner_modules = blaze_scope::blocks::block_modules(crate_key, exp.item, &owner_nested, alloc);
            let mut call_key = if let Some((block, n)) = super::scope_table::statement_call(&owner_nested, root_call_offset) {
                let owner_mod = usize::try_from(block).ok().and_then(|idx| owner_modules.get(idx)).copied().flatten()?;
                blaze_scope::task::key_of(crate_key, owner_mod, "MacroCall", "", n)
            } else {
                if owner_nested.head_calls.is_empty() {
                    return None;
                }
                let head_mod = blaze_scope::blocks::head_call_module(crate_key, exp.item);
                let n = super::scope_table::head_call(&owner_nested, root_call_offset)?;
                blaze_scope::task::key_of(crate_key, head_mod, "MacroCall", "", n)
            };
            for &next_exp in chain.get(1..).unwrap_or(&[]) {
                let blaze_lower::scope_origin::CallAt::Expansion { expansion: writer, start } = exp.view.expansion_call_at(next_exp).ok()? else {
                    return None;
                };
                let writer_text = exp.view.expansion_text(writer).ok()?;
                let writer_blocks = blaze_scope::expanded_blocks::expansion_blocks(writer_text, alloc).ok()?;
                let block_count = writer_blocks.nested.blocks.len();
                let mut writer_modules: blaze_own::AVec<'_, Option<LibKey>> = blaze_own::AVec::with_capacity_in(block_count, alloc);
                writer_modules.resize(block_count, None);
                for (b, block) in writer_blocks.nested.blocks.iter().enumerate() {
                    let Some(ordinal) = writer_blocks.ordinals.get(b).copied().flatten() else { continue };
                    let outer = match block.parent {
                        None | Some(0) => Some(call_key),
                        Some(p) => usize::try_from(p).ok().and_then(|idx| writer_modules.get(idx)).copied().flatten(),
                    };
                    let name = blaze_scope::expanded_blocks::expansion_block_name(call_key, ordinal, alloc);
                    if let Some(slot) = writer_modules.get_mut(b) {
                        *slot = outer.map(|o| blaze_scope::task::key_of(crate_key, o, "mod", name.as_str(), 0));
                    }
                }
                let target_start = start.checked_add(blaze_scope::expanded_blocks::WRAP_OPEN)?;
                let (block, n) = super::scope_table::statement_call(&writer_blocks.nested, target_start)?;
                let owner_mod = if block == 0 {
                    call_key
                } else {
                    usize::try_from(block).ok().and_then(|idx| writer_modules.get(idx)).copied().flatten()?
                };
                call_key = blaze_scope::task::key_of(crate_key, owner_mod, "MacroCall", "", n);
            }
            Some(super::anon::HeadExpansion { call: call_key, root_call_offset })
        })
    }

    /// If `text` is an unmodified subslice of `self.owner`'s scanned text (or of a macro expansion
    /// in `self.expanded`) and may declare items or invoke macros, returns its
    /// [`super::anon::HeadOwner`] coordinates, optional expansion text, and—when `text` contains
    /// a macro call (`!`)—its observable [`super::anon::HeadSite`] (`(line, col)` and shown file
    /// path via `body_place`/`body_file`) so inserting a blank line or moving a module across
    /// `#[path]` files invalidates cached head constants.
    fn head_owner_of(&self, text: &str) -> Option<(super::anon::HeadOwner, Option<&'b str>, Option<super::anon::HeadSite<'n>>)> {
        if !blaze_file_items::nested::may_write_block(text) {
            return None;
        }
        let len = u32::try_from(text.len()).ok()?;
        if let Some((item, owner_text)) = self.owner
            && let Some(offset) = blaze_scope::expansion_origins::offset_in(owner_text, text)
        {
            let site = super::anon::head_site_of(self.resolver.names, item, offset, text);
            return Some((super::anon::HeadOwner { item, offset, len, expansion: None }, None, site));
        }
        let exp = self.expanded?;
        for j in 0..exp.view.expansion_keys.len() {
            let Ok(j_u32) = u32::try_from(j) else { break };
            let Ok(exp_text) = exp.view.expansion_text(j_u32) else { continue };
            if let Some(offset) = blaze_scope::expansion_origins::offset_in(exp_text, text) {
                let head_exp = self.resolve_expansion_call(exp, j_u32)?;
                let site = super::anon::head_site_of(self.resolver.names, exp.item, head_exp.root_call_offset, text);
                return Some((
                    super::anon::HeadOwner { item: exp.item, offset, len, expansion: Some(head_exp) },
                    Some(exp_text),
                    site,
                ));
            }
        }
        None
    }
    /// The portable type of `t`, by a work list (no recursion).
    pub fn ty(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, t: lf::TypeId) -> Result<PTy, Why> {
        self.walk_ty(tys, env, v, t, None)
    }

    /// [`Builder::ty`], with the type's region slots pushed into `sink` in the same walk
    /// (D148): one fragment more on its stack, the slots in `regions::each_slot_node`'s order
    /// over the made type. A region position this producer does not lay is the sink's
    /// refusal ([`RegionSink::refused`]), never the type's: the erased type is made as by
    /// [`Builder::ty`]. A program error of the lifetimes themselves (E0106) is the type's.
    pub fn ty_regions(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, t: lf::TypeId, sink: &mut RegionSink<'_>) -> Result<PTy, Why> {
        self.walk_ty(tys, env, v, t, Some(sink))
    }

    fn walk_ty(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, t: lf::TypeId, mut sink: Option<&mut RegionSink<'_>>) -> Result<PTy, Why> {
        #[derive(Clone, Copy)]
        enum S {
            Visit(lf::TypeId),
            Make(lf::TypeId, usize),
        }
        let mut todo: SmallPath<S, 32> = SmallPath::new(S::Visit(lf::TypeId(0)));
        let mut done: SmallPath<PTy, 32> = SmallPath::new(PTy(0));
        let mut kids: SmallPath<lf::TypeId, 16> = SmallPath::new(lf::TypeId(0));
        todo.push(S::Visit(t));
        while let Some(step) = todo.pop() {
            match step {
                S::Visit(t) => {
                    kids.clear();
                    children_into(v, t, &mut |k| kids.push(k))?;
                    todo.push(S::Make(t, kids.len()));
                    for &k in kids.iter().rev() {
                        todo.push(S::Visit(k));
                    }
                }
                S::Make(t, nkids) => {
                    let start = done.len() - nkids;
                    let mut args = None;
                    let made = if matches!(v.ty(t)?, lf::TypeRef::Macro(_)) {
                        let r = place(v, v.type_range(t))?;
                        self.macro_ty(tys, env, v, t, r, sink.as_deref_mut())?
                    } else {
                        let made = self.make(tys, env, v, t, &done[start..], &mut args)?;
                        if let Some(s) = sink.as_deref_mut() {
                            self.regions_made(tys, env, v, t, made, &done[start..], args.as_deref().unwrap_or(&[]), s)?;
                        }
                        made
                    };
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
        self.const_value_bits_owned_typed(env, text, text, r, ty, None, None).map(|(bits, _)| bits)
    }

    #[expect(clippy::too_many_arguments, reason = "const value evaluation carrying original text, head owner, and substitution edits")]
    fn const_value_bits_owned_typed(
        &self,
        env: &Env<'_>,
        orig_source: &str,
        text: &str,
        r: Range,
        ty: Option<PNode>,
        carried_owner: Option<(super::anon::HeadOwner, Option<&str>, Option<super::anon::HeadSite<'n>>)>,
        carried_edits: Option<blaze_own::AVec<'static, super::anon::TextEdit>>,
    ) -> Result<(u128, Option<PNode>), Why> {
        let text = text.trim();
        let orig_source = orig_source.trim();
        let other = || Why::ConstPath(Box::from(text), r);
        let (abs, rest) = match text.strip_prefix("::") {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let mut segs: SmallPath<&str, 8> = SmallPath::new("");
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
        let owner_site = carried_owner.or_else(|| self.head_owner_of(orig_source));
        let active_marks = self.marks_for(orig_source);
        let need_edits = owner_site.is_some() || active_marks.is_some();
        blaze_mem::frame::task(blaze_mem::Tag::Parse, |frame| {
        let scratch = blaze_own::Erased::new(frame);
        let mut qpath_edits = need_edits.then(|| self.heads.mark_alloc()).flatten().map(blaze_own::AVec::new_in);
        let with_values = match super::const_qpath::substitute_mapped(self.heads, &resolve_ty, text, scratch.handle(), qpath_edits.as_mut()) {
            Ok(done) => done,
            Err(super::const_qpath::Stop::Need(need)) => return Err(Why::NeedConst(need, r)),
            Err(super::const_qpath::Stop::Failed { start, end, why }) => {
                let leaf = text.get(start..end).unwrap_or(text);
                return Err(Why::ConstPath(format!("{leaf}: {why}").into_boxed_str(), r));
            }
        };
        let source = text;
        let text = match &with_values {
            Some(done) => done.as_str(),
            None => text,
        };
        if plain {
            // A constant item's path resolves where its first token's mark says, as every
            // written path of a macro-written head does (item_site.rs `written_site`): a
            // `$crate::N` another crate's macro wrote is that crate's `N`.
            let first = segs.first().copied().unwrap_or("");
            let mark = match active_marks.map(|m| m.mark_of(first)) {
                Some(Ok(mark)) => mark,
                Some(Err(miss)) if first == "crate" => return Err(program(format!("the constant path `{source}`: {miss}"), r)),
                Some(Err(_)) | None => None,
            };
            let site = super::item_site::marked_site(self.resolver.names, mark, first, abs, env.module, r)?;
            let found = self.resolver.path_with(site.module, site.dollar_crate, abs, &segs, Ns::Value).map_err(|e| Why::Resolve(e, r))?;
            if found.kind == Kind::Const && found.consumed as usize == segs.len() {
                // Tried and refused by the evaluator (front2/tables/head.rs `freeze_seeded`
                // files it, as for an anonymous constant below): this head's refusal by name.
                if let (None, Some(why)) = (self.heads.const_bits(found.key), self.heads.anon_failed(found.key)) {
                    return Err(Why::ConstPath(format!("{text}: {why}").into_boxed_str(), r));
                }
                let bits = self.heads.const_bits(found.key).ok_or(Why::NeedConst(found.key, r))?;
                return Ok((bits, self.heads.const_ty(found.key)));
            }
        }
        let Some(mut ask) = ty.and_then(|ty| super::anon::Ask::of(env.module, ty, text)) else { return Err(other()) };
        if let Some(alloc) = self.heads.mark_alloc() {
            let final_edits = match (carried_edits, qpath_edits) {
                (Some(first_edits), Some(second_edits)) if with_values.is_some() && !second_edits.is_empty() => {
                    if first_edits.is_empty() {
                        Some(second_edits)
                    } else {
                        let mut merged = blaze_own::AVec::with_capacity_in(first_edits.len() + second_edits.len(), alloc);
                        super::anon::compose_edits(&first_edits, &second_edits, &mut merged).map(|()| merged)
                    }
                }
                (None, Some(second_edits)) if with_values.is_some() => Some(second_edits),
                (Some(first_edits), _) => Some(first_edits),
                (None, _) => Some(blaze_own::AVec::new_in(alloc)),
            };
            // A macro-written head's expression is lowered with its tokens' marks (anon.rs
            // `head_marks_with_edits`), and keyed by them (`Ask::carry_marks`): zerocopy's
            // `{ $crate::cryptocorrosion_derive_traits!(..) }` in an impl it writes in ppv-lite86,
            // including after inherent-constant and/or qualified-constant substitution.
            let carried_marks = if let Some(marks) = active_marks {
                let mark_edits = final_edits.as_deref().unwrap_or(&[]);
                match super::anon::head_marks_with_edits(marks, orig_source, text, mark_edits, alloc, r)? {
                    Some(rows) => Some(rows),
                    // Not the item's text: its marks are not read here. Only a `crate` would be
                    // misread without them (a `macro_rules!` expansion's other paths resolve at the
                    // call site, which is the head's module): refused by name.
                    None if super::anon::names_crate(text) => {
                        return Err(Why::Unsupported("a `crate` path in a constant expression of a macro-written head whose text is not the item's (its tokens' marks are not read: types.rs `const_value_bits`)", r));
                    }
                    None => None,
                }
            } else {
                None
            };
            if let (Some((owner, exp_text, site)), Some(edits)) = (owner_site, final_edits) {
                ask = ask.in_head_item(owner, exp_text, site, edits, alloc);
            }
            if let Some(rows) = carried_marks {
                ask = ask.carry_marks(rows);
            }
        }
        if let Some(bits) = self.heads.const_bits(ask.need) {
            return Ok((bits, self.heads.const_ty(ask.need)));
        }
        // Tried and refused by the evaluator (`size_of::<u8>()` where a call it needs does not
        // check): the head is refused by the expression's name with the evaluator's reason, as
        // an expression was before there was an evaluator behind the heads; never asked again.
        if let Some(why) = self.heads.anon_failed(ask.need) {
            return Err(Why::ConstPath(format!("{text}: {why}").into_boxed_str(), r));
        }
        let need = ask.need;
        if self.heads.ask_anon(ask) { Err(Why::NeedConst(need, r)) } else { Err(other()) }
        })
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
        let orig_text = text;
        let orig_owner = self.head_owner_of(orig_text);
        let active_marks = self.marks_for(orig_text);
        let need_edits = orig_owner.is_some() || active_marks.is_some();
        let mut inherent_edits = need_edits.then(|| self.heads.mark_alloc()).flatten().map(blaze_own::AVec::new_in);
        // The substituted text is kept in the allocator of the type table this call holds.
        let substituted = match super::const_inherent::substitute_mapped(self.heads, &inherent_resolve, self_adt, text, tys.alloc(), inherent_edits.as_mut()) {
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
            if let Some(value) = literal_value(tys, text, ty, self.resolver.names, env.module, r)? {
                return Ok(value);
            }
        }
        // Any other constant: a constant item's value or an anonymous constant's, in `ty`'s
        // width (`ty`'s node says how the anonymous one is typed; an unknown type, a body's
        // `_`, takes a constant item's path or an expression whose type is its own, filed
        // with its type left to the checker: anon.rs `written_ty`).
        let node = tys.view().get(ty).copied();
        // An expression that names `Self` where `Self` is a ground type has a value here
        // (`[u8; size_of::<Self>()]` in `impl u16`: front2/const_open.rs, state one): its
        // anonymous constant is filed with that `Self` in scope. Every other expression is
        // read as before; one over a generic parameter is refused there by name.
        let (bits, eval_ty) = match super::const_open::ground_self(tys.view(), env, text) {
            Some((self_ty, self_node)) => super::anon::self_value_typed(self.heads, env.module, node, orig_text, text, self_ty, self_node, active_marks, orig_owner, inherent_edits, r)?,
            None => self.const_value_bits_owned_typed(env, orig_text, text, r, node, orig_owner, inherent_edits)?,
        };
        let ty = if node == Some(PNode::Infer) {
            match eval_ty {
                Some(prim) => intern(tys, prim)?,
                None => ty,
            }
        } else {
            ty
        };
        intern(tys, PNode::ConstValue { ty, bits })
    }

    /// The declared type of the const parameter that argument `index` of `target` fills:
    /// - a regular item path (`Foo<A>`, `Trait<A>`): its head's [`Heads::const_params`] (a
    ///   trait's arguments follow `Self`), resolved where the path's first token's mark says
    ///   ([`super::item_site::resolve_written`]);
    /// - a generic associated type on a type parameter (`F::Config<A>`, `<F>::Config<A>`), on
    ///   `Self` (`Self::Config<A>`, `<Self>::Config<A>`), or on a qualified trait path
    ///   (`<T as Tr>::Config<A>`): the declaring trait's [`Heads::assoc_const_params`]
    ///   ([`Builder::gat_const_want`]).
    #[expect(clippy::too_many_arguments, reason = "one const parameter lookup: the table, env, view, target and site")]
    fn const_want(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, target: ConstTarget<'_>, index: u32, r: Range) -> Result<PTy, Why> {
        match target {
            ConstTarget::Path { p, abs, texts } => {
                let found = self.resolve_at(env, v, p, abs, texts, Ns::Type, r)?;
                if usize::try_from(found.consumed).ok() != Some(texts.len()) {
                    return Err(Why::Unsupported("a const argument on an associated item's path", r));
                }
                let at = if matches!(found.kind, Kind::Trait | Kind::TraitAlias) { index + 1 } else { index };
                let consts = self.heads.const_params(found.key).ok_or(Why::Need(found.key))?;
                match consts.iter().find(|(i, _)| *i == at) {
                    Some(&(_, t)) => Ok(t),
                    None => Err(program(format!("argument {} of {} is a constant; that parameter is not a const parameter", index + 1, texts.join("::")), r)),
                }
            }
            ConstTarget::ParamAssoc { param, name, shown } => {
                let (bounds, done) = match env.bounds {
                    Bounds::Building(b) => (b, false),
                    Bounds::Done(b) => (b, true),
                };
                let mut found: blaze_own::AVec<'_, (LibKey, List)> = blaze_own::AVec::new_in(tys.alloc());
                for b in bounds.iter().filter(|b| b.param == param) {
                    if let Some(owner) = self.assoc_owner(tys, b.trait_, b.args, name, r, shown)? {
                        if !found.iter().any(|x| *x == owner) {
                            found.push(owner);
                        }
                    }
                }
                match found.as_slice() {
                    &[(owner, _)] => self.gat_const_want(owner, name, index, r, shown),
                    [] if !done => Err(Why::Unsupported("`T::X` in a bound written before the bound of `T` that declares `X`", r)),
                    [] => Err(Why::AssocNotFound(shown.into(), r)),
                    many => {
                        let traits = many.iter().map(|&(t, _)| t).collect();
                        Err(Why::AssocAmbiguous(shown.into(), traits, r))
                    }
                }
            }
            ConstTarget::SelfAssoc { name, shown } => match env.self_ {
                SelfTy::Trait(tr) => {
                    let (g, _) = self.heads.trait_head(tr).ok_or(Why::Need(tr))?;
                    let mut args: blaze_own::AVec<'_, PTy> = blaze_own::AVec::with_capacity_in(usize::try_from(g).unwrap_or(0), tys.alloc());
                    for i in 0..g {
                        args.push(intern(tys, PNode::Param(i))?);
                    }
                    let args = intern_list(tys, &args)?;
                    let mut found: blaze_own::AVec<'_, (LibKey, List)> = blaze_own::AVec::new_in(tys.alloc());
                    if let Some(owner) = self.assoc_owner(tys, tr, args, name, r, shown)? {
                        found.push(owner);
                    }
                    let (Bounds::Building(bounds) | Bounds::Done(bounds)) = env.bounds;
                    for b in bounds.iter().filter(|b| b.param == 0) {
                        if let Some(owner) = self.assoc_owner(tys, b.trait_, b.args, name, r, shown)? {
                            if !found.iter().any(|x| *x == owner) {
                                found.push(owner);
                            }
                        }
                    }
                    match found.as_slice() {
                        &[(owner, _)] => self.gat_const_want(owner, name, index, r, shown),
                        [] => Err(Why::Program(blaze_own::format_in!(tys.alloc(), "{shown} names no associated type of the trait, its supertraits or a bound on `Self` in scope").as_str().into(), r)),
                        many => {
                            let traits = many.iter().map(|&(t, _)| t).collect();
                            Err(Why::AssocAmbiguous(shown.into(), traits, r))
                        }
                    }
                }
                SelfTy::Ty(_, Some((tr, args))) => match self.assoc_owner(tys, tr, args, name, r, shown)? {
                    Some((owner, _)) => self.gat_const_want(owner, name, index, r, shown),
                    None => Err(Why::Program(blaze_own::format_in!(tys.alloc(), "{shown} names no associated type of the trait or its supertraits").as_str().into(), r)),
                },
                SelfTy::Ty(_, None) => Err(Why::Unsupported("type or const arguments on an inherent associated type", r)),
                SelfTy::None => Err(Why::Unsupported(SELF_HERE, r)),
            },
            ConstTarget::TraitAssoc { trait_path, name, shown } => {
                let (tabs, ttexts, _, tr_r) = path_texts(v, trait_path)?;
                let Bounded::Trait(tr) = self.bound_target(env, v, trait_path, tabs, &ttexts, tr_r)? else {
                    return Err(Why::Program(blaze_own::format_in!(tys.alloc(), "`{shown}`: trait alias stands for bounds, not one trait").as_str().into(), tr_r));
                };
                let Some(owner) = self.assoc_owner_key(tr, name, r, shown, tys.alloc())? else {
                    return Err(Why::Program(blaze_own::format_in!(tys.alloc(), "`{shown}` names no associated type of the trait").as_str().into(), r));
                };
                self.gat_const_want(owner, name, index, r, shown)
            }
        }
    }

    /// The trait declaring associated type `name` among `tr` and its supertraits (by key only,
    /// without substituting arguments): used by [`Builder::const_want`] for `<T as Tr>::X<..>`.
    fn assoc_owner_key(&self, tr: LibKey, name: Name, r: Range, shown: &str, alloc: Alloc<'_>) -> Result<Option<LibKey>, Why> {
        const MOST_SUPERS: usize = 256;
        let mut todo: blaze_own::AVec<'_, LibKey> = blaze_own::AVec::new_in(alloc);
        todo.push(tr);
        let mut found: Option<LibKey> = None;
        let mut seen: blaze_own::AVec<'_, LibKey> = blaze_own::AVec::new_in(alloc);
        for _ in 0..MOST_SUPERS {
            let Some(t) = todo.pop() else { break };
            if seen.iter().any(|x| *x == t) {
                continue;
            }
            seen.push(t);
            if self.heads.trait_has_assoc(t, name).ok_or(Why::Need(t))? {
                match found {
                    Some(prev) if prev != t => return Err(Why::Program(blaze_own::format_in!(alloc, "`{shown}` is ambiguous: two supertraits declare it").as_str().into(), r)),
                    _ => found = Some(t),
                }
                continue;
            }
            for &(st, _) in self.heads.trait_supers(t).ok_or(Why::Need(t))? {
                todo.push(st);
            }
        }
        if !todo.is_empty() {
            return Err(Why::Unsupported("a supertrait tree past 256 traits", r));
        }
        Ok(found)
    }

    /// A path's generic arguments in written order: each type argument from `kids` (as
    /// [`children`] listed them: a qualified path's self type first), each const argument
    /// and each one-name argument (`N`: a type if the name is a type parameter or resolves in
    /// the type namespace, else a constant; the Reference, "Generic parameters") made here.
    pub(super) fn path_args(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, p: PathIdx, kids: &[PTy]) -> Result<SmallPath<PTy, 8>, Why> {
        let r = place(v, v.path_range(p))?;
        let shown = v.text_at(v.path_range(p)?)?;
        let mut out: SmallPath<PTy, 8> = SmallPath::new(PTy(0));
        let mut next = kids.iter().copied();
        let (abs, texts, segs, _) = path_texts_of(v, p, true)?;
        let handle_seg = |tys: &mut PortableTys<'_>,
                          out: &mut SmallPath<PTy, 8>,
                          next: &mut core::iter::Copied<core::slice::Iter<'_, PTy>>,
                          s: lf::Segment,
                          target: ConstTarget<'_>|
         -> Result<(), Why> {
            if !s.has_args {
                return Ok(());
            }
            if s.parenthesized {
                for &n in v.rows(s.args)? {
                    if let lf::GenericArg::Type(_) = lf::GenericArg::decode(n)? {
                        out.push(next.next().ok_or_else(|| Why::Program("a type argument is missing".into(), r))?);
                    }
                }
                if s.ty.is_some() {
                    out.push(next.next().ok_or_else(|| Why::Program("a type argument is missing".into(), r))?);
                }
                return Ok(());
            }
            let mut index = 0u32;
            for &n in v.rows(s.args)? {
                let made = match lf::GenericArg::decode(n)? {
                    lf::GenericArg::Type(t) => match bare_name(v, t)? {
                        None => next.next().ok_or_else(|| Why::Program("a type argument is missing".into(), r))?,
                        Some(name) => {
                            let param = env.params.iter().find(|(n, _)| *n == Name::of(name)).map(|&(_, i)| i);
                            let is_const = param.is_some_and(|i| env.consts.iter().any(|(c, _)| *c == i));
                            let lf::TypeRef::Path(tp) = v.ty(t)? else { return Err(Why::Program("a one-name argument that is no path".into(), r)) };
                            let ar = place(v, v.type_range(t))?;
                            let allow_bare_trait = self.allows_bare_trait_object_at(env, v, tp, ar)?;
                            // An item the body declares takes part as in a body path's own
                            // argument (body.rs `name_arg`; the one rule is const_open.rs
                            // `local_arg`): the Reference, "Generic parameters": a one-name
                            // argument is a type if it resolves to one in the type
                            // namespace, else a const; "Statements", "Item declarations":
                            // a block's item is in scope in its block. tempo's
                            // `Exec::for_config::<Config<SPEC_ID>>()` names the `const
                            // SPEC_ID` of its block; read at the module alone, it was
                            // "cannot find type `SPEC_ID`".
                            let local = match env.body {
                                Some(body) => super::const_open::local_arg(&self.resolver, self.marks, body.locals, body.owner, body.module, self.heads, v, t, name, ar),
                                None => Ok(None),
                            };
                            // The name resolves where its token's mark says, as the
                            // argument's own type would (`path_ty`): never at the module
                            // alone when a macro wrote it.
                            let at_mark = |ns: Ns| self.resolve_at(env, v, tp, false, &[name], ns, ar);
                            // A type if a type parameter, or the names library's answer in
                            // the type namespace is a type (a constant item answers there
                            // too, as the library files it: then it is a constant). In
                            // Rust 2015/2018 a trait also names a bare trait object type.
                            let names_a_type = || match at_mark(Ns::Type) {
                                Ok(f) => {
                                    f.consumed == 1
                                        && (matches!(f.kind, Kind::Struct | Kind::Enum | Kind::Union | Kind::TypeAlias | Kind::Builtin(_) | Kind::WideInt { .. })
                                            || (allow_bare_trait && matches!(f.kind, Kind::Trait | Kind::TraitAlias)))
                                }
                                Err(_) => false,
                            };
                            let local_type = matches!(local, Ok(Some(super::const_open::LocalArg::Type)));
                            let is_type = local_type || (!is_const && (param.is_some() || names_a_type()));
                            if is_type {
                                self.path_ty(tys, env, v, tp, &[])?
                            } else if let Ok(Some(super::const_open::LocalArg::Const(bits, eval_ty))) = local {
                                // The constant's bits in its evaluated primitive type (falling
                                // back to the parameter's expected type only when evaluation
                                // carried no primitive type), as `const_term` does for module
                                // constants. A scalar parameter only: a fieldless enum's value
                                // is its variant under the enum's key (const_enum.rs), which a
                                // body's constant's bits are not.
                                let ty = self.const_want(tys, env, v, target, index, ar)?;
                                if !matches!(tys.view().get(ty), Some(PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Infer | PNode::Error)) {
                                    return Err(Why::Unsupported("a const argument naming a `const` the body declares, for a parameter whose type is no integer, `bool` or `char`", ar));
                                }
                                let val_ty = match eval_ty {
                                    Some(prim) => intern(tys, prim)?,
                                    None => ty,
                                };
                                intern(tys, PNode::ConstValue { ty: val_ty, bits })?
                            } else if let Err(why) = local {
                                // A body's constant not evaluated yet (`Why::NeedConst`: the
                                // driver evaluates it and lowers again), or a body's value
                                // item that is no constant: refused by name, never read at
                                // the module, where the name would mean another item.
                                return Err(why);
                            } else {
                                // A name that is no parameter, no type and no value here
                                // names nothing: refused as unresolved, never guessed to be
                                // a constant (zerocopy's `HasField<Client, value, ..>`, whose
                                // `value` is an enum of the impl's own block, was refused
                                // "argument 2 of `HasField` is a constant").
                                if param.is_none() && name != "true" && name != "false" {
                                    at_mark(Ns::Value)?;
                                }
                                let want = |tys: &mut PortableTys<'_>| self.const_want(tys, env, v, target, index, ar);
                                self.const_term(tys, env, name, ar, &want)?
                            }
                        }
                    },
                    lf::GenericArg::Const(c) => {
                        let at = v.const_range(c)?;
                        let ar = v.site(at)?;
                        let text = v.text_at(at)?;
                        let want = |tys: &mut PortableTys<'_>| self.const_want(tys, env, v, target, index, ar);
                        self.const_term(tys, env, text, ar, &want)?
                    }
                    lf::GenericArg::AssocType { args, .. } if args.len == 0 => {
                        let made = next.next().ok_or_else(|| Why::Program("a type argument is missing".into(), r))?;
                        out.push(made);
                        continue;
                    }
                    _ => continue,
                };
                out.push(made);
                index += 1;
            }
            Ok(())
        };
        for (k, &s) in segs.iter().enumerate() {
            if s.kind == lf::SegKind::Anchor {
                out.push(next.next().ok_or(program(format!("a qualified path's self type is missing"), r))?);
                // `<T>::X` has no trait whose arguments are listed (`children`).
                if let Some(tr) = s.trait_ {
                    let lf::TypeRef::Path(tp) = v.ty(tr)? else { return Err(program(format!("a trait that is no path"), r)) };
                    let (tabs, ttexts, tsegs, _) = path_texts(v, tp)?;
                    for &ts in tsegs.iter() {
                        handle_seg(tys, &mut out, &mut next, ts, ConstTarget::Path { p: tp, abs: tabs, texts: &ttexts })?;
                    }
                }
            } else if s.has_args
                && let Some(upto) = texts.get(..=k)
            {
                let first_seg = segs.first().copied();
                let target = if k == 1
                    && let Some(first) = first_seg
                    && first.kind == lf::SegKind::Anchor
                {
                    if let Some(n) = s.name {
                        let name = Name(v.name(n)?);
                        if let Some(tr) = first.trait_
                            && let lf::TypeRef::Path(tp) = v.ty(tr)?
                        {
                            ConstTarget::TraitAssoc { trait_path: tp, name, shown }
                        } else if first.trait_.is_none() {
                            let is_self = match first.ty {
                                Some(written) => match v.ty(written)? {
                                    lf::TypeRef::Path(sp) => {
                                        let (sabs, stexts, _, _) = path_texts(v, sp)?;
                                        !sabs && stexts.as_slice() == ["Self"]
                                    }
                                    _ => false,
                                },
                                None => false,
                            };
                            if is_self {
                                ConstTarget::SelfAssoc { name, shown }
                            } else if let Some(PNode::Param(param)) = out.first().and_then(|&t| tys.view().get(t).copied()) {
                                ConstTarget::ParamAssoc { param, name, shown }
                            } else {
                                ConstTarget::Path { p, abs, texts: upto }
                            }
                        } else {
                            ConstTarget::Path { p, abs, texts: upto }
                        }
                    } else {
                        ConstTarget::Path { p, abs, texts: upto }
                    }
                } else if k == 1
                    && !abs
                    && let [first_text, second_text] = texts.as_slice()
                    && *first_text == "Self"
                {
                    ConstTarget::SelfAssoc { name: Name::of(second_text), shown }
                } else if k == 1
                    && !abs
                    && let [first_text, second_text] = texts.as_slice()
                    && first_seg.is_some_and(|first| !first.has_args)
                    && let Some(&(_, param)) = env.params.iter().find(|(n, _)| *n == Name::of(first_text))
                    && !env.consts.iter().any(|(c, _)| *c == param)
                {
                    ConstTarget::ParamAssoc { param, name: Name::of(second_text), shown }
                } else {
                    ConstTarget::Path { p, abs, texts: upto }
                };
                handle_seg(tys, &mut out, &mut next, s, target)?;
            }
        }
        if next.next().is_some() {
            return Err(program(format!("a type argument is left over"), r));
        }
        Ok(out)
    }

    /// The type `t` made of its children `kids`; a path's arguments (`path_args`' answer) are
    /// left in `args` for its region slots ([`Builder::regions_made`]).
    fn make(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, t: lf::TypeId, kids: &[PTy], args: &mut Option<SmallPath<PTy, 8>>) -> Result<PTy, Why> {
        let r = place(v, v.type_range(t))?;
        match v.ty(t)? {
            // The written lifetime is the reference's region slot (`regions_made`), never
            // part of the erased type.
            lf::TypeRef::Ref { mutbl, .. } => intern(tys, PNode::Ref(mutbl, kids[0])),
            lf::TypeRef::Ptr { mutbl, .. } => intern(tys, PNode::Ptr(mutbl, kids[0])),
            lf::TypeRef::Slice(_) => intern(tys, PNode::Slice(kids[0])),
            lf::TypeRef::Tuple(_) => {
                let l = intern_list(tys, kids)?;
                intern(tys, PNode::Tuple(l))
            }
            lf::TypeRef::Record(fields) => {
                let rows = v.rows(fields)?;
                let alloc = tys.alloc();
                // Merged fields `(field_text, Name, PTy)`. Spreads (`f.name.is_none()`) expand
                // left-to-right, and later spreads or explicit fields override earlier ones.
                let mut merged: blaze_own::Vec<(&str, Name, PTy), Alloc<'_>> = blaze_own::Vec::with_capacity_in(rows.len(), alloc);
                let mut has_default = false;
                let mut spread_defaults_tag: u64 = 0;
                for (i, &n) in rows.iter().enumerate() {
                    let f = lf::Field::decode(n)?;
                    if f.default.is_some() {
                        has_default = true;
                    }
                    let Some(&kid_ty) = kids.get(i) else {
                        return Err(Why::Unsupported("record field count mismatch", r));
                    };
                    match f.name {
                        Some(nm) => {
                            let text = v.name_text(nm)?;
                            let name = Name::of(text);
                            self.heads.note_record_field_name(name, text);
                            if let Some(slot) = merged.iter_mut().find(|(_, existing, _)| *existing == name) {
                                *slot = (text, name, kid_ty);
                            } else {
                                merged.push((text, name, kid_ty));
                            }
                        }
                        None => {
                            let tv = tys.view();
                            let Some(PNode::Record(base_schema, base_list)) = tv.get(kid_ty).copied() else {
                                let spread_r = place(v, v.type_range(f.ty)).unwrap_or(r);
                                return Err(Why::Unsupported("record type spread (`..Base`) requires a structural record type", spread_r));
                            };
                            let base_tag = tv.record_defaults_tag(kid_ty);
                            if base_tag != 0 {
                                spread_defaults_tag = base_tag;
                            }
                            let base_names = tv
                                .record_names(base_schema)
                                .ok_or(Why::Unsupported("record type spread target has no registered schema", r))?;
                            let base_tys = tv.list(base_list).map_err(|_| table_full())?;
                            if base_names.len() != base_tys.len() {
                                return Err(Why::Unsupported("record type spread field count mismatch", r));
                            }
                            for (&bnm, &bty) in base_names.iter().zip(base_tys.iter()) {
                                let btext = self
                                    .heads
                                    .record_field_name(bnm)
                                    .ok_or(Why::Unsupported("record type spread field name is not registered", r))?;
                                if let Some(slot) = merged.iter_mut().find(|(_, existing, _)| *existing == bnm) {
                                    *slot = (btext, bnm, bty);
                                } else {
                                    merged.push((btext, bnm, bty));
                                }
                            }
                        }
                    }
                }
                merged.sort_by(|a, b| a.0.cmp(b.0));
                let mut sorted_names = blaze_own::Vec::with_capacity_in(merged.len(), alloc);
                let mut sorted_tys = blaze_own::Vec::with_capacity_in(merged.len(), alloc);
                for &(_, nm, fty) in merged.iter() {
                    sorted_names.push(nm);
                    sorted_tys.push(fty);
                }
                let schema = tc::RecordSchema::of_sorted_names(&sorted_names);
                tys.register_record_schema(schema, &sorted_names).map_err(|_| table_full())?;
                let l = intern_list(tys, &sorted_tys)?;
                let defaults_tag = if has_default || spread_defaults_tag != 0 {
                    let owner_ck = self
                        .owner_key
                        .or_else(|| self.owner.map(|(k, _)| super::keys::checker_of(k)))
                        .or_else(|| env.body.map(|bs| bs.owner));
                    match owner_ck {
                        Some(ock) => super::heads::Store::scoped_record_defaults_tag(ock, schema, t.0, alloc),
                        None => spread_defaults_tag,
                    }
                } else {
                    0
                };
                tys.intern_record_with_defaults(schema, l, defaults_tag).map_err(|_| table_full())
            }
            lf::TypeRef::Union(_) => {
                let mut flat = blaze_own::Vec::with_capacity_in(kids.len(), tys.alloc());
                for &kid in kids {
                    if let Some(PNode::Union(inner)) = tys.view().get(kid).copied() {
                        if let Ok(slice) = tys.view().list(inner) {
                            for &member in slice {
                                if !flat.iter().any(|&seen| seen == member) {
                                    flat.push(member);
                                }
                            }
                        }
                    } else if !flat.iter().any(|&seen| seen == kid) {
                        flat.push(kid);
                    }
                }
                let l = intern_list(tys, &flat)?;
                intern(tys, PNode::Union(l))
            }
            lf::TypeRef::Never => intern(tys, PNode::Never),
            lf::TypeRef::Infer => intern(tys, PNode::Infer),
            lf::TypeRef::Path(p) => {
                let made_args = self.path_args(tys, env, v, p, kids)?;
                let made = self.path_ty(tys, env, v, p, &made_args)?;
                *args = Some(made_args);
                Ok(made)
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
                    None => match body_const_len(&self.resolver, self.marks, self.heads, env, v, t, text, lr)? {
                        Some(bits) => intern(tys, PNode::ConstValue { ty: usize_ty, bits })?,
                        None => self.const_term(tys, env, text, lr, &|_| Ok(usize_ty))?,
                    },
                };
                intern(tys, PNode::Array(kids[0], n))
            }
            // `[unsafe] [extern "abi"] fn(A, B) -> R`: the inputs, then the output (`()` when
            // none is written), as `children` listed them. A C-variadic one (`fn(A, ...)`) lists
            // its named inputs and says `...` in its header: the Reference, "Function pointer
            // types", allows `...` last on an extern fn pointer type of a calling convention
            // that has variadics ([items.extern.variadic.conventions], the set `heads` checks
            // for a fn item's `...`); any other ABI is the language's E0045.
            lf::TypeRef::FnPtr { ret, abi, unsafety, extern_, variadic, .. } => {
                let abi = match (abi, extern_) {
                    (None, false) => tc::Abi::Rust,
                    (None, true) => tc::Abi::C,
                    (Some(at), _) => fn_ptr_abi(v.text_at(at)?, r)?,
                };
                if variadic && !abi.takes_c_variadic() {
                    return Err(program(format!("`...` on a fn pointer type of the \"{}\" ABI: that calling convention has no variadics (E0045)", abi.name()), r));
                }
                let safety = if unsafety { tc::Safety::Unsafe } else { tc::Safety::Safe };
                let mut l = blaze_own::Vec::with_capacity_in(kids.len() + 1, tys.alloc());
                l.extend_from_slice(kids);
                if ret.is_none() {
                    let unit = intern_list(tys, &[])?;
                    l.push(intern(tys, PNode::Tuple(unit))?);
                }
                let l = intern_list(tys, &l)?;
                intern(tys, PNode::FnPtr(l, tc::FnPtrHeader { abi, safety, c_variadic: variadic }))
            }
            lf::TypeRef::Macro(_) => self.macro_ty(tys, env, v, t, r, None),
            lf::TypeRef::Pat { pat, .. } => self.pat_ty(tys, env, v, kids[0], pat, r),
            // `for<'a> fn(&'a T)`: the binder's lifetimes are erased ([`super::preds::lifetimes_only`],
            // checked where `children` lists the inner type), so the type is its inner type.
            lf::TypeRef::For { .. } => Ok(kids[0]),
            lf::TypeRef::Impl(_) => match env.anon.iter().find(|(at, _)| *at == t) {
                Some(&(_, ty)) => Ok(ty),
                // Neither in a fn's parameter types nor in its return type nor in a type
                // alias's type (super::tait: each of those is the alias's opaque): a field, a
                // `let`, an `impl Trait` inside a fn pointer. Refused by name.
                None => Err(Why::Unsupported("an `impl Trait` type outside a fn's parameters and return type", r)),
            },
            _ => Err(Why::Unsupported("this type", r)),
        }
    }

    /// Resolves a written path in `ns` by the block and first-token rule in
    /// [`super::scope_table::written`]. Item heads read the first token's site directly.
    #[expect(clippy::too_many_arguments, reason = "one resolve of one path: the path, its view and its env")]
    fn resolve_at(&self, env: &Env<'_>, v: &View<'_>, p: PathIdx, abs: bool, texts: &[&str], ns: Ns, r: Range) -> Result<crate::resolve::Target, Why> {
        if let Some(body) = env.body
            && let Some(found) = super::scope_table::written(&self.resolver, self.marks, &body, v, p, abs, texts, ns, r)?
        {
            return Ok(found);
        }
        super::item_site::resolve_written(&self.resolver, self.marks, env.module, v, p, abs, texts, ns, r)
    }

    /// Whether path `p` of `v` allows bare trait object types without an explicit `dyn` keyword
    /// (Rust 2015 and Rust 2018; hard error E0782 since Rust 2021). Reads the path token's
    /// edition mark when present (`v.mark_of_path(p)` or `self.marks`), falling back to
    /// `env.module`'s crate edition for unmarked tokens.
    pub(super) fn allows_bare_trait_object_at(&self, env: &Env<'_>, v: &View<'_>, p: PathIdx, r: Range) -> Result<bool, Why> {
        let edition = match super::item_site::path_mark(self.marks, v, p, r)? {
            Some(mark) => Some(mark.edition),
            None => self.resolver.names.crate_of(env.module).and_then(|c| self.resolver.names.edition_of(c)),
        };
        Ok(edition.is_some_and(|e| matches!(e, blaze_surface::Edition::E2015 | blaze_surface::Edition::E2018)))
    }

    fn path_ty(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, p: PathIdx, kids: &[PTy]) -> Result<PTy, Why> {
        let pr = place(v, v.path_range(p))?;
        let allow_bare_trait = self.allows_bare_trait_object_at(env, v, p, pr)?;
        let (abs, texts, segs, r) = path_texts_of(v, p, allow_bare_trait)?;
        let shown = v.text_at(v.path_range(p)?)?;
        let parenthesized = segs.last().is_some_and(|s| s.parenthesized);
        if segs[0].kind == lf::SegKind::Anchor {
            if parenthesized {
                return Err(Why::Unsupported("parenthesized generic arguments (`Fn(A) -> B`)", r));
            }
            if has_assoc_arg(v, segs.get(1..).unwrap_or(&[]))? {
                return Err(Why::Program("associated item constraints are not allowed on this path (E0229)".into(), r));
            }
            let [_, last] = segs.as_slice() else {
                return Err(Why::Unsupported("a qualified path of more than one item", r));
            };
            let Some(tr) = segs[0].trait_ else {
                return self.qualified_no_trait(tys, env, v, &segs[0], last, kids, r, shown);
            };
            let lf::TypeRef::Path(tp) = v.ty(tr)? else { return Err(program(format!("a trait that is no path"), r)) };
            let (_, _, tsegs, _) = path_texts_of(v, tp, true)?;
            if has_assoc_arg(v, &tsegs)? {
                return Err(Why::Program("associated item constraints are not allowed on a qualified path's trait (E0229)".into(), r));
            }
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
            if parenthesized {
                return Err(Why::Unsupported("parenthesized generic arguments (`Fn(A) -> B`)", r));
            }
            if has_assoc_arg(v, &segs)? {
                return Err(Why::Program("associated item constraints are not allowed on `Self` (E0229)".into(), r));
            }
            return match (texts.len(), env.self_) {
                (1, SelfTy::Trait(_)) => intern(tys, PNode::Param(0)),
                (1, SelfTy::Ty(t, _)) => Ok(t),
                (2, _) => self.self_assoc_path(tys, env, v, Name::of(texts[1]), kids, r, shown),
                _ => Err(Why::Unsupported(SELF_HERE, r)),
            };
        }
        // A block-local type item or `use` shadows an enclosing generic parameter of the same
        // name (the Reference, "Statements", "Item declarations"; matching `body.rs` `name_arg`).
        let local_shadows = if !abs
            && (texts.len() == 1 || texts.len() == 2)
            && env.params.iter().any(|(n, _)| *n == Name::of(texts[0]))
            && let Some(body) = env.body
        {
            super::scope_table::written(&self.resolver, self.marks, &body, v, p, abs, &texts, Ns::Type, r)?.is_some()
        } else {
            false
        };
        if !local_shadows && !abs && texts.len() == 1 {
            if let Some(&(_, i)) = env.params.iter().find(|(n, _)| *n == Name::of(texts[0])) {
                if parenthesized {
                    return Err(Why::Unsupported("parenthesized generic arguments (`Fn(A) -> B`)", r));
                }
                if has_assoc_arg(v, &segs)? {
                    return Err(Why::Program("associated item constraints are not allowed on a type parameter (E0229)".into(), r));
                }
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
        if !local_shadows && !abs && texts.len() == 2 {
            if let Some(&(_, i)) = env.params.iter().find(|(n, _)| *n == Name::of(texts[0])) {
                if parenthesized {
                    return Err(Why::Unsupported("parenthesized generic arguments (`Fn(A) -> B`)", r));
                }
                if has_assoc_arg(v, &segs)? {
                    return Err(Why::Program("associated item constraints are not allowed on an associated type path (E0229)".into(), r));
                }
                // `T::X<A>` is `<T as Tr>::X<A>` (the Reference, "Paths", "Paths in types";
                // "Associated Items", generic associated types): `kids` are the arguments
                // written on `X`'s own segment, the parameter's segment taking none.
                if segs[0].has_args {
                    return Err(Why::Unsupported("generic arguments on a type parameter (`T<A>::X`)", r));
                }
                return self.param_assoc(tys, env, v, i, Name::of(texts[1]), kids, r, shown);
            }
        }
        self.resolved_path_ty(tys, env, v, p, &segs, abs, &texts, kids, r, shown)
    }

    /// `<T>::X<own..>`, a qualified path with no trait (the anchor `anchor`, the associated
    /// type's segment `last`; `kids` the self type, then the arguments on `last`): the
    /// type-relative path `T::X` written with its self type in angle brackets. The Reference,
    /// "Paths", "Qualified paths": "Fully qualified paths allow for disambiguating the path for
    /// trait implementations"; with no `as Trait` the type is only bracketed, so the
    /// associated item is looked up on the type as for `T::X` ("Paths in types"; rustc lowers
    /// both forms through one type-relative lookup). For a type parameter that is through its
    /// bounds in scope, E0220 when none declares `X`, E0221 when two do ([`Self::param_assoc`]);
    /// for `Self` it is `Self::X` ([`Self::self_assoc_path`]). chacha20's NEON backend writes it
    /// through a `$variant:ty` fragment: `size_of::<<$variant>::Counter>()` in
    /// `impl<R: Rounds, V: Variant> Backend<R, V>`. Another self type would be an inherent
    /// associated type (unstable `inherent_associated_types`; without it rustc's E0223), refused
    /// by name.
    #[expect(clippy::too_many_arguments, reason = "the path's two segments, its lowered children and where it is written")]
    fn qualified_no_trait(
        &self,
        tys: &mut PortableTys<'_>,
        env: &Env<'_>,
        v: &View<'_>,
        anchor: &lf::Segment,
        last: &lf::Segment,
        kids: &[PTy],
        r: Range,
        shown: &str,
    ) -> Result<PTy, Why> {
        let Some(n) = last.name else { return Err(Why::Unsupported("this qualified path", r)) };
        let name = Name(v.name(n)?);
        let Some((&self_ty, own)) = kids.split_first() else {
            return Err(program(format!("a qualified path's self type is missing"), r));
        };
        let written = anchor.ty.ok_or(program(format!("a qualified path's self type is missing"), r))?;
        if let lf::TypeRef::Path(sp) = v.ty(written)? {
            let (sabs, stexts, _, _) = path_texts(v, sp)?;
            if !sabs && stexts.as_slice() == ["Self"] {
                return self.self_assoc_path(tys, env, v, name, own, r, shown);
            }
        }
        // The self type as lowered: a parameter however it was written (`V`, a `$v:ty`
        // fragment, an alias of a parameter).
        let lowered = tys.view().get(self_ty).copied();
        match lowered {
            Some(PNode::Param(i)) => self.param_assoc(tys, env, v, i, name, own, r, shown),
            _ => Err(Why::Unsupported("`<T>::X` with `T` neither a type parameter nor `Self` (an inherent associated type; rustc's E0223 without `inherent_associated_types`)", r)),
        }
    }

    /// `Self::name<own..>`: in a trait (or its member), through the bounds on `Self` in scope;
    /// in a trait impl, the projection of the impl's trait; in an inherent impl, the impl's own
    /// associated type.
    #[expect(clippy::too_many_arguments, reason = "the associated type's name and arguments and where it is written")]
    fn self_assoc_path(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, name: Name, kids: &[PTy], r: Range, shown: &str) -> Result<PTy, Why> {
        match env.self_ {
            SelfTy::Trait(tr) => {
                let (g, _) = self.heads.trait_head(tr).ok_or(Why::Need(tr))?;
                // The trait's supertraits are filed when its own bounds are lowered (heads.rs
                // `trait_members`); until then `v` IS its header, and a name the trait does not
                // declare is found through the bounds written there ([`super::written`]):
                // asking for the filed supertraits here is the trait's members needing
                // themselves (reth's `trait Call: LoadState<Error: From<Self::Evm>> + ..`, the
                // cycle that stopped tempo's contracts).
                if self.heads.trait_supers(tr).is_none() && !self.heads.trait_has_assoc(tr, name).ok_or(Why::Need(tr))? {
                    return self.written_self_assoc(tys, env, v, name, kids, r, shown);
                }
                let mut args = blaze_own::Vec::with_capacity_in(g as usize, tys.alloc());
                for i in 0..g {
                    args.push(intern(tys, PNode::Param(i))?);
                }
                let args = intern_list(tys, &args)?;
                self.trait_self_assoc(tys, env, tr, args, name, kids, r, shown)
            }
            // In a trait impl, `Self::X` is the PROJECTION `<SelfTy as Tr>::X` of the trait (or
            // the one supertrait) declaring `X`, never the impl's `type X = ..` text (the
            // Reference, "Paths", "`Self`" and "Paths in types"; rustc lowers it so and
            // normalizes it where it is used). Its value comes from normalization in the
            // environment of the use: the impl's type where no where-bound shadows the impl,
            // the rigid alias where one does (core's `rposition` in `impl Iterator for Iter<'a,
            // T>`, `Self: ExactSizeIterator + DoubleEndedIterator`): the same answer on every
            // side. Reading the impl's text here made the bound's `FnMut(Self::Item)` `&'a T`
            // beside a rigid `x`: a false E0308. Arguments on the segment are the associated
            // type's own (`Self::Out<A>`). Core's `impl DoubleEndedIterator for X { fn rfold(..)
            // -> Self::Item }`, `Item` `Iterator`'s, is the supertrait's projection the same
            // way. An inherent impl has no trait: its own associated type is its text
            // (inherent associated types, unstable).
            SelfTy::Ty(_, tr) => match (tr, env.self_assoc.iter().find(|(n, _)| *n == name)) {
                (Some((tr, args)), _) => match self.assoc_owner(tys, tr, args, name, r, shown)? {
                    Some((t, a)) => self.assoc_with_args(tys, t, name, a, kids, r, shown),
                    None => Err(program(format!("`{shown}` names no associated type of the trait or its supertraits"), r)),
                },
                (None, Some(&(_, t))) if kids.is_empty() => Ok(t),
                (None, Some(_)) => Err(Why::Unsupported("type or const arguments on an inherent associated type", r)),
                (None, None) => Err(program(format!("`{shown}` names no associated type of the impl"), r)),
            },
            SelfTy::None => Err(Why::Unsupported(SELF_HERE, r)),
        }
    }

    /// A written path that is no parameter and no `Self` form: what the names library resolves
    /// it to (a nominal type, an alias, a built-in type, or a Rust 2015/2018 bare trait object).
    #[expect(clippy::too_many_arguments, reason = "one resolve of one path: the path, its view and its env")]
    fn resolved_path_ty(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, p: PathIdx, segs: &[lf::Segment], abs: bool, texts: &[&str], kids: &[PTy], r: Range, shown: &str) -> Result<PTy, Why> {
        // A built-in type is the names library's answer (`Kind::Builtin`): front2 has no lookup
        // rule of its own for one. Where the path resolves is its first token's mark.
        let found = self.resolve_at(env, v, p, abs, texts, Ns::Type, r)?;
        if found.consumed as usize != texts.len() {
            return Err(Why::Unsupported("an associated type of a type (`T::X`)", r));
        }
        if matches!(found.kind, Kind::Trait | Kind::TraitAlias) {
            if self.allows_bare_trait_object_at(env, v, p, r)? {
                return self.bare_dyn_ty(tys, v, p, segs, found.key, found.kind == Kind::TraitAlias, kids, r);
            }
            let msg = blaze_own::format_in!(tys.alloc(), "`{shown}` is not a type (trait objects without `dyn` are invalid since Rust 2021: E0782)");
            return Err(Why::Program(msg.as_str().into(), r));
        }
        if segs.last().is_some_and(|s| s.parenthesized) {
            return Err(Why::Unsupported("parenthesized generic arguments (`Fn(A) -> B`)", r));
        }
        if has_assoc_arg(v, segs)? {
            return Err(Why::Program("associated item constraints are not allowed on a non-trait type path (E0229)".into(), r));
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

    /// `Self::name` written in the trait `tr` (its arguments `args`) or one of its members: the
    /// one trait declaring `name` among every bound on `Self` in scope, as for a type parameter
    /// ([`Self::param_assoc`]): `Self: Tr<args>` itself with its supertraits, and the bounds on
    /// `Self` in `env` (`Param(0)`'s: the trait's `where Self: ..` and the member's own, tap's
    /// `fn try_conv<T>(self) -> Result<T, Self::Error> where Self: TryInto<T>`, where `Error`
    /// is `TryInto<T>`'s). Two distinct traits declaring it are ambiguous (rustc's E0221).
    #[expect(clippy::too_many_arguments, reason = "the trait, the associated item's own arguments, and the path's site")]
    fn trait_self_assoc(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, tr: LibKey, args: List, name: Name, own: &[PTy], r: Range, shown: &str) -> Result<PTy, Why> {
        let mut found: blaze_own::Vec<(LibKey, List), Alloc<'_>> = blaze_own::Vec::new_in(tys.alloc());
        if let Some(owner) = self.assoc_owner(tys, tr, args, name, r, shown)? {
            found.push(owner);
        }
        let (Bounds::Building(bounds) | Bounds::Done(bounds)) = env.bounds;
        for b in bounds.iter().filter(|b| b.param == 0) {
            if let Some(owner) = self.assoc_owner(tys, b.trait_, b.args, name, r, shown)? {
                if !found.contains(&owner) {
                    found.push(owner);
                }
            }
        }
        match found.as_slice() {
            &[(t, a)] => self.assoc_with_args(tys, t, name, a, own, r, shown),
            [] => Err(program(format!("`{shown}` names no associated type of the trait, its supertraits or a bound on `Self` in scope"), r)),
            many => {
                let mut traits = Vec::with_capacity_in(many.len(), Global);
                traits.extend(many.iter().map(|&(t, _)| t));
                Err(Why::AssocAmbiguous(Box::from(shown), traits, r))
            }
        }
    }

    /// `T::name<own..>` with `T` the parameter `param`: `<T as Tr>::name<own..>` for the one
    /// trait `Tr` among `T`'s bounds in `env`, supertraits included, that declares `name`, the
    /// arguments `own` written on `name`'s segment being the generic associated type's own
    /// ([`Self::assoc_with_args`]; none is [`Self::alias`]). While the bounds are themselves
    /// being lowered (`Bounds::Building`) and none lowered so far declares it, the bounds of `T`
    /// as written in the header `v` are read ([`super::written`]: `T: Tr<T::X> + Declares`, a
    /// bound naming `X` before the bound that declares it).
    #[expect(clippy::too_many_arguments, reason = "the path, where it is written, and the parameter")]
    fn param_assoc(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, param: u32, name: Name, own: &[PTy], r: Range, shown: &str) -> Result<PTy, Why> {
        let (bounds, done) = match env.bounds {
            Bounds::Building(b) => (b, false),
            Bounds::Done(b) => (b, true),
        };
        let mut found: blaze_own::Vec<(LibKey, List), Alloc<'_>> = blaze_own::Vec::new_in(tys.alloc());
        for b in bounds.iter().filter(|b| b.param == param) {
            if let Some(owner) = self.assoc_owner(tys, b.trait_, b.args, name, r, shown)? {
                if !found.contains(&owner) {
                    found.push(owner);
                }
            }
        }
        match found.as_slice() {
            &[(t, a)] => self.assoc_with_args(tys, t, name, a, own, r, shown),
            [] if !done => match self.written_param_assoc(tys, env, v, param, name, own, r, shown)? {
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
        let alloc = tys.alloc();
        let mut todo = blaze_own::Vec::new_in(alloc);
        todo.push((tr, args));
        let mut found: Option<(LibKey, List)> = None;
        let mut seen = blaze_own::Vec::new_in(alloc);
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
            let mut given_v = blaze_own::Vec::with_capacity_in(given.len(), alloc);
            given_v.extend_from_slice(given);
            for &(st, sargs) in self.heads.trait_supers(t).ok_or(Why::Need(t))? {
                let written = tys.view().list(sargs).map_err(|_| program(format!("a list outside the table"), r))?;
                let mut written_v = blaze_own::Vec::with_capacity_in(written.len(), alloc);
                written_v.extend_from_slice(written);
                let mut out = blaze_own::Vec::with_capacity_in(written_v.len(), alloc);
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
    pub(super) fn assoc_with_args(&self, tys: &mut PortableTys<'_>, trait_: LibKey, name: Name, args: List, own: &[PTy], r: Range, shown: &str) -> Result<PTy, Why> {
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
        let mut all = Vec::with_capacity_in(of_trait.len() + own.len(), tys.alloc());
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
        let mut written: SmallPath<lf::TypeId, 8> = SmallPath::new(lf::TypeId(0));
        children_into(v, t, &mut |id| written.push(id))?;
        let mut given: SmallPath<PTy, 8> = SmallPath::new(PTy(0));
        for &w in written.iter() {
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
        self.trait_at(tys, env, v, p, abs, &texts, r, self_ty, given)
    }

    /// [`Self::trait_path`] over the path's texts.
    #[expect(clippy::too_many_arguments, reason = "the path, its view and the trait's arguments")]
    pub(super) fn trait_at(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, p: PathIdx, abs: bool, texts: &[&str], r: Range, self_ty: PTy, given: &[PTy]) -> Result<(LibKey, List), Why> {
        match self.bound_target(env, v, p, abs, texts, r)? {
            Bounded::Trait(key) => self.trait_args(tys, key, r, self_ty, given),
            Bounded::Alias(_) => Err(program(format!("`{}` is a trait alias, which stands for bounds, not one trait (an impl's trait, a `T::X` bound)", texts.join("::")), r)),
        }
    }

    /// What a bound's path `p` of `v` names, by the names library where its first token's mark
    /// says (front2 item_site.rs): a trait or a trait alias.
    pub(super) fn bound_target(&self, env: &Env<'_>, v: &View<'_>, p: PathIdx, abs: bool, texts: &[&str], r: Range) -> Result<Bounded, Why> {
        let found = self.resolve_at(env, v, p, abs, texts, Ns::Type, r)?;
        match found.kind {
            _ if found.consumed as usize != texts.len() => Err(program(format!("this path is not a trait"), r)),
            Kind::Trait => Ok(Bounded::Trait(found.key)),
            Kind::TraitAlias => Ok(Bounded::Alias(found.key)),
            _ => Err(program(format!("this path is not a trait"), r)),
        }
    }

    /// Fills any omitted defaulted type arguments of trait alias `alias` (`trait Alias<T = u8> = Gen<T>;`),
    /// returning the full argument list after `Self` (of length `head.generics - 1`).
    pub(super) fn alias_args_with_defaults<'a>(
        &self,
        tys: &mut PortableTys<'a>,
        alias: LibKey,
        given: &[PTy],
        self_ty: PTy,
        r: Range,
    ) -> Result<blaze_own::Vec<PTy, Alloc<'a>>, Why> {
        let head = self.heads.trait_alias(alias).ok_or(Why::Need(alias))?;
        let want = usize::try_from(head.generics).unwrap_or(0).saturating_sub(1);
        let alloc = tys.alloc();
        let wrong_arity = || {
            let msg = blaze_own::format_in!(alloc, "a trait alias written with {} type arguments; it takes {want}", given.len());
            Why::Program(msg.as_str().into(), r)
        };
        if given.len() == want {
            let mut out = blaze_own::Vec::with_capacity_in(want, alloc);
            out.extend_from_slice(given);
            return Ok(out);
        }
        if given.len() > want {
            return Err(wrong_arity());
        }
        let Some(alias_text) = self.resolver.names.signature_text_of(alias) else {
            return Err(wrong_arity());
        };
        let marks = super::item_site::item_marks(self.resolver.names, alias, alias_text.text)
            .map_err(|_| Why::Unsupported("trait alias token marks unavailable", r))?;
        let lowered = super::heads::with_lowered(self.resolver.names, alias, alias_text.text, |av| -> Result<blaze_own::Vec<PTy, Alloc<'a>>, Why> {
            let mut ps: blaze_own::Vec<(Name, u32), Alloc<'a>> = blaze_own::Vec::new_in(alloc);
            let mut defaults: blaze_own::Vec<Option<AliasDefault>, Alloc<'a>> = blaze_own::Vec::new_in(alloc);
            let mut param_i = 1u32;
            for &row in av.rows(av.list(blaze_lower::head::GENERICS))? {
                match lf::GenericParam::decode(row)? {
                    lf::GenericParam::Lifetime { .. } => {}
                    lf::GenericParam::Type { name, default, .. } => {
                        ps.push((Name(av.name(name)?), param_i));
                        defaults.push(default.map(AliasDefault::Type));
                        param_i += 1;
                    }
                    lf::GenericParam::Const { name, default, .. } => {
                        ps.push((Name(av.name(name)?), param_i));
                        defaults.push(default.map(|c| AliasDefault::Const(param_i, c)));
                        param_i += 1;
                    }
                }
            }
            let cs = self.heads.const_params(alias).unwrap_or(&[]);
            let alias_env = Env {
                module: alias_text.module,
                params: &ps,
                bounds: Bounds::Building(&[]),
                self_: SelfTy::Ty(self_ty, None),
                self_assoc: &[],
                anon: &[],
                consts: cs,
                body: None,
            };
            let alias_builder = Builder {
                resolver: crate::resolve::Paths { names: self.resolver.names },
                heads: self.heads,
                marks,
                owner: Some((alias, alias_text.text)),
                owner_key: Some(super::keys::checker_of(alias)),
                expanded: None,
                expanded_marks: None,
            };
            let mut full = blaze_own::Vec::with_capacity_in(want + 1, alloc);
            full.push(self_ty);
            full.extend_from_slice(given);
            for i in given.len()..want {
                let raw = match defaults.get(i).copied().flatten() {
                    Some(AliasDefault::Type(def_ty)) => alias_builder.ty(tys, &alias_env, av, def_ty)?,
                    Some(AliasDefault::Const(pi, c)) => {
                        let at = av.const_range(c)?;
                        let cr = av.site(at)?;
                        let want_ty = cs
                            .iter()
                            .find(|(ci, _)| *ci == pi)
                            .map(|&(_, t)| t)
                            .ok_or_else(|| Why::Program("a const parameter with no type".into(), cr))?;
                        alias_builder.const_term(tys, &alias_env, av.text_at(at)?, cr, &|_| Ok(want_ty))?
                    }
                    None => {
                        return Err(wrong_arity());
                    }
                };
                let filled = subst(tys, raw, &full)?;
                full.push(filled);
            }
            let mut out = blaze_own::Vec::with_capacity_in(want, alloc);
            out.extend_from_slice(full.get(1..).unwrap_or(&[]));
            Ok(out)
        });
        match lowered {
            Ok(res) => res,
            Err(_) => Err(wrong_arity()),
        }
    }

    /// The trait alias `alias` as a bound on `self_ty` with the written arguments `given`: its
    /// clauses with `Self` and its parameters substituted (the unstable book, `trait_alias`:
    /// a bound naming it "is equivalent to" its right-hand side), into `out`.
    pub(super) fn alias_clauses<O: Extend<PClause>>(&self, tys: &mut PortableTys<'_>, alias: LibKey, r: Range, self_ty: PTy, given: &[PTy], out: &mut O) -> Result<(), Why> {
        let filled = self.alias_args_with_defaults(tys, alias, given, self_ty, r)?;
        let head = self.heads.trait_alias(alias).ok_or(Why::Need(alias))?;
        let mut args = blaze_own::Vec::with_capacity_in(filled.len() + 1, tys.alloc());
        args.push(self_ty);
        args.extend_from_slice(&filled);
        for &c in &head.clauses {
            out.extend(core::iter::once(subst_clause(tys, c, &args)?));
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
        let mut args = blaze_own::Vec::with_capacity_in(want + 1, tys.alloc());
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

    /// The trait `key`'s arguments AFTER `Self` for a `for<F in fields(..)> F: Trait<args...>`
    /// bound, filling any defaulted trait parameters that do not reference `Self`.
    pub(super) fn trait_args_after_self(&self, tys: &mut PortableTys<'_>, key: LibKey, r: Range, given: &[PTy]) -> Result<List, Why> {
        let (g, defaults) = self.heads.trait_head(key).ok_or(Why::Need(key))?;
        let want = usize::try_from(g).unwrap_or(0).saturating_sub(1);
        if given.len() > want {
            return Err(Why::Unsupported("too many type arguments on a `for<F in fields(..)>` trait bound", r));
        }
        if want == 0 {
            return Ok(List::EMPTY);
        }
        let alloc = tys.alloc();
        let mut args: blaze_own::Vec<PTy, Alloc<'_>> = blaze_own::Vec::with_capacity_in(want + 1, alloc);
        let dummy_self = intern(tys, PNode::Error)?;
        args.push(dummy_self);
        args.extend_from_slice(given);
        for i in given.len()..want {
            let Some(d) = defaults.get(i).copied().flatten() else {
                return Err(Why::Unsupported("missing required type argument on a `for<F in fields(..)>` trait bound", r));
            };
            if mentions_param0(tys.view(), d, alloc) {
                return Err(Why::Unsupported("a defaulted trait argument referring to `Self` in a `for<F in fields(..)>` bound", r));
            }
            let d = subst(tys, d, &args)?;
            args.push(d);
        }
        intern_list(tys, args.get(1..).unwrap_or(&[]))
    }
}

/// Whether `root` references `PNode::Param(0)` (used to detect `Self`-dependent defaults).
fn mentions_param0(tys: tc::PTys<'_>, root: PTy, alloc: Alloc<'_>) -> bool {
    let mut stack: blaze_own::Vec<PTy, Alloc<'_>> = blaze_own::Vec::new_in(alloc);
    let mut kids: blaze_own::Vec<PTy, Alloc<'_>> = blaze_own::Vec::new_in(alloc);
    stack.push(root);
    let mut steps = 0usize;
    while let Some(t) = stack.pop() {
        steps += 1;
        if steps > 4096 {
            return true;
        }
        let Ok(node) = tys.node(t) else { return true };
        if matches!(node, PNode::Param(0)) {
            return true;
        }
        kids.clear();
        if tys.children_of(node, &mut kids).is_ok() {
            for &c in kids.iter() {
                stack.push(c);
            }
        }
    }
    false
}

/// What item or generic associated type a segment's const arguments belong to
/// ([`Builder::const_want`]).
#[derive(Clone, Copy)]
enum ConstTarget<'a> {
    /// A regular path segment (`Foo<A>`, `Trait<A>`): resolve `texts` in `Ns::Type`.
    Path { p: PathIdx, abs: bool, texts: &'a [&'a str] },
    /// A generic associated type on a type parameter (`F::Config<A>` or `<F>::Config<A>`).
    ParamAssoc { param: u32, name: Name, shown: &'a str },
    /// A generic associated type on `Self` (`Self::Config<A>` or `<Self>::Config<A>`).
    SelfAssoc { name: Name, shown: &'a str },
    /// A generic associated type on a qualified trait path (`<T as Tr>::Config<A>`).
    TraitAssoc { trait_path: PathIdx, name: Name, shown: &'a str },
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
        let alloc = tys.alloc();
        let old_slice = tys.view().list(l).map_err(|_| program(format!("a clause refers past the type table"), Range::new(0, 0)))?;
        let mut old = blaze_own::Vec::with_capacity_in(old_slice.len(), alloc);
        old.extend_from_slice(old_slice);
        let mut new = blaze_own::Vec::with_capacity_in(old.len(), alloc);
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
        PClause::ForFields { container, trait_, args: l } => {
            let container = subst(tys, container, args)?;
            PClause::ForFields { container, trait_, args: list(tys, l)? }
        }
        PClause::TypeOutlives { ty, region } => PClause::TypeOutlives { ty: subst(tys, ty, args)?, region },
        c @ PClause::RegionOutlives { .. } => c,
    })
}

/// Instantiate parameters throughout the portable type, including a trait object's predicates.
pub fn subst(tys: &mut PortableTys<'_>, t: PTy, args: &[PTy]) -> Result<PTy, Why> {
    tys.subst(t, args).map_err(|e| {
        let message = blaze_own::text::format(tys.alloc(), format_args!("substituting type {t:?}: {e}"));
        Why::Program(message.as_str().into(), Range::new(0, 0))
    })
}

/// A written ABI string (`"C"`, with its quotes): the one reading of it, for fn items
/// (`heads::abi_of`) and fn pointer types.
///
/// The rule: an ABI is part of a fn pointer type's and a fn item's IDENTITY (the Reference,
/// "Function pointer types": `extern "C" fn()` and `fn()` are two types, and no coercion relates
/// two ABIs), and CHECKING needs nothing else of it. So every ABI the pinned nightly knows is a
/// type here (the checker's [`tc::Abi`], one variant per string of rustc's `ExternAbi`, read by
/// [`tc::Abi::of_name`], the one table): an interpreter's tail-call table, `type TailInstrFn<T> =
/// extern "rust-preserve-none" fn(..)`, is a type like any other. Only the BUILDER refuses to
/// make a call through an ABI it cannot emit, by name (program/head_abi.rs `of_head`; blaze-build
/// walk.rs). A string the nightly does not know is the language's E0703 ("invalid ABI").
pub(super) fn fn_ptr_abi(written: &str, r: Range) -> Result<tc::Abi, Why> {
    let name = written.trim().strip_prefix('"').and_then(|t| t.strip_suffix('"')).ok_or(Why::Unsupported("an ABI that is no plain string", r))?;
    tc::Abi::of_name(name).ok_or_else(|| program(format!("invalid ABI: found `{name}` (E0703): no ABI of the pinned nightly has this name"), r))
}

/// The length of the array type `t` of a BODY, written as one name (`[u8; SIZE]`, `[u8; {
/// SIZE }]`) that a `const` item of the body declares: that constant's value. The Reference,
/// "Statements", "Item declarations": "Declaring an item within a statement block restricts its
/// scope to the block containing the statement", so `const SIZE: usize = ..;` in a fn is in
/// scope for every type written in that block and the blocks inside it, a written type's array
/// length included (rmp's `read_data_f32`: `const SIZE: usize = size_of::<f32>(); let mut buf:
/// [u8; SIZE] = [0u8; SIZE];`, refused E0412 "cannot find type `SIZE`" when the length was read
/// at the fn's module, where no `SIZE` is).
///
/// The block is the one the type is written in: the block scope of the paths written inside
/// the type (every path of one written type stands in one block; a body expression's length is
/// read so too, body.rs `scope_within`), and the constant is found from that block outwards
/// ([`super::locals::Locals::find`]), as an expression's repeat length is
/// (super::const_open::local_const). `None`: not a body's type, not one name, no path in the
/// type to place it, or no item of the body takes the name: the caller reads the length as
/// before (at the module). A body item of the name that is no constant is refused by name.
/// The block scope enclosing written type `t` in `v`: first any path written inside `t`
/// (its own `scope`, exact for `[u8; SIZE]`), then the innermost `Expr::Block` whose range
/// covers `t` (needed when `t` contains no path, such as `*const [_; SIZE]` in `bytes`'s
/// `buf_try_get_impl!` macro), falling back to the call site's enclosing block when `t`
/// lives in a macro expansion with no block of its own.
fn type_scope(v: &View<'_>, t: lf::TypeId) -> Result<Option<lf::ScopeId>, Why> {
    let at = v.type_range(t)?;
    for (p, _) in (0u32..).zip(v.paths) {
        let p = PathIdx(p);
        if at.cover(v.path_range(p)?) != Some(at) {
            continue;
        }
        if let Some(s) = v.path(p)?.scope {
            return Ok(Some(s));
        }
    }
    let mut best: Option<(u32, lf::ScopeId)> = None;
    for (e, _) in (0u32..).zip(v.exprs) {
        let eid = blaze_typeck::ExprId(e);
        if let lf::Expr::Block { scope, .. } = v.expr(eid)? {
            let br = v.expr_range(eid)?;
            if br.cover(at) == Some(br) {
                let len = br.len();
                if best.is_none_or(|(b, _)| len < b) {
                    best = Some((len, scope));
                }
            }
        }
    }
    if let Some((_, scope)) = best {
        return Ok(Some(scope));
    }
    if at.in_expansion() {
        let site = v.site(at)?;
        for (e, _) in (0u32..).zip(v.exprs) {
            let eid = blaze_typeck::ExprId(e);
            if let lf::Expr::Block { scope, .. } = v.expr(eid)? {
                let br = v.expr_range(eid)?;
                if let Some(ir) = v.item_relative(br)
                    && ir.start <= site.start
                    && site.end <= ir.end
                {
                    let len = ir.end.saturating_sub(ir.start);
                    if best.is_none_or(|(b, _)| len < b) {
                        best = Some((len, scope));
                    }
                }
            }
        }
        if let Some((_, scope)) = best {
            return Ok(Some(scope));
        }
    }
    Ok(None)
}

#[expect(clippy::too_many_arguments, reason = "one lookup of a body-local array type length")]
fn body_const_len<'n, H: Heads + ?Sized, S: NamesSource<'n>>(
    resolver: &Paths<S>,
    marks: Option<blaze_scope::crate_roots::TextMarks<'_>>,
    heads: &H,
    env: &Env<'_>,
    v: &View<'_>,
    t: lf::TypeId,
    text: &str,
    r: Range,
) -> Result<Option<u128>, Why> {
    let Some(body) = env.body else { return Ok(None) };
    let mut name = text.trim();
    if let Some(inner) = name.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        name = inner.trim();
    }
    let ident = name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && name.chars().all(|c| c.is_alphanumeric() || c == '_');
    if !ident {
        return Ok(None);
    }
    let Some((owner, scope)) = body.locals.scope_or_declared(body.owner, type_scope(v, t)?.map(|s| s.0)) else {
        return Ok(None);
    };
    super::const_open::local_const_in_scope(
        resolver,
        marks,
        body.locals,
        owner,
        body.module,
        heads,
        v,
        scope,
        None,
        name,
        r,
        super::const_open::ArrayLenSite::Type,
    )
}

/// The types `t` is made of, in order; `None` past the table.
pub fn children_of(t: blaze_typeck::PTys<'_>, ty: PTy, out: &mut impl Extend<PTy>) -> Option<()> {
    t.children_of(*t.get(ty)?, out).ok()
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
///
/// The same numbering is a type alias's (super::tait `alias_opaques`: each `impl` an alias's
/// type writes is one opaque of the alias), so `out` takes the caller's allocator.
pub(super) fn return_impls<A: core::alloc::Allocator>(v: &View<'_>, ret: lf::TypeId, out: &mut Vec<lf::TypeId, A>) -> Result<bool, Why> {
    let mut todo: SmallPath<lf::TypeId, 16> = SmallPath::new(lf::TypeId(0));
    let mut kids: SmallPath<lf::TypeId, 16> = SmallPath::new(lf::TypeId(0));
    let mut macros = false;
    todo.push(ret);
    while let Some(t) = todo.pop() {
        kids.clear();
        match v.ty(t)? {
            lf::TypeRef::Impl(bounds) => {
                out.push(t);
                for &b in v.rows(bounds)? {
                    if let lf::Bound::Trait { ty, .. } = lf::Bound::decode(b)? {
                        bound_arg_types_into(v, ty, &mut |id| kids.push(id))?;
                    }
                }
            }
            lf::TypeRef::Macro(_) => macros = true,
            _ => children_into(v, t, &mut |id| kids.push(id))?,
        }
        for &k in kids.iter().rev() {
            todo.push(k);
        }
    }
    Ok(macros)
}

/// The types written in a bound's trait path's arguments: type arguments, associated type
/// bindings (`Item = T`) and a parenthesized `Fn(A) -> B`'s inputs and output, in order.
fn bound_arg_types_into(v: &View<'_>, ty: lf::TypeId, out: &mut impl FnMut(lf::TypeId)) -> Result<(), Why> {
    let lf::TypeRef::Path(p) = v.ty(ty)? else { return Ok(()) };
    for &n in v.segments(v.path(p)?)? {
        let s = lf::Segment::decode(n)?;
        if !s.has_args && !s.parenthesized {
            continue;
        }
        for &a in v.rows(s.args)? {
            match lf::GenericArg::decode(a)? {
                lf::GenericArg::Type(t) | lf::GenericArg::AssocType { ty: t, .. } => out(t),
                _ => {}
            }
        }
        if s.parenthesized {
            if let Some(ret) = s.ty {
                out(ret);
            }
        }
    }
    Ok(())
}

/// The written types a type is made of, in order (the work list's children).
pub(super) fn children(v: &View<'_>, t: lf::TypeId, out: &mut Vec<lf::TypeId, Global>) -> Result<(), Why> {
    children_into(v, t, &mut |id| out.push(id))
}

pub(super) fn children_into(v: &View<'_>, t: lf::TypeId, out: &mut impl FnMut(lf::TypeId)) -> Result<(), Why> {
    let r = place(v, v.type_range(t))?;
    match v.ty(t)? {
        lf::TypeRef::Ref { inner, .. } | lf::TypeRef::Ptr { inner, .. } | lf::TypeRef::Slice(inner) => out(inner),
        lf::TypeRef::Array { elem, .. } => out(elem),
        lf::TypeRef::Tuple(l) | lf::TypeRef::Union(l) => {
            for &i in v.ids(l)? {
                out(lf::TypeId(i));
            }
        }
        lf::TypeRef::Path(p) => {
            let path = v.path(p)?;
            for &n in v.segments(path)? {
                let s = lf::Segment::decode(n)?;
                if s.kind == lf::SegKind::Anchor {
                    let Some(ty) = s.ty else { return Err(program(format!("a qualified path's self type is missing"), r)) };
                    out(ty);
                    // `<T>::X`: the self type only (`Builder::qualified_no_trait`).
                    let Some(tr) = s.trait_ else { continue };
                    let lf::TypeRef::Path(tp) = v.ty(tr)? else { return Err(program(format!("a trait that is no path"), r)) };
                    for &tn in v.segments(v.path(tp)?)? {
                        arg_types_into(v, &lf::Segment::decode(tn)?, r, out)?;
                    }
                } else {
                    arg_types_into(v, &s, r, out)?;
                }
            }
        }
        lf::TypeRef::Never | lf::TypeRef::Infer => {}
        // A C-variadic `fn(A, ...)`'s `...` is no input: the header says it (`make`). It may
        // only be the last parameter (the language's "`...` must be the last argument of a
        // C-variadic function").
        lf::TypeRef::FnPtr { params, ret, .. } => {
            let rows = v.rows(params)?;
            for (i, &n) in rows.iter().enumerate() {
                let p = lf::Param::decode(n)?;
                if p.variadic {
                    if i + 1 != rows.len() {
                        return Err(program(format!("`...` must be the last parameter of a C-variadic fn pointer type"), r));
                    }
                    continue;
                }
                out(p.ty.ok_or(Why::Unsupported("a fn pointer parameter with no type", r))?);
            }
            if let Some(ret) = ret {
                out(ret);
            }
        }
        lf::TypeRef::Dyn(bounds) => {
            for &b in v.rows(bounds)? {
                if let Some((_, seg)) = super::dyn_ty::dyn_bound(v, lf::Bound::decode(b)?, r)? {
                    super::dyn_ty::dyn_arg_types_into(v, &seg, r, out)?;
                }
            }
        }
        // An anonymous type parameter is made of nothing: its bounds are its fn's predicates
        // ([`anon_params`]); any other `impl Trait` is refused where it is made.
        lf::TypeRef::Impl(_) => {}
        // A signature's type macro is expanded from its own text ([`super::pattern_ty`]).
        lf::TypeRef::Macro(_) => {}
        lf::TypeRef::Pat { base, .. } => out(base),
        lf::TypeRef::Record(fields) => {
            // Yield record field and spread types in written order so `make()` and
            // `record_regions()` align `rows[i]` with `kids[i]` before expanding spreads and
            // sorting into canonical alphabetical field-name order.
            for &n in v.rows(fields)? {
                out(lf::Field::decode(n)?.ty);
            }
        }
        lf::TypeRef::For { binder, inner } => {
            super::preds::lifetimes_only(v, binder, r)?;
            out(inner);
        }
        lf::TypeRef::Error => return Err(Why::Unsupported("this type", r)),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Region slots beside the made types (D148, piece L1a; docs/v2/plan/design/flow.md "Lifetimes
// are produced once and carried beside the types"). The walk that interns a written type
// pushes, for each type it makes, that type's slots in `regions::each_slot_node`'s order: a
// node's own first, then its children's in order. Nothing reads a signature's text for its
// lifetimes after this walk.

/// What a written type's text says at one region position, before its item's lifetimes are
/// numbered (heads.rs `Store::fn_head` numbers them with `regions::Numbering`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
    /// `'static`.
    Static,
    /// A lifetime written by name and bound by no binder inside the type: the item's or its
    /// container's, numbered by the head.
    Named(Name, Range),
    /// An elided lifetime (`&T`, `'_`, a path written without its ADT's lifetimes): its site's
    /// index ([`RegionSink::elided_at`]).
    Elided(u32),
    /// Bound inside the made type: `binder` the binding node's index among its binder nodes
    /// (fn pointers and trait objects, [`is_binder`]) in pre-order, as `Region::Late`.
    Late { binder: u32, var: u32 },
    /// Bound by the trait object being made (its bounds' `for<..>`, its `Fn` sugar's elided
    /// inputs): `Late { binder: 0, var }` once its slots are placed.
    Bound(u32),
    /// Given whole by the head: `Self`'s regions, an opaque type's captured lifetimes.
    Given(Region),
    /// A trait object's lifetime bound not written: its context decides it (the Reference,
    /// "Lifetime elision", "Default trait object lifetimes").
    ObjectDefault(Range),
}

/// One region slot: what it is, and whether the elision rules count it as a lifetime its
/// inputs use (a written or elided lifetime does; `Self`'s and a trait object's default do
/// not: the Reference, "Lifetime elision in functions").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    pub kind: SlotKind,
    pub counts: bool,
}

impl Slot {
    pub const fn written(kind: SlotKind) -> Slot {
        Slot { kind, counts: true }
    }

    pub const fn quiet(kind: SlotKind) -> Slot {
        Slot { kind, counts: false }
    }

    /// The slot inside a parent whose binder nodes before it are `by`.
    fn shifted(self, by: u32) -> Slot {
        match self.kind {
            SlotKind::Late { binder, var } => Slot { kind: SlotKind::Late { binder: binder + by, var }, counts: self.counts },
            _ => self,
        }
    }
}

/// Whether two slots name one region (a name is one lifetime wherever it is written).
pub fn same_region(a: SlotKind, b: SlotKind) -> bool {
    match (a, b) {
        (SlotKind::Named(x, _), SlotKind::Named(y, _)) => x == y,
        _ => a == b,
    }
}

/// How many lifetimes `slots` use, as the elision rules count them: the counted slots whose
/// kind `local` takes (a binder's own variables) or that are written or `'static`.
pub enum Used {
    None,
    One(SlotKind),
    Many,
}

pub fn used_lifetimes(slots: &[Slot], local: fn(SlotKind) -> bool) -> Used {
    let mut one: Option<SlotKind> = None;
    for s in slots {
        if !s.counts {
            continue;
        }
        let usable = match s.kind {
            SlotKind::Static | SlotKind::Named(..) | SlotKind::Elided(_) => true,
            SlotKind::Late { .. } | SlotKind::Bound(_) | SlotKind::Given(_) => local(s.kind),
            SlotKind::ObjectDefault(_) => false,
        };
        if !usable {
            continue;
        }
        match one {
            None => one = Some(s.kind),
            Some(o) if same_region(o, s.kind) => {}
            Some(_) => return Used::Many,
        }
    }
    one.map_or(Used::None, Used::One)
}

/// A fn pointer's own variables: `Late { binder: 0, .. }`.
fn fn_ptr_local(k: SlotKind) -> bool {
    matches!(k, SlotKind::Late { binder: 0, .. })
}

/// A trait object's own variables, before its slots are placed: `Bound`.
fn object_local(k: SlotKind) -> bool {
    matches!(k, SlotKind::Bound(_))
}

/// The nodes that bind regions of their children: a fn pointer (its elided and `for<..>`
/// lifetimes) and a trait object (its bounds' `for<..>` and `Fn` sugar).
pub fn is_binder(n: PNode) -> bool {
    matches!(n, PNode::FnPtr(..) | PNode::Dynamic(_))
}

/// The slots of one made type on a [`RegionSink`]'s stack, and how many binder nodes the type
/// has (what a parent shifts its `Late` binders by).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frag {
    pub start: u32,
    pub len: u32,
    pub binders: u32,
}

/// Why a made type's region slots are not laid: the erased type stands; the head's regions
/// are refused by this name (rule 19), never guessed (rule 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionRefusal {
    /// The one counter or the column refused (blaze-typeck regions.rs).
    Regions(RegionError),
    /// A lifetime written by name that no scope of the head declares (E0261).
    Undeclared { at: Range },
    /// A path to an ADT written with `got` lifetime arguments where it declares `want`
    /// (E0107).
    LifetimeArgs { at: Range, want: u32, got: u32 },
    /// A trait object's default lifetime its context does not decide here (an argument of
    /// a type that has lifetime parameters, or of a type alias).
    ObjectDefault { at: Range },
    /// A region position this producer does not lay yet, by its typed cause (a reader in
    /// another store is told it: head_record.rs `Unlaid::NotLaid`).
    NotLaid { why: NotLaidWhy, at: Range },
    /// A written type that names nothing (E0412): no column is laid over the error type.
    NoType { at: Range },
    /// The container's lifetimes were not filed with its head (`Store::container_regions`).
    NoContainer { container: ItemKey },
}

impl From<RegionError> for RegionRefusal {
    fn from(e: RegionError) -> RegionRefusal {
        RegionRefusal::Regions(e)
    }
}

impl RegionRefusal {
    /// A region position this producer does not lay, by its typed cause, at `at`.
    pub const fn not_laid(why: NotLaidWhy, at: Range) -> RegionRefusal {
        RegionRefusal::NotLaid { why, at }
    }
}

/// What a person reads of a region refusal (rule 19): the cause in Rust's words and the
/// item-relative range it is at (rule 71: a `file:line:col` only where printed with its file).
impl core::fmt::Display for RegionRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            RegionRefusal::Regions(e) => write!(f, "{e}"),
            RegionRefusal::Undeclared { at } => write!(f, "a lifetime written by name that no scope declares, at {}..{} (E0261)", at.start, at.end),
            RegionRefusal::LifetimeArgs { at, want, got } => write!(f, "a path written with {got} lifetime argument(s) where it declares {want}, at {}..{} (E0107)", at.start, at.end),
            RegionRefusal::ObjectDefault { at } => write!(f, "a trait object's default lifetime its context does not decide here, at {}..{}", at.start, at.end),
            RegionRefusal::NotLaid { why, at } => write!(f, "[NotLaid/{}] {}, at {}..{}", why.name(), why.text(), at.start, at.end),
            RegionRefusal::NoType { at } => write!(f, "a written type that names nothing, at {}..{} (E0412)", at.start, at.end),
            RegionRefusal::NoContainer { .. } => f.write_str("its impl's or trait's own regions were not filed with the container's head"),
        }
    }
}

/// Why a region position is not laid ([`RegionRefusal::NotLaid`]): one variant per distinct
/// cause the walks name, so a reader in any store (head_record.rs `Unlaid::NotLaid`) and the
/// profile's "refused by cause" line count the refusals by cause, never by a producer's text.
/// Its byte ([`NotLaidWhy::byte`]) is part of the frozen head's entry (front2 tables/head.rs
/// form 12): a variant is never renumbered; a new one takes the next byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NotLaidWhy {
    /// A made type's children's slots are not on the sink's stack where the walk left them (a
    /// producer's bug: the walk's own bookkeeping).
    Stack,
    /// The made type and the region walk of it disagree (a path to an ADT made as no ADT, a
    /// projection made as no alias, a path's, an alias's, `Self`'s, a trait object's or a
    /// signature's slots out of step with the made type): a producer's bug.
    OutOfStep,
    /// A written type whose slots pass the table's `u32` count.
    TooMany,
    /// The region walk did not resolve a path (a type, a one-name argument, a bound) as the
    /// erased walk did a step before, or its second walk of a written type was refused.
    Resolve,
    /// A declared lifetime count (an ADT's, a trait's, an associated type's) the names do not
    /// hold.
    DeclaredCount,
    /// A fn pointer's, or a trait object's `Fn(..) -> ..` sugar's, return type elides a
    /// lifetime its inputs do not give exactly one of (rustc: E0106 there).
    FnPtrElision,
    /// A `for<..>` binder over a type that is no fn pointer.
    ForBinder,
    /// A return `impl Trait` whose captured lifetimes its rule or `use<..>` did not decide
    /// (heads_regions.rs `captures` refused them).
    OpaqueCaptures,
    /// `'_` in a return opaque's bound, and the inputs give no single lifetime for it (rustc:
    /// E0106).
    OpaqueElided,
    /// `_` in a declared type.
    Infer,
    /// A type macro's expansion.
    TypeMacro,
    /// A type form the region walk has no step for.
    OtherForm,
    /// A qualified path `<T>::Name` with no trait, or whose trait is no path.
    QualifiedNoTrait,
    /// A qualified path `<T as Tr<'a>>::Name` whose trait declares a lifetime. No walk names
    /// it now (the trait's lifetimes are laid: `Builder::qualified_slots`); kept for its byte,
    /// which a frozen head's entry may hold.
    QualifiedTraitLifetimes,
    /// A one-name type argument (`R` for `R<'_>`) whose own arguments hold a lifetime.
    OneNameArgs,
    /// A lifetime argument on a path's segment before its last.
    EarlySegmentLifetime,
    /// A projection path read through no bound (`T::A::B`, `Self::A::B`, `T::Name` outside
    /// the forms read through a bound) whose arguments or trait hold a lifetime.
    ProjectionPath,
    /// `Self`'s regions are not known where the type is written.
    SelfUnknown,
    /// A defaulted type argument (of an ADT or a type alias) that holds a lifetime.
    DefaultArg,
    /// A type alias whose head is not made.
    AliasUnmade,
    /// A type alias whose type holds a lifetime its head did not lay, or a bound lifetime
    /// whose binder is not above it.
    AliasRegions,
    /// A path whose made type holds a region this producer does not place.
    PathRegion,
    /// `Self::Name` of a supertrait (in a trait, or in an impl of its subtrait) whose trait
    /// holds a lifetime.
    SelfSupertrait,
    /// `Self::Name` in a trait impl whose trait holds a lifetime, laid where the header's
    /// trait column is not known (an associated type's value, a member's where clause or
    /// constant: those walks are given `Self`'s regions only; or the impl's regions refused).
    ImplTraitLifetimes,
    /// A projection read through a `for<..>` bound or where clause.
    ProjectionHigherRanked,
    /// A projection `T::Name` whose trait holds a lifetime and no bound on `T` in this item
    /// names (a supertrait's, or the container's bound).
    ProjectionBoundElsewhere,
    /// A projection whose parameter has two bounds naming its trait.
    ProjectionTwoBounds,
    /// A projection whose bound's type arguments hold a lifetime.
    ProjectionBoundArgs,
    /// An elided lifetime in the bound a projection is read through.
    ProjectionElidedBound,
    /// A trait object with more than one lifetime bound.
    DynLifetimes,
    /// A region position in a trait object no written lifetime gives, or arguments of it the
    /// walk could not place.
    DynRegion,
    /// A bound lifetime (a fn pointer's or a trait object's) where the walk does not place
    /// it: an impl's self type, outside its object, a column a producer reads.
    BoundVariable,
    /// A member's container's lifetime group and its lifetimes' names disagree (D148).
    ContainerDisagree,
    /// An `async fn`'s future whose in-scope lifetimes were not counted.
    AsyncCount,
    /// An elided lifetime of the output that the inputs' group does not hold, or that the
    /// sink gave no site.
    ElidedOutput,
    /// A written type the head's walk did not lay (it was not listed).
    Unlisted,
}

#[derive(Clone, Copy)]
struct NotLaidInfo {
    byte: u8,
    name: &'static str,
    text: &'static str,
}

impl NotLaidWhy {
    /// Every cause, in byte order.
    pub const ALL: [NotLaidWhy; 36] = [
        NotLaidWhy::Stack,
        NotLaidWhy::OutOfStep,
        NotLaidWhy::TooMany,
        NotLaidWhy::Resolve,
        NotLaidWhy::DeclaredCount,
        NotLaidWhy::FnPtrElision,
        NotLaidWhy::ForBinder,
        NotLaidWhy::OpaqueCaptures,
        NotLaidWhy::OpaqueElided,
        NotLaidWhy::Infer,
        NotLaidWhy::TypeMacro,
        NotLaidWhy::OtherForm,
        NotLaidWhy::QualifiedNoTrait,
        NotLaidWhy::QualifiedTraitLifetimes,
        NotLaidWhy::OneNameArgs,
        NotLaidWhy::EarlySegmentLifetime,
        NotLaidWhy::ProjectionPath,
        NotLaidWhy::SelfUnknown,
        NotLaidWhy::DefaultArg,
        NotLaidWhy::AliasUnmade,
        NotLaidWhy::AliasRegions,
        NotLaidWhy::PathRegion,
        NotLaidWhy::SelfSupertrait,
        NotLaidWhy::ImplTraitLifetimes,
        NotLaidWhy::ProjectionHigherRanked,
        NotLaidWhy::ProjectionBoundElsewhere,
        NotLaidWhy::ProjectionTwoBounds,
        NotLaidWhy::ProjectionBoundArgs,
        NotLaidWhy::ProjectionElidedBound,
        NotLaidWhy::DynLifetimes,
        NotLaidWhy::DynRegion,
        NotLaidWhy::BoundVariable,
        NotLaidWhy::ContainerDisagree,
        NotLaidWhy::AsyncCount,
        NotLaidWhy::ElidedOutput,
        NotLaidWhy::Unlisted,
    ];

    const fn info(self) -> NotLaidInfo {
        let (byte, name, text) = match self {
            NotLaidWhy::Stack => (0, "Stack", "a type whose children's region slots are not where the walk left them (a producer's bug)"),
            NotLaidWhy::OutOfStep => (1, "OutOfStep", "a type whose region walk is out of step with its made type (a producer's bug)"),
            NotLaidWhy::TooMany => (2, "TooMany", "a written type whose region slots pass a u32 count"),
            NotLaidWhy::Resolve => (3, "Resolve", "a path the region walk did not resolve as the type walk did"),
            NotLaidWhy::DeclaredCount => (4, "DeclaredCount", "a declared lifetime count (an ADT's, a trait's or an associated type's) the names do not hold"),
            NotLaidWhy::FnPtrElision => (5, "FnPtrElision", "a fn pointer's or `Fn(..)` sugar's return type elides a lifetime its inputs do not give exactly one of (rustc: E0106)"),
            NotLaidWhy::ForBinder => (6, "ForBinder", "a `for<..>` binder over a type that is no fn pointer"),
            NotLaidWhy::OpaqueCaptures => (7, "OpaqueCaptures", "a return `impl Trait` whose captured lifetimes its rule or `use<..>` did not decide"),
            NotLaidWhy::OpaqueElided => (8, "OpaqueElided", "`'_` in a return `impl Trait`'s bound, and the inputs give no single lifetime for it (rustc: E0106)"),
            NotLaidWhy::Infer => (9, "Infer", "`_` in a declared type"),
            NotLaidWhy::TypeMacro => (10, "TypeMacro", "a type macro's expansion"),
            NotLaidWhy::OtherForm => (11, "OtherForm", "a type form the region walk has no step for"),
            NotLaidWhy::QualifiedNoTrait => (12, "QualifiedNoTrait", "a qualified path `<T>::Name` with no trait whose projection holds a lifetime, or whose trait is no path"),
            NotLaidWhy::QualifiedTraitLifetimes => (13, "QualifiedTraitLifetimes", "a qualified path `<T as Tr<'a>>::Name` whose trait declares a lifetime"),
            NotLaidWhy::OneNameArgs => (14, "OneNameArgs", "a one-name type argument whose own arguments hold a lifetime"),
            NotLaidWhy::EarlySegmentLifetime => (15, "EarlySegmentLifetime", "a lifetime argument on a path's segment before its last"),
            NotLaidWhy::ProjectionPath => (16, "ProjectionPath", "a projection path read through no bound whose arguments or trait hold a lifetime"),
            NotLaidWhy::SelfUnknown => (17, "SelfUnknown", "`Self`'s regions, not known where this type is written"),
            NotLaidWhy::DefaultArg => (18, "DefaultArg", "a defaulted type argument that holds a lifetime"),
            NotLaidWhy::AliasUnmade => (19, "AliasUnmade", "a type alias whose head is not made"),
            NotLaidWhy::AliasRegions => (20, "AliasRegions", "a type alias whose type holds a lifetime its head did not lay"),
            NotLaidWhy::PathRegion => (21, "PathRegion", "a path whose made type holds a region the walk does not place"),
            NotLaidWhy::SelfSupertrait => (22, "SelfSupertrait", "`Self::Name` of a supertrait whose trait holds a lifetime"),
            NotLaidWhy::ImplTraitLifetimes => (23, "ImplTraitLifetimes", "`Self::Name` in an impl whose trait holds a lifetime, where the header's trait lifetimes are not known"),
            NotLaidWhy::ProjectionHigherRanked => (24, "ProjectionHigherRanked", "a projection read through a `for<..>` bound or where clause"),
            NotLaidWhy::ProjectionBoundElsewhere => (25, "ProjectionBoundElsewhere", "a projection `T::Name` whose trait holds a lifetime and no bound on `T` in this item names (a supertrait's or the container's)"),
            NotLaidWhy::ProjectionTwoBounds => (26, "ProjectionTwoBounds", "a projection whose parameter has two bounds naming its trait"),
            NotLaidWhy::ProjectionBoundArgs => (27, "ProjectionBoundArgs", "a projection whose bound's type arguments hold a lifetime"),
            NotLaidWhy::ProjectionElidedBound => (28, "ProjectionElidedBound", "an elided lifetime in the bound a projection is read through"),
            NotLaidWhy::DynLifetimes => (29, "DynLifetimes", "a trait object with more than one lifetime bound"),
            NotLaidWhy::DynRegion => (30, "DynRegion", "a region position in a trait object no written lifetime gives"),
            NotLaidWhy::BoundVariable => (31, "BoundVariable", "a bound lifetime of a fn pointer or trait object where the walk does not place it"),
            NotLaidWhy::ContainerDisagree => (32, "ContainerDisagree", "a member whose container's lifetime group and its lifetimes' names disagree (D148)"),
            NotLaidWhy::AsyncCount => (33, "AsyncCount", "an `async fn`'s future whose in-scope lifetimes were not counted"),
            NotLaidWhy::ElidedOutput => (34, "ElidedOutput", "an elided lifetime of the output the inputs' group does not hold"),
            NotLaidWhy::Unlisted => (35, "Unlisted", "a written type the head's walk did not lay"),
        };
        NotLaidInfo { byte, name, text }
    }

    /// Its byte in a record: its index in [`NotLaidWhy::ALL`].
    pub const fn byte(self) -> u8 {
        self.info().byte
    }

    /// The cause of byte `b`, or `None` for a byte no cause writes.
    pub fn of_byte(b: u8) -> Option<NotLaidWhy> {
        NotLaidWhy::ALL.get(usize::from(b)).copied()
    }

    /// Its variant's name: the short class a refusal is counted by (`[NotLaid/<name>/..]`).
    pub const fn name(self) -> &'static str {
        self.info().name
    }

    /// What a person reads (rule 19): the region position, in Rust's words.
    pub const fn text(self) -> &'static str {
        self.info().text
    }
}

/// Which part of an item's signature a region refusal was met in: what a refusal is placed by
/// beside its cause ([`NotLaidWhy`]). A sink is told the part it walks
/// ([`RegionSink::set_part`]); a check across the parts is the signature's `Whole`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SigPart {
    /// The signature as a whole (a check across its parts), or a walk of no one part.
    Whole,
    /// Input `n` of a fn (its `self` the first).
    Input(u32),
    /// A fn's return type (an `async fn`'s future and its written type), a constant's or
    /// static's type, a return opaque.
    Output,
    /// An impl's header: its self type.
    Header,
}

impl SigPart {
    /// Its tag and index in a record.
    pub const fn bytes(self) -> (u8, u32) {
        match self {
            SigPart::Whole => (0, 0),
            SigPart::Input(n) => (1, n),
            SigPart::Output => (2, 0),
            SigPart::Header => (3, 0),
        }
    }

    /// The part of tag `tag` and index `n`; `None` for a tag no part writes, or an index on a
    /// part that has none (one form per part: the bytes are canonical).
    pub fn of_bytes(tag: u8, n: u32) -> Option<SigPart> {
        match (tag, n) {
            (0, 0) => Some(SigPart::Whole),
            (1, n) => Some(SigPart::Input(n)),
            (2, 0) => Some(SigPart::Output),
            (3, 0) => Some(SigPart::Header),
            _ => None,
        }
    }

    /// Its kind's name: the class a refusal is counted by (its index is not in the class).
    pub const fn name(self) -> &'static str {
        match self {
            SigPart::Whole => "Whole",
            SigPart::Input(_) => "Input",
            SigPart::Output => "Output",
            SigPart::Header => "Header",
        }
    }
}

impl core::fmt::Display for SigPart {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SigPart::Whole => f.write_str("the signature"),
            SigPart::Input(n) => write!(f, "input {n}"),
            SigPart::Output => f.write_str("the output"),
            SigPart::Header => f.write_str("the impl header"),
        }
    }
}

/// A region step's answer: the binder count of the slots it left in the sink's scratch, or
/// why it laid none. A program error of the lifetimes (E0106) is the step's `Why`.
type Laid = Result<u32, RegionRefusal>;

/// What a projection written `X::Name` is read through ([`Builder::projection_slots`]): the
/// bounds on the type parameter `X` in this item, or, in a trait, the trait itself (`Self`),
/// or, in a trait impl, the impl's trait with `Self` the impl's self type (`SelfImpl`).
#[derive(Clone, Copy)]
enum Through<'t> {
    Param(&'t str),
    SelfTrait(LibKey),
    SelfImpl(LibKey),
}

/// Takes the soft answer of a step, or returns it.
macro_rules! soft {
    ($e:expr) => {
        match $e {
            Ok(x) => x,
            Err(why) => return Ok(Err(RegionRefusal::from(why))),
        }
    };
}

/// The shadow arguments a trait object is made again with, to see where each of its written
/// arguments lands in it: `Param(SHADOW_BASE + i)` for argument `i`. Below `u32::MAX`, the
/// erased `Self` of `dyn_ty`.
const SHADOW_BASE: u32 = 0xffff_0000;

/// One made type's region facts that its slots do not hold: its slot total, binder nodes and
/// nodes (a subtree's, by the one walk).
#[derive(Clone, Copy)]
pub(super) struct Sub {
    pub(super) slots: u32,
    pub(super) binders: u32,
    pub(super) nodes: u32,
}

pub(super) fn subtree<F: tc::regions::Arity + ?Sized>(tys: tc::PTys<'_>, ty: PTy, facts: &F, alloc: Alloc<'_>) -> Result<Sub, RegionError> {
    let mut sub = Sub { slots: 0, binders: 0, nodes: 0 };
    let slots = each_slot_node(tys, ty, facts, alloc, &mut |_, node, _| {
        sub.nodes += 1;
        sub.binders += u32::from(is_binder(node));
        Ok(())
    })?;
    sub.slots = slots;
    Ok(sub)
}

/// The captured lifetimes of the opaque types of the item being made (D148): each opaque's
/// count (`counts`, its `opaque_captures`), and the regions it captures in the fn's numbering
/// (`regions`: one run per opaque, in `counts`' order, each in ascending `Param` order as
/// heads_regions.rs `captures` gives it). The run is the `Opaque` node's own slots in the
/// signature's column: `Param(0..n)` under the every-lifetime rule, and the listed ones under
/// a `use<..>` bound or a pre-2024 RPIT's named-lifetimes rule (`fn sleep(&self, ..) -> impl
/// Future + use<Self>` in a trait captures NO lifetime: commonware-runtime's `Clock::sleep`).
#[derive(Clone, Copy, Debug)]
pub struct Captures<'c> {
    pub counts: &'c [(ItemKey, u32)],
    pub regions: &'c [Region],
}

impl<'c> Captures<'c> {
    /// No opaque type's captures: an item that has no return-position opaque.
    pub const NONE: Captures<'static> = Captures { counts: &[], regions: &[] };

    /// The regions `opaque` captures, `None` where it is not one of these opaques (or the runs
    /// are out of step with the counts: never a guessed run).
    pub fn of(&self, opaque: ItemKey) -> Option<&'c [Region]> {
        let mut lo = 0usize;
        for &(k, n) in self.counts {
            let hi = lo.checked_add(n as usize)?;
            if k == opaque {
                return self.regions.get(lo..hi);
            }
            lo = hi;
        }
        None
    }
}

/// What a walk in a container's member is given for `Self` (D148): `Self`'s regions (the impl's
/// A non-higher-ranked trait bound on a container's own type parameter (`PNode::Param(param)`
/// bounded by `trait_` with `regions` = the trait clause column's slots, numbered in the
/// container's lifetime group).
#[derive(Clone, Debug)]
pub struct ContainerParamBound {
    pub param: u32,
    pub trait_: ItemKey,
    pub regions: blaze_own::Vec<Region, Alloc<'static>>,
}

/// What a container's member's region walk is given for `Self` and for bounds on the container's
/// own type parameters: in an impl's member, `Self`'s regions (the impl's self type's column),
/// in a trait impl's member the impl's trait reference's column (`projection_slots`,
/// `Through::SelfImpl`), and in any trait or impl member the container's non-higher-ranked trait
/// bounds on its type parameters (`Through::Param`). Made from the container's filed regions by
/// `ContainerRegions::given` (heads.rs), so every member walk (a fn's signature, its clauses and
/// opaque bounds, a constant's type) is given the same context.
#[derive(Clone, Copy, Debug)]
pub struct SelfGiven<'a> {
    pub self_: Option<&'a [Region]>,
    pub impl_trait: Option<&'a [Region]>,
    pub param_bounds: &'a [ContainerParamBound],
}

impl SelfGiven<'_> {
    /// Nothing given: outside an impl/trait, or where the container's regions are not filed yet.
    pub const NONE: SelfGiven<'static> = SelfGiven { self_: None, impl_trait: None, param_bounds: &[] };
}

/// `Self`'s regions alone (no trait column or param bounds): a header's own walk, where the
/// trait column and clauses are what is being laid.
impl<'a> From<Option<&'a [Region]>> for SelfGiven<'a> {
    fn from(self_: Option<&'a [Region]>) -> SelfGiven<'a> {
        SelfGiven { self_, impl_trait: None, param_bounds: &[] }
    }
}

/// Maximum nesting depth when laying defaulted type arguments from an ADT's or type alias's
/// head text (rule 18: bounded recursion).
const MOST_DEFAULT_DEPTH: u8 = 16;

/// The region slots of the types one walk makes, a fragment per made type on a stack beside
/// the walk's own (D148): a type's slots are its children's fragments composed, with its own
/// first. The head ([`Builder::ty_regions`]'s caller) numbers what is left.
pub struct RegionSink<'s> {
    alloc: Alloc<'s>,
    slots: Vec<Slot, Alloc<'s>>,
    frags: Vec<Frag, Alloc<'s>>,
    /// Each elided lifetime's site, by its id.
    elided: blaze_own::Vec<Range, Alloc<'s>>,
    /// `Self`'s regions where `Self` is a type (an impl's self type, numbered as its members'
    /// container group); `None` where they are not known.
    self_slots: Option<Vec<Region, Alloc<'s>>>,
    /// In a trait impl's member, the impl's trait reference's column (its trait's lifetimes as
    /// the header writes them first, `heads_regions.rs` `header_trait`), numbered as `Self`'s;
    /// `None` where there is none or it is not known.
    impl_trait: Option<Vec<Region, Alloc<'s>>>,
    /// The container's non-higher-ranked trait bounds on its type parameters: `(param, trait_,
    /// start, len)` indexing `param_bound_regions`.
    param_bounds: blaze_own::Vec<(u32, ItemKey, u32, u32), Alloc<'s>>,
    param_bound_regions: blaze_own::Vec<Region, Alloc<'s>>,
    /// The captured lifetime count of each opaque type of the item being made.
    opaques: blaze_own::Vec<(ItemKey, u32), Alloc<'s>>,
    /// The regions each of `opaques` captures, one run per opaque ([`Captures`]).
    captured: blaze_own::Vec<Region, Alloc<'s>>,
    scratch: blaze_own::Vec<Slot, Alloc<'s>>,
    refused: Option<RegionRefusal>,
    /// The part of the signature being laid ([`RegionSink::set_part`]).
    part: SigPart,
    /// The part `refused` was met in.
    refused_part: SigPart,
    /// Nesting depth while laying defaulted type arguments from head text.
    default_depth: u8,
}

impl<'s> RegionSink<'s> {
    pub fn new_in(alloc: Alloc<'s>) -> RegionSink<'s> {
        RegionSink {
            alloc,
            slots: Vec::new_in(alloc),
            frags: Vec::new_in(alloc),
            elided: blaze_own::Vec::new_in(alloc),
            self_slots: None,
            impl_trait: None,
            param_bounds: blaze_own::Vec::new_in(alloc),
            param_bound_regions: blaze_own::Vec::new_in(alloc),
            opaques: blaze_own::Vec::new_in(alloc),
            captured: blaze_own::Vec::new_in(alloc),
            scratch: blaze_own::Vec::new_in(alloc),
            refused: None,
            part: SigPart::Whole,
            refused_part: SigPart::Whole,
            default_depth: 0,
        }
    }

    /// `Self`'s regions for the types made next (`None`: not known; a `Self` with regions is
    /// then refused by name).
    pub fn set_self(&mut self, regions: Option<&[Region]>) {
        let alloc = self.alloc;
        self.self_slots = regions.map(|r| {
            let mut v = Vec::with_capacity_in(r.len(), alloc);
            v.extend_from_slice(r);
            v
        });
    }

    pub fn self_regions(&self) -> Option<&[Region]> {
        self.self_slots.as_deref()
    }

    /// Snapshot of slot, elided-site and refusal state before walking a type macro's expansion.
    pub(super) fn mark_expansion(&self) -> (usize, usize, bool) {
        (self.slots.len(), self.elided.len(), self.refused.is_some())
    }

    /// Rewrites ranges recorded while walking a type macro's expansion (which are relative to
    /// the expanded text) to the macro call's range `at` in the enclosing item.
    pub(super) fn remap_expansion_ranges(&mut self, mark: (usize, usize, bool), at: Range) {
        let (slots_from, elided_from, had_refused) = mark;
        for slot in self.slots.get_mut(slots_from..).into_iter().flatten() {
            match &mut slot.kind {
                SlotKind::Named(_, r) | SlotKind::ObjectDefault(r) => *r = at,
                _ => {}
            }
        }
        for r in self.elided.get_mut(elided_from..).into_iter().flatten() {
            *r = at;
        }
        if !had_refused
            && let Some(why) = &mut self.refused
        {
            match why {
                RegionRefusal::Undeclared { at: r }
                | RegionRefusal::LifetimeArgs { at: r, .. }
                | RegionRefusal::ObjectDefault { at: r }
                | RegionRefusal::NotLaid { at: r, .. }
                | RegionRefusal::NoType { at: r } => *r = at,
                RegionRefusal::Regions(_) | RegionRefusal::NoContainer { .. } => {}
            }
        }
    }

    /// The impl's trait reference's column for the types made next, in the numbering `Self`'s
    /// regions are in (`None`: none or not known; a `Self::Name` of a trait with lifetimes is
    /// then refused by name).
    pub fn set_impl_trait(&mut self, regions: Option<&[Region]>) {
        let alloc = self.alloc;
        self.impl_trait = regions.map(|r| {
            let mut v = Vec::with_capacity_in(r.len(), alloc);
            v.extend_from_slice(r);
            v
        });
    }

    /// The container's non-higher-ranked trait bounds on its type parameters.
    pub fn set_param_bounds(&mut self, bounds: &[ContainerParamBound]) {
        self.param_bounds.clear();
        self.param_bound_regions.clear();
        for b in bounds {
            let start = u32::try_from(self.param_bound_regions.len()).unwrap_or(u32::MAX);
            self.param_bound_regions.extend_from_slice(&b.regions);
            let len = u32::try_from(b.regions.len()).unwrap_or(u32::MAX);
            self.param_bounds.push((b.param, b.trait_, start, len));
        }
    }

    /// What a container's member's walk is given ([`SelfGiven`]): `Self`'s regions
    /// ([`Self::set_self`]), the impl's trait column ([`Self::set_impl_trait`]), and the
    /// container's parameter bounds ([`Self::set_param_bounds`]).
    pub fn set_given(&mut self, given: SelfGiven<'_>) {
        self.set_self(given.self_);
        self.set_impl_trait(given.impl_trait);
        self.set_param_bounds(given.param_bounds);
    }

    /// The captured lifetimes of the opaques of the item being made.
    pub fn set_opaques(&mut self, captures: Captures<'_>) {
        self.opaques.clear();
        self.opaques.extend_from_slice(captures.counts);
        self.captured.clear();
        self.captured.extend_from_slice(captures.regions);
    }

    /// The captured lifetime counts of the opaques of the item being made.
    pub fn opaques(&self) -> &[(ItemKey, u32)] {
        &self.opaques
    }

    /// The captured lifetimes of the opaques of the item being made.
    pub fn captures(&self) -> Captures<'_> {
        Captures { counts: &self.opaques, regions: &self.captured }
    }

    /// The first refusal of any type this sink laid, if one was refused.
    pub fn refused(&self) -> Option<RegionRefusal> {
        self.refused
    }

    /// The part of the signature the types laid next are (an input by index, the output, an
    /// impl's header): what [`RegionSink::refused_part`] names of a refusal met while laying
    /// them. [`SigPart::Whole`] until set.
    pub fn set_part(&mut self, part: SigPart) {
        self.part = part;
    }

    /// The part the first refusal ([`RegionSink::refused`]) was met in; [`SigPart::Whole`]
    /// when nothing was refused or no part was set.
    pub fn refused_part(&self) -> SigPart {
        self.refused_part
    }

    /// Refuses the regions of what is being made (the first refusal is kept, with the part it
    /// was met in).
    pub fn refuse(&mut self, why: RegionRefusal) {
        if self.refused.is_none() {
            self.refused = Some(why);
            self.refused_part = self.part;
        }
    }

    /// The fragments on the stack, the oldest first.
    pub fn frags(&self) -> &[Frag] {
        &self.frags
    }

    pub fn slots_of(&self, f: Frag) -> &[Slot] {
        self.slots.get(f.start as usize..(f.start + f.len) as usize).unwrap_or(&[])
    }

    /// The site of the elided lifetime `id`.
    pub fn elided_at(&self, id: u32) -> Option<Range> {
        usize::try_from(id).ok().and_then(|i| self.elided.get(i).copied())
    }

    /// A new elided lifetime at `at`.
    pub fn new_elided(&mut self, at: Range) -> SlotKind {
        let id = u32::try_from(self.elided.len()).unwrap_or(u32::MAX);
        self.elided.push(at);
        SlotKind::Elided(id)
    }

    /// Where the stack is: what [`RegionSink::reset`] goes back to (a walk that stopped half
    /// way, as a type that names nothing does, leaves its children's fragments).
    pub fn mark(&self) -> (usize, usize) {
        (self.frags.len(), self.slots.len())
    }

    pub fn reset(&mut self, mark: (usize, usize)) {
        self.frags.truncate(mark.0);
        self.slots.truncate(mark.1);
    }

    /// Pushes a fragment of `slots` whose type has `binders` binder nodes (a type the head
    /// makes itself: `&self`, `self`).
    pub fn push(&mut self, slots: &[Slot], binders: u32) {
        let start = self.slots.len() as u32;
        self.slots.extend_from_slice(slots);
        self.frags.push(Frag { start, len: slots.len() as u32, binders });
    }

    /// The `k`-th of the top `n` fragments.
    fn kid(&self, n: usize, k: usize) -> Option<Frag> {
        self.frags.len().checked_sub(n).and_then(|b| self.frags.get(b + k)).copied()
    }

    /// Replaces the top `n` fragments with the scratch's slots, of `binders` binder nodes.
    fn replace(&mut self, n: usize, binders: u32) {
        let keep = self.frags.len().saturating_sub(n);
        let base = self.frags.get(keep).map_or(self.slots.len(), |f| f.start as usize);
        self.frags.truncate(keep);
        self.slots.truncate(base);
        let start = self.slots.len() as u32;
        self.slots.extend_from_slice(&self.scratch);
        self.frags.push(Frag { start, len: self.scratch.len() as u32, binders });
        self.scratch.clear();
    }

    /// `own` (a reference's slot) then the `n` children's slots in order: a child trait
    /// object's default is `own` where the child is the reference's pointee, `'static`
    /// anywhere else (a raw pointer, a slice, an array, a tuple: no lifetime of theirs).
    fn wrap(&mut self, n: usize, own: Option<SlotKind>) -> Laid {
        self.scratch.clear();
        if let Some(o) = own {
            self.scratch.push(Slot::written(o));
        }
        let mut binders = 0u32;
        for k in 0..n {
            let f = self.kid(n, k).ok_or(RegionRefusal::not_laid(NotLaidWhy::Stack, Range::new(0, 0)))?;
            for i in f.start..f.start + f.len {
                let slot = self.slots[i as usize];
                let slot = match (slot.kind, own) {
                    (SlotKind::ObjectDefault(_), Some(o)) => Slot::quiet(o),
                    (SlotKind::ObjectDefault(_), None) => Slot::quiet(SlotKind::Static),
                    _ => slot.shifted(binders),
                };
                self.scratch.push(slot);
            }
            binders += f.binders;
        }
        Ok(binders)
    }

    /// A fn pointer: binder 0 of its own slots. Its inputs' elided lifetimes are its own
    /// variables, one each in input slot order; its output's take the one lifetime its inputs
    /// use, or are E0106 (the Reference, "Lifetime elision in functions", which "applies to
    /// function pointer types" as to fns).
    fn fn_ptr(&mut self, n: usize, ret_written: bool, r: Range) -> Result<Laid, Why> {
        let inputs = if ret_written { n.saturating_sub(1) } else { n };
        self.scratch.clear();
        let mut vars: Vec<u32, Alloc<'s>> = Vec::new_in(self.alloc);
        let mut binders = 1u32;
        let mut input_end = 0usize;
        for k in 0..n {
            let f = soft!(self.kid(n, k).ok_or(RegionRefusal::not_laid(NotLaidWhy::Stack, r)));
            for i in f.start..f.start + f.len {
                let slot = self.slots[i as usize];
                let slot = match slot.kind {
                    SlotKind::ObjectDefault(_) => Slot::quiet(SlotKind::Static),
                    SlotKind::Elided(id) if k < inputs => {
                        let var = match vars.iter().position(|&v| v == id) {
                            Some(p) => p as u32,
                            None => {
                                vars.push(id);
                                vars.len() as u32 - 1
                            }
                        };
                        Slot::written(SlotKind::Late { binder: 0, var })
                    }
                    _ => slot.shifted(binders),
                };
                self.scratch.push(slot);
            }
            binders += f.binders;
            if k < inputs {
                input_end = self.scratch.len();
            }
        }
        if ret_written {
            let one = used_lifetimes(&self.scratch[..input_end], fn_ptr_local);
            for i in input_end..self.scratch.len() {
                if let SlotKind::Elided(_) = self.scratch[i].kind {
                    match one {
                        Used::One(k) => self.scratch[i].kind = k,
                        // A regions refusal by name, the head made: E0106 is the program's
                        // only for a fn's own return type (heads.rs `missing_lifetime`).
                        Used::None | Used::Many => {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::FnPtrElision, r)));
                        }
                    }
                }
            }
        }
        Ok(Ok(binders))
    }

    /// `for<'a, ..> fn(..)` or a Rust 2015/2018 `for<'a, ..> Trait<'a>`: its names are the
    /// fn pointer's or trait object's variables, before the elided ones.
    fn for_binder(&mut self, tys: tc::PTys<'_>, v: &View<'_>, binder: lf::List, made: PTy, r: Range) -> Result<Laid, Why> {
        if !matches!(tys.node(made), Ok(PNode::FnPtr(..) | PNode::Dynamic(..))) {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::ForBinder, r)));
        }
        let mut names: Vec<Name, Alloc<'s>> = Vec::new_in(self.alloc);
        for &row in v.rows(binder)? {
            if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(row)? {
                names.push(Name(v.name(name)?));
            }
        }
        let nb = names.len() as u32;
        let f = soft!(self.kid(1, 0).ok_or(RegionRefusal::not_laid(NotLaidWhy::Stack, r)));
        self.scratch.clear();
        for i in f.start..f.start + f.len {
            let slot = self.slots[i as usize];
            let kind = match slot.kind {
                SlotKind::Late { binder: 0, var } => SlotKind::Late { binder: 0, var: var + nb },
                SlotKind::Named(n, at) => match names.iter().position(|&x| x == n) {
                    Some(p) => SlotKind::Late { binder: 0, var: p as u32 },
                    None => SlotKind::Named(n, at),
                },
                k => k,
            };
            self.scratch.push(Slot { kind, counts: slot.counts });
        }
        Ok(Ok(f.binders))
    }

    /// An `impl Trait` type: a parameter's (`Param`) has no slot; a return type's opaque has
    /// its captured lifetimes, in its fn's numbering ([`Captures`]): `Param(0..n)` under the
    /// every-lifetime rule (edition 2024, an `async fn`'s future, a trait's or trait impl's
    /// fn), the listed ones under `use<..>` or a pre-2024 RPIT's named-lifetimes rule.
    fn opaque(&mut self, tys: tc::PTys<'_>, made: PTy, r: Range) -> Laid {
        self.scratch.clear();
        match tys.node(made) {
            Ok(PNode::Opaque(k, _)) => {
                let captures = Captures { counts: &self.opaques, regions: &self.captured };
                let run = captures.of(k).ok_or(RegionRefusal::not_laid(NotLaidWhy::OpaqueCaptures, r))?;
                for &c in run {
                    self.scratch.push(Slot::quiet(SlotKind::Given(c)));
                }
                Ok(0)
            }
            Ok(_) => Ok(0),
            Err(e) => Err(RegionRefusal::Regions(RegionError::Table(e))),
        }
    }
}

/// Where a path's segment stands, for the lifetimes written on it ([`Builder::path_regions`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SegRole {
    /// The path's last segment: its lifetimes are the named item's (an ADT's, an alias's, a
    /// projection's own).
    Last,
    /// The last segment of a qualified path's trait (`Tr<'a>` in `<T as Tr<'a>>::Name`): its
    /// lifetimes are the trait reference's, the alias's first slots.
    QualifiedTrait,
    /// Any other segment: a lifetime there is refused by name
    /// ([`NotLaidWhy::EarlySegmentLifetime`]).
    Early,
}

/// The slot a written lifetime gives (`'_` and none written are elided, a new one each).
fn lifetime_kind(v: &View<'_>, n: Option<lf::NameIdx>, r: Range, s: &mut RegionSink<'_>) -> Result<SlotKind, Why> {
    let Some(n) = n else { return Ok(s.new_elided(r)) };
    let text = v.name_text(n)?;
    let at = place(v, v.name_range(n))?;
    Ok(match text.strip_prefix('\'').unwrap_or(text) {
        "_" => s.new_elided(at),
        "static" => SlotKind::Static,
        _ => SlotKind::Named(Name(v.name(n)?), at),
    })
}

impl<'b, 'n, H: Heads, S: NamesSource<'n>> Builder<'b, H, S> {
    /// The region step of the type `t` the walk just made (`made`, of the children `kids`,
    /// with `args` its path's arguments): its fragment replaces its children's on `s`.
    #[expect(clippy::too_many_arguments, reason = "one made type: its written form, its children, its path's arguments and the sink")]
    fn regions_made(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, t: lf::TypeId, made: PTy, kids: &[PTy], args: &[PTy], s: &mut RegionSink<'_>) -> Result<(), Why> {
        let n = kids.len();
        let r = place(v, v.type_range(t))?;
        let laid = if s.refused.is_some() {
            Ok(0)
        } else {
            match v.ty(t)? {
                lf::TypeRef::Ref { lifetime, .. } => {
                    let own = lifetime_kind(v, lifetime, r, s)?;
                    s.wrap(n, Some(own))
                }
                lf::TypeRef::Ptr { .. } | lf::TypeRef::Slice(_) | lf::TypeRef::Array { .. } | lf::TypeRef::Pat { .. } | lf::TypeRef::Tuple(_) | lf::TypeRef::Never => s.wrap(n, None),
                lf::TypeRef::Record(fields) => self.record_regions(tys, v, fields, kids, n, s, r)?,
                lf::TypeRef::Union(_) => {
                    let raw_count: u32 = kids
                        .iter()
                        .map(|&kid| match tys.view().get(kid).copied() {
                            Some(PNode::Union(inner)) => inner.len,
                            _ => 1,
                        })
                        .sum();
                    let kept_count = match tys.view().get(made).copied() {
                        Some(PNode::Union(l)) => l.len,
                        _ => raw_count,
                    };
                    let kid_slots: u32 = (0..n).filter_map(|k| s.kid(n, k).map(|f| f.len)).sum();
                    if kid_slots == 0 || raw_count == kept_count {
                        s.wrap(n, None)
                    } else {
                        Err(RegionRefusal::not_laid(NotLaidWhy::OtherForm, r))
                    }
                }
                lf::TypeRef::FnPtr { ret, .. } => s.fn_ptr(n, ret.is_some(), r)?,
                lf::TypeRef::For { binder, .. } => s.for_binder(tys.view(), v, binder, made, r)?,
                lf::TypeRef::Path(p) => self.path_regions(tys, env, v, p, made, args, n, s, r)?,
                lf::TypeRef::Dyn(bounds) => self.dyn_regions(tys, env, v, bounds, made, kids, s, r)?,
                lf::TypeRef::Impl(_) => s.opaque(tys.view(), made, r),
                lf::TypeRef::Infer => Err(RegionRefusal::not_laid(NotLaidWhy::Infer, r)),
                lf::TypeRef::Macro(_) => Err(RegionRefusal::not_laid(NotLaidWhy::TypeMacro, r)),
                _ => Err(RegionRefusal::not_laid(NotLaidWhy::OtherForm, r)),
            }
        };
        match laid {
            Ok(binders) => s.replace(n, binders),
            Err(why) => {
                s.refuse(why);
                s.scratch.clear();
                s.replace(n, 0);
            }
        }
        Ok(())
    }

    /// A structural record type's region slots in canonical alphabetical field-name order,
    /// expanding any `..Base` spread entries and applying left-to-right field overrides.
    #[expect(clippy::too_many_arguments, reason = "one made record: its fields, lowered kids and region sink")]
    fn record_regions(&self, tys: &mut PortableTys<'_>, v: &View<'_>, fields: lf::List, kids: &[PTy], n: usize, s: &mut RegionSink<'_>, r: Range) -> Result<Laid, Why> {
        let kid_slots: u32 = (0..n).filter_map(|k| s.kid(n, k).map(|f| f.len)).sum();
        let kid_binders: u32 = (0..n).filter_map(|k| s.kid(n, k).map(|f| f.binders)).sum();
        if kid_slots == 0 && kid_binders == 0 {
            return Ok(s.wrap(n, None));
        }
        if kid_binders != 0 {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OtherForm, r)));
        }
        let rows = v.rows(fields)?;
        let alloc = s.alloc;
        let tv = tys.view();
        let arity = HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names };
        let mut merged: blaze_own::Vec<(&str, Name, u32, u32), Alloc<'_>> = blaze_own::Vec::with_capacity_in(rows.len(), alloc);
        for (i, &row) in rows.iter().enumerate() {
            let f = lf::Field::decode(row)?;
            let frag = soft!(s.kid(n, i).ok_or(RegionRefusal::not_laid(NotLaidWhy::Stack, r)));
            match f.name {
                Some(nm) => {
                    let text = v.name_text(nm)?;
                    let name = Name::of(text);
                    if let Some(slot) = merged.iter_mut().find(|(_, existing, _, _)| *existing == name) {
                        *slot = (text, name, frag.start, frag.len);
                    } else {
                        merged.push((text, name, frag.start, frag.len));
                    }
                }
                None => {
                    let Some(&kid_ty) = kids.get(i) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                    };
                    let Some(PNode::Record(base_schema, base_list)) = tv.get(kid_ty).copied() else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OtherForm, r)));
                    };
                    let Some(base_names) = tv.record_names(base_schema) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OtherForm, r)));
                    };
                    let Ok(base_tys) = tv.list(base_list) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OtherForm, r)));
                    };
                    let mut cursor = frag.start;
                    for (&bnm, &bty) in base_names.iter().zip(base_tys.iter()) {
                        let sub = soft!(subtree(tv, bty, &arity, alloc));
                        let Some(btext) = self.heads.record_field_name(bnm) else {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OtherForm, r)));
                        };
                        if let Some(slot) = merged.iter_mut().find(|(_, existing, _, _)| *existing == bnm) {
                            *slot = (btext, bnm, cursor, sub.slots);
                        } else {
                            merged.push((btext, bnm, cursor, sub.slots));
                        }
                        cursor = cursor.saturating_add(sub.slots);
                    }
                    if cursor != frag.start.saturating_add(frag.len) {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                    }
                }
            }
        }
        merged.sort_by(|a, b| a.0.cmp(b.0));
        s.scratch.clear();
        for &(_, _, start, len) in merged.iter() {
            let Ok(st) = usize::try_from(start) else {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
            };
            let Ok(en) = usize::try_from(start.saturating_add(len)) else {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
            };
            let Some(slice) = s.slots.get(st..en) else {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
            };
            for &slot in slice {
                let slot = match slot.kind {
                    SlotKind::ObjectDefault(_) => Slot::quiet(SlotKind::Static),
                    _ => slot,
                };
                s.scratch.push(slot);
            }
        }
        Ok(Ok(0))
    }

    /// A path's slots. A struct's, enum's or union's: its written lifetime arguments (or one
    /// elided lifetime per declared one when none is written: the Reference, "Lifetime
    /// elision", "elided in paths"), then its arguments' slots; a type alias's: its type's
    /// with each parameter's argument's slots where the parameter is; `Self`'s: the head's;
    /// a type parameter's and a primitive's: none. A projection's are laid only when it has
    /// none (its own come from its trait's lifetime arguments: not laid yet).
    #[expect(clippy::too_many_arguments, reason = "one made path: its written form, its arguments and the sink")]
    fn path_regions(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, p: PathIdx, made: PTy, args: &[PTy], n: usize, s: &mut RegionSink<'_>, r: Range) -> Result<Laid, Why> {
        // The segments as `path_args` reads them (the last one's `Fn(A) -> B` sugar too).
        let (abs, texts, segs, _) = path_texts_of(v, p, true)?;
        let alloc = s.alloc;
        // Each argument's slots in `args`' order (`path_args`' walk): a written type
        // argument's from its fragment, a one-name or const argument's made here.
        let mut pool: Vec<Slot, Alloc<'_>> = Vec::new_in(alloc);
        let mut index: Vec<(u32, u32, u32), Alloc<'_>> = Vec::new_in(alloc);
        let mut lifetimes: Vec<SlotKind, Alloc<'_>> = Vec::new_in(alloc);
        // The lifetimes written on a qualified path's trait (`<T as Tr<'a>>::Name`): the
        // trait reference's own arguments, the alias's first slots ([`Builder::qualified_slots`]).
        let mut trait_lifetimes: Vec<SlotKind, Alloc<'_>> = Vec::new_in(alloc);
        let mut stray = false;
        let mut todo: Vec<(lf::Segment, SegRole), Alloc<'_>> = Vec::new_in(alloc);
        let anchored = segs.first().is_some_and(|s0| s0.kind == lf::SegKind::Anchor);
        for (k, seg) in segs.iter().enumerate() {
            if seg.kind == lf::SegKind::Anchor {
                // `<T>::Name` writes no trait: no trait segment is walked; it is laid below only
                // when its projection holds no slot.
                let Some(tr) = seg.trait_ else { continue };
                let lf::TypeRef::Path(tp) = v.ty(tr)? else { return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::QualifiedNoTrait, r))) };
                let tsegs = path_texts(v, tp)?.2;
                let nt = tsegs.len();
                for (j, &ts) in tsegs.iter().enumerate() {
                    todo.push((ts, if j + 1 == nt { SegRole::QualifiedTrait } else { SegRole::Early }));
                }
            } else {
                todo.push((*seg, if k + 1 == segs.len() { SegRole::Last } else { SegRole::Early }));
            }
        }
        let mut next_kid = 0usize;
        let mut at = 0usize;
        // Made at each use, not held: a one-name alias argument hands the whole sink on.
        // (Written out at its two uses: a closure cannot return a borrow of its argument's field
        // under one inferred signature: "lifetime may not live long enough".)
        let take_kid =|s_slots: &[Slot], f: Frag, pool: &mut Vec<Slot, Alloc<'_>>, index: &mut Vec<(u32, u32, u32), Alloc<'_>>| {
            let start = pool.len() as u32;
            pool.extend_from_slice(s_slots.get(f.start as usize..(f.start + f.len) as usize).unwrap_or(&[]));
            index.push((start, f.len, f.binders));
        };
        if anchored {
            let f = soft!(s.kid(n, next_kid).ok_or(RegionRefusal::not_laid(NotLaidWhy::Stack, r)));
            take_kid(&s.slots, f, &mut pool, &mut index);
            next_kid += 1;
            at += 1;
        }
        for &(seg, role) in &todo {
            if !seg.has_args {
                continue;
            }
            for &row in v.rows(seg.args)? {
                match lf::GenericArg::decode(row)? {
                    lf::GenericArg::Type(ta) if seg.parenthesized || bare_name(v, ta)?.is_none() => {
                        let f = soft!(s.kid(n, next_kid).ok_or(RegionRefusal::not_laid(NotLaidWhy::Stack, r)));
                        take_kid(&s.slots, f, &mut pool, &mut index);
                        next_kid += 1;
                        at += 1;
                    }
                    lf::GenericArg::Type(_) | lf::GenericArg::Const(_) => {
                        // Made in `path_args`: a type parameter, a primitive, a constant (no
                        // slot), or a path to an ADT with no arguments, whose lifetimes are
                        // elided.
                        let a = soft!(args.get(at).copied().ok_or(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                        let sub = soft!(subtree(tys.view(), a, &HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names }, alloc));
                        // A one-name type alias (`Result<B256, ErrorObjectOwned>`, jsonrpsee's
                        // `type ErrorObjectOwned = ErrorObject<'static>`): its made type is its
                        // expansion's, so the ADT rule below would read `ErrorObject` written
                        // bare and elide its lifetime, a false E0106 in a return type with two
                        // input lifetimes (tempo `zone_node::rpc::operator_commitment`). Its
                        // slots are the alias's, as the path arm lays a written `R<..>`.
                        // In Rust 2015/2018 a one-name bare trait object (`Box<Error>`) is
                        // likewise laid by `path_regions_found` so its `ObjectDefault` slot is
                        // resolved by its enclosing container.
                        if sub.slots != 0
                            && let lf::GenericArg::Type(ta) = lf::GenericArg::decode(row)?
                            && let Some(name) = bare_name(v, ta)?
                            && !env.params.iter().any(|(nm, _)| *nm == Name::of(name))
                            && let lf::TypeRef::Path(tp) = v.ty(ta)?
                        {
                            let ar = place(v, v.type_range(ta))?;
                            let found = match self.resolve_at(env, v, tp, false, &[name], Ns::Type, ar) {
                                Ok(found) => found,
                                Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                                Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::Resolve, ar))),
                            };
                            let allow_bare_trait = self.allows_bare_trait_object_at(env, v, tp, ar)?;
                            if matches!(found.kind, Kind::TypeAlias | Kind::Struct | Kind::Enum | Kind::Union)
                                || (allow_bare_trait && matches!(found.kind, Kind::Trait | Kind::TraitAlias))
                            {
                                s.scratch.clear();
                                let b = match self.path_regions_found(tys, env, v, tp, (a, &[]), s, ar, &found, (&[], &[], &[]), sub)? {
                                    Ok(b) => b,
                                    Err(why) => return Ok(Err(why)),
                                };
                                let start = pool.len() as u32;
                                pool.extend_from_slice(&s.scratch);
                                index.push((start, s.scratch.len() as u32, b));
                                s.scratch.clear();
                                at += 1;
                                continue;
                            }
                        }
                        let own = match tys.view().node(a) {
                            Ok(PNode::Adt(k, _)) => soft!(declared_lifetimes(self.heads, self.resolver.names, k, Declared::Adt).ok_or(RegionRefusal::not_laid(NotLaidWhy::DeclaredCount, r))),
                            _ => 0,
                        };
                        if sub.slots != own {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OneNameArgs, r)));
                        }
                        let start = pool.len() as u32;
                        for _ in 0..own {
                            let id = s.elided.len() as u32;
                            s.elided.push(r);
                            pool.push(Slot::written(SlotKind::Elided(id)));
                        }
                        index.push((start, own, sub.binders));
                        at += 1;
                    }
                    lf::GenericArg::AssocType { args, .. } if !seg.parenthesized && args.len == 0 => {
                        let f = soft!(s.kid(n, next_kid).ok_or(RegionRefusal::not_laid(NotLaidWhy::Stack, r)));
                        take_kid(&s.slots, f, &mut pool, &mut index);
                        next_kid += 1;
                        at += 1;
                    }
                    lf::GenericArg::Lifetime(nm) if role != SegRole::Early => {
                        let text = v.name_text(nm)?;
                        let lat = place(v, v.name_range(nm))?;
                        let kind = match text.strip_prefix('\'').unwrap_or(text) {
                            "_" => {
                                let id = s.elided.len() as u32;
                                s.elided.push(lat);
                                SlotKind::Elided(id)
                            }
                            "static" => SlotKind::Static,
                            _ => SlotKind::Named(Name(v.name(nm)?), lat),
                        };
                        match role {
                            SegRole::QualifiedTrait => trait_lifetimes.push(kind),
                            SegRole::Last | SegRole::Early => lifetimes.push(kind),
                        }
                    }
                    lf::GenericArg::Lifetime(_) => stray = true,
                    _ => {}
                }
            }
            if seg.parenthesized && seg.ty.is_some() {
                let f = soft!(s.kid(n, next_kid).ok_or(RegionRefusal::not_laid(NotLaidWhy::Stack, r)));
                take_kid(&s.slots, f, &mut pool, &mut index);
                next_kid += 1;
                at += 1;
            }
        }
        if at != args.len() || next_kid != n {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
        }
        if stray {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::EarlySegmentLifetime, r)));
        }
        let param = |name: &str| env.params.iter().any(|(nm, _)| *nm == Name::of(name));
        let self_path = !abs && texts.first() == Some(&"Self");
        let whole = soft!(subtree(tys.view(), made, &HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names }, alloc));
        s.scratch.clear();
        // `T::Name` and, in a trait, `Self::Name` whose trait holds a lifetime: read through
        // the bound that names the trait ([`Builder::projection_slots`]).
        if !anchored && !abs && texts.len() == 2 && whole.slots != 0 {
            let through = match env.self_ {
                _ if param(texts[0]) => Some(Through::Param(texts[0])),
                SelfTy::Trait(tr) if self_path => Some(Through::SelfTrait(tr)),
                // In a trait impl `Self::Name` is the projection `<SelfTy as Tr>::Name` (the
                // erased walk's `path_ty`): laid as one, `Self`'s slots the impl's self type's.
                SelfTy::Ty(_, Some((tr, _))) if self_path => Some(Through::SelfImpl(tr)),
                _ => None,
            };
            if let Some(through) = through {
                return self.projection_slots(tys, env, v, made, through, (&lifetimes, &pool, &index), s, r);
            }
        }
        // A qualified path `<T as Tr<'a>>::Name`: laid as the projection it is, never
        // normalized here (a head is a function of its own text, rules 65 and 21: no trait
        // selection). Its slots are the alias node's, in `each_slot_node`'s order: its trait's
        // lifetimes (those written on the trait's last segment, `trait_lifetimes`), its own (a
        // GAT's, written on `Name`), then its arguments' (the self type's, the trait's type
        // arguments', `Name`'s), each from the stack. A reader that knows the impl the
        // projection selects maps the normalized type's regions through the impl's column
        // (`RegionFacts::impl_assoc`, the impl's self column for its parameters); one that does
        // not keeps the alias's positions. Lifetimes written on a trait that declares none are
        // a count the walk refuses there (E0107), never dropped.
        // `<T>::Name` with no trait whose projection holds a lifetime: its trait's lifetimes are
        // the bound's (as `T::Name`'s, `projection_slots`), not read through this form yet.
        if anchored && segs.first().is_some_and(|s0| s0.trait_.is_none()) && whole.slots != 0 {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::QualifiedNoTrait, r)));
        }
        if anchored && (whole.slots != 0 || !trait_lifetimes.is_empty()) {
            return Ok(self.qualified_slots(tys, made, (&trait_lifetimes, &lifetimes, &pool, &index), whole, s, r));
        }
        if anchored || (self_path && texts.len() >= 2) || (!abs && texts.len() == 2 && param(texts[0])) {
            if !lifetimes.is_empty() || !pool.is_empty() || whole.slots != 0 {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::ProjectionPath, r)));
            }
            return Ok(Ok(whole.binders));
        }
        if self_path {
            if let SelfTy::Ty(..) = env.self_ {
                let Some(given) = s.self_slots.as_ref() else {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::SelfUnknown, r)));
                };
                if given.len() as u32 != whole.slots {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                }
                for &g in given.iter() {
                    s.scratch.push(Slot::quiet(SlotKind::Given(g)));
                }
            }
            return Ok(Ok(whole.binders));
        }
        if !abs && texts.len() == 1 && param(texts[0]) {
            return Ok(Ok(0));
        }
        // The erased walk resolved this path in this env a step before (`walk_ty`'s `make`), so
        // a refusal here is the regions walk's own and never the head's: the regions are
        // refused by name, the erased type stands (a need still goes up, to be met).
        let found = match self.resolve_at(env, v, p, abs, &texts, Ns::Type, r) {
            Ok(found) => found,
            Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
            Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::Resolve, r))),
        };
        self.path_regions_found(tys, env, v, p, (made, args), s, r, &found, (&lifetimes, &pool, &index), whole)
    }

    /// [`Builder::path_regions`] once its path is resolved to `found`: the ADT's, the type
    /// alias's, or (in Rust 2015/2018) the bare trait object's slots from the arguments'
    /// (`pool`, by `index`) and the written lifetimes. In a
    /// frame of its own: `path_regions`' frame stays on the stack under the resolution, which
    /// can make another head (blaze-int notes t29 overflow-bt4.txt: 55,072 bytes), and a debug
    /// frame gives each temporary of these walks a slot.
    #[inline(never)]
    #[expect(clippy::too_many_arguments, reason = "one resolved path: its written form, its gathered arguments and the sink")]
    fn path_regions_found<'s>(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, p: PathIdx, (made, args): (PTy, &[PTy]), s: &mut RegionSink<'s>, r: Range, found: &crate::resolve::Target, (lifetimes, pool, index): (&[SlotKind], &[Slot], &[(u32, u32, u32)]), whole: Sub) -> Result<Laid, Why> {
        let alloc = s.alloc;
        match found.kind {
            Kind::Struct | Kind::Enum | Kind::Union => {
                let Ok(PNode::Adt(k, l)) = tys.view().node(made) else {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                };
                let want = soft!(declared_lifetimes(self.heads, self.resolver.names, k, Declared::Adt).ok_or(RegionRefusal::not_laid(NotLaidWhy::DeclaredCount, r)));
                if lifetimes.is_empty() {
                    for _ in 0..want {
                        let id = s.elided.len() as u32;
                        s.elided.push(r);
                        s.scratch.push(Slot::written(SlotKind::Elided(id)));
                    }
                } else if lifetimes.len() as u32 == want {
                    s.scratch.extend(lifetimes.iter().map(|&k| Slot::written(k)));
                } else {
                    return Ok(Err(RegionRefusal::LifetimeArgs { at: r, want, got: lifetimes.len() as u32 }));
                }
                let mut adt_lts: Vec<SlotKind, Alloc<'s>> = Vec::with_capacity_in(want as usize, alloc);
                for slot in s.scratch.iter().take(want as usize) {
                    adt_lts.push(slot.kind);
                }
                let list_slice = soft!(tys.view().list(l).map_err(RegionError::Table));
                let mut list: Vec<PTy, Alloc<'s>> = Vec::with_capacity_in(list_slice.len(), alloc);
                list.extend_from_slice(list_slice);
                let mut def_pool: Vec<Slot, Alloc<'s>> = Vec::new_in(alloc);
                let mut def_index: Vec<(u32, u32, u32), Alloc<'s>> = Vec::new_in(alloc);
                let mut added: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                let mut item_lts = DefaultLifetimes::Fixed(&adt_lts);
                let mut binders = 0u32;
                for (i, &a) in list.iter().enumerate() {
                    match index.get(i) {
                        Some(&(start, len, b)) => {
                            for &slot in &pool[start as usize..(start + len) as usize] {
                                let slot = match slot.kind {
                                    // A trait object argument of a type with no lifetime
                                    // parameter: `'static` (`Box<dyn Tr>`).
                                    SlotKind::ObjectDefault(_) if want == 0 => Slot::quiet(SlotKind::Static),
                                    SlotKind::ObjectDefault(oat) => return Ok(Err(RegionRefusal::ObjectDefault { at: oat })),
                                    _ => slot.shifted(binders),
                                };
                                s.scratch.push(slot);
                            }
                            binders += b;
                        }
                        None => {
                            let sub = soft!(subtree(
                                tys.view(),
                                a,
                                &HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names },
                                alloc,
                            ));
                            if sub.slots != 0 {
                                let raw_default = self.heads.adt_generics(found.key).and_then(|(_, ds)| ds.get(i).copied().flatten());
                                added.clear();
                                if let Err(why) = self.lay_default_arg(
                                    tys,
                                    found.key,
                                    i,
                                    raw_default,
                                    sub,
                                    DefaultOwner::Adt { want },
                                    &mut item_lts,
                                    (pool, index),
                                    (&def_pool, &def_index),
                                    s,
                                    r,
                                    &mut added,
                                )? {
                                    return Ok(Err(why));
                                }
                                let Ok(dstart) = u32::try_from(def_pool.len()) else {
                                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                                };
                                let Ok(dlen) = u32::try_from(added.len()) else {
                                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                                };
                                def_pool.extend_from_slice(&added);
                                def_index.push((dstart, dlen, sub.binders));
                                for &slot in &added {
                                    s.scratch.push(slot.shifted(binders));
                                }
                            } else {
                                def_index.push((0, 0, sub.binders));
                            }
                            binders += sub.binders;
                        }
                    }
                }
                Ok(Ok(binders))
            }
            Kind::TypeAlias => {
                let Some(Some((g, body))) = self.heads.alias(found.key) else {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::AliasUnmade, r)));
                };
                let shown = v.text_at(v.path_range(p)?)?;
                let full = with_defaults(tys, args, g, self.heads.alias_defaults(found.key), shown, r)?;
                let mut visits: Vec<(PTy, PNode, u32), Alloc<'s>> = Vec::new_in(alloc);
                soft!(each_slot_node(
                    tys.view(),
                    body,
                    &HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names },
                    alloc,
                    &mut |t, node, own| {
                        visits.push((t, node, own));
                        Ok(())
                    },
                ));
                // The alias's own lifetimes as this use gives them: written (`R<'q>`), or, none
                // written, each an elided lifetime of this position (`fn f(x: R)`, the
                // position's elision rule decides it, as for an ADT's), made where first used.
                let laid = self.heads.alias_slots(found.key);
                let alias_want = laid.map(|l| l.lifetimes).or_else(|| self.resolver.names.declared_lifetimes(found.key));
                let mut given: Vec<Option<SlotKind>, Alloc<'s>> = Vec::new_in(alloc);
                match laid {
                    Some(l) if lifetimes.is_empty() => given.resize(l.lifetimes as usize, None),
                    Some(l) if lifetimes.len() as u32 == l.lifetimes => given.extend(lifetimes.iter().map(|&k| Some(k))),
                    Some(l) => return Ok(Err(RegionRefusal::LifetimeArgs { at: r, want: l.lifetimes, got: lifetimes.len() as u32 })),
                    None => {}
                }
                let mut def_pool: Vec<Slot, Alloc<'s>> = Vec::new_in(alloc);
                let mut def_index: Vec<(u32, u32, u32), Alloc<'s>> = Vec::new_in(alloc);
                let uses_default = full.len() > index.len()
                    && visits.iter().any(|&(_, node, _)| {
                        matches!(node, PNode::Param(pi) if (pi as usize) >= index.len() && (pi as usize) < full.len())
                    });
                if uses_default {
                    let mut added: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                    let mut item_lts = DefaultLifetimes::Lazy(&mut given);
                    for i in index.len()..full.len() {
                        let a = full[i];
                        let sub = soft!(subtree(
                            tys.view(),
                            a,
                            &HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names },
                            alloc,
                        ));
                        if sub.slots != 0 {
                            let raw_default = self.heads.alias_defaults(found.key).get(i).copied().flatten();
                            added.clear();
                            if let Err(why) = self.lay_default_arg(
                                tys,
                                found.key,
                                i,
                                raw_default,
                                sub,
                                DefaultOwner::Alias { want: alias_want },
                                &mut item_lts,
                                (pool, index),
                                (&def_pool, &def_index),
                                s,
                                r,
                                &mut added,
                            )? {
                                return Ok(Err(why));
                            }
                            let Ok(dstart) = u32::try_from(def_pool.len()) else {
                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                            };
                            let Ok(dlen) = u32::try_from(added.len()) else {
                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                            };
                            def_pool.extend_from_slice(&added);
                            def_index.push((dstart, dlen, sub.binders));
                        } else {
                            def_index.push((0, 0, sub.binders));
                        }
                    }
                }
                // Each binder node of the alias's type, its index among the made type's.
                let mut binder_at: Vec<u32, Alloc<'s>> = Vec::new_in(alloc);
                let mut next = 0usize;
                // The alias's type in pre-order: a parameter (a leaf) takes its argument's slots,
                // any other node its own from the alias's laid column.
                let mut binders = 0u32;
                for &(_, node, own) in &visits {
                    if let PNode::Param(i) = node
                        && (i as usize) < full.len()
                    {
                        match index.get(i as usize) {
                            Some(&(start, len, b)) => {
                                for &slot in &pool[start as usize..(start + len) as usize] {
                                    let slot = match slot.kind {
                                        // A trait object argument of a type alias with no lifetime
                                        // parameter (`type Arc<T> = alloc::sync::Arc<T>; Arc<dyn Tr>`):
                                        // `'static` (the Reference, "Default trait object lifetimes").
                                        SlotKind::ObjectDefault(_) if alias_want == Some(0) => Slot::quiet(SlotKind::Static),
                                        SlotKind::ObjectDefault(oat) => return Ok(Err(RegionRefusal::ObjectDefault { at: oat })),
                                        _ => slot.shifted(binders),
                                    };
                                    s.scratch.push(slot);
                                }
                                binders += b;
                            }
                            None => match i
                                .checked_sub(index.len() as u32)
                                .and_then(|di| def_index.get(di as usize))
                            {
                                Some(&(start, len, b)) => {
                                    for &slot in &def_pool[start as usize..(start + len) as usize] {
                                        let slot = match slot.kind {
                                            SlotKind::ObjectDefault(_) if alias_want == Some(0) => Slot::quiet(SlotKind::Static),
                                            SlotKind::ObjectDefault(oat) => return Ok(Err(RegionRefusal::ObjectDefault { at: oat })),
                                            _ => slot.shifted(binders),
                                        };
                                        s.scratch.push(slot);
                                    }
                                    binders += b;
                                }
                                None => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r))),
                            },
                        }
                        continue;
                    }
                    if own > 0 {
                        let Some(l) = laid else {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::AliasRegions, r)));
                        };
                        let Some(col) = l.slots.get(next..next + own as usize) else {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                        };
                        next += own as usize;
                        for &a in col {
                            let slot = match a {
                                AliasSlot::Static => Slot::written(SlotKind::Static),
                                AliasSlot::ObjectDefault => Slot::quiet(SlotKind::ObjectDefault(r)),
                                AliasSlot::Late { binder, var } => match binder_at.get(binder as usize) {
                                    Some(&b) => Slot::written(SlotKind::Late { binder: b, var }),
                                    None => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::AliasRegions, r))),
                                },
                                AliasSlot::Lifetime(i) => match given.get_mut(i as usize) {
                                    Some(Some(k)) => Slot::written(*k),
                                    Some(g @ None) => {
                                        let id = s.elided.len() as u32;
                                        s.elided.push(r);
                                        *g = Some(SlotKind::Elided(id));
                                        Slot::written(SlotKind::Elided(id))
                                    }
                                    None => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r))),
                                },
                            };
                            s.scratch.push(slot);
                        }
                    }
                    if is_binder(node) {
                        binder_at.push(binders);
                    }
                    binders += u32::from(is_binder(node));
                }
                if laid.is_some_and(|l| l.slots.len() != next) {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                }
                if s.scratch.len() as u32 != whole.slots || binders != whole.binders {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                }
                Ok(Ok(binders))
            }
            Kind::Trait | Kind::TraitAlias if self.allows_bare_trait_object_at(env, v, p, r)? => {
                self.bare_dyn_regions(tys, v, p, (made, args), found.key, found.kind == Kind::TraitAlias, (lifetimes, pool, index), whole, s, r)
            }
            _ => {
                if whole.slots != 0 {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::PathRegion, r)));
                }
                Ok(Ok(whole.binders))
            }
        }
    }

    /// Lays the region slots of omitted defaulted type argument `arg_idx` of item `item_key`
    /// (an ADT or a type alias) into `added`, in `each_slot_node` order over the substituted
    /// argument (whose counts are `sub`), with local binder indices `0..sub.binders` and
    /// `Slot::quiet` so defaulted lifetimes never perturb call-site elision counts.
    ///
    /// Fast path: when `raw_default` has zero own slots (all slots come purely from earlier type
    /// parameters `PNode::Param(p)`), substitutes from `(written_pool, def_pool)` directly.
    /// General path: when the default type itself writes lifetimes (e.g.
    /// `struct Records<'r, 'n, S = &'n Names<'n>>` or `struct Def<T = &'static u8>`), lowers
    /// `item_key`'s signature text once via `signature_text_of` (recording the read), lays the
    /// default AST's regions, and substitutes both `item_key`'s declared lifetimes (`item_lts`)
    /// and its earlier type arguments (`PNode::Param(p)`).
    #[expect(clippy::too_many_arguments, reason = "one defaulted argument: owner item, argument index, already-laid args, sink and output")]
    fn lay_default_arg<'s>(
        &self,
        tys: &mut PortableTys<'_>,
        item_key: LibKey,
        arg_idx: usize,
        raw_default: Option<PTy>,
        sub: Sub,
        owner: DefaultOwner,
        item_lts: &mut DefaultLifetimes<'_, 's>,
        (written_pool, written_index): (&[Slot], &[(u32, u32, u32)]),
        (def_pool, def_index): (&[Slot], &[(u32, u32, u32)]),
        s: &mut RegionSink<'s>,
        r: Range,
        added: &mut blaze_own::Vec<Slot, Alloc<'s>>,
    ) -> Result<Result<(), RegionRefusal>, Why> {
        let alloc = s.alloc;
        let param_base: u32 = match owner {
            DefaultOwner::Trait { .. } => 1,
            DefaultOwner::Adt { .. } | DefaultOwner::Alias { .. } => 0,
        };
        let Ok(cap) = usize::try_from(sub.slots) else {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
        };
        added.reserve(cap);
        // Fast path: default holds no lifetimes of its own, only earlier type parameters.
        if let Some(raw) = raw_default {
            let mut visits: Vec<(PTy, PNode, u32), Alloc<'s>> = Vec::new_in(alloc);
            soft!(each_slot_node(
                tys.view(),
                raw,
                &HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names },
                alloc,
                &mut |t, node, own| {
                    visits.push((t, node, own));
                    Ok(())
                },
            ));
            if visits.iter().all(|&(_, _, own)| own == 0) {
                let mut db = 0u32;
                for &(_, node, _) in &visits {
                    if let PNode::Param(p) = node
                        && let Some(off) = p.checked_sub(param_base)
                        && let Ok(pi) = usize::try_from(off)
                        && let Some((slice, b)) = arg_slots(pi, written_pool, written_index, def_pool, def_index)
                    {
                        for &slot in slice {
                            let slot = match (slot.kind, owner) {
                                (SlotKind::ObjectDefault(_), DefaultOwner::Adt { want: 0 } | DefaultOwner::Alias { want: Some(0) } | DefaultOwner::Trait { want: 0 }) => {
                                    Slot::quiet(SlotKind::Static)
                                }
                                (SlotKind::ObjectDefault(oat), _) => return Ok(Err(RegionRefusal::ObjectDefault { at: oat })),
                                _ => Slot::quiet(slot.shifted(db).kind),
                            };
                            added.push(slot);
                        }
                        db += b;
                        continue;
                    }
                    if is_binder(node) {
                        db += 1;
                    }
                }
                if u32::try_from(added.len()).ok() == Some(sub.slots) && db == sub.binders {
                    return Ok(Ok(()));
                }
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
            }
        }
        // General path: the default type writes lifetimes of its own (`'static`, item lifetime
        // parameters, or `for<..>` / `fn(..)` binders).
        if s.default_depth >= MOST_DEFAULT_DEPTH {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
        }
        let Some(item_text) = self.resolver.names.signature_text_of(item_key) else {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
        };
        let marks = match super::item_site::item_marks(self.resolver.names, item_key, item_text.text) {
            Ok(m) => m,
            Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r))),
        };
        let lowered = super::heads::with_lowered(self.resolver.names, item_key, item_text.text, |dv| -> Result<Result<(), RegionRefusal>, Why> {
            let mut lt_names: Vec<Name, Alloc<'s>> = Vec::new_in(alloc);
            let mut ps: Vec<(Name, u32), Alloc<'s>> = Vec::new_in(alloc);
            let mut param_i = param_base;
            let mut default_ty: Option<lf::TypeId> = None;
            for &row in dv.rows(dv.list(blaze_lower::head::GENERICS))? {
                match lf::GenericParam::decode(row)? {
                    lf::GenericParam::Lifetime { name, .. } => {
                        lt_names.push(Name(dv.name(name)?));
                    }
                    lf::GenericParam::Type { name, default, .. } => {
                        if param_i.checked_sub(param_base).and_then(|d| usize::try_from(d).ok()) == Some(arg_idx) {
                            default_ty = default;
                        }
                        ps.push((Name(dv.name(name)?), param_i));
                        param_i += 1;
                    }
                    lf::GenericParam::Const { name, .. } => {
                        ps.push((Name(dv.name(name)?), param_i));
                        param_i += 1;
                    }
                }
            }
            let Some(default_ty) = default_ty else {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
            };
            let cs = self.heads.const_params(item_key).unwrap_or(&[]);
            let def_env = Env {
                module: item_text.module,
                params: &ps,
                bounds: Bounds::Building(&[]),
                self_: SelfTy::None,
                self_assoc: &[],
                anon: &[],
                consts: cs,
                body: None,
            };
            let def_owner_text = if let Some(hdr) = item_text.impl_header {
                hdr
            } else if matches!(self.resolver.names.detail_of(item_key), Some(blaze_surface::ItemDetail::Trait)) {
                blaze_file_items::nested::trait_head(item_text.text)
            } else {
                item_text.text
            };
            let def_builder = Builder {
                resolver: crate::resolve::Paths { names: self.resolver.names },
                heads: self.heads,
                marks,
                owner: Some((item_key, def_owner_text)),
                owner_key: Some(super::keys::checker_of(item_key)),
                expanded: None,
                expanded_marks: None,
            };
            let mut def_sink = RegionSink::new_in(alloc);
            def_sink.default_depth = s.default_depth + 1;
            let raw_ty = match def_builder.ty_regions(tys, &def_env, dv, default_ty, &mut def_sink) {
                Ok(t) => t,
                Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r))),
            };
            let ([frag], None) = (def_sink.frags(), def_sink.refused()) else {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
            };
            let def_slots = def_sink.slots_of(*frag);
            let mut visits: Vec<(PTy, PNode, u32), Alloc<'s>> = Vec::new_in(alloc);
            soft!(each_slot_node(
                tys.view(),
                raw_ty,
                &HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names },
                alloc,
                &mut |t, node, own| {
                    visits.push((t, node, own));
                    Ok(())
                },
            ));
            let mut binder_at: Vec<u32, Alloc<'s>> = Vec::new_in(alloc);
            let mut next = 0usize;
            let mut db = 0u32;
            for &(_, node, own) in &visits {
                if let PNode::Param(p) = node {
                    let Some(off) = p.checked_sub(param_base) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
                    };
                    let Ok(pi) = usize::try_from(off) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
                    };
                    let Some((slice, b)) = arg_slots(pi, written_pool, written_index, def_pool, def_index) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
                    };
                    for &slot in slice {
                        let slot = match (slot.kind, owner) {
                            (SlotKind::ObjectDefault(_), DefaultOwner::Adt { want: 0 } | DefaultOwner::Alias { want: Some(0) } | DefaultOwner::Trait { want: 0 }) => {
                                Slot::quiet(SlotKind::Static)
                            }
                            (SlotKind::ObjectDefault(oat), _) => return Ok(Err(RegionRefusal::ObjectDefault { at: oat })),
                            _ => Slot::quiet(slot.shifted(db).kind),
                        };
                        added.push(slot);
                    }
                    db += b;
                    continue;
                }
                if own > 0 {
                    let Some(col) = def_slots.get(next..next + own as usize) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
                    };
                    next += own as usize;
                    for &dslot in col {
                        let mapped = match dslot.kind {
                            SlotKind::Static => Slot::quiet(SlotKind::Static),
                            SlotKind::Named(name, _) => {
                                let Some(lt_idx) = lt_names.iter().position(|&n| n == name) else {
                                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
                                };
                                let Some(use_kind) = item_lts.get(lt_idx, s, r) else {
                                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
                                };
                                Slot::quiet(use_kind)
                            }
                            SlotKind::Late { binder, var } => match binder_at.get(binder as usize) {
                                Some(&b) => Slot::quiet(SlotKind::Late { binder: b, var }),
                                None => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r))),
                            },
                            SlotKind::ObjectDefault(_) => match owner {
                                DefaultOwner::Adt { want: 0 } | DefaultOwner::Alias { want: Some(0) } | DefaultOwner::Trait { want: 0 } => {
                                    Slot::quiet(SlotKind::Static)
                                }
                                DefaultOwner::Adt { .. } | DefaultOwner::Trait { .. } => return Ok(Err(RegionRefusal::ObjectDefault { at: r })),
                                DefaultOwner::Alias { .. } => Slot::quiet(SlotKind::ObjectDefault(r)),
                            },
                            SlotKind::Elided(_) | SlotKind::Bound(_) | SlotKind::Given(_) => {
                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
                            }
                        };
                        added.push(mapped);
                    }
                }
                if is_binder(node) {
                    binder_at.push(db);
                    db += 1;
                }
            }
            if next != def_slots.len() || u32::try_from(added.len()).ok() != Some(sub.slots) || db != sub.binders {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
            }
            Ok(Ok(()))
        });
        match lowered {
            Ok(res) => res,
            Err(_) => Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r))),
        }
    }

    /// The slots of the projection `made` written `T::Name` (`T` a type parameter) or, in a
    /// trait, `Self::Name` (D148), in `each_slot_node`'s order: the alias's own (its trait's
    /// lifetimes, then its own, `regions::Arity::alias_lifetimes`), then its arguments'. Its
    /// trait's lifetimes are those the one bound naming the trait writes on `T` in this item
    /// (the Reference, "Paths", "Paths in types": `T::Name` is `<T as Trait>::Name` for the
    /// bound `T: Trait`; serde's `V: Visitor<'de>` gives `V::Value` the slot `'de`), or the
    /// trait's own for `Self::Name` (`Param(0..)`, the container group its members number
    /// first). Its own lifetimes and arguments are those written on `Name`. A trait that
    /// declares no lifetime needs no bound at all (none gives it a slot). A projection whose
    /// trait has lifetimes, read through a supertrait, a container's bound, a `for<..>` bound
    /// or two bounds, or whose other arguments hold a lifetime, is refused by name: not laid
    /// yet, never guessed.
    #[expect(clippy::too_many_arguments, reason = "one made projection: its written form, its arguments and the sink")]
    fn projection_slots(
        &self,
        tys: &mut PortableTys<'_>,
        env: &Env<'_>,
        v: &View<'_>,
        made: PTy,
        through: Through<'_>,
        (own_written, pool, index): (&[SlotKind], &[Slot], &[(u32, u32, u32)]),
        s: &mut RegionSink<'_>,
        r: Range,
    ) -> Result<Laid, Why> {
        let alloc = s.alloc;
        let Ok(PNode::Alias { trait_, name, args }) = tys.view().node(made) else {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
        };
        let Some(trait_count) = declared_lifetimes(self.heads, self.resolver.names, trait_, Declared::Trait) else {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DeclaredCount, r)));
        };
        let Some(own_count) = declared_lifetimes(self.heads, self.resolver.names, trait_, Declared::Assoc(name)) else {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DeclaredCount, r)));
        };
        if own_written.len() as u32 != own_count {
            return Ok(Err(RegionRefusal::LifetimeArgs { at: r, want: own_count, got: own_written.len() as u32 }));
        }
        let mut slots: Vec<Slot, Alloc<'_>> = Vec::new_in(alloc);
        // A trait that declares no lifetime gives the alias no slot of its own trait's: which
        // bound, supertrait or container names it decides nothing of the regions (its type
        // arguments are the alias's arguments, read from the made type below and refused if
        // they hold a lifetime). So `T::Token<'_>` under the container's `T: SolType`
        // (alloy-sol-types' `trait SolTypeValue<T: SolType> { fn stv_to_tokens(&self) ->
        // T::Token<'_>; }`) and `Self::Name` of a supertrait without lifetimes are laid
        // exactly; only a trait WITH lifetimes needs the one bound that writes them.
        let mut param_bound_col: Option<(usize, usize)> = None;
        match through {
            Through::SelfTrait(tr) => {
                if self.heads.checker(tr) != trait_ && trait_count != 0 {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::SelfSupertrait, r)));
                }
                for i in 0..trait_count {
                    slots.push(Slot::quiet(SlotKind::Given(Region::Param(i))));
                }
            }
            // The impl's own trait: `Self::Name` is `<SelfTy as Tr<'x..>>::Name` with the
            // trait's lifetimes as the impl's header writes them (the Reference, "Paths",
            // "`Self`", and "Implementations": the impl implements `Tr<'x..>` for `SelfTy`;
            // serde's derived `impl<'de> Visitor<'de> for __Visitor<'de> { fn visit_seq<A>(..)
            // -> Result<Self::Value, A::Error> }` is `<__Visitor<'de> as Visitor<'de>>::Value`).
            // They are the first `trait_count` regions of the header's trait column
            // (heads_regions.rs `header_trait`: the trait's lifetimes, each elided one a fresh
            // parameter of the impl, then `Self`'s, then the arguments'), numbered as `Self`'s
            // (the members' container group); `Self`'s and the arguments' slots are put below.
            Through::SelfImpl(tr) => {
                if self.heads.checker(tr) != trait_ && trait_count != 0 {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::SelfSupertrait, r)));
                }
                if trait_count != 0 {
                    let Some(col) = s.impl_trait.as_ref() else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::ImplTraitLifetimes, r)));
                    };
                    let Some(lead) = col.get(..trait_count as usize) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                    };
                    slots.extend(lead.iter().map(|&g| Slot::quiet(SlotKind::Given(g))));
                }
            }
            Through::Param(t) => {
                let container_match = env
                    .params
                    .iter()
                    .find(|&&(pn, _)| pn == Name::of(t))
                    .map(|&(_, pidx)| {
                        let mut matched: Option<(usize, usize)> = None;
                        let mut conflict = false;
                        for &(bp, btr, bstart, blen) in &s.param_bounds {
                            if bp != pidx || btr != trait_ {
                                continue;
                            }
                            let bs = bstart as usize;
                            let be = bs.saturating_add(blen as usize);
                            if let Some((ms, me)) = matched {
                                if s.param_bound_regions.get(ms..me) != s.param_bound_regions.get(bs..be) {
                                    conflict = true;
                                }
                            } else {
                                matched = Some((bs, be));
                            }
                        }
                        (matched, conflict)
                    });
                if let Some((Some(m), false)) = container_match {
                    param_bound_col = Some(m);
                }
                if trait_count != 0 {
                    let mut found = 0u32;
                    for &n in v.rows(v.list(blaze_lower::head::GENERICS))? {
                        if let lf::GenericParam::Type { name: pn, bounds, .. } = lf::GenericParam::decode(n)?
                            && v.name_text(pn)? == t
                            && let Err(why) = self.bound_lifetimes(env, v, bounds, trait_, &mut found, &mut slots, r)?
                        {
                            return Ok(Err(why));
                        }
                    }
                    for &n in v.rows(v.list(blaze_lower::head::WHERE))? {
                        let lf::WherePred::Type { binder, ty, bounds } = lf::WherePred::decode(n)? else { continue };
                        let lf::TypeRef::Path(p) = v.ty(ty)? else { continue };
                        let (pabs, ptexts, _, _) = path_texts(v, p)?;
                        if pabs || !matches!(ptexts.as_slice(), [only] if *only == t) {
                            continue;
                        }
                        if !v.rows(binder)?.is_empty() {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::ProjectionHigherRanked, r)));
                        }
                        if let Err(why) = self.bound_lifetimes(env, v, bounds, trait_, &mut found, &mut slots, r)? {
                            return Ok(Err(why));
                        }
                    }
                    if found == 0 {
                        match container_match {
                            Some((_, true)) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::ProjectionTwoBounds, r))),
                            Some((Some((bs, be)), false)) => {
                                let Some(col) = s.param_bound_regions.get(bs..be) else {
                                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                                };
                                let Some(lead) = col.get(..trait_count as usize) else {
                                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                                };
                                if lead.iter().any(|g| matches!(g, Region::Late { .. })) {
                                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::ProjectionHigherRanked, r)));
                                }
                                slots.extend(lead.iter().map(|&g| Slot::quiet(SlotKind::Given(g))));
                                param_bound_col = Some((bs, be));
                            }
                            _ => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::ProjectionBoundElsewhere, r))),
                        }
                    }
                    if slots.len() as u32 != trait_count {
                        return Ok(Err(RegionRefusal::LifetimeArgs { at: r, want: trait_count, got: slots.len() as u32 }));
                    }
                }
            }
        }
        slots.extend(own_written.iter().map(|&k| Slot::written(k)));
        // The alias's arguments: `Self` and the trait's (from the impl's trait column or
        // container's clause column when available; otherwise they must hold no lifetime), then
        // those written on `Name` (`index`, from the stack).
        let list = soft!(tys.view().list(args).map_err(RegionError::Table));
        let Some(lead) = list.len().checked_sub(index.len()) else {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
        };
        let arity = HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names };
        let mut binders = 0u32;
        let impl_trait_own = matches!(through, Through::SelfImpl(tr) if self.heads.checker(tr) == trait_);
        let mut col_off = trait_count as usize;
        for (i, &a) in list[..lead].iter().enumerate() {
            let sub = soft!(subtree(tys.view(), a, &arity, alloc));
            // In an impl the first is `Self`, the impl's self type: its slots are the regions
            // the container hands its members (as a bare `Self` is laid in `path_regions`).
            if i == 0 && matches!(through, Through::SelfImpl(_)) {
                let Some(given) = s.self_slots.as_ref() else {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::SelfUnknown, r)));
                };
                if given.len() as u32 != sub.slots {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                }
                slots.extend(given.iter().map(|&g| Slot::quiet(SlotKind::Given(g))));
                col_off += sub.slots as usize;
                binders += sub.binders;
                continue;
            }
            if sub.slots != 0 {
                let from_col = if impl_trait_own {
                    s.impl_trait.as_deref().and_then(|col| col.get(col_off..col_off + sub.slots as usize))
                } else if let Some((bs, be)) = param_bound_col {
                    s.param_bound_regions.get(bs..be).and_then(|col| col.get(col_off..col_off + sub.slots as usize))
                } else {
                    None
                };
                if let Some(given) = from_col
                    && !given.iter().any(|g| matches!(g, Region::Late { .. }))
                {
                    slots.extend(given.iter().map(|&g| Slot::quiet(SlotKind::Given(g))));
                    col_off += sub.slots as usize;
                    binders += sub.binders;
                    continue;
                }
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::ProjectionBoundArgs, r)));
            }
            binders += sub.binders;
        }
        for &(start, len, b) in index {
            for &slot in &pool[start as usize..(start + len) as usize] {
                if let SlotKind::ObjectDefault(oat) = slot.kind {
                    return Ok(Err(RegionRefusal::ObjectDefault { at: oat }));
                }
                slots.push(slot.shifted(binders));
            }
            binders += b;
        }
        let whole = soft!(subtree(tys.view(), made, &arity, alloc));
        if slots.len() as u32 != whole.slots || binders != whole.binders {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
        }
        s.scratch.clear();
        s.scratch.extend_from_slice(&slots);
        Ok(Ok(binders))
    }

    /// The slots of the qualified projection `made` (`<T as Tr<'a>>::Name`, the alias node),
    /// in `each_slot_node`'s order: its trait's lifetimes (`trait_written`, those written on
    /// the trait's last segment: the trait reference `Tr<'a>` the Reference's "Qualified
    /// paths" names; none written, one elided lifetime per declared one, as for an ADT's path:
    /// "Lifetime elision", "elided in paths"), its own written on `Name` (`own_written`), then
    /// each argument's from the stack (`pool`, `index`: the self type first, as the path's walk
    /// pushed them). Checked against the made type's count (`whole`), never assumed.
    fn qualified_slots(
        &self,
        tys: &PortableTys<'_>,
        made: PTy,
        (trait_written, own_written, pool, index): (&[SlotKind], &[SlotKind], &[Slot], &[(u32, u32, u32)]),
        whole: Sub,
        s: &mut RegionSink<'_>,
        r: Range,
    ) -> Laid {
        let Ok(PNode::Alias { trait_, name, .. }) = tys.view().node(made) else {
            return Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r));
        };
        let Some(trait_count) = declared_lifetimes(self.heads, self.resolver.names, trait_, Declared::Trait) else {
            return Err(RegionRefusal::not_laid(NotLaidWhy::DeclaredCount, r));
        };
        let Some(own_count) = declared_lifetimes(self.heads, self.resolver.names, trait_, Declared::Assoc(name)) else {
            return Err(RegionRefusal::not_laid(NotLaidWhy::DeclaredCount, r));
        };
        if own_written.len() as u32 != own_count {
            return Err(RegionRefusal::LifetimeArgs { at: r, want: own_count, got: own_written.len() as u32 });
        }
        let mut slots: Vec<Slot, Alloc<'_>> = Vec::new_in(s.alloc);
        if trait_written.is_empty() {
            for _ in 0..trait_count {
                slots.push(Slot::written(s.new_elided(r)));
            }
        } else if trait_written.len() as u32 == trait_count {
            slots.extend(trait_written.iter().map(|&k| Slot::written(k)));
        } else {
            return Err(RegionRefusal::LifetimeArgs { at: r, want: trait_count, got: trait_written.len() as u32 });
        }
        slots.extend(own_written.iter().map(|&k| Slot::written(k)));
        let mut binders = 0u32;
        for &(start, len, b) in index {
            for &slot in &pool[start as usize..(start + len) as usize] {
                if let SlotKind::ObjectDefault(oat) = slot.kind {
                    return Err(RegionRefusal::ObjectDefault { at: oat });
                }
                slots.push(slot.shifted(binders));
            }
            binders += b;
        }
        if slots.len() as u32 != whole.slots || binders != whole.binders {
            return Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r));
        }
        s.scratch.clear();
        s.scratch.extend_from_slice(&slots);
        Ok(binders)
    }

    /// The lifetimes written on each bound of `bounds` (a type parameter's) that names the
    /// trait `trait_`, pushed on `out` in order; `found` counts those bounds, a second one
    /// refused (which of two bounds a projection reads is not decided here). For
    /// [`Builder::projection_slots`].
    #[expect(clippy::too_many_arguments, reason = "one parameter's bounds, the trait sought and what is found")]
    fn bound_lifetimes(&self, env: &Env<'_>, v: &View<'_>, bounds: lf::List, trait_: ItemKey, found: &mut u32, out: &mut Vec<Slot, Alloc<'_>>, r: Range) -> Result<Result<(), RegionRefusal>, Why> {
        for &row in v.rows(bounds)? {
            let lf::Bound::Trait { ty, binder, mods } = lf::Bound::decode(row)? else { continue };
            if mods.maybe || mods.negative {
                continue;
            }
            let lf::TypeRef::Path(p) = v.ty(ty)? else { continue };
            let (abs, texts, segs, pr) = path_texts_of(v, p, true)?;
            // The erased walk resolved this bound (preds.rs `param_bound`): a refusal here is
            // the regions walk's own.
            let target = match self.bound_target(env, v, p, abs, &texts, pr) {
                Ok(target) => target,
                Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::Resolve, r))),
            };
            let Bounded::Trait(key) = target else { continue };
            if self.heads.checker(key) != trait_ {
                continue;
            }
            *found += 1;
            if *found > 1 {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::ProjectionTwoBounds, r)));
            }
            if !v.rows(binder)?.is_empty() {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::ProjectionHigherRanked, r)));
            }
            let Some(last) = segs.last() else { continue };
            if !last.has_args {
                continue;
            }
            for &a in v.rows(last.args)? {
                let lf::GenericArg::Lifetime(nm) = lf::GenericArg::decode(a)? else { continue };
                let text = v.name_text(nm)?;
                let lat = place(v, v.name_range(nm))?;
                let kind = match text.strip_prefix('\'').unwrap_or(text) {
                    "_" => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::ProjectionElidedBound, lat))),
                    "static" => SlotKind::Static,
                    _ => SlotKind::Named(Name(v.name(nm)?), lat),
                };
                // Not written in the projection's type: no input lifetime for elision.
                out.push(Slot::quiet(kind));
            }
        }
        Ok(Ok(()))
    }

    /// A trait object's slots: its own (its written lifetime bound, else its default, decided
    /// by its context), then each existential trait/projection's own lifetime arguments and its
    /// type arguments' where each lands in its made type (found by making it again with shadow
    /// arguments, `SHADOW_BASE`). Its bounds' `for<..>` names and its `Fn` sugar's elided inputs
    /// are its own variables (binder 0); an elided lifetime of the sugar's output takes the one
    /// its inputs use, or is E0106.
    #[expect(clippy::too_many_arguments, reason = "one made trait object: its bounds, its children and the sink")]
    fn dyn_regions(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, bounds: lf::List, made: PTy, kids: &[PTy], s: &mut RegionSink<'_>, r: Range) -> Result<Laid, Why> {
        let n = kids.len();
        let alloc = s.alloc;
        let mut at = 0usize;
        let mut vars = 0u32;
        let mut object: Option<SlotKind> = None;
        let mut written = 0u32;
        let mut resolved_traits: blaze_own::Vec<(LibKey, usize, usize), Alloc<'_>> = blaze_own::Vec::new_in(alloc);
        let mut resolved_args: blaze_own::Vec<Option<SlotKind>, Alloc<'_>> = blaze_own::Vec::new_in(alloc);
        let mut alias_direct_bounds: blaze_own::Vec<SlotKind, Alloc<'_>> = blaze_own::Vec::new_in(alloc);
        let mut trait_lts: blaze_own::Vec<(ItemKey, u32, u32), Alloc<'_>> = blaze_own::Vec::new_in(alloc);
        let mut lt_pool: blaze_own::Vec<Slot, Alloc<'_>> = blaze_own::Vec::new_in(alloc);
        for &b in v.rows(bounds)? {
            let bd = lf::Bound::decode(b)?;
            if let lf::Bound::Lifetime(nm) = bd {
                written += 1;
                object = Some(lifetime_kind(v, Some(nm), r, s)?);
                continue;
            }
            let lf::Bound::Trait { binder, .. } = bd else { continue };
            let Some((p, seg)) = super::dyn_ty::dyn_bound(v, bd, r)? else { continue };
            let mut names: Vec<Name, Alloc<'_>> = Vec::new_in(alloc);
            for &row in v.rows(binder)? {
                if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(row)? {
                    names.push(Name(v.name(name)?));
                }
            }
            let first = vars;
            // Lifetime arguments written on this trait bound (`dyn Store<'m>`, `dyn Store<'_>`,
            // `dyn for<'a> Tr<'a>`), or elided fresh lifetimes when the trait declares lifetimes
            // and none are written.
            let lt_start = lt_pool.len() as u32;
            let mut written_lts = 0u32;
            if !seg.parenthesized {
                for &row in v.rows(seg.args)? {
                    if let lf::GenericArg::Lifetime(nm) = lf::GenericArg::decode(row)? {
                        let mut kind = lifetime_kind(v, Some(nm), r, s)?;
                        if let SlotKind::Named(name_hash, _) = kind
                            && let Some(pos) = names.iter().position(|&x| x == name_hash)
                        {
                            kind = SlotKind::Bound(first + pos as u32);
                        }
                        lt_pool.push(Slot::written(kind));
                        written_lts += 1;
                    }
                }
            }
            let (abs, texts, _, pr) = path_texts_of(v, p, true)?;
            match self.bound_target(env, v, p, abs, &texts, pr) {
                Ok(Bounded::Trait(key)) => {
                    let checker_key = self.heads.checker(key);
                    let mut lt_len = 0u32;
                    if let Some(want) = declared_lifetimes(self.heads, self.resolver.names, checker_key, Declared::Trait) {
                        if written_lts == 0 {
                            for _ in 0..want {
                                lt_pool.push(Slot::written(s.new_elided(pr)));
                            }
                            if want > 0 {
                                trait_lts.push((checker_key, lt_start, want));
                                lt_len = want;
                            }
                        } else if written_lts == want {
                            trait_lts.push((checker_key, lt_start, written_lts));
                            lt_len = written_lts;
                        } else {
                            return Ok(Err(RegionRefusal::LifetimeArgs { at: pr, want, got: written_lts }));
                        }
                        if let Err(why) = self.populate_supertrait_lts(key, lt_start, want, &mut trait_lts, &mut lt_pool, s, pr)? {
                            return Ok(Err(why));
                        }
                    } else if written_lts > 0 {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DeclaredCount, pr)));
                    }
                    let r_start = resolved_args.len();
                    if let (Ok(s0), Ok(ln)) = (usize::try_from(lt_start), usize::try_from(lt_len))
                        && let Some(slice) = s0.checked_add(ln).and_then(|e0| lt_pool.get(s0..e0))
                    {
                        for slot in slice {
                            resolved_args.push(Some(slot.kind));
                        }
                    }
                    resolved_traits.push((key, r_start, resolved_args.len() - r_start));
                }
                Ok(Bounded::Alias(alias)) => {
                    let mut alias_args: blaze_own::Vec<Slot, Alloc<'_>> = blaze_own::Vec::new_in(alloc);
                    if let (Ok(s0), Ok(ln)) = (usize::try_from(lt_start), usize::try_from(written_lts))
                        && let Some(slice) = s0.checked_add(ln).and_then(|e0| lt_pool.get(s0..e0))
                    {
                        alias_args.extend_from_slice(slice);
                    }
                    if let Ok(s0) = usize::try_from(lt_start) {
                        lt_pool.truncate(s0);
                    }
                    if let Some(want) = declared_lifetimes(self.heads, self.resolver.names, self.heads.checker(alias), Declared::Trait) {
                        if written_lts == 0 {
                            for _ in 0..want {
                                alias_args.push(Slot::written(s.new_elided(pr)));
                            }
                        } else if written_lts != want {
                            return Ok(Err(RegionRefusal::LifetimeArgs { at: pr, want, got: written_lts }));
                        }
                    }
                    if let Some(ah) = self.heads.trait_alias(alias) {
                        for &ao in ah.super_lifetimes.alias_outlives.iter() {
                            let mapped = match ao {
                                SuperLifetime::Static => Some(SlotKind::Static),
                                SuperLifetime::Param(pi) => usize::try_from(pi)
                                    .ok()
                                    .and_then(|i| alias_args.get(i))
                                    .map(|slot| slot.kind),
                                SuperLifetime::Other => None,
                            };
                            if let Some(kind) = mapped {
                                alias_direct_bounds.push(kind);
                            }
                        }
                        for &(k, e_start, e_end) in ah.super_lifetimes.edges.iter() {
                            let r_start = resolved_args.len();
                            let Ok(edge_lt_start) = u32::try_from(lt_pool.len()) else {
                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, pr)));
                            };
                            let mut all_laid = true;
                            let mut laid_count = 0u32;
                            if let (Ok(es), Ok(ee)) = (usize::try_from(e_start), usize::try_from(e_end))
                                && let Some(edge_args) = ah.super_lifetimes.args.get(es..ee)
                            {
                                for &ea in edge_args {
                                    let mapped = match ea {
                                        SuperLifetime::Static => Some(Slot::quiet(SlotKind::Static)),
                                        SuperLifetime::Param(pi) => usize::try_from(pi).ok().and_then(|i| alias_args.get(i).copied()),
                                        SuperLifetime::Other => None,
                                    };
                                    resolved_args.push(mapped.map(|slot| slot.kind));
                                    if let Some(slot) = mapped {
                                        lt_pool.push(slot);
                                        laid_count = laid_count.saturating_add(1);
                                    } else {
                                        all_laid = false;
                                    }
                                }
                            }
                            let ck = self.heads.checker(k);
                            let want = declared_lifetimes(self.heads, self.resolver.names, ck, Declared::Trait);
                            if all_laid && want == Some(laid_count) && laid_count > 0 {
                                trait_lts.push((ck, edge_lt_start, laid_count));
                            } else if let Ok(st) = usize::try_from(edge_lt_start) {
                                lt_pool.truncate(st);
                            }
                            resolved_traits.push((k, r_start, resolved_args.len() - r_start));
                        }
                    }
                }
                Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                Err(_) => {
                    lt_pool.truncate(lt_start as usize);
                }
            }
            // How many children this bound gave the walk (`dyn_arg_types`' count).
            let mut inputs = 0usize;
            for &row in v.rows(seg.args)? {
                match lf::GenericArg::decode(row)? {
                    lf::GenericArg::Type(_) => inputs += 1,
                    lf::GenericArg::AssocType { args, .. } if !seg.parenthesized && args.len == 0 => inputs += 1,
                    _ => {}
                }
            }
            let output = seg.parenthesized && seg.ty.is_some();
            let count = inputs + usize::from(output);
            let mut next = vars + names.len() as u32;
            let mut ids: Vec<u32, Alloc<'_>> = Vec::new_in(alloc);
            let mut input_slots: Vec<Slot, Alloc<'_>> = Vec::new_in(alloc);
            for k in at..at + count {
                let f = soft!(s.kid(n, k).ok_or(RegionRefusal::not_laid(NotLaidWhy::Stack, r)));
                let is_input = k < at + inputs;
                for i in f.start..f.start + f.len {
                    let slot = &mut s.slots[i as usize];
                    if let SlotKind::Named(nm, _) = slot.kind
                        && let Some(p) = names.iter().position(|&x| x == nm)
                    {
                        slot.kind = SlotKind::Bound(first + p as u32);
                    }
                    if seg.parenthesized
                        && is_input
                        && let SlotKind::Elided(id) = slot.kind
                    {
                        let var = match ids.iter().position(|&x| x == id) {
                            Some(p) => first + names.len() as u32 + p as u32,
                            None => {
                                ids.push(id);
                                next += 1;
                                next - 1
                            }
                        };
                        slot.kind = SlotKind::Bound(var);
                    }
                    if is_input {
                        input_slots.push(*slot);
                    }
                }
                if output && !is_input {
                    let one = used_lifetimes(&input_slots, object_local);
                    for i in f.start..f.start + f.len {
                        if let SlotKind::Elided(_) = s.slots[i as usize].kind {
                            match one {
                                Used::One(kind) => s.slots[i as usize].kind = kind,
                                // A regions refusal by name, the head made (heads.rs
                                // `missing_lifetime`: E0106 is hard for a fn's return type only).
                                Used::None | Used::Many => {
                                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::FnPtrElision, r)));
                                }
                            }
                        }
                    }
                }
            }
            vars = next;
            at += count;
        }
        if at != n {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
        }
        if written > 1 {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynLifetimes, r)));
        }
        let own = if let Some(k) = object {
            Slot::written(k)
        } else {
            match self.resolve_rfc1156_default_lifetime(&resolved_traits, &resolved_args, &alias_direct_bounds, alloc) {
                TraitDefaultBound::Unique(kind) => Slot::quiet(kind),
                TraitDefaultBound::None => Slot::quiet(SlotKind::ObjectDefault(r)),
                TraitDefaultBound::Ambiguous => return Ok(Err(RegionRefusal::ObjectDefault { at: r })),
            }
        };
        let arity = HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names };
        let whole = soft!(subtree(tys.view(), made, &arity, alloc));
        s.scratch.clear();
        let mut kid_slots = 0u32;
        for k in 0..n {
            kid_slots += s.kid(n, k).map_or(0, |f| f.len);
        }
        let find_super_proj = |sup_tr: ItemKey, name: Name| -> Option<(&[Region], u32, u32)> {
            for &(rk, _, _) in resolved_traits.iter() {
                let sub_ck = self.heads.checker(rk);
                if let Some(regs) = self.heads.trait_super_proj_regions(sub_ck, sup_tr, name) {
                    let (lt_start, lt_len) = trait_lts.iter().find(|&&(k, _, _)| k == sub_ck).map_or((0, 0), |&(_, st, ln)| (st, ln));
                    return Some((regs, lt_start, lt_len));
                }
            }
            None
        };
        let push_super_proj_own = |regs: &[Region], cur: &mut usize, lt_start: u32, lt_len: u32, rown: u32, binders: u32, out: &mut blaze_own::Vec<Slot, Alloc<'_>>| -> bool {
            let Ok(rn) = usize::try_from(rown) else { return false };
            let Some(end) = cur.checked_add(rn) else { return false };
            let Some(slice) = regs.get(*cur..end) else { return false };
            *cur = end;
            for &reg in slice {
                let kind = match reg {
                    Region::Param(i) if i < lt_len => match usize::try_from(lt_start + i).ok().and_then(|idx| lt_pool.get(idx)) {
                        Some(&sl) => sl.kind,
                        None => return false,
                    },
                    Region::Static => SlotKind::Static,
                    Region::Late { binder, var } => SlotKind::Late { binder: binder.saturating_add(binders), var },
                    _ => return false,
                };
                out.push(Slot::quiet(kind));
            }
            true
        };
        let binders = if kid_slots == 0 && whole.slots == 1 {
            s.scratch.push(own);
            whole.binders
        } else if kid_slots == 0 {
            let mut real: Vec<(PTy, PNode, u32), Alloc<'_>> = Vec::new_in(alloc);
            soft!(each_slot_node(tys.view(), made, &arity, alloc, &mut |t, node, own| {
                real.push((t, node, own));
                Ok(())
            }));
            let mut binders = 0u32;
            let mut active_sp: Option<(&[Region], usize, u32, u32, usize)> = None;
            for (ri, &(rt, rn, rown)) in real.iter().enumerate() {
                if active_sp.is_some_and(|(_, _, _, _, end)| ri >= end) {
                    active_sp = None;
                }
                if active_sp.is_none()
                    && let PNode::ExProj { trait_: sup_tr, name, .. } = rn
                    && !trait_lts.iter().any(|&(k, _, _)| k == sup_tr)
                    && let Some((regs, lt_start, lt_len)) = find_super_proj(sup_tr, name)
                {
                    let sub_nodes = usize::try_from(soft!(subtree(tys.view(), rt, &arity, alloc)).nodes).unwrap_or(usize::MAX);
                    active_sp = Some((regs, 0, lt_start, lt_len, ri.saturating_add(sub_nodes)));
                }
                if rown > 0 {
                    if ri == 0 && rown == 1 {
                        s.scratch.push(own);
                    } else if let Some((regs, ref mut cur, lt_start, lt_len, _)) = active_sp {
                        if !push_super_proj_own(regs, cur, lt_start, lt_len, rown, binders, &mut s.scratch) {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                        }
                    } else if !push_dyn_node_own_slots(rn, rown, &trait_lts, &lt_pool, &mut s.scratch) {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                    }
                }
                binders += u32::from(is_binder(rn));
            }
            if s.scratch.len() as u32 != whole.slots || binders != whole.binders {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
            }
            binders
        } else {
            // Made again with shadow arguments: where each `Param(SHADOW_BASE + k)` is, child
            // `k`'s slots go; every other node must agree with the made type's and have no
            // slot of its own but the root's and each bound trait's own lifetimes.
            let mut shadow_kids: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
            for k in 0..n {
                shadow_kids.push(intern(tys, PNode::Param(SHADOW_BASE + k as u32))?);
            }
            let Ok(shadow) = self.dyn_ty(tys, env, v, bounds, &shadow_kids, r) else {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
            };
            let arity = HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names };
            let mut real: Vec<(PTy, PNode, u32), Alloc<'_>> = Vec::new_in(alloc);
            soft!(each_slot_node(tys.view(), made, &arity, alloc, &mut |t, node, own| {
                real.push((t, node, own));
                Ok(())
            }));
            let mut seen: Vec<(PTy, PNode, u32), Alloc<'_>> = Vec::new_in(alloc);
            soft!(each_slot_node(tys.view(), shadow, &arity, alloc, &mut |t, node, own| {
                seen.push((t, node, own));
                Ok(())
            }));
            let out_of_step = RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r);
            let mut ri = 0usize;
            let mut binders = 0u32;
            let mut active_sp: Option<(&[Region], usize, u32, u32, usize)> = None;
            for (si, &(st, sn, sown)) in seen.iter().enumerate() {
                if active_sp.is_some_and(|(_, _, _, _, end)| si >= end) {
                    active_sp = None;
                }
                if active_sp.is_none()
                    && let PNode::ExProj { trait_: sup_tr, name, .. } = sn
                    && !trait_lts.iter().any(|&(k, _, _)| k == sup_tr)
                    && let Some((regs, lt_start, lt_len)) = find_super_proj(sup_tr, name)
                {
                    let sub_nodes = usize::try_from(soft!(subtree(tys.view(), st, &arity, alloc)).nodes).unwrap_or(usize::MAX);
                    active_sp = Some((regs, 0, lt_start, lt_len, si.saturating_add(sub_nodes)));
                }
                if let PNode::Param(p) = sn
                    && p >= SHADOW_BASE
                    && ((p - SHADOW_BASE) as usize) < n
                {
                    let k = (p - SHADOW_BASE) as usize;
                    let Some(&(rt, _, _)) = real.get(ri) else { return Ok(Err(out_of_step)) };
                    if rt != kids[k] {
                        return Ok(Err(out_of_step));
                    }
                    let f = soft!(s.kid(n, k).ok_or(out_of_step));
                    for i in f.start..f.start + f.len {
                        let slot = s.slots[i as usize];
                        s.scratch.push(match slot.kind {
                            SlotKind::ObjectDefault(_) => Slot::quiet(SlotKind::Static),
                            _ => slot.shifted(binders),
                        });
                    }
                    binders += f.binders;
                    ri += soft!(subtree(tys.view(), kids[k], &arity, alloc)).nodes as usize;
                    continue;
                }
                let Some(&(_, rn, rown)) = real.get(ri) else { return Ok(Err(out_of_step)) };
                if core::mem::discriminant(&rn) != core::mem::discriminant(&sn) || rown != sown {
                    return Ok(Err(out_of_step));
                }
                if rown > 0 {
                    if si == 0 && rown == 1 {
                        s.scratch.push(own);
                    } else if let Some((regs, ref mut cur, lt_start, lt_len, _)) = active_sp {
                        if !push_super_proj_own(regs, cur, lt_start, lt_len, rown, binders, &mut s.scratch) {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                        }
                    } else if !push_dyn_node_own_slots(rn, rown, &trait_lts, &lt_pool, &mut s.scratch) {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                    }
                }
                binders += u32::from(is_binder(rn));
                ri += 1;
            }
            if ri != real.len() || u32::try_from(s.scratch.len()) != Ok(whole.slots) || binders != whole.binders {
                return Ok(Err(out_of_step));
            }
            binders
        };
        for slot in s.scratch.iter_mut() {
            if let SlotKind::Bound(var) = slot.kind {
                slot.kind = SlotKind::Late { binder: 0, var };
            }
        }
        Ok(Ok(binders))
    }

    /// RFC 1156 ("Default trait object lifetimes") two-phase resolver over pre-filed `Heads`
    /// facts (`trait_self_static`, `trait_self_outlives_params`, `trait_super_lifetimes`),
    /// shared by both `dyn_regions` and `bare_dyn_regions`.
    /// - Any derived `'static` bound -> `TraitDefaultBound::Unique(SlotKind::Static)` (absorbing).
    /// - 0 bounds -> `TraitDefaultBound::None` (context decides).
    /// - 1 unique non-`'static` bound -> `TraitDefaultBound::Unique(kind)`.
    /// - >1 distinct non-`'static` bounds -> `TraitDefaultBound::Ambiguous` (E0228).
    fn resolve_rfc1156_default_lifetime<'s>(
        &self,
        resolved_traits: &[(LibKey, usize, usize)],
        resolved_args: &[Option<SlotKind>],
        alias_direct_bounds: &[SlotKind],
        alloc: Alloc<'s>,
    ) -> TraitDefaultBound {
        #[derive(Clone, Copy)]
        enum BoundAcc {
            None,
            One(SlotKind),
            Static,
            Ambiguous,
        }
        impl BoundAcc {
            fn push(&mut self, kind: SlotKind) -> bool {
                if matches!(kind, SlotKind::Bound(_)) {
                    return false;
                }
                if matches!(kind, SlotKind::Static) {
                    *self = BoundAcc::Static;
                    return true;
                }
                match *self {
                    BoundAcc::Static => true,
                    BoundAcc::None => {
                        *self = BoundAcc::One(kind);
                        false
                    }
                    BoundAcc::One(prev) if same_region(prev, kind) => false,
                    BoundAcc::One(_) | BoundAcc::Ambiguous => {
                        *self = BoundAcc::Ambiguous;
                        false
                    }
                }
            }
        }
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        enum SlotKey {
            Static,
            Named(Name),
            Elided(u32),
            Late { binder: u32, var: u32 },
            Given(Region),
            ObjectDefault(u32, u32),
        }
        let slot_key = |kind: SlotKind| -> Option<SlotKey> {
            match kind {
                SlotKind::Static => Some(SlotKey::Static),
                SlotKind::Named(name, _) => Some(SlotKey::Named(name)),
                SlotKind::Elided(id) => Some(SlotKey::Elided(id)),
                SlotKind::Late { binder, var } => Some(SlotKey::Late { binder, var }),
                SlotKind::Bound(_) => None,
                SlotKind::Given(reg) => Some(SlotKey::Given(reg)),
                SlotKind::ObjectDefault(at) => Some(SlotKey::ObjectDefault(at.start, at.end)),
            }
        };
        let mut acc = BoundAcc::None;
        for &kind in alias_direct_bounds.iter() {
            if acc.push(kind) {
                break;
            }
        }
        let mut visited_traits: blaze_hash::HashSet<LibKey, Alloc<'_>> = blaze_hash::HashSet::new_in(alloc);
        let mut trait_work: blaze_own::Vec<LibKey, Alloc<'_>> = blaze_own::Vec::new_in(alloc);
        let mut visited_params: blaze_hash::HashSet<(LibKey, u32, SlotKey), Alloc<'_>> = blaze_hash::HashSet::new_in(alloc);
        let mut param_work: blaze_own::Vec<(LibKey, u32, SlotKind), Alloc<'_>> = blaze_own::Vec::new_in(alloc);
        let enqueue_param = |k: LibKey,
                             idx: u32,
                             kind: SlotKind,
                             visited_params: &mut blaze_hash::HashSet<(LibKey, u32, SlotKey), Alloc<'_>>,
                             param_work: &mut blaze_own::Vec<(LibKey, u32, SlotKind), Alloc<'_>>| {
            if let Some(sk) = slot_key(kind)
                && visited_params.insert((k, idx, sk))
            {
                param_work.push((k, idx, kind));
            }
        };
        if !matches!(acc, BoundAcc::Static) {
            for &(key, r_start, r_len) in resolved_traits.iter() {
                if visited_traits.insert(key) {
                    trait_work.push(key);
                }
                if let Some(slice) = r_start.checked_add(r_len).and_then(|e0| resolved_args.get(r_start..e0)) {
                    for (i, &opt_kind) in slice.iter().enumerate() {
                        if let (Some(kind), Ok(idx)) = (opt_kind, u32::try_from(i)) {
                            enqueue_param(key, idx, kind, &mut visited_params, &mut param_work);
                        }
                    }
                }
            }
        }
        // Phase 1: Reachable traits and parameter-independent `'static` sources/edges — O(V + E).
        let mut ti = 0usize;
        while let Some(&k) = trait_work.get(ti) {
            ti += 1;
            if self.heads.trait_self_static(k) == Some(true) && acc.push(SlotKind::Static) {
                break;
            }
            if let Some(sl) = self.heads.trait_super_lifetimes(k) {
                if sl.alias_outlives.scan_for(&SuperLifetime::Static).is_some() && acc.push(SlotKind::Static) {
                    break;
                }
                for &(sk, e_start, e_end) in sl.edges.iter() {
                    if visited_traits.insert(sk) {
                        trait_work.push(sk);
                    }
                    if let (Ok(es), Ok(ee)) = (usize::try_from(e_start), usize::try_from(e_end))
                        && let Some(edge_args) = sl.args.get(es..ee)
                    {
                        for (j, &ea) in edge_args.iter().enumerate() {
                            if ea == SuperLifetime::Static
                                && let Ok(j32) = u32::try_from(j)
                            {
                                enqueue_param(sk, j32, SlotKind::Static, &mut visited_params, &mut param_work);
                            }
                        }
                    }
                }
            }
        }
        // Phase 2: Per-parameter `(trait, param_idx, slot)` propagation — O((V + E) * S).
        if !matches!(acc, BoundAcc::Static) {
            let mut pi = 0usize;
            while let Some(&(k, param_idx, kind)) = param_work.get(pi) {
                pi += 1;
                if let Some(outlives) = self.heads.trait_self_outlives_params(k)
                    && outlives.binary_search(&param_idx).is_ok()
                    && acc.push(kind)
                {
                    break;
                }
                if let Some(sl) = self.heads.trait_super_lifetimes(k) {
                    if sl.alias_outlives.scan_for(&SuperLifetime::Param(param_idx)).is_some() && acc.push(kind) {
                        break;
                    }
                    for &(sk, e_start, e_end) in sl.edges.iter() {
                        if let (Ok(es), Ok(ee)) = (usize::try_from(e_start), usize::try_from(e_end))
                            && let Some(edge_args) = sl.args.get(es..ee)
                        {
                            for (j, &ea) in edge_args.iter().enumerate() {
                                if ea == SuperLifetime::Param(param_idx)
                                    && let Ok(j32) = u32::try_from(j)
                                {
                                    enqueue_param(sk, j32, kind, &mut visited_params, &mut param_work);
                                }
                            }
                        }
                    }
                }
            }
        }
        match acc {
            BoundAcc::Static => TraitDefaultBound::Unique(SlotKind::Static),
            BoundAcc::None => TraitDefaultBound::None,
            BoundAcc::One(kind) => TraitDefaultBound::Unique(kind),
            BoundAcc::Ambiguous => TraitDefaultBound::Ambiguous,
        }
    }

    /// Derives the RFC 1156 trait-bound default object lifetime for a single trait or trait alias
    /// `root_key` with substituted lifetime slots `root_lts`, using pre-filed `Heads` facts.
    fn trait_object_default_bound<'s>(
        &self,
        root_key: LibKey,
        root_lts: &[Slot],
        s: &mut RegionSink<'s>,
        _r: Range,
    ) -> Result<Result<TraitDefaultBound, RegionRefusal>, Why> {
        let alloc = s.alloc;
        let mut resolved_traits: blaze_own::Vec<(LibKey, usize, usize), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
        let mut resolved_args: blaze_own::Vec<Option<SlotKind>, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
        let mut alias_direct_bounds: blaze_own::Vec<SlotKind, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
        if let Some(ah) = self.heads.trait_alias(root_key) {
            for &ao in ah.super_lifetimes.alias_outlives.iter() {
                let mapped = match ao {
                    SuperLifetime::Static => Some(SlotKind::Static),
                    SuperLifetime::Param(pi) => usize::try_from(pi).ok().and_then(|i| root_lts.get(i)).map(|slot| slot.kind),
                    SuperLifetime::Other => None,
                };
                if let Some(kind) = mapped {
                    alias_direct_bounds.push(kind);
                }
            }
            for &(k, e_start, e_end) in ah.super_lifetimes.edges.iter() {
                let r_start = resolved_args.len();
                if let (Ok(es), Ok(ee)) = (usize::try_from(e_start), usize::try_from(e_end))
                    && let Some(edge_args) = ah.super_lifetimes.args.get(es..ee)
                {
                    for &ea in edge_args {
                        let mapped = match ea {
                            SuperLifetime::Static => Some(SlotKind::Static),
                            SuperLifetime::Param(pi) => usize::try_from(pi).ok().and_then(|i| root_lts.get(i)).map(|slot| slot.kind),
                            SuperLifetime::Other => None,
                        };
                        resolved_args.push(mapped);
                    }
                }
                resolved_traits.push((k, r_start, resolved_args.len().saturating_sub(r_start)));
            }
        } else {
            for &slot in root_lts {
                resolved_args.push(Some(slot.kind));
            }
            resolved_traits.push((root_key, 0, resolved_args.len()));
        }
        Ok(Ok(self.resolve_rfc1156_default_lifetime(&resolved_traits, &resolved_args, &alias_direct_bounds, alloc)))
    }

    /// Populates `trait_lts` and `lt_pool` with substituted lifetime slots for every supertrait
    /// reachable from `root_key` (bounded to 64 traits) so `ExProj` nodes targeting a supertrait
    /// (e.g. `trait Super<'a> { type A; } trait Sub<'a>: Super<'a> {}` written `&Sub<'a, A = &'a u8>`)
    /// find their owner trait's lifetime slots.
    #[expect(clippy::too_many_arguments, reason = "supertrait lifetime walk: root key, root pool slice, outputs, sink, site")]
    fn populate_supertrait_lts<'s>(
        &self,
        root_key: LibKey,
        root_start: u32,
        root_len: u32,
        trait_lts: &mut blaze_own::Vec<(ItemKey, u32, u32), Alloc<'s>>,
        lt_pool: &mut blaze_own::Vec<Slot, Alloc<'s>>,
        s: &mut RegionSink<'s>,
        r: Range,
    ) -> Result<Result<(), RegionRefusal>, Why> {
        const MOST_SUPER_WALK: usize = 64;
        let alloc = s.alloc;
        let mut work: blaze_own::Vec<(LibKey, u32, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
        let mut visited: blaze_own::Vec<LibKey, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
        work.push((root_key, root_start, root_len));
        visited.push(root_key);
        let mut next = 0usize;
        while let Some(&(k, start, len)) = work.get(next) {
            next += 1;
            let Some(item_text) = self.resolver.names.signature_text_of(k) else {
                continue;
            };
            let marks = match super::item_site::item_marks(self.resolver.names, k, item_text.text) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let Ok(st) = usize::try_from(start) else { continue };
            let Ok(en) = usize::try_from(start.saturating_add(len)) else { continue };
            let mut cur_lts: blaze_own::Vec<SlotKind, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            if let Some(sl) = lt_pool.get(st..en) {
                for &slot in sl {
                    cur_lts.push(slot.kind);
                }
            } else {
                continue;
            }
            let lowered = super::heads::with_lowered(self.resolver.names, k, item_text.text, |tv| -> Result<(), Why> {
                let mut lt_names: blaze_own::Vec<Name, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                for &row in tv.rows(tv.list(blaze_lower::head::GENERICS))? {
                    if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(row)? {
                        lt_names.push(Name(tv.name(name)?));
                    }
                }
                if lt_names.len() != cur_lts.len() {
                    return Ok(());
                }
                let mut bound_lists: blaze_own::Vec<List, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                bound_lists.push(tv.list(blaze_lower::head::BOUNDS));
                for &row in tv.rows(tv.list(blaze_lower::head::WHERE))? {
                    if let lf::WherePred::Type { ty, bounds, .. } = lf::WherePred::decode(row)?
                        && super::dyn_ty::is_self(tv, ty)?
                    {
                        bound_lists.push(bounds);
                    }
                }
                for bl in bound_lists {
                    for &row in tv.rows(bl)? {
                        let lf::Bound::Trait { ty, binder, mods } = lf::Bound::decode(row)? else { continue };
                        if mods.maybe || mods.negative || !tv.rows(binder)?.is_empty() {
                            continue;
                        }
                        let lf::TypeRef::Path(bp) = tv.ty(ty)? else { continue };
                        let Ok((babs, btexts, bsegs, br)) = path_texts_of(tv, bp, true) else { continue };
                        let mut sub_lts: blaze_own::Vec<SlotKind, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                        let mut bad_lt = false;
                        if let Some(blast) = bsegs.last()
                            && blast.has_args
                            && !blast.parenthesized
                        {
                            for &arow in tv.rows(blast.args)? {
                                if let lf::GenericArg::Lifetime(nm) = lf::GenericArg::decode(arow)? {
                                    let text = tv.name_text(nm)?;
                                    let kind = match text.strip_prefix('\'').unwrap_or(text) {
                                        "static" => SlotKind::Static,
                                        "_" => s.new_elided(r),
                                        _ => {
                                            let name = Name(tv.name(nm)?);
                                            match lt_names.iter().position(|&x| x == name).and_then(|pos| cur_lts.get(pos).copied()) {
                                                Some(k) => k,
                                                None => {
                                                    bad_lt = true;
                                                    break;
                                                }
                                            }
                                        }
                                    };
                                    sub_lts.push(kind);
                                }
                            }
                        }
                        if bad_lt {
                            continue;
                        }
                        let found = match super::item_site::resolve_written(&self.resolver, marks, item_text.module, tv, bp, babs, &btexts, Ns::Type, br) {
                            Ok(f) if usize::try_from(f.consumed).ok() == Some(btexts.len()) => f,
                            Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                            _ => continue,
                        };
                        match found.kind {
                            Kind::Trait => {
                                let sub_checker = self.heads.checker(found.key);
                                let Some(sub_want) = declared_lifetimes(self.heads, self.resolver.names, sub_checker, Declared::Trait) else {
                                    continue;
                                };
                                let Ok(sub_start) = u32::try_from(lt_pool.len()) else { continue };
                                let written_n = u32::try_from(sub_lts.len()).unwrap_or(u32::MAX);
                                if written_n == 0 {
                                    for _ in 0..sub_want {
                                        lt_pool.push(Slot::quiet(s.new_elided(r)));
                                    }
                                } else if written_n == sub_want {
                                    for &kind in &sub_lts {
                                        lt_pool.push(Slot::quiet(kind));
                                    }
                                } else {
                                    continue;
                                }
                                if sub_want > 0 && !trait_lts.iter().any(|&(ck, _, _)| ck == sub_checker) {
                                    trait_lts.push((sub_checker, sub_start, sub_want));
                                }
                                if !visited.iter().any(|&v| v == found.key) && visited.len() < MOST_SUPER_WALK {
                                    visited.push(found.key);
                                    work.push((found.key, sub_start, sub_want));
                                }
                            }
                            Kind::TraitAlias => {
                                let Ok(sub_start) = u32::try_from(lt_pool.len()) else { continue };
                                let Ok(sub_len) = u32::try_from(sub_lts.len()) else { continue };
                                for &kind in &sub_lts {
                                    lt_pool.push(Slot::quiet(kind));
                                }
                                if !visited.iter().any(|&v| v == found.key) && visited.len() < MOST_SUPER_WALK {
                                    visited.push(found.key);
                                    work.push((found.key, sub_start, sub_len));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Ok(())
            });
            if let Ok(Err(why)) = lowered {
                return Err(why);
            }
        }
        Ok(Ok(()))
    }

    /// Lays the full region column of a bare trait alias (`trait Alias<'a, U> = Gen<&'a u8, U> + ...;`)
    /// into `s.scratch`, including nested type-argument and projection-term regions, supertrait
    /// projections, use-site associated bindings (`&IterAlias<Item = &'a u8>`), omitted type/const
    /// defaults, and nested trait aliases (bounded to depth 8).
    #[expect(clippy::too_many_arguments, reason = "trait alias dyn region layout: target, made type, lifetimes, caller args/slots, assoc bindings, whole counts, sink, site, depth")]
    fn lay_trait_alias_dyn_regions<'s>(
        &self,
        tys: &mut PortableTys<'_>,
        made: PTy,
        alias_key: LibKey,
        use_lifetimes: &[SlotKind],
        (args, pool, index): (&[PTy], &[Slot], &[(u32, u32, u32)]),
        assoc_binds: &[(Name, PTy, (u32, u32, u32))],
        whole: Sub,
        s: &mut RegionSink<'s>,
        r: Range,
        depth: u8,
    ) -> Result<Laid, Why> {
        const MOST_ALIAS_DEPTH: u8 = 8;
        let alloc = s.alloc;
        let mut cur_alias_key = alias_key;
        let mut cur_lts: blaze_own::Vec<SlotKind, Alloc<'s>> = blaze_own::Vec::with_capacity_in(use_lifetimes.len(), alloc);
        cur_lts.extend_from_slice(use_lifetimes);
        let mut cur_args: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::with_capacity_in(args.len(), alloc);
        cur_args.extend_from_slice(args);
        let mut cur_pool: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::with_capacity_in(pool.len(), alloc);
        cur_pool.extend_from_slice(pool);
        let mut cur_index: blaze_own::Vec<(u32, u32, u32), Alloc<'s>> = blaze_own::Vec::with_capacity_in(index.len(), alloc);
        cur_index.extend_from_slice(index);
        let mut cur_assoc_binds: blaze_own::Vec<(Name, PTy, (u32, u32, u32)), Alloc<'s>> = blaze_own::Vec::with_capacity_in(assoc_binds.len(), alloc);
        cur_assoc_binds.extend_from_slice(assoc_binds);
        let mut cur_depth = depth;
        let mut post_stack: blaze_own::Vec<AliasDynPost, Alloc<'s>> = blaze_own::Vec::new_in(alloc);

        loop {
            if cur_depth >= MOST_ALIAS_DEPTH || s.default_depth >= MOST_DEFAULT_DEPTH {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
            }
            let alias_key = cur_alias_key;
            let use_lifetimes = &cur_lts[..];
            let args = &cur_args[..];
            let pool = &cur_pool[..];
            let index = &cur_index[..];
            let assoc_binds = &cur_assoc_binds[..];
            let Some(alias_text) = self.resolver.names.signature_text_of(alias_key) else {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
            };
            let marks = match super::item_site::item_marks(self.resolver.names, alias_key, alias_text.text) {
                Ok(m) => m,
                Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r))),
            };
            let lowered = super::heads::with_lowered(self.resolver.names, alias_key, alias_text.text, |av| -> Result<Result<AliasDynStep<'s>, RegionRefusal>, Why> {
            let mut lt_names: blaze_own::Vec<Name, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            let mut ps: blaze_own::Vec<(Name, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            let mut param_defaults: blaze_own::Vec<Option<AliasDefault>, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            let mut param_i = 1u32;
            for &row in av.rows(av.list(blaze_lower::head::GENERICS))? {
                match lf::GenericParam::decode(row)? {
                    lf::GenericParam::Lifetime { name, .. } => lt_names.push(Name(av.name(name)?)),
                    lf::GenericParam::Type { name, default, .. } => {
                        ps.push((Name(av.name(name)?), param_i));
                        param_defaults.push(default.map(AliasDefault::Type));
                        param_i += 1;
                    }
                    lf::GenericParam::Const { name, default, .. } => {
                        ps.push((Name(av.name(name)?), param_i));
                        param_defaults.push(default.map(|c| AliasDefault::Const(param_i, c)));
                        param_i += 1;
                    }
                }
            }
            let want = u32::try_from(lt_names.len()).unwrap_or(u32::MAX);
            let got = u32::try_from(use_lifetimes.len()).unwrap_or(u32::MAX);
            let mut given_lts: blaze_own::Vec<SlotKind, Alloc<'s>> = blaze_own::Vec::with_capacity_in(lt_names.len(), alloc);
            if use_lifetimes.is_empty() {
                for _ in 0..lt_names.len() {
                    given_lts.push(s.new_elided(r));
                }
            } else if got == want {
                given_lts.extend_from_slice(use_lifetimes);
            } else {
                return Ok(Err(RegionRefusal::LifetimeArgs { at: r, want, got }));
            }
            let cs = self.heads.const_params(alias_key).unwrap_or(&[]);
            let alias_env = Env {
                module: alias_text.module,
                params: &ps,
                bounds: Bounds::Building(&[]),
                self_: SelfTy::None,
                self_assoc: &[],
                anon: &[],
                consts: cs,
                body: None,
            };
            let alias_builder = Builder {
                resolver: crate::resolve::Paths { names: self.resolver.names },
                heads: self.heads,
                marks,
                owner: Some((alias_key, alias_text.text)),
                owner_key: Some(super::keys::checker_of(alias_key)),
                expanded: None,
                expanded_marks: None,
            };
            // Fill any omitted defaulted type/const arguments declared by the trait alias
            // (`trait Alias<T = &'static u8> = Gen<T>;`, `trait Alias<const N: usize = 3> = Gen<N>;`)
            // into `all_args`/`all_pool`/`all_index` with `Slot::quiet`.
            let mut all_args: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::with_capacity_in(ps.len(), alloc);
            all_args.extend_from_slice(args);
            let mut all_pool: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::with_capacity_in(pool.len(), alloc);
            all_pool.extend_from_slice(pool);
            let mut all_index: blaze_own::Vec<(u32, u32, u32), Alloc<'s>> = blaze_own::Vec::with_capacity_in(ps.len(), alloc);
            all_index.extend_from_slice(index);
            if all_index.len() < ps.len() {
                let mut subst_so_far: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::with_capacity_in(ps.len() + 1, alloc);
                subst_so_far.push(intern(tys, PNode::Param(u32::MAX))?);
                subst_so_far.extend_from_slice(&all_args);
                let mut def_elided_map: blaze_own::Vec<(u32, SlotKind), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                for def_idx in all_index.len()..ps.len() {
                    let def_ty_id = match param_defaults.get(def_idx).copied().flatten() {
                        Some(AliasDefault::Type(t)) => t,
                        Some(AliasDefault::Const(pi, c)) => {
                            let at = av.const_range(c)?;
                            let cr = av.site(at)?;
                            let Some(&(_, want_ty)) = cs.iter().find(|(ci, _)| *ci == pi) else {
                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
                            };
                            let raw_c = match alias_builder.const_term(tys, &alias_env, av.text_at(at)?, cr, &|_| Ok(want_ty)) {
                                Ok(t) => t,
                                Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                                Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r))),
                            };
                            let filled_ty = subst(tys, raw_c, &subst_so_far)?;
                            all_index.push((0, 0, 0));
                            all_args.push(filled_ty);
                            subst_so_far.push(filled_ty);
                            continue;
                        }
                        None => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r))),
                    };
                    let mut def_sink = RegionSink::new_in(alloc);
                    def_sink.default_depth = s.default_depth + 1;
                    let raw_def = match alias_builder.ty_regions(tys, &alias_env, av, def_ty_id, &mut def_sink) {
                        Ok(t) => t,
                        Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                        Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r))),
                    };
                    let ([frag], None) = (def_sink.frags(), def_sink.refused()) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
                    };
                    let raw_slots = def_sink.slots_of(*frag);
                    let Ok(dstart) = u32::try_from(all_pool.len()) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                    };
                    let mut dvisits: blaze_own::Vec<(PTy, PNode, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                    let def_arity = HeadsArity { heads: self.heads, opaques: &def_sink.opaques, names: self.resolver.names };
                    soft!(each_slot_node(tys.view(), raw_def, &def_arity, alloc, &mut |t, node, own| {
                        dvisits.push((t, node, own));
                        Ok(())
                    }));
                    let mut dbinder_at: blaze_own::Vec<u32, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                    let mut dnext = 0usize;
                    let mut ddb = 0u32;
                    for &(_, dnode, down) in &dvisits {
                        if let PNode::Param(pv) = dnode
                            && let Some(off) = pv.checked_sub(1)
                            && let Ok(pi) = usize::try_from(off)
                            && let Some(&(pstart, plen, pb)) = all_index.get(pi)
                        {
                            let Ok(pst) = usize::try_from(pstart) else { return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r))) };
                            let Ok(pen) = usize::try_from(pstart.saturating_add(plen)) else { return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r))) };
                            let mut copied: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                            if let Some(psl) = all_pool.get(pst..pen) {
                                copied.extend_from_slice(psl);
                            } else {
                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                            }
                            for slot in copied {
                                all_pool.push(match slot.kind {
                                    SlotKind::ObjectDefault(_) => Slot::quiet(SlotKind::Static),
                                    _ => Slot::quiet(slot.shifted(ddb).kind),
                                });
                            }
                            ddb += pb;
                            continue;
                        }
                        if down > 0 {
                            let Ok(own_u) = usize::try_from(down) else { return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r))) };
                            let Some(col) = raw_slots.get(dnext..dnext.saturating_add(own_u)) else {
                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                            };
                            dnext += own_u;
                            for &dslot in col {
                                let mapped = match dslot.kind {
                                    SlotKind::Static => Slot::quiet(SlotKind::Static),
                                    SlotKind::Named(name, _) => {
                                        let Some(pos) = lt_names.iter().position(|&n| n == name) else {
                                            return Ok(Err(RegionRefusal::Undeclared { at: r }));
                                        };
                                        let Some(&k) = given_lts.get(pos) else {
                                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                                        };
                                        Slot::quiet(k)
                                    }
                                    SlotKind::Late { binder, var } => match usize::try_from(binder).ok().and_then(|bi| dbinder_at.get(bi)) {
                                        Some(&b) => Slot::quiet(SlotKind::Late { binder: b, var }),
                                        None => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r))),
                                    },
                                    SlotKind::ObjectDefault(_) => Slot::quiet(SlotKind::Static),
                                    SlotKind::Elided(id) => {
                                        let kind = match def_elided_map.iter().find(|&&(eid, _)| eid == id) {
                                            Some(&(_, k)) => k,
                                            None => {
                                                let k = s.new_elided(r);
                                                def_elided_map.push((id, k));
                                                k
                                            }
                                        };
                                        Slot::quiet(kind)
                                    }
                                    SlotKind::Bound(_) | SlotKind::Given(_) => {
                                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DefaultArg, r)));
                                    }
                                };
                                all_pool.push(mapped);
                            }
                        }
                        if is_binder(dnode) {
                            dbinder_at.push(ddb);
                            ddb += 1;
                        }
                    }
                    let Ok(dlen) = u32::try_from(all_pool.len()).map(|e| e.saturating_sub(dstart)) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                    };
                    all_index.push((dstart, dlen, ddb));
                    let filled_ty = subst(tys, raw_def, &subst_so_far)?;
                    all_args.push(filled_ty);
                    subst_so_far.push(filled_ty);
                }
            }
            let bounds = av.list(blaze_lower::head::BOUNDS);
            // Partition nested `Kind::TraitAlias` bounds: pure auto-trait aliases (which expand
            // only to auto traits with zero projections and zero slots, e.g. `trait Marker = Send;`)
            // can coexist with a principal trait or principal-bearing nested alias
            // (`trait Combo = Base + Marker;`).
            let is_auto_only_alias = |ak: LibKey| -> bool {
                let Some(head) = self.heads.trait_alias(ak) else { return false };
                let no_projs = !head.clauses.iter().any(|c| matches!(c, PClause::Projection { .. }));
                let all_auto = head.traits.iter().all(|&(tk, _)| self.heads.trait_auto(tk) == Some(true));
                no_projs && all_auto
            };
            let mut principal_alias: Option<(LibKey, PathIdx, lf::Segment, lf::List)> = None;
            let mut has_direct_principal = false;
            let mut extra_obj_lt: Option<Slot> = None;
            let mut extra_obj_written = 0u32;
            let mut extra_has_static = false;
            let mut extra_ambiguous = false;
            let mut extra_unique: Option<SlotKind> = None;
            for &row in av.rows(bounds)? {
                match lf::Bound::decode(row)? {
                    lf::Bound::Lifetime(nm) => {
                        let text = av.name_text(nm)?;
                        let kind = match text.strip_prefix('\'').unwrap_or(text) {
                            "static" => {
                                extra_has_static = true;
                                SlotKind::Static
                            }
                            "_" => s.new_elided(r),
                            _ => {
                                let name = Name(av.name(nm)?);
                                match lt_names.iter().position(|&x| x == name).and_then(|pos| given_lts.get(pos).copied()) {
                                    Some(k) => k,
                                    None => return Ok(Err(RegionRefusal::Undeclared { at: r })),
                                }
                            }
                        };
                        extra_obj_written += 1;
                        extra_obj_lt = Some(Slot::written(kind));
                    }
                    lf::Bound::Trait { ty, binder, mods } => {
                        if mods.maybe || mods.negative {
                            continue;
                        }
                        let Some((bp, bseg)) = super::dyn_ty::dyn_bound(av, lf::Bound::Trait { ty, binder, mods }, r)? else {
                            continue;
                        };
                        let mut bound_names: blaze_own::Vec<Name, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                        for &brow in av.rows(binder)? {
                            if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(brow)? {
                                bound_names.push(Name(av.name(name)?));
                            }
                        }
                        let (babs, btexts, _, br) = path_texts_of(av, bp, true)?;
                        let found = match super::item_site::resolve_written(&self.resolver, marks, alias_text.module, av, bp, babs, &btexts, Ns::Type, br) {
                            Ok(f) if usize::try_from(f.consumed).ok() == Some(btexts.len()) => f,
                            Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                            _ => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::Resolve, r))),
                        };
                        if found.kind == Kind::TraitAlias {
                            if is_auto_only_alias(found.key) {
                                // Collect any lifetime or trait default bound from this pure
                                // auto-trait alias without refusing multiple nested aliases.
                                let mut sub_lt_slots: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                                if bseg.has_args && !bseg.parenthesized {
                                    for &arow in av.rows(bseg.args)? {
                                        if let lf::GenericArg::Lifetime(nm) = lf::GenericArg::decode(arow)? {
                                            let text = av.name_text(nm)?;
                                            let kind = match text.strip_prefix('\'').unwrap_or(text) {
                                                "static" => SlotKind::Static,
                                                "_" => s.new_elided(r),
                                                _ => {
                                                    let name = Name(av.name(nm)?);
                                                    if let Some(bpos) = bound_names.iter().position(|&x| x == name) {
                                                        let Ok(v) = u32::try_from(bpos) else {
                                                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                                                        };
                                                        SlotKind::Bound(v)
                                                    } else if let Some(k) = lt_names.iter().position(|&x| x == name).and_then(|pos| given_lts.get(pos).copied()) {
                                                        k
                                                    } else {
                                                        return Ok(Err(RegionRefusal::Undeclared { at: r }));
                                                    }
                                                }
                                            };
                                            sub_lt_slots.push(Slot::written(kind));
                                        }
                                    }
                                }
                                match self.trait_object_default_bound(found.key, &sub_lt_slots, s, r)? {
                                    Ok(TraitDefaultBound::Unique(SlotKind::Static)) => extra_has_static = true,
                                    Ok(TraitDefaultBound::Unique(k)) => match extra_unique {
                                        None => extra_unique = Some(k),
                                        Some(prev) if same_region(prev, k) => {}
                                        Some(_) => extra_ambiguous = true,
                                    },
                                    Ok(TraitDefaultBound::Ambiguous) => extra_ambiguous = true,
                                    Ok(TraitDefaultBound::None) => {}
                                    Err(why) => return Ok(Err(why)),
                                }
                            } else {
                                if principal_alias.is_some() || has_direct_principal {
                                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                                }
                                principal_alias = Some((found.key, bp, bseg, binder));
                            }
                        } else if found.kind == Kind::Trait {
                            if self.heads.trait_auto(found.key) != Some(true) {
                                if principal_alias.is_some() || has_direct_principal {
                                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                                }
                                has_direct_principal = true;
                            }
                            let mut sub_lt_slots: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                            if bseg.has_args && !bseg.parenthesized {
                                for &arow in av.rows(bseg.args)? {
                                    if let lf::GenericArg::Lifetime(nm) = lf::GenericArg::decode(arow)? {
                                        let text = av.name_text(nm)?;
                                        let kind = match text.strip_prefix('\'').unwrap_or(text) {
                                            "static" => SlotKind::Static,
                                            "_" => s.new_elided(r),
                                            _ => {
                                                let name = Name(av.name(nm)?);
                                                if let Some(bpos) = bound_names.iter().position(|&x| x == name) {
                                                    let Ok(v) = u32::try_from(bpos) else {
                                                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                                                    };
                                                    SlotKind::Bound(v)
                                                } else if let Some(k) = lt_names.iter().position(|&x| x == name).and_then(|pos| given_lts.get(pos).copied()) {
                                                    k
                                                } else {
                                                    return Ok(Err(RegionRefusal::Undeclared { at: r }));
                                                }
                                            }
                                        };
                                        sub_lt_slots.push(Slot::written(kind));
                                    }
                                }
                            }
                            match self.trait_object_default_bound(found.key, &sub_lt_slots, s, r)? {
                                Ok(TraitDefaultBound::Unique(SlotKind::Static)) => extra_has_static = true,
                                Ok(TraitDefaultBound::Unique(k)) => match extra_unique {
                                    None => extra_unique = Some(k),
                                    Some(prev) if same_region(prev, k) => {}
                                    Some(_) => extra_ambiguous = true,
                                },
                                Ok(TraitDefaultBound::Ambiguous) => extra_ambiguous = true,
                                Ok(TraitDefaultBound::None) => {}
                                Err(why) => return Ok(Err(why)),
                            }
                        }
                    }
                    lf::Bound::Use(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r))),
                }
            }
            if extra_obj_written > 1 {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynLifetimes, r)));
            }
            if let Some((sub_alias_key, _bp, bseg, b_binder)) = principal_alias {
                // Nested trait alias (`trait B<'b> = A<'b> + Auto + 'b;`): substitute lifetime,
                // type, and const arguments (plus any `for<'a>` binder) passed to `sub_alias_key`,
                // then recurse.
                if bseg.parenthesized {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                }
                let mut b_names: blaze_own::Vec<Name, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                for &brow in av.rows(b_binder)? {
                    if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(brow)? {
                        b_names.push(Name(av.name(name)?));
                    }
                }
                #[derive(Clone, Copy)]
                enum NestedAliasArg {
                    Type(lf::TypeId),
                    Const(lf::ConstIdx),
                }
                let mut sub_lts: blaze_own::Vec<SlotKind, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                let mut sub_arg_items: blaze_own::Vec<NestedAliasArg, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                if bseg.has_args {
                    for &arow in av.rows(bseg.args)? {
                        match lf::GenericArg::decode(arow)? {
                            lf::GenericArg::Lifetime(nm) => {
                                let text = av.name_text(nm)?;
                                let kind = match text.strip_prefix('\'').unwrap_or(text) {
                                    "static" => SlotKind::Static,
                                    "_" => s.new_elided(r),
                                    _ => {
                                        let name = Name(av.name(nm)?);
                                        if let Some(bpos) = b_names.iter().position(|&x| x == name) {
                                            let Ok(v) = u32::try_from(bpos) else {
                                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                                            };
                                            SlotKind::Bound(v)
                                        } else if let Some(k) = lt_names.iter().position(|&x| x == name).and_then(|pos| given_lts.get(pos).copied()) {
                                            k
                                        } else {
                                            return Ok(Err(RegionRefusal::Undeclared { at: r }));
                                        }
                                    }
                                };
                                sub_lts.push(kind);
                            }
                            lf::GenericArg::Type(t) => sub_arg_items.push(NestedAliasArg::Type(t)),
                            lf::GenericArg::Const(c) => sub_arg_items.push(NestedAliasArg::Const(c)),
                            _ => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r))),
                        }
                    }
                }
                let sub_cs = self.heads.const_params(sub_alias_key).unwrap_or(&[]);
                let mut sub_args: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::with_capacity_in(sub_arg_items.len(), alloc);
                let mut sub_pool: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                let mut sub_index: blaze_own::Vec<(u32, u32, u32), Alloc<'s>> = blaze_own::Vec::with_capacity_in(sub_arg_items.len(), alloc);
                let mut elided_map: blaze_own::Vec<(u32, SlotKind), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                let mut subst_args: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::with_capacity_in(all_args.len() + 1, alloc);
                subst_args.push(intern(tys, PNode::Param(u32::MAX))?);
                subst_args.extend_from_slice(&all_args);
                for (arg_pos, &item) in sub_arg_items.iter().enumerate() {
                    let sub_const_ty = u32::try_from(arg_pos.saturating_add(1))
                        .ok()
                        .and_then(|pi| sub_cs.iter().find(|&&(ci, _)| ci == pi).map(|&(_, ty)| ty));
                    let at_id = match item {
                        NestedAliasArg::Const(c) => {
                            let Some(want_ty) = sub_const_ty else {
                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                            };
                            let at = av.const_range(c)?;
                            let cr = av.site(at)?;
                            let raw_c = match alias_builder.const_term(tys, &alias_env, av.text_at(at)?, cr, &|_| Ok(want_ty)) {
                                Ok(t) => t,
                                Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                                Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r))),
                            };
                            sub_index.push((0, 0, 0));
                            sub_args.push(subst(tys, raw_c, &subst_args)?);
                            continue;
                        }
                        NestedAliasArg::Type(t) => {
                            if let Some(nm) = bare_name(av, t)? {
                                if let Some(&(_, pidx)) = alias_env.params.iter().find(|(n, _)| *n == Name::of(nm))
                                    && alias_env.consts.iter().any(|&(c, _)| c == pidx)
                                {
                                    let raw_c = intern(tys, PNode::Param(pidx))?;
                                    sub_index.push((0, 0, 0));
                                    sub_args.push(subst(tys, raw_c, &subst_args)?);
                                    continue;
                                }
                                if let Some(want_ty) = sub_const_ty {
                                    let cr = av.site(av.type_range(t)?)?;
                                    let raw_c = match alias_builder.const_term(tys, &alias_env, nm, cr, &|_| Ok(want_ty)) {
                                        Ok(c) => c,
                                        Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                                        Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r))),
                                    };
                                    sub_index.push((0, 0, 0));
                                    sub_args.push(subst(tys, raw_c, &subst_args)?);
                                    continue;
                                }
                            }
                            t
                        }
                    };
                    let mut tmp_sink = RegionSink::new_in(alloc);
                    tmp_sink.default_depth = s.default_depth + 1;
                    let raw_a = match alias_builder.ty_regions(tys, &alias_env, av, at_id, &mut tmp_sink) {
                        Ok(t) => t,
                        Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                        Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r))),
                    };
                    let ([frag], None) = (tmp_sink.frags(), tmp_sink.refused()) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                    };
                    let raw_slots = tmp_sink.slots_of(*frag);
                    let start = u32::try_from(sub_pool.len()).map_err(|_| Why::Program("region pool overflow".into(), r))?;
                    let mut visits: blaze_own::Vec<(PTy, PNode, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                    let tmp_arity = HeadsArity { heads: self.heads, opaques: &tmp_sink.opaques, names: self.resolver.names };
                    soft!(each_slot_node(tys.view(), raw_a, &tmp_arity, alloc, &mut |t, node, own| {
                        visits.push((t, node, own));
                        Ok(())
                    }));
                    let mut binder_at: blaze_own::Vec<u32, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                    let mut next = 0usize;
                    let mut db = 0u32;
                    for &(_, node, own) in &visits {
                        if let PNode::Param(pv) = node
                            && let Some(off) = pv.checked_sub(1)
                            && let Ok(pi) = usize::try_from(off)
                            && let Some(&(pstart, plen, pb)) = all_index.get(pi)
                        {
                            let Ok(pst) = usize::try_from(pstart) else { return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r))) };
                            let Ok(pen) = usize::try_from(pstart.saturating_add(plen)) else { return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r))) };
                            let Some(psl) = all_pool.get(pst..pen) else { return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r))) };
                            for &slot in psl {
                                sub_pool.push(match slot.kind {
                                    SlotKind::ObjectDefault(_) => Slot::quiet(SlotKind::Static),
                                    _ => slot.shifted(db),
                                });
                            }
                            db += pb;
                            continue;
                        }
                        if own > 0 {
                            let Ok(own_u) = usize::try_from(own) else { return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r))) };
                            let Some(col) = raw_slots.get(next..next.saturating_add(own_u)) else {
                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                            };
                            next += own_u;
                            for &dslot in col {
                                let mapped = match dslot.kind {
                                    SlotKind::Static => Slot { kind: SlotKind::Static, counts: dslot.counts },
                                    SlotKind::Named(name, _) => {
                                        if let Some(bpos) = b_names.iter().position(|&n| n == name) {
                                            let Ok(v) = u32::try_from(bpos) else {
                                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                                            };
                                            Slot { kind: SlotKind::Bound(v), counts: dslot.counts }
                                        } else {
                                            let Some(pos) = lt_names.iter().position(|&n| n == name) else {
                                                return Ok(Err(RegionRefusal::Undeclared { at: r }));
                                            };
                                            let Some(&k) = given_lts.get(pos) else {
                                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                                            };
                                            Slot { kind: k, counts: dslot.counts }
                                        }
                                    }
                                    SlotKind::Late { binder, var } => match usize::try_from(binder).ok().and_then(|bi| binder_at.get(bi)) {
                                        Some(&b) => Slot { kind: SlotKind::Late { binder: b, var }, counts: dslot.counts },
                                        None => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r))),
                                    },
                                    SlotKind::ObjectDefault(_) => Slot::quiet(SlotKind::Static),
                                    SlotKind::Elided(id) => {
                                        let kind = match elided_map.iter().find(|&&(eid, _)| eid == id) {
                                            Some(&(_, k)) => k,
                                            None => {
                                                let k = s.new_elided(r);
                                                elided_map.push((id, k));
                                                k
                                            }
                                        };
                                        Slot { kind, counts: dslot.counts }
                                    }
                                    SlotKind::Bound(_) | SlotKind::Given(_) => {
                                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                                    }
                                };
                                sub_pool.push(mapped);
                            }
                        }
                        if is_binder(node) {
                            binder_at.push(db);
                            db += 1;
                        }
                    }
                    let Ok(sub_len) = u32::try_from(sub_pool.len()).map(|e| e.saturating_sub(start)) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                    };
                    sub_index.push((start, sub_len, db));
                    sub_args.push(subst(tys, raw_a, &subst_args)?);
                }
                let mut sub_assoc_binds: blaze_own::Vec<(Name, PTy, (u32, u32, u32)), Alloc<'s>> = blaze_own::Vec::with_capacity_in(assoc_binds.len(), alloc);
                for &(bn, bt, (bstart, blen, bb)) in assoc_binds {
                    let Ok(bst) = usize::try_from(bstart) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                    };
                    let Ok(ben) = usize::try_from(bstart.saturating_add(blen)) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                    };
                    let Some(bsl) = pool.get(bst..ben) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                    };
                    let Ok(new_start) = u32::try_from(sub_pool.len()) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                    };
                    sub_pool.extend_from_slice(bsl);
                    sub_assoc_binds.push((bn, bt, (new_start, blen, bb)));
                }
                return Ok(Ok(AliasDynStep::Next {
                    alias_key: sub_alias_key,
                    use_lifetimes: sub_lts,
                    args: sub_args,
                    pool: sub_pool,
                    index: sub_index,
                    assoc_binds: sub_assoc_binds,
                    post: AliasDynPost {
                        has_b_names: !b_names.is_empty(),
                        extra_obj_lt,
                        extra_has_static,
                        extra_ambiguous,
                        extra_unique,
                    },
                }));
            }

            // Direct trait bounds (`trait Alias<'a, U> = Gen<&'a u8, U> + Iter<Item = &'a u8> + ...;`):
            // lower and lay the alias's `head::BOUNDS` via `dyn_ty` + `dyn_regions` in `alias_env`,
            // then substitute the alias's lifetime and type parameters into `s.scratch`.
            let mut bound_arg_tys: blaze_own::Vec<lf::TypeId, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            for &row in av.rows(bounds)? {
                let Some((_, seg)) = super::dyn_ty::dyn_bound(av, lf::Bound::decode(row)?, r)? else { continue };
                super::dyn_ty::dyn_arg_types_into(av, &seg, r, &mut |t| bound_arg_tys.push(t))?;
            }
            let mut sub_sink = RegionSink::new_in(alloc);
            sub_sink.default_depth = s.default_depth + 1;
            let mut sub_kids: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::with_capacity_in(bound_arg_tys.len(), alloc);
            for &arg_t in &bound_arg_tys {
                // A const parameter of the alias written as a bare identifier in a bound
                // (`trait Alias<const N: usize = 3> = GenConst<N>;`) parses as `GenericArg::Type`;
                // it has 0 region slots and lowers directly to `PNode::Param(param_idx)`.
                if let Some(nm) = bare_name(av, arg_t)?
                    && let Some(&(_, pidx)) = alias_env.params.iter().find(|(n, _)| *n == Name::of(nm))
                    && alias_env.consts.iter().any(|&(c, _)| c == pidx)
                {
                    sub_sink.frags.push(Frag { start: 0, len: 0, binders: 0 });
                    sub_kids.push(intern(tys, PNode::Param(pidx))?);
                    continue;
                }
                let kt = match alias_builder.ty_regions(tys, &alias_env, av, arg_t, &mut sub_sink) {
                    Ok(t) => t,
                    Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                    Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r))),
                };
                sub_kids.push(kt);
            }
            if let Some(why) = sub_sink.refused() {
                return Ok(Err(why));
            }
            let raw_dyn = match alias_builder.dyn_ty(tys, &alias_env, av, bounds, &sub_kids, r) {
                Ok(t) => t,
                Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r))),
            };
            if let Err(why) = alias_builder.dyn_regions(tys, &alias_env, av, bounds, raw_dyn, &sub_kids, &mut sub_sink, r)? {
                return Ok(Err(why));
            }
            let def_slots = &sub_sink.scratch;
            let mut visits: blaze_own::Vec<(PTy, PNode, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            let sub_arity = HeadsArity { heads: self.heads, opaques: &sub_sink.opaques, names: self.resolver.names };
            soft!(each_slot_node(tys.view(), raw_dyn, &sub_arity, alloc, &mut |t, node, own| {
                visits.push((t, node, own));
                Ok(())
            }));
            s.scratch.clear();
            let mut binder_at: blaze_own::Vec<u32, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            let mut elided_map: blaze_own::Vec<(u32, SlotKind), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            let mut next = 0usize;
            let mut db = 0u32;
            for (vi, &(_, node, own)) in visits.iter().enumerate() {
                if let PNode::Param(pv) = node
                    && let Some(off) = pv.checked_sub(1)
                    && let Ok(pi) = usize::try_from(off)
                    && let Some(&(pstart, plen, pb)) = all_index.get(pi)
                {
                    let Ok(pst) = usize::try_from(pstart) else { return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r))) };
                    let Ok(pen) = usize::try_from(pstart.saturating_add(plen)) else { return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r))) };
                    let Some(psl) = all_pool.get(pst..pen) else { return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r))) };
                    for &slot in psl {
                        s.scratch.push(match slot.kind {
                            SlotKind::ObjectDefault(_) => Slot::quiet(SlotKind::Static),
                            _ => slot.shifted(db),
                        });
                    }
                    db += pb;
                    continue;
                }
                if own > 0 {
                    let Ok(own_u) = usize::try_from(own) else { return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r))) };
                    let Some(col) = def_slots.get(next..next.saturating_add(own_u)) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                    };
                    next += own_u;
                    for &dslot in col {
                        let mapped = match dslot.kind {
                            SlotKind::Static => Slot { kind: SlotKind::Static, counts: dslot.counts },
                            SlotKind::Named(name, _) => {
                                let Some(pos) = lt_names.iter().position(|&n| n == name) else {
                                    return Ok(Err(RegionRefusal::Undeclared { at: r }));
                                };
                                let Some(&k) = given_lts.get(pos) else {
                                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                                };
                                Slot { kind: k, counts: dslot.counts }
                            }
                            SlotKind::Late { binder, var } => match usize::try_from(binder).ok().and_then(|bi| binder_at.get(bi)) {
                                Some(&b) => Slot { kind: SlotKind::Late { binder: b, var }, counts: dslot.counts },
                                None => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r))),
                            },
                            SlotKind::ObjectDefault(_) if vi == 0 => Slot::quiet(SlotKind::ObjectDefault(r)),
                            SlotKind::ObjectDefault(_) => Slot::quiet(SlotKind::Static),
                            SlotKind::Elided(id) => {
                                let kind = match elided_map.iter().find(|&&(eid, _)| eid == id) {
                                    Some(&(_, k)) => k,
                                    None => {
                                        let k = s.new_elided(r);
                                        elided_map.push((id, k));
                                        k
                                    }
                                };
                                Slot { kind, counts: dslot.counts }
                            }
                            SlotKind::Bound(_) | SlotKind::Given(_) => {
                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                            }
                        };
                        s.scratch.push(mapped);
                    }
                }
                if is_binder(node) {
                    binder_at.push(db);
                    db += 1;
                }
            }
            if assoc_binds.is_empty() {
                if next != def_slots.len() || u32::try_from(s.scratch.len()).ok() != Some(whole.slots) || db != whole.binders {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                }
            } else {
                // Use-site associated bindings (`&IterAlias<Item = &'a u8>`): `made` contains the
                // alias's expanded predicates plus the use-site `ExProj` predicates sorted in
                // canonical order. Reassemble `s.scratch` in `made`'s predicate order.
                if next != def_slots.len() {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                }
                let mut raw_laid: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::with_capacity_in(s.scratch.len(), alloc);
                raw_laid.extend_from_slice(&s.scratch);
                let Some(&root_slot) = raw_laid.first() else {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                };
                // Map each predicate of `raw_dyn` (substituted with `all_args`) to its slice in `raw_laid`.
                let mut subst_all: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::with_capacity_in(all_args.len() + 1, alloc);
                subst_all.push(intern(tys, PNode::Param(u32::MAX))?);
                subst_all.extend_from_slice(&all_args);
                let mut raw_preds: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                if let Ok(PNode::Dynamic(pl)) = tys.view().node(raw_dyn)
                    && let Ok(sl) = tys.view().list(pl)
                {
                    raw_preds.extend_from_slice(sl);
                }
                let mut pred_slices: blaze_own::Vec<(PTy, usize, usize, u32), Alloc<'s>> = blaze_own::Vec::with_capacity_in(raw_preds.len(), alloc);
                let mut r_off = 1usize;
                {
                    let arity_now = HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names };
                    for &rp in &raw_preds {
                        let sp = subst(tys, rp, &subst_all)?;
                        let psub = soft!(subtree(tys.view(), sp, &arity_now, alloc));
                        let Ok(plen) = usize::try_from(psub.slots) else {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                        };
                        pred_slices.push((sp, r_off, r_off.saturating_add(plen), psub.binders));
                        r_off = r_off.saturating_add(plen);
                    }
                }
                // Populate trait & supertrait lifetime slots from the alias's `super_lifetimes`
                // so supertrait `ExProj` bindings (`&SubAlias<'a, A = &'a u8>`) find owner slots.
                let mut owner_lts: blaze_own::Vec<(ItemKey, u32, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                let mut owner_pool: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                if let Some(ah) = self.heads.trait_alias(alias_key) {
                    for &(k, e_start, e_end) in ah.super_lifetimes.edges.iter() {
                        let Ok(edge_start) = u32::try_from(owner_pool.len()) else { continue };
                        let mut laid_n = 0u32;
                        let mut ok_edge = true;
                        if let (Ok(es), Ok(ee)) = (usize::try_from(e_start), usize::try_from(e_end))
                            && let Some(edge_args) = ah.super_lifetimes.args.get(es..ee)
                        {
                            for &ea in edge_args {
                                match ea {
                                    SuperLifetime::Static => {
                                        owner_pool.push(Slot::quiet(SlotKind::Static));
                                        laid_n = laid_n.saturating_add(1);
                                    }
                                    SuperLifetime::Param(pi) => {
                                        if let Some(&gk) = usize::try_from(pi).ok().and_then(|i| given_lts.get(i)) {
                                            owner_pool.push(Slot::quiet(gk));
                                            laid_n = laid_n.saturating_add(1);
                                        } else {
                                            ok_edge = false;
                                        }
                                    }
                                    SuperLifetime::Other => ok_edge = false,
                                }
                            }
                        }
                        let ck = self.heads.checker(k);
                        if ok_edge && declared_lifetimes(self.heads, self.resolver.names, ck, Declared::Trait) == Some(laid_n) {
                            if laid_n > 0 {
                                owner_lts.push((ck, edge_start, laid_n));
                            }
                            if let Err(why) = self.populate_supertrait_lts(k, edge_start, laid_n, &mut owner_lts, &mut owner_pool, s, r)? {
                                return Ok(Err(why));
                            }
                        }
                    }
                }
                let mut made_preds: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                if let Ok(PNode::Dynamic(pl)) = tys.view().node(made)
                    && let Ok(sl) = tys.view().list(pl)
                {
                    made_preds.extend_from_slice(sl);
                }
                s.scratch.clear();
                s.scratch.push(root_slot);
                // `PNode::Dynamic` itself is binder 0 (`is_binder(PNode::Dynamic(_)) == true`).
                db = 1;
                for &mp in &made_preds {
                    if let Some(&(_, st, en, pb)) = pred_slices.iter().find(|&&(sp, _, _, _)| sp == mp) {
                        let Some(sl) = raw_laid.get(st..en) else {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                        };
                        let extra = db.saturating_sub(1);
                        for &slot in sl {
                            s.scratch.push(slot.shifted(extra));
                        }
                        db += pb;
                    } else if let Ok(PNode::ExProj { trait_: owner, name, term, .. }) = tys.view().node(mp)
                        && let Some(&(_, _, (bstart, blen, bb))) = assoc_binds.iter().find(|&&(bn, bt, _)| bn == name && bt == term)
                    {
                        let own_n = declared_lifetimes(self.heads, self.resolver.names, owner, Declared::Trait).unwrap_or(0);
                        if own_n > 0 {
                            let node = PNode::ExProj { trait_: owner, name, args: List::EMPTY, term };
                            if !push_dyn_node_own_slots(node, own_n, &owner_lts, &owner_pool, &mut s.scratch) {
                                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                            }
                        }
                        let Ok(bst) = usize::try_from(bstart) else {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                        };
                        let Ok(ben) = usize::try_from(bstart.saturating_add(blen)) else {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                        };
                        let Some(bsl) = pool.get(bst..ben) else {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                        };
                        for &slot in bsl {
                            s.scratch.push(match slot.kind {
                                SlotKind::ObjectDefault(_) => Slot::quiet(SlotKind::Static),
                                _ => slot.shifted(db),
                            });
                        }
                        db += bb;
                    } else {
                        let arity_now = HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names };
                        let psub = soft!(subtree(tys.view(), mp, &arity_now, alloc));
                        if psub.slots != 0 {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                        }
                        db += psub.binders;
                    }
                }
                if u32::try_from(s.scratch.len()).ok() != Some(whole.slots) || db != whole.binders {
                    return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                }
            }
            if let Some(first) = s.scratch.first_mut()
                && extra_obj_lt.is_none()
            {
                if extra_has_static {
                    *first = Slot::quiet(SlotKind::Static);
                } else if extra_ambiguous {
                    if !matches!(first.kind, SlotKind::Static) {
                        return Ok(Err(RegionRefusal::ObjectDefault { at: r }));
                    }
                } else if let Some(u) = extra_unique {
                    match first.kind {
                        SlotKind::ObjectDefault(_) => *first = Slot::quiet(u),
                        SlotKind::Static => {}
                        prev if same_region(prev, u) => {}
                        _ => return Ok(Err(RegionRefusal::ObjectDefault { at: r })),
                    }
                }
            }
            Ok(Ok(AliasDynStep::Done(db)))
            });
            let step = match lowered {
                Ok(Ok(step)) => step,
                Ok(Err(why)) => return Err(why),
                Err(_) => return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r))),
            };
            match step {
                Ok(AliasDynStep::Next { alias_key, use_lifetimes, args, pool, index, assoc_binds, post }) => {
                    post_stack.push(post);
                    cur_alias_key = alias_key;
                    cur_lts = use_lifetimes;
                    cur_args = args;
                    cur_pool = pool;
                    cur_index = index;
                    cur_assoc_binds = assoc_binds;
                    cur_depth = cur_depth.saturating_add(1);
                }
                Ok(AliasDynStep::Done(binders)) => {
                    while let Some(post) = post_stack.pop() {
                        if post.has_b_names {
                            let mut base = 0u32;
                            for slot in s.scratch.iter() {
                                if let SlotKind::Late { binder: 0, var } = slot.kind {
                                    base = base.max(var.saturating_add(1));
                                }
                            }
                            for slot in s.scratch.iter_mut() {
                                if let SlotKind::Bound(var) = slot.kind {
                                    slot.kind = SlotKind::Late { binder: 0, var: base.saturating_add(var) };
                                }
                            }
                        }
                        if let Some(first) = s.scratch.first_mut() {
                            if let Some(explicit) = post.extra_obj_lt {
                                *first = explicit;
                            } else if post.extra_has_static {
                                *first = Slot::quiet(SlotKind::Static);
                            } else if post.extra_ambiguous {
                                if !matches!(first.kind, SlotKind::Static) {
                                    return Ok(Err(RegionRefusal::ObjectDefault { at: r }));
                                }
                            } else if let Some(u) = post.extra_unique {
                                match first.kind {
                                    SlotKind::ObjectDefault(_) => *first = Slot::quiet(u),
                                    SlotKind::Static => {}
                                    prev if same_region(prev, u) => {}
                                    _ => return Ok(Err(RegionRefusal::ObjectDefault { at: r })),
                                }
                            }
                        }
                    }
                    return Ok(Ok(binders));
                }
                Err(why) => return Ok(Err(why)),
            }
        }
    }

    /// Lays the region slots of a Rust 2015/2018 bare trait object path (`&Error`, `Box<Error>`,
    /// `&Store<'a>`, `&Iterator<Item = &'a u8>`, `&Fn(&str) -> &str`, `&Alias<'a>`, `&DefaultTr`),
    /// matching [`Self::dyn_regions`] with no explicit lifetime bound.
    #[expect(clippy::too_many_arguments, reason = "one bare trait object path: target, gathered slots/arguments, whole counts, and sink")]
    fn bare_dyn_regions<'s>(
        &self,
        tys: &mut PortableTys<'_>,
        v: &View<'_>,
        p: PathIdx,
        (made, args): (PTy, &[PTy]),
        found_key: LibKey,
        is_alias: bool,
        (lifetimes, pool, index): (&[SlotKind], &[Slot], &[(u32, u32, u32)]),
        whole: Sub,
        s: &mut RegionSink<'s>,
        r: Range,
    ) -> Result<Laid, Why> {
        let alloc = s.alloc;
        let (_, _, segs, _) = path_texts_of(v, p, true)?;
        let Some(last) = segs.last() else {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
        };
        if is_alias {
            let mut pos_args: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            let mut pos_index: blaze_own::Vec<(u32, u32, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            let mut assoc_binds: blaze_own::Vec<(Name, PTy, (u32, u32, u32)), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            if !last.parenthesized && last.has_args {
                let mut at = 0usize;
                for &row in v.rows(last.args)? {
                    match lf::GenericArg::decode(row)? {
                        lf::GenericArg::Type(_) | lf::GenericArg::Const(_) => {
                            if let (Some(&arg), Some(&entry)) = (args.get(at), index.get(at)) {
                                pos_args.push(arg);
                                pos_index.push(entry);
                            }
                            at += 1;
                        }
                        lf::GenericArg::AssocType { name, args: a_args, .. } if a_args.len == 0 => {
                            if let (Some(&arg), Some(&entry)) = (args.get(at), index.get(at)) {
                                assoc_binds.push((Name(v.name(name)?), arg, entry));
                            }
                            at += 1;
                        }
                        _ => {}
                    }
                }
            } else {
                pos_args.extend_from_slice(args);
                pos_index.extend_from_slice(index);
            }
            return self.lay_trait_alias_dyn_regions(tys, made, found_key, lifetimes, (&pos_args, pool, &pos_index), &assoc_binds, whole, s, r, 0);
        }
        let mut trait_lts: blaze_own::Vec<(ItemKey, u32, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
        let mut lt_pool: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
        let checker_key = self.heads.checker(found_key);
        let written_lts = u32::try_from(lifetimes.len()).unwrap_or(u32::MAX);
        let mut own_lt_len = 0usize;
        if let Some(want) = declared_lifetimes(self.heads, self.resolver.names, checker_key, Declared::Trait) {
            if written_lts == 0 {
                for _ in 0..want {
                    lt_pool.push(Slot::written(s.new_elided(r)));
                }
                if want > 0 {
                    trait_lts.push((checker_key, 0, want));
                }
            } else if written_lts == want {
                for &k in lifetimes {
                    lt_pool.push(Slot::written(k));
                }
                trait_lts.push((checker_key, 0, written_lts));
            } else {
                return Ok(Err(RegionRefusal::LifetimeArgs { at: r, want, got: written_lts }));
            }
            own_lt_len = lt_pool.len();
            if let Err(why) = self.populate_supertrait_lts(found_key, 0, want, &mut trait_lts, &mut lt_pool, s, r)? {
                return Ok(Err(why));
            }
        } else if written_lts > 0 {
            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DeclaredCount, r)));
        }
        let own_lts_slice = lt_pool.get(..own_lt_len).unwrap_or(&[]);
        let own = match self.trait_object_default_bound(found_key, own_lts_slice, s, r)? {
            Ok(TraitDefaultBound::Unique(kind)) => Slot::quiet(kind),
            Ok(TraitDefaultBound::None) => Slot::quiet(SlotKind::ObjectDefault(r)),
            Ok(TraitDefaultBound::Ambiguous) => return Ok(Err(RegionRefusal::ObjectDefault { at: r })),
            Err(why) => return Ok(Err(why)),
        };
        // Lay omitted defaulted type arguments of the trait (`trait Tr<T = &'static u8> {}` written
        // as `&Tr`): substitute any trait lifetimes and earlier arguments via `lay_default_arg`.
        let mut def_pool: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
        let mut def_index: blaze_own::Vec<(u32, u32, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
        let mut def_tys: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
        if !is_alias
            && !last.parenthesized
            && let Some((g, _)) = self.heads.trait_head(found_key)
        {
            let want_pos = usize::try_from(g).unwrap_or(0).saturating_sub(1);
            let mut pos_index: blaze_own::Vec<(u32, u32, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            if last.has_args {
                let mut at = 0usize;
                for &row in v.rows(last.args)? {
                    match lf::GenericArg::decode(row)? {
                        lf::GenericArg::Type(_) | lf::GenericArg::Const(_) => {
                            if let Some(&entry) = index.get(at) {
                                pos_index.push(entry);
                            }
                            at += 1;
                        }
                        lf::GenericArg::AssocType { args: a_args, .. } if a_args.len == 0 => {
                            at += 1;
                        }
                        _ => {}
                    }
                }
            }
            if pos_index.len() < want_pos {
                let checker_key = self.heads.checker(found_key);
                let mut ex_args_buf: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                if let Ok(PNode::Dynamic(preds_list)) = tys.view().node(made)
                    && let Ok(preds) = tys.view().list(preds_list)
                {
                    for &pred in preds {
                        if let Ok(PNode::ExTrait(k, a_list)) = tys.view().node(pred)
                            && k == checker_key
                            && let Ok(sl) = tys.view().list(a_list)
                        {
                            ex_args_buf.extend_from_slice(sl);
                            break;
                        }
                    }
                }
                let own_lt_slice = lt_pool.get(..own_lt_len).unwrap_or(&[]);
                let mut trait_lt_kinds: blaze_own::Vec<SlotKind, Alloc<'s>> = blaze_own::Vec::with_capacity_in(own_lt_slice.len(), alloc);
                for &slot in own_lt_slice {
                    trait_lt_kinds.push(slot.kind);
                }
                let trait_want = u32::try_from(trait_lt_kinds.len()).unwrap_or(u32::MAX);
                let mut item_lts = DefaultLifetimes::Fixed(&trait_lt_kinds);
                let mut added: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                for i in pos_index.len()..want_pos {
                    let Some(&a) = ex_args_buf.get(i) else {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
                    };
                    let sub = soft!(subtree(
                        tys.view(),
                        a,
                        &HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names },
                        alloc,
                    ));
                    if sub.slots != 0 {
                        let raw_default = self.heads.trait_head(found_key).and_then(|(_, ds)| ds.get(i).copied().flatten());
                        added.clear();
                        if let Err(why) = self.lay_default_arg(
                            tys,
                            found_key,
                            i,
                            raw_default,
                            sub,
                            DefaultOwner::Trait { want: trait_want },
                            &mut item_lts,
                            (pool, &pos_index),
                            (&def_pool, &def_index),
                            s,
                            r,
                            &mut added,
                        )? {
                            return Ok(Err(why));
                        }
                        let Ok(dstart) = u32::try_from(def_pool.len()) else {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                        };
                        let Ok(dlen) = u32::try_from(added.len()) else {
                            return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::TooMany, r)));
                        };
                        def_pool.extend_from_slice(&added);
                        def_index.push((dstart, dlen, sub.binders));
                    } else {
                        def_index.push((0, 0, sub.binders));
                    }
                    def_tys.push(a);
                }
            }
        }
        let arity = HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names };
        let has_def_slots = def_index.iter().any(|&(_, len, _)| len > 0);
        s.scratch.clear();
        let kid_slots: u32 = index.iter().map(|&(_, len, _)| len).sum();
        let binders = if kid_slots == 0 && !has_def_slots && whole.slots == 1 {
            s.scratch.push(own);
            whole.binders
        } else if kid_slots == 0 && !has_def_slots {
            let mut real: blaze_own::Vec<(PTy, PNode, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            soft!(each_slot_node(tys.view(), made, &arity, alloc, &mut |t, node, own_n| {
                real.push((t, node, own_n));
                Ok(())
            }));
            let mut binders = 0u32;
            for (ri, &(_, rn, rown)) in real.iter().enumerate() {
                if rown > 0 {
                    if ri == 0 && rown == 1 {
                        s.scratch.push(own);
                    } else if !push_dyn_node_own_slots(rn, rown, &trait_lts, &lt_pool, &mut s.scratch) {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                    }
                }
                binders += u32::from(is_binder(rn));
            }
            if u32::try_from(s.scratch.len()).ok() != Some(whole.slots) || binders != whole.binders {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r)));
            }
            binders
        } else {
            let out_of_step = RegionRefusal::not_laid(NotLaidWhy::OutOfStep, r);
            let Ok(pool_base) = u32::try_from(pool.len()) else { return Ok(Err(out_of_step)) };
            let mut local_pool: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::with_capacity_in(pool.len() + def_pool.len(), alloc);
            local_pool.extend_from_slice(pool);
            local_pool.extend_from_slice(&def_pool);
            let mut all_index: blaze_own::Vec<(u32, u32, u32), Alloc<'s>> = blaze_own::Vec::with_capacity_in(index.len() + def_index.len(), alloc);
            all_index.extend_from_slice(index);
            for &(dstart, dlen, db) in &def_index {
                all_index.push((pool_base.saturating_add(dstart), dlen, db));
            }
            let mut all_args: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::with_capacity_in(args.len() + def_tys.len(), alloc);
            all_args.extend_from_slice(args);
            all_args.extend_from_slice(&def_tys);
            if last.parenthesized {
                let output = last.ty.is_some();
                let inputs = index.len().saturating_sub(usize::from(output));
                let mut ids: blaze_own::Vec<u32, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                let mut input_slots: blaze_own::Vec<Slot, Alloc<'s>> = blaze_own::Vec::new_in(alloc);
                for (k, &(start, len, _)) in index.iter().enumerate() {
                    let Ok(st) = usize::try_from(start) else { return Ok(Err(out_of_step)) };
                    let Ok(en) = usize::try_from(start.saturating_add(len)) else { return Ok(Err(out_of_step)) };
                    let Some(slice) = local_pool.get_mut(st..en) else { return Ok(Err(out_of_step)) };
                    if k < inputs {
                        for slot in slice.iter_mut() {
                            if let SlotKind::Elided(id) = slot.kind {
                                let var = match ids.iter().position(|&x| x == id) {
                                    Some(pos) => u32::try_from(pos).unwrap_or(u32::MAX),
                                    None => {
                                        let pos = u32::try_from(ids.len()).unwrap_or(u32::MAX);
                                        ids.push(id);
                                        pos
                                    }
                                };
                                slot.kind = SlotKind::Bound(var);
                            }
                            input_slots.push(*slot);
                        }
                    } else if output {
                        let one = used_lifetimes(&input_slots, object_local);
                        for slot in slice.iter_mut() {
                            if let SlotKind::Elided(_) = slot.kind {
                                match one {
                                    Used::One(kind) => slot.kind = kind,
                                    Used::None | Used::Many => {
                                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::FnPtrElision, r)));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let mut shadow_written: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::with_capacity_in(index.len(), alloc);
            for k in 0..index.len() {
                let Ok(ku) = u32::try_from(k) else { return Ok(Err(out_of_step)) };
                shadow_written.push(intern(tys, PNode::Param(SHADOW_BASE.saturating_add(ku)))?);
            }
            let mut shadow_defaults: blaze_own::Vec<PTy, Alloc<'s>> = blaze_own::Vec::with_capacity_in(def_index.len(), alloc);
            for k in index.len()..all_index.len() {
                let Ok(ku) = u32::try_from(k) else { return Ok(Err(out_of_step)) };
                shadow_defaults.push(intern(tys, PNode::Param(SHADOW_BASE.saturating_add(ku)))?);
            }
            let Ok(shadow) = self.bare_dyn_ty_with_extra(tys, v, p, &segs, found_key, is_alias, &shadow_written, &shadow_defaults, r) else {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
            };
            let arity = HeadsArity { heads: self.heads, opaques: &s.opaques, names: self.resolver.names };
            let mut real: blaze_own::Vec<(PTy, PNode, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            soft!(each_slot_node(tys.view(), made, &arity, alloc, &mut |t, node, own_n| {
                real.push((t, node, own_n));
                Ok(())
            }));
            let mut seen: blaze_own::Vec<(PTy, PNode, u32), Alloc<'s>> = blaze_own::Vec::new_in(alloc);
            soft!(each_slot_node(tys.view(), shadow, &arity, alloc, &mut |t, node, own_n| {
                seen.push((t, node, own_n));
                Ok(())
            }));
            let mut ri = 0usize;
            let mut binders = 0u32;
            for (si, &(_, sn, sown)) in seen.iter().enumerate() {
                if let PNode::Param(pv) = sn
                    && let Some(off) = pv.checked_sub(SHADOW_BASE)
                    && let Ok(k) = usize::try_from(off)
                    && let Some(&(start, len, kb)) = all_index.get(k)
                {
                    let Some(&(rt, _, _)) = real.get(ri) else { return Ok(Err(out_of_step)) };
                    let Some(&ak) = all_args.get(k) else { return Ok(Err(out_of_step)) };
                    if rt != ak {
                        return Ok(Err(out_of_step));
                    }
                    let Ok(st) = usize::try_from(start) else { return Ok(Err(out_of_step)) };
                    let Ok(en) = usize::try_from(start.saturating_add(len)) else { return Ok(Err(out_of_step)) };
                    let Some(slice) = local_pool.get(st..en) else { return Ok(Err(out_of_step)) };
                    for &slot in slice {
                        s.scratch.push(match slot.kind {
                            SlotKind::ObjectDefault(_) => Slot::quiet(SlotKind::Static),
                            _ => slot.shifted(binders),
                        });
                    }
                    binders += kb;
                    let Ok(sub_nodes) = usize::try_from(soft!(subtree(tys.view(), ak, &arity, alloc)).nodes) else {
                        return Ok(Err(out_of_step));
                    };
                    ri += sub_nodes;
                    continue;
                }
                let Some(&(_, rn, rown)) = real.get(ri) else { return Ok(Err(out_of_step)) };
                if core::mem::discriminant(&rn) != core::mem::discriminant(&sn) || rown != sown {
                    return Ok(Err(out_of_step));
                }
                if rown > 0 {
                    if si == 0 && rown == 1 {
                        s.scratch.push(own);
                    } else if !push_dyn_node_own_slots(rn, rown, &trait_lts, &lt_pool, &mut s.scratch) {
                        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::DynRegion, r)));
                    }
                }
                binders += u32::from(is_binder(rn));
                ri += 1;
            }
            if ri != real.len() || u32::try_from(s.scratch.len()).ok() != Some(whole.slots) || binders != whole.binders {
                return Ok(Err(out_of_step));
            }
            binders
        };
        for slot in s.scratch.iter_mut() {
            if let SlotKind::Bound(var) = slot.kind {
                slot.kind = SlotKind::Late { binder: 0, var };
            }
        }
        Ok(Ok(binders))
    }
}

/// Post-processing frame for one nested trait alias step in `Builder::lay_trait_alias_dyn_regions`.
struct AliasDynPost {
    has_b_names: bool,
    extra_obj_lt: Option<Slot>,
    extra_has_static: bool,
    extra_ambiguous: bool,
    extra_unique: Option<SlotKind>,
}

/// One iteration result of `Builder::lay_trait_alias_dyn_regions`.
enum AliasDynStep<'s> {
    Done(u32),
    Next {
        alias_key: LibKey,
        use_lifetimes: blaze_own::Vec<SlotKind, Alloc<'s>>,
        args: blaze_own::Vec<PTy, Alloc<'s>>,
        pool: blaze_own::Vec<Slot, Alloc<'s>>,
        index: blaze_own::Vec<(u32, u32, u32), Alloc<'s>>,
        assoc_binds: blaze_own::Vec<(Name, PTy, (u32, u32, u32)), Alloc<'s>>,
        post: AliasDynPost,
    },
}

/// A declared default argument on a trait alias generic parameter (either a type or a const).
#[derive(Clone, Copy)]
enum AliasDefault {
    Type(lf::TypeId),
    Const(u32, lf::ConstIdx),
}

/// Result of deriving a trait's declared `Self:` lifetime bound for default trait object
/// lifetimes (the Reference, "Default trait object lifetimes").
enum TraitDefaultBound {
    None,
    Unique(SlotKind),
    Ambiguous,
}

/// Whether any segment in `segs` carries an associated item constraint (`Item = T` or
/// `Item: Bound`). Allowed only on trait / trait-alias object paths, never on nominal types,
/// type aliases, `Self`, or type parameters (E0229).
fn has_assoc_arg(v: &View<'_>, segs: &[lf::Segment]) -> Result<bool, Why> {
    for &s in segs {
        if !s.has_args {
            continue;
        }
        for &row in v.rows(s.args)? {
            if matches!(
                lf::GenericArg::decode(row)?,
                lf::GenericArg::AssocType { .. } | lf::GenericArg::AssocBound { .. }
            ) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Pushes the own lifetime slots of an existential trait or projection node inside a `dyn` type
/// (`ExTrait(Tr, ..)` or `ExProj { trait_: Tr, .. }`), from the lifetimes written (or elided) on
/// the `dyn` bound for `Tr`. For an `ExProj` of the same trait (non-GAT: `rown == trait_lts.len`),
/// the trait's lifetimes are pushed `quiet` so output elision counts them once. Returns `false`
/// when `node`'s own slots are not accounted for (caller refuses `NotLaidWhy::DynRegion`).
fn push_dyn_node_own_slots(
    node: PNode,
    rown: u32,
    trait_lts: &[(ItemKey, u32, u32)],
    lt_pool: &[Slot],
    out: &mut blaze_own::Vec<Slot, Alloc<'_>>,
) -> bool {
    let (key, quiet) = match node {
        PNode::ExTrait(k, _) => (k, false),
        PNode::ExProj { trait_: k, .. } => (k, true),
        _ => return false,
    };
    let Some(&(_, start, len)) = trait_lts.iter().find(|&&(k, _, _)| k == key) else {
        return false;
    };
    if len != rown {
        return false;
    }
    let (Ok(st), Ok(ln)) = (usize::try_from(start), usize::try_from(len)) else {
        return false;
    };
    let Some(slice) = st.checked_add(ln).and_then(|end| lt_pool.get(st..end)) else {
        return false;
    };
    for &slot in slice {
        out.push(if quiet { Slot::quiet(slot.kind) } else { slot });
    }
    true
}

/// Whether a defaulted type argument belongs to an ADT, a type alias, or a trait (in all, a
/// `dyn Trait` object default resolves to `'static` when the item declares no lifetime).
#[derive(Clone, Copy)]
enum DefaultOwner {
    Adt { want: u32 },
    Alias { want: Option<u32> },
    Trait { want: u32 },
}

/// How an item's declared lifetimes are supplied when substituting into a defaulted type argument.
enum DefaultLifetimes<'l, 's> {
    /// An ADT's declared lifetime arguments `0..want`, already laid (written or elided).
    Fixed(&'l [SlotKind]),
    /// A type alias's declared lifetime arguments, lazily elided on first use when omitted.
    Lazy(&'l mut Vec<Option<SlotKind>, Alloc<'s>>),
}

impl<'s> DefaultLifetimes<'_, 's> {
    fn get(&mut self, idx: usize, s: &mut RegionSink<'s>, r: Range) -> Option<SlotKind> {
        match self {
            Self::Fixed(slice) => slice.get(idx).copied(),
            Self::Lazy(slots) => match slots.get_mut(idx)? {
                Some(k) => Some(*k),
                slot @ None => {
                    let k = s.new_elided(r);
                    *slot = Some(k);
                    Some(k)
                }
            },
        }
    }
}

/// Looks up the already-laid slots and binder count of generic argument `pi`, checking written
/// arguments first and then earlier defaulted arguments.
fn arg_slots<'p>(
    pi: usize,
    written_pool: &'p [Slot],
    written_index: &[(u32, u32, u32)],
    def_pool: &'p [Slot],
    def_index: &[(u32, u32, u32)],
) -> Option<(&'p [Slot], u32)> {
    let (pool, &(start, len, b)) = if let Some(entry) = written_index.get(pi) {
        (written_pool, entry)
    } else {
        let di = pi.checked_sub(written_index.len())?;
        (def_pool, def_index.get(di)?)
    };
    let st = usize::try_from(start).ok()?;
    let ln = usize::try_from(len).ok()?;
    let end = st.checked_add(ln)?;
    Some((pool.get(st..end)?, b))
}

#[cfg(test)]
mod ptr_width_tests {
    use super::*;
    use blaze_ids::ContentId;
    use blaze_own::AVec;

    #[derive(Clone, Copy)]
    struct CfgOnlySource<'n> {
        krate: Option<usize>,
        atoms: Option<&'n [(&'n str, Option<&'n str>)]>,
    }

    impl<'n> NamesSource<'n> for CfgOnlySource<'n> {
        fn solved(self) -> Option<&'n blaze_scope::resolve::Names<'n>> { None }
        fn head_lowerings(self) -> Option<&'n super::super::heads::HeadLowerings> { None }
        fn gap(self, _question: &'static str, _key: Option<LibKey>) {}
        fn crate_count(self) -> usize { usize::from(self.krate.is_some()) }
        fn crate_of(self, _key: LibKey) -> Option<usize> { self.krate }
        fn crate_key(self, _krate: usize) -> Option<blaze_id::CrateItemKey> { None }
        fn root_module(self, _krate: usize) -> Option<LibKey> { None }
        fn edition_of(self, _krate: usize) -> Option<blaze_surface::Edition> { None }
        fn cfg_atoms(self, _krate: usize) -> Option<&'n [(&'n str, Option<&'n str>)]> { self.atoms }
        fn proc_macro(self, _krate: usize) -> Option<bool> { Some(false) }
        fn std_is(self, _krate: usize) -> Option<blaze_unit_graph::StdIs> { None }
        fn root_attrs(self, _krate: usize) -> Option<blaze_scope::graph::RootAttrs> { None }
        fn has_feature(self, _krate: usize, _name: &str) -> Option<bool> { None }
        fn linked<'a>(self, _roots: &[usize], _alloc: Alloc<'a>) -> Option<Result<super::super::linked::Linked<'a>, super::super::linked::RootsError>> { None }
        fn sysroot_std(self) -> Option<Option<usize>> { None }
        fn may_write_incoherent(self, _krate: usize) -> Option<bool> { None }
        fn walk(self, module: LibKey, _ns: blaze_surface::Namespace, _written: blaze_scope::names::Written<'_, &str>) -> Result<blaze_scope::names::Resolved, blaze_scope::names::PathError> {
            Err(blaze_scope::names::PathError { module, at: 0, why: blaze_scope::names::Why::NotRead { module, layer: blaze_scope::names::Unheld::Step } })
        }
        fn written_module(self, module: LibKey, _n: u8) -> Result<LibKey, blaze_scope::names::PathError> {
            Err(blaze_scope::names::PathError { module, at: 0, why: blaze_scope::names::Why::NotRead { module, layer: blaze_scope::names::Unheld::Step } })
        }
        fn traits_in_scope<'b>(self, module: LibKey, _alloc: Alloc<'b>) -> Result<AVec<'b, LibKey>, blaze_scope::names::PathError> {
            Err(blaze_scope::names::PathError { module, at: 0, why: blaze_scope::names::Why::NotRead { module, layer: blaze_scope::names::Unheld::Step } })
        }
        fn traits_exported<'b>(self, module: LibKey, _alloc: Alloc<'b>) -> Result<AVec<'b, LibKey>, blaze_scope::names::PathError> {
            Err(blaze_scope::names::PathError { module, at: 0, why: blaze_scope::names::Why::NotRead { module, layer: blaze_scope::names::Unheld::Step } })
        }
        fn variants(self, _owner: LibKey) -> Option<super::super::source::VariantRows<'n>> { None }
        fn variant_of(self, _variant: LibKey) -> Option<(LibKey, u32)> { None }
        fn has_text(self, _key: LibKey) -> bool { false }
        fn text_of(self, _key: LibKey) -> Option<blaze_scope::task::ItemText<'n>> { None }
        fn name_of(self, _key: LibKey) -> Option<&'n str> { None }
        fn home_of(self, _key: LibKey) -> Option<LibKey> { None }
        fn place_of(self, _key: LibKey) -> Option<blaze_scope::resolve::Place> { None }
        fn item_start(self, _key: LibKey) -> Option<(blaze_scope::resolve::At, u32)> { None }
        fn file_paths(self, _krate: usize, _file: u32) -> Option<super::super::source::FilePaths<'n>> { None }
        fn file_text(self, _krate: usize, _file: u32) -> Option<&'n str> { None }
        fn call_origin(self, _krate: usize, call: LibKey) -> Result<super::super::expansion_origin::CallOrigin<'n>, super::super::expansion_origin::Unmapped> {
            Err(super::super::expansion_origin::Unmapped::NotServed { call })
        }
        fn member_beginning(self, _krate: usize, _item: blaze_typeck::ItemKey) -> Result<Option<LibKey>, crate::seam::PositionGap> { Ok(None) }
        fn enclosing_keys(self, _krate: usize, _visit: impl FnMut(LibKey) -> Result<(), crate::seam::PositionGap>) -> Result<(), crate::seam::PositionGap> { Ok(()) }
        fn expansion_text(self, _krate: usize, _call: LibKey) -> Option<&'n str> { None }
        fn text_marks(self, _krate: usize, _call: LibKey) -> Result<Option<blaze_scope::crate_roots::TextMarks<'n>>, super::super::item_site::MarksLost> { Ok(None) }
        fn extern_abi(self, _key: LibKey) -> Option<Option<&'n str>> { None }
        fn is_wasm_import(self, _key: LibKey) -> Option<bool> { None }
        fn attr_value(self, _key: LibKey, _attr: &'static str) -> Option<Option<blaze_scope::attr_values::RowValue<'n>>> { None }
        fn in_sans_io_scope(self, _key: LibKey, _module: LibKey) -> bool { false }
        fn kind_of_item(self, _key: LibKey) -> Option<blaze_surface::ItemKind> { None }
        fn detail_of(self, _key: LibKey) -> Option<blaze_surface::ItemDetail> { None }
        fn is_ctor(self, _key: LibKey) -> bool { false }
        fn each_member(self, _owner: LibKey, _each: &mut dyn FnMut(super::super::source::MemberOf<'n>)) -> bool { false }
        fn member_owner(self, _member: LibKey) -> Option<(LibKey, &'n str)> { None }
        fn each_impl(self, _krate: usize, _each: &mut dyn FnMut(super::super::index::ImplAt<'n>)) {}
        fn each_impl_row(self, _krate: usize, _each: &mut dyn FnMut(blaze_scope::items_record::ImplRow<'n>)) -> bool { false }
        fn impl_with_header(self, _krate: usize, _key: LibKey, _module: LibKey) -> Option<super::super::index::ImplAt<'n>> { None }
        fn each_lang(self, _krate: usize, _each: &mut dyn FnMut(&'n str, LibKey)) {}
        fn macro_body_of(self, module: LibKey, _written: blaze_scope::names::Written<'_, &str>) -> Result<Option<blaze_scope::task::MacroBody<'n>>, blaze_scope::names::PathError> {
            Err(blaze_scope::names::PathError { module, at: 0, why: blaze_scope::names::Why::NotRead { module, layer: blaze_scope::names::Unheld::Step } })
        }
        fn macro_at(self, module: LibKey, _written: blaze_scope::names::Written<'_, &str>) -> Result<super::super::source::MacroAt<'n>, blaze_scope::names::PathError> {
            Err(blaze_scope::names::PathError { module, at: 0, why: blaze_scope::names::Why::NotRead { module, layer: blaze_scope::names::Unheld::Step } })
        }
        fn module_path(self, _module: LibKey) -> Option<&'n str> { None }
        fn failed_call<'b>(self, _call: LibKey, _bump: blaze_own::Bump<'b>) -> Option<blaze_scope::names_record::FailedRow<'b>>
        where
            'n: 'b,
        {
            None
        }
        fn each_item(self, _krate: usize, _each: &mut dyn FnMut(LibKey, LibKey)) -> bool { false }
        fn each_export_candidate(self, _krate: usize, _alloc: Alloc<'_>, _each: &mut dyn FnMut(LibKey, Option<&'n str>, bool, bool)) -> bool { false }
        fn block_binding(self, _module: LibKey, _ns: blaze_surface::Namespace, _name: &str) -> Option<Option<blaze_surface::Binding>> { None }
        fn binds(self, _module: LibKey, _ns: blaze_surface::Namespace, _name: &'n str) -> bool { false }
        fn key_beginning(self, _first: [u8; 16]) -> Option<LibKey> { None }
        fn declared_lifetimes(self, _key: LibKey) -> Option<u32> { None }
        fn assoc_lifetimes(self, _trait_: LibKey, _name: blaze_typeck::Name) -> Option<u32> { None }
    }

    #[test]
    fn literal_value_refuses_missing_target_pointer_width_and_enforces_32_bit_overflow() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut tys = PortableTys::new_in(alloc);
        let usize_ty = tys.intern(PNode::Uint(tc::UintTy::Usize)).expect("usize");
        let u8_ty = tys.intern(PNode::Uint(tc::UintTy::U8)).expect("u8");
        let module = LibKey(ContentId([9; 32]));
        let r = Range::new(0, 1);

        // Missing `cfg_atoms` refuses pointer-sized literals instead of silently assuming 64-bit.
        let no_cfg = CfgOnlySource { krate: Some(0), atoms: None };
        let err = literal_value(&mut tys, "0x1_0000_0000usize", usize_ty, no_cfg, module, r).expect_err("missing cfg_atoms must refuse");
        assert!(matches!(err, Why::Unsupported(msg, _) if msg.contains("crate cfg")), "unexpected err: {err:?}");

        // Present `cfg_atoms` without `target_pointer_width` refuses any value whose validity or
        // bit representation depends on 32-bit vs 64-bit (never silently assumes 64-bit).
        let isize_ty = tys.intern(PNode::Int(tc::IntTy::Isize)).expect("isize");
        let empty_cfg = CfgOnlySource { krate: Some(0), atoms: Some(&[]) };
        let err = literal_value(&mut tys, "0x1_0000_0000usize", usize_ty, empty_cfg, module, r).expect_err("missing target_pointer_width must refuse > u32::MAX");
        assert!(matches!(err, Why::Unsupported(msg, _) if msg.contains("target_pointer_width")), "unexpected err: {err:?}");
        let err = literal_value(&mut tys, "-1isize", isize_ty, empty_cfg, module, r).expect_err("missing target_pointer_width must refuse negative isize");
        assert!(matches!(err, Why::Unsupported(msg, _) if msg.contains("target_pointer_width")), "unexpected err: {err:?}");

        // Fixed-width literals (`u8`) do not depend on `target_pointer_width`.
        let got_u8 = literal_value(&mut tys, "1u8", u8_ty, no_cfg, module, r).expect("u8 succeeds without cfg").expect("u8 literal");
        assert_eq!(tys.view().get(got_u8), Some(&PNode::ConstValue { ty: u8_ty, bits: 1 }));

        // 32-bit target rejects `0x1_0000_0000usize` overflow.
        let w32 = [("target_pointer_width", Some("32"))];
        let src32 = CfgOnlySource { krate: Some(0), atoms: Some(&w32) };
        let err = literal_value(&mut tys, "0x1_0000_0000usize", usize_ty, src32, module, r).expect_err("32-bit must reject 2^32 usize");
        assert!(matches!(err, Why::Program(ref msg, _) if msg.contains("overflows `usize`")), "unexpected 32-bit err: {err:?}");

        // 64-bit target accepts `0x1_0000_0000usize`.
        let w64 = [("target_pointer_width", Some("64"))];
        let src64 = CfgOnlySource { krate: Some(0), atoms: Some(&w64) };
        let got64 = literal_value(&mut tys, "0x1_0000_0000usize", usize_ty, src64, module, r).expect("64-bit accepts 2^32").expect("usize literal");
        assert_eq!(tys.view().get(got64), Some(&PNode::ConstValue { ty: usize_ty, bits: 0x1_0000_0000 }));
    }
}
