//! x-gen-05: trait objects of different types in one array, called in a loop.
//!
//! The Reference, "Trait objects": each `&dyn Trait` carries its own value's vtable, so one
//! call site reaches each element's own impl.
//! items = [&Add(1), &Mul(3), &Add(10), &Neg]; folding v = item.apply(v) from v = 2:
//! 2 + 1 = 3; 3 * 3 = 9; 9 + 10 = 19; Neg gives 100 - 19 = 81.
//! 1. the fold gives 81.
//! 2. the ids of the items in order, as digits: Add 1, Mul 2, Add 1, Neg 3: 1213.
//! 3. the array's length 4 and a second pass from 0: 0+1 = 1, 3, 13, 87.
//! A wrong builder that uses the first element's vtable for all exits 1 (2+1+1+10+1 = 15 or
//! similar).

trait Op {
    fn apply(&self, v: u32) -> u32;
    fn id(&self) -> u32;
}

struct Add(u32);
struct Mul(u32);
struct Neg;

impl Op for Add {
    fn apply(&self, v: u32) -> u32 {
        v + self.0
    }
    fn id(&self) -> u32 {
        1
    }
}

impl Op for Mul {
    fn apply(&self, v: u32) -> u32 {
        v * self.0
    }
    fn id(&self) -> u32 {
        2
    }
}

impl Op for Neg {
    fn apply(&self, v: u32) -> u32 {
        100 - v
    }
    fn id(&self) -> u32 {
        3
    }
}

fn run(items: &[&dyn Op], start: u32) -> (u32, u32) {
    let (mut v, mut ids, mut i) = (start, 0, 0);
    while i < items.len() {
        v = items[i].apply(v);
        ids = ids * 10 + items[i].id();
        i += 1;
    }
    (v, ids)
}

fn main() -> std::entry::Status {
    let items: [&dyn Op; 4] = [&Add(1), &Mul(3), &Add(10), &Neg];
    let (v, ids) = run(&items, 2);
    let (again, _) = run(&items, 0);
    let status = if v != 81 {
        1
    } else if ids != 1213 {
        2
    } else if items.len() != 4 || again != 87 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
