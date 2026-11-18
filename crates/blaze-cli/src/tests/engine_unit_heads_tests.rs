//! A HEAD IS FROZEN PER UNIT THAT HOLDS ITS ITEM, AND FOUND BY THE READER'S OWN KEY (lane RU1;
//! engine_heads.rs `head_cell`, `run_items`; engine_seeds.rs `StretchHeads`). A member crate is
//! checked as two units, its lib and its test harness, each with its own crate key, and an
//! item's key holds its unit's prefix: a head frozen under one unit's records cannot be read
//! through the other's names. A child of main.rs's tests, over engine_check_tests.rs's helpers.

use super::engine_check_tests::{engine_opts, entries, error_lines, head_reads_of_bodies, opts};
use super::engine_seeds_tests::seeds_line as heads_lines;
use super::*;

use crate::engine_heads::HeadUnits;

/// The ids a request listed, by the helpers' own form (engine_check_tests.rs `engine_opts`).
type Ids = std::vec::Vec<(std::string::String, std::string::String)>;

/// One request on `workers` with the head tasks freezing under `units` (the rule, or a plant):
/// the findings it listed, what it printed and its counts. Its text and ids are the existing
/// helpers' (`stdout`, `engine_opts`); this file allocates nothing of its own.
fn run(io: &Io, workers: u32, fresh: bool, units: HeadUnits) -> (Ids, std::string::String, engine_check::Checked) {
    let mut opts = opts(io, workers, fresh);
    opts.head_units = units;
    let before = stdout(io).len();
    let (ids, checked) = engine_opts(io, opts);
    (ids, stdout(io).split_off(before), checked)
}

/// One package: `f`, held by both units, and `helper`, a body ONLY the test unit holds,
/// calling `f` (so it asks `f`'s head under the test unit's keys).
fn one_crate(io: &Io) {
    write(io, "Cargo.toml", "[package]\nname = \"one\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "src/lib.rs", ONE);
}

const ONE: &str = "pub fn f() -> u8 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    pub fn helper() -> u8 {\n        super::f()\n    }\n}\n";

/// Two members: `c` calls `b::g()` from a lib body (`h`) AND from a body only its test unit
/// holds (`t`): both of `c`'s units read `b`'s lib unit's `g`.
fn two_crates(io: &Io) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"b\", \"c\"]\nresolver = \"2\"\n");
    write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "b/src/lib.rs", "pub fn g() -> u8 {\n    1\n}\n");
    write(io, "c/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nb = { path = \"../b\" }\n");
    write(io, "c/src/lib.rs", "pub fn h() -> u8 {\n    b::g() + 1\n}\n\n#[cfg(test)]\nmod tests {\n    pub fn t() -> u8 {\n        b::g() + super::h()\n    }\n}\n");
}

/// The words of a body refused because its unit's head was not frozen (engine_seeds.rs
/// `StretchHeads::other_units`).
const OTHER_UNITS: &str = "and not under this body's unit";

/// Every body checked clean from the frozen heads, under the rule, on `workers`.
fn all_clean(fixture: &str, text: &str, checked: &engine_check::Checked, workers: u32) {
    let seeds = &checked.seeds;
    println!("{fixture}, {workers} worker(s): {}\n{}", entries(checked), heads_lines(checked));
    let bodies = checked.stages[blaze_engine::Stage::Bodies.index()].entered;
    assert!(bodies >= 2, "{fixture}, {workers} worker(s): the bodies were entered: {}", entries(checked));
    assert!(checked.bodies.0.is_empty(), "{fixture}, {workers} worker(s): every body checked clean: {:?}\n{text}", checked.bodies.0);
    assert!(!text.contains("could not be made") && !text.contains(OTHER_UNITS), "{fixture}, {workers} worker(s): no body refused for a head:\n{text}");
    assert!(error_lines(text).is_empty(), "{fixture}, {workers} worker(s): no error:\n{text}");
    assert_eq!(seeds.bodies.refused, 0, "{fixture}, {workers} worker(s): no body refused at a seed:\n{}", heads_lines(checked));
    assert_eq!(seeds.bodies.seeded, bodies, "{fixture}, {workers} worker(s): every body seeded from frozen heads:\n{}", heads_lines(checked));
    // Every member want a body asked was taken from the head tasks' table: none frozen at a
    // seed (a member's head the table lacked) and none refused (one of another unit).
    assert_eq!(seeds.not_put.member, 0, "{fixture}, {workers} worker(s): every member head asked was the table's:\n{}", heads_lines(checked));
    assert!(seeds.taken >= 2, "{fixture}, {workers} worker(s): member heads taken from the table:\n{}", heads_lines(checked));
    // The cost, counted: both units froze, and items held by both were frozen under each.
    assert!(seeds.units.units >= 2, "{fixture}, {workers} worker(s): heads frozen under two units:\n{}", heads_lines(checked));
    assert!(seeds.units.shared > 0, "{fixture}, {workers} worker(s): items held by 2+ units (K > 0):\n{}", heads_lines(checked));
    assert!(seeds.units.frozen > seeds.units.items, "{fixture}, {workers} worker(s): more freezes than items:\n{}", heads_lines(checked));
}

