//! What is asked of one item after its name is found, as an entry of that item (AGENTS.md
//! rule 65): its attribute flags, and the assoc items of an `impl` or a `trait`.
//!
//! An entry is the answer of the query it stands before, under a key made of the item's own
//! source hashes (its row of its file's index, `hir_expand::declarative::stored::index`) and
//! what the answer read beside them. The query asks here first: found, it parses nothing and
//! collects nothing; not found, it runs as it always did and its answer is kept. No key names
//! a crate, a file or a module, so an entry is found by every crate and every revision the
//! item is written the same in.
//!
//! | family | key | value |
//! |---|---|---|
//! | `attr flags` | the kind of item, how its file is read, its `attrs`; the crate's cfg when the attributes name `cfg` | the flags |
//! | `assoc items` | impl or trait, how its file is read, its `header`; the crate's cfg when the header names `cfg` | each item's kind, ast id and name |
//!
//! Only what is a function of that key is kept:
//!
//! - an item written in a file and outside any body (one with a row in its file's index); one
//!   a macro wrote is answered as before, until an expansion has a key of its inputs;
//! - of flags, every item but a module (whose attributes are of two files and the command
//!   line);
//! - of assoc items, an `impl` or `trait` that is `plain` (no macro call at assoc position
//!   and no attribute a crate could resolve as a macro), so no name is looked up to collect
//!   it. Any other reads which macro a path names, which is a fact its key does not hold yet.
//!
//! The values name nothing of a database: an assoc item is its ast id in the file of its
//! container, and the reader makes its id from the container it asked of.
//!
//! The invariant: equal keys are equal bytes; an entry found is the answer the query makes.

use base_db::{Crate, Db, EditionedFileId};
use blaze_ids::{ContentId, Hasher};
use hir_expand::{
    HirFileId, InFile, Intern, Lookup,
    declarative::stored::{self, index::ItemSource},
    name::Name,
};
use intern::Symbol;
use span::{AstIdNode, ErasedFileAstId, FileAstId};
use thin_vec::ThinVec;

use super::{R, entry, keep, of};
use crate::{
    AdtId, AssocItemId, AstIdLoc, AttrDefId, ConstLoc, FunctionLoc, HasModule, ImplId,
    ItemContainerId, MacroId, TraitId, TypeAliasLoc,
    attrs::AttrFlags,
    nameres::{
        assoc::{ImplItems, TraitItems},
        diagnostics::DefDiagnostics,
        frozen::Unreadable,
    },
};

#[cfg(test)]
mod tests;

/// The layout of both families: part of their keys, and the first word of an assoc value.
pub const LAYOUT: u32 = 1;

/// The family of an item's attribute flags, as the entry store names it.
pub const FLAGS: &str = "attr flags";
/// The family of the assoc items of an `impl` or a `trait`.
pub const ASSOC: &str = "assoc items";

const FLAGS_DOMAIN: &str = "blaze v2 attr flags";
const ASSOC_DOMAIN: &str = "blaze v2 assoc items";

/// Whose assoc items an entry holds.
const IMPL: u8 = 0;
const TRAIT: u8 = 1;

/// The kind of an assoc item in a value.
const FN: u8 = 0;
const CONST: u8 = 1;
const TYPE: u8 = 2;

/// Where an item is written and what it is made of: all a key takes.
struct Place {
    file: EditionedFileId,
    /// The set its entries are packed in: its file's.
    set: ContentId,
    /// How its file is read (`stored::read_as`).
    read_as: u8,
    krate: Crate,
    source: ItemSource,
}

/// The place of the item `lookup` names: `None` when this database keeps no entries, when a
/// macro wrote the item, or when it has no row in its file's index (it is in a body).
fn place<N: AstIdNode>(
    db: &Db,
    lookup: impl Lookup<Data = impl AstIdLoc<Ast = N> + HasModule> + Copy,
) -> Option<Place> {
    of(db)?;
    let loc = lookup.lookup(db);
    let id = loc.ast_id();
    let HirFileId::FileId(file) = id.file_id else { return None };
    let krate = loc.krate(db);
    let source = stored::index::of(db, file, krate).get(id.value.erase())?;
    Some(Place {
        file,
        set: stored::file_set(db, file),
        read_as: stored::read_as(db, file),
        krate,
        source,
    })
}

