//! A crate's names read BY CELL (drive.md, "Names at module grain" point 3, READS ARE PER CELL;
//! lane CELL, 2026-11-17). Until the module-grain kinds replace it (point 7), a body or head
//! reads its crate's `CrateScope` record, and read WHOLE it is dirtied by every change of the
//! crate's names: one private fn added made every `Signature` and `BodyCheck` of the crate again
//! (blaze-cli signature_grain_tests.rs `a_private_item_added_makes_itself_alone`: (7, 7) where
//! the floor is (1, 1)). Read by cell, a reader is dirtied only when a cell its walks consulted
//! holds another digest today.
//!
//! THE CELLS ([`NameCell`]), each a 16-byte key ([`NameCell::key`]) in its own domain:
//! - `Binding(module, ns, name)`: the module's own binding of the name ([`cell_key`]), or
//!   absent: what [`ScopeRead::get`] answered;
//! - `Module(module)`: everything else the walk reads of the module but its textual macros: its
//!   chain and flags (`#[no_implicit_prelude]`, a block's module), its glob edges, its first
//!   refused call, and the traits it binds (named and `as _`); absent for a module the record
//!   does not hold;
//! - `Textual(module)`: the module's `macro_rules!` sequence and the marks of its `mod` lines
//!   in it: what a textual macro lookup reads ([`LoggedScope`]'s `textual` and `marked`), and
//!   nothing else does. Apart from `Module` so a `macro_rules!` added moves no path walk
//!   through the module (blaze-cli engine_scope_reads_tests.rs, g203837: a `#[macro_use]`
//!   macro added entered its module's 7 bodies and 11 heads, all identical);
//! - `Root(crate)`: the crate root record (the extern prelude, the `#[macro_use]` prelude, the
//!   `#[macro_export]` macros, the head) but the root file's content, which moves with every
//!   edit of that file and which [`Logged`] does not hand out ([`walked_head`]);
//! - `Variants(owner)`: an enum's variants, absent for no enum.
//! - From the items record (lane ND): `MemberOf(member)`, `VariantOf(variant)` and
//!   `Beginning(first 16 bytes)`, the three by-key searches whose "not found" was a read of the
//!   whole items section: each a row only where the search finds something, so "not found" is
//!   the cell absent, and the name added defines it. `Beginning` holds every key beginning so,
//!   in full: the found answer depends on no OTHER key beginning alike, so a second key added
//!   with the prefix moves it (before, the found path read only the key's own `Item` cell,
//!   which that edit does not move: rule 21).
//!
//! A private fn added moves exactly one cell, its own `Binding`, which a walk consulted only if
//! it looked that name up in that module.
//!
//! WHO WRITES AND WHO READS. [`table`] writes a crate's cell table from its names record:
//! sorted (key, digest) rows, [`TABLE_ROW`] bytes each, every cell a walk can name that holds
//! something. [`seen_in`] is the lookup a reader's part is decided by: one binary search of the
//! table, `None` for a cell absent. [`Logged`] is the walk's world wrapped at the one seam
//! (rule 78, names/seam.rs): every `scope`, `get`, `chain`, flag, root and variant read logs its
//! cell, so what a walk consulted is complete by construction (rule 21): a read the seam does
//! not see cannot reach the walk. The reader never logs a digest: its digest is decided from
//! the same table it would be checked against, so the two cannot disagree.

use core::cell::RefCell;

use blaze_hash::{FixedState, Set, SetOpts};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ids::{Domain, Hasher};
use blaze_own::{AVec, Alloc, Bump};
use blaze_surface::{Binding, CrateRootHead, CrateRootView, DefKind, Globs, ItemKind, MacroShapes, Namespace, RefusedWhy, ScopeView};

use crate::cell_key::{BindingKey, BindingKeys, CellKey, cell_key};
use crate::expansion_records::{Directory, ExpansionRecordsError};
use crate::facts::Name;
use crate::items_record::{ItemsRecord, ItemsRecordError, TextOf, TextSpan};
use crate::names::{RootRead, ScopeRead, VariantHit, World};
use crate::names_record::{NamesRecord, NamesRecordError, Verified};
use crate::resolve::{CrateNames, aligned};

/// The domain of a cell's key: the schema's name, so a change of the recipe is a new domain
/// and old reads miss.
pub const KEY_DOMAIN: &str = "blaze v2 crate names cell key 1";
/// The domain of a cell's digest. "1": a `Module` cell that held the module's textual macros
/// and marks, now the `Textual` cell's.
pub const DIGEST_DOMAIN: &str = "blaze v2 crate names cell digest 3";

static KEYS: Domain = Domain::new(KEY_DOMAIN);
static DIGESTS: Domain = Domain::new(DIGEST_DOMAIN);

/// One row of a cell table: the cell's key, then its digest.
pub const TABLE_ROW: usize = 16 + 32;

/// One cell of a crate's names: see the module doc.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum NameCell {
    Binding { module: ItemKey, name: CellKey },
    Module { module: ItemKey },
    /// The crate root of the crate whose keys begin with these 8 bytes.
    Root { krate: [u8; 8] },
    Variants { owner: ItemKey },
    /// THE ITEMS RECORD's rows of one key: its item row and its module row, each or both
    /// absent ([`ItemsRecord::item`], [`ItemsRecord::module`]).
    Item { key: ItemKey },
    /// An owner's member rows, in order (`RecordItems::members`); absent for no owner.
    Members { owner: ItemKey },
    /// A macro call's expansion row (its text's content id and length) and, when the crate's
    /// expansions directory is given (`Whole { section: Expansions }`'s bytes), the key of the
    /// expansion record its directory row names (expansion_records.rs: the text AND its origin
    /// column), so a reader of the text or its origins moves when either does; absent for none.
    Expansion { call: ItemKey },
    /// Which trait or impl owns the named member `member`, and its name
    /// ([`ItemsRecord::member_owner`]); absent for no named member of that key: the "not
    /// found" a reader asking of a free item reads.
    MemberOf { member: ItemKey },
    /// Which enum `variant` is of, and its index ([`ItemsRecord::variant_home`]); absent for
    /// no variant of that key.
    VariantOf { variant: ItemKey },
    /// Every key of the crate's item, module and variant rows whose first 16 bytes are
    /// `first`, each list's in full: what `key_beginning(first)` answers (one key, none, or two
    /// refused), so the cell moves when a second key with the prefix is added. Absent: no key
    /// begins so.
    Beginning { first: [u8; 16] },
    /// The crate's lang rows, in (name, key) order ([`ItemsRecord::lang_at`]): what a reader
    /// walking every crate's `#[lang]` items reads of this one. Absent: the crate declares
    /// none (most crates), so a reader of a workspace crate moves only when one is added there.
    Lang { krate: [u8; 8] },
    /// Whether the crate may write an inherent impl of a type it does not define
    /// ([`ItemsRecord::may_write_incoherent`], question 16): what a probe of a primitive's or
    /// a foreign type's inherent methods reads of each lang crate (blaze-ladder
    /// `incoherent_crates`). Absent: it may not (every crate but `core`, `alloc` and `std`), so
    /// an item added to a workspace crate never moves it.
    Incoherent { krate: [u8; 8] },
    /// The crate's `#![panic_runtime]` flag and any `#[rustc_std_internal_symbol]` function items
    /// (by key, name, and text): what `internal_symbol::record_search` reads of every crate in
    /// the scope. Absent when the crate has neither (every normal workspace and registry crate),
    /// so adding an ordinary private item to a dependency never wakes callers of internal symbols.
    InternalSymbols { krate: [u8; 8] },
    /// The crate's link facts: its `proc_macro` flag, its extern prelude crate roots, and every
    /// `Origin::ExternCrate` binding target across its modules. What `front2::linked` reads of
    /// each crate reached by the link closure, instead of recording both `Names` and `Items`
    /// whole for every crate in the scope.
    Links { krate: [u8; 8] },
    /// The crate's impl rows, in record order ([`ItemsRecord::impl_at`]): what an `ImplBucket`
    /// task and `NamesSource::each_impl_row` read of a crate's own items record. Absent when the
    /// crate declares no impls, so adding a private `fn` or other non-impl item never wakes the
    /// crate's `ImplBucket` task.
    ImplRows { krate: [u8; 8] },
    /// The crate's held file paths, in file-index order ([`ItemsRecord::file_surface`]'s `path`
    /// and `shown`, without each file's `surface` digest): what `NamesSource::file_paths`,
    /// `FileIs::NotHeld`, and `Heads` no-member / not-held checks read. Absent when the crate
    /// holds no files, so adding an item to an existing file never wakes readers of file paths.
    Files { krate: [u8; 8] },
    /// The crate's leading `#![blaze::...]` syntax extension flags
    /// (`blaze_file_items::detect_blaze_extensions` over the crate root file): what
    /// `NamesSource::blaze_extensions` reads in body/head/symbol lowering. Absent when no
    /// extension is enabled (normal Rust crates), so adding or editing items in any file never
    /// wakes unrelated bodies or heads.
    Extensions { krate: [u8; 8] },
    /// The crate's cfg atoms, in record order ([`ItemsRecord::cfg_atoms`]): what `Decision::Absent`
    /// and `NamesSource::cfg_atoms` depend on besides the item's own cfg predicates. Absent when
    /// the crate has no cfg atoms, so adding or editing items in any file never moves it.
    CfgAtoms { krate: [u8; 8] },
    /// A module's `macro_rules!` sequence and its `mod` lines' marks in it: what a textual
    /// macro lookup reads of the module, and no other walk; absent for a module the record does
    /// not hold a scope of.
    Textual { module: ItemKey },
    /// A section of the crate's record read WHOLE, by a question no cell answers (every impl,
    /// every item, the lang rows, the files): its digest is the section's bytes'.
    Whole { krate: [u8; 8], section: Section },
}

