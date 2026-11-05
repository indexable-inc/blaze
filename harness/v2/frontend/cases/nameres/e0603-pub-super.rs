//@ expect: reject E0603
//@ edition: 2021
mod a {
    pub mod b {
        pub(super) fn f() {}
    }
    pub fn g() {
        b::f();
    }
}

fn main() {
    a::g();
    a::b::f();
}
