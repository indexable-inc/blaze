//! Allocation budgets of the expansion queries' memo hits: once a call is expanded, reading its
//! expansion, argument, span map and ast id map again allocates nothing.

use hir_expand::{HirFileId, MacroCallId, MacroCallKind};
use hir_expand::fixture;

use super::builtin_errors::macro_calls;
use crate::test_db::TestDB;

#[test]
fn expansion_memo_hits_allocate_nothing() {
    crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(false);
    let db = fixture::db(
        r#"
//- minicore: derive, clone
macro_rules! make { ($($t:tt)*) => { fn made() { $($t)* } } }
make!(let _x = 1;);
#[derive(Clone)]
struct S { a: u8, b: u16 }
"#,
    )
    .into_db();
    let calls = macro_calls(&db);
    assert!(
        calls.iter().any(|call| matches!(call.loc(&db).kind, MacroCallKind::FnLike { .. }))
            && calls.iter().any(|call| matches!(call.loc(&db).kind, MacroCallKind::Derive { .. })),
        "the `macro_rules!` call and the derive: {calls:?}"
    );
    let read = |call: MacroCallId| {
        let expansion = call.parse_macro_expansion(&db);
        let (arg, _, span) = call.macro_arg_considering_derives(&db, &call.loc(&db).kind);
        let file = HirFileId::from(call);
        let _ = file.span_map(&db);
        let _ = file.ast_id_map(&db);
        (std::ptr::from_ref(expansion), std::ptr::from_ref(arg), span)
    };
    let warm: Vec<_> = calls.iter().map(|&call| read(call)).collect();
    for (at, &call) in calls.iter().enumerate() {
        let hit = blaze_alloc_count::assert_at_most(0, "an expansion's memo hits", || read(call));
        assert!(hit == warm[at], "a hit gives the memo's own values");
    }
    crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(true);
}

/// Mapping into and out of an expansion through `ExpansionInfo`, as `hir::view::Semantics` does on
/// every IDE request: after one round (the downmap index built, the worker's `u32`s grown), a
/// second allocates nothing.
#[test]
fn warm_span_mapping_allocates_nothing() {
    let db = fixture::db(
        r#"
macro_rules! make { ($($t:tt)*) => { fn made() { $($t)* } } }
make!(let _x = 1 + 2;);
"#,
    )
    .into_db();
    let [call] = macro_calls(&db)[..] else { panic!("the `make!` call") };
    let round = || {
        let info = hir_expand::ExpansionInfo::new(&db, call);
        let range = info.expanded_view().value.text_range();
        let up = info.map_node_range_up(&db, range);
        let at = info.span_for_offset(&db, range.start());
        let (arg, _, _) = call.macro_arg_considering_derives(&db, &call.loc(&db).kind);
        let first = arg.token_trees().first_span().expect("an argument token");
        let down = info.map_range_down_view(first).map_or(0, |it| it.value.count());
        (up.map(|it| it.0), at.0, down)
    };
    let warm = round();
    let again = blaze_alloc_count::assert_at_most(0, "a warm span mapping round", round);
    assert_eq!(again, warm);
}

/// A new id for `call`: its location one expansion deeper, so its argument and expansion are
/// computed again (a miss) while the file's parse, span map, ast ids and the macro's definition
/// stay warm.
fn fresh(db: &TestDB, call: MacroCallId) -> MacroCallId {
    let loc = call.loc(db);
    // clone: a test's call kind, once per fresh call
    loc.def.make_call(db, loc.krate, loc.kind.clone(), loc.ctxt, loc.macro_depth + 1)
}

/// What the argument tree of `call` owns: one allocation per non-empty column, counted as a copy
/// of it costs.
fn arg_columns(db: &TestDB, call: MacroCallId) -> u64 {
    let (tree, _, _) = call.macro_arg_considering_derives(db, &call.loc(db).kind);
    blaze_alloc_count::measure(|| tree.copy()).1.allocations
}

