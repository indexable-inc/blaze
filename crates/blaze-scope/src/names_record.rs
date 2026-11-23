//! A crate's full names as a record of their own (per-crate-entry.md, "Runner twenty"): every
//! module's solved scope, chain and `#[no_implicit_prelude]`, every enum's variants and the crate
//! root, so a path written ANYWHERE in the crate resolves through it ([`crate::names::path`])
//! with the answers of a fresh solve. Written when the crate is solved, beside its exports
//! record, inner record and read list; read back per crate, on demand.
//!
//! The record (integers little-endian, every section 16-aligned): a 128-byte header (`BZNM`,
//! version, then u32 counts and offsets of each section, the crate key, the root module key);
//! module rows, sorted by key, FIXED 48 bytes (key, scope at u32 and length u32 into the blob,
//! `u32::MAX` for a module whose task failed; first chain key u32 (`u32::MAX`: none), chain
//! length u16, flags u16: bit 0 `#[no_implicit_prelude]`, bit 1 a block's module, anonymous to
//! a written `self`/`super`: `crate::names::written_up`); the chain keys (32 bytes each);
//! enum rows, sorted by owner, 40 bytes (owner, first variant u32, count u32); variant rows,
//! 48 bytes (key, name at u32 and length u32 into the strings, record u8, pad); the strings; the
//! crate's `CrateRoot` record; the blob, each module's blaze-surface `ModuleScope` record; the
//! refused calls (version 5; failed ones only from version 2): per call of the crate the solve
//! stored refused, sorted by call key, its macro path as written, its shape, whether it is
//! D104's implicit `::std::main`, its file in its package, a derive's item name (version 6),
//! and its reason's kind
//! ([`crate::call_refusal::RefusalKind`]) with the reason: blaze-expand's record of
//! [`blaze_expand::Failed`] for a failed expansion, else the words naming what refused it
//! ([`NamesRecord::failed`]).
//!
//! A RECORD IS ITS OWN INDEX (D114 rule 1). The bytes are checked once in a process, whole:
//! [`Verified::check`] reads every module's scope and the crate root with their own checks,
//! and every row's ranges, and is the one way to a [`Verified`], the proof every reader takes
//! (rule 74). [`NamesRecord::open`] of a proof is a copy of its header (O(1), no allocation);
//! every answer after it is a binary search of the mapped bytes: a module's row, then its scope
//! viewed in place ([`ScopeView::trusted`]) and searched in place, its chain a slice of the
//! bytes, an enum's variants a run of rows, the crate root a view. Nothing is decoded, kept or
//! counted per module: a fresh process answers its first lookup in the time of the searches.

use core::cell::OnceCell;

use blaze_format::FormatError;
use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc, Bump};
use blaze_surface::{BuildError, CrateRootView, RecordBytes, ScopeView};
#[cfg(test)]
use blaze_surface::ScopeBuilder;

use crate::facts::Name;
use crate::names::{VariantHit, World};
use blaze_surface::CallShape;

use crate::call_refusal::{CallRefusal, RefusalKind};
use crate::resolve::{CrateNames, ExpandNote};
use crate::task::Variant;

const MAGIC: [u8; 4] = *b"BZNM";
/// 2: the failed-calls section (implicit-main-reason, 2026-11-16). A version-1 record is
/// [`NamesRecordError::NotThisRecord`]: an old home's names are made again (rule 26).
/// 3: row flag bit 1, a block's module (super-in-block-1, 2026-11-16); an older record has it
/// clear everywhere and would read `super` in a block wrong.
/// Reading in place (D114 rule 1) changes no byte: the writer is the same, so the version stays.
/// 4: the crate root it holds is form 4 (blaze-surface `CrateRootRoot`: the root's
/// `#![feature(..)]` names); a version-3 record is not read, and its names are made again.
/// 5: a row for EVERY refused call, not only a failed expansion: the flags gain the call's
/// shape and the reason's kind, the row its file ([`crate::call_refusal`], MR 2026-11-17); a
/// version-4 record is not read, and its names are made again.
/// 6: a refused derive's row names the item it is on (+60, its length; its text after the
/// file's: [`FailedRow::on`], D193 point 5); a version-5 record is not read, and its names are
/// made again.
const VERSION: u32 = 6;
/// A refused call's row: call key, path at and length, record at and length, flags (bit 0
/// implicit, byte 1 shape, byte 2 kind), file at and length, then the length of the name of the
/// item a derive is on (0: none; its text follows the file's in the texts).
const FAILED_ROW: usize = 64;
const ALIGN: usize = 16;
const HEADER: usize = 128;
const ROW: usize = 48;
const ENUM_ROW: usize = 40;
const VARIANT_ROW: usize = 48;
const KEY: usize = 32;
const NONE: u32 = u32::MAX;
const NO_IMPLICIT_PRELUDE: u16 = 1;
const ANONYMOUS: u16 = 2;

