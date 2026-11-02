//@ expect: accept
//@ edition: 2021
// At one step the by-value candidate wins over the autoref'd one.
trait Name {
    fn name(self) -> &'static str;
}
impl Name for u8 {
    fn name(self) -> &'static str {
        "value"
    }
}
impl Name for &u8 {
    fn name(self) -> &'static str {
        "ref"
    }
}

fn main() {
    let x = 1u8;
    println!("{}", x.name());
}
