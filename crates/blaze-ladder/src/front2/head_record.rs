//! A head record (drive.md, "End state: the whole pipeline on the command", piece 10): ONE
//! lowered head of the store ([`super::heads::Store`]) as bytes, written once and read back
//! into a caller's type table, so a head can be a record keyed by its item and compared by its
//! out digest ([`digest`]) instead of being rebuilt in memory on every command.
//!
//! The form, by the tree's record conventions (blaze-scope items_record.rs, names_record.rs):
//! a magic and a version first, every variable part after its length, every field fixed-width
//! little-endian, a reader that checks each length and refuses by [`HeadRecordError`] naming
//! what was short or unknown, never a panic on bad bytes.
//!
//! ```text
//! "BZHD" | version u32 | kind u8 | terms: count u32, then each node | the kind's fields
//! ```
//!
//! Canonical (rule 25): the same head gives the same bytes on any machine and in any order of
//! interning. A term is written by its STRUCTURE through a table local to the record, filled in
//! first-use order: the head's fields are walked in their fixed order and each type root is
//! walked depth first, children before the node (an explicit stack with a cap; nothing
//! recurses), so a node's children always have smaller local indices and the store's own
//! interner indices never reach the bytes. An item is named by its key (the checker's
//! `ItemKey`, 16 bytes, or the library's, 32 bytes), never by a path or a file place. Lists keep
//! the order the lowering gives them: every list of a head is in written order (parameters,
//! fields, variants, members), which is meaningful and machine-free; no map order reaches here.
//!
//! A fn's text (what its body is lowered from) is not copied: the record holds its length and
//! digest, and the reader hands the text in (it has it: the item's own text, by which the record
//! is keyed); a text that is not the one the head was made from is [`HeadRecordError::Text`].
//!
//! Refused by name ([`HeadRecordError::Unencoded`]), never skipped: an inference hole, a
//! closure type and an async block's future in a head's terms (body types, never a head's); an
//! alias whose head was refused (its why is a `Why`, not a record). Every lifetime part is in
//! the form (10): a fn's, an ADT's, a trait's and an impl's, made or refused, so an edit of a
//! lifetime bound alone moves the head's reader digest. A refused alias and a member refused
//! alone are records of their why's text (forms 12 and 13: `alias_gaps`, `fn_gaps`). An impl's const parameters are its frame's
//! (form 9), filed under the impl's library key by whoever holds it (the want).

use blaze_build::layout::{IntRepr, ReprAttrs};
use blaze_hash::HashMap;
use blaze_id::{ContentId, ItemKey as LibKey};
use blaze_lower::form as lf;
use blaze_own::{Alloc, Vec};
use blaze_surface::RecordBytes;
use blaze_typeck as tc;
use blaze_typeck::dyn_compat::{FnFacts, MemberFacts, MemberShape, Receiver, Unread};
use blaze_typeck::{FloatTy, IntTy, ItemKey, List, Name, PClause, PNode, PTy, PTys, PortableTys, UintTy};

use super::gat::{AssocWhere, Outlived, Outlives};
use super::heads::{AdtRow, CtorSig, FnHead, ImplHead, MemberFrame, Store, TraitExtra, TraitHead};
use super::keys::checker_of;
use super::lifetimes::{AdtLife, ContainerLife, FnLife, LifeOf, LifeRefusal, LifeVariance, Region, Variance};
use super::preds::Preds;
use super::types::{ParamBound, SelfTy, TraitAliasHead};

/// The allocator a decoded head's own lists and lifetime parts are kept in: the store's.
type Mem = Alloc<'static>;

const MAGIC: [u8; 4] = *b"BZHD";
/// Raised on any change of the form: an old record is then [`HeadRecordError::Version`].
/// 2: an impl's record holds one `self` flag per fn (`ImplHead::fn_self`, brief 14).
/// 3: a fn's record holds its C-variadic flag after its body flag (`FnHead::c_variadic`).
/// 4: a trait's flags carry `COINDUCTIVE` from `#[rustc_coinductive]` (census 14, brief A).
/// 5: a trait member's predicates hold `Self: MetaSized` unless the trait states a sizedness
/// (self_size.rs).
/// 6: a fn's record holds its `#[rustc_legacy_const_generics]` positions after its C-variadic
/// flag (`FnHead::legacy_const_generics`).
/// 7: an ADT's record holds its `#[blaze::args]` mark after its valid range
/// (`AdtRow::args`, rule 83).
/// 8: an ADT's record holds its field defaults (RFC 3681) after that mark: one optional key
/// per field (`Store::field_defaults`), each the key of an anonymous constant's fn head.
/// 9: the store's side tables a body or a later head reads (engine E2a, front2/tables/head.rs):
/// an ADT's record holds its fieldless-enum variant count (`Maps::fieldless_enums`); a trait's,
/// per associated type in `TraitHead::assoc` order, its own parameter count, outlives clauses
/// and own const parameters (`Maps::assoc_generics`, `assoc_where`, `assoc_consts`); an impl's,
/// its members' frame (`Store::member_frame`), whose const parameters are the impl's own.
/// 10: every lifetime part (engine E2a step 4): a fn's (`FnLife`), an ADT's (`AdtLife`), a
/// trait's and an impl's (`ContainerLife`, after the trait's associated type rows and after the
/// impl's frame), each a slot of 0 not made, 1 made, 2 refused with its typed reason
/// (`LifeRefusal`: its rule's tag and two u32s, a range zeroed in the reader digest).
/// 11: a trait's record holds, per associated type after its own const parameters, its item
/// bounds (`Store::preds` under the type's key, filed with the members); a trait one of whose
/// associated types had its bounds refused is refused by name (`Unencoded`).
/// 12: a type alias whose head was refused is a record too (`K_ALIAS_REFUSED`: its key and its
/// why as the store keeps it, `Maps::alias_gaps`' text), filed back as refused.
/// 13: a member fn or constant whose signature was refused alone is a record too
/// (`K_FN_REFUSED`: its key and its why's text, `Store::fn_gaps`), filed back as that member's
/// gap: a trait whose one member's type holds a constant that does not evaluate is frozen with
/// its other members, and a body that calls that one is refused for its reason.
/// 14: an ADT's record holds its valid range's two optional bounds where the narrowed-range
/// flag was (`AdtRow::valid`: layout takes its niches outside them, as rustc does).
pub const VERSION: u32 = 14;
/// The digest domains: the record's out digest and a fn text's check.
const DIGEST_DOMAIN: &str = "blaze.head-record.v1";
const TEXT_DOMAIN: &str = "blaze.head-record.text.v1";
/// The most terms one head holds, and the deepest one term's walk goes: past either, refused.
const MOST_TERMS: u32 = 1 << 20;
const MOST_STACK: usize = 1 << 16;

const K_FN: u8 = 1;
const K_ADT: u8 = 2;
const K_CTOR: u8 = 3;
const K_TRAIT: u8 = 4;
const K_IMPL: u8 = 5;
const K_ALIAS: u8 = 6;
const K_TRAIT_ALIAS: u8 = 7;
const K_OPAQUE: u8 = 8;
const K_ALIAS_REFUSED: u8 = 9;
const K_FN_REFUSED: u8 = 10;

// Term tags, one per `PNode` form written.
const T_BOOL: u8 = 0;
const T_CHAR: u8 = 1;
const T_INT: u8 = 2;
const T_UINT: u8 = 3;
const T_FLOAT: u8 = 4;
const T_STR: u8 = 5;
const T_NEVER: u8 = 6;
const T_TUPLE: u8 = 7;
const T_REF: u8 = 8;
const T_PTR: u8 = 9;
const T_ARRAY: u8 = 10;
const T_SLICE: u8 = 11;
const T_PAT: u8 = 12;
const T_ADT: u8 = 13;
const T_FNDEF: u8 = 14;
const T_FNPTR: u8 = 15;
const T_PARAM: u8 = 16;
const T_ALIAS: u8 = 17;
const T_OPAQUE: u8 = 18;
const T_ERROR: u8 = 19;
const T_CONST_VALUE: u8 = 20;
const T_CONST_PARAM: u8 = 21;
const T_DYNAMIC: u8 = 22;
const T_EX_TRAIT: u8 = 23;
const T_EX_PROJ: u8 = 24;
const T_EX_AUTO: u8 = 25;

