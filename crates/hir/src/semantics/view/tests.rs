//! The view [`Semantics`], node by node on one fixture: every node of the file and of every
//! macro expansion in it is asked for its defs, types, resolutions, ranges and ancestors, and
//! the answers are held to the stored ones in `expect/`. Until the handle `Semantics` was
//! deleted these tests checked each answer against the handle's for the same node.

use base_db::EditionedFileId;
use blaze_alloc_count::assert_at_most;
use expect_test::expect_file;
use hir_expand::HirFileId;
use syntax::view::{self, AstNode as _};
use hir_expand::fixture;

use super::{HasSourceView, Semantics};
use crate::ModuleDef;

const FIXTURE: &str = r#"
//- /main.rs crate:main
macro_rules! id { ($($t:tt)*) => { $($t)* }; }
macro_rules! make_fn { ($name:ident) => { fn $name() -> u32 { 1 } }; }

mod inner {
    pub struct Point { pub x: u32, pub y: u32 }
    pub struct Pair(pub u32, pub i64);
    pub enum Shape { Dot(Point), Line { from: Point, to: Point }, Empty = 3 }
    pub union Bits { pub a: u32, pub b: f32 }
    pub trait Area { const SIDES: u32; type Out; fn area(&self) -> u32; }
    impl Area for Point {
        const SIDES: u32 = 0;
        type Out = u32;
        fn area(&self) -> u32 { self.x * self.y }
    }
    impl Point { pub fn new(x: u32) -> Self { Point { x, y: x } } }
}

use inner::{Area, Pair, Point, Shape};

const LIMIT: usize = 4;
static NAME: &str = "n";
type Alias<'a, T, const N: usize> = &'a [T; N];

make_fn!(made);

fn generic<'a, T: Copy, const N: usize>(xs: &'a [T; N]) -> Option<&'a T> { xs.first() }

fn origin() -> Point { Point::new(0) }

fn main() {
    id! { let p = origin(); }
    let q = Point { x: 1, y: 2 };
    let Point { x, y: why } = q;
    let pair = Pair(1, 2);
    let Pair(first, _) = pair;
    let _sum = p.area() + x + why + first + pair.0 as u32 + made();
    let _s = Shape::Dot(p);
    let _l = Shape::Line { from: q, to: q };
    let arr = [0u8; LIMIT];
    let _g = generic(&arr);
    let add = |a: u32, b: u32| a + b;
    let _r = 'outer: loop { break 'outer add(id!(1), 2); };
    let _n = NAME.len() + <Point as Area>::SIDES as usize;
    match _s { Shape::Dot(Point { x: px, .. }) => { let _ = px; } Shape::Line { .. } | Shape::Empty => {} }
}
"#;

/// The file's root and the roots of every macro expansion under it, nested ones included,
/// each with its file.
fn roots<'db>(
    vsema: &Semantics<'db>,
    file_id: EditionedFileId,
) -> Vec<(HirFileId, view::Node<'db>)> {
    let mut roots = vec![(HirFileId::from(file_id), vsema.parse(file_id).syntax())];
    let mut at = 0;
    while let Some(&(_, root)) = roots.get(at) {
        at += 1;
        for call in root.descendants().filter_map(view::MacroCall::cast) {
            if let Some(expansion) = vsema.expand_macro_call(call) {
                roots.push((expansion.file_id, expansion.value));
            }
        }
    }
    roots
}

/// `out` with each raw interned id renumbered, per id type, in the order the answers first name
/// it: `Crate(Id(10240))` is `Crate(Id(#0))`, `FunctionId(8c03)` is `FunctionId(#0)`. A raw id is
/// its place in interning order, which anything interned before it moves; which answers name
/// the same id is what the stored files hold.
fn stable_ids(out: &str) -> String {
    use std::fmt::Write as _;
    let mut labels = blaze_hash::HashMap::<(&str, &str), usize>::default();
    let mut counts = blaze_hash::HashMap::<&str, usize>::default();
    let mut result = String::with_capacity(out.len());
    let (mut at, mut copied) = (0, 0);
    while let Some(found) = out[at..].find("Id(") {
        let open = at + found + 2;
        let name_start = out[..open]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(0, |it| it + 1);
        let name = &out[name_start..open];
        let end = out[open + 1..].find(|c: char| !c.is_ascii_hexdigit()).map(|it| open + 1 + it);
        match end {
            Some(end) if end > open + 1 && out[end..].starts_with(')') => {
                let raw = &out[open + 1..end];
                let label = *labels.entry((name, raw)).or_insert_with(|| {
                    let count = counts.entry(name).or_default();
                    *count += 1;
                    *count - 1
                });
                result.push_str(&out[copied..=open]);
                write!(result, "#{label}").unwrap();
                copied = end;
                at = end;
            }
            _ => at = open + 1,
        }
    }
    result.push_str(&out[copied..]);
    result
}

