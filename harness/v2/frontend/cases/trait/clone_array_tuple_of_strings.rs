//@ expect: accept
//@ edition: 2021
// Arrays and tuples of non-Copy Clone types are Clone (not Copy); scalars, shared references
// and tuples of them are both.
fn need_copy<T: Copy>(t: T) -> (T, T) {
    (t, t)
}

fn main() {
    let a = [String::from("a"), String::from("b")];
    let b = a.clone();
    let t = (String::from("x"), 3u8, vec![1u8, 2]);
    let u = t.clone();
    let (p, q) = need_copy((1u8, 'c', &a[0], [2u16; 3]));
    println!("{} {} {} {} {}", b[1], u.0, u.2.len(), p.1, q.2);
    println!("{} {}", a[0], t.0);
}
