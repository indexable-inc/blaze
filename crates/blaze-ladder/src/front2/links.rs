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

use blaze_expand::AttrLiteralError;
use blaze_file_items::Outline;
use blaze_file_items::nested::{NestedEnd, NestedItem, NestedKind};
use blaze_id::{CrateItemKey, ItemKey};
use blaze_lower::{AttrMeta, Lowered, head};
use blaze_native::resolve::VERBATIM;
use blaze_own::{AVec, Alloc, BVec, Bump, Erased};
use blaze_scope::attr_values::{RowRefusal, RowValue, ValueRow, is_call, row};
use blaze_scope::resolve::{At, CrateNames, ExpansionText};
use blaze_surface::{CallShape, Edition, Item, ItemDetail, ItemIndex, Range};

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
    /// Every export of the crate with the item it names, sorted by name; one export a name
    /// (a second is [`LinksError::TwoExports`], never merged).
    pub exports: AVec<'a, CrateExport<'a>>,
}

/// One export of a crate: its symbol name (`export_name`'s value, else the identifier: the
/// Reference, [abi.no_mangle], [abi.export_name]), the item that exports it (its key, as
/// nested_exports.rs's `NestedExport::key`), and that item's identifier (for the message).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CrateExport<'a> {
    pub name: &'a str,
    pub item: ItemKey,
    pub ident: &'a str,
}

/// Which text of a crate an entry is in, as a refusal names it: a file by the path `file!()`
/// gives (package-relative, no machine path), or the expansion of the item macro call with
/// this key (an `include!` of a build script's output is one).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextOf<'a> {
    File(&'a str),
    Expansion(ItemKey),
}

