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
//! A fn record checks the length and digest of the text its head depends on. For a
//! body-independent head this is the prefix before the body's opening brace. The reader
//! supplies the current item's complete text for body lowering.
//!
//! Refused by name ([`HeadRecordError::Unencoded`]), never skipped: an inference hole, a
//! closure type and an async block's future in a head's terms (body types, never a head's); an
//! alias whose head was refused (its why is a `Why`, not a record). Every lifetime part is in
//! the form (10): a fn's, an ADT's, a trait's and an impl's, made or refused, so an edit of a
//! lifetime bound alone moves the head's reader digest. A refused alias and a member refused
//! alone are records of their why's text (forms 12 and 13: `alias_gaps`, `fn_gaps`). An impl's const parameters are its frame's
//! (form 9), filed under the impl's library key by whoever holds it (the want).
//!
//! A head's REGIONS (form 22, D148: blaze-typeck region_facts.rs `HeadRegions`) are one
//! section after the kind's fields, the same for every kind: the lifetime parameters, each
//! column (a slot per region position of a declared type, in its pre-order) and the counts the
//! one counter (`regions::region_arity`) needed for the nominal nodes under them, so a record
//! is opened with no world: every column is laid again at open (`RegionCol::lay`), a column
//! one slot short or long is refused there, once (rule 76). Two digests read it: the reader
//! digest ([`reader_digest`]) leaves the section out, so a lifetime-only edit of a signature
//! moves no type check; the regions digest ([`regions_digest`]) holds it, and the borrow
//! verdict reads that. The reader of opened records is [`RegionTable`] / [`RegionReader`]
//! (`RegionFacts`).

use blaze_build::layout::{IntRepr, ReprAttrs};
use blaze_hash::HashMap;
use blaze_id::{ContentId, ItemKey as LibKey};
use blaze_lower::form as lf;
use blaze_own::{Alloc, Vec};
use blaze_surface::RecordBytes;
use blaze_typeck as tc;
use blaze_typeck::dyn_compat::{FnFacts, MemberFacts, MemberShape, Receiver, Unread};
use blaze_typeck::region_facts::{
    ClauseRegions, ClauseView, ColView, HeadRegions, Outlives as ROutlives, ParamsView, RegionFacts, RegionsUnread, SigRegions, UnreadWhy,
};
use blaze_typeck::regions::{Arity, Count, LifetimeParams, Region as RRegion, RegionCol, RegionError, each_slot_node};
use blaze_typeck::{FloatTy, IntTy, ItemKey, List, Name, PClause, PNode, PTy, PTys, PortableTys, UintTy};

use super::gat::{AssocWhere, Outlived, Outlives};
use super::heads::{AdtRow, CoherenceMarks, CtorSig, FnHead, ImplHead, MemberFrame, MemberVis, Store, TraitExtra, TraitHead, Want};
use super::keys::{checker_of, opaque_key};
use super::lifetimes::{AdtLife, ContainerLife, LifeOf, LifeRefusal, LifeVariance, Region, Variance};
use super::preds::Preds;
use super::rows::Rows;
use super::type_miss::{HeadMiss, MissOf};
use super::types::{AliasRegions, AliasSlot, NotLaidWhy, ParamBound, SelfTy, SigPart, TraitAliasHead};

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
/// 15: a trait's `supertraits` holds every clause its header implies (heads.rs `trait_members`,
/// preds.rs `predicates_implying`): the bounds on its associated types too (`Curve<A: B>`),
/// where before only clauses on `Self` were kept.
/// 16: an ADT's lifetime variances read its fn-pointer fields (lifetimes.rs `variances`, lane
/// B15): a lifetime in a fn pointer's input is `Contra`, where it was `In` (not computed).
/// 17: a fn text slot distinguishes a signature prefix from a whole item (lane CX17: a head
/// record checks its signature's text alone unless the fn's key holds its body).
/// 18: an opaque type term carries the defining head and its tagged owner/ordinal.
/// 19: nominal heads carry their coherence marks, traits say whether all parameters default
/// to `Self`, and ADTs carry their lifetime arity and the borrow checker's region positions
/// for each field, including substituted type arguments.
/// Function heads carry their cfg-expanded typing attributes, const qualifier and std-internal binding.
/// 21 (two lanes' changes, merged 2026-11-18; each had called its own form 20): an impl's
/// record holds one visibility per fn after its `self` flags (`ImplHead::fn_vis`, as written:
/// what method lookup's `World::member_visible` reads); and a trait's flags retain the presence
/// of lifetime parameters for impl selection at calls.
/// 22 (D148, piece L2): every record ends with its head's regions section (`HeadRegions`, the
/// module docs say how); a fn's form-10 lifetime part (`FnLife`) is gone from it, its regions
/// are that section's. An ADT's, a trait's and an impl's form-10 parts stay until piece L6.
/// The section holds, after an impl's header columns, its `type Name = U;` values' columns in
/// strict name order (`HeadRegions::impl_assoc`; added before form 22 shipped, so still 22).
/// A trait's record holds no lifetime count: a declared count is a fact of the item's text,
/// whose one home is its crate's items record (D155; the region walk reads it through the
/// names, types.rs `declared_lifetimes`); the counts form 22 carried here before it shipped
/// are gone (still 22: no record of the form with them shipped). A fn's record ends (after its static mutability) with its signature's E0412s
/// and E0405s (`HeadRef::Fn::misses`: each an item-relative range, its words and whether a
/// type or a bound's trait named nothing), which the fn's own body
/// check says from a store filed from the record (also before form 22 shipped). A type
/// alias's record ends with its type laid over its own lifetimes (`HeadRef::Alias::slots`,
/// `Maps::alias_slots`), what a path naming it lays its expansion by (also before 22 shipped).
/// 23: an impl fn's visibility byte tells `pub(in crate::a::b)` and `pub(in super::super)` by
/// depth (`MemberVis::InCrate`, `MemberVis::Up`, bytes 5 and up); 22 wrote 4 for both.
/// 24: a trait's or a trait impl's `-> impl Trait` captures every lifetime in scope in every
/// edition (heads.rs `fn_head`, `in_trait`): a 23 record of such a fn before 2024 left the
/// opaque's captures undecided and its regions refused (tempo's `fn sleep(&self, ..) -> impl
/// Future` rows, read from 23 records made before the fix).
/// 25: an opaque with a written `use<..>`, or a pre-2024 RPIT naming fewer than every lifetime
/// in scope, captures the listed lifetimes and its regions are laid (types.rs `Captures`); a 24
/// record of such a fn left them refused (commonware-runtime's `Clock::sleep(&self, ..) -> impl
/// Future + Send + 'static + use<Self>`: tempo's 485 "a region position the signature's walk
/// does not lay"). The format is unchanged; the content for the same key is not (rule 21).
/// 26: a qualified path `<T as Tr>::Name` whose arguments hold a lifetime is laid as the
/// projection (its own lifetimes, then its arguments' slots: types.rs `qualified_slots`), never
/// normalized; a 25 record of such a signature left its regions refused ("a projection whose
/// arguments or trait hold a lifetime"). Same format, new content for the same key (rule 21).
/// 27: an `async fn`'s future (opaque 0) files its regions (captures every in-scope lifetime,
/// heads.rs `async_future_filed`); a 26 record refused them ("an `async fn`'s future"), so its
/// `opaque_captures` was not known. Same format, new content for the same key (rule 21).
/// 28: a projection `T::Name` or `Self::Name` whose trait declares no lifetime is laid with
/// no bound looked up (types.rs `projection_slots`: no bound gives such a trait a slot), so
/// alloy-sol-types' `T::Token<'_>` under the container's `T: SolType` is laid; a 27 record of
/// such a signature left its regions refused ("a projection whose trait no bound on its
/// parameter in this item names"). A refusal of the regions is filed by its typed cause and
/// the signature part it was met in (`Unlaid::NotLaid { why, part }`). Same format, new
/// content for the same key (rule 21).
/// 29: `Self::Name` in a trait impl whose trait declares lifetimes is laid, the trait's
/// lifetimes read from the header's trait column (types.rs `projection_slots`,
/// `Through::SelfImpl`): serde's derived `impl<'de> Visitor<'de> for __Visitor<'de> { fn
/// visit_seq<A>(..) -> Result<Self::Value, A::Error> }`; a 28 record of such a member left its
/// regions refused (`NotLaid/ImplTraitLifetimes/Output`, tempo's 167). Same format, new content
/// for the same key (rule 21).
/// 30: a qualified path `<T as Tr<'a>>::Name` whose trait declares lifetimes is laid, the
/// trait's lifetimes those written on its last segment (none written: one elided each) ahead
/// of the alias's own and its arguments' (types.rs `qualified_slots`); a 29 record of such a
/// signature left its regions refused (`NotLaid/EarlySegmentLifetime/..`). Same format, new
/// content for the same key (rule 21).
/// 31: `Self::Name` in a trait impl whose trait declares lifetimes is laid in a member fn's
/// where clauses and its `impl Trait`'s bounds and in a member constant's type too (heads.rs
/// `SelfGiven`: every member walk is given the impl's trait column, not the signature's alone);
/// a 30 record of such a member left its regions refused (`NotLaid/ImplTraitLifetimes/..`).
/// Same format, new content for the same key (rule 21).
/// 32: the same in a trait impl's own `type X = ..;` values and its header's where clauses
/// (heads.rs `impl_`: the header's columns are laid first, heads_regions.rs
/// `impl_header_regions`, and those walks are given its trait column); a 31 record of such an
/// impl left its regions refused (`NotLaid/ImplTraitLifetimes/..`). Same format, new content
/// for the same key (rule 21).
/// 33: the term tag 26 (`T_FOREIGN`, an extern type `extern { type T; }` by its item) is new:
/// heads.rs `alias` files a type with no `= ..` as one (core's `Code`, the type of
/// `FnPtr::as_ptr`'s `NonNull<Code>`), where a 32 record refused it.
/// 34: an impl's record holds its unstable members after its fn visibilities
/// (`ImplHead::unstable`: each a name, its feature's text and an optional `implied_by` text;
/// what method and path lookup's `World::member_unstable` reads).
/// 35: `T_RECORD` terms carry their canonical sorted field names so cross-item signatures keep
/// `RecordSchema -> &[Name]` mappings.
/// 36: `PClause::ForFields` (tag 5) for field-quantified bounds `for<F in fields(T)> F: Trait<args...>`.
pub const VERSION: u32 = 36;
/// The digest domains: the record's out digest, a fn text's check, the regions digest.
const DIGEST_DOMAIN: &str = "blaze.head-record.v1";
const TEXT_DOMAIN: &str = "blaze.head-record.text.v1";
const REGIONS_DOMAIN: &str = "blaze.head-record.regions.v1";
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
const T_FOREIGN: u8 = 26;
const T_RECORD: u8 = 27;
const T_UNION: u8 = 28;

/// Where to read an opaque's bounds even when an unrelated head first exposes its type.
/// A trait method's checker key is `owner`; its bounds are in its trait's `Members` cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpaqueOrigin {
    pub want: Want,
    pub owner: ItemKey,
    pub index: u32,
}

impl OpaqueOrigin {
    /// The ordinal in the owner's syntax (the alias bit belongs only to the owner row).
    pub fn ordinal(self) -> u32 {
        self.index & !super::tait::ALIAS_OPAQUE
    }

    pub(crate) fn check(self, key: ItemKey) -> Result<(), HeadRecordError> {
        self.wire_tag(key).map(|_| ())
    }

    fn wire_tag(self, key: ItemKey) -> Result<u8, HeadRecordError> {
        let alias = super::tait::of_alias(self.index);
        let container = checker_of(self.want.key());
        if self.owner.0 >> 64 != container.0 >> 64 || opaque_key(self.owner, self.ordinal()) != key {
            return Err(E::InvalidOpaqueOrigin { key, origin: self });
        }
        match self.want {
            Want::Head(_) if alias && self.owner == container => Ok(1),
            Want::Fn(_) if !alias && self.owner == container => Ok(4),
            Want::Members(_) if !alias => Ok(5),
            Want::MemberFn(_, member) if !alias && self.owner == member => Ok(6),
            _ => Err(E::InvalidOpaqueOrigin { key, origin: self }),
        }
    }
}

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
    /// The current item does not match the fn's recorded text extent and digest.
    Text { key: ItemKey, want_len: u32, got_len: usize },
    /// The store has no such head made.
    NotMade { at: HeadAt },
    /// The caller supplied a term without the route to its defining row.
    MissingOpaqueOrigin { key: ItemKey },
    /// The stored owner has no defining cell in this snapshot.
    OpaqueOwner { key: ItemKey, owner: ItemKey, error: super::fill::FillError },
    /// The term's key, checker owner, tagged ordinal or defining want disagree.
    InvalidOpaqueOrigin { key: ItemKey, origin: OpaqueOrigin },
    /// A definition row does not have the key derived from its owner and ordinal.
    InvalidOpaqueRow { key: ItemKey, owner: ItemKey, index: u32 },
    /// Two references name different definitions for the same opaque key.
    ConflictingOpaqueOrigin { key: ItemKey, first: OpaqueOrigin, second: OpaqueOrigin },
    /// The defining head lacks the row or its predicates.
    MissingOpaqueRow { key: ItemKey, origin: OpaqueOrigin },
    /// The type table refused a term (its cap).
    Table(tc::CheckError),
    /// A column of the head's regions does not lie over its type (a slot short or long, a
    /// nominal node whose count the record does not hold), or its parameters refused an index:
    /// `what` the part, `error` the one home's reason (blaze-typeck regions.rs).
    Regions { what: &'static str, error: RegionError },
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
            HeadRecordError::Text { key, want_len, got_len } => {
                write!(f, "head record: fn {key:?}'s text differs from its recorded {want_len}-byte extent or digest (current item {got_len} bytes)")
            }
            HeadRecordError::NotMade { at } => write!(f, "head record: the store has no head made for {at:?}"),
            HeadRecordError::MissingOpaqueOrigin { key } => write!(f, "head record: no defining head for opaque type {:#034x}", key.0),
            HeadRecordError::OpaqueOwner { key, owner, error } => write!(f, "head record: opaque type {:#034x} has owner {owner:?}: {error}", key.0),
            HeadRecordError::InvalidOpaqueOrigin { key, origin } => write!(f, "head record: opaque type {:#034x} disagrees with {origin:?}", key.0),
            HeadRecordError::InvalidOpaqueRow { key, owner, index } => write!(f, "head record: opaque row {:#034x} disagrees with owner {owner:?} and tagged ordinal {index}", key.0),
            HeadRecordError::ConflictingOpaqueOrigin { key, first, second } => write!(f, "head record: opaque type {:#034x} has conflicting origins {first:?} and {second:?}", key.0),
            HeadRecordError::MissingOpaqueRow { key, origin } => write!(f, "head record: {:?} lacks the promised row for opaque type #{} ({:#034x}) of {:?}", origin.want, origin.ordinal(), key.0, origin.owner),
            HeadRecordError::Table(e) => write!(f, "head record: the type table refused a term: {e:?}"),
            HeadRecordError::Regions { what, error } => write!(f, "head record: {what}: {error} (D148)"),
        }
    }
}

type E = HeadRecordError;

