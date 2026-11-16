//! uniq: `#![blaze::uniq]`, `&uniq T` the unique reference, spelled for what it guarantees. A working layer accepts it.
//! expect: accept
#![blaze::uniq]

fn bump(x: &uniq i32) {
    *x = *x + 1;
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut n = 41;
    bump(&uniq n);
    n
}
