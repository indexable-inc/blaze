//! The one pass of [`Scan::new`] against the scans it replaced, each a whole-store walk of its
//! own as the checks ran them per body: every table the same, in the same order, for every
//! body of a fixture that gives each of them entries.

pub(crate) mod db;

use base_db::all_crates;
use blaze_hash::{HashMap, HashSet};
use hir_def::{
    DefWithBodyId, HasModule as _, ModuleDefId,
    expr_store::ExpressionStore,
    hir::{BindingId, Expr, ExprId, Pat, PatId, Statement},
    nameres::crate_def_map,
};
use hir_expand::fixture;
use hir_ty::{
    InferBodyId, InferenceResult,
    next_solver::{DbInterner, Region, RegionExt as _, Ty},
};

use super::{Scan, irrefutable};
use crate::regions::{Annotation, spells_generics};

use self::db::TestDB;

fn matched(store: &ExpressionStore, infer: &InferenceResult<'_>) -> HashSet<ExprId> {
    let mut found = HashSet::default();
    for (expr, it) in store.exprs() {
        if let Expr::Match { expr: scrutinee, arms } = it
            && (arms.is_empty() || arms.iter().any(|arm| !irrefutable(store, infer, arm.pat)))
        {
            found.insert(expr);
            found.insert(scrutinee);
        }
    }
    found
}

fn tested_at(store: &ExpressionStore) -> HashMap<PatId, ExprId> {
    let mut found = HashMap::default();
    for (_, it) in store.exprs() {
        if let Expr::Match { expr: scrutinee, arms } = it {
            for arm in arms.iter() {
                store.walk_pats(arm.pat, &mut |pat| {
                    found.insert(pat, scrutinee);
                });
            }
        }
    }
    found
}

fn union_literals(store: &ExpressionStore) -> HashSet<ExprId> {
    let mut found = HashSet::default();
    for (expr, it) in store.exprs() {
        if let Expr::RecordLit { fields, .. } = it {
            found.insert(expr);
            found.extend(fields.iter().map(|field| field.expr));
        }
    }
    found
}

type Guarded = (
    HashMap<PatId, usize>,
    HashMap<PatId, (usize, Option<usize>)>,
    Vec<Vec<(usize, ExprId)>>,
);

/// The part of `guard_borrows` that read the store.
fn guarded(store: &ExpressionStore) -> Guarded {
    let mut tested_by = HashMap::default();
    let mut bound_by = HashMap::default();
    let mut matches = Vec::new();
    let mut arms_of = 0;
    for (_, expr) in store.exprs() {
        let Expr::Match { arms, .. } = expr else { continue };
        if arms.iter().all(|arm| arm.guard.is_none()) {
            continue;
        }
        let mut guarded = Vec::new();
        for arm in arms.iter() {
            store.walk_pats(arm.pat, &mut |pat| {
                tested_by.insert(pat, matches.len());
                if matches!(store.pat(pat), Pat::Bind { .. }) {
                    bound_by.insert(pat, (matches.len(), arm.guard.map(|_| arms_of)));
                }
            });
            if let Some(guard) = arm.guard {
                guarded.push((arms_of, guard));
                arms_of += 1;
            }
        }
        matches.push(guarded);
    }
    (tested_by, bound_by, matches)
}

fn scrutinees(store: &ExpressionStore) -> HashMap<BindingId, Option<ExprId>> {
    let mut map = HashMap::default();
    let mut add = |pat, scrutinee: ExprId, is_let: bool| {
        let simple = is_let && matches!(store.pat(pat), Pat::Bind { subpat: None, .. });
        store.walk_bindings_in_pat(pat, |binding| {
            map.insert(binding, if simple { None } else { Some(scrutinee) });
        });
    };
    for (_, expr) in store.exprs() {
        match expr {
            Expr::Match { expr, arms } => {
                for arm in arms.iter() {
                    add(arm.pat, expr, false);
                }
            }
            Expr::Let { pat, expr } => add(pat, expr, false),
            Expr::Block { statements, .. } => {
                for statement in statements.iter() {
                    if let Statement::Let { pat, initializer: Some(init), .. } = statement {
                        add(pat, init, true);
                    }
                }
            }
            _ => (),
        }
    }
    map
}

