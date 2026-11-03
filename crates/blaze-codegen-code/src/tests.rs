//! The identity on the made-up workspace of [`crate::fixture`].

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use super::*;
use crate::fixture::{Workspace, env, write};

fn code_after(edit: impl FnOnce(&Path)) -> [u8; 32] {
    let workspace = Workspace::new("edit", false);
    edit(workspace.root());
    workspace.code()
}

fn base() -> [u8; 32] {
    code_after(|_| {})
}

fn relock(root: &Path, from: &str, to: &str) {
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    assert!(lock.contains(from), "{from:?} in the fixture's lock");
    std::fs::write(root.join("Cargo.lock"), lock.replace(from, to)).unwrap();
}

#[test]
fn an_edit_outside_the_code_generator_keeps_its_code() {
    let code = base();
    let edits: [(&str, Box<dyn FnOnce(&Path)>); 6] = [
        ("the program's source", Box::new(|root| write(root, "crates/cli/src/main.rs", "fn main() { println!(\"hi\") }\n"))),
        ("a test of a dependency", Box::new(|root| write(root, "crates/front/tests/it.rs", "#[test] fn other() {}\n"))),
        ("a dotfile", Box::new(|root| write(root, "crates/front/.hidden", "x"))),
        ("a file outside every package", Box::new(|root| write(root, "README.md", "x"))),
        ("a checkout dev-dependency", Box::new(|root| write(root, "crates/devonly/src/lib.rs", "pub fn helps() { }\n"))),
        (
            "a registry dev-dependency's checksum",
            Box::new(|root| relock(root, "checksum = \"cc\"", "checksum = \"dd\"")),
        ),
    ];
    for (what, edit) in edits {
        assert_eq!(code_after(edit), code, "{what} is no input of the code generator's code");
    }
}

#[test]
fn an_edit_of_the_code_generator_changes_its_code() {
    let code = base();
    let edits: [(&str, Box<dyn FnOnce(&Path)>); 6] = [
        ("its own source", Box::new(|root| write(root, "crates/hir-ty-codegen/src/lib.rs", "pub fn compile() { }\n"))),
        ("a dependency's source", Box::new(|root| write(root, "crates/front/src/lib.rs", "pub fn lower() { }\n"))),
        ("a new file in a dependency", Box::new(|root| write(root, "crates/front/src/new.rs", ""))),
        (
            "a workspace manifest",
            Box::new(|root| {
                let manifest = std::fs::read_to_string(root.join("crates/cli/Cargo.toml")).unwrap();
                write(root, "crates/cli/Cargo.toml", &format!("{manifest}\n[features]\nextra = []\n"));
            }),
        ),
        ("the pinned nightly", Box::new(|root| write(root, "harness/toolchain", "nightly-2026-11-02\n"))),
        ("a registry package's checksum", Box::new(|root| relock(root, "checksum = \"aa\"", "checksum = \"bb\""))),
    ];
    for (what, edit) in edits {
        assert_ne!(code_after(edit), code, "{what} is an input of the code generator's code");
    }
}

/// C1 of the review: code installed into the front end through a `dyn` hook from past the code
/// generator's packages (blaze-item-hash's `BodyFacts`) chooses the MIR it compiles, so its
/// package is a root of the closure, and so is every package it is built from.
#[test]
fn an_edit_of_code_the_front_end_runs_through_a_hook_changes_the_code() {
    assert_ne!(
        code_after(|root| write(root, "crates/blaze-item-hash/src/lib.rs", "pub fn facts() { /* other MIR */ }\n")),
        base(),
        "blaze-item-hash, installed as `BodyFacts`, is an input"
    );
    let worked = Workspace::new("hooked", false).work_out().unwrap();
    assert!(worked.packages.contains("blaze-item-hash"), "{:?}", worked.packages);
    assert!(ROOTS.iter().any(|it| it.package == "blaze-item-hash"));
}

