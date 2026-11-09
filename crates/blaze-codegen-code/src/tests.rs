//! The identity on the made-up workspace of [`crate::fixture`].

use std::{collections::BTreeMap, path::Path};

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
        (
            "the program's source",
            Box::new(|root| {
                write(root, "crates/cli/src/main.rs", "fn main() { println!(\"hi\") }\n")
            }),
        ),
        (
            "a test of a dependency",
            Box::new(|root| write(root, "crates/front/tests/it.rs", "#[test] fn other() {}\n")),
        ),
        ("a dotfile", Box::new(|root| write(root, "crates/front/.hidden", "x"))),
        ("a file outside every package", Box::new(|root| write(root, "README.md", "x"))),
        (
            "a checkout dev-dependency",
            Box::new(|root| write(root, "crates/devonly/src/lib.rs", "pub fn helps() { }\n")),
        ),
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
        (
            "its own source",
            Box::new(|root| {
                write(root, "crates/hir-ty-codegen/src/lib.rs", "pub fn compile() { }\n")
            }),
        ),
        (
            "a dependency's source",
            Box::new(|root| write(root, "crates/front/src/lib.rs", "pub fn lower() { }\n")),
        ),
        ("a new file in a dependency", Box::new(|root| write(root, "crates/front/src/new.rs", ""))),
        (
            "a workspace manifest",
            Box::new(|root| {
                let manifest = std::fs::read_to_string(root.join("crates/cli/Cargo.toml")).unwrap();
                write(
                    root,
                    "crates/cli/Cargo.toml",
                    &format!("{manifest}\n[features]\nextra = []\n"),
                );
            }),
        ),
        (
            "the pinned nightly",
            Box::new(|root| write(root, "harness/toolchain", "nightly-2026-11-02\n")),
        ),
        (
            "a registry package's checksum",
            Box::new(|root| relock(root, "checksum = \"aa\"", "checksum = \"bb\"")),
        ),
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
        code_after(|root| write(
            root,
            "crates/blaze-item-hash/src/lib.rs",
            "pub fn facts() { /* other MIR */ }\n"
        )),
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
    assert_ne!(
        work_out(&read, &other).unwrap().code,
        code,
        "the compiler that builds blaze is an input"
    );
    assert!(!env().vars.contains_key("HOME"), "$HOME is no input");
}

