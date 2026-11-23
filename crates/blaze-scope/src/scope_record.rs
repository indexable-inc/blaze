//! A crate's `CrateScope` record, read in place (bottom.md section 6, "Per-crate entry"; D114
//! rule 1, a record is its own index; rule 74, verify once).
//!
//! A dependent reads of a dependency exactly what [`crate::resolve`]'s `Built` serves: its key,
//! root module, edition and proc-macro key; a module's finished scope and chain; an enum's
//! variants; a macro's source; and `reaches`, every other crate its scopes name an item of. The
//! record is laid out so that each of those is a binary search of fixed-width rows of the
//! mapped bytes: nothing is decoded, copied or allocated to open it or to answer from it.
//!
//! The bytes (integers little-endian; every section starts at a multiple of 16 after the one
//! before it, the gap zero):
//! - a 192-byte header: `BZCS`, version u32, crate key, root key, edition u8, has-proc u8,
//!   errored u8 ([`ERRORED`]), a zero byte, the proc key (zeros without), the crate's name (at
//!   u32 and length u32 into the strings), then for each of the eight sections below its count
//!   u32 (bytes, for the strings and the blob) and its offset u32; zeros to 192;
//! - reaches: crate keys, strictly increasing;
//! - module rows, 48 bytes, strictly increasing by key: key, scope at u32 into the blob (each
//!   scope where the one before ends, rounded up to 16) and length u32, first chain key u32
//!   (`u32::MAX`: no chain) and chain length u32;
//! - chain keys, 32 bytes each: each module's run right after the one before it;
//! - enum rows, 40 bytes, strictly increasing by owner: owner, first variant u32, count u32;
//! - variant rows, 48 bytes, the names record's form (one reader for both,
//!   [`crate::names_record::Variants`]): key, name at u32 and length u32, record u8, 7 zeros;
//! - macro rows, 96 bytes, strictly increasing by definition: definition, module, name at u32
//!   and length u32, the body's first and second texts (at u32, length u32; (0, 0) unused),
//!   export u8, local-inner u8, body tag u8 (0 `macro_rules!`, 1 a 2.0 `macro`, 2 a builtin
//!   function, 3 a builtin attribute, 4 a proc macro, 5 a builtin derive), extra u8 (a 2.0
//!   macro's has-params, a call shape, a proc-macro kind; else 0), 4 zeros;
//! - the strings: the name, variant names and macro texts, each range checked UTF-8 once;
//! - the blob: each module's blaze-surface `ModuleScope` record.
//!
//! Sorted rows and a binary search, not a hash table: no seed and no machine in the bytes (rule
//! 25: the bytes are a function of the content alone, so two builders of one key write equal
//! bytes), no hash an attacker can collide in a record another builder wrote, and a lookup is
//! log2(rows) compares of 32 bytes on the pages the search touches (11 for a crate of 2,000
//! modules), against which a hash's one probe saves nothing a solve can see. Every byte has the
//! one writer's value, and [`Verified::check`] refuses any other arrangement (a pad not zero, a
//! section not where the one before ends, a row out of order, a range a byte long too many).
//!
//! [`Verified::check`] is the one maker of the proof every reader takes, O(the record) with no
//! allocation, made once where the bytes enter the process; [`DepRecord::open`] of a proof is
//! O(1). An exports record's scopes carry no textual scope and no marks (another crate reads a
//! module's names, never its `macro_rules!` order: crate_scope.rs `write_records`), and the check
//! refuses one that does, so [`ExportsScope`] answers those reads empty exactly as the scope does.

#![blaze::sans_io]

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{Binding, BuildError, CallShape, Edition, Globs, Namespace, ProcKind, RecordBytes, ScopeBuilder, ScopeView, TextualDef};

use crate::crate_scope::RecordRead;
use crate::facts::Name;
use crate::names::{VariantHit, variant_in};
use crate::names_record::{VARIANT_ROW_WIDTH, Variants, find_row, key_at, le32};
use crate::task::{MacroBody, MacroSrc, Variant};

pub(crate) const MAGIC: [u8; 4] = *b"BZCS";
// 2: a body block holding a statement-position macro call is a block module
// (macro-block-modules-1, 2026-11-16): block ordinals and made modules change.
// 3: a built-in derive is its own macro body (tag 5) with its `attributes(..)` text
// (names-attr-4); a version-2 record is refused (`NotThisRecord`) and the crate solved again.
// 4: a written `self`/`super` and `pub(self)`/`pub(super)`/private in a block's module count
// from the module around the body (super-in-block-1, 2026-11-16): block modules' scopes change.
// 5: read in place (2026-11-21): a fixed header of section offsets, fixed-width sorted rows and
// one strings section in place of the length-prefixed stream; a version-4 record is refused
// (`NotThisRecord`), so every home solves each crate once more and writes this form.
pub(crate) const VERSION: u32 = 5;
pub(crate) const ALIGN: usize = 16;
/// The errored flag of an error crate's record (crate_scope.rs `error_record`); 0 in any other.
pub(crate) const ERRORED: u8 = 1;
/// Where the errored flag sits: magic, version, crate key, root key, edition, has-proc.
pub(crate) const ERRORED_AT: usize = 4 + 4 + 32 + 32 + 1 + 1;

