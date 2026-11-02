//! What an edit of a file makes lowered again. A body is lowered from its own text
//! ([`crate::expr_store::body::BodySyntax`]), so an edit that changes nothing a body is made of
//! leaves its store as it was, and an edit of one body leaves every other body and every
//! signature as they were, not lowered again.

use base_db::Db;
use hir_expand::fixture;
use triomphe::Arc;

use crate::{
    AssocItemId, DefWithBodyId, GenericDefId, ModuleDefId,
    expr_store::{Body, scope::ExprScopes},
    hir::generics::GenericParams,
    nameres::{assoc::{ImplItems, TraitItems}, crate_def_map},
    signatures::FunctionSignature,
    test_db::{TestDB, TestDBExt},
};

const CODE: &str = r#"
pub struct S { a: u32 }

impl S {
    pub fn get(&self) -> u32 { self.a }
    pub fn add(&self, b: u32) -> u32 {
        let c = self.get() + b;
        c * 2
    }
}

pub fn first(x: u32) -> u32 {
    let y = x + 1;
    if y > 2 { y } else { 0 }
}

pub fn second(x: u32) -> u32 {
    match x {
        0 => first(x),
        n => n - 1,
    }
}

pub const K: u32 = 3;
pub static L: u32 = K + 1;
"#;

/// What reads a signature's store: whether it ran again is whether the store changed.
#[blaze_query::tracked(returns(clone))]
fn signature_store_bytes(db: &Db, def: GenericDefId) -> Vec<u8> {
    let (_, store) = GenericParams::with_store(db, def);
    blaze_format::to_bytes(store)
}

/// Every body and every signature of the test crate, in the order the crate declares them.
fn defs(db: &TestDB) -> (Vec<DefWithBodyId>, Vec<GenericDefId>) {
    let (mut bodies, mut signatures) = (Vec::new(), Vec::new());
    for (_, module) in crate_def_map(db, db.fetch_test_crate()).modules(db) {
        let scope = module.scope(db);
        for decl in scope.declarations() {
            match decl {
                ModuleDefId::FunctionId(it) => {
                    bodies.push(it.into());
                    signatures.push(it.into());
                }
                ModuleDefId::ConstId(it) => {
                    bodies.push(it.into());
                    signatures.push(it.into());
                }
                ModuleDefId::StaticId(it) => {
                    bodies.push(it.into());
                    signatures.push(it.into());
                }
                ModuleDefId::AdtId(it) => signatures.push(it.into()),
                _ => {}
            }
        }
        for imp in scope.impls() {
            signatures.push(imp.into());
            for &(_, item) in ImplItems::of(db, imp).0.items.iter() {
                if let AssocItemId::FunctionId(it) = item {
                    bodies.push(it.into());
                    signatures.push(it.into());
                }
            }
        }
    }
    (bodies, signatures)
}

fn function_named(db: &TestDB, name: &str) -> DefWithBodyId {
    let (bodies, _) = defs(db);
    let named = |def: &DefWithBodyId| match *def {
        DefWithBodyId::FunctionId(it) => FunctionSignature::of(db, it).name.as_str() == name,
        _ => false,
    };
    *bodies.iter().find(|it| named(it)).unwrap_or_else(|| panic!("no fn `{name}`"))
}

/// The body of `def` lowered from its own text, as the query keeps it.
fn relative_body(db: &TestDB, def: DefWithBodyId) -> Arc<Body> {
    Body::relative(db, def).as_ref().expect("a body written in a file").0.clone()
}

fn count(log: &[String], query: &str) -> usize {
    log.iter().filter(|it| *it == query).count()
}

