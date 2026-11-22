//! What a checker key stands for, told ONCE, and the World answers that go by it.
//!
//! The class this file closes (rule 54): a front2 `World` answer handled some kinds of key and
//! let the rest fall through to a wrong want (a module, a `use` or an anonymous constant asked
//! for as a fn: `want_of_item`'s old `_ => Want::Fn(lib)`) or to a mislabelled
//! `FillError::UnknownKey` for a key the map does know (a `#[lang]` fn missing from its made
//! trait, a variant index past its made ADT, a member of a member). Forty-eight std rungs
//! stopped on one instance of it or another (the unknown-key lane's probe, 2026-11-16).
//!
//! The design: every key is classified here, once, into a closed enum the compiler makes each
//! answer match whole ([`ItemKind`] for a library item by its outline detail, [`KeyKind`] for
//! any key of the [`Keys`] map, [`WorldKind`] for a key of a body's world, which adds the
//! opaque types the head store filed). A want is made from a kind ([`item_want`], [`key_want`],
//! [`world_want`]): a kind no want makes is a typed refusal ([`FillError::NoHead`],
//! [`FillError::AnonConst`], [`FillError::MemberOfMember`], [`FillError::NotAContainer`]),
//! never another kind's want. A want already made whose answer is still not in the store is
//! [`KindGap::MadeWithoutRow`], never asked again (that was a loop to `MOST_ROUNDS`).
//!
//! The answers are functions of (names, keys, store) returning [`Unmade`] where they cannot
//! answer: what to make, or why not, by name. `BodyWorld` turns an [`Unmade`] into its fill or
//! gap in one place ([`BodyWorld::unmade`]); a test calls the functions with no world at all.

use blaze_id::ItemKey as LibKey;
use blaze_surface::ItemKind as SurfaceKind;
use blaze_typeck::{Def, ItemKey, Name, PTy, Question};

use super::fill::FillError;
use super::heads::{Store, Want};
use super::keys::{Keys, MemberKind, Origin, checker_of};
use super::source::NamesSource;
use super::types::Why;
use super::world_items::{BodyWorld, Gap};

/// A library item by what its outline says it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// A struct, an enum or a union.
    Adt(LibKey),
    Trait(LibKey),
    TraitAlias(LibKey),
    Impl(LibKey),
    Fn(LibKey),
    Const(LibKey),
    Static(LibKey),
    TypeAlias(LibKey),
    /// An enum's variant: its enum and its index there.
    Variant { lib: LibKey, enum_: LibKey, index: u32 },
    /// A member of a trait or impl the library keys (a `#[lang]` member fn, `Try::branch`).
    LibMember { lib: LibKey, owner: LibKey },
    /// Any other item (a module, a `use`, a macro, an `extern` block, ..): it has no head.
    /// Its outline kind (a module has no outline entry of its own: its kind is
    /// `SurfaceKind::Module` from the module tree).
    Other { lib: LibKey, kind: SurfaceKind },
}

