//! Facts a body MAKES that another body reads (D114 rulings, "Signatures", "FACTS A BODY MAKES",
//! (i) to (iii)): each a stale check across two files, expected RED today. These tests show the
//! gaps. They fix nothing. A child of main.rs's tests: its helpers are theirs.
//!
//! Each runs three checks of a two-file crate: as written, after an edit of `src/a.rs`, after
//! the edit is undone. The right counts are `(0, 1, 0)`: the edit makes one error in the body
//! of `src/b.rs`, the undo clears it. Today the reader in `b.rs` reads no fact of what `a.rs`'s
//! BODY made, so it is never entered again: `(0, 0, 0)` is expected, KNOWN RED.

use super::*;

/// The crate root: the two modules.
const TWO_FILES: &str = "mod a;\nmod b;\n";

/// The errors of three checks: as written, with `a.rs` edited to `edited`, with it put back.
fn three(seed: u64, before: &str, edited: &str, reader: &str) -> ((u32, u32, u32), std::string::String) {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", TWO_FILES);
    write(&io, "src/a.rs", before);
    write(&io, "src/b.rs", reader);
    let first = check(&io, work::Sizes::USUAL);
    assert!(first.failed.is_empty(), "{:?}", first.failed);
    write(&io, "src/a.rs", edited);
    let after = check(&io, work::Sizes::USUAL);
    assert!(after.failed.is_empty(), "{:?}", after.failed);
    let words = std::format!("{:?}\n{:?}\n{}", after.ids, after.words, after.profile);
    write(&io, "src/a.rs", before);
    let undone = check(&io, work::Sizes::USUAL);
    assert!(undone.failed.is_empty(), "{:?}", undone.failed);
    ((first.errors, after.errors, undone.errors), words)
}

/// (i) HIDDEN TYPES: an `async fn`'s future leaks its auto traits to its callers. The Reference,
/// "Special types and traits", `Send`: auto traits are implemented for a type when all its
/// fields are; an `async fn` returns an anonymous future type holding every local live across
/// an `.await` ("Async functions"); `alloc::rc::Rc` is `!Send` (the pinned library's
/// `library/alloc/src/rc.rs`: `impl<T: ?Sized, A: Allocator> !Send for Rc<T, A> {}`). So once
/// `f` holds an `Rc` across its `.await`, `need(f())` with `fn need<T: Send>(_: T)` is one error
/// at the call in `b.rs`, and `f`'s SIGNATURE is unchanged. KNOWN RED: right `(0, 1, 0)`. Prints
/// `stale facts, hidden Send: errors (a, b, c)`.
#[test]
fn a_future_that_stops_being_send_reaches_its_caller() {
    let before = "pub async fn f() {\n    let x = 1u8;\n    async {}.await;\n    let _ = x;\n}\n";
    let edited = "pub async fn f() {\n    let x = std::rc::Rc::new(1u8);\n    async {}.await;\n    let _ = x;\n}\n";
    let reader = "fn need<T: Send>(_: T) {}\npub fn g() {\n    need(crate::a::f());\n}\n";
    let (errors, words) = three(41, before, edited, reader);
    std::println!("stale facts, hidden Send: errors {errors:?}");
    assert_eq!(errors, (0, 1, 0), "the caller in b.rs must see f's future stop being Send:\n{words}");
}

/// (ii) CONST VALUES: a `const`'s value is read where an array length names it. The Reference,
/// "Array and array index expressions": `[operand; N]` has type `[T; N]` with `N` evaluated as a
/// constant; "Let statements": the initializer must have the annotated type. `N` made 8 gives
/// `[u8; 8]` against `[u8; 4]`: one error in `b.rs`. The edit is to `N`'s VALUE, not its type,
/// so `N`'s `Signature` (its type) is unchanged. KNOWN RED: right `(0, 1, 0)`. Prints
/// `stale facts, const value: errors (a, b, c)`.
#[test]
fn a_const_value_reaches_an_array_length() {
    let before = "pub const N: usize = 4;\n";
    let edited = "pub const N: usize = 8;\n";
    let reader = "pub fn g() {\n    let _: [u8; 4] = [0u8; crate::a::N];\n}\n";
    let (errors, words) = three(42, before, edited, reader);
    std::println!("stale facts, const value: errors {errors:?}");
    assert_eq!(errors, (0, 1, 0), "the array in b.rs must see N's new value:\n{words}");
}

/// (iii) ITEMS IN A BODY: an `impl` written inside a fn's body is an impl of the crate (the
/// Reference, "Implementations": "Implementations are not restricted to the module scope"; an
/// item in a block is in scope only there by NAME, but an impl has no name and applies
/// crate-wide). Removed by the edit, `S.m()` in `b.rs` finds no method `m` (the Reference,
/// "Method call expressions": candidates come from the impls of the receiver's type and the
/// traits in scope): one error. The edit is inside `host`'s BODY, so no `Signature` and no
/// `Surface` moves. KNOWN RED: right `(0, 1, 0)`. Prints `stale facts, impl in a body: errors (a, b, c)`.
#[test]
fn an_impl_removed_inside_a_body_reaches_its_users() {
    let before = "pub struct S;\npub trait Tr {\n    fn m(&self) {}\n}\npub fn host() {\n    impl Tr for S {}\n}\n";
    let edited = "pub struct S;\npub trait Tr {\n    fn m(&self) {}\n}\npub fn host() {}\n";
    let reader = "use crate::a::Tr;\npub fn g() {\n    crate::a::S.m();\n}\n";
    let (errors, words) = three(43, before, edited, reader);
    std::println!("stale facts, impl in a body: errors {errors:?}");
    assert_eq!(errors, (0, 1, 0), "the call in b.rs must see the impl gone:\n{words}");
}
