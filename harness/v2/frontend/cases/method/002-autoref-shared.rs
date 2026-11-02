//@ expect: accept
//@ edition: 2021
// No by-value candidate: `&` is tried before `&mut`.
trait Name {
    fn name(self) -> &'static str;
}
impl Name for &u8 {
    fn name(self) -> &'static str {
        "ref"
    }
}
impl Name for &mut u8 {
    fn name(self) -> &'static str {
        "mut"
    }
}

fn main() {
    let mut x = 1u8;
    println!("{}", x.name());
    x += 1;
    println!("{}", x);
}
