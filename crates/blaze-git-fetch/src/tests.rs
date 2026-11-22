//! The crate's promises, each with its oracle and a planted bug:
//!
//! - SHA-1 is FIPS 180-4's, and an object id is git's;
//! - a pack built here (whole objects, offset deltas, reference deltas before their bases)
//!   reads back to the files it was built from;
//!   through a base cache of any bound;
//! - a corrupted pack, a missing base and an oversized object are refused, and so is an
//!   object a lying server changed deep in a delta chain (the trailer made right): no object
//!   hashes to the id its tree names;
//! - a tree name that could leave the checkout is refused;
//! - every cap of [`PackLimits`] and [`TreeLimits`], drawn down to its smallest legal value
//!   (D165), refuses exactly what passes it and nothing under it, and the cache holds at most
//!   its bound;
//! - the protocol's requests are v2's bytes, and its answers are read as v2 says.
//!
//! Every plant is a [`GitFetchPlant`] passed through the index's type parameter (D165): each
//! oracle's `plants!` enum names the invariant each plant must trip.

use core::fmt;
use std::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec,
    vec::Vec as StdVec,
};

use blaze_own::{Alloc, Vec};
use blaze_sim::{Broken, Config, NoPlant, Planted, Point, Tape, check, controls};

use crate::{
    CacheStats, GitFetchPlant, Index, Kind, Mode, Oid, PackError, PackLimits, PackWhy, Pkt, PlantedIndex, TreeError,
    TreeLimits, TreeWhy, check_advertisement, fetch_request, fetched_pack, files, find_ref, ls_refs,
    sha1::{Sha1, sha1},
};

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

const PACK: PackLimits =
    PackLimits { most_objects: 10_000, most_object: 1 << 24, most_total: 1 << 26, most_depth: 64, most_cache: 1 << 20 };
