//! A BODY'S RUN FACTS ARE A RECORD (run.md, "Run and test read the engine's answer", piece E1;
//! engine_body_facts.rs). A child of main.rs's tests, over engine_check_tests.rs's helpers.
//!
//! The record's bytes: what is written reads back, and every bad byte string is refused by a
//! typed error naming it. The class rule: one function over the rules of a task's findings.
//! On the engine: after a fresh check of one crate with a clean fn and a fn with a type error,
//! each `BodyCheck` name of the stored answer has a record whose facts read `Clean` and
//! `Errors { errors: 1, .. }` and cover one item each; a second command with nothing changed
//! reads the same bytes back and puts none; a one-body edit puts exactly one, and the facts of
//! the edited tree equal a fresh command's (rule 26).
//!
//! PLANTED CONTROL (rule 23): a `class_of` that answers `Clean` for everything fails the
//! assertion "the fn with a type error is Errors { errors: 1 }" of
//! `a_body_s_run_facts_are_put_with_its_row_and_stand_with_it`, and the `class_of` test's
//! `Errors`, `Untyped` and `Typed` rows.

use super::engine_answer_rows_tests::with_store;
use super::engine_check_tests::engine_from;
use super::*;

use blaze_drive::{Kind, Name};
use blaze_engine::Stage;

use crate::engine_body_facts::{self as facts, BodyFacts, BodyFactsError, Class};
use crate::store::Store as _;

/// Two covered keys, sorted.
const KEYS: [[u8; 16]; 2] = [[1; 16], [2; 16]];

/// The record's bytes for (`class`, `covers`) with no typed record.
fn made(class: Class, covers: &[[u8; 16]]) -> std::vec::Vec<u8> {
    made_typed(class, covers, None)
}

/// The record's bytes for (`class`, `covers`, `typed`).
fn made_typed(class: Class, covers: &[[u8; 16]], typed: Option<blaze_drive::Key>) -> std::vec::Vec<u8> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut bytes: blaze_own::Vec<u8, _> = blaze_own::Vec::new_in(alloc);
    facts::write(class, covers, typed, &mut bytes).expect("sorted keys are written");
    bytes.to_vec()
}

/// A typed record's store key.
const TYPED: blaze_drive::Key = blaze_drive::Key(blaze_ids::ContentId([7; 32]));

/// Expected: every class with 0, 1 and 2 keys reads back as written, with equal bytes written
/// twice; each bad byte string is its own typed refusal.
#[test]
fn a_body_s_facts_read_back_as_written_and_bad_bytes_are_refused() {
    let classes = [Class::Clean, Class::Errors { errors: core::num::NonZeroU32::new(3).expect("3 is not zero"), first: crate::body_types::TYPES }, Class::Untyped { rule: crate::work::NOT_RUN }, Class::Typed { rule: crate::body_types::BORROWS_NOT_CHECKED }];
    for class in classes {
        for covers in [&[][..], &KEYS[..1], &KEYS[..]] {
            let bytes = made(class, covers);
            assert_eq!(facts::read(&bytes), Ok(BodyFacts { class, covers, typed: None }), "{class:?} over {} keys reads back", covers.len());
            assert_eq!(made(class, covers), bytes, "{class:?}: two writes of one value are equal bytes (rule 25)");
            let typed = made_typed(class, covers, Some(TYPED));
            assert_eq!(facts::read(&typed), Ok(BodyFacts { class, covers, typed: Some(TYPED) }), "{class:?} over {} keys with a typed record reads back", covers.len());
            assert_ne!(typed, bytes, "{class:?}: the typed record's key is in the bytes, so the facts record's key moves with it (rule 21)");
            let keys: std::vec::Vec<u128> = facts::read(&bytes).expect("reads").covered().map(|key| key.0).collect();
            assert_eq!(keys, covers.iter().map(|key| u128::from_be_bytes(*key)).collect::<std::vec::Vec<_>>(), "the keys as the old payload's checker keys");
        }
    }

    let good = made(Class::Clean, &KEYS);
    assert_eq!(facts::read(&good[..19]), Err(BodyFactsError::Short { len: 19 }), "shorter than its head");
    assert_eq!(facts::read(&[]), Err(BodyFactsError::Short { len: 0 }), "empty");
    let edited = |at: usize, byte: u8| {
        let mut bytes = good.clone();
        bytes[at] = byte;
        bytes
    };
    assert_eq!(facts::read(&edited(0, b'X')), Err(BodyFactsError::Magic { found: [b'X', b'R', b'F', b'2'] }), "another form's magic");
    assert_eq!(facts::read(&edited(4, 1)), Err(BodyFactsError::Form { found: 1 }), "another form number");
    assert_eq!(facts::read(&edited(6, 9)), Err(BodyFactsError::Tag { tag: 9 }), "a tag of no class");
    assert_eq!(facts::read(&edited(8, 1)), Err(BodyFactsError::Fields { tag: 0 }), "a clean body with an error count");
    assert_eq!(facts::read(&edited(7, 1)), Err(BodyFactsError::Reserved { at: 7 }), "a reserved byte set");
    assert_eq!(facts::read(&edited(6, 1)), Err(BodyFactsError::NoErrors), "an error class with no error");
    assert_eq!(facts::read(&good[..good.len() - 1]), Err(BodyFactsError::Size { count: 2, len: good.len() - 1 }), "a key cut short");
    let mut longer = good.clone();
    longer.push(0);
    assert_eq!(facts::read(&longer), Err(BodyFactsError::Size { count: 2, len: good.len() + 1 }), "a byte past the keys");
    // The two keys swapped: not sorted. The two equal: not one each.
    let mut swapped = good.clone();
    let (first, second) = (swapped[20..36].to_vec(), swapped[36..52].to_vec());
    swapped[20..36].copy_from_slice(&second);
    swapped[36..52].copy_from_slice(&first);
    assert_eq!(facts::read(&swapped), Err(BodyFactsError::Unsorted { at: 1 }), "unsorted keys");
    let mut twice = good.clone();
    twice[36..52].copy_from_slice(&first);
    assert_eq!(facts::read(&twice), Err(BodyFactsError::Unsorted { at: 1 }), "a key twice");
    // The typed mark: another value, and a typed key cut short or followed by a byte.
    assert_eq!(facts::read(&edited(good.len() - 1, 2)), Err(BodyFactsError::TypedMark { mark: 2 }), "a typed mark of no meaning");
    let with_typed = made_typed(Class::Clean, &KEYS, Some(TYPED));
    assert_eq!(facts::read(&with_typed[..with_typed.len() - 1]), Err(BodyFactsError::Size { count: 2, len: with_typed.len() - 1 }), "a typed key cut short");
    let mut past = with_typed.clone();
    past.push(0);
    assert_eq!(facts::read(&past), Err(BodyFactsError::Size { count: 2, len: with_typed.len() + 1 }), "a byte past the typed key");

    // The writer refuses what the reader would, and writes nothing.
    let alloc = blaze_own::testing::heap().alloc();
    let mut out: blaze_own::Vec<u8, _> = blaze_own::Vec::new_in(alloc);
    assert_eq!(facts::write(Class::Clean, &[KEYS[1], KEYS[0]], Some(TYPED), &mut out), Err(BodyFactsError::Unsorted { at: 1 }), "unsorted keys are not written");
    // An error class with no error cannot be written: its count is a `NonZeroU32` (finding 13).
    assert_eq!(out.len(), 0, "a refusal writes nothing");
}