/// Any key of the [`Keys`] map (or one its source finds), by what it stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind<'k> {
    Item(ItemKind),
    /// A fn of the trait or impl `container`.
    MemberFn { container: ItemKey, name: &'k str },
    /// An associated constant of `container`.
    MemberConst { container: ItemKey, name: &'k str },
    /// An associated type of `container`.
    MemberType { container: ItemKey, name: &'k str },
    /// A struct's constructor, of the ADT `container`.
    Ctor { container: ItemKey, name: &'k str },
    /// An anonymous constant of `module` ([`super::anon`]): filed with its head by
    /// `anon::file_asked`, made by no want.
    Anon { module: ItemKey, name: &'k str },
}

/// A key a body's world is asked about: a key of the map, or an opaque type a fn's head filed
/// (`Store::opaques`; an opaque's key is a function of its fn's key and index,
/// [`super::keys::opaque_key`], and is never filed in [`Keys`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorldKind<'k> {
    Key(KeyKind<'k>),
    Opaque { fn_: ItemKey, index: u32 },
}

/// A kind with its payload dropped: what a refusal says the key is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyClass {
    Adt,
    Trait,
    TraitAlias,
    Impl,
    Fn,
    Const,
    Static,
    TypeAlias,
    Variant,
    LibMember,
    MemberFn,
    MemberConst,
    MemberType,
    Ctor,
    Anon,
    Opaque,
    Other(SurfaceKind),
}

impl core::fmt::Display for KeyClass {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match *self {
            KeyClass::Adt => "a struct, enum or union",
            KeyClass::Trait => "a trait",
            KeyClass::TraitAlias => "a trait alias",
            KeyClass::Impl => "an impl",
            KeyClass::Fn => "a fn",
            KeyClass::Const => "a `const` item",
            KeyClass::Static => "a `static` item",
            KeyClass::TypeAlias => "a type alias",
            KeyClass::Variant => "an enum's variant",
            KeyClass::LibMember => "a member of a trait or impl, by its library key",
            KeyClass::MemberFn => "a member fn",
            KeyClass::MemberConst => "an associated constant",
            KeyClass::MemberType => "an associated type",
            KeyClass::Ctor => "a struct's constructor",
            KeyClass::Anon => "an anonymous constant",
            KeyClass::Opaque => "an opaque type (`impl Trait`)",
            // The surface kind's own word ("module", "use", "extern crate", ..).
            KeyClass::Other(k) => k.name(),
        })
    }
}

impl ItemKind {
    pub fn class(self) -> KeyClass {
        match self {
            ItemKind::Adt(_) => KeyClass::Adt,
            ItemKind::Trait(_) => KeyClass::Trait,
            ItemKind::TraitAlias(_) => KeyClass::TraitAlias,
            ItemKind::Impl(_) => KeyClass::Impl,
            ItemKind::Fn(_) => KeyClass::Fn,
            ItemKind::Const(_) => KeyClass::Const,
            ItemKind::Static(_) => KeyClass::Static,
            ItemKind::TypeAlias(_) => KeyClass::TypeAlias,
            ItemKind::Variant { .. } => KeyClass::Variant,
            ItemKind::LibMember { .. } => KeyClass::LibMember,
            ItemKind::Other { kind, .. } => KeyClass::Other(kind),
        }
    }
}

impl KeyKind<'_> {
    pub fn class(self) -> KeyClass {
        match self {
            KeyKind::Item(i) => i.class(),
            KeyKind::MemberFn { .. } => KeyClass::MemberFn,
            KeyKind::MemberConst { .. } => KeyClass::MemberConst,
            KeyKind::MemberType { .. } => KeyClass::MemberType,
            KeyKind::Ctor { .. } => KeyClass::Ctor,
            KeyKind::Anon { .. } => KeyClass::Anon,
        }
    }
}

impl WorldKind<'_> {
    pub fn class(self) -> KeyClass {
        match self {
            WorldKind::Key(k) => k.class(),
            WorldKind::Opaque { .. } => KeyClass::Opaque,
        }
    }
}

