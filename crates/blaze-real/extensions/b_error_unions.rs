//! error unions: `#![blaze::error_unions]`, `A | B` in type position, an anonymous order-free enum. A working layer accepts it.
//! expect: accept
#![blaze::error_unions]

struct NotFound;
struct Denied;

fn open(code: i32) -> NotFound | Denied {
    if code == 0 { NotFound } else { Denied }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    match open(1) {
        NotFound => 0,
        Denied => 42,
    }
}