/// Rule 25: the same tree in another place, its files made in another order, is one code.
#[test]
fn the_code_is_of_the_contents_alone() {
    let a = Workspace::new("one", false).work_out().unwrap();
    let b = Workspace::new("two", true).work_out().unwrap();
    assert_eq!(a.code, b.code);
    assert_eq!(a.listing(), b.listing());
    assert_eq!(
        a.crates,
        [
            "blaze_codegen_code",
            "blaze_item_hash",
            "blaze_macro_eval",
            "front",
            "hir_def",
            "hir_ty",
            "hir_ty_codegen",
            "reg"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
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
    relock(
        workspace.root(),
        &format!("source = \"{}\"\nchecksum = \"aa\"", fixture::REGISTRY),
        "source = \"git+https://example.com/reg#0123abcd\"",
    );
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
    saved(
        "the lock",
        &|it| relock(it.root(), "checksum = \"cc\"", "checksum = \"ddd\""),
        "Cargo.lock",
    );
    saved(
        "a manifest",
        &|it| {
            it.write(
                "crates/cli/Cargo.toml",
                "[package]\nname = \"cli\"\nversion = \"0.1.0\"\n# saved\n",
            )
        },
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
    for name in [
        "CC",
        "CXX",
        "HOST_CFLAGS",
        "CC_aarch64-apple-darwin",
        "CXXFLAGS_aarch64_apple_darwin",
        "MACOSX_DEPLOYMENT_TARGET",
    ] {
        assert!(unkeyed_env(name).is_some(), "{name}");
    }
    for name in ["LLVM_SYS_231_FFI_WORKAROUND", "ISLE_SOURCE_DIR", "CI", "CCACHE"] {
        assert!(unkeyed_env(name).is_none(), "{name}");
    }
}

/// The inputs of `worked` of `kind`, by name.
fn of_kind(worked: &Worked, kind: &str) -> BTreeMap<String, [u8; 32]> {
    worked
        .inputs
        .iter()
        .filter(|it| it.kind == kind)
        .map(|it| (it.name.clone(), it.digest))
        .collect()
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
    assert_eq!(
        llvm.keys().collect::<Vec<_>>(),
        ["pin aarch64-apple-darwin"],
        "{}",
        worked.listing()
    );
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
    assert_ne!(
        pin_of(&fixture::LLVM_JSON.replace(version, "23.1.5")),
        pinned,
        "the pinned version is an input"
    );
    assert_ne!(
        pin_of(&fixture::LLVM_JSON.replace(commit, &"0".repeat(40))),
        pinned,
        "the pinned commit is an input"
    );

    // Built on one host for another target: both releases are.
    workspace.write("crates/hir-ty-codegen/llvm.json", fixture::LLVM_JSON);
    let mut cross = env();
    cross.vars.insert("TARGET".to_owned(), Some("x86_64-unknown-linux-gnu".to_owned()));
    let cross = of_kind(&work_out(&workspace.read().unwrap(), &cross).unwrap(), "llvm");
    assert_eq!(
        cross.keys().collect::<Vec<_>>(),
        ["pin aarch64-apple-darwin", "pin x86_64-unknown-linux-gnu"]
    );
}

/// C3 of the seedprof review: the stencil libraries are inputs by the ids their pin names
/// (blaze-stencil's build script holds every library it includes to them), and the seed, which
/// links no LLVM and makes no stencils with it, keys and checks no LLVM pin or prefix.
#[test]
fn the_stencils_are_inputs_by_their_pin_and_the_seed_keys_no_llvm() {
    let workspace = Workspace::with_llvm("stencils");
    let worked = workspace.work_out().unwrap();
    let stencils = of_kind(&worked, "stencils");
    assert_eq!(
        stencils.keys().collect::<Vec<_>>(),
        ["aarch64.stencils", "x86_64.stencils"],
        "{}",
        worked.listing()
    );
    assert_eq!(stencils["x86_64.stencils"], [0x44; 32]);
    workspace.write(
        "crates/blaze-stencil/stencils.json",
        &fixture::STENCILS_JSON.replace(&"4".repeat(64), &"6".repeat(64)),
    );
    let repinned = workspace.work_out().unwrap();
    assert_eq!(of_kind(&repinned, "stencils")["x86_64.stencils"], [0x66; 32]);
    assert_ne!(repinned.code, worked.code, "another pinned library is another code");

    let mut seed = env();
    seed.vars.insert(SEED_FEATURE.to_owned(), Some(String::new()));
    // No LLVM is read in the seed; the stencil libraries are blaze-stencil's committed files,
    // keyed by the pin as in every build.
    seed.llvm_prefix = Some(workspace.above().join("no-llvm").to_str().unwrap().to_owned());
    let worked =
        work_out(&workspace.read().unwrap(), &seed).expect("the seed checks no LLVM prefix");
    assert!(of_kind(&worked, "llvm").is_empty(), "{}", worked.listing());
    assert!(!worked.rerun_env.iter().any(|it| it == LLVM_PREFIX));
    assert!(worked.llvm_prefix.is_none());
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
        write(
            &prefix,
            pin::PIN_MARKER,
            &format!(
                "{{\"version\": \"{version}\", \"commit\": \"{commit}\", \"target\": \"t\"}}\n"
            ),
        );
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
        write(
            &prefix,
            pin::PIN_MARKER,
            &format!(
                "{{\"version\": \"{}\", \"commit\": \"{}\", \"target\": \"other\"}}\n",
                fixture::LLVM_PIN.0,
                fixture::LLVM_PIN.1
            ),
        );
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
    for bad in [
        blaze("ab"),
        blaze(&"zz".repeat(32)),
        format!("{one}{BLAZE_IDENTITY}{}\n", "cd".repeat(32)),
    ] {
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
    assert!(
        of_kind(&worked, "manifest").contains_key("$CARGO_HOME/config.toml"),
        "{}",
        worked.listing()
    );
    assert!(worked.files.contains(&home.join("config.toml")));
}

fn front_end_after(edit: impl FnOnce(&Path)) -> [u8; 32] {
    let workspace = Workspace::new("front-end", false);
    edit(workspace.root());
    workspace.code_of(&FRONT_END)
}

/// The front end's identity is of the packages the front end is built from, and of how they are
/// built: an edit of the code generator (`hir-ty-codegen`, its build dependency `reg`) or of
/// the program keeps it; one of `hir-def`, of what it is built from or of a hook keeps nothing.
#[test]
fn the_front_end_is_keyed_by_its_own_code_alone() {
    let code = front_end_after(|_| {});
    let kept: [(&str, Box<dyn FnOnce(&Path)>); 3] = [
        (
            "the code generator",
            Box::new(|root| {
                write(root, "crates/hir-ty-codegen/src/lib.rs", "pub fn compile() { }\n")
            }),
        ),
        (
            "the code generator's build dependency",
            Box::new(|root| relock(root, "checksum = \"aa\"", "checksum = \"bb\"")),
        ),
        (
            "the program's source",
            Box::new(|root| {
                write(root, "crates/cli/src/main.rs", "fn main() { println!(\"hi\") }\n")
            }),
        ),
    ];
    for (what, edit) in kept {
        assert_eq!(front_end_after(edit), code, "{what} is no input of the front end's code");
    }
    let changed: [(&str, Box<dyn FnOnce(&Path)>); 4] = [
        (
            "hir-def",
            Box::new(|root| write(root, "crates/hir-def/src/lib.rs", "pub fn resolve() { }\n")),
        ),
        (
            "a package a hook is built from",
            Box::new(|root| write(root, "crates/front/src/lib.rs", "pub fn lower() { }\n")),
        ),
        (
            "a hook (`BodyFacts`)",
            Box::new(|root| {
                write(root, "crates/blaze-item-hash/src/lib.rs", "pub fn facts() { }\n")
            }),
        ),
        (
            "the pinned nightly",
            Box::new(|root| write(root, "harness/toolchain", "nightly-2026-11-02\n")),
        ),
    ];
    for (what, edit) in changed {
        assert_ne!(front_end_after(edit), code, "{what} is an input of the front end's code");
    }
    let workspace = Workspace::new("front-end-built", false);
    let read = workspace.read().unwrap();
    let worked = work_out_of(&read, &FRONT_END, &env()).unwrap();
    assert_eq!(
        worked.packages,
        ["blaze-codegen-code", "blaze-item-hash", "blaze-macro-eval", "front", "hir-def", "hir-ty"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    let mut other = env();
    other.vars.insert("CARGO_ENCODED_RUSTFLAGS".to_owned(), Some("-Copt-level=1".to_owned()));
    assert_ne!(
        work_out_of(&read, &FRONT_END, &other).unwrap().code,
        worked.code,
        "how blaze is built is an input (rule 21)"
    );
    assert_ne!(
        worked.code,
        work_out(&read, &env()).unwrap().code,
        "each identity is of its own domain"
    );
}

/// C3 of the fekey review: cargo unifies features across the packages it builds together, so a
/// package outside an identity's closure that depends on one of it can turn on a feature of it:
/// `feat`, a registry package only the program uses, depends on `reg`, the code generator's build
/// dependency; `outside`, a package of the checkout that is no member, on `hir-def`. Its checksum,
/// or its manifest, is an input of each identity whose closure it reaches, and of none other.
#[test]
fn a_package_that_can_turn_on_a_feature_of_the_closure_is_an_input() {
    let (codegen, front_end) = (base(), front_end_after(|_| {}));
    let feat = |root: &Path| {
        relock(root, &format!("checksum = \"{}\"", fixture::FEAT_CHECKSUM), "checksum = \"ef\"")
    };
    assert_ne!(
        code_after(feat),
        codegen,
        "`feat` can turn on a feature of `reg`, of the code generator's closure"
    );
    assert_eq!(
        front_end_after(feat),
        front_end,
        "`feat` reaches nothing of the front end's closure"
    );
    let outside = |root: &Path| {
        let features =
            "[dependencies]\nhir-def = { path = \"../../crates/hir-def\", features = [\"x\"] }\n";
        write(root, fixture::OUTSIDE_MANIFEST, &fixture::manifest("outside", features));
    };
    assert_ne!(front_end_after(outside), front_end, "`outside` can turn on a feature of `hir-def`");
    assert_ne!(
        code_after(outside),
        codegen,
        "and `hir-def` is of the code generator's closure too"
    );
    assert_eq!(
        code_after(|root| write(root, "vendor/outside/src/lib.rs", "pub fn outside() { }\n")),
        codegen,
        "what a dependent's code is turns on no feature"
    );
    let worked = Workspace::new("dependents", false).work_out().unwrap();
    assert!(
        of_kind(&worked, "dependent").keys().any(|it| it.starts_with("feat 1.0.0")),
        "{}",
        worked.listing()
    );
    assert!(
        of_kind(&worked, "manifest").contains_key(fixture::OUTSIDE_MANIFEST),
        "{}",
        worked.listing()
    );
    assert!(
        !worked.packages.contains("feat") && !worked.packages.contains("outside"),
        "a dependent is no package of the closure"
    );
}

/// C4 of the fekey review: a file outside every package whose reader is not in the closure is an
/// error naming it, never left out: the identity names it wrongly.
#[test]
fn an_extra_whose_reader_is_outside_the_closure_is_an_error() {
    let wrong = Identity { extra: &[("nowhere", "../../harness/toolchain")], ..FRONT_END };
    let workspace = Workspace::new("extra-reader", false);
    let read = workspace.read().unwrap();
    let why = work_out_of(&read, &wrong, &env()).unwrap_err();
    assert!(why.contains("`nowhere`") && why.contains("harness/toolchain"), "{why}");
}

/// A package an identity cuts is no input, nor what only it is built from, though a root is
/// built from it: an edit of it keeps the identity, and an edit of a root still moves it. A cut
/// that names no package the program links, or a root, is refused, naming it (rule 21: a cut
/// names a package that is there, never one gone stale).
#[test]
fn a_cut_package_is_no_input_and_a_stale_cut_is_refused() {
    let cut =
        Identity { cut: &[Cut { package: "front", why: "the test's", named_in: &[] }], ..CODEGEN };
    let workspace = Workspace::new("cut", false);
    let worked = work_out_of(&workspace.read().unwrap(), &cut, &env()).unwrap();
    assert!(worked.packages.contains(CODE_GENERATOR), "{}", worked.listing());
    for gone in ["front", "hir-def", "blaze-macro-eval"] {
        assert!(
            !worked.packages.contains(gone),
            "{gone} is reached through the cut alone: {}",
            worked.listing()
        );
    }
    assert_ne!(worked.code, workspace.code(), "a cut is another closure");
    workspace.write("crates/front/src/lib.rs", "pub fn lower() { let _ = 1; }\n");
    let edited = work_out_of(&workspace.read().unwrap(), &cut, &env()).unwrap();
    assert_eq!(edited.code, worked.code, "an edit of a cut package keeps the identity");
    workspace.write("crates/hir-ty-codegen/src/lib.rs", "pub fn compile() { let _ = 2; }\n");
    let rooted = work_out_of(&workspace.read().unwrap(), &cut, &env()).unwrap();
    assert_ne!(rooted.code, worked.code, "an edit of a root moves the identity");

    let read = workspace.read().unwrap();
    let stale = Identity {
        cut: &[Cut { package: "nowhere", why: "the test's", named_in: &[] }],
        ..CODEGEN
    };
    let why = work_out_of(&read, &stale, &env()).unwrap_err();
    assert!(why.contains("`nowhere`") && why.contains("does not link"), "{why}");
    let both = Identity {
        cut: &[Cut { package: CODE_GENERATOR, why: "the test's", named_in: &[] }],
        ..CODEGEN
    };
    let why = work_out_of(&read, &both, &env()).unwrap_err();
    assert!(why.contains("both roots and cuts"), "{why}");
    // M2 of the overkey review: a root and a cut are one package by either of its names.
    let short = Identity {
        cut: &[Cut { package: "item-hash", why: "the test's", named_in: &[] }],
        ..CODEGEN
    };
    let why = work_out_of(&read, &short, &env()).unwrap_err();
    assert!(why.contains("both roots and cuts"), "{why}");

    // C1 of the overkey review: `hir-def` is built from by `front` and `hir-ty`, which are of the
    // closure and no root, so code the closure runs would be past it unreviewed.
    let reached = Identity {
        cut: &[Cut { package: "hir-def", why: "the test's", named_in: &[] }],
        ..CODEGEN
    };
    let why = work_out_of(&read, &reached, &env()).unwrap_err();
    assert!(
        why.contains("`hir-def`")
            && why.contains("`front`")
            && why.contains("`hir-ty`")
            && why.contains("no root"),
        "{why}"
    );
}

/// M2 of the fekey review: a link to a directory under a directory an identity reads outside
/// every package is refused, naming it; followed, a link to a directory above it never ends.
#[cfg(unix)]
#[test]
fn a_link_to_a_directory_in_an_extra_is_refused() {
    let workspace = Workspace::new("extra-link", false);
    let bridge = workspace.root().join("library/proc_macro/src/bridge");
    std::os::unix::fs::symlink(&bridge, bridge.join("again")).unwrap();
    let why = workspace.read().and_then(|read| work_out_of(&read, &FRONT_END, &env())).unwrap_err();
    assert!(why.contains("again") && why.contains("a link to a directory"), "{why}");
}

/// One walk for several identities hashes each package once and works out what a walk of each
/// alone does.
#[test]
fn one_walk_for_two_identities_is_each_walk_alone() {
    let workspace = Workspace::new("walk", false);
    let read = workspace.read().unwrap();
    let mut walk = Walk::default();
    let codegen = walk.work_out(&read, &CODEGEN, &env()).unwrap();
    let front_end = walk.work_out(&read, &FRONT_END, &env()).unwrap();
    for (shared, alone) in [
        (&codegen, work_out(&read, &env()).unwrap()),
        (&front_end, work_out_of(&read, &FRONT_END, &env()).unwrap()),
    ] {
        assert_eq!(shared.code, alone.code);
        assert_eq!(shared.listing(), alone.listing());
        assert_eq!(shared.files, alone.files);
        assert_eq!(shared.watched, alone.watched);
    }
}

/// A package cut from the checkout (published, a Nix unit's source) carries the tree of each
/// identity ([`ship`]), and its build script works the identity out of it with how it is built:
/// the identity, inputs and all, a build of the checkout has in the same environment, with the
/// shipped file the one thing of the tree it reruns on, and nothing of the checkout read again.
#[test]
fn a_shipped_tree_works_out_the_identity_the_checkout_does() {
    let llvm = Workspace::with_llvm("ship-codegen");
    let solver = Workspace::with_solver("ship-front-end");
    for (workspace, identity) in [(&llvm, &CODEGEN), (&solver, &FRONT_END)] {
        let home = workspace.above().join("home");
        write(&home, "config.toml", "[build]\nrustflags = [\"-Copt-level=1\"]\n");
        let mut built = env();
        built.cargo_home = Some(home);
        let read = workspace.read().unwrap();
        let checkout = work_out_of(&read, identity, &built).unwrap();
        let package = workspace.above().join("package");
        write(&package, identity.shipped, &ship(&read, identity).unwrap());
        let shipped = package.join(identity.shipped);
        let from_package = work_out_shipped(&shipped, identity, &built).unwrap();
        assert_eq!(
            from_package.listing(),
            checkout.listing(),
            "{}: the same inputs",
            identity.what
        );
        assert_eq!(from_package.code, checkout.code, "{}: the same identity", identity.what);
        assert!(from_package.watched.contains(&shipped), "{:?}", from_package.watched);
        assert!(
            !from_package.watched.iter().any(|it| it.starts_with(workspace.root())),
            "{:?}",
            from_package.watched
        );
        let (here, lock) = work_out_here(&package, &[identity], &built).unwrap();
        assert_eq!(
            (here[0].code, lock),
            (checkout.code, None),
            "{}: the shipped tree, and no lock to rerun on",
            identity.what
        );
        let (here, lock) =
            work_out_here(&workspace.root().join("crates/cli"), &[identity], &built).unwrap();
        assert_eq!(
            (here[0].code, lock),
            (checkout.code, Some(read.lock_path.clone())),
            "{}: the checkout",
            identity.what
        );
        workspace.write(
            "crates/hir-def/src/lib.rs",
            "pub fn resolve() { /* edited after shipping */ }\n",
        );
        assert_eq!(
            work_out_shipped(&shipped, identity, &built).unwrap().code,
            checkout.code,
            "{}: of the tree shipped",
            identity.what
        );
        assert_ne!(
            work_out_of(&workspace.read().unwrap(), identity, &built).unwrap().code,
            checkout.code
        );
    }
    let other = Workspace::new("ship-other", false);
    let read = other.read().unwrap();
    let wrong = other.above().join(FRONT_END.shipped);
    write(other.above(), FRONT_END.shipped, &ship(&read, &CODEGEN).unwrap());
    let refused = work_out_shipped(&wrong, &FRONT_END, &env()).unwrap_err();
    assert!(refused.contains(CODEGEN.domain) && refused.contains(FRONT_END.domain), "{refused}");
}

/// A program with neither a checkout nor a shipped tree is refused, naming both: no identity
/// stands in for one (AGENTS.md rule 3).
#[test]
fn no_checkout_and_no_shipped_tree_is_refused_naming_both() {
    let workspace = Workspace::new("nothing", false);
    let alone = workspace.above().join("alone");
    write(&alone, "Cargo.toml", "[package]\nname = \"alone\"\nversion = \"0.1.0\"\n");
    let refused = work_out_here(&alone, &[&CODEGEN, &FRONT_END], &env()).unwrap_err();
    assert!(refused.contains("no checkout"), "{refused}");
    assert!(refused.contains(CODEGEN.shipped), "{refused}");
}

/// The trait solver is of the front end's identity: an edit of its source, its patch series or a
/// patch is another identity (every stored answer misses), one of its tests keeps it.
#[test]
fn the_vendored_solver_is_of_the_front_ends_identity() {
    let after = |edit: &dyn Fn(&Workspace)| {
        let workspace = Workspace::with_solver("solver");
        edit(&workspace);
        workspace.code_of(&FRONT_END)
    };
    let code = after(&|_| {});
    let (source, text) = fixture::SOLVER_SOURCE;
    let changed: [(&str, &dyn Fn(&Workspace)); 3] = [
        ("its source", &|it| it.write(source, &format!("{text}// another rule\n"))),
        ("its patch series", &|it| {
            it.write("vendor/rustc_next_trait_solver/PATCHES.md", "# Patches\n")
        }),
        ("a patch", &|it| {
            it.write("vendor/rustc_next_trait_solver/patches/0001-one.patch", "--- a\n+++ c\n")
        }),
    ];
    for (what, edit) in changed {
        assert_ne!(after(edit), code, "the solver's {what} is an input of the front end's code");
    }
    assert_eq!(
        after(
            &|it| it.write("vendor/rustc_next_trait_solver/tests/it.rs", "#[test] fn other() {}\n")
        ),
        code
    );
    let workspace = Workspace::with_solver("solver-files");
    let worked = work_out_of(&workspace.read().unwrap(), &FRONT_END, &env()).unwrap();
    assert!(worked.packages.contains("rustc_next_trait_solver"), "{:?}", worked.packages);
    assert!(
        worked.files.contains(&workspace.root().join(source).canonicalize().unwrap()),
        "{:?}",
        worked.files
    );
}

mod cuts;
mod hooks;
