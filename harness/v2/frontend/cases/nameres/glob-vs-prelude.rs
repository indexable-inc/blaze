//@ expect: accept
//@ edition: 2021
mod shadow {
    #[derive(Debug)]
    pub struct String(pub u8);
}

use shadow::*;

fn main() {
    let s = String(3);
    let real = std::string::String::from("real");
    println!("{:?} {}", s, real);
}
