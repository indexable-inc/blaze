//! The name an instance's PGO counts are kept under, made of its item and its arguments: what
//! the LLVM back end names a function by while it instruments it or reads its profile
//! (`hir_ty_codegen`'s `llvm::pgo::name`), and what `blaze check`'s profile lints look a
//! function up by (`blaze_lints`' `profile`). One definition, so the two never drift.

use blaze_ids::{ContentId, Hasher};

use crate::ItemKey;

/// The domain of [`name_bytes`].
pub const NAME_DOMAIN: &str = "blaze v2 pgo name 1";

/// The domain of an instance's arguments' content ID (`hir_ty_codegen`'s `args_of`): each type
/// or constant argument's content ID in order, regions left out.
pub const ARGS_DOMAIN: &str = "blaze v2 args 1";

/// What a PGO name starts with: `blaze.pgo.` and [`name_bytes`] in lowercase hex.
pub const NAME_PREFIX: &str = "blaze.pgo.";

/// The bytes a function's PGO name spells, of its item and its arguments' content ID.
pub fn name_bytes(item: ItemKey, args: ContentId) -> [u8; 16] {
    let id = Hasher::new(NAME_DOMAIN).id(&item.0).id(&args).finish();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&id.0[..16]);
    bytes
}

/// The arguments' content ID of an instance with no type or constant arguments (a function
/// that is generic over lifetimes alone has the same): [`ARGS_DOMAIN`] over nothing.
pub fn no_args() -> ContentId {
    Hasher::new(ARGS_DOMAIN).finish()
}

/// The bytes of a PGO name, `blaze.pgo.` and 32 lowercase hex digits; `None` for any other
/// name.
pub fn parse_name(name: &[u8]) -> Option<[u8; 16]> {
    let hex = name.strip_prefix(NAME_PREFIX.as_bytes())?;
    if hex.len() != 32 {
        return None;
    }
    let digit = |it: u8| match it {
        b'0'..=b'9' => Some(it - b'0'),
        b'a'..=b'f' => Some(it - b'a' + 10),
        _ => None,
    };
    let mut bytes = [0; 16];
    for (at, pair) in hex.chunks_exact(2).enumerate() {
        bytes[at] = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(bytes)
}
