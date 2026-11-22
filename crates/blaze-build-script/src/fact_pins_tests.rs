//! Tests of the derived facts' pins' one producer (brief P): the userland's text read as
//! `sw_vers` reads it, and `clang --version` composed from the pin table as clang's driver
//! prints it (clang/lib/Basic/Version.cpp `getClangToolFullVersion`: `clang version <v> (<repo>
//! <rev>)`; clang/lib/Driver/Driver.cpp `Driver::PrintVersion`: `Target:`, `Thread model:`,
//! `InstalledDir:` lines; llvm/lib/TargetParser/Host.cpp `updateTripleOSVersion`: the default
//! triple's darwin version is the running kernel's release). The repository and revision are
//! the pinned macOS archive's own (`strings` of its `bin/clang-23` holds
//! `https://github.com/llvm/llvm-project` and `7a941dfb2877a0b8cf30323fefa85ad019e0b0f5`; its
//! default triple string is `arm64-apple-darwin23.6.0`, the release builder's).
//!
//! Expected from `lanecheck.sh blaze-build-script test fact_pins`:
//! `test blaze-build-script: exit 0; 4 passed, 0 failed`.

use crate::{
    fact_pins::{AppleUserland, CLANG_GUEST_DIR, FactPinsError, UserlandError, apple_userland, fact_pins},
    facts::{answer, question},
};
use blaze_pins::CLANG_JSON as PIN;

/// A macOS 27.0.1 `SystemVersion.plist`, as the machine holds it (trimmed of keys not read).
const PLIST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>BuildID</key>\n\t<string>0B27979E-B7E7-11F1-B3D2-24EC53F49301</string>\n\t<key>ProductBuildVersion</key>\n\t<string>26A434</string>\n\t<key>ProductName</key>\n\t<string>macOS</string>\n\t<key>ProductUserVisibleVersion</key>\n\t<string>27.0.1</string>\n\t<key>ProductVersion</key>\n\t<string>27.0.1</string>\n</dict>\n</plist>\n";

const USERLAND: AppleUserland<'static> = AppleUserland { product_version: "27.0.1", build_version: "26A434", darwin_release: "27.0.0" };

#[test]
fn fact_pins_userland_is_read_from_its_text() {
    // `ProductUserVisibleVersion` comes first in key order but is not `ProductVersion`: the key
    // is matched whole.
    assert_eq!(apple_userland(PLIST, "27.0.0\n"), Ok(USERLAND));
    assert_eq!(apple_userland("<dict></dict>", "27.0.0"), Err(UserlandError::NoKey { key: "ProductVersion" }));
    let bad = PLIST.replace("<string>27.0.1</string>\n</dict>", "<string>27.x</string>\n</dict>");
    assert_eq!(apple_userland(&bad, "27.0.0"), Err(UserlandError::Malformed { what: "ProductVersion", text: "27.x" }));
    assert_eq!(apple_userland(PLIST, "Darwin"), Err(UserlandError::Malformed { what: "kernel release", text: "Darwin" }));
}

#[test]
fn fact_pins_clang_version_text_is_composed_from_the_pin() {
    let alloc = blaze_own::testing::heap().alloc();
    let pins = fact_pins(PIN, "aarch64-apple-darwin", &USERLAND, CLANG_GUEST_DIR, alloc).expect("pins from the checked-in table");
    assert_eq!(
        pins.clang_version_text.as_str(),
        "clang version 23.1.4 (https://github.com/llvm/llvm-project 7a941dfb2877a0b8cf30323fefa85ad019e0b0f5)\nTarget: arm64-apple-darwin27.0.0\nThread model: posix\nInstalledDir: /usr/bin\n"
    );
    assert_eq!((pins.macos_product_version.as_str(), pins.macos_build_version.as_str()), ("27.0.1", "26A434"));
    // Another userland gives another text: the pins carry it (rule 21), the shelf's path never.
    let other = AppleUserland { darwin_release: "25.1.0", ..USERLAND };
    let moved = fact_pins(PIN, "aarch64-apple-darwin", &other, CLANG_GUEST_DIR, alloc).expect("pins");
    assert!(moved.clang_version_text.as_str().contains("Target: arm64-apple-darwin25.1.0\n"));
    assert!(!pins.clang_version_text.as_str().contains(".blaze"), "no host path in the pins");
}

#[test]
fn fact_pins_refuse_by_name() {
    let alloc = blaze_own::testing::heap().alloc();
    assert_eq!(
        fact_pins(PIN, "x86_64-unknown-linux-gnu", &USERLAND, CLANG_GUEST_DIR, alloc).err(),
        Some(FactPinsError::NoTriple { target: "x86_64-unknown-linux-gnu" })
    );
    let no_revision = PIN.replace("\"revision\"", "\"rev\"");
    assert_eq!(fact_pins(&no_revision, "aarch64-apple-darwin", &USERLAND, CLANG_GUEST_DIR, alloc).err(), Some(FactPinsError::Pin { field: "revision" }));
    let other_version = PIN.replacen("\"version\": \"23.1.4\"", "\"version\": \"23.1.5\"", 1);
    assert_eq!(
        fact_pins(&other_version, "aarch64-apple-darwin", &USERLAND, CLANG_GUEST_DIR, alloc).err(),
        Some(FactPinsError::VersionMismatch { pinned: "23.1.5" })
    );
}

/// libusb1-sys 0.7.0's first two spawns (build.rs: `sw_vers -productVersion`, then
/// `pkg-config --libs --cflags libusb-1.0`) answered from the produced pins.
#[test]
fn fact_pins_answer_libusb1_sys() {
    let alloc = blaze_own::testing::heap().alloc();
    let owned = fact_pins(PIN, "aarch64-apple-darwin", &USERLAND, CLANG_GUEST_DIR, alloc).expect("pins");
    let pins = owned.pins();
    let sw_vers = question("sw_vers", &["-productVersion"]).expect("a fact");
    assert_eq!(answer(&sw_vers, &pins, alloc).expect("answered").stdout.as_str(), "27.0.1\n");
    let pkg = question("pkg-config", &["--libs", "--cflags", "libusb-1.0"]).expect("a fact");
    let got = answer(&pkg, &pins, alloc).expect("answered");
    assert_eq!(got.code, 1);
    assert!(got.stdout.as_str().is_empty());
    assert!(got.stderr.as_str().starts_with("Package libusb-1.0 was not found in the pkg-config search path.\n"), "{}", got.stderr.as_str());
}
