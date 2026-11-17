//! A crate's full names as a record of their own (per-crate-entry.md, "Runner twenty"): every
//! module's solved scope, chain and `#[no_implicit_prelude]`, every enum's variants and the crate
//! root, so a path written ANYWHERE in the crate resolves through it ([`crate::names::path`])
//! with the answers of a fresh solve. Written when the crate is solved, beside its exports
//! record, inner record and read list; read back per crate, on demand.
//!
//! NEVER COMPILED (lane d114a-inplace, 2026-11-16): the wiring moves this file over
//! names_record.rs whole.
//!
//! The record (integers little-endian, every section 16-aligned): a 128-byte header (`BZNM`,
//! version, then u32 counts and offsets of each section, the crate key, the root module key);
//! module rows, sorted by key, FIXED 48 bytes (key, scope at u32 and length u32 into the blob,
//! `u32::MAX` for a module whose task failed; first chain key u32 (`u32::MAX`: none), chain
//! length u16, flags u16: bit 0 `#[no_implicit_prelude]`); the chain keys (32 bytes each);
//! enum rows, sorted by owner, 40 bytes (owner, first variant u32, count u32); variant rows,
//! 48 bytes (key, name at u32 and length u32 into the strings, record u8, pad); the strings; the
//! crate's `CrateRoot` record; the blob, each module's blaze-surface `ModuleScope` record.
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
use blaze_own::{AVec, Alloc};
use blaze_surface::{BuildError, CrateRootView, RecordBytes, ScopeView};
#[cfg(test)]
use blaze_surface::ScopeBuilder;

use crate::facts::Name;
use crate::names::{VariantHit, World};
use crate::resolve::CrateNames;
use crate::task::Variant;

const MAGIC: [u8; 4] = *b"BZNM";
const VERSION: u32 = 1;
const ALIGN: usize = 16;
const HEADER: usize = 128;
const ROW: usize = 48;
const ENUM_ROW: usize = 40;
const VARIANT_ROW: usize = 48;
const KEY: usize = 32;
const NONE: u32 = u32::MAX;
const NO_IMPLICIT_PRELUDE: u16 = 1;

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
        let flags = if names.info(*m).is_some_and(|i| i.no_implicit_prelude) { NO_IMPLICIT_PRELUDE } else { 0 };
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
    let mut out = RecordBytes::with_capacity_in(blob_at + blob.len(), alloc);
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
    let mut k = [0u8; KEY];
    k.copy_from_slice(&bytes[at..at + KEY]);
    ItemKey(ContentId(k))
}

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
        let (mut lo, mut hi) = (0usize, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let row = at + mid * width;
            match self.bytes[row..row + KEY].cmp(&key.0.0[..]) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
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
    fn at(&self, i: usize) -> Variant<&'m str> {
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
}
