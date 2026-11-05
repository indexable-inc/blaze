//@ expect: accept
//@ edition: 2021
// `<F as FnOnce<()>>::Output` for a fn item returning an opaque normalizes to that opaque,
// whose hidden `u32` makes the `F::Output: Send` bound hold.
use std::fmt::Display;

fn mk() -> impl Display {
    7u32
}

fn run<F: FnOnce()>(f: F) -> F::Output
where
    F::Output: Send,
{
    f()
}

fn main() {
    let r = run(mk);
    println!("{r}");
}