/// What library item `lib` is, by its KIND, never by its detail's data (world-by-origin-2: a
/// module's key has no outline entry of its own, so `detail_of` is `None` for it and the
/// module was refused `NoDetail`). An item with an outline entry: its kind
/// (`NamesSource::kind_of_item`). A key with none is told by the table that holds it: an enum's
/// variant (`variant_of`), a library-keyed member (`member_owner`), a module (`module_path`:
/// the crate's module tree, walked or macro-made); a key in none of them is
/// [`FillError::NoDetail`]. A `Variant` entry in no enum's table is
/// [`FillError::VariantOfNoEnum`].
pub fn kind_of_lib<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> Result<ItemKind, FillError> {
    let kind = match names.kind_of_item(lib) {
        Some(kind) => kind,
        None => match (names.variant_of(lib), names.member_owner(lib), names.module_path(lib)) {
            (Some((enum_, index)), _, _) => return Ok(ItemKind::Variant { lib, enum_, index }),
            (None, Some((owner, _)), _) => return Ok(ItemKind::LibMember { lib, owner }),
            (None, None, Some(_)) => SurfaceKind::Module,
            (None, None, None) => return Err(FillError::NoDetail(lib)),
        },
    };
    Ok(match kind {
        SurfaceKind::Struct | SurfaceKind::Enum | SurfaceKind::Union => ItemKind::Adt(lib),
        SurfaceKind::Trait => ItemKind::Trait(lib),
        SurfaceKind::TraitAlias => ItemKind::TraitAlias(lib),
        SurfaceKind::Impl => ItemKind::Impl(lib),
        SurfaceKind::Function => ItemKind::Fn(lib),
        SurfaceKind::Const => ItemKind::Const(lib),
        SurfaceKind::Static => ItemKind::Static(lib),
        SurfaceKind::TypeAlias => ItemKind::TypeAlias(lib),
        SurfaceKind::Variant => match names.variant_of(lib) {
            Some((enum_, index)) => ItemKind::Variant { lib, enum_, index },
            None => return Err(FillError::VariantOfNoEnum(lib)),
        },
        SurfaceKind::Module
        | SurfaceKind::MacroRules
        | SurfaceKind::Use
        | SurfaceKind::ExternCrate
        | SurfaceKind::Field
        | SurfaceKind::Macro2
        | SurfaceKind::ExternBlock
        | SurfaceKind::MacroCall
        | SurfaceKind::Error
        | SurfaceKind::Unhandled => ItemKind::Other { lib, kind },
    })
}

/// What checker key `key` stands for: its row of the map, else (a key in no row) the item its
/// source finds under it ([`Keys::library_in`]); neither is [`FillError::UnknownKey`].
pub fn kind_of_key<'k, 'n, S: NamesSource<'n>>(names: S, keys: &'k Keys, key: ItemKey) -> Result<KeyKind<'k>, FillError> {
    match keys.origin(key) {
        // An item declared in a body: the kind its body's lowering filed (the library holds
        // no outline entry for it); any other item: the library's outline.
        Some(Origin::Item(lib)) => Ok(KeyKind::Item(match keys.local_kind(key) {
            Some(local) => kind_of_local(*lib, local)?,
            None => kind_of_lib(names, *lib)?,
        })),
        Some(Origin::Member { container, kind, name }) => {
            let (container, name) = (*container, &**name);
            Ok(match kind {
                MemberKind::Fn => KeyKind::MemberFn { container, name },
                MemberKind::Const => KeyKind::MemberConst { container, name },
                MemberKind::Type => KeyKind::MemberType { container, name },
                MemberKind::Ctor => KeyKind::Ctor { container, name },
                MemberKind::Anon => KeyKind::Anon { module: container, name },
            })
        }
        None => match keys.library_in(names, key) {
            Some(lib) => Ok(KeyKind::Item(kind_of_lib(names, lib)?)),
            None => Err(FillError::UnknownKey(key)),
        },
    }
}

/// What the item `lib` declared in a body is, by the kind its body's lowering filed
/// ([`Keys::local`]). The Reference, "Statements", "Item declarations": "An item declaration
/// statement has a syntactic form identical to an item declaration within a module": a fn, a
/// `const` or a `static` has a head like a module's, made by `Want::Fn` from its local row
/// (heads.rs `fn_head` through `Locals::head_of`). Any other local item has no head made from
/// its key (a type is read at its block's module): [`FillError::LocalWithoutHead`] names it.
pub fn kind_of_local(lib: LibKey, kind: super::locals::LocalKind) -> Result<ItemKind, FillError> {
    use super::locals::LocalKind;
    match kind {
        LocalKind::Fn => Ok(ItemKind::Fn(lib)),
        LocalKind::Const => Ok(ItemKind::Const(lib)),
        LocalKind::Static => Ok(ItemKind::Static(lib)),
        other => Err(FillError::LocalWithoutHead { lib, form: other.form() }),
    }
}

/// [`kind_of_key`] for a body's world: an opaque type its store filed is told first (its key
/// is in no row of the map, and no library key begins like it).
pub fn world_kind<'k, 'n, S: NamesSource<'n>>(
    names: S,
    keys: &'k Keys,
    store: &Store<'n>,
    key: ItemKey,
) -> Result<WorldKind<'k>, FillError> {
    if let Some(&(fn_, index)) = store.opaques.get(&key) {
        return Ok(WorldKind::Opaque { fn_, index });
    }
    Ok(WorldKind::Key(kind_of_key(names, keys, key)?))
}