#[test]
fn how_blaze_is_built_is_in_the_code() {
    let workspace = Workspace::new("env", false);
    let read = workspace.read().unwrap();
    let code = work_out(&read, &env()).unwrap().code;
    for (name, value) in [
        ("PROFILE", "debug"),
        ("CARGO_FEATURE_LLVM", "1"),
        ("CARGO_ENCODED_RUSTFLAGS", "-Copt-level=1"),
        ("CI", "true"),
        ("LLVM_SYS_231_FFI_WORKAROUND", "1"),
    ] {
        let mut other = env();
        other.vars.insert(name.to_owned(), Some(value.to_owned()));
        assert_ne!(work_out(&read, &other).unwrap().code, code, "${name} is an input");
    }
    let mut other = env();
    other.compiler = Some("rustc 1.101.0-nightly (1111111 2026-11-02)\n".to_owned());
    assert_ne!(work_out(&read, &other).unwrap().code, code, "the compiler that builds blaze is an input");
    assert!(!env().vars.contains_key("HOME"), "$HOME is no input");
}

/// Rule 22: the same tree in another place, its files made in another order, is one code.
#[test]
fn the_code_is_of_the_contents_alone() {
    let a = Workspace::new("one", false).work_out().unwrap();
    let b = Workspace::new("two", true).work_out().unwrap();
    assert_eq!(a.code, b.code);
    assert_eq!(a.listing(), b.listing());
    assert_eq!(a.crates, ["blaze_item_hash", "front", "hir_ty_codegen", "reg"].into_iter().map(str::to_owned).collect());
}

#[test]
fn an_input_that_cant_be_had_is_an_error_naming_it() {
    let workspace = Workspace::new("missing", false);
    std::fs::remove_file(workspace.root().join("harness/toolchain")).unwrap();
    let why = workspace.work_out().unwrap_err();
    assert!(why.contains("harness/toolchain"), "{why}");

    let workspace = Workspace::new("unlocked", false);
    relock(workspace.root(), "checksum = \"aa\"\n", "");
    let why = workspace.work_out().unwrap_err();
    assert!(why.contains("reg 1.0.0") && why.contains("checksum"), "{why}");

    let workspace = Workspace::new("git", false);
    relock(workspace.root(), &format!("source = \"{}\"\nchecksum = \"aa\"", fixture::REGISTRY), "source = \"git+https://example.com/reg#0123abcd\"");
    let why = workspace.work_out().unwrap_err();
    assert!(why.contains("reg 1.0.0") && why.contains("git"), "{why}");

    let workspace = Workspace::new("nolock", false);
    std::fs::remove_file(workspace.root().join("Cargo.lock")).unwrap();
    let why = workspace.work_out().unwrap_err();
    assert!(why.contains("Cargo.lock"), "{why}");
}

/// C4 of the review: a file saved while the identity was worked out is named, so the build
/// script refuses an identity of no one tree.
#[test]
fn a_file_saved_while_it_was_worked_out_is_named() {
    let workspace = Workspace::new("saved", false);
    let worked = workspace.work_out().unwrap();
    assert!(worked.changed().is_empty(), "{:?}", worked.changed());
    workspace.write("crates/front/src/lib.rs", "pub fn lower() { /* saved during the build */ }\n");
    let changed = worked.changed();
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert!(changed[0].ends_with("crates/front/src/lib.rs"), "{changed:?}");
}

/// N3 of the re-review: the lock and a manifest parsed to find the closure, and a file added
/// to a package, are seen too; a manifest parsed early and hashed later keeps its first stamp.
#[test]
fn a_lock_manifest_or_new_file_saved_while_it_was_worked_out_is_named() {
    let saved = |what: &str, edit: &dyn Fn(&Workspace), named: &str| {
        let workspace = Workspace::new("stamped", false);
        let read = workspace.read().unwrap();
        // Between parsing for the closure and hashing.
        edit(&workspace);
        let changed = work_out(&read, &env()).unwrap().changed();
        assert!(changed.iter().any(|it| it.ends_with(named)), "{what}: {changed:?}");
    };
    saved("the lock", &|it| relock(it.root(), "checksum = \"cc\"", "checksum = \"ddd\""), "Cargo.lock");
    saved(
        "a manifest",
        &|it| it.write("crates/cli/Cargo.toml", "[package]\nname = \"cli\"\nversion = \"0.1.0\"\n# saved\n"),
        "crates/cli/Cargo.toml",
    );
    let workspace = Workspace::new("added", false);
    let worked = workspace.work_out().unwrap();
    workspace.write("crates/front/src/added.rs", "");
    let changed = worked.changed();
    assert!(changed.iter().any(|it| it.ends_with("crates/front/src")), "{changed:?}");
}

