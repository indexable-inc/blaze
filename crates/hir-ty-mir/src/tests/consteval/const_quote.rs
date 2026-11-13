//! `const_ast::quote!` in a macro crate (docs/future/const-macros.md, "The tree API in const"):
//! a derive written with the quote expands, through the macro crate's analysis, the builder
//! calls the quote lowers to, and the host's check of the output, to the tree its text parses
//! to; a quote that splices a node where the parse put a token is refused by the host, naming
//! the element.

use syntax::{
    AstNode, Edition, NodeOrToken, SourceFile, SyntaxKind, SyntaxNode,
    ast::{self, HasModuleItem, HasName},
};

use super::const_ast::EVERY_VIEW;
use super::const_macro::{derives, load, with_const_ast};
use crate::test_db::TestDB;

/// `Fields` written with `quote!`: the struct's name spliced as the impl's type, its field
/// count and its name's length as integer literals. `Int` is a splice of its own, a literal,
/// since a quote's `#x` is any value with a `splice`.
const QUOTED_FIELDS: &str = r#"
#[derive(Clone, Copy)]
struct Int(&'static str);

impl Int {
    const fn splice(self, b: &mut const_ast::Builder<'static>, _wrap: &[const_ast::Kind]) {
        let literal = b.open(const_ast::Kind::LITERAL);
        b.token(self.0);
        b.close(literal);
    }
}

#[proc_macro_derive(Fields)]
pub const fn fields(item: &const_ast::Item) -> const_ast::Output {
    let mut built = const_ast::Builder::new(const_ast::Span::mixed_site());
    let b = &mut built;
    let (name, count) = match item.as_struct() {
        Some(adt) => match adt.name() {
            Some(name) => (
                name,
                match adt.field_list() {
                    Some(const_ast::FieldList::RecordFieldList(list)) => list.fields().count(),
                    _ => 0,
                },
            ),
            None => panic!("Fields: a struct has a name"),
        },
        None => panic!("Fields derives a struct"),
    };
    let count = Int(b.decimal(count as u128));
    let len = Int(b.decimal(name.text().as_bytes().len() as u128));
    const_ast::quote!(b, items =>
        impl Fields for #name {
            const COUNT: usize = #count;
            const NAME_LEN: usize = #len;
            const INPUT_SPAN: bool = true;
        }
    );
    built.finish(const_ast::Entry::Items)
}
"#;

/// The refinement: `Fields` written with `quote!` expands, for every struct of `EVERY_VIEW`,
/// to the tree of the text the stage-0 derive wrote for it, node for node (whitespace aside).
#[test]
fn a_quoted_derive_is_the_built_one() {
    let main = format!(
        "pub trait Fields {{ const COUNT: usize; const NAME_LEN: usize; const INPUT_SPAN: bool; }}\n{}",
        derived(EVERY_VIEW, "macros::Fields")
    );
    let fixture = with_const_ast(QUOTED_FIELDS, &main);
    let (db, _, _) = load(&fixture, "Fields");
    let built = blaze_query::db::attach(&db, || expansions(&db));
    let expected = expected_fields(EVERY_VIEW);
    assert_eq!(expected.len(), 3, "Record, Tuple and Unit");
    let mut built: Vec<String> =
        built.into_iter().filter(|it| it.contains("`Fields`")).collect();
    built.sort();
    assert_eq!(built, expected);
}

