//! The tests record ([`crate::tests_record`]) on a two-file crate solved twice, as the unit
//! graph makes a library and its test unit: once with no `test` in its cfg, once with it.
//!
//! Expected line: `test tests::tests_record::the_tests_record_lists_a_test_units_tests ... ok`.

use std::vec::Vec;

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::Alloc;

use super::entry::{Texts, input, resolved};
use crate::resolve::{CrateInput, CrateNames, aligned};
use crate::tests_record::{ShouldPanic, TestRow, TestsRecord, TestsRecordError, encode};

/// Three `#[test]` functions (one at the root, two in the test module), a `#[cfg(test)] mod`
/// in its own file, and plain functions beside them.
const LIB: &str = "#![no_core]
pub fn plain() {}
#[test]
fn top() {}
#[cfg(test)]
mod tests;
";

const TESTS: &str = "fn helper() {}
#[test]
#[ignore = \"slow\"]
fn skipped() {}
#[test]
#[should_panic(expected = \"out of \\\"bounds\\\"\")]
fn panics() {}
";

static TEXTS: Texts = Texts(&[&[("/t/lib.rs", LIB), ("/t/tests.rs", TESTS)], &[("/t/lib.rs", LIB), ("/t/tests.rs", TESTS)]]);

fn key_named(names: &CrateNames<'_>, name: &str) -> ItemKey {
    let mut found: Vec<ItemKey> = names.tables.texts.iter().filter(|(_, (it, _))| it.name == Some(name)).map(|(k, _)| *k).collect();
    assert_eq!(found.len(), 1, "one item named `{name}`");
    found.remove(0)
}

fn has_module(names: &CrateNames<'_>, name: &str) -> bool {
    names.tree.modules.iter().any(|m| m.name == Some(name))
}

/// The Reference, "Testing attributes": "These functions are only compiled when in test mode",
/// `ignore` "may optionally be written with the MetaNameValueStr syntax to specify a reason",
/// and `should_panic`'s "input string that must appear within the panic message"; "Conditional
/// compilation": a `#[cfg(test)]` module is no part of a unit whose cfg does not hold `test`.
/// So: the plain unit holds no `tests` module and has no tests record; the test unit's record
/// lists exactly the three tests, in path order, each with its key and flags, and neither
/// `plain` nor `helper`.
#[test]
fn the_tests_record_lists_a_test_units_tests() {
    let g = Alloc::global();
    let plain = input("t", "/t/lib.rs", &[]);
    let tested = CrateInput {
        key: CrateItemKey(ContentId::of("blaze-scope test crate", b"t with test")),
        cfg: &[("test", None)],
        ..input("t", "/t/lib.rs", &[])
    };
    let names = resolved(&[plain, tested], &TEXTS);
    let (plain, tested) = (names.krate(0), names.krate(1));

    assert!(!has_module(plain, "tests"), "the plain unit's names hold no `tests` module");
    assert!(has_module(tested, "tests"), "the test unit's names hold the `tests` module");
    assert_eq!(encode(plain, g).map(|b| b.len()), Err(TestsRecordError::NotATestUnit { krate: plain.key }));

    let bytes = encode(tested, g).unwrap_or_else(|e| panic!("encode: {e}"));
    let record = TestsRecord::open(aligned(&bytes, g)).unwrap_or_else(|e| panic!("open: {e}"));
    assert_eq!(record.key(), tested.key);
    let rows: Vec<TestRow<'_>> = (0..record.len()).map(|i| record.row(i).unwrap_or_else(|e| panic!("row {i}: {e}")).expect("a row below len")).collect();
    assert_eq!(
        rows,
        [
            TestRow {
                key: key_named(tested, "panics"),
                path: "tests::panics",
                ignore: None,
                should_panic: ShouldPanic::WithMessage("out of \"bounds\""),
                bench: false,
            },
            TestRow { key: key_named(tested, "skipped"), path: "tests::skipped", ignore: Some("slow"), should_panic: ShouldPanic::No, bench: false },
            TestRow { key: key_named(tested, "top"), path: "top", ignore: None, should_panic: ShouldPanic::No, bench: false },
        ]
    );
    assert_eq!(record.row(3), Ok(None));
    assert_eq!(record.find("tests::skipped").map(|r| r.map(|r| r.key)), Ok(Some(key_named(tested, "skipped"))));
    assert_eq!(record.find("plain"), Ok(None), "a plain function is no test");
    assert_eq!(record.find("tests::helper"), Ok(None), "a function without `#[test]` is no test");
    // Two builders of one key write equal bytes.
    assert_eq!(encode(tested, g).unwrap_or_else(|e| panic!("encode again: {e}")), bytes);
}