const TREE: TreeLimits = TreeLimits { most_files: 10_000, most_path: 4096, most_depth: 64 };

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn sha1_known_answers() {
    assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
    assert_eq!(
        hex(&sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    let million = vec![b'a'; 1_000_000];
    assert_eq!(hex(&sha1(&million)), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    // Fed in odd pieces, the same.
    let mut h = Sha1::new();
    for piece in million.chunks(1013) {
        h.update(piece);
    }
    assert_eq!(hex(&h.finish()), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    // `git hash-object` of a file holding "hello world\n".
    assert_eq!(Oid::of(Kind::Blob, b"hello world\n").to_string(), "3b18e512dba79e4c8300dd08aeb37f8e728b8dad");
    assert_eq!(Oid::of(Kind::Tree, b"").to_string(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
}

// ---- a pack writer, for the tests only ----

fn zlib(bytes: &[u8]) -> StdVec<u8> {
    let mut out = Vec::new_in(heap());
    blaze_deflate::zlib(bytes, blaze_deflate::Level::Default, heap(), &mut out).expect("deflated");
    out.as_slice().to_vec()
}

fn entry_header(number: u8, size: usize, out: &mut StdVec<u8>) {
    let mut size = size as u64;
    let mut byte = (number << 4) | (size & 15) as u8;
    size >>= 4;
    while size > 0 {
        out.push(byte | 0x80);
        byte = (size & 0x7f) as u8;
        size >>= 7;
    }
    out.push(byte);
}

fn ofs(back: u64, out: &mut StdVec<u8>) {
    let mut bytes = vec![(back & 0x7f) as u8];
    let mut n = back >> 7;
    while n > 0 {
        n -= 1;
        bytes.push(0x80 | (n & 0x7f) as u8);
        n >>= 7;
    }
    bytes.reverse();
    out.extend_from_slice(&bytes);
}

fn varint(mut n: usize, out: &mut StdVec<u8>) {
    while n >= 0x80 {
        out.push((n & 0x7f) as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

#[derive(Clone, Debug)]
enum Op {
    Copy(usize, usize),
    Insert(StdVec<u8>),
}

fn delta(base: &[u8], ops: &[Op]) -> (StdVec<u8>, StdVec<u8>) {
    let mut target = StdVec::new();
    let mut body = StdVec::new();
    for op in ops {
        match op {
            Op::Copy(off, len) => {
                target.extend_from_slice(&base[*off..off + len]);
                let mut cmd = 0x80u8;
                let mut args = StdVec::new();
                for i in 0..4 {
                    let b = (off >> (8 * i)) as u8;
                    if b != 0 {
                        cmd |= 1 << i;
                        args.push(b);
                    }
                }
                for i in 0..3 {
                    let b = (len >> (8 * i)) as u8;
                    if b != 0 {
                        cmd |= 0x10 << i;
                        args.push(b);
                    }
                }
                body.push(cmd);
                body.extend_from_slice(&args);
            }
            Op::Insert(bytes) => {
                target.extend_from_slice(bytes);
                body.push(bytes.len() as u8);
                body.extend_from_slice(bytes);
            }
        }
    }
    let mut out = StdVec::new();
    varint(base.len(), &mut out);
    varint(target.len(), &mut out);
    out.extend_from_slice(&body);
    (out, target)
}

#[derive(Clone, Copy, Debug)]
enum How {
    Whole,
    Ofs(usize),
    Ref(usize),
}

/// A pack of `objects` (kind, bytes, how), reference deltas listed first when `early`.
fn write_pack(objects: &[(Kind, StdVec<u8>, How)], deltas: &[StdVec<u8>], early: &[bool]) -> StdVec<u8> {
    let number = |k: Kind| match k {
        Kind::Commit => 1,
        Kind::Tree => 2,
        Kind::Blob => 3,
        Kind::Tag => 4,
    };
    let mut order: StdVec<usize> = (0..objects.len()).filter(|&i| early[i]).collect();
    order.extend((0..objects.len()).filter(|&i| !early[i]));
    let mut out = StdVec::from(&b"PACK"[..]);
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    let mut at = vec![0u64; objects.len()];
    for i in order {
        at[i] = out.len() as u64;
        let (kind, bytes, how) = &objects[i];
        match how {
            How::Whole => {
                entry_header(number(*kind), bytes.len(), &mut out);
                out.extend_from_slice(&zlib(bytes));
            }
            How::Ofs(base) => {
                entry_header(6, deltas[i].len(), &mut out);
                ofs(at[i] - at[*base], &mut out);
                out.extend_from_slice(&zlib(&deltas[i]));
            }
            How::Ref(base) => {
                entry_header(7, deltas[i].len(), &mut out);
                out.extend_from_slice(&Oid::of(objects[*base].0, &objects[*base].1).0);
                out.extend_from_slice(&zlib(&deltas[i]));
            }
        }
    }
    let trailer = sha1(&out);
    out.extend_from_slice(&trailer);
    out
}

fn tree_bytes(entries: &mut StdVec<(String, u32, Oid)>) -> StdVec<u8> {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = StdVec::new();
    for (name, mode, oid) in entries.iter() {
        out.extend_from_slice(format!("{mode:o} {name}\0").as_bytes());
        out.extend_from_slice(&oid.0);
    }
    out
}

/// What a built pack holds, measured against each cap: the truth a refusal is checked by.
#[derive(Clone, Debug, Default)]
struct Stats {
    /// Entries in the pack.
    objects: u32,
    /// Every entry's inflated payload (an object's bytes, a delta's) and every object's size.
    sizes: StdVec<u64>,
    /// The bytes of all objects, resolved.
    total: u64,
    /// The longest delta chain.
    depth: u16,
    /// Files in the tree.
    files: u32,
    /// The longest file path.
    longest_path: u64,
    /// The most components of a directory that holds an entry.
    dir_depth: u16,
}

struct Built {
    pack: StdVec<u8>,
    commit: Oid,
    files: BTreeMap<String, (Mode, StdVec<u8>)>,
    stats: Stats,
}

const DIRS: [&str; 4] = ["", "src", "src/bin", "docs"];

/// A random commit: files in nested directories, many blobs deltas of earlier ones.
fn build(tape: &Tape) -> Built {
    let n = tape.int_in(1usize..=10, "files");
    let mut objects: StdVec<(Kind, StdVec<u8>, How)> = StdVec::new();
    let mut deltas: StdVec<StdVec<u8>> = StdVec::new();
    let mut early = StdVec::new();
    let mut files = BTreeMap::new();
    let mut dirs: BTreeMap<&str, StdVec<(String, u32, Oid)>> = BTreeMap::new();
    for i in 0..n {
        let dir = DIRS[tape.index(DIRS.len(), "dir")];
        let name = format!("f{i}.rs");
        let path = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
        let blobs: StdVec<usize> = (0..objects.len()).collect();
        let (content, how, d) = match blobs.is_empty() || !tape.coin("delta") {
            true => (tape.bytes(0..=600, "content"), How::Whole, StdVec::new()),
            false => {
                let base = blobs[tape.index(blobs.len(), "base")];
                let base_bytes = objects[base].1.clone();
                let ops = tape.vec(1..=8, "ops", |t| {
                    if !base_bytes.is_empty() && t.coin("copy") {
                        let off = t.index(base_bytes.len(), "offset");
                        let len = t.int_in(1..=(base_bytes.len() - off).min(300), "len");
                        Op::Copy(off, len)
                    } else {
                        Op::Insert(t.bytes(1..=100, "insert"))
                    }
                });
                let (d, target) = delta(&base_bytes, &ops);
                let how = if tape.coin("by ref") { How::Ref(base) } else { How::Ofs(base) };
                (target, how, d)
            }
        };
        early.push(matches!(how, How::Ref(_)) && tape.coin("early"));
        let mode = match tape.index(3, "mode") {
            0 => (Mode::File, 0o100644),
            1 => (Mode::Exec, 0o100755),
            _ => (Mode::Symlink, 0o120000),
        };
        let oid = Oid::of(Kind::Blob, &content);
        dirs.entry(dir).or_default().push((name, mode.1, oid));
        files.insert(path, (mode.0, content.clone()));
        objects.push((Kind::Blob, content, how));
        deltas.push(d);
    }
    // Trees, deepest first.
    let mut tree_of: BTreeMap<&str, Oid> = BTreeMap::new();
    for dir in ["src/bin", "docs", "src", ""] {
        let mut entries = dirs.remove(dir).unwrap_or_default();
        for (child, name) in [("src/bin", "bin"), ("src", "src"), ("docs", "docs")] {
            let parent = child.rsplit_once('/').map_or("", |(p, _)| p);
            if parent == dir
                && let Some(&oid) = tree_of.get(child)
            {
                entries.push((name.to_string(), 0o040000, oid));
            }
        }
        if entries.is_empty() && !dir.is_empty() {
            continue;
        }
        let bytes = tree_bytes(&mut entries);
        tree_of.insert(dir, Oid::of(Kind::Tree, &bytes));
        objects.push((Kind::Tree, bytes, How::Whole));
        deltas.push(StdVec::new());
        early.push(false);
    }
    let commit = format!("tree {}\nauthor a <a> 0 +0000\ncommitter a <a> 0 +0000\n\nm\n", tree_of[""]).into_bytes();
    let commit_oid = Oid::of(Kind::Commit, &commit);
    objects.push((Kind::Commit, commit, How::Whole));
    deltas.push(StdVec::new());
    early.push(false);
    let stats = measure(&objects, &deltas, &files);
    Built { pack: write_pack(&objects, &deltas, &early), commit: commit_oid, files, stats }
}

/// The pack's and the tree's measures, from what was built.
fn measure(
    objects: &[(Kind, StdVec<u8>, How)],
    deltas: &[StdVec<u8>],
    files: &BTreeMap<String, (Mode, StdVec<u8>)>,
) -> Stats {
    let mut stats = Stats { objects: objects.len() as u32, ..Stats::default() };
    let mut depth: StdVec<u16> = StdVec::new();
    for (at, (_, bytes, how)) in objects.iter().enumerate() {
        stats.sizes.push(bytes.len() as u64);
        stats.total += bytes.len() as u64;
        let deep = match how {
            How::Whole => 0,
            How::Ofs(base) | How::Ref(base) => {
                stats.sizes.push(deltas[at].len() as u64);
                depth[*base] + 1
            }
        };
        stats.depth = stats.depth.max(deep);
        depth.push(deep);
    }
    for path in files.keys() {
        stats.files += 1;
        stats.longest_path = stats.longest_path.max(path.len() as u64);
        stats.dir_depth = stats.dir_depth.max(path.matches('/').count() as u16);
    }
    stats
}

/// Why reading a built commit back stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReadBack {
    /// The pack was refused while indexing.
    Pack(PackError),
    /// The tree walk refused.
    Tree(TreeError),
    /// A file's blob did not read back.
    Read(PackError),
}

impl fmt::Display for ReadBack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadBack::Pack(e) => write!(f, "indexing refused: {e}"),
            ReadBack::Tree(e) => write!(f, "the tree walk refused: {e}"),
            ReadBack::Read(e) => write!(f, "a blob's read refused: {e}"),
        }
    }
}

type Files = BTreeMap<String, (Mode, StdVec<u8>)>;

/// Every file of the built commit read back through an index under `pack` and a walk under
/// `tree`, the index's plant `plant`; and the cache's account.
fn read_back(
    built: &Built,
    pack: PackLimits,
    tree: TreeLimits,
    plant: impl Planted<GitFetchPlant>,
) -> Result<(Files, CacheStats), ReadBack> {
    let mut index = PlantedIndex::new_planted(&built.pack, pack, heap(), plant).map_err(ReadBack::Pack)?;
    let mut listed = StdVec::new();
    files(&mut index, built.commit, tree, heap(), |file| {
        listed.push((file.path.to_string(), file.mode, file.object));
    })
    .map_err(ReadBack::Tree)?;
    let mut got = BTreeMap::new();
    for (path, mode, object) in listed {
        let mut bytes = vec![0u8; object.size as usize];
        index.read(object, &mut bytes).map_err(ReadBack::Read)?;
        got.insert(path, (mode, bytes));
    }
    Ok((got, index.stats()))
}

const CACHES: [u64; 3] = [0, 700, 1 << 20];

/// The round trip's invariants.
#[derive(Clone, PartialEq, Eq, Debug)]
enum TripBroken {
    /// The files read back are not those packed.
    Differs,
    /// A well-formed pack was refused.
    Refused { why: ReadBack },
}

impl fmt::Display for TripBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TripBroken::Differs => f.write_str("the files read back differ from those packed"),
            TripBroken::Refused { why } => write!(f, "the files read back differ: refused: {why}"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs the round trip's control plants.
    enum TripPlant breaks TripBroken {
        /// The reader as it is.
        None,
        /// A delta's copy starts one byte late: the blob it makes hashes to no id the tree names
        /// (or a reference delta's base is then not in the pack).
        CopyOffsetOffByOne => TripBroken::Refused {
            why: ReadBack::Tree(TreeError { why: TreeWhy::Missing { .. }, .. })
                | ReadBack::Pack(PackError { why: PackWhy::MissingBase { .. }, .. }),
        },
    }
}

impl TripPlant {
    fn planted(self) -> GitFetchPlant {
        match self {
            TripPlant::None => GitFetchPlant::None,
            TripPlant::CopyOffsetOffByOne => GitFetchPlant::CopyOffsetOffByOne,
        }
    }
}

fn round_trip(tape: &Tape, plant: impl Planted<GitFetchPlant>) -> Result<(), Broken<TripBroken>> {
    let built = build(tape);
    let cache = CACHES[tape.index(CACHES.len(), "cache")];
    match read_back(&built, PackLimits { most_cache: cache, ..PACK }, TREE, plant) {
        Ok((got, _)) if got == built.files => Ok(()),
        Ok(_) => Err(Broken::Oracle(TripBroken::Differs)),
        Err(why) => Err(Broken::Oracle(TripBroken::Refused { why })),
    }
}

#[test]
fn a_pack_with_deltas_reads_back_to_its_files() {
    check(Config::cases(64), |tape| round_trip(tape, NoPlant));
}

#[test]
fn every_round_trip_plant_is_caught_by_its_invariant() {
    controls(Config::cases(64), |tape, plant: TripPlant| round_trip(tape, plant.planted()));
}

/// The trailer's invariant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CorruptBroken {
    /// A byte flipped at `at` was not refused by the trailer: indexed, or refused for another
    /// reason (`refused`).
    NotRefused { at: usize, refused: Option<PackError> },
}

impl fmt::Display for CorruptBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CorruptBroken::NotRefused { at, refused: None } => {
                write!(f, "a byte flipped at {at} was not refused by the trailer: indexed")
            }
            CorruptBroken::NotRefused { at, refused: Some(e) } => {
                write!(f, "a byte flipped at {at} was not refused by the trailer, but: {e}")
            }
        }
    }
}

blaze_sim::plants! {
    /// The bugs the corrupted pack's control plants.
    enum CorruptPlant breaks CorruptBroken {
        /// The reader as it is.
        None,
        /// The trailer is not checked.
        TrailerUnchecked => CorruptBroken::NotRefused { .. },
    }
}

impl CorruptPlant {
    fn planted(self) -> GitFetchPlant {
        match self {
            CorruptPlant::None => GitFetchPlant::None,
            CorruptPlant::TrailerUnchecked => GitFetchPlant::TrailerUnchecked,
        }
    }
}

fn corruption(tape: &Tape, plant: impl Planted<GitFetchPlant>) -> Result<(), Broken<CorruptBroken>> {
    let mut built = build(tape);
    // A byte flipped in an entry, past the header: the trailer must catch it.
    let at = 12 + tape.index(built.pack.len() - 32, "byte");
    built.pack[at] ^= 1 << tape.index(8, "bit");
    match PlantedIndex::new_planted(&built.pack, PACK, heap(), plant) {
        Err(e) if e.why == PackWhy::Checksum => Ok(()),
        Err(e) => Err(Broken::Oracle(CorruptBroken::NotRefused { at, refused: Some(e) })),
        Ok(_) => Err(Broken::Oracle(CorruptBroken::NotRefused { at, refused: None })),
    }
}

/// A pack whose last blob is a delta three deep, then that delta's inserted bytes changed and
/// the trailer made right again (a server that lies, not a wire that flips): the tree still
/// names the honest blob's id, which no object of the pack now hashes to.
fn deep_chain(tape: &Tape) -> (Built, usize) {
    let mut objects: StdVec<(Kind, StdVec<u8>, How)> = StdVec::new();
    let mut deltas: StdVec<StdVec<u8>> = StdVec::new();
    let mut content = tape.bytes(64..=400, "base");
    objects.push((Kind::Blob, content.clone(), How::Whole));
    deltas.push(StdVec::new());
    for depth in 0..3 {
        let insert = tape.bytes(8..=40, "insert");
        let (d, target) = delta(&content, &[Op::Copy(0, content.len() / 2), Op::Insert(insert)]);
        let how = if tape.coin("by ref") { How::Ref(depth) } else { How::Ofs(depth) };
        objects.push((Kind::Blob, target.clone(), how));
        deltas.push(d);
        content = target;
    }
    let deepest = objects.len() - 1;
    let mut entries = StdVec::new();
    for (at, (_, bytes, _)) in objects.iter().enumerate() {
        entries.push((format!("f{at}"), 0o100644, Oid::of(Kind::Blob, bytes)));
    }
    let tree = tree_bytes(&mut entries);
    let commit = format!("tree {}\n\nm\n", Oid::of(Kind::Tree, &tree)).into_bytes();
    let commit_oid = Oid::of(Kind::Commit, &commit);
    objects.push((Kind::Tree, tree, How::Whole));
    objects.push((Kind::Commit, commit, How::Whole));
    deltas.push(StdVec::new());
    deltas.push(StdVec::new());
    let mut files = BTreeMap::new();
    for (at, (_, bytes, _)) in objects.iter().take(deepest + 1).enumerate() {
        files.insert(format!("f{at}"), (Mode::File, bytes.clone()));
    }
    // The deepest delta's last insert changed: one byte of what it adds.
    let last = deltas[deepest].len() - 1;
    deltas[deepest][last] ^= 1 << tape.index(8, "bit");
    let early = vec![false; objects.len()];
    let stats = measure(&objects, &deltas, &files);
    (Built { pack: write_pack(&objects, &deltas, &early), commit: commit_oid, files, stats }, deepest)
}

/// The invariants of an object corrupted deep in a chain.
#[derive(Clone, PartialEq, Eq, Debug)]
enum DeepBroken {
    /// Refused, but not because the honest id names nothing in the pack.
    OtherRefusal { why: ReadBack },
    /// The corrupted object was accepted.
    Accepted,
}

impl fmt::Display for DeepBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeepBroken::OtherRefusal { why } => write!(f, "refused for another reason: {why}"),
            DeepBroken::Accepted => f.write_str("a corrupted object deep in a chain was accepted: not refused"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs the deep chain's control plants.
    enum DeepPlant breaks DeepBroken {
        /// The reader as it is.
        None,
        /// A lookup that misses takes the entry where the id would be.
        NearestId => DeepBroken::Accepted,
    }
}

impl DeepPlant {
    fn planted(self) -> GitFetchPlant {
        match self {
            DeepPlant::None => GitFetchPlant::None,
            DeepPlant::NearestId => GitFetchPlant::NearestId,
        }
    }
}

fn corrupted_deep_in_a_chain(tape: &Tape, plant: impl Planted<GitFetchPlant>) -> Result<(), Broken<DeepBroken>> {
    let (built, deepest) = deep_chain(tape);
    let cache = CACHES[tape.index(CACHES.len(), "cache")];
    match read_back(&built, PackLimits { most_cache: cache, ..PACK }, TREE, plant) {
        Err(
            ReadBack::Tree(TreeError { why: TreeWhy::Missing { .. }, .. })
            | ReadBack::Pack(PackError { why: PackWhy::MissingBase { .. }, .. }),
        ) => Ok(()),
        Err(why) => Err(Broken::Oracle(DeepBroken::OtherRefusal { why })),
        Ok((got, _)) if got.get(&format!("f{deepest}")) == built.files.get(&format!("f{deepest}")) => {
            Err(Broken::Harness(String::from("the corruption did not change the blob")))
        }
        Ok(_) => Err(Broken::Oracle(DeepBroken::Accepted)),
    }
}

#[test]
fn an_object_corrupted_deep_in_a_delta_chain_is_refused() {
    check(Config::cases(64), |tape| corrupted_deep_in_a_chain(tape, NoPlant));
}

#[test]
fn every_deep_chain_plant_is_caught_by_its_invariant() {
    controls(Config::cases(64), |tape, plant: DeepPlant| corrupted_deep_in_a_chain(tape, plant.planted()));
}

#[test]
fn a_corrupted_pack_is_refused() {
    check(Config::cases(32), |tape| corruption(tape, NoPlant));
}

#[test]
fn every_corrupted_pack_plant_is_caught_by_its_invariant() {
    controls(Config::cases(32), |tape, plant: CorruptPlant| corruption(tape, plant.planted()));
}

#[test]
fn a_missing_base_and_an_oversized_object_are_refused() {
    let base = b"the base".to_vec();
    let (d, target) = delta(&base, &[Op::Copy(0, 3), Op::Insert(b"!".to_vec())]);
    // The delta's base is not in the pack: a thin pack.
    let objects = vec![(Kind::Blob, base, How::Whole), (Kind::Blob, target, How::Ref(0))];
    let full = write_pack(&objects, &[StdVec::new(), d], &[false, false]);
    let alone = {
        let mut out = StdVec::from(&full[..8]);
        out.extend_from_slice(&1u32.to_be_bytes());
        // The first entry: a one-byte header (blob, size 8) and its zlib stream.
        let first = 12 + 1 + zlib(b"the base").len();
        out.extend_from_slice(&full[first..full.len() - 20]);
        let trailer = sha1(&out);
        out.extend_from_slice(&trailer);
        out
    };
    let refused = Index::new_in(&alone, PACK, heap()).map(|_| ()).map_err(|e| e.why);
    assert_eq!(refused, Err(PackWhy::MissingBase { base: Oid::of(Kind::Blob, b"the base") }));
    let small = PackLimits { most_object: 4, ..PACK };
    let refused = Index::new_in(&full, small, heap()).map(|_| ()).map_err(|e| e.why);
    assert_eq!(refused, Err(PackWhy::ObjectOver { size: 8, most: 4 }));
    let mut ok = Index::new_in(&full, PACK, heap()).expect("read");
    let found = ok.find(&Oid::of(Kind::Blob, b"the!")).expect("found by its id");
    let mut bytes = [0u8; 4];
    ok.read(found, &mut bytes).expect("read back");
    assert_eq!(&bytes, b"the!");
    assert!(ok.read(found, &mut [0u8; 3]).is_err(), "a slice of another size is refused");
}

// ---- the caps, drawn (D165) ----

impl blaze_sim::Limits for PackLimits {
    const SHIPPED: PackLimits = PackLimits::SHIPPED;

    fn draw(tape: &Tape) -> PackLimits {
        let shipped = PackLimits::SHIPPED;
        PackLimits {
            most_objects: tape.limit(shipped.most_objects, 1, "most objects"),
            most_object: tape.limit(shipped.most_object, 1, "most bytes of one object"),
            most_total: tape.limit(shipped.most_total, 1, "most bytes of all objects"),
            most_depth: tape.limit(shipped.most_depth, 0, "most deltas on a chain"),
            most_cache: tape.limit(shipped.most_cache, 0, "most bytes cached"),
        }
    }
}

impl blaze_sim::Limits for TreeLimits {
    const SHIPPED: TreeLimits = TreeLimits::SHIPPED;

    fn draw(tape: &Tape) -> TreeLimits {
        let shipped = TreeLimits::SHIPPED;
        TreeLimits {
            most_files: tape.limit(shipped.most_files, 1, "most files"),
            most_path: tape.limit(shipped.most_path, 1, "most bytes of a path"),
            most_depth: tape.limit(shipped.most_depth, 0, "most directories deep"),
        }
    }
}

blaze_sim::points! {
    /// Where the caps' simulation has to get: each cap refusing, the cache at a lowered bound,
    /// and a commit read back whole.
    enum CapPoint {
        /// `PackLimits::most_objects` refused a pack.
        ObjectsOver,
        /// `PackLimits::most_object` refused an object or a delta.
        ObjectOver,
        /// `PackLimits::most_total` refused the objects' sum.
        TotalOver,
        /// `PackLimits::most_depth` refused a delta chain.
        DeltaDepthOver,
        /// A commit with deltas read back through a cache held to a lowered bound.
        CacheBound,
        /// `TreeLimits::most_files` refused a tree.
        FilesOver,
        /// `TreeLimits::most_path` refused a path.
        PathOver,
        /// `TreeLimits::most_depth` refused a directory.
        TreeDepthOver,
        /// The commit read back whole.
        ReadBack,
    }
}

/// The caps' invariants.
#[derive(Clone, PartialEq, Eq, Debug)]
enum CapBroken {
    /// Read back under the caps, but not the files packed.
    Differs,
    /// The cache held more than its bound.
    CacheOver { held: u64, most: u64 },
    /// A refusal no cap warrants: a cap's refusal of what does not pass it, or any other
    /// refusal of a well-formed pack.
    Unwarranted { why: ReadBack },
    /// Read back whole though the measure `cap` passed its cap.
    Unenforced { cap: &'static str, value: u64, most: u64 },
}

impl fmt::Display for CapBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapBroken::Differs => f.write_str("read back under the caps, the files differ from those packed"),
            CapBroken::CacheOver { held, most } => write!(f, "the cache held {held} bytes, past its bound of {most}"),
            CapBroken::Unwarranted { why } => write!(f, "a refusal no cap warrants: {why}"),
            CapBroken::Unenforced { cap, value, most } => {
                write!(f, "read back whole though its {cap} is {value}, past the cap of {most}")
            }
        }
    }
}

