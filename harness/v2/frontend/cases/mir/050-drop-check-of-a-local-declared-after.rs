//@ expect: accept
struct Holder<'a>(&'a String);

impl Drop for Holder<'_> {
    fn drop(&mut self) {
        println!("holding {}", self.0);
    }
}

fn main() {
    let s = String::from("dropped last");
    let holder;
    holder = Holder(&s);
    println!("made {}", holder.0.len());
}
