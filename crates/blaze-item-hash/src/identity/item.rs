//! An item a body names, as the identity hashes it: its key (which item) and its signature
//! hash (`blaze_items::signature_hash`): its interface without what bodies leak into it.
//!
//! The identity is computed inside `infer_body_query`, so nothing it hashes may depend on the
//! inference of a body: an interface hash includes what bodies leak (an `async fn`'s or an
//! `impl Trait` return's hidden type, a `const fn`'s, a const's and a static's body), which
//! could reach back to the body being inferred, a query cycle. A signature hash leaves each
//! leak out; what a body reads of one (a hidden type, a constant's value) is a fact of its
//! trace instead (section 5). An item of any crate is hashed alike.

//!
//! The body's own item is "self" ([`SELF`] in place of its key): a rename keeps its bodies'
//! identities. Two items whose bodies are then alike share records, which name it relative to
//! the body (`crate::keys`); what its own steps read of it by key is its signature and
//! attributes, which section 3.2 hashes, or what its own body decides.

use base_db::Db;
use blaze_items::{ItemId, key};
use hir_def::{DefWithBodyId, EnumVariantId, Lookup};

/// An item's key and signature, back to back.
pub(crate) type Part = [u8; 64];

/// What the body's own item is written as in place of its key.
pub(crate) const SELF: [u8; 32] = *b"blaze v2 item identity: own item";

pub(crate) fn item_part(db: &Db, item: ItemId) -> Result<Part, &'static str> {
    Ok(join(&key::item_key(db, item).0.0, &signature(db, item)?))
}

/// The body's own item: [`SELF`] and its signature.
pub(crate) fn self_part(db: &Db, item: ItemId) -> Result<Part, &'static str> {
    Ok(join(&SELF, &signature(db, item)?))
}

/// The item `def` is the body of, which is "self" in its identity; `None` for a variant's
/// discriminant, whose enum is hashed by key.
pub(crate) fn own_item(def: DefWithBodyId) -> Option<ItemId> {
    match def {
        DefWithBodyId::FunctionId(it) => Some(ItemId::Function(it)),
        DefWithBodyId::ConstId(it) => Some(ItemId::Const(it)),
        DefWithBodyId::StaticId(it) => Some(ItemId::Static(it)),
        DefWithBodyId::VariantId(_) => None,
    }
}

/// A variant: its key and its enum's signature.
pub(crate) fn variant_part(db: &Db, variant: EnumVariantId) -> Result<Part, &'static str> {
    let enum_ = ItemId::Adt(variant.lookup(db).parent.into());
    Ok(join(&key::variant_key(db, variant).0.0, &signature(db, enum_)?))
}

fn signature(db: &Db, item: ItemId) -> Result<[u8; 32], &'static str> {
    blaze_items::signature_hash(db, item).map(|it| it.0.0).map_err(|it| it.what)
}

fn join(key: &[u8; 32], signature: &[u8; 32]) -> Part {
    let mut part = [0; 64];
    part[..32].copy_from_slice(key);
    part[32..].copy_from_slice(signature);
    part
}
