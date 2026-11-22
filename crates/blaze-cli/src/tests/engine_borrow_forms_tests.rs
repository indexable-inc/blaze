//! THE BORROW FORMS OF THE DERIVES (piece RB7, NEVER COMPILED when written, 2026-11-17): the
//! bodies clap's, serde's and thiserror's derives write, in miniature, through the engine's
//! check, on the seam the real projects take. A subpattern that peels a reference under an ADT
//! field (clap's `Some(Cmd::Hello { who })` against `Option<&Cmd>`) is lowered now, not refused
//! `PeelUnderAdtField`; a call of a head that writes lifetimes and could not be related (a
//! projection of a trait with a lifetime, `V::Value` of `Visitor<'de>`) is still refused, but its
//! note says which callee and why (rule 19), never the bare "the flow lowering refused a form".
//! Each miniature has a negative with one injected borrow error, which must be found with its
//! code (rule 23: the check ran on that body). The checks run on a toolchain whose core
//! declares `copy`, `drop` and `owned_box` (blaze-world `fixture::CORE_WITH_LANGS`, piece RB8);
//! one control keeps the plain core and shows the missing `copy` is still a refusal by name.
//! A child of main.rs's tests, beside engine_check_tests.rs, whose helpers it uses.

use super::*;
use super::engine_check_tests::{engine_opts, error_lines, opts};

/// clap's `Cmd` match, the derive's `(a, Self::V { x })` against `(&u8, &Self)`, and a variant
/// inside a wrapper's field.
const CLAP: &str = "pub enum Cmd {\n    Hello { who: u8 },\n    Quit,\n}\n\npub enum Opt<T> {\n    No,\n    Yes(T),\n}\n\npub fn run(c: Opt<&Cmd>) -> u8 {\n    match c {\n        Opt::Yes(Cmd::Hello { who }) => *who,\n        _ => 0,\n    }\n}\n\npub enum Two {\n    V { x: u8 },\n    W,\n}\n\nimpl Two {\n    pub fn pick(a: &u8, b: &Two) -> u8 {\n        match (a, b) {\n            (a, Self::V { x }) => { let _ = a; *x }\n            _ => 0,\n        }\n    }\n}\n\npub struct Wrapper {\n    pub inner: Two,\n}\n\npub fn unwrap(w: &Wrapper) -> u8 {\n    match w {\n        Wrapper { inner: Two::V { x } } => *x,\n        _ => 0,\n    }\n}\n";

/// [`CLAP`]'s `run` over `Opt<&mut Cmd>` with one injected error: `who: &mut u8` (the peel of
/// `&mut` makes the mode `ref mut`), `r` reborrows `*who`, and `*who = 2` writes it while `r`
/// is used after (E0506).
const CLAP_WRONG: &str = "pub enum Cmd {\n    Hello { who: u8 },\n    Quit,\n}\n\npub enum Opt<T> {\n    No,\n    Yes(T),\n}\n\npub fn keep(_: &u8) {}\n\npub fn run(c: Opt<&mut Cmd>) -> u8 {\n    match c {\n        Opt::Yes(Cmd::Hello { who }) => { let r: &u8 = &*who; *who = 2; keep(r); 0 }\n        _ => 0,\n    }\n}\n";

/// serde's `Deserialize` in miniature: a `Visitor<'de>` with a `visit_seq`-like fn, a
/// `Deserializer<'de>` whose `deserialize_seq` returns `V::Value`, and the derive's
/// `deserialize` calling it through the caller's `D: Deserializer<'de>`.
const SERDE: &str = "pub trait Visitor<'de> {\n    type Value;\n    fn visit_seq(self, n: &'de [u8]) -> Self::Value;\n}\n\npub trait Deserializer<'de> {\n    fn deserialize_seq<V: Visitor<'de>>(self, v: V) -> V::Value;\n}\n\npub struct PointVisitor;\n\nimpl<'de> Visitor<'de> for PointVisitor {\n    type Value = u8;\n    fn visit_seq(self, n: &'de [u8]) -> u8 {\n        let _ = n;\n        0\n    }\n}\n\npub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> u8 {\n    d.deserialize_seq(PointVisitor)\n}\n";

