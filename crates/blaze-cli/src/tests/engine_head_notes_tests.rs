//! The engine's head tasks with a constant evaluator, and the reason a head was not frozen
//! always printed (engine_head_consts.rs, engine_head_notes.rs). A child of main.rs's tests,
//! over engine_check_tests.rs's helpers.

use super::engine_check_tests::{engine_opts, entries, opts, said};
use super::engine_heads_once_tests::{fixture_signature, frozen_runs, module_members};
use super::*;
use crate::engine_heads::ListingsPlant;

/// One package `heads`: `takes` holds an array of `LEN` in its signature, `uses` calls it, and
/// `LEN` is alone in `src/len.rs` (so its edit moves no other file).
fn constant_crate(io: &Io, len: &str) {
    write(io, "Cargo.toml", "[package]\nname = \"heads\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "src/lib.rs", "mod len;\n\nuse len::LEN;\n\npub fn takes(x: [u8; LEN]) -> u8 {\n    x[0]\n}\n\npub fn uses() -> u8 {\n    takes([1; LEN])\n}\n");
    write(io, "src/len.rs", &format!("pub const LEN: usize = {len};\n"));
}

/// A HEAD WHOSE TYPE HOLDS A CONSTANT IS FROZEN, AND READS WHAT ITS VALUE WAS MADE FROM:
/// `takes(x: [u8; LEN])` with `LEN = 2 + 2` is frozen and `uses` is checked clean; `LEN`
/// edited to `3 + 2` enters, past the outline of `len.rs`, exactly `LEN`'s head task and body,
/// `takes`' head task (its array length changed)
/// and the two bodies that read `takes`' head (its own and `uses`), and `uses` is clean again.
/// CONTROL: with no evaluator (`consts: None`, before), `takes`' head failed with "the freeze
/// has no constant evaluator" and `uses` was refused by that read.
#[test]
fn a_head_holding_a_constant_is_frozen_and_moves_with_its_value() {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    constant_crate(&io, "2 + 2");
    let (_, first_lines, first) = said(&io, false);
    let first_text = first_lines.join("\n");
    println!("the first request: {}\n{first_text}", entries(&first));
    assert!(!first_text.contains("could not be frozen"), "every head frozen:\n{first_text}");
    assert!(!first_text.contains("heads: "), "no head failed:\n{first_text}");
    assert!(first.bodies.0.is_empty(), "every body checked clean: {:?}\n{first_text}", first.bodies.0);
    constant_crate(&io, "3 + 2");
    let (_, now_lines, now) = said(&io, false);
    let now_text = now_lines.join("\n");
    println!("LEN edited: {}\n{now_text}", entries(&now));
    assert!(now.bodies.0.is_empty(), "every body checked clean after the edit: {:?}\n{now_text}", now.bodies.0);
    let entered = |stage: blaze_engine::Stage| now.stages[stage.index()].entered;
    assert_eq!(entered(blaze_engine::Stage::Outlines), 1, "the outline of `len.rs` alone: {}", entries(&now));
    assert_eq!(entered(blaze_engine::Stage::Heads), 2, "`LEN`'s head task and `takes`': {}", entries(&now));
    assert_eq!(entered(blaze_engine::Stage::Bodies), 6, "`LEN`'s body, `takes` and `uses`, each in its 2 units' rows (D191: the library's and its test build's): {}", entries(&now));
    // The incremental answer is a fresh run's.
    let (_, fresh_lines, fresh) = said(&io, true);
    assert_eq!(now.bodies.0, fresh.bodies.0, "the incremental body notes are a fresh run's");
    assert_eq!(now.status.0, fresh.status.0, "the incremental status is a fresh run's:\n{}", fresh_lines.join("\n"));
}

const WIDTH_FILE: &str = "a/src/width.rs";
const LOCAL: &str = "pub fn local(x: [u8; LEN]) -> u8 { x[0] }";
const REMOTE: &str = "pub fn remote(x: [u8; a::width::LEN]) -> u8 { x[0] }";
const OTHER: &str = "pub fn other(x: [u8; a::width::LEN]) -> u8 { x[0] }";

fn width_file(offset: u8, width: u8, comparison: &str) -> std::string::String {
    format!("pub const fn width() -> usize {{ {width} }}\npub const LEN: usize = width() + {offset};\npub struct Book;\nimpl Book {{ pub fn is_empty(a: usize, b: usize) -> bool {{ {comparison} }} }}\n{LOCAL}\n")
}

fn length_workspace(source: &str) -> Io {
    module_members(WIDTH_FILE, source, "pub mod width;\n", &format!("{REMOTE}\n{OTHER}\n"))
}

fn check_lengths(io: &Io, plant: ListingsPlant) -> engine_check::Checked {
    let mut options = opts(io, 2, false);
    options.head_jobs.listings = plant;
    let (ids, checked) = engine_opts(io, options);
    assert!(ids.is_empty(), "well-typed workspace: {ids:?}");
    checked
}

/// Read each unit's actual frozen array type. Bytes from a stale warm and a stale cold
/// evaluator could agree, so the declared length is an independent oracle as well.
fn length_heads(io: &Io, width_source: &str, expected: u64) -> (bool, std::vec::Vec<std::vec::Vec<u8>>) {
    use blaze_ladder::front2::head_record::{self, Head};
    use blaze_ladder::front2::tables::head::FrozenHead;
    use blaze_typeck::PNode;

    let alloc = blaze_own::testing::heap().alloc();
    let remote_source = format!("{REMOTE}\n{OTHER}\n");
    let mut matches = true;
    let mut payloads = std::vec::Vec::new();
    for (path, source, name, item) in [
        (WIDTH_FILE, width_source, "local", LOCAL),
        ("b/src/lib.rs", remote_source.as_str(), "remote", REMOTE),
        ("b/src/lib.rs", remote_source.as_str(), "other", OTHER),
    ] {
        let (_, runs) = frozen_runs(io, fixture_signature(path, source, name));
        let mut count = 0;
        crate::engine_seeds::each_run(&runs, &mut |_, _, at, len| {
            let frozen = FrozenHead::read(&runs[at..at + len]).expect("valid frozen function");
            let mut heads = frozen.heads();
            let record = heads.next().expect("the function head").record;
            assert!(heads.next().is_none(), "{name} has one head in this unit");
            let tys = blaze_ladder::front2::rows::stable_tys(alloc).expect("fresh portable types");
            let Head::Fn { head, .. } = head_record::decode(record, Some(item), &tys, alloc).expect("decode the signature") else { panic!("{name} is a function") };
            let [input] = &head.inputs[..] else { panic!("{name} takes one parameter") };
            let PNode::Array(_, len) = tys.view().node(*input).expect("parameter type") else { panic!("{name} takes an array") };
            let len = tys.view().value_u64(len).expect("a concrete array length");
            matches &= len == expected;
            count += 1;
        }).expect("stored signature runs");
        assert!(count > 0, "no unit froze {name}");
        payloads.push(runs);
    }
    (matches, payloads)
}

#[test]
fn an_array_head_depends_on_its_initializer_and_called_const_fn_across_members() {
    use blaze_engine::{CellId, Stage};
    use crate::engine_scope_reads::CellKind;

    let initial = width_file(1, 2, "a == b");
    let unrelated = width_file(1, 2, "b == a");
    assert_eq!(initial.len(), unrelated.len());
    let init_changed = width_file(2, 2, "b == a");
    let callee_changed = width_file(2, 3, "b == a");
    let io = length_workspace(&initial);
    check_lengths(&io, ListingsPlant::Rule);
    assert!(length_heads(&io, &initial, 3).0);
    for (source, length, edit) in [(&unrelated, 3, "unrelated body"), (&init_changed, 4, "initializer"), (&callee_changed, 5, "const fn body")] {
        write(&io, WIDTH_FILE, source);
        let now = check_lengths(&io, ListingsPlant::Rule);
        assert!(now.from_last && !now.certified, "{edit} reopened the previous answer");
        assert_eq!(now.stages[Stage::Outlines.index()].entered, 1, "{edit} changed one file");
        let heads = now.stages[Stage::Heads.index()].entered;
        if edit == "unrelated body" {
            assert_eq!(heads, 0, "none of the array heads reads is_empty: {}", entries(&now));
        }
        assert!(now.because.iter().all(|(group, _)| !(group.stage == Stage::Heads && group.definer.is_some_and(|name| name.kind == blaze_drive::Kind::FileItems) && group.cell == Some(CellKind::WholeRecord))), "{edit}: {:?}", now.because);
        let fresh = length_workspace(source);
        check_lengths(&fresh, ListingsPlant::Rule);
        let (correct, bytes) = length_heads(&io, source, length);
        let (fresh_correct, fresh_bytes) = length_heads(&fresh, source, length);
        assert!(correct && fresh_correct, "{edit}: local and remote arrays have length {length}");
        assert_eq!(bytes, fresh_bytes, "{edit}: per-unit frozen bytes");
        println!("{edit}: {heads} heads entered, local and remote lengths {length}, frozen bytes equal fresh");
    }

    let broad = length_workspace(&initial);
    check_lengths(&broad, ListingsPlant::WholeItemFile);
    write(&broad, WIDTH_FILE, &unrelated);
    let extra = check_lengths(&broad, ListingsPlant::WholeItemFile);
    assert!(extra.stages[Stage::Heads.index()].entered > 0, "a whole-file read must break the zero-head bound");
    assert!(extra.because.iter().any(|(group, _)| group.stage == Stage::Heads && group.definer == Some(blaze_drive::Name::file(WIDTH_FILE.as_bytes())) && group.cell == Some(CellKind::WholeRecord)));

    // The const declaration still has type usize after its initializer changes.
    // Its frozen type alone cannot prove the value embedded in the array types.
    let missing = length_workspace(&unrelated);
    let len = fixture_signature(WIDTH_FILE, &unrelated, "LEN");
    let id = CellId { of: blaze_drive::Name::file(WIDTH_FILE.as_bytes()), cell: crate::engine_tasks::definition_cell(len) };
    let plant = ListingsPlant::DropItemText(id);
    check_lengths(&missing, plant);
    assert!(length_heads(&missing, &unrelated, 3).0);
    write(&missing, WIDTH_FILE, &init_changed);
    check_lengths(&missing, plant);
    let (correct, stale) = length_heads(&missing, &init_changed, 4);
    assert!(!correct, "dropping the initializer read must leave a stale length");
    let fresh = length_workspace(&init_changed);
    check_lengths(&fresh, ListingsPlant::Rule);
    assert_ne!(stale, length_heads(&fresh, &init_changed, 4).1, "the oracle must detect different frozen bytes");
    println!("whole-file control entered {} heads; missing initializer input left stale frozen bytes", extra.stages[Stage::Heads.index()].entered);
}

#[test]
fn restored_array_heads_match_fresh_after_reordered_edits() {
    blaze_sim::check(blaze_sim::Config::cases(4), |tape| {
        let mut offset = 1;
        let mut width = 2;
        let mut swapped = false;
        let io = length_workspace(&width_file(offset, width, "a == b"));
        check_lengths(&io, ListingsPlant::Rule);
        for _ in 0..4 {
            let edit = tape.range(0..3, "edit unrelated body, initializer or const fn");
            match edit {
                0 => swapped = !swapped,
                1 => offset = 3 - offset,
                _ => width = 5 - width,
            }
            let source = width_file(offset, width, if swapped { "b == a" } else { "a == b" });
            write(&io, WIDTH_FILE, &source);
            let warm = check_lengths(&io, ListingsPlant::Rule);
            if edit == 0 && warm.stages[blaze_engine::Stage::Heads.index()].entered != 0 {
                return Err(blaze_sim::Broke::Oracle(format!("unrelated body entered a head: {}", entries(&warm))));
            }
            let fresh = length_workspace(&source);
            check_lengths(&fresh, ListingsPlant::Rule);
            let expected = u64::from(offset + width);
            let (right, actual) = length_heads(&io, &source, expected);
            let (fresh_right, cold) = length_heads(&fresh, &source, expected);
            if !right || !fresh_right || actual != cold {
                return Err(blaze_sim::Broke::Oracle(format!("edit {edit}, offset {offset}, width {width}: restored or fresh frozen lengths differ from {expected}, or their record bytes differ")));
            }
        }
        Ok(())
    });
}

/// One package `heads`: an impl of two fns, `bad`'s signature holding a constant whose value
/// cannot be evaluated (its initializer calls a fn that does not exist), and a caller of each.
/// `calls_bad` names no `BAD` of its own (its argument is a parameter): a `[0; BAD]` in its body
/// was its own lowering's refusal, before it read `bad`'s head, so its note was not `bad`'s.
fn member_crate(io: &Io) {
    write(io, "Cargo.toml", "[package]\nname = \"heads\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(
        io,
        "src/lib.rs",
        "pub struct S;\n\npub const BAD: usize = not_defined();\n\nimpl S {\n    pub fn good(&self) -> u8 {\n        1\n    }\n\n    pub fn bad(&self, x: [u8; BAD]) -> u8 {\n        x[0]\n    }\n}\n\npub fn calls_good(s: &S) -> u8 {\n    s.good()\n}\n\npub fn calls_bad(s: &S, x: [u8; 4]) -> u8 {\n    s.bad(x)\n}\n",
    );
}

/// ONE MEMBER'S HEAD FAILS ALONE, AND ITS REASON IS ALWAYS PRINTED (runtime.md A8): `bad`'s
/// head cannot be frozen (its constant has no value); `good`'s, in the same impl's head task,
/// is, so `calls_good` is checked; `calls_bad` is refused with a note that names `bad` and
/// quotes the head's reason; the closing lines group it ("heads: 1 head(s) failed: ..."); and a
/// second request with nothing changed prints the same note and group from the stored lines.
/// CONTROL: before, the impl's head task failed whole (both callers refused), the note said
/// "its head task failed (engine_heads.rs; the Heads stage's refusals name it)" with no head
/// and no reason, and the failed head task's row was among the "not printed" tasks.
#[test]
fn one_member_fails_alone_and_its_reason_is_printed() {
    let io = Io::simulated(SimWorld::new(19, Limits::USUAL));
    member_crate(&io);
    let quoted = "the value of a constant its type holds";
    let mut answers = std::vec::Vec::new();
    for round in ["first", "nothing changed"] {
        let (_, lines, checked) = said(&io, false);
        let text = lines.join("\n");
        println!("{round}: {}\n{text}", entries(&checked));
        let note = lines.iter().find(|line| line.contains("body `calls_bad`")).unwrap_or_else(|| panic!("{round}: `calls_bad` is refused with a note:\n{text}"));
        assert!(note.contains("::bad` it reads could not be frozen") && note.contains(quoted), "{round}: the note names `bad` and quotes its head's reason: {note}");
        // Checked: its only note may be the borrow check's (this fixture's core has no `Copy`
        // lang item, so borrows are "not checked: a trait fact of an ADT is not known"), never
        // a refusal of the body.
        let refused = lines.iter().any(|line| line.contains("body `calls_good`") && !line.contains("borrows not checked: "));
        assert!(!refused, "{round}: `calls_good` is checked (`good`'s head is frozen):\n{text}");
        assert!(!text.contains("kept no reason"), "{round}: every failed head kept its reason:\n{text}");
        let grouped: std::vec::Vec<&std::string::String> = lines.iter().filter(|line| line.contains("heads: ")).collect();
        assert_eq!(grouped.len(), 1, "{round}: one reason grouped:\n{text}");
        assert!(grouped[0].contains("heads: 1 head(s) failed: ") && grouped[0].contains(quoted) && grouped[0].contains("::bad`"), "{round}: the group names its reason and `bad`: {}", grouped[0]);
        assert!(!text.contains("not printed here"), "{round}: no task stands unprinted:\n{text}");
        answers.push((note.clone(), grouped[0].clone()));
    }
    assert_eq!(answers[0], answers[1], "nothing changed prints the same note and group");
}
