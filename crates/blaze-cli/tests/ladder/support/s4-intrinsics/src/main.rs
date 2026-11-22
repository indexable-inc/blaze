// Support program s4: intrinsics (crates/blaze-build/src/intrinsics.rs), declared here as
// `#[rustc_intrinsic]` with no body, each built in place by the table:
// - `size_of::<u64>()` is 8, `ctpop(0xff_u32)` is 8;
// - `wrapping_add(250_u8, 10)` is 4, `rotate_left(0x81_u8, 1)` is 3;
// - `bswap(0x1300_0000_u32)` is 0x13 = 19;
// - `add_with_overflow(200_u8, 100)` is (44, true): exit 1 if not.
// Prints `intrinsics`, exits 8 + 8 + 4 + 3 + 19 = 42.
#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u32 {}

#[rustc_intrinsic]
const fn size_of<T>() -> usize;

#[rustc_intrinsic]
const fn ctpop<T: Copy>(x: T) -> u32;

#[rustc_intrinsic]
const fn wrapping_add<T: Copy>(a: T, b: T) -> T;

#[rustc_intrinsic]
const fn rotate_left<T: Copy>(x: T, shift: u32) -> T;

#[rustc_intrinsic]
const fn bswap<T: Copy>(x: T) -> T;

#[rustc_intrinsic]
const fn add_with_overflow<T: Copy>(x: T, y: T) -> (T, bool);

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let (sum, over) = add_with_overflow(200_u8, 100);
    if sum != 44 || !over {
        return 1;
    }
    unsafe {
        write(1, b"intrinsics\n" as *const u8, 11);
    }
    let a = size_of::<u64>() as u32 + ctpop(0xff_u32);
    let b = wrapping_add(250_u8, 10) as u32 + rotate_left(0x81_u8, 1) as u32;
    (a + b + bswap(0x1300_0000_u32)) as i32
}