fn guard_bindings(store: &ExpressionStore) -> HashMap<ExprId, Vec<BindingId>> {
    let mut out: HashMap<ExprId, Vec<BindingId>> = HashMap::default();
    for (_, expr) in store.exprs() {
        let Expr::Match { arms, .. } = expr else { continue };
        for arm in arms.iter() {
            let Some(guard) = arm.guard else { continue };
            let mut bindings = Vec::new();
            store.walk_bindings_in_pat(arm.pat, |it| bindings.push(it));
            if bindings.is_empty() {
                continue;
            }
            let mut stack = vec![guard];
            while let Some(expr) = stack.pop() {
                out.entry(expr).or_default().extend(bindings.iter().copied());
                store.walk_child_exprs(expr, |it| stack.push(it));
            }
        }
    }
    out
}

fn statement_values<'db>(
    store: &ExpressionStore,
    infer: &InferenceResult<'db>,
    interner: DbInterner<'db>,
) -> HashMap<ExprId, Option<Ty<'db>>> {
    let erased = |ty: Ty<'db>| {
        rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_erased(interner))
    };
    let mut found = HashMap::default();
    for (_, expr) in store.exprs() {
        let Expr::Block { statements, .. } = expr else { continue };
        for statement in statements.iter() {
            let Statement::Expr { expr, .. } = statement else { continue };
            match store.expr(expr) {
                Expr::Match { expr: matched, .. } => {
                    let value = erased(infer.expr_ty(expr));
                    if value != erased(infer.expr_ty(matched)) {
                        found.insert(expr, Some(value));
                    }
                }
                _ => {
                    found.insert(expr, None);
                }
            }
        }
    }
    found
}

fn lets(store: &ExpressionStore) -> Vec<Annotation> {
    let mut found = Vec::new();
    for (block, expr) in store.exprs() {
        let Expr::Block { statements, .. } = expr else { continue };
        for statement in statements.iter() {
            if let Statement::Let { pat, type_ref: Some(type_ref), initializer, .. } = statement {
                found.push(Annotation {
                    pat,
                    type_ref,
                    initializer,
                    block,
                });
            }
        }
    }
    found
}

fn spelled_patterns(store: &ExpressionStore) -> Vec<(PatId, Option<ExprId>)> {
    let spelled = |pat: Pat<'_>| match pat {
        Pat::Record { path, .. } | Pat::TupleStruct { path, .. } | Pat::Path(path) => {
            spells_generics(path)
        }
        _ => false,
    };
    let mut found = Vec::new();
    for (_, expr) in store.exprs() {
        match expr {
            Expr::Block { statements, .. } => {
                for statement in statements.iter() {
                    if let Statement::Let { pat, type_ref: None, initializer, .. } = statement
                        && spelled(store.pat(pat))
                    {
                        found.push((pat, initializer));
                    }
                }
            }
            Expr::Match { expr, arms } => {
                for arm in arms.iter() {
                    if spelled(store.pat(arm.pat)) {
                        found.push((arm.pat, Some(expr)));
                    }
                }
            }
            _ => (),
        }
    }
    found
}

/// The paths `path_arguments` reads, with the closure each is the closure's of, by the parent
/// map it built.
fn spelled_paths(store: &ExpressionStore) -> Vec<(ExprId, Option<ExprId>)> {
    let mut parent = HashMap::default();
    for expr in store.expr_ids() {
        store.walk_child_exprs(expr, |child| {
            parent.insert(child, expr);
        });
    }
    let owner = |mut expr| loop {
        match parent.get(&expr) {
            Some(&up) if matches!(store.expr(up), Expr::Closure { .. }) => return Some(up),
            Some(&up) => expr = up,
            None => return None,
        }
    };
    store
        .exprs()
        .filter_map(|(expr, it)| match it {
            Expr::Path(path) if spells_generics(path) => Some((expr, owner(expr))),
            _ => None,
        })
        .collect()
}