/// Why a `link_name` or `export_name` value names no symbol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymbolWhy<'a> {
    /// The value is no string literal, nor a macro call that expands to one (the Reference,
    /// attributes, meta item syntax: "Expressions in meta items must macro-expand to literal
    /// expressions"): blaze-expand `attr_literal`'s refusal, which names the token or macro.
    NotAString(AttrLiteralError<'a>),
    /// The string is empty, or `\u{1}` alone: no symbol has an empty name.
    Empty,
    /// A NUL byte at byte `at` of the string's value: a symbol table's name ends at its first
    /// NUL (Mach-O `nlist` and ELF `st_name` index NUL-terminated strings; `dlsym` reads a C
    /// string), so no symbol holds one.
    Nul { at: u32 },
    /// The value is a macro call (`concat!(prefix!(), stringify!(f))`, ring's `prefixed_item!`)
    /// and the crate's names hold no expansion of it for this item and attribute
    /// (blaze-scope attr_values: the names' fill expands each at its item's place).
    NoValue,
    /// The value is a macro call whose expansion at the item was refused: the macro, why, and
    /// the last form reached (blaze-scope `ValueRefused`, or its words as the crate's items
    /// record keeps them).
    Value(RowRefusal<'a>),
    /// The value is a macro call on an item declared in a block the names library does not
    /// write as a module (blaze-scope blocks.rs: a block with no item that makes a module and
    /// no statement macro call): the crate's solve expands the values of the items its names
    /// hold only, so no row of it exists. Met on the code side by the key's `INNER` bit (a key
    /// of the body's lowering, front2 locals.rs, with no library twin: [`values_key`]) and in
    /// the link table by [`Gather::body_blocks`]. Ring 0.17.14's in-body `prefixed_extern!`
    /// (limb.rs `LIMBS_less_than`, the sha2 and aes ffi) is a statement call, so its block is
    /// written and its items are the library's: never this.
    InBody,
    /// The names source did not answer the row (over records: the crate's items record did not
    /// read; the error is in the source's log).
    Unanswered,
}

/// Why a crate's link facts could not be read: each names the crate, and the item where one
/// has a place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinksError<'a> {
    /// A malformed `#[link(..)]` (or `link_name`/`export_name`), or a form not taken; `why`
    /// names the rule.
    Attr { krate: &'a str, at: ItemIndex, why: &'static str },
    /// A `link_name` or `export_name` whose value names no symbol: the crate, the text the item
    /// is in, its entry and its path within that text, the attribute, its value as written, and
    /// why.
    Symbol { krate: &'a str, text: TextOf<'a>, at: ItemIndex, item: &'a str, attr: &'static str, written: &'a str, why: SymbolWhy<'a> },
    /// `kind = "raw-dylib"`: Windows only (Reference, items.extern.attributes.link.raw-dylib).
    RawDylib { krate: &'a str, at: ItemIndex },
    UnknownKind { krate: &'a str, kind: &'a str },
    UnknownModifier { krate: &'a str, text: &'a str },
    /// An item's `cfg` (or a `#[link(cfg(..))]`) the crate's cfg set does not answer.
    Cfg { krate: &'a str, at: ItemIndex },
    /// Two items of the crate export one symbol name (Reference, [abi.no_mangle] and
    /// [abi.export_name]: the symbol is the identifier or the given name, and
    /// [abi.no_mangle.unsafe]: an unmangled symbol "may collide with another symbol with the same
    /// name"; one crate exporting one name twice is an error). `first` and `second` in key order.
    TwoExports { krate: &'a str, first: CrateExport<'a>, second: CrateExport<'a> },
    /// An entry the names give no item key where the walk needs one (`needs`): the crate, the
    /// text, the entry, its line in that text (1-based; a body's block is named by its body's
    /// entry and path), and its path (the names of the entries it is written in and its own).
    /// Refused, never passed over: an export could not say which item it is, a macro call in
    /// an extern block could not find its expansion, a body could not say which of its blocks
    /// the names library writes. Every item, member and attribute macro's input of a crate's
    /// texts has an answer ([`scope_links`]'s `entry_of`), so this names a names bug.
    Unkeyed { krate: &'a str, text: TextOf<'a>, at: ItemIndex, line: u32, item: &'a str, needs: KeyFor<'a> },
    /// A macro call inside an extern block (its key `call`) whose expansion the names refused:
    /// the block's entries it writes are not known, so its roots are not either.
    ForeignCall { krate: &'a str, call: ItemKey },
    Alloc,
}

/// What an entry's key was needed for ([`LinksError::Unkeyed`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyFor<'a> {
    /// A macro call `mac!` inside an extern block: its expansion declares the block's entries.
    Call { mac: &'a str },
    /// A `#[no_mangle]` or `#[export_name]` fn or static: the export names its item.
    Export,
    /// A fn, const or static whose body writes a block (blaze-scope blocks.rs) and holds an
    /// extern block: which blocks are written is keyed by the body's item.
    Body,
}

/// What the names hold for one entry of a walked text ([`scope_links`]'s `entry_of`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Entry {
    /// An item or a member of the crate: its key.
    Keyed(ItemKey),
    /// The input of an attribute macro (and the D104 implicit `main`'s): the Reference,
    /// "Procedural Macros", "Attribute macros": "The item to which the attribute is applied is
    /// replaced by the zero or more items in the returned TokenStream". It is no item of the
    /// crate (blaze-scope lower.rs files no text of it); the call's expansion is walked as its
    /// own text ([`scope_links`]'s `rest`), so the input is passed over with what it holds.
    Input,
    /// The names give it no key: an entry of a text that is no text of the crate (an extern
    /// block in a body, read alone), or a test's text.
    Unkeyed,
}

impl Entry {
    fn key(self) -> Option<ItemKey> {
        match self {
            Entry::Keyed(key) => Some(key),
            Entry::Input | Entry::Unkeyed => None,
        }
    }
}

/// An entry's place in a crate's texts as a sortable value: the expansion's call (`None` for a
/// file), the file's index, the entry's index.
type Spot = (Option<ItemKey>, u32, u32);

fn spot(at: At, item: ItemIndex) -> Spot {
    match at {
        At::File(n) => (None, n, item.0),
        At::Expansion(call) => (Some(call), 0, item.0),
    }
}

/// The entries of the crate's texts an attribute macro takes as its input (blaze-scope
/// lower.rs `item`: a `CallSrc` of shape `Attr` whose place is the entry), sorted.
fn taken_inputs<'a>(c: &CrateNames<'a>, alloc: Alloc<'a>) -> Result<AVec<'a, Spot>, LinksError<'a>> {
    let mut out: AVec<'a, Spot> = AVec::new_in(alloc);
    for src in c.tables.calls.values() {
        let Some(place) = src.place.as_ref() else { continue };
        let Some(at) = place.text else { continue };
        if matches!(src.kind, CallShape::Attr) {
            out.try_push(spot(at, place.item)).map_err(|_| LinksError::Alloc)?;
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// The names' answer for entry `item` of the text at `at` (its outline `outline`): its key by
/// place (the item texts, `Tables::by_place`); else an attribute macro's input
/// ([`Entry::Input`]); else a member of a trait or an impl, which the names file by owner,
/// never by place (blaze-scope lower.rs `assoc`: `Tables::members`): one written in its
/// owner's body is the member at its entry with no writing call, one a member-position call
/// wrote (an entry at the top of that call's expansion) the member that call wrote at its
/// entry. prost-derive's `impl Message for T { fn merge_field(..) { .. } }` is such a member.
fn entry_of(c: &CrateNames<'_>, inputs: &[Spot], at: At, outline: &Outline<'_>, item: ItemIndex) -> Entry {
    if let Some(key) = c.tables.by_place.get(&(at, item)) {
        return Entry::Keyed(*key);
    }
    if inputs.binary_search(&spot(at, item)).is_ok() {
        return Entry::Input;
    }
    let (owner, written_by) = match outline.item(item).parent {
        Some(p) if matches!(outline.item(p).detail, ItemDetail::Impl | ItemDetail::Trait) => (c.tables.by_place.get(&(at, p)).copied(), None),
        Some(_) => return Entry::Unkeyed,
        None => match at {
            At::Expansion(call) => (c.tables.calls.get(&call).and_then(|src| src.owner), Some(call)),
            At::File(_) => return Entry::Unkeyed,
        },
    };
    let member = owner.and_then(|o| c.tables.members.get(&o)).and_then(|list| list.iter().find(|m| m.item == item && m.written_by == written_by));
    member.map_or(Entry::Unkeyed, |m| Entry::Keyed(m.key))
}

/// Whether `text` holds the word `extern` (not `external_id`, `my_extern`): a text without it
/// declares no extern block. Over the bytes, so a comment or a string holding the word counts
/// (the body's scan decides); `r#extern` counts too.
fn has_extern_word(text: &str) -> bool {
    let bytes = text.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
    let mut from = 0usize;
    while let Some(i) = text.get(from..).and_then(|rest| rest.find("extern")) {
        let (start, end) = (from + i, from + i + "extern".len());
        let before = start == 0 || !ident(bytes[start - 1]);
        let after = end >= bytes.len() || !ident(bytes[end]);
        if before && after {
            return true;
        }
        from = end;
    }
    false
}

/// The link facts of ONE resolved crate `c`: what its `CrateScope` job already holds (its
/// files, its cfg, its item keys by place and every expansion), so the job files the table
/// with nothing made again (native_links.rs `scope_table`, rule 77). Each file, each followed by
/// the expansions of the macro calls written in it (and theirs, in source order: [`expand_met`]);
/// then the expansions of calls no text holds as an entry (an attribute macro's, a derive's, a
/// body's), ordered by the call's key, never by the map.
pub(crate) fn scope_links<'a>(c: &CrateNames<'a>, bump: Bump<'a>) -> Result<CrateLinks<'a>, LinksError<'a>> {
    let alloc = bump.alloc();
    let mut gather = Gather::new(c.name, c.edition, c.cfg.atoms(), &c.values, bump);
    gather.crate_key = Some(c.key);
    let inputs = taken_inputs(c, alloc)?;
    // The calls whose expansion was walked, sorted: each is walked once.
    let mut walked: AVec<'a, ItemKey> = AVec::new_in(alloc);
    for (n, file) in c.files.iter().enumerate() {
        let at = At::File(n as u32);
        gather.text(TextOf::File(file.shown), file.text, &file.outline, &|item| entry_of(c, &inputs, at, &file.outline, item))?;
        gather.met.reverse();
        expand_met(c, &inputs, &mut gather, &mut walked)?;
    }
    // An extern block written by an item macro (ring's `prefixed_extern!`) declares roots too,
    // wherever the call is. Only the expansions no walked text holds as an entry start a walk
    // here (a body's block modules, an attribute macro's, a derive's): a call that is an entry
    // of a text (its outline entry keyed by the call, `Tables::by_place`) is met when that
    // text is walked, in its context. Walked on its own first, in key order, the
    // `prefixed_item!` call inside ring's `extern "C" { .. }` (in the block module of
    // `limbs_less_than_limbs`) had its `#[link_name = ..] fn` read as a module's fn, not as
    // the block's entry: no root, and the extern block's later walk passed the walked call by.
    let mut rest: AVec<'a, (ItemKey, &'a ExpansionText<'a>)> = AVec::new_in(alloc);
    for (call, expansion) in c.expansions.iter() {
        let Some(m) = expansion.made else { continue };
        let entry = c.tables.calls.get(call).and_then(|src| src.place.as_ref()).and_then(|p| Some((p.text?, p.item)));
        if entry.is_some_and(|(at, item)| c.tables.by_place.get(&(at, item)) == Some(call)) {
            continue;
        }
        rest.try_push((*call, m)).map_err(|_| LinksError::Alloc)?;
    }
    rest.sort_unstable_by_key(|it| it.0);
    for &(call, m) in rest.iter() {
        let Err(slot) = walked.binary_search(&call) else { continue };
        walked.try_insert_slice_shift(slot, &[call]).map_err(|_| LinksError::Alloc)?;
        let at = At::Expansion(call);
        gather.text(TextOf::Expansion(call), m.text, &m.outline, &|item| entry_of(c, &inputs, at, &m.outline, item))?;
        gather.met.reverse();
        expand_met(c, &inputs, &mut gather, &mut walked)?;
    }
    gather.finish()
}

/// The expansion of each macro call `gather` met and has not walked (its work list, the next
/// call last), and the calls each of those writes in turn, depth first in source order: a work
/// list, no recursion. A call met inside an extern block has its expansion's top-level entries
/// read as that block's entries (the Reference, items.extern: an extern block's items may be
/// macro invocations; ring's `prefixed_extern!` writes `extern "C" { prefixed_item! { .. } }`,
/// each `prefixed_item!` one `#[link_name = ..] fn`), never as the module's items.
fn expand_met<'a>(c: &CrateNames<'a>, inputs: &[Spot], gather: &mut Gather<'a, '_>, walked: &mut AVec<'a, ItemKey>) -> Result<(), LinksError<'a>> {
    while let Some((call, foreign)) = gather.met.pop() {
        let Some(expansion) = c.expansions.get(&call) else { continue };
        if foreign && expansion.refused.is_some() {
            return Err(LinksError::ForeignCall { krate: gather.krate, call });
        }
        let Some(m) = expansion.made else { continue };
        let Err(slot) = walked.binary_search(&call) else { continue };
        walked.try_insert_slice_shift(slot, &[call]).map_err(|_| LinksError::Alloc)?;
        let base = gather.met.len();
        let at = At::Expansion(call);
        gather.top_foreign = foreign;
        let walk = gather.text(TextOf::Expansion(call), m.text, &m.outline, &|item| entry_of(c, inputs, at, &m.outline, item));
        gather.top_foreign = false;
        walk?;
        // The calls this text wrote, first written popped first.
        gather.met[base..].reverse();
    }
    Ok(())
}

/// The facts of one text `text` as the only text of crate `krate`, under cfg `atoms` (natives.rs's
/// tests, which have no `Names`).
#[cfg(test)]
pub(crate) fn text_links<'a>(
    krate: &'a str,
    text: &'a str,
    atoms: &[(&str, Option<&str>)],
    bump: Bump<'a>,
) -> Result<CrateLinks<'a>, LinksError<'a>> {
    let outline = blaze_file_items::outline(text, Edition::E2021, bump.alloc());
    let mut gather = Gather::new(krate, Edition::E2021, atoms, &[], bump);
    gather.text(TextOf::File("src/lib.rs"), text, &outline, &|at| Entry::Keyed(exports_tests::index_key(at)))?;
    gather.finish()
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
    /// The crate's `link_name`/`export_name` values written as macro calls, each expanded at
    /// its item by the names' fill (`CrateNames::values`): read by item key, never expanded here.
    values: &'c [ValueRow<'a>],
    bump: Bump<'a>,
    /// The text being walked ([`Gather::text`]'s `place`), for a refusal's words.
    place: TextOf<'a>,
    /// The text being walked is the expansion of a macro call inside an extern block: its
    /// top-level entries are that block's ([`expand_met`]).
    top_foreign: bool,
    /// The macro calls met in the texts walked and not yet expanded, by key, with whether each
    /// is inside an extern block: [`expand_met`]'s work list.
    met: AVec<'a, (ItemKey, bool)>,
    /// The crate's key, under which the modules of a body's written blocks are keyed
    /// ([`Gather::body_blocks`] asks which blocks are written); `None` reading one text alone
    /// ([`text_links`]): a body that writes a block and declares an extern block is then
    /// refused as unkeyed.
    crate_key: Option<CrateItemKey>,
    /// While an extern block declared in a body is read ([`Gather::body_blocks`]): the entry
    /// of the walked text whose body declares it, and its path. A refusal names that entry (the
    /// block's own entries are indexes of a text that is no text of the crate).
    body_of: Option<Outer<'a>>,
    /// The walked text of the crate (a file or an expansion, never a body's block read alone):
    /// what a refusal's line is counted in.
    whole: &'a str,
    out: CrateLinks<'a>,
}

/// The entry whose body declares the extern block being read, and its path in its text.
#[derive(Clone, Copy)]
struct Outer<'a> {
    at: ItemIndex,
    path: &'a str,
}

/// `r` of `text`; `None` past its end or across a character.
fn cut(text: &str, r: Range) -> Option<&str> {
    text.get(r.start as usize..r.end as usize)
}

/// A plain string literal's contents (no escape, no raw form): borrowed as written, the value
/// itself. Anything else is read by [`symbol_value`] or [`list_value`].
fn plain_str(text: &str) -> Option<&str> {
    let inner = text.trim().strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('\\') && !inner.contains('"')).then_some(inner)
}