/// The want that makes library item `lib`'s whole record (`fill::want_of_item`): an ADT's
/// definition, a trait's members and predicates, an impl's header, a fn's, constant's or
/// static's signature, an alias's head; a variant's is its enum's, a library-keyed member's its
/// owner's. Any other item has no head: [`FillError::NoHead`] names its kind.
pub fn item_want<'n, S: NamesSource<'n>>(names: S, kind: ItemKind) -> Result<Want, FillError> {
    match kind {
        ItemKind::Adt(lib) => Ok(Want::Adt(lib)),
        ItemKind::Trait(lib) => Ok(Want::Members(lib)),
        ItemKind::Impl(lib) => Ok(Want::Impl(lib)),
        ItemKind::Fn(lib) | ItemKind::Const(lib) | ItemKind::Static(lib) => Ok(Want::Fn(lib)),
        // An alias's whole record is its head (`Store::step`: `Want::Head` of a type alias or
        // a trait alias).
        ItemKind::TypeAlias(lib) | ItemKind::TraitAlias(lib) => Ok(Want::Head(lib)),
        // A variant's row is made with its enum's definition.
        ItemKind::Variant { enum_, .. } => Ok(Want::Adt(enum_)),
        ItemKind::LibMember { lib, owner } => match kind_of_lib(names, owner)? {
            ItemKind::Trait(t) => Ok(Want::Members(t)),
            ItemKind::Impl(i) => Ok(Want::Impl(i)),
            other => Err(FillError::NotAContainer { member: checker_of(lib), container: checker_of(owner), class: other.class() }),
        },
        ItemKind::Other { lib, kind } => Err(FillError::NoHead(lib, kind)),
    }
}

/// The want for a key of kind `kind`: an item's own ([`item_want`]); an impl's fn's own
/// signature ([`Want::MemberFn`]); another member's, its container's whole record (a trait's
/// members, an impl's header, a ctor's ADT). A member whose container is itself a member is
/// [`FillError::MemberOfMember`]; one whose container is no trait, impl or ADT is
/// [`FillError::NotAContainer`]; an anonymous constant is [`FillError::AnonConst`].
pub fn key_want<'n, S: NamesSource<'n>>(names: S, keys: &Keys, key: ItemKey, kind: KeyKind<'_>) -> Result<Want, FillError> {
    let (container, is_fn) = match kind {
        KeyKind::Item(item) => return item_want(names, item),
        KeyKind::Anon { .. } => return Err(FillError::AnonConst(key)),
        KeyKind::MemberFn { container, .. } => (container, true),
        KeyKind::MemberConst { container, .. } | KeyKind::MemberType { container, .. } | KeyKind::Ctor { container, .. } => (container, false),
    };
    match kind_of_key(names, keys, container)? {
        KeyKind::Item(ItemKind::Impl(lib)) if is_fn => Ok(Want::MemberFn(lib, key)),
        KeyKind::Item(ItemKind::Impl(lib)) => Ok(Want::Impl(lib)),
        KeyKind::Item(ItemKind::Trait(lib)) => Ok(Want::Members(lib)),
        KeyKind::Item(ItemKind::Adt(lib)) => Ok(Want::Adt(lib)),
        KeyKind::Item(other) => Err(FillError::NotAContainer { member: key, container, class: other.class() }),
        other @ (KeyKind::MemberFn { .. } | KeyKind::MemberConst { .. } | KeyKind::MemberType { .. } | KeyKind::Ctor { .. } | KeyKind::Anon { .. }) => {
            Err(FillError::MemberOfMember { member: key, container, class: other.class() })
        }
    }
}