/// The number of caps one case may lower: five of the pack's, three of the tree's.
const CAPS: usize = 8;

/// A refusal of `built` checked against its measures: `Ok` with the point it reached when the
/// cap it names was passed, else the invariant it breaks.
fn warranted(stats: &Stats, why: ReadBack) -> Result<CapPoint, CapBroken> {
    let unwarranted = Err(CapBroken::Unwarranted { why });
    match why {
        ReadBack::Pack(e) | ReadBack::Read(e) => match e.why {
            PackWhy::ObjectsOver { count, most } if count == stats.objects && count > most => Ok(CapPoint::ObjectsOver),
            PackWhy::ObjectOver { size, most } if size > most && stats.sizes.contains(&size) => Ok(CapPoint::ObjectOver),
            PackWhy::TotalOver { most } if stats.total > most => Ok(CapPoint::TotalOver),
            PackWhy::DepthOver { most } if stats.depth > most => Ok(CapPoint::DeltaDepthOver),
            _ => unwarranted,
        },
        ReadBack::Tree(e) => match e.why {
            TreeWhy::FilesOver { most } if stats.files > most => Ok(CapPoint::FilesOver),
            TreeWhy::PathOver { most } if stats.longest_path > u64::from(most) => Ok(CapPoint::PathOver),
            TreeWhy::DepthOver { most } if stats.dir_depth > most => Ok(CapPoint::TreeDepthOver),
            _ => unwarranted,
        },
    }
}