/// Feeds the cfg of the place's crate where `names` says the hashed text names `cfg`; else
/// only that it does not. An item with no `cfg` in what is read of it is the same under
/// every cfg.
fn cfg_part(db: &Db, hasher: &mut Hasher, place: &Place, names: bool) {
    if names {
        hasher.update(&[1]).id(&base_db::cfg_options_content(place.krate.cfg_options(db)));
    } else {
        hasher.update(&[0]);
    }
}

/// The key of the flags of an item of `kind` at `place`.
fn flags_key(db: &Db, kind: u8, place: &Place, mixed: Option<&ContentId>) -> ContentId {
    let mut hasher = Hasher::new(FLAGS_DOMAIN);
    hasher
        .update(&LAYOUT.to_le_bytes())
        .update(&[kind, place.read_as])
        .update(&place.source.attrs.to_le_bytes());
    cfg_part(db, &mut hasher, place, place.source.attrs_cfg);
    if let Some(mixed) = mixed {
        hasher.id(mixed);
    }
    hasher.finish()
}

/// The key of the assoc items of the `impl` or `trait` (`whose`) at `place`.
fn assoc_key(db: &Db, whose: u8, place: &Place, mixed: Option<&ContentId>) -> ContentId {
    let mut hasher = Hasher::new(ASSOC_DOMAIN);
    hasher
        .update(&LAYOUT.to_le_bytes())
        .update(&[whose, place.read_as])
        .update(&place.source.header.to_le_bytes());
    cfg_part(db, &mut hasher, place, place.source.header_cfg);
    if let Some(mixed) = mixed {
        hasher.id(mixed);
    }
    hasher.finish()
}