/// Expected: no finding is `Clean`; an error rule is counted and the first kept; a rule of the
/// not-finished kind wins over errors (the first such); an advice-only rule is no error.
#[test]
fn a_class_is_one_rule_over_a_task_s_findings_rules() {
    use crate::body_types::{BORROWS_ARE_ERRORS, BORROWS_NOT_CHECKED, TYPES, TYPES_NOT_CHECKED};
    assert_eq!(facts::class_of([]), Class::Clean, "no finding");
    assert_eq!(facts::class_of([TYPES]), Class::Errors { errors: core::num::NonZeroU32::MIN, first: TYPES }, "one error");
    assert_eq!(facts::class_of([TYPES, TYPES]), Class::Errors { errors: core::num::NonZeroU32::new(2).expect("2 is not zero"), first: TYPES }, "two errors");
    assert_eq!(facts::class_of([TYPES, TYPES_NOT_CHECKED]), Class::Untyped { rule: TYPES_NOT_CHECKED }, "types not checked wins over an error");
    assert_eq!(facts::class_of([crate::work::NOT_RUN]), Class::Untyped { rule: crate::work::NOT_RUN }, "not run");
    // A typed-whole body whose borrow check was refused: Typed, whether or not borrows are errors
    // (the reader decides; the record says what was checked).
    assert_eq!(facts::class_of([BORROWS_NOT_CHECKED]), Class::Typed { rule: BORROWS_NOT_CHECKED }, "types whole, borrows not checked");
    assert_eq!(facts::class_of([BORROWS_NOT_CHECKED, TYPES]), Class::Errors { errors: core::num::NonZeroU32::MIN, first: TYPES }, "an error beats the borrow note");
    assert_eq!(facts::class_of([BORROWS_NOT_CHECKED, TYPES_NOT_CHECKED]), Class::Untyped { rule: TYPES_NOT_CHECKED }, "types not whole beats the borrow note");
    let _ = BORROWS_ARE_ERRORS;
}

/// One crate: `good` clean, `bad` with a type error.
fn one_crate(io: &Io, good: &str) {
    write(io, "Cargo.toml", MANIFEST);
    write(io, "src/lib.rs", &format!("pub fn good() -> u8 {{\n    {good}\n}}\n\npub fn bad() -> u8 {{\n    true\n}}\n"));
}

