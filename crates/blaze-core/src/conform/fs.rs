//! The file system's conformance suite: each case one function over [`Service`], run on the
//! simulated disk ([`Model`], a `unit` test) and on this host's native driver in a scratch
//! directory ([`Real`], a `process` test), by [`suite!`], which declares both from one line.
//!
//! What the model allowed that the real disk does not, each found by a case here and fixed in
//! the model on 2026-11-20 (`sim/host.rs`, `sim.rs`):
//!
//! - a folder vanished with its last file, so `RemoveDir` of it failed and `MakeDir` of it was
//!   done again (`a_folder_outlives_its_last_file`);
//! - a folder a write made was never journaled, so it outlived every crash
//!   (`sim/host/dir_names/tests.rs`, scenario (5));
//! - a file was made under a name that is a file (`a_name_under_a_file`) and where a
//!   directory is (`a_file_where_a_folder_is`);
//! - a stat of a name under a file answered "not there" where `stat(2)` fails `ENOTDIR`;
//! - a stat followed no symbolic link (`a_link_is_followed_by_stat_alone`);
//! - two names one disk folds to one were two (`a_name_spelled_two_ways`): the model now
//!   refuses the second, stricter than both kinds of disk.
//!
//! And what the native driver did that its contract does not say, fixed in `fs.rs` and the
//! ring: a rename of a missing file made the target's folders (`a_rename_of_nothing`), and a
//! folder any effect made was made unsynced (`make_parent`, the folders rule on `Effect`; no
//! case here: see [`a_durable_write_into_new_folders_survives_a_crash`]).

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::PathBuf;

use crate::{DirName, Done, Driver, Effect, FileName, Tag, run_until_idle, sim::{Faults, Sim}};

/// The systems a case's expectation may differ by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Platform {
    /// The simulated disk.
    Model,
    /// macOS's native driver on the temporary folder's volume: APFS, which folds case as
    /// formatted by default.
    MacOs,
    /// Linux's native driver: a file system that keeps case (ext4, btrfs, xfs, tmpfs).
    Linux,
}

/// One answer of the service, as a kind: what it is, never the text of a failure (which
/// differs by system and is not what code acts on).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Answer {
    /// Done, with nothing to give back (written, renamed, made, removed).
    Did,
    /// Failed.
    Refused,
    /// A stat's: a file of this length.
    File(u64),
    /// A stat's: a directory (its length is the system's own and not compared).
    Dir,
    /// A stat's: not there.
    Missing,
    /// A listing's names, each with whether it is a directory, in the order given.
    Listed(Vec<(String, bool)>),
    /// A read's bytes.
    Read(Vec<u8>),
}

/// What a case expects: one value everywhere, or one per platform where systems legitimately
/// differ. Never "skipped".
pub(super) enum Expect {
    Everywhere(Vec<Answer>),
    ByPlatform { model: Vec<Answer>, macos: Vec<Answer>, linux: Vec<Answer> },
}

impl Expect {
    fn on(self, platform: Platform) -> Vec<Answer> {
        match (self, platform) {
            (Expect::Everywhere(it), _) => it,
            (Expect::ByPlatform { model, .. }, Platform::Model) => model,
            (Expect::ByPlatform { macos, .. }, Platform::MacOs) => macos,
            (Expect::ByPlatform { linux, .. }, Platform::Linux) => linux,
        }
    }
}

/// The file system as a case sees it: effects asked through a [`Driver`], and the world
/// changed from outside (a checkout, a person) before them.
pub(super) trait Service {
    /// `effect` run to its end: its answer.
    fn ask(&mut self, effect: Effect) -> Done;
    /// `file` made from outside to hold `bytes`, with its folders, durably.
    fn put(&mut self, file: &str, bytes: &[u8]);
    /// `file` made from outside a symbolic link to `target`, with its folders, durably.
    fn link(&mut self, file: &str, target: &str);
    fn platform(&self) -> Platform;
}

