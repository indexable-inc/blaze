//! The `Fetch` task (docs/v2/plan/design/fetch.md; drive.md, "Tasks and Need"): one lock entry's
//! sources in the store. One task a unit of blaze-fetch's plan: a crates.io crate, or a git
//! commit once however many of the lock's packages it holds.
//!
//! - **Name and source.** Both are the unit's record key, from the entry's text alone (a
//!   crate's name, version and SHA-256; a commit's URL and id) and the record's form. They are
//!   known before any work, so a repeat stands on its record and asks nothing (rules 65, 77). A
//!   lock edit that adds one dependency adds one name.
//! - **Run.** The task wants its unit of the command's one fetch machine
//!   ([`blaze_fetch::Machine::wanting`], which the caller makes once over the lock's plan, so
//!   every `Fetch` task of the command shares one byte budget and one in-flight limit). It then
//!   joins on the unit's outcome. The caller drives that machine with
//!   [`blaze_fetch::drive::step`] over its [`blaze_fetch::FetchIo`], and the checks go on beside
//!   it. Nothing here does IO.
//! - **What a reader sees.** [`Made::Held::out`] is the unit's key itself. A record that was
//!   present and one fetched now read the same, so `UnitGraph`'s read of it changes only when
//!   the entry does.
//! - **A failure** is one finding naming the unit and why (rule 19), and the build goes on.
//!   [`Made::Failed::lasting`] says whether the fault is in the bytes (a checksum that is not the
//!   lock's, an archive that does not unpack, an advertisement that is not git's), which the
//!   same key meets again. Otherwise the fault is the network's or the store's, which a later
//!   command may not meet: the caller keeps no record of it, so the next command asks again.
//! - **Without the network** (blaze-cli built without `fetch`), a unit whose record the store
//!   lacks is answered by [`cannot_fetch`]'s finding.

use blaze_fetch::fetch::{Outcome, Why};
use blaze_fetch::plan::Unit;
use blaze_fetch::Failed;
use blaze_id::ContentId;
use blaze_own::{Alloc, String, Vec};
use blaze_text::Writes as _;

use crate::key::{Kind, Name};

/// The `Fetch` name of `unit`: its subject is the unit's record key.
pub fn name(unit: &Unit<'_, '_>) -> Name {
    Name::new(Kind::Fetch, source(unit))
}

/// The task's source, known before the work: the unit's record key (rule 65). A record valid by
/// it is the unit's sources as the entry pins them.
pub fn source(unit: &Unit<'_, '_>) -> ContentId {
    unit.key()
}

/// The plan's units by key, made once a command ([`index`]), so each `Fetch` task finds its
/// unit in `log n`.
pub type Index<'a> = Vec<(ContentId, u32), Alloc<'a>>;

/// `units`' keys, sorted, each with its unit.
pub fn index<'a>(units: &[Unit<'_, '_>], alloc: Alloc<'a>) -> Index<'a> {
    let mut index = Vec::with_capacity_in(units.len(), alloc);
    for (at, unit) in units.iter().enumerate() {
        index.push((unit.key(), at as u32));
    }
    index.sort_unstable();
    index
}

/// The unit a `Fetch` name is of: the one whose key is the name's subject. `None` for another
/// kind, or a name no unit of this lock has.
pub fn unit_of(index: &[(ContentId, u32)], name: Name) -> Option<u32> {
    if name.kind != Kind::Fetch {
        return None;
    }
    let at = index.binary_search_by(|(key, _)| key.cmp(&name.subject)).ok()?;
    Some(index[at].1)
}

/// What a `Fetch` task made of its unit's outcome.
#[derive(Debug)]
pub enum Made<'a> {
    /// The store holds the unit's record. `out` is the digest a reader records: the key.
    /// `down` is the bytes downloaded for it now (0 when it was present).
    Held { out: ContentId, down: u64 },
    /// No record: one finding, `message`, naming the unit and why.
    Failed { message: String<Alloc<'a>>, lasting: bool },
}

