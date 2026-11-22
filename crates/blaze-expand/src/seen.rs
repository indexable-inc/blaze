//! What a crate SEES as its own root (decision D96, docs/v2/plan/decisions.json): the one value
//! `env!("CARGO_MANIFEST_DIR")`, a proc macro and a build script are given, and the root an
//! absolute `include!` path is read under. It never depends on the workspace that uses the
//! crate, so a dependency's records are equal in every workspace:
//! - a dependency (registry, git, toolchain) sees ONE constant root, [`PACKAGE_ROOT`];
//! - a workspace member sees [`WORKSPACE_ROOT`]`/<member dir>`, with its workspace above it;
//!   the member's directory is in its crate key (rule 21), and member records are not shared.
//!
//! This is the value USER CODE sees. Where the host reads a crate's files (a tree number, a
//! pack, the checkout) is the caller's path space and never this (rule 76: two types, so one is
//! never passed for the other).
//!
//! The cargo book, "Environment variables Cargo sets for crates": "CARGO_MANIFEST_DIR — The
//! directory containing the manifest of your package."

use core::fmt;

use crate::cx::is_root_relative;

/// The root every dependency sees as its package (D96 (2)): `CARGO_MANIFEST_DIR` of a registry,
/// git or toolchain crate; the proc-macro sandbox serves the package's files here.
pub const PACKAGE_ROOT: &str = "/package";

/// The root a workspace member sees its workspace at (D96 (3)): a member's
/// `CARGO_MANIFEST_DIR` is `/workspace/<member dir>`.
pub const WORKSPACE_ROOT: &str = "/workspace";

/// The root a crate sees: see the module's doc. `Display` writes `CARGO_MANIFEST_DIR`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SeenRoot<'a> {
    /// A dependency: [`PACKAGE_ROOT`].
    Package,
    /// A workspace member at `dir` under the workspace: `/`-separated, no `.`, `..`, leading or
    /// trailing `/`; empty for the workspace's root package. Made by [`SeenRoot::member`].
    Member { dir: &'a str },
}

/// A member's directory that is not a plain path under the workspace ([`SeenRoot::member`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MemberDirError<'a> {
    pub dir: &'a str,
}

impl fmt::Display for MemberDirError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "member directory `{}` is not a plain path under the workspace (no `.`, `..`, `\\`, leading or trailing `/`): it cannot be seen at {WORKSPACE_ROOT}/<dir> (decision D96)",
            self.dir
        )
    }
}

impl core::error::Error for MemberDirError<'_> {}

impl<'a> SeenRoot<'a> {
    /// A member at `dir` under its workspace (`""` for the root package); a `dir` with `.`,
    /// `..`, `\` or a leading, trailing or doubled `/` is refused by name.
    pub fn member(dir: &'a str) -> Result<SeenRoot<'a>, MemberDirError<'a>> {
        if dir.is_empty() || is_root_relative(dir) {
            Ok(SeenRoot::Member { dir })
        } else {
            Err(MemberDirError { dir })
        }
    }

