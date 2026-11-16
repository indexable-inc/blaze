//! The build-script runner against small command modules written here byte by byte (the
//! vendored Wasmtime takes no WAT text): each hostile script stopped by its own limit with a
//! typed error naming the package; and, in the simulation, a changed file or environment value
//! invalidating exactly the records that read it, with two planted bugs caught (a read left
//! out of the record, a variable left out of the key).

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use blaze_build_script::{
    Env,
    key::{CMode, RunLimits},
};
use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};

use crate::{
    script::{LoadedScript, ScriptError, ScriptLimit, ScriptRuntime, ScriptWhy},
    script_sandbox::{GitView, Read, Refusal, Script, SpawnRefusal, Tree, answer_now},
};
use blaze_build_script::git;

/// A fresh folder under the system's temporary one, by this process and a count.
fn folder(name: &str) -> PathBuf {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("blaze-build-script-test-{}-{name}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temporary folder");
    dir
}

fn uleb(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn sleb(mut value: i64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
        out.push(if done { byte } else { byte | 0x80 });
        if done {
            return;
        }
    }
}

fn i32c(value: i64) -> Vec<u8> {
    let mut out = vec![0x41];
    sleb(value, &mut out);
    out
}

fn i64c(value: i64) -> Vec<u8> {
    let mut out = vec![0x42];
    sleb(value, &mut out);
    out
}

fn call(index: u8) -> [u8; 2] {
    [0x10, index]
}

const LOOP: [u8; 2] = [0x03, 0x40];
const END: u8 = 0x0b;
const BR0: [u8; 2] = [0x0c, 0];
const BR_IF0: [u8; 2] = [0x0d, 0];
const DROP: u8 = 0x1a;
const GROW: [u8; 2] = [0x40, 0];
const NE: u8 = 0x47;
const UNREACHABLE: u8 = 0x00;
const LOAD: [u8; 3] = [0x28, 2, 0];

/// The imports, in function-index order.
const FD_WRITE: u8 = 0;
const PATH_OPEN: u8 = 1;
const SPAWN: u8 = 2;

/// Where the module's data sits.
const A_TXT: (i64, i64) = (1024, 15);
const B_TXT: (i64, i64) = (1056, 15);
const ESCAPE: (i64, i64) = (1088, 26);
const BIG: (i64, i64) = (1152, 7);
const CFG_IOV: i64 = 512;
const BIG_IOV: i64 = 528;
const WRITTEN: i64 = 600;
const OPENED: i64 = 604;
const GIT: i64 = 1280;
/// One write's bytes: 64 KiB of zeros from address 65536.
const CHUNK: i64 = 64 << 10;

/// The spawn request of `git rev-parse` in the shim's encoding.
fn git_request() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&2u32.to_le_bytes());
    for arg in ["git", "rev-parse"] {
        out.extend_from_slice(&(arg.len() as u32).to_le_bytes());
        out.extend_from_slice(arg.as_bytes());
    }
    out.push(0);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&[2, 2]);
    out
}

fn section(id: u8, body: &[u8], out: &mut Vec<u8>) {
    out.push(id);
    uleb(body.len() as u64, out);
    out.extend_from_slice(body);
}

fn name(text: &str, out: &mut Vec<u8>) {
    uleb(text.len() as u64, out);
    out.extend_from_slice(text.as_bytes());
}

