// Own program x0-static-03-fn-pointers (front2): a static that holds fn pointers. The
// Reference, "Static items": a static "represents an allocation in the program that is
// initialized with the initializer expression"; "Function pointer types": a fn pointer "refers
// to a function whose identity is not necessarily known at compile-time", made here by
// coercing a fn item ("Type coercions", [coerce.types.fn]).
//
// - `TABLE`, two fn pointers: `forty` and `two` are named ONLY by the static's initializer, so
//   no call in any body reaches them: the run must build them because the static points at them,
//   and write each one's address into the static's words once the code's addresses are known.
// - `PICK`, a static holding `&TABLE`: a fn pointer read through another static's relocation.
//
// Status: (TABLE[0])() + (PICK[1])() = 40 + 2 = 42. A word never written is 0 and faults; a
// fn not built stops the run (blaze's own stop, status 125); the two words swapped make
// `PICK[1]` return 40, and the status is 1.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

#[lang = "sync"]
unsafe trait Sync {}

unsafe impl Sync for [fn() -> u32; 2] {}
unsafe impl<'a> Sync for &'a [fn() -> u32; 2] {}

#[lang = "panic_bounds_check"]
fn panic_bounds_check(index: usize, len: usize) -> ! {
    loop {}
}

fn forty() -> u32 {
    40
}

fn two() -> u32 {
    2
}

static TABLE: [fn() -> u32; 2] = [forty, two];
static PICK: &[fn() -> u32; 2] = &TABLE;

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let second = (PICK[1])();
    match second {
        2 => ((TABLE[0])() + second) as i32,
        _ => 1,
    }
}
