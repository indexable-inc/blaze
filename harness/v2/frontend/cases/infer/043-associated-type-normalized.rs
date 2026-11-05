//@ expect: accept
trait Source {
    type Item;
    fn get(&self) -> Self::Item;
}

struct Numbers([u32; 2]);

impl Source for Numbers {
    type Item = [u32; 2];
    fn get(&self) -> [u32; 2] {
        self.0
    }
}

fn len_of<S: Source<Item = [u32; 2]>>(s: &S) -> usize {
    let item = s.get();
    let slice: &[u32] = &item;
    slice.len()
}

fn main() {
    assert!(len_of(&Numbers([1, 2])) == 2);
}
