//! HEADS A MACRO WROTE ARE THEIR ENTRY'S HEAD TASK'S (lane RH4, engine_heads.rs "WRITTEN
//! HEADS"): a struct, an impl, an impl fn and a fn a module-level `macro_rules!` call writes, the
//! same through a call nested in another macro's output, two calls of one macro in one file, and
//! two impls of one member name one call writes, are each frozen by the head task of the file
//! entry their outermost call stands at, under a cell that holds where under it they were
//! written. No body freezes one (the counter of member heads frozen at a seed is 0: before this
//! lane the fixture `module-facts` counted 1 and `unnamed-calls` 2, engine_member_heads_tests.rs),
//! and no body is refused for one. A child of main.rs's tests, over engine_check_tests.rs's
//! helpers. Every fixture text is a `&'static str` made by `concat!`.
//!
//! THE ORACLE: each incremental answer equals a fresh request's on the same files; and the head
//! tasks of the macro-written fixture freeze as many heads by item as those of its twin written
//! by hand.

use super::*;

use super::engine_check_tests::{engine_opts, entries, error_lines, opts};
use crate::engine_heads::HeadUnits;
use blaze_engine::Stage;

/// `src/lib.rs`: a trait; `make!` writing a struct, its inherent impl with `new`, an impl of the
/// trait and (its tail, `$make_tail`) a fn whose body declares an item of its own; `outer!`
/// calling `make!` (a nested call); `two_impls!` writing two impls whose fns share a name (one
/// entry, two unnamed impls: two cells only by where they were written); three calls; a reader
/// of `a` (`$read`); and a fn the comment row edits.
macro_rules! lib {
    ($make_tail:literal, $a_call:literal, $c_call:literal, $read:literal, $unrelated:literal) => {
        concat!(
            "pub trait Tr {\n    fn get(&self) -> u8;\n}\n\n",
            "macro_rules! make {\n    ($n:ident, $f:ident) => {\n        pub struct $n(pub u8);\n        impl $n {\n            pub fn new() -> $n {\n                $n(1)\n            }\n        }\n        impl Tr for $n {\n            fn get(&self) -> u8 {\n                self.0\n            }\n        }\n",
            $make_tail,
            "    };\n}\n\n",
            "macro_rules! outer {\n    ($n:ident, $f:ident) => {\n        make!($n, $f);\n    };\n}\n\n",
            "macro_rules! two_impls {\n    () => {\n        impl Tr for Pair {\n            fn get(&self) -> u8 {\n                2\n            }\n        }\n        impl Tr for Other {\n            fn get(&self) -> u8 {\n                3\n            }\n        }\n    };\n}\n\n",
            $a_call,
            $c_call,
            "outer!(D, d);\n\npub struct Pair;\npub struct Other;\n\ntwo_impls!();\n\n",
            "pub fn read() -> u8 {\n    ",
            $read,
            "\n}\n\npub fn unrelated() -> u8 {\n",
            $unrelated,
            "    7\n}\n"
        )
    };
}

/// `make!`'s tail in [`BASE`]: the fn `$f`, whose body declares `Local` and its impl (front2
/// tables/seeds.rs `of_the_body`: its body's own store makes their heads), and reads `$n`'s.
macro_rules! base {
    ($a_call:literal, $c_call:literal, $unrelated:literal) => {
        lib!(
            "        pub fn $f() -> u8 {\n            struct Local(u8);\n            impl Local {\n                fn get(&self) -> u8 {\n                    self.0\n                }\n            }\n            Local($n::new().get()).get()\n        }\n",
            $a_call,
            $c_call,
            "a()",
            $unrelated
        )
    };
}

const BASE: &str = base!("make!(A, a);\n", "make!(C, c);\n", "");
/// A comment in `unrelated`'s body (R2's edit, at a fn no macro wrote).
const COMMENTED: &str = base!("make!(A, a);\n", "make!(C, c);\n", "    // a comment\n");
/// The first call's input edited: `A` is `B`.
const A_RENAMED: &str = base!("make!(B, a);\n", "make!(C, c);\n", "");
/// The other call's input edited: `C` is `E`.
const C_RENAMED: &str = base!("make!(A, a);\n", "make!(E, c);\n", "");
/// `make!`'s definition edited (the tail's `Local` takes `0`): every call's output moves.
const DEFINITION: &str = lib!(
    "        pub fn $f() -> u8 {\n            struct Local(u8);\n            impl Local {\n                fn get(&self) -> u8 {\n                    self.0\n                }\n            }\n            Local(0).get()\n        }\n",
    "make!(A, a);\n",
    "make!(C, c);\n",
    "a()",
    ""
);

