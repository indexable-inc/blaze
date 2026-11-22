//! The files a proc macro called in a crate is shown: the one home of "a crate's sandbox files
//! from its place", for every caller (blaze-world's `names` example, blaze-real's `names_load`,
//! and the command line's expansion task once it runs proc macros).
//!
//! THE RULE: a macro called in crate C sees C's own package directory and manifest, at the
//! sandbox's one stable path ([`blaze_proc_macro::sandbox::PACKAGE`], `/package`, which
//! `CARGO_MANIFEST_DIR` names), whoever C is: a workspace member, a registry package or a git
//! package; never the workspace's root and never another package's folder.
//!
//! Why it matters: cargo documents `CARGO_MANIFEST_DIR` as "The directory containing the
//! manifest of your package". proc-macro-crate's `crate_name` (jsonrpsee's `#[rpc(server)]`,
//! through `find_jsonrpsee_crate`) reads `$CARGO_MANIFEST_DIR/Cargo.toml` and looks only in its
//! `[dependencies]` tables. Shown a virtual workspace root's manifest (tempo's: `[workspace]`
//! only), the lookup fails, the macro emits `compile_error!`, and `EthApiServer` (with reth's
//! other `*ApiServer` traits) is never made: census 13's 87 `ItemKey(e97737e3dcaa)` lines.

use std::path::Path;
use std::string::{String, ToString};
use std::sync::Arc;
use std::vec::Vec;

use blaze_fetch::PackView;
use blaze_proc_macro::sandbox::Files;

/// Where one crate's package is, as the caller already knows it from loading the crate's own
/// files (the same place its sources are read from).
#[derive(Clone, Copy)]
pub enum PackageAt<'a> {
    /// A workspace member: its folder on disk (the folder holding its `Cargo.toml`).
    Disk(&'a Path),
    /// A fetched package: its fetch record, and the package's folder inside it (`""` for a
    /// registry crate, whose record is its package; a git package's folder in its commit).
    /// The folder may be written with or without a trailing `/`.
    Pack { view: PackView<'static>, dir: &'a str },
    /// A toolchain crate (core, alloc, std, proc_macro): it calls no proc macro of a package,
    /// so it is shown no files.
    Toolchain,
}

