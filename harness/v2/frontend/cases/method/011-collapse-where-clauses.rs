//@ expect: accept
//@ edition: 2021
// Two where-clause candidates of one trait collapse to a trait pick.
trait Conv<T> {
    fn conv(&self) -> T;
}

struct S;
impl Conv<u8> for S {
    fn conv(&self) -> u8 {
        1
    }
}
impl Conv<u16> for S {
    fn conv(&self) -> u16 {
        2
    }
}

fn both<X: Conv<u8> + Conv<u16>>(x: &X) -> (u8, u16) {
    let a: u8 = x.conv();
    let b: u16 = x.conv();
    (a, b)
}

fn main() {
    println!("{:?}", both(&S));
}
