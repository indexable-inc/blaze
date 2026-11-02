//! The pruned walks against the walks they replace: `walk_with` and `required` against `walk`
//! and every type's requirements, `wf_into` against a walk of every type that instantiates
//! every predicate.

pub(crate) mod db;

use hir_ty::db::HirDatabase;
use base_db::{Db, all_crates};
use hir_def::{AdtId, ModuleDefId, nameres::crate_def_map};
use hir_expand::fixture;

use super::*;
use crate::{Site, wfcheck};

use self::db::TestDB;

const FIXTURE: &str = r#"
//- minicore: sized, option
struct Wrap<'a, T: 'a>(&'a T);
struct Plain { a: u8, b: Option<u32>, c: &'static str, d: [u16; 4], e: (u8, bool) }
struct Mixed<'a, 'b, T, U: 'b> {
    a: &'a T,
    b: Option<Wrap<'b, U>>,
    c: Option<u8>,
    d: fn(&'a u8) -> &'b U,
    e: for<'x> fn(&'x T),
    f: (Wrap<'static, u8>, &'a &'b ()),
    g: *const [Wrap<'a, T>],
}
enum Either<'a, T> { A(&'a T), B(Option<Wrap<'a, T>>), C(u8) }
trait Tr<'a> { type Out; }
struct Proj<'a, T: Tr<'a>> { x: <T as Tr<'a>>::Out, y: &'a T }
struct Bounded<'a, T> where T: 'a { x: Option<&'a T>, y: Wrap<'a, Wrap<'a, T>> }
struct Static<T> { x: &'static T }
"#;

fn adts(db: &TestDB) -> Vec<AdtId> {
    let mut adts = Vec::new();
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            adts.extend(module.scope(db).declarations().filter_map(|def| match def {
                ModuleDefId::AdtId(it) => Some(it),
                _ => None,
            }));
        }
    }
    adts
}

/// Every field type of every ADT of the fixture's crates.
fn field_tys<'db>(db: &'db TestDB) -> Vec<(AdtId, Ty<'db>)> {
    let mut tys = Vec::new();
    for adt in adts(db) {
        for variant in adt.variants(db) {
            for (_, field) in db.field_types(variant).iter() {
                tys.push((adt, field.ty().instantiate_identity().skip_norm_wip()));
            }
        }
    }
    tys
}

/// `required` as it was: every predicate instantiated, whatever `ty` has in it.
fn required_reference<'db>(
    db: &'db Db,
    ty: Ty<'db>,
    found: &mut Vec<OutlivesClause<DbInterner<'db>, GenericArg<'db>>>,
) {
    let interner = DbInterner::new_no_crate(db);
    let mut needs = Vec::new();
    match ty.kind() {
        TyKind::Ref(region, inner, _) => needs.push(Needs::Type(inner, region)),
        TyKind::Adt(def, args) => {
            let clauses = GenericPredicates::query_all(db, def.def_id().into())
                .iter_instantiated(interner, args.as_slice())
                .map(|it| it.skip_norm_wip());
            clause_needs(clauses, &mut needs);
            for predicate in Inferred::default().of(db, def.def_id()) {
                let OutlivesClause(arg, region) = EarlyBinder::bind(interner, predicate)
                    .instantiate(interner, args)
                    .skip_norm_wip();
                match arg.kind() {
                    GenericArgKind::Type(ty) => needs.push(Needs::Type(ty, region)),
                    GenericArgKind::Lifetime(longer) => needs.push(Needs::Region(longer, region)),
                    GenericArgKind::Const(_) => (),
                }
            }
        }
        TyKind::Alias(_, alias)
            if let rustc_type_ir::AliasTyKind::Projection { def_id: TraitAssocTyId(item) } =
                alias.kind
                && let ItemContainerId::TraitId(trait_) = item.lookup(db).container =>
        {
            let clauses = GenericPredicates::query_all(db, trait_.into())
                .iter_instantiated(interner, alias.args.as_slice())
                .map(|it| it.skip_norm_wip());
            clause_needs(clauses, &mut needs);
        }
        _ => (),
    }
    for need in needs {
        match need {
            Needs::Type(ty, region) if is_free(region) => {
                for component in components(ty) {
                    let arg: GenericArg<'db> = match component {
                        Component::Region(longer) if is_free(longer) => longer.into(),
                        Component::Param(ty) | Component::Alias(ty) => ty.into(),
                        Component::Region(_) => continue,
                    };
                    found.push(OutlivesClause(arg, region));
                }
            }
            Needs::Region(longer, shorter) if is_free(longer) && is_free(shorter) => {
                found.push(OutlivesClause(longer.into(), shorter));
            }
            _ => (),
        }
    }
}

