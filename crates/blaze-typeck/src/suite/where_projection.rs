//! A where-clause that holds a projection is assumed as written (Reference, "Trait and
//! lifetime bounds": "Bounds on an item must be satisfied when using the item. When
//! type-checking and borrow-checking a generic item, the bounds can be used to determine that a
//! trait is implemented for a type."). The clause `T: Inject<<T as Other>::A>` enters the
//! param env with `<T as Other>::A` kept as the alias: an assumption is not a goal, so there is
//! nothing to normalize it to (bstd's `From::from` with `Inject<S, <U as UnionFrom<S>>::Path>`).
//!
//! ```text
//! trait Other { type A; }
//! trait Inject<X> { fn go(self) -> u32; }
//! fn main<T: Other>(t: T) -> u32 where T: Inject<<T as Other>::A> { t.go() }
//! ```

use std::vec::Vec;

use super::{Want, check_body, expect};
use crate::fixture::{Bb, Fx};
use crate::ids::{ExprId, ItemKey, LangItem, PatId};
use crate::infer::InferPlant;
use crate::machine::Plant;

fn world() -> (Fx, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let other = fx.tr("Other", 1, Vec::new(), &["A"]);
    let inject = fx.tr("Inject", 2, Vec::new(), &[]);
    let p0 = fx.param(0);
    let u32_ = fx.u32();
    fx.trait_fn(inject, "go", true, &[p0], u32_);
    fx.use_trait(inject);
    let a = fx.alias(other, "A", &[p0]);
    let is_other = fx.bound(other, &[p0]);
    let injects = fx.bound(inject, &[p0, a]);
    let main = fx.func("main", 1, &[p0], u32_, std::vec![is_other, injects]);
    fx.finish();
    (fx, main)
}

/// `t.go()` is `<T as Inject<<T as Other>::A>>::go`, proved by the where-clause as written.
#[test]
fn a_projection_in_a_where_clause_is_assumed_as_written() {
    let (fx, main) = world();
    let mut b = Bb::new();
    let t = b.param("t");
    let e0 = b.local(t);
    let e1 = b.method(e0, "go", &[]);
    let root = b.block(&[], Some(e1));
    let c = check_body(&fx, &b, main, root, Plant::None, InferPlant::None);
    expect(
        &fx,
        &b,
        &c,
        &Want { exprs: &[(ExprId(0), "P0"), (ExprId(1), "u32"), (ExprId(2), "u32")], pats: &[(PatId(0), "P0")], diags: &[] },
    );
}
