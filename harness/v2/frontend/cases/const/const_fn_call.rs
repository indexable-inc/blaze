//@ expect: accept
//@ crate: bin
//@ edition: 2021
// constants that call `const fn`s: plain, generic over a type (`size_of::<T>()` inside), and one defined in a block
const fn sq(x: u32) -> u32 {
    x * x
}

const fn sz<T>() -> usize {
    std::mem::size_of::<T>()
}

const A: u32 = sq(12);
const B: usize = sz::<(u8, u32)>();
const C: i64 = {
    const fn neg(x: i64) -> i64 {
        -x
    }
    neg(5)
};
const D: u32 = sq(sq(3)) + A;

fn main() {
    println!("{} {} {} {}", A, B, C, D);
}
