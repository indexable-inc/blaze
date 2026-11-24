//! The one printer of an item in a refusal (rule 19: an error names the thing, by its PATH).
//!
//! A key carries no name (rules 21 and 25: a key holds no machine data and no text), and its
//! short `Display` is its CRATE's part (`blaze_id::ItemKey`: the first 12 hex digits are the
//! crate prefix), so a refusal that prints a key names the crate, never the item: on the hello
//! copy users saw "ItemKey(c41ed2ee1fe2)" for a library key and a 38-digit number for a
//! checker key. So a refusal names its item by the path the printing layer resolves from the
//! names it already holds (the names source and the key map), with the key's ITEM part as a
//! short suffix tools can grep: [`ItemShown`].
//!
//! The path is built by a fixed number of steps (an owner, then the item), never by recursion:
//! a member's (an associated fn, const or type) is its trait's or impl's path and its name; a
//! variant's is its enum's path and its name; an impl, which has no name, is shown by its
//! header as written (`<impl Default for stat>`); an item whose module has no path (a block's
//! module) says so instead of being cut to its bare name.

use core::fmt;
use std::boxed::Box;
use std::format;
use std::string::{String, ToString};

use blaze_id::ItemKey as LibKey;
use blaze_own::Alloc;
use blaze_scope::items_record::{ItemsRecord, ItemsRecordError};
use blaze_surface::{ItemKind, Range};
use blaze_typeck::ItemKey;

use super::check::Records;
use super::heads::{HeadError, Want};
use super::keys::{Keys, Origin};
use super::source::NamesSource;

/// A body fact's defining site, resolved from the crate's names and syntax. A missing
/// semantic head (including the opaque's entire owner row) does not affect this lookup.
#[derive(Debug)]
pub struct FactDefinition {
    pub krate: Option<usize>,
    /// Supplied by the body's package scope when available.
    pub version: Option<&'static str>,
    /// A resolved item's path already names its crate.
    unresolved_in: Option<String>,
    site: Result<DefinitionSite, DefinitionLost>,
}

#[derive(Debug)]
struct DefinitionSite {
    words: String,
    kind: &'static str,
    opaque: Option<u32>,
    unmapped: Option<super::expansion_origin::Unmapped>,
}

#[derive(Debug)]
enum DefinitionLost {
    NoCrate,
    NoRecord,
    Record(ItemsRecordError),
    NoMatch,
    Ambiguous,
    Missing { item: LibKey, what: &'static str },
}

impl From<ItemsRecordError> for DefinitionLost {
    fn from(error: ItemsRecordError) -> Self {
        Self::Record(error)
    }
}

impl fmt::Display for FactDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = &self.unresolved_in {
            write!(f, "in crate {name}: ")?;
        }
        match &self.site {
            Ok(DefinitionSite { words, kind, opaque, unmapped }) => {
                if let Some(index) = opaque {
                    write!(f, "opaque type #{index} of ")?;
                } else {
                    write!(f, "{kind} ")?;
                }
                f.write_str(words)?;
                if let Some(why) = unmapped {
                    write!(f, " (source origin unavailable: {why:?})")?;
                }
            }
            Err(DefinitionLost::Record(error)) => write!(f, "definition lookup refused by its items record: {error:?}")?,
            Err(DefinitionLost::Missing { item, what }) => write!(f, "definition lookup lacks {what} of {item:x}")?,
            Err(why) => write!(f, "definition lookup refused ({why:?}, crate row {:?})", self.krate)?,
        }
        if let Some(version) = self.version {
            write!(f, " (crate version {version})")?;
        }
        Ok(())
    }
}

impl FactDefinition {
    pub fn of<'n>(names: &'n super::source::Records<'n>, key: ItemKey, owner: Option<(ItemKey, u32)>, alloc: Alloc<'_>) -> Self {
        let mut prefix = [0; 32];
        prefix[..16].copy_from_slice(&key.0.to_be_bytes());
        let krate = names.crate_of(LibKey(blaze_id::ContentId(prefix)));
        let mut unresolved_in = None;
        let site = krate.ok_or(DefinitionLost::NoCrate).and_then(|krate| {
            let record = names.items_record(krate).ok_or(DefinitionLost::NoRecord)?;
            let mut search = DefinitionSearch { names, record, key, owner, alloc, found: None, incomplete: None };
            let site = search.find();
            if site.is_err() {
                unresolved_in = Some(record.name()?.to_owned());
            }
            site
        });
        Self { krate, version: None, unresolved_in, site }
    }
}

#[derive(Clone, Copy)]
struct Declaration<'n> {
    container: LibKey,
    lib: LibKey,
    checker: ItemKey,
    kind: ItemKind,
    member: Option<super::source::MemberOf<'n>>,
}

struct DefinitionSearch<'n, 'a> {
    names: &'n super::source::Records<'n>,
    record: &'n ItemsRecord<'n>,
    key: ItemKey,
    owner: Option<(ItemKey, u32)>,
    alloc: Alloc<'a>,
    found: Option<DefinitionSite>,
    incomplete: Option<DefinitionLost>,
}

