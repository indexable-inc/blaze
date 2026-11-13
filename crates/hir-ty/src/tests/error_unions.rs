//! blaze's error unions (`#![blaze::error_unions]`): `A | B` is core's error union of its
//! members in one canonical order, so `A | B`, `B | A`, `A | A | B` and `A | (B | A)` are one
//! type; a member that is not a known named type is refused naming why, and a crate without
//! the attribute is refused. A member, or a union of some of the members, widens to a union:
//! by coercion where a union is expected, and by `?` through core's `From`; anything else is
//! refused. A union is `Debug`, `Display`, `Error`, `Clone` and `Hash` when its members are.
//! A constructor pattern of a member matches the union by member (`PatAdjust::UnionMember`),
//! through references and under `x @`, only in a crate with the attribute.

use base_db::{Crate, Db, all_crates};
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, ModuleDefId, VariantId,
    expr_store::Body,
    lang_item::lang_items,
    nameres::crate_def_map,
    signatures::{FunctionSignature, StructSignature, TraitSignature},
};
use hir_expand::fixture;
use rustc_type_ir::{TyKind, inherent::IntoKind as _};

use crate::{
    Adjust, ErrorUnionRefusal, InferBodyId, InferenceDiagnostic, InferenceResult,
    TyLoweringDiagnostic,
    infer::PatAdjust,
    db::HirDatabase,
    next_solver::{DbInterner, Ty},
    solver_errors::SolverDiagnosticKind,
};