/// A section of a crate's `CrateScope` record a [`NameCell::Whole`] stands for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Section {
    Names = 1,
    Items = 2,
    Expansions = 3,
}

impl NameCell {
    /// The cell's key: the first 16 bytes of BLAKE3 in [`KEY_DOMAIN`] over a tag, the key it is
    /// about, and for a binding its name's [`cell_key`].
    pub fn key(self) -> [u8; 16] {
        let mut h = Hasher::of(&KEYS);
        match self {
            NameCell::Binding { module, name } => h.update(&[1]).id(&module.0).update(&name.0),
            NameCell::Module { module } => h.update(&[2]).id(&module.0),
            NameCell::Root { krate } => h.update(&[3]).update(&krate),
            NameCell::Variants { owner } => h.update(&[4]).id(&owner.0),
            NameCell::Item { key } => h.update(&[5]).id(&key.0),
            NameCell::Members { owner } => h.update(&[6]).id(&owner.0),
            NameCell::Expansion { call } => h.update(&[7]).id(&call.0),
            NameCell::Whole { krate, section } => h.update(&[8, section as u8]).update(&krate),
            NameCell::MemberOf { member } => h.update(&[9]).id(&member.0),
            NameCell::VariantOf { variant } => h.update(&[10]).id(&variant.0),
            NameCell::Beginning { first } => h.update(&[11]).update(&first),
            NameCell::Lang { krate } => h.update(&[12]).update(&krate),
            NameCell::Incoherent { krate } => h.update(&[13]).update(&krate),
            NameCell::Textual { module } => h.update(&[14]).id(&module.0),
            NameCell::InternalSymbols { krate } => h.update(&[15]).update(&krate),
            NameCell::Links { krate } => h.update(&[16]).update(&krate),
            NameCell::ImplRows { krate } => h.update(&[17]).update(&krate),
            NameCell::Files { krate } => h.update(&[18]).update(&krate),
            NameCell::Extensions { krate } => h.update(&[19]).update(&krate),
            NameCell::CfgAtoms { krate } => h.update(&[20]).update(&krate),
        };
        let mut key = [0; 16];
        key.copy_from_slice(&h.finish().0[..16]);
        key
    }

    /// The first 8 bytes of every key of the crate the cell is of: whose `CrateScope` holds it.
    pub fn crate_prefix(self) -> [u8; 8] {
        match self {
            NameCell::Binding { module, .. } | NameCell::Module { module } | NameCell::Textual { module } => module.crate_prefix(),
            NameCell::Root { krate }
            | NameCell::Whole { krate, .. }
            | NameCell::Lang { krate }
            | NameCell::Incoherent { krate }
            | NameCell::InternalSymbols { krate }
            | NameCell::Links { krate }
            | NameCell::ImplRows { krate }
            | NameCell::Files { krate }
            | NameCell::Extensions { krate }
            | NameCell::CfgAtoms { krate } => krate,
            NameCell::Variants { owner } | NameCell::Members { owner } => owner.crate_prefix(),
            NameCell::Item { key } => key.crate_prefix(),
            NameCell::Expansion { call } => call.crate_prefix(),
            NameCell::MemberOf { member } => member.crate_prefix(),
            NameCell::VariantOf { variant } => variant.crate_prefix(),
            NameCell::Beginning { first } => {
                let mut krate = [0u8; 8];
                krate.copy_from_slice(&first[..8]);
                krate
            }
        }
    }
}

/// How an items cell's digest reads a row's text: the crate's file or expansion, as the record
/// proves it ([`ItemsRecord::slice`]).
pub type ReadText<'t, 'a> = &'t dyn Fn(TextOf<'_>) -> Option<&'a str>;

/// A span's TEXT into `h`, never its place (rule 71): an item moved in its file, its text the
/// same, holds the same cell.
fn span_text<'a>(h: &mut Hasher, items: &ItemsRecord<'a>, span: Option<TextSpan>, read: ReadText<'_, 'a>) -> Result<(), ItemsRecordError> {
    match span {
        None => {
            h.update(&[0]);
        }
        Some(span) => {
            let text = items.slice(span, read)?;
            h.update(&[1]).std(&span.at).update(&(text.len() as u64).to_le_bytes()).update(text.as_bytes());
        }
    }
    Ok(())
}

