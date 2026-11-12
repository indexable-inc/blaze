//! What is asked of a crate's items, as pages: one record a crate, an answer an entry, each
//! found by its item's key and decoded alone (docs/v2/plan/design/restore.md, "Items read in
//! place").
//!
//! A crate whose `DefMap` is read from a record ([`super::pages`]) still parsed its files and
//! expanded its macros for what is asked of an item after its name is found: the assoc items
//! of an impl or a trait, an item's attribute flags, the crate's `include!`s. This writes those
//! answers when the crate is first resolved, so a crate read from them makes no syntax, no
//! item tree and no expansion to answer them.
//!
//! | segment | holds | read when |
//! |---|---|---|
//! | header | the layout, the encoding, where every other segment is | the crate's items are first asked for |
//! | slots | open-addressed: the hash of an entry's key and where the entry is, at most half full | an entry is looked up |
//! | files | the files the segments name, as [`Enc::finish`] writes them | with the header |
//! | an entry | its key, then one query's answer for one item | that query is asked of that item |
//! | includes | the crate's `include!` calls and the file each reads | `include_macro_invoc` |
//!
//! An entry's key is which query ([`What`]) and the item as [`Key`] writes it: its module's
//! index, its container and its ast id, a macro file as its call. Nothing of it asks for a
//! parse, and nothing is a database's id, so two databases write the same bytes. The slot is
//! found by FNV-1a of the key's bytes; the entry starts with the key, which tells two keys of
//! one hash apart. A lookup reads a window of slots and one entry, whatever the crate's size.
//!
//! The bytes come through the crate's [`PageSource`], held only while a read runs. A record
//! that does not read, or that lacks an entry of an item of the crate's modules, is
//! [`Unreadable`], naming the crate and the item: nothing is lowered from syntax in its place.
//! An item in a block has no entry: its block is made when its body is lowered, from that
//! body's syntax (lane records-bodies' record), so it is answered as before.
//!
//! Not here yet (restore.md lists each with its reader): attribute values, stability, fields'
//! and generic parameters' flags, the tokens of a `macro_rules!` definition, positions, lint
//! levels, the crate-wide tables.

use std::sync::Arc;

use base_db::{Crate, Db, EditionedFileId, FileId};
use blaze_hash::HashMap;
use hir_expand::MacroCallId;
use std::alloc::Global;
use syntax::ast;
use thin_vec::ThinVec;

use super::{
    Dec, Enc, R, Unreadable,
    pages::{
        PageSource, Paged, SLOT, Slot, WINDOW, fnv, read, segment, table_of, u32_at, u64_at,
        write_segment,
    },
};
use crate::{
    AdtId, AssocItemId, AstId, AttrDefId, HasModule, ImplId, Lookup, MacroId, ModuleDefId,
    ModuleId, TraitId,
    attrs::AttrFlags,
    deps::signatures::key::Key,
    nameres::{
        DefDiagnostics,
        assoc::{ImplItems, TraitItems},
        crate_def_map,
    },
    signatures::EnumVariants,
};

/// What [`Unreadable`] calls these bytes.
const KIND: &str = "item pages";

/// What a record of item pages starts with.
const MAGIC: [u8; 8] = *b"blzitpg\0";

/// The layout of a record: it changes with every change to the header, the slots, which
/// segments there are, which queries have entries ([`What`]) or what an entry holds. What a
/// value in an entry is written as is [`super::ENCODING`]'s, which the header names as well.
pub const LAYOUT: u32 = 1;

/// Bytes of the header.
const HEADER: usize = 56;

/// The encoding of the segments, as the header names it.
const ENCODING: u64 = fnv(super::ENCODING.as_bytes());

/// Which query an entry answers: the first byte of its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum What {
    /// `ImplItems::of`.
    ImplItems = 0,
    /// `TraitItems::query_with_diagnostics`.
    TraitItems = 1,
    /// `AttrFlags::query`.
    AttrFlags = 2,
}

