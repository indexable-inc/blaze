//! The watcher under the simulation, as each system's journal behaves across a restart
//! (docs/v2/plan/design/restore.md): what changed while blaze was down is found from
//! the saved cursor and column, a lost journal is named and looked at again only where it was,
//! and no file whose stamp did not move, nor one that only moved, is read.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use blaze_core::{
    Completion, Cursor, Done, Driver, Effect, FileName, Journaled, Lost, Mapping, Stamp, Tag,
    sim::{Faults, Journal, Rng, Sim},
};

use super::{Watched, Watcher, cmp_below, is_below, reference};
use crate::{Mode, Op, Plant, Vfs, disk::Disks, section::Section};

/// A watcher under test: today's, or the reference it is held to.
trait Watch {
    fn new(head: &str, dir: FileName, tags: u64) -> Self;
    fn start(&mut self, vfs: &Vfs, effects: &mut Vec<Effect>);
    fn stop(&mut self, effects: &mut Vec<Effect>);
    fn owns(&self, tag: Tag) -> bool;
    fn idle(&self) -> bool;
    fn complete(
        &mut self,
        vfs: &mut Vfs,
        completion: Completion,
        effects: &mut Vec<Effect>,
    ) -> Vec<Watched>;
}

macro_rules! watch {
    ($watcher:ty) => {
        impl Watch for $watcher {
            fn new(head: &str, dir: FileName, tags: u64) -> Self {
                <$watcher>::new(head, dir, tags)
            }
            fn start(&mut self, vfs: &Vfs, effects: &mut Vec<Effect>) {
                <$watcher>::start(self, vfs, effects)
            }
            fn stop(&mut self, effects: &mut Vec<Effect>) {
                <$watcher>::stop(self, effects)
            }
            fn owns(&self, tag: Tag) -> bool {
                <$watcher>::owns(self, tag)
            }
            fn idle(&self) -> bool {
                <$watcher>::idle(self)
            }
            fn complete(
                &mut self,
                vfs: &mut Vfs,
                completion: Completion,
                effects: &mut Vec<Effect>,
            ) -> Vec<Watched> {
                <$watcher>::complete(self, vfs, completion, effects)
            }
        }
    };
}

/// The wall clock these tests give the watcher: far past every stamp the simulation makes (its
/// clock counts changes), so none is racy and the watcher saves what the reference does. The
/// racy window is `tests/stamps.rs`'s, on a disk with a coarse clock.
const STEADY: u64 = u64::MAX;

impl Watch for Watcher {
    fn new(head: &str, dir: FileName, tags: u64) -> Self {
        Watcher::new(head, dir, tags)
    }
    fn start(&mut self, vfs: &Vfs, effects: &mut Vec<Effect>) {
        Watcher::start(self, vfs, STEADY, effects)
    }
    fn stop(&mut self, effects: &mut Vec<Effect>) {
        Watcher::stop(self, effects)
    }
    fn owns(&self, tag: Tag) -> bool {
        Watcher::owns(self, tag)
    }
    fn idle(&self) -> bool {
        Watcher::idle(self)
    }
    fn complete(
        &mut self,
        vfs: &mut Vfs,
        completion: Completion,
        effects: &mut Vec<Effect>,
    ) -> Vec<Watched> {
        Watcher::complete(self, vfs, completion, STEADY, effects)
    }
}

watch!(reference::Watcher);

const KINDS: [Journal; 3] = [Journal::None, Journal::FsEvents, Journal::Usn];
const TAGS: u64 = 1 << 40;

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn ws(path: &str) -> FileName {
    name(&format!("ws/{path}"))
}

/// The watcher, its head, the simulated machine, and what the watcher asked of it.
struct Run<W = Watcher> {
    sim: Sim,
    /// The machine the VFS's journal is written on, apart from the watched one: the journal's
    /// records (the watcher's `SEEN`s among them, which the reference makes none of) never
    /// move what the watched machine draws, so both watchers meet the same disk.
    journal: Sim,
    vfs: Vfs,
    watcher: W,
    effects: Vec<Effect>,
    /// What the disk holds, as the test made it.
    disk: BTreeMap<String, Vec<u8>>,
    /// Every file read, and every name stamped, since the last start.
    reads: Vec<String>,
    stamped: Vec<String>,
    out: Vec<Watched>,
    /// Every effect handed to the machine, the watcher's and the VFS's, in order.
    log: Vec<Effect>,
}

impl Run {
    fn new(seed: u64, kind: Journal, files: &[(&str, &str)]) -> Run {
        Run::with(seed, kind, files)
    }
}

impl<W: Watch> Run<W> {
    fn with(seed: u64, kind: Journal, files: &[(&str, &str)]) -> Run<W> {
        Run::linked(seed, kind, files, &[])
    }

    /// A run that starts with `files` and the symbolic links `links` (path, target) on disk.
    fn linked(seed: u64, kind: Journal, files: &[(&str, &str)], links: &[(&str, &str)]) -> Run<W> {
        let mut sim = Sim::new(seed, Faults::default());
        sim.set_journal(kind);
        let mut run = Run {
            sim,
            journal: Sim::new(seed ^ 0x6a6f_7572, Faults::default()),
            vfs: Vfs::recover(name("state/journal"), 0, &[]).0,
            watcher: W::new("disk", name("ws"), TAGS),
            effects: Vec::new(),
            disk: BTreeMap::new(),
            reads: Vec::new(),
            stamped: Vec::new(),
            out: Vec::new(),
            log: Vec::new(),
        };
        for (path, bytes) in files {
            run.write(path, bytes);
        }
        for (path, target) in links {
            run.link(path, target);
        }
        run.watcher.start(&run.vfs, &mut run.effects);
        run.pump();
        run
    }

    fn write(&mut self, path: &str, bytes: &str) {
        self.sim.change(&ws(path), bytes.as_bytes());
        self.disk.insert(path.into(), bytes.as_bytes().to_vec());
    }

    fn delete(&mut self, path: &str) {
        self.sim.delete(&ws(path));
        self.disk.remove(path);
    }

    /// `path` made a symbolic link to `target`: the head holds it as its target's bytes.
    fn link(&mut self, path: &str, target: &str) {
        self.sim.symlink(&ws(path), target);
        self.disk.insert(path.into(), target.as_bytes().to_vec());
    }

    /// The mode the head has at `path`.
    fn mode(&self, path: &str) -> Option<Mode> {
        let root = self.vfs.root(self.vfs.head("disk")?).expect("a head's root");
        self.vfs.tree.entry(root, path).map(|it| it.1)
    }

    fn rename(&mut self, from: &str, to: &str) {
        self.sim.rename(&ws(from), &ws(to));
        let bytes = self.disk.remove(from).expect("renamed what is there");
        self.disk.insert(to.into(), bytes);
    }

    /// Runs everything in flight, handing each completion to whichever asked for it.
    fn pump(&mut self) {
        self.pump_with(None);
    }

    /// As [`Run::pump`], but each read of a file the watcher asks for is kept in `held`
    /// instead of handed to the machine: in flight, its stamp taken and its bytes not yet
    /// read, while the test changes the disk. [`Run::finish_reads`] does them.
    fn pump_holding_reads(&mut self, held: &mut Vec<Effect>) {
        self.pump_with(Some(held));
    }

    /// Completes the reads `held` against the disk as it is now, as [`Effect::ReadAll`] does
    /// (a file not there reads as empty), before the journal tells anything more: the point
    /// of a transcript where a read falls after a change and before its event. Then pumps.
    fn finish_reads(&mut self, held: &mut Vec<Effect>) {
        for effect in held.drain(..) {
            let Effect::ReadAll { file, tag } = effect else { panic!("held {effect:?}") };
            let path = &file.as_str()["ws/".len()..];
            self.reads.push(path.into());
            let bytes = self.disk.get(path).cloned().unwrap_or_default();
            let done = Done::Read(bytes);
            let out = self.watcher.complete(&mut self.vfs, Completion { tag, done }, &mut self.effects);
            self.out.extend(out);
        }
        self.pump();
    }

    fn pump_with(&mut self, mut held: Option<&mut Vec<Effect>>) {
        let mut done = Vec::new();
        loop {
            if let Some(held) = held.as_deref_mut() {
                let is_read =
                    |it: &Effect| matches!(it, Effect::ReadAll { .. } | Effect::ReadLink { .. });
                held.extend(self.effects.extract_if(.., |it| is_read(it)));
            }
            for effect in &self.effects {
                match effect {
                    Effect::ReadAll { file, .. } | Effect::ReadLink { file, .. } => {
                        self.reads.push(file.as_str()["ws/".len()..].into())
                    }
                    Effect::Stamp { files, .. } => {
                        self.stamped.extend(files.iter().map(str::to_owned))
                    }
                    _ => {}
                }
            }
            let is_journal = |it: &Effect| {
                matches!(
                    it,
                    Effect::Append { .. }
                        | Effect::Sync { .. }
                        | Effect::Write { .. }
                        | Effect::Truncate { .. }
                )
            };
            let mut journal: Vec<Effect> = self.effects.extract_if(.., |it| is_journal(it)).collect();
            self.log.extend(self.effects.iter().cloned());
            self.sim.submit(&mut self.effects);
            self.journal.submit(&mut journal);
            let watched = self.sim.poll(&mut done, blaze_core::wait::Deadline::NEVER);
            let written = self.journal.poll(&mut done, blaze_core::wait::Deadline::NEVER);
            if watched + written == 0 && self.effects.is_empty() {
                return;
            }
            for completion in done.drain(..) {
                if self.vfs.owns(completion.tag) {
                    self.vfs.complete(completion, &mut self.effects);
                } else {
                    assert!(self.watcher.owns(completion.tag), "{completion:?}");
                    let out = self.watcher.complete(&mut self.vfs, completion, &mut self.effects);
                    self.out.extend(out);
                }
            }
        }
    }

