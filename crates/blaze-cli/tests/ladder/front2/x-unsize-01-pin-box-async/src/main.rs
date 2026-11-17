//! x-unsize-01: `Pin<Box<{async block}>>` unsizes to `Pin<Box<dyn Future<Output = u8>>>`, and
//! dropping the pinned box drops the unstarted future's captures through the new vtable.
//!
//! The Reference, "Type coercions", "Unsized coercions": "`Foo<..., T, ...>` to `Foo<..., U,
//! ...>`, when: `Foo` is a struct. `T` implements `Unsize<U>`. The last field of `Foo` has a
//! type involving `T`. If that field has type `Bar<T>`, then `Bar<T>` implements
//! `Unsize<Bar<U>>`." core's `CoerceUnsized` (library/core/src/ops/unsize.rs, nightly-2026-10-27):
//! "The coercion will work by coercing the `Bar<T>` field into `Bar<U>` and filling in the rest
//! of the fields from `Foo<T>` to create a `Foo<U>`. This will effectively drill down to a
//! pointer field and coerce that." core's pin.rs has `impl<Ptr, U> CoerceUnsized<Pin<U>> for
//! Pin<Ptr> where Ptr: CoerceUnsized<U> + PinSafePointer, U: PinSafePointer`; the box is
//! blaze-own's (the edition's prelude, D120), `PinSafePointer` when its allocator is a
//! `StaticAllocator` (crates/blaze-own/src/boxed.rs): `Pin { pointer: Box<T, Heap> }`,
//! `Box { ptr: NonNull<T>, alloc: Heap, _owns: PhantomData<T> }`, `NonNull { pointer: *const T }`:
//! the builder walks the three structs to the raw pointer and makes its two words there
//! (crates/blaze-build/src/walk/unsize_nested.rs). A pinned box needs an allocator that never
//! reuses a block before its box frees it, which a frame is not (it can be dropped after a
//! leaked box), so the boxes are in the process's heap, the `heap: Heap` grant the entry hands
//! `main`. The Reference, "Destructors": an async
//! block's future that was never polled drops the values it captured when it is dropped.
//! Each drop appends its id: log = log * 10 + id.
//! 1. f: `Pin<Box<dyn Future<Output = u8> + Send, Heap>>` of `async { 7u8 }`: nothing logged.
//! 2. g: `Pin<Box<dyn Future<Output = u8> + '_, Heap>>` of an `async move` block holding
//!    Counted(3): nothing logged while it lives: log 0.
//! 3. `drop(g)`: the future's drop (vtable word 0) drops Counted(3): log 3.
//! 4. `drop(f)`: nothing to log: 3.
//! Exits 0. Before walk/unsize_nested.rs the build refuses `main` ("an unsizing to other than
//! a slice or a trait object": `Pin`'s one differing argument is a `Box`); a builder that
//! dropped through a wrong table exits 2 or crashes; one that never drops the captures exits 2.

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;

struct Counted<'a>(u8, &'a Cell<u32>);

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0 as u32);
    }
}

fn main(_cx: &Cx, heap: Heap) -> std::entry::Status {
    let log = Cell::new(0);
    let f: Pin<Box<dyn Future<Output = u8> + Send, Heap>> = Box::pin_in(async { 7u8 }, heap);
    let c = Counted(3, &log);
    let g: Pin<Box<dyn Future<Output = u8> + '_, Heap>> = Box::pin_in(
        async move {
            let c = c;
            c.0 + 1
        },
        heap,
    );
    let before = log.get();
    drop(g);
    let after_g = log.get();
    drop(f);
    let status = if before != 0 {
        1
    } else if after_g != 3 || log.get() != 3 {
        2
    } else {
        0
    };
    std::entry::Status(status)
}