/// A command module whose `_start` runs `body`. Its memory is two pages; the data: the
/// paths `workspace/a.txt`, `workspace/b.txt`, `workspace/../../etc/passwd` and `out/big`,
/// the iovec of `cargo:rustc-cfg=x\n` at [`CFG_IOV`], the iovec of a [`CHUNK`] of zeros at
/// [`BIG_IOV`], and the request of `git rev-parse` at [`GIT`].
fn wasm(body: &[u8]) -> Vec<u8> {
    const I32: u8 = 0x7f;
    const I64: u8 = 0x7e;
    let mut out = b"\0asm\x01\0\0\0".to_vec();
    // Types: 0 fd_write, 1 path_open, 2 spawn, 3 `_start`.
    let types: [(&[u8], &[u8]); 4] =
        [(&[I32, I32, I32, I32], &[I32]), (&[I32, I32, I32, I32, I32, I64, I64, I32, I32], &[I32]), (&[I32, I32], &[I32]), (&[], &[])];
    let mut s = vec![types.len() as u8];
    for (params, results) in types {
        s.push(0x60);
        s.push(params.len() as u8);
        s.extend_from_slice(params);
        s.push(results.len() as u8);
        s.extend_from_slice(results);
    }
    section(1, &s, &mut out);
    let mut s = vec![3];
    for (module, field, ty) in [("wasi_snapshot_preview1", "fd_write", 0), ("wasi_snapshot_preview1", "path_open", 1), ("blaze_build_script", "spawn", 2)] {
        name(module, &mut s);
        name(field, &mut s);
        s.extend_from_slice(&[0x00, ty]);
    }
    section(2, &s, &mut out);
    section(3, &[1, 3], &mut out);
    section(5, &[1, 0x00, 2], &mut out);
    let mut s = vec![2];
    for (field, kind, index) in [("memory", 2u8, 0u8), ("_start", 0, 3)] {
        name(field, &mut s);
        s.extend_from_slice(&[kind, index]);
    }
    section(7, &s, &mut out);
    let mut code = vec![0];
    code.extend_from_slice(body);
    code.push(END);
    let mut s = vec![1];
    uleb(code.len() as u64, &mut s);
    s.extend(code);
    section(10, &s, &mut out);
    let mut cfg_iov = 1200u32.to_le_bytes().to_vec();
    cfg_iov.extend_from_slice(&18u32.to_le_bytes());
    let mut big_iov = 65536u32.to_le_bytes().to_vec();
    big_iov.extend_from_slice(&(CHUNK as u32).to_le_bytes());
    let git = git_request();
    let data: [(i64, &[u8]); 8] = [
        (A_TXT.0, b"workspace/a.txt"),
        (B_TXT.0, b"workspace/b.txt"),
        (ESCAPE.0, b"workspace/../../etc/passwd"),
        (BIG.0, b"out/big"),
        (CFG_IOV, &cfg_iov),
        (BIG_IOV, &big_iov),
        (1200, b"cargo:rustc-cfg=x\n"),
        (GIT, &git),
    ];
    let mut s = vec![data.len() as u8];
    for (at, bytes) in data {
        s.push(0);
        s.extend(i32c(at));
        s.push(END);
        uleb(bytes.len() as u64, &mut s);
        s.extend_from_slice(bytes);
    }
    section(11, &s, &mut out);
    out
}

/// `fd_write(fd, iov, 1, WRITTEN)`, its errno dropped.
fn write(fd: Vec<u8>, iov: i64) -> Vec<u8> {
    let mut out = fd;
    out.extend(i32c(iov));
    out.extend(i32c(1));
    out.extend(i32c(WRITTEN));
    out.extend(call(FD_WRITE));
    out.push(DROP);
    out
}

/// Opens the `len`-byte path at `at` from the root (fd 3), `oflags` and rights given, into
/// [`OPENED`]; its errno dropped.
fn open((at, len): (i64, i64), oflags: i64, rights: i64) -> Vec<u8> {
    let mut out = Vec::new();
    for arg in [3, 0, at, len, oflags] {
        out.extend(i32c(arg));
    }
    out.extend(i64c(rights));
    out.extend(i64c(0));
    out.extend(i32c(0));
    out.extend(i32c(OPENED));
    out.extend(call(PATH_OPEN));
    out.push(DROP);
    out
}

/// Prints `cargo:rustc-cfg=x`.
fn print_cfg() -> Vec<u8> {
    write(i32c(1), CFG_IOV)
}

/// `body` forever.
fn forever(body: Vec<u8>) -> Vec<u8> {
    let mut out = LOOP.to_vec();
    out.extend(body);
    out.extend(BR0);
    out.push(END);
    out
}

const LIMITS: RunLimits = RunLimits { fuel: 1 << 26, memory: 16 << 20, out_dir_bytes: 1 << 20, out_dir_files: 16, output: 1 << 20, line: 256 << 10 };

/// The environment of the scripts here: `OUT_DIR` at `/out` and `X`.
fn env_of(x: &str) -> Vec<(String, String)> {
    vec![("OUT_DIR".to_owned(), "/out".to_owned()), ("X".to_owned(), x.to_owned())]
}

