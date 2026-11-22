//! The driver loop's fill side: a question the checker answered `Missing` to, as the
//! [`Want`] that makes its record in the [`Store`](super::heads::Store). The `World` over the
//! store (`world_*.rs`, next) answers from the store alone; this is the one map from what the
//! checker asks to what the store makes.

use blaze_id::ItemKey as LibKey;
use blaze_surface::ItemKind as SurfaceKind;
use blaze_typeck::{ItemKey, Question};

use super::by_origin::{KeyClass, item_want, key_want, kind_of_key, kind_of_lib};
use super::heads::Want;
use super::keys::Keys;

/// Why a question has no [`Want`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillError {
    /// The checker key is in no row of the key map.
    UnknownKey(ItemKey),
    /// The library holds no item detail for the key.
    NoDetail(LibKey),
    /// The question is not filled from item heads (paths, lang items, buckets, bodies).
    NotAHead(Question),
    /// A library item of a kind no want makes (a module, a `use`, a macro, an `extern` block,
    /// ..): it has no head ([`super::by_origin::item_want`]).
    NoHead(LibKey, SurfaceKind),
    /// A library item whose outline kind is a variant and which no enum's table holds.
    VariantOfNoEnum(LibKey),
    /// An anonymous constant: filed with its head by `anon::file_asked`; no want makes it.
    AnonConst(ItemKey),
    /// A member filed under a container that is itself a member: the member, the container and
    /// what the container is.
    MemberOfMember { member: ItemKey, container: ItemKey, class: KeyClass },
    /// A member filed under a container that is no trait, impl or ADT.
    NotAContainer { member: ItemKey, container: ItemKey, class: KeyClass },
    /// An item declared in a body of a kind whose head is not made from its key (a struct,
    /// a `use`, ..: [`super::by_origin::kind_of_local`]): the item and its form.
    LocalWithoutHead { lib: LibKey, form: &'static str },
}

impl core::fmt::Display for FillError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FillError::UnknownKey(k) => write!(f, "the checker key {:#034x} ({k:?}) is in no row of the front end's key map, and its crate's records hold no item under it (gap Fill/UnknownKey)", k.0),
            FillError::NoDetail(k) => write!(f, "the names library holds no detail for item {k:?}"),
            FillError::NotAHead(q) => write!(f, "question {q:?} is not answered from an item's head"),
            FillError::NoHead(k, kind) => write!(f, "the item {k:?} is of kind `{kind}`, which has no head: no want makes its record (front2 by_origin, rule 3)"),
            FillError::VariantOfNoEnum(k) => write!(f, "the item {k:?} is a variant by its outline kind, and no enum's table holds it (front2 by_origin)"),
            FillError::AnonConst(k) => write!(f, "the anonymous constant {k:?} is filed with its head where a head writes it (front2 anon::file_asked); no want makes it"),
            FillError::MemberOfMember { member, container, class } => {
                write!(f, "the member {member:?} is filed under {container:?}, which is itself {class} (a member of a member): no want makes it (front2 by_origin)")
            }
            FillError::NotAContainer { member, container, class } => {
                write!(f, "the member {member:?} is filed under {container:?}, which is {class}, not a trait, an impl or an ADT (front2 by_origin)")
            }
            FillError::LocalWithoutHead { lib, form } => {
                write!(f, "the item {lib:?} is {form}: no head is made from its key, so no signature, predicates or record is asked of it (front2 by_origin kind_of_local)")
            }
        }
    }
}

/// The want that makes library item `lib`'s whole record, by what the item is
/// ([`super::by_origin::kind_of_lib`], [`super::by_origin::item_want`]): an ADT's definition,
/// a trait's members and predicates, an impl's header, a fn's, constant's or static's
/// signature, an alias's head, a variant's enum's definition. Any other item (a module, a `use`,
/// ..) is [`FillError::NoHead`], never asked for as a fn.
pub fn want_of_item<'n, S: super::source::NamesSource<'n>>(names: S, lib: LibKey) -> Result<Want, FillError> {
    item_want(names, kind_of_lib(names, lib)?)
}

/// The want for checker key `key`, by what it is ([`super::by_origin::kind_of_key`],
/// [`super::by_origin::key_want`]): its own item's, an impl's fn's own signature
/// ([`Want::MemberFn`]), or another member's container's. A key in no row of the map is asked
/// of the source (`Keys::library_in`); a member of a member is [`FillError::MemberOfMember`],
/// never "unknown"; an anonymous constant is [`FillError::AnonConst`].
pub fn want_of_key<'n, S: super::source::NamesSource<'n>>(names: S, keys: &Keys, key: ItemKey) -> Result<Want, FillError> {
    key_want(names, keys, key, kind_of_key(names, keys, key)?)
}

/// The want that answers `q`, for the questions answered from item heads.
pub fn want_of<'n, S: super::source::NamesSource<'n>>(names: S, keys: &Keys, q: Question) -> Result<Want, FillError> {
    match q {
        Question::FnSig(k)
        | Question::Predicates(k)
        | Question::Adt(k)
        | Question::ImplHeader(k)
        | Question::TraitDef(k)
        | Question::AssocParent(k)
        | Question::Attrs(k)
        | Question::AssocItem(k, _)
        | Question::AssocGenerics(k, _)
        | Question::TypeAlias(k)
        | Question::FieldDefaults(k)
        | Question::MemberVisible(k, _)
        | Question::MemberUnstable(k, _) => want_of_key(names, keys, k),
        _ => Err(FillError::NotAHead(q)),
    }
}