/// Which cell holds what a body's check takes about a key (tables/task.rs `fact_read`): the
/// two classes of every kind, told HERE once and whole (rule 76: no wildcard arm, so a kind
/// added to [`ItemKind`] or [`KeyKind`] does not compile until it is classed). Which
/// outline kinds are [`ItemKind::Other`] is [`kind_of_lib`]'s match, total over the
/// surface's kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactHolder {
    /// A kind with a head: a head task's cell holds its facts (a signature, predicates, a
    /// definition, a header), the cell of the want [`key_want`] makes (tables/head.rs
    /// `cell_of`): a variant's and a constructor's is its ADT's, a member's its container's.
    Head,
    /// A kind with no head (a module, a `use`, a `macro_rules!`, a `macro`, an `extern crate`,
    /// a field, an `extern` block, a macro call, an error or unhandled item): a check takes of
    /// it only what the names record holds of its key, that it is there and is of this kind
    /// (its outline row; a module's: its parent and path), which is the names cell
    /// `Item { key: lib }` (blaze-scope names_cells.rs). Its head-shaped questions are answered
    /// by that kind alone (world_items.rs: `fn_sig_once` answers none for `ItemKind::Other`,
    /// `attrs_once` refuses `NotAHead`), so that cell is all such an answer can move by.
    Names { lib: LibKey, kind: SurfaceKind },
    /// An anonymous constant ([`super::anon`]): no cell. Its head (no inputs, its type as
    /// output, no predicates: anon.rs `file_asked`) is a function of its key, which is of its
    /// module, its type and its text; its value is evaluated in the asking task's own world,
    /// its text's names read there through the task's recorded view and each head it reaches
    /// filed in that task's store, or else inside the freeze of the head that wrote it, whose
    /// cell holds the value in its bytes. Nothing moves its answer that one of those reads
    /// does not see.
    OwnKey,
}

/// The [`FactHolder`] of a key of kind `kind`.
pub fn fact_holder(kind: KeyKind<'_>) -> FactHolder {
    match kind {
        KeyKind::Item(item) => match item {
            ItemKind::Adt(_)
            | ItemKind::Trait(_)
            | ItemKind::TraitAlias(_)
            | ItemKind::Impl(_)
            | ItemKind::Fn(_)
            | ItemKind::Const(_)
            | ItemKind::Static(_)
            | ItemKind::TypeAlias(_)
            | ItemKind::Variant { .. }
            | ItemKind::LibMember { .. } => FactHolder::Head,
            ItemKind::Other { lib, kind } => FactHolder::Names { lib, kind },
        },
        KeyKind::MemberFn { .. } | KeyKind::MemberConst { .. } | KeyKind::MemberType { .. } | KeyKind::Ctor { .. } => FactHolder::Head,
        KeyKind::Anon { .. } => FactHolder::OwnKey,
    }
}

/// [`key_want`] for a body's world: an opaque type's record is made with its fn's head.
pub fn world_want<'n, S: NamesSource<'n>>(names: S, keys: &Keys, key: ItemKey, kind: WorldKind<'_>) -> Result<Want, FillError> {
    match kind {
        WorldKind::Key(k) => key_want(names, keys, key, k),
        WorldKind::Opaque { fn_, .. } => key_want(names, keys, fn_, kind_of_key(names, keys, fn_)?),
    }
}

impl Store<'_> {
    /// Whether `want` is made: the one test `Store::step` and the world's answers share (a
    /// want made is never asked again; a question still without its answer then is
    /// [`KindGap::MadeWithoutRow`]).
    pub fn made(&self, want: Want) -> bool {
        match want {
            Want::Head(k) => {
                self.maps.generics.contains_key(&k)
                    || self.maps.traits.contains_key(&k)
                    || self.maps.aliases.contains_key(&k)
                    || self.maps.trait_aliases.contains_key(&k)
            }
            Want::Adt(k) => self.adts.contains_key(&checker_of(k)),
            Want::Impl(k) => self.impls.contains_key(&checker_of(k)),
            Want::Fn(k) => self.fns.contains_key(&checker_of(k)),
            Want::Members(k) => self.preds.contains_key(&checker_of(k)),
            // Made, or refused as its own gap (`member_fn_head`).
            Want::MemberFn(_, f) => self.fns.contains_key(&f) || self.fn_gaps.contains_key(&f),
        }
    }
}

