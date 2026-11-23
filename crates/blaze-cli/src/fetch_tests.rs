//! The command adopts (fetch.md, "End state: dependencies on the command path", piece 2; D78): a
//! `Fetch` task whose unit the sources store lacks takes the archive cargo keeps on this machine
//! when, and only when, its SHA-256 is the lock's; the record it writes is the network path's
//! record of the same bytes, at the same key. No test here opens a socket: with `fetch`, the
//! adopted unit is answered by the machine's `lookup` before any request is made, and the
//! tampered archive is tried through the second place alone (`adopt_local`), then, in a build
//! without `fetch`, through the whole task. Piece 3: the record lives in the machine's sources
//! store under the home, so a second worktree of the project fetches and adopts nothing.

use blaze_drive::fetch_task;
use blaze_fetch::local::{Miss, Tried, Try};
use blaze_fetch::plan::Unit;
use blaze_own::{Alloc, String as OwnString, Vec as OwnVec};
use blaze_tar::{Make, Writer};
use blaze_text::Writes as _;
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, Durable, ReadWrite};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::{Bytes, TASK_FRAME};
use bstd::path::Path;
use bstd::test::BUDGET;

use super::{CargoCache, FetchEnv, Lockfile, STORE_PLACE, adopt_local, record_path, run, sources_dir};
use crate::work::Did;

/// The checkout, beneath the simulation's root, as `blaze check` started in `/` names it.
const CHECKOUT: &str = "one";
/// The user's home and cargo's home in the simulation (`HOME=/h`, `CARGO_HOME=/c`): the
/// simulation keeps every second tree beneath its one root (`Fs::home`).
const HOME: &str = "/h";
const CARGO_HOME: &str = "/c";
/// crates.io's folder in cargo's cache, as cargo 1.85 names it on this Mac.
const FOLDER: &str = "index.crates.io-1949cf8c6b5b557f";
/// The archive's place in the simulation: `<cargo home>/registry/cache/<folder>/c-1.0.0.crate`.
const ARCHIVE_AT: &str = "c/registry/cache/index.crates.io-1949cf8c6b5b557f/c-1.0.0.crate";

/// A `.crate`: a gzip of a tar whose every entry is under `c-1.0.0/` (the form crates.io serves).
fn archive() -> OwnVec<u8, Alloc<'static>> {
    let mut tar = OwnVec::new_in(Alloc::global());
    let mut w = Writer::new();
    w.entry("c-1.0.0", Make::Dir, &[], &mut tar).expect("a folder entry");
    w.entry("c-1.0.0/Cargo.toml", Make::File { exec: false }, b"[package]\nname = \"c\"\nversion = \"1.0.0\"\n", &mut tar)
        .expect("a manifest entry");
    w.entry("c-1.0.0/src/lib.rs", Make::File { exec: false }, b"pub fn a() {}\n", &mut tar).expect("a source entry");
    w.finish(&mut tar).expect("the tar's end");
    let mut gz = OwnVec::new_in(Alloc::global());
    blaze_deflate::gzip(tar.as_slice(), blaze_deflate::Level::Fast, Alloc::global(), &mut gz).expect("gzip");
    gz
}

/// The checkout's `Cargo.lock`: the member `app` and the registry package `c`, pinned by
/// `sha256` (the form cargo writes, version 4).
fn lock_text(sha256: &[u8; 32]) -> OwnString<Alloc<'static>> {
    let mut text = OwnString::new_in(Alloc::global());
    text.str("version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"c\",\n]\n\n")
        .str("[[package]]\nname = \"c\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"")
        .hex_bytes(sha256)
        .str("\"\n");
    text
}

fn write(io: &Io, cx: &Cx, path: &str, bytes: &[u8]) {
    io.need_fs().expect("files").root().publish(cx, &Path::new(path).expect("a path"), Content::bytes(bytes), Durable::No).expect("write");
}

