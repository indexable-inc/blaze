// A program `blaze run` refuses by name (run_output.rs
// `a_landing_record_with_no_routine_is_refused_naming_its_fn`): built `panic = "unwind"`, it
// declares no `#[lang = "eh_personality"]` fn, and `main` holds `Noisy` (a value with a `Drop`)
// across a call through a fn pointer, which may unwind. `main`'s ABI is "C", which may not
// unwind (blaze-build walk/nounwind.rs): that call's unwind edge lands in its terminate block,
// so `main`'s record has a row that lands, and its unwind entry would need a personality
// routine the program does not have. Refused at that record, naming `main`; the same program
// with no such call runs (rule 4: see `x0-static-05-interior-mut`, which lands nowhere).
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

#[lang = "drop"]
trait Drop {
    fn drop(&mut self);
}

#[lang = "drop_glue"]
unsafe fn drop_glue<T>(_: &mut T) {}

#[lang = "panic_cannot_unwind"]
fn panic_cannot_unwind() -> ! {
    loop {}
}

struct Noisy {
    n: i32,
}

impl Drop for Noisy {
    fn drop(&mut self) {
        self.n = 0;
    }
}

fn may_unwind() {}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let noisy = Noisy { n: 7 };
    let f: fn() = may_unwind;
    f();
    noisy.n
}
