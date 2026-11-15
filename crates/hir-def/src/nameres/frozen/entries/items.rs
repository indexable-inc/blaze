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
//! | `assoc items` | impl or trait, how its file is read, its `header`; the crate's cfg when the header names `cfg` | each item's kind, name and place among the assoc items written |
//!
//! Only what is a function of that key is kept:
//!
//! - an item written outside any body in a file (a row in its file's index) or in an expansion
//!   with a key of its inputs (`stored::expansion::key`; its index found by that key, with no
//!   expansion); one an expansion with no key wrote (a proc macro's, an eager call's) is
//!   answered as before;
//! - of flags, every item but a module (whose attributes are of two files and the command
//!   line);
//! - of assoc items, an `impl` or `trait` that is `plain` (no macro call at assoc position
//!   and no attribute a crate could resolve as a macro), so no name is looked up to collect
//!   it. Any other reads which macro a path names, which is a fact its key does not hold yet.
//!
//! The values name nothing of a database and nothing of a file: an assoc item is its place
//! among the assoc items written in its container (the third written, whatever a cfg leaves
//! of them), and the reader makes its id from the container it asked of and the ast id its
//! own file's index lists at that place (`FileIndex::assoc_items`). An ast id is of its file
//! (its hash is of its parent's id, its index counts the file's other items of that hash), so
//! two `impl`s written the same, in one file or in two, have one key and other ids: a value
//! holding ids was two values under one key (layout 1; `agents/annoyances/assoc-key-1.json`).
//! Read by an `impl` it was not made for, a value can name only that `impl`'s own items.
//!
//! The cfg part is the crate's whole cfg where the hashed text names `cfg`: coarser than the
//! atoms the item's `cfg` predicates name (rule 65), which no row holds yet.
//!
//! The invariant: equal keys are equal bytes; an entry found is the answer the query makes.

use base_db::{Crate, Db};
use blaze_ids::{ContentId, Hasher};
use std::fmt::Write as _;

use hir_expand::{
    HirFileId, InFile, Intern, Lookup,
    declarative::stored::{
        self,
        index::{AssocItems, ItemSource},
    },
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
/// 2: an assoc item by its place among those written, where layout 1 held its ast id.
pub const LAYOUT: u32 = 2;

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
    /// A file, or an expansion with a key (`stored::expansion::key`).
    file: HirFileId,
    /// Its ast id in `file`: what the file's index lists its assoc items under and an error
    /// names it by. Of the file, so never part of a key or a value.
    ast: ErasedFileAstId,
    /// The set its entries are packed in: its file's, or its expansion's key
    /// (`stored::set_of`).
    set: ContentId,
    /// How its text is read (`stored::set_of`).
    read_as: u8,
    krate: Crate,
    source: ItemSource,
}

