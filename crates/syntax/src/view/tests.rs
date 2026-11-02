//! The view extension layer against its handle twin: for every ported method, on the same
//! parses, the view gives the node, token, text or value the handle gives.

use crate::{
    AstNode as _, AstToken as _, Edition, Parse, SourceFile, SyntaxKind, SyntaxNode, SyntaxToken,
    Text, TextRange, TextSize,
    unescape::EscapeError,
    ast::{
        self, HasArgList as _, HasAttrs as _, HasLoopBody as _, HasModuleItem as _, HasName as _,
        IsString as _, RangeItem as _,
    },
    view::{
        self, AstNode as _, AstToken as _, HasArgList as _, HasAttrs as _, HasLoopBody as _,
        HasModuleItem as _, HasName as _, IsString as _, RangeItem as _,
    },
};

const SOURCE: &str = r##"
//! inner doc
#![allow(unused)]
/// outer doc
/** block doc */
// plain comment
/* block comment */
#[cfg_attr(feature = "a", derive(Debug), cfg_attr(test, inline, must_use))]
#[path = "x.rs"]
#[unsafe(no_mangle)]
#[cfg(all(unix, not(test), true, false))]
#[doc(hidden)]
#[a::b]
pub(crate) struct S<'a, T: Clone + 'a, const N: usize> where T: Copy { a: &'a T, pub(super) b: [u8; N] }
pub(self) struct U(i32, pub(in crate::m) u8);
struct Unit;
pub union Un { x: u32, y: f32 }
enum E { A, B(u8), C { x: i32 } }
macro_rules! m { ($e:expr) => { $e }; }
macro m2($x:ident) { $x }
macro m3 { () => {} }
extern "C" { fn ext(x: i32); static mut EXT: u8; type Opaque; }
extern crate core as c;
mod outer { mod inner { mod deeper {} } }
use std::{self, collections::{HashMap, hash_map::{self, Entry}}, io::*};
use crate::a::b::*;
use super::{x};
use self::y::{/* inner */ z};
const trait Tr { const C: u8; fn f(&self); type T; }
impl<T> Tr for S<'_, T, 1> {}
impl S<'static, u8, 2> { fn new(self, mut x: Self) -> Self { x } fn g(&mut self) {} fn h(&self) {} }
impl !Send for Unit {}
impl<T> <T as Tr>::T {}
fn generic<'a, 'b: 'a + 'static, T: Iterator<Item = u8> + ?Sized + for<'c> Fn(&'c u8) + use<'a, T>, const C: bool>(x: impl Tr + 'a) -> Box<dyn Tr + 'b> where for<'d> &'d T: Copy { loop {} }
fn qualified() { <Vec<u8> as IntoIterator>::into_iter; <u8>::max; crate::a::b::c(); self::d; super::e; Self::f; a::<u8>::b::<'a, u16>(); }
static ST: u8 = 1;
const CO: u8 = { #[attr] 5 };
fn exprs(a: i32, b: &mut [u8]) {
    let s = S { a, b: [0; 2], c: d, 0: e };
    let S { a, b: ref mut c, box d, 0: f, .. } = s;
    let r = 1..2; let r = ..=3; let r = ..; let r = 4..;
    match a { 0 | 1 => {}, | 2 => {}, 3..=5 => {}, 6..9 => {}, ..=10 => {}, 11..=12 => {}, x if x > 0 => {}, _ => {} }
    let [first, rest @ .., last] = [1, 2, 3, 4];
    let [p, ref q @ .., z] = b; let [&.., w] = c; let [.., y] = d; let [k, l] = e;
    a += 1; a -= 1; a *= 2; a /= 2; a %= 2; a <<= 1; a >>= 1; a ^= 1; a |= 1; a &= 1; a = 3;
    let _ = a + b * c - d / e % f << g >> h ^ i | j & k == l && m != n || o < p && q > r || s <= t && u >= v;
    let _ = -a; let _ = !b; let _ = *c; let _ = &d; let _ = &mut e; let _ = #[cfg(all())] !b;
    let _ = x.0; let _ = x.y; let _ = x.1.2; let _ = x.f(1); let _ = (x.f)(); let _ = f(1, , 2,); let _ = g(,); let _ = x[0];
    let _ = [1, 2, 3]; let _ = [0u8; 16]; let _ = [];
    let _ = if a { 1 } else if b { 2 } else { 3 };
    let _ = if { true } { 1 } else { 2 };
    let _ = if if a { b } else { c } { 1 };
    while a { } while let Some(x) = y { } while { a } { } for x in it { } for x in { it } { } 'l: loop { break 'l; }
    let _ = async { }; let _ = unsafe { }; let _ = try { }; let _ = const { }; let _ = async gen { }; let _ = gen { }; let _ = 'b: { };
    { #![allow(unused)] }
    let _ = |x| x + 1; let _ = |x| -> u8 { x }; let _ = move || return;
    let _ = "str\n\x41"; let _ = "plain"; let _ = "bad \q"; let _ = r"raw\n"; let _ = r#"raw "hash""#;
    let _ = b"bytes\xff"; let _ = br"raw bytes"; let _ = c"cstr\u{1F600}\xff"; let _ = cr"raw c";
    let _ = "line \
             continued";
    let _ = 'c'; let _ = '\n'; let _ = '\u{41}'; let _ = b'b'; let _ = b'\x7f'; let _ = true; let _ = false;
    let _ = 0; let _ = 1_000u32; let _ = 0xff_u8; let _ = 0o17; let _ = 0b1010_1010i64; let _ = 0xABCDEFi128;
    let _ = 340282366920938463463374607431768211455; let _ = 340282366920938463463374607431768211456;
    let _ = 0x; let _ = 0b; let _ = 0b102; let _ = 0o9;
    let _ = 1.0; let _ = 1e10; let _ = 2.5E-3f64; let _ = 1_2.3_4f32; let _ = 3.0e; let _ = 4.0ef16;
    let _ = x as u8 < 3; let _ = x as T << 3; let _ = return; let _ = break; let _ = x?; let _ = y.await; let _ = (1, 2); let _ = (1);
    let _ = format_args!("{}", a); let _ = builtin # offset_of(S, a); let _ = builtin # format_args("{}", a, b = 1);
    m!(a b [c] {d} (e));
    let Some(x) = y else { return };
    if return {} {}
    if S {} == 1 {}
    if a && let Some(b) = c {}
    let _ = (return) || x;
    let _ = - - a; let _ = (a + b) * c; let _ = a + (b * c); let _ = (a.b)(); let _ = (a)(); let _ = &(a + b); let _ = (&a).b;
    ({ 0 } + 1);
    yield; do yeet; become f();
}
fn not_simple(ref mut a: u8, b @ 1: u8, mut c: u8) {}
fn param<'a: 'b + 'c, 'b>() {}
type Ty = (dyn A + Send, &dyn B, *const impl C, fn(u8) -> u8);
"##;

type Key = (SyntaxKind, TextRange);
type Shape = (u8, Vec<Option<Key>>);

fn hn<N: ast::AstNode>(n: &N) -> Key {
    (n.syntax().kind(), n.syntax().text_range())
}

fn vn<'t, N: view::AstNode<'t>>(n: &N) -> Key {
    let syntax = n.syntax();
    (syntax.kind(), syntax.text_range())
}

fn ht(t: &SyntaxToken) -> Key {
    (t.kind(), t.text_range())
}

fn vt(t: &view::Token<'_>) -> Key {
    (t.kind(), t.text_range())
}

fn htk<T: ast::AstToken>(t: &T) -> Key {
    ht(t.syntax())
}

fn vtk<'t, T: view::AstToken<'t>>(t: &T) -> Key {
    vt(&t.syntax())
}

fn hs<N: ast::AstNode>(it: impl IntoIterator<Item = N>) -> Vec<Key> {
    it.into_iter().map(|it| hn(&it)).collect()
}

fn vs<'t, N: view::AstNode<'t>>(it: impl IntoIterator<Item = N>) -> Vec<Key> {
    it.into_iter().map(|it| vn(&it)).collect()
}

fn hts(it: impl IntoIterator<Item = SyntaxToken>) -> Vec<Key> {
    it.into_iter().map(|it| ht(&it)).collect()
}

fn vts<'t>(it: impl IntoIterator<Item = view::Token<'t>>) -> Vec<Key> {
    it.into_iter().map(|it| vt(&it)).collect()
}

fn ho<N: ast::AstNode>(n: Option<N>) -> Option<Key> {
    n.as_ref().map(hn)
}

fn vo<'t, N: view::AstNode<'t>>(n: Option<N>) -> Option<Key> {
    n.as_ref().map(vn)
}

fn hto(t: Option<SyntaxToken>) -> Option<Key> {
    t.as_ref().map(ht)
}

fn vto(t: Option<view::Token<'_>>) -> Option<Key> {
    t.as_ref().map(vt)
}

struct Fixture {
    parse: Parse<SourceFile>,
}

impl Fixture {
    fn new(text: &str) -> Fixture {
        Fixture { parse: SourceFile::parse(text, Edition::CURRENT) }
    }

    fn source() -> Fixture {
        Fixture::new(SOURCE)
    }

    fn handle(&self) -> SyntaxNode {
        self.parse.syntax_node()
    }

    fn view(&self) -> view::Node<'_> {
        self.parse.root()
    }

    /// Every node that is an `H` and its view twin, in preorder; the two walks find the same.
    fn nodes<'t, H: ast::AstNode, V: view::AstNode<'t>>(&'t self) -> Vec<(H, V)> {
        let handles: Vec<H> = self.handle().descendants().filter_map(H::cast).collect();
        let views: Vec<V> = self.view().descendants().filter_map(V::cast).collect();
        assert_eq!(
            handles.iter().map(hn).collect::<Vec<_>>(),
            views.iter().map(vn).collect::<Vec<_>>()
        );
        handles.into_iter().zip(views).collect()
    }

    /// Every token that is an `H` and its view twin, in order.
    fn tokens<'t, H: ast::AstToken, V: view::AstToken<'t>>(&'t self) -> Vec<(H, V)> {
        let handles: Vec<H> = self
            .handle()
            .descendants_with_tokens()
            .filter_map(|it| it.into_token())
            .filter_map(H::cast)
            .collect();
        let views: Vec<V> = self
            .view()
            .descendants_with_tokens()
            .filter_map(|it| it.into_token())
            .filter_map(V::cast)
            .collect();
        assert_eq!(
            handles.iter().map(htk).collect::<Vec<_>>(),
            views.iter().map(vtk).collect::<Vec<_>>()
        );
        handles.into_iter().zip(views).collect()
    }
}