/// What the `Fetch` task needs of the command, as main.rs gives it with `HOME=/h` and
/// `CARGO_HOME=/c`, started in `/`, in the checkout `checkout`.
fn env(io: &Io, checkout: &'static str) -> FetchEnv<'static> {
    FetchEnv {
        clock: io.need_clock().expect("a clock"),
        checkout,
        served: crate::engine_script_reads::Served::Simulated,
        cert_file: None,
        cert_dir: None,
        rustflags: blaze_manifest::rustflags::RustflagsEnv::default(),
        home: Some(HOME),
        cargo_home: Some(CARGO_HOME),
        start: Some("/"),
        toolchain: blaze_toolchain::PINNED,
        fs: io.need_fs().expect("files"),
        sdk: crate::sdk::NONE_IN_TESTS,
        exe_dir: Err(crate::fetch::ExeDirWhy::NotRead),
        lan_peer: crate::fetch::LanPeerConfig::DISABLED,
        heap: blaze_own::testing::heap(),
    }
}

/// The plan's unit `c`.
fn unit_c<'f>(units: &'f [Unit<'f, 'static>]) -> &'f Unit<'f, 'static> {
    units.iter().find(|unit| matches!(unit, Unit::Crate { name: "c", .. })).expect("the lock's registry unit `c`")
}

/// The machine's sources store under `HOME=/h` (`h/.blaze/v2/sources` in the simulation).
fn store(io: &Io, cx: &Cx) -> Dir<ReadWrite> {
    match sources_dir(cx, &env(io, CHECKOUT)) {
        Ok(store) => store,
        Err(error) => panic!("the sources store: {error:?}"),
    }
}

/// The record at `unit`'s key in `dir` (the sources store, or a checkout's old folder), if
/// one is there.
fn held_bytes(cx: &Cx, dir: &Dir<ReadWrite>, unit: &Unit<'_, '_>) -> Option<OwnVec<u8, Alloc<'static>>> {
    let path = record_path(&unit.key()).expect("a record's path");
    dir.stat(cx, &path).expect("stat")?;
    let mut bytes = Bytes::new_in(cx.frame(), "a source record, compared");
    dir.read_all(cx, &path, &mut bytes).expect("read");
    let mut out = OwnVec::new_in(Alloc::global());
    out.extend_from_slice(bytes.as_slice());
    Some(out)
}

/// One command's `Fetch` task of the unit `c` over the lock `lock`, in the checkout `checkout`:
/// what it did.
fn fetch_c<'b>(io: &Io, cx: &Cx, checkout: &'static str, lock: &[u8], held: &'b bstd::mem::Frame) -> (Did<'b>, blaze_ids::ContentId) {
    let mut lockfile = match Lockfile::read(lock, blaze_own::testing::heap(), cx.frame()) {
        Ok(lockfile) => lockfile,
        Err(words) => panic!("the lock is refused: {}", words.as_str()),
    };
    let units = lockfile.units;
    let name = fetch_task::name(unit_c(units));
    match run(cx, &mut lockfile, &env(io, checkout), name, held) {
        Ok(did) => (did, name.subject),
        Err(error) => panic!("the task stopped the command: {error:?}"),
    }
}

/// Whether a `Fetch` task answered its unit held, with no finding.
fn answered_held(did: &Did<'_>, subject: blaze_ids::ContentId) -> bool {
    matches!(did, Did::Done(record) if record.out == subject && record.findings.is_empty())
}

