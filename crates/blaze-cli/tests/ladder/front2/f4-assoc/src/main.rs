// Own program f4-assoc (front2): an associated type at an instance. The generic `first` names
// `I::Item` in its signature; at `I = Count` the code builder asks the front end to normalize
// `<Count as Iterator>::Item` (the seam's `LayoutFacts::normalize`, answered by the checker's
// own selection and projection step), and the instance's key covers what it normalized to.
// `no_core`, so the program's own `Iterator` and `Option` are the whole library.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for u8 {}

enum Option<T> {
    None,
    Some(T),
}

trait Iterator {
    type Item;
    fn next(&mut self) -> Option<Self::Item>;
}

struct Count {
    n: u8,
}

impl Iterator for Count {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        if self.n < 3 {
            self.n += 1;
            Option::Some(self.n * 10)
        } else {
            Option::None
        }
    }
}

fn first<I: Iterator>(mut i: I) -> Option<I::Item> {
    i.next()
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    // The first item is 10: 10 + 32 = 42.
    match first(Count { n: 0 }) {
        Option::Some(v) => v as i32 + 32,
        Option::None => 1,
    }
}
