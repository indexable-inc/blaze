// Own program x0-static-04-generic-field (front2): a static whose fields' types are a generic
// struct at concrete arguments and an atomic-like field through an associated-type
// projection, the shape of alloc's `static STATIC_INNER_SLICE: SliceArcInnerForStatic`
// (library/alloc/src/rcs/arc.rs: `#[repr(C, align(16))] struct SliceArcInnerForStatic
// { inner: ArcInner<[u8; 1]> }`, `strong`/`weak` of `Atomic<usize>`, core's
// `struct Atomic<T: AtomicPrimitive> { v: UnsafeCell<T::Storage> }`), which the hello world's
// walk left out with "a field type with no ground form is not evaluated yet" (front2/
// const_project.rs grounds it through the checker's solver now).
//
// The Reference, "Static items": "The static initializer is a constant expression evaluated
// at compile time"; "Type layout", "The `C` Representation": each field is placed at the next
// offset that is a multiple of its alignment; "Associated Items", "Associated types":
// `<u32 as Prim>::Storage` is the impl's `Al8<u32>` (size 8, align 8).
//
// - `HELD.inner.strong`, 20, made by a `const fn` (as `AtomicUsize::new` makes the library's).
// - `HELD.inner.weak`, 15, at offset 8 (after `Al8<u32>`'s padding), written as a literal.
// - `HELD.inner.data[0]`, 7, at offset 16.
//
// Status: 20 + 15 + 7 = 42. A field type grounded as `u32` (projection answered wrongly)
// puts `weak` at 4 and `data` at 8, and reads 20 + 0 + 0 = 20 or garbage; a static left out
// does not build.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for u32 {}
impl Copy for u8 {}

#[lang = "sync"]
unsafe trait Sync {}

unsafe impl Sync for [u8; 4] {}
unsafe impl<'a> Sync for &'a u8 {}

#[lang = "panic_bounds_check"]
fn panic_bounds_check(index: usize, len: usize) -> ! {
    loop {}
}

trait Prim {
    type Storage;
}

#[repr(C, align(8))]
struct Al8<T>(T);

impl Prim for u32 {
    type Storage = Al8<u32>;
}

#[repr(C)]
struct Atom<T: Prim> {
    v: T::Storage,
}

impl Atom<u32> {
    const fn new(v: u32) -> Atom<u32> {
        Atom { v: Al8(v) }
    }
}

#[repr(C)]
struct Inner<T> {
    strong: Atom<u32>,
    weak: Atom<u32>,
    data: T,
}

#[repr(C, align(16))]
struct Holder {
    inner: Inner<[u8; 1]>,
}

unsafe impl Sync for Holder {}

static HELD: Holder = Holder { inner: Inner { strong: Atom::new(20), weak: Atom { v: Al8(15) }, data: [7] } };

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    (HELD.inner.strong.v.0 + HELD.inner.weak.v.0 + HELD.inner.data[0] as u32) as i32
}