/// The hash a key's slot is found by. Never 0, which marks an empty slot.
fn hash_of(key: &[u8]) -> u64 {
    match fnv(key) {
        0 => 1,
        hash => hash,
    }
}

/// The header: where the slots, the files and the includes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    /// The slots of the table: a power of two, or 0 for a crate with no entry.
    slots: u32,
    entries: u32,
    files: (u32, u32),
    slots_at: u32,
    includes: (u32, u32),
    total: u32,
}

impl Header {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&LAYOUT.to_le_bytes());
        out.extend_from_slice(&self.slots.to_le_bytes());
        out.extend_from_slice(&ENCODING.to_le_bytes());
        for it in [
            self.files.0,
            self.files.1,
            self.slots_at,
            self.includes.0,
            self.includes.1,
            self.total,
            self.entries,
            0,
        ] {
            out.extend_from_slice(&it.to_le_bytes());
        }
    }

    /// The header in `bytes`, or why they are no item pages this blaze reads: another record,
    /// a record of another layout, or one whose segments are of another encoding, each said by
    /// name.
    fn parse(bytes: &[u8]) -> R<Header> {
        if bytes.len() != HEADER {
            // alloc: error path
            return Err(format!("a header of {} bytes", bytes.len()));
        }
        if bytes[..8] != MAGIC {
            // alloc: error path
            return Err(format!("no item pages: the record starts with {:02x?}", &bytes[..8]));
        }
        let layout = u32_at(bytes, 8);
        if layout != LAYOUT {
            // alloc: error path
            return Err(format!(
                "item pages of layout {layout}, where this blaze reads layout {LAYOUT}"
            ));
        }
        let encoding = u64_at(bytes, 16);
        if encoding != ENCODING {
            // alloc: error path
            return Err(format!(
                "item pages whose segments are of encoding {encoding:#x}, where this blaze \
                 reads `{}` ({ENCODING:#x})",
                super::ENCODING
            ));
        }
        let pair = |at: usize| (u32_at(bytes, at), u32_at(bytes, at + 4));
        Ok(Header {
            slots: u32_at(bytes, 12),
            files: pair(24),
            slots_at: u32_at(bytes, 32),
            includes: pair(36),
            total: u32_at(bytes, 44),
            entries: u32_at(bytes, 48),
        })
    }
}

/// The crates whose items are read from pages, set by whoever has the records: read outside
/// queries alone ([`all`]). A query reads its crate's own ([`ItemPagesSlot`], [`paged`]).
#[blaze_query::input(singleton, debug)]
pub struct PagedItems {
    #[returns(ref)]
    pub maps: Arc<HashMap<Crate, Paged, Global>>,
}

/// Has the item queries read each crate of `maps` from its pages. Only a crate whose `DefMap`
/// is read from a record too ([`super::served`]) is given item pages: the serve that gives
/// one gives both, and takes both away together.
pub fn set(db: &mut Db, maps: HashMap<Crate, Paged, Global>) {
    use blaze_query::db::{Durability, Setter};
    // clone: the handle of the map set before, to tell which crates' pages move
    let was = PagedItems::try_get(&*db).map(|it| it.maps(&*db).clone());
    crate::deps::given::give_changed::<ItemPagesSlot, Paged>(db, was.as_deref(), &maps);
    // alloc: once per serve that changes which crates' items are read from pages
    let maps = Arc::new(maps);
    match PagedItems::try_get(db) {
        Some(it) => {
            it.set_maps(db).with_durability(Durability::HIGH).to(maps);
        }
        None => {
            _ = PagedItems::new(db, Durability::HIGH, maps);
        }
    }
}

/// The crates whose items are read from pages, to add to or take from.
pub fn all(db: &Db) -> HashMap<Crate, Paged, Global> {
    // clone: what was set before, once per serve that changes it
    PagedItems::try_get(db).map(|it| (**it.maps(db)).clone()).unwrap_or_default()
}

