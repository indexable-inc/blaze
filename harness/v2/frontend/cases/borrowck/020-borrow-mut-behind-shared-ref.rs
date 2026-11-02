//@ expect: reject E0596
fn add(v: &Vec<i32>) {
    v.push(1); //~ ERROR E0596
}

fn main() {
    let v = vec![0];
    add(&v);
    println!("{}", v.len());
}
