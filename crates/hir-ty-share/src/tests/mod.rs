//! The share key over fixtures: which instances of one body are one, which are not and why,
//! and that equal keys mean the same monomorphized MIR up to the argument types' names.
//!
//! The programs in `programs/` are the same cases as whole programs, with what the pinned
//! nightly's rustc prints for each, for a differential run with sharing on.

mod db;
mod identity;

use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId, TypeAliasId,
    expr_store::Body,
    hir::Expr,
    nameres::crate_def_map,
    signatures::{FunctionSignature, TypeAliasSignature},
};
use hir_ty::{
    InferBodyId, InferenceResult, ParamEnvAndCrate, TyDefId,
    consteval::usize_const,
    db::HirDatabase,
    intrinsic::Intrinsic,
    mir::{MirOptLevel, MirOptions},
    next_solver::{GenericArg, GenericArgs, ParamEnv},
};
use hir_ty_mir::mir::{monomorphized, monomorphized_args, print_mir_body};
use hir_expand::fixture;

use crate::{
    BodyOptions, Class, OptionsSet, ShareDef, ShareInstance, ShareKey, Unshared, classify,
    share_key,
};
use db::TestDB;

const fn options(level: MirOptLevel, ub_checks: bool, inline: bool) -> MirOptions {
    MirOptions { level, ub_checks, lifetime_markers: true, inline }
}

const OFF: MirOptions = options(MirOptLevel::Off, false, false);
const DEBUG: MirOptions = options(MirOptLevel::Debug, false, false);
const DEBUG_CHECKED: MirOptions = options(MirOptLevel::Debug, true, false);
const RELEASE: MirOptions = options(MirOptLevel::Release, false, false);
/// A body of the library's crates: always at release, inlined, as shipped (`Build::options`).
const LANG: MirOptions = options(MirOptLevel::Release, false, true);

const RUNTIME: BodyOptions =
    BodyOptions { lang: OptionsSet::of(&[OFF]), other: OptionsSet::of(&[OFF]) };

/// What a build of debug units with and without debug assertions and release units asks.
const EVERY_LEVEL: BodyOptions = BodyOptions {
    lang: OptionsSet::of(&[LANG]),
    other: OptionsSet::of(&[OFF, DEBUG, DEBUG_CHECKED, RELEASE, LANG]),
};

const UB_CHECKED: BodyOptions =
    BodyOptions { lang: OptionsSet::of(&[LANG]), other: OptionsSet::of(&[DEBUG_CHECKED]) };

const MINICORE: &str =
    "sized, copy, clone, drop, size_of, unsize, coerce_unsized, dispatch_from_dyn, fn";

/// One program for every case: the generic functions, and the types and constants they are
/// instantiated with, by type aliases. `P`, `Q` and `S` are one `u32` each, so pointers to them
/// have one fact; `R` is a `u64`; `D` has drop glue.
const PROGRAM: &str = r#"
#[rustc_intrinsic]
pub fn type_id<T: ?Sized + 'static>() -> u128;
#[rustc_intrinsic]
pub fn type_name<T: ?Sized>() -> &'static str;
#[rustc_intrinsic]
pub fn needs_drop<T: ?Sized>() -> bool;

pub struct P(pub u32);
pub struct Q(pub u32);
pub struct S(pub u32);
pub struct R(pub u64);
pub struct D(pub u32);
impl Drop for D {
    fn drop(&mut self) {}
}

pub type RefP = &'static P;
pub type RefQ = &'static Q;
pub type RefS = &'static S;
pub type RefR = &'static R;
pub type RefD = &'static D;
pub type OwnP = P;
pub type OwnQ = Q;
pub type OwnD = D;
pub type U32 = u32;
pub type I32 = i32;
pub type PairPD = (P, D);
pub type PairQD = (Q, D);

