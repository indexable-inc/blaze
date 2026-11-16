//! theorem: `#[blaze::theorem]` on a function returning `bool`: a statement for every argument, proved in Lean. Here it is false for every `x`. A working layer refuses it.
//! expect: refuse

#[blaze::theorem]
fn grows(x: i32) -> bool {
    x > x
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    42
}