/// Why a names record was not written or not read.
#[derive(Debug)]
pub enum NamesRecordError {
    /// A section's size does not fit its u32 field.
    TooBig { what: &'static str },
    /// A module's scope or the crate root did not write.
    Write(BuildError),
    /// The bytes do not start on a 16-byte boundary: map or copy them to one.
    Unaligned,
    /// Not `BZNM` and this version.
    NotThisRecord,
    /// A section or a row's range runs past the end of its section.
    Short { what: &'static str, at: usize },
    /// A table's rows are not in strictly increasing key order, which its search relies on.
    OutOfOrder { what: &'static str, at: usize },
    /// A module's `ModuleScope` record did not read.
    Scope { module: ItemKey, error: FormatError },
    /// A module row's scope is the scope of another module.
    NotItsScope { module: ItemKey, found: ItemKey },
    /// The `CrateRoot` record did not read.
    Root(FormatError),
    /// A variant's name is not UTF-8.
    Name { owner: ItemKey },
    /// A failed call's macro path is not UTF-8.
    FailedPath { call: ItemKey },
    /// A failed call's reason did not read as blaze-expand's record of a failure.
    FailedRecord { call: ItemKey, error: blaze_expand::RecordError },
    /// A refused call's words are not UTF-8.
    FailedWords { call: ItemKey },
    /// A refused call's flags name no call shape or no refusal kind.
    FailedFlags { call: ItemKey },
}

/// A REFUSED macro call of the crate, read from its names record ([`crate::call_refusal`]):
/// the call, the macro's path as written (`::std::main` for D104's implicit call over the root
/// `fn main`), the call's shape, the file it is written in (its path in its package, as
/// `file!()` shows it), and why it was refused, typed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FailedRow<'a> {
    pub call: ItemKey,
    pub path: &'a str,
    pub shape: CallShape,
    pub implicit: bool,
    /// The file the call is written in, past its package's directory (`src/lib.rs`); empty for
    /// a call the lowering does not know.
    pub file: &'a str,
    /// A derive's: the name of the item it is on ([`crate::task::CallSrc::on`]), the self type
    /// of the impls it would have written; empty for any other call.
    pub on: &'a str,
    pub why: CallRefusal<'a>,
}

/// One refused call of `names` as its row holds it, from the solve's own values: the call's
/// lowering ([`CrateNames::tables`]) for its path, shape and file, and the note that refused it
/// ([`crate::resolve::Expansion::refused_by`]) for why. `None`: `call` is no refused call of it.
/// The one join of the writer ([`failed_section`]) and of a reader of the solve's values (blaze-
/// ladder's `NamesSource` over `Names`), so the two read alike (rule 30).
pub fn refused_row<'n, 'b>(names: &CrateNames<'n>, call: ItemKey, alloc: Bump<'b>) -> Option<FailedRow<'b>>
where
    'n: 'b,
{
    let expansion = names.expansions.get(&call)?;
    expansion.refused?;
    let src = names.tables.calls.get(&call);
    let (path, shape, implicit) = src.map_or(("", CallShape::FnLike, false), |s| (s.name, s.kind, s.implicit));
    let file = src.and_then(|s| names.files.get(s.loc.file as usize)).map_or("", |f| f.shown);
    let on = src.and_then(|s| s.on).unwrap_or("");
    let why = match expansion.refused_by {
        Some((ExpandNote::Failed { failed, .. }, _)) => CallRefusal::Failed(failed),
        Some((note, env)) => {
            let (kind, words) = crate::call_refusal::words_of(&note, env, alloc.alloc());
            CallRefusal::Refused { kind, words: alloc.alloc_str(&words) }
        }
        None => CallRefusal::Refused { kind: RefusalKind::Unnoted, words: "" },
    };
    Some(FailedRow { call, path, shape, implicit, file, on, why })
}

/// The refused-calls section of `names` (the module doc says its form): a row per call the
/// solve stored refused, whatever refused it. A function of the solve's values alone, sorted by
/// call key at this boundary (rule 25).
fn failed_section<'b>(names: &CrateNames<'_>, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, NamesRecordError> {
    let mut keys: AVec<'b, ItemKey> = AVec::new_in(alloc);
    keys.extend(names.expansions.iter().filter(|(_, e)| e.refused.is_some()).map(|(call, _)| *call));
    keys.sort_unstable();
    let mut calls = AVec::with_capacity_in(keys.len(), alloc);
    for call in keys.iter() {
        let Some(expansion) = names.expansions.get(call) else { continue };
        if expansion.refused.is_none() {
            continue;
        }
        let src = names.tables.calls.get(call);
        let (path, shape, implicit) = src.map_or(("", CallShape::FnLike, false), |s| (s.name, s.kind, s.implicit));
        let file = src.and_then(|s| usize::try_from(s.loc.file).ok()).and_then(|idx| names.files.get(idx)).map_or("", |f| f.shown);
        let on = src.and_then(|s| s.on).unwrap_or("");
        let (kind, reason) = match expansion.refused_by {
            Some((ExpandNote::Failed { failed, .. }, _)) => (RefusalKind::Failed, failed.write_record(alloc)),
            Some((note, env)) => {
                let (kind, words) = crate::call_refusal::words_of(&note, env, alloc);
                let mut bytes = RecordBytes::new_in(alloc);
                bytes.extend_from_slice(words.as_bytes());
                (kind, bytes)
            }
            None => (RefusalKind::Unnoted, RecordBytes::new_in(alloc)),
        };
        calls.push((*call, path, shape, implicit, file, on, kind, reason));
    }
    let mut texts = RecordBytes::new_in(alloc);
    for &(_, path, _, _, file, on, _, _) in calls.iter() {
        texts.extend_from_slice(path.as_bytes());
        texts.extend_from_slice(file.as_bytes());
        texts.extend_from_slice(on.as_bytes());
    }
    pad(&mut texts);
    let records_at = ALIGN + calls.len() * FAILED_ROW + texts.len();
    let mut out = RecordBytes::new_in(alloc);
    out.extend_from_slice(&u32_of(calls.len(), "failed calls")?.to_le_bytes());
    out.resize(ALIGN, 0);
    let (mut text_at, mut record_at) = (0usize, records_at);
    for (call, path, shape, implicit, file, on, kind, record) in calls.iter() {
        let flags = u32::from_le_bytes([u8::from(*implicit), shape.byte(), kind.byte(), 0]);
        out.extend_from_slice(&call.0.0);
        out.extend_from_slice(&u32_of(text_at, "failed call paths")?.to_le_bytes());
        out.extend_from_slice(&u32_of(path.len(), "a failed call's path")?.to_le_bytes());
        out.extend_from_slice(&u32_of(record_at, "failed call records")?.to_le_bytes());
        out.extend_from_slice(&u32_of(record.len(), "a failed call's record")?.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&u32_of(text_at + path.len(), "failed call files")?.to_le_bytes());
        out.extend_from_slice(&u32_of(file.len(), "a failed call's file")?.to_le_bytes());
        out.extend_from_slice(&u32_of(on.len(), "a failed derive's item name")?.to_le_bytes());
        text_at += path.len() + file.len() + on.len();
        record_at = (record_at + record.len()).next_multiple_of(ALIGN);
    }
    out.extend_from_slice(&texts);
    for (_, _, _, _, _, _, _, record) in calls.iter() {
        out.extend_from_slice(record);
        pad(&mut out);
    }
    Ok(out)
}

/// A failed-call row's flags: bit 0 D104's implicit call, byte 1 the shape, byte 2 the kind.
fn row_flags(flags: u32) -> (bool, Option<CallShape>, Option<RefusalKind>) {
    let [low, shape, kind, _] = flags.to_le_bytes();
    (low & 1 == 1, CallShape::from_byte(shape), RefusalKind::from_byte(kind))
}

fn u32_of(n: usize, what: &'static str) -> Result<u32, NamesRecordError> {
    u32::try_from(n).map_err(|_| NamesRecordError::TooBig { what })
}

fn pad(out: &mut RecordBytes<'_>) {
    out.resize(out.len().next_multiple_of(ALIGN), 0);
}

/// The planted bugs of the record's oracle (tests/names_record.rs), applied by the encoder to
/// the first module (by key) whose scope binds anything. Set on the test's thread.
#[cfg(test)]
pub(crate) mod plant {
    use core::cell::Cell;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Plant {
        None,
        /// The scope's first binding is left out.
        DropEntry,
        /// The scope's first binding's visibility is flipped (public to private, else public).
        FlipVis,
    }

    std::thread_local! {
        pub(crate) static PLANT: Cell<Plant> = const { Cell::new(Plant::None) };
    }
}

#[cfg(test)]
fn planted<'s, 'n>(module: ItemKey, scope: &'s ScopeBuilder<'n, &'n str>, done: &mut bool) -> Option<ScopeBuilder<'n, &'n str>> {
    use blaze_surface::Visibility;
    use core::cell::Cell;
    use plant::{PLANT, Plant};
    let plant = PLANT.with(Cell::get);
    if plant == Plant::None || *done || scope.bindings.is_empty() {
        return None;
    }
    *done = true;
    let mut changed = scope.clone();
    match plant {
        Plant::DropEntry => {
            changed.bindings.remove(0);
        }
        Plant::FlipVis => {
            let vis = &mut changed.bindings[0].binding.vis;
            *vis = if *vis == Visibility::Public { Visibility::Restricted(module) } else { Visibility::Public };
        }
        Plant::None => {}
    }
    Some(changed)
}

/// The names record of a solved crate. Its bytes are a function of the solve's answers: every
/// table is sorted by key, so any worker in any order writes the same bytes.
pub fn encode<'b>(names: &CrateNames<'_>, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, NamesRecordError> {
    let mut modules = AVec::with_capacity_in(names.chains.len() + names.done.len(), alloc);
    modules.extend(names.chains.keys().copied());
    modules.extend(names.done.keys().copied());
    modules.sort_unstable();
    modules.dedup();

    let mut rows = RecordBytes::new_in(alloc);
    let mut chains = RecordBytes::new_in(alloc);
    let mut blob = RecordBytes::new_in(alloc);
    let mut chain_keys = 0usize;
    #[cfg(test)]
    let mut planted_once = false;
    for m in modules.iter() {
        rows.extend_from_slice(&m.0.0);
        match names.done.get(m) {
            Some(done) => {
                #[cfg(test)]
                let bytes = match planted(*m, &done.scope, &mut planted_once) {
                    Some(changed) => changed.write(alloc),
                    None => done.scope.write(alloc),
                };
                #[cfg(not(test))]
                let bytes = done.scope.write(alloc);
                let bytes = bytes.map_err(NamesRecordError::Write)?;
                rows.extend_from_slice(&u32_of(blob.len(), "names blob")?.to_le_bytes());
                rows.extend_from_slice(&u32_of(bytes.len(), "a module scope")?.to_le_bytes());
                blob.extend_from_slice(&bytes);
                pad(&mut blob);
            }
            None => {
                rows.extend_from_slice(&NONE.to_le_bytes());
                rows.extend_from_slice(&0u32.to_le_bytes());
            }
        }
        match names.chains.get(m) {
            Some(chain) => {
                rows.extend_from_slice(&u32_of(chain_keys, "names chains")?.to_le_bytes());
                let len = u16::try_from(chain.len()).map_err(|_| NamesRecordError::TooBig { what: "a module chain" })?;
                rows.extend_from_slice(&len.to_le_bytes());
                for k in chain.iter() {
                    chains.extend_from_slice(&k.0.0);
                }
                chain_keys += chain.len();
            }
            None => {
                rows.extend_from_slice(&NONE.to_le_bytes());
                rows.extend_from_slice(&0u16.to_le_bytes());
            }
        }
        let mut flags = 0u16;
        if names.info(*m).is_some_and(|i| i.no_implicit_prelude) {
            flags |= NO_IMPLICIT_PRELUDE;
        }
        if names.anonymous(*m) {
            flags |= ANONYMOUS;
        }
        rows.extend_from_slice(&flags.to_le_bytes());
    }

    let mut owners = AVec::from_iter_in(names.tables.enums.keys().copied(), alloc);
    owners.sort_unstable();
    let mut enum_rows = RecordBytes::new_in(alloc);
    let mut variant_rows = RecordBytes::new_in(alloc);
    let mut strings = RecordBytes::new_in(alloc);
    let mut variants = 0usize;
    for owner in owners.iter() {
        let list = &names.tables.enums[owner];
        enum_rows.extend_from_slice(&owner.0.0);
        enum_rows.extend_from_slice(&u32_of(variants, "names variants")?.to_le_bytes());
        enum_rows.extend_from_slice(&u32_of(list.len(), "an enum's variants")?.to_le_bytes());
        for v in list.iter() {
            variant_rows.extend_from_slice(&v.key.0.0);
            variant_rows.extend_from_slice(&u32_of(strings.len(), "names strings")?.to_le_bytes());
            variant_rows.extend_from_slice(&u32_of(v.name.len(), "a variant name")?.to_le_bytes());
            variant_rows.extend_from_slice(&[u8::from(v.record), 0, 0, 0, 0, 0, 0, 0]);
            strings.extend_from_slice(v.name.as_bytes());
        }
        variants += list.len();
    }
    pad(&mut enum_rows);
    pad(&mut strings);
    let root = names.crate_root.write(alloc).map_err(NamesRecordError::Write)?;

    let rows_at = HEADER;
    let chains_at = rows_at + rows.len();
    let enums_at = chains_at + chains.len();
    let variants_at = enums_at + enum_rows.len();
    let strings_at = variants_at + variant_rows.len();
    let root_at = strings_at + strings.len();
    let blob_at = (root_at + root.len()).next_multiple_of(ALIGN);
    let failed = failed_section(names, alloc)?;
    let failed_at = (blob_at + blob.len()).next_multiple_of(ALIGN);
    let mut out = RecordBytes::with_capacity_in(failed_at + failed.len(), alloc);
    out.extend_from_slice(&MAGIC);
    let fields = [
        (VERSION as usize, "version"),
        (modules.len(), "names modules"),
        (chain_keys, "names chains"),
        (owners.len(), "names enums"),
        (variants, "names variants"),
        (chains_at, "names offsets"),
        (enums_at, "names offsets"),
        (variants_at, "names offsets"),
        (strings_at, "names offsets"),
        (root_at, "names offsets"),
        (root.len(), "the crate root"),
        (blob_at, "names offsets"),
        (blob.len(), "names blob"),
        (failed_at, "names offsets"),
        (failed.len(), "names failed calls"),
    ];
    for (n, what) in fields {
        out.extend_from_slice(&u32_of(n, what)?.to_le_bytes());
    }
    out.resize(HEADER - 64, 0);
    out.extend_from_slice(&names.key.0.0);
    out.extend_from_slice(&names.root.0.0);
    out.extend_from_slice(&rows);
    out.extend_from_slice(&chains);
    out.extend_from_slice(&enum_rows);
    out.extend_from_slice(&variant_rows);
    out.extend_from_slice(&strings);
    out.extend_from_slice(&root);
    pad(&mut out);
    out.extend_from_slice(&blob);
    pad(&mut out);
    debug_assert_eq!(out.len(), failed_at);
    out.extend_from_slice(&failed);
    Ok(out)
}

// The row helpers of a record read in place: this record's and the `CrateScope` exports
// record's (crate::scope_record), one home for both (rule 30).

pub(crate) fn le32(bytes: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(b)
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

pub(crate) fn key_at(bytes: &[u8], at: usize) -> ItemKey {
    let mut k = [0u8; KEY];
    k.copy_from_slice(&bytes[at..at + KEY]);
    ItemKey(ContentId(k))
}

/// Binary search of `count` fixed-width rows of `width` bytes at `at` of `bytes`, keyed by their
/// first 32 bytes in strictly increasing order (each reader's `Verified` checked it): the row's
/// index.
pub(crate) fn find_row(bytes: &[u8], at: usize, width: usize, count: usize, key: ItemKey) -> Option<usize> {
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let row = at + mid * width;
        match bytes[row..row + KEY].cmp(&key.0.0[..]) {
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
            core::cmp::Ordering::Equal => return Some(mid),
        }
    }
    None
}

/// A variant row's width: key, name at u32 and length u32 into the strings, record u8, pad. The
/// `CrateScope` exports record holds the same rows ([`Variants::trusted`]).
pub(crate) const VARIANT_ROW_WIDTH: usize = VARIANT_ROW;

/// What a record's header says: the sections' places and counts, checked to lie inside it.
#[derive(Clone, Copy, Debug)]
struct Head {
    key: CrateItemKey,
    root_module: ItemKey,
    modules: usize,
    chain_keys: usize,
    enums: usize,
    variants: usize,
    chains_at: usize,
    enums_at: usize,
    variants_at: usize,
    strings_at: usize,
    root_at: usize,
    root_len: usize,
    blob_at: usize,
    blob_len: usize,
    /// The failed-calls section (version 2).
    failed_at: usize,
    failed_len: usize,
}

impl Head {
    /// The header of `bytes`, every section checked to lie inside them: O(1).
    fn read(bytes: &[u8]) -> Result<Head, NamesRecordError> {
        if !(bytes.as_ptr() as usize).is_multiple_of(ALIGN) {
            return Err(NamesRecordError::Unaligned);
        }
        if bytes.len() < HEADER || bytes[..4] != MAGIC || le32(bytes, 4) != VERSION {
            return Err(NamesRecordError::NotThisRecord);
        }
        let f = |i: usize| le32(bytes, 8 + 4 * i) as usize;
        let head = Head {
            key: CrateItemKey(key_at(bytes, HEADER - 64).0),
            root_module: key_at(bytes, HEADER - 32),
            modules: f(0),
            chain_keys: f(1),
            enums: f(2),
            variants: f(3),
            chains_at: f(4),
            enums_at: f(5),
            variants_at: f(6),
            strings_at: f(7),
            root_at: f(8),
            root_len: f(9),
            blob_at: f(10),
            blob_len: f(11),
            failed_at: f(12),
            failed_len: f(13),
        };
        let within = |what: &'static str, at: usize, count: usize, width: usize| match count.checked_mul(width).and_then(|len| at.checked_add(len)) {
            Some(end) if end <= bytes.len() => Ok(()),
            _ => Err(NamesRecordError::Short { what, at }),
        };
        within("module rows", HEADER, head.modules, ROW)?;
        within("chain keys", head.chains_at, head.chain_keys, KEY)?;
        within("enum rows", head.enums_at, head.enums, ENUM_ROW)?;
        within("variant rows", head.variants_at, head.variants, VARIANT_ROW)?;
        if head.strings_at > head.root_at {
            return Err(NamesRecordError::Short { what: "strings", at: head.strings_at });
        }
        within("strings", head.strings_at, head.root_at - head.strings_at, 1)?;
        within("crate root", head.root_at, head.root_len, 1)?;
        within("blob", head.blob_at, head.blob_len, 1)?;
        within("failed calls", head.failed_at, head.failed_len, 1)?;
        if head.failed_len < ALIGN
            || !head.failed_at.is_multiple_of(ALIGN)
            || (le32(bytes, head.failed_at) as usize).checked_mul(FAILED_ROW).is_none_or(|rows| head.failed_len < ALIGN + rows)
        {
            return Err(NamesRecordError::Short { what: "failed call rows", at: head.failed_at });
        }
        Ok(head)
    }
}

/// A names record checked whole, once in this process: the proof every reader takes (rule 74).
/// Made only by [`Verified::check`]; a `Copy` of a pointer and the header.
///
/// The check is O(the record): the header, each table's key order, every row's ranges, every
/// module's `ModuleScope` record and the `CrateRoot` record by their own checks
/// ([`ScopeView::read`], [`CrateRootView::read`]), each variant name as UTF-8. The command makes
/// it where the bytes enter the process (front_load.rs's one call, or a store fetch whose digest
/// was checked: the store's `Checked` proof, once it lands, plus this form check), and keeps it
/// as long as the bytes.
#[derive(Clone, Copy, Debug)]
pub struct Verified<'m> {
    bytes: &'m [u8],
    head: Head,
}

