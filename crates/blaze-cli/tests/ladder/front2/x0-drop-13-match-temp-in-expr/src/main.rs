// Own program x0-drop-13-match-temp-in-expr (front2), wrong-answers lane, 2026-11-16: a
// `match` by value on a temporary that needs drop, inside a larger expression (the shape of
// `io.need_out()?.line(cx, "..")?`: each `?` is a match on a temporary). The Reference,
// "Destructors", [destructors.scope.temporary]: the temporary scope of an expression is the
// smallest of a list naming "A statement" and "The body expression for a match arm", and "The
// scrutinee of a `match` expression is not a temporary scope, so temporaries in the scrutinee
// can be dropped after the `match` expression"; [destructors.scope.intro]: temporaries are
// dropped "in reverse order of creation". What an arm moved out is not dropped again.
//
// `G(n)` writes the digit `n` to LOG when dropped.
// `let v = (match make(1, 2) { E::A(a, _) => 10 + a.0, .. }) + (match one(3) { E::B(_) => 30, .. });`
// - `a` (G1) is moved out by the first arm and dropped at the arm's end: 1;
// - both scrutinees live to the end of the `let` statement, after the second operand, and drop
//   last created first: B(G3) (3), then A's rest (G2; never G1 again): 132;
// - `v` is 11 + 30 = 41; then `k = G(4)` drops at the function's end: 1324.
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

struct G(i32);

impl Drop for G {
    fn drop(&mut self) {
        note(self.0);
    }
}

enum E {
    A(G, G),
    B(G),
}

fn make(a: i32, b: i32) -> E {
    E::A(G(a), G(b))
}

fn one(a: i32) -> E {
    E::B(G(a))
}

fn run() -> i32 {
    let v = (match make(1, 2) {
        E::A(a, _) => 10 + a.0,
        E::B(_) => 20,
    }) + (match one(3) {
        E::B(_) => 30,
        E::A(..) => 40,
    });
    let at_statement = log();
    let k = G(4);
    if at_statement != 132 {
        return 1;
    }
    v
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let v = run();
    let after = log();
    if v == 1 {
        1
    } else if v != 41 {
        2
    } else if after != 1324 {
        3
    } else {
        42
    }
}
