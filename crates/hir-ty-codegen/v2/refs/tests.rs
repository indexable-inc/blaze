//! The reference walk over fixtures: every instance a root reaches walks the same each time
//! and in another database, lists each reference once, and lists all that its IR asks for
//! with every setting of a unit.

#[path = "../tests/db.rs"]
mod db;

use base_db::Db;
use blaze_codegen::{
    Backend,
    backend::FrontRef,
    settings::{DebugInfo, Panic, Relocation, Sanitize, Spec, Toolchain},
};
use blaze_hash::HashSet;
use blaze_ids::ContentId;
use cranelift_codegen::ir;
use hir_def::{FunctionId, StaticId, signatures::FunctionSignature};
use hir_expand::fixture;
use hir_ty::db::HirDatabase;

use super::walk;
use crate::{
    Instance, InstanceDef,
    home::Homes,
    v2::{
        BuildTime, Built, Databases, HashSource, HirFrontend, HirInstance, HirStatic, Kind,
        backend::{built, compiled_in},
        build::{self, Build},
        by_id::ById,
        places::{self, Placer, Ref, place_requests},
    },
};
use db::TestDB;

/// The fixture lives as long as the test process, as the frontend borrows it.
fn frontend(text: &str) -> (&'static TestDB, HirFrontend<'static>) {
    let db: &'static TestDB = Box::leak(Box::new(fixture::db(text).into_db()));
    (db, HirFrontend::new(Databases::new(db), HashSource::ById(ById), std::sync::Arc::new(BuildTime(Default::default()))))
}

#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    hir_def::test_items::function(db, fixture::test_crate(db), name)
}