impl<'m> Verified<'m> {
    /// `bytes` checked whole as a names record.
    pub fn check(bytes: &'m [u8]) -> Result<Verified<'m>, NamesRecordError> {
        let head = Head::read(bytes)?;
        let increasing = |what: &'static str, at: usize, count: usize, width: usize| {
            for i in 1..count {
                let (a, b) = (at + (i - 1) * width, at + i * width);
                if bytes[a..a + KEY] >= bytes[b..b + KEY] {
                    return Err(NamesRecordError::OutOfOrder { what, at: b });
                }
            }
            Ok(())
        };
        increasing("module rows", HEADER, head.modules, ROW)?;
        increasing("enum rows", head.enums_at, head.enums, ENUM_ROW)?;
        for i in 0..head.modules {
            let row = HEADER + i * ROW;
            let module = key_at(bytes, row);
            let at = le32(bytes, row + 32);
            if at != NONE {
                let (start, len) = (at as usize, le32(bytes, row + 36) as usize);
                if start.checked_add(len).is_none_or(|end| end > head.blob_len) {
                    return Err(NamesRecordError::Short { what: "a module scope", at: head.blob_at + start });
                }
                let start = head.blob_at + start;
                let view = ScopeView::read(&bytes[start..start + len]).map_err(|error| NamesRecordError::Scope { module, error })?;
                if view.module() != module {
                    return Err(NamesRecordError::NotItsScope { module, found: view.module() });
                }
            }
            let first = le32(bytes, row + 40);
            if first != NONE {
                let len = usize::from(le16(bytes, row + 44));
                if (first as usize).checked_add(len).is_none_or(|end| end > head.chain_keys) {
                    return Err(NamesRecordError::Short { what: "a module's chain", at: row });
                }
            }
        }
        let strings_len = head.root_at - head.strings_at;
        for i in 0..head.enums {
            let row = head.enums_at + i * ENUM_ROW;
            let owner = key_at(bytes, row);
            let (first, count) = (le32(bytes, row + 32) as usize, le32(bytes, row + 36) as usize);
            if first.checked_add(count).is_none_or(|end| end > head.variants) {
                return Err(NamesRecordError::Short { what: "an enum's variant rows", at: row });
            }
            for k in first..first + count {
                let v = head.variants_at + k * VARIANT_ROW;
                let (at, len) = (le32(bytes, v + 32) as usize, le32(bytes, v + 36) as usize);
                if at.checked_add(len).is_none_or(|end| end > strings_len) {
                    return Err(NamesRecordError::Short { what: "a variant name", at: v });
                }
                let start = head.strings_at + at;
                core::str::from_utf8(&bytes[start..start + len]).map_err(|_| NamesRecordError::Name { owner })?;
            }
        }
        CrateRootView::read(&bytes[head.root_at..head.root_at + head.root_len]).map_err(NamesRecordError::Root)?;
        // The refused calls: rows by increasing call key, each path and file a UTF-8 range of
        // the texts, its shape and kind known bytes, and each reason a range of the section:
        // words in UTF-8, or a failure's record, read when asked ([`NamesRecord::failed`],
        // blaze-expand's check of its record).
        let s = &bytes[head.failed_at..head.failed_at + head.failed_len];
        let count = le32(s, 0) as usize;
        increasing("failed call rows", head.failed_at + ALIGN, count, FAILED_ROW)?;
        let texts_at = ALIGN + count * FAILED_ROW;
        let text = |at: usize, len: usize| texts_at.checked_add(at).and_then(|start| Some(start..start.checked_add(len)?)).and_then(|r| s.get(r));
        for i in 0..count {
            let row = ALIGN + i * FAILED_ROW;
            let call = key_at(s, row);
            let path = text(le32(s, row + 32) as usize, le32(s, row + 36) as usize);
            let path = path.ok_or(NamesRecordError::Short { what: "a failed call's path", at: head.failed_at + row })?;
            core::str::from_utf8(path).map_err(|_| NamesRecordError::FailedPath { call })?;
            let file = text(le32(s, row + 52) as usize, le32(s, row + 56) as usize);
            let file = file.ok_or(NamesRecordError::Short { what: "a failed call's file", at: head.failed_at + row })?;
            core::str::from_utf8(file).map_err(|_| NamesRecordError::FailedPath { call })?;
            // A derive's item name: right after the file's text.
            let on = (le32(s, row + 52) as usize).checked_add(le32(s, row + 56) as usize).and_then(|at| text(at, le32(s, row + 60) as usize));
            let on = on.ok_or(NamesRecordError::Short { what: "a failed derive's item name", at: head.failed_at + row })?;
            core::str::from_utf8(on).map_err(|_| NamesRecordError::FailedPath { call })?;
            let (record_at, record_len) = (le32(s, row + 40) as usize, le32(s, row + 44) as usize);
            let Some(record) = record_at.checked_add(record_len).and_then(|end| s.get(record_at..end)) else {
                return Err(NamesRecordError::Short { what: "a failed call's record", at: head.failed_at + row });
            };
            match row_flags(le32(s, row + 48)) {
                (_, Some(_), Some(RefusalKind::Failed)) => {}
                (_, Some(_), Some(_)) => {
                    core::str::from_utf8(record).map_err(|_| NamesRecordError::FailedWords { call })?;
                }
                _ => return Err(NamesRecordError::FailedFlags { call }),
            }
        }
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
}

/// One crate's names record, read in place: a `Copy` of the proof's pointer and header. Every
/// answer is a binary search of the bytes; nothing is decoded or kept.
#[derive(Clone, Copy, Debug)]
pub struct NamesRecord<'m> {
    bytes: &'m [u8],
    head: Head,
}

