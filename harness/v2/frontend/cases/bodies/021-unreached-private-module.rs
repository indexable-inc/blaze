//@ expect: reject E0308
mod never_used {
    pub fn helper() -> bool {
        0u8 //~ ERROR E0308
    }
}

fn main() {}
