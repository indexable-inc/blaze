// Own program x0-drop-14-if-let-temp (front2), if-let lane, 2026-11-16: `if let`, `while let`
// and a `let` chain on a temporary that needs drop (bstd's `Held::flush`: `if let Err(error) =
// self.out.parts(..) { self.failed = Some(error); }`). The Reference, "Destructors":
// - "Temporary scopes": the temporary scope of an expression is the smallest of a list naming
//   "The pattern-matching condition(s) and consequent body of `if`" and "The pattern-matching
//   condition and loop body of `while`"; edition 2024 ("if let temporary scope"): an `if let`'s
//   scrutinee temporaries drop before the `else` block (2021: at the statement's end). This
//   crate is edition 2024.
// - "Drop scopes": "dropped in reverse order of declaration (for variables) or creation (for
//   temporaries)": a binding made from the scrutinee drops before what is left of it.
//
// `G(n)` writes the digit `n` to LOG when dropped.
// 1. `if let E::A(x, _) = make(1, 2) { let _t = G(7); } else { note(8); }`: `_t` (7), `x` (1),
//    the rest (2): 712.
// 2. the same on `one(3)` (`B(G3)`): the temporary drops BEFORE the `else` block: 38 (the
//    2021 rule would give 83).
// 3. `while let E::A(d, _) = pick(i) { if i == 2 { break; } i = i + 1; }`, `pick(i)` is
//    `A(G(i), G(i + 5))` below 4, else `C`: each iteration drops `d` then the rest; the
//    `break` drops them once: 1627.
// 4. the same with no `break`, until `C`: 162738.
// 5. `if let E::A(x, _) = make(1, 2) && let E::B(y) = one(3) { let _t = G(7); } else {
//    note(8); }`: 7, `y` (3), `x` (1), the first's rest (2): 7312.
// 6. the second link fails (`none()`, `C`): `x` (1), the first's rest (2), then 8: 128.
// 7. the first link fails (`one(3)`): it drops whole (3), the second is never made, then 8: 38.
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
    C,
}

fn make(a: i32, b: i32) -> E {
    E::A(G(a), G(b))
}

fn one(a: i32) -> E {
    E::B(G(a))
}

fn none() -> E {
    E::C
}

fn pick(i: i32) -> E {
    if i == 4 { E::C } else { E::A(G(i), G(i + 5)) }
}

fn while_let(stop: i32) -> i32 {
    reset();
    let mut i = 1;
    while let E::A(d, _) = pick(i) {
        if i == stop {
            break;
        }
        i = i + 1;
    }
    log()
}

fn three() -> E {
    one(3)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    if if_let_temp(true) != 712 {
        1
    } else if if_let_temp(false) != 38 {
        2
    } else if while_let(2) != 1627 {
        3
    } else if while_let(0) != 162738 {
        4
    } else if chain_temp(0) != 7312 {
        5
    } else if chain_temp(1) != 128 {
        6
    } else if chain_temp(2) != 38 {
        7
    } else {
        42
    }
}

// Each scrutinee is a VALUE expression (a parenthesized `if` choosing a call), so a temporary
// of the `if let` itself; a parameter would be a place, dropped at its function's end.
fn if_let_temp(taken: bool) -> i32 {
    reset();
    if let E::A(x, _) = (if taken { make(1, 2) } else { one(3) }) {
        let _t = G(7);
    } else {
        note(8);
    }
    log()
}

fn chain_temp(fail: i32) -> i32 {
    reset();
    if let E::A(x, _) = (if fail == 2 { one(3) } else { make(1, 2) })
        && let E::B(y) = (if fail == 1 { none() } else { three() })
    {
        let _t = G(7);
    } else {
        note(8);
    }
    log()
}