fn as_env(vars: &[(String, String)]) -> Env<'static> {
    let alloc = Alloc::global();
    let mut out = blaze_own::Vec::new_in(alloc);
    for (name, value) in vars {
        out.push((blaze_own::String::from_str_in(name, alloc), blaze_own::String::from_str_in(value, alloc)));
    }
    Env { vars: out }
}

fn tree(a: u8, b: u8) -> Tree {
    Tree::Mem { files: Arc::new(vec![("a.txt".to_owned(), vec![b'a', a]), ("b.txt".to_owned(), vec![b'b', b])]), git: None }
}

fn script(name: &str, tree: Tree, env: Vec<(String, String)>) -> Arc<Script> {
    Arc::new(Script {
        package: format!("{name} 0.0.0"),
        tree,
        folder: String::new(),
        env,
        target: "aarch64-apple-darwin".to_owned(),
        host: "aarch64-apple-darwin".to_owned(),
        c_mode: CMode::Deferred,
        limits: LIMITS,
        table: Arc::new(Vec::new()),
    })
}

fn load(runtime: &ScriptRuntime, name: &str, body: &[u8]) -> LoadedScript {
    runtime.load(&format!("{name} 0.0.0"), &wasm(body)).unwrap_or_else(|e| panic!("{e}"))
}

/// The error of running `body` as the package `name`.
fn stopped(name: &str, body: &[u8]) -> ScriptError {
    let runtime = ScriptRuntime::new(&folder("store")).expect("the runtime");
    let loaded = load(&runtime, name, body);
    let vars = env_of("0");
    match runtime.run(&loaded, script(name, tree(0, 0), vars.clone()), &as_env(&vars), Alloc::global()) {
        Ok(_) => panic!("{name} ran to its end"),
        Err(error) => {
            assert_eq!(error.package, format!("{name} 0.0.0"), "the error names the package: {error}");
            error
        }
    }
}

#[test]
fn a_well_behaved_script_runs_and_its_second_run_is_the_record() {
    let runtime = ScriptRuntime::new(&folder("store")).expect("the runtime");
    let mut body = open(A_TXT, 0, 2);
    body.extend(print_cfg());
    let loaded = load(&runtime, "good", &body);
    let vars = env_of("0");
    let first = runtime.run(&loaded, script("good", tree(0, 0), vars.clone()), &as_env(&vars), Alloc::global()).unwrap_or_else(|e| panic!("{e}"));
    assert!(!first.from_record);
    assert_eq!(first.outputs.cfgs.len(), 1);
    assert_eq!(first.reads.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(), ["file:a.txt"]);
    let second = runtime.run(&loaded, script("good", tree(0, 0), vars.clone()), &as_env(&vars), Alloc::global()).unwrap_or_else(|e| panic!("{e}"));
    assert!(second.from_record);
    assert_eq!(second.outputs.cfgs.len(), 1);
}

#[test]
fn an_endless_loop_is_stopped_by_fuel() {
    let error = stopped("looper", &forever(Vec::new()));
    assert!(matches!(error.why, ScriptWhy::Limit(ScriptLimit::Fuel, _)), "{error}");
}

#[test]
fn a_memory_balloon_is_stopped_by_the_memory_limit() {
    let mut body = LOOP.to_vec();
    body.extend(i32c(16));
    body.extend(GROW);
    body.extend(i32c(-1));
    body.push(NE);
    body.extend(BR_IF0);
    body.push(END);
    body.push(UNREACHABLE);
    let error = stopped("balloon", &body);
    assert!(matches!(error.why, ScriptWhy::Limit(ScriptLimit::Memory, _)), "{error}");
}

#[test]
fn a_huge_out_dir_file_is_stopped_by_the_out_dir_limit() {
    // `out/big` created and truncated, opened to write (right 6, `fd_write`), then written
    // forever.
    let mut body = open(BIG, 1 | 8, 1 << 6);
    let mut fd = i32c(OPENED);
    fd.extend(LOAD);
    body.extend(forever(write(fd, BIG_IOV)));
    let error = stopped("filler", &body);
    assert!(matches!(error.why, ScriptWhy::Limit(ScriptLimit::OutDir, _)), "{error}");
}

