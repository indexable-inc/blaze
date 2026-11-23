//! The fetch machine against a simulated origin (rule 43, fetch.md "Proof"). A case draws a
//! handful of crates (some hostile: an entry outside the crate's folder, a gzip bomb, a tar
//! header claiming 4 GiB), a store already holding some of them, and an origin whose every
//! answer has its own fate: on time, late past its deadline, never, a dropped connection, a 503,
//! a 404, a body cut short, one byte flipped, another crate's bytes, the crate's own folder with
//! other contents, or a body past its limit. The run may crash at any effect; a calm run on the
//! same store follows, then a third that must ask for nothing.
//!
//! A third of the units are git commits, served as GitHub serves protocol v2 (an
//! advertisement, then a depth-1 `fetch` answer, its pack on side band 1 in pkt-lines, with
//! no length stated). Some are hostile: a tree entry named `..`, or an 8 MiB blob packed to a
//! few KiB. The same fates apply, with "another crate's bytes" as another commit's answer, and
//! "tampered" as one blob changed after its tree named it, the pack's trailer made right.
//!
//! The oracles: every record in the store is the truth for its key ("wrong bytes"); no hostile
//! crate is ever stored ("outside its folder", "a bomb was stored"); no allocation goes past
//! its bound ("allocated past its bound"); every run ends ("hang"); a calm run fetches every
//! honest crate ("calm run left"); the run after it asks for nothing ("asked again"); a run
//! never asks a unit again after a failure ("tried again": one try, D172). Each
//! planted bug of [`Plant`] is caught by the oracle that names it.

use std::alloc::{AllocError, Allocator, Global, Layout};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ptr::NonNull;

use blaze_ids::ContentId;
use blaze_manifest::lock::LockId;
use blaze_own::{Alloc, Erased, String as OwnString, Vec as OwnVec};
use blaze_sim::{Broke, Config, Tape};
use blaze_git_fetch::{Kind as GitKind, Oid, PackLimits, TreeLimits};
use blaze_tar::{Make, Writer};
use blaze_text::Writes as _;

use crate::fetch::{Effect, Limits, Machine, NetFailure, Outcome, Reply, StoreFailure, Why};
use crate::pack::PackView;
use crate::plan::Unit;
use crate::{PLANT, Plant, unpack};

/// An allocator that refuses, and counts, any one allocation past `most` bytes.
pub(super) struct Bounded {
    pub(super) most: usize,
    pub(super) over: Cell<usize>,
}