/// What `Item(key)` holds: the item row of `key` (its meaning and its text; its place in the
/// file and its outline index left out) and its module row, `None` when both are absent. A
/// row that does not read is the record's error.
fn item_digest<'a>(items: &ItemsRecord<'a>, key: ItemKey, read: ReadText<'_, 'a>) -> Result<Option<ContentId>, ItemsRecordError> {
    let (item, module) = (items.item(key)?, items.module(key)?);
    if item.is_none() && module.is_none() {
        return Ok(None);
    }
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[5]);
    match item {
        None => {
            h.update(&[0]);
        }
        Some(row) => {
            h.update(&[1]).id(&row.key.0).std(&row.kind).id(&row.module.0).std(&row.name);
            h.update(&[u8::from(row.body_holds_items), u8::from(row.ctor), u8::from(row.impl_row.is_some())]);
            // The block ABI of a foreign fn or static (`ItemRow::foreign`): what
            // `NamesSource::extern_abi` answers through this cell alone, so an edit of the
            // block's string moves it.
            h.std(&row.foreign);
            // The lifetimes its generics declare (D155, `ItemIdentity::lifetimes`): what
            // `NamesSource::declared_lifetimes` answers through this cell alone (the text is
            // hashed here by its place only), so an edit of the generics moves it.
            h.std(&row.lifetimes);
            // Its `link_name`/`export_name` values written as macro calls, as the solve
            // expanded them (the record's value table, `NamesSource::attr_value`): what a body
            // naming the item's symbol reads through this cell alone, so a value that moves
            // while the item's text stays (ring's `prefix!()` reads `CARGO_PKG_VERSION`, a
            // macro defined elsewhere is edited) moves it (rule 21). An item with no such row
            // hashes nothing here: its digest is the one it had before the table existed.
            for (byte, attr) in crate::attr_values::ATTRS.iter().enumerate() {
                if let Some(answer) = items.attr_value(key, attr)? {
                    let (state, text) = match answer {
                        Ok(value) => (0u8, value),
                        Err(words) => (1u8, words),
                    };
                    h.update(&[6, byte as u8, state]).update(&(text.len() as u64).to_le_bytes()).update(text.as_bytes());
                }
            }
            if row.kind == ItemKind::Module {
                // A module's row holds its HEAD only (attributes, visibility, name): what it
                // holds is its own cells', so an item added inside an inline module moves that
                // module's cells and never the cell of its row in the parent.
                match row.text {
                    None => {
                        h.update(&[0]);
                    }
                    Some(span) => {
                        let head = module_head(items.slice(span, read)?);
                        h.update(&[2]).std(&span.at).update(&(head.len() as u64).to_le_bytes()).update(head.as_bytes());
                    }
                }
            } else {
                h.std(&row.text.map(|span| span.at));
            }
            span_text(&mut h, items, row.impl_header, read)?;
        }
    }
    h.std(&module);
    Ok(Some(h.finish()))
}

/// The head of a `mod` item's text: up to its body's `{` or its `;`, past its attributes and
/// comments (whose brackets, strings and braces are skipped as the lexer would).
pub fn module_head(text: &str) -> &str {
    let b = text.as_bytes();
    let mut i = 0;
    // Depth of `[` inside an attribute: a `{` or `;` there is the attribute's, not the head's.
    let mut depth = 0u32;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut nest = 1u32;
                i += 2;
                while i < b.len() && nest > 0 {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        nest += 1;
                        i += 1;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        nest -= 1;
                        i += 1;
                    }
                    i += 1;
                }
                continue;
            }
            // A raw string, `r"..."` or `r#"..."#`: ends at a quote and as many `#`.
            b'r' if matches!(b.get(i + 1), Some(b'"' | b'#')) && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) => {
                let mut j = i + 1;
                let mut hashes = 0;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) == Some(&b'"') {
                    j += 1;
                    while j < b.len() && !(b[j] == b'"' && b.len() - j > hashes && b[j + 1..=j + hashes].iter().all(|c| *c == b'#')) {
                        j += 1;
                    }
                    i = j + hashes;
                }
            }
            // A char literal (`'{'`, `'\''`); a lifetime has no closing quote and is skipped as
            // its letters are.
            b'\'' if b.get(i + 2) == Some(&b'\'') && b.get(i + 1) != Some(&b'\\') => i += 2,
            b'\'' if b.get(i + 1) == Some(&b'\\') => {
                i += 2;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'[' => depth += 1,
            b']' => depth = depth.saturating_sub(1),
            b'{' | b';' if depth == 0 => return &text[..i],
            _ => {}
        }
        i += 1;
    }
    text
}

/// What `Members(owner)` holds, `None` for no owner: each member's meaning and text, in order.
fn members_digest<'a>(items: &ItemsRecord<'a>, owner: ItemKey, read: ReadText<'_, 'a>) -> Result<Option<ContentId>, ItemsRecordError> {
    let Some(members) = items.members(owner)? else { return Ok(None) };
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[6]).update(&(members.len() as u64).to_le_bytes());
    for member in members.iter() {
        let member = member?;
        h.id(&member.key.0).std(&member.name).std(&member.kind).update(&[u8::from(member.body_holds_items)]).std(&member.written_by);
        if member.kind == ItemKind::Function {
            h.std(&member.text.map(|span| span.at));
        } else {
            span_text(&mut h, items, member.text, read)?;
        }
    }
    Ok(Some(h.finish()))
}

/// What `Expansion(call)` holds, `None` for no expansion row: the items row, then the record key
/// `directory`'s row for the call names (a 0 byte when no directory is given or it has no row
/// for the call, a 1 and the key when it has).
fn expansion_digest(items: &ItemsRecord<'_>, directory: Option<&Directory<'_>>, call: ItemKey) -> Result<Option<ContentId>, CellsError> {
    let Some(row) = items.expansion(call) else { return Ok(None) };
    let record = match directory {
        Some(directory) => directory.entry(call).map_err(CellsError::Expansions)?.map(|entry| entry.record),
        None => None,
    };
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[7]).std(&row);
    match record {
        Some(key) => h.update(&[1]).id(&key),
        None => h.update(&[0]),
    };
    Ok(Some(h.finish()))
}

/// The expansions directory among `wholes` (the `Expansions` section's bytes), opened; `None`
/// when it is not given.
fn directory_of<'w>(wholes: &[(Section, &'w [u8])]) -> Result<Option<Directory<'w>>, CellsError> {
    match wholes.iter().find(|(section, _)| *section == Section::Expansions) {
        Some((_, bytes)) => Directory::open(bytes).map(Some).map_err(CellsError::Expansions),
        None => Ok(None),
    }
}

/// What `Lang(krate)` holds: every lang row of the crate in order, each name and key; `None`
/// for a crate that declares no lang item.
fn lang_digest(items: &ItemsRecord<'_>) -> Result<Option<ContentId>, ItemsRecordError> {
    if items.langs() == 0 {
        return Ok(None);
    }
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[12]).update(&(items.langs() as u64).to_le_bytes());
    for i in 0..items.langs() {
        let (name, key) = items.lang_at(i)?.ok_or(ItemsRecordError::Short { what: "the lang rows", at: i })?;
        h.part(name.as_bytes()).id(&key.0);
    }
    Ok(Some(h.finish()))
}

/// What `Incoherent(krate)` holds: one fixed digest when the crate may write an inherent impl
/// of a type it does not define, `None` (absent) when it may not. The flag is the whole fact
/// `incoherent_crates` reads of the crate, so the cell moves exactly when it flips.
fn incoherent_digest(items: &ItemsRecord<'_>) -> Option<ContentId> {
    items.may_write_incoherent().then(|| Hasher::of(&DIGESTS).update(&[13]).finish())
}