fn check_opaque_row(key: ItemKey, owner: ItemKey, index: u32) -> Result<(), E> {
    if opaque_key(owner, index & !super::tait::ALIAS_OPAQUE) != key {
        return Err(E::InvalidOpaqueRow { key, owner, index });
    }
    Ok(())
}

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
    /// where-clauses and its regions (form 22; `None` when the store filed none), and its
    /// signature's written types that name nothing (`misses`, each the error type in `head`:
    /// the fn's own body check says them as E0412, front2 type_miss.rs [`HeadMiss`]).
    Fn { key: ItemKey, head: &'h FnHead<'n>, preds: Option<&'h Preds>, static_: Option<tc::Mutbl>, regions: Option<&'h FiledRegions>, misses: &'h [HeadMiss] },
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
        /// Its regions (form 22): its fields' columns among them.
        regions: Option<&'h FiledRegions>,
    },
    /// A tuple struct's or variant's constructor fn, and its regions (form 22).
    Ctor { key: ItemKey, sig: &'h CtorSig, regions: Option<&'h FiledRegions> },
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
        /// Its regions (form 22).
        regions: Option<&'h FiledRegions>,
    },
    /// An impl: its header, its where-clauses, its members' frame (form 9), its lifetime
    /// part (form 10) and its regions (form 22).
    Impl {
        key: ItemKey,
        head: &'h ImplHead,
        preds: Option<&'h Preds>,
        frame: Option<&'h MemberFrame>,
        life: Option<Result<&'h ContainerLife, LifeRefusal>>,
        regions: Option<&'h FiledRegions>,
    },
    /// A type alias whose head was made; `slots` its type laid over its own lifetimes
    /// (`Maps::alias_slots`), `None` where its head laid none.
    Alias { key: LibKey, generics: u32, ty: PTy, defaults: &'h [Option<PTy>], const_params: &'h [(u32, PTy)], slots: Option<&'h AliasRegions> },
    /// A type alias whose head was refused: its why's text as the store keeps it
    /// (`Maps::alias_gaps`), as bytes (UTF-8, checked where it is filed: heads.rs `file_heads`).
    AliasRefused { key: LibKey, why: &'h [u8] },
    TraitAlias { key: LibKey, head: &'h TraitAliasHead },
    /// An `impl Trait` a fn's head made: its fn and index, its bounds (`Self` the opaque), its
    /// regions (form 22: its captured-lifetime count among them).
    Opaque { key: ItemKey, fn_: ItemKey, index: u32, preds: Option<&'h Preds>, regions: Option<&'h FiledRegions> },
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
    pub bounds: Option<HeadPreds>,
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
            AssocSides::Rows(rows) => rows.iter().find(|r| r.name == name).and_then(|r| r.bounds.as_deref()),
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

impl<'h> HeadRef<'h, '_> {
    /// The head's regions (form 22), when it has them; a kind that holds none has `None`.
    pub fn regions(&self) -> Option<&'h FiledRegions> {
        match *self {
            HeadRef::Fn { regions, .. }
            | HeadRef::Adt { regions, .. }
            | HeadRef::Ctor { regions, .. }
            | HeadRef::Trait { regions, .. }
            | HeadRef::Impl { regions, .. }
            | HeadRef::Opaque { regions, .. } => regions,
            HeadRef::FnRefused { .. } | HeadRef::Alias { .. } | HeadRef::AliasRefused { .. } | HeadRef::TraitAlias { .. } => None,
        }
    }

    /// Whether its kind holds regions (D148): such a head is recorded with them laid or
    /// refused by name, never with neither (front2 tables/head.rs form 11).
    pub fn holds_regions(&self) -> bool {
        match *self {
            HeadRef::Fn { .. } | HeadRef::Adt { .. } | HeadRef::Ctor { .. } | HeadRef::Trait { .. } | HeadRef::Impl { .. } | HeadRef::Opaque { .. } => true,
            HeadRef::FnRefused { .. } | HeadRef::Alias { .. } | HeadRef::AliasRefused { .. } | HeadRef::TraitAlias { .. } => false,
        }
    }
}

/// A decoded head's clauses, in its allocator: kept out of the frames that decode, match and
/// file heads, as [`FiledRegions`] is (a `Preds` is ~250 bytes, in five variants).
pub type HeadPreds = blaze_own::Box<Preds, Mem>;

/// The most bytes a [`Head`] takes inline (its largest variant's own rows; its clauses and
/// regions are boxed). Every frame from `decode_retaining` up to `file_frozen` holds a few
/// heads by value, so this bounds the stack a head costs there. By count (aarch64, 32-byte
/// `LibKey`, 16-byte-aligned `u128`): the ADT and impl variants are ~520 bytes, the fn ~470,
/// the trait ~460; with the clauses and regions inline the ADT was ~1.4 KB.
pub const HEAD_MOST: usize = 768;
const _: () = assert!(size_of::<Head<'static>>() <= HEAD_MOST);
const _: () = assert!(size_of::<Option<HeadPreds>>() <= 16);

/// One head, decoded: the store's own head types, so it is usable where a lowered one is.
pub enum Head<'n> {
    Fn { key: ItemKey, head: FnHead<'n>, preds: Option<HeadPreds>, static_: Option<tc::Mutbl>, regions: Option<FiledRegions>, misses: Vec<HeadMiss, Mem> },
    Adt {
        key: LibKey,
        row: AdtRow,
        defaults: Vec<Option<PTy>, Mem>,
        const_params: Vec<(u32, PTy), Mem>,
        field_defaults: Vec<Option<ItemKey>, Mem>,
        preds: Option<HeadPreds>,
        life: Option<Result<AdtLife, LifeRefusal>>,
        fieldless: Option<u32>,
        regions: Option<FiledRegions>,
    },
    Ctor { key: ItemKey, sig: CtorSig, regions: Option<FiledRegions> },
    Trait {
        key: LibKey,
        head: TraitHead,
        extra: Option<TraitExtra>,
        supers: Vec<(LibKey, List), Mem>,
        const_params: Vec<(u32, PTy), Mem>,
        preds: Option<HeadPreds>,
        assoc_side: Vec<AssocSide, Mem>,
        life: Option<Result<ContainerLife, LifeRefusal>>,
        regions: Option<FiledRegions>,
    },
    Impl {
        key: ItemKey,
        head: ImplHead,
        preds: Option<HeadPreds>,
        frame: Option<MemberFrame>,
        life: Option<Result<ContainerLife, LifeRefusal>>,
        regions: Option<FiledRegions>,
    },
    Alias { key: LibKey, generics: u32, ty: PTy, defaults: Vec<Option<PTy>, Mem>, const_params: Vec<(u32, PTy), Mem>, slots: Option<AliasRegions> },
    /// Its why's text ([`HeadRef::AliasRefused`]).
    AliasRefused { key: LibKey, why: Vec<u8, Mem> },
    /// Its why's text ([`HeadRef::FnRefused`]).
    FnRefused { key: ItemKey, why: Vec<u8, Mem> },
    TraitAlias { key: LibKey, head: TraitAliasHead },
    Opaque { key: ItemKey, fn_: ItemKey, index: u32, preds: Option<HeadPreds>, regions: Option<FiledRegions> },
}

impl<'n> Head<'n> {
    /// The decoded head, borrowed as the encoder and [`same`] read one.
    pub fn as_ref(&self) -> HeadRef<'_, 'n> {
        match self {
            Head::Fn { key, head, preds, static_, regions, misses } => {
                HeadRef::Fn { key: *key, head, preds: preds.as_deref(), static_: *static_, regions: regions.as_ref(), misses: &misses[..] }
            }
            Head::Adt { key, row, defaults, const_params, field_defaults, preds, life, fieldless, regions } => HeadRef::Adt {
                key: *key,
                row,
                defaults: &defaults[..],
                const_params: &const_params[..],
                field_defaults: &field_defaults[..],
                preds: preds.as_deref(),
                life: life_ref(life),
                fieldless: *fieldless,
                regions: regions.as_ref(),
            },
            Head::Ctor { key, sig, regions } => HeadRef::Ctor { key: *key, sig, regions: regions.as_ref() },
            Head::Trait { key, head, extra, supers, const_params, preds, assoc_side, life, regions } => HeadRef::Trait {
                key: *key,
                head,
                extra: extra.as_ref(),
                supers: &supers[..],
                const_params: &const_params[..],
                preds: preds.as_deref(),
                assoc_side: AssocSides::Rows(&assoc_side[..]),
                life: life_ref(life),
                regions: regions.as_ref(),
            },
            Head::Impl { key, head, preds, frame, life, regions } => {
                HeadRef::Impl { key: *key, head, preds: preds.as_deref(), frame: frame.as_ref(), life: life_ref(life), regions: regions.as_ref() }
            }
            Head::Alias { key, generics, ty, defaults, const_params, slots } => HeadRef::Alias {
                key: *key,
                generics: *generics,
                ty: *ty,
                defaults: &defaults[..],
                const_params: &const_params[..],
                slots: slots.as_ref(),
            },
            Head::AliasRefused { key, why } => HeadRef::AliasRefused { key: *key, why: &why[..] },
            Head::FnRefused { key, why } => HeadRef::FnRefused { key: *key, why: GapWhy::Text(&why[..]) },
            Head::TraitAlias { key, head } => HeadRef::TraitAlias { key: *key, head },
            Head::Opaque { key, fn_, index, preds, regions } => {
                HeadRef::Opaque { key: *key, fn_: *fn_, index: *index, preds: preds.as_deref(), regions: regions.as_ref() }
            }
        }
    }

    /// Puts the regions read from the record's section on the head; a kind that holds none
    /// refuses them by name.
    fn set_regions(&mut self, filed: FiledRegions) -> Result<(), HeadRecordError> {
        match self {
            Head::Fn { regions, .. }
            | Head::Adt { regions, .. }
            | Head::Ctor { regions, .. }
            | Head::Trait { regions, .. }
            | Head::Impl { regions, .. }
            | Head::Opaque { regions, .. } => {
                *regions = Some(filed);
                Ok(())
            }
            Head::Alias { .. } | Head::AliasRefused { .. } | Head::FnRefused { .. } | Head::TraitAlias { .. } => {
                Err(E::Shape { what: "a regions section on a head kind that holds none (an alias, a trait alias or a refused head)" })
            }
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
/// filed for it (lifetimes.rs `Store::make_lives`; `None` when never made) and the regions its
/// lowering filed (`Store::regions`, form 22; `None` when none were filed).
pub fn from_store<'h, 'n>(store: &'h Store<'n>, at: HeadAt) -> Result<HeadRef<'h, 'n>, HeadRecordError> {
    let none = E::NotMade { at };
    let regions = store.regions.made(regions_key(at));
    let lives = store.lives_of(at);
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
            let misses = store.type_misses.get(&k).map_or(&[][..], |m| &m[..]);
            HeadRef::Fn { key: k, head, preds: store.preds.get(&k), static_: store.statics.get(&k).copied(), regions, misses }
        }
        HeadAt::Adt(lib) => {
            let ck = checker_of(lib);
            let row = store.adts.get(&ck).ok_or(none)?;
            let defaults = store.maps.generics.get(&lib).map_or(&[][..], |(_, d)| &d[..]);
            let const_params = store.maps.const_params.get(&lib).map_or(&[][..], |c| &c[..]);
            // Made with the row (`Store::adt`): a row without it is not this store's own.
            let field_defaults = store.field_defaults.get(&ck).ok_or(E::NotMade { at })?;
            let fieldless = store.maps.fieldless_enums.get(&ck).copied();
            HeadRef::Adt {
                key: lib,
                row,
                defaults,
                const_params,
                field_defaults: &field_defaults[..],
                preds: store.preds.get(&ck),
                life: adt_life,
                fieldless,
                regions,
            }
        }
        HeadAt::Ctor(k) => HeadRef::Ctor { key: k, sig: store.ctors.get(&k).ok_or(none)?, regions },
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
                regions,
            }
        }
        HeadAt::Impl(k) => HeadRef::Impl {
            key: k,
            head: store.impls.get(&k).ok_or(none)?,
            preds: store.preds.get(&k),
            frame: store.member_frame(k),
            life: container_life,
            regions,
        },
        HeadAt::Alias(lib) => {
            let Some((generics, ty)) = *store.maps.aliases.get(&lib).ok_or(none)? else {
                // Refused: the store keeps its why's text beside it (`Store::step`, `AliasGap`).
                let gap = store.maps.alias_gaps.get(&lib).ok_or(E::NotMade { at })?;
                return Ok(HeadRef::AliasRefused { key: lib, why: gap.text.as_bytes() });
            };
            let defaults = store.maps.alias_defaults.get(&lib).map_or(&[][..], |d| &d[..]);
            let const_params = store.maps.const_params.get(&lib).map_or(&[][..], |c| &c[..]);
            HeadRef::Alias { key: lib, generics, ty, defaults, const_params, slots: store.maps.alias_slots.get(&lib) }
        }
        HeadAt::TraitAlias(lib) => HeadRef::TraitAlias { key: lib, head: store.maps.trait_aliases.get(&lib).ok_or(none)? },
        HeadAt::Opaque(k) => {
            let &(fn_, index) = store.opaques.get(&k).ok_or(none)?;
            HeadRef::Opaque { key: k, fn_, index, preds: store.preds.get(&k), regions }
        }
    })
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

/// A head with no opaque references as a record. To encode the terms of a stored head
/// together with their defining cells, use [`encode_with_origins`].
pub fn encode<'b>(head: HeadRef<'_, '_>, tys: PTys<'_>, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, HeadRecordError> {
    encode_with_origins(head, tys, alloc, &missing_origin)
}

fn missing_origin(key: ItemKey) -> Result<OpaqueOrigin, E> {
    Err(E::MissingOpaqueOrigin { key })
}

/// Encode a stored head, resolving only opaque terms reached by its type walk.
pub fn encode_with_origins<'b>(
    head: HeadRef<'_, '_>, tys: PTys<'_>, alloc: Alloc<'b>,
    origin: &dyn Fn(ItemKey) -> Result<OpaqueOrigin, HeadRecordError>,
) -> Result<RecordBytes<'b>, HeadRecordError> {
    encode_with(head, tys, alloc, Text::Kept, Parts::WithRegions, origin)
}

/// What a head's READERS compare (the drive's `Signature` out, head_digest.rs `want_digest`):
/// [`digest`] of `head`'s record with a fn's text slot (its length and digest) zeroed, so the
/// out holds what the head MEANS (its terms, keys, names, counts and flags) and nothing of how
/// it is spelled. A fn's text is its whole item, spacing, comments and body included: it is in
/// the record only so [`decode`] can check the text it is handed (the body is lowered from
/// it), and a reader of the head never reads it. Its own body's record is keyed by that text.
/// (Before: a reformat of `fn f( _x : u8 )` moved every reader of `f`, blaze-cli test
/// `a_signature_edit_makes_its_head_and_its_readers_only`, 2026-11-17.) Never decoded.
///
/// The ERASED digest (form 22): the regions section is left out too, so a lifetime-only edit of
/// a signature (`-> &'a u8` to `-> &'b u8`) moves no reader that checks types; the borrow
/// verdict reads [`regions_digest`] beside it.
pub fn reader_digest(head: HeadRef<'_, '_>, tys: PTys<'_>, alloc: Alloc<'_>) -> Result<ContentId, HeadRecordError> {
    reader_digest_with_origins(head, tys, alloc, &missing_origin)
}

/// The semantic digest over the same types and defining cells as the record writer.
pub fn reader_digest_with_origins(
    head: HeadRef<'_, '_>, tys: PTys<'_>, alloc: Alloc<'_>,
    origin: &dyn Fn(ItemKey) -> Result<OpaqueOrigin, HeadRecordError>,
) -> Result<ContentId, HeadRecordError> {
    let bytes = encode_with(head, tys, alloc, Text::Zeroed, Parts::Erased, origin)?;
    Ok(digest(&bytes))
}

/// The REGIONS digest (form 22, D148): [`reader_digest`]'s bytes with the head's regions
/// section kept, under its own domain. A lifetime-only edit moves it and not the erased digest;
/// any edit that moves the erased digest moves it too (it holds every byte that one does), so
/// a reader keyed by it alone never misses a type change. Read by the borrow verdict; type
/// checks read the erased digest. Never decoded.
pub fn regions_digest(head: HeadRef<'_, '_>, tys: PTys<'_>, alloc: Alloc<'_>) -> Result<ContentId, HeadRecordError> {
    regions_digest_with_origins(head, tys, alloc, &missing_origin)
}

/// [`regions_digest`] over the same types and defining cells as the record writer.
pub fn regions_digest_with_origins(
    head: HeadRef<'_, '_>, tys: PTys<'_>, alloc: Alloc<'_>,
    origin: &dyn Fn(ItemKey) -> Result<OpaqueOrigin, HeadRecordError>,
) -> Result<ContentId, HeadRecordError> {
    let bytes = encode_with(head, tys, alloc, Text::Zeroed, Parts::WithRegions, origin)?;
    Ok(ContentId::of(REGIONS_DOMAIN, &bytes))
}

/// Whether a record holds the head's regions section (the record itself, the regions digest)
/// or leaves it out (the erased digest).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Parts {
    WithRegions,
    Erased,
}

/// Whether a fn's text slot holds its text's length and digest, or zeros of the same width.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Text {
    Kept,
    Zeroed,
}

/// A prefix ends immediately before the brace recorded by the parser. Checking the brace
/// also rejects a longer signature that begins with all the bytes of the old one.
fn fn_record_text(text: &str, prefix: Option<u32>) -> Option<&[u8]> {
    let bytes = text.as_bytes();
    match prefix {
        None => Some(bytes),
        Some(end) => {
            let (signature, body) = bytes.split_at_checked(end as usize)?;
            (body.first() == Some(&b'{')).then_some(signature)
        }
    }
}

