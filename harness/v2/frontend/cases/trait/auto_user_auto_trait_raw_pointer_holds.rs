//@ expect: accept
//@ edition: 2021
// A raw pointer's constituent is its pointee: a user auto trait (with no negative impl for
// pointers, unlike Send and Sync) holds for `*const u8` and `*mut (u8, char)`.
#![feature(auto_traits)]

auto trait Plain {}

struct Raw {
    p: *const u8,
    q: *mut (u8, char),
}

fn is_plain<T: Plain>() -> u32 {
    1
}

fn main() {
    let r = Raw { p: std::ptr::null(), q: std::ptr::null_mut() };
    let n = is_plain::<*const u8>() + is_plain::<*mut (u8, char)>() + is_plain::<Raw>();
    println!("{} {} {}", n, r.p.is_null(), r.q.is_null());
}
