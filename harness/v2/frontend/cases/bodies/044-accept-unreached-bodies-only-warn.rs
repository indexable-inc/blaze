//@ expect: accept
fn unused_panics() -> u32 {
    panic!("never called")
}

fn unused_recursion(n: u64) -> u64 {
    unused_recursion(n)
}

fn unused_loop() -> ! {
    loop {}
}

struct NeverBuilt {
    field: u32,
}

impl NeverBuilt {
    fn unused_method(&self) -> u32 {
        self.field
    }
}

fn main() {}
