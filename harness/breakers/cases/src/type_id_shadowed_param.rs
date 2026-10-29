//! A type declared in a function can have the name of one of the function's type parameters,
//! and a function declared in one the name of one of its const parameters. rustc gives the
//! parameter its path first, so the item is the second of its name there and its `TypeId`
//! hashes that (astral-experiments/blaze#71 and #72).

use std::any::{Any, TypeId};

fn shadowed<Key>() -> impl Copy {
    #[derive(Clone, Copy)]
    struct Key;
    Key
}

#[allow(non_upper_case_globals)]
fn nested<const query: u8>() -> impl Copy {
    fn query() {}
    query
}

/// The hash itself: rustc's code and blaze's have to agree on it.
fn id<T: Any>(_: T) -> TypeId {
    TypeId::of::<T>()
}

pub fn run() -> [TypeId; 2] {
    [id(shadowed::<()>()), id(nested::<0>())]
}
