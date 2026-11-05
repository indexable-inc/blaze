//@ expect: accept
//@ edition: 2018
mod b {
    mod c {
        pub fn h() -> u8 {
            4
        }
    }
    use c::h;
    pub fn g() -> u8 {
        h()
    }
}

fn main() {
    println!("{}", b::g());
}
