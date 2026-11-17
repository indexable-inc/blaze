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

use blaze_id::ItemKey;
use blaze_ids::{ContentId, Domain, Hasher};
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