pub fn id<T>(x: T) -> T { x }
pub fn outer<T>(x: T) -> T { id(x) }
pub fn size<T>() -> usize { core::mem::size_of::<T>() }
pub fn tid<T: 'static>() -> u128 { type_id::<T>() }
pub fn name<T>() -> &'static str { type_name::<T>() }
pub fn nd<T>() -> bool { needs_drop::<T>() }
pub fn hold<T>(x: T) { let _held = x; }
pub fn read<T: Copy>(p: *const T) -> T { unsafe { *p } }
pub fn counter<T>() -> &'static u32 { static C: u32 = 7; &C }

pub trait K { const V: u32; }
impl K for &'static P { const V: u32 = 1; }
impl K for &'static Q { const V: u32 = 2; }
impl K for &'static S { const V: u32 = 1; }
pub fn v<T: K>() -> u32 { T::V }

pub fn plus_zero(x: u32) -> u32 { x }
pub fn plus_one(x: u32) -> u32 { x + 1 }
pub trait Pick { const F: fn(u32) -> u32; }
impl Pick for &'static P { const F: fn(u32) -> u32 = plus_zero; }
impl Pick for &'static Q { const F: fn(u32) -> u32 = plus_one; }
impl Pick for &'static S { const F: fn(u32) -> u32 = plus_zero; }
pub fn pick<T: Pick>(x: u32) -> u32 { (T::F)(x) }

pub trait Out { type O; }
impl Out for &'static P { type O = u32; }
impl Out for &'static Q { type O = u64; }
impl Out for &'static S { type O = u32; }
pub struct Holds<T: Out> { pub f: fn(T::O) }
pub fn holds<T: Out>(h: Holds<T>) -> Holds<T> { h }

pub fn n<const N: usize>() -> usize { N }

pub trait Show { fn show(&self) -> u32; }
impl Show for P { fn show(&self) -> u32 { self.0 } }
impl Show for Q { fn show(&self) -> u32 { self.0 + 1 } }
impl<T: Show> Show for &'static T { fn show(&self) -> u32 { (**self).show() } }
pub fn erase<T: Show>(x: &T) -> &dyn Show { x }
pub fn call<T: Show>(x: &T) -> u32 { x.show() }

pub fn ptr<T>() -> fn(T) -> T { id::<T> }
pub fn with_closure<T>(x: T) -> T { let f = |y: T| y; f(x) }
pub fn apply<F: FnOnce(u32) -> u32>(f: F, x: u32) -> u32 { f(x) }
pub fn two() -> u32 { apply(|x| x + 1, 1) + apply(|x| x * 2, 2) }

pub fn rec<T>(n: u32, x: T) -> T { if n == 0 { x } else { rec::<T>(n - 1, x) } }
pub fn ping<T>(n: u32) -> u32 { if n == 0 { 0 } else { pong::<T>(n - 1) } }
pub fn pong<T>(n: u32) -> u32 { if n == 0 { 1 } else { ping::<T>(n - 1) } }
"#;

fn fixture() -> TestDB {
    fixture::db(&format!("//- minicore: {MINICORE}\n{PROGRAM}")).into_db()
}

fn declarations(db: &TestDB) -> Vec<ModuleDefId> {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .collect()
}

#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    let found: Vec<FunctionId> = declarations(db)
        .into_iter()
        .filter_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} functions named `{name}`", found.len()),
    }
}

#[track_caller]
fn alias(db: &TestDB, name: &str) -> TypeAliasId {
    let found: Vec<TypeAliasId> = declarations(db)
        .into_iter()
        .filter_map(|def| match def {
            ModuleDefId::TypeAliasId(it) if TypeAliasSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} type aliases named `{name}`", found.len()),
    }
}

/// A generic argument: a type by the alias that names it, or a `usize` constant.
#[derive(Clone, Copy)]
enum Arg {
    Ty(&'static str),
    Usize(u128),
}

fn instance<'db>(db: &'db TestDB, func: &str, args: &[Arg]) -> ShareInstance {
    let krate = fixture::test_crate(db);
    let args: Vec<GenericArg<'db>> = args
        .iter()
        .map(|arg| match *arg {
            Arg::Ty(name) => db.ty(TyDefId::TypeAliasId(alias(db, name))).skip_binder().into(),
            Arg::Usize(value) => usize_const(db, Some(value), krate).into(),
        })
        .collect();
    ShareInstance {
        def: ShareDef::Fn(function(db, func)),
        args: GenericArgs::new_from_slice(&args).store(),
    }
}