fn encode_with<'b>(
    head: HeadRef<'_, '_>, tys: PTys<'_>, alloc: Alloc<'b>, slot: Text, parts: Parts, origin: &dyn Fn(ItemKey) -> Result<OpaqueOrigin, E>,
) -> Result<RecordBytes<'b>, HeadRecordError> {
    // A section that does not lie over its head is the producer's bug, refused before any
    // byte is written (the reader checks the same at open).
    if let Some(r) = head.regions() {
        regions_fit_head(head, r.regions(), tys)?;
    }
    let mut o = Out {
        tys,
        parts,
        origin,
        terms: RecordBytes::new_in(alloc),
        body: RecordBytes::new_in(alloc),
        seen: HashMap::new_in(alloc),
        count: 0,
        stack: Vec::new_in(alloc),
        kids: Vec::new_in(alloc),
    };
    let kind = match head {
        HeadRef::Fn { key, head, preds, static_, regions: _, misses } => {
            o.key(key);
            match slot {
                Text::Kept => {
                    let text = fn_record_text(head.text, head.text_prefix).ok_or(E::Shape { what: "a fn text prefix must end at the body's opening brace" })?;
                    let len = u32::try_from(text.len()).map_err(|_| E::TooBig { what: "a fn's text" })?;
                    o.flag(head.text_prefix.is_some());
                    o.u32(len);
                    o.body.extend_from_slice(&ContentId::of(TEXT_DOMAIN, text).0);
                }
                Text::Zeroed => {
                    o.flag(false);
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
            o.byte(head.qualifiers.byte());
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
            // The signature's E0412s and E0405s, in text order: item-relative
            // ranges and the words (type_miss.rs, from the names alone: deterministic).
            o.count(misses.len(), "a fn's signature misses")?;
            for m in misses {
                o.u32(m.at.start);
                o.u32(m.at.end);
                o.count(m.words.len(), "a signature miss's words")?;
                o.body.extend_from_slice(m.words.as_bytes());
                // What it failed to name: 0 a type (E0412), 1 a bound's trait (E0405).
                o.byte(match m.of {
                    MissOf::Type => 0,
                    MissOf::Trait => 1,
                });
            }
            K_FN
        }
        HeadRef::Adt { key, row, defaults, const_params, field_defaults, preds, life, fieldless, regions: _ } => {
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
            o.coherence_marks(row.marks);
            o.u32(row.lifetimes);
            o.field_regions(row)?;
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
                if l.lifetimes != row.lifetimes {
                    return Err(E::Shape { what: "an ADT's lifetime analysis disagrees with its declared arity" });
                }
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
        HeadRef::Ctor { key, sig, regions: _ } => {
            o.key(key);
            o.key(sig.adt);
            o.u32(sig.generics);
            o.tys_of(&sig.inputs, "a constructor's inputs")?;
            o.ty(sig.output)?;
            K_CTOR
        }
        HeadRef::Trait { key, head, extra, supers, const_params, preds, assoc_side, life, regions: _ } => {
            o.lib(key);
            o.u32(head.generics);
            o.defaults(&head.defaults)?;
            o.count(head.assoc.len(), "a trait's associated types")?;
            for &n in &head.assoc {
                o.name(n);
            }
            o.named_keys(&head.fns, "a trait's fns")?;
            o.u32(head.flags.0);
            o.coherence_marks(head.marks);
            o.flag(head.all_self_defaults);
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
        HeadRef::Impl { key, head, preds, frame, life, regions: _ } => {
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
            // Form 20: one visibility per fn, as written.
            o.count(head.fn_vis.len(), "an impl's fn visibilities")?;
            for &vis in head.fn_vis.iter() {
                o.byte(vis.byte());
            }
            // Form 34: an inherent impl's unstable members (`ImplHead::unstable`).
            o.count(head.unstable.len(), "an impl's unstable members")?;
            for (name, u) in head.unstable.iter() {
                o.name(*name);
                o.text(&u.feature, "an unstable member's feature")?;
                o.opt(u.implied_by.as_deref(), |o, by| o.text(by, "an unstable member's `implied_by`"))?;
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
        HeadRef::Alias { key, generics, ty, defaults, const_params, slots } => {
            o.lib(key);
            o.u32(generics);
            o.ty(ty)?;
            o.defaults(defaults)?;
            o.const_params(const_params)?;
            // Its laid regions: the lifetime count, then each slot (0 a lifetime and its
            // index, 1 `'static`, 2 a bound one and its binder and variable).
            o.opt(slots, |o, l| {
                o.u32(l.lifetimes);
                o.count(l.slots.len(), "an alias's region slots")?;
                for &s in l.slots.iter() {
                    match s {
                        AliasSlot::Lifetime(i) => {
                            o.byte(0);
                            o.u32(i);
                        }
                        AliasSlot::Static => o.byte(1),
                        AliasSlot::Late { binder, var } => {
                            o.byte(2);
                            o.u32(binder);
                            o.u32(var);
                        }
                        AliasSlot::ObjectDefault => o.byte(3),
                    }
                }
                Ok(())
            })?;
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
        HeadRef::Opaque { key, fn_, index, preds, regions: _ } => {
            check_opaque_row(key, fn_, index)?;
            o.key(key);
            o.key(fn_);
            o.u32(index);
            o.opt(preds, |o, p| o.preds(p))?;
            K_OPAQUE
        }
    };
    // Form 22: the regions section, after every kind's fields; left out of the erased digest.
    if parts == Parts::WithRegions {
        o.opt(head.regions(), |o, r| o.regions_section(r))?;
    }
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
struct Out<'b, 't, 'o> {
    tys: PTys<'t>,
    /// Whether region bytes are written (the record, the regions digest) or left out (the
    /// erased digest): the regions section and an outlives clause's regions alike.
    parts: Parts,
    origin: &'o dyn Fn(ItemKey) -> Result<OpaqueOrigin, E>,
    terms: RecordBytes<'b>,
    body: RecordBytes<'b>,
    /// Each term written, by its index in `tys`, to its index in the record's table.
    seen: HashMap<PTy, u32, Alloc<'b>>,
    count: u32,
    /// The walk's work list: a term, and whether its children are already pushed.
    stack: Vec<(PTy, bool), Alloc<'b>>,
    kids: Vec<PTy, Alloc<'b>>,
}

impl Out<'_, '_, '_> {
    fn byte(&mut self, b: u8) {
        self.body.push(b);
    }
    /// A text: its length, then its bytes ([`In::text`]).
    fn text(&mut self, s: &str, what: &'static str) -> Result<(), E> {
        self.count(s.len(), what)?;
        self.body.extend_from_slice(s.as_bytes());
        Ok(())
    }
    fn flag(&mut self, b: bool) {
        self.body.push(u8::from(b));
    }
    fn coherence_marks(&mut self, marks: CoherenceMarks) {
        self.byte(u8::from(marks.fundamental) | (u8::from(marks.incoherent) << 1));
    }
    fn field_regions(&mut self, row: &AdtRow) -> Result<(), E> {
        use blaze_flow_lower::FieldRegion;

        let regions = &row.field_regions;
        if regions.at.len() != row.fields.len() {
            return Err(E::Shape { what: "an ADT needs one region row per field" });
        }
        // Store a count per field. The decoder constructs the offsets, so a record cannot
        // point a field into a sibling's regions. An absent row differs from a known empty one.
        let mut taken = 0usize;
        for &(start, len) in &regions.at {
            if (start, len) == super::borrows::NO_ROW {
                self.flag(false);
                continue;
            }
            let end = taken.checked_add(len as usize).ok_or(E::TooBig { what: "an ADT's field regions" })?;
            if start as usize != taken || end > regions.list.len() {
                return Err(E::Shape { what: "an ADT's field region ranges are not contiguous" });
            }
            self.flag(true);
            self.u32(len);
            for &region in &regions.list[taken..end] {
                match region {
                    FieldRegion::Static => self.byte(0),
                    FieldRegion::Lifetime(i) => {
                        self.byte(1);
                        self.u32(i);
                    }
                    FieldRegion::Arg(i) => {
                        self.byte(2);
                        self.u32(i);
                    }
                    FieldRegion::Bound(i) => {
                        self.byte(3);
                        self.u32(i);
                    }
                }
            }
            taken = end;
        }
        if taken != regions.list.len() {
            return Err(E::Shape { what: "field regions left over after the ADT's last field" });
        }
        Ok(())
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
                // The regions are written in the record and the regions digest only: the erased
                // digest leaves them out, as `ContentHash` does (a lifetime-only edit moves no
                // type check).
                PClause::TypeOutlives { ty, region } => {
                    self.byte(3);
                    self.ty(ty)?;
                    if self.parts == Parts::WithRegions {
                        self.rregion(region);
                    }
                }
                PClause::RegionOutlives { long, short } => {
                    self.byte(4);
                    if self.parts == Parts::WithRegions {
                        self.rregion(long);
                        self.rregion(short);
                    }
                }
                PClause::ForFields { container, trait_, args } => {
                    self.byte(5);
                    self.ty(container)?;
                    self.key(trait_);
                    self.list(args, "a for-fields clause's arguments")?;
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
    /// The regions section (form 22): the arity table, the parameters, then each part in
    /// `HeadRegions`' field order. Every list is in the order the head gives it (clauses by
    /// `nth`, fields by variant then field, the table by key: checked when filed), so equal
    /// heads write equal bytes.
    fn regions_section(&mut self, filed: &FiledRegions) -> Result<(), E> {
        let a = &filed.parts.arity;
        self.count(a.adts.len(), "a head's region arity: ADTs")?;
        for &(k, n) in a.adts.iter() {
            self.key(k);
            self.u32(n);
        }
        self.count(a.opaques.len(), "a head's region arity: opaque types")?;
        for &(k, n) in a.opaques.iter() {
            self.key(k);
            self.u32(n);
        }
        self.count(a.aliases.len(), "a head's region arity: projections")?;
        for &(k, name, n) in a.aliases.iter() {
            self.key(k);
            self.name(name);
            self.u32(n);
        }
        self.count(a.traits.len(), "a head's region arity: traits")?;
        for &(k, n) in a.traits.iter() {
            self.key(k);
            self.u32(n);
        }
        let r = filed.regions();
        self.rparams(ParamsView::of(&r.params))?;
        self.opt(r.sig.as_ref(), |o, c| o.rcol(ColView::of(c), "a head's signature column"))?;
        self.opt(r.written_output.as_ref(), |o, c| o.rcol(ColView::of(c), "a head's written output column"))?;
        self.count(r.clauses.len(), "a head's clause columns")?;
        for c in r.clauses.iter() {
            self.u32(c.nth);
            self.u32(c.binders);
            self.rcol(ColView::of(&c.col), "a clause's column")?;
        }
        self.count(r.outlives.len(), "a head's outlives clauses")?;
        for &c in r.outlives.iter() {
            match c {
                ROutlives::Type { clause, region } => {
                    self.byte(0);
                    self.u32(clause);
                    self.rregion(region);
                }
                ROutlives::Region { long, short } => {
                    self.byte(1);
                    self.rregion(long);
                    self.rregion(short);
                }
            }
        }
        self.count(r.fields.len(), "a head's field columns")?;
        for (variant, field, col) in r.fields.iter() {
            self.u32(*variant);
            self.u32(*field);
            self.rcol(ColView::of(col), "a field's column")?;
        }
        self.opt(r.impl_header.as_ref(), |o, (self_, trait_)| {
            o.rcol(ColView::of(self_), "an impl's self type column")?;
            o.opt(trait_.as_ref(), |o, t| o.rcol(ColView::of(t), "an impl's trait reference column"))
        })?;
        self.count(r.impl_assoc.len(), "an impl's associated type columns")?;
        for (name, col) in r.impl_assoc.iter() {
            self.name(*name);
            self.rcol(ColView::of(col), "an impl's associated type column")?;
        }
        self.opt(r.opaque_captures, |o, n| {
            o.u32(n);
            Ok(())
        })
    }
    /// A head's lifetime parameters: the count, the late-bound ones by index (ascending), the
    /// declared outlives pairs in the order declared.
    fn rparams(&mut self, p: ParamsView<'_>) -> Result<(), E> {
        let count = p.count();
        self.u32(count);
        let mut late = 0u32;
        for i in 0..count {
            if p.is_late(i).map_err(|error| E::Regions { what: "a head's lifetime parameters", error })? {
                late += 1;
            }
        }
        self.u32(late);
        for i in 0..count {
            if p.is_late(i).map_err(|error| E::Regions { what: "a head's lifetime parameters", error })? {
                self.u32(i);
            }
        }
        self.count(p.outlives().len(), "a head's lifetime outlives pairs")?;
        for &(long, short) in p.outlives() {
            self.rregion(long);
            self.rregion(short);
        }
        Ok(())
    }
    /// A column: its type (a term of the record), its slot count, each slot.
    fn rcol(&mut self, c: ColView<'_>, what: &'static str) -> Result<(), E> {
        self.ty(c.ty())?;
        self.count(c.as_slice().len(), what)?;
        for &r in c.as_slice() {
            self.rregion(r);
        }
        Ok(())
    }
    fn rregion(&mut self, r: RRegion) {
        match r {
            RRegion::Static => self.byte(0),
            RRegion::Param(i) => {
                self.byte(1);
                self.u32(i);
            }
            RRegion::Late { binder, var } => {
                self.byte(2);
                self.u32(binder);
                self.u32(var);
            }
        }
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
            | PNode::ExAuto(_)
            | PNode::Foreign(_) => {}
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
            | PNode::Record(_, l)
            | PNode::Union(l)
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
            PNode::Record(s, l) => {
                self.t8(T_RECORD);
                self.terms.extend_from_slice(&s.0.to_le_bytes());
                let names = self.tys.record_names(s).unwrap_or(&[]);
                let n = u32::try_from(names.len()).map_err(|_| E::TooBig { what: "a record's field names" })?;
                self.t32(n);
                for &nm in names {
                    self.terms.extend_from_slice(&nm.0.to_le_bytes());
                }
                self.tlist(l)?;
            }
            PNode::Union(l) => {
                self.t8(T_UNION);
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
                let origin = (self.origin)(k)?;
                let tag = origin.wire_tag(k)?;
                self.t8(T_OPAQUE);
                self.tkey(k);
                // The checker owner is also the member key of a MemberFn want.
                self.t8(tag);
                self.terms.extend_from_slice(&origin.want.key().0.0);
                self.tkey(origin.owner);
                self.t32(origin.index);
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
            PNode::Foreign(k) => {
                self.t8(T_FOREIGN);
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
/// This entry point is for records with no opaque references. Use [`decode_retaining`] to
/// read any head on its own or [`decode_with_origins`] to import it into a store.
pub fn decode<'n>(bytes: &[u8], text: Option<&'n str>, tys: &PortableTys<'_>, alloc: Mem) -> Result<Head<'n>, HeadRecordError> {
    decode_with_origins(bytes, text, tys, alloc, &mut |key, _| Err(E::MissingOpaqueOrigin { key }))
}

/// Deliver each distinct origin by key only after checking the complete record, before
/// returning a head through which the caller can see the terms. Importers stage these
/// registrations until every record of the frozen cell has passed its checks.
pub fn decode_with_origins<'n>(
    bytes: &[u8],
    text: Option<&'n str>,
    tys: &PortableTys<'_>,
    alloc: Mem,
    register: &mut dyn FnMut(ItemKey, OpaqueOrigin) -> Result<(), HeadRecordError>,
) -> Result<Head<'n>, HeadRecordError> {
    decode_with_origins_boxed(bytes, text, tys, alloc, register).map(blaze_own::Box::into_inner)
}

/// As [`decode_with_origins`], keeping the decoded [`Head`] boxed in `alloc` to avoid placing
/// a large enum value on the stack during small-stack body checks.
pub fn decode_with_origins_boxed<'n>(
    bytes: &[u8],
    text: Option<&'n str>,
    tys: &PortableTys<'_>,
    alloc: Mem,
    register: &mut dyn FnMut(ItemKey, OpaqueOrigin) -> Result<(), HeadRecordError>,
) -> Result<blaze_own::Box<Head<'n>, Mem>, HeadRecordError> {
    let record = decode_retaining(bytes, text, tys, alloc)?;
    for &(key, origin) in record.origins.iter() {
        register(key, origin)?;
    }
    Ok(record.head)
}

/// A standalone head and its origin table. The table belongs to this record, so writing it
/// again needs only the table in which its terms were interned.
pub struct DecodedHead<'n> {
    head: blaze_own::Box<Head<'n>, Mem>,
    origins: Vec<(ItemKey, OpaqueOrigin), Mem>,
}

impl<'n> DecodedHead<'n> {
    pub fn as_ref(&self) -> HeadRef<'_, 'n> {
        (*self.head).as_ref()
    }

    pub fn encode<'b>(&self, tys: PTys<'_>, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, HeadRecordError> {
        encode_with_origins(self.as_ref(), tys, alloc, &|key| {
            let at = self.origins.binary_search_by_key(&key, |&(k, _)| k).map_err(|_| E::MissingOpaqueOrigin { key })?;
            Ok(self.origins[at].1)
        })
    }
}

/// Open a head for standalone inspection or byte-exact rewriting. A store imports with
/// [`decode_with_origins`] so it also has the routes when another type exposes an opaque.
pub fn decode_retaining<'n>(bytes: &[u8], text: Option<&'n str>, tys: &PortableTys<'_>, alloc: Mem) -> Result<DecodedHead<'n>, HeadRecordError> {
    let mut r = In { bytes, at: 0, tys, locals: Vec::new_in(alloc), scratch: Vec::new_in(alloc), origins: Vec::new_in(alloc), alloc };
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
    let head = r.decode_head_kind(kind, text)?;
    let mut head = head;
    // Form 22: the regions section, every column laid again over its type here, once.
    if let Some(filed) = r.opt("a head's regions section", |r| r.regions_section())? {
        regions_fit_head((*head).as_ref(), filed.regions(), tys.view())?;
        head.set_regions(filed)?;
    }
    if r.at != bytes.len() {
        return Err(E::Trailing { at: r.at });
    }
    r.dedup_origins()?;
    Ok(DecodedHead { head, origins: r.origins })
}

/// The reader: the bytes, where it is, and the record's terms as interned so far.
struct In<'r, 't, 'a> {
    bytes: &'r [u8],
    at: usize,
    tys: &'t PortableTys<'a>,
    /// The record's term `i` is `locals[i]` in `tys`.
    locals: Vec<PTy, Mem>,
    scratch: Vec<PTy, Mem>,
    origins: Vec<(ItemKey, OpaqueOrigin), Mem>,
    alloc: Mem,
}

impl<'r> In<'r, '_, '_> {
    fn box_head<'n>(&self, head: Head<'n>) -> blaze_own::Box<Head<'n>, Mem> {
        blaze_own::Box::new_in(head, self.alloc)
    }

    #[inline(never)]
    fn decode_head_kind<'n>(&mut self, kind: u8, text: Option<&'n str>) -> Result<blaze_own::Box<Head<'n>, Mem>, E> {
        match kind {
            K_FN => self.fn_head(text),
            K_ADT => self.adt(),
            K_CTOR => self.ctor(),
            K_TRAIT => self.trait_(),
            K_IMPL => self.impl_(),
            K_ALIAS => self.alias(),
            K_ALIAS_REFUSED => self.alias_refused(),
            K_FN_REFUSED => self.fn_refused(),
            K_TRAIT_ALIAS => self.trait_alias(),
            K_OPAQUE => self.opaque(),
            byte => Err(E::Byte { what: "the head's kind", byte }),
        }
    }

    #[inline(never)]
    fn dedup_origins(&mut self) -> Result<(), E> {
        self.origins.sort_unstable_by_key(|&(key, _)| key);
        for pair in self.origins.windows(2) {
            let (key, first) = pair[0];
            let (next, second) = pair[1];
            if key == next && first != second {
                return Err(E::ConflictingOpaqueOrigin { key, first, second });
            }
        }
        self.origins.dedup_by_key(|(key, _)| *key);
        Ok(())
    }

    #[inline(never)]
    fn ctor<'n>(&mut self) -> Result<blaze_own::Box<Head<'n>, Mem>, E> {
        let key = self.key("a constructor's key")?;
        let adt = self.key("a constructor's ADT")?;
        let generics = self.u32("a constructor's generics")?;
        let inputs = self.many("a constructor's inputs", |r| r.ty("a constructor's input"))?;
        let output = self.ty("a constructor's output")?;
        Ok(self.box_head(Head::Ctor { key, sig: CtorSig { adt, generics, inputs, output }, regions: None }))
    }

    #[inline(never)]
    fn alias<'n>(&mut self) -> Result<blaze_own::Box<Head<'n>, Mem>, E> {
        let key = self.lib("an alias's key")?;
        let generics = self.u32("an alias's generics")?;
        let ty = self.ty("an alias's type")?;
        let defaults = self.defaults()?;
        let const_params = self.const_params_own()?;
        let slots = self.opt("an alias's laid regions", |r| {
            let lifetimes = r.u32("an alias's lifetime count")?;
            let slots = r.own("an alias's region slots", |r| match r.u8("an alias's region slot")? {
                0 => match r.u32("an alias's lifetime index")? {
                    i if i < lifetimes => Ok(AliasSlot::Lifetime(i)),
                    _ => Err(E::Shape { what: "an alias's region slot names a lifetime past its count" }),
                },
                1 => Ok(AliasSlot::Static),
                2 => Ok(AliasSlot::Late { binder: r.u32("an alias's bound lifetime's binder")?, var: r.u32("an alias's bound lifetime's variable")? }),
                3 => Ok(AliasSlot::ObjectDefault),
                _ => Err(E::Shape { what: "an alias's region slot of no known kind" }),
            })?;
            Ok(AliasRegions { lifetimes, slots })
        })?;
        Ok(self.box_head(Head::Alias { key, generics, ty, defaults, const_params, slots }))
    }

    #[inline(never)]
    fn alias_refused<'n>(&mut self) -> Result<blaze_own::Box<Head<'n>, Mem>, E> {
        let key = self.lib("a refused alias's key")?;
        let n = self.count("a refused alias's why")?;
        let text = self.take(n, "a refused alias's why")?;
        let mut why = Vec::with_capacity_in(n, self.alloc);
        why.extend_from_slice(text);
        Ok(self.box_head(Head::AliasRefused { key, why }))
    }

    #[inline(never)]
    fn fn_refused<'n>(&mut self) -> Result<blaze_own::Box<Head<'n>, Mem>, E> {
        let key = self.key("a refused fn's key")?;
        let n = self.count("a refused fn's why")?;
        let text = self.take(n, "a refused fn's why")?;
        let mut why = Vec::with_capacity_in(n, self.alloc);
        why.extend_from_slice(text);
        Ok(self.box_head(Head::FnRefused { key, why }))
    }

    #[inline(never)]
    fn trait_alias<'n>(&mut self) -> Result<blaze_own::Box<Head<'n>, Mem>, E> {
        let key = self.lib("a trait alias's key")?;
        let generics = self.u32("a trait alias's generics")?;
        let clauses = self.many("a trait alias's clauses", |r| r.clause())?;
        let traits = self.many("a trait alias's traits", |r| Ok((r.lib("a trait alias's trait")?, r.list("a trait alias's trait arguments")?)))?;
        Ok(self.box_head(Head::TraitAlias { key, head: TraitAliasHead { generics, clauses, traits } }))
    }

    #[inline(never)]
    fn opaque<'n>(&mut self) -> Result<blaze_own::Box<Head<'n>, Mem>, E> {
        let key = self.key("an opaque type's key")?;
        let fn_ = self.key("an opaque type's fn")?;
        let index = self.u32("an opaque type's index")?;
        check_opaque_row(key, fn_, index)?;
        let preds = self.opt("an opaque type's bounds", |r| r.preds())?;
        Ok(self.box_head(Head::Opaque { key, fn_, index, preds, regions: None }))
    }

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
    /// A text written by [`Out::text`]: its length, then its bytes, UTF-8.
    fn text(&mut self, what: &'static str) -> Result<&'r str, E> {
        let n = self.count(what)?;
        let bytes = self.take(n, what)?;
        core::str::from_utf8(bytes).map_err(|_| E::Shape { what })
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
    fn coherence_marks(&mut self) -> Result<CoherenceMarks, E> {
        let byte = self.u8("coherence marks")?;
        if byte & !3 != 0 {
            return Err(E::Byte { what: "coherence marks", byte });
        }
        Ok(CoherenceMarks { fundamental: byte & 1 != 0, incoherent: byte & 2 != 0 })
    }
    fn field_region(&mut self, lifetimes: u32, generics: u32) -> Result<blaze_flow_lower::FieldRegion, E> {
        use blaze_flow_lower::FieldRegion;

        Ok(match self.u8("a field region's kind")? {
            0 => FieldRegion::Static,
            1 => {
                let i = self.u32("a field's lifetime parameter")?;
                if i >= lifetimes {
                    return Err(E::Shape { what: "a field's lifetime parameter exceeds the ADT's arity" });
                }
                FieldRegion::Lifetime(i)
            }
            2 => {
                let i = self.u32("a field's type parameter")?;
                if i >= generics {
                    return Err(E::Shape { what: "a field's type parameter exceeds the ADT's arity" });
                }
                FieldRegion::Arg(i)
            }
            3 => FieldRegion::Bound(self.u32("a field's bound region")?),
            byte => return Err(E::Byte { what: "a field region's kind", byte }),
        })
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
        let tag = self.u8("a term's tag")?;
        if tag == T_DYNAMIC {
            return self.node_dynamic();
        }
        let n = match tag {
            T_BOOL | T_CHAR | T_INT | T_UINT | T_FLOAT | T_STR | T_NEVER | T_ERROR | T_PARAM | T_CONST_PARAM => self.node_scalar(tag)?,
            T_TUPLE | T_REF | T_PTR | T_ARRAY | T_SLICE | T_PAT | T_ADT | T_FNDEF | T_FNPTR => self.node_compound(tag)?,
            T_ALIAS | T_OPAQUE | T_CONST_VALUE | T_EX_TRAIT | T_EX_PROJ | T_EX_AUTO | T_FOREIGN => self.node_item(tag)?,
            T_RECORD | T_UNION => self.node_structural(tag)?,
            byte => return Err(E::Byte { what: "a term's tag", byte }),
        };
        self.intern(n)
    }

    #[inline(never)]
    fn node_dynamic(&mut self) -> Result<(), E> {
        // Through the table's own constructor, which keeps the one canonical form.
        self.tys_into_scratch("a trait object's predicates")?;
        let t = self.tys.dynamic_shared(&self.scratch).map_err(E::Table)?;
        self.locals.push(t);
        Ok(())
    }

    #[inline(never)]
    fn node_scalar(&mut self, tag: u8) -> Result<PNode, E> {
        Ok(match tag {
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
            T_ERROR => PNode::Error,
            T_PARAM => PNode::Param(self.u32("a parameter's index")?),
            T_CONST_PARAM => PNode::ConstParam(self.u32("a const parameter's index")?),
            byte => return Err(E::Byte { what: "a term's tag", byte }),
        })
    }

    #[inline(never)]
    fn node_compound(&mut self, tag: u8) -> Result<PNode, E> {
        const W: &str = "a term";
        Ok(match tag {
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
            byte => return Err(E::Byte { what: "a term's tag", byte }),
        })
    }

    #[inline(never)]
    fn node_item(&mut self, tag: u8) -> Result<PNode, E> {
        const W: &str = "a term";
        Ok(match tag {
            T_ALIAS => {
                let trait_ = self.key("a projection's trait")?;
                let name = self.name("a projection's name")?;
                PNode::Alias { trait_, name, args: self.list("a projection's arguments")? }
            }
            T_OPAQUE => {
                let k = self.key("an opaque type's key")?;
                let tag = self.u8("an opaque type's defining head kind")?;
                let lib = self.lib("an opaque type's defining library item")?;
                let owner = self.key("an opaque type's checker owner")?;
                let index = self.u32("an opaque type's tagged ordinal")?;
                let want = match tag {
                    1 => Want::Head(lib),
                    4 => Want::Fn(lib),
                    5 => Want::Members(lib),
                    6 => Want::MemberFn(lib, owner),
                    byte => return Err(E::Byte { what: "an opaque type's defining head kind", byte }),
                };
                let origin = OpaqueOrigin { want, owner, index };
                origin.check(k)?;
                self.origins.push((k, origin));
                PNode::Opaque(k, self.list("an opaque type's arguments")?)
            }
            T_CONST_VALUE => {
                let ty = self.ty(W)?;
                PNode::ConstValue { ty, bits: self.u128("a const value's bits")? }
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
            T_FOREIGN => PNode::Foreign(self.key("an extern type")?),
            byte => return Err(E::Byte { what: "a term's tag", byte }),
        })
    }

    #[inline(never)]
    fn node_structural(&mut self, tag: u8) -> Result<PNode, E> {
        Ok(match tag {
            T_RECORD => {
                let s = blaze_typeck::RecordSchema(self.u64("a record's schema")?);
                let n = self.u32("a record's field name count")?;
                let mut names = Vec::with_capacity_in(usize::try_from(n).unwrap_or(0), self.tys.alloc());
                for _ in 0..n {
                    names.push(self.name("a record's field name")?);
                }
                if n > 0 || s == blaze_typeck::RecordSchema::EMPTY {
                    self.tys.register_record_schema_shared(s, &names).map_err(E::Table)?;
                }
                PNode::Record(s, self.list("a record's fields")?)
            }
            T_UNION => PNode::Union(self.list("a union's members")?),
            byte => return Err(E::Byte { what: "a term's tag", byte }),
        })
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
            3 => {
                let ty = self.ty("a type outlives clause's type")?;
                PClause::TypeOutlives { ty, region: self.rregion()? }
            }
            4 => {
                let long = self.rregion()?;
                PClause::RegionOutlives { long, short: self.rregion()? }
            }
            5 => {
                let container = self.ty("a for-fields clause's container")?;
                let trait_ = self.key("a for-fields clause's trait")?;
                PClause::ForFields { container, trait_, args: self.list("a for-fields clause's arguments")? }
            }
            byte => return Err(E::Byte { what: "a clause's tag", byte }),
        })
    }
    /// A head's clauses, boxed where they are made ([`HeadPreds`]): what `opt` and the head's
    /// constructor move is the box.
    fn preds(&mut self) -> Result<HeadPreds, E> {
        let clauses = self.many("where-clauses", |r| r.clause())?;
        let conds = self.many("`where const` conditions", |r| Ok(lf::ConstIdx(r.u32("a `where const` condition")?)))?;
        Ok(blaze_own::Box::new_in(Preds::decoded(clauses, conds, self.alloc), self.alloc))
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

    /// [`Out::regions_section`] read back. Every length is checked here, once: the arity table
    /// in strict key order, each column laid again over its type by the one counter (a slot
    /// short or long is [`HeadRecordError::Regions`]), each `Param` inside the head's count,
    /// clauses by `nth` and fields by (variant, field) strictly ascending ([`check_parts`]).
    #[inline(never)]
    fn regions_arity(&mut self) -> Result<ArityTable, E> {
        let adts = self.own("a head's region arity: ADTs", |r| Ok((r.key("an ADT's key")?, r.u32("an ADT's lifetime count")?)))?;
        let opaques = self.own("a head's region arity: opaque types", |r| Ok((r.key("an opaque type's key")?, r.u32("an opaque type's captures")?)))?;
        let aliases = self.own("a head's region arity: projections", |r| {
            Ok((r.key("a projection's trait")?, r.name("a projection's name")?, r.u32("a projection's lifetime count")?))
        })?;
        let traits = self.own("a head's region arity: traits", |r| Ok((r.key("a trait's key")?, r.u32("a trait's lifetime count")?)))?;
        if !adts.windows(2).all(|w| w[0].0 < w[1].0)
            || !opaques.windows(2).all(|w| w[0].0 < w[1].0)
            || !aliases.windows(2).all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1))
            || !traits.windows(2).all(|w| w[0].0 < w[1].0)
        {
            return Err(E::Shape { what: "a head's region arity table is not in strict key order" });
        }
        Ok(ArityTable { adts, opaques, aliases, traits })
    }

    #[inline(never)]
    fn regions_clauses(&mut self, arity: &ArityTable, regions: &mut HeadRegions<Mem>) -> Result<(), E> {
        regions.sig = self.opt("a head's signature column", |r| r.rcol(arity, "a head's signature column"))?;
        regions.written_output = self.opt("a head's written output column", |r| r.rcol(arity, "a head's written output column"))?;
        let clauses = self.count("a head's clause columns")?;
        for _ in 0..clauses {
            let nth = self.u32("a clause column's place")?;
            let binders = self.u32("a clause column's binders")?;
            let col = self.rcol(arity, "a clause's column")?;
            regions.clauses.push(ClauseRegions { nth, binders, col });
        }
        let outlives = self.count("a head's outlives clauses")?;
        for _ in 0..outlives {
            let c = match self.u8("an outlives clause's form")? {
                0 => ROutlives::Type { clause: self.u32("a type outlives clause's clause")?, region: self.rregion()? },
                1 => ROutlives::Region { long: self.rregion()?, short: self.rregion()? },
                byte => return Err(E::Byte { what: "an outlives clause's form", byte }),
            };
            regions.outlives.push(c);
        }
        Ok(())
    }

    #[inline(never)]
    fn regions_members(&mut self, arity: &ArityTable, regions: &mut HeadRegions<Mem>) -> Result<(), E> {
        let fields = self.count("a head's field columns")?;
        for _ in 0..fields {
            let variant = self.u32("a field column's variant")?;
            let field = self.u32("a field column's field")?;
            let col = self.rcol(arity, "a field's column")?;
            regions.fields.push((variant, field, col));
        }
        regions.impl_header = self.opt("an impl's header columns", |r| {
            let self_ = r.rcol(arity, "an impl's self type column")?;
            let trait_ = r.opt("an impl's trait reference column", |r| r.rcol(arity, "an impl's trait reference column"))?;
            Ok((self_, trait_))
        })?;
        let assocs = self.count("an impl's associated type columns")?;
        for _ in 0..assocs {
            let name = self.name("an impl's associated type's name")?;
            let col = self.rcol(arity, "an impl's associated type column")?;
            regions.impl_assoc.push((name, col));
        }
        regions.opaque_captures = self.opt("an opaque type's captures", |r| r.u32("an opaque type's captures"))?;
        Ok(())
    }

    #[inline(never)]
    fn regions_section(&mut self) -> Result<FiledRegions, E> {
        let arity = self.regions_arity()?;
        let params = self.rparams()?;
        let mut regions = HeadRegions::new_in(params, self.alloc);
        self.regions_clauses(&arity, &mut regions)?;
        self.regions_members(&arity, &mut regions)?;
        check_parts(&regions)?;
        Ok(FiledRegions::of(regions, arity, self.alloc))
    }
    /// [`Out::rparams`] read back: late indices strictly ascending and inside the count.
    fn rparams(&mut self) -> Result<LifetimeParams<Mem>, E> {
        let what = "a head's lifetime parameters";
        let count = self.u32("a head's lifetime parameter count")?;
        let mut params = LifetimeParams::new_in(count, self.alloc);
        let late = self.count("a head's late-bound lifetimes")?;
        let mut last: Option<u32> = None;
        for _ in 0..late {
            let i = self.u32("a late-bound lifetime")?;
            if last.is_some_and(|l| l >= i) {
                return Err(E::Shape { what: "a head's late-bound lifetimes are not in strict order" });
            }
            last = Some(i);
            params.set_late(i).map_err(|error| E::Regions { what, error })?;
        }
        let pairs = self.count("a head's lifetime outlives pairs")?;
        for _ in 0..pairs {
            let long = self.rregion()?;
            let short = self.rregion()?;
            params.add_outlives(long, short).map_err(|error| E::Regions { what, error })?;
        }
        Ok(params)
    }
    /// [`Out::rcol`] read back and laid over its type with the record's own counts.
    fn rcol(&mut self, arity: &ArityTable, what: &'static str) -> Result<RegionCol<Mem>, E> {
        let ty = self.ty(what)?;
        let n = self.count(what)?;
        let mut slots: Vec<RRegion, Mem> = Vec::with_capacity_in(n, self.alloc);
        for _ in 0..n {
            let s = self.rregion()?;
            slots.push(s);
        }
        RegionCol::lay(self.tys.view(), ty, arity, &slots, self.alloc).map_err(|error| E::Regions { what, error })
    }
    fn rregion(&mut self) -> Result<RRegion, E> {
        Ok(match self.u8("a region's form")? {
            0 => RRegion::Static,
            1 => RRegion::Param(self.u32("a region's parameter")?),
            2 => RRegion::Late { binder: self.u32("a late region's binder")?, var: self.u32("a late region's variable")? },
            byte => return Err(E::Byte { what: "a region's form", byte }),
        })
    }

    #[inline(never)]
    fn fn_head<'n>(&mut self, text: Option<&'n str>) -> Result<blaze_own::Box<Head<'n>, Mem>, E> {
        let (key, head) = self.fn_head_core(text)?;
        let preds = self.opt("a fn's where-clauses", |r| r.preds())?;
        let static_ = self.opt("a static's mutability", |r| mutbl_of(r.u8("a static's mutability")?))?;
        let misses = self.fn_head_misses(head.text.len())?;
        Ok(self.box_head(Head::Fn { key, head, preds, static_, regions: None, misses }))
    }

    #[inline(never)]
    fn fn_head_core<'n>(&mut self, text: Option<&'n str>) -> Result<(ItemKey, FnHead<'n>), E> {
        let key = self.key("a fn's key")?;
        let prefix = self.flag("a fn text's signature-prefix flag")?;
        let want_len = self.u32("a fn text's length")?;
        let want: [u8; 32] = self.array("a fn text's digest")?;
        let text = text.ok_or(E::NoText)?;
        let text_prefix = prefix.then_some(want_len);
        if !fn_record_text(text, text_prefix).is_some_and(|part| part.len() == want_len as usize && ContentId::of(TEXT_DOMAIN, part).0 == want) {
            return Err(E::Text { key, want_len, got_len: text.len() });
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
        let byte = self.u8("a fn's qualifiers")?;
        let qualifiers = super::heads::FnQualifiers::from_byte(byte).ok_or(E::Byte { what: "a fn's qualifiers", byte })?;
        let legacy_const_generics = self.many("a fn's legacy const generic positions", |r| r.u32("a legacy const generic position"))?;
        let module = self.lib("a fn's module")?;
        let (params, consts, bounds, self_) = self.scope()?;
        let self_assoc = self.named_tys("a fn's `Self` associated types")?;
        Ok((
            key,
            FnHead {
                lib,
                text,
                text_prefix,
                container,
                parent,
                own,
                declared,
                self_param,
                header,
                c_variadic,
                qualifiers,
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
            },
        ))
    }

    #[inline(never)]
    fn fn_head_misses(&mut self, text_len: usize) -> Result<Vec<HeadMiss, Mem>, E> {
        self.own("a fn's signature misses", |r| {
            let at = blaze_surface::Range::new(r.u32("a signature miss's start")?, r.u32("a signature miss's end")?);
            if at.start > at.end || usize::try_from(at.end).map_or(true, |end| end > text_len) {
                return Err(E::Shape { what: "a signature miss's range is not within the fn's text" });
            }
            let n = r.count("a signature miss's words")?;
            let words = core::str::from_utf8(r.take(n, "a signature miss's words")?).map_err(|_| E::Shape { what: "a signature miss's words are not UTF-8" })?;
            let words = words.into();
            let of = match r.u8("a signature miss's kind")? {
                0 => MissOf::Type,
                1 => MissOf::Trait,
                _ => return Err(E::Shape { what: "a signature miss of no known kind" }),
            };
            Ok(HeadMiss { at, words, of })
        })
    }

    #[inline(never)]
    fn adt<'n>(&mut self) -> Result<blaze_own::Box<Head<'n>, Mem>, E> {
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
        let fields: std::vec::Vec<tc::Field> = self.many("an ADT's fields", |r| Ok(tc::Field { name: r.name("a field's name")?, ty: r.ty("a field's type")? }))?;
        let repr = self.repr()?;
        let valid = blaze_build::layout::ValidRange {
            start: self.opt("an ADT's valid range start", |r| r.u128("a valid range start"))?,
            end: self.opt("an ADT's valid range end", |r| r.u128("a valid range end"))?,
        };
        let args = self.flag("an ADT's `#[blaze::args]` mark")?;
        let marks = self.coherence_marks()?;
        let lifetimes = self.u32("an ADT's declared lifetime arity")?;
        let mut field_region_at = Vec::with_capacity_in(fields.len(), self.alloc);
        let mut field_region_list = Vec::new_in(self.alloc);
        for _ in &fields {
            if self.flag("a field's region row is present")? {
                let len = self.count("a field's region count")?;
                let end = field_region_list.len().checked_add(len).ok_or(E::TooBig { what: "an ADT's field regions" })?;
                u32::try_from(end).map_err(|_| E::TooBig { what: "an ADT's field regions" })?;
                field_region_at.push((field_region_list.len() as u32, len as u32));
                for _ in 0..len {
                    field_region_list.push(self.field_region(lifetimes, generics)?);
                }
            } else {
                field_region_at.push(super::borrows::NO_ROW);
            }
        }
        let discrs = self.many("an enum's discriminants", |r| Ok(i128::from_le_bytes(r.array("a discriminant")?)))?;
        let defaults = self.defaults()?;
        let const_params = self.const_params_own()?;
        let field_defaults = self.own("an ADT's field defaults", |r| r.opt("a field default", |r| r.key("a field default's key")))?;
        let preds = self.opt("an ADT's where-clauses", |r| r.preds())?;
        let life = self.life("an ADT's lifetime part", |r| {
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
        let field_regions = super::borrows::FieldRegions::new(field_region_at, field_region_list, self.alloc);
        let row = AdtRow { kind, generics, variants, fields, repr, valid, discrs, args, marks, lifetimes, field_regions };
        for v in &row.variants {
            let end = v.fields.start.checked_add(v.fields.len);
            if end.and_then(|e| usize::try_from(e).ok()).is_none_or(|e| e > row.fields.len()) {
                return Err(E::Shape { what: "a variant's fields run past the ADT's" });
            }
        }
        if !field_defaults.is_empty() && field_defaults.len() != row.fields.len() {
            return Err(E::Shape { what: "an ADT's field defaults are not one per field" });
        }
        Ok(self.box_head(Head::Adt { key, row, defaults, const_params, field_defaults, preds, life, fieldless, regions: None }))
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

    #[inline(never)]
    fn trait_<'n>(&mut self) -> Result<blaze_own::Box<Head<'n>, Mem>, E> {
        let key = self.lib("a trait's key")?;
        let generics = self.u32("a trait's generics")?;
        let defaults = self.many("parameter defaults", |r| r.opt("a parameter default", |r| r.ty("a parameter default")))?;
        let assoc = self.many("a trait's associated types", |r| r.name("an associated type's name"))?;
        let fns = self.named_keys("a trait's fns")?;
        let flags = tc::TraitFlags(self.u32("a trait's flags")?);
        let marks = self.coherence_marks()?;
        let all_self_defaults = self.flag("a trait's parameters all default to Self")?;
        let members = self.many("a trait's member facts", |r| r.member())?;
        let head = TraitHead { generics, defaults, assoc, fns, flags, members, marks, all_self_defaults };
        if head.all_self_defaults != super::heads::all_self_defaults(&head.defaults, self.tys.view()) {
            return Err(E::Shape { what: "a trait's Self-default flag disagrees with its parameter defaults" });
        }
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
        Ok(self.box_head(Head::Trait { key, head, extra, supers, const_params, preds, assoc_side, life, regions: None }))
    }

    #[inline(never)]
    fn impl_<'n>(&mut self) -> Result<blaze_own::Box<Head<'n>, Mem>, E> {
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
        let fn_vis = self.many("an impl's fn visibilities", |r| {
            let byte = r.u8("an impl's fn visibility")?;
            MemberVis::of_byte(byte).ok_or(E::Byte { what: "an impl's fn visibility", byte })
        })?;
        let unstable = self.many("an impl's unstable members", |r| {
            let name = r.name("an unstable member's name")?;
            let feature = Box::<str>::from(r.text("an unstable member's feature")?);
            let implied_by = r.opt("an unstable member's `implied_by`", |r| Ok(Box::<str>::from(r.text("an unstable member's `implied_by`")?)))?;
            Ok((name, super::heads::Unstable { feature, implied_by }))
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
        let head = ImplHead { generics, self_ty, trait_, trait_args, assoc, fns, fn_self, fn_vis, unstable, consts, polarity };
        if head.fn_self.len() != head.fns.len() {
            return Err(E::Shape { what: "an impl's fn `self` flags are not one per fn" });
        }
        if head.fn_vis.len() != head.fns.len() {
            return Err(E::Shape { what: "an impl's fn visibilities are not one per fn" });
        }
        let life = self.container_life()?;
        Ok(self.box_head(Head::Impl { key, head, preds, frame, life, regions: None }))
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
                (PClause::TypeOutlives { ty: t1, region: r1 }, PClause::TypeOutlives { ty: t2, region: r2 }) => t1 == t2 && r1 == r2,
                (PClause::RegionOutlives { long: l1, short: s1 }, PClause::RegionOutlives { long: l2, short: s2 }) => l1 == l2 && s1 == s2,
                (
                    PClause::ForFields { container: c1, trait_: t1, args: a1 },
                    PClause::ForFields { container: c2, trait_: t2, args: a2 },
                ) => c1 == c2 && t1 == t2 && l(a1, a2),
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
        (HeadRef::Fn { key: k1, head: h1, preds: p1, static_: s1, regions: g1, misses: m1 }, HeadRef::Fn { key: k2, head: h2, preds: p2, static_: s2, regions: g2, misses: m2 }) => {
            need!(regions_same(g1, g2), "a fn's regions");
            need!(m1 == m2, "a fn's signature misses");
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
            need!(h1.qualifiers == h2.qualifiers, "a fn's typing attributes, const qualifier and std-internal binding");
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
            None
        }
        (
            HeadRef::Adt { key: k1, row: r1, defaults: d1, const_params: c1, field_defaults: f1, preds: p1, life: l1, fieldless: e1, regions: g1 },
            HeadRef::Adt { key: k2, row: r2, defaults: d2, const_params: c2, field_defaults: f2, preds: p2, life: l2, fieldless: e2, regions: g2 },
        ) => {
            need!(regions_same(g1, g2), "an ADT's regions");
            need!(e1 == e2, "a fieldless enum's variant count");
            need!(f1 == f2, "an ADT's field defaults");
            need!(k1 == k2, "an ADT's key");
            need!(r1.kind == r2.kind && r1.generics == r2.generics, "an ADT's kind or generics");
            need!(r1.variants[..] == r2.variants[..], "an ADT's variants");
            need!(r1.fields[..] == r2.fields[..], "an ADT's fields");
            need!(r1.repr == r2.repr && r1.valid == r2.valid, "an ADT's repr");
            need!(r1.args == r2.args, "an ADT's `#[blaze::args]` mark");
            need!(r1.marks == r2.marks, "an ADT's coherence marks");
            need!(r1.lifetimes == r2.lifetimes, "an ADT's declared lifetime arity");
            need!(r1.field_regions.at[..] == r2.field_regions.at[..] && r1.field_regions.list[..] == r2.field_regions.list[..], "an ADT's field region positions");
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
        (HeadRef::Ctor { key: k1, sig: s1, regions: g1 }, HeadRef::Ctor { key: k2, sig: s2, regions: g2 }) => {
            need!(regions_same(g1, g2), "a constructor's regions");
            need!(k1 == k2 && s1.adt == s2.adt && s1.generics == s2.generics, "a constructor's keys or generics");
            need!(s1.inputs[..] == s2.inputs[..] && s1.output == s2.output, "a constructor's signature");
            None
        }
        (
            HeadRef::Trait { key: k1, head: h1, extra: x1, supers: u1, const_params: c1, preds: p1, assoc_side: s1, life: l1, regions: g1 },
            HeadRef::Trait { key: k2, head: h2, extra: x2, supers: u2, const_params: c2, preds: p2, assoc_side: s2, life: l2, regions: g2 },
        ) => {
            need!(regions_same(g1, g2), "a trait's regions");
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
            need!(h1.marks == h2.marks, "a trait's coherence marks");
            need!(h1.all_self_defaults == h2.all_self_defaults, "a trait's parameters all default to Self");
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
        (
            HeadRef::Impl { key: k1, head: h1, preds: p1, frame: m1, life: l1, regions: g1 },
            HeadRef::Impl { key: k2, head: h2, preds: p2, frame: m2, life: l2, regions: g2 },
        ) => {
            need!(regions_same(g1, g2), "an impl's regions");
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
            need!(h1.fns[..] == h2.fns[..] && h1.fn_self[..] == h2.fn_self[..] && h1.fn_vis[..] == h2.fn_vis[..] && h1.unstable[..] == h2.unstable[..] && h1.consts[..] == h2.consts[..], "an impl's members");
            need!(h1.polarity == h2.polarity, "an impl's polarity");
            need!(preds(p1, p2), "an impl's where-clauses");
            None
        }
        (
            HeadRef::Alias { key: k1, generics: g1, ty: t1, defaults: d1, const_params: c1, slots: s1 },
            HeadRef::Alias { key: k2, generics: g2, ty: t2, defaults: d2, const_params: c2, slots: s2 },
        ) => {
            need!(k1 == k2 && g1 == g2 && t1 == t2, "an alias's head");
            need!(d1 == d2 && c1 == c2, "an alias's defaults or const parameters");
            need!(s1 == s2, "an alias's laid regions");
            None
        }
        (HeadRef::TraitAlias { key: k1, head: h1 }, HeadRef::TraitAlias { key: k2, head: h2 }) => {
            need!(k1 == k2 && h1.generics == h2.generics, "a trait alias's key or generics");
            need!(clauses(&h1.clauses, &h2.clauses), "a trait alias's clauses");
            need!(lib_lists(&h1.traits, &h2.traits), "a trait alias's traits");
            None
        }
        (
            HeadRef::Opaque { key: k1, fn_: f1, index: i1, preds: p1, regions: g1 },
            HeadRef::Opaque { key: k2, fn_: f2, index: i2, preds: p2, regions: g2 },
        ) => {
            need!(regions_same(g1, g2), "an opaque type's regions");
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

/// Whether two heads' regions are one: both absent, or both filed with equal parts and equal
/// counts (both in one type table).
fn regions_same(a: Option<&FiledRegions>, b: Option<&FiledRegions>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.parts.arity == y.parts.arity && x.regions().same(y.regions()),
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Regions (form 22, D148).

/// A head's regions as the store files them and a record holds them: the producer's
/// [`HeadRegions`] and, for every nominal node under its columns, the count the one counter
/// (`regions::region_arity`) read for it, so a record lays its columns again at open with no
/// world. Its only makers are [`FiledRegions::new`] and the record's reader: both check every
/// part ([`check_parts`]).
///
/// Its parts live in the head's allocator, never inline: a `Head` holds one in six of its
/// variants and a debug frame keeps every arm's copy of it, so ~620 bytes inline made each
/// frame that decodes, matches or files a head kilobytes wider (blaze-cli
/// `a_future_that_stops_being_send_reaches_its_caller` overflowed a 2 MiB test stack through
/// `decode_retaining` and `file_frozen`). [`FILED_REGIONS_MOST`] holds the size down.
pub struct FiledRegions {
    parts: blaze_own::Box<FiledParts, Mem>,
}

/// What a [`FiledRegions`] holds.
struct FiledParts {
    regions: HeadRegions<Mem>,
    arity: ArityTable,
}

/// The most bytes a [`FiledRegions`] takes inline: one box (its pointer and allocator).
pub const FILED_REGIONS_MOST: usize = 16;
const _: () = assert!(size_of::<FiledRegions>() <= FILED_REGIONS_MOST);
const _: () = assert!(size_of::<Option<FiledRegions>>() <= FILED_REGIONS_MOST);
// CONTROL (rule 23): the inline form, the cause, passes neither bound. The largest variant
// holds both a box of clauses and one of regions; with both inline it is past `HEAD_MOST`.
const _: () = assert!(size_of::<FiledParts>() > FILED_REGIONS_MOST);
const _: () = assert!(size_of::<Head<'static>>() - 2 * FILED_REGIONS_MOST + size_of::<FiledParts>() + size_of::<Preds>() > HEAD_MOST);

impl FiledRegions {
    /// `regions` as the producer made them, with `facts` the counts it laid them by (the same
    /// [`Arity`] its `RegionCol::lay` calls read: only the nodes under its columns are asked).
    /// Refused by name: a `Param` past the head's count, clauses not in `nth` order, fields not
    /// in (variant, field) order, a node `facts` does not know, one item given two counts.
    pub fn new<F: Arity + ?Sized>(regions: HeadRegions<Mem>, facts: &F, tys: PTys<'_>, alloc: Mem) -> Result<FiledRegions, HeadRecordError> {
        check_parts(&regions)?;
        let mut arity = ArityTable { adts: Vec::new_in(alloc), opaques: Vec::new_in(alloc), aliases: Vec::new_in(alloc), traits: Vec::new_in(alloc) };
        let mut clash = false;
        each_col(&regions, &mut |col, what| {
            each_slot_node(tys, col.ty(), facts, alloc, &mut |_, node, own| {
                clash |= match node {
                    PNode::Adt(k, _) => note(&mut arity.adts, k, own),
                    PNode::Opaque(k, _) => note(&mut arity.opaques, k, own),
                    PNode::Alias { trait_, name, .. } | PNode::ExProj { trait_, name, .. } => note_alias(&mut arity.aliases, trait_, name, own),
                    PNode::ExTrait(k, _) => note(&mut arity.traits, k, own),
                    _ => false,
                };
                Ok(())
            })
            .map(|_| ())
            .map_err(|error| E::Regions { what, error })
        })?;
        if clash {
            return Err(E::Shape { what: "one item given two region counts under one head's columns" });
        }
        Ok(FiledRegions::of(regions, arity, alloc))
    }

    /// The one constructor of the box (the producer's [`FiledRegions::new`] and the record's
    /// reader both check every part before it).
    fn of(regions: HeadRegions<Mem>, arity: ArityTable, alloc: Mem) -> FiledRegions {
        FiledRegions { parts: blaze_own::Box::new_in(FiledParts { regions, arity }, alloc) }
    }

    /// The regions the producer made.
    pub fn regions(&self) -> &HeadRegions<Mem> {
        &self.parts.regions
    }

    /// The counts its columns were laid by (and, restored, read back by): a walk of a type
    /// under one of its columns counts by these, never by what the store has made since.
    pub fn arity(&self) -> &dyn Arity {
        &self.parts.arity
    }
}

/// `(k, n)` into the key-sorted `v`; whether `k` was there with another count.
fn note(v: &mut Vec<(ItemKey, u32), Mem>, k: ItemKey, n: u32) -> bool {
    match v.binary_search_by(|e| e.0.cmp(&k)) {
        Ok(i) => v[i].1 != n,
        Err(i) => {
            v.insert_shift(i, (k, n));
            false
        }
    }
}

fn note_alias(v: &mut Vec<(ItemKey, Name, u32), Mem>, trait_: ItemKey, name: Name, n: u32) -> bool {
    match v.binary_search_by(|e| (e.0, e.1).cmp(&(trait_, name))) {
        Ok(i) => v[i].2 != n,
        Err(i) => {
            v.insert_shift(i, (trait_, name, n));
            false
        }
    }
}

/// The counts a record's columns were laid by, each list in strict key order: what
/// [`RegionCol::lay`] asks at open. A node not in it is `Count::NotKnown`, so a column over a
/// type the record did not count is refused, never laid by a guess.
#[derive(Debug, PartialEq, Eq)]
struct ArityTable {
    adts: Vec<(ItemKey, u32), Mem>,
    opaques: Vec<(ItemKey, u32), Mem>,
    aliases: Vec<(ItemKey, Name, u32), Mem>,
    /// Each trait under a clause's or an object's column (`ExTrait`): its lifetime count.
    traits: Vec<(ItemKey, u32), Mem>,
}

impl Arity for ArityTable {
    fn trait_lifetimes(&self, trait_: ItemKey) -> Count {
        match self.traits.binary_search_by(|e| e.0.cmp(&trait_)) {
            Ok(i) => Count::Known(self.traits[i].1),
            Err(_) => Count::NotKnown,
        }
    }
    fn adt_lifetimes(&self, adt: ItemKey) -> Count {
        match self.adts.binary_search_by(|e| e.0.cmp(&adt)) {
            Ok(i) => Count::Known(self.adts[i].1),
            Err(_) => Count::NotKnown,
        }
    }
    fn opaque_captures(&self, opaque: ItemKey) -> Count {
        match self.opaques.binary_search_by(|e| e.0.cmp(&opaque)) {
            Ok(i) => Count::Known(self.opaques[i].1),
            Err(_) => Count::NotKnown,
        }
    }
    fn alias_lifetimes(&self, trait_: ItemKey, name: Name) -> Count {
        match self.aliases.binary_search_by(|e| (e.0, e.1).cmp(&(trait_, name))) {
            Ok(i) => Count::Known(self.aliases[i].2),
            Err(_) => Count::NotKnown,
        }
    }
}

/// Every column of `r`, with what it is, in `HeadRegions`' field order.
fn each_col(r: &HeadRegions<Mem>, f: &mut dyn FnMut(&RegionCol<Mem>, &'static str) -> Result<(), E>) -> Result<(), E> {
    if let Some(c) = &r.sig {
        f(c, "a head's signature column")?;
    }
    if let Some(c) = &r.written_output {
        f(c, "a head's written output column")?;
    }
    for c in r.clauses.iter() {
        f(&c.col, "a clause's column")?;
    }
    for (_, _, c) in r.fields.iter() {
        f(c, "a field's column")?;
    }
    if let Some((s, t)) = &r.impl_header {
        f(s, "an impl's self type column")?;
        if let Some(t) = t {
            f(t, "an impl's trait reference column")?;
        }
    }
    for (_, c) in r.impl_assoc.iter() {
        f(c, "an impl's associated type column")?;
    }
    Ok(())
}

/// The parts' own checks, once, where regions are filed and where a record is opened: every
/// `Param` inside the head's count, clauses by `nth` and fields by (variant, field) strictly
/// ascending.
fn check_parts(r: &HeadRegions<Mem>) -> Result<(), E> {
    let count = r.params.count();
    let inside = |what: &'static str, x: RRegion| match x {
        RRegion::Param(index) if index >= count => Err(E::Regions { what, error: RegionError::LifetimePast { index, count } }),
        _ => Ok(()),
    };
    each_col(r, &mut |col, what| col.as_slice().iter().try_for_each(|&s| inside(what, s)))?;
    for &c in r.outlives.iter() {
        match c {
            ROutlives::Type { region, .. } => inside("a head's type outlives clause", region)?,
            ROutlives::Region { long, short } => {
                inside("a head's lifetime outlives clause", long)?;
                inside("a head's lifetime outlives clause", short)?;
            }
        }
    }
    if !r.clauses.windows(2).all(|w| w[0].nth < w[1].nth) {
        return Err(E::Shape { what: "a head's clause columns are not in strict clause order" });
    }
    if !r.fields.windows(2).all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1)) {
        return Err(E::Shape { what: "a head's field columns are not in strict (variant, field) order" });
    }
    // Strict name order: each name once, and the bytes one order for one set (rule 25).
    if !r.impl_assoc.windows(2).all(|w| w[0].0 < w[1].0) {
        return Err(E::Shape { what: "an impl's associated type columns are not in strict name order" });
    }
    Ok(())
}

/// Whether the regions lie over THIS head's types: a fn's or a constructor's signature column
/// over `FnPtr(inputs.., output)`, an ADT's field columns over those fields' types, an impl's
/// self column over its self type. Checked at encode (the producer's bug) and at open (the
/// record's), with both in one table.
fn regions_fit_head(head: HeadRef<'_, '_>, r: &HeadRegions<Mem>, tys: PTys<'_>) -> Result<(), E> {
    let sig_over = |inputs: &[PTy], output: PTy, what: &'static str| -> Result<(), E> {
        let Some(sig) = r.sig.as_ref() else { return Ok(()) };
        let fits = match tys.node(sig.ty()) {
            Ok(PNode::FnPtr(l, _)) => tys.list(l).is_ok_and(|io| io.len() == inputs.len() + 1 && io[..inputs.len()] == *inputs && io[inputs.len()] == output),
            _ => false,
        };
        if fits { Ok(()) } else { Err(E::Shape { what }) }
    };
    match head {
        HeadRef::Fn { head, .. } => sig_over(&head.inputs[..], head.output, "a fn's signature column is not laid over `FnPtr(inputs.., output)`"),
        HeadRef::Ctor { sig, .. } => sig_over(&sig.inputs[..], sig.output, "a constructor's signature column is not laid over `FnPtr(inputs.., output)`"),
        HeadRef::Adt { row, .. } => {
            for (variant, field, col) in r.fields.iter() {
                let ty = row
                    .variants
                    .get(*variant as usize)
                    .filter(|v| *field < v.fields.len)
                    .and_then(|v| v.fields.start.checked_add(*field))
                    .and_then(|at| row.fields.get(at as usize))
                    .map(|f| f.ty);
                if ty != Some(col.ty()) {
                    return Err(E::Shape { what: "a field's column is not laid over that field's type" });
                }
            }
            Ok(())
        }
        HeadRef::Impl { head, .. } => {
            if let Some((s, _)) = &r.impl_header
                && s.ty() != head.self_ty
            {
                return Err(E::Shape { what: "an impl's self type column is not laid over its self type" });
            }
            // Each associated type's column over that member's value (`ImplHead::assoc`).
            for (name, col) in r.impl_assoc.iter() {
                if !head.assoc.iter().any(|&(n, value)| n == *name && value == col.ty()) {
                    return Err(E::Shape { what: "an impl's associated type column is not laid over that member's value" });
                }
            }
            Ok(())
        }
        HeadRef::Trait { .. } | HeadRef::Opaque { .. } | HeadRef::FnRefused { .. } | HeadRef::Alias { .. } | HeadRef::AliasRefused { .. } | HeadRef::TraitAlias { .. } => Ok(()),
    }
}

/// The checker key a head's regions are filed and asked under: the key [`RegionFacts`] takes.
pub fn regions_key(at: HeadAt) -> ItemKey {
    match at {
        HeadAt::Fn(k) | HeadAt::Ctor(k) | HeadAt::Impl(k) | HeadAt::Opaque(k) => k,
        HeadAt::Adt(lib) | HeadAt::Trait(lib) | HeadAt::Alias(lib) | HeadAt::TraitAlias(lib) => checker_of(lib),
    }
}

/// The version an opened record had when it was of another form: what [`RegionTable::file_form`]
/// files, so a reader names the cause while the head is rebuilt.
pub fn form_of(e: &HeadRecordError) -> Option<u32> {
    match *e {
        HeadRecordError::Version { found } => Some(found),
        _ => None,
    }
}

/// The store's regions, by checker key ([`regions_key`]): each made head's, filed by its
/// lowering or from its record (heads.rs `Store::file_heads`); each refused head, so a reader
/// says so; each head whose record was of another form, while it is rebuilt. Filed through
/// `&self` as the store's other rows are, at addresses that hold until it drops.
pub struct RegionTable {
    made: Rows<ItemKey, FiledRegions, Mem>,
    refused: Rows<ItemKey, (), Mem>,
    forms: Rows<ItemKey, u32, Mem>,
    /// Heads filed whole whose regions were refused by name ([`RegionTable::file_unlaid`]).
    unlaid: Rows<ItemKey, (Unlaid, &'static str), Mem>,
}

/// Why a head filed whole has no regions: the kind of its soft refusal, the one name a
/// reader is told ([`UnreadWhy::Refused`]) and the byte a frozen cell records it by (front2
/// tables/head.rs form 11: a head of a kind that holds regions is recorded laid or refused,
/// never absent, so its reader in another store reads the same answer as the store that made
/// it). Fresh and restored read equal: a reader is told the kind's name, not the producer's
/// detail (that is `Store::region_gaps`, the making store's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unlaid {
    /// The walk met a region position it does not lay yet (types.rs `RegionRefusal::NotLaid`):
    /// its typed cause and the signature part it was met in, the same in every store (front2
    /// tables/head.rs form 12 records both); the making store's `region_gaps` holds which
    /// range.
    NotLaid { why: NotLaidWhy, part: SigPart },
    /// The one counter or the column refused: a count not filed, a table read, the step
    /// bound (types.rs `RegionRefusal::Regions`).
    Counts,
    /// Its container's regions were not filed with the container's head (types.rs
    /// `RegionRefusal::NoContainer`): the impl or trait refused its own.
    NoContainer,
    /// A lifetime written by name that no scope declares (E0261; `RegionRefusal::Undeclared`).
    Undeclared,
    /// A path written with another count of lifetime arguments than its ADT declares (E0107;
    /// `RegionRefusal::LifetimeArgs`).
    LifetimeArgs,
    /// A trait object's default lifetime its context does not decide here
    /// (`RegionRefusal::ObjectDefault`).
    ObjectDefault,
    /// A written type that names nothing (E0412; `RegionRefusal::NoType`).
    NoType,
    /// Its laid regions refused by the record's form ([`FiledRegions::new`]).
    Form,
    /// Its clauses', fields', header's or captures' columns refused (heads_regions.rs
    /// `RegionsRefusal`).
    Columns,
    /// Its cell's freeze made the head and not its regions (a producer's gap, D148).
    NotMade,
    /// Its record's regions and lifetime part did not line up (heads.rs `restore_container`).
    Disagree,
}

impl Unlaid {
    /// How many detail bytes follow the kind byte in a frozen cell's entry
    /// ([`Unlaid::detail`]): a cause byte, a part tag, a part index (u32 LE).
    pub const DETAIL: usize = 6;

    /// One of each kind (`NotLaid` by one cause and part: its detail is round-tripped on its
    /// own).
    const ALL: [Unlaid; 11] = [
        Unlaid::NotLaid { why: NotLaidWhy::Stack, part: SigPart::Whole },
        Unlaid::Form,
        Unlaid::Columns,
        Unlaid::NotMade,
        Unlaid::Disagree,
        Unlaid::Counts,
        Unlaid::NoContainer,
        Unlaid::Undeclared,
        Unlaid::LifetimeArgs,
        Unlaid::ObjectDefault,
        Unlaid::NoType,
    ];

    /// The name a reader is told.
    pub const fn name(self) -> &'static str {
        match self {
            Unlaid::NotLaid { why, .. } => why.text(),
            Unlaid::Counts => "a lifetime count the region walk was not given, or its column refused (types.rs `RegionRefusal::Regions`)",
            Unlaid::NoContainer => "its impl's or trait's own regions were not filed with the container's head, so a member's `Self` and group are unknown (types.rs `RegionRefusal::NoContainer`)",
            Unlaid::Undeclared => "a lifetime written by name that no scope of the head declares (rustc: E0261; types.rs `RegionRefusal::Undeclared`)",
            Unlaid::LifetimeArgs => "a path written with another number of lifetime arguments than it declares (rustc: E0107; types.rs `RegionRefusal::LifetimeArgs`)",
            Unlaid::ObjectDefault => "a trait object's default lifetime its context does not decide here (types.rs `RegionRefusal::ObjectDefault`)",
            Unlaid::NoType => "a written type that names nothing (rustc: E0412; types.rs `RegionRefusal::NoType`)",
            Unlaid::Form => "its laid regions were refused by the record's form (head_record.rs `FiledRegions::new`)",
            Unlaid::Columns => "its clauses', fields', header's or captures' region columns were refused (heads_regions.rs `RegionsRefusal`)",
            Unlaid::NotMade => "its cell's freeze made the head and not its regions (a producer's gap)",
            Unlaid::Disagree => "its record's regions and lifetime part disagree (heads.rs `restore_container`)",
        }
    }

    /// Its kind's byte in a frozen cell's directory (its detail follows: [`Unlaid::detail`]).
    pub const fn byte(self) -> u8 {
        match self {
            Unlaid::NotLaid { .. } => 0,
            Unlaid::Form => 1,
            Unlaid::Columns => 2,
            Unlaid::NotMade => 3,
            Unlaid::Disagree => 4,
            Unlaid::Counts => 5,
            Unlaid::NoContainer => 6,
            Unlaid::Undeclared => 7,
            Unlaid::LifetimeArgs => 8,
            Unlaid::ObjectDefault => 9,
            Unlaid::NoType => 10,
        }
    }

    /// Its detail in a frozen cell's directory: `NotLaid`'s cause byte, its part's tag and
    /// index (u32 LE); zeros for every other kind.
    pub const fn detail(self) -> [u8; Unlaid::DETAIL] {
        match self {
            Unlaid::NotLaid { why, part } => {
                let (tag, n) = part.bytes();
                let n = n.to_le_bytes();
                [why.byte(), tag, n[0], n[1], n[2], n[3]]
            }
            _ => [0; Unlaid::DETAIL],
        }
    }

    /// The refusal of kind byte `b` and detail `detail`, or `None` for a byte no kind writes or
    /// a detail its kind does not (one form per refusal: the bytes are canonical).
    pub fn of_bytes(b: u8, detail: [u8; Unlaid::DETAIL]) -> Option<Unlaid> {
        let kind = Unlaid::ALL.into_iter().find(|u| u.byte() == b)?;
        match kind {
            Unlaid::NotLaid { .. } => {
                let why = NotLaidWhy::of_byte(detail[0])?;
                let part = SigPart::of_bytes(detail[1], u32::from_le_bytes([detail[2], detail[3], detail[4], detail[5]]))?;
                Some(Unlaid::NotLaid { why, part })
            }
            other => (detail == [0; Unlaid::DETAIL]).then_some(other),
        }
    }

    /// The short class a refusal is counted by and printed FIRST (rule 19; blaze-cli
    /// `first_refusal_class` keeps a note up to its first `] `): `NotLaid/<cause>/<part>`
    /// (`NotLaid/ProjectionBoundElsewhere/Output`), or the kind's own name (`Counts`).
    pub fn class(self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        match self {
            Unlaid::NotLaid { why, part } => write!(out, "NotLaid/{}/{}", why.name(), part.name()),
            Unlaid::Counts => out.write_str("Counts"),
            Unlaid::NoContainer => out.write_str("NoContainer"),
            Unlaid::Undeclared => out.write_str("Undeclared"),
            Unlaid::LifetimeArgs => out.write_str("LifetimeArgs"),
            Unlaid::ObjectDefault => out.write_str("ObjectDefault"),
            Unlaid::NoType => out.write_str("NoType"),
            Unlaid::Form => out.write_str("Form"),
            Unlaid::Columns => out.write_str("Columns"),
            Unlaid::NotMade => out.write_str("NotMade"),
            Unlaid::Disagree => out.write_str("Disagree"),
        }
    }
}

impl RegionTable {
    pub fn new_in(alloc: Mem) -> RegionTable {
        RegionTable { made: Rows::new_in(alloc), refused: Rows::new_in(alloc), forms: Rows::new_in(alloc), unlaid: Rows::new_in(alloc) }
    }

    /// Files `item`'s regions; a refusal or old form filed before is taken back (it is made now).
    pub fn file(&self, item: ItemKey, regions: FiledRegions) {
        self.refused.remove(&item);
        self.forms.remove(&item);
        self.unlaid.remove(&item);
        self.made.insert(item, regions);
    }

    /// Files that `item`'s head is filed whole and its regions are refused, of kind `kind`, for
    /// `what` (its name, or a restore's own detail): a reader is told so by name
    /// ([`UnreadWhy::Refused`]), never the head's error.
    pub fn file_unlaid(&self, item: ItemKey, kind: Unlaid, what: &'static str) {
        self.made.remove(&item);
        self.refused.remove(&item);
        self.unlaid.insert(item, (kind, what));
    }

    /// The kind of `item`'s regions refusal, when its head was filed whole without them.
    pub fn unlaid_of(&self, item: ItemKey) -> Option<Unlaid> {
        self.unlaid.get(&item).map(|&(kind, _)| kind)
    }

    /// Files that `item`'s head was refused (its error is the head's own, reported there).
    pub fn file_refused(&self, item: ItemKey) {
        self.made.remove(&item);
        self.unlaid.remove(&item);
        self.refused.insert(item, ());
    }

    /// Files that `item`'s record was of form `found`: reported and rebuilt, never migrated.
    pub fn file_form(&self, item: ItemKey, found: u32) {
        self.forms.insert(item, found);
    }

    /// `item`'s regions, when made.
    pub fn made(&self, item: ItemKey) -> Option<&FiledRegions> {
        self.made.get(&item)
    }

    /// Why `item`'s regions are not here.
    pub fn unread(&self, item: ItemKey) -> RegionsUnread {
        let why = if self.refused.contains_key(&item) {
            UnreadWhy::HeadRefused
        } else if let Some(&found) = self.forms.get(&item) {
            UnreadWhy::RecordForm { found }
        } else if let Some(&(_, what)) = self.unlaid.get(&item) {
            UnreadWhy::Refused { what }
        } else {
            UnreadWhy::NoHead
        };
        RegionsUnread { item, why }
    }

    /// The reader over this table; `alloc` holds the clause views it hands out.
    pub fn reader<A: core::alloc::Allocator + Copy>(&self, alloc: A) -> RegionReader<'_, A> {
        RegionReader { table: self, views: Rows::new_in(alloc), alloc }
    }
}

/// [`RegionFacts`] over a [`RegionTable`]: what the flow lowering's `Facts` hands its readers
/// (pieces L3, L4, L5). Each item's clause views are made once, when first asked, and kept.
pub struct RegionReader<'s, A: core::alloc::Allocator + Copy = Mem> {
    table: &'s RegionTable,
    views: Rows<ItemKey, Vec<ClauseView<'s>, A>, A>,
    alloc: A,
}

impl<'s, A: core::alloc::Allocator + Copy> RegionReader<'s, A> {
    fn made(&self, item: ItemKey) -> Result<&'s FiledRegions, RegionsUnread> {
        let table: &'s RegionTable = self.table;
        table.made(item).ok_or_else(|| table.unread(item))
    }
}

impl<A: core::alloc::Allocator + Copy> RegionFacts for RegionReader<'_, A> {
    fn sig(&self, item: ItemKey) -> Result<SigRegions<'_>, RegionsUnread> {
        self.made(item)?.regions().sig_view().ok_or(RegionsUnread { item, why: UnreadWhy::NoHead })
    }

    fn clauses(&self, item: ItemKey) -> Result<&[ClauseView<'_>], RegionsUnread> {
        if let Some(v) = self.views.get(&item) {
            return Ok(&v[..]);
        }
        let r = self.made(item)?;
        let mut v = Vec::with_capacity_in(r.regions().clauses.len(), self.alloc);
        for c in r.regions().clauses.iter() {
            v.push(ClauseView::of(c));
        }
        self.views.insert(item, v);
        self.views.get(&item).map(|v| &v[..]).ok_or(RegionsUnread { item, why: UnreadWhy::NoHead })
    }

    fn outlives(&self, item: ItemKey) -> Result<&[ROutlives], RegionsUnread> {
        Ok(&self.made(item)?.regions().outlives[..])
    }

    fn field(&self, adt: ItemKey, variant: u32, field: u32) -> Result<ColView<'_>, RegionsUnread> {
        let r = self.made(adt)?;
        let at = r.regions().fields.binary_search_by(|e| (e.0, e.1).cmp(&(variant, field))).map_err(|_| RegionsUnread { item: adt, why: UnreadWhy::NoHead })?;
        Ok(ColView::of(&r.regions().fields[at].2))
    }

    fn impl_header(&self, impl_: ItemKey) -> Result<(ColView<'_>, Option<ColView<'_>>), RegionsUnread> {
        let r = self.made(impl_)?;
        let (s, t) = r.regions().impl_header.as_ref().ok_or(RegionsUnread { item: impl_, why: UnreadWhy::NoHead })?;
        Ok((ColView::of(s), t.as_ref().map(ColView::of)))
    }

    fn opaque_captures(&self, opaque: ItemKey) -> Result<u32, RegionsUnread> {
        self.made(opaque)?.regions().opaque_captures.ok_or(RegionsUnread { item: opaque, why: UnreadWhy::NoHead })
    }

    fn lifetime_params(&self, item: ItemKey) -> Result<ParamsView<'_>, RegionsUnread> {
        Ok(ParamsView::of(&self.made(item)?.regions().params))
    }

    fn impl_assoc(&self, impl_: ItemKey, name: Name) -> Result<ColView<'_>, RegionsUnread> {
        let r = self.made(impl_)?;
        // Strict name order (`check_parts`): one search.
        let at = r.regions().impl_assoc.binary_search_by(|e| e.0.cmp(&name)).map_err(|_| RegionsUnread { item: impl_, why: UnreadWhy::NoHead })?;
        Ok(ColView::of(&r.regions().impl_assoc[at].1))
    }
}