// A test's planted bug, `KEYED_BY_FILE`: with it on, a key also takes its file's set (its
// content), as a key of a file or a crate would, and an edit anywhere in the file misses every
// entry of it. `RAN`: how many times this thread ran a query's own work for an item that has
// a place.
#[cfg(test)]
thread_local! {
    pub(super) static KEYED_BY_FILE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(super) static RAN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn mixed(place: &Place) -> Option<&ContentId> {
    #[cfg(test)]
    if KEYED_BY_FILE.get() {
        return Some(&place.set);
    }
    _ = place;
    None
}

fn ran() {
    #[cfg(test)]
    RAN.set(RAN.get() + 1);
}

/// The place of an owner whose flags are of its own attributes alone, and its kind.
fn flags_place(db: &Db, owner: AttrDefId) -> Option<(u8, Place)> {
    Some(match owner {
        // Its attributes are its declaration's, its file's and, of a crate's root, the
        // command line's: not one row.
        AttrDefId::ModuleId(_) => return None,
        AttrDefId::AdtId(AdtId::StructId(it)) => (1, place(db, it)?),
        AttrDefId::AdtId(AdtId::UnionId(it)) => (2, place(db, it)?),
        AttrDefId::AdtId(AdtId::EnumId(it)) => (3, place(db, it)?),
        AttrDefId::FunctionId(it) => (4, place(db, it)?),
        AttrDefId::EnumVariantId(it) => (5, place(db, it)?),
        AttrDefId::StaticId(it) => (6, place(db, it)?),
        AttrDefId::ConstId(it) => (7, place(db, it)?),
        AttrDefId::TraitId(it) => (8, place(db, it)?),
        AttrDefId::TypeAliasId(it) => (9, place(db, it)?),
        AttrDefId::MacroId(MacroId::MacroRulesId(it)) => (10, place(db, it)?),
        AttrDefId::MacroId(MacroId::Macro2Id(it)) => (11, place(db, it)?),
        AttrDefId::MacroId(MacroId::ProcMacroId(it)) => (12, place(db, it)?),
        AttrDefId::ImplId(it) => (13, place(db, it)?),
        AttrDefId::ExternBlockId(it) => (14, place(db, it)?),
        AttrDefId::ExternCrateId(it) => (15, place(db, it)?),
        AttrDefId::UseId(it) => (16, place(db, it)?),
    })
}

fn refuse(db: &Db, place: &Place, family: &'static str, why: String) -> ! {
    Unreadable::of(db, place.krate, family, why).throw()
}

/// `AttrFlags::query` of `owner`: its entry where one is kept, else what `make` (the query's
/// own work) answers, kept where the owner has a place.
pub(crate) fn attr_flags(db: &Db, owner: AttrDefId, make: impl FnOnce() -> AttrFlags) -> AttrFlags {
    let Some((kind, place)) = flags_place(db, owner) else { return make() };
    let key = flags_key(db, kind, &place, mixed(&place));
    match entry(db, FLAGS, &place.set, key, decode_flags) {
        Ok(Some(found)) => return found,
        Ok(None) => {}
        Err(why) => refuse(db, &place, FLAGS, why),
    }
    ran();
    let flags = make();
    if let Err(why) = keep(db, FLAGS, &place.set, key, &flags.bits().to_le_bytes()) {
        refuse(db, &place, FLAGS, why)
    }
    flags
}

fn decode_flags(bytes: &[u8]) -> R<AttrFlags> {
    let bytes: [u8; 16] =
        // alloc: error path
        bytes.try_into().map_err(|_| format!("{} bytes, and flags are 16", bytes.len()))?;
    Ok(AttrFlags::from_bits_retain(u128::from_le_bytes(bytes)))
}

/// What collecting the assoc items of a trait or an impl gives.
type Assoc = (
    Box<[(Name, AssocItemId)]>,
    ThinVec<(hir_expand::AstId<syntax::ast::Item>, crate::MacroCallId)>,
    DefDiagnostics,
);

/// `ImplItems::of` of `id`: its entry where one is kept, else what `make` answers, kept where
/// the impl is plain and has a place.
pub(crate) fn impl_items(
    db: &Db,
    id: ImplId,
    make: impl FnOnce() -> (ImplItems, DefDiagnostics),
) -> (ImplItems, DefDiagnostics) {
    let (items, macro_calls, diagnostics) =
        assoc(db, IMPL, place(db, id), ItemContainerId::ImplId(id), || {
            let (ImplItems { items, macro_calls }, diagnostics) = make();
            (items, macro_calls, diagnostics)
        });
    (ImplItems { items, macro_calls }, diagnostics)
}

/// `TraitItems::query_with_diagnostics` of `id`, as [`impl_items`].
pub(crate) fn trait_items(
    db: &Db,
    id: TraitId,
    make: impl FnOnce() -> (TraitItems, DefDiagnostics),
) -> (TraitItems, DefDiagnostics) {
    let (items, macro_calls, diagnostics) =
        assoc(db, TRAIT, place(db, id), ItemContainerId::TraitId(id), || {
            let (TraitItems { items, macro_calls }, diagnostics) = make();
            (items, macro_calls, diagnostics)
        });
    (TraitItems { items, macro_calls }, diagnostics)
}

fn assoc(
    db: &Db,
    whose: u8,
    place: Option<Place>,
    container: ItemContainerId,
    make: impl FnOnce() -> Assoc,
) -> Assoc {
    let Some(place) = place.filter(|it| it.source.plain) else { return make() };
    let key = assoc_key(db, whose, &place, mixed(&place));
    let file = place.file;
    match entry(db, ASSOC, &place.set, key, |bytes| decode_assoc(db, file, container, bytes)) {
        Ok(Some(items)) => {
            // alloc: empty; allocates nothing
            return (items, ThinVec::new(), DefDiagnostics::new(Vec::new()));
        }
        Ok(None) => {}
        Err(why) => refuse(db, &place, ASSOC, why),
    }
    ran();
    let made = make();
    // A plain impl's collection reports nothing and expands nothing but an item its cfg
    // leaves out (a diagnostic naming its module): that one is not kept, and is made again.
    if made.1.is_empty()
        && made.2.iter().next().is_none()
        && let Some(bytes) = encode_assoc(db, file, &made.0)
        && let Err(why) = keep(db, ASSOC, &place.set, key, &bytes)
    {
        refuse(db, &place, ASSOC, why)
    }
    made
}

/// The assoc items as a value: `None` when one is not of `file` or has a name with a hygiene
/// key, which no plain impl's has.
fn encode_assoc(db: &Db, file: EditionedFileId, items: &[(Name, AssocItemId)]) -> Option<Vec<u8>> {
    // alloc: one impl's value, a few dozen bytes an item
    let mut out = Vec::new();
    out.extend_from_slice(&LAYOUT.to_le_bytes());
    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for (name, item) in items {
        let (kind, id) = match *item {
            AssocItemId::FunctionId(it) => (FN, it.lookup(db).id.map(|it| it.erase())),
            AssocItemId::ConstId(it) => (CONST, it.lookup(db).id.map(|it| it.erase())),
            AssocItemId::TypeAliasId(it) => (TYPE, it.lookup(db).id.map(|it| it.erase())),
        };
        if id.file_id != HirFileId::FileId(file) || name.key().is_some() {
            return None;
        }
        let text = name.symbol().as_str();
        out.push(kind);
        out.extend_from_slice(&id.value.into_raw().to_le_bytes());
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }
    Some(out)
}

/// What is left of a value being read.
struct In<'a>(&'a [u8]);

