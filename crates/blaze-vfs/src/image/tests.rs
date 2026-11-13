//! The tree opened in place from an image against the same history recovered from a journal
//! that never had one (docs/v2/plan/design/restore.md): equal over random histories, at every
//! step of writing an image and rewriting the journal to start from it, with no file's text
//! asked of the store until something reads that file, a text the store lacks named, and an
//! image that holds no file's bytes.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use blaze_core::{
    Driver, Effect, FileName, Mapping,
    sim::{Faults, Rng, Sim},
};
use blaze_format::schema;
use blaze_ids::ContentId;
use blaze_image::{Builder, Cause, Image, MIN_GROUP_SHIFT, Refused, SectionId, TextWhy};

use super::{
    DISKS_SCHEMA, DisksUnused, ImageError, ImageFile, ImageMark, ImageUse, TREE_SCHEMA,
    tree_layout::{W_TEXT_BYTES, WORDS},
};
use crate::{
    Bytes, Change, FILE_DOMAIN, Mode, Op, OpError, Vfs,
    journal::base_of,
    texts::{HeldTexts, StoreFault, Texts},
    tree::NodeId,
};

pub(crate) fn file() -> FileName {
    FileName::new("vfs/journal").unwrap()
}

/// The store the tests' images put their texts in, one a process: texts are by content ID, so
/// tests share it as homes share a machine's layer. A test that breaks a store makes its own.
pub(crate) fn texts() -> Arc<HeldTexts> {
    static TEXTS: OnceLock<Arc<HeldTexts>> = OnceLock::new();
    // clone: a count of the tests' one store.
    Arc::clone(TEXTS.get_or_init(HeldTexts::shared))
}

/// An image's bytes at an address aligned for every column, as a mapped file's are, with the
/// tests' store of texts.
pub(crate) fn mapping(bytes: &[u8]) -> ImageFile {
    ImageFile { file: Mapping::copy(bytes), texts: texts() }
}

/// The image of `vfs` as it is now, its texts in the tests' store, in groups of the smallest
/// size.
pub(crate) fn image_of(vfs: &mut Vfs) -> Result<(ImageMark, Vec<u8>), ImageError> {
    image_in(vfs, &*texts())
}

/// [`image_of`] with its texts put in `texts`.
pub(crate) fn image_in(
    vfs: &mut Vfs,
    texts: &dyn Texts,
) -> Result<(ImageMark, Vec<u8>), ImageError> {
    let mut builder = Builder::with_group_shift(MIN_GROUP_SHIFT);
    let mark = vfs.write_image(&mut builder, texts)?;
    Ok((mark, builder.finish().expect("an image under the limit")))
}

/// A store that counts the texts asked of it.
struct Counted {
    held: Arc<HeldTexts>,
    gets: AtomicUsize,
}

impl Texts for Counted {
    fn get(&self, id: &ContentId) -> Result<Option<Bytes>, StoreFault> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        self.held.get(id)
    }

    fn has(&self, id: &ContentId) -> bool {
        self.held.has(id)
    }

    fn put(&self, id: &ContentId, bytes: &[u8]) -> Result<(), StoreFault> {
        self.held.put(id, bytes)
    }

    fn durable(&self) -> bool {
        self.held.durable()
    }
}

pub(crate) fn run(sim: &mut Sim, vfs: &mut Vfs, effects: &mut Vec<Effect>) {
    let mut done = Vec::new();
    while !effects.is_empty() || sim.in_flight() > 0 {
        sim.submit(effects);
        sim.poll(&mut done, blaze_core::wait::Deadline::NEVER);
        for completion in done.drain(..) {
            assert!(vfs.owns(completion.tag));
            vfs.complete(completion, effects);
        }
    }
}

/// Flushes `vfs` and runs every effect that leads to.
pub(crate) fn settle(sim: &mut Sim, vfs: &mut Vfs) {
    let mut effects = Vec::new();
    vfs.flush(&mut effects);
    run(sim, vfs, &mut effects);
}