/// How many entries each table had over the fixture's bodies, so none is compared empty only.
#[derive(Default, Debug)]
struct Seen {
    matched: usize,
    tested_at: usize,
    union_literals: usize,
    guards: usize,
    bound_by: usize,
    scrutinees: usize,
    simple_lets: usize,
    guard_bindings: usize,
    statement_values: usize,
    typed_statement_values: usize,
    lets: usize,
    spelled_patterns: usize,
    spelled_paths: usize,
    in_closures: usize,
}

fn same<K: Eq + std::hash::Hash, V: PartialEq, W>(
    got: &crate::mem::FMap<'_, K, V>,
    want: &HashMap<K, W>,
    eq: impl Fn(&V, &W) -> bool,
) -> bool {
    got.len() == want.len() && want.iter().all(|(k, w)| got.get(k).is_some_and(|v| eq(v, w)))
}

fn check(db: &TestDB, def: DefWithBodyId, seen: &mut Seen) {
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| check_in(frame, db, def, seen));
}

fn check_in(frame: &crate::mem::Frame, db: &TestDB, def: DefWithBodyId, seen: &mut Seen) {
    let owner = InferBodyId::DefWithBodyId(def);
    let infer = InferenceResult::of(db, owner);
    let store = ExpressionStore::of(db, owner.expression_store_owner(db));
    let interner = DbInterner::new_with(db, owner.krate(db));
    let scan = Scan::new(frame, store, infer, interner);

    let want = matched(store, infer);
    let got: HashSet<ExprId> = store.expr_ids().filter(|&it| scan.is_matched(it)).collect();
    assert_eq!(got, want, "{def:?}: matched");
    seen.matched += want.len();

    let want = tested_at(store);
    let got: HashMap<PatId, ExprId> =
        store.pat_ids().filter_map(|pat| Some((pat, scan.tested_at(pat)?))).collect();
    assert_eq!(got, want, "{def:?}: tested_at");
    seen.tested_at += want.len();

    let want = union_literals(store);
    let got: HashSet<ExprId> =
        store.expr_ids().filter(|&it| scan.is_union_literal(it)).collect();
    assert_eq!(got, want, "{def:?}: union_literals");
    seen.union_literals += want.len();

    let (tested_by, bound_by, matches) = guarded(store);
    assert_eq!(scan.guarded.matches, matches.len(), "{def:?}: guarded matches");
    assert!(same(&scan.guarded.tested_by, &tested_by, PartialEq::eq), "{def:?}: tested_by");
    assert!(same(&scan.guarded.bound_by, &bound_by, PartialEq::eq), "{def:?}: bound_by");
    let want: Vec<(usize, usize, ExprId)> = matches
        .iter()
        .enumerate()
        .flat_map(|(index, arms)| arms.iter().map(move |&(arm, guard)| (index, arm, guard)))
        .collect();
    let got: Vec<(usize, usize, ExprId)> = scan
        .guarded
        .guards
        .iter()
        .enumerate()
        .map(|(arm, &(index, guard))| (index, arm, guard))
        .collect();
    assert_eq!(got, want, "{def:?}: guards");
    seen.guards += want.len();
    seen.bound_by += bound_by.len();

    let want = scrutinees(store);
    assert!(same(&scan.scrutinees, &want, PartialEq::eq), "{def:?}: scrutinees");
    for (&binding, &scrutinee) in &want {
        assert_eq!(scan.scrutinee_of(binding), Some(scrutinee), "{def:?}: {binding:?}");
    }
    seen.scrutinees += want.len();
    seen.simple_lets += want.values().filter(|it| it.is_none()).count();

    let want = guard_bindings(store);
    assert!(
        same(&scan.guard_bindings, &want, |got, want: &Vec<BindingId>| {
            got.as_slice() == want.as_slice()
        }),
        "{def:?}: guard_bindings"
    );
    for (&expr, bindings) in &want {
        assert_eq!(scan.guard_bindings(expr), bindings.as_slice(), "{def:?}: {expr:?}");
    }
    seen.guard_bindings += want.len();

    let want = statement_values(store, infer, interner);
    assert!(same(&scan.statement_values, &want, PartialEq::eq), "{def:?}: statement_values");
    seen.statement_values += want.len();
    seen.typed_statement_values += want.values().filter(|it| it.is_some()).count();

    let want = lets(store);
    assert_eq!(scan.lets, want, "{def:?}: lets");
    seen.lets += want.len();

    let want = spelled_patterns(store);
    assert_eq!(scan.spelled_patterns, want, "{def:?}: spelled_patterns");
    seen.spelled_patterns += want.len();

    let want = spelled_paths(store);
    assert_eq!(scan.spelled_paths, want, "{def:?}: spelled_paths");
    seen.spelled_paths += want.len();
    seen.in_closures += want.iter().filter(|(_, closure)| closure.is_some()).count();
}