fn env(db: &TestDB) -> hir_ty::traits::StoredParamEnvAndCrate {
    ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: fixture::test_crate(db) }.store()
}

fn key_at(
    db: &TestDB,
    options: BodyOptions,
    func: &str,
    args: &[Arg],
) -> Result<ShareKey, Unshared> {
    hir_ty::attach_db(db, || {
        let instance = instance(db, func, args);
        share_key(db, &instance, &env(db), options).clone()
    })
}

#[track_caller]
fn key(db: &TestDB, func: &str, args: &[Arg]) -> ShareKey {
    key_at(db, RUNTIME, func, args)
        .unwrap_or_else(|why| panic!("`{func}` has no share key: {why}"))
}

#[track_caller]
fn same(db: &TestDB, func: &str, a: Arg, b: Arg) {
    assert_eq!(key(db, func, &[a]), key(db, func, &[b]), "`{func}` shares");
}

#[track_caller]
fn apart(db: &TestDB, func: &str, a: Arg, b: Arg) {
    assert_ne!(key(db, func, &[a]), key(db, func, &[b]), "`{func}` must not share");
}

use Arg::{Ty, Usize};

#[test]
fn reflection_intrinsics_read_identity() {
    for index in 0..Intrinsic::COUNT as u16 {
        let intrinsic = Intrinsic::from_index(index).expect("an intrinsic at each index");
        let name = intrinsic.to_string();
        let class = classify(intrinsic);
        // `type_id_eq(a: TypeId, b: TypeId)` has no generic argument: it compares two values
        // whose identity the `type_id` calls that made them read.
        if (name.starts_with("type_id") && name != "type_id_eq") || name.starts_with("type_name")
        {
            assert_eq!(class, Class::Identity, "`{name}` tells types apart");
        }
        if name.starts_with("atomic_") || name.starts_with("simd_") {
            assert_eq!(class, Class::Layout, "`{name}` reads only layouts");
        }
    }
}

#[test]
fn intrinsic_classes() {
    for (intrinsic, class) in [
        (Intrinsic::TypeId, Class::Identity),
        (Intrinsic::TypeName, Class::Identity),
        (Intrinsic::AssertInhabited, Class::Identity),
        (Intrinsic::AssertZeroValid, Class::Identity),
        (Intrinsic::ConstEvalSelect, Class::Identity),
        (Intrinsic::SizeOf, Class::Layout),
        (Intrinsic::AlignOf, Class::Layout),
        (Intrinsic::NeedsDrop, Class::Layout),
        (Intrinsic::Transmute, Class::Layout),
        (Intrinsic::DiscriminantValue, Class::Layout),
        (Intrinsic::VariantCount, Class::Layout),
        (Intrinsic::CallerLocation, Class::Layout),
        (Intrinsic::Autodiff, Class::PerInstance),
        (Intrinsic::Offload, Class::PerInstance),
    ] {
        assert_eq!(classify(intrinsic), class, "{intrinsic}");
    }
}

#[test]
fn options_are_a_set() {
    let set = OptionsSet::of(&[RELEASE, OFF, DEBUG_CHECKED]);
    assert!(set.contains(OFF) && set.contains(RELEASE) && set.contains(DEBUG_CHECKED));
    assert!(!set.contains(DEBUG));
    assert!(!set.contains(MirOptions { lifetime_markers: false, ..OFF }));
    assert!(!set.contains(LANG), "an inlined body is not the uninlined one");
    assert_eq!(set.iter().collect::<Vec<_>>(), [OFF, DEBUG_CHECKED, RELEASE]);
    assert!(OptionsSet::of(&[]).is_empty() && OptionsSet::EMPTY.is_empty());
    let every: Vec<MirOptions> = [OFF, DEBUG, DEBUG_CHECKED, RELEASE]
        .into_iter()
        .chain([MirOptLevel::Release, MirOptLevel::Off].map(|level| options(level, true, false)))
        .flat_map(|it| [it, MirOptions { lifetime_markers: false, ..it }])
        .flat_map(|it| [it, MirOptions { inline: true, ..it }])
        .collect();
    let all = OptionsSet::of(&every);
    assert_eq!(all.iter().count(), 24, "each of the twenty-four options has its own bit");
    let mut sorted = every.clone();
    sorted.sort();
    assert_eq!(all.iter().collect::<Vec<_>>(), sorted, "in `MirOptions`' order");
}

