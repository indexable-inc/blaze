//! What the tempo examples share: the module store read through identities, a package's
//! folder, a checkout's `.rs` files and a file's package.

use std::path::{Path, PathBuf};

use blaze_proc_macro::{
    module::{CrateId, LockedPackages, ModuleRecord, Source, files_digest, package_files, read_module},
    record::{DirStore, native_world},
    sandbox::{Files, Package},
};

/// The expansion records under the folder `store`, through a world this program opens over it.
pub fn expansion_records(store: &Path) -> DirStore {
    let (io, root) = native_world(store).unwrap_or_else(|error| panic!("the records at {}: {error}", store.display()));
    DirStore::expansions(io.spawner(), io.boot().ok(), &root)
}

/// The modules in the store, read through their identities: a key the crate computes
/// differently from the builder's reads as missing.
pub fn modules(store: &Path, checkout: &Path) -> Vec<ModuleRecord> {
    let mut out = Vec::new();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(store.join("proc-macro/modules")).expect("the module store").flatten().map(|it| it.path()).collect();
    dirs.sort();
    let lock_text = std::fs::read(checkout.join("Cargo.lock")).expect("the checkout's lock");
    let lock = blaze_manifest::lock::Lock::read(&lock_text, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the checkout's Cargo.lock: {e}"));
    let locked = LockedPackages::of(&lock);
    for dir in dirs {
        let identity = std::fs::read_to_string(dir.join("identity")).expect("a module's identity");
        let field = |name: &str| identity.lines().find_map(|line| line.strip_prefix(name).and_then(|it| it.strip_prefix(' '))).unwrap_or("").to_owned();
        let (name, version, checksum, git, files, features) =
            (field("name"), field("version"), field("checksum"), field("git"), field("files"), field("features"));
        let features: Vec<&str> = features.split(',').filter(|it| !it.is_empty()).collect();
        // A path package's digest is computed here from the checkout as it is, never taken from
        // the identity: an edit to the macro crate reads as a missing module.
        let digest = if files.is_empty() {
            String::new()
        } else {
            let dir = package_dir(checkout, &name).expect("the path package in the checkout");
            files_digest(&mut package_files(&dir).expect("the path package's files"))
        };
        let (commit, path) = git.split_once(' ').unwrap_or(("", ""));
        let source = if !files.is_empty() {
            Source::Path { files_sha256: &digest }
        } else if !git.is_empty() {
            Source::Git { commit, path }
        } else {
            Source::Registry { checksum: &checksum }
        };
        let deps = locked.closure(&name, &version).expect("the macro crate's closure in the lock");
        let id = CrateId { name: &name, version: &version, source, deps: &deps, features: &features };
        match read_module(store, &id) {
            // The identity of a record an older builder keyed otherwise now names another
            // folder of the store (visited in its own turn): this one is no module of today's.
            Ok(module) if Some(module.key.as_str()) != dir.file_name().and_then(|it| it.to_str()) => {
                println!("module {}: its identity is keyed {} today: skipped", dir.display(), module.key);
            }
            Ok(module) => out.push(module),
            Err(error) => println!("module {}: {error}", dir.display()),
        }
    }
    out
}

/// The folder of the package named `name` in the checkout: the one whose `Cargo.toml` says
/// `name = "<name>"` in its `[package]`.
pub fn package_dir(checkout: &Path, name: &str) -> Option<PathBuf> {
    let wanted = format!("name = {name:?}");
    let mut stack = vec![checkout.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let base = entry.file_name();
            if path.is_dir() {
                if base != "target" && base != ".git" {
                    stack.push(path);
                }
            } else if base == "Cargo.toml"
                && let Ok(text) = std::fs::read_to_string(&path)
                && text.split("\n[").next().is_some_and(|package| package.lines().any(|line| line.trim() == wanted))
            {
                return Some(dir);
            }
        }
    }
    None
}

/// Every `.rs` file under `root`, sorted, but in `target` and `.git`.
pub fn sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if name != "target" && name != ".git" {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|it| it == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The package of `file`: its nearest manifest's folder, from the checkout's root, and the
/// variables cargo gives its crate.
pub fn package_of(root: &Path, file: &Path) -> Package {
    let mut dir = file.parent();
    while let Some(at) = dir {
        if at.join("Cargo.toml").exists() || at == root {
            break;
        }
        dir = at.parent();
    }
    let dir = dir.unwrap_or(root);
    let folder = dir.strip_prefix(root).unwrap_or(Path::new("")).to_string_lossy().into_owned();
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
    let name = manifest
        .lines()
        .find_map(|line| line.trim().strip_prefix("name").map(|it| it.trim().trim_start_matches('=').trim().trim_matches('"').to_owned()))
        .unwrap_or_default();
    // A member (D96 (3)): its workspace served at `/workspace`, the member at its dir in it;
    // the path variable is the row's (src/sandbox/seen_env.rs).
    let files = Files::Member {
        workspace: root.to_path_buf(),
        root: blaze_proc_macro::script_sandbox::ScriptRoot::member(&folder).unwrap_or_else(|e| panic!("{}: {e}", dir.display())),
    };
    let mut env = vec![
        ("CARGO_MANIFEST_DIR".to_owned(), files.seen().to_string()),
        ("CARGO_PKG_NAME".to_owned(), name.clone()),
        ("CARGO_CRATE_NAME".to_owned(), name.replace('-', "_")),
    ];
    // The `[package]` keys cargo passes as `CARGO_PKG_*`, a `.workspace = true` one taken from
    // the root's `[workspace.package]`.
    let root_manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    let table = |text: &str, header: &str, key: &str| -> Option<String> {
        let body = text.split(&format!("\n{header}\n")).nth(1).or_else(|| text.strip_prefix(&format!("{header}\n")))?;
        let body = body.split("\n[").next()?;
        body.lines().find_map(|line| {
            let (k, v) = line.split_once('=')?;
            (k.trim() == key).then(|| v.trim().to_owned())
        })
    };
    for (key, var) in [
        ("version", "CARGO_PKG_VERSION"),
        ("authors", "CARGO_PKG_AUTHORS"),
        ("description", "CARGO_PKG_DESCRIPTION"),
        ("homepage", "CARGO_PKG_HOMEPAGE"),
        ("repository", "CARGO_PKG_REPOSITORY"),
        ("license", "CARGO_PKG_LICENSE"),
        ("rust-version", "CARGO_PKG_RUST_VERSION"),
    ] {
        let own = table(&manifest, "[package]", key);
        let inherited = table(&manifest, "[package]", &format!("{key}.workspace")).is_some();
        let raw = if inherited { table(&root_manifest, "[workspace.package]", key) } else { own };
        let value = match raw {
            // `authors` is a list: cargo joins it with `:`.
            Some(list) if list.starts_with('[') => {
                list.trim_matches(|c| c == '[' || c == ']').split(',').map(|it| it.trim().trim_matches('"')).filter(|it| !it.is_empty()).collect::<Vec<_>>().join(":")
            }
            Some(text) => text.trim_matches('"').to_owned(),
            None => String::new(),
        };
        env.push((var.to_owned(), value));
    }
    let env = blaze_proc_macro::package_env::PackageEnv::set_only(env).unwrap_or_else(|e| panic!("the variables of `{folder}`: {e}"));
    Package::new(files, env).unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
}