/// The symbol a `link_name` or `export_name` value written as `written` names (the Reference,
/// items.extern.attributes.link_name and abi.export_name: "uses the MetaNameValueStr syntax";
/// attributes, meta item syntax: "Expressions in meta items must macro-expand to literal
/// expressions"): the string's value, its escapes applied (`"\u{1}_f"`, `"a\x41"`), a raw
/// string as written, a `concat!`/`stringify!` of literals expanded. blaze-expand's
/// `attr_literal` is the one reader of an attribute's value (rule 30); a plain literal is
/// borrowed and never copied (rule 11).
///
/// A leading `\u{1}` is KEPT: it is LLVM's mark, which rustc passes through and bindgen writes
/// on every name it must not have mangled (`#[link_name = "\u{1}_proc_listpids"]`): the rest is
/// the exact symbol, with no platform prefix ([`VERBATIM`], blaze-native `resolve::mangle`).
/// Refused: an empty name (or the mark alone) and a NUL byte.
pub(crate) fn symbol_value<'a>(written: &'a str, edition: Edition, bump: Bump<'a>) -> Result<&'a str, SymbolWhy<'a>> {
    let value = match plain_str(written) {
        Some(plain) => plain,
        None => blaze_expand::attr_literal(written, edition, bump).map_err(SymbolWhy::NotAString)?,
    };
    checked(value)
}

/// The symbol of a `link_name` or `export_name` value `written` on the item keyed `key`: a
/// value written as a macro call (`concat!(prefix!(), stringify!(f))`) is the row `row` gives
/// for the item (blaze-scope attr_values: expanded once, at the item, where its macros are in
/// scope, by the crate's solve), never expanded here; any other value is [`symbol_value`]'s.
/// An item with no key, or no row, has no value ([`SymbolWhy::NoValue`]); an item declared in
/// a body has none either ([`SymbolWhy::InBody`]).
///
/// The one reader for the link table (`row` reads `CrateNames::values` at the solve) and every
/// code-side reader of these attributes (`row` is [`NamesSource::attr_value`]: the same rows,
/// over the solved names or the crate's items record): the two sides name one symbol.
///
/// [`NamesSource::attr_value`]: super::source::NamesSource::attr_value
pub(crate) fn symbol_of<'a>(
    written: &'a str,
    key: Option<ItemKey>,
    row: impl FnOnce(ItemKey) -> Option<Option<RowValue<'a>>>,
    edition: Edition,
    bump: Bump<'a>,
) -> Result<&'a str, SymbolWhy<'a>> {
    if !is_call(written) {
        return symbol_value(written, edition, bump);
    }
    let Some(key) = key else { return Err(SymbolWhy::NoValue) };
    if key.is_inner() {
        return Err(SymbolWhy::InBody);
    }
    match row(key) {
        None => Err(SymbolWhy::Unanswered),
        Some(None) => Err(SymbolWhy::NoValue),
        Some(Some(Ok(value))) => checked(value),
        Some(Some(Err(refused))) => Err(SymbolWhy::Value(refused)),
    }
}

/// Like [`symbol_of`], for callers that only need the resolved symbol transiently and do not hold
/// a [`Bump`] arena: plain string literals and macro-call rows allocate nothing; escaped/raw
/// literals or literal-only built-ins use a temporary frame that is reclaimed on return.
pub(crate) fn with_symbol_of<'a, R>(
    written: &'a str,
    key: Option<ItemKey>,
    row: impl FnOnce(ItemKey) -> Option<Option<RowValue<'a>>>,
    edition: Edition,
    f: impl FnOnce(Result<&str, SymbolWhy<'_>>) -> R,
) -> R {
    if is_call(written) {
        let res = match key {
            None => Err(SymbolWhy::NoValue),
            Some(key) if key.is_inner() => Err(SymbolWhy::InBody),
            Some(key) => match row(key) {
                None => Err(SymbolWhy::Unanswered),
                Some(None) => Err(SymbolWhy::NoValue),
                Some(Some(Ok(value))) => checked(value),
                Some(Some(Err(refused))) => Err(SymbolWhy::Value(refused)),
            },
        };
        return f(res);
    }
    if let Some(plain) = plain_str(written) {
        return f(checked(plain));
    }
    let Ok(frame) = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer) else {
        return f(Err(SymbolWhy::Unanswered));
    };
    let erased = Erased::new(&frame);
    let res = symbol_value(written, edition, erased.bump());
    f(res)
}

/// The ABI strings of the pinned nightly whose `extern` block declares NO symbol an image imports:
/// "llvm-intrinsic" alone. Its entries name LLVM intrinsics, which the code builder emits by their
/// `link_name` (program/tabled.rs; foreign_head.rs `BlockAbi::LlvmIntrinsic`); no linker resolves
/// one, so none is a row of [`CrateLinks::foreign`]. "rust-intrinsic" and "platform-intrinsic" are
/// no ABI of the pinned nightly (both removed from rustc; neither is in its library or in the
/// checker's ABI table: an unknown string, refused by name by foreign_head.rs, E0703). Every other
/// ABI ("C", "system", "Rust", "rust-call", the `-unwind` twins, "aapcs", "sysv64", ..) declares
/// symbols the image imports: ring's and libproc's blocks are "C" and stay as they are.
const NO_SYMBOL_ABIS: [&str; 1] = ["llvm-intrinsic"];

/// Whether the block whose `extern "abi"` text is `written` declares no symbol an image imports
/// ([`NO_SYMBOL_ABIS`]). A bare `extern` is "C" (Reference, items.extern.abi: "If the ABI string
/// is omitted, it defaults to \"C\""): it declares symbols.
fn declares_no_symbol(written: &str) -> bool {
    let abi = written.split_once('"').and_then(|(_, rest)| rest.split_once('"')).map(|(abi, _)| abi);
    abi.is_some_and(|abi| NO_SYMBOL_ABIS.contains(&abi))
}

/// The key whose value rows the `link_name`/`export_name` of the item `lib` reads: its own; for
/// a fn or static an `extern` block in a BODY declares (a key of the body's lowering, its
/// `INNER` bit set: front2 locals.rs), the names library's twin of it, when it has one.
///
/// The names library makes a body's block a module when it holds a macro call in statement
/// position (blaze-scope blocks.rs: ring's `fn limbs_less_than_limbs(..) { prefixed_extern! {
/// fn LIMBS_less_than(..) -> ..; } .. }`), and expands the call there by the one driver: the
/// foreign fn it writes is then an item of the block's module, under the library's key, its
/// value expanded by the crate's solve at its place in the block (the same expander and the
/// same textual cut as at module level: a `macro_rules!` written earlier in the block is in
/// scope, one written after is not), its row in the crate's `values`, and its symbol in the
/// crate's link table (the block's module text and the call's expansion are among the crate's
/// expansions, which [`scope_links`] walks). The body's lowering writes the same declaration
/// under its own key; the twin is the item of the same name the block's module binds in the
/// value namespace, itself a foreign item (`extern_abi`), written in that module. `lib` itself
/// when there is none (a block the library wrote no module for: then a value written as a
/// call is [`SymbolWhy::InBody`]).
pub(crate) fn values_key<'n, S: super::source::NamesSource<'n>>(names: S, store: &super::heads::Store<'_>, lib: ItemKey) -> ItemKey {
    if !lib.is_inner() {
        return lib;
    }
    body_twin(names, store, lib).unwrap_or(lib)
}

/// [`values_key`]'s twin of the body's foreign declaration `lib`.
fn body_twin<'n, S: super::source::NamesSource<'n>>(names: S, store: &super::heads::Store<'_>, lib: ItemKey) -> Option<ItemKey> {
    let ck = super::keys::checker_of(lib);
    store.locals.foreign(ck)?;
    let h = store.locals.head(ck)?;
    let name = super::panic_entry::local_name(store, ck).ok()??;
    // Only a block's own module: the module a head in an unwritten block reads at is the
    // body's, whose item of the same name is another item.
    if !names.name_of(h.names_at).is_some_and(blaze_scope::blocks::is_block_module) {
        return None;
    }
    let segments = [name];
    let written = blaze_scope::names::Written { kind: blaze_surface::PathKind::Plain, segments: &segments };
    let found = names.walk(h.names_at, blaze_surface::Namespace::Values, written).ok()?.whole(1)?;
    let declared_here = names.text_of(found.key)?.module == h.names_at;
    (declared_here && matches!(names.extern_abi(found.key), Some(Some(_)))).then_some(found.key)
}

/// A value as a symbol: refused when empty (or the mark alone) or holding a NUL byte.
fn checked(value: &str) -> Result<&str, SymbolWhy<'_>> {
    if let Some(at) = value.bytes().position(|b| b == 0) {
        return Err(SymbolWhy::Nul { at: u32::try_from(at).unwrap_or(u32::MAX) });
    }
    let exact = value.as_bytes().first() == Some(&VERBATIM);
    if value.len() == usize::from(exact) {
        return Err(SymbolWhy::Empty);
    }
    Ok(value)
}

/// The value of one `key = ".."` entry of a `#[link(..)]` list (Reference,
/// items.extern.attributes.link: "It uses the MetaListNameValueStr syntax"): a string literal,
/// plain, escaped or raw, read as [`symbol_value`] reads one. A list entry takes a literal
/// only (no macro call: the meta item grammar's list entries hold literals), so anything that
/// does not start as a string literal is `None`.
fn list_value<'a>(written: &'a str, edition: Edition, bump: Bump<'a>) -> Option<&'a str> {
    if let Some(plain) = plain_str(written) {
        return Some(plain);
    }
    let t = written.trim();
    let literal = t.starts_with('"') || t.starts_with("r\"") || t.starts_with("r#");
    literal.then(|| blaze_expand::attr_literal(t, edition, bump).ok()).flatten()
}