/// (a) `edited` changes only whitespace or comments in `first`'s body: the store stays equal,
/// and so do its bytes; the source map, which says where each part is, moves.
#[track_caller]
fn check_store_unchanged_by(edited: &str) {
    assert_ne!(edited, CODE);
    let fx = fixture::db(CODE);
    let file = fx.only_file();
    let mut db = fx.into_db();
    let def = function_named(&db, "first");
    let before = relative_body(&db, def);
    let bytes = blaze_format::to_bytes(&*before);
    let map = blaze_format::to_bytes(&Body::with_source_map(&db, def).1);

    db.set_file_text(file.file_id(&db), edited);
    let after = Body::of(&db, def);
    assert_eq!(*before, *after, "the store changed for:\n{edited}");
    assert_eq!(bytes, blaze_format::to_bytes(after), "the store's bytes changed for:\n{edited}");
    let moved = blaze_format::to_bytes(&Body::with_source_map(&db, def).1);
    assert_ne!(map, moved, "the source map did not move for:\n{edited}");
}

#[test]
fn whitespace_in_a_body_keeps_its_store() {
    check_store_unchanged_by(&CODE.replace("let y = x + 1;", "let   y =\n        x  +  1 ;"));
}

#[test]
fn comments_in_a_body_keep_its_store() {
    check_store_unchanged_by(&CODE.replace("let y = x + 1;", "let y = x /* one */ + 1; // two"));
}

/// (b) An edit of `second`'s body lowers that body again and nothing else: every other body's
/// store is the one kept from before (not lowered again), and every signature's store is equal,
/// so nothing that reads one runs again. Nothing here reads a source map, so `Body::relative`
/// doesn't run at all.
#[test]
fn an_edit_in_one_body_lowers_only_that_body() {
    let fx = fixture::db(CODE);
    let file = fx.only_file();
    let mut db = fx.into_db();
    let (bodies, signatures) = defs(&db);
    assert_eq!((bodies.len(), signatures.len()), (6, 8), "{bodies:?}\n{signatures:?}");
    let edited = function_named(&db, "second");
    // `Body::of` lends its body only until the next edit: keep where it was and its bytes.
    let before: Vec<(*const Body, Vec<u8>)> = bodies
        .iter()
        .map(|&def| {
            let body = Body::of(&db, def);
            (std::ptr::from_ref(body), blaze_format::to_bytes(body))
        })
        .collect();
    for &def in &bodies {
        ExprScopes::body_expr_scopes(&db, def);
    }
    let signatures_before: Vec<Vec<u8>> =
        signatures.iter().map(|&def| signature_store_bytes(&db, def)).collect();

    db.set_file_text(file.file_id(&db), &CODE.replace("n => n - 1,", "n => n - 2,"));
    let log = db.executed_in_read_order(|| {
        for &def in &bodies {
            ExprScopes::body_expr_scopes(&db, def);
        }
        for &def in &signatures {
            signature_store_bytes(&db, def);
        }
    });
    for query in ["Body::of", "ExprScopes::body_expr_scopes"] {
        assert_eq!(count(&log, query), 1, "`{query}` ran for more than the edited body: {log:#?}");
    }
    assert_eq!(count(&log, "Body::relative"), 0, "a source map was lowered: {log:#?}");
    assert_eq!(count(&log, "signature_store_bytes"), 0, "a signature's store changed: {log:#?}");

    for (&def, (at, bytes)) in bodies.iter().zip(&before) {
        let after = Body::of(&db, def);
        if def == edited {
            assert_ne!(*bytes, blaze_format::to_bytes(after), "the edit changed nothing of `second`");
            continue;
        }
        assert_eq!(*bytes, blaze_format::to_bytes(after), "{def:?}");
        assert!(std::ptr::eq(*at, after), "{def:?} was lowered again");
    }
    for (&def, before) in signatures.iter().zip(&signatures_before) {
        assert_eq!(*before, signature_store_bytes(&db, def), "{def:?}");
    }
}

