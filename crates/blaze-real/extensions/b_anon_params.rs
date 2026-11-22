//! anonymous parameters: `#![feature(blaze_anon_params)]`, a `_` type argument in a parameter type is a generic parameter of its own. A working layer accepts it.
//! expect: accept
#![feature(blaze_anon_params)]

struct Tagged<T, U> {
    value: T,
    tag: U,
}

fn value_of(t: &Tagged<i32, _>) -> i32 {
    t.value
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    value_of(&Tagged { value: 42, tag: 0u8 })
}