impl<'n> DefinitionSearch<'n, '_> {
    fn find(&mut self) -> Result<DefinitionSite, DefinitionLost> {
        use super::keys::{MemberKind, checker_of, member_key_of};
        if let Some((owner, _)) = self.owner
            && let Some(lib) = self.names.key_beginning(owner.0.to_be_bytes())
            && let Some(row) = self.record.item(lib)?
            && matches!(row.kind, ItemKind::Function | ItemKind::TypeAlias)
        {
            let decl = Declaration { container: lib, lib, checker: owner, kind: row.kind, member: None };
            return self.opaque(decl)?.ok_or(DefinitionLost::NoMatch);
        }
        if self.owner.is_none()
            && let Some(lib) = self.names.key_beginning(self.key.0.to_be_bytes())
        {
            if let Some(row) = self.record.item(lib)? {
                let decl = Declaration { container: lib, lib, checker: self.key, kind: row.kind, member: None };
                return self.show_item(decl, row.kind.name());
            }
            if let Some((adt, index)) = self.record.variant_home(lib)? {
                let decl = Declaration { container: adt, lib, checker: self.key, kind: ItemKind::Variant, member: None };
                let text = self.text(decl)?;
                let lowered = self.lower(decl, text)?;
                let view = lowered.view();
                let missing = || DefinitionLost::Missing { item: lib, what: "the variant's name range" };
                let rows = view.rows(view.list(blaze_lower::head::FIELDS)).map_err(|_| missing())?;
                let variant = blaze_lower::form::Variant::decode(*rows.get(index as usize).ok_or_else(missing)?).map_err(|_| missing())?;
                let name = view.name_range(variant.name).map_err(|_| missing())?;
                return self.show(decl, "variant", None, view.site(name).map_err(|_| missing())?, view.text_at(name).map_err(|_| missing())?);
            }
            if let Some(module) = self.record.module(lib)? {
                let path = module.path.ok_or(DefinitionLost::Missing { item: lib, what: "the module's path" })?;
                let mut words = path.to_owned();
                words.push_str(" (the module row has no declaration range)");
                return Ok(DefinitionSite { words, kind: "module", opaque: None, unmapped: None });
            }
        }
        // Library members have their own sorted key section. A checker member is keyed
        // differently, by its container, kind and name.
        let lookup = self.owner.map_or(self.key, |(owner, _)| owner);
        if self.names.errors().iter().any(|error| {
            matches!(error, super::source::SourceError::KeysBeginAlike { first, .. } if checker_of(*first) == lookup)
        }) {
            return Err(DefinitionLost::Ambiguous);
        }
        let mut lo = 0;
        let mut hi = self.record.owned_members();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let lib = self.record.owned_key_at(mid)?.ok_or(DefinitionLost::NoRecord)?;
            if checker_of(lib) < lookup { lo = mid + 1; } else { hi = mid; }
        }
        if let Some(lib) = self.record.owned_key_at(lo)?
            && checker_of(lib) == lookup
            && let Some((container, _)) = self.record.member_owner(lib)?
        {
            if self.record.owned_key_at(lo + 1)?.is_some_and(|next| checker_of(next) == lookup) {
                return Err(DefinitionLost::Ambiguous);
            }
            self.members(container, Some(lib))?;
            return self.take();
        }
        let record = self.record;
        for i in 0..record.items() {
            let row = record.item_at(i)?.ok_or(DefinitionLost::NoRecord)?;
            let lib = row.key;
            let checker = checker_of(lib);
            let decl = Declaration { container: lib, lib, checker, kind: row.kind, member: None };
            match row.kind {
                ItemKind::Trait | ItemKind::Impl => self.members(lib, None)?,
                ItemKind::Struct if row.ctor && member_key_of(checker, MemberKind::Ctor, "") == self.key => {
                    let site = self.show_item(decl, "constructor")?;
                    self.put(site)?;
                }
                ItemKind::Function | ItemKind::TypeAlias => self.candidate(decl)?,
                _ => {}
            }
        }
        self.take()
    }

    fn put(&mut self, site: DefinitionSite) -> Result<(), DefinitionLost> {
        if self.found.replace(site).is_some() { Err(DefinitionLost::Ambiguous) } else { Ok(()) }
    }

    fn take(&mut self) -> Result<DefinitionSite, DefinitionLost> {
        self.found.take().ok_or_else(|| self.incomplete.take().unwrap_or(DefinitionLost::NoMatch))
    }

    fn members(&mut self, container: LibKey, only: Option<LibKey>) -> Result<(), DefinitionLost> {
        use super::keys::{MemberKind, checker_of, member_key_of};
        let mut result = Ok(());
        self.names.each_member(container, &mut |m| {
            if result.is_err() || only.is_some_and(|lib| m.key != lib) { return; }
            let Some(name) = m.name else { return };
            let kind = match m.kind {
                ItemKind::Function => MemberKind::Fn,
                ItemKind::TypeAlias => MemberKind::Type,
                ItemKind::Const => MemberKind::Const,
                _ => return,
            };
            let checker = member_key_of(checker_of(container), kind, name);
            let decl = Declaration { container, lib: m.key, checker, kind: m.kind, member: Some(m) };
            result = if self.owner.is_none() && (checker == self.key || only == Some(m.key)) {
                self.show_item(decl, m.kind.name()).and_then(|site| self.put(site))
            } else if m.kind == ItemKind::Function {
                self.candidate(decl)
            } else { Ok(()) };
        });
        result
    }

    fn text(&self, decl: Declaration<'n>) -> Result<&'n str, DefinitionLost> {
        decl.member.map(|m| m.text).or_else(|| self.names.text_of(decl.container).map(|it| it.text))
            .ok_or(DefinitionLost::Missing { item: decl.lib, what: "the item text" })
    }

    fn lower<'t>(&self, decl: Declaration<'_>, text: &'t str) -> Result<blaze_lower::Lowered<'_, 't>, DefinitionLost> {
        let missing = |what| DefinitionLost::Missing { item: decl.lib, what };
        let krate = self.names.crate_of(decl.container).ok_or(DefinitionLost::NoCrate)?;
        let atoms = self.names.cfg_atoms(krate).ok_or_else(|| missing("the crate's cfg"))?;
        blaze_lower::lower_signature(text, self.record.edition(), &mut super::heads::CrateCfg(atoms), self.alloc)
            .map_err(|_| missing("a readable signature"))
    }

    fn candidate(&mut self, decl: Declaration<'n>) -> Result<(), DefinitionLost> {
        if self.owner.is_some_and(|(owner, _)| owner != decl.checker) { return Ok(()); }
        match self.opaque(decl) {
            Ok(Some(site)) => self.put(site),
            Ok(None) => Ok(()),
            Err(error) => {
                // An unrelated unlowerable signature cannot identify this key. Retain the
                // reason in case no signature with an exact identity can be found.
                self.incomplete.get_or_insert(error);
                Ok(())
            }
        }
    }

    fn opaque(&self, decl: Declaration<'n>) -> Result<Option<DefinitionSite>, DefinitionLost> {
        use super::keys::opaque_key;
        use blaze_lower::{form::TypeId, head, machine::sig};
        let text = self.text(decl)?;
        let alias = decl.kind == ItemKind::TypeAlias;
        let has_key = |n| self.owner.is_none_or(|(_, tagged)| super::tait::of_alias(tagged) == alias && (tagged & !super::tait::ALIAS_OPAQUE) == n)
            && opaque_key(decl.checker, n) == self.key;
        // Without a macro, each numbered opaque needs one written 'impl', apart from
        // the implicit future. This upper bound only skips impossible key matches.
        if (alias || !text.contains('!')) && self.owner.is_none() {
            let max = text.matches("impl").count() + usize::from(!alias && text.contains("async"));
            if !(0..max).any(|n| u32::try_from(n).is_ok_and(has_key)) { return Ok(None); }
        }
        let lowered = self.lower(decl, text)?;
        let view = lowered.view();
        let first = u32::from(!alias && view.head[head::FLAGS] & sig::ASYNC != 0);
        let missing = |what| DefinitionLost::Missing { item: decl.lib, what };
        if first == 1 && has_key(0) {
            let end = match view.head_opt(head::FN_TEXT_PREFIX) {
                Some(n) => text.get(..n as usize).ok_or_else(|| missing("the async signature's prefix"))?,
                None => text,
            };
            return self.show(decl, "async fn", Some(0), Range::new(0, end.len() as u32), end).map(Some);
        }
        let Some(root) = view.head_opt(head::TY).map(TypeId) else { return Ok(None) };
        let mut ids = std::vec::Vec::new_in(self.alloc);
        let macros = super::types::return_impls(&view, root, &mut ids).map_err(|_| missing("opaque syntax numbering"))?;
        let expanded;
        let mut offset = 0;
        let opaque_view = if macros && !alias {
            let at = view.type_range(root).map_err(|_| missing("the return type's range"))?;
            let range = view.site(at).map_err(|_| missing("the return type's site"))?;
            let written = view.text_at(at).map_err(|_| missing("the return type's text"))?;
            let module = self.names.home_of(decl.container).ok_or_else(|| missing("the defining module"))?;
            let marks = super::item_site::member_marks(self.names, decl.container, decl.member.and_then(|m| m.written_by), text)
                .map_err(|_| missing("the defining signature's token marks"))?;
            expanded = super::type_expand::expand_type_text(self.names, module, written, marks, range, self.alloc)
                .map_err(|_| missing("the expanded return type"))?;
            ids.clear();
            if super::types::return_impls(&expanded.view(), expanded.root, &mut ids).map_err(|_| missing("the expanded opaque types"))? {
                return Err(missing("the remaining type macro's expansion"));
            }
            offset = range.start;
            expanded.view()
        } else { view };
        for (j, t) in ids.into_iter().enumerate() {
            let n = u32::try_from(j).ok().and_then(|j| first.checked_add(j)).ok_or_else(|| missing("a representable opaque ordinal"))?;
            if !has_key(n) { continue; }
            let at = opaque_view.type_range(t).map_err(|_| missing("the opaque type's range"))?;
            let site = opaque_view.site(at).map_err(|_| missing("the opaque type's source"))?;
            let snippet = opaque_view.text_at(at).map_err(|_| missing("the opaque type's text"))?;
            // For an expanded term, site() projects through the lowering's expansion_calls
            // to its outermost call in the WRITTEN return text. Only then move into the item.
            let start = offset.checked_add(site.start).ok_or_else(|| missing("the opaque type's item-relative start"))?;
            let end = offset.checked_add(site.end).ok_or_else(|| missing("the opaque type's item-relative end"))?;
            let mut shown = self.show(decl, decl.kind.name(), Some(n), Range::new(start, end), snippet)?;
            if at.in_expansion() {
                shown.words.push_str(" (range of the type macro call, text from its expansion)");
            }
            return Ok(Some(shown));
        }
        Ok(None)
    }

    fn show_item(&self, decl: Declaration<'n>, kind: &'static str) -> Result<DefinitionSite, DefinitionLost> {
        let text = self.text(decl)?;
        self.show(decl, kind, None, Range::new(0, text.len() as u32), text)
    }

    fn show(&self, decl: Declaration<'n>, kind: &'static str, opaque: Option<u32>, range: Range, snippet: &str) -> Result<DefinitionSite, DefinitionLost> {
        use core::fmt::Write;
        let missing = |what| DefinitionLost::Missing { item: decl.lib, what };
        let mut words = lib_path(self.names, decl.lib).ok_or_else(|| missing("the definition's path"))?;
        let crate_name = self.record.name()?;
        if words.split("::").next() != Some(crate_name) {
            words.insert_str(0, "::");
            words.insert_str(0, crate_name);
        }
        let krate = self.names.crate_of(decl.container).ok_or(DefinitionLost::NoCrate)?;
        let (at, start) = self.names.item_start(decl.container).ok_or_else(|| missing("the declaration's position"))?;
        let (at, start) = if let Some(member) = decl.member {
            if let Some(call) = member.written_by {
                let at = blaze_scope::resolve::At::Expansion(call);
                let text = self.names.whole_text(krate, at).ok_or_else(|| missing("the expansion's text"))?;
                (at, blaze_scope::expansion_origins::offset_in(text, member.text).ok_or_else(|| missing("the member's expansion range"))?)
            } else {
                let whole = self.names.text_of(decl.container).ok_or_else(|| missing("the container text"))?;
                let offset = blaze_scope::expansion_origins::offset_in(whole.text, member.text).ok_or_else(|| missing("the member's range"))?;
                (at, start.checked_add(offset).ok_or_else(|| missing("the member's absolute start"))?)
            }
        } else { (at, start) };
        let mut unmapped = None;
        match at {
            blaze_scope::resolve::At::File(file) => {
                let file_name = self.names.file_shown(krate, file).ok_or_else(|| missing("the file path"))?;
                let file_text = self.names.file_text(krate, file).ok_or_else(|| missing("the current file text"))?;
                let position = start.checked_add(range.start).ok_or_else(|| missing("the absolute source position"))?;
                let before = file_text.get(..position as usize).ok_or_else(|| missing("a UTF-8 source position"))?;
                let line = 1 + before.bytes().filter(|&b| b == b'\n').count();
                let col = 1 + before.rsplit('\n').next().map_or(0, |s| s.chars().count());
                write!(words, " at {file_name}:{line}:{col}").expect("writing a string");
            }
            blaze_scope::resolve::At::Expansion(call) => {
                let placed = match decl.member {
                    Some(m) => super::expansion_origin::file_member_range(self.names, decl.container, m.text, blaze_typeck::Range { start: range.start, end: range.end }, self.alloc),
                    None => super::expansion_origin::file_range(self.names, Some(decl.container), blaze_typeck::Range { start: range.start, end: range.end }, self.alloc),
                };
                match placed {
                    super::expansion_origin::Filed::Unmapped(why) => {
                        unmapped = Some(why);
                        write!(words, " in expansion {call:x}").expect("writing a string");
                    }
                    _ => write!(words, " (expansion {call:x}, {placed})").expect("writing a string"),
                }
            }
        }
        let end = snippet.char_indices().nth(60).map_or(snippet.len(), |(at, _)| at);
        write!(words, ", bytes {}..{} in the {kind}, `{}`", range.start, range.end, &snippet[..end]).expect("writing a string");
        Ok(DefinitionSite { words, kind, opaque, unmapped })
    }
}

