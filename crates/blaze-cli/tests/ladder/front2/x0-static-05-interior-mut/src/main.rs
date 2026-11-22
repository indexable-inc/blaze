// Own program x0-static-05-interior-mut (front2): a static whose type is not `Freeze` is
// written by the program, so it is never placed read-only. The Reference, "Static items":
// "Statics may contain interior mutability" (through `UnsafeCell`, core/src/cell.rs: "the
// only legal way to obtain aliasable data that is considered mutable"); core/src/marker.rs
// `Freeze`: "Used to determine whether a type contains any `UnsafeCell` internally, but not
// through an indirection".
//
// One static per shape, each written through its cell and read back:
// - `A: AtomicU8`: the cell a field of the static;
// - `B: (u8, Cell)`: the cell inside a tuple's second field (the first, 2, is read back too);
// - `C: [AtomicUsize; 2]`: the cells array elements (only `C[1]` is written; `C[0]` stays 4);
// - `S: Slot`: the cell an enum variant's payload;
// - `D: &AtomicU8 = &AtomicU8 { .. }`: the pointee its own allocation, written through `D`
//   (the static itself, a reference, is `Freeze` and stays read-only);
// - `E: u32`: plain, read-only, read back as 8.
//
// Status 0 and the line below when every value read back is the one written; else the first
// failed check's number (a static placed read-only faults on its first write instead).
//
// Built `panic = "unwind"` with no `eh_personality`: `main`'s ABI is "C", which may not unwind
// (blaze-build walk/nounwind.rs), so every call in it is to an `extern "C"` fn (none can
// unwind into it) and `C` is read by an array pattern, not an index (no bounds check, whose
// panic would need a terminate pad): no record lands, so none needs a personality routine.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for u8 {}
impl Copy for usize {}
impl Copy for u32 {}

#[lang = "sync"]
unsafe trait Sync {}

#[lang = "unsafe_cell"]
#[repr(transparent)]
struct UnsafeCell<T> {
    value: T,
}

#[lang = "panic_bounds_check"]
fn panic_bounds_check(index: usize, len: usize) -> ! {
    loop {}
}

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

struct AtomicU8 {
    v: UnsafeCell<u8>,
}

struct AtomicUsize {
    v: UnsafeCell<usize>,
}

struct Cell {
    v: UnsafeCell<u8>,
}

enum Slot {
    Empty,
    Full(UnsafeCell<u32>),
}

unsafe impl Sync for AtomicU8 {}
unsafe impl Sync for (u8, Cell) {}
unsafe impl Sync for [AtomicUsize; 2] {}
unsafe impl Sync for Slot {}
unsafe impl<'a> Sync for &'a AtomicU8 {}
unsafe impl Sync for u32 {}

static A: AtomicU8 = AtomicU8 { v: UnsafeCell { value: 1 } };
static B: (u8, Cell) = (2, Cell { v: UnsafeCell { value: 3 } });
static C: [AtomicUsize; 2] = [AtomicUsize { v: UnsafeCell { value: 4 } }, AtomicUsize { v: UnsafeCell { value: 5 } }];
static S: Slot = Slot::Full(UnsafeCell { value: 6 });
static D: &AtomicU8 = &AtomicU8 { v: UnsafeCell { value: 7 } };
static E: u32 = 8;

extern "C" fn put_u8(cell: &UnsafeCell<u8>, value: u8) {
    unsafe { *(cell as *const UnsafeCell<u8> as *mut u8) = value }
}

extern "C" fn get_u8(cell: &UnsafeCell<u8>) -> u8 {
    unsafe { *(cell as *const UnsafeCell<u8> as *const u8) }
}

extern "C" fn put_usize(cell: &UnsafeCell<usize>, value: usize) {
    unsafe { *(cell as *const UnsafeCell<usize> as *mut usize) = value }
}

extern "C" fn get_usize(cell: &UnsafeCell<usize>) -> usize {
    unsafe { *(cell as *const UnsafeCell<usize> as *const usize) }
}

extern "C" fn put_u32(cell: &UnsafeCell<u32>, value: u32) {
    unsafe { *(cell as *const UnsafeCell<u32> as *mut u32) = value }
}

extern "C" fn get_u32(cell: &UnsafeCell<u32>) -> u32 {
    unsafe { *(cell as *const UnsafeCell<u32> as *const u32) }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let [c0, c1] = &C;
    put_u8(&A.v, 11);
    put_u8(&B.1.v, 12);
    put_usize(&c1.v, 13);
    match &S {
        Slot::Full(cell) => put_u32(cell, 14),
        Slot::Empty => return 4,
    }
    put_u8(&D.v, 15);
    if get_u8(&A.v) != 11 {
        return 1;
    }
    if B.0 != 2 || get_u8(&B.1.v) != 12 {
        return 2;
    }
    if get_usize(&c0.v) != 4 || get_usize(&c1.v) != 13 {
        return 3;
    }
    match &S {
        Slot::Full(cell) => {
            if get_u32(cell) != 14 {
                return 4;
            }
        }
        Slot::Empty => return 4,
    }
    if get_u8(&D.v) != 15 {
        return 5;
    }
    if E != 8 {
        return 6;
    }
    unsafe {
        write(1, b"six statics written and read back\n" as *const u8, 34);
    }
    0
}
