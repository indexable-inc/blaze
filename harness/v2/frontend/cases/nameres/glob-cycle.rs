//@ expect: accept
//@ edition: 2021
mod a {
    pub use super::b::*;
    pub fn fa() -> u8 {
        1
    }
}

mod b {
    pub use super::a::*;
    pub fn fb() -> u8 {
        2
    }
}

fn main() {
    println!("{} {}", a::fb(), b::fa());
}