/// The digits of a key shown after a path: bytes 8..14, the item's part (bytes 0..8 are the
/// crate's prefix, which the path already names).
const TAIL: core::ops::Range<usize> = 8..14;

/// An item for a refusal: its whole path when the names source gives one, and its key.
/// `Display`: `a::b::Item (key 1a2b3c4d5e6f)`; with no path, every digit of the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemShown {
    /// The item's path, whole; `None`: the names source holds none for the key.
    path: Option<Box<str>>,
    /// For an item with no path, WHERE it is written ([`written`]: its module, and its file and
    /// line or the macro call that wrote it), so a path-less item is still named by a place a
    /// person can open (rule 19). `None` when it has a path, or the source holds no place.
    written: Option<Box<str>>,
    /// The key's first 16 bytes (a checker key is all of them, a library key's first 16).
    key: [u8; 16],
}

impl ItemShown {
    /// A checker key `k` over the key map `keys` (which files members) and `names`.
    pub fn of<'n, S: NamesSource<'n>>(names: S, keys: &Keys, k: ItemKey) -> ItemShown {
        // The library key a place is read from: the item's own, or a member's container's.
        let mut placed = keys.library_in(names, k);
        let path = match keys.origin(k) {
            Some(Origin::Member { container, name, .. }) => {
                let c = keys.library_in(names, *container);
                placed = placed.or(c);
                c.and_then(|c| lib_path(names, c)).map(|o| format!("{o}::{name}"))
            }
            _ => placed.and_then(|lib| lib_path(names, lib)),
        };
        let written = match path {
            Some(_) => None,
            None => placed.and_then(|lib| written(names, lib)),
        };
        ItemShown { path: path.map(String::into_boxed_str), written, key: k.0.to_be_bytes() }
    }

