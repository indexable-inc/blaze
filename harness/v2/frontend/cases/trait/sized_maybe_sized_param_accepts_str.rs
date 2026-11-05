//@ expect: accept
//@ edition: 2021
// A `?Sized` parameter takes `str`, `[T]` and `dyn` behind a reference.
use std::fmt::{Debug, Display};

fn show<T: ?Sized + Display>(t: &T) -> String {
    format!("[{t}]")
}

fn debug<T: ?Sized + Debug>(t: &T) -> String {
    format!("{t:?}")
}

fn main() {
    let d: &dyn Display = &7u8;
    println!("{} {} {} {}", show("abc"), show(d), debug(&[1u8, 2][..]), debug("q"));
}
