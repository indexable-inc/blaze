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
use hir_ty::db::HirDatabase;
use hir_ty_mir::mir::MirSpan;
use hir_expand::fixture;

use super::{
    Backends, BuildTime, Databases, HashSource, HirFrontend, HirInstance, Kind,
    allocator::{self, Forward},
    by_id::ById,
    backend::{built, compiled_in},
    build::{self, Build},
    places::{self, Placer, place_requests},
    refs,
};
use crate::{InstanceDef, home::Homes};
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
        HirFrontend::new(Databases::new(db), HashSource::ById(ById), std::sync::Arc::new(BuildTime(Default::default())));
    (db, frontend.with_policy(policy))
}

/// A build at `level` of members and dependencies, whose units are as [`spec`]'s.
fn policy(level: OptLevel) -> Policy {
    let toolchain = Toolchain { llvm: Some("test".to_owned()), ..spec().toolchain };
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
        },
        mode: Mode::Output,
        fold: false,
        sanitize: Sanitize::default(),
        pgo: blaze_codegen::settings::Pgo::None,
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
    let target = crate::Target::selected().expect("a target");
    let arch = match target.arch {
        crate::Arch::X86_64 => blaze_codegen::Arch::X86_64,
        crate::Arch::Aarch64 => blaze_codegen::Arch::Aarch64,
        crate::Arch::Wasm32 => blaze_codegen::Arch::Wasm32,
    };
    let pic = crate::target().expect("a code generator").flags().is_pic();
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
            frame_pointers: crate::backend::frame_pointers(),
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
    }
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
fn called(
    db: &Db,
    homes: &mut Homes,
    root: &HirInstance,
    of: FunctionId,
) -> Vec<HirInstance> {
    let is_of = |instance: &HirInstance| {
        matches!(&instance.0, Kind::Code(called, _) if called.def == InstanceDef::Fn(of))
    };
    refs::walk(db, root, false, homes)
        .expect("the walk of the root")
        .into_iter()
        .filter_map(|it| match it {
            FrontRef::Function(instance) if is_of(&instance) => Some(instance),
            _ => None,
        })
        .collect()
}

/// What a backend is given for the references of an instance: one target each.
fn refs_of(
    db: &Db,
    homes: &mut Homes,
    (instance, inlined): (&HirInstance, bool),
) -> Vec<blaze_codegen::Target> {
    let walked = refs::walk(db, instance, inlined, homes).expect("the walk");
    (0..walked.len()).map(|at| blaze_codegen::Target::Extern(format!("ref{at}"))).collect()
}

fn shape(frontend: &HirFrontend, instance: &HirInstance) -> Shape {
    prepared(frontend, &spec(), instance).0
}