const HEADER: usize = 192;
const KEY: usize = 32;
const NONE: u32 = u32::MAX;
const EDITION_AT: usize = 72;
const HAS_PROC_AT: usize = 73;
const PROC_AT: usize = 76;
const NAME_AT: usize = 108;
const SECTIONS_AT: usize = 116;
const MODULE_ROW: usize = 48;
const ENUM_ROW: usize = 40;
const VARIANT_ROW: usize = VARIANT_ROW_WIDTH;
const MACRO_ROW: usize = 96;

const REACHES: usize = 0;
const MODULES: usize = 1;
const CHAINS: usize = 2;
const ENUMS: usize = 3;
const VARIANTS: usize = 4;
const MACROS: usize = 5;
const STRINGS: usize = 6;
const BLOB: usize = 7;
/// The sections after the header, in their order in the bytes: what a refusal names each, and
/// its row's width (1: bytes).
const SECTIONS: [(&str, usize); 8] = [
    ("reaches", KEY),
    ("module rows", MODULE_ROW),
    ("chain keys", KEY),
    ("enum rows", ENUM_ROW),
    ("variant rows", VARIANT_ROW),
    ("macro rows", MACRO_ROW),
    ("strings", 1),
    ("blob", 1),
];

/// The field of section `i`'s count; its offset is the next four bytes.
const fn field_of(section: usize) -> usize {
    SECTIONS_AT + 8 * section
}

// ---------------------------------------------------------------------------------------------
// Writing.

/// One module of a record: its key, its `ModuleScope` record's bytes, and its chain up to the
/// crate's root (itself first) when it has one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Module<'p> {
    pub(crate) key: ItemKey,
    pub(crate) scope: &'p [u8],
    pub(crate) chain: Option<&'p [ItemKey]>,
}

/// What one `CrateScope` record holds. The tables in any order: [`write`] sorts them, so the
/// bytes are a function of the content alone (rule 25).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Parts<'p> {
    pub(crate) key: CrateItemKey,
    pub(crate) root: ItemKey,
    pub(crate) edition: Edition,
    pub(crate) proc_key: Option<CrateItemKey>,
    pub(crate) errored: bool,
    pub(crate) name: &'p str,
    /// A set: written sorted, once each.
    pub(crate) reaches: &'p [CrateItemKey],
    pub(crate) modules: &'p [Module<'p>],
    /// Each enum's variants, in declaration order (the order is the enum's).
    pub(crate) enums: &'p [(ItemKey, &'p [Variant<&'p str>])],
    pub(crate) macros: &'p [(ItemKey, MacroSrc<'p>)],
}

fn u32_of(n: usize, table: &'static str) -> Result<u32, BuildError> {
    u32::try_from(n).map_err(|_| BuildError::TooLarge { table })
}

fn word(out: &mut RecordBytes<'_>, n: usize, table: &'static str) -> Result<(), BuildError> {
    out.extend_from_slice(&u32_of(n, table)?.to_le_bytes());
    Ok(())
}

fn pad(out: &mut RecordBytes<'_>) {
    let to = out.len().next_multiple_of(ALIGN);
    out.resize(to, 0);
}

/// `s` appended to the strings: its (at, length).
fn put(strings: &mut RecordBytes<'_>, s: &str) -> Result<(u32, u32), BuildError> {
    let at = u32_of(strings.len(), "CrateScope strings")?;
    let len = u32_of(s.len(), "a CrateScope string")?;
    strings.extend_from_slice(s.as_bytes());
    Ok((at, len))
}

/// The indices `0..n` in increasing order of `key`; two equal keys are [`BuildError::Twice`].
fn order<'b>(n: usize, key: impl Fn(usize) -> ItemKey, table: &'static str, alloc: Alloc<'b>) -> Result<AVec<'b, usize>, BuildError> {
    let mut order = AVec::from_iter_in(0..n, alloc);
    order.sort_unstable_by_key(|&i| key(i));
    if let Some(w) = order.windows(2).find(|w| key(w[0]) == key(w[1])) {
        return Err(BuildError::Twice { table, key: key(w[1]).0 });
    }
    Ok(order)
}

