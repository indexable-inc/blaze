//! Const macros (docs/future/const-macros.md): a derive whose body is a function of a macro
//! crate, run by the const evaluator, that is given the item as a flat syntax tree and writes
//! its output as one with `const_ast::Builder`; the host turns the tree into tokens with the
//! call's spans, refuses a tree that is not their parse, and the using crate evaluates the impl.

use std::{fmt::Write as _, sync::Arc};

use base_db::all_crates;
use hir_expand::{
    EditionedFileId, fixture,
    proc_macro::{ProcMacroCall, ProcMacroExpander as _, ProcMacroKind},
    tt,
};
use intern::Symbol;
use span::{Span, SpanAnchor, SyntaxContext};
use syntax::{
    AstNode, Edition, SyntaxKind, TextRange, TextSize,
    ast::{self, HasModuleItem, HasName},
};

use super::{eval_goal, pretty_print_err};
use crate::{const_macro::ConstMacroExpander, test_db::TestDB};

/// The macro crate's derives, every one written with the raw builder: `Named`, `NamedAtCall`
/// and `Fields` build impls; the rest are the planted bugs the host's checks name.
const MACROS: &str = r#"
use const_ast::{Builder, Entry, Item, Kind, Name, Open, Output, Span};

/// The nodes a name is wrapped in to stand where a type goes.
const AS_TYPE: [Kind; 4] = [Kind::PATH_TYPE, Kind::PATH, Kind::PATH_SEGMENT, Kind::NAME_REF];

/// The type named by one written name: `usize`, `Named`.
const fn path_type(b: &mut Builder<'static>, name: &'static str) {
    let ty = b.open(Kind::PATH_TYPE);
    let path = b.open(Kind::PATH);
    let segment = b.open(Kind::PATH_SEGMENT);
    let name_ref = b.open(Kind::NAME_REF);
    b.token(name);
    b.close(name_ref);
    b.close(segment);
    b.close(path);
    b.close(ty);
}

/// `const NAME: TY = VALUE;`, an associated constant.
const fn constant(
    b: &mut Builder<'static>,
    name: &'static str,
    ty: &'static str,
    value: &'static str,
) {
    let item = b.open(Kind::CONST);
    b.token("const");
    let n = b.open(Kind::NAME);
    b.token(name);
    b.close(n);
    b.token(":");
    path_type(b, ty);
    b.token("=");
    let literal = b.open(Kind::LITERAL);
    b.token(value);
    b.close(literal);
    b.token(";");
    b.close(item);
}

/// The name of the struct `item` is.
const fn struct_name<'a>(item: &Item<'a>) -> Name<'a> {
    match item.as_struct() {
        Some(adt) => match adt.name() {
            Some(name) => name,
            None => panic!("the struct has no name"),
        },
        None => panic!("the derive is for a struct"),
    }
}

/// `impl TRAIT for NAME {`, the name spliced from the input; what `close_impl` closes.
const fn open_impl(b: &mut Builder<'static>, trait_name: &'static str, name: Name<'_>) -> [Open; 2] {
    let imp = b.open(Kind::IMPL);
    b.token("impl");
    path_type(b, trait_name);
    b.token("for");
    name.splice(b, &AS_TYPE);
    let list = b.open(Kind::ASSOC_ITEM_LIST);
    b.token("{");
    [imp, list]
}

const fn close_impl(b: &mut Builder<'static>, opens: [Open; 2]) {
    b.token("}");
    b.close(opens[1]);
    b.close(opens[0]);
}

/// `impl Named for NAME { const LEN: usize = N; }`, N the name's length, every token but the
/// name's at `site`.
const fn named_at(item: &Item<'_>, site: Span) -> Output {
    let mut b = Builder::new(site);
    let name = struct_name(item);
    let len = b.decimal(name.text().as_bytes().len() as u128);
    let opens = open_impl(&mut b, "Named", name);
    constant(&mut b, "LEN", "usize", len);
    close_impl(&mut b, opens);
    b.finish(Entry::Items)
}

#[proc_macro_derive(Named)]
pub const fn named(item: &Item) -> Output {
    named_at(item, Span::mixed_site())
}

