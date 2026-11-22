//! D160 on bstd's simulated world, the store on the world's lent driver as a command opens it:
//! a home holding a store of another form (the legacy format 4 layout, or a newer blaze's
//! `f6s2`) runs, makes this blaze's own folder beside it, says the other once by name and size
//! (the second command says nothing), and leaves it byte for byte; then the sweep of other
//! forms removes exactly the expired ones, checked by an oracle after every sweep, with planted
//! controls the oracle must catch.

use blaze_cas::plain::Settings;
use bstd::fs::{Content, Durable};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::{Bytes, TASK_FRAME};
use bstd::path::Path;
use bstd::test::BUDGET;

use super::super::{STORE_DIR, STORE_ROOT, after_answer};
use super::{EXPIRY_DAYS, FormKind, Note, OTHER_FORMS_LOOKED, expired, list, note_once, root};
use crate::clean::{SweepPlant, sweep_other_forms};
use crate::store::{Facts, Home};

const HOME: &str = "/h";
const DAY_NS: u64 = 86_400 * 1_000_000_000;

/// The legacy layout: a format 4 store's files directly in `v2/store`, and its cache tag.
const LEGACY: [(&str, &[u8]); 4] = [
    ("h/.blaze/v2/store/head", b"a format 4 head, 4096 bytes in life"),
    ("h/.blaze/v2/store/flip", b""),
    ("h/.blaze/v2/store/seg-0000000000000001", b"a format 4 segment"),
    ("h/.blaze/v2/store/manifests-0000000000000002", b"a format 4 manifest"),
];
const TAG: &str = "h/.blaze/v2/store/CACHEDIR.TAG";
/// A newer blaze's folder.
const NEWER: [(&str, &[u8]); 2] = [("h/.blaze/v2/store/f6s2/head", b"a format 6 head"), ("h/.blaze/v2/store/f6s2/seg-00000000000000aa", b"a format 6 segment")];
/// An older blaze's folder (a store that moved to its place at format 4, say).
const OLDER: [(&str, &[u8]); 1] = [("h/.blaze/v2/store/f4s2/head", b"a format 4 head in its place")];
/// Names under the root of no store's shape: never said, never removed.
const NOT_A_STORE: [(&str, &[u8]); 2] = [("h/.blaze/v2/store/notes", b"mine"), ("h/.blaze/v2/store/f05s2/head", b"a leading zero: no form's name")];

fn world(seed: u64, files: &[(&str, &[u8])]) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").root();
    for (path, bytes) in files {
        // Each folder made as a folder, as a real disk holds one (clean_tests.rs `world`).
        for (at, _) in path.match_indices('/') {
            let folder = Path::new(&path[..at]).expect("a path");
            if root.stat(&cx, &folder).expect("stat").is_none() {
                root.make_dir(&cx, &folder, Durable::No).expect("a folder");
            }
        }
        root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(bytes), Durable::No).expect("write");
    }
    io
}

fn stderr(io: &Io) -> std::string::String {
    let err = io.script(|host| host.err.clone()).expect("a simulated host");
    std::string::String::from_utf8(err).expect("UTF-8")
}

/// The bytes at `path`, or `None` when it is not there.
fn bytes(io: &Io, path: &str) -> Option<std::vec::Vec<u8>> {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").root();
    let path = Path::new(path).expect("a path");
    root.stat(&cx, &path).expect("a stat")?;
    let mut into = Bytes::new_in(cx.frame(), "a test's read");
    root.read_all(&cx, &path, &mut into).expect("a read");
    Some(into.as_slice().to_vec())
}

fn there(io: &Io, path: &str) -> bool {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    io.need_fs().expect("files").root().stat(&cx, &Path::new(path).expect("a path")).expect("a stat").is_some()
}

