// Support program s2: statics and constant data (crates/blaze-build/src/data.rs, blaze-emit's
// `Reloc`). A `static` of plain bytes printed through `write`; a `static mut` counter written
// by a callee and read back; a static holding the address of another static (a relocation in
// the statics' image). Prints `statics`, exits 38 + 2 bumps + *BASE_REF (2) = 42.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for u32 {}

#[lang = "sync"]
unsafe trait Sync {}

unsafe impl Sync for u32 {}
unsafe impl Sync for [u8; 8] {}
unsafe impl<'a> Sync for &'a u32 {}

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

static MSG: [u8; 8] = [b's', b't', b'a', b't', b'i', b'c', b's', b'\n'];
static BASE: u32 = 2;
static BASE_REF: &u32 = &BASE;
static mut COUNT: u32 = 38;

fn bump() {
    unsafe {
        COUNT = COUNT + 1;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    bump();
    bump();
    unsafe {
        write(1, &MSG as *const [u8; 8] as *const u8, 8);
        (COUNT + *BASE_REF) as i32
    }
}
