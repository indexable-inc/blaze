// Own program x0-dropglue-02-slice (front2): the drop glue of a SLICE and of `str`, the body of
// `drop_glue::<[T]>` whose one parameter is a wide `&mut [T]`. The lang fn is declared as the
// PINNED core declares it, library/core/src/ptr/mod.rs (the toolchain shelf, nightly-2026-10-27):
//   /// Helper function for `drop_in_place`. The compiler replaces this by the actual drop glue.
//   #[lang = "drop_glue"]
//   pub(crate) const unsafe fn drop_glue<T: PointeeSized>(_: &mut T)
// and `drop_in_place<T: PointeeSized>(to_drop: *mut T)` calls `drop_glue(&mut *to_drop)`. A
// builder that takes only a thin `(*mut T)` parameter refuses this program ("a `drop_in_place`
// whose signature is not `(*mut T)`").
//
// The Reference, "Destructors" (destructors.operation): "The destructor of a type T consists of:
// If T: Drop, calling <T as std::ops::Drop>::drop; Recursively running the destructor of all of
// its fields", and "The elements of an array or owned slice are dropped from the first element
// to the last."
//
// A `[Counted; 3]` (weights 1, 10, 31) kept in a `ManuallyDrop` (so its scope's end drops
// nothing) is dropped once through a `*mut [Counted]` made by unsizing: each `Counted::drop` adds
// its weight to `count` (42 when each runs exactly once: a skipped element gives 41, 32 or 11, a
// doubled one more than 42, an empty glue 0) and doubles-then-adds its tag (1, 2, 3) to `seen`,
// so first to last gives 11 and last to first 17 (status 1). A `*mut str`'s drop does nothing:
// `count` stays 42. Exit 42.
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
impl<T> Copy for *mut T {}

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

unsafe fn drop_in_place<T: PointeeSized>(to_drop: *mut T) {
    unsafe { drop_glue(&mut *to_drop) }
}

#[lang = "manually_drop"]
#[repr(transparent)]
struct ManuallyDrop<T> {
    value: T,
}

struct Counted {
    weight: i32,
    tag: i32,
    count: *mut i32,
    seen: *mut i32,
}

impl Drop for Counted {
    fn drop(&mut self) {
        unsafe {
            *self.count += self.weight;
            let prev = *self.seen;
            *self.seen += prev;
            *self.seen += self.tag;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut count = 0;
    let mut seen = 0;
    let c = &mut count as *mut i32;
    let s = &mut seen as *mut i32;
    let mut arr = ManuallyDrop {
        value: [
            Counted { weight: 1, tag: 1, count: c, seen: s },
            Counted { weight: 10, tag: 2, count: c, seen: s },
            Counted { weight: 31, tag: 3, count: c, seen: s },
        ],
    };
    let p: *mut [Counted] = &mut arr.value as *mut [Counted; 3];
    unsafe { drop_in_place(p) };
    match seen {
        11 => {}
        _ => return 1,
    }
    let text: *const str = "abc";
    unsafe { drop_in_place(text as *mut str) };
    count
}
