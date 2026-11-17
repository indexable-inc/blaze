//! s4-err-03: a panic in `main` ends the process with status 101, after what was printed before.
//!
//! The pinned std's entry (library/std/src/rt.rs, `lang_start_internal`): `main` runs under
//! `panic::catch_unwind`; on a panic the payload is dropped and "Return error code for panicking
//! programs." `101`; then `cleanup()` flushes standard output. The default hook writes
//! "thread 'main' (..) panicked at src/main.rs:..:..:\ndivision by zero: 1/0" to standard error,
//! which no rung compares. Standard output holds the two lines printed before the panic.

fn divide(a: i32, b: i32) -> i32 {
    if b == 0 {
        panic!("division by zero: {a}/{b}");
    }
    a / b
}

fn main() {
    println!("{}", divide(10, 2));
    println!("about to panic");
    let v = divide(1, 0);
    println!("unreachable {v}");
}
