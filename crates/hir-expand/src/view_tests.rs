//! hir-expand's syntax APIs over views, over a database: a real file, and the macro files of
//! calls of builtin macros made by hand. Each answer, element by element, is held to the stored
//! one in `view_tests/` (they were held to the handle functions' until those were deleted with
//! the IDE); and the allocation budgets of the walks
//! (docs/v2/plan/lanes/frontend/tree-move-expand.md).

use std::{fmt::Write as _, ops::ControlFlow};

use base_db::{
    AbsPathBuf, Crate, CrateGraphBuilder, CrateOrigin, CrateWorkspaceData, Db, EditionedFileId,
    Env, FileId, FileSet, SourceRoot, SourceRootId, TargetKind, VfsPath,
};
use blaze_query::db::Durability;
use cfg::CfgOptions;
use expect_test::expect_file;
use span::{Edition, SyntaxContext};
use syntax::{
    AstPtr, SourceFile, TextRange, ast,
    view::{self, AstNode as _, HasName as _},
};
use triomphe::Arc;

use crate::{
    AstId, ExpandTo, ExpansionInfo, HirFileId, InFile, MacroCallId, MacroCallKind, MacroDefId,
    MacroDefKind,
    attrs::{
        AstKeyValueMetaExt, AstPathExt, AttrId, collect_item_tree_attrs_view,
        expand_cfg_attr_view,
    },
    builtin::{BuiltinFnLikeExpander, EagerExpander},
    eager::expand_eager_macro_input_view,
    files::{FileRangeWrapper, InRealFile},
    mod_path::{ModPath, PathKind},
    name::AsName,
};

const FIXTURE: &str = r#"
#[rustc_builtin_macro]
macro_rules! stringify { () => {} }
#[rustc_builtin_macro]
macro_rules! line { () => {} }
#[rustc_builtin_macro]
macro_rules! concat { () => {} }

/// The fixture's struct.
#[cfg_attr(all(), derive(Clone), allow(dead_code))]
#[repr(C)]
#[doc = "more"]
pub struct S { pub a: u32, b: (u8, i16) }

