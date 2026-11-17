//! A crate's native-link facts (NATIVE.md 3.2, M10): its `#[link(..)]` attributes in source
//! order, the symbol of every foreign fn and static it declares (the native image's roots), and
//! the symbol of every fn and static it exports by `#[no_mangle]` or `#[export_name = ".."]`
//! (the Rust words C may call).
//!
//! Read from each item's signature lowering (blaze-lower's attribute rows, `cfg` and
//! `cfg_attr` already evaluated under the crate's cfg), as [`super::program`]'s `symbols_of`
//! reads an entry's export: no rule of its own beyond the attribute grammar. Every position is
//! an item index of its text's outline (rule 71), never a file line. Walked by a work list over
//! the outline's entries in source order (an entry before the entries written inside it): no
//! recursion.
//!
//! A new stage: it changes what no existing producer makes, so no key domain is bumped.

use blaze_file_items::Outline;
use blaze_id::ItemKey;
use blaze_lower::{AttrMeta, Lowered, head};
use blaze_own::{AVec, Alloc};
use blaze_scope::resolve::{ExpansionText, Names};
use blaze_surface::{Edition, Item, ItemDetail, ItemIndex, Range};

use super::heads::CrateCfg;

/// The kind of a `#[link]` library (Reference, items.extern.attributes.link: `dylib` is the
/// default when `kind` is not given).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkKind { Dylib, Static, Framework }

/// The modifiers that change what goes on the link line (NATIVE.md 3.2): `+whole-archive` pulls
/// every member, `+verbatim` names the file itself. `bundle` and `as-needed` are accepted and
/// change nothing in one image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Modifiers { pub whole_archive: bool, pub verbatim: bool }

/// One `#[link(..)]` that holds under the crate's cfg, on the extern block at `block` (an
/// item index of its text's outline: item-relative, rule 71).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LinkDecl<'a> { pub name: &'a str, pub kind: LinkKind, pub modifiers: Modifiers, pub block: ItemIndex }

/// What [`crate_links`] gives.
pub struct CrateLinks<'a> {
    /// Source order (file order, then item order; expansions after the files, by call key).
    pub decls: AVec<'a, LinkDecl<'a>>,
    /// `link_name` or identifier of every foreign fn/static, sorted, unique.
    pub foreign: AVec<'a, &'a str>,
    /// `no_mangle` (identifier) and `export_name` values, sorted, unique.
    pub exports: AVec<'a, &'a str>,
}

/// Why a crate's link facts could not be read: each names the crate, and the item where one
/// has a place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinksError<'a> {
    /// A malformed `#[link(..)]` (or `link_name`/`export_name`), or a form not taken; `why`
    /// names the rule.
    Attr { krate: &'a str, at: ItemIndex, why: &'static str },
    /// `kind = "raw-dylib"`: Windows only (Reference, items.extern.attributes.link.raw-dylib).
    RawDylib { krate: &'a str, at: ItemIndex },
    UnknownKind { krate: &'a str, kind: &'a str },
    UnknownModifier { krate: &'a str, text: &'a str },
    /// An item's `cfg` (or a `#[link(cfg(..))]`) the crate's cfg set does not answer.
    Cfg { krate: &'a str, at: ItemIndex },
    Alloc,
}

/// The link facts of crate `krate`: every file of the crate, then every item macro's expansion
/// (ordered by the call's key, never by the map), each walked over its outline.
pub(crate) fn crate_links<'a>(names: &Names<'a>, krate: usize, alloc: Alloc<'a>) -> Result<CrateLinks<'a>, LinksError<'a>> {
    let c = names.krate(krate);
    let mut gather = Gather::new(c.name, names.edition(krate), c.cfg.atoms(), alloc);
    for file in c.files.iter() {
        gather.text(file.text, &file.outline)?;
    }
    // An extern block written by an item macro (ring's `prefixed_extern!`) declares roots too.
    let mut made: AVec<'a, (ItemKey, &'a ExpansionText<'a>)> = AVec::new_in(alloc);
    for (call, expansion) in c.expansions.iter() {
        if let Some(m) = expansion.made {
            made.try_push((*call, m)).map_err(|_| LinksError::Alloc)?;
        }
    }
    made.sort_unstable_by_key(|it| it.0);
    for &(_, m) in made.iter() {
        gather.text(m.text, &m.outline)?;
    }
    Ok(gather.finish())
}