    /// [`ItemShown::of`] with the store's body-local items. A fn, static, const or impl
    /// declared in a block has no names row, so `of` had no path for it (curve25519-dalek's
    /// `fn m` in `<&FieldElement51 as Mul>::mul`, tempo's tests t1530). Its path is the path of
    /// the item whose body declares it, followed by each local's declared name (row_path.rs
    /// `RowPath::of`, the one reading of it: an impl's segment is its header), as rustc spells
    /// a local item (`a::f::m`); its key is its own. The walk goes through at most 64 owners
    /// (rule 18). Past that, or at a segment no declaration names, this is [`ItemShown::of`]'s
    /// answer, never a guess.
    pub fn of_in<'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &super::heads::Store<'n>, k: ItemKey) -> ItemShown {
        let mut idents: std::vec::Vec<String> = std::vec::Vec::new();
        let mut at = k;
        for _ in 0..64 {
            let Some(local) = store.locals.head(at) else {
                if idents.is_empty() {
                    break;
                }
                let outer = Self::of(names, keys, at);
                idents.reverse();
                let path = outer.path.map(|p| format!("{p}::{}", idents.join("::")).into_boxed_str());
                return ItemShown { path, written: outer.written, key: k.0.to_be_bytes() };
            };
            match super::row_path::RowPath::of(names, keys, store, at) {
                Ok(p) => idents.push(p.ident().to_string()),
                Err(_) => break,
            }
            at = local.enclosing;
        }
        Self::of(names, keys, k)
    }

    /// A library key `lib` over `names`.
    pub fn of_lib<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> ItemShown {
        let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15, ..] = lib.0.0;
        let key = [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15];
        let path = lib_path(names, lib);
        let written = match path {
            Some(_) => None,
            None => written(names, lib),
        };
        ItemShown { path: path.map(String::into_boxed_str), written, key }
    }

    /// The path, when the names source gave one.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// The key's first 16 bytes, as [`Display`](fmt::Display) shows them.
    pub fn key(&self) -> [u8; 16] {
        self.key
    }

    /// Where a path-less item is written ([`written`]), when the source holds its place.
    pub fn written(&self) -> Option<&str> {
        self.written.as_deref()
    }

    /// The text a refusal's field holds (`Front2Refusal`'s `function`, `adt`, ...).
    pub fn boxed(&self) -> Box<str> {
        self.to_string().into_boxed_str()
    }
}

impl fmt::Display for ItemShown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(p) => {
                write!(f, "{p} (key ")?;
                self.key[TAIL].iter().try_for_each(|b| write!(f, "{b:02x}"))?;
                write!(f, ")")
            }
            None => {
                match &self.written {
                    Some(w) => write!(f, "an item with no path, {w} (key ")?,
                    None => write!(f, "an item the names source holds no path for (key ")?,
                }
                self.key.iter().try_for_each(|b| write!(f, "{b:02x}"))?;
                write!(f, ")")
            }
        }
    }
}

/// At most this many macro calls are followed out to a file (rule 18): an expansion's item is
/// placed at its call, which may itself be an expansion's.
const MOST_CALLS: usize = 16;

