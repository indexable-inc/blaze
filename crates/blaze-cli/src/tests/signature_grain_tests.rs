//! A body's check reads the HEADS it uses, at signature grain (D114, drive.md "Check times: the
//! end state in numbers", rule 4 and piece (d)). A child of main.rs's tests: its helpers are
//! theirs.
//!
//! First, the stale check (correctness): a signature edited in ANOTHER file or crate that leaves
//! the crates' exports equal must still reach every body that calls it. Until a body's read list
//! names each head it used, a body's key held its own item's text and its file's text around
//! bodies, and its reads were the crates' `CrateScope` outs (exports digests): `f(x: u8)` made
//! `f(x: u16)` in `a.rs` moved none of them for a caller in `b.rs`, and the caller's old
//! "checked" record stood (body_types.rs's header, gap (a)).
//!
//! The rule the expected error rests on (the Rust Reference, "Call expressions": "the arguments
//! [..] are coerced to the parameter types"; "Type coercions": no coercion site turns `u8` into
//! `u16`, numeric widening is an `as` cast only): `f(1u8)` against `fn f(_x: u16)` is one type
//! error at the call.

use super::*;

/// Bodies a macro wrote at module level, checked by the call's own `BodyCheck` (lane I).
#[path = "expansion_bodies_tests.rs"]
mod expansion_bodies_tests;

/// The library root of a crate whose two modules are one file each: the callee's and the
/// caller's.
const TWO_FILES: &str = "mod a;\nmod b;\n";
/// The callee as first written, then with its one parameter's type edited.
const CALLEE_U8: &str = "pub fn f(_x: u8) {}\n";
const CALLEE_U16: &str = "pub fn f(_x: u16) {}\n";
/// The caller: a `u8` literal by its suffix, so no inference can make it a `u16`.
const CALLER: &str = "pub fn g() {\n    crate::a::f(1u8);\n}\n";

/// The errors of three checks of one workspace: as written, after the callee's edit, after the
/// edit is undone. Right: `(0, 1, 0)`.
struct ThreeChecks {
    errors: (u32, u32, u32),
    /// The edited check's findings, (code, id) and words: what a failing assertion quotes.
    after: std::string::String,
}

/// Checks the workspace as written, edits the file `callee` to `CALLEE_U16` and checks, puts
/// `CALLEE_U8` back and checks.
fn three_checks(io: &Io, callee: &str, sizes: work::Sizes) -> ThreeChecks {
    let first = check(io, sizes);
    assert!(first.failed.is_empty(), "{:?}", first.failed);
    write(io, callee, CALLEE_U16);
    let edited = check(io, sizes);
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    // On one line: a runner quotes a failure's first lines.
    let rows = edited.profile.lines().filter(|l| !l.starts_with("store") && !l.starts_with("gate")).collect::<std::vec::Vec<_>>().join(" / ");
    let after = std::format!("{:?} {:?} {rows}", edited.ids, edited.words);
    write(io, callee, CALLEE_U8);
    let undone = check(io, sizes);
    assert!(undone.failed.is_empty(), "{:?}", undone.failed);
    ThreeChecks { errors: (first.errors, edited.errors, undone.errors), after }
}

