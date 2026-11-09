//! The front end's code identity (`blaze_codegen_code::FRONT_END`) and that of what makes a unit's
//! record (`blaze_codegen_code::RECORD`) held to what building them read. An integration test of
//! blaze-cargo, which is built from every package of both closures (the record's is blaze-cargo's
//! own, but its cuts), for `CARGO_TARGET_TMPDIR`, which names the target directory whose dep-info
//! and build-script output it reads wherever the test executable runs from.

use std::path::Path;

/// What the front end's code identity is of covers every file and variable rustc read building
/// the front end's crates, as their dep-info says, and every one their build scripts say they read
/// (`blaze_codegen_code::coverage::hold`). Anything else would be an input missing from the key
/// of every image, check outcome, checked body, constant, goal and kept expansion (AGENTS.md
/// rule 21), and is named here.
#[test]
fn the_front_end_identity_hashes_every_input_the_front_end_read() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    // As blaze-engine's build script reads it: the program that runs the front end.
    let workspace =
        blaze_codegen_code::Workspace::read(&manifest_dir.join("../blaze-engine")).unwrap();
    let mut env = std::collections::BTreeMap::new();
    for name in ["TARGET", "HOST"] {
        env.insert(name.to_owned(), blaze_codegen_code::coverage::host().to_owned());
    }
    let build = blaze_codegen_code::BuildEnv::of(&env);
    let worked =
        blaze_codegen_code::work_out_of(&workspace, &blaze_codegen_code::FRONT_END, &build)
            .unwrap();
    for krate in ["hir_def", "hir_ty", "blaze_macro_eval", "blaze_diagnostics"] {
        assert!(worked.crates.contains(krate), "{krate} is of the front end: {:?}", worked.crates);
    }
    let made = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .expect("the target directory above CARGO_TARGET_TMPDIR");
    let exe = std::env::current_exe().unwrap();
    blaze_codegen_code::coverage::hold(&workspace, &worked, made, &exe)
        .unwrap()
        .assert_covered("the front end");
}

/// What the record's code identity is of covers every file and variable rustc read building
/// blaze-cargo and the crates it is built from (but the packages the identity cuts, which are no
/// input), and every one their build scripts say they read. Anything else would be an input
/// missing from every unit record's key and the standard library's shared directory (AGENTS.md
/// rule 21), and is named here.
#[test]
fn the_record_identity_hashes_every_input_what_makes_a_record_read() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace =
        blaze_codegen_code::Workspace::read(&manifest_dir.join("../blaze-engine")).unwrap();
    // cargo gives every build script `DEBUG`, `OPT_LEVEL` and `PROFILE`, which the identity keys
    // as how blaze is built; ring's build script says it reads `DEBUG`.
    let mut env = std::collections::BTreeMap::new();
    for name in ["TARGET", "HOST"] {
        env.insert(name.to_owned(), blaze_codegen_code::coverage::host().to_owned());
    }
    for (name, value) in [("DEBUG", "true"), ("OPT_LEVEL", "0"), ("PROFILE", "debug")] {
        env.insert(name.to_owned(), value.to_owned());
    }
    let build = blaze_codegen_code::BuildEnv::of(&env);
    let worked =
        blaze_codegen_code::work_out_of(&workspace, &blaze_codegen_code::RECORD, &build).unwrap();
    for krate in ["blaze_cargo", "blaze_lints", "blaze_link", "hir_ty_codegen", "hir_def"] {
        assert!(worked.crates.contains(krate), "{krate} makes a record: {:?}", worked.crates);
    }
    for cut in blaze_codegen_code::RECORD_CUT {
        assert!(
            !worked.packages.contains(cut.package),
            "{} is cut: {:?}",
            cut.package,
            worked.packages
        );
    }
    let made = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .expect("the target directory above CARGO_TARGET_TMPDIR");
    let exe = std::env::current_exe().unwrap();
    blaze_codegen_code::coverage::hold(&workspace, &worked, made, &exe)
        .unwrap()
        .assert_covered("what makes a record");
}

/// Every file of the vendored trait solver is an input of the front end's code identity, so of
/// the goal and item stores' solver version: each file under `vendor/rustc_next_trait_solver`,
/// `vendor/rustc_type_ir` and `vendor/rustc_type_ir_macros` (`src/`, the manifests, `PATCHES.md`
/// and every patch) and the pinned nightly. A solver change, in a patch or straight in `src/`, is
/// another identity, so no stored answer of the old solver is found (hir-ty's key tests show the
/// miss). The patches are what dep-info cannot show: rustc reads no patch.
#[test]
fn the_front_end_identity_hashes_every_vendored_solver_file_and_patch() {
    fn files_under(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in
            std::fs::read_dir(dir).unwrap_or_else(|why| panic!("listing {}: {why}", dir.display()))
        {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_under(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace =
        blaze_codegen_code::Workspace::read(&manifest_dir.join("../blaze-engine")).unwrap();
    let worked = blaze_codegen_code::work_out_of(
        &workspace,
        &blaze_codegen_code::FRONT_END,
        &blaze_codegen_code::BuildEnv::default(),
    )
    .unwrap();
    let mut on_disk = vec![workspace.root.join("harness/toolchain")];
    for krate in ["rustc_next_trait_solver", "rustc_type_ir", "rustc_type_ir_macros"] {
        files_under(&workspace.root.join("vendor").join(krate), &mut on_disk);
    }
    let patches =
        on_disk.iter().filter(|it| it.extension().is_some_and(|ext| ext == "patch")).count();
    assert!(patches >= 2, "the vendored solver has patches: found {patches} under vendor/");
    let missing: Vec<String> = on_disk
        .iter()
        .filter(|it| !worked.files.contains(*it))
        .map(|it| it.display().to_string())
        .collect();
    assert!(
        missing.is_empty(),
        "the front end's code identity misses {} solver file(s): {}",
        missing.len(),
        missing.join(", ")
    );
}
