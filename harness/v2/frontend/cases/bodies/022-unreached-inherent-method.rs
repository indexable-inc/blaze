//@ expect: reject E0308
struct Counter(u32);

impl Counter {
    fn used(&self) -> u32 {
        self.0
    }

    fn unused(&self) -> u32 {
        'c' //~ ERROR E0308
    }
}

fn main() {
    let _ = Counter(1).used();
}