/// The record of `parts`, its working buffers and its bytes in `alloc`.
pub(crate) fn write<'b>(parts: &Parts<'_>, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, BuildError> {
    let mut reaches = AVec::from_slice_in(parts.reaches, alloc);
    reaches.sort_unstable();
    reaches.dedup();
    let modules = order(parts.modules.len(), |i| parts.modules[i].key, "CrateScope modules", alloc)?;
    let enums = order(parts.enums.len(), |i| parts.enums[i].0, "CrateScope enums", alloc)?;
    let macros = order(parts.macros.len(), |i| parts.macros[i].0, "CrateScope macros", alloc)?;

    let mut strings = RecordBytes::new_in(alloc);
    let name = put(&mut strings, parts.name)?;

    let mut reach_keys = RecordBytes::with_capacity_in(reaches.len() * KEY, alloc);
    for k in reaches.iter() {
        reach_keys.extend_from_slice(&k.0.0);
    }

    let mut module_rows = RecordBytes::with_capacity_in(modules.len() * MODULE_ROW, alloc);
    let mut chain_keys = RecordBytes::new_in(alloc);
    let mut blob = RecordBytes::new_in(alloc);
    let mut chains = 0usize;
    for &i in modules.iter() {
        let m = &parts.modules[i];
        module_rows.extend_from_slice(&m.key.0.0);
        word(&mut module_rows, blob.len(), "CrateScope blob")?;
        word(&mut module_rows, m.scope.len(), "a module scope")?;
        blob.extend_from_slice(m.scope);
        pad(&mut blob);
        match m.chain {
            Some(chain) => {
                word(&mut module_rows, chains, "CrateScope chain keys")?;
                word(&mut module_rows, chain.len(), "a module's chain")?;
                for k in chain {
                    chain_keys.extend_from_slice(&k.0.0);
                }
                chains += chain.len();
            }
            None => {
                module_rows.extend_from_slice(&NONE.to_le_bytes());
                module_rows.extend_from_slice(&0u32.to_le_bytes());
            }
        }
    }

    let mut enum_rows = RecordBytes::with_capacity_in(enums.len() * ENUM_ROW, alloc);
    let mut variant_rows = RecordBytes::new_in(alloc);
    let mut variants = 0usize;
    for &i in enums.iter() {
        let (owner, list) = parts.enums[i];
        enum_rows.extend_from_slice(&owner.0.0);
        word(&mut enum_rows, variants, "CrateScope variants")?;
        word(&mut enum_rows, list.len(), "an enum's variants")?;
        for v in list {
            variant_rows.extend_from_slice(&v.key.0.0);
            let (at, len) = put(&mut strings, v.name)?;
            variant_rows.extend_from_slice(&at.to_le_bytes());
            variant_rows.extend_from_slice(&len.to_le_bytes());
            variant_rows.extend_from_slice(&[u8::from(v.record), 0, 0, 0, 0, 0, 0, 0]);
        }
        variants += list.len();
    }

    let mut macro_rows = RecordBytes::with_capacity_in(macros.len() * MACRO_ROW, alloc);
    for &i in macros.iter() {
        let (def, m) = parts.macros[i];
        let name = put(&mut strings, m.name)?;
        let none = (0u32, 0u32);
        let (tag, extra, a, b) = match m.body {
            MacroBody::Rules(text) => (0u8, 0u8, put(&mut strings, text)?, none),
            MacroBody::Macro2 { params, body } => {
                let a = match params {
                    Some(p) => put(&mut strings, p)?,
                    None => none,
                };
                (1, u8::from(params.is_some()), a, put(&mut strings, body)?)
            }
            MacroBody::BuiltinFn(text) => (2, 0, put(&mut strings, text)?, none),
            MacroBody::BuiltinOther(shape) => (3, shape.byte(), none, none),
            MacroBody::Proc { kind, helpers } => (4, kind.byte(), put(&mut strings, helpers)?, none),
            MacroBody::BuiltinDerive { helpers } => (5, 0, put(&mut strings, helpers)?, none),
        };
        macro_rows.extend_from_slice(&def.0.0);
        macro_rows.extend_from_slice(&m.module.0.0);
        for w in [name.0, name.1, a.0, a.1, b.0, b.1] {
            macro_rows.extend_from_slice(&w.to_le_bytes());
        }
        macro_rows.extend_from_slice(&[u8::from(m.export), u8::from(m.local_inner), tag, extra, 0, 0, 0, 0]);
    }

    let sections: [(&[u8], usize); 8] = [
        (&reach_keys, reaches.len()),
        (&module_rows, modules.len()),
        (&chain_keys, chains),
        (&enum_rows, enums.len()),
        (&variant_rows, variants),
        (&macro_rows, macros.len()),
        (&strings, strings.len()),
        (&blob, blob.len()),
    ];
    let mut at = [0usize; 8];
    let mut end = HEADER;
    for (i, (bytes, _)) in sections.iter().enumerate() {
        at[i] = end.next_multiple_of(ALIGN);
        end = at[i] + bytes.len();
    }
    let mut out = RecordBytes::with_capacity_in(end, alloc);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&parts.key.0.0);
    out.extend_from_slice(&parts.root.0.0);
    out.extend_from_slice(&[parts.edition.byte(), u8::from(parts.proc_key.is_some()), if parts.errored { ERRORED } else { 0 }, 0]);
    out.extend_from_slice(&parts.proc_key.map_or([0; KEY], |k| k.0.0));
    out.extend_from_slice(&name.0.to_le_bytes());
    out.extend_from_slice(&name.1.to_le_bytes());
    for (i, (_, count)) in sections.iter().enumerate() {
        word(&mut out, *count, SECTIONS[i].0)?;
        word(&mut out, at[i], "CrateScope offsets")?;
    }
    out.resize(HEADER, 0);
    for (i, (bytes, _)) in sections.iter().enumerate() {
        out.resize(at[i], 0);
        out.extend_from_slice(bytes);
    }
    debug_assert_eq!(out.len(), end);
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// The head, read in place before any check.

/// Whether `bytes` start as a record of this form and version.
fn is_this(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER && bytes[..4] == MAGIC && le32(bytes, 4) == VERSION
}

/// Whether `bytes`, a record of this version, is an error crate's: one byte of its head.
pub(crate) fn is_errored(bytes: &[u8]) -> bool {
    is_this(bytes) && bytes[ERRORED_AT] == ERRORED
}

