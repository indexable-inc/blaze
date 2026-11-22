// Own program x0-const-block-01 (front2; walk list-AH class 5, brief 10): `const { .. }` blocks
// in bodies, one value per instance. The Reference, "Block expressions", "Const blocks"
// ([expr.block.const]): "A const block is a variant of a block expression whose body evaluates
// at compile-time instead of at runtime." and ([expr.block.const.generic-params]): "Const
// blocks have the ability to reference generic parameters in scope, unlike free constant
// items. They are desugared to constant items with generic parameters in scope". So
// `sz::<u64>()` is 8 and `sz::<u16>()` is 2 (core's `type_name::<T>()` is `const {
// intrinsics::reflection::type_name::<T>() }`: the same form), and `k()` is 42.
//
// Status 42 when all hold; 1: the non-generic block's value is wrong; 2: one value was used for
// both instances (8 at `u16` or 2 at `u64`), or the size is wrong.
#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for usize {}
impl Copy for u32 {}
impl Copy for i32 {}

#[rustc_intrinsic]
#[rustc_nounwind]
const fn size_of<T>() -> usize;

fn sz<T>() -> usize {
    const { size_of::<T>() }
}

fn k() -> u32 {
    const { 40 + 2 }
}

fn check() -> i32 {
    match k() {
        42 => {}
        _ => return 1,
    }
    match (sz::<u64>(), sz::<u16>()) {
        (8, 2) => {}
        _ => return 2,
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    match check() {
        0 => 42,
        failed => failed,
    }
}
