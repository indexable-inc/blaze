//! The state a macro's module keeps between calls: the statics of its crates that its running
//! code can write, as the link lists them in the module's `blaze.statics` custom section
//! (blaze-link's `wasm::STATICS`, whose layout [`List::parse`] reads), each found again among
//! the macro crate's crates by its item key ([`Statics::resolve`]).
//!
//! The standard library's (a sysroot crate's) are not watched, as on the evaluator
//! ([`run::of_sysroot`](crate::run::of_sysroot)): its state is the runtime's, made again in
//! each instance. Nor is what no Rust static holds (wasi-libc's data, the heap): only what a
//! static of the macro's own crates holds, or points to, is its state.
//!
//! A module with no list, an entry of a crate the macro crate does not reach, or one that names
//! no static of it, is refused at load: none is run as if it kept no state.

use std::ops::Range;

use base_db::{Crate, Db};
use blaze_format::leb;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_items::{ItemId, ItemKey, key::Resolved};
use hir_def::StaticId;
use std::alloc::Global;

/// The custom section the link writes the list in.
pub(crate) const SECTION: &str = "blaze.statics";
/// An entry's bytes before its records: `__heap_base`, the item key, the count.
const HEADER: usize = 40;
const RECORD: usize = 8;
const MAGIC: &[u8] = b"\0asm\x01\0\0\0";

/// The module's list as written: each static's item key and its records' range of `records`,
/// each record an address and a length in the module's memory.
#[derive(Debug, Default)]
pub(crate) struct List {
    heap_base: u32,
    entries: Vec<(ContentId, Range<usize>)>,
    records: Vec<(u32, u32)>,
}

/// The `n`th little-endian word of `bytes` from `at`.
fn word(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(*bytes.get(at..at + 4)?.first_chunk()?))
}

impl List {
    /// The list in the module `module`, or why there is none: a module the link wrote always
    /// has one, since std's statics are in every proc macro's module.
    pub(crate) fn parse(module: &[u8]) -> Result<List, String> {
        let Some(mut at) = module.starts_with(MAGIC).then_some(MAGIC.len()) else {
            return Err("is no wasm module of version 1".to_owned()); // alloc: error path
        };
        let mut found: Option<&[u8]> = None;
        while at < module.len() {
            let id = module[at];
            at += 1;
            let len = match leb::read_u32(module, &mut at) {
                Ok(len) => len,
                Err(error) => {
                    return Err(format!("has a section at {at} whose size can't be read: {error}"));
                } // alloc: error path
            };
            let Some(payload) = module.get(at..at + len as usize) else {
                return Err(format!("has a section at {at} of {len} bytes, past its end")); // alloc: error path
            };
            at += len as usize;
            if id != 0 {
                continue;
            }
            let mut name_at = 0;
            let name = leb::read_u32(payload, &mut name_at)
                .ok()
                .and_then(|n| payload.get(name_at..name_at + n as usize));
            if name != Some(SECTION.as_bytes()) {
                continue;
            }
            if found.is_some() {
                return Err(format!("has two `{SECTION}` sections")); // alloc: error path
            }
            found = Some(&payload[name_at + SECTION.len()..]);
        }
        let Some(bytes) = found else {
            // alloc: error path
            return Err(format!(
                "has no `{SECTION}` section listing the statics its code can write, which blaze's \
                 link writes in every module: without it, the state its macros keep between calls \
                 can't be seen"
            ));
        };
        Self::entries(bytes)
    }