/// The first measure of `stats` past its cap, if any.
fn passed(stats: &Stats, pack: PackLimits, tree: TreeLimits) -> Option<CapBroken> {
    let largest = stats.sizes.iter().copied().max().unwrap_or(0);
    let over = |cap, value: u64, most: u64| (value > most).then_some(CapBroken::Unenforced { cap, value, most });
    over("object count", u64::from(stats.objects), u64::from(pack.most_objects))
        .or_else(|| over("largest object", largest, pack.most_object))
        .or_else(|| over("total bytes", stats.total, pack.most_total))
        .or_else(|| over("delta depth", u64::from(stats.depth), u64::from(pack.most_depth)))
        .or_else(|| over("file count", u64::from(stats.files), u64::from(tree.most_files)))
        .or_else(|| over("longest path", stats.longest_path, u64::from(tree.most_path)))
        .or_else(|| over("directory depth", u64::from(stats.dir_depth), u64::from(tree.most_depth)))
}

/// One case: a commit, one cap of the eight drawn down (the rest shipped, so a case is at most
/// one cap's boundary), and the commit read back: a refusal must be warranted by the cap it
/// names, a read back must be whole, within every cap, through a cache held to its bound.
fn within_caps(tape: &Tape) -> Result<(), Broken<CapBroken>> {
    let built = build(tape);
    let drawn_pack: PackLimits = tape.limits();
    let drawn_tree: TreeLimits = tape.limits();
    let (mut pack, mut tree) = (PackLimits::SHIPPED, TreeLimits::SHIPPED);
    match tape.index(CAPS, "lowered cap") {
        0 => pack.most_objects = drawn_pack.most_objects,
        1 => pack.most_object = drawn_pack.most_object,
        2 => pack.most_total = drawn_pack.most_total,
        3 => pack.most_depth = drawn_pack.most_depth,
        4 => pack.most_cache = drawn_pack.most_cache,
        5 => tree.most_files = drawn_tree.most_files,
        6 => tree.most_path = drawn_tree.most_path,
        _ => tree.most_depth = drawn_tree.most_depth,
    }
    match read_back(&built, pack, tree, NoPlant) {
        Err(why) => {
            let point = warranted(&built.stats, why).map_err(Broken::Oracle)?;
            tape.reach(point);
            Ok(())
        }
        Ok((got, stats)) => {
            if got != built.files {
                return Err(Broken::Oracle(CapBroken::Differs));
            }
            if stats.held_most > pack.most_cache {
                return Err(Broken::Oracle(CapBroken::CacheOver { held: stats.held_most, most: pack.most_cache }));
            }
            if let Some(broken) = passed(&built.stats, pack, tree) {
                return Err(Broken::Oracle(broken));
            }
            if pack.most_cache < PackLimits::SHIPPED.most_cache && stats.misses > 0 {
                tape.reach(CapPoint::CacheBound);
            }
            tape.reach(CapPoint::ReadBack);
            Ok(())
        }
    }
}

