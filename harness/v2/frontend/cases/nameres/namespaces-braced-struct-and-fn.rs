//@ expect: accept
//@ edition: 2021
struct T {}

#[allow(non_snake_case)]
fn T() -> u8 {
    5
}

fn main() {
    let _t = T {};
    println!("{}", T());
}