#[cfg(test)]
mod region_tests {
    //! Form 22: a head's regions in its record. Restore equals fresh in bytes (rule 26); every
    //! part survives; a column one slot short is refused at open; a lifetime-only edit moves
    //! the regions digest and not the erased one; an old form, a refused head and no head are
    //! each named by the reader.

    use super::*;
    use blaze_typeck::{Abi, FnPtrHeader, Mutbl, Safety};

    const RUST: FnPtrHeader = FnPtrHeader { abi: Abi::Rust, safety: Safety::Safe, c_variadic: false };
    const S: ItemKey = ItemKey(0x5);
    const CTOR: ItemKey = ItemKey(0xc7);
    /// Two associated type names, `ASSOC_A < ASSOC_B`.
    const ASSOC_A: Name = Name(1);
    const ASSOC_B: Name = Name(2);

    fn a() -> Mem {
        Alloc::global()
    }

    /// Every refusal kind has its own byte and name, and reads back from its bytes (front2
    /// tables/head.rs form 12): the split of the one "position" kind by refusal form names the
    /// class in any store (rule 19). Control: a byte no kind writes reads as none, so
    /// `of_bytes` is not total by accident.
    #[test]
    fn each_unlaid_kind_round_trips_by_its_own_byte_and_name() {
        for (i, u) in Unlaid::ALL.iter().enumerate() {
            assert_eq!(Unlaid::of_bytes(u.byte(), u.detail()), Some(*u), "ORACLE: {u:?} does not read back from its bytes");
            for w in Unlaid::ALL[..i].iter() {
                assert_ne!(w.byte(), u.byte(), "ORACLE: {w:?} and {u:?} share a byte");
                assert_ne!(w.name(), u.name(), "ORACLE: {w:?} and {u:?} share a name");
            }
        }
        assert_eq!(Unlaid::of_bytes(Unlaid::ALL.len() as u8, [0; Unlaid::DETAIL]), None, "CONTROL: a byte past the kinds read as one");
    }