/// (b') The item tree of a block with items is lowered from its body's own text: an edit of one
/// body lowers again only that body's block's tree, and every block is owned by its body.
#[test]
fn an_edit_in_one_body_lowers_only_its_blocks_item_tree() {
    const BLOCKS: &str = r#"
pub fn first(x: u32) -> u32 {
    struct A;
    { fn inner() -> u32 { 1 } inner() + x }
}

pub fn second(x: u32) -> u32 {
    struct B;
    x + 2
}
"#;
    let fx = fixture::db(BLOCKS);
    let file = fx.only_file();
    let mut db = fx.into_db();
    let (bodies, _) = defs(&db);
    let mut blocks = 0;
    for &def in &bodies {
        for (block, _) in Body::of(&db, def).blocks(&db) {
            assert_eq!(block.owner(&db), Some(def), "{block:?}");
            blocks += 1;
        }
    }
    assert_eq!(blocks, 3, "a block for each fn body and the inner block of `first`");

    db.set_file_text(file.file_id(&db), &BLOCKS.replace("x + 2", "x + 3"));
    let log = db.executed_in_read_order(|| {
        for &def in &bodies {
            Body::of(&db, def).blocks(&db).count();
        }
    });
    assert_eq!(count(&log, "Body::of"), 1, "{log:#?}");
    assert_eq!(count(&log, "block_item_tree_query"), 1, "only `second`'s block: {log:#?}");
}

/// A block a macro call in a body expands to has the body as its owner, and its item tree is
/// the expansion's: its id is of the expansion (there the root's), not of the body's text. The
/// fn begins the file, so the body's anchor is the fn and not the file's root.
#[test]
fn a_block_a_macro_expands_to_is_lowered_from_the_expansion() {
    let fx = fixture::db(
        "fn first() -> u32 { macro_rules! m { () => { { struct C; 1 } } } { struct D; m!() } }\n",
    );
    let db = fx.into_db();
    let (bodies, _) = defs(&db);
    let [def] = bodies[..] else { panic!("one body: {bodies:?}") };
    let mut in_file = 0;
    let mut in_macro = 0;
    for (block, def_map) in Body::of(&db, def).blocks(&db) {
        assert_eq!(block.owner(&db), Some(def), "{block:?}");
        assert_eq!(def_map.modules(&db).count(), 1, "{block:?}");
        match block.ast_id(&db).file_id.is_macro() {
            true => in_macro += 1,
            false => in_file += 1,
        }
    }
    assert_eq!((in_file, in_macro), (2, 1), "the fn's body, `{{ struct D; .. }}`, the expansion");
}

/// A block outside fn bodies (an associated const's, in an `impl` that begins the file) has an
/// id of the whole file's: its item tree is lowered whichever way its body was.
#[test]
fn an_associated_consts_block_has_its_item_tree() {
    let fx = fixture::db("impl S {\n    const C: u32 = { struct T; 1 };\n}\nstruct S;\n");
    let db = fx.into_db();
    let mut blocks = 0;
    for (_, module) in crate_def_map(&db, db.fetch_test_crate()).modules(&db) {
        for imp in module.scope(&db).impls() {
            for &(_, item) in ImplItems::of(&db, imp).0.items.iter() {
                let AssocItemId::ConstId(it) = item else { continue };
                for (block, def_map) in Body::of(&db, it.into()).blocks(&db) {
                    assert_eq!(def_map.modules(&db).count(), 1, "{block:?}");
                    blocks += 1;
                }
            }
        }
    }
    assert_eq!(blocks, 1, "`C`'s block");
}