#[test]
fn the_c_compiler_is_no_input_and_what_else_a_build_script_reads_is() {
    for name in ["CC", "CXX", "HOST_CFLAGS", "CC_aarch64-apple-darwin", "CXXFLAGS_aarch64_apple_darwin", "MACOSX_DEPLOYMENT_TARGET"] {
        assert!(unkeyed_env(name).is_some(), "{name}");
    }
    for name in ["LLVM_SYS_231_FFI_WORKAROUND", "ISLE_SOURCE_DIR", "CI", "CCACHE"] {
        assert!(unkeyed_env(name).is_none(), "{name}");
    }
}

/// The inputs of `worked` of `kind`, by name.
fn of_kind(worked: &Worked, kind: &str) -> BTreeMap<String, [u8; 32]> {
    worked.inputs.iter().filter(|it| it.kind == kind).map(|it| (it.name.clone(), it.digest)).collect()
}

/// N1 of the re-review: LLVM is an input by the pin llvm-sys checks it against (llvm.json's
/// version, commit and the release of each tuple it is built for), which needs no prefix, so a
/// build with none set is no error here; and a package closure with no llvm-sys has no LLVM.
#[test]
fn llvm_is_an_input_by_its_pin_and_needs_no_prefix() {
    let none = Workspace::new("nollvm", false).work_out().unwrap();
    assert!(of_kind(&none, "llvm").is_empty(), "{:?}", none.listing());

    let workspace = Workspace::with_llvm("pin");
    let worked = workspace.work_out().expect("no $LLVM_SYS_231_PREFIX is no error");
    let llvm = of_kind(&worked, "llvm");
    assert_eq!(llvm.keys().collect::<Vec<_>>(), ["pin aarch64-apple-darwin"], "{}", worked.listing());
    assert!(worked.llvm_prefix.is_none());
    assert!(worked.rerun_env.iter().any(|it| it == LLVM_PREFIX));
    let pin_of = |json: &str| {
        workspace.write("crates/hir-ty-codegen/llvm.json", json);
        of_kind(&workspace.work_out().unwrap(), "llvm")["pin aarch64-apple-darwin"]
    };
    let pinned = llvm["pin aarch64-apple-darwin"];
    let other_target = fixture::LLVM_JSON.replace(&"2".repeat(64), &"3".repeat(64));
    assert_eq!(pin_of(&other_target), pinned, "another target's release is no input");
    let own_target = fixture::LLVM_JSON.replace(&"1".repeat(64), &"3".repeat(64));
    assert_ne!(pin_of(&own_target), pinned, "the release of the target built for is an input");
    let (version, commit) = fixture::LLVM_PIN;
    assert_ne!(pin_of(&fixture::LLVM_JSON.replace(version, "23.1.5")), pinned, "the pinned version is an input");
    assert_ne!(pin_of(&fixture::LLVM_JSON.replace(commit, &"0".repeat(40))), pinned, "the pinned commit is an input");

    // Built on one host for another target: both releases are.
    workspace.write("crates/hir-ty-codegen/llvm.json", fixture::LLVM_JSON);
    let mut cross = env();
    cross.vars.insert("TARGET".to_owned(), Some("x86_64-unknown-linux-gnu".to_owned()));
    let cross = of_kind(&work_out(&workspace.read().unwrap(), &cross).unwrap(), "llvm");
    assert_eq!(cross.keys().collect::<Vec<_>>(), ["pin aarch64-apple-darwin", "pin x86_64-unknown-linux-gnu"]);
}

