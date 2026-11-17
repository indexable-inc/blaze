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
//! failed calls (version 2): per call of the crate whose expansion failed, sorted by call key,
//! its macro path, whether it is D104's implicit `::std::main`, and the matcher's reason as
//! blaze-expand's record of [`blaze_expand::Failed`] ([`NamesRecord::failed`]).
//!
//! The unit of decode is ONE MODULE's scope: the path walk asks for a module's scope
//! (`World::scope`, a `&ScopeBuilder`) one segment at a time, and a body's paths touch a handful
//! of modules of a handful of crates. Opening a record checks its header and section bounds and
//! parses nothing; a row is found by binary search in place; a module's scope, chain, an enum's
//! variants and the crate root are each turned into their working form the first time asked,
//! then kept by that crate's reader for the rest of the command.

use core::cell::{Cell, OnceCell};

use blaze_format::FormatError;
use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{BuildError, CrateRootBuilder, CrateRootView, RecordBytes, ScopeBuilder, ScopeView};

use crate::names::World;
use crate::resolve::{CrateNames, ExpandNote, NamesNote};
use crate::task::Variant;

const MAGIC: [u8; 4] = *b"BZNM";
/// 2: the failed-calls section (implicit-main-reason, 2026-11-16). A version-1 record is
/// [`NamesRecordError::NotThisRecord`]: an old home's names are made again (rule 26).
/// 3: row flag bit 1, a block's module (super-in-block-1, 2026-11-16); an older record has it
/// clear everywhere and would read `super` in a block wrong.
const VERSION: u32 = 3;
/// A failed call's row: call key, path at and length, record at and length, flags, padding.
const FAILED_ROW: usize = 64;
const FAILED_IMPLICIT: u32 = 1;
const ALIGN: usize = 16;
const HEADER: usize = 128;
const ROW: usize = 48;
const ENUM_ROW: usize = 40;
const VARIANT_ROW: usize = 48;
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
    /// A section runs past the end of the record.
    Short { what: &'static str, at: usize },
    /// A module's `ModuleScope` record did not read.
    Scope { module: ItemKey, error: FormatError },
    /// The `CrateRoot` record did not read.
    Root(FormatError),
    /// A variant's name is not UTF-8.
    Name { owner: ItemKey },
    /// A failed call's macro path is not UTF-8.
    FailedPath { call: ItemKey },
    /// A failed call's reason did not read as blaze-expand's record of a failure.
    FailedRecord { call: ItemKey, error: blaze_expand::RecordError },
}

/// A failed macro call of the crate, read from its names record: the call, the macro's path as
/// written (`::std::main` for D104's implicit call over the root `fn main`), and why it failed,
/// the matcher's own typed reason.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FailedRow<'a> {
    pub call: ItemKey,
    pub path: &'a str,
    pub implicit: bool,
    pub failed: blaze_expand::Failed<'a>,
}

