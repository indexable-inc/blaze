//! blaze's records (`#![blaze::records]`, docs/v2/plan/design/records.md): `{ a: A, b: B }` is
//! core's `Record` over its fields in one canonical order, so the order written is no part of
//! the type; a record has a trait exactly when the tuple of its fields has it; a record value
//! and a record pattern are checked against the record's fields by name; a record parameter's
//! defaults are constants of the function's declaration, read one field at a time by the calls
//! that leave that field out; and each refusal names what it refuses.

use base_db::{Crate, Db, all_crates};
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, FunctionId, ModuleDefId, VariantId,
    nameres::crate_def_map,
    signatures::{FunctionSignature, StructSignature},
};
use hir_expand::{fixture, name::Name};
use rustc_type_ir::{TyKind, inherent::IntoKind as _};

use crate::{
    InferBodyId, InferenceDiagnostic, InferenceResult, RecordProblem, RecordRefusal,
    TyLoweringDiagnostic,
    db::HirDatabase,
    next_solver::Ty,
    record::record_fields,
    record_param_default, record_param_defaults,
};

const TYPES: &str = r#"
//- minicore: sized, record
//- /main.rs crate:main
#![blaze::records]
struct A;
struct B;
struct V {
    ab: { a: A, b: B },
    ba: { b: B, a: A },
    swapped: { a: B, b: A },
    other: { a: A, c: B },
    one: { a: A },
    empty: {},
    nested: { inner: { b: B, a: A }, x: A },
    nested_again: { x: A, inner: { a: A, b: B } },
    twice: { a: A, b: B, a: B },
    defaulted: { a: u8 = 1 },
}
//- /other.rs crate:other
struct A;
struct V {
    a: { a: A },
}
"#;

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// The field types of `krate`'s struct `V` in `fixture`, in order, and the diagnostics of
/// lowering them.
fn fields(fixture: &str, krate: &str, check: impl FnOnce(&Db, &[Ty<'_>], &[TyLoweringDiagnostic])) {
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

fn refusals(diagnostics: &[TyLoweringDiagnostic]) -> Vec<RecordRefusal> {
    diagnostics
        .iter()
        .filter_map(|it| match it {
            TyLoweringDiagnostic::RecordRefused { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}

#[test]
fn a_record_is_one_type_whatever_the_order_of_its_fields() {
    fields(TYPES, "main", |db, tys, _| {
        let [ab, ba, swapped, other, one, empty, nested, nested_again, ..] = tys else {
            panic!("{tys:?}")
        };
        assert_eq!(ab, ba, "the order written is no part of the type");
        assert_ne!(ab, swapped, "the same names with other types are another record");
        assert_ne!(ab, other, "other names are another record");
        assert_ne!(ab, one);
        assert_ne!(one, empty);
        assert_eq!(nested, nested_again, "a record in a record is canonical too");
        let lang_items = hir_def::lang_item::lang_items(db, crate_named(db, "main"));
        let record = record_fields(lang_items, *ab).expect("a record");
        assert_eq!(record.names().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(record_fields(lang_items, *swapped).unwrap().list, record.list);
        assert_eq!(record_fields(lang_items, *empty).unwrap().len(), 0);
        match ab.kind() {
            TyKind::Adt(_, args) => assert_eq!(args.len(), 2, "the names and the tuple: {ab:?}"),
            other => panic!("core's `Record`, not {other:?}"),
        }
    });
}

#[test]
fn a_record_type_is_refused_naming_the_field_and_the_rule() {
    fields(TYPES, "main", |_, _, diagnostics| {
        assert_eq!(
            refusals(diagnostics),
            // `twice` names `a` again at its third field; `defaulted` is no parameter.
            [RecordRefusal::DuplicateField(2), RecordRefusal::DefaultOutsideParameter(0)],
            "{diagnostics:?}",
        );
    });
}

/// `other` shares `main`'s source root, so its file parses with the record; hir-ty refuses
/// it, as the crate lacks the attribute.
#[test]
fn a_crate_without_the_attribute_is_refused() {
    fields(TYPES, "other", |_, _, diagnostics| {
        assert_eq!(refusals(diagnostics), [RecordRefusal::NotEnabled], "{diagnostics:?}");
    });
}

const BODIES: &str = r#"
//- minicore: sized, record, copy, clone, eq, default, builtin_impls
//- /main.rs crate:main
#![blaze::records]
pub struct Yes;
impl Clone for Yes {
    fn clone(&self) -> Yes { Yes }
}
impl Copy for Yes {}
impl PartialEq for Yes {
    fn eq(&self, _: &Yes) -> bool { true }
}
impl Default for Yes {
    fn default() -> Yes { Yes }
}
pub struct No;

const MODE: u32 = 0o644;
const fn same(x: u32) -> u32 { x }

pub fn open(path: u8, opts: { create: bool = false, mode: u32 = MODE, depth: u32 = same(2) }) {}
pub fn required(opts: { name: u8, create: bool = false }) {}
pub fn same_but_mode(path: u8, opts: { create: bool = false, mode: u32 = 0o600, depth: u32 = same(2) }) {}
pub fn takes(point: { x: i32, y: i32 }) -> i32 { point.x }
extern "C" fn foreign(opts: { a: u8 }) {}

pub trait Store {
    fn put(&self, key: u8, opts: { sync: bool = true, retries: u8 = 3 });
}
pub struct Disk;
impl Store for Disk {
    fn put(&self, key: u8, opts: { sync: bool, retries: u8 }) {}
}
pub struct Greedy;
impl Store for Greedy {
    fn put(&self, key: u8, opts: { sync: bool = false, retries: u8 }) {}
}

fn needs_clone<T: Clone>(_: &T) {}
fn needs_copy<T: Copy>(_: &T) {}
fn needs_eq<T: PartialEq>(_: &T) {}
fn needs_default<T: Default>(_: &T) {}

fn literal_in_any_order() -> i32 {
    let a: { x: i32, y: i32 } = { y: 2, x: 1 };
    let b = { x: 1, y: 2 };
    let c: { y: i32, x: i32 } = b;
    let _first = takes(a);
    let _second = takes({ y: 4, x: 3 });
    c.x
}
fn shorthand_and_field_reads(x: i32, y: i32) -> (i32, i32, i32) {
    let p = { x, y };
    let one = { x, };
    (p.x, p.y, one.x)
}
fn functional_update(old: { x: i32, y: i32 }) -> { x: i32, y: i32 } {
    { x: 5, ..old }
}
fn patterns(p: { x: i32, y: i32 }, r: &{ x: i32, y: i32 }) -> (i32, i32, i32, &i32, i32) {
    let { x, y: other } = p;
    let { x: first, .. } = p;
    let { y: by_ref, .. } = r;
    let whole = match p {
        { x: 0, y } => y,
        { x, .. } => x,
    };
    (x, other, first, by_ref, whole)
}
fn pattern_names_every_field() -> i32 {
    let { a, b } = { b: 2, a: 1 };
    let _b: i32 = b;
    a
}
fn defaults_fill_what_is_left_out(disk: &Disk) {
    open(1, { create: true, .. });
    open(1, { depth: 1, .. });
    open(1, { create: true, mode: 1, depth: 2 });
    required({ name: 1, .. });
    disk.put(1, { retries: 1, .. });
    Store::put(disk, 1, { sync: false, .. });
    Store::put(disk, 1, { sync: false, retries: 2 });
}
fn has_the_traits_its_fields_have(r: { a: Yes, b: Yes }) {
    needs_clone(&r);
    needs_copy(&r);
    needs_eq(&r);
    needs_default(&r);
}
fn lacks_clone(r: { a: Yes, b: No }) { needs_clone(&r); }
fn lacks_copy(r: { a: Yes, b: No }) { needs_copy(&r); }
fn lacks_eq(r: { a: Yes, b: No }) { needs_eq(&r); }
fn lacks_default(r: { a: Yes, b: No }) { needs_default(&r); }

fn missing_field() { takes({ x: 1 }); }
fn unknown_field() { takes({ x: 1, y: 2, z: 3 }); }
fn field_twice() { let r = { x: 1, x: 2 }; }
fn required_left_out() { required({ create: true, .. }); }
fn through_a_pointer(f: fn(u8, { create: bool, mode: u32, depth: u32 })) { f(1, { create: true, .. }); }
fn rest_of_an_unknown_type(x: i32) { let r = { x, .. }; }
fn update_of_an_unknown_type(x: i32, old: { x: i32, y: i32 }) { let r = { x, ..old }; }
fn rest_where_no_record_is_expected() { let r: u8 = { x: 1, .. }; }
fn pattern_misses_a_field(p: { x: i32, y: i32 }) { let { x } = p; }
fn pattern_names_no_field(p: { x: i32, y: i32 }) { let { x, z, .. } = p; }
fn pattern_on_no_record(p: u8) { let { x, .. } = p; }
fn field_of_no_such_name(p: { x: i32 }) -> i32 { p.z }
//- /plain.rs crate:plain deps:main
fn not_gated() {
    main::takes({ x: 1, y: 2 });
}
"#;

/// The function `name` of `krate`: one of its module-level functions.
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

/// Infers `krate`'s function `name` of [`BODIES`].
fn inferred(krate: &str, name: &str, check: impl FnOnce(&Db, &InferenceResult<'_>)) {
    let db = fixture::db(BODIES).into_db();
    blaze_query::db::attach(&db, || {
        let func = function(&db, krate, name);
        let body = InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func));
        check(&db, InferenceResult::of(&db, body));
    })
}

fn problems(result: &InferenceResult<'_>) -> Vec<RecordProblem> {
    result
        .diagnostics()
        .iter()
        .filter_map(|it| match it {
            // clone: a test's copy of the reason
            InferenceDiagnostic::RecordRefused { problem, .. } => Some(problem.clone()),
            _ => None,
        })
        .collect()
}

fn assert_clean(name: &str, result: &InferenceResult<'_>) {
    assert!(result.diagnostics().is_empty(), "{name}: {:?}", result.diagnostics());
    assert!(!result.has_type_mismatches(), "{name} has a type mismatch");
}

fn field(name: &str) -> Name {
    Name::new_root(name)
}

#[test]
fn values_field_reads_patterns_and_updates_check() {
    for name in [
        "literal_in_any_order",
        "shorthand_and_field_reads",
        "functional_update",
        "patterns",
        "pattern_names_every_field",
        "defaults_fill_what_is_left_out",
    ] {
        inferred("main", name, |_, result| assert_clean(name, result));
    }
}

#[test]
fn a_record_has_a_trait_exactly_when_its_fields_have_it() {
    inferred("main", "has_the_traits_its_fields_have", |_, result| {
        assert_clean("has_the_traits_its_fields_have", result)
    });
    for name in ["lacks_clone", "lacks_copy", "lacks_eq", "lacks_default"] {
        inferred("main", name, |_, result| {
            let unimplemented = result
                .diagnostics()
                .iter()
                .any(|it| matches!(it, InferenceDiagnostic::SolverDiagnostic(_)));
            assert!(unimplemented, "{name}: {:?}", result.diagnostics());
            assert!(problems(result).is_empty(), "{name}: {:?}", problems(result));
        });
    }
}

#[test]
fn each_refusal_names_the_field_or_the_rule() {
    let cases: [(&str, Vec<RecordProblem>); 11] = [
        ("missing_field", vec![RecordProblem::MissingField(field("y"))]),
        ("unknown_field", vec![RecordProblem::NoSuchField(field("z"))]),
        ("field_twice", vec![RecordProblem::DuplicateField(field("x"))]),
        ("through_a_pointer", vec![RecordProblem::NoCallee]),
        ("rest_of_an_unknown_type", vec![RecordProblem::TypeUnknown]),
        ("update_of_an_unknown_type", vec![RecordProblem::TypeUnknown]),
        ("rest_where_no_record_is_expected", vec![RecordProblem::NotARecord]),
        ("pattern_misses_a_field", vec![RecordProblem::MissingField(field("y"))]),
        ("pattern_names_no_field", vec![RecordProblem::NoSuchField(field("z"))]),
        ("pattern_on_no_record", vec![RecordProblem::NotARecord]),
        ("field_of_no_such_name", vec![]),
    ];
    for (name, expected) in cases {
        inferred("main", name, |_, result| assert_eq!(problems(result), expected, "{name}"));
    }
    // `p.z` on a record with no `z` is the unresolved field it is on any type.
    inferred("main", "field_of_no_such_name", |_, result| {
        let unresolved = result
            .diagnostics()
            .iter()
            .any(|it| matches!(it, InferenceDiagnostic::UnresolvedField { .. }));
        assert!(unresolved, "{:?}", result.diagnostics());
    });
}

#[test]
fn a_field_left_out_with_no_default_names_the_field_and_the_function() {
    inferred("main", "required_left_out", |db, result| {
        let function = function(db, "main", "required");
        let expected = RecordProblem::MissingDefault { field: field("name"), function };
        assert_eq!(problems(result), [expected]);
    });
}

/// `plain` lacks the attribute: the record value it writes is refused, whatever it is
/// passed to.
#[test]
fn a_crate_without_the_attribute_writes_no_record_value() {
    inferred("plain", "not_gated", |_, result| {
        assert_eq!(problems(result), [RecordProblem::NotEnabled]);
    });
}

/// The type of parameter `index` of `function`.
fn parameter<'db>(db: &'db Db, function: FunctionId, index: usize) -> Ty<'db> {
    let sig = db
        .callable_item_signature(CallableDefId::FunctionId(function))
        .instantiate_identity()
        .skip_norm_wip()
        .skip_binder();
    sig.inputs_and_output.inputs()[index]
}

#[test]
fn defaults_are_no_part_of_the_type_and_are_keyed_one_field_at_a_time() {
    let db = fixture::db(BODIES).into_db();
    blaze_query::db::attach(&db, || {
        let open = function(&db, "main", "open");
        let other = function(&db, "main", "same_but_mode");
        // The parameter is the record `{ create: bool, depth: u32, mode: u32 }` in both: a
        // default is no part of it, so a changed default changes no caller's types.
        assert_eq!(parameter(&db, open, 1), parameter(&db, other, 1));
        let lang_items = hir_def::lang_item::lang_items(&db, crate_named(&db, "main"));
        let record = record_fields(lang_items, parameter(&db, open, 1)).expect("a record");
        assert_eq!(record.names().collect::<Vec<_>>(), ["create", "depth", "mode"]);

        // The defaults, in the order declared.
        let declared = record_param_defaults(&db, open);
        let names: Vec<_> = declared.iter().map(|it| (it.param, it.field.as_str())).collect();
        assert_eq!(names, [(1, "create"), (1, "mode"), (1, "depth")]);

        // The key is the function, the parameter and the field; the value that one constant.
        // The two functions differ in `mode`'s default alone: a caller that leaves out
        // `create` or `depth` reads a value that did not change, and one that leaves out
        // `mode` reads one that did.
        let default = |function, name: &str| record_param_default(&db, function, 1, field(name));
        assert!(default(open, "create").is_some());
        assert_eq!(default(open, "create"), default(other, "create"));
        assert_ne!(default(open, "mode"), default(other, "mode"));
        assert_eq!(default(open, "create"), default(open, "create"), "evaluated the same per call");
        assert_eq!(default(open, "missing"), None);
        assert_eq!(record_param_default(&db, open, 0, field("create")), None, "another parameter");
        let required = function(&db, "main", "required");
        assert_eq!(record_param_default(&db, required, 0, field("name")), None);
        assert!(record_param_default(&db, required, 0, field("create")).is_some());
    });
}

/// The method `name` of the impl of `Store` for the struct `for_`.
fn impl_method(db: &Db, for_: &str, name: &str) -> FunctionId {
    let krate = crate_named(db, "main");
    crate_def_map(db, krate)
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).impls().collect::<Vec<_>>())
        .find_map(|impl_| {
            let self_ty = db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
            let TyKind::Adt(def, _) = self_ty.kind() else { return None };
            let AdtId::StructId(it) = def.def_id() else { return None };
            if StructSignature::of(db, it).name.as_str() != for_ {
                return None;
            }
            impl_.impl_items(db).items.iter().find_map(|(item, id)| match id {
                hir_def::AssocItemId::FunctionId(it) if item.as_str() == name => Some(*it),
                _ => None,
            })
        })
        .unwrap_or_else(|| panic!("the fixture's `impl Store for {for_}`"))
}

#[test]
fn a_trait_methods_defaults_are_its_declarations() {
    let db = fixture::db(BODIES).into_db();
    blaze_query::db::attach(&db, || {
        let disk = impl_method(&db, "Disk", "put");
        let declared = crate::record_defaults_owner(&db, disk);
        assert_ne!(declared, disk, "the trait's method declares them");
        assert_eq!(record_param_defaults(&db, declared).len(), 2);
        // The impl's method has the trait's default, the same constant.
        let of_impl = record_param_default(&db, disk, 2, field("sync"));
        assert!(of_impl.is_some());
        assert_eq!(of_impl, record_param_default(&db, declared, 2, field("sync")));
        // An impl that writes a default is refused, naming the field.
        let greedy = impl_method(&db, "Greedy", "put");
        let lowered = crate::lower::fn_sig_for_fn(&db, greedy);
        assert_eq!(
            refusals(lowered.diagnostics()),
            [RecordRefusal::DefaultInTraitImpl(0)],
            "{:?}",
            lowered.diagnostics(),
        );
        assert!(refusals(crate::lower::fn_sig_for_fn(&db, disk).diagnostics()).is_empty());
    });
}

#[test]
fn a_function_of_another_abi_takes_no_record() {
    let db = fixture::db(BODIES).into_db();
    blaze_query::db::attach(&db, || {
        let foreign = function(&db, "main", "foreign");
        let lowered = crate::lower::fn_sig_for_fn(&db, foreign);
        assert_eq!(refusals(lowered.diagnostics()), [RecordRefusal::ExternAbi]);
        let takes = function(&db, "main", "takes");
        assert!(refusals(crate::lower::fn_sig_for_fn(&db, takes).diagnostics()).is_empty());
    });
}
