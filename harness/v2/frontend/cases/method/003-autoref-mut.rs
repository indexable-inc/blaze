//@ expect: accept
//@ edition: 2021
// Only `&mut` applies: the receiver is borrowed mutably.
trait Bump {
    fn bump(self) -> u8;
}
impl Bump for &mut u8 {
    fn bump(self) -> u8 {
        *self += 1;
        *self
    }
}

fn main() {
    let mut x = 1u8;
    println!("{}", x.bump());
    println!("{}", x);
}