/// The kind of a crate's slot holding its entry of [`PagedItems`].
pub struct ItemPagesSlot;

/// The item pages of `krate`, by value: a [`set`] that leaves them as they were is backdated
/// here, so it runs none of the queries of `krate` again.
#[blaze_query::tracked(returns(as_ref))]
fn paged(db: &Db, krate: Crate) -> Option<Paged> {
    // clone: a count of the source and a key
    krate.given::<ItemPagesSlot, Paged>(db).cloned()
}

/// Whether the items of `krate` are read from pages.
pub fn is_paged(db: &Db, krate: Crate) -> bool {
    paged(db, krate).is_some()
}

/// What every reader of a crate's item pages needs first, read once: the header, the files
/// the segments name and the crate's modules.
#[derive(Debug, PartialEq, Eq)]
struct Head {
    header: Header,
    files: Box<[FileId]>,
    /// The crate's modules, in its `DefMap`'s order.
    own: Box<[ModuleId]>,
}

/// The head of the item pages of `krate`.
#[blaze_query::tracked(returns(ref))]
fn head(db: &Db, krate: Crate) -> Head {
    // The value is the record's, found by content: told to the engine (`row::STORED`).
    blaze_query::db::report_stored_read(db);
    let Some(paged) = paged(db, krate) else {
        // alloc: error path
        Unreadable::of(db, krate, KIND, "the crate has no item pages".to_owned()).throw()
    };
    match read_head(db, krate, &*paged.source) {
        Ok(head) => head,
        Err(why) => Unreadable::of(db, krate, KIND, why).throw(),
    }
}

// dyn: the crate's one source
fn read_head(db: &Db, krate: Crate, source: &dyn PageSource) -> R<Head> {
    let header = read(source, 0, HEADER as u32, Header::parse)?;
    if u64::from(header.total) != source.size() {
        // alloc: error path
        return Err(format!(
            "a record of {} bytes whose header says {}",
            source.size(),
            header.total
        ));
    }
    if header.slots != 0 && !header.slots.is_power_of_two() {
        // alloc: error path
        return Err(format!("a table of {} slots", header.slots));
    }
    let files = read(source, header.files.0, header.files.1, |bytes| {
        let mut dec = Dec::new(db, krate, bytes);
        dec.files()?;
        dec.finish()?;
        // alloc: the head's own files, once per crate read from item pages
        Ok(dec.files.into_owned().into_boxed_slice())
    })?;
    // alloc: the head's own modules, once per crate read from item pages
    let own = crate_def_map(db, krate).table(db).keys().copied().collect();
    Ok(Head { header, files, own })
}