#[test]
fn a_megabyte_line_is_stopped_by_the_line_limit() {
    let error = stopped("shouter", &forever(write(i32c(1), BIG_IOV)));
    assert!(matches!(&error.why, ScriptWhy::Limit(ScriptLimit::Output, why) if why.contains("line")), "{error}");
}

#[test]
fn a_path_out_of_the_package_is_refused() {
    let error = stopped("escaper", &open(ESCAPE, 0, 2));
    assert!(matches!(error.why, ScriptWhy::Refused(Refusal::Path(_))), "{error}");
}

#[test]
fn a_git_question_blaze_cannot_answer_is_refused_by_name() {
    let mut body = i32c(GIT);
    body.extend(i32c(git_request().len() as i64));
    body.extend(call(SPAWN));
    body.push(DROP);
    let error = stopped("spawner", &body);
    assert!(
        matches!(&error.why, ScriptWhy::Refused(Refusal::Spawn(SpawnRefusal::Git { command, why: git::Refused::CannotAnswerYet })) if command == "rev-parse"),
        "{error}"
    );
}

const COMMIT_A: &str = "1939939b0c2d4e6f8a1b3c5d7e9f0a2b4c6d8e0f";
const COMMIT_B: &str = "defa64b2aa11bb22cc33dd44ee55ff6600778899";

/// A checkout written by hand into a fresh folder (never by running `git`): `(path, text)` each.
fn checkout(name: &str, files: &[(&str, &str)]) -> (PathBuf, Tree) {
    let root = folder(name);
    for (path, text) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a folder");
        std::fs::write(&path, text).expect("a fixture file");
    }
    (root.clone(), Tree::Dir(root))
}

/// `git <args>` asked in `folder` of `tree`: the answer's code and output, and the reads.
fn ask(tree: &Tree, folder: &str, args: &[&str]) -> (Result<(i32, String), git::Refused>, Vec<Read>) {
    let mut reads = Vec::new();
    let answered = git::answer(git::question(args), folder, tree.vcs(), &mut GitView::new(tree, &mut reads, Alloc::global()), Alloc::global());
    (answered.map(|answer| (answer.code, answer.stdout.as_str().to_owned())), reads)
}

/// A link standing for a folder inside the served tree leads nowhere: what lies behind it is
/// absent to stat, list and ranged read (git: `has_symlink_leading_path`, the entry ` D`), and
/// the link itself stats as a link. Remove the per-folder `lstat` in `GitView::inside` and the
/// outside file is reached.
#[test]
fn git_view_never_passes_through_a_linked_folder() {
    use blaze_build_script::git::GitFiles;
    let outside = folder("git-view-outside");
    std::fs::write(outside.join("secret"), "outside").expect("a fixture file");
    let (root, tree) = checkout("git-view-link", &[("inner/f", "inside")]);
    std::os::unix::fs::symlink(&outside, root.join("link")).expect("a link");
    std::os::unix::fs::symlink(outside.join("secret"), root.join("inner/last")).expect("a link");
    let mut reads = Vec::new();
    let mut view = GitView::new(&tree, &mut reads, Alloc::global());
    assert_eq!(view.stat("inner/f").map(|stat| (stat.kind, stat.size)), Some((git::FileKind::File, 6)));
    assert_eq!(view.stat("link").map(|stat| stat.kind), Some(git::FileKind::Symlink));
    assert_eq!(view.stat("inner/last").map(|stat| stat.kind), Some(git::FileKind::Symlink));
    assert!(view.stat("link/secret").is_none());
    assert!(view.stat("inner/../link/secret").is_none());
    let mut names = blaze_own::Vec::new_in(Alloc::global());
    view.list("link", Alloc::global(), &mut names);
    assert!(names.is_empty());
    view.list("inner", Alloc::global(), &mut names);
    assert_eq!(names.iter().map(|name| name.as_str()).collect::<Vec<_>>(), ["f", "last"]);
    let mut bytes = blaze_own::Vec::new_in(Alloc::global());
    assert!(!view.read_range("link/secret", 0, 7, &mut bytes));
    assert!(!view.read_range("inner/last", 0, 7, &mut bytes));
    assert!(view.read_range("inner/f", 0, 6, &mut bytes));
    assert_eq!(&bytes[..], b"inside");
}

