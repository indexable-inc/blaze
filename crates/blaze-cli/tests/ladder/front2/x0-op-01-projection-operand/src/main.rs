// Own program x0-op-01-projection-operand (front2): an operator whose operand's type is an
// associated type path, `<Table as Index<usize>>::Output`. Whether the operator is the builtin
// one is decided on the operand's type once that path is normalized (`i32`), never on the type
// as first seen. This is x0-reach-01's ORIGINAL body (`count + t[1]`; that rung was changed on
// 2026-11-16 to bind `t[1]` at `i32` first, because the checker asked for the `add` lang item
// here), with the same prelude and the same `Table`.
//
// The Reference:
// - "Associated Items", [items.associated.type.alias]: "If a type `Item` has an associated type
//   `Assoc` from a trait `Trait`, then `<Item as Trait>::Assoc` is a type that is an alias of the
//   type specified in the associated type definition." So `t[1]`, whose type is
//   `<Table as Index<usize>>::Output` ("Array and slice indexing expressions": "For other types
//   an index expression `a[b]` is equivalent to `*std::ops::Index::index(&a, b)`"), is an `i32`.
// - "Operator expressions", "Arithmetic and Logical Binary Operators",
//   [expr.arith-logic.behavior]: "This table summarizes the behavior of arithmetic and logical
//   binary operators on primitive types and which traits are used to overload these operators
//   for other types." `+` on Integer is "Addition": `count + t[1]` is `i32 + i32`.
// - "Comparison Operators", [expr.cmp.intro]: "Comparison operators are also defined both for
//   primitive types and many types in the standard library."; `==` is "Equal".
// - "Negation operators", [expr.negate.results]: `-` on a signed integer is "Negation".
//
// This crate declares no `add`, `eq` or `neg` lang trait: the three operators are the builtin
// ones on `i32`, and a checker that decided on the unnormalized type reports a missing lang
// item instead.
//
// 1. `count + t[1]` with `count = 30`: 42.
// 2. `t[1] == 12`: true.
// 3. `-t[1]`: -12.
// Success is status 42 (30 + 12 again, through the operator under test); a failed check
// returns its number.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for i32 {}

#[lang = "drop"]
trait Drop {
    fn drop(&mut self);
}

#[lang = "drop_glue"]
unsafe fn drop_glue<T>(_: &mut T) {}

#[lang = "index"]
trait Index<Idx> {
    type Output;
    fn index(&self, index: Idx) -> &Self::Output;
}

#[lang = "panic_bounds_check"]
fn panic_bounds_check(index: usize, len: usize) -> ! {
    loop {}
}

struct Table {
    cells: [i32; 3],
}

impl Index<usize> for Table {
    type Output = i32;
    fn index(&self, index: usize) -> &i32 {
        &self.cells[index]
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let t = Table { cells: [5, 12, 7] };
    let count = 30;
    if count + t[1] != 42 {
        1
    } else if t[1] == 12 {
        if -t[1] != -12 { 3 } else { count + t[1] }
    } else {
        2
    }
}