impl<'m> NamesRecord<'m> {
    /// The record the proof is of: O(1), no allocation, nothing read.
    pub fn open(proof: Verified<'m>) -> NamesRecord<'m> {
        NamesRecord { bytes: proof.bytes, head: proof.head }
    }

    pub fn key(&self) -> CrateItemKey {
        self.head.key
    }

    pub fn root_module(&self) -> ItemKey {
        self.head.root_module
    }

    /// Binary search of fixed-width rows of `width` bytes at `at`, keyed by their first 32.
    fn find(&self, at: usize, width: usize, count: usize, key: ItemKey) -> Option<usize> {
        find_row(self.bytes, at, width, count, key)
    }

    /// Every module of the crate by key, in key order: what the cell table
    /// ([`crate::names_cells::table`]) writes a row of each for.
    pub fn modules(&self) -> impl ExactSizeIterator<Item = ItemKey> + use<'m> {
        let (bytes, count) = (self.bytes, self.head.modules);
        (0..count).map(move |i| key_at(bytes, HEADER + i * ROW))
    }

    /// Every enum of the crate by key, in key order.
    pub fn enums(&self) -> impl ExactSizeIterator<Item = ItemKey> + use<'m> {
        let (bytes, at, count) = (self.bytes, self.head.enums_at, self.head.enums);
        (0..count).map(move |i| key_at(bytes, at + i * ENUM_ROW))
    }

    /// The module's row: where it starts in the bytes.
    fn row(&self, module: ItemKey) -> Option<usize> {
        self.find(HEADER, ROW, self.head.modules, module).map(|i| HEADER + i * ROW)
    }

    /// The module's solved scope, viewed in place; `None` for no such module or a failed one.
    pub fn scope(&self, module: ItemKey) -> Option<ScopeView<'m>> {
        let bytes: &'m [u8] = self.bytes;
        let row = self.row(module)?;
        let at = le32(bytes, row + 32);
        if at == NONE {
            return None;
        }
        let start = self.head.blob_at + at as usize;
        let part = &bytes[start..start + le32(bytes, row + 36) as usize];
        // SAFETY: `Verified::check`, the one maker of the proof this record was opened from,
        // ran `ScopeView::read` on exactly these bytes: the same row of the same immutable
        // `'m` bytes gives the same range at the same address, and `read` returned `Ok`.
        Some(unsafe { ScopeView::trusted(part) })
    }

    /// The module's chain up to the root, itself first: a slice of the bytes.
    pub fn chain(&self, module: ItemKey) -> Option<&'m [ItemKey]> {
        let bytes: &'m [u8] = self.bytes;
        let row = self.row(module)?;
        let first = le32(bytes, row + 40);
        if first == NONE {
            return None;
        }
        let len = usize::from(le16(bytes, row + 44));
        let start = self.head.chains_at + first as usize * KEY;
        let (words, _) = bytes[start..start + len * KEY].as_chunks::<KEY>();
        Some(ItemKey::slice_of(words))
    }

    fn no_implicit_prelude_here(&self, module: ItemKey) -> bool {
        self.row(module).is_some_and(|row| le16(self.bytes, row + 46) & NO_IMPLICIT_PRELUDE != 0)
    }

    /// Whether `module` or an ancestor is `#[no_implicit_prelude]`.
    pub fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        self.chain(module).is_some_and(|c| c.iter().any(|m| self.no_implicit_prelude_here(*m)))
    }

    /// Whether `module` is a block's module (row flag bit 1).
    pub fn anonymous(&self, module: ItemKey) -> bool {
        self.row(module).is_some_and(|row| le16(self.bytes, row + 46) & ANONYMOUS != 0)
    }

    /// The refused call of this crate under `call` (a [`crate::names::Why::Refused`]'s `call`):
    /// its path, shape, file, whether it is D104's implicit `::std::main`, and why it was
    /// refused, read back equal to the solve's ([`refused_row`]). `None`: no call of this crate
    /// was refused under that key. Found by binary search in place; the path, file and words
    /// are slices of the bytes, and a failure's reason, the one part with a working form, is
    /// decoded into `alloc` (the caller's: the record holds no allocator).
    pub fn failed<'b>(&self, call: ItemKey, alloc: Bump<'b>) -> Result<Option<FailedRow<'b>>, NamesRecordError>
    where
        'm: 'b,
    {
        let bytes: &'m [u8] = self.bytes;
        let s = &bytes[self.head.failed_at..self.head.failed_at + self.head.failed_len];
        let count = le32(s, 0) as usize;
        let Some(i) = self.find(self.head.failed_at + ALIGN, FAILED_ROW, count, call) else { return Ok(None) };
        let row = ALIGN + i * FAILED_ROW;
        let texts_at = ALIGN + count * FAILED_ROW;
        let text = |at: usize, len: usize| -> &'m str {
            let start = texts_at + at;
            // SAFETY: `Verified::check`, the one maker of this record's proof, checked this
            // row's path, file and item-name ranges inside the section and their bytes UTF-8
            // (`str::from_utf8`), in the same immutable `'m` bytes.
            unsafe { core::str::from_utf8_unchecked(&s[start..start + len]) }
        };
        let path = text(le32(s, row + 32) as usize, le32(s, row + 36) as usize);
        let file = text(le32(s, row + 52) as usize, le32(s, row + 56) as usize);
        let on = text(le32(s, row + 52) as usize + le32(s, row + 56) as usize, le32(s, row + 60) as usize);
        let (record_at, record_len) = (le32(s, row + 40) as usize, le32(s, row + 44) as usize);
        let record = &s[record_at..record_at + record_len];
        let (implicit, shape, kind) = row_flags(le32(s, row + 48));
        let (Some(shape), Some(kind)) = (shape, kind) else { return Err(NamesRecordError::FailedFlags { call }) };
        let why = match kind {
            RefusalKind::Failed => CallRefusal::Failed(blaze_expand::Failed::read_record(record, alloc).map_err(|error| NamesRecordError::FailedRecord { call, error })?),
            kind => {
                // SAFETY: `Verified::check` checked the words of a row of any kind but
                // `Failed` UTF-8, in the same immutable `'m` bytes.
                let words: &'m str = unsafe { core::str::from_utf8_unchecked(record) };
                CallRefusal::Refused { kind, words }
            }
        };
        Ok(Some(FailedRow { call, path, shape, implicit, file, on, why }))
    }

    /// The crate root, viewed in place.
    pub fn crate_root(&self) -> CrateRootView<'m> {
        let bytes: &'m [u8] = self.bytes;
        let part = &bytes[self.head.root_at..self.head.root_at + self.head.root_len];
        // SAFETY: `Verified::check` ran `CrateRootView::read` on exactly these bytes of the
        // same immutable `'m` record, and it returned `Ok`.
        unsafe { CrateRootView::trusted(part) }
    }

    /// An enum's variants, or `None` when `owner` is no enum of this crate.
    pub fn variants(&self, owner: ItemKey) -> Option<Variants<'m>> {
        let i = self.find(self.head.enums_at, ENUM_ROW, self.head.enums, owner)?;
        let row = self.head.enums_at + i * ENUM_ROW;
        let first = le32(self.bytes, row + 32) as usize;
        Some(Variants {
            bytes: self.bytes,
            strings_at: self.head.strings_at,
            rows_at: self.head.variants_at + first * VARIANT_ROW,
            count: le32(self.bytes, row + 36) as usize,
        })
    }
}