/// D78, the place believed by its pin only: with no record in the sources store and no network
/// asked, the `Fetch` task of `c` takes cargo's archive of it, because its SHA-256 is the lock's.
/// The task's out is its subject, which is how the `UnitGraph` task counts a package as held
/// (graph.rs `run`: `input.out == input.name.subject`), and it carries no finding (no
/// `cannot-fetch`, 13). The record written at the unit's key is the network path's record of the
/// same bytes (`unpack::crate_record`, which the machine's `Http` answer goes through), so no
/// reader can tell where it came from. Then cargo's archive is deleted and a second command asks
/// again: the sources store, the first place, answers it alone. Without the adoption (`lookup`
/// and `run` back to the store alone) the first task fails with finding 13, and this test with it.
///
/// Expected: `test fetch::tests::a_unit_in_cargos_cache_is_adopted_by_the_locks_pin_and_held ... ok`
#[test]
fn a_unit_in_cargos_cache_is_adopted_by_the_locks_pin_and_held() {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let crate_bytes = archive();
    let sha256 = blaze_sha256::digest(crate_bytes.as_slice());
    let lock = lock_text(&sha256);
    write(&io, &cx, ARCHIVE_AT, crate_bytes.as_slice());
    write(&io, &cx, "one/Cargo.lock", lock.as_str().as_bytes());

    let (did, subject) = fetch_c(&io, &cx, CHECKOUT, lock.as_str().as_bytes(), cx.frame());
    match did {
        Did::Done(record) => {
            assert_eq!(record.out, subject, "the unit is held: its out is its subject (graph.rs counts it handed)");
            assert!(record.findings.is_empty(), "no finding: {:?}", core::str::from_utf8(record.messages.as_slice()));
        }
        Did::Failed(failure) => panic!("the task failed: {failure:?}"),
        Did::FailedSaying(failure, words) | Did::Hole(failure, words) => panic!("the task failed: {failure:?}: {words}"),
        Did::Need(_) => panic!("a `Fetch` task needs nothing"),
    }

    let lockfile = match Lockfile::read(lock.as_str().as_bytes(), blaze_own::testing::heap(), cx.frame()) {
        Ok(lockfile) => lockfile,
        Err(words) => panic!("the lock is refused: {}", words.as_str()),
    };
    let unit = unit_c(lockfile.units);
    let held = held_bytes(&cx, &store(&io, &cx), unit).expect("the adopted record is at the unit's key in the sources store");
    let (network, _) = blaze_fetch::unpack::crate_record("c", "1.0.0", sha256, crate_bytes.as_slice(), &blaze_fetch::fetch::Limits::REAL.unpack, Alloc::global())
        .expect("the network path's record of the same bytes");
    assert_eq!(held.as_slice(), network.as_slice(), "the adopted record is the network path's, byte for byte");

    io.need_fs().expect("files").root().remove(&cx, &Path::new(ARCHIVE_AT).expect("a path"), Durable::No).expect("cargo's archive deleted");
    let (did, subject) = fetch_c(&io, &cx, CHECKOUT, lock.as_str().as_bytes(), cx.frame());
    assert!(answered_held(&did, subject), "the second command's task did not answer from the sources store");
}

