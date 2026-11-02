//@ expect: accept
//@ edition: 2021
mod a {
    pub(crate) fn f() -> u8 {
        5
    }
    pub(in crate::a) fn g() -> u8 {
        6
    }
    pub fn h() -> u8 {
        g()
    }
}

fn main() {
    println!("{} {}", a::f(), a::h());
}