    /// The entries of the section's `bytes`.
    fn entries(bytes: &[u8]) -> Result<List, String> {
        let mut list = List::default();
        let mut at = 0;
        let bad =
            |at: usize, what: &str| format!("has a `{SECTION}` section whose entry at {at} {what}"); // alloc: error path
        while at < bytes.len() {
            let (Some(heap_base), Some(count)) = (word(bytes, at), word(bytes, at + 36)) else {
                return Err(bad(at, "is cut short"));
            };
            let item = ContentId(*bytes[at + 4..at + 36].first_chunk().expect("32 bytes in range"));
            if count == 0 {
                return Err(bad(at, "has no records"));
            }
            if !list.entries.is_empty() && heap_base != list.heap_base {
                return Err(bad(
                    at,
                    &format!(
                        "says the heap starts at {heap_base}, another says {}",
                        list.heap_base
                    ),
                ));
            }
            list.heap_base = heap_base;
            let start = list.records.len();
            for n in 0..count as usize {
                let record = at + HEADER + RECORD * n;
                let (Some(addr), Some(len)) = (word(bytes, record), word(bytes, record + 4)) else {
                    return Err(bad(at, "is cut short"));
                };
                if u64::from(addr) + u64::from(len) > u64::from(heap_base) {
                    return Err(bad(
                        at,
                        &format!("has {len} bytes at {addr}, past the heap's start {heap_base}"),
                    ));
                }
                list.records.push((addr, len));
            }
            list.entries.push((item, start..list.records.len()));
            at += HEADER + RECORD * count as usize;
        }
        Ok(list)
    }
}

/// A watched static: which, its path as errors name it, and its records' range of
/// [`Statics::records`].
#[derive(Debug)]
pub(crate) struct Watched {
    pub(crate) id: StaticId,
    pub(crate) path: Box<str>,
    pub(crate) records: Range<usize>,
}

/// The statics a module's expansions watch and carry: the list's entries of the macro's own
/// crates, each found by its key ([`Statics::resolve`]); none until then, for a list that has any.
#[derive(Debug, Default)]
pub(crate) struct Statics {
    pub(crate) heap_base: u32,
    pub(crate) watched: Vec<Watched>,
    /// Each record's address and length, by [`Watched::records`].
    pub(crate) records: Vec<(u32, u32)>,
    /// Whether the list was resolved, or had nothing to resolve.
    pub(crate) resolved: bool,
}

impl Statics {
    /// What `list` watches before it is resolved: nothing, and resolved only when it lists
    /// nothing.
    pub(crate) fn unresolved(list: &List) -> Statics {
        Statics {
            heap_base: list.heap_base,
            resolved: list.entries.is_empty(),
            ..Statics::default()
        }
    }

    /// The entries of `list` of the crates `krate` (the proc-macro crate the module was built
    /// from) reaches, but the sysroot's, each found as a static of the database by its item key.
    pub(crate) fn resolve(db: &Db, krate: Crate, list: &List) -> Result<Statics, String> {
        // Each crate it reaches by the first 8 bytes of its key, which an item key begins with.
        let mut crates: HashMap<[u8; 8], Crate, Global> = HashMap::new_in(Global);
        // alloc: once a module loaded, the crates to visit
        let mut stack = vec![krate];
        while let Some(it) = stack.pop() {
            let key = blaze_items::key::crate_key(db, it);
            let prefix = *key.0.first_chunk().expect("a key is 32 bytes");
            match crates.insert(prefix, it) {
                Some(other) if other != it => {
                    // alloc: error path
                    return Err(format!(
                        "reaches the crates `{}` and `{}`, whose keys begin alike: their items can't be told apart",
                        other.named(db),
                        it.named(db)
                    ));
                }
                Some(_) => continue,
                None => stack.extend(it.data(db).dependencies.iter().map(|dep| dep.crate_id)),
            }
        }
        let mut statics =
            Statics { heap_base: list.heap_base, resolved: true, ..Statics::default() };
        for (item, records) in &list.entries {
            let prefix = *item.0.first_chunk().expect("a key is 32 bytes");
            let Some(&of) = crates.get(&prefix) else {
                // alloc: error path
                return Err(format!(
                    "lists the static {item:?}, of no crate `{}` reaches: its state can't be named",
                    krate.named(db)
                ));
            };
            if matches!(of.data(db).origin, base_db::CrateOrigin::Lang(_)) {
                continue;
            }
            let id = match blaze_items::key::resolve(db, ItemKey(*item)) {
                Some(Resolved::Item(ItemId::Static(id))) => id,
                found => {
                    // alloc: error path
                    return Err(format!(
                        "lists the static {item:?} of `{}`, which names {}",
                        of.named(db),
                        match found {
                            Some(_) => "something else there",
                            None => "nothing there",
                        }
                    ));
                }
            };
            let start = statics.records.len();
            statics.records.extend_from_slice(&list.records[records.clone()]);
            statics.watched.push(Watched {
                id,
                path: blaze_items::key::path(db, ItemId::Static(id)).into_boxed_str(),
                records: start..statics.records.len(),
            });
        }
        Ok(statics)
    }

