//@ expect: accept
//@ crate: bin
//@ edition: 2021
// an allowed overflow in a fn that is never called: the program builds and runs to the end
#[allow(arithmetic_overflow)]
fn never() -> u8 {
    255u8 + 1
}

fn main() {
    if std::env::args().count() > 100 {
        println!("{}", never());
    }
    println!("not run");
}