/// What `InternalSymbols(krate)` holds: `None` (absent) when the crate's root file carries no
/// `panic_runtime` and no function item carries `rustc_std_internal_symbol`; otherwise the digest
/// of that flag and every such function's key, name, and text in key order.
fn internal_symbols_digest<'a>(items: &ItemsRecord<'a>, read: ReadText<'_, 'a>) -> Result<Option<ContentId>, ItemsRecordError> {
    let has_panic_runtime = match items.file_surface(0)? {
        Some(file) => read(TextOf::File { index: 0, path: file.path }).is_some_and(|text| text.contains("panic_runtime")),
        None => false,
    };
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[15, u8::from(has_panic_runtime)]);
    let mut count = 0u64;
    for i in 0..items.items() {
        let Some(row) = items.item_at(i)? else { continue };
        if row.kind != ItemKind::Function {
            continue;
        }
        let Some(span) = row.text else { continue };
        let text = items.slice(span, read)?;
        if text.contains("rustc_std_internal_symbol") {
            h.id(&row.key.0).std(&row.name).part(text.as_bytes());
            count = count.saturating_add(1);
        }
    }
    if !has_panic_runtime && count == 0 {
        return Ok(None);
    }
    h.update(&count.to_le_bytes());
    Ok(Some(h.finish()))
}

/// What `Links(krate)` holds: the crate's `proc_macro` flag, its extern prelude crate roots,
/// and every `Origin::ExternCrate` binding target across its modules (in deterministic module
/// and binding order). Covers everything `front2::source::Records::linked_logged` reads.
fn links_digest(record: &NamesRecord<'_>, items: &ItemsRecord<'_>) -> Result<ContentId, ItemsRecordError> {
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[16, u8::from(items.proc_macro())]);
    let root = record.crate_root();
    let extern_count = u64::try_from(root.externs().len()).unwrap_or(u64::MAX);
    h.update(&extern_count.to_le_bytes());
    for (_, dep_root) in root.externs() {
        h.id(&dep_root.0);
    }
    let mut count = 0u64;
    for m in 0..items.modules() {
        let Some(row) = items.module_at(m)? else { break };
        let Some(scope) = record.scope(row.key) else { continue };
        for entry in scope.bindings() {
            if matches!(entry.binding.origin, blaze_surface::Origin::ExternCrate) {
                h.id(&entry.binding.def.key.0);
                count = count.saturating_add(1);
            }
        }
    }
    h.update(&count.to_le_bytes());
    Ok(h.finish())
}

/// What `ImplRows(krate)` holds: every impl row of the crate in record order (every field of
/// [`crate::items_record::ImplRow`]); `None` for a crate that declares no impls.
fn impl_rows_digest(items: &ItemsRecord<'_>) -> Result<Option<ContentId>, ItemsRecordError> {
    if items.impls() == 0 {
        return Ok(None);
    }
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[17]).update(&u64::try_from(items.impls()).unwrap_or(u64::MAX).to_le_bytes());
    for i in 0..items.impls() {
        let crate::items_record::ImplRow {
            key,
            module,
            trait_path,
            has_trait,
            bare,
            flags,
            head,
            self_path,
            header_unread,
            trait_unread,
            refused_call,
            trait_root,
            self_root,
        } = items.impl_at(i)?.ok_or(ItemsRecordError::Short { what: "the impl rows", at: i })?;
        h.id(&key.0)
            .id(&module.0)
            .std(&trait_path)
            .update(&[u8::from(has_trait), u8::from(bare), u8::from(header_unread), u8::from(trait_unread), u8::from(refused_call)])
            .std(&flags)
            .std(&head)
            .std(&self_path);
        match trait_root {
            Some(k) => h.update(&[1]).id(&k.0),
            None => h.update(&[0]),
        };
        match self_root {
            Some(k) => h.update(&[1]).id(&k.0),
            None => h.update(&[0]),
        };
    }
    Ok(Some(h.finish()))
}

/// What `Files(krate)` holds: every file row's package path and shown path, in file-index order
/// (leaving out each file's `surface` digest, which moves on every surface edit); `None` when the
/// crate holds no files.
fn files_digest(items: &ItemsRecord<'_>) -> Result<Option<ContentId>, ItemsRecordError> {
    if items.files() == 0 {
        return Ok(None);
    }
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[18]).update(&u64::try_from(items.files()).unwrap_or(u64::MAX).to_le_bytes());
    for i in 0..items.files() {
        let row = items
            .file_surface(u32::try_from(i).unwrap_or(u32::MAX))?
            .ok_or(ItemsRecordError::Short { what: "the file rows", at: i })?;
        h.part(row.path.as_str().as_bytes()).part(row.shown.as_bytes());
    }
    Ok(Some(h.finish()))
}

/// What `Extensions(krate)` holds: the bitmask of `#![blaze::...]` syntax extensions enabled at
/// the top of the crate's root file (`blaze_file_items::detect_blaze_extensions`); `None` (absent)
/// when none are enabled.
fn extensions_digest<'a>(items: &ItemsRecord<'a>, read: ReadText<'_, 'a>) -> Result<Option<ContentId>, ItemsRecordError> {
    let bits = match items.file_surface(0)? {
        Some(file) => read(TextOf::File { index: 0, path: file.path })
            .map(blaze_file_items::detect_blaze_extensions)
            .map_or(0, |ext| ext.bits()),
        None => 0,
    };
    if bits == 0 {
        return Ok(None);
    }
    Ok(Some(Hasher::of(&DIGESTS).update(&[19, bits]).finish()))
}

/// What `CfgAtoms(krate)` holds: every `(name, value)` cfg atom of the crate in record order
/// ([`ItemsRecord::cfg_atoms`]); `None` (absent) when empty.
fn cfg_atoms_digest(items: &ItemsRecord<'_>) -> Result<Option<ContentId>, ItemsRecordError> {
    let atoms = items.cfg_atoms()?;
    if atoms.is_empty() {
        return Ok(None);
    }
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[20]).update(&u64::try_from(atoms.len()).unwrap_or(u64::MAX).to_le_bytes());
    for &(name, value) in atoms {
        h.part(name.as_bytes());
        match value {
            Some(v) => h.update(&[1]).part(v.as_bytes()),
            None => h.update(&[0]),
        };
    }
    Ok(Some(h.finish()))
}

/// The first 16 bytes of a key: what [`NameCell::Beginning`] is keyed by (`key_beginning`'s
/// question).
pub fn first_16(key: ItemKey) -> [u8; 16] {
    let mut first = [0u8; 16];
    first.copy_from_slice(&key.0.0[..16]);
    first
}

/// What `MemberOf(member)` holds, `None` for no named member of that key: the answer of
/// [`ItemsRecord::member_owner`] (the owner's key and the member's name), whole.
fn member_of_digest(items: &ItemsRecord<'_>, member: ItemKey) -> Result<Option<ContentId>, ItemsRecordError> {
    Ok(items.member_owner(member)?.map(|(owner, name)| Hasher::of(&DIGESTS).update(&[9]).id(&owner.0).std(name).finish()))
}

/// What `VariantOf(variant)` holds for the variant table's rows of one key: `None` for none,
/// the enum and the index for one ([`ItemsRecord::variant_home`]'s answer), and a "refused"
/// digest for two or more (`variant_home` refuses the key by name; the table is still
/// written, and the key's readers move when it stops being refused).
fn variant_of_digest(rows: &[(ItemKey, u32)]) -> Option<ContentId> {
    match rows {
        [] => None,
        [(owner, index)] => Some(Hasher::of(&DIGESTS).update(&[10, 1]).id(&owner.0).update(&index.to_le_bytes()).finish()),
        _ => Some(Hasher::of(&DIGESTS).update(&[10, 0xff]).finish()),
    }
}

