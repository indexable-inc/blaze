//@ expect: reject E0308
#[allow(dead_code)]
struct NeverBuilt;

impl Drop for NeverBuilt {
    fn drop(&mut self) {
        let _: u32 = "dropped"; //~ ERROR E0308
    }
}

fn main() {}
