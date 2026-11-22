// Own program x0-dropglue-03-boxed-slice (front2): the drop glue of `Box<[T]>` and `Box<str>`.
// The glue of a box drops its contents, then runs the box's `Drop` (the free). The pinned
// library, library/alloc/src/boxed.rs (`impl Drop for Box`): "the T in the Box is dropped by the
// compiler before the destructor is run", and the destructor only deallocates (by
// `Layout::for_value_raw`, the wide pointer's length times the element size). The Reference,
// "Destructors" (destructors.operation): "The elements of an array or owned slice are dropped
// from the first element to the last."
//
// The program declares its own `#[lang = "owned_box"]` box over a raw pointer into a local
// array (the "heap"), whose `Drop` frees nothing: it counts itself in FREES and records the
// order log SEEN at that moment in SEEN_AT_FREE. Each `Counted` adds its weight (1, 10, 31) to
// COUNT and doubles-then-adds its tag (1, 2, 3) to SEEN: first to last gives 11, last to first
// 17.
// 1. A `Box<str>` at its scope's end: the free only (FREES 1, COUNT 0), else status 1.
// 2. A `Box<[Counted]>` of three at its scope's end: each element once, first to last, then the
//    free (FREES 2, SEEN_AT_FREE 11), else status 2 or 3; a builder that frees before the
//    elements gives SEEN_AT_FREE 0.
// Exit COUNT: 42 when each element dropped exactly once. A builder that refuses the glue stops
// with "the drop of a boxed slice's or `str`'s elements".
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for i32 {}

#[lang = "unsize"]
trait Unsize<T: PointeeSized>: PointeeSized {}

#[lang = "coerce_unsized"]
trait CoerceUnsized<T: PointeeSized> {}

impl<T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<*mut U> for *mut T {}

#[lang = "drop"]
trait Drop {
    fn drop(&mut self);
}

#[lang = "drop_glue"]
unsafe fn drop_glue<T: PointeeSized>(_: &mut T) {}

#[lang = "manually_drop"]
#[repr(transparent)]
struct ManuallyDrop<T> {
    value: T,
}

#[lang = "owned_box"]
struct Box<T: ?Sized>(*mut T);

impl<T: ?Sized> Drop for Box<T> {
    fn drop(&mut self) {
        unsafe {
            FREES += 1;
            SEEN_AT_FREE = SEEN;
        }
    }
}

static mut COUNT: i32 = 0;
static mut SEEN: i32 = 0;
static mut FREES: i32 = 0;
static mut SEEN_AT_FREE: i32 = 0;

struct Counted {
    weight: i32,
    tag: i32,
}

impl Drop for Counted {
    fn drop(&mut self) {
        unsafe {
            COUNT += self.weight;
            let prev = SEEN;
            SEEN += prev;
            SEEN += self.tag;
        }
    }
}

fn boxed_str() {
    let text: *const str = "abc";
    let _b: Box<str> = Box(text as *mut str);
}

fn boxed_slice() {
    let mut heap = ManuallyDrop {
        value: [Counted { weight: 1, tag: 1 }, Counted { weight: 10, tag: 2 }, Counted { weight: 31, tag: 3 }],
    };
    let p: *mut [Counted] = &mut heap.value as *mut [Counted; 3];
    let _b: Box<[Counted]> = Box(p);
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    boxed_str();
    match unsafe { FREES } {
        1 => {}
        _ => return 1,
    }
    match unsafe { COUNT } {
        0 => {}
        _ => return 1,
    }
    boxed_slice();
    match unsafe { FREES } {
        2 => {}
        _ => return 2,
    }
    match unsafe { SEEN_AT_FREE } {
        11 => {}
        _ => return 3,
    }
    unsafe { COUNT }
}
