//! The driver on real directories, each test in a fresh one under the system's temporary
//! directory: what it refuses to touch, and the lease races it must lose.

use std::path::{Path, PathBuf};

use crate::{
    fs::{self, TRASH_MARK},
    lease::{self, Lease, Taken},
    plan::{Class, Swept, TRASH},
};

/// A fresh directory of this test's own.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "blaze-gc-{name}-{}-{}",
        std::process::id(),
        crate::record::now_ns()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn names(area: &Path) -> Vec<String> {
    let found = fs::scan(
        &[area.to_string_lossy().into_owned()],
        None,
        u32::MAX,
        None,
        &crate::plan::Clock::default(),
        &mut fs::Sizes::new(),
    )
    .unwrap_or_else(|why| panic!("{why}"));
    found.objects.into_iter().filter(|it| it.class == Class::Trash).map(|it| it.path).collect()
}

#[test]
fn trash_ignores_foreign_entries_and_an_unmarked_trash() {
    let area = scratch("foreign");
    let trash = area.join(TRASH);
    // Without blaze's marker nothing in it is listed.
    std::fs::create_dir_all(trash.join("00000000000000000001-abcdef-x")).unwrap();
    assert!(names(&area).is_empty());
    assert!(fs::trash_dir(&area).unwrap_err().contains(TRASH_MARK));
    // With it, only what blaze names so.
    std::fs::write(trash.join(TRASH_MARK), b"").unwrap();
    std::fs::write(trash.join("notes.txt"), b"mine").unwrap();
    std::fs::create_dir_all(trash.join("photos")).unwrap();
    let listed = names(&area);
    assert_eq!(listed, [trash.join("00000000000000000001-abcdef-x").to_string_lossy()]);
    // An unlink of a foreign entry, or of anything outside a marked trash, is refused.
    assert!(fs::unlink(&trash.join("notes.txt"), Vec::new()).is_err());
    assert!(fs::unlink(&area.join("00000000000000000001-abcdef-x"), Vec::new()).is_err());
    assert!(trash.join("notes.txt").is_file() && trash.join("photos").is_dir());
    assert!(fs::unlink(&trash.join("00000000000000000001-abcdef-x"), Vec::new()).is_ok());
    std::fs::remove_dir_all(&area).unwrap();
}

/// A collector that opened a directory's lease, then lost the race to someone who renamed the
/// directory and made it again, does not rename the new one (review C4).
#[test]
fn a_recreated_lease_is_not_renamed() {
    let area = scratch("recreated");
    let dir = area.join("build").join("w-1");
    drop(Lease::hold(&dir).unwrap_or_else(|why| panic!("{why}")));
    let mut old = std::fs::File::open(lease::lease_of(&dir)).unwrap();
    assert_eq!(lease::verified(&mut old, &dir).unwrap(), lease::Check::Own);
    std::fs::rename(&dir, area.join("away")).unwrap();
    drop(Lease::hold(&dir).unwrap_or_else(|why| panic!("{why}")));
    assert!(
        lease::verified(&mut old, &dir).unwrap() == lease::Check::NotOwn,
        "the old lease file is not the new directory's"
    );
    // The new directory's own lease is taken as its own.
    assert!(matches!(lease::take_exclusive(&dir).unwrap(), Taken::Exclusive(_)));
    std::fs::remove_dir_all(&area).unwrap();
}

#[test]
fn a_held_lease_is_not_trashed() {
    let area = scratch("held");
    let dir = area.join("build").join("w-1");
    let held = Lease::hold(&dir).unwrap_or_else(|why| panic!("{why}"));
    let to = fs::trash_dir(&area).unwrap().join("00000000000000000001-000001-w-1");
    assert_eq!(fs::trash(&dir, &to, Class::Workspace, None), Swept::Leased);
    assert!(dir.is_dir() && !to.exists());
    drop(held);
    assert_eq!(fs::trash(&dir, &to, Class::Workspace, None), Swept::Moved);
    assert!(!dir.exists() && to.is_dir());
    std::fs::remove_dir_all(&area).unwrap();
}

/// A build dir with another build's leased dir inside it (a `--target-dir` named there) is not
/// moved to trash while that build holds its lease: the swept names the dir inside (review
/// round 5, B1).
#[test]
fn a_build_dir_holding_another_builds_leased_dir_is_not_trashed() {
    let area = scratch("nested");
    let dir = area.join("build").join("w-1");
    let inner = dir.join("debug").join("other-target");
    std::fs::create_dir_all(&inner).unwrap();
    lease::make(&dir).unwrap_or_else(|why| panic!("{why}"));
    let held = Lease::hold(&inner).unwrap_or_else(|why| panic!("{why}"));
    let to = fs::trash_dir(&area).unwrap().join("00000000000000000001-000001-w-1");
    let swept = fs::trash(&dir, &to, Class::Workspace, None);
    assert_eq!(swept, Swept::NestedLeased { dir: inner.to_string_lossy().into_owned() });
    assert!(inner.is_dir() && !to.exists());
    drop(held);
    assert_eq!(fs::trash(&dir, &to, Class::Workspace, None), Swept::Moved);
    std::fs::remove_dir_all(&area).unwrap();
}

