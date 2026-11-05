//@ expect: accept
//@ edition: 2021
mod a {
    pub mod inner {
        pub const X: u8 = 7;
        pub mod deeper {
            pub const Y: u8 = 8;
        }
        pub use self::deeper::*;
    }
    pub use self::inner::*;
}

use a::*;

fn main() {
    println!("{} {}", X, Y);
}
