//! blaze's implied bounds and anonymous parameters (docs/v2/plan/design/implied-bounds.md):
//! `#![feature(blaze_implied_bounds)]` gives a function, impl or trait the bounds its header's
//! types and traits declare; `#![feature(blaze_anon_params)]` makes a `_` type argument of a
//! function's parameter types or an impl's header a parameter of its own, bounded by them.
//! Each accepted with its gate and refused without, and each refusal by what it says.

use crate::tests::{LIBRARY_FEATURES, checked, library_features};

/// `source` as the crate `main`, beside a crate `dep` of `dep_source` with no gate and the
/// staged crate that declares the two features (as `library/core/src/blaze.rs` does).
fn fixture(dep_source: &str, source: &str) -> String {
    let features = library_features(&["blaze_implied_bounds", "blaze_anon_params"]);
    format!(
        "//- minicore: sized, copy, clone, option, slice, add\n{features}\
         //- /dep.rs crate:dep\n{dep_source}\n\
         //- /main.rs crate:main deps:dep,{LIBRARY_FEATURES}\n{source}"
    )
}

fn messages(source: &str) -> Vec<String> {
    checked(&fixture("", source)).messages()
}

fn stderr(source: &str) -> String {
    checked(&fixture("", source)).stderr()
}

/// The crate `source`, beside `dep_source`, has no error (a warning is not one).
#[track_caller]
fn accepted(dep_source: &str, source: &str) {
    let out = checked(&fixture(dep_source, source)).stderr();
    assert!(!out.contains("error"), "{out}");
}

/// A trait with a method a body calls through the implied bound, and a struct declaring it.
const DECLS: &str = r#"
pub trait X {
    fn x(&self) -> u32;
}
pub struct C<A: X>(pub A);
"#;

const USES: &str = r#"
impl<A> C<A> {
    pub fn get(&self) -> u32 {
        self.0.x()
    }
}
pub fn f<A>(c: &C<A>) -> u32 {
    c.0.x()
}
fn main() {}
"#;

#[test]
fn an_impl_and_a_function_get_the_bounds_their_headers_name() {
    accepted("", &format!("#![feature(blaze_implied_bounds)]\n{DECLS}{USES}"));
}

/// The accepted crate above less its gate line: each call through the unwritten bound is the
/// ordinary method error, and nothing else is said.
#[test]
fn without_the_gate_the_bound_must_be_written() {
    let out = messages(&format!("{DECLS}{USES}"));
    let unbound = "E0599: no method named `x` found for type parameter `A` in the current scope";
    assert_eq!(out, [unbound, unbound], "{out:#?}");
}

/// A caller of `f::<u8>` proves `u8: X`, which `C` declares and `f` was given: refused as a
/// written where clause is, pointing at the bound where `C` declares it.
#[test]
fn a_caller_that_cannot_prove_the_bound_is_refused() {
    let source = format!(
        "#![feature(blaze_implied_bounds)]\n{DECLS}\
         pub fn f<A>(_c: Option<C<A>>) -> u32 {{ 0 }}\n\
         fn main() {{ let _ = f::<u8>(None); }}\n"
    );
    let out = stderr(&source);
    assert!(out.contains("error[E0277]: the trait bound `u8: X` is not satisfied"), "{out}");
    assert!(out.contains("pub struct C<A: X>(pub A);"), "{out}");
}

/// `C<A>` under an `Option`, a reference and a slice still names `C`.
#[test]
fn a_type_named_at_any_depth_gives_its_bounds() {
    let source = format!(
        "#![feature(blaze_implied_bounds)]\n{DECLS}\
         pub fn first<A>(c: Option<&[C<A>]>) -> u32 {{\n\
             match c {{ Some([c, ..]) => c.0.x(), _ => 0 }}\n\
         }}\nfn main() {{}}\n"
    );
    accepted("", &source);
}

