//@ expect: reject E0433
//@ edition: 2021
mod a {
    pub fn f() {}
}

fn main() {
    a::f();
    a::missing::g();
}
