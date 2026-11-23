//! The member owners section of the items record ([`crate::items_record::owners`]) round
//! trips: a crate of one file with a trait of two fns, an impl of it and an inherent impl is
//! solved once and its record written (twice: equal bytes); then every named member of every
//! owner, read from the owner's member list, is asked of the section by its key alone and
//! must answer that owner and that name; the section holds exactly those rows; a key that is
//! no member answers `None`; the reader over a set of crates ([`RecordItems::member_owner`])
//! answers the same.
//!
//! PLANTED BUGS, on copies of the record's bytes: (1) the first row of the section given
//! another owner's row: that member's owner must now differ from the member list's; (2) the
//! first row given another member's row: the reader must refuse it by name (the member row
//! holds another key), never answer that row's name.
//!
//! Expected, from `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh
//! blaze-scope test a_member_finds_its_owner_in_one_search`:
//! `test tests::items_owners::a_member_finds_its_owner_in_one_search ... ok`.

use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};
use blaze_surface::ItemKind;

use super::entry::{Texts, input};
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::items_record::owners::{F_OWNED_AT, OWNED_ROW};
use crate::items_record::{ItemsRecord, ItemsRecordError, LoadItems, RecordItems, encode};
use crate::resolve::aligned;
use crate::task::Serial;

const WAIT: core::time::Duration = core::time::Duration::from_secs(60);
const PATH: &str = "/k0/lib.rs";
const TEXT: &str = "#![no_core]\ntrait T { fn a(); fn b(); }\nstruct S;\nimpl T for S { fn a() {} fn b() {} }\nimpl S { fn own() {} }\n";

static FILES: [(&str, &str); 1] = [(PATH, TEXT)];
static CRATES: [&[(&str, &str)]; 1] = [&FILES];
static TEXTS: Texts = Texts(&CRATES);

/// The crate's record as written, and its one file.
#[derive(Clone, Copy)]
struct Kept {
    record: &'static [u8],
}

impl LoadItems<'static> for Kept {
    fn load(&self, krate: usize) -> Option<&'static [u8]> {
        (krate == 0).then_some(self.record)
    }

    fn file(&self, krate: usize, path: crate::package_path::PackagePath<'_>) -> Option<&'static str> {
        // The crate's package is at `/` (entry.rs `input`): its record holds `k0/lib.rs`.
        (krate == 0 && path.in_package("/", Alloc::global()).as_deref() == Some(PATH)).then_some(TEXT)
    }

    fn expansion(&self, _krate: usize, _call: ItemKey) -> Option<&'static str> {
        None
    }
}

/// Every named member of every trait and impl of `record`, from the owners' member lists:
/// (member key, owner key, name), in owner then declaration order.
fn listed(record: &ItemsRecord<'static>) -> AVec<'static, (ItemKey, ItemKey, &'static str)> {
    let mut out = AVec::new_in(Alloc::global());
    for i in 0..record.items() {
        let row = record.item_at(i).unwrap_or_else(|e| panic!("item row {i}: {e:?}")).unwrap_or_else(|| panic!("no item row {i}"));
        if !matches!(row.kind, ItemKind::Trait | ItemKind::Impl) {
            continue;
        }
        let members = record.members(row.key).unwrap_or_else(|e| panic!("the members of {:?}: {e:?}", row.key)).unwrap_or_else(|| panic!("owner {:?} has no member row", row.key));
        for m in members.iter() {
            let m = m.unwrap_or_else(|e| panic!("a member row of {:?}: {e:?}", row.key));
            if let Some(name) = m.name {
                out.push((m.key, row.key, name));
            }
        }
    }
    out
}

/// `bytes` with the u32 at `at` set to `value`, aligned as a record is read.
fn with_u32(bytes: &[u8], at: usize, value: u32) -> &'static [u8] {
    let mut copy = AVec::from_slice_in(bytes, Alloc::global());
    copy[at..at + 4].copy_from_slice(&value.to_le_bytes());
    aligned(&copy, super::SyncFrame::leaked())
}

