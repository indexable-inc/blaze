// front2's own program: a trait's provided method. `Plain` leaves `weight` out, so its call
// runs the TRAIT's body with `Self = Plain` (which calls `Plain::size` back through `Self`);
// `Heavy` writes its own. Both reached through a generic bound, so the walk meets `weight`
// on a parameter and must walk the default body as the instance `Plain` resolves to.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for usize {}

trait Sized2 {
    fn size(&self) -> usize;
    fn weight(&self) -> usize {
        self.size() * 2
    }
}

struct Plain {
    n: usize,
}

struct Heavy {
    n: usize,
}

impl Sized2 for Plain {
    fn size(&self) -> usize {
        self.n
    }
}

impl Sized2 for Heavy {
    fn size(&self) -> usize {
        self.n
    }
    fn weight(&self) -> usize {
        self.n + 1
    }
}

fn weigh<T: Sized2>(item: &T) -> usize {
    item.weight()
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let plain = Plain { n: 15 };
    let heavy = Heavy { n: 11 };
    // 30 + 12 = 42.
    (weigh(&plain) + weigh(&heavy)) as i32
}
