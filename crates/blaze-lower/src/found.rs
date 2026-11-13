//! A crate's plan key: what a lowering finds a crate's records by before it reads a file of
//! the crate's package or makes the crate (docs/v2/plan/design/restore.md, "Restart reads
//! records (d)", B).
//!
//! The key is a hash, by [`PLAN_KEY_DOMAIN`], of
//!
//! - what the plan says of the crate ([`Said`]): the fields `hir_def::deps`'s image key
//!   hashes of a crate (its `crate_inputs`), written with the same markers
//!   ([`hir_def::deps::PlanBase`]) so no path of the machine is in it, and what a front end
//!   keeps two crates of one root apart by beside those (whether the root is an alias, how
//!   cargo passes it, whether its package is a library's);
//! - each directory read into the crate's source root ([`dir_key`]): the directory's Merkle
//!   ID from the tree, with the paths under it that the load skips left out of it;
//! - each dependency's plan key, in the crate's order, with its name and flags.
//!
//! The front end's code identity is not in it: the lowering does not know it. The record's
//! key covers it (`hir_def::deps::found_key(plan, front_end)`), so a record written by one
//! front end is never asked for by another.
//!
//! **Cost.** [`dir_key`] asks the tree for one node ([`blaze_query::Tree::dir`]) when the
//! load skips nothing under the directory, and otherwise lists only the directories on the
//! way to a skipped path or to a mount below: the depth of those paths, never the
//! directory's files, and no file's bytes.
//!
//! **Symlinks.** What a symlink reaches is not under its directory's ID. A directory listed
//! on the way to a skipped path that holds one has no key ([`NoPlanKey::Symlink`]). A
//! directory taken whole by its ID is not listed, so a link inside it is not seen here: the
//! *writer* of a found record holds the other half of the rule, and files a record only for
//! a package whose walk followed no link (`files::Mounts::walk`'s `followed`). A directory
//! whose ID is the one a record was written for holds what it held then, links included, so
//! it holds none.
//!
//! **What the key is not.** It does not cover what a build script's run gives the crate
//! (cfg, variables, `OUT_DIR` files): a found record is a claim the serve checks against the
//! image key it makes from the database (`hir_def::deps::found`).

use base_db::{CrateOrigin, CrateTypes, ExtraCrateData, TargetKind};
use blaze_ids::{ContentId, Hasher};
use blaze_query::Tree;
use cfg::CfgOptions;
use hir_def::deps::PlanBase;
use span::Edition;

use crate::{LowerError, files};

/// The domain of a crate's plan key.
pub(crate) const PLAN_KEY_DOMAIN: &str = "blaze v2 found key 2";
/// The domain of a directory's key ([`dir_key`]).
pub(crate) const DIR_KEY_DOMAIN: &str = "blaze v2 found key directory 1";

/// Why a crate has no plan key: it is lowered by reading its package.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NoPlanKey {
    /// The crate was made by no plan's unit (a doctest example, a const-macro crate): the
    /// lowering kept nothing of it to make a key from.
    Unplanned,
    /// `path`, in a directory listed on the way to a skipped path, is a symlink.
    Symlink { path: String },
    /// The directory `dir` is not a directory of its mount's files.
    NotInTree { dir: String },
    /// The crate's root, or how cargo passes it, is under neither its package's directory
    /// nor its `OUT_DIR`: the key would hold a path of this machine.
    RootOutsidePackage { root: String },
    /// The crate's root is among the files of no package.
    NoPackage,
    /// A dependency, named as the crate names it, has no plan key.
    Dependency { name: String },
}

impl std::fmt::Display for NoPlanKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoPlanKey::Unplanned => f.write_str("it was made by no unit of a plan"),
            NoPlanKey::Symlink { path } => write!(
                f,
                "`{path}` is a symlink (rule: what a link reaches is not under its directory's ID)"
            ),
            NoPlanKey::NotInTree { dir } => {
                write!(f, "the directory `{dir}` is not in its mount's files")
            }
            NoPlanKey::RootOutsidePackage { root } => write!(
                f,
                "its root `{root}` is outside its package's directory (rule: a plan key holds no path of the machine)"
            ),
            NoPlanKey::NoPackage => f.write_str("its root is a file of no package"),
            NoPlanKey::Dependency { name } => write!(f, "its dependency `{name}` has no plan key"),
        }
    }
}