/// [`BASE`] written by hand: what each call writes, in place of the calls.
const TWIN: &str = concat!(
    "pub trait Tr {\n    fn get(&self) -> u8;\n}\n\n",
    "pub struct A(pub u8);\nimpl A {\n    pub fn new() -> A {\n        A(1)\n    }\n}\nimpl Tr for A {\n    fn get(&self) -> u8 {\n        self.0\n    }\n}\n",
    "pub fn a() -> u8 {\n    struct Local(u8);\n    impl Local {\n        fn get(&self) -> u8 {\n            self.0\n        }\n    }\n    Local(A::new().get()).get()\n}\n\n",
    "pub struct C(pub u8);\nimpl C {\n    pub fn new() -> C {\n        C(1)\n    }\n}\nimpl Tr for C {\n    fn get(&self) -> u8 {\n        self.0\n    }\n}\n",
    "pub fn c() -> u8 {\n    struct Local(u8);\n    impl Local {\n        fn get(&self) -> u8 {\n            self.0\n        }\n    }\n    Local(C::new().get()).get()\n}\n\n",
    "pub struct D(pub u8);\nimpl D {\n    pub fn new() -> D {\n        D(1)\n    }\n}\nimpl Tr for D {\n    fn get(&self) -> u8 {\n        self.0\n    }\n}\n",
    "pub fn d() -> u8 {\n    struct Local(u8);\n    impl Local {\n        fn get(&self) -> u8 {\n            self.0\n        }\n    }\n    Local(D::new().get()).get()\n}\n\n",
    "pub struct Pair;\npub struct Other;\n\n",
    "impl Tr for Pair {\n    fn get(&self) -> u8 {\n        2\n    }\n}\nimpl Tr for Other {\n    fn get(&self) -> u8 {\n        3\n    }\n}\n\n",
    "pub fn read() -> u8 {\n    a()\n}\n\npub fn unrelated() -> u8 {\n    7\n}\n"
);

/// THE CONTROL's fixture before its edit: `make!` writes no fn, and `read` reads nothing of it.
const NEW_ITEM_BEFORE: &str = lib!("", "make!(A, a);\n", "make!(C, c);\n", "0", "");
/// After: `make!`'s definition writes a fn `$f` (a NEW item of every call's output, so no `Item`
/// cell a head task read before moves), and `read` calls `a`.
const NEW_ITEM_AFTER: &str = lib!("        pub fn $f() -> u8 {\n            1\n        }\n", "make!(A, a);\n", "make!(C, c);\n", "a()", "");

/// What one request listed, printed (its closing line dropped) and counted.
struct Answer {
    lines: std::vec::Vec<std::string::String>,
    checked: engine_check::Checked,
}

/// One request on `workers`, from the stored answer unless `fresh`, its head tasks run `units`'
/// way (the rule, or THE PLANT).
fn request(io: &Io, workers: u32, fresh: bool, units: HeadUnits) -> Answer {
    let mut opts = opts(io, workers, fresh);
    opts.head_units = units;
    let before = stdout(io).len();
    let (_, checked) = engine_opts(io, opts);
    let text = stdout(io).split_off(before);
    let mut lines: std::vec::Vec<std::string::String> = text.lines().map(str::to_owned).collect();
    lines.pop();
    Answer { lines, checked }
}

/// The printed answer: each finding line and each body's note, in order.
fn answer_lines(answer: &Answer) -> std::vec::Vec<&str> {
    answer.lines.iter().map(std::string::String::as_str).filter(|line| line.contains("error[") || line.contains("warning[") || line.starts_with("note:")).collect()
}

/// Whether two answers say the same: their findings and notes, and their status.
fn same_answer(one: &Answer, other: &Answer) -> bool {
    answer_lines(one) == answer_lines(other) && one.checked.status.0 == other.checked.status.0
}

