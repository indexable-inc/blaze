//! A made-up workspace to work the identity out over, for tests here and in the code generator,
//! on disk with its manifests and lock file: the program `cli` depends on `hir-ty-codegen` and
//! `blaze-item-hash`; `hir-ty-codegen` depends on `front`, on the registry package `reg` as a
//! build dependency, and on the registry package `testonly` and the checkout package `devonly`
//! as dev-dependencies; `blaze-item-hash` depends on `front`, and `front` on `hir-def`, the
//! required root of the front end's identity ([`crate::FRONT_END`]), and on `blaze-macro-eval`,
//! which depends on `hir-ty`, which depends on `hir-def` and has `blaze-codegen-code` as its build
//! dependency: each reads the files outside every package [`crate::FRONT_END_EXTRA`] names, which
//! are there. `cli` also depends on the registry package `feat`, which depends on `reg`, and on
//! `outside`, a package of the checkout that is no member (under `vendor/`) and depends on
//! `hir-def`: each can turn on a feature of a package of the closures. [`Workspace::with_llvm`] adds the
//! checkout package `llvm-sys` as a build dependency of `hir-ty-codegen`, and its pin
//! `crates/hir-ty-codegen/llvm.json`; and `blaze-stencil`, a dependency of `hir-ty-codegen` with
//! `llvm-sys` as its build dependency, and its pin `crates/blaze-stencil/stencils.json`.
//! [`Workspace::with_solver`] adds the vendored trait solver `rustc_next_trait_solver`
//! (`vendor/`, with its `PATCHES.md`, a patch and a test), a dependency of `hir-def`, as hir-ty's
//! is in blaze.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use crate::{BuildEnv, Identity, Worked, work_out, work_out_of};

pub const CHECKSUM: &str = "aa";
/// The checksum of `feat`, the registry package that depends on `reg` and that only `cli` uses.
pub const FEAT_CHECKSUM: &str = "ee";

/// The workspace in `ws/` of a fresh directory of its own, removed when dropped.
pub struct Workspace {
    /// The directory of its own, above the workspace.
    dir: PathBuf,
    root: PathBuf,
}

impl Workspace {
    /// Made with its files in order, or in the reverse order.
    pub fn new(what: &str, reversed: bool) -> Workspace {
        Workspace::made(what, reversed, false, false)
    }

    /// Made with `llvm-sys` and its pin.
    pub fn with_llvm(what: &str) -> Workspace {
        Workspace::made(what, false, true, false)
    }

    /// Made with the vendored trait solver, of the front end's closure ([`SOLVER_SOURCE`]).
    pub fn with_solver(what: &str) -> Workspace {
        Workspace::made(what, false, false, true)
    }

