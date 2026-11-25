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
    let mut cargo = CargoCache::new();
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

fn git_pack_and_idx_of(objects: &[(blaze_git_fetch::Kind, &[u8])]) -> (OwnVec<u8, Alloc<'static>>, OwnVec<u8, Alloc<'static>>) {
    let mut pack = OwnVec::new_in(Alloc::global());
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&u32::try_from(objects.len()).unwrap_or(0).to_be_bytes());
    let mut entries: std::vec::Vec<(blaze_git_fetch::Oid, u32)> = std::vec::Vec::new();
    for (kind, bytes) in objects {
        let offset = u32::try_from(pack.len()).unwrap_or(0);
        entries.push((blaze_git_fetch::Oid::of(*kind, bytes), offset));
        let number: u8 = match kind {
            blaze_git_fetch::Kind::Commit => 1,
            blaze_git_fetch::Kind::Tree => 2,
            blaze_git_fetch::Kind::Blob => 3,
            blaze_git_fetch::Kind::Tag => 4,
        };
        let mut size = u64::try_from(bytes.len()).unwrap_or(0);
        let mut byte = (number << 4) | u8::try_from(size & 15).unwrap_or(0);
        size >>= 4;
        while size > 0 {
            pack.push(byte | 0x80);
            byte = u8::try_from(size & 0x7f).unwrap_or(0);
            size >>= 7;
        }
        pack.push(byte);
        let mut z = OwnVec::new_in(Alloc::global());
        assert!(blaze_deflate::zlib(bytes, blaze_deflate::Level::Fast, Alloc::global(), &mut z).is_ok());
        pack.extend_from_slice(z.as_slice());
    }
    let pack_sha = blaze_git_fetch::sha1::sha1(pack.as_slice());
    pack.extend_from_slice(&pack_sha);

    entries.sort_by(|a, b| a.0.0.cmp(&b.0.0));
    let mut idx = OwnVec::new_in(Alloc::global());
    idx.extend_from_slice(&[0xff, b't', b'O', b'c']);
    idx.extend_from_slice(&2u32.to_be_bytes());
    for first in 0u8..=255 {
        let count = entries.iter().filter(|(oid, _)| oid.0[0] <= first).count();
        idx.extend_from_slice(&u32::try_from(count).unwrap_or(0).to_be_bytes());
    }
    for (oid, _) in &entries {
        idx.extend_from_slice(&oid.0);
    }
    for _ in &entries {
        idx.extend_from_slice(&0u32.to_be_bytes());
    }
    for (_, offset) in &entries {
        idx.extend_from_slice(&offset.to_be_bytes());
    }
    idx.extend_from_slice(&pack_sha);
    let idx_sha = blaze_git_fetch::sha1::sha1(idx.as_slice());
    idx.extend_from_slice(&idx_sha);
    (pack, idx)
}

fn git_loose_of(kind: blaze_git_fetch::Kind, body: &[u8]) -> OwnVec<u8, Alloc<'static>> {
    let mut raw = OwnVec::new_in(Alloc::global());
    let header = std::format!("{} {}\0", kind.name(), body.len());
    raw.extend_from_slice(header.as_bytes());
    raw.extend_from_slice(body);
    let mut z = OwnVec::new_in(Alloc::global());
    assert!(blaze_deflate::zlib(raw.as_slice(), blaze_deflate::Level::Fast, Alloc::global(), &mut z).is_ok());
    z
}

fn git_tree_of(entries: &mut [(&str, u32, blaze_git_fetch::Oid)]) -> OwnVec<u8, Alloc<'static>> {
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = OwnVec::new_in(Alloc::global());
    for (name, mode, oid) in entries.iter() {
        let header = std::format!("{mode:o} {name}\0");
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&oid.0);
    }
    out
}

fn git_lock_text(commit: blaze_git_fetch::Oid) -> OwnString<Alloc<'static>> {
    let mut text = OwnString::new_in(Alloc::global());
    let s = std::format!(
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"g\",\n]\n\n[[package]]\nname = \"g\"\nversion = \"0.1.0\"\nsource = \"git+https://github.com/example/g.git?rev={commit}#{commit}\"\n"
    );
    text.str(s.as_str());
    text
}

fn unit_g<'f>(units: &'f [Unit<'f, 'static>]) -> Option<&'f Unit<'f, 'static>> {
    units.iter().find(|unit| matches!(unit, Unit::Git { .. }))
}