/// A record's crate key and reach list (32 bytes each, as held), read from its head in place
/// and bounds-checked, nothing else: what a caller asks for before it hands the records
/// (crate_scope.rs `unmet_reaches`). The bytes need no alignment.
pub(crate) fn peek_reaches(bytes: &[u8]) -> Result<(CrateItemKey, &[[u8; KEY]]), RecordRead> {
    if !is_this(bytes) {
        return Err(RecordRead::NotThisRecord);
    }
    let field = field_of(REACHES);
    let (count, at) = (le32(bytes, field) as usize, le32(bytes, field + 4) as usize);
    let keys = count.checked_mul(KEY).and_then(|len| bytes.get(at..at.checked_add(len)?)).ok_or(RecordRead::Past { table: "reaches", at: field })?;
    Ok((CrateItemKey(key_at(bytes, 8).0), keys.as_chunks::<KEY>().0))
}

/// A record's crate key and name, read from its head in place and bounds-checked (the name's
/// bytes checked UTF-8): `None` for bytes too short or not of this form. No alignment needed.
pub(crate) fn peek_name(bytes: &[u8]) -> Option<(CrateItemKey, &str)> {
    if !is_this(bytes) {
        return None;
    }
    let strings = le32(bytes, field_of(STRINGS) + 4) as usize;
    let (at, len) = (le32(bytes, NAME_AT) as usize, le32(bytes, NAME_AT + 4) as usize);
    let start = strings.checked_add(at)?;
    let name = core::str::from_utf8(bytes.get(start..start.checked_add(len)?)?).ok()?;
    Some((CrateItemKey(key_at(bytes, 8).0), name))
}

/// What a record's header says, each section checked to start where the one before it ends
/// (rounded up to 16) and to end inside the bytes.
#[derive(Clone, Copy, Debug)]
struct Head {
    key: CrateItemKey,
    root: ItemKey,
    edition: Edition,
    proc_key: Option<CrateItemKey>,
    errored: bool,
    /// The crate's name: (at, length) in the strings.
    name: (usize, usize),
    /// Each section's (count, offset).
    sections: [(usize, usize); 8],
}

/// The bytes of `range` all zero, or the first that is not, named `what`.
fn zeros(bytes: &[u8], range: core::ops::Range<usize>, what: &'static str) -> Result<(), RecordRead> {
    match bytes[range.start..range.end].iter().position(|b| *b != 0) {
        Some(i) => Err(RecordRead::BadByte { at: range.start + i, what }),
        None => Ok(()),
    }
}

/// The byte at `at` as a flag: 0 or 1, nothing else.
fn flag(bytes: &[u8], at: usize, what: &'static str) -> Result<bool, RecordRead> {
    match bytes[at] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(RecordRead::BadByte { at, what }),
    }
}

impl Head {
    /// The header of `bytes` (16-aligned), every section's place checked: O(1).
    fn read(bytes: &[u8]) -> Result<Head, RecordRead> {
        if !(bytes.as_ptr() as usize).is_multiple_of(ALIGN) {
            return Err(RecordRead::Unaligned);
        }
        if !is_this(bytes) {
            return Err(RecordRead::NotThisRecord);
        }
        let edition = Edition::from_byte(bytes[EDITION_AT]).ok_or(RecordRead::BadByte { at: EDITION_AT, what: "edition" })?;
        let has_proc = flag(bytes, HAS_PROC_AT, "has-proc flag")?;
        let errored = match bytes[ERRORED_AT] {
            0 => false,
            ERRORED => true,
            _ => return Err(RecordRead::BadByte { at: ERRORED_AT, what: "errored flag" }),
        };
        zeros(bytes, ERRORED_AT + 1..PROC_AT, "header pad")?;
        if !has_proc {
            zeros(bytes, PROC_AT..PROC_AT + KEY, "proc key of a crate with none")?;
        }
        zeros(bytes, field_of(SECTIONS.len())..HEADER, "header pad")?;
        let mut sections = [(0usize, 0usize); 8];
        let mut end = HEADER;
        for (i, &(table, width)) in SECTIONS.iter().enumerate() {
            let field = field_of(i);
            let (count, at) = (le32(bytes, field) as usize, le32(bytes, field + 4) as usize);
            if at != end.next_multiple_of(ALIGN) {
                // The section before ends elsewhere: its count (or this offset) is not the
                // writer's. Named by the section whose rows end there, at its count's field.
                return Err(match i {
                    0 => RecordRead::Layout { table: "header", next: table, at: field + 4 },
                    _ => RecordRead::Layout { table: SECTIONS[i - 1].0, next: table, at: field_of(i - 1) },
                });
            }
            let next = count.checked_mul(width).and_then(|len| at.checked_add(len)).filter(|e| *e <= bytes.len()).ok_or(RecordRead::Past { table, at: field })?;
            zeros(bytes, end..at, "section pad")?;
            end = next;
            sections[i] = (count, at);
        }
        if end != bytes.len() {
            return Err(RecordRead::Layout { table: "blob", next: "the record's end", at: field_of(BLOB) });
        }
        let name = (le32(bytes, NAME_AT) as usize, le32(bytes, NAME_AT + 4) as usize);
        if name.0.checked_add(name.1).is_none_or(|e| e > sections[STRINGS].0) {
            return Err(RecordRead::Past { table: "crate name", at: NAME_AT });
        }
        let proc = CrateItemKey(key_at(bytes, PROC_AT).0);
        Ok(Head { key: CrateItemKey(key_at(bytes, 8).0), root: key_at(bytes, 40), edition, proc_key: has_proc.then_some(proc), errored, name, sections })
    }
}

// ---------------------------------------------------------------------------------------------
// The check, once.

