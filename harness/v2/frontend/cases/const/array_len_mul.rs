//@ expect: accept
//@ crate: bin
//@ edition: 2021
// array lengths computed from constants: `[u8; N * 2]` in a constant's type, a field, a parameter and `size_of`
const N: usize = 3;
const ARR: [u8; N * 2] = [7; N * 2];
const LEN: usize = ARR.len();

struct Buf {
    data: [u16; N + N * N],
}

fn first(a: [i32; N << 1]) -> i32 {
    a[0]
}

fn main() {
    let b = Buf { data: [1; 12] };
    println!("{} {} {} {}", LEN, b.data.len(), first([5; 6]), std::mem::size_of::<[u32; N * 2 - 1]>());
}
