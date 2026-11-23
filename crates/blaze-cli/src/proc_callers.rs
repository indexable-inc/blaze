//! Where each locked package's files are, as the names world placed them: the ONE answer the
//! proc-macro runner (proc_runner.rs) shows a macro of, for the crate a call is written in
//! (piece pm-call-expand-deps, 2026-11-16; rules 9, 21, 30).
//!
//! Why it is here: the runner gave a package only to checkout crates (`/0/..`), so a
//! fn-like, attribute or derive proc-macro call written in a crate of a FETCHED package
//! (tempo's dependencies: `sol!` in alloy's crates, serde's derives, thiserror's, clap's) was
//! refused (`HostWhy::Refused`) before the macro ran. The fetched package's files are already
//! reached by one path: blaze-world's `Fetched::place` over blaze-unit-graph's `StoreSources`,
//! a registry or git package's fetch record mapped in place (`Place::Pack`), a path package
//! its checkout folder (`Place::Checkout`). [`Placing`] wraps that path while the names world
//! is made and keeps each answer; nothing opens a second view of the store.
//!
//! Lifetimes: the names world is `World<'f>`, its pack views `PackView<'f>`, but the sandbox
//! serves a fetched package's files as `&'static [u8]` (blaze-proc-macro `Files::Listed`, lent
//! to the Wasmtime worker). The records' bytes ARE held to the end of the command (names.rs
//! `Folder` reads them into the names frame, which is leaked), so names.rs types its
//! `StoreSources` and `Fetched` at `'static` and hands the world this wrapper, which gives the
//! same `Place` at the world's shorter `'f` (a `Place` is covariant) and keeps the `'static` one.
//!
//! What the macro sees stays what blaze-world's `procs::files::sandbox_files` gives (its one
//! home): the calling crate's own package, its files read in place from its record, at the
//! sandbox's `/package`; nothing else (rule 9).

use blaze_manifest::lock::LockId;
use blaze_own::{Alloc, Vec};
use blaze_world::{Packages, Place, WorldError};

/// The places answered while the names world was made: each `(package, place)`, sorted by
/// package (a package placed twice answers the same place: `StoreSources` keeps each view).
pub(crate) struct Placed<'a> {
    by_package: Vec<(LockId, Place<'static>), Alloc<'a>>,
}

impl Placed<'static> {
    /// Leaks the sorted package places onto the static heap slice so worker threads can share it.
    pub(crate) fn leak_slice(self) -> &'static [(LockId, Place<'static>)] {
        self.by_package.leak_static()
    }
}

/// Looks up `package` in a sorted slice produced by [`Placed::leak_slice`].
pub(crate) fn placed_of(placed: &[(LockId, Place<'static>)], package: LockId) -> Option<Place<'static>> {
    let at = placed.partition_point(|(id, _)| *id < package);
    placed.get(at).filter(|(id, _)| *id == package).map(|(_, place)| *place)
}

impl<'a> Placed<'a> {
    /// Where the package `package` is, as the world was told; `None`: the world never placed
    /// it (no unit of it was listed), which the runner refuses by the crate's name.
    pub(crate) fn of(&self, package: LockId) -> Option<Place<'static>> {
        placed_of(&self.by_package, package)
    }
}

/// The world's `Packages` while it is made: every answer of `inner` passed on and kept.
pub(crate) struct Placing<'p, 'a, P> {
    inner: &'p mut P,
    seen: Vec<(LockId, Place<'static>), Alloc<'a>>,
}

impl<'p, 'a, P: Packages<'static>> Placing<'p, 'a, P> {
    pub(crate) fn new(inner: &'p mut P, alloc: Alloc<'a>) -> Placing<'p, 'a, P> {
        Placing { inner, seen: Vec::new_in(alloc) }
    }

    /// The places kept, sorted by package (stable: the first answer of a package first).
    pub(crate) fn finish(self) -> Placed<'a> {
        let mut by_package = self.seen;
        by_package.sort_by_key(|(id, _)| *id);
        Placed { by_package }
    }
}

impl<'f, P: Packages<'static>> Packages<'f> for Placing<'_, '_, P> {
    fn place(&mut self, package: LockId) -> Result<Place<'f>, WorldError<'f>> {
        let place = match self.inner.place(package) {
            Ok(place) => place,
            Err(error) => return Err(error),
        };
        self.seen.push((package, place));
        Ok(place)
    }
}

