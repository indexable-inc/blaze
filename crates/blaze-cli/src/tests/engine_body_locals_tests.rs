//! A BODY'S OWN ITEMS ARE ITS OWN STORE'S (front2 tables/seeds.rs `of_the_body`, lane RU4): an
//! item declared in a body (a struct, its impls, a trait impl: what serde's and clap's derives
//! write inside the fns they write) has a key its body's lowering derives (front2 locals.rs:
//! `ItemKey::INNER` set) and no row in the names records, so no head task, dependency record or
//! other store can freeze its head. Asked of the request, its stretch's freezer froze it in a
//! store without the body's rows and failed `HeadError::NoText` ("the names library holds no
//! text of an item it needs", the real clap and serde projects, 2026-11-17: the key they printed,
//! `e459039bf6e7562d a088..`, has byte 8 `a0`: the inner bit `0x80` set, which blaze-scope
//! task/lower.rs `make_key` clears in every names-record key). The body's own store lowers it now.
//! A child of main.rs's tests, beside engine_check_tests.rs, whose helpers it uses.

use super::*;
use super::engine_check_tests::{engine_opts, error_lines, opts};
use blaze_ladder::front2::tables::seeds::BodyLocals;

/// A macro that writes a fn whose body declares a struct, its inherent impl and an impl of the
/// crate's trait, and calls them (the derive shape: serde's `fn deserialize` declares
/// `struct __Visitor` and `impl Visitor for __Visitor`); and a fn written by hand with the same
/// kind of body (the CONTROL that the macro is not the cause).
const LOCALS: &str = "pub trait Tr {\n    fn get(&self) -> u8;\n}\n\nmacro_rules! write_with_locals {\n    () => {\n        pub fn written() -> u8 {\n            struct V(u8);\n            impl Tr for V {\n                fn get(&self) -> u8 { self.0 }\n            }\n            impl V {\n                fn make() -> V { V(1) }\n            }\n            V::make().get()\n        }\n    };\n}\nwrite_with_locals!();\n\npub fn by_hand() -> u8 {\n    struct W(u8);\n    impl W {\n        fn get(&self) -> u8 { self.0 }\n    }\n    W(2).get()\n}\n";

/// [`LOCALS`] with the hand-written local fn returning its `u16` field as `u8`: one type error.
const LOCALS_WRONG: &str = "pub trait Tr {\n    fn get(&self) -> u8;\n}\n\nmacro_rules! write_with_locals {\n    () => {\n        pub fn written() -> u8 {\n            struct V(u8);\n            impl Tr for V {\n                fn get(&self) -> u8 { self.0 }\n            }\n            impl V {\n                fn make() -> V { V(1) }\n            }\n            V::make().get()\n        }\n    };\n}\nwrite_with_locals!();\n\npub fn by_hand() -> u8 {\n    struct W(u16);\n    impl W {\n        fn get(&self) -> u8 { self.0 }\n    }\n    W(2).get()\n}\n";

/// One fresh request on `workers` with a body's own items made `locals`' way: what it printed
/// and its counts. Its text is the existing helpers' (`stdout`, `engine_opts`).
fn run(io: &Io, workers: u32, locals: BodyLocals) -> (std::string::String, engine_check::Checked) {
    let mut opts = opts(io, workers, true);
    opts.body_locals = locals;
    let before = stdout(io).len();
    let (_, checked) = engine_opts(io, opts);
    (stdout(io).split_off(before), checked)
}

