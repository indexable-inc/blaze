//@ expect: accept
struct Cell {
    value: u32,
}

impl Cell {
    fn get(&self) -> u32 {
        self.value
    }

    fn set(&mut self, value: u32) {
        self.value = value;
    }
}

fn main() {
    let mut c = Cell { value: 1 };
    c.set(c.get() + 1);
    c.set(c.get() * 5);
    assert!(c.get() == 10);
}
