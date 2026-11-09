//! Compile identity over fixtures (the walk that was `hir-ty-share`'s share key): which
//! instances of one body are one, which are not and why, and that equal identities mean the
//! same monomorphized MIR up to the argument types' names.

use hir_def::CallableDefId;
use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId, TypeAliasId,
    expr_store::Body as HirBody,
    hir::Expr,
    nameres::crate_def_map,
    signatures::{FunctionSignature, TypeAliasSignature},
};
use hir_expand::fixture;
use hir_ty::next_solver::{DbInterner, Region, RegionExt as _, RegionKind, TyKind};
use hir_ty::{
    InferBodyId, InferenceResult, ParamEnvAndCrate, TyDefId,
    consteval::usize_const,
    db::HirDatabase,
    mir::{MirOptLevel, MirOptions, MirOwner, MirStage},
    next_solver::{GenericArg, GenericArgs, ParamEnv},
};
use hir_ty_mir::mir::{
    OperandKind, TerminatorKind, dispatch::Body, monomorphized, monomorphized_args, print_mir_body,
};
use hir_ty_share::{Through, Unshared};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};

use crate::{CompileIdentity, Compiled, Instance, OptionsSet, Shim, Subject, instance_identity};

type TestDB = base_db::Db;

const fn options(level: MirOptLevel, ub_checks: bool, inline: bool) -> MirOptions {
    MirOptions {
        level,
        ub_checks,
        lifetime_markers: true,
        inline,
        heap_to_stack: false,
        world: None,
    }
}

const OFF: MirOptions = options(MirOptLevel::Off, false, false);
const DEBUG: MirOptions = options(MirOptLevel::Debug, false, false);
const DEBUG_CHECKED: MirOptions = options(MirOptLevel::Debug, true, false);
const RELEASE: MirOptions = options(MirOptLevel::Release, false, false);
/// A body of the library's crates: always at release, inlined, as shipped (`Build::options`).
const LANG: MirOptions = options(MirOptLevel::Release, false, true);

const RUNTIME: Compiled = Compiled { lang: OptionsSet::of(&[OFF]), other: OptionsSet::of(&[OFF]) };

/// What a build of debug units with and without debug assertions and release units asks.
const EVERY_LEVEL: Compiled = Compiled {
    lang: OptionsSet::of(&[LANG]),
    other: OptionsSet::of(&[OFF, DEBUG, DEBUG_CHECKED, RELEASE, LANG]),
};

const UB_CHECKED: Compiled =
    Compiled { lang: OptionsSet::of(&[LANG]), other: OptionsSet::of(&[DEBUG_CHECKED]) };

/// A unit whose bodies are optimized for release with the inliner on, as std's are.
const INLINED: Compiled =
    Compiled { lang: OptionsSet::of(&[LANG]), other: OptionsSet::of(&[LANG]) };

const MINICORE: &str =
    "sized, copy, clone, drop, size_of, unsize, coerce_unsized, dispatch_from_dyn, fn";

/// One program for every case: the generic functions, and the types and constants they are
/// instantiated with, by type aliases. `P`, `Q` and `S` are one `u32` each, so they and
/// pointers to them have one fact; `R` is a `u64`; `D` and `D2` have drop glue; `W`, `UP`
/// and `E2` are a `u32`'s size with another shape (`transparent`, a union, other
/// discriminants), and `E1` and `E3` are one shape.
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
pub struct D2(pub u32);
impl Drop for D2 {
    fn drop(&mut self) {}
}
#[repr(transparent)]
pub struct W(pub u32);
pub union UP { pub a: u32 }
pub enum E1 { A, B }
pub enum E2 { A = 1, B = 2 }
pub enum E3 { X, Y }
#[repr(packed(4))]
pub struct Pk(pub u32);
#[repr(C)]
pub struct C(pub u32);
#[repr(C, packed(4))]
pub struct CPk(pub u32);
#[rustc_pass_indirectly_in_non_rustic_abis]
pub struct PI(pub u32);
#[lang = "blaze_test_lang"]
pub struct L(pub u32);

pub type RefP = &'static P;
pub type RefQ = &'static Q;
pub type RefS = &'static S;
pub type RefR = &'static R;
pub type RefD = &'static D;
pub type OwnP = P;
pub type OwnQ = Q;
pub type OwnR = R;
pub type OwnD = D;
pub type OwnS = S;
pub type OwnD2 = D2;
pub type OwnW = W;
pub type OwnUP = UP;
pub type OwnE1 = E1;
pub type OwnE2 = E2;
pub type OwnE3 = E3;
pub type OwnPk = Pk;
pub type OwnC = C;
pub type OwnCPk = CPk;
pub type OwnPI = PI;
pub type OwnL = L;
pub type U64 = u64;
pub type Usize = usize;
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

pub trait Nk { const N: u32; }
impl Nk for &'static P { const N: u32 = 1; }
impl Nk for &'static Q { const N: u32 = 2; }
impl Nk for &'static S { const N: u32 = 1; }
#[inline(always)]
pub fn n_ref<T: Nk>() -> &'static u32 { &T::N }
pub fn read_n<T: Nk>() -> u32 { *n_ref::<T>() }

