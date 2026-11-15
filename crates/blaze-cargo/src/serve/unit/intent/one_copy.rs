//! A query and an edit read the job's crates alone, one tree a file: over the copies a real plan
//! makes. One front end holds what two jobs lowered into it, as the daemon's kept front end does
//! after a default check and a check of tests: `p`'s lib, its test unit (its root an alias of
//! `src/lib.rs` with a harness appended, as `harness::write` appends one), and `p`'s bin, which
//! neither job below reads (the stranger: another job's crate).
//!
//! The planted bug is the code before 2026-11-14: every crate of the database read
//! (`Crate::all`), which read `src/lib.rs` twice (the file and its alias) and the bin's file too.

use base_db::FileId;
use blaze_api::{Edit, intent::Target, query::Cell};
use blaze_core::FileName;
use blaze_intent::Files as _;
use blaze_lower::{Appended, Lowered};
use blaze_units::{
    query::{
        BuildConfig, Command, Invocation, Mount, Origin, PackagePlan, TestSandbox, UnitPlan, Units,
    },
    schema::{Mode, Profile, Strip, Stripped, Target as UnitTarget, Unit, UnitDep, UnitGraph},
};
use blaze_vfs::{Mode as FileMode, Op, Vfs};
use std::alloc::{Allocator, Global};

use super::{ViewFiles, read_order};
use crate::serve::unit::files::Mounted;

const LIB: &str = "\
pub struct S;
impl S { pub fn unwrap(self) -> u32 { 1 } }
pub struct T;
impl T { pub fn unwrap(self) -> u32 { 2 } }
pub fn live() -> u32 { S.unwrap() }
#[cfg(test)]
mod tests {
    #[test]
    fn t() { let _ = crate::S.unwrap() + crate::live(); }
}
";

/// What the test unit's root has after the file: a harness that names the test and calls
/// `unwrap`, as no file of the view does.
const HARNESS: &str = "\nmod __harness { pub fn main() { crate::tests::t(); let _ = crate::S.unwrap(); } }\n";

const MAIN: &str = "fn main() { let _ = p::S.unwrap() + p::live(); }\n";

const MANIFEST: &[u8] = b"[package]\nname = \"p\"\n";

const RULES: &str = "(rule (unwrap-call e r) (expr e \"$r.unwrap()\"))\n";

/// The plan's units: `core`, then `p`'s lib, its test unit and its bin.
const CORE: usize = 0;
const PLAIN: usize = 1;
const TESTED: usize = 2;
const STRANGER: usize = 3;

fn unit(pkg: &str, kind: &str, name: &str, src: &str, checked_as_test: bool, deps: &[(usize, &str)]) -> Unit {
    Unit {
        pkg_id: pkg.to_owned(),
        target: UnitTarget {
            kind: vec![kind.to_owned()],
            crate_types: vec![kind.to_owned()],
            name: name.to_owned(),
            src_path: src.into(),
            edition: "2024".to_owned(),
            required_features: None,
            doc: false,
            doctest: false,
            test: true,
        },
        profile: Profile { incremental: true, strip: Strip::Resolved(Stripped::None), ..Profile::dev() },
        platform: None,
        macro_wasm: false,
        mode: Mode::Check,
        checked_as_test,
        features: Vec::new(),
        is_std: pkg.contains("library"),
        dependencies: deps
            .iter()
            .map(|&(index, name)| UnitDep {
                index,
                extern_crate_name: name.to_owned(),
                public: None,
                noprelude: Some(name == "core"),
                nounused: None,
                dev: false,
                gpu: false,
            })
            .collect(),
    }
}

fn plan(checked_as_test: bool) -> UnitPlan {
    UnitPlan {
        checked_as_test,
        metadata: String::new(),
        extra_filename: String::new(),
        unit_id: "0123456789abcdef".to_owned(),
        cfgs: Vec::new(),
        check_cfg: Vec::new(),
        rustflags: Vec::new(),
        lints: Vec::new(),
        env: Vec::new(),
        outputs: Vec::new(),
        out_dir: None,
        links: None,
        unsandboxed: false,
        runner: Vec::new(),
        harness: true,
        clippy: None,
        macro_wasm: false,
        gpu_msl: None,
    }
}

