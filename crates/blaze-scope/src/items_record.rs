//! A crate's items as a record of their own (per-crate-entry.md, "Runner twenty-one"): the
//! second half of "a crate's full names as records", beside [`crate::names_record`]. It answers
//! what the type-checking front end asks of a crate's [`CrateNames`] that is about ITEMS, so the
//! front end opens exactly the crates a body touches and never walks every crate: an item by its
//! key (kind, module, name, where its text is), a trait's or an impl's members, the crate's
//! impls with what an impl index files them by, its lang items, its macros' definitions, its
//! modules' paths, its cfg atoms, edition and files. Written when the crate is solved; read back
//! per crate, on demand ([`RecordItems`]).
//!
//! The record holds WHERE a text is, never its bytes: a file by its path with the digest the
//! solve saw, an expansion by its call's key with the digest of its text. A reader gets the
//! text from its loader and the record hashes it once per text per reader against that digest:
//! a text that changed under the record is [`ItemsRecordError::Stale`], never the old range.
//!
//! Nothing here is resolved: an impl's trait and self type are kept as written (resolving them
//! at write would ask other crates questions the crate's read list does not hold); the reader
//! resolves each run of equal (trait path, module) once through the names records.
//!
//! The form (integers little-endian, every section 16-aligned, every table sorted, so any worker
//! in any order writes equal bytes): a 256-byte header (`BZIT`, version, then u32 fields: crate
//! flags, edition, the count of each table, the offset of each section, the strings' length, the
//! crate's name; at 160 the crate key, at 192 the root module, at 224 the proc-macro key); then
//! the rows of each table, each laid out at its row type below; then the strings; then the
//! item grain section ([`grain`]: each item row's chain and piece digests, its offset in header
//! field 37), by which an edit voids only the items it touches; then the member owners section
//! ([`owners`]: each named member's key with its owner's row, sorted by member key, its count
//! and offset in header fields 35 and 36), by which "who owns this member" is one search; then
//! the variant table ([`variants`]: each variant's key with its enum and index, header fields
//! 29 and 30), by which "which enum is this variant of" is one search; then the bucket index
//! ([`buckets`]: the impls filed by resolved (trait or inherent, self type's head), header
//! fields 31 and 32, and the impls the writer could not resolve, fields 33 and 34), by which
//! one impl question reads one bucket of the crate and no other impl's row.

pub mod buckets;
pub mod grain;
pub mod owners;
pub mod variants;

pub mod members;

pub use grain::{ItemNow, ItemPieces, ItemToday};
pub use members::Members;

use core::cell::{Cell, OnceCell, RefCell};

use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{CallShape, Edition, ImplFlags, ItemDetail, ItemIndex, ItemKind, ProcKind, RecordBytes};

use crate::package_path::{NotPackagePath, PackagePath};
use crate::resolve::{At, CrateNames};
use crate::task::{MacroBody, MacroSrc};

const MAGIC: [u8; 4] = *b"BZIT";
/// 2: the item grain section ([`grain`]); a version-1 record is refused (`NotThisRecord`) and
/// the crate is solved again. 3: the member owners section ([`owners`]), added after it; a
/// version-2 record is refused the same way. 4: the variant table ([`variants`]) and the bucket
/// index ([`buckets`]), added after them; a version-3 record is refused the same way. 5: a file
/// row's path is its [`PackagePath`] (D96), never the caller's `/<tree>/...`; a version-4
/// record is refused the same way. 6: a built-in derive is its own body kind
/// ([`BODY_BUILTIN_DERIVE`], its helpers in the strings: names-attr-4); a version-5 record is
/// refused the same way. 7: the crate's `std` (header field [`F_STD`], [`ItemsRecord::std_is`]);
/// a version-6 record is refused the same way. 8: an impl row keeps the crate each path's root
/// names when an expansion wrote `crate` for another crate's `$crate` (token-mark-2 R2; row 128
/// bytes); a version-7 record is refused the same way. 9: an item row keeps the ABI of the
/// `extern` block that declares it ([`ItemRow::foreign`], flag [`FOREIGN`]; row 80 bytes),
/// whoever wrote the item (a file or a macro call's expansion); a version-8 record is refused
/// the same way.
const VERSION: u32 = 9;
const ALIGN: usize = 16;
const HEADER: usize = 256;
const KEYS_AT: usize = 160;
const NONE: u32 = u32::MAX;
/// In an `at` field: the text is an expansion's, and the low bits are its row.
const EXPANSION: u32 = 1 << 31;

const FILE_ROW: usize = 56;
const EXPANSION_ROW: usize = 72;
const ITEM_ROW: usize = 80;
const OWNER_ROW: usize = 40;
const MEMBER_ROW: usize = 56;
const IMPL_ROW: usize = 128;
/// An impl row's roots word (+60): the trait path's root key is held (+64).
const ROOT_TRAIT: u32 = 1;
/// The self path's root key is held (+96).
const ROOT_SELF: u32 = 2;
const LANG_ROW: usize = 40;
const MACRO_ROW: usize = 72;
const MODULE_ROW: usize = 48;
const ATOM_ROW: usize = 16;

// The header's u32 fields, by index (field `i` is at byte `8 + 4 * i`).
const F_FLAGS: usize = 0;
const F_EDITION: usize = 1;
const F_COUNTS: usize = 2; // files, expansions, items, owners, members, impls, langs, macros, modules, atoms
const F_OFFSETS: usize = 12; // the same ten sections, then the strings
const F_STRINGS_LEN: usize = 23;
const F_NAME_AT: usize = 24;
const F_NAME_LEN: usize = 25;
/// Which `std` the crate's code names `std` ([`std_field`]): what an E0425 after a written
/// `std` notes (blaze-ladder front2/std_is_bstd.rs).
const F_STD: usize = 26;
const TABLES: usize = 10;
const WIDTHS: [usize; TABLES] = [FILE_ROW, EXPANSION_ROW, ITEM_ROW, OWNER_ROW, MEMBER_ROW, IMPL_ROW, LANG_ROW, MACRO_ROW, MODULE_ROW, ATOM_ROW];
const WHAT: [&str; TABLES] = ["file rows", "expansion rows", "item rows", "owner rows", "member rows", "impl rows", "lang rows", "macro rows", "module rows", "cfg atom rows"];
const T_FILES: usize = 0;
const T_EXPANSIONS: usize = 1;
const T_ITEMS: usize = 2;
const T_OWNERS: usize = 3;
const T_MEMBERS: usize = 4;
const T_IMPLS: usize = 5;
const T_LANGS: usize = 6;
const T_MACROS: usize = 7;
const T_MODULES: usize = 8;
const T_ATOMS: usize = 9;

// Crate flags (header field 0).
/// The crate is a proc-macro unit: its items are never linked into a program, so none of its
/// impls answers a program's question (the impl index skips it).
const CRATE_PROC_MACRO: u32 = 1;
/// Its root file names `rustc_attrs` or `rustc_coherence_is_core` (or it has no file): it may
/// write an inherent impl of a type it does not define.
const CRATE_INCOHERENT: u32 = 1 << 1;
/// The proc-macro key at 224 is set.
const CRATE_PROC_KEY: u32 = 1 << 2;

// Item and member flags.
const NAMED: u8 = 1;
const BODY_HOLDS_ITEMS: u8 = 1 << 1;
/// Item: a tuple or unit struct, its own constructor. Member: written by a macro call's
/// expansion (its `at` is that expansion).
const CTOR_OR_WRITTEN_BY: u8 = 1 << 2;
/// Item only: an `extern` block declares it, and the row's ABI string ([`ItemRow::foreign`])
/// is set.
const FOREIGN: u8 = 1 << 3;

// Impl marks.
const HAS_TRAIT: u8 = 1;
/// The trait path is written with no generic arguments.
const BARE: u8 = 1 << 1;
/// The header did not read ([`blaze_file_items::impl_header`] refused it).
const HEADER_UNREAD: u8 = 1 << 2;
/// The trait is not written as a plain path (`for<'a> Tr<'a>`, a macro call).
const TRAIT_UNREAD: u8 = 1 << 3;

// Module flags.
const LISTED: u32 = 1;
const MADE: u32 = 1 << 1;
const HAS_PATH: u32 = 1 << 2;

// Macro flags and body kinds.
const EXPORT: u8 = 1;
const LOCAL_INNER: u8 = 1 << 1;
const HAS_PARAMS: u8 = 1 << 2;
const BODY_RULES: u8 = 0;
const BODY_MACRO2: u8 = 1;
const BODY_BUILTIN_FN: u8 = 2;
const BODY_BUILTIN_OTHER: u8 = 3;
const BODY_PROC: u8 = 4;
const BODY_BUILTIN_DERIVE: u8 = 5;

/// The digest domain of an expansion's text (a file's is its `SourceFile::id`).
const EXPANSION_DOMAIN: &str = "blaze-scope expansion text 1";

/// The content id of an expansion's text, as an expansion row holds it.
pub fn expansion_id(text: &str) -> ContentId {
    ContentId::of(EXPANSION_DOMAIN, text.as_bytes())
}

/// Which text of a crate: what a reader's loader is asked for, and what an error names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextOf<'r> {
    /// The crate's file `index`, at `path` in the crate's package: the reader joins it onto the
    /// directory its world placed that crate's package at.
    File { index: u32, path: PackagePath<'r> },
    /// The expansion of the macro call `call`: its own record's text.
    Expansion { call: ItemKey },
}