    /// The journal tells `files` changed, as one batch.
    fn tell(&mut self, files: &[&str]) {
        let cursor = self.vfs.disks.get("disk").expect("a disk").cursor;
        let mut files: Vec<FileName> = files.iter().map(|it| ws(it)).collect();
        files.sort_unstable();
        files.dedup();
        let told = Journaled { files, lost: Vec::new(), cursor, caught_up: true };
        let journal = Completion { tag: Tag(TAGS), done: Done::Journaled(Box::new(told)) };
        let out = self.watcher.complete(&mut self.vfs, journal, &mut self.effects);
        self.out.extend(out);
    }

    /// The bytes `path` had in each root the watcher committed, in order (`None` where it
    /// was not there).
    fn committed(&self, path: &str) -> Vec<Option<Vec<u8>>> {
        let tree = &self.vfs.tree;
        let commits = self.out.iter().filter_map(|it| match it {
            Watched::Committed(commit) => Some(commit.root),
            _ => None,
        });
        commits
            .map(|root| {
                let root = self.vfs.root(root).expect("a committed root");
                tree.entry(root, path)
                    .map(|(node, _)| tree.bytes(node).expect("not corrupt").expect("a file").to_vec())
            })
            .collect()
    }

    /// blaze stops: the machine keeps its disk and journal, the watch is gone, and the column
    /// goes through the image's bytes (encoded, then read in place). `down` changes the disk
    /// while nothing watches; then a new watcher starts from what was saved.
    fn restart(&mut self, down: impl FnOnce(&mut Run<W>)) {
        assert!(self.watcher.idle(), "stopped with looks in flight: {:?}", self.out);
        self.sim.crash();
        self.journal.crash();
        down(self);
        let mut bytes = Vec::new();
        self.vfs.disks.encode(&mut bytes);
        let len = bytes.len();
        let section = Section::new(Mapping::copy(&bytes), 0..len).expect("a section");
        self.vfs.disks = Disks::open(&section).expect("the column reopens");
        self.watcher = W::new("disk", name("ws"), TAGS);
        (self.reads.clear(), self.stamped.clear(), self.out.clear());
        self.watcher.start(&self.vfs, &mut self.effects);
        self.pump();
    }

    fn head(&self) -> BTreeMap<String, Vec<u8>> {
        let Some(id) = self.vfs.head("disk") else { return BTreeMap::new() };
        let tree = &self.vfs.tree;
        let root = self.vfs.root(id).expect("a head's root");
        tree.files(root, Mode::Regular, "")
            .into_iter()
            .map(|(path, node, _)| {
                (path, tree.bytes(node).expect("not corrupt").expect("a file").to_vec())
            })
            .collect()
    }

    fn reads(&self) -> BTreeSet<&str> {
        self.reads.iter().map(String::as_str).collect()
    }

    fn lost(&self) -> Vec<(&str, Lost)> {
        let lost = self.out.iter().filter_map(|it| match it {
            Watched::Lost { path, why } => Some((path.as_str(), *why)),
            _ => None,
        });
        lost.collect()
    }

    fn check(&self, what: &str) {
        assert_eq!(self.head(), self.disk, "{what}: the head is the disk");
        assert!(self.watcher.idle(), "{what}: {:?}", self.out);
        let odd = self.out.iter().find(|it| {
            !matches!(it, Watched::Committed(_) | Watched::Scanned | Watched::Lost { .. })
        });
        assert!(odd.is_none(), "{what}: {odd:?} in {:?}", self.out);
    }
}

const TREE: &[(&str, &str)] = &[
    ("Cargo.toml", "[package]"),
    ("src/lib.rs", "pub mod a;"),
    ("src/a.rs", "pub fn a() {}"),
    ("src/old/one.rs", "1"),
    ("src/old/two.rs", "2"),
    ("src/old/deep/three.rs", "3"),
    ("tests/t.rs", "#[test] fn t() {}"),
    ("README", "read me"),
];

#[test]
fn a_first_start_reads_the_directory_once() {
    for kind in KINDS {
        let run = Run::new(1, kind, TREE);
        run.check(&format!("{kind:?}"));
        let all: BTreeSet<&str> = TREE.iter().map(|it| it.0).collect();
        assert_eq!(run.reads(), all, "{kind:?}");
        assert!(run.out.contains(&Watched::Scanned), "{kind:?}");
    }
}

/// A symbolic link is an entry whose bytes are its target, as git keeps one, and is never
/// followed: one to a directory (a checkout's `.claude/skills` to `../.agents/skills`) is its
/// target and nothing under it, made while watched or while down, retargeted, or replaced by a
/// file. Today's watcher and the reference alike.
#[test]
fn a_symlink_to_a_directory_is_its_target_never_followed() {
    fn case<W: Watch>(kind: Journal) {
        let what = format!("{kind:?} {}", std::any::type_name::<W>());
        let files = [(".agents/skills/a/SKILL.md", "a"), ("src/lib.rs", "pub fn a() {}")];
        let mut run: Run<W> = Run::with(1, kind, &files);
        run.link(".claude/skills", "../.agents/skills");
        run.link("lib.rs", "src/lib.rs");
        run.pump();
        run.check(&format!("{what}: made while watched"));
        assert_eq!(run.mode(".claude/skills"), Some(Mode::Symlink), "{what}");
        assert_eq!(run.mode("lib.rs"), Some(Mode::Symlink), "{what}");
        assert_eq!(run.mode(".claude/skills/a/SKILL.md"), None, "{what}: not followed");

        run.restart(|run| {
            run.link(".claude/skills", "../.agents");
            run.link("up", "..");
            run.write("lib.rs", "pub fn b() {}");
        });
        run.check(&format!("{what}: changed while down"));
        assert_eq!(run.mode(".claude/skills"), Some(Mode::Symlink), "{what}");
        assert_eq!(
            run.mode("up"),
            Some(Mode::Symlink),
            "{what}: one out of the checkout is kept as it is"
        );
        assert_eq!(run.mode("lib.rs"), Some(Mode::Regular), "{what}: a link replaced by a file");

        let fresh: Run<W> =
            Run::linked(2, kind, &files, &[(".claude/skills", "../.agents/skills")]);
        fresh.check(&format!("{what}: there at the first look"));
        assert_eq!(fresh.mode(".claude/skills"), Some(Mode::Symlink), "{what}");
        assert!(fresh.reads.iter().any(|it| it == ".claude/skills"), "{what}: its target read");
    }
    for kind in KINDS {
        case::<Watcher>(kind);
        case::<reference::Watcher>(kind);
    }
}

/// A symbolic link to a directory that only a listing finds, the journal having named no file
/// in its directory ([`Lost::Unnamed`]: that directory alone is listed), is read as a link and
/// committed as one, never read as a file: one new to the directory and one that replaced a
/// file there alike. Today's watcher and the reference alike.
#[test]
fn a_symlink_to_a_directory_found_by_a_relisting_is_read_as_a_link() {
    fn case<W: Watch>() {
        let what = std::any::type_name::<W>();
        let files = [
            (".agents/skills/a/SKILL.md", "a"),
            (".claude/settings.json", "{}"),
            (".claude/rules", "r"),
        ];
        let mut run: Run<W> = Run::with(7, Journal::Usn, &files);
        run.check(what);
        let cursor = run.vfs.disks.get("disk").expect("a disk").cursor;
        run.watcher.stop(&mut run.effects);
        run.pump();
        assert!(run.out.contains(&Watched::Ended(None)), "{what}");
        (run.reads.clear(), run.stamped.clear(), run.out.clear(), run.log.clear());
        // Changes no watch hears.
        run.link(".claude/skills", "../.agents/skills");
        run.link(".claude/rules", "../.agents");
        let told = Journaled {
            files: Vec::new(),
            lost: vec![(ws(".claude"), Lost::Unnamed)],
            cursor,
            caught_up: true,
        };
        let journal = Completion { tag: Tag(TAGS), done: Done::Journaled(Box::new(told)) };
        let out = run.watcher.complete(&mut run.vfs, journal, &mut run.effects);
        run.out.extend(out);
        assert!(
            matches!(&run.effects[..], [Effect::Entries { dir, .. }] if *dir == ws(".claude")),
            "{what}: {:?}",
            run.effects
        );
        run.pump();
        run.check(what);
        assert_eq!(run.lost(), [(".claude", Lost::Unnamed)], "{what}");
        // Nothing but the two links read, each stamped once more after its read.
        run.stamped.sort_unstable();
        assert_eq!(run.stamped, ["ws/.claude/rules", "ws/.claude/skills"], "{what}");
        for link in [".claude/skills", ".claude/rules"] {
            assert_eq!(run.mode(link), Some(Mode::Symlink), "{what}: {link}");
            let file = ws(link);
            let as_file = run
                .log
                .iter()
                .any(|it| matches!(it, Effect::ReadAll { file: read, .. } if *read == file));
            assert!(!as_file, "{what}: {link} read as a file");
            let as_link = run
                .log
                .iter()
                .any(|it| matches!(it, Effect::ReadLink { file: read, .. } if *read == file));
            assert!(as_link, "{what}: {link}'s target read");
        }
        assert_eq!(run.mode(".claude/skills/a/SKILL.md"), None, "{what}: not followed");
    }
    case::<Watcher>();
    case::<reference::Watcher>();
}

