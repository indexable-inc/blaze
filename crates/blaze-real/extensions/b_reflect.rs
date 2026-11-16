//! reflect: `#[blaze::reflect]` (consent to reflect private fields) and `#[blaze::mark(..)]` on a field, read by `fields_of`. A working layer accepts it.
//! expect: accept
#![feature(blaze_comptime)]

struct Key;

#[blaze::reflect]
struct Row {
    #[blaze::mark(Key)]
    id: i32,
    note: i32,
}

fn key(r: &Row) -> i32 {
    let mut k = 0;
    for const f in core::reflect::fields_of::<Row>().marked::<Key>() {
        k = r.[f];
    }
    k
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    key(&Row { id: 42, note: 0 })
}
