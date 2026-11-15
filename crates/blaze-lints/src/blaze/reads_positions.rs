//! `reads_positions`: a function of the front end's query crates that reads where code is
//! written (a source map, the live parse, an ast id's place in its file) and is not marked
//! `#[blaze::renders_positions]` (rules `semantics-in-front-end` and `keys-cover-every-input`).
//!
//! The rule: a query's result depends on what it reads. A query that reads a position depends on
//! every edit above that position, a comment or a blank line included, so its result is never
//! kept across one: every borrow check read its body's source map, so no borrow-check result was
//! ever kept; `const_check` and the attribute queries read the whole file's parse and reran on
//! any edit. The semantic queries read the position-free forms (the body, the signatures, the
//! item tree, the outline) and leave positions to the functions whose output is about them: the
//! wording of a diagnostic, debug info, an IDE lookup. Those are marked
//! `#[blaze::renders_positions]` (a blaze extension: `AttrFlags::RENDERS_POSITIONS`, inert under
//! rustc with `#![feature(register_tool)]` and `#![register_tool(blaze)]` at the crate root).
//!
//! The readers are the base readers ([`base_reader`]), by resolved path:
//!
//! - `base_db`: `EditionedFileId::parse`;
//! - `hir_expand`: `HirFileId::{root, parse_or_expand, ast_id_map}`, and of `InFile` (an
//!   `AstId`, an `InFile<AstPtr>`) `root`, `file_syntax`, `to_node`, `to_view`, `to_ptr`,
//!   `to_range`, `to_in_file_view`, `to_in_file_node`;
//! - `hir_def`: an associated `source_map`, `relative`, `unmoved_source_map`, `unmoved` or
//!   `with_source_map` (`Body`'s, `ExpressionStore`'s, each signature's, `VariantFields`',
//!   `GenericParams`'), a `*_syntax` method or `self_param` of a source map
//!   (`ExpressionStoreSourceMap`, `BodySourceMap` and their persisted `*Ref`s), any
//!   `source_view` or `*_source_view`, and `source` and `ast_ptr` of `hir_def::src`
//!   (`HasSource`);
//!
//! and every function marked `#[blaze::renders_positions]`. A call of a reader from a body
//! (its closures included) is a finding when:
//!
//! - the body is of one of the query crates: `hir_def`, `hir_ty`, `hir_ty_mir`,
//!   `hir_ty_borrowck`, `hir_ty_unsafeck`, `hir_ty_pure`, `hir_ty_codegen`, `blaze_items`,
//!   `blaze_item_hash`;
//! - its function is not marked, and is not itself a base reader (`Body::with_source_map`
//!   calling `Body::source_map` is the reader, not a reader of it);
//! - it is not in one of `hir_def`'s lowering modules, `item_tree`, `nameres`, `expr_store`,
//!   `signatures` and `src`: a lowering builds the position-free form (the item tree, the def
//!   map, the stores, the outline) and its source map from the syntax, so reading the syntax is
//!   its work, and it is held to its firewall by its own equality checks, not by this lint. A
//!   function there that hands a position on to its caller is marked, so that its callers
//!   elsewhere are found;
//! - it is not test code.
//!
//! The allowance passes to a callee only through a marked function: an unmarked helper that
//! reads a position is a finding even if only marked functions call it, and an unmarked
//! function that calls a marked one is a finding, since it reads positions too.
//!
//! Not found: a reader passed as a value (`.map(Body::source_map)`), which is no call; a source
//! map's `node_*` methods, which go from a node to an id: the node came from a reader.
//! There is no fix: the position-free fact is the caller's to find.

use base_db::Db;
use hir_def::{
    AttrDefId, DefWithBodyId, FunctionId, HasModule as _, attrs::AttrFlags, hir::Expr,
    signatures::FunctionSignature,
};

use super::{Finding, Lint, alloc::written_callee, facts, sans_io_core::fn_name};
use crate::check::Cx;

const LINT: Lint = Lint::ReadsPositions;

/// The front end's query crates: where a position read makes a query's result depend on every
/// edit above it.
const QUERY_CRATES: &[&str] = &[
    "hir_def",
    "hir_ty",
    "hir_ty_mir",
    "hir_ty_borrowck",
    "hir_ty_unsafeck",
    "hir_ty_pure",
    "hir_ty_codegen",
    "blaze_items",
    "blaze_item_hash",
];

