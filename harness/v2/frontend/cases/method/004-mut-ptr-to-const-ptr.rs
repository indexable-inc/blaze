//@ expect: accept
//@ edition: 2021
// A `*mut T` receiver is adjusted to `*const T` after the autorefs fail.
trait Addr {
    fn addr_of(self) -> usize;
}
impl Addr for *const u8 {
    fn addr_of(self) -> usize {
        self as usize
    }
}

fn main() {
    let mut x = 1u8;
    let p: *mut u8 = &mut x;
    println!("{}", p.addr_of() == p as usize);
}