#[test]
fn a_restart_with_nothing_changed_reads_and_commits_nothing() {
    for kind in KINDS {
        for seed in 0..20 {
            let mut run = Run::new(seed, kind, TREE);
            let head = run.vfs.head("disk");
            run.restart(|_| {});
            let what = format!("{kind:?} seed {seed}");
            run.check(&what);
            assert_eq!(run.reads, Vec::<String>::new(), "{what}");
            assert_eq!(run.vfs.head("disk"), head, "{what}");
            assert!(!run.out.iter().any(|it| matches!(it, Watched::Committed(_))), "{what}");
            // Every start, with a journal or not, sweeps the column once: one stamp of every
            // tracked name and nothing else, and no read.
            let swept: BTreeSet<&str> = run.stamped.iter().map(String::as_str).collect();
            assert!(swept.contains("ws/src/old/deep/three.rs"), "{what}: {swept:?}");
            assert!(swept.iter().all(|it| *it == "ws" || it.starts_with("ws/")), "{what}");
            assert_eq!(swept.len(), run.stamped.len(), "{what}: each name once");
        }
    }
}

#[test]
fn what_changed_while_down_is_read_and_what_moved_is_not() {
    for kind in KINDS {
        for seed in 0..20 {
            let mut run = Run::new(seed, kind, TREE);
            run.restart(|run| {
                run.write("src/a.rs", "pub fn a() { changed() }");
                run.write("src/new/b.rs", "pub fn b() {}");
                run.delete("tests/t.rs");
                run.rename("README", "README.md");
                // A directory renamed: each of its files keeps its inode and mtime.
                run.rename("src/old/one.rs", "src/moved/one.rs");
                run.rename("src/old/two.rs", "src/moved/two.rs");
                run.rename("src/old/deep/three.rs", "src/moved/deep/three.rs");
                run.sim.chmod(&ws("Cargo.toml"), 0o600);
            });
            let what = format!("{kind:?} seed {seed}");
            run.check(&what);
            // A chmod moves the stamp, so the file is read; its bytes are the head's, so it
            // makes no op.
            let want = BTreeSet::from(["src/a.rs", "src/new/b.rs", "Cargo.toml"]);
            assert_eq!(run.reads(), want, "{what}");
            assert!(run.lost().is_empty(), "{what}: {:?}", run.lost());
            // What did not change is stamped by the start's sweep alone, journal or not.
            let lib = run.stamped.iter().filter(|it| *it == "ws/src/lib.rs").count();
            assert_eq!(lib, 1, "{what}: {:?}", run.stamped);
            let commits = run.out.iter().filter(|it| matches!(it, Watched::Committed(_))).count();
            assert_eq!(commits, 1, "{what}: what changed while down is one transaction");
        }
    }
}

#[test]
fn a_file_renamed_over_another_is_not_read() {
    for kind in KINDS {
        let mut run = Run::new(3, kind, TREE);
        run.restart(|run| run.rename("src/a.rs", "src/lib.rs"));
        let what = format!("{kind:?}");
        run.check(&what);
        assert_eq!(run.reads, Vec::<String>::new(), "{what}");
        assert_eq!(run.head()["src/lib.rs"], b"pub fn a() {}", "{what}");
    }
}

#[test]
fn a_lost_journal_is_named_and_only_its_subtree_is_swept() {
    for kind in [Journal::FsEvents, Journal::Usn] {
        for seed in 0..10 {
            let what = format!("{kind:?} seed {seed}");
            // The journal reset (FSEvents' store made again, the USN journal deleted): the
            // whole directory is swept by stamps, and only what changed is read.
            let mut run = Run::new(seed, kind, TREE);
            run.restart(|run| {
                run.write("src/lib.rs", "pub mod a; pub mod b;");
                run.sim.reset_journal();
            });
            run.check(&what);
            assert_eq!(run.lost(), [("", Lost::Reset)], "{what}");
            assert_eq!(run.reads(), BTreeSet::from(["src/lib.rs"]), "{what}");

            // The journal no longer reaches the cursor.
            run.restart(|run| {
                run.delete("src/a.rs");
                run.sim.truncate_journal();
                run.write("README", "read me again");
            });
            run.check(&what);
            assert_eq!(run.lost(), [("", Lost::Truncated)], "{what}");
            assert_eq!(run.reads(), BTreeSet::from(["README"]), "{what}");

            // The system says to look at one subtree again: past the start's own sweep of the
            // column (every start's), nothing outside it is stamped.
            let from = run.log.len();
            run.restart(|run| {
                run.write("src/old/deep/three.rs", "three");
                run.sim.lose_changes(&ws("src/old"), Lost::Rescan);
            });
            run.check(&what);
            assert!(run.lost().contains(&("src/old", Lost::Rescan)), "{what}: {:?}", run.lost());
            assert_eq!(run.reads(), BTreeSet::from(["src/old/deep/three.rs"]), "{what}");
            let mut stamps = run.log[from..].iter().filter_map(|it| match it {
                Effect::Stamp { files, .. } => Some(files),
                _ => None,
            });
            let sweep = stamps.next().expect("the start's sweep");
            assert!(sweep.iter().any(|it| it == "ws/README"), "{what}: the start sweeps it all");
            let outside: Vec<&str> =
                stamps.flat_map(|it| it.iter()).filter(|it| !it.starts_with("ws/src/old")).collect();
            assert_eq!(outside, Vec::<&str>::new(), "{what}: stamped outside the lost subtree");
        }
    }
}

#[test]
fn with_no_journal_a_saved_cursor_is_named_lost_and_swept() {
    let mut run = Run::new(5, Journal::FsEvents, TREE);
    // The same disk, now on a system with no journal across a restart.
    run.restart(|run| {
        run.sim.set_journal(Journal::None);
        run.write("src/a.rs", "pub fn a() { linux() }");
    });
    run.check("none");
    assert_eq!(run.lost(), [("", Lost::NoJournal)]);
    assert_eq!(run.reads(), BTreeSet::from(["src/a.rs"]));
}

/// [`Lost::Unnamed`], as NTFS's unprivileged read tells it: the directory alone is listed and
/// diffed against what was in it. Nothing is stamped; only the new file is read, and the
/// renamed one is moved.
#[test]
fn an_unnamed_change_lists_its_directory_alone() {
    let mut run = Run::new(7, Journal::Usn, TREE);
    let cursor = run.vfs.disks.get("disk").expect("a disk").cursor;
    run.watcher.stop(&mut run.effects);
    run.pump();
    assert!(run.out.contains(&Watched::Ended(None)));
    (run.reads.clear(), run.stamped.clear(), run.out.clear());
    // Changes no watch hears.
    run.delete("src/lib.rs");
    run.write("src/c.rs", "pub fn c() {}");
    run.rename("src/a.rs", "src/b.rs");
    run.delete("src/old/one.rs");
    run.delete("src/old/two.rs");
    run.delete("src/old/deep/three.rs");
    let told = Journaled {
        files: Vec::new(),
        lost: vec![(ws("src"), Lost::Unnamed)],
        cursor,
        caught_up: true,
    };
    let journal = Completion { tag: blaze_core::Tag(TAGS), done: Done::Journaled(Box::new(told)) };
    let out = run.watcher.complete(&mut run.vfs, journal, STEADY, &mut run.effects);
    run.out.extend(out);
    assert!(
        matches!(&run.effects[..], [Effect::Entries { dir, .. }] if *dir == ws("src")),
        "{:?}",
        run.effects
    );
    run.pump();
    run.check("unnamed");
    assert_eq!(run.lost(), [("src", Lost::Unnamed)]);
    // Nothing but the one file read, stamped once more after its read.
    assert_eq!(run.stamped, ["ws/src/c.rs"]);
    assert_eq!(run.reads(), BTreeSet::from(["src/c.rs"]));
    assert!(!run.head().keys().any(|it| it.starts_with("src/old")));
}

#[test]
fn a_cursor_is_saved_only_with_what_it_covers() {
    for kind in [Journal::FsEvents, Journal::Usn] {
        let mut run = Run::new(9, kind, TREE);
        let first = run.vfs.disks.get("disk").expect("a disk").cursor;
        assert!(matches!(first, Cursor::Sim { .. }), "{kind:?}: {first:?}");
        run.write("src/live.rs", "live");
        run.pump();
        run.check(&format!("{kind:?} live"));
        let after = run.vfs.disks.get("disk").expect("a disk").cursor;
        assert_ne!(after, first, "{kind:?}: a committed change moves the cursor");
        // A restart from it hears nothing again.
        run.restart(|_| {});
        assert_eq!(run.reads, Vec::<String>::new(), "{kind:?}");
    }
}