    /// `NotLaid` carries its cause and its signature part through its detail bytes, every cause
    /// and each part (an input's index too), and its class names both, short and first
    /// (`NotLaid/ProjectionBoundElsewhere/Output`, under 100 characters: blaze-cli
    /// `first_refusal_class`'s cut). Controls (the bytes are canonical): a cause byte past the
    /// causes, a part tag past the parts, an index on the output, and a detail under another
    /// kind each read as none.
    #[test]
    fn a_not_laid_refusal_round_trips_its_cause_and_part() {
        let parts = [SigPart::Whole, SigPart::Input(0), SigPart::Input(7), SigPart::Input(u32::MAX), SigPart::Output, SigPart::Header];
        let mut classes: std::vec::Vec<std::string::String> = std::vec::Vec::new();
        for why in NotLaidWhy::ALL {
            assert_eq!(NotLaidWhy::of_byte(why.byte()), Some(why), "ORACLE: {why:?} does not read back from its byte");
            for part in parts {
                let u = Unlaid::NotLaid { why, part };
                assert_eq!(Unlaid::of_bytes(u.byte(), u.detail()), Some(u), "ORACLE: {u:?} does not read back from its bytes");
                let mut class = std::string::String::new();
                u.class(&mut class).unwrap();
                assert!(class.starts_with("NotLaid/") && class.len() < 100 && !class.contains("] "), "ORACLE: {u:?}'s class {class:?}");
                assert!(class.contains(why.name()) && class.ends_with(part.name()), "ORACLE: {u:?}'s class {class:?} does not name its cause and part");
                classes.push(class);
            }
        }
        // Distinct causes are distinct classes (an input's index is not in the class).
        classes.sort();
        classes.dedup();
        assert_eq!(classes.len(), NotLaidWhy::ALL.len() * 4, "ORACLE: two causes or parts share a class");
        let good = Unlaid::NotLaid { why: NotLaidWhy::ProjectionBoundElsewhere, part: SigPart::Output }.detail();
        let mut bad = good;
        bad[0] = NotLaidWhy::ALL.len() as u8;
        assert_eq!(Unlaid::of_bytes(0, bad), None, "CONTROL: a cause byte past the causes read as one");
        let mut bad = good;
        bad[1] = 4;
        assert_eq!(Unlaid::of_bytes(0, bad), None, "CONTROL: a part tag past the parts read as one");
        let mut bad = good;
        bad[2] = 1;
        assert_eq!(Unlaid::of_bytes(0, bad), None, "CONTROL: an index on the output read as one");
        assert_eq!(Unlaid::of_bytes(Unlaid::Counts.byte(), good), None, "CONTROL: a detail under `Counts` read as one");
    }

