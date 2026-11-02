//@ expect: accept
fn describe(v: &[i32]) -> String {
    match v {
        [] => "empty".to_string(),
        [one] => format!("one {one}"),
        [first, .., last] => format!("{first}..{last}"),
    }
}

fn main() {
    let mut v = vec![1, 2, 3, 4];
    if let [a, .., b] = v.as_mut_slice() {
        std::mem::swap(a, b);
    }
    if let [_, mid @ .., _] = v.as_mut_slice() {
        for m in mid {
            *m *= 10;
        }
    }
    println!("{:?}", v);
    println!("{} {} {}", describe(&[]), describe(&[7]), describe(&v));
    let owned = vec![String::from("x"), String::from("yz")];
    let n: usize = match owned.as_slice() {
        [a, b] => a.len() + b.len(),
        _ => 0,
    };
    println!("{n}");
}