/// One attribute row as written: its path, its arguments (the token tree with its delimiters,
/// or the value after `=`), and its meta's form.
#[derive(Clone, Copy)]
struct Written<'a> {
    path: &'a str,
    args: &'a str,
    meta: AttrMeta,
}

/// The walk's state: one crate's answers, filled text by text.
struct Gather<'a, 'c> {
    krate: &'a str,
    edition: Edition,
    atoms: &'c [(&'c str, Option<&'c str>)],
    alloc: Alloc<'a>,
    out: CrateLinks<'a>,
}

/// `r` of `text`; `None` past its end or across a character.
fn cut(text: &str, r: Range) -> Option<&str> {
    text.get(r.start as usize..r.end as usize)
}

/// A plain string literal's contents. An escape or a raw string is refused by the caller: no
/// link name, symbol or modifier list of a real crate needs one, and none is guessed.
fn plain_str(text: &str) -> Option<&str> {
    let inner = text.trim().strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('\\') && !inner.contains('"')).then_some(inner)
}

impl<'a, 'c> Gather<'a, 'c> {
    fn new(krate: &'a str, edition: Edition, atoms: &'c [(&'c str, Option<&'c str>)], alloc: Alloc<'a>) -> Gather<'a, 'c> {
        Gather {
            krate,
            edition,
            atoms,
            alloc,
            out: CrateLinks { decls: AVec::new_in(alloc), foreign: AVec::new_in(alloc), exports: AVec::new_in(alloc) },
        }
    }

    fn attr_err(&self, at: ItemIndex, why: &'static str) -> LinksError<'a> {
        LinksError::Attr { krate: self.krate, at, why }
    }

    /// The walk over one text's outline: extern blocks give their `#[link]`s and their entries'
    /// symbols; free fns, statics and members give their exports; an entry whose own `cfg` is
    /// off is passed over with everything written inside it.
    fn text(&mut self, text: &'a str, outline: &Outline<'_>) -> Result<(), LinksError<'a>> {
        let items = outline.items();
        let mut at = 0usize;
        while at < items.len() {
            let item = items[at];
            let index = ItemIndex(at as u32);
            let item_text = cut(text, item.range).ok_or(self.attr_err(index, "an outline entry outside its text"))?;
            let in_block = item.parent.is_some_and(|p| matches!(outline.item(p).detail, ItemDetail::ExternBlock { .. }));
            let off = match item.detail {
                ItemDetail::ExternBlock { .. } => self.extern_block(item_text, index)?,
                _ if in_block => self.foreign_entry(&item, item_text, index)?,
                ItemDetail::Fn { .. } | ItemDetail::Static { .. } => self.exported(&item, item_text, index)?,
                ItemDetail::Mod { .. } | ItemDetail::Impl | ItemDetail::Trait => {
                    // A container is lowered only when its attributes can turn it off.
                    let sig = cut(item_text, item.signature).ok_or(self.attr_err(index, "a signature outside its entry"))?;
                    sig.contains("cfg") && self.lower(item_text, index)?.view().cfg_out()
                }
                _ => false,
            };
            at = if off { item.end.at().max(at + 1) } else { at + 1 };
        }
        Ok(())
    }