    /// `struct S<'a>(&'a u8)`: one lifetime; nothing else is nominal here.
    struct Toy;

    impl Arity for Toy {
        fn adt_lifetimes(&self, adt: ItemKey) -> Count {
            if adt == S { Count::Known(1) } else { Count::NotKnown }
        }
        fn opaque_captures(&self, _: ItemKey) -> Count {
            Count::NotKnown
        }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Count {
            Count::NotKnown
        }
        fn trait_lifetimes(&self, _: ItemKey) -> Count {
            Count::NotKnown
        }
    }

    /// The types of `S`'s constructor `fn(&'a u8) -> S<'a>`: `&u8`, `S`, the fn pointer.
    struct Tys {
        tys: PortableTys<'static>,
        r: PTy,
        s: PTy,
        f: PTy,
    }

    fn tys() -> Tys {
        let tys = super::super::rows::stable_tys(a()).expect("a stable table");
        let u = tys.intern_shared(PNode::Uint(UintTy::U8)).unwrap();
        let r = tys.intern_shared(PNode::Ref(Mutbl::Not, u)).unwrap();
        let s = tys.intern_shared(PNode::Adt(S, List::EMPTY)).unwrap();
        let io = tys.intern_list_shared(&[r, s]).unwrap();
        let f = tys.intern_shared(PNode::FnPtr(io, RUST)).unwrap();
        Tys { tys, r, s, f }
    }