#[test]
fn stable_ids_renumber_by_first_use() {
    assert_eq!(
        stable_ids("Crate(Id(10240)) FileId(EditionedFileId(Id(13312))) Crate(Id(10240)) FunctionId(8c03) Idx::<FieldData>(0) FunctionId(\"f\")"),
        "Crate(Id(#0)) FileId(EditionedFileId(Id(#1))) Crate(Id(#0)) FunctionId(#0) Idx::<FieldData>(0) FunctionId(\"f\")",
    );
}

/// One line of a stored answer: what was asked, of the node at `range` in `file`.
fn line(out: &mut String, file: HirFileId, range: syntax::TextRange, what: &str, answer: impl std::fmt::Debug) {
    use std::fmt::Write as _;
    writeln!(out, "{file:?} {range:?} {what}: {answer:?}").unwrap();
}

macro_rules! record_def {
    ($vsema:ident, $out:ident, $file:ident, $node:ident, $checked:ident: $($kind:ident),* $(,)?) => {$(
        if let Some(v) = view::$kind::cast($node) {
            line(&mut $out, $file, $node.text_range(), concat!("to_def ", stringify!($kind)), $vsema.to_def(v));
            $checked += 1;
        }
    )*};
}

#[test]
fn to_def_of_every_node() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let vsema = Semantics::new(&db);
        let mut out = String::new();
        let mut checked = 0u32;
        for (file, root) in roots(&vsema, file_id) {
            for node in root.descendants() {
                record_def!(vsema, out, file, node, checked:
                    Module, Struct, Enum, Union, Trait, Impl, TypeAlias, Const, Static, Fn,
                    RecordField, TupleField, Variant, TypeParam, LifetimeParam, ConstParam,
                    Macro, SelfParam, Label, Adt, IdentPat, MacroCall,
                );
            }
        }
        assert!(checked > 60, "the fixture has defs of every kind, {checked} checked");
        expect_file!["expect/to_def.txt"].assert_eq(&stable_ids(&out));
    });
}

#[test]
fn ancestors_ranges_and_scopes_of_every_node() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let vsema = Semantics::new(&db);
        let mut out = String::new();
        for (file, root) in roots(&vsema, file_id) {
            for node in root.descendants() {
                let at = node.text_range();
                assert_eq!(vsema.hir_file_for(node), file, "the file of {at:?} in {file:?}");
                let ancestors: Vec<_> =
                    vsema.ancestors_with_macros(node).map(|it| (it.kind(), it.text_range())).collect();
                line(&mut out, file, at, "ancestors_with_macros", ancestors);
                line(&mut out, file, at, "original_range", vsema.original_range(node));
                line(&mut out, file, at, "original_range_opt", vsema.original_range_opt(node));
                line(
                    &mut out,
                    file,
                    at,
                    "scope",
                    vsema.scope(node).map(|it| (it.module(), it.krate(), it.file_id())),
                );
            }
        }
        expect_file!["expect/ancestors_ranges_scopes.txt"].assert_eq(&stable_ids(&out));
    });
}

