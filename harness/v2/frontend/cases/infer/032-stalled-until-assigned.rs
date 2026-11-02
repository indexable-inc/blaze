//@ expect: accept
fn copy_of<T: Copy>(t: &T) -> T {
    *t
}

fn main() {
    let mut slot = None;
    let mut total = 0u32;
    let mut i = 0u32;
    while i < 3 {
        if let Some(prev) = slot {
            total += copy_of(&prev);
        }
        slot = Some(i);
        i += 1;
    }
    assert!(total == 1);
}