fn read_of<'r>(reads: &'r [Read], path: &str) -> Option<&'r Read> {
    reads.iter().find(|read| read.subject == format!("file:{path}"))
}

#[test]
fn git_answers_a_loose_ref_and_a_new_commit_changes_exactly_its_reads() {
    let (root, tree) = checkout("git-loose", &[(".git/HEAD", "ref: refs/heads/main\n"), (".git/refs/heads/main", &format!("{COMMIT_A}\n")), ("crates/node/build.rs", "")]);
    let (head, reads) = ask(&tree, "crates/node", &["rev-parse", "HEAD"]);
    assert_eq!(head, Ok((0, format!("{COMMIT_A}\n"))));
    assert_eq!(ask(&tree, "crates/node", &["rev-parse", "--abbrev-ref", "HEAD"]).0, Ok((0, "main\n".to_owned())));
    assert_eq!(ask(&tree, "", &["symbolic-ref", "HEAD"]).0, Ok((0, "refs/heads/main\n".to_owned())));
    assert_eq!(ask(&tree, "", &["branch", "--show-current"]).0, Ok((0, "main\n".to_owned())));
    assert_eq!(ask(&tree, "", &["rev-parse", "--is-inside-work-tree"]).0, Ok((0, "true\n".to_owned())));
    assert_eq!(ask(&tree, "", &["--version"]).0, Ok((0, format!("{}\n", git::VERSION))));
    // Every place looked at is a read: the absent `.git` of the package's folders too.
    for absent in ["crates/node/.git", "crates/node/.git/HEAD", "crates/.git", ".git/commondir"] {
        assert_eq!(read_of(&reads, absent).map(|read| read.digest), Some(None), "{absent} in {reads:?}");
    }
    let loose = read_of(&reads, ".git/refs/heads/main").expect("the loose ref is a read").clone();
    let head_file = read_of(&reads, ".git/HEAD").expect("HEAD is a read").clone();
    // A new commit: the ref's read answers differently now, HEAD's the same.
    std::fs::write(root.join(".git/refs/heads/main"), format!("{COMMIT_B}\n")).expect("the new commit");
    assert_ne!(answer_now(&tree, &loose.subject), Ok(loose.digest), "the commit reruns the script");
    assert_eq!(answer_now(&tree, &head_file.subject), Ok(head_file.digest));
    assert_eq!(ask(&tree, "crates/node", &["rev-parse", "HEAD"]).0, Ok((0, format!("{COMMIT_B}\n"))));
}

#[test]
fn git_answers_a_packed_ref_and_a_detached_head() {
    let packed = format!("# pack-refs with: peeled fully-peeled sorted\n{COMMIT_B} refs/heads/other\n{COMMIT_A} refs/heads/main\n^{COMMIT_B}\n");
    let (_, tree) = checkout("git-packed", &[(".git/HEAD", "ref: refs/heads/main\n"), (".git/packed-refs", &packed)]);
    let (head, reads) = ask(&tree, "", &["rev-parse", "HEAD"]);
    assert_eq!(head, Ok((0, format!("{COMMIT_A}\n"))));
    assert_eq!(read_of(&reads, ".git/refs/heads/main").map(|read| read.digest), Some(None), "the absent loose ref is a read: {reads:?}");
    assert!(read_of(&reads, ".git/packed-refs").is_some_and(|read| read.digest.is_some()), "{reads:?}");

    let (_, tree) = checkout("git-detached", &[(".git/HEAD", &format!("{COMMIT_B}\n"))]);
    assert_eq!(ask(&tree, "", &["rev-parse", "HEAD"]).0, Ok((0, format!("{COMMIT_B}\n"))));
    assert_eq!(ask(&tree, "", &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "HEAD"]).0, Ok((0, "HEAD\n".to_owned())));
    assert_eq!(ask(&tree, "", &["branch", "--show-current"]).0, Ok((0, String::new())));
    assert_eq!(ask(&tree, "", &["symbolic-ref", "HEAD"]).0, Ok((128, String::new())));
}