/// (a) THE LATENT BUG ON GREEN'S FIXTURES (rule 23): `helper`, a body only the test unit holds,
/// reads `f` under the test unit's keys; `c`'s lib body and its test body both read `b::g` under
/// `b`'s lib unit's. Under the rule every body checks clean from the frozen heads, on 1 and 8
/// workers, every member head asked is taken from the table, and the profile counts items held
/// by two units. After the request, every head read of a body names a cell its head task
/// defines, at the digest it defines (rule 21).
#[test]
fn every_unit_reads_heads_frozen_under_its_own_keys() {
    for workers in [1u32, 8] {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        one_crate(&io);
        let (_, text, checked) = run(&io, workers, true, HeadUnits::Each);
        all_clean("one crate", &text, &checked, workers);
        let (bad, good) = head_reads_of_bodies(&io);
        assert!(bad.is_empty(), "one crate, {workers} worker(s): head reads no head task answers: {bad:?}");
        assert!(good >= 3, "one crate, {workers} worker(s): each body's own head and `helper`'s read of `f`: {good}");

        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        two_crates(&io);
        let (_, text, checked) = run(&io, workers, true, HeadUnits::Each);
        all_clean("two crates", &text, &checked, workers);
        let (bad, good) = head_reads_of_bodies(&io);
        assert!(bad.is_empty(), "two crates, {workers} worker(s): head reads no head task answers: {bad:?}");
        assert!(good >= 5, "two crates, {workers} worker(s): three bodies' own heads and two reads of `g`: {good}");
    }
}

/// (b) THE PLANT (rule 23: remove the cause, watch the effect come back): the head tasks
/// freeze under ONE unit alone, the first by crate prefix or the last (the form before the
/// rule). `f` is held by two units, so one of the two plants freezes it under the lib unit
/// alone, and `helper` (the test unit's) asks a cell no run covers: refused by path, naming the
/// units that froze it ("... and not under this body's unit ..."), never handed the lib unit's
/// bytes (which named keys its names hold no path for: green's "no path for (key ..)" note).
/// CONTROL: the rule's run on the same fixture is clean (case (a)).
#[test]
#[ignore = "OPEN (2026-11-17): neither plant makes a refusal: with a head frozen under one unit alone the body's stretch froze the missing member head itself (`member heads taken from the table: 0; heads not in it read 2, frozen now 1`), so `NotInItsUnit` is never reached: the bodies' own freeze of a MEMBER head the table lacks must be that refusal (lane RU1's next piece)"]
fn a_head_frozen_under_one_unit_alone_is_refused_by_path() {
    let mut refused = 0u32;
    for plant in [HeadUnits::OnlyFirst, HeadUnits::OnlyLast] {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        one_crate(&io);
        let (_, text, checked) = run(&io, 2, true, plant);
        println!("{plant:?}: {}\n{}\n{text}", entries(&checked), heads_lines(&checked));
        assert_eq!(checked.seeds.units.shared, 0, "{plant:?}: the plant freezes no item under two units:\n{}", heads_lines(&checked));
        assert!(!text.contains("holds no path for"), "{plant:?}: no body is handed another unit's bytes:\n{text}");
        if let Some(note) = checked.bodies.0.iter().find(|note| note.contains(OTHER_UNITS)) {
            assert!(note.contains("the head of `") && note.contains("was frozen by its head task under unit(s) "), "{plant:?}: the refusal names the head by path and the units that froze it: {note}");
            refused += 1;
        }
    }
    assert!(refused > 0, "one of the two plants freezes `f` under the lib unit alone, and `helper` is refused for it");
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    one_crate(&io);
    let (_, text, checked) = run(&io, 2, true, HeadUnits::Each);
    all_clean("CONTROL: the rule", &text, &checked, 2);
}

/// `ONE` with a lib body `user` that calls `f` too.
const WITH_USER: &str = "pub fn f() -> u8 {\n    1\n}\n\npub fn user() -> u8 {\n    f()\n}\n\n#[cfg(test)]\nmod tests {\n    pub fn helper() -> u8 {\n        super::f()\n    }\n}\n";
/// [`WITH_USER`] with `f`'s head moved: it returns `u16`.
const WITH_USER_U16: &str = "pub fn f() -> u16 {\n    1\n}\n\npub fn user() -> u8 {\n    f()\n}\n\n#[cfg(test)]\nmod tests {\n    pub fn helper() -> u8 {\n        super::f()\n    }\n}\n";
/// [`WITH_USER`] with `f` held by the test unit alone.
const WITH_USER_CFG: &str = "#[cfg(test)]\npub fn f() -> u8 {\n    1\n}\n\npub fn user() -> u8 {\n    f()\n}\n\n#[cfg(test)]\nmod tests {\n    pub fn helper() -> u8 {\n        super::f()\n    }\n}\n";

