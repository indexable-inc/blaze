//! The watcher under the simulation, as each system's journal behaves across a restart
//! (docs/v2/plan/lanes/frontend/journal.md): what changed while blaze was down is found from
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
use crate::{Mode, Vfs, disk::Disks, section::Section};

/// A watcher under test: today's, or the reference it is held to.
trait Watch {
    fn new(head: &str, dir: FileName, tags: u64) -> Self;
    fn start(&mut self, vfs: &Vfs, effects: &mut Vec<Effect>);
    fn stop(&mut self, effects: &mut Vec<Effect>);
    fn owns(&self, tag: Tag) -> bool;
    fn idle(&self) -> bool;
    fn complete(&mut self, vfs: &mut Vfs, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Watched>;
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
            fn complete(&mut self, vfs: &mut Vfs, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Watched> {
                <$watcher>::complete(self, vfs, completion, effects)
            }
        }
    };
}

watch!(Watcher);
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
        let mut done = Vec::new();
        loop {
            for effect in &self.effects {
                match effect {
                    Effect::ReadAll { file, .. } | Effect::ReadLink { file, .. } => {
                        self.reads.push(file.as_str()["ws/".len()..].into())
                    }
                    Effect::Stamp { files, .. } => self.stamped.extend(files.iter().map(str::to_owned)),
                    _ => {}
                }
            }
            self.log.extend(self.effects.iter().cloned());
            self.sim.submit(&mut self.effects);
            if self.sim.poll(&mut done, Duration::ZERO) == 0 && self.effects.is_empty() {
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

    /// blaze stops: the machine keeps its disk and journal, the watch is gone, and the column
    /// goes through the image's bytes (encoded, then read in place). `down` changes the disk
    /// while nothing watches; then a new watcher starts from what was saved.
    fn restart(&mut self, down: impl FnOnce(&mut Run<W>)) {
        assert!(self.watcher.idle(), "stopped with looks in flight: {:?}", self.out);
        self.sim.crash();
        down(self);
        let mut bytes = Vec::new();
        self.vfs.disks.encode(&mut bytes);
        let len = bytes.len();
        let section = Section::new(Mapping::copy(&bytes), 0..len).expect("a section");
        self.vfs.disks = Disks::open(section).expect("the column reopens");
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
        assert_eq!(run.mode("up"), Some(Mode::Symlink), "{what}: one out of the checkout is kept as it is");
        assert_eq!(run.mode("lib.rs"), Some(Mode::Regular), "{what}: a link replaced by a file");

        let fresh: Run<W> = Run::linked(2, kind, &files, &[(".claude/skills", "../.agents/skills")]);
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
        let files = [(".agents/skills/a/SKILL.md", "a"), (".claude/settings.json", "{}"), (".claude/rules", "r")];
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
        let told = Journaled { files: Vec::new(), lost: vec![(ws(".claude"), Lost::Unnamed)], cursor, caught_up: true };
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
        assert_eq!(run.stamped, Vec::<String>::new(), "{what}");
        for link in [".claude/skills", ".claude/rules"] {
            assert_eq!(run.mode(link), Some(Mode::Symlink), "{what}: {link}");
            let file = ws(link);
            let as_file = run.log.iter().any(|it| matches!(it, Effect::ReadAll { file: read, .. } if *read == file));
            assert!(!as_file, "{what}: {link} read as a file");
            let as_link = run.log.iter().any(|it| matches!(it, Effect::ReadLink { file: read, .. } if *read == file));
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
            match kind {
                // No journal: one sweep of stamps, of every tracked name and nothing else.
                Journal::None => {
                    let swept: BTreeSet<&str> = run.stamped.iter().map(String::as_str).collect();
                    assert!(swept.contains("ws/src/old/deep/three.rs"), "{what}: {swept:?}");
                    assert!(swept.iter().all(|it| *it == "ws" || it.starts_with("ws/")), "{what}");
                    assert_eq!(swept.len(), run.stamped.len(), "{what}: each name once");
                }
                // A journal: nothing is looked at.
                _ => assert_eq!(run.stamped, Vec::<String>::new(), "{what}"),
            }
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
            // A journal names what changed: nothing it did not name is stamped.
            if kind != Journal::None {
                assert!(!run.stamped.iter().any(|it| it == "ws/src/lib.rs"), "{what}: {:?}", run.stamped);
            }
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

            // The system says to look at one subtree again: nothing outside it is stamped.
            run.restart(|run| {
                run.write("src/old/deep/three.rs", "three");
                run.sim.lose_changes(&ws("src/old"), Lost::Rescan);
            });
            run.check(&what);
            assert!(run.lost().contains(&("src/old", Lost::Rescan)), "{what}: {:?}", run.lost());
            assert_eq!(run.reads(), BTreeSet::from(["src/old/deep/three.rs"]), "{what}");
            let outside = run.stamped.iter().find(|it| !it.starts_with("ws/src/old"));
            assert_eq!(outside, None, "{what}: stamped outside the lost subtree");
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
    let out = run.watcher.complete(&mut run.vfs, journal, &mut run.effects);
    run.out.extend(out);
    assert!(
        matches!(&run.effects[..], [Effect::Entries { dir, .. }] if *dir == ws("src")),
        "{:?}",
        run.effects
    );
    run.pump();
    run.check("unnamed");
    assert_eq!(run.lost(), [("src", Lost::Unnamed)]);
    assert_eq!(run.stamped, Vec::<String>::new());
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
                            && !disk.keys().any(|k| k.starts_with(&format!("{it}/")) || it.starts_with(&format!("{k}/")))
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
                        _ if disk.keys().any(|k| path.starts_with(&format!("{k}/")) || k.starts_with(&format!("{path}/"))) => {}
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
    const PATHS: &[&str] = &["a", "b", "d/x", "d/y", "d/e/z", "f/g/h", "k", "src/old/one.rs", "src/new.rs"];
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
                let Some(to) = PATHS.iter().find(|it| !disk.contains_key(**it) && !clash(&disk, it)) else {
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
/// head is the same.
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
        let Some(Effect::Stamp { files, tag }) = run.effects.pop() else { panic!("{:?}", run.effects) };
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
            run.watcher.complete(&mut run.vfs, completion, &mut run.effects)
        });
        assert!(out.is_empty(), "{out:?}");
        let completion = stamps(&mut run, 0);
        let out = assert_at_most(budget(0), "their stamps settled", || {
            run.watcher.complete(&mut run.vfs, completion, &mut run.effects)
        });
        assert!(out.is_empty(), "{out:?}");
        assert!(run.effects.is_empty(), "nothing read or written: {:?}", run.effects);
        assert!(run.watcher.idle());

        // Every stamp moved (the same inode, a later mtime): each file is read.
        let completion = journaled(&run);
        assert_at_most(budget(0), "a batch of changed files told", || {
            run.watcher.complete(&mut run.vfs, completion, &mut run.effects)
        });
        let completion = stamps(&mut run, 1);
        let Done::Stamped(moved) = &completion.done else { unreachable!() };
        let n = moved.len() as u64;
        assert_at_most(budget(n), "their stamps, a read asked for each", || {
            run.watcher.complete(&mut run.vfs, completion, &mut run.effects)
        });
        assert_eq!(run.effects.iter().filter(|it| matches!(it, Effect::ReadAll { .. })).count() as u64, n);
        run.pump();
        assert!(run.watcher.idle());
        assert_eq!(run.head(), run.disk, "the same bytes read make no change");
    }
}

/// `cmp_below` is the order of a name against a directory and a `/`, and `is_below` whether it
/// is under it, as the strings made would say.
#[test]
fn a_name_compares_with_a_directory_as_the_joined_string_does() {
    let names = ["", "a", "a-", "a.b", "a/", "a/b", "a0", "ab", "a/b/c", "b", "/", "a//", "\u{e9}", "a\u{e9}"];
    for name in names {
        for dir in names {
            let joined = format!("{dir}/");
            assert_eq!(cmp_below(name, dir), name.cmp(&joined), "{name:?} against {dir:?}/");
            assert_eq!(is_below(name, dir), name.starts_with(&joined), "{name:?} under {dir:?}");
        }
    }
}