#[test]
fn every_cap_refuses_exactly_what_passes_it() {
    check(Config::cases(512).must_reach_points(<CapPoint as Point>::IDS), within_caps);
}

// ---- tree names ----

fn one_entry_tree(name: &[u8], mode: u32) -> (StdVec<u8>, Oid) {
    let blob = b"x".to_vec();
    let mut tree = format!("{mode:o} ").into_bytes();
    tree.extend_from_slice(name);
    tree.push(0);
    tree.extend_from_slice(&Oid::of(Kind::Blob, &blob).0);
    let commit = format!("tree {}\n\nm\n", Oid::of(Kind::Tree, &tree)).into_bytes();
    let oid = Oid::of(Kind::Commit, &commit);
    let objects =
        vec![(Kind::Blob, blob, How::Whole), (Kind::Tree, tree, How::Whole), (Kind::Commit, commit, How::Whole)];
    (write_pack(&objects, &[StdVec::new(), StdVec::new(), StdVec::new()], &[false; 3]), oid)
}

fn walk(name: &[u8], mode: u32, plant: impl Planted<GitFetchPlant>) -> Result<usize, TreeWhy> {
    let (bytes, commit) = one_entry_tree(name, mode);
    let mut index = PlantedIndex::new_planted(&bytes, PACK, heap(), plant).expect("read");
    let mut n = 0;
    files(&mut index, commit, TREE, heap(), |_| n += 1).map_err(|e| e.why)?;
    Ok(n)
}