/// The `Signature` head tasks a request entered, counted by kind from the engine's entered list
/// (engine_check.rs `Checked::signatures_entered`). Not the Heads stage's entries less
/// `Checked::heads.0`: that is the Heads rows of the whole answer, standing ones too (the first
/// run's helper said 0 where the stage line said 1 entered).
fn head_tasks(checked: &engine_check::Checked) -> u32 {
    checked.signatures_entered
}

/// Why each task was entered, with the kind of the read that moved (engine_check.rs `Because`):
/// which read carried an edit to a head task.
fn moved_reads(checked: &engine_check::Checked) -> std::vec::Vec<(Stage, &'static str, Option<crate::engine_scope_reads::CellKind>, u32)> {
    checked.because.iter().map(|(group, count)| (group.stage, group.cause, group.cell, *count)).collect()
}

/// The words of a body left unchecked for a head (engine_seeds.rs `no_cell`, `other_units`) or
/// of a head task refused for two heads under one cell (engine_heads.rs `heads_of`).
const REFUSALS: [&str; 4] = ["a body never freezes a member's head", "and not under this body's unit", "fall under one cell", "holds no text"];

/// Exclude borrow-check notes because the fixture's core has no `Copy` lang item.
fn unchecked(answer: &Answer) -> std::vec::Vec<&std::string::String> {
    answer.checked.bodies.0.iter().filter(|note| !note.contains("borrows not checked: ")).collect()
}

/// THE RULE on one fresh answer of a clean fixture: no body refused or left unchecked, no member
/// head frozen at a seed, no head task refused for two heads under one cell, no error.
fn written_heads_hold(what: &str, answer: &Answer) {
    let seeds = &answer.checked.seeds;
    let text = answer.lines.join("\n");
    println!("{what}: member heads frozen at a seed {}, bodies refused {}, heads frozen {} for {} items; {}", seeds.not_put.member, seeds.bodies.refused, seeds.units.frozen, seeds.units.items, entries(&answer.checked));
    for words in REFUSALS.iter() {
        assert!(!text.contains(words), "{what}: nothing refused ({words:?}):\n{text}");
    }
    assert_eq!(seeds.not_put.member, 0, "{what}: no member crate's head frozen at a seed: the head tasks froze every written head");
    assert_eq!(seeds.bodies.refused, 0, "{what}: no body refused at a seed");
    assert!(unchecked(answer).is_empty(), "{what}: every body checked: {:?}", unchecked(answer));
    assert!(error_lines(&text).is_empty(), "{what}: the fixture checks clean:\n{text}");
    assert_ne!(answer.checked.status.0, Status::FAILED.0, "{what}: the request does not fail:\n{text}");
}

/// EVERY WRITTEN ITEM HAS A HEAD TASK: fresh, on 1 and 4 workers, the macro-written fixture
/// checks clean with no member head frozen at a seed and every body checked (the written fns'
/// bodies with their own `Local` items included), and its head tasks freeze as many heads by
/// item as its twin written by hand (each struct, impl, impl fn and fn a call wrote is one; a
/// `macro_rules!` definition and a call are none). CONTROL: before lane RH4 the written heads
/// were frozen at their bodies' seeds and counted (engine_seeds.rs, the counter this lane
/// deleted with its hole), and the twin's count stands for "every item one head task's".
#[test]
fn engine_expansion_heads_freeze_every_written_item() {
    for workers in [1u32, 4] {
        let written = Io::simulated(SimWorld::new(31, Limits::USUAL));
        write(&written, "Cargo.toml", MANIFEST);
        write(&written, "src/lib.rs", BASE);
        let macro_answer = request(&written, workers, true, HeadUnits::Each);
        written_heads_hold("written by macros", &macro_answer);
        let by_hand = Io::simulated(SimWorld::new(31, Limits::USUAL));
        write(&by_hand, "Cargo.toml", MANIFEST);
        write(&by_hand, "src/lib.rs", TWIN);
        let twin_answer = request(&by_hand, workers, true, HeadUnits::Each);
        written_heads_hold("written by hand", &twin_answer);
        // Heads by item, not cells: a written head's cell holds where it was written.
        assert_eq!(macro_answer.checked.seeds.units.items, twin_answer.checked.seeds.units.items, "{workers} worker(s): the head tasks freeze each written item's head, as many as the twin by hand");
    }
}