/// The key of the directory `dir` (absolute, as cargo names it) as a load of it that skips
/// `skipped` reads it: equal for two directories exactly when the files such a load reads
/// from them are (path under the directory, mode, content), wherever they are.
///
/// A directory with nothing skipped under it and no mount below it is one node of the tree.
/// Any other is listed, and each entry is a skipped path (by name alone: nothing of it is
/// read), a directory (its own key, the same way) or a file (name, mode and content ID,
/// which the tree holds: no bytes are read).
pub(crate) fn dir_key<T: Tree>(
    mounts: &files::Mounts<'_, T>,
    dir: &str,
    skipped: &[String],
) -> Result<Result<ContentId, NoPlanKey>, LowerError> {
    let (root, under) = mounts.locate(dir)?;
    let below = |it: &str| files::rest(it, dir).is_some_and(|rest| !rest.is_empty());
    let listed = skipped.iter().any(|it| below(it)) || mounts.mounts.iter().any(|it| below(&it.at));
    let mut hasher = Hasher::new(DIR_KEY_DOMAIN);
    if !listed {
        return Ok(match mounts.tree.dir(root, &under) {
            // alloc: a reason, once per package with no plan key
            None => Err(NoPlanKey::NotInTree { dir: dir.to_owned() }),
            Some(id) => Ok(hasher.update(&[0]).id(&id).finish()),
        });
    }
    let Some(entries) = mounts.tree.list(root, &under) else {
        // alloc: a reason, once per package with no plan key
        return Ok(Err(NoPlanKey::NotInTree { dir: dir.to_owned() }));
    };
    hasher.update(&[1]);
    for (name, is_dir) in entries {
        // alloc: a path a listed entry, on the way to a skipped path only
        let child = format!("{dir}/{name}");
        if skipped.iter().any(|it| *it == child) {
            hasher.part(name.as_bytes()).update(&[2]);
            continue;
        }
        if is_dir {
            match dir_key(mounts, &child, skipped)? {
                Ok(id) => hasher.part(name.as_bytes()).update(&[1]).id(&id),
                Err(none) => return Ok(Err(none)),
            };
            continue;
        }
        let (root, under) = mounts.locate(&child)?;
        let file = mounts
            .tree
            .file(root, &under)
            .map_err(|corrupt| LowerError::at(format!("`{child}` can't be read: {corrupt}"), &child))?;
        match file {
            Some((_, blaze_vfs::Mode::Symlink, _)) => {
                return Ok(Err(NoPlanKey::Symlink { path: child }));
            }
            Some((id, mode, _)) => {
                hasher.part(name.as_bytes()).update(&[0]).update(&mode.git().to_le_bytes()).id(&id);
            }
            // Listed and gone: the tree of a root never changes, so this is no file of it.
            None => {
                return Err(LowerError::at(
                    format!("`{child}` is listed in its directory and is no file of its mount"),
                    &child,
                ));
            }
        }
    }
    Ok(Ok(hasher.finish()))
}

/// A dependency as a crate's plan key holds it.
pub(crate) struct Dep<'a> {
    /// The dependency's plan key.
    pub(crate) key: ContentId,
    /// The name the crate gives it.
    pub(crate) name: &'a str,
    pub(crate) prelude: bool,
    pub(crate) sysroot: bool,
    pub(crate) private: bool,
}

/// What the plan says of a crate, before the crate is made: every field is one
/// [`plan_key`] hashes, so a field added here does not build until the key covers it.
pub(crate) struct Said<'a> {
    /// The package's directory (`CARGO_MANIFEST_DIR`), absolute.
    pub(crate) package_dir: &'a str,
    /// The crate's `OUT_DIR`, when a run gives it one.
    pub(crate) out_dir: Option<&'a str>,
    /// The root file, absolute and normal.
    pub(crate) root: &'a str,
    /// The root's directory as cargo passes the root to rustc (`Spelling::passed`).
    pub(crate) passed_dir: &'a str,
    /// Whether the crate's root is an alias of the file (a module is appended to it).
    pub(crate) aliased: bool,
    /// Whether its package is a library's (its inputs' durability).
    pub(crate) library: bool,
    pub(crate) edition: Edition,
    pub(crate) origin: &'a CrateOrigin,
    pub(crate) crate_attrs: &'a [Box<str>],
    pub(crate) target_kind: TargetKind,
    pub(crate) crate_types: CrateTypes,
    /// The directory its proc macros run in, absolute.
    pub(crate) proc_macro_cwd: &'a str,
    pub(crate) extra: &'a ExtraCrateData,
    /// `CrateWorkspaceData::content` of its workspace data.
    pub(crate) workspace: ContentId,
    pub(crate) cfg: &'a CfgOptions,
    pub(crate) env: &'a [(String, String)],
    pub(crate) deps: &'a [Dep<'a>],
    /// The [`dir_key`] of each directory read into its source root, by the directory's path.
    pub(crate) dirs: &'a [ContentId],
}