    /// Where a sandbox (a build script's, a proc macro's) serves the tree the crate's package
    /// is in: a dependency's tree is its package, served at [`PACKAGE_ROOT`]; a member's tree is
    /// its workspace, served at [`WORKSPACE_ROOT`] with the member at [`SeenRoot::folder`] in
    /// it. The one home of both mount points (rule 30): no crate spells `/workspace` itself.
    pub const fn mount(&self) -> &'static str {
        match self {
            SeenRoot::Package => PACKAGE_ROOT,
            SeenRoot::Member { .. } => WORKSPACE_ROOT,
        }
    }

    /// The package's folder in the tree served at [`SeenRoot::mount`]: `""` for a dependency
    /// (its tree is its package) and for the workspace's root package, else the member's dir.
    pub const fn folder(&self) -> &'a str {
        match *self {
            SeenRoot::Package => "",
            SeenRoot::Member { dir } => dir,
        }
    }

    /// The part of the absolute `written` path under [`SeenRoot::mount`] (a path from the
    /// served tree's root): `Some("")` for the mount itself, `None` when `written` is not under
    /// it or climbs (`..`): what a sandbox opens is read from its tree by this path.
    pub fn in_tree<'w>(&self, written: &'w str) -> Option<&'w str> {
        let rest = written.strip_prefix(self.mount())?;
        let rest = if rest.is_empty() { rest } else { rest.strip_prefix('/')? };
        (!rest.split('/').any(|part| part == "..")).then_some(rest)
    }

    /// Whether `in_tree`, a path from the served tree's root (what [`SeenRoot::in_tree`] gives,
    /// and what a sandbox's read list names a file by: `crates/sibling/x.json`), lies outside
    /// the package's own folder (decision D111: a member's read of a sibling, allowed and
    /// reported). Never for a dependency (its tree is its package) or the workspace's root
    /// package (dir `""`: every path of the tree is under it); for a member, every path not
    /// its dir nor under it (`crates/app2/x` is outside `crates/app`).
    pub fn left_package(&self, in_tree: &str) -> bool {
        match *self {
            SeenRoot::Package | SeenRoot::Member { dir: "" } => false,
            SeenRoot::Member { dir } => !(in_tree == dir || in_tree.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))),
        }
    }

    /// The part of the absolute `written` path under this root: `Some("data/x.txt")` for
    /// `/package/data/x.txt` when the crate sees `/package`. `None` when `written` is not under
    /// it (any other absolute path names the machine: an include of it stays refused).
    pub fn under<'w>(&self, written: &'w str) -> Option<&'w str> {
        let rest = match *self {
            SeenRoot::Package => written.strip_prefix(PACKAGE_ROOT)?,
            SeenRoot::Member { dir } => {
                let rest = written.strip_prefix(WORKSPACE_ROOT)?;
                if dir.is_empty() { rest } else { rest.strip_prefix('/')?.strip_prefix(dir)? }
            }
        };
        let rest = rest.strip_prefix('/')?;
        (!rest.is_empty()).then_some(rest)
    }

    /// Writes the path `file!()` and a panic's `Location` give for the file at `path` in the
    /// crate's package (package-relative): a member's workspace-relative path (`<dir>/<path>`,
    /// openable from the workspace's root: member records are not shared), a dependency's
    /// `<name>-<version>/<path>` (says which crate, stable, already in its key). D96 (4).
    pub fn write_shown(&self, package: (&str, &str), path: &str, out: &mut impl fmt::Write) -> fmt::Result {
        match *self {
            SeenRoot::Package => write!(out, "{}-{}/{path}", package.0, package.1),
            SeenRoot::Member { dir: "" } => out.write_str(path),
            SeenRoot::Member { dir } => write!(out, "{dir}/{path}"),
        }
    }
}