/// An item's path for a refusal's text (`check::key_shown_in`: a library item's module path and
/// name, an anonymous constant's text, a member's container's path and its name).
pub type Shown = Box<str>;

/// Why a World answer is refused, by kind, each naming the item by its path (rule 19) and its
/// key (tools grep it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KindGap {
    /// The want that makes the answer is made, and the answer is not in the store.
    MadeWithoutRow { key: ItemKey, want: Want, path: Shown },
    /// A variant index past the made ADT's variants.
    NoVariant { adt: ItemKey, index: u32, variants: usize, path: Shown },
    /// A question the world does not answer for a key of this kind.
    Unanswered { question: Question, class: KeyClass, path: Shown },
    /// An anonymous constant in the map with no row in the store: `anon::file_asked` filed its
    /// key and then refused its head.
    AnonUnfiled { key: ItemKey, path: Shown },
    /// An opaque type whose fn's head is made but holds no bounds for it (a head record
    /// restored the opaque without its predicates).
    OpaqueWithoutBounds { opaque: ItemKey, fn_: ItemKey, index: u32, path: Shown },
    /// A trait's associated type with its own generic parameters (a GAT) has no derived
    /// checker-indexed const-parameter row, even though its declaration count is filed.
    GatConstParams { key: ItemKey, own: u32, path: Shown },
}

impl core::fmt::Display for KindGap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            KindGap::MadeWithoutRow { key, want, path } => {
                write!(f, "{path} ({:#034x}): {want:?} is made and its record holds no answer for it (front2 by_origin, rule 3: never asked again)", key.0)
            }
            KindGap::NoVariant { adt, index, variants, path } => {
                write!(f, "{path} ({:#034x}) has {variants} variants in its made head; variant index {index} is past them (front2 by_origin, rule 19)", adt.0)
            }
            KindGap::Unanswered { question, class, path } => {
                write!(f, "{path} is {class}; question {question:?} is not answered for {class} (front2 by_origin, rule 3)")
            }
            KindGap::AnonUnfiled { key, path } => {
                write!(f, "{path} ({:#034x}): an anonymous constant in the key map with no head in the store (anon::file_asked refused it)", key.0)
            }
            KindGap::OpaqueWithoutBounds { opaque, fn_, index, path } => {
                write!(f, "opaque type {index} ({:#034x}) of {path} ({:#034x}): its fn's head is made and holds no bounds for it", opaque.0, fn_.0)
            }
            KindGap::GatConstParams { key, own, path } => {
                write!(f, "{path} ({:#034x}): an associated type with {own} generic parameters of its own has no checker-indexed const-parameter row (front2 by_origin)", key.0)
            }
        }
    }
}

/// A refusal of a World answer.
#[derive(Debug)]
pub enum Refusal<'s> {
    Fill(FillError),
    Kind(KindGap),
    /// A signature refused when its container's head was made (`Store::fn_gaps`), and why.
    FnHead(ItemKey, &'s Why),
}

/// A World answer not in the store: the want that makes it, or why there is none.
#[derive(Debug)]
pub enum Unmade<'s> {
    Want(Want),
    Refused(Refusal<'s>),
}

fn path<'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, key: ItemKey) -> Shown {
    super::check::key_shown_in(names, keys, store, key)
}

/// The want that makes `key`'s record, by its kind; a made want is [`KindGap::MadeWithoutRow`].
pub fn want_unmade<'s, 'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &'s Store<'n>, key: ItemKey) -> Unmade<'s> {
    let want = world_kind(names, keys, store,key).and_then(|kind| world_want(names, keys, key, kind));
    match want {
        Ok(w) if store.made(w) => Unmade::Refused(Refusal::Kind(KindGap::MadeWithoutRow { key, want: w, path: path(names, keys, store, key) })),
        Ok(w) => Unmade::Want(w),
        Err(e) => Unmade::Refused(Refusal::Fill(e)),
    }
}

