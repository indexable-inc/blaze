//! Facts a body MAKES that another body reads (D114 rulings, "Signatures", "FACTS A BODY MAKES",
//! (i) to (iii)): each a stale check across two files.
//!
//! Each runs three checks of a two-file crate: as written, after an edit of `src/a.rs`, after
//! the edit is undone. The right counts are `(0, 1, 0)`: the edit makes one error in the body
//! of `src/b.rs`, the undo clears it.

use super::*;

/// A body with no read of either module controls the grain of the edit.
const TWO_FILES: &str = "mod a;\nmod b;\npub fn unrelated() {}\n";

struct Revisions {
    errors: (u32, u32, u32),
    after: Checked,
    lines: std::vec::Vec<std::string::String>,
}

impl Revisions {
    fn assert_reader(&self, place: &str, code: &str) {
        assert_eq!(self.errors, (0, 1, 0), "edited findings: {:?}\n{:?}\n{}", self.lines, self.after.words, self.after.profile);
        assert!(self.lines.iter().any(|line| line.contains(place) && line.contains("error[types]")), "the error must be at the read in b.rs: {:?}", self.lines);
        assert!(self.after.words.iter().any(|words| words.split('\x1f').next() == Some(code)), "the reader must report {code}: {:?}", self.after.words);
        // D191: the crate's library and its test build each hold both bodies, a row each.
        assert_eq!(self.after.entered(blaze_engine::Stage::Bodies), 4, "the edit must remake the definition and its reader, in their 2 units' rows each (D191: the library's and its test build's). The body using only the signature must stand.\n{}", self.after.profile);
    }
}

/// The errors of three checks: as written, with `a.rs` edited to `edited`, with it put back.
fn three(seed: u64, before: &str, edited: &str, reader: &str, install: fn(&Io) -> blaze_toolchain::ToolchainId) -> Revisions {
    let world = |definition| {
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", TWO_FILES);
        write(&io, "src/a.rs", definition);
        write(&io, "src/b.rs", reader);
        let toolchain = install(&io);
        (io, toolchain)
    };
    let check = |io: &Io, toolchain: blaze_toolchain::ToolchainId, phase| {
        let on_shelf = || {
            let mut opts = engine_check_tests::opts(io, 1, false);
            opts.toolchain = toolchain;
            opts
        };
        let result = check_opts::<fix::Atomic>(io, &on_shelf, View::Person, None);
        assert_eq!(result.failures, 0, "{phase}: {:?}\n{}", result.words, result.profile);
        assert!(
            result.ids.iter().all(|(rule, _)| !matches!(rule.as_str(), "types-not-checked" | "borrows-not-checked" | "not-run")),
            "{phase}: each body must be checked: {:?}", result.words
        );
        result
    };
    let (io, toolchain) = world(before);
    let first = check(&io, toolchain, "initial");
    write(&io, "src/a.rs", edited);
    let offset = stdout(&io).len();
    let after = check(&io, toolchain, "edited");
    let lines = findings_since(&io, offset);
    let (fresh, fresh_toolchain) = world(edited);
    let cold = check(&fresh, fresh_toolchain, "fresh on edited source");
    assert_eq!(lines, findings_since(&fresh, 0), "the edited home must print the fresh home's findings (incremental {} errors, fresh {})", after.errors, cold.errors);
    write(&io, "src/a.rs", before);
    let undone = check(&io, toolchain, "undone");
    Revisions { errors: (first.errors, after.errors, undone.errors), after, lines }
}

/// (i) HIDDEN TYPES: an `async fn`'s future leaks its auto traits to its callers. The Reference,
/// "Special types and traits", `Send`: auto traits are implemented for a type when all its
/// fields are; an `async fn` returns an anonymous future type holding every local live across
/// an `.await` ("Async functions"). Core declares a negative impl of `Send` for `*const T`.
/// Holding that pointer across the await makes `need(f())` one E0277 at the call in `b.rs`
/// while `f`'s signature stays the same. Prints
/// `stale facts, hidden Send: errors (a, b, c)`.
#[test]
fn a_future_that_stops_being_send_reaches_its_caller() {
    let before = "fn consume<T>(_: T) {}\npub async fn f() {\n    let x = 1u8;\n    async {}.await;\n    consume(x);\n}\n";
    let edited = "fn consume<T>(_: T) {}\npub async fn f() {\n    let x = &1u8 as *const u8;\n    async {}.await;\n    consume(x);\n}\n";
    // Both callers need f's signature. Only g asks whether its future is Send.
    let reader = "fn need<T: core::marker::Send>(_: T) {}\npub fn g() {\n    need(crate::a::f());\n}\npub fn only_calls() { let _ = crate::a::f(); }\n";
    let revisions = three(41, before, edited, reader, |io| {
        install_shelf(io, blaze_world::fixture::shelf_with_async_langs(graph::TRIPLE, blaze_world::fixture::STD, SHELF_CFG, blaze_own::testing::heap().alloc()))
    });
    std::println!("stale facts, hidden Send: errors {:?}", revisions.errors);
    revisions.assert_reader("src/b.rs:3:", "E0277");
}

/// (ii) CONST VALUES: a `const`'s value is read where an array length names it. The Reference,
/// "Array and array index expressions": `[operand; N]` has type `[T; N]` with `N` evaluated as a
/// constant; "Let statements": the initializer must have the annotated type. `N` made 8 gives
/// `[u8; 8]` against `[u8; 4]`: one error in `b.rs`. The edit is to `N`'s VALUE, not its type,
/// so its declared type is unchanged. Prints
/// `stale facts, const value: errors (a, b, c)`.
#[test]
fn a_const_value_reaches_an_array_length() {
    let before = "pub const N: usize = 4;\n";
    let edited = "pub const N: usize = 8;\n";
    // Checking the return type of value needs N's type. Only g needs its evaluated length.
    let reader = "pub fn g() {\n    let _: [u8; 4] = [0u8; crate::a::N];\n}\npub fn value() -> usize { crate::a::N }\n";
    let revisions = three(42, before, edited, reader, lang_shelf);
    std::println!("stale facts, const value: errors {:?}", revisions.errors);
    revisions.assert_reader("src/b.rs:2:", "E0308");
}

/// (iii) ITEMS IN A BODY: an `impl` written inside a fn's body is an impl of the crate (the
/// Reference, "Implementations": "Implementations are not restricted to the module scope"; an
/// item in a block is in scope only there by NAME, but an impl has no name and applies
/// crate-wide). Removed by the edit, `S.m()` in `b.rs` finds no method `m` (the Reference,
/// "Method call expressions": candidates come from the impls of the receiver's type and the
/// traits in scope): one error. The edit is inside `host`'s BODY, so no `Signature` and no
/// `Surface` moves. Prints `stale facts, impl in a body: errors (a, b, c)`.
#[test]
fn an_impl_removed_inside_a_body_reaches_its_users() {
    let before = "pub struct S;\npub trait Tr {\n    fn m(&self) {}\n}\npub fn host() {\n    impl Tr for S {}\n}\n";
    let edited = "pub struct S;\npub trait Tr {\n    fn m(&self) {}\n}\npub fn host() {}\n";
    let reader = "use crate::a::Tr;\npub fn g() {\n    crate::a::S.m();\n}\n";
    let revisions = three(43, before, edited, reader, lang_shelf);
    std::println!("stale facts, impl in a body: errors {:?}", revisions.errors);
    assert_eq!(revisions.errors, (0, 1, 0), "the call in b.rs must see the impl gone: {:?}", revisions.lines);
}
