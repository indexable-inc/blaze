//@ expect: reject E0433
//@ edition: 2018
mod m {
    pub fn f() -> u8 {
        9
    }
}

fn main() {
    println!("{}", ::m::f());
}
