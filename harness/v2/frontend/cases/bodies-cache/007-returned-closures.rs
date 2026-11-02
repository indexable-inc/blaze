//@ expect: accept
//@ edition: 2021
//@ aux-crate: closures=closures.rs
fn main() {
    let add5 = closures::adder(5);
    let f = closures::compose(add5, |x| x * 2);
    println!("{} {}", f(1), closures::apply_n(&|x| x + 3, 4, 1));
}
