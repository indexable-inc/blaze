//@ expect: accept
fn main() {
    let a = 1;
    let mut v: Vec<&i32> = Vec::new();
    v.push(&a);
    let mut w: Vec<&i32> = Vec::new();
    let b = 2;
    w.push(&b);
    w.push(&a);
    let s = String::from("s");
    let mut names: Vec<&str> = Vec::new();
    names.push(&s);
    println!("{:?} {:?} {:?}", v, w, names);
}
