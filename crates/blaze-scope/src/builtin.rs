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
pub const PREFIX: [u8; 8] = *b"builtin\0";

/// The definition `name` is as a builtin type: a key of its own, the same in every crate and
/// on every machine, so a record that names `u8` names one thing.
pub fn builtin_type(name: &str) -> Option<Def> {
    let at = BUILTIN_TYPES.binary_search(&name).ok()?;
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&PREFIX);
    bytes[8] = u8::try_from(at).ok()? + 1;
    Some(Def { key: ItemKey(ContentId(bytes)), kind: DefKind::Other })
}

/// Byte 8 of an integer of any width's key, `| 1` when signed: above every named type's index
/// (`1..=19`), so the two kinds of key never meet. Byte 9 is the width; the rest are zero.
const ARBITRARY_INT: u8 = 0x80;

/// The definition `name` is as a builtin type in a crate whose root says
/// `#![blaze::arbitrary_ints]` or not (`arbitrary_ints`): a named builtin, or with the
/// attribute an integer of any width, `u1`..`u127` and `i1`..`i127` but the named widths
/// (docs/v2/plan/design/arbitrary-ints.md). A crate without it may name its own `u24`.
pub fn builtin_type_in(name: &str, arbitrary_ints: bool) -> Option<Def> {
    if let Some(def) = builtin_type(name) {
        return Some(def);
    }
    if !arbitrary_ints {
        return None;
    }
    let (signed, bits) = arbitrary_int(name)?;
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&PREFIX);
    bytes[8] = ARBITRARY_INT | u8::from(signed);
    bytes[9] = bits;
    Some(Def { key: ItemKey(ContentId(bytes)), kind: DefKind::Other })
}

/// Whether `bits` is the width of an integer of any width: `1..=127`, not a named width.
fn arbitrary_width(bits: u8) -> bool {
    (1..=127).contains(&bits) && !matches!(bits, 8 | 16 | 32 | 64)
}

/// `u<N>` or `i<N>` as (signed, N): N in decimal with no leading zero and an arbitrary width.
fn arbitrary_int(name: &str) -> Option<(bool, u8)> {
    let (signed, digits) = match name.as_bytes().split_first()? {
        (b'u', digits) => (false, digits),
        (b'i', digits) => (true, digits),
        _ => return None,
    };
    if digits.is_empty() || digits.len() > 3 || digits[0] == b'0' || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut bits = 0u16;
    for &d in digits {
        bits = bits * 10 + u16::from(d - b'0');
    }
    let bits = u8::try_from(bits).ok()?;
    arbitrary_width(bits).then_some((signed, bits))
}

/// A builtin type, as the front end takes it from its key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Builtin {
    /// One of [`BUILTIN_TYPES`].
    Named(&'static str),
    /// `u<bits>` or `i<bits>` under `#![blaze::arbitrary_ints]`.
    Int { signed: bool, bits: u8 },
}

/// The builtin type `key` is the key of ([`builtin_type_in`]'s), or `None`.
pub fn builtin_of(key: ItemKey) -> Option<Builtin> {
    let b = &key.0.0;
    if b[..8] != PREFIX || b[10..].iter().any(|&x| x != 0) {
        return None;
    }
    match b[8] {
        0 => None,
        at if at & !1 == ARBITRARY_INT => arbitrary_width(b[9]).then_some(Builtin::Int { signed: at & 1 == 1, bits: b[9] }),
        at if b[9] == 0 => BUILTIN_TYPES.get(usize::from(at - 1)).map(|&name| Builtin::Named(name)),
        _ => None,
    }
}

/// Whether `key` is a builtin type's.
pub fn is_builtin_type(key: ItemKey) -> bool {
    key.0.0[..8] == PREFIX
}

#[cfg(test)]
mod tests {
    use std::format;
    use std::vec::Vec;

    use blaze_id::ItemKey;

    use super::{BUILTIN_TYPES, Builtin, builtin_of, builtin_type, builtin_type_in};
    use crate::tests::{V, World, solve};
    use crate::{DefKind, Edition, Namespace};