fn package(id: &str, name: &str, dir: &str, is_member: bool) -> PackagePlan {
    PackagePlan {
        id: id.to_owned(),
        name: name.to_owned(),
        version: "0.1.0".to_owned(),
        manifest_path: format!("{dir}/Cargo.toml"),
        source: Some(format!("({dir})")),
        is_member,
        default_run: None,
        checksum: None,
        tree: None,
        tests: TestSandbox::default(),
        const_macros: None,
    }
}

/// The front end both jobs lowered into, the test unit's harness appended to its root.
fn lowered() -> Lowered {
    let tree = blaze_query::MemTree::default();
    let library = tree.root(&[
        ("core/Cargo.toml", b"[package]\nname = \"core\"\n"),
        ("core/src/lib.rs", b"pub mod marker {}\n"),
    ]);
    let view = tree.root(&[
        ("Cargo.toml", MANIFEST),
        ("src/lib.rs", LIB.as_bytes()),
        ("src/main.rs", MAIN.as_bytes()),
    ]);
    let (core, p) = ("path+file:///rustc/abc/library/core#0.0.0", "path+file:///w#0.1.0");
    let units = Units {
        graph: UnitGraph {
            version: 1,
            units: vec![
                unit(core, "lib", "core", "/rustc/abc/library/core/src/lib.rs", false, &[]),
                unit(p, "lib", "p", "/w/src/lib.rs", false, &[(CORE, "core")]),
                unit(p, "lib", "p", "/w/src/lib.rs", true, &[(CORE, "core")]),
                unit(p, "bin", "p", "/w/src/main.rs", false, &[(CORE, "core"), (PLAIN, "p")]),
            ],
            roots: vec![TESTED, STRANGER],
        },
        plans: vec![plan(false), plan(false), plan(true), plan(false)],
        packages: vec![
            package(core, "core", "/rustc/abc/library/core", false),
            package(p, "p", "/w", true),
        ],
        lock: None,
        warnings: Vec::new(),
        notes: Vec::new(),
        target_dir: "/w/target".to_owned(),
        build_dir: "/w/target".to_owned(),
        finished: String::new(),
        link_dirs: Vec::new(),
        native: Vec::new(),
    };
    let config = BuildConfig {
        invocation: Invocation {
            command: Command::Check,
            args: Vec::new(),
            root: "/w".to_owned(),
            cwd: "/w".to_owned(),
            env: Vec::new(),
            files: Vec::new(),
        },
        mounts: vec![
            Mount { at: "/rustc/abc/library".to_owned(), origin: Origin::Root(library), under: String::new() },
            Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() },
        ],
        host: "x86_64-unknown-linux-gnu".to_owned(),
        library: "/rustc/abc/library".to_owned(),
        std: "/s/std".to_owned(),
        builds: "/s/build".to_owned(),
        sources: "/s/sources".to_owned(),
        blaze: String::new(),
    };
    let mut lowered = blaze_lower::lower(blaze_lower::Lower {
        units: &units,
        config: &config,
        files: &tree,
        root: view,
        host_cpu: None,
        sealed: &blaze_lower::unsealed(&units),
    })
    .unwrap();
    let harness = |_: &_, unit: usize, _| {
        Ok((unit == TESTED).then(|| Appended { after: HARNESS.to_owned(), module: "__harness" }))
    };
    lowered.append_roots([TESTED as u32], harness).unwrap();
    lowered
}

/// The view's files in the VFS, mounted at `/w` as a job mounts them.
fn mounted(vfs: &mut Vfs) -> Mounted {
    let write = |path: &str, bytes: &[u8]| Op::Write { mode: FileMode::Regular, path: path.into(), bytes: bytes.to_vec() };
    let ops = [write("Cargo.toml", MANIFEST), write("src/lib.rs", LIB.as_bytes()), write("src/main.rs", MAIN.as_bytes())];
    let root = vfs.apply(vfs.empty(), &ops).unwrap();
    let mounts = [Mount { at: "/w".into(), origin: Origin::View, under: String::new() }];
    Mounted::new(vfs.reader(), root, &mounts, "/w")
}