/// What a proc macro called in a crate whose package the world placed at `place` is shown:
/// a MEMBER's workspace (the checkout at `checkout`) mounted at `/workspace` with the member
/// at its dir in it (decision D96, the same `ScriptRoot` build scripts get, scripts.rs
/// `tree_of`), a fetched package's record at `/package` (blaze-world procs/files.rs). The
/// files' `seen()` is the one value both the mount and `CARGO_MANIFEST_DIR` are read from.
/// `Err`: a member dir no sandbox can mount, named (D96).
#[cfg(feature = "sandbox")]
pub(crate) fn shown(
    place: Place<'static>,
    checkout: &str,
) -> Result<blaze_proc_macro::sandbox::Files, blaze_expand::seen::MemberDirError<'static>> {
    use blaze_world::procs::files::{PackageAt, sandbox_files};
    match place {
        Place::Checkout { dir } => Ok(blaze_proc_macro::sandbox::Files::Member {
            workspace: std::path::PathBuf::from(checkout),
            root: blaze_proc_macro::script_sandbox::ScriptRoot::member(dir)?,
        }),
        Place::Path { dir } => {
            // Validate `dir` by the same rule (no `..` or absolute path), then mount that
            // package's folder alone at `/package` (`Files::Folder` has `SeenRoot::Package`).
            let _ = blaze_proc_macro::script_sandbox::ScriptRoot::member(dir)?;
            let base = if checkout.is_empty() { std::path::Path::new(".") } else { std::path::Path::new(checkout) };
            let folder = if dir.is_empty() { base.to_path_buf() } else { base.join(dir) };
            Ok(blaze_proc_macro::sandbox::Files::Folder(folder))
        }
        Place::Pack { view, dir } => Ok(sandbox_files(PackageAt::Pack { view, dir })),
    }
}

#[cfg(test)]
mod tests {
    //! A crate of a fetched registry package is shown its own record's files, served at the
    //! sandbox's `/package`. Rust's rule: the Reference, "Procedural Macros": a proc macro is
    //! "run during compilation" on the crate that invokes it; Cargo's "Environment variables
    //! Cargo sets for crates": `CARGO_MANIFEST_DIR` is "the directory containing the manifest
    //! of your package", the package the call is written in (alloy's `sol!` reads its JSON ABI
    //! files from under it, which is why `sol!` in a registry crate needs them).
    //!
    //! The oracle: through the wrapper, at a world's shorter lifetime, the fetched package's
    //! place is kept, and `sandbox_files` of it lists the record's `Cargo.toml` and
    //! `abi/Token.json` with their bytes. The control is a planted copy of the runner's old
    //! caller rule (a package only for a `shown_from` under `/0/`): it gives the same crate
    //! nothing, the refusal this piece removes.
    //!
    //! Expected (`lanecheck.sh blaze-cli test proc_callers`):
    //! `test proc_callers::tests::a_fetched_crate_is_shown_its_own_record ... ok`

    use blaze_fetch::Mode;
    use blaze_fetch::SourceId;
    use blaze_fetch::pack::{Builder, Kind, Limits, PackView};
    use blaze_manifest::lock::LockId;
    use blaze_proc_macro::sandbox::Files;
    use blaze_world::procs::files::{PackageAt, sandbox_files};
    use blaze_world::{Packages, Place, WorldError};

    use super::Placing;

    const MANIFEST: &str = "[package]\nname = \"tok\"\nversion = \"1.0.0\"\nedition = \"2021\"\n";
    const ABI: &str = "[{\"type\":\"function\",\"name\":\"transfer\"}]";
    const LIB: &str = "alloy_sol_types::sol!(Token, \"abi/Token.json\");\n";
    /// The archive's SHA-256 the lock would state.
    const SHA: [u8; 32] = [9; 32];

    /// The fetch store as blaze-world's `Fetched` answers it: package 0 a member at `app`,
    /// package 1 the registry crate `tok 1.0.0`, its record's files at the record's root.
    struct Store {
        view: PackView<'static>,
    }

