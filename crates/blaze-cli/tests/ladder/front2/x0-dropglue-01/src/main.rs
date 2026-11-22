// Own program x0-dropglue-01 (front2): the drop glue's lang fn declared as the PINNED core
// declares it, `drop_glue`, not the older `drop_in_place`. library/core/src/ptr/mod.rs (the
// toolchain shelf, nightly-2026-10-27):
//   /// Helper function for `drop_in_place`. The compiler replaces this by the actual drop glue.
//   #[lang = "drop_glue"]
//   pub(crate) const unsafe fn drop_glue<T: PointeeSized>(_: &mut T)
// and the public `pub const unsafe fn drop_in_place<T: PointeeSized>(to_drop: *mut T)` is only
// `#[rustc_diagnostic_item = "ptr_drop_in_place"]`, calling `drop_glue(&mut *to_drop)`. A seam
// that looks only for `#[lang = "drop_in_place"]` refuses this program ("a value's drop needs
// the `drop_in_place` lang item fn, which the program does not declare").
//
// The Reference, "Destructors": "When a value of type T is dropped: If T: Drop, calling
// <T as core::ops::Drop>::drop; Recursively running the destructor of all of its fields", and
// a variable's value is dropped when its scope ends. `Holder` (no Drop of its own) holds a
// `Counted` (whose Drop adds its weight to the counter): `h` dropped at the inner scope's end
// runs `Counted::drop` once through `Holder`'s glue (adds 40); `c` alone runs it once (adds 2).
// Exactly one drop each: 42. A glue run twice gives 84 or 44; none gives 0, 2 or 40.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for i32 {}
impl<T> Copy for *mut T {}

#[lang = "drop"]
trait Drop {
    fn drop(&mut self);
}

#[lang = "drop_glue"]
unsafe fn drop_glue<T>(_: &mut T) {}

struct Counted {
    weight: i32,
    count: *mut i32,
}

impl Drop for Counted {
    fn drop(&mut self) {
        unsafe {
            *self.count += self.weight;
        }
    }
}

struct Holder {
    tag: u8,
    inner: Counted,
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut count = 0;
    let p = &mut count as *mut i32;
    {
        let h = Holder { tag: 1, inner: Counted { weight: 40, count: p } };
        let c = Counted { weight: 2, count: p };
    }
    count
}
