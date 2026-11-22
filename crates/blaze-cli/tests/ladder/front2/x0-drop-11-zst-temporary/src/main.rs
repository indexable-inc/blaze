// Own program x0-drop-11-zst-temporary (front2): a REDUCTION of a suspect, written and NEVER
// RUN by the wrong-answers lane (2026-11-16, WRONG-NOTES.md "Step 5", suspect S1): the code
// builder decides "this value needs drop" from how the value is HELD
// (blaze-build walk.rs `repr_needs_drop`: only an aggregate can need drop), and a zero-sized
// type is held as nothing. A zero-sized BINDING that needs drop is refused by name; a
// zero-sized TEMPORARY that needs drop is, by reading, dropped never and without a word.
//
// The Reference, "Destructors":
// - [destructors.intro] "When an initialized variable or temporary goes out of scope, its
//   destructor is run or it is dropped."
// - [destructors.operation] "The destructor of a type `T` consists of: If `T: Drop`, calling
//   `<T as core::ops::Drop>::drop`".
// - "Temporary scopes", [destructors.scope.temporary.enclosing]: "Apart from lifetime
//   extension, the temporary scope of an expression is the smallest scope that contains the
//   expression and is one of the following: The entire function. A statement. .."
// Nothing there depends on the type's size: `Guard` has no bytes and has a destructor.
//
// 1. `make();`: the statement's value is a temporary, dropped at the statement's end: 1 run.
// 2. `let _ = make();`: nothing is bound; the temporary is dropped at the statement's end: 2.
// 3. `touch(&make());`: a borrowed temporary, dropped at the statement's end, after the
//    call: `touch` sees 2 runs, and after the statement there are 3.
// Success is status 42; a failed check returns its number. A refusal by name (a zero-sized
// temporary that needs drop) is an honest answer too; 1, 2, 3 or 4 is the wrong one.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for i32 {}

#[lang = "sync"]
unsafe trait Sync {}

#[lang = "drop"]
trait Drop {
    fn drop(&mut self);
}

#[lang = "drop_glue"]
unsafe fn drop_glue<T>(_: &mut T) {}

static mut RUNS: i32 = 0;

struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            RUNS += 1;
        }
    }
}

fn make() -> Guard {
    Guard
}

fn runs() -> i32 {
    unsafe { RUNS }
}

fn touch(_: &Guard) -> i32 {
    runs()
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    make();
    let one = runs();
    let _ = make();
    let two = runs();
    let seen = touch(&make());
    let three = runs();
    if one != 1 {
        1
    } else if two != 2 {
        2
    } else if seen != 2 {
        3
    } else if three != 3 {
        4
    } else {
        42
    }
}
