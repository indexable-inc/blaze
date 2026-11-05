//! Section 3.4: what each id a body's portable bytes name is in every database. An item, a
//! variant and a crate by their keys (an item and a variant with the signature inference reads
//! of them, [`super::item`]); a file, a macro call, a syntax context and a block as
//! [`PortableBody::fingerprint`] writes them. The body's own item is "self" ([`super::item`]).

use base_db::{Crate, Db};
use blaze_items::{ItemId, key};
use hir_def::{
    MacroId,
    item_hash::{Entry, PortableBody},
};

use super::item::{item_part, self_part, variant_part};

/// The entries of `body`, a body of `home` whose own item is `own`, into `out`.
pub(super) fn write(
    db: &Db,
    home: Crate,
    own: Option<ItemId>,
    body: &PortableBody,
    out: &mut Vec<u8>,
) -> Result<(), &'static str> {
    let print = body
        .fingerprint(db, home)
        .map_err(|_| "an id of its HIR has no form that is the same in every database")?;
    out.extend_from_slice(&(print.len() as u64).to_le_bytes());
    out.extend_from_slice(&print);
    for entry in body.entries() {
        let item = match entry {
            Entry::Crate(it) => {
                out.extend_from_slice(&key::crate_key(db, it).0);
                continue;
            }
            Entry::EnumVariant(it) => {
                out.extend_from_slice(&variant_part(db, it)?);
                continue;
            }
            Entry::File(_)
            | Entry::SpanFile(_)
            | Entry::MacroCall(_)
            | Entry::Block(_)
            | Entry::Context(_) => continue,
            Entry::Function(it) => ItemId::Function(it),
            Entry::Struct(it) => ItemId::Adt(it.into()),
            Entry::Union(it) => ItemId::Adt(it.into()),
            Entry::Enum(it) => ItemId::Adt(it.into()),
            Entry::Const(it) => ItemId::Const(it),
            Entry::Static(it) => ItemId::Static(it),
            Entry::Trait(it) => ItemId::Trait(it),
            Entry::TypeAlias(it) => ItemId::Alias(it),
            Entry::Impl(it) => ItemId::Impl(it),
            Entry::Macro2(it) => ItemId::Macro(MacroId::Macro2Id(it)),
            Entry::MacroRules(it) => ItemId::Macro(MacroId::MacroRulesId(it)),
            Entry::ProcMacro(it) => ItemId::Macro(MacroId::ProcMacroId(it)),
        };
        let part = match Some(item) == own {
            true => self_part(db, item)?,
            false => item_part(db, item)?,
        };
        out.extend_from_slice(&part);
    }
    Ok(())
}