#[test]
fn git_follows_a_linked_worktree_to_its_common_folder() {
    let (root, tree) = checkout(
        "git-worktree",
        &[
            ("main/.git/HEAD", "ref: refs/heads/main\n"),
            ("main/.git/refs/heads/main", &format!("{COMMIT_A}\n")),
            ("main/.git/refs/heads/feature", &format!("{COMMIT_B}\n")),
            ("main/.git/worktrees/wt/HEAD", "ref: refs/heads/feature\n"),
            ("main/.git/worktrees/wt/commondir", "../..\n"),
            ("wt/.git", "gitdir: ../main/.git/worktrees/wt\n"),
        ],
    );
    let (head, reads) = ask(&tree, "wt", &["rev-parse", "HEAD"]);
    assert_eq!(head, Ok((0, format!("{COMMIT_B}\n"))));
    for read in ["wt/.git", "main/.git/worktrees/wt/HEAD", "main/.git/worktrees/wt/commondir", "main/.git/refs/heads/feature"] {
        assert!(read_of(&reads, read).is_some_and(|read| read.digest.is_some()), "{read} in {reads:?}");
    }
    assert_eq!(ask(&tree, "wt", &["rev-parse", "--abbrev-ref", "HEAD"]).0, Ok((0, "feature\n".to_owned())));
    // git writes the gitdir absolute: it is followed while it stays inside the served tree.
    let absolute = format!("gitdir: {}/main/.git/worktrees/wt\n", root.display());
    std::fs::write(root.join("wt/.git"), absolute).expect("the absolute gitdir");
    assert_eq!(ask(&tree, "wt", &["rev-parse", "HEAD"]).0, Ok((0, format!("{COMMIT_B}\n"))));
    std::fs::write(root.join("wt/.git"), "gitdir: /elsewhere/.git/worktrees/wt\n").expect("a gitdir outside");
    assert_eq!(ask(&tree, "wt", &["rev-parse", "HEAD"]).0, Err(git::Refused::Outside(git::GitFile::Gitdir)));
}

#[test]
fn git_refuses_by_name_what_it_cannot_answer_exactly() {
    let (_, unborn) = checkout("git-refusals", &[(".git/HEAD", "ref: refs/heads/unborn\n")]);
    assert_eq!(ask(&unborn, "", &["rev-parse", "HEAD"]).0, Err(git::Refused::NoCommit));
    assert_eq!(ask(&unborn, "", &["rev-parse", "--short", "HEAD"]).0, Err(git::Refused::Short));
    assert_eq!(ask(&unborn, "", &["rev-parse", "--short=8", "HEAD"]).0, Err(git::Refused::Short));
    assert_eq!(ask(&unborn, "", &["status", "--porcelain"]).0, Err(git::Refused::NoCommit));
    assert_eq!(ask(&unborn, "", &["status", "--short"]).0, Err(git::Refused::Dirty));
    assert_eq!(ask(&unborn, "", &["describe", "--always", "--dirty"]).0, Err(git::Refused::Dirty));
    assert_eq!(ask(&unborn, "", &["describe", "--always", "--tags"]).0, Err(git::Refused::NoCommit));
    assert_eq!(ask(&unborn, "", &["log", "-1", "--pretty=format:%cI"]).0, Err(git::Refused::NoCommit));
    assert_eq!(ask(&unborn, "", &["log", "-1", "--pretty=format:%an"]).0, Err(git::Refused::Placeholder(*b"an")));
    assert_eq!(ask(&tree(0, 0), "", &["rev-parse", "HEAD"]).0, Err(git::Refused::NoRepository));
    assert_eq!(ask(&tree(0, 0), "", &["status", "--porcelain"]).0, Err(git::Refused::NoRepository));
    let words = |command: &str| git::shell_git(command, Alloc::global()).map(|words| words.iter().map(|w| w.as_str().to_owned()).collect::<Vec<_>>());
    assert_eq!(words("git rev-parse --abbrev-ref --symbolic-full-name HEAD").expect("plain"), ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "HEAD"]);
    // smolgen's quoted forms, read as `sh` reads them.
    assert_eq!(words("git log -1 --pretty=format:'%cI'").expect("quoted"), ["log", "-1", "--pretty=format:%cI"]);
    assert_eq!(words("git describe --always --tags --match \"tempo/v[0-9]*\"").expect("quoted"), ["describe", "--always", "--tags", "--match", "tempo/v[0-9]*"]);
    assert_eq!(git::question(&["status", "--porcelain", "--untracked-files=no"]), git::Question::Status { untracked: false });
    for refused in ["git status; rm -rf /", "git log \"$HOME\"", "git log 'open", "git describe --match v*", "sh -c git"] {
        assert!(words(refused).is_none(), "{refused}");
    }
}