#[test]
fn a_member_finds_its_owner_in_one_search() {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let inputs = [input("k0", PATH, &[])];
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("solve: {e:?}"));
    let names = solved[0].names;
    let a = encode(names, g).unwrap_or_else(|e| panic!("encode: {e:?}"));
    let b = encode(names, g).unwrap_or_else(|e| panic!("encode again: {e:?}"));
    assert!(a == b, "the record encodes to different bytes twice");
    let bytes: &'static [u8] = aligned(&a, bump);
    let record = super::items_record::one_file(bytes, names.files[0].text, bump);

    // Every named member, by its key alone, answers the owner and the name its list holds.
    let members = listed(&record);
    assert!(members.len() == 5, "the trait's `a` and `b`, the impl's `a` and `b`, the inherent `own`: {} named members listed", members.len());
    for (member, owner, name) in members.iter() {
        let got = record.member_owner(*member).unwrap_or_else(|e| panic!("the owner of `{name}` ({member:?}): {e:?}"));
        assert!(got == Some((*owner, *name)), "the owner of `{name}` ({member:?}): the section says {got:?}, its list {owner:?}");
    }
    assert!(record.owned_members() == members.len(), "the section has {} rows for {} named members", record.owned_members(), members.len());

    // A key that is no member: the struct's.
    let s = (0..record.items())
        .filter_map(|i| record.item_at(i).ok().flatten())
        .find(|row| row.kind == ItemKind::Struct)
        .unwrap_or_else(|| panic!("no item row for the struct"))
        .key;
    assert!(matches!(record.member_owner(s), Ok(None)), "a struct's key is no member's: {:?}", record.member_owner(s));

    // The reader over a set of crates answers the same.
    let read = RecordItems::new(&[names.key], Kept { record: bytes }, bump);
    for (member, owner, name) in members.iter() {
        assert!(matches!(read.member_owner(*member), Ok(Some(got)) if got == (*owner, *name)), "`{name}` through the crates' reader: {:?}", read.member_owner(*member));
    }

    // PLANTED BUG 1: the section's first row names another owner's row.
    let at = u32::from_le_bytes(bytes[8 + 4 * F_OWNED_AT..][..4].try_into().expect("four bytes")) as usize;
    assert!(at % 16 == 0 && at + members.len() * OWNED_ROW <= bytes.len(), "the section at {at} lies in the record's {} bytes", bytes.len());
    let first: ItemKey = {
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes[at..at + 32]);
        ItemKey(blaze_id::ContentId(key))
    };
    let (_, true_owner, first_name) = *members.iter().find(|m| m.0 == first).unwrap_or_else(|| panic!("the section's first key {first:?} is no listed member"));
    let owner_row = u32::from_le_bytes(bytes[at + 32..][..4].try_into().expect("four bytes"));
    let other_owner = if owner_row == 0 { 1 } else { 0 };
    let planted = ItemsRecord::open(with_u32(bytes, at + 32, other_owner), g).unwrap_or_else(|e| panic!("open the planted record: {e:?}"));
    let got = planted.member_owner(first).unwrap_or_else(|e| panic!("the planted owner of `{first_name}`: {e:?}"));
    assert!(got.is_some_and(|(owner, name)| owner != true_owner && name == first_name), "planted bug 1 (another owner's row) was not caught: `{first_name}` answers {got:?}, its owner is {true_owner:?}");

    // PLANTED BUG 2: the section's first row names another member's row.
    let member_row = u32::from_le_bytes(bytes[at + 36..][..4].try_into().expect("four bytes"));
    let other_member = if member_row == 0 { 1 } else { 0 };
    let planted = ItemsRecord::open(with_u32(bytes, at + 36, other_member), g).unwrap_or_else(|e| panic!("open the second planted record: {e:?}"));
    let got = planted.member_owner(first);
    assert!(
        matches!(got, Err(ItemsRecordError::Short { what: "a member owner's member row (it holds another key)", .. })),
        "planted bug 2 (another member's row) was not refused by name: {got:?}"
    );
}
