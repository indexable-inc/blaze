//@ expect: accept
fn main() {
    let mut v = Vec::new();
    for i in 0..4 {
        let r = &mut v;
        r.push(i);
        let s = &v;
        println!("{}", s.len());
    }
    let mut cur = 0;
    while cur < 3 {
        let r = &mut cur;
        *r += 1;
    }
    println!("{:?} {cur}", v);
}
