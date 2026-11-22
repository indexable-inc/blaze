//! Syntax of the pinned nightly that `core` uses and the parser once refused: `final fn` in a
//! trait (core/src/clone.rs, core/src/io/size_hint.rs), a const closure in statement position
//! (core/src/iter/traits/iterator.rs), and `const impl` / `impl const` headers.

use super::{has_run, parse_lines};
use crate::Edition;

fn clean(text: &str) -> Vec<String> {
    let (tree, errors) = parse_lines(text, Edition::CURRENT);
    assert_eq!(errors, Vec::<String>::new(), "{text}: {tree:#?}");
    tree
}

#[test]
fn a_final_method_is_a_fn() {
    let tree = clean("trait Share: Clone { final fn share(&self) -> Self { self.clone() } }");
    assert!(has_run(&tree, &["FN", "FINAL_KW \"final\""]), "{tree:#?}");
    let tree = clean("trait T { final const unsafe fn f(); }");
    assert!(has_run(&tree, &["FN", "FINAL_KW \"final\""]), "{tree:#?}");
}

#[test]
fn a_const_closure_is_an_expression_where_a_statement_is() {
    for text in [
        "fn f() -> impl Fn() { const move |(), x| { g(x) } }",
        "fn f() { const |x| x; }",
        "fn f() { let c = 1; const || c }",
    ] {
        let tree = clean(text);
        assert!(tree.iter().any(|it| it == "CLOSURE_EXPR"), "{text}: {tree:#?}");
    }
    // A const item in a body is still an item.
    let tree = clean("fn f() { const X: u8 = 1; }");
    assert!(tree.iter().any(|it| it == "CONST"), "{tree:#?}");
}

/// The expansions of `core`'s `fmt_refs!`, `bitop_impls!` and `impl_number!` as the pipeline
/// example writes them (each joint token glued to the next), which the outline refused.
#[test]
fn cores_expansions_parse_as_items() {
    for text in [
        "impl<T: PointeeSized + Debug> Debug for &T { fn fmt(&self, f: &mut Formatter<'_>) -> Result { Debug::fmt(&**self, f) } }",
        "const impl BitAndAssign<&'_ Ipv4Addr> for Ipv4Addr { fn bitand_assign(&mut self, rhs: &'_ Ipv4Addr) { self.bitand_assign(*rhs); } }",
        "const impl BitAndAssign for Ipv4Addr { fn bitand_assign(&mut self, rhs: Ipv4Addr) { let mut idx = 0; while idx < self.octets.len() { self.octets[idx].bitand_assign(rhs.octets[idx]); idx += 1; } } }",
        "impl<const N: usize> SimdPartialEq for Simd<f16, N> { type Mask = Mask<<f16 as SimdElement>::Mask, N>; }",
    ] {
        clean(text);
    }
}

#[test]
fn const_trait_impl_headers_parse() {
    for text in [
        "const impl<T: ?Sized> Borrow<T> for T { fn borrow(&self) -> &T { self } }",
        "impl<T> const Default for W<T> {}",
        "unsafe impl const Send for S {}",
    ] {
        let tree = clean(text);
        assert!(tree.iter().any(|it| it == "IMPL"), "{text}: {tree:#?}");
    }
}