/// `predicates(item)` where the store holds no row for `item` (and no `fn_gaps` refusal: the
/// world answers that first). Each kind by name: an opaque's bounds are made with its fn's head;
/// an anonymous constant's empty row is filed with its head, so none means it was refused; any
/// other kind's row is made by its own want ([`want_unmade`]).
pub fn predicates_unmade<'s, 'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &'s Store<'n>, item: ItemKey) -> Unmade<'s> {
    let kind = match world_kind(names, keys, store,item) {
        Ok(kind) => kind,
        Err(e) => return Unmade::Refused(Refusal::Fill(e)),
    };
    match kind {
        WorldKind::Opaque { fn_, index } => match store.fns.contains_key(&fn_) {
            true => Unmade::Refused(Refusal::Kind(KindGap::OpaqueWithoutBounds { opaque: item, fn_, index, path: path(names, keys, store, fn_) })),
            false => want_unmade(names, keys, store,fn_),
        },
        WorldKind::Key(KeyKind::Anon { .. }) => Unmade::Refused(Refusal::Kind(KindGap::AnonUnfiled { key: item, path: path(names, keys, store, item) })),
        WorldKind::Key(k @ KeyKind::Item(ItemKind::Other { .. })) => {
            Unmade::Refused(Refusal::Kind(KindGap::Unanswered { question: Question::Predicates(item), class: k.class(), path: path(names, keys, store, item) }))
        }
        WorldKind::Key(
            KeyKind::Item(
                ItemKind::Adt(_)
                | ItemKind::Trait(_)
                | ItemKind::TraitAlias(_)
                | ItemKind::Impl(_)
                | ItemKind::Fn(_)
                | ItemKind::Const(_)
                | ItemKind::Static(_)
                | ItemKind::TypeAlias(_)
                | ItemKind::Variant { .. }
                | ItemKind::LibMember { .. },
            )
            | KeyKind::MemberFn { .. }
            | KeyKind::MemberConst { .. }
            | KeyKind::MemberType { .. }
            | KeyKind::Ctor { .. },
        ) => want_unmade(names, keys, store,item),
    }
}

