//@ expect: reject E0599
trait Describe {
    fn describe(&self) -> String;
}

struct Unit;

impl Describe for Unit {
    fn describe(&self) -> String {
        self.no_such_method() //~ ERROR E0599
    }
}

fn main() {
    let _ = Unit;
}