/// One command: the store opened as the command opens it, the note, the commit after the
/// answer. How many other forms it said.
fn command(io: &Io, nonce: u64) -> u32 {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let fs = io.need_fs().expect("files");
    let err = io.need_err().expect("err");
    let home = Home::of(&cx, &fs, Some(HOME)).expect("the home");
    let lent = home.open_store(&Settings::default(), Facts { boot: Some(1), nonce }, blaze_own::testing::heap().alloc()).expect("the store opens beside the other forms");
    let said = note_once(&cx, &home.dir, &lent, &err).expect("the note");
    after_answer(&cx, &lent, 0).expect("the commit after the answer");
    said
}

/// The note of a home whose other form is `files`, run twice: said once, verbatim; this
/// blaze's folder made; every one of `files` byte for byte as it was.
fn said_once_and_kept(seed: u64, files: &[(&str, &[u8])], verbatim: &str) {
    let io = world(seed, files);
    assert_eq!(command(&io, 1), 1, "the first command's look");
    let said = stderr(&io);
    println!("{said}");
    assert!(said.contains(verbatim), "the note is not the one stated:\n{said}");
    assert_eq!(said.matches("other-version store").count(), 1, "more than one note group: {said}");
    assert!(there(&io, &format!("h/.blaze/{STORE_DIR}/head")), "this blaze's folder was not made");
    assert!(there(&io, TAG), "the store root's cache tag is not there");
    assert_eq!(command(&io, 2), 0, "the second command looked again");
    assert_eq!(stderr(&io), said, "the second command said more");
    for (path, was) in files {
        assert_eq!(bytes(&io, path).as_deref(), Some(*was), "{path} was touched");
    }
}

#[test]
fn a_legacy_store_is_said_once_by_size_and_kept_byte_for_byte() {
    let mut files = LEGACY.to_vec();
    files.push((TAG, blaze_cas::plain::CACHEDIR_SIGNATURE));
    let size: u64 = LEGACY.iter().map(|(_, it)| it.len() as u64).sum();
    let human = blaze_report::ui::fmt_bytes(size);
    let verbatim = format!("~/.blaze/v2/store  format <=4 · 4 files · {human}");
    said_once_and_kept(21, &files, &verbatim);
}

#[test]
fn a_newer_blazes_folder_is_said_once_and_kept() {
    let size: u64 = NEWER.iter().map(|(_, it)| it.len() as u64).sum();
    let human = blaze_report::ui::fmt_bytes(size);
    let verbatim = format!("~/.blaze/v2/store/f6s2  other version · 2 files · {human}");
    said_once_and_kept(22, &NEWER, &verbatim);
}

/// A home of this blaze alone: nothing said, the look still recorded once.
#[test]
fn a_home_of_this_form_alone_says_nothing() {
    let io = world(23, &[("h/.profile", b"")]);
    assert_eq!(command(&io, 1), 0);
    assert_eq!(command(&io, 2), 0);
    assert_eq!(stderr(&io), "", "something was said of a home with no other form");
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let home = Home::of(&cx, &io.need_fs().expect("files"), Some(HOME)).expect("the home");
    let lent = home.open_store(&Settings::default(), Facts { boot: Some(1), nonce: 3 }, blaze_own::testing::heap().alloc()).expect("the store opens");
    assert!(lent.roots().iter().any(|(name, _)| name == OTHER_FORMS_LOOKED), "the look is not recorded");
}

/// Names of no store's shape are not other forms: never listed, so never said or removed.
#[test]
fn a_name_of_no_stores_shape_is_none_of_blazes() {
    let io = world(24, &NOT_A_STORE);
    assert_eq!(command(&io, 1), 0);
    assert_eq!(stderr(&io), "");
    for (path, was) in NOT_A_STORE {
        assert_eq!(bytes(&io, path).as_deref(), Some(was), "{path} was touched");
    }
}

/// What the sweep must leave, from the state before it and its `now_ns`.
struct Before {
    /// Every file under this blaze's folder, and the root's tag, with its bytes.
    own: std::vec::Vec<(std::string::String, std::vec::Vec<u8>)>,
    /// Each other form's files (paths under the sim's root), and whether it is expired.
    others: std::vec::Vec<(std::vec::Vec<std::string::String>, bool)>,
    /// Files of no store's shape.
    foreign: std::vec::Vec<(std::string::String, std::vec::Vec<u8>)>,
}

