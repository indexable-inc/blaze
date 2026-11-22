// Own program x0-drop-12-let-else-temp (front2), wrong-answers lane, 2026-11-16: a `let`-`else`
// on a temporary that needs drop (bstd's `start`: `let Some(_entered) = Entered::enter(&this)
// else { return .. };`). Its drop point:
// - matched: what the pattern did not move out drops at the end of the `let` statement;
//   the bindings drop at their block's end, in reverse order of declaration
//   (the Reference, "Destructors", [destructors.scope.intro]: "dropped in reverse order of
//   declaration (for variables) or creation (for temporaries)");
// - not matched: the whole temporary drops BEFORE the `else` block runs. The Reference's copy
//   on this machine does not state it for `let`-`else`; rustc's own test
//   tests/ui/let-else/let-else-temporary-lifetime.rs does (`let Foo(0) = Foo(&mut foo) else {
//   *&mut foo = 1; todo!() };` compiles only if the temporary is gone when `else` starts).
//
// `G(n)` writes the digit `n` to LOG when dropped.
// 1. `matched`: `let E::A(x, _) = make(1, 2) else { return 0 };` G2 drops at the statement's
//    end (LOG 2 seen by the next statement); then `k = G(3)`; at the end `k`, then `x`: 231.
// 2. `missed`: `let E::B(y) = make(4, 5) else { note(7); return log() };` A(G4, G5) drops
//    before the `else` block (its fields in order: 45), then 7: the `else` block sees 457, and
//    nothing drops after (`y` was never made).
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

fn reset() {
    unsafe {
        LOG = 0;
    }
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

fn matched() -> i32 {
    let E::A(x, _) = make(1, 2) else {
        return 0;
    };
    let at_statement = log();
    let k = G(3);
    at_statement
}

fn missed() -> i32 {
    let E::B(y) = make(4, 5) else {
        note(7);
        return log();
    };
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let at_statement = matched();
    let after = log();
    reset();
    let in_else = missed();
    let after_missed = log();
    if at_statement != 2 {
        1
    } else if after != 231 {
        2
    } else if in_else != 457 {
        3
    } else if after_missed != 457 {
        4
    } else {
        42
    }
}