/// [`SERDE`] with one injected error in the visitor's `visit_seq`: a shared borrow while a
/// mutable one is live (E0502).
const SERDE_WRONG: &str = "pub trait Visitor<'de> {\n    type Value;\n    fn visit_seq(self, n: &'de [u8]) -> Self::Value;\n}\n\npub fn keep(_: &u8) {}\n\npub struct PointVisitor;\n\nimpl<'de> Visitor<'de> for PointVisitor {\n    type Value = u8;\n    fn visit_seq(self, n: &'de [u8]) -> u8 {\n        let _ = n;\n        let mut v = 1u8;\n        let a = &mut v;\n        let b = &v;\n        keep(b);\n        *a = 2;\n        0\n    }\n}\n";

/// thiserror's `Display` in miniature: `fmt(&self, f: &mut Fmt<'_>)` writing through a helper
/// whose head writes lifetimes, and an `Arguments<'_>` builder.
const FAULT: &str = "pub struct Fmt<'a> {\n    pub buf: &'a mut u8,\n}\n\npub trait Show {\n    fn fmt(&self, f: &mut Fmt<'_>) -> u8;\n}\n\npub fn write_code(f: &mut Fmt<'_>, c: &u8) -> u8 {\n    *f.buf = *c;\n    0\n}\n\npub struct Fault {\n    code: u8,\n}\n\nimpl Show for Fault {\n    fn fmt(&self, f: &mut Fmt<'_>) -> u8 {\n        write_code(f, &self.code)\n    }\n}\n\npub struct Arguments<'a> {\n    pub parts: &'a [u8],\n    pub arg: &'a u8,\n}\n\nimpl<'a> Arguments<'a> {\n    pub fn new(parts: &'a [u8], arg: &'a u8) -> Arguments<'a> {\n        Arguments { parts, arg }\n    }\n}\n\npub fn build<'a>(p: &'a [u8], a: &'a u8) -> Arguments<'a> {\n    Arguments::new(p, a)\n}\n";

/// [`FAULT`]'s `Fmt` with one injected error: a shared borrow of `*c` while `f` holds it
/// mutably and is used after (E0502).
const FMT_WRONG: &str = "pub struct Fmt<'a> {\n    pub buf: &'a mut u8,\n}\n\npub fn write_code(f: &mut Fmt<'_>, c: &u8) -> u8 {\n    *f.buf = *c;\n    0\n}\n\npub fn bad(c: &mut u8) -> u8 {\n    let mut f = Fmt { buf: &mut *c };\n    let r: &u8 = &*c;\n    write_code(&mut f, r)\n}\n";

/// [`FAULT`]'s `Arguments` with one injected error: an assignment to `x` while the built
/// `Arguments` holds it and is used after (E0506).
const ARGS_WRONG: &str = "pub struct Arguments<'a> {\n    pub parts: &'a [u8],\n    pub arg: &'a u8,\n}\n\nimpl<'a> Arguments<'a> {\n    pub fn new(parts: &'a [u8], arg: &'a u8) -> Arguments<'a> {\n        Arguments { parts, arg }\n    }\n}\n\npub fn keep(_: &u8) {}\n\npub fn bad(p: &[u8]) -> u8 {\n    let mut x = 1u8;\n    let a = Arguments::new(p, &x);\n    x = 2;\n    keep(a.arg);\n    x\n}\n";

/// blaze-world's fixture shelf whose core declares the borrow check's lang items
/// (`fixture::CORE_WITH_LANGS`: `copy`, `drop`, `owned_box`), written on the home `/h` beside
/// the plain one (main.rs `shelf`), in `alloc`: its id.
fn shelf_with_langs(io: &Io, alloc: blaze_own::Alloc<'_>) -> blaze_toolchain::ToolchainId {
    let (id, files) = blaze_world::fixture::shelf_with_langs(graph::TRIPLE, blaze_world::fixture::STD, SHELF_CFG, alloc);
    for (name, bytes) in files {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let path = Path::new(&format!("h/{}/{name}", toolchain::SHELF)).expect("a path");
        io.need_fs().expect("files").root().publish(&cx, &path, Content::bytes(&bytes), Durable::No).expect("write");
    }
    id
}