    fn made(what: &str, reversed: bool, llvm: bool, solver: bool) -> Workspace {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "blaze-codegen-code-{what}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("ws");
        std::fs::create_dir_all(&root).expect("a temporary directory");
        made_at(&root, reversed, CHECKSUM, llvm, solver);
        Workspace { dir, root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory above the workspace, the fixture's own.
    pub fn above(&self) -> &Path {
        &self.dir
    }

    /// Writes `text` to `path`, relative to the workspace.
    pub fn write(&self, path: &str, text: &str) {
        write(&self.root, path, text);
    }

    /// The workspace as the program `cli`'s build script reads it.
    pub fn read(&self) -> Result<crate::Workspace, String> {
        crate::Workspace::read(&self.root.join("crates/cli"))
    }

    pub fn work_out(&self) -> Result<Worked, String> {
        work_out(&self.read()?, &env())
    }

    /// The code identity of its code generator.
    pub fn code(&self) -> [u8; 32] {
        self.work_out().expect("the fixture's code identity").code
    }

    /// `identity` of its program.
    pub fn code_of(&self, identity: &Identity) -> [u8; 32] {
        work_out_of(&self.read().expect("the fixture's workspace"), identity, &env())
            .expect("the fixture's identity")
            .code
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// Writes `text` to `path` under `root`.
pub fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// The manifest of `outside`, relative to the workspace.
pub const OUTSIDE_MANIFEST: &str = "vendor/outside/Cargo.toml";

/// The manifest of the package `name`, with `deps` after its `[package]` table.
pub fn manifest(name: &str, deps: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n{deps}")
}

/// The lock file, with `reg`'s checksum, and `llvm-sys` if `llvm`.
pub fn lock(checksum: &str, llvm: bool) -> String {
    lock_of(checksum, llvm, false)
}

/// [`lock`], with the vendored trait solver if `solver`.
fn lock_of(checksum: &str, llvm: bool, solver: bool) -> String {
    let local = |name: &str, deps: &[&str]| {
        let deps: String = deps.iter().map(|it| format!(" \"{it}\",\n")).collect();
        let deps =
            if deps.is_empty() { String::new() } else { format!("dependencies = [\n{deps}]\n") };
        format!("[[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\n{deps}\n")
    };
    let registry_with = |name: &str, checksum: &str, deps: &[&str]| {
        let deps: String = deps.iter().map(|it| format!(" \"{it}\",\n")).collect();
        let deps =
            if deps.is_empty() { String::new() } else { format!("dependencies = [\n{deps}]\n") };
        format!(
            "[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\nsource = \"{REGISTRY}\"\nchecksum = \"{checksum}\"\n{deps}\n"
        )
    };
    let registry = |name: &str, checksum: &str| registry_with(name, checksum, &[]);
    let codegen: &[&str] = if llvm {
        &["blaze-stencil", "devonly", "front", "llvm-sys", "reg", "testonly"]
    } else {
        &["devonly", "front", "reg", "testonly"]
    };
    [
        "version = 4\n\n".to_owned(),
        local("blaze-codegen-code", &[]),
        local("blaze-item-hash", &["front"]),
        local("blaze-macro-eval", &["hir-ty"]),
        if llvm { local("blaze-stencil", &["llvm-sys"]) } else { String::new() },
        local("cli", &["blaze-item-hash", "feat", "hir-ty-codegen", "outside"]),
        local("devonly", &[]),
        registry_with("feat", FEAT_CHECKSUM, &["reg"]),
        local("front", &["blaze-macro-eval", "hir-def"]),
        local("hir-def", if solver { &["rustc_next_trait_solver"][..] } else { &[] }),
        local("hir-ty", &["blaze-codegen-code", "hir-def"]),
        local("hir-ty-codegen", codegen),
        if llvm { local("llvm-sys", &[]) } else { String::new() },
        local("outside", &["hir-def"]),
        registry("reg", checksum),
        if solver { local("rustc_next_trait_solver", &[]) } else { String::new() },
        registry("testonly", "cc"),
    ]
    .concat()
}

/// The LLVM version and commit [`LLVM_JSON`] pins.
pub const LLVM_PIN: (&str, &str) = ("23.1.4", "e152d9fa2bc259b3f831769ba1e1b17cf71d45da");

/// The fixture's llvm.json: releases for aarch64-apple-darwin and x86_64-unknown-linux-gnu.
pub const LLVM_JSON: &str = r#"{
  "version": "23.1.4",
  "commit": "e152d9fa2bc259b3f831769ba1e1b17cf71d45da",
  "targets": {
    "aarch64-apple-darwin": {
      "name": "blaze-llvm-aarch64-apple-darwin.tar.xz",
      "sha256": "1111111111111111111111111111111111111111111111111111111111111111"
    },
    "x86_64-unknown-linux-gnu": {
      "name": "blaze-llvm-x86_64-unknown-linux-gnu.tar.xz",
      "sha256": "2222222222222222222222222222222222222222222222222222222222222222"
    }
  }
}
"#;

/// The fixture's stencils.json: two libraries.
pub const STENCILS_JSON: &str = r#"{
  "x86_64.stencils": "4444444444444444444444444444444444444444444444444444444444444444",
  "aarch64.stencils": "5555555555555555555555555555555555555555555555555555555555555555"
}
"#;

/// A source file of the fixture's vendored trait solver ([`Workspace::with_solver`]) and its text.
pub const SOLVER_SOURCE: (&str, &str) =
    ("vendor/rustc_next_trait_solver/src/lib.rs", "pub fn solve() {}\n");

/// The front end's code identity ([`crate::FRONT_END`]) of a workspace with the vendored trait
/// solver ([`Workspace::with_solver`]), before and after an edit of the solver's source
/// ([`SOLVER_SOURCE`]): what the goal and item stores' key tests (hir-ty) install as `build`,
/// the identity blaze-engine's build script works out and its main hands over.
pub fn front_end_around_a_solver_edit() -> ([u8; 32], [u8; 32]) {
    let workspace = Workspace::with_solver("solver-edit");
    let before = workspace.code_of(&crate::FRONT_END);
    let (source, text) = SOLVER_SOURCE;
    workspace.write(source, &format!("{text}pub fn another_rule() {{}}\n"));
    (before, workspace.code_of(&crate::FRONT_END))
}

/// The workspace's files.
fn files(checksum: &str, llvm: bool, solver: bool) -> Vec<(&'static str, String)> {
    let llvm_sys = if llvm { "llvm-sys = { path = \"../llvm-sys\" }\n" } else { "" };
    let stencil = if llvm { "blaze-stencil = { path = \"../blaze-stencil\" }\n" } else { "" };
    let mut files = vec![
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/excluded\"]\n".to_owned(),
        ),
        ("Cargo.lock", lock_of(checksum, llvm, solver)),
        ("harness/toolchain", "nightly-2026-11-01\n".to_owned()),
        (
            "crates/hir-ty-codegen/Cargo.toml",
            manifest(
                "hir-ty-codegen",
                &format!(
                    "[dependencies]\nfront = {{ path = \"../front\" }}\n{stencil}\n[build-dependencies]\nreg = \"1\"\n{llvm_sys}\n\
                     [dev-dependencies]\ntestonly = \"1\"\ndevonly = {{ path = \"../devonly\" }}\n"
                ),
            ),
        ),
        ("crates/hir-ty-codegen/src/lib.rs", "pub fn compile() {}\n".to_owned()),
        (
            "crates/front/Cargo.toml",
            manifest(
                "front",
                "[dependencies]\nhir-def = { path = \"../hir-def\" }\nblaze-macro-eval = { path = \"../blaze-macro-eval\" }\n",
            ),
        ),
        (
            "crates/hir-def/Cargo.toml",
            manifest(
                "hir-def",
                if solver {
                    "[dependencies]\nrustc_next_trait_solver = { path = \"../../vendor/rustc_next_trait_solver\" }\n"
                } else {
                    ""
                },
            ),
        ),
        (
            "crates/hir-ty/Cargo.toml",
            manifest(
                "hir-ty",
                "[dependencies]\nhir-def = { path = \"../hir-def\" }\n\n\
                 [build-dependencies]\nblaze-codegen-code = { path = \"../blaze-codegen-code\" }\n",
            ),
        ),
        ("crates/hir-ty/src/lib.rs", "pub fn infer() {}\n".to_owned()),
        ("crates/blaze-codegen-code/Cargo.toml", manifest("blaze-codegen-code", "")),
        ("crates/blaze-codegen-code/src/lib.rs", "pub fn emit() {}\n".to_owned()),
        (
            "crates/blaze-macro-eval/Cargo.toml",
            manifest("blaze-macro-eval", "[dependencies]\nhir-ty = { path = \"../hir-ty\" }\n"),
        ),
        ("crates/blaze-macro-eval/src/lib.rs", "pub fn expand() {}\n".to_owned()),
        // What `FRONT_END_EXTRA` names outside every package.
        ("vendor/llvm-sys/pin.rs", "pub fn pin() {}\n".to_owned()),
        ("crates/blaze-stencil/pin.rs", "pub fn pin() {}\n".to_owned()),
        ("library/proc_macro/src/bridge/mod.rs", "pub mod client;\n".to_owned()),
        ("library/proc_macro/src/bridge/client.rs", "pub fn run() {}\n".to_owned()),
        // No member (the workspace's are `crates/*`): a package of the checkout only `cli` names.
        (
            OUTSIDE_MANIFEST,
            manifest("outside", "[dependencies]\nhir-def = { path = \"../../crates/hir-def\" }\n"),
        ),
        ("vendor/outside/src/lib.rs", "pub fn outside() {}\n".to_owned()),
        ("crates/hir-def/src/lib.rs", "pub fn resolve() {}\n".to_owned()),
        ("crates/front/src/lib.rs", "pub fn lower() {}\n".to_owned()),
        ("crates/front/tests/it.rs", "#[test] fn it() {}\n".to_owned()),
        ("crates/devonly/Cargo.toml", manifest("devonly", "")),
        ("crates/devonly/src/lib.rs", "pub fn helps() {}\n".to_owned()),
        (
            "crates/blaze-item-hash/Cargo.toml",
            manifest("blaze-item-hash", "[dependencies]\nfront = { path = \"../front\" }\n"),
        ),
        ("crates/blaze-item-hash/src/lib.rs", "pub fn facts() {}\n".to_owned()),
        (
            "crates/cli/Cargo.toml",
            manifest(
                "cli",
                "[dependencies]\nhir-ty-codegen = { path = \"../hir-ty-codegen\" }\n\
                 blaze-item-hash = { path = \"../blaze-item-hash\" }\nfeat = \"1\"\n\
                 outside = { path = \"../../vendor/outside\" }\n",
            ),
        ),
        ("crates/cli/src/main.rs", "fn main() {}\n".to_owned()),
        // Excluded from the workspace: no member, and nothing it says is read.
        ("crates/excluded/Cargo.toml", "this is no TOML\n".to_owned()),
    ];
    if solver {
        files.push((
            "vendor/rustc_next_trait_solver/Cargo.toml",
            manifest("rustc_next_trait_solver", ""),
        ));
        files.push((SOLVER_SOURCE.0, SOLVER_SOURCE.1.to_owned()));
        files.push((
            "vendor/rustc_next_trait_solver/PATCHES.md",
            "# Patches\n\n0001-one.patch\n".to_owned(),
        ));
        files.push((
            "vendor/rustc_next_trait_solver/patches/0001-one.patch",
            "--- a\n+++ b\n".to_owned(),
        ));
        files.push((
            "vendor/rustc_next_trait_solver/tests/it.rs",
            "#[test] fn it() {}\n".to_owned(),
        ));
    }
    if llvm {
        files.push(("crates/llvm-sys/Cargo.toml", manifest("llvm-sys", "")));
        files.push(("crates/llvm-sys/src/lib.rs", "pub fn link() {}\n".to_owned()));
        files.push(("crates/hir-ty-codegen/llvm.json", LLVM_JSON.to_owned()));
        files.push((
            "crates/blaze-stencil/Cargo.toml",
            manifest(
                "blaze-stencil",
                "[build-dependencies]\nllvm-sys = { path = \"../llvm-sys\" }\n",
            ),
        ));
        files.push(("crates/blaze-stencil/src/lib.rs", "pub fn patch() {}\n".to_owned()));
        files.push(("crates/blaze-stencil/stencils.json", STENCILS_JSON.to_owned()));
    }
    files
}

/// Makes the workspace at `root`, its files in order or in the reverse order.
pub fn workspace(root: &Path, reversed: bool, checksum: &str, llvm: bool) {
    made_at(root, reversed, checksum, llvm, false);
}

/// [`workspace`], with the vendored trait solver if `solver`.
fn made_at(root: &Path, reversed: bool, checksum: &str, llvm: bool, solver: bool) {
    let mut files = files(checksum, llvm, solver);
    if reversed {
        files.reverse();
    }
    for (path, text) in files {
        write(root, path, &text);
    }
}

/// A release build's environment, with a variable that is no input.
pub fn env() -> BuildEnv {
    let env: BTreeMap<String, String> = [
        ("TARGET", "aarch64-apple-darwin"),
        ("HOST", "aarch64-apple-darwin"),
        ("PROFILE", "release"),
        ("HOME", "/somewhere"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let mut env = BuildEnv::of(&env);
    env.compiler =
        Some("rustc 1.101.0-nightly (0000000 2026-11-01)\nhost: aarch64-apple-darwin\n".to_owned());
    env
}