#[test]
fn one_instance_asked_twice_and_in_two_databases_has_one_key() {
    let (a, b) = (fixture(), fixture());
    assert_eq!(key(&a, "id", &[Ty("RefP")]), key(&a, "id", &[Ty("RefP")]));
    assert_eq!(key(&a, "id", &[Ty("RefP")]), key(&b, "id", &[Ty("RefP")]), "no id is in a key");
}

#[test]
fn pointers_to_types_of_one_layout_share() {
    let db = fixture();
    same(&db, "id", Ty("RefP"), Ty("RefQ"));
    same(&db, "size", Ty("RefP"), Ty("RefQ"));
    same(&db, "hold", Ty("RefP"), Ty("RefD"));
    apart(&db, "id", Ty("RefP"), Ty("RefR"));
}

#[test]
fn values_of_two_types_do_not_share() {
    let db = fixture();
    apart(&db, "id", Ty("OwnP"), Ty("OwnQ"));
    apart(&db, "id", Ty("U32"), Ty("I32"));
}

#[test]
fn a_callee_is_its_own_share_key() {
    let db = fixture();
    same(&db, "outer", Ty("RefP"), Ty("RefQ"));
    apart(&db, "outer", Ty("RefP"), Ty("RefR"));
}

#[test]
fn type_id_and_type_name_tell_types_apart() {
    let db = fixture();
    apart(&db, "tid", Ty("RefP"), Ty("RefQ"));
    apart(&db, "name", Ty("RefP"), Ty("RefQ"));
    assert_eq!(key(&db, "tid", &[Ty("RefP")]), key(&db, "tid", &[Ty("RefP")]));
}

#[test]
fn needs_drop_is_in_the_fact() {
    let db = fixture();
    same(&db, "nd", Ty("RefP"), Ty("RefD"));
    apart(&db, "nd", Ty("OwnP"), Ty("OwnD"));
    apart(&db, "hold", Ty("PairPD"), Ty("PairQD"));
}

#[test]
fn a_static_in_a_generic_function_is_one_for_every_instance() {
    let db = fixture();
    same(&db, "counter", Ty("RefP"), Ty("RefQ"));
    same(&db, "counter", Ty("OwnP"), Ty("OwnQ"));
}

#[test]
fn an_associated_const_is_its_value() {
    let db = fixture();
    apart(&db, "v", Ty("RefP"), Ty("RefQ"));
    same(&db, "v", Ty("RefP"), Ty("RefS"));
}

/// A constant that is a function pointer is the function it points to, by that function's
/// identity, as the code generator links it.
#[test]
fn a_function_pointer_constant_is_its_function() {
    let db = fixture();
    apart(&db, "pick", Ty("RefP"), Ty("RefQ"));
    same(&db, "pick", Ty("RefP"), Ty("RefS"));
}

/// A field's type is normalized all through once the ADT's arguments are in it: the
/// projection in `fn(T::O)` is the type it names, as rustc has it before layout and codegen.
#[test]
fn a_projection_inside_a_field_is_the_type_it_names() {
    let db = fixture();
    apart(&db, "holds", Ty("RefP"), Ty("RefQ"));
    same(&db, "holds", Ty("RefP"), Ty("RefS"));
}

