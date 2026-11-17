//! A package's path-valued variables come from its mount row (decision D96, the coordinator's
//! ruling of 2026-11-16 19:2x): the path at which a unit's package is seen is ONE fact,
//! [`Files::seen`], and both the sandbox (what it serves where) and the environment
//! (`CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`) read it. So:
//! - a macro's read of either name is answered from the row ([`Package::row_var`], read first by
//!   `Sandbox::var` and `answer_now`), never from the list it was given;
//! - [`Package::new`] refuses a list whose set entry of either name differs from the row
//!   ([`SeenMismatch`]): a member served as a plain folder at `/package` while its crate's
//!   environment says `/workspace/<dir>` (the fault a member's `sol!("abi/x.json")` met) is a
//!   typed refusal naming both values, never a quiet override.
//!
//! The cargo book, "Environment variables Cargo sets for crates": "CARGO_MANIFEST_DIR — The
//! directory containing the manifest of your package." and "CARGO_MANIFEST_PATH — The path to
//! the manifest of your package." Both are paths of the one folder, so both follow the row.
//!
//! `OUT_DIR` is not answered here: the macro sandbox serves no `OUT_DIR` (a macro's reads are
//! its package's tree), so its value is the crate's environment's, passed through unchanged.

use blaze_expand::seen::SeenRoot;

use super::{Files, Package};
use crate::package_env::PackageEnv;

/// The variables whose value is a path of the package's own folder: derived from the row.
pub const ROW_VARS: [&str; 2] = ["CARGO_MANIFEST_DIR", "CARGO_MANIFEST_PATH"];

/// The value of `name` for a package seen at `seen`, when `name` is one of [`ROW_VARS`].
pub fn row_value(seen: SeenRoot<'_>, name: &str) -> Option<String> {
    match name {
        "CARGO_MANIFEST_DIR" => Some(format!("{seen}")),
        "CARGO_MANIFEST_PATH" => Some(format!("{seen}/Cargo.toml")),
        _ => None,
    }
}

/// A package's environment gave a path variable another value than its mount row
/// ([`Package::new`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SeenMismatch {
    /// The variable (one of [`ROW_VARS`]).
    pub name: String,
    /// The value the environment gave.
    pub given: String,
    /// The value the row gives: where the sandbox serves the package.
    pub row: String,
}

impl std::fmt::Display for SeenMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the package's environment gives {}={}, but its files are served at {} (its mount row, decision D96): a path variable is derived from the row, never given apart from it",
            self.name, self.given, self.row
        )
    }
}

impl std::error::Error for SeenMismatch {}

impl Package {
    /// The package served as `files`, its macros shown the variables `env`. A set variable of
    /// `env` named in [`ROW_VARS`] must equal the row's value ([`row_value`] of
    /// `files.seen()`), else it is refused by name; one not set is answered from the row all
    /// the same ([`Package::row_var`]).
    pub fn new(files: Files, env: PackageEnv) -> Result<Package, SeenMismatch> {
        for (name, given) in env.vars() {
            if let Some(row) = row_value(files.seen(), name)
                && row != given
            {
                return Err(SeenMismatch { name: name.to_owned(), given: given.to_owned(), row });
            }
        }
        Ok(Package { files, env })
    }

    /// The variable `name` when it is a path of the package's folder ([`ROW_VARS`]): its value
    /// from the row. `None`: not such a variable (read it from [`Package::env`]).
    pub fn row_var(&self, name: &str) -> Option<String> {
        row_value(self.files.seen(), name)
    }
}

#[cfg(test)]
mod tests {
    //! D96, one mount row: a member's macro that joins `CARGO_MANIFEST_DIR` with a relative path
    //! (`sol!`'s file form, include-style macros) reads where the variable says. No macro runs:
    //! the tests ask the sandbox what a guest's `getenv` and `path_open` are answered (the WASI
    //! calls a macro's `std::env::var` and `std::fs::read` make), as `seen_tests` does.
    //!
    //! Control (run once with the fix out, rule 8): in `member_files`, serve the member as the
    //! examples did, `Files::Folder(ws.join("crates/m"))` (blaze-real names_load's form):
    //! `a_member_s_macro_reads_its_file_where_cargo_manifest_dir_says` then fails with
    //! "a member's package: the package's environment gives
    //! CARGO_MANIFEST_DIR=/workspace/crates/m, but its files are served at /package (its mount
    //! row, decision D96): ..."; `the_sibling_s_file_is_read_and_listed` the same, and
    //! `an_env_entry_that_differs_from_the_row_is_refused` on its `path.name` assertion.
    //!
    //! Expected (`lanecheck.sh blaze-proc-macro test seen_env`): "test blaze-proc-macro: exit 0;
    //! 4 passed, 0 failed".

    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use blaze_expand::ProcLimit;
    use blaze_wasi::{Open, ROOT_FD, System};