/// Which core the check's toolchain has.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Core {
    /// `fixture::CORE_WITH_LANGS`: bodies naming an ADT are borrow-checked.
    Langs,
    /// `fixture::CORE`: no `Copy` lang item, so such a body's borrows are not checked (the
    /// refusal [`engine_check_without_the_copy_lang_item_checks_no_borrows`] keeps).
    Plain,
}

/// One check of `text` on 2 workers: what it printed, the codes it listed, and its notes.
fn check_text(text: &str) -> (std::string::String, std::vec::Vec<std::string::String>, std::vec::Vec<std::string::String>) {
    check_text_on(text, Core::Langs)
}

/// [`check_text`] on the toolchain whose core is `core`.
fn check_text_on(text: &str, core: Core) -> (std::string::String, std::vec::Vec<std::string::String>, std::vec::Vec<std::string::String>) {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", text);
    let before = stdout(&io).len();
    let mut o = opts(&io, 2, true);
    if core == Core::Langs {
        o.toolchain = shelf_with_langs(&io, o.heap.alloc());
    }
    let (ids, checked) = engine_opts(&io, o);
    let said = stdout(&io).split_off(before);
    let codes = ids.into_iter().map(|(code, _)| code).collect();
    let (notes, _) = &checked.bodies;
    (said, codes, notes.iter().map(|n| n.to_string()).collect())
}

/// Each note of a body whose borrows were not checked says why, by name: never the bare
/// lowering refusal of a call or a peel, and a call refused for its head names the callee's
/// head and what it lacked, with the shape's or the alignment's reason (rule 19).
fn notes_say_why(case: &str, notes: &[std::string::String], said: &str) {
    for note in notes {
        assert!(!note.contains("the flow lowering refused a form"), "{case}: a form the lowering refused without words: {note}\n{said}");
        if note.contains("CallUnrelated {") || note.contains("the call could not be related") {
            let reason = note.split_once("borrows not checked: [")
                .and_then(|(_, rest)| rest.split_once("] the call could not be related through the callee's signature"))
                .map(|(reason, _)| reason);
            assert!(reason.is_some_and(|r| r.starts_with("Shape/") || r.starts_with("Unaligned/") || r == "Bounds"), "{case}: the call's reason leads its note: {note}\n{said}");
            let worded = note.contains("shape_why: Some(") || (note.contains("Unaligned(") && !note.contains("Unaligned(NotTried)")) || note.contains("lacks: Bounds");
            assert!(worded, "{case}: a call refused without its reason: {note}\n{said}");
            assert!(note.contains("head: \"") || note.contains("fn "), "{case}: the callee is named by its head: {note}\n{said}");
        }
    }
}

/// THE RULE (A): clap's peels under a field lower: no note of a refused form, no error.
#[test]
fn engine_check_lowers_a_peel_under_an_adt_field() {
    let (said, codes, notes) = check_text(CLAP);
    println!("clap miniature: codes {codes:?}, notes {notes:?}\n{said}");
    notes_say_why("clap", &notes, &said);
    assert!(notes.iter().all(|n| !n.contains("PeelUnderAdtField")), "the peel is lowered: {notes:?}\n{said}");
    assert!(error_lines(&said).is_empty(), "clean text, no error: {said}");
}

/// RULE 23 for (A): one injected borrow error under the peel is found: E0506. Why that code,
/// from the program: `c: Opt<&mut Cmd>` matched by `Opt::Yes(Cmd::Hello { who })` meets `&mut
/// Cmd` with a non-reference pattern, so the default binding mode becomes `ref mut` (the
/// Reference, "Binding modes") and `who: &mut u8` borrows `(*c.0).who`; `r = &*who` is a
/// shared loan of `*who`, live until `keep(r)`; `*who = 2` between them writes a place that
/// loan covers: "cannot assign to `*who` because it is borrowed", E0506 (not E0502: the
/// conflicting access is an assignment, not a new borrow).
#[test]
fn engine_check_finds_a_borrow_error_under_a_peel() {
    let (said, codes, notes) = check_text(CLAP_WRONG);
    println!("clap miniature, wrong: codes {codes:?}, notes {notes:?}\n{said}");
    notes_say_why("clap wrong", &notes, &said);
    // At this seam a borrow finding's id is its rule, `borrows` (first run, 2026-11-18: `codes
    // ["borrows"]`); the E-code named above is which finding the flow check makes of it.
    assert!(codes.iter().any(|c| c == "borrows"), "the assignment to the borrowed field is found: {codes:?}\n{said}");
}

