//! The names and keys a body's check is filed under (lane R8, 2026-11-17): what separates two
//! equal texts in two crates, what separates two crate lists over one file (hello world's
//! `run` then `check`: "held CrateScope(1e39..), put CrateScope(1e08..)", one key), the out of a
//! file entry whose macro calls wrote heads (signature.rs, "A HEAD A MACRO WROTE"), and that a
//! job's reads have one order.

use blaze_drive::key::{ABSENT, Tool, owned_key, owners_of, value_key};
use blaze_drive::{Kind, Name};
use blaze_ids::ContentId;
use blaze_surface::{Edition, ItemKind};

use super::{body_check_key, canonical_reads};
use crate::signature::{entry_out, expansion_out, file_subject, is_signature_entry, signature_name};

const TEXT: &str = "pub fn same(x: u32) -> u32 {\n    x + 1\n}\n";

fn id(byte: u8) -> ContentId {
    ContentId([byte; 32])
}

/// One `fn same` written alike in two crates' `src/lib.rs`: its `Signature` names differ (the
/// file subject is the real path, which holds the crate's directory), and so do its
/// `BodyCheck` keys under one body name, one item digest, one graph and one owners out: the
/// file's `Owners` name is in the key. Also: the same file's key under two crate lists differs
/// (the `UnitGraph` out is in the key, B), and a rerun of the same inputs gives the same key.
#[test]
fn equal_text_in_two_crates_gives_two_keys() {
    let outline = blaze_file_items::outline(TEXT, Edition::E2021, blaze_own::testing::heap().alloc());
    let item = outline.items().iter().find(|i| matches!(i.kind(), ItemKind::Function)).expect("the outline lists `fn same`");
    let a = signature_name(file_subject("a/src/lib.rs"), &outline, item, TEXT);
    let b = signature_name(file_subject("b/src/lib.rs"), &outline, item, TEXT);
    assert_ne!(a, b, "one head text in two crates is two `Signature`s");
    assert_eq!(a, signature_name(file_subject("a/src/lib.rs"), &outline, item, TEXT), "a rerun names it alike");

    let tool = Tool(id(7));
    let body = Name::new(Kind::BodyCheck, id(1));
    let (file_a, file_b) = (Name::file(b"a/src/lib.rs"), Name::file(b"b/src/lib.rs"));
    let (graph, owners) = (id(2), id(3));
    let ka = body_check_key(tool, body, item.hash, file_a, graph, owners);
    let kb = body_check_key(tool, body, item.hash, file_b, graph, owners);
    assert_ne!(ka, kb, "one body text in two crates' files is two `BodyCheck` keys");
    assert_eq!(ka, body_check_key(tool, body, item.hash, file_a, graph, owners), "a rerun keys it alike");
    let other_list = body_check_key(tool, body, item.hash, file_a, id(4), owners);
    assert_ne!(ka, other_list, "one file under two crate lists (run, then check) is two keys");
    let no_graph = body_check_key(tool, body, item.hash, file_a, ABSENT, owners);
    assert_ne!(ka, no_graph, "a command with no unit graph keys apart from one with");
}

/// The drive's fold and blaze-cli's are one derivation: `owned_key` is the value key of the
/// task key over exactly the (UnitGraph, Owners) reads, in name order.
#[test]
fn owned_key_is_the_value_key_of_graph_and_owners() {
    let task = blaze_drive::key::task_key(Tool(id(7)), Name::new(Kind::Signature, id(1)), &id(9));
    let owners = owners_of(Name::file(b"a/src/lib.rs"));
    assert!(Name::unit_graph() < owners, "the read list's order puts the graph first");
    assert_eq!(owned_key(task, id(2), owners, id(3)), value_key(task, &[(Name::unit_graph(), id(2)), (owners, id(3))]));
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

/// A read list gathered in two orders, with a name twice, is one list: sorted by name, one read
/// a name, the first of a name kept.
#[test]
fn reads_have_one_canonical_order() {
    let names = [Name::new(Kind::CrateScope, id(5)), Name::unit_graph(), owners_of(Name::file(b"a/src/lib.rs")), Name::new(Kind::CrateScope, id(1))];
    let mut one = [(names[0], id(1)), (names[1], id(2)), (names[2], id(3)), (names[3], id(4)), (names[0], id(9))];
    let mut two = [(names[3], id(4)), (names[0], id(1)), (names[2], id(3)), (names[0], id(9)), (names[1], id(2))];
    let (k1, k2) = (canonical_reads(&mut one), canonical_reads(&mut two));
    assert_eq!(k1, 4, "one read a name");
    assert_eq!(&one[..k1], &two[..k2], "the order gathered in does not reach the list");
    assert!(one[..k1].windows(2).all(|w| w[0].0 < w[1].0), "sorted by name");
    assert_eq!(one[..k1].iter().find(|(n, _)| *n == names[0]).map(|(_, out)| *out), Some(id(1)), "the first of a name kept (as gathered in `one`)");
}