fn prepared(frontend: &HirFrontend, spec: &Spec, instance: &HirInstance) -> (Shape, super::Prepared) {
    frontend.with(|db, homes| {
        let refs = refs_of(db, homes, (instance, inlines(spec)));
        let refs: &Refs = &refs;
        let walked = (instance, inlines(spec));
        Backends::default()
            .prepared(db, homes, &frontend.hashes, &unit(spec), spec, walked, refs)
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
    let siblings = frontend.with(|db, homes| called(db, homes, &root, outer));
    let [a, b] = &siblings[..] else { panic!("{siblings:?}") };
    assert_ne!(a, b);
    assert_eq!(shape(&frontend, a), shape(&frontend, b));
}

#[test]
fn different_constants_are_different_shapes() {
    let (db, frontend) = frontend(SIBLINGS);
    let (roots, konst) = (function(&db, "roots"), function(&db, "konst"));
    let root = frontend.root(roots);
    let siblings = frontend.with(|db, homes| called(db, homes, &root, konst));
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
/// (`id::<&A>`, `id::<&B>`) have one share key in their arguments' place, so they are one
/// instance: one key, which the build compiles once, and one shape. `tid::<&A>` and
/// `tid::<&B>` read `type_id`, which tells them apart, and stay two.
#[test]
fn instances_with_one_share_key_are_one() {
    use blaze_codegen::backend::Frontend as _;
    let (db, frontend) = frontend(SHARED);
    // A failure's message formats instances, whose types are the database's.
    hir_ty::attach_db(db, || {
        let policy = policy(OptLevel::O0);
        let root = frontend.root(function(db, "roots"));
        let origin = frontend.with(|hir, _| frontend.origin(hir, fixture::test_crate(&db)));
        let key = |instance: &HirInstance| {
            let described = (frontend.describe(instance, &origin))
                .unwrap_or_else(|e| panic!("{instance:?}: {}", e.what));
            let choice = policy.choose(&described.origin, described.overflow_checks).expect("the plan");
            let settings = choice.first.settings();
            UnitInstance { code: described.code, args: described.args, settings }.key()
        };
        let of = |name| {
            let func = function(db, name);
            frontend.with(|hir, homes| called(hir, homes, &root, func))
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

/// A cached function writes the hash of all its closure reaches into its own code, so an
/// edit of a function the closure reaches, which leaves the cached function's body, its
/// closure's and every interface as they were, still changes the cached function's code.
#[test]
fn a_cached_function_is_new_code_when_what_its_closure_reaches_is() {
    use blaze_codegen::backend::Frontend as _;
    let code = |deep: &str| {
        let (db, frontend) = frontend(&cached_fixture(deep));
        hir_ty::attach_db(db, || {
            let root = frontend.root(function(db, "cached"));
            let origin = frontend.with(|hir, _| frontend.origin(hir, fixture::test_crate(&db)));
            let described = (frontend.describe(&root, &origin))
                .unwrap_or_else(|e| panic!("{root:?}: {}", e.what));
            described.code
        })
    };
    assert_eq!(code("x ^ 1"), code("x ^ 1"));
    assert_ne!(code("x ^ 1"), code("x ^ 2"));
}

#[test]
fn a_root_takes_function_items_for_its_type_parameters() {
    use blaze_codegen::backend::Frontend as _;
    let src = "pub fn expand<'a, F>(_: &'a u8) {}\npub fn two<F, G>() {}\npub fn sized<const N: usize, F>() {}\npub fn mac() {}\npub fn other() {}\n";
    let (db, frontend) = frontend(src);
    hir_ty::attach_db(db, || {
        let [expand, two, sized, mac, other] =
            ["expand", "two", "sized", "mac", "other"].map(|name| function(db, name));
        let root = |func, items: &[_]| frontend.root_with_fn_items(func, items);
        let with_mac = root(expand, &[mac]).unwrap();
        assert_ne!(with_mac, root(expand, &[other]).unwrap());
        assert_eq!(with_mac, root(expand, &[mac]).unwrap());
        let origin = frontend.with(|hir, _| frontend.origin(hir, fixture::test_crate(&db)));
        frontend.describe(&with_mac, &origin).unwrap_or_else(|e| panic!("{with_mac:?}: {}", e.what));
        for (func, items, name) in [(two, &[mac][..], "two"), (expand, &[], "expand"), (sized, &[mac], "sized")] {
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
    let origin = frontend.with(|hir, _| frontend.origin(hir, fixture::test_crate(&db)));
    let options = |policy: &Policy| {
        let build_time = std::sync::Arc::new(BuildTime(Default::default()));
        let frontend =
            HirFrontend::new(Databases::new(db), HashSource::ById(ById), build_time).with_policy(policy);
        frontend.body_options(&origin).expect("the options")
    };
    let at = |level, ub_checks, inline| MirOptions { level, ub_checks, lifetime_markers: true, inline };
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
        HirFrontend::new(Databases::new(db), HashSource::ById(ById), std::sync::Arc::new(BuildTime(Default::default())));
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
    frontend.with(|db, homes| {
        let mut seen = vec![root.clone()];
        let mut at = 0;
        while let Some(instance) = seen.get(at).cloned() {
            at += 1;
            let walked = refs::walk(db, &instance, false, homes).expect("the walk");
            assert_eq!(walked, refs::walk(db, &instance, false, homes).expect("the walk again"));
            let mut code = build::with(Build::of(&spec), || built(db, &instance, false))
                .and_then(|it| it)
                .unwrap_or_else(|e| panic!("{instance:?}: {e}"));
            let requests = match &mut code {
                super::Built::Ir(pending) => &mut pending.wants.requests,
                super::Built::Assembled(_, requests) => requests,
            };
            let mut placer = Placer::listed(walked.clone());
            place_requests(db, homes, &HashSource::ById(ById), compiled_in(db, &instance), &mut placer, requests)
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
    frontend.with(|db, homes| {
        let mut code = build::with(Build::of(&spec), || built(db, &root, false))
            .and_then(|it| it)
            .expect("the IR");
        let super::Built::Ir(pending) = &mut code else { panic!("asm") };
        let mut placer = Placer::listed(Vec::new());
        let krate = compiled_in(db, &root);
        let placed = place_requests(db, homes, &HashSource::ById(ById), krate, &mut placer, &mut pending.wants.requests);
        assert!(placed.is_err());
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
    frontend.with(|db, homes| {
        let walked = refs::walk(db, &root, false, homes).expect("the walk");
        let built_in = refs::built_in(db, &root, false).expect("the walk");
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
        let inlined =
            super::frontend::inlined(db, &HashSource::ById(ById), (&root, false)).expect("what is built in");
        assert_eq!(inlined.len(), 2, "outer and twice: {inlined:?}");
        let mut code = build::with(Build::of(&spec), || built(db, &root, false))
            .and_then(|it| it)
            .unwrap_or_else(|e| panic!("{root:?}: {e}"));
        let super::Built::Ir(pending) = &mut code else { panic!("asm") };
        let mut placer = Placer::listed(walked);
        place_requests(db, homes, &HashSource::ById(ById), compiled_in(db, &root), &mut placer, &mut pending.wants.requests)
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

/// An `asm!` function is its asker's for the dependency image. The one a dependency's generic
/// asks for, compiled as a member asks (core's `__isb::<SY>` for build-time `blaze-spin`), is
/// the image's, or the image would refer to the workspace's; the member's own is the
/// workspace's. Both are one function of one key, which the build gives the image.
#[test]
fn an_asm_function_a_dependencys_generic_asks_for_is_the_images() {
    use blaze_codegen::{Owner, backend::Frontend as _};
    let (db, frontend) = frontend(ASKED_BY_THE_IMAGE);
    let root = frontend.root(function(&db, "roots"));
    let origin = frontend.with(|hir, _| frontend.origin(hir, fixture::test_crate(&db)));
    hir_ty::attach_db(db, || {
        let describe = |instance: &HirInstance| {
            frontend.describe(instance, &origin).unwrap_or_else(|e| panic!("{instance:?}: {}", e.what))
        };
        let asm = |refs: &[FrontRef<HirInstance, super::HirStatic>]| -> Vec<HirInstance> {
            (refs.iter())
                .filter_map(|it| match it {
                    FrontRef::Function(instance @ HirInstance(Kind::Asm(..))) => Some(instance.clone()),
                    _ => None,
                })
                .collect()
        };
        let rooted = describe(&root);
        assert_eq!(rooted.owner, Owner::Workspace);
        let fence = (rooted.refs.iter()).find_map(|it| match it {
            FrontRef::Function(instance @ HirInstance(Kind::Code(called, _))) => match called.def {
                InstanceDef::Fn(func) if FunctionSignature::of(db, func).name.as_str() == "fence" => {
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
    let called = frontend.with(|db, homes| called(db, homes, &root, takes));
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
    let level = |spec: &Spec| Build::of(spec).options(db, krate, krate).level;
    let mut spec = spec();
    assert_eq!(level(&spec), MirOptLevel::Debug);
    spec.backend = Backend::Llvm { opt: 0 };
    assert_eq!(level(&spec), MirOptLevel::Debug);
    spec.backend = Backend::Llvm { opt: 2 };
    assert_eq!(level(&spec), MirOptLevel::Release);
    assert_eq!(Build::WALK.options(db, krate, krate).level, MirOptLevel::Debug);
    spec.debug_assertions = true;
    assert!(Build::of(&spec).options(db, krate, krate).ub_checks);
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
    frontend.with(|db, homes| {
        let walked = refs::walk(db, &ours, false, homes).expect("the walk");
        assert_eq!(walked, vec![FrontRef::Extern("__blaze_global_alloc".to_owned())]);
        let mut code = build::with(Build::of(&spec), || built(db, &ours, false))
            .and_then(|it| it)
            .expect("the IR");
        let super::Built::Ir(pending) = &mut code else { panic!("asm") };
        let mut placer = Placer::listed(walked);
        place_requests(db, homes, &HashSource::ById(ById), compiled_in(db, &ours), &mut placer, &mut pending.wants.requests)
            .expect("what the walk lists");
    });
    // The fixture links no `alloc`: its program has no shim.
    assert!(frontend.allocator_shim(krate).expect("the shim").is_empty());
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
    let closure = frontend.with(|db, homes| {
        refs::walk(db, &root("closure"), false, homes)
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
    let (quiet, loud) = (frontend.root(function(&db, "quiet")), frontend.root(function(&db, "loud")));
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
    let refs = frontend.with(|db, homes| refs_of(db, homes, (&loud, false)));
    let refs: &Refs = &refs;
    for spec in [sanitizing(Sanitizers::ADDRESS, 0), sanitizing(Sanitizers::NONE, 1)] {
        let Err(e) = Backends::default().prepare(&frontend, &unit(&spec), &spec, &loud, refs) else {
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
    build::with(Build::of(spec), || built(db, instance, false))
        .and_then(|it| it)
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
    let inline = |build: Build, written| build.options(db, written, krate).inline;
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
    let generic = frontend.with(|db, homes| called(db, homes, &root, outer)).remove(0);
    let inlines = |policy: &Policy, instance: &HirInstance| {
        let build_time = std::sync::Arc::new(BuildTime(Default::default()));
        let frontend =
            HirFrontend::new(Databases::new(db), HashSource::ById(ById), build_time).with_policy(policy);
        frontend.with(|db, _| frontend.inlines(db, instance)).expect("the policy")
    };
    assert!(!inlines(&policy(OptLevel::O0), &root));
    assert!(inlines(&policy(OptLevel::O3), &root));
    assert!(inlines(&policy(OptLevel::O2), &root));
    assert!(!inlines(&policy(OptLevel::O1), &root));
    let mut set_apart = policy(OptLevel::O3);
    let name = frontend.with(|hir, _| match frontend.origin(hir, fixture::test_crate(&db)) {
        blaze_codegen::plan::Origin::Member(name) => name.as_str().to_owned(),
        other => panic!("the fixture's crate is {other:?}"),
    });
    let o0 = Override { opt_level: Some(OptLevel::O0), ..Override::default() };
    set_apart.profile.packages.insert(name, o0);
    assert!(!inlines(&set_apart, &root));
    // Other members and the library's crates still inline, and any of them may ask for it.
    assert!(inlines(&set_apart, &generic));
    let unwalked =
        HirFrontend::new(Databases::new(db), HashSource::ById(ById), std::sync::Arc::new(BuildTime(Default::default())));
    let refused = unwalked.with(|db, _| unwalked.inlines(db, &root));
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
    frontend.with(|db, homes| {
        assert!(frontend.inlines(db, &root).expect("the policy"));
        let plain = refs::walk(db, &root, false, homes).expect("the walk");
        let walked = refs::walk(db, &root, true, homes).expect("the walk for inlining");
        assert!(plain.iter().any(|it| is_call(it, f)), "{plain:?}");
        assert!(!plain.iter().any(|it| is_call(it, g)), "{plain:?}");
        assert!(walked.iter().any(|it| is_call(it, g)), "{walked:?}");
        assert_eq!(walked[..plain.len()], plain[..]);
        let built_in = refs::built_in(db, &root, true).expect("the walk");
        assert!(built_in.iter().any(|&at| is_call(&walked[at as usize], f)), "{built_in:?}");
        assert!(refs::built_in(db, &root, false).expect("the walk").is_empty());
        let krate = compiled_in(db, &root);
        let mut place = |refs: Vec<places::Ref>| {
            let mut code = built_as(db, &llvm(2), &root);
            let super::Built::Ir(pending) = &mut code else { panic!("asm") };
            let mut placer = Placer::listed(refs);
            place_requests(db, homes, &HashSource::ById(ById), krate, &mut placer, &mut pending.wants.requests)
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
    frontend.with(|db, homes| {
        let walked = refs::walk(db, &root, true, homes).expect("the walk");
        let built_in = refs::built_in(db, &root, true).expect("the walk");
        assert_eq!(walked.iter().filter(|it| is_call(it, leaf)).count(), 1, "{walked:?}");
        let [one, two] = built_in[..] else { panic!("built in: {built_in:?} of {walked:?}") };
        assert_ne!(one, two);
        let of = |func| built_in.iter().any(|&at| is_call(&walked[at as usize], func));
        assert!(of(leaf) && of(mid), "{built_in:?} of {walked:?}");
        let inlined = super::frontend::inlined(db, &HashSource::ById(ById), (&root, true)).expect("the code");
        assert_eq!(inlined.len(), 2, "leaf and mid: {inlined:?}");
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
    frontend.with(|db, homes| {
        let walked = refs::walk(db, &root, true, homes).expect("the walk");
        let built_in = refs::built_in(db, &root, true).expect("the walk");
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
    let expr = |index| {
        MirSpan::ExprId(hir_def::hir::ExprId::from_raw(la_arena::RawIdx::from_u32(index)))
    };
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
    frontend.with(|db, _| {
        let lines = |spec: &Spec| {
            let super::Built::Ir(pending) = built_as(db, spec, &root) else { panic!("asm") };
            (pending.wants.requests.datas.iter())
                .filter_map(|it| match it {
                    DataRequest::Location(Some((owner, span))) => {
                        let place =
                            *hir_ty_mir::mir::location::source_location_in_file(db, *owner, *span);
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
    let method = |name: &str| p.trait_items(db).method_by_name(&Name::new_root(name)).expect("`P`'s method");
    let (distance, twice) = (method("distance"), method("twice"));
    let (concrete, bounded) = (function(db, "concrete"), function(db, "bounded"));
    hir_ty::attach_db(db, || {
        let interner = DbInterner::new_with(db, krate);
        let env = |def: FunctionId| ParamEnvAndCrate { param_env: db.trait_environment(def.into()), krate };
        let s = Ty::new_adt(interner, AdtId::StructId(s), GenericArgs::empty());
        let why = crate::impl_method(db, env(concrete), distance, GenericArgs::new_from_slice(&[s.into()]))
            .expect_err("no impl of `P` is for `S`");
        assert!(why.starts_with("trait P method distance for [S"), "{why}");
        assert!(why.ends_with(": no impl holds"), "{why}");

        let param = GenericArgs::identity_for_item(interner, bounded.into()).type_at(0);
        let (found, _) = crate::impl_method(db, env(bounded), distance, GenericArgs::new_from_slice(&[param.into()]))
            .expect("a where clause proves `T: P`");
        assert_eq!(found, either::Either::Left(distance));

        let inherits = Ty::new_adt(interner, AdtId::StructId(inherits), GenericArgs::empty());
        let (found, _) = crate::impl_method(db, env(concrete), twice, GenericArgs::new_from_slice(&[inherits.into()]))
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
    hir_ty::attach_db(db, || {
        let policy = policy(OptLevel::O0);
        let root = frontend.root(function(db, "roots"));
        let origin = frontend.with(|hir, _| frontend.origin(hir, fixture::test_crate(&db)));
        let key = |instance: &HirInstance| {
            let described = (frontend.describe(instance, &origin))
                .unwrap_or_else(|e| panic!("{instance:?}: {}", e.what));
            let choice = policy.choose(&described.origin, described.overflow_checks).expect("the plan");
            UnitInstance { code: described.code, args: described.args, settings: choice.first.settings() }.key()
        };
        let glues: Vec<HirInstance> = frontend.with(|hir, homes| {
            refs::walk(hir, &root, false, homes)
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