/// WHERE the path-less item `lib` is written, as words: `written in `core::fmt` at
/// library/core/src/fmt/mod.rs:420`, or `written by a macro call in `core::fmt`, the call at
/// library/core/src/fmt/mod.rs:420` (each call out to the outermost, by [`MOST_CALLS`] steps,
/// never recursion), with whichever part the source holds. Read only for a message: the line is
/// counted in the file's text the source already holds (rule 71: no `file:line` is kept).
/// `None`: the source holds neither the item's module path nor its place.
fn written<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> Option<Box<str>> {
    use blaze_scope::resolve::At;
    let module = names.home_of(lib).and_then(|m| names.module_path(m)).map(|p| shown_segments(p).collect::<std::vec::Vec<_>>().join("::")).filter(|p| !p.is_empty());
    let mut k = lib;
    let mut by_macro = false;
    let mut at: Option<String> = None;
    for _ in 0..MOST_CALLS {
        let Some(place) = names.place_of(k) else { break };
        match place.at {
            At::File(file) => {
                let shown = names.file_shown(place.krate, file);
                let line = names.item_start(k).and_then(|(_, start)| {
                    let text = names.file_text(place.krate, file)?;
                    let before = text.as_bytes().get(..start as usize)?;
                    Some(before.iter().filter(|c| **c == b'\n').count() + 1)
                });
                at = match (shown, line) {
                    (Some(s), Some(l)) => Some(format!("{s}:{l}")),
                    (Some(s), None) => Some(format!("{s}")),
                    (None, _) => None,
                };
                break;
            }
            At::Expansion(call) => {
                by_macro = true;
                k = call;
            }
        }
    }
    let how = if by_macro { "written by a macro call" } else { "written" };
    let text = match (module, at) {
        (Some(m), Some(a)) if by_macro => format!("{how} in `{m}`, the call at {a}"),
        (Some(m), Some(a)) => format!("{how} in `{m}` at {a}"),
        (Some(m), None) => format!("{how} in `{m}`"),
        (None, Some(a)) if by_macro => format!("{how}, the call at {a}"),
        (None, Some(a)) => format!("{how} at {a}"),
        (None, None) => return None,
    };
    Some(text.into_boxed_str())
}

/// `k` as a refusal's field holds it ([`ItemShown::boxed`]), over a body's records: what
/// program.rs passes to `Front2Refusal` where it named the item by `reach::fn_shown` (its bare
/// name, which stays the name rows and symbols are made from).
pub(crate) fn refused<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, k: ItemKey) -> Box<str> {
    ItemShown::of(r.names, &*r.keys, k).boxed()
}

/// `lib`'s path: its owner's (a member's trait or impl, a variant's enum, else itself) module
/// path and name, then its own name. `None`: the source names neither the item nor its owner.
fn lib_path<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> Option<String> {
    let (owner, last) = match names.member_owner(lib) {
        Some((owner, name)) => (owner, Some(name)),
        None => match names.variant_of(lib) {
            // A variant has no item row (no `name_of`): its name is its enum's list's.
            Some((enum_, index)) => (enum_, Some(names.variants(enum_)?.get(index as usize)?.name)),
            None => (lib, None),
        },
    };
    let mut out = String::new();
    match names.home_of(owner) {
        Some(m) => match names.module_path(m) {
            Some(p) => {
                for segment in shown_segments(p) {
                    out.push_str(segment);
                    out.push_str("::");
                }
            }
            None => out.push_str("<a module the names source holds no path for>::"),
        },
        None => {}
    }
    match names.name_of(owner) {
        Some(n) => out.push_str(n),
        // An impl has no name: its header as written, from its first token.
        None => out.push_str(&impl_shown(names.signature_text_of(owner)?.impl_header?)),
    }
    if let Some(l) = last {
        out.push_str("::");
        out.push_str(l);
    }
    Some(out)
}

/// The segments of module path `path` a person is shown: every one but a block's module. The
/// names library writes a body's blocks, and a statement call's blocks, as modules named
/// `__blaze_block_N` and `__blaze_block_x<tag>_N` (blaze-scope blocks.rs, expanded_blocks.rs):
/// names no text writes and no path can name (`blocks::is_block_module`, the one test), so a
/// path holding one (tempo's `serde_sealed_or_recovered_block::__blaze_block_0::__blaze_block_0
/// ::<impl ..>::serialize`, rand's four `__blaze_block_x..` around `<impl Fill for [u64]>`)
/// names the item by a word that is in no file. rustc's path of an item in a block names the
/// enclosing body instead (`a::f::<impl ..>`); the module does not say which body it is, so
/// the block is left out and the key, shown beside the path, tells two such items apart.
fn shown_segments(path: &str) -> impl Iterator<Item = &str> {
    path.split("::").filter(|s| !s.is_empty() && !blaze_scope::blocks::is_block_module(s))
}

/// An impl as a path segment shows it (here and in row_path.rs, a row's path): `<` its header
/// `>`, from its first modifier (`const`, `unsafe`) or the keyword `impl`, on one line. The
/// header's span starts after the impl's outer attributes but not after a comment written
/// between them and the header (core's `// FIXME(fee1-dead): ..` line above
/// `const impl<T, A: ..> RawVec<T, A>`, which hello's stops printed as the path), so the
/// comments, doc comments and attributes before the first token are left out, and each run of
/// whitespace and comments inside it is one space: a path is one line whatever the header's
/// layout. A string literal (an attribute's, a const argument's) is read whole, so a `//` or
/// `]` in it is neither a comment nor an attribute's end. A loop over bytes that cuts only at
/// ASCII bytes, so every slice is on a char boundary.
pub(crate) fn impl_shown(header: &str) -> String {
    let mut out = String::with_capacity(header.len() + 2);
    write_impl_shown(header, &mut out);
    out
}

/// Writes `header` as a single-line `<...>` impl segment into `out` without allocating:
/// outer attributes and comments before the first token are skipped, and each internal run of
/// whitespace and comments collapses to one space.
pub(crate) fn write_impl_shown(header: &str, out: &mut dyn fmt::Write) {
    let bytes = header.as_bytes();
    let _ = out.write_char('<');
    let mut at = 0usize;
    // Whitespace or a comment stands since the last token byte copied.
    let mut gap = false;
    // No token is copied yet: an attribute here is an outer one, left out.
    let mut lead = true;
    while at < bytes.len() {
        let c = bytes[at];
        if c.is_ascii_whitespace() {
            at += 1;
            gap = true;
            continue;
        }
        if c == b'/' && bytes.get(at + 1) == Some(&b'/') {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
            gap = true;
            continue;
        }
        if c == b'/' && bytes.get(at + 1) == Some(&b'*') {
            at = block_comment_end(bytes, at);
            gap = true;
            continue;
        }
        if lead && c == b'#' {
            if let Some(end) = attribute_end(bytes, at) {
                at = end;
                gap = true;
                continue;
            }
        }
        if gap && !lead {
            let _ = out.write_char(' ');
        }
        gap = false;
        lead = false;
        let start = at;
        if c == b'"' {
            at = string_end(bytes, at);
        } else {
            at += 1;
            while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'/' && bytes[at] != b'"' {
                at += 1;
            }
        }
        let _ = out.write_str(&header[start..at]);
    }
    let _ = out.write_char('>');
}

