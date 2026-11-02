//@ expect: accept
//@ crate: bin
//@ edition: 2021
// constants computed by loops: `while` in a const fn (20!), `while` in a block, `loop` with `break value`
const fn fact(n: u64) -> u64 {
    let mut r = 1;
    let mut i = 2;
    while i <= n {
        r *= i;
        i += 1;
    }
    r
}

const F20: u64 = fact(20);
const SUM: u32 = {
    let mut s = 0;
    let mut i = 0;
    while i < 10 {
        s += i;
        i += 1;
    }
    s
};
const FIRST_POW: u32 = {
    let mut p = 1;
    loop {
        if p > 1000 {
            break p;
        }
        p *= 3;
    }
};

fn main() {
    println!("{} {} {}", F20, SUM, FIRST_POW);
}