/// C3 of the seedprof review: the stencil libraries are inputs by the ids their pin names
/// (blaze-stencil's build script holds every library it includes to them), and the seed, which
/// links no LLVM and makes no stencils with it, keys and checks no LLVM pin or prefix.
#[test]
fn the_stencils_are_inputs_by_their_pin_and_the_seed_keys_no_llvm() {
    let workspace = Workspace::with_llvm("stencils");
    let worked = workspace.work_out().unwrap();
    let stencils = of_kind(&worked, "stencils");
    assert_eq!(stencils.keys().collect::<Vec<_>>(), ["aarch64.stencils", "x86_64.stencils"], "{}", worked.listing());
    assert_eq!(stencils["x86_64.stencils"], [0x44; 32]);
    workspace.write("crates/blaze-stencil/stencils.json", &fixture::STENCILS_JSON.replace(&"4".repeat(64), &"6".repeat(64)));
    let repinned = workspace.work_out().unwrap();
    assert_eq!(of_kind(&repinned, "stencils")["x86_64.stencils"], [0x66; 32]);
    assert_ne!(repinned.code, worked.code, "another pinned library is another code");

    let mut seed = env();
    seed.vars.insert(SEED_FEATURE.to_owned(), Some(String::new()));
    // Neither is read in the seed: no LLVM is checked, and the bytes are blaze-stencil's to check.
    seed.llvm_prefix = Some(workspace.above().join("no-llvm").to_str().unwrap().to_owned());
    seed.stencils_dir = Some("/prebuilt".to_owned());
    let worked = work_out(&workspace.read().unwrap(), &seed).expect("the seed checks no LLVM prefix");
    assert!(of_kind(&worked, "llvm").is_empty(), "{}", worked.listing());
    assert!(!worked.rerun_env.iter().any(|it| it == LLVM_PREFIX));
    assert!(worked.llvm_prefix.is_none());
    assert_eq!(worked.stencils_dir.as_deref(), Some(Path::new("/prebuilt")));
    assert_eq!(of_kind(&worked, "stencils"), of_kind(&repinned, "stencils"));

    workspace.write("crates/blaze-stencil/stencils.json", "{ \"x86_64.stencils\": \"44\" }\n");
    let error = workspace.work_out().unwrap_err();
    assert!(error.contains("stencils.json") && error.contains("x86_64.stencils"), "{error}");
}

/// A prefix set is checked to be the pin by pin.rs, refused with its message when it is not,
/// and its marker is an input when it is.
#[test]
fn a_prefix_is_checked_to_be_the_pin() {
    let workspace = Workspace::with_llvm("prefix");
    let prefix = workspace.above().join("llvm");
    std::fs::create_dir_all(prefix.join("bin")).unwrap();
    let with_prefix = || {
        let mut env = env();
        env.llvm_prefix = Some(prefix.to_str().unwrap().to_owned());
        work_out(&workspace.read().unwrap(), &env)
    };
    let why = with_prefix().unwrap_err();
    assert!(why.contains("blaze links only the LLVM") && why.contains(pin::PIN_MARKER), "{why}");
    let marker = |commit: &str| {
        let (version, _) = fixture::LLVM_PIN;
        write(&prefix, pin::PIN_MARKER, &format!("{{\"version\": \"{version}\", \"commit\": \"{commit}\", \"target\": \"t\"}}\n"));
    };
    marker(&"0".repeat(40));
    let why = with_prefix().unwrap_err();
    assert!(why.contains(&format!("at commit {}", "0".repeat(40))), "{why}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let config = prefix.join("bin/llvm-config");
        write(&prefix, "bin/llvm-config", &format!("#!/bin/sh\necho {}\n", fixture::LLVM_PIN.0));
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o755)).unwrap();
        marker(fixture::LLVM_PIN.1);
        let worked = with_prefix().unwrap();
        assert!(of_kind(&worked, "llvm").contains_key(pin::PIN_MARKER), "{}", worked.listing());
        assert_eq!(worked.llvm_prefix.as_deref(), Some(prefix.canonicalize().unwrap().as_path()));
        let code = worked.code;
        write(&prefix, pin::PIN_MARKER, &format!(
            "{{\"version\": \"{}\", \"commit\": \"{}\", \"target\": \"other\"}}\n",
            fixture::LLVM_PIN.0,
            fixture::LLVM_PIN.1
        ));
        assert_ne!(with_prefix().unwrap().code, code, "the prefix's marker is an input");
    }
}