/// Why an items record was not written or not read.
#[derive(Debug)]
pub enum ItemsRecordError {
    /// A section's size does not fit its u32 field.
    TooBig { what: &'static str },
    /// A module named by a row is not among the module rows (a writer's bug).
    NotListed { module: ItemKey },
    /// An item's entry is not in the outline of the text it is said to be in.
    NoOutline { item: ItemKey },
    /// The bytes do not start on a 16-byte boundary: map or copy them to one.
    Unaligned,
    /// Not `BZIT` and this version.
    NotThisRecord,
    /// A section, a row's string or a row's index runs past its table.
    Short { what: &'static str, at: usize },
    /// A string of the record is not UTF-8.
    Utf8 { what: &'static str, at: usize },
    /// A byte that is no value of its enum (an item kind, an edition, a self type's head).
    Byte { what: &'static str, byte: u8 },
    /// Impl flags with bits no impl row has.
    ImplFlags { key: ItemKey, bits: u32 },
    /// The record holds no place for this item's text (the solve could not place it in a file
    /// or an expansion): it cannot be read through the record.
    NoPlace { key: ItemKey },
    /// The loader has no such text: the file or the expansion's record is gone.
    Gone { krate: CrateItemKey, file: Option<u32>, call: Option<ItemKey> },
    /// The text is not the one the record saw (rule: a range is valid only in the text whose
    /// digest its row holds): the record is stale for it and no range of it is answered.
    Stale { krate: CrateItemKey, file: Option<u32>, call: Option<ItemKey>, saw: ContentId, found: ContentId },
    /// A range of the record lies outside its (verified) text, or cuts a character: a
    /// writer's bug.
    Range { krate: CrateItemKey, start: u32, len: u32 },
    /// File row `at`'s path is not a package-relative path (D96: a record holds no root).
    NotPackagePath { at: usize, why: NotPackagePath },
    /// An impl header's path in an expansion text is no slice of the text its marks are of
    /// (token-mark-2 R2): its root cannot be read, so the record is not written.
    RootMark { impl_: ItemKey, miss: crate::crate_roots::MarkMiss },
    /// An impl row's roots word holds bits no row has.
    Roots { key: ItemKey, bits: u32 },
    /// The `extern` block that declares `item` writes its ABI as no one quoted string
    /// (`extern "abi"`, or a bare `extern`): the row's ABI cannot be written, so the record is
    /// not written.
    ForeignAbi { item: ItemKey },
}

/// Header field [`F_STD`]: 0 for Rust's `std`, 1 + the edition's discriminant for bstd under a
/// blaze edition.
fn std_field(std: blaze_unit_graph::StdIs) -> u32 {
    match std {
        blaze_unit_graph::StdIs::Rust => 0,
        blaze_unit_graph::StdIs::Bstd(edition) => 1 + edition as u32,
    }
}

/// [`std_field`] read back; `None` for a value no edition of this blaze has.
fn std_of(field: u32) -> Option<blaze_unit_graph::StdIs> {
    match field {
        0 => Some(blaze_unit_graph::StdIs::Rust),
        n => edition::BlazeEdition::ALL.iter().copied().find(|e| *e as u32 + 1 == n).map(blaze_unit_graph::StdIs::Bstd),
    }
}

fn u32_of(n: usize, what: &'static str) -> Result<u32, ItemsRecordError> {
    u32::try_from(n).map_err(|_| ItemsRecordError::TooBig { what })
}

fn pad(out: &mut RecordBytes<'_>) {
    out.resize(out.len().next_multiple_of(ALIGN), 0);
}

fn put32(out: &mut RecordBytes<'_>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}

fn put16(out: &mut RecordBytes<'_>, n: u16) {
    out.extend_from_slice(&n.to_le_bytes());
}

/// The planted bugs of the record's oracle (tests/items_record.rs). Set on the test's thread.
#[cfg(test)]
pub(crate) mod plant {
    use core::cell::Cell;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Plant {
        None,
        /// The encoder leaves out the first impl row.
        DropImpl,
        /// The encoder leaves out the last member of the first owner that has one.
        ShortMembers,
        /// The reader hands out a text without comparing its digest with the row's.
        SkipDigest,
    }

    std::thread_local! {
        pub(crate) static PLANT: Cell<Plant> = const { Cell::new(Plant::None) };
    }

    pub(crate) fn is(plant: Plant) -> bool {
        PLANT.with(Cell::get) == plant
    }
}

/// The record's strings while written: each distinct string once, in first-use order (the rows
/// are written in sorted order, so the order is a function of the solve's answers).
struct Strings<'b, 's> {
    bytes: RecordBytes<'b>,
    seen: HashMap<&'s str, (u32, u32), Alloc<'b>>,
}

impl<'b, 's> Strings<'b, 's> {
    fn new(alloc: Alloc<'b>) -> Strings<'b, 's> {
        Strings { bytes: RecordBytes::new_in(alloc), seen: HashMap::new_in(alloc) }
    }

    fn put(&mut self, s: &'s str) -> Result<(u32, u32), ItemsRecordError> {
        if let Some(at) = self.seen.get(s) {
            return Ok(*at);
        }
        let at = (u32_of(self.bytes.len(), "items strings")?, u32_of(s.len(), "a string")?);
        self.bytes.extend_from_slice(s.as_bytes());
        self.seen.insert(s, at);
        Ok(at)
    }
}

/// Where `part` is in `base`, when it is a slice of it.
fn span_in(base: &str, part: &str) -> Option<(u32, u32)> {
    let (b, p) = (base.as_ptr() as usize, part.as_ptr() as usize);
    let start = p.checked_sub(b)?;
    if start.checked_add(part.len())? > base.len() {
        return None;
    }
    Some((u32::try_from(start).ok()?, u32::try_from(part.len()).ok()?))
}

/// The ABI of the `extern` block that declares `entry` of `outline` (the outline of `text`, a
/// file's or a macro call's expansion's): the block's string unquoted as written, `"C"` for a
/// bare `extern` (the Reference, [items.extern.abi.default]); `None` when no block declares it
/// (its parent is no `extern` block, or it has none). The fact of the item the names task
/// writes into its row ([`ItemRow::foreign`]), so no reader outlines a text again to ask it.
///
/// The outline's ABI range is the block's `Abi` node, `extern "abi"` (blaze-file-items
/// walk/rows.rs `(EXTERN_BLOCK, ABI)`), relative to the block's start (rule 71). A range outside
/// the text is [`ItemsRecordError::Range`]; a written ABI that is not one quoted string (a raw
/// string, an escape) is [`ItemsRecordError::ForeignAbi`], never read as "C" (rule 3).
///
/// The one home of the reading (rule 30): the solved names' `extern_abi` (blaze-ladder
/// front2/source.rs) calls it on the same outline, so the two sources answer alike.
pub fn foreign_abi<'t>(text: &'t str, outline: &blaze_file_items::Outline<'_>, entry: &blaze_surface::Item, item: ItemKey, krate: CrateItemKey) -> Result<Option<&'t str>, ItemsRecordError> {
    let Some(parent) = entry.parent else { return Ok(None) };
    let block = outline.item(parent);
    let ItemDetail::ExternBlock { abi } = block.detail else { return Ok(None) };
    let Some(r) = abi else { return Ok(Some("C")) };
    let (start, end) = (block.range.start.saturating_add(r.start), block.range.start.saturating_add(r.end));
    let written = text.get(start as usize..end as usize).ok_or(ItemsRecordError::Range { krate, start, len: end.saturating_sub(start) })?;
    let written = written.trim();
    let written = written.strip_prefix("extern").map_or(written, str::trim_start);
    if written.is_empty() {
        return Ok(Some("C"));
    }
    match written.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        Some(abi) if !abi.contains(['"', '\\']) => Ok(Some(abi)),
        _ => Err(ItemsRecordError::ForeignAbi { item }),
    }
}

fn body_holds_items(detail: ItemDetail) -> bool {
    matches!(
        detail,
        ItemDetail::Fn { body_holds_items: true } | ItemDetail::Const { body_holds_items: true } | ItemDetail::Static { body_holds_items: true }
    )
}

/// The head of an impl's self type AS WRITTEN: what an impl index files the impl under without
/// reading more of the type. A path is not resolved here: the reader resolves it in the impl's
/// module ([`ImplRow::self_path`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelfHead {
    /// A path (`S<T>`, `a::S`, `u32`): [`ImplRow::self_path`] holds it without its arguments.
    Path,
    /// One segment naming a generic parameter of the impl (`impl<T> Tr for T`): a blanket impl.
    Param,
    Ref,
    RefMut,
    PtrConst,
    PtrMut,
    Slice,
    Array,
    Never,
    /// A tuple of this many elements (`()` is 0).
    Tuple(u16),
    /// Any other form (`dyn Tr`, `fn(A) -> B`, `<T as Tr>::Out`, `(T)`, a macro call): the
    /// reader reads the text, or refuses the impl by name.
    Other,
}

impl SelfHead {
    fn byte(self) -> (u8, u16) {
        match self {
            SelfHead::Path => (0, 0),
            SelfHead::Param => (1, 0),
            SelfHead::Ref => (2, 0),
            SelfHead::RefMut => (3, 0),
            SelfHead::PtrConst => (4, 0),
            SelfHead::PtrMut => (5, 0),
            SelfHead::Slice => (6, 0),
            SelfHead::Array => (7, 0),
            SelfHead::Never => (8, 0),
            SelfHead::Tuple(n) => (9, n),
            SelfHead::Other => (10, 0),
        }
    }

    fn from_byte(byte: u8, arity: u16) -> Option<SelfHead> {
        Some(match byte {
            0 => SelfHead::Path,
            1 => SelfHead::Param,
            2 => SelfHead::Ref,
            3 => SelfHead::RefMut,
            4 => SelfHead::PtrConst,
            5 => SelfHead::PtrMut,
            6 => SelfHead::Slice,
            7 => SelfHead::Array,
            8 => SelfHead::Never,
            9 => SelfHead::Tuple(arity),
            10 => SelfHead::Other,
            _ => return None,
        })
    }
}

/// The crate `path`'s first segment names when it is NOT `here` (token-mark-2 R2): a `crate` the
/// renderer wrote for another crate's `$crate` carries that crate's root mark (crate_roots
/// `root_mark`), read by the one resolver, [`crate::crate_roots::root_by_mark`]. `None`: no marks
/// (a file, a kept item, a block text: read as the scope read them), an absolute path, a first
/// segment that is no root, or this crate's root.
fn root_of_path(marks: Option<crate::crate_roots::TextMarks<'_>>, path: &str, here: CrateItemKey, impl_: ItemKey) -> Result<Option<CrateItemKey>, ItemsRecordError> {
    let Some(marks) = marks else { return Ok(None) };
    let t = path.trim_start();
    if t.starts_with("::") {
        return Ok(None);
    }
    let end = t.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(t.len());
    let first = &t[..end];
    let mark = marks.mark_of(first).map_err(|miss| ItemsRecordError::RootMark { impl_, miss })?;
    Ok(match crate::crate_roots::root_by_mark(first, mark) {
        crate::crate_roots::Root::Crate(k) if k != here => Some(k),
        _ => None,
    })
}

/// Whether `s` is a path of plain segments: `a`, `a::b`, `::a::b`, `r#a`, a macro's
/// `$crate::a`; no spaces, no arguments, no anchor.
fn plain_path(s: &str) -> bool {
    let rest = s.strip_prefix("::").unwrap_or(s);
    !rest.is_empty()
        && rest.split("::").all(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'#' || b == b'$' || b >= 0x80))
}

/// A written path type or trait split at its first `<` or `(`: the path, and whether it is
/// written with no arguments.
fn path_and_bare(s: &str) -> (&str, bool) {
    match s.find(['<', '(']) {
        Some(i) => (s[..i].trim_end(), false),
        None => (s, true),
    }
}

/// How many elements a bracketed list's inside holds at its own depth, and whether a `;` is
/// written at that depth (an array's). Brackets are counted, angle brackets are not (a `>` of
/// `->` is no bracket, and a comma inside `<..>` is still inside a `(`/`[` or at top level of a
/// generic list, which is counted below as its own depth).
fn list_shape(inside: &str) -> (u16, bool, bool) {
    let (mut depth, mut angle) = (0usize, 0usize);
    let (mut commas, mut semi, mut trailing) = (0u16, false, false);
    let mut prev = 0u8;
    for b in inside.bytes() {
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'<' => angle += 1,
            b'>' if prev != b'-' => angle = angle.saturating_sub(1),
            b',' if depth == 0 && angle == 0 => {
                commas = commas.saturating_add(1);
                trailing = true;
            }
            b';' if depth == 0 && angle == 0 => semi = true,
            _ => {}
        }
        if !b.is_ascii_whitespace() && b != b',' {
            trailing = false;
        }
        prev = b;
    }
    (commas, semi, trailing)
}

/// The head of the self type `ty` (trimmed, as [`blaze_file_items::ImplHeader::self_ty`] gives
/// it) of an impl whose generic parameters are `params`; for a path, the path without its
/// arguments.
fn self_head<'t>(ty: &'t str, params: &[&str]) -> (SelfHead, Option<&'t str>) {
    let b = ty.as_bytes();
    match b.first() {
        Some(b'&') => {
            // `&'a mut T`, `& mut T`, `&&T` (a reference to a reference).
            let mut rest = ty[1..].trim_start();
            if rest.starts_with('\'') {
                let end = rest[1..].find(|c: char| !(c.is_alphanumeric() || c == '_')).map_or(rest.len(), |i| i + 1);
                rest = rest[end..].trim_start();
            }
            let is_mut = rest.strip_prefix("mut").is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'));
            (if is_mut { SelfHead::RefMut } else { SelfHead::Ref }, None)
        }
        Some(b'*') => {
            let rest = ty[1..].trim_start();
            if rest.starts_with("mut") {
                (SelfHead::PtrMut, None)
            } else if rest.starts_with("const") {
                (SelfHead::PtrConst, None)
            } else {
                (SelfHead::Other, None)
            }
        }
        Some(b'!') if ty.len() == 1 => (SelfHead::Never, None),
        Some(b'[') if b.last() == Some(&b']') => {
            let (_, semi, _) = list_shape(&ty[1..ty.len() - 1]);
            (if semi { SelfHead::Array } else { SelfHead::Slice }, None)
        }
        Some(b'(') if b.last() == Some(&b')') => {
            let inside = ty[1..ty.len() - 1].trim();
            let (commas, _, trailing) = list_shape(inside);
            if inside.is_empty() {
                (SelfHead::Tuple(0), None)
            } else if commas == 0 {
                // `(T)`: a parenthesized type, not a tuple.
                (SelfHead::Other, None)
            } else {
                (SelfHead::Tuple(if trailing { commas } else { commas.saturating_add(1) }), None)
            }
        }
        _ => {
            let (path, _) = path_and_bare(ty);
            if !plain_path(path) || matches!(path, "dyn" | "impl" | "fn" | "unsafe" | "extern" | "for" | "Self") {
                (SelfHead::Other, None)
            } else if params.iter().any(|p| *p == path) {
                (SelfHead::Param, Some(path))
            } else {
                (SelfHead::Path, Some(path))
            }
        }
    }
}

