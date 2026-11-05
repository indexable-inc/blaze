//@ expect: reject E0277
//@ compile-flags: --test
//@ edition: 2021
//@ issue: #1522
// rustc rejects `assert_eq!` on a type that is not `Debug` inside a `#[test]` function (E0277).
// blaze let it through to codegen, which found no `fmt` for the vtable.

#[derive(PartialEq)]
pub struct Digest(pub [u8; 4]);

#[test]
fn holders() {
    let pack = Digest([1, 2, 3, 4]);
    assert_eq!(vec![pack], vec![Digest([1, 2, 3, 4])]);
}