/// `wf_into` as it was: every type entered, every predicate instantiated.
fn wf_reference<'db>(
    db: &'db Db,
    inferred: &mut Inferred<'db>,
    ty: Ty<'db>,
    out: &mut Vec<Needs<'db>>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    let args = |inferred: &mut Inferred<'db>, args: GenericArgs<'db>, out: &mut Vec<Needs<'db>>| {
        for arg in args.iter() {
            if let GenericArgKind::Type(ty) = arg.kind() {
                wf_reference(db, inferred, ty, out, depth + 1);
            }
        }
    };
    match ty.kind() {
        TyKind::Ref(region, inner, _) => {
            out.push(Needs::Type(inner, region));
            wf_reference(db, inferred, inner, out, depth + 1);
        }
        TyKind::Adt(def, adt_args) => {
            let interner = DbInterner::new_no_crate(db);
            let clauses = GenericPredicates::query_all(db, def.def_id().into())
                .iter_instantiated(interner, adt_args.as_slice())
                .map(|it| it.skip_norm_wip());
            clause_needs(clauses, out);
            for predicate in inferred.of(db, def.def_id()) {
                let OutlivesClause(arg, region) = EarlyBinder::bind(interner, predicate)
                    .instantiate(interner, adt_args)
                    .skip_norm_wip();
                out.push(match arg.kind() {
                    GenericArgKind::Type(ty) => Needs::Type(ty, region),
                    GenericArgKind::Lifetime(longer) => Needs::Region(longer, region),
                    GenericArgKind::Const(_) => continue,
                });
            }
            args(inferred, adt_args, out);
        }
        TyKind::FnPtr(sig, _) => {
            for it in sig.skip_binder().inputs_and_output.iter() {
                wf_reference(db, inferred, it, out, depth + 1);
            }
        }
        TyKind::Dynamic(predicates, _) => {
            for predicate in predicates.iter() {
                match predicate.skip_binder() {
                    ExistentialPredicate::Trait(it) => args(inferred, it.args, out),
                    ExistentialPredicate::Projection(it) => {
                        args(inferred, it.args, out);
                        if let Some(term) = it.term.as_type() {
                            wf_reference(db, inferred, term, out, depth + 1);
                        }
                    }
                    ExistentialPredicate::AutoTrait(_) => (),
                }
            }
        }
        TyKind::Tuple(tys) => {
            for it in tys.iter() {
                wf_reference(db, inferred, it, out, depth + 1);
            }
        }
        TyKind::Array(inner, _) | TyKind::Slice(inner) | TyKind::RawPtr(inner, _) => {
            wf_reference(db, inferred, inner, out, depth + 1)
        }
        TyKind::Alias(_, alias) => args(inferred, alias.args, out),
        _ => (),
    }
}

/// A requirement that holds and tells an item nothing: of a type with nothing `MAY_NEED`
/// names, of regions that are `'static`, erased or bound.
fn is_trivial(need: Needs<'_>) -> bool {
    let fixed = |region: Region<'_>| {
        matches!(
            region.kind(),
            RegionKind::ReStatic | RegionKind::ReErased | RegionKind::ReBound(..)
        )
    };
    match need {
        Needs::Type(ty, region) => !ty.has_type_flags(MAY_NEED) && fixed(region),
        Needs::Region(longer, shorter) => fixed(longer) && fixed(shorter),
    }
}

#[test]
fn the_pruned_walk_is_the_walk_of_types_with_the_flags() {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let tys = field_tys(&db);
        assert!(tys.len() > 15, "{}", tys.len());
        for (_, ty) in tys {
            let mut all = Vec::new();
            walk(ty, &mut all);
            all.retain(|it| it.has_type_flags(TypeFlags::HAS_RE_PARAM));
            let mut pruned = Vec::new();
            walk_with(ty, TypeFlags::HAS_RE_PARAM, &mut pruned);
            assert_eq!(pruned, all, "{ty:?}");
        }
    });
}

#[test]
fn inferred_outlives_finds_what_every_type_s_requirements_find() {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let mut found_any = false;
        for (_, ty) in field_tys(&db) {
            let mut all = Vec::new();
            walk(ty, &mut all);
            let mut expected = Vec::new();
            for &it in &all {
                required_reference(&db, it, &mut expected);
            }
            let mut pruned = Vec::new();
            walk_with(ty, TypeFlags::HAS_RE_PARAM, &mut pruned);
            let mut actual = Vec::new();
            for &it in &pruned {
                required(&db, it, &mut actual);
            }
            found_any |= !actual.is_empty();
            assert_eq!(actual, expected, "{ty:?}");
        }
        assert!(found_any);
    });
}