/// The task's answer for `unit` once the machine gave it `outcome`; `None` while it is
/// [`Outcome::Pending`].
pub fn made<'a>(unit: &Unit<'_, '_>, outcome: &Outcome, alloc: Alloc<'a>) -> Option<Made<'a>> {
    match outcome {
        Outcome::Pending => None,
        Outcome::Present => Some(Made::Held { out: source(unit), down: 0 }),
        Outcome::Fetched { down, .. } => Some(Made::Held { out: source(unit), down: *down }),
        Outcome::Failed(why) => {
            let mut message = String::new_in(alloc);
            message.str("its sources were not fetched: ").show(&Failed { unit, why });
            Some(Made::Failed { message, lasting: lasting(why) })
        }
    }
}

/// Whether the same key fails the same way again: the bytes' fault, not the network's or the
/// store's.
fn lasting(why: &Why) -> bool {
    match why {
        Why::Mismatch { .. } | Why::Unpack(_) | Why::Git(_) | Why::TooBig { .. } => true,
        Why::Status { .. } | Why::Net { .. } | Why::Store(_) => false,
    }
}

/// The finding of a build that cannot fetch (blaze-cli without its `fetch` feature) for `unit`,
/// whose record the store lacks. Not lasting: a build with the feature fetches it.
pub fn cannot_fetch<'a>(unit: &Unit<'_, '_>, alloc: Alloc<'a>) -> Made<'a> {
    let mut message = String::new_in(alloc);
    message.show(unit).str(": its sources are not in the store, and this build cannot fetch (built without `fetch`)");
    Made::Failed { message, lasting: false }
}

#[cfg(test)]
mod tests {
    use blaze_fetch::fetch::{NetFailure, Outcome, Why};
    use blaze_fetch::plan::Unit;
    use blaze_git_fetch::Oid;
    use blaze_manifest::lock::LockId;
    use blaze_own::String;
    use blaze_text::Writes as _;

    use super::{Made, index, made, name, unit_of};
    use crate::key::{Kind, Name};

    /// Each unit's `Fetch` name finds that unit and no other; a name of another kind or of no
    /// unit finds none. A unit present and the same unit fetched read the same; a failure is a
    /// finding naming the unit, lasting only when the bytes are at fault.
    #[test]
    fn a_fetch_name_is_its_units_and_its_reader_sees_the_key_however_it_came() {
        let alloc = blaze_own::testing::heap().alloc();
        let version = |text: &str| {
            let mut out = String::new_in(alloc);
            out.str(text);
            out
        };
        let units = [
            Unit::Crate { package: LockId(0), name: "serde", version: version("1.0.0"), sha256: [1; 32] },
            Unit::Crate { package: LockId(1), name: "serde", version: version("1.0.1"), sha256: [1; 32] },
            Unit::Git { repo: "https://example.test/a.git", commit: Oid([2; 20]), packages: 3 },
        ];
        let index = index(&units, alloc);
        for (at, unit) in units.iter().enumerate() {
            assert_eq!(unit_of(&index, name(unit)), Some(at as u32), "unit {at}");
        }
        assert_eq!(unit_of(&index, Name::new(Kind::Units, units[0].key())), None);
        assert_eq!(unit_of(&index, Name::new(Kind::Fetch, Name::units().subject)), None);

        let out = |made: Option<Made<'_>>| match made {
            Some(Made::Held { out, .. }) => out,
            other => panic!("held, not {other:?}"),
        };
        let fetched = Outcome::Fetched { down: 10, files: 2, bytes: 30, cache: None };
        assert_eq!(out(made(&units[0], &Outcome::Present, alloc)), out(made(&units[0], &fetched, alloc)));
        assert_ne!(out(made(&units[0], &Outcome::Present, alloc)), out(made(&units[1], &Outcome::Present, alloc)));
        assert!(made(&units[0], &Outcome::Pending, alloc).is_none());

        let failed = |why: Why| match made(&units[1], &Outcome::Failed(why), alloc) {
            Some(Made::Failed { message, lasting }) => {
                assert!(message.as_str().contains("crate serde 1.0.1"), "{}", message.as_str());
                lasting
            }
            other => panic!("failed, not {other:?}"),
        };
        assert!(failed(Why::Mismatch { want: [1; 32], got: [3; 32] }));
        assert!(!failed(Why::Net { failure: NetFailure::Reset }));
        assert!(!failed(Why::Status { status: 503 }));
    }
}