/// `A: X` implied, then elaborated to `X`'s supertrait `Sup`; and a trait whose supertrait
/// `Holder<A>` declares `A: X` gives its default body `A: X`.
#[test]
fn an_implied_bound_brings_its_supertraits() {
    let source = r#"#![feature(blaze_implied_bounds)]
pub trait Sup {
    fn s(&self) -> u32;
}
pub trait X: Sup {}
pub struct C<A: X>(pub A);
impl<A> C<A> {
    pub fn s(&self) -> u32 {
        self.0.s()
    }
}
pub trait Holder<A: X> {}
pub trait User<A>: Holder<A> {
    fn u(a: &A) -> u32 {
        a.s()
    }
}
fn main() {}
"#;
    accepted("", source);
}

/// `dep` has no gate: its `C` still gives `main`'s function the bound, since the gate is the
/// crate's whose item gets it.
#[test]
fn a_dependency_s_definition_gives_its_bounds() {
    let dep = "pub trait X { fn x(&self) -> u32; }\npub struct C<A: X>(pub A);";
    let source = "#![feature(blaze_implied_bounds)]\nuse dep::X;\n\
                  pub fn f<A>(c: &dep::C<A>) -> u32 { c.0.x() }\nfn main() {}\n";
    accepted(dep, source);
}

const ALLOC: &str = r#"
pub trait Alloc {
    fn id(&self) -> u32;
}
pub struct V<T, A: Alloc>(pub T, pub A);
pub struct G;
pub struct B;
impl Alloc for G {
    fn id(&self) -> u32 { 1 }
}
impl Alloc for B {
    fn id(&self) -> u32 { 2 }
}
"#;

/// `_` in a parameter type and in an impl's self type: each its own parameter, bounded by
/// `V`'s `A: Alloc`, which the bodies call through; `first::<u8>` names `T` only.
#[test]
fn an_anonymous_parameter_takes_any_argument_its_position_allows() {
    let source = format!(
        "#![feature(blaze_anon_params)]\n{ALLOC}\
         pub fn first<T: Copy>(v: &V<T, _>) -> T {{ v.0 }}\n\
         pub fn ids(a: &V<u8, _>, b: &V<u8, _>) -> u32 {{ a.1.id() + b.1.id() }}\n\
         impl<T> V<T, _> {{ pub fn alloc_id(&self) -> u32 {{ self.1.id() }} }}\n\
         pub fn take(_x: _) {{}}\n\
         fn main() {{\n\
             let (g, b) = (V(1u8, G), V(2u8, B));\n\
             let _ = first(&g) + first::<u8>(&b);\n\
             let _ = ids(&g, &b) + g.alloc_id() + b.alloc_id();\n\
             take(1u8);\n\
             take(g);\n\
         }}\n"
    );
    accepted("", &source);
}

/// Each place a `_` is not an anonymous parameter, refused as rustc refuses it, with the note
/// naming the rule.
#[test]
fn a_placeholder_elsewhere_is_refused_by_its_position() {
    let source = format!(
        "#![feature(blaze_anon_params)]\n{ALLOC}\
         pub fn ret() -> V<u8, _> {{ V(0, G) }}\n\
         pub struct Field {{ pub v: V<u8, _> }}\n\
         pub const K: Option<_> = None::<u8>;\n\
         pub struct N<const M: usize>;\n\
         pub fn konst(_n: N<_>) {{}}\n\
         pub fn bound<T>(_t: T) where Option<_>: Copy {{}}\n\
         fn main() {{}}\n"
    );
    let out = stderr(&source);
    for note in [
        "a return type is chosen by the function, not the caller",
        "a field type cannot have an anonymous parameter",
        "a constant or static has no parameters",
        "a `_` const argument is not an anonymous const parameter",
        "a `_` is an anonymous type parameter only in a function's parameter types or an impl's \
         self type or trait",
    ] {
        assert!(out.contains(note), "{note}\n{out}");
    }
    assert!(out.contains("error[E0121]"), "{out}");
}

