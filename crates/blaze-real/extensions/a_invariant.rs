//! invariant: `#[blaze::invariant(..)]` on a private-field struct; a constructor breaks it. A working layer refuses it.
//! expect: refuse

#[blaze::invariant(self.lo <= self.hi)]
struct Span {
    lo: i32,
    hi: i32,
}

fn backwards() -> Span {
    Span { lo: 9, hi: 1 }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let s = backwards();
    s.lo - s.hi
}