/// D78, nothing believed of the place: cargo's archive of `c` with one byte changed (its last,
/// in gzip's trailer) is refused by its SHA-256 before a byte is inflated, nothing is written at
/// the unit's key, and the words of the finding name each place tried and what it gave: the
/// sources store absent, and the crates.io folder's archive with both digests. In a build
/// without `fetch` the whole task answers with finding 13 (`cannot-fetch`) carrying those
/// words, its out not its subject (graph.rs counts it refused). Taking the archive without the
/// check would write a record and answer held, and this test fails.
///
/// Expected: `test fetch::tests::a_tampered_archive_in_cargos_cache_is_refused_and_named ... ok`
#[test]
fn a_tampered_archive_in_cargos_cache_is_refused_and_named() {
    let io = Io::simulated(SimWorld::new(19, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let pinned = archive();
    let sha256 = blaze_sha256::digest(pinned.as_slice());
    let mut tampered = OwnVec::new_in(Alloc::global());
    tampered.extend_from_slice(pinned.as_slice());
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    let got = blaze_sha256::digest(tampered.as_slice());
    let lock = lock_text(&sha256);
    write(&io, &cx, ARCHIVE_AT, tampered.as_slice());
    write(&io, &cx, "one/Cargo.lock", lock.as_str().as_bytes());

    let root = store(&io, &cx);
    let lockfile = match Lockfile::read(lock.as_str().as_bytes(), blaze_own::testing::heap(), cx.frame()) {
        Ok(lockfile) => lockfile,
        Err(words) => panic!("the lock is refused: {}", words.as_str()),
    };
    let unit = unit_c(lockfile.units);
    let env = env(&io, CHECKOUT);
    let mut cargo = CargoCache { looked: None };
    let mut tries = OwnVec::new_in(Alloc::global());
    tries.push(Try { place: STORE_PLACE, miss: Miss::Absent });
    let adopted = adopt_local(&cx, &root, &mut cargo, &env, unit, &mut tries).expect("no error of the tool's");
    assert!(!adopted, "a tampered archive is never adopted");
    assert!(held_bytes(&cx, &root, unit).is_none(), "nothing is written at the unit's key");
    let mut words = OwnString::new_in(Alloc::global());
    words.show(&Tried { unit, tries: tries.as_slice() });
    let mut want = OwnString::new_in(Alloc::global());
    want.str("crate c 1.0.0: not on this machine (blaze's sources store: absent; /c/registry/cache/")
        .str(FOLDER)
        .str(": its sha256 is ")
        .hex_bytes(&got)
        .str(", not the lock's ")
        .hex_bytes(&sha256)
        .str(")");
    assert_eq!(words.as_str(), want.as_str());

    #[cfg(not(feature = "fetch"))]
    match fetch_c(&io, &cx, CHECKOUT, lock.as_str().as_bytes(), cx.frame()) {
        (Did::Done(record), subject) => {
            assert_ne!(record.out, subject, "a refused unit's out is not its subject");
            assert_eq!(record.findings.len(), 1, "one finding");
            assert_eq!(record.findings[0].0, super::CANNOT_FETCH);
            let said = core::str::from_utf8(record.messages.as_slice()).expect("UTF-8");
            want.str("; this build cannot fetch (built without `fetch`)");
            assert!(said.contains(want.as_str()), "the finding names every place tried: {said}");
        }
        _ => panic!("the task did not answer with its finding"),
    }
}

/// Sources in the home (fetch.md piece 3): two worktrees of one project on one machine
/// (`one` and `two`, as `git worktree add ../two` makes them), one home. Worktree one's `Fetch`
/// task adopts `c` from cargo's cache and writes its record in the machine's sources store,
/// `<home>/.blaze/v2/sources` (`h/.blaze/v2/sources` in the simulation), and nothing under the
/// checkout. Then cargo's archive is deleted, so no place but that store could give `c`, and
/// worktree two's `Fetch` task answers it held: it fetches and adopts nothing. The record's
/// place is checked before worktree two runs, so a store under the checkout (the old design)
/// fails here, naming it, before any task could ask the network. The record is published whole
/// under a temporary name and renamed onto its key (`write_record`, `Dir::publish`); two
/// racing commands are piece 4's simulation.
///
/// Expected: `test fetch::tests::a_second_worktree_fetches_and_adopts_nothing ... ok`
#[test]
fn a_second_worktree_fetches_and_adopts_nothing() {
    let io = Io::simulated(SimWorld::new(23, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let crate_bytes = archive();
    let lock = lock_text(&blaze_sha256::digest(crate_bytes.as_slice()));
    write(&io, &cx, ARCHIVE_AT, crate_bytes.as_slice());
    write(&io, &cx, "one/Cargo.lock", lock.as_str().as_bytes());
    write(&io, &cx, "two/Cargo.lock", lock.as_str().as_bytes());

    let (did, subject) = fetch_c(&io, &cx, "one", lock.as_str().as_bytes(), cx.frame());
    assert!(answered_held(&did, subject), "worktree one adopts `c` from cargo's cache");

    let lockfile = match Lockfile::read(lock.as_str().as_bytes(), blaze_own::testing::heap(), cx.frame()) {
        Ok(lockfile) => lockfile,
        Err(words) => panic!("the lock is refused: {}", words.as_str()),
    };
    let unit = unit_c(lockfile.units);
    let fs = io.need_fs().expect("files");
    assert!(held_bytes(&cx, &store(&io, &cx), unit).is_some(), "the record is in the home's sources store");
    for checkout in ["one", "two"] {
        let old = fs.open_dir(&cx, &Path::new(checkout).expect("a path").join(&Path::new(super::SOURCES).expect("a path"))).expect("open");
        assert!(held_bytes(&cx, &old, unit).is_none(), "no record under the checkout `{checkout}`");
    }

    fs.root().remove(&cx, &Path::new(ARCHIVE_AT).expect("a path"), Durable::No).expect("cargo's archive deleted");
    let (did, subject) = fetch_c(&io, &cx, "two", lock.as_str().as_bytes(), cx.frame());
    assert!(answered_held(&did, subject), "worktree two answers `c` from the home's store, with no place left to adopt or fetch it from");
}