/// A watch's looks in flight are slots of its slab: once a rescan's worth has been in flight,
/// another rescan's takes the same slots and gives them back with no allocation, each tag
/// naming its own look.
#[test]
fn looks_in_flight_reuse_their_slots_without_allocating() {
    const N: usize = 4096;
    let mut watcher = Watcher::new("disk", name("ws"), TAGS);
    let mut tags = Vec::with_capacity(N);
    for round in 0..2 {
        let mut ask_and_take = || {
            tags.clear();
            for at in 0..N {
                tags.push(watcher.tag(super::Pending::Read(0, at)));
            }
            assert!(tags.iter().all(|it| watcher.owns(*it)));
            for (at, tag) in tags.iter().enumerate().rev() {
                assert!(matches!(watcher.take(*tag), super::Pending::Read(0, it) if it == at));
            }
        };
        match round {
            0 => ask_and_take(),
            _ => blaze_alloc_count::assert_at_most(0, "a rescan's looks in flight", ask_and_take),
        }
    }
    assert!(watcher.pending.is_empty());
}

/// Random edits, deletes, renames and chmods while blaze is down, for each system, over many
/// seeds and several restarts: the head is always the disk, and a file is read only when it
/// was written or chmodded while down, or renamed onto a path that had a file.
#[test]
fn the_head_follows_the_disk_across_restarts() {
    const PATHS: &[&str] = &["a", "b", "d/x", "d/y", "d/e/z", "f/g/h", "k"];
    for kind in KINDS {
        for seed in 0..100 {
            let mut rng = Rng::new(seed ^ 0x6a75_726e);
            let mut run = Run::new(seed, kind, &[("keep", "k"), ("d/x", "x")]);
            for round in 0..4 {
                let mut touched = BTreeSet::new();
                let mut steps = Vec::new();
                let mut disk = run.disk.clone();
                for step in 0..1 + rng.below(8) {
                    let path = PATHS[rng.below(PATHS.len() as u64) as usize];
                    let there = disk.contains_key(path);
                    let free = PATHS.iter().find(|it| {
                        !disk.contains_key(**it)
                            && !disk.keys().any(|k| {
                                k.starts_with(&format!("{it}/")) || it.starts_with(&format!("{k}/"))
                            })
                    });
                    match rng.below(4) {
                        0 if there => {
                            disk.remove(path);
                            steps.push((0, path.to_owned(), String::new()));
                        }
                        1 if there && free.is_some() => {
                            let to = free.expect("checked").to_string();
                            let bytes = disk.remove(path).expect("there");
                            disk.insert(to.clone(), bytes);
                            // A file written or chmodded before its rename is read where it went;
                            // so is one renamed onto a path that had a file at the start (its
                            // change time moved, as it does for a file moved away and back).
                            if touched.remove(path) || run.disk.contains_key(&to) {
                                touched.insert(to.clone());
                            }
                            steps.push((1, path.to_owned(), to));
                        }
                        2 if there => {
                            touched.insert(path.to_owned());
                            steps.push((2, path.to_owned(), String::new()));
                        }
                        _ if disk.keys().any(|k| {
                            path.starts_with(&format!("{k}/")) || k.starts_with(&format!("{path}/"))
                        }) => {}
                        _ => {
                            let bytes = format!("{seed}:{round}:{step}");
                            disk.insert(path.into(), bytes.clone().into_bytes());
                            touched.insert(path.to_owned());
                            steps.push((3, path.to_owned(), bytes));
                        }
                    }
                }
                run.restart(|run| {
                    for (op, path, arg) in steps {
                        match op {
                            0 => run.delete(&path),
                            1 => run.rename(&path, &arg),
                            2 => run.sim.chmod(&ws(&path), 0o600 + (round as u32 % 2)),
                            _ => run.write(&path, &arg),
                        }
                    }
                });
                let what = format!("{kind:?} seed {seed} round {round}");
                run.check(&what);
                let extra: Vec<&str> =
                    run.reads().into_iter().filter(|it| !touched.contains(*it)).collect();
                assert_eq!(extra, Vec::<&str>::new(), "{what}: read but not changed");
            }
        }
    }
}

/// A change `act` makes: a delete (0), a rename to `arg` (1), a chmod (2) or a write of `arg` (3).
type Step = (u8, String, String);

/// Up to 8 random changes among `PATHS` to a disk holding `disk`, each only where the disk
/// allows it.
fn steps(rng: &mut Rng, disk: &BTreeMap<String, Vec<u8>>, mark: &str) -> Vec<Step> {
    const PATHS: &[&str] =
        &["a", "b", "d/x", "d/y", "d/e/z", "f/g/h", "k", "src/old/one.rs", "src/new.rs"];
    let clash = |disk: &BTreeMap<String, Vec<u8>>, path: &str| {
        disk.keys().any(|k| is_below(path, k) || is_below(k, path))
    };
    let mut disk = disk.clone();
    let mut steps = Vec::new();
    for step in 0..1 + rng.below(8) {
        let path = PATHS[rng.below(PATHS.len() as u64) as usize];
        let there = disk.contains_key(path);
        match rng.below(4) {
            0 if there => {
                disk.remove(path);
                steps.push((0, path.to_owned(), String::new()));
            }
            1 if there => {
                let Some(to) =
                    PATHS.iter().find(|it| !disk.contains_key(**it) && !clash(&disk, it))
                else {
                    continue;
                };
                let bytes = disk.remove(path).expect("there");
                disk.insert((*to).to_owned(), bytes);
                steps.push((1, path.to_owned(), (*to).to_owned()));
            }
            2 if there => steps.push((2, path.to_owned(), String::new())),
            _ if !there && clash(&disk, path) => {}
            _ => {
                let bytes = format!("{mark}:{step}");
                disk.insert(path.to_owned(), bytes.clone().into_bytes());
                steps.push((3, path.to_owned(), bytes));
            }
        }
    }
    steps
}

/// Makes `steps` on `run`'s disk the way `how` says: live under the watch (0), while blaze is
/// down (1), while down with the journal losing a subtree (2), or with the journal reset (3).
fn act<W: Watch>(run: &mut Run<W>, steps: &[Step], how: u64, kind: Journal) {
    let apply = |run: &mut Run<W>| {
        for (op, path, arg) in steps {
            match op {
                0 => run.delete(path),
                1 => run.rename(path, arg),
                2 => run.sim.chmod(&ws(path), 0o600 + (run.log.len() as u32 % 2)),
                _ => run.write(path, arg),
            }
        }
    };
    match how {
        0 => {
            apply(run);
            run.pump();
        }
        1 => run.restart(apply),
        2 if kind != Journal::None => run.restart(|run| {
            apply(run);
            run.sim.lose_changes(&ws("src"), Lost::Rescan);
        }),
        _ if kind != Journal::None => run.restart(|run| {
            apply(run);
            run.sim.reset_journal();
        }),
        _ => run.restart(apply),
    }
}

/// Today's watcher, whose names are interned, and the reference it was rewritten from, each on
/// its own machine with the same seed and told the same changes (live, across restarts, with a
/// subtree lost or the journal reset): every effect either asks for, every output and every
/// head is the same. The column's own root, its `CHOSE` stamps and its `SEEN` records are
/// today's alone and change no effect here: with no other writer every saved stamp vouches,
/// the reference's rescans force every file of the head where today's take only the diff since
/// the column's root (a forced name whose stamp is the saved one settles with no effect), and
/// no step puts a file above a written path or over a directory.
#[test]
fn the_watcher_does_what_the_reference_did() {
    for kind in KINDS {
        for seed in 0..30 {
            let mut rng = Rng::new(seed ^ 0x7061_7468);
            let mut new: Run = Run::with(seed, kind, TREE);
            let mut old: Run<reference::Watcher> = Run::with(seed, kind, TREE);
            for round in 0..8 {
                let what = format!("{kind:?} seed {seed} round {round}");
                assert_eq!(new.log, old.log, "{what}: the same effects, in order");
                assert_eq!(new.out, old.out, "{what}: the same outputs");
                assert_eq!(new.reads, old.reads, "{what}: the same reads");
                assert_eq!(new.vfs.head("disk"), old.vfs.head("disk"), "{what}: the same head");
                assert_eq!(new.disk, old.disk);
                assert_eq!(new.watcher.idle(), old.watcher.idle(), "{what}");
                (new.log.clear(), old.log.clear());
                let steps = steps(&mut rng, &new.disk, &what);
                let how = rng.below(4);
                act(&mut new, &steps, how, kind);
                act(&mut old, &steps, how, kind);
            }
        }
    }
}