fn before(io: &Io, own_files: &[std::string::String], forms: &[&[(&str, &[u8])]], now_ns: u64) -> Before {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let home = Home::of(&cx, &io.need_fs().expect("files"), Some(HOME)).expect("the home");
    let listed = list(&cx, &home.dir, &root().expect("the root")).expect("the listing");
    let mut others = std::vec::Vec::new();
    for files in forms {
        let leaf = files[0].0.trim_start_matches("h/.blaze/v2/store/");
        let form = listed
            .iter()
            .find(|it| match it.kind {
                FormKind::Legacy => !leaf.contains('/'),
                FormKind::Folder(name) => leaf.starts_with(&format!("{}/", name.as_str())),
            })
            .expect("each other form listed");
        others.push((files.iter().map(|(path, _)| std::string::String::from(*path)).collect(), expired(form, now_ns)));
    }
    let mut own: std::vec::Vec<_> = own_files.iter().map(|path| (path.clone(), bytes(io, path).expect("this blaze's file"))).collect();
    own.push((std::string::String::from(TAG), bytes(io, TAG).expect("the root's tag")));
    let foreign = NOT_A_STORE.iter().map(|(path, _)| (std::string::String::from(*path), bytes(io, path).expect("a foreign file"))).collect();
    Before { own, others, foreign }
}

/// THE ORACLE: this blaze's folder and the root's tag byte for byte; a name of no store's shape
/// untouched; an other form not expired whole; an expired one wholly gone.
fn oracle(io: &Io, before: &Before) -> Result<(), std::string::String> {
    for (path, was) in before.own.iter().chain(&before.foreign) {
        if bytes(io, path).as_ref() != Some(was) {
            return Err(format!("{path} is not kept byte for byte"));
        }
    }
    for (files, expired) in &before.others {
        for path in files {
            match (*expired, there(io, path)) {
                (false, false) => return Err(format!("{path}, of a store used within {EXPIRY_DAYS} days, was removed")),
                (true, true) => return Err(format!("{path}, of a store unused for {EXPIRY_DAYS} days, was kept")),
                _ => {}
            }
        }
    }
    Ok(())
}

/// The last use of the legacy layout, as the sweep reads it.
fn legacy_used(io: &Io) -> u64 {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let home = Home::of(&cx, &io.need_fs().expect("files"), Some(HOME)).expect("the home");
    let listed = list(&cx, &home.dir, &root().expect("the root")).expect("the listing");
    listed.iter().find(|it| it.kind == FormKind::Legacy).expect("the legacy layout").used_ns
}

/// One home of every kind, a command run on it, then a sweep at `days` past the legacy
/// layout's last use under `plant`: the oracle's verdict.
fn sweep_once(seed: u64, days: u64, plant: SweepPlant) -> Result<u32, std::string::String> {
    let mut files: std::vec::Vec<(&str, &[u8])> = LEGACY.to_vec();
    files.push((TAG, blaze_cas::plain::CACHEDIR_SIGNATURE));
    files.extend(NEWER);
    files.extend(OLDER);
    files.extend(NOT_A_STORE);
    let io = world(seed, &files);
    // This blaze's folder is made by the store itself (its cache tag's durable `Write` makes
    // it, blaze-cas plain.rs "Durable names"), a folder like any other, so the planted sweep
    // that takes it reaches the oracle.
    command(&io, 1);
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let home = Home::of(&cx, &io.need_fs().expect("files"), Some(HOME)).expect("the home");
    let own_files: std::vec::Vec<std::string::String> = {
        let at = Path::new(STORE_DIR).expect("a path");
        let entries = home.dir.entries(&cx, &at).expect("a listing").expect("this blaze's folder");
        (0..entries.len()).map(|i| format!("h/.blaze/{STORE_DIR}/{}", entries.name(i))).collect()
    };
    let now_ns = legacy_used(&io) + days * DAY_NS;
    let forms: [&[(&str, &[u8])]; 3] = [&LEGACY, &NEWER, &OLDER];
    let was = before(&io, &own_files, &forms, now_ns);
    let removed = sweep_other_forms(&cx, &home.dir, now_ns, &io.need_err().expect("err"), plant).map_err(|error| format!("the sweep failed: {error}"))?;
    oracle(&io, &was)?;
    // A second sweep at the same time removes nothing more and still passes.
    let again = sweep_other_forms(&cx, &home.dir, now_ns, &io.need_err().expect("err"), SweepPlant::None).map_err(|error| format!("the second sweep failed: {error}"))?;
    if again != 0 {
        return Err(format!("a second sweep removed {again} more"));
    }
    oracle(&io, &was)?;
    Ok(removed)
}

