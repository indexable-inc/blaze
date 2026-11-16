// front2's own program: a trait's method called on a type that HOLDS a type parameter below
// its top (`Score::score` on `Wrap<T>` inside the generic `twice`). The impl can only be
// picked where `twice`'s instance is made: selecting it while checking the generic body
// stopped with "an argument is not ground" (list-U, "check: Resolve"). The walk decides the
// call at the instance and walks the impls of `Wrap`'s shape.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for i32 {}

trait Score {
    fn score(&self) -> i32;
}

struct Wrap<T> {
    v: T,
}

impl<T: Score> Score for Wrap<T> {
    fn score(&self) -> i32 {
        self.v.score() + 1
    }
}

impl Score for i32 {
    fn score(&self) -> i32 {
        *self
    }
}

fn twice<T: Score>(w: &Wrap<T>) -> i32 {
    w.score() * 2
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    // (20 + 1) * 2 = 42.
    twice(&Wrap { v: 20i32 })
}
