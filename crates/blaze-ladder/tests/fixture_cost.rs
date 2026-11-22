//! The std world is made once per machine and read after (tests/toolchain_store): a test that
//! finds it parses no toolchain source while its world is built.
//!
//! - In one process: the second construction reads the world (`Made::Read`) and reads 0
//!   toolchain files; the records answer (`Option::Some` from the program's root).
//! - In a fresh process after a first: this binary runs itself on `fixture_cost_child`, which
//!   prints what its construction did; it too reads the world and 0 files.
//! - The control (rule 23: remove the cause, the effect goes): the key over the same inputs is
//!   one key, and a toolchain crate's cfg with one atom more is another, under which the store
//!   holds no world; a harness whose key ignored the cfg would find one there.
//! - The counter counts: when the first construction in this process made the world, it read
//!   toolchain files (more than 0); a counter stuck at 0 fails there.
//!
//! It should print, on a machine where the world is made (times are the run's):
//!
//! ```text
//! fixture cost: first Read (0 files) in 400 ms, second Read (0 files) in 300 ms, program solve 40 ms, ratio 7.5 to the program's solve
//! fixture cost: a fresh process: made=Read toolchain_files_read=0
//! fixture cost: a cfg atom more is another key, not made
//! ```

mod toolchain_store;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::resolve::{Ns, Paths};
use blaze_scope::resolve::CrateInput;
use blaze_toolchain::PINNED;

use toolchain_store::{Made, OverRecords, front_over_records, is_made, names_row, shelf_dir, toolchain_only, world_key};

const EDITION: &str = "blaze-2026.11";

const MAIN: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, \"Hello, world!\")?;
    Ok(())
}
";

/// Set in the child's environment by [`a_fresh_process_reads_the_world_an_earlier_one_made`]:
/// `fixture_cost_child` does its work only under it.
const CHILD: &str = "BLAZE_FIXTURE_COST_CHILD";

/// The longest the child may take: a world already made is read in well under a second; this
/// covers a cold page cache and a loaded machine (rule 69). Past it the child is killed and the
/// test fails naming it.
const CHILD_TIMEOUT: Duration = Duration::from_secs(300);

fn user() -> &'static UserCrate<'static> {
    let files: &'static [(String, String)] = Vec::leak(vec![("/hello-world/src/main.rs".to_string(), MAIN.to_string())]);
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    Box::leak(Box::new(UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files, edition, panic: blaze_build::Panic::Unwind }))
}

fn world() -> OverRecords {
    front_over_records(&shelf_dir(), user(), blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the std world: {e}"))
}

#[test]
fn a_second_world_in_one_process_parses_no_toolchain_source() {
    let first = world();
    if let Made::Written { crates } = first.made {
        assert!(crates > 0, "a written world holds the toolchain's crates");
        assert!(first.toolchain_files_read > 0, "the world was made from source but the counter read {} files: it counts nothing", first.toolchain_files_read);
    }
    let start = Instant::now();
    let second = world();
    let second_ms = start.elapsed().as_millis();

    assert_eq!(second.made, Made::Read, "the second construction in this process made the world again");
    assert_eq!(second.toolchain_files_read, 0, "the second construction read {} toolchain source files", second.toolchain_files_read);
    assert_eq!(first.world, second.world, "one program over one shelf is one world");
    assert_eq!(first.store, second.store);
    assert!(second.store.starts_with(toolchain_store::store_base().expect("the test store's place")));

    let some = Paths { names: second.records }.path(second.root, false, &["Option", "Some"], Ns::Value);
    assert!(some.is_ok(), "`Option::Some` from the program's root over the store's records: {some:?}");

    let program_ms = second.program_took.as_secs_f64() * 1e3;
    println!(
        "fixture cost: first {:?} ({} files) in {} ms, second {:?} ({} files) in {second_ms} ms, program solve {:.0} ms, ratio {:.1} to the program's solve",
        first.made,
        first.toolchain_files_read,
        first.took.as_millis(),
        second.made,
        second.toolchain_files_read,
        program_ms,
        second.took.as_secs_f64() * 1e3 / program_ms.max(1e-3),
    );
}

/// Run by [`a_fresh_process_reads_the_world_an_earlier_one_made`] in a child process; on its
/// own (no [`CHILD`] in the environment) it does nothing.
#[test]
fn fixture_cost_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let w = world();
    println!("fixture cost child: made={:?} toolchain_files_read={}", w.made, w.toolchain_files_read);
}

