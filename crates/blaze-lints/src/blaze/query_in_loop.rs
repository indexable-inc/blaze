//! `query_in_loop`: a query called once per element of a collection, when a query beside it
//! takes the batch. Each call records a dependency edge of the caller's query and checks its
//! memo; one call of the batch query records one edge, and is one memo to keep valid.
//!
//! A query is [`lock_across_query::query_of`]'s: a function an attribute macro of
//! `blaze_query_macros` made, or a method of a trait implemented for `base_db::Db`. Per element
//! is [`per_item`]'s (the body of a `for` loop, or a closure given to an iterator's method), and
//! one of the call's arguments reads a local holding the element, so a call whose key is the
//! same every time (a loop-invariant query) is not this lint's.
//!
//! Flagged only when a batch can be named, so every finding has a fix to make: a query of the
//! same kind (made by one of `blaze_query_macros`' macros in the same module, or a method of the
//! same database trait) whose return type holds the per-item query's return type as its
//! element: a slice or an array of it, a collection that is `IntoIterator` (`Vec`, a map, a
//! `SmallVec`, not `Option` or `Result`) whose type arguments name it, alone or in a tuple
//! (`Vec<(Key, Value)>`), or a `Box`, `Arc` or `Rc` of one of those. Its keys differ from the
//! per-item query's and it takes at least one. Queries returning `()` or `bool` are not
//! matched; neither are generic ones, whose types cannot be compared across functions.

use base_db::Db;
use blaze_hash::HashMap;
use hir::AsAssocItem as _;
use hir_def::{DefWithBodyId, FunctionId, hir::Expr};
use std::alloc::Global;

use super::{
    Finding, Lint,
    alloc::written_callee,
    facts::{self, Parents},
    lock_across_query::{Query, query_macro, query_of},
    per_item::{element_bindings, mentions, per_item, written_call},
};
use crate::check::Cx;

const LINT: Lint = Lint::QueryInLoop;

/// How many wrappers (`Arc`, `Box`, a collection) deep a batch's return type is opened.
const WRAP_DEPTH: u32 = 3;

/// The pointers a batch's collection may sit behind, by crate.
const POINTERS: &[(&str, &[&str])] = &[("alloc", &["Box", "Arc", "Rc"]), ("triomphe", &["Arc"])];

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
    let body = analysis.body();
    let infer = analysis.infer();
    // alloc: each callee asked once per body
    let mut known: HashMap<FunctionId, Option<(Query, FunctionId)>, Global> =
        HashMap::new_in(Global);
    let mut parents = None;
    for (id, expr) in body.exprs() {
        let args = match expr {
            Expr::Call { args, .. } | Expr::MethodCall { args, .. } => &args[..],
            _ => continue,
        };
        if !written_call(analysis, id) {
            continue;
        }
        let Some((f, generic_args)) = written_callee(infer, body, id) else { continue };
        let entry = known.entry(f).or_insert_with(|| {
            let query = query_of(db, analysis, f, generic_args)?;
            let batch = batch_of(db, f, &query)?;
            Some((query, batch))
        });
        let Some((query, batch)) = entry.clone() else { continue };
        let parents = parents.get_or_insert_with(|| Parents::of(body));
        let Some(over) = per_item(cx, analysis, parents, id) else { continue };
        let bindings = element_bindings(body, over);
        if !args.iter().any(|&it| mentions(db, def, body, it, &bindings)) {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        let (one, all) = (name_of(db, f), name_of(db, batch));
        let message = format!(
            "the query `{one}` ({}) is called once per element of {}; `{all}` takes them all in one call",
            query.say(),
            over.say(db),
        );
        let note = format!(
            "every call of `{one}` records a dependency edge of the calling query and checks its memo, once per \
             element; one call of `{all}` records one edge"
        );
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}

/// The query beside `f` of the same kind that takes a batch of what `f` returns one of: the
/// first by name, so the finding does not depend on declaration order.
fn batch_of(db: &Db, f: FunctionId, query: &Query) -> Option<FunctionId> {
    let function = hir::Function::from(f);
    let one = item_type(db, function)?;
    let keys = key_types(db, function);
    // alloc: the queries beside one query, asked once per callee per body
    let mut candidates: Vec<hir::Function> = match query {
        Query::Macro(_) => function
            .module(db)
            .declarations(db)
            .into_iter()
            .filter_map(|it| match it {
                hir::ModuleDef::Function(it) => Some(it),
                _ => None,
            })
            .filter(|it| FunctionId::try_from(*it).is_ok_and(|id| query_macro(db, id).is_some()))
            .collect(),
        Query::Database(_) => {
            let trait_ = match function.as_assoc_item(db)?.container(db) {
                hir::AssocItemContainer::Trait(it) => it,
                hir::AssocItemContainer::Impl(it) => it.trait_(db)?,
            };
            trait_
                .items(db)
                .into_iter()
                .filter_map(|it| match it {
                    hir::AssocItem::Function(it) => Some(it),
                    _ => None,
                })
                .collect()
        }
    };
    candidates.sort_by(|a, b| a.name(db).as_str().cmp(b.name(db).as_str()));
    candidates.into_iter().filter(|&it| it != function).find_map(|it| {
        let id = FunctionId::try_from(it).ok()?;
        let theirs = key_types(db, it);
        let batch = !theirs.is_empty()
            && theirs != keys
            && it
                .ret_type(db)
                .strip_references()
                .without_params(db)
                .is_some_and(|ret| wraps(db, &ret, &one, WRAP_DEPTH));
        batch.then_some(id)
    })
}

/// What `f` returns, its references peeled and owned by no generics: none for `()`, `bool` and
/// a generic return.
fn item_type(db: &Db, f: hir::Function) -> Option<hir::Type<'_>> {
    let ty = f.ret_type(db).strip_references().without_params(db)?;
    (!ty.is_unit() && !ty.is_bool()).then_some(ty)
}