/// The invariant: sweeping other store forms removes only expired foreign forms, leaves this blaze's folder and cache tag intact, and respects each form's last use.
#[test]
fn the_sweep_removes_exactly_the_expired_other_forms() {
    for seed in 30..36 {
        for days in [0, 1, EXPIRY_DAYS - 1, EXPIRY_DAYS, EXPIRY_DAYS + 1, 400] {
            let removed = sweep_once(seed, days, SweepPlant::None).unwrap_or_else(|why| panic!("seed {seed}, {days} days: {why}"));
            if days > EXPIRY_DAYS {
                assert_eq!(removed, 3, "seed {seed}, {days} days: every other form is past the period");
            } else if days < EXPIRY_DAYS {
                assert_eq!(removed, 0, "seed {seed}, {days} days: no other form is past the period");
            }
        }
    }
    println!("sweep: 6 seeds x 6 ages, the oracle held after every sweep");
}

/// CONTROL: a sweep that takes this blaze's own folder for another form's. The oracle names it
/// ("a folder of THIS format is never removed by this rule").
#[test]
fn the_oracle_catches_a_sweep_that_takes_its_own_folder() {
    let why = sweep_once(40, 400, SweepPlant::TakesItsOwn).expect_err("a sweep that removed this blaze's folder passed");
    println!("planted TakesItsOwn: {why}");
    assert!(why.contains(STORE_DIR) && why.contains("not kept byte for byte"), "{why}");
}

/// CONTROL: a sweep blind to the last use. The oracle names a store used within the period
/// ("a legacy store touched within the period is kept").
#[test]
fn the_oracle_catches_a_sweep_blind_to_the_last_use() {
    let why = sweep_once(41, 1, SweepPlant::IgnoresUse).expect_err("a sweep that removed a store in use passed");
    println!("planted IgnoresUse: {why}");
    assert!(why.contains(&format!("used within {EXPIRY_DAYS} days, was removed")), "{why}");
}

/// CONTROL: a legacy removal that takes the root's cache tag with the format 4 files.
#[test]
fn the_oracle_catches_a_legacy_removal_that_takes_the_tag() {
    let why = sweep_once(42, 400, SweepPlant::LegacyTakesTheTag).expect_err("a sweep that removed the tag passed");
    println!("planted LegacyTakesTheTag: {why}");
    assert!(why.contains("CACHEDIR.TAG"), "{why}");
}

/// The rule of expiry at its edges, and a clock set back.
#[test]
fn expiry_is_strictly_past_the_period() {
    let form = |used_ns| super::OtherForm { kind: FormKind::Legacy, files: 1, bytes: 1, used_ns };
    let used = 1_000 * DAY_NS;
    assert!(!expired(&form(used), used));
    assert!(!expired(&form(used), used + EXPIRY_DAYS * DAY_NS));
    assert!(expired(&form(used), used + EXPIRY_DAYS * DAY_NS + 1));
    assert!(!expired(&form(used), used - DAY_NS), "a last use after now is not past");
    assert_eq!(STORE_ROOT, "v2/store");
    assert!(STORE_DIR.starts_with("v2/store/f"), "{STORE_DIR}");
    let _ = Note(&form(used)).to_string();
}