/// Why a head record was not written or not read.
#[derive(Debug)]
pub enum HeadRecordError {
    /// Not `BZHD`.
    NotThisRecord,
    /// A head record of another version: rebuilt, never migrated.
    Version { found: u32 },
    /// The bytes end inside `what`, which starts at `at`.
    Short { what: &'static str, at: usize },
    /// A byte that is no value of its kind (`what`: a term's tag, a region's, a flag).
    Byte { what: &'static str, byte: u8 },
    /// A term refers to one not before it in the record's table (`child` is not below `node`).
    Order { what: &'static str, node: u32, child: u32 },
    /// Bytes after the head's last field.
    Trailing { at: usize },
    /// A count does not fit its field, or a head passes the cap on terms or a walk's depth.
    TooBig { what: &'static str },
    /// A form this record does not hold, by name: refused, never skipped.
    Unencoded { what: &'static str },
    /// A term or list of the head is not in the table it was handed with.
    Outside { what: &'static str },
    /// Well-formed bytes that are no head (`what`: an inherent impl with trait arguments, a
    /// variant's fields past the row's).
    Shape { what: &'static str },
    /// A fn head is read without its text.
    NoText,
    /// The text handed in is not the one the head was made from.
    Text { want_len: u32, got_len: usize },
    /// The store has no such head made.
    NotMade { at: HeadAt },
    /// The type table refused a term (its cap).
    Table(tc::CheckError),
}

impl core::fmt::Display for HeadRecordError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HeadRecordError::NotThisRecord => write!(f, "head record: not a head record (no `BZHD`)"),
            HeadRecordError::Version { found } => write!(f, "head record: version {found}, this reader is {VERSION}: rebuild it"),
            HeadRecordError::Short { what, at } => write!(f, "head record: the bytes end inside {what} (at byte {at})"),
            HeadRecordError::Byte { what, byte } => write!(f, "head record: {byte:#04x} is no value of {what}"),
            HeadRecordError::Order { what, node, child } => write!(f, "head record: {what}: term {node} names term {child}, not before it"),
            HeadRecordError::Trailing { at } => write!(f, "head record: bytes after the head's last field (at byte {at})"),
            HeadRecordError::TooBig { what } => write!(f, "head record: {what} past its cap"),
            HeadRecordError::Unencoded { what } => write!(f, "head record: {what} is not in the head record's form"),
            HeadRecordError::Outside { what } => write!(f, "head record: {what} is not in the type table handed in"),
            HeadRecordError::Shape { what } => write!(f, "head record: {what}"),
            HeadRecordError::NoText => write!(f, "head record: a fn head is read without its item's text"),
            HeadRecordError::Text { want_len, got_len } => {
                write!(f, "head record: the text handed in ({got_len} bytes) is not the one the head was made from ({want_len} bytes, by digest)")
            }
            HeadRecordError::NotMade { at } => write!(f, "head record: the store has no head made for {at:?}"),
            HeadRecordError::Table(e) => write!(f, "head record: the type table refused a term: {e:?}"),
        }
    }
}

type E = HeadRecordError;

/// The out digest of a head record: what a reader compares to know a head did not move.
pub fn digest(bytes: &[u8]) -> ContentId {
    ContentId::of(DIGEST_DOMAIN, bytes)
}

/// Where a head is in the store, by the key it is filed under there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HeadAt {
    Fn(ItemKey),
    Adt(LibKey),
    Ctor(ItemKey),
    Trait(LibKey),
    Impl(ItemKey),
    Alias(LibKey),
    TraitAlias(LibKey),
    Opaque(ItemKey),
}

/// One head, borrowed from where it is kept, as the encoder reads it.
#[derive(Clone, Copy)]
pub enum HeadRef<'h, 'n> {
    /// A fn, a const (a fn of no inputs) or a static (`static_` its mutability), with its
    /// where-clauses and its lifetime part (form 10: made or refused; `None` when never made,
    /// lifetimes.rs `Store::make_lives`).
    Fn { key: ItemKey, head: &'h FnHead<'n>, preds: Option<&'h Preds>, static_: Option<tc::Mutbl>, life: Option<Result<&'h FnLife, LifeRefusal>> },
    /// A member fn or constant whose signature was refused when its container was made
    /// (`Store::fn_gaps`, form 13): its why, so a body that uses it is refused for that reason
    /// and the container's other members stand.
    FnRefused { key: ItemKey, why: GapWhy<'h> },
    /// A struct, enum or union: its row, its parameters' defaults and const parameters, its
    /// where-clauses and lifetime part.
    Adt {
        key: LibKey,
        row: &'h AdtRow,
        defaults: &'h [Option<PTy>],
        const_params: &'h [(u32, PTy)],
        /// Its field defaults (`Store::field_defaults`): one per field, or empty when no
        /// field has one. Each default's own head is the fn record of its key.
        field_defaults: &'h [Option<ItemKey>],
        preds: Option<&'h Preds>,
        life: Option<Result<&'h AdtLife, LifeRefusal>>,
        /// Its variant count when it is a fieldless enum (`Maps::fieldless_enums`, form 9).
        fieldless: Option<u32>,
    },
    /// A tuple struct's or variant's constructor fn.
    Ctor { key: ItemKey, sig: &'h CtorSig },
    /// A trait: its head, its members' part once made, its supertraits on `Self`, its const
    /// parameters and where-clauses.
    Trait {
        key: LibKey,
        head: &'h TraitHead,
        extra: Option<&'h TraitExtra>,
        supers: &'h [(LibKey, List)],
        const_params: &'h [(u32, PTy)],
        preds: Option<&'h Preds>,
        /// Its associated types' side rows (form 9), one per `head.assoc` name.
        assoc_side: AssocSides<'h, 'n>,
        /// Its lifetime part (form 10).
        life: Option<Result<&'h ContainerLife, LifeRefusal>>,
    },
    /// An impl: its header, its where-clauses, its members' frame (form 9) and its lifetime
    /// part (form 10).
    Impl { key: ItemKey, head: &'h ImplHead, preds: Option<&'h Preds>, frame: Option<&'h MemberFrame>, life: Option<Result<&'h ContainerLife, LifeRefusal>> },
    /// A type alias whose head was made.
    Alias { key: LibKey, generics: u32, ty: PTy, defaults: &'h [Option<PTy>], const_params: &'h [(u32, PTy)] },
    /// A type alias whose head was refused: its why's text as the store keeps it
    /// (`Maps::alias_gaps`), as bytes (UTF-8, checked where it is filed: heads.rs `file_heads`).
    AliasRefused { key: LibKey, why: &'h [u8] },
    TraitAlias { key: LibKey, head: &'h TraitAliasHead },
    /// An `impl Trait` a fn's head made: its fn and index, its bounds (`Self` the opaque).
    Opaque { key: ItemKey, fn_: ItemKey, index: u32, preds: Option<&'h Preds> },
}

/// One associated type's side rows of a trait (form 9): what the store files beside the trait's
/// members, by the trait and the name.
pub struct AssocSide {
    pub name: Name,
    /// Its own type and const parameter count (`Maps::assoc_generics`).
    pub generics: Option<u32>,
    /// Its outlives where clauses (`Maps::assoc_where`).
    pub where_: Option<AssocWhere>,
    /// Its own const parameters (`Maps::assoc_consts`).
    pub consts: Option<Vec<(u32, PTy), Mem>>,
    /// Its item bounds (form 11: `type A: B;` and the implied `Sized`, `Self` the alias), filed
    /// by the trait's members under the associated type's own key (`Store::preds`); none until
    /// the members are made.
    pub bounds: Option<Preds>,
}

/// Where a trait's [`AssocSide`] rows are read from: the store's maps, or a decoded head's own.
#[derive(Clone, Copy)]
pub enum AssocSides<'h, 'n> {
    Store { store: &'h Store<'n>, trait_: LibKey },
    Rows(&'h [AssocSide]),
}

impl AssocSides<'_, '_> {
    /// `name`'s rows: its parameter count, outlives clauses and own const parameters.
    pub fn of(&self, name: Name) -> (Option<u32>, Option<AssocWhere>, Option<&[(u32, PTy)]>) {
        match *self {
            AssocSides::Store { store, trait_ } => (
                store.maps.assoc_generics.get(&(trait_, name)).copied(),
                store.maps.assoc_where.get(&(trait_, name)).copied(),
                store.maps.assoc_consts.get(&(trait_, name)).map(|c| &c[..]),
            ),
            AssocSides::Rows(rows) => match rows.iter().find(|r| r.name == name) {
                Some(r) => (r.generics, r.where_, r.consts.as_deref()),
                None => (None, None, None),
            },
        }
    }

    /// `name`'s item bounds (form 11), filed under its key (`item`, from the trait's members
    /// part) by the members' making.
    pub fn bounds(&self, name: Name, item: Option<ItemKey>) -> Option<&Preds> {
        match *self {
            AssocSides::Store { store, .. } => item.and_then(|k| store.preds.get(&k)),
            AssocSides::Rows(rows) => rows.iter().find(|r| r.name == name).and_then(|r| r.bounds.as_ref()),
        }
    }
}

impl<'n> HeadRef<'_, 'n> {
    /// A fn head's text: what [`decode`] is handed back.
    pub fn text(&self) -> Option<&'n str> {
        match self {
            HeadRef::Fn { head, .. } => Some(head.text),
            _ => None,
        }
    }
}

/// One head, decoded: the store's own head types, so it is usable where a lowered one is.
pub enum Head<'n> {
    Fn { key: ItemKey, head: FnHead<'n>, preds: Option<Preds>, static_: Option<tc::Mutbl>, life: Option<Result<FnLife, LifeRefusal>> },
    Adt {
        key: LibKey,
        row: AdtRow,
        defaults: Vec<Option<PTy>, Mem>,
        const_params: Vec<(u32, PTy), Mem>,
        field_defaults: Vec<Option<ItemKey>, Mem>,
        preds: Option<Preds>,
        life: Option<Result<AdtLife, LifeRefusal>>,
        fieldless: Option<u32>,
    },
    Ctor { key: ItemKey, sig: CtorSig },
    Trait {
        key: LibKey,
        head: TraitHead,
        extra: Option<TraitExtra>,
        supers: Vec<(LibKey, List), Mem>,
        const_params: Vec<(u32, PTy), Mem>,
        preds: Option<Preds>,
        assoc_side: Vec<AssocSide, Mem>,
        life: Option<Result<ContainerLife, LifeRefusal>>,
    },
    Impl { key: ItemKey, head: ImplHead, preds: Option<Preds>, frame: Option<MemberFrame>, life: Option<Result<ContainerLife, LifeRefusal>> },
    Alias { key: LibKey, generics: u32, ty: PTy, defaults: Vec<Option<PTy>, Mem>, const_params: Vec<(u32, PTy), Mem> },
    /// Its why's text ([`HeadRef::AliasRefused`]).
    AliasRefused { key: LibKey, why: Vec<u8, Mem> },
    /// Its why's text ([`HeadRef::FnRefused`]).
    FnRefused { key: ItemKey, why: Vec<u8, Mem> },
    TraitAlias { key: LibKey, head: TraitAliasHead },
    Opaque { key: ItemKey, fn_: ItemKey, index: u32, preds: Option<Preds> },
}

impl<'n> Head<'n> {
    /// The decoded head, borrowed as the encoder and [`same`] read one.
    pub fn as_ref(&self) -> HeadRef<'_, 'n> {
        match self {
            Head::Fn { key, head, preds, static_, life } => {
                HeadRef::Fn { key: *key, head, preds: preds.as_ref(), static_: *static_, life: life_ref(life) }
            }
            Head::Adt { key, row, defaults, const_params, field_defaults, preds, life, fieldless } => HeadRef::Adt {
                key: *key,
                row,
                defaults: &defaults[..],
                const_params: &const_params[..],
                field_defaults: &field_defaults[..],
                preds: preds.as_ref(),
                life: life_ref(life),
                fieldless: *fieldless,
            },
            Head::Ctor { key, sig } => HeadRef::Ctor { key: *key, sig },
            Head::Trait { key, head, extra, supers, const_params, preds, assoc_side, life } => HeadRef::Trait {
                key: *key,
                head,
                extra: extra.as_ref(),
                supers: &supers[..],
                const_params: &const_params[..],
                preds: preds.as_ref(),
                assoc_side: AssocSides::Rows(&assoc_side[..]),
                life: life_ref(life),
            },
            Head::Impl { key, head, preds, frame, life } => {
                HeadRef::Impl { key: *key, head, preds: preds.as_ref(), frame: frame.as_ref(), life: life_ref(life) }
            }
            Head::Alias { key, generics, ty, defaults, const_params } => HeadRef::Alias {
                key: *key,
                generics: *generics,
                ty: *ty,
                defaults: &defaults[..],
                const_params: &const_params[..],
            },
            Head::AliasRefused { key, why } => HeadRef::AliasRefused { key: *key, why: &why[..] },
            Head::FnRefused { key, why } => HeadRef::FnRefused { key: *key, why: GapWhy::Text(&why[..]) },
            Head::TraitAlias { key, head } => HeadRef::TraitAlias { key: *key, head },
            Head::Opaque { key, fn_, index, preds } => HeadRef::Opaque { key: *key, fn_: *fn_, index: *index, preds: preds.as_ref() },
        }
    }
}

/// Why a member's signature was refused ([`HeadRef::FnRefused`]): the store's own `Why` as it
/// filed it (`Store::fn_gaps`), or the text a record held. The record holds the text (the
/// `Why`'s `Debug`, as `Maps::alias_gaps` keeps an alias's), so two builders write equal bytes.
#[derive(Clone, Copy)]
pub enum GapWhy<'h> {
    Made(&'h super::types::Why),
    Text(&'h [u8]),
}

impl GapWhy<'_> {
    /// Its text, written into `out` (a `Why` by its `Debug`, a record's as held).
    fn write_into(self, out: &mut RecordBytes<'_>) {
        match self {
            GapWhy::Text(text) => out.extend_from_slice(text),
            GapWhy::Made(why) => out.extend_from_slice(std::format!("{why:?}").as_bytes()),
        }
    }

    /// Whether two say the same text.
    fn same(self, other: GapWhy<'_>) -> bool {
        match (self, other) {
            (GapWhy::Text(a), GapWhy::Text(b)) => a == b,
            (GapWhy::Made(a), GapWhy::Made(b)) => std::format!("{a:?}") == std::format!("{b:?}"),
            (GapWhy::Made(why), GapWhy::Text(text)) | (GapWhy::Text(text), GapWhy::Made(why)) => std::format!("{why:?}").as_bytes() == text,
        }
    }
}

/// A kept lifetime part, borrowed.
fn life_ref<T>(l: &Option<Result<T, LifeRefusal>>) -> Option<Result<&T, LifeRefusal>> {
    l.as_ref().map(|r| r.as_ref().map_err(|w| *w))
}

/// Every head the store has made, by where it is filed, sorted (no map order leaves here).
pub fn heads_in<'a>(store: &Store<'_>, alloc: Alloc<'a>) -> Vec<HeadAt, Alloc<'a>> {
    let mut out = Vec::new_in(alloc);
    for &k in store.fns.keys() {
        out.push(HeadAt::Fn(k));
    }
    for &k in store.maps.generics.keys() {
        if store.adts.contains_key(&checker_of(k)) {
            out.push(HeadAt::Adt(k));
        }
    }
    for &k in store.ctors.keys() {
        out.push(HeadAt::Ctor(k));
    }
    for &k in store.maps.traits.keys() {
        out.push(HeadAt::Trait(k));
    }
    for &k in store.impls.keys() {
        out.push(HeadAt::Impl(k));
    }
    for &k in store.maps.aliases.keys() {
        out.push(HeadAt::Alias(k));
    }
    for &k in store.maps.trait_aliases.keys() {
        out.push(HeadAt::TraitAlias(k));
    }
    for &k in store.opaques.keys() {
        out.push(HeadAt::Opaque(k));
    }
    out.sort_unstable();
    out
}

/// The head filed at `at`, borrowed from the store, with the lifetime part the store made or
/// filed for it (lifetimes.rs `Store::make_lives`; `None` when never made).
pub fn from_store<'h, 'n>(store: &'h Store<'n>, at: HeadAt) -> Result<HeadRef<'h, 'n>, HeadRecordError> {
    let none = E::NotMade { at };
    let lives = store.lives_of(at);
    let fn_life = match lives {
        Some(LifeOf::Fn(l)) => Some(l),
        _ => None,
    };
    let adt_life = match lives {
        Some(LifeOf::Adt(l)) => Some(l),
        _ => None,
    };
    let container_life = match lives {
        Some(LifeOf::Container(l)) => Some(l),
        _ => None,
    };
    Ok(match at {
        HeadAt::Fn(k) => {
            // A member refused alone (`Store::member_fn_head`): its why (form 13).
            if let (None, Some(why)) = (store.fns.get(&k), store.fn_gaps.get(&k)) {
                return Ok(HeadRef::FnRefused { key: k, why: GapWhy::Made(why) });
            }
            let head = store.fns.get(&k).ok_or(none)?;
            HeadRef::Fn { key: k, head, preds: store.preds.get(&k), static_: store.statics.get(&k).copied(), life: fn_life }
        }
        HeadAt::Adt(lib) => {
            let ck = checker_of(lib);
            let row = store.adts.get(&ck).ok_or(none)?;
            let defaults = store.maps.generics.get(&lib).map_or(&[][..], |(_, d)| &d[..]);
            let const_params = store.maps.const_params.get(&lib).map_or(&[][..], |c| &c[..]);
            // Made with the row (`Store::adt`): a row without it is not this store's own.
            let field_defaults = store.field_defaults.get(&ck).ok_or(E::NotMade { at })?;
            let fieldless = store.maps.fieldless_enums.get(&ck).copied();
            HeadRef::Adt { key: lib, row, defaults, const_params, field_defaults: &field_defaults[..], preds: store.preds.get(&ck), life: adt_life, fieldless }
        }
        HeadAt::Ctor(k) => HeadRef::Ctor { key: k, sig: store.ctors.get(&k).ok_or(none)? },
        HeadAt::Trait(lib) => {
            let ck = checker_of(lib);
            let head = store.maps.traits.get(&lib).ok_or(none)?;
            let supers = store.maps.supers.get(&lib).map_or(&[][..], |s| &s[..]);
            let const_params = store.maps.const_params.get(&lib).map_or(&[][..], |c| &c[..]);
            let assoc_side = AssocSides::Store { store, trait_: lib };
            let extra = store.trait_defs.get(&ck);
            // An associated type whose bounds were refused (`Store::trait_members` files the
            // why in `fn_gaps` under its key): its why is a `Why`, not a record.
            if extra.is_some_and(|x| x.assoc.iter().any(|a| store.fn_gaps.contains_key(&a.item))) {
                return Err(E::Unencoded { what: "an associated type whose bounds were refused (its why is a `Why`, not a record)" });
            }
            HeadRef::Trait {
                key: lib,
                head,
                extra,
                supers,
                const_params,
                preds: store.preds.get(&ck),
                assoc_side,
                life: container_life,
            }
        }
        HeadAt::Impl(k) => HeadRef::Impl {
            key: k,
            head: store.impls.get(&k).ok_or(none)?,
            preds: store.preds.get(&k),
            frame: store.member_frame(k),
            life: container_life,
        },
        HeadAt::Alias(lib) => {
            let Some((generics, ty)) = *store.maps.aliases.get(&lib).ok_or(none)? else {
                // Refused: the store keeps its why's text beside it (`Store::step`, `AliasGap`).
                let gap = store.maps.alias_gaps.get(&lib).ok_or(E::NotMade { at })?;
                return Ok(HeadRef::AliasRefused { key: lib, why: gap.text.as_bytes() });
            };
            let defaults = store.maps.alias_defaults.get(&lib).map_or(&[][..], |d| &d[..]);
            let const_params = store.maps.const_params.get(&lib).map_or(&[][..], |c| &c[..]);
            HeadRef::Alias { key: lib, generics, ty, defaults, const_params }
        }
        HeadAt::TraitAlias(lib) => HeadRef::TraitAlias { key: lib, head: store.maps.trait_aliases.get(&lib).ok_or(none)? },
        HeadAt::Opaque(k) => {
            let &(fn_, index) = store.opaques.get(&k).ok_or(none)?;
            HeadRef::Opaque { key: k, fn_, index, preds: store.preds.get(&k) }
        }
    })
}

/// A fn's lifetime part, copied into `alloc`.
pub fn copy_fn_life(l: &FnLife, alloc: Mem) -> FnLife {
    FnLife {
        regions: copy(&l.regions, alloc),
        ends: copy(&l.ends, alloc),
        bounds: copy(&l.bounds, alloc),
        container: l.container,
        named: l.named,
    }
}

/// An ADT's lifetime part, copied into `alloc`.
pub fn copy_adt_life(l: &AdtLife, alloc: Mem) -> AdtLife {
    AdtLife {
        lifetimes: l.lifetimes,
        variances: copy(&l.variances, alloc),
        bounds: copy(&l.bounds, alloc),
        fields: copy(&l.fields, alloc),
        field_ends: copy(&l.field_ends, alloc),
    }
}

fn copy<T: Copy>(from: &[T], alloc: Mem) -> Vec<T, Mem> {
    let mut v = Vec::with_capacity_in(from.len(), alloc);
    v.extend_from_slice(from);
    v
}

// ---------------------------------------------------------------------------------------------
// Writing.

/// Whether two `Self`s are one, their lists compared by `l` ([`same`]).
fn self_same(a: SelfTy, b: SelfTy, l: &impl Fn(List, List) -> bool) -> bool {
    match (a, b) {
        (SelfTy::None, SelfTy::None) => true,
        (SelfTy::Trait(x), SelfTy::Trait(y)) => x == y,
        (SelfTy::Ty(x, None), SelfTy::Ty(y, None)) => x == y,
        (SelfTy::Ty(x, Some((t1, a1))), SelfTy::Ty(y, Some((t2, a2)))) => x == y && t1 == t2 && l(a1, a2),
        _ => false,
    }
}

/// `head` as a record. Its terms are read from `tys`, the table they are in.
pub fn encode<'b>(head: HeadRef<'_, '_>, tys: PTys<'_>, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, HeadRecordError> {
    encode_with(head, tys, alloc, Text::Kept)
}

/// What a head's READERS compare (the drive's `Signature` out, head_digest.rs `want_digest`):
/// [`digest`] of `head`'s record with a fn's text slot (its length and digest) zeroed, so the
/// out holds what the head MEANS (its terms, keys, names, counts and flags) and nothing of how
/// it is spelled. A fn's text is its whole item, spacing, comments and body included: it is in
/// the record only so [`decode`] can check the text it is handed (the body is lowered from
/// it), and a reader of the head never reads it. Its own body's record is keyed by that text.
/// (Before: a reformat of `fn f( _x : u8 )` moved every reader of `f`, blaze-cli test
/// `a_signature_edit_makes_its_head_and_its_readers_only`, 2026-11-17.) Never decoded.
pub fn reader_digest(head: HeadRef<'_, '_>, tys: PTys<'_>, alloc: Alloc<'_>) -> Result<ContentId, HeadRecordError> {
    let bytes = encode_with(head, tys, alloc, Text::Zeroed)?;
    Ok(digest(&bytes))
}

/// Whether a fn's text slot holds its text's length and digest, or zeros of the same width.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Text {
    Kept,
    Zeroed,
}

fn encode_with<'b>(head: HeadRef<'_, '_>, tys: PTys<'_>, alloc: Alloc<'b>, slot: Text) -> Result<RecordBytes<'b>, HeadRecordError> {
    let mut o = Out {
        tys,
        terms: RecordBytes::new_in(alloc),
        body: RecordBytes::new_in(alloc),
        seen: HashMap::new_in(alloc),
        count: 0,
        stack: Vec::new_in(alloc),
        kids: Vec::new_in(alloc),
    };
    let kind = match head {
        HeadRef::Fn { key, head, preds, static_, life } => {
            o.key(key);
            let text = head.text.as_bytes();
            let len = u32::try_from(text.len()).map_err(|_| E::TooBig { what: "a fn's text" })?;
            match slot {
                Text::Kept => {
                    o.u32(len);
                    o.body.extend_from_slice(&ContentId::of(TEXT_DOMAIN, text).0);
                }
                Text::Zeroed => {
                    o.u32(0);
                    o.body.extend_from_slice(&[0; 32]);
                }
            }
            o.lib(head.lib);
            o.opt(head.container, |o, k| {
                o.key(k);
                Ok(())
            })?;
            o.u32(head.parent);
            o.u32(head.own);
            o.u32(head.declared);
            o.flag(head.self_param);
            o.byte(head.header.bits());
            o.tys_of(&head.inputs, "a fn's inputs")?;
            o.ty(head.output)?;
            o.flag(head.has_body);
            o.flag(head.c_variadic);
            o.count(head.legacy_const_generics.len(), "a fn's legacy const generic positions")?;
            for &k in &head.legacy_const_generics {
                o.u32(k);
            }
            o.lib(head.module);
            o.scope(&head.params, &head.consts, &head.bounds, head.self_)?;
            o.named_tys(&head.self_assoc, "a fn's `Self` associated types")?;
            o.opt(preds, |o, p| o.preds(p))?;
            o.opt(static_, |o, m| {
                o.byte(mutbl_tag(m));
                Ok(())
            })?;
            o.life(life, slot, |o, l| {
                o.regions(&l.regions, "a fn's regions")?;
                o.u32s(&l.ends, "a fn's region ends")?;
                o.region_pairs(&l.bounds, "a fn's lifetime bounds")?;
                o.u32(l.container);
                o.u32(l.named);
                Ok(())
            })?;
            K_FN
        }
        HeadRef::Adt { key, row, defaults, const_params, field_defaults, preds, life, fieldless } => {
            o.lib(key);
            o.byte(match row.kind {
                tc::AdtKind::Struct => 0,
                tc::AdtKind::Enum => 1,
                tc::AdtKind::Union => 2,
            });
            o.u32(row.generics);
            o.count(row.variants.len(), "an ADT's variants")?;
            for v in &row.variants {
                o.key(v.key);
                o.name(v.name);
                o.byte(match v.ctor {
                    tc::CtorKind::Braces => 0,
                    tc::CtorKind::Tuple => 1,
                    tc::CtorKind::Unit => 2,
                });
                o.u32(v.fields.start);
                o.u32(v.fields.len);
            }
            o.count(row.fields.len(), "an ADT's fields")?;
            for f in &row.fields {
                o.name(f.name);
                o.ty(f.ty)?;
            }
            o.repr(row.repr);
            o.opt(row.valid.start, |o, s| {
                o.body.extend_from_slice(&s.to_le_bytes());
                Ok(())
            })?;
            o.opt(row.valid.end, |o, s| {
                o.body.extend_from_slice(&s.to_le_bytes());
                Ok(())
            })?;
            o.flag(row.args);
            o.count(row.discrs.len(), "an enum's discriminants")?;
            for &d in &row.discrs {
                o.body.extend_from_slice(&d.to_le_bytes());
            }
            o.defaults(defaults)?;
            o.const_params(const_params)?;
            // Its field defaults (form 8): one optional key per field, or none at all.
            o.count(field_defaults.len(), "an ADT's field defaults")?;
            for &d in field_defaults {
                o.opt(d, |o, k| {
                    o.key(k);
                    Ok(())
                })?;
            }
            o.opt(preds, |o, p| o.preds(p))?;
            o.life(life, slot, |o, l| {
                o.u32(l.lifetimes);
                o.count(l.variances.len(), "an ADT's variances")?;
                for v in l.variances.iter() {
                    o.byte(match v.variance {
                        Variance::Co => 0,
                        Variance::Contra => 1,
                        Variance::In => 2,
                    });
                    o.flag(v.computed);
                }
                o.region_pairs(&l.bounds, "an ADT's lifetime bounds")?;
                o.regions(&l.fields, "an ADT's field regions")?;
                o.u32s(&l.field_ends, "an ADT's field region ends")
            })?;
            o.opt(fieldless, |o, n| {
                o.u32(n);
                Ok(())
            })?;
            K_ADT
        }
        HeadRef::Ctor { key, sig } => {
            o.key(key);
            o.key(sig.adt);
            o.u32(sig.generics);
            o.tys_of(&sig.inputs, "a constructor's inputs")?;
            o.ty(sig.output)?;
            K_CTOR
        }
        HeadRef::Trait { key, head, extra, supers, const_params, preds, assoc_side, life } => {
            o.lib(key);
            o.u32(head.generics);
            o.defaults(&head.defaults)?;
            o.count(head.assoc.len(), "a trait's associated types")?;
            for &n in &head.assoc {
                o.name(n);
            }
            o.named_keys(&head.fns, "a trait's fns")?;
            o.u32(head.flags.0);
            o.count(head.members.len(), "a trait's member facts")?;
            for m in &head.members {
                o.member(*m);
            }
            o.opt(extra, |o, x| {
                o.clauses(&x.supertraits, "a trait's supertrait clauses")?;
                o.count(x.assoc.len(), "a trait's associated type rows")?;
                for a in &x.assoc {
                    o.name(a.name);
                    o.key(a.item);
                    o.opt(a.default, |o, t| o.ty(t))?;
                }
                o.named_keys(&x.consts, "a trait's associated constants")
            })?;
            o.lib_lists(supers, "a trait's supertraits")?;
            o.const_params(const_params)?;
            o.opt(preds, |o, p| o.preds(p))?;
            // Form 9: each associated type's side rows, in `head.assoc` order (written order).
            for &n in &head.assoc {
                let (generics, where_, consts) = assoc_side.of(n);
                o.opt(generics, |o, g| {
                    o.u32(g);
                    Ok(())
                })?;
                o.opt(where_, |o, w| {
                    o.count(w.outlives().len(), "an associated type's outlives clauses")?;
                    for c in w.outlives() {
                        match c.subject {
                            Outlived::SelfTy => o.byte(0),
                            Outlived::Type(t) => {
                                o.byte(1);
                                o.name(t);
                            }
                            Outlived::Lifetime(l) => {
                                o.byte(2);
                                o.name(l);
                            }
                        }
                        o.name(c.lifetime);
                    }
                    Ok(())
                })?;
                o.opt(consts, |o, c| o.const_params(c))?;
                // Form 11: its item bounds, by its key in the members part.
                let item = extra.and_then(|x| x.assoc.iter().find(|a| a.name == n)).map(|a| a.item);
                o.opt(assoc_side.bounds(n, item), |o, p| o.preds(p))?;
            }
            o.container_life(life, slot)?;
            K_TRAIT
        }
        HeadRef::Impl { key, head, preds, frame, life } => {
            o.key(key);
            o.u32(head.generics);
            o.ty(head.self_ty)?;
            o.opt(head.trait_, |o, k| {
                o.key(k);
                Ok(())
            })?;
            o.list(head.trait_args.list(), "an impl's trait arguments")?;
            o.named_tys(&head.assoc, "an impl's associated types")?;
            o.named_keys(&head.fns, "an impl's fns")?;
            o.count(head.fn_self.len(), "an impl's fn `self` flags")?;
            for &s in head.fn_self.iter() {
                o.byte(u8::from(s));
            }
            o.named_keys(&head.consts, "an impl's constants")?;
            o.byte(match head.polarity {
                tc::ImplPolarity::Positive => 0,
                tc::ImplPolarity::Negative => 1,
                tc::ImplPolarity::Reservation => 2,
            });
            o.opt(preds, |o, p| o.preds(p))?;
            // Form 9: the frame its fns' signatures are made in.
            o.opt(frame, |o, f| {
                o.lib(f.module);
                o.u32(f.generics);
                o.scope(&f.params, &f.consts, &f.bounds, f.self_)
            })?;
            o.container_life(life, slot)?;
            K_IMPL
        }
        HeadRef::Alias { key, generics, ty, defaults, const_params } => {
            o.lib(key);
            o.u32(generics);
            o.ty(ty)?;
            o.defaults(defaults)?;
            o.const_params(const_params)?;
            K_ALIAS
        }
        HeadRef::AliasRefused { key, why } => {
            o.lib(key);
            o.count(why.len(), "an alias's refusal")?;
            o.body.extend_from_slice(why);
            K_ALIAS_REFUSED
        }
        HeadRef::FnRefused { key, why } => {
            o.key(key);
            let mut text = RecordBytes::new_in(alloc);
            why.write_into(&mut text);
            o.count(text.len(), "a refused fn's why")?;
            o.body.extend_from_slice(&text);
            K_FN_REFUSED
        }
        HeadRef::TraitAlias { key, head } => {
            o.lib(key);
            o.u32(head.generics);
            o.clauses(&head.clauses, "a trait alias's clauses")?;
            o.lib_lists(&head.traits, "a trait alias's traits")?;
            K_TRAIT_ALIAS
        }
        HeadRef::Opaque { key, fn_, index, preds } => {
            o.key(key);
            o.key(fn_);
            o.u32(index);
            o.opt(preds, |o, p| o.preds(p))?;
            K_OPAQUE
        }
    };
    let mut out = RecordBytes::new_in(alloc);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&o.count.to_le_bytes());
    out.extend_from_slice(&o.terms);
    out.extend_from_slice(&o.body);
    Ok(out)
}

/// The writer: the record's term table (first-use order) and the head's fields after it.
struct Out<'b, 't> {
    tys: PTys<'t>,
    terms: RecordBytes<'b>,
    body: RecordBytes<'b>,
    /// Each term written, by its index in `tys`, to its index in the record's table.
    seen: HashMap<PTy, u32, Alloc<'b>>,
    count: u32,
    /// The walk's work list: a term, and whether its children are already pushed.
    stack: Vec<(PTy, bool), Alloc<'b>>,
    kids: Vec<PTy, Alloc<'b>>,
}

impl Out<'_, '_> {
    fn byte(&mut self, b: u8) {
        self.body.push(b);
    }
    fn flag(&mut self, b: bool) {
        self.body.push(u8::from(b));
    }
    fn u32(&mut self, v: u32) {
        self.body.extend_from_slice(&v.to_le_bytes());
    }
    fn lib(&mut self, k: LibKey) {
        self.body.extend_from_slice(&k.0.0);
    }
    fn key(&mut self, k: ItemKey) {
        self.body.extend_from_slice(&k.0.to_le_bytes());
    }
    fn name(&mut self, n: Name) {
        self.body.extend_from_slice(&n.0.to_le_bytes());
    }
    fn count(&mut self, n: usize, what: &'static str) -> Result<(), E> {
        let n = u32::try_from(n).map_err(|_| E::TooBig { what })?;
        self.u32(n);
        Ok(())
    }
    fn opt<T>(&mut self, v: Option<T>, one: impl FnOnce(&mut Self, T) -> Result<(), E>) -> Result<(), E> {
        match v {
            None => {
                self.byte(0);
                Ok(())
            }
            Some(v) => {
                self.byte(1);
                one(self, v)
            }
        }
    }
    /// A lifetime part (form 10): 0 not made, 1 made (then `one`), 2 refused (then its reason,
    /// [`Out::refusal`]).
    fn life<T>(&mut self, v: Option<Result<&T, LifeRefusal>>, slot: Text, one: impl FnOnce(&mut Self, &T) -> Result<(), E>) -> Result<(), E> {
        match v {
            None => {
                self.byte(0);
                Ok(())
            }
            Some(Ok(l)) => {
                self.byte(1);
                one(self, l)
            }
            Some(Err(why)) => {
                self.byte(2);
                self.refusal(why, slot);
                Ok(())
            }
        }
    }
    /// A trait's or an impl's lifetime part (form 10).
    fn container_life(&mut self, life: Option<Result<&ContainerLife, LifeRefusal>>, slot: Text) -> Result<(), E> {
        self.life(life, slot, |o, l| {
            o.u32(l.lifetimes);
            o.u32(l.elided);
            o.regions(&l.self_regions, "a container's self regions")?;
            o.region_pairs(&l.bounds, "a container's lifetime bounds")?;
            o.count(l.names.len(), "a container's lifetime names")?;
            for &(name, index) in l.names.iter() {
                o.body.extend_from_slice(&name.to_le_bytes());
                o.u32(index);
            }
            Ok(())
        })
    }
    /// A lifetime refusal: its rule's tag, then its counts or its item-relative range. The
    /// range is where, not what: zeroed in the reader digest (`Text::Zeroed`), so a comment
    /// written before it moves no reader.
    fn refusal(&mut self, why: LifeRefusal, slot: Text) {
        use LifeRefusal as R;
        let (tag, a, b) = match why {
            R::NoHead => (0, 0, 0),
            R::Text => (1, 0, 0),
            R::Form => (2, 0, 0),
            R::TypeTable => (3, 0, 0),
            R::UnknownAdt => (4, 0, 0),
            R::ParamCount { text, head } => (5, text, head),
            R::FieldCount { text, head } => (6, text, head),
            R::TypeNode => (7, 0, 0),
            R::Bound => (8, 0, 0),
            R::Unaligned { at } => (9, at.start, at.end),
            R::Unresolved { at } => (10, at.start, at.end),
            R::Alias { at } => (11, at.start, at.end),
            R::DefaultWithPositions { at } => (12, at.start, at.end),
            R::LifetimeArgs { at } => (13, at.start, at.end),
            R::TwoLifetimesOneOrigin { at } => (14, at.start, at.end),
            R::UnknownLifetime { at } => (15, at.start, at.end),
            R::ElisionUndecided { at } => (16, at.start, at.end),
            R::ElidedInField { at } => (17, at.start, at.end),
            R::ObjectDefault { at } => (18, at.start, at.end),
            R::SelfRegions { at } => (19, at.start, at.end),
        };
        self.byte(tag);
        let ranged = tag >= 9;
        let (a, b) = if ranged && slot == Text::Zeroed { (0, 0) } else { (a, b) };
        self.u32(a);
        self.u32(b);
    }
    fn ty(&mut self, t: PTy) -> Result<(), E> {
        let at = self.term(t)?;
        self.u32(at);
        Ok(())
    }
    fn tys_of(&mut self, ts: &[PTy], what: &'static str) -> Result<(), E> {
        self.count(ts.len(), what)?;
        for &t in ts {
            self.ty(t)?;
        }
        Ok(())
    }
    /// A list of the table, by its elements (never its place in the table's list column).
    fn list(&mut self, l: List, what: &'static str) -> Result<(), E> {
        let ts = self.tys.list(l).map_err(|_| E::Outside { what })?;
        self.tys_of(ts, what)
    }
    fn named_tys(&mut self, v: &[(Name, PTy)], what: &'static str) -> Result<(), E> {
        self.count(v.len(), what)?;
        for &(n, t) in v {
            self.name(n);
            self.ty(t)?;
        }
        Ok(())
    }
    fn named_keys(&mut self, v: &[(Name, ItemKey)], what: &'static str) -> Result<(), E> {
        self.count(v.len(), what)?;
        for &(n, k) in v {
            self.name(n);
            self.key(k);
        }
        Ok(())
    }
    fn lib_lists(&mut self, v: &[(LibKey, List)], what: &'static str) -> Result<(), E> {
        self.count(v.len(), what)?;
        for &(k, l) in v {
            self.lib(k);
            self.list(l, what)?;
        }
        Ok(())
    }
    fn defaults(&mut self, v: &[Option<PTy>]) -> Result<(), E> {
        self.count(v.len(), "parameter defaults")?;
        for &d in v {
            self.opt(d, |o, t| o.ty(t))?;
        }
        Ok(())
    }
    fn const_params(&mut self, v: &[(u32, PTy)]) -> Result<(), E> {
        self.count(v.len(), "const parameters")?;
        for &(i, t) in v {
            self.u32(i);
            self.ty(t)?;
        }
        Ok(())
    }
    /// The scope a fn's signature is read in (a fn's own, an impl's members' frame): the
    /// parameters by name, the const parameters, the bounds and `Self`.
    fn scope(&mut self, params: &[(Name, u32)], consts: &[(u32, PTy)], bounds: &[ParamBound], self_: SelfTy) -> Result<(), E> {
        self.count(params.len(), "a fn's parameters in scope")?;
        for &(n, i) in params {
            self.name(n);
            self.u32(i);
        }
        self.const_params(consts)?;
        self.count(bounds.len(), "a fn's parameter bounds")?;
        for b in bounds {
            self.u32(b.param);
            self.lib(b.trait_);
            self.list(b.args, "a parameter bound's arguments")?;
        }
        match self_ {
            SelfTy::None => self.byte(0),
            SelfTy::Trait(t) => {
                self.byte(1);
                self.lib(t);
            }
            SelfTy::Ty(t, tr) => {
                self.byte(2);
                self.ty(t)?;
                self.opt(tr, |o, (k, l)| {
                    o.lib(k);
                    o.list(l, "`Self`'s trait arguments")
                })?;
            }
        }
        Ok(())
    }
    fn clauses(&mut self, v: &[PClause], what: &'static str) -> Result<(), E> {
        self.count(v.len(), what)?;
        for &c in v {
            match c {
                PClause::Trait { trait_, args } => {
                    self.byte(0);
                    self.key(trait_);
                    self.list(args, "a trait clause's arguments")?;
                }
                PClause::Projection { trait_, args, name, term } => {
                    self.byte(1);
                    self.key(trait_);
                    self.list(args, "a projection clause's arguments")?;
                    self.name(name);
                    self.ty(term)?;
                }
                PClause::Const { cond, args } => {
                    self.byte(2);
                    self.key(cond);
                    self.list(args, "a const clause's arguments")?;
                }
            }
        }
        Ok(())
    }
    fn preds(&mut self, p: &Preds) -> Result<(), E> {
        self.clauses(&p.clauses, "where-clauses")?;
        self.count(p.conds.len(), "`where const` conditions")?;
        for c in &p.conds {
            self.u32(c.0);
        }
        Ok(())
    }
    fn regions(&mut self, v: &[Region], what: &'static str) -> Result<(), E> {
        self.count(v.len(), what)?;
        for &r in v {
            self.region(r);
        }
        Ok(())
    }
    fn region(&mut self, r: Region) {
        let (tag, n) = match r {
            Region::Static => (0, 0),
            Region::Named(n) => (1, n),
            Region::Anon(n) => (2, n),
            Region::Hidden(n) => (3, n),
        };
        self.byte(tag);
        self.u32(n);
    }
    fn region_pairs(&mut self, v: &[(Region, Region)], what: &'static str) -> Result<(), E> {
        self.count(v.len(), what)?;
        for &(a, b) in v {
            self.region(a);
            self.region(b);
        }
        Ok(())
    }
    fn u32s(&mut self, v: &[u32], what: &'static str) -> Result<(), E> {
        self.count(v.len(), what)?;
        for &n in v {
            self.u32(n);
        }
        Ok(())
    }
    fn repr(&mut self, r: ReprAttrs) {
        let bits = u8::from(r.c) | u8::from(r.transparent) << 1 | u8::from(r.simd) << 2 | u8::from(r.hide_niche) << 3;
        self.byte(bits);
        match r.int {
            None => self.byte(0),
            Some(IntRepr::Signed(i)) => {
                self.byte(1);
                self.byte(int_tag(i));
            }
            Some(IntRepr::Unsigned(u)) => {
                self.byte(2);
                self.byte(uint_tag(u));
            }
        }
        for v in [r.packed, r.align] {
            match v {
                None => self.byte(0),
                Some(n) => {
                    self.byte(1);
                    self.byte(n);
                }
            }
        }
    }
    fn member(&mut self, m: MemberFacts) {
        self.name(m.name);
        match m.shape {
            MemberShape::Const => self.byte(0),
            MemberShape::Type { generic } => {
                self.byte(1);
                self.flag(generic);
            }
            MemberShape::Fn(f) => {
                self.byte(2);
                self.byte(match f.receiver {
                    Receiver::Absent => 0,
                    Receiver::Value => 1,
                    Receiver::Ref => 2,
                    Receiver::RefMut => 3,
                    Receiver::Box => 4,
                    Receiver::Rc => 5,
                    Receiver::Arc => 6,
                    Receiver::PinOfPointer => 7,
                    Receiver::Other => 8,
                });
                self.u32(f.own_non_lifetime_params);
                let bits = u8::from(f.where_self_sized)
                    | u8::from(f.is_async) << 1
                    | u8::from(f.opaque_return) << 2
                    | u8::from(f.self_outside_receiver) << 3;
                self.byte(bits);
            }
            MemberShape::Unread(u) => {
                self.byte(3);
                self.byte(match u {
                    Unread::TypedReceiver => 0,
                    Unread::SignatureType => 1,
                });
            }
        }
    }

    /// The record's index of `root`, writing it and every term under it not yet written:
    /// depth first, children before the node, by a work list with a cap.
    fn term(&mut self, root: PTy) -> Result<u32, E> {
        if let Some(&at) = self.seen.get(&root) {
            return Ok(at);
        }
        self.stack.clear();
        self.stack.push((root, false));
        while let Some((t, open)) = self.stack.pop() {
            if self.seen.contains_key(&t) {
                continue;
            }
            let node = self.tys.node(t).map_err(|_| E::Outside { what: "a term of the head" })?;
            if open {
                self.write_node(node)?;
                let at = self.count;
                self.count = at.checked_add(1).filter(|&c| c <= MOST_TERMS).ok_or(E::TooBig { what: "a head's terms" })?;
                self.seen.insert(t, at);
            } else {
                self.stack.push((t, true));
                self.push_children(node)?;
            }
        }
        self.seen.get(&root).copied().ok_or(E::Outside { what: "a term of the head" })
    }

    /// Pushes `n`'s children not yet written, the first on top. Every form is named: one a
    /// head never holds is refused here.
    fn push_children(&mut self, n: PNode) -> Result<(), E> {
        self.kids.clear();
        match n {
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Param(_)
            | PNode::Error
            | PNode::ConstParam(_)
            | PNode::ExAuto(_) => {}
            PNode::Ref(_, t) | PNode::Ptr(_, t) | PNode::Slice(t) | PNode::ConstValue { ty: t, .. } => self.kids.push(t),
            PNode::Array(e, len) => {
                self.kids.push(e);
                self.kids.push(len);
            }
            PNode::Pat(base, ranges) => {
                self.kids.push(base);
                self.kid_list(ranges)?;
            }
            PNode::Tuple(l)
            | PNode::Adt(_, l)
            | PNode::FnDef(_, l)
            | PNode::FnPtr(l, _)
            | PNode::Alias { args: l, .. }
            | PNode::Opaque(_, l)
            | PNode::Dynamic(l)
            | PNode::ExTrait(_, l) => self.kid_list(l)?,
            PNode::ExProj { args, term, .. } => {
                self.kid_list(args)?;
                self.kids.push(term);
            }
            PNode::Infer => return Err(E::Unencoded { what: "an inference hole `_` (a body's type, never a head's)" }),
            PNode::Closure { .. } => return Err(E::Unencoded { what: "a closure type (a body's, never a head's)" }),
            PNode::Future(..) => return Err(E::Unencoded { what: "an async block's future type (a body's, never a head's)" }),
            PNode::Coroutine { .. } => return Err(E::Unencoded { what: "an async block's future by identity (a body's hidden type, never a head's)" }),
        }
        if self.stack.len() + self.kids.len() > MOST_STACK {
            return Err(E::TooBig { what: "a term's walk" });
        }
        for i in (0..self.kids.len()).rev() {
            let k = self.kids[i];
            if !self.seen.contains_key(&k) {
                self.stack.push((k, false));
            }
        }
        Ok(())
    }

    fn kid_list(&mut self, l: List) -> Result<(), E> {
        let ts = self.tys.list(l).map_err(|_| E::Outside { what: "a term's list" })?;
        self.kids.extend_from_slice(ts);
        Ok(())
    }

    /// The record's index of a child already written.
    fn local(&self, t: PTy) -> Result<u32, E> {
        self.seen.get(&t).copied().ok_or(E::Order { what: "a child not written before its term", node: self.count, child: t.0 })
    }
    fn t8(&mut self, b: u8) {
        self.terms.push(b);
    }
    fn t32(&mut self, v: u32) {
        self.terms.extend_from_slice(&v.to_le_bytes());
    }
    fn tkey(&mut self, k: ItemKey) {
        self.terms.extend_from_slice(&k.0.to_le_bytes());
    }
    fn tchild(&mut self, t: PTy) -> Result<(), E> {
        let at = self.local(t)?;
        self.t32(at);
        Ok(())
    }
    fn tlist(&mut self, l: List) -> Result<(), E> {
        let ts = self.tys.list(l).map_err(|_| E::Outside { what: "a term's list" })?;
        let n = u32::try_from(ts.len()).map_err(|_| E::TooBig { what: "a term's list" })?;
        self.t32(n);
        for &t in ts {
            self.tchild(t)?;
        }
        Ok(())
    }

    fn write_node(&mut self, n: PNode) -> Result<(), E> {
        match n {
            PNode::Bool => self.t8(T_BOOL),
            PNode::Char => self.t8(T_CHAR),
            PNode::Int(i) => {
                self.t8(T_INT);
                self.t8(int_tag(i));
            }
            PNode::Uint(u) => {
                self.t8(T_UINT);
                self.t8(uint_tag(u));
            }
            PNode::Float(f) => {
                self.t8(T_FLOAT);
                self.t8(float_tag(f));
            }
            PNode::Str => self.t8(T_STR),
            PNode::Never => self.t8(T_NEVER),
            PNode::Tuple(l) => {
                self.t8(T_TUPLE);
                self.tlist(l)?;
            }
            PNode::Ref(m, t) => {
                self.t8(T_REF);
                self.t8(mutbl_tag(m));
                self.tchild(t)?;
            }
            PNode::Ptr(m, t) => {
                self.t8(T_PTR);
                self.t8(mutbl_tag(m));
                self.tchild(t)?;
            }
            PNode::Array(e, len) => {
                self.t8(T_ARRAY);
                self.tchild(e)?;
                self.tchild(len)?;
            }
            PNode::Slice(t) => {
                self.t8(T_SLICE);
                self.tchild(t)?;
            }
            PNode::Pat(base, ranges) => {
                self.t8(T_PAT);
                self.tchild(base)?;
                self.tlist(ranges)?;
            }
            PNode::Adt(k, l) => {
                self.t8(T_ADT);
                self.tkey(k);
                self.tlist(l)?;
            }
            PNode::FnDef(k, l) => {
                self.t8(T_FNDEF);
                self.tkey(k);
                self.tlist(l)?;
            }
            PNode::FnPtr(l, h) => {
                self.t8(T_FNPTR);
                self.t8(h.bits());
                self.tlist(l)?;
            }
            PNode::Param(i) => {
                self.t8(T_PARAM);
                self.t32(i);
            }
            PNode::Alias { trait_, name, args } => {
                self.t8(T_ALIAS);
                self.tkey(trait_);
                self.terms.extend_from_slice(&name.0.to_le_bytes());
                self.tlist(args)?;
            }
            PNode::Opaque(k, l) => {
                self.t8(T_OPAQUE);
                self.tkey(k);
                self.tlist(l)?;
            }
            PNode::Error => self.t8(T_ERROR),
            PNode::ConstValue { ty, bits } => {
                self.t8(T_CONST_VALUE);
                self.tchild(ty)?;
                self.terms.extend_from_slice(&bits.to_le_bytes());
            }
            PNode::ConstParam(i) => {
                self.t8(T_CONST_PARAM);
                self.t32(i);
            }
            PNode::Dynamic(l) => {
                self.t8(T_DYNAMIC);
                self.tlist(l)?;
            }
            PNode::ExTrait(k, l) => {
                self.t8(T_EX_TRAIT);
                self.tkey(k);
                self.tlist(l)?;
            }
            PNode::ExProj { trait_, name, args, term } => {
                self.t8(T_EX_PROJ);
                self.tkey(trait_);
                self.terms.extend_from_slice(&name.0.to_le_bytes());
                self.tlist(args)?;
                self.tchild(term)?;
            }
            PNode::ExAuto(k) => {
                self.t8(T_EX_AUTO);
                self.tkey(k);
            }
            PNode::Infer => return Err(E::Unencoded { what: "an inference hole `_` (a body's type, never a head's)" }),
            PNode::Closure { .. } => return Err(E::Unencoded { what: "a closure type (a body's, never a head's)" }),
            PNode::Future(..) => return Err(E::Unencoded { what: "an async block's future type (a body's, never a head's)" }),
            PNode::Coroutine { .. } => return Err(E::Unencoded { what: "an async block's future by identity (a body's hidden type, never a head's)" }),
        }
        Ok(())
    }
}

fn int_tag(i: IntTy) -> u8 {
    match i {
        IntTy::Isize => 0,
        IntTy::I8 => 1,
        IntTy::I16 => 2,
        IntTy::I32 => 3,
        IntTy::I64 => 4,
        IntTy::I128 => 5,
    }
}

fn uint_tag(u: UintTy) -> u8 {
    match u {
        UintTy::Usize => 0,
        UintTy::U8 => 1,
        UintTy::U16 => 2,
        UintTy::U32 => 3,
        UintTy::U64 => 4,
        UintTy::U128 => 5,
    }
}

fn float_tag(f: FloatTy) -> u8 {
    match f {
        FloatTy::F16 => 0,
        FloatTy::F32 => 1,
        FloatTy::F64 => 2,
        FloatTy::F128 => 3,
    }
}

fn mutbl_tag(m: tc::Mutbl) -> u8 {
    match m {
        tc::Mutbl::Not => 0,
        tc::Mutbl::Mut => 1,
    }
}

// ---------------------------------------------------------------------------------------------
// Reading.

/// The head in `bytes`, its terms interned into `tys` through the table's own constructors
/// (`intern_shared`, `intern_list_shared`, `dynamic_shared`), so equal structure is an equal
/// `PTy` there. Through a SHARED borrow: `tys` is a table over stable columns (heads.rs
/// `Store::new`), so a head is imported into a body's table while the body's check holds a
/// view of it (blaze-typeck `PTys`, live); a table over owned columns refuses
/// (`Malformed::SharedIntern`). A fn head's `text` is handed in and checked against the
/// record's digest of it; `alloc` holds the head's own lists and its lifetime part.
pub fn decode<'n>(bytes: &[u8], text: Option<&'n str>, tys: &PortableTys<'_>, alloc: Mem) -> Result<Head<'n>, HeadRecordError> {
    let mut r = In { bytes, at: 0, tys, locals: Vec::new_in(alloc), scratch: Vec::new_in(alloc), alloc };
    if r.take(4, "the magic")? != &MAGIC[..] {
        return Err(E::NotThisRecord);
    }
    let found = r.u32("the version")?;
    if found != VERSION {
        return Err(E::Version { found });
    }
    let kind = r.u8("the head's kind")?;
    let terms = r.count("the term table")?;
    for _ in 0..terms {
        r.node()?;
    }
    let head = match kind {
        K_FN => r.fn_head(text)?,
        K_ADT => r.adt()?,
        K_CTOR => {
            let key = r.key("a constructor's key")?;
            let adt = r.key("a constructor's ADT")?;
            let generics = r.u32("a constructor's generics")?;
            let inputs = r.many("a constructor's inputs", |r| r.ty("a constructor's input"))?;
            let output = r.ty("a constructor's output")?;
            Head::Ctor { key, sig: CtorSig { adt, generics, inputs, output } }
        }
        K_TRAIT => r.trait_()?,
        K_IMPL => r.impl_()?,
        K_ALIAS => {
            let key = r.lib("an alias's key")?;
            let generics = r.u32("an alias's generics")?;
            let ty = r.ty("an alias's type")?;
            let defaults = r.defaults()?;
            let const_params = r.const_params_own()?;
            Head::Alias { key, generics, ty, defaults, const_params }
        }
        K_ALIAS_REFUSED => {
            let key = r.lib("a refused alias's key")?;
            let n = r.count("a refused alias's why")?;
            let text = r.take(n, "a refused alias's why")?;
            let mut why = Vec::with_capacity_in(n, r.alloc);
            why.extend_from_slice(text);
            Head::AliasRefused { key, why }
        }
        K_FN_REFUSED => {
            let key = r.key("a refused fn's key")?;
            let n = r.count("a refused fn's why")?;
            let text = r.take(n, "a refused fn's why")?;
            let mut why = Vec::with_capacity_in(n, r.alloc);
            why.extend_from_slice(text);
            Head::FnRefused { key, why }
        }
        K_TRAIT_ALIAS => {
            let key = r.lib("a trait alias's key")?;
            let generics = r.u32("a trait alias's generics")?;
            let clauses = r.many("a trait alias's clauses", |r| r.clause())?;
            let traits = r.many("a trait alias's traits", |r| Ok((r.lib("a trait alias's trait")?, r.list("a trait alias's trait arguments")?)))?;
            Head::TraitAlias { key, head: TraitAliasHead { generics, clauses, traits } }
        }
        K_OPAQUE => {
            let key = r.key("an opaque type's key")?;
            let fn_ = r.key("an opaque type's fn")?;
            let index = r.u32("an opaque type's index")?;
            let preds = r.opt("an opaque type's bounds", |r| r.preds())?;
            Head::Opaque { key, fn_, index, preds }
        }
        byte => return Err(E::Byte { what: "the head's kind", byte }),
    };
    if r.at != bytes.len() {
        return Err(E::Trailing { at: r.at });
    }
    Ok(head)
}

/// The reader: the bytes, where it is, and the record's terms as interned so far.
struct In<'r, 't, 'a> {
    bytes: &'r [u8],
    at: usize,
    tys: &'t PortableTys<'a>,
    /// The record's term `i` is `locals[i]` in `tys`.
    locals: Vec<PTy, Mem>,
    scratch: Vec<PTy, Mem>,
    alloc: Mem,
}

impl<'r> In<'r, '_, '_> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'r [u8], E> {
        let bytes: &'r [u8] = self.bytes;
        let short = E::Short { what, at: self.at };
        let end = self.at.checked_add(n).ok_or(E::Short { what, at: self.at })?;
        let got = bytes.get(self.at..end).ok_or(short)?;
        self.at = end;
        Ok(got)
    }
    fn array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N], E> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N, what)?);
        Ok(a)
    }
    fn u8(&mut self, what: &'static str) -> Result<u8, E> {
        Ok(self.array::<1>(what)?[0])
    }
    fn u32(&mut self, what: &'static str) -> Result<u32, E> {
        Ok(u32::from_le_bytes(self.array(what)?))
    }
    fn u64(&mut self, what: &'static str) -> Result<u64, E> {
        Ok(u64::from_le_bytes(self.array(what)?))
    }
    fn u128(&mut self, what: &'static str) -> Result<u128, E> {
        Ok(u128::from_le_bytes(self.array(what)?))
    }
    fn flag(&mut self, what: &'static str) -> Result<bool, E> {
        match self.u8(what)? {
            0 => Ok(false),
            1 => Ok(true),
            byte => Err(E::Byte { what, byte }),
        }
    }
    fn lib(&mut self, what: &'static str) -> Result<LibKey, E> {
        Ok(LibKey(ContentId(self.array(what)?)))
    }
    fn key(&mut self, what: &'static str) -> Result<ItemKey, E> {
        Ok(ItemKey(self.u128(what)?))
    }
    fn name(&mut self, what: &'static str) -> Result<Name, E> {
        Ok(Name(self.u64(what)?))
    }
    /// A count, refused when the bytes left cannot hold that many entries of one byte each.
    fn count(&mut self, what: &'static str) -> Result<usize, E> {
        let at = self.at;
        let n = usize::try_from(self.u32(what)?).map_err(|_| E::TooBig { what })?;
        if n > self.bytes.len().saturating_sub(self.at) {
            return Err(E::Short { what, at });
        }
        Ok(n)
    }
    fn opt<T>(&mut self, what: &'static str, one: impl FnOnce(&mut Self) -> Result<T, E>) -> Result<Option<T>, E> {
        if self.flag(what)? { Ok(Some(one(self)?)) } else { Ok(None) }
    }
    /// `n` entries into a list of the head types' own kind (their allocator is theirs).
    fn many<T, C: Default + Extend<T>>(&mut self, what: &'static str, mut one: impl FnMut(&mut Self) -> Result<T, E>) -> Result<C, E> {
        let n = self.count(what)?;
        let mut c = C::default();
        for _ in 0..n {
            let v = one(self)?;
            c.extend(core::iter::once(v));
        }
        Ok(c)
    }
    /// `n` entries into a list in the reader's allocator.
    fn own<T>(&mut self, what: &'static str, mut one: impl FnMut(&mut Self) -> Result<T, E>) -> Result<Vec<T, Mem>, E> {
        let n = self.count(what)?;
        let mut v = Vec::with_capacity_in(n, self.alloc);
        for _ in 0..n {
            let x = one(self)?;
            v.push(x);
        }
        Ok(v)
    }
    /// A term reference: an index into the record's terms read so far.
    fn ty(&mut self, what: &'static str) -> Result<PTy, E> {
        let i = self.u32(what)?;
        let node = u32::try_from(self.locals.len()).map_err(|_| E::TooBig { what })?;
        let at = usize::try_from(i).map_err(|_| E::TooBig { what })?;
        self.locals.get(at).copied().ok_or(E::Order { what, node, child: i })
    }
    fn tys_into_scratch(&mut self, what: &'static str) -> Result<(), E> {
        let n = self.count(what)?;
        self.scratch.clear();
        for _ in 0..n {
            let t = self.ty(what)?;
            self.scratch.push(t);
        }
        Ok(())
    }
    /// A list, interned into the table.
    fn list(&mut self, what: &'static str) -> Result<List, E> {
        self.tys_into_scratch(what)?;
        self.tys.intern_list_shared(&self.scratch).map_err(E::Table)
    }
    fn intern(&mut self, n: PNode) -> Result<(), E> {
        let t = self.tys.intern_shared(n).map_err(E::Table)?;
        self.locals.push(t);
        Ok(())
    }

    /// One term of the record's table, interned, its children by earlier indices only.
    fn node(&mut self) -> Result<(), E> {
        const W: &str = "a term";
        let n = match self.u8("a term's tag")? {
            T_BOOL => PNode::Bool,
            T_CHAR => PNode::Char,
            T_INT => PNode::Int(int_of(self.u8("an integer type")?)?),
            T_UINT => PNode::Uint(uint_of(self.u8("an unsigned integer type")?)?),
            T_FLOAT => PNode::Float(match self.u8("a float type")? {
                0 => FloatTy::F16,
                1 => FloatTy::F32,
                2 => FloatTy::F64,
                3 => FloatTy::F128,
                byte => return Err(E::Byte { what: "a float type", byte }),
            }),
            T_STR => PNode::Str,
            T_NEVER => PNode::Never,
            T_TUPLE => PNode::Tuple(self.list("a tuple's elements")?),
            T_REF => {
                let m = mutbl_of(self.u8("a reference's mutability")?)?;
                PNode::Ref(m, self.ty(W)?)
            }
            T_PTR => {
                let m = mutbl_of(self.u8("a pointer's mutability")?)?;
                PNode::Ptr(m, self.ty(W)?)
            }
            T_ARRAY => {
                let e = self.ty(W)?;
                PNode::Array(e, self.ty(W)?)
            }
            T_SLICE => PNode::Slice(self.ty(W)?),
            T_PAT => {
                let base = self.ty(W)?;
                PNode::Pat(base, self.list("a pattern type's ranges")?)
            }
            T_ADT => {
                let k = self.key("an ADT's key")?;
                PNode::Adt(k, self.list("an ADT's arguments")?)
            }
            T_FNDEF => {
                let k = self.key("a fn item's key")?;
                PNode::FnDef(k, self.list("a fn item's arguments")?)
            }
            T_FNPTR => {
                let h = header_of(self.u8("a fn pointer's header")?)?;
                let l = self.list("a fn pointer's inputs and output")?;
                PNode::FnPtr(l, h)
            }
            T_PARAM => PNode::Param(self.u32("a parameter's index")?),
            T_ALIAS => {
                let trait_ = self.key("a projection's trait")?;
                let name = self.name("a projection's name")?;
                PNode::Alias { trait_, name, args: self.list("a projection's arguments")? }
            }
            T_OPAQUE => {
                let k = self.key("an opaque type's key")?;
                PNode::Opaque(k, self.list("an opaque type's arguments")?)
            }
            T_ERROR => PNode::Error,
            T_CONST_VALUE => {
                let ty = self.ty(W)?;
                PNode::ConstValue { ty, bits: self.u128("a const value's bits")? }
            }
            T_CONST_PARAM => PNode::ConstParam(self.u32("a const parameter's index")?),
            T_DYNAMIC => {
                // Through the table's own constructor, which keeps the one canonical form.
                self.tys_into_scratch("a trait object's predicates")?;
                let t = self.tys.dynamic_shared(&self.scratch).map_err(E::Table)?;
                self.locals.push(t);
                return Ok(());
            }
            T_EX_TRAIT => {
                let k = self.key("a trait object's principal")?;
                PNode::ExTrait(k, self.list("a principal's arguments")?)
            }
            T_EX_PROJ => {
                let trait_ = self.key("a trait object's projection trait")?;
                let name = self.name("a trait object's projection name")?;
                let args = self.list("a trait object's projection arguments")?;
                PNode::ExProj { trait_, name, args, term: self.ty(W)? }
            }
            T_EX_AUTO => PNode::ExAuto(self.key("a trait object's auto trait")?),
            byte => return Err(E::Byte { what: "a term's tag", byte }),
        };
        self.intern(n)
    }

    fn named_tys<C: Default + Extend<(Name, PTy)>>(&mut self, what: &'static str) -> Result<C, E> {
        self.many(what, |r| Ok((r.name(what)?, r.ty(what)?)))
    }
    fn named_keys<C: Default + Extend<(Name, ItemKey)>>(&mut self, what: &'static str) -> Result<C, E> {
        self.many(what, |r| Ok((r.name(what)?, r.key(what)?)))
    }
    fn defaults(&mut self) -> Result<Vec<Option<PTy>, Mem>, E> {
        self.own("parameter defaults", |r| r.opt("a parameter default", |r| r.ty("a parameter default")))
    }
    fn const_params_own(&mut self) -> Result<Vec<(u32, PTy), Mem>, E> {
        self.own("const parameters", |r| Ok((r.u32("a const parameter")?, r.ty("a const parameter's type")?)))
    }
    /// `Out::scope`'s fields, into the store's own lists.
    fn scope<P, C, B>(&mut self) -> Result<(P, C, B, SelfTy), E>
    where
        P: Default + Extend<(Name, u32)>,
        C: Default + Extend<(u32, PTy)>,
        B: Default + Extend<ParamBound>,
    {
        let params = self.many("a fn's parameters in scope", |r| Ok((r.name("a parameter's name")?, r.u32("a parameter's index")?)))?;
        let consts = self.many("const parameters", |r| Ok((r.u32("a const parameter")?, r.ty("a const parameter's type")?)))?;
        let bounds = self.many("a fn's parameter bounds", |r| {
            let param = r.u32("a bound's parameter")?;
            let trait_ = r.lib("a bound's trait")?;
            Ok(ParamBound { param, trait_, args: r.list("a parameter bound's arguments")? })
        })?;
        let self_ = match self.u8("a fn's `Self`")? {
            0 => SelfTy::None,
            1 => SelfTy::Trait(self.lib("a fn's `Self` trait")?),
            2 => {
                let t = self.ty("a fn's `Self` type")?;
                let tr = self.opt("`Self`'s trait", |r| Ok((r.lib("`Self`'s trait")?, r.list("`Self`'s trait arguments")?)))?;
                SelfTy::Ty(t, tr)
            }
            byte => return Err(E::Byte { what: "a fn's `Self`", byte }),
        };
        Ok((params, consts, bounds, self_))
    }
    fn clause(&mut self) -> Result<PClause, E> {
        Ok(match self.u8("a clause's tag")? {
            0 => {
                let trait_ = self.key("a trait clause's trait")?;
                PClause::Trait { trait_, args: self.list("a trait clause's arguments")? }
            }
            1 => {
                let trait_ = self.key("a projection clause's trait")?;
                let args = self.list("a projection clause's arguments")?;
                let name = self.name("a projection clause's name")?;
                PClause::Projection { trait_, args, name, term: self.ty("a projection clause's term")? }
            }
            2 => {
                let cond = self.key("a const clause's condition")?;
                PClause::Const { cond, args: self.list("a const clause's arguments")? }
            }
            byte => return Err(E::Byte { what: "a clause's tag", byte }),
        })
    }
    fn preds(&mut self) -> Result<Preds, E> {
        let clauses = self.many("where-clauses", |r| r.clause())?;
        let conds = self.many("`where const` conditions", |r| Ok(lf::ConstIdx(r.u32("a `where const` condition")?)))?;
        Ok(Preds { clauses, conds })
    }
    fn region(&mut self) -> Result<Region, E> {
        let tag = self.u8("a region's tag")?;
        let n = self.u32("a region's index")?;
        Ok(match tag {
            0 => Region::Static,
            1 => Region::Named(n),
            2 => Region::Anon(n),
            3 => Region::Hidden(n),
            byte => return Err(E::Byte { what: "a region's tag", byte }),
        })
    }
    /// [`Out::life`] read back.
    fn life<T>(&mut self, what: &'static str, one: impl FnOnce(&mut Self) -> Result<T, E>) -> Result<Option<Result<T, LifeRefusal>>, E> {
        Ok(match self.u8(what)? {
            0 => None,
            1 => Some(Ok(one(self)?)),
            2 => Some(Err(self.refusal()?)),
            byte => return Err(E::Byte { what, byte }),
        })
    }
    /// [`Out::refusal`] read back.
    fn refusal(&mut self) -> Result<LifeRefusal, E> {
        use LifeRefusal as R;
        let tag = self.u8("a lifetime refusal's rule")?;
        let a = self.u32("a lifetime refusal's first field")?;
        let b = self.u32("a lifetime refusal's second field")?;
        let at = blaze_surface::Range::new(a, b);
        Ok(match tag {
            0 => R::NoHead,
            1 => R::Text,
            2 => R::Form,
            3 => R::TypeTable,
            4 => R::UnknownAdt,
            5 => R::ParamCount { text: a, head: b },
            6 => R::FieldCount { text: a, head: b },
            7 => R::TypeNode,
            8 => R::Bound,
            9 => R::Unaligned { at },
            10 => R::Unresolved { at },
            11 => R::Alias { at },
            12 => R::DefaultWithPositions { at },
            13 => R::LifetimeArgs { at },
            14 => R::TwoLifetimesOneOrigin { at },
            15 => R::UnknownLifetime { at },
            16 => R::ElisionUndecided { at },
            17 => R::ElidedInField { at },
            18 => R::ObjectDefault { at },
            19 => R::SelfRegions { at },
            byte => return Err(E::Byte { what: "a lifetime refusal's rule", byte }),
        })
    }
    /// A trait's or an impl's lifetime part (form 10).
    fn container_life(&mut self) -> Result<Option<Result<ContainerLife, LifeRefusal>>, E> {
        self.life("a container's lifetime part", |r| {
            let lifetimes = r.u32("a container's lifetimes")?;
            let elided = r.u32("a container's elided lifetimes")?;
            let self_regions = r.own("a container's self regions", |r| r.region())?;
            let bounds = r.own("a container's lifetime bounds", |r| Ok((r.region()?, r.region()?)))?;
            let names = r.own("a container's lifetime names", |r| Ok((r.u64("a lifetime's name")?, r.u32("a lifetime's index")?)))?;
            Ok(ContainerLife { lifetimes, elided, self_regions, bounds, names })
        })
    }

    fn fn_head<'n>(&mut self, text: Option<&'n str>) -> Result<Head<'n>, E> {
        let key = self.key("a fn's key")?;
        let want_len = self.u32("a fn text's length")?;
        let want: [u8; 32] = self.array("a fn text's digest")?;
        let text = text.ok_or(E::NoText)?;
        if usize::try_from(want_len).ok() != Some(text.len()) || ContentId::of(TEXT_DOMAIN, text.as_bytes()).0 != want {
            return Err(E::Text { want_len, got_len: text.len() });
        }
        let lib = self.lib("a fn's library key")?;
        let container = self.opt("a fn's container", |r| r.key("a fn's container"))?;
        let parent = self.u32("a fn's parent generics")?;
        let own = self.u32("a fn's own generics")?;
        let declared = self.u32("a fn's declared generics")?;
        let self_param = self.flag("a fn's `self` parameter")?;
        let header = header_of(self.u8("a fn's header")?)?;
        let inputs = self.many("a fn's inputs", |r| r.ty("a fn's input"))?;
        let output = self.ty("a fn's output")?;
        let has_body = self.flag("a fn's body flag")?;
        let c_variadic = self.flag("a fn's C-variadic flag")?;
        let legacy_const_generics = self.many("a fn's legacy const generic positions", |r| r.u32("a legacy const generic position"))?;
        let module = self.lib("a fn's module")?;
        let (params, consts, bounds, self_) = self.scope()?;
        let self_assoc = self.named_tys("a fn's `Self` associated types")?;
        let head = FnHead {
            lib,
            text,
            container,
            parent,
            own,
            declared,
            self_param,
            header,
            c_variadic,
            legacy_const_generics,
            inputs,
            output,
            has_body,
            module,
            params,
            consts,
            bounds,
            self_,
            self_assoc,
        };
        let preds = self.opt("a fn's where-clauses", |r| r.preds())?;
        let static_ = self.opt("a static's mutability", |r| mutbl_of(r.u8("a static's mutability")?))?;
        let life = self.life("a fn's lifetime part", |r| {
            let regions = r.own("a fn's regions", |r| r.region())?;
            let ends = r.own("a fn's region ends", |r| r.u32("a region end"))?;
            let bounds = r.own("a fn's lifetime bounds", |r| Ok((r.region()?, r.region()?)))?;
            let container = r.u32("a fn's container lifetimes")?;
            let named = r.u32("a fn's named lifetimes")?;
            Ok(FnLife { regions, ends, bounds, container, named })
        })?;
        Ok(Head::Fn { key, head, preds, static_, life })
    }

    fn adt<'n>(&mut self) -> Result<Head<'n>, E> {
        let key = self.lib("an ADT's key")?;
        let kind = match self.u8("an ADT's kind")? {
            0 => tc::AdtKind::Struct,
            1 => tc::AdtKind::Enum,
            2 => tc::AdtKind::Union,
            byte => return Err(E::Byte { what: "an ADT's kind", byte }),
        };
        let generics = self.u32("an ADT's generics")?;
        // Both lists' types are the row's (inferred from it below), never named here.
        let variants = self.many("an ADT's variants", |r| {
            let key = r.key("a variant's key")?;
            let name = r.name("a variant's name")?;
            let ctor = match r.u8("a variant's constructor kind")? {
                0 => tc::CtorKind::Braces,
                1 => tc::CtorKind::Tuple,
                2 => tc::CtorKind::Unit,
                byte => return Err(E::Byte { what: "a variant's constructor kind", byte }),
            };
            let start = r.u32("a variant's first field")?;
            let len = r.u32("a variant's field count")?;
            Ok(tc::Variant { key, name, ctor, fields: List { start, len } })
        })?;
        let fields = self.many("an ADT's fields", |r| Ok(tc::Field { name: r.name("a field's name")?, ty: r.ty("a field's type")? }))?;
        let repr = self.repr()?;
        let valid = blaze_build::layout::ValidRange {
            start: self.opt("an ADT's valid range start", |r| r.u128("a valid range start"))?,
            end: self.opt("an ADT's valid range end", |r| r.u128("a valid range end"))?,
        };
        let args = self.flag("an ADT's `#[blaze::args]` mark")?;
        let discrs = self.many("an enum's discriminants", |r| Ok(i128::from_le_bytes(r.array("a discriminant")?)))?;
        let defaults = self.defaults()?;
        let const_params = self.const_params_own()?;
        let field_defaults = self.own("an ADT's field defaults", |r| r.opt("a field default", |r| r.key("a field default's key")))?;
        let preds = self.opt("an ADT's where-clauses", |r| r.preds())?;
        let life = self.life("an ADT's lifetime part", |r| {
            let lifetimes = r.u32("an ADT's lifetimes")?;
            let variances = r.own("an ADT's variances", |r| {
                let variance = match r.u8("a variance")? {
                    0 => Variance::Co,
                    1 => Variance::Contra,
                    2 => Variance::In,
                    byte => return Err(E::Byte { what: "a variance", byte }),
                };
                Ok(LifeVariance { variance, computed: r.flag("a variance's computed flag")? })
            })?;
            let bounds = r.own("an ADT's lifetime bounds", |r| Ok((r.region()?, r.region()?)))?;
            let fields = r.own("an ADT's field regions", |r| r.region())?;
            let field_ends = r.own("an ADT's field region ends", |r| r.u32("a field region end"))?;
            Ok(AdtLife { lifetimes, variances, bounds, fields, field_ends })
        })?;
        let fieldless = self.opt("a fieldless enum's variant count", |r| r.u32("a fieldless enum's variant count"))?;
        let row = AdtRow { kind, generics, variants, fields, repr, valid, discrs, args };
        for v in &row.variants {
            let end = v.fields.start.checked_add(v.fields.len);
            if end.and_then(|e| usize::try_from(e).ok()).is_none_or(|e| e > row.fields.len()) {
                return Err(E::Shape { what: "a variant's fields run past the ADT's" });
            }
        }
        if !field_defaults.is_empty() && field_defaults.len() != row.fields.len() {
            return Err(E::Shape { what: "an ADT's field defaults are not one per field" });
        }
        Ok(Head::Adt { key, row, defaults, const_params, field_defaults, preds, life, fieldless })
    }

    fn repr(&mut self) -> Result<ReprAttrs, E> {
        let bits = self.u8("an ADT's repr flags")?;
        if bits & !0x0f != 0 {
            return Err(E::Byte { what: "an ADT's repr flags", byte: bits });
        }
        let int = match self.u8("an ADT's integer repr")? {
            0 => None,
            1 => Some(IntRepr::Signed(int_of(self.u8("an integer repr")?)?)),
            2 => Some(IntRepr::Unsigned(uint_of(self.u8("an integer repr")?)?)),
            byte => return Err(E::Byte { what: "an ADT's integer repr", byte }),
        };
        let packed = self.opt("an ADT's `packed`", |r| r.u8("an ADT's `packed`"))?;
        let align = self.opt("an ADT's `align`", |r| r.u8("an ADT's `align`"))?;
        Ok(ReprAttrs {
            c: bits & 1 != 0,
            int,
            transparent: bits & 2 != 0,
            packed,
            align,
            simd: bits & 4 != 0,
            hide_niche: bits & 8 != 0,
        })
    }

    fn member(&mut self) -> Result<MemberFacts, E> {
        let name = self.name("a member's name")?;
        let shape = match self.u8("a member's shape")? {
            0 => MemberShape::Const,
            1 => MemberShape::Type { generic: self.flag("an associated type's generic flag")? },
            2 => {
                let receiver = match self.u8("a member fn's receiver")? {
                    0 => Receiver::Absent,
                    1 => Receiver::Value,
                    2 => Receiver::Ref,
                    3 => Receiver::RefMut,
                    4 => Receiver::Box,
                    5 => Receiver::Rc,
                    6 => Receiver::Arc,
                    7 => Receiver::PinOfPointer,
                    8 => Receiver::Other,
                    byte => return Err(E::Byte { what: "a member fn's receiver", byte }),
                };
                let own_non_lifetime_params = self.u32("a member fn's parameters")?;
                let bits = self.u8("a member fn's flags")?;
                if bits & !0x0f != 0 {
                    return Err(E::Byte { what: "a member fn's flags", byte: bits });
                }
                MemberShape::Fn(FnFacts {
                    receiver,
                    own_non_lifetime_params,
                    where_self_sized: bits & 1 != 0,
                    is_async: bits & 2 != 0,
                    opaque_return: bits & 4 != 0,
                    self_outside_receiver: bits & 8 != 0,
                })
            }
            3 => MemberShape::Unread(match self.u8("a member's unread form")? {
                0 => Unread::TypedReceiver,
                1 => Unread::SignatureType,
                byte => return Err(E::Byte { what: "a member's unread form", byte }),
            }),
            byte => return Err(E::Byte { what: "a member's shape", byte }),
        };
        Ok(MemberFacts { name, shape })
    }

    fn trait_<'n>(&mut self) -> Result<Head<'n>, E> {
        let key = self.lib("a trait's key")?;
        let generics = self.u32("a trait's generics")?;
        let defaults = self.many("parameter defaults", |r| r.opt("a parameter default", |r| r.ty("a parameter default")))?;
        let assoc = self.many("a trait's associated types", |r| r.name("an associated type's name"))?;
        let fns = self.named_keys("a trait's fns")?;
        let flags = tc::TraitFlags(self.u32("a trait's flags")?);
        let members = self.many("a trait's member facts", |r| r.member())?;
        let head = TraitHead { generics, defaults, assoc, fns, flags, members };
        let extra = self.opt("a trait's members part", |r| {
            let supertraits = r.many("a trait's supertrait clauses", |r| r.clause())?;
            let assoc = r.many("a trait's associated type rows", |r| {
                let name = r.name("an associated type's name")?;
                let item = r.key("an associated type's key")?;
                let default = r.opt("an associated type's default", |r| r.ty("an associated type's default"))?;
                Ok(tc::AssocTyDef { name, item, default })
            })?;
            let consts = r.named_keys("a trait's associated constants")?;
            Ok(TraitExtra { supertraits, assoc, consts })
        })?;
        let supers = self.own("a trait's supertraits", |r| Ok((r.lib("a supertrait")?, r.list("a supertrait's arguments")?)))?;
        let const_params = self.const_params_own()?;
        let preds = self.opt("a trait's where-clauses", |r| r.preds())?;
        let mut assoc_side = Vec::with_capacity_in(head.assoc.len(), self.alloc);
        for &name in &head.assoc {
            let generics = self.opt("an associated type's parameter count", |r| r.u32("an associated type's parameter count"))?;
            let where_ = self.opt("an associated type's outlives clauses", |r| {
                let n = r.count("an associated type's outlives clauses")?;
                let mut clauses = [Outlives { subject: Outlived::SelfTy, lifetime: Name(0) }; super::gat::MOST_OUTLIVES];
                let Some(kept) = clauses.get_mut(..n) else { return Err(E::Shape { what: "more outlives clauses than an associated type keeps" }) };
                for c in kept.iter_mut() {
                    let subject = match r.u8("an outlives clause's subject")? {
                        0 => Outlived::SelfTy,
                        1 => Outlived::Type(r.name("an outlives clause's type")?),
                        2 => Outlived::Lifetime(r.name("an outlives clause's lifetime")?),
                        byte => return Err(E::Byte { what: "an outlives clause's subject", byte }),
                    };
                    *c = Outlives { subject, lifetime: r.name("an outlives clause's bound")? };
                }
                Ok(AssocWhere::of(&clauses[..n]))
            })?;
            let consts = self.opt("an associated type's const parameters", |r| r.const_params_own())?;
            let bounds = self.opt("an associated type's item bounds", |r| r.preds())?;
            if bounds.is_some() && !extra.as_ref().is_some_and(|x| x.assoc.iter().any(|a| a.name == name)) {
                return Err(E::Shape { what: "an associated type's item bounds with no row in the trait's members part" });
            }
            assoc_side.push(AssocSide { name, generics, where_, consts, bounds });
        }
        let life = self.container_life()?;
        Ok(Head::Trait { key, head, extra, supers, const_params, preds, assoc_side, life })
    }

    fn impl_<'n>(&mut self) -> Result<Head<'n>, E> {
        let key = self.key("an impl's key")?;
        let generics = self.u32("an impl's generics")?;
        let self_ty = self.ty("an impl's self type")?;
        let trait_ = self.opt("an impl's trait", |r| r.key("an impl's trait"))?;
        let n = self.count("an impl's trait arguments")?;
        self.scratch.clear();
        self.scratch.push(self_ty);
        for _ in 0..n {
            let t = self.ty("an impl's trait argument")?;
            self.scratch.push(t);
        }
        let trait_args = match trait_ {
            None if n == 0 => tc::ArgsAfterSelf::NONE,
            None => return Err(E::Shape { what: "an inherent impl with trait arguments" }),
            Some(_) => {
                // The trait reference's whole list, `Self` first, split as the lowering does.
                let with_self = self.tys.intern_list_shared(&self.scratch).map_err(E::Table)?;
                tc::ArgsAfterSelf::split(self.tys.view(), with_self, self_ty)
                    .map_err(|_| E::Shape { what: "an impl's trait arguments do not follow its self type" })?
            }
        };
        let assoc = self.named_tys("an impl's associated types")?;
        let fns = self.named_keys("an impl's fns")?;
        let fn_self = self.many("an impl's fn `self` flags", |r| match r.u8("an impl's fn `self` flag")? {
            0 => Ok(false),
            1 => Ok(true),
            byte => Err(E::Byte { what: "an impl's fn `self` flag", byte }),
        })?;
        let consts = self.named_keys("an impl's constants")?;
        let polarity = match self.u8("an impl's polarity")? {
            0 => tc::ImplPolarity::Positive,
            1 => tc::ImplPolarity::Negative,
            2 => tc::ImplPolarity::Reservation,
            byte => return Err(E::Byte { what: "an impl's polarity", byte }),
        };
        let preds = self.opt("an impl's where-clauses", |r| r.preds())?;
        let frame = self.opt("an impl's members' frame", |r| {
            let module = r.lib("a frame's module")?;
            let generics = r.u32("a frame's generics")?;
            let (params, consts, bounds, self_) = r.scope()?;
            Ok(MemberFrame { module, generics, params, consts, bounds, self_ })
        })?;
        let head = ImplHead { generics, self_ty, trait_, trait_args, assoc, fns, fn_self, consts, polarity };
        if head.fn_self.len() != head.fns.len() {
            return Err(E::Shape { what: "an impl's fn `self` flags are not one per fn" });
        }
        let life = self.container_life()?;
        Ok(Head::Impl { key, head, preds, frame, life })
    }
}