/// Which record a check is of: a crate's exports (what a dependent reads: its scopes hold no
/// textual scope and no marks) or its inner record (the rest: they may).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Holds {
    Exports,
    Inner,
}

/// Each row of `section` keyed above the one before it, which its search relies on.
fn increasing(bytes: &[u8], head: &Head, section: usize) -> Result<(), RecordRead> {
    let (table, width) = SECTIONS[section];
    let (count, at) = head.sections[section];
    for i in 1..count {
        let (a, b) = (at + (i - 1) * width, at + i * width);
        if bytes[a..a + KEY] >= bytes[b..b + KEY] {
            return Err(RecordRead::OutOfOrder { table, at: b });
        }
    }
    Ok(())
}

/// `bytes` checked whole: O(the record), no allocation. Every range a row or the header names
/// lies inside what holds it and every text range is UTF-8; every keyed table is strictly
/// increasing; every module's `ModuleScope` record reads by its own check ([`ScopeView::read`])
/// and is that module's; each run (scopes, chains, variants) starts where the one before ends;
/// every flag, tag and pad byte is the writer's.
fn check(bytes: &[u8], holds: Holds) -> Result<Head, RecordRead> {
    let head = Head::read(bytes)?;
    let (strings_len, strings_at) = head.sections[STRINGS];
    // The texts in the writer's order (the name, each enum's variants, each macro's name and
    // texts), each where the one before ends, so the strings are covered exactly once: a
    // strings count a byte long too many is refused, not read as a zero nothing names.
    let strings_end = core::cell::Cell::new(0usize);
    let text = |table: &'static str, field: usize, at: usize, len: usize| -> Result<(), RecordRead> {
        if at != strings_end.get() {
            return Err(RecordRead::Misplaced { table, at: field });
        }
        let end = at.checked_add(len).filter(|e| *e <= strings_len).ok_or(RecordRead::Past { table, at: field + 4 })?;
        strings_end.set(end);
        core::str::from_utf8(&bytes[strings_at + at..strings_at + end]).map(|_| ()).map_err(|_| RecordRead::Utf8 { table, at: field })
    };
    text("crate name", NAME_AT, head.name.0, head.name.1)?;
    for section in [REACHES, MODULES, ENUMS, MACROS] {
        increasing(bytes, &head, section)?;
    }

    let (blob_len, blob_at) = head.sections[BLOB];
    let (modules, modules_at) = head.sections[MODULES];
    let chain_keys = head.sections[CHAINS].0;
    let (mut scope_end, mut chain_end) = (0usize, 0usize);
    for i in 0..modules {
        let row = modules_at + i * MODULE_ROW;
        let module = key_at(bytes, row);
        let (at, len) = (le32(bytes, row + 32) as usize, le32(bytes, row + 36) as usize);
        if at != scope_end {
            return Err(RecordRead::Misplaced { table: "module scope", at: row + 32 });
        }
        let end = at.checked_add(len).filter(|e| *e <= blob_len).ok_or(RecordRead::Past { table: "module scope", at: row + 36 })?;
        scope_end = end.next_multiple_of(ALIGN);
        if scope_end > blob_len {
            return Err(RecordRead::Past { table: "module scope", at: row + 36 });
        }
        zeros(bytes, blob_at + end..blob_at + scope_end, "module scope pad")?;
        let view = ScopeView::read(&bytes[blob_at + at..blob_at + end]).map_err(|error| RecordRead::Scope { module, error })?;
        if view.module() != module {
            return Err(RecordRead::NotItsScope { module, found: view.module() });
        }
        if holds == Holds::Exports && (view.textual().next().is_some() || view.marks().next().is_some()) {
            return Err(RecordRead::NotExports { module });
        }
        let (first, n) = (le32(bytes, row + 40), le32(bytes, row + 44) as usize);
        if first == NONE {
            if n != 0 {
                return Err(RecordRead::BadByte { at: row + 44, what: "chain length of a module with no chain" });
            }
        } else {
            if first as usize != chain_end {
                return Err(RecordRead::Misplaced { table: "chain keys", at: row + 40 });
            }
            chain_end = chain_end.checked_add(n).filter(|e| *e <= chain_keys).ok_or(RecordRead::Past { table: "chain keys", at: row + 44 })?;
        }
    }
    if scope_end != blob_len {
        return Err(RecordRead::Layout { table: "module rows", next: "blob", at: field_of(BLOB) });
    }
    if chain_end != chain_keys {
        return Err(RecordRead::Layout { table: "module rows", next: "chain keys", at: field_of(CHAINS) });
    }

    let (enums, enums_at) = head.sections[ENUMS];
    let (variants, variants_at) = head.sections[VARIANTS];
    let mut variant_end = 0usize;
    for i in 0..enums {
        let row = enums_at + i * ENUM_ROW;
        let (first, count) = (le32(bytes, row + 32) as usize, le32(bytes, row + 36) as usize);
        if first != variant_end {
            return Err(RecordRead::Misplaced { table: "variant rows", at: row + 32 });
        }
        variant_end = first.checked_add(count).filter(|e| *e <= variants).ok_or(RecordRead::Past { table: "variant rows", at: row + 36 })?;
        for k in first..variant_end {
            let v = variants_at + k * VARIANT_ROW;
            text("variant name", v + 32, le32(bytes, v + 32) as usize, le32(bytes, v + 36) as usize)?;
            flag(bytes, v + 40, "variant record flag")?;
            zeros(bytes, v + 41..v + VARIANT_ROW, "variant row pad")?;
        }
    }
    if variant_end != variants {
        return Err(RecordRead::Layout { table: "enum rows", next: "variant rows", at: field_of(VARIANTS) });
    }

    let (macros, macros_at) = head.sections[MACROS];
    for i in 0..macros {
        let row = macros_at + i * MACRO_ROW;
        text("macro name", row + 64, le32(bytes, row + 64) as usize, le32(bytes, row + 68) as usize)?;
        flag(bytes, row + 88, "macro export flag")?;
        flag(bytes, row + 89, "macro local-inner flag")?;
        let extra = bytes[row + 91];
        let none_extra = || match extra {
            0 => Ok(()),
            _ => Err(RecordRead::BadByte { at: row + 91, what: "macro extra byte" }),
        };
        let (uses_a, uses_b) = match bytes[row + 90] {
            0 | 2 | 5 => {
                none_extra()?;
                (true, false)
            }
            1 => (flag(bytes, row + 91, "2.0 macro has-params flag")?, true),
            3 => {
                CallShape::from_byte(extra).ok_or(RecordRead::BadByte { at: row + 91, what: "call shape" })?;
                (false, false)
            }
            4 => {
                ProcKind::from_byte(extra).ok_or(RecordRead::BadByte { at: row + 91, what: "proc-macro kind" })?;
                (true, false)
            }
            _ => return Err(RecordRead::BadByte { at: row + 90, what: "macro body tag" }),
        };
        for (used, field) in [(uses_a, row + 72), (uses_b, row + 80)] {
            let (at, len) = (le32(bytes, field) as usize, le32(bytes, field + 4) as usize);
            if used {
                text("macro text", field, at, len)?;
            } else if (at, len) != (0, 0) {
                return Err(RecordRead::BadByte { at: field, what: "unused macro text range" });
            }
        }
        zeros(bytes, row + 92..row + MACRO_ROW, "macro row pad")?;
    }
    if strings_end.get() != strings_len {
        return Err(RecordRead::Layout { table: "strings", next: "blob", at: field_of(STRINGS) });
    }
    Ok(head)
}

