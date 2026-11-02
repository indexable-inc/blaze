//@ expect: accept
//@ edition: 2015
mod m {
    pub fn f() -> u8 {
        9
    }
}

fn main() {
    println!("{}", ::m::f());
}