/// Without the gate a `_` in a parameter type is rustc's E0121, with nothing of blaze's.
#[test]
fn without_the_gate_a_placeholder_is_rustc_s_error() {
    let out = stderr(&format!(
        "{ALLOC}pub fn first<T: Copy>(v: &V<T, _>) -> T {{ v.0 }}\nfn main() {{}}\n"
    ));
    assert!(
        out.contains(
            "error[E0121]: the placeholder `_` is not allowed within types on item signatures \
             for functions"
        ),
        "{out}"
    );
    assert!(!out.contains("blaze_anon_params"), "{out}");
}

/// A trait with an associated type, and a struct declaring it on its parameter.
const ITEMS: &str = r#"
pub trait It {
    type Item;
    fn next(&mut self) -> Option<Self::Item>;
}
pub struct Wrapper<I: It>(pub I);
"#;

/// `I::Item` is resolved through the bound `Wrapper<I>` implies on `I`, in a function's
/// return type; without the gate `I` has no bound to find `Item` in.
#[test]
fn a_shorthand_projection_reads_the_bounds_a_function_s_header_implies() {
    let sum = "pub fn sum<I>(mut it: Wrapper<I>) -> Option<I::Item> { it.0.next() }\n\
               fn main() {}\n";
    accepted("", &format!("#![feature(blaze_implied_bounds)]\n{ITEMS}{sum}"));
    let out = messages(&format!("{ITEMS}{sum}"));
    let not_found = "E0220: associated type `Item` not found for `I`";
    assert!(out.iter().any(|it| it == not_found), "{out:#?}");
}

/// The same through an impl's self type: its method's `I::Item` reads the impl's header.
#[test]
fn a_shorthand_projection_reads_the_bounds_an_impl_s_header_implies() {
    let first = "impl<I> Wrapper<I> {\n\
                     pub fn first(&mut self) -> Option<I::Item> { self.0.next() }\n\
                 }\nfn main() {}\n";
    accepted("", &format!("#![feature(blaze_implied_bounds)]\n{ITEMS}{first}"));
    let out = messages(&format!("{ITEMS}{first}"));
    let not_found = "E0220: associated type `Item` not found for `I`";
    assert!(out.iter().any(|it| it == not_found), "{out:#?}");
}

/// Two implied bounds whose traits each define `Item` are the ambiguity two written ones are.
#[test]
fn two_implied_bounds_defining_the_name_are_ambiguous() {
    let source = "#![feature(blaze_implied_bounds)]\n\
                  pub trait P { type Item; }\npub trait Q { type Item; }\n\
                  pub struct Wp<I: P>(pub I);\npub struct Wq<I: Q>(pub I);\n\
                  pub fn f<I>(_p: Wp<I>, _q: Wq<I>) -> I::Item { loop {} }\n\
                  fn main() {}\n";
    let out = messages(source);
    let ambiguous = "E0221: ambiguous associated type `Item` in bounds of `I`";
    assert!(out.iter().any(|it| it == ambiguous), "{out:#?}");
}

/// `C`'s bound on `A` names `B`, whose argument is written with the shorthand being resolved:
/// a cycle, refused as rustc refuses `fn f<A: Tr<A::Out>>`, never a silent accept.
#[test]
fn a_shorthand_in_the_argument_its_own_bound_needs_is_a_cycle() {
    let source = "#![feature(blaze_implied_bounds)]\n\
                  pub trait Tr<B> { type Out; }\n\
                  pub struct C<A: Tr<B>, B>(pub A, pub B);\n\
                  pub fn f<A>(_c: C<A, A::Out>) {}\n\
                  fn main() {}\n";
    let out = messages(source);
    let cycle = "E0391: cycle detected when computing the bounds for type parameter `A`";
    assert!(out.iter().any(|it| it == cycle), "{out:#?}");
}