// SAFETY: every grant is Global's, passed through; a refusal grants nothing.
unsafe impl Allocator for &Bounded {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.size() > self.most {
            self.over.set(self.over.get().max(layout.size()));
            return Err(AllocError);
        }
        Global.allocate(layout)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller's contract; every pointer here came from Global.
        unsafe { Global.deallocate(ptr, layout) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hostile {
    No,
    /// An entry beside the crate's folder, which the lock's checksum covers (a hostile publisher).
    Outside,
    /// A small gzip that inflates past every limit.
    Bomb,
    /// A tar header that says its file is 4 GiB.
    Huge,
    /// A gzip trailer (ISIZE) that says the tar is near 4 GiB: the record's room is sized
    /// from it.
    TrailerHigh,
    /// A gzip trailer that says the tar is a byte shorter than it is.
    TrailerLow,
    /// A git tree with an entry named `..` (a commit the lock pins, whose files would leave
    /// the checkout).
    Escape,
    /// A git blob of 8 MiB of zeros, a few KiB packed: past every limit once inflated.
    BigBlob,
}

/// One unit of the origin: a crates.io crate, or a git commit (`git`).
struct Crate {
    name: std::string::String,
    version: std::string::String,
    hostile: Hostile,
    /// A crate's `.crate`; a git commit's `fetch` answer (its pack in pkt-lines).
    archive: std::vec::Vec<u8>,
    /// What a tampering origin serves: the same folder with other contents; for git, the
    /// same objects with one blob changed and the pack's trailer made right, so a blob no
    /// longer hashes to the id its tree names.
    tampered: std::vec::Vec<u8>,
    sha256: [u8; 32],
    /// The record an honest unit makes.
    truth: Option<std::vec::Vec<u8>>,
    git: Option<Git>,
}

struct Git {
    repo: std::string::String,
    commit: Oid,
    /// The protocol v2 advertisement.
    advert: std::vec::Vec<u8>,
    /// Another commit's `fetch` answer: what an origin serving the wrong commit sends.
    decoy: std::vec::Vec<u8>,
}

fn limits(tape: &Tape) -> Limits {
    let mut limits = Limits::REAL;
    limits.in_flight_most = 1 + tape.below(4, "in flight") as u32;
    limits.ask_ns = 1_000_000_000;
    limits.crate_most = 256 << 10;
    // Small enough that some answers pass a first request's `most` and some requests wait.
    limits.first_most = 256 + tape.below(8192, "first most");
    limits.bytes_most = 1024 + tape.below(64 << 10, "bytes most");
    limits.unpack.pack.most_files = 64;
    limits.unpack.pack.most_bytes = 128 << 10;
    limits.unpack.pack.most_path = 200;
    limits.unpack.most_inflated = 256 << 10;
    // Some records put from the heap, some made in place, by the size the trailer states.
    limits.reserve_from = tape.below(160 << 10, "reserve from");
    limits.advert_most = 16 << 10;
    limits.git_most = 256 << 10;
    // Some packs past a first request's `most`: asked again with the limit held to the budget.
    limits.git_first_most = 256 + tape.below(16 << 10, "git first most");
    limits.unpack.git_pack =
        PackLimits { most_objects: 256, most_object: 256 << 10, most_total: 1 << 20, most_depth: 16, most_cache: 64 << 10 };
    limits.unpack.git_tree = TreeLimits { most_files: 64, most_path: 200, most_depth: 16 };
    limits
}

/// Whether the machine can take `unit`'s answer under `limits` at all: a git pack's answer
/// states no length, so one past its first request's `most` is asked again with the limit held
/// to the budget, and one past that fails by name (`Limits::git_first_most`).
fn fits(unit: &Crate, limits: &Limits) -> bool {
    let len = unit.archive.len() as u64;
    unit.git.is_none() || len <= limits.git_first_most || len <= limits.git_most.min(limits.bytes_most)
}

// ---- a git origin: whole objects, a pack, its protocol v2 answer ----

fn zlib(bytes: &[u8]) -> std::vec::Vec<u8> {
    let mut out = OwnVec::new_in(Global);
    blaze_deflate::zlib(bytes, blaze_deflate::Level::Fast, Global, &mut out).unwrap();
    out.as_slice().to_vec()
}

/// A pack of whole `objects`, its SHA-1 trailer last.
fn pack_of(objects: &[(GitKind, &[u8])]) -> std::vec::Vec<u8> {
    let mut out = b"PACK".to_vec();
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    for (kind, bytes) in objects {
        let number: u8 = match kind {
            GitKind::Commit => 1,
            GitKind::Tree => 2,
            GitKind::Blob => 3,
            GitKind::Tag => 4,
        };
        let mut size = bytes.len() as u64;
        let mut byte = (number << 4) | (size & 15) as u8;
        size >>= 4;
        while size > 0 {
            out.push(byte | 0x80);
            byte = (size & 0x7f) as u8;
            size >>= 7;
        }
        out.push(byte);
        out.extend_from_slice(&zlib(bytes));
    }
    let trailer = blaze_git_fetch::sha1::sha1(&out);
    out.extend_from_slice(&trailer);
    out
}

fn tree_of(entries: &mut [(std::string::String, u32, Oid)]) -> std::vec::Vec<u8> {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = std::vec::Vec::new();
    for (name, mode, oid) in entries.iter() {
        out.extend_from_slice(format!("{mode:o} {name}\0").as_bytes());
        out.extend_from_slice(&oid.0);
    }
    out
}

fn pkt(payload: &[u8], out: &mut std::vec::Vec<u8>) {
    out.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
    out.extend_from_slice(payload);
}

/// The advertisement GitHub sends for protocol v2.
fn advert() -> std::vec::Vec<u8> {
    let mut out = std::vec::Vec::new();
    pkt(b"# service=git-upload-pack\n", &mut out);
    out.extend_from_slice(b"0000");
    pkt(b"version 2\n", &mut out);
    pkt(b"agent=git/sim\n", &mut out);
    pkt(b"ls-refs=unborn\n", &mut out);
    pkt(b"fetch=shallow wait-for-done filter\n", &mut out);
    out.extend_from_slice(b"0000");
    out
}

/// A depth-1 `fetch` answer of `commit`: its shallow-info, then `pack` on side band 1, cut
/// into pkt-lines, with progress on band 2 between.
fn answer_of(commit: Oid, pack: &[u8]) -> std::vec::Vec<u8> {
    let mut out = std::vec::Vec::new();
    pkt(b"shallow-info\n", &mut out);
    pkt(format!("shallow {commit}\n").as_bytes(), &mut out);
    out.extend_from_slice(b"0001");
    pkt(b"packfile\n", &mut out);
    for piece in pack.chunks(4096) {
        let mut line = vec![1u8];
        line.extend_from_slice(piece);
        pkt(&line, &mut out);
        pkt(b"\x02Counting objects\n", &mut out);
    }
    out.extend_from_slice(b"0000");
    out
}

/// A random commit, `hostile` as asked: its id, its honest answer and a tampered one.
fn build_commit(tape: &Tape, tag: &str, hostile: Hostile) -> (Oid, std::vec::Vec<u8>, std::vec::Vec<u8>) {
    let mut blobs: std::vec::Vec<(std::string::String, std::vec::Vec<u8>, u32)> = (0..1 + tape.below(3, "git files"))
        .map(|f| {
            let mut bytes = vec![0u8; tape.below(600, "git file len") as usize];
            tape.fill(&mut bytes, "git file bytes");
            let mode = if tape.one_in(5, "git exec") { 0o100755 } else { 0o100644 };
            (format!("f{f}.rs"), bytes, mode)
        })
        .collect();
    if hostile == Hostile::BigBlob {
        blobs.push(("big.rs".into(), vec![0u8; 8 << 20], 0o100644));
    }
    let manifest = format!("[package]\nname = \"{tag}\"\n").into_bytes();
    let objects_with = |blobs: &[(std::string::String, std::vec::Vec<u8>, u32)], changed: bool| {
        let mut src: std::vec::Vec<_> =
            blobs.iter().map(|(name, bytes, mode)| (name.clone(), *mode, Oid::of(GitKind::Blob, bytes))).collect();
        let src = tree_of(&mut src);
        let src_id = Oid::of(GitKind::Tree, &src);
        let mut root = vec![("Cargo.toml".to_string(), 0o100644, Oid::of(GitKind::Blob, &manifest)), ("src".to_string(), 0o40000, src_id)];
        if hostile == Hostile::Escape {
            root.push(("..".to_string(), 0o40000, src_id));
        }
        let root = tree_of(&mut root);
        let commit =
            format!("tree {}\nauthor a <a> 0 +0000\ncommitter a <a> 0 +0000\n\n{tag}\n", Oid::of(GitKind::Tree, &root)).into_bytes();
        let id = Oid::of(GitKind::Commit, &commit);
        // The tampered pack: one blob's bytes changed after its tree named it.
        let mut stored: std::vec::Vec<std::vec::Vec<u8>> = blobs.iter().map(|(_, bytes, _)| bytes.clone()).collect();
        if changed {
            stored[0].push(b'!');
        }
        let mut objects: std::vec::Vec<(GitKind, &[u8])> = stored.iter().map(|it| (GitKind::Blob, it.as_slice())).collect();
        objects.push((GitKind::Blob, &manifest));
        objects.push((GitKind::Tree, &src));
        objects.push((GitKind::Tree, &root));
        objects.push((GitKind::Commit, &commit));
        (id, answer_of(id, &pack_of(&objects)))
    };
    let (id, answer) = objects_with(&blobs, false);
    let (_, tampered) = objects_with(&blobs, true);
    (id, answer, tampered)
}

fn draw_git(tape: &Tape, at: usize, limits: &Limits) -> Crate {
    let name = format!("g{at}");
    let hostile = match tape.below(8, "git hostile") {
        0 => Hostile::Escape,
        1 => Hostile::BigBlob,
        _ => Hostile::No,
    };
    let (commit, archive, tampered) = build_commit(tape, &name, hostile);
    let (_, decoy, _) = build_commit(tape, "decoy", Hostile::No);
    let truth = (hostile == Hostile::No).then(|| {
        let (record, _) = unpack::git_record(commit, &archive, limits.git_most, &limits.unpack, blaze_own::testing::heap().alloc()).unwrap();
        record.as_slice().to_vec()
    });
    let repo = format!("https://git.example.test/{name}.git");
    Crate {
        name,
        version: "0.0.0".into(),
        hostile,
        archive,
        tampered,
        sha256: [0; 32],
        truth,
        git: Some(Git { repo, commit, advert: advert(), decoy }),
    }
}

/// The bound no allocation of a case may pass: well above every limit's, far below a lie's.
const BOUND: usize = 4 << 20;

fn tar_of(folder: &str, files: &[(std::string::String, std::vec::Vec<u8>, bool)], extra: Option<&str>) -> std::vec::Vec<u8> {
    let mut out = OwnVec::new_in(Global);
    let mut w = Writer::new();
    w.entry(folder, Make::Dir, &[], &mut out).unwrap();
    for (path, bytes, exec) in files {
        w.entry(&format!("{folder}/{path}"), Make::File { exec: *exec }, bytes, &mut out).unwrap();
    }
    if files.len() > 1 {
        // A link inside the folder, which is kept.
        w.entry(&format!("{folder}/link"), Make::Symlink { target: &files[0].0 }, &[], &mut out).unwrap();
    }
    if let Some(extra) = extra {
        w.entry(extra, Make::File { exec: false }, b"fn evil() {}", &mut out).unwrap();
    }
    w.finish(&mut out).unwrap();
    out.as_slice().to_vec()
}

fn gzip(bytes: &[u8]) -> std::vec::Vec<u8> {
    let mut out = OwnVec::new_in(Global);
    blaze_deflate::gzip(bytes, blaze_deflate::Level::Fast, Global, &mut out).unwrap();
    out.as_slice().to_vec()
}

fn draw_crate(tape: &Tape, at: usize, unpack: &unpack::Limits) -> Crate {
    let name = format!("c{at}");
    let version = format!("1.{}.0", tape.below(3, "minor"));
    let folder = format!("{name}-{version}");
    let hostile = match tape.below(12, "hostile") {
        0 => Hostile::Outside,
        1 => Hostile::Bomb,
        2 => Hostile::Huge,
        3 => Hostile::TrailerHigh,
        4 => Hostile::TrailerLow,
        _ => Hostile::No,
    };
    let files: std::vec::Vec<_> = (0..1 + tape.below(4, "files"))
        .map(|f| {
            let mut bytes = vec![0u8; tape.below(3000, "file len") as usize];
            tape.fill(&mut bytes, "file bytes");
            (format!("src/f{f}.rs"), bytes, tape.one_in(5, "exec"))
        })
        .collect();
    let mut other = files.clone();
    other[0].1.push(b'!');
    let tar = match hostile {
        Hostile::No | Hostile::Bomb | Hostile::TrailerHigh | Hostile::TrailerLow => tar_of(&folder, &files, None),
        Hostile::Outside => tar_of(&folder, &files, Some(&format!("{name}-evil/x.rs"))),
        Hostile::Escape | Hostile::BigBlob => unreachable!("a git commit's, never drawn for a crate"),
        Hostile::Huge => {
            let mut out = OwnVec::new_in(Global);
            let mut w = Writer::new();
            w.begin(&format!("{folder}/huge"), Make::File { exec: false }, 4 << 30, &mut out).unwrap();
            let mut tar = out.as_slice().to_vec();
            tar.extend_from_slice(&[7u8; 4096]);
            tar
        }
    };
    let mut archive = if hostile == Hostile::Bomb { gzip(&vec![0u8; 2 << 20]) } else { gzip(&tar) };
    // The trailer's ISIZE rewritten by a hostile publisher (the lock's checksum is over it).
    let isize_at = archive.len() - 4;
    let real = u32::from_le_bytes(archive[isize_at..].try_into().unwrap());
    match hostile {
        Hostile::TrailerHigh => archive[isize_at..].copy_from_slice(&(u32::MAX - 15).to_le_bytes()),
        Hostile::TrailerLow => archive[isize_at..].copy_from_slice(&(real - 1).to_le_bytes()),
        _ => {}
    }
    let tampered = gzip(&tar_of(&folder, &other, None));
    let sha256 = blaze_sha256::digest(&archive);
    let truth = (hostile == Hostile::No).then(|| {
        let (record, _) = unpack::crate_record(&name, &version, sha256, &archive, unpack, blaze_own::testing::heap().alloc()).unwrap();
        record.as_slice().to_vec()
    });
    Crate { name, version, hostile, archive, tampered, sha256, truth, git: None }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fate {
    Calm,
    Late,
    Lost,
    Connect,
    Reset,
    Busy,
    Missing,
    Short,
    Flipped,
    Wrong,
    Tampered,
    TooBig,
    TooBigBody,
}

const FATES: [Fate; 13] = [
    Fate::Calm,
    Fate::Late,
    Fate::Lost,
    Fate::Connect,
    Fate::Reset,
    Fate::Busy,
    Fate::Missing,
    Fate::Short,
    Fate::Flipped,
    Fate::Wrong,
    Fate::Tampered,
    Fate::TooBig,
    Fate::TooBigBody,
];

/// What a run did.
struct Ran {
    outcomes: std::vec::Vec<Outcome>,
    asked: u32,
    crashed: bool,
}

/// One run of the machine over `units` against the origin, the store kept across runs.
fn run(
    tape: &Tape,
    units: &[Unit<'_, '_>],
    crates: &[Crate],
    store: &mut BTreeMap<ContentId, std::vec::Vec<u8>>,
    limits: Limits,
    calm: bool,
    crash_at: Option<u32>,
    alloc: Alloc<'_>,
) -> Result<Ran, Broke> {
    // Every unit wanted at once (a whole lock), or wanted a few at a time as `Fetch` tasks
    // come, some after the machine was done with the rest.
    let staggered = !calm && tape.one_in(2, "staggered");
    let mut machine = match staggered {
        false => Machine::new(units, limits, alloc),
        true => Machine::wanting(units, limits, alloc),
    };
    let mut unwanted: std::vec::Vec<u32> = match staggered {
        false => std::vec::Vec::new(),
        true => (0..units.len() as u32).rev().collect(),
    };
    let mut now = 0u64;
    // Bodies a reservation may ask for, by unit (the last answer given).
    let mut kept: BTreeMap<u32, std::vec::Vec<u8>> = BTreeMap::new();
    // Answers on their way: (when, ask, fate).
    // Answers on their way: (when, ask, fate, the request's `most`, whether it asks for a git
    // advertisement).
    let mut wire: std::vec::Vec<(u64, crate::fetch::Ask, Fate, u64, bool)> = std::vec::Vec::new();
    let mut effects = 0u32;
    let mut asked = 0u32;
    for _ in 0..20_000 {
        if !unwanted.is_empty() && (tape.one_in(2, "want one") || machine.is_done()) {
            let unit = unwanted.pop().expect("not empty");
            if !machine.want(unit) || machine.want(unit) {
                return Err(Broke::Oracle(format!("unit {unit}: wanted first refused, or wanted twice taken")));
            }
        }
        while let Some(effect) = machine.poll(now) {
            effects += 1;
            if crash_at.is_some_and(|at| effects >= at) {
                return Ok(Ran { outcomes: machine.outcomes().to_vec(), asked, crashed: true });
            }
            match effect {
                Effect::Lookup { unit, key } => machine.looked(unit, store.contains_key(&key)),
                // This origin's machine holds no other tool's archives (the local step's own
                // simulation is `local_step`): every crate goes on to HTTPS.
                Effect::Local { unit } => {
                    if machine.local_found(unit, crate::fetch::LocalArchive::Absent, &[]) != crate::fetch::Adopt::Done {
                        return Err(Broke::Oracle(format!("unit {unit}: nothing found, yet a reservation asked")));
                    }
                }
                Effect::Put { unit, key, record } => {
                    store.insert(key, record.as_slice().to_vec());
                    machine.stored(unit, Ok(()));
                }
                // A reservation in place: zeros of the room asked (or a store that is full),
                // the record written into them, kept at the length the machine gives.
                Effect::Reserve { unit, key, room } => {
                    let Some(body) = kept.remove(&unit) else {
                        return Err(Broke::Oracle(format!("unit {unit}: a reservation with no answer kept")));
                    };
                    if room > 64 << 20 {
                        return Err(Broke::Oracle(format!("unit {unit}: a reservation of {room} bytes")));
                    }
                    if !calm && tape.below(16, "store full") == 0 {
                        _ = machine.reserved(unit, &body, Err(StoreFailure::Full));
                        continue;
                    }
                    let mut reserved = vec![0u8; room as usize];
                    if let Some(len) = machine.reserved(unit, &body, Ok(&mut reserved)) {
                        reserved.truncate(len as usize);
                        store.insert(key, reserved);
                        machine.stored(unit, Ok(()));
                    }
                }
                Effect::Http { ask, url, most, deadline_ns, .. } => {
                    asked += 1;
                    let (form, limit, advert) = match &crates[ask.unit as usize].git {
                        None => (url.as_str().starts_with(crate::fetch::CRATES), limits.crate_most, false),
                        Some(git) => {
                            let advert = url.as_str() == format!("{}/info/refs?service=git-upload-pack", git.repo);
                            let fetch = url.as_str() == format!("{}/git-upload-pack", git.repo);
                            (advert || fetch, if advert { limits.advert_most } else { limits.git_most }, advert)
                        }
                    };
                    if !form || most > limit || deadline_ns <= now {
                        return Err(Broke::Oracle(format!("a request off its form: {} most {most}", url.as_str())));
                    }
                    let fate = if calm { Fate::Calm } else { *tape.pick(&FATES, "fate").unwrap() };
                    let latency = match fate {
                        Fate::Late => limits.ask_ns + 1 + tape.below(limits.ask_ns, "late by"),
                        Fate::Lost => u64::MAX,
                        _ => tape.below(limits.ask_ns / 2, "latency"),
                    };
                    wire.push((now.saturating_add(latency), ask, fate, most, advert));
                }
                Effect::PeerProbe { .. } | Effect::Cancel { .. } => {}
            }
        }
        if machine.is_done() && !unwanted.is_empty() {
            continue;
        }
        if machine.is_done() {
            // Late answers still arrive, and must change nothing.
            for (_, ask, _, _, _) in wire.drain(..) {
                machine.answered(ask, Reply::Failed(NetFailure::Reset));
            }
            // The budget: requests' `most`s within it, and at most one record past it.
            let bound =
                limits.bytes_most.max(limits.crate_most).max(limits.git_most) + limits.unpack.pack.most_bytes + (64 << 10);
            if machine.in_flight_peak() > bound {
                return Err(Broke::Oracle(format!("bytes in flight {} past the budget's bound {bound}", machine.in_flight_peak())));
            }
            return Ok(Ran { outcomes: machine.outcomes().to_vec(), asked, crashed: false });
        }
        let next_wire = wire.iter().map(|it| it.0).filter(|it| *it != u64::MAX).min();
        let next = match (next_wire, machine.next_deadline()) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => return Err(Broke::Oracle("a hang: nothing in flight, nothing due, and not done".into())),
        };
        now = now.max(next);
        let mut at = 0;
        while at < wire.len() {
            if wire[at].0 > now {
                at += 1;
                continue;
            }
            let (_, ask, fate, most, advert) = wire.swap_remove(at);
            let unit = &crates[ask.unit as usize];
            let (mut body, limit) = match (&unit.git, advert) {
                (None, _) => (unit.archive.clone(), limits.crate_most),
                (Some(git), true) => (git.advert.clone(), limits.advert_most),
                (Some(_), false) => (unit.archive.clone(), limits.git_most),
            };
            // A git `fetch` answer is sent in chunks: it states no length.
            let chunked = unit.git.is_some() && !advert;
            let stated = |len: usize| (!chunked).then_some(len as u64);
            // As blaze-http does: a body past the request's `most` is not read, its stated
            // length said.
            let fate = match fate {
                Fate::Calm | Fate::Late if body.len() as u64 > most => {
                    machine.answered(ask, Reply::Failed(NetFailure::TooBig { said: stated(body.len()) }));
                    continue;
                }
                fate => fate,
            };
            let reply = match fate {
                Fate::Calm | Fate::Late => Reply::Body { status: 200, bytes: &body },
                Fate::Lost => unreachable!("never delivered"),
                Fate::Connect => Reply::Failed(NetFailure::Connect),
                Fate::Reset => Reply::Failed(NetFailure::Reset),
                Fate::Busy => Reply::Body { status: 503, bytes: b"busy" },
                Fate::Missing => Reply::Body { status: 404, bytes: b"no such crate" },
                Fate::Short => {
                    let keep = tape.below(body.len() as u64, "cut at") as usize;
                    body.truncate(keep);
                    Reply::Body { status: 200, bytes: &body }
                }
                Fate::Flipped => {
                    let at = tape.index(body.len(), "flip at");
                    body[at] ^= 1 << tape.below(8, "bit");
                    Reply::Body { status: 200, bytes: &body }
                }
                // Another crate's bytes; for git, another commit's answer (to an advertisement's
                // request too).
                Fate::Wrong => {
                    body = match &unit.git {
                        None => crates[(ask.unit as usize + 1) % crates.len()].archive.clone(),
                        Some(git) => git.decoy.clone(),
                    };
                    Reply::Body { status: 200, bytes: &body }
                }
                Fate::Tampered => {
                    body = unit.tampered.clone();
                    Reply::Body { status: 200, bytes: &body }
                }
                // The driver refused a body past the request's `most`, its length stated (as
                // the answer's real one, or a lie past the limit) or not.
                Fate::TooBig => Reply::Failed(NetFailure::TooBig {
                    said: match tape.below(3, "said") {
                        0 => None,
                        1 => stated(body.len()),
                        _ => Some(limit + 1),
                    },
                }),
                Fate::TooBigBody => {
                    body.resize(limit as usize + 1, 0);
                    Reply::Body { status: 200, bytes: &body }
                }
            };
            // As a driver does: the body is kept until a reservation asked for it is answered.
            if let Reply::Body { bytes, .. } = reply {
                kept.insert(ask.unit, bytes.to_vec());
            }
            machine.answered(ask, reply);
        }
    }
    Err(Broke::Oracle("a hang: the run took 20,000 steps".into()))
}

/// The store's records against the truth, and no hostile crate in it.
fn judge_store(units: &[Unit<'_, '_>], crates: &[Crate], store: &BTreeMap<ContentId, std::vec::Vec<u8>>) -> Result<(), Broke> {
    for (unit, krate) in units.iter().zip(crates) {
        let Some(held) = store.get(&unit.key()) else { continue };
        if PackView::open(held).is_err() {
            return Err(Broke::Oracle(format!("wrong bytes: the record of {} does not open", krate.name)));
        }
        match (&krate.truth, krate.hostile) {
            (Some(truth), _) if truth != held => {
                return Err(Broke::Oracle(format!("wrong bytes: the record of {} is not its crate's", krate.name)));
            }
            (Some(_), _) => {}
            (None, Hostile::Outside) => {
                return Err(Broke::Oracle(format!("an entry outside its folder was stored: {}", krate.name)));
            }
            (None, hostile) => return Err(Broke::Oracle(format!("a hostile crate ({hostile:?}) was stored: {}", krate.name))),
        }
    }
    Ok(())
}

fn case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    PLANT.set(Plant::None);
    let limits = limits(tape);
    let crates: std::vec::Vec<Crate> = (0..1 + tape.index(6, "crates"))
        .map(|at| match tape.one_in(3, "git") {
            true => draw_git(tape, at, &limits),
            false => draw_crate(tape, at, &limits.unpack),
        })
        .collect();
    PLANT.set(plant);
    let result = case_inner(tape, &crates, limits);
    PLANT.set(Plant::None);
    result
}

fn case_inner(tape: &Tape, crates: &[Crate], limits: Limits) -> Result<(), Broke> {
    let bounded = Bounded { most: BOUND, over: Cell::new(0) };
    let erased = Erased::new(&bounded);
    let alloc = erased.handle();
    let units: std::vec::Vec<Unit<'_, '_>> = crates
        .iter()
        .enumerate()
        .map(|(at, c)| match &c.git {
            Some(git) => Unit::Git { repo: &git.repo, commit: git.commit, packages: 1 },
            None => {
                let mut version = OwnString::new_in(alloc);
                version.str(&c.version);
                Unit::Crate { package: LockId(at as u32), name: &c.name, version, sha256: c.sha256 }
            }
        })
        .collect();
    let mut store = BTreeMap::new();
    // Some records held before: the truth, put there by an earlier run.
    for (unit, krate) in units.iter().zip(crates) {
        if let Some(truth) = &krate.truth
            && tape.one_in(4, "held before")
        {
            store.insert(unit.key(), truth.clone());
        }
    }
    let crash_at = tape.one_in(3, "crash").then(|| 1 + tape.below(12, "crash at") as u32);
    let first = run(tape, &units, crates, &mut store, limits, false, crash_at, alloc)?;
    judge_store(&units, crates, &store)?;
    // One try (D172): whatever the origin's fates, no unit is asked more than its resized asks
    // (a crate three times: the first, its stated length, the limit; a git commit four: its
    // advertisement and its pack, each once more past a first `most`). A failure is never
    // asked again in the run: a retry would pass this bound under a run of failing fates.
    let tried_most: u32 = crates.iter().map(|krate| if krate.git.is_some() { 4 } else { 3 }).sum();
    if first.asked > tried_most {
        return Err(Broke::Oracle(format!("tried again: {} asks in a run, past one try's {tried_most}", first.asked)));
    }
    if !first.crashed {
        for (outcome, krate) in first.outcomes.iter().zip(crates) {
            match (outcome, krate.hostile) {
                (Outcome::Pending, _) => return Err(Broke::Oracle(format!("a hang: {} has no outcome", krate.name))),
                (Outcome::Failed(Why::Mismatch { want, got }), _) if want == got => {
                    return Err(Broke::Oracle("a mismatch of equal digests".into()));
                }
                _ => {}
            }
        }
    }
    let calm = run(tape, &units, crates, &mut store, limits, true, None, alloc)?;
    judge_store(&units, crates, &store)?;
    for (outcome, krate) in calm.outcomes.iter().zip(crates) {
        let fine = matches!(outcome, Outcome::Present | Outcome::Fetched { .. });
        // A git answer past what the budget lets it ask for fails by name, honest or not.
        let past_budget = matches!(outcome, Outcome::Failed(Why::Net { failure: NetFailure::TooBig { said: None }, .. }));
        if !fits(krate, &limits) && !past_budget && !(fine && krate.hostile == Hostile::No) {
            return Err(Broke::Oracle(format!("{} past the budget ended {outcome:?}", krate.name)));
        }
        if !fits(krate, &limits) {
            continue;
        }
        if krate.hostile == Hostile::No && !fine {
            return Err(Broke::Oracle(format!("calm run left {} at {outcome:?}", krate.name)));
        }
        if krate.hostile != Hostile::No && !matches!(outcome, Outcome::Failed(Why::Unpack(_))) {
            return Err(Broke::Oracle(format!("a hostile unit ({:?}) of {} ended {outcome:?}", krate.hostile, krate.name)));
        }
    }
    let again = run(tape, &units, crates, &mut store, limits, true, None, alloc)?;
    let stored = |o: &Outcome| matches!(o, Outcome::Present | Outcome::Fetched { .. });
    let honest_left =
        calm.outcomes.iter().zip(&again.outcomes).any(|(calm, again)| stored(calm) && *again != Outcome::Present);
    // Each unit not stored is asked again: a crate at most three times (the first, its stated
    // length, the limit), a git commit at least twice (its advertisement, its pack) and at
    // most four (each once more past a first `most`).
    let (mut least, mut most) = (0u32, 0u32);
    for (outcome, krate) in calm.outcomes.iter().zip(crates) {
        if !stored(outcome) {
            let (l, m) = if krate.git.is_some() { (2, 4) } else { (1, 3) };
            least += l;
            most += m;
        }
    }
    if honest_left || again.asked < least || again.asked > most {
        return Err(Broke::Oracle(format!("asked again: {} asks, not {least} to {most}", again.asked)));
    }
    if bounded.over.get() > 0 {
        return Err(Broke::Oracle(format!("allocated past its bound: {} bytes asked at once", bounded.over.get())));
    }
    Ok(())
}

/// The invariant: a fetch stores only checksum- and path-verified records within allocation bounds, never trusts an unverified commit or wire size, and terminates under any origin fault or crash.
#[test]
fn a_fetch_stores_only_verified_records_and_ends_whatever_the_origin_does() {
    blaze_sim::check(Config::cases(300), |tape| case(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_pack_stored_before_its_checksum() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::StoreBeforeChecksum, "wrong bytes", case);
}

#[test]
fn the_oracle_catches_an_entry_outside_its_folder() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::EntryOutside, "outside its folder", case);
}