#[proc_macro_derive(NamedAtCall)]
pub const fn named_at_call(item: &Item) -> Output {
    named_at(item, Span::call_site())
}

/// `impl Fields for NAME { const COUNT: usize = N; const NAME_LEN: usize = L; const
/// INPUT_SPAN: bool = B; }`: how many fields the struct has, its name's length, and whether the
/// name's span is one of the input's (after the three sites in the call's table). The name is
/// spliced by its token, inside the nodes a type's path is.
#[proc_macro_derive(Fields)]
pub const fn fields(item: &Item) -> Output {
    let mut b = Builder::new(Span::mixed_site());
    let name = struct_name(item);
    let count = match item.as_struct() {
        Some(adt) => match adt.field_list() {
            Some(const_ast::FieldList::RecordFieldList(list)) => list.fields().count(),
            _ => 0,
        },
        None => 0,
    };
    let Some(token) = name.token() else { panic!("the name has no token") };
    let count = b.decimal(count as u128);
    let len = b.decimal(name.text().as_bytes().len() as u128);
    let input_span = if token.span().index() >= 3 { "true" } else { "false" };
    let imp = b.open(Kind::IMPL);
    b.token("impl");
    path_type(&mut b, "Fields");
    b.token("for");
    let ty = b.open(Kind::PATH_TYPE);
    let path = b.open(Kind::PATH);
    let segment = b.open(Kind::PATH_SEGMENT);
    let name_ref = b.open(Kind::NAME_REF);
    b.tok(token);
    b.close(name_ref);
    b.close(segment);
    b.close(path);
    b.close(ty);
    let list = b.open(Kind::ASSOC_ITEM_LIST);
    b.token("{");
    constant(&mut b, "COUNT", "usize", count);
    constant(&mut b, "NAME_LEN", "usize", len);
    constant(&mut b, "INPUT_SPAN", "bool", input_span);
    b.token("}");
    b.close(list);
    b.close(imp);
    b.finish(Entry::Items)
}

/// The planted bug of the parse check: `impl Named for NAME {}`'s tokens under a `STRUCT`.
#[proc_macro_derive(Mislabeled)]
pub const fn mislabeled(item: &Item) -> Output {
    let mut b = Builder::new(Span::mixed_site());
    let name = struct_name(item);
    let s = b.open(Kind::STRUCT);
    b.token("impl");
    path_type(&mut b, "Named");
    b.token("for");
    name.splice(&mut b, &AS_TYPE);
    let list = b.open(Kind::ASSOC_ITEM_LIST);
    b.token("{");
    b.token("}");
    b.close(list);
    b.close(s);
    b.finish(Entry::Items)
}

/// The planted bug of the span check: `struct Far;` whose `struct` has the span 9,999, made
/// from its number, which no call's table has.
#[proc_macro_derive(FarSpan)]
pub const fn far_span(_item: &Item) -> Output {
    let mut b = Builder::new(Span::mixed_site());
    let far = unsafe { core::mem::transmute::<u32, Span>(9999) };
    let s = b.open(Kind::STRUCT);
    b.token_at("struct", far);
    let n = b.open(Kind::NAME);
    b.token("Far");
    b.close(n);
    b.token(";");
    b.close(s);
    b.finish(Entry::Items)
}

/// A `const_ast::Node`'s layout: what `NoElement` forges one from.
#[repr(C)]
struct Forged<'a> {
    tree: &'a const_ast::Tree<'a>,
    at: u32,
}

/// The planted bug of the splice check: a node of the input's tree past its end.
#[proc_macro_derive(NoElement)]
pub const fn no_element(item: &Item) -> Output {
    let mut b = Builder::new(Span::mixed_site());
    let forged = Forged { tree: item.syntax().tree(), at: 99999 };
    let node = unsafe { core::mem::transmute::<Forged<'_>, const_ast::Node<'_>>(forged) };
    b.node(node);
    b.finish(Entry::Items)
}

/// The planted bug of the return type: a byte, not a tree.
#[proc_macro_derive(Byte)]
pub const fn byte(_item: &Item) -> u8 {
    0
}