fn int_of(b: u8) -> Result<IntTy, E> {
    Ok(match b {
        0 => IntTy::Isize,
        1 => IntTy::I8,
        2 => IntTy::I16,
        3 => IntTy::I32,
        4 => IntTy::I64,
        5 => IntTy::I128,
        byte => return Err(E::Byte { what: "an integer type", byte }),
    })
}

fn uint_of(b: u8) -> Result<UintTy, E> {
    Ok(match b {
        0 => UintTy::Usize,
        1 => UintTy::U8,
        2 => UintTy::U16,
        3 => UintTy::U32,
        4 => UintTy::U64,
        5 => UintTy::U128,
        byte => return Err(E::Byte { what: "an unsigned integer type", byte }),
    })
}

fn mutbl_of(b: u8) -> Result<tc::Mutbl, E> {
    match b {
        0 => Ok(tc::Mutbl::Not),
        1 => Ok(tc::Mutbl::Mut),
        byte => Err(E::Byte { what: "a mutability", byte }),
    }
}

/// The inverse of `FnPtrHeader::bits` (its one home, blaze-solve term.rs).
fn header_of(b: u8) -> Result<tc::FnPtrHeader, E> {
    tc::FnPtrHeader::from_bits(b).ok_or(E::Byte { what: "a fn header", byte: b })
}