/// An impl as its row is written.
struct ImplOut<'s> {
    key: ItemKey,
    module: ItemKey,
    trait_path: &'s str,
    self_path: &'s str,
    flags: u32,
    head: SelfHead,
    marks: u8,
    /// Where the bucket index files it ([`buckets::filing`]).
    filing: buckets::Filing,
    /// The crate the trait path's / self path's first segment names, when not this one
    /// ([`root_of_path`]).
    trait_root: Option<CrateItemKey>,
    self_root: Option<CrateItemKey>,
}

/// The items record of a solved crate. Its bytes are a function of the solve's answers: every
/// table is sorted by key (the impls by what they are filed by), so any worker in any order
/// writes the same bytes.
///
/// The bucket index ([`buckets`]) is resolved by the crate's OWN names only
/// ([`buckets::OwnNames`]): an impl whose trait or self type is named through another crate's
/// names is in the unfiled list (`TraitNotRead`, `SelfNotRead`), which a reader files itself.
/// A caller that holds the names of the crate's dependencies calls [`encode_with`].
pub fn encode<'n, 'b>(names: &'n CrateNames<'_>, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, ItemsRecordError> {
    encode_with(names, &buckets::OwnNames(names), alloc)
}

/// [`encode`], its bucket index resolved by `heads`: the names every impl header's trait path
/// and self type are walked through when the record is written (the whole graph's `Names`, or
/// the crate's own with its dependencies' records). The record's bytes are a function of the
/// solve's answers AND of what `heads` answers for the crate's impl headers: a key or read
/// list the record is kept under must cover the names `heads` reads (rule 21).
pub fn encode_with<'n, 'b, H: buckets::HeadNames>(names: &'n CrateNames<'_>, heads: &H, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, ItemsRecordError> {
    let mut strings: Strings<'b, 'n> = Strings::new(alloc);
    let tables = &names.tables;

    // Files, by index; expansions that made a text, by call key.
    let mut file_rows = RecordBytes::new_in(alloc);
    for f in names.files.iter() {
        let (path, shown) = (strings.put(f.package.as_str())?, strings.put(f.shown)?);
        file_rows.extend_from_slice(&f.id.0);
        for n in [path.0, path.1, shown.0, shown.1, u32_of(f.text.len(), "a file's text")?, 0] {
            put32(&mut file_rows, n);
        }
    }
    if names.files.len() >= EXPANSION as usize {
        return Err(ItemsRecordError::TooBig { what: "a crate's files" });
    }
    let mut calls: AVec<'b, (ItemKey, &'n str)> = AVec::new_in(alloc);
    calls.extend(names.expansions.iter().filter_map(|(k, e)| e.made.map(|m| (*k, m.text))));
    calls.sort_unstable_by_key(|c| c.0);
    if calls.len() >= EXPANSION as usize {
        return Err(ItemsRecordError::TooBig { what: "a crate's expansions" });
    }
    let mut expansion_rows = RecordBytes::new_in(alloc);
    for (call, text) in calls.iter() {
        expansion_rows.extend_from_slice(&call.0.0);
        expansion_rows.extend_from_slice(&expansion_id(text).0);
        put32(&mut expansion_rows, u32_of(text.len(), "an expansion's text")?);
        put32(&mut expansion_rows, 0);
    }
    // A text's code in a row and its bytes, when the record has a row for it.
    let text_of = |at: At| -> Option<(u32, &'n str)> {
        match at {
            At::File(i) => names.files.get(i as usize).map(|f| (i, f.text)),
            At::Expansion(call) => calls.binary_search_by_key(&call, |c| c.0).ok().map(|i| (EXPANSION | i as u32, calls[i].1)),
        }
    };
    // Where `part` is: its text's code, start and length; all `NONE`/0 when it is in no text
    // the record has a row for (a reader is refused by name: `NoPlace`).
    let place = |at: Option<At>, part: &str| -> (u32, u32, u32) {
        at.and_then(|at| text_of(at)).and_then(|(code, base)| span_in(base, part).map(|(start, len)| (code, start, len))).unwrap_or((NONE, 0, 0))
    };

    // The impls, read from their headers and sorted by what the index files them by.
    let mut impls: AVec<'b, ImplOut<'n>> = AVec::with_capacity_in(tables.impls.len(), alloc);
    for i in tables.impls.iter() {
        let out = match blaze_file_items::impl_header(i.header, names.edition, alloc) {
            Err(_) => ImplOut {
                key: i.key,
                module: i.module,
                trait_path: "",
                self_path: "",
                flags: 0,
                head: SelfHead::Other,
                marks: HEADER_UNREAD,
                filing: buckets::Filing::Not(buckets::NotFiled::HeaderUnread),
                trait_root: None,
                self_root: None,
            },
            Ok(h) => {
                let (head, self_path) = self_head(h.self_ty, h.params.as_slice());
                let (trait_path, marks) = match h.trait_ {
                    None => ("", 0),
                    Some(t) => {
                        let (path, bare) = path_and_bare(t);
                        // A plain path, or one that reads as a simple path by its tokens: an
                        // expansion's text writes `:: core :: clone :: Clone` (a derive's impl),
                        // which is kept as written (a reader trims each segment). Measured
                        // 2026-11-16 (front2_records, 70 crates): 8,774 of 28,719 impls had
                        // their trait unread here and were filed from their header's text.
                        if plain_path(path) || crate::task::parse_path(path, names.key, alloc).is_some() {
                            (path, HAS_TRAIT | if bare { BARE } else { 0 })
                        } else {
                            ("", HAS_TRAIT | TRAIT_UNREAD)
                        }
                    }
                };
                let negative = h.flags.bits() & ImplFlags::NEGATIVE.bits() != 0;
                // token-mark-2 R2: a path an expansion wrote with another crate's root keeps it;
                // the row's text alone would read `crate` as this crate.
                let trait_root = match h.trait_ {
                    Some(t) => root_of_path(i.marks, path_and_bare(t).0, names.key, i.key)?,
                    None => None,
                };
                let self_root = match self_path {
                    Some(p) => root_of_path(i.marks, p, names.key, i.key)?,
                    None => None,
                };
                let filing = buckets::filing(heads, names.key, i.module, h.trait_, negative, head, self_path, trait_root, self_root, alloc);
                ImplOut {
                    key: i.key,
                    module: i.module,
                    trait_path,
                    self_path: self_path.unwrap_or(""),
                    flags: h.flags.bits(),
                    head,
                    marks,
                    filing,
                    trait_root,
                    self_root,
                }
            }
        };
        impls.push(out);
    }
    impls.sort_by(|a, b| (a.trait_path.as_bytes(), a.module, a.key).cmp(&(b.trait_path.as_bytes(), b.module, b.key)));
    #[cfg(test)]
    if plant::is(plant::Plant::DropImpl) && !impls.is_empty() {
        impls.remove(0);
    }

    // The modules: those `Names::modules` lists (walked, then made), and any other key a row
    // names as a module (a parent, an item's home), unlisted.
    let mut modules: AVec<'b, ItemKey> = AVec::new_in(alloc);
    for m in names.tree.modules.iter() {
        modules.push(m.key);
        modules.extend(m.parent.map(|p| names.tree.modules[p].key));
    }
    for (k, m) in names.made.iter() {
        modules.push(*k);
        modules.extend(m.info.parent);
    }
    modules.extend(tables.texts.values().map(|t| t.0.module));
    modules.extend(tables.macros.values().map(|m| m.module));
    modules.extend(impls.iter().map(|i| i.module));
    modules.sort_unstable();
    modules.dedup();
    let module_at = |m: ItemKey| -> Result<u32, ItemsRecordError> {
        modules.binary_search(&m).map(|i| i as u32).map_err(|_| ItemsRecordError::NotListed { module: m })
    };
    u32_of(modules.len(), "a crate's modules")?;
    let mut module_rows = RecordBytes::new_in(alloc);
    for m in modules.iter() {
        let walked = names.tree.by_key.get(m).map(|i| &names.tree.modules[*i]);
        let made = names.made.get(m);
        let parent = match (walked, made) {
            (Some(w), _) => w.parent.map(|p| names.tree.modules[p].key),
            (None, Some(made)) => made.info.parent,
            (None, None) => None,
        };
        let parent = match parent {
            Some(p) => module_at(p)?,
            None => NONE,
        };
        let mut flags = 0;
        if walked.is_some() || made.is_some() {
            flags |= LISTED;
        }
        if walked.is_none() && made.is_some() {
            flags |= MADE;
        }
        let path = match names.path_of(*m) {
            Some(p) => {
                flags |= HAS_PATH;
                strings.put(p)?
            }
            None => (0, 0),
        };
        module_rows.extend_from_slice(&m.0.0);
        for n in [parent, path.0, path.1, flags] {
            put32(&mut module_rows, n);
        }
    }

    // The impl rows, and each impl's row by its key (an item row names it).
    let mut impl_rows = RecordBytes::new_in(alloc);
    let mut impl_row_of: HashMap<ItemKey, u32, Alloc<'b>> = HashMap::new_in(alloc);
    for (at, i) in impls.iter().enumerate() {
        impl_row_of.insert(i.key, u32_of(at, "a crate's impls")?);
        let (trait_path, self_path) = (strings.put(i.trait_path)?, strings.put(i.self_path)?);
        let (head, arity) = i.head.byte();
        impl_rows.extend_from_slice(&i.key.0.0);
        for n in [module_at(i.module)?, trait_path.0, trait_path.1, self_path.0, self_path.1, i.flags] {
            put32(&mut impl_rows, n);
        }
        impl_rows.extend_from_slice(&[head, i.marks]);
        put16(&mut impl_rows, arity);
        let roots = (if i.trait_root.is_some() { ROOT_TRAIT } else { 0 }) | (if i.self_root.is_some() { ROOT_SELF } else { 0 });
        put32(&mut impl_rows, roots);
        impl_rows.extend_from_slice(&i.trait_root.map_or([0; 32], |k| k.0.0));
        impl_rows.extend_from_slice(&i.self_root.map_or([0; 32], |k| k.0.0));
    }

    // The items, by key.
    let mut items: AVec<'b, ItemKey> = AVec::from_iter_in(tables.texts.keys().copied(), alloc);
    items.sort_unstable();
    let mut item_rows = RecordBytes::new_in(alloc);
    for key in items.iter() {
        let (it, at) = &tables.texts[key];
        let (whole, outline) = names.outline_at(*at).ok_or(ItemsRecordError::NoOutline { item: *key })?;
        let entry = outline.items().get(it.item.0 as usize).ok_or(ItemsRecordError::NoOutline { item: *key })?;
        let detail = entry.detail;
        // The block's ABI, a fact of the item, from the outline the solve made of its text
        // (a file's, or an expansion's: the names task outlines each expansion once).
        let foreign = foreign_abi(whole, outline, entry, *key, names.key)?;
        let (code, start, len) = place(Some(*at), it.text);
        let (name, mut flags) = match it.name {
            Some(n) => (strings.put(n)?, NAMED),
            None => ((0, 0), 0),
        };
        if body_holds_items(detail) {
            flags |= BODY_HOLDS_ITEMS;
        }
        if tables.ctors.contains_key(key) {
            flags |= CTOR_OR_WRITTEN_BY;
        }
        let abi = match foreign {
            Some(abi) => {
                flags |= FOREIGN;
                strings.put(abi)?
            }
            None => (0, 0),
        };
        // The header's place in the same text; `NONE` for no impl (or a header out of it).
        let header = match it.impl_header {
            Some(h) if code != NONE => text_of(*at).and_then(|(_, base)| span_in(base, h)).unwrap_or((NONE, 0)),
            _ => (NONE, 0),
        };
        item_rows.extend_from_slice(&key.0.0);
        for n in [module_at(it.module)?, code, start, len, it.item.0, name.0] {
            put32(&mut item_rows, n);
        }
        put16(&mut item_rows, u16::try_from(name.1).map_err(|_| ItemsRecordError::TooBig { what: "an item's name" })?);
        item_rows.extend_from_slice(&[detail.kind().byte(), flags]);
        for n in [header.0, header.1, impl_row_of.get(key).copied().unwrap_or(NONE), abi.0, abi.1] {
            put32(&mut item_rows, n);
        }
    }

    // Each trait's and impl's members, in the solve's (declaration) order.
    let mut owners: AVec<'b, ItemKey> = AVec::from_iter_in(tables.members.keys().copied(), alloc);
    owners.sort_unstable();
    let mut owner_rows = RecordBytes::new_in(alloc);
    let mut member_rows = RecordBytes::new_in(alloc);
    let mut members = 0usize;
    // Each named member's key, its owner's row and its own row: the member owners section
    // ([`owners`]), written after everything else.
    let mut owned: AVec<'b, (ItemKey, u32, u32)> = AVec::new_in(alloc);
    #[cfg(test)]
    let mut cut = plant::is(plant::Plant::ShortMembers);
    for owner in owners.iter() {
        let list = tables.members[owner].as_slice();
        #[cfg(test)]
        let list = if cut && !list.is_empty() {
            cut = false;
            &list[..list.len() - 1]
        } else {
            list
        };
        let owner_at = tables.texts.get(owner).map(|t| t.1);
        owner_rows.extend_from_slice(&owner.0.0);
        put32(&mut owner_rows, u32_of(members, "a crate's members")?);
        put32(&mut owner_rows, u32_of(list.len(), "an owner's members")?);
        for m in list.iter() {
            let at = match m.written_by {
                Some(call) => Some(At::Expansion(call)),
                None => owner_at,
            };
            let (code, start, len) = place(at, m.text);
            let (name, mut flags) = match m.name {
                Some(n) => (strings.put(n)?, NAMED),
                None => ((0, 0), 0),
            };
            if body_holds_items(m.detail) {
                flags |= BODY_HOLDS_ITEMS;
            }
            if m.written_by.is_some() {
                flags |= CTOR_OR_WRITTEN_BY;
            }
            member_rows.extend_from_slice(&m.key.0.0);
            for n in [code, start, len, m.item.0, name.0] {
                put32(&mut member_rows, n);
            }
            put16(&mut member_rows, u16::try_from(name.1).map_err(|_| ItemsRecordError::TooBig { what: "a member's name" })?);
            member_rows.extend_from_slice(&[m.detail.kind().byte(), flags]);
            if m.name.is_some() {
                // Both rows are whole here: the owner's was pushed before its members, this
                // member's just above.
                owned.push((m.key, u32_of(owner_rows.len() / OWNER_ROW - 1, "a crate's owners")?, u32_of(member_rows.len() / MEMBER_ROW - 1, "a crate's members")?));
            }
        }
        members += list.len();
    }

    // Lang items by (name, key).
    let mut langs: AVec<'b, (&'n str, ItemKey)> = AVec::from_iter_in(tables.langs.iter().map(|l| (l.name, l.key)), alloc);
    langs.sort_unstable_by(|a, b| (a.0.as_bytes(), a.1).cmp(&(b.0.as_bytes(), b.1)));
    langs.dedup();
    let mut lang_rows = RecordBytes::new_in(alloc);
    for (name, key) in langs.iter() {
        let name = strings.put(*name)?;
        lang_rows.extend_from_slice(&key.0.0);
        put32(&mut lang_rows, name.0);
        put32(&mut lang_rows, name.1);
    }

    // Macro definitions by key: where the body is, never its bytes.
    let mut macros: AVec<'b, ItemKey> = AVec::from_iter_in(tables.macros.keys().copied(), alloc);
    macros.sort_unstable();
    let mut macro_rows = RecordBytes::new_in(alloc);
    for key in macros.iter() {
        let m = &tables.macros[key];
        let at = tables.texts.get(key).map(|t| t.1);
        let name = strings.put(m.name)?;
        let mut flags = 0u8;
        if m.export {
            flags |= EXPORT;
        }
        if m.local_inner {
            flags |= LOCAL_INNER;
        }
        // (text code, body start, body length), (second start or string, its length), kind, sub.
        let (body, second, kind, sub) = match m.body {
            MacroBody::Rules(body) => (place(at, body), (0, 0), BODY_RULES, 0),
            MacroBody::Macro2 { params: None, body } => (place(at, body), (0, 0), BODY_MACRO2, 0),
            MacroBody::Macro2 { params: Some(p), body } => {
                flags |= HAS_PARAMS;
                let (body_at, params_at) = (place(at, body), place(at, p));
                // Both parts are in the item's one text, or the row has no place at all.
                if body_at.0 != NONE && params_at.0 == body_at.0 {
                    (body_at, (params_at.1, params_at.2), BODY_MACRO2, 0)
                } else {
                    ((NONE, 0, 0), (0, 0), BODY_MACRO2, 0)
                }
            }
            MacroBody::BuiltinFn(name) => ((NONE, 0, 0), strings.put(name)?, BODY_BUILTIN_FN, 0),
            MacroBody::BuiltinOther(shape) => ((NONE, 0, 0), (0, 0), BODY_BUILTIN_OTHER, shape.byte()),
            MacroBody::Proc { kind, helpers } => ((NONE, 0, 0), strings.put(helpers)?, BODY_PROC, kind.byte()),
            MacroBody::BuiltinDerive { helpers } => ((NONE, 0, 0), strings.put(helpers)?, BODY_BUILTIN_DERIVE, 0),
        };
        macro_rows.extend_from_slice(&key.0.0);
        for n in [module_at(m.module)?, name.0, name.1, body.0, body.1, body.2, second.0, second.1] {
            put32(&mut macro_rows, n);
        }
        macro_rows.extend_from_slice(&[kind, flags, sub, 0]);
        put32(&mut macro_rows, 0);
    }

    // The cfg atoms, in `Cfg::atoms`' own (sorted) order.
    let atoms = names.cfg.atoms();
    let mut atom_rows = RecordBytes::new_in(alloc);
    for (name, value) in atoms.iter() {
        let name = strings.put(*name)?;
        let value = match value {
            Some(v) => strings.put(*v)?,
            None => (NONE, 0),
        };
        for n in [name.0, name.1, value.0, value.1] {
            put32(&mut atom_rows, n);
        }
    }
    let crate_name = strings.put(names.name)?;

    let counts = [names.files.len(), calls.len(), items.len(), owners.len(), members, impls.len(), langs.len(), macros.len(), modules.len(), atoms.len()];
    let mut sections = [file_rows, expansion_rows, item_rows, owner_rows, member_rows, impl_rows, lang_rows, macro_rows, module_rows, atom_rows];
    let mut offsets = [0usize; TABLES + 1];
    let mut at = HEADER;
    for (i, s) in sections.iter_mut().enumerate() {
        pad(s);
        offsets[i] = at;
        at += s.len();
    }
    offsets[TABLES] = at;
    let root_is_marked = names.files.first().is_none_or(|f| f.text.contains("rustc_attrs") || f.text.contains("rustc_coherence_is_core"));
    let mut flags = 0;
    if names.implicit.proc_macro {
        flags |= CRATE_PROC_MACRO;
    }
    if root_is_marked {
        flags |= CRATE_INCOHERENT;
    }
    if names.proc_key.is_some() {
        flags |= CRATE_PROC_KEY;
    }
    let mut out = RecordBytes::with_capacity_in(at + strings.bytes.len(), alloc);
    out.extend_from_slice(&MAGIC);
    put32(&mut out, VERSION);
    put32(&mut out, flags);
    put32(&mut out, u32::from(names.edition.byte()));
    for (i, n) in counts.iter().enumerate() {
        put32(&mut out, u32_of(*n, WHAT[i])?);
    }
    for n in offsets.iter() {
        put32(&mut out, u32_of(*n, "items offsets")?);
    }
    put32(&mut out, u32_of(strings.bytes.len(), "items strings")?);
    put32(&mut out, crate_name.0);
    put32(&mut out, crate_name.1);
    put32(&mut out, std_field(names.implicit.std));
    out.resize(KEYS_AT, 0);
    out.extend_from_slice(&names.key.0.0);
    out.extend_from_slice(&names.root.0.0);
    out.extend_from_slice(&names.proc_key.map_or([0u8; 32], |k| k.0.0));
    for s in sections.iter() {
        out.extend_from_slice(s);
    }
    out.extend_from_slice(&strings.bytes);
    grain::append(&mut out, names, &items)?;
    owners::append(&mut out, &mut owned)?;
    variants::append(&mut out, &tables.enums, alloc)?;
    // In the impl table's order: row `i`'s filing at index `i`.
    let filings: AVec<'b, buckets::Filing> = AVec::from_iter_in(impls.iter().map(|i| i.filing), alloc);
    buckets::append(&mut out, &filings, alloc)?;
    Ok(out)
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(b)
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn id_at(bytes: &[u8], at: usize) -> ContentId {
    let mut k = [0u8; 32];
    k.copy_from_slice(&bytes[at..at + 32]);
    ContentId(k)
}

fn key_at(bytes: &[u8], at: usize) -> ItemKey {
    ItemKey(id_at(bytes, at))
}

/// A stretch of one of a crate's texts: where an item's, a member's or an impl header's text is.
/// Valid only in the text whose digest the record holds ([`ItemsRecord::slice`] checks it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TextSpan {
    pub at: At,
    pub start: u32,
    pub len: u32,
}

