//! The key of one names CELL: a (namespace, name) of one module's table, the `part` a reader's
//! read names (drive.md "Part reads": "one CELL of the record (a 128-bit key)"). One home
//! (rule 30): every names kind (`ModuleScope`, `Imports`, `ExportedMacros`) writes its cells
//! under this key, and the drive's `part(record bytes, cell)` for those kinds searches by it, so
//! a reader's logged cell and the record's row meet without a second rule.
//!
//! Made as the first 16 bytes of BLAKE3 in derive-key mode under the domain
//! [`CELL_DOMAIN`], over the namespace byte ([`Namespace`]'s stable byte: types 0, values 1,
//! macros 2) and the name's UTF-8 bytes length-prefixed ([`Hasher::part`]). The module is NOT in
//! the key: a cell is searched within one module's record, whose own key names the module. The
//! layer (`Full` or `Globs`) is not either: a reader across tables reads `Full` answers only.
//! 128 bits: a collision among a module's cells is a refusal by the record's writer (two names
//! under one key, rule 21), never a quiet merge.

use core::hash::Hasher as _;

use blaze_hash::{FixedHasher, FixedState, Map, MapOpts};
use blaze_id::ItemKey;
use blaze_ids::{ContentId, Domain, Hasher};
use blaze_own::{AVec, Alloc};
use blaze_surface::Namespace;

/// The BLAKE3 domain of a cell key; its text names the schema, so a change of the recipe is a
/// new domain and old reads simply miss.
pub const CELL_DOMAIN: &str = "blaze v2 names cell 1";

/// [`CELL_DOMAIN`] with its BLAKE3 context key derived once, on first use: a key is made per
/// logged read, so re-deriving the context per key was a second compression each time.
static CELL: Domain = Domain::new(CELL_DOMAIN);

/// One names cell's key: see the module doc. Ordered by bytes, so a record's cell rows sort and
/// are found by binary search on this key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct CellKey(pub [u8; 16]);

/// The key of the cell `(ns, name)`: the one function every names kind and the drive's `part`
/// use.
pub fn cell_key(ns: Namespace, name: &str) -> CellKey {
    let id: ContentId = Hasher::of(&CELL).update(&[ns as u8]).part(name.as_bytes()).finish();
    let mut out = [0u8; 16];
    out.copy_from_slice(&id.0[..16]);
    CellKey(out)
}

/// The most distinct `(module, namespace, name)` reads one [`BindingKeys`] keeps (rule 18): the
/// recorder's own cap on a task's reads (blaze-ladder front2 tables/recorder.rs `MOST_READS`),
/// so a task that reads more is refused there before the memo is full.
pub const MOST_BINDING_KEYS: usize = 1 << 20;

/// A binding read's cell key as [`BindingKeys::key`] answers it: the key, and whether this is
/// the first time the memo was asked it (`fresh`). A read that is not fresh was answered
/// before with the same key, so a sink that keeps each read once may drop it at once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BindingKey {
    pub key: CellKey,
    pub fresh: bool,
}

/// One distinct binding read: its module, namespace and name (bytes `at..at + len` of the
/// memo's name bytes), the key derived for it, and the next read of the same fingerprint.
#[derive(Clone, Copy, Debug)]
struct BindingRow {
    module: ItemKey,
    ns: Namespace,
    at: u32,
    len: u32,
    key: CellKey,
    next: Option<u32>,
}

/// The cell keys of a walk's binding reads, each DERIVED ONCE (rule 77): a names lookup logs
/// its read as the module, the namespace and the name itself ([`crate::names_cells::CellSink::put_binding`]),
/// and the BLAKE3 of [`cell_key`] is paid the first time that read is seen, never on a repeat.
/// Before (a stack sample of `engine-check` on a 700-crate workspace, 2026-11-18): every
/// lookup hashed its name to log it, 721 samples in `cell_key` of 4,337 in the path walks.
///
/// A repeat is found by a fast fingerprint (foldhash over the module, the namespace byte and
/// the name) and confirmed by comparing the three, so two reads that share a fingerprint stay
/// two (chained by `next`). The key handed out is [`cell_key`]'s own, so every key, record and
/// read list is byte-equal to what logging per lookup made (rule 25).
pub struct BindingKeys<'a> {
    /// Fingerprint -> the first row of that fingerprint.
    heads: Map<u64, u32, FixedState, Alloc<'a>>,
    rows: AVec<'a, BindingRow>,
    /// Every distinct read's name, one after another.
    names: AVec<'a, u8>,
    /// How many keys were derived ([`cell_key`] calls): one per distinct read.
    derived: u64,
    /// How many reads were asked, repeats too.
    asked: u64,
}

