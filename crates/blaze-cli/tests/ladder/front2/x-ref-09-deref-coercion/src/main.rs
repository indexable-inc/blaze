//! x-ref-09: `&&[u8]` and `&[u8; N]` coerce to `&[u8]`; method calls auto-dereference.
//!
//! The Reference, "Type coercions", coercion types: "`&T` or `&mut T` to `&U` if `T`
//! implements `Deref<Target = U>`" (and `&[u8]` implements `Deref`... through `&T: Deref<Target
//! = T>`), and "Unsized coercions": "`[T; n]` to `[T]`". "Method call expressions": the
//! receiver is dereferenced repeatedly to find the method.
//! s = &[1, 2, 3] (len 3, sum 6):
//! 1. len_of(&s) with `&s: &&[u8]`: coerced to `&[u8]`: 3.
//! 2. sum(&&&s): 6.
//! 3. an array reference `&[4u8, 5, 6, 7]` passed as `&[u8]`: len 4, sum 22.
//! 4. method call on `&&[u8]`: `ss.len()` is 3, `ss[2]` (auto-deref for indexing) is 3.
//! A wrong builder that passes the outer reference's address as the slice's data exits 2.

fn len_of(s: &[u8]) -> usize {
    s.len()
}

fn sum(s: &[u8]) -> u32 {
    let (mut t, mut i) = (0, 0);
    while i < s.len() {
        t += s[i] as u32;
        i += 1;
    }
    t
}

fn main() -> std::entry::Status {
    let s: &[u8] = &[1, 2, 3];
    let ss: &&[u8] = &s;
    let arr = [4u8, 5, 6, 7];
    let status = if len_of(&s) != 3 {
        1
    } else if sum(&&&s) != 6 {
        2
    } else if len_of(&arr) != 4 || sum(&arr) != 22 {
        3
    } else if ss.len() != 3 || ss[2] != 3 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
