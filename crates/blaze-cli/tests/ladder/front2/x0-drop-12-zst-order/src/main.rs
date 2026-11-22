// Own program x0-drop-12-zst-order (front2): the ORDER of drops of zero-sized values whose
// types have a destructor, in every position a value can be dropped from. Written by the
// wrong-answers lane (2026-11-16, WRONG-NOTES.md "Runner 3", S1) after x0-drop-11 printed
// "WRONG got status 1": whether a value needs drop is a fact of its type, never of its size.
//
// The Reference, "Destructors":
// - [destructors.intro] "When an initialized variable or temporary goes out of scope, its
//   destructor is run or it is dropped."
// - [destructors.scope.intro] "When control flow leaves a drop scope all variables associated
//   to that scope are dropped in reverse order of declaration (for variables) or creation (for
//   temporaries)."
// - [destructors.scope.temporary.enclosing]: a temporary's scope is the smallest of "The
//   entire function. A statement. The body of an if, while or loop expression. .." that
//   contains it: here the statement.
// - [destructors.operation] "The destructor of a type `T` consists of: 1. If `T: Drop`,
//   calling `<T as core::ops::Drop>::drop` 2. Recursively running the destructor of all of its
//   fields. The fields of a struct are dropped in declaration order. .. The elements of an
//   array or owned slice are dropped from the first element to the last."
// - "Patterns", wildcard: `let _ = e;` binds nothing, so a value expression `e` is a temporary
//   dropped at the end of the `let` statement.
// - [destructors.scope.bindings]: a function parameter is dropped at the end of the function
//   body: a zero-sized argument passed by value is dropped by the callee, once.
//
// `A`, `B` and `C` have no bytes; each destructor appends its digit to `LOG`.
// 1. `see(&A, &B)`: two temporaries; inside the call nothing has dropped (0); at the
//    statement's end they drop in reverse order of creation: B then A: LOG = 21.
// 2. `let _ = C;`: dropped at the end of the statement: LOG = 213.
// 3. `let a = A; let b = B; let c = C; take(c);`: `c` is moved into `take`, which drops it at
//    its end (3); at the block's end `b` then `a`: LOG = 213321.
// 4. `let w = W { a: A, n: 0 }; let arr = [B, B];`: at the block's end `arr` first (its two
//    elements, 2 and 2), then `w`'s field (1): LOG = 213321221.
// Success is status 42; a failed check returns its number.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for i32 {}

#[lang = "sync"]
unsafe trait Sync {}

#[lang = "drop"]
trait Drop {
    fn drop(&mut self);
}

#[lang = "drop_glue"]
unsafe fn drop_glue<T>(_: &mut T) {}

static mut LOG: i32 = 0;

fn note(digit: i32) {
    unsafe {
        LOG = LOG * 10 + digit;
    }
}

fn log() -> i32 {
    unsafe { LOG }
}

struct A;

impl Drop for A {
    fn drop(&mut self) {
        note(1);
    }
}

struct B;

impl Drop for B {
    fn drop(&mut self) {
        note(2);
    }
}

struct C;

impl Drop for C {
    fn drop(&mut self) {
        note(3);
    }
}

struct W {
    a: A,
    n: i32,
}

fn see(_: &A, _: &B) -> i32 {
    log()
}

fn take(_c: C) -> i32 {
    log()
}

fn locals() -> i32 {
    let _a = A;
    let _b = B;
    let c = C;
    take(c)
}

fn parts() -> i32 {
    let w = W { a: A, n: 7 };
    let _arr = [B, B];
    w.n
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let seen = see(&A, &B);
    let one = log();
    let _ = C;
    let two = log();
    let in_take = locals();
    let three = log();
    let n = parts();
    let four = log();
    if seen != 0 {
        1
    } else if one != 21 {
        2
    } else if two != 213 {
        3
    } else if in_take != 213 {
        4
    } else if three != 213321 {
        5
    } else if n != 7 {
        6
    } else if four != 213321221 {
        7
    } else {
        42
    }
}