mod m { #![allow(unused)] pub mod g { pub fn r#try<T>(x: u32) -> u32 { x } } }

fn f(x: u32, v: S) -> u32 {
    let s = stringify!(a + b * c);
    let l = line!();
    let c = concat!("a", stringify!(b), line!(), concat!("c", 1), nope!());
    let S { a, b: (y, _) } = v;
    crate::m::g::r#try::<u8>(x) + self::h() + v.a + v.b.0 as u32 + super::k()
}
"#;

/// Calls in every position a macro call can be in, and some it can't.
const POSITIONS: &str = r#"
m!();
fn g() { m!(); let m!() = m!(); let _: m!() = (m!(), [m!()]); match m!() { m!() => m!() } }
struct T(m!());
impl T { m!(); }
trait U { m!(); }
extern "C" { m!(); }
const _: [u8; m!()] = [];
"#;

struct Fixture {
    db: Db,
    file: EditionedFileId,
    krate: Crate,
}

/// A database of one crate whose root is `FIXTURE`.
fn fixture() -> Fixture {
    let mut db = Db::for_tests();
    let id = FileId::from_raw(0);
    db.set_file_text_with_durability(id, FIXTURE, Durability::LOW);
    db.set_file_source_root_with_durability(id, SourceRootId(0), Durability::MEDIUM);
    let mut set = FileSet::default();
    set.insert(id, VfsPath::new_virtual_path("/a/lib.rs".to_owned()));
    let root = Arc::new(SourceRoot::new_local(set));
    db.set_source_root_with_durability(SourceRootId(0), root, Durability::MEDIUM);
    let mut graph = CrateGraphBuilder::default();
    let builder = graph.add_crate_root(
        id,
        Edition::CURRENT,
        None,
        None,
        CfgOptions::default(),
        None,
        Env::default(),
        CrateOrigin::Local { repo: None, name: None },
        Vec::new(),
        TargetKind::Lib { is_proc_macro: false },
        Arc::new(AbsPathBuf::assert_utf8(std::env::temp_dir())),
        Arc::new(CrateWorkspaceData {
            target: Err("no target in tests".into()),
            toolchain: None,
            const_eval: Default::default(),
        }),
    );
    let krate = graph.set_in_db(&mut db)[&builder];
    let file = EditionedFileId::new(&db, id, Edition::CURRENT);
    Fixture { db, file, krate }
}

/// The builtin macro the file's `macro_rules! name` stands for, as `kind` of its definition.
fn builtin(
    f: &Fixture,
    name: &str,
    kind: impl FnOnce(AstId<ast::Macro>) -> MacroDefKind,
) -> MacroDefId {
    let file = HirFileId::from(f.file);
    let rules = file
        .root(&f.db)
        .descendants()
        .filter_map(view::MacroRules::cast)
        .find(|it| it.name().is_some_and(|it| it.text() == name))
        .expect("the macro's definition");
    let ptr = AstPtr::<ast::Macro>::of(view::Macro::MacroRules(rules));
    let ast_id = InFile::new(file, file.ast_id_map(&f.db).ast_id_for_ptr(ptr));
    MacroDefId {
        krate: f.krate,
        edition: Edition::CURRENT,
        kind: kind(ast_id),
        local_inner: false,
        allow_internal_unsafe: false,
        edition_panic: false,
    }
}

/// The file's macro calls, in preorder, with their IDs.
fn calls(f: &Fixture) -> Vec<(view::MacroCall<'_>, AstId<ast::MacroCall>)> {
    let file = HirFileId::from(f.file);
    let map = file.ast_id_map(&f.db);
    file.root(&f.db)
        .descendants()
        .filter_map(view::MacroCall::cast)
        .map(|call| (call, InFile::new(file, map.ast_id_of_view::<ast::MacroCall>(call))))
        .collect()
}

fn name_of(call: view::MacroCall<'_>) -> Option<&str> {
    Some(call.path()?.segment()?.name_ref()?.text())
}

fn fn_like(f: &Fixture, name: &str, expander: BuiltinFnLikeExpander) -> MacroDefId {
    builtin(f, name, |id| MacroDefKind::BuiltIn(id, expander))
}

/// The calls of `stringify!` and `line!` of the file's body, each made a macro file.
fn macro_files(f: &Fixture) -> Vec<MacroCallId> {
    let stringify = fn_like(f, "stringify", BuiltinFnLikeExpander::Stringify);
    let line = fn_like(f, "line", BuiltinFnLikeExpander::Line);
    let made: Vec<_> = calls(f)
        .iter()
        .filter_map(|&(call, ast_id)| {
            let def = match name_of(call)? {
                "stringify" => stringify,
                "line" => line,
                _ => return None,
            };
            let expand_to = ExpandTo::of_call_site_view(ast_id.to_view(&f.db))
                .expect("a call in an expression");
            let kind = MacroCallKind::FnLike { ast_id, expand_to, eager: None };
            Some(def.make_call(&f.db, f.krate, kind, SyntaxContext::root(Edition::CURRENT), 0))
        })
        .collect();
    assert_eq!(made.len(), 2, "the `stringify!` and `line!` of `f`");
    made
}

/// The real file and the macro files.
fn files(f: &Fixture) -> Vec<HirFileId> {
    let mut files = vec![HirFileId::from(f.file)];
    files.extend(macro_files(f).into_iter().map(HirFileId::from));
    files
}

/// One line of a dump.
macro_rules! put {
    ($out:expr, $($arg:tt)*) => {
        writeln!($out, $($arg)*).expect("a `String` takes every write")
    };
}

/// A file as the dumps name it: `f` and its index in [`files`].
fn label(files: &[HirFileId], file: impl Into<HirFileId>) -> String {
    let file = file.into();
    match files.iter().position(|&it| it == file) {
        Some(index) => format!("f{index}"),
        None => format!("{file:?}"),
    }
}

/// A node as the dumps show it: its file, kind and range.
fn shown(files: &[HirFileId], node: InFile<view::Node<'_>>) -> String {
    format!("{}:{:?}@{:?}", label(files, node.file_id), node.value.kind(), node.value.text_range())
}

/// A file range as the dumps show it.
fn shown_range<F: Into<HirFileId>>(files: &[HirFileId], range: FileRangeWrapper<F>) -> String {
    format!("{}:{:?}", label(files, range.file_id), range.range)
}

fn root_ctxt(_: TextRange) -> SyntaxContext {
    SyntaxContext::root(Edition::CURRENT)
}

#[test]
fn roots_and_pointers() {
    let f = fixture();
    let db = &f.db;
    let files = files(&f);
    let mut out = String::new();
    for &file in &files {
        let (root, in_file) = (file.root(db), InFile::new(file, ()).root(db));
        let elements = root.descendants_with_tokens().count();
        put!(
            out,
            "{}, {elements} elements; in-file root {}",
            shown(&files, InFile::new(file, root)),
            shown(&files, InFile::new(file, in_file)),
        );
    }
    for (call, ast_id) in calls(&f) {
        let in_file = ast_id.to_in_file_view(db);
        let ptr = InFile::new(ast_id.file_id, ast_id.to_ptr(db)).to_view(db);
        put!(
            out,
            "{}! {}: view {}; in file {}; range {}; syntax {}; pointer {}",
            name_of(call).unwrap_or("?"),
            shown(&files, InFile::new(ast_id.file_id, call.syntax())),
            shown(&files, InFile::new(ast_id.file_id, ast_id.to_view(db).syntax())),
            shown(&files, in_file.map(|it| it.syntax())),
            shown_range(&files, in_file.node_file_range_view()),
            shown(&files, in_file.syntax_view()),
            shown(&files, InFile::new(ast_id.file_id, ptr.syntax())),
        );
    }
    expect_file!["view_tests/roots_and_pointers.txt"].assert_eq(&out);
}

#[test]
fn walks_and_ranges_up() {
    let f = fixture();
    let db = &f.db;
    let files = files(&f);
    let mut out = String::new();
    for &file in &files {
        for node in file.root(db).descendants() {
            let node = InFile::new(file, node);
            let up: Vec<_> = node.ancestors_with_macros_view(db).collect();
            let parents = node.parent_ancestors_with_macros_view(db).count();
            let crossings: Vec<_> = up
                .windows(2)
                .filter(|it| it[0].file_id != it[1].file_id)
                .map(|it| shown(&files, it[1]))
                .collect();
            let opt = node.original_file_range_opt_view(db);
            let rooted = node.original_syntax_node_rooted_view(db);
            put!(
                out,
                "{}: up {} (parents {parents}) crossing {crossings:?}; rooted {}; with input {}; \
                 opt {:?}; node rooted {:?}",
                shown(&files, node),
                up.len(),
                shown_range(&files, node.original_file_range_rooted_view(db)),
                shown_range(&files, node.original_file_range_with_macro_call_input_view(db)),
                opt.map(|(range, ctx)| (shown_range(&files, range), ctx)),
                rooted.map(|it| shown(&files, InFile::new(it.file_id.into(), it.value))),
            );
            if let Some(item) = view::Item::cast(node.value) {
                let rooted = InFile::new(file, item).original_ast_node_rooted_view(db);
                let rooted = rooted
                    .map(|it| shown(&files, InFile::new(it.file_id.into(), it.value.syntax())));
                put!(out, "  item rooted {rooted:?}");
            }
        }
        for token in file.root(db).descendants_with_tokens().filter_map(|it| it.into_token()) {
            let in_file = InFile::new(file, token);
            put!(
                out,
                "{}:{:?}@{:?}: {}, opt {:?}",
                label(&files, file),
                token.kind(),
                token.text_range(),
                shown_range(&files, in_file.original_file_range(db)),
                in_file.original_file_range_opt(db).map(|it| shown_range(&files, it)),
            );
        }
    }
    expect_file!["view_tests/walks_and_ranges_up.txt"].assert_eq(&out);
}

#[test]
fn calls_and_expansions() {
    let f = fixture();
    let db = &f.db;
    let files = files(&f);
    let mut out = String::new();
    fn real(it: InRealFile<view::Node<'_>>) -> InFile<view::Node<'_>> {
        InFile::new(it.file_id.into(), it.value)
    }
    for id in macro_files(&f) {
        let (loc, file) = (id.loc(db), HirFileId::from(id));
        put!(
            out,
            "{}: call {}; item {}; call node {}; the file's {:?}; original {:?}",
            label(&files, file),
            shown(&files, loc.to_node_view(db)),
            shown(&files, loc.to_node_item_view(db).map(|it| it.syntax())),
            shown(&files, id.call_node_view(db)),
            file.call_node_view(db).map(|it| shown(&files, it)),
            file.original_call_node_view(db).map(|it| shown(&files, real(it))),
        );

        let info = ExpansionInfo::new(db, id);
        let (expanded, arg) = (info.expanded_view(), info.arg_view());
        put!(
            out,
            "  expanded {}, {} elements; argument {:?}",
            shown(&files, InFile::new(expanded.file_id.into(), expanded.value)),
            expanded.value.descendants_with_tokens().count(),
            arg.value.map(|it| shown(&files, InFile::new(arg.file_id, it))),
        );
        let Some(arg_node) = arg.value else { continue };
        let map = arg.file_id.span_map(db);
        for token in arg_node.descendants_with_tokens().filter_map(|it| it.into_token()) {
            let span = map.span_for_range(token.text_range());
            let down = info.map_range_down_view(span).map(|it| {
                let tokens = it.value.map(|(t, ctx)| (t.text_range(), ctx));
                (label(&files, it.file_id), tokens.collect::<Vec<_>>())
            });
            let exact = info.map_range_down_exact_view(span).map(|it| {
                let tokens = it.value.map(|(t, ctx)| (t.text_range(), ctx));
                (label(&files, it.file_id), tokens.collect::<Vec<_>>())
            });
            let at = (token.kind(), token.text_range());
            put!(out, "  {:?}@{:?}: down {down:?}, exact {exact:?}", at.0, at.1);
        }
    }
    expect_file!["view_tests/calls_and_expansions.txt"].assert_eq(&out);
}

#[test]
fn what_a_call_expands_to_in_every_position() {
    use ExpandTo::*;
    let parse = SourceFile::parse(POSITIONS, Edition::CURRENT);
    let got: Vec<_> = parse
        .root()
        .descendants()
        .filter_map(view::MacroCall::cast)
        .map(ExpandTo::of_call_site_view)
        .collect();
    let expected = [
        // `m!();` in the file
        Items,
        // `fn g`: a statement, a pattern and its initializer, a type, a tuple's and an array's
        // element, a scrutinee, an arm's pattern and its body
        Statements,
        Pattern,
        Expr,
        Type,
        Expr,
        Expr,
        Expr,
        Pattern,
        Expr,
        // a tuple field's type, the impl's, trait's and extern block's items, an array length
        Type,
        Items,
        Items,
        Items,
        Expr,
    ];
    assert_eq!(got, expected.map(Ok));
}

#[test]
fn names_and_paths() {
    let f = fixture();
    let db = &f.db;
    let file = HirFileId::from(f.file);
    let mut out = String::new();
    for node in file.root(db).descendants() {
        let at = (node.kind(), node.text_range());
        if let Some(it) = view::NameRef::cast(node) {
            put!(out, "{at:?}: {:?}", it.as_name());
        }
        if let Some(it) = view::Name::cast(node) {
            put!(out, "{at:?}: {:?}", it.as_name());
        }
        if let Some(it) = view::RecordPatField::cast(node) {
            put!(out, "{at:?}: field {:?}", it.field_name().map(|it| it.as_name()));
        }
        if let Some(it) = view::FieldExpr::cast(node) {
            put!(out, "{at:?}: field {:?}", it.field_access().map(|it| it.as_name()));
        }
        if let Some(it) = view::Path::cast(node) {
            put!(
                out,
                "{at:?}: is `Clone` {}, one segment {:?}, up to two {:?}, qualifier's one {:?}",
                it.is1("Clone"),
                it.as_one_segment(),
                it.as_up_to_two_segment(),
                it.qualifier().as_one_segment(),
            );
        }
        if let Some(it) = view::KeyValueMeta::cast(node) {
            put!(out, "{at:?}: value {:?}", it.value_string());
        }
    }
    expect_file!["view_tests/names_and_paths.txt"].assert_eq(&out);
}

#[test]
fn paths_lower_to_mod_paths() {
    let f = fixture();
    let db = &f.db;
    let lowered: Vec<_> = HirFileId::from(f.file)
        .root(db)
        .descendants()
        .filter_map(view::Path::cast)
        .filter(|it| it.syntax().parent().and_then(view::Path::cast).is_none())
        .map(|it| {
            let path = ModPath::from_src_view(db, it, &mut root_ctxt);
            let path = path.map(|it| (it.kind, it.display_verbatim(db).to_string()));
            (it.syntax().text().to_string(), path)
        })
        .collect();
    for (text, kind, display) in [
        ("self::h", PathKind::SELF, "self::h"),
        ("super::k", PathKind::Super(1), "super::k"),
        ("stringify", PathKind::Plain, "stringify"),
        ("u32", PathKind::Plain, "u32"),
    ] {
        let want = (text.to_owned(), Some((kind, display.to_owned())));
        assert!(lowered.contains(&want), "{text}: {lowered:?}");
    }
    let call = lowered.iter().find(|(text, _)| text.starts_with("crate::m::g::"));
    let call = call.expect("the path of the call of `r#try`");
    assert_eq!(call.1.as_ref().map(|it| it.0), Some(PathKind::Crate), "{call:?}");
}

#[test]
fn attributes() {
    let f = fixture();
    let db = &f.db;
    let cfg = CfgOptions::default();
    let file = HirFileId::from(f.file);
    let mut out = String::new();
    let mut adts = 0;
    for node in file.root(db).descendants() {
        let Some(owner) = view::AnyHasAttrs::cast(node) else { continue };
        let at = (node.kind(), node.text_range());
        let mut metas = Vec::new();
        let _ = expand_cfg_attr_view(view::attrs_including_inner(&owner), || &cfg, |meta, top| {
            metas.push((meta.syntax().text_range(), top.syntax().text_range()));
            ControlFlow::<()>::Continue(())
        });
        let mut kept = Vec::new();
        let result = collect_item_tree_attrs_view(owner, || &cfg, |meta, top| {
            kept.push((meta.syntax().text_range(), top.syntax().text_range()));
            ControlFlow::<()>::Continue(())
        });
        put!(out, "{at:?}: metas {metas:?}; kept {kept:?}, stopped {}", result.is_some());

        if let Some(adt) = view::Adt::cast(node) {
            let ast_id = InFile::new(file, file.ast_id_map(db).ast_id_of_view::<ast::Adt>(adt));
            for index in 0..kept.len() as u32 {
                let id = AttrId::from_item_tree_index(index);
                let (attr, meta) = id.find_attr_range_view(db, f.krate, ast_id);
                let owner = view::AnyHasAttrs::new(adt);
                let (_, sourced) = id.find_attr_range_with_source_view(db, f.krate, owner);
                put!(
                    out,
                    "  attribute {index}: {:?}, meta {:?}, with its source {:?}",
                    attr.syntax().text_range(),
                    meta.syntax().text_range(),
                    sourced.syntax().text_range(),
                );
            }
            // `derive(Clone)`, `allow(dead_code)`, `repr(C)`, `doc = ..`; the item tree keeps
            // only the `derive`.
            assert_eq!((metas.len(), kept.len()), (4, 1), "{at:?}");
            adts += 1;
        }
    }
    assert_eq!(adts, 1, "`S`");
    expect_file!["view_tests/attributes.txt"].assert_eq(&out);
}

#[test]
fn an_eager_expansion_replaces_its_nested_calls() {
    let f = fixture();
    let db = &f.db;
    let stringify = fn_like(&f, "stringify", BuiltinFnLikeExpander::Stringify);
    let line = fn_like(&f, "line", BuiltinFnLikeExpander::Line);
    let concat = builtin(&f, "concat", |id| MacroDefKind::BuiltInEager(id, EagerExpander::Concat));
    let resolver = |path: &ModPath| match path.as_ident()?.as_str() {
        "stringify" => Some(stringify),
        "line" => Some(line),
        "concat" => Some(concat),
        _ => None,
    };
    let ctxt = SyntaxContext::root(Edition::CURRENT);
    let mut expanded = 0;
    for (call, ast_id) in calls(&f) {
        if name_of(call) != Some("concat") {
            continue;
        }
        let mut nested = Vec::new();
        let result = expand_eager_macro_input_view(
            db, f.krate, call, ast_id, concat, ctxt, 0, 128, &resolver, &mut nested,
        );
        assert!(result.err.is_some(), "`nope!` is unresolved");
        let id = result.value.expect("the call expands past the unresolved one");
        let defs: Vec<_> = nested.iter().map(|(_, id)| id.loc(db).def).collect();
        assert_eq!(defs, [stringify, line, concat], "the nested calls, in preorder");
        let MacroCallKind::FnLike { eager: Some(info), .. } = &id.loc(db).kind else {
            panic!("an eager call");
        };
        let arg = info.arg.to_string();
        for kept in ["\"a\"", "\"b\"", "\"c1\"", "nope"] {
            assert!(arg.contains(kept), "{kept} in {arg}");
        }
        for replaced in ["stringify", "line", "concat"] {
            assert!(!arg.contains(replaced), "{replaced} in {arg}");
        }
        expanded += 1;
    }
    assert_eq!(expanded, 1);
}

#[test]
fn the_twins_walks_allocate_nothing() {
    let f = fixture();
    let db = &f.db;
    for file in files(&f) {
        let root = file.root(db);
        let walk = || {
            root.descendants()
                .map(|node| InFile::new(file, node).ancestors_with_macros_view(db).count())
                .sum::<usize>()
        };
        let warm = walk();
        let (again, used) = blaze_alloc_count::measure(walk);
        assert_eq!(again, warm);
        assert_eq!(used.allocations, 0, "{file:?}: the walk up from every node, budget 0");
    }

    let root = HirFileId::from(f.file).root(db);
    let names =
        || root.descendants().filter_map(view::NameRef::cast).map(|it| it.as_name()).count();
    let warm = names();
    let (again, used) = blaze_alloc_count::measure(names);
    assert_eq!(again, warm);
    assert_eq!(used.allocations, 0, "a `Name` of every name reference, warm, budget 0");

    let parse = SourceFile::parse(POSITIONS, Edition::CURRENT);
    let kinds = || {
        parse
            .root()
            .descendants()
            .filter_map(view::MacroCall::cast)
            .filter(|it| ExpandTo::of_call_site_view(*it).is_ok())
            .count()
    };
    let warm = kinds();
    let (again, used) = blaze_alloc_count::measure(kinds);
    assert_eq!(again, warm);
    assert_eq!(used.allocations, 0, "what each call expands to, budget 0");

    let cfg = CfgOptions::default();
    let item = SourceFile::parse("#[a] #[b(c)] #[c = \"d\"] struct E;", Edition::CURRENT);
    let owner = view::AnyHasAttrs::from(
        item.root().descendants().find_map(view::Struct::cast).expect("the struct"),
    );
    let attrs = || {
        let mut n = 0;
        let _ = collect_item_tree_attrs_view(owner, || &cfg, |_, _| {
            n += 1;
            ControlFlow::<()>::Continue(())
        });
        n
    };
    let warm = attrs();
    let (again, used) = blaze_alloc_count::measure(attrs);
    assert_eq!((again, warm), (3, 3));
    assert_eq!(used.allocations, 0, "an item's attributes, budget 0");
}
