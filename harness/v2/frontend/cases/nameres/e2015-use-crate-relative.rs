//@ expect: accept
//@ edition: 2015
mod a {
    pub fn f() -> u8 {
        1
    }
}

mod b {
    use a::f;
    pub fn g() -> u8 {
        f()
    }
}

fn main() {
    println!("{}", b::g());
}