#[test]
fn the_oracle_catches_a_commit_taken_from_the_server() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::CommitFromServer, "wrong bytes", case);
}

#[test]
fn the_oracle_catches_a_size_from_the_wire_allocated_unchecked() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::SizeFromWire, "allocated past its bound", case);
}

/// A gzip trailer that lies about the tar's size, either way, is a typed refusal (the gzip
/// reader's `Size`) and makes no record; a lie upward reserves no more than the limits allow.
#[test]
fn a_gzip_trailer_that_lies_about_its_size_is_refused_either_way() {
    blaze_sim::check(Config::cases(200), |tape| {
        let unpack = limits(tape).unpack;
        let krate = draw_crate(tape, 0, &unpack);
        if !matches!(krate.hostile, Hostile::TrailerHigh | Hostile::TrailerLow) {
            return Ok(());
        }
        let bounded = Bounded { most: BOUND, over: Cell::new(0) };
        let erased = Erased::new(&bounded);
        let made = unpack::crate_record(&krate.name, &krate.version, krate.sha256, &krate.archive, &unpack, erased.handle());
        match made {
            Err(unpack::UnpackError::Gzip(e)) if matches!(e.why, blaze_deflate::Why::Size { .. }) => {}
            other => return Err(Broke::Oracle(format!("{:?}: not refused for its size: {:?}", krate.hostile, other.map(|_| ())))),
        }
        if bounded.over.get() > 0 {
            return Err(Broke::Oracle(format!("allocated past its bound: {} bytes asked at once", bounded.over.get())));
        }
        Ok(())
    });
}

