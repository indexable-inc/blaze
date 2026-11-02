//@ expect: accept
#[allow(dead_code)]
mod unused {
    pub struct Stack<T> {
        items: Vec<T>,
    }

    impl<T: Clone> Stack<T> {
        pub fn new() -> Self {
            Stack { items: Vec::new() }
        }

        pub fn push(&mut self, it: T) {
            self.items.push(it);
        }

        pub fn top(&self) -> Option<T> {
            self.items.last().cloned()
        }
    }

    pub fn exercise() -> Option<u8> {
        let mut s = Stack::new();
        s.push(1u8);
        s.top()
    }
}

fn main() {}