#[test]
fn git_answers_a_fetched_source_from_its_commit() {
    let commit = git::Oid::parse_hex(COMMIT_A.as_bytes()).expect("hex");
    let fetched = Tree::Mem { files: Arc::new(vec![("crates/node/core/build.rs".to_owned(), Vec::new())]), git: Some(commit) };
    let (head, reads) = ask(&fetched, "crates/node/core", &["rev-parse", "HEAD"]);
    assert_eq!(head, Ok((0, format!("{COMMIT_A}\n"))));
    assert!(reads.is_empty(), "no `.git` is looked for: {reads:?}");
    assert_eq!(ask(&fetched, "", &["rev-parse", "--is-inside-work-tree"]).0, Ok((0, "true\n".to_owned())));
    // The served files are the commit's tree: clean, nothing untracked.
    assert_eq!(ask(&fetched, "", &["status", "--porcelain"]).0, Ok((0, String::new())));
    assert_eq!(ask(&fetched, "", &["status", "--porcelain", "--untracked-files=no"]).0, Ok((0, String::new())));
    // git's failure, stated: smolgen tolerates a non-zero `log` with no commit-date keys (D54).
    assert_eq!(ask(&fetched, "", &["log", "-1", "--pretty=format:%cI"]).0, Ok((128, String::new())));
    // A shallow, tagless checkout (D54): no tag reachable, so `--always` prints the commit at
    // git's automatic length, and without it git's own failure.
    assert_eq!(ask(&fetched, "", &["describe", "--always"]).0, Ok((0, format!("{}\n", &COMMIT_A[..7]))));
    assert_eq!(ask(&fetched, "crates/node/core", &["describe", "--always", "--tags", "--match", "v[0-9]*"]).0, Ok((0, format!("{}\n", &COMMIT_A[..7]))));
    assert_eq!(ask(&fetched, "", &["describe"]).0, Ok((128, String::new())));
    // The fact is keyed by the commit: another commit's record answers differently.
    let subject = "git:crates/node/core\nrev-parse HEAD";
    let other = Tree::Mem { files: Arc::new(Vec::new()), git: git::Oid::parse_hex(COMMIT_B.as_bytes()) };
    assert_ne!(answer_now(&fetched, subject), answer_now(&other, subject));
}

