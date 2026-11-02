//@ expect: reject E0597
struct Holder<'a>(Vec<&'a i32>);

impl Drop for Holder<'_> {
    fn drop(&mut self) {
        println!("{}", self.0.len());
    }
}

fn main() {
    let mut h = Holder(Vec::new());
    let x = 1;
    h.0.push(&x); //~ ERROR E0597
}
