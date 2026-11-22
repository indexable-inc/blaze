//! Tests of c_tasks_tree.rs: a script's process-tree requests run once, as one task, over a
//! FAKE runner (no process runs): its archive's bytes are a function of everything the job
//! hands it, so a key that missed an input would show as a file that should have changed.
//!
//! (a) found by key on the second ask: the runner runs once;
//! (b) a changed input reruns: a step's environment, the state's bytes, a tool's digest, the
//!     compiler, the OS build; control: equal inputs run nothing;
//! (c) a failing tree's error names the crate, the command, the exit and the last lines, and
//!     nothing is recorded;
//! (d) a tree stopped at its deadline is an error saying so;
//! (e) a record without the trees' folders is refused by name; the trees' section round-trips,
//!     `open` refuses a malformed one, and `built` drops it and stores the tree's files.

use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::c_tasks::{self, CStore, CompilerKind, GuestRoots, Refused, StoreFailed, Toolchain};
use crate::c_tasks_tree::{TreeError, TreeExit, TreeInputs, TreeJob, TreeRan, TreeRunner, last_lines, run_tree};
use crate::key::HostToolKey;
use crate::outputs::{CRequest, CState, CTree, FileLimits, FileMode, OutFileRef, Outputs, OutputsRef, RecordError, Snap, StoredFile, snap_digest};

const OUT: &str = "/target/debug/build/tikv-jemalloc-sys-0/out";
const PACKAGE: &str = "tikv-jemalloc-sys";

fn roots() -> GuestRoots<'static> {
    GuestRoots { package: "/package", out: OUT, builds: "/target/debug/build", deps: "/deps", sysroot: "/sdk" }
}

fn own<'a>(text: &str, alloc: Alloc<'a>) -> AString<'a> {
    String::from_str_in(text, alloc)
}

fn joined<'a>(argv: &[&str], alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    for (at, arg) in argv.iter().enumerate() {
        if at > 0 {
            out.push('\0');
        }
        out.push_str(arg);
    }
    out
}

/// What one fixture record varies.
#[derive(Clone, Copy)]
struct Shape {
    cc: &'static str,
    configure: &'static str,
    /// The sandbox kept each tree's folder and environment.
    trees: bool,
}

const BASE: Shape = Shape { cc: "cc", configure: "#!/bin/sh\necho configured\n", trees: true };