/// Fails when a fixture has none of a node kind a test means to exercise.
fn some<T>(pairs: Vec<T>) -> Vec<T> {
    assert!(!pairs.is_empty(), "the fixture has none of these");
    pairs
}

// region: node_ext

#[test]
fn an_untyped_parse_keeps_its_tree_and_casts_as_the_handle_does() {
    let parse = SourceFile::parse(SOURCE, Edition::CURRENT);
    let errors = parse.errors();
    let handle = parse.syntax_node();
    let untyped = parse.into_untyped();
    assert_eq!(
        hn(&ast::SourceFile::cast(handle.clone()).unwrap()),
        vn(&view::SourceFile::cast(untyped.root()).unwrap())
    );
    assert_eq!(untyped.errors(), errors);
    let other = SourceFile::parse(SOURCE, Edition::CURRENT).into_untyped();
    assert!(other.cast::<ast::Fn>().is_none());
    let file = untyped.cast::<ast::SourceFile>().expect("the root is a source file");
    assert_eq!(file.syntax_node(), handle);
    assert_eq!(vn(&file.view()), hn(&ast::SourceFile::cast(handle).unwrap()));
}

#[test]
fn either_and_macro_pointers_view_their_nodes() {
    let f = Fixture::source();
    type Handle = either::Either<ast::Fn, ast::Struct>;
    type View<'t> = either::Either<view::Fn<'t>, view::Struct<'t>>;
    for (h, v) in some(f.nodes::<Handle, View<'_>>()) {
        let found = crate::AstPtr::new(&h).to_view(f.view());
        assert_eq!(vn(&found), vn(&v));
        assert_eq!(found.is_left(), h.is_left());
    }
    for (h, v) in some(f.nodes::<ast::Macro, view::Macro<'_>>()) {
        let found = crate::AstPtr::new(&h).to_view(f.view());
        assert_eq!(vn(&found), vn(&v));
        assert_eq!(
            matches!(found, view::Macro::MacroRules(_)),
            matches!(h, ast::Macro::MacroRules(_))
        );
    }
}

#[test]
fn an_items_written_range_leaves_out_its_attributes_and_comments() {
    let text = "/// d\n#[a]\n// c\npub fn f() {}";
    let f = Fixture::new(text);
    let (_, item) = some(f.nodes::<ast::Fn, view::Fn<'_>>()).remove(0);
    let start = TextSize::try_from(text.find("pub").unwrap()).unwrap();
    let end = item.syntax().text_range().end();
    assert_eq!(view::written_range(item.syntax()), TextRange::new(start, end));

    let range = TextRange::new(TextSize::from(2), TextSize::from(9));
    let attrs = [(SyntaxKind::ATTR, TextRange::new(TextSize::from(2), TextSize::from(6)))];
    assert_eq!(view::written_range_of(range, attrs), range);

    let f = Fixture::source();
    for (h, v) in f.handle().descendants().zip(f.view().descendants()) {
        let of_handle = view::written_range_of(
            h.text_range(),
            h.children_with_tokens().map(|it| (it.kind(), it.text_range())),
        );
        assert_eq!(view::written_range(v), of_handle, "{:?}", v.kind());
    }
}

/// blaze-diagnostics' and blaze-lints' `signature_range` before it moved here, on handles.
fn signature_range_reference(item: &ast::Fn) -> TextRange {
    let node = item.syntax();
    let range = view::written_range_of(
        node.text_range(),
        node.children_with_tokens().map(|it| (it.kind(), it.text_range())),
    );
    let end = match item.body() {
        Some(body) => {
            let mut end = body.syntax().text_range().start();
            let mut at = body.syntax().prev_sibling_or_token();
            while let Some(it) = at {
                if !it.kind().is_trivia() {
                    end = it.text_range().end();
                    break;
                }
                at = it.prev_sibling_or_token();
            }
            end
        }
        None => range.end(),
    };
    TextRange::new(range.start(), end.max(range.start()))
}

#[test]
fn a_fns_signature_range_ends_before_its_body() {
    let text = "/// d\n#[a]\npub fn f(x: u8) -> u8 /* c */ { x }\nfn g();";
    let f = Fixture::new(text);
    let fns = f.nodes::<ast::Fn, view::Fn<'_>>();
    assert_eq!(fns.len(), 2);
    let at = |s: &str| TextSize::try_from(text.find(s).unwrap()).unwrap();
    let ret_end = at(" /* c */");
    assert_eq!(view::signature_range(fns[0].1), TextRange::new(at("pub"), ret_end));
    assert_eq!(view::signature_range(fns[1].1), fns[1].1.syntax().text_range());

    for f in [Fixture::source(), f] {
        for (h, v) in f.nodes::<ast::Fn, view::Fn<'_>>() {
            assert_eq!(view::signature_range(v), signature_range_reference(&h), "{v:?}");
        }
    }
}

