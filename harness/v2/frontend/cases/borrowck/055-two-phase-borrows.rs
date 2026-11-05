//@ expect: accept
fn main() {
    let mut v = vec![1usize];
    v.push(v.len());
    let r = &mut v;
    r.push(r.len());
    let mut w: Vec<usize> = Vec::new();
    w.insert(0, v.len() + w.len());
    w.push(w[0] * 2);
    println!("{:?} {:?}", v, w);
}