// ---------------------------------------------------------------------------------------------
// Equality.

/// The first part in which `a` and `b` differ, or `None`: both heads' terms must be in the one
/// hash-consed table `tys` (equal structure is an equal `PTy` there), so a type is compared by
/// its `PTy` and a list by its elements (never by where it sits in the list column). A fn's
/// text is compared by its bytes. Across two tables, compare the encodings instead (they are
/// canonical).
pub fn same(a: HeadRef<'_, '_>, b: HeadRef<'_, '_>, tys: PTys<'_>) -> Option<&'static str> {
    macro_rules! need {
        ($c:expr, $what:literal) => {
            if !$c {
                return Some($what);
            }
        };
    }
    let l = |x: List, y: List| matches!((tys.list(x), tys.list(y)), (Ok(p), Ok(q)) if p == q);
    let clauses = |x: &[PClause], y: &[PClause]| {
        x.len() == y.len()
            && x.iter().zip(y).all(|(p, q)| match (*p, *q) {
                (PClause::Trait { trait_: t1, args: a1 }, PClause::Trait { trait_: t2, args: a2 }) => t1 == t2 && l(a1, a2),
                (
                    PClause::Projection { trait_: t1, args: a1, name: n1, term: e1 },
                    PClause::Projection { trait_: t2, args: a2, name: n2, term: e2 },
                ) => t1 == t2 && l(a1, a2) && n1 == n2 && e1 == e2,
                (PClause::Const { cond: c1, args: a1 }, PClause::Const { cond: c2, args: a2 }) => c1 == c2 && l(a1, a2),
                _ => false,
            })
    };
    let preds = |x: Option<&Preds>, y: Option<&Preds>| match (x, y) {
        (None, None) => true,
        (Some(p), Some(q)) => clauses(&p.clauses, &q.clauses) && p.conds.iter().map(|c| c.0).eq(q.conds.iter().map(|c| c.0)),
        _ => false,
    };
    let lib_lists = |x: &[(LibKey, List)], y: &[(LibKey, List)]| x.len() == y.len() && x.iter().zip(y).all(|(p, q)| p.0 == q.0 && l(p.1, q.1));
    match (a, b) {
        (HeadRef::Fn { key: k1, head: h1, preds: p1, static_: s1, life: l1 }, HeadRef::Fn { key: k2, head: h2, preds: p2, static_: s2, life: l2 }) => {
            need!(k1 == k2, "a fn's key");
            need!(h1.lib == h2.lib, "a fn's library key");
            need!(h1.text == h2.text, "a fn's text");
            need!(h1.container == h2.container, "a fn's container");
            need!((h1.parent, h1.own, h1.declared) == (h2.parent, h2.own, h2.declared), "a fn's generic counts");
            need!(h1.self_param == h2.self_param, "a fn's `self` parameter");
            need!(h1.header == h2.header, "a fn's header");
            need!(h1.inputs[..] == h2.inputs[..], "a fn's inputs");
            need!(h1.output == h2.output, "a fn's output");
            need!(h1.has_body == h2.has_body, "a fn's body flag");
            need!(h1.c_variadic == h2.c_variadic, "a fn's C-variadic flag");
            need!(h1.legacy_const_generics[..] == h2.legacy_const_generics[..], "a fn's legacy const generic positions");
            need!(h1.module == h2.module, "a fn's module");
            need!(h1.params[..] == h2.params[..], "a fn's parameters in scope");
            need!(h1.consts[..] == h2.consts[..], "a fn's const parameters");
            need!(
                h1.bounds.len() == h2.bounds.len()
                    && h1.bounds.iter().zip(&h2.bounds).all(|(x, y)| x.param == y.param && x.trait_ == y.trait_ && l(x.args, y.args)),
                "a fn's parameter bounds"
            );
            need!(self_same(h1.self_, h2.self_, &l), "a fn's `Self`");
            need!(h1.self_assoc[..] == h2.self_assoc[..], "a fn's `Self` associated types");
            need!(preds(p1, p2), "a fn's where-clauses");
            need!(s1 == s2, "a static's mutability");
            need!(
                life_same(l1, l2, |x, y| {
                    x.regions[..] == y.regions[..] && x.ends[..] == y.ends[..] && x.bounds[..] == y.bounds[..] && (x.container, x.named) == (y.container, y.named)
                }),
                "a fn's lifetime part"
            );
            None
        }
        (
            HeadRef::Adt { key: k1, row: r1, defaults: d1, const_params: c1, field_defaults: f1, preds: p1, life: l1, fieldless: e1 },
            HeadRef::Adt { key: k2, row: r2, defaults: d2, const_params: c2, field_defaults: f2, preds: p2, life: l2, fieldless: e2 },
        ) => {
            need!(e1 == e2, "a fieldless enum's variant count");
            need!(f1 == f2, "an ADT's field defaults");
            need!(k1 == k2, "an ADT's key");
            need!(r1.kind == r2.kind && r1.generics == r2.generics, "an ADT's kind or generics");
            need!(r1.variants[..] == r2.variants[..], "an ADT's variants");
            need!(r1.fields[..] == r2.fields[..], "an ADT's fields");
            need!(r1.repr == r2.repr && r1.valid == r2.valid, "an ADT's repr");
            need!(r1.args == r2.args, "an ADT's `#[blaze::args]` mark");
            need!(r1.discrs[..] == r2.discrs[..], "an enum's discriminants");
            need!(d1 == d2, "an ADT's parameter defaults");
            need!(c1 == c2, "an ADT's const parameters");
            need!(preds(p1, p2), "an ADT's where-clauses");
            need!(
                life_same(l1, l2, |x, y| {
                    x.lifetimes == y.lifetimes
                        && x.variances[..] == y.variances[..]
                        && x.bounds[..] == y.bounds[..]
                        && x.fields[..] == y.fields[..]
                        && x.field_ends[..] == y.field_ends[..]
                }),
                "an ADT's lifetime part"
            );
            None
        }
        (HeadRef::Ctor { key: k1, sig: s1 }, HeadRef::Ctor { key: k2, sig: s2 }) => {
            need!(k1 == k2 && s1.adt == s2.adt && s1.generics == s2.generics, "a constructor's keys or generics");
            need!(s1.inputs[..] == s2.inputs[..] && s1.output == s2.output, "a constructor's signature");
            None
        }
        (
            HeadRef::Trait { key: k1, head: h1, extra: x1, supers: u1, const_params: c1, preds: p1, assoc_side: s1, life: l1 },
            HeadRef::Trait { key: k2, head: h2, extra: x2, supers: u2, const_params: c2, preds: p2, assoc_side: s2, life: l2 },
        ) => {
            need!(life_same(l1, l2, container_same), "a trait's lifetime part");
            need!(h1.assoc.iter().all(|&n| s1.of(n) == s2.of(n)), "a trait's associated types' side rows");
            let item_of = |x: Option<&TraitExtra>, n: Name| x.and_then(|x| x.assoc.iter().find(|a| a.name == n)).map(|a| a.item);
            need!(
                h1.assoc.iter().all(|&n| preds(s1.bounds(n, item_of(x1, n)), s2.bounds(n, item_of(x2, n)))),
                "a trait's associated types' item bounds"
            );
            need!(k1 == k2 && h1.generics == h2.generics, "a trait's key or generics");
            need!(h1.defaults[..] == h2.defaults[..], "a trait's parameter defaults");
            need!(h1.assoc[..] == h2.assoc[..], "a trait's associated types");
            need!(h1.fns[..] == h2.fns[..], "a trait's fns");
            need!(h1.flags == h2.flags, "a trait's flags");
            need!(h1.members[..] == h2.members[..], "a trait's member facts");
            need!(
                match (x1, x2) {
                    (None, None) => true,
                    (Some(x), Some(y)) => clauses(&x.supertraits, &y.supertraits) && x.assoc[..] == y.assoc[..] && x.consts[..] == y.consts[..],
                    _ => false,
                },
                "a trait's members part"
            );
            need!(lib_lists(u1, u2), "a trait's supertraits");
            need!(c1 == c2, "a trait's const parameters");
            need!(preds(p1, p2), "a trait's where-clauses");
            None
        }
        (HeadRef::Impl { key: k1, head: h1, preds: p1, frame: m1, life: l1 }, HeadRef::Impl { key: k2, head: h2, preds: p2, frame: m2, life: l2 }) => {
            need!(life_same(l1, l2, container_same), "an impl's lifetime part");
            need!(
                match (m1, m2) {
                    (None, None) => true,
                    (Some(x), Some(y)) => {
                        x.module == y.module
                            && x.generics == y.generics
                            && x.params[..] == y.params[..]
                            && x.consts[..] == y.consts[..]
                            && x.bounds.len() == y.bounds.len()
                            && x.bounds.iter().zip(&y.bounds).all(|(a, b)| a.param == b.param && a.trait_ == b.trait_ && l(a.args, b.args))
                            && self_same(x.self_, y.self_, &l)
                    }
                    _ => false,
                },
                "an impl's members' frame"
            );
            need!(k1 == k2 && h1.generics == h2.generics, "an impl's key or generics");
            need!(h1.self_ty == h2.self_ty, "an impl's self type");
            need!(h1.trait_ == h2.trait_ && l(h1.trait_args.list(), h2.trait_args.list()), "an impl's trait");
            need!(h1.assoc[..] == h2.assoc[..], "an impl's associated types");
            need!(h1.fns[..] == h2.fns[..] && h1.fn_self[..] == h2.fn_self[..] && h1.consts[..] == h2.consts[..], "an impl's members");
            need!(h1.polarity == h2.polarity, "an impl's polarity");
            need!(preds(p1, p2), "an impl's where-clauses");
            None
        }
        (
            HeadRef::Alias { key: k1, generics: g1, ty: t1, defaults: d1, const_params: c1 },
            HeadRef::Alias { key: k2, generics: g2, ty: t2, defaults: d2, const_params: c2 },
        ) => {
            need!(k1 == k2 && g1 == g2 && t1 == t2, "an alias's head");
            need!(d1 == d2 && c1 == c2, "an alias's defaults or const parameters");
            None
        }
        (HeadRef::TraitAlias { key: k1, head: h1 }, HeadRef::TraitAlias { key: k2, head: h2 }) => {
            need!(k1 == k2 && h1.generics == h2.generics, "a trait alias's key or generics");
            need!(clauses(&h1.clauses, &h2.clauses), "a trait alias's clauses");
            need!(lib_lists(&h1.traits, &h2.traits), "a trait alias's traits");
            None
        }
        (HeadRef::Opaque { key: k1, fn_: f1, index: i1, preds: p1 }, HeadRef::Opaque { key: k2, fn_: f2, index: i2, preds: p2 }) => {
            need!(k1 == k2 && f1 == f2 && i1 == i2, "an opaque type's keys");
            need!(preds(p1, p2), "an opaque type's bounds");
            None
        }
        (HeadRef::AliasRefused { key: k1, why: w1 }, HeadRef::AliasRefused { key: k2, why: w2 }) => {
            need!(k1 == k2 && w1 == w2, "a refused alias's key and why");
            None
        }
        (HeadRef::FnRefused { key: k1, why: w1 }, HeadRef::FnRefused { key: k2, why: w2 }) => {
            need!(k1 == k2 && w1.same(w2), "a refused fn's key and why");
            None
        }
        _ => Some("the head's kind"),
    }
}

/// Whether two lifetime parts are one: both not made, both refused by one rule at one place,
/// or both made and `eq`.
fn life_same<T>(a: Option<Result<&T, LifeRefusal>>, b: Option<Result<&T, LifeRefusal>>, eq: impl Fn(&T, &T) -> bool) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Ok(x)), Some(Ok(y))) => eq(x, y),
        (Some(Err(x)), Some(Err(y))) => x == y,
        _ => false,
    }
}

fn container_same(x: &ContainerLife, y: &ContainerLife) -> bool {
    (x.lifetimes, x.elided) == (y.lifetimes, y.elided) && x.self_regions[..] == y.self_regions[..] && x.bounds[..] == y.bounds[..] && x.names[..] == y.names[..]
}