/// The sandbox files of the package at `at`: a member's own folder, a fetched package's files
/// read in place from ITS fetch record (by their path from the package's folder, in the
/// record's order, which is sorted by path with each path once: blaze-fetch's pack form, so
/// stripping one common folder keeps both), a toolchain crate's none.
///
/// The rule this function is the home of: a macro called in crate C sees C's own package
/// directory and manifest, as the stable `/package` path, whoever C is.
pub fn sandbox_files(at: PackageAt<'_>) -> Files {
    match at {
        PackageAt::Disk(dir) => Files::Folder(dir.to_path_buf()),
        PackageAt::Pack { view, dir } => {
            let dir = dir.trim_matches('/');
            let mut prefix = String::with_capacity(dir.len() + 1);
            if !dir.is_empty() {
                prefix.push_str(dir);
                prefix.push('/');
            }
            let listed: Vec<(String, &'static [u8])> =
                view.files().filter_map(|file| file.path.strip_prefix(prefix.as_str()).map(|rel| (rel.to_string(), file.bytes))).collect();
            Files::Listed(Arc::from(listed))
        }
        PackageAt::Toolchain => Files::Listed(Arc::from(Vec::new())),
    }
}

#[cfg(test)]
mod tests {
    //! A dependency crate and a member crate each get their own directory and manifest. The
    //! oracle (`shown_manifest`) reads `Cargo.toml` from what a macro would be shown, as
    //! proc-macro-crate reads `$CARGO_MANIFEST_DIR/Cargo.toml`; a planted copy of the old
    //! `names_load::procs_of` mapping (every crate that is not a member shown the workspace's
    //! root) must fail it.
    //!
    //! Expected line (`cargo test -p blaze-world --features procs --lib procs::files`):
    //! `test procs::files::tests::each_crate_is_shown_its_own_package ... ok`

    use std::path::{Path, PathBuf};
    use std::string::String;
    use std::vec::Vec;
    use std::{format, vec};

    use blaze_fetch::PackView;
    use blaze_proc_macro::sandbox::Files;

    use super::{PackageAt, sandbox_files};
    use crate::fixture::pack;

    /// tempo's root manifest's shape: a virtual workspace, jsonrpsee only in
    /// `[workspace.dependencies]`.
    const ROOT_MANIFEST: &str = "[workspace]\nmembers = [\"crates/node\"]\n\n[workspace.dependencies]\njsonrpsee = \"0.26\"\n";
    /// reth-rpc-eth-api's manifest's shape: jsonrpsee in its own `[dependencies]`.
    const DEP_MANIFEST: &str = "[package]\nname = \"reth-rpc-eth-api\"\n\n[dependencies]\njsonrpsee = { version = \"0.26\", features = [\"server\", \"macros\"] }\n";
    /// A sibling package in the same git commit: never shown to the dependency's macros.
    const SIBLING_MANIFEST: &str = "[package]\nname = \"reth-rpc-api\"\n";

    /// What the macro sees as `$CARGO_MANIFEST_DIR/Cargo.toml`: a listed record's file, or a
    /// folder's (read from disk only to compare paths here: the test names the folder).
    enum Shown {
        Listed(Option<Vec<u8>>),
        Folder(PathBuf),
    }

    fn shown_manifest(files: &Files) -> Shown {
        match files {
            Files::Listed(files) => Shown::Listed(
                files.binary_search_by(|(path, _)| path.as_str().cmp("Cargo.toml")).ok().map(|at| files[at].1.to_vec()),
            ),
            Files::Folder(dir) => Shown::Folder(dir.clone()),
            // D96 (3): a member's manifest is at its dir in its workspace.
            Files::Member { workspace, root } => Shown::Folder(workspace.join(root.seen().folder())),
        }
    }

    /// The old mapping, planted: a member's folder, else the workspace's root (`root.join("")`).
    fn planted(at: PackageAt<'_>, root: &Path) -> Files {
        match at {
            PackageAt::Disk(dir) => Files::Folder(dir.to_path_buf()),
            PackageAt::Pack { .. } | PackageAt::Toolchain => Files::Folder(root.join("")),
        }
    }

    /// Whether `files` show the package its caller named: the member's own folder, or the
    /// dependency's own manifest and nothing of its sibling.
    fn own_package(files: &Files, want: &Want<'_>) -> Result<(), String> {
        match (shown_manifest(files), want) {
            (Shown::Folder(dir), Want::Folder(member)) if dir == *member => Ok(()),
            (Shown::Listed(Some(bytes)), Want::Manifest(text)) if bytes == text.as_bytes() => {
                let Files::Listed(listed) = files else { return Err(String::from("not a listing")) };
                match listed.iter().find(|(path, _)| path.contains("rpc-api") || path.starts_with("crates/")) {
                    Some((path, _)) => Err(format!("a file of another package is shown: {path}")),
                    None => Ok(()),
                }
            }
            (Shown::Folder(dir), _) => Err(format!("shown the folder {}", dir.display())),
            (Shown::Listed(Some(bytes)), _) => Err(format!("shown another manifest: {}", String::from_utf8_lossy(&bytes))),
            (Shown::Listed(None), _) => Err(String::from("shown no Cargo.toml")),
        }
    }

    enum Want<'a> {
        Folder(&'a Path),
        Manifest(&'a str),
    }

    /// Cargo's book, "Environment variables Cargo sets for crates": `CARGO_MANIFEST_DIR` is
    /// "The directory containing the manifest of your package". A git record holds several
    /// packages: the one asked for is shown alone, its folder written either way.
    #[test]
    fn each_crate_is_shown_its_own_package() {
        let root = PathBuf::from("/checkout/tempo");
        let member = root.join("crates/node");
        let git = pack(&[
            ("Cargo.toml", ROOT_MANIFEST),
            ("crates/rpc/rpc-api/Cargo.toml", SIBLING_MANIFEST),
            ("crates/rpc/rpc-api/src/lib.rs", "pub trait Web3Api {}"),
            ("crates/rpc/rpc-eth-api/Cargo.toml", DEP_MANIFEST),
            ("crates/rpc/rpc-eth-api/src/core.rs", "pub trait EthApi {}"),
            ("crates/rpc/rpc-eth-api/src/lib.rs", "mod core;"),
        ], blaze_own::testing::heap().alloc());
        let git: &'static [u8] = std::boxed::Box::leak(git.into_boxed_slice());
        let git = PackView::open_verified(git).expect("a pack");
        let registry = pack(&[("Cargo.toml", DEP_MANIFEST), ("src/lib.rs", "")], blaze_own::testing::heap().alloc());
        let registry: &'static [u8] = std::boxed::Box::leak(registry.into_boxed_slice());
        let registry = PackView::open_verified(registry).expect("a pack");

        let cases: Vec<(&str, PackageAt<'_>, Want<'_>)> = vec![
            ("member", PackageAt::Disk(&member), Want::Folder(&member)),
            ("git dependency", PackageAt::Pack { view: git, dir: "crates/rpc/rpc-eth-api" }, Want::Manifest(DEP_MANIFEST)),
            ("git dependency, folder with `/`", PackageAt::Pack { view: git, dir: "crates/rpc/rpc-eth-api/" }, Want::Manifest(DEP_MANIFEST)),
            ("registry dependency", PackageAt::Pack { view: registry, dir: "" }, Want::Manifest(DEP_MANIFEST)),
        ];
        for (what, at, want) in &cases {
            own_package(&sandbox_files(*at), want).unwrap_or_else(|why| panic!("{what}: {why}"));
        }
        // The git dependency's listing is its own files, by path from its folder, in order.
        let Files::Listed(listed) = sandbox_files(cases[1].1) else { panic!("a git dependency is not a listing") };
        let paths: Vec<&str> = listed.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["Cargo.toml", "src/core.rs", "src/lib.rs"]);
        // A toolchain crate is shown nothing.
        let Files::Listed(none) = sandbox_files(PackageAt::Toolchain) else { panic!("a toolchain crate is not a listing") };
        assert!(none.is_empty(), "a toolchain crate is shown {} files", none.len());

        // The planted bug: every crate that is not a member shown the workspace's root. The
        // member still passes; each dependency must fail.
        for (what, at, want) in &cases {
            let caught = own_package(&planted(*at, &root), want).is_err();
            match want {
                Want::Folder(_) => assert!(!caught, "{what}: the planted mapping keeps members right"),
                Want::Manifest(_) => assert!(caught, "{what}: the planted root-for-every-dependency mapping was not caught"),
            }
        }
    }
}
