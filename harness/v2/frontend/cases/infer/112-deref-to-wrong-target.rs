//@ expect: reject E0308
use core::ops::Deref;

struct Holder(u32);

impl Deref for Holder {
    type Target = u32;
    fn deref(&self) -> &u32 {
        &self.0
    }
}

fn takes(_: &u8) {}

fn main() {
    let h = Holder(1);
    takes(&h); //~ ERROR E0308
}
