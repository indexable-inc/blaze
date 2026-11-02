//! The tree opened in place from an image against the same history recovered from a journal
//! that never had one (docs/v2/plan/design/restore.md): equal over random histories, at every
//! step of writing an image and rewriting the journal to start from it, with no file's bytes
//! read until something reads that file, and a corrupt page group named.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use blaze_core::{
    Driver, Effect, FileName, Mapping,
    sim::{Faults, Rng, Sim},
};
use blaze_ids::ContentId;
use blaze_image::{Builder, Image, MIN_GROUP_SHIFT, SectionId};

use super::{BLOBS_SCHEMA, DISKS_SCHEMA, DisksUnused, ImageError, ImageMark, ImageUse, TREE_SCHEMA};
use crate::{Change, Mode, Op, OpError, Vfs, journal::base_of, store::Source, tree::NodeId};

fn file() -> FileName {
    FileName::new("vfs/journal").unwrap()
}

/// Bytes at an address aligned for every column, as a mapped file's are.
fn mapping(bytes: &[u8]) -> Mapping {
    Mapping::copy(bytes)
}

/// The image of `vfs` as it is now, in groups of the smallest size, so files span several.
fn image_of(vfs: &mut Vfs) -> Result<(ImageMark, Vec<u8>), ImageError> {
    let mut builder = Builder::with_group_shift(MIN_GROUP_SHIFT);
    let mark = vfs.write_image(&mut builder)?;
    Ok((mark, builder.finish().expect("an image under the limit")))
}

fn run(sim: &mut Sim, vfs: &mut Vfs, effects: &mut Vec<Effect>) {
    let mut done = Vec::new();
    while !effects.is_empty() || sim.in_flight() > 0 {
        sim.submit(effects);
        sim.poll(&mut done, Duration::ZERO);
        for completion in done.drain(..) {
            assert!(vfs.owns(completion.tag));
            vfs.complete(completion, effects);
        }
    }
}

/// Flushes `vfs` and runs every effect that leads to.
fn settle(sim: &mut Sim, vfs: &mut Vfs) {
    let mut effects = Vec::new();
    vfs.flush(&mut effects);
    run(sim, vfs, &mut effects);
}

fn start(seed: u64) -> (Vfs, Sim) {
    (Vfs::recover(file(), 0, &[]).0, Sim::new(seed, Faults::default()))
}

/// Everything a VFS answers about its heads: each head's root and sequence, every file with
/// its mode and bytes, every directory's listing, and the diff between each two heads.
#[derive(PartialEq, Eq, Debug)]
struct State {
    heads: Vec<(String, ContentId, u64)>,
    last: u64,
    files: BTreeMap<String, BTreeMap<String, (Mode, Vec<u8>)>>,
    dirs: BTreeMap<String, Listings>,
    diffs: Vec<(String, String, Vec<Change>)>,
}

/// Each head's root and sequence, reading no file.
fn heads(vfs: &Vfs) -> Vec<(String, ContentId, u64)> {
    vfs.heads()
        .into_iter()
        .map(|(name, root)| (name.to_owned(), root, vfs.sequence(name).expect("a sequence")))
        .collect()
}

fn state(vfs: &Vfs) -> State {
    let heads = heads(vfs);
    let (mut files, mut dirs) = (BTreeMap::new(), BTreeMap::new());
    for (name, root, _) in &heads {
        let node = vfs.root(*root).expect("a head's root");
        let of = vfs.tree.files(node, Mode::Regular, "").into_iter().map(|(path, file, mode)| {
            let bytes = vfs.tree.bytes(file).expect("no corrupt group").expect("a file");
            (path, (mode, bytes.to_vec()))
        });
        files.insert(name.clone(), of.collect());
        let mut listed = BTreeMap::new();
        list_dirs(vfs, node, String::new(), &mut listed);
        dirs.insert(name.clone(), listed);
    }
    let mut diffs = Vec::new();
    for (a, from, _) in &heads {
        for (b, to, _) in &heads {
            diffs.push((a.clone(), b.clone(), vfs.diff(*from, *to)));
        }
    }
    State { heads, last: vfs.journal.last_sequence(), files, dirs, diffs }
}