impl fmt::Display for SeenRoot<'_> {
    /// `CARGO_MANIFEST_DIR`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SeenRoot::Package => f.write_str(PACKAGE_ROOT),
            SeenRoot::Member { dir: "" } => f.write_str(WORKSPACE_ROOT),
            SeenRoot::Member { dir } => write!(f, "{WORKSPACE_ROOT}/{dir}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use core::fmt::{self, Write};

    use super::SeenRoot;

    /// A fixed buffer to write into (the crate is `no_std`).
    struct Buf {
        bytes: [u8; 96],
        len: usize,
    }

    impl Write for Buf {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let end = self.len + s.len();
            self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?.copy_from_slice(s.as_bytes());
            self.len = end;
            Ok(())
        }
    }

    fn shown(f: impl FnOnce(&mut Buf) -> fmt::Result) -> Buf {
        let mut b = Buf { bytes: [0; 96], len: 0 };
        f(&mut b).expect("fits");
        b
    }

    fn text(b: &Buf) -> &str {
        core::str::from_utf8(&b.bytes[..b.len]).expect("utf-8")
    }

    /// CARGO_MANIFEST_DIR: `/package` for every dependency, `/workspace/<dir>` for a member;
    /// a member dir that leaves the workspace is refused. Prints nothing; passes.
    #[test]
    fn a_crate_sees_one_root_by_its_placement() {
        let member = SeenRoot::member("crates/app").expect("a plain dir");
        assert_eq!(text(&shown(|b| write!(b, "{}", SeenRoot::Package))), "/package");
        assert_eq!(text(&shown(|b| write!(b, "{member}"))), "/workspace/crates/app");
        assert_eq!(text(&shown(|b| write!(b, "{}", SeenRoot::member("").expect("root")))), "/workspace");
        assert!(SeenRoot::member("../out").is_err() && SeenRoot::member("/abs").is_err() && SeenRoot::member("a/").is_err());
    }

    /// An absolute path under the crate's own root is the package's (D96 (4)); any other
    /// absolute path is not. Prints nothing; passes.
    #[test]
    fn an_absolute_path_under_the_seen_root_is_the_packages() {
        let member = SeenRoot::member("crates/app").expect("a plain dir");
        assert_eq!(SeenRoot::Package.under("/package/data/x.txt"), Some("data/x.txt"));
        assert_eq!(SeenRoot::Package.under("/packages/x"), None);
        assert_eq!(SeenRoot::Package.under("/package"), None);
        assert_eq!(member.under("/workspace/crates/app/tests/data"), Some("tests/data"));
        assert_eq!(member.under("/workspace/crates/apps/x"), None);
        assert_eq!(member.under("/package/x"), None);
        assert_eq!(SeenRoot::Package.under("/etc/passwd"), None);
    }

    /// A sandbox serves a dependency's package at `/package` and a member's workspace at
    /// `/workspace` with the member at its dir; `CARGO_MANIFEST_DIR` is the mount joined to the
    /// folder in both. A path is read from the tree only under its own mount. Prints nothing;
    /// passes.
    #[test]
    fn a_sandbox_mounts_a_dependency_at_package_and_a_member_s_workspace_at_workspace() {
        let member = SeenRoot::member("crates/app").expect("a plain dir");
        assert_eq!((SeenRoot::Package.mount(), SeenRoot::Package.folder()), ("/package", ""));
        assert_eq!((member.mount(), member.folder()), ("/workspace", "crates/app"));
        assert_eq!(SeenRoot::Package.in_tree("/package/build.rs"), Some("build.rs"));
        assert_eq!(SeenRoot::Package.in_tree("/package"), Some(""));
        assert_eq!(SeenRoot::Package.in_tree("/workspace/build.rs"), None);
        assert_eq!(SeenRoot::Package.in_tree("/packagex/a"), None);
        assert_eq!(SeenRoot::Package.in_tree("/package/../etc"), None);
        assert_eq!(member.in_tree("/workspace/README.md"), Some("README.md"));
        assert_eq!(member.in_tree("/package/README.md"), None);
    }

    /// D111: a member's path outside its own dir left its package, its own (and its dir) did
    /// not; a dependency's and the root package's never do. CONTROL (rule 8): write the member
    /// arm as `!in_tree.starts_with(dir)`: `crates/app2/x.json` is taken as the member's own
    /// and the third assertion fails. Prints nothing; passes.
    #[test]
    fn a_member_s_path_outside_its_dir_left_its_package() {
        let member = SeenRoot::member("crates/app").expect("a plain dir");
        assert!(!member.left_package("crates/app/abi/x.json"), "its own file");
        assert!(!member.left_package("crates/app"), "its own dir");
        assert!(member.left_package("crates/app2/x.json"), "a sibling sharing its name's start");
        assert!(member.left_package("crates/sibling/x.json"), "a sibling");
        assert!(member.left_package("README.md"), "the workspace's root");
        assert!(!SeenRoot::Package.left_package("src/x.rs"), "a dependency's tree is its package");
        let root = SeenRoot::member("").expect("the root package");
        assert!(!root.left_package("crates/app/x.json"), "the root package's folder is the tree");
    }

    /// `file!()`: a dependency's `<name>-<version>/<path>`, a member's workspace-relative path.
    #[test]
    fn file_shows_a_dependency_by_name_and_version_and_a_member_from_the_workspace() {
        let member = SeenRoot::member("crates/app").expect("a plain dir");
        assert_eq!(text(&shown(|b| SeenRoot::Package.write_shown(("serde", "1.0.0"), "src/lib.rs", b))), "serde-1.0.0/src/lib.rs");
        assert_eq!(text(&shown(|b| member.write_shown(("app", "0.1.0"), "src/lib.rs", b))), "crates/app/src/lib.rs");
    }
}