    fn lay(t: &Tys, ty: PTy, slots: &[RRegion]) -> RegionCol<Mem> {
        RegionCol::lay(t.tys.view(), ty, &Toy, slots, a()).unwrap_or_else(|e| panic!("lay: {e}"))
    }

    /// A head with every part: two parameters (`'b` late, `'a: 'static`), the signature, a
    /// written output, two clauses (one with a binder), both outlives forms, a field, an impl
    /// header with its trait reference, a capture count. `out` is the output's slot: the
    /// lifetime-only edit is `Param(0)` against `Param(1)`.
    fn every_part(t: &Tys, out: RRegion) -> FiledRegions {
        let (p0, p1) = (RRegion::Param(0), RRegion::Param(1));
        let mut params = LifetimeParams::new_in(2, a());
        params.set_late(1).unwrap();
        params.add_outlives(p0, RRegion::Static).unwrap();
        let mut r = HeadRegions::new_in(params, a());
        r.sig = Some(lay(t, t.f, &[p0, out]));
        r.written_output = Some(lay(t, t.s, &[p1]));
        r.clauses.push(ClauseRegions { nth: 0, binders: 1, col: lay(t, t.r, &[RRegion::Late { binder: 0, var: 0 }]) });
        r.clauses.push(ClauseRegions { nth: 2, binders: 0, col: lay(t, t.s, &[RRegion::Static]) });
        r.outlives.push(ROutlives::Type { clause: 0, region: RRegion::Static });
        r.outlives.push(ROutlives::Region { long: p0, short: p1 });
        r.fields.push((0, 0, lay(t, t.r, &[p0])));
        r.impl_header = Some((lay(t, t.s, &[p0]), Some(lay(t, t.r, &[p1]))));
        // Two `type X = U;` values, in name order: `&'a u8` and `S<'static>`.
        r.impl_assoc.push((ASSOC_A, lay(t, t.r, &[p0])));
        r.impl_assoc.push((ASSOC_B, lay(t, t.s, &[RRegion::Static])));
        r.opaque_captures = Some(1);
        FiledRegions::new(r, &Toy, t.tys.view(), a()).unwrap_or_else(|e| panic!("filed: {e}"))
    }