/// The child's side of the tests below: set, it names the dir whose lease this process takes and
/// holds until its stdin closes.
const HOLD_LEASE: &str = "BLAZE_GC_TEST_HOLD_LEASE";

/// What the child prints once it holds the lease. Matched at a line's end: libtest prints
/// `test <name> ... ` with no newline before a test's own output (review round 8, C1).
const HELD: &str = "blaze-gc-test: lease held";

/// How long the parent waits for the child at each step before it kills it and fails, naming
/// the step (rule bounded-progress).
const CHILD_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// When this process is a test's child ([`HOLD_LEASE`] set): holds that dir's lease until stdin
/// closes, then `true`, for the test to return at once.
fn child_side() -> bool {
    use std::io::Read as _;
    let Some(dir) = std::env::var_os(HOLD_LEASE) else { return false };
    let held = Lease::hold(Path::new(&dir)).unwrap_or_else(|why| panic!("{why}"));
    println!("{HELD}");
    // Held until the parent closes stdin: its end, or the parent's exit.
    let mut rest = Vec::new();
    _ = std::io::stdin().read_to_end(&mut rest);
    drop(held);
    true
}

/// A child process (this test binary, running `test` alone) that holds `dir`'s lease.
struct Holder {
    child: std::process::Child,
    /// The child's stdout, a line at a time, from a reader thread: waited on with a timeout.
    lines: crossbeam_channel::Receiver<String>,
}

impl Holder {
    /// Spawned, and waited for (at most [`CHILD_WAIT`]) until it holds `dir`'s lease.
    fn take(test: &str, dir: &Path) -> Holder {
        use std::io::BufRead as _;
        let filter = format!("fs_tests::{test}");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([filter.as_str(), "--exact", "--nocapture", "--test-threads=1"])
            .env(HOLD_LEASE, dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let out = std::io::BufReader::new(child.stdout.take().unwrap());
        let (send, lines) = crossbeam_channel::unbounded();
        // Reads to the end, so the child's harness never writes into a closed pipe.
        std::thread::spawn(move || {
            for line in out.lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let mut holder = Holder { child, lines };
        holder.wait_for(
            &format!("the child of {test} to take the lease of {}", dir.display()),
            |line| line.ends_with(HELD),
        );
        holder
    }

    /// Lines read until one that `done` accepts, bounded by [`CHILD_WAIT`]: past it the child is
    /// killed and the test fails, naming `step`; the child's output ending first fails it too.
    fn wait_for(&mut self, step: &str, done: impl Fn(&str) -> bool) {
        let until = std::time::Instant::now() + CHILD_WAIT;
        // alloc: a test's child's lines, said on a failure
        let mut seen = Vec::new();
        loop {
            let left = until.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if done(&line) => return,
                Ok(line) => seen.push(line),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    _ = self.child.kill();
                    _ = self.child.wait();
                    panic!(
                        "waiting for {step}: nothing in {CHILD_WAIT:?}, so the child was killed; it printed {seen:?}"
                    );
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    let status = self.child.wait();
                    panic!(
                        "waiting for {step}: the child's output ended ({status:?}); it printed {seen:?}"
                    );
                }
            }
        }
    }

    /// The lease let go: stdin closed, the child's output read to its end (bounded), and its
    /// exit checked.
    fn release(mut self) {
        drop(self.child.stdin.take());
        let until = std::time::Instant::now() + CHILD_WAIT;
        loop {
            let left = until.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(_) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    _ = self.child.kill();
                    _ = self.child.wait();
                    panic!(
                        "waiting for the lease holder to end after its stdin closed: nothing in {CHILD_WAIT:?}, so it was killed"
                    );
                }
            }
        }
        let status = self.child.wait().unwrap();
        assert!(status.success(), "the lease holder ended {status:?}");
    }
}