#[test]
fn a_fresh_process_reads_the_world_an_earlier_one_made() {
    // The first: this process, which makes the world if no binary made it before.
    let first = world();
    let exe = std::env::current_exe().expect("this test binary's path");
    let mut child = Command::new(&exe)
        .args(["--exact", "fixture_cost_child", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("starting {}: {e}", exe.display()));
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        match child.try_wait().expect("the child's status") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                child.kill().expect("killing the child");
                panic!("the child `fixture_cost_child` did not finish in {CHILD_TIMEOUT:?}: a world read is well under a second");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let out = child.wait_with_output().expect("the child's output");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "the child failed ({}):\n{stdout}\n{}", out.status, String::from_utf8_lossy(&out.stderr));
    // The harness prints `test fixture_cost_child ... ` before the child's own line with no
    // newline between (observed 2026-11-19), so the report is found inside a line, not at its start.
    let line = stdout
        .lines()
        .find_map(|l| l.find("fixture cost child: ").map(|at| &l[at..]))
        .unwrap_or_else(|| panic!("the child printed no report line (it did not run its construction):\n{stdout}"));
    assert_eq!(
        line, "fixture cost child: made=Read toolchain_files_read=0",
        "a fresh process after a first (which said {:?}) made the world again or parsed toolchain source",
        first.made
    );
    println!("fixture cost: a fresh process: made=Read toolchain_files_read=0");
}

#[test]
fn a_changed_key_makes_the_world_again() {
    // The world made under the real key, so "not made" below is the key's doing, not an
    // empty store's.
    let made = world();
    let alloc = blaze_own::testing::heap().alloc();
    let tc: &'static Toolchain = Box::leak(Box::new(Toolchain::open(&mut DirShelf::new(shelf_dir())).expect("the toolchain shelf")));
    let inputs: &'static Inputs<'static> = Box::leak(Box::new(Inputs::new(user(), tc).expect("the program's crates")));
    let crates: &'static [CrateInput<'static>] = Vec::leak(inputs.crates());
    let toolchain = toolchain_only(crates).expect("the toolchain's crates");
    let row = names_row(alloc).expect("the names producer row");

    let key = world_key(PINNED, &row, toolchain);
    assert_eq!(key, world_key(PINNED, &row, toolchain), "one set of inputs, two keys: the key holds something not of its inputs");
    assert_eq!(key, made.world, "the harness keys the world as `world_key` does");
    assert!(is_made(key, alloc).expect("the store under the real key"), "the world just made is not found under its key");

    // One atom more in core's cfg: an input of every record of core and of all above it.
    let core = toolchain.iter().position(|c| c.name == "core").expect("the toolchain holds core");
    let mut cfg: Vec<(&'static str, Option<&'static str>)> = toolchain[core].cfg.to_vec();
    cfg.push(("blaze_fixture_cost_control", None));
    let mut moved: Vec<CrateInput<'static>> = toolchain
        .iter()
        .map(|c| CrateInput {
            name: c.name,
            key: c.key,
            edition: c.edition,
            implicit: c.implicit,
            root: c.root,
            shown_from: c.shown_from,
            features: c.features,
            deps: c.deps,
            cfg: c.cfg,
            script: None,
        })
        .collect();
    moved[core].cfg = Vec::leak(cfg);
    let other = world_key(PINNED, &row, &moved);
    assert_ne!(key, other, "a toolchain crate's cfg moved and the world's key did not");
    assert!(!is_made(other, alloc).expect("the store under the moved key"), "a world is found under a key no construction made");

    // The producer row is in the key too: another row is another key.
    let mut row_moved = row;
    row_moved[0] ^= 1;
    assert_ne!(key, world_key(PINNED, &row_moved, toolchain), "the names producer moved and the world's key did not");
    println!("fixture cost: a cfg atom more is another key, not made");
}
