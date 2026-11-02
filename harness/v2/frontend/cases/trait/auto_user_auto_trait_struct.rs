//@ expect: accept
//@ edition: 2021
// A user auto trait holds structurally for a struct of scalars, tuples, arrays, `&'static str`
// and a reference back to itself (a coinductive cycle).
#![feature(auto_traits)]

auto trait Plain {}

struct Record {
    a: u8,
    b: (u16, [u32; 2]),
    c: &'static str,
    d: Option<&'static Record>,
}

fn is_plain<T: ?Sized + Plain>() -> u32 {
    1
}

static LEAF: Record = Record { a: 1, b: (2, [3, 4]), c: "leaf", d: None };

fn main() {
    let r = Record { a: 5, b: (6, [7, 8]), c: "root", d: Some(&LEAF) };
    let n = is_plain::<Record>() + is_plain::<str>() + is_plain::<[Record]>() + is_plain::<(u8, char)>();
    println!("{} {} {} {}", n, r.a, r.c, r.d.map_or(0, |l| l.b.1[1]));
}