/// An enum's variants as rows of the record, in declaration order.
#[derive(Clone, Copy, Debug)]
pub struct Variants<'m> {
    bytes: &'m [u8],
    strings_at: usize,
    rows_at: usize,
    count: usize,
}

impl<'m> Variants<'m> {
    /// `count` variant rows ([`VARIANT_ROW_WIDTH`] each) at `rows_at` of `bytes`, their names
    /// ranges of the strings at `strings_at`: another record's run of the same rows (the
    /// `CrateScope` exports record, crate::scope_record). O(1), nothing read.
    ///
    /// # Safety
    ///
    /// The maker of the caller's proof checked, in these same immutable `'m` bytes, that every
    /// one of the rows lies inside `bytes` and that each name's range lies inside `bytes` and is
    /// UTF-8 (`str::from_utf8` returned `Ok`): what [`Variants::at`]'s unchecked read relies on.
    pub(crate) unsafe fn trusted(bytes: &'m [u8], strings_at: usize, rows_at: usize, count: usize) -> Variants<'m> {
        Variants { bytes, strings_at, rows_at, count }
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn name_bytes(&self, row: usize) -> &'m [u8] {
        let bytes: &'m [u8] = self.bytes;
        let start = self.strings_at + le32(bytes, row + 32) as usize;
        &bytes[start..start + le32(bytes, row + 36) as usize]
    }

    /// Variant `i` of `0..len()`.
    pub(crate) fn at(&self, i: usize) -> Variant<&'m str> {
        let row = self.rows_at + i * VARIANT_ROW;
        // SAFETY: `Verified::check` ran `str::from_utf8` on exactly this range of the same
        // immutable `'m` record, and it returned `Ok`.
        let name = unsafe { core::str::from_utf8_unchecked(self.name_bytes(row)) };
        Variant { name, key: key_at(self.bytes, row), record: self.bytes[row + 40] != 0 }
    }

