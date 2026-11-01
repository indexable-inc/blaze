//! `hash_order_output` over [`super`]'s fixtures, with an `alloc` whose `Vec` and `String` keep
//! an order and a `std` whose `HashMap` iterates in its table's.

use super::check_in;

const ORDER_CRATES: &str = r#"
//- /alloc/lib.rs crate:alloc
pub mod vec {
    pub struct Vec<T>(*mut T);
    impl<T> Vec<T> {
        pub fn new() -> Vec<T> {
            loop {}
        }
        pub fn push(&mut self, _value: T) {}
        pub fn sort(&mut self) {}
    }
    impl<T> core::iter::FromIterator<T> for Vec<T> {
        fn from_iter<I: core::iter::IntoIterator<Item = T>>(_iter: I) -> Vec<T> {
            loop {}
        }
    }
}
pub mod collections {
    pub struct BTreeSet<T>(*mut T);
    impl<T> core::iter::FromIterator<T> for BTreeSet<T> {
        fn from_iter<I: core::iter::IntoIterator<Item = T>>(_iter: I) -> BTreeSet<T> {
            loop {}
        }
    }
}
//- /std/lib.rs crate:std deps:alloc
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod collections {
    pub struct HashMap<K, V>(*mut (K, V));
    pub struct Keys<'a, K>(&'a K);
    impl<'a, K> Iterator for Keys<'a, K> {
        type Item = &'a K;
        fn next(&mut self) -> Option<&'a K> {
            None
        }
    }
    impl<K, V> HashMap<K, V> {
        pub fn keys(&self) -> Keys<'_, K> {
            loop {}
        }
    }
    impl<'a, K, V> IntoIterator for &'a HashMap<K, V> {
        type Item = &'a K;
        type IntoIter = Keys<'a, K>;
        fn into_iter(self) -> Keys<'a, K> {
            loop {}
        }
    }
}
//- /src/lib.rs crate:main deps:hir_ty,alloc,std
"#;

/// A hash map's order collected into a `Vec` that is returned, stored or handed on, or pushed by
/// a `for` loop into a `Vec` never sorted, is a finding, under a renamed map type and in a
/// macro's loop; a sorted `Vec`, an ordered set and a loop that only counts are none.
#[test]
fn hash_order_output() {
    check_in(
        ORDER_CRATES,
        r#"
use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use std::collections::HashMap as Table;
pub struct Names<'a> {
    names: Vec<&'a u32>,
}
pub fn returned(map: &Table<u32, u32>) -> Vec<&u32> {
    map.keys().collect() //~ hash_order_output
}
pub fn through_a_local(map: &Table<u32, u32>) -> Vec<&u32> {
    let keys: Vec<&u32> = map.keys().collect(); //~ hash_order_output
    keys
}
pub fn stored(map: &Table<u32, u32>) -> Names<'_> {
    Names { names: map.keys().collect() } //~ hash_order_output
}
pub fn handed_on(map: &Table<u32, u32>) {
    take(map.keys().collect()); //~ hash_order_output
}
pub fn take(_keys: Vec<&u32>) {}
pub fn sorted(map: &Table<u32, u32>) -> Vec<&u32> {
    let mut keys: Vec<&u32> = map.keys().collect();
    keys.sort();
    keys
}
pub fn ordered(map: &Table<u32, u32>) -> BTreeSet<&u32> {
    map.keys().collect()
}
pub fn pushed(map: &Table<u32, u32>) -> Vec<u32> {
    let mut out = Vec::new();
    for key in map { //~ hash_order_output
        out.push(*key);
    }
    out
}
pub fn pushed_then_sorted(map: &Table<u32, u32>) -> Vec<u32> {
    let mut out = Vec::new();
    for key in map.keys() {
        out.push(*key);
    }
    out.sort();
    out
}
pub fn counted(map: &Table<u32, u32>) -> u32 {
    let mut n = 0;
    for _key in map {
        n += 1;
    }
    n
}
macro_rules! gather {
    ($map:expr, $out:ident) => {
        for key in $map.keys() {
            $out.push(*key);
        }
    };
}
pub fn by_macro(map: &Table<u32, u32>) -> Vec<u32> {
    let mut out = Vec::new();
    gather!(map, out); //~ hash_order_output
    out
}
"#,
        "-A clippy::all --warn=blaze::hash_order_output",
    );
}
