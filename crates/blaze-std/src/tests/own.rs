//! `bstd::own` (docs/v2/plan/design/ownership.md): the derived properties as this crate's own
//! types meet them, and the handles at run time. The walk itself, each refusal's path and the
//! property test against a reference model are the front end's
//! (`crates/hir-ty-mir/src/tests/consteval/reach.rs`), which evaluate the same text.

use crate::{
    cell::Token,
    own::{Acyclic, AnyCell, AnySecret, NoCapability, Released, Shared, Shares, none},
    sync::{LocalLock, LocalShared},
};

struct List {
    head: u32,
    tail: Option<Box<List>>,
}

struct Tree {
    kids: Vec<Tree>,
    name: String,
}

#[allow(dead_code, reason = "a shape for the walk to decide, never built")]
struct Counted {
    hits: LocalLock<u32>,
    last: LocalShared<List>,
}

// Decided as the crate compiles: each is the walk's answer for one instance.
const _: () = assert!(none::<List, AnySecret>());
const _: () = assert!(none::<Tree, AnyCell>());

fn acyclic<T: ?Sized + Acyclic>() {}
fn no_capability<T: ?Sized + NoCapability>() {}

/// Types that own themselves only through sole pointers, or hold a counted pointer no cell
/// can reset, are `Acyclic`; plain data holds no capability.
#[test]
fn derived_properties_hold_of_plain_types() {
    acyclic::<List>();
    acyclic::<Tree>();
    acyclic::<Counted>();
    acyclic::<Shared<Vec<u8>>>();
    no_capability::<List>();
    no_capability::<(u8, Tree)>();
}

/// An ordinary value in a handle is what it was in an `Arc`: cloned, compared, counted.
#[test]
fn a_handle_to_an_ordinary_value_is_cloned_and_dropped() {
    let first = Shared::new(vec![1u8, 2]);
    let second = first.clone();
    assert!(Shared::ptr_eq(&first, &second));
    assert_eq!(Shared::handles(&first), 2);
    assert_eq!(*second, [1, 2]);
    drop(first);
    assert_eq!(Shared::handles(&second), 1);
}

/// The value comes back from the last handle released, and from no other.
#[test]
fn the_last_release_gives_the_value_back() {
    let first = Shared::holding(List { head: 7, tail: None });
    let second = first.share();
    assert!(matches!(first.release(), Released::NotLast));
    match second.release() {
        Released::Last(list) => assert_eq!((list.head, list.tail.is_none()), (7, true)),
        Released::NotLast => panic!("the last handle gives the value back"),
    }
}

/// Parts are split, read and joined with the arithmetic in their types, and the handle that
/// holds every part gives the value back.
#[test]
fn shares_split_join_and_give_the_whole_back() {
    let total = Token::scope(|token| {
        let all: Shares<Tree, _, 4, 4> = Shares::new(token, Tree { kids: Vec::new(), name: "root".to_owned() });
        let (one, three) = all.split::<1, 3>();
        let (two, last) = three.split::<2, 1>();
        assert_eq!((one.name.as_str(), two.name.as_str(), last.kids.len()), ("root", "root", 0));
        let three: Shares<Tree, _, 3, 4> = one.join(two);
        let mut all: Shares<Tree, _, 4, 4> = three.join(last);
        all.get_mut().name.push('!');
        all.whole().name
    });
    assert_eq!(total, "root!");
}
