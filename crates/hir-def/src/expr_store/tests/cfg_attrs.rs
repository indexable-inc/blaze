//! Lowering's cfg fast path ([`may_have_attrs`]) skips the attribute walk only for nodes with no
//! attribute of their own.

use span::Edition;
use syntax::{
    SourceFile,
    view::{self as ast, AstNode},
};

use crate::expr_store::lower::may_have_attrs;

/// Attributes, outer and inner, on every kind of node lowering checks the cfg of, and doc
/// comments, beside the same kinds without them.
const CODE: &str = r#"
#![allow(unused)]
fn f(#[cfg(a)] x: u32, y: u32, z: u32) -> u32 {
    #![cfg_attr(b, allow(unused))]
    #[cfg(c)]
    let w = 1;
    let v = 1;
    #[cfg(d)] g();
    g();
    let s = S { #[cfg(e)] a: 1, b: 2 };
    let S { #[cfg(f)] a, b } = s;
    let t = (#[cfg(g)] 1, 2);
    let a = [#[cfg(h)] 1, 2];
    let m = match y {
        #[cfg(i)]
        0 => 1,
        _ => #[cfg(j)] { 2 },
    };
    let c = #[cfg(k)] |u: u32| u;
    'l: {
        #![cfg(m)]
    }
    unsafe {
        #![allow(unused)]
    }
    /// A doc comment.
    let d = 1;
    asm!("", #[cfg(n)] in(reg) x, in(reg) z);
    #[cfg(o)]
    macro_rules! mac { () => {} }
    #[cfg(p)] y + 1
}

fn g<#[cfg(q)] T, #[cfg(r)] 'a, #[cfg(s)] const N: usize, U>(#[cfg(t)] self_: u32, plain: u32) {}
"#;

#[test]
fn a_node_with_an_attribute_is_never_skipped() {
    let parse = SourceFile::parse(CODE, Edition::CURRENT);
    let (mut with, mut skipped) = (0, 0);
    for node in parse.root().descendants() {
        let Some(owner) = ast::AnyHasAttrs::cast(node) else { continue };
        let has = ast::attrs_including_inner(&owner).next().is_some();
        let may = may_have_attrs(&owner);
        assert!(may || !has, "{:?} has an attribute and is skipped", owner.syntax());
        with += usize::from(has);
        skipped += usize::from(!may);
    }
    assert!(with >= 15, "the fixture has attributes in every position, found {with}");
    assert!(skipped >= 20, "most nodes take the fast path, {skipped} did");
}
