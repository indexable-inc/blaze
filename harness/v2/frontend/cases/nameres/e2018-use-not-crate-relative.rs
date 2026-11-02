//@ expect: reject E0432
//@ edition: 2018
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