/// The planted bug of the parameter: a byte, not a tree.
#[proc_macro_derive(TakesByte)]
pub const fn takes_byte(_item: &u8) -> Output {
    Builder::new(Span::mixed_site()).finish(Entry::Items)
}
"#;

/// A fixture whose macro crate depends on `const_ast` (blaze's files to the byte, each ending
/// in a newline, under `blaze_const_eval`, with the proc-macro crate `const_ast_quote` it
/// takes `quote!` from) and defines `macros`, and whose crate `main` is `main`.
pub(super) fn with_const_ast(macros: &str, main: &str) -> String {
    let [(_, lib), (_, kinds), (_, views), (_, builder)] = crate::const_macro::CONST_AST;
    format!(
        "//- minicore: sized, copy, clone, derive, option, result, slice, index, coerce_unsized, str, iterator, panic, transmute, size_of
//- /const_ast_quote/lib.rs crate:proc-macro:const_ast_quote
#[proc_macro]
pub fn quote(_input: ()) {{}}
//- /const_ast/lib.rs crate:const_ast deps:const_ast_quote cfg:blaze_const_eval
{lib}//- /const_ast/kinds.rs
{kinds}//- /const_ast/views.rs
{views}//- /const_ast/builder.rs
{builder}//- /macros.rs crate:proc-macro:macros deps:const_ast
{macros}
//- /main.rs crate:main deps:macros
{main}"
    )
}

#[test]
fn a_const_derive_generates_an_impl() {
    let fixture = with_const_ast(
        MACROS,
        "pub trait Named { const LEN: usize; }

#[derive(macros::Named)]
pub struct Point { x: i32, y: i32 }

const GOAL: usize = <Point as Named>::LEN;
",
    );
    assert_eq!(goal(&fixture, "Named")[..], 5usize.to_le_bytes()[..]);
}

#[test]
fn a_const_derive_reads_the_item_through_the_views() {
    let fixture = with_const_ast(
        MACROS,
        "pub trait Fields { const COUNT: usize; const NAME_LEN: usize; const INPUT_SPAN: bool; }

#[derive(macros::Fields)]
pub struct Point {
    x: i32,
    #[allow(unused)]
    pub y: i32,
    pub(crate) z: [u8; 4],
}

const GOAL: usize = <Point as Fields>::COUNT * 100
    + <Point as Fields>::NAME_LEN * 10
    + <Point as Fields>::INPUT_SPAN as usize;
",
    );
    assert_eq!(goal(&fixture, "Fields")[..], 351usize.to_le_bytes()[..]);
}

/// The refinement: for every struct of the real files the views' test walks, the tree the
/// built `Fields` derive gives hir-expand has the shape of the parse of the text the text-era
/// derive wrote for it, computed here with `syntax::ast`.
#[test]
fn a_built_derive_is_the_tree_its_text_parsed_to() {
    let files =
        [include_str!("../../../../cfg/src/cfg_expr.rs"), include_str!("../../../../span/src/lib.rs")];
    let mut main = String::from(
        "pub trait Fields { const COUNT: usize; const NAME_LEN: usize; const INPUT_SPAN: bool; }\n",
    );
    let mut expected = Vec::new();
    for text in files {
        let file = ast::SourceFile::parse(text, Edition::CURRENT).tree();
        for item in file.items() {
            let ast::Item::Struct(adt) = item else { continue };
            let Some(name) = adt.name() else { continue };
            let count = match adt.field_list() {
                Some(ast::FieldList::RecordFieldList(list)) => list.fields().count(),
                _ => 0,
            };
            // The struct without its attributes and doc comments: the derive is its only one.
            let start = adt
                .syntax()
                .children_with_tokens()
                .find(|it| {
                    !matches!(it.kind(), SyntaxKind::ATTR | SyntaxKind::COMMENT | SyntaxKind::WHITESPACE)
                })
                .expect("a struct has tokens")
                .text_range()
                .start();
            let source = &text[usize::from(start)..usize::from(adt.syntax().text_range().end())];
            _ = writeln!(main, "mod m{} {{\n#[derive(macros::Fields)]\n{source}\n}}", expected.len());
            let name = name.text().to_string();
            expected.push(shape_of_text(&format!(
                "impl Fields for {name} {{ const COUNT: usize = {count}; const NAME_LEN: usize = {}; \
                 const INPUT_SPAN: bool = true; }}",
                name.len()
            )));
        }
    }
    assert!(expected.len() >= 5, "the files have {} structs", expected.len());
    let fixture = with_const_ast(MACROS, &main);
    let (db, _, _) = load(&fixture, "Fields");
    let mut built = blaze_query::db::attach(&db, || {
        let main = *all_crates(&db).last().expect("the main crate");
        expansions(&db, main)
    });
    built.sort();
    expected.sort();
    assert_eq!(built, expected);
}