/// The simulated disk, with no faults: what it answers is the model's rule, not a drawn fault.
pub(super) struct Model {
    sim: Sim,
}

impl Model {
    pub(super) fn new() -> Model {
        Model { sim: Sim::new(0x0c0f_f0a1, Faults::default()) }
    }
}

impl Service for Model {
    fn ask(&mut self, effect: Effect) -> Done {
        ended(&mut self.sim, effect)
    }

    fn put(&mut self, file: &str, bytes: &[u8]) {
        self.sim.change(&name(file), bytes);
    }

    fn link(&mut self, file: &str, target: &str) {
        self.sim.symlink(&name(file), target);
    }

    fn platform(&self) -> Platform {
        Platform::Model
    }
}

/// This host's native driver over a scratch directory of the test's own, removed when it
/// drops; the setup from outside is `std::fs` in the same directory.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) struct Real {
    driver: crate::Native,
    // Dropped after the driver, which holds descriptors in it.
    root: crate::temp::TempDir,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Real {
    pub(super) fn new(case: &str) -> Real {
        let root = crate::temp::TempDir::for_test(&format!("blaze-conform-{case}-"));
        let driver = crate::native_in(crate::testing::os(), root.path(), 1)
            .unwrap_or_else(|error| panic!("the native driver over {}: {error}", root.path().display()));
        Real { driver, root }
    }

    fn path(&self, file: &str) -> PathBuf {
        self.root.join(file)
    }

    fn parent_of(&self, file: &str) -> PathBuf {
        let path = self.path(file);
        path.parent().expect("a name under the root").to_path_buf()
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Service for Real {
    fn ask(&mut self, effect: Effect) -> Done {
        ended(&mut self.driver, effect)
    }

    fn put(&mut self, file: &str, bytes: &[u8]) {
        let parent = self.parent_of(file);
        std::fs::create_dir_all(&parent).unwrap_or_else(|error| panic!("{}: {error}", parent.display()));
        let path = self.path(file);
        std::fs::write(&path, bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    }

    fn link(&mut self, file: &str, target: &str) {
        let parent = self.parent_of(file);
        std::fs::create_dir_all(&parent).unwrap_or_else(|error| panic!("{}: {error}", parent.display()));
        let path = self.path(file);
        std::os::unix::fs::symlink(target, &path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    }

    fn platform(&self) -> Platform {
        match cfg!(target_os = "macos") {
            true => Platform::MacOs,
            false => Platform::Linux,
        }
    }
}

/// `effect` run on `driver` until nothing is in flight: its one completion.
fn ended(driver: &mut impl Driver, effect: Effect) -> Done {
    let mut effects = vec![effect];
    let mut seen = None;
    run_until_idle(driver, &mut effects, |done, _| {
        assert!(seen.is_none(), "one effect, two completions: {:?} and {:?}", seen, done.done);
        seen = Some(done.done);
    })
    .expect("the effect ends");
    seen.expect("the effect completed")
}

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap_or_else(|| panic!("{it:?} is no name"))
}

fn dir(it: &str) -> DirName {
    DirName::Named(name(it))
}

fn answer(done: Done) -> Answer {
    match done {
        Done::Failed(_) => Answer::Refused,
        Done::Stat(None) => Answer::Missing,
        Done::Stat(Some(meta)) if meta.dir => Answer::Dir,
        Done::Stat(Some(meta)) => Answer::File(meta.len),
        Done::Listed(listed) => Answer::Listed(
            listed.into_iter().map(|(name, meta)| (name.as_str().to_owned(), meta.dir)).collect(),
        ),
        Done::Read(bytes) => Answer::Read(bytes),
        Done::Written | Done::Renamed | Done::MadeDir | Done::RemovedDir | Done::Removed => Answer::Did,
        other => panic!("the suite asks no effect that ends {other:?}"),
    }
}

// The effects a case asks, each answered as a kind.

fn write(s: &mut dyn Service, file: &str, bytes: &[u8]) -> Answer {
    answer(s.ask(Effect::Write { file: name(file), bytes: bytes.to_vec(), tag: Tag(1) }))
}

fn make_dir(s: &mut dyn Service, it: &str) -> Answer {
    answer(s.ask(Effect::MakeDir { dir: name(it), tag: Tag(1) }))
}

fn remove_dir(s: &mut dyn Service, it: &str) -> Answer {
    answer(s.ask(Effect::RemoveDir { dir: name(it), tag: Tag(1) }))
}

fn remove(s: &mut dyn Service, file: &str) -> Answer {
    answer(s.ask(Effect::Remove { file: name(file), tag: Tag(1) }))
}

fn rename(s: &mut dyn Service, from: &str, to: &str) -> Answer {
    answer(s.ask(Effect::Rename { from: name(from), to: name(to), tag: Tag(1) }))
}

fn list(s: &mut dyn Service, it: DirName) -> Answer {
    answer(s.ask(Effect::List { dir: it, tag: Tag(1) }))
}

fn stat(s: &mut dyn Service, file: &str) -> Answer {
    answer(s.ask(Effect::Stat { file: name(file), tag: Tag(1) }))
}

fn lstat(s: &mut dyn Service, file: &str) -> Answer {
    answer(s.ask(Effect::Lstat { file: name(file), tag: Tag(1) }))
}

fn read(s: &mut dyn Service, file: &str) -> Answer {
    answer(s.ask(Effect::ReadAll { file: name(file), tag: Tag(1) }))
}

fn read_link(s: &mut dyn Service, file: &str) -> Answer {
    answer(s.ask(Effect::ReadLink { file: name(file), tag: Tag(1) }))
}

/// A case: what it saw, and what it expects.
type Case = fn(&mut dyn Service) -> (Vec<Answer>, Expect);

/// The one assert of a case on a service.
fn held(case: Case, s: &mut dyn Service) {
    let platform = s.platform();
    let (seen, expect) = case(s);
    assert_eq!(seen, expect.on(platform), "on {platform:?}");
}

use Answer::{Dir, Did, File, Missing, Refused};

/// A write into a folder that is not there makes it, and the folders above it: the contract
/// of [`Effect::Write`] ("its directories are made"), and what every native driver does
/// (`fs.rs`'s `making_parent`, the ring's `make_parents`). Not refused, as the brief of
/// D167 supposed: the real disk is the reference here, and it makes them.
fn a_write_into_a_missing_folder(s: &mut dyn Service) -> (Vec<Answer>, Expect) {
    let seen = vec![write(s, "a/b/f", b"one"), stat(s, "a"), stat(s, "a/b"), stat(s, "a/b/f")];
    (seen, Expect::Everywhere(vec![Did, Dir, Dir, File(3)]))
}

/// A folder made, then a file written in it; a folder made twice, or under a missing one,
/// refused.
fn a_folder_made_then_written(s: &mut dyn Service) -> (Vec<Answer>, Expect) {
    let seen = vec![
        make_dir(s, "d"),
        write(s, "d/f", b"x"),
        stat(s, "d/f"),
        make_dir(s, "d"),
        make_dir(s, "x/y"),
        stat(s, "x"),
    ];
    (seen, Expect::Everywhere(vec![Did, Did, File(1), Refused, Refused, Missing]))
}

/// An empty folder removed; a full one and one never made refused, the full one kept whole.
fn folders_removed(s: &mut dyn Service) -> (Vec<Answer>, Expect) {
    s.put("full/f", b"kept");
    let seen = vec![
        make_dir(s, "empty"),
        remove_dir(s, "empty"),
        stat(s, "empty"),
        remove_dir(s, "full"),
        stat(s, "full/f"),
        remove_dir(s, "never"),
    ];
    (seen, Expect::Everywhere(vec![Did, Did, Missing, Refused, File(4), Refused]))
}

/// A folder is there until removed: its last file going leaves it, empty, to be listed,
/// removed, and not made again. (The model before 2026-11-20 had it vanish.)
fn a_folder_outlives_its_last_file(s: &mut dyn Service) -> (Vec<Answer>, Expect) {
    s.put("d/f", b"x");
    let seen = vec![
        remove(s, "d/f"),
        stat(s, "d"),
        list(s, dir("d")),
        make_dir(s, "d"),
        remove_dir(s, "d"),
        stat(s, "d"),
    ];
    (seen, Expect::Everywhere(vec![Did, Dir, Answer::Listed(Vec::new()), Refused, Did, Missing]))
}

/// A rename over a file that is there replaces it: the old name gone, the new one holding
/// the moved bytes.
fn a_rename_over_a_file(s: &mut dyn Service) -> (Vec<Answer>, Expect) {
    s.put("a", b"1");
    s.put("b", b"22");
    let seen = vec![rename(s, "a", "b"), stat(s, "a"), read(s, "b")];
    (seen, Expect::Everywhere(vec![Did, Missing, Answer::Read(b"1".to_vec())]))
}

/// A rename into a folder that is not there makes it; the folder it left stays.
fn a_rename_across_folders(s: &mut dyn Service) -> (Vec<Answer>, Expect) {
    s.put("x/a", b"abc");
    let seen = vec![rename(s, "x/a", "y/z/a"), stat(s, "x/a"), stat(s, "y/z/a"), stat(s, "x"), stat(s, "y/z")];
    (seen, Expect::Everywhere(vec![Did, Missing, File(3), Dir, Dir]))
}

/// A rename of a file that is not there fails and makes nothing: no folder of the target.
/// (The native driver before 2026-11-20 made them, then failed.)
fn a_rename_of_nothing(s: &mut dyn Service) -> (Vec<Answer>, Expect) {
    let seen = vec![rename(s, "missing", "q/r"), stat(s, "q")];
    (seen, Expect::Everywhere(vec![Refused, Missing]))
}

/// A listing of a folder not there, and of a file, fails; of an empty folder, lists nothing.
fn listings(s: &mut dyn Service) -> (Vec<Answer>, Expect) {
    s.put("f", b"x");
    let empty = make_dir(s, "e");
    let seen = vec![empty, list(s, dir("nope")), list(s, dir("f")), list(s, dir("e"))];
    (seen, Expect::Everywhere(vec![Did, Refused, Refused, Answer::Listed(Vec::new())]))
}

/// A stat of a name not there is "not there"; of a name under a file, a failure (`ENOTDIR`),
/// not "not there". (The model before 2026-11-20 answered "not there".)
fn a_name_under_a_file(s: &mut dyn Service) -> (Vec<Answer>, Expect) {
    s.put("f", b"x");
    let seen = vec![stat(s, "nope"), stat(s, "f/g"), write(s, "f/g", b"y"), stat(s, "f")];
    (seen, Expect::Everywhere(vec![Missing, Refused, Refused, File(1)]))
}

/// A file is not made where a folder is (`EISDIR`): the folder stays.
fn a_file_where_a_folder_is(s: &mut dyn Service) -> (Vec<Answer>, Expect) {
    let seen = vec![make_dir(s, "d"), write(s, "d", b"x"), stat(s, "d")];
    (seen, Expect::Everywhere(vec![Did, Refused, Dir]))
}

/// A symbolic link: a stat follows it, an lstat and a listing see the link itself, a file of
/// its target's length in bytes, and a read of the link gives the target. One that dangles
/// is "not there" to a stat and still a link to an lstat. What the store relies on: an lstat
/// of a placed file sees a link put there as the wrong length (`Effect::Lstat`).
fn a_link_is_followed_by_stat_alone(s: &mut dyn Service) -> (Vec<Answer>, Expect) {
    s.put("t", b"abc");
    s.link("l", "t");
    s.link("dangling", "nope");
    let seen = vec![
        stat(s, "l"),
        lstat(s, "l"),
        read_link(s, "l"),
        stat(s, "dangling"),
        lstat(s, "dangling"),
        list(s, DirName::Root),
    ];
    let listed = Answer::Listed(vec![("dangling".into(), false), ("l".into(), false), ("t".into(), false)]);
    (seen, Expect::Everywhere(vec![File(3), File(1), Answer::Read(b"t".to_vec()), Missing, File(4), listed]))
}

/// A name spelled two ways: on a disk that folds case (macOS's default APFS) the second
/// spelling is the first name, on one that keeps case (Linux) they are two, and the model,
/// stricter than both, refuses to make the second (what blaze-fetch's tree refuses for the
/// same reason, `tree.rs`).
fn a_name_spelled_two_ways(s: &mut dyn Service) -> (Vec<Answer>, Expect) {
    s.put("Case/a", b"x");
    let seen = vec![write(s, "case/b", b"y"), stat(s, "Case/b"), stat(s, "case/b")];
    let expect = Expect::ByPlatform {
        model: vec![Refused, Missing, Missing],
        macos: vec![Did, File(1), File(1)],
        linux: vec![Did, Missing, File(1)],
    };
    (seen, expect)
}

/// Declares each case twice from one line: `<case>::model::kind_unit` on [`Model`] and
/// `<case>::disk::kind_process` on [`Real`].
macro_rules! suite {
    ($($case:ident),* $(,)?) => {$(
        mod $case {
            blaze_sim::kind_test! {
                unit fn model() {
                    super::held(super::$case, &mut super::Model::new());
                }
            }

            #[cfg(any(target_os = "macos", target_os = "linux"))]
            blaze_sim::kind_test! {
                process fn disk() {
                    super::held(super::$case, &mut super::Real::new(stringify!($case)));
                }
            }
        }
    )*};
}

suite! {
    a_write_into_a_missing_folder,
    a_folder_made_then_written,
    folders_removed,
    a_folder_outlives_its_last_file,
    a_rename_over_a_file,
    a_rename_across_folders,
    a_rename_of_nothing,
    listings,
    a_name_under_a_file,
    a_file_where_a_folder_is,
    a_link_is_followed_by_stat_alone,
    a_name_spelled_two_ways,
}

/// A sync's promise across a crash, on the model only. [`Effect::Write`] promises that once
/// it completes the file survives a crash under its name, the folders it made with it; the
/// model keeps that promise by syncing each made folder's name into its parent, and a crash
/// cut at every point after the write keeps the file (and the control without those syncs
/// loses it: `sim/host/dir_names/tests.rs`, scenario (5), `Plant::MadeParentsNotSynced`).
///
/// What cannot be tested on the real side, and why: the promise is about a power cut or a
/// kernel panic, which a test cannot make in its own process: the page cache it would read
/// back is the same cache whether or not anything reached the device, so a real "after the
/// crash" read sees every name, synced or not. Only the calls are observable: that
/// `make_parent` syncs each folder it made into its parent, after making it and
/// before the file's own rename, is a claim about the order of `mkdir` and `fsync`
/// (`F_FULLFSYNC` on macOS) that a trace of system calls could check and this suite does not;
/// that a device honours `fsync` at all is the device's, and no test's.
fn a_durable_write_into_new_folders_survives_a_crash() {
    let mut model = Model::new();
    let seen = vec![write(&mut model, "a/b/f", b"one")];
    model.sim.crash();
    let after = vec![stat(&mut model, "a"), stat(&mut model, "a/b"), stat(&mut model, "a/b/f")];
    assert_eq!((seen, after), (vec![Did], vec![Dir, Dir, File(3)]));
}

blaze_sim::kind_test! {
    unit fn a_durable_write_survives_a_crash() {
        a_durable_write_into_new_folders_survives_a_crash();
    }
}