/// The variant table's rows of `variant` (at most two: a second is the refusal), by one binary
/// search of the table as [`ItemsRecord::variant_home`] does it.
fn variant_rows_of(items: &ItemsRecord<'_>, variant: ItemKey) -> Result<([(ItemKey, u32); 2], usize), ItemsRecordError> {
    let count = items.variant_rows();
    let row = |i: usize| -> Result<(ItemKey, ItemKey, u32), ItemsRecordError> { items.variant_at(i)?.ok_or(ItemsRecordError::Short { what: "the variant table", at: i }) };
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if row(mid)?.0 < variant {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let mut out = [(variant, 0u32); 2];
    let mut n = 0;
    while n < 2 && lo + n < count {
        let (key, owner, index) = row(lo + n)?;
        if key != variant {
            break;
        }
        out[n] = (owner, index);
        n += 1;
    }
    Ok((out, n))
}

/// What `Beginning(first)` holds, `None` when no key of the three lists begins with `first`:
/// each list's run of keys beginning so, in full and in order, with its length. `lists` are the
/// crate's item, module and variant rows' keys, each sorted (as the record holds them, and
/// as `key_beginning` searches them: one key, none, or two refused, a function of these runs).
pub fn beginning_digest(first: [u8; 16], lists: [&[ItemKey]; 3]) -> Option<ContentId> {
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[11]);
    let mut any = false;
    for list in lists {
        let from = list.partition_point(|key| first_16(*key) < first);
        let run = &list[from..from + list[from..].iter().take_while(|key| first_16(**key) == first).count()];
        any |= !run.is_empty();
        h.update(&(run.len() as u64).to_le_bytes());
        for key in run {
            h.id(&key.0);
        }
    }
    any.then(|| h.finish())
}

/// The keys of the crate's item rows, module rows and variant rows, each in the record's order,
/// refused unless sorted (every search of them, `key_beginning`'s among them, assumes it).
fn beginning_lists<'b>(items: &ItemsRecord<'_>, alloc: Alloc<'b>) -> Result<[AVec<'b, ItemKey>; 3], CellsError> {
    let mut lists = [AVec::with_capacity_in(items.items(), alloc), AVec::with_capacity_in(items.modules(), alloc), AVec::with_capacity_in(items.variant_rows(), alloc)];
    for i in 0..items.items() {
        if let Some(row) = items.item_at(i).map_err(CellsError::Items)? {
            lists[0].push(row.key);
        }
    }
    for i in 0..items.modules() {
        if let Some(row) = items.module_at(i).map_err(CellsError::Items)? {
            lists[1].push(row.key);
        }
    }
    for i in 0..items.variant_rows() {
        if let Some(row) = items.variant_at(i).map_err(CellsError::Items)? {
            lists[2].push(row.0);
        }
    }
    for (list, what) in lists.iter().zip(["the item rows", "the module rows", "the variant rows"]) {
        if list.windows(2).any(|pair| pair[0] > pair[1]) {
            return Err(CellsError::Unsorted { what });
        }
    }
    Ok(lists)
}

/// What `Whole(section)` holds: the digest of the section's bytes as the record holds them.
pub fn whole_digest(section: Section, bytes: &[u8]) -> ContentId {
    Hasher::of(&DIGESTS).update(&[8, section as u8]).part(bytes).finish()
}

fn binding_digest(binding: Binding) -> ContentId {
    Hasher::of(&DIGESTS).update(&[1]).std(&binding).finish()
}

/// The head a walk through [`Logged`] is handed: the root file's content left out. It moves with
/// every edit of the root file, and nothing a walk answers depends on it, so the `Root` cell
/// does not hold it and the seam does not hand it out (what a reader cannot see, it cannot
/// read stale).
pub fn walked_head(head: CrateRootHead) -> CrateRootHead {
    CrateRootHead { root_file: ContentId([0; 32]), ..head }
}

fn root_digest(record: &NamesRecord<'_>) -> ContentId {
    let root = record.crate_root();
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[3]).std(&walked_head(CrateRootView::head(&root)));
    h.update(&(root.externs().len() as u64).to_le_bytes());
    for extern_crate in root.externs() {
        h.std(&extern_crate);
    }
    h.update(&(root.deps().len() as u64).to_le_bytes());
    for dep in root.deps() {
        h.std(&dep);
    }
    // Inherent, named: [`RootRead::macro_use`] takes an index and would be picked first.
    h.update(&(CrateRootView::macro_use(&root).len() as u64).to_le_bytes());
    for key in CrateRootView::macro_use(&root) {
        h.id(&key.0);
    }
    h.update(&(root.macro_exports().len() as u64).to_le_bytes());
    for export in root.macro_exports() {
        h.std(&export);
    }
    names(&mut h, root.tools());
    names(&mut h, root.builtin_attrs());
    names(&mut h, root.features());
    h.finish()
}

fn names<'x>(h: &mut Hasher, list: impl ExactSizeIterator<Item = &'x str>) {
    h.update(&(list.len() as u64).to_le_bytes());
    for name in list {
        h.std(name);
    }
}

fn variants_digest(record: &NamesRecord<'_>, owner: ItemKey) -> Option<ContentId> {
    let variants = record.variants(owner)?;
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[4]);
    for variant in variants.iter() {
        h.std(&variant);
    }
    Some(h.finish())
}

/// What `Module(module)` holds, `None` for a module the record has no row of.
fn module_digest(record: &NamesRecord<'_>, module: ItemKey) -> Option<ContentId> {
    let chain = record.chain(module);
    let scope = record.scope(module);
    if chain.is_none() && scope.is_none() {
        return None;
    }
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[2]);
    match chain {
        Some(chain) => {
            h.update(&(chain.len() as u64).to_le_bytes());
            for key in chain {
                h.id(&key.0);
            }
        }
        None => {
            h.update(&[0xff]);
        }
    }
    h.update(&[u8::from(record.no_implicit_prelude(module)), u8::from(record.anonymous(module))]);
    match scope {
        None => {
            h.update(&[0]);
        }
        Some(view) => {
            h.update(&[1]);
            // The inherent reads, named: [`ScopeRead`]'s by-value methods of the same names
            // would be picked first.
            for edge in ScopeView::globs(&view) {
                h.std(&edge);
            }
            // The textual macros and the marks are the `Textual` cell's ([`textual_cell_digest`]).
            h.update(&[0xfe]).std(&ScopeView::refused(&view));
            h.update(&[0xfc]);
            for anon in view.traits() {
                h.std(&anon);
            }
            h.update(&[0xfb]);
            for entry in view.bindings() {
                if entry.ns == Namespace::Types && entry.binding.def.kind == DefKind::Trait {
                    h.id(&entry.binding.def.key.0);
                }
            }
        }
    }
    Some(h.finish())
}

/// What `Textual(module)` holds, `None` for a module the record holds no scope of: its
/// `macro_rules!` sequence in order and each `mod` line's mark in it, what
/// [`ScopeRead::textual`] and [`ScopeRead::marked`] answer from.
fn textual_cell_digest(record: &NamesRecord<'_>, module: ItemKey) -> Option<ContentId> {
    let view: ScopeView<'_> = record.scope(module)?;
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[14]);
    // The inherent reads, named, as in [`module_digest`].
    let defs = ScopeView::textual(&view);
    h.update(&(defs.len() as u64).to_le_bytes());
    for def in defs {
        h.std(&def);
    }
    let marks = view.marks();
    h.update(&(marks.len() as u64).to_le_bytes());
    for mark in marks {
        h.std(&mark);
    }
    Some(h.finish())
}

