//! nm-row: the command's native setting refuses a shelf that lacks a member of
//! `toolchain::SHELF_MEMBERS` (an unpack from before nm-row, without `bin/llvm-nm`) by the
//! member's path, before anything runs; with the member there the shelf passes and the next
//! check (the SDK) is what refuses.

use blaze_build_script::toolchain::{self, SHELF_MEMBERS};
use blaze_pins::CLANG_JSON as PIN;

use crate::native_setting::{NativeSettingError, native_setting};

const HOST: &str = "aarch64-apple-darwin";

/// Prints "shelf without bin/llvm-nm refused: <shelf>/bin/llvm-nm: ...". A wrong fix (no member
/// check, or a list without llvm-nm) gets past the shelf and fails at the first `matches!`.
#[test]
fn a_shelf_without_llvm_nm_is_refused_by_name() {
    let alloc = blaze_own::testing::heap().alloc();
    let pin = toolchain::clang_pin(PIN, HOST).expect("the macOS pin");
    let digest: std::string::String = pin.sha256.iter().map(|b| std::format!("{b:02x}")).collect();
    let root = std::env::temp_dir().join(std::format!("blaze-nm-row-{}", std::process::id()));
    let shelf = root.join(&digest);
    let _ = std::fs::remove_dir_all(&root);
    for member in SHELF_MEMBERS.iter().filter(|m| **m != "bin/llvm-nm") {
        let path = shelf.join(member);
        if member.starts_with("lib/clang") {
            std::fs::create_dir_all(&path).expect("a member folder");
        } else {
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("bin/ and lib/");
            std::fs::write(&path, b"").expect("a member file");
        }
    }
    let shelf_text = shelf.to_str().expect("a UTF-8 temp folder");
    let work = root.join("work");
    let work_text = work.to_str().expect("a UTF-8 temp folder");
    let sdk = root.join("no-sdk");
    let sdk_text = sdk.to_str().expect("a UTF-8 temp folder");

    let refused = native_setting(shelf_text, sdk_text, work_text, HOST, alloc).expect_err("a shelf without llvm-nm");
    let said = refused.to_string();
    assert!(matches!(refused, NativeSettingError::ShelfIncomplete { member: "bin/llvm-nm", .. }), "{said}");
    assert!(said.contains("/bin/llvm-nm"), "{said}");

    // With the member there, the shelf passes and the missing SDK is what is refused.
    std::fs::write(shelf.join("bin/llvm-nm"), b"").expect("the member");
    let next = native_setting(shelf_text, sdk_text, work_text, HOST, alloc).expect_err("no SDK");
    assert!(matches!(next, NativeSettingError::NoSdk { .. }), "{next}");
    let _ = std::fs::remove_dir_all(&root);
    println!("shelf without bin/llvm-nm refused: {said}");
}
