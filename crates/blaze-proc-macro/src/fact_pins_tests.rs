//! The derived facts' pins reaching a script's spawns (brief P), driven through
//! `ScriptSandbox::spawn` with the shim's encoding (spawn_route_tests.rs's `shim`); nothing runs
//! pkg-config, sw_vers or the machine's reader (the userland's text is given).
//!
//! What is right: libusb1-sys 0.7.0's build.rs asks `pkg-config --libs --cflags libusb-1.0`;
//! the sandbox has no `.pc` file, so pkg-config 0.29.2 exits 1 and prints its not-found message
//! (pkg.c `internal_get_package`, quoted in blaze-build-script's facts_tests.rs). With no pins
//! the spawn is refused by name. A fact read is reused only under the same pins (rule 21).
//!
//! Expected from `lanecheck.sh blaze-proc-macro test fact_pins`:
//! `test blaze-proc-macro: exit 0; 3 passed, 0 failed`.

use blaze_build_script::key::CMode;
use blaze_own::AString;
use blaze_record::Digest;

use crate::{
    fact_pins::{FactNow, UserlandText, fact_now, pinned_setting, read_now},
    script_sandbox::{Refusal, ScriptSandbox, SpawnRefusal, Tree},
    spawn_route::SpawnSetting,
    spawn_route_tests::{script, shim},
};

fn plist(product_version: &str, build_version: &str) -> String {
    format!(
        "<plist version=\"1.0\">\n<dict>\n\t<key>ProductBuildVersion</key>\n\t<string>{build_version}</string>\n\t<key>ProductVersion</key>\n\t<string>{product_version}</string>\n</dict>\n</plist>\n"
    )
}

fn setting(product_version: &str, build_version: &str, darwin_release: &str) -> SpawnSetting {
    let alloc = blaze_own::testing::heap().alloc();
    let text = UserlandText {
        plist: AString::from_str_in(&plist(product_version, build_version), alloc),
        darwin_release: AString::from_str_in(darwin_release, alloc),
    };
    pinned_setting("aarch64-apple-darwin", &text, alloc).unwrap_or_else(|e| panic!("{e}"))
}

/// The reply the shim reads back: code, stdout, stderr.
fn reply(bytes: &[u8]) -> (i32, Vec<u8>, Vec<u8>) {
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes")) as usize;
    let code = i32::from_le_bytes(bytes[0..4].try_into().expect("four bytes"));
    let out_len = word(4);
    let stdout = bytes[8..8 + out_len].to_vec();
    let err_len = word(8 + out_len);
    let stderr = bytes[12 + out_len..12 + out_len + err_len].to_vec();
    (code, stdout, stderr)
}

const LIBUSB: [&str; 4] = ["pkg-config", "--libs", "--cflags", "libusb-1.0"];
const LIBUSB_SUBJECT: &str = "pkg-config\n--libs\n--cflags\nlibusb-1.0";

#[test]
fn fact_pins_libusb1_sys_pkg_config_is_answered_with_pins() {
    let script = script(CMode::Deferred, None, setting("27.0.1", "26A434", "27.0.0"));
    let mut sandbox = ScriptSandbox::new(std::sync::Arc::clone(&script));
    sandbox.spawn(&shim(&LIBUSB, false, &[], None, b"", [2, 2])).unwrap_or_else(|stop| panic!("refused: {}", stop.0));
    let (code, stdout, stderr) = reply(&sandbox.reply);
    let stderr = String::from_utf8(stderr).expect("utf-8");
    assert_eq!((code, stdout.as_slice()), (1, &b""[..]));
    assert!(stderr.starts_with("Package libusb-1.0 was not found in the pkg-config search path.\n"), "{stderr}");
    assert!(stderr.ends_with("Package 'libusb-1.0', required by 'virtual:world', not found\n"), "{stderr}");
    let read = sandbox.reads.iter().find(|read| read.subject == LIBUSB_SUBJECT).expect("the fact is in the read list");
    assert_eq!(FactNow::Answer(read.digest), fact_now(LIBUSB_SUBJECT, script.spawn_setting.pins.as_ref(), blaze_own::testing::heap().alloc()));
}

#[test]
fn fact_pins_none_is_refused_by_name() {
    let script = script(CMode::Deferred, None, SpawnSetting::off());
    let mut sandbox = ScriptSandbox::new(std::sync::Arc::clone(&script));
    let Err(stop) = sandbox.spawn(&shim(&LIBUSB, false, &[], None, b"", [2, 2])) else { panic!("answered with no pins") };
    assert!(stop.0.contains("the command gave no pins for derived facts"), "{}", stop.0);
    assert!(
        matches!(&sandbox.refused, Some(Refusal::Spawn(SpawnRefusal::Fact { program, .. })) if program == "pkg-config"),
        "{:?}",
        sandbox.refused
    );
}

/// The pins are part of what a fact read says: the same pins give the recorded digest, another
/// userland's give another (a `fact_now` that ignored its pins fails here), no pins cannot be
/// worked out (the list is passed over), and a file read is not a fact's.
#[test]
fn fact_pins_reads_are_answered_again_under_todays_pins() {
    let alloc = blaze_own::testing::heap().alloc();
    let today = setting("27.0.1", "26A434", "27.0.0");
    let script = script(CMode::Deferred, None, today.clone());
    let mut sandbox = ScriptSandbox::new(std::sync::Arc::clone(&script));
    sandbox.spawn(&shim(&["sw_vers", "-productVersion"], false, &[], None, b"", [2, 2])).unwrap_or_else(|stop| panic!("refused: {}", stop.0));
    assert_eq!(reply(&sandbox.reply).1, b"27.0.1\n");
    let subject = "sw_vers\n-productVersion";
    let read = sandbox.reads.iter().find(|read| read.subject == subject).expect("the fact is in the read list");
    let recorded = read.digest.expect("an answered fact has a digest");

    let tree = Tree::Mem { files: std::sync::Arc::new(Vec::new()), git: None };
    assert_eq!(read_now(&tree, subject, today.pins.as_ref(), alloc), Some(Digest::Is(recorded)));
    let upgraded = setting("27.1", "26B12", "27.1.0");
    assert_ne!(read_now(&tree, subject, upgraded.pins.as_ref(), alloc), Some(Digest::Is(recorded)), "another userland misses");
    assert_eq!(fact_now(subject, None, alloc), FactNow::NoPins);
    assert_eq!(read_now(&tree, subject, None, alloc), None);
    assert_eq!(fact_now("file:build.rs", today.pins.as_ref(), alloc), FactNow::NotAFact);
}
