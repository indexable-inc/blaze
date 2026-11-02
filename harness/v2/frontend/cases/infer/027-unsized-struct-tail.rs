//@ expect: accept
struct Tail<T: ?Sized> {
    head: u32,
    rest: T,
}

fn count(t: &Tail<[u32]>) -> usize {
    t.head as usize + t.rest.len()
}

fn main() {
    let t = Tail { head: 10, rest: [1u32, 2] };
    let u: &Tail<[u32]> = &t;
    assert!(count(u) == 12);
    assert!(count(&t) == 12);
}