/// A file of the crate, as the solve read it.
///
/// Row, 56 bytes, by file index (an [`At::File`]'s): digest 32 (what proves a text handed to a
/// reader is the one the ranges were made in: question 7), path at/len u32 (the file's
/// [`PackagePath`]: what the loader is asked for with the crate, 7, 19), shown path at/len u32
/// (what a finding prints: 19), text length u32, pad.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileRow<'a> {
    pub path: PackagePath<'a>,
    pub shown: &'a str,
    pub id: ContentId,
    pub len: u32,
}

/// An expansion that made a text: the record that holds the text of the items a macro call
/// wrote is the expansion's own, found by this call key.
///
/// Row, 72 bytes, sorted by call key: call key 32 (an [`At::Expansion`]'s: 7, 19), digest of
/// the text 32 ([`expansion_id`]: the same proof as a file's), text length u32, pad.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ExpansionRow {
    pub call: ItemKey,
    pub id: ContentId,
    pub len: u32,
}

/// An item with text: of a file or of an expansion.
///
/// Row, 80 bytes, sorted by key (found by binary search in place): key 32; module u32, a module
/// row (question 8: the module its names are read in); at u32 (a file index, or bit 31 and an
/// expansion row; `u32::MAX`: no place), start u32, length u32 (7: where its whole text is,
/// attributes included; the attributes the front end reads, `repr`, `lang`, `rustc_*`,
/// `fundamental`, are read from that text by its lowering: question 23); outline entry u32
/// (10: its `Place`); name at u32, name length u16 (9: asked without the text); kind u8 (11:
/// `ItemKind`'s byte); flags u8 (named; body holds items: 11; a tuple or unit struct, its own
/// constructor: 12); impl header start u32 and length u32 in the same text (14: an impl's
/// header for a message; `u32::MAX`: none); impl row u32 (14: its row among the impls); the
/// ABI of the `extern` block that declares it at u32, length u32 (10c: set when flag
/// [`FOREIGN`] is, else zero).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ItemRow<'a> {
    pub key: ItemKey,
    pub kind: ItemKind,
    pub module: ItemKey,
    /// `None`: the record has no place for its text.
    pub text: Option<TextSpan>,
    pub outline: ItemIndex,
    pub name: Option<&'a str>,
    pub body_holds_items: bool,
    pub ctor: bool,
    pub impl_header: Option<TextSpan>,
    pub impl_row: Option<u32>,
    /// `Some(abi)`: an `extern` block declares it (a foreign fn or static), and `abi` is the
    /// block's string unquoted as written, `"C"` for a bare `extern`; `None`: no block does.
    /// Written by the names task from the outline of the text the item is in, a file's or an
    /// expansion's alike, so a reader never outlines a text again to ask it.
    pub foreign: Option<&'a str>,
}