    /// Variant `i`.
    pub fn get(&self, i: usize) -> Option<Variant<&'m str>> {
        (i < self.count).then(|| self.at(i))
    }

    /// Every variant, in declaration order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Variant<&'m str>> + use<'m> {
        let all = *self;
        (0..all.count).map(move |i| all.at(i))
    }

    /// The variant named `name`, comparing the bytes in place.
    pub fn find(&self, name: &str) -> Option<VariantHit> {
        (0..self.count).map(|i| self.rows_at + i * VARIANT_ROW).find(|row| self.name_bytes(*row) == name.as_bytes()).map(|row| VariantHit {
            key: key_at(self.bytes, row),
            record: self.bytes[row + 40] != 0,
        })
    }
}

/// Where a reader gets a crate's names record: through the caller's effect layer (a kept file,
/// the store), 16-aligned, the first time the crate is asked about.
pub trait LoadNames<'a> {
    /// The names record of the crate at `krate` (an index of the keys the reader was given), or
    /// `None` when none is kept.
    fn load(&self, krate: usize) -> Option<&'a [u8]>;
}

/// What a reader opened so far: crates and their records' bytes. Nothing else is counted, as
/// nothing is decoded.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Decoded {
    /// Records opened (crates asked about), each checked once.
    pub crates: usize,
    /// Their bytes.
    pub bytes: usize,
}

