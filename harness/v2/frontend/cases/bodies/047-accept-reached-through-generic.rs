//@ expect: accept
trait Weight {
    fn weight(&self) -> u32;
}

impl Weight for u8 {
    fn weight(&self) -> u32 {
        u32::from(*self)
    }
}

impl Weight for &str {
    fn weight(&self) -> u32 {
        self.len() as u32
    }
}

fn total<T: Weight>(items: &[T]) -> u32 {
    items.iter().map(Weight::weight).sum()
}

fn main() {
    println!("{} {}", total(&[1u8, 2, 3]), total(&["ab", "cde"]));
}
