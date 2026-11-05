//@ expect: reject E0382 E0499
fn main() {
    let s = String::from("a");
    drop(s);
    let n = s.len(); //~ ERROR E0382
    let mut v = vec![1];
    let a = &mut v;
    let b = &mut v; //~ ERROR E0499
    a.push(n);
    b.push(2);
}