/// Why a cell table was not written or a lookup refused.
#[derive(Debug)]
pub enum CellsError {
    /// Two cells of one crate under one key (rule 21): a collision of 128 bits, refused.
    TwoUnderOneKey { key: [u8; 16] },
    /// A table's bytes are not whole rows in increasing key order.
    NotATable { at: usize },
    /// A row of the items record did not read.
    Items(ItemsRecordError),
    /// An items cell asked of a crate whose items record was not given.
    NoItemsRecord,
    /// The names record just written does not check.
    Names(NamesRecordError),
    /// A list of the items record every search of assumes sorted is not (a writer's bug).
    Unsorted { what: &'static str },
    /// A crate's cell table past [`MAX_ROWS`] (rule 18).
    TooManyRows { rows: usize },
    /// The expansions directory given as a whole section does not open, or its row of a call
    /// does not read (expansion_records.rs).
    Expansions(ExpansionRecordsError),
}

/// The most rows one crate's cell table holds: four of each item, module, variant and member
/// row of a crate several times `core`'s size (rule 18: every table has a cap).
pub const MAX_ROWS: usize = 1 << 24;

/// The cell table of a crate just solved, from the two records written of it: what the crate's
/// `CrateScope` record files beside them (blaze-cli names.rs). Item texts are read from the
/// solve's own files and expansions; each of `wholes` is a [`NameCell::Whole`].
pub fn crate_table<'a>(names: &CrateNames<'a>, names_record: &[u8], items_record: &[u8], wholes: &[(Section, &[u8])], bump: Bump<'a>) -> Result<AVec<'a, u8>, CellsError> {
    let alloc = bump.alloc();
    let record = NamesRecord::open(Verified::check(aligned(names_record, bump)).map_err(CellsError::Names)?);
    let mut items = ItemsRecord::open(aligned(items_record, bump), alloc).map_err(CellsError::Items)?;
    items.use_solved_texts(names, bump);
    let read = |of: TextOf<'_>| -> Option<&'a str> {
        match of {
            TextOf::File { index, .. } => names.files.get(index as usize).map(|file| file.text),
            TextOf::Expansion { call } => names.expansions.get(&call).and_then(|expansion| expansion.made).map(|made| made.text),
        }
    };
    table(&record, Some((&items, &read)), wholes, alloc)
}

/// The cell table of `record`'s crate: every cell with something in it, sorted by key, as
/// [`TABLE_ROW`]-byte rows. With `items`, its rows' cells too (every item and module key,
/// every owner's members, every expansion); each of `wholes` is a [`NameCell::Whole`] over
/// those bytes.
pub fn table<'a, 'b>(record: &NamesRecord<'_>, items: Option<(&ItemsRecord<'a>, ReadText<'_, 'a>)>, wholes: &[(Section, &[u8])], alloc: Alloc<'b>) -> Result<AVec<'b, u8>, CellsError> {
    let mut rows: AVec<'b, ([u8; 16], ContentId)> = AVec::new_in(alloc);
    let krate = record.key().prefix();
    for (section, bytes) in wholes {
        rows.push((NameCell::Whole { krate, section: *section }.key(), whole_digest(*section, bytes)));
    }
    if let Some((items, read)) = items {
        let mut keys: AVec<'_, ItemKey> = AVec::with_capacity_in(items.items() + items.modules(), alloc);
        for i in 0..items.items() {
            if let Some(row) = items.item_at(i).map_err(CellsError::Items)? {
                keys.push(row.key);
                if let Some(digest) = members_digest(items, row.key, read).map_err(CellsError::Items)? {
                    rows.push((NameCell::Members { owner: row.key }.key(), digest));
                }
            }
        }
        for i in 0..items.modules() {
            if let Some(row) = items.module_at(i).map_err(CellsError::Items)? {
                keys.push(row.key);
            }
        }
        keys.sort_unstable();
        keys.dedup();
        for key in keys.iter() {
            if let Some(digest) = item_digest(items, *key, read).map_err(CellsError::Items)? {
                rows.push((NameCell::Item { key: *key }.key(), digest));
            }
        }
        let directory = directory_of(wholes)?;
        for i in 0..items.expansions() {
            if let Some(row) = items.expansion_at(i) {
                if let Some(digest) = expansion_digest(items, directory.as_ref(), row.call)? {
                    rows.push((NameCell::Expansion { call: row.call }.key(), digest));
                }
            }
        }
        // One `MemberOf` a named member (the owners section is sorted by member key; a key
        // under two owners is one cell, its answer `member_owner`'s).
        let mut last: Option<ItemKey> = None;
        for i in 0..items.owned_members() {
            let Some(member) = items.owned_key_at(i).map_err(CellsError::Items)? else { break };
            if last == Some(member) {
                continue;
            }
            last = Some(member);
            if let Some(digest) = member_of_digest(items, member).map_err(CellsError::Items)? {
                rows.push((NameCell::MemberOf { member }.key(), digest));
            }
        }
        // One `VariantOf` a variant key, over its run of the (sorted) variant table.
        let mut i = 0;
        while i < items.variant_rows() {
            let Some((variant, ..)) = items.variant_at(i).map_err(CellsError::Items)? else { break };
            let (run, n) = variant_rows_of(items, variant).map_err(CellsError::Items)?;
            if let Some(digest) = variant_of_digest(&run[..n]) {
                rows.push((NameCell::VariantOf { variant }.key(), digest));
            }
            // Past the whole run (a refused key may have more than the two rows read).
            i += 1;
            while i < items.variant_rows() && items.variant_at(i).map_err(CellsError::Items)?.is_some_and(|row| row.0 == variant) {
                i += 1;
            }
        }
        // One `Beginning` a 16-byte prefix any item, module or variant key has.
        let lists = beginning_lists(items, alloc)?;
        let mut firsts: AVec<'_, [u8; 16]> = AVec::with_capacity_in(lists.iter().map(|list| list.len()).sum(), alloc);
        for list in lists.iter() {
            for key in list.iter() {
                firsts.push(first_16(*key));
            }
        }
        firsts.sort_unstable();
        firsts.dedup();
        for first in firsts.iter() {
            if let Some(digest) = beginning_digest(*first, [&lists[0][..], &lists[1][..], &lists[2][..]]) {
                rows.push((NameCell::Beginning { first: *first }.key(), digest));
            }
        }
        if let Some(digest) = lang_digest(items).map_err(CellsError::Items)? {
            rows.push((NameCell::Lang { krate }.key(), digest));
        }
        if let Some(digest) = incoherent_digest(items) {
            rows.push((NameCell::Incoherent { krate }.key(), digest));
        }
        if let Some(digest) = internal_symbols_digest(items, read).map_err(CellsError::Items)? {
            rows.push((NameCell::InternalSymbols { krate }.key(), digest));
        }
        rows.push((NameCell::Links { krate }.key(), links_digest(record, items).map_err(CellsError::Items)?));
        if let Some(digest) = impl_rows_digest(items).map_err(CellsError::Items)? {
            rows.push((NameCell::ImplRows { krate }.key(), digest));
        }
        if let Some(digest) = files_digest(items).map_err(CellsError::Items)? {
            rows.push((NameCell::Files { krate }.key(), digest));
        }
        if let Some(digest) = extensions_digest(items, read).map_err(CellsError::Items)? {
            rows.push((NameCell::Extensions { krate }.key(), digest));
        }
        if let Some(digest) = cfg_atoms_digest(items).map_err(CellsError::Items)? {
            rows.push((NameCell::CfgAtoms { krate }.key(), digest));
        }
    }
    for module in record.modules() {
        if let Some(digest) = module_digest(record, module) {
            rows.push((NameCell::Module { module }.key(), digest));
        }
        if let Some(digest) = textual_cell_digest(record, module) {
            rows.push((NameCell::Textual { module }.key(), digest));
        }
        if let Some(view) = record.scope(module) {
            for entry in view.bindings() {
                let cell = NameCell::Binding { module, name: cell_key(entry.ns, entry.name) };
                rows.push((cell.key(), binding_digest(entry.binding)));
            }
        }
    }
    for owner in record.enums() {
        if let Some(digest) = variants_digest(record, owner) {
            rows.push((NameCell::Variants { owner }.key(), digest));
        }
    }
    rows.push((NameCell::Root { krate: record.key().prefix() }.key(), root_digest(record)));
    if rows.len() > MAX_ROWS {
        return Err(CellsError::TooManyRows { rows: rows.len() });
    }
    rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut bytes = AVec::with_capacity_in(rows.len() * TABLE_ROW, alloc);
    for (i, (key, digest)) in rows.iter().enumerate() {
        if i > 0 && rows[i - 1].0 == *key {
            return Err(CellsError::TwoUnderOneKey { key: *key });
        }
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(&digest.0);
    }
    Ok(bytes)
}

/// What the table holds at `key`: its digest, or `None` for a cell absent. One binary search.
pub fn seen_in(table: &[u8], key: [u8; 16]) -> Result<Option<ContentId>, CellsError> {
    if table.len() % TABLE_ROW != 0 {
        return Err(CellsError::NotATable { at: table.len() });
    }
    let (mut lo, mut hi) = (0usize, table.len() / TABLE_ROW);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let row = &table[mid * TABLE_ROW..(mid + 1) * TABLE_ROW];
        match row[..16].cmp(&key[..]) {
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
            core::cmp::Ordering::Equal => {
                let mut digest = [0; 32];
                digest.copy_from_slice(&row[16..]);
                return Ok(Some(ContentId(digest)));
            }
        }
    }
    Ok(None)
}