    use super::super::{Files, Package, Sandbox};
    use super::SeenMismatch;
    use crate::package_env::PackageEnv;
    use crate::script_sandbox::ScriptRoot;

    /// `path_open` as wasi-libc asks for `O_RDONLY`: the right `fd_read`, no `oflags`.
    const READ_ONLY: Open = Open { dirflags: 1, oflags: 0, rights_base: 1 << 1, rights_inheriting: 0, fdflags: 0 };

    /// A workspace on disk: the member `crates/m` with `abi/x.json` ("member"), its sibling
    /// `crates/sibling` with `x.json` ("sibling").
    fn workspace() -> PathBuf {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        let ws = std::env::temp_dir().join(format!("blaze-proc-macro-seen-env-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        for (path, bytes) in [
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/m\", \"crates/sibling\"]\n"),
            ("crates/m/Cargo.toml", "[package]\nname = \"m\"\n"),
            ("crates/m/abi/x.json", "member"),
            ("crates/sibling/Cargo.toml", "[package]\nname = \"sibling\"\n"),
            ("crates/sibling/x.json", "sibling"),
        ] {
            let at = ws.join(path);
            std::fs::create_dir_all(at.parent().expect("a file has a folder")).expect("a temporary folder");
            std::fs::write(&at, bytes).expect("a temporary file");
        }
        ws
    }

    /// The member `crates/m` as the real command serves it (D96 (3)): its workspace at
    /// `/workspace`, the member at its dir.
    fn member_files(ws: &Path) -> Files {
        Files::Member { workspace: ws.to_path_buf(), root: ScriptRoot::member("crates/m").unwrap_or_else(|e| panic!("{e}")) }
    }

    fn env_of(pairs: &[(&str, &str)]) -> PackageEnv {
        PackageEnv::set_only(pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()).unwrap_or_else(|e| panic!("{e}"))
    }

    /// The member's environment as its crate's `CrateEnv` gives it (blaze-scope crate_env.rs
    /// `CrateEnv::of`: `CARGO_MANIFEST_DIR` is the seen root, `/workspace/<dir>`).
    const MEMBER_ENV: [(&str, &str); 3] =
        [("CARGO_MANIFEST_DIR", "/workspace/crates/m"), ("CARGO_MANIFEST_PATH", "/workspace/crates/m/Cargo.toml"), ("CARGO_PKG_NAME", "m")];

    fn member_sandbox(ws: &Path) -> Sandbox {
        let package = Package::new(member_files(ws), env_of(&MEMBER_ENV)).unwrap_or_else(|e| panic!("a member's package: {e}"));
        Sandbox::new(Arc::new(package))
    }

    /// The macro's `std::env::var(name)`: set, else a panic naming why not.
    fn var(sandbox: &mut Sandbox, name: &str) -> String {
        match sandbox.var(name) {
            Ok(Some(value)) => value,
            Ok(None) => panic!("{name} is unset"),
            Err(refused) => panic!("{name} is refused: {refused:?}"),
        }
    }