/// tikv-jemalloc-sys's record as `check` leaves it: three tree requests in `OUT_DIR/build`, one
/// state holding the copied `configure`, and the script's own copy of it as an inline file.
fn record(shape: Shape, alloc: Alloc<'_>) -> AVec<'_, u8> {
    let mut outputs = Outputs::new_in(OUT, alloc);
    let mut configure = own(OUT, alloc);
    configure.push_str("/build/configure");
    let mut prefix = own("--prefix=", alloc);
    prefix.push_str(OUT);
    let requests: [&[&str]; 3] = [&["sh", configure.as_str(), prefix.as_str()], &["make", "-j", "1"], &["make", "-j", "1", "install_lib_static", "install_include"]];
    let mut cwd = own(OUT, alloc);
    cwd.push_str("/build");
    for (at, argv) in requests.iter().enumerate() {
        outputs.c_requests.push(CRequest { argv: joined(argv, alloc), state: 0 });
        if shape.trees {
            let mut env = Vec::new_in(alloc);
            env.push((own("CC", alloc), own(shape.cc, alloc)));
            env.push((own("CFLAGS", alloc), own("-O2 -isysroot /sdk", alloc)));
            outputs.c_trees.push(CTree { request: at as u32, cwd: own(cwd.as_str(), alloc), env });
        }
    }
    let digest = snap_digest(shape.configure.as_bytes());
    outputs.hold_c_bytes(digest, shape.configure.as_bytes(), alloc);
    let mut entries = Vec::new_in(alloc);
    entries.push((own("build", alloc), Snap::Folder));
    entries.push((own("build/configure", alloc), Snap::Held(digest)));
    outputs.c_states.push(CState { entries });
    outputs.add_file("build/configure", shape.configure.as_bytes(), FileLimits::DEFAULT, alloc).expect("the script's copy");
    outputs.add_file("build/lib/libjemalloc_pic.a", b"a placeholder the script never wrote", FileLimits::DEFAULT, alloc).expect("a stale file");
    let mut bytes = Vec::new_in(alloc);
    outputs.encode(&mut bytes);
    bytes
}

/// The runner: its archive is the steps' text, folders, environments and the laid state's
/// bytes; `fails` makes one step end so.
struct Fake {
    ran: u32,
    fails: Option<(u32, TreeExit)>,
}

impl TreeRunner for Fake {
    fn run<'a>(&mut self, job: &TreeJob<'_, '_>, outputs: &OutputsRef<'_>, _key: ContentId, alloc: Alloc<'a>) -> Result<TreeRan<'a>, Refused<'a>> {
        self.ran += 1;
        if let Some((request, exit)) = self.fails {
            return Ok(TreeRan::Failed { request, exit, last_lines: last_lines(b"checking for cc... cc\nmake: *** [lib/libjemalloc.a] Error 2\n", 1, alloc) });
        }
        let mut archive = std::vec::Vec::new();
        for step in job.steps.iter() {
            archive.extend_from_slice(step.text.as_bytes());
            archive.extend_from_slice(step.tree.cwd.as_bytes());
            for (name, value) in step.tree.env() {
                archive.extend_from_slice(name.as_bytes());
                archive.extend_from_slice(value.as_bytes());
            }
        }
        let laid = c_tasks::laid_state(outputs, job.state, alloc).map_err(|_| Refused { words: own("the state", alloc) })?;
        for (name, bytes) in laid.iter() {
            archive.extend_from_slice(name.as_bytes());
            archive.extend_from_slice(bytes.unwrap_or(b"(folder)"));
        }
        let mut files = Vec::new_in(alloc);
        // Unsorted on purpose: `run_tree` sorts.
        files.push(StoredFile { name: own("include/jemalloc/jemalloc.h", alloc), len: 3, mode: FileMode::Plain, sha256: [5; 32] });
        files.push(StoredFile { name: own("build/lib/libjemalloc_pic.a", alloc), len: archive.len() as u64, mode: FileMode::Plain, sha256: ContentId::of("fake tree", &archive).0 });
        Ok(TreeRan::Made { files, left_out: 1 })
    }
}

struct Store {
    held: std::vec::Vec<(ContentId, std::vec::Vec<u8>)>,
    puts: u32,
}

