//! x-ref-02: an `&mut` passed to a function is reborrowed, not moved: it is usable after the call.
//!
//! Passing `r: &mut T` where `&mut T` is expected reborrows `&mut *r` (the Reference,
//! "Type coercions": coercion sites include function arguments; the reborrow keeps `r`). Every
//! write goes to the one place.
//! 1. add(r, 2) twice, then `*r += 1` with x = 0: x = 5.
//! 2. an explicit reborrow `let s = &mut *r; *s *= 3;` then a write through r: (5 * 3) + 1 = 16.
//! 3. a method taking `&mut self` called twice on a struct through `&mut`: count 2.
//! 4. a shared reborrow `&*r` read while r is not used: reads 16.
//! A wrong builder that copies the pointee for the call exits 1 (x = 1).

struct Counter {
    count: u32,
}

impl Counter {
    fn hit(&mut self) {
        self.count += 1;
    }
}

fn add(p: &mut u32, n: u32) {
    *p += n;
}

fn main() -> std::entry::Status {
    let mut x = 0u32;
    let mut c = Counter { count: 0 };
    let (one, two, four) = {
        let r = &mut x;
        add(r, 2);
        add(r, 2);
        *r += 1;
        let one = *r;
        let s = &mut *r;
        *s *= 3;
        *r += 1;
        let two = *r;
        let shared = &*r;
        (one, two, *shared)
    };
    let cr = &mut c;
    cr.hit();
    cr.hit();
    let status = if one != 5 {
        1
    } else if two != 16 || x != 16 {
        2
    } else if c.count != 2 {
        3
    } else if four != 16 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
