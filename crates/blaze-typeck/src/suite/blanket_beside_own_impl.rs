//! A type's own impl beside a blanket impl whose bound the type does not meet (Class O of the
//! hello run, 2026-11-16: bstd's `fast::Beside: Clone` refused as "`Beside: ArbitraryInt`
//! because `Beside: Clone`", core's `const impl<T: ArbitraryInt> Clone for T` in
//! library/core/src/num/arbitrary.rs beside `#[derive(Clone)] struct Beside`).
//!
//! ```text
//! trait Clone { fn clone(&self) -> Self; }   trait Marker {}
//! impl<T: Marker> Clone for T {}             // the blanket impl (core's ArbitraryInt one)
//! struct B;  impl Clone for B {}             // left out by the control
//! fn need<T: Clone>(x: &T) -> u32;
//! fn f(x: &B) -> u32 { need(x) }
//! ```
//!
//! The Reference, "Trait and lifetime bounds": a bound `T: Clone` is met when an
//! implementation of `Clone` for the type exists whose own where-clauses hold; "Implementations",
//! "Trait implementation coherence": the two impls do not overlap only because `B: Marker`
//! does not hold, so the blanket impl is NOT an implementation for `B`, and `impl Clone for B`
//! is. A candidate whose where-clause fails is dropped and the others decide (blaze-solve
//! trait_goals.rs `merge` sees only the candidates that held): `B: Clone` holds.
//!
//! What this pins down (a causal claim, tested by removing the cause): the message the hello
//! run printed is what the checker says exactly when the type's OWN impl is not among the
//! impls the world answers for `(Clone, Adt(B))`: with it, nothing; without it (the control),
//! E0277 on `need(x)` with the leaf `B: Marker` under the goal `B: Clone` (machine.rs
//! `failed_leaf`: exactly one impl header matched, the blanket one). So Class O is an impl
//! table that left `Beside`'s derived impl out (or filed it under another shape or trait), not
//! a solver that stops at the first failing candidate.

use std::format;
use std::string::String;
use std::vec::Vec;

use super::check_body;
use crate::diag::{Because, UnmetGoal};
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, Mutbl};
use crate::infer::InferPlant;
use crate::machine::Plant;

/// The world of the module doc; `impl Clone for B` only `with_own`. Returns it with the keys
/// of `Clone` and `Marker`.
fn world(with_own: bool) -> (Fx, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let clone = fx.tr("Clone", 1, Vec::new(), &[]);
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    fx.trait_fn(clone, "clone", true, &[r_self], p0);
    let marker = fx.tr("Marker", 1, Vec::new(), &[]);
    let t_marker = fx.bound(marker, &[p0]);
    fx.imp("blanket_clone", 1, p0, Some((clone, &[])), &[], std::vec![t_marker]);
    let b_k = fx.strukt("B", 0, &[]);
    let b = fx.adt(b_k, &[]);
    if with_own {
        fx.imp("clone_b", 0, b, Some((clone, &[])), &[], Vec::new());
    }
    let u32_ = fx.u32();
    let t_clone = fx.bound(clone, &[p0]);
    fx.func("need", 1, &[r_self], u32_, std::vec![t_clone]);
    let r_b = fx.r(Mutbl::Not, b);
    fx.func("f", 0, &[r_b], u32_, Vec::new());
    fx.finish();
    (fx, clone, marker)
}

/// `need(x)` in `f`, checked: each diagnostic as `code at expr: goal-trait / leaf-trait` (the
/// traits' names; `-` where there is none). Expression ids: `need` 0, `x` 1, the call 2,
/// the block 3.
fn findings(with_own: bool) -> Vec<String> {
    let (fx, clone, marker) = world(with_own);
    let name = |k: ItemKey| -> &'static str {
        if k == clone {
            "Clone"
        } else if k == marker {
            "Marker"
        } else {
            "other"
        }
    };
    let mut b = Bb::new();
    let x = b.param("x");
    let f = b.path(&["need"]);
    let e1 = b.local(x);
    let call = b.call(f, &[e1]);
    let root = b.block(&[], Some(call));
    let c = check_body(&fx, &b, fx.key_of("f"), root, Plant::None, InferPlant::None);
    let mut out = Vec::new();
    for d in c.diags.iter() {
        let (goal, leaf) = match d.because {
            Some(Because::Unmet { goal: UnmetGoal::Trait { trait_, .. }, leaf }) => (
                name(trait_),
                match leaf {
                    Some(UnmetGoal::Trait { trait_: l, .. }) => name(l),
                    _ => "-",
                },
            ),
            _ => ("-", "-"),
        };
        out.push(format!("{:?} at {}: {goal} / {leaf}", d.code, d.at.start));
    }
    out.sort();
    out
}

/// `B: Clone` holds by `impl Clone for B` although the blanket impl's `T: Marker` fails for
/// `B`. Prints nothing; fails if a failing blanket candidate decided the goal.
#[test]
fn own_impl_holds_beside_a_failing_blanket() {
    let got = findings(true);
    assert!(got.is_empty(), "want no finding, got:\n{}", got.join("\n"));
}

/// The control: without `impl Clone for B` the finding is the hello run's, E0277 naming the
/// blanket impl's bound as the leaf. Prints "without B's own impl: [\"Unsatisfied at 1:
/// Clone / Marker\"]".
#[test]
fn control_own_impl_left_out_blames_the_blanket_bound() {
    let got = findings(false);
    std::println!("without B's own impl: {got:?}");
    // The bound a callee's where-clauses owe is filed at the argument passed for the input that
    // names it (`x` of `need(x)`, 1: rules/obligation_site.rs).
    assert_eq!(got, std::vec![String::from("Unsatisfied at 1: Clone / Marker")]);
}
