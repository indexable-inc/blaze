//@ expect: accept
#![feature(explicit_tail_calls)]
#![expect(incomplete_features)]

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn count(n: u64, acc: D) -> u64 {
    let _local = D("local");
    if n == 0 {
        println!("done with {}", acc.0);
        return 0;
    }
    become count(n - 1, D(if n % 2 == 0 { "even" } else { "odd" }));
}

fn sum(n: u64, acc: u64) -> u64 {
    if n == 0 { acc } else { become sum(n - 1, acc + n) }
}

fn via_pointer(n: u64, acc: u64) -> u64 {
    let f: fn(u64, u64) -> u64 = sum;
    let _outer = D("outer");
    become f(n, acc);
}

fn main() {
    println!("{}", count(3, D("start")));
    println!("{}", sum(1_000_000, 0));
    println!("{}", via_pointer(10, 0));
}
