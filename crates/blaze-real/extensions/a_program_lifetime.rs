//! program_lifetime: `#[blaze::program_lifetime]` on a generic type, which the fact forbids. A working layer refuses it.
//! expect: refuse

#[blaze::program_lifetime]
struct Name<T> {
    value: T,
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let n = Name { value: 42 };
    n.value
}
