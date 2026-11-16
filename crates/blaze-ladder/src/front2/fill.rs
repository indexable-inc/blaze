//! The driver loop's fill side: a question the checker answered `Missing` to, as the
//! [`Want`] that makes its record in the [`Store`](super::heads::Store). The `World` over the
//! store (`world_*.rs`, next) answers from the store alone; this is the one map from what the
//! checker asks to what the store makes.

use blaze_id::ItemKey as LibKey;
use blaze_surface::ItemDetail;
use blaze_typeck::{ItemKey, Question};

use super::heads::Want;
use super::keys::{Keys, Origin};

/// Why a question has no [`Want`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillError {
    /// The checker key is in no row of the key map.
    UnknownKey(ItemKey),
    /// The library holds no item detail for the key.
    NoDetail(LibKey),
    /// The question is not filled from item heads (paths, lang items, buckets, bodies).
    NotAHead(Question),
}

impl core::fmt::Display for FillError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FillError::UnknownKey(k) => write!(f, "checker key {k:?} is in no row of the front end's key map"),
            FillError::NoDetail(k) => write!(f, "the names library holds no detail for item {k:?}"),
            FillError::NotAHead(q) => write!(f, "question {q:?} is not answered from an item's head"),
        }
    }
}

/// The want that makes library item `lib`'s whole record: an ADT's definition, a trait's
/// members and predicates, an impl's header with its fns, a fn's signature.
pub fn want_of_item<'n, S: super::source::NamesSource<'n>>(names: S, lib: LibKey) -> Result<Want, FillError> {
    let detail = names.detail_of(lib).ok_or(FillError::NoDetail(lib))?;
    Ok(match detail {
        ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union => Want::Adt(lib),
        ItemDetail::Trait => Want::Members(lib),
        ItemDetail::Impl => Want::Impl(lib),
        _ => Want::Fn(lib),
    })
}

/// The want for checker key `key`: its own item's, or a member's container's.
pub fn want_of_key<'n, S: super::source::NamesSource<'n>>(names: S, keys: &Keys, key: ItemKey) -> Result<Want, FillError> {
    // A key in no row of the map is asked of the source (`Keys::library_in`: over records the
    // map is not filled up front; over the solved names the answer is `None` and the key is
    // unknown, as before).
    let container = match keys.origin(key) {
        Some(Origin::Item(lib)) => return want_of_item(names, *lib),
        Some(Origin::Member { container, .. }) => *container,
        None => return want_of_item(names, keys.library_in(names, key).ok_or(FillError::UnknownKey(key))?),
    };
    match keys.origin(container) {
        Some(Origin::Item(lib)) => want_of_item(names, *lib),
        Some(Origin::Member { .. }) => Err(FillError::UnknownKey(container)),
        None => want_of_item(names, keys.library_in(names, container).ok_or(FillError::UnknownKey(container))?),
    }
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
        | Question::AssocGenerics(k, _) => want_of_key(names, keys, k),
        _ => Err(FillError::NotAHead(q)),
    }
}