/// Once warm (its names' room, a spare batch and the stamp buffer the driver gave back kept), a
/// batch the journal tells of files that did not change is stamped and settled with no
/// allocation at all; one of files whose stamps moved allocates only the name each read hands
/// the driver, which takes it owned.
#[test]
fn a_warm_batch_allocates_only_the_names_its_reads_hand_the_driver() {
    use blaze_alloc_count::assert_at_most;

    assert!(blaze_alloc_count::installed());
    let mut run = Run::new(11, Journal::Usn, TREE);
    run.check("start");
    let told: Vec<FileName> = TREE.iter().map(|(path, _)| ws(path)).collect();
    let journaled = |run: &Run| {
        let cursor = run.vfs.disks.get("disk").expect("a disk").cursor;
        let told = Journaled { files: told.clone(), lost: Vec::new(), cursor, caught_up: true };
        Completion { tag: Tag(TAGS), done: Done::Journaled(Box::new(told)) }
    };
    // The stamps of the batch the watcher just asked for: what the column saved of each name,
    // its mtime moved by `moved`. Its buffer is dropped, as a driver drops it once done.
    let stamps = |run: &mut Run, moved: u64| {
        let Some(Effect::Stamp { files, tag }) = run.effects.pop() else {
            panic!("{:?}", run.effects)
        };
        assert!(run.effects.is_empty(), "{:?}", run.effects);
        let disk = run.vfs.disks.get("disk").expect("a disk");
        let stamps: Vec<Stamp> = files
            .iter()
            .map(|name| {
                let saved = disk.get(name).expect("a file the column has");
                Stamp { mtime_ns: saved.mtime_ns + moved, ..saved }
            })
            .collect();
        drop(files);
        Completion { tag, done: Done::Stamped(stamps) }
    };
    for round in 0..3u64 {
        let budget = |warm: u64| if round == 0 { u64::MAX } else { warm };

        // Nothing changed: stamped, settled, no read and no commit.
        let completion = journaled(&run);
        let out = assert_at_most(budget(0), "a batch of unchanged files told", || {
            run.watcher.complete(&mut run.vfs, completion, STEADY, &mut run.effects)
        });
        assert!(out.is_empty(), "{out:?}");
        let completion = stamps(&mut run, 0);
        let out = assert_at_most(budget(0), "their stamps settled", || {
            run.watcher.complete(&mut run.vfs, completion, STEADY, &mut run.effects)
        });
        assert!(out.is_empty(), "{out:?}");
        assert!(run.effects.is_empty(), "nothing read or written: {:?}", run.effects);
        assert!(run.watcher.idle());

        // Every stamp moved (the same inode, a later mtime): each file is read.
        let completion = journaled(&run);
        assert_at_most(budget(0), "a batch of changed files told", || {
            run.watcher.complete(&mut run.vfs, completion, STEADY, &mut run.effects)
        });
        let completion = stamps(&mut run, 1);
        let Done::Stamped(moved) = &completion.done else { unreachable!() };
        let n = moved.len() as u64;
        assert_at_most(budget(n), "their stamps, a read asked for each", || {
            run.watcher.complete(&mut run.vfs, completion, STEADY, &mut run.effects)
        });
        assert_eq!(
            run.effects.iter().filter(|it| matches!(it, Effect::ReadAll { .. })).count() as u64,
            n
        );
        // Each read done with the bytes the disk has; with the last, the files read are
        // stamped once more, in the batch's own buffer: no allocation.
        let reads: Vec<Completion> = run
            .effects
            .drain(..)
            .map(|effect| {
                let Effect::ReadAll { file, tag } = effect else { panic!("{effect:?}") };
                let bytes = run.disk[&file.as_str()["ws/".len()..]].clone();
                Completion { tag, done: Done::Read(bytes) }
            })
            .collect();
        assert_at_most(budget(0), "their reads, and their stamps asked for again", || {
            for completion in reads {
                let out = run.watcher.complete(&mut run.vfs, completion, STEADY, &mut run.effects);
                assert!(out.is_empty(), "{out:?}");
            }
        });
        // The stamps taken again are those taken before the reads: the bytes are committed.
        let completion = stamps(&mut run, 1);
        run.watcher.complete(&mut run.vfs, completion, STEADY, &mut run.effects);
        run.pump();
        assert!(run.watcher.idle());
        assert_eq!(run.head(), run.disk, "the same bytes read make no change");
    }
}

const OLD: &str = "fn main() { old() }";
const NEW: &str = "fn main() { the_new_one() }";
const TEMP: &str = "src/.main.rs.tmp";
const MAIN: &str = "src/main.rs";

/// A run whose journal tells nothing by itself (the machine's watch is stopped once the
/// start's sweep is in, as the tests of changes no watch hears do): the test tells each batch
/// ([`Run::tell`]), so a transcript says exactly which names a batch holds and where each
/// read falls.
fn transcript<W: Watch>(seed: u64, files: &[(&str, &str)]) -> Run<W> {
    let mut run: Run<W> = Run::with(seed, Journal::Usn, files);
    run.check("start");
    run.watcher.stop(&mut run.effects);
    run.pump();
    assert!(run.out.contains(&Watched::Ended(None)));
    run.out.clear();
    run
}

/// Every commit of `run` held `path`, where it had it, as one of `had`.
fn only_ever<W: Watch>(run: &Run<W>, path: &str, had: &[&[u8]]) -> Result<(), String> {
    for bytes in run.committed(path).into_iter().flatten() {
        if !had.contains(&&bytes[..]) {
            return Err(format!(
                "a commit held {path} as {:?}, which it never was",
                String::from_utf8_lossy(&bytes)
            ));
        }
    }
    Ok(())
}

/// How the journal tells an editor's rename of a temporary file over `src/main.rs`, after the
/// batch that told of the temporary file: FSEvents names both paths in one batch (seen
/// 2026-11-11), inotify and fanotify tell a move as two events (from, to), which the watch's
/// reads of them may hand on as one batch or two, in the order they happened; the last is two
/// batches the other way round, which no kernel tells and the watcher must still stand.
const TOLD: [&[&[&str]]; 4] =
    [&[&[TEMP, MAIN]], &[&[TEMP], &[MAIN]], &[&[TEMP], &[TEMP, MAIN]], &[&[MAIN], &[TEMP]]];

/// The transcript seen on macOS on 2026-11-11 (`blaze run` at once after `sed -i ''` or any
/// write-then-rename: E0601 "`main` function not found", until the file was written again),
/// under a watcher that saves a file's first stamp whatever its read met (`trusts_reads`) or
/// today's: the journal tells of the temporary file; it is stamped (there, with the new
/// bytes); the editor renames it over `src/main.rs`; its read is done, by name, of a file no
/// longer there, which `ReadAll` gives as empty; the journal tells the rename as `told` says.
/// `Err` says what a commit held that the file never was, or that the head did not end as the
/// disk is.
fn renamed_over<W: Watch>(
    seed: u64,
    told: &[&[&str]],
    plant: impl FnOnce(&mut Run<W>),
) -> Result<(), String> {
    let mut run: Run<W> = transcript(seed, &[("Cargo.toml", "[package]"), (MAIN, OLD)]);
    plant(&mut run);
    run.write(TEMP, NEW);
    run.tell(&[TEMP]);
    let mut held = Vec::new();
    run.pump_holding_reads(&mut held);
    assert_eq!(held.len(), 1, "the temporary file is stamped and its read asked for");
    run.rename(TEMP, MAIN);
    run.finish_reads(&mut held);
    for batch in told {
        run.tell(batch);
        if seed % 2 == 0 {
            run.pump();
        }
    }
    run.pump();
    only_ever(&run, MAIN, &[OLD.as_bytes(), NEW.as_bytes()])?;
    match run.head() == run.disk && run.watcher.idle() {
        true => Ok(()),
        false => Err(format!("the head is {:?} and the disk {:?}", run.head(), run.disk)),
    }
}

/// The invariant: a file an editor replaces by a rename is seen as what it was or what it is,
/// never as empty, whichever way the journal tells the rename and wherever the read of the
/// temporary file falls; and once everything told is looked at, the head is the disk.
#[test]
fn a_file_renamed_over_another_between_its_stamp_and_its_read_is_never_read_as_empty() {
    for (order, told) in TOLD.iter().enumerate() {
        for seed in 0..10 {
            for (who, got) in [
                ("the watcher", renamed_over::<Watcher>(seed, told, |_| {})),
                ("the reference", renamed_over::<reference::Watcher>(seed, told, |_| {})),
            ] {
                assert_eq!(got, Ok(()), "{who}, told as {order}, seed {seed}");
            }
        }
    }
}

/// The control: a watcher that saves a file's first stamp whatever its read met, as until
/// 2026-11-11, commits `src/main.rs` empty when the journal names both paths of the rename
/// in one batch, as FSEvents does.
#[test]
fn the_oracle_catches_a_stamp_saved_for_a_read_that_found_the_file_renamed_away() {
    for seed in 0..10 {
        let got = renamed_over::<Watcher>(seed, TOLD[0], |run| run.watcher.trusts_reads = true);
        let why = got.expect_err("the planted bug was not caught");
        assert!(why.contains("src/main.rs as \"\""), "seed {seed}: caught as {why}");
    }
}