/// A member of a trait or an impl.
///
/// Owner row, 40 bytes, sorted by owner key: owner 32, first member u32, count u32. Member row,
/// 56 bytes, in the owner's declaration order (question 13: `Names::members`' order): key 32;
/// at, start, length u32 (its whole text: the owner's text, or the expansion that wrote it);
/// outline entry u32; name at u32, length u16; kind u8; flags u8 (named; body holds items;
/// written by a call's expansion, which `at` then names).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MemberRow<'a> {
    pub key: ItemKey,
    pub name: Option<&'a str>,
    pub outline: ItemIndex,
    pub kind: ItemKind,
    pub body_holds_items: bool,
    pub text: Option<TextSpan>,
    /// The call whose expansion it is in; `None` when written in its owner's body.
    pub written_by: Option<ItemKey>,
}

/// An impl of the crate, with what an impl index files it by, read from its header when the
/// crate was solved: no text is read to file it (question 14, `Buckets::file`'s facts).
///
/// Row, 128 bytes, sorted by (trait path as written, module, key), so the inherent impls come
/// first and each run of equal (trait path, module) is resolved once: key 32; module u32 (the
/// module the two paths are resolved in); trait path at/len u32 (without its arguments and
/// without a negative impl's `!`; empty: an inherent impl or an unread trait); self path at/len
/// u32 (the self type's head path without arguments, for `Path` and `Param`); impl flags u32
/// (`ImplFlags`: negative, unsafe, default, const); head u8 and arity u16 ([`SelfHead`]); marks
/// u8 (has a trait; bare: the trait has no arguments, `Homes::bare`; header unread; trait not a
/// plain path); roots u32 ([`ROOT_TRAIT`], [`ROOT_SELF`]); trait root key 32 and self root key 32
/// (zero unless its bit is set: token-mark-2 R2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ImplRow<'a> {
    pub key: ItemKey,
    pub module: ItemKey,
    /// The trait's path as written, without arguments; `None` for an inherent impl, and for a
    /// trait impl whose trait did not read as a plain path (`trait_unread`).
    pub trait_path: Option<&'a str>,
    /// A trait impl (even when `trait_path` is `None` because it did not read).
    pub has_trait: bool,
    /// The trait is written with no generic arguments.
    pub bare: bool,
    pub flags: ImplFlags,
    pub head: SelfHead,
    /// The self type's head path as written, for [`SelfHead::Path`] and [`SelfHead::Param`].
    pub self_path: Option<&'a str>,
    /// The header did not read at all: nothing else of the row is known.
    pub header_unread: bool,
    /// A trait impl whose trait is not a plain path.
    pub trait_unread: bool,
    /// The crate `trait_path`'s first segment `crate` names when an expansion wrote it for
    /// another crate's `$crate` (token-mark-2 R2); `None`: as the text reads in this crate.
    pub trait_root: Option<CrateItemKey>,
    /// The same for `self_path`.
    pub self_root: Option<CrateItemKey>,
}

/// A module of the crate.
///
/// Row, 48 bytes, sorted by key: key 32; parent u32 (a module row; `u32::MAX`: none); path
/// at/len u32 (`krate::a::b`: question 20); flags u32 (listed: one `Names::modules` gives, the
/// rest being keys other rows name as a home; written by a macro; has a path).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ModuleRow<'a> {
    pub key: ItemKey,
    pub parent: Option<ItemKey>,
    pub path: Option<&'a str>,
    pub listed: bool,
    pub made: bool,
}