impl<'a> In<'a> {
    fn take(&mut self, len: usize) -> R<&'a [u8]> {
        if self.0.len() < len {
            // alloc: error path
            return Err(format!("{} bytes where {len} are needed", self.0.len()));
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(taken)
    }

    fn word(&mut self) -> R<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }
}

fn decode_assoc(
    db: &Db,
    file: EditionedFileId,
    container: ItemContainerId,
    bytes: &[u8],
) -> R<Box<[(Name, AssocItemId)]>> {
    let mut input = In(bytes);
    let layout = input.word()?;
    if layout != LAYOUT {
        // alloc: error path
        return Err(format!("assoc items layout {layout}, and this reads layout {LAYOUT}"));
    }
    let count = input.word()? as usize;
    // alloc: the query's value, one impl's items; a count the bytes cannot hold fails below
    let mut items = Vec::with_capacity(count.min(bytes.len() / 9));
    let file = HirFileId::FileId(file);
    for _ in 0..count {
        let kind = input.take(1)?[0];
        let ast = ErasedFileAstId::from_raw(input.word()?);
        let len = input.word()? as usize;
        let text = std::str::from_utf8(input.take(len)?).map_err(|_| "a name that is not UTF-8")?;
        let name = Name::new_symbol_root(Symbol::intern(text));
        let item = match kind {
            FN => {
                let id = InFile::new(file, FileAstId::from_erased_unchecked(ast));
                AssocItemId::FunctionId(FunctionLoc { container, id }.intern(db))
            }
            CONST => {
                let id = InFile::new(file, FileAstId::from_erased_unchecked(ast));
                AssocItemId::ConstId(ConstLoc { container, id }.intern(db))
            }
            TYPE => {
                let id = InFile::new(file, FileAstId::from_erased_unchecked(ast));
                AssocItemId::TypeAliasId(TypeAliasLoc { container, id }.intern(db))
            }
            // alloc: error path
            other => return Err(format!("an assoc item of kind {other}")),
        };
        items.push((name, item));
    }
    if !input.0.is_empty() {
        // alloc: error path
        return Err(format!("{} bytes after the last item", input.0.len()));
    }
    Ok(items.into_boxed_slice())
}
