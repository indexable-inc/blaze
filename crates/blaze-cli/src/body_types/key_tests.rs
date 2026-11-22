//! The names and outs a body's heads are filed under (lane R8, 2026-11-17; signature.rs): what
//! separates two equal texts in two crates, and the out of a file entry whose macro calls wrote
//! heads (signature.rs, "A HEAD A MACRO WROTE").

use blaze_ids::ContentId;
use blaze_surface::{Edition, ItemKind};

use crate::signature::{entry_out, expansion_out, file_subject, is_signature_entry, signature_name};

const TEXT: &str = "pub fn same(x: u32) -> u32 {\n    x + 1\n}\n";

fn id(byte: u8) -> ContentId {
    ContentId([byte; 32])
}

/// One `fn same` written alike in two crates' `src/lib.rs`: its `Signature` names differ (the
/// file subject is the real path, which holds the crate's directory), and a rerun names it alike.
#[test]
fn equal_text_in_two_crates_gives_two_signatures() {
    let outline = blaze_file_items::outline(TEXT, Edition::E2021, blaze_own::testing::heap().alloc());
    let item = outline.items().iter().find(|i| matches!(i.kind(), ItemKind::Function)).expect("the outline lists `fn same`");
    let a = signature_name(file_subject("a/src/lib.rs"), &outline, item, TEXT);
    let b = signature_name(file_subject("b/src/lib.rs"), &outline, item, TEXT);
    assert_ne!(a, b, "one head text in two crates is two `Signature`s");
    assert_eq!(a, signature_name(file_subject("a/src/lib.rs"), &outline, item, TEXT), "a rerun names it alike");
}

/// A macro call at module level is a `Signature` entry of its own (the heads its expansion
/// writes are read by it); one inside a fn's body is not (the body's own text covers it).
#[test]
fn a_module_level_macro_call_is_a_signature_entry() {
    let text = "write_two!();\nfn host() {\n    write_two!();\n}\n";
    let outline = blaze_file_items::outline(text, Edition::E2021, blaze_own::testing::heap().alloc());
    let calls: std::vec::Vec<bool> = outline.items().iter().filter(|i| i.kind() == ItemKind::MacroCall).map(|i| is_signature_entry(&outline, i)).collect();
    assert_eq!(calls.first(), Some(&true), "the module-level call is an entry: {calls:?}");
    assert!(calls.iter().skip(1).all(|entry| !entry), "a call inside a body is no entry: {calls:?}");
}

/// An entry's out: with no expansion it is its own head's out, unchanged (every existing
/// `Signature` keeps its out); each expansion at it moves it, and two expansions in another
/// order or count are another out.
#[test]
fn an_entry_out_holds_its_expansions_in_order() {
    let own = id(1);
    assert_eq!(entry_out(own, &[]), own, "no expansion: the entry's own out");
    assert_ne!(entry_out(own, &[id(2)]), own, "an expansion's heads are in the out");
    assert_ne!(entry_out(own, &[id(2), id(3)]), entry_out(own, &[id(3), id(2)]), "the calls' order is the key order and is held");
    assert_ne!(entry_out(own, &[id(2)]), entry_out(own, &[id(2), id(2)]), "the count is held");
    assert_eq!(entry_out(own, &[id(2), id(3)]), entry_out(own, &[id(2), id(3)]), "a rerun, the same out");
}

/// An expansion's out moves when a head it writes is edited (a parameter's type), not when
/// only a body it writes is, nor when only its want digests are equal and its text is
/// reformatted; another want digest moves it.
#[test]
fn an_expansion_out_moves_on_its_heads_only() {
    let out = |text: &str, digests: &[ContentId]| {
        let outline = blaze_file_items::outline(text, Edition::E2021, blaze_own::testing::heap().alloc());
        expansion_out(digests, &outline, text, Edition::E2021)
    };
    let written = out("pub fn made(x: u32) -> u32 { x }\n", &[id(4)]);
    assert_ne!(written, out("pub fn made(x: u64) -> u32 { 0 }\n", &[id(4)]), "a head edit moves it");
    assert_eq!(written, out("pub fn made(x: u32) -> u32 { x + 1 }\n", &[id(4)]), "a body edit does not");
    assert_eq!(written, out("pub fn made( x : u32 ) -> u32 { x }\n", &[id(4)]), "a reformat does not");
    assert_ne!(written, out("pub fn made(x: u32) -> u32 { x }\n", &[id(5)]), "another head record digest moves it");
}