impl CStore for Store {
    fn get(&mut self, key: ContentId, into: &mut AVec<'_, u8>) -> Result<bool, StoreFailed> {
        into.clear();
        match self.held.iter().find(|(k, _)| *k == key) {
            Some((_, bytes)) => {
                into.extend_from_slice(bytes);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn put(&mut self, key: ContentId, bytes: &[u8]) -> Result<(), StoreFailed> {
        self.puts += 1;
        match self.held.iter().find(|(k, _)| *k == key) {
            Some((_, held)) if held == bytes => Ok(()),
            Some(_) => Err(StoreFailed),
            None => {
                self.held.push((key, bytes.to_vec()));
                Ok(())
            }
        }
    }
}

/// The inputs beyond the record that (b) varies.
#[derive(Clone, Copy)]
struct Beyond {
    compiler: u8,
    make: u8,
    os_build: &'static str,
}

const SAME: Beyond = Beyond { compiler: 7, make: 1, os_build: "25B78" };

/// One ask: the tree's files and key, or its error in words.
fn ask(record: &[u8], beyond: Beyond, runner: &mut Fake, store: &mut Store) -> Result<(ContentId, std::vec::Vec<(std::string::String, [u8; 32])>, bool), std::string::String> {
    let alloc = blaze_own::testing::heap().alloc();
    let outputs = OutputsRef::open(record).expect("the record opens");
    let plan = c_tasks::plan(&outputs, &roots(), &[], alloc).expect("the plan");
    assert_eq!(plan.trees.as_slice(), [0, 1, 2], "the three requests are the tree");
    let toolchain = Toolchain { kind: CompilerKind::ShelfClang, compiler: [beyond.compiler; 32], sdk: b"26.1 25B78", target: "aarch64-apple-darwin" };
    let tools = [
        HostToolKey { name: "make", digest: [beyond.make; 32], runs_out_dir: false },
        HostToolKey { name: "sh", digest: [3; 32], runs_out_dir: true },
    ];
    let sources = [("/package", [2u8; 32])];
    let inputs = TreeInputs { toolchain: &toolchain, tools: &tools, os_build: beyond.os_build, sources: &sources, runner: "fake 1" };
    match run_tree(PACKAGE, &outputs, &roots(), &plan.trees, &inputs, runner, store, alloc) {
        Ok(done) => Ok((done.key, done.files.iter().map(|f| (f.name.as_str().to_owned(), f.sha256)).collect(), done.ran)),
        Err(error) => Err(error.to_string()),
    }
}

/// (a) The second ask is one get: the runner runs once, the files are equal and sorted.
#[test]
fn a_tree_is_found_by_key_on_the_second_ask() {
    let alloc = blaze_own::testing::heap().alloc();
    let bytes = record(BASE, alloc);
    let (mut runner, mut store) = (Fake { ran: 0, fails: None }, Store { held: std::vec::Vec::new(), puts: 0 });
    let first = ask(&bytes, SAME, &mut runner, &mut store).unwrap_or_else(|e| panic!("{e}"));
    let second = ask(&bytes, SAME, &mut runner, &mut store).unwrap_or_else(|e| panic!("{e}"));
    assert!(first.2 && !second.2, "ran first, found second");
    assert_eq!(runner.ran, 1, "the second ask ran nothing");
    assert_eq!(store.puts, 1);
    assert_eq!(first.0, second.0);
    assert_eq!(first.1, second.1);
    assert_eq!(first.1.iter().map(|f| f.0.as_str()).collect::<std::vec::Vec<_>>(), ["build/lib/libjemalloc_pic.a", "include/jemalloc/jemalloc.h"], "sorted by name");
}

/// (b) Each input moves the key and reruns the tree; control: the same inputs do not.
#[test]
fn a_changed_input_reruns_the_tree() {
    let alloc = blaze_own::testing::heap().alloc();
    let base = record(BASE, alloc);
    let mut store = Store { held: std::vec::Vec::new(), puts: 0 };
    let mut runner = Fake { ran: 0, fails: None };
    let (key, _, _) = ask(&base, SAME, &mut runner, &mut store).unwrap_or_else(|e| panic!("{e}"));
    let cases: [(&str, AVec<'_, u8>, Beyond); 5] = [
        ("a step's environment", record(Shape { cc: "clang", ..BASE }, alloc), SAME),
        ("the state's bytes", record(Shape { configure: "#!/bin/sh\necho other\n", ..BASE }, alloc), SAME),
        ("a tool's digest", record(BASE, alloc), Beyond { make: 9, ..SAME }),
        ("the compiler", record(BASE, alloc), Beyond { compiler: 8, ..SAME }),
        ("the OS build", record(BASE, alloc), Beyond { os_build: "25C56", ..SAME }),
    ];
    for (at, (what, bytes, beyond)) in cases.iter().enumerate() {
        let (moved, _, ran) = ask(bytes, *beyond, &mut runner, &mut store).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_ne!(moved, key, "{what} moves the key");
        assert!(ran, "{what} reruns the tree");
        assert_eq!(runner.ran, at as u32 + 2, "{what}: one run more");
    }
    // Control: the same inputs again run nothing.
    let (again, _, ran) = ask(&base, SAME, &mut runner, &mut store).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((again, ran), (key, false));
    assert_eq!(runner.ran, 6);
}

/// (c) A failing step: the crate, its command as recorded, its exit and its last lines; nothing
/// recorded, so a fixed tree runs.
#[test]
fn a_failing_tree_names_the_crate_command_exit_and_last_lines() {
    let alloc = blaze_own::testing::heap().alloc();
    let bytes = record(BASE, alloc);
    let mut store = Store { held: std::vec::Vec::new(), puts: 0 };
    let mut failing = Fake { ran: 0, fails: Some((1, TreeExit::Code(2))) };
    let error = ask(&bytes, SAME, &mut failing, &mut store).expect_err("the tree fails");
    assert!(error.contains(PACKAGE), "the crate: {error}");
    assert!(error.contains("process-tree request 1 failed (exit 2): make -j 1\n"), "the command and exit: {error}");
    assert!(error.ends_with("make: *** [lib/libjemalloc.a] Error 2\n"), "the last line: {error}");
    assert!(!error.contains("checking for cc"), "only the last line was asked for: {error}");
    assert_eq!(store.puts, 0, "nothing recorded");
    let mut fixed = Fake { ran: 0, fails: None };
    assert!(ask(&bytes, SAME, &mut fixed, &mut store).unwrap_or_else(|e| panic!("{e}")).2, "the fixed tree runs");
}

/// (d) A tree stopped at its deadline is an error that says so (rule 18).
#[test]
fn a_tree_past_its_deadline_is_an_error() {
    let alloc = blaze_own::testing::heap().alloc();
    let bytes = record(BASE, alloc);
    let mut store = Store { held: std::vec::Vec::new(), puts: 0 };
    let mut slow = Fake { ran: 0, fails: Some((0, TreeExit::Deadline)) };
    let error = ask(&bytes, SAME, &mut slow, &mut store).expect_err("the tree is stopped");
    assert!(error.contains("deadline"), "{error}");
    assert!(error.contains("process group killed"), "{error}");
    assert!(error.contains("sh /target/debug/build/tikv-jemalloc-sys-0/out/build/configure"), "{error}");
    assert_eq!(store.puts, 0);
}

/// (e) A record whose trees have no folder is refused by name, never run.
#[test]
fn a_tree_without_its_folder_is_refused_by_name() {
    let alloc = blaze_own::testing::heap().alloc();
    let bytes = record(Shape { trees: false, ..BASE }, alloc);
    let mut runner = Fake { ran: 0, fails: None };
    let error = ask(&bytes, SAME, &mut runner, &mut Store { held: std::vec::Vec::new(), puts: 0 }).expect_err("refused");
    assert!(error.contains("request 0 has no recorded folder"), "{error}");
    assert_eq!(runner.ran, 0);
}

/// (e) The trees' section round-trips; `open` refuses a request past the last, a relative
/// folder and an unsorted environment; `built` drops the section, keeps the tree's files as
/// stored files in place of the script's own, and its record opens.
#[test]
fn the_trees_section_round_trips_and_built_drops_it() {
    let alloc = blaze_own::testing::heap().alloc();
    let bytes = record(BASE, alloc);
    let opened = OutputsRef::open(&bytes).expect("opens");
    let trees: std::vec::Vec<_> = opened.c_trees().map(|t| (t.request, t.cwd.to_owned(), t.var("CC").map(str::to_owned))).collect();
    let cwd = format!("{OUT}/build");
    assert_eq!(trees, [(0, cwd.clone(), Some("cc".to_owned())), (1, cwd.clone(), Some("cc".to_owned())), (2, cwd, Some("cc".to_owned()))]);
    let mut again = Vec::new_in(alloc);
    opened.to_owned(alloc).encode(&mut again);
    assert_eq!(again.as_slice(), bytes.as_slice(), "to_owned and encode give the same bytes");

    let broken = |edit: &dyn Fn(&mut Outputs<'static>)| {
        let mut outputs = OutputsRef::open(&bytes).expect("opens").to_owned(alloc);
        edit(&mut outputs);
        let mut out = Vec::new_in(alloc);
        outputs.encode(&mut out);
        OutputsRef::open(&out).err()
    };
    assert!(matches!(broken(&|o| o.c_trees[2].request = 3), Some(RecordError::CInput { .. })), "a request past the last");
    assert!(matches!(broken(&|o| o.c_trees[1].cwd = own("build", alloc)), Some(RecordError::CInput { .. })), "a relative folder");
    assert!(matches!(broken(&|o| o.c_trees[0].env.swap(0, 1)), Some(RecordError::CInput { .. })), "an unsorted environment");
    assert!(matches!(broken(&|o| o.c_trees.swap(0, 1)), Some(RecordError::CInput { .. })), "requests out of order");

    let tree = [StoredFile { name: own("build/lib/libjemalloc_pic.a", alloc), len: 9, mode: FileMode::Plain, sha256: [4; 32] }];
    let built = c_tasks::built(&opened, &[], &tree, alloc).expect("built");
    let mut out = Vec::new_in(alloc);
    built.encode(&mut out);
    let reopened = OutputsRef::open(&out).expect("the built record opens");
    assert_eq!(reopened.c_trees().count(), 0);
    assert_eq!(reopened.c_requests().count(), 0);
    match reopened.file("build/lib/libjemalloc_pic.a") {
        Some(OutFileRef::Stored(stored)) => assert_eq!((stored.len, *stored.sha256), (9, [4; 32])),
        other => panic!("the tree's archive is a stored file in place of the script's: {other:?}"),
    }
    assert!(matches!(reopened.file("build/configure"), Some(OutFileRef::Inline(_))), "the script's other files are kept");
}

/// A runner whose tree made `file` holding the host folder `/hosts/x/work/trees/k/out`.
struct Leaky {
    file: &'static str,
}

impl TreeRunner for Leaky {
    fn run<'a>(&mut self, _job: &TreeJob<'_, '_>, _outputs: &OutputsRef<'_>, _key: ContentId, alloc: Alloc<'a>) -> Result<TreeRan<'a>, Refused<'a>> {
        // As native_c_tasks.rs's scan answers: a log or a depfile is let go, any other said.
        match crate::c_tasks_tree::left_out_holding_a_host_folder(self.file) {
            true => Ok(TreeRan::Made { files: Vec::new_in(alloc), left_out: 1 }),
            false => Ok(TreeRan::HostPath { file: own(self.file, alloc), host: own("/hosts/x/work/trees/k/out", alloc) }),
        }
    }
}

/// (f) Before 2026-11-21 a file the tree made holding a host folder was left out whatever it
/// was (`left_out += 1; continue`), so a later step found it missing with no cause. Now only a
/// log or a depfile is let go: `config.log` and `src/arena.d` (the TWINS, a log and a depfile
/// at depth) are counted and the tree is made; `bin/jemalloc-config` (the CONTROL: a file the
/// tree installs, neither) is the crate's error naming the file and the folder it holds, and
/// nothing is recorded.
#[test]
fn a_file_holding_a_host_folder_is_named_unless_a_log_or_a_depfile() {
    let alloc = blaze_own::testing::heap().alloc();
    let bytes = record(BASE, alloc);
    let run = |file: &'static str, store: &mut Store| -> Result<u32, std::string::String> {
        let outputs = OutputsRef::open(&bytes).expect("the record opens");
        let plan = c_tasks::plan(&outputs, &roots(), &[], alloc).expect("the plan");
        let toolchain = Toolchain { kind: CompilerKind::ShelfClang, compiler: [7; 32], sdk: b"26.1 25B78", target: "aarch64-apple-darwin" };
        let inputs = TreeInputs { toolchain: &toolchain, tools: &[], os_build: "25B78", sources: &[], runner: "fake 1" };
        run_tree(PACKAGE, &outputs, &roots(), &plan.trees, &inputs, &mut Leaky { file }, store, alloc).map(|done| done.left_out).map_err(|error| error.to_string())
    };
    for file in ["config.log", "src/arena.d"] {
        let mut store = Store { held: std::vec::Vec::new(), puts: 0 };
        assert_eq!(run(file, &mut store), Ok(1), "twin: {file} is let go and counted");
    }
    let mut store = Store { held: std::vec::Vec::new(), puts: 0 };
    let error = run("bin/jemalloc-config", &mut store).expect_err("control: an installed file holding the folder is refused");
    assert!(error.contains(PACKAGE) && error.contains("`$OUT_DIR/bin/jemalloc-config`") && error.contains("`/hosts/x/work/trees/k/out`"), "{error}");
    assert_eq!(store.puts, 0, "nothing recorded");
    assert!(!crate::c_tasks_tree::left_out_holding_a_host_folder("lib/libjemalloc.a"), "an archive is never let go");
    assert!(!crate::c_tasks_tree::left_out_holding_a_host_folder("src/arena.d/x.h"), "a folder named like a depfile is not one");
}

/// A tree error's words name what the coordinator reads first.
#[test]
fn a_tree_error_names_the_rule_for_a_cwd_outside() {
    let error = TreeError::CwdOutside { package: PACKAGE, request: 0, cwd: "/package" };
    let words = error.to_string();
    assert!(words.contains(PACKAGE) && words.contains("`/package`") && words.contains("outside its OUT_DIR"), "{words}");
}