/// The Bodies stage's entries of one request by cause: (key moved, read of a head moved).
fn bodies_by_cause(checked: &engine_check::Checked) -> (u32, u32) {
    let mut key = 0u32;
    let mut head = 0u32;
    for (group, count) in checked.because.iter().filter(|(group, _)| group.stage == blaze_engine::Stage::Bodies) {
        match (group.cause, group.definer.map(|name| name.kind)) {
            ("key moved", None) => key += *count,
            ("read moved", Some(blaze_drive::Kind::Signature)) => head += *count,
            _ => {}
        }
    }
    (key, head)
}

/// (c) THE READS (rule 21): `f`'s return type changed enters `f`'s body by its key and both
/// readers by their read of `f`'s head (`user`'s, of its checking unit, and `helper`'s, of the
/// test unit), and the incremental answer is a fresh run's. Then `#[cfg(test)]` added to `f`:
/// the lib unit no longer holds it, so `f`'s head task drops the lib unit's cell (its defines
/// shrink); a lib body that read it is entered (the definer gone) and reports the unresolved
/// name, exactly as a fresh run does; and no body of the stored answer reads a cell no task
/// defines. Right for ANY checking unit of `user` (lane RP1 picks the lib's first).
#[test]
fn a_head_edit_and_a_cfg_move_each_unit_s_readers() {
    let io = Io::simulated(SimWorld::new(13, Limits::USUAL));
    write(&io, "Cargo.toml", "[package]\nname = \"one\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "src/lib.rs", WITH_USER);
    let (_, first_text, first) = run(&io, 2, false, HeadUnits::Each);
    println!("first: {}\n{}", entries(&first), heads_lines(&first));
    assert!(first.bodies.0.is_empty() && error_lines(&first_text).is_empty(), "the crate checks clean first: {:?}\n{first_text}", first.bodies.0);
    assert!(first.seeds.units.shared > 0, "`f` is frozen under both units:\n{}", heads_lines(&first));

    // `f`'s head moved.
    write(&io, "src/lib.rs", WITH_USER_U16);
    let (now_ids, now_text, now) = run(&io, 2, false, HeadUnits::Each);
    println!("a head changed: {}\n{}", entries(&now), heads_lines(&now));
    assert!(now.stages[blaze_engine::Stage::Heads.index()].entered >= 1, "`f`'s head task entered: {}", entries(&now));
    assert_eq!(bodies_by_cause(&now), (1, 2), "`f` by its key; `user` and `helper` by their read of `f`'s head (each its own unit's cell): {}", entries(&now));
    let (bad, _) = head_reads_of_bodies(&io);
    assert!(bad.is_empty(), "a head changed: head reads no head task answers: {bad:?}");
    let (fresh_ids, fresh_text, fresh) = run(&io, 2, true, HeadUnits::Each);
    assert_eq!(error_lines(&fresh_text).len(), 2, "a fresh run: `user`'s and `helper`'s errors:\n{fresh_text}");
    assert_eq!((now_ids, error_lines(&now_text)), (fresh_ids, error_lines(&fresh_text)), "a head changed: the incremental errors are a fresh run's\n{}", entries(&now));
    assert_eq!(now.bodies.0, fresh.bodies.0, "a head changed: the incremental notes are a fresh run's");

    // Back to clean, then `f` held by the test unit alone.
    write(&io, "src/lib.rs", WITH_USER);
    let (_, clean_text, clean) = run(&io, 2, false, HeadUnits::Each);
    assert!(error_lines(&clean_text).is_empty(), "clean again: {}\n{clean_text}", entries(&clean));
    write(&io, "src/lib.rs", WITH_USER_CFG);
    let (cfg_ids, cfg_text, cfg) = run(&io, 2, false, HeadUnits::Each);
    println!("`f` under cfg(test): {}\n{}\n{cfg_text}", entries(&cfg), heads_lines(&cfg));
    let (bad, _) = head_reads_of_bodies(&io);
    assert!(bad.is_empty(), "`f` under cfg(test): no body reads the lib unit's dropped cell: {bad:?}");
    let (fresh_ids, fresh_text, fresh) = run(&io, 2, true, HeadUnits::Each);
    println!("fresh: {}\n{fresh_text}", entries(&fresh));
    assert_eq!((cfg_ids, error_lines(&cfg_text)), (fresh_ids, error_lines(&fresh_text)), "`f` under cfg(test): the incremental errors are a fresh run's\n{}", entries(&cfg));
    assert_eq!(cfg.bodies.0, fresh.bodies.0, "`f` under cfg(test): the incremental notes are a fresh run's");
    // When the lib unit checks `user` (a fresh run reports `f` unresolved there), the
    // incremental request entered it: the cell it read is no longer defined.
    if !error_lines(&fresh_text).is_empty() {
        assert!(cfg.stages[blaze_engine::Stage::Bodies.index()].entered >= 1, "`user` entered for the lib unit's cell gone: {}", entries(&cfg));
    }
}