/// The crates a job of `units` reads, as [`super::read_crates`] orders them, in `alloc`.
fn job<A: Allocator + Clone>(lowered: &Lowered, units: &[usize], alloc: A) -> Vec<hir::Crate, A> {
    let mut read = Vec::new_in(alloc.clone()); // clone: the caller's allocator handle
    read.extend(units.iter().map(|&at| (at == TESTED, lowered.krate(at))));
    read_order(&read, alloc)
}

/// A file of the view a row can be in.
#[derive(Clone, Copy, Debug, PartialEq)]
enum In {
    Lib,
    Main,
}

/// Each `unwrap-call` row's place, its file and start, in reading order, in `alloc`.
fn unwraps<A: Allocator + Clone>(lowered: &Lowered, crates: &[hir::Crate], alloc: A) -> Vec<(In, u32), A> {
    let (mut vfs, _) = Vfs::restore(FileName::new("vfs/journal").unwrap(), 0, &[], None);
    let mounted = mounted(&mut vfs);
    let db = lowered.db();
    let alias = |id: FileId| db.files().alias_of(id).is_some();
    let files = ViewFiles::new(&mounted, lowered.paths(), alias, alloc.clone()); // clone: the caller's allocator handle
    let rows = blaze_astlog::query(db, &files, crates, RULES).unwrap();
    let relation = rows.relations.iter().find(|it| it.name == "unwrap-call").unwrap();
    let mut out = Vec::new_in(alloc);
    out.extend(relation.rows.iter().map(|row| match &row[0] {
        Cell::Code { path, start, .. } => match path.as_str() {
            "src/lib.rs" => (In::Lib, *start),
            "src/main.rs" => (In::Main, *start),
            other => panic!("a row in {other}"),
        },
        other => panic!("not code: {other:?}"),
    }));
    out
}

/// `bytes`, a file's text after edits, as text.
fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

/// `edits` made through `crates`: `src/lib.rs` and `src/main.rs` after them, in `alloc`.
fn edited<A: Allocator + Clone>(
    lowered: &Lowered,
    crates: &[hir::Crate],
    edits: &[Edit],
    alloc: A,
) -> (Vec<u8, A>, Vec<u8, A>) {
    let (mut vfs, _) = Vfs::restore(FileName::new("vfs/journal").unwrap(), 0, &[], None);
    let mounted = mounted(&mut vfs);
    let db = lowered.db();
    let alias = |id: FileId| db.files().alias_of(id).is_some();
    let files = ViewFiles::new(&mounted, lowered.paths(), alias, alloc.clone()); // clone: the caller's allocator handle
    let applied = blaze_intent::apply(db, &files, crates, edits, alloc.clone()).unwrap(); // clone: the caller's allocator handle
    assert!(files.corrupt.into_inner().is_none());
    let mut texts = (Vec::new_in(alloc.clone()), Vec::new_in(alloc.clone())); // clone: the caller's allocator handle
    texts.0.extend_from_slice(LIB.as_bytes());
    texts.1.extend_from_slice(MAIN.as_bytes());
    let mut ops = Vec::new_in(alloc);
    ops.extend(applied.edited.ops.iter());
    ops.sort_by_key(|it| match it {
        Op::Edit { at, .. } => std::cmp::Reverse(*at),
        other => panic!("not a text edit: {other:?}"),
    });
    for op in ops {
        let Op::Edit { at, len, path, bytes } = op else { continue };
        let text = match path.as_str() {
            "src/lib.rs" => &mut texts.0,
            "src/main.rs" => &mut texts.1,
            other => panic!("an edit of {other}"),
        };
        let (at, len) = (*at as usize, *len as usize);
        let tail = text.split_off(at + len);
        text.truncate(at);
        text.extend_from_slice(bytes);
        text.extend_from_slice(&tail);
    }
    texts
}

/// The places of `S.unwrap()` in `live` and `crate::S.unwrap()` in the test, each once.
fn the_two() -> [(In, u32); 2] {
    let at = |code: &str| (In::Lib, u32::try_from(LIB.find(code).unwrap()).unwrap());
    [at("S.unwrap() }"), at("crate::S.unwrap()")]
}

