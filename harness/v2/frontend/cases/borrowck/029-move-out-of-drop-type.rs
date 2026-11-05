//@ expect: reject E0509
struct D {
    s: String,
}

impl Drop for D {
    fn drop(&mut self) {}
}

fn main() {
    let d = D { s: String::from("d") };
    let s = d.s; //~ ERROR E0509
    println!("{s}");
}