/// The failed-calls section of `names` (the module doc says its form). A function of the
/// solve's notes alone, sorted at this boundary (rule 25).
fn failed_section<'b>(names: &CrateNames<'_>, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, NamesRecordError> {
    let mut calls = AVec::new_in(alloc);
    for note in names.notes.iter() {
        if let NamesNote::Expand { call, note: ExpandNote::Failed { path, implicit, failed }, .. } = note {
            calls.push((*call, *path, *implicit, failed.write_record(alloc)));
        }
    }
    calls.sort_unstable_by(|a, b| (a.0, &a.3[..]).cmp(&(b.0, &b.3[..])));
    calls.dedup_by_key(|(call, ..)| *call);
    let mut paths = RecordBytes::new_in(alloc);
    for (_, path, ..) in calls.iter() {
        paths.extend_from_slice(path.as_bytes());
    }
    pad(&mut paths);
    let records_at = ALIGN + calls.len() * FAILED_ROW + paths.len();
    let mut out = RecordBytes::new_in(alloc);
    out.extend_from_slice(&u32_of(calls.len(), "failed calls")?.to_le_bytes());
    out.resize(ALIGN, 0);
    let (mut path_at, mut record_at) = (0usize, records_at);
    for (call, path, implicit, record) in calls.iter() {
        out.extend_from_slice(&call.0.0);
        out.extend_from_slice(&u32_of(path_at, "failed call paths")?.to_le_bytes());
        out.extend_from_slice(&u32_of(path.len(), "a failed call's path")?.to_le_bytes());
        out.extend_from_slice(&u32_of(record_at, "failed call records")?.to_le_bytes());
        out.extend_from_slice(&u32_of(record.len(), "a failed call's record")?.to_le_bytes());
        out.extend_from_slice(&(if *implicit { FAILED_IMPLICIT } else { 0 }).to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        path_at += path.len();
        record_at = (record_at + record.len()).next_multiple_of(ALIGN);
    }
    out.extend_from_slice(&paths);
    for (.., record) in calls.iter() {
        out.extend_from_slice(record);
        pad(&mut out);
    }
    Ok(out)
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

fn le32(bytes: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(b)
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn key_at(bytes: &[u8], at: usize) -> ItemKey {
    let mut k = [0u8; 32];
    k.copy_from_slice(&bytes[at..at + 32]);
    ItemKey(ContentId(k))
}

/// What a reader decoded so far: the measure of "decoded on demand".
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Decoded {
    /// Records opened (crates asked about).
    pub crates: usize,
    /// Records' bytes, opened.
    pub bytes: usize,
    /// Module scopes turned into their working form.
    pub scopes: usize,
    /// Their records' bytes.
    pub scope_bytes: usize,
}

/// One crate's names record, read in place; each part decoded the first time it is asked.
pub struct NamesRecord<'a> {
    bytes: &'a [u8],
    key: CrateItemKey,
    root_module: ItemKey,
    modules: usize,
    enums: usize,
    chains_at: usize,
    enums_at: usize,
    variants_at: usize,
    strings_at: usize,
    root_at: usize,
    root_len: usize,
    blob_at: usize,
    /// The failed-calls section (version 2).
    failed_at: usize,
    failed_len: usize,
    scopes: AVec<'a, OnceCell<ScopeBuilder<'a, &'a str>>>,
    chains: AVec<'a, OnceCell<AVec<'a, ItemKey>>>,
    variants: AVec<'a, OnceCell<AVec<'a, Variant<&'a str>>>>,
    root: OnceCell<CrateRootBuilder<'a, &'a str>>,
    decoded: Cell<Decoded>,
    alloc: Alloc<'a>,
}

impl<'a> NamesRecord<'a> {
    /// Checks the header and that every section lies inside `bytes`; parses nothing else.
    pub fn open(bytes: &'a [u8], alloc: Alloc<'a>) -> Result<NamesRecord<'a>, NamesRecordError> {
        if bytes.as_ptr() as usize % ALIGN != 0 {
            return Err(NamesRecordError::Unaligned);
        }
        if bytes.len() < HEADER || bytes[..4] != MAGIC || le32(bytes, 4) != VERSION {
            return Err(NamesRecordError::NotThisRecord);
        }
        let f = |i: usize| le32(bytes, 8 + 4 * i) as usize;
        let (modules, chain_keys, enums, variants) = (f(0), f(1), f(2), f(3));
        let (chains_at, enums_at, variants_at, strings_at, root_at, root_len, blob_at, blob_len) = (f(4), f(5), f(6), f(7), f(8), f(9), f(10), f(11));
        let (failed_at, failed_len) = (f(12), f(13));
        let within = |what: &'static str, at: usize, len: usize| {
            if at.checked_add(len).is_some_and(|end| end <= bytes.len()) { Ok(()) } else { Err(NamesRecordError::Short { what, at }) }
        };
        within("module rows", HEADER, modules * ROW)?;
        within("chain keys", chains_at, chain_keys * 32)?;
        within("enum rows", enums_at, enums * ENUM_ROW)?;
        within("variant rows", variants_at, variants * VARIANT_ROW)?;
        within("strings", strings_at, root_at.saturating_sub(strings_at))?;
        within("crate root", root_at, root_len)?;
        within("blob", blob_at, blob_len)?;
        within("failed calls", failed_at, failed_len)?;
        if failed_len < ALIGN || failed_at % ALIGN != 0 || failed_len < ALIGN + le32(bytes, failed_at) as usize * FAILED_ROW {
            return Err(NamesRecordError::Short { what: "failed call rows", at: failed_at });
        }
        fn cells<'a, T>(n: usize, alloc: Alloc<'a>) -> AVec<'a, OnceCell<T>> {
            let mut v = AVec::with_capacity_in(n, alloc);
            v.resize_with(n, OnceCell::new);
            v
        }
        let decoded = Decoded { crates: 1, bytes: bytes.len(), ..Decoded::default() };
        Ok(NamesRecord {
            bytes,
            key: CrateItemKey(key_at(bytes, HEADER - 64).0),
            root_module: key_at(bytes, HEADER - 32),
            modules,
            enums,
            chains_at,
            enums_at,
            variants_at,
            strings_at,
            root_at,
            root_len,
            blob_at,
            failed_at,
            failed_len,
            scopes: cells(modules, alloc),
            chains: cells(modules, alloc),
            variants: cells(enums, alloc),
            root: OnceCell::new(),
            decoded: Cell::new(decoded),
            alloc,
        })
    }

    pub fn key(&self) -> CrateItemKey {
        self.key
    }

    pub fn root_module(&self) -> ItemKey {
        self.root_module
    }

    /// The failed call of this crate under `call` (a [`crate::names::Why::Refused`]'s `call`):
    /// its path, whether it is D104's implicit `::std::main`, and the matcher's reason, read back
    /// equal to the solve's [`ExpandNote::Failed`]. `None`: no call of this crate failed under
    /// that key. Found by binary search in place; the reason is decoded into the reader's alloc.
    pub fn failed(&self, call: ItemKey) -> Result<Option<FailedRow<'a>>, NamesRecordError> {
        let s = &self.bytes[self.failed_at..self.failed_at + self.failed_len];
        let count = le32(s, 0) as usize;
        let paths_at = ALIGN + count * FAILED_ROW;
        let (mut lo, mut hi) = (0usize, count);
        let row = loop {
            if lo >= hi {
                return Ok(None);
            }
            let mid = lo + (hi - lo) / 2;
            let at = ALIGN + mid * FAILED_ROW;
            match key_at(s, at).cmp(&call) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => break at,
            }
        };
        let (path_at, path_len) = (le32(s, row + 32) as usize, le32(s, row + 36) as usize);
        let (record_at, record_len) = (le32(s, row + 40) as usize, le32(s, row + 44) as usize);
        let flags = le32(s, row + 48);
        let short = |what: &'static str, at: usize| NamesRecordError::Short { what, at: self.failed_at + at };
        let raw = s.get(paths_at + path_at..paths_at + path_at + path_len).ok_or(short("a failed call's path", paths_at + path_at))?;
        let path = core::str::from_utf8(raw).map_err(|_| NamesRecordError::FailedPath { call })?;
        let record = s.get(record_at..record_at + record_len).ok_or(short("a failed call's record", record_at))?;
        let failed = blaze_expand::Failed::read_record(record, self.alloc).map_err(|error| NamesRecordError::FailedRecord { call, error })?;
        Ok(Some(FailedRow { call, path, implicit: flags & FAILED_IMPLICIT != 0, failed }))
    }

    pub fn decoded(&self) -> Decoded {
        self.decoded.get()
    }

    /// Binary search of fixed-width rows of `width` bytes at `at`, keyed by their first 32.
    fn find(&self, at: usize, width: usize, count: usize, key: ItemKey) -> Option<usize> {
        let (mut lo, mut hi) = (0usize, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let row = at + mid * width;
            match self.bytes[row..row + 32].cmp(&key.0.0[..]) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    fn row(&self, module: ItemKey) -> Option<(usize, usize)> {
        self.find(HEADER, ROW, self.modules, module).map(|i| (i, HEADER + i * ROW))
    }

    /// The module's solved scope, decoded on first ask; `Ok(None)` for no such module or a failed
    /// one.
    pub fn scope(&self, module: ItemKey) -> Result<Option<&ScopeBuilder<'a, &'a str>>, NamesRecordError> {
        let Some((i, row)) = self.row(module) else { return Ok(None) };
        if let Some(scope) = self.scopes[i].get() {
            return Ok(Some(scope));
        }
        let (at, len) = (le32(self.bytes, row + 32), le32(self.bytes, row + 36) as usize);
        if at == NONE {
            return Ok(None);
        }
        let start = self.blob_at + at as usize;
        let part = self.bytes.get(start..start + len).ok_or(NamesRecordError::Short { what: "a module scope", at: start })?;
        let view = ScopeView::read(part).map_err(|error| NamesRecordError::Scope { module, error })?;
        let mut d = self.decoded.get();
        d.scopes += 1;
        d.scope_bytes += len;
        self.decoded.set(d);
        Ok(Some(self.scopes[i].get_or_init(|| ScopeBuilder::from(&view, self.alloc))))
    }

    /// The module's chain up to the root, itself first.
    pub fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        let (i, row) = self.row(module)?;
        let first = le32(self.bytes, row + 40);
        if first == NONE {
            return None;
        }
        let chain = self.chains[i].get_or_init(|| {
            let len = usize::from(le16(self.bytes, row + 44));
            let mut chain = AVec::with_capacity_in(len, self.alloc);
            for k in 0..len {
                chain.push(key_at(self.bytes, self.chains_at + (first as usize + k) * 32));
            }
            chain
        });
        Some(chain.as_slice())
    }

    fn no_implicit_prelude_here(&self, module: ItemKey) -> bool {
        self.row(module).is_some_and(|(_, row)| le16(self.bytes, row + 46) & NO_IMPLICIT_PRELUDE != 0)
    }

    /// Whether `module` or an ancestor is `#[no_implicit_prelude]`.
    pub fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        self.chain(module).is_some_and(|c| c.iter().any(|m| self.no_implicit_prelude_here(*m)))
    }

    /// Whether `module` is a block's module (row flag bit 1).
    pub fn anonymous(&self, module: ItemKey) -> bool {
        self.row(module).is_some_and(|(_, row)| le16(self.bytes, row + 46) & ANONYMOUS != 0)
    }

    /// The crate root record, decoded on first ask.
    pub fn crate_root(&self) -> Result<&CrateRootBuilder<'a, &'a str>, NamesRecordError> {
        if let Some(root) = self.root.get() {
            return Ok(root);
        }
        let part = &self.bytes[self.root_at..self.root_at + self.root_len];
        let view = CrateRootView::read(part).map_err(NamesRecordError::Root)?;
        Ok(self.root.get_or_init(|| CrateRootBuilder::from(&view, self.alloc)))
    }

    /// An enum's variants, or `Ok(None)` when `owner` is no enum of this crate.
    pub fn variants(&self, owner: ItemKey) -> Result<Option<&[Variant<&'a str>]>, NamesRecordError> {
        let Some(i) = self.find(self.enums_at, ENUM_ROW, self.enums, owner) else { return Ok(None) };
        if let Some(v) = self.variants[i].get() {
            return Ok(Some(v.as_slice()));
        }
        let row = self.enums_at + i * ENUM_ROW;
        let (first, count) = (le32(self.bytes, row + 32) as usize, le32(self.bytes, row + 36) as usize);
        let mut list = AVec::with_capacity_in(count, self.alloc);
        for k in first..first + count {
            let v = self.variants_at + k * VARIANT_ROW;
            let (at, len) = (le32(self.bytes, v + 32) as usize, le32(self.bytes, v + 36) as usize);
            let start = self.strings_at + at;
            let raw: &'a [u8] = self.bytes.get(start..start + len).ok_or(NamesRecordError::Short { what: "a variant name", at: start })?;
            let name = core::str::from_utf8(raw).map_err(|_| NamesRecordError::Name { owner })?;
            list.push(Variant { name, key: key_at(self.bytes, v), record: self.bytes[v + 40] != 0 });
        }
        Ok(Some(self.variants[i].get_or_init(|| list).as_slice()))
    }
}

