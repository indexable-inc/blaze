//! records: `#![blaze::records]`, a structural record type with a field default, a record value and named arguments. A working layer accepts it.
//! expect: accept
#![blaze::records]

fn open(opts: { create: bool = false, mode: i32 }) -> i32 {
    if opts.create { opts.mode } else { 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    open({ create: true, mode: 42 })
}
