//! `#![feature(blaze_anon_params)]` (and the implied bounds it needs) run
//! (docs/v2/plan/design/implied-bounds.md): a function and an impl over `V<T, _>` are
//! instantiated once per allocator they are called with, and each body calls the allocator
//! through the bound `V` declares, which nothing at the use writes.

use super::eval::check_pass;

#[test]
fn an_anonymous_parameter_is_instantiated_per_argument() {
    check_pass(
        r#"
//- minicore: sized, copy, clone, add, panic
#![feature(blaze_anon_params)]

trait Alloc {
    fn id(&self) -> u32;
}
struct V<T, A: Alloc>(T, A);
struct Global;
struct Arena(u32);
impl Alloc for Global {
    fn id(&self) -> u32 { 1 }
}
impl Alloc for Arena {
    fn id(&self) -> u32 { self.0 }
}

fn first<T: Copy>(v: &V<T, _>) -> T { v.0 }
fn ids(a: &V<u8, _>, b: &V<u8, _>) -> u32 { a.1.id() + b.1.id() }
impl<T> V<T, _> {
    fn alloc_id(&self) -> u32 { self.1.id() }
}
struct Pool<A: Alloc>(A);
impl<A> Pool<A> {
    fn twice(&self) -> u32 { self.0.id() + self.0.id() }
}

fn main() {
    let g = V(3u8, Global);
    let a = V(4u8, Arena(10));
    if first(&g) + first::<u8>(&a) != 7 {
        panic!("`first` read the wrong element");
    }
    if ids(&g, &a) != 11 || g.alloc_id() != 1 || a.alloc_id() != 10 {
        panic!("a body called the wrong allocator");
    }
    if Pool(Arena(5)).twice() != 10 {
        panic!("an implied bound's method was not the impl's");
    }
}
"#,
    );
}