/// A macro definition's body as its row holds it (question 21).
///
/// Row, 72 bytes, sorted by the definition's key: key 32; module u32; name at/len u32; body at,
/// start, length u32 (a `macro_rules!`' or `macro`'s body in its item's text); a second pair
/// u32 (a `macro`'s parameters' start and length in the same text; a builtin's name or a proc
/// macro's helpers in the strings); body kind u8; flags u8 (exported, `local_inner_macros`, has
/// parameters); sub u8 (a builtin's call shape, a proc macro's kind); pad.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MacroBodyAt<'a> {
    Rules(Option<TextSpan>),
    Macro2 { params: Option<TextSpan>, body: Option<TextSpan> },
    BuiltinFn(&'a str),
    BuiltinOther(CallShape),
    Proc { kind: ProcKind, helpers: &'a str },
    BuiltinDerive { helpers: &'a str },
}

/// A macro definition: [`MacroSrc`] with its body's place for its text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MacroRow<'a> {
    pub key: ItemKey,
    pub name: &'a str,
    pub module: ItemKey,
    pub export: bool,
    pub local_inner: bool,
    pub body: MacroBodyAt<'a>,
}

/// What a reader decoded so far: the measure of "decoded on demand".
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ItemsDecoded {
    /// Records opened (crates asked about).
    pub crates: usize,
    /// Records' bytes, opened.
    pub bytes: usize,
    /// Texts asked of the loader and checked against their digest.
    pub texts: usize,
    /// Their bytes (hashed once each).
    pub text_bytes: usize,
}

/// A text as a reader saw it the one time it asked: checked once, then answered from here.
#[derive(Clone, Copy)]
enum Seen<'a> {
    Fresh(&'a str),
    Stale(ContentId),
    Gone,
}

/// One crate's items record, read in place (D114 rule 1, a record is its own index): a row, a
/// file's rows, an owner's members ([`Members`]) and an expansion are each found by binary search
/// and read from the bytes each time asked. Opening allocates nothing. The cfg atoms are made
/// the first time asked, and each text is checked against its digest the first time asked; both
/// are kept for the rest of the command.
pub struct ItemsRecord<'a> {
    bytes: &'a [u8],
    key: CrateItemKey,
    root_module: ItemKey,
    counts: [usize; TABLES],
    offsets: [usize; TABLES + 1],
    strings_len: usize,
    atoms: OnceCell<AVec<'a, (&'a str, Option<&'a str>)>>,
    /// Each text asked, by where it is, as its one check saw it: empty, and unallocated, until
    /// the first text is asked.
    seen: RefCell<HashMap<At, Seen<'a>, Alloc<'a>>>,
    decoded: Cell<ItemsDecoded>,
    alloc: Alloc<'a>,
}