/// `hir_def`'s modules that lower syntax to the position-free forms and their source maps.
const LOWERING: &[&str] = &["item_tree", "nameres", "expr_store", "signatures", "src"];

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let module = hir::Module::from(def.module(db));
    let krate = module.krate(db);
    if !QUERY_CRATES.iter().any(|it| facts::crate_is(db, krate, it)) || in_lowering(db, module) {
        return found;
    }
    let owner = match def {
        DefWithBodyId::FunctionId(f) => Some(f),
        _ => None,
    };
    if owner.is_some_and(|f| marked(db, f) || base_reader(db, f)) || cx.is_in_test(cx.root()) {
        return found;
    }
    // alloc: a message's part
    let owner_name = owner.map_or_else(|| "this body".to_owned(), |f| format!("`{}`", fn_name(db, f)));
    let body = analysis.body();
    let infer = analysis.infer();
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((f, _)) = written_callee(infer, body, id) else { continue };
        let through = if base_reader(db, f) {
            ""
        } else if marked(db, f) {
            ", which is marked `#[blaze::renders_positions]`"
        } else {
            continue;
        };
        let Some(range) = analysis.expr_range(id) else { continue };
        let message = format!(
            "{owner_name} reads where code is written through `{}`{through}, and is not marked \
             `#[blaze::renders_positions]`",
            fn_name(db, f)
        );
        // alloc: a message's part
        let note = "a query that reads a position (a source map, the live parse, an ast id's range) depends on \
                    every edit above it in its file, a comment included, so its result is never kept across \
                    one (keys-cover-every-input): read the position-free fact instead (the body, \
                    `ExpressionStore::is_synthesized`, the outline, the item index)"
            .to_owned();
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}

/// Whether `f` is marked `#[blaze::renders_positions]`: its output is about where code is
/// written.
fn marked(db: &Db, f: FunctionId) -> bool {
    AttrFlags::query(db, AttrDefId::FunctionId(f)).contains(AttrFlags::RENDERS_POSITIONS)
}

/// Whether `module` is in one of `hir_def`'s [`LOWERING`] modules.
fn in_lowering(db: &Db, module: hir::Module) -> bool {
    if !facts::crate_is(db, module.krate(db), "hir_def") {
        return false;
    }
    let top = module.path_to_root(db).into_iter().rev().find_map(|it| it.name(db));
    top.is_some_and(|it| LOWERING.contains(&it.as_str()))
}

