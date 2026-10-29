//! astral-sh/blaze#73 (indexable-inc/blaze#1329, #1331): the `TypeId` of a function of an
//! `impl` a macro makes. rustc's hash of it counts the `impl`s of the module before it, which
//! blaze had for none a macro makes, so where every crate the type names is built already (the
//! standard library over its unit, `NonZero<u8>`'s methods coming from `nonzero_integer!`) it
//! hashed nothing and stopped. Two `impl`s a macro makes, and one written out, with methods of
//! one name, must still be told apart.

use std::any::{Any, TypeId};
use std::num::NonZero;

fn id<A: Any>(_: &A) -> TypeId {
    TypeId::of::<A>()
}

struct Written;
struct ByMacro;
struct AlsoByMacro;

impl Written {
    fn get() -> u8 {
        1
    }
}

macro_rules! getter {
    ($ty:ty, $value:expr) => {
        impl $ty {
            fn get() -> u8 {
                $value
            }
        }
    };
}

getter!(ByMacro, 2);
getter!(AlsoByMacro, 3);

pub fn run() -> ([bool; 6], u32, u8) {
    let narrow = id(&NonZero::<u8>::leading_zeros);
    let wide = id(&NonZero::<u16>::leading_zeros);
    let ours = [id(&Written::get), id(&ByMacro::get), id(&AlsoByMacro::get)];
    let known = [
        narrow == id(&NonZero::<u8>::leading_zeros),
        narrow != wide,
        ours[1] == id(&ByMacro::get),
        ours[0] != ours[1],
        ours[1] != ours[2],
        ours[0] != ours[2],
    ];
    let sum = Written::get() + ByMacro::get() + AlsoByMacro::get();
    (known, NonZero::new(1u8).unwrap().leading_zeros(), sum)
}
