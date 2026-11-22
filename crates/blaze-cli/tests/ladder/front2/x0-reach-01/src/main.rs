// Own program x0-reach-01 (front2): two fns the code builder calls on its own account, which
// no body of the program calls by name, so the walk (blaze-ladder front2/reach.rs, with
// front2/back_needs.rs) must queue, check and table them before the build asks for them:
//
// 1. `Index::index` of `Table`: the Reference, "Array and slice indexing expressions": "For
//    other types an index expression `a[b]` is equivalent to `*std::ops::Index::index(&a, b)`,
//    or `*std::ops::IndexMut::index_mut(&mut a, b)` in a mutable place expression context."
//    The checker records no callee for `t[1]`; the walk asks the `index` lang trait's impls at
//    the program's shapes once a reached body indexes a type that is not an array or a slice.
// 2. `Drop::drop` of `Inner`, nested in `Outer` (no Drop of its own): the Reference,
//    "Destructors": "When a value of type T is dropped: If T: Drop, calling
//    <T as core::ops::Drop>::drop; Recursively running the destructor of all of its fields".
//    `o` dropped at the inner scope's end runs `Inner::drop` once through `Outer`'s glue.
//
// `Inner::drop` adds 30 to the counter, once; `t[1]` is 12 (index 0 gives 5, index 2 gives 7):
// 30 + 12 = 42. A walk that did not queue `Index::index` refuses the build by name ("never
// reached by the walk"); glue run twice gives 72, none gives 12; a wrong element gives 35 or 37.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for i32 {}

#[lang = "drop"]
trait Drop {
    fn drop(&mut self);
}

#[lang = "drop_glue"]
unsafe fn drop_glue<T>(_: &mut T) {}

#[lang = "index"]
trait Index<Idx> {
    type Output;
    fn index(&self, index: Idx) -> &Self::Output;
}

#[lang = "panic_bounds_check"]
fn panic_bounds_check(index: usize, len: usize) -> ! {
    loop {}
}

struct Inner {
    weight: i32,
    count: *mut i32,
}

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe {
            *self.count += self.weight;
        }
    }
}

struct Outer {
    tag: u8,
    inner: Inner,
}

struct Table {
    cells: [i32; 3],
}

impl Index<usize> for Table {
    type Output = i32;
    fn index(&self, index: usize) -> &i32 {
        &self.cells[index]
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut count = 0;
    let p = &mut count as *mut i32;
    {
        let o = Outer { tag: 1, inner: Inner { weight: 30, count: p } };
    }
    let t = Table { cells: [5, 12, 7] };
    // Annotated: `t[1]`'s type is the projection `<Table as Index<usize>>::Output`, and the
    // `+` on `i32` is then the built-in one (this no_core crate declares no `add` lang trait).
    let e: i32 = t[1];
    count + e
}