/// Where a walk through [`Logged`] puts each cell it consults: the names half of a task's
/// recorder (engine.md E2: one per-task recorder of every read, names, heads and impls, over
/// read-only tables). [`Log`] is the plain one. A recorder over shared tables is per task and
/// the tables hold no per-task cache, so what it is handed is a function of the task's questions
/// alone, never of which task asked first (reads.rs's "what is not listed").
pub trait CellSink {
    fn put(&self, cell: NameCell);

    /// The read of `module`'s own binding of `(ns, name)`, a hit or a miss (a miss is a read
    /// too: the name added later moves it): what [`LoggedScope::get`] logs, as the module, the
    /// namespace and the name, so a lookup derives no key itself. The cell is
    /// `NameCell::Binding { module, name: cell_key(ns, name) }`, byte-equal however it is
    /// derived (rule 21). This default derives the key now, per read; a sink that keeps each
    /// read once overrides it to derive the key once per DISTINCT read ([`BindingKeys`], as
    /// [`Log`] does).
    fn put_binding(&self, module: ItemKey, ns: Namespace, name: &str) {
        self.put(NameCell::Binding { module, name: cell_key(ns, name) });
    }
}

impl CellSink for Log<'_> {
    fn put(&self, cell: NameCell) {
        Log::put(self, cell)
    }

    fn put_binding(&self, module: ItemKey, ns: Namespace, name: &str) {
        let BindingKey { key, fresh } = self.keys.borrow_mut().key(module, ns, name);
        if fresh {
            Log::put(self, NameCell::Binding { module, name: key });
        }
    }
}

/// The cells a walk consulted, each once, with the cell it is; keyed ([`NameCell::key`]) once
/// per distinct cell, when they are listed ([`Log::cells`]), never per read.
pub struct Log<'a> {
    cells: RefCell<Set<NameCell, FixedState, Alloc<'a>>>,
    /// The binding reads' cell keys, each derived once ([`CellSink::put_binding`]).
    keys: RefCell<BindingKeys<'a>>,
}

impl<'a> Log<'a> {
    pub fn new(alloc: Alloc<'a>) -> Log<'a> {
        Log {
            cells: RefCell::new(Set::new(SetOpts { hasher: FixedState, alloc: alloc, capacity: None })),
            keys: RefCell::new(BindingKeys::new_in(alloc)),
        }
    }

    /// Logs `cell`: what a reader outside the walk (an items record question) consulted.
    pub fn put(&self, cell: NameCell) {
        self.cells.borrow_mut().insert(cell);
    }

    /// Every cell logged so far, by key order, into `alloc`: each distinct cell's key derived
    /// here, once.
    pub fn cells<'b>(&self, alloc: Alloc<'b>) -> AVec<'b, ([u8; 16], NameCell)> {
        let mut all: AVec<'b, ([u8; 16], NameCell)> = AVec::new_in(alloc);
        for cell in self.cells.borrow().iter() {
            all.push((cell.key(), *cell));
        }
        all.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        all
    }

    /// How many binding cell keys ([`cell_key`]) this log derived: one per distinct binding
    /// read, whatever the number of lookups (the floor's counter).
    pub fn binding_keys_derived(&self) -> u64 {
        self.keys.borrow().derived()
    }

    /// How many binding reads were logged, repeats too.
    pub fn binding_reads(&self) -> u64 {
        self.keys.borrow().asked()
    }

    pub fn len(&self) -> usize {
        self.cells.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A walk's world whose every read is put in `log`: the seam a reader by cell walks through.
pub struct Logged<'l, W> {
    pub world: &'l W,
    pub log: &'l dyn CellSink,
}

/// A module's scope as [`Logged`] hands it out: each `get` logs its binding's cell, each
/// `textual` and `marked` its `Textual` cell (the scope's other reads are its `Module` cell,
/// logged when it was handed out).
#[derive(Clone, Copy)]
pub struct LoggedScope<'w, S> {
    scope: S,
    module: ItemKey,
    log: &'w dyn CellSink,
}

impl<'w, N: Name, S: ScopeRead<'w, N>> ScopeRead<'w, N> for LoggedScope<'w, S> {
    fn get(self, ns: Namespace, name: N) -> Option<Binding> {
        // Logged as the read itself: no key is derived per lookup ([`CellSink::put_binding`]).
        self.log.put_binding(self.module, ns, name.as_ref());
        self.scope.get(ns, name)
    }

    fn globs(self) -> Globs<'w> {
        self.scope.globs()
    }

    fn refused(self) -> Option<(ItemKey, RefusedWhy)> {
        self.scope.refused()
    }

    fn textual(self, child: Option<ItemKey>, name: N) -> Option<(ItemKey, MacroShapes)> {
        self.log.put(NameCell::Textual { module: self.module });
        self.scope.textual(child, name)
    }

    /// The marks are the scope's `Textual` cell's ([`textual_cell_digest`]).
    fn marked(self, child: ItemKey) -> bool {
        self.log.put(NameCell::Textual { module: self.module });
        self.scope.marked(child)
    }

    fn each_trait(self, each: &mut dyn FnMut(ItemKey)) {
        self.scope.each_trait(each)
    }
}

/// A crate root as [`Logged`] hands it out: its `Root` cell was logged when it was, and its
/// head is [`walked_head`].
#[derive(Clone, Copy)]
pub struct LoggedRoot<R>(R);

impl<N, R: RootRead<N>> RootRead<N> for LoggedRoot<R> {
    fn head(self) -> CrateRootHead {
        walked_head(self.0.head())
    }