#[track_caller]
fn static_named(db: &TestDB, name: &str) -> StaticId {
    hir_def::test_items::static_item(db, fixture::test_crate(db), name)
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

/// Every setting of a unit its IR is built with: overflow checks (which `Backends::prepared`
/// passes as `checked`, and `Build::of` does not read), debug assertions, panic strategy.
fn specs() -> Vec<Spec> {
    let mut specs = Vec::new();
    for overflow_checks in [false, true] {
        for debug_assertions in [false, true] {
            for panic in [Panic::Unwind, Panic::Abort] {
                specs.push(Spec { overflow_checks, debug_assertions, panic, ..spec() });
            }
        }
    }
    specs
}

/// Each instance of code a root reaches, in the order found, with its walk.
type Reached = Vec<(HirInstance, Vec<Ref>)>;

/// The walk of `instance`, checked: the same twice over and in another of the build's
/// databases, and each reference once.
fn walked(
    frontend: &HirFrontend,
    db: &Db,
    homes: &mut Homes,
    instance: &HirInstance,
) -> Vec<Ref> {
    let first = walk(db, instance, false, homes).unwrap_or_else(|e| panic!("{e}"));
    let again = walk(db, instance, false, homes).unwrap_or_else(|e| panic!("again: {e}"));
    assert_eq!(first, again, "{instance:?} walks two ways");
    let elsewhere = frontend
        .with(|db, homes| walk(db, instance, false, homes))
        .unwrap_or_else(|e| panic!("in another database: {e}"));
    assert_eq!(first, elsewhere, "{instance:?} walks otherwise in another database");
    let distinct: HashSet<&Ref> = first.iter().collect();
    assert_eq!(distinct.len(), first.len(), "{instance:?} lists a reference twice: {first:?}");
    first
}

/// The IR of `instance` built with `spec` and overflow checks `checked` asks for nothing its
/// walk has not listed.
fn placed(
    db: &Db,
    homes: &mut Homes,
    spec: &Spec,
    instance: &HirInstance,
    walked: &[Ref],
) {
    let checked = spec.overflow_checks;
    let setting = format!(
        "overflow checks {checked}, debug assertions {}, panic {:?}",
        spec.debug_assertions, spec.panic
    );
    let mut code = build::with(Build::of(spec), || built(db, instance, checked))
        .and_then(|it| it)
        .unwrap_or_else(|e| panic!("{instance:?} with {setting}: {e}"));
    let requests = match &mut code {
        Built::Ir(pending) => &mut pending.wants.requests,
        Built::Assembled(_, requests) => requests,
    };
    let mut placer = Placer::listed(walked.to_vec());
    place_requests(db, homes, &HashSource::ById(ById), compiled_in(db, instance), &mut placer, requests).unwrap_or_else(|e| {
        panic!("{instance:?} with {setting} asks for what its walk has not: {e}")
    });
}

/// Walks from `root` through every instance of code it reaches, checking each walk
/// ([`walked`]) and the IR of each with every setting ([`placed`]). An assembly function it
/// reaches walks empty.
fn reach(frontend: &HirFrontend, root: HirInstance) -> Reached {
    let specs = specs();
    frontend.with(|db, homes| {
        let mut seen = HashSet::default();
        seen.insert(root.clone());
        let mut queue = vec![root];
        let mut reached = Reached::new();
        while let Some(instance) = queue.get(reached.len()).cloned() {
            let walk_of = walked(frontend, db, homes, &instance);
            for spec in &specs {
                placed(db, homes, spec, &instance, &walk_of);
            }
            for it in &walk_of {
                let FrontRef::Function(next) = it else { continue };
                match &next.0 {
                    Kind::Code(..) => {
                        if seen.insert(next.clone()) {
                            queue.push(next.clone());
                        }
                    }
                    Kind::Asm(..) => assert_eq!(walk(db, next, false, homes), Ok(Vec::new())),
                    Kind::Start(_) | Kind::Allocator(_) => (),
                }
            }
            reached.push((instance, walk_of));
        }
        reached
    })
}

fn reach_from(text: &str, root: &str) -> (&'static TestDB, Reached) {
    let (db, frontend) = frontend(text);
    let root = frontend.root(function(&db, root));
    let reached = reach(&frontend, root);
    (db, reached)
}

/// The names of the functions reached, one for each instance.
fn names(db: &TestDB, reached: &Reached) -> Vec<String> {
    reached
        .iter()
        .filter_map(|(instance, _)| match &instance.0 {
            Kind::Code(Instance { def: InstanceDef::Fn(func), .. }, _) => {
                Some(FunctionSignature::of(db, *func).name.as_str().to_owned())
            }
            _ => None,
        })
        .collect()
}

fn count(reached: &Reached, def: impl Fn(&InstanceDef) -> bool) -> usize {
    reached
        .iter()
        .filter(|(instance, _)| matches!(&instance.0, Kind::Code(code, _) if def(&code.def)))
        .count()
}

#[track_caller]
fn assert_reached(db: &TestDB, reached: &Reached, wanted: &[&str]) {
    let names = names(db, reached);
    for name in wanted {
        assert!(names.iter().any(|it| it == name), "`{name}` is not reached: {names:?}");
    }
}

fn root_walk(reached: &Reached) -> &[Ref] {
    &reached.first().expect("the root").1
}

#[track_caller]
fn assert_extern(walked: &[Ref], symbol: &str) {
    assert!(
        walked.iter().any(|it| matches!(it, FrontRef::Extern(name) if name.ends_with(symbol))),
        "`{symbol}` is not listed: {walked:?}"
    );
}

const PLAIN: &str = r#"
//- minicore: sized
pub fn callee(x: u32) -> u32 { x }
pub fn roots(x: u32) -> u32 { callee(x) + callee(x) }
"#;

/// A plain call lists its callee once, however often it is called.
#[test]
fn a_plain_call_lists_its_callee_once() {
    let (db, reached) = reach_from(PLAIN, "roots");
    let callee = function(&db, "callee");
    let calls = root_walk(&reached)
        .iter()
        .filter(|it| {
            matches!(it, FrontRef::Function(HirInstance(Kind::Code(code, _)))
                if code.def == InstanceDef::Fn(callee))
        })
        .count();
    assert_eq!(calls, 1, "{reached:?}");
}

const GENERIC: &str = r#"
//- minicore: sized
pub fn id<T>(x: T) -> T { x }
pub fn pair<A, B>(a: A, b: B) -> (A, B) { (id(a), id(b)) }
pub fn roots() -> u32 {
    let _ = pair(1u32, 2u64);
    id(3u32)
}
"#;

/// A generic callee is one instance per type: `id::<u32>` asked for by `roots` and by `pair`
/// is the same reference.
#[test]
fn a_generic_call_is_one_instance_per_type() {
    let (db, reached) = reach_from(GENERIC, "roots");
    let id = function(&db, "id");
    assert_eq!(count(&reached, |def| *def == InstanceDef::Fn(id)), 2, "{reached:?}");
}

const DROPS: &str = r#"
//- minicore: sized, drop
pub struct Held(pub u32);
impl Drop for Held { fn drop(&mut self) {} }
pub struct Holder { pub held: Held, pub count: u32 }
pub struct Buffer<T> { pub ptr: *mut T, pub len: usize }
impl<T> Drop for Buffer<T> {
    fn drop(&mut self) { unsafe { core::ptr::drop_in_place(self.ptr) } }
}
pub fn roots(x: u32, at: *mut Holder) {
    let _held = Held(x);
    let _holder = Holder { held: Held(x), count: x };
    let _buffer = Buffer { ptr: at, len: 1 };
}
"#;

/// A local with `Drop`, a struct whose field has drop glue and a buffer that drops what it
/// points to through `drop_in_place` list the glue of each, which lists the `Drop` impls.
#[test]
fn drops_list_their_glue() {
    let (db, reached) = reach_from(DROPS, "roots");
    assert!(count(&reached, |def| *def == InstanceDef::DropGlue) >= 3, "{reached:?}");
    assert_reached(&db, &reached, &["drop"]);
}

const DYN: &str = r#"
//- minicore: sized, unsize, coerce_unsized, dispatch_from_dyn, drop
pub struct Held(pub u32);
impl Drop for Held { fn drop(&mut self) {} }
pub trait Sides { fn sides(&self) -> u32; }
pub trait Shape: Sides { fn area(&self) -> u32; }
pub struct Square { pub held: Held, pub side: u32 }
impl Sides for Square { fn sides(&self) -> u32 { 4 } }
impl Shape for Square { fn area(&self) -> u32 { self.side * self.side } }
pub fn measure(shape: &dyn Shape) -> u32 { shape.area() + shape.sides() }
pub fn roots(side: u32) -> u32 {
    let square = Square { held: Held(side), side };
    let shape: &dyn Shape = &square;
    let sides: &dyn Sides = shape;
    measure(shape) + sides.sides()
}
"#;

/// A coercion to `dyn Trait` lists the vtable's methods, the supertrait's among them, and its
/// drop glue; calls through it and the upcast to the supertrait ask for nothing more.
#[test]
fn a_dyn_coercion_lists_its_vtable() {
    let (db, reached) = reach_from(DYN, "roots");
    assert_reached(&db, &reached, &["area", "sides", "measure", "drop"]);
    assert!(count(&reached, |def| *def == InstanceDef::DropGlue) >= 1, "{reached:?}");
}

const DYN_PROVIDED: &str = r#"
//- minicore: sized, unsize, coerce_unsized, dispatch_from_dyn
pub trait P {
    fn distance(&self) -> u32;
    fn twice(&self) -> u32 { self.distance() * 2 }
}
pub struct Inherits(pub u32);
impl P for Inherits { fn distance(&self) -> u32 { self.0 } }
pub fn roots(x: u32) -> u32 {
    let inherits = Inherits(x);
    let p: &dyn P = &inherits;
    p.twice() + inherits.twice()
}
"#;

/// An impl that leaves out a provided method (`Write::write_fmt` of `Stderr`) fills its
/// vtable slot and its direct calls with the trait's provided method.
#[test]
fn a_provided_method_an_impl_inherits_fills_its_vtable_slot() {
    let (db, reached) = reach_from(DYN_PROVIDED, "roots");
    assert_reached(&db, &reached, &["distance", "twice"]);
}

const DYN_FN: &str = r#"
//- minicore: sized, copy, unsize, coerce_unsized, dispatch_from_dyn, fn, drop
pub struct Held(pub u32);
impl Drop for Held { fn drop(&mut self) {} }
pub fn call_dyn(f: &dyn Fn(u32) -> u32, x: u32) -> u32 { f(x) }
pub fn call_dyn_mut(f: &mut dyn FnMut(u32) -> u32, x: u32) -> u32 { f(x) }
pub fn roots(x: u32) -> u32 {
    let held = Held(x);
    let add = move |y: u32| y + held.0;
    let mut count = 0;
    let mut bump = |y: u32| { count += y; count };
    call_dyn(&add, 1) + call_dyn_mut(&mut bump, 2)
}
"#;

/// A closure coerced to `dyn Fn` or `dyn FnMut` lists its body, its `call_once` shim and its
/// drop glue; the call through the object asks for nothing more.
#[test]
fn a_dyn_fn_call_lists_the_closure_vtable() {
    let (db, reached) = reach_from(DYN_FN, "roots");
    assert_reached(&db, &reached, &["call_dyn", "call_dyn_mut"]);
    assert!(count(&reached, |def| matches!(def, InstanceDef::Closure(_))) >= 2, "{reached:?}");
    assert!(count(&reached, |def| *def == InstanceDef::CallOnceShim) >= 1, "{reached:?}");
    assert!(count(&reached, |def| *def == InstanceDef::DropGlue) >= 1, "{reached:?}");
}

const DERIVED: &str = r#"
//- minicore: sized, copy, clone, derive, eq, unsize, coerce_unsized, dispatch_from_dyn
#[derive(Clone, PartialEq)]
pub struct Pair(pub u32, pub u64);
pub fn roots(p: &Pair) -> bool {
    let q = p.clone();
    let other: &dyn PartialEq<Pair> = &q;
    *p == q && other.eq(p)
}
"#;

/// rustc compiles what a builtin derive expands to. Where the database expands them, the
/// derived `clone` and `eq` are reached as written methods are, called and in the vtable of
/// `dyn PartialEq`; where it only records them, the call is refused naming the derive.
#[test]
fn a_derived_method_is_reached_where_derives_are_expanded() {
    let mut expanded = fixture::db(DERIVED).into_db();
    hir_def::nameres::set_expand_builtin_derives(&mut expanded, true);
    let db: &'static TestDB = Box::leak(Box::new(expanded));
    let expanding =
        HirFrontend::new(Databases::new(db), HashSource::ById(ById), std::sync::Arc::new(BuildTime(Default::default())));
    let reached = reach(&expanding, expanding.root(function(db, "roots")));
    assert_reached(db, &reached, &["clone", "eq"]);

    let (db, frontend) = frontend(DERIVED);
    let root = frontend.root(function(db, "roots"));
    let refused = frontend.with(|db, homes| walk(db, &root, false, homes)).expect_err("the call of `clone`");
    assert!(refused.contains("`Clone::clone` of") && refused.contains("#[derive(Clone)]"), "{refused}");
}

const SOFT_FLOATS: &str = r#"
//- minicore: sized
pub fn roots(a: f128, b: f16) -> f128 { a * a + b as f128 }
"#;

/// `f16` and `f128` arithmetic lists the compiler-builtins functions LLVM calls for it, on
/// each target whose code generator lowers them: Apple's AArch64 as System V's.
#[test]
fn soft_float_arithmetic_lists_its_builtins() {
    let (_, reached) = reach_from(SOFT_FLOATS, "roots");
    for symbol in ["__multf3", "__addtf3", "__extendhftf2"] {
        assert_extern(root_walk(&reached), symbol);
    }
}

const OVERFLOW: &str = r#"
//- minicore: sized, index, slice, panic_location
#![feature(lang_items, rustc_attrs)]
pub mod panicking {
    #[lang = "panic_bounds_check"]
    #[track_caller]
    pub fn panic_bounds_check(index: usize, len: usize) -> ! { loop {} }
    pub mod panic_const {
        #[track_caller] pub const fn panic_const_add_overflow() -> ! { loop {} }
        #[track_caller] pub const fn panic_const_sub_overflow() -> ! { loop {} }
        #[track_caller] pub const fn panic_const_mul_overflow() -> ! { loop {} }
        #[track_caller] pub const fn panic_const_div_overflow() -> ! { loop {} }
        #[track_caller] pub const fn panic_const_rem_overflow() -> ! { loop {} }
        #[track_caller] pub const fn panic_const_shl_overflow() -> ! { loop {} }
        #[track_caller] pub const fn panic_const_shr_overflow() -> ! { loop {} }
        #[track_caller] pub const fn panic_const_neg_overflow() -> ! { loop {} }
        #[track_caller] pub const fn panic_const_div_by_zero() -> ! { loop {} }
        #[track_caller] pub const fn panic_const_rem_by_zero() -> ! { loop {} }
    }
}
#[rustc_inherit_overflow_checks]
pub fn roots(a: i32, b: i32, s: u32, xs: [i32; 4], ys: &[i32], i: usize) -> i32 {
    let sum = a + b - a * b;
    let quotient = a / b + a % b;
    let shifted = (a << s) + (a >> s);
    let negated = -a;
    sum + quotient + shifted + negated + xs[i] + ys[i]
}
"#;

/// Each check of `+ - *`, `/ %`, `<< >>`, `-a` and an index lists the panic it makes when it
/// fails, whether or not the unit checks overflow.
#[test]
fn checks_list_their_panics() {
    let (db, reached) = reach_from(OVERFLOW, "roots");
    assert_reached(
        &db,
        &reached,
        &[
            "panic_bounds_check",
            "panic_const_add_overflow",
            "panic_const_sub_overflow",
            "panic_const_mul_overflow",
            "panic_const_div_overflow",
            "panic_const_rem_overflow",
            "panic_const_shl_overflow",
            "panic_const_shr_overflow",
            "panic_const_neg_overflow",
            "panic_const_div_by_zero",
            "panic_const_rem_by_zero",
        ],
    );
}

const STATICS: &str = r#"
//- minicore: sized
#![feature(thread_local)]
pub static COUNT: u32 = 3;
pub static mut TOTAL: u32 = 0;
#[thread_local]
pub static mut LOCAL: u32 = 0;
pub fn roots(x: u32) -> u32 {
    unsafe {
        LOCAL = x;
        TOTAL = COUNT;
        LOCAL + TOTAL
    }
}
"#;

/// A static is listed as itself, and a `#[thread_local]` one with the routine Cranelift calls
/// to reach it, which no request of the IR names.
#[test]
fn statics_are_listed_with_what_reaches_them() {
    let (db, reached) = reach_from(STATICS, "roots");
    let walked = root_walk(&reached);
    for name in ["TOTAL", "LOCAL"] {
        let it = FrontRef::Static(HirStatic(static_named(&db, name)));
        assert!(walked.contains(&it), "`{name}` is not listed: {walked:?}");
    }
    let tls = FrontRef::Extern(places::libcall_symbol(ir::LibCall::ElfTlsGetAddr));
    assert!(walked.contains(&tls), "{walked:?}");
}

const CLOSURES: &str = r#"
//- minicore: sized, fn, copy
pub fn apply(f: impl Fn(u32) -> u32, x: u32) -> u32 { f(x) }
pub fn apply_once<F: FnOnce() -> u32>(f: F) -> u32 { f() }
pub fn roots(x: u32) -> u32 {
    let offset = x;
    let direct = |y: u32| y + offset;
    let pointer: fn(u32) -> u32 = |y| y * 2;
    direct(1) + apply(direct, 2) + apply(pointer, 3) + pointer(4) + apply_once(move || offset)
}
"#;

/// A closure called directly, passed as `impl Fn`, passed as `FnOnce` and coerced to a `fn`
/// pointer lists each body, and the shim the pointer points at.
#[test]
fn closures_list_their_bodies() {
    let (db, reached) = reach_from(CLOSURES, "roots");
    assert_reached(&db, &reached, &["apply", "apply_once"]);
    assert!(count(&reached, |def| matches!(def, InstanceDef::Closure(_))) >= 3, "{reached:?}");
    assert!(count(&reached, |def| *def == InstanceDef::FnPointerShim) >= 1, "{reached:?}");
}

const DECIDED: &str = r#"
//- minicore: sized
pub trait Flag { const ON: bool; }
pub struct Yes;
pub struct No;
impl Flag for Yes { const ON: bool = true; }
impl Flag for No { const ON: bool = false; }
pub fn taken() -> u32 { 1 }
pub fn skipped() -> u32 { 2 }
pub fn pick<T: Flag>() -> u32 { if T::ON { taken() } else { skipped() } }
pub fn roots() -> u32 {
    let by_literal = match 3u32 { 3 => taken(), _ => skipped() };
    let by_cfg = if false { skipped() } else { taken() };
    by_literal + by_cfg + pick::<Yes>() + pick::<No>()
}
"#;

/// A switch on a constant (a literal, `cfg!`'s `false`, an associated constant) is decided
/// alike by the walk and the IR builder: an arm the walk leaves out the IR never asks for.
#[test]
fn a_decided_switch_is_decided_alike() {
    let (db, reached) = reach_from(DECIDED, "roots");
    let pick = function(&db, "pick");
    assert_eq!(count(&reached, |def| *def == InstanceDef::Fn(pick)), 2, "{reached:?}");
    assert_reached(&db, &reached, &["taken"]);
}

const TRACKED: &str = r#"
//- minicore: sized, fn, panic_location
#[track_caller]
pub fn here() -> u32 { core::panic::Location::caller().line() }
pub fn roots() -> u32 {
    let pointer: fn() -> u32 = here;
    here() + pointer()
}
"#;

/// A `#[track_caller]` callee, called and taken as a pointer, lists what its location and the
/// pointer need.
#[test]
fn a_track_caller_callee_lists_what_its_location_needs() {
    let (db, reached) = reach_from(TRACKED, "roots");
    assert_reached(&db, &reached, &["here"]);
}

const EXTERN: &str = r#"
//- minicore: sized
unsafe extern "C" {
    pub fn outside(x: u32) -> u32;
    pub fn printf(format: *const u8, ...) -> i32;
}
pub extern "C" fn exported(x: u32) -> u32 { x }
pub fn roots(x: u32) -> i32 {
    let format = 0u8;
    let pointer: extern "C" fn(u32) -> u32 = exported;
    unsafe { outside(exported(x) + pointer(x)) as i32 + printf(&format as *const u8, 1.5f64, x) }
}
"#;

/// An `extern "C"` function is listed by its symbol, one defined in Rust as its instance; a
/// call of a variadic one with a float argument on x86-64 System V lists the assembly it goes
/// through, which walks empty and assembles naming nothing.
#[test]
fn extern_and_variadic_calls_list_their_symbols() {
    let (db, frontend) = frontend(EXTERN);
    let root = frontend.root(function(&db, "roots"));
    let reached = reach(&frontend, root);
    let walked = root_walk(&reached);
    assert_extern(walked, "outside");
    assert_extern(walked, "printf");
    assert_reached(&db, &reached, &["exported"]);
    let spec = spec();
    let system_v = spec.toolchain.arch == blaze_codegen::Arch::X86_64
        && !spec.toolchain.target.contains("windows");
    let assembly: Vec<&HirInstance> = walked
        .iter()
        .filter_map(|it| match it {
            FrontRef::Function(instance @ HirInstance(Kind::Asm(..))) => Some(instance),
            _ => None,
        })
        .collect();
    if !system_v {
        assert!(assembly.is_empty(), "{walked:?}");
        return;
    }
    let [assembly] = assembly[..] else { panic!("{walked:?}") };
    frontend.with(|db, homes| {
        assert_eq!(walk(db, assembly, false, homes), Ok(Vec::new()));
        let mut code = built(db, assembly, false).expect("the assembly");
        let Built::Assembled(_, requests) = &mut code else { panic!("IR of assembly") };
        place_requests(db, homes, &HashSource::ById(ById), None, &mut Placer::listed(Vec::new()), requests)
            .expect("assembly that names nothing");
    });
}

const INTRINSICS: &str = r#"
//- minicore: sized
#![feature(intrinsics)]
pub mod intrinsics {
    #[rustc_intrinsic]
    pub const fn abort_immediate() -> !;
    #[rustc_intrinsic]
    pub fn sqrt<T>(x: T) -> T;
}
pub fn root_of(x: f64) -> f64 { intrinsics::sqrt(x) + intrinsics::sqrt(x as f32) as f64 }
pub fn remainder(x: f64) -> f64 { x % 2.0 }
pub fn give_up(fail: bool) -> u32 { if fail { intrinsics::abort_immediate() } 1 }
pub fn roots(x: f64, fail: bool) -> f64 { root_of(x) + remainder(x) + give_up(fail) as f64 }
"#;

/// `abort_immediate` and a float `sqrt` are the IR builder's own and list no function; a float
/// `%` lists the C library's `fmod`.
#[test]
fn intrinsics_list_what_they_lower_to() {
    let (db, reached) = reach_from(INTRINSICS, "roots");
    assert_reached(&db, &reached, &["root_of", "remainder", "give_up"]);
    let remainder = function(&db, "remainder");
    let (_, walked) = reached
        .iter()
        .find(|(instance, _)| {
            matches!(&instance.0, Kind::Code(code, _) if code.def == InstanceDef::Fn(remainder))
        })
        .expect("`remainder` is reached");
    assert_extern(walked, "fmod");
}

const LIBCALLS: &str = r#"
//- minicore: sized
#![feature(intrinsics)]
pub mod intrinsics {
    #[rustc_intrinsic]
    pub fn powi<T>(a: T, x: i32) -> T;
    #[rustc_intrinsic]
    pub fn powf<T>(a: T, x: T) -> T;
    #[rustc_intrinsic]
    pub const fn fma<T>(a: T, b: T, c: T) -> T;
    #[rustc_intrinsic]
    pub unsafe fn frem_fast<T>(a: T, b: T) -> T;
    #[rustc_intrinsic]
    pub const unsafe fn unchecked_div<T>(x: T, y: T) -> T;
    #[rustc_intrinsic]
    pub const fn mul_with_overflow<T>(x: T, y: T) -> (T, bool);
}
use intrinsics::*;
pub fn roots(x: f64, a: u128, b: u128) -> f64 {
    let (product, _) = mul_with_overflow(a, b);
    let quotient = unsafe { unchecked_div(a, b) };
    let floats = powi(x, 3) + powf(x, x) + fma(x, x, x) + unsafe { frem_fast(x, x) };
    floats + (product ^ quotient) as f64
}
"#;

/// Intrinsics dispatched by the enum the call holds list the functions their names did: the
/// generic `powi`, `powf` and `fma` those of `powif64`, `powf64` and `fmaf64`, `frem_fast` C's
/// `fmod`, and the 128-bit quotient and product compiler-builtins'.
#[test]
fn intrinsics_by_enum_list_what_their_names_did() {
    let (_, reached) = reach_from(LIBCALLS, "roots");
    let walked = root_walk(&reached);
    for symbol in ["__powidf2", "pow", "fma", "fmod", "__udivti3", "__rust_u128_mulo"] {
        assert_extern(walked, symbol);
    }
}

const PROMOTED: &str = r#"
//- minicore: sized, copy
pub fn double(x: u32) -> u32 { x * 2 }
pub fn triple(x: u32) -> u32 { x * 3 }
pub fn square(x: u32) -> u32 { x * x }
pub const TABLE: &[fn(u32) -> u32; 1] = &[square];
pub fn numbers() -> &'static [u32; 3] { &[1, 2, 3] }
pub fn table() -> &'static [fn(u32) -> u32; 2] { &[double, triple] }
pub fn roots(x: u32) -> u32 {
    let &[first, second] = table();
    let &[_, _, third] = numbers();
    let &[named] = TABLE;
    first(x) + second(third) + named(x)
}
"#;

/// A promoted constant and a named one that hold function pointers list the functions they
/// point at.
#[test]
fn a_constant_lists_the_functions_it_points_at() {
    let (db, reached) = reach_from(PROMOTED, "roots");
    assert_reached(&db, &reached, &["table", "numbers", "double", "triple", "square"]);
}

/// The walk of an assembly function is empty: its text names nothing.
#[test]
fn assembly_walks_empty() {
    let (_, frontend) = frontend(PLAIN);
    let assembly = HirInstance(Kind::Asm("nop".to_owned(), false));
    frontend.with(|db, homes| assert_eq!(walk(db, &assembly, false, homes), Ok(Vec::new())));
}

const BROKEN: &str = r#"
//- minicore: sized
pub fn broken() -> u32 { "not a number" }
"#;

/// A body that does not lower is an error of the walk that names the instance, not a panic.
#[test]
fn a_body_that_does_not_lower_is_an_error_naming_the_instance() {
    let (db, frontend) = frontend(BROKEN);
    let root = frontend.root(function(&db, "broken"));
    let walked = frontend.with(|db, homes| walk(db, &root, false, homes));
    let error = walked.expect_err("the walk of a body with a type error");
    assert!(error.contains(&format!("{root:?}")), "{error}");
}
