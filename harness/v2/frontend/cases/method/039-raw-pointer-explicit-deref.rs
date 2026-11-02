//@ expect: accept
//@ edition: 2021
// An explicit deref of a raw pointer reaches the method.
struct S(u8);
impl S {
    fn get(&self) -> u8 {
        self.0
    }
}

fn main() {
    let s = S(4);
    let p: *const S = &s;
    println!("{} {}", unsafe { (*p).get() }, p.is_null());
}
