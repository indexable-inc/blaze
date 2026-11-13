pub mod deep;

pub struct Pair {
    pub a: u32,
    pub b: u32,
}

impl Pair {
    pub fn sum(&self) -> u32 {
        self.a + self.b
    }
}