/// The facts record's payload of every `BodyCheck` name of the stored answer, by name: found
/// through the answer's record map and the store, as a later command finds it.
fn stored_facts(io: &Io) -> std::vec::Vec<(Name, std::vec::Vec<u8>)> {
    with_store(io, |cx, store, opts, _| {
        let frame = cx.frame();
        let name = crate::engine_answer::root_name(opts.place.checkout, frame);
        let pointer = crate::engine_answer::load_pointer(cx, store, &name).expect("the store reads").expect("a stored answer").expect("its pointer reads");
        let (kept, _) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("the store reads").expect("its pages read");
        let mut found = std::vec::Vec::new();
        for (name, key) in kept.records.iter().filter(|(name, _)| name.kind == Kind::BodyCheck) {
            let held = store.get(cx, *key).expect("the store reads").unwrap_or_else(|| panic!("{name:?}: the answer names a facts record the store lacks"));
            let payload = match blaze_drive::record::open(held.bytes) {
                Ok(blaze_drive::record::Stored::Whole(view)) => view.payload.to_vec(),
                _ => panic!("{name:?}: its facts record is no whole drive record"),
            };
            found.push((*name, payload));
        }
        found
    })
}

/// The Bodies stage's entered tasks and puts of a request.
fn bodies_counts(checked: &engine_check::Checked) -> (usize, usize) {
    let stage = Stage::Bodies.index();
    (checked.stages[stage].entered as usize, checked.records.lines[stage].puts as usize)
}

/// Expected: (b) after a fresh check, a record a body, `Clean` for `good` and `Errors { errors:
/// 1 }` for `bad`, each covering one item, all keys distinct; one put a body entered. (c) A
/// second command with nothing changed enters nothing, puts no facts record, and its records'
/// bytes equal the first's. (d) A one-body edit enters one body and puts one facts record; the
/// stored facts equal those of a fresh command on the edited tree (rule 26).
#[test]
fn a_body_s_run_facts_are_put_with_its_row_and_stand_with_it() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    one_crate(&io, "1");
    let (_, fresh) = engine_from(&io, 2, true);
    let (entered, puts) = bodies_counts(&fresh);
    let first = stored_facts(&io);
    println!("fresh: {entered} bodies entered, {puts} facts records put, {} stored", first.len());
    assert!(entered >= 2, "the fixture's two bodies were entered: {entered}");
    assert_eq!((puts, first.len()), (entered, entered), "one facts record put a body entered, each body's name has one in the answer");
    let read: std::vec::Vec<BodyFacts<'_>> = first.iter().map(|(name, bytes)| facts::read(bytes).unwrap_or_else(|error| panic!("{name:?}: {error}"))).collect();
    let errors: std::vec::Vec<Class> = read.iter().map(|one| one.class).filter(|class| matches!(class, Class::Errors { .. })).collect();
    // D191: every unit that holds a body checks it, a row each, and each row puts its facts: the
    // library's and its test build's row of the fn with a type error are both Errors.
    assert_eq!(errors.len(), 2, "exactly the fn with a type error is Errors, in its 2 units' rows (D191): {read:?}");
    assert!(matches!(errors[0], Class::Errors { errors, .. } if errors.get() == 1), "the fn with a type error is Errors {{ errors: 1 }}: {errors:?}");
    assert!(read.iter().any(|one| one.class == Class::Clean), "the clean fn is Clean: {read:?}");
    assert!(read.iter().all(|one| matches!(one.class, Class::Clean | Class::Errors { .. })), "both bodies were checked whole: {read:?}");
    assert!(read.iter().all(|one| one.covers.len() == 1), "each fn covers its own item: {read:?}");
    let mut keys: std::vec::Vec<[u8; 16]> = read.iter().map(|one| one.covers[0]).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), read.len(), "the covered keys are distinct: {read:?}");

    // (c) Nothing changed: the facts stand as they were, none put.
    let (_, again) = engine_from(&io, 2, false);
    let (entered_again, puts_again) = bodies_counts(&again);
    assert_eq!((entered_again, puts_again), (0, 0), "nothing changed: no body entered, no facts record put");
    assert_eq!(stored_facts(&io), first, "the facts read back after the command equal the bytes made");

    // (d) One body edited: one entered, one put, the same facts as a fresh command's.
    one_crate(&io, "2");
    let (_, edited) = engine_from(&io, 2, false);
    let (entered_edit, puts_edit) = bodies_counts(&edited);
    println!("one-body edit: {entered_edit} bodies entered, {puts_edit} facts records put");
    assert_eq!((entered_edit, puts_edit), (2, 2), "one body edited: its 2 units' rows entered, a facts record put each (D191)");
    let after = stored_facts(&io);
    let clean = Io::simulated(SimWorld::new(11, Limits::USUAL));
    one_crate(&clean, "2");
    let _ = engine_from(&clean, 2, true);
    assert_eq!(after, stored_facts(&clean), "a restore equals a fresh build in bytes (rule 26)");
}
