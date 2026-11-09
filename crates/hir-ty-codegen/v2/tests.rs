//! The prepared IR of instances over fixtures: what their shapes share and what the reference
//! walk lists of what the IR builder asks for.

mod db;

use base_db::Db;
use blaze_codegen::{
    Backend, Instance as UnitInstance, Unit,
    backend::{FrontRef, Shape},
    plan::{Mode, OptLevel, Override, Policy, Profile},
    settings::{Coverage, DebugInfo, Panic, Relocation, Sanitize, Sanitizers, Spec, Toolchain},
    template::Refs,
};
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use hir_def::{DefWithBodyId, FunctionId, signatures::FunctionSignature};
use hir_expand::fixture;
use hir_ty::db::HirDatabase;
use hir_ty_mir::mir::MirSpan;

use super::{
    Backends, BuildTime, Databases, HashSource, HirFrontend, HirInstance, Kind,
    allocator::{self, Forward},
    backend::{built, compiled_in},
    build::Build,
    by_id::ById,
    places::{self, Placer, place_requests},
    refs,
};
use crate::InstanceDef;
use db::TestDB;

/// The fixture lives as long as the test process, as the frontend borrows it.
fn frontend(text: &str) -> (&'static TestDB, HirFrontend<'static>) {
    held(fixture::db(text).into_db())
}

fn held(db: TestDB) -> (&'static TestDB, HirFrontend<'static>) {
    held_as(db, &policy(OptLevel::O0))
}

fn held_as(db: TestDB, policy: &Policy) -> (&'static TestDB, HirFrontend<'static>) {
    let db: &'static TestDB = Box::leak(Box::new(db));
    let frontend =
        HirFrontend::new(Databases::new(db), HashSource::ById(ById), BuildTime::default());
    (db, frontend.with_policy(policy))
}

/// The stencil library the process copies from for the fixtures' target, as a toolchain names
/// it: what a unit at opt-level 0 or 1 compiles with.
fn stencils() -> Option<String> {
    let stencils = crate::Target::for_fixtures().stencil()?;
    let library = blaze_stencil::library_id(&stencils).expect("the stencil library");
    Some(blaze_format::hex::encode(&library))
}

/// A build at `level` of members and dependencies, whose units are as [`spec`]'s.
fn policy(level: OptLevel) -> Policy {
    let toolchain =
        Toolchain { llvm: Some("test".to_owned()), stencils: stencils(), ..spec().toolchain };
    Policy {
        toolchain,
        profile: Profile {
            opt_level: level,
            overflow_checks: false,
            debug_assertions: false,
            debuginfo: DebugInfo::None,
            panic: Panic::Unwind,
            packages: Default::default(),
            build_override: Override::default(),
            stencils: false,
        },
        mode: Mode::Output,
        fold: false,
        sanitize: Sanitize::default(),
        pgo: blaze_codegen::settings::Pgo::None,
        proc_macro_bridge: false,
        report: false,
        fuel: false,
        heap_to_stack: false,
    }
}

/// Whether a unit of `spec` MIR-inlines, which its references are walked for.
fn inlines(spec: &Spec) -> bool {
    Build::of(spec).inlines
}

#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    hir_def::test_items::function(db, fixture::test_crate(db), name)
}

fn spec() -> Spec {
    let target = crate::Target::for_fixtures();
    let arch = match target.arch {
        crate::Arch::X86_64 => blaze_codegen::Arch::X86_64,
        crate::Arch::Aarch64 => blaze_codegen::Arch::Aarch64,
        crate::Arch::Wasm32 => blaze_codegen::Arch::Wasm32,
        crate::Arch::Wasm64 => blaze_codegen::Arch::Wasm64,
    };
    let pic =
        crate::isa_for(crate::Target::for_fixtures()).expect("a code generator").flags().is_pic();
    Spec {
        toolchain: Toolchain {
            id: ContentId::default(),
            target: target.triple().to_owned(),
            arch,
            cpu: target.cpu().to_owned(),
            features: Vec::new(),
            cranelift: "0.136".to_owned(),
            llvm: None,
            stencils: None,
            frame_pointers: false,
            relocation: if pic { Relocation::Pic } else { Relocation::Static },
        },
        backend: Backend::Cranelift { speed: false },
        overflow_checks: false,
        debug_assertions: false,
        panic: Panic::Unwind,
        debuginfo: DebugInfo::None,
        fold: false,
        counted: false,
        mir_inline: false,
        pgo: blaze_codegen::settings::Pgo::None,
        sanitize: Sanitize::default(),
        proc_macro_bridge: false,
        report: false,
        fuel: false,
        heap_to_stack: false,
    }
}

/// How a unit at `opt-level = 3` builds its bodies: optimized and MIR-inlined, with
/// [`spec`]'s other settings (no debug assertions, as a release build has).
fn o3() -> Build {
    Build::of(&Spec { backend: Backend::Llvm { opt: 3 }, mir_inline: true, ..spec() })
}

/// What the compile of `instance` under `build` built in, by code: its `Function::inlined`.
fn inlined_by_build(
    db: &Db,
    hashes: &HashSource,
    instance: &HirInstance,
    build: Build,
) -> Vec<CodeHash> {
    let code = built(db, instance, crate::Target::for_fixtures(), false, build)
        .unwrap_or_else(|e| panic!("{instance:?}: {e}"));
    let built_in = match &code {
        super::Built::Ir(pending) => pending.built_in(),
        super::Built::Assembled(..) => &[],
    };
    super::frontend::inlined(db, hashes, instance, built_in).expect("what its code builds in")
}

fn unit(spec: &Spec) -> Unit {
    Unit {
        instance: UnitInstance {
            code: CodeHash(ContentId::default()),
            args: ContentId::default(),
            settings: spec.settings(),
        },
        backend: spec.backend,
        arch: spec.toolchain.arch,
        item: ItemKey(ContentId::default()),
    }
}

/// The instances of `of` that `root` calls, in the order it calls them.
fn called(db: &Db, root: &HirInstance, of: FunctionId) -> Vec<HirInstance> {
    let is_of = |instance: &HirInstance| matches!(&instance.0, Kind::Code(called, _) if called.def == InstanceDef::Fn(of));
    refs::walk(db, root, refs::Walking::plain(false))
        .expect("the walk of the root")
        .into_iter()
        .filter_map(|it| match it {
            FrontRef::Function(instance) if is_of(&instance) => Some(instance),
            _ => None,
        })
        .collect()
}

/// What a backend is given for the references of an instance: one target each.
fn refs_of(db: &Db, (instance, inlined): (&HirInstance, bool)) -> Vec<blaze_codegen::Target> {
    let walked = refs::walk(db, instance, refs::Walking::plain(inlined)).expect("the walk");
    (0..walked.len()).map(|at| blaze_codegen::Target::Extern(format!("ref{at}"))).collect()
}

fn shape(frontend: &HirFrontend, instance: &HirInstance) -> Shape {
    prepared(frontend, &spec(), instance).0
}

fn prepared(
    frontend: &HirFrontend,
    spec: &Spec,
    instance: &HirInstance,
) -> (Shape, super::Prepared) {
    frontend.with(|db| {
        let refs = refs_of(db, (instance, inlines(spec)));
        let refs: &Refs = &refs;
        let walked = (instance, refs::Walking::plain(inlines(spec)));
        Backends::default()
            .prepared(db, &frontend.hashes, &unit(spec), spec, walked, refs)
            .unwrap_or_else(|e| panic!("{instance:?}: {e:?}"))
    })
}

const SIBLINGS: &str = r#"
//- minicore: sized
pub struct A(pub u32);
pub struct B(pub u64);
pub fn inner<T>(x: &T) -> &T { x }
pub fn outer<T>(x: &T) -> &T { inner(x) }
pub fn konst<const N: usize>() -> usize { N }
pub fn roots() {
    outer(&A(1));
    outer(&B(2));
    konst::<1>();
    konst::<2>();
}
"#;

#[test]
fn siblings_over_pointers_have_one_shape() {
    let (db, frontend) = frontend(SIBLINGS);
    let (roots, outer) = (function(&db, "roots"), function(&db, "outer"));
    let root = frontend.root(roots);
    let siblings = frontend.with(|db| called(db, &root, outer));
    let [a, b] = &siblings[..] else { panic!("{siblings:?}") };
    assert_ne!(a, b);
    assert_eq!(shape(&frontend, a), shape(&frontend, b));
}

#[test]
fn different_constants_are_different_shapes() {
    let (db, frontend) = frontend(SIBLINGS);
    let (roots, konst) = (function(&db, "roots"), function(&db, "konst"));
    let root = frontend.root(roots);
    let siblings = frontend.with(|db| called(db, &root, konst));
    let [one, two] = &siblings[..] else { panic!("{siblings:?}") };
    assert_ne!(shape(&frontend, one), shape(&frontend, two));
}

#[test]
fn a_shape_is_the_same_each_time() {
    let (db, frontend) = frontend(SIBLINGS);
    let root = frontend.root(function(&db, "roots"));
    assert_eq!(shape(&frontend, &root), shape(&frontend, &root));
}

const SHARED: &str = r#"
//- minicore: sized
#[lang = "type_id"]
pub struct TypeId { data: [*const (); 2] }
#[rustc_intrinsic]
#[rustc_comptime]
pub fn type_id<T: ?Sized>() -> TypeId;
pub struct A(pub u32);
pub struct B(pub u32);
pub fn id<T>(x: T) -> T { x }
pub fn tid<T: 'static>() -> TypeId { const { type_id::<T>() } }
pub fn roots() -> (TypeId, TypeId) {
    let (a, b) = (A(1), B(2));
    id(&a);
    id(&b);
    (tid::<&'static A>(), tid::<&'static B>())
}
"#;

/// Two instances of one body whose code reads nothing that tells their arguments apart
/// (`id::<&A>`, `id::<&B>`) have one compile identity in their arguments' place, so they are one
/// instance: one key, which the build compiles once, and one shape. `tid::<&A>` and
/// `tid::<&B>` read `type_id`, which tells them apart, and stay two.
#[test]
fn instances_with_one_share_key_are_one() {
    use blaze_codegen::backend::Frontend as _;
    let (db, frontend) = frontend(SHARED);
    // A failure's message formats instances, whose types are the database's.
    blaze_query::db::attach(db, || {
        let policy = policy(OptLevel::O0);
        let root = frontend.root(function(db, "roots"));
        let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
        let key = |instance: &HirInstance| {
            let described = (frontend.describe(instance, &origin))
                .unwrap_or_else(|e| panic!("{instance:?}: {}", e.what));
            let choice =
                policy.choose(&described.origin, described.overflow_checks).expect("the plan");
            let settings = choice.first.settings();
            UnitInstance { code: described.code, args: described.args, settings }.key()
        };
        let of = |name| {
            let func = function(db, name);
            frontend.with(|hir| called(hir, &root, func))
        };
        let ids = of("id");
        let [a, b] = &ids[..] else { panic!("{ids:?}") };
        assert_ne!(a, b);
        assert_eq!(key(a), key(b));
        assert_eq!(shape(&frontend, a), shape(&frontend, b));
        let tids = of("tid");
        let [one, two] = &tids[..] else { panic!("{tids:?}") };
        assert_ne!(key(one), key(two));
    });
}

