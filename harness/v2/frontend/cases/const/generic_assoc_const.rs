//@ expect: accept
//@ crate: bin
//@ edition: 2021
// generic associated constants: `<T as Tr>::N` in a generic fn, in an array length, and through a generic impl
trait Tr {
    const N: usize;
}

impl Tr for u8 {
    const N: usize = 1;
}

impl Tr for u32 {
    const N: usize = 4 * <u8 as Tr>::N;
}

struct Wrap<T>(T);

impl<T: Tr> Tr for Wrap<T> {
    const N: usize = T::N * 2 + 1;
}

fn n<T: Tr>() -> usize {
    <T as Tr>::N
}

const ARR: [u8; <u32 as Tr>::N] = [0; <u32 as Tr>::N];

fn main() {
    println!("{} {} {} {}", n::<u8>(), n::<u32>(), <Wrap<Wrap<u32>> as Tr>::N, ARR.len());
}