    impl Packages<'static> for Store {
        fn place(&mut self, package: LockId) -> Result<Place<'static>, WorldError<'static>> {
            match package.0 {
                0 => Ok(Place::Checkout { dir: "app" }),
                1 => Ok(Place::Pack { view: self.view, dir: "" }),
                _ => Err(WorldError::PathNotPlaced { package: "unknown" }),
            }
        }
    }

    /// A world's ask, at its own lifetime `'f` (shorter than the store's).
    fn ask<'f, P: Packages<'f>>(packages: &mut P, package: LockId) -> Place<'f> {
        match packages.place(package) {
            Ok(place) => place,
            Err(error) => panic!("package {}: {error}", package.0),
        }
    }

    /// The runner's caller rule before this piece (proc_runner.rs `make`): a package only for
    /// a crate placed under the checkout.
    fn old_rule_has_package(shown_from: &str) -> bool {
        shown_from.starts_with("/0/") || shown_from == "/0"
    }

    #[test]
    fn a_fetched_crate_is_shown_its_own_record() {
        let alloc = blaze_own::testing::heap().alloc();
        let limits = Limits { most_files: 16, most_bytes: 1 << 16, most_path: 128 };
        let mut builder = Builder::new(Kind::Crate, SHA, limits, 0, alloc).expect("a builder");
        for (path, text) in [("Cargo.toml", MANIFEST), ("abi/Token.json", ABI), ("src/lib.rs", LIB)] {
            builder.file(path, Mode::File, text.as_bytes()).expect("a file");
        }
        let bytes: &'static [u8] = std::boxed::Box::leak(builder.finish().expect("a pack").to_vec().into_boxed_slice());
        let source = SourceId::Crate { name: "tok", version: "1.0.0", sha256: SHA };
        let view = PackView::open_source_put(&source, bytes).expect("the record is the crate's");
        let mut store = Store { view };
        let mut placing = Placing::new(&mut store, alloc);
        // The world asks in its own order, a package more than once.
        {
            let _member = ask(&mut placing, LockId(0));
            let _fetched = ask(&mut placing, LockId(1));
            let _again = ask(&mut placing, LockId(1));
        }
        let placed = placing.finish();
        assert!(matches!(placed.of(LockId(0)), Some(Place::Checkout { dir: "app" })), "the member's place was not kept");
        assert!(placed.of(LockId(2)).is_none(), "a package the world never placed has a place");
        let Some(Place::Pack { view, dir }) = placed.of(LockId(1)) else { panic!("the fetched package's place was not kept") };
        let Files::Listed(files) = sandbox_files(PackageAt::Pack { view, dir }) else { panic!("a fetched package is not served from its record") };
        let shown: std::vec::Vec<(&str, &[u8])> = files.iter().map(|(path, bytes)| (path.as_str(), *bytes)).collect();
        assert_eq!(shown, [("Cargo.toml", MANIFEST.as_bytes()), ("abi/Token.json", ABI.as_bytes()), ("src/lib.rs", LIB.as_bytes())], "the macro is shown its own package's files");
        // The control: the old rule had no package for this crate (tree 1, the first fetched
        // package's tree), so its calls were refused before the macro ran.
        assert!(!old_rule_has_package("/1/"), "the planted old rule showed a fetched crate a package");
        println!("fetched crate shown {} files of its own record", shown.len());
    }

    /// D96: a member's proc macro sees its workspace at `/workspace` with the member at its dir,
    /// and `CARGO_MANIFEST_DIR` names that same folder (both read from one `seen()`); a fetched
    /// package stays at `/package`. Cargo's "Environment variables Cargo sets for crates":
    /// `CARGO_MANIFEST_DIR` is "the directory containing the manifest of your package".
    /// Control: the old member arm (`PackageAt::Disk`, `Files::Folder`) mounts the member at
    /// `/package`, which is not `/workspace/crates/app`.
    ///
    /// Expected (`lanecheck.sh blaze-cli test proc_callers`):
    /// `test proc_callers::tests::a_member_s_macro_sees_its_workspace_mount ... ok`, printing
    /// "member macro sees /workspace/crates/app".
    #[cfg(feature = "sandbox")]
    #[test]
    fn a_member_s_macro_sees_its_workspace_mount() {
        use std::string::ToString;

        let member = super::shown(Place::Checkout { dir: "crates/app" }, "/ws").expect("a plain member dir");
        let seen = member.seen();
        assert_eq!(seen.mount(), "/workspace", "a member is mounted at the workspace's mount");
        assert_eq!(seen.folder(), "crates/app", "the member's folder in the mount");
        assert_eq!(seen.to_string(), "/workspace/crates/app", "CARGO_MANIFEST_DIR is the mounted folder");
        let Files::Member { workspace, .. } = &member else { panic!("a member is not served as its workspace: {member:?}") };
        assert_eq!(workspace.as_path(), std::path::Path::new("/ws"), "the workspace served is the checkout");
        let root = super::shown(Place::Checkout { dir: "" }, "/ws").expect("the root package");
        assert_eq!(root.seen().to_string(), "/workspace", "the root package is the workspace's folder");
        assert!(super::shown(Place::Checkout { dir: "../out" }, "/ws").is_err(), "a dir above the workspace was mounted");
        // The control: the old arm.
        let old = sandbox_files(PackageAt::Disk(std::path::Path::new("/ws/crates/app")));
        assert_ne!(old.seen().to_string(), seen.to_string(), "the planted old arm mounted the member where D96 does");
        println!("member macro sees {seen}");
    }

    /// MX (2026-11-17): a member at a non-empty dir of a checkout the command names `""` (blaze
    /// started in the workspace's root, `FetchEnv::checkout`) is entered and read by its macro,
    /// as the shim does at each call (`chdir` into `CARGO_MANIFEST_DIR`, asked as
    /// `path_filestat_get(root fd, "workspace/<dir>")`): before the fix every call was refused
    /// `Limit(Paths)` (port27/mini/serde, `crates/app`; tempo's 321 call groups). The member here
    /// is `src`: cargo runs this test in crates/blaze-cli, so `""` is that folder. CONTROLS: the
    /// member's macro naming `/workspace/../x` is refused by that name; a fetched package's
    /// macro cannot climb out of `/package`.
    ///
    /// Expected (`runner.py ask MX --test "-p blaze-cli --lib -- proc_callers"`):
    /// `test proc_callers::tests::a_member_of_a_checkout_started_in_is_entered ... ok`.
    #[cfg(feature = "sandbox")]
    #[test]
    fn a_member_of_a_checkout_started_in_is_entered() {
        use std::sync::Arc;

        use blaze_proc_macro::package_env::PackageEnv;
        use blaze_proc_macro::sandbox::{Package, Sandbox};
        use blaze_wasi::{Filetype, ROOT_FD, System};

        let files = super::shown(Place::Checkout { dir: "src" }, "").expect("a plain member dir");
        let package = Package::new(files, PackageEnv::default()).unwrap_or_else(|e| panic!("{e}"));
        let mut sandbox = Sandbox::new(Arc::new(package));
        let cwd = sandbox.package().cwd();
        assert_eq!(cwd, "/workspace/src");
        let entered = sandbox.path_stat(ROOT_FD, cwd.trim_start_matches('/').as_bytes(), true);
        assert!(matches!(&entered, Ok(Ok(stat)) if stat.filetype == Filetype::Directory), "the shim's chdir: {:?}", entered.map_err(|stop| stop.0).map(|it| it.map(|s| s.filetype)));
        assert!(matches!(sandbox.path_stat(ROOT_FD, b"workspace/src/proc_callers.rs", true), Ok(Ok(_))), "the member's own file is answered");
        let above = sandbox.path_stat(ROOT_FD, b"/workspace/../x", true);
        let Err(stop) = above else { panic!("a path above the workspace was answered") };
        assert!(stop.0.contains("named /workspace/../x, outside /workspace"), "{}", stop.0);
        // A fetched package's macro: its record at `/package`, nothing above it.
        let fetched: Arc<[(std::string::String, &'static [u8])]> = Arc::from(std::vec![(std::string::String::from("Cargo.toml"), &b"[package]"[..])]);
        let dependency = Package::new(Files::Listed(fetched), PackageEnv::default()).unwrap_or_else(|e| panic!("{e}"));
        let mut dep = Sandbox::new(Arc::new(dependency));
        assert!(matches!(dep.path_stat(ROOT_FD, b"/package/Cargo.toml", true), Ok(Ok(_))));
        let Err(stop) = dep.path_stat(ROOT_FD, b"/package/../workspace/Cargo.toml", true) else { panic!("a fetched package's macro climbed out of /package") };
        assert!(stop.0.contains("outside /package"), "{}", stop.0);
        println!("member entered at {cwd}");
    }
}