/// The const generic parameters of `item` ("the parent's first", blaze-typeck `ConstParams`),
/// each kind by name. A made row answers: an item's own (`Maps::const_params`), a member's or an
/// anonymous constant's signature row (`FnHead::consts`, its container's first). Else: a
/// variant's and a constructor's are its ADT's; an opaque type's are its fn's (the Reference,
/// "Impl trait", "Capturing": a return-position `impl Trait` "automatically captures all in-scope
/// type and const generic parameters"); a trait's associated type with no parameters of its own
/// has its trait's, while a GAT uses its stored row with own parameters shifted after the trait's;
/// a head not made yet is wanted; anything else is refused by name. Never "no parameters" for a
/// head that is not made (the old `.unwrap_or(&[])`).
pub fn const_params_of<'s, 'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &'s Store<'n>, item: ItemKey) -> Result<&'s [(u32, PTy)], Unmade<'s>> {
    let lib_row = |key: ItemKey| keys.library_in(names, key).and_then(|lib| store.maps.const_params.get(&lib)).map(|c| &c[..]);
    if let Some(params) = lib_row(item) {
        return Ok(params);
    }
    if let Some(f) = store.fns.get(&item) {
        return Ok(&f.consts[..]);
    }
    if let Some(why) = store.fn_gaps.get(&item) {
        return Err(Unmade::Refused(Refusal::FnHead(item, why)));
    }
    let kind = world_kind(names, keys, store,item).map_err(|e| Unmade::Refused(Refusal::Fill(e)))?;
    let refuse = |class: KeyClass| Unmade::Refused(Refusal::Kind(KindGap::Unanswered { question: Question::ConstParams(item), class, path: path(names, keys, store, item) }));
    match kind {
        // An opaque of a type alias (front2/tait.rs): its const parameters are the alias's.
        WorldKind::Opaque { fn_, index } if super::tait::of_alias(index) => match kind_of_key(names, keys, fn_).map_err(|e| Unmade::Refused(Refusal::Fill(e)))? {
            KeyKind::Item(ItemKind::TypeAlias(lib)) => match store.maps.const_params.get(&lib) {
                Some(c) => Ok(&c[..]),
                None => Err(want_unmade(names, keys, store, fn_)),
            },
            other => Err(refuse(other.class())),
        },
        WorldKind::Opaque { fn_, .. } => match store.fns.get(&fn_) {
            Some(f) => Ok(&f.consts[..]),
            None => Err(want_unmade(names, keys, store,fn_)),
        },
        WorldKind::Key(KeyKind::Item(ItemKind::Variant { enum_, .. })) => match store.maps.const_params.get(&enum_) {
            Some(c) => Ok(&c[..]),
            None => Err(want_unmade(names, keys, store,item)),
        },
        WorldKind::Key(KeyKind::Ctor { container, .. }) => match lib_row(container) {
            Some(c) => Ok(c),
            None => Err(want_unmade(names, keys, store,item)),
        },
        WorldKind::Key(KeyKind::MemberType { container, name }) => {
            let trait_ = match kind_of_key(names, keys, container) {
                Ok(KeyKind::Item(ItemKind::Trait(lib))) => lib,
                Ok(_) => return Err(refuse(KeyClass::MemberType)),
                Err(e) => return Err(Unmade::Refused(Refusal::Fill(e))),
            };
            let (Some(params), Some(&own)) = (store.maps.const_params.get(&trait_), store.maps.assoc_generics.get(&(trait_, Name::of(name)))) else {
                return Err(want_unmade(names, keys, store,item));
            };
            match own {
                0 => Ok(&params[..]),
                _ => match store.gat_const_params.get(&item) {
                    Some(c) => Ok(&c[..]),
                    None => Err(Unmade::Refused(Refusal::Kind(KindGap::GatConstParams { key: item, own, path: path(names, keys, store, item) }))),
                },
            }
        }
        WorldKind::Key(KeyKind::Anon { .. }) => Err(Unmade::Refused(Refusal::Kind(KindGap::AnonUnfiled { key: item, path: path(names, keys, store, item) }))),
        WorldKind::Key(k @ KeyKind::Item(ItemKind::Other { .. } | ItemKind::LibMember { .. })) => Err(refuse(k.class())),
        // Their heads file the row: want them (a made one with no row is `MadeWithoutRow`).
        WorldKind::Key(
            KeyKind::Item(
                ItemKind::Adt(_)
                | ItemKind::Trait(_)
                | ItemKind::TraitAlias(_)
                | ItemKind::Impl(_)
                | ItemKind::Fn(_)
                | ItemKind::Const(_)
                | ItemKind::Static(_)
                | ItemKind::TypeAlias(_),
            )
            | KeyKind::MemberFn { .. }
            | KeyKind::MemberConst { .. },
        ) => Err(want_unmade(names, keys, store,item)),
    }
}

/// A variant's (or a struct's, index 0) definition as a body names it, from its ADT's made
/// head; an index past its variants is [`KindGap::NoVariant`], naming the ADT and the index.
pub fn ctor_def<'s, 'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &'s Store<'n>, adt: LibKey, variant: u32) -> Result<Def, Unmade<'s>> {
    let k = checker_of(adt);
    let Some(a) = store.adts.get(&k) else { return Err(Unmade::Want(Want::Adt(adt))) };
    match a.variants.get(variant as usize) {
        Some(v) => Ok(Def::Ctor { adt: k, variant, kind: v.ctor }),
        None => Err(Unmade::Refused(Refusal::Kind(KindGap::NoVariant { adt: k, index: variant, variants: a.variants.len(), path: path(names, keys, store, k) }))),
    }
}

impl<'w, 'n, S: NamesSource<'n>> BodyWorld<'w, 'n, S> {
    /// The one place an [`Unmade`] becomes the world's fill or gap.
    pub(super) fn unmade<T>(&self, u: Unmade<'_>) -> blaze_typeck::Fetch<T> {
        match u {
            Unmade::Want(w) => self.store_want(w),
            Unmade::Refused(Refusal::Fill(e)) => self.gap(Gap::Fill(e)),
            Unmade::Refused(Refusal::Kind(g)) => self.gap(Gap::Kind(g)),
            Unmade::Refused(Refusal::FnHead(k, why)) => self.gap(Gap::FnHead(k, format!("{why:?}").into_boxed_str())),
        }
    }
}