/// (c) A body lowered again on this thread, on the scratch the lowering of other bodies left,
/// is the store it was lowered as the first time.
#[test]
fn a_body_lowered_again_on_a_reused_scratch_is_the_same() {
    let fx = fixture::db(CODE);
    let file = fx.only_file();
    let mut db = fx.into_db();
    let add = function_named(&db, "add");
    let first = function_named(&db, "first");
    let before = relative_body(&db, add);

    // `add` and then `first`, both bigger, so the scratch holds more than `add` needs.
    let bigger = CODE
        .replace("c * 2", "{ let d = [c; 4]; d[0] * 2 }")
        .replace("let y = x + 1;", "let y = (x + 1, x * 3, [x; 2]).0;");
    db.set_file_text(file.file_id(&db), &bigger);
    assert_ne!(*before, *relative_body(&db, add), "the edit changed nothing of `add`");
    relative_body(&db, first);

    db.set_file_text(file.file_id(&db), CODE);
    let log = db.executed_in_read_order(|| {
        relative_body(&db, add);
    });
    assert_eq!(count(&log, "Body::relative"), 1, "`add` was not lowered again: {log:#?}");
    let again = relative_body(&db, add);
    assert!(!Arc::ptr_eq(&before, &again), "`add` was not lowered again");
    assert_eq!(*before, *again);
    assert_eq!(blaze_format::to_bytes(&*before), blaze_format::to_bytes(&*again));
}

/// An item's visibility is read off its file's outline, which an edit of a body leaves the
/// same: growing a body that comes before them resolves no visibility again, a `pub(in path)`
/// one (whose path needs spans) included, nor the trait's that its items take.
#[test]
fn an_edit_in_a_body_resolves_no_visibility_again() {
    const VISIBILITIES: &str = r#"
pub fn first(x: u32) -> u32 { x + 1 }

pub(crate) struct S;

impl S {
    pub(in crate) fn get(&self) -> u32 { 1 }
    const C: u32 = 0;
}

pub trait Tr { fn m(&self); }
"#;
    let fx = fixture::db(VISIBILITIES);
    let file = fx.only_file();
    let mut db = fx.into_db();
    let items = |db: &TestDB| {
        let mut items = Vec::new();
        for (_, module) in crate_def_map(db, db.fetch_test_crate()).modules(db) {
            let scope = module.scope(db);
            for imp in scope.impls() {
                items.extend(ImplItems::of(db, imp).0.items.iter().map(|&(_, it)| it));
            }
            for decl in scope.declarations() {
                if let ModuleDefId::TraitId(it) = decl {
                    items.extend(TraitItems::query(db, it).items.iter().map(|&(_, it)| it));
                }
            }
        }
        items
    };
    let assoc = items(&db);
    assert_eq!(assoc.len(), 3, "`get`, `C` and `m`");
    let before: Vec<_> = assoc.iter().map(|it| it.assoc_visibility(&db)).collect();
    let outline = |db: &TestDB| crate::signatures::file_outline(db, file).text();
    let outline_before = outline(&db);

    let file_id = file.file_id(&db);
    let edited = db.file_text(file_id).text(&db).as_str().replace("x + 1", "x + 1 + 2 + 3");
    db.set_file_text(file_id, &edited);
    assert_eq!(outline_before, outline(&db), "the outline's text, every body left out");
    let mut after = Vec::new();
    let log = db.executed_in_read_order(|| after = assoc.iter().map(|it| it.assoc_visibility(&db)).collect());
    assert_eq!(before, after);
    assert_eq!(count(&log, "AssocItemId::assoc_visibility"), 0, "{log:#?}");
}

/// A fn of an inline module is its own anchor, so it is in its file's outline with its body
/// left out, an item in that body included: what reads its signature, attributes or
/// visibility reads them off the outline, not the file.
#[test]
fn a_fn_of_an_inline_module_with_an_item_in_its_body_is_in_the_outline() {
    let fx = fixture::db(
        r#"
pub fn first() -> u32 { 1 }

mod inner {
    pub fn helper(n: u32) -> u32 {
        fn nested(m: u32) -> u32 { m + 1 }
        { let k = nested(n); k * 2 }
    }
}
"#,
    );
    let db = fx.into_db();
    let DefWithBodyId::FunctionId(helper) = function_named(&db, "helper") else { panic!("`helper` is a fn") };
    let id = crate::Lookup::lookup(&helper, &db).id;
    assert!(crate::signatures::outline_item(&db, id).is_some(), "`helper` is not in the outline");
}