impl<'a> ItemsRecord<'a> {
    /// Checks the header and that every section lies inside `bytes`; parses nothing else.
    pub fn open(bytes: &'a [u8], alloc: Alloc<'a>) -> Result<ItemsRecord<'a>, ItemsRecordError> {
        if bytes.as_ptr() as usize % ALIGN != 0 {
            return Err(ItemsRecordError::Unaligned);
        }
        if bytes.len() < HEADER || bytes[..4] != MAGIC || le32(bytes, 4) != VERSION {
            return Err(ItemsRecordError::NotThisRecord);
        }
        let f = |i: usize| le32(bytes, 8 + 4 * i) as usize;
        let mut counts = [0usize; TABLES];
        let mut offsets = [0usize; TABLES + 1];
        for i in 0..TABLES {
            counts[i] = f(F_COUNTS + i);
        }
        for i in 0..=TABLES {
            offsets[i] = f(F_OFFSETS + i);
        }
        let strings_len = f(F_STRINGS_LEN);
        let within = |what: &'static str, at: usize, len: Option<usize>| match len.and_then(|len| at.checked_add(len)) {
            Some(end) if end <= bytes.len() => Ok(()),
            _ => Err(ItemsRecordError::Short { what, at }),
        };
        for i in 0..TABLES {
            within(WHAT[i], offsets[i], counts[i].checked_mul(WIDTHS[i]))?;
        }
        within("strings", offsets[TABLES], Some(strings_len))?;
        Edition::from_byte(u8::try_from(f(F_EDITION)).unwrap_or(u8::MAX)).ok_or(ItemsRecordError::Byte { what: "an edition", byte: bytes[8 + 4 * F_EDITION] })?;
        std_of(le32(bytes, 8 + 4 * F_STD)).ok_or(ItemsRecordError::Byte { what: "the crate's std", byte: bytes[8 + 4 * F_STD] })?;
        Ok(ItemsRecord {
            bytes,
            key: CrateItemKey(id_at(bytes, KEYS_AT)),
            root_module: key_at(bytes, KEYS_AT + 32),
            counts,
            offsets,
            strings_len,
            atoms: OnceCell::new(),
            seen: RefCell::new(HashMap::new_in(alloc)),
            decoded: Cell::new(ItemsDecoded { crates: 1, bytes: bytes.len(), ..ItemsDecoded::default() }),
            alloc,
        })
    }

    pub fn key(&self) -> CrateItemKey {
        self.key
    }

    pub fn root_module(&self) -> ItemKey {
        self.root_module
    }

    fn field(&self, i: usize) -> u32 {
        le32(self.bytes, 8 + 4 * i)
    }

    /// The crate's edition (question 5).
    pub fn edition(&self) -> Edition {
        // `open` checked the byte.
        Edition::from_byte(self.bytes[8 + 4 * F_EDITION]).unwrap_or(Edition::E2015)
    }

    /// Whether the crate is a proc-macro unit (question 17).
    pub fn proc_macro(&self) -> bool {
        self.field(F_FLAGS) & CRATE_PROC_MACRO != 0
    }

    /// Whether the crate may write an inherent impl of a type it does not define (question 16).
    pub fn may_write_incoherent(&self) -> bool {
        self.field(F_FLAGS) & CRATE_INCOHERENT != 0
    }

    /// The key its proc-macro runner knows it by, for a proc-macro crate that has one.
    pub fn proc_key(&self) -> Option<CrateItemKey> {
        (self.field(F_FLAGS) & CRATE_PROC_KEY != 0).then(|| CrateItemKey(id_at(self.bytes, KEYS_AT + 64)))
    }

    /// Which `std` the crate's code names `std`: Rust's, or bstd under a blaze edition
    /// (blaze-unit-graph's `implicit`, as the crate was solved with it).
    pub fn std_is(&self) -> blaze_unit_graph::StdIs {
        std_of(self.field(F_STD)).expect("`open` refused a header whose F_STD names no std")
    }

    /// The crate's library name.
    pub fn name(&self) -> Result<&'a str, ItemsRecordError> {
        self.str_at("the crate's name", self.field(F_NAME_AT), self.field(F_NAME_LEN))
    }

    pub fn decoded(&self) -> ItemsDecoded {
        self.decoded.get()
    }

    fn str_at(&self, what: &'static str, at: u32, len: u32) -> Result<&'a str, ItemsRecordError> {
        let (at, len) = (at as usize, len as usize);
        if at.checked_add(len).is_none_or(|end| end > self.strings_len) {
            return Err(ItemsRecordError::Short { what, at });
        }
        let start = self.offsets[TABLES] + at;
        let bytes: &'a [u8] = &self.bytes[start..start + len];
        core::str::from_utf8(bytes).map_err(|_| ItemsRecordError::Utf8 { what, at })
    }

    /// Row `i` of table `t`: its offset in the bytes. `i` is below the table's count.
    fn row(&self, t: usize, i: usize) -> usize {
        self.offsets[t] + i * WIDTHS[t]
    }

    /// Binary search of table `t`'s rows, keyed by their first 32 bytes.
    fn find(&self, t: usize, key: ItemKey) -> Option<usize> {
        let (mut lo, mut hi) = (0usize, self.counts[t]);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let row = self.row(t, mid);
            match self.bytes[row..row + 32].cmp(&key.0.0[..]) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    fn module_key(&self, index: u32) -> Result<ItemKey, ItemsRecordError> {
        if (index as usize) < self.counts[T_MODULES] {
            Ok(key_at(self.bytes, self.row(T_MODULES, index as usize)))
        } else {
            Err(ItemsRecordError::Short { what: "a module row", at: index as usize })
        }
    }

    /// The text a row's `at` code names; `None` for no place.
    fn at_of(&self, code: u32) -> Result<Option<At>, ItemsRecordError> {
        if code == NONE {
            return Ok(None);
        }
        if code & EXPANSION != 0 {
            let i = (code & !EXPANSION) as usize;
            if i >= self.counts[T_EXPANSIONS] {
                return Err(ItemsRecordError::Short { what: "an expansion row", at: i });
            }
            return Ok(Some(At::Expansion(key_at(self.bytes, self.row(T_EXPANSIONS, i)))));
        }
        if code as usize >= self.counts[T_FILES] {
            return Err(ItemsRecordError::Short { what: "a file row", at: code as usize });
        }
        Ok(Some(At::File(code)))
    }

    fn span(&self, code: u32, start: u32, len: u32) -> Result<Option<TextSpan>, ItemsRecordError> {
        Ok(self.at_of(code)?.map(|at| TextSpan { at, start, len }))
    }

    /// How many files the crate has (question 19).
    pub fn files(&self) -> usize {
        self.counts[T_FILES]
    }

    /// File `index` of the crate.
    pub fn file(&self, index: u32) -> Result<Option<FileRow<'a>>, ItemsRecordError> {
        if index as usize >= self.counts[T_FILES] {
            return Ok(None);
        }
        let r = self.row(T_FILES, index as usize);
        let b = self.bytes;
        Ok(Some(FileRow {
            id: id_at(b, r),
            path: PackagePath::new(self.str_at("a file's path", le32(b, r + 32), le32(b, r + 36))?)
                .map_err(|why| ItemsRecordError::NotPackagePath { at: index as usize, why })?,
            shown: self.str_at("a file's shown path", le32(b, r + 40), le32(b, r + 44))?,
            len: le32(b, r + 48),
        }))
    }

    /// How many expansions made a text.
    pub fn expansions(&self) -> usize {
        self.counts[T_EXPANSIONS]
    }

    /// Expansion row `i`, in call-key order.
    pub fn expansion_at(&self, i: usize) -> Option<ExpansionRow> {
        (i < self.counts[T_EXPANSIONS]).then(|| {
            let r = self.row(T_EXPANSIONS, i);
            ExpansionRow { call: key_at(self.bytes, r), id: id_at(self.bytes, r + 32), len: le32(self.bytes, r + 64) }
        })
    }

    /// The expansion of `call`, when it made a text.
    pub fn expansion(&self, call: ItemKey) -> Option<ExpansionRow> {
        self.expansion_at(self.find(T_EXPANSIONS, call)?)
    }

    /// The whole text `at`, from `read`, checked against the digest its row holds the first
    /// time it is asked. A text that is not the one the record saw is refused
    /// ([`ItemsRecordError::Stale`]) every time it is asked.
    pub fn whole(&self, at: At, read: &dyn Fn(TextOf<'_>) -> Option<&'a str>) -> Result<&'a str, ItemsRecordError> {
        let (saw, ask, id_of, file, call): (ContentId, TextOf<'_>, fn(&str) -> ContentId, _, _) = match at {
            At::File(index) => {
                let row = self.file(index)?.ok_or(ItemsRecordError::Short { what: "a file row", at: index as usize })?;
                (row.id, TextOf::File { index, path: row.path }, crate::resolve::file_id, Some(index), None)
            }
            At::Expansion(call) => {
                let i = self.find(T_EXPANSIONS, call).ok_or(ItemsRecordError::Gone { krate: self.key, file: None, call: Some(call) })?;
                let row = self.expansion_at(i).ok_or(ItemsRecordError::Short { what: "an expansion row", at: i })?;
                (row.id, TextOf::Expansion { call }, expansion_id, None, Some(call))
            }
        };
        // The one check of the text, the first time it is asked. No borrow of the memo is held
        // across `read`.
        let known = self.seen.borrow().get(&at).copied();
        let seen = match known {
            Some(seen) => seen,
            None => {
                let seen = self.check(read(ask), saw, id_of);
                self.seen.borrow_mut().insert(at, seen);
                seen
            }
        };
        match seen {
            Seen::Fresh(text) => Ok(text),
            Seen::Stale(found) => Err(ItemsRecordError::Stale { krate: self.key, file, call, saw, found }),
            Seen::Gone => Err(ItemsRecordError::Gone { krate: self.key, file, call }),
        }
    }

    /// The one check of a text: its digest, by the row's domain, against the row's.
    fn check(&self, text: Option<&'a str>, saw: ContentId, id_of: fn(&str) -> ContentId) -> Seen<'a> {
        let Some(text) = text else { return Seen::Gone };
        let mut d = self.decoded.get();
        d.texts += 1;
        d.text_bytes += text.len();
        self.decoded.set(d);
        #[cfg(test)]
        if plant::is(plant::Plant::SkipDigest) {
            return Seen::Fresh(text);
        }
        let found = id_of(text);
        if found == saw { Seen::Fresh(text) } else { Seen::Stale(found) }
    }

    /// The stretch `span` of its text, the text checked as [`ItemsRecord::whole`] checks it.
    pub fn slice(&self, span: TextSpan, read: &dyn Fn(TextOf<'_>) -> Option<&'a str>) -> Result<&'a str, ItemsRecordError> {
        let text = self.whole(span.at, read)?;
        let (start, len) = (span.start as usize, span.len as usize);
        start
            .checked_add(len)
            .and_then(|end| text.get(start..end))
            .ok_or(ItemsRecordError::Range { krate: self.key, start: span.start, len: span.len })
    }

    /// How many items with text the crate has (question 24: the rows are a slice, by key).
    pub fn items(&self) -> usize {
        self.counts[T_ITEMS]
    }

    /// Item row `i`, in key order.
    pub fn item_at(&self, i: usize) -> Result<Option<ItemRow<'a>>, ItemsRecordError> {
        if i >= self.counts[T_ITEMS] {
            return Ok(None);
        }
        let (b, r) = (self.bytes, self.row(T_ITEMS, i));
        let key = key_at(b, r);
        let flags = b[r + 59];
        let code = le32(b, r + 36);
        let header_start = le32(b, r + 60);
        let impl_row = le32(b, r + 68);
        Ok(Some(ItemRow {
            key,
            kind: ItemKind::from_byte(b[r + 58]).ok_or(ItemsRecordError::Byte { what: "an item kind", byte: b[r + 58] })?,
            module: self.module_key(le32(b, r + 32))?,
            text: self.span(code, le32(b, r + 40), le32(b, r + 44))?,
            outline: ItemIndex(le32(b, r + 48)),
            name: if flags & NAMED != 0 { Some(self.str_at("an item's name", le32(b, r + 52), u32::from(le16(b, r + 56)))?) } else { None },
            body_holds_items: flags & BODY_HOLDS_ITEMS != 0,
            ctor: flags & CTOR_OR_WRITTEN_BY != 0,
            impl_header: if header_start == NONE { None } else { self.span(code, header_start, le32(b, r + 64))? },
            impl_row: (impl_row != NONE).then_some(impl_row),
            foreign: if flags & FOREIGN != 0 { Some(self.str_at("an item's block ABI", le32(b, r + 72), le32(b, r + 76))?) } else { None },
        }))
    }

    /// The item `key`: its kind, module, name and where its text is (questions 7 to 12);
    /// `Ok(None)` when the crate has no item with text under that key.
    pub fn item(&self, key: ItemKey) -> Result<Option<ItemRow<'a>>, ItemsRecordError> {
        match self.find(T_ITEMS, key) {
            Some(i) => self.item_at(i),
            None => Ok(None),
        }
    }

    /// How many impls the crate writes (question 14): the impl index of the crate is rows
    /// `0..impls()`, in (trait path, module, key) order, read in place.
    pub fn impls(&self) -> usize {
        self.counts[T_IMPLS]
    }

    /// Impl row `i`.
    pub fn impl_at(&self, i: usize) -> Result<Option<ImplRow<'a>>, ItemsRecordError> {
        if i >= self.counts[T_IMPLS] {
            return Ok(None);
        }
        let (b, r) = (self.bytes, self.row(T_IMPLS, i));
        let key = key_at(b, r);
        let marks = b[r + 57];
        let trait_path = self.str_at("an impl's trait path", le32(b, r + 36), le32(b, r + 40))?;
        let self_path = self.str_at("an impl's self path", le32(b, r + 44), le32(b, r + 48))?;
        let bits = le32(b, r + 52);
        let roots = le32(b, r + 60);
        if roots & !(ROOT_TRAIT | ROOT_SELF) != 0 {
            return Err(ItemsRecordError::Roots { key, bits: roots });
        }
        let root = |flag: u32, at: usize| (roots & flag != 0).then(|| CrateItemKey(id_at(b, r + at)));
        Ok(Some(ImplRow {
            key,
            module: self.module_key(le32(b, r + 32))?,
            trait_path: (!trait_path.is_empty()).then_some(trait_path),
            has_trait: marks & HAS_TRAIT != 0,
            bare: marks & BARE != 0,
            flags: ImplFlags::from_bits(bits).map_err(|_| ItemsRecordError::ImplFlags { key, bits })?,
            head: SelfHead::from_byte(b[r + 56], le16(b, r + 58)).ok_or(ItemsRecordError::Byte { what: "a self type's head", byte: b[r + 56] })?,
            self_path: (!self_path.is_empty()).then_some(self_path),
            header_unread: marks & HEADER_UNREAD != 0,
            trait_unread: marks & TRAIT_UNREAD != 0,
            trait_root: root(ROOT_TRAIT, 64),
            self_root: root(ROOT_SELF, 96),
        }))
    }

    /// How many lang items the crate declares (question 15).
    pub fn langs(&self) -> usize {
        self.counts[T_LANGS]
    }

    /// Lang row `i`, in (name, key) order: a name twice in a row is the lang item declared twice.
    pub fn lang_at(&self, i: usize) -> Result<Option<(&'a str, ItemKey)>, ItemsRecordError> {
        if i >= self.counts[T_LANGS] {
            return Ok(None);
        }
        let (b, r) = (self.bytes, self.row(T_LANGS, i));
        Ok(Some((self.str_at("a lang item's name", le32(b, r + 32), le32(b, r + 36))?, key_at(b, r))))
    }

    /// The item `#[lang = "name"]` declares in this crate: the first by key when the crate
    /// declares it twice ([`ItemsRecord::lang_at`] shows both).
    pub fn lang(&self, name: &str) -> Result<Option<ItemKey>, ItemsRecordError> {
        let (mut lo, mut hi) = (0usize, self.counts[T_LANGS]);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.lang_at(mid)? {
                Some((n, _)) if n.as_bytes() < name.as_bytes() => lo = mid + 1,
                _ => hi = mid,
            }
        }
        Ok(self.lang_at(lo)?.filter(|(n, _)| *n == name).map(|(_, k)| k))
    }

    /// How many macro definitions the crate has (question 21).
    pub fn macros(&self) -> usize {
        self.counts[T_MACROS]
    }

    /// Macro row `i`, in key order: [`ItemsRecord::macro_row`] of its key.
    pub fn macro_at(&self, i: usize) -> Result<Option<MacroRow<'a>>, ItemsRecordError> {
        if i >= self.counts[T_MACROS] {
            return Ok(None);
        }
        self.macro_row(key_at(self.bytes, self.row(T_MACROS, i)))
    }

    /// The macro definition `def` (question 21): its name, module, export marks and where its
    /// body is.
    pub fn macro_row(&self, def: ItemKey) -> Result<Option<MacroRow<'a>>, ItemsRecordError> {
        let Some(i) = self.find(T_MACROS, def) else { return Ok(None) };
        let (b, r) = (self.bytes, self.row(T_MACROS, i));
        let (kind, flags, sub) = (b[r + 64], b[r + 65], b[r + 66]);
        let code = le32(b, r + 44);
        let first = self.span(code, le32(b, r + 48), le32(b, r + 52))?;
        let (second_at, second_len) = (le32(b, r + 56), le32(b, r + 60));
        let body = match kind {
            BODY_RULES => MacroBodyAt::Rules(first),
            BODY_MACRO2 => MacroBodyAt::Macro2 { params: if flags & HAS_PARAMS != 0 { self.span(code, second_at, second_len)? } else { None }, body: first },
            BODY_BUILTIN_FN => MacroBodyAt::BuiltinFn(self.str_at("a builtin macro's name", second_at, second_len)?),
            BODY_BUILTIN_OTHER => MacroBodyAt::BuiltinOther(CallShape::from_byte(sub).ok_or(ItemsRecordError::Byte { what: "a macro call shape", byte: sub })?),
            BODY_PROC => MacroBodyAt::Proc {
                kind: ProcKind::from_byte(sub).ok_or(ItemsRecordError::Byte { what: "a proc macro kind", byte: sub })?,
                helpers: self.str_at("a proc macro's helpers", second_at, second_len)?,
            },
            BODY_BUILTIN_DERIVE => MacroBodyAt::BuiltinDerive { helpers: self.str_at("a builtin derive's helpers", second_at, second_len)? },
            other => return Err(ItemsRecordError::Byte { what: "a macro body kind", byte: other }),
        };
        Ok(Some(MacroRow {
            key: def,
            name: self.str_at("a macro's name", le32(b, r + 36), le32(b, r + 40))?,
            module: self.module_key(le32(b, r + 32))?,
            export: flags & EXPORT != 0,
            local_inner: flags & LOCAL_INNER != 0,
            body,
        }))
    }

    /// The macro definition `def` as the solve's own [`MacroSrc`], its body's text read (and
    /// checked) through `read`: what [`crate::resolve::definition_of`] takes.
    pub fn macro_src(&self, def: ItemKey, read: &dyn Fn(TextOf<'_>) -> Option<&'a str>) -> Result<Option<MacroSrc<'a>>, ItemsRecordError> {
        let Some(row) = self.macro_row(def)? else { return Ok(None) };
        let text = |span: Option<TextSpan>| self.slice(span.ok_or(ItemsRecordError::NoPlace { key: def })?, read);
        let body = match row.body {
            MacroBodyAt::Rules(body) => MacroBody::Rules(text(body)?),
            MacroBodyAt::Macro2 { params, body } => MacroBody::Macro2 {
                params: match params {
                    Some(p) => Some(text(Some(p))?),
                    None => None,
                },
                body: text(body)?,
            },
            MacroBodyAt::BuiltinFn(name) => MacroBody::BuiltinFn(name),
            MacroBodyAt::BuiltinOther(shape) => MacroBody::BuiltinOther(shape),
            MacroBodyAt::Proc { kind, helpers } => MacroBody::Proc { kind, helpers },
            MacroBodyAt::BuiltinDerive { helpers } => MacroBody::BuiltinDerive { helpers },
        };
        Ok(Some(MacroSrc { name: row.name, body, module: row.module, export: row.export, local_inner: row.local_inner }))
    }

    /// How many module rows the record has (listed or not).
    pub fn modules(&self) -> usize {
        self.counts[T_MODULES]
    }

    /// Module row `i`, in key order.
    pub fn module_at(&self, i: usize) -> Result<Option<ModuleRow<'a>>, ItemsRecordError> {
        if i >= self.counts[T_MODULES] {
            return Ok(None);
        }
        let (b, r) = (self.bytes, self.row(T_MODULES, i));
        let (parent, flags) = (le32(b, r + 32), le32(b, r + 44));
        Ok(Some(ModuleRow {
            key: key_at(b, r),
            parent: if parent == NONE { None } else { Some(self.module_key(parent)?) },
            path: if flags & HAS_PATH != 0 { Some(self.str_at("a module's path", le32(b, r + 36), le32(b, r + 40))?) } else { None },
            listed: flags & LISTED != 0,
            made: flags & MADE != 0,
        }))
    }

    /// The module `key`: its parent and its path `krate::a::b` (question 20).
    pub fn module(&self, key: ItemKey) -> Result<Option<ModuleRow<'a>>, ItemsRecordError> {
        match self.find(T_MODULES, key) {
            Some(i) => self.module_at(i),
            None => Ok(None),
        }
    }

    /// The cfg atoms the crate's text was lowered under, sorted as `Cfg::atoms` gives them
    /// (question 6), decoded on first ask.
    pub fn cfg_atoms(&self) -> Result<&[(&'a str, Option<&'a str>)], ItemsRecordError> {
        if let Some(atoms) = self.atoms.get() {
            return Ok(atoms.as_slice());
        }
        let b = self.bytes;
        let mut atoms = AVec::with_capacity_in(self.counts[T_ATOMS], self.alloc);
        for i in 0..self.counts[T_ATOMS] {
            let r = self.row(T_ATOMS, i);
            let name = self.str_at("a cfg atom's name", le32(b, r), le32(b, r + 4))?;
            let value_at = le32(b, r + 8);
            let value = if value_at == NONE { None } else { Some(self.str_at("a cfg atom's value", value_at, le32(b, r + 12))?) };
            atoms.push((name, value));
        }
        Ok(self.atoms.get_or_init(|| atoms).as_slice())
    }
}