/// A build in another process that takes a lease inside a build dir after the collector's walk
/// for the leases inside and before its lock (nothing orders a nested build's lease after the
/// outer lock) does not lose its directory: the tree is renamed back, the swept names that dir
/// at its own path, and nothing is in trash. Once that build ends the tree goes (review round 7,
/// C1).
#[test]
fn a_lease_taken_between_the_walk_and_the_lock_moves_the_tree_back() {
    const NAME: &str = "a_lease_taken_between_the_walk_and_the_lock_moves_the_tree_back";
    if child_side() {
        return;
    }
    let area = scratch("window");
    let dir = area.join("build").join("w-1");
    let inner = dir.join("debug").join("other-target");
    std::fs::create_dir_all(&dir).unwrap();
    lease::make(&dir).unwrap_or_else(|why| panic!("{why}"));
    let to = fs::trash_dir(&area).unwrap().join("00000000000000000001-000001-w-1");
    let mut holder = None;
    let swept = fs::trash_between(&dir, &to, Class::Workspace, None, &mut |step| {
        if step == fs::Step::Walked {
            holder = Some(Holder::take(NAME, &inner));
        }
    });
    assert_eq!(swept, Swept::NestedLeased { dir: inner.to_string_lossy().into_owned() });
    assert!(
        inner.join(lease::LEASE).is_file() && !to.exists(),
        "the tree is back at {}",
        dir.display()
    );
    holder.expect("the child ran").release();
    assert_eq!(fs::trash(&dir, &to, Class::Workspace, None), Swept::Moved);
    std::fs::remove_dir_all(&area).unwrap();
}

/// When the tree cannot be renamed back (its path made again meanwhile), it is said as left in
/// trash, not as never moved, and no unlink takes it while the build inside holds its lease;
/// once that build ends it goes (review round 8, M1 and M2).
#[test]
fn a_tree_that_cannot_go_back_stays_in_trash_and_is_not_unlinked_while_leased() {
    const NAME: &str = "a_tree_that_cannot_go_back_stays_in_trash_and_is_not_unlinked_while_leased";
    if child_side() {
        return;
    }
    let area = scratch("not-back");
    let dir = area.join("build").join("w-1");
    let inner = dir.join("debug").join("other-target");
    std::fs::create_dir_all(&dir).unwrap();
    lease::make(&dir).unwrap_or_else(|why| panic!("{why}"));
    let to = fs::trash_dir(&area).unwrap().join("00000000000000000001-000001-w-1");
    let mut holder = None;
    let swept = fs::trash_between(&dir, &to, Class::Workspace, None, &mut |step| match step {
        fs::Step::Walked => holder = Some(Holder::take(NAME, &inner)),
        // Something makes the dir again before the walk of the renamed tree.
        fs::Step::Renamed => {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("new"), b"made again").unwrap();
        }
    });
    let Swept::LeftInTrash { why } = &swept else { panic!("{swept:?}") };
    assert!(why.contains("not back") && why.contains(&to.display().to_string()), "{why}");
    assert!(to.is_dir() && dir.join("new").is_file());
    let moved_inner = to.join("debug").join("other-target");
    let refused = fs::unlink(&to, Vec::new()).unwrap_err();
    assert!(
        refused.contains(&moved_inner.display().to_string()) && refused.contains("holds its lease"),
        "{refused}"
    );
    assert!(moved_inner.join(lease::LEASE).is_file());
    holder.expect("the child ran").release();
    fs::unlink(&to, Vec::new()).unwrap_or_else(|why| panic!("{why}"));
    assert!(!to.exists());
    std::fs::remove_dir_all(&area).unwrap();
}

/// One list refuses what neither `blaze clean` nor a build's mark may touch (review N13): a
/// target dir named inside `CARGO_HOME` (cargo's `git/`, which carries a `CACHEDIR.TAG`), in
/// the daemon's state or a std store, or one that holds `$HOME`, blaze's root or the machine's
/// shared directory, is refused, naming what it would touch; a build dir under `<root>/build`
/// and a project's own target are not.
#[test]
fn a_target_dir_inside_cargo_home_or_the_daemons_state_is_never_removed_or_marked() {
    let top = std::fs::canonicalize(scratch("protected")).unwrap();
    let (home, root, machine) = (top.join("home"), top.join("root"), top.join("machine"));
    let cargo = home.join(".cargo");
    for dir in [cargo.join("git"), root.join("v2"), machine.join("std"), root.join("build")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let protected =
        fs::Protected::new(Some(home.clone()), Some(cargo.clone()), None, &root, Some(&machine))
            .unwrap_or_else(|why| panic!("{why}"));
    let refused = |dir: &Path| protected.refuses(dir).unwrap_or_else(|why| panic!("{why}"));
    for dir in [
        cargo.join("git"),
        cargo.join("registry/cache"),
        root.join("v2/store"),
        machine.join("std/abc"),
        home.clone(),
        root.clone(),
        machine.clone(),
        top.clone(),
        root.join("build/../v2"),
        // blaze's build directories themselves (review round 5, B1).
        root.join("build"),
    ] {
        let why = refused(&dir).unwrap_or_else(|| panic!("{} is not refused", dir.display()));
        assert!(why.contains("never removes or marks"), "{why}");
    }
    assert_eq!(refused(&root.join("build/ws-1")), None);
    assert_eq!(refused(&home.join("src/project/target")), None);
    std::fs::remove_dir_all(&top).unwrap();
}