/// The record's form: what is written opens, finds each file, and refuses a flipped byte.
#[test]
fn a_record_opens_finds_and_refuses_a_flipped_byte() {
    blaze_sim::check(Config::cases(200), |tape| {
        let unpack = limits(tape).unpack;
        let krate = draw_crate(tape, 0, &unpack);
        let Some(record) = &krate.truth else { return Ok(()) };
        let view = PackView::open(record).map_err(|e| Broke::Oracle(format!("{e:?}")))?;
        for file in view.files() {
            if view.find(file.path) != Some(file) {
                return Err(Broke::Oracle(format!("{} not found by its path", file.path)));
            }
        }
        if view.source() != &krate.sha256 {
            return Err(Broke::Oracle("the source digest is not the crate's".into()));
        }
        // The reader by the lock's id: these sources open, any other id's are refused.
        let id = crate::SourceId::Crate { name: &krate.name, version: &krate.version, sha256: krate.sha256 };
        let by_id = PackView::open_source(&id, record).map_err(|e| Broke::Oracle(format!("{e:?}")))?;
        for file in by_id.files() {
            if file.blake3 != crate::Blake3::of(file.bytes) {
                return Err(Broke::Oracle(format!("{}: the row's BLAKE3 is not its bytes'", file.path)));
            }
        }
        let mut other = krate.sha256;
        other[tape.index(32, "other sha byte")] ^= 1;
        let other = crate::SourceId::Crate { name: &krate.name, version: &krate.version, sha256: other };
        if PackView::open_source(&other, record).is_ok() {
            return Err(Broke::Oracle("a record opened for another crate's checksum".into()));
        }
        let mut flipped = record.clone();
        let at = tape.index(flipped.len(), "flip at");
        flipped[at] ^= 1;
        if PackView::open(&flipped).is_ok() {
            return Err(Broke::Oracle(format!("a record with byte {at} flipped opened")));
        }
        Ok(())
    });
}