/// The transcripts of the other ways a path changes between its stamp and its read: each ends
/// with the head as the disk is and no commit of bytes the file never had. `told` is what the
/// journal names for the change.
#[test]
fn a_path_that_changes_between_its_stamp_and_its_read_is_looked_at_again() {
    type Change = fn(&mut Run);
    const FIRST: &str = "fn main() { a_first_edit() }";
    let changes: [(&str, Change, &[&str]); 5] = [
        ("deleted", |run| run.delete(MAIN), &[MAIN]),
        (
            "deleted and made again",
            |run| {
                run.delete(MAIN);
                run.write(MAIN, NEW);
            },
            &[MAIN],
        ),
        ("renamed away", |run| run.rename(MAIN, "src/moved.rs"), &[MAIN, "src/moved.rs"]),
        (
            "replaced by a rename",
            |run| {
                run.write(TEMP, NEW);
                run.rename(TEMP, MAIN);
            },
            &[TEMP, MAIN],
        ),
        (
            "written in place",
            |run| {
                run.sim.advance(Duration::from_millis(1));
                run.write(MAIN, NEW);
            },
            &[MAIN],
        ),
    ];
    for (name, change, told) in changes {
        for seed in 0..10 {
            let what = format!("{name}, seed {seed}");
            let mut run: Run = transcript(seed, &[(MAIN, OLD)]);
            // A first write, told and stamped; its read is in flight when the path changes.
            run.sim.advance(Duration::from_millis(1));
            run.write(MAIN, FIRST);
            run.tell(&[MAIN]);
            let mut held = Vec::new();
            run.pump_holding_reads(&mut held);
            assert_eq!(held.len(), 1, "{what}");
            change(&mut run);
            run.finish_reads(&mut held);
            run.tell(told);
            run.pump();
            let had = [OLD.as_bytes(), FIRST.as_bytes(), NEW.as_bytes()];
            assert_eq!(only_ever(&run, MAIN, &had), Ok(()), "{what}");
            run.check(&what);
        }
    }
}

/// A file that really is empty is read as empty: one made empty, and one truncated in place.
#[test]
fn an_empty_file_is_read_as_empty() {
    let mut run: Run = transcript(3, &[(MAIN, OLD)]);
    run.write("src/empty.rs", "");
    run.tell(&["src/empty.rs"]);
    run.pump();
    run.check("made empty");
    assert_eq!(run.head()["src/empty.rs"], b"");
    run.sim.advance(Duration::from_millis(1));
    run.write(MAIN, "");
    run.tell(&[MAIN]);
    run.pump();
    run.check("truncated");
    assert_eq!(run.head()[MAIN], b"");
}

/// One run of an editor against the watcher: `src/main.rs` replaced by a rename `rounds`
/// times (a temporary file written, then renamed over it), each version its own text. After
/// each of the editor's steps `rng` picks what happens before the next: the journal tells
/// some of what changed so far (several changes as one batch, as FSEvents hands them, or one
/// each, as inotify does), the watcher's looks run with their reads held in flight, or the
/// reads held are done. After half the replacements a request is made: everything is told
/// and looked at, and the head must then be the disk, which is what the request builds.
/// `Err` names a request that would build something else, or a commit that held
/// `src/main.rs` with bytes it never had.
fn replaced_by_renames(seed: u64, rounds: u64, trusts_reads: bool) -> Result<(), String> {
    let mut rng = Rng::new(seed ^ 0x7265_706c);
    let mut run: Run = transcript(seed, &[(MAIN, "fn main() { v0() }")]);
    run.watcher.trusts_reads = trusts_reads;
    let mut versions = vec![b"fn main() { v0() }".to_vec()];
    let mut held = Vec::new();
    // What changed and the journal has not told yet, in the order it happened.
    let mut untold: Vec<String> = Vec::new();
    fn tell_some(run: &mut Run, untold: &mut Vec<String>, rng: &mut Rng) {
        if untold.is_empty() {
            return;
        }
        let n = 1 + rng.below(untold.len() as u64) as usize;
        let batch: Vec<String> = untold.drain(..n).collect();
        let batch: Vec<&str> = batch.iter().map(String::as_str).collect();
        run.tell(&batch);
    }
    let mut step = |run: &mut Run, held: &mut Vec<Effect>, untold: &mut Vec<String>| {
        match rng.below(5) {
            0 => {
                tell_some(run, untold, &mut rng);
                run.pump_holding_reads(held);
            }
            1 => {
                tell_some(run, untold, &mut rng);
                run.finish_reads(held);
            }
            2 => run.finish_reads(held),
            3 => run.pump_holding_reads(held),
            _ => {}
        }
        rng.below(2) == 0
    };
    for round in 1..=rounds {
        let text = format!("fn main() {{ v{round}() }}");
        let temp = format!("src/.main.rs.tmp{round}");
        run.sim.advance(Duration::from_millis(1));
        run.write(&temp, &text);
        untold.push(temp.clone());
        step(&mut run, &mut held, &mut untold);
        run.rename(&temp, MAIN);
        untold.extend([temp, MAIN.to_owned()]);
        versions.push(text.into_bytes());
        let request = step(&mut run, &mut held, &mut untold);
        if request || round == rounds {
            while !untold.is_empty() {
                let n = 1 + rng_below(seed, round, untold.len());
                let batch: Vec<String> = untold.drain(..n).collect();
                let batch: Vec<&str> = batch.iter().map(String::as_str).collect();
                run.tell(&batch);
            }
            run.finish_reads(&mut held);
            if run.head() != run.disk || !run.watcher.idle() {
                return Err(format!(
                    "round {round}: a request would build src/main.rs as {:?}, and the disk has {:?}",
                    run.head().get(MAIN).map(|it| String::from_utf8_lossy(it).into_owned()),
                    String::from_utf8_lossy(&run.disk[MAIN])
                ));
            }
        }
    }
    let versions: Vec<&[u8]> = versions.iter().map(Vec::as_slice).collect();
    only_ever(&run, MAIN, &versions)
}

/// How many of `len` untold changes a request's batch takes, from the seed and the round.
fn rng_below(seed: u64, round: u64, len: usize) -> usize {
    Rng::new(seed.wrapping_mul(0x9e37_79b9).wrapping_add(round)).below(len as u64) as usize
}

/// The invariant: whatever falls between a file's stamp and its read, a file replaced by a
/// rename is seen as what it was or what it is, never as anything else, and a request made
/// after a replacement builds what the disk has. 500 replacements a seed, with a request
/// after about half of them.
#[test]
fn a_file_replaced_by_renames_500_times_is_always_one_of_its_versions() {
    for seed in 0..16 {
        if let Err(why) = replaced_by_renames(seed, 500, false) {
            panic!("seed {seed}: {why}");
        }
    }
}

/// The control of [`a_file_replaced_by_renames_500_times_is_always_one_of_its_versions`]: a
/// watcher that saves a file's first stamp whatever its read met is caught in every seed.
#[test]
fn the_oracle_catches_a_request_that_builds_what_the_disk_never_had() {
    for seed in 0..16 {
        let why = replaced_by_renames(seed, 500, true).expect_err("the planted bug was not caught");
        assert!(why.contains("src/main.rs as"), "seed {seed}: {why}");
    }
}