impl<'a, 'c> Gather<'a, 'c> {
    fn new(krate: &'a str, edition: Edition, atoms: &'c [(&'c str, Option<&'c str>)], values: &'c [ValueRow<'a>], bump: Bump<'a>) -> Gather<'a, 'c> {
        let alloc = bump.alloc();
        Gather {
            krate,
            edition,
            atoms,
            values,
            bump,
            place: TextOf::File(""),
            top_foreign: false,
            met: AVec::new_in(alloc),
            crate_key: None,
            body_of: None,
            whole: "",
            out: CrateLinks { decls: AVec::new_in(alloc), foreign: AVec::new_in(alloc), exports: AVec::new_in(alloc) },
        }
    }

    /// The entry a refusal at `at` names: `at`, or the entry whose body declares the extern
    /// block being read ([`Gather::body_of`]).
    fn at(&self, at: ItemIndex) -> ItemIndex {
        self.body_of.map_or(at, |outer| outer.at)
    }

    /// The 1-based line of `part` (an entry's text) in the walked text of the crate.
    fn line_of(&self, part: &str, at: ItemIndex) -> Result<u32, LinksError<'a>> {
        let start = blaze_scope::expansion_origins::offset_in(self.whole, part).ok_or(self.attr_err(at, "an entry outside the text walked"))?;
        let before = self.whole.as_bytes().get(..start as usize).ok_or(self.attr_err(at, "an entry outside the text walked"))?;
        let lines = before.iter().filter(|b| **b == b'\n').count();
        Ok(u32::try_from(lines).unwrap_or(u32::MAX).saturating_add(1))
    }

    /// The refusal of entry `at` (of `text`, outline `outline`) that has no key where `needs`
    /// one: its path and line, or, in a body's extern block, its body's path.
    fn unkeyed(&self, text: &'a str, outline: &Outline<'_>, at: ItemIndex, needs: KeyFor<'a>) -> LinksError<'a> {
        let entry_text = match cut(text, outline.item(at).range) {
            Some(t) => t,
            None => return self.attr_err(at, "an outline entry outside its text"),
        };
        let path = match self.body_of {
            Some(outer) => Ok(outer.path),
            None => self.item_path(text, outline, at),
        };
        match (path, self.line_of(entry_text, at)) {
            (Ok(item), Ok(line)) => LinksError::Unkeyed { krate: self.krate, text: self.place, at: self.at(at), line, item, needs },
            (Err(e), _) | (_, Err(e)) => e,
        }
    }

    fn attr_err(&self, at: ItemIndex, why: &'static str) -> LinksError<'a> {
        LinksError::Attr { krate: self.krate, at: self.at(at), why }
    }

    /// The path of entry `at` within its text, for a refusal's words: the names of the entries
    /// it is written in (a module's, a trait's; an extern block or an impl has none and is
    /// passed over) and its own, outermost first, joined by `::`. A body's block module
    /// (`__blaze_block_N`, the names library's module text of a written block) is no name the
    /// source writes: passed over too, so an item of a block is named as written there. The
    /// parent chain ends at the text's top (an outline's parent precedes its child), so the
    /// walk is bounded by `at`.
    fn item_path(&self, text: &'a str, outline: &Outline<'_>, at: ItemIndex) -> Result<&'a str, LinksError<'a>> {
        let mut names: AVec<'a, &'a str> = AVec::new_in(self.bump.alloc());
        let mut next = Some(at);
        while let Some(i) = next {
            let item = outline.item(i);
            let name = cut(text, item.range).and_then(|t| item.name.and_then(|r| cut(t, r)));
            let block = matches!(item.detail, ItemDetail::Mod { .. }) && name.is_some_and(blaze_scope::blocks::is_block_module);
            if let Some(name) = name.filter(|_| !block) {
                names.try_push(name).map_err(|_| LinksError::Alloc)?;
            }
            next = item.parent.filter(|p| p.0 < i.0);
        }
        let mut path: BVec<'a, u8> = BVec::new_in(self.bump);
        for (n, name) in names.iter().rev().enumerate() {
            if n > 0 {
                path.try_extend_from_slice(b"::").map_err(|_| LinksError::Alloc)?;
            }
            path.try_extend_from_slice(name.as_bytes()).map_err(|_| LinksError::Alloc)?;
        }
        let path: &'a [u8] = path.into_bump_slice();
        core::str::from_utf8(path).map_err(|_| LinksError::Alloc)
    }

    /// The symbol `w` (a `link_name` or `export_name` row of entry `at`) names, or its refusal
    /// naming the crate, the text, the entry's path, the attribute and the value as written.
    /// A value written as a macro call is the names' row of the entry's item (`key`) and the
    /// attribute: expanded once, at the item, where its macros are in scope (rule 77).
    fn symbol(&self, w: Written<'a>, attr: &'static str, text: &'a str, outline: &Outline<'_>, at: ItemIndex, key: Option<ItemKey>) -> Result<&'a str, LinksError<'a>> {
        let values = self.values;
        match symbol_of(w.args, key, |key| Some(row(values, key, attr).map(|found| found.answer())), self.edition, self.bump) {
            Ok(symbol) => Ok(symbol),
            Err(why) => {
                let item = self.item_path(text, outline, at)?;
                // An item of an extern block a body declares has no key: no row was made.
                let why = match (why, self.body_of) {
                    (SymbolWhy::NoValue, Some(_)) => SymbolWhy::InBody,
                    (why, _) => why,
                };
                Err(LinksError::Symbol { krate: self.krate, text: self.place, at: self.at(at), item, attr, written: w.args, why })
            }
        }
    }

    /// The walk over one text's outline: extern blocks give their `#[link]`s and their entries'
    /// symbols; free fns, statics and members give their exports; an entry whose own `cfg` is
    /// off is passed over with everything written inside it. `place` is which text of the crate
    /// it is (a refusal names it); `key_of` gives what the names hold for an entry of this text
    /// ([`entry_of`]): an attribute macro's input is passed over with what it holds.
    fn text(&mut self, place: TextOf<'a>, text: &'a str, outline: &Outline<'_>, key_of: &dyn Fn(ItemIndex) -> Entry) -> Result<(), LinksError<'a>> {
        self.place = place;
        if self.body_of.is_none() {
            self.whole = text;
        }
        let items = outline.items();
        let mut at = 0usize;
        while at < items.len() {
            let item = items[at];
            let index = ItemIndex(at as u32);
            let item_text = cut(text, item.range).ok_or(self.attr_err(index, "an outline entry outside its text"))?;
            let in_block = match item.parent {
                Some(p) => matches!(outline.item(p).detail, ItemDetail::ExternBlock { .. }),
                None => self.top_foreign,
            };
            let entry = key_of(index);
            let off = match item.detail {
                // The call's expansion holds what it declares, walked as its own text.
                _ if entry == Entry::Input => true,
                ItemDetail::MacroCall { .. } => self.call(text, outline, &item, index, in_block, entry)?,
                // A block that declares no symbol an image imports (`extern "llvm-intrinsic"`):
                // passed over with its entries and its `#[link]`s, as a `cfg`-off block is. Core's
                // SVE predicates write their `link_name` as `concat!(..)` inside a body a macro
                // wrote; read as a symbol, that refused core's whole table (2026-11-21, t82).
                ItemDetail::ExternBlock { abi: Some(abi) } if declares_no_symbol(item.text(text, abi)) => true,
                ItemDetail::ExternBlock { .. } => self.extern_block(item_text, index)?,
                _ if in_block => self.foreign_entry(text, outline, &item, item_text, index, entry.key())?,
                ItemDetail::Fn { .. } | ItemDetail::Static { .. } => {
                    let off = self.exported(text, outline, &item, item_text, index, entry.key())?;
                    if !off {
                        self.body_blocks(text, outline, item_text, index, entry.key())?;
                    }
                    off
                }
                ItemDetail::Const { .. } => {
                    // Lowered only when its attributes can turn it off.
                    let sig = cut(item_text, item.signature).ok_or(self.attr_err(index, "a signature outside its entry"))?;
                    let off = sig.contains("cfg") && self.lower(item_text, index)?.view().cfg_out();
                    if !off {
                        self.body_blocks(text, outline, item_text, index, entry.key())?;
                    }
                    off
                }
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

    /// Sorts the symbol lists (deterministic output: the same crate gives the same lists in any
    /// walk order). Foreign names are deduplicated (two declarations of one foreign symbol are
    /// one import); exports are not: two of one name are refused, naming both items.
    fn finish(mut self) -> Result<CrateLinks<'a>, LinksError<'a>> {
        self.out.foreign.sort_unstable();
        self.out.foreign.dedup();
        self.out.exports.sort_unstable_by(|a, b| a.name.cmp(b.name).then(a.item.cmp(&b.item)));
        if let Some(pair) = self.out.exports.windows(2).find(|pair| pair[0].name == pair[1].name) {
            return Err(LinksError::TwoExports { krate: self.krate, first: pair[0], second: pair[1] });
        }
        Ok(self.out)
    }

    fn lower(&self, item_text: &'a str, at: ItemIndex) -> Result<Lowered<'a, 'a>, LinksError<'a>> {
        blaze_lower::lower_signature(item_text, self.edition, &mut CrateCfg(self.atoms), self.bump.alloc())
            .map_err(|_| LinksError::Cfg { krate: self.krate, at: self.at(at) })
    }

    /// The extern blocks declared inside the body of entry `at` (`item_text`: a fn's, const's
    /// or static's whole text; `key`: its item key) in the blocks the names library does not
    /// write as modules, and in the bodies of the fns, consts and statics declared in such
    /// blocks, at any depth: each read as an extern block of the crate, its `#[link]`s and its
    /// entries' symbols (the native image's roots are the crate's, natives.rs `native_key`).
    ///
    /// The Reference, "External blocks" ([items.extern.namespace]): an extern block may stand
    /// in a block, and its items link by name wherever it stands; "Statements", "Item
    /// declarations": the scope is the block, the symbol is not. The pinned core's
    /// `panic_fmt` declares `panic_impl` so; so do crates that call one C function from one fn.
    ///
    /// A block the library writes (blaze-scope blocks.rs: one holding an item that makes a
    /// module or a macro call in statement position, ring 0.17.14's in-body
    /// `prefixed_extern! { fn LIMBS_less_than(..) -> ..; }`, and every block around it) is
    /// copied whole, every item statement of it, into a module text among the crate's
    /// expansions, which [`scope_links`] walks with the library's keys and value rows: it is
    /// passed over here, so no block is read twice. A fn declared in a block the library does
    /// not write writes no block of its own (`NestedItem::body_writes`), so below the top
    /// body every block is read. A statement's outer `cfg` (`#[cfg(windows)] { .. }`) removes
    /// its block, as the library's module text puts it on the block's module. A text without
    /// the word `extern` ([`has_extern_word`]: prost-derive's `self.external_id` is not it)
    /// declares no extern block and is not scanned; the body's key is asked only when the body
    /// writes a block and an extern block can stand in it. A work list over texts, each a
    /// strict part of the one it is in: the walk ends.
    fn body_blocks(&mut self, text: &'a str, outline: &Outline<'_>, item_text: &'a str, at: ItemIndex, key: Option<ItemKey>) -> Result<(), LinksError<'a>> {
        if !has_extern_word(item_text) {
            return Ok(());
        }
        let alloc = self.bump.alloc();
        let mut work: AVec<'a, (&'a str, bool)> = AVec::new_in(alloc);
        work.try_push((item_text, true)).map_err(|_| LinksError::Alloc)?;
        while let Some((body, top)) = work.pop() {
            let nested = blaze_file_items::nested::scan(body, alloc);
            if nested.end != NestedEnd::Whole {
                return Err(self.attr_err(at, "a body whose brackets do not balance: the extern blocks in its blocks are not read"));
            }
            // An entry that can declare an extern block: one, or a fn, const or static whose
            // text holds the word (its own body is scanned in turn).
            let candidate = |item: &NestedItem| match item.kind {
                NestedKind::ExternBlock => true,
                NestedKind::Fn | NestedKind::Const | NestedKind::Static => cut(body, item.range).is_some_and(has_extern_word),
                _ => false,
            };
            if !nested.items.iter().any(candidate) {
                continue;
            }
            // Which blocks the names library writes: by the same scan and the same rule that
            // writes them (blaze-scope `block_modules`; `None`: not written).
            let written = match (top && nested.writes(), self.crate_key, key) {
                (false, ..) => None,
                (true, Some(krate), Some(key)) => Some(blaze_scope::blocks::block_modules(krate, key, &nested, alloc)),
                (true, ..) => return Err(self.unkeyed(text, outline, at, KeyFor::Body)),
            };
            for item in nested.items.iter() {
                if written.as_ref().is_some_and(|w| w.get(item.block as usize).copied().flatten().is_some()) {
                    continue;
                }
                if !candidate(item) {
                    continue;
                }
                let walk_in = matches!(item.kind, NestedKind::Fn | NestedKind::Const | NestedKind::Static);
                let t = cut(body, item.range).ok_or(self.attr_err(at, "a block item outside its body"))?;
                if !self.block_holds(body, &nested, item.block, at)? {
                    continue;
                }
                if walk_in {
                    // Its own `cfg` removes it with its body.
                    if !(t.contains("cfg") && self.lower(t, at)?.view().cfg_out()) {
                        work.try_push((t, false)).map_err(|_| LinksError::Alloc)?;
                    }
                    continue;
                }
                // The block read as a text of its own: its entry first, its own `cfg` and
                // `#[link]`s by `extern_block`, its fns and statics by `foreign_entry`, with no
                // keys (a value written as a macro call is `SymbolWhy::InBody`; a macro call
                // among its entries is unkeyed).
                let here = match self.body_of {
                    Some(outer) => outer,
                    None => Outer { at, path: self.item_path(text, outline, at)? },
                };
                let block = blaze_file_items::outline(t, self.edition, alloc);
                let (place, outer) = (self.place, self.body_of.replace(here));
                let walk = self.text(place, t, &block, &|_| Entry::Unkeyed);
                self.place = place;
                self.body_of = outer;
                walk?;
            }
        }
        Ok(())
    }

    /// Whether block `b` of `body` (scanned to `nested`) and every block around it are kept
    /// under the crate's cfg: a block's statement's outer attributes are read as the names
    /// library's module text writes them, on an item (blaze-scope expanded_blocks.rs `render`:
    /// on the block's `mod`), by the one signature lowering.
    fn block_holds(&self, body: &'a str, nested: &blaze_file_items::nested::Nested<'_>, b: u32, at: ItemIndex) -> Result<bool, LinksError<'a>> {
        let mut next = Some(b);
        // A chain is at most as long as the body's blocks.
        for _ in 0..nested.blocks.len() {
            let Some(here) = next else { break };
            let block = nested.blocks.get(here as usize).ok_or(self.attr_err(at, "a block the scan of its body does not hold"))?;
            let attrs = cut(body, block.attrs).ok_or(self.attr_err(at, "a block's attributes outside its body"))?;
            // Outer ones only, as the library reads them: an inner `#![..]` at the start of the
            // block around is that block's own.
            if attrs.contains("cfg") && attrs.strip_prefix('#').is_some_and(|rest| rest.trim_start().starts_with('[')) {
                let mut bytes: BVec<'a, u8> = BVec::new_in(self.bump);
                bytes.try_extend_from_slice(attrs.as_bytes()).map_err(|_| LinksError::Alloc)?;
                bytes.try_extend_from_slice(b"\nmod __blaze_block {}").map_err(|_| LinksError::Alloc)?;
                let bytes: &'a [u8] = bytes.into_bump_slice();
                let item = core::str::from_utf8(bytes).map_err(|_| LinksError::Alloc)?;
                if self.lower(item, at)?.view().cfg_out() {
                    return Ok(false);
                }
            }
            next = block.parent;
        }
        Ok(true)
    }

    /// The attribute rows of a lowered item, their texts cut from `item_text` (so they live as
    /// long as the crate's text, not the lowering).
    fn written(&self, low: &Lowered<'a, 'a>, item_text: &'a str, at: ItemIndex) -> Result<AVec<'a, Written<'a>>, LinksError<'a>> {
        let v = low.view();
        let bad = self.attr_err(at, "an attribute row the signature's lowering does not hold");
        let mut out: AVec<'a, Written<'a>> = AVec::new_in(self.bump.alloc());
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
        let mut wasm_module: Option<&'a str> = None;
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
            let value = list_value(value, self.edition, self.bump).ok_or(self.attr_err(at, "a `#[link]` value that is no string literal (Reference: MetaListNameValueStr)"))?;
            let slot = match key {
                "name" => &mut name,
                "kind" => &mut kind,
                "modifiers" => &mut modifiers,
                "wasm_import_module" => &mut wasm_module,
                "import_name_type" => return Err(self.attr_err(at, "`import_name_type` (Windows x86 only) is not taken (NATIVE.md 3.2)")),
                _ => return Err(self.attr_err(at, "an unknown key in `#[link(..)]` (Reference: name, kind, modifiers, cfg)")),
            };
            if slot.replace(value).is_some() {
                return Err(self.attr_err(at, "a key given twice in one `#[link]`"));
            }
        }
        // Reference, items.extern.attributes.link.wasm_import_module: "the wasm_import_module
        // key may be used to specify the WebAssembly module name for the items within an extern
        // block when importing symbols from the host environment"; rustc takes it on every
        // target, refuses it beside any other key (`IncompatibleWasmLink`), and links nothing
        // for it off wasm. Every platform blaze links for (blaze-native `Platform`) is native:
        // the block declares no library, its entries are still foreign names.
        if wasm_module.is_some() {
            if name.is_some() || kind.is_some() || modifiers.is_some() || cfg.is_some() {
                return Err(self.attr_err(at, "`wasm_import_module` beside another key of `#[link(..)]` (rustc: incompatible)"));
            }
            return Ok(None);
        }
        if let Some(predicate) = cfg {
            let alloc = self.bump.alloc();
            let tokens = blaze_expand::lex(predicate, self.edition, alloc)
                .map_err(|_| self.attr_err(at, "a `#[link(cfg(..))]` whose predicate does not lex"))?;
            let holds = blaze_lower::cfg_outcome(&tokens, &mut CrateCfg(self.atoms), alloc)
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
            Some("raw-dylib") => return Err(LinksError::RawDylib { krate: self.krate, at: self.at(at) }),
            Some(other) => return Err(LinksError::UnknownKind { krate: self.krate, kind: other }),
        };
        let modifiers = match modifiers {
            None => Modifiers { whole_archive: false, verbatim: false },
            Some(list) => self.modifiers(list, kind, at)?,
        };
        Ok(Some(LinkDecl { name, kind, modifiers, block: self.at(at) }))
    }

    /// The top-level comma-separated entries of a `#[link(..)]` list, each trimmed, empty ones
    /// (a trailing comma) dropped. Strings and nested groups are passed over whole.
    fn split_list(&self, inner: &'a str, at: ItemIndex) -> Result<AVec<'a, &'a str>, LinksError<'a>> {
        let mut out: AVec<'a, &'a str> = AVec::new_in(self.bump.alloc());
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

    /// A macro call written as an entry: kept for [`expand_met`] with whether it is inside an
    /// extern block. One there with no key is refused ([`LinksError::Unkeyed`], naming the
    /// macro and the block's path): its expansion, which declares the block's entries, could
    /// not be found. One elsewhere with no key is walked by [`scope_links`]'s `rest` (every
    /// expansion of the crate). Never off: a call under a false `cfg` is not expanded, so it
    /// has no expansion to walk.
    fn call(&mut self, text: &'a str, outline: &Outline<'_>, item: &Item, at: ItemIndex, foreign: bool, entry: Entry) -> Result<bool, LinksError<'a>> {
        match entry {
            Entry::Keyed(key) => self.met.try_push((key, foreign)).map_err(|_| LinksError::Alloc)?,
            Entry::Unkeyed if foreign => {
                let ItemDetail::MacroCall { path: Some(path), .. } = item.detail else {
                    return Err(self.attr_err(at, "a macro call with no path in its outline"));
                };
                let mac = item.checked_in_file(path).and_then(|r| cut(text, r)).ok_or(self.attr_err(at, "a macro call's path outside its entry"))?;
                return Err(self.unkeyed(text, outline, at, KeyFor::Call { mac: mac.trim() }));
            }
            Entry::Unkeyed | Entry::Input => {}
        }
        Ok(false)
    }

    /// An entry of an extern block: a fn's or static's symbol (its `link_name`, else its
    /// identifier) joins `foreign`; a foreign type has none. Gives whether it is off.
    fn foreign_entry(
        &mut self,
        text: &'a str,
        outline: &Outline<'_>,
        item: &Item,
        item_text: &'a str,
        at: ItemIndex,
        key: Option<ItemKey>,
    ) -> Result<bool, LinksError<'a>> {
        match item.detail {
            ItemDetail::Fn { .. } | ItemDetail::Static { .. } => {}
            ItemDetail::TypeAlias => return Ok(false),
            // Met by `text` as a call ([`Gather::call`]): its expansion's entries are read.
            ItemDetail::MacroCall { .. } => return Ok(false),
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
                let value = self.symbol(*w, "link_name", text, outline, at, key)?;
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
    fn exported(
        &mut self,
        text: &'a str,
        outline: &Outline<'_>,
        item: &Item,
        item_text: &'a str,
        at: ItemIndex,
        key: Option<ItemKey>,
    ) -> Result<bool, LinksError<'a>> {
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
                    let value = self.symbol(*w, "export_name", text, outline, at, key)?;
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
        let Some(key) = key else { return Err(self.unkeyed(text, outline, at, KeyFor::Export)) };
        let ident = self.identifier(item, item_text, at)?;
        self.out.exports.try_push(CrateExport { name: symbol, item: key, ident }).map_err(|_| LinksError::Alloc)?;
        Ok(false)
    }

    /// An item's identifier as its symbol: `r#` dropped (the raw form is the same identifier).
    fn identifier(&self, item: &Item, item_text: &'a str, at: ItemIndex) -> Result<&'a str, LinksError<'a>> {
        let name = item.name.and_then(|r| cut(item_text, r)).ok_or(self.attr_err(at, "a fn or static without a name in its outline"))?;
        Ok(name.strip_prefix("r#").unwrap_or(name))
    }
}

#[cfg(test)]
#[path = "links_exports_tests.rs"]
mod exports_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use super::exports_tests::index_key;

    /// The facts of one file `text` as the only text of crate `t`, under `atoms`.
    fn links_of<'a>(text: &'a str, atoms: &[(&str, Option<&str>)], bump: Bump<'a>) -> Result<CrateLinks<'a>, LinksError<'a>> {
        let outline = blaze_file_items::outline(text, Edition::E2021, bump.alloc());
        let mut gather = Gather::new("t", Edition::E2021, atoms, &[], bump);
        gather.text(TextOf::File("src/lib.rs"), text, &outline, &|at| Entry::Keyed(index_key(at)))?;
        gather.finish()
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
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        let text = "#[link(name = \"z\")]\n\
                    extern \"C\" { fn f(); #[link_name = \"g2\"] fn g(); }\n\
                    #[cfg_attr(target_os = \"macos\", link(name = \"CoreFoundation\", kind = \"framework\"))]\n\
                    extern \"C\" {}\n\
                    #[link(name = \"w\", cfg(windows))]\n\
                    extern \"C\" {}\n\
                    #[unsafe(no_mangle)] pub extern \"C\" fn cb() {}\n\
                    #[cfg(test)] mod t { #[no_mangle] pub extern \"C\" fn hidden() {} }\n";
        let links = links_of(text, MAC, bump).unwrap_or_else(|e| panic!("refused: {e:?}"));
        let plain = Modifiers { whole_archive: false, verbatim: false };
        assert_eq!(
            links.decls.as_slice(),
            [
                LinkDecl { name: "z", kind: LinkKind::Dylib, modifiers: plain, block: ItemIndex(0) },
                LinkDecl { name: "CoreFoundation", kind: LinkKind::Framework, modifiers: plain, block: ItemIndex(3) },
            ]
        );
        assert_eq!(links.foreign.as_slice(), ["f", "g2"]);
        assert_eq!(links.exports.iter().map(|e| e.name).collect::<std::vec::Vec<_>>(), ["cb"]);
        // Off macOS the framework's `cfg_attr` does not hold: no decl from it.
        let linux = links_of(text, &[("target_os", Some("linux")), ("unix", None)], bump)
            .unwrap_or_else(|e| panic!("refused: {e:?}"));
        assert_eq!(linux.decls.len(), 1);
    }

    /// An `extern "llvm-intrinsic"` block declares no symbol an image imports ([`NO_SYMBOL_ABIS`]):
    /// its entries are no row of `foreign`, and a `link_name` written as a call inside a body is
    /// not read (core's SVE predicates, core_arch/src/aarch64/sve/mod.rs `impl_internal_sve_predicate!`:
    /// `link_name = concat!("llvm.aarch64.sve.convert.to.svbool.nxv", $elt, "i1")` in a method body
    /// a macro writes refused core's whole table, 2026-11-21 t82, so every `blaze run` stopped).
    /// CONTROL: the same blocks as `extern "C"`: the module-level entry is a foreign symbol and the
    /// in-body call is the `InBody` refusal, as before.
    ///
    /// Prints `links: llvm-intrinsic blocks declare no symbol; as "C" the same entries are read`.
    #[test]
    fn an_llvm_intrinsic_block_declares_no_symbol() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        let intrinsic = "extern \"llvm-intrinsic\" { #[link_name = \"llvm.x.y\"] fn top(); }\n\
                         extern \"C\" { fn real(); }\n\
                         pub fn sve() { unsafe extern \"llvm-intrinsic\" { #[link_name = concat!(\"llvm.a\", \"b\")] fn inner(); } }\n";
        let links = links_of(intrinsic, MAC, bump).unwrap_or_else(|e| panic!("an llvm-intrinsic block was read for symbols: {e:?}"));
        assert_eq!(links.foreign.as_slice(), ["real"], "only the \"C\" block's entry is a foreign symbol");
        assert!(links.decls.is_empty());
        // CONTROL: as "C", the module-level entry is a symbol and the in-body call is refused.
        let top_c = "extern \"C\" { #[link_name = \"llvm.x.y\"] fn top(); }\n";
        let links = links_of(top_c, MAC, bump).unwrap_or_else(|e| panic!("refused: {e:?}"));
        assert_eq!(links.foreign.as_slice(), ["llvm.x.y"]);
        let body_c = "pub fn sve() { unsafe extern \"C\" { #[link_name = concat!(\"llvm.a\", \"b\")] fn inner(); } }\n";
        let refused = links_of(body_c, MAC, bump);
        assert!(matches!(refused, Err(LinksError::Symbol { attr: "link_name", why: SymbolWhy::InBody, .. })), "{:?}", refused.as_ref().err());
        // The ABI string, not its bytes elsewhere: a bare `extern` is "C".
        for (written, none) in [("extern \"llvm-intrinsic\"", true), ("extern \"C\"", false), ("extern", false), ("extern \"C-unwind\"", false), ("extern \"rust-intrinsic\"", false)] {
            assert_eq!(declares_no_symbol(written), none, "`{written}`");
        }
        std::println!("links: llvm-intrinsic blocks declare no symbol; as \"C\" the same entries are read");
    }

    /// Reference, items.extern.attributes.link.raw-dylib: "raw-dylib ... This is only valid for
    /// Windows targets": refused by name, never linked as a dylib. And modifiers-syntax: "each
    /// modifier prefixed with either a + or -": `+whole-archive` is read, an unknown one refused.
    ///
    /// Prints `test front2::links::tests::raw_dylib_and_modifiers ... ok`.
    #[test]
    fn raw_dylib_and_modifiers() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        let raw = "#[link(name = \"x\", kind = \"raw-dylib\")] extern \"C\" { fn x(); }\n";
        assert_eq!(links_of(raw, MAC, bump).err(), Some(LinksError::RawDylib { krate: "t", at: ItemIndex(0) }));
        let whole = "#[link(name = \"blst\", kind = \"static\", modifiers = \"+whole-archive,-bundle\")] extern \"C\" {}\n";
        let links = links_of(whole, MAC, bump).unwrap_or_else(|e| panic!("refused: {e:?}"));
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
        assert_eq!(links_of(odd, MAC, bump).err(), Some(LinksError::UnknownModifier { krate: "t", text: "+lazy" }));
    }

    /// The Reference, items.extern.attributes.link_name: "The link_name attribute uses the
    /// MetaNameValueStr syntax", its value ANY string literal (Tokens, string literals: escapes
    /// stand for their character; a raw string is as written), and attributes, meta item syntax:
    /// "Expressions in meta items must macro-expand to literal expressions" (a macro call's value
    /// is the names' row, `values_written_as_calls_are_the_names_rows`). LLVM's
    /// `\1` mark is kept in the symbol: bindgen's libproc 0.14.11 output
    /// (`#[link_name = "\u{1}__ZL9UUID_NULL"]`, docs_rs/osx_libproc_bindings.rs:6964, the text
    /// its build script writes) is the exact symbol `__ZL9UUID_NULL`.
    ///
    /// TWINS: the same block with a plain name gives the plain name (the platform's `_` is added
    /// later, blaze-native `mangle`); an integer value, a NUL byte, the mark alone and a macro
    /// call the names hold no row of are each the typed refusal naming the crate, the file, the
    /// item's path and the attribute.
    ///
    /// Prints `links: link_name read as a string literal: 5 symbols; 4 refusals named`.
    /// Without the piece: the first `\u{1}` name is refused "a `link_name` that is no plain
    /// string literal" (the run of 05:52 on tempo, libproc, ItemIndex(4936)).
    #[test]
    fn link_name_is_any_string_literal() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        let text = "extern \"C\" {\n\
                    #[link_name = \"\\u{1}__ZL9UUID_NULL\"] static UUID_NULL: [u8; 16];\n\
                    #[link_name = \"plain\"] fn b();\n\
                    #[link_name = \"\\x41b\"] fn c();\n\
                    #[link_name = r\"raw\\name\"] fn d();\n\
                    #[link_name = r#\"q\"uote\"#] fn e();\n\
                    }\n";
        let links = links_of(text, MAC, bump).unwrap_or_else(|e| panic!("refused: {e:?}"));
        assert_eq!(links.foreign.as_slice(), ["\u{1}__ZL9UUID_NULL", "Ab", "plain", "q\"uote", "raw\\name"]);

        // Each refusal names the crate, the file, the entry's path in it, the attribute and
        // the value as written.
        let refused = |value: &'static str| {
            let text: &'static str = std::boxed::Box::leak(std::format!("mod m {{ extern \"C\" {{ #[link_name = {value}] fn x(); }} }}\n").into_boxed_str());
            match links_of(text, MAC, bump) {
                Err(LinksError::Symbol { krate: "t", text: TextOf::File("src/lib.rs"), item: "m::x", attr: "link_name", written, why, .. }) => {
                    assert_eq!(written, value);
                    why
                }
                other => panic!("`{value}` was not refused by name: {:?}", other.map(|l| l.foreign.len())),
            }
        };
        assert!(matches!(refused("5"), SymbolWhy::NotAString(_)), "an integer is no string literal");
        assert_eq!(refused("\"a\\0b\""), SymbolWhy::Nul { at: 1 }, "a NUL byte is in no symbol");
        assert_eq!(refused("\"\\u{1}\""), SymbolWhy::Empty, "the mark alone names no symbol");
        // A macro call is never expanded here: with no row of the names for it, it is refused.
        assert_eq!(refused("concat!(\"con\", \"cat\")"), SymbolWhy::NoValue, "a call the names did not expand");
        // An `export_name` is read by the same rule.
        let export = "#[export_name = \"\\u{1}_exact\"] pub extern \"C\" fn g() {}\n";
        let links = links_of(export, MAC, bump).unwrap_or_else(|e| panic!("refused: {e:?}"));
        assert_eq!(links.exports.iter().map(|e| e.name).collect::<std::vec::Vec<_>>(), ["\u{1}_exact"]);
        std::println!("links: link_name read as a string literal: 5 symbols; 4 refusals named");
    }

    /// The Reference, attributes, meta item syntax: "Expressions in meta items must
    /// macro-expand to literal expressions". A `link_name` or `export_name` written as a macro
    /// call (ring's `#[link_name = concat!(prefix!(), stringify!(f))]`) is the names' row for
    /// the entry's item and attribute (blaze-scope attr_values, expanded at the item): its
    /// value is the symbol, its refusal the typed `SymbolWhy::Value` naming the macro, and a
    /// row's empty value (`concat!()`) is refused as an empty name. TWIN: the same text with
    /// the row of ANOTHER item reads no value (`NoValue`): a row is read by its item's key.
    ///
    /// Prints `links: values written as calls are the names' rows (ring_core_0_17_14__f)`.
    #[test]
    fn values_written_as_calls_are_the_names_rows() {
        use blaze_scope::attr_values::{ValueRefused, ValueWhy};
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        // Entries: 0 the block, 1 `f`; then 2 `g` outside it.
        let text = "extern \"C\" { #[link_name = concat!(prefix!(), stringify!(f))] fn f(); }\n\
                    #[export_name = concat!(prefix!(), \"g\")] pub extern \"C\" fn g() {}\n";
        let with = |rows: &'static [ValueRow<'static>]| {
            let outline = blaze_file_items::outline(text, Edition::E2021, bump.alloc());
            let mut gather = Gather::new("t", Edition::E2021, MAC, rows, bump);
            gather.text(TextOf::File("src/lib.rs"), text, &outline, &|at| Entry::Keyed(index_key(at)))?;
            gather.finish()
        };
        let f = index_key(ItemIndex(1));
        let g = index_key(ItemIndex(2));
        let mut rows = [
            ValueRow { item: f, attr: "link_name", value: Ok("ring_core_0_17_14__f"), reads: &[] },
            ValueRow { item: g, attr: "export_name", value: Ok("ring_core_0_17_14__g"), reads: &[] },
        ];
        rows.sort_by(|a, b| (a.item, a.attr).cmp(&(b.item, b.attr)));
        let rows: &'static [ValueRow<'static>] = std::boxed::Box::leak(std::boxed::Box::new(rows));
        let links = with(rows).unwrap_or_else(|e| panic!("refused: {e:?}"));
        assert_eq!(links.foreign.as_slice(), ["ring_core_0_17_14__f"]);
        assert_eq!(links.exports.iter().map(|e| (e.name, e.item)).collect::<std::vec::Vec<_>>(), [("ring_core_0_17_14__g", g)]);

        // A refused row is the link table's refusal, naming the macro.
        let why = ValueWhy::TooDeep { mac: "prefix" };
        let refusal = ValueRefused { item: f, attr: "link_name", why, form: "prefix ! ()" };
        let refused: &'static [ValueRow<'static>] = std::boxed::Box::leak(std::boxed::Box::new([
            ValueRow { item: f, attr: "link_name", value: Err(refusal), reads: &[] },
        ]));
        assert!(
            matches!(with(refused), Err(LinksError::Symbol { attr: "link_name", item: "f", why: SymbolWhy::Value(RowRefusal::Typed(ValueRefused { why: ValueWhy::TooDeep { mac: "prefix" }, .. })), .. })),
            "the row's refusal is the table's"
        );
        // `concat!()`'s empty value names no symbol.
        let empty: &'static [ValueRow<'static>] = std::boxed::Box::leak(std::boxed::Box::new([ValueRow { item: f, attr: "link_name", value: Ok(""), reads: &[] }]));
        assert!(matches!(with(empty), Err(LinksError::Symbol { why: SymbolWhy::Empty, .. })));
        // TWIN: the row of another item (`g`'s key under `link_name`) is not `f`'s.
        let other: &'static [ValueRow<'static>] = std::boxed::Box::leak(std::boxed::Box::new([ValueRow { item: g, attr: "link_name", value: Ok("ring_core_0_17_14__f"), reads: &[] }]));
        assert!(matches!(with(other), Err(LinksError::Symbol { item: "f", why: SymbolWhy::NoValue, .. })));
        std::println!("links: values written as calls are the names' rows ({})", links.foreign[0]);
    }

    /// The Reference, items.extern.attributes.link: "It uses the MetaListNameValueStr syntax":
    /// a value is any string literal, an escaped or raw one too. CONTROL: a value that is no
    /// string literal is refused by name.
    #[test]
    fn link_values_are_any_string_literal() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        let text = "#[link(name = r\"z\", kind = \"st\\x61tic\")] extern \"C\" {}\n";
        let links = links_of(text, MAC, bump).unwrap_or_else(|e| panic!("refused: {e:?}"));
        assert_eq!(links.decls.iter().map(|d| (d.name, d.kind)).collect::<std::vec::Vec<_>>(), [("z", LinkKind::Static)]);
        let bad = "#[link(name = z)] extern \"C\" {}\n";
        assert!(matches!(links_of(bad, MAC, bump), Err(LinksError::Attr { krate: "t", .. })));
    }

    /// Reference, items.extern.attributes.link.wasm_import_module: a wasm module's name, taken
    /// by rustc on every target and linked as nothing off wasm: the block declares no library
    /// and its entries are still foreign names. CONTROL: beside `name` it is refused, as rustc
    /// refuses it (`IncompatibleWasmLink`).
    #[test]
    fn wasm_import_module_links_nothing_off_wasm() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        let text = "#[link(wasm_import_module = \"wasi:cli/exit@0.2.0\")]\n\
                    extern \"C\" { #[link_name = \"exit\"] fn wit_import(_: i32); }\n";
        let links = links_of(text, MAC, bump).unwrap_or_else(|e| panic!("refused: {e:?}"));
        assert!(links.decls.is_empty(), "no library: {:?}", links.decls.as_slice());
        assert_eq!(links.foreign.as_slice(), ["exit"]);
        let both = "#[link(name = \"z\", wasm_import_module = \"m\")] extern \"C\" {}\n";
        assert!(matches!(links_of(both, MAC, bump), Err(LinksError::Attr { krate: "t", .. })));
    }

    /// The Reference, items.extern: an extern block's items may be macro invocations, expanded
    /// to the block's entries. ring 0.17.14 src/prefixed.rs: `prefixed_extern!` writes
    /// `extern "C" { prefixed_item! { link_name f { fn f(); } } }`, and `prefixed_item!` writes
    /// `#[link_name = ..] fn f();`. The call inside the block is met as a call there (not
    /// refused), and its expansion's top-level fn is read as a foreign entry: its `link_name`
    /// joins the roots. CONTROL: the same expansion walked as a module's text (a call outside
    /// any block) adds no root, and a call in a block with no key is refused by name.
    ///
    /// Prints `links: a macro call in an extern block: met in the block, its fn a root (ring_core_f)`.
    #[test]
    fn a_macro_call_in_an_extern_block_declares_its_entries() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        let file = "extern \"C\" {\n    prefixed_item! { link_name f { fn f(); } }\n}\n";
        let expansion = "#[link_name = \"ring_core_f\"] fn f();\n";
        fn walk<'a>(file: &'a str, expansion: &'a str, top_foreign: bool, bump: Bump<'a>) -> Result<CrateLinks<'a>, LinksError<'a>> {
            let outline = blaze_file_items::outline(file, Edition::E2021, bump.alloc());
            let mut gather = Gather::new("t", Edition::E2021, MAC, &[], bump);
            gather.text(TextOf::File("src/lib.rs"), file, &outline, &|at| Entry::Keyed(index_key(at)))?;
            assert_eq!(gather.met.as_slice(), [(index_key(ItemIndex(1)), true)], "the call is met inside the block");
            let made = blaze_file_items::outline(expansion, Edition::E2021, bump.alloc());
            gather.top_foreign = top_foreign;
            gather.text(TextOf::Expansion(index_key(ItemIndex(1))), expansion, &made, &|at| Entry::Keyed(index_key(at)))?;
            gather.finish()
        }
        let read = walk(file, expansion, true, bump).unwrap_or_else(|e| panic!("refused: {e:?}"));
        assert_eq!(read.foreign.as_slice(), ["ring_core_f"]);
        let as_items = walk(file, expansion, false, bump).unwrap_or_else(|e| panic!("refused: {e:?}"));
        assert!(as_items.foreign.is_empty(), "control: a module's bodiless fn is no root: {:?}", as_items.foreign.as_slice());
        let outline = blaze_file_items::outline(file, Edition::E2021, bump.alloc());
        let mut unkeyed = Gather::new("t", Edition::E2021, MAC, &[], bump);
        assert_eq!(
            unkeyed.text(TextOf::File("src/lib.rs"), file, &outline, &|_| Entry::Unkeyed).err(),
            Some(LinksError::Unkeyed { krate: "t", text: TextOf::File("src/lib.rs"), at: ItemIndex(1), line: 2, item: "", needs: KeyFor::Call { mac: "prefixed_item" } })
        );
        std::println!("links: a macro call in an extern block: met in the block, its fn a root ({})", read.foreign[0]);
    }

    /// gcloud-sdk 0.29.0 (the run of 08:40 on tempo: `Unkeyed { krate: "gcloud_sdk", at:
    /// ItemIndex(2) }`): prost-derive's `impl Message for T`, its member `merge_field` (entry 2
    /// of the derive's expansion), whose body calls a macro (so it writes a block, blaze-scope
    /// blocks.rs) and reads a field `external_id`. The names give a member no key by place
    /// (lower.rs `assoc` files it by owner), and the old rule asked the body's key whenever
    /// the text held the BYTES `extern` and the body wrote a block. Now: the body holds no word
    /// `extern`, so no key is asked and the table is empty; an unkeyed body that does declare
    /// an extern block is still refused, naming the member and why (CONTROL). The old rule's
    /// three conditions hold of this text (the CONTROL of the cause): the bytes are there, the
    /// body writes, and the walk had no key.
    ///
    /// Prints `links: gcloud's merge_field: no extern word, no key asked; with one: Unkeyed (Body)`.
    #[test]
    fn a_member_body_without_the_word_extern_asks_no_key() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        let gcloud = "impl Message for Msg {\n\
                      fn encode_raw(&self) {}\n\
                      fn merge_field(&mut self, tag: u32) -> u32 {\n\
                      let name = stringify!(Msg);\n\
                      match tag { 1 => self.external_id, _ => 0 }\n\
                      }\n\
                      }\n";
        let walk = |text: &'static str| {
            let outline = blaze_file_items::outline(text, Edition::E2021, bump.alloc());
            let mut gather = Gather::new("gcloud_sdk", Edition::E2021, MAC, &[], bump);
            gather.text(TextOf::Expansion(index_key(ItemIndex(9))), text, &outline, &|_| Entry::Unkeyed)?;
            gather.finish()
        };
        let links = walk(gcloud).unwrap_or_else(|e| panic!("gcloud's shape was refused: {e:?}"));
        assert!(links.foreign.is_empty() && links.exports.is_empty() && links.decls.is_empty(), "nothing to link");

        // The cause's CONTROL: what the old rule refused on.
        let outline = blaze_file_items::outline(gcloud, Edition::E2021, bump.alloc());
        let member = outline.item(ItemIndex(2));
        let body = cut(gcloud, member.range).unwrap_or_else(|| panic!("entry 2 is outside its text"));
        assert!(body.contains("extern") && !has_extern_word(body), "the bytes `extern` are there, the word is not");
        assert!(blaze_file_items::nested::scan(body, bump.alloc()).writes(), "the body writes a block (its `stringify!` call)");

        // CONTROL: the same member declaring an extern block needs its key, and names itself.
        let declares: &'static str = std::boxed::Box::leak(gcloud.replace("let name", "extern \"C\" { fn real(); }\nlet name").into_boxed_str());
        assert_eq!(
            walk(declares).err(),
            Some(LinksError::Unkeyed { krate: "gcloud_sdk", text: TextOf::Expansion(index_key(ItemIndex(9))), at: ItemIndex(2), line: 3, item: "merge_field", needs: KeyFor::Body })
        );

        // The word, not its bytes.
        for (text, word) in [("extern \"C\" {}", true), ("x.external_id", false), ("my_extern()", false), ("{extern}", true), ("éextern", false), ("", false)] {
            assert_eq!(has_extern_word(text), word, "`{text}`");
        }
        std::println!("links: gcloud's merge_field: no extern word, no key asked; with one: Unkeyed (Body)");
    }

    /// The Reference, "Attribute macros": "The item to which the attribute is applied is
    /// replaced by the zero or more items in the returned TokenStream". An attribute macro's
    /// input is no item of the crate: its export and the extern blocks in its body are not the
    /// crate's (the call's expansion declares what stays, walked as its own text). CONTROL: the
    /// same text with the entry keyed gives both.
    #[test]
    fn an_attribute_macro_s_input_is_passed_over() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        let text = "#[no_mangle] pub extern \"C\" fn taken() { extern \"C\" { fn inside(); } }\n\
                    #[no_mangle] pub extern \"C\" fn kept() {}\n";
        let walk = |input: bool| {
            let outline = blaze_file_items::outline(text, Edition::E2021, bump.alloc());
            let mut gather = Gather::new("t", Edition::E2021, MAC, &[], bump);
            let key_of = |at: ItemIndex| if input && at == ItemIndex(0) { Entry::Input } else { Entry::Keyed(index_key(at)) };
            gather.text(TextOf::File("src/lib.rs"), text, &outline, &key_of).unwrap_or_else(|e| panic!("refused: {e:?}"));
            gather.finish().unwrap_or_else(|e| panic!("refused: {e:?}"))
        };
        let passed = walk(true);
        assert_eq!(passed.exports.iter().map(|e| e.name).collect::<std::vec::Vec<_>>(), ["kept"]);
        assert!(passed.foreign.is_empty(), "the input's body is not read: {:?}", passed.foreign.as_slice());
        let keyed = walk(false);
        assert_eq!(keyed.exports.iter().map(|e| e.name).collect::<std::vec::Vec<_>>(), ["kept", "taken"]);
        assert_eq!(keyed.foreign.as_slice(), ["inside"]);
    }

    /// TWIN of the gcloud fix: a macro call inside an extern block with no key is still
    /// refused, now naming the macro, the block's path (`ffi`), its line and the text.
    #[test]
    fn an_unkeyed_call_in_an_extern_block_names_the_macro_and_the_block() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = Erased::new(&frame);
        let bump = erased.bump();
        let file = "mod ffi {\n    extern \"C\" {\n        prefixed_item! { link_name f { fn f(); } }\n    }\n}\n";
        let outline = blaze_file_items::outline(file, Edition::E2021, bump.alloc());
        let mut gather = Gather::new("t", Edition::E2021, MAC, &[], bump);
        assert_eq!(
            gather.text(TextOf::File("src/ffi.rs"), file, &outline, &|_| Entry::Unkeyed).err(),
            Some(LinksError::Unkeyed { krate: "t", text: TextOf::File("src/ffi.rs"), at: ItemIndex(2), line: 3, item: "ffi", needs: KeyFor::Call { mac: "prefixed_item" } })
        );
    }
}