/// hir-expand's derive `type_param_span` before it moved here, on handles.
fn type_param_span_reference(param: &ast::TypeParam) -> Option<TextRange> {
    let name = param.name()?.syntax().text_range();
    Some(match param.default_type() {
        Some(ty) => name.cover(ty.syntax().text_range()),
        None => name,
    })
}

/// blaze-diagnostics' `type_param_range` before it moved here.
fn type_param_range_reference(param: view::TypeParam<'_>) -> Option<TextRange> {
    let name = param.name()?.syntax().text_range();
    Some(match param.default_type() {
        Some(default) => TextRange::new(name.start(), default.syntax().text_range().end()),
        None => name,
    })
}

#[test]
fn a_type_params_span_is_its_name_to_the_end_of_its_default() {
    let text = "struct S<#[a] T: Copy = u8, U, V = Vec<T>>(T, U, V);";
    let f = Fixture::new(text);
    let params = f.nodes::<ast::TypeParam, view::TypeParam<'_>>();
    assert_eq!(params.len(), 3);
    let at = |s: &str| TextSize::try_from(text.find(s).unwrap()).unwrap();
    let range = |from: &str, to: &str| TextRange::new(at(from), at(to) + TextSize::of(to));
    assert_eq!(view::type_param_span(params[0].1), Some(range("T: Copy", "u8")));
    assert_eq!(view::type_param_span(params[1].1), Some(range("U,", "U")));
    assert_eq!(view::type_param_span(params[2].1), Some(range("V =", "Vec<T>")));

    for f in [Fixture::source(), f] {
        for (h, v) in f.nodes::<ast::TypeParam, view::TypeParam<'_>>() {
            assert_eq!(view::type_param_span(v), type_param_span_reference(&h), "{v:?}");
            assert_eq!(view::type_param_span(v), type_param_range_reference(v), "{v:?}");
        }
    }
}

#[test]
fn peel_parens_leaves_the_expression_inside_every_pair() {
    let f = Fixture::new("fn f() { ((a)); (a).b; (); }");
    let peeled: Vec<_> = f
        .view()
        .descendants()
        .filter_map(view::ExprStmt::cast)
        .filter_map(|it| it.expr())
        .map(|it| it.peel_parens())
        .collect();
    let [a, field, unit] = peeled[..] else { panic!("three statements, got {}", peeled.len()) };
    assert!(matches!(a, view::Expr::PathExpr(_)) && a.syntax().text() == "a");
    assert!(matches!(field, view::Expr::FieldExpr(_)));
    assert!(matches!(unit, view::Expr::TupleExpr(_)));
}

#[test]
fn names_and_lifetimes() {
    let f = Fixture::source();
    for (h, v) in some(f.nodes::<ast::Name, view::Name<'_>>()) {
        assert_eq!(h.text(), v.text());
    }
    for (h, v) in some(f.nodes::<ast::Lifetime, view::Lifetime<'_>>()) {
        assert_eq!(h.text(), v.text());
    }
    for (h, v) in some(f.nodes::<ast::NameRef, view::NameRef<'_>>()) {
        assert_eq!(h.text(), v.text());
        assert_eq!(h.as_tuple_field(), v.as_tuple_field());
        assert_eq!(h.token_kind(), v.token_kind());
        assert_eq!(
            ho(ast::Impl::for_trait_name_ref(&h)),
            vo(view::Impl::for_trait_name_ref(v))
        );
        assert_eq!(
            ho(ast::RecordExprField::for_field_name(&h)),
            vo(view::RecordExprField::for_field_name(v))
        );
        assert_eq!(
            ho(ast::RecordExprField::for_name_ref(&h)),
            vo(view::RecordExprField::for_name_ref(v))
        );
        assert_eq!(
            ho(ast::RecordPatField::for_field_name_ref(&h)),
            vo(view::RecordPatField::for_field_name_ref(v))
        );
    }
    for (h, v) in some(f.nodes::<ast::Name, view::Name<'_>>()) {
        assert_eq!(
            ho(ast::RecordPatField::for_field_name(&h)),
            vo(view::RecordPatField::for_field_name(v))
        );
    }
}

#[test]
fn name_likes() {
    let f = Fixture::source();
    for (h, v) in some(f.nodes::<ast::NameLike, view::NameLike<'_>>()) {
        assert_eq!(h.text(), v.text());
        assert_eq!(h.as_name_ref().map(hn), v.as_name_ref().as_ref().map(vn));
        assert_eq!(h.as_lifetime().map(hn), v.as_lifetime().as_ref().map(vn));
    }
    for (h, v) in some(f.nodes::<ast::NameOrNameRef, view::NameOrNameRef<'_>>()) {
        assert_eq!(h.text(), v.text());
        assert_eq!(h.to_string(), v.to_string());
        let tag = |name: bool| name as u8;
        assert_eq!(
            tag(matches!(h, ast::NameOrNameRef::Name(_))),
            tag(matches!(v, view::NameOrNameRef::Name(_)))
        );
    }
}