/// The entry of `key` in the item pages of `krate`, read by `decode`; `None` when the record
/// has no entry of the key.
fn find<T>(
    db: &Db,
    krate: Crate,
    key: &[u8],
    decode: impl Fn(&mut Dec<'_>) -> R<T>,
) -> R<Option<T>> {
    let paged = paged(db, krate).ok_or("the crate has no item pages")?;
    let source = &*paged.source;
    let head = head(db, krate);
    let slots = head.header.slots;
    if slots == 0 {
        return Ok(None);
    }
    let hash = hash_of(key);
    let mask = slots - 1;
    let mut at = hash as u32 & mask;
    let mut probed = 0;
    // The bound: every slot once. A table is at most half full, so a walk ends at an empty
    // slot long before; one that does not is a table this did not write.
    while probed < slots {
        let run = WINDOW.min(slots - at);
        let window =
            read(source, head.header.slots_at + at * SLOT as u32, run * SLOT as u32, |bytes| {
                let mut window = [Slot::default(); WINDOW as usize];
                for (slot, bytes) in window.iter_mut().zip(bytes.chunks_exact(SLOT)) {
                    *slot = Slot::parse(bytes);
                }
                Ok(window)
            })?;
        for slot in &window[..run as usize] {
            if slot.hash == 0 {
                return Ok(None);
            }
            if slot.hash != hash {
                continue;
            }
            let found = read(source, slot.at, slot.len, |bytes| {
                let mut dec = segment(db, krate, &head.files, &head.own, bytes);
                if dec.len()? != key.len() {
                    return Ok(None);
                }
                for &byte in key {
                    if dec.u8()? != byte {
                        return Ok(None);
                    }
                }
                let found = decode(&mut dec)?;
                dec.finish()?;
                Ok(Some(found))
            })?;
            if found.is_some() {
                return Ok(found);
            }
        }
        probed += run;
        at = (at + run) & mask;
    }
    // alloc: error path
    Err(format!("a table of {slots} slots with none empty"))
}

/// Writes `owner`, an item of a module of the key's crate, after the key's [`What`].
fn owner_key(key: &mut Key<'_>, db: &Db, owner: AttrDefId) {
    match owner {
        AttrDefId::ModuleId(it) => {
            key.u8(0);
            key.module(it);
        }
        AttrDefId::AdtId(it) => {
            key.u8(1);
            key.adt(it);
        }
        AttrDefId::FunctionId(it) => {
            key.u8(2);
            let loc = it.lookup(db);
            key.container(loc.container);
            key.ast(loc.id);
        }
        AttrDefId::EnumVariantId(it) => {
            key.u8(3);
            let loc = it.lookup(db);
            key.adt(AdtId::EnumId(loc.parent));
            key.ast(loc.id);
        }
        AttrDefId::StaticId(it) => {
            key.u8(4);
            let loc = it.lookup(db);
            key.container(loc.container);
            key.ast(loc.id);
        }
        AttrDefId::ConstId(it) => {
            key.u8(5);
            let loc = it.lookup(db);
            key.container(loc.container);
            key.ast(loc.id);
        }
        AttrDefId::TraitId(it) => {
            key.u8(6);
            key.trait_(it);
        }
        AttrDefId::TypeAliasId(it) => {
            key.u8(7);
            let loc = it.lookup(db);
            key.container(loc.container);
            key.ast(loc.id);
        }
        AttrDefId::MacroId(MacroId::Macro2Id(it)) => {
            key.u8(8);
            let loc = it.lookup(db);
            key.module(loc.container);
            key.ast(loc.id);
        }
        AttrDefId::MacroId(MacroId::MacroRulesId(it)) => {
            key.u8(9);
            let loc = it.lookup(db);
            key.module(loc.container);
            key.ast(loc.id);
        }
        AttrDefId::MacroId(MacroId::ProcMacroId(it)) => {
            key.u8(10);
            let loc = it.lookup(db);
            key.module(loc.container);
            key.ast(loc.id);
        }
        AttrDefId::ImplId(it) => {
            key.u8(11);
            key.impl_(it);
        }
        AttrDefId::ExternBlockId(it) => {
            key.u8(12);
            let loc = it.lookup(db);
            key.module(loc.container);
            key.ast(loc.id);
        }
        AttrDefId::ExternCrateId(it) => {
            key.u8(13);
            let loc = it.lookup(db);
            key.module(loc.container);
            key.ast(loc.id);
        }
        AttrDefId::UseId(it) => {
            key.u8(14);
            let loc = it.lookup(db);
            key.module(loc.container);
            key.ast(loc.id);
        }
    }
}

/// One entry: a query and the item it is asked of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asked {
    Impl(ImplId),
    Trait(TraitId),
    Flags(AttrDefId),
}

impl Asked {
    /// The module the item is in.
    fn home(self, db: &Db) -> ModuleId {
        match self {
            Asked::Impl(it) => it.lookup(db).container,
            Asked::Trait(it) => it.lookup(db).container,
            Asked::Flags(it) => it.module(db),
        }
    }