const FIXTURE: &str = r#"
struct Text;
enum E { A(Text), B(u32), C }
struct S { x: u32 }
union U { a: u32, b: f32 }
struct W<T>(T);
struct R<T> { value: T }
fn new<T>() -> W<T> { loop {} }
fn record<T>() -> R<T> { loop {} }

fn guards(e: E, flag: bool) -> u32 {
    match e {
        E::A(text) if flag => 1,
        E::B(n) if n > 2 => n,
        E::B(_) => 0,
        _ => 3,
    }
}

fn nested_guards(e: E, f: E) -> u32 {
    match e {
        E::B(n) if match f { E::B(m) if m == n => true, _ => false } => n,
        _ => 0,
    }
}

fn bindings(e: E, f: E) {
    let x = 1u32;
    let (a, b) = (2u32, 3u32);
    if let E::B(n) = e {}
    let E::B(m) = f else { return };
}

fn spelled() {
    let w: W<u32> = new();
    let W::<u32>(v) = new::<u32>();
    let R::<u8> { value } = record::<u8>();
    match new::<u16>() {
        W::<u16>(z) => {}
    }
}

fn statements(e: E) -> u32 {
    match e { E::A(_) => {}, _ => {} };
    match e { E::B(n) => n, _ => 0 };
    1u32;
    2
}

fn literals() -> u32 {
    let u = U { a: 1 };
    let s = S { x: 2 };
    s.x
}

fn closures(e: E) {
    let f = |g: E| match g {
        E::B(n) if n == 0 => new::<u64>(),
        _ => new::<u64>(),
    };
    let h = || {
        let k: W<i8> = new::<i8>();
        let inner = || new::<i16>();
    };
}

fn irrefutable_match(s: S) -> u32 {
    match s { S { x } => x }
}

fn empty_match(e: E) {
    match e {}
}
"#;

#[test]
fn the_one_pass_gives_what_each_scan_gave() {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let mut seen = Seen::default();
        let mut bodies = 0;
        for &krate in all_crates(&db).iter() {
            let def_map = crate_def_map(&db, krate);
            for (_, module) in def_map.modules(&db) {
                for def in module.scope(&db).declarations() {
                    if let ModuleDefId::FunctionId(it) = def {
                        check(&db, it.into(), &mut seen);
                        bodies += 1;
                    }
                }
            }
        }
        assert_eq!(bodies, 11, "the fixture's functions");
        let Seen {
            matched,
            tested_at,
            union_literals,
            guards,
            bound_by,
            scrutinees,
            simple_lets,
            guard_bindings,
            statement_values,
            typed_statement_values,
            lets,
            spelled_patterns,
            spelled_paths,
            in_closures,
        } = seen;
        for (name, count) in [
            ("matched", matched),
            ("tested_at", tested_at),
            ("union_literals", union_literals),
            ("guards", guards),
            ("bound_by", bound_by),
            ("scrutinees", scrutinees),
            ("simple_lets", simple_lets),
            ("guard_bindings", guard_bindings),
            ("statement_values", statement_values),
            ("typed_statement_values", typed_statement_values),
            ("lets", lets),
            ("spelled_patterns", spelled_patterns),
            ("spelled_paths", spelled_paths),
            ("in_closures", in_closures),
        ] {
            assert!(count > 0, "the fixture gives `{name}` no entry");
        }
    });
}