/// A crate's exports record checked whole, once in this process: the proof every reader takes
/// (rule 74). Made only by [`Verified::check`]; a `Copy` of a pointer and the header.
///
/// The command makes it where the bytes enter the process (a wave's mapped record, a store
/// fetch whose digest was checked) and hands it to every dependent of the request, which
/// opens it in O(1) ([`DepRecord::open`]): never once per reader.
#[derive(Clone, Copy, Debug)]
pub struct Verified<'a> {
    bytes: &'a [u8],
    head: Head,
}

impl<'a> Verified<'a> {
    /// `bytes` (16-aligned) checked whole as a crate's exports record: O(the record), with no
    /// allocation. A refusal names the table and the byte offset that broke it.
    pub fn check(bytes: &'a [u8]) -> Result<Verified<'a>, RecordRead> {
        let head = check(bytes, Holds::Exports)?;
        Ok(Verified { bytes, head })
    }

    /// The crate the record is of.
    pub fn key(self) -> CrateItemKey {
        self.head.key
    }

    /// How many bytes the record is.
    pub fn byte_len(self) -> usize {
        self.bytes.len()
    }

    /// The checked bytes, for a reader of the head alone (crate_scope.rs `unmet_reaches`,
    /// `error_record_crate`) or one that takes bytes and checks them itself.
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
}

/// Whether `module`, in the crate whose INNER record is `inner` (16-aligned), binds `name` in
/// any namespace: the record checked whole (an inner record's scopes may hold textual scopes),
/// then one module row found and its scope searched in place. No allocation.
pub(crate) fn inner_binds(inner: &[u8], module: ItemKey, name: &str) -> Result<bool, RecordRead> {
    let head = check(inner, Holds::Inner)?;
    let record = DepRecord { bytes: inner, head };
    Ok(record.view(module).is_some_and(|v| v.bindings().any(|e| e.name == name)))
}

// ---------------------------------------------------------------------------------------------
// Reading in place.

/// A crate's exports record, read in place: a `Copy` of the proof's pointer and header. Every
/// answer is a binary search of the bytes; nothing is decoded, kept or allocated.
#[derive(Clone, Copy, Debug)]
pub struct DepRecord<'a> {
    bytes: &'a [u8],
    head: Head,
}

impl<'a> DepRecord<'a> {
    /// The record the proof is of: O(1), no allocation, nothing read.
    pub fn open(proof: Verified<'a>) -> DepRecord<'a> {
        DepRecord { bytes: proof.bytes, head: proof.head }
    }

    pub fn key(&self) -> CrateItemKey {
        self.head.key
    }

    pub fn root(&self) -> ItemKey {
        self.head.root
    }

    pub fn edition(&self) -> Edition {
        self.head.edition
    }

    pub fn proc_key(&self) -> Option<CrateItemKey> {
        self.head.proc_key
    }

    /// Whether this is an error crate's record (crate_scope.rs `error_record`).
    pub fn errored(&self) -> bool {
        self.head.errored
    }

    /// A range of the strings, as text.
    fn text(&self, at: usize, len: usize) -> &'a str {
        let bytes: &'a [u8] = self.bytes;
        let start = self.head.sections[STRINGS].1 + at;
        // SAFETY: every caller passes a range the header or a row of this record names, and
        // `check`, the one maker of the proof this record was opened from, ran `str::from_utf8`
        // on exactly that range of the same immutable `'a` bytes and it returned `Ok`.
        unsafe { core::str::from_utf8_unchecked(&bytes[start..start + len]) }
    }