#[test]
fn a_spliced_name_keeps_its_span_and_a_built_token_takes_the_site() {
    let fixture = with_const_ast(MACROS, "pub trait Named { const LEN: usize; }\n");
    let (db, _, root) = load(&fixture, "Named");
    let anchor = SpanAnchor {
        file_id: span::EditionedFileId::new(span::FileId::from_raw(7), Edition::CURRENT),
        ast_id: span::ROOT_ERASED_FILE_AST_ID,
    };
    let ctx = SyntaxContext::root(Edition::CURRENT);
    let site = |at: u32| Span { range: TextRange::empty(TextSize::new(at)), anchor, ctx };
    let (def_site, call_site, mixed_site) = (site(100), site(200), site(300));
    let input = syntax_bridge::parse_to_token_tree(
        Edition::CURRENT,
        anchor,
        ctx,
        "pub struct Point { x: i32 }",
    )
    .expect("the input is tokens");
    let point = span_of(&input, "Point");
    let env = base_db::Env::default();
    blaze_query::db::attach(&db, || {
        let expand = |name: &str| {
            let call = ProcMacroCall {
                subtree: &input,
                attrs: None,
                env: &env,
                def_site,
                call_site,
                mixed_site,
                current_dir: "",
            };
            match expander_of(root, name).expand(&db, call) {
                Ok(it) => it.tree,
                Err(e) => panic!("expanding `{name}`: {e:?}"),
            }
        };
        let mixed = expand("Named");
        assert_eq!(span_of(&mixed, "Point"), point, "the spliced name keeps the input's span");
        assert_eq!(span_of(&mixed, "impl"), mixed_site, "a written token has the builder's site");
        let call = expand("NamedAtCall");
        assert_eq!(span_of(&call, "Point"), point);
        assert_eq!(span_of(&call, "impl"), call_site);
    });
}

/// The planted bugs, each refused by the check that names it: the derive `name` on a struct
/// gives a message with `refusal` in it.
fn refused(name: &str, refusal: &str) {
    let fixture = with_const_ast(
        MACROS,
        &format!("pub trait Named {{ const LEN: usize; }}\n\n#[derive(macros::{name})]\npub struct Point;\n"),
    );
    let (db, _, _) = load(&fixture, name);
    blaze_query::db::attach(&db, || {
        let main = *all_crates(&db).last().expect("the main crate");
        let derives = derives(&db, main);
        assert!(derives.contains(refusal), "{derives}");
    });
}

#[test]
fn a_tree_that_is_not_its_tokens_parse_is_refused_naming_the_element() {
    refused(
        "Mislabeled",
        "the const macro `Mislabeled` returned a tree that is refused: built `STRUCT` (depth 1) \
         at output element 0 where its tokens parse to `IMPL` (depth 1)",
    );
}

#[test]
fn a_span_outside_the_table_is_refused() {
    refused(
        "FarSpan",
        "the const macro `FarSpan` returned a tree that is refused: token 1 `struct` has span \
         9999; the call's table has",
    );
}

#[test]
fn a_splice_of_no_element_is_refused() {
    refused(
        "NoElement",
        "the const macro `NoElement` returned a tree that is refused: element 0 splices element \
         99999 of input 0, which has",
    );
}

#[test]
fn a_const_macro_returns_output() {
    refused("Byte", "the const macro `Byte` returns `u8`; it returns `const_ast::Output`");
}