/// The invariant: a match is one row whatever copies of its file the front end holds. A default
/// job reads its lib's tree; a job of tests reads the test unit's, the one its root's alias
/// is, and nothing of the harness appended to it. Neither reads the bin, which is another
/// job's.
#[test]
fn a_query_is_one_row_a_match_through_the_jobs_crates_alone() {
    let lowered = lowered();
    assert!(lowered.db().files().alias_of(lowered.krate(TESTED).unwrap().data(lowered.db()).root_file_id).is_some());
    let rows = |units: &[usize]| unwraps(&lowered, &job(&lowered, units, Global), Global);
    assert_eq!(rows(&[CORE, PLAIN]), the_two(), "the default job");
    assert_eq!(rows(&[CORE, PLAIN, TESTED]), the_two(), "the job of tests");
    assert_eq!(rows(&[CORE, TESTED]), the_two(), "the test unit alone");
}

/// The control: every crate of the database read (the planted bug) reads `src/lib.rs` through
/// the file and its alias, and the bin's file, which the invariant's oracle sees.
#[test]
fn the_oracle_catches_a_query_that_reads_every_crate() {
    let lowered = lowered();
    let every = hir::Crate::all(lowered.db());
    let rows = unwraps(&lowered, &every, Global);
    assert_ne!(rows, the_two(), "the planted bug's rows went unseen");
    assert!(rows.iter().any(|it| it.0 == In::Main), "{rows:?}");
}

/// A rename and a replace through a job of tests edit each reference in `src/lib.rs` once, and
/// nothing of the bin, which is another job's; the test unit's harness, which names the test,
/// is no file and is not edited.
#[test]
fn an_edit_by_meaning_is_one_edit_a_reference_through_the_jobs_crates_alone() {
    let lowered = lowered();
    let tests = job(&lowered, &[CORE, PLAIN, TESTED], Global);
    let rename = |item: &str, to: &str| Edit::Rename {
        to: to.to_owned(),
        item: Target { file: "src/lib.rs".to_owned(), item: item.to_owned() },
    };
    let (lib, main) = edited(&lowered, &tests, &[rename("live", "alive")], Global);
    assert_eq!(text(&lib), LIB.replace("live()", "alive()"), "the rename of a fn the test calls");
    assert_eq!(text(&main), MAIN, "the bin is another job's");
    let (lib, _) = edited(&lowered, &tests, &[rename("tests::t", "t2")], Global);
    assert_eq!(text(&lib), LIB.replace("fn t()", "fn t2()"), "the rename of the test the harness names");
    let replace = Edit::Replace { from: "p::S".to_owned(), to: "p::T".to_owned() };
    let (lib, main) = edited(&lowered, &tests, &[replace], Global);
    // A replace spells its target from `crate` in its own crate.
    let want = LIB
        .replace("impl S {", "impl crate::T {")
        .replace("{ S.unwrap()", "{ crate::T.unwrap()")
        .replace("crate::S.unwrap()", "crate::T.unwrap()");
    assert_eq!(text(&lib), want, "each reference to `S` retargeted once");
    assert_eq!(text(&main), MAIN, "the bin is another job's");
}

/// What the spec's "Not built" says: with the bin in the job of tests, a rename of the lib's
/// `live` renames the test unit's, which the bin does not depend on, so the bin's call is left.
/// Pinned so the day it changes, the spec does too.
#[test]
fn a_rename_through_a_job_of_tests_leaves_a_dependent_of_the_libs_own_copy() {
    let lowered = lowered();
    let all = job(&lowered, &[CORE, PLAIN, TESTED, STRANGER], Global);
    let rename = Edit::Rename {
        to: "alive".to_owned(),
        item: Target { file: "src/lib.rs".to_owned(), item: "live".to_owned() },
    };
    let (lib, main) = edited(&lowered, &all, &[rename], Global);
    assert_eq!(text(&lib), LIB.replace("live()", "alive()"));
    assert_eq!(text(&main), MAIN, "the bin's call of the lib's own `live`");
}