    /// The entry's key, of an item of a module of `krate` (never one in a block).
    fn key(self, db: &Db, krate: Crate) -> Vec<u8> {
        let mut key = Key::new(db, krate);
        match self {
            Asked::Impl(it) => {
                key.u8(What::ImplItems as u8);
                key.impl_(it);
            }
            Asked::Trait(it) => {
                key.u8(What::TraitItems as u8);
                key.trait_(it);
            }
            Asked::Flags(it) => {
                key.u8(What::AttrFlags as u8);
                owner_key(&mut key, db, it);
            }
        }
        // alloc: a key's few dozen bytes, once per lookup that reaches the record
        key.bytes().to_vec()
    }
}

/// The answer of `asked` read from its crate's item pages: `None` when its crate's items are
/// not read from pages or the item is in a block. An item of the crate's modules the record
/// has no entry of, or an entry that does not read, is [`Unreadable`], naming the item.
fn served<T>(db: &Db, asked: Asked, decode: impl Fn(&mut Dec<'_>) -> R<T>) -> Option<T> {
    let module = asked.home(db);
    if module.block(db).is_some() {
        return None;
    }
    let krate = module.krate(db);
    if !is_paged(db, krate) {
        return None;
    }
    // The value is the record's, found by content: told to the engine (`row::STORED`).
    blaze_query::db::report_stored_read(db);
    let key = asked.key(db, krate);
    match find(db, krate, &key, decode) {
        Ok(Some(found)) => Some(found),
        // alloc: error path
        Ok(None) => Unreadable::of(db, krate, KIND, format!("no entry of {asked:?}")).throw(),
        // alloc: error path
        Err(why) => Unreadable::of(db, krate, KIND, format!("the entry of {asked:?}: {why}"))
            .throw(),
    }
}

/// [`ImplItems::of`] from its crate's item pages, if its items are read from them.
pub(crate) fn impl_items(db: &Db, id: ImplId) -> Option<(ImplItems, DefDiagnostics)> {
    served(db, Asked::Impl(id), |dec| {
        let (items, macro_calls, diagnostics) = dec.assoc()?;
        Ok((ImplItems { items, macro_calls }, diagnostics))
    })
}

/// [`TraitItems::query_with_diagnostics`] from its crate's item pages, if its items are read
/// from them.
pub(crate) fn trait_items(db: &Db, id: TraitId) -> Option<(TraitItems, DefDiagnostics)> {
    served(db, Asked::Trait(id), |dec| {
        let (items, macro_calls, diagnostics) = dec.assoc()?;
        Ok((TraitItems { items, macro_calls }, diagnostics))
    })
}

/// [`AttrFlags::query`] from its crate's item pages, if its items are read from them.
pub(crate) fn attr_flags(db: &Db, owner: AttrDefId) -> Option<AttrFlags> {
    served(db, Asked::Flags(owner), |dec| dec.flags())
}

/// [`crate::include_macro_invoc`] from the item pages of `krate`, if its items are read from
/// them: one list, where the query decoded every module's scope to walk its calls.
pub(crate) fn includes(db: &Db, krate: Crate) -> Option<Box<[(MacroCallId, EditionedFileId)]>> {
    let paged = paged(db, krate)?;
    // The value is the record's, found by content: told to the engine (`row::STORED`).
    blaze_query::db::report_stored_read(db);
    let head = head(db, krate);
    let (at, len) = head.header.includes;
    let found = read(&*paged.source, at, len, |bytes| {
        let mut dec = segment(db, krate, &head.files, &head.own, bytes);
        let all = dec.many(|dec| Ok((dec.call()?, dec.file()?)))?;
        dec.finish()?;
        Ok(all.into_boxed_slice())
    });
    match found {
        Ok(found) => Some(found),
        // alloc: error path
        Err(why) => Unreadable::of(db, krate, KIND, format!("its includes: {why}")).throw(),
    }
}

/// What collecting the assoc items of a trait or an impl gives.
type Assoc = (
    Box<[(hir_expand::name::Name, AssocItemId)]>,
    ThinVec<(AstId<ast::Item>, MacroCallId)>,
    DefDiagnostics,
);

impl Enc<'_> {
    /// The assoc items of a trait or an impl, the macro calls at assoc position some came
    /// from, and what collecting them reported. `without` leaves the items of that name out:
    /// a test's planted bug.
    fn assoc(
        &mut self,
        items: &[(hir_expand::name::Name, AssocItemId)],
        calls: &[(AstId<ast::Item>, MacroCallId)],
        diagnostics: &DefDiagnostics,
        without: Option<&str>,
    ) -> R {
        let kept = |name: &hir_expand::name::Name| without.is_none_or(|it| it != name.as_str());
        self.usize(items.iter().filter(|(name, _)| kept(name)).count())?;
        for (name, item) in items.iter().filter(|(name, _)| kept(name)) {
            self.name(name)?;
            self.def(match *item {
                AssocItemId::FunctionId(it) => ModuleDefId::FunctionId(it),
                AssocItemId::ConstId(it) => ModuleDefId::ConstId(it),
                AssocItemId::TypeAliasId(it) => ModuleDefId::TypeAliasId(it),
            })?;
        }
        self.usize(calls.len())?;
        for &(ast, call) in calls {
            self.ast_id(ast)?;
            self.call(call)?;
        }
        self.usize(diagnostics.iter().count())?;
        for it in diagnostics.iter() {
            self.diagnostic(it)?;
        }
        Ok(())
    }

    fn flags(&mut self, flags: AttrFlags) {
        let bits = flags.bits();
        self.u32(bits as u32);
        self.u32((bits >> 32) as u32);
        self.u32((bits >> 64) as u32);
        self.u32((bits >> 96) as u32);
    }
}