/// (B) serde's `Deserialize`: every body's note says why; the visitor's body is checked. A call
/// of `deserialize_seq` through `D: Deserializer<'de>` may stay refused (its `V::Value` is a
/// projection of a trait with a lifetime, whose `'de` is the caller's: see the lane's report),
/// but then by name and reason.
#[test]
fn engine_check_words_a_deserialize_call() {
    let (said, codes, notes) = check_text(SERDE);
    println!("serde miniature: codes {codes:?}, notes {notes:?}\n{said}");
    notes_say_why("serde", &notes, &said);
    assert!(error_lines(&said).is_empty(), "clean text, no error: {said}");
}

/// RULE 23 for (B): the visitor's injected borrow error is found: E0502. From the program: `a =
/// &mut v` is a mutable loan of `v` live until `*a = 2`; `b = &v` between them is a NEW shared
/// borrow of the same place: "cannot borrow `v` as immutable because it is also borrowed as
/// mutable", E0502. The body names `PointVisitor` (`self`), an ADT, so before the fixture core
/// declared `copy` it was not checked at all.
#[test]
fn engine_check_finds_a_borrow_error_in_a_visitor() {
    let (said, codes, notes) = check_text(SERDE_WRONG);
    println!("serde miniature, wrong: codes {codes:?}, notes {notes:?}\n{said}");
    notes_say_why("serde wrong", &notes, &said);
    assert!(codes.iter().any(|c| c == "borrows"), "the visitor's injected borrow error is found: {codes:?}\n{said}");
}

/// (C) thiserror's `Display` and an `Arguments<'_>` builder: every note says why, the clean text
/// has no error, and each negative's one injected error is found with its code.
#[test]
fn engine_check_lowers_display_and_arguments() {
    let (said, codes, notes) = check_text(FAULT);
    println!("thiserror miniature: codes {codes:?}, notes {notes:?}\n{said}");
    notes_say_why("thiserror", &notes, &said);
    assert!(error_lines(&said).is_empty(), "clean text, no error: {said}");
}

/// RULE 23 for (C): each negative's one injected error is found with its code. From the
/// programs: in [`FMT_WRONG`], `f = Fmt { buf: &mut *c }` holds a mutable loan of `*c` in `f`,
/// live until `write_code(&mut f, r)`; `r = &*c` before that is a new shared borrow of `*c`:
/// E0502. In [`ARGS_WRONG`], `a = Arguments::new(p, &x)` puts the loan of `x` into `a` (the
/// head's `arg: &'a u8` and output `Arguments<'a>` share `'a`), live until `keep(a.arg)`; `x =
/// 2` between them assigns the borrowed place: E0506.
#[test]
fn engine_check_finds_the_borrow_errors_in_display_and_arguments() {
    for (case, text, code) in [("fmt wrong", FMT_WRONG, "E0502"), ("arguments wrong", ARGS_WRONG, "E0506")] {
        let (said, codes, notes) = check_text(text);
        println!("{case}: codes {codes:?}, notes {notes:?}\n{said}");
        notes_say_why(case, &notes, &said);
        assert!(codes.iter().any(|c| c == "borrows"), "{case}: the injected error ({code}) is found as a `borrows` finding: {codes:?}\n{said}");
    }
}

/// CONTROL (rule 23) for the lang items: the same negative under the peel on the plain core,
/// which declares no `copy`, is NOT borrow-checked: its note says `NoLangItem` (a refusal by
/// name, never a guess that an ADT is not `Copy`) and E0506 is not listed. So the E0506 of
/// [`engine_check_finds_a_borrow_error_under_a_peel`] comes from the lang item.
#[test]
fn engine_check_without_the_copy_lang_item_checks_no_borrows() {
    let (said, codes, notes) = check_text_on(CLAP_WRONG, Core::Plain);
    println!("clap miniature, wrong, plain core: codes {codes:?}, notes {notes:?}\n{said}");
    assert!(notes.iter().any(|n| n.contains("NoLangItem")), "the missing lang item is named: {notes:?}\n{said}");
    assert!(codes.iter().all(|c| c != "borrows"), "no borrow check ran on `run`: {codes:?}\n{said}");
}