/// The types of `f`'s keys: its parameters but `self` and the database, each owned by no
/// generics (a generic parameter is kept as `None`, never equal to a concrete one).
fn key_types(db: &Db, f: hir::Function) -> Vec<Option<hir::Type<'_>>> {
    f.params_without_self(db)
        .iter()
        .map(|it| it.ty().strip_references())
        .filter(|it| !it.as_adt().is_some_and(|adt| facts::adt_is(db, adt, "base_db", "Db")))
        .map(|it| it.without_params(db))
        .collect() // alloc: a query's few key types, asked once per callee
}

/// Whether `ret` holds `one` as its element: a slice or array of it, an `IntoIterator`
/// collection (not `Option` or `Result`) with it among its type arguments, alone or in a
/// tuple, or one of [`POINTERS`] to such, at most `depth` wrappers deep.
fn wraps<'db>(db: &'db Db, ret: &hir::Type<'db>, one: &hir::Type<'db>, depth: u32) -> bool {
    let Some(depth) = depth.checked_sub(1) else { return false };
    let element = |it: &hir::Type<'db>| {
        let it = it.strip_references();
        it == *one
            || (it.is_tuple()
                && it.tuple_fields(db).iter().any(|field| field.strip_references() == *one))
    };
    if let Some(inner) = ret.as_slice() {
        return element(&inner) || wraps(db, &inner, one, depth);
    }
    if let Some((inner, _)) = ret.as_array(db) {
        return element(&inner) || wraps(db, &inner, one, depth);
    }
    let Some(adt) = ret.as_adt() else { return false };
    let krate = adt.module(db).krate(db);
    let pointer = POINTERS.iter().any(|(of, names)| {
        names.contains(&adt.name(db).as_str())
            && (facts::crate_is(db, krate, of) || facts::crate_is(db, krate, "std"))
    });
    if pointer {
        return ret.type_arguments().next().is_some_and(|inner| wraps(db, &inner, one, depth));
    }
    if facts::adt_is(db, adt, "core", "Option") || facts::adt_is(db, adt, "core", "Result") {
        return false;
    }
    ret.into_iterator_iter(db).is_some()
        && ret.type_arguments().any(|it| element(&it) || wraps(db, &it, one, depth))
}

/// `f` as a message names it: `Trait::name` for a database method, its name otherwise.
fn name_of(db: &Db, f: FunctionId) -> String {
    let function = hir::Function::from(f);
    let name = function.name(db);
    let owner = function.as_assoc_item(db).and_then(|it| match it.container(db) {
        hir::AssocItemContainer::Trait(it) => Some(it),
        hir::AssocItemContainer::Impl(it) => it.trait_(db),
    });
    match owner {
        Some(trait_) => format!("{}::{}", trait_.name(db).as_str(), name.as_str()),
        None => name.as_str().to_owned(), // alloc: a message's part
    }
}
