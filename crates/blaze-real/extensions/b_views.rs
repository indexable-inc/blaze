//! views: `#![blaze::views]`, a method that borrows only some fields of `self` (`&mut self.{a}`). A working layer accepts it.
//! expect: accept
#![blaze::views]

struct Two {
    a: i32,
    b: i32,
}

impl Two {
    fn bump(&mut self.{a}) {
        self.a = self.a + 1;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut t = Two { a: 39, b: 2 };
    t.bump();
    t.a + t.b
}
