//! A trait object of a trait whose method takes `self` by value and tracks its caller in the
//! impl (image 0.25's `ImageEncoder::write_image`, reached through `Box<dyn ImageEncoderBoxed>`
//! in zed's build script): its vtable slot is rustc's `ShimKind::VTable` of the trait's method,
//! which blaze refused to make ("vtable method taking `self` by value that tracks its caller").
//! Here a small receiver passed in registers, a large one passed in memory, and a method whose
//! declaration tracks its caller too.

use std::panic::Location;

trait Encode {
    fn encode(self, scale: u32) -> (u32, u32);

    #[track_caller]
    fn declared(self) -> u32;

    fn name(&self) -> u32;
}

trait Boxed: Encode {
    fn encode_boxed(self: Box<Self>, scale: u32) -> (u32, u32);
}

impl<T: Encode> Boxed for T {
    fn encode_boxed(self: Box<Self>, scale: u32) -> (u32, u32) {
        (*self).encode(scale)
    }
}

struct Small(u8);

struct Large([u64; 8]);

impl Encode for Small {
    #[track_caller]
    fn encode(self, scale: u32) -> (u32, u32) {
        (u32::from(self.0) * scale, Location::caller().line())
    }

    fn declared(self) -> u32 {
        Location::caller().line()
    }

    fn name(&self) -> u32 {
        1
    }
}

impl Encode for Large {
    #[track_caller]
    fn encode(self, scale: u32) -> (u32, u32) {
        (self.0.iter().sum::<u64>() as u32 * scale, Location::caller().line())
    }

    fn declared(self) -> u32 {
        Location::caller().line()
    }

    fn name(&self) -> u32 {
        2
    }
}

#[inline(never)]
fn boxed(which: u8) -> Box<dyn Boxed> {
    match which {
        0 => Box::new(Small(3)),
        _ => Box::new(Large([1; 8])),
    }
}

pub fn run() -> Vec<(u32, (u32, u32), u32)> {
    let mut out = Vec::new();
    for which in 0..2 {
        let it = boxed(which);
        let name = it.name();
        let encoded = it.encode_boxed(5);
        let declared = match which {
            0 => Small(1).declared(),
            _ => Large([0; 8]).declared(),
        };
        out.push((name, encoded, declared));
    }
    out
}