#[test]
fn names_that_leave_the_checkout_are_refused() {
    assert_eq!(walk(b"ok.rs", 0o100644, NoPlant), Ok(1));
    for name in [&b".."[..], b".", b".git", b".GIT", b"a\\b", b"", b"\xff"] {
        assert_eq!(walk(name, 0o100644, NoPlant), Err(TreeWhy::Name), "{name:?}");
    }
    assert_eq!(walk(b"sub", 0o160000, NoPlant), Err(TreeWhy::Submodule));
    assert_eq!(walk(b"odd", 0o100600, NoPlant), Err(TreeWhy::Mode { mode: 0o100600 }));
}

/// The invariant of the name `..`.
#[derive(Clone, PartialEq, Eq, Debug)]
enum ParentBroken {
    /// A tree entry `..` was not refused: walked, or refused for another reason.
    Taken { walked: Result<usize, TreeWhy> },
}

impl fmt::Display for ParentBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParentBroken::Taken { walked: Ok(n) } => write!(f, "a tree entry `..` was not refused: {n} files walked"),
            ParentBroken::Taken { walked: Err(_) } => {
                f.write_str("a tree entry `..` was not refused as a name, but for another reason")
            }
        }
    }
}

blaze_sim::plants! {
    /// The bugs the name control plants.
    enum ParentPlant breaks ParentBroken {
        /// The walk as it is.
        None,
        /// A tree entry named `..` is taken.
        ParentNameTaken => ParentBroken::Taken { .. },
    }
}

