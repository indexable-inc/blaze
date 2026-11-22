//! The profile's `panic` key (panic.rs). Rust's rule and Cargo's profiles reference ("Profiles",
//! `panic`): "The default when not specified is `"unwind"`"; the stable values are `"unwind"`
//! and `"abort"`; `dev` and `release` are root profiles and inherit from none.
//!
//! Expected: `test layout::panic::panic_tests::release_aborts_dev_unwinds ... ok` and the other
//! three; the control (hard-wire `Ok(ProfilePanic::Unwind)` at the top of `panic_strategy`)
//! fails `release_aborts_dev_unwinds` with "release writes `panic = \"abort\"`".

use super::{PanicError, ProfilePanic, panic_strategy};
use crate::layout::ManifestId;
use crate::layout::profile::BuiltinProfile;

const PATH: &str = "Cargo.toml";

fn read(text: &str, profile: BuiltinProfile) -> Result<ProfilePanic, PanicError<'static>> {
    let alloc = blaze_own::testing::heap().alloc();
    panic_strategy(text, ManifestId(0), PATH, profile, alloc)
}

#[test]
fn release_aborts_dev_unwinds() {
    let text = "[package]\nname = \"p\"\n\n[profile.release]\npanic = \"abort\"\n";
    assert_eq!(read(text, BuiltinProfile::Release).ok(), Some(ProfilePanic::Abort), "release writes `panic = \"abort\"`");
    assert_eq!(read(text, BuiltinProfile::Dev).ok(), Some(ProfilePanic::Unwind), "dev writes no `panic`: Rust's rule, unwind");
}

#[test]
fn unwritten_is_unwind() {
    assert_eq!(read("[package]\nname = \"p\"\n", BuiltinProfile::Dev).ok(), Some(ProfilePanic::Unwind));
    assert_eq!(read("[profile.dev]\nopt-level = 1\n", BuiltinProfile::Dev).ok(), Some(ProfilePanic::Unwind));
    assert_eq!(read("[profile.dev]\npanic = \"unwind\"\n", BuiltinProfile::Dev).ok(), Some(ProfilePanic::Unwind));
}

#[test]
fn bad_values_are_refused_by_name() {
    match read("[profile.release]\npanic = \"Abort\"\n", BuiltinProfile::Release) {
        Err(error @ PanicError::Unknown { .. }) => {
            let said = std::format!("{error}");
            assert!(said.contains("Cargo.toml") && said.contains("profile.release.panic") && said.contains("\"Abort\""), "names file, profile, value: {said}");
        }
        other => panic!("`panic = \"Abort\"` (the value is case-sensitive) must be refused as Unknown: {other:?}"),
    }
    assert!(matches!(read("[profile.dev]\npanic = \"immediate-abort\"\n", BuiltinProfile::Dev), Err(PanicError::ImmediateAbort { profile: "dev", .. })));
    assert!(matches!(read("[profile.dev]\npanic = true\n", BuiltinProfile::Dev), Err(PanicError::Layout(_))));
}

#[test]
fn root_inherits_and_package_panic_are_refused() {
    let inherits = "[profile.dev]\npanic = \"abort\"\n\n[profile.release]\ninherits = \"dev\"\n";
    assert!(matches!(read(inherits, BuiltinProfile::Release), Err(PanicError::InheritsInRoot { profile: "release", .. })));
    let package = "[profile.dev.package.serde]\npanic = \"abort\"\n";
    match read(package, BuiltinProfile::Dev) {
        Err(PanicError::InPackageOverride { package, .. }) => assert_eq!(package.as_str(), "serde"),
        other => panic!("`panic` in a package override must be refused: {other:?}"),
    }
}