impl Dec<'_> {
    fn assoc(&mut self) -> R<Assoc> {
        let items = self.many(|dec| {
            let name = dec.name()?;
            let item = match dec.def()? {
                ModuleDefId::FunctionId(it) => AssocItemId::FunctionId(it),
                ModuleDefId::ConstId(it) => AssocItemId::ConstId(it),
                ModuleDefId::TypeAliasId(it) => AssocItemId::TypeAliasId(it),
                // alloc: error path
                other => return Err(format!("an assoc item that is {other:?}")),
            };
            Ok((name, item))
        })?;
        let calls = self.many(|dec| Ok((dec.ast_id::<ast::Item>()?, dec.call()?)))?;
        let diagnostics = self.many(|dec| dec.diagnostic())?;
        Ok((
            items.into_boxed_slice(),
            calls.into_iter().collect(),
            DefDiagnostics::new(diagnostics),
        ))
    }

    fn flags(&mut self) -> R<AttrFlags> {
        let mut bits = 0u128;
        for shift in [0, 32, 64, 96] {
            bits |= u128::from(self.u32()?) << shift;
        }
        Ok(AttrFlags::from_bits_retain(bits))
    }
}

/// Every entry the record of `krate` holds: each query here of each item of its modules that
/// a reader can name (through a scope, an enum's variants, a trait's or an impl's items).
/// Items in blocks are left out, and so is a macro another crate declared that a scope holds.
fn asked(db: &Db, krate: Crate) -> Vec<Asked> {
    // alloc: the entries of one crate, listed once as its record is written
    let mut all = Vec::new();
    let mut flags = |all: &mut Vec<Asked>, owner: AttrDefId| {
        let home = owner.module(db);
        if home.krate(db) == krate && home.block(db).is_none() {
            all.push(Asked::Flags(owner));
        }
    };
    let assoc = |all: &mut Vec<Asked>,
                     // dyn: one closure handed to another, a few calls a trait or an impl
                     flags: &mut dyn FnMut(&mut Vec<Asked>, AttrDefId),
                     items: &[(hir_expand::name::Name, AssocItemId)]| {
        for &(_, item) in items {
            flags(
                all,
                match item {
                    AssocItemId::FunctionId(it) => AttrDefId::FunctionId(it),
                    AssocItemId::ConstId(it) => AttrDefId::ConstId(it),
                    AssocItemId::TypeAliasId(it) => AttrDefId::TypeAliasId(it),
                },
            );
        }
    };
    for (&module, data) in crate_def_map(db, krate).table(db).iter() {
        let scope = data.scope(db);
        flags(&mut all, AttrDefId::ModuleId(module));
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(it) => flags(&mut all, AttrDefId::FunctionId(it)),
                ModuleDefId::AdtId(it) => {
                    flags(&mut all, AttrDefId::AdtId(it));
                    if let AdtId::EnumId(it) = it {
                        for &(variant, _) in EnumVariants::of(db, it).0.variants.values() {
                            flags(&mut all, AttrDefId::EnumVariantId(variant));
                        }
                    }
                }
                ModuleDefId::ConstId(it) => flags(&mut all, AttrDefId::ConstId(it)),
                ModuleDefId::StaticId(it) => flags(&mut all, AttrDefId::StaticId(it)),
                ModuleDefId::TraitId(it) => {
                    flags(&mut all, AttrDefId::TraitId(it));
                    all.push(Asked::Trait(it));
                    // dyn: one closure handed to another, a few calls a trait
                    assoc(&mut all, &mut flags, &TraitItems::query_with_diagnostics(db, it).0.items);
                }
                ModuleDefId::TypeAliasId(it) => flags(&mut all, AttrDefId::TypeAliasId(it)),
                ModuleDefId::MacroId(it) => flags(&mut all, AttrDefId::MacroId(it)),
                // A module has its own turn of the loop; a variant is listed with its enum.
                ModuleDefId::ModuleId(_)
                | ModuleDefId::EnumVariantId(_)
                | ModuleDefId::BuiltinType(_) => {}
            }
        }
        for (_, macros) in scope.legacy_macros() {
            for &it in macros {
                flags(&mut all, AttrDefId::MacroId(it));
            }
        }
        for it in scope.unnamed_consts() {
            flags(&mut all, AttrDefId::ConstId(it));
        }
        for it in scope.extern_crate_decls() {
            flags(&mut all, AttrDefId::ExternCrateId(it));
        }
        for it in scope.use_decls() {
            flags(&mut all, AttrDefId::UseId(it));
        }
        for it in scope.extern_blocks() {
            flags(&mut all, AttrDefId::ExternBlockId(it));
        }
        for it in scope.impls() {
            flags(&mut all, AttrDefId::ImplId(it));
            all.push(Asked::Impl(it));
            // dyn: as above
            assoc(&mut all, &mut flags, &ImplItems::of(db, it).0.items);
        }
    }
    all
}