impl ParentPlant {
    fn planted(self) -> GitFetchPlant {
        match self {
            ParentPlant::None => GitFetchPlant::None,
            ParentPlant::ParentNameTaken => GitFetchPlant::ParentNameTaken,
        }
    }
}

fn parent_refused(tape: &Tape, plant: impl Planted<GitFetchPlant>) -> Result<(), Broken<ParentBroken>> {
    let mode = match tape.coin("executable") {
        true => 0o100755,
        false => 0o100644,
    };
    match walk(b"..", mode, plant) {
        Err(TreeWhy::Name) => Ok(()),
        walked => Err(Broken::Oracle(ParentBroken::Taken { walked })),
    }
}

#[test]
fn every_parent_name_plant_is_caught_by_its_invariant() {
    controls(Config::cases(1), |tape, plant: ParentPlant| parent_refused(tape, plant.planted()));
}

// ---- the protocol ----

fn pkt_lines(bytes: &[u8]) -> StdVec<String> {
    let mut lines = crate::Lines::new(bytes);
    let mut out = StdVec::new();
    while let Some(line) = lines.next_line().expect("pkt-line") {
        out.push(match line {
            Pkt::Data(d) => String::from_utf8_lossy(d).into_owned(),
            Pkt::Flush => String::from("<flush>"),
            Pkt::Delim => String::from("<delim>"),
            Pkt::End => String::from("<end>"),
        });
    }
    out
}