#[test]
fn a_const_argument_is_its_value() {
    let db = fixture();
    assert_ne!(key(&db, "n", &[Usize(1)]), key(&db, "n", &[Usize(2)]));
    assert_eq!(key(&db, "n", &[Usize(3)]), key(&db, "n", &[Usize(3)]));
}

#[test]
fn a_vtable_and_a_method_tell_types_apart() {
    let db = fixture();
    apart(&db, "erase", Ty("OwnP"), Ty("OwnQ"));
    apart(&db, "call", Ty("RefP"), Ty("RefQ"));
}

#[test]
fn a_function_pointer_is_of_its_instance() {
    let db = fixture();
    apart(&db, "ptr", Ty("RefP"), Ty("RefQ"));
}

#[test]
fn a_closure_shares_with_its_parent() {
    let db = fixture();
    same(&db, "with_closure", Ty("RefP"), Ty("RefQ"));
    apart(&db, "with_closure", Ty("RefP"), Ty("RefR"));
}

/// `apply`'s body calls its closure, which is all its argument says of which: two closures of
/// one signature in a body with no generics, as `and_then`'s in rustc_apfloat's build script,
/// were one callee, so both instances ran the first closure.
#[test]
fn two_closures_of_one_signature_are_two_callees() {
    let db = fixture();
    hir_ty::attach_db(&db, || {
        let owner = DefWithBodyId::from(function(&db, "two"));
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(owner));
        let keys: Vec<ShareKey> = Body::of(&db, owner)
            .exprs()
            .filter(|(_, expr)| matches!(expr, Expr::Closure { .. }))
            .map(|(expr, _)| {
                let args = GenericArgs::new_from_slice(&[infer.expr_ty(expr).into()]).store();
                let instance = ShareInstance { def: ShareDef::Fn(function(&db, "apply")), args };
                share_key(&db, &instance, &env(&db), RUNTIME).clone().unwrap_or_else(|why| panic!("{why}"))
            })
            .collect();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1], "`apply` of two closures calls two bodies");
    });
}

#[test]
fn direct_recursion_shares_and_mutual_recursion_is_unshared() {
    let db = fixture();
    same(&db, "rec", Ty("RefP"), Ty("RefQ"));
    assert_eq!(key_at(&db, RUNTIME, "ping", &[Ty("RefP")]), Err(Unshared::Recursive));
    assert_eq!(key_at(&db, RUNTIME, "pong", &[Ty("RefP")]), Err(Unshared::Recursive));
}

#[test]
fn a_shim_is_unshared() {
    let db = fixture();
    let got = hir_ty::attach_db(&db, || {
        let args = instance(&db, "id", &[Ty("OwnD")]).args;
        let shim = ShareInstance { def: ShareDef::DropGlue, args };
        share_key(&db, &shim, &env(&db), RUNTIME).clone()
    });
    assert_eq!(got, Err(Unshared::Shim("drop glue")));
}

#[test]
fn no_level_is_an_error_and_every_level_is_keyed() {
    let db = fixture();
    let none = BodyOptions { lang: OptionsSet::EMPTY, other: OptionsSet::EMPTY };
    assert_eq!(key_at(&db, none, "id", &[Ty("RefP")]), Err(Unshared::NoOptions));
    let every = |ty| key_at(&db, EVERY_LEVEL, "outer", &[Ty(ty)]).expect("a key at every level");
    assert_eq!(every("RefP"), every("RefQ"));
    assert_ne!(every("RefP"), key(&db, "outer", &[Ty("RefP")]), "the options are in the key");
}

/// With UB checks, `CheckAlignment` and `CheckNull` add code at each read through a raw
/// pointer, `<T as SizedTypeProperties>::ALIGN` and `SIZE` of its pointee: the key reads the
/// body codegen compiles with those options. The fixture has no panic handler, so the passes
/// add nothing here (`check_pointers`); this checks the body is the one asked with the
/// options and that each options is in the key.
#[test]
fn a_ub_checked_body_is_keyed_as_compiled() {
    let db = fixture();
    let checked = |ty| key_at(&db, UB_CHECKED, "read", &[Ty(ty)]).expect("a UB-checked key");
    assert_eq!(checked("RefP"), checked("RefQ"), "pointers of one alignment share");
    assert_ne!(checked("RefP"), checked("U32"), "pointees of two alignments do not");
    assert_ne!(checked("RefP"), key(&db, "read", &[Ty("RefP")]), "the UB checks are in the key");
}

