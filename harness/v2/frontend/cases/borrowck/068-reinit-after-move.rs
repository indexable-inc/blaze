//@ expect: accept
fn main() {
    let ok = std::env::args().count() < 100;
    let mut s = String::from("a");
    if ok {
        drop(s);
        s = String::from("b");
    }
    println!("{s}");
    let mut v = vec![1];
    for i in 0..3 {
        let old = v;
        v = old.into_iter().map(|x| x + i).collect();
    }
    println!("{:?}", v);
}
