//! x-gen-09: a blanket impl gives a trait to every type with another trait.
//!
//! The Reference, "Implementations", "Generic implementations": an impl with type parameters
//! (`impl<T: Area> Describe for T`) implements the trait for every type the bound admits.
//! Describe::describe(&self) = area * 100 + sides.
//! 1. Sq(3): area 9, sides 4: 904.       2. Tri(2, 5): area 2 * 5 / 2 = 5, sides 3: 503.
//! 3. through a generic `T: Describe` bound (the blanket impl's instance at T): Sq(1): 104.
//! 4. through `&dyn Describe` (the blanket impl at Tri): Tri(4, 4): area 8, 803.
//! A wrong builder that instantiates the blanket method at the wrong T exits 1 or 2.

trait Area {
    fn area(&self) -> u32;
    fn sides(&self) -> u32;
}

trait Describe {
    fn describe(&self) -> u32;
}

impl<T: Area> Describe for T {
    fn describe(&self) -> u32 {
        self.area() * 100 + self.sides()
    }
}

struct Sq(u32);
struct Tri(u32, u32);

impl Area for Sq {
    fn area(&self) -> u32 {
        self.0 * self.0
    }
    fn sides(&self) -> u32 {
        4
    }
}

impl Area for Tri {
    fn area(&self) -> u32 {
        self.0 * self.1 / 2
    }
    fn sides(&self) -> u32 {
        3
    }
}

fn via<T: Describe>(t: &T) -> u32 {
    t.describe()
}

fn main() -> std::entry::Status {
    let d: &dyn Describe = &Tri(4, 4);
    let status = if Sq(3).describe() != 904 {
        1
    } else if Tri(2, 5).describe() != 503 {
        2
    } else if via(&Sq(1)) != 104 {
        3
    } else if d.describe() != 803 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
