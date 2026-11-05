//@ expect: reject E0499
struct Pool<T> {
    items: Vec<T>,
}

impl<T> Pool<T> {
    fn never_called(&mut self) {
        let a = &mut self.items;
        let b = &mut self.items; //~ ERROR E0499
        a.clear();
        b.clear();
    }
}

fn main() {
    let _ = Pool::<u8> { items: Vec::new() }.items.len();
}