/// The same on this machine's own kernel and journal (FSEvents on macOS, fanotify on Linux),
/// through the driver the daemon runs: `src/main.rs` is replaced by a rename 500 times, the
/// rename held back in two rounds of three until the watcher has stamped the temporary file
/// and asked for its read, so that its look falls across the rename, and a request follows
/// each at once (a flush
/// of the watch, then everything told looked at, as the daemon holds a request). No commit
/// may hold `src/main.rs` as anything but the version before or the new one, and the request
/// must see the new one.
#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn a_file_replaced_by_renames_500_times_on_this_kernel_is_always_one_of_its_versions() {
    use std::time::Instant;

    use blaze_core::wait::Deadline;

    const FLUSHES: u64 = TAGS + (1 << 33);
    let root = std::env::temp_dir().join(format!("blaze-vfs-renames-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("ws/src")).unwrap();
    std::fs::create_dir_all(root.join("state")).unwrap();
    let main = root.join("ws/src/main.rs");
    std::fs::write(&main, "fn main() { v0() }").unwrap();

    let mut driver = blaze_core::native(&root, 2).unwrap();
    let mut vfs = Vfs::recover(name("state/journal"), 0, &[]).0;
    let mut watcher = Watcher::new("disk", name("ws"), TAGS);
    let mut effects = Vec::new();
    watcher.start(&vfs, wall_ns(), &mut effects);
    let read = |vfs: &Vfs| -> Option<Vec<u8>> {
        let root = vfs.root(vfs.head("disk")?).expect("a head's root");
        let (node, _) = vfs.tree.entry(root, MAIN)?;
        Some(vfs.tree.bytes(node).expect("not corrupt").expect("a file").to_vec())
    };
    // One turn of the driver: what was asked is handed on, what is done within `wait` is
    // taken in. `allowed` is what `src/main.rs` may be in a commit made now. Returns the
    // flush told, if one was.
    let mut done = Vec::new();
    let mut turn = |driver: &mut blaze_core::Native,
                    vfs: &mut Vfs,
                    watcher: &mut Watcher,
                    effects: &mut Vec<Effect>,
                    wait: Duration,
                    allowed: [&str; 2],
                    round: u64|
     -> (Option<u64>, bool) {
        driver.submit(effects);
        driver.poll(&mut done, Deadline::after(driver.now(), wait));
        let (mut flushed, mut scanned) = (None, false);
        for completion in done.drain(..) {
            if vfs.owns(completion.tag) {
                vfs.complete(completion, effects);
            } else if watcher.owns(completion.tag) {
                for out in watcher.complete(vfs, completion, wall_ns(), effects) {
                    match out {
                        Watched::Committed(_) => {
                            let bytes = read(vfs).expect("src/main.rs is never deleted");
                            assert!(
                                allowed.iter().any(|it| it.as_bytes() == bytes),
                                "round {round}: a commit held src/main.rs as {:?}, which is neither {allowed:?}",
                                String::from_utf8_lossy(&bytes)
                            );
                        }
                        Watched::Scanned => scanned = true,
                        Watched::Lost { .. } => {}
                        other => panic!("round {round}: {other:?}"),
                    }
                }
            } else {
                assert_eq!(completion.done, Done::Flushed, "round {round}: {completion:?}");
                flushed = Some(completion.tag.0);
            }
        }
        (flushed, scanned)
    };
    // Bounded: a wait that does not end fails naming what it waited for.
    let within = |began: Instant, what: &str| {
        assert!(began.elapsed() < Duration::from_secs(60), "60 s and still waiting for {what}");
    };
    let began = Instant::now();
    let mut scanned = false;
    while !(scanned && watcher.idle()) {
        within(began, "the start's sweep");
        let wait = Duration::from_millis(20);
        scanned |= turn(&mut driver, &mut vfs, &mut watcher, &mut effects, wait, ["fn main() { v0() }"; 2], 0).1;
    }
    let mut last = String::from("fn main() { v0() }");
    // The rounds whose rename fell between the temporary file's stamp and its read.
    let mut crossed = 0u64;
    for round in 1..=500u64 {
        let text = format!("fn main() {{ v{round}() }}");
        let temp = root.join(format!("ws/src/.main.rs.tmp{round}"));
        std::fs::write(&temp, &text).unwrap();
        // Two rounds in three, the rename waits (up to 200 ms) until the watcher has stamped
        // the temporary file and asked for its read, which is then done after the rename:
        // the interleaving that read it as empty. The third renames at once.
        let reads_temp = |effects: &[Effect]| {
            effects.iter().any(|it| matches!(it, Effect::ReadAll { file, .. } if file.as_str().contains(".tmp")))
        };
        let began = Instant::now();
        while round % 3 != 0 && !reads_temp(&effects) && began.elapsed() < Duration::from_millis(200) {
            let wait = Duration::from_millis(1);
            turn(&mut driver, &mut vfs, &mut watcher, &mut effects, wait, [&last, &text], round);
        }
        crossed += u64::from(reads_temp(&effects));
        std::fs::rename(&temp, &main).unwrap();
        // The request, at once: the watch flushed, and everything told before it looked at.
        effects.push(Effect::Flush { watch: Tag(TAGS), tag: Tag(FLUSHES + round) });
        let (began, mut flushed) = (Instant::now(), false);
        while !(flushed && watcher.idle()) {
            within(began, &format!("round {round}'s request"));
            let wait = Duration::from_millis(20);
            let told = turn(&mut driver, &mut vfs, &mut watcher, &mut effects, wait, [&last, &text], round).0;
            flushed |= told == Some(FLUSHES + round);
        }
        let got = read(&vfs).expect("src/main.rs is never deleted");
        assert_eq!(String::from_utf8_lossy(&got), text, "round {round}: what a request builds");
        last = text;
    }
    watcher.stop(&mut effects);
    driver.submit(&mut effects);
    drop(driver);
    _ = std::fs::remove_dir_all(&root);
    assert!(crossed >= 100, "only {crossed} of 500 renames fell between a stamp and its read");
}

/// `cmp_below` is the order of a name against a directory and a `/`, and `is_below` whether it
/// is under it, as the strings made would say.
#[test]
fn a_name_compares_with_a_directory_as_the_joined_string_does() {
    let names = [
        "", "a", "a-", "a.b", "a/", "a/b", "a0", "ab", "a/b/c", "b", "/", "a//", "\u{e9}",
        "a\u{e9}",
    ];
    for name in names {
        for dir in names {
            let joined = format!("{dir}/");
            assert_eq!(cmp_below(name, dir), name.cmp(&joined), "{name:?} against {dir:?}/");
            assert_eq!(is_below(name, dir), name.starts_with(&joined), "{name:?} under {dir:?}");
        }
    }
}

/// The invariant: a file the daemon writes into the checkout during a build, through the
/// build's own view (the head's root with the file written over it, which moves no head),
/// comes back from the watcher as that view and no other: the head's root after the report is
/// the root the build was of, byte for byte, so the next request is asked on a view a build
/// has already seen; and the same bytes reported again commit nothing. The control is where
/// the cause is removed: a build that recorded the view it was asked on, without the file
/// (the daemon until 2026-11-12, which wrote lock files to the disk alone), is of a root the
/// head has left, and the echo of its own write is a change to it.
#[test]
fn a_file_the_daemon_wrote_through_its_view_is_the_builds_own_when_reported() {
    const LOCK: &str = "# a lock\n";
    for kind in [Journal::FsEvents, Journal::Usn] {
        for seed in 0..20 {
            let what = format!("{kind:?} seed {seed}");
            let mut run = Run::new(seed, kind, TREE);
            let asked = run.vfs.head("disk").expect("the checkout's head");
            // The build's view: the lock file written over the root it was asked on.
            let lock =
                Op::Write { mode: Mode::Regular, path: "Cargo.lock".into(), bytes: LOCK.into() };
            let built = run.vfs.apply_owned(asked, vec![lock]).expect("the write applies");
            assert_ne!(built, asked, "{what}");
            assert_eq!(run.vfs.head("disk"), Some(asked), "{what}: a view's write moves no head");
            // The driver writes the file; the watcher reports it later.
            run.write("Cargo.lock", LOCK);
            run.pump();
            run.check(&what);
            assert_eq!(run.vfs.head("disk"), Some(built), "{what}: the echo is the build's own view");
            // The control: the view the build was asked on is not what the head holds now.
            assert_ne!(run.vfs.head("disk"), Some(asked), "{what}");
            // Reported again with the bytes the head has: no commit, the root as it was.
            run.out.clear();
            run.sim.advance(Duration::from_millis(1));
            run.write("Cargo.lock", LOCK);
            run.pump();
            run.check(&what);
            assert!(!run.out.iter().any(|it| matches!(it, Watched::Committed(_))), "{what}: {:?}", run.out);
            assert_eq!(run.vfs.head("disk"), Some(built), "{what}");
        }
    }
}

/// The wall clock, for the test on the machine's own kernel: its stamps are the file system's.
fn wall_ns() -> u64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since.expect("the clock is past 1970").as_nanos() as u64
}

/// The kinds of journal whose watch hears changes live, so a look can be held in flight while
/// another writer commits.
const LIVE: [Journal; 2] = [Journal::FsEvents, Journal::Usn];
const COLUMN_SEEDS: u64 = 40;

/// What a check of the column's invariant found: how many names it held to the disk, or the
/// first breach.
type Checked = Result<u64, String>;

/// The oracle of the column's invariant, on `vfs`'s column of the watched head: each name it
/// vouches for (its stamp handed, not [`Stamp::UNKNOWN`]) whose stamp is the one the disk
/// gives now, a file or a link, has in the column's root the bytes the disk holds there. The
/// simulation's disk never leaves one stamp over two contents (its clock counts changes), so a
/// stamp that is the disk's now was taken of the bytes the disk holds now. A column paired with
/// no root vouches for nothing. The names it held to the disk.
fn column_holds(vfs: &Vfs, sim: &mut Sim, disk: &BTreeMap<String, Vec<u8>>) -> Checked {
    let Some(column) = vfs.column("disk") else { return Ok(0) };
    let Some(root) = column.root else { return Ok(0) };
    let node = vfs.root(root).expect("a column's root is a root of its tree");
    let mut vouched = Vec::new();
    column.each(|file, stamp| {
        if !stamp.is_unknown() && !stamp.is_gone() && !stamp.is_dir() {
            vouched.push((file.to_owned(), stamp));
        }
    });
    let tree = &vfs.tree;
    let mut checked = 0;
    for (file, stamp) in vouched {
        let Some(path) = file.strip_prefix("ws/") else { continue };
        if sim.stamp(&name(&file)) != stamp {
            continue;
        }
        checked += 1;
        let held =
            tree.entry(node, path).and_then(|(node, _)| tree.bytes(node).expect("not corrupt"));
        let there = disk.get(path).map(Vec::as_slice);
        if held != there {
            return Err(format!(
                "{file}: the column pairs {root:x} with {stamp:?}, the disk's stamp now, over \
                 {:?}, and the root holds {:?}",
                there.map(String::from_utf8_lossy),
                held.map(String::from_utf8_lossy),
            ));
        }
    }
    Ok(checked)
}

/// The end of each record of a VFS journal's file, in order: where a crash can cut it (a cut
/// inside a record recovers as the end before it, the torn record failing its checksum). Each
/// record is framed again from its kind and payload ([`crate::journal::framed`]) and must be
/// those bytes, so the layout is the journal's own; a file that is not whole records is a
/// broken run, not a finding.
fn record_ends(bytes: &[u8]) -> Vec<usize> {
    let header = crate::journal::HEADER;
    let mut ends = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let len = bytes
            .get(at + 4..at + 8)
            .and_then(|it| <[u8; 4]>::try_from(it).ok())
            .unwrap_or_else(|| panic!("the journal's file ends inside a record's header at {at}"));
        let len = u32::from_le_bytes(len) as usize;
        let start = at + header;
        let payload = bytes
            .get(start..start + len)
            .unwrap_or_else(|| panic!("the journal's file ends inside the record at {at}"));
        let record = crate::journal::framed(bytes[at + header - 1], payload);
        let end = at + record.len();
        assert_eq!(bytes.get(at..end), Some(&record[..]), "the record at {at} is not whole");
        ends.push(end);
        at = end;
    }
    ends
}

