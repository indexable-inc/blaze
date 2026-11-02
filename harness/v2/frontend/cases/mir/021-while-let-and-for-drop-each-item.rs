//@ expect: accept
struct D(u32);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn main() {
    let mut stack = vec![D(1), D(2), D(3)];
    while let Some(top) = stack.pop() {
        println!("popped {}", top.0);
        if top.0 == 2 {
            break;
        }
    }
    println!("left {}", stack.len());
    for d in vec![D(10), D(20)] {
        println!("item {}", d.0);
    }
    for d in &stack {
        println!("borrowed {}", d.0);
    }
    println!("end");
}