/// A `std` (a lang crate, so built without overflow checks, as rustc ships it) whose `u8`
/// addition is core's `impl Add for u8` (`core::internal_macros`' `add_impl!`): `#[inline]`
/// and `#[rustc_inherit_overflow_checks]`; and core's `forward_ref_binop!` impl of `u8 + &u8`,
/// `#[inline]`, generic in a lifetime only and keeping no check of its own, which calls it (as
/// `impl Sum<&u8> for u8` and `impl AddAssign<&u8> for u8` do). Beside them, a body that
/// inherits the checks but is `#[inline(never)]`. The program, built with overflow checks
/// (`cfg:overflow_checks`, as a debug build is), adds `255 + 1` through a generic `sum`,
/// through `255 + &1` and through the shared body.
const INHERITED_CHECKS: &str = r#"
//- minicore: sized
//- /std.rs crate:std
#![feature(lang_items, rustc_attrs)]
pub mod panicking {
    #[lang = "panic_bounds_check"]
    #[track_caller]
    pub fn panic_bounds_check(index: usize, len: usize) -> ! { loop {} }
    pub mod panic_const {
        #[track_caller] pub const fn panic_const_add_overflow() -> ! { loop {} }
    }
}
pub mod ops {
    #[lang = "add"]
    pub trait Add<Rhs = Self> {
        type Output;
        fn add(self, rhs: Rhs) -> Self::Output;
    }
    impl Add for u8 {
        type Output = u8;
        #[inline]
        #[rustc_inherit_overflow_checks]
        fn add(self, other: u8) -> u8 { self + other }
    }
    impl<'a> Add<&'a u8> for u8 {
        type Output = u8;
        #[inline]
        fn add(self, other: &'a u8) -> u8 { <u8 as Add>::add(self, *other) }
    }
}
#[inline(never)]
#[rustc_inherit_overflow_checks]
pub fn add_shared(a: u8, b: u8) -> u8 { a + b }
//- /main.rs crate:main deps:std cfg:overflow_checks
use std::ops::Add;
pub fn sum<T: Add<Output = T>>(a: T, b: T) -> T { a + b }
pub fn by_ref(a: u8, b: &u8) -> u8 { a + b }
pub fn roots() -> (u8, u8, u8) {
    (sum(255u8, 1u8), std::add_shared(255u8, 1u8), by_ref(255u8, &1u8))
}
"#;

/// A build whose profile has overflow checks `checked` (a debug build's, or a release
/// build's), as [`policy`] at opt-level 0 otherwise.
fn checks_policy(checked: bool) -> Policy {
    let mut policy = policy(OptLevel::O0);
    policy.profile.overflow_checks = checked;
    policy
}

/// Whether `instance` is a function named `name`.
fn is_named(db: &Db, instance: &HirInstance, name: &str) -> bool {
    matches!(&instance.0, Kind::Code(called, _)
        if matches!(called.def, InstanceDef::Fn(func) if FunctionSignature::of(db, func).name.as_str() == name))
}

/// The functions named `name` that `from` calls, each instance once, in the order called.
fn called_named(db: &Db, from: &HirInstance, name: &str) -> Vec<HirInstance> {
    refs::walk(db, from, refs::Walking::plain(false))
        .expect("the walk")
        .into_iter()
        .filter_map(|it| match it {
            FrontRef::Function(instance) if is_named(db, &instance, name) => Some(instance),
            _ => None,
        })
        .collect()
}

/// Whether the code of `instance` compiled with overflow checks `checked` can panic on an
/// addition that overflows: its IR calls `panicking::panic_const::panic_const_add_overflow`.
/// Its walk lists that panic whatever the setting, and its IR asks for nothing past its walk;
/// the IR asks for the panic exactly when it is built against the walk without the panic and
/// asks for what that has not.
fn panics_on_overflow(frontend: &HirFrontend, instance: &HirInstance, checked: bool) -> bool {
    let build = Build::of(&spec());
    frontend.with(|db| {
        let walked = refs::walk(db, instance, refs::Walking::plain(false)).expect("the walk");
        let placed = |listed: Vec<FrontRef<HirInstance, super::HirStatic>>| {
            let mut code = built(db, instance, crate::Target::for_fixtures(), checked, build)
                .unwrap_or_else(|e| panic!("{instance:?}: {e}"));
            let requests = match &mut code {
                super::Built::Ir(pending) => &mut pending.wants.requests,
                super::Built::Assembled(_, requests) => requests,
            };
            let mut placer = Placer::listed(listed);
            place_requests(db, &HashSource::ById(ById), compiled_in(db, instance), &mut placer, requests)
        };
        let is_panic = |it: &FrontRef<HirInstance, super::HirStatic>| {
            matches!(it, FrontRef::Function(f) if is_named(db, f, "panic_const_add_overflow"))
        };
        assert_eq!(walked.iter().filter(|&it| is_panic(it)).count(), 1, "{instance:?} walks {walked:?}");
        placed(walked.clone()).unwrap_or_else(|e| panic!("{instance:?} asks for what its walk has not: {e}"));
        placed(walked.into_iter().filter(|it| !is_panic(it)).collect()).is_err()
    })
}

/// rustc compiles an `#[inline]` function of another crate in each crate that uses it
/// (`rustc_monomorphize` `should_codegen_locally`), and `#[rustc_inherit_overflow_checks]`
/// makes its kept overflow checks run where that crate's `-C overflow-checks` says
/// (`rustc_codegen_ssa` `codegen_assert_terminator`). So a crate built with overflow checks
/// whose generic `sum::<u8>` adds `255 + 1` through std's `impl Add for u8` panics, and built
/// without it wraps: the impl is compiled under its user's setting, and the two compiles are
/// two keys (rule 21). The `#[inline(never)]` body is compiled once, in std, built without
/// overflow checks, so it wraps whoever calls it, as under rustc.
#[test]
fn an_inline_body_that_inherits_overflow_checks_takes_its_users_setting() {
    use blaze_codegen::backend::Frontend as _;
    let (db, frontend) = held_as(fixture::db(INHERITED_CHECKS).into_db(), &checks_policy(true));
    // A failure's message formats instances, whose types are the database's.
    blaze_query::db::attach(db, || {
        let root = frontend.root(function(db, "roots"));
        let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
        let sums = frontend.with(|hir| called(hir, &root, function(db, "sum")));
        let [sum] = &sums[..] else { panic!("{sums:?}") };
        let adds = frontend.with(|hir| called_named(hir, sum, "add"));
        let [add] = &adds[..] else { panic!("`sum::<u8>` calls {adds:?}") };
        let shared = frontend.with(|hir| called_named(hir, &root, "add_shared"));
        let [shared] = &shared[..] else { panic!("`roots` calls {shared:?}") };
        // The overflow checks each is compiled with, asked for by the program's crate, in a
        // build whose profile has `checked`, and the key of its unit.
        let planned = |instance: &HirInstance, checked: bool| {
            let described = (frontend.describe(instance, &origin))
                .unwrap_or_else(|e| panic!("{instance:?}: {}", e.what));
            let choice = (checks_policy(checked)
                .choose(&described.origin, described.overflow_checks))
            .expect("the plan");
            let settings = choice.first.settings();
            let key = UnitInstance { code: described.code, args: described.args, settings }.key();
            (choice.first.overflow_checks, key)
        };
        let (debug, debug_key) = planned(add, true);
        assert!(
            debug,
            "`<u8 as Add>::add` is compiled under std's overflow checks, not its user's"
        );
        assert!(
            panics_on_overflow(&frontend, add, debug),
            "`sum(255u8, 1u8)` wraps in a debug build"
        );
        let (release, release_key) = planned(add, false);
        assert!(!release && !panics_on_overflow(&frontend, add, release));
        assert_ne!(debug_key, release_key, "one key for the add compiled with and without checks");
        let (once, _) = planned(shared, true);
        assert!(!once, "an `#[inline(never)]` body of std is compiled under std's setting");
        assert!(!panics_on_overflow(&frontend, shared, once));
    });
}

/// rustc compiles every instance its user compiles in the user's crate, so the user's
/// overflow checks reach `<u8 as Add>::add` through `impl Add<&u8> for u8`, which keeps no
/// check of its own and is generic in a lifetime only: `255u8 + &1u8`,
/// `[200u8, 100].iter().sum::<u8>()` and `acc += &v` panic in a debug build. The forwarding
/// impl is compiled for the crate that asks for it, which then asks for the add, as the
/// driver's walk does (a callee's requester is its caller's origin).
#[test]
fn overflow_checks_reach_an_inheriting_body_through_inline_bodies_without_checks() {
    use blaze_codegen::backend::Frontend as _;
    let (db, frontend) = held_as(fixture::db(INHERITED_CHECKS).into_db(), &checks_policy(true));
    blaze_query::db::attach(db, || {
        let root = frontend.root(function(db, "roots"));
        let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
        let by_ref = frontend.with(|hir| called(hir, &root, function(db, "by_ref")));
        let [by_ref] = &by_ref[..] else { panic!("`roots` calls {by_ref:?}") };
        let forwards = frontend.with(|hir| called_named(hir, by_ref, "add"));
        let [forward] = &forwards[..] else { panic!("`by_ref` calls {forwards:?}") };
        let adds = frontend.with(|hir| called_named(hir, forward, "add"));
        let [add] = &adds[..] else { panic!("`<u8 as Add<&u8>>::add` calls {adds:?}") };
        let forwarded = (frontend.describe(forward, &origin))
            .unwrap_or_else(|e| panic!("{forward:?}: {}", e.what));
        assert_eq!(
            forwarded.origin, origin,
            "`impl Add<&u8> for u8` is compiled for std, not its user"
        );
        let described = (frontend.describe(add, &forwarded.origin))
            .unwrap_or_else(|e| panic!("{add:?}: {}", e.what));
        let choice = (checks_policy(true).choose(&described.origin, described.overflow_checks))
            .expect("the plan");
        assert!(choice.first.overflow_checks, "`255u8 + &1u8` wraps in a debug build");
        assert!(panics_on_overflow(&frontend, add, choice.first.overflow_checks));
    });
}

/// An instance its users compile (`#[inline]`: std's `<u8 as Add>::add`) is walked as one
/// whatever asks for it: describing it for a crate whose units inline nothing (the member, at
/// opt-level 0) and for one whose units inline (std, as shipped), and preparing it, which
/// names no requester, list one walk. Were the walk its asker's, a unit of one origin would
/// be prepared against references listed for another and the build refuse it (a unit whose
/// MIR is inlined of references listed for units that inline none, or a reference count
/// other than the walk's), as with a dependency at opt-level 3 and a member at 0.
#[test]
fn an_instance_its_users_compile_is_walked_alike_for_every_asker() {
    use blaze_codegen::{backend::Frontend as _, plan::Origin};
    let policy = checks_policy(true);
    let (db, frontend) = held_as(fixture::db(INHERITED_CHECKS).into_db(), &policy);
    blaze_query::db::attach(db, || {
        let root = frontend.root(function(db, "roots"));
        let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
        let sums = frontend.with(|hir| called(hir, &root, function(db, "sum")));
        let [sum] = &sums[..] else { panic!("{sums:?}") };
        let adds = frontend.with(|hir| called_named(hir, sum, "add"));
        let [add] = &adds[..] else { panic!("`sum::<u8>` calls {adds:?}") };
        let described =
            (frontend.describe(add, &origin)).unwrap_or_else(|e| panic!("{add:?}: {}", e.what));
        let spec =
            (policy.choose(&described.origin, described.overflow_checks)).expect("the plan").first;
        frontend.with(|hir| {
            let walking = frontend.walking(hir, add).expect("the walk");
            for asker in [origin, Origin::Std] {
                let inlined = frontend.inlines_for(hir, add, &asker).expect("the walk");
                assert_eq!(
                    inlined, walking.inlined,
                    "described for {asker:?} and prepared, walked apart"
                );
            }
            let refs: Vec<blaze_codegen::Target> = (0..described.refs.len())
                .map(|at| blaze_codegen::Target::Extern(format!("ref{at}")))
                .collect();
            Backends::default()
                .prepared(hir, &frontend.hashes, &unit(&spec), &spec, (add, walking), &refs)
                .unwrap_or_else(|e| panic!("{add:?} described for the member, prepared: {e:?}"));
        });
    });
}

const SHAPES: &str = r#"
//- minicore: sized, option
pub struct A(pub u32);
pub struct B(pub u32);
pub struct C(pub u64);
#[repr(transparent)]
pub struct W(pub u32);
pub enum E1 { X, Y }
pub enum E3 { P, Q }
pub struct F(pub f32);
pub struct G(pub f32);
pub fn id<T>(x: T) -> T { x }
pub fn swap<T>(a: T, b: T) -> (T, T) { (b, a) }
pub fn wrap<T>(x: T) -> Option<T> { Some(x) }
pub extern "C" fn c<T>(_x: T) {}
pub fn roots() {
    id(A(1)); id(B(2)); id(C(3)); id(W(4)); id(E1::X); id(E3::P); id(F(1.0)); id(G(2.0));
    swap(A(1), A(2)); swap(B(1), B(2)); swap(C(1), C(2)); swap(W(1), W(2));
    swap(E1::X, E1::Y); swap(E3::P, E3::Q); swap(F(1.0), F(2.0)); swap(G(1.0), G(2.0));
    wrap(A(1)); wrap(B(2)); wrap(C(3)); wrap(W(4)); wrap(E1::X); wrap(E3::P); wrap(F(1.0)); wrap(G(2.0));
    c(A(1)); c(B(2)); c(W(4));
}
"#;

/// The property polymorphization rests on (docs/v2/plan/design/polymorph.md): instances whose
/// keys are equal compile to one shape, at every debuginfo level. Values of two types of one
/// shape (`A` and `B`, `E1` and `E3`, `F` and `G`) are one instance of `id`, `swap` and `wrap`
/// where no local is told to a debugger; a `u64`, a `transparent` wrapper and a float are not
/// one with a `u32` struct. Where locals are told (`DebugInfo::tells_locals`) their types are
/// the instance's own, so no two instances share. `c` is an `extern "C"` function, whose
/// arguments are passed as the C ABI reads them (`scalar_leaves`, `classify`, `decays_in_c`):
/// `A` and `B` are one instance of it and the `transparent` `W` is not. Each of the 27
/// instances is prepared on its own at its unit's spec and every pair is compared, so any
/// input a key misses fails here.
#[test]
fn equal_share_keys_compile_to_one_shape() {
    use blaze_codegen::backend::Frontend as _;
    for debuginfo in
        [DebugInfo::None, DebugInfo::LineTablesOnly, DebugInfo::Limited, DebugInfo::Full]
    {
        let mut policy = policy(OptLevel::O0);
        policy.profile.debuginfo = debuginfo;
        let (db, frontend) = held_as(fixture::db(SHAPES).into_db(), &policy);
        blaze_query::db::attach(db, || {
            let root = frontend.root(function(db, "roots"));
            let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
            let keyed = |instance: &HirInstance| {
                let described = (frontend.describe(instance, &origin))
                    .unwrap_or_else(|e| panic!("{debuginfo:?} {instance:?}: {}", e.what));
                let choice =
                    policy.choose(&described.origin, described.overflow_checks).expect("the plan");
                let key = UnitInstance {
                    code: described.code,
                    args: described.args,
                    settings: choice.first.settings(),
                }
                .key();
                (key, prepared(&frontend, &choice.first, instance).0)
            };
            // `roots` asks for each body with `A, B, C, W, E1, E3, F, G`, in that order.
            let expected: &[(usize, usize)] =
                if debuginfo.tells_locals() { &[] } else { &[(0, 1), (4, 5), (6, 7)] };
            let mut all = Vec::new(); // alloc: a test's list
            for name in ["id", "swap", "wrap"] {
                let func = function(db, name);
                let instances = frontend.with(|hir| called(hir, &root, func));
                assert_eq!(instances.len(), 8, "{debuginfo:?} `{name}`: {instances:?}");
                let keyed: Vec<_> = instances
                    .into_iter()
                    .map(|it| {
                        let k = keyed(&it);
                        (it, k)
                    })
                    .collect(); // alloc: a test's list
                let mut shared = Vec::new(); // alloc: a test's list
                for (at, (_, (key_a, _))) in keyed.iter().enumerate() {
                    for (other, (_, (key_b, _))) in keyed.iter().enumerate().skip(at + 1) {
                        if key_a == key_b {
                            shared.push((at, other));
                        }
                    }
                }
                assert_eq!(shared, expected, "{debuginfo:?} `{name}`: the instances of one key");
                all.extend(keyed);
            }
            // `roots` asks for `c` with `A, B, W`, in that order.
            {
                let func = function(db, "c");
                let instances = frontend.with(|hir| called(hir, &root, func));
                assert_eq!(instances.len(), 3, "{debuginfo:?} `c`: {instances:?}");
                let keyed: Vec<_> = instances
                    .into_iter()
                    .map(|it| {
                        let k = keyed(&it);
                        (it, k)
                    })
                    .collect(); // alloc: a test's list
                let shared: Vec<(usize, usize)> = (0..keyed.len()) // alloc: a test's list
                    .flat_map(|at| (at + 1..keyed.len()).map(move |other| (at, other)))
                    .filter(|&(at, other)| keyed[at].1.0 == keyed[other].1.0)
                    .collect();
                let expected: &[(usize, usize)] =
                    if debuginfo.tells_locals() { &[] } else { &[(0, 1)] };
                assert_eq!(shared, expected, "{debuginfo:?} `c`: the instances of one key");
                all.extend(keyed);
            }
            // Equal key => equal shape, across every instance of every body.
            for (at, (a, (key_a, shape_a))) in all.iter().enumerate() {
                for (b, (key_b, shape_b)) in all.iter().skip(at + 1) {
                    if key_a == key_b {
                        assert_eq!(shape_a, shape_b, "{debuginfo:?}: {a:?} and {b:?} share a key");
                    }
                }
            }
        });
    }
}

/// Rule 21 for debug info: a local a debugger is told of carries its own instance's type into
/// the shape and the side table (`side::Variable::ty`), so where the unit tells them
/// (`DebugInfo::Limited` and `Full`) `id::<A>` and `id::<B>`, one compile identity, have two
/// keys; where none is told they stay one key and one shape. Each is prepared at its unit's
/// spec, so a key that is equal where the shape is not fails. Two builds of the blaze repo
/// gave `UnsafeCell<usize>::get` and `UnsafeCell<Align8<*mut ()>>::get` one key and two shapes.
#[test]
fn a_key_covers_the_types_of_the_locals_a_debugger_is_told() {
    use blaze_codegen::backend::Frontend as _;
    for debuginfo in
        [DebugInfo::None, DebugInfo::LineTablesOnly, DebugInfo::Limited, DebugInfo::Full]
    {
        let mut policy = policy(OptLevel::O0);
        policy.profile.debuginfo = debuginfo;
        let (db, frontend) = held_as(fixture::db(SHAPES).into_db(), &policy);
        blaze_query::db::attach(db, || {
            let root = frontend.root(function(db, "roots"));
            let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
            let func = function(db, "id");
            let instances = frontend.with(|hir| called(hir, &root, func));
            // `roots` asks for `id` with `A` first, then `B`, one compile identity.
            let [a, b, ..] = &instances[..] else { panic!("{instances:?}") };
            let keyed = |instance: &HirInstance| {
                let described = (frontend.describe(instance, &origin))
                    .unwrap_or_else(|e| panic!("{instance:?}: {}", e.what));
                let choice =
                    policy.choose(&described.origin, described.overflow_checks).expect("the plan");
                let key = UnitInstance {
                    code: described.code,
                    args: described.args,
                    settings: choice.first.settings(),
                }
                .key();
                (key, prepared(&frontend, &choice.first, instance).0)
            };
            let ((key_a, shape_a), (key_b, shape_b)) = (keyed(a), keyed(b));
            let told = debuginfo.tells_locals();
            assert_eq!(
                shape_a == shape_b,
                !told,
                "{debuginfo:?}: the told locals' types are in the shape"
            );
            assert_eq!(key_a == key_b, !told, "{debuginfo:?}: the key follows the shape");
        });
    }
}

/// Rule 21 across specs: one instance compiled under two specs that differ only in debug info
/// gets either equal keys and equal shapes, or two keys. Which locals the shape tells of is
/// decided at the spec it is prepared at, and the key's settings hold that spec's debug info,
/// so a unit at another level than its origin's first spec can't share a key with a shape of
/// another set of told locals. Every instance of `id`, `swap`, `wrap` and `c` is keyed and
/// prepared under each level, in the order `roots` asks for them.
#[test]
fn one_instance_under_two_debuginfo_levels_shares_a_key_only_with_its_shape() {
    use blaze_codegen::backend::Frontend as _;
    let levels = [DebugInfo::None, DebugInfo::LineTablesOnly, DebugInfo::Limited, DebugInfo::Full];
    let keyed: Vec<Vec<(String, blaze_codegen::InstanceKey, Shape)>> = levels // alloc: a test's list
        .iter()
        .map(|&debuginfo| {
            let mut policy = policy(OptLevel::O0);
            policy.profile.debuginfo = debuginfo;
            let (db, frontend) = held_as(fixture::db(SHAPES).into_db(), &policy);
            blaze_query::db::attach(db, || {
                let root = frontend.root(function(db, "roots"));
                let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
                let mut all = Vec::new(); // alloc: a test's list
                for name in ["id", "swap", "wrap", "c"] {
                    let func = function(db, name);
                    let instances = frontend.with(|hir| called(hir, &root, func));
                    for (at, instance) in instances.iter().enumerate() {
                        let described = (frontend.describe(instance, &origin))
                            .unwrap_or_else(|e| panic!("{debuginfo:?} {instance:?}: {}", e.what));
                        let choice = policy
                            .choose(&described.origin, described.overflow_checks)
                            .expect("the plan");
                        let settings = choice.first.settings();
                        let key =
                            UnitInstance { code: described.code, args: described.args, settings }
                                .key();
                        let shape = prepared(&frontend, &choice.first, instance).0;
                        all.push((format!("`{name}` #{at}"), key, shape)); // alloc: a test's name
                    }
                }
                all
            })
        })
        .collect();
    for (i, a) in keyed.iter().enumerate() {
        for (j, b) in keyed.iter().enumerate().skip(i + 1) {
            assert_eq!(
                a.len(),
                b.len(),
                "{:?} and {:?} ask for as many instances",
                levels[i],
                levels[j]
            );
            for ((name, key_a, shape_a), (_, key_b, shape_b)) in a.iter().zip(b) {
                if key_a == key_b {
                    assert_eq!(
                        shape_a, shape_b,
                        "{name} at {:?} and {:?}: one key, two shapes",
                        levels[i], levels[j]
                    );
                }
            }
        }
    }
}

const FORWARDERS: &str = r#"
//- minicore: sized
//- /std.rs crate:std lang
pub fn one() -> u32 { 1 }
pub fn two() -> u32 { 2 }
pub fn via_one() -> u32 { one() }
pub fn via_two() -> u32 { two() }
//- /main.rs crate:main deps:std
pub fn one() -> u32 { 1 }
pub fn two() -> u32 { 2 }
pub fn via_one() -> u32 { one() }
pub fn via_two() -> u32 { two() }
pub fn roots() -> u32 { std::via_one() + std::via_two() + via_one() + via_two() }
"#;

/// `described` of the function `name` of crate `krate` as `roots` (of crate `main`) asks for
/// it, under `policy`, with the items lane's hashes: its template code, its code and whether
/// it fills its own anchors.
fn described_in(
    text: &str,
    policy: &Policy,
    krate: &str,
    name: &str,
) -> (CodeHash, CodeHash, bool) {
    use base_db::all_crates;
    use blaze_codegen::backend::Frontend as _;
    let db: &'static TestDB = Box::leak(Box::new(fixture::db(text).into_db()));
    let hashes = HashSource::Items(super::ItemHashes::new(Box::new(|_| None)));
    let frontend =
        HirFrontend::new(Databases::new(db), hashes, BuildTime::default()).with_policy(policy);
    let named = |name: &str| {
        all_crates(db)
            .iter()
            .copied()
            .find(|it| {
                it.extra_data(db)
                    .display_name
                    .as_ref()
                    .is_some_and(|it| it.crate_name().as_str() == name)
            })
            .unwrap_or_else(|| panic!("no crate `{name}`"))
    };
    let main = named("main");
    blaze_query::db::attach(db, || {
        let root = frontend.root(hir_def::test_items::function(db, main, "roots"));
        let origin = frontend.with(|hir| frontend.origin(hir, main));
        let func = hir_def::test_items::function(db, named(krate), name);
        let instances = frontend.with(|hir| called(hir, &root, func));
        let [instance] = &instances[..] else { panic!("`{name}`: {instances:?}") };
        let described = (frontend.describe(instance, &origin))
            .unwrap_or_else(|e| panic!("`{name}`: {}", e.what));
        (described.template_code, described.code, described.anchors.is_some())
    })
}

/// The library's bodies are MIR-inlined in every unit, as shipped (`Build::options`), so a
/// body of a `Lang` crate is its callees' bodies too: it is kept under its code, never a
/// template code its callees' interfaces alone make, and fills no anchors of its own. Two
/// forwarders written alike over two callees of one signature are so two keys.
#[test]
fn a_library_body_the_library_inlines_into_is_kept_under_its_code() {
    let policy = policy(OptLevel::O0);
    let one = described_in(FORWARDERS, &policy, "std", "via_one");
    let two = described_in(FORWARDERS, &policy, "std", "via_two");
    assert_eq!((one.0, one.2), (one.1, false), "`via_one` is kept under its code");
    assert_eq!((two.0, two.2), (two.1, false), "`via_two` is kept under its code");
    assert_ne!(one.0, two.0, "two library forwarders over two callees are two template keys");
}

/// A unit that puts names of its own in its code (a report's rows, a PGO record, a
/// sanitizer's descriptors and its skip of `__sanitizer_*`) keeps its bodies under their
/// codes: `Units::names`. The same body under a unit that does none of those fills its own
/// anchors under a template code of its own.
#[test]
fn a_unit_that_names_its_code_keeps_it_under_its_code() {
    let plain = described_in(FORWARDERS, &policy(OptLevel::O0), "main", "via_one");
    assert!(plain.2, "the premise: a member's body at -O0 is held by table");
    assert_ne!(plain.0, plain.1, "under a template code of its own");
    let mut report = policy(OptLevel::O0);
    report.report = true;
    let mut pgo = policy(OptLevel::O0);
    pgo.pgo = blaze_codegen::settings::Pgo::Generate;
    let mut sanitize = policy(OptLevel::O0);
    sanitize.sanitize.sanitizers = blaze_codegen::settings::Sanitizers::ADDRESS;
    for (why, policy) in [("a report", report), ("PGO", pgo), ("a sanitizer", sanitize)] {
        let (template, code, anchored) = described_in(FORWARDERS, &policy, "main", "via_one");
        assert_eq!((template, anchored), (code, false), "under {why}, kept under its code");
    }
}

const TOLD_LOCAL: &str = r#"
//- minicore: sized
//- /main.rs crate:main
pub struct A(pub u32);
pub fn holds() -> u32 { let held = A(1); held.0 }
pub fn roots() -> u32 { holds() }
"#;

/// What a debugger is told of a function's locals' types is in its compiled function's side
/// table, so it is folded into its link code (`with_names`), not its template code alone:
/// renaming the type a local is of is another code where locals are told.
#[test]
fn a_told_locals_type_is_in_its_link_code() {
    let told = |debuginfo: DebugInfo, text: &str| {
        let mut policy = policy(OptLevel::O0);
        policy.profile.debuginfo = debuginfo;
        described_in(text, &policy, "main", "holds")
    };
    let renamed = TOLD_LOCAL.replace("struct A", "struct B").replace("A(1)", "B(1)");
    let (before, after) = (told(DebugInfo::Full, TOLD_LOCAL), told(DebugInfo::Full, &renamed));
    assert_ne!(before.1, after.1, "the told type's new name is a new link code");
    assert_ne!(before.0, after.0, "and a new template code");
    assert_ne!(
        told(DebugInfo::None, TOLD_LOCAL).1,
        before.1,
        "untold, its code holds no told type"
    );
}

/// A function `#[blaze::cache]` made, with `deep` its closure reaches: `{deep}` is its body.
fn cached_fixture(deep: &str) -> String {
    format!(
        r#"
//- minicore: sized, fn
pub struct Code {{ hash: [u64; 2], name: &'static str }}
pub fn __blaze_cache_code<F>(_: &F) -> Code {{ loop {{}} }}
pub fn deep(x: u32) -> u32 {{ {deep} }}
pub fn cached(x: u32) -> u32 {{
    let body = move || deep(x);
    let _code = __blaze_cache_code(&body);
    body()
}}
"#
    )
}

/// A cached function writes the hash of all its closure reaches into its own code (its
/// closure's `hir_ty_identity::code_id`), so an edit of a function the closure reaches, which
/// leaves the cached function's body, its closure's and every interface as they were, still
/// changes the cached function's code, and a comment or formatting there does not.
#[test]
fn a_cached_function_is_new_code_when_what_its_closure_reaches_is() {
    use blaze_codegen::backend::Frontend as _;
    let code = |deep: &str| {
        let (db, frontend) = frontend(&cached_fixture(deep));
        blaze_query::db::attach(db, || {
            let root = frontend.root(function(db, "cached"));
            let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
            let described = (frontend.describe(&root, &origin))
                .unwrap_or_else(|e| panic!("{root:?}: {}", e.what));
            described.code
        })
    };
    assert_eq!(code("x ^ 1"), code("x ^ 1"));
    assert_eq!(code("x ^ 1"), code("/* a comment */ x  ^\n 1"));
    assert_ne!(code("x ^ 1"), code("x ^ 2"));
}

/// A cached function whose closure reads `S`, `held`: a raw pointer to the first of a
/// `[&A, &B]` of `&dyn Tr`, or a `static mut`.
fn cached_reading(held: &str, read: &str) -> String {
    format!(
        r#"
//- minicore: sized, fn, sync, unsize, coerce_unsized, dispatch_from_dyn, slice
pub struct Code {{ hash: [u64; 2], name: &'static str }}
pub fn __blaze_cache_code<F>(_: &F) -> Code {{ loop {{}} }}
pub trait Tr {{ fn f(&self) -> u32; }}
pub struct A(pub u32);
impl Tr for A {{ fn f(&self) -> u32 {{ self.0 }} }}
pub struct B(pub u32, pub u32);
impl Tr for B {{ fn f(&self) -> u32 {{ self.1 }} }}
pub struct P(pub *const &'static (dyn Tr + Sync));
unsafe impl Sync for P {{}}
{held}
fn keep<T>(_: T) {{}}
pub fn cached(x: u32) -> u32 {{
    let body = move || {{ keep({read}); x }};
    let _code = __blaze_cache_code(&body);
    body()
}}
"#
    )
}

/// A cached function whose body reaches code the code generator refuses, and nothing impure, is
/// a build error naming the function, what it reaches that can't be compiled, the refusal and
/// the rule (rule 3), from the code generator and from the frontend both. Writing a zero hash
/// and running the function uncached took another path in its place. `deep` holds const eval's
/// first id (0x0600_0000) as a `usize` with no provenance, under a type that says it points to a
/// function, which the code generator refuses (`refuse_ids_left`). A body that is impure still
/// compiles and runs uncached, also when it reaches `deep` beside: that is the program's doing,
/// and what can't be compiled can add to an impurity, never take it away.
#[test]
fn a_cached_function_whose_body_blaze_cannot_compile_is_an_error() {
    use blaze_codegen::backend::Frontend as _;
    let deep = "fn eight() -> u32 { 8 }\n\
                const C: (fn() -> u32, *const fn() -> u32) =\n\
                    (eight, &0x0600_0000usize as *const usize as *const fn() -> u32);\n\
                pub fn deep() -> u32 { keep(C); 1 }\n\
                static mut M: u32 = 0;";
    let refusal = "it has no provenance, so const eval did not make it a pointer";
    let named = |what: &str| {
        assert!(what.contains("cached`") && what.contains("can't be built"), "{what}");
        assert!(
            what.contains("it reaches `")
                && what.contains("deep`, which can't be compiled by blaze"),
            "{what}"
        );
        assert!(what.contains(refusal) && what.contains("rule 3"), "{what}");
    };
    let refused = cached_reading(deep, "deep()");
    let (db, front) = frontend(&refused);
    blaze_query::db::attach(db, || {
        let cached = function(db, "cached");
        let compiled =
            crate::compiled_root(db, cached).as_ref().map(|_| ()).map_err(ToString::to_string);
        named(&compiled.expect_err("the code generator refuses `cached`"));
        let root = front.root(cached);
        let origin = front.with(|hir| front.origin(hir, fixture::test_crate(&db)));
        named(
            &front
                .describe(&root, &origin)
                .map(|_| ())
                .expect_err("the frontend refuses `cached`")
                .what,
        );
    });
    for read in ["unsafe { M }", "(unsafe { M }, deep())"] {
        let impure = cached_reading(deep, read);
        let (db, _) = frontend(&impure);
        blaze_query::db::attach(db, || {
            let compiled = crate::compiled_root(db, function(db, "cached"));
            if let Err(e) = compiled {
                panic!("an impure cached function reading `{read}` compiles, uncached: {e}");
            }
        });
    }
}

#[test]
fn a_root_takes_function_items_for_its_type_parameters() {
    use blaze_codegen::backend::Frontend as _;
    let src = "pub fn expand<'a, F>(_: &'a u8) {}\npub fn two<F, G>() {}\npub fn sized<const N: usize, F>() {}\npub fn mac() {}\npub fn other() {}\n";
    let (db, frontend) = frontend(src);
    blaze_query::db::attach(db, || {
        let [expand, two, sized, mac, other] =
            ["expand", "two", "sized", "mac", "other"].map(|name| function(db, name));
        let root = |func, items: &[_]| frontend.root_with_fn_items(func, items);
        let with_mac = root(expand, &[mac]).unwrap();
        assert_ne!(with_mac, root(expand, &[other]).unwrap());
        assert_eq!(with_mac, root(expand, &[mac]).unwrap());
        let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
        frontend
            .describe(&with_mac, &origin)
            .unwrap_or_else(|e| panic!("{with_mac:?}: {}", e.what));
        for (func, items, name) in
            [(two, &[mac][..], "two"), (expand, &[], "expand"), (sized, &[mac], "sized")]
        {
            let what = root(func, items).unwrap_err();
            assert!(what.contains(&format!("`{name}`")), "{what}");
        }
    });
}

/// A generic is shared under every options a unit of its origin builds a body with: the
/// first spec's, the LLVM spec a live build tiers it up to, the LLVM tier of a tiered run,
/// and UB checks where its crate has debug assertions. The library's bodies are at level 2,
/// inlined, as shipped. Leaving one out would key two bodies as one under it.
#[test]
fn a_generic_is_shared_under_every_options_its_units_build_with() {
    use hir_ty_mir::mir::{
        MirOptLevel::{Debug, Release},
        MirOptions,
    };
    let (db, frontend) = frontend(SHARED);
    let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
    let options = |policy: &Policy| {
        let build_time = BuildTime::default();
        let frontend = HirFrontend::new(Databases::new(db), HashSource::ById(ById), build_time)
            .with_policy(policy);
        frontend.body_options(&origin).expect("the options")
    };
    let at = |level, ub_checks, inline| MirOptions {
        level,
        ub_checks,
        lifetime_markers: true,
        inline,
        heap_to_stack: false,
        world: None,
    };
    let o0 = options(&policy(OptLevel::O0));
    assert!(o0.other.contains(at(Debug, false, false)), "{o0:?}");
    assert!(!o0.other.contains(at(Release, false, false)), "{o0:?}");
    assert!(o0.lang.contains(at(Release, false, true)), "{o0:?}");
    let live = options(&Policy { mode: Mode::Live, ..policy(OptLevel::O3) });
    assert!(live.other.contains(at(Debug, false, false)), "{live:?}");
    assert!(live.other.contains(at(Release, false, false)), "{live:?}");
    let mut tiered = Policy { mode: Mode::Tiered, ..policy(OptLevel::O0) };
    tiered.toolchain.stencils = Some("test".to_owned());
    let tiered = options(&tiered);
    assert!(tiered.other.contains(at(Debug, false, false)), "{tiered:?}");
    assert!(tiered.other.contains(at(Release, false, false)), "{tiered:?}");
    let o3 = options(&policy(OptLevel::O3));
    assert!(o3.other.contains(at(Release, false, true)), "{o3:?}");
    let mut checked = policy(OptLevel::O0);
    checked.profile.debug_assertions = true;
    let checked = options(&checked);
    assert!(checked.other.contains(at(Debug, true, false)), "{checked:?}");
    let unplanned =
        HirFrontend::new(Databases::new(db), HashSource::ById(ById), BuildTime::default());
    assert!(unplanned.body_options(&origin).is_err_and(|e| e.contains("with_policy")));
}

/// A warm unit's shape takes nothing of the global allocator: its bytes are the thread's
/// buffer, hashed as written, and its tables the frame's. The key is the one the prepare gave.
#[test]
fn a_warm_shape_allocates_nothing() {
    let (db, frontend) = frontend(SIBLINGS);
    let root = frontend.root(function(&db, "roots"));
    let spec = spec();
    let unit = unit(&spec);
    let (shape, prepared) = prepared(&frontend, &spec, &root);
    let super::Prepared::Cranelift(function) = &prepared else { panic!("a Cranelift unit") };
    let of = || {
        blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            super::shape::of(&unit, function, frame)
        })
    };
    let cold = of().expect("a shape");
    let warm = blaze_alloc_count::assert_at_most(0, "a warm unit's shape", of).expect("a shape");
    assert_eq!(cold, warm);
    assert_eq!(warm, shape);
}

/// Preparing an instance again takes fewer of the global allocator's blocks than the first
/// time, and its frame is used. The rest is Cranelift's IR, the queries' results and the prepared
/// function, which it keeps: no prepare can take none.
#[test]
fn a_warm_prepare_takes_its_tables_from_its_frame() {
    let (db, frontend) = frontend(SIBLINGS);
    let root = frontend.root(function(&db, "roots"));
    let spec = spec();
    let (_, cold) = blaze_alloc_count::measure(|| prepared(&frontend, &spec, &root));
    let before = blaze_mem::tag::snapshot().get(blaze_mem::Tag::Codegen).allocated;
    let ((shape, _), warm) = blaze_alloc_count::measure(|| prepared(&frontend, &spec, &root));
    let after = blaze_mem::tag::snapshot().get(blaze_mem::Tag::Codegen).allocated;
    assert!(warm.allocations < cold.allocations, "warm {warm:?}, cold {cold:?}");
    assert!(after > before, "the prepare's tables went to no frame");
    assert_eq!(shape, prepared(&frontend, &spec, &root).0);
}

const REFERRING: &str = r#"
//- minicore: sized, fn, drop
pub struct Held(pub u32);
impl Drop for Held { fn drop(&mut self) {} }
pub static COUNT: u32 = 3;
extern "C" { pub fn outside(x: u32) -> u32; }
pub fn callee(x: u32) -> u32 { x + COUNT }
pub fn pointer() -> fn(u32) -> u32 { callee }
pub fn generic<T>(x: T) -> T { x }
pub fn roots(x: u32) -> u32 {
    let _held = Held(x);
    let f = pointer();
    let g = |y: u32| y + 1;
    unsafe { outside(f(g(generic(x)))) }
}
"#;

/// The walk lists everything the IR of each instance it reaches asks for: a closed placer
/// over the walk places every request of the IR builder's.
#[test]
fn the_walk_lists_what_the_ir_asks_for() {
    let (db, frontend) = frontend(REFERRING);
    let root = frontend.root(function(&db, "roots"));
    let spec = spec();
    frontend.with(|db| {
        let mut seen = vec![root.clone()];
        let mut at = 0;
        while let Some(instance) = seen.get(at).cloned() {
            at += 1;
            let walked = refs::walk(db, &instance, refs::Walking::plain(false)).expect("the walk");
            assert_eq!(
                walked,
                refs::walk(db, &instance, refs::Walking::plain(false)).expect("the walk again")
            );
            let mut code =
                built(db, &instance, crate::Target::for_fixtures(), false, Build::of(&spec))
                    .unwrap_or_else(|e| panic!("{instance:?}: {e}"));
            let requests = match &mut code {
                super::Built::Ir(pending) => &mut pending.wants.requests,
                super::Built::Assembled(_, requests) => requests,
            };
            let mut placer = Placer::listed(walked.clone());
            place_requests(
                db,
                &HashSource::ById(ById),
                compiled_in(db, &instance),
                &mut placer,
                requests,
            )
            .unwrap_or_else(|e| panic!("{instance:?} asks for what its walk has not: {e}"));
            for it in walked {
                if let FrontRef::Function(next) = it
                    && matches!(next.0, Kind::Code(..))
                    && !seen.contains(&next)
                {
                    seen.push(next);
                }
            }
        }
        assert!(seen.len() > 3, "{seen:?}");
    });
}

/// A placer closed over a list refuses what is not on it: the IR builder asking for more
/// than the walk said is an error, never a reference added on the way.
#[test]
fn a_reference_left_out_of_the_walk_is_an_error() {
    let (db, frontend) = frontend(REFERRING);
    let root = frontend.root(function(&db, "callee"));
    let spec = spec();
    frontend.with(|db| {
        let mut code = built(db, &root, crate::Target::for_fixtures(), false, Build::of(&spec))
            .expect("the IR");
        let super::Built::Ir(pending) = &mut code else { panic!("asm") };
        let mut placer = Placer::listed(Vec::new());
        let krate = compiled_in(db, &root);
        let placed = place_requests(
            db,
            &HashSource::ById(ById),
            krate,
            &mut placer,
            &mut pending.wants.requests,
        );
        assert!(placed.is_err());
    });
}

/// A function holding a value with drop glue across a call: unwinding, the call has a
/// landing pad that drops it; aborting, it has none.
const HOLDS_ACROSS_A_CALL: &str = r#"
//- minicore: sized, drop
pub struct Held(u32);
impl Drop for Held {
    fn drop(&mut self) {}
}
#[inline(never)]
pub fn callee(x: u32) -> u32 { x }
pub fn holds(x: u32) -> u32 {
    let _held = Held(x);
    callee(x)
}
"#;

/// One function built under two units' settings in one database gives two IRs, and neither
/// build reuses the other's: the settings are an argument of the compile and the key of every
/// body it reads (`MirStage::compiled`'s options), never state on the thread, which a fiber
/// that waits in a query leaves behind (rule 27) and a memo would not cover (rule 21).
/// Unwinding, the call has a landing pad that drops the held value; aborting, none. Built
/// unwinding, then aborting, then unwinding again, each is its own settings' IR, and the
/// compile directly under `instance_code` agrees with `built`.
#[test]
fn one_function_under_two_settings_builds_two_irs_in_one_database() {
    let (db, frontend) = frontend(HOLDS_ACROSS_A_CALL);
    let root = frontend.root(function(&db, "holds"));
    let aborting = Build::of(&Spec { panic: Panic::Abort, ..spec() });
    let unwinding = Build::of(&spec());
    frontend.with(|db| {
        let text = |code: super::Built| match code {
            super::Built::Ir(pending) => pending.func.display().to_string(),
            super::Built::Assembled(..) => panic!("`holds` is no assembly"),
        };
        let under = |build: Build| {
            built(db, &root, crate::Target::for_fixtures(), false, build)
                .map(text)
                .unwrap_or_else(|e| panic!("{root:?} under {build:?}: {e}"))
        };
        let unwinds = under(unwinding);
        let aborts = under(aborting);
        let unwinds_again = under(unwinding);
        assert_ne!(
            aborts, unwinds,
            "built after the unwinding IR, the aborting one is its own, not a reuse"
        );
        assert_eq!(
            unwinds_again, unwinds,
            "built after the aborting IR, the unwinding one is its own again"
        );
        let HirInstance(Kind::Code(instance, env)) = &root else {
            panic!("{root:?} is no function")
        };
        let instance = crate::Instance { checked: false, ..*instance };
        let direct = match crate::instance_code(
            db,
            instance,
            *env,
            crate::Detach::Unit,
            Some(aborting),
            &mut None,
        ) {
            Ok(crate::Produced::Pending(pending)) => pending.func.display().to_string(),
            Ok(crate::Produced::Code(_)) => {
                panic!("a unit's compile leaves its IR to the back end")
            }
            Err(e) => panic!("{root:?}: {e}"),
        };
        assert_eq!(direct, aborts, "the compile's own settings, wherever it is called from");
    });
}

const ALWAYS: &str = r#"
//- /dep.rs crate:dep library
pub fn helper(x: u32) -> u32 { x }
#[inline(always)]
pub fn twice(x: u32) -> u32 { helper(x) }
#[inline(always)]
pub fn outer(x: u32) -> u32 { twice(x) }
//- /main.rs crate:main deps:dep
pub fn roots(x: u32) -> u32 { dep::outer(x) }
"#;

/// An `#[inline(always)]` callee of a crate no edit reaches is built in, as LLVM's
/// always-inliner does at -O0: its place is listed as built in, what its body calls is among
/// the caller's references, and the caller's code names the code of each body built in at
/// any depth.
#[test]
fn an_always_inline_callee_of_a_dependency_is_built_in() {
    let (db, frontend) = held(fixture::db(ALWAYS).into_db());
    let root = frontend.root(function(&db, "roots"));
    let spec = spec();
    frontend.with(|db| {
        let walked = refs::walk(db, &root, refs::Walking::plain(false)).expect("the walk");
        let built_in = refs::built_in(db, &root, refs::Walking::plain(false)).expect("the walk");
        let [at, nested] = built_in[..] else { panic!("built in: {built_in:?} of {walked:?}") };
        let FrontRef::Function(outer) = &walked[at as usize] else { panic!("{walked:?}") };
        let FrontRef::Function(twice) = &walked[nested as usize] else { panic!("{walked:?}") };
        let named = |it: &HirInstance, name: &str| match &it.0 {
            Kind::Code(instance, _) => match instance.def {
                InstanceDef::Fn(func) => FunctionSignature::of(db, func).name.as_str() == name,
                _ => false,
            },
            _ => false,
        };
        assert!(named(outer, "outer"), "{outer:?}");
        assert!(named(twice, "twice"), "a callee built in at depth two is listed too: {twice:?}");
        let calls_helper = |it: &FrontRef<HirInstance, _>| {
            matches!(it, FrontRef::Function(callee) if named(callee, "helper"))
        };
        assert!(walked.iter().any(calls_helper), "{walked:?}");
        let mut code = built(db, &root, crate::Target::for_fixtures(), false, Build::of(&spec))
            .unwrap_or_else(|e| panic!("{root:?}: {e}"));
        let super::Built::Ir(pending) = &mut code else { panic!("asm") };
        let inlined = super::frontend::inlined(db, &HashSource::ById(ById), &root, pending.built_in())
            .expect("what is built in");
        assert_eq!(inlined.len(), 2, "outer and twice: {inlined:?}");
        let mut placer = Placer::listed(walked);
        place_requests(db, &HashSource::ById(ById), compiled_in(db, &root), &mut placer, &mut pending.wants.requests)
            .expect("the IR asks only for what the walk lists");
    });
}

const ASKED_BY_THE_IMAGE: &str = r#"
//- minicore: sized, asm
//- /dep.rs crate:dep library
pub fn fence<T>() { unsafe { core::arch::asm!("nop") } }
//- /main.rs crate:main deps:dep
pub fn roots() {
    dep::fence::<u8>();
    unsafe { core::arch::asm!("nop") }
}
"#;

const OVER_A_PATH_CRATE: &str = r#"
//- minicore: sized
//- /sys.rs crate:sys
pub struct Ctx(pub u8);
pub fn make() -> Ctx { Ctx(0) }
//- /safe.rs crate:safe deps:sys library
pub fn open() -> sys::Ctx { sys::make() }
pub static OPEN: fn() -> sys::Ctx = open;
//- /other.rs crate:other library
pub fn plain() -> u8 { 1 }
//- /main.rs crate:main deps:safe,other
pub fn roots() {
    safe::open();
    other::plain();
    (safe::OPEN)();
}
"#;

/// The front end of a build that links a dependency image (`HirFrontend::with_image`), as a
/// dev executable's is, over `db`.
fn held_imaged(db: TestDB) -> (&'static TestDB, HirFrontend<'static>) {
    let (db, frontend) = held_as(db, &policy(OptLevel::O0));
    (db, frontend.with_image(true))
}

/// A registry crate over a path crate (`zstd-safe` over a `[patch]`ed `zstd-sys`) is the
/// workspace's for the dependency image: an edit of the path crate reaches its code, and in
/// the image it would call the path crate's functions, which are the workspace's. Its statics
/// are the workspace's by the same rule, so one pointing at its own function does not refer
/// to the workspace's code from the image. A registry crate over none stays the image's in a
/// build that links one, and is the workspace's in one that links none.
#[test]
fn a_registry_crate_over_a_path_crate_is_the_workspaces() {
    use blaze_codegen::{Owner, backend::Frontend as _};
    let (db, frontend) = held_imaged(fixture::db(OVER_A_PATH_CRATE).into_db());
    let root = frontend.root(function(&db, "roots"));
    let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
    blaze_query::db::attach(db, || {
        let describe = |instance: &HirInstance| {
            frontend
                .describe(instance, &origin)
                .unwrap_or_else(|e| panic!("{instance:?}: {}", e.what))
        };
        let rooted = describe(&root);
        let called = |name: &str| {
            (rooted.refs.iter())
                .find_map(|it| match it {
                    FrontRef::Function(instance @ HirInstance(Kind::Code(called, _))) => {
                        match called.def {
                            InstanceDef::Fn(func)
                                if FunctionSignature::of(db, func).name.as_str() == name =>
                            {
                                Some(instance.clone())
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no `{name}` in {:?}", rooted.refs))
        };
        assert_eq!(
            describe(&called("open")).owner,
            Owner::Workspace,
            "`safe::open`, over the path crate `sys`"
        );
        assert_eq!(
            describe(&called("plain")).owner,
            Owner::Dependency,
            "`other::plain`, over no path crate"
        );
        let table = (rooted.refs.iter())
            .find_map(|it| match it {
                FrontRef::Static(value) => Some(*value),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no `safe::OPEN` in {:?}", rooted.refs));
        let table =
            frontend.describe_static(&table).unwrap_or_else(|e| panic!("`safe::OPEN`: {}", e.what));
        let points_at_open = |it: &FrontRef<HirInstance, super::HirStatic>| match it {
            FrontRef::Function(HirInstance(Kind::Code(called, _))) => {
                matches!(called.def, InstanceDef::Fn(func) if FunctionSignature::of(db, func).name.as_str() == "open")
            }
            _ => false,
        };
        assert!(
            table.refs.iter().any(points_at_open),
            "`safe::OPEN` points at `safe::open`: {:?}",
            table.refs
        );
        assert_eq!(
            table.owner,
            Owner::Workspace,
            "`safe::OPEN`, a static of `safe`, is its crate's as `safe::open` is"
        );
    });
    let (db, frontend) = held_as(fixture::db(OVER_A_PATH_CRATE).into_db(), &policy(OptLevel::O0));
    let root = frontend.root(function(&db, "roots"));
    let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
    blaze_query::db::attach(db, || {
        let rooted =
            frontend.describe(&root, &origin).unwrap_or_else(|e| panic!("{root:?}: {}", e.what));
        for it in &rooted.refs {
            let FrontRef::Function(instance) = it else { continue };
            let described = frontend
                .describe(instance, &origin)
                .unwrap_or_else(|e| panic!("{instance:?}: {}", e.what));
            assert_eq!(
                described.owner,
                Owner::Workspace,
                "a build that links no image: {instance:?}"
            );
        }
    });
}

/// An `asm!` function is its asker's for the dependency image. The one a dependency's generic
/// asks for, compiled as a member asks (core's `__isb::<SY>` for build-time `blaze-spin`), is
/// the image's, or the image would refer to the workspace's; the member's own is the
/// workspace's. Both are one function of one key, which the build gives the image.
#[test]
fn an_asm_function_a_dependencys_generic_asks_for_is_the_images() {
    use blaze_codegen::{Owner, backend::Frontend as _};
    let (db, frontend) = held_imaged(fixture::db(ASKED_BY_THE_IMAGE).into_db());
    let root = frontend.root(function(&db, "roots"));
    let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
    blaze_query::db::attach(db, || {
        let describe = |instance: &HirInstance| {
            frontend
                .describe(instance, &origin)
                .unwrap_or_else(|e| panic!("{instance:?}: {}", e.what))
        };
        let asm = |refs: &[FrontRef<HirInstance, super::HirStatic>]| -> Vec<HirInstance> {
            (refs.iter())
                .filter_map(|it| match it {
                    FrontRef::Function(instance @ HirInstance(Kind::Asm(..))) => {
                        Some(instance.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        let rooted = describe(&root);
        assert_eq!(rooted.owner, Owner::Workspace);
        let fence = (rooted.refs.iter()).find_map(|it| match it {
            FrontRef::Function(instance @ HirInstance(Kind::Code(called, _))) => match called.def {
                InstanceDef::Fn(func)
                    if FunctionSignature::of(db, func).name.as_str() == "fence" =>
                {
                    Some(instance.clone())
                }
                _ => None,
            },
            _ => None,
        });
        let fence = fence.unwrap_or_else(|| panic!("no `fence` in {:?}", rooted.refs));
        let fenced = describe(&fence);
        assert_eq!(fenced.owner, Owner::Dependency, "{fence:?}");
        let [asked] = &asm(&fenced.refs)[..] else { panic!("{:?}", fenced.refs) };
        let [own] = &asm(&rooted.refs)[..] else { panic!("{:?}", rooted.refs) };
        let (asked, own) = (describe(asked), describe(own));
        assert_eq!(asked.owner, Owner::Dependency);
        assert_eq!(own.owner, Owner::Workspace);
        assert_eq!((asked.code, asked.args), (own.code, own.args));
    });
}

const UNEVALUATED: &str = r#"
//- minicore: sized
pub const N: usize = 2 + 2;
pub trait Has { type Held; }
impl Has for u8 { type Held = [u8; N]; }
pub fn takes<T>(x: T) -> T { x }
pub fn roots() {
    takes([0u8; N]);
    takes::<<u8 as Has>::Held>([1u8; 4]);
}
"#;

/// An instance's arguments are as codegen sees them, every alias normalized and every
/// constant evaluated, which is all `Hashes` gives an ID of: `[u8; N]` of a `const N`, and
/// the projection that names it, are `[u8; 4]`.
#[test]
fn an_instances_arguments_are_evaluated() {
    use rustc_type_ir::TypeVisitableExt;
    let (db, frontend) = frontend(UNEVALUATED);
    let (roots, takes) = (function(&db, "roots"), function(&db, "takes"));
    let root = frontend.root(roots);
    let called = frontend.with(|db| called(db, &root, takes));
    assert!(!called.is_empty(), "no instance of `takes`");
    for it in &called {
        let Kind::Code(instance, _) = &it.0 else { panic!("{it:?}") };
        assert!(!instance.args.as_ref().has_aliases(), "{instance:?}");
    }
}

/// A unit's MIR level is rustc's: level 2 for an LLVM unit above -O0 and for the library's
/// crates, else 1; the walk reads level 1, whose bodies ask for all that level 2's do.
#[test]
fn a_units_mir_level_is_rustcs() {
    use hir_ty_mir::mir::MirOptLevel;
    let (db, _) = frontend(SIBLINGS);
    let krate = fixture::test_crate(&db);
    let level = |spec: &Spec| Build::of(spec).options(db, krate, false).level;
    let mut spec = spec();
    assert_eq!(level(&spec), MirOptLevel::Debug);
    spec.backend = Backend::Llvm { opt: 0 };
    assert_eq!(level(&spec), MirOptLevel::Debug);
    spec.backend = Backend::Llvm { opt: 2 };
    assert_eq!(level(&spec), MirOptLevel::Release);
    assert_eq!(Build::WALK.options(db, krate, false).level, MirOptLevel::Debug);
    spec.debug_assertions = true;
    assert!(Build::of(&spec).options(db, krate, false).ub_checks);
}

/// An entry of the allocator shim is built against what its walk lists, the function it
/// calls by symbol, and is another code for another callee.
#[test]
fn an_allocator_entry_calls_what_its_walk_lists() {
    let (db, frontend) = frontend(REFERRING);
    let krate = fixture::test_crate(&db);
    let entry = |to| Forward { symbol: "__rust_alloc", to, params: 2, returns: true, krate };
    let ours = HirInstance(Kind::Allocator(entry(Some("__blaze_global_alloc"))));
    let defaults = entry(Some("__rdl_alloc"));
    assert_ne!(allocator::code(&defaults), allocator::code(&entry(Some("__blaze_global_alloc"))));
    assert!(allocator::walk(&entry(None)).is_empty());
    let spec = spec();
    frontend.with(|db| {
        let walked = refs::walk(db, &ours, refs::Walking::plain(false)).expect("the walk");
        assert_eq!(walked, vec![FrontRef::Extern("__blaze_global_alloc".to_owned())]);
        let mut code = built(db, &ours, crate::Target::for_fixtures(), false, Build::of(&spec))
            .expect("the IR");
        let super::Built::Ir(pending) = &mut code else { panic!("asm") };
        let mut placer = Placer::listed(walked);
        place_requests(
            db,
            &HashSource::ById(ById),
            compiled_in(db, &ours),
            &mut placer,
            &mut pending.wants.requests,
        )
        .expect("what the walk lists");
    });
    // The fixture links no `alloc`: its program has no shim.
    assert!(frontend.allocator_shim(krate).expect("the shim").is_empty());
}

/// What every allocator fixture links: a `std` (a lang crate, as std is) whose `System` only
/// allocates, and which declares `#[global_allocator]` as std's prelude does.
const ALLOCATORS: &str = r#"
//- minicore: sized, allocator_api, add
//- /std.rs crate:std
#![feature(decl_macro, rustc_attrs)]
pub mod alloc {
    // The expansion's paths start at `$crate`, this crate, as std re-exports core's.
    pub use core::alloc::{GlobalAlloc, Layout};
    pub struct System;
    unsafe impl core::alloc::GlobalAlloc for System {
        unsafe fn alloc(&self, _layout: core::alloc::Layout) -> *mut u8 { 0 as *mut u8 }
        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {}
    }
}
#[rustc_builtin_macro]
pub macro global_allocator($item:item) {
    /* compiler built-in */
}
"#;

/// A crate whose global allocator is std's `System`.
const SYSTEM_ALLOCATOR: &str = r#"
use std::global_allocator;
#[global_allocator]
static GLOBAL: std::alloc::System = std::alloc::System;
"#;

/// A crate whose global allocator counts the allocations, which the program can read.
const COUNTING_ALLOCATOR: &str = r#"
use core::alloc::{GlobalAlloc, Layout};
use std::global_allocator;
pub static mut ALLOCATIONS: usize = 0;
pub struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        unsafe { ALLOCATIONS = ALLOCATIONS + 1 };
        0 as *mut u8
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}
#[global_allocator]
static GLOBAL: Counting = Counting;
"#;

/// An allocator entry, written out, that names two statics: no one static is the allocator.
const TWO_STATICS: &str = r#"
use core::alloc::{GlobalAlloc, Layout};
pub struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 { 0 as *mut u8 }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}
static A: Counting = Counting;
static B: Counting = Counting;
#[export_name = "__blaze_global_alloc"]
unsafe extern "C" fn __blaze_global_alloc(size: usize, align: usize) -> *mut u8 {
    let _first = unsafe { GlobalAlloc::alloc(&A, Layout::from_size_align_unchecked(size, align)) };
    unsafe { GlobalAlloc::alloc(&B, Layout::from_size_align_unchecked(size, align)) }
}
"#;

/// Whether the program of [`ALLOCATORS`] and `program` declares a global allocator, and what
/// [`allocator::allocations_unobserved`] says of it.
fn unobserved(program: &str) -> (bool, Result<bool, String>) {
    let db = fixture::db(&format!(
        "{ALLOCATORS}//- /main.rs crate:main deps:std\n{program}\nfn main() {{}}\n"
    ))
    .into_db();
    let krate = fixture::test_crate(&db);
    (crate::declares_global_allocator(&db, krate), allocator::allocations_unobserved(&db, krate))
}

/// A program's allocations are unobserved under std's allocator, none declared or `System`,
/// and observed under any other: one that counts, or one whose static can't be told, which
/// keeps every allocation (`false`) and is no error.
#[test]
fn allocations_are_unobserved_only_under_stds_allocator() {
    assert_eq!(unobserved(""), (false, Ok(true)));
    assert_eq!(unobserved(SYSTEM_ALLOCATOR), (true, Ok(true)));
    assert_eq!(unobserved(COUNTING_ALLOCATOR), (true, Ok(false)));
    assert_eq!(unobserved(TWO_STATICS), (true, Ok(false)));
}

/// Two `#[global_allocator]`s are an error naming both crates in order of name, whichever
/// order the fixture, and so the crate graph, lists them in. `main` names both: rustc links
/// only the dependencies a crate's code names.
#[test]
fn two_allocators_are_named_in_order_of_name() {
    for (first, second) in [("zz_alloc", "aa_alloc"), ("aa_alloc", "zz_alloc")] {
        let text = format!(
            "{ALLOCATORS}//- /{first}.rs crate:{first} deps:std\n{SYSTEM_ALLOCATOR}\
             //- /{second}.rs crate:{second} deps:std\n{SYSTEM_ALLOCATOR}\
             //- /main.rs crate:main deps:std,{first},{second}\n\
             extern crate {first};\nextern crate {second};\nfn main() {{}}\n"
        );
        let db = fixture::db(&text).into_db();
        let krate = fixture::test_crate(&db);
        assert_eq!(
            allocator::allocations_unobserved(&db, krate),
            Err("two `#[global_allocator]`s in one program, in crates `aa_alloc` and `zz_alloc`"
                .to_owned()),
            "{first} listed first"
        );
    }
}

/// The toolchain's panic runtimes as a build gives them: crates of the standard library, each
/// built with the strategy it implements (`panic_abort` always aborts), and a `std` built with
/// `std` (`unwind`, as the sysroot's, or `abort` where the target aborts) that depends on both
/// and names neither. A program's crate also depends on `panic_unwind` directly, as
/// `-Zbuild-std` gives every crate (a `noprelude` extern, blaze-units' `attach_std`). Every
/// crate says its strategy, as a build's cfg does, so no minicore, whose `core` says none.
fn panic_runtimes(std: &str) -> String {
    format!(
        r#"
//- /unwind.rs crate:panic_unwind lang cfg:panic=unwind
#![feature(panic_runtime)]
#![panic_runtime]
//- /abort.rs crate:panic_abort lang cfg:panic=abort
#![feature(panic_runtime)]
#![panic_runtime]
//- /std.rs crate:std deps:panic_unwind,panic_abort cfg:panic={std}
#![feature(needs_panic_runtime)]
#![needs_panic_runtime]
"#
    )
}

/// A panic runtime of the program's own, not the standard library's.
const CUSTOM_RUNTIME: &str = r#"
//- /custom.rs crate:panic_custom cfg:panic=unwind
#![feature(panic_runtime)]
#![panic_runtime]
"#;

/// The panic runtimes the program of `main` links, by name, of the fixture `text` with `main`.
fn runtimes_of(text: &str) -> Result<Vec<String>, String> {
    let db = fixture::db(text).into_db();
    let krate = fixture::test_crate(&db);
    let mut names: Vec<String> = crate::linked_crates(&db, krate)?
        .into_iter()
        .map(|it| it.named(&db).to_string())
        .filter(|it| it.starts_with("panic_"))
        .collect();
    names.sort_unstable();
    Ok(names)
}

/// The panic runtimes the program of `main` links, by name, `main` having `deps` beside `std`
/// and `panic_unwind`, `-C panic=<strategy>` and the code `main`, std built to unwind.
/// `panic_custom` is in the fixture only when `deps` names it: the test crate is the one
/// nothing depends on.
fn linked_runtimes(strategy: &str, deps: &str, main: &str) -> Result<Vec<String>, String> {
    let custom = if deps.contains("panic_custom") { CUSTOM_RUNTIME } else { "" };
    let runtimes = panic_runtimes("unwind");
    runtimes_of(&format!(
        "{runtimes}{custom}//- /main.rs crate:main deps:std,panic_unwind{deps} cfg:panic={strategy}\n{main}\nfn main() {{}}\n"
    ))
}

/// A program links one panic runtime: the one its strategy names, though std and the program's
/// own crate depend on `panic_unwind`, or one it names itself, as rustc's
/// `inject_panic_runtime` and `activate_injected_dep` choose. Both linked defined
/// `__rust_panic_cleanup` twice, and a `catch_unwind` cleaned up `panic_unwind`'s exception
/// with `panic_abort`'s (#1611); `panic_unwind` alone under `abort` made every panic "failed to
/// initiate panic".
#[test]
fn a_program_links_the_one_panic_runtime_its_strategy_names() {
    assert_eq!(linked_runtimes("unwind", "", ""), Ok(vec!["panic_unwind".to_owned()]));
    assert_eq!(
        linked_runtimes("abort", "", ""),
        Ok(vec!["panic_abort".to_owned()]),
        "a dependency on `panic_unwind` the program's code never names is no runtime it names"
    );
    assert_eq!(
        linked_runtimes("abort", ",panic_abort", "extern crate panic_abort;"),
        Ok(vec!["panic_abort".to_owned()]),
        "the runtime the program names, built with its strategy"
    );
    assert_eq!(
        linked_runtimes("unwind", ",panic_custom", "extern crate panic_custom;"),
        Ok(vec!["panic_custom".to_owned()]),
        "a runtime of the program's own replaces the injected one"
    );
    let runtimes = panic_runtimes("unwind");
    let text = format!("{runtimes}//- /main.rs crate:main deps:std,panic_unwind\nfn main() {{}}\n");
    let db = fixture::db(&text).into_db();
    assert_eq!(
        crate::linked_crates(&db, fixture::test_crate(&db)).map(drop),
        Err("crate `main` has a panic strategy that names no panic runtime: its cfg has neither \
             `panic = \"unwind\"` nor `panic = \"abort\"`"
            .to_owned()),
        "no strategy is an error, never a runtime guessed"
    );
}

/// A runtime the program names that is not built with the program's strategy is an error
/// naming the program, the runtime, the crate that named it and both strategies, as rustc's
/// `BadPanicStrategy` refuses it (tests/ui/panic-runtime/want-unwind-got-abort.rs,
/// want-abort-got-unwind.rs).
#[test]
fn a_panic_runtime_of_another_strategy_is_refused_naming_it() {
    assert_eq!(
        linked_runtimes("unwind", ",panic_abort", "extern crate panic_abort;"),
        Err("the program of crate `main` panics by `unwind`, and it links the panic runtime \
             `panic_abort` (named by crate `main`), built to panic by `abort`: a program's panic \
             runtime is built with its panic strategy"
            .to_owned())
    );
    assert_eq!(
        linked_runtimes("abort", "", "extern crate panic_unwind;"),
        Err("the program of crate `main` panics by `abort`, and it links the panic runtime \
             `panic_unwind` (named by crate `main`), built to panic by `unwind`: a program's panic \
             runtime is built with its panic strategy"
            .to_owned())
    );
    // Found apart from the walk, which reads nothing once no ordinary dependency is left: in a
    // body by the file's text, written by a macro at the level of items by the def map.
    let refused = Err("the program of crate `main` panics by `unwind`, and it links the panic runtime \
                       `panic_abort` (named by crate `main`), built to panic by `abort`: a program's \
                       panic runtime is built with its panic strategy"
        .to_owned());
    assert_eq!(
        linked_runtimes("unwind", ",panic_abort", "fn f() { extern crate panic_abort; }"),
        refused,
        "an `extern crate` in a body"
    );
    assert_eq!(
        linked_runtimes(
            "unwind",
            ",panic_abort",
            "macro_rules! m { () => { extern crate panic_abort; } }\nm!();"
        ),
        refused,
        "an `extern crate` a macro writes"
    );
}

/// Two runtimes named are refused naming each and the crate that named it, the one to edit
/// (rustc's `TwoPanicRuntimes`).
#[test]
fn two_panic_runtimes_named_are_refused_naming_who_named_each() {
    let runtimes = panic_runtimes("unwind");
    let text = format!(
        "{runtimes}{CUSTOM_RUNTIME}//- /mid.rs crate:mid deps:panic_custom cfg:panic=unwind\n\
         extern crate panic_custom;\n\
         //- /main.rs crate:main deps:std,panic_unwind,mid cfg:panic=unwind\n\
         extern crate mid;\nextern crate panic_unwind;\nfn main() {{}}\n"
    );
    assert_eq!(
        runtimes_of(&text),
        Err(
            "the program of crate `main` names two panic runtimes, `panic_custom` (named by crate \
             `mid`) and `panic_unwind` (named by crate `main`): a program links one"
                .to_owned()
        )
    );
}

/// A program that aborts links the standard library built to unwind with `panic_abort`, as the
/// toolchain's sysroot has it (an unwinding test of an aborting profile shares that std), and a
/// crate built to abort is refused in a program that unwinds, naming it, as rustc's
/// `RequiredPanicStrategy` refuses it. A std built to abort under an unwinding test was
/// refused as the runtime's strategy instead, or linked unchecked (#1611 round 2, C5).
#[test]
fn crates_built_to_abort_link_only_into_programs_that_abort() {
    // `dep`, built to `dep_panic`, is in the fixture only when `main` depends on it: the test
    // crate is the first nothing depends on, which a `dep` written before `main` would be.
    let program = |std: &str, strategy: &str, deps: &str, dep_panic: &str| {
        let runtimes = panic_runtimes(std);
        let (dep, call) = match deps.contains("dep") {
            true => (
                format!("//- /dep.rs crate:dep cfg:panic={dep_panic}\npub fn f() {{}}\n"),
                "dep::f();",
            ),
            false => (String::new(), ""),
        };
        format!(
            "{runtimes}{dep}//- /main.rs crate:main deps:std,panic_unwind{deps} cfg:panic={strategy}\n\
             fn main() {{ {call} }}\n"
        )
    };
    let main = |std: &str, strategy: &str, deps: &str| program(std, strategy, deps, "abort");
    assert_eq!(runtimes_of(&main("unwind", "abort", "")), Ok(vec!["panic_abort".to_owned()]));
    assert_eq!(runtimes_of(&main("unwind", "abort", ",dep")), Ok(vec!["panic_abort".to_owned()]));
    assert_eq!(
        runtimes_of(&main("unwind", "unwind", ",dep")),
        Err("the program of crate `main` panics by `unwind`, and it links crate `dep`, built to \
             panic by `abort`: a crate built to panic by `abort` is linked only into a program \
             that panics by `abort`"
            .to_owned())
    );
    assert_eq!(
        runtimes_of(&main("abort", "unwind", ",dep")),
        Err("the program of crate `main` panics by `unwind`, and it links crates `dep`, `std`, \
             built to panic by `abort`: a crate built to panic by `abort` is linked only into a \
             program that panics by `abort`"
            .to_owned()),
        "a std built to abort under a program that unwinds"
    );
    // rustc lets a crate built to unwind into a program that aborts unless it makes a call that
    // may unwind out of foreign code; blaze checks no bodies for that, so it links one only
    // from the standard library, and refuses any other by name (#1611 round 3, L5).
    assert_eq!(
        runtimes_of(&program("unwind", "abort", ",dep", "unwind")),
        Err("the program of crate `main` panics by `abort`, and it links crate `dep`, built to \
             panic by `unwind` and not the standard library's: blaze does not check a crate \
             built to unwind for calls that may unwind out of foreign code (rustc's \
             `has_ffi_unwind_calls`), so a program that aborts links one only from the standard \
             library"
            .to_owned())
    );
}

const SANITIZED: &str = r#"
//- minicore: sized, fn
#![feature(sanitize)]
#[sanitize(address = "off")]
pub fn quiet() {}
pub fn loud() {}
#[sanitize(address = "off", thread = "off")]
pub mod inside {
    pub fn inherited() {}
    #[sanitize(thread = "on")]
    pub fn nearest() {}
    pub fn closure() -> u32 { (|| 1)() }
}
#[unsafe(no_mangle)]
pub extern "C" fn __sanitizer_cov_trace_pc_guard(_guard: *mut u32) {}
"#;

fn sanitizing(sanitizers: Sanitizers, coverage: u8) -> Spec {
    let coverage = Coverage { level: coverage, flags: 0 };
    Spec { sanitize: Sanitize { sanitizers, coverage, ..Sanitize::default() }, ..spec() }
}

/// What a function has off of the unit's sanitizers is its nearest `#[sanitize]`, as rustc
/// reads it; a closure has its function's; a `__sanitizer_*` symbol has SanitizerCoverage
/// off; and without a set nothing is off.
#[test]
fn a_functions_sanitizers_off_are_its_nearest_attributes() {
    let (db, frontend) = frontend(SANITIZED);
    let both = Sanitizers::ADDRESS.union(Sanitizers::THREAD);
    let off = |spec: &Spec, instance: &HirInstance| {
        prepared(&frontend, spec, instance).1.function().unsanitized
    };
    let root = |name| frontend.root(function(&db, name));
    let address = sanitizing(Sanitizers::ADDRESS, 0);
    let all = sanitizing(both, 0);
    assert_eq!(off(&address, &root("quiet")), Sanitizers::ADDRESS);
    assert_eq!(off(&address, &root("loud")), Sanitizers::NONE);
    assert_eq!(off(&sanitizing(Sanitizers::THREAD, 0), &root("quiet")), Sanitizers::NONE);
    assert_eq!(off(&spec(), &root("quiet")), Sanitizers::NONE);
    assert_eq!(off(&all, &root("inherited")), both);
    assert_eq!(off(&all, &root("nearest")), Sanitizers::ADDRESS);
    let closure = frontend.with(|db| {
        refs::walk(db, &root("closure"), refs::Walking::plain(false))
            .expect("the walk")
            .into_iter()
            .find_map(|it| match it {
                FrontRef::Function(it) => match &it.0 {
                    Kind::Code(code, _) if matches!(code.def, InstanceDef::Closure(_)) => Some(it),
                    _ => None,
                },
                _ => None,
            })
            .expect("the closure")
    });
    assert_eq!(off(&all, &closure), both);
    let callback = root("__sanitizer_cov_trace_pc_guard");
    assert_eq!(off(&sanitizing(Sanitizers::NONE, 2), &callback), Sanitizers::COVERAGE);
    assert_eq!(off(&address, &callback), Sanitizers::NONE);
    assert_eq!(off(&sanitizing(Sanitizers::NONE, 2), &root("loud")), Sanitizers::NONE);
}

/// A function with a sanitizer off is another template than the same IR with it on, in one
/// unit's settings (which a shape is of as well).
#[test]
fn what_is_unsanitized_is_in_the_shape() {
    let (db, frontend) = frontend(SANITIZED);
    let (quiet, loud) =
        (frontend.root(function(&db, "quiet")), frontend.root(function(&db, "loud")));
    let address = sanitizing(Sanitizers::ADDRESS, 0);
    let thread = sanitizing(Sanitizers::THREAD, 0);
    assert_ne!(prepared(&frontend, &address, &quiet).0, prepared(&frontend, &address, &loud).0);
    assert_eq!(prepared(&frontend, &thread, &quiet).0, prepared(&frontend, &thread, &loud).0);
}

/// Cranelift instruments for no sanitizer, so a unit with a set it would build is refused,
/// never built without the checks.
#[test]
fn cranelift_refuses_a_sanitized_unit() {
    use blaze_codegen::backend::Prepare as _;
    let (db, frontend) = frontend(SANITIZED);
    let loud = frontend.root(function(&db, "loud"));
    let refs = frontend.with(|db| refs_of(db, (&loud, false)));
    let refs: &Refs = &refs;
    for spec in [sanitizing(Sanitizers::ADDRESS, 0), sanitizing(Sanitizers::NONE, 1)] {
        let Err(e) = Backends::default().prepare(&frontend, &unit(&spec), &spec, &loud, refs)
        else {
            panic!("a sanitized Cranelift unit was built");
        };
        assert!(e.what.contains("-Zsanitizer and SanitizerCoverage"), "{e:?}");
    }
    let plain = spec();
    let built = Backends::default().prepare(&frontend, &unit(&plain), &plain, &loud, refs);
    built.expect("a plain unit");
}

/// A unit at LLVM's `opt`, MIR-inlined where the plan inlines one (`Spec::mir_inline`).
fn llvm(opt: u8) -> Spec {
    Spec { backend: Backend::Llvm { opt }, mir_inline: opt >= 2, ..spec() }
}

fn is_call(it: &places::Ref, func: FunctionId) -> bool {
    let FrontRef::Function(HirInstance(Kind::Code(called, _))) = it else { return false };
    called.def == InstanceDef::Fn(func)
}

/// The IR of `instance` built as `spec` says, with what it asked for.
fn built_as(db: &Db, spec: &Spec, instance: &HirInstance) -> super::Built {
    built(db, instance, crate::Target::for_fixtures(), false, Build::of(spec))
        .unwrap_or_else(|e| panic!("{instance:?}: {e}"))
}

/// A unit's bodies are MIR-inlined as rustc's: the library's always, as shipped; every other
/// crate's only in an LLVM unit at level 2 or 3, and in the walk for such a unit.
#[test]
fn a_units_bodies_are_inlined_as_rustcs() {
    let (db, _) = frontend(SIBLINGS);
    let krate = fixture::test_crate(&db);
    let core = base_db::all_crates(db)
        .iter()
        .copied()
        .find(|it| it.data(db).origin.is_lang())
        .expect("minicore's crate");
    let inline = |build: Build, written| build.options(db, written, false).inline;
    let backends = [
        (Backend::Cranelift { speed: false }, false),
        (Backend::Cranelift { speed: true }, false),
        (Backend::Llvm { opt: 0 }, false),
        (Backend::Llvm { opt: 1 }, false),
        (Backend::Llvm { opt: 2 }, true),
        (Backend::Llvm { opt: 3 }, true),
    ];
    for (backend, inlines) in backends {
        let build = Build::of(&Spec { backend, mir_inline: inlines, ..spec() });
        assert_eq!(inline(build, krate), inlines, "{backend:?}");
        assert!(inline(build, core), "the library's at {backend:?}");
    }
    // `s`, `z` and a build that patches are at LLVM's level 2 without the inliner.
    let sized = Build::of(&Spec { backend: Backend::Llvm { opt: 2 }, mir_inline: false, ..spec() });
    assert!(!inline(sized, krate));
    assert!(inline(sized, core));
    assert!(!inline(Build::WALK, krate));
    assert!(inline(Build::WALK_INLINED, krate));
}

/// A member at O3 is inlined, at O0 not, and a package the profile sets apart is as the
/// profile says of it; a generic, which any crate may ask for, is walked for inlining where
/// any is inlined.
#[test]
fn which_units_inline_is_the_policys() {
    let (db, frontend) = frontend(SIBLINGS);
    let root = frontend.root(function(db, "roots"));
    let outer = function(db, "outer");
    let generic = frontend.with(|db| called(db, &root, outer)).remove(0);
    let inlines = |policy: &Policy, instance: &HirInstance| {
        let build_time = BuildTime::default();
        let frontend = HirFrontend::new(Databases::new(db), HashSource::ById(ById), build_time)
            .with_policy(policy);
        frontend.with(|db| frontend.inlines(db, instance)).expect("the policy")
    };
    assert!(!inlines(&policy(OptLevel::O0), &root));
    assert!(inlines(&policy(OptLevel::O3), &root));
    assert!(inlines(&policy(OptLevel::O2), &root));
    assert!(!inlines(&policy(OptLevel::O1), &root));
    let mut set_apart = policy(OptLevel::O3);
    let name = frontend.with(|hir| match frontend.origin(hir, fixture::test_crate(&db)) {
        blaze_codegen::plan::Origin::Member(name) => name.as_str().to_owned(),
        other => panic!("the fixture's crate is {other:?}"),
    });
    let o0 = Override { opt_level: Some(OptLevel::O0), ..Override::default() };
    set_apart.profile.packages.insert(name, o0);
    assert!(!inlines(&set_apart, &root));
    // Other members and the library's crates still inline, and any of them may ask for it.
    assert!(inlines(&set_apart, &generic));
    let unwalked =
        HirFrontend::new(Databases::new(db), HashSource::ById(ById), BuildTime::default());
    let refused = unwalked.with(|db| unwalked.inlines(db, &root));
    assert!(refused.is_err_and(|e| e.contains("with_policy")));
}

const INLINED: &str = r#"
//- minicore: sized, copy
#[inline(never)]
pub fn g(x: u32) -> u32 { x ^ 7 }
#[inline]
pub fn f(x: u32) -> u32 { g(x) }
pub fn caller(x: u32) -> u32 { f(x) }
"#;

/// The walk for a unit that MIR-inlines lists what the inlined code calls: `caller` with `f`
/// inlined calls `g`, which the walk at level 1 does not list, as `caller` calls only `f`
/// there. It lists all the walk without inlining does, first.
#[test]
fn the_walk_lists_what_inlined_code_calls() {
    let (db, frontend) = held_as(fixture::db(INLINED).into_db(), &policy(OptLevel::O3));
    let root = frontend.root(function(db, "caller"));
    let (f, g) = (function(db, "f"), function(db, "g"));
    frontend.with(|db| {
        assert!(frontend.inlines(db, &root).expect("the policy"));
        let plain = refs::walk(db, &root, refs::Walking::plain(false)).expect("the walk");
        let walked =
            refs::walk(db, &root, refs::Walking::plain(true)).expect("the walk for inlining");
        assert!(plain.iter().any(|it| is_call(it, f)), "{plain:?}");
        assert!(!plain.iter().any(|it| is_call(it, g)), "{plain:?}");
        assert!(walked.iter().any(|it| is_call(it, g)), "{walked:?}");
        assert_eq!(walked[..plain.len()], plain[..]);
        let built_in = refs::built_in(db, &root, refs::Walking::plain(true)).expect("the walk");
        assert!(built_in.iter().any(|&at| is_call(&walked[at as usize], f)), "{built_in:?}");
        assert!(
            refs::built_in(db, &root, refs::Walking::plain(false)).expect("the walk").is_empty()
        );
        let krate = compiled_in(db, &root);
        let mut place = |refs: Vec<places::Ref>| {
            let mut code = built_as(db, &llvm(2), &root);
            let super::Built::Ir(pending) = &mut code else { panic!("asm") };
            let mut placer = Placer::listed(refs);
            place_requests(
                db,
                &HashSource::ById(ById),
                krate,
                &mut placer,
                &mut pending.wants.requests,
            )
            .map(drop)
        };
        assert!(place(plain).is_err(), "the inlined IR asks only for what the plain walk lists");
        place(walked).expect("the inlined IR asks only for what its walk lists");
    });
}

const TWICE: &str = r#"
//- minicore: sized, copy
#[inline]
pub fn leaf(x: u32) -> u32 { x ^ 1 }
#[inline]
pub fn mid(x: u32) -> u32 { leaf(x) ^ 2 }
pub fn caller(x: u32) -> u32 { leaf(x) ^ mid(x) }
"#;

/// A callee inlined directly and through another is listed once, built in once, and its code
/// is among what the caller's key holds once.
#[test]
fn a_callee_inlined_twice_is_built_in_once() {
    let (db, frontend) = held_as(fixture::db(TWICE).into_db(), &policy(OptLevel::O3));
    let root = frontend.root(function(db, "caller"));
    let (leaf, mid) = (function(db, "leaf"), function(db, "mid"));
    frontend.with(|db| {
        let walked = refs::walk(db, &root, refs::Walking::plain(true)).expect("the walk");
        let built_in = refs::built_in(db, &root, refs::Walking::plain(true)).expect("the walk");
        assert_eq!(walked.iter().filter(|it| is_call(it, leaf)).count(), 1, "{walked:?}");
        let [one, two] = built_in[..] else { panic!("built in: {built_in:?} of {walked:?}") };
        assert_ne!(one, two);
        let of = |func| built_in.iter().any(|&at| is_call(&walked[at as usize], func));
        assert!(of(leaf) && of(mid), "{built_in:?} of {walked:?}");
        let inlined = inlined_by_build(db, &HashSource::ById(ById), &root, o3());
        assert_eq!(inlined.len(), 2, "leaf and mid: {inlined:?}");
    });
}

const BUILT_IN_INLINES: &str = r#"
//- minicore: sized, copy
//- /dep.rs crate:dep library
#[inline(never)]
pub fn leaf(x: u32) -> u32 { x ^ 7 }
#[inline]
pub fn mid(x: u32) -> u32 { leaf(x) }
#[cold]
#[inline(always)]
pub fn load(x: u32) -> u32 { mid(x) }
//- /main.rs crate:main deps:dep
pub fn caller(x: u32) -> u32 { dep::load(x) }
"#;

/// A body the MIR inliner inlines nothing into still builds in its `#[inline(always)]`
/// callees, whose own bodies a unit that inlines has MIR-inlined: foldhash's
/// `hash_bytes_long`, whose built-in `load` calls `check_language_ub::runtime` once
/// `read_unaligned` is inlined into it. `caller` has nothing inlined (a cold callee is never
/// MIR-inlined), yet built at level 2 it asks for `leaf`, which `load` reaches through `mid`:
/// the walk for inlining units lists it, the walk without does not.
#[test]
fn the_walk_lists_what_a_built_in_callees_inlined_body_calls() {
    let (db, frontend) = held_as(fixture::db(BUILT_IN_INLINES).into_db(), &policy(OptLevel::O3));
    let root = frontend.root(function(db, "caller"));
    frontend.with(|db| {
        let named = |it: &places::Ref, name: &str| match it {
            FrontRef::Function(HirInstance(Kind::Code(called, _))) => match called.def {
                InstanceDef::Fn(func) => FunctionSignature::of(db, func).name.as_str() == name,
                _ => false,
            },
            _ => false,
        };
        assert!(frontend.inlines(db, &root).expect("the policy"));
        let plain = refs::walk(db, &root, refs::Walking::plain(false)).expect("the walk");
        let walked =
            refs::walk(db, &root, refs::Walking::plain(true)).expect("the walk for inlining");
        assert!(!plain.iter().any(|it| named(it, "leaf")), "{plain:?}");
        assert!(walked.iter().any(|it| named(it, "leaf")), "{walked:?}");
        let krate = compiled_in(db, &root);
        let mut place = |refs: Vec<places::Ref>| {
            let mut code = built_as(db, &llvm(2), &root);
            let super::Built::Ir(pending) = &mut code else { panic!("asm") };
            let mut placer = Placer::listed(refs);
            place_requests(
                db,
                &HashSource::ById(ById),
                krate,
                &mut placer,
                &mut pending.wants.requests,
            )
            .map(drop)
        };
        assert!(place(plain).is_err(), "the unit's IR asks for `leaf`");
        place(walked).expect("the unit's IR asks only for what its walk lists");
    });
}

const LIBRARY_BUILDS_IN: &str = r#"
//- minicore: sized, copy, ord
//- /dep.rs crate:dep library
#[inline(never)]
pub fn leaf(x: u32) -> bool { x > 7 }
#[inline]
pub fn mid(x: u32) -> bool { leaf(x) }
pub struct S;
impl core::cmp::PartialEq<u32> for S {
    #[inline(always)]
    fn eq(&self, other: &u32) -> bool { mid(*other) }
}
//- /main.rs crate:main deps:dep
pub fn caller(a: &dep::S, b: &u32) -> bool { a == b }
"#;

/// A body of the library's, whose own MIR is the same under every unit's settings, still
/// builds in the `#[inline(always)]` callees of other crates under the unit's, where their
/// bodies are MIR-inlined: core's `<&S as PartialEq<&u32>>::eq` builds in `dep`'s
/// `<S as PartialEq<u32>>::eq`, which inlined asks for `leaf` through `mid`, as smol_str's
/// `<&SmolStr as PartialEq<&str>>::eq` asked for `Repr::as_str` and `memcmp`. The walk for
/// inlining units lists it, the walk without does not, and the unit's IR asks only for what
/// its walk lists.
#[test]
fn the_walk_of_a_library_body_lists_what_its_built_in_callees_inlined_call() {
    use hir_def::HasModule as _;
    let (db, frontend) = held_as(fixture::db(LIBRARY_BUILDS_IN).into_db(), &policy(OptLevel::O3));
    let root = frontend.root(function(db, "caller"));
    frontend.with(|db| {
        let named = |it: &places::Ref, name: &str, lang: bool| match it {
            FrontRef::Function(HirInstance(Kind::Code(called, _))) => match called.def {
                InstanceDef::Fn(func) => {
                    FunctionSignature::of(db, func).name.as_str() == name
                        && func.krate(db).data(db).origin.is_lang() == lang
                }
                _ => false,
            },
            _ => false,
        };
        let called =
            refs::walk(db, &root, refs::Walking::plain(false)).expect("the walk of `caller`");
        let Some(FrontRef::Function(eq)) = called.iter().find(|it| named(it, "eq", true)).cloned()
        else {
            panic!("`caller` calls core's `eq`: {called:?}")
        };
        assert!(frontend.inlines(db, &eq).expect("the policy"));
        let plain = refs::walk(db, &eq, refs::Walking::plain(false)).expect("the walk");
        let walked =
            refs::walk(db, &eq, refs::Walking::plain(true)).expect("the walk for inlining");
        assert!(plain.iter().any(|it| named(it, "mid", false)), "{plain:?}");
        assert!(!plain.iter().any(|it| named(it, "leaf", false)), "{plain:?}");
        assert!(walked.iter().any(|it| named(it, "leaf", false)), "{walked:?}");
        assert_eq!(walked[..plain.len()], plain[..]);
        let krate = compiled_in(db, &eq);
        let mut place = |refs: Vec<places::Ref>| {
            let mut code = built_as(db, &llvm(2), &eq);
            let super::Built::Ir(pending) = &mut code else { panic!("asm") };
            let mut placer = Placer::listed(refs);
            place_requests(
                db,
                &HashSource::ById(ById),
                krate,
                &mut placer,
                &mut pending.wants.requests,
            )
            .map(drop)
        };
        assert!(place(plain).is_err(), "the unit's IR asks for `leaf`");
        place(walked).expect("the unit's IR asks only for what its walk lists");
    });
}

const TRACKED: &str = r#"
//- minicore: sized, copy, panic_location
use core::panic::Location;
#[track_caller]
#[inline]
pub fn here() -> &'static Location<'static> { Location::caller() }
pub fn caller() -> &'static Location<'static> { here() }
"#;

/// A `#[track_caller]` callee inlined reports where its caller called it, as it does when it
/// is called: the location is of the caller's body, at the call.
#[test]
fn an_inlined_track_caller_callee_reports_the_call() {
    use crate::fragment::DataRequest;
    let (db, frontend) = held_as(fixture::db(TRACKED).into_db(), &policy(OptLevel::O3));
    let root = frontend.root(function(db, "caller"));
    let here = function(db, "here");
    let caller: DefWithBodyId = function(db, "caller").into();
    frontend.with(|db| {
        let walked = refs::walk(db, &root, refs::Walking::plain(true)).expect("the walk");
        let built_in = refs::built_in(db, &root, refs::Walking::plain(true)).expect("the walk");
        assert!(built_in.iter().any(|&at| is_call(&walked[at as usize], here)), "{built_in:?}");
        let locations = |spec: &Spec| {
            let code = built_as(db, spec, &root);
            let super::Built::Ir(pending) = &code else { panic!("asm") };
            (pending.wants.requests.datas.iter())
                .filter_map(|it| match it {
                    DataRequest::Location(at) => Some(*at),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let called = locations(&spec());
        let inlined = locations(&llvm(2));
        assert!(matches!(called[..], [Some((owner, _))] if owner == caller), "{called:?}");
        assert_eq!(inlined, called);
    });
}

/// Where each statement is written: the body's own, and inlined code in the body it is of,
/// each scope at its call; own spans read the call the outermost inlined body is at. A
/// statement of a body that is no item's has no place, which is an error.
#[test]
fn inlined_statements_are_written_in_their_callees() {
    use hir_ty_mir::mir::{InlinedScope, InlinedSpan};
    let (db, _) = frontend(TWICE);
    let [owner, leaf, mid] =
        ["caller", "leaf", "mid"].map(|it| DefWithBodyId::from(function(db, it)));
    let expr =
        |index| MirSpan::ExprId(hir_def::hir::ExprId::from_raw(la_arena::RawIdx::from_u32(index)));
    // `mid` is inlined at 1, and `leaf` into it at its 2.
    let scopes = [
        InlinedScope { callee: 0, call_site: expr(1) },
        InlinedScope { callee: 1, call_site: MirSpan::Inlined(0) },
    ];
    let spans = [InlinedSpan { scope: 0, span: expr(2) }, InlinedSpan { scope: 1, span: expr(3) }];
    let statements = vec![expr(5), MirSpan::Inlined(1), MirSpan::Inlined(0)];
    let callees = vec![Some((mid, None)), Some((leaf, None))];
    let source =
        crate::fragment::Source::resolved(owner, &spans, &scopes, callees, statements.clone())
            .expect("a source");
    assert_eq!(source.spans, [expr(5), expr(1), expr(1)]);
    let written = |at| source.written(at, None).map(|it| (it.owner, it.span));
    assert_eq!(written(0), Some((owner, expr(5))));
    assert_eq!(written(1), Some((leaf, expr(3))));
    assert_eq!(written(2), Some((mid, expr(2))));
    assert_eq!(written(3), None);
    let site = |scope| source.call_site(scope, None).map(|it| (it.owner, it.span));
    assert_eq!(site(0), Some((owner, expr(1))));
    assert_eq!(site(1), Some((mid, expr(2))));
    assert_eq!([0, 1, 2].map(|at| source.scope_of(at)), [None, Some(1), Some(0)]);
    let unplaced = vec![None, Some((leaf, None))];
    let refused = crate::fragment::Source::resolved(owner, &spans, &scopes, unplaced, statements);
    assert!(refused.is_err_and(|e| e.contains("no item's body")));
}

const LOCALS: &str = r#"
//- minicore: sized, add, mul
pub fn counted(x: u32) -> u32 {
    let one = x + 1;
    let two = one * 2;
    two
}
"#;

/// A function's named locals are told by binding, so renaming one changes nothing its side
/// table is made of, and the name a debugger is told is read from the database of the build.
#[test]
fn renaming_a_local_keeps_what_its_function_tells_a_debugger() {
    let full = Spec { debuginfo: DebugInfo::Full, ..spec() };
    let told = |text: &str| {
        let (db, frontend) = frontend(text);
        let counted = function(db, "counted");
        let prepared = prepared(&frontend, &full, &frontend.root(counted)).1;
        let function = prepared.function();
        let body = hir_def::expr_store::Body::of(db, counted.into());
        let mut names: Vec<String> = (function.variables.iter())
            .map(|variable| {
                let (_, binding) = (body.bindings())
                    .find(|(id, _)| id.into_raw().into_u32() == variable.binding)
                    .unwrap_or_else(|| panic!("binding {}", variable.binding));
                binding.name.as_str().to_owned()
            })
            .collect();
        names.sort_unstable();
        (function.variables.clone(), format!("{:?}", function.scopes), names)
    };
    let (variables, scopes, names) = told(LOCALS);
    assert_eq!(names, ["one", "two", "x"]);
    let (renamed, renamed_scopes, renamed_names) = told(&LOCALS.replace("one", "uno"));
    assert_eq!((renamed, renamed_scopes), (variables, scopes), "nothing of the side table changed");
    assert_eq!(renamed_names, ["two", "uno", "x"], "the debugger is told the new name");
}

const LINE_READ: &str = r#"
//- minicore: panic_location
#[rustc_builtin_macro]
macro_rules! line {() => {}}

pub fn here() -> u32 {
    line!()
}
"#;

/// A function reads `line!()` from the `Location` of where it is written, called or MIR-inlined
/// alike: the link fills the record with the line a fresh build has.
#[test]
fn a_line_is_read_from_the_location_where_it_is_written() {
    use crate::fragment::DataRequest;
    let fx = fixture::db(LINE_READ);
    let file = fx.only_file();
    let db = fx.into_db();
    let text = db.file_text(file.file_id(&db)).text(&db).to_string();
    let written = text[..text.find("line!()").expect("the call")].matches('\n').count() as u32 + 1;
    let (db, frontend) = held(db);
    let here = function(db, "here");
    let root = frontend.root(here);
    frontend.with(|db| {
        let lines = |spec: &Spec| {
            let super::Built::Ir(pending) = built_as(db, spec, &root) else { panic!("asm") };
            (pending.wants.requests.datas.iter())
                .filter_map(|it| match it {
                    DataRequest::Location(Some((owner, span))) => {
                        let place =
                            hir_ty_mir::mir::location::source_location_in_file(db, *owner, *span);
                        Some(place.expect("a place").1)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(lines(&spec()), [written]);
        assert_eq!(lines(&llvm(2)), [written], "inlined `Location::caller` reads the same");
    });
}

/// A trait method's call that no impl holds for, its arguments concrete, stops naming the
/// trait, the method, the arguments and why; one a where clause proves names the trait's own
/// method, whose impl is chosen once its arguments are known; one whose impl inherits the
/// trait's provided method (`Write::write_fmt` of `Stderr`) is of that provided method.
#[test]
fn a_trait_call_no_impl_holds_for_stops_with_why_and_a_provided_method_is_inherited() {
    use hir_def::{AdtId, ModuleDefId};
    use hir_expand::name::Name;
    use hir_ty::{
        ParamEnvAndCrate,
        next_solver::{DbInterner, GenericArgs, Ty},
    };
    use rustc_type_ir::inherent::{GenericArgs as _, Ty as _};
    let (db, _) = frontend(
        r#"
pub trait P {
    fn distance(&self) -> usize;
    fn twice(&self) -> usize { self.distance() * 2 }
}
pub struct S;
pub struct Inherits;
impl P for Inherits {
    fn distance(&self) -> usize { 1 }
}
pub fn concrete(_: &S) {}
pub fn bounded<T: P>(at: &T) -> usize {
    at.distance()
}
"#,
    );
    let krate = fixture::test_crate(db);
    let declared = hir_def::test_items::declarations(db, krate);
    let p = declared.iter().find_map(|it| match it {
        ModuleDefId::TraitId(it) => Some(*it),
        _ => None,
    });
    let structs: Vec<_> = declared
        .iter()
        .filter_map(|it| match it {
            ModuleDefId::AdtId(AdtId::StructId(it)) => Some(*it),
            _ => None,
        })
        .collect();
    let p = p.expect("the trait `P`");
    let [s, inherits] = structs[..] else { panic!("the structs `S` and `Inherits`: {structs:?}") };
    let method =
        |name: &str| p.trait_items(db).method_by_name(&Name::new_root(name)).expect("`P`'s method");
    let (distance, twice) = (method("distance"), method("twice"));
    let (concrete, bounded) = (function(db, "concrete"), function(db, "bounded"));
    blaze_query::db::attach(db, || {
        let interner = DbInterner::new_with(db, krate);
        let env = |def: FunctionId| ParamEnvAndCrate {
            param_env: db.trait_environment(def.into()),
            krate,
        };
        let s = Ty::new_adt(interner, AdtId::StructId(s), GenericArgs::empty());
        let why = crate::impl_method(
            db,
            env(concrete),
            distance,
            GenericArgs::new_from_slice(&[s.into()]),
        )
        .expect_err("no impl of `P` is for `S`");
        assert!(why.starts_with("trait P method distance for [S"), "{why}");
        assert!(why.ends_with(": no impl holds"), "{why}");

        let param = GenericArgs::identity_for_item(interner, bounded.into()).type_at(0);
        let (found, _) = crate::impl_method(
            db,
            env(bounded),
            distance,
            GenericArgs::new_from_slice(&[param.into()]),
        )
        .expect("a where clause proves `T: P`");
        assert_eq!(found, either::Either::Left(distance));

        let inherits = Ty::new_adt(interner, AdtId::StructId(inherits), GenericArgs::empty());
        let (found, _) = crate::impl_method(
            db,
            env(concrete),
            twice,
            GenericArgs::new_from_slice(&[inherits.into()]),
        )
        .expect("`Inherits` inherits `P::twice`");
        assert_eq!(found, either::Either::Left(twice));
    });
}

/// The drop glues of two types of one layout, each with its own `Drop`, are two instances:
/// one key would make one compile and one template serve both bodies.
#[test]
fn drop_glues_of_two_types_have_two_keys() {
    use blaze_codegen::backend::Frontend as _;
    let (db, frontend) = frontend(
        r#"
//- minicore: sized, drop
pub struct A(pub u32);
impl Drop for A { fn drop(&mut self) {} }
pub struct B(pub u32);
impl Drop for B { fn drop(&mut self) {} }
pub fn roots(x: u32) {
    let _a = A(x);
    let _b = B(x);
}
"#,
    );
    // A failure's message formats instances, whose types are the database's.
    blaze_query::db::attach(db, || {
        let policy = policy(OptLevel::O0);
        let root = frontend.root(function(db, "roots"));
        let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
        let key = |instance: &HirInstance| {
            let described = (frontend.describe(instance, &origin))
                .unwrap_or_else(|e| panic!("{instance:?}: {}", e.what));
            let choice =
                policy.choose(&described.origin, described.overflow_checks).expect("the plan");
            UnitInstance {
                code: described.code,
                args: described.args,
                settings: choice.first.settings(),
            }
            .key()
        };
        let glues: Vec<HirInstance> = frontend.with(|hir| {
            refs::walk(hir, &root, refs::Walking::plain(false))
                .expect("the walk of the root")
                .into_iter()
                .filter_map(|it| match it {
                    FrontRef::Function(instance)
                        if matches!(&instance.0, Kind::Code(it, _) if it.def == InstanceDef::DropGlue) =>
                    {
                        Some(instance)
                    }
                    _ => None,
                })
                .collect()
        });
        let [a, b] = &glues[..] else { panic!("{glues:?}") };
        assert_ne!(key(a), key(b), "{a:?} and {b:?}");
    });
}

/// Rule 21 for which callees are one instance: `one::<A>` and `one::<B>` have one compile
/// identity, so `two::<A, B>` and `two::<A, A>` do too, and one shared identity in their
/// arguments' place (asserted first, so the keys differ only by their placement). But the first
/// calls two instances and the second one: two places against one, and other code, so they are
/// two keys. `three::<A, B, A>` and `three::<A, B, B>` list the same two places and differ only
/// in which one the third call is given; `reified::<A, B, A>` and `reified::<A, B, B>` the
/// same through function pointers, which the recorder gives a request asked for before
/// (`Wants::asks`). `two::<A, A>` and `two::<B, B>` place their calls alike and stay one key.
/// A build of blaze gave `needs_realloc::<Vec<usize>, ExtendedKeyPurpose>` and
/// `needs_realloc::<CertificateDer, CertificateDer>` one key over 12 references against 11.
#[test]
fn instances_whose_calls_are_placed_apart_have_two_keys() {
    use blaze_codegen::backend::Frontend as _;
    let (db, frontend) = frontend(
        r#"
//- minicore: sized
pub struct A(pub u32);
pub struct B(pub u32);
pub fn one<T>(x: &T) -> &T { x }
pub fn two<S, D>(s: &S, d: &D) -> (&S, &D) { (one(s), one(d)) }
pub fn three<S, D, E>(s: &S, d: &D, e: &E) { one(s); one(d); one(e); }
pub fn nothing<T>() {}
pub fn reified<S, D, E>() -> [fn(); 3] { [nothing::<S>, nothing::<D>, nothing::<E>] }
pub fn roots(a: A, b: B) {
    two(&a, &b);
    two(&a, &a);
    two(&b, &b);
    three(&a, &b, &a);
    three(&a, &b, &b);
    reified::<A, B, A>();
    reified::<A, B, B>();
}
"#,
    );
    // A failure's message formats instances, whose types are the database's.
    blaze_query::db::attach(db, || {
        let policy = policy(OptLevel::O0);
        let root = frontend.root(function(db, "roots"));
        let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(&db)));
        let key = |instance: &HirInstance| {
            let described = (frontend.describe(instance, &origin))
                .unwrap_or_else(|e| panic!("{instance:?}: {}", e.what));
            let choice =
                policy.choose(&described.origin, described.overflow_checks).expect("the plan");
            let key = UnitInstance {
                code: described.code,
                args: described.args,
                settings: choice.first.settings(),
            }
            .key();
            (key, described.code)
        };
        // What the key held in the arguments' place before the placement was mixed in.
        let shared = |instance: &HirInstance| {
            frontend
                .with(|hir| frontend.shared_args(hir, &instance.0, &origin))
                .unwrap_or_else(|e| panic!("{instance:?}: {e}"))
        };
        let of = |name| {
            let func = function(db, name);
            frontend.with(|hir| called(hir, &root, func))
        };
        let one = function(db, "one");
        let ones = |instance: &HirInstance| frontend.with(|hir| called(hir, instance, one)).len();

        let twos = of("two");
        let [ab, aa, bb] = &twos[..] else { panic!("{twos:?}") };
        // The precondition: one code and one shared identity, so only the placement tells
        // them apart.
        assert_eq!((key(ab).1, shared(ab)), (key(aa).1, shared(aa)), "{ab:?} and {aa:?}");
        assert_eq!((key(aa).1, shared(aa)), (key(bb).1, shared(bb)), "{aa:?} and {bb:?}");
        // The cause: `two::<A, B>` lists two instances of `one`, `two::<A, A>` one.
        assert_eq!((ones(ab), ones(aa), ones(bb)), (2, 1, 1));
        assert_ne!(key(ab).0, key(aa).0, "{ab:?} and {aa:?}");
        assert_ne!(key(ab).0, key(bb).0, "{ab:?} and {bb:?}");
        assert_eq!(key(aa).0, key(bb).0, "{aa:?} and {bb:?}");

        let threes = of("three");
        let [aba, abb] = &threes[..] else { panic!("{threes:?}") };
        assert_eq!((key(aba).1, shared(aba)), (key(abb).1, shared(abb)), "{aba:?} and {abb:?}");
        // Two places each: only which one the third call is given differs.
        assert_eq!((ones(aba), ones(abb)), (2, 2));
        assert_ne!(key(aba).0, key(abb).0, "{aba:?} and {abb:?}");

        let reifieds = of("reified");
        let [aba, abb] = &reifieds[..] else { panic!("{reifieds:?}") };
        assert_ne!(key(aba).0, key(abb).0, "{aba:?} and {abb:?}");
    });
}

pub(super) const POINTERS_TO_DYN: &str = r#"
//- minicore: sized, coerce_unsized, dispatch_from_dyn
pub trait Tr { fn f(&self) -> u32; }
pub struct A(pub u32);
impl Tr for A { fn f(&self) -> u32 { self.0 } }
pub struct B(pub u32);
impl Tr for B { fn f(&self) -> u32 { self.0 } }
pub struct C(pub u32);
impl Tr for C { fn f(&self) -> u32 { self.0 } }
pub const RAW: *const dyn Tr = &A(9) as &dyn Tr;
pub fn raw() -> *const dyn Tr { RAW }
pub trait Has { type Obj: ?Sized + 'static; }
pub struct H;
impl Has for H { type Obj = dyn Tr; }
pub struct W<U: Has>(pub &'static U::Obj);
pub const ALIASED: W<H> = W(&B(8) as &dyn Tr);
pub fn aliased() -> W<H> { ALIASED }
pub struct RawRef(pub *const &'static dyn Tr);
pub const BEHIND: RawRef = RawRef(&(&C(7) as &dyn Tr) as *const &dyn Tr);
pub fn behind() -> RawRef { BEHIND }
pub struct D(pub u32);
impl Tr for D { fn f(&self) -> u32 { self.0 } }
pub struct Wrap<X: ?Sized>(pub u32, pub X);
pub const WRAP: &Wrap<dyn Tr> = &Wrap(100, D(5));
pub fn wrap() -> &'static Wrap<dyn Tr> { WRAP }
pub struct E(pub u32);
impl Tr for E { fn f(&self) -> u32 { self.0 } }
pub const RAW_SLICE: *const [&dyn Tr] = &[&E(1) as &dyn Tr] as *const [&dyn Tr];
pub fn raw_slice() -> *const [&'static dyn Tr] { RAW_SLICE }
pub fn eleven() -> u32 { 11 }
pub struct RawFn(pub *const fn() -> u32);
pub const FN_BEHIND: RawFn = RawFn(&(eleven as fn() -> u32) as *const fn() -> u32);
pub fn fn_behind() -> RawFn { FN_BEHIND }
"#;

/// A constant's pointer to `dyn` points at its value and its metadata at the vtable whatever
/// reaches it, as rustc relocates every pointer by its provenance: a raw `*const dyn Tr`
/// (const eval's id was kept as the vtable pointer, and a call through it crashed), a
/// reference whose pointee is an alias that normalizes to `dyn` (`&<H as Has>::Obj`, whose
/// value const eval did not keep either), a `&dyn` in memory behind a raw pointer or a raw
/// slice, and a reference to a struct with a `dyn` tail. A function pointer behind a raw
/// pointer is the function's address.
#[test]
fn every_pointer_to_dyn_in_a_constant_points_at_its_vtable() {
    use crate::fragment::DataRequest;
    let (db, frontend) = frontend(POINTERS_TO_DYN);
    let word = crate::Target::for_fixtures().word() as u32;
    blaze_query::db::attach(db, || {
        let cases =
            [("raw", "A"), ("aliased", "B"), ("behind", "C"), ("wrap", "D"), ("raw_slice", "E")];
        for (name, of) in cases {
            let root = frontend.root(function(db, name));
            let (vtable, pointers) = frontend.with(|db| {
                let super::Built::Ir(pending) = built_as(db, &spec(), &root) else { panic!("asm") };
                let datas = &pending.wants.requests.datas;
                let vtable = datas.iter().position(|it| {
                    let DataRequest::Vtable { ty, .. } = it else { return false };
                    format!("{:?}", ty.as_ref()).contains(of)
                });
                // Each blob's relocated words: the offset and what it points at.
                let pointers: Vec<Vec<(u32, u32)>> = (datas.iter())
                    .filter_map(|it| match it {
                        DataRequest::Blob { blob, .. } => {
                            let mut it: Vec<_> =
                                blob.relocs.iter().map(|&(at, to, _)| (at, to.as_u32())).collect();
                            it.sort_unstable();
                            Some(it)
                        }
                        _ => None,
                    })
                    .collect();
                (vtable, pointers)
            });
            let Some(vtable) = vtable else {
                panic!("`{name}` asks for no vtable of `{of}`: {pointers:?}")
            };
            // The `dyn` pointer: its data pointer relocated, its metadata the vtable.
            assert!(
                pointers.iter().any(|it| {
                    matches!(it[..], [(0, _), (at, to)] if at == word && to == vtable as u32)
                }),
                "`{name}`'s pointer to `dyn` is not relocated whole: {pointers:?}"
            );
        }
        let root = frontend.root(function(db, "fn_behind"));
        let functions = frontend.with(|db| {
            let super::Built::Ir(pending) = built_as(db, &spec(), &root) else { panic!("asm") };
            (pending.wants.requests.datas.iter())
                .filter_map(|it| match it {
                    DataRequest::Blob { blob, .. } => {
                        Some(blob.fn_relocs.iter().map(|it| it.0).collect::<Vec<_>>())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        assert!(
            functions.iter().any(|it| it[..] == [0]),
            "the `fn` behind `FN_BEHIND` is not relocated: {functions:?}"
        );
    });
}

pub(super) const CONSTANT_MEMORY: &str = r#"
//- minicore: sized, coerce_unsized, dispatch_from_dyn, fn, sync, transmute, copy, manually_drop, phantom_data, pointee, index, slice
pub trait Tr { fn f(&self) -> u32; }
pub struct A(pub u32);
impl Tr for A { fn f(&self) -> u32 { self.0 } }
pub struct B(pub u32);
impl Tr for B { fn f(&self) -> u32 { self.0 } }
pub struct F(pub u32);
impl Tr for F { fn f(&self) -> u32 { self.0 } }
pub struct G(pub u32);
impl Tr for G { fn f(&self) -> u32 { self.0 } }
pub struct K(pub u32);
impl Tr for K { fn f(&self) -> u32 { self.0 } }
pub struct Vt { pub name: &'static u32 }
pub struct P(pub *const u8);
pub const CAST: P = P(&Vt { name: &5 } as *const Vt as *const u8);
pub fn cast() -> P { CAST }
pub struct Two(pub *const u8, pub &'static &'static dyn Tr);
pub const TWO: Two = {
    let t: &&dyn Tr = &(&A(1) as &dyn Tr);
    Two(t as *const &dyn Tr as *const u8, t)
};
pub fn two() -> Two { TWO }
pub struct Holder(pub &'static dyn Tr);
impl Tr for Holder { fn f(&self) -> u32 { self.0.f() } }
pub struct Wrap<X: ?Sized>(pub u32, pub X);
pub const TAIL: &Wrap<dyn Tr> = &Wrap(1, Holder(&F(2) as &dyn Tr));
pub fn tail() -> &'static Wrap<dyn Tr> { TAIL }
pub trait Has { type Obj: ?Sized + 'static; }
pub struct H;
impl Has for H { type Obj = dyn Tr; }
pub struct Outer<U: Has>(pub &'static Wrap<U::Obj>);
pub const ALIAS_TAIL: Outer<H> = Outer(&Wrap(3, G(4)) as &Wrap<dyn Tr>);
pub fn alias_tail() -> Outer<H> { ALIAS_TAIL }
pub const CLOSURE: &dyn Fn() -> u32 = &{ let t: &'static dyn Tr = &K(6); move || t.f() };
pub fn closure() -> &'static dyn Fn() -> u32 { CLOSURE }
#[repr(C)]
pub struct End(pub [u8; 4], pub ());
pub static ENDS: End = End([1, 2, 3, 4], ());
pub struct Range(pub *const u8, pub *const ());
pub const STATIC_END: Range = Range(&raw const ENDS.0 as *const u8, &raw const ENDS.1);
pub fn static_end() -> Range { STATIC_END }
pub const KEPT: &End = &End([5, 6, 7, 8], ());
pub const KEPT_END: Range = Range(&raw const KEPT.0 as *const u8, &raw const KEPT.1);
pub fn kept_end() -> Range { KEPT_END }
pub static AA: A = A(7);
pub const TO_STATIC: *const dyn Tr = &AA as &dyn Tr as *const dyn Tr;
pub fn to_static() -> *const dyn Tr { TO_STATIC }
pub const BYTES: [u8; 8] = unsafe { core::mem::transmute::<&u32, [u8; 8]>(&5) };
pub fn bytes() -> [u8; 8] { BYTES }
pub const END_REF: &() = &ENDS.1;
pub fn end_ref() -> &'static () { END_REF }
pub const DEEP: P = P(&(&raw const ENDS.1) as *const *const () as *const u8);
pub fn deep() -> P { DEEP }
#[repr(C)]
pub struct End8(pub [u8; 8], pub ());
pub struct Pair(pub Range, pub &'static End8);
pub const PAIR: Pair = {
    let a: &End8 = &End8([1; 8], ());
    let b: &End8 = &End8([2; 8], ());
    Pair(Range(&raw const a.0 as *const u8, &raw const a.1), b)
};
pub fn pair() -> Pair { PAIR }
pub struct Prefix<X: ?Sized>(pub &'static u32, pub X);
pub const PREFIX: &Prefix<[u8]> = &Prefix(&9, [1, 2]);
pub fn prefix() -> &'static Prefix<[u8]> { PREFIX }
pub struct OrderP(pub *const &'static u32);
pub const ORDER: (&[&u32], OrderP) = {
    let arr: &'static [&'static u32; 3] = &[&1, &2, &3];
    let a: &'static [&'static u32] = arr;
    (a, OrderP(&arr[1] as *const &'static u32))
};
pub fn order() -> (&'static [&'static u32], OrderP) { ORDER }
pub struct Reordered { pub x: Option<&'static dyn Tr>, pub y: &'static dyn Tr }
pub struct OrderD(pub *const &'static dyn Tr);
pub const ORDER_DYN: (&Reordered, OrderD) = {
    let s: &'static Reordered = &Reordered { x: Some(&F(1)), y: &G(2) };
    (s, OrderD(&s.y as *const &'static dyn Tr))
};
pub fn order_dyn() -> (&'static Reordered, OrderD) { ORDER_DYN }
fn seven() -> u32 { 7 }
pub const NUM: ([u64; 4], fn() -> u32) = ([0x0600_0000, 0x0600_0001, 0x0600_0002, 0x0600_0003], seven);
pub fn num() -> ([u64; 4], fn() -> u32) { NUM }
pub const SLTAIL: &Wrap<[&dyn Tr]> = &Wrap(1, [&F(5) as &dyn Tr, &G(6) as &dyn Tr]);
pub fn sltail() -> &'static Wrap<[&'static dyn Tr]> { SLTAIL }
pub struct AsPtr(pub *const &'static dyn Tr);
pub const PAIR_OF_DYN: &[&dyn Tr; 2] = &[&F(1) as &dyn Tr, &G(2) as &dyn Tr];
pub const ASPTR: AsPtr = AsPtr(PAIR_OF_DYN as *const [&dyn Tr; 2] as *const &dyn Tr);
pub fn asptr() -> AsPtr { ASPTR }
pub union Mu<T: Copy> { pub uninit: (), pub value: T }
pub const MUNINIT: Mu<&dyn Tr> = Mu { value: &K(4) as &dyn Tr };
pub fn muninit() -> Mu<&'static dyn Tr> { MUNINIT }
pub union U2 { pub d: &'static dyn Tr, pub n: (usize, usize) }
pub const U2_DYN: U2 = U2 { d: &A(1) as &dyn Tr };
pub fn u2_dyn() -> U2 { U2_DYN }
pub const U2_NUM: U2 = U2 { n: (1, 2) };
pub fn u2_num() -> U2 { U2_NUM }
#[repr(C, packed(2))]
pub struct Pk<X: ?Sized>(pub u16, pub core::mem::ManuallyDrop<Prefix<X>>);
pub const PKPRE: &Pk<dyn Tr> =
    &Pk(1, core::mem::ManuallyDrop::new(Prefix(&77, Holder(&K(9) as &dyn Tr))));
pub fn pkpre() -> &'static Pk<dyn Tr> { PKPRE }
pub struct In2<X: ?Sized>(pub u8, pub X);
#[repr(C, packed(2))]
pub struct PkTail<X: ?Sized>(pub u16, pub core::mem::ManuallyDrop<X>);
pub const PK_SLICE: &PkTail<In2<[&dyn Tr]>> =
    &PkTail(1, core::mem::ManuallyDrop::new(In2(2, [&F(3) as &dyn Tr, &G(4)])));
pub fn pk_slice() -> &'static PkTail<In2<[&'static dyn Tr]>> { PK_SLICE }
pub const TWO_VIEWS: (&(usize, usize), &&dyn Tr) = {
    let t: &&dyn Tr = &(&A(1) as &dyn Tr);
    (unsafe { core::mem::transmute::<&&dyn Tr, &(usize, usize)>(t) }, t)
};
pub fn two_views() -> (&'static (usize, usize), &'static &'static dyn Tr) { TWO_VIEWS }
fn eight() -> u32 { 8 }
pub const FN_NUM: (fn() -> u32, *const fn() -> u32) =
    (eight, &0x0600_0000usize as *const usize as *const fn() -> u32);
pub fn fn_num() -> (fn() -> u32, *const fn() -> u32) { FN_NUM }
pub struct Meta(pub *const (), pub core::ptr::DynMetadata<<H as Has>::Obj>);
pub const META: Meta = unsafe { core::mem::transmute::<&dyn Tr, Meta>(&F(3) as &dyn Tr) };
pub fn meta() -> Meta { META }
#[repr(C)]
pub struct E3(pub [u8; 3], pub [u8; 0]);
pub const EMP: (&[u8], &[u8], &[u8]) = {
    let e: &'static E3 = &E3([1, 2, 3], []);
    let a: &'static [u8] = &e.0;
    unsafe {
        (
            a,
            core::mem::transmute::<(*const u8, usize), &[u8]>((&e.0[1] as *const u8, 0)),
            core::mem::transmute::<(*const u8, usize), &[u8]>((&e.1 as *const [u8; 0] as *const u8, 0)),
        )
    }
};
pub fn emp() -> (&'static [u8], &'static [u8], &'static [u8]) { EMP }
pub const TUP: &(u8, dyn Tr) = unsafe {
    let held: &dyn Tr = &Holder(&G(0) as &dyn Tr);
    let parts = core::mem::transmute::<&dyn Tr, (*const (), *const ())>(held);
    let tuple: &'static (u8, Holder) = &(1u8, Holder(&G(5) as &dyn Tr));
    core::mem::transmute::<(*const (u8, Holder), *const ()), &(u8, dyn Tr)>((tuple, parts.1))
};
pub fn tup() -> &'static (u8, dyn Tr) { TUP }
pub const NOTHING: (&(), &[u32], &[u8; 0]) = {
    let e: &'static E3 = &E3([1, 2, 3], []);
    (&(), &[], &e.1)
};
pub fn nothing() -> (&'static (), &'static [u32], &'static [u8; 0]) { NOTHING }
pub const RAW_DYN: P = P(&(&A(2) as &dyn Tr) as *const &dyn Tr as *const u8);
pub fn raw_dyn() -> P { RAW_DYN }
pub trait TOne { fn one(&self) -> u32; }
pub trait TThree { fn t1(&self) -> u32; fn t2(&self) -> u32; fn t3(&self) -> u32; }
pub struct XOne(pub u32);
impl TOne for XOne { fn one(&self) -> u32 { self.0 } }
pub struct YBoth(pub u32);
impl TOne for YBoth { fn one(&self) -> u32 { self.0 } }
impl TThree for YBoth {
    fn t1(&self) -> u32 { 1 }
    fn t2(&self) -> u32 { 2 }
    fn t3(&self) -> u32 { 3 }
}
pub struct MixedP(pub *const &'static dyn TOne);
pub const MIXED: MixedP = MixedP(
    &(&XOne(1) as &dyn TOne, &YBoth(2) as &dyn TThree) as *const (&dyn TOne, &dyn TThree)
        as *const &dyn TOne,
);
pub fn mixed() -> MixedP { MIXED }
pub trait Sub: TOne + TThree {}
impl Sub for YBoth {}
pub const UP: &dyn TOne = &YBoth(3) as &dyn Sub as &dyn TOne;
pub fn up() -> &'static dyn TOne { UP }
pub const ID_NUM: (&dyn Tr, P) =
    (&A(1) as &dyn Tr, P(&0x0600_0000usize as *const usize as *const u8));
pub fn id_num() -> (&'static dyn Tr, P) { ID_NUM }
"#;

/// What the IR of `name`, a function of [`CONSTANT_MEMORY`], asks for as data, each with a
/// description (a vtable's type), or the error building it stops with.
fn constant_datas(name: &str) -> Result<Vec<(crate::fragment::DataRequest, String)>, String> {
    let (db, frontend) = frontend(CONSTANT_MEMORY);
    blaze_query::db::attach(db, || {
        let root = frontend.root(function(db, name));
        frontend.with(|db| {
            let code = built(db, &root, crate::Target::for_fixtures(), false, Build::of(&spec()));
            match code {
                Ok(super::Built::Ir(pending)) => {
                    let datas = pending.wants.requests.datas;
                    let named = (datas.into_iter().enumerate())
                        .map(|(index, it)| {
                            let name = described(index, &it);
                            (it, name)
                        })
                        .collect();
                    Ok(named)
                }
                Ok(_) => Err("not IR".to_owned()),
                Err(e) => Err(e.to_string()),
            }
        })
    })
}

/// Each blob's relocated words, sorted: the offset, the data it points at and the addend.
fn relocated_words(datas: &[(crate::fragment::DataRequest, String)]) -> Vec<Vec<(u32, u32, i64)>> {
    use crate::fragment::DataRequest;
    (datas.iter())
        .filter_map(|it| match &it.0 {
            DataRequest::Blob { blob, .. } => {
                let mut it: Vec<_> =
                    blob.relocs.iter().map(|&(at, to, add)| (at, to.as_u32(), add)).collect();
                it.sort_unstable();
                Some(it)
            }
            _ => None,
        })
        .collect()
}

/// What the data request `index` is, for a test to find it and for a failure's message.
fn described(index: usize, data: &crate::fragment::DataRequest) -> String {
    use crate::fragment::DataRequest;
    match data {
        DataRequest::Blob { blob, .. } => {
            let (len, relocs, fns) = (blob.bytes.len(), &blob.relocs, &blob.fn_relocs);
            format!("{index}: {len} bytes, relocs {relocs:?}, functions {fns:?}")
        }
        DataRequest::Vtable { ty, blob, .. } => {
            format!("{index}: vtable of {:?}, {} bytes", ty.as_ref(), blob.bytes.len())
        }
        DataRequest::Static(_) => format!("{index}: a static"),
        _ => format!("{index}: other"),
    }
}

/// [`described`] of each.
fn all_described(datas: &[(crate::fragment::DataRequest, String)]) -> String {
    datas.iter().map(|it| &*it.1).collect::<Vec<_>>().join("\n")
}

/// The index of the vtable asked for of a type whose name has `of` in it.
fn vtable_of(datas: &[(crate::fragment::DataRequest, String)], of: &str) -> Option<u32> {
    let vtable = datas.iter().position(|it| it.1.contains("vtable of") && it.1.contains(of));
    vtable.map(|it| it as u32)
}

/// A pointer in memory behind a raw pointer cast to a type that says nothing of pointers
/// (`*const u8` from a `*const Vt`) is relocated by its provenance: the walk by type of `u8`
/// covers its first byte and must not hide the pointer there.
#[test]
fn a_pointer_behind_a_raw_pointer_of_another_type_is_relocated() {
    let datas = constant_datas("cast").unwrap();
    let words = relocated_words(&datas);
    // `CAST`'s own pointer, and the `&5` at offset 0 of the `Vt` it points to.
    assert!(
        words.iter().filter(|it| matches!(it[..], [(0, _, 0)])).count() >= 2,
        "the `&u32` behind the `*const u8` is not relocated: {words:?}"
    );
}

/// A vtable in memory reached first through a pointer of a type that does not say it is there
/// (`*const u8` from a `&&dyn Tr`, then the `&&dyn Tr`) points at the vtable const eval's id
/// names, `A`'s for `Tr`: the id names the trait as well as the type, as rustc's
/// `GlobalAlloc::VTable` does, whatever type reaches the word.
#[test]
fn a_vtable_reached_first_through_a_raw_pointer_points_at_its_trait() {
    let datas = constant_datas("two").unwrap_or_else(|e| panic!("`TWO` is refused: {e}"));
    let word = crate::Target::for_fixtures().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        points_at_vtable(&datas, &words, word, "A"),
        "`TWO`'s vtable is not relocated: {}",
        all_described(&datas)
    );
}

/// A vtable in memory reached only through a raw pointer of a type that says nothing of it
/// (`*const u8` from a `&&dyn Tr`) points at the vtable const eval's id names.
#[test]
fn a_vtable_reached_only_through_a_raw_pointer_points_at_its_trait() {
    let datas = constant_datas("raw_dyn").unwrap_or_else(|e| panic!("`RAW_DYN` is refused: {e}"));
    let word = crate::Target::for_fixtures().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        points_at_vtable(&datas, &words, word, "A"),
        "`RAW_DYN`'s vtable is not relocated: {}",
        all_described(&datas)
    );
}

/// The length of the vtable of a type whose name has `of` in it, from [`described`].
fn vtable_len(datas: &[(crate::fragment::DataRequest, String)], of: &str) -> Option<usize> {
    let index = vtable_of(datas, of)? as usize;
    let (_, rest) = datas[index].1.rsplit_once(", ")?;
    rest.strip_suffix(" bytes")?.parse().ok()
}

/// An array of a `&dyn TOne` and a `&dyn TThree` reached through a `*const &dyn TOne` gets
/// each element's own vtable: the walk behind the raw pointer reads every element as a
/// `&dyn TOne`, and `YBoth`'s vtable must still be the one for `TThree` (three methods, longer
/// than `XOne`'s for `TOne`), not one for `TOne`.
#[test]
fn a_raw_pointer_over_dyn_pointers_of_two_traits_keeps_each_ones_vtable() {
    let datas = datas_or("mixed").unwrap_or_else(|e| panic!("`MIXED` is refused: {e}"));
    let (one, three) = (vtable_len(&datas, "XOne"), vtable_len(&datas, "YBoth"));
    assert!(
        matches!((one, three), (Some(one), Some(three)) if three > one),
        "`YBoth`'s vtable is not the one for `TThree`: {}",
        all_described(&datas)
    );
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        points_at_vtable(&datas, &words, word, "XOne")
            && points_at_vtable(&datas, &words, 3 * word, "YBoth"),
        "`MIXED`'s vtables are not relocated: {}",
        all_described(&datas)
    );
}

/// A word equal to one of const eval's ids that has no provenance (a number behind a
/// `*const u8`, which no type says is a number) is refused, naming the word and the id's type:
/// every vtable and function const eval makes has provenance, so this one is a number or a
/// pointer made a number, and emitting it as either could be wrong.
#[test]
fn an_id_word_with_no_provenance_is_refused() {
    let e = match datas_or("id_num") {
        Ok(datas) => panic!("`ID_NUM` built: {}", all_described(&datas)),
        Err(e) => e,
    };
    assert!(e.contains("const eval's id of `A`") && e.contains("no provenance"), "{e}");
}

/// A `&dyn Sub` upcast to a `&dyn TOne` in a constant points at `YBoth`'s vtable for `TOne`:
/// const eval gives the upcast pointer the vtable of its concrete type for the target trait,
/// as rustc does, and the trait it names is the pointer's.
#[test]
fn an_upcast_constant_points_at_the_targets_vtable() {
    let datas = datas_or("up").unwrap_or_else(|e| panic!("`UP` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        points_at_vtable(&datas, &words, word, "YBoth"),
        "`UP`'s vtable is not relocated: {}",
        all_described(&datas)
    );
}

/// A `&dyn` inside the `dyn` tail of a struct (`&Wrap<dyn Tr>` holding a `Holder(&dyn Tr)`)
/// gets its vtable: the tail is walked as its concrete type, where the sized fields end at its
/// alignment.
#[test]
fn a_dyn_pointer_in_a_dyn_tail_points_at_its_vtable() {
    let datas = constant_datas("tail").unwrap();
    let vtable = vtable_of(&datas, "F")
        .unwrap_or_else(|| panic!("no vtable of `F`: {}", all_described(&datas)));
    let words = relocated_words(&datas);
    // The tail, a `Holder`, at 8: its data pointer and its vtable.
    assert!(
        words.iter().any(|it| matches!(it[..], [(8, _, 0), (16, to, 0)] if to == vtable)),
        "the `&dyn Tr` in the tail is not relocated whole: {words:?}"
    );
}

/// A reference to a struct whose tail is an alias that normalizes to `dyn`
/// (`&Wrap<<H as Has>::Obj>`) keeps its memory and points at its vtable.
#[test]
fn a_dyn_tail_named_by_an_alias_is_relocated() {
    let datas = constant_datas("alias_tail").unwrap();
    let vtable = vtable_of(&datas, "G")
        .unwrap_or_else(|| panic!("no vtable of `G`: {}", all_described(&datas)));
    let word = crate::Target::for_fixtures().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        words.iter().any(|it| matches!(it[..], [(0, _, 0), (at, to, 0)]
            if at == word && to == vtable)),
        "the `&Wrap<<H as Has>::Obj>` is not relocated whole: {words:?}"
    );
}

/// A closure's captures are walked as a struct's fields: a captured `&dyn Tr` in a constant
/// closure points at its value and its vtable.
#[test]
fn a_closures_captured_dyn_pointer_points_at_its_vtable() {
    let datas = constant_datas("closure").unwrap();
    let vtable = vtable_of(&datas, "K")
        .unwrap_or_else(|| panic!("no vtable of `K`: {}", all_described(&datas)));
    let word = crate::Target::for_fixtures().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        words.iter().any(|it| matches!(it[..], [(0, _, 0), (at, to, 0)]
            if at == word && to == vtable)),
        "the closure's `&dyn Tr` is not relocated whole: {words:?}"
    );
}

/// A pointer one past the end of a static is the static's, at its size.
#[test]
fn a_pointer_one_past_a_statics_end_points_into_it() {
    use crate::fragment::DataRequest;
    let datas = constant_datas("static_end").unwrap();
    let statics: Vec<u32> = (datas.iter().enumerate())
        .filter(|(_, it)| matches!(it.0, DataRequest::Static(_)))
        .map(|(index, _)| index as u32)
        .collect();
    let word = crate::Target::for_fixtures().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        words.iter().any(|it| matches!(it[..], [(0, a, 0), (at, b, 4)]
            if at == word && a == b && statics.contains(&a))),
        "`STATIC_END` does not point at `ENDS` and one past its end: {words:?}, statics {statics:?}"
    );
}

/// A pointer one past the end of memory const eval kept is that memory's, at its size, and
/// not the start of the memory after it.
#[test]
fn a_pointer_one_past_kept_memorys_end_points_into_it() {
    let datas = constant_datas("kept_end").unwrap();
    let word = crate::Target::for_fixtures().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        words.iter().any(|it| matches!(it[..], [(0, a, 0), (at, b, 4)] if at == word && a == b)),
        "`KEPT_END` does not point at `KEPT` and one past its end: {words:?}"
    );
}

/// A raw pointer to `dyn` to a static points at the static and at its vtable.
#[test]
fn a_raw_dyn_pointer_to_a_static_points_at_it_and_its_vtable() {
    use crate::fragment::DataRequest;
    let datas = constant_datas("to_static").unwrap();
    let vtable = vtable_of(&datas, "A").unwrap_or_else(|| panic!("no vtable of `A`"));
    let word = crate::Target::for_fixtures().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        words.iter().any(|it| matches!(it[..], [(0, a, 0), (at, to, 0)]
            if at == word && to == vtable
                && matches!(datas[a as usize].0, DataRequest::Static(_)))),
        "`TO_STATIC` is not relocated to `AA` and its vtable: {words:?}"
    );
}

/// A pointer with provenance to memory const eval did not keep (no type of the constant says
/// it is a pointer, so nothing it points to was kept: `[u8; 8]` transmuted from a `&u32`) is
/// an error, not const eval's address emitted as bytes.
#[test]
fn a_pointer_to_memory_not_kept_is_refused() {
    let e = match constant_datas("bytes") {
        Ok(datas) => panic!("`BYTES` built: {}", all_described(&datas)),
        Err(e) => e,
    };
    assert!(e.contains("dangling"), "{e}");
}

/// The relocations of the data asked for that point at a static.
fn static_targets(datas: &[(crate::fragment::DataRequest, String)]) -> Vec<u32> {
    use crate::fragment::DataRequest;
    (datas.iter().enumerate())
        .filter(|(_, it)| matches!(it.0, DataRequest::Static(_)))
        .map(|(index, _)| index as u32)
        .collect()
}

/// A reference one past the end of a static (`&ENDS.1`, a `&()` at its size) is to the static,
/// not to memory of nothing: const eval takes it as the static's, as rustc's provenance does.
#[test]
fn a_reference_one_past_a_statics_end_is_to_the_static() {
    let datas = constant_datas("end_ref").unwrap();
    let statics = static_targets(&datas);
    let words = relocated_words(&datas);
    assert!(
        words.iter().any(|it| matches!(it[..], [(0, a, 4)] if statics.contains(&a))),
        "`END_REF` does not point one past `ENDS`'s end: {words:?}, statics {statics:?}"
    );
}

/// A pointer one past the end of a static in memory behind a raw pointer of no pointer type
/// (`*const u8`) is to the static: const eval keeps what such a word points to by the static
/// it is in, one past its end included.
#[test]
fn a_pointer_one_past_a_statics_end_behind_a_raw_pointer_is_to_the_static() {
    let datas = constant_datas("deep").unwrap_or_else(|e| panic!("`DEEP` is refused: {e}"));
    let statics = static_targets(&datas);
    let words = relocated_words(&datas);
    assert!(
        words.iter().any(|it| matches!(it[..], [(0, a, 4)] if statics.contains(&a))),
        "`DEEP`'s pointer does not point one past `ENDS`'s end: {words:?}, statics {statics:?}"
    );
}

/// A pointer one past the end of a heap block whose size is its whole size class
/// (`End8`, 8 bytes) is that block's, at its size, and not the start of the next block, which
/// const eval places after a gap.
#[test]
fn a_pointer_one_past_a_full_blocks_end_is_not_the_next_block() {
    let datas = constant_datas("pair").unwrap();
    let word = crate::Target::for_fixtures().word() as u32;
    let words = relocated_words(&datas);
    // The fields may be laid out in any order: the `Range` is two words that point at one
    // block, at 0 and at its end.
    assert!(
        words.iter().any(|it| it.windows(2).any(|pair| matches!(pair, [(at, a, 0), (end, b, 8)]
            if end - at == word && a == b))),
        "`PAIR`'s end pointer is not one past its block's end: {words:?}"
    );
}

/// A pointer in the sized fields of a struct with an unsized tail (`&Prefix<[u8]>` holding a
/// `&u32`) is followed: const eval keeps what it points to, and the code points at it.
#[test]
fn a_pointer_before_an_unsized_tail_is_relocated() {
    let datas = constant_datas("prefix").unwrap_or_else(|e| panic!("`PREFIX` is refused: {e}"));
    let words = relocated_words(&datas);
    assert!(
        words.iter().filter(|it| matches!(it[..], [(0, _, 0)])).count() >= 2,
        "the `&u32` before the `[u8]` tail is not relocated: {words:?}"
    );
}

const LEAF: &str = r#"
pub fn leaf(x: u32) -> u32 { x }
"#;

/// A unit's code keeps frame pointers as its own toolchain says (`Toolchain::frame_pointers`),
/// whatever another toolchain selected in the same process between its check and its emit
/// says: one function in two units, one keeping frame pointers and one not, each selected and
/// prepared (its spec checked) before either is emitted, as a cross build's target and build
/// scripts are. Each one's code is the code it gets built alone.
///
/// LLVM's x86-64 leaf sets up `rbp` only when frame pointers are kept (Cranelift's x86-64 code
/// always does), and the fixture's crate is x86-64's, whatever the host.
#[cfg(feature = "llvm")]
#[test]
fn interleaved_units_keep_their_own_frame_pointers() {
    use blaze_codegen::backend::Prepare;
    let (db, frontend) = frontend(LEAF);
    let root = frontend.root(function(&db, "leaf"));
    let target = crate::Target::of("x86_64-unknown-linux-gnu").expect("x86-64's target");
    let pic = crate::isa_for(target).expect("a code generator").flags().is_pic();
    let with = |keep: bool| {
        let base = spec();
        let toolchain = Toolchain {
            target: target.triple().to_owned(),
            arch: blaze_codegen::Arch::X86_64,
            cpu: target.cpu().to_owned(),
            llvm: Some("test".to_owned()),
            frame_pointers: keep,
            relocation: if pic { Relocation::Pic } else { Relocation::Static },
            ..base.toolchain
        };
        Spec { toolchain, backend: Backend::Llvm { opt: 1 }, mir_inline: false, ..base }
    };
    let (keeps, omits) = (with(true), with(false));
    assert_ne!(
        unit(&keeps).instance.key(),
        unit(&omits).instance.key(),
        "the setting is in the unit's key"
    );
    let refs: Vec<blaze_codegen::Target> = frontend.with(|db| refs_of(db, (&root, false)));
    let backends = Backends::default();
    let prepare = |spec: &Spec| {
        super::toolchain::select(&spec.toolchain).expect("the toolchain");
        backends
            .prepare(&frontend, &unit(spec), spec, &root, &refs)
            .unwrap_or_else(|e| panic!("prepared: {e:?}"))
            .1
    };
    let emit = |spec: &Spec, prepared| {
        let emitted = backends.emit(&unit(spec), spec, prepared, &refs, None);
        emitted.unwrap_or_else(|e| panic!("emitted: {e:?}")).function.code
    };
    let alone_keeps = emit(&keeps, prepare(&keeps));
    let alone_omits = emit(&omits, prepare(&omits));
    // `push rbp; mov rbp, rsp`
    let frame = [0x55, 0x48, 0x89, 0xe5];
    assert!(alone_keeps.starts_with(&frame), "kept frame pointers set up rbp: {alone_keeps:02x?}");
    assert!(
        !alone_omits.starts_with(&frame),
        "omitted frame pointers set up none: {alone_omits:02x?}"
    );
    // Each order: the second toolchain is selected after the first unit's check, before its emit.
    let (first, second) = (prepare(&keeps), prepare(&omits));
    assert_eq!(
        emit(&keeps, first),
        alone_keeps,
        "the unit keeping frame pointers, emitted after the other's select"
    );
    assert_eq!(emit(&omits, second), alone_omits, "the unit omitting frame pointers");
    let (first, second) = (prepare(&omits), prepare(&keeps));
    assert_eq!(
        emit(&omits, first),
        alone_omits,
        "the unit omitting frame pointers, emitted after the other's select"
    );
    assert_eq!(emit(&keeps, second), alone_keeps, "the unit keeping frame pointers");
}

const SPIN: &str = r#"
pub fn spin(n: u32) -> u32 {
    let mut i = 0;
    loop {
        if i == n {
            return i;
        }
        i = i + 1;
    }
}
"#;

/// Fuel (#1605) at `backend`: a unit metered for fuel is keyed apart, its function calls
/// the fuel runtime by its symbols (kept as the template's own outside symbols) and is the
/// same code every time; with fuel off it names no fuel symbol and is the same code every
/// time too. That off leaves every key as it was is `Spec::settings`' construction (fuel is
/// hashed only when on, `blaze_codegen::settings`).
fn fuel_meters_a_loop_at(spec: &Spec) {
    use blaze_codegen::backend::Prepare;
    let (db, frontend) = frontend(SPIN);
    let root = frontend.root(function(&db, "spin"));
    let refs: Vec<blaze_codegen::Target> = frontend.with(|db| refs_of(db, (&root, false)));
    let backends = Backends::default();
    let emit = |spec: &Spec| {
        super::toolchain::select(&spec.toolchain).expect("the toolchain");
        let (shape, prepared) = backends
            .prepare(&frontend, &unit(spec), spec, &root, &refs)
            .unwrap_or_else(|e| panic!("prepared: {e:?}"));
        let emitted = backends.emit(&unit(spec), spec, prepared, &refs, None);
        (shape, emitted.unwrap_or_else(|e| panic!("emitted: {e:?}")))
    };
    let fuel = |emitted: &blaze_codegen::backend::Emitted| {
        let mut names: Vec<String> = emitted
            .function
            .relocs
            .targets()
            .iter()
            .filter_map(|it| match it {
                blaze_codegen::Target::Extern(name) if name.starts_with("__blaze_fuel_") => {
                    Some(name.clone())
                }
                _ => None,
            })
            .collect();
        names.sort();
        names
    };
    let on = Spec { fuel: true, ..spec.clone() };
    assert_ne!(unit(spec).instance.key(), unit(&on).instance.key(), "fuel is in the unit's key");
    let ((off_shape, off), (_, again)) = (emit(spec), emit(spec));
    assert_eq!(off.function.code, again.function.code, "unmetered code is the same every time");
    assert!(fuel(&off).is_empty() && off.extra.is_empty(), "unmetered code calls no fuel runtime");
    let ((on_shape, metered), (_, again)) = (emit(&on), emit(&on));
    assert_eq!(metered.function.code, again.function.code, "metered code is the same every time");
    assert_eq!(fuel(&metered), ["__blaze_fuel_cell", "__blaze_fuel_out"]);
    let mut extra = metered.extra.clone();
    extra.sort();
    assert_eq!(extra, ["__blaze_fuel_cell", "__blaze_fuel_out"], "the template keeps them");
    assert_ne!(on_shape, off_shape, "the shape covers the metered IR");
    assert_ne!(metered.function.code, off.function.code);
}

#[test]
fn fuel_meters_a_loop() {
    fuel_meters_a_loop_at(&spec());
}

#[cfg(feature = "llvm")]
#[test]
fn fuel_meters_a_loop_in_llvm() {
    let base = spec();
    let toolchain = Toolchain { llvm: Some("test".to_owned()), ..base.toolchain.clone() };
    fuel_meters_a_loop_at(&Spec { toolchain, backend: Backend::Llvm { opt: 0 }, ..base });
}

/// What `name` builds or the error it stops with, and a failure message's description.
fn datas_or(name: &str) -> Result<Vec<(crate::fragment::DataRequest, String)>, String> {
    constant_datas(name)
}

/// The data `index` of `datas` points at a vtable of a type whose name has `of` in it, at
/// `at`.
fn points_at_vtable(
    datas: &[(crate::fragment::DataRequest, String)],
    words: &[Vec<(u32, u32, i64)>],
    at: u32,
    of: &str,
) -> bool {
    let vtable = vtable_of(datas, of);
    words.iter().any(|it| it.iter().any(|&(here, to, _)| here == at && Some(to) == vtable))
}

/// One allocation reached by a slice and by a raw pointer into its middle is one data object:
/// the walks relocate the same words, in whatever order they find them.
#[test]
fn an_allocation_reached_from_its_middle_is_one_object() {
    let datas = datas_or("order").unwrap_or_else(|e| panic!("`ORDER` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    // The value: the slice at 0 and the raw pointer at 16, into one object, at 0 and a word.
    assert!(
        words.iter().any(|it| matches!(it[..], [(0, a, 0), (at, b, add)]
            if at == 2 * word && a == b && add == i64::from(word))),
        "`ORDER`'s pointers are not into one object: {}",
        all_described(&datas)
    );
}

/// An allocation whose fields are laid out out of their declared order (`y` first) and that
/// is reached by its type and by a raw pointer to a field is one object: the two walks find
/// its vtables in different orders, which are the same relocations.
#[test]
fn an_allocation_two_walks_find_in_different_orders_is_one_object() {
    let datas = datas_or("order_dyn").unwrap_or_else(|e| panic!("`ORDER_DYN` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        points_at_vtable(&datas, &words, word, "G")
            && points_at_vtable(&datas, &words, 3 * word, "F"),
        "`Reordered`'s vtables are not relocated: {}",
        all_described(&datas)
    );
}

/// A number a type says is one stays a number, though it equals one of const eval's ids.
#[test]
fn a_number_equal_to_an_id_is_a_number() {
    let datas = datas_or("num").unwrap_or_else(|e| panic!("`NUM` is refused: {e}"));
    assert!(
        datas.iter().all(|it| !it.1.contains("relocs [(0,")),
        "a number of `NUM` is relocated: {}",
        all_described(&datas)
    );
}

/// A struct whose tail is a slice of `&dyn` points at each element's vtable.
#[test]
fn a_slice_tails_dyn_pointers_point_at_their_vtables() {
    let datas = datas_or("sltail").unwrap_or_else(|e| panic!("`SLTAIL` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        points_at_vtable(&datas, &words, 2 * word, "F")
            && points_at_vtable(&datas, &words, 4 * word, "G"),
        "the slice tail's vtables are not relocated: {}",
        all_described(&datas)
    );
}

/// A raw pointer to the first of an array of `&dyn` (`as_ptr`) reaches every element: each
/// points at its vtable.
#[test]
fn a_raw_pointer_to_an_array_of_dyn_pointers_reaches_each() {
    let datas = datas_or("asptr").unwrap_or_else(|e| panic!("`ASPTR` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        points_at_vtable(&datas, &words, word, "F")
            && points_at_vtable(&datas, &words, 3 * word, "G"),
        "the array's vtables are not relocated: {}",
        all_described(&datas)
    );
}

/// A union whose one non-empty field is a `&dyn` (`MaybeUninit<&dyn Tr>`) points at its
/// vtable.
#[test]
fn a_union_of_one_dyn_pointer_points_at_its_vtable() {
    let datas = datas_or("muninit").unwrap_or_else(|e| panic!("`MUNINIT` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        points_at_vtable(&datas, &words, word, "K"),
        "`MUNINIT`'s vtable is not relocated: {}",
        all_described(&datas)
    );
}

/// A vtable in a union of two non-empty fields (`U2 { d: &dyn Tr }` beside a `(usize,
/// usize)`) points at the vtable const eval's id names: its provenance says which word it is
/// and the id which vtable, so no field has to be chosen.
#[test]
fn a_vtable_in_a_union_of_two_fields_points_at_its_vtable() {
    let datas = datas_or("u2_dyn").unwrap_or_else(|e| panic!("`U2_DYN` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        points_at_vtable(&datas, &words, word, "A"),
        "`U2_DYN`'s vtable is not relocated: {}",
        all_described(&datas)
    );
}

/// A union's fields are not walked by type: numbers in one (`U2 { n: (1, 2) }`) are no
/// `&dyn`'s, and no vtable is looked for.
#[test]
fn a_union_holding_numbers_is_not_walked_as_its_pointer_field() {
    datas_or("u2_num").unwrap_or_else(|e| panic!("`U2_NUM` is refused: {e}"));
}

/// The sized fields of an unsized struct inside a `repr(packed(2))` one are where the packed
/// layout puts them: the `&u32` at 2, then the `Holder`'s `&dyn` at 2 plus a word.
#[test]
fn a_packed_structs_unsized_field_is_at_its_capped_offset() {
    let datas = datas_or("pkpre").unwrap_or_else(|e| panic!("`PKPRE` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        words.iter().any(|it| it.iter().any(|&(at, _, _)| at == 2))
            && points_at_vtable(&datas, &words, 2 + 2 * word, "K"),
        "`PKPRE`'s fields are not relocated at the packed offsets: {}",
        all_described(&datas)
    );
}

/// A slice at the end of a packed struct's nested tail (`PkTail<In2<[&dyn Tr]>>`, packed to
/// 2) starts where the layout puts it, 2 + 8 = 10 bytes in as rustc has it: a slice's
/// alignment is its element's, so the layout's offsets already hold the cap `packed` puts on
/// it, and its `dyn` pointers' vtables are at 10 + a word and 10 + 3 words.
#[test]
fn a_slice_in_a_packed_structs_nested_tail_is_at_the_layouts_offset() {
    let datas = datas_or("pk_slice").unwrap_or_else(|e| panic!("`PK_SLICE` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    let tail = 2 + word;
    assert!(
        points_at_vtable(&datas, &words, tail + word, "F")
            && points_at_vtable(&datas, &words, tail + 3 * word, "G"),
        "`PK_SLICE`'s vtables are not at the slice's offset: {}",
        all_described(&datas)
    );
}

/// One allocation reached as numbers (`&(usize, usize)`, transmuted) and as a `&&dyn Tr`
/// points at its vtable: which walk reaches it first decides nothing, since the vtable is
/// relocated from its provenance and const eval's id.
#[test]
fn an_allocation_reached_as_numbers_and_as_a_dyn_pointer_points_at_its_vtable() {
    let datas = datas_or("two_views").unwrap_or_else(|e| panic!("`TWO_VIEWS` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        points_at_vtable(&datas, &words, word, "A"),
        "`TWO_VIEWS`'s vtable is not relocated: {}",
        all_described(&datas)
    );
}

/// A word equal to a function's id behind a raw pointer, with no provenance, is no function:
/// only the `fn` const eval made is relocated as one.
#[test]
fn a_number_equal_to_a_functions_id_behind_a_raw_pointer_is_no_function() {
    match datas_or("fn_num") {
        Ok(datas) => {
            let functions = datas.iter().filter(|it| it.1.contains("functions [(")).count();
            assert!(functions <= 1, "a number became a function: {}", all_described(&datas));
        }
        Err(e) => assert!(e.contains("const eval's id of"), "{e}"),
    }
}

/// A `DynMetadata` of an alias that normalizes to `dyn` points at its vtable.
#[test]
fn a_dyn_metadata_of_an_alias_points_at_its_vtable() {
    let datas = datas_or("meta").unwrap_or_else(|e| panic!("`META` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        points_at_vtable(&datas, &words, word, "F"),
        "`META`'s vtable is not relocated: {}",
        all_described(&datas)
    );
}

/// An empty slice into memory const eval kept points into it, in the middle and at its end,
/// not at its alignment.
#[test]
fn an_empty_slice_into_kept_memory_points_into_it() {
    let datas = datas_or("emp").unwrap_or_else(|e| panic!("`EMP` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        words.iter().any(|it| matches!(it[..], [(0, a, 0), (m, b, 1), (e, c, 3)]
            if m == 2 * word && e == 4 * word && a == b && b == c)),
        "`EMP`'s empty slices do not point into `e`: {}",
        all_described(&datas)
    );
}

/// A tuple whose tail is `dyn` keeps its memory, and the `&dyn` in its concrete value points
/// at its vtable.
#[test]
fn a_tuple_with_a_dyn_tail_is_relocated() {
    let datas = datas_or("tup").unwrap_or_else(|e| panic!("`TUP` is refused: {e}"));
    let word = crate::Target::for_host().word() as u32;
    let words = relocated_words(&datas);
    assert!(
        points_at_vtable(&datas, &words, 2 * word, "G"),
        "the tuple tail's `&dyn` is not relocated: {}",
        all_described(&datas)
    );
}

/// A reference to nothing (`&()`, `&[]`, or one past the end of memory const eval did not
/// keep, `&e.1`, whose provenance says it is a pointer) is its alignment, which the walk by
/// provenance leaves as it is: there is no memory to point it into.
#[test]
fn a_reference_to_nothing_is_its_alignment() {
    let datas = datas_or("nothing").unwrap_or_else(|e| panic!("`NOTHING` is refused: {e}"));
    let words = relocated_words(&datas);
    assert!(words.iter().all(|it| it.is_empty()), "`NOTHING` is relocated: {words:?}");
}

/// `k` depends on `dep` but names it only through `m`'s macro in a body, which
/// `loaded_crates` misses: the solver of `k`'s environment does not see `dep`'s impls, while
/// `main`, which names `dep`, does. `dep`'s impl of `t::Tr` specializes `t`'s `default`
/// blanket impl, so which `go` `k::run::<u8>` calls depends on the crate it is compiled in.
const MACRO_ONLY_DEP: &str = r#"
//- minicore: sized
//- /t.rs crate:t
#![feature(min_specialization)]
pub trait Tr {
    fn go(&self) -> u32;
}
impl<T> Tr for T {
    default fn go(&self) -> u32 { 1 }
}
//- /m.rs crate:m
#[macro_export]
macro_rules! val {
    () => {
        ::dep::DType
    };
}
//- /dep.rs crate:dep deps:t
#![feature(min_specialization)]
pub struct DType;
impl t::Tr for DType {
    fn go(&self) -> u32 { 2 }
}
//- /k.rs crate:k deps:t,m,dep
pub fn run<T>(_: T) -> u32 {
    t::Tr::go(&m::val!())
}
//- /main.rs crate:main deps:k,dep
pub fn roots() -> u32 {
    let _ = dep::DType;
    k::run(0u8)
}
"#;

/// `k::run::<u8>`, generic, which rustc instantiates in `main`, is compiled where `dep`'s
/// specializing impl is seen: `k` is no home of it asked from `main`
/// (`hir_ty_mir::mir::home::stands_for`), so its call of `go` on `DType` is `dep`'s. Compiled
/// in `k`, the one crate it names, it would call `t`'s default and return 1 where rustc's
/// returns 2 (rule 21: the home is in the instance's key, and `k` misses an input `main`'s
/// answer reads).
#[test]
fn an_instance_is_not_compiled_in_a_crate_that_misses_a_specializing_impl_its_asker_sees() {
    use base_db::{Crate, all_crates};
    use hir_def::HasModule;
    use hir_ty::method_resolution::loaded_crates;
    let (db, frontend) = frontend(MACRO_ONLY_DEP);
    let named = |name: &str| -> Crate {
        all_crates(db)
            .iter()
            .copied()
            .find(|it| {
                it.extra_data(db)
                    .display_name
                    .as_ref()
                    .is_some_and(|it| it.crate_name().as_str() == name)
            })
            .unwrap_or_else(|| panic!("no crate `{name}`"))
    };
    let (k, dep, main) = (named("k"), named("dep"), named("main"));
    blaze_query::db::attach(db, || {
        assert!(
            !loaded_crates(db, k).contains(&dep),
            "the fixture's premise: `k` names `dep` only through a macro"
        );
        assert!(loaded_crates(db, main).contains(&dep));
    });
    let root = frontend.root(hir_def::test_items::function(db, main, "roots"));
    let run = hir_def::test_items::function(db, k, "run");
    let called = frontend.with(|db| called(db, &root, run));
    let [run] = &called[..] else { panic!("one call of `k::run`: {called:?}") };
    let Kind::Code(_, env) = &run.0 else { panic!("{run:?}") };
    let go: Vec<Crate> = frontend.with(|db| {
        refs::walk(db, run, refs::Walking::plain(false))
            .expect("the walk of `k::run`")
            .iter()
            .filter_map(|it| match it {
                FrontRef::Function(HirInstance(Kind::Code(callee, _))) => match callee.def {
                    InstanceDef::Fn(func)
                        if FunctionSignature::of(db, func).name.as_str() == "go" =>
                    {
                        Some(func.krate(db))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    });
    assert_eq!(
        go,
        [dep],
        "`k::run` compiled in {:?} (`k` is {k:?}, `main` {main:?}) calls `go` of {go:?}",
        env.krate
    );
}

/// `generic::<u8>`, which `main` asks for and every unit may (walked for inlining, as an
/// `opt-level = 3` member inlines), builds in `always`, `#[inline(always)]` of `dep`, which no
/// MIR inliner takes. Built under the caller's settings, `always` has `leaf` MIR-inlined, as
/// `winnow`'s `<&str as Stream>::eof_offset` has `<str>::as_bytes` in a `LocatingSlice`
/// method; `dep`'s own units, at level 0, inline nothing.
fn built_in_inlined(leaf: u32) -> String {
    format!(
        r#"
//- minicore: sized, copy
//- /dep.rs crate:dep library
#[inline]
pub fn leaf(x: u32) -> u32 {{ x ^ {leaf} }}
#[inline(always)]
#[rustc_no_mir_inline]
pub fn always(x: u32) -> u32 {{ leaf(x) }}
pub fn generic<T>(x: u32) -> u32 {{ always(x) }}
//- /main.rs crate:main deps:dep
pub fn roots(x: u32) -> u32 {{ dep::generic::<u8>(x) }}
"#
    )
}

/// What the MIR inliner put in the body of a callee a caller builds in is the caller's code,
/// so the caller's template key holds it (rule 21): two `leaf`s are two keys. The key holds
/// the code of what the caller's walk lists as built in and, as the driver closes it, what
/// each of those lists as walked for its own units; `dep`'s units inline nothing, so only the
/// caller's walk can name `leaf`. Without it both programs' `generic::<u8>` had one key, the
/// key of `always` alone, and the build refused the compile that said it built `leaf` in.
#[test]
fn a_callers_key_holds_what_the_inliner_put_in_a_callee_it_builds_in() {
    use base_db::{Crate, all_crates};
    use blaze_codegen::backend::Frontend as _;
    let program = |leaf: u32| {
        let db: &'static TestDB =
            Box::leak(Box::new(fixture::db(&built_in_inlined(leaf)).into_db()));
        let dep = (all_crates(db).iter().copied())
            .find(|it: &Crate| {
                it.extra_data(db)
                    .display_name
                    .as_ref()
                    .is_some_and(|it| it.crate_name().as_str() == "dep")
            })
            .expect("the crate `dep`");
        let mut policy = policy(OptLevel::O3);
        policy.profile.packages.insert(
            "dep".to_owned(),
            Override { opt_level: Some(OptLevel::O0), ..Override::default() },
        );
        let hashes = super::ItemHashes::new(Box::new(|_| Some("src/lib.rs".to_owned())));
        let frontend =
            HirFrontend::new(Databases::new(db), HashSource::Items(hashes), BuildTime::default())
                .with_policy(&policy);
        let root = frontend.root(function(db, "roots"));
        let (generic, always) = (
            hir_def::test_items::function(db, dep, "generic"),
            hir_def::test_items::function(db, dep, "always"),
        );
        let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(db)));
        blaze_query::db::attach(db, || {
            let [caller] = &frontend.with(|hir| called(hir, &root, generic))[..] else {
                panic!("one `generic::<u8>`")
            };
            let described = |it: &HirInstance| {
                frontend.describe(it, &origin).unwrap_or_else(|e| panic!("{it:?}: {}", e.what))
            };
            // What the driver closes the key over: each body built in, by its own description.
            let (mut kept, mut queue, mut seen) = (Vec::new(), vec![caller.clone()], Vec::new());
            while let Some(at) = queue.pop() {
                let it = described(&at);
                for &place in &it.built_in {
                    let FrontRef::Function(callee) = &it.refs[place as usize] else {
                        panic!("{:?}", it.refs[place as usize])
                    };
                    if !seen.contains(callee) {
                        seen.push(callee.clone());
                        kept.push(described(callee).code);
                        queue.push(callee.clone());
                    }
                }
            }
            kept.sort_unstable();
            assert!(
                seen.iter().any(
                    |it| matches!(&it.0, Kind::Code(it, _) if it.def == InstanceDef::Fn(always))
                ),
                "the premise: `always` is built in: {seen:?}"
            );
            let walking = frontend.with(|hir| frontend.walking(hir, caller)).expect("the policy");
            assert!(
                walking.inlined,
                "the premise: a generic is walked for inlining where any unit inlines"
            );
            let inlined =
                frontend.with(|hir| inlined_by_build(hir, &frontend.hashes, caller, o3()));
            (kept, inlined)
        })
    };
    let (seven, nine) = (program(7), program(9));
    assert_ne!(seven.1, nine.1, "the premise: `leaf`'s body is in the caller's code");
    for (kept, inlined) in [&seven, &nine] {
        let unkept: Vec<_> = inlined.iter().filter(|it| !kept.contains(it)).collect();
        assert!(unkept.is_empty(), "built in, not in the key: {unkept:?} (the key holds {kept:?})");
    }
    assert_ne!(seven.0, nine.0, "two codes of `generic::<u8>` under one key");
}

/// `dep::resize::<K>`, `#[inline(always)]`, takes `hasher: &dyn Fn(u32) -> u32`, as hashbrown's
/// `RawTableInner::resize_inner` does, and `main`'s `roots` at `opt-level = 3` inlines it. `K`
/// is `main`'s, so `resize::<K>` is the workspace's, and its own unit is built against the
/// program's dyn facts.
const DYN_FN_BUILT_IN: &str = r#"
//- minicore: sized, copy, unsize, coerce_unsized, dispatch_from_dyn, fn
//- /dep.rs crate:dep library
#[inline(always)]
pub fn resize<T>(x: u32, hasher: &dyn Fn(u32) -> u32) -> u32 { hasher(x) + 1 }
//- /main.rs crate:main deps:dep
pub struct K;
pub fn roots(x: u32) -> u32 { dep::resize::<K>(x, &|y| y * 3) }
"#;

/// A callee whose own unit is built against the program's dyn facts, built into a caller,
/// keeps a key that holds what the caller built in. Its own code hashes the facts its body
/// read (`devirt::code`), but the copy built into the caller is built against none (the
/// inliner devirtualizes no callee), so the caller's compile names it by its built-in code.
/// A held body holds both ([`blaze_codegen::backend::Described::held_codes`]). Held by its own
/// code alone, the build refused hashbrown's `reserve_rehash_inner`, which built in
/// `resize_inner::<Global>` (sb-tplkey, Input 2).
#[test]
fn an_inlined_callee_that_takes_a_dyn_fn_keeps_a_matching_key() {
    use base_db::{Crate, all_crates};
    use blaze_codegen::{backend::Frontend as _, plan::Origin};
    let (db, frontend) = held_as(fixture::db(DYN_FN_BUILT_IN).into_db(), &policy(OptLevel::O3));
    let main = fixture::test_crate(db);
    let dep = (all_crates(db).iter().copied())
        .find(|it: &Crate| {
            it.extra_data(db)
                .display_name
                .as_ref()
                .is_some_and(|it| it.crate_name().as_str() == "dep")
        })
        .expect("the crate `dep`");
    let resize = hir_def::test_items::function(db, dep, "resize");
    let origin = frontend.with(|hir| frontend.origin(hir, main));
    let root = frontend.root(function(db, "roots"));
    blaze_query::db::attach(db, || {
        let described = |it: &HirInstance, by: &Origin| {
            frontend.describe(it, by).unwrap_or_else(|e| panic!("{it:?}: {}", e.what))
        };
        // The driver's first walk of the program: everything it reaches, then its dyn facts.
        frontend.devirt.makes(super::RootKind::Executable, main);
        let (mut reached, mut queue): (Vec<(HirInstance, Origin)>, _) =
            (Vec::new(), vec![(root.clone(), origin)]);
        while let Some((at, by)) = queue.pop() {
            if reached.iter().any(|(it, _)| *it == at) {
                continue;
            }
            let it = described(&at, &by);
            for it_ref in &it.refs {
                if let FrontRef::Function(callee) = it_ref {
                    queue.push((callee.clone(), it.origin));
                }
            }
            reached.push((at, by));
        }
        frontend.reached(&reached, &[]);
        assert!(
            frontend.closed().expect("the program's dyn facts"),
            "the premise: the program has a world"
        );
        let [callee] = &frontend.with(|hir| called(hir, &root, resize))[..] else {
            panic!("one `resize::<K>`")
        };
        let node = described(callee, &origin);
        assert_ne!(
            node.code, node.built_in_code,
            "the premise: `resize::<K>`'s own code read the program's dyn facts"
        );
        // What the driver closes the key over: each body built in, by its own description.
        let (mut kept, mut queue, mut seen) = (Vec::new(), vec![root.clone()], Vec::new());
        while let Some(at) = queue.pop() {
            let it = described(&at, &origin);
            for &place in &it.built_in {
                let FrontRef::Function(built_in) = &it.refs[place as usize] else {
                    panic!("{:?}", it.refs[place as usize])
                };
                if !seen.contains(built_in) {
                    seen.push(built_in.clone());
                    kept.extend(described(built_in, &origin).held_codes());
                    queue.push(built_in.clone());
                }
            }
        }
        assert!(seen.contains(callee), "the premise: `resize::<K>` is built in: {seen:?}");
        let walking = frontend.with(|hir| frontend.walking(hir, &root)).expect("the policy");
        let build = Build { world: walking.world, ..o3() };
        let inlined = frontend.with(|hir| inlined_by_build(hir, &frontend.hashes, &root, build));
        assert!(
            inlined.contains(&node.built_in_code),
            "the premise: `resize::<K>` is built in as itself: {inlined:?}"
        );
        let unkept: Vec<_> = inlined.iter().filter(|it| !kept.contains(it)).collect();
        assert!(unkept.is_empty(), "built in, not in the key: {unkept:?} (the key holds {kept:?})");
    });
}

/// `reads` costs the MIR inliner more than it takes where its UB checks are on, a call at
/// each `ub_checks()` branch, and less where they are off, which the cost model discounts
/// (rustc's `CostChecker` at `UbChecks`): a unit with debug assertions calls it, a unit
/// without (a `--release` one) inlines it, and its code then calls `leaf`, which only `reads`
/// calls.
const UB_CHECKED_CALLEE: &str = r#"
//- minicore: sized, copy
#[rustc_intrinsic]
pub const fn ub_checks() -> bool;
#[inline(never)]
pub fn precondition(x: u32) -> u32 { x }
#[inline(never)]
pub fn leaf(x: u32) -> u32 { x ^ 5 }
#[inline]
pub fn reads(a: u32, b: u32, c: u32, d: u32) -> u32 {
    if ub_checks() { precondition(a); }
    if ub_checks() { precondition(b); }
    if ub_checks() { precondition(c); }
    if ub_checks() { precondition(d); }
    leaf(a ^ b ^ c ^ d)
}
pub fn roots(a: u32, b: u32, c: u32, d: u32) -> u32 { reads(a, b, c, d) }
"#;

/// The walk lists what a unit without debug assertions asks for, though the walk with them
/// inlines less: what the MIR inliner takes is not monotone in them (`Build::WALKS_INLINED`).
/// Walked with them alone, `roots`' release unit asked for `leaf`, which its references did
/// not list, as `blaze-mem`'s `os::reserve` at `--release` asked for core's `runtime`.
#[test]
fn a_walk_lists_what_a_unit_without_debug_assertions_inlines() {
    let (db, frontend) = frontend(UB_CHECKED_CALLEE);
    let root = frontend.root(function(db, "roots"));
    let (reads, leaf) = (function(db, "reads"), function(db, "leaf"));
    frontend.with(|db| {
        let Kind::Code(instance, env) = &root.0 else { panic!("{root:?}") };
        let inlines = |build: Build| {
            crate::instance_body(db, instance, env.clone(), Some(build))
                .unwrap_or_else(|e| panic!("{root:?} as {build:?}: {e}"))
                .inlined
                .callees
                .iter()
                .any(|it| it.body == hir_ty_mir::mir::InlinedBody::Fn(reads))
        };
        let release = Build { debug_assertions: false, ..o3() };
        assert!(!inlines(Build::WALK_INLINED), "the premise: with UB checks, `reads` stays a call");
        assert!(inlines(release), "the premise: without them, `reads` is inlined");
        let walking = refs::Walking::plain(true);
        let walked = refs::walk(db, &root, walking).expect("the walk");
        assert!(walked.iter().any(|it| is_call(it, leaf)), "`leaf` is listed: {walked:?}");
        let mut code = built(db, &root, crate::Target::for_fixtures(), false, release)
            .unwrap_or_else(|e| panic!("{root:?}: {e}"));
        let super::Built::Ir(pending) = &mut code else { panic!("asm") };
        let mut placer = Placer::listed(walked);
        place_requests(
            db,
            &HashSource::ById(ById),
            compiled_in(db, &root),
            &mut placer,
            &mut pending.wants.requests,
        )
        .expect("the release unit's IR asks only for what the walk lists");
    });
}

/// A copy of a body built into its caller is built under one rule, whichever side builds it
/// in: the code generator's always-inliner asks under `Build::built_in`, `hir-ty-mir`'s
/// inliner under `MirOptions::built_in`, and for every unit's settings the two give one set
/// of options, which read no dyn facts and move no allocation to the stack.
#[test]
fn a_copy_built_in_is_built_under_one_rule() {
    use hir_ty_mir::mir::devirt::DevirtWorld;
    let (db, frontend) = frontend(TWICE);
    let krate = fixture::test_crate(db);
    frontend.with(|db| {
        for bits in 0..64u32 {
            let bit = |at: u32| bits & (1 << at) != 0;
            for world in [None, Some(DevirtWorld([7; 32]))] {
                let build = Build {
                    debug_assertions: bit(0),
                    aborts: bit(1),
                    optimized: bit(2),
                    inlines: bit(3),
                    bridged: bit(4),
                    heap_to_stack: bit(5),
                    world,
                };
                for generic in [false, true] {
                    let copy = build.built_in().options(db, krate, generic);
                    assert_eq!(copy, build.options(db, krate, generic).built_in(), "{build:?}");
                    assert_eq!((copy.world, copy.heap_to_stack), (None, false), "{build:?}");
                }
            }
        }
    });
}

/// `lib::g::<D>` makes a `&dyn Debug` of `main`'s `D`, whose `Debug` is a builtin derive or a
/// written impl.
fn downstream_debug(derived: bool) -> String {
    let d = match derived {
        true => "#[derive(Debug)]\npub struct D;\n",
        false => {
            "pub struct D;\nimpl core::fmt::Debug for D {\n    \
                  fn fmt(&self, _: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { Ok(()) }\n}\n"
        }
    };
    format!(
        r#"
//- minicore: sized, coerce_unsized, dispatch_from_dyn, fmt, derive
//- /lib.rs crate:lib library
pub fn g<T: core::fmt::Debug>(t: &T) -> &dyn core::fmt::Debug {{ t }}
//- /main.rs crate:main deps:lib
{d}pub fn roots() {{ let _ = lib::g(&D); }}
"#
    )
}

/// sb-testkey's soundness argument for generic bodies (rule 23): an item's bound hash takes
/// the impls of an object's trait only from the crates its own crate loads, so `lib::g`'s
/// says nothing of `main`'s `impl Debug for D`. What covers it is the instance: the vtable
/// `g::<D>` makes is among its refs (its slots through `callee`), which `exec::bound` binds by
/// item. So swapping `D`'s derive for a written impl changes `g::<D>`'s exec bound through its
/// refs alone, with its own description held as it was.
#[test]
fn a_downstream_impl_changes_its_generic_instances_exec_bound() {
    use base_db::{Crate, all_crates};
    use blaze_codegen::{InstanceKey, Target, backend::Frontend as _};
    let program = |derived: bool| {
        let mut db = fixture::db(&downstream_debug(derived)).into_db();
        // As a build's database: builtin derives expanded, so `D`'s derived `Debug::fmt` has a body.
        hir_def::nameres::set_expand_builtin_derives(&mut db, true);
        let db: &'static TestDB = Box::leak(Box::new(db));
        let lib = (all_crates(db).iter().copied())
            .find(|it: &Crate| {
                it.extra_data(db)
                    .display_name
                    .as_ref()
                    .is_some_and(|it| it.crate_name().as_str() == "lib")
            })
            .expect("the crate `lib`");
        let hashes = super::ItemHashes::new(Box::new(|_| Some("src/lib.rs".to_owned())));
        let frontend =
            HirFrontend::new(Databases::new(db), HashSource::Items(hashes), BuildTime::default())
                .with_policy(&policy(OptLevel::O0));
        let root = frontend.root(function(db, "roots"));
        let g = hir_def::test_items::function(db, lib, "g");
        let origin = frontend.with(|hir| frontend.origin(hir, fixture::test_crate(db)));
        blaze_query::db::attach(db, || {
            let [instance] = &frontend.with(|hir| called(hir, &root, g))[..] else {
                panic!("one `g::<D>`")
            };
            let described = |it: &HirInstance| {
                frontend.describe(it, &origin).unwrap_or_else(|e| panic!("{it:?}: {}", e.what))
            };
            let it = described(instance);
            // Each ref as the driver gives it to `exec::bound`: a function by a key of its
            // place, which binds to its item. A static or a symbol is the same in both.
            let mut items = Vec::new();
            let targets: Vec<Target> = (it.refs.iter().enumerate())
                .map(|(at, front)| match front {
                    FrontRef::Function(callee) => {
                        let key =
                            InstanceKey(ContentId::of("test ref", &(at as u64).to_le_bytes()));
                        items.push((key, described(callee).item));
                        Target::Function(key)
                    }
                    FrontRef::Static(_) => Target::Extern(format!("static{at}")),
                    FrontRef::Extern(name) => Target::Extern(name.clone()),
                    FrontRef::Weak(name) => Target::Weak(name.clone()),
                })
                .collect();
            (it.bound, targets, items)
        })
    };
    let exec = |own: &Result<ContentId, String>,
                targets: &[Target],
                items: &[(InstanceKey, ItemKey)]| {
        let item_of =
            |key: &InstanceKey| items.iter().find(|(it, _)| it == key).map(|(_, item)| *item);
        blaze_codegen::exec::bound(own, targets, item_of).unwrap_or_else(|e| panic!("g::<D>: {e}"))
    };
    let (derived_own, derived_refs, derived_items) = program(true);
    let (written_own, written_refs, written_items) = program(false);
    assert!(derived_own.is_ok(), "the premise: `lib::g` has a bound hash: {derived_own:?}");
    let derived = exec(&derived_own, &derived_refs, &derived_items);
    assert_ne!(
        derived,
        exec(&written_own, &written_refs, &written_items),
        "a derive swapped for a written impl leaves `g::<D>`'s exec bound"
    );
    assert_ne!(
        derived,
        exec(&derived_own, &written_refs, &written_items),
        "under one description, the written impl's refs alone change the bound"
    );
}

/// Two functions of a wasm32 crate that return a pair: of 4 and 4 bytes, and of 1 and 4.
const WASM32_PAIRS: &str = r#"
//- target_data_layout: e-m:e-p:32:32-p10:8:8-p20:8:8-i64:64-i128:128-n32:64-S128-ni:1:10:20
//- target_arch: wasm32
//- minicore: sized, copy
//- /main.rs
pub fn pair(x: u32) -> (u32, u32) { (x, x ^ 85) }
pub fn mixed(x: u8) -> (u8, u32) { (x, 7) }
pub fn copied(x: &[u32; 6]) -> [u32; 6] { *x }
"#;

/// A wasm32 function that returns a pair returns it as LLVM's code of rustc's does, whatever
/// blaze was built with: its IR returns the two values (rustc's `PassMode::Pair`), and its wasm
/// takes the pointer they are written through ahead of its parameters and has no result.
/// rustc and LLVM of the pinned nightly (2026-10-27, `--target wasm32-wasip1 -O`) give both
/// `(func (param i32 i32))`, writing the first value at 0 and the second at 4 (`i32.store8`
/// and `i32.store offset=4` for `mixed`): LLVM demotes a return of more than one value
/// (`WebAssembly::canLowerReturn`, multivalue returns being only for the `experimental-mv`
/// ABI). Run with and without hir-ty-codegen's `wasm` feature: that build once returned a pair
/// through a pointer of the Rust ABI's own (`one_result`), so the IR differed by how blaze was
/// built.
#[test]
fn a_wasm32_pair_is_returned_as_llvm_returns_it_whatever_blaze_was_built_with() {
    use crate::wasm::{Address, WasmSignature, WasmType};
    use cranelift_codegen::ir::{ArgumentPurpose, types};
    let (db, frontend) = frontend(WASM32_PAIRS);
    let target = crate::Target::of("wasm32-wasip1").expect("wasm32's target");
    for (name, first) in [("pair", types::I32), ("mixed", types::I8)] {
        let root = frontend.root(function(db, name));
        let code = frontend
            .with(|db| built(db, &root, target, false, Build::of(&spec())))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let super::Built::Ir(pending) = &code else { panic!("{name}: assembled") };
        let signature = &pending.func().signature;
        let returned: Vec<_> = signature.returns.iter().map(|it| it.value_type).collect();
        assert_eq!(returned, [first, types::I32], "{name}: the IR returns the pair's two values");
        assert!(
            !signature.params.iter().any(|it| it.purpose == ArgumentPurpose::StructReturn),
            "{name}: the IR returns the pair through a pointer of its own: {signature}"
        );
        let (lowered, _) =
            crate::wasm::lower(pending.func(), crate::Calls::Direct, Address::of(target))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        let llvms = WasmSignature { params: vec![WasmType::I32, WasmType::I32], results: vec![] };
        assert_eq!(lowered.signature, llvms, "{name}: the wasm signature LLVM gives");
    }
    // A copy of 24 bytes, a `memcpy` the build verifies before the copy lowering makes loads
    // and stores of it (`crate::verified`), and again after: both pass on wasm32. The IR of
    // dfgalias's bug, refused, is `ir_the_verifier_refuses_is_the_functions_error_before_any_pass`.
    let copied = frontend.root(function(db, "copied"));
    frontend
        .with(|db| built(db, &copied, target, false, Build::of(&spec())))
        .unwrap_or_else(|e| panic!("copied: {e}"));
}