    /// The bytes of each watched static's records of `memory`, one after another in the order
    /// watched ([`Statics::span`] says where each static's are), into `out`.
    pub(crate) fn read(&self, memory: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        for &(addr, len) in &self.records {
            let Some(bytes) = memory.get(addr as usize..addr as usize + len as usize) else {
                // alloc: error path
                return Err(format!(
                    "lists {len} bytes at {addr}, past its memory of {}",
                    memory.len()
                ));
            };
            out.extend_from_slice(bytes);
        }
        Ok(())
    }

    /// The range of [`Statics::read`]'s bytes of watched static `at`.
    pub(crate) fn span(&self, at: usize) -> Range<usize> {
        let offset = |records: &[(u32, u32)]| records.iter().map(|it| it.1 as usize).sum::<usize>();
        let watched = &self.watched[at];
        let start = offset(&self.records[..watched.records.start]);
        start..start + offset(&self.records[watched.records.clone()])
    }

    /// Whether `bytes`, watched static `at`'s bytes as [`Statics::read`] gives them, hold an
    /// address of the heap: a word at a 4-byte aligned address of a record that is at or past
    /// `__heap_base` and inside the memory of `memory_len` bytes. An address of the stack is
    /// not told: one left in a static when the call returned points at a frame that is gone,
    /// as it does under rustc.
    pub(crate) fn holds_heap_address(&self, at: usize, bytes: &[u8], memory_len: usize) -> bool {
        let mut offset = 0;
        for &(addr, len) in &self.records[self.watched[at].records.clone()] {
            let record = &bytes[offset..offset + len as usize];
            offset += len as usize;
            let first = (addr.next_multiple_of(4) - addr) as usize;
            let words = record.get(first..).unwrap_or_default().chunks_exact(4);
            if words
                .map(|it| u32::from_le_bytes(*it.first_chunk().expect("4 bytes")))
                .any(|word| word >= self.heap_base && (word as usize) < memory_len)
            {
                return true;
            }
        }
        false
    }

    /// Where `bytes` of watched static `id`, as [`Statics::read`] gives them, are written: each
    /// record's address and its part of the bytes. `None` when `id` is not watched or the bytes
    /// are not its size.
    pub(crate) fn places<'b>(
        &self,
        id: StaticId,
        bytes: &'b [u8],
    ) -> Option<impl Iterator<Item = (u32, &'b [u8])> + use<'_, 'b>> {
        let at = self.watched.iter().position(|it| it.id == id)?;
        (self.span(at).len() == bytes.len()).then(|| {
            let mut offset = 0;
            self.records[self.watched[at].records.clone()].iter().map(move |&(addr, len)| {
                let part = &bytes[offset..offset + len as usize];
                offset += len as usize;
                (addr, part)
            })
        })
    }
}

#[cfg(test)]
pub(crate) mod encode {
    //! The list as the link writes it, for the tests' modules.

    /// One entry: `__heap_base`, the item key, then each record.
    pub(crate) fn entry(heap_base: u32, item: &[u8; 32], records: &[(u32, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&heap_base.to_le_bytes());
        out.extend_from_slice(item);
        out.extend_from_slice(&(records.len() as u32).to_le_bytes());
        for (addr, len) in records {
            out.extend_from_slice(&addr.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
        }
        out
    }

    /// `bytes` as a wat string's contents, each byte escaped.
    pub(crate) fn wat_string(bytes: &[u8]) -> String {
        bytes.iter().map(|it| format!("\\{it:02x}")).collect()
    }
}
