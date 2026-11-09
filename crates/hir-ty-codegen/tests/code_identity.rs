//! The code identity (`blaze_codegen_code`) held to what building the code generator read. An
//! integration test, for `CARGO_TARGET_TMPDIR`, which names the target directory whose dep-info
//! and build-script output it reads wherever the test executable runs from.

use std::path::Path;

/// What the code identity (`blaze_codegen_code::CODEGEN`) is of covers every file and variable
/// rustc read building the code generator's crates, as their dep-info (`<crate>-<hash>.d` beside
/// this test) says, and every one their build scripts say they read
/// (`blaze_codegen_code::coverage::hold`). Anything else would be an input missing from every
/// template's key (AGENTS.md rule 21), and is named here.
#[test]
fn the_code_identity_hashes_every_input_the_code_generator_read() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    // As blaze-engine's build script reads it: the program that links the code generator.
    let workspace =
        blaze_codegen_code::Workspace::read(&manifest_dir.join("../blaze-engine")).unwrap();
    let mut env = std::collections::BTreeMap::new();
    if let Ok(prefix) = std::env::var(blaze_codegen_code::LLVM_PREFIX) {
        env.insert(blaze_codegen_code::LLVM_PREFIX.to_owned(), prefix);
    }
    // Built for the host, as this test is: whose release of the pinned LLVM is linked.
    for name in ["TARGET", "HOST"] {
        env.insert(name.to_owned(), blaze_codegen_code::coverage::host().to_owned());
    }
    let worked =
        blaze_codegen_code::work_out(&workspace, &blaze_codegen_code::BuildEnv::of(&env)).unwrap();
    assert!(worked.crates.contains("hir_ty_codegen"), "{:?}", worked.crates);
    // The target directory, the parent of the `tmp` cargo gives integration tests (the
    // executable may run from a copy elsewhere): every unit's dep-info is under it, in either of
    // cargo's layouts, and so is everything build scripts made.
    let made = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .expect("the target directory above CARGO_TARGET_TMPDIR");
    let exe = std::env::current_exe().unwrap();
    blaze_codegen_code::coverage::hold(&workspace, &worked, made, &exe)
        .unwrap()
        .assert_covered("the code generator");
}