/// The item pages of `krate`, from what this database answers of its items, or why they
/// cannot be written. Called by the task that resolved the crate from source, which has its
/// trees: nothing is parsed again for it. Refuses what [`super::encode`] refuses.
pub fn encode(db: &Db, krate: Crate) -> R<Vec<u8>> {
    encode_with(db, krate, None)
}

/// [`encode`]; `without` leaves the assoc items of that name out of every trait's and impl's
/// entry: a test's planted bug.
fn encode_with(db: &Db, krate: Crate, without: Option<&str>) -> R<Vec<u8>> {
    let mut enc = Enc::new(db, krate);
    for (index, &module) in crate_def_map(db, krate).table(db).keys().enumerate() {
        enc.modules.insert(module, (0, index as u32));
    }
    enc.indexed.insert(krate);

    // Each entry by its key, so the record's order is the keys' and never the order a
    // database's maps gave the items in.
    // alloc: encoding, once per crate per write of its item pages (not on a build's path)
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    // Bounded by the crate's own items: one entry an impl, a trait, and an attribute owner.
    // An impl or a trait no build asked has its assoc items collected here (name resolution
    // does not hold them: they are `ImplItems::of`'s own walk), so the span says how many.
    let asked = asked(db, krate);
    let collected =
        asked.iter().filter(|it| matches!(it, Asked::Impl(_) | Asked::Trait(_))).count();
    let _span = tracing::info_span!(
        target: "blaze",
        "write item pages",
        krate = %krate.named(db),
        entries = asked.len() as u64,
        assoc_owners = collected as u64,
    )
    .entered();
    for asked in asked {
        let key = asked.key(db, krate);
        // alloc: encoding, as above
        let mut bytes = Vec::new();
        write_segment(&mut enc, &mut bytes, |enc| {
            enc.usize(key.len())?;
            for &byte in &key {
                enc.u8(byte);
            }
            match asked {
                Asked::Impl(it) => {
                    let (items, diagnostics) = ImplItems::of(db, it);
                    enc.assoc(&items.items, &items.macro_calls, diagnostics, without)
                }
                Asked::Trait(it) => {
                    let (items, diagnostics) = TraitItems::query_with_diagnostics(db, it);
                    enc.assoc(&items.items, &items.macro_calls, diagnostics, without)
                }
                Asked::Flags(it) => {
                    enc.flags(AttrFlags::query(db, it));
                    Ok(())
                }
            }
        })
        // alloc: error path
        .map_err(|why| format!("the entry of {asked:?}: {why}"))?;
        entries.push((key, bytes));
    }
    entries.sort_unstable();
    entries.dedup();
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err("two answers under one key".to_owned()); // alloc: error path
    }

    // alloc: encoding, as above
    let mut body: Vec<u8> = Vec::new();
    // alloc: encoding, as above
    let mut placed: Vec<(u64, (u32, u32))> = Vec::with_capacity(entries.len());
    for (key, bytes) in &entries {
        let at = u32::try_from(body.len()).map_err(|_| "item pages over 4 GiB")?;
        let len = u32::try_from(bytes.len()).map_err(|_| "an entry over 4 GiB")?;
        body.extend_from_slice(bytes);
        placed.push((hash_of(key), (at, len)));
    }
    // The crate's `include!`s, in the order of their calls' fingerprints.
    // alloc: encoding, as above
    let mut included = Vec::new();
    for &(call, file) in crate::include_macro_invoc(db, krate) {
        included.push((enc.fingerprint(|enc| enc.call(call))?, call, file));
    }
    included.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let includes = write_segment(&mut enc, &mut body, |enc| {
        enc.usize(included.len())?;
        for &(_, call, file) in &included {
            enc.call(call)?;
            enc.file(file)?;
        }
        Ok(())
    })?;

    // The files every segment names, then the segments: `Enc::finish` writes the first before
    // the second and refuses bytes that name a place of one checkout.
    enc.calls.clear();
    enc.contexts.clear();
    let body_len = body.len();
    enc.out = body;
    let named = enc.finish()?;
    let files_len = named.len() - body_len;

    let slots = match placed.len() {
        0 => 0,
        entries => (entries * 2).next_power_of_two(),
    };
    let slots_at = HEADER;
    let files_at = slots_at + slots * SLOT;
    let base = u32::try_from(files_at + files_len).map_err(|_| "item pages over 4 GiB")?;
    let total = u32::try_from(files_at + named.len()).map_err(|_| "item pages over 4 GiB")?;
    // Every place so far is from the body's start: from the record's.
    for (_, (at, _)) in &mut placed {
        *at += base;
    }
    let (count, table) = table_of(&placed);
    debug_assert_eq!(count as usize, slots);
    let header = Header {
        slots: count,
        entries: placed.len() as u32,
        files: (files_at as u32, files_len as u32),
        slots_at: slots_at as u32,
        includes: (includes.0 + base, includes.1),
        total,
    };
    // alloc: the record, once per crate per write of its item pages
    let mut out = Vec::with_capacity(total as usize);
    header.write(&mut out);
    debug_assert_eq!(out.len(), HEADER);
    out.extend_from_slice(&table);
    debug_assert_eq!(out.len(), files_at);
    out.extend_from_slice(&named);
    debug_assert_eq!(out.len(), total as usize);
    Ok(out)
}

#[cfg(test)]
mod tests;