type Listings = BTreeMap<String, Vec<(String, bool)>>;

fn list_dirs(vfs: &Vfs, node: NodeId, path: String, out: &mut Listings) {
    let entries = vfs.tree.entries(node).expect("a directory");
    let mut listing = Vec::new();
    for at in 0..entries.len() {
        let (name, child, _) = entries.get(at);
        let name = vfs.tree.name_str(name).to_owned();
        if entries.is_dir(at) {
            list_dirs(vfs, child, format!("{path}{name}/"), out);
        }
        listing.push((name, entries.is_dir(at)));
    }
    out.insert(path, listing);
}

/// An image used whole: none given, the tree mapped with the journal's tail, or the whole
/// journal over it after a stop between the image's rename and the journal's rewrite.
fn usable(it: &ImageUse) -> bool {
    matches!(
        it,
        ImageUse::None | ImageUse::Used { disks: None, .. } | ImageUse::Resumed { disks: None, .. }
    )
}

const HEADS: [&str; 3] = ["main", "b", "pr/1"];

/// One step of a history, made from the random numbers once and done to every VFS alike.
enum Act {
    Commit(&'static str, Vec<Op>),
    Branch { from: &'static str, to: &'static str },
    Remove(&'static str),
    Collect,
}

fn bytes(rng: &mut Rng) -> Vec<u8> {
    // Some files span page groups, so a read checks more than one.
    let len = if rng.one_in(8) { 5000 + rng.below(9000) } else { 1 + rng.below(40) };
    let fill = rng.below(251) as u8;
    (0..len).map(|at| fill.wrapping_add(at as u8)).collect()
}

fn commit(rng: &mut Rng, head: &'static str, step: usize) -> Act {
    let mut ops = Vec::new();
    for _ in 0..=rng.below(3) {
        let dir = ["src", "src/x", "tests", "docs/a/b"][rng.below(4) as usize];
        let path = format!("{dir}/f{}", rng.below(5));
        ops.push(match rng.below(6) {
            0 => Op::Delete { path },
            1 => Op::Rename { from: path, to: format!("moved/{step}") },
            _ => {
                let mode = if rng.one_in(5) { Mode::Executable } else { Mode::Regular };
                Op::Write { mode, path, bytes: bytes(rng) }
            }
        });
    }
    Act::Commit(head, ops)
}

fn act(rng: &mut Rng, step: usize) -> Act {
    let head = |rng: &mut Rng| HEADS[rng.below(HEADS.len() as u64) as usize];
    match rng.below(20) {
        0 => Act::Collect,
        1 => Act::Remove(head(rng)),
        2 | 3 => Act::Branch { from: head(rng), to: head(rng) },
        _ => {
            let to = head(rng);
            commit(rng, to, step)
        }
    }
}

/// Does `act` to `vfs`; what it answered, to compare.
fn apply(vfs: &mut Vfs, act: &Act) -> String {
    match act {
        Act::Commit(head, ops) => {
            let base = vfs.head(head).unwrap_or(vfs.empty());
            format!("{:?}", vfs.commit(head, base, ops).map(|it| (it.root, it.rebased)))
        }
        Act::Branch { from, to } => {
            format!("{:?}", vfs.head(from).map(|root| vfs.set_head(to, root).map(|_| ())))
        }
        Act::Remove(head) => format!("{}", vfs.remove_head(head).is_some()),
        Act::Collect => {
            vfs.collect(&[]);
            String::new()
        }
    }
}

/// A history done to two VFSes, one that never writes an image and one that writes them
/// and starts from them: every start of the second answers as a start of the first, and as
/// it did before stopping.
fn history_run(seed: u64) {
    let mut rng = Rng::new(seed ^ 0x1a6e);
    let (mut plain, mut plain_sim) = start(seed);
    let (mut imaged, mut sim) = start(seed.wrapping_add(1));
    let mut image: Option<Vec<u8>> = None;
    let mut images = 0;
    for step in 0..160 {
        match rng.below(16) {
            0 => match image_of(&mut imaged) {
                Ok((mark, bytes)) => {
                    image = Some(bytes);
                    images += 1;
                    assert!(imaged.image_durable(mark.name), "seed {seed}: {step}");
                }
                Err(ImageError::Busy) => {}
                Err(error) => panic!("seed {seed}: {step}: {error}"),
            },
            1 => {
                let (back, found) =
                    Vfs::restore(file(), 0, &sim.contents(&file()), image.as_deref().map(mapping));
                assert!(usable(&found.image), "seed {seed}: {step}: {found:?}");
                assert_eq!((found.skipped, found.heads_dropped), (0, 0), "seed {seed}: {step}");
                let (twin, _) = Vfs::recover(file(), 0, &plain_sim.contents(&file()));
                let restored = state(&back);
                assert_eq!(restored, state(&twin), "seed {seed}: {step}");
                assert_eq!(restored, state(&imaged), "seed {seed}: {step}");
                (imaged, plain) = (back, twin);
            }
            _ => {
                let act = act(&mut rng, step);
                let answers = (apply(&mut imaged, &act), apply(&mut plain, &act));
                assert_eq!(answers.0, answers.1, "seed {seed}: {step}");
            }
        }
        settle(&mut sim, &mut imaged);
        settle(&mut plain_sim, &mut plain);
    }
    let (back, found) =
        Vfs::restore(file(), 0, &sim.contents(&file()), image.as_deref().map(mapping));
    assert!(usable(&found.image), "seed {seed}: {found:?}");
    assert_eq!(image.is_some(), images > 0);
    assert_eq!(state(&back), state(&plain), "seed {seed}");
}

#[test]
fn an_image_and_its_journal_restore_what_the_journal_alone_recovers() {
    let seeds =
        std::env::var("BLAZE_DST_SEEDS").ok().and_then(|it| it.parse().ok()).unwrap_or(64u64);
    for seed in 0..seeds {
        history_run(seed);
    }
}

/// The steps of writing two images and moving the journal onto each, with a stop at one of
/// them: [`Crash::point`] is each place one can come.
struct Crash {
    seed: u64,
    rng: Rng,
    plain: Vfs,
    plain_sim: Sim,
    vfs: Vfs,
    sim: Sim,
    /// The image file as the last rename left it.
    image: Option<Vec<u8>>,
    at: usize,
    stop_at: usize,
}

impl Crash {
    fn new(seed: u64, stop_at: usize) -> Crash {
        let (plain, plain_sim) = start(seed);
        let (vfs, sim) = start(seed.wrapping_add(1));
        Crash { seed, rng: Rng::new(seed), plain, plain_sim, vfs, sim, image: None, at: 0, stop_at }
    }

    /// Commits to `main` and `b`, each durable before the next.
    fn history(&mut self, steps: usize) {
        for step in 0..steps {
            let head = if self.rng.one_in(3) { "b" } else { "main" };
            let act = commit(&mut self.rng, head, self.at * 100 + step);
            self.both(&act);
        }
    }

    fn both(&mut self, act: &Act) {
        assert_eq!(apply(&mut self.vfs, act), apply(&mut self.plain, act), "seed {}", self.seed);
        settle(&mut self.sim, &mut self.vfs);
        settle(&mut self.plain_sim, &mut self.plain);
    }

    /// A place a stop can come: if it is this run's, the machine stops (what is in flight is
    /// lost), and the VFS started from what the disk holds answers as the one that never wrote
    /// an image; after its next flush, a start again maps the image with the journal's tail.
    fn point(&mut self) -> bool {
        self.at += 1;
        if self.at - 1 != self.stop_at {
            return false;
        }
        let (seed, stop) = (self.seed, self.stop_at);
        self.sim.crash();
        let image = || self.image.as_deref().map(mapping);
        let (mut back, found) = Vfs::restore(file(), 0, &self.sim.contents(&file()), image());
        assert!(usable(&found.image), "seed {seed}: stop {stop}: {found:?}");
        assert_eq!(found.heads_dropped, 0, "seed {seed}: stop {stop}");
        let want = state(&self.plain);
        assert_eq!(state(&back), want, "seed {seed}: stop {stop}");
        settle(&mut self.sim, &mut back);
        let (again, found) = Vfs::restore(file(), 0, &self.sim.contents(&file()), image());
        match &self.image {
            Some(_) => assert!(
                matches!(found.image, ImageUse::Used { disks: None, .. }),
                "seed {seed}: stop {stop}: {found:?}"
            ),
            None => assert_eq!(found.image, ImageUse::None),
        }
        assert_eq!(state(&again), want, "seed {seed}: stop {stop}");
        true
    }

    /// One image written, renamed into place and named by the journal's rewrite, with
    /// commits between; `true` once the stop came.
    fn round(&mut self) -> bool {
        let (mark, bytes) = image_of(&mut self.vfs).expect("an image");
        if self.point() {
            return true;
        }
        // Commits made after the image and before its rename go to the old journal.
        self.history(2);
        if self.point() {
            return true;
        }
        self.image = Some(bytes);
        if self.point() {
            return true;
        }
        assert!(self.vfs.image_durable(mark.name));
        let mut effects = Vec::new();
        self.vfs.flush(&mut effects);
        assert!(matches!(effects[..], [Effect::Write { .. }]), "the rewrite onto the image");
        if self.point() {
            return true;
        }
        self.sim.submit(&mut effects);
        if self.point() {
            return true;
        }
        run(&mut self.sim, &mut self.vfs, &mut effects);
        let journal = self.sim.contents(&file());
        assert_eq!(base_of(&journal), Some((mark.name, mark.nodes)));
        if self.point() {
            return true;
        }
        self.history(2);
        self.point()
    }

    /// The whole run; `false` when it ended before its stop.
    fn go(mut self) -> bool {
        // The random commits can all fail (seed 2's do), leaving an image of the empty root
        // alone, which no collection drops: `main` holds a file before the first image.
        let first =
            Op::Write { mode: Mode::Regular, path: "src/first".into(), bytes: b"first".to_vec() };
        self.both(&Act::Commit("main", vec![first]));
        self.history(6);
        if self.point() || self.round() {
            return true;
        }
        self.history(2);
        // A head removed after the image holds it stays removed through the next rewrite.
        self.both(&Act::Remove("b"));
        let bytes = format!("{} after the first image", self.seed).into_bytes();
        let ops = vec![Op::Write { mode: Mode::Regular, path: "src/after".into(), bytes }];
        self.both(&Act::Commit("main", ops));
        // Dropping nodes of the image the journal starts from leaves it naming them until
        // its rewrite is durable: no image is written over that journal before then.
        self.vfs.collect(&[]);
        self.plain.collect(&[]);
        assert!(matches!(image_of(&mut self.vfs), Err(ImageError::Busy)), "seed {}", self.seed);
        if self.point() {
            return true;
        }
        settle(&mut self.sim, &mut self.vfs);
        settle(&mut self.plain_sim, &mut self.plain);
        if self.point() {
            return true;
        }
        self.round()
    }
}

#[test]
fn a_stop_at_any_step_of_moving_onto_an_image_loads_what_was_durable() {
    for seed in 0..8 {
        let mut stop_at = 0;
        while Crash::new(seed, stop_at).go() {
            stop_at += 1;
        }
        assert!(stop_at > 15, "seed {seed}: {stop_at} places");
    }
}

/// `main` with a small file and one spanning four page groups, its image durable and the
/// journal rewritten onto it.
fn imaged() -> (Vfs, Vec<u8>, ImageMark, Vec<u8>) {
    let (mut vfs, mut sim) = start(1);
    let big: Vec<u8> = (0..4 << MIN_GROUP_SHIFT).map(|at: u32| (at % 253) as u8).collect();
    let ops = [
        Op::Write { mode: Mode::Regular, path: "small".into(), bytes: b"small".to_vec() },
        Op::Write { mode: Mode::Regular, path: "dir/big".into(), bytes: big.clone() },
    ];
    vfs.commit("main", vfs.empty(), &ops).unwrap();
    settle(&mut sim, &mut vfs);
    let (mark, image) = image_of(&mut vfs).unwrap();
    assert!(vfs.image_durable(mark.name));
    assert!(!vfs.image_durable(ContentId([7; 32])), "only the image last written");
    settle(&mut sim, &mut vfs);
    (vfs, image, mark, sim.contents(&file()))
}

/// Where the file at `path` of `main` is in the image's blobs section.
fn place(vfs: &Vfs, path: &str) -> (u64, u64) {
    let root = vfs.root(vfs.head("main").unwrap()).unwrap();
    let (node, _) = vfs.tree.entry(root, path).unwrap();
    let table = vfs.tree.table();
    match table.source(table.shape(node.0)) {
        Source::Mapped { at, len } => (at, len),
        Source::Heap(_) => panic!("{path} is not the image's"),
    }
}

fn groups(at: u64, len: u64) -> std::ops::RangeInclusive<u32> {
    (at >> MIN_GROUP_SHIFT) as u32..=((at + len - 1) >> MIN_GROUP_SHIFT) as u32
}

#[test]
fn a_start_reads_the_journals_tail_and_no_file_until_one_is_read() {
    let (vfs, image, mark, journal) = imaged();
    assert_eq!(base_of(&journal), Some((mark.name, mark.nodes)));
    assert!(journal.len() < 512, "the journal holds no file the image has: {}", journal.len());
    let (back, found) = Vfs::restore(file(), 0, &journal, Some(mapping(&image)));
    assert_eq!(found.image, ImageUse::Used { name: mark.name, nodes: mark.nodes, disks: None });
    assert_eq!(heads(&back), heads(&vfs));

    let mapped = Arc::clone(back.tree.table().mapped().expect("a mapped base"));
    let (image, blobs) = (&mapped.image, &mapped.blobs);
    let checked = |group| image.is_checked(blobs, group);
    assert!(blobs.groups >= 4);
    assert!((0..blobs.groups).all(|group| !checked(group)), "a start reads no file");
    let root = back.root(back.head("main").unwrap()).unwrap();
    assert_eq!(back.tree.read(root, "small").unwrap(), Some(&b"small"[..]));
    let (at, len) = place(&back, "small");
    let small = groups(at, len);
    for group in 0..blobs.groups {
        assert_eq!(checked(group), small.contains(&group), "group {group}");
    }
    let tree = image.section(SectionId::VFS_TREE, TREE_SCHEMA).unwrap();
    assert!((0..tree.groups).all(|group| image.is_checked(&tree, group)), "the tree, whole");
}

#[test]
fn a_corrupt_disks_section_is_named_not_taken_for_another_schema() {
    let (_, image, _, journal) = imaged();
    let disks_at = {
        let opened = Image::open(mapping(&image)).unwrap();
        opened.section(SectionId::VFS_DISKS, DISKS_SCHEMA).unwrap().at
    };
    let mut corrupt = image.clone();
    corrupt[disks_at as usize] ^= 1;
    let (_, found) = Vfs::restore(file(), 0, &journal, Some(mapping(&corrupt)));
    let ImageUse::Used { disks: Some(DisksUnused::Corrupt(why)), .. } = &found.image else {
        panic!("a corrupt disks section, named: {found:?}");
    };
    assert_eq!(why.section, SectionId::VFS_DISKS);
}

#[test]
fn a_corrupt_file_is_named_by_its_group_and_mended_by_writing_it_again() {
    let (vfs, image, _, journal) = imaged();
    let clean = Vfs::restore(file(), 0, &journal, Some(mapping(&image))).0;
    let (big_at, big_len) = place(&clean, "dir/big");
    let blobs_at = {
        let opened = Image::open(mapping(&image)).unwrap();
        opened.section(SectionId::VFS_BLOBS, BLOBS_SCHEMA).unwrap().at
    };
    let bad = big_at + big_len / 2;
    let mut corrupt = image.clone();
    corrupt[(blobs_at + bad) as usize] ^= 1;

    let (mut back, found) = Vfs::restore(file(), 0, &journal, Some(mapping(&corrupt)));
    assert!(matches!(found.image, ImageUse::Used { .. }), "{found:?}");
    assert_eq!(back.corrupt(), None, "nothing read yet");
    let root = back.root(back.head("main").unwrap()).unwrap();
    assert_eq!(back.tree.read(root, "small").unwrap(), Some(&b"small"[..]));
    let error = back.tree.read(root, "dir/big").unwrap_err();
    let group = (bad >> MIN_GROUP_SHIFT) as u32;
    assert_eq!((error.section, error.group), (SectionId::VFS_BLOBS, group));
    assert_eq!(back.corrupt(), Some(error));
    assert_eq!(
        OpError::from(error),
        OpError::Corrupt { section: SectionId::VFS_BLOBS.0, group: error.group }
    );
    // No image is written of bytes that are not there.
    assert!(matches!(image_of(&mut back), Err(ImageError::Corrupt(it)) if it == error));

    // The same bytes written again (a scan of the disk, a checkout) are the node's again.
    let big = vfs.tree.read(vfs.root(vfs.head("main").unwrap()).unwrap(), "dir/big").unwrap();
    let bytes = big.expect("a file").to_vec();
    let ops = [Op::Write { mode: Mode::Regular, path: "dir/big".into(), bytes }];
    let main = back.head("main").unwrap();
    assert_eq!(back.commit("main", main, &ops).unwrap().root, main);
    assert_eq!(state(&back).files, state(&vfs).files);
    assert!(image_of(&mut back).is_ok());
}

#[test]
fn a_corrupt_tree_leaves_the_image_unused_and_says_so() {
    let (_, image, mark, journal) = imaged();
    let tree_at = {
        let opened = Image::open(mapping(&image)).unwrap();
        opened.section(SectionId::VFS_TREE, TREE_SCHEMA).unwrap().at
    };
    let mut corrupt = image.clone();
    corrupt[tree_at as usize + 8] ^= 1;
    let (back, found) = Vfs::restore(file(), 0, &journal, Some(mapping(&corrupt)));
    match found.image {
        ImageUse::Corrupt(it) => assert_eq!((it.section, it.group), (SectionId::VFS_TREE, 0)),
        other => panic!("{other:?}"),
    }
    // The journal alone names nodes only the image has: its head is dropped, and said to be.
    assert_eq!(found.heads_dropped, 1);
    assert_eq!(back.head("main"), None);

    // An image this journal does not start from, nor was written over, is not used.
    let (mut other, _) = start(9);
    let ops = [Op::Write { mode: Mode::Regular, path: "x".into(), bytes: b"x".to_vec() }];
    other.commit("main", other.empty(), &ops).unwrap();
    let (theirs, unrelated) = image_of(&mut other).unwrap();
    let (_, found) = Vfs::restore(file(), 0, &journal, Some(mapping(&unrelated)));
    assert_eq!(found.image, ImageUse::Unnamed { image: theirs.name, journal: Some(mark.name) });
    let (_, found) = Vfs::restore(file(), 0, &journal, Some(mapping(b"not an image")));
    assert!(matches!(found.image, ImageUse::Unopened(_)), "{found:?}");
}

/// An image written over a journal that starts from none (a stop before the first rewrite)
/// adds only nodes when paired with another VFS's such journal: its heads are the journal's.
#[test]
fn an_image_brings_nodes_and_never_heads() {
    let (mut ours, mut sim) = start(2);
    let ops = [Op::Write { mode: Mode::Regular, path: "ours".into(), bytes: b"1".to_vec() }];
    ours.commit("main", ours.empty(), &ops).unwrap();
    settle(&mut sim, &mut ours);
    let (_, image) = image_of(&mut ours).unwrap();
    let (mut theirs, mut their_sim) = start(3);
    let ops = [Op::Write { mode: Mode::Regular, path: "theirs".into(), bytes: b"2".to_vec() }];
    theirs.commit("pr", theirs.empty(), &ops).unwrap();
    settle(&mut their_sim, &mut theirs);
    let (back, found) =
        Vfs::restore(file(), 0, &their_sim.contents(&file()), Some(mapping(&image)));
    assert!(matches!(found.image, ImageUse::Resumed { .. }), "{found:?}");
    assert_eq!(state(&back), state(&theirs));
}