#[test]
fn types_and_resolutions_of_every_node() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let vsema = Semantics::new(&db);
        let mut out = String::new();
        let (mut exprs, mut paths) = (0u32, 0u32);
        for (file, root) in roots(&vsema, file_id) {
            for node in root.descendants() {
                let at = node.text_range();
                let mut line = |what: &str, answer: &dyn std::fmt::Debug| line(&mut out, file, at, what, answer);
                if let Some(v) = view::Expr::cast(node) {
                    line("type_of_expr", &vsema.type_of_expr(v).map(|it| (it.original, it.adjusted)));
                    line(
                        "resolve_expr_as_callable",
                        &vsema.resolve_expr_as_callable(v).map(|it| it.return_type()),
                    );
                    exprs += 1;
                }
                if let Some(v) = view::Pat::cast(node) {
                    line("type_of_pat", &vsema.type_of_pat(v).map(|it| (it.original, it.adjusted)));
                }
                if let Some(v) = view::IdentPat::cast(node) {
                    line("type_of_binding_in_pat", &vsema.type_of_binding_in_pat(v));
                    line("resolve_bind_pat_to_const", &vsema.resolve_bind_pat_to_const(v));
                }
                if let Some(v) = view::Path::cast(node) {
                    line("resolve_path", &vsema.resolve_path(v));
                    let per_ns = vsema.resolve_path_per_ns(v).map(|it| (it.type_ns, it.value_ns, it.macro_ns));
                    line("resolve_path_per_ns", &per_ns);
                    let scope = vsema.scope(v.syntax()).expect("a path has a scope");
                    line("speculative_resolve_view", &scope.speculative_resolve_view(v));
                    paths += 1;
                }
                if let Some(v) = view::Type::cast(node) {
                    line("resolve_type", &vsema.resolve_type(v));
                }
                if let Some(v) = view::MethodCallExpr::cast(node) {
                    line("resolve_method_call", &vsema.resolve_method_call(v));
                    line(
                        "resolve_method_call_as_callable",
                        &vsema.resolve_method_call_as_callable(v).map(|it| it.return_type()),
                    );
                }
                if let Some(v) = view::FieldExpr::cast(node) {
                    line("resolve_field", &vsema.resolve_field(v));
                }
                if let Some(v) = view::RecordExprField::cast(node) {
                    line("resolve_record_field", &vsema.resolve_record_field(v));
                }
                if let Some(v) = view::RecordPatField::cast(node) {
                    line("resolve_record_pat_field", &vsema.resolve_record_pat_field(v));
                }
                if let Some(v) = view::TupleStructPat::cast(node) {
                    line("resolve_tuple_struct_pat_fields", &vsema.resolve_tuple_struct_pat_fields(v));
                }
                if let Some(v) = view::RecordExpr::cast(node) {
                    line("resolve_variant", &vsema.resolve_variant(v));
                }
                if let Some(v) = view::MacroCall::cast(node) {
                    line("resolve_macro_call", &vsema.resolve_macro_call(v));
                    line("resolve_macro_call_arm", &vsema.resolve_macro_call_arm(v));
                }
                if let Some(v) = view::Lifetime::cast(node) {
                    line("resolve_lifetime_param", &vsema.resolve_lifetime_param(v));
                    line("resolve_label", &vsema.resolve_label(v));
                }
                if let Some(v) = view::Item::cast(node) {
                    line("resolve_attr_macro_call", &vsema.resolve_attr_macro_call(v));
                }
            }
        }
        assert!(exprs > 50 && paths > 30, "{exprs} expressions, {paths} paths");
        expect_file!["expect/types_resolutions.txt"].assert_eq(&stable_ids(&out));
    });
}

#[test]
fn descending_every_token() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let vsema = Semantics::new(&db);
        let mut out = String::new();
        let mut into_macros = 0u32;
        for token in vsema.parse(file_id).syntax().descendants_with_tokens().filter_map(|it| it.into_token()) {
            if token.text_range().is_empty() {
                continue;
            }
            let views: Vec<_> = vsema
                .descend_into_macros_exact(token)
                .into_iter()
                .map(|it| (it.kind(), it.text_range(), vsema.hir_file_for(it.parent().unwrap())))
                .collect();
            into_macros += u32::from(views.iter().any(|(.., file)| file.is_macro()));
            let what = format!("descend_into_macros_exact {:?}", token.text());
            line(&mut out, file_id.into(), token.text_range(), &what, views);
        }
        assert!(into_macros >= 3, "the tokens of `id!` and `make_fn!` descend, {into_macros} did");
        expect_file!["expect/descend.txt"].assert_eq(&stable_ids(&out));
    });
}