/// A miss of `parse_macro_expansion` (with its argument's `macro_arg`) allocates the query
/// engine's run cost, the argument tree and the expansion's parse and span map, nothing else: the
/// expansion tree and the semicolon split are the run's frame's. The parse's cost is measured by
/// parsing the same tokens outside the database, warm; the engine's run cost by the miss of a call that
/// expands to nothing, less what that one owns.
#[test]
fn an_expansion_miss_allocates_its_result_alone() {
    let db = fixture::db(
        r#"
macro_rules! make { ($($t:tt)*) => { fn made() { $($t)* } } }
macro_rules! nothing { () => {} }
make!(let _x = 1 + 2; let _y = [3, 4, 5]; let _z = (6, 7););
nothing!();
"#,
    )
    .into_db();
    let calls = macro_calls(&db);
    let [make, nothing] = calls[..] else { panic!("two calls: {calls:?}") };
    let span = make.macro_arg_considering_derives(&db, &make.loc(&db).kind).2;
    let parse_cost = |tokens: &tt::TopSubtree| {
        let run = || {
            syntax_bridge::token_tree_to_syntax_node(
                tokens.view(),
                syntax_bridge::TopEntryPoint::MacroItems,
                &mut |_| span::Edition::CURRENT,
            )
        };
        let _warm = run();
        blaze_alloc_count::measure(run).1.allocations
    };
    let text = make.parse_macro_expansion(&db).value.0.root().text();
    let tokens = syntax_bridge::parse_to_token_tree_static_span(span::Edition::CURRENT, span, text)
        .expect("the expansion's tokens");
    let empty = tt::TopSubtree::empty(tt::DelimSpan::from_single(span));
    let (make, nothing) = (fresh(&db, make), fresh(&db, nothing));
    let (_, base) =
        blaze_alloc_count::measure(|| make.parse_macro_expansion(&db).value.1.iter().count());
    let (_, run) =
        blaze_alloc_count::measure(|| nothing.parse_macro_expansion(&db).value.1.iter().count());
    let engine = run.allocations.saturating_sub(parse_cost(&empty) + arg_columns(&db, nothing));
    let owned = parse_cost(&tokens) + arg_columns(&db, make);
    assert!(
        base.allocations <= engine + owned,
        "a miss: {} allocations; the engine's run {engine}, its argument, parse and map {owned}",
        base.allocations,
    );
}

/// A miss of `macro_arg` for a derive's attribute call is the argument tree's columns and the
/// query engine's run cost (the same query for a derive of a unit struct, less its tree): the cfg walk's
/// attribute list and inserted leaves are the frame's.
#[test]
fn a_derive_argument_miss_allocates_its_tree_alone() {
    crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(false);
    let db = fixture::db(
        r#"
//- minicore: derive, clone
#[derive(Clone)]
#[cfg_attr(all(), allow(unused))]
struct Wide { #[cfg(all())] a: u8, b: u16, #[cfg(any())] c: u32, d: (u8, u8) }
#[derive(Clone)]
struct Unit;
"#,
    )
    .into_db();
    let mut derives: Vec<_> = macro_calls(&db)
        .into_iter()
        .filter_map(|call| match call.loc(&db).kind {
            MacroCallKind::Derive { ast_id, derive_macro_id, .. } => {
                Some((ast_id.to_ptr(&db).text_range().start(), derive_macro_id))
            }
            _ => None,
        })
        .collect(); // alloc: a test's list of derive calls, outside the measured runs
    derives.sort_by_key(|&(at, _)| at);
    let [(_, wide), (_, unit)] = derives[..] else { panic!("two derives: {derives:?}") };
    let (wide, unit) = (fresh(&db, wide), fresh(&db, unit));
    // The thread's query stacks grow to their high-water marks once: a miss of each, unmeasured,
    // first (one call deeper, so the measured ones still miss).
    for warm in [fresh(&db, wide), fresh(&db, unit)] {
        let _ = warm.macro_arg_considering_derives(&db, &warm.loc(&db).kind);
    }
    let (_, run) = blaze_alloc_count::measure(|| {
        let _ = unit.macro_arg_considering_derives(&db, &unit.loc(&db).kind);
    });
    let engine = run.allocations.saturating_sub(arg_columns(&db, unit));
    let (_, miss) = blaze_alloc_count::measure(|| {
        let _ = wide.macro_arg_considering_derives(&db, &wide.loc(&db).kind);
    });
    let owned = arg_columns(&db, wide);
    crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(true);
    assert!(
        miss.allocations <= engine + owned,
        "`macro_arg` of a derive: {} allocations; the engine's run {engine}, its tree {owned}",
        miss.allocations,
    );
}