/// N2: what blaze as `rustc` says of itself is part of the compiler, so a blaze built by blaze
/// is another code than one rustc built; and a `-vV` that names its blaze badly is refused.
#[test]
fn which_blaze_built_blaze_is_in_the_code() {
    let rustc = "rustc 1.101.0-nightly (16d1ffac1 2026-10-26)\nhost: aarch64-apple-darwin\nrelease: 1.101.0-nightly\n";
    let blaze = |hex: &str| format!("{rustc}{BLAZE_IDENTITY}{hex}\n");
    assert_eq!(check_compiler("rustc", rustc.to_owned()).as_deref(), Ok(rustc));
    let one = blaze(&"ab".repeat(32));
    assert_eq!(check_compiler("blaze", one.clone()), Ok(one.clone()));
    for bad in [blaze("ab"), blaze(&"zz".repeat(32)), format!("{one}{BLAZE_IDENTITY}{}\n", "cd".repeat(32))] {
        let why = check_compiler("blaze", bad).unwrap_err();
        assert!(why.contains(BLAZE_IDENTITY.trim()), "{why}");
    }
    let usage = check_compiler("blaze", "Usage: blaze <COMMAND>\n".to_owned()).unwrap_err();
    assert!(usage.contains("no rustc's version"), "{usage}");

    let workspace = Workspace::new("compiler", false);
    let read = workspace.read().unwrap();
    let code = |said: &str| {
        let mut env = env();
        env.compiler = Some(said.to_owned());
        work_out(&read, &env).unwrap().code
    };
    assert_ne!(code(&one), code(rustc), "blaze as rustc is another compiler than rustc");
    assert_ne!(code(&one), code(&blaze(&"cd".repeat(32))), "another blaze is another compiler");
}

/// N6: cranelift-codegen's ISLE from a directory of the environment's is no input, so a build
/// with it set is refused, naming it.
#[test]
fn an_isle_source_dir_is_refused() {
    let workspace = Workspace::new("isle", false);
    let read = workspace.read().unwrap();
    let mut env = env();
    env.vars.insert(ISLE_SOURCE_DIR.to_owned(), Some("/elsewhere/isle".to_owned()));
    let why = work_out(&read, &env).unwrap_err();
    assert!(why.contains(ISLE_SOURCE_DIR) && why.contains("/elsewhere/isle"), "{why}");
}

/// M1: cargo's configuration above the workspace and in `$CARGO_HOME` is an input.
#[test]
fn cargo_configuration_above_the_workspace_and_in_cargo_home_is_an_input() {
    let workspace = Workspace::new("configs", false);
    let code = workspace.code();
    write(workspace.above(), ".cargo/config.toml", "[profile.release]\nopt-level = 1\n");
    let above = workspace.code();
    assert_ne!(above, code, "a configuration above the workspace is an input");
    let home = workspace.above().join("home");
    write(&home, "config.toml", "[build]\nrustflags = [\"-Copt-level=1\"]\n");
    let mut with_home = env();
    with_home.cargo_home = Some(home.clone());
    let worked = work_out(&workspace.read().unwrap(), &with_home).unwrap();
    assert_ne!(worked.code, above, "$CARGO_HOME's configuration is an input");
    assert!(of_kind(&worked, "manifest").contains_key("$CARGO_HOME/config.toml"), "{}", worked.listing());
    assert!(worked.files.contains(&home.join("config.toml")));
}