    fn extern_crate(self, name: N) -> Option<ItemKey> {
        self.0.extern_crate(name)
    }

    fn macro_use(self, i: usize) -> Option<ItemKey> {
        self.0.macro_use(i)
    }

    fn macro_export(self, name: N) -> Option<(ItemKey, MacroShapes)> {
        self.0.macro_export(name)
    }
}

impl<'l, N: Name, W: World<N>> World<N> for Logged<'l, W> {
    type Scope<'w>
        = LoggedScope<'w, W::Scope<'w>>
    where
        Self: 'w;
    type Root<'w>
        = LoggedRoot<W::Root<'w>>
    where
        Self: 'w;

    fn scope(&self, module: ItemKey) -> Option<Self::Scope<'_>> {
        self.log.put(NameCell::Module { module });
        let scope = self.world.scope(module)?;
        Some(LoggedScope { scope, module, log: self.log })
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.log.put(NameCell::Module { module });
        self.world.chain(module)
    }

    fn root_of(&self, key: ItemKey) -> Option<Self::Root<'_>> {
        self.log.put(NameCell::Root { krate: key.crate_prefix() });
        self.world.root_of(key).map(LoggedRoot)
    }

    fn root(&self, krate: CrateItemKey) -> Option<Self::Root<'_>> {
        self.log.put(NameCell::Root { krate: krate.prefix() });
        self.world.root(krate).map(LoggedRoot)
    }

    fn variant(&self, owner: ItemKey, name: N) -> Option<Option<VariantHit>> {
        self.log.put(NameCell::Variants { owner });
        self.world.variant(owner, name)
    }

    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        self.log.put(NameCell::Module { module });
        self.world.no_implicit_prelude(module)
    }

    fn anonymous(&self, module: ItemKey) -> bool {
        self.log.put(NameCell::Module { module });
        self.world.anonymous(module)
    }
}

/// The digest `key` holds in a crate's names, read from the record itself rather than its
/// table: the second home a test holds the table to ([`table`] over the same record).
pub fn seen_by_record<'a>(record: &NamesRecord<'_>, items: Option<(&ItemsRecord<'a>, ReadText<'_, 'a>)>, wholes: &[(Section, &[u8])], cell: NameCell) -> Result<Option<ContentId>, CellsError> {
    let items_of = || items.map(|(items, _)| items).ok_or(CellsError::NoItemsRecord);
    let read_of = || items.map(|(_, read)| read).ok_or(CellsError::NoItemsRecord);
    Ok(match cell {
        NameCell::Binding { module, name } => {
            let Some(view) = record.scope(module) else { return Ok(None) };
            let view: ScopeView<'_> = view;
            view.bindings().find(|entry| cell_key(entry.ns, entry.name) == name).map(|entry| binding_digest(entry.binding))
        }
        NameCell::Module { module } => module_digest(record, module),
        NameCell::Textual { module } => textual_cell_digest(record, module),
        NameCell::Root { krate } => (krate == record.key().prefix()).then(|| root_digest(record)),
        NameCell::Variants { owner } => variants_digest(record, owner),
        NameCell::Item { key } => item_digest(items_of()?, key, read_of()?).map_err(CellsError::Items)?,
        NameCell::Members { owner } => members_digest(items_of()?, owner, read_of()?).map_err(CellsError::Items)?,
        NameCell::Expansion { call } => expansion_digest(items_of()?, directory_of(wholes)?.as_ref(), call)?,
        NameCell::Whole { section, .. } => wholes.iter().find(|(s, _)| *s == section).map(|(s, bytes)| whole_digest(*s, bytes)),
        NameCell::MemberOf { member } => member_of_digest(items_of()?, member).map_err(CellsError::Items)?,
        NameCell::VariantOf { variant } => {
            let items = items_of()?;
            let (run, n) = variant_rows_of(items, variant).map_err(CellsError::Items)?;
            // One row: the record's own search answers it (the second home of the cell).
            if n == 1 {
                let home = items.variant_home(variant).map_err(CellsError::Items)?;
                variant_of_digest(home.as_slice())
            } else {
                variant_of_digest(&run[..n])
            }
        }
        NameCell::Beginning { first } => beginning_by_record(items_of()?, first)?,
        NameCell::Lang { krate } if krate == record.key().prefix() => lang_digest(items_of()?).map_err(CellsError::Items)?,
        NameCell::Lang { .. } => None,
        NameCell::Incoherent { krate } if krate == record.key().prefix() => incoherent_digest(items_of()?),
        NameCell::Incoherent { .. } => None,
        NameCell::InternalSymbols { krate } if krate == record.key().prefix() => internal_symbols_digest(items_of()?, read_of()?).map_err(CellsError::Items)?,
        NameCell::InternalSymbols { .. } => None,
        NameCell::Links { krate } if krate == record.key().prefix() => Some(links_digest(record, items_of()?).map_err(CellsError::Items)?),
        NameCell::Links { .. } => None,
        NameCell::ImplRows { krate } if krate == record.key().prefix() => impl_rows_digest(items_of()?).map_err(CellsError::Items)?,
        NameCell::ImplRows { .. } => None,
        NameCell::Files { krate } if krate == record.key().prefix() => files_digest(items_of()?).map_err(CellsError::Items)?,
        NameCell::Files { .. } => None,
        NameCell::Extensions { krate } if krate == record.key().prefix() => extensions_digest(items_of()?, read_of()?).map_err(CellsError::Items)?,
        NameCell::Extensions { .. } => None,
        NameCell::CfgAtoms { krate } if krate == record.key().prefix() => cfg_atoms_digest(items_of()?).map_err(CellsError::Items)?,
        NameCell::CfgAtoms { .. } => None,
    })
}

/// `Beginning(first)` read from the record by three binary searches, nothing allocated: the
/// second home of [`beginning_digest`] (equal by construction of the same runs).
fn beginning_by_record(items: &ItemsRecord<'_>, first: [u8; 16]) -> Result<Option<ContentId>, CellsError> {
    let item = |i: usize| -> Result<ItemKey, CellsError> { items.item_at(i).map_err(CellsError::Items)?.map(|row| row.key).ok_or(CellsError::Items(ItemsRecordError::Short { what: "the item rows", at: i })) };
    let module = |i: usize| -> Result<ItemKey, CellsError> { items.module_at(i).map_err(CellsError::Items)?.map(|row| row.key).ok_or(CellsError::Items(ItemsRecordError::Short { what: "the module rows", at: i })) };
    let variant = |i: usize| -> Result<ItemKey, CellsError> { items.variant_at(i).map_err(CellsError::Items)?.map(|row| row.0).ok_or(CellsError::Items(ItemsRecordError::Short { what: "the variant rows", at: i })) };
    let lists: [(usize, &dyn Fn(usize) -> Result<ItemKey, CellsError>); 3] = [(items.items(), &item), (items.modules(), &module), (items.variant_rows(), &variant)];
    let mut h = Hasher::of(&DIGESTS);
    h.update(&[11]);
    let mut any = false;
    for (count, key_at) in lists {
        let (mut lo, mut hi) = (0usize, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if first_16(key_at(mid)?) < first {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut end = lo;
        while end < count && first_16(key_at(end)?) == first {
            end += 1;
        }
        any |= end > lo;
        h.update(&((end - lo) as u64).to_le_bytes());
        for i in lo..end {
            h.id(&key_at(i)?.0);
        }
    }
    Ok(any.then(|| h.finish()))
}
