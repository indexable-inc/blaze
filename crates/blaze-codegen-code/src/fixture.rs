//! A made-up workspace to work the identity out over, for tests here and in the code generator,
//! on disk with its manifests and lock file: the program `cli` depends on `hir-ty-codegen` and
//! `blaze-item-hash`; `hir-ty-codegen` depends on `front`, on the registry package `reg` as a
//! build dependency, and on the registry package `testonly` and the checkout package `devonly`
//! as dev-dependencies; `blaze-item-hash` depends on `front`. [`Workspace::with_llvm`] adds the
//! checkout package `llvm-sys` as a build dependency of `hir-ty-codegen`, and its pin
//! `crates/hir-ty-codegen/llvm.json`; and `blaze-stencil`, a dependency of `hir-ty-codegen` with
//! `llvm-sys` as its build dependency, and its pin `crates/blaze-stencil/stencils.json`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use crate::{BuildEnv, Worked, work_out};

pub const CHECKSUM: &str = "aa";

/// The workspace in `ws/` of a fresh directory of its own, removed when dropped.
pub struct Workspace {
    /// The directory of its own, above the workspace.
    dir: PathBuf,
    root: PathBuf,
}

impl Workspace {
    /// Made with its files in order, or in the reverse order.
    pub fn new(what: &str, reversed: bool) -> Workspace {
        Workspace::made(what, reversed, false)
    }

    /// Made with `llvm-sys` and its pin.
    pub fn with_llvm(what: &str) -> Workspace {
        Workspace::made(what, false, true)
    }

    fn made(what: &str, reversed: bool, llvm: bool) -> Workspace {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "blaze-codegen-code-{what}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("ws");
        std::fs::create_dir_all(&root).expect("a temporary directory");
        workspace(&root, reversed, CHECKSUM, llvm);
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

fn manifest(name: &str, deps: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n{deps}")
}

/// The lock file, with `reg`'s checksum, and `llvm-sys` if `llvm`.
pub fn lock(checksum: &str, llvm: bool) -> String {
    let local = |name: &str, deps: &[&str]| {
        let deps: String = deps.iter().map(|it| format!(" \"{it}\",\n")).collect();
        let deps = if deps.is_empty() { String::new() } else { format!("dependencies = [\n{deps}]\n") };
        format!("[[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\n{deps}\n")
    };
    let registry = |name: &str, checksum: &str| {
        format!("[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\nsource = \"{REGISTRY}\"\nchecksum = \"{checksum}\"\n\n")
    };
    let codegen: &[&str] = if llvm {
        &["blaze-stencil", "devonly", "front", "llvm-sys", "reg", "testonly"]
    } else {
        &["devonly", "front", "reg", "testonly"]
    };
    [
        "version = 4\n\n".to_owned(),
        local("blaze-item-hash", &["front"]),
        if llvm { local("blaze-stencil", &["llvm-sys"]) } else { String::new() },
        local("cli", &["blaze-item-hash", "hir-ty-codegen"]),
        local("devonly", &[]),
        local("front", &[]),
        local("hir-ty-codegen", codegen),
        if llvm { local("llvm-sys", &[]) } else { String::new() },
        registry("reg", checksum),
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

/// The workspace's files.
fn files(checksum: &str, llvm: bool) -> Vec<(&'static str, String)> {
    let llvm_sys = if llvm { "llvm-sys = { path = \"../llvm-sys\" }\n" } else { "" };
    let stencil = if llvm { "blaze-stencil = { path = \"../blaze-stencil\" }\n" } else { "" };
    let mut files = vec![
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/excluded\"]\n".to_owned()),
        ("Cargo.lock", lock(checksum, llvm)),
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
        ("crates/front/Cargo.toml", manifest("front", "")),
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
                 blaze-item-hash = { path = \"../blaze-item-hash\" }\n",
            ),
        ),
        ("crates/cli/src/main.rs", "fn main() {}\n".to_owned()),
        // Excluded from the workspace: no member, and nothing it says is read.
        ("crates/excluded/Cargo.toml", "this is no TOML\n".to_owned()),
    ];
    if llvm {
        files.push(("crates/llvm-sys/Cargo.toml", manifest("llvm-sys", "")));
        files.push(("crates/llvm-sys/src/lib.rs", "pub fn link() {}\n".to_owned()));
        files.push(("crates/hir-ty-codegen/llvm.json", LLVM_JSON.to_owned()));
        files.push((
            "crates/blaze-stencil/Cargo.toml",
            manifest("blaze-stencil", "[build-dependencies]\nllvm-sys = { path = \"../llvm-sys\" }\n"),
        ));
        files.push(("crates/blaze-stencil/src/lib.rs", "pub fn patch() {}\n".to_owned()));
        files.push(("crates/blaze-stencil/stencils.json", STENCILS_JSON.to_owned()));
    }
    files
}

/// Makes the workspace at `root`, its files in order or in the reverse order.
pub fn workspace(root: &Path, reversed: bool, checksum: &str, llvm: bool) {
    let mut files = files(checksum, llvm);
    if reversed {
        files.reverse();
    }
    for (path, text) in files {
        write(root, path, &text);
    }
}

/// A release build's environment, with a variable that is no input.
pub fn env() -> BuildEnv {
    let env: BTreeMap<String, String> =
        [("TARGET", "aarch64-apple-darwin"), ("HOST", "aarch64-apple-darwin"), ("PROFILE", "release"), ("HOME", "/somewhere")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let mut env = BuildEnv::of(&env);
    env.compiler = Some("rustc 1.101.0-nightly (0000000 2026-11-01)\nhost: aarch64-apple-darwin\n".to_owned());
    env
}