/// THE RULE: a body that declares items, written by a macro or by hand, checks with the heads of
/// its own items lowered in its own store: every body checked, no note, none of them "not
/// checked", nothing "holds no text"; the one wrong local
/// fn is the one error, as `check` prints it. 1 and 4 workers answer the same.
#[test]
fn engine_check_makes_a_bodys_own_items_in_its_own_store() {
    for (text, wrong) in [(LOCALS, false), (LOCALS_WRONG, true)] {
        for workers in [1, 4] {
            let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
            write(&io, "Cargo.toml", MANIFEST);
            write(&io, "src/lib.rs", text);
            let (said, checked) = run(&io, workers, BodyLocals::Lowered);
            let (notes, clean) = &checked.bodies;
            println!("a body's own items (wrong {wrong}, {workers} worker(s)): {clean} clean, notes {notes:?}\n{said}");
            assert!(!said.contains("holds no text") && notes.iter().all(|note| !note.contains("holds no text")), "a body's own item's head is never asked of a store without its rows (wrong {wrong}, {workers} worker(s)): notes {notes:?}\n{said}");
            // The borrow check's own note is this fixture's (its core has no `Copy` lang item:
            // "borrows not checked: a trait fact of an ADT is not known"), not a body left
            // unchecked for a head: every OTHER note would be one.
            // (Since lane SW1 the note reads "types checked; borrows not checked: ..".)
            let refused: std::vec::Vec<&std::string::String> = notes.iter().filter(|note| !note.contains("borrows not checked: ")).collect();
            assert!(refused.is_empty(), "every body checked (wrong {wrong}, {workers} worker(s)): {refused:?}");
            // What is right, not what the old path printed (it answered status 4, incomplete, for
            // the wrong variant where the engine finds the one error: status 1): the old path was
            // no oracle (rule 7) and went on 2026-11-19.
            if wrong {
                assert_eq!(checked.status.0, 1, "the wrong local fn's error fails the check (wrong {wrong}, {workers} worker(s)):\n{said}");
            }
            assert_eq!(error_lines(&said).len(), usize::from(wrong), "the wrong local fn's one error and nothing else (wrong {wrong}, {workers} worker(s)):\n{said}");
            // NOT asserted: "types checked whole". The engine prints `NOT checked: types` when any
            // body carries a note, a `borrows not checked` one included (this fixture's two: its
            // core has no `Copy` lang item), though such a body's types WERE checked: an OPEN
            // wording fault of the coverage notes (engine_check.rs `not_checked_notes`,
            // engine_lines.rs `not_checked_whole`), seen here on 2026-11-17.
        }
    }
}

/// RULE 23, THE CAUSE REMOVED: with a body's own items asked of the request again (the plant
/// `BodyLocals::AskTheRequest`, the form before lane RU4), the same clean text is NOT checked
/// whole: a body that declares items is refused with a note naming the head it could not make,
/// the real projects' failure, so the rule's clean run is the rule's doing. The note names the
/// item as what it is, never by a bare key (engine_head_consts.rs `head_label`, rule 19).
/// CONTROL on the same checkout: the rule checks it whole.
#[test]
#[ignore = "OPEN (2026-11-17): the plant does NOT reproduce the failure on this fixture: with `BodyLocals::AskTheRequest` both bodies still check (the first run printed only the fixture's two `borrows not checked` notes, 0 refused), so this fixture's body-declared items never reach the stretch's freezer the way clap's and serde's did (`the names library holds no text of an item it needs`); until a fixture reproduces it, the fix is judged by the real run of those two projects alone"]
fn engine_check_refuses_a_bodys_own_items_asked_of_the_request() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", LOCALS);
    let (said, planted) = run(&io, 2, BodyLocals::AskTheRequest);
    let (notes, clean) = &planted.bodies;
    println!("a body's own items asked of the request (the plant): {clean} clean, notes {notes:?}\n{said}");
    assert!(notes.iter().any(|note| note.contains("not checked")), "the plant takes: a body that declares items is not checked when its items are asked of the request: notes {notes:?}\n{said}");
    assert!(!said.contains("the item of key") && notes.iter().all(|note| !note.contains("the item of key")), "a head is named by what it is, never by a bare key (rule 19): notes {notes:?}\n{said}");
    let (said, kept) = run(&io, 2, BodyLocals::Lowered);
    let (notes, _) = &kept.bodies;
    assert!(notes.is_empty(), "CONTROL: the rule checks the same text whole: notes {notes:?}\n{said}");
}