/// The full names of a set of crates, through their names records, each crate's record checked
/// the first time something asks about it (its [`Verified`] kept: one pointer and a header a
/// crate, the only state); every answer after is read in place. The `World` the path walk
/// ([`crate::names::path`]) takes, with the answers of the solved crates' own
/// [`crate::resolve::Names`], for any name type.
pub struct RecordNames<'a, L> {
    keys: AVec<'a, CrateItemKey>,
    by_prefix: HashMap<[u8; 8], usize, Alloc<'a>>,
    proofs: AVec<'a, OnceCell<Option<Verified<'a>>>>,
    load: L,
}

impl<'a, L: LoadNames<'a>> RecordNames<'a, L> {
    /// `keys`: each crate by index, as `load` knows them. Nothing is read here; the tables are
    /// sized by the crates, never by their modules.
    pub fn new(keys: &[CrateItemKey], load: L, alloc: Alloc<'a>) -> RecordNames<'a, L> {
        let mut by_prefix = HashMap::new_in(alloc);
        for (i, k) in keys.iter().enumerate() {
            by_prefix.insert(k.prefix(), i);
        }
        let mut proofs = AVec::with_capacity_in(keys.len(), alloc);
        proofs.resize_with(keys.len(), OnceCell::new);
        RecordNames { keys: AVec::from_slice_in(keys, alloc), by_prefix, proofs, load }
    }

    /// The crate whose key `key` (any item of it) begins with.
    pub fn crate_of(&self, key: ItemKey) -> Option<usize> {
        let mut p = [0u8; 8];
        p.copy_from_slice(&key.0.0[..8]);
        self.by_prefix.get(&p).copied()
    }

    /// The crate's record, checked the first time asked. A record that does not check is a
    /// writer's bug (its bytes were verified when fetched): it fails this command, naming the
    /// crate.
    pub fn record(&self, krate: usize) -> Option<NamesRecord<'a>> {
        let proof = self.proofs.get(krate)?.get_or_init(|| {
            let bytes = self.load.load(krate)?;
            Some(Verified::check(bytes).unwrap_or_else(|e| panic!("the names record of crate {:?} does not check: {e:?}", self.keys[krate])))
        });
        proof.map(NamesRecord::open)
    }

    /// What was opened so far, summed over the crates.
    pub fn decoded(&self) -> Decoded {
        let mut sum = Decoded::default();
        for proof in self.proofs.iter().filter_map(|c| c.get()).flatten() {
            sum.crates += 1;
            sum.bytes += proof.byte_len();
        }
        sum
    }

    fn of(&self, key: ItemKey) -> Option<NamesRecord<'a>> {
        self.record(self.crate_of(key)?)
    }
}

