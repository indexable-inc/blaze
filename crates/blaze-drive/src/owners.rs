//! A file's owners (drive.md, "RUNNER 18", item 1; the coordinator's ruling (A), 2026-11-16
//! 20:30, and its answer on roots and demand): the crate roots whose module tree reaches the
//! file. A kept row the request machine computes itself, never a task: owners(F) is the union
//! of owners(P) over F's parents P, a parent that names crates (a `Units` or `UnitGraph` record,
//! [`names_crates`]) adding F itself as a root, ASKED when that parent is a `Units` record (a
//! workspace member the command is about) and not when it is the `UnitGraph`'s (a dependency).
//! Its read list is each parent's name and each `FileItems` parent's `Owners` name, with their
//! outs; its out is [`out_of`] the sorted roots; its payload is [`write_roots`] of them.
//!
//! A body reads its file's `Owners` (`Want { name: key::owners_of(file), home: file }`) and
//! needs only its owners' scopes: one failed scope blocks only the bodies its crates own. The
//! machine makes a body's job from its file's part list only when an owner is asked; any other
//! body runs only when a task Needs it.
//!
//! The payload's form: `BDO1`, a `u32` count, then per root its 32-byte `FileItems` subject and
//! one byte, 1 when asked, else 0; strictly increasing by subject (sorted, no repeat), so it is
//! deterministic by its writer (rule 25).

use blaze_id::ContentId;
use blaze_ids::Hasher;
use blaze_own::{AVec, Alloc, Vec};

use crate::key::{Kind, Name};
use crate::wire::{WireError, WireWhy};

/// The payload's magic: a bump is a new tool key (lib.rs `FORMATS`).
pub(crate) const MAGIC: &[u8; 4] = b"BDO1";

const WHAT: &str = "owners payload";
const OUT_DOMAIN: &str = "blaze-drive owners out 1";
/// A root's bytes in the payload: its subject and its asked byte.
const ENTRY: usize = 33;

/// One owner of a file: a crate root's `FileItems` name, and whether the command asked about its
/// crate (a workspace member's root) rather than reaching it as a dependency.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Root {
    pub file: Name,
    pub asked: bool,
}

/// The `Owners` name of the file `file` (a `FileItems` name).
pub fn of(file: Name) -> Name {
    Name::new(Kind::Owners, file.subject)
}

/// The `FileItems` name of the file an `Owners` name is about.
pub fn file_of(owners: Name) -> Name {
    Name::new(Kind::FileItems, owners.subject)
}

/// Whether a record of `kind` names crates, so a file part of it is a crate root.
pub fn names_crates(kind: Kind) -> bool {
    matches!(kind, Kind::Units | Kind::UnitGraph)
}

/// Sorts `roots` by file and merges repeats, a root asked by any parent being asked.
pub fn merge(roots: &mut AVec<'_, Root>) {
    roots.sort_unstable();
    let mut at = 0;
    for i in 0..roots.len() {
        if at > 0 && roots[at - 1].file == roots[i].file {
            roots[at - 1].asked |= roots[i].asked;
        } else {
            roots[at] = roots[i];
            at += 1;
        }
    }
    roots.truncate(at);
}

/// The out digest of an `Owners` record: its sorted roots, so a reader is dirtied only when
/// the set (or an asked flag) moves.
pub fn out_of(roots: &[Root]) -> ContentId {
    let mut h = Hasher::new(OUT_DOMAIN);
    for root in roots {
        h.id(&root.file.subject).update(&[u8::from(root.asked)]);
    }
    h.finish()
}

/// The payload of an `Owners` record: `roots`, which the caller [`merge`]d.
pub fn write_roots<'a>(roots: &[Root], alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut bytes = Vec::with_capacity_in(8 + ENTRY * roots.len(), alloc);
    bytes.extend_from_slice(MAGIC);
    let count = u32::try_from(roots.len()).expect("a file's owners fit u32");
    bytes.extend_from_slice(&count.to_le_bytes());
    for root in roots {
        bytes.extend_from_slice(&root.file.subject.0);
        bytes.push(u8::from(root.asked));
    }
    bytes
}

/// The roots an `Owners` payload holds, in order.
pub fn roots(payload: &[u8]) -> Result<Roots<'_>, WireError> {
    let error = |at: usize, why| WireError { what: WHAT, at: u32::try_from(at).unwrap_or(u32::MAX), why };
    let Some((head, rest)) = payload.split_first_chunk::<8>() else {
        return Err(error(payload.len(), WireWhy::Short));
    };
    if &head[..4] != MAGIC {
        return Err(error(0, WireWhy::Magic));
    }
    let count = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    let Some(want) = (count as usize).checked_mul(ENTRY).filter(|want| *want <= rest.len()) else {
        return Err(error(4, WireWhy::Count(count)));
    };
    if rest.len() != want {
        return Err(error(8 + want, WireWhy::Trailing));
    }
    let mut at = 0;
    while at < want {
        if rest[at + 32] > 1 {
            return Err(error(8 + at + 32, WireWhy::Tag(rest[at + 32])));
        }
        if at > 0 && rest[at - ENTRY..at - 1] >= rest[at..at + 32] {
            return Err(error(8 + at, WireWhy::Unsorted));
        }
        at += ENTRY;
    }
    Ok(Roots { rest })
}

/// The roots of a payload ([`roots`]), in order.
#[derive(Clone, Debug)]
pub struct Roots<'b> {
    rest: &'b [u8],
}

impl Iterator for Roots<'_> {
    type Item = Root;

    fn next(&mut self) -> Option<Root> {
        let (head, rest) = self.rest.split_first_chunk::<ENTRY>()?;
        self.rest = rest;
        let mut subject = [0u8; 32];
        subject.copy_from_slice(&head[..32]);
        Some(Root { file: Name::new(Kind::FileItems, ContentId(subject)), asked: head[32] == 1 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload round-trips; a repeat merges with its asked flag kept; a payload whose roots
    /// are out of order is refused by name.
    /// Expected: "test owners::tests::roots_round_trip_merge_and_refuse_unsorted ... ok".
    #[test]
    fn roots_round_trip_merge_and_refuse_unsorted() {
        let heap = blaze_own::testing::heap();
        let (a, b) = (Name::file(b"a.rs"), Name::file(b"b.rs"));
        let mut roots: AVec<'_, Root> = Vec::new_in(heap.alloc());
        roots.extend([Root { file: b, asked: false }, Root { file: a, asked: false }, Root { file: b, asked: true }]);
        merge(&mut roots);
        let mut want = [Root { file: a, asked: false }, Root { file: b, asked: true }];
        want.sort_unstable();
        assert_eq!(roots.as_slice(), want.as_slice());
        let bytes = write_roots(&roots, heap.alloc());
        let back: std::vec::Vec<Root> = roots_of(&bytes);
        assert_eq!(back.as_slice(), want.as_slice());
        let swapped = write_roots(&[want[1], want[0]], heap.alloc());
        assert_eq!(super::roots(&swapped).err().map(|e| e.why), Some(WireWhy::Unsorted));
    }

    fn roots_of(bytes: &[u8]) -> std::vec::Vec<Root> {
        roots(bytes).expect("a written payload reads").collect()
    }
}