#[test]
fn a_const_macro_takes_item_or_tokens() {
    refused(
        "TakesByte",
        "a parameter of the const macro `TakesByte` is `&u8`; it takes `&const_ast::Item` or \
         `&const_ast::TokenTree`",
    );
}

/// The invariant: the builder's code is in the expansion's key. Expanding again with nothing
/// changed runs no expansion (the control); an edit of `Builder::close`, which every derive
/// reaches, runs it again, and it gives the same tree.
#[test]
fn editing_a_builder_function_reruns_the_expansions_that_used_it() {
    let fixture = with_const_ast(
        MACROS,
        "pub trait Named { const LEN: usize; }\n\n#[derive(macros::Named)]\npub struct Point { x: i32 }\n",
    );
    let (mut db, files, _) = load_files(&fixture, "Named");
    let builder = files
        .iter()
        .map(|it| it.file_id(&db))
        .find(|&file| db.file_text(file).text(&db).contains("pub struct Builder<'a>"))
        .expect("the fixture has builder.rs");
    let main = *all_crates(&db).last().expect("the main crate");
    let expand = |db: &TestDB| {
        blaze_query::db::attach(db, || {
            let mut tree = String::new();
            let executed = db.log_executed(|| tree = expansions(db, main).concat());
            (tree, executed)
        })
    };
    let ran = |executed: &[String]| executed.iter().any(|it| it.contains("proc_macro_expansion"));
    let (first, _) = expand(&db);
    assert!(first.contains("IMPL"), "{first}");
    let (again, executed) = expand(&db);
    assert!(!ran(&executed), "nothing changed, and the expansion ran: {executed:?}");
    assert_eq!(again, first);
    let text = db.file_text(builder).text(&db).to_string();
    let close = "    pub const fn close(&mut self, open: Open) {\n";
    assert!(text.contains(close), "builder.rs has `Builder::close`");
    db.set_file_text(builder, &text.replace(close, &format!("{close}        let _edited = 0u8;\n")));
    let (edited, executed) = expand(&db);
    assert!(ran(&executed), "`Builder::close` changed, and the expansion did not run: {executed:?}");
    assert_eq!(edited, first);
}

/// A database of `fixture`, whose macro crate defines the derive `name` and is given every
/// macro its root declares (and `const_ast_quote` the `quote!` expander), the file of its last
/// crate, and the macro crate's root.
pub(super) fn load(fixture: &str, name: &str) -> (TestDB, EditionedFileId, span::FileId) {
    let (db, files, root) = load_files(fixture, name);
    (db, *files.last().expect("the last crate's file"), root)
}

/// [`load`], with every file of the fixture.
fn load_files(fixture: &str, name: &str) -> (TestDB, Vec<EditionedFileId>, span::FileId) {
    let mut fx = fixture::db(fixture);
    let (macros, quote, given) = {
        let db = fx.db();
        let (mut macros, mut quote) = (None, None);
        for &krate in all_crates(db).iter() {
            if !krate.data(db).target_kind.is_proc_macro() {
                continue;
            }
            match krate.rust_name(db) {
                Ok(it) if it.as_str() == "const_ast_quote" => quote = Some(krate),
                _ => macros = Some(krate),
            }
        }
        let macros = macros.expect("the macro crate");
        let data = macros.data(db);
        let given =
            crate::const_macro::load(data.root_file_id, db.file_text(data.root_file_id).text(db), data.edition);
        (macros, quote, given)
    };
    assert!(
        given.iter().any(|it| it.name.as_str() == name && it.kind == ProcMacroKind::CustomDerive),
        "the macro crate defines the derive `{name}`"
    );
    let root = macros.data(fx.db()).root_file_id;
    if let Some(quote) = quote {
        fx.set_proc_macros(quote, vec![crate::const_macro::quote::proc_macro()]);
    }
    fx.set_proc_macros(macros, given);
    let files = fx.files().to_vec();
    (fx.into_db(), files, root)
}