/// The two-file workspace, written into a fresh simulated home.
fn two_files() -> Io {
    let io = Io::simulated(SimWorld::new(21, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", TWO_FILES);
    write(&io, "src/a.rs", CALLEE_U8);
    write(&io, "src/b.rs", CALLER);
    io
}

/// Two members, `a` (the callee) and `b` (the caller, depending on `a` by path).
fn two_crates() -> Io {
    let io = Io::simulated(SimWorld::new(22, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "a/src/lib.rs", CALLEE_U8);
    write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    write(&io, "b/src/lib.rs", "pub fn g() {\n    a::f(1u8);\n}\n");
    io
}

/// THE STALE CHECK, across files. `fn f(_x: u8)` in `a.rs`, its caller `g` in `b.rs`; `f` made
/// to take `u16`: the second check reports the caller's `types` error, and the undo clears it.
/// Before the fix the counts are `(0, 0, 0)`: `b.rs`'s body key (its item's text and its file
/// around bodies) and its reads (the crate's `CrateScope`, whose out is the exports' digest,
/// which a parameter's type leaves equal) all stand. Prints
/// `stale check, two files: errors (0, 1, 0)`.
#[test]
fn a_signature_edit_in_another_file_reaches_its_caller() {
    let io = two_files();
    let seen = three_checks(&io, "src/a.rs", work::Sizes::USUAL);
    assert_eq!(seen.errors, (0, 1, 0), "the caller in b.rs must see f's new parameter type:\n{}", seen.after);
    assert!(seen.after.contains("\"types\""), "the error is the caller's type error:\n{}", seen.after);
    std::println!("stale check, two files: errors {:?}", seen.errors);
}

/// THE STALE CHECK, across crates: the same edit in member `a`, the caller in member `b`.
/// Before the fix `(0, 0, 0)`, for the same reason (`a`'s `CrateScope` out is its exports'
/// digest). Prints `stale check, two crates: errors (0, 1, 0)`.
#[test]
fn a_signature_edit_in_another_crate_reaches_its_caller() {
    let io = two_crates();
    let seen = three_checks(&io, "a/src/lib.rs", work::Sizes::USUAL);
    assert_eq!(seen.errors, (0, 1, 0), "the caller in member b must see a::f's new parameter type:\n{}", seen.after);
    assert!(seen.after.contains("\"types\""), "the error is the caller's type error:\n{}", seen.after);
    std::println!("stale check, two crates: errors {:?}", seen.errors);
}

/// THE PLANTED BUG (work::Plant::HeadReadLeftOut): each body drops its first head read, so `g`
/// does not list `f`'s `Signature`. The stale check must then fail: the counts are NOT
/// `(0, 1, 0)`. This test passes when the plant is caught. Prints
/// `planted head read left out: errors (0, 0, 0), not (0, 1, 0)`.
#[test]
fn a_head_read_left_out_is_caught() {
    let io = two_files();
    let planted = work::Sizes { plant: work::Plant::HeadReadLeftOut, ..work::Sizes::USUAL };
    let seen = three_checks(&io, "src/a.rs", planted);
    assert_ne!(seen.errors, (0, 1, 0), "a read list missing the callee's head still saw the edit: the stale check does not test the head reads:\n{}", seen.after);
    std::println!("planted head read left out: errors {:?}, not (0, 1, 0)", seen.errors);
}

/// `f` written by a `macro_rules!` call at module level (signature.rs, "A HEAD A MACRO WROTE"):
/// its head has no file entry; the call's entry `write_f!(u8);` has the `Signature` that covers
/// it. Then with the call's argument edited, which edits the head the expansion writes.
const MACRO_CALLEE_U8: &str = "macro_rules! write_f {\n    ($t:ty) => {\n        pub fn f(_x: $t) {}\n    };\n}\nwrite_f!(u8);\n";
const MACRO_CALLEE_U16: &str = "macro_rules! write_f {\n    ($t:ty) => {\n        pub fn f(_x: $t) {}\n    };\n}\nwrite_f!(u16);\n";

/// THE STALE CHECK for a head a macro of the workspace wrote (lane R8, 2026-11-17: hello
/// world's blaze-edition `main`, which `bstd::main` takes, stopped `blaze run` with "a head it
/// reads was written by a macro of a workspace crate, and no task makes its `Signature` yet").
/// `f` in `a.rs` is written by `write_f!(u8);`, its caller `g` in `b.rs`:
/// - the first check is clean: no error, and no note that a macro-written head is unread;
/// - `write_f!(u16);` reports the caller's `types` error, and the undo clears it: `(0, 1, 0)`;
/// - the edit makes the call entry's `Signature` (1) and two bodies: its one reader's (`g`) and
///   the call entry's own check, which covers the body `f` the call writes (its key is the
///   call's text: expansion_bodies_tests.rs); no other `Signature` or body of the workspace.
/// CONTROL: before the call's entry had a `Signature` (`is_signature_entry` without
/// `MacroCall`), the first check filed the `HEAD_IN_EXPANSION` note for `g` and the edit moved
/// no read of it: `(0, 0, 0)`. Prints
/// `macro-written head: errors (0, 1, 0); call edit (Signature, BodyCheck) made (1, 2)`.
#[test]
fn a_head_a_workspace_macro_wrote_is_read_by_its_call() {
    let io = Io::simulated(SimWorld::new(41, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", TWO_FILES);
    write(&io, "src/a.rs", MACRO_CALLEE_U8);
    write(&io, "src/b.rs", CALLER);
    let first = check(&io, work::Sizes::USUAL);
    std::println!("first: failed {:?}; errors {}; words {:?}", first.failed, first.errors, first.words);
    assert!(first.failed.is_empty(), "{:?}", first.failed);
    assert_eq!(first.errors, 0, "the workspace as written is clean: {:?}", first.words);
    assert!(
        !first.words.iter().any(|w| w.contains("written by a macro") || w.contains("no task makes")),
        "a body read a head a macro wrote and did not name it: {:?}",
        first.words
    );
    write(&io, "src/a.rs", MACRO_CALLEE_U16);
    let edited = check(&io, work::Sizes::USUAL);
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    let made_counts = (made(&edited.profile, "Signature"), made(&edited.profile, "BodyCheck"));
    let after = std::format!("{:?}\n{:?}\n{}", edited.ids, edited.words, edited.profile);
    std::println!("edited: failed {:?}; errors {}; made {made_counts:?}; words {:?}", edited.failed, edited.errors, edited.words);
    write(&io, "src/a.rs", MACRO_CALLEE_U8);
    let undone = check(&io, work::Sizes::USUAL);
    assert!(undone.failed.is_empty(), "{:?}", undone.failed);
    let errors = (first.errors, edited.errors, undone.errors);
    assert_eq!(errors, (0, 1, 0), "the caller in b.rs must see the macro-written f's new parameter type:\n{after}");
    assert!(after.contains("\"types\""), "the error is the caller's type error:\n{after}");
    assert_eq!(made_counts, (1, 2), "the call's edit makes its entry's head, the one body that read it and the call's own check:\n{after}");
    std::println!("macro-written head: errors {errors:?}; call edit (Signature, BodyCheck) made {made_counts:?}");
}

/// One head edit across two files, seen whole: the two-file workspace with `a.rs` as `first`
/// and `b.rs` as `caller`, checked; `a.rs` made `edited` and checked; `first` put back and
/// checked. The first check's words, the three error counts, the edited check's made counts
/// (`Signature`, `BodyCheck`), and its findings and profile for a failing assertion.
struct EditSeen {
    first_words: std::string::String,
    errors: (u32, u32, u32),
    made: (u32, u32),
    after: std::string::String,
}

fn edit_seen(seed: u64, first: &str, edited: &str, caller: &str) -> EditSeen {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", TWO_FILES);
    write(&io, "src/a.rs", first);
    write(&io, "src/b.rs", caller);
    let one = check(&io, work::Sizes::USUAL);
    // Printed before any assertion: a failing test's output is what its reader sees first.
    std::println!("first: failed {:?}; errors {}; words {:?}", one.failed, one.errors, one.words);
    assert!(one.failed.is_empty(), "{:?}\n{}", one.failed, one.profile);
    write(&io, "src/a.rs", edited);
    let two = check(&io, work::Sizes::USUAL);
    let rows = two.profile.lines().filter(|l| l.starts_with("Signature") || l.starts_with("BodyCheck")).collect::<std::vec::Vec<_>>().join(" / ");
    std::println!("edited: failed {:?}; errors {}; words {:?}; {rows}", two.failed, two.errors, two.words);
    assert!(two.failed.is_empty(), "{:?}\n{}", two.failed, two.profile);
    let made_counts = (made(&two.profile, "Signature"), made(&two.profile, "BodyCheck"));
    let after = std::format!("{:?}\n{:?}\n{}", two.ids, two.words, two.profile);
    write(&io, "src/a.rs", first);
    let three = check(&io, work::Sizes::USUAL);
    std::println!("undone: failed {:?}; errors {}", three.failed, three.errors);
    assert!(three.failed.is_empty(), "{:?}\n{}", three.failed, three.profile);
    EditSeen { first_words: std::format!("{:?}", one.words), errors: (one.errors, two.errors, three.errors), made: made_counts, after }
}

/// One check of a run of [`checks_of`]: its errors, its words, what it made (`Signature`,
/// `BodyCheck`) and its findings and profile for a failing assertion.
struct Seen {
    errors: u32,
    words: std::string::String,
    made: (u32, u32),
    all: std::string::String,
}

/// The two-file workspace checked once a state, each `(a.rs, b.rs)` in turn on one store; each
/// check's [`Seen`], printed before any assertion.
fn checks_of(seed: u64, states: &[(&str, &str)]) -> std::vec::Vec<Seen> {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", TWO_FILES);
    let mut seen = std::vec::Vec::new();
    for (i, (a, b)) in states.iter().enumerate() {
        write(&io, "src/a.rs", a);
        write(&io, "src/b.rs", b);
        let got = check(&io, work::Sizes::USUAL);
        let made_counts = (made(&got.profile, "Signature"), made(&got.profile, "BodyCheck"));
        std::println!("check {i}: failed {:?}; errors {}; made (Signature, BodyCheck) {made_counts:?}; words {:?}", got.failed, got.errors, got.words);
        assert!(got.failed.is_empty(), "check {i}: {:?}\n{}", got.failed, got.profile);
        let all = std::format!("{:?}\n{:?}\n{}", got.ids, got.words, got.profile);
        seen.push(Seen { errors: got.errors, words: std::format!("{:?}", got.words), made: made_counts, all });
    }
    seen
}

/// Whether a check's words hold a refusal of a text the records saw changed under them
/// (blaze-scope items_record.rs `Stale`).
fn stale(words: &str) -> bool {
    words.contains("Stale {")
}

/// The impl the item-grain tests edit: `f` and its sibling `g` in `impl S`, in `a.rs`.
const IMPL_FG: &str = "pub struct S;\nimpl S {\n    pub fn f(_x: u8) {}\n    pub fn g(&self) {}\n}\n";
/// Its caller in `b.rs`: reads `S::f`'s head.
const CALLS_F: &str = "pub fn h() {\n    crate::a::S::f(1u8);\n}\n";
/// A caller of a free `f` in `b.rs`.
const CALLS_FREE_F: &str = "pub fn h() {\n    crate::a::f(1u8);\n}\n";

/// A MEMBER OF A CHANGED FILE is read through its owner's row of today (blaze-ladder front2
/// item_grain.rs `member_now`): `f`'s body edited inside `impl S` leaves every other item
/// readable, so the check after it has no `Stale` note, makes no `Signature` (a body is no
/// head) and one `BodyCheck`, `f`'s. CONTROL: with a member read through its file's digest
/// (the member has no item row, so `item_now` answered `None` and the whole file's refusal was
/// kept), every owner's front end over records was refused: "the items record of crate `wide`:
/// Stale { file: Some(1) .. }" for `f`, and nothing in the crate was checked (2026-11-17 04:40).
/// Prints `impl body edit: made (0, 1)`.
#[test]
fn a_body_edit_inside_an_impl_checks_that_body_alone() {
    const EDITED: &str = "pub struct S;\nimpl S {\n    pub fn f(_x: u8) {\n        let _y = 1u8;\n    }\n    pub fn g(&self) {}\n}\n";
    let seen = checks_of(82, &[(IMPL_FG, CALLS_F), (EDITED, CALLS_F)]);
    assert_eq!(seen[0].errors, 0, "{}", seen[0].all);
    assert!(!stale(&seen[1].words), "a body edit inside an impl refused its file: {}", seen[1].all);
    assert_eq!(seen[1].errors, 0, "{}", seen[1].all);
    assert_eq!(seen[1].made, (0, 1), "a body edit makes no head and that one body:\n{}", seen[1].all);
    std::println!("impl body edit: made {:?}", seen[1].made);
}

/// AN IMPL FN'S HEAD EDIT moves its `Signature` and its readers alone: `f`'s parameter made
/// `u16` makes `f`'s `Signature`, `f`'s body and its caller `h` in `b.rs` (which now reports its
/// `u8` argument), and not `f`'s sibling `g`, whose read list holds no head of `f`. Prints
/// `impl head edit: errors 1; made (1, 2)`.
#[test]
fn a_head_edit_inside_an_impl_moves_its_readers_alone() {
    const EDITED: &str = "pub struct S;\nimpl S {\n    pub fn f(_x: u16) {}\n    pub fn g(&self) {}\n}\n";
    let seen = checks_of(83, &[(IMPL_FG, CALLS_F), (EDITED, CALLS_F), (IMPL_FG, CALLS_F)]);
    assert!(!stale(&seen[1].words), "{}", seen[1].all);
    assert_eq!((seen[0].errors, seen[1].errors, seen[2].errors), (0, 1, 0), "h must see f's new parameter type:\n{}", seen[1].all);
    assert_eq!(seen[1].made, (1, 2), "f's head, f's body and its caller, not its sibling g:\n{}", seen[1].all);
    std::println!("impl head edit: errors {}; made {:?}", seen[1].errors, seen[1].made);
}

/// A MEMBER ADDED TO AN IMPL: `pub fn k() {}` added to `impl S`, then called from `b.rs`. The
/// new member is read at once (no `Stale` note, no error). COARSE: the file's `Surface` moved,
/// so its four heads' `Signature`s are made again by their keys, besides `k`'s (5); the impl's
/// out holds its member list (`Want::Impl`), so the bodies that read the impl, `f`, `g` and
/// `h`, are checked again with `k`'s (4). Then calling `k` makes `h` alone, `(0, 1)`. Prints
/// `member added: made (5, 4); call added: made (0, 1)`.
#[test]
fn a_member_added_to_an_impl_is_read_at_once() {
    const ADDED: &str = "pub struct S;\nimpl S {\n    pub fn f(_x: u8) {}\n    pub fn g(&self) {}\n    pub fn k() {}\n}\n";
    const CALLS_K: &str = "pub fn h() {\n    crate::a::S::f(1u8);\n    crate::a::S::k();\n}\n";
    let seen = checks_of(84, &[(IMPL_FG, CALLS_F), (ADDED, CALLS_F), (ADDED, CALLS_K)]);
    for (i, s) in seen.iter().enumerate() {
        assert!(!stale(&s.words), "check {i}: {}", s.all);
        assert_eq!(s.errors, 0, "check {i}: {}", s.all);
    }
    assert_eq!(seen[1].made, (5, 4), "adding k makes a.rs's heads by their keys, k's body and the impl's three readers:\n{}", seen[1].all);
    assert_eq!(seen[2].made, (0, 1), "calling k makes h alone:\n{}", seen[2].all);
    std::println!("member added: made {:?}; call added: made {:?}", seen[1].made, seen[2].made);
}

/// A MEMBER REMOVED FROM AN IMPL: `f` removed while `h` in `b.rs` still calls it. `h` reports
/// the real error (no fn `f` on `S`) and the check answers (the drive refused no record as two
/// values under one key: `TwoValues`); putting `f` back clears it. Prints `member removed:
/// errors (0, 1, 0)`.
///
/// IGNORED until the drive keys a `BodyCheck` by what its read NAMES follow from: a removal
/// changes which heads `h` names while `h`'s key (its text, its file's owners, the unit graph:
/// blaze-drive key.rs `owned_key`) stands, so the drive refuses the new record as two values
/// under one key, `TwoValues { field: "reads" }`. A free fn removed the same way is refused
/// alike (control, 2026-11-17 04:43: "held [Signature 270707.., Signature 8bc33c..], put []"):
/// the cause is the key, not item grain.
#[test]
#[ignore = "blaze-drive: a removal changes a BodyCheck's read names under one key (TwoValues), free fns alike"]
fn a_member_removed_from_an_impl_is_its_callers_error() {
    let free = checks_of(86, &[("pub fn f(_x: u8) {}\npub fn g() {}\n", CALLS_FREE_F), ("pub fn g() {}\n", CALLS_FREE_F), ("pub fn f(_x: u8) {}\npub fn g() {}\n", CALLS_FREE_F)]);
    assert_eq!((free[0].errors, free[1].errors, free[2].errors), (0, 1, 0), "control: h must report the missing free f:\n{}", free[1].all);
    const REMOVED: &str = "pub struct S;\nimpl S {\n    pub fn g(&self) {}\n}\n";
    let seen = checks_of(85, &[(IMPL_FG, CALLS_F), (REMOVED, CALLS_F), (IMPL_FG, CALLS_F)]);
    assert!(!stale(&seen[1].words), "{}", seen[1].all);
    assert_eq!((seen[0].errors, seen[1].errors, seen[2].errors), (0, 1, 0), "h must report the missing f:\n{}", seen[1].all);
    std::println!("member removed: errors ({}, {}, {})", seen[0].errors, seen[1].errors, seen[2].errors);
}

/// Whether a check's words hold a head no `Signature` covered (heads.rs `HEAD_NOT_NAMED`,
/// `HEAD_IN_EXPANSION`).
fn unnamed(words: &str) -> bool {
    words.contains("could not be named as a `Signature`") || words.contains("at no entry a `Signature` covers")
}

/// A FN OF AN IMPL is a `Signature` entry of its own (signature.rs `is_signature_entry`): `S::f`
/// in `a.rs`, its caller `g` in `b.rs`. The first check is clean with no note of an unnamed
/// head; `f`'s parameter made `u16` reports `g`'s type error and the undo clears it, `(0, 1, 0)`;
/// the edit makes `f`'s `Signature` (1) and two bodies, `f`'s own and `g`. CONTROL: with the
/// module-level rule asked first (every fn a head of its own in modules only), no impl fn had a
/// `Signature`: the first check filed "a head it reads could not be named" for `f` and `g`
/// (tempo, 2026-11-17 03:55: 1,767 such notes) and the edit moved no read: `(0, 0, 0)`. Prints
/// `impl fn head: errors (0, 1, 0); parameter edit (Signature, BodyCheck) made (1, 2)`.
#[test]
fn an_impl_fn_head_is_read_by_its_callers() {
    const FIRST: &str = "pub struct S;\nimpl S {\n    pub fn f(&self, _x: u8) {}\n}\n";
    const EDITED: &str = "pub struct S;\nimpl S {\n    pub fn f(&self, _x: u16) {}\n}\n";
    let seen = edit_seen(71, FIRST, EDITED, "pub fn g() {\n    crate::a::S.f(1u8);\n}\n");
    assert!(!unnamed(&seen.first_words), "a head of an impl fn was not named: {}", seen.first_words);
    assert_eq!(seen.errors, (0, 1, 0), "the caller in b.rs must see S::f's new parameter type:\n{}", seen.after);
    assert_eq!(seen.made, (1, 2), "the edit makes f's head, f's body and its one reader:\n{}", seen.after);
    std::println!("impl fn head: errors {:?}; parameter edit (Signature, BodyCheck) made {:?}", seen.errors, seen.made);
}

/// A FN A MACRO CALL INSIDE AN IMPL BLOCK WROTE (`impl S { write_f!(u8); }`, the shape of tempo's
/// precompile and storage macros) is covered by its impl's `Signature` (signature.rs
/// `covering_entry`; heads.rs `member_writer`, `make_expanded`), whose out holds the call's
/// expansion and not its text. Its caller `g` in `b.rs`: the first check is clean with no note
/// of an unnamed head; `write_f!(u16);` reports `g`'s type error and the undo clears it,
/// `(0, 1, 0)`; the edit makes `a.rs`'s two `Signature`s (a call's tokens are in its file's
/// `Surface`, which every head's key holds: `S`'s out stands, the impl's moves) and `g` among
/// the bodies. CONTROL: with the call's members found by `filed_member` (a member's key only
/// once its impl's head was made in that world, which no `Signature` task's world had), the
/// impl's out held no expansion and the edit moved no read: `(0, 0, 0)` (2026-11-17 04:39).
/// Prints `impl-level macro head: errors (0, 1, 0); call edit (Signature, BodyCheck) made
/// (2, B)`.
#[test]
fn a_fn_an_impl_level_macro_wrote_is_read_by_its_impl() {
    const MACRO: &str = "pub struct S;\nmacro_rules! write_f {\n    ($t:ty) => {\n        pub fn f(&self, _x: $t) {}\n    };\n}\n";
    let first = std::format!("{MACRO}impl S {{\n    write_f!(u8);\n}}\n");
    let edited = std::format!("{MACRO}impl S {{\n    write_f!(u16);\n}}\n");
    let seen = edit_seen(72, &first, &edited, "pub fn g() {\n    crate::a::S.f(1u8);\n}\n");
    assert!(!unnamed(&seen.first_words), "a fn an impl-level call wrote was not named: {}", seen.first_words);
    assert_eq!(seen.errors, (0, 1, 0), "the caller in b.rs must see the macro-written S::f's new parameter type:\n{}", seen.after);
    assert_eq!(seen.made.0, 2, "the call's edit makes a.rs's two heads by their keys (S's and the impl's):\n{}", seen.after);
    assert!((1..=2).contains(&seen.made.1), "the call's edit makes its one reader (and the call's own check, if any):\n{}", seen.after);
    std::println!("impl-level macro head: errors {:?}; call edit (Signature, BodyCheck) made {:?}", seen.errors, seen.made);
}

/// A FOREIGN FN is covered by its `extern` block's `Signature` (signature.rs `covering_entry`: an
/// `extern` block in modules is an entry, its whole text its heads, its out each foreign item's
/// want): `f` in `a.rs`'s `unsafe extern "C"` block, its caller `g` in `b.rs`. The first check is
/// clean; `f`'s parameter made `u16` reports `g`'s type error and the undo clears it,
/// `(0, 1, 0)`; the edit makes the block's `Signature` (1) and `g` (a foreign fn has no body).
/// CONTROL: before, "a head written inside a body or in an extern block .. has no `Signature`",
/// `(0, 0, 0)`. Prints `foreign fn head: errors (0, 1, 0); parameter edit (Signature, BodyCheck)
/// made (1, 1)`.
#[test]
fn a_foreign_fn_head_is_read_by_its_extern_block() {
    const FIRST: &str = "unsafe extern \"C\" {\n    pub fn f(_x: u8);\n}\n";
    const EDITED: &str = "unsafe extern \"C\" {\n    pub fn f(_x: u16);\n}\n";
    let seen = edit_seen(73, FIRST, EDITED, "pub fn g() {\n    unsafe { crate::a::f(1u8) };\n}\n");
    assert!(!unnamed(&seen.first_words), "a foreign fn's head was not named: {}", seen.first_words);
    assert_eq!(seen.errors, (0, 1, 0), "the caller in b.rs must see the foreign f's new parameter type:\n{}", seen.after);
    assert_eq!(seen.made, (1, 1), "the edit makes the block's head and its one reader:\n{}", seen.after);
    std::println!("foreign fn head: errors {:?}; parameter edit (Signature, BodyCheck) made {:?}", seen.errors, seen.made);
}

/// AN ITEM DECLARED IN A BODY needs no `Signature` (heads.rs `local_fact`, `where_is`): a struct
/// declared in `f`'s body and used there is named only inside it, so `f`'s own text covers it.
/// The check is clean, with no note of an unnamed head. Prints `body-local struct: clean`.
#[test]
fn a_struct_declared_in_a_body_needs_no_signature() {
    let first = fresh_two_files(74, "pub fn f() -> u8 {\n    struct L {\n        x: u8,\n    }\n    let l = L { x: 1 };\n    l.x\n}\n", "pub fn g() {\n    let _y = crate::a::f();\n}\n");
    let words = std::format!("{:?}", first.words);
    assert_eq!(first.errors, 0, "{words}");
    assert!(!unnamed(&words), "a body-local struct was read as a head no `Signature` covers: {words}");
    std::println!("body-local struct: clean");
}

/// The count after `word` of `kind`'s profile row (blaze-drive profile.rs's `Display`); a row
/// or word the profile does not have is a broken test, never a 0.
fn count(profile: &str, kind: &str, word: &str) -> u32 {
    let row = profile.lines().find(|line| line.split_whitespace().next() == Some(kind));
    let words: std::vec::Vec<&str> = row.unwrap_or_else(|| panic!("no `{kind}` row:\n{profile}")).split_whitespace().collect();
    let at = words.iter().position(|w| *w == word).unwrap_or_else(|| panic!("no `{word}` in the `{kind}` row:\n{profile}"));
    words.get(at + 1).and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no count after `{word}` in the `{kind}` row:\n{profile}"))
}

/// The two-file workspace with `caller` as `b.rs`, checked once fresh (seed `seed`): the check.
fn fresh_two_files(seed: u64, callee: &str, caller: &str) -> Checked {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", TWO_FILES);
    write(&io, "src/a.rs", callee);
    write(&io, "src/b.rs", caller);
    let first = check(&io, work::Sizes::USUAL);
    assert!(first.failed.is_empty(), "{:?}\n{}", first.failed, first.profile);
    first
}

/// A BODY READS A HEAD AT ITS TASK'S OUT (body_types/heads.rs `HeadReads`). `g` in `b.rs` calls
/// `f` of `a.rs`: its first entry names `f`'s `Signature`, which it was not handed, so it
/// answers a need of it (at `a.rs`'s `FileItems`), is entered again with it handed, and settles
/// with the read at that out: the drive refuses any other (`DriveError::ReadNotHanded`, which
/// fails the check). Then:
/// - THE BOUND: no body is entered more than 5 times (graph and owners; the owners' crate
///   scopes; rarely the `no_core` scope; the missing `Signature`s and its file's `Surface`, in
///   one need; settle): the `BodyCheck` entries are at most 5 a body made, and no job of the
///   check took more than 4 (the histogram's 5-and-over buckets are empty);
/// - the `Signature` costs no entry of its own: `g` calling nothing of `a.rs` is entered as
///   often (its `Surface` need is the one its `Signature` need rides with);
/// - the repeat stands on its certificate with no run: the read list is the one the record
///   holds (before, the body read an out it made, which differed from the task's: tempo's
///   bodies were entered 64 times).
/// Prints `handed head: BodyCheck entries E for B bodies (needs N), the same without the call`.
#[test]
fn a_body_reading_another_files_head_needs_its_signature_once_and_settles() {
    let io = Io::simulated(SimWorld::new(51, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", TWO_FILES);
    write(&io, "src/a.rs", CALLEE_U8);
    write(&io, "src/b.rs", CALLER);
    let reading = check(&io, work::Sizes::USUAL);
    let rows = |profile: &str| profile.lines().filter(|l| l.starts_with("BodyCheck") || l.starts_with("Signature") || l.starts_with("schedule")).collect::<std::vec::Vec<_>>().join(" / ");
    assert!(reading.failed.is_empty() && reading.errors == 0, "{:?} {:?} {}", reading.failed, reading.words, rows(&reading.profile));
    let repeat = check(&io, work::Sizes::USUAL);
    assert!(repeat.certified && repeat.runs == 0, "the repeat ran {} tasks: {}", repeat.runs, rows(&repeat.profile));
    let alone = fresh_two_files(51, CALLEE_U8, "pub fn g() {\n    let _y = 1u8;\n}\n");
    assert_eq!(alone.errors, 0, "{:?} {}", alone.words, rows(&alone.profile));
    let (entries, bodies, needs) = (count(&reading.profile, "BodyCheck", "entries"), count(&reading.profile, "BodyCheck", "made"), count(&reading.profile, "BodyCheck", "needs"));
    assert!(bodies == 2 && entries <= 5 * bodies, "a body settles in at most 5 entries: {}", rows(&reading.profile));
    let hist = reading.profile.lines().find(|line| line.starts_with("schedule")).and_then(|line| line.split("more ").nth(1)).unwrap_or("");
    assert!(hist.ends_with(", 0, 0, 0]"), "no job took more than 4 entries: {}", rows(&reading.profile));
    assert_eq!(entries, count(&alone.profile, "BodyCheck", "entries"), "the `Signature` need is an entry more: {} --- {}", rows(&reading.profile), rows(&alone.profile));
    std::println!("handed head: BodyCheck entries {entries} for {bodies} bodies (needs {needs}), the same without the call");
}

/// A HEAD WHOSE `Signature` CANNOT BE MADE is read as that task's record (its out the tokens
/// alone, with its `TYPES_NOT_CHECKED` finding), never left out, so the reader's read list is
/// the same whatever order the tasks ran in: two homes on the same files with different
/// simulated orders (seeds) list the same findings, and each repeat stands on its certificate
/// with no run (a read list that varied with the order would miss it, rule 21). `f`'s parameter
/// names a type that does not exist. Prints `failed head: findings equal over two orders (N)`.
#[test]
fn a_head_whose_signature_fails_is_read_alike_in_any_order() {
    const BROKEN: &str = "pub fn f(_x: Missing) {}\n";
    let mut seen = std::vec::Vec::new();
    for seed in [61, 62] {
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", TWO_FILES);
        write(&io, "src/a.rs", BROKEN);
        write(&io, "src/b.rs", CALLER);
        let first = check(&io, work::Sizes::USUAL);
        assert!(first.failed.is_empty(), "{:?}\n{}", first.failed, first.profile);
        let repeat = check(&io, work::Sizes::USUAL);
        assert!(repeat.certified && repeat.runs == 0, "seed {seed}: the repeat ran {} tasks:\n{}", repeat.runs, repeat.profile);
        seen.push((first.errors, first.ids, first.words));
    }
    assert_eq!(seen[0], seen[1], "two orders listed different findings");
    std::println!("failed head: findings equal over two orders ({})", seen[0].1.len());
}

/// The made counts of one check after `edit` is written to `src/a.rs` of the two-file
/// workspace, checked once before: (`Signature`, `BodyCheck`).
fn made_after(seed: u64, edit: &str) -> (u32, u32) {
    made_after_rows(seed, edit).0
}

/// [`made_after`], with the edited check's profile rows a reader of a failure needs.
fn made_after_rows(seed: u64, edit: &str) -> ((u32, u32), std::string::String) {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", TWO_FILES);
    write(&io, "src/a.rs", CALLEE_U8);
    write(&io, "src/b.rs", CALLER);
    let first = check(&io, work::Sizes::USUAL);
    assert!(first.failed.is_empty(), "{:?}", first.failed);
    write(&io, "src/a.rs", edit);
    let edited = check(&io, work::Sizes::USUAL);
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    let rows = edited.profile.lines().filter(|l| !l.starts_with("store") && !l.starts_with("gate")).collect::<std::vec::Vec<_>>().join(" / ");
    ((made(&edited.profile, "Signature"), made(&edited.profile, "BodyCheck")), rows)
}

/// THE GRAIN, by count, in the two-file workspace (`f` in `a.rs`, its one caller `g` in
/// `b.rs`):
/// - `f`'s parameter type edited: `f`'s `Signature` made again (1), and two bodies, `f`'s own
///   (its text is its key) and its caller `g` (its read of `f`'s `Signature` moved): `(1, 2)`;
/// - `f`'s body edited: no `Signature` (a body is no head), `f`'s body alone: `(0, 1)`;
/// - `f`'s signature respaced, or a comment written inside it: its `Signature` is made again
///   (its source is its bytes) with an EQUAL out (head records without the fn's text slot,
///   head_record.rs `reader_digest`, and tokens without trivia), so `g` stands and only `f`'s
///   own body runs: `(1, 1)` each. CONTROL: with the text slot in the out (`encode`'s bytes
///   digested), each is `(1, 2)`.
/// Before D114 the parameter edit made `(0, 1)` (the caller never ran: the stale check), and a
/// reformat in `b.rs` would have re-checked every body of that file. Prints
/// `signature counts: parameter edit (Signature, BodyCheck) made (1, 2); body edit (0, 1); reformat (1, 1)`.
#[test]
fn a_signature_edit_makes_its_head_and_its_readers_only() {
    let parameter = made_after(31, CALLEE_U16);
    let body = made_after(32, "pub fn f(_x: u8) {\n    let _y = 1;\n}\n");
    let (reformat, rows) = made_after_rows(33, "pub fn f( _x : u8 ) {}\n");
    let (commented, comment_rows) = made_after_rows(34, "pub fn f(\n    // the byte\n    _x: u8\n) {}\n");
    assert_eq!(parameter, (1, 2), "a parameter edit makes its head and the bodies that read it");
    assert_eq!(body, (0, 1), "a body edit makes no head and that one body");
    assert_eq!(reformat, (1, 1), "a respaced head is made again with an equal out: its caller stands: {rows}");
    assert_eq!(commented, (1, 1), "a comment in a head is made again with an equal out: its caller stands: {comment_rows}");
    std::println!("signature counts: parameter edit (Signature, BodyCheck) made {parameter:?}; body edit {body:?}; respace {reformat:?}; comment {commented:?}");
}