/// The control: a quote that splices a type (`#ty`, a `Type` view, which splices as its node)
/// where its parse has a name (`struct #ty;`) builds a tree that is not its tokens' parse, and
/// the host refuses it, naming the output element.
#[test]
fn a_quote_that_splices_a_type_where_a_name_goes_is_refused_by_the_host() {
    let fixture = with_const_ast(
        r#"
#[proc_macro_derive(Bad)]
pub const fn bad(item: &const_ast::Item) -> const_ast::Output {
    let mut built = const_ast::Builder::new(const_ast::Span::mixed_site());
    let b = &mut built;
    let ty = match item.as_struct() {
        Some(adt) => match adt.field_list() {
            Some(const_ast::FieldList::RecordFieldList(list)) => match list.fields().next() {
                Some(field) => match field.ty() {
                    Some(ty) => ty,
                    None => panic!("Bad: a field has a type"),
                },
                None => panic!("Bad: a struct with a field"),
            },
            _ => panic!("Bad: a struct with named fields"),
        },
        None => panic!("Bad derives a struct"),
    };
    const_ast::quote!(b, items => struct #ty;);
    built.finish(const_ast::Entry::Items)
}
"#,
        "pub trait Bad {}

#[derive(macros::Bad)]
pub struct Point { x: u8 }
",
    );
    let (db, _, _) = load(&fixture, "Bad");
    blaze_query::db::attach(&db, || {
        let main = *base_db::all_crates(&db).last().expect("the main crate");
        let derives = derives(&db, main);
        assert!(
            derives.contains("built `PATH_TYPE` at output element 2 where its tokens parse to `NAME`"),
            "{derives}"
        );
    });
}

/// `text` with `#[derive(DERIVE)]` on each struct.
fn derived(text: &str, derive: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("struct ") || trimmed.contains(" struct ") {
            out.push_str(&format!("#[derive({derive})]\n"));
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The shape of what stage 0's `Fields` wrote for each struct of `text`, sorted: the parse of
/// `impl Fields for NAME { const COUNT: usize = N; const NAME_LEN: usize = L; const
/// INPUT_SPAN: bool = true; }`.
fn expected_fields(text: &str) -> Vec<String> {
    let file = SourceFile::parse(text, Edition::CURRENT).tree();
    let mut out: Vec<String> = file
        .items()
        .filter_map(|item| match item {
            ast::Item::Struct(it) => Some(it),
            _ => None,
        })
        .map(|it| {
            let name = it.name().expect("a struct's name").text().to_string();
            let count = match it.field_list() {
                Some(ast::FieldList::RecordFieldList(list)) => list.fields().count(),
                _ => 0,
            };
            let impl_ = format!(
                "impl Fields for {name} {{ const COUNT: usize = {count}; const NAME_LEN: usize \
                 = {}; const INPUT_SPAN: bool = true; }}",
                name.len()
            );
            shape_of_items(SourceFile::parse(&impl_, Edition::CURRENT).tree().syntax())
        })
        .collect();
    out.sort();
    out
}

/// The shape of every derive's expansion in the last crate.
fn expansions(db: &TestDB) -> Vec<String> {
    let main = *base_db::all_crates(db).last().expect("the main crate");
    let def_map = hir_def::nameres::crate_def_map(db, main);
    let mut out = Vec::new();
    for (_, module) in def_map.modules(db) {
        for (_, invocations) in module.scope(db).derive_macro_invocs() {
            for (_, _, calls) in invocations {
                for call in calls.iter().flatten() {
                    let either::Either::Left(call) = call else { continue };
                    let expansion = call.parse_macro_expansion(db);
                    if let Some(err) = &expansion.err {
                        let message = err.render_to_string(db).message;
                        if message.contains("Fields") {
                            panic!("the derive failed: {message}\n{}", derives(db, main));
                        }
                        continue;
                    }
                    out.push(shape_of_items(&expansion.value.0.syntax_node()));
                }
            }
        }
    }
    out
}

/// The shape of `root`'s items: each element below it by depth, a node by kind and a token by
/// text, whitespace left out.
fn shape_of_items(root: &SyntaxNode) -> String {
    fn walk(node: &SyntaxNode, depth: usize, out: &mut String) {
        for child in node.children_with_tokens() {
            match child {
                NodeOrToken::Node(it) => {
                    out.push_str(&format!("{depth} {:?}\n", it.kind()));
                    walk(&it, depth + 1, out);
                }
                NodeOrToken::Token(it) if it.kind() == SyntaxKind::WHITESPACE => {}
                NodeOrToken::Token(it) => out.push_str(&format!("{depth} `{}`\n", it.text())),
            }
        }
    }
    let mut out = String::new();
    walk(root, 0, &mut out);
    out
}