/// THE EDIT ROWS of written heads, each from the stored answer of the row before and checked
/// against a fresh request on the same files (THE ORACLE):
/// - a comment in `unrelated`'s body enters no head task (a comment is no token of its entry's
///   key) and no call's;
/// - `make!(A, a)` to `make!(B, a)` enters that call's head task alone (no head reads `A`);
/// - `make!(C, c)` to `make!(E, c)` enters that call's alone: the file's other call's is not;
/// - `make!`'s definition edited enters every call's (`make!(A)`, `make!(C)` and `outer!(D)`,
///   whose nested `make!` reads its record), and the definition's own entry when it has one.
#[test]
fn engine_expansion_heads_edit_rows() {
    let io = Io::simulated(SimWorld::new(37, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", BASE);
    let cold = request(&io, 2, true, HeadUnits::Each);
    written_heads_hold("cold", &cold);
    // Each row's edit is one entry's from the row before (the definition's: its own entry, if
    // it has a head task, and the three calls it writes for).
    let rows: [(&str, &str, core::ops::RangeInclusive<u32>); 7] = [
        // A comment is no token: since item keys are from tokens (lane K1, 2026-11-18) the edit
        // moves no head task's key (first run: 0 entered, the answer a fresh request's). It was
        // 1 while a fn's head task held its text's bytes.
        ("a comment in an unrelated fn", COMMENTED, 0..=0),
        ("the comment taken out", BASE, 0..=0),
        ("`make!(A, a)` is `make!(B, a)`", A_RENAMED, 1..=1),
        ("`make!(B, a)` back", BASE, 1..=1),
        ("`make!(C, c)` is `make!(E, c)`, the other call of the file", C_RENAMED, 1..=1),
        ("`make!(E, c)` back", BASE, 1..=1),
        ("`make!`'s definition edited", DEFINITION, 3..=4),
    ];
    for (row, text, heads) in rows.iter() {
        write(&io, "src/lib.rs", text);
        let now = request(&io, 2, false, HeadUnits::Each);
        let fresh = request(&io, 2, true, HeadUnits::Each);
        let entered = head_tasks(&now.checked);
        println!("row {row}: head tasks entered {entered} (floor {heads:?}); moved {:?}; {}", moved_reads(&now.checked), entries(&now.checked));
        written_heads_hold(row, &now);
        assert!(same_answer(&now, &fresh), "row {row}: the incremental answer is a fresh request's:\nnow {:?}\nfresh {:?}", answer_lines(&now), answer_lines(&fresh));
        assert!(heads.contains(&entered), "row {row}: {entered} head task(s) entered, the floor is {heads:?} (the edited entries' own, and every call's under an edited definition): {}", entries(&now.checked));
    }
}

/// THE CONTROL's fixture before its edit: the call `later!()` writes a `macro_rules!` and no
/// head, so its head task froze nothing and its freeze read no cell of the call's expansion.
const LATER_BEFORE: &str = concat!(
    "macro_rules! later {\n    () => {\n        macro_rules! nothing {\n            () => {};\n        }\n    };\n}\n\n",
    "later!();\n\n",
    "pub fn read() -> u8 {\n    0\n}\n"
);
/// After: `later!`'s definition writes a fn `m` (the call's first head) and `read` calls it.
/// The call's text, so its head task's source, is the same; of everything its task read before
/// only the call's expansion record moved.
const LATER_AFTER: &str = concat!(
    "macro_rules! later {\n    () => {\n        pub fn m() -> u8 {\n            1\n        }\n    };\n}\n\n",
    "later!();\n\n",
    "pub fn read() -> u8 {\n    m()\n}\n"
);

/// One control case: (its name, the files before, after, whether the plant must go stale).
type Case = (&'static str, &'static str, &'static str, bool);

/// THE CONTROL (rule 23): THE PLANT `HeadUnits::NoExpansionReads` drops every head task's read
/// of its calls' expansion records (engine_heads.rs `heads_of`).
/// - `later!()` comes to write its first head (`LATER_*`): nothing else its head task read moves
///   (its source is the call's text; it froze no head, so its freeze read no `Item` or
///   `text_marks` cell of the call: front2 tables/view.rs `text_marks` reads
///   `NameCell::Expansion` only for a head it freezes). Under the plant the task is not entered,
///   `m`'s head is no task's, and `read` is refused where a fresh request checks it: the oracle
///   catches the stale head. With the rule the expansion read moves and the task is entered.
/// - `make!`'s definition writes a new fn under every call (`NEW_ITEM_*`; the first run's
///   fixture): the calls' head tasks already read their expansion through the heads they froze
///   (`text_marks`), so the plant goes NOT stale; printed with the kind of each moved read
///   (`moved_reads`) as the witness of which read carried it.
#[test]
fn engine_expansion_heads_plant_drops_expansion_reads() {
    let cases: [Case; 2] = [("a call's first head", LATER_BEFORE, LATER_AFTER, true), ("a new fn under calls that already write heads", NEW_ITEM_BEFORE, NEW_ITEM_AFTER, false)];
    for (case, before, after, plant_stale) in cases.iter() {
        for (units, stale) in [(HeadUnits::Each, false), (HeadUnits::NoExpansionReads, *plant_stale)] {
            let io = Io::simulated(SimWorld::new(41, Limits::USUAL));
            write(&io, "Cargo.toml", MANIFEST);
            write(&io, "src/lib.rs", before);
            let cold = request(&io, 2, true, units);
            written_heads_hold(case, &cold);
            write(&io, "src/lib.rs", after);
            let now = request(&io, 2, false, units);
            let fresh = request(&io, 2, true, units);
            println!("{case}, {units:?}: head tasks entered {}; moved {:?}; now {:?}; fresh {:?}; {}", head_tasks(&now.checked), moved_reads(&now.checked), answer_lines(&now), answer_lines(&fresh), entries(&now.checked));
            written_heads_hold(case, &fresh);
            assert_eq!(!same_answer(&now, &fresh), stale, "{case}, {units:?}: the incremental answer is stale exactly where the plant must show it:\nnow {:?}\nfresh {:?}", answer_lines(&now), answer_lines(&fresh));
            if !stale {
                written_heads_hold(case, &now);
            }
        }
    }
}

/// AN UNNAMED ENTRY IS SHOWN BY ITS HEAD (item_name.rs `shown_name`, the one home of an item's
/// shown name): an impl, an impl with an attribute and a `where` on the next line, a macro call,
/// `const _` and a `use` each print their head as written, and no entry prints `<unnamed>`.
/// CONTROL: a named fn still prints its name alone.
#[test]
fn engine_expansion_heads_unnamed_entries_show_their_heads() {
    const TEXT: &str = concat!(
        "pub trait Tr {}\npub struct A<T>(T);\n",
        "impl Tr for u8 {}\n",
        "#[doc = \"[x]\"]\n// a comment\nimpl<T> Tr for A<T>\nwhere\n    T: Copy,\n{\n}\n",
        "make!(A, a);\n",
        "const _: () = ();\n",
        "use core::fmt;\n",
        "pub fn named() {}\n"
    );
    let frame = bstd::mem::Frame::with_reserve(1 << 22, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let outline = blaze_file_items::outline(TEXT, blaze_surface::Edition::E2021, erased.handle());
    let shown: std::vec::Vec<&str> = outline.items().iter().map(|item| crate::item_name::shown_name(item, TEXT)).collect();
    println!("shown: {shown:?}");
    assert!(!shown.contains(&"<unnamed>"), "no entry is shown as `<unnamed>`: {shown:?}");
    for head in ["impl Tr for u8", "impl<T> Tr for A<T>", "const _: ()", "named"] {
        assert!(shown.contains(&head), "{head:?} is shown: {shown:?}");
    }
    assert!(shown.iter().any(|one| one.starts_with("make!")), "the call is shown by its macro: {shown:?}");
    assert!(shown.iter().any(|one| one.starts_with("use core::fmt")), "the `use` is shown by its path: {shown:?}");
}
