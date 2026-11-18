//! What a test unit's tests record says about single functions of the project's own code
//! ([`blaze_scope::tests_record::TestsSection`]), as findings of the crate's names record at
//! each function, placed item-relative (rule 71) like an unresolved path ([`super::UNRESOLVED`]'s
//! message form: file, item chain, edition, then the words):
//!
//! - [`TEST_REFUSED`], an error: a function the record leaves out (`#[test]` and `#[bench]` on
//!   it, `#[test]` on a method, an `ignore` or `should_panic` in a form not read, ..). Before
//!   it, one such function emptied the unit's whole tests section and failed its crate (tempo
//!   2026-11-17: crates `inline` and `deferred`, "marked `test` or `bench` more than once").
//! - [`TEST_DUPLICATED`], a warning: rustc's `duplicate_macro_attributes` ("duplicated
//!   attribute"), a bare `#[test]` after another test marker on one function. It is one test.
//!
//! A function written by a macro (`#[tokio::test]`'s output) is placed at the call that wrote
//! it, at the function's name when the call's item holds it. A dependency's functions are not
//! the project's to fix and are not listed (as with unresolved paths).

use std::alloc::Global;

use blaze_scope::resolve::{At, CrateNames};
use blaze_scope::tests_record::{TestsRecordError, TestsSection};

use super::{Unresolved, find_word};

/// The rule of a function the tests record refuses as a test. An error: blaze-report's words,
/// number 26, "test-refused".
pub(crate) const TEST_REFUSED: u16 = 26;

/// The rule of a test marked twice where rustc warns (`duplicate_macro_attributes`). A
/// warning: blaze-report's words, number 27, "duplicated-test-attribute".
pub(crate) const TEST_DUPLICATED: u16 = 27;

/// The findings of `section` in crate `c`'s own files, each with its rule, sorted by (rule,
/// message, range). A function whose place is not found is placed at the crate (an empty file
/// piece), never dropped.
pub(super) fn findings(c: &CrateNames<'_>, section: &TestsSection<'_>) -> blaze_own::Vec<(u16, Unresolved), Global> {
    let mut own = blaze_own::Vec::new_in(Global);
    if !c.files.iter().any(|file| file.path.starts_with("/0/")) {
        return own;
    }
    for refused in section.refused.iter() {
        let Some(item) = refused.item() else { continue };
        let why = match refused {
            TestsRecordError::TestAndBench { .. } => std::string::String::from("it is marked both `#[test]` and `#[bench]`, and a function is one or the other"),
            TestsRecordError::NotAFreeFn { .. } => std::string::String::from("`#[test]` marks a free function, and this one is a member of an impl or a trait"),
            TestsRecordError::Form { attr, .. } => std::format!("its `#[{attr}]` is in a form not read"),
            TestsRecordError::CfgUnread { .. } => std::string::String::from("a `cfg_attr` predicate on it did not read"),
            other => std::format!("{other}"),
        };
        own.push((TEST_REFUSED, finding(c, item, Some(&why))));
    }
    for item in section.duplicated.iter() {
        own.push((TEST_DUPLICATED, finding(c, *item, None)));
    }
    own.sort_unstable_by(|a, b| (a.0, &a.1.message[..], a.1.at).cmp(&(b.0, &b.1.message[..], b.1.at)));
    own
}

/// One finding at function `key`: the place pieces, then `the function `name``, then `why`
/// when the rule's words take it.
fn finding(c: &CrateNames<'_>, key: blaze_id::ItemKey, why: Option<&str>) -> Unresolved {
    let mut message = blaze_own::Vec::new_in(Global);
    let mut at = (0u32, 0u32);
    let text = c.tables.texts.get(&key);
    let name = text.and_then(|(it, _)| it.name).unwrap_or("");
    // The file, the item holding the function (itself, or the call that wrote it) and the
    // function name's range inside that item.
    let place = text.and_then(|(it, written)| match written {
        At::File(file) => {
            let file = *c.files.get(*file as usize)?;
            let item = file.outline.items().get(it.item.0 as usize)?;
            let range = item.name.map_or((0, 0), |range| (range.start, range.end));
            Some((file, item, range))
        }
        At::Expansion(call) => {
            let loc = c.tables.calls.get(call)?.loc;
            let file = *c.files.get(loc.file as usize)?;
            let item = crate::work::item_holding(&file.outline, loc.offset)?;
            let from = loc.offset.saturating_sub(item.range.start);
            let range = match find_word(item.range.of(file.text), name, from as usize) {
                Some(start) => (start as u32, (start + name.len()) as u32),
                None => (from, from),
            };
            Some((file, item, range))
        }
    });
    match place {
        Some((file, item, range)) => {
            at = range;
            message.extend_from_slice(file.path.trim_start_matches("/0/").as_bytes());
            message.push(0x1F);
            let chain = crate::work::item_chain(&file.outline, item, file.text);
            for shift in (0..16).rev() {
                message.push(b"0123456789abcdef"[((chain >> (shift * 4)) & 0xF) as usize]);
            }
        }
        None => message.push(0x1F),
    }
    message.push(0x1F);
    message.push(b'0' + c.edition.byte());
    message.push(0x1F);
    message.extend_from_slice(std::format!("the function `{name}`").as_bytes());
    if let Some(why) = why {
        message.push(0x1F);
        message.extend_from_slice(why.as_bytes());
    }
    Unresolved { at, message }
}
