//! `blaze::implied_bound_written` (docs/v2/plan/design/implied-bounds.md): a bound a header
//! already implies is found on an impl and on a function, also one an associated type's short
//! form goes through (the short form reads the implied bounds too); one the header does not
//! imply, one the short form would not see without it, and every bound of a crate without
//! the feature, are not.

use crate::tests::lint;

const FLAGS: &str = "-A clippy::all --warn=blaze::implied_bound_written";

fn linted(main: &str) -> String {
    let fixture = format!(
        "//- minicore: sized, copy, clone, iterator\n//- /src/lib.rs crate:main\n{}",
        main.trim_start_matches('\n')
    );
    lint(&fixture, FLAGS)
}

const SOURCE: &str = r#"
pub trait Alloc {}
pub struct Pool<T, A: Alloc> {
    t: T,
    a: A,
}
impl<T, A: Alloc> Pool<T, A> {
    pub fn get(&self) -> &T {
        &self.t
    }
}
pub fn size<T, A: Alloc + Clone>(_p: &Pool<T, A>) -> usize {
    0
}
pub struct Iter<I: Iterator>(I);
impl<I: Iterator> Iter<I> {
    pub fn first(&mut self) -> Option<I::Item> {
        self.0.next()
    }
}
"#;

/// The impls' `A: Alloc` and `I: Iterator` and the function's `A: Alloc` are found, the
/// second even though `I::Item` goes through it; `A: Clone` is no bound of `Pool`.
#[test]
fn a_bound_the_header_implies_is_found() {
    let out = linted(&format!("#![feature(blaze_implied_bounds)]\n{SOURCE}"));
    assert!(out.contains("the bound `Alloc` on the impl is already implied"), "{out}");
    assert!(out.contains("the bound `Alloc` on `size` is already implied"), "{out}");
    assert!(!out.contains("`Clone`"), "{out}");
    assert!(out.contains("the bound `Iterator` on the impl is already implied"), "{out}");
    assert_eq!(out.matches("is already implied by").count(), 3, "{out}");
}

/// `B: Iterator` implied only by `Tr`'s declaration through the shorthand `A::Out`, and only
/// by `W` through the alias `Al`: the shorthand `B::Item` reads the header as written and sees
/// neither, so removing the bound would be E0220, and neither is found. The control: the same
/// bound implied by `W<B>` named in a parameter, which the shorthand sees, is found.
#[test]
fn a_bound_only_the_written_header_hides_from_a_shorthand_is_not_found() {
    let source = r#"#![feature(blaze_implied_bounds)]
pub trait Tr<B: Iterator> {
    type Out;
}
pub struct W<B: Iterator>(B);
pub type Al<B> = W<B>;
pub fn through_projection<A: Tr<B>, B: Iterator>(_a: A::Out) -> B::Item {
    loop {}
}
pub fn through_alias<B: Iterator>(_w: Al<B>) -> B::Item {
    loop {}
}
pub fn through_type<B: Iterator>(_w: W<B>) -> B::Item {
    loop {}
}
"#;
    let out = linted(source);
    assert!(out.contains("the bound `Iterator` on `through_type` is already implied"), "{out}");
    assert!(!out.contains("`through_projection`"), "{out}");
    assert!(!out.contains("`through_alias`"), "{out}");
    assert_eq!(out.matches("is already implied by").count(), 1, "{out}");
}

/// Without the feature a bound is the only place it is said: nothing is found.
#[test]
fn without_the_feature_nothing_is_found() {
    let out = linted(SOURCE);
    assert_eq!(out.matches("is already implied by").count(), 0, "{out}");
}