/// The property under the key: for each body and each pair of arguments, equal keys mean the
/// two monomorphized bodies print the same once each argument's name is replaced by one name.
#[test]
fn equal_keys_mean_one_monomorphized_body() {
    let db = fixture();
    let pool = ["RefP", "RefQ", "RefS", "RefR", "RefD", "OwnP", "OwnQ", "OwnD", "U32", "I32"];
    let bodies = ["id", "outer", "size", "nd", "hold", "counter", "with_closure", "rec"];
    for body in bodies {
        let mut shared = 0;
        for (at, &a) in pool.iter().enumerate() {
            for &b in &pool[at + 1..] {
                let (Ok(ka), Ok(kb)) =
                    (key_at(&db, RUNTIME, body, &[Ty(a)]), key_at(&db, RUNTIME, body, &[Ty(b)]))
                else {
                    continue;
                };
                if ka != kb {
                    continue;
                }
                shared += 1;
                let ((raw_a, named_a), (raw_b, named_b)) = (printed(&db, body, a), printed(&db, body, b));
                // Renaming `u32` also renames a `u32` the body names itself (`counter`'s), so
                // bodies that print the same as written are one body too.
                if raw_a != raw_b {
                    assert_eq!(named_a, named_b, "`{body}` of `{a}` and `{b}`");
                }
            }
        }
        assert!(shared > 0, "some instances of `{body}` share");
    }
}

/// A key asked for again is one memo hit, and a hit allocates nothing.
#[test]
fn a_key_hit_allocates_nothing() {
    let db = fixture();
    hir_ty::attach_db(&db, || {
        let instance = instance(&db, "outer", &[Ty("RefP")]);
        let env = env(&db);
        share_key(&db, &instance, &env, RUNTIME).as_ref().expect("a key");
        let (hit, used) =
            blaze_alloc_count::measure(|| share_key(&db, &instance, &env, RUNTIME).is_ok());
        assert!(hit);
        assert_eq!(used.allocations, 0, "a memo hit allocates nothing");
    });
}

/// The key substitutes a body's sites at once, never the whole body: each local's type it reads
/// is the one `monomorphized` puts in the body the code generator compiles.
#[test]
fn sites_are_filled_as_the_body_is() {
    let db = fixture();
    let pool = ["RefP", "RefQ", "RefD", "OwnP", "OwnD", "U32"];
    let bodies = ["id", "outer", "size", "nd", "hold", "counter", "with_closure", "rec"];
    hir_ty::attach_db(&db, || {
        for body in bodies {
            let owner = hir_ty::InferBodyId::DefWithBodyId(function(&db, body).into());
            let generic = db.runtime_mir_body(owner).expect("a body");
            let sites = crate::walk::sites(&db, ShareDef::Fn(function(&db, body)), OFF)
                .as_ref()
                .unwrap_or_else(|why| panic!("`{body}` has no sites: {why}"));
            for arg in pool {
                let instance = instance(&db, body, &[Ty(arg)]);
                let whole = monomorphized(&db, generic, &instance.args, &env(&db)).expect("monomorphized");
                let filled = monomorphized_args(&db, sites.filled.as_ref(), &instance.args, &env(&db))
                    .expect("the sites filled");
                let locals = filled.as_slice();
                assert!(locals.len() >= whole.locals.len(), "`{body}`: every local is a site");
                for (at, (_, local)) in whole.locals.iter().enumerate() {
                    assert_eq!(locals[at].ty(), Some(local.ty.as_ref()), "`{body}` of `{arg}`, local {at}");
                }
            }
        }
    });
}

