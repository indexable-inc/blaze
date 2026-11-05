//@ expect: accept
//@ edition: 2021
// Copy and Clone impls on a non-generic struct, each with a global where clause that holds:
// the value is copied on use.
struct Pair(u8, u16);

impl Clone for Pair
where
    u8: Copy,
{
    fn clone(&self) -> Pair {
        *self
    }
}

impl Copy for Pair where u8: Copy {}

fn sum(p: Pair) -> u32 {
    p.0 as u32 + p.1 as u32
}

fn main() {
    let p = Pair(1, 2);
    let a = sum(p);
    let b = sum(p);
    let c = p.clone();
    println!("{} {} {}", a, b, c.1);
}