    /// The crate's own name.
    pub fn name(&self) -> &'a str {
        self.text(self.head.name.0, self.head.name.1)
    }

    /// Every other crate its scopes name an item of, sorted: a slice of the bytes.
    pub fn reaches(&self) -> Reaches<'a> {
        let bytes: &'a [u8] = self.bytes;
        let (count, at) = self.head.sections[REACHES];
        Reaches::Words(bytes[at..at + count * KEY].as_chunks::<KEY>().0)
    }

    /// The row of `key` in `section`: where it starts in the bytes.
    fn row(&self, section: usize, key: ItemKey) -> Option<usize> {
        let (count, at) = self.head.sections[section];
        let width = SECTIONS[section].1;
        find_row(self.bytes, at, width, count, key).map(|i| at + i * width)
    }

    /// The module's scope in place, whatever record the check was of.
    fn view(&self, module: ItemKey) -> Option<ScopeView<'a>> {
        let bytes: &'a [u8] = self.bytes;
        let row = self.row(MODULES, module)?;
        let start = self.head.sections[BLOB].1 + le32(bytes, row + 32) as usize;
        let part = &bytes[start..start + le32(bytes, row + 36) as usize];
        // SAFETY: `check`, the one maker of the proof this record was opened from, ran
        // `ScopeView::read` on exactly these bytes (the same row of the same immutable `'a`
        // bytes gives the same range at the same address), and it returned `Ok`.
        Some(unsafe { ScopeView::trusted(part) })
    }

    /// The module's finished scope, viewed in place; `None` for a module the record does not
    /// hold (no other crate can walk into it).
    pub fn scope(&self, module: ItemKey) -> Option<ExportsScope<'a>> {
        // An exports record's: `Verified::check` refused any scope with a textual scope or a
        // mark, so the view answers those empty, as the scope does.
        self.view(module).map(ExportsScope)
    }

    /// The module's chain up to the crate's root, itself first: a slice of the bytes.
    pub fn chain(&self, module: ItemKey) -> Option<&'a [ItemKey]> {
        let bytes: &'a [u8] = self.bytes;
        let row = self.row(MODULES, module)?;
        let first = le32(bytes, row + 40);
        if first == NONE {
            return None;
        }
        let start = self.head.sections[CHAINS].1 + first as usize * KEY;
        let len = le32(bytes, row + 44) as usize;
        Some(ItemKey::slice_of(bytes[start..start + len * KEY].as_chunks::<KEY>().0))
    }

    /// An enum's variants as rows of the record, or `None` when `owner` is no enum it holds.
    pub fn variants(&self, owner: ItemKey) -> Option<Variants<'a>> {
        let bytes: &'a [u8] = self.bytes;
        let row = self.row(ENUMS, owner)?;
        let (first, count) = (le32(bytes, row + 32) as usize, le32(bytes, row + 36) as usize);
        let rows_at = self.head.sections[VARIANTS].1 + first * VARIANT_ROW;
        // SAFETY: `check`, the one maker of this record's proof, checked this enum's run of
        // variant rows inside the variant rows section, and each row's name range inside the
        // strings and UTF-8, in the same immutable `'a` bytes.
        Some(unsafe { Variants::trusted(bytes, self.head.sections[STRINGS].1, rows_at, count) })
    }

    /// A macro's source, made from its row: its texts slices of the bytes.
    pub fn macro_of(&self, def: ItemKey) -> Option<MacroSrc<'a>> {
        let row = self.row(MACROS, def)?;
        Some(self.macro_at(row))
    }

    fn macro_at(&self, row: usize) -> MacroSrc<'a> {
        let bytes: &'a [u8] = self.bytes;
        let range = |field: usize| self.text(le32(bytes, field) as usize, le32(bytes, field + 4) as usize);
        let extra = bytes[row + 91];
        let body = match bytes[row + 90] {
            0 => MacroBody::Rules(range(row + 72)),
            1 => MacroBody::Macro2 { params: (extra != 0).then(|| range(row + 72)), body: range(row + 80) },
            2 => MacroBody::BuiltinFn(range(row + 72)),
            3 => match CallShape::from_byte(extra) {
                Some(shape) => MacroBody::BuiltinOther(shape),
                None => unreachable!("a macro row's call shape at {} was checked by `check`", row + 91),
            },
            4 => match ProcKind::from_byte(extra) {
                Some(kind) => MacroBody::Proc { kind, helpers: range(row + 72) },
                None => unreachable!("a macro row's proc-macro kind at {} was checked by `check`", row + 91),
            },
            5 => MacroBody::BuiltinDerive { helpers: range(row + 72) },
            tag => unreachable!("a macro row's body tag {tag} at {} was checked by `check`", row + 90),
        };
        MacroSrc { name: range(row + 64), body, module: key_at(bytes, row + 32), export: bytes[row + 88] != 0, local_inner: bytes[row + 89] != 0 }
    }

    /// Every module the record holds, by key, in the stored (key) order.
    pub fn modules(&self) -> impl ExactSizeIterator<Item = ItemKey> + use<'a> {
        let (bytes, (count, at)) = (self.bytes, self.head.sections[MODULES]);
        (0..count).map(move |i| key_at(bytes, at + i * MODULE_ROW))
    }

    /// Every enum the record holds, by owner, in the stored (key) order.
    pub fn enums(&self) -> impl ExactSizeIterator<Item = ItemKey> + use<'a> {
        let (bytes, (count, at)) = (self.bytes, self.head.sections[ENUMS]);
        (0..count).map(move |i| key_at(bytes, at + i * ENUM_ROW))
    }

    /// Every macro the record holds, by definition, in the stored (key) order.
    pub fn macros(&self) -> impl ExactSizeIterator<Item = ItemKey> + use<'a> {
        let (bytes, (count, at)) = (self.bytes, self.head.sections[MACROS]);
        (0..count).map(move |i| key_at(bytes, at + i * MACRO_ROW))
    }

    /// How many bytes the record is.
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }
}