    /// Rule (arbitrary-ints.md, "The opt-in"): `u1`..`u128` and `i1`..`i128` are integer types
    /// in a crate that says `#![blaze::arbitrary_ints]`, the standard widths keeping their own
    /// names, so every width has one spelling; `u8`, `i128` stay the named builtins, and `u0`,
    /// `i200`, `u08`, `u24x` are no type. Each key turns back into its type.
    #[test]
    fn arbitrary_widths_are_builtin_types_by_one_spelling() {
        for (name, signed, bits) in [("i80", true, 80), ("u24", false, 24), ("u1", false, 1), ("i127", true, 127)] {
            let key = builtin_type_in(name, true).map(|d| d.key);
            assert_eq!(key.and_then(builtin_of), Some(Builtin::Int { signed, bits }), "{name}");
            assert_eq!(builtin_type_in(name, false), None, "{name}");
        }
        for name in ["u8", "i128", "u128", "bool"] {
            assert_eq!(builtin_type_in(name, true), builtin_type(name), "{name}");
            let named = builtin_type(name).map(|d| d.key).and_then(builtin_of);
            assert_eq!(named, Some(Builtin::Named(name)), "{name}");
        }
        for name in ["u0", "i200", "u08", "u24x", "u", "i", "x24", "u-1", "u256"] {
            assert_eq!(builtin_type_in(name, true), None, "{name}");
        }
        let mut keys: Vec<ItemKey> = BUILTIN_TYPES.iter().filter_map(|n| builtin_type(n)).map(|d| d.key).collect();
        for bits in 1..=127u16 {
            for sign in ["u", "i"] {
                let name = format!("{sign}{bits}");
                if builtin_type(&name).is_none() {
                    keys.extend(builtin_type_in(&name, true).map(|d| d.key));
                }
            }
        }
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!((n, keys.len()), (19 + 2 * 123, 19 + 2 * 123), "every builtin type has its own key");
    }

    /// Rule (arbitrary-ints.md: "They are real primitive types, shadowed like `u8`"; "a crate
    /// that doesn't say it is unchanged, including one that defines its own `struct u3`"):
    /// with the attribute `i80` names the builtin and a local `u24` shadows the builtin as a
    /// local `u8` does; without it `i80` names nothing and a local `u24` is the local item.
    /// Expected: "test blaze-scope: exit 0; 1 passed, 0 failed". Fails without names-G2-G3's
    /// `Cells::builtin` (`builtin_type_in` with the root's flag): `i80` NotFound.
    #[test]
    fn arbitrary_widths_resolve_only_under_the_attribute() {
        for arbitrary_ints in [true, false] {
            let mut w = World::new();
            let root = w.root(1, Edition::E2021);
            w.cr(root).head.arbitrary_ints = arbitrary_ints;
            let local = w.item(root, "u24", Namespace::Types, DefKind::Other, V::Priv);
            w.use_as(root, "u24", "mine", V::Priv);
            let p = w.module(root, "p", V::Pub);
            w.use_(p, "i80", V::Pub);
            w.use_(p, "u24", V::Pub);
            let s = match solve(&w, None) {
                Ok(s) => s,
                Err(e) => panic!("the driver failed: {e}"),
            };
            assert_eq!(s.def(root, Namespace::Types, "mine"), Some(local), "attribute: {arbitrary_ints}");
            let i80 = s.def(p, Namespace::Types, "i80");
            let u24 = s.def(p, Namespace::Types, "u24");
            if arbitrary_ints {
                assert!(s.errors(p).is_empty(), "{:?}", s.errors(p));
                assert_eq!(i80.and_then(builtin_of), Some(Builtin::Int { signed: true, bits: 80 }));
                assert_eq!(u24.and_then(builtin_of), Some(Builtin::Int { signed: false, bits: 24 }));
            } else {
                assert_eq!((i80, u24), (None, None));
            }
        }
    }
}
