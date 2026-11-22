//! x-gen-07: a supertrait's method is callable through the subtrait's bound and its trait object.
//!
//! The Reference, "Traits", "Supertraits": "Supertraits are traits that are required to be
//! implemented for a type to implement a specific trait. Furthermore, anywhere a generic or
//! trait object is bounded by a trait, it has access to the associated items of its
//! supertraits."
//! trait Named { fn name(&self) -> u32; }  trait Greeter: Named { fn greet(&self) -> u32 {
//! self.name() * 2 } }; Ann: name 5; Bob: name 7, greet overridden as name + 100.
//! 1. Ann.greet() = 10; Bob.greet() = 107.
//! 2. through `&dyn Greeter`: name() (a supertrait method from the subtrait's vtable): 5 and 7.
//! 3. through `&dyn Greeter`: greet(): 10 and 107.
//! 4. a generic `T: Greeter` calling both: name + greet: Ann 15, Bob 114.
//! A wrong builder that lays the supertrait's slots after the subtrait's (or not at all) exits 2.

trait Named {
    fn name(&self) -> u32;
}

trait Greeter: Named {
    fn greet(&self) -> u32 {
        self.name() * 2
    }
}

struct Ann;
struct Bob;

impl Named for Ann {
    fn name(&self) -> u32 {
        5
    }
}

impl Greeter for Ann {}

impl Named for Bob {
    fn name(&self) -> u32 {
        7
    }
}

impl Greeter for Bob {
    fn greet(&self) -> u32 {
        self.name() + 100
    }
}

fn both<T: Greeter>(t: &T) -> u32 {
    t.name() + t.greet()
}

fn main() -> std::entry::Status {
    let gs: [&dyn Greeter; 2] = [&Ann, &Bob];
    let status = if Ann.greet() != 10 || Bob.greet() != 107 {
        1
    } else if gs[0].name() != 5 || gs[1].name() != 7 {
        2
    } else if gs[0].greet() != 10 || gs[1].greet() != 107 {
        3
    } else if both(&Ann) != 15 || both(&Bob) != 114 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