#[test]
fn well_formedness_leaves_out_only_what_holds_and_tells_nothing() {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let mut inferred = Inferred::default();
        let mut left_out = 0;
        for (_, ty) in field_tys(&db) {
            let mut expected = Vec::new();
            wf_reference(&db, &mut inferred, ty, &mut expected, 0);
            let mut actual = Vec::new();
            wf_into(&db, &mut inferred, ty, &mut actual, 0);
            // `actual` is `expected` with some left out, in the same order.
            let mut rest = expected.iter().copied();
            for &need in &actual {
                loop {
                    let next = rest.next().unwrap_or_else(|| {
                        panic!("{ty:?}: {need:?} is not in {expected:?} in order")
                    });
                    if next == need {
                        break;
                    }
                    assert!(is_trivial(next), "{ty:?}: left out {next:?}");
                    left_out += 1;
                }
            }
            for next in rest {
                assert!(is_trivial(next), "{ty:?}: left out {next:?}");
                left_out += 1;
            }
        }
        assert!(left_out > 0);
    });
}

const TRAITS: &str = r#"
//- minicore: sized, option
trait Tr { type Out; }
impl Tr for u8 { type Out = u16; }
impl<'a, T: 'a> Tr for &'a T { type Out = T; }
trait Marker {}
impl<T: 'static> Marker for T {}
struct NeedsTr<T: Tr>(T);
struct NeedsMarker<T: Marker>(T);
struct Uses<'a, T: 'a + Tr> {
    a: NeedsTr<u8>,
    b: NeedsMarker<u32>,
    c: NeedsTr<&'a T>,
    d: <u8 as Tr>::Out,
    e: NeedsMarker<&'static str>,
    f: NeedsTr<T>,
    g: Option<NeedsMarker<Option<u8>>>,
}
"#;

/// Global predicates answered by evaluation alone leave out only needs that hold and tell
/// nothing, in order, and fail where proving them in a context fails.
#[test]
fn global_predicates_leave_out_only_what_holds() {
    let fx = fixture::db(TRAITS);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let mut globals = 0;
        for (adt, ty) in field_tys(&db) {
            blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
                let mut predicates = Vec::new_in(frame);
                trait_predicates(&db, frame, ty, &mut predicates);
                globals += predicates.iter().filter(|it| it.is_global()).count();
                let mut expected = Vec::new();
                prove_needs_reference(&Wfcx::new(frame, &db, adt.into()), &predicates, &mut expected);
                let mut actual = Vec::new();
                prove_needs(&Wfcx::new(frame, &db, adt.into()), &predicates, &mut actual);
                let mut rest = expected.iter().copied();
                for &need in &actual {
                    loop {
                        let next = rest.next().unwrap_or_else(|| {
                            panic!("{ty:?}: {need:?} is not in {expected:?} in order")
                        });
                        if next == need {
                            break;
                        }
                        assert!(is_trivial(next), "{ty:?}: left out {next:?}");
                    }
                }
                for next in rest {
                    assert!(is_trivial(next), "{ty:?}: left out {next:?}");
                }
            });
        }
        assert!(globals >= 4, "{globals}");
    });
}

#[test]
fn a_field_of_static_reference_to_a_parameter_needs_it_static() {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let named = |name: &str| {
            adts(&db)
                .into_iter()
                .find(|&adt| {
                    let name_of = match adt {
                        AdtId::StructId(it) => {
                            hir_def::signatures::StructSignature::of(&db, it).name.clone()
                        }
                        AdtId::EnumId(it) => {
                            hir_def::signatures::EnumSignature::of(&db, it).name.clone()
                        }
                        AdtId::UnionId(it) => {
                            hir_def::signatures::UnionSignature::of(&db, it).name.clone()
                        }
                    };
                    name_of.as_str() == name
                })
                .unwrap_or_else(|| panic!("no ADT {name}"))
        };
        let found = &wfcheck(&db, named("Static").into()).diagnostics;
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].code, "E0310");
        assert!(matches!(found[0].site, Site::Field(..)), "{found:?}");
        for name in ["Plain", "Wrap", "Bounded", "Either"] {
            let found = &wfcheck(&db, named(name).into()).diagnostics;
            assert!(found.is_empty(), "{name}: {found:?}");
        }
    });
}