pub fn __blaze_cache_code<F>(_: &F) -> u32 { loop {} }
pub fn cached<T>(x: T) -> u32 { let body = move || { let _held = x; }; __blaze_cache_code(&body) }
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
            ModuleDefId::TypeAliasId(it)
                if TypeAliasSignature::of(db, it).name.as_str() == name =>
            {
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

/// A generic argument: a type by the alias that names it, that type with its free regions
/// erased, an erased lifetime (both as the code generator's roots have them), or a `usize`
/// constant.
#[derive(Clone, Copy)]
enum Arg {
    Ty(&'static str),
    TyErased(&'static str),
    Lifetime,
    Usize(u128),
}

fn instance<'db>(db: &'db TestDB, func: &str, args: &[Arg]) -> Instance {
    let krate = fixture::test_crate(db);
    let interner = DbInterner::new_with(db, krate);
    let aliased = |name| db.ty(TyDefId::TypeAliasId(alias(db, name))).skip_binder();
    let args: Vec<GenericArg<'db>> = args
        .iter()
        .map(|arg| match *arg {
            Arg::Ty(name) => aliased(name).into(),
            Arg::TyErased(name) => {
                rustc_type_ir::fold_regions(interner, aliased(name), |region, _| {
                    match region.kind() {
                        RegionKind::ReBound(..) => region,
                        _ => Region::new_erased(interner),
                    }
                })
                .into()
            }
            Arg::Lifetime => Region::new_erased(interner).into(),
            Arg::Usize(value) => usize_const(db, Some(value), krate).into(),
        })
        .collect();
    Instance {
        subject: Subject::Body(Body::Fn(function(db, func))),
        args: GenericArgs::new_from_slice(&args).store(),
    }
}

fn env(db: &TestDB) -> hir_ty::traits::StoredParamEnvAndCrate {
    ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: fixture::test_crate(db) }.store()
}

fn key_at(
    db: &TestDB,
    options: Compiled,
    func: &str,
    args: &[Arg],
) -> Result<CompileIdentity, Unshared> {
    blaze_query::db::attach(db, || {
        let instance = instance(db, func, args);
        instance_identity(db, &instance, &env(db), options).clone()
    })
}

#[track_caller]
fn key(db: &TestDB, func: &str, args: &[Arg]) -> CompileIdentity {
    key_at(db, RUNTIME, func, args)
        .unwrap_or_else(|why| panic!("`{func}` has no compile identity: {why}"))
}

#[track_caller]
fn same(db: &TestDB, func: &str, a: Arg, b: Arg) {
    assert_eq!(key(db, func, &[a]), key(db, func, &[b]), "`{func}` shares");
}

#[track_caller]
fn apart(db: &TestDB, func: &str, a: Arg, b: Arg) {
    assert_ne!(key(db, func, &[a]), key(db, func, &[b]), "`{func}` must not share");
}

use Arg::{Lifetime, Ty, TyErased, Usize};

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
        .flat_map(|it| [it, MirOptions { heap_to_stack: true, ..it }])
        .collect();
    let all = OptionsSet::of(&every);
    assert_eq!(all.iter().count(), 48, "each of the forty-eight options has its own bit");
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

/// Values of two types of one shape are moved, copied, sized and returned by the same code:
/// `P`, `Q` and `S` are each a struct of one `u32`, `E1` and `E3` each an enum of two unit
/// variants numbered alike, and `usize` is `u64` on a 64-bit target.
#[test]
fn values_of_one_shape_share() {
    let db = fixture();
    same(&db, "id", Ty("OwnP"), Ty("OwnQ"));
    same(&db, "outer", Ty("OwnP"), Ty("OwnS"));
    same(&db, "size", Ty("OwnP"), Ty("OwnQ"));
    same(&db, "nd", Ty("OwnP"), Ty("OwnQ"));
    same(&db, "with_closure", Ty("OwnP"), Ty("OwnQ"));
    same(&db, "id", Ty("OwnE1"), Ty("OwnE3"));
    #[cfg(target_pointer_width = "64")]
    same(&db, "id", Ty("U64"), Ty("Usize"));
}

/// A shape is more than a size: signedness, a struct around a field, `repr(transparent)`, a
/// union and an enum's discriminants each tell values apart.
#[test]
fn what_a_shape_holds_tells_values_apart() {
    let db = fixture();
    apart(&db, "id", Ty("U32"), Ty("I32"));
    apart(&db, "id", Ty("OwnP"), Ty("U32"));
    apart(&db, "id", Ty("OwnP"), Ty("OwnW"));
    apart(&db, "id", Ty("OwnP"), Ty("OwnUP"));
    apart(&db, "id", Ty("OwnE1"), Ty("OwnE2"));
    apart(&db, "id", Ty("OwnP"), Ty("OwnR"));
}

/// Each of these is a struct of one `u32` with `P`'s layout, told apart from `P` only by
/// what the code generator reads off the definition: `repr(packed)` (unaligned field access),
/// C's indirect passing and a lang item by `IDENTIFYING`, and `repr(C)`. `C` and `CPk`
/// differ only in `pack`, so that case fails if `write_shape` drops the pack bytes.
#[test]
fn what_the_code_generator_reads_off_a_definition_tells_values_apart() {
    let db = fixture();
    apart(&db, "id", Ty("OwnP"), Ty("OwnPk"));
    apart(&db, "id", Ty("OwnC"), Ty("OwnCPk"));
    apart(&db, "id", Ty("OwnP"), Ty("OwnPI"));
    apart(&db, "id", Ty("OwnP"), Ty("OwnL"));
    apart(&db, "id", Ty("OwnP"), Ty("OwnC"));
}

/// What tells two types of one shape apart is read where the body uses it, never through the
/// shape: `TypeId` and `type_name` by identity, a method by its callee, drop glue by its
/// type, and a code the code generator writes for a closure (`__blaze_cache_code`) by the
/// closure's full type.
#[test]
fn what_tells_one_shape_apart_is_read_where_it_is_used() {
    let db = fixture();
    apart(&db, "tid", Ty("OwnP"), Ty("OwnQ"));
    apart(&db, "name", Ty("OwnP"), Ty("OwnQ"));
    apart(&db, "call", Ty("OwnP"), Ty("OwnQ"));
    apart(&db, "erase", Ty("OwnP"), Ty("OwnQ"));
    apart(&db, "hold", Ty("OwnD"), Ty("OwnD2"));
    apart(&db, "cached", Ty("OwnP"), Ty("OwnQ"));
}

#[test]
fn a_callee_is_its_own_instance_identity() {
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

/// A promoted constant of an inlined callee is evaluated with the callee's arguments filled
/// from the instance's, as the code generator evaluates it (`promoted_constants`): `n_ref`'s
/// `&T::N`, inlined into `read_n`, tells `T`s apart by `N`'s value, never by the shape of `T`.
#[test]
fn an_inlined_callees_promoted_constant_is_its_value() {
    let db = fixture();
    blaze_query::db::attach(&db, || {
        let owner = MirOwner::Body(InferBodyId::from(function(&db, "read_n")));
        let body = db
            .mir(owner, MirStage::compiled(owner, LANG))
            .unwrap_or_else(|error| panic!("`read_n` does not lower: {error:?}"));
        assert!(
            !body.inlined.promoted.is_empty(),
            "`n_ref`'s `&T::N` is inlined as a promoted constant"
        );
        assert!(body.promoted_locals.is_empty(), "`read_n` promotes nothing of its own");
    });
    let at = |ty| {
        key_at(&db, INLINED, "read_n", &[Ty(ty)])
            .unwrap_or_else(|why| panic!("`read_n::<{ty}>`: {why}"))
    };
    assert_ne!(at("RefP"), at("RefQ"), "`N` is 1 for one and 2 for the other");
    assert_eq!(at("RefP"), at("RefS"), "`N` is 1 for both");
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
    blaze_query::db::attach(&db, || {
        let owner = DefWithBodyId::from(function(&db, "two"));
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(owner));
        let keys: Vec<CompileIdentity> = HirBody::of(&db, owner)
            .exprs()
            .filter(|(_, expr)| matches!(expr, Expr::Closure { .. }))
            .map(|(expr, _)| {
                let args = GenericArgs::new_from_slice(&[infer.expr_ty(expr).into()]).store();
                let instance =
                    Instance { subject: Subject::Body(Body::Fn(function(&db, "apply"))), args };
                instance_identity(&db, &instance, &env(&db), RUNTIME)
                    .clone()
                    .unwrap_or_else(|why| panic!("{why}"))
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
    let got = blaze_query::db::attach(&db, || {
        let args = instance(&db, "id", &[Ty("OwnD")]).args;
        let shim = Instance { subject: Subject::Shim(Shim::DropGlue), args };
        instance_identity(&db, &shim, &env(&db), RUNTIME).clone()
    });
    assert_eq!(got, Err(Unshared::Shim("drop glue")));
}

#[test]
fn no_level_is_an_error_and_every_level_is_keyed() {
    let db = fixture();
    let none = Compiled { lang: OptionsSet::EMPTY, other: OptionsSet::EMPTY };
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
    let pool = [
        "RefP", "RefQ", "RefS", "RefR", "RefD", "OwnP", "OwnQ", "OwnS", "OwnD", "OwnD2", "OwnW",
        "OwnUP", "OwnE1", "OwnE2", "OwnE3", "OwnR", "U32", "I32", "U64", "Usize",
    ];
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
                let ((raw_a, named_a), (raw_b, named_b)) =
                    (printed(&db, body, a), printed(&db, body, b));
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

/// A key asked for again is one memo hit, and a hit allocates nothing. The root's regions are
/// erased, as the code generator's are: a root naming `'static` is erased on each ask
/// (`instance_identity`), which folds it with a memo.
#[test]
fn a_key_hit_allocates_nothing() {
    let db = fixture();
    blaze_query::db::attach(&db, || {
        let instance = instance(&db, "outer", &[TyErased("RefP")]);
        let env = env(&db);
        instance_identity(&db, &instance, &env, RUNTIME).as_ref().expect("a key");
        let (hit, used) =
            blaze_alloc_count::measure(|| instance_identity(&db, &instance, &env, RUNTIME).is_ok());
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
    blaze_query::db::attach(&db, || {
        for body in bodies {
            let owner = hir_ty::InferBodyId::DefWithBodyId(function(&db, body).into());
            let generic = db.runtime_mir_body(owner).expect("a body");
            let sites =
                crate::instance::sites(&db, Subject::Body(Body::Fn(function(&db, body))), OFF)
                    .as_ref()
                    .unwrap_or_else(|why| panic!("`{body}` has no sites: {why}"));
            for arg in pool {
                let instance = instance(&db, body, &[Ty(arg)]);
                let whole =
                    monomorphized(&db, generic, &instance.args, &env(&db)).expect("monomorphized");
                let filled =
                    monomorphized_args(&db, sites.filled.as_ref(), &instance.args, &env(&db))
                        .expect("the sites filled");
                let locals = filled.as_slice();
                assert!(locals.len() >= whole.locals.len(), "`{body}`: every local is a site");
                for (at, (_, local)) in whole.locals.iter().enumerate() {
                    assert_eq!(
                        locals[at].ty(),
                        Some(local.ty.as_ref()),
                        "`{body}` of `{arg}`, local {at}"
                    );
                }
            }
        }
    });
}

/// The monomorphized runtime body of `func` with the alias `arg`, printed as it is and with the
/// name of the type `arg` points to or is replaced by `_`.
fn printed(db: &TestDB, func: &str, arg: &'static str) -> (String, String) {
    blaze_query::db::attach(db, || {
        let instance = instance(db, func, &[Ty(arg)]);
        let owner = hir_ty::InferBodyId::DefWithBodyId(function(db, func).into());
        let generic = db.runtime_mir_body(owner).expect("a body");
        let body = monomorphized(db, generic, &instance.args, &env(db)).expect("monomorphized");
        let named = match arg {
            "U32" => "u32",
            "I32" => "i32",
            "U64" => "u64",
            "Usize" => "usize",
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

/// A crate of `PROGRAM`'s minicore and `source`.
fn crate_of(source: &str) -> TestDB {
    fixture::db(&format!("//- minicore: {MINICORE}, option\n{source}\n")).into_db()
}

/// `alias` and `alias{1..=levels}` type aliases, each `wrap` of the one before: `alias0` is `base`.
fn chain(alias: &str, levels: u32, base: &str, wrap: impl Fn(&str) -> String) -> String {
    let mut out = format!("pub type {alias}0 = {base};\n");
    for level in 1..=levels {
        out.push_str(&format!(
            "pub type {alias}{level} = {};\n",
            wrap(&format!("{alias}{}", level - 1))
        ));
    }
    out
}

/// A body that calls itself with growing arguments names that callee by them, never asking its
/// identity, so the walk ends at once and reads no limit of any crate: rustc's recursion limit is
/// the build's walk's (`blaze_codegen` driver), and a `#![recursion_limit]` changes nothing here.
#[test]
fn a_body_calling_itself_with_growing_arguments_has_an_identity() {
    for attrs in ["", "#![recursion_limit = \"16\"]"] {
        let db =
            crate_of(&format!("{attrs}\npub type U32 = u32;\npub fn f<T>(x: T) {{ f((x,)) }}"));
        let result = key_at(&db, RUNTIME, "f", &[Ty("U32")]);
        assert!(result.is_ok(), "`f::<u32>` with {attrs:?}: {result:?}");
    }
}

/// A chain of instances that grows through two bodies names each deeper callee by its
/// arguments, which reads only each instance's own key: an instance in the middle of the chain
/// asked first, or after the root that reaches it, has one identity, and so has the root.
#[test]
fn a_mid_chain_instance_has_one_identity_whichever_is_asked_first() {
    let source = format!(
        "pub fn f<T>(x: T) {{ g((x,)) }}\npub fn g<T>(x: T) {{ f((x,)) }}\n{}",
        chain("T", 2, "u32", |it| format!("({it},)"))
    );
    let (mid_first, root_first) = (crate_of(&source), crate_of(&source));
    let mid = key_at(&mid_first, RUNTIME, "f", &[Ty("T2")]);
    let root = key_at(&mid_first, RUNTIME, "f", &[Ty("T0")]);
    let root_again = key_at(&root_first, RUNTIME, "f", &[Ty("T0")]);
    let mid_again = key_at(&root_first, RUNTIME, "f", &[Ty("T2")]);
    assert!(mid.is_ok() && root.is_ok(), "{mid:?} {root:?}");
    assert_eq!(mid, mid_again, "the mid-chain instance");
    assert_eq!(root, root_again, "the root");
    assert_ne!(mid, root, "`f::<u32>` and `f::<((u32,),)>` are other code");
}

/// A type nested deeper than compile identity looks, with no recursion (an `Option` 130 deep,
/// as the nightly builds), is compiled for its arguments: `drop::<Deep>` is `TooDeep`, which the
/// code generator builds per instance, and a generic caller of it has an identity, naming it by
/// its arguments (deeper than the caller's, so never asked).
#[test]
fn a_deep_but_not_recursive_drop_builds_and_its_caller_has_an_identity() {
    let source = format!(
        "pub type U32 = u32;\npub fn drop<T>(_x: T) {{}}\n\
         pub fn caller<T>(x: T) {{ drop::<(T, D130)>((x, None)) }}\n{}",
        chain("D", 130, "u8", |it| format!("Option<{it}>"))
    );
    let db = crate_of(&source);
    let deep = key_at(&db, RUNTIME, "drop", &[Ty("D130")]);
    assert!(
        matches!(deep, Err(Unshared::TooDeep { depth: 131, limit: 128 })),
        "`drop::<Option<..130..<u8>>>`: {deep:?}"
    );
    let caller = key_at(&db, RUNTIME, "caller", &[Ty("U32")]);
    assert!(caller.is_ok(), "`caller::<u32>`: {caller:?}");
}

/// How deep [`a_type_with_an_alias_at_the_bottom_normalizes_at_any_depth`] nests.
const ALIAS_DEPTH: u32 = 3000;

/// How deep the types [`a_type_with_an_alias_at_the_bottom_normalizes_at_any_depth`] builds by
/// hand nest: deeper, as the folds it runs on them take less stack per level.
const BUILT_DEPTH: u32 = 100_000;

/// A type nested `ALIAS_DEPTH` deep with an associated type at the bottom normalizes (review 7,
/// C4, rule 20): every fold `deeply_normalize` runs goes down through all the levels to reach the
/// alias (`set_aliases_to_non_rigid`'s `RigidnessFolder`, the `NormalizationFolder`, the
/// resolver), each with one stack guard per level; the first and the last are also run on
/// their own, as `deeply_normalize`'s input has no rigid alias and no variable. Without the
/// guards a release build overflows the test thread's default stack on it. A type 130 deep, as review 7 sketched it,
/// fits that stack unguarded, so it would show nothing. It passes in any profile; only the
/// overflow with a guard taken out needs `--release` to show.
#[test]
fn a_type_with_an_alias_at_the_bottom_normalizes_at_any_depth() {
    let source = format!(
        "pub trait Tr {{ type A; }}\npub struct X;\nimpl Tr for X {{ type A = u8; }}\n{}",
        chain("D", ALIAS_DEPTH, "<X as Tr>::A", |it| format!("Option<{it}>"))
    );
    let db = crate_of(&source);
    blaze_query::db::attach(&db, || {
        let krate = fixture::test_crate(&db);
        let interner = DbInterner::new_with(&db, krate);
        let infcx = hir_ty::next_solver::infer::DbInternerInferExt::infer_ctxt(interner)
            .build(hir_ty::next_solver::TypingMode::Codegen);
        let deep =
            db.ty(TyDefId::TypeAliasId(alias(&db, &format!("D{ALIAS_DEPTH}")))).skip_binder();
        let normalized = hir_ty_share::deeply_normalize(&infcx, ParamEnv::empty(), deep)
            .unwrap_or_else(|why| panic!("`D{ALIAS_DEPTH}` does not normalize: {why}"));
        // The other two folds, each down the same depth: an alias made rigid and back (the
        // `RigidnessFolder` both ways), and a type variable at the bottom resolved (the resolver).
        // Their frames are smaller, so these are built by hand, `BUILT_DEPTH` deep.
        let TyKind::Adt(option, _) = deep.kind() else { panic!("`{deep:?}` is no `Option`") };
        fn built<'db>(
            interner: DbInterner<'db>,
            option: hir_def::AdtId,
            bottom: hir_ty::next_solver::Ty<'db>,
        ) -> hir_ty::next_solver::Ty<'db> {
            (0..BUILT_DEPTH).fold(bottom, |inner, _| {
                hir_ty::next_solver::Ty::new_adt(
                    interner,
                    option,
                    GenericArgs::new_from_slice(&[inner.into()]),
                )
            })
        }
        let wrapped = |bottom| built(interner, option.def_id(), bottom);
        let alias_at_bottom = wrapped(db.ty(TyDefId::TypeAliasId(alias(&db, "D0"))).skip_binder());
        let rigid = rustc_type_ir::set_aliases_to_rigid(interner, alias_at_bottom);
        let back = rustc_type_ir::set_aliases_to_non_rigid(interner, rigid).skip_normalization();
        assert!(back == alias_at_bottom, "made rigid and back");
        let var_at_bottom = wrapped(infcx.next_ty_var(hir_ty::Span::Dummy));
        assert!(
            infcx.deeply_resolve_ignoring_regions(var_at_bottom) == var_at_bottom,
            "an unresolved variable stays"
        );
        let mut levels = 0;
        let mut at = normalized;
        while let TyKind::Adt(_, args) = at.kind() {
            levels += 1;
            at = args.type_at(0);
        }
        assert_eq!(levels, ALIAS_DEPTH, "`{at:?}` at the bottom");
        assert!(
            matches!(at.kind(), TyKind::Uint(rustc_type_ir::UintTy::U8)),
            "`{at:?}` at the bottom"
        );
    });
}

/// How deep [`a_body_naming_a_deep_type_has_an_identity`] nests its types, and by how many
/// levels each of its statements wraps. Less in a debug build, whose frames are several times
/// larger: type checking the body folds each type in folds not guarded yet (the
/// `DeepVariableResolver` of rustc_type_ir, hir-ty's `InferenceFudger`), which fit the default
/// stack 2000 deep only in a release build.
const BODY_DEPTH: u32 = if cfg!(debug_assertions) { 500 } else { 2000 };
const BODY_STEP: u32 = 50;

/// A body whose types nest `BODY_DEPTH` deep has an identity (review 8, rule 20): `caller`'s
/// locals are `Option<..<T::A>..>` up to `BODY_DEPTH` deep, `BODY_STEP` levels more per
/// statement, so no type is written deeper than `BODY_STEP` and the front end never nests far.
/// Its MIR checks each local for `Copy` (the solver canonicalizes the type and instantiates the
/// answer), and monomorphizing it for `instance_identity` folds each local's type down to the
/// bottom, putting `X` in place of `T` and normalizing `<X as Tr>::A`; `D{BODY_DEPTH}`, an alias
/// chain with `<X as Tr>::A` at the bottom, has its form made one query inside another per level
/// (blaze-items' `ty_code`). Each of those is one stack guard per level; without them a release
/// build overflows the test thread's default stack. It passes in any profile; only the overflow
/// with a guard taken out needs `--release` to show.
///
/// `#![recursion_limit = "4096"]` keeps it valid Rust: under the default 128 the pinned rustc
/// rejects it (E0320, drop-check overflow; 2000 is the least limit it takes at 2000 deep, and
/// 500 deep without one gives 25 errors), and the solver's work past an overflowing limit is
/// what made it quadratic. (The pinned rustc needs a 32 MiB stack, `RUST_MIN_STACK`, to build
/// it 2000 deep; its 8 MiB main thread overflows in the solver.)
#[test]
fn a_body_naming_a_deep_type_has_an_identity() {
    let wrapped =
        format!("{}T{}", "Option<".repeat(BODY_STEP as usize), ">".repeat(BODY_STEP as usize));
    let made = format!("{}x{}", "Some(".repeat(BODY_STEP as usize), ")".repeat(BODY_STEP as usize));
    let mut lets = String::from("let a0 = x;\n");
    for step in 1..=BODY_DEPTH / BODY_STEP {
        lets.push_str(&format!("let a{step} = wrap(a{});\n", step - 1));
    }
    let source = format!(
        "#![recursion_limit = \"4096\"]\n\
         pub trait Tr {{ type A; }}\npub struct X;\npub type Xa = X;\nimpl Tr for X {{ type A = u8; }}\n\
         pub fn drop<T>(_x: T) {{}}\npub fn wrap<T>(x: T) -> {wrapped} {{ {made} }}\n\
         pub fn caller<T: Tr>(x: T::A) {{\n{lets}drop::<(D{BODY_DEPTH}, _)>((None, a{}))\n}}\n{}",
        BODY_DEPTH / BODY_STEP,
        chain("D", BODY_DEPTH, "<X as Tr>::A", |it| format!("Option<{it}>"))
    );
    let db = crate_of(&source);
    let caller = key_at(&db, RUNTIME, "caller", &[Ty("Xa")]);
    assert!(caller.is_ok(), "`caller::<X>`: {caller:?}");
}

/// A callee whose identity can't be made for a reason that is no kind of per-instance code (an
/// intrinsic missing from the table) fails its caller with that reason, never a key of its
/// arguments; the caller's error names the callee it went through and the body that calls the
/// intrinsic (rule 19), and says that `above` calls `uses`, never that it calls the intrinsic.
#[test]
fn a_callee_without_an_identity_fails_its_caller() {
    let db = crate_of(
        "pub type U32 = u32;\n#[rustc_intrinsic]\npub fn not_in_the_table<T>() -> u32;\n\
         pub fn uses<T>() -> u32 { not_in_the_table::<T>() }\npub fn above<T>() -> u32 { uses::<T>() }",
    );
    let uses = key_at(&db, RUNTIME, "uses", &[Ty("U32")]);
    let Err(Unshared::UnknownIntrinsic { name, body, through: Through::Own }) = &uses else {
        panic!("`uses`: {uses:?}")
    };
    assert_eq!(name, "not_in_the_table");
    assert!(body.ends_with("uses`"), "the body that calls it: {body}");
    let above = key_at(&db, RUNTIME, "above", &[Ty("U32")]);
    let Err(Unshared::UnknownIntrinsic {
        name: above_name,
        body: above_body,
        through: Through::Calls,
    }) = &above
    else {
        panic!("`above`: {above:?}")
    };
    assert_eq!((above_name, above_body), (name, body), "`above` calls `uses`, which calls it");
    let said = above.as_ref().unwrap_err().to_string();
    assert_eq!(
        said,
        format!("it calls {body}, which calls the unclassified intrinsic `not_in_the_table`")
    );
    let said = uses.as_ref().unwrap_err().to_string();
    assert_eq!(said, format!("{body} calls the unclassified intrinsic `not_in_the_table`"));
}

/// A closure whose own MIR fails to lower names itself in its error, never its owner, whose
/// first failing body may be another one's (review 6, C3): blaze-items' miss names the owner
/// item, so the error is read from the closure's own lowering.
#[test]
fn a_closure_s_lowering_error_is_its_own() {
    use blaze_query::db::AsId as _;
    let db = crate_of(
        "#[rustc_intrinsic]\npub fn not_in_the_table<T>() -> u32;\n\
         pub fn f() -> u32 { let c = || not_in_the_table::<u8>(); c() }",
    );
    let got = blaze_query::db::attach(&db, || {
        let owner = DefWithBodyId::from(function(&db, "f"));
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(owner));
        let (expr, _) = HirBody::of(&db, owner)
            .exprs()
            .find(|(_, expr)| matches!(expr, Expr::Closure { .. }))
            .expect("a closure");
        let TyKind::Closure(id, args) = infer.expr_ty(expr).kind() else {
            panic!("a closure's type")
        };
        let parent = GenericArgs::new_from_slice(args.as_closure().parent_args()).store();
        let instance =
            Instance { subject: Subject::Body(Body::Closure(id.0.as_id())), args: parent };
        instance_identity(&db, &instance, &env(&db), RUNTIME).clone()
    });
    let Err(Unshared::UnknownIntrinsic { name, body, through: Through::Own }) = &got else {
        panic!("the closure: {got:?}")
    };
    assert_eq!(name, "not_in_the_table");
    assert!(
        body.starts_with("a closure in `") && body.ends_with("f`"),
        "names the closure, not its owner: {body}"
    );
}

/// Two bodies with one path (`Foo::m` of `impl Foo<u8>` and of `impl Foo<u16>`) are told apart
/// by what calls what, never by their names (review 7, M2): `top` calls `Foo::<u16, T>::m`,
/// which calls `Foo::<u8, T>::m`, which calls the intrinsic, so `top` reaches it, and never "calls".
#[test]
fn a_caller_two_calls_away_reaches_a_body_of_its_callee_s_path() {
    let db = crate_of(
        "pub type U32 = u32;\n#[rustc_intrinsic]\npub fn not_in_the_table<T>() -> u32;\npub struct Foo<A, T>(A, T);\n\
         impl<T> Foo<u8, T> { pub fn m() -> u32 { not_in_the_table::<T>() } }\n\
         impl<T> Foo<u16, T> { pub fn m() -> u32 { Foo::<u8, T>::m() } }\n\
         pub fn top<T>() -> u32 { Foo::<u16, T>::m() }",
    );
    let got = key_at(&db, RUNTIME, "top", &[Ty("U32")]);
    let Err(Unshared::UnknownIntrinsic { body, through: Through::Reaches(callee), .. }) = &got
    else {
        panic!("`top`: {got:?}")
    };
    assert_eq!(callee, body, "one path for both bodies");
    let said = got.as_ref().unwrap_err().to_string();
    assert!(said.contains("which reaches"), "{said}");
}

/// An owner whose code fails at a closure's call of an intrinsic missing from the table names
/// the closure as the body that calls it, never itself (review 7, C2): blaze-items' miss is the
/// owner's, and passes on which nested body failed.
#[test]
fn an_owner_failing_at_its_closure_names_the_closure() {
    let db = crate_of(
        "#[rustc_intrinsic]\npub fn not_in_the_table<T>() -> u32;\n\
         pub fn f() -> u32 { let c = || not_in_the_table::<u8>(); c() }",
    );
    let got = key_at(&db, RUNTIME, "f", &[]);
    let Err(Unshared::UnknownIntrinsic { name, body, through: Through::Nested }) = &got else {
        panic!("`f`: {got:?}")
    };
    assert_eq!(name, "not_in_the_table");
    assert!(
        body.starts_with("a closure in `") && body.ends_with("f`"),
        "names the closure, not its owner: {body}"
    );
}

/// rust#37311's shape: each call doubles its argument (`(&T, &T)`), so the instance at step k is
/// a type of 2^k parts as a tree and 2k levels deep. The root's identity names its callee by its
/// arguments and ends, and an instance 70 steps in is `TooDeep` before its arguments are spelled
/// (its depth is measured over distinct parts). The middle (steps 20 to 63) is not asked: there
/// the callee's arguments are spelled as a tree (`identity::args`), 2^k parts, until rust#37311's
/// memoized type identity lands.
#[test]
fn a_doubling_chain_s_root_and_its_too_deep_end_answer() {
    let source = format!(
        "pub fn recurse<T>(x: &T) {{ recurse(&(x, x)) }}\n{}",
        chain("A", 70, "u32", |it| format!("(&'static {it}, &'static {it})"))
    );
    let db = crate_of(&source);
    let root = key_at(&db, RUNTIME, "recurse", &[Ty("A0")]);
    assert!(root.is_ok(), "`recurse::<u32>`: {root:?}");
    let deep = key_at(&db, RUNTIME, "recurse", &[Ty("A70")]);
    assert!(matches!(deep, Err(Unshared::TooDeep { depth: 141, .. })), "step 70: {deep:?}");
}

/// A caller of one instance of a cycle has an identity: `Recursive`, the answer of every
/// instance of the cycle whichever is asked first, names the callee by its arguments, so the
/// caller's identity is the same asked before the cycle or after it.
#[test]
fn a_caller_of_a_recursive_callee_has_one_identity_whichever_is_asked_first() {
    let source = "pub type U32 = u32;\n\
        pub fn ping<T>(n: u32) -> u32 { if n == 0 { 0 } else { pong::<T>(n - 1) } }\n\
        pub fn pong<T>(n: u32) -> u32 { if n == 0 { 1 } else { ping::<T>(n - 1) } }\n\
        pub fn calls_ping<T>(n: u32) -> u32 { ping::<T>(n) }";
    let (first, after) = (crate_of(source), crate_of(source));
    let caller_first = key_at(&first, RUNTIME, "calls_ping", &[Ty("U32")]);
    assert_eq!(key_at(&after, RUNTIME, "pong", &[Ty("U32")]), Err(Unshared::Recursive));
    assert_eq!(key_at(&after, RUNTIME, "ping", &[Ty("U32")]), Err(Unshared::Recursive));
    let caller_after = key_at(&after, RUNTIME, "calls_ping", &[Ty("U32")]);
    assert!(caller_first.is_ok(), "{caller_first:?}");
    assert_eq!(caller_first, caller_after, "asked before the cycle and after it");
}

/// A body calling itself with shallower arguments (`<Vec<Vec<A>> as Clone>::clone` calling
/// `<Vec<A> as Clone>::clone`) asks the callee's identity, so two instances that differ only in
/// what no fact reads (`A` and `B`, each a `u32`) share, as one with deeper or new arguments
/// would not.
#[test]
fn shrinking_recursion_through_one_body_shares() {
    let db = crate_of(
        "pub trait Dup { fn dup(&self) -> Self; }\n\
         pub struct W<T>(pub T);\n\
         impl<T: Dup> Dup for W<T> { fn dup(&self) -> Self { W(self.0.dup()) } }\n\
         pub struct Leaf<T>(pub T);\n\
         impl<T: Copy> Dup for Leaf<T> { fn dup(&self) -> Self { Leaf(self.0) } }\n\
         pub struct A(pub u32);\nimpl Clone for A { fn clone(&self) -> A { *self } }\nimpl Copy for A {}\n\
         pub struct B(pub u32);\nimpl Clone for B { fn clone(&self) -> B { *self } }\nimpl Copy for B {}\n\
         pub type WA = W<W<Leaf<A>>>;\npub type WB = W<W<Leaf<B>>>;\n\
         pub fn dup_of<T: Dup>(x: &T) -> T { x.dup() }",
    );
    same(&db, "dup_of", Ty("WA"), Ty("WB"));
}

/// addr2line's `LookupResult`/`MappedLookup`/`LoopingLookup` shape: a dropped field whose type
/// is a field's declared type instantiated with its ADT's arguments (`Load<L::Buf>`) holds a
/// projection inside, which the drop key normalizes all through before it spells it. Two roots
/// whose `Buf` types differ (`Slice` over a `u8`, `Wide` over a `u16`) have two identities.
#[test]
fn a_dropped_field_whose_type_nests_a_projection_has_an_identity() {
    let source = r#"
pub enum Flow<B, C> { Continue(C), Break(B) }
pub enum Res2<T, E> { Ok(T), Err(E) }
pub struct Dwarf<R>(pub R);
pub struct Guard(pub u32);
impl Drop for Guard { fn drop(&mut self) {} }
pub struct Load<R> { pub parent: Option<Dwarf<R>>, pub guard: Guard }
pub trait Reader: Copy {}
#[derive(Clone, Copy)]
pub struct Slice<'a>(pub &'a u8);
impl Reader for Slice<'_> {}

pub enum LookupResult<L: LookupContinuation> {
    Load { load: Load<<L as LookupContinuation>::Buf>, continuation: L },
    Output(<L as LookupContinuation>::Output),
}

pub trait LookupContinuation: Sized {
    type Output;
    type Buf: Reader;
    fn resume(self, input: Option<Dwarf<Self::Buf>>) -> LookupResult<Self>;
}

impl<L: LookupContinuation> LookupResult<L> {
    pub fn skip_all_loads(mut self) -> L::Output {
        loop {
            self = match self {
                LookupResult::Output(t) => return t,
                LookupResult::Load { continuation, .. } => continuation.resume(None),
            };
        }
    }

    fn map<T, F: FnOnce(L::Output) -> T>(self, f: F) -> LookupResult<MappedLookup<T, L, F>> {
        match self {
            LookupResult::Output(t) => LookupResult::Output(f(t)),
            LookupResult::Load { load, continuation } => LookupResult::Load {
                load,
                continuation: MappedLookup { original: continuation, mutator: f, marker: core::marker::PhantomData },
            },
        }
    }
}

pub struct SimpleLookup<T, R, F>
where
    F: FnOnce(Option<Dwarf<R>>) -> T,
    R: Reader,
{
    f: F,
    phantom: core::marker::PhantomData<(T, R)>,
}

impl<T, R, F> LookupContinuation for SimpleLookup<T, R, F>
where
    F: FnOnce(Option<Dwarf<R>>) -> T,
    R: Reader,
{
    type Output = T;
    type Buf = R;
    fn resume(self, v: Option<Dwarf<Self::Buf>>) -> LookupResult<Self> {
        LookupResult::Output((self.f)(v))
    }
}

pub struct MappedLookup<T, L, F>
where
    L: LookupContinuation,
    F: FnOnce(L::Output) -> T,
{
    original: L,
    mutator: F,
    marker: core::marker::PhantomData<T>,
}

impl<T, L, F> LookupContinuation for MappedLookup<T, L, F>
where
    L: LookupContinuation,
    F: FnOnce(L::Output) -> T,
{
    type Output = T;
    type Buf = L::Buf;
    fn resume(self, v: Option<Dwarf<Self::Buf>>) -> LookupResult<Self> {
        match self.original.resume(v) {
            LookupResult::Output(t) => LookupResult::Output((self.mutator)(t)),
            LookupResult::Load { load, continuation } => LookupResult::Load {
                load,
                continuation: MappedLookup { original: continuation, mutator: self.mutator, marker: core::marker::PhantomData },
            },
        }
    }
}

pub struct LoopingLookup<T, L, F>
where
    L: LookupContinuation,
    F: FnMut(L::Output) -> Flow<T, LookupResult<L>>,
{
    continuation: L,
    mutator: F,
    marker: core::marker::PhantomData<T>,
}

impl<T, L, F> LoopingLookup<T, L, F>
where
    L: LookupContinuation,
    F: FnMut(L::Output) -> Flow<T, LookupResult<L>>,
{
    fn new_lookup(mut r: LookupResult<L>, mut mutator: F) -> LookupResult<Self> {
        loop {
            match r {
                LookupResult::Output(l) => match mutator(l) {
                    Flow::Break(t) => return LookupResult::Output(t),
                    Flow::Continue(r2) => r = r2,
                },
                LookupResult::Load { load, continuation } => {
                    return LookupResult::Load { load, continuation: LoopingLookup { continuation, mutator, marker: core::marker::PhantomData } };
                }
            }
        }
    }
}

impl<T, L, F> LookupContinuation for LoopingLookup<T, L, F>
where
    L: LookupContinuation,
    F: FnMut(L::Output) -> Flow<T, LookupResult<L>>,
{
    type Output = T;
    type Buf = L::Buf;
    fn resume(self, v: Option<Dwarf<Self::Buf>>) -> LookupResult<Self> {
        let r = self.continuation.resume(v);
        LoopingLookup::new_lookup(r, self.mutator)
    }
}

pub struct Unit<R: Reader> { data: R, split: bool }

impl<R: Reader> Unit<R> {
    fn dwarf(&self) -> LookupResult<SimpleLookup<Res2<R, ()>, R, impl FnOnce(Option<Dwarf<R>>) -> Res2<R, ()> + '_>> {
        if self.split {
            LookupResult::Load {
                load: Load { parent: Some(Dwarf(self.data)), guard: Guard(0) },
                continuation: SimpleLookup {
                    f: move |dwo: Option<Dwarf<R>>| match dwo { Some(d) => Res2::Ok(d.0), None => Res2::Ok(self.data) },
                    phantom: core::marker::PhantomData,
                },
            }
        } else {
            LookupResult::Output(Res2::Ok(self.data))
        }
    }

    fn find(&self, probe: u64) -> LookupResult<impl LookupContinuation<Output = Res2<u64, ()>, Buf = R> + '_> {
        self.dwarf().map(move |r| match r { Res2::Ok(_) => Res2::Ok(probe), Res2::Err(e) => Res2::Err(e) })
    }
}

pub struct Context<R: Reader> { first: Unit<R>, second: Unit<R> }

impl<R: Reader> Context<R> {
    pub fn find_frames(&self, probe: u64) -> LookupResult<impl LookupContinuation<Output = Res2<u64, ()>, Buf = R> + '_> {
        let mut next = Some(&self.second);
        LoopingLookup::new_lookup(self.first.find(probe), move |r| {
            Flow::Break(match r {
                Res2::Err(e) => Res2::Err(e),
                Res2::Ok(0) => match next {
                    Some(unit) => {
                        next = None;
                        return Flow::Continue(unit.find(probe));
                    }
                    None => Res2::Ok(0),
                },
                Res2::Ok(f) => Res2::Ok(f),
            })
        })
    }
}

pub fn root(byte: &u8) -> u64 {
    let ctx = Context { first: Unit { data: Slice(byte), split: true }, second: Unit { data: Slice(byte), split: false } };
    match ctx.find_frames(7).skip_all_loads() { Res2::Ok(it) => it, Res2::Err(()) => 0 }
}

#[derive(Clone, Copy)]
pub struct Wide<'a>(pub &'a u16);
impl Reader for Wide<'_> {}

pub fn root_wide(word: &u16) -> u64 {
    let ctx = Context { first: Unit { data: Wide(word), split: true }, second: Unit { data: Wide(word), split: false } };
    match ctx.find_frames(7).skip_all_loads() { Res2::Ok(it) => it, Res2::Err(()) => 0 }
}
"#;
    let db =
        fixture::db(&format!("//- minicore: {MINICORE}, phantom_data, option, derive\n{source}\n"))
            .into_db();
    // Each generic instance a root calls (`find_frames::<Slice>`, `skip_all_loads::<{the
    // opaque}>`), as the code generator asks them; their own callees are asked through them.
    blaze_query::db::attach(&db, || {
        let interner = DbInterner::new_with(&db, fixture::test_crate(&db));
        let calls = |root: &str| -> Vec<(String, Instance)> {
            let owner = InferBodyId::DefWithBodyId(function(&db, root).into());
            let generic = db
                .runtime_mir_body(owner)
                .unwrap_or_else(|error| panic!("`{root}` lowers: {error:?}"));
            // As the code generator compiles it: the opaque types revealed, lifetimes erased.
            let empty = GenericArgs::new_from_slice(&[]).store();
            let body = monomorphized(&db, generic, &empty, &env(&db)).expect("monomorphized");
            let mut calls = Vec::new();
            for (_, block) in body.basic_blocks.iter() {
                let Some(TerminatorKind::Call { func, .. }) =
                    block.terminator.as_ref().map(|it| &it.kind)
                else {
                    continue;
                };
                let fn_ty = match &func.kind {
                    OperandKind::Constant { ty, .. } => ty.as_ref(),
                    OperandKind::Allocation { allocation } => allocation.as_ref().ty,
                    _ => continue,
                };
                let TyKind::FnDef(def, args) = fn_ty.kind() else { continue };
                let args =
                    rustc_type_ir::fold_regions(interner, args.skip_binder(), |region, _| {
                        match region.kind() {
                            RegionKind::ReBound(..) => region,
                            _ => Region::new_erased(interner),
                        }
                    });
                let CallableDefId::FunctionId(callee) = def.0 else { continue };
                if args.is_empty() {
                    continue;
                }
                let name = FunctionSignature::of(&db, callee).name.as_str().to_owned();
                calls.push((
                    name,
                    Instance { subject: Subject::Body(Body::Fn(callee)), args: args.store() },
                ));
            }
            calls
        };
        let (narrow, wide) = (calls("root"), calls("root_wide"));
        for (name, instance) in narrow.iter().chain(&wide) {
            for (options_name, options) in
                [("RUNTIME", RUNTIME), ("INLINED", INLINED), ("EVERY_LEVEL", EVERY_LEVEL)]
            {
                let key = instance_identity(&db, instance, &env(&db), options).clone();
                assert!(key.is_ok(), "`{name}` under {options_name}: {key:?}");
            }
        }
        assert!(
            narrow.len() >= 2,
            "`root` calls `find_frames` and `skip_all_loads`: {} generic calls",
            narrow.len()
        );
        let find_frames = |calls: &[(String, Instance)]| {
            let (_, instance) = calls
                .iter()
                .find(|(name, _)| name == "find_frames")
                .expect("a call of `find_frames`");
            instance_identity(&db, instance, &env(&db), RUNTIME).clone().expect("an identity")
        };
        assert_ne!(
            find_frames(&narrow),
            find_frames(&wide),
            "`find_frames` over two `Buf` types is two bodies"
        );
    });
}

/// How many compile identities `ask` makes on this thread, each one execution of the query.
fn identities_made<T>(ask: impl FnOnce() -> T) -> (T, u64) {
    let before = crate::instance::MADE.with(|made| made.get());
    let got = ask();
    (got, crate::instance::MADE.with(|made| made.get()) - before)
}

/// A binary counter through two bodies (`a::<N>` calls `b::<N::Out>`, which calls
/// `a::<<N::Out as Inc>::Out>`) counts at a fixed depth: from a number of 40 digits it would
/// take 2^40 steps to grow a digit. Each callee is a projection's value, no sub-list of its
/// caller's arguments, so it is named by them and the identity is made from one key, with no
/// growth to wait for (rule 18). `NEAR` (three low zeros under 124 ones, 128 deep) is asked
/// first: without the rule it counts 8 steps and then grows past `SHARED_DEPTH`, so this fails
/// on the count rather than hanging on `N0`.
#[test]
fn a_counter_through_two_bodies_answers_from_a_bounded_number_of_keys() {
    let source = format!(
        "pub struct Z;\npub struct O<T>(pub T);\npub struct I<T>(pub T);\n\
         pub trait Inc {{ type Out: Inc; }}\n\
         impl Inc for Z {{ type Out = I<Z>; }}\n\
         impl<T: Inc> Inc for O<T> {{ type Out = I<T>; }}\n\
         impl<T: Inc> Inc for I<T> {{ type Out = O<T::Out>; }}\n\
         pub fn a<N: Inc>() {{ b::<N::Out>() }}\npub fn b<N: Inc>() {{ a::<N::Out>() }}\n\
         pub type NEAR = O<O<O<H124>>>;\n{}{}",
        chain("N", 40, "Z", |it| format!("O<{it}>")),
        chain("H", 124, "Z", |it| format!("I<{it}>")),
    );
    for root in ["NEAR", "N0", "N40"] {
        let db = crate_of(&source);
        let (got, made) = identities_made(|| key_at(&db, RUNTIME, "a", &[Ty(root)]));
        assert!(got.is_ok(), "`a::<{root}>`: {got:?}");
        assert!((1..=2).contains(&made), "`a::<{root}>` made {made} identities");
    }
}

/// A chain that branches as it grows (`f::<T>` calls `g::<(T, u8)>` and `g::<(T, u16)>`, and
/// `g::<T>` calls `f::<T>`) has 2^k instances k steps in: each deeper callee is named by its
/// arguments, so an identity, even of one 40 levels in, is made from at most two keys. `T121`
/// (122 deep) is asked first: without the rule its chain grows to `SHARED_DEPTH` in 7 steps,
/// about 2^7 identities, so this fails on the count rather than hanging on `T0`.
#[test]
fn a_branching_growing_chain_answers_from_a_bounded_number_of_keys() {
    let source = format!(
        "pub fn f<T>() {{ g::<(T, u8)>(); g::<(T, u16)>() }}\npub fn g<T>() {{ f::<T>() }}\n{}",
        chain("T", 121, "u32", |it| format!("({it},)"))
    );
    for (func, root, most) in [("g", "T121", 2), ("f", "T0", 1), ("g", "T0", 2), ("g", "T40", 2)] {
        let db = crate_of(&source);
        let (got, made) = identities_made(|| key_at(&db, RUNTIME, func, &[Ty(root)]));
        assert!(got.is_ok(), "`{func}::<{root}>`: {got:?}");
        assert!(
            (1..=most).contains(&made),
            "`{func}::<{root}>` made {made} identities, past {most}"
        );
    }
}

/// A callee whose arguments are a projection's value holding a constant (`T::A = [u8; 7]`) is
/// named by its arguments, which tell `[u8; 7]` and `[i8; 7]` apart; `[P; 7]` and `[Q; 7]`
/// (each a struct of one `u32`) are apart too, where asking the callee's identity would share
/// them. `[P; 7]` nests deeper than `XP`, so this pins the depth half of `Walk::asks` (round 5's
/// check for a new constant, `7`, also refused it, and is gone since round 6). The P/Q half tells
/// asking from naming only because `P` and `Q` have one fact, which takes the ADT shape facts
/// (`ADT_SHAPE` in `hir_ty_share::fact`); with facts that tell them apart the pair is apart
/// either way, and this half is vacuous.
#[test]
fn a_callee_of_an_associated_array_is_named_by_its_arguments() {
    let db = crate_of(
        "pub struct P(pub u32);\npub struct Q(pub u32);\n\
         pub trait Arr { type A; }\n\
         pub struct X;\npub struct Y;\npub struct XP;\npub struct XQ;\n\
         impl Arr for X { type A = [u8; 7]; }\nimpl Arr for Y { type A = [i8; 7]; }\n\
         impl Arr for XP { type A = [P; 7]; }\nimpl Arr for XQ { type A = [Q; 7]; }\n\
         pub fn g<T>() { let _held: Option<T> = None; }\n\
         pub fn f<T: Arr>() { g::<T::A>() }\n\
         pub type AX = X;\npub type AY = Y;\npub type AXP = XP;\npub type AXQ = XQ;",
    );
    apart(&db, "f", Ty("AX"), Ty("AY"));
    apart(&db, "f", Ty("AXP"), Ty("AXQ"));
}

/// A counter whose digits are separate type parameters (`a::<D1, .., Dk, P>` calls `b` with
/// each digit the value of a projection, the carry rippled through, and `b` calls `a` back),
/// beside a deep argument `P = (Z, O)` that holds every digit's value, counts 2^(k+1) steps at
/// one depth. Under round 5's rule each step was asked, since every argument was one of the
/// instance's arguments' parts (review 5, C1): 2^(k+1) nested identities, a hang at k = 40. A
/// callee as deep as its caller is asked only when it passes on a sub-list of the caller's
/// arguments, which a step of the counter never does, so each root's identity is made from at
/// most two keys. k = 4 goes first, so a looser rule fails on the count (about 32 identities)
/// before k = 20 would run for 2^21. The second root also holds `Z` and `O` as top-level
/// arguments of their own (`a<D1..Dk, X, Y, P>` with `X = Z`, `Y = O`), so every digit of every
/// step is one of the caller's top-level arguments: a rule that asks where each argument is one
/// of the caller's (membership, or a permutation) counts 2^k there and fails at k = 4 (review 6,
/// M1), where the first root is refused at its first step by either.
#[test]
fn a_counter_held_in_separate_parameters_answers_from_a_bounded_number_of_keys() {
    for (digits, top_level) in [(4usize, false), (4, true), (20, false), (20, true)] {
        let params = (1..=digits).map(|i| format!("D{i}: Bit")).collect::<Vec<_>>().join(", ");
        let passed = (1..=digits).map(|i| format!("D{i}")).collect::<Vec<_>>().join(", ");
        // `C{i}` is the carry into digit i + 1: digit 1 always takes one, then each carry is the
        // digit and the carry into it.
        let mut carry = "O".to_owned();
        let mut stepped = Vec::new();
        for i in 1..=digits {
            stepped.push(format!("<D{i} as Bit>::Xor<{carry}>"));
            carry = format!("<D{i} as Bit>::And<{carry}>");
        }
        let stepped = stepped.join(", ");
        let (params, passed, stepped) = match top_level {
            true => {
                (format!("{params}, X, Y"), format!("{passed}, X, Y"), format!("{stepped}, X, Y"))
            }
            false => (params, passed, stepped),
        };
        let source = format!(
            "pub struct Z;\npub struct O;\n\
             pub trait Bit {{ type And<B: Bit>: Bit; type Xor<B: Bit>: Bit; type Not: Bit; }}\n\
             impl Bit for Z {{ type And<B: Bit> = Z; type Xor<B: Bit> = B; type Not = O; }}\n\
             impl Bit for O {{ type And<B: Bit> = B; type Xor<B: Bit> = B::Not; type Not = Z; }}\n\
             pub fn a<{params}, P>() {{ b::<{stepped}, P>() }}\n\
             pub fn b<{params}, P>() {{ a::<{passed}, P>() }}\n\
             pub type ZB = Z;\npub type OB = O;\npub type BALLAST = (Z, O);\n"
        );
        let db = crate_of(&source);
        let mut root = vec![Ty("ZB"); digits];
        if top_level {
            root.extend([Ty("ZB"), Ty("OB")]);
        }
        root.push(Ty("BALLAST"));
        let (got, made) = identities_made(|| key_at(&db, RUNTIME, "a", &root));
        assert!(got.is_ok(), "`a` over {digits} digits (`Z`, `O` top-level: {top_level}): {got:?}");
        assert!(
            (1..=2).contains(&made),
            "`a` over {digits} digits (`Z`, `O` top-level: {top_level}) made {made} identities"
        );
    }
}

/// The permutation π of 8 positions with cycles (1 2 3) and (4 5 6 7 8), as `π(i)` at index
/// `i - 1`: its order is lcm(3, 5) = 15. The first position of each cycle is seeded `'static`.
const PERMUTED: [usize; 8] = [2, 3, 1, 5, 6, 7, 8, 4];
const SEEDED: [usize; 2] = [1, 4];

/// `a` calls `b` with `seeded`, `b` calls `c` with its parameters put through π (`permuted`),
/// and `c` calls `b` with its own: one source for the lifetime and the type variant. `bounds`
/// is each function's where-clause.
fn region_cycle(params: &str, bounds: &str, seeded: &str, permuted: &str) -> String {
    format!(
        "pub type U32 = u32;\npub type R = &'static u8;\n\
         pub fn a<{params}>() {bounds} {{ b::<{seeded}>() }}\n\
         pub fn b<{params}>() {bounds} {{ c::<{permuted}>() }}\n\
         pub fn c<{params}>() {bounds} {{ b::<{params}>() }}\n"
    )
}

fn listed(f: impl Fn(usize) -> String) -> String {
    (1..=PERMUTED.len()).map(f).collect::<Vec<_>>().join(", ")
}

/// Callees that differ only in their lifetimes are one key (C1 of review 6, rules 18 and 4).
/// `b` calls `c` with its 8 lifetimes put through π, and `c` calls `b` back, from `'static`
/// seeded at the head of each cycle. Each lifetime is early-bound by its where-clause (`'l1:
/// 'l1`; an unused lifetime is late-bound, in rustc too, and takes no explicit argument): the keys `b[x]`, `c[πx]`, `b[πx]`, … would be
/// distinct for 15 steps (and for lcm of the cycle lengths in general, 2.2e8 at k = 100), though
/// their type arguments are always `[T]`. With the free regions erased in the callee's key they
/// are `a`, `b` and `c` once each, and `b` and `c` close a cycle.
#[test]
fn callees_differing_only_in_lifetimes_answer_from_a_bounded_number_of_keys() {
    let lifetime = |i: usize| format!("'l{i}");
    let params = format!("{}, T", listed(lifetime));
    let seeded = format!(
        "{}, T",
        listed(|i| if SEEDED.contains(&i) { "'static".to_owned() } else { lifetime(i) })
    );
    let permuted = format!("{}, T", listed(|i| lifetime(PERMUTED[i - 1])));
    let bounds = format!("where {}", listed(|i| format!("{}: {}", lifetime(i), lifetime(i))));
    let db = crate_of(&region_cycle(&params, &bounds, &seeded, &permuted));
    let mut root = vec![Lifetime; PERMUTED.len()];
    root.push(Ty("U32"));
    let (got, made) = identities_made(|| key_at(&db, RUNTIME, "a", &root));
    assert!((1..=3).contains(&made), "`a` over 8 lifetimes made {made} identities ({got:?})");
    assert!(got.is_ok(), "`a` over 8 lifetimes: {got:?}");
}

/// The same cycle through type arguments that are all `&u8` and differ only in their regions:
/// `b::<&'static u8, T2, ..>` and `c::<T2, T3, T1, ..>` are a sub-list of the caller's
/// arguments up to regions, so they are asked, and only erasing the regions in the key makes
/// them one key each.
#[test]
fn callees_differing_only_in_type_arguments_regions_answer_from_a_bounded_number_of_keys() {
    let ty = |i: usize| format!("T{i}");
    let params = listed(ty);
    let seeded = listed(|i| if SEEDED.contains(&i) { "&'static u8".to_owned() } else { ty(i) });
    let permuted = listed(|i| ty(PERMUTED[i - 1]));
    let db = crate_of(&region_cycle(&params, "", &seeded, &permuted));
    let root = vec![TyErased("R"); PERMUTED.len()];
    let (got, made) = identities_made(|| key_at(&db, RUNTIME, "a", &root));
    assert!((1..=3).contains(&made), "`a` over 8 `&u8`s made {made} identities ({got:?})");
    assert!(got.is_ok(), "`a` over 8 `&u8`s: {got:?}");
}

/// The cycle through 8 distinct types (review 7, M1): `a::<P1..P8>` calls `b` with its own
/// arguments, `b` calls `c` with them put through π, and `c` calls `b` back. `b`'s call of `c`
/// is no sub-list of `b`'s arguments in order, so `c` is named by its arguments and never asked:
/// the real rule makes `a` and `b`, 2 identities. A rule that also passed on a permutation of
/// the caller's arguments would ask `c[πx]`, `b[πx]`, … around π's 15 steps, 31 identities,
/// which this test refuses; the counter tests can't tell it apart, as each counter step but one
/// changes how many of each type there are.
#[test]
fn a_permutation_of_the_caller_s_distinct_arguments_is_not_passed_on() {
    let ty = |i: usize| format!("T{i}");
    let params = listed(ty);
    let permuted = listed(|i| ty(PERMUTED[i - 1]));
    let distinct = ["u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64"];
    let aliases: String = distinct
        .iter()
        .enumerate()
        .map(|(i, it)| format!("pub type P{} = {it};\n", i + 1))
        .collect();
    let db = crate_of(&format!("{aliases}{}", region_cycle(&params, "", &params, &permuted)));
    let root: Vec<Arg> =
        ["P1", "P2", "P3", "P4", "P5", "P6", "P7", "P8"].into_iter().map(Ty).collect();
    let (got, made) = identities_made(|| key_at(&db, RUNTIME, "a", &root));
    assert_eq!(made, 2, "`a` over 8 distinct types made {made} identities ({got:?})");
    assert!(got.is_ok(), "`a` over 8 distinct types: {got:?}");
}

/// Const eval's id of a vtable names its type and the `dyn` it is for, as rustc's
/// `GlobalAlloc::VTable` does (rule 21): `One::V` and `Two::V` have one type and equal bytes, each
/// the vtable of `u32` for another trait, and are two values to the code generator (one vtable's
/// slot runs `T1::a`, the other's `T2::b`), so `v::<One>` and `v::<Two>` share no compile
/// identity (`Again::V` is `One::V` again, and shares), and a body reading one has another code
/// id than one reading the other.
#[test]
fn a_vtable_of_one_type_for_two_traits_is_two_values() {
    let db = fixture::db(&format!(
        "//- minicore: {MINICORE}, transmute
{}",
        r#"
pub trait T1 { fn a(&self) -> u32; }
pub trait T2 { fn b(&self) -> u32; }
impl T1 for u32 { fn a(&self) -> u32 { 1 } }
impl T2 for u32 { fn b(&self) -> u32 { 2 } }
pub struct Parts(pub *const (), pub *const ());
pub struct One;
pub struct Two;
pub struct Again;
pub type OneT = One;
pub type TwoT = Two;
pub type AgainT = Again;
pub trait K { const V: Parts; }
impl K for One { const V: Parts = unsafe { core::mem::transmute::<&dyn T1, Parts>(&5u32) }; }
impl K for Two { const V: Parts = unsafe { core::mem::transmute::<&dyn T2, Parts>(&5u32) }; }
impl K for Again { const V: Parts = unsafe { core::mem::transmute::<&dyn T1, Parts>(&5u32) }; }
pub fn v<T: K>() -> Parts { T::V }
"#
    ))
    .into_db();
    same(&db, "v", Ty("OneT"), Ty("AgainT"));
    apart(&db, "v", Ty("OneT"), Ty("TwoT"));
    // A code id names its root's arguments, so it is read off a root with none: `f` reads `P` by
    // projection, whose value's trait is picked in `make`'s body, so `f`'s own code is one text
    // either way (a free constant read by name would not do: its interface is its value, `dyn`
    // types included), and only `Facts::allocation`'s hash of the vtable's `dyn` tells them
    // apart.
    let step = |tr: &str| {
        let db = fixture::db(&format!(
            "//- minicore: {MINICORE}, transmute
             pub trait T1 {{ fn a(&self) -> u32; }}
pub trait T2 {{ fn b(&self) -> u32; }}
             impl T1 for u32 {{ fn a(&self) -> u32 {{ 1 }} }}
impl T2 for u32 {{ fn b(&self) -> u32 {{ 2 }} }}
             pub struct Parts(pub *const (), pub *const ());
             const fn make() -> Parts {{ unsafe {{ core::mem::transmute::<&dyn {tr}, Parts>(&5u32) }} }}
             pub trait Get {{ const P: Parts; }}
             pub struct K;
             impl Get for K {{ const P: Parts = make(); }}
             pub fn f() -> Parts {{ <K as Get>::P }}
"
        ))
        .into_db();
        blaze_query::db::attach(&db, || crate::code_id(&db, &instance(&db, "f", &[]), &env(&db)))
            .unwrap_or_else(|why| panic!("`f` reading the vtable for `{tr}` has no code id: {why}"))
    };
    assert_eq!(step("T1"), step("T1"), "built twice");
    assert_ne!(step("T1"), step("T2"), "the code id names the vtable's `dyn`");
}