const FIXTURE: &str = r#"
//- minicore: sized, error_union
//- /main.rs crate:main
#![blaze::error_unions]
struct A;
struct B;
struct V<T> {
    ab: A | B,
    ba: B | A,
    aab: A | A | B,
    nested: A | (B | A),
    one: A | A,
    param: A | T,
    imp: A | impl Sized,
    reference: A | &'static B,
}
//- /other.rs crate:other
struct A;
struct B;
struct V {
    ab: A | B,
}
"#;

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// The field types of `krate`'s struct `V`, in order, and the diagnostics of lowering them.
fn fields(krate: &str, check: impl FnOnce(&Db, &[Ty<'_>], &[TyLoweringDiagnostic])) {
    fields_of(FIXTURE, krate, check)
}

/// [`fields`] in `fixture`.
fn fields_of(
    fixture: &str,
    krate: &str,
    check: impl FnOnce(&Db, &[Ty<'_>], &[TyLoweringDiagnostic]),
) {
    let db = fixture::db(fixture).into_db();
    blaze_query::db::attach(&db, || {
        let krate = crate_named(&db, krate);
        let v = crate_def_map(&db, krate)
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::AdtId(AdtId::StructId(it))
                    if StructSignature::of(&db, it).name.as_str() == "V" =>
                {
                    Some(it)
                }
                _ => None,
            })
            .expect("the fixture's `V`");
        let lowered = db.field_types_with_diagnostics(VariantId::StructId(v));
        let tys = lowered
            .value
            .as_slice()
            .iter()
            .map(|it| it.ty().instantiate_identity().skip_norm_wip())
            .collect::<Vec<_>>();
        check(&db, &tys, lowered.diagnostics());
    })
}

fn refusals(diagnostics: &[TyLoweringDiagnostic]) -> Vec<ErrorUnionRefusal> {
    diagnostics
        .iter()
        .filter_map(|it| match it {
            TyLoweringDiagnostic::ErrorUnionRefused { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}

#[test]
fn a_union_is_one_type_whatever_the_order_and_repeats() {
    fields("main", |_, tys, _| {
        let [ab, ba, aab, nested, one, ..] = tys else { panic!("{tys:?}") };
        assert_eq!(ab, ba);
        assert_eq!(ab, aab);
        assert_eq!(ab, nested, "a member that is a union gives its members");
        match ab.kind() {
            TyKind::Adt(_, args) => assert!(
                args.types().any(|it| matches!(it.kind(), TyKind::Tuple(members) if members.len() == 2)),
                "{ab:?}",
            ),
            other => panic!("the error union struct, not {other:?}"),
        }
        assert!(matches!(one.kind(), TyKind::Adt(..)), "`A | A` is `A`: {one:?}");
        assert_ne!(ab, one);
    });
}

#[test]
fn a_member_that_is_not_a_known_named_type_is_refused_naming_why() {
    fields("main", |_, _, diagnostics| {
        assert_eq!(
            refusals(diagnostics),
            [ErrorUnionRefusal::TypeParam, ErrorUnionRefusal::NotNamed, ErrorUnionRefusal::NotNamed],
            "{diagnostics:?}",
        );
    });
}

/// `other` lacks the attribute, so its file parses as Rust's though it shares `main`'s source
/// root (a file is read under its own crate's extensions): its `A | B` is the syntax error it
/// is in Rust, and no union of it reaches hir-ty. `main`'s file, read under `main`'s, has none.
#[test]
fn a_crate_without_the_attribute_parses_no_union() {
    let db = fixture::db(FIXTURE).into_db();
    blaze_query::db::attach(&db, || {
        let (main, other) = (crate_named(&db, "main"), crate_named(&db, "other"));
        assert!(main.extensions(&db).error_unions());
        assert!(other.extensions(&db).is_none());
        let (main, other) = (main.root_file_id(&db), other.root_file_id(&db));
        assert!(main.extensions(&db).error_unions() && other.extensions(&db).is_none());
        let errors = main.parse(&db).errors();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!other.parse(&db).errors().is_empty(), "`A | B` is no type in `other`");
    })
}

/// Unions a macro carries: `main`, with the attribute, writes one in its macro's definition
/// and hands one to a macro; `plain`, without it, calls `main`'s macro.
const IN_MACROS: &str = r#"
//- minicore: sized, error_union
//- /main.rs crate:main
#![blaze::error_unions]
pub struct A;
pub struct B;
#[macro_export]
macro_rules! union_of { ($a:ty, $b:ty) => { $a | $b }; }
macro_rules! same { ($t:ty) => { $t }; }
struct V {
    written: union_of!(A, B),
    handed: same!(B | A),
    outside: A | B,
}
//- /plain.rs crate:plain deps:main
struct V {
    written: main::union_of!(main::A, main::B),
}
"#;

/// In a crate with the attribute, `A | B` written by the crate's macro or handed to one is
/// the union it is outside a macro: a token is parsed under the extensions of the crate that
/// wrote it, in a matcher's `$t:ty` and in the expansion.
#[test]
fn a_union_in_a_gated_crates_macro_is_the_union() {
    fields_of(IN_MACROS, "main", |_, tys, diagnostics| {
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let [written, handed, outside] = tys else { panic!("{tys:?}") };
        assert!(matches!(outside.kind(), TyKind::Adt(..)), "{outside:?}");
        assert_eq!(written, outside);
        assert_eq!(handed, outside);
    });
}

/// The `|` of `main`'s macro is `main`'s, so its expansion in `plain` parses to a union type
/// (no syntax error there), and lowering refuses it by `plain`'s missing attribute, naming
/// the rule: the type is never quietly another one.
#[test]
fn a_gated_crates_macro_union_is_refused_in_a_crate_without_the_attribute() {
    fields_of(IN_MACROS, "plain", |_, _, diagnostics| {
        assert_eq!(refusals(diagnostics), [ErrorUnionRefusal::NotEnabled], "{diagnostics:?}");
    });
}

/// Bodies that widen into unions, in a crate with the attribute (`main`), and one without it
/// (`plain`) that calls `main`'s functions taking a union.
const BODIES: &str = r#"
//- minicore: sized, error_union, result, try, from, fmt, error, clone, hash
//- /main.rs crate:main
#![blaze::error_unions]
use core::{error::Error, fmt};

pub struct A;
pub struct B;
pub struct C;
macro_rules! error {
    ($($t:ident)*) => {$(
        impl fmt::Debug for $t {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result { loop {} }
        }
        impl fmt::Display for $t {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result { loop {} }
        }
        impl Error for $t {}
        impl Clone for $t {
            fn clone(&self) -> Self { loop {} }
        }
    )*};
}
error!(A B C);

pub type AB = A | B;
pub type ABC = A | B | C;

pub fn take(_: AB) {}
fn needs_error<E: Error>(_: &E) {}
fn needs_display<E: fmt::Display>(_: &E) {}
fn needs_clone<E: Clone>(_: &E) {}

fn in_return(a: A) -> AB { a }
fn as_argument(b: B) { take(b) }
fn in_typed_let(b: B) { let _x: A | B = b; }
macro_rules! union_of { ($a:ty, $b:ty) => { $a | $b }; }
macro_rules! typed_let { ($t:ty, $e:expr) => { { let _x: $t = $e; } }; }
fn in_macro_written(a: A) -> union_of!(A, B) { a }
fn in_macro_handed(b: B) { typed_let!(A | B, b) }
#[test]
fn under_test() { let _x: A | B = B; }
fn in_err(a: A) -> Result<(), AB> { Err(a) }
fn subset_union(ab: AB) -> ABC { ab }
fn same_union(ab: B | A) -> AB { ab }

fn try_single(r: Result<(), A>) -> Result<(), AB> { r?; Ok(()) }
fn try_subset(r: Result<(), AB>) -> Result<(), ABC> { r?; Ok(()) }
fn try_same(r: Result<(), AB>) -> Result<(), B | A> { r?; Ok(()) }
fn try_not_a_member(r: Result<(), C>) -> Result<(), AB> { r?; Ok(()) }
fn try_not_a_subset(r: Result<(), ABC>) -> Result<(), AB> { r?; Ok(()) }

fn delegated(e: AB) {
    needs_error(&e);
    needs_display(&e);
    needs_clone(&e);
}

pub struct S { pub n: u8 }
pub enum E { X, Y(u8) }
pub struct NotIn;
pub type SE = S | E;
fn takes_s(_: S) {}
fn takes_s_ref(_: &S) {}
fn by_member(u: S | E) -> u8 {
    match u {
        S { n } => n,
        E::X => 0,
        E::Y(n) => n,
    }
}
fn by_member_through_a_reference(u: &(S | E)) -> u8 {
    match u {
        S { n } => *n,
        E::X => 0,
        E::Y(n) => *n,
    }
}
fn bound_whole(u: S | E) {
    match u {
        s @ S { .. } => takes_s(s),
        E::X | E::Y(_) => {}
    }
}
fn bound_whole_through_a_reference(u: &(S | E)) {
    match u {
        s @ S { .. } => takes_s_ref(s),
        _ => {}
    }
}
fn no_member(u: S | E) {
    match u {
        NotIn => {}
        _ => {}
    }
}
//- /plain.rs crate:plain deps:main
fn not_gated() {
    main::take(main::A);
}
fn not_gated_pattern(u: main::SE) {
    match u {
        main::S { .. } => {}
        _ => {}
    }
}
"#;

/// The function `name` of `krate`.
fn function(db: &Db, krate: &str, name: &str) -> FunctionId {
    crate_def_map(db, crate_named(db, krate))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the fixture's `{krate}::{name}`"))
}

/// Infers `krate`'s function `name` and passes its result and how many of its expressions
/// were widened into a union by coercion.
fn inferred(krate: &str, name: &str, check: impl FnOnce(&Db, &InferenceResult<'_>, usize)) {
    let db = fixture::db(BODIES).into_db();
    blaze_query::db::attach(&db, || {
        let func = function(&db, krate, name);
        let result =
            InferenceResult::of(&db, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)));
        let widened = Body::of(&db, DefWithBodyId::FunctionId(func))
            .exprs()
            .filter(|(id, _)| {
                result
                    .expr_adjustment(*id)
                    .is_some_and(|it| it.iter().any(|it| matches!(it.kind, Adjust::ErrorUnion)))
            })
            .count();
        check(&db, result, widened);
    })
}

fn assert_clean(name: &str, result: &InferenceResult<'_>) {
    assert!(result.diagnostics().is_empty(), "{name}: {:?}", result.diagnostics());
    assert!(!result.has_type_mismatches(), "{name} has a type mismatch");
}

#[test]
fn a_member_coerces_to_the_union_wherever_a_union_is_expected() {
    for name in ["in_return", "as_argument", "in_typed_let", "in_err", "subset_union"] {
        inferred("main", name, |_, result, widened| {
            assert_clean(name, result);
            assert_eq!(widened, 1, "{name} widens once");
        });
    }
}

/// A union a macro's definition writes, one handed to a macro, and one in a function under
/// `#[test]` (read as the attribute macro's expansion) type-check as one outside a macro
/// does: the member coerces to it, once.
#[test]
fn a_member_coerces_to_a_union_a_macro_carries() {
    for name in ["in_macro_written", "in_macro_handed", "under_test"] {
        inferred("main", name, |_, result, widened| {
            assert_clean(name, result);
            assert_eq!(widened, 1, "{name} widens once");
        });
    }
}

/// `B | A` is `A | B`: they unify, no widening.
#[test]
fn the_same_union_is_not_widened() {
    inferred("main", "same_union", |_, result, widened| {
        assert_clean("same_union", result);
        assert_eq!(widened, 0);
    });
}

/// `plain` lacks the attribute: `main::take`'s union parameter takes no `A`.
#[test]
fn a_crate_without_the_attribute_does_not_coerce() {
    inferred("plain", "not_gated", |_, result, widened| {
        assert_eq!(widened, 0);
        assert!(result.has_type_mismatches(), "{:?}", result.diagnostics());
    });
}

#[test]
fn question_mark_widens_a_member_or_a_subset() {
    for name in ["try_single", "try_subset", "try_same"] {
        inferred("main", name, |_, result, _| assert_clean(name, result));
    }
}

/// `?` from a type that is not a member, or a union that is not a subset, is refused: the
/// bound that does not hold is the union's `From` of it, or under it `UnionFrom`, which the
/// diagnostic names the missing members of.
#[test]
fn question_mark_refuses_what_is_not_a_subset() {
    for name in ["try_not_a_member", "try_not_a_subset"] {
        inferred("main", name, |db, result, _| {
            let interner = DbInterner::new_no_crate(db);
            let union_from =
                lang_items(db, crate_named(db, "main")).ErrorUnionFrom.expect("the lang item");
            let from_or_union_from = result.diagnostics().iter().any(|it| match it {
                InferenceDiagnostic::SolverDiagnostic(it) => match &it.kind {
                    SolverDiagnosticKind::TraitUnimplemented {
                        trait_predicate,
                        parent_trait_predicates,
                        ..
                    } => std::iter::once(trait_predicate)
                        .chain(parent_trait_predicates)
                        .any(|it| {
                            let id = it.get(interner).def_id().0;
                            id == union_from || TraitSignature::of(db, id).name.as_str() == "From"
                        }),
                    _ => false,
                },
                _ => false,
            });
            assert!(from_or_union_from, "{name}: {:?}", result.diagnostics());
        });
    }
}

/// core's `Debug`, `Display`, `Error` and `Clone` of a union hold when its members' do.
#[test]
fn a_union_is_an_error_when_its_members_are() {
    inferred("main", "delegated", |_, result, _| assert_clean("delegated", result));
}

/// How many of the patterns of `krate`'s function `name` match an error union by member.
fn member_patterns(krate: &str, name: &str, check: impl FnOnce(&InferenceResult<'_>, usize)) {
    let db = fixture::db(BODIES).into_db();
    blaze_query::db::attach(&db, || {
        let func = function(&db, krate, name);
        let result =
            InferenceResult::of(&db, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)));
        let by_member = Body::of(&db, DefWithBodyId::FunctionId(func))
            .pats()
            .filter(|(id, _)| {
                result
                    .pat_adjustment(*id)
                    .is_some_and(|it| it.iter().any(|it| it.kind == PatAdjust::UnionMember))
            })
            .count();
        check(result, by_member);
    })
}

/// A struct pattern and an enum's variant patterns each match the member they are of: one
/// adjustment per pattern, last after the references peeled, and the bindings have the
/// member's field types (the bodies type-check, `*n` of a binding by reference included).
#[test]
fn a_constructor_pattern_of_a_member_matches_the_union_by_member() {
    for name in ["by_member", "by_member_through_a_reference"] {
        member_patterns("main", name, |result, by_member| {
            assert_clean(name, result);
            assert_eq!(by_member, 3, "{name}: `S {{ n }}`, `E::X` and `E::Y(n)`");
        });
    }
}

/// `s @ S { .. }` binds the member, not the union: `s` is an `S` (a `&S` through a
/// reference), and the adjustment is on the binding, so the pattern after `@` has none.
#[test]
fn a_binding_at_a_member_pattern_binds_the_member() {
    member_patterns("main", "bound_whole", |result, by_member| {
        assert_clean("bound_whole", result);
        assert_eq!(by_member, 3, "`s @ S {{ .. }}`, `E::X` and `E::Y(_)`");
    });
    member_patterns("main", "bound_whole_through_a_reference", |result, by_member| {
        assert_clean("bound_whole_through_a_reference", result);
        assert_eq!(by_member, 1);
    });
}

/// A pattern of a type that is no member is checked against the union as it is: a mismatch,
/// never a match of nothing. And a crate without the attribute matches nothing by member.
#[test]
fn a_pattern_of_no_member_and_a_crate_without_the_attribute_are_mismatches() {
    member_patterns("main", "no_member", |result, by_member| {
        assert_eq!(by_member, 0);
        assert!(result.has_type_mismatches(), "{:?}", result.diagnostics());
    });
    member_patterns("plain", "not_gated_pattern", |result, by_member| {
        assert_eq!(by_member, 0);
        assert!(result.has_type_mismatches(), "{:?}", result.diagnostics());
    });
}

/// The names of the members of the union `ty`, in the order the type has them.
fn member_names(db: &Db, ty: Ty<'_>) -> Vec<String> {
    let interner = DbInterner::new_with(db, crate_named(db, "main"));
    let members = crate::error_union::members(interner, ty).expect("an error union");
    members
        .iter()
        .map(|member| match member.kind() {
            TyKind::Adt(def, _) => match def.def_id() {
                AdtId::StructId(it) => StructSignature::of(db, it).name.as_str().to_owned(),
                AdtId::EnumId(it) => {
                    hir_def::signatures::EnumSignature::of(db, it).name.as_str().to_owned()
                }
                AdtId::UnionId(_) => panic!("no member is a union"),
            },
            other => panic!("a member that is no item: {other:?}"),
        })
        .collect()
}

/// The same three members, as two builds of one crate have them: `items` declares them in
/// one order (so the database numbers them in that order), and the unions are written in
/// `orders`, in the crate that declares the members and in one that depends on it.
fn a_build(items: &str, orders: [&str; 3]) -> String {
    let [first, second, elsewhere] = orders;
    format!(
        "//- minicore: sized, error_union\n\
         //- /main.rs crate:main\n#![blaze::error_unions]\n{items}\n\
         pub struct V {{ first: {first}, second: {second} }}\n\
         //- /user.rs crate:user deps:main\n#![blaze::error_unions]\n\
         use main::{{FsError, Missing, Stopped}};\n\
         struct V {{ elsewhere: {elsewhere} }}\n"
    )
}

/// The invariant: a set's members have one order, whatever order they are written in,
/// whatever order their items are declared in (which is the order a database numbers them
/// in), whichever crate writes the set and whichever build lowers it; so the set is one
/// type, with one representation, everywhere ((c) of the feature's check, rule
/// `deterministic-output`). Two builds that declare and write the members in opposite
/// orders agree on the order, and the crate that depends on the members' crate has the very
/// type that crate has.
#[test]
fn a_sets_members_have_one_order_in_every_crate_and_every_build() {
    let builds = [
        a_build(
            "pub struct Missing;\npub struct FsError;\npub enum Stopped { Cancelled }",
            ["Missing | FsError | Stopped", "Stopped | FsError | Missing", "FsError | Stopped | Missing"],
        ),
        a_build(
            "pub struct Unrelated;\npub enum Stopped { Cancelled }\npub struct FsError;\npub struct Missing;",
            ["Stopped | Missing | FsError", "FsError | Missing | Stopped", "Missing | Stopped | FsError"],
        ),
    ];
    let mut orders = Vec::new();
    for build in &builds {
        let mut in_main = None;
        fields_of(build, "main", |db, tys, diagnostics| {
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            let [first, second] = tys else { panic!("{tys:?}") };
            assert_eq!(first, second, "one type, whatever order it is written in");
            in_main = Some(member_names(db, *first));
        });
        fields_of(build, "user", |db, tys, diagnostics| {
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            let [elsewhere] = tys else { panic!("{tys:?}") };
            assert_eq!(Some(member_names(db, *elsewhere)), in_main, "one order in every crate");
        });
        orders.push(in_main.expect("the union of `main`"));
    }
    assert_eq!(orders[0], orders[1], "one order in every build");
    let mut sorted = orders[0].clone();
    sorted.sort();
    assert_eq!(sorted, ["FsError", "Missing", "Stopped"]);
}

/// The control of the invariant above: the order is no accident of these members' names or
/// of the order they are declared in. Members named so that their names sort one way are
/// not all ordered by name, which an order by name (or by declaration) would be: the order
/// is by identity, a hash.
#[test]
fn the_oracle_catches_an_order_by_name_or_declaration() {
    let names = ["Aa", "Bb", "Cc", "Dd", "Ee", "Ff", "Gg", "Hh"];
    let items: String = names.iter().map(|it| format!("pub struct {it};\n")).collect();
    let pairs: String = names
        .windows(2)
        .enumerate()
        .map(|(at, pair)| format!("p{at}: {} | {},\n", pair[0], pair[1]))
        .collect();
    let fixture = format!(
        "//- minicore: sized, error_union\n//- /main.rs crate:main\n\
         #![blaze::error_unions]\n{items}struct V {{\n{pairs}}}\n"
    );
    fields_of(&fixture, "main", |db, tys, diagnostics| {
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let by_name = tys
            .iter()
            .filter(|ty| {
                let members = member_names(db, **ty);
                members[0] < members[1]
            })
            .count();
        assert!(
            by_name != 0 && by_name != tys.len(),
            "{by_name} of {} pairs are in the order of their names: an order by identity \
             has some each way",
            tys.len()
        );
    });
}
