//! proved unreachable: `blaze::unreachable!()` at a site a path does reach (`x` may be 0). A working layer refuses it.
//! expect: refuse

fn sign(x: i32) -> i32 {
    if x > 0 {
        1
    } else if x < 0 {
        -1
    } else {
        blaze::unreachable!()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    sign(5) + 41
}