/// The strategy is the sink alone (rules 25 and 78): a record made in place, in a reservation
/// of the bound `crate_room` states, is the heap's record byte for byte; one with too little
/// room is refused by name, never written past.
#[test]
fn a_record_is_the_same_bytes_in_the_heap_or_in_place() {
    blaze_sim::check(Config::cases(200), |tape| {
        let unpack = limits(tape).unpack;
        let krate = draw_crate(tape, 0, &unpack);
        let Some(truth) = &krate.truth else { return Ok(()) };
        let alloc = blaze_own::testing::heap().alloc();
        let room = unpack::crate_room(&krate.archive, &unpack);
        if room.most < truth.len() as u64 {
            return Err(Broke::Oracle(format!("a bound of {} under a record of {}", room.most, truth.len())));
        }
        let mut reserved = vec![0xa5u8; room.most as usize];
        let fixed = crate::pack::Fixed::new(&mut reserved);
        let (fixed, _) = unpack::crate_record_in(&krate.name, &krate.version, krate.sha256, &krate.archive, &unpack, fixed, alloc)
            .map_err(|e| Broke::Oracle(format!("in place: {e:?}")))?;
        let len = fixed.written() as usize;
        if reserved[..len] != truth[..] {
            return Err(Broke::Oracle(format!("in place, {len} bytes unlike the heap's {}", truth.len())));
        }
        let short = tape.below(truth.len() as u64, "short room") as usize;
        let mut small = vec![0u8; short];
        match unpack::crate_record_in(&krate.name, &krate.version, krate.sha256, &krate.archive, &unpack, crate::pack::Fixed::new(&mut small), alloc) {
            Err(unpack::UnpackError::Pack(crate::pack::PackRefused::RoomOver { room })) if room == short as u64 => Ok(()),
            other => Err(Broke::Oracle(format!("{short} bytes of room for a record of {}: {:?}", truth.len(), other.map(|(_, m)| m)))),
        }
    });
}

