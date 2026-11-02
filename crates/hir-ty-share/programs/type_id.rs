// `TypeId` tells apart types of one layout: `tid::<&P>` and `tid::<&Q>` must not share.
use std::any::TypeId;

struct P(#[allow(dead_code)] u32);
struct Q(#[allow(dead_code)] u32);

fn tid<T: 'static>() -> TypeId {
    TypeId::of::<T>()
}

fn main() {
    println!("{}", tid::<&'static P>() == tid::<&'static Q>());
    println!("{}", tid::<&'static P>() == tid::<&'static P>());
    println!("{}", tid::<P>() == TypeId::of::<P>());
    println!("{}", tid::<u32>() == tid::<i32>());
}
