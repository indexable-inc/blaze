//@ expect: accept
trait Counter {
    fn bump(&mut self) -> u32;
}

struct Count(u32);

impl Counter for Count {
    fn bump(&mut self) -> u32 {
        self.0 += 1;
        self.0
    }
}

fn twice(c: &mut dyn Counter) -> u32 {
    c.bump();
    c.bump()
}

fn main() {
    let mut c = Count(0);
    assert!(twice(&mut c) == 2);
    let d: &mut dyn Counter = &mut c;
    assert!(twice(d) == 4);
}