    fn ctor(t: &Tys) -> CtorSig {
        CtorSig { adt: S, generics: 0, inputs: std::vec![t.r], output: t.s }
    }

    fn record(t: &Tys, sig: &CtorSig, regions: Option<&FiledRegions>) -> std::vec::Vec<u8> {
        let head = HeadRef::Ctor { key: CTOR, sig, regions };
        encode(head, t.tys.view(), a()).unwrap_or_else(|e| panic!("encode: {e}")).to_vec()
    }

    /// Rule 26: the record decoded into a FRESH table and written again is the same bytes; read
    /// back into its own table it is the same head; a record of other regions is other bytes.
    #[test]
    fn restore_equals_fresh_in_bytes() {
        let t = tys();
        let sig = ctor(&t);
        let filed = every_part(&t, RRegion::Param(0));
        let bytes = record(&t, &sig, Some(&filed));
        assert_eq!(&bytes[4..8], &VERSION.to_le_bytes());

        let fresh = tys_empty();
        let opened = decode(&bytes, None, &fresh, a()).unwrap_or_else(|e| panic!("open fresh: {e}"));
        let again = encode(opened.as_ref(), fresh.view(), a()).unwrap_or_else(|e| panic!("re-encode: {e}"));
        assert_eq!(&again[..], &bytes[..], "restore differs from fresh in bytes (rule 26)");

        let own = decode(&bytes, None, &t.tys, a()).unwrap_or_else(|e| panic!("open: {e}"));
        let made = HeadRef::Ctor { key: CTOR, sig: &sig, regions: Some(&filed) };
        assert_eq!(same(made, own.as_ref(), t.tys.view()), None);

        // CONTROL: other regions are other bytes and another head.
        let other = every_part(&t, RRegion::Param(1));
        assert_ne!(record(&t, &sig, Some(&other)), bytes, "CONTROL: a record ignores its regions");
        let planted = HeadRef::Ctor { key: CTOR, sig: &sig, regions: Some(&other) };
        assert_eq!(same(made, planted, t.tys.view()), Some("a constructor's regions"), "CONTROL: `same` ignores the regions");
    }

    fn tys_empty() -> PortableTys<'static> {
        super::super::rows::stable_tys(a()).expect("a stable table")
    }

    /// Every part read back is the part written, through the reader too.
    #[test]
    fn a_head_with_every_part_round_trips() {
        let t = tys();
        let sig = ctor(&t);
        let filed = every_part(&t, RRegion::Param(0));
        let bytes = record(&t, &sig, Some(&filed));
        let Head::Ctor { regions: Some(back), .. } = decode(&bytes, None, &t.tys, a()).unwrap_or_else(|e| panic!("open: {e}")) else {
            panic!("a constructor with its regions")
        };
        assert!(back.regions().same(filed.regions()), "a part changed in the round trip");
        assert!(back.parts.arity == filed.parts.arity);

        let table = RegionTable::new_in(a());
        table.file(CTOR, back);
        let read = table.reader(a());
        let (p0, p1) = (RRegion::Param(0), RRegion::Param(1));
        let s = read.sig(CTOR).expect("the signature");
        assert_eq!(s.params.count(), 2);
        assert_eq!(s.params.is_late(1), Ok(true));
        assert_eq!(s.params.is_late(0), Ok(false));
        assert_eq!(s.params.outlives(), [(p0, RRegion::Static)]);
        assert_eq!(s.col.as_slice(), [p0, p0]);
        assert_eq!(s.col.child(0), Some(&[p0][..]));
        assert_eq!(s.col.child(1), Some(&[p0][..]));
        assert_eq!(s.written_output.map(|c| c.ty()), Some(t.s));
        let clauses = read.clauses(CTOR).expect("the clauses");
        assert_eq!(clauses.len(), 2);
        assert_eq!((clauses[0].nth, clauses[0].binders), (0, 1));
        assert_eq!(clauses[0].col.as_slice(), [RRegion::Late { binder: 0, var: 0 }]);
        assert_eq!((clauses[1].nth, clauses[1].col.ty()), (2, t.s));
        // Asked again: the same views, kept.
        assert_eq!(read.clauses(CTOR).expect("again").len(), 2);
        assert_eq!(read.outlives(CTOR).expect("outlives"), [ROutlives::Type { clause: 0, region: RRegion::Static }, ROutlives::Region { long: p0, short: p1 }]);
        assert_eq!(read.field(CTOR, 0, 0).expect("the field").as_slice(), [p0]);
        assert_eq!(read.field(CTOR, 0, 1).map(|_| ()), Err(RegionsUnread { item: CTOR, why: UnreadWhy::NoHead }));
        let (self_, trait_) = read.impl_header(CTOR).expect("the impl header");
        assert_eq!((self_.ty(), self_.as_slice()), (t.s, &[p0][..]));
        assert_eq!(trait_.map(|c| c.ty()), Some(t.r));
        let a_ = read.impl_assoc(CTOR, ASSOC_A).expect("the first associated type");
        assert_eq!((a_.ty(), a_.as_slice()), (t.r, &[p0][..]));
        let b_ = read.impl_assoc(CTOR, ASSOC_B).expect("the second associated type");
        assert_eq!((b_.ty(), b_.as_slice()), (t.s, &[RRegion::Static][..]));
        // CONTROL: a name not filed is `NoHead`, never another name's column.
        assert_eq!(read.impl_assoc(CTOR, Name(3)).map(|_| ()), Err(RegionsUnread { item: CTOR, why: UnreadWhy::NoHead }));
        assert_eq!(read.opaque_captures(CTOR), Ok(1));
        assert_eq!(read.lifetime_params(CTOR).map(|p| p.count()), Ok(2));
    }

    /// A column one slot short is refused when the record is opened, naming the part; the
    /// untouched record opens (the control).
    #[test]
    fn a_column_one_slot_short_is_refused_at_open() {
        let t = tys();
        let sig = ctor(&t);
        let p0 = RRegion::Param(0);
        // Only a signature, so the record's tail is known: the signature column's slot count
        // (u32), its two slots (a tag and a u32 each), then the empty parts: written output (1),
        // clauses (4), outlives (4), fields (4), impl header (1), associated types (4),
        // captures (1).
        let mut params = LifetimeParams::new_in(1, a());
        params.add_outlives(p0, RRegion::Static).unwrap();
        let mut r = HeadRegions::new_in(params, a());
        r.sig = Some(lay(&t, t.f, &[p0, p0]));
        let filed = FiledRegions::new(r, &Toy, t.tys.view(), a()).unwrap_or_else(|e| panic!("filed: {e}"));
        let bytes = record(&t, &sig, Some(&filed));
        assert!(decode(&bytes, None, &tys_empty(), a()).is_ok(), "CONTROL: the whole record does not open");

        let tail = 1 + 4 + 4 + 4 + 1 + 4 + 1;
        let slots_end = bytes.len() - tail;
        let one_slot = 1 + 4;
        let count_at = slots_end - 2 * one_slot - 4;
        assert_eq!(&bytes[count_at..count_at + 4], &2u32.to_le_bytes(), "the test's offsets do not find the column's count");
        let mut short = std::vec::Vec::new();
        short.extend_from_slice(&bytes[..count_at]);
        short.extend_from_slice(&1u32.to_le_bytes());
        short.extend_from_slice(&bytes[count_at + 4..slots_end - one_slot]);
        short.extend_from_slice(&bytes[slots_end..]);
        match decode(&short, None, &tys_empty(), a()) {
            Err(HeadRecordError::Regions { what, error: RegionError::Length { want: 2, got: 1, .. } }) => assert_eq!(what, "a head's signature column"),
            Err(e) => panic!("a short column refused for another reason: {e}"),
            Ok(_) => panic!("a column one slot short was opened"),
        }
        // A `Param` past the head's one lifetime is refused at open too.
        let mut past = bytes.clone();
        let last_param = slots_end - 4;
        past[last_param..slots_end].copy_from_slice(&1u32.to_le_bytes());
        assert!(
            matches!(decode(&past, None, &tys_empty(), a()), Err(HeadRecordError::Regions { error: RegionError::LifetimePast { index: 1, count: 1 }, .. })),
            "a `Param` past the head's count was opened"
        );
    }

    /// A lifetime-only edit (the output's `Param(0)` to `Param(1)`) moves the regions digest and
    /// not the erased one; an erased edit (another output type) moves both (the control).
    #[test]
    fn a_lifetime_only_edit_moves_only_the_regions_digest() {
        let t = tys();
        let sig = ctor(&t);
        let x = every_part(&t, RRegion::Param(0));
        let y = every_part(&t, RRegion::Param(1));
        let hx = HeadRef::Ctor { key: CTOR, sig: &sig, regions: Some(&x) };
        let hy = HeadRef::Ctor { key: CTOR, sig: &sig, regions: Some(&y) };
        let erased = |h| reader_digest(h, t.tys.view(), a()).unwrap_or_else(|e| panic!("erased: {e}"));
        let regions = |h| regions_digest(h, t.tys.view(), a()).unwrap_or_else(|e| panic!("regions: {e}"));
        assert_eq!(erased(hx), erased(hy), "a lifetime-only edit moved the erased digest");
        assert_ne!(regions(hx), regions(hy), "a lifetime-only edit did not move the regions digest");
        assert_eq!(regions(hx), regions(hx), "one head, two digests");

        // CONTROL: an erased edit, `fn(&u8) -> &u8` in place of `fn(&u8) -> S`, moves both.
        let io = t.tys.intern_list_shared(&[t.r, t.r]).unwrap();
        let f2 = t.tys.intern_shared(PNode::FnPtr(io, RUST)).unwrap();
        let sig2 = CtorSig { adt: S, generics: 0, inputs: std::vec![t.r], output: t.r };
        let mut r2 = HeadRegions::new_in(LifetimeParams::new_in(2, a()), a());
        r2.sig = Some(lay(&t, f2, &[RRegion::Param(0), RRegion::Param(0)]));
        let z = FiledRegions::new(r2, &Toy, t.tys.view(), a()).unwrap_or_else(|e| panic!("filed: {e}"));
        let hz = HeadRef::Ctor { key: CTOR, sig: &sig2, regions: Some(&z) };
        assert_ne!(erased(hx), erased(hz), "CONTROL: an erased edit did not move the erased digest");
        assert_ne!(regions(hx), regions(hz), "CONTROL: an erased edit did not move the regions digest");
    }

    /// An old form is the reader's `RecordForm`, a refused head its `HeadRefused`, no head its
    /// `NoHead`: each naming the item.
    #[test]
    fn the_reader_names_why_regions_are_not_read() {
        let t = tys();
        let sig = ctor(&t);
        let filed = every_part(&t, RRegion::Param(0));
        let mut old = record(&t, &sig, Some(&filed));
        old[4..8].copy_from_slice(&(VERSION - 1).to_le_bytes());
        let e = match decode(&old, None, &tys_empty(), a()) {
            Err(e) => e,
            Ok(_) => panic!("a record of the last form was opened"),
        };
        assert_eq!(form_of(&e), Some(VERSION - 1));
        let table = RegionTable::new_in(a());
        table.file_form(CTOR, VERSION - 1);
        table.file_refused(ItemKey(0xbad));
        let read = table.reader(a());
        assert_eq!(read.sig(CTOR).map(|_| ()), Err(RegionsUnread { item: CTOR, why: UnreadWhy::RecordForm { found: VERSION - 1 } }));
        assert_eq!(read.clauses(ItemKey(0xbad)).map(|_| ()), Err(RegionsUnread { item: ItemKey(0xbad), why: UnreadWhy::HeadRefused }));
        assert_eq!(read.lifetime_params(ItemKey(0x404)).map(|_| ()), Err(RegionsUnread { item: ItemKey(0x404), why: UnreadWhy::NoHead }));
        // CONTROL: rebuilt and filed, the same item reads.
        table.file(CTOR, filed);
        assert!(table.reader(a()).sig(CTOR).is_ok(), "CONTROL: a rebuilt head's regions do not read");
    }

    /// A signature column over another type than the head's inputs and output is refused at
    /// encode, by name.
    #[test]
    fn a_column_over_another_type_is_refused() {
        let t = tys();
        let sig = ctor(&t);
        let mut r = HeadRegions::new_in(LifetimeParams::new_in(1, a()), a());
        r.sig = Some(lay(&t, t.s, &[RRegion::Param(0)]));
        let filed = FiledRegions::new(r, &Toy, t.tys.view(), a()).unwrap_or_else(|e| panic!("filed: {e}"));
        let head = HeadRef::Ctor { key: CTOR, sig: &sig, regions: Some(&filed) };
        assert!(matches!(encode(head, t.tys.view(), a()), Err(HeadRecordError::Shape { .. })), "a signature column over `S` was written");
    }

    /// An impl's associated type columns out of name order (or one name twice) are refused
    /// when filed, by name: the reader's one search and the bytes' one order rest on it. A
    /// `Param` past the impl's count in one is refused too. CONTROL: the same columns in name
    /// order are filed.
    #[test]
    fn associated_type_columns_are_in_strict_name_order() {
        let t = tys();
        let p0 = RRegion::Param(0);
        let filed = |names: [Name; 2], count: u32| {
            let mut r = HeadRegions::new_in(LifetimeParams::new_in(count, a()), a());
            r.impl_assoc.push((names[0], lay(&t, t.r, &[p0])));
            r.impl_assoc.push((names[1], lay(&t, t.s, &[RRegion::Static])));
            FiledRegions::new(r, &Toy, t.tys.view(), a()).map(|_| ())
        };
        assert!(
            matches!(filed([ASSOC_B, ASSOC_A], 1), Err(HeadRecordError::Shape { what: "an impl's associated type columns are not in strict name order" })),
            "associated type columns out of name order were filed"
        );
        assert!(matches!(filed([ASSOC_A, ASSOC_A], 1), Err(HeadRecordError::Shape { .. })), "one associated type name filed twice");
        assert!(
            matches!(filed([ASSOC_A, ASSOC_B], 0), Err(HeadRecordError::Regions { what: "an impl's associated type column", error: RegionError::LifetimePast { index: 0, count: 0 } })),
            "an associated type column naming a `Param` past the impl's count was filed"
        );
        let control = filed([ASSOC_A, ASSOC_B], 1);
        assert!(control.is_ok(), "CONTROL: columns in name order were refused: {:?}", control.err().map(|e| std::format!("{e}")));
    }

    /// A head record written with the previous layout version (`34`) is rejected cleanly.
    #[test]
    fn previous_version_34_is_rejected() {
        let t = tys();
        let sig = ctor(&t);
        let filed = every_part(&t, RRegion::Param(0));
        let mut old = record(&t, &sig, Some(&filed));
        old[4..8].copy_from_slice(&34u32.to_le_bytes());
        let err = decode(&old, None, &tys_empty(), a()).err().expect("version 34 must be rejected");
        assert_eq!(form_of(&err), Some(34));
    }
}
