//! A read-list digest is a function of a head's CONTENT, never of where its terms sit in the
//! table it was read from (`Hashes::rooted`, `Tabled::hash_shape`, portable.rs
//! `hash_node_shape`).
//!
//! Why: a body's record is valid by its read list (the plan's "a record is valid by its key, or
//! else by its read list checked against today's facts"), and the same head reaches a checker
//! from different stores: the solved names of a whole program, or per-crate records loaded on
//! demand. Each store numbers its terms in the order its run met them. On 2026-11-16 the
//! `front2_records` test's body 1 read `Box::leak`'s signature (`fn leak<'a>(b: Self) -> &'a mut
//! T`, alloc/src/boxed.rs: inputs `Box<T, A>`, output `&mut T`) equal in content from both
//! stores, its input at `PTy(478)` in one and `PTy(486)` in the other, and the two digests
//! differed (records-5-diff.txt: "READ FnSig ...: digest over records 4558583610268652552,
//! solved 8417850094779154848; content equal: true").
//!
//! The test builds one signature in two tables that intern the same terms in different orders
//! (and with a term the head never reaches in one of them), and asserts one digest; then a
//! signature whose `Box` arguments are swapped, which must differ. Without the shape hash
//! (`Hashes::term` hashing `hash_node`, which writes child indices and list starts) the first
//! assertion fails.
//!
//! Expected line: `test world::read_digest::one_head_in_two_tables_has_one_digest ... ok`.

use blaze_own::{Alloc, Vec};

use super::{FnSig, Hashes};
use crate::ids::{Abi, FnPtrHeader, ItemKey, List, Safety};
use crate::portable::{PNode, PTy, PTys};

const BOX: ItemKey = ItemKey(7);

fn sig<'t>(types: PTys<'t>, inputs: &'t [PTy], output: PTy) -> FnSig<'t> {
    FnSig {
        parent_count: 2,
        own_count: 0,
        declared_count: 0,
        self_param: false,
        header: FnPtrHeader { abi: Abi::Rust, safety: Safety::Safe, c_variadic: false },
        c_variadic: false,
        legacy_const_generics: &[],
        inputs,
        output,
        types,
    }
}

fn digest(s: &FnSig<'_>) -> u64 {
    let mut h = Hashes::new_in(Alloc::global());
    h.rooted(s).unwrap_or_else(|e| panic!("the digest of a well-formed head: {e:?}"))
}

#[test]
fn one_head_in_two_tables_has_one_digest() {
    // `fn(Box<T, A>) -> &mut T` with `T` = Param(0), `A` = Param(1).
    // Table one: T, A, Box<T, A>, &mut T.
    let one_nodes = [
        PNode::Param(0),
        PNode::Param(1),
        PNode::Adt(BOX, List { start: 0, len: 2 }),
        PNode::Ref(crate::ids::Mutbl::Mut, PTy(0)),
    ];
    let one_lists = [PTy(0), PTy(1)];
    let one = PTys::flat(&one_nodes, &one_lists);
    let one_inputs = [PTy(2)];
    let a = digest(&sig(one, &one_inputs, PTy(3)));

    // Table two: a term no head here reaches first, then A before T, a list elsewhere.
    let two_nodes = [
        PNode::Bool,
        PNode::Param(1),
        PNode::Param(0),
        PNode::Ref(crate::ids::Mutbl::Mut, PTy(2)),
        PNode::Adt(BOX, List { start: 1, len: 2 }),
    ];
    let two_lists = [PTy(0), PTy(2), PTy(1)];
    let two = PTys::flat(&two_nodes, &two_lists);
    let two_inputs = [PTy(4)];
    let b = digest(&sig(two, &two_inputs, PTy(3)));
    assert_eq!(a, b, "one head read from two tables gave two digests");

    // `fn(Box<A, T>) -> &mut T`: another head, another digest.
    let swapped_lists = [PTy(1), PTy(0)];
    let swapped = PTys::flat(&one_nodes, &swapped_lists);
    let c = digest(&sig(swapped, &one_inputs, PTy(3)));
    assert_ne!(a, c, "`Box<T, A>` and `Box<A, T>` gave one digest");

    // The input count is content: `fn(Box<T, A>, Box<T, A>) -> &mut T` differs too.
    let mut twice: Vec<PTy, Alloc<'_>> = Vec::new_in(Alloc::global());
    twice.push(PTy(2));
    twice.push(PTy(2));
    let d = digest(&sig(one, &twice, PTy(3)));
    assert_ne!(a, d, "one input and two inputs gave one digest");
}