/// The monomorphized runtime body of `func` with the alias `arg`, printed as it is and with the
/// name of the type `arg` points to or is replaced by `_`.
fn printed(db: &TestDB, func: &str, arg: &'static str) -> (String, String) {
    hir_ty::attach_db(db, || {
        let instance = instance(db, func, &[Ty(arg)]);
        let owner = hir_ty::InferBodyId::DefWithBodyId(function(db, func).into());
        let generic = db.runtime_mir_body(owner).expect("a body");
        let body = monomorphized(db, generic, &instance.args, &env(db)).expect("monomorphized");
        let named = match arg {
            "U32" => "u32",
            "I32" => "i32",
            other => other.trim_start_matches("Ref").trim_start_matches("Own"),
        };
        let raw = print_mir_body(db, &body, func);
        let renamed = replace_word(&raw, named, "_");
        (raw, renamed)
    })
}

/// `text` with each whole identifier `word` replaced by `with`.
fn replace_word(text: &str, word: &str, with: &str) -> String {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(word) {
        out.push_str(&rest[..at]);
        let before = out.chars().next_back();
        let after = rest[at + word.len()..].chars().next();
        if before.is_some_and(is_ident) || after.is_some_and(is_ident) {
            out.push_str(word);
        } else {
            out.push_str(with);
        }
        rest = &rest[at + word.len()..];
    }
    out.push_str(rest);
    out
}

#[test]
fn replace_word_replaces_whole_identifiers() {
    assert_eq!(replace_word("&P Pair P(1) <P>", "P", "_"), "&_ Pair _(1) <_>");
}

/// Every differential program has what rustc prints beside it.
#[test]
fn every_program_has_its_expected_output() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("programs");
    let mut programs = 0;
    for entry in std::fs::read_dir(&dir).expect("the programs directory") {
        let path = entry.expect("an entry").path();
        if path.extension().is_some_and(|it| it == "rs") {
            programs += 1;
            let expected = path.with_extension("stdout");
            assert!(expected.exists(), "{} has no {}", path.display(), expected.display());
        }
    }
    assert!(programs >= 9, "{programs} programs");
}

/// The share of `f::<u32>` in a crate whose `fn f<T>(x: T)` calls `f((x,))`, under the crate
/// attributes `attrs`.
fn growing_chain(attrs: &str) -> Result<ShareKey, Unshared> {
    let db: TestDB = fixture::db(&format!(
        "//- minicore: {MINICORE}\n{attrs}\npub type U32 = u32;\npub fn f<T>(x: T) {{ f((x,)) }}\npub fn main() {{ f(0u32) }}\n"
    ))
    .into_db();
    key_at(&db, RUNTIME, "f", &[Ty("U32")])
}

/// Arguments that grow with each call never end: past the crate's `recursion_limit` (128, or
/// what `#![recursion_limit]` says) the share stops with an error naming the crate, the body
/// and the chain, as rustc's collector does, never a key of the instance's arguments.
#[test]
fn a_chain_of_growing_instances_stops_at_the_recursion_limit() {
    for (attrs, limit) in [("", 128), ("#![recursion_limit = \"16\"]", 16)] {
        let result = growing_chain(attrs);
        let Err(why) = &result else { panic!("`f::<u32>` with {attrs:?} has a key: {result:?}") };
        let Unshared::RecursionLimit { body, depth, limit: found, chain, instance, .. } = why else {
            panic!("`f::<u32>` with {attrs:?}: {why}")
        };
        assert_eq!((body.as_str(), *depth, *found), ("f", limit + 1, limit), "{why}");
        assert!(chain.first().is_some_and(|it| it.contains("f::<u32>")), "{why}");
        // The first ten levels, each its callee with its arguments, then how many more, then the last three.
        assert_eq!(chain.len(), 10 + 1 + 3, "{why}");
        assert!(chain[1].contains("f::<(u32,)>"), "{why}");
        assert!(chain[10].ends_with(" more)"), "{why}");
        assert!(instance.contains("(((u32,),),)"), "{why}");
        assert!(why.to_string().starts_with("reached the recursion limit while instantiating"), "{why}");
    }
}
