//@ expect: accept
//@ edition: 2021
// A struct whose last field is a `?Sized` parameter, unsized to `[u32]` through `Box`.
struct Tail<T: ?Sized> {
    n: u8,
    tail: T,
}

fn total(t: &Tail<[u32]>) -> u32 {
    t.n as u32 + t.tail.iter().sum::<u32>()
}

fn main() {
    let b: Box<Tail<[u32]>> = Box::new(Tail { n: 3, tail: [1, 2, 3] });
    let r: &Tail<[u32]> = &Tail { n: 1, tail: [10, 20] };
    println!("{} {} {} {}", b.n, b.tail.len(), total(&b), total(r));
}