/// The byte after the string literal whose opening `"` is at `at` (its escapes skipped); the
/// text's end when it is not closed.
fn string_end(bytes: &[u8], at: usize) -> usize {
    let mut i = at + 1;
    while i < bytes.len() && bytes[i] != b'"' {
        i += if bytes[i] == b'\\' { 2 } else { 1 };
    }
    (i + 1).min(bytes.len())
}

/// The byte after the block comment opened at `at` (`/*`), nested ones counted as Rust nests
/// them; the text's end when it is not closed.
fn block_comment_end(bytes: &[u8], at: usize) -> usize {
    let mut i = at + 2;
    let mut depth = 1usize;
    while i < bytes.len() && depth > 0 {
        match (bytes[i], bytes.get(i + 1)) {
            (b'/', Some(b'*')) => {
                depth += 1;
                i += 2;
            }
            (b'*', Some(b'/')) => {
                depth -= 1;
                i += 2;
            }
            _ => i += 1,
        }
    }
    i
}

/// The byte after the attribute whose `#` is at `at` (`#[..]` or `#![..]`, spaced as an
/// expansion prints it: `# [doc = ".."]`), its brackets balanced outside string literals;
/// `None`: no attribute starts there.
fn attribute_end(bytes: &[u8], at: usize) -> Option<usize> {
    let mut i = at + 1;
    let skip_space = |mut i: usize| {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    i = skip_space(i);
    if bytes.get(i) == Some(&b'!') {
        i = skip_space(i + 1);
    }
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i = string_end(bytes, i);
                continue;
            }
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// What a want asks for, by name: `the head of a::B (key ..)`.
fn want_shown<'n, S: NamesSource<'n>>(names: S, w: Want) -> String {
    let (what, k) = match w {
        Want::Head(k) => ("the head of", k),
        Want::Adt(k) => ("the ADT head of", k),
        Want::Impl(k) => ("the header of", k),
        Want::Fn(k) => ("the signature of", k),
        Want::Members(k) => ("the members' predicates and signatures of", k),
        Want::MemberFn(k, f) => return format!("the signature of member fn (key {:032x}) of {}", f.0, ItemShown::of_lib(names, k)),
    };
    format!("{what} {}", ItemShown::of_lib(names, k))
}

/// What stopped a body's check ([`CheckFail`](super::check::CheckFail)), as a phrase a refusal
/// note groups items by (rule 19: `blaze check` said "check stopped" and named no reason,
/// port27/mini/clap-root 2026-11-17): the variant and its error's class, each item it is about
/// by its PATH (never a key or a range, so one cause of many bodies is one line); an item the
/// names source holds no path for says so. The whole refusal, keys and all, stays in the
/// finding's detail.
pub fn check_fail_class<'n, S: NamesSource<'n>>(names: S, keys: &Keys, e: &super::check::CheckFail) -> String {
    use super::check::CheckFail;
    let path = |k: ItemKey| -> String {
        match ItemShown::of(names, keys, k).path() {
            Some(p) => format!("`{p}`"),
            None => String::from("an item the names source holds no path for"),
        }
    };
    let lib = |k: LibKey| -> String {
        match ItemShown::of_lib(names, k).path() {
            Some(p) => format!("`{p}`"),
            None => String::from("an item the names source holds no path for"),
        }
    };
    match e {
        CheckFail::Gap { gaps, .. } => match gaps.first() {
            Some(first @ (super::world_items::Gap::Hidden(_) | super::world_items::Gap::HiddenRow { .. } | super::world_items::Gap::CoroutineRow { .. })) => first.to_string(),
            Some(first) => format!("a question the world cannot answer yet ({})", debug_class(&format!("{first:?}"), 2)),
            None => String::from("a question the world cannot answer yet"),
        },
        CheckFail::NothingToFill { questions, .. } => match questions.first() {
            Some(q) => format!("the checker needs a record the world noted nothing to make for ({})", debug_class(&format!("{q:?}"), 1)),
            None => String::from("the checker needs a record the world noted nothing to make for"),
        },
        CheckFail::Head { error, .. } => format!("a head the store could not make: {}", head_error_shown(names, error)),
        CheckFail::Buckets { error, .. } => match error {
            super::buckets::Unfilled::Impl(k) => format!("the candidate impl {} has no header in the store when its question is answered", lib(*k)),
            super::buckets::Unfilled::Fn(k) => format!("the inherent impl fn {} has no `self` flag in its impl's head", path(*k)),
            super::buckets::Unfilled::Sight(why) => format!("{why}"),
        },
        CheckFail::Tables { error, .. } => format!("the impl tables could not file a question's impls: {error}"),
        // By path, never the key (one cause of many bodies is one line).
        CheckFail::ImplQuestion { error, .. } => match error {
            super::buckets::ImplQuestionError::NoLibraryKey { of, key } => format!("{of}, {}, has no library key, so no impl of it can be asked for", path(*key)),
            other => format!("an impl question could not be asked: {other}"),
        },
        CheckFail::Check { error, .. } => format!("the checker stopped: {}", check_error_class(&path, error)),
        CheckFail::Diags { .. } => String::from("the check found errors"),
        // The first hole of the last round, its item by path.
        CheckFail::Rounds { last, .. } => match last.first() {
            Some(q) => match super::body_reads::question_item(q) {
                Some(k) => format!("still asking after {} rounds ({} of {})", super::check::MOST_ROUNDS, debug_class(&format!("{q:?}"), 1), path(k)),
                None => format!("still asking after {} rounds ({})", super::check::MOST_ROUNDS, debug_class(&format!("{q:?}"), 1)),
            },
            None => format!("still asking after {} rounds", super::check::MOST_ROUNDS),
        },
        CheckFail::Const { shown, error, .. } => format!("the constant `{shown}` an array length names has no value ({})", debug_class(&format!("{error:?}"), 2)),
        CheckFail::Resolve { item, error, .. } => {
            let why = match error {
                blaze_typeck::ResolveError::Missing(q) => format!("the world has no answer to {}", debug_class(&format!("{q:?}"), 1)),
                blaze_typeck::ResolveError::NoImpl { trait_, .. } => format!("no impl of {} holds", path(*trait_)),
                blaze_typeck::ResolveError::Overlap { trait_, .. } => format!("two impls of {} hold", path(*trait_)),
                blaze_typeck::ResolveError::NotGround { .. } => String::from("an argument is not ground"),
                blaze_typeck::ResolveError::Check(c) => check_error_class(&path, c),
            };
            format!("instance resolution of the call of {} failed: {why}", path(*item))
        }
        CheckFail::ReadsDiffer { .. } => String::from("checked again, the body read other answers than its first check (rule 21)"),
        CheckFail::InlineFor { why, .. } => (*why).into(),
    }
}

/// [`blaze_typeck::CheckError`]'s class: its variant and the inner variant, items by `path`.
fn check_error_class(path: &dyn Fn(ItemKey) -> String, e: &blaze_typeck::CheckError) -> String {
    use blaze_typeck::CheckError as E;
    match e {
        E::Limit(l) => format!("a bound of the checker passed ({l:?})"),
        E::Malformed(blaze_typeck::Malformed::CoroutineFactsMissing { reader, at, owner, ordinal }) => {
            let mut text = path(*reader);
            fmt::Write::write_fmt(&mut text, format_args!(
                " at item bytes {}..{} needs the checked output and stored types of coroutine #{ordinal} of {}: the auto trait of another fn's future needs its witness record",
                at.start, at.end, path(*owner)
            )).expect("writing to a String is infallible");
            text
        }
        E::Malformed(blaze_typeck::Malformed::BodyTypeInRecord(blaze_typeck::Question::Coroutine(owner, ordinal))) => {
            let mut text = path(*owner);
            fmt::Write::write_fmt(&mut text, format_args!(
                ", coroutine #{ordinal}: the importer cannot reconstruct its output and stored types from the defining body's record"
            )).expect("writing to a String is infallible");
            text
        }
        E::Malformed(blaze_typeck::Malformed::FutureIdentity { owner, block, at }) => {
            let mut text = path(*owner);
            fmt::Write::write_fmt(&mut text, format_args!(
                " at item bytes {}..{} has no coroutine ordinal for future expression {}", at.start, at.end, block.0
            )).expect("writing to a String is infallible");
            text
        }
        E::Malformed(blaze_typeck::Malformed::NestedFutureIdentity { owner, ordinal, block, reader }) => {
            let mut text = path(*owner);
            fmt::Write::write_fmt(&mut text, format_args!(
                ", coroutine #{ordinal}: nested future expression {} needs its owner-qualified coroutine ordinal", block.0
            )).expect("writing to a String is infallible");
            if let Some((item, range)) = reader {
                fmt::Write::write_fmt(&mut text, format_args!(
                    " at item bytes {}..{} of {}", range.start, range.end, path(*item)
                )).expect("writing to a String is infallible");
            }
            text
        }
        E::Malformed(blaze_typeck::Malformed::FutureOutsideOwner { .. } | blaze_typeck::Malformed::CoroutineSubstitution { .. } | blaze_typeck::Malformed::BodyRecordCycle { .. }) => e.to_string(),
        E::Malformed(m) => format!("malformed input ({})", debug_class(&format!("{m:?}"), 1)),
        // Rule 19: the trait whose members the facts do not hold, by its path (the item is a
        // solver key: `assoc_def(trait, name)`, no item of the names).
        E::Solve(blaze_solve::SolveError::Unsupported(blaze_solve::error::Unsupported::MemberNotFiled { trait_, .. })) => format!(
            "the trait solver needs the members of {} (an associated item of it is not filed: its bounds and default are not known)",
            path(ItemKey(trait_.0))
        ),
        E::Solve(s) => format!("the trait solver ({})", debug_class(&format!("{s:?}"), 2)),
        E::Unported(what) => format!("{what} is not checked yet"),
        E::FutureWitness { .. } => e.to_string(),
        // Rule 19: the fn whose future's record is missing, by its path.
        E::ForeignWitness { gap, item, ordinal: Some(ordinal), at } => format!(
            "{gap}: coroutine #{ordinal} of {} has only its check's superset interior (its defining body's flow was refused) at item bytes {}..{}",
            path(*item), at.start, at.end
        ),
        E::ForeignWitness { gap, item, ordinal: None, at } => format!(
            "{gap}: the hidden type of {} (another fn's future) has no record here at item bytes {}..{}",
            path(*item), at.start, at.end
        ),
        // Rule 19: the root trait and the undecided goal under it, each by its path.
        E::FutureWitnessGoal { trait_, undecided, .. } => {
            let mut text = path(*trait_) + ": " + &e.to_string();
            if let Some(blaze_typeck::UndecidedLeaf { trait_: Some(under), .. }) = undecided {
                text = text + " (the undecided goal's trait: " + &path(*under) + ")";
            }
            text
        }
        E::DynUndecided { trait_, .. } => format!("the dyn compatibility of {} is not decided", path(*trait_)),
        E::UnknownInCleanBody { .. } => String::from("a body with no diagnostic recorded an unknown type (the checker's own invariant)"),
        E::DiagWithoutPlace { code, .. } => format!("diagnostic {} has an empty range (the checker's own invariant)", code.code()),
        E::ImplTraitArity { trait_, .. } => format!("an impl header of {} has the wrong number of trait arguments", path(*trait_)),
        E::OpaqueHiddenUnknown { opaque, trait_ } => {
            let mut text = path(*trait_);
            fmt::Write::write_fmt(&mut text, format_args!(" requires the defining body's hidden type for opaque {:032x}", opaque.0))
                .expect("writing to a String is infallible");
            text
        }
        E::OpaqueAutoUndecided { opaque, trait_, at, constituent } => {
            let mut text = path(*trait_);
            fmt::Write::write_fmt(&mut text, format_args!(
                " remains undecided for the supplied hidden type of opaque {:032x} at item bytes {}..{}", opaque.0, at.start, at.end
            )).expect("writing to a String is infallible");
            if let Some((part, needed)) = constituent {
                fmt::Write::write_fmt(&mut text, format_args!("; the stored type from {} requires {}", path(*part), path(*needed)))
                    .expect("writing to a String is infallible");
            }
            text
        }
        E::TraitDefaultsNotKnown { trait_ } => format!("the defaults of {} were not read", path(*trait_)),
    }
}

/// The leading variant names of a `Debug` text, `depth` levels into tuple variants:
/// `Malformed(Expr(ExprId(3)))` at depth 2 is `Malformed Expr`; a struct variant stops at its
/// name. No key, number or range survives, so one class of many bodies is one phrase.
fn debug_class(d: &str, depth: usize) -> String {
    let mut out = String::new();
    let mut rest = d;
    for _ in 0..depth {
        let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
        let name = &rest[..end];
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(name);
        match rest[end..].strip_prefix('(') {
            Some(inner) => rest = inner,
            None => break,
        }
    }
    out
}

/// [`HeadError`]'s text with each key shown by its path ([`ItemShown`]): what its `Display`
/// says, which holds no names and so prints keys. An [`HeadError::Item`] reads
/// `item a::B (key ..): <why>`, so a census that cuts at the first `"): "` keeps one class per
/// reason.
pub fn head_error_shown<'n, S: NamesSource<'n>>(names: S, e: &HeadError) -> String {
    let item = |k: LibKey| ItemShown::of_lib(names, k);
    match e {
        HeadError::NoText(k) => format!("the names library holds no text for item {}", item(*k)),
        HeadError::Cfg(k) => format!("item {} asks for cfg atoms its crate's set does not answer", item(*k)),
        HeadError::NotAHead(k, what) => format!("item {} is {what}: no head is made for it under that want", item(*k)),
        HeadError::Cycle(ks) => {
            let mut out = String::from("these items need each other's heads in a cycle: [");
            for (i, k) in ks.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&item(*k).to_string());
            }
            out.push(']');
            out
        }
        HeadError::NeedMade { need, by } => format!(
            "{} needs {}, which is made, yet asks for it again (its lookup reads what that want does not make)",
            want_shown(names, *by),
            want_shown(names, *need)
        ),
        HeadError::Key(c) => format!("{c}"),
        HeadError::Item(k, why) => format!("item {}: {why:?}", item(*k)),
        HeadError::Seed { need, by } => format!(
            "{} needs {}, whose frozen head could not be filed (the seed holds why)",
            want_shown(names, *by),
            want_shown(names, *need)
        ),
        HeadError::Filed { want, at, what } => format!("the frozen head of {}, head {at:?}: {what}", want_shown(names, *want)),
    }
}