#[test]
fn items() {
    let f = Fixture::source();
    for (h, v) in some(f.nodes::<ast::Abi, view::Abi<'_>>()) {
        assert_eq!(h.abi_string().as_ref().map(htk), v.abi_string().as_ref().map(vtk));
    }
    for (h, v) in some(f.nodes::<ast::StmtList, view::StmtList<'_>>()) {
        assert_eq!(hs(h.items()), vs(v.items()));
    }
    for (h, v) in some(f.nodes::<ast::Macro, view::Macro<'_>>()) {
        assert_eq!(ho(h.name()), vo(v.name()));
        assert_eq!(hs(h.attrs()), vs(v.attrs()));
    }
    for (h, v) in some(f.nodes::<ast::AssocItem, view::AssocItem<'_>>()) {
        assert_eq!(hn(&ast::Item::from(h.clone())), vn(&view::Item::from(v)));
        assert_eq!(hn(&ast::AnyHasAttrs::from(h)), vn(&view::AnyHasAttrs::from(v)));
    }
    for (h, v) in some(f.nodes::<ast::ExternItem, view::ExternItem<'_>>()) {
        assert_eq!(hn(&ast::Item::from(h)), vn(&view::Item::from(v)));
    }
    for (h, v) in some(f.nodes::<ast::Adt, view::Adt<'_>>()) {
        assert_eq!(hn(&ast::Item::from(h)), vn(&view::Item::from(v)));
    }
    for (h, v) in some(f.nodes::<ast::Item, view::Item<'_>>()) {
        assert_eq!(ho(h.generic_param_list()), vo(v.generic_param_list()));
        assert_eq!(hn(&ast::AnyHasAttrs::from(h)), vn(&view::AnyHasAttrs::from(v)));
    }
    for (h, v) in some(f.nodes::<ast::Impl, view::Impl<'_>>()) {
        assert_eq!(ho(h.self_ty()), vo(v.self_ty()));
        assert_eq!(ho(h.trait_()), vo(v.trait_()));
    }
    for (h, v) in some(f.nodes::<ast::Trait, view::Trait<'_>>()) {
        assert_eq!(hto(h.const_token()), vto(v.const_token()));
    }
    for (h, v) in some(f.nodes::<ast::Module, view::Module<'_>>()) {
        assert_eq!(ho(h.parent()), vo(v.parent()));
    }
    for (h, v) in some(f.nodes::<ast::Use, view::Use<'_>>()) {
        assert_eq!(h.is_simple_glob(), v.is_simple_glob());
    }
    for (h, v) in some(f.nodes::<ast::UseTree, view::UseTree<'_>>()) {
        assert_eq!(h.is_simple_path(), v.is_simple_path());
        assert_eq!(ho(h.parent_use_tree_list()), vo(v.parent_use_tree_list()));
        assert_eq!(hn(&h.top_use_tree()), vn(&v.top_use_tree()));
    }
    for (h, v) in some(f.nodes::<ast::UseTreeList, view::UseTreeList<'_>>()) {
        assert_eq!(hn(&h.parent_use_tree()), vn(&v.parent_use_tree()));
        assert_eq!(h.has_inner_comment(), v.has_inner_comment());
        assert_eq!(hts(h.comma()), vts(v.comma()));
    }
}

#[test]
fn fn_bodies() {
    let f = Fixture::source();
    for (h, v) in some(f.nodes::<ast::Fn, view::Fn<'_>>()) {
        assert_eq!(h.body_is_lazy(), v.body_is_lazy());
        assert_eq!(ho(h.body()), vo(v.body()));
    }
    for (h, v) in some(f.nodes::<ast::ClosureExpr, view::ClosureExpr<'_>>()) {
        assert_eq!(ho(h.body()), vo(v.body()));
    }
    for (h, v) in some(f.nodes::<ast::Const, view::Const<'_>>()) {
        assert_eq!(ho(h.body()), vo(v.body()));
    }
    for (h, v) in some(f.nodes::<ast::Static, view::Static<'_>>()) {
        assert_eq!(ho(h.body()), vo(v.body()));
    }

    let text = "fn f() { let x = 1; }\nfn g() { #![allow(unused)] let y = 2; }\nfn h();\n";
    let outline = SourceFile::parse_outline(Text::from(text), Edition::CURRENT);
    let handles: Vec<ast::Fn> =
        outline.syntax_node().descendants().filter_map(ast::Fn::cast).collect();
    let views: Vec<view::Fn<'_>> =
        outline.root().descendants().filter_map(view::Fn::cast).collect();
    assert_eq!(handles.len(), 3);
    assert_eq!(views.len(), 3);
    for (h, v) in handles.iter().zip(&views) {
        assert_eq!(h.body_is_lazy(), v.body_is_lazy());
    }
    assert!(views[0].body_is_lazy() && !views[2].body_is_lazy());
}

#[test]
fn blocks() {
    let f = Fixture::source();
    for (h, v) in some(f.nodes::<ast::BlockExpr, view::BlockExpr<'_>>()) {
        assert_eq!(hs(h.statements()), vs(v.statements()));
        assert_eq!(ho(h.tail_expr()), vo(v.tail_expr()));
        assert_eq!(h.may_carry_attributes(), v.may_carry_attributes());
        assert_eq!(h.is_standalone(), v.is_standalone());
        assert_eq!(h.modifier().map(h_modifier), v.modifier().map(v_modifier));
    }
}

fn h_modifier(it: ast::BlockModifier) -> Shape {
    match it {
        ast::BlockModifier::Async(t) => (0, vec![Some(ht(&t))]),
        ast::BlockModifier::Unsafe(t) => (1, vec![Some(ht(&t))]),
        ast::BlockModifier::Try { try_token, bikeshed_token, result_type } => {
            (2, vec![Some(ht(&try_token)), hto(bikeshed_token), ho(result_type)])
        }
        ast::BlockModifier::Const(t) => (3, vec![Some(ht(&t))]),
        ast::BlockModifier::AsyncGen(t) => (4, vec![Some(ht(&t))]),
        ast::BlockModifier::Gen(t) => (5, vec![Some(ht(&t))]),
        ast::BlockModifier::Label(l) => (6, vec![Some(hn(&l))]),
    }
}

fn v_modifier(it: view::BlockModifier<'_>) -> Shape {
    match it {
        view::BlockModifier::Async(t) => (0, vec![Some(vt(&t))]),
        view::BlockModifier::Unsafe(t) => (1, vec![Some(vt(&t))]),
        view::BlockModifier::Try { try_token, bikeshed_token, result_type } => {
            (2, vec![Some(vt(&try_token)), vto(bikeshed_token), vo(result_type)])
        }
        view::BlockModifier::Const(t) => (3, vec![Some(vt(&t))]),
        view::BlockModifier::AsyncGen(t) => (4, vec![Some(vt(&t))]),
        view::BlockModifier::Gen(t) => (5, vec![Some(vt(&t))]),
        view::BlockModifier::Label(l) => (6, vec![Some(vn(&l))]),
    }
}