/// The trait an `impl` line implements, by its last path segment: `impl<T> a::Hook<T> for X {`
/// is `Hook`.
fn implemented(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let rest = line.strip_prefix("unsafe ").unwrap_or(line).strip_prefix("impl")?;
    let (head, _) = rest.split_once(" for ")?;
    let mut head = head.trim_start();
    if head.starts_with('<') {
        let mut depth = 0i32;
        let end = head.char_indices().find_map(|(at, c)| {
            depth += i32::from(c == '<') - i32::from(c == '>');
            (depth == 0).then_some(at)
        })?;
        head = &head[end + 1..];
    }
    let path = head.split('<').next()?.trim();
    Some(path.rsplit("::").next().unwrap_or(path))
}

/// Test code (a `tests` directory, `tests.rs`, `*_tests.rs`): no part of a program.
fn is_test(path: &Path) -> bool {
    path.components().any(|it| it.as_os_str() == "tests")
        || path.file_name().and_then(|it| it.to_str()).is_some_and(|it| it == "tests.rs" || it.ends_with("_tests.rs"))
}

/// The `.rs` files of the package at `dir`, but those of packages inside it.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|it| it.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if path.file_name() != Some("target".as_ref()) && !path.join("Cargo.toml").is_file() {
                rust_files(&path, out);
            }
        } else if path.extension() == Some("rs".as_ref()) {
            out.push(path);
        }
    }
}

/// C1 of the review, held on blaze's own tree: every implementation in the checkout, past its
/// tests, of a hook the front end or code generator calls through `dyn` is of a package of the
/// code generator's closure (so an input of its code) or listed, reviewed, in
/// [`REVIEWED_HOOKS`]; and every listed one of the checkout is still there.
#[test]
fn every_hook_implemented_past_the_closure_is_reviewed() {
    let engine = Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-engine");
    let workspace = crate::Workspace::read(&engine).unwrap();
    let closure = workspace.closure_names().unwrap();
    assert!(closure.contains("blaze-item-hash"), "blaze-item-hash, installed as `BodyFacts`, is in the closure");
    let checkout = workspace.checkout();
    let mut found = BTreeSet::new();
    let mut unreviewed = Vec::new();
    for (package, dir) in &checkout {
        if closure.contains(*package) {
            continue;
        }
        let mut files = Vec::new();
        rust_files(dir, &mut files);
        for path in files.iter().filter(|it| !is_test(it.strip_prefix(dir).unwrap())) {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            for (at, line) in text.lines().enumerate() {
                let Some(hook) = implemented(line).and_then(|it| HOOK_TRAITS.iter().find(|hook| **hook == it)) else { continue };
                found.insert((*package, *hook));
                if !REVIEWED_HOOKS.iter().any(|it| it.package == *package && it.hook == *hook) {
                    unreviewed.push(format!("{}:{}: {package} implements `{hook}`", path.display(), at + 1));
                }
            }
        }
    }
    assert!(
        unreviewed.is_empty(),
        "code past the code generator's closure is installed into it: add its package to ROOTS, or review \
         it into REVIEWED_HOOKS:\n{}",
        unreviewed.join("\n")
    );
    let stale: Vec<&Hook> = REVIEWED_HOOKS
        .iter()
        .filter(|it| checkout.iter().any(|(package, _)| *package == it.package) && !found.contains(&(it.package, it.hook)))
        .collect();
    assert!(stale.is_empty(), "reviewed hooks no longer implemented: {stale:?}");
}

#[test]
fn an_impl_line_names_its_trait() {
    assert_eq!(implemented("impl BodyFacts for Bodies {"), Some("BodyFacts"));
    assert_eq!(implemented("    impl blaze_codegen::driver::Lookup for Reading<'_> {"), Some("Lookup"));
    assert_eq!(implemented("impl<'db, T: Into<U>> Prepare<T> for Backends<'db> {"), Some("Prepare"));
    assert_eq!(implemented("unsafe impl Send for X {}"), Some("Send"));
    assert_eq!(implemented("impl Bodies {"), None);
}