#[test]
fn requests_are_v2s_bytes() {
    let mut out = Vec::new_in(heap());
    let want = Oid::of(Kind::Blob, b"");
    fetch_request(want, 1, &mut out).expect("written");
    assert_eq!(
        pkt_lines(&out),
        [
            "command=fetch\n".to_string(),
            "agent=blaze\n".into(),
            "object-format=sha1\n".into(),
            "<delim>".into(),
            format!("want {want}\n"),
            "deepen 1\n".into(),
            "ofs-delta\n".into(),
            "no-progress\n".into(),
            "done\n".into(),
            "<flush>".into(),
        ]
    );
    assert!(out.starts_with(b"0012command=fetch\n"));
    let mut out = Vec::new_in(heap());
    ls_refs(&["refs/heads/main"], &mut out).expect("written");
    assert!(pkt_lines(&out).contains(&"ref-prefix refs/heads/main\n".to_string()));
}

fn pkt(payload: &[u8], out: &mut StdVec<u8>) {
    out.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
    out.extend_from_slice(payload);
}

#[test]
fn answers_are_read_as_v2_says() {
    let mut ad = StdVec::new();
    pkt(b"# service=git-upload-pack\n", &mut ad);
    ad.extend_from_slice(b"0000");
    pkt(b"version 2\n", &mut ad);
    pkt(b"agent=git/github\n", &mut ad);
    pkt(b"ls-refs=unborn\n", &mut ad);
    pkt(b"fetch=shallow wait-for-done filter\n", &mut ad);
    ad.extend_from_slice(b"0000");
    assert_eq!(check_advertisement(&ad), Ok(()));
    let no_shallow = {
        let mut ad = StdVec::new();
        pkt(b"version 2\n", &mut ad);
        pkt(b"ls-refs\n", &mut ad);
        pkt(b"fetch\n", &mut ad);
        ad.extend_from_slice(b"0000");
        ad
    };
    assert!(check_advertisement(&no_shallow).is_err());

    let tag = Oid::of(Kind::Tag, b"t");
    let commit = Oid::of(Kind::Commit, b"c");
    let mut refs = StdVec::new();
    pkt(format!("{commit} refs/heads/main\n").as_bytes(), &mut refs);
    pkt(format!("{tag} refs/tags/v1 peeled:{commit}\n").as_bytes(), &mut refs);
    refs.extend_from_slice(b"0000");
    assert_eq!(find_ref(&refs, "refs/tags/v1"), Ok(Some(commit)));
    assert_eq!(find_ref(&refs, "refs/heads/main"), Ok(Some(commit)));
    assert_eq!(find_ref(&refs, "refs/heads/other"), Ok(None));

    let mut answer = StdVec::new();
    pkt(b"shallow-info\n", &mut answer);
    pkt(format!("shallow {commit}\n").as_bytes(), &mut answer);
    answer.extend_from_slice(b"0001");
    pkt(b"packfile\n", &mut answer);
    pkt(b"\x01PACK", &mut answer);
    pkt(b"\x02counting objects\n", &mut answer);
    pkt(b"\x01rest", &mut answer);
    answer.extend_from_slice(b"0000");
    let mut got = Vec::new_in(heap());
    fetched_pack(&answer, 1 << 20, &mut got).expect("demuxed");
    assert_eq!(&got[..], b"PACKrest");
    let mut error = StdVec::new();
    pkt(b"packfile\n", &mut error);
    pkt(b"\x03fatal: no such object\n", &mut error);
    assert_eq!(fetched_pack(&error, 1 << 20, &mut Vec::new_in(heap())), Err(crate::PktWhy::Remote));
}
