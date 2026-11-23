//! Where a build script's record goes (main's ruling on D115 sharing, 2026-11-16 21:3x): sharing
//! is a positive list by subject; the cli adds a script's subject only when its crate is public
//! AND the run is placed on the wasm path (`scripts::run_is_public`), and the placement is in the
//! run's key (`scripts::parts`). Included by store.rs as `#[cfg(test)] #[path =
//! "store/sharing_tests.rs"] mod sharing_tests;`. Needs blaze-drive's `record::write_plain`
//! (wiring d115-7) and blaze-cli's d115-7-placement.
//!
//! Controls: `run_is_public` returning `crate_public` fails the first test with "a natively
//! placed `Script` record was public"; `parts` without the placement byte fails the second with
//! "a native and a wasm run of one subject had one key".

use super::{Sharing, ToolchainCrates};
use crate::scripts::{self, RanAs};
use blaze_drive::record::write_plain;
use blaze_drive::{Kind, Name};
use blaze_ids::ContentId;

/// The seam as the cli fills it for one crate (`names::world`): the crate's `CrateScope` subject
/// when it is public, and its script's subject when `run_is_public` says so.
fn sharing_for(crate_public: bool, ran_as: RanAs, crate_subject: ContentId, script_subject: ContentId, dep_subject: ContentId) -> ToolchainCrates {
    let mut sharing = ToolchainCrates::new();
    sharing.add_public(&[dep_subject]);
    if crate_public {
        sharing.add_public(&[crate_subject]);
    }
    if scripts::run_is_public(crate_public, ran_as) {
        sharing.add_public(&[script_subject]);
    }
    sharing
}

#[test]
fn a_native_script_record_and_its_reader_stay_local() {
    // The test's one frame, made at its top (its memory the OS's), handed down as a handle.
    let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let script_subject = ContentId([1; 32]);
    let crate_subject = ContentId([2; 32]);
    let dep_subject = ContentId([3; 32]);
    let script = Name::new(Kind::Script, script_subject);
    let reader = Name::new(Kind::CrateScope, crate_subject);
    let dep = Name::new(Kind::CrateScope, dep_subject);
    let script_out = ContentId([4; 32]);
    // The `Script` record (its payload's first byte says how it ran: a fact for `--stats`, which
    // sharing does not read) and the crate's `CrateScope` reading it and a public dependency.
    let record_of = |ran_as: RanAs| write_plain(script, ContentId([6; 32]), script_out, &[], &[ran_as as u8], alloc);
    let reader_record = write_plain(reader, ContentId([7; 32]), ContentId([8; 32]), &[(dep, ContentId([5; 32])), (script, script_out)], &[], alloc);

    // A public crate's wasm-placed run, on a trusted builder: both shared.
    let wasm = sharing_for(true, RanAs::Wasm, crate_subject, script_subject, dep_subject);
    assert!(wasm.all_names_public(&[&record_of(RanAs::Wasm)]), "a public crate's wasm `Script` record was local");
    assert!(wasm.all_names_public(&[&reader_record]), "the reader of a public crate's wasm `Script` was local");

    // The same crate placed native: both this machine's.
    let native = sharing_for(true, RanAs::Native, crate_subject, script_subject, dep_subject);
    assert!(!native.all_names_public(&[&record_of(RanAs::Native)]), "a natively placed `Script` record was public");
    assert!(!native.all_names_public(&[&reader_record]), "the reader of a natively placed `Script` was public");

    // A private (workspace) crate: local either way.
    for ran_as in [RanAs::Wasm, RanAs::Native] {
        let private = sharing_for(false, ran_as, crate_subject, script_subject, dep_subject);
        assert!(!private.all_names_public(&[&record_of(ran_as)]), "a private crate's `Script` record ({ran_as:?}) was public");
        assert!(!private.all_names_public(&[&reader_record]), "a private crate's `CrateScope` ({ran_as:?}) was public");
    }
    std::println!("wasm public, native local, private local");
}

#[test]
fn two_placements_of_one_subject_are_two_keys() {
    let subject = ContentId([1; 32]);
    let crate_key = ContentId([2; 32]);
    let wasm = scripts::parts([(subject, crate_key, RanAs::Wasm)].into_iter(), None);
    let native = scripts::parts([(subject, crate_key, RanAs::Native)].into_iter(), None);
    assert_eq!(wasm[0].0, native[0].0, "the name is the subject's alone (one subject function)");
    assert_ne!(wasm[0].1, native[0].1, "a native and a wasm run of one subject had one key");
    std::println!("two placements, one name, two keys");
}

#[test]
fn public_impl_bucket_record_is_publishable_and_private_stays_local() {
    let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let scope_subject = ContentId([10; 32]);
    let tables_subject = ContentId([11; 32]);
    let private_subject = ContentId([12; 32]);
    let scope_name = Name::new(Kind::CrateScope, scope_subject);
    let tables_name = Name::new(Kind::ImplBucket, tables_subject);
    let private_name = Name::new(Kind::CrateScope, private_subject);

    let public_tables = write_plain(tables_name, ContentId([1; 32]), ContentId([2; 32]), &[(scope_name, ContentId([3; 32]))], &[], alloc);
    let tainted_tables = write_plain(tables_name, ContentId([1; 32]), ContentId([2; 32]), &[(private_name, ContentId([4; 32]))], &[], alloc);

    let mut sharing = ToolchainCrates::new();
    sharing.add_public(&[scope_subject, tables_subject]);
    assert!(sharing.all_names_public(&[&public_tables]), "closed public `ImplBucket` record must be publishable");
    assert!(!sharing.all_names_public(&[&tainted_tables]), "`ImplBucket` reading a private crate must stay local");
}