/// Where a reader gets a crate's names record: through the caller's effect layer (a kept file,
/// the store), 16-aligned, the first time the crate is asked about.
pub trait LoadNames<'a> {
    /// The names record of the crate at `krate` (an index of the keys the reader was given), or
    /// `None` when none is kept.
    fn load(&self, krate: usize) -> Option<&'a [u8]>;
}

/// The full names of a set of crates, through their names records, each crate opened the first
/// time something asks about it: the `World` the path walk ([`crate::names::path`]) takes, with
/// the answers of the solved crates' own [`crate::resolve::Names`].
pub struct RecordNames<'a, L> {
    keys: AVec<'a, CrateItemKey>,
    by_prefix: HashMap<[u8; 8], usize, Alloc<'a>>,
    open: AVec<'a, OnceCell<Option<NamesRecord<'a>>>>,
    load: L,
    alloc: Alloc<'a>,
}

impl<'a, L: LoadNames<'a>> RecordNames<'a, L> {
    /// `keys`: each crate by index, as `load` knows them. Nothing is read here.
    pub fn new(keys: &[CrateItemKey], load: L, alloc: Alloc<'a>) -> RecordNames<'a, L> {
        let mut by_prefix = HashMap::new_in(alloc);
        for (i, k) in keys.iter().enumerate() {
            by_prefix.insert(k.prefix(), i);
        }
        let mut open = AVec::with_capacity_in(keys.len(), alloc);
        open.resize_with(keys.len(), OnceCell::new);
        RecordNames { keys: AVec::from_slice_in(keys, alloc), by_prefix, open, load, alloc }
    }

    /// The crate whose key `key` (any item of it) begins with.
    pub fn crate_of(&self, key: ItemKey) -> Option<usize> {
        let mut p = [0u8; 8];
        p.copy_from_slice(&key.0.0[..8]);
        self.by_prefix.get(&p).copied()
    }

    /// The crate's record, opened on first ask. A record that does not open is a writer's bug
    /// (its bytes were verified when fetched): it fails this command, naming the crate.
    pub fn record(&self, krate: usize) -> Option<&NamesRecord<'a>> {
        self.open
            .get(krate)?
            .get_or_init(|| {
                let bytes = self.load.load(krate)?;
                Some(NamesRecord::open(bytes, self.alloc).unwrap_or_else(|e| panic!("the names record of crate {:?} did not open: {e:?}", self.keys[krate])))
            })
            .as_ref()
    }

    /// What was decoded so far, summed over the crates opened.
    pub fn decoded(&self) -> Decoded {
        let mut sum = Decoded::default();
        for r in self.open.iter().filter_map(|c| c.get()).flatten() {
            let d = r.decoded();
            sum.crates += d.crates;
            sum.bytes += d.bytes;
            sum.scopes += d.scopes;
            sum.scope_bytes += d.scope_bytes;
        }
        sum
    }

    fn of(&self, key: ItemKey) -> Option<&NamesRecord<'a>> {
        self.record(self.crate_of(key)?)
    }
}