/// D54's early cutoff: the record lists `git:<folder>\n<question>` and its answer's digest. An
/// edit to `.git` that leaves the answer alone keeps the digest (the script is not rerun); one
/// that changes it does not.
#[test]
fn a_git_fact_reruns_its_script_only_when_its_answer_changes() {
    let (root, tree) = checkout("git-cutoff", &[(".git/HEAD", "ref: refs/heads/main\n"), (".git/refs/heads/main", &format!("{COMMIT_A}\n"))]);
    let subject = "git:\nrev-parse HEAD";
    let first = answer_now(&tree, subject).expect("a git subject");
    assert!(first.is_some());
    // HEAD's file changes (the branch is left for a detached head on the same commit), the
    // fact does not: no rerun.
    std::fs::write(root.join(".git/HEAD"), format!("{COMMIT_A}\n")).expect("detach");
    assert_eq!(answer_now(&tree, subject), Ok(first));
    // The answer flips: rerun. And a question refused now is an answer that differs.
    std::fs::write(root.join(".git/HEAD"), format!("{COMMIT_B}\n")).expect("move");
    assert_ne!(answer_now(&tree, subject), Ok(first));
    std::fs::remove_file(root.join(".git/HEAD")).expect("break");
    assert_eq!(answer_now(&tree, subject), Ok(None));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plant {
    None,
    /// The record is kept without the run's reads.
    UnrecordedRead,
    /// The key is made from the environment without `X`.
    VariableOutOfKey,
}

/// One case: four scripts, kept as records: 0 reads `a.txt`, 1 reads `b.txt`, 2 and 3 read
/// nothing and are given `X` (each its own value). Then some of the four inputs change, and
/// every script is looked up again. The oracle: a record answers exactly when its one input
/// is unchanged.
fn invalidation(runtime: &ScriptRuntime, loaded: &[LoadedScript; 3], tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let harness = |e: &dyn std::fmt::Debug| Broke::Harness(format!("{e:?}"));
    let mut values = [0u8; 4];
    for value in &mut values {
        *value = tape.int_in(0u8..=3, "value");
    }
    // Script `at`'s module and its run's tree and environment under `values`.
    let of = |at: usize, values: &[u8; 4]| {
        let module = &loaded[at.min(2)];
        // Scripts 2 and 3 run one module: their values differ by script, or one's changed input
        // could equal the other's, whose record then rightly answers.
        let vars = env_of(&if at >= 2 { format!("{at}-{}", values[at]) } else { "0".to_owned() });
        (module, script(&format!("s{at}"), tree(values[0], values[1]), vars.clone()), vars)
    };
    let key_env = |vars: &[(String, String)]| match plant {
        Plant::VariableOutOfKey => as_env(&vars.iter().filter(|(name, _)| name != "X").cloned().collect::<Vec<_>>()),
        _ => as_env(vars),
    };
    for at in 0..4 {
        let (module, script, vars) = of(at, &values);
        let base = runtime.base(module, &script, &key_env(&vars));
        let ran = runtime.fresh(module, script, base, Alloc::global()).map_err(|e| harness(&e))?;
        let expected: &[&str] = match at {
            0 => &["file:a.txt"],
            1 => &["file:b.txt"],
            _ => &[],
        };
        let read: Vec<&str> = ran.reads.iter().map(|r| r.subject.as_str()).collect();
        if read != expected {
            return Err(Broke::Oracle(format!("script {at} recorded {read:?}, not {expected:?}")));
        }
        let reads = if plant == Plant::UnrecordedRead { Vec::new() } else { ran.reads };
        runtime.keep(base, &reads, &ran.outputs).map_err(|e| harness(&e))?;
    }
    let mut changed = [false; 4];
    for (at, change) in changed.iter_mut().enumerate() {
        *change = tape.bool("change");
        if *change {
            values[at] += 1;
        }
    }
    for (at, changed) in changed.into_iter().enumerate() {
        let (module, script, vars) = of(at, &values);
        let base = runtime.base(module, &script, &key_env(&vars));
        let found = runtime.find(&base, &script.tree, Alloc::global()).map_err(|e| harness(&e))?;
        match (found.is_some(), changed) {
            (true, true) => return Err(Broke::Oracle(format!("stale: script {at}'s record answered after its input changed"))),
            (false, false) => return Err(Broke::Oracle(format!("script {at}'s record was lost, though its input did not change"))),
            _ => {}
        }
    }
    Ok(())
}

/// One case on a fresh store: the three modules (read `a.txt`, read `b.txt`, read nothing;
/// each prints a cfg) loaded, then [`invalidation`].
fn case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let runtime = ScriptRuntime::new(&folder("store")).map_err(|e| Broke::Harness(e.to_string()))?;
    let body = |read: Option<(i64, i64)>| {
        let mut body = read.map_or_else(Vec::new, |path| open(path, 0, 2));
        body.extend(print_cfg());
        body
    };
    let loaded = [load(&runtime, "s0", &body(Some(A_TXT))), load(&runtime, "s1", &body(Some(B_TXT))), load(&runtime, "s2", &body(None))];
    invalidation(&runtime, &loaded, tape, plant)
}

#[test]
fn a_changed_file_or_variable_invalidates_exactly_the_records_that_read_it() {
    blaze_sim::check(Config::cases(24), |tape| case(tape, Plant::None));
}

#[test]
fn control_a_read_left_out_of_the_record_is_caught() {
    blaze_sim::control(Config::cases(24), Plant::None, Plant::UnrecordedRead, "stale", case);
}

#[test]
fn control_a_variable_left_out_of_the_key_is_caught() {
    blaze_sim::control(Config::cases(24), Plant::None, Plant::VariableOutOfKey, "stale", case);
}