pub(crate) fn start(seed: u64) -> (Vfs, Sim) {
    (Vfs::recover(file(), 0, &[]).0, Sim::new(seed, Faults::default()))
}

/// Everything a VFS answers about its heads: each head's root and sequence, every file with
/// its mode and bytes, every directory's listing, and the diff between each two heads.
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct State {
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

pub(crate) fn state(vfs: &Vfs) -> State {
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
pub(crate) fn usable(it: &ImageUse) -> bool {
    matches!(
        it,
        ImageUse::None | ImageUse::Used { disks: None, .. } | ImageUse::Resumed { disks: None, .. }
    )
}

const HEADS: [&str; 3] = ["main", "b", "pr/1"];

/// One step of a history, made from the random numbers once and done to every VFS alike.
pub(crate) enum Act {
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

pub(crate) fn act(rng: &mut Rng, step: usize) -> Act {
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
pub(crate) fn apply(vfs: &mut Vfs, act: &Act) -> String {
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
            // An image is written only of a quiet journal: what was committed is durable first.
            0 => {
                settle(&mut sim, &mut imaged);
                match image_of(&mut imaged) {
                    Ok((mark, bytes)) => {
                        image = Some(bytes);
                        images += 1;
                        assert!(imaged.image_durable(mark.name), "seed {seed}: {step}");
                    }
                    Err(ImageError::Busy) => {}
                    Err(error) => panic!("seed {seed}: {step}: {error}"),
                }
            }
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

/// `main` with a small file and one spanning four page groups, its image durable (its texts
/// in `texts`) and the journal rewritten onto it.
fn imaged_in(texts: &dyn Texts) -> (Vfs, Vec<u8>, ImageMark, Vec<u8>) {
    let (mut vfs, mut sim) = start(1);
    let big: Vec<u8> = (0..4 << MIN_GROUP_SHIFT).map(|at: u32| (at % 253) as u8).collect();
    let ops = [
        Op::Write { mode: Mode::Regular, path: "small".into(), bytes: b"small".to_vec() },
        Op::Write { mode: Mode::Regular, path: "dir/big".into(), bytes: big.clone() },
    ];
    vfs.commit("main", vfs.empty(), &ops).unwrap();
    settle(&mut sim, &mut vfs);
    let (mark, image) = image_in(&mut vfs, texts).unwrap();
    assert!(vfs.image_durable(mark.name));
    assert!(!vfs.image_durable(ContentId([7; 32])), "only the image last written");
    settle(&mut sim, &mut vfs);
    (vfs, image, mark, sim.contents(&file()))
}

/// [`imaged_in`] the tests' store.
fn imaged() -> (Vfs, Vec<u8>, ImageMark, Vec<u8>) {
    imaged_in(&*texts())
}

/// The file node at `path` of `main`.
fn node_at(vfs: &Vfs, path: &str) -> NodeId {
    let root = vfs.root(vfs.head("main").unwrap()).unwrap();
    vfs.tree.entry(root, path).unwrap().0
}

/// The invariant: a start asks the store for no text; the first read of a file asks for that
/// file's alone, and a second read of it asks for nothing; the tree section is read whole.
/// The control: a store that answers a read with another file's text is refused, naming the
/// file (the read checks the text it is given, so a store or a key that is wrong is caught).
#[test]
fn a_start_reads_the_journals_tail_and_no_text_until_one_is_read() {
    let held = HeldTexts::shared();
    let (vfs, image, mark, journal) = imaged_in(&*held);
    assert_eq!(base_of(&journal), Some((mark.name, mark.nodes)));
    assert!(journal.len() < 512, "the journal holds no file the image has: {}", journal.len());
    // clone: a count of the test's store.
    let counted = Arc::new(Counted { held: Arc::clone(&held), gets: AtomicUsize::new(0) });
    let texts: Arc<dyn Texts> = counted.clone(); // clone: a count, kept to read the counter
    let (back, found) =
        Vfs::restore(file(), 0, &journal, Some(ImageFile { file: Mapping::copy(&image), texts }));
    assert_eq!(found.image, ImageUse::Used { name: mark.name, nodes: mark.nodes, disks: None });
    assert_eq!(heads(&back), heads(&vfs));
    assert_eq!(counted.gets.load(Ordering::Relaxed), 0, "a start reads no text");

    let root = back.root(back.head("main").unwrap()).unwrap();
    assert_eq!(back.tree.read(root, "small").unwrap(), Some(&b"small"[..]));
    assert_eq!(counted.gets.load(Ordering::Relaxed), 1, "one read pages one text");
    assert_eq!(back.tree.read(root, "small").unwrap(), Some(&b"small"[..]));
    assert_eq!(counted.gets.load(Ordering::Relaxed), 1, "a text paged in is kept");
    let mapped = Arc::clone(back.tree.table().mapped().expect("a mapped base"));
    let tree = mapped.image.section(SectionId::VFS_TREE, TREE_SCHEMA).unwrap();
    assert!((0..tree.groups).all(|group| mapped.image.is_checked(&tree, group)), "whole");

    // The control: the store answers `dir/big` with `small`'s bytes.
    let big = back.tree.id(node_at(&back, "dir/big"));
    held.replace(&big, b"small");
    let error = back.tree.read(root, "dir/big").unwrap_err();
    let found = ContentId::of(FILE_DOMAIN, b"small");
    assert_eq!(error.cause, Cause::Text { file: big, why: TextWhy::Mismatch { found, len: 5 } });
    assert!(error.to_string().contains(&format!("{big:x}")), "{error}");
}

#[test]
fn a_corrupt_disks_section_is_named_not_taken_for_another_schema() {
    let (_, image, _, journal) = imaged();
    let disks_at = {
        let opened = Image::open(Mapping::copy(&image)).unwrap();
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

/// The invariant: a file of the image whose text the store lacks reads as an error naming
/// that file's ID (never as other bytes, never from another place), the VFS says so, and
/// writing the same bytes again mends it. The control is the store before the text is
/// dropped: the same read gives the file.
#[test]
fn a_file_the_store_lacks_is_named_by_its_id_and_mended_by_writing_it_again() {
    let held = HeldTexts::shared();
    let (vfs, image, _, journal) = imaged_in(&*held);
    let at = |held: &Arc<HeldTexts>| {
        // clone: a count of the test's store.
        let texts: Arc<dyn Texts> = held.clone();
        Some(ImageFile { file: Mapping::copy(&image), texts })
    };
    let clean = Vfs::restore(file(), 0, &journal, at(&held)).0;
    let root = clean.root(clean.head("main").unwrap()).unwrap();
    let big = vfs.tree.read(vfs.root(vfs.head("main").unwrap()).unwrap(), "dir/big").unwrap();
    let big = big.expect("a file").to_vec();
    assert_eq!(clean.tree.read(root, "dir/big").unwrap(), Some(&big[..]), "the control");

    let id = clean.tree.id(node_at(&clean, "dir/big"));
    assert!(held.forget(&id));
    let (mut back, found) = Vfs::restore(file(), 0, &journal, at(&held));
    assert!(matches!(found.image, ImageUse::Used { .. }), "{found:?}");
    assert_eq!(back.corrupt(), None, "nothing read yet");
    assert_eq!(back.tree.read(root, "small").unwrap(), Some(&b"small"[..]));
    let error = back.tree.read(root, "dir/big").unwrap_err();
    assert_eq!(error.section, SectionId::VFS_TREE);
    assert_eq!(error.cause, Cause::Text { file: id, why: TextWhy::Missing });
    assert!(error.to_string().contains(&format!("{id:x}")), "the error names the file: {error}");
    assert_eq!(back.corrupt(), Some(error));
    assert_eq!(
        OpError::from(error),
        OpError::Corrupt { section: SectionId::VFS_TREE.0, group: error.group }
    );

    // The same bytes written again (a scan of the disk, a checkout) are the node's again.
    let ops = [Op::Write { mode: Mode::Regular, path: "dir/big".into(), bytes: big }];
    let main = back.head("main").unwrap();
    assert_eq!(back.commit("main", main, &ops).unwrap().root, main);
    assert_eq!(state(&back).files, state(&vfs).files);
    // An image is written only of a quiet journal: the commit is made durable first. It puts
    // the mended text back.
    settle(&mut Sim::new(5, Faults::default()), &mut back);
    assert!(image_in(&mut back, &*held).is_ok());
    assert!(held.has(&id), "the next image's write puts the text it names");
}

/// The invariant (persisted-state-round-trips): every file read from a restore of an image is,
/// byte for byte, the file the tree that wrote it reads, and the image holds none of their
/// bytes. The control: an image whose tree names an ID the store lacks fails that read,
/// naming the ID.
#[test]
fn a_restore_reads_every_file_as_the_tree_that_wrote_it() {
    let held = HeldTexts::shared();
    let (mut vfs, mut sim) = start(21);
    let mut rng = Rng::new(21);
    // alloc: a test's ops
    let ops: Vec<Op> = (0..40)
        .map(|at| Op::Write {
            mode: if at % 7 == 0 { Mode::Executable } else { Mode::Regular },
            path: format!("d{}/f{at}", at % 5),
            bytes: bytes(&mut rng),
        })
        .collect();
    vfs.commit("main", vfs.empty(), &ops).unwrap();
    settle(&mut sim, &mut vfs);
    let (mark, image) = image_in(&mut vfs, &*held).unwrap();
    assert!(vfs.image_durable(mark.name));
    settle(&mut sim, &mut vfs);
    for op in &ops {
        let Op::Write { bytes, .. } = op else { unreachable!("writes") };
        if bytes.len() >= 8 {
            assert!(!contains(&image, bytes), "the image holds a file's bytes");
        }
    }
    // clone: a count of the test's store.
    let texts: Arc<dyn Texts> = held.clone();
    let journal = sim.contents(&file());
    let (back, found) =
        Vfs::restore(file(), 0, &journal, Some(ImageFile { file: Mapping::copy(&image), texts }));
    assert!(matches!(found.image, ImageUse::Used { .. }), "{found:?}");
    assert_eq!(state(&back), state(&vfs), "every file read is the writer's, byte for byte");

    // The control.
    let gone = back.tree.id(node_at(&back, "d3/f3"));
    held.forget(&gone);
    // clone: a count of the test's store.
    let texts: Arc<dyn Texts> = held.clone();
    let (back, _) =
        Vfs::restore(file(), 0, &journal, Some(ImageFile { file: Mapping::copy(&image), texts }));
    let root = back.root(back.head("main").unwrap()).unwrap();
    let error = back.tree.read(root, "d3/f3").unwrap_err();
    assert_eq!(error.cause, Cause::Text { file: gone, why: TextWhy::Missing });
}

/// Whether `needle` is somewhere in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|it| it == needle)
}

/// The image of `n` files of `size` bytes each, under the same names.
fn image_of_files(n: usize, size: usize) -> Vec<u8> {
    let (mut vfs, mut sim) = start(31);
    // alloc: a test's ops
    let ops: Vec<Op> = (0..n)
        .map(|at| {
            let mut bytes = vec![(at % 251) as u8; size];
            bytes[..8].copy_from_slice(&(at as u64).to_le_bytes());
            Op::Write { mode: Mode::Regular, path: format!("lib/f{at:05}.rs"), bytes }
        })
        .collect();
    vfs.commit("main", vfs.empty(), &ops).unwrap();
    settle(&mut sim, &mut vfs);
    let held = HeldTexts::new();
    image_in(&mut vfs, &held).unwrap().1
}

/// The invariant: the image of a tree of N library files of B bytes in all is O(N),
/// independent of B: 1,000 files of 16 bytes and of 4 KiB give images of one length, about
/// 100 bytes a file. The control: an image that writes the texts back in (the layout before,
/// planted) grows with B, and the same check fails.
#[test]
fn an_image_is_o_of_its_files_not_of_their_bytes() {
    let (small, large) = (image_of_files(1000, 16), image_of_files(1000, 4 << 10));
    assert_eq!(small.len(), large.len(), "an image's length is independent of its texts'");
    assert!(large.len() < 1000 * 160, "about 100 bytes a file, not {}", large.len() / 1000);
    let text_bytes = |image: &[u8]| {
        let opened = Image::open(Mapping::copy(image)).unwrap();
        let tree = opened.section(SectionId::VFS_TREE, TREE_SCHEMA).unwrap();
        opened.slice::<u64>(&tree, 0, WORDS as u64).unwrap()[W_TEXT_BYTES]
    };
    assert_eq!(text_bytes(&large), 1000 * (4 << 10), "the header says the bytes it names");

    PLANT_TEXTS.with(|it| it.set(true));
    let planted = image_of_files(1000, 4 << 10);
    PLANT_TEXTS.with(|it| it.set(false));
    assert!(planted.len() > 1000 * (4 << 10), "the control writes the texts in");
    assert_ne!(small.len(), planted.len(), "the control is caught by the length check");
}

thread_local! {
    /// A planted bug, for [`an_image_is_o_of_its_files_not_of_their_bytes`]'s control: the
    /// image's writer writes every file's bytes into a section of their own again.
    pub(crate) static PLANT_TEXTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The invariant: an image is never written naming a text that is not durable in the store:
/// with the store's puts held back, the write is refused and nothing is written; once they are
/// durable the same write succeeds. The control: the store as it is (durable puts) writes it.
#[test]
fn an_image_waits_for_its_texts_to_be_durable() {
    let (mut vfs, mut sim) = start(41);
    let ops = [Op::Write { mode: Mode::Regular, path: "a".into(), bytes: b"a text".to_vec() }];
    vfs.commit("main", vfs.empty(), &ops).unwrap();
    settle(&mut sim, &mut vfs);
    let held = HeldTexts::new();
    held.hold_back(true);
    let mut builder = Builder::with_group_shift(MIN_GROUP_SHIFT);
    assert_eq!(vfs.write_image(&mut builder, &held), Err(ImageError::Storing));
    let unwritten = Image::open(Mapping::copy(&builder.finish().unwrap())).unwrap();
    assert!(
        matches!(unwritten.section(SectionId::VFS_TREE, TREE_SCHEMA), Err(Refused::Missing { .. })),
        "nothing is written of an image whose texts are not durable"
    );
    assert!(held.has(&ContentId::of(FILE_DOMAIN, b"a text")), "the text was put");
    held.hold_back(false);
    assert!(image_in(&mut vfs, &held).is_ok(), "the control: durable texts, an image");
}

/// The invariant (persisted-state-round-trips): an image of the layout before (tree 2, whose
/// files were ranges of a blobs section) is a named miss, refused by its schema, and the
/// journal is loaded alone, as `Vfs::recover` loads it; never a tree read as this layout. The
/// control: the same section under this layout's schema is not refused by schema.
#[test]
fn an_image_of_the_layout_before_is_refused_by_its_schema() {
    const TREE_2: blaze_format::SchemaHash = schema(
        "blaze v2 vfs tree 2: header [u64; 30]; ids ContentId; shapes [u32; 2]; entry names u32; \
         entry nodes u32; entry modes u8; files [u64; 2]; index u32; spans [u32; 2]; name bytes u8; \
         name index u32 by SipHash-1-3 under the header's key, blaze_ids::index_key of the names",
    );
    let (_, image, _, journal) = imaged();
    let opened = Image::open(Mapping::copy(&image)).unwrap();
    let section = opened.section(SectionId::VFS_TREE, TREE_SCHEMA).unwrap();
    let tree = opened.whole(&section).unwrap().to_vec();
    let old = |schema| {
        let mut builder = Builder::with_group_shift(MIN_GROUP_SHIFT);
        builder.add(SectionId::VFS_TREE, schema, &tree);
        builder.add(SectionId(0x0301), schema_blobs(), b"the bytes of every file");
        builder.finish().unwrap()
    };
    let (back, found) = Vfs::restore(file(), 0, &journal, Some(mapping(&old(TREE_2))));
    assert_eq!(
        found.image,
        ImageUse::Refused(Refused::Schema {
            id: SectionId::VFS_TREE,
            found: TREE_2,
            wanted: TREE_SCHEMA
        })
    );
    let (alone, want) = Vfs::recover(file(), 0, &journal);
    assert_eq!(heads(&back), heads(&alone), "the journal alone");
    assert_eq!(found.heads_dropped, want.heads_dropped);
    let (_, control) = Vfs::restore(file(), 0, &journal, Some(mapping(&old(TREE_SCHEMA))));
    assert!(!matches!(control.image, ImageUse::Refused(_)), "{:?}", control.image);
}

/// The schema the retired blobs section was written with.
fn schema_blobs() -> blaze_format::SchemaHash {
    schema("blaze v2 vfs blobs 1: file bytes")
}

#[test]
fn a_corrupt_tree_leaves_the_image_unused_and_says_so() {
    let (_, image, mark, journal) = imaged();
    let tree_at = {
        let opened = Image::open(Mapping::copy(&image)).unwrap();
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
    let (mut other, mut other_sim) = start(9);
    let ops = [Op::Write { mode: Mode::Regular, path: "x".into(), bytes: b"x".to_vec() }];
    other.commit("main", other.empty(), &ops).unwrap();
    settle(&mut other_sim, &mut other);
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

/// File names a checkout's author picked to share the hash the tree's and the image's indexes
/// of names were once placed by: naming them in a commit, and finding them in the image a
/// restart maps, each look at a bounded number of places, not at every name before them.
#[test]
fn names_with_one_old_hash_take_bounded_probes_in_the_tree_and_its_image() {
    let names = crate::probes::one_old_hash(4000);
    let bound = 16 * names.len() as u64;
    let (mut vfs, mut sim) = start(3);
    let ops: Vec<Op> = names // alloc: a test's ops
        .iter()
        .map(|name| Op::Write { mode: Mode::Regular, path: name.clone(), bytes: b"x".to_vec() })
        .collect();
    crate::probes::take();
    let began = std::time::Instant::now();
    vfs.commit("main", vfs.empty(), &ops).unwrap();
    let named = crate::probes::take();
    eprintln!(
        "flood: a commit of {} names with one old hash: {:?}, {named} names compared",
        names.len(),
        began.elapsed()
    );
    assert!(
        named <= bound,
        "a commit of {} names with one old hash compared {named} names, over {bound}",
        names.len()
    );
    settle(&mut sim, &mut vfs);
    let (mark, image) = image_of(&mut vfs).unwrap();
    let (mut twin, mut twin_sim) = start(4);
    twin.commit("main", twin.empty(), &ops).unwrap();
    settle(&mut twin_sim, &mut twin);
    assert!(
        image_of(&mut twin).unwrap().1 == image,
        "the same names give the same image, its key and index"
    );
    assert!(vfs.image_durable(mark.name));
    settle(&mut sim, &mut vfs);

    let (mut back, found) = Vfs::restore(file(), 0, &sim.contents(&file()), Some(mapping(&image)));
    assert!(matches!(found.image, ImageUse::Used { .. }), "{:?}", found.image);
    let kept = back.tree.table().names().len();
    crate::probes::take();
    let began = std::time::Instant::now();
    for name in &names {
        let id = back.tree.name(name);
        assert_eq!(back.tree.name_str(id), name);
        assert_eq!(back.tree.table().names().len(), kept, "{name} is found in the image");
    }
    let looked = crate::probes::take();
    eprintln!(
        "flood: finding {} names with one old hash in the image: {:?}, {looked} places",
        names.len(),
        began.elapsed()
    );
    assert!(
        looked <= bound,
        "finding {} names with one old hash in the image looked at {looked} places, over {bound}",
        names.len()
    );
    let root = back.root(back.head("main").unwrap()).unwrap();
    assert_eq!(back.tree.read(root, &names[17]).unwrap(), Some(&b"x"[..]));
}