impl<'a, L: LoadNames<'a>> World<&'a str> for RecordNames<'a, L> {
    fn scope(&self, module: ItemKey) -> Option<&ScopeBuilder<'_, &'a str>> {
        let r = self.of(module)?;
        r.scope(module).unwrap_or_else(|e| panic!("a module scope of crate {:?} did not read: {e:?}", r.key()))
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.of(module)?.chain(module)
    }

    fn root_of(&self, key: ItemKey) -> Option<&CrateRootBuilder<'_, &'a str>> {
        let r = self.of(key)?;
        Some(r.crate_root().unwrap_or_else(|e| panic!("the crate root of {:?} did not read: {e:?}", r.key())))
    }

    fn root(&self, krate: CrateItemKey) -> Option<&CrateRootBuilder<'_, &'a str>> {
        let r = self.record(*self.by_prefix.get(&krate.prefix())?)?;
        Some(r.crate_root().unwrap_or_else(|e| panic!("the crate root of {:?} did not read: {e:?}", r.key())))
    }

    fn variants(&self, owner: ItemKey) -> Option<&[Variant<&'a str>]> {
        let r = self.of(owner)?;
        r.variants(owner).unwrap_or_else(|e| panic!("the variants of {owner:?} did not read: {e:?}"))
    }

    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        self.of(module).is_some_and(|r| r.no_implicit_prelude(module))
    }

    fn anonymous(&self, module: ItemKey) -> bool {
        self.of(module).is_some_and(|r| r.anonymous(module))
    }
}
