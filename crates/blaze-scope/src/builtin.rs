//! The language's builtin types (rustc's `PrimTy`): the last lexical layer of the type
//! namespace, in scope everywhere (`#[no_implicit_prelude]` too), shadowed by any nearer
//! binding of the name (`core`'s `pub mod u8` included). `core/src/primitive.rs` imports them
//! by name (`pub use bool;`).

#![blaze::sans_io]

use blaze_id::{ContentId, ItemKey};
use blaze_surface::{Def, DefKind};

/// Their names, in the order of their text.
pub const BUILTIN_TYPES: [&str; 19] = [
    "bool", "char", "f128", "f16", "f32", "f64", "i128", "i16", "i32", "i64", "i8", "isize", "str",
    "u128", "u16", "u32", "u64", "u8", "usize",
];

/// The first eight bytes of a builtin type's key, where an item's key holds its crate's
/// prefix (a hash, never these bytes but by a 2^-64 chance).
const PREFIX: [u8; 8] = *b"builtin\0";

/// The definition `name` is as a builtin type: a key of its own, the same in every crate and
/// on every machine, so a record that names `u8` names one thing.
pub fn builtin_type(name: &str) -> Option<Def> {
    let at = BUILTIN_TYPES.binary_search(&name).ok()?;
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&PREFIX);
    bytes[8] = u8::try_from(at).ok()? + 1;
    Some(Def { key: ItemKey(ContentId(bytes)), kind: DefKind::Other })
}

/// Whether `key` is a builtin type's.
pub fn is_builtin_type(key: ItemKey) -> bool {
    key.0.0[..8] == PREFIX
}