/// The plan key of the crate `said` is of, or why it has none.
///
/// The invariant: two crates of equal plan keys are made of the same package files, the same
/// settings and dependencies of equal plan keys (the front end is the record key's,
/// `hir_def::deps::found_key`); and the key is the
/// same for one crate in two checkouts at different paths (each path is written from the
/// package's directory or `OUT_DIR`, and a variable holding any other absolute path as one
/// marker).
pub(crate) fn plan_key(said: &Said<'_>) -> Result<ContentId, NoPlanKey> {
    Ok(joined(own_key(said)?, said.deps, said.dirs))
}

/// The domain of [`joined`].
pub(crate) const JOINED_DOMAIN: &str = "blaze v2 found key joined 1";

/// A crate's plan key from its [`own_key`], its dependencies' plan keys and its directories'
/// keys: the half of [`plan_key`] that moves with a file or a dependency, which the lowering
/// works out again after a write ([`crate::Lowered::plan_key`]) from the own key it kept when
/// it made the crate.
pub(crate) fn joined(own: ContentId, deps: &[Dep<'_>], dirs: &[ContentId]) -> ContentId {
    let mut hasher = Hasher::new(JOINED_DOMAIN);
    hasher.id(&own);
    hasher.update(&(deps.len() as u64).to_le_bytes());
    for dep in deps {
        hasher
            .id(&dep.key)
            .part(dep.name.as_bytes())
            .update(&[dep.prelude as u8, dep.sysroot as u8, dep.private as u8]);
    }
    hasher.update(&(dirs.len() as u64).to_le_bytes());
    for dir in dirs {
        hasher.id(dir);
    }
    hasher.finish()
}

/// The key of what the plan says of the crate itself: every field of `said` but `deps` and
/// `dirs`, which [`joined`] adds. Fixed for a crate of the front end: a crate whose own key
/// would differ is another crate (`Graph::made`).
pub(crate) fn own_key(said: &Said<'_>) -> Result<ContentId, NoPlanKey> {
    let Said {
        package_dir,
        out_dir,
        root,
        passed_dir,
        aliased,
        library,
        edition,
        origin,
        crate_attrs,
        target_kind,
        crate_types,
        proc_macro_cwd,
        extra,
        workspace,
        cfg,
        env,
        deps: _,
        dirs: _,
    } = said;
    let ExtraCrateData { version, display_name, potential_cfg_options, built_for, build_script_runs } =
        extra;
    let base = PlanBase::new(Some(*package_dir), *out_dir, env.iter().map(|(_, value)| value.as_str()));
    for path in [root, passed_dir] {
        if base.holds_machine_path(path) {
            // alloc: a reason, once per crate with no plan key
            return Err(NoPlanKey::RootOutsidePackage { root: (*path).to_owned() });
        }
    }
    let mut hasher = Hasher::new(PLAN_KEY_DOMAIN);
    base.path(&mut hasher, root);
    base.path(&mut hasher, passed_dir);
    hasher.update(&[*aliased as u8, *library as u8]);
    hasher.update(&(edition.number() as u64).to_le_bytes());
    origin.hash_into(&mut hasher);
    hasher.update(&(crate_attrs.len() as u64).to_le_bytes());
    for attr in crate_attrs.iter() {
        hasher.part(attr.as_bytes());
    }
    hasher.update(&[target_kind.tag(), crate_types.bits()]);
    // The directory a member's proc macros run in is its workspace's root, outside the
    // package: written as the one marker of an absolute path, like a variable's value. What
    // an expansion reads of it is the crate's reads (`hir_def::deps`'s `reads`), not its key.
    base.value(&mut hasher, proc_macro_cwd);
    hasher.opt_part(version.as_deref());
    match display_name {
        None => hasher.update(&[0]),
        Some(name) => hasher
            .update(&[1])
            .part(name.crate_name().as_str().as_bytes())
            .part(name.canonical_name().as_str().as_bytes()),
    };
    match potential_cfg_options {
        None => hasher.update(&[0]),
        Some(cfg) => hasher.update(&[1]).id(&base_db::cfg_options_content(cfg)),
    };
    built_for.hash_into(&mut hasher);
    hasher.update(&(build_script_runs.len() as u64).to_le_bytes());
    for run in build_script_runs.iter() {
        hasher.part(run.as_bytes());
    }
    hasher.id(workspace).id(&base_db::cfg_options_content(cfg));
    // alloc: the crate's variables sorted, once per crate per lowering
    let mut env: Vec<(&str, &str)> =
        env.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
    env.sort_unstable();
    hasher.update(&(env.len() as u64).to_le_bytes());
    for (name, value) in env {
        hasher.part(name.as_bytes());
        base.value(&mut hasher, value);
    }
    Ok(hasher.finish())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use blaze_query::MemTree;
    use blaze_units::query::{Mount, Origin};
    use blaze_vfs::Mode;

    use super::*;

    fn view_mount(at: &str) -> Vec<Mount> {
        vec![Mount { at: at.to_owned(), origin: Origin::View, under: String::new() }]
    }

    fn key(tree: &MemTree, view: ContentId, mounts: &[Mount], dir: &str, skipped: &[&str]) -> Result<ContentId, NoPlanKey> {
        let mounts = files::Mounts { mounts, view, tree, read_view: Cell::new(false) };
        let skipped: Vec<String> = skipped.iter().map(|it| (*it).to_owned()).collect();
        dir_key(&mounts, dir, &skipped).expect("no error")
    }

    /// What a load of `dir` skipping `skipped` reads, by path under `dir`: the oracle
    /// [`dir_key`] is held to.
    fn loaded(tree: &MemTree, view: ContentId, mounts: &[Mount], dir: &str, skipped: &[&str]) -> Vec<(String, ContentId)> {
        let mounts = files::Mounts { mounts, view, tree, read_view: Cell::new(false) };
        let skipped: Vec<String> = skipped.iter().map(|it| (*it).to_owned()).collect();
        let mut out = Vec::new();
        mounts.walk(dir, &skipped, &mut out).expect("walked");
        let mut read: Vec<(String, ContentId)> = out
            .into_iter()
            .map(|it| (files::rest(&it.path, dir).expect("under the directory").to_owned(), it.id))
            .collect();
        read.sort_by(|a, b| a.0.cmp(&b.0));
        read
    }

    const PACKAGE: &[(&str, &[u8])] = &[
        ("p/Cargo.toml", b"[package]"),
        ("p/src/lib.rs", b"pub fn f() {}"),
        ("p/src/a/mod.rs", b"mod b;"),
        ("p/target/debug/x", b"built 1"),
        ("p/nested/Cargo.toml", b"[package] # nested"),
        ("p/nested/src/lib.rs", b"nested"),
        ("beside.rs", b"beside"),
    ];

    fn with(change: &[(&'static str, &'static [u8])], without: &[&str]) -> Vec<(&'static str, &'static [u8])> {
        let mut files: Vec<(&str, &[u8])> = PACKAGE
            .iter()
            .copied()
            .filter(|(path, _)| !without.contains(path) && !change.iter().any(|(it, _)| it == path))
            .collect();
        files.extend_from_slice(change);
        files
    }

    /// The refinement: over a set of trees that differ in one thing each, two trees give one
    /// [`dir_key`] exactly when a load of the directory reads the same files from both
    /// (`files::Mounts::walk`, the reader the key stands for). What the load skips (the
    /// build's directory, a nested package) moves nothing; anything it reads does.
    #[test]
    fn a_directorys_key_is_equal_exactly_when_its_load_reads_the_same_files() {
        let tree = MemTree::default();
        let mounts = view_mount("/w");
        let skipped = ["/w/p/target", "/w/p/nested"];
        let variants: Vec<Vec<(&str, &[u8])>> = vec![
            with(&[], &[]),
            with(&[("p/target/debug/x", b"built 2")], &[]),
            with(&[("p/target/debug/y", b"more")], &[]),
            with(&[("p/nested/src/lib.rs", b"nested edited")], &[]),
            with(&[("beside.rs", b"beside edited")], &[]),
            with(&[("p/src/lib.rs", b"pub fn g() {}")], &[]),
            with(&[("p/src/a/mod.rs", b"mod c;")], &[]),
            with(&[("p/src/new.rs", b"")], &[]),
            with(&[("p/README", b"read me")], &[]),
            with(&[], &["p/src/a/mod.rs"]),
            with(&[("p/build.rs", b"fn main() {}")], &["p/Cargo.toml"]),
        ];
        let roots: Vec<ContentId> = variants.iter().map(|files| tree.root(files)).collect();
        let keys: Vec<ContentId> =
            roots.iter().map(|&root| key(&tree, root, &mounts, "/w/p", &skipped).expect("a key")).collect();
        let reads: Vec<_> = roots.iter().map(|&root| loaded(&tree, root, &mounts, "/w/p", &skipped)).collect();
        let mut same = 0;
        for a in 0..variants.len() {
            for b in 0..variants.len() {
                assert_eq!(keys[a] == keys[b], reads[a] == reads[b], "variants {a} and {b}");
                same += usize::from(a != b && keys[a] == keys[b]);
            }
        }
        assert_eq!(same, 4 * 5, "the five trees apart only in what the load skips or is beside it share a key");
        // The same package at another path of another checkout: the same key.
        let moved: Vec<(String, &[u8])> =
            PACKAGE.iter().map(|(path, bytes)| (format!("deep/er/{path}"), *bytes)).collect();
        let moved: Vec<(&str, &[u8])> = moved.iter().map(|(path, bytes)| (path.as_str(), *bytes)).collect();
        let elsewhere = tree.root(&moved);
        let there = key(&tree, elsewhere, &view_mount("/home/other"), "/home/other/deep/er/p", &["/home/other/deep/er/p/target", "/home/other/deep/er/p/nested"]);
        assert_eq!(there, Ok(keys[0]));
    }

    /// The control of the refinement above: a key that is the directory's whole ID, skipped
    /// paths and all, moves when only the build's directory did, which the oracle (what the
    /// load reads) says is the same package.
    #[test]
    fn the_oracle_catches_a_key_that_covers_what_the_load_skips() {
        let tree = MemTree::default();
        let mounts = view_mount("/w");
        let skipped = ["/w/p/target", "/w/p/nested"];
        let (one, two) = (tree.root(&with(&[], &[])), tree.root(&with(&[("p/target/debug/x", b"built 2")], &[])));
        assert_eq!(loaded(&tree, one, &mounts, "/w/p", &skipped), loaded(&tree, two, &mounts, "/w/p", &skipped));
        // The planted bug: nothing skipped is told to the key.
        let planted = |root| key(&tree, root, &mounts, "/w/p", &[]);
        assert_ne!(planted(one), planted(two), "the whole directory's ID moved with the build's files");
        assert_eq!(key(&tree, one, &mounts, "/w/p", &skipped), key(&tree, two, &mounts, "/w/p", &skipped));
    }

    /// A directory with nothing skipped under it costs one node: it is not listed, so its
    /// key is the same whether it holds three files or three thousand, and a tree that
    /// counts listings sees none.
    #[test]
    fn a_directory_with_nothing_skipped_is_one_node_at_two_sizes() {
        struct Counting(MemTree, std::sync::atomic::AtomicUsize, std::sync::atomic::AtomicUsize);
        impl Tree for Counting {
            fn file(&self, root: ContentId, path: &str) -> Result<Option<(ContentId, Mode, blaze_vfs::Bytes)>, blaze_vfs::Corrupt> {
                self.2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.0.file(root, path)
            }
            fn read_dir(&self, root: ContentId, path: &str, read: &mut dyn FnMut(blaze_query::Names<'_, '_>)) -> bool {
                self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.0.read_dir(root, path, read)
            }
            fn dir(&self, root: ContentId, path: &str) -> Option<ContentId> {
                self.0.dir(root, path)
            }
        }
        for size in [3usize, 3000] {
            let names: Vec<String> = (0..size).map(|at| format!("p/src/m{at}/mod.rs")).collect();
            let mut files: Vec<(&str, &[u8])> = names.iter().map(|it| (it.as_str(), b"mod x;".as_slice())).collect();
            files.push(("p/target/x", b"built"));
            let tree = Counting(MemTree::default(), Default::default(), Default::default());
            let view = tree.0.root(&files);
            let list = view_mount("/w");
            let mounts = files::Mounts { mounts: &list, view, tree: &tree, read_view: Cell::new(false) };
            let read = |tree: &Counting| {
                (tree.1.load(std::sync::atomic::Ordering::Relaxed), tree.2.load(std::sync::atomic::Ordering::Relaxed))
            };
            // Nothing skipped under `src`: no listing, no file asked for.
            assert!(dir_key(&mounts, "/w/p/src", &[]).unwrap().is_ok());
            assert_eq!(read(&tree), (0, 0), "{size} files");
            // The package with its `target` skipped: its own directory listed, `src` not.
            assert!(dir_key(&mounts, "/w/p", &["/w/p/target".to_owned()]).unwrap().is_ok());
            assert_eq!(read(&tree), (1, 0), "{size} files: one listing, of the package's directory");
        }
    }

    #[test]
    fn a_symlink_on_the_way_and_a_missing_directory_have_no_key_and_say_why() {
        let tree = MemTree::default();
        let mounts = view_mount("/w");
        let view = tree.root_with_modes(&[
            ("p/src/lib.rs", Mode::Regular, b"x"),
            ("p/link.rs", Mode::Symlink, b"../outside.rs"),
            ("p/target/x", Mode::Regular, b"built"),
        ]);
        let none = key(&tree, view, &mounts, "/w/p", &["/w/p/target"]);
        assert_eq!(none, Err(NoPlanKey::Symlink { path: "/w/p/link.rs".to_owned() }));
        assert!(none.unwrap_err().to_string().contains("`/w/p/link.rs` is a symlink"));
        assert_eq!(key(&tree, view, &mounts, "/w/q", &[]), Err(NoPlanKey::NotInTree { dir: "/w/q".to_owned() }));
        assert_eq!(
            key(&tree, view, &mounts, "/w/p/src/lib.rs", &[]),
            Err(NoPlanKey::NotInTree { dir: "/w/p/src/lib.rs".to_owned() }),
            "a file is no directory"
        );
    }

    #[expect(clippy::too_many_arguments, reason = "each is one thing a test varies")]
    fn said<'a>(
        dir: &'a str,
        root: &'a str,
        origin: &'a CrateOrigin,
        extra: &'a ExtraCrateData,
        cfg: &'a CfgOptions,
        env: &'a [(String, String)],
        deps: &'a [Dep<'a>],
        dirs: &'a [ContentId],
    ) -> Said<'a> {
        Said {
            package_dir: dir,
            out_dir: None,
            root,
            passed_dir: files::parent(root),
            aliased: false,
            library: false,
            edition: Edition::Edition2024,
            origin,
            crate_attrs: &[],
            target_kind: TargetKind::Lib { is_proc_macro: false },
            crate_types: CrateTypes::default(),
            proc_macro_cwd: dir,
            extra,
            workspace: ContentId::of("blaze v2 found test", b"workspace"),
            cfg,
            env,
            deps,
            dirs,
        }
    }

    fn extra(version: &str) -> ExtraCrateData {
        ExtraCrateData {
            version: Some(version.to_owned()),
            display_name: None,
            potential_cfg_options: None,
            built_for: base_db::BuiltFor::default(),
            build_script_runs: Box::default(),
        }
    }

    /// The invariant of [`plan_key`]: the same crate in two checkouts has one key (no path
    /// of the machine is in it), and each thing the plan says of it moves the key.
    #[test]
    fn a_plan_key_is_the_same_in_two_checkouts_and_moves_with_each_thing_said() {
        let origin = CrateOrigin::Local { repo: None, name: None };
        let (one, cfg) = (extra("0.1.0"), CfgOptions::default());
        let dirs = [ContentId::of("blaze v2 found test", b"dir")];
        let env_at = |dir: &str| {
            vec![
                ("CARGO_MANIFEST_DIR".to_owned(), dir.to_owned()),
                ("CARGO_PKG_NAME".to_owned(), "p".to_owned()),
                ("CARGO".to_owned(), format!("{}/bin/cargo", dir.rsplit_once('/').unwrap().0)),
            ]
        };
        let (here, there) = (env_at("/home/a/w/p"), env_at("/srv/checkout/p"));
        let base = plan_key(&said("/home/a/w/p", "/home/a/w/p/src/lib.rs", &origin, &one, &cfg, &here, &[], &dirs));
        let moved = plan_key(&said("/srv/checkout/p", "/srv/checkout/p/src/lib.rs", &origin, &one, &cfg, &there, &[], &dirs));
        assert!(base.is_ok());
        assert_eq!(base, moved, "two checkouts of one crate");

        let of = |change: &dyn Fn(&mut Said<'_>)| {
            let mut said = said("/home/a/w/p", "/home/a/w/p/src/lib.rs", &origin, &one, &cfg, &here, &[], &dirs);
            change(&mut said);
            plan_key(&said)
        };
        let two = extra("0.2.0");
        let other_dirs = [ContentId::of("blaze v2 found test", b"dir edited")];
        let other_env = {
            let mut env = here.clone();
            env.push(("FEATURE".to_owned(), "1".to_owned()));
            env
        };
        let dep = [Dep { key: dirs[0], name: "dep", prelude: true, sysroot: false, private: false }];
        let dep_edited = [Dep { key: other_dirs[0], name: "dep", prelude: true, sysroot: false, private: false }];
        let dep_renamed = [Dep { key: dirs[0], name: "other", prelude: true, sysroot: false, private: false }];
        let library = CrateOrigin::Local { repo: Some("repo".to_owned()), name: None };
        let changes: Vec<(&str, Result<ContentId, NoPlanKey>)> = vec![
            ("root", plan_key(&said("/home/a/w/p", "/home/a/w/p/src/main.rs", &origin, &one, &cfg, &here, &[], &dirs))),
            ("aliased", of(&|it| it.aliased = true)),
            ("library", of(&|it| it.library = true)),
            ("edition", of(&|it| it.edition = Edition::Edition2021)),
            ("origin", plan_key(&said("/home/a/w/p", "/home/a/w/p/src/lib.rs", &library, &one, &cfg, &here, &[], &dirs))),
            ("target kind", of(&|it| it.target_kind = TargetKind::Lib { is_proc_macro: true })),
            ("version", plan_key(&said("/home/a/w/p", "/home/a/w/p/src/lib.rs", &origin, &two, &cfg, &here, &[], &dirs))),
            ("env", plan_key(&said("/home/a/w/p", "/home/a/w/p/src/lib.rs", &origin, &one, &cfg, &other_env, &[], &dirs))),
            ("a file of the package", plan_key(&said("/home/a/w/p", "/home/a/w/p/src/lib.rs", &origin, &one, &cfg, &here, &[], &other_dirs))),
            ("a dependency", plan_key(&said("/home/a/w/p", "/home/a/w/p/src/lib.rs", &origin, &one, &cfg, &here, &dep, &dirs))),
            ("a dependency's key", plan_key(&said("/home/a/w/p", "/home/a/w/p/src/lib.rs", &origin, &one, &cfg, &here, &dep_edited, &dirs))),
            ("a dependency's name", plan_key(&said("/home/a/w/p", "/home/a/w/p/src/lib.rs", &origin, &one, &cfg, &here, &dep_renamed, &dirs))),
            ("workspace", of(&|it| it.workspace = ContentId::of("blaze v2 found test", b"another workspace"))),
            ("how cargo passes the root", of(&|it| it.passed_dir = "/home/a/w/p/src/../src")),
        ];
        let mut seen = vec![base.clone()];
        for (what, key) in changes {
            assert!(key.is_ok(), "{what}: {key:?}");
            assert!(!seen.contains(&key), "{what} did not move the key, or moved it onto another's");
            seen.push(key);
        }
        // A root outside the package would put this machine's path in the key: no key.
        let outside = plan_key(&said("/home/a/w/p", "/home/a/w/shared/lib.rs", &origin, &one, &cfg, &here, &[], &dirs));
        assert_eq!(outside, Err(NoPlanKey::RootOutsidePackage { root: "/home/a/w/shared/lib.rs".to_owned() }));
    }
}
