// Own program x0-static-02-late-body (front2, walk-AH Brief 15): statics read only by bodies the
// walk reaches at an instance, never by name: each `v` is an impl's method called through the
// generic `g`, so its body is checked when `g::<u8>` / `g::<u16>` resolves `X::v`, after the
// bodies named from `main`. The Reference, "Static items": a static "represents an allocation
// in the program that is initialized with the initializer expression. All references and raw
// pointers to the static refer to the same allocation." Each such static must be evaluated
// and placed like one `main` names (front2/statics.rs `static_values` over every tabled body).
//
// Status: g(1u8) + g(1u16) = S + T = 40 + 2 = 42. A static never asked fails the build with
// "NoStatic"; the two impls reading one static's place give 80 or 4.
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

static S: u32 = 40;
static T: u32 = 2;

trait V {
    fn v(&self) -> u32;
}

impl V for u8 {
    fn v(&self) -> u32 {
        S
    }
}

impl V for u16 {
    fn v(&self) -> u32 {
        T
    }
}

fn g<X: V>(x: X) -> u32 {
    x.v()
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    (g(1u8) + g(1u16)) as i32
}