/// The place of the item `lookup` names: `None` when this database keeps no entries, when a
/// macro whose expansion has no key wrote the item, or when it has no row in its file's index
/// (it is in a body).
fn place<N: AstIdNode>(
    db: &Db,
    lookup: impl Lookup<Data = impl AstIdLoc<Ast = N> + HasModule> + Copy,
) -> Option<Place> {
    of(db)?;
    let loc = lookup.lookup(db);
    let id = loc.ast_id();
    let file = id.file_id;
    let krate = loc.krate(db);
    let ast = id.value.erase();
    let source = stored::index::source(db, file, ast, krate)?;
    let (set, read_as) = stored::set_of(db, file)?;
    Some(Place { file, ast, set, read_as, krate, source })
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
// a place. `IDS_IN_VALUE`, the planted bug of layout 1: with it on, an assoc value holds each
// item's ast id where it holds its place, so two `impl`s written the same are two values
// under one key, and one reads the other's items.
#[cfg(test)]
thread_local! {
    pub(super) static KEYED_BY_FILE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(super) static IDS_IN_VALUE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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

fn ids_in_value() -> bool {
    #[cfg(test)]
    if IDS_IN_VALUE.get() {
        return true;
    }
    false
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

/// Refuses the entry of `family` of the item at `place`, naming the item (rule 19): a key
/// alone names nothing a person can find.
fn refuse(db: &Db, place: &Place, family: &'static str, why: String) -> ! {
    let cfg = if family == FLAGS { place.source.attrs_cfg } else { place.source.header_cfg };
    // alloc: error path
    Unreadable::of(db, place.krate, family, format!("{}: {why}", says(db, place, cfg))).throw()
}

/// The item at `place` as a refusal names it: its first line, its file and line, which build
/// of its crate asked, its ast id, and whether its key took the crate's cfg (`cfg`). An
/// error's path: it reads the file's text and its ast ids.
fn says(db: &Db, place: &Place, cfg: bool) -> String {
    let file = match place.file {
        HirFileId::FileId(file) => file.file_id(db),
        HirFileId::MacroFile(call) => {
            let takes = if cfg { "takes" } else { "does not take" };
            // alloc: error path
            let mut out = format!("an item of the expansion {call:?} (ast id {:?}) of ", place.ast);
            crate::span_names::write_crate_label(db, place.krate, &mut out);
            _ = write!(out, " (its key {takes} the crate's cfg)");
            return out;
        }
    };
    let text = db.file_text(file).text(db);
    let node = place.file.ast_id_map(db).try_get_erased(place.ast);
    let (line, first) = match node {
        Some(node) => {
            let range = node.text_range();
            let (start, end) = (usize::from(range.start()), usize::from(range.end()));
            let line = text.get(..start).map_or(0, |it| it.matches('\n').count()) + 1;
            // Its first line that is not an attribute or a comment.
            let first = text
                .get(start..end)
                .unwrap_or("")
                .lines()
                .map(str::trim)
                .find(|it| !it.is_empty() && !it.starts_with("//") && !it.starts_with('#'))
                .unwrap_or("");
            (line, first)
        }
        None => (0, ""),
    };
    // alloc: error path
    let mut out = String::new();
    _ = write!(out, "`{first}` at ");
    match db.path_for_file(file) {
        Some(path) => _ = write!(out, "{path}"),
        None => _ = write!(out, "{file:?}, which has no path"),
    }
    _ = write!(out, ":{line} of ");
    crate::span_names::write_crate_label(db, place.krate, &mut out);
    let takes = if cfg { "takes" } else { "does not take" };
    _ = write!(out, " (ast id {:?}; its key {takes} the crate's cfg)", place.ast);
    out
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
    // The assoc items written in this one, by the ast ids they have in its own file. The
    // index is there: `place` found the impl's row in it.
    let Some(index) = stored::index::of_any(db, file, place.krate) else { return make() };
    let listed = index.assoc_items(place.ast);
    let read = |bytes: &[u8]| decode_assoc(db, file, listed, container, bytes);
    match entry(db, ASSOC, &place.set, key, read) {
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
    if made.1.is_empty() && made.2.iter().next().is_none() {
        match encode_assoc(db, file, listed, &made.0) {
            Ok(Some(bytes)) => {
                if let Err(why) = keep(db, ASSOC, &place.set, key, &bytes) {
                    refuse(db, &place, ASSOC, why)
                }
            }
            Ok(None) => {}
            // alloc: error path
            Err(why) => refuse(db, &place, ASSOC, format!("its value is not made: {why}")),
        }
    }
    made
}

/// The assoc items as a value: each by its kind, its place among the assoc items written in
/// its `impl` or `trait` (`listed`, its file's index of them) and its name. No ast id: one is
/// of its file, and this value is found by every `impl` written the same, wherever it is.
/// `Ok(None)` when one is not of `file` or has a name with a hygiene key, which no plain
/// impl's has; `Err` when one is not among `listed`, which is the index and the item's file
/// disagreeing.
fn encode_assoc(
    db: &Db,
    file: HirFileId,
    listed: AssocItems<'_>,
    items: &[(Name, AssocItemId)],
) -> R<Option<Vec<u8>>> {
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
        if id.file_id != file || name.key().is_some() {
            return Ok(None);
        }
        let text = name.symbol().as_str();
        let at = if ids_in_value() {
            id.value.into_raw()
        } else {
            match listed.position(id.value) {
                Some(at) => at as u32,
                None => {
                    // alloc: error path
                    return Err(format!(
                        "assoc item `{text}` ({:?}) is not one of the {} its file's index lists \
                         for its `impl` or `trait`",
                        id.value,
                        listed.len()
                    ));
                }
            }
        };
        out.push(kind);
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }
    Ok(Some(out))
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

/// The assoc items a value holds, as those of `container`: each one's id is the ast id
/// `listed` (the container's own assoc items, of its own file's index) has at the place the
/// value names, so a value names no node but the container's own. `Err` when the value names
/// a place `listed` has none at, or an item of another kind than is written there: bytes
/// under a key that does not cover them.
fn decode_assoc(
    db: &Db,
    file: HirFileId,
    listed: AssocItems<'_>,
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
    for _ in 0..count {
        let kind = input.take(1)?[0];
        let at = input.word()?;
        let len = input.word()? as usize;
        let text = std::str::from_utf8(input.take(len)?).map_err(|_| "a name that is not UTF-8")?;
        let ast = if ids_in_value() {
            ErasedFileAstId::from_raw(at)
        } else {
            listed.get(at as usize).ok_or_else(|| {
                // alloc: error path
                format!(
                    "it names assoc item {at} (`{text}`), and the file's index lists {} written \
                     in this `impl` or `trait`",
                    listed.len()
                )
            })?
        };
        let written = match kind {
            FN => ast.may_be::<syntax::ast::Fn>(),
            CONST => ast.may_be::<syntax::ast::Const>(),
            TYPE => ast.may_be::<syntax::ast::TypeAlias>(),
            // alloc: error path
            other => return Err(format!("an assoc item of kind {other}")),
        };
        if !written {
            // alloc: error path
            return Err(format!(
                "it holds assoc item {at} (`{text}`) as kind {kind}, and {ast:?} is written there"
            ));
        }
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