    /// Sorts and dedups the symbol lists (deterministic output: the same crate gives the same
    /// lists in any walk order).
    fn finish(mut self) -> CrateLinks<'a> {
        self.out.foreign.sort_unstable();
        self.out.foreign.dedup();
        self.out.exports.sort_unstable();
        self.out.exports.dedup();
        self.out
    }

    fn lower(&self, item_text: &'a str, at: ItemIndex) -> Result<Lowered<'a, 'a>, LinksError<'a>> {
        blaze_lower::lower_signature(item_text, self.edition, &mut CrateCfg(self.atoms), self.alloc)
            .map_err(|_| LinksError::Cfg { krate: self.krate, at })
    }

    /// The attribute rows of a lowered item, their texts cut from `item_text` (so they live as
    /// long as the crate's text, not the lowering).
    fn written(&self, low: &Lowered<'a, 'a>, item_text: &'a str, at: ItemIndex) -> Result<AVec<'a, Written<'a>>, LinksError<'a>> {
        let v = low.view();
        let bad = self.attr_err(at, "an attribute row the signature's lowering does not hold");
        let mut out: AVec<'a, Written<'a>> = AVec::new_in(self.alloc);
        for &n in v.rows(v.list(head::ATTRS)).map_err(|_| bad)? {
            let a = v.attr(n).map_err(|_| bad)?;
            let path = v.item_relative(a.path).and_then(|r| cut(item_text, r)).ok_or(bad)?;
            let args = v.item_relative(a.args).and_then(|r| cut(item_text, r)).ok_or(bad)?;
            out.try_push(Written { path: path.trim(), args: args.trim(), meta: a.meta }).map_err(|_| LinksError::Alloc)?;
        }
        Ok(out)
    }

    /// An extern block: off by its `cfg`, or each of its `#[link]`s that holds pushed as a
    /// decl. Gives whether it is off.
    fn extern_block(&mut self, item_text: &'a str, at: ItemIndex) -> Result<bool, LinksError<'a>> {
        let low = self.lower(item_text, at)?;
        if low.view().cfg_out() {
            return Ok(true);
        }
        let rows = self.written(&low, item_text, at)?;
        for w in rows.iter() {
            if w.path != "link" {
                continue;
            }
            if let Some(decl) = self.link_attr(*w, at)? {
                self.out.decls.try_push(decl).map_err(|_| LinksError::Alloc)?;
            }
        }
        Ok(false)
    }

    /// One `#[link(..)]` (Reference, items.extern.attributes.link: "It uses the
    /// MetaListNameValueStr syntax"): `name`, `kind`, `modifiers` and `cfg(..)` taken; `None`
    /// when its `cfg` is false.
    fn link_attr(&self, w: Written<'a>, at: ItemIndex) -> Result<Option<LinkDecl<'a>>, LinksError<'a>> {
        if w.meta != AttrMeta::Tokens {
            return Err(self.attr_err(at, "`#[link]` is a list: `#[link(name = \"..\")]` (Reference: MetaListNameValueStr)"));
        }
        let inner = w
            .args
            .strip_prefix('(')
            .and_then(|t| t.strip_suffix(')'))
            .ok_or(self.attr_err(at, "`#[link]` takes its list in parentheses"))?;
        let entries = self.split_list(inner, at)?;
        let (mut name, mut kind, mut modifiers, mut cfg): (Option<&'a str>, Option<&'a str>, Option<&'a str>, Option<&'a str>) =
            (None, None, None, None);
        for &entry in entries.iter() {
            let key_end = entry.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(entry.len());
            let (key, rest) = entry.split_at(key_end);
            let rest = rest.trim_start();
            if let Some(list) = rest.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
                if key != "cfg" {
                    return Err(self.attr_err(at, "a list in `#[link(..)]` other than `cfg(..)`"));
                }
                if cfg.replace(list).is_some() {
                    return Err(self.attr_err(at, "`cfg(..)` given twice in one `#[link]`"));
                }
                continue;
            }
            let value = rest.strip_prefix('=').ok_or(self.attr_err(at, "an entry of `#[link(..)]` that is no `key = \"..\"`"))?;
            let value = plain_str(value).ok_or(self.attr_err(at, "a `#[link]` value that is no plain string literal (escapes and raw strings are not taken)"))?;
            let slot = match key {
                "name" => &mut name,
                "kind" => &mut kind,
                "modifiers" => &mut modifiers,
                "wasm_import_module" => return Err(self.attr_err(at, "`wasm_import_module` (wasm targets only) is not taken (NATIVE.md 3.2)")),
                "import_name_type" => return Err(self.attr_err(at, "`import_name_type` (Windows x86 only) is not taken (NATIVE.md 3.2)")),
                _ => return Err(self.attr_err(at, "an unknown key in `#[link(..)]` (Reference: name, kind, modifiers, cfg)")),
            };
            if slot.replace(value).is_some() {
                return Err(self.attr_err(at, "a key given twice in one `#[link]`"));
            }
        }
        if let Some(predicate) = cfg {
            let tokens = blaze_expand::lex(predicate, self.edition, self.alloc)
                .map_err(|_| self.attr_err(at, "a `#[link(cfg(..))]` whose predicate does not lex"))?;
            let holds = blaze_lower::cfg_outcome(&tokens, &mut CrateCfg(self.atoms), self.alloc)
                .map_err(|_| LinksError::Cfg { krate: self.krate, at })?;
            if !holds {
                return Ok(None);
            }
        }
        // Reference, items.extern.attributes.link.name-requirement: "The name key must be
        // included if kind is specified"; with no kind there is nothing to link either.
        let name = name.ok_or(self.attr_err(at, "`#[link]` without `name = \"..\"`"))?;
        if name.is_empty() {
            return Err(self.attr_err(at, "`#[link(name = \"\")]`: an empty library name"));
        }
        let kind = match kind {
            None | Some("dylib") => LinkKind::Dylib,
            Some("static") => LinkKind::Static,
            Some("framework") => LinkKind::Framework,
            Some("raw-dylib") => return Err(LinksError::RawDylib { krate: self.krate, at }),
            Some(other) => return Err(LinksError::UnknownKind { krate: self.krate, kind: other }),
        };
        let modifiers = match modifiers {
            None => Modifiers { whole_archive: false, verbatim: false },
            Some(list) => self.modifiers(list, kind, at)?,
        };
        Ok(Some(LinkDecl { name, kind, modifiers, block: at }))
    }

    /// The top-level comma-separated entries of a `#[link(..)]` list, each trimmed, empty ones
    /// (a trailing comma) dropped. Strings and nested groups are passed over whole.
    fn split_list(&self, inner: &'a str, at: ItemIndex) -> Result<AVec<'a, &'a str>, LinksError<'a>> {
        let mut out: AVec<'a, &'a str> = AVec::new_in(self.alloc);
        let mut depth = 0u32;
        let mut in_str = false;
        let mut escaped = false;
        let mut start = 0usize;
        let bytes = inner.as_bytes();
        let mut i = 0usize;
        while i <= bytes.len() {
            let end = i == bytes.len();
            let b = if end { b',' } else { bytes[i] };
            if in_str && !end {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    in_str = false;
                }
                i += 1;
                continue;
            }
            match b {
                b'"' => in_str = true,
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth = depth.checked_sub(1).ok_or(self.attr_err(at, "an unbalanced group in `#[link(..)]`"))?;
                }
                b',' if depth == 0 => {
                    if end && in_str {
                        return Err(self.attr_err(at, "an unclosed string in `#[link(..)]`"));
                    }
                    let entry = inner[start..i].trim();
                    if !entry.is_empty() {
                        out.try_push(entry).map_err(|_| LinksError::Alloc)?;
                    }
                    start = i + 1;
                }
                _ => {}
            }
            i += 1;
        }
        if depth != 0 || in_str {
            return Err(self.attr_err(at, "an unbalanced group or unclosed string in `#[link(..)]`"));
        }
        Ok(out)
    }

    /// `modifiers = "+whole-archive,-bundle,.."` (Reference,
    /// items.extern.attributes.link.modifiers-syntax: "a comma-delimited string with each
    /// modifier prefixed with either a + or -"). Only whole-archive, bundle, verbatim and
    /// as-needed are taken; one named twice is refused (modifiers-multiple: "multiple identical
    /// modifiers in the same modifiers argument is not currently supported").
    fn modifiers(&self, list: &'a str, kind: LinkKind, at: ItemIndex) -> Result<Modifiers, LinksError<'a>> {
        let mut out = Modifiers { whole_archive: false, verbatim: false };
        // whole-archive, bundle, verbatim, as-needed: seen yet.
        let mut seen = [false; 4];
        for text in list.split(',') {
            let text = text.trim();
            let unknown = LinksError::UnknownModifier { krate: self.krate, text };
            let (on, name) = match text.as_bytes().first() {
                Some(b'+') => (true, &text[1..]),
                Some(b'-') => (false, &text[1..]),
                _ => return Err(unknown),
            };
            let slot = match name {
                "whole-archive" => 0,
                "bundle" => 1,
                "verbatim" => 2,
                "as-needed" => 3,
                _ => return Err(unknown),
            };
            if seen[slot] {
                return Err(self.attr_err(at, "a modifier named twice in one `modifiers` (Reference: modifiers-multiple)"));
            }
            seen[slot] = true;
            // Reference, modifiers-whole-archive.allowed-kinds and modifiers-bundle.allowed-kinds:
            // "This modifier is only compatible with the static linking kind."
            if (slot == 0 || slot == 1) && kind != LinkKind::Static {
                return Err(self.attr_err(at, "`whole-archive` or `bundle` on a kind that is not `static` (Reference: allowed-kinds)"));
            }
            match slot {
                0 => out.whole_archive = on,
                2 => out.verbatim = on,
                // bundle and as-needed change nothing in one image (NATIVE.md 3.2).
                _ => {}
            }
        }
        Ok(out)
    }

    /// An entry of an extern block: a fn's or static's symbol (its `link_name`, else its
    /// identifier) joins `foreign`; a foreign type has none. Gives whether it is off.
    fn foreign_entry(&mut self, item: &Item, item_text: &'a str, at: ItemIndex) -> Result<bool, LinksError<'a>> {
        match item.detail {
            ItemDetail::Fn { .. } | ItemDetail::Static { .. } => {}
            ItemDetail::TypeAlias => return Ok(false),
            ItemDetail::MacroCall { .. } => return Err(self.attr_err(at, "a macro call inside an extern block: its entries are not read (rule 3: refused by name)")),
            _ => return Err(self.attr_err(at, "an entry of an extern block that is no fn, static or type")),
        }
        let low = self.lower(item_text, at)?;
        if low.view().cfg_out() {
            return Ok(true);
        }
        let rows = self.written(&low, item_text, at)?;
        let mut symbol = None;
        for w in rows.iter() {
            // Reference, items.extern.attributes.link_name: "The link_name attribute uses the
            // MetaNameValueStr syntax."
            if w.path == "link_name" {
                if w.meta != AttrMeta::KeyValue {
                    return Err(self.attr_err(at, "`link_name` is `#[link_name = \"..\"]` (Reference: MetaNameValueStr)"));
                }
                let value = plain_str(w.args).ok_or(self.attr_err(at, "a `link_name` that is no plain string literal"))?;
                if symbol.replace(value).is_some() {
                    return Err(self.attr_err(at, "`link_name` given twice"));
                }
            }
        }
        let symbol = match symbol {
            Some(s) => s,
            None => self.identifier(item, item_text, at)?,
        };
        self.out.foreign.try_push(symbol).map_err(|_| LinksError::Alloc)?;
        Ok(false)
    }

    /// A fn or static outside an extern block: its export, when it has one (Reference, abi:
    /// "When the no_mangle and export_name attributes are both applied to the same item, the
    /// symbol name from export_name is used"). Lowered only when its attributes can name one or
    /// turn it off. Gives whether it is off.
    fn exported(&mut self, item: &Item, item_text: &'a str, at: ItemIndex) -> Result<bool, LinksError<'a>> {
        let sig = cut(item_text, item.signature).ok_or(self.attr_err(at, "a signature outside its entry"))?;
        if !(sig.contains("no_mangle") || sig.contains("export_name") || sig.contains("cfg")) {
            return Ok(false);
        }
        let low = self.lower(item_text, at)?;
        if low.view().cfg_out() {
            return Ok(true);
        }
        let rows = self.written(&low, item_text, at)?;
        let (mut no_mangle, mut export_name) = (false, None);
        for w in rows.iter() {
            match w.path {
                // `#[unsafe(no_mangle)]`'s row has the inner path (blaze-lower's UNSAFE_META).
                "no_mangle" => no_mangle = true,
                "export_name" => {
                    if w.meta != AttrMeta::KeyValue {
                        return Err(self.attr_err(at, "`export_name` is `#[export_name = \"..\"]` (Reference: MetaNameValueStr)"));
                    }
                    let value = plain_str(w.args).ok_or(self.attr_err(at, "an `export_name` that is no plain string literal"))?;
                    if export_name.replace(value).is_some() {
                        return Err(self.attr_err(at, "`export_name` given twice"));
                    }
                }
                _ => {}
            }
        }
        let symbol = match (export_name, no_mangle) {
            (Some(s), _) => s,
            // Reference, abi: "The symbol for the item is the item's identifier."
            (None, true) => self.identifier(item, item_text, at)?,
            (None, false) => return Ok(false),
        };
        self.out.exports.try_push(symbol).map_err(|_| LinksError::Alloc)?;
        Ok(false)
    }

    /// An item's identifier as its symbol: `r#` dropped (the raw form is the same identifier).
    fn identifier(&self, item: &Item, item_text: &'a str, at: ItemIndex) -> Result<&'a str, LinksError<'a>> {
        let name = item.name.and_then(|r| cut(item_text, r)).ok_or(self.attr_err(at, "a fn or static without a name in its outline"))?;
        Ok(name.strip_prefix("r#").unwrap_or(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The facts of one file `text` as the only text of crate `t`, under `atoms`.
    fn links_of<'a>(text: &'a str, atoms: &[(&str, Option<&str>)], alloc: Alloc<'a>) -> Result<CrateLinks<'a>, LinksError<'a>> {
        let outline = blaze_file_items::outline(text, Edition::E2021, alloc);
        let mut gather = Gather::new("t", Edition::E2021, atoms, alloc);
        gather.text(text, &outline)?;
        Ok(gather.finish())
    }

    const MAC: &[(&str, Option<&str>)] = &[("target_os", Some("macos")), ("unix", None)];

    /// Reference, items.extern.attributes.link: "The link attribute specifies the name of a
    /// native library ... The kind key is an optional value ... dylib ... This is the default
    /// if kind is not specified"; items.extern.attributes.link_name: "to specify the symbol to
    /// import for the given function or static"; abi, no_mangle: "The symbol for the item is the
    /// item's identifier"; a `cfg_attr` that holds leaves its attribute, a `cfg(test)` module
    /// outside a test build is gone with what it holds.
    ///
    /// Prints `test front2::links::tests::links_foreign_and_exports_of_a_crate ... ok`.
    /// Without the piece: "cannot find `links` in `front2`".
    #[test]
    fn links_foreign_and_exports_of_a_crate() {
        let text = "#[link(name = \"z\")]\n\
                    extern \"C\" { fn f(); #[link_name = \"g2\"] fn g(); }\n\
                    #[cfg_attr(target_os = \"macos\", link(name = \"CoreFoundation\", kind = \"framework\"))]\n\
                    extern \"C\" {}\n\
                    #[link(name = \"w\", cfg(windows))]\n\
                    extern \"C\" {}\n\
                    #[unsafe(no_mangle)] pub extern \"C\" fn cb() {}\n\
                    #[cfg(test)] mod t { #[no_mangle] pub extern \"C\" fn hidden() {} }\n";
        let links = links_of(text, MAC, Alloc::global()).unwrap_or_else(|e| panic!("refused: {e:?}"));
        let plain = Modifiers { whole_archive: false, verbatim: false };
        assert_eq!(
            links.decls.as_slice(),
            [
                LinkDecl { name: "z", kind: LinkKind::Dylib, modifiers: plain, block: ItemIndex(0) },
                LinkDecl { name: "CoreFoundation", kind: LinkKind::Framework, modifiers: plain, block: ItemIndex(3) },
            ]
        );
        assert_eq!(links.foreign.as_slice(), ["f", "g2"]);
        assert_eq!(links.exports.as_slice(), ["cb"]);
        // Off macOS the framework's `cfg_attr` does not hold: no decl from it.
        let linux = links_of(text, &[("target_os", Some("linux")), ("unix", None)], Alloc::global())
            .unwrap_or_else(|e| panic!("refused: {e:?}"));
        assert_eq!(linux.decls.len(), 1);
    }

    /// Reference, items.extern.attributes.link.raw-dylib: "raw-dylib ... This is only valid for
    /// Windows targets": refused by name, never linked as a dylib. And modifiers-syntax: "each
    /// modifier prefixed with either a + or -": `+whole-archive` is read, an unknown one refused.
    ///
    /// Prints `test front2::links::tests::raw_dylib_and_modifiers ... ok`.
    #[test]
    fn raw_dylib_and_modifiers() {
        let raw = "#[link(name = \"x\", kind = \"raw-dylib\")] extern \"C\" { fn x(); }\n";
        assert_eq!(links_of(raw, MAC, Alloc::global()).err(), Some(LinksError::RawDylib { krate: "t", at: ItemIndex(0) }));
        let whole = "#[link(name = \"blst\", kind = \"static\", modifiers = \"+whole-archive,-bundle\")] extern \"C\" {}\n";
        let links = links_of(whole, MAC, Alloc::global()).unwrap_or_else(|e| panic!("refused: {e:?}"));
        assert_eq!(
            links.decls.as_slice(),
            [LinkDecl {
                name: "blst",
                kind: LinkKind::Static,
                modifiers: Modifiers { whole_archive: true, verbatim: false },
                block: ItemIndex(0),
            }]
        );
        let odd = "#[link(name = \"q\", kind = \"static\", modifiers = \"+lazy\")] extern \"C\" {}\n";
        assert_eq!(links_of(odd, MAC, Alloc::global()).err(), Some(LinksError::UnknownModifier { krate: "t", text: "+lazy" }));
    }
}
