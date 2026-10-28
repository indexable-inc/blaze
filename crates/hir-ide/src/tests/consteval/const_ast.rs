//! `const_ast`'s views (docs/future/const-macros.md, stage 2) run by the const evaluator: what a
//! derive reads of a file through them, folded into one number (`const_ast_digest.rs`), is the
//! same evaluated as compiled, on real files and on one with every kind of node the views read.

use syntax::{AstNode, Edition, SourceFile};

use super::check_number;

#[path = "const_ast_digest.rs"]
mod digest;

const LIB: &str = include_str!("../../../../const-ast/src/lib.rs");
const KINDS: &str = include_str!("../../../../const-ast/src/kinds.rs");
const VIEWS: &str = include_str!("../../../../const-ast/src/views.rs");
const DIGEST: &str = include_str!("const_ast_digest.rs");

/// Every kind of node the views read.
const EVERY_VIEW: &str = r#"
#![allow(dead_code)]
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[unsafe(no_mangle)]
#[doc = "a doc"]
/// A doc comment.
pub(crate) struct Record<'a, T: Copy + 'a, const N: usize = 4>
where
    T: Default,
    'a: 'static,
{
    pub x: &'a mut T,
    pub(in crate::a) y: [u8; N],
    z: *const (u8, i32),
    w: Option<Vec<std::collections::HashMap<u8, &'static str>>> = None,
    s: [T],
}
pub struct Tuple(pub u8, #[allow(unused)] (), fn(u8) -> u8);
struct Unit;
enum E<T> { A, B(T, u8), C { x: u8 } = 3, #[cfg(test)] D }
union U { a: u32, b: f32 }
fn f() {}
"#;

/// The arrays of the flat tree of `text`, as `const_ast::Tree::new` takes them, and the same
/// written as Rust.
fn arrays(text: &str) -> (Vec<u16>, Vec<u32>, Vec<u32>, Vec<String>) {
    let root = SourceFile::parse(text, Edition::CURRENT).tree().syntax().to_flat();
    let (tree, _) = root.flat().expect("a flat tree");
    let (mut kinds, mut ends, mut spans, mut texts) = (vec![], vec![], vec![], vec![]);
    for at in 0..tree.len() {
        let token = tree.is_token(at);
        kinds.push(tree.kind(at).0 | if token { const_ast::TOKEN } else { 0 });
        ends.push(tree.end(at));
        // Any number stands for a span here; the element's own checks they are read back.
        spans.push(if token { at * 7 % 1000 } else { const_ast::NO_SPAN });
        texts.push(if token { tree.token_text(at).to_owned() } else { String::new() });
    }
    (kinds, ends, spans, texts)
}

/// The digest of `text` evaluated, against the digest compiled.
fn check(text: &str) {
    let (kinds, ends, spans, texts) = arrays(text);
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let tree = const_ast::Tree::new(&kinds, &ends, &spans, &refs);
    let compiled = digest::digest(&tree);
    assert_ne!(compiled, 0, "the file has items");
    let list = |it: &mut dyn Iterator<Item = String>| it.collect::<Vec<_>>().join(", ");
    let fixture = format!(
        "//- minicore: sized, copy, clone, derive, option, result, slice, index, coerce_unsized, str, iterator, panic
//- /const_ast/lib.rs crate:const_ast
{LIB}
//- /const_ast/kinds.rs
{KINDS}
//- /const_ast/views.rs
{VIEWS}
//- /main.rs crate:main deps:const_ast
mod digest {{
{DIGEST}
}}
const KINDS: &[u16] = &[{}];
const ENDS: &[u32] = &[{}];
const SPANS: &[u32] = &[{}];
const TEXTS: &[&str] = &[{}];
const TREE: const_ast::Tree<'static> = const_ast::Tree::new(KINDS, ENDS, SPANS, TEXTS);
const GOAL: u64 = digest::digest(&TREE);
",
        list(&mut kinds.iter().map(|it| it.to_string())),
        list(&mut ends.iter().map(|it| it.to_string())),
        list(&mut spans.iter().map(|it| it.to_string())),
        list(&mut texts.iter().map(|it| format!("{it:?}"))),
    );
    check_number(&fixture, compiled as i128);
}

#[test]
fn every_view_evaluates_as_compiled() {
    check(EVERY_VIEW);
}

#[test]
fn a_real_file_evaluates_as_compiled() {
    check(include_str!("../../../../cfg/src/cfg_expr.rs"));
    check(include_str!("../../../../span/src/lib.rs"));
}