/// Where a reader gets a crate's items record and the texts its rows point into: through the
/// caller's effect layer (a kept file, the store), the first time each is asked.
pub trait LoadItems<'a> {
    /// The items record of the crate at `krate` (an index of the keys the reader was given),
    /// 16-aligned, or `None` when none is kept.
    fn load(&self, krate: usize) -> Option<&'a [u8]>;

    /// The text of the crate's file at `path` in its package, as it is now: the loader joins
    /// `path` onto the directory its world placed the crate's package at
    /// ([`PackagePath::in_package`]). The record checks the text against the digest it holds,
    /// so a loader need not.
    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'a str>;

    /// The text the macro call `call` of the crate expanded to, from the expansion's own
    /// record.
    fn expansion(&self, krate: usize, call: ItemKey) -> Option<&'a str>;

    /// The entry of the crate's file at `path`, as it is now, whose item chain is `chain`
    /// ([`grain::item_chain`]): what a row whose file changed is checked against, item by item
    /// ([`ItemsRecord::item_now`]). This one outlines the file's text now in `alloc`; a loader
    /// that holds the file's `FileItems` record answers from that instead, with equal entries.
    fn item(&self, krate: usize, path: PackagePath<'_>, chain: ContentId, edition: Edition, alloc: Alloc<'a>) -> Option<ItemToday<'a>> {
        grain::outline_today(self.file(krate, path)?, chain, edition, alloc)
    }
}

/// The items of a set of crates, through their items records, each crate opened the first time
/// something asks about it: what the front end asks in place of each crate's [`CrateNames`]
/// tables. A question about a key of no crate given, or of a crate with no record kept, is
/// answered `Ok(None)`; a record that does not read, a text that is gone and a text that
/// changed are typed errors.
pub struct RecordItems<'a, L> {
    keys: AVec<'a, CrateItemKey>,
    by_prefix: HashMap<[u8; 8], usize, Alloc<'a>>,
    open: AVec<'a, OnceCell<Option<ItemsRecord<'a>>>>,
    load: L,
    alloc: Alloc<'a>,
}

impl<'a, L: LoadItems<'a>> RecordItems<'a, L> {
    /// `keys`: each crate by index, as `load` knows them. Nothing is read here.
    pub fn new(keys: &[CrateItemKey], load: L, alloc: Alloc<'a>) -> RecordItems<'a, L> {
        let mut by_prefix = HashMap::new_in(alloc);
        for (i, k) in keys.iter().enumerate() {
            by_prefix.insert(k.prefix(), i);
        }
        let mut open = AVec::with_capacity_in(keys.len(), alloc);
        open.resize_with(keys.len(), OnceCell::new);
        RecordItems { keys: AVec::from_slice_in(keys, alloc), by_prefix, open, load, alloc }
    }

    /// The crate whose key `key` (any item of it) begins with (question 4): no record is opened.
    pub fn crate_of(&self, key: ItemKey) -> Option<usize> {
        let mut p = [0u8; 8];
        p.copy_from_slice(&key.0.0[..8]);
        self.by_prefix.get(&p).copied()
    }

    /// The crate's record, opened on first ask. A record that does not open is a writer's bug
    /// (its bytes were verified when fetched): it fails this command, naming the crate.
    pub fn record(&self, krate: usize) -> Option<&ItemsRecord<'a>> {
        self.open
            .get(krate)?
            .get_or_init(|| {
                let bytes = self.load.load(krate)?;
                Some(ItemsRecord::open(bytes, self.alloc).unwrap_or_else(|e| panic!("the items record of crate {:?} did not open: {e:?}", self.keys[krate])))
            })
            .as_ref()
    }

    /// What was decoded so far, summed over the crates opened.
    pub fn decoded(&self) -> ItemsDecoded {
        let mut sum = ItemsDecoded::default();
        for r in self.open.iter().filter_map(|c| c.get()).flatten() {
            let d = r.decoded();
            sum.crates += d.crates;
            sum.bytes += d.bytes;
            sum.texts += d.texts;
            sum.text_bytes += d.text_bytes;
        }
        sum
    }

    fn of(&self, key: ItemKey) -> Option<(usize, &ItemsRecord<'a>)> {
        let krate = self.crate_of(key)?;
        Some((krate, self.record(krate)?))
    }

    /// The item `key` (questions 7 to 12): [`ItemsRecord::item`] of its crate.
    pub fn item(&self, key: ItemKey) -> Result<Option<ItemRow<'a>>, ItemsRecordError> {
        match self.of(key) {
            Some((_, r)) => r.item(key),
            None => Ok(None),
        }
    }

    /// The module an item is declared in (question 8): `Names::module_of`.
    pub fn module_of(&self, item: ItemKey) -> Result<Option<ItemKey>, ItemsRecordError> {
        Ok(self.item(item)?.map(|row| row.module))
    }

    /// The stretch `span` of a text of crate `krate`, the text asked of the loader and checked
    /// against the record's digest once.
    pub fn text(&self, krate: usize, span: TextSpan) -> Result<&'a str, ItemsRecordError> {
        let record = self.record(krate).ok_or(ItemsRecordError::Short { what: "a crate with an items record", at: krate })?;
        record.slice(span, &|which| self.read(krate, which))
    }

    /// The whole text `at` of crate `krate` (question 19: a row of the front end's text table).
    pub fn whole(&self, krate: usize, at: At) -> Result<&'a str, ItemsRecordError> {
        let record = self.record(krate).ok_or(ItemsRecordError::Short { what: "a crate with an items record", at: krate })?;
        record.whole(at, &|which| self.read(krate, which))
    }

    fn read(&self, krate: usize, which: TextOf<'_>) -> Option<&'a str> {
        match which {
            TextOf::File { path, .. } => self.load.file(krate, path),
            TextOf::Expansion { call } => self.load.expansion(krate, call),
        }
    }

    /// An item's whole text (question 7): `Names::item_text(key).text`. `Ok(None)` for no such
    /// item; an item the record has no place for is [`ItemsRecordError::NoPlace`].
    pub fn item_text(&self, key: ItemKey) -> Result<Option<&'a str>, ItemsRecordError> {
        let Some((krate, r)) = self.of(key) else { return Ok(None) };
        let Some(row) = r.item(key)? else { return Ok(None) };
        self.text(krate, row.text.ok_or(ItemsRecordError::NoPlace { key })?).map(Some)
    }

    /// A trait's or an impl's members (question 13): `Names::members`, read in place.
    pub fn members(&self, owner: ItemKey) -> Result<Option<Members<'_, 'a>>, ItemsRecordError> {
        match self.of(owner) {
            Some((_, r)) => r.members(owner),
            None => Ok(None),
        }
    }

    /// A member's whole text; `owner` says which crate.
    pub fn member_text(&self, owner: ItemKey, member: &MemberRow<'a>) -> Result<&'a str, ItemsRecordError> {
        let krate = self.crate_of(owner).ok_or(ItemsRecordError::NoPlace { key: member.key })?;
        self.text(krate, member.text.ok_or(ItemsRecordError::NoPlace { key: member.key })?)
    }

    /// The macro definition `def` (question 21): `Names::macro_src`.
    pub fn macro_src(&self, def: ItemKey) -> Result<Option<MacroSrc<'a>>, ItemsRecordError> {
        match self.of(def) {
            Some((krate, r)) => r.macro_src(def, &|which| self.read(krate, which)),
            None => Ok(None),
        }
    }

    /// The module `key` (question 20).
    pub fn module(&self, key: ItemKey) -> Result<Option<ModuleRow<'a>>, ItemsRecordError> {
        match self.of(key) {
            Some((_, r)) => r.module(key),
            None => Ok(None),
        }
    }
}