/// A lock through the plan: crates.io crates with their checksums, a git commit once for its
/// two packages, and each package that cannot be verified named.
#[test]
fn a_lock_plans_each_unit_once_and_names_what_it_skips() {
    let lock = br#"version = 4

[[package]]
name = "a"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"

[[package]]
name = "b"
version = "0.1.0"
source = "git+https://github.com/o/r?rev=abc#0123456789abcdef0123456789abcdef01234567"

[[package]]
name = "c"
version = "0.1.0"
source = "git+https://github.com/o/r?rev=abc#0123456789abcdef0123456789abcdef01234567"

[[package]]
name = "d"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "e"
version = "0.1.0"
source = "git+ssh://git@example.com/o/r#0123456789abcdef0123456789abcdef01234567"

[[package]]
name = "w"
version = "0.1.0"
"#;
    let lock = blaze_manifest::lock::Lock::read(lock, blaze_own::testing::heap().alloc()).unwrap();
    let plan = crate::plan::plan(&lock, blaze_own::testing::heap().alloc()).unwrap();
    assert_eq!((plan.packages, plan.paths, plan.units.len(), plan.skipped.len()), (6, 1, 2, 2));
    assert!(matches!(plan.units[0], Unit::Crate { name: "a", .. }));
    assert!(matches!(plan.units[1], Unit::Git { packages: 2, .. }));
    let mut text = OwnString::new_in(Global);
    for skip in plan.skipped.iter() {
        text.show(skip).str("\n");
    }
    let text = text.as_str();
    assert!(text.contains("d: the lock states no checksum"), "{text}");
    assert!(text.contains("e: git source ssh://"), "{text}");
}
