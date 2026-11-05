//@ expect: accept
fn same<T: ?Sized>(x: &T) -> &T {
    let y: &T = x;
    y
}

fn first<T: Copy>(xs: &[T]) -> T {
    xs[0]
}

fn main() {
    let a = [3u32, 4];
    assert!(first(same(&a)) == 3);
    let s: &[u32] = same(&a);
    assert!(s.len() == 2);
}