impl<'a> BindingKeys<'a> {
    /// An empty memo in `alloc`; it allocates nothing until its first read.
    pub fn new_in(alloc: Alloc<'a>) -> BindingKeys<'a> {
        BindingKeys {
            heads: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            rows: AVec::new_in(alloc),
            names: AVec::new_in(alloc),
            derived: 0,
            asked: 0,
        }
    }

    /// The cell key of `module`'s binding of `(ns, name)`: derived now if this read was not
    /// seen before, else the key derived then. Past [`MOST_BINDING_KEYS`] distinct reads (or
    /// 4 GiB of names) a new read is keyed and handed out `fresh` but not kept: the answer is
    /// the same key, only a later repeat of it pays the hash again (the task is refused at its
    /// own cap first).
    pub fn key(&mut self, module: ItemKey, ns: Namespace, name: &str) -> BindingKey {
        self.asked += 1;
        let print = fingerprint(module, ns, name);
        let mut at = self.heads.get(&print).copied();
        let mut last: Option<u32> = None;
        while let Some(i) = at {
            let row = self.rows[i as usize];
            let (s, e) = (row.at as usize, row.at as usize + row.len as usize);
            if row.module == module && row.ns == ns && &self.names[s..e] == name.as_bytes() {
                return BindingKey { key: row.key, fresh: false };
            }
            last = Some(i);
            at = row.next;
        }
        let key = cell_key(ns, name);
        self.derived += 1;
        let (Ok(slot), Ok(start), Ok(len)) = (u32::try_from(self.rows.len()), u32::try_from(self.names.len()), u32::try_from(name.len())) else {
            return BindingKey { key, fresh: true };
        };
        if self.rows.len() >= MOST_BINDING_KEYS || start.checked_add(len).is_none() {
            return BindingKey { key, fresh: true };
        }
        self.names.extend_from_slice(name.as_bytes());
        self.rows.push(BindingRow { module, ns, at: start, len, key, next: None });
        match last {
            Some(prev) => self.rows[prev as usize].next = Some(slot),
            None => {
                self.heads.insert(print, slot);
            }
        }
        BindingKey { key, fresh: true }
    }

    /// How many keys were derived: one per distinct read (the floor's counter).
    pub fn derived(&self) -> u64 {
        self.derived
    }

    /// How many reads were asked, repeats too.
    pub fn asked(&self) -> u64 {
        self.asked
    }

    /// How many distinct reads are kept.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// A binding read's fingerprint for [`BindingKeys`]'s index: foldhash over the module's bytes,
/// the namespace byte and the name (the module is fixed-width, so the three cannot run
/// together). A lookup only; never persisted.
fn fingerprint(module: ItemKey, ns: Namespace, name: &str) -> u64 {
    let mut h = FixedHasher::default();
    h.write(&module.0.0);
    h.write_u8(ns as u8);
    h.write(name.as_bytes());
    h.finish()
}

/// The BLAKE3 domain of a `Textual` table's cut cell ([`textual_cut_key`]).
pub const TEXTUAL_CUT_DOMAIN: &str = "blaze v2 names textual cut cell 1";

static TEXTUAL_CUT: Domain = Domain::new(TEXTUAL_CUT_DOMAIN);

/// The cell of a `Textual` table's CUT row for the macro `name` at `child`'s `mod` line: the
/// last definition of `name` before that line, written only where the name's row (its last
/// definition) is after the line and its first before it, so a child's lookup reads one cell
/// more and never leaves the name undecided (links.rs `lookup_textual`). Keyed by the child's
/// whole key and the name, in its own domain: a collision with a name or mark cell is the
/// writer's `Twice` (rule 21).
pub fn textual_cut_key(child: ItemKey, name: &str) -> CellKey {
    let id: ContentId = Hasher::of(&TEXTUAL_CUT).id(&child.0).part(name.as_bytes()).finish();
    let mut out = [0u8; 16];
    out.copy_from_slice(&id.0[..16]);
    CellKey(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected line: `test cell_key::tests::a_cell_key_is_its_namespace_and_name ... ok`.
    /// The same (namespace, name) gives the same key; another namespace or name another key;
    /// the length prefix keeps names apart that one byte would join.
    #[test]
    fn a_cell_key_is_its_namespace_and_name() {
        let k = cell_key(Namespace::Types, "Vec");
        assert_eq!(k, cell_key(Namespace::Types, "Vec"));
        assert_ne!(k, cell_key(Namespace::Values, "Vec"));
        assert_ne!(k, cell_key(Namespace::Macros, "Vec"));
        assert_ne!(k, cell_key(Namespace::Types, "Ve"));
        assert_ne!(cell_key(Namespace::Types, ""), cell_key(Namespace::Values, ""));
    }

    /// Expected line: `test cell_key::tests::the_once_derived_domain_gives_the_same_key ... ok`.
    /// The domain derived once ([`CELL`]) makes byte-equal keys to the domain derived per call,
    /// so records written before the change are read the same (rule 25).
    #[test]
    fn the_once_derived_domain_gives_the_same_key() {
        for (ns, name) in [(Namespace::Types, "Vec"), (Namespace::Values, ""), (Namespace::Macros, "println"), (Namespace::Types, "a\u{e9}b")] {
            let fresh: ContentId = Hasher::new(CELL_DOMAIN).update(&[ns as u8]).part(name.as_bytes()).finish();
            assert_eq!(&cell_key(ns, name).0[..], &fresh.0[..16], "the once-derived domain differs for {name:?}");
        }
    }
}