/// Whether `f` is a base reader: a function that hands back where code is written, by its
/// crate, its impl's type and its name (the module doc lists them).
pub(super) fn base_reader(db: &Db, f: FunctionId) -> bool {
    let module = hir::Function::from(f).module(db);
    let krate = module.krate(db);
    let is = |name: &str| facts::crate_is(db, krate, name);
    let name = FunctionSignature::of(db, f).name.as_str();
    let adt = facts::impl_adt(db, f).map(|it| it.name(db));
    let adt = adt.as_ref().map(hir::Name::as_str);
    if is("base_db") {
        return adt == Some("EditionedFileId") && name == "parse";
    }
    if is("hir_expand") {
        return match adt {
            Some("HirFileId") => matches!(name, "root" | "parse_or_expand" | "ast_id_map"),
            Some("InFileWrapper") => matches!(
                name,
                "root"
                    | "file_syntax"
                    | "to_node"
                    | "to_view"
                    | "to_ptr"
                    | "to_range"
                    | "to_in_file_view"
                    | "to_in_file_node"
            ),
            _ => false,
        };
    }
    if !is("hir_def") {
        return false;
    }
    if name == "source_view" || name.ends_with("_source_view") {
        return true;
    }
    match adt {
        Some(
            "ExpressionStoreSourceMap"
            | "BodySourceMap"
            | "ExpressionStoreSourceMapRef"
            | "BodySourceMapRef",
        ) => name.ends_with("_syntax") || name == "self_param",
        Some(_) => matches!(
            name,
            "source_map" | "relative" | "unmoved_source_map" | "unmoved" | "with_source_map"
        ),
        None => {
            matches!(name, "source" | "ast_ptr")
                && module.name(db).is_some_and(|it| it.as_str() == "src")
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const FOUND: &str = "reads where code is written through";

    /// A `base_db` with the live parse and a `hir_def` with a body's source map, by the paths
    /// the real ones have.
    const CRATES: &str = r#"
//- minicore: sized, copy, clone, option, result, fn
//- /base_db/lib.rs crate:base_db
pub struct Db;
pub struct Parse(pub u32);
#[derive(Clone, Copy)]
pub struct EditionedFileId(pub u32);
impl EditionedFileId {
    pub fn parse(self, _db: &Db) -> Parse { Parse(self.0) }
}
//- /hir_def/lib.rs crate:hir_def deps:base_db
pub mod expr_store {
    use base_db::Db;
    #[derive(Clone, Copy)]
    pub struct ExprId(pub u32);
    pub struct ExpressionStoreSourceMap(pub u32);
    impl ExpressionStoreSourceMap {
        pub fn expr_syntax(&self, _expr: ExprId) -> Result<u32, ()> { Err(()) }
        pub fn patterns_for_binding(&self, _binding: u32) -> u32 { self.0 }
    }
    pub struct Body(pub u32);
    impl Body {
        pub fn of(_db: &Db, def: u32) -> Body { Body(def) }
        pub fn source_map(_db: &Db, def: u32) -> ExpressionStoreSourceMap { ExpressionStoreSourceMap(def) }
        pub fn is_synthesized(&self, _expr: ExprId) -> bool { false }
    }
}
"#;

    fn found(main: &str) -> String {
        let fixture = format!(
            "{}//- /src/lib.rs crate:hir_ty deps:base_db,hir_def\n#![feature(register_tool)]\n#![register_tool(blaze)]\n{}",
            CRATES.trim_start_matches('\n'),
            main.trim_start_matches('\n')
        );
        lint(&fixture, "-A clippy::all --warn=blaze::reads_positions")
    }

    /// The bug in miniature: a borrow check that asks the source map whether an expression was
    /// written, an attribute query that reads the live parse from a closure, and a function
    /// that calls a marked one; the marked function itself is not found.
    #[test]
    fn a_query_reading_positions_is_found() {
        let out = found(
            r#"
use base_db::{Db, EditionedFileId};
use hir_def::expr_store::{Body, ExprId};
pub fn borrowck(db: &Db, def: u32) -> bool {
    let map = Body::source_map(db, def);
    map.expr_syntax(ExprId(0)).is_err()
}
pub fn attrs(db: &Db, file: EditionedFileId) -> u32 {
    let read = |file: EditionedFileId| file.parse(db).0;
    read(file)
}
#[blaze::renders_positions]
pub fn diagnostic(db: &Db, def: u32) -> Result<u32, ()> {
    Body::source_map(db, def).expr_syntax(ExprId(0))
}
pub fn words_a_diagnostic(db: &Db, def: u32) -> bool {
    diagnostic(db, def).is_ok()
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 4, "{out}");
        assert!(out.contains("`borrowck` reads where code is written through `Body::source_map`"), "{out}");
        assert!(
            out.contains("`borrowck` reads where code is written through `ExpressionStoreSourceMap::expr_syntax`"),
            "{out}"
        );
        assert!(out.contains("`attrs` reads where code is written through `EditionedFileId::parse`"), "{out}");
        assert!(
            out.contains("`words_a_diagnostic` reads where code is written through `diagnostic`, which is marked"),
            "{out}"
        );
        assert!(!out.contains("`diagnostic` reads"), "{out}");
        assert!(out.contains("keys-cover-every-input"), "{out}");
    }

    /// The fix: the position-free fact (`Body::is_synthesized`), a source map's id lookup on a
    /// map handed in, and a test that reads the source map are not found.
    #[test]
    fn position_free_facts_and_tests_are_left_alone() {
        let out = found(
            r#"
use base_db::Db;
use hir_def::expr_store::{Body, ExprId, ExpressionStoreSourceMap};
pub fn borrowck(db: &Db, def: u32) -> bool {
    Body::of(db, def).is_synthesized(ExprId(0))
}
pub fn bindings(map: &ExpressionStoreSourceMap) -> u32 {
    map.patterns_for_binding(0)
}
#[test]
fn reads_the_map() {
    let _ = Body::source_map(&Db, 0).expr_syntax(ExprId(0));
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }

    /// An `#[expect]` on a finding leaves no output, and no unfulfilled expectation.
    #[test]
    fn an_expect_on_a_finding_is_fulfilled() {
        let out = found(
            r#"
use base_db::Db;
use hir_def::expr_store::{Body, ExprId};
#[expect(blaze::reads_positions, reason = "the body's own synthesized marks are not kept yet")]
pub fn borrowck(db: &Db, def: u32) -> bool {
    Body::source_map(db, def).expr_syntax(ExprId(0)).is_err()
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
        assert!(!out.contains("unfulfilled"), "{out}");
    }

    /// `hir_def`'s lowering modules read syntax by design and are not found; its other modules
    /// are, and a crate outside the front end is not linted.
    #[test]
    fn the_lowering_and_crates_outside_the_front_end_are_not_linted() {
        let fixture = r#"
//- minicore: sized, copy, clone, option, result, fn
//- /base_db/lib.rs crate:base_db
pub struct Db;
pub struct Parse(pub u32);
#[derive(Clone, Copy)]
pub struct EditionedFileId(pub u32);
impl EditionedFileId {
    pub fn parse(self, _db: &Db) -> Parse { Parse(self.0) }
}
//- /src/lib.rs crate:hir_def deps:base_db
pub mod item_tree {
    pub fn lower(db: &base_db::Db, file: base_db::EditionedFileId) -> u32 {
        file.parse(db).0
    }
}
pub mod attrs {
    pub fn crate_attrs(db: &base_db::Db, file: base_db::EditionedFileId) -> u32 {
        file.parse(db).0
    }
}
"#;
        let out = lint(fixture, "-A clippy::all --warn=blaze::reads_positions");
        assert_eq!(out.matches(FOUND).count(), 1, "{out}");
        assert!(out.contains("`crate_attrs` reads where code is written"), "{out}");
        let elsewhere = fixture.replace("crate:hir_def", "crate:blaze_cargo");
        let out = lint(&elsewhere, "-A clippy::all --warn=blaze::reads_positions");
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }
}
