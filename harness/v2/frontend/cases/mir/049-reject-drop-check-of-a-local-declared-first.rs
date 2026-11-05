//@ expect: reject E0597
struct Holder<'a>(&'a String);

impl Drop for Holder<'_> {
    fn drop(&mut self) {
        println!("holding {}", self.0);
    }
}

fn main() {
    let holder;
    let s = String::from("dropped first");
    holder = Holder(&s); //~ ERROR E0597
}