impl<'a, N: Name, L: LoadNames<'a>> World<N> for RecordNames<'a, L> {
    type Scope<'w>
        = ScopeView<'w>
    where
        Self: 'w;
    type Root<'w>
        = CrateRootView<'w>
    where
        Self: 'w;

    fn scope(&self, module: ItemKey) -> Option<ScopeView<'_>> {
        self.of(module)?.scope(module)
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.of(module)?.chain(module)
    }

    fn root_of(&self, key: ItemKey) -> Option<CrateRootView<'_>> {
        Some(self.of(key)?.crate_root())
    }

    fn root(&self, krate: CrateItemKey) -> Option<CrateRootView<'_>> {
        Some(self.record(*self.by_prefix.get(&krate.prefix())?)?.crate_root())
    }

    fn variant(&self, owner: ItemKey, name: N) -> Option<Option<VariantHit>> {
        Some(self.of(owner)?.variants(owner)?.find(name.as_ref()))
    }

    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        self.of(module).is_some_and(|r| r.no_implicit_prelude(module))
    }

    fn anonymous(&self, module: ItemKey) -> bool {
        self.of(module).is_some_and(|r| r.anonymous(module))
    }
}

/// One crate's record alone as a world: a walk through another crate's module stops with
/// `Why::NotRead`. What a job that holds one crate's proof reads (body_types.rs), and the test.
impl<'m, N: Name> World<N> for NamesRecord<'m> {
    type Scope<'w>
        = ScopeView<'w>
    where
        Self: 'w;
    type Root<'w>
        = CrateRootView<'w>
    where
        Self: 'w;

    fn scope(&self, module: ItemKey) -> Option<ScopeView<'_>> {
        NamesRecord::scope(self, module)
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        NamesRecord::chain(self, module)
    }

    fn root_of(&self, key: ItemKey) -> Option<CrateRootView<'_>> {
        (key.crate_prefix() == self.head.key.prefix()).then(|| self.crate_root())
    }

    fn root(&self, krate: CrateItemKey) -> Option<CrateRootView<'_>> {
        (krate == self.head.key).then(|| self.crate_root())
    }

    fn variant(&self, owner: ItemKey, name: N) -> Option<Option<VariantHit>> {
        Some(self.variants(owner)?.find(name.as_ref()))
    }

    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        NamesRecord::no_implicit_prelude(self, module)
    }

    fn anonymous(&self, module: ItemKey) -> bool {
        NamesRecord::anonymous(self, module)
    }
}