#[cfg(test)]
mod tests {
    use super::{debug_class, impl_shown};

    /// A check refusal's class keeps variant names only: no key, number or struct field, so
    /// two bodies stopped by one class group under one note.
    #[test]
    fn debug_class_keeps_variants_only() {
        assert_eq!(debug_class("Malformed(Expr(ExprId(3)))", 2), "Malformed Expr");
        assert_eq!(debug_class("NoRecord(FnSig(ItemKey(12)))", 1), "NoRecord");
        assert_eq!(debug_class("Assoc { trait_: ItemKey(1) }", 2), "Assoc");
        assert_eq!(debug_class("Solve(Unported(\"x\"))", 3), "Solve Unported");
    }

    /// core's RawVec impl as its header span holds it (a comment line before `const impl`), an
    /// impl with a doc comment, an attribute whose string holds `]`, nested block comments and
    /// a where clause over three lines, and an expansion's spaced `# [doc = "//"]`: each is
    /// shown from its first modifier or `impl`, on one line, an attribute inside the generics
    /// kept. Prints nothing; before the fix the first was shown as `<// FIXME(fee1-dead): ..`.
    #[test]
    fn an_impl_is_shown_from_its_first_token_on_one_line() {
        let raw_vec = "// FIXME(fee1-dead): temporary measure before rustfmt is bumped\nconst impl<T, A: [const] Allocator + [const] Destruct> RawVec<T, A>";
        assert_eq!(impl_shown(raw_vec), "<const impl<T, A: [const] Allocator + [const] Destruct> RawVec<T, A>>");
        let boxed = "/// docs\n#[rustc_const_unstable(feature = \"a]b\")] /* x /* y */ */ unsafe impl<#[may_dangle] T> Drop for Box<T>\nwhere\n    T: Sized, // why\n";
        assert_eq!(impl_shown(boxed), "<unsafe impl<#[may_dangle] T> Drop for Box<T> where T: Sized,>");
        assert_eq!(impl_shown("# [doc = \"//\"] impl Default for Stat"), "<impl Default for Stat>");
        assert_eq!(impl_shown("impl Default for stat"), "<impl Default for stat>");
    }