// ---------------------------------------------------------------------------------------------
// What a read of a dependency answers, in either form (rule 78: the working form's values or
// the record's rows behind one type each).

/// A module's scope in a crate's exports record, viewed in place: one whose check found no
/// textual scope and no marks ([`Verified::check`]).
#[derive(Clone, Copy, Debug)]
pub struct ExportsScope<'a>(ScopeView<'a>);

impl<'a> ExportsScope<'a> {
    pub fn view(self) -> ScopeView<'a> {
        self.0
    }
}

/// A finished scope as a read finds it: a working form's (a module of this fill, a crate held
/// whole, the decoded control form) or a dependency's exports record's, read in place.
pub enum ScopeOf<'h, N> {
    Built(&'h ScopeBuilder<'h, N>),
    Exports(ExportsScope<'h>),
}

impl<N> Clone for ScopeOf<'_, N> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<N> Copy for ScopeOf<'_, N> {}

impl<'h, N: Name> ScopeOf<'h, N> {
    /// The module's own binding of `name` in `ns`.
    pub fn get(self, ns: Namespace, name: N) -> Option<Binding> {
        match self {
            ScopeOf::Built(s) => s.get(ns, name).copied(),
            ScopeOf::Exports(s) => s.0.get(ns, name.as_ref()),
        }
    }

    /// Its glob edges, by source.
    pub fn globs(self) -> Globs<'h> {
        match self {
            ScopeOf::Built(s) => Globs::of(&s.globs),
            ScopeOf::Exports(s) => s.0.edges(),
        }
    }

    /// Where its `mod child` line falls in its textual scope (an exports scope has none).
    pub fn mark(self, child: ItemKey) -> Option<u32> {
        match self {
            ScopeOf::Built(s) => s.mark(child),
            ScopeOf::Exports(_) => None,
        }
    }

    /// Its textual scope, in order (an exports scope has none).
    pub fn textual(self) -> &'h [TextualDef<N>] {
        match self {
            ScopeOf::Built(s) => &s.textual,
            ScopeOf::Exports(_) => &[],
        }
    }
}

/// An enum's variants: a working form's slice or a record's rows, in declaration order.
#[derive(Clone, Copy, Debug)]
pub enum VariantsOf<'a> {
    Slice(&'a [Variant<&'a str>]),
    Rows(Variants<'a>),
}

impl<'a> VariantsOf<'a> {
    pub fn len(self) -> usize {
        match self {
            VariantsOf::Slice(v) => v.len(),
            VariantsOf::Rows(v) => v.len(),
        }
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Variant `i`.
    pub fn get(self, i: usize) -> Option<Variant<&'a str>> {
        match self {
            VariantsOf::Slice(v) => v.get(i).copied(),
            VariantsOf::Rows(v) => v.get(i),
        }
    }

    /// Every variant, in declaration order.
    pub fn iter(self) -> impl ExactSizeIterator<Item = Variant<&'a str>> + use<'a> {
        let all = self;
        (0..all.len()).map(move |i| match all {
            VariantsOf::Slice(v) => v[i],
            VariantsOf::Rows(v) => v.at(i),
        })
    }

    /// The variant named `name` (the record's compared in place).
    pub fn find(self, name: &str) -> Option<VariantHit> {
        match self {
            VariantsOf::Slice(v) => variant_in(v, name),
            VariantsOf::Rows(v) => v.find(name),
        }
    }
}

/// A crate's reach list: a working form's keys or a record's 32-byte words, sorted either way.
#[derive(Clone, Copy, Debug)]
pub enum Reaches<'a> {
    Keys(&'a [CrateItemKey]),
    Words(&'a [[u8; KEY]]),
}

impl<'a> Reaches<'a> {
    pub fn len(self) -> usize {
        match self {
            Reaches::Keys(k) => k.len(),
            Reaches::Words(w) => w.len(),
        }
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Key `i`.
    pub fn get(self, i: usize) -> Option<CrateItemKey> {
        match self {
            Reaches::Keys(k) => k.get(i).copied(),
            Reaches::Words(w) => w.get(i).map(|w| CrateItemKey(ContentId(*w))),
        }
    }

    /// Every key, in the held order.
    pub fn iter(self) -> impl ExactSizeIterator<Item = CrateItemKey> + use<'a> {
        let all = self;
        (0..all.len()).map(move |i| match all {
            Reaches::Keys(k) => k[i],
            Reaches::Words(w) => CrateItemKey(ContentId(w[i])),
        })
    }
}