/// The VFS journal's file as last checked, and how many of its records were: each cut past
/// them is checked once, against the disk as the round that wrote it left it.
#[derive(Default)]
struct Cuts {
    bytes: Vec<u8>,
    ends: usize,
}

impl Cuts {
    /// Every cut of the journal's file made since the last check, recovered as a start after
    /// a crash there would: its column holds the invariant too.
    fn check(&mut self, run: &mut Run) -> Checked {
        let bytes = run.journal.contents(&name("state/journal"));
        let ends = record_ends(&bytes);
        // A rewrite replaced the file: every record of it is new.
        let from = match bytes.starts_with(&self.bytes) {
            true => self.ends,
            false => 0,
        };
        let mut checked = 0;
        for &end in &ends[from..] {
            let (vfs, _) = Vfs::recover(name("state/journal"), 0, &bytes[..end]);
            checked += column_holds(&vfs, &mut run.sim, &run.disk).map_err(|it| {
                format!("recovered from the journal cut at byte {end} of {}: {it}", bytes.len())
            })?;
        }
        (self.bytes, self.ends) = (bytes, ends.len());
        Ok(checked)
    }
}

/// One of `from`, drawn; `None` when it is empty.
fn pick(rng: &mut Rng, from: Vec<String>) -> Option<String> {
    match from.is_empty() {
        true => None,
        false => Some(from[rng.below(from.len() as u64) as usize].clone()), // clone: the pick
    }
}

/// A commit to the watched head by another writer (a build's view committed, an agent's
/// edit), onto the root it is at, its records asked to be written.
fn another_writer(run: &mut Run, ops: &[Op]) {
    let base = run.vfs.head("disk").expect("the head the watcher made");
    if let Err(error) = run.vfs.commit("disk", base, ops) {
        panic!("another writer's commit onto the head: {error}");
    }
    run.vfs.flush(&mut run.effects);
}

/// A conflict: the disk writes a file the head has, and while its read is in flight another
/// writer commits other bytes there; the head keeps its own.
fn conflict(run: &mut Run, rng: &mut Rng, what: &str) {
    let head = run.head();
    let both: Vec<String> = run.disk.keys().filter(|it| head.contains_key(*it)).cloned().collect();
    let Some(path) = pick(rng, both) else { return };
    run.write(&path, &format!("{what}: the disk's"));
    let mut held = Vec::new();
    run.pump_holding_reads(&mut held);
    let bytes = format!("{what}: the head's").into_bytes();
    another_writer(run, &[Op::Write { mode: Mode::Regular, path, bytes }]);
    run.finish_reads(&mut held);
}

/// A move looked at again: the disk writes one file and renames another; while the write's
/// read is in flight another writer deletes the renamed file's source and the tree drops its
/// bytes, so the move has no node to take and its target is looked at again.
fn moved_from_nothing(run: &mut Run, rng: &mut Rng, what: &str) {
    const HELD: &str = "held.rs";
    let head = run.head();
    let mut sources = Vec::new();
    for (path, bytes) in &run.disk {
        // Its bytes no other file of the head has: dropped once its source is deleted.
        let alone = head.values().filter(|it| *it == bytes).count() == 1;
        if path != HELD && head.get(path) == Some(bytes) && alone {
            sources.push(path.clone()); // clone: a candidate
        }
    }
    let Some(source) = pick(rng, sources) else { return };
    let target = format!("moved/{}", rng.below(1 << 32));
    if run.disk.contains_key(&target) {
        return;
    }
    run.write(HELD, &format!("{what}: held"));
    run.rename(&source, &target);
    let mut held = Vec::new();
    run.pump_holding_reads(&mut held);
    another_writer(run, &[Op::Delete { path: source }]);
    run.vfs.collect(&[]);
    run.finish_reads(&mut held);
}

/// Another writer's commit to the head with the disk unchanged: an edit, an add or a delete of
/// a file; or an edit of a file the disk then renames (the head's bytes there are its own, not
/// the disk's, so the rename is no move of the disk's bytes).
fn outside(run: &mut Run, rng: &mut Rng, what: &str) {
    let files: Vec<String> = run.head().into_keys().collect();
    let bytes = format!("{what}: another writer's").into_bytes();
    let how = rng.below(4);
    let op = match how {
        0 => {
            let path = format!("outside/{}", rng.below(1 << 32));
            Op::Write { mode: Mode::Regular, path, bytes }
        }
        _ => {
            let Some(path) = pick(rng, files) else { return };
            match how {
                1 => Op::Delete { path },
                _ => Op::Write { mode: Mode::Regular, path, bytes },
            }
        }
    };
    let renamed = match &op {
        // clone: the path the disk renames after
        Op::Write { path, .. } if how == 3 && run.disk.contains_key(path) => Some(path.clone()),
        _ => None,
    };
    another_writer(run, &[op]);
    run.pump();
    if let Some(from) = renamed {
        let to = format!("moved/{}", rng.below(1 << 32));
        if !run.disk.contains_key(&to) {
            run.rename(&from, &to);
            run.pump();
        }
    }
}

/// A history of `seed` on a disk watched live by `kind`: rounds of the disk's own changes (live,
/// across restarts, with a subtree lost or the journal reset), conflicts, moves looked at again
/// and commits by another writer, the column held to the disk after each, live and at every cut
/// of the journal since the last round. The names held to the disk, or the first breach.
fn column_run(seed: u64, kind: Journal) -> Checked {
    let mut rng = Rng::new(seed ^ 0x636f_6c75);
    let mut run = Run::new(seed, kind, TREE);
    let mut cuts = Cuts::default();
    let mut checked = 0;
    for round in 0..12 {
        let what = format!("{kind:?} seed {seed} round {round}");
        match rng.below(6) {
            0 | 1 => {
                let steps = steps(&mut rng, &run.disk, &what);
                let how = rng.below(4);
                act(&mut run, &steps, how, kind);
            }
            2 => conflict(&mut run, &mut rng, &what),
            3 => moved_from_nothing(&mut run, &mut rng, &what),
            _ => outside(&mut run, &mut rng, &what),
        }
        checked += column_holds(&run.vfs, &mut run.sim, &run.disk)
            .map_err(|it| format!("{what}: the live column: {it}"))?;
        checked += cuts.check(&mut run).map_err(|it| format!("{what}: {it}"))?;
    }
    Ok(checked)
}

/// The invariant: a column never pairs a root with a stamp of bytes other than the ones that
/// root holds at that path; after any torn prefix of the VFS journal the restored column holds
/// it too. Over watcher batches that meet a conflict (another writer's bytes committed while a
/// read is in flight), moves looked at again (their source's node dropped before the commit)
/// and commits by another writer (an edit, an add, a delete, an edit then renamed on the disk),
/// mixed with the disk's own changes and restarts, for each live journal over many seeds.
/// Controls: each of the three cases' fixes reverted ([`Plant`]) is caught.
#[test]
fn a_column_never_vouches_for_bytes_its_root_does_not_hold() {
    let mut checked = 0;
    for kind in LIVE {
        for seed in 0..COLUMN_SEEDS {
            match column_run(seed, kind) {
                Ok(n) => checked += n,
                Err(error) => panic!("{error}"),
            }
        }
    }
    assert!(checked > 0, "the oracle held no name to the disk");
}

/// The first breach of the column's invariant over the histories, with `plant` in the code.
fn caught(plant: Plant) -> Option<String> {
    crate::PLANT.set(plant);
    let caught = LIVE
        .into_iter()
        .find_map(|kind| (0..COLUMN_SEEDS).find_map(|seed| column_run(seed, kind).err()));
    crate::PLANT.set(Plant::None);
    caught
}

/// The control of the conflicts: a conflict's stamp saved as one that vouches pairs the head's
/// root with the disk's stamp over the bytes the head did not take.
#[test]
fn the_oracle_catches_a_conflict_saved_as_vouching() {
    assert!(caught(Plant::ConflictVouched).is_some(), "no history caught a vouching conflict");
}

/// The control of the moves looked at again: the target's stamp saved, and its removal in a
/// record after the commit's, leaves a cut of the journal between them whose column vouches
/// for a file its root lacks.
#[test]
fn the_oracle_catches_a_move_looked_at_again_recorded_apart() {
    assert!(caught(Plant::AgainApart).is_some(), "no cut caught a move recorded apart");
}

/// The control of another writer's commits: a column left as it was, and paired with the
/// head's root whatever it was kept against, vouches for the bytes the commit replaced.
#[test]
fn the_oracle_catches_a_column_left_behind_by_another_writer() {
    assert!(caught(Plant::ColumnOfTheHead).is_some(), "no history caught the column left behind");
}

/// The control of moves: a rename from a name whose stamp the head chose against, taken as a
/// move of the head's bytes, pairs the column's root with the stamp of the disk's bytes.
#[test]
fn the_oracle_catches_a_move_from_a_chosen_name() {
    assert!(caught(Plant::MoveFromChosen).is_some(), "no history caught a move from a chosen name");
}
