//! A field whose type has no ground form is refused NAMING the field
//! ([`Refusal::FieldGround`]): the ADT, the field's row and name, and why grounding refused.
//! The Reference, "Associated Items", "Associated types": a projection `<u8 as Tr>::S` is a
//! type only through the impl that answers it; a world that answers none (the trait's default
//! `LayoutFacts::normalize`) leaves the field with no ground form, and the layout says which.
//!
//! Line it prints (and asserts the shape of): `type <n>: field 1 (name 0x1) of ADT 0x1000:
//! grounding its type: ..`. A wrong change (the field's index not carried, or the old unnamed
//! `Refusal::Ground`) fails the `matches!`.

use std::alloc::Global;
use std::format;

use blaze_typeck::{ItemKey, List, Name, PNode, UintTy};

use super::super::{Layouts, Refusal, ReprAttrs};
use super::{F, G, World, lay};

#[test]
fn a_field_with_no_ground_form_is_refused_by_name() {
    let mut w = World::default();
    // `struct S { a: u8, b: <u8 as Tr>::S }`: field 1's type is a projection the world does
    // not normalize.
    let s = w.strukt(0, &[F::Leaf(PNode::Uint(UintTy::U8)), F::Leaf(PNode::Uint(UintTy::U8))], ReprAttrs::RUST);
    {
        let d = w.def_mut(s);
        let u8_ = d.types.intern(PNode::Uint(UintTy::U8)).unwrap();
        let args = d.types.intern_list(&[u8_]).unwrap();
        let proj = d.types.intern(PNode::Alias { trait_: ItemKey(0x77), name: Name(5), args }).unwrap();
        d.fields[1].ty = proj;
    }
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let t = g.n(PNode::Adt(s, List::EMPTY));
    let error = lay(&w, &mut g, &mut ls, t).unwrap_err();
    let shown = format!("{error}");
    std::eprintln!("{shown}");
    assert!(matches!(error.why, Refusal::FieldGround { adt, field: 1, name: Name(1), .. } if adt == s), "{error:?}");
    assert!(shown.contains("field 1 (name 0x1) of ADT 0x1000"), "{shown}");
}