    /// What a macro reading `$CARGO_MANIFEST_DIR/<rel>` gets: the file's bytes, or the refusal
    /// (`Err` with the sandbox's words), or the errno.
    fn read_joined(sandbox: &mut Sandbox, rel: &str) -> Result<Vec<u8>, String> {
        let dir = var(sandbox, "CARGO_MANIFEST_DIR");
        let path = format!("{dir}/{rel}");
        let fd = match sandbox.open(ROOT_FD, path.as_bytes(), READ_ONLY) {
            Ok(Ok(fd)) => fd,
            Ok(Err(errno)) => return Err(format!("{path}: errno {errno:?}")),
            Err(stop) => return Err(stop.0),
        };
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            match sandbox.read(fd, &mut buf) {
                Ok(Ok(0)) => return Ok(out),
                Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
                Ok(Err(errno)) => return Err(format!("{path}: read errno {errno:?}")),
                Err(stop) => return Err(stop.0),
            }
        }
    }

    fn read_subjects(sandbox: &Sandbox) -> Vec<&str> {
        sandbox.reads.iter().map(|read| read.subject.as_str()).collect()
    }

    /// `sol!("abi/x.json")` in the member: the macro joins `CARGO_MANIFEST_DIR` with the path
    /// and reads the member's own file; the variable is answered from the row, and the read is
    /// listed by its path from the workspace. Expected line:
    /// `test sandbox::seen_env::tests::a_member_s_macro_reads_its_file_where_cargo_manifest_dir_says ... ok`.
    #[test]
    fn a_member_s_macro_reads_its_file_where_cargo_manifest_dir_says() {
        let ws = workspace();
        let mut sandbox = member_sandbox(&ws);
        assert_eq!(var(&mut sandbox, "CARGO_MANIFEST_DIR"), "/workspace/crates/m");
        assert_eq!(var(&mut sandbox, "CARGO_MANIFEST_PATH"), "/workspace/crates/m/Cargo.toml");
        let bytes = read_joined(&mut sandbox, "abi/x.json").unwrap_or_else(|why| panic!("the member's own file: {why}"));
        assert_eq!(bytes, b"member");
        let subjects = read_subjects(&sandbox);
        assert!(subjects.contains(&"env:CARGO_MANIFEST_DIR"), "the variable's read is listed: {subjects:?}");
        assert!(subjects.contains(&"file:crates/m/abi/x.json"), "the file's read is listed from the workspace: {subjects:?}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// `"../sibling/x.json"` from the member leaves the member and stays in the workspace
    /// (decision D111): allowed, the sibling's bytes, and the read listed by its path from the
    /// workspace: the read the caller files the D111 finding from (blaze-scope's
    /// `run_proc_macro` wrapper; that finding's test is blaze-cli's). Expected line:
    /// `test sandbox::seen_env::tests::the_sibling_s_file_is_read_and_listed ... ok`.
    #[test]
    fn the_sibling_s_file_is_read_and_listed() {
        let ws = workspace();
        let mut sandbox = member_sandbox(&ws);
        let bytes = read_joined(&mut sandbox, "../sibling/x.json").unwrap_or_else(|why| panic!("the sibling's file: {why}"));
        assert_eq!(bytes, b"sibling");
        assert!(read_subjects(&sandbox).contains(&"file:crates/sibling/x.json"), "the read is listed: {:?}", read_subjects(&sandbox));
        assert!(sandbox.refused.is_none(), "a read inside the workspace is not refused (D111)");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// A fetched package is served at `/package` and nothing above it exists: its macro's
    /// `$CARGO_MANIFEST_DIR/../x` is refused, naming the path. Expected line:
    /// `test sandbox::seen_env::tests::a_fetched_package_s_climb_is_refused_by_name ... ok`.
    #[test]
    fn a_fetched_package_s_climb_is_refused_by_name() {
        let files: Vec<(String, &'static [u8])> = vec![("Cargo.toml".to_owned(), &b"[package]\nname = \"dep\"\n"[..]), ("x".to_owned(), &b"inside"[..])];
        let package = Package::new(Files::Listed(Arc::from(files)), env_of(&[("CARGO_MANIFEST_DIR", "/package")])).unwrap_or_else(|e| panic!("a fetched package: {e}"));
        let mut sandbox = Sandbox::new(Arc::new(package));
        assert_eq!(read_joined(&mut sandbox, "x").as_deref(), Ok(&b"inside"[..]));
        let why = read_joined(&mut sandbox, "../x").expect_err("a read above /package is refused");
        assert!(why.contains("../x") && why.contains("outside /package"), "the refusal names the path and the tree: {why}");
        assert!(matches!(sandbox.refused, Some((ProcLimit::Paths, _))), "the run stops on the path limit");
    }

    /// An environment entry that differs from the row is refused at construction, naming the
    /// variable and both values: the member served as a folder at `/package` while its crate
    /// says `/workspace/crates/m` (the D96 fault), and a member whose environment says
    /// `/package`. Expected line:
    /// `test sandbox::seen_env::tests::an_env_entry_that_differs_from_the_row_is_refused ... ok`.
    #[test]
    fn an_env_entry_that_differs_from_the_row_is_refused() {
        let ws = PathBuf::from("/nonexistent-ws");
        let folder = Package::new(Files::Folder(ws.join("crates/m")), env_of(&MEMBER_ENV)).expect_err("a member served at /package");
        assert_eq!(folder, SeenMismatch { name: "CARGO_MANIFEST_DIR".to_owned(), given: "/workspace/crates/m".to_owned(), row: "/package".to_owned() });
        let mut pairs = MEMBER_ENV;
        pairs[1].1 = "/package/Cargo.toml";
        let path = Package::new(member_files(&ws), env_of(&pairs)).expect_err("CARGO_MANIFEST_PATH under /package for a member");
        assert_eq!(path.name, "CARGO_MANIFEST_PATH");
        assert_eq!(path.row, "/workspace/crates/m/Cargo.toml");
        assert!(path.to_string().contains("decision D96"), "the refusal names its rule: {path}");
        // Not set in the list: answered from the row all the same.
        let bare = Package::new(member_files(&ws), PackageEnv::default()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(bare.row_var("CARGO_MANIFEST_DIR").as_deref(), Some("/workspace/crates/m"));
        let mut sandbox = Sandbox::new(Arc::new(bare));
        assert_eq!(var(&mut sandbox, "CARGO_MANIFEST_DIR"), "/workspace/crates/m");
    }
}
