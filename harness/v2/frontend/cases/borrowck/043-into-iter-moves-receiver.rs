//@ expect: reject E0382
fn main() {
    let v = vec![1, 2];
    let it = v.into_iter();
    let n = v.len(); //~ ERROR E0382
    println!("{} {n}", it.count());
}
