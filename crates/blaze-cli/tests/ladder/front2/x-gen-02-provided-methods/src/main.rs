//! x-gen-02: a trait's provided method runs for an impl that does not override it; an impl
//! that overrides it runs its own; the provided method calls the impl's required method.
//!
//! The Reference, "Traits": "Associated functions may ... have a default implementation";
//! "Implementations": an impl "may also include" an item that overrides the default.
//! trait Speak { fn base(&self) -> u32; fn loud(&self) -> u32 { self.base() * 10 } }
//! 1. Cat (base 3, no override): loud = 30.
//! 2. Dog (base 4, overrides loud as base + 1000): 1004.
//! 3. through a generic `twice<T: Speak>`: loud + loud: Cat 60, Dog 2008.
//! 4. the provided method's `self.base()` dispatches to each impl: Cat base 3, Dog base 4.
//! A wrong builder that resolves `loud` to the trait's default for Dog exits 2 (40).

trait Speak {
    fn base(&self) -> u32;
    fn loud(&self) -> u32 {
        self.base() * 10
    }
}

struct Cat;
struct Dog;

impl Speak for Cat {
    fn base(&self) -> u32 {
        3
    }
}

impl Speak for Dog {
    fn base(&self) -> u32 {
        4
    }
    fn loud(&self) -> u32 {
        self.base() + 1000
    }
}

fn twice<T: Speak>(t: &T) -> u32 {
    t.loud() + t.loud()
}

fn main() -> std::entry::Status {
    let status = if Cat.loud() != 30 {
        1
    } else if Dog.loud() != 1004 {
        2
    } else if twice(&Cat) != 60 || twice(&Dog) != 2008 {
        3
    } else if Cat.base() != 3 || Dog.base() != 4 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