/// `GOAL` of the last file of `fixture`, whose macro crate has the derive `name`.
fn goal(fixture: &str, name: &str) -> Vec<u8> {
    let (db, file_id, _) = load(fixture, name);
    blaze_query::db::attach(&db, || match eval_goal(&db, file_id) {
        Ok(goal) => goal.memory.to_vec(),
        Err(e) => {
            let main = *base_db::all_crates(&db).last().expect("the main crate");
            panic!("evaluating GOAL: {}\n{}", pretty_print_err(e, &db), derives(&db, main));
        }
    })
}

fn expander_of(root: span::FileId, name: &str) -> ConstMacroExpander {
    ConstMacroExpander { root, name: Symbol::intern(name), kind: ProcMacroKind::CustomDerive }
}

/// The span of the identifier `name` in `tree`.
fn span_of(tree: &tt::TopSubtree, name: &str) -> Span {
    tree.view()
        .as_token_trees()
        .iter_flat_tokens()
        .find_map(|it| match it {
            tt::TokenTree::Leaf(tt::Leaf::Ident(ident)) if ident.sym.as_str() == name => {
                Some(ident.span)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no `{name}` in {tree:?}"))
}

/// The shape of a tree: each node's kind and each token's text, indented by its depth, one a
/// line, with no whitespace. Two trees of one shape are the same tree to the parser.
pub(super) fn shape_of(node: syntax::view::Node<'_>) -> String {
    let (tree, root) = node.flat();
    let mut out = String::new();
    let mut open: Vec<u32> = Vec::new();
    for at in root..tree.end(root) {
        while open.last() == Some(&at) {
            open.pop();
        }
        let kind = SyntaxKind::from(tree.kind(at).0);
        let indent = "  ".repeat(open.len());
        if tree.is_token(at) {
            if !kind.is_trivia() {
                _ = writeln!(out, "{indent}{:?}", tree.token_text(at));
            }
        } else {
            _ = writeln!(out, "{indent}{kind:?}");
            open.push(tree.end(at));
        }
    }
    out
}

/// The shape of `text` parsed as a macro's items, as hir-expand parses an expansion.
fn shape_of_text(text: &str) -> String {
    let tokens = syntax_bridge::parse_to_token_tree_static_span(
        Edition::CURRENT,
        syntax_bridge::dummy_test_span_utils::DUMMY,
        text,
    )
    .expect("the text is tokens");
    let (parse, _) = syntax_bridge::token_tree_to_syntax_node(
        tokens.view(),
        parser::TopEntryPoint::MacroItems,
        &mut |_| Edition::CURRENT,
    );
    assert!(parse.errors().is_empty(), "{text}: {:?}", parse.errors());
    shape_of(parse.root())
}

/// The shape of what each derive of `krate` expanded to, in the order the def map has them.
///
/// # Panics
///
/// When one failed, with why.
fn expansions(db: &TestDB, krate: base_db::Crate) -> Vec<String> {
    let mut out = Vec::new();
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    for (_, module) in def_map.modules(db) {
        for (_, invocations) in module.scope(db).derive_macro_invocs() {
            for (_, _, calls) in invocations {
                for call in calls.iter().flatten() {
                    let either::Either::Left(call) = call else { continue };
                    let expansion = call.parse_macro_expansion(db);
                    if let Some(err) = &expansion.err {
                        panic!("a derive failed: {}", err.render_to_string(db).message);
                    }
                    out.push(shape_of(expansion.value.0.root()));
                }
            }
        }
    }
    out
}

/// What the derives of `krate` expanded to, and why they did not, for a failure to say.
pub(super) fn derives(db: &TestDB, krate: base_db::Crate) -> String {
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    let mut out = format!("def map diagnostics: {:?}\n", def_map.diagnostics(db));
    for (_, module) in def_map.modules(db) {
        for (_, invocations) in module.scope(db).derive_macro_invocs() {
            for (_, _, calls) in invocations {
                for call in calls.iter().flatten() {
                    let either::Either::Left(call) = call else { continue };
                    let expansion = call.parse_macro_expansion(db);
                    out += &format!("derive expanded to: {}\n", expansion.value.0.syntax_node());
                    if let Some(err) = &expansion.err {
                        out += &format!("derive failed: {}\n", err.render_to_string(db).message);
                    }
                }
            }
        }
    }
    out
}