#[test]
fn source_view_of_every_def() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        use std::fmt::Write as _;
        let vsema = Semantics::new(&db);
        let krate = vsema.file_to_module_def(file_id.file_id(&db)).expect("a module").krate(&db);
        let mut out = String::new();
        let mut checked = 0u32;
        for module in krate.modules(&db) {
            let def = module.definition_source_view(&db);
            writeln!(
                out,
                "{module:?} definition_source_view: {:?}",
                (def.file_id, def.value.node().kind(), def.value.node().text_range()),
            )
            .unwrap();
            writeln!(
                out,
                "{module:?} declaration_source_view: {:?}",
                module.declaration_source_view(&db).map(|it| (it.file_id, it.value.syntax().text_range())),
            )
            .unwrap();
            for decl in module.declarations(&db) {
                macro_rules! compare {
                    ($def:expr) => {{
                        let def = $def;
                        writeln!(
                            out,
                            "{def:?} source_view: {:?}",
                            def.source_view(&db).map(|it| (it.file_id, it.value.syntax().kind(), it.value.syntax().text_range())),
                        )
                        .unwrap();
                        checked += 1;
                    }};
                }
                match decl {
                    ModuleDef::Function(it) => compare!(it),
                    ModuleDef::Adt(it) => {
                        compare!(it);
                        for variant in it.as_enum().map(|it| it.variants(&db)).unwrap_or_default() {
                            compare!(variant);
                            for field in variant.fields(&db) {
                                compare!(field);
                            }
                        }
                        if let crate::Adt::Struct(s) = it {
                            for field in s.fields(&db) {
                                compare!(field);
                            }
                        }
                    }
                    ModuleDef::Const(it) => compare!(it),
                    ModuleDef::Static(it) => compare!(it),
                    ModuleDef::Trait(it) => compare!(it),
                    ModuleDef::TypeAlias(it) => compare!(it),
                    ModuleDef::Macro(it) => compare!(it),
                    _ => {}
                }
            }
            for impl_ in module.impl_defs(&db) {
                writeln!(
                    out,
                    "{impl_:?} source_view: {:?}",
                    impl_.source_view(&db).map(|it| (it.file_id, it.value.syntax().text_range())),
                )
                .unwrap();
                checked += 1;
            }
        }
        assert!(checked > 20, "{checked} defs checked");
        expect_file!["expect/source_view.txt"].assert_eq(&stable_ids(&out));
    });
}

#[test]
fn a_file_only_a_block_declares_is_a_module_whose_paths_resolve() {
    // No crate def map lists `m.rs`: only `main`'s block's def map has the module `m`.
    let fx = fixture::db(
        r#"
//- /main.rs crate:main
pub fn d() {}
fn main() {
    #[path = "m.rs"]
    mod m;
}
//- /m.rs
pub fn inner() {
    crate::d();
}
"#,
    );
    let m = *fx.files().last().expect("m.rs");
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let vsema = Semantics::new(&db);
        assert!(vsema.file_to_module_def(m.file_id(&db)).is_some(), "m.rs is the module `m` of `main`'s block");
        assert!(vsema.find_parent_file(HirFileId::from(m)).is_some(), "the `mod m;` declaring m.rs");
        let root = vsema.parse(m).syntax();
        let path = root
            .descendants()
            .filter_map(view::Path::cast)
            .find(|it| it.qualifier().is_some())
            .expect("`crate::d`");
        assert!(
            matches!(vsema.resolve_path(path), Some(crate::PathResolution::Def(ModuleDef::Function(_)))),
            "`crate::d` in m.rs resolves to the function `d`"
        );
    });
}

#[test]
fn a_warm_lookup_walk_allocates_nothing() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let vsema = Semantics::new(&db);
        let root = vsema.parse(file_id).syntax();
        let strukt = root.descendants().find_map(view::Struct::cast).expect("`struct Point`");
        let field = root.descendants().find_map(view::RecordField::cast).expect("field `x`");
        let walk = || {
            let s = vsema.to_def(strukt);
            let f = vsema.to_def(field);
            let file = vsema.hir_file_for(field.syntax());
            let depth = vsema.ancestors_with_macros(field.syntax()).count();
            (s, f, file, depth)
        };
        let warm = walk();
        let measured = assert_at_most(0, "to_def, hir_file_for and ancestors_with_macros", walk);
        assert_eq!(warm, measured);
        assert!(warm.0.is_some() && warm.1.is_some());
    });
}