    /// The module paths of tempo's t1855d refusals: a body's block module and the derive's
    /// `const _` block module (`serde_sealed_or_recovered_block::__blaze_block_0::
    /// __blaze_block_0`), and rand's four statement-call blocks under a body's block. Each is
    /// shown without its block modules. TWIN: a module whose name only begins like one
    /// (`__blaze_blocks`, no `_` after `block`) and an ordinary path are kept whole. CONTROL:
    /// the names the library writes are block modules by the one test (`is_block_module`), so
    /// the filter is what removed them.
    #[test]
    fn a_block_module_is_not_shown_in_a_path() {
        use super::shown_segments;
        let shown = |p: &str| shown_segments(p).collect::<std::vec::Vec<_>>().join("::");
        let serde = "tempo_payload_types::serde_sealed_or_recovered_block::__blaze_block_0::__blaze_block_0";
        assert_eq!(shown(serde), "tempo_payload_types::serde_sealed_or_recovered_block");
        let rand = "rand::rng::__blaze_block_0::__blaze_block_x49b4c4837b78ffc5_0::__blaze_block_x49b4c4837b78ffc5_0::__blaze_block_x49b4c4837b78ffc5_0::__blaze_block_x49b4c4837b78ffc5_0";
        assert_eq!(shown(rand), "rand::rng");
        assert_eq!(shown("a::__blaze_blocks::b"), "a::__blaze_blocks::b");
        assert_eq!(shown("tempo_primitives::transaction"), "tempo_primitives::transaction");
        assert_eq!(shown(""), "");
        assert!(blaze_scope::blocks::is_block_module("__blaze_block_0"));
        assert!(blaze_scope::blocks::is_block_module("__blaze_block_x49b4c4837b78ffc5_0"));
    }
}