#[test]
fn metas_and_attrs() {
    let f = Fixture::source();
    for (h, v) in some(f.nodes::<ast::Meta, view::Meta<'_>>()) {
        assert_eq!(h.as_simple_atom().as_deref(), v.as_simple_atom());
        assert_eq!(
            h.as_simple_call().map(|(name, tt)| (name.to_string(), hn(&tt))),
            v.as_simple_call().map(|(name, tt)| (name.to_owned(), vn(&tt)))
        );
        assert_eq!(ho(h.as_simple_path()), vo(v.as_simple_path()));
        assert_eq!(h.simple_name().as_deref(), v.simple_name());
        assert_eq!(ho(h.path()), vo(v.path()));
        assert_eq!(hs(h.clone().skip_cfg_attrs()), vs(v.skip_cfg_attrs()));
        assert_eq!(ho(h.parent_attr()), vo(v.parent_attr()));
    }
    let attrs = some(f.nodes::<ast::Attr, view::Attr<'_>>());
    let mut flattened = 0;
    for (h, v) in attrs {
        assert_eq!(h.as_simple_atom().as_deref(), v.as_simple_atom());
        assert_eq!(
            h.as_simple_call().map(|(name, tt)| (name.to_string(), hn(&tt))),
            v.as_simple_call().map(|(name, tt)| (name.to_owned(), vn(&tt)))
        );
        assert_eq!(ho(h.as_simple_path()), vo(v.as_simple_path()));
        assert_eq!(h.simple_name().as_deref(), v.simple_name());
        assert_eq!(ho(h.path()), vo(v.path()));
        assert_eq!(h.kind(), v.kind());
        let metas = vs(v.skip_cfg_attrs());
        assert_eq!(hs(h.skip_cfg_attrs()), metas);
        flattened = flattened.max(metas.len());
    }
    // `cfg_attr(feature = "a", derive(Debug), cfg_attr(test, inline, must_use))`.
    assert!(flattened >= 3, "{flattened}");
    for (h, v) in some(f.nodes::<ast::DocComment, view::DocComment<'_>>()) {
        assert_eq!(h.kind(), v.kind());
        assert_eq!(htk(&h.token()), vtk(&v.token()));
        assert_eq!(h.shape(), v.shape());
        assert_eq!(h.text_with_markers(), v.text_with_markers());
        assert_eq!(h.text(), v.text());
        assert_eq!(ast::DocComment::PREFIX_LEN, view::DocComment::PREFIX_LEN);
    }
    for (h, v) in some(f.nodes::<ast::AnyAttr, view::AnyAttr<'_>>()) {
        assert_eq!(h.kind(), v.kind());
    }
    for (h, v) in some(f.nodes::<ast::CfgAtom, view::CfgAtom<'_>>()) {
        let hk = h.key().map(|it| match it {
            ast::CfgAtomKey::True => (0, None),
            ast::CfgAtomKey::False => (1, None),
            ast::CfgAtomKey::Ident(t) => (2, Some(ht(&t))),
        });
        let vk = v.key().map(|it| match it {
            view::CfgAtomKey::True => (0, None),
            view::CfgAtomKey::False => (1, None),
            view::CfgAtomKey::Ident(t) => (2, Some(vt(&t))),
        });
        assert_eq!(hk, vk);
    }
}

#[test]
fn paths() {
    let f = Fixture::source();
    for (h, v) in some(f.nodes::<ast::Path, view::Path<'_>>()) {
        assert_eq!(ho(h.parent_path()), vo(v.parent_path()));
        assert_eq!(ho(h.as_single_segment()), vo(v.as_single_segment()));
        assert_eq!(ho(h.as_single_name_ref()), vo(v.as_single_name_ref()));
        assert_eq!(hn(&h.first_qualifier_or_self()), vn(&v.first_qualifier_or_self()));
        assert_eq!(ho(h.first_qualifier()), vo(v.first_qualifier()));
        assert_eq!(ho(h.first_segment()), vo(v.first_segment()));
        assert_eq!(hs(h.segments()), vs(v.segments()));
        assert_eq!(hs(h.qualifiers()), vs(v.qualifiers()));
        assert_eq!(hn(&h.top_path()), vn(&v.top_path()));
    }
    for (h, v) in some(f.nodes::<ast::PathSegment, view::PathSegment<'_>>()) {
        assert_eq!(hn(&h.parent_path()), vn(&v.parent_path()));
        assert_eq!(hto(h.crate_token()), vto(v.crate_token()));
        assert_eq!(hto(h.self_token()), vto(v.self_token()));
        assert_eq!(hto(h.self_type_token()), vto(v.self_type_token()));
        assert_eq!(hto(h.super_token()), vto(v.super_token()));
        assert_eq!(ho(h.qualifying_trait()), vo(v.qualifying_trait()));
        let hk = h.kind().map(|it| match it {
            ast::PathSegmentKind::Name(it) => (0, vec![Some(hn(&it))]),
            ast::PathSegmentKind::Type { type_ref, trait_ref } => {
                (1, vec![ho(type_ref), ho(trait_ref)])
            }
            ast::PathSegmentKind::SelfTypeKw => (2, vec![]),
            ast::PathSegmentKind::SelfKw => (3, vec![]),
            ast::PathSegmentKind::SuperKw => (4, vec![]),
            ast::PathSegmentKind::CrateKw => (5, vec![]),
        });
        let vk = v.kind().map(|it| match it {
            view::PathSegmentKind::Name(it) => (0, vec![Some(vn(&it))]),
            view::PathSegmentKind::Type { type_ref, trait_ref } => {
                (1, vec![vo(type_ref), vo(trait_ref)])
            }
            view::PathSegmentKind::SelfTypeKw => (2, vec![]),
            view::PathSegmentKind::SelfKw => (3, vec![]),
            view::PathSegmentKind::SuperKw => (4, vec![]),
            view::PathSegmentKind::CrateKw => (5, vec![]),
        });
        assert_eq!(hk, vk);
    }
    for (h, v) in some(f.nodes::<ast::Visibility, view::Visibility<'_>>()) {
        assert_eq!(h_visibility(h.kind()), v_visibility(v.kind()));
    }
    for (h, v) in some(f.nodes::<ast::VisibilityInner, view::VisibilityInner<'_>>()) {
        assert_eq!(h_visibility(h.kind()), v_visibility(v.kind()));
    }
}

fn h_visibility(it: ast::VisibilityKind) -> Shape {
    match it {
        ast::VisibilityKind::In(path) => (0, vec![Some(hn(&path))]),
        ast::VisibilityKind::PubCrate => (1, vec![]),
        ast::VisibilityKind::PubSuper => (2, vec![]),
        ast::VisibilityKind::PubSelf => (3, vec![]),
        ast::VisibilityKind::Pub => (4, vec![]),
    }
}

fn v_visibility(it: view::VisibilityKind<'_>) -> Shape {
    match it {
        view::VisibilityKind::In(path) => (0, vec![Some(vn(&path))]),
        view::VisibilityKind::PubCrate => (1, vec![]),
        view::VisibilityKind::PubSuper => (2, vec![]),
        view::VisibilityKind::PubSelf => (3, vec![]),
        view::VisibilityKind::Pub => (4, vec![]),
    }
}

#[test]
fn structs_and_fields() {
    let f = Fixture::source();
    fn h_kind(it: ast::StructKind) -> Shape {
        match it {
            ast::StructKind::Record(it) => (0, vec![Some(hn(&it))]),
            ast::StructKind::Tuple(it) => (1, vec![Some(hn(&it))]),
            ast::StructKind::Unit => (2, vec![]),
        }
    }
    fn v_kind(it: view::StructKind<'_>) -> Shape {
        match it {
            view::StructKind::Record(it) => (0, vec![Some(vn(&it))]),
            view::StructKind::Tuple(it) => (1, vec![Some(vn(&it))]),
            view::StructKind::Unit => (2, vec![]),
        }
    }
    for (h, v) in some(f.nodes::<ast::Struct, view::Struct<'_>>()) {
        assert_eq!(h_kind(h.kind()), v_kind(v.kind()));
    }
    for (h, v) in some(f.nodes::<ast::Union, view::Union<'_>>()) {
        assert_eq!(h_kind(h.kind()), v_kind(v.kind()));
    }
    for (h, v) in some(f.nodes::<ast::Variant, view::Variant<'_>>()) {
        assert_eq!(h_kind(h.kind()), v_kind(v.kind()));
        assert_eq!(hn(&h.parent_enum()), vn(&v.parent_enum()));
    }
    for (h, v) in some(f.nodes::<ast::RecordExprField, view::RecordExprField<'_>>()) {
        assert_eq!(ho(h.field_name()), vo(v.field_name()));
        assert_eq!(hn(&h.parent_record_lit()), vn(&v.parent_record_lit()));
    }
    for (h, v) in some(f.nodes::<ast::RecordPatField, view::RecordPatField<'_>>()) {
        assert_eq!(ho(h.field_name()), vo(v.field_name()));
        assert_eq!(hn(&h.parent_record_pat()), vn(&v.parent_record_pat()));
    }
    for (h, v) in some(f.nodes::<ast::FieldExpr, view::FieldExpr<'_>>()) {
        assert_eq!(hto(h.index_token()), vto(v.index_token()));
        let hk = h.field_access().map(|it| match it {
            ast::FieldKind::Name(it) => (0, hn(&it)),
            ast::FieldKind::Index(it) => (1, ht(&it)),
        });
        let vk = v.field_access().map(|it| match it {
            view::FieldKind::Name(it) => (0, vn(&it)),
            view::FieldKind::Index(it) => (1, vt(&it)),
        });
        assert_eq!(hk, vk);
    }
}

#[test]
fn types_and_generics() {
    let f = Fixture::source();
    for (h, v) in some(f.nodes::<ast::Type, view::Type<'_>>()) {
        assert_eq!(ho(h.generic_arg_list()), vo(v.generic_arg_list()));
        assert_eq!(h.needs_angles_in_path(), v.needs_angles_in_path());
        if let (Some(hp), Some(vp)) = (h.syntax().parent(), v.syntax().parent()) {
            assert_eq!(h.needs_parens_in(&hp), v.needs_parens_in(vp));
        }
    }
    for (h, v) in some(f.nodes::<ast::TypeBound, view::TypeBound<'_>>()) {
        let hk = h.kind().map(|it| match it {
            ast::TypeBoundKind::PathType(binder, ty) => (0, vec![ho(binder), Some(hn(&ty))]),
            ast::TypeBoundKind::Use(args) => (1, vec![Some(hn(&args))]),
            ast::TypeBoundKind::Lifetime(it) => (2, vec![Some(hn(&it))]),
        });
        let vk = v.kind().map(|it| match it {
            view::TypeBoundKind::PathType(binder, ty) => (0, vec![vo(binder), Some(vn(&ty))]),
            view::TypeBoundKind::Use(args) => (1, vec![Some(vn(&args))]),
            view::TypeBoundKind::Lifetime(it) => (2, vec![Some(vn(&it))]),
        });
        assert_eq!(hk, vk);
    }
    for (h, v) in some(f.nodes::<ast::TypeOrConstParam, view::TypeOrConstParam<'_>>()) {
        assert_eq!(ho(h.name()), vo(v.name()));
        assert_eq!(hs(h.attrs()), vs(v.attrs()));
        assert_eq!(hn(&ast::GenericParam::from(h)), vn(&view::GenericParam::from(v)));
    }
    for (h, v) in some(f.nodes::<ast::LifetimeParam, view::LifetimeParam<'_>>()) {
        assert_eq!(hts(h.lifetime_bounds()), vts(v.lifetime_bounds()));
    }
    for (h, v) in some(f.nodes::<ast::GenericArgList, view::GenericArgList<'_>>()) {
        assert_eq!(hs(h.lifetime_args()), vs(v.lifetime_args()));
    }
    for (h, v) in some(f.nodes::<ast::GenericParamList, view::GenericParamList<'_>>()) {
        assert_eq!(hs(h.lifetime_params()), vs(v.lifetime_params()));
        assert_eq!(hs(h.type_or_const_params()), vs(v.type_or_const_params()));
    }
}

#[test]
fn patterns() {
    let f = Fixture::source();
    for (h, v) in some(f.nodes::<ast::SlicePat, view::SlicePat<'_>>()) {
        let hc = h.components();
        let vc = v.components();
        assert_eq!(hs(hc.prefix), vs(vc.prefix));
        assert_eq!(ho(hc.slice), vo(vc.slice));
        assert_eq!(hs(hc.suffix), vs(vc.suffix));
    }
    for (h, v) in some(f.nodes::<ast::IdentPat, view::IdentPat<'_>>()) {
        assert_eq!(h.is_simple_ident(), v.is_simple_ident());
    }
    for (h, v) in some(f.nodes::<ast::SelfParam, view::SelfParam<'_>>()) {
        assert_eq!(h.kind(), v.kind());
    }
    for (h, v) in some(f.nodes::<ast::RangePat, view::RangePat<'_>>()) {
        assert_eq!(ho(h.start()), vo(v.start()));
        assert_eq!(ho(h.end()), vo(v.end()));
        assert_eq!(hto(h.op_token()), vto(v.op_token()));
        assert_eq!(h.op_kind(), v.op_kind());
    }
    for (h, v) in some(f.nodes::<ast::OrPat, view::OrPat<'_>>()) {
        assert_eq!(hto(h.leading_pipe()), vto(v.leading_pipe()));
    }
    for (h, v) in some(f.nodes::<ast::MatchGuard, view::MatchGuard<'_>>()) {
        assert_eq!(ho(h.condition()), vo(v.condition()));
    }
    for (h, v) in some(f.nodes::<ast::MatchArm, view::MatchArm<'_>>()) {
        assert_eq!(hn(&h.parent_match()), vn(&v.parent_match()));
    }
}

#[test]
fn token_trees() {
    let f = Fixture::source();
    let h_element = |it: crate::NodeOrToken<ast::TokenTree, SyntaxToken>| match it {
        crate::NodeOrToken::Node(it) => hn(&it),
        crate::NodeOrToken::Token(it) => ht(&it),
    };
    let v_element = |it: crate::NodeOrToken<view::TokenTree<'_>, view::Token<'_>>| match it {
        crate::NodeOrToken::Node(it) => vn(&it),
        crate::NodeOrToken::Token(it) => vt(&it),
    };
    for (h, v) in some(f.nodes::<ast::TokenTree, view::TokenTree<'_>>()) {
        assert_eq!(
            h.token_trees_and_tokens().map(h_element).collect::<Vec<_>>(),
            v.token_trees_and_tokens().map(v_element).collect::<Vec<_>>()
        );
        assert_eq!(hto(h.left_delimiter_token()), vto(v.left_delimiter_token()));
        assert_eq!(hto(h.right_delimiter_token()), vto(v.right_delimiter_token()));
        assert_eq!(ho(h.parent_meta()), vo(v.parent_meta()));
        assert_eq!(
            ast::TokenTreeChildren::new(&h).map(h_element).collect::<Vec<_>>(),
            view::TokenTreeChildren::new(v).map(v_element).collect::<Vec<_>>()
        );
    }
}

#[test]
fn loops_and_args() {
    let f = Fixture::source();
    for (h, v) in some(f.nodes::<ast::ArgList, view::ArgList<'_>>()) {
        assert_eq!(
            h.args_maybe_empty().map(ho).collect::<Vec<_>>(),
            v.args_maybe_empty().map(vo).collect::<Vec<_>>()
        );
        assert_eq!(hs(h.args()), vs(v.args()));
    }
    for (h, v) in some(f.nodes::<ast::ForExpr, view::ForExpr<'_>>()) {
        assert_eq!(ho(h.iterable()), vo(v.iterable()));
        assert_eq!(ho(h.loop_body()), vo(v.loop_body()));
        assert_eq!(ho(h.label()), vo(v.label()));
    }
    for (h, v) in some(f.nodes::<ast::WhileExpr, view::WhileExpr<'_>>()) {
        assert_eq!(ho(h.condition()), vo(v.condition()));
        assert_eq!(ho(h.loop_body()), vo(v.loop_body()));
    }
    for (h, v) in some(f.nodes::<ast::FormatArgsExpr, view::FormatArgsExpr<'_>>()) {
        assert_eq!(hs(h.args()), vs(v.args()));
    }
}

// endregion

// region: expr_ext

#[test]
fn expressions() {
    let f = Fixture::source();
    for (h, v) in some(f.nodes::<ast::Expr, view::Expr<'_>>()) {
        assert_eq!(h.is_block_like(), v.is_block_like());
        assert_eq!(hs(h.attrs()), vs(v.attrs()));
    }
    for (h, v) in some(f.nodes::<ast::IfExpr, view::IfExpr<'_>>()) {
        assert_eq!(ho(h.condition()), vo(v.condition()));
        assert_eq!(ho(h.then_branch()), vo(v.then_branch()));
        assert_eq!(ho(h.else_branch()), vo(v.else_branch()));
    }
    for (h, v) in some(f.nodes::<ast::ElseBranch, view::ElseBranch<'_>>()) {
        assert_eq!(
            matches!(h, ast::ElseBranch::Block(_)),
            matches!(v, view::ElseBranch::Block(_))
        );
    }
    for (h, v) in some(f.nodes::<ast::PrefixExpr, view::PrefixExpr<'_>>()) {
        assert_eq!(h.op_kind(), v.op_kind());
        assert_eq!(hto(h.op_token()), vto(v.op_token()));
    }
    for (h, v) in some(f.nodes::<ast::BinExpr, view::BinExpr<'_>>()) {
        assert_eq!(
            h.op_details().map(|(t, op)| (ht(&t), op)),
            v.op_details().map(|(t, op)| (vt(&t), op))
        );
        assert_eq!(h.op_kind(), v.op_kind());
        assert_eq!(hto(h.op_token()), vto(v.op_token()));
        assert_eq!(ho(h.lhs()), vo(v.lhs()));
        assert_eq!(ho(h.rhs()), vo(v.rhs()));
        let (hl, hr) = h.sub_exprs();
        let (vl, vr) = v.sub_exprs();
        assert_eq!((ho(hl), ho(hr)), (vo(vl), vo(vr)));
    }
    for (h, v) in some(f.nodes::<ast::RangeExpr, view::RangeExpr<'_>>()) {
        assert_eq!(h.is_range_full(), v.is_range_full());
        assert_eq!(ho(h.start()), vo(v.start()));
        assert_eq!(ho(h.end()), vo(v.end()));
        assert_eq!(hto(h.op_token()), vto(v.op_token()));
        assert_eq!(h.op_kind(), v.op_kind());
    }
    for (h, v) in some(f.nodes::<ast::IndexExpr, view::IndexExpr<'_>>()) {
        assert_eq!(ho(h.base()), vo(v.base()));
        assert_eq!(ho(h.index()), vo(v.index()));
    }
    for (h, v) in some(f.nodes::<ast::ArrayExpr, view::ArrayExpr<'_>>()) {
        let hk = match h.kind() {
            ast::ArrayExprKind::Repeat { initializer, repeat } => {
                (0, vec![ho(initializer), ho(repeat)])
            }
            ast::ArrayExprKind::ElementList(it) => (1, it.map(|it| Some(hn(&it))).collect()),
        };
        let vk = match v.kind() {
            view::ArrayExprKind::Repeat { initializer, repeat } => {
                (0, vec![vo(initializer), vo(repeat)])
            }
            view::ArrayExprKind::ElementList(it) => (1, it.map(|it| Some(vn(&it))).collect()),
        };
        assert_eq!(hk, vk);
    }
    for (h, v) in some(f.nodes::<ast::CallableExpr, view::CallableExpr<'_>>()) {
        assert_eq!(ho(h.arg_list()), vo(v.arg_list()));
        assert_eq!(
            matches!(h, ast::CallableExpr::Call(_)),
            matches!(v, view::CallableExpr::Call(_))
        );
    }
    for (h, v) in some(f.nodes::<ast::MacroDef, view::MacroDef<'_>>()) {
        assert_eq!(ho(h.args()), vo(v.args()));
        assert_eq!(ho(h.body()), vo(v.body()));
    }
}

#[test]
fn literals() {
    let f = Fixture::source();
    let mut kinds = [false; 9];
    for (h, v) in some(f.nodes::<ast::Literal, view::Literal<'_>>()) {
        assert_eq!(ht(&h.token()), vt(&v.token()));
        let hk = match h.kind() {
            ast::LiteralKind::String(t) => (0, Some(htk(&t))),
            ast::LiteralKind::ByteString(t) => (1, Some(htk(&t))),
            ast::LiteralKind::CString(t) => (2, Some(htk(&t))),
            ast::LiteralKind::IntNumber(t) => (3, Some(htk(&t))),
            ast::LiteralKind::FloatNumber(t) => (4, Some(htk(&t))),
            ast::LiteralKind::Char(t) => (5, Some(htk(&t))),
            ast::LiteralKind::Byte(t) => (6, Some(htk(&t))),
            ast::LiteralKind::Bool(b) => (7 + b as usize, None),
        };
        let vk = match v.kind() {
            view::LiteralKind::String(t) => (0, Some(vtk(&t))),
            view::LiteralKind::ByteString(t) => (1, Some(vtk(&t))),
            view::LiteralKind::CString(t) => (2, Some(vtk(&t))),
            view::LiteralKind::IntNumber(t) => (3, Some(vtk(&t))),
            view::LiteralKind::FloatNumber(t) => (4, Some(vtk(&t))),
            view::LiteralKind::Char(t) => (5, Some(vtk(&t))),
            view::LiteralKind::Byte(t) => (6, Some(vtk(&t))),
            view::LiteralKind::Bool(b) => (7 + b as usize, None),
        };
        assert_eq!(hk, vk);
        kinds[vk.0] = true;
    }
    assert!(kinds.iter().all(|&it| it), "every literal kind: {kinds:?}");
}

// endregion

// region: token_ext

#[test]
fn comments_and_whitespace() {
    let f = Fixture::source();
    for (h, v) in some(f.tokens::<ast::Comment, view::Comment<'_>>()) {
        assert_eq!(h.shape(), v.shape());
        assert_eq!(h.text_without_markers(), v.text_without_markers());
    }
    for (h, v) in some(f.tokens::<ast::AnyComment, view::AnyComment<'_>>()) {
        assert_eq!(h.shape(), v.shape());
        assert_eq!(h.doc_kind(), v.doc_kind());
        assert_eq!(h.kind(), v.kind());
        assert_eq!(h.prefix(), v.prefix());
        assert_eq!(h.is_inner(), v.is_inner());
        assert_eq!(h.is_outer(), v.is_outer());
        assert_eq!(h.text_with_markers(), v.text_with_markers());
        assert_eq!(h.text(), v.text());
    }
    for (h, v) in some(f.tokens::<ast::Whitespace, view::Whitespace<'_>>()) {
        assert_eq!(h.spans_multiple_lines(), v.spans_multiple_lines());
    }
}

/// What `IsString` answers for a literal: raw, quotes, contents, text, escapes, mapped ranges.
type StringFacts = (
    bool,
    Option<((TextRange, TextRange), TextRange)>,
    Option<TextRange>,
    std::string::String,
    Option<TextRange>,
    Option<TextRange>,
    Vec<(TextRange, Result<char, EscapeError>)>,
    Option<TextRange>,
    Option<TextSize>,
);

fn h_strings<S: ast::IsString>(h: &S) -> StringFacts {
    let mut escapes = Vec::new();
    h.escaped_char_ranges(&mut |range, it| escapes.push((range, it)));
    let contents = h.text_range_between_quotes();
    (
        h.is_raw(),
        h.quote_offsets().map(|it| (it.quotes, it.contents)),
        contents,
        h.text_without_quotes().to_owned(),
        h.open_quote_text_range(),
        h.close_quote_text_range(),
        escapes,
        contents.and_then(|it| h.map_range_up(TextRange::up_to(it.len()))),
        h.map_offset_down(h.syntax().text_range().end()),
    )
}

fn v_strings<'t, S: view::IsString<'t>>(v: &S) -> StringFacts {
    let mut escapes = Vec::new();
    v.escaped_char_ranges(|range, it| escapes.push((range, it)));
    let contents = v.text_range_between_quotes();
    (
        v.is_raw(),
        v.quote_offsets().map(|it| (it.quotes, it.contents)),
        contents,
        v.text_without_quotes().to_owned(),
        v.open_quote_text_range(),
        v.close_quote_text_range(),
        escapes,
        contents.and_then(|it| v.map_range_up(TextRange::up_to(it.len()))),
        v.map_offset_down(v.syntax().text_range().end()),
    )
}

#[test]
fn strings() {
    let crlf = Fixture::new("fn f() { \"a\r\nb\\n\"; b\"c\r\nd\"; c\"e\r\nf\"; r\"g\r\nh\"; }");
    for f in [Fixture::source(), crlf] {
        for (h, v) in some(f.tokens::<ast::String, view::String<'_>>()) {
            assert_eq!(h_strings(&h), v_strings(&v));
            assert_eq!(h.value().as_deref(), v.value().as_deref());
        }
        for (h, v) in some(f.tokens::<ast::ByteString, view::ByteString<'_>>()) {
            assert_eq!(h_strings(&h), v_strings(&v));
            assert_eq!(h.value().as_deref(), v.value().as_deref());
        }
        for (h, v) in some(f.tokens::<ast::CString, view::CString<'_>>()) {
            assert_eq!(h_strings(&h), v_strings(&v));
            assert_eq!(h.value().as_deref(), v.value().as_deref());
        }
        for (h, v) in some(f.tokens::<ast::AnyString, view::AnyString<'_>>()) {
            assert_eq!(h_strings(&h), v_strings(&v));
            assert_eq!(h.value().as_deref(), v.value().as_deref());
            assert_eq!(h.raw_prefix(), v.raw_prefix());
        }
    }
}

#[test]
fn numbers() {
    let f = Fixture::source();
    let mut errors = 0;
    for (h, v) in some(f.tokens::<ast::IntNumber, view::IntNumber<'_>>()) {
        assert_eq!(h.radix(), v.radix());
        assert_eq!(h.split_into_parts(), v.split_into_parts());
        assert_eq!(h.value(), v.value(), "{}", v.text());
        assert_eq!(h.suffix(), v.suffix());
        assert_eq!(h.value_string(), v.value_string());
        errors += v.value().is_err() as usize;
    }
    // `340282366920938463463374607431768211456`, `0x`, `0b`, `0b102`, `0o9`.
    assert!(errors >= 4, "{errors}");
    for (h, v) in some(f.tokens::<ast::FloatNumber, view::FloatNumber<'_>>()) {
        assert_eq!(h.split_into_parts(), v.split_into_parts());
        assert_eq!(h.suffix(), v.suffix());
        assert_eq!(h.value_string(), v.value_string());
    }
    for (h, v) in some(f.tokens::<ast::Char, view::Char<'_>>()) {
        assert_eq!(h.value(), v.value());
    }
    for (h, v) in some(f.tokens::<ast::Byte, view::Byte<'_>>()) {
        assert_eq!(h.value(), v.value());
    }
}

/// `digits_value` is `u128::from_str_radix` of the text without `_`, error kind included, on
/// every string of up to four of these bytes and on the long edges.
#[test]
fn digits_value_is_from_str_radix() {
    let alphabet = ["", "0", "1", "7", "9", "a", "f", "z", "_", "+", "-", "é"];
    let check = |text: &str| {
        for radix in [2, 8, 10, 16] {
            let expected = u128::from_str_radix(&text.replace('_', ""), radix);
            let got = super::token_ext::digits_value(text, radix);
            assert_eq!(got, expected, "{text:?} radix {radix}");
            if let (Err(got), Err(expected)) = (&got, &expected) {
                assert_eq!(got.kind(), expected.kind(), "{text:?} radix {radix}");
            }
        }
    };
    for a in alphabet {
        for b in alphabet {
            for c in alphabet {
                for d in alphabet {
                    check(&[a, b, c, d].concat());
                }
            }
        }
    }
    for edge in [
        "340282366920938463463374607431768211455",
        "340282366920938463463374607431768211456",
        "3402823669209384634633746074317682114550",
        "ffffffffffffffffffffffffffffffff",
        "1_0000_0000_0000_0000_0000_0000_0000_0000",
        "+_1",
        "_+1",
    ] {
        check(edge);
    }
}

// endregion

// region: prec

#[test]
fn precedence_and_parens() {
    let f = Fixture::source();
    let exprs = some(f.nodes::<ast::Expr, view::Expr<'_>>());
    for (h, v) in &exprs {
        assert_eq!(ast::prec::precedence(h), view::prec::precedence(*v));
        assert_eq!(h.precedence(), v.precedence());
        assert_eq!(h.is_ret_like_with_no_value(), v.is_ret_like_with_no_value());
        if let (Some(hp), Some(vp)) = (h.syntax().parent(), v.syntax().parent()) {
            assert_eq!(h.needs_parens_in(&hp), v.needs_parens_in(vp), "{}", v.syntax());
        }
    }
    // Every expression in the place of each of the first ones with a parent.
    let places: Vec<_> = exprs
        .iter()
        .filter_map(|(h, v)| Some((h.syntax().parent()?, h, v.syntax().parent()?, v)))
        .take(64)
        .collect();
    let mut parens = 0;
    for (h, v) in &exprs {
        for (hp, hplace, vp, vplace) in &places {
            let answer = v.needs_parens_in_place_of(*vp, vplace.syntax());
            assert_eq!(h.needs_parens_in_place_of(hp, hplace.syntax()), answer);
            parens += answer as usize;
        }
    }
    assert!(parens > 0);
}

// endregion

/// The fixtures exercise offsets past their ends only through `TextSize`; this keeps the import
/// honest for `map_offset_down`.
#[test]
fn offsets_down_of_a_string() {
    let f = Fixture::new(r#"fn f() { "abc"; }"#);
    for (h, v) in some(f.tokens::<ast::String, view::String<'_>>()) {
        let start = v.syntax().text_range().start();
        for at in 0..8u32 {
            let offset = start + TextSize::from(at);
            assert_eq!(h.map_offset_down(offset), v.map_offset_down(offset));
        }
    }
}