/// Piece 14: a git commit spread across loose objects and multiple `.idx`/`.pack` files in
/// `<cargo home>/git/db/<repo>-<16hex>/objects` is adopted on demand, equals `git_pack_record`
/// byte for byte, and is kept in the home's sources store so deleting cargo's git db leaves a
/// second command answered from the store with no network.
///
/// Expected: `test fetch::tests::a_git_commit_in_cargos_git_db_is_adopted_by_the_locks_commit_and_held ... ok`
#[test]
fn a_git_commit_in_cargos_git_db_is_adopted_by_the_locks_commit_and_held() {
    use blaze_git_fetch::{Kind as GitKind, Oid};

    let io = Io::simulated(SimWorld::new(29, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let lib = b"pub fn g() -> u32 { 42 }\n";
    let manifest = b"[package]\nname = \"g\"\nversion = \"0.1.0\"\n";
    let lib_oid = Oid::of(GitKind::Blob, lib);
    let manifest_oid = Oid::of(GitKind::Blob, manifest);
    let src_tree = git_tree_of(&mut [("lib.rs", 0o100644, lib_oid)]);
    let src_oid = Oid::of(GitKind::Tree, src_tree.as_slice());
    let root_tree = git_tree_of(&mut [("Cargo.toml", 0o100644, manifest_oid), ("src", 0o40000, src_oid)]);
    let root_oid = Oid::of(GitKind::Tree, root_tree.as_slice());
    let commit_bytes = std::format!("tree {root_oid}\nauthor a <a> 0 +0000\ncommitter a <a> 0 +0000\n\ninit\n");
    let commit = Oid::of(GitKind::Commit, commit_bytes.as_bytes());
    let lock = git_lock_text(commit);

    // Split across loose commit + 2 separate packfiles to verify whole-ODB adoption.
    let commit_hex = commit.hex();
    let loose_at = std::format!(
        "c/git/db/g-0123456789abcdef/objects/{}/{}",
        core::str::from_utf8(&commit_hex[..2]).expect("hex"),
        core::str::from_utf8(&commit_hex[2..]).expect("hex"),
    );
    let loose_bytes = git_loose_of(GitKind::Commit, commit_bytes.as_bytes());
    let (pack1, idx1) = git_pack_and_idx_of(&[(GitKind::Tree, root_tree.as_slice()), (GitKind::Blob, manifest)]);
    let (pack2, idx2) = git_pack_and_idx_of(&[(GitKind::Tree, src_tree.as_slice()), (GitKind::Blob, lib)]);
    let pack1_at = "c/git/db/g-0123456789abcdef/objects/pack/pack-0000000000000000000000000000000000000001.pack";
    let idx1_at = "c/git/db/g-0123456789abcdef/objects/pack/pack-0000000000000000000000000000000000000001.idx";
    let pack2_at = "c/git/db/g-0123456789abcdef/objects/pack/pack-0000000000000000000000000000000000000002.pack";
    let idx2_at = "c/git/db/g-0123456789abcdef/objects/pack/pack-0000000000000000000000000000000000000002.idx";
    write(&io, &cx, &loose_at, loose_bytes.as_slice());
    write(&io, &cx, pack1_at, pack1.as_slice());
    write(&io, &cx, idx1_at, idx1.as_slice());
    write(&io, &cx, pack2_at, pack2.as_slice());
    write(&io, &cx, idx2_at, idx2.as_slice());
    write(&io, &cx, "one/Cargo.lock", lock.as_str().as_bytes());

    let fetch_g = |checkout: &'static str| {
        let mut lockfile = match Lockfile::read(lock.as_str().as_bytes(), blaze_own::testing::heap(), cx.frame()) {
            Ok(lockfile) => lockfile,
            Err(words) => panic!("the lock is refused: {}", words.as_str()),
        };
        let name = fetch_task::name(unit_g(lockfile.units).expect("the lock's git unit"));
        match run(&cx, &mut lockfile, &env(&io, checkout), name, cx.frame()) {
            Ok(did) => (did, name.subject),
            Err(error) => panic!("the task stopped the command: {error:?}"),
        }
    };

    let (did, subject) = fetch_g(CHECKOUT);
    assert!(answered_held(&did, subject), "the git unit is adopted from cargo's git/db");

    let lockfile = match Lockfile::read(lock.as_str().as_bytes(), blaze_own::testing::heap(), cx.frame()) {
        Ok(lockfile) => lockfile,
        Err(words) => panic!("the lock is refused: {}", words.as_str()),
    };
    let unit = unit_g(lockfile.units).expect("the lock's git unit");
    let held = held_bytes(&cx, &store(&io, &cx), unit).expect("the adopted git record is in the sources store");
    let (full_pack, _) = git_pack_and_idx_of(&[
        (GitKind::Blob, lib),
        (GitKind::Blob, manifest),
        (GitKind::Tree, src_tree.as_slice()),
        (GitKind::Tree, root_tree.as_slice()),
        (GitKind::Commit, commit_bytes.as_bytes()),
    ]);
    let (direct, _) = blaze_fetch::unpack::git_pack_record(commit, full_pack.as_slice(), &blaze_fetch::fetch::Limits::REAL.unpack, Alloc::global())
        .expect("direct git_pack_record");
    assert_eq!(held.as_slice(), direct.as_slice(), "adopted git record equals git_pack_record byte for byte");

    // Delete loose commit and packs; second command serves from the home's sources store.
    for p in [&loose_at, pack1_at, idx1_at, pack2_at, idx2_at] {
        io.need_fs().expect("files").root().remove(&cx, &Path::new(p).expect("a path"), Durable::No).expect("deleted");
    }
    let (did, subject) = fetch_g(CHECKOUT);
    assert!(answered_held(&did, subject), "second command serves the git unit from the sources store");
}

/// Piece 14: a git db lacking the pinned commit or holding a tampered object is refused, named
/// in `Tried`, and never adopted.
///
/// Expected: `test fetch::tests::a_git_db_without_the_pinned_commit_or_tampered_is_refused_and_named ... ok`
#[test]
fn a_git_db_without_the_pinned_commit_or_tampered_is_refused_and_named() {
    use blaze_git_fetch::{Kind as GitKind, Oid};

    let io = Io::simulated(SimWorld::new(31, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let commit = Oid([0x44; 20]);
    let lock = git_lock_text(commit);

    // Folder 1: valid indexed pack of another commit (lacks `commit`).
    let other_blob = b"pub fn other() {}\n";
    let (other_pack, other_idx) = git_pack_and_idx_of(&[(GitKind::Blob, other_blob)]);
    write(&io, &cx, "c/git/db/g-0000000000000001/objects/pack/pack-0000000000000000000000000000000000000001.pack", other_pack.as_slice());
    write(&io, &cx, "c/git/db/g-0000000000000001/objects/pack/pack-0000000000000000000000000000000000000001.idx", other_idx.as_slice());

    // Folder 2: tampered loose object at `commit`'s path whose bytes do not hash to `commit`.
    let commit_hex = commit.hex();
    let tampered_at = std::format!(
        "c/git/db/g-0000000000000002/objects/{}/{}",
        core::str::from_utf8(&commit_hex[..2]).expect("hex"),
        core::str::from_utf8(&commit_hex[2..]).expect("hex"),
    );
    let tampered = git_loose_of(GitKind::Commit, b"tree 0000000000000000000000000000000000000000\n");
    write(&io, &cx, &tampered_at, tampered.as_slice());
    write(&io, &cx, "one/Cargo.lock", lock.as_str().as_bytes());

    let root = store(&io, &cx);
    let lockfile = match Lockfile::read(lock.as_str().as_bytes(), blaze_own::testing::heap(), cx.frame()) {
        Ok(lockfile) => lockfile,
        Err(words) => panic!("the lock is refused: {}", words.as_str()),
    };
    let unit = unit_g(lockfile.units).expect("the lock's git unit");
    let env = env(&io, CHECKOUT);
    let mut cargo = CargoCache::new();
    let mut tries = OwnVec::new_in(Alloc::global());
    tries.push(Try { place: STORE_PLACE, miss: Miss::Absent });
    let adopted = adopt_local(&cx, &root, &mut cargo, &env, unit, &mut tries).expect("no tool error");
    assert!(!adopted, "missing and tampered git dbs are never adopted");
    assert!(held_bytes(&cx, &root, unit).is_none(), "nothing written to sources store");
    let mut words = OwnString::new_in(Alloc::global());
    words.show(&Tried { unit, tries: tries.as_slice() });
    let said = words.as_str();
    assert!(said.contains("g-0000000000000001"), "names missing repo folder: {said}");
    assert!(said.contains("the pack holds no commit of the pinned id"), "names NoCommit: {said}");
    assert!(said.contains("g-0000000000000002"), "names tampered repo folder: {said}");
    assert!(said.contains("an object whose bytes do not hash to its id"), "names SHA-1 mismatch: {said}");
}

/// Piece 14 security: symlinks placed at a repo folder, `objects/pack`, or `.idx`/`.pack` inside
/// `<cargo home>/git/db` are refused by kernel-confined traversal and reads rather than followed
/// outside `CARGO_HOME`.
///
/// Expected: `test fetch::tests::a_symlink_inside_cargos_git_db_is_refused_by_confined_reads ... ok`
#[cfg(unix)]
#[test]
fn a_symlink_inside_cargos_git_db_is_refused_by_confined_reads() {
    use blaze_git_fetch::{Kind as GitKind, Oid};

    let base = std::env::temp_dir().join(std::format!("blaze-git-db-confined-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let home_dir = base.join("home");
    let cargo_dir = base.join("cargo");
    let outside = base.join("outside");
    let checkout = base.join("work");
    std::fs::create_dir_all(home_dir.join(".blaze/v2/sources")).expect("sources");
    std::fs::create_dir_all(cargo_dir.join("git/db")).expect("git/db");
    std::fs::create_dir_all(outside.join("objects/pack")).expect("outside");
    std::fs::create_dir_all(&checkout).expect("checkout");

    let lib = b"pub fn leaked() {}\n";
    let manifest = b"[package]\nname = \"g\"\nversion = \"0.1.0\"\n";
    let src_tree = git_tree_of(&mut [("lib.rs", 0o100644, Oid::of(GitKind::Blob, lib))]);
    let root_tree = git_tree_of(&mut [
        ("Cargo.toml", 0o100644, Oid::of(GitKind::Blob, manifest)),
        ("src", 0o40000, Oid::of(GitKind::Tree, src_tree.as_slice())),
    ]);
    let commit_bytes = std::format!(
        "tree {}\nauthor a <a> 0 +0000\ncommitter a <a> 0 +0000\n\ninit\n",
        Oid::of(GitKind::Tree, root_tree.as_slice())
    );
    let commit = Oid::of(GitKind::Commit, commit_bytes.as_bytes());
    let (pack, idx) = git_pack_and_idx_of(&[
        (GitKind::Blob, lib),
        (GitKind::Blob, manifest),
        (GitKind::Tree, src_tree.as_slice()),
        (GitKind::Tree, root_tree.as_slice()),
        (GitKind::Commit, commit_bytes.as_bytes()),
    ]);
    let pack_name = "pack-0000000000000000000000000000000000000001.pack";
    let idx_name = "pack-0000000000000000000000000000000000000001.idx";
    std::fs::write(outside.join("objects/pack").join(pack_name), pack.as_slice()).expect("write pack");
    std::fs::write(outside.join("objects/pack").join(idx_name), idx.as_slice()).expect("write idx");

    // 1. Symlinked repo folder under git/db pointing outside.
    std::os::unix::fs::symlink(&outside, cargo_dir.join("git/db/g-0000000000000001")).expect("symlink repo");
    // 2. Real repo folder with symlinked `objects/pack` pointing outside.
    std::fs::create_dir_all(cargo_dir.join("git/db/g-0000000000000002/objects")).expect("real repo");
    std::os::unix::fs::symlink(outside.join("objects/pack"), cargo_dir.join("git/db/g-0000000000000002/objects/pack")).expect("symlink pack dir");
    // 3. Real repo folder and `objects/pack`, with symlinked `.idx` and `.pack` pointing outside.
    std::fs::create_dir_all(cargo_dir.join("git/db/g-0000000000000003/objects/pack")).expect("real pack dir");
    std::os::unix::fs::symlink(outside.join("objects/pack").join(idx_name), cargo_dir.join("git/db/g-0000000000000003/objects/pack").join(idx_name)).expect("symlink idx");
    std::os::unix::fs::symlink(outside.join("objects/pack").join(pack_name), cargo_dir.join("git/db/g-0000000000000003/objects/pack").join(pack_name)).expect("symlink pack");

    let io: Io = bstd::test::native(&checkout).expect("native io");
    let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
    let home_str = home_dir.to_str().expect("utf8 home");
    let cargo_str = cargo_dir.to_str().expect("utf8 cargo");
    let native_env = FetchEnv {
        clock: io.need_clock().expect("clock"),
        checkout: "",
        served: crate::engine_script_reads::Served::Simulated,
        cert_file: None,
        cert_dir: None,
        rustflags: blaze_manifest::rustflags::RustflagsEnv::default(),
        home: Some(home_str),
        cargo_home: Some(cargo_str),
        start: Some(checkout.to_str().expect("utf8 checkout")),
        toolchain: blaze_toolchain::PINNED,
        fs: io.need_fs().expect("fs"),
        sdk: crate::sdk::NONE_IN_TESTS,
        exe_dir: Err(crate::fetch::ExeDirWhy::NotRead),
        lan_peer: crate::fetch::LanPeerConfig::DISABLED,
        heap: blaze_own::testing::heap(),
    };
    let root = sources_dir(&cx, &native_env).expect("sources dir");
    let lock = git_lock_text(commit);
    {
        let lockfile = match Lockfile::read(lock.as_str().as_bytes(), blaze_own::testing::heap(), cx.frame()) {
            Ok(lockfile) => lockfile,
            Err(words) => panic!("lock refused: {}", words.as_str()),
        };
        let unit = unit_g(lockfile.units).expect("git unit");
        let mut cargo = CargoCache::new();
        let mut tries = OwnVec::new_in(Alloc::global());
        let adopted = adopt_local(&cx, &root, &mut cargo, &native_env, unit, &mut tries).expect("no tool error");
        assert!(!adopted, "no symlinked repo, pack dir, or pack/idx file may be followed or adopted");
        assert!(held_bytes(&cx, &root, unit).is_none(), "nothing written to sources store");
        assert!(
            tries.iter().any(|t| t.place.contains("g-0000000000000003") && matches!(t.miss, Miss::Unreadable)),
            "symlinked .idx in g-0000000000000003 must fail closed as Miss::Unreadable"
        );

        // 4. Real pack-only Cargo git/db (NO loose objects) adopts cleanly over native confined
        // reads (preserving ENOENT on loose probe before searching packed objects).
        let real_pack_dir = cargo_dir.join("git/db/g-0000000000000004/objects/pack");
        std::fs::create_dir_all(&real_pack_dir).expect("real pack-only dir");
        std::fs::write(real_pack_dir.join(pack_name), pack.as_slice()).expect("write real pack");
        std::fs::write(real_pack_dir.join(idx_name), idx.as_slice()).expect("write real idx");
        let mut cargo_ok = CargoCache::new();
        let mut tries_ok = OwnVec::new_in(Alloc::global());
        let adopted_ok = adopt_local(&cx, &root, &mut cargo_ok, &native_env, unit, &mut tries_ok).expect("no tool error");
        assert!(adopted_ok, "pack-only Cargo git/db must adopt over native confined reads");
        let held = held_bytes(&cx, &root, unit).expect("record written");
        let view = blaze_fetch::PackView::open_source(&unit.source(), &held).expect("record opens");
        assert_eq!(
            view.read("src/lib.rs", blaze_own::testing::heap().alloc()).expect("read").map(|f| f.bytes.as_slice().to_vec()),
            Some(lib.to_vec())
        );

        // 5. A real .idx whose .pack is replaced by a symlink after indexing fails closed as
        // Miss::Unreadable (propagating confined pack-read errors rather than reporting PackHeader).
        let bad_pack_dir = cargo_dir.join("git/db/g-0000000000000005/objects/pack");
        std::fs::create_dir_all(&bad_pack_dir).expect("bad pack dir");
        std::fs::write(bad_pack_dir.join(idx_name), idx.as_slice()).expect("write real idx");
        std::fs::write(bad_pack_dir.join(pack_name), pack.as_slice()).expect("write real pack");
        let _ = std::fs::remove_dir_all(cargo_dir.join("git/db/g-0000000000000004"));
        let mut cargo_bad = CargoCache::new();
        // Prime `cargo_bad`'s index cache for repo `g` with an absent commit so `g-0000000000000005`
        // is indexed while `.pack` is a regular file, then swap `.pack` for a symlink.
        let other_lock = git_lock_text(Oid([0xee; 20]));
        let other_lockfile = match Lockfile::read(other_lock.as_str().as_bytes(), blaze_own::testing::heap(), cx.frame()) {
            Ok(lockfile) => lockfile,
            Err(words) => panic!("lock refused: {}", words.as_str()),
        };
        let other_unit = unit_g(other_lockfile.units).expect("other git unit");
        let mut prime_tries = OwnVec::new_in(Alloc::global());
        assert!(!adopt_local(&cx, &root, &mut cargo_bad, &native_env, other_unit, &mut prime_tries).expect("prime"));
        std::fs::remove_file(bad_pack_dir.join(pack_name)).expect("remove real pack");
        std::os::unix::fs::symlink(outside.join("objects/pack").join(pack_name), bad_pack_dir.join(pack_name)).expect("symlink pack");
        let mut tries_bad = OwnVec::new_in(Alloc::global());
        let adopted_bad = adopt_local(&cx, &root, &mut cargo_bad, &native_env, unit, &mut tries_bad).expect("no tool error");
        assert!(!adopted_bad, "symlinked .pack must be refused");
        assert!(
            tries_bad.iter().any(|t| t.place.contains("g-0000000000000005") && matches!(t.miss, Miss::Unreadable)),
            "symlinked .pack must report Miss::Unreadable"
        );
    }
    drop((root, native_env, cx, io));
    let _ = std::fs::remove_dir_all(&base);
}

/// Deterministic crash, fault-injection, and tape-driven concurrent-interleaving DST exercising
/// `Reserve` / `Dir::reserve` across simulated mid-flight crashes, sync failures, and racing
/// reservations on one key (evicting the stale pre-race value as soon as a commit completes).
///
/// Expected: `test fetch::tests::dir_reserve_crash_faults_and_interleaving_dst ... ok`
#[test]
fn dir_reserve_crash_faults_and_interleaving_dst() {
    use blaze_core::reserve::{Next, Reserve};
    use blaze_core::sim::{Faults, Sim};
    use blaze_core::{Done, Effect, FileName, Tag};
    use blaze_own::Share as _;

    let payload_of = |seq: u8, len: usize| -> std::vec::Vec<u8> {
        let fill = seq.wrapping_mul(37).wrapping_add(1);
        (0..len).map(|i| fill.wrapping_add((i % 251) as u8)).collect()
    };
    let ask_sim = |sim: &mut Sim, effect: Effect, skip_sync: bool| -> Done {
        if skip_sync && matches!(effect, Effect::Sync { .. }) {
            return Done::Synced;
        }
        let mut effects = std::vec![effect];
        let mut got = None;
        let _ = blaze_core::run_until_idle(sim, &mut effects, |c, _| got = Some(c.done));
        got.expect("sim completes one effect")
    };
    let read_sim = |sim: &mut Sim, key: &FileName| -> std::option::Option<std::vec::Vec<u8>> {
        for _ in 0..64 {
            match ask_sim(sim, Effect::ReadAll { file: key.share(), tag: Tag(0xffff_ffff) }, false) {
                Done::Read(bytes) if bytes.is_empty() => {
                    let st = ask_sim(sim, Effect::Stat { file: key.share(), tag: Tag(0xffff_fffe) }, false);
                    if matches!(st, Done::Stat(None)) {
                        return None;
                    }
                    if matches!(st, Done::Stat(Some(_))) {
                        return Some(bytes);
                    }
                }
                Done::Read(bytes) => return Some(bytes),
                _ => continue,
            }
        }
        None
    };

    struct Actor {
        reserve: Reserve,
        pending: std::option::Option<Effect>,
        payload: std::vec::Vec<u8>,
        refuse: bool,
        renamed_submitted: bool,
    }

    let run_one = |seed: u64, skip_sync: bool| -> Result<(), std::string::String> {
        let fail_one_in = if skip_sync || seed % 3 == 0 { 0 } else { 7 };
        let fail_syncs = !skip_sync && seed % 4 == 0;
        let mut sim = Sim::new(seed, Faults { fail_one_in, ..Faults::default() });
        let dir_name = FileName::new("sources").expect("dir");
        let key = FileName::new("sources/k").expect("key");
        if fail_syncs {
            sim.fail_syncs(dir_name.share());
        }
        let mut actors: [std::option::Option<Actor>; 2] = [None, None];
        let mut next_tag = 1u64;
        let mut seq = 0u8;
        let mut allowed: std::vec::Vec<std::option::Option<std::vec::Vec<u8>>> = std::vec![None];

        // Deterministic PRNG schedule over 2 interleaved actors + mid-flight crashes.
        let mut rng = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let steps = if skip_sync { 6 } else { 32 };
        for step in 0..steps {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let op = if skip_sync {
                match step {
                    0 => 6,
                    1..=4 => 0,
                    _ => 9,
                }
            } else if step + 1 == steps {
                9
            } else {
                (rng >> 33) % 10
            };
            match op {
                6 | 7 => {
                    let slot = if op == 6 { 0 } else { 1 };
                    if actors[slot].is_some() {
                        continue;
                    }
                    seq = seq.wrapping_add(1);
                    let len = if skip_sync { 8192 } else { 1 + (((rng >> 16) as usize) % 8192) };
                    let refuse = !skip_sync && ((rng >> 8) & 3) == 0;
                    let payload = payload_of(seq, len);
                    let room = u64::try_from(len + 4096).unwrap_or(u64::MAX);
                    let (reserve, first) = Reserve::new(key.share(), room, Tag(next_tag));
                    next_tag += 1;
                    actors[slot] = Some(Actor { reserve, pending: Some(first), payload, refuse, renamed_submitted: false });
                }
                0..=5 => {
                    let slot = if op <= 2 { 0 } else { 1 };
                    let Some(mut actor) = actors[slot].take() else { continue };
                    let Some(effect) = actor.pending.take() else { continue };
                    let is_rename = matches!(effect, Effect::Rename { .. });
                    let done = ask_sim(&mut sim, effect, skip_sync);
                    if is_rename && matches!(done, Done::Renamed) {
                        actor.renamed_submitted = true;
                        let cand = Some(actor.payload.clone());
                        if !allowed.contains(&cand) {
                            allowed.push(cand);
                        }
                    }
                    match actor.reserve.answer(done) {
                        Ok(Next::Write(mut mapping)) => {
                            mapping.bytes_mut()[..actor.payload.len()].copy_from_slice(&actor.payload);
                            if actor.refuse {
                                actor.pending = actor.reserve.abandon(Some(mapping));
                                if actor.pending.is_some() {
                                    actors[slot] = Some(actor);
                                }
                            } else {
                                let len = u64::try_from(actor.payload.len()).unwrap_or(0);
                                actor.pending = Some(actor.reserve.finish(mapping, len).map_err(|e| std::format!("{e}"))?);
                                actors[slot] = Some(actor);
                            }
                        }
                        Ok(Next::Ask(next_eff)) => {
                            actor.pending = Some(next_eff);
                            actors[slot] = Some(actor);
                        }
                        Ok(Next::Stored) => {
                            // Commit completed: reject the pre-race value!
                            let mut next_allowed = std::vec![Some(actor.payload.clone())];
                            if let Some(other) = actors[1 - slot].as_ref()
                                && other.renamed_submitted
                            {
                                let cand = Some(other.payload.clone());
                                if !next_allowed.contains(&cand) {
                                    next_allowed.push(cand);
                                }
                            }
                            allowed = next_allowed;
                        }
                        Ok(Next::Abandoned) => {}
                        Err(_) => {
                            if let Some(cleanup) = actor.reserve.abandon(None) {
                                actor.pending = Some(cleanup);
                                actors[slot] = Some(actor);
                            }
                        }
                    }
                }
                _ => {
                    actors = [None, None];
                    sim.crash();
                    if fail_syncs {
                        sim.fail_syncs(dir_name.share());
                    }
                    // Sweep any orphan `.part-` scratch files left by the crash and verify none remain.
                    let list_names = |sim: &mut Sim| -> std::vec::Vec<FileName> {
                        for _ in 0..64 {
                            if let Done::Listed(entries) = ask_sim(sim, Effect::List { dir: dir_name.share().into(), tag: Tag(0xffff_fffd) }, false) {
                                return entries.into_iter().map(|(n, _)| n).collect();
                            }
                        }
                        std::vec::Vec::new()
                    };
                    for entry in list_names(&mut sim) {
                        if blaze_core::reserve::part_tag(entry.as_str()).is_some() {
                            for _ in 0..64 {
                                if matches!(ask_sim(&mut sim, Effect::Remove { file: entry.share(), tag: Tag(0xffff_fffc) }, false), Done::Removed) {
                                    break;
                                }
                            }
                        }
                    }
                    if list_names(&mut sim).iter().any(|e| blaze_core::reserve::part_tag(e.as_str()).is_some()) {
                        return Err(std::format!("seed {seed} step {step}: orphan .part- remained after sweep"));
                    }
                    let got = read_sim(&mut sim, &key);
                    if !allowed.contains(&got) {
                        return Err(std::format!("seed {seed} step {step}: torn/stale record ({:?})", got.as_ref().map(|v| v.len())));
                    }
                    allowed = std::vec![got];
                }
            }
        }
        Ok(())
    };

    for seed in 1u64..=64 {
        run_one(seed, false).expect("interleaved Reserve DST holds across crashes and faults");
    }
    assert!(
        (1u64..=64).any(|seed| run_one(seed, true).is_err()),
        "planted skipped-sync bug must be caught by the crash oracle"
    );

    // Real mid-protocol crash right BEFORE `Effect::Rename`: leaves an actual `<key>.part-<tag>`
    // scratch file on disk, never publishes `<key>`, and is reclaimed cleanly by the sweep.
    {
        let mut sim = Sim::new(42, Faults::default());
        let dir_name = FileName::new("sources").expect("dir");
        let inflight_key = FileName::new("sources/inflight").expect("key");
        let (mut res, first) = Reserve::new(inflight_key.share(), 4096, Tag(777));
        let done = ask_sim(&mut sim, first, false);
        let mapping = match res.answer(done) {
            Ok(Next::Write(mut m)) => {
                m.bytes_mut()[..12].copy_from_slice(b"half-written");
                m
            }
            other => panic!("expected Write, got {other:?}"),
        };
        let trunc = res.finish(mapping, 12).expect("finish");
        let done = ask_sim(&mut sim, trunc, false);
        let sync_eff = match res.answer(done) {
            Ok(Next::Ask(eff)) => eff,
            other => panic!("expected Sync, got {other:?}"),
        };
        let done = ask_sim(&mut sim, sync_eff, false);
        assert!(matches!(res.answer(done), Ok(Next::Ask(Effect::Rename { .. }))));
        let _ = ask_sim(&mut sim, Effect::SyncDir { dir: blaze_core::DirName::Named(dir_name.share()), tag: Tag(778) }, false);
        sim.crash();
        assert_eq!(read_sim(&mut sim, &inflight_key), None, "pre-rename crash must not publish target");
        let listed = match ask_sim(&mut sim, Effect::List { dir: dir_name.share().into(), tag: Tag(779) }, false) {
            Done::Listed(entries) => entries,
            other => panic!("list failed: {other:?}"),
        };
        let orphans: std::vec::Vec<FileName> = listed
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| blaze_core::reserve::part_tag(n.as_str()).is_some())
            .collect();
        assert_eq!(orphans.len(), 1, "real in-flight .part- scratch must exist after pre-rename crash");
        assert!(matches!(ask_sim(&mut sim, Effect::Remove { file: orphans[0].share(), tag: Tag(780) }, false), Done::Removed));
        let after = match ask_sim(&mut sim, Effect::List { dir: dir_name.share().into(), tag: Tag(781) }, false) {
            Done::Listed(entries) => entries,
            other => panic!("list after sweep failed: {other:?}"),
        };
        assert!(after.iter().all(|(n, _)| blaze_core::reserve::part_tag(n.as_str()).is_none()));
    }

    // Also verify `bstd::fs::Dir::reserve` end-to-end across a simulated crash AND panic unwind
    // cleanup (leaving zero `.part-` scratch files when the write callback panics).
    for seed in 1u64..=8 {
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
        let dir = io.need_fs().expect("fs").root().subdir(&Path::new("sources").expect("dir"));
        let target = Path::new("record_key").expect("target");
        let p = payload_of(1, 8192);
        let committed: Result<(), &str> = dir
            .reserve(&cx, &target, 16_384, |buf| {
                buf[..p.len()].copy_from_slice(&p);
                Ok(u64::try_from(p.len()).unwrap_or(0))
            })
            .expect("reserve ok");
        assert_eq!(committed, Ok(()));
        // Panicking inside `Dir::reserve` callback must clean up the `.part-` scratch file via Drop guard.
        let panic_target = Path::new("panic_key").expect("panic_key");
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<Result<(), &str>, _> = dir.reserve(&cx, &panic_target, 4096, |buf| {
                buf[..4].copy_from_slice(b"boom");
                panic!("simulated callback panic");
            });
        }));
        assert!(panicked.is_err(), "write callback panic propagated");
        assert!(dir.stat(&cx, &panic_target).expect("stat panic target").is_none());
        assert_eq!(dir.sweep_parts(&cx, |_, _| true).expect("no .part- left after panic"), 0);
        assert!(io.crash(), "simulated world crashes");
        let after_cx = io.cx(BUDGET, TASK_FRAME).expect("after crash cx");
        let mut read = Bytes::new_in(after_cx.frame(), "read after crash");
        dir.read_all(&after_cx, &target, &mut read).expect("durable record survives crash");
        assert_eq!(read.as_slice(), p.as_slice());
    }

    // Delayed-completion stop check: when `poll_fuel == 0` prevents an in-flight `MapWrite` from
    // settling under latency, `Dir::reserve` returns `FsKind::Stopped` without issuing a racing
    // `Effect::Remove`.
    {
        let mut world = SimWorld::new(99, Limits { poll_fuel: 0, ..Limits::USUAL });
        world.faults = Faults { max_latency: core::time::Duration::from_millis(10), ..Faults::default() };
        let io = Io::simulated(world);
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
        let dir = io.need_fs().expect("fs").root().subdir(&Path::new("sources").expect("dir"));
        let res: Result<Result<(), &str>, _> = dir.reserve(&cx, &Path::new("stalled").expect("key"), 4096, |_| Ok(0));
        assert!(matches!(res, Err(err) if err.stopped().is_some()), "unsettled stop returns Stopped");
    }
}
