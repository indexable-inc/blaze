//! The IR handed to the compiled tier of proc macros, over fixtures: the stencil backend's,
//! its names, what it refuses, and the instances a root reaches.

#[path = "../tests/db.rs"]
mod db;

use blaze_codegen::{
    Instance as UnitInstance, InstanceKey,
    backend::Prepare as _,
    driver::{self, Lookup},
    memo::Memo,
    plan::{Mode, OptLevel, Override, Policy, Profile},
    settings::{DebugInfo, Panic, Pgo, Sanitize, Toolchain},
};
use blaze_cas::Key;
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use hir_def::FunctionId;
use hir_expand::fixture;

use super::*;
use crate::v2::{Backends, BuildTime, Databases, HashSource, Prepared, by_id::ById, refs};
use db::TestDB;

const PROGRAM: &str = r#"
//- minicore: sized, asm
pub static COUNT: u32 = 3;
pub fn callee(x: u32) -> u32 { x + COUNT }
pub fn caller(x: u32) -> u32 { callee(x) + 1 }
pub fn spins() { unsafe { core::arch::asm!("nop") } }
#[unsafe(naked)]
pub extern "C" fn bare() { core::arch::naked_asm!("ret") }
"#;

/// The tier's spec: copy-and-patch's IR of static code that counts nothing and folds nothing.
fn spec() -> Spec {
    let target = crate::Target::selected().expect("a target");
    let arch = match target.arch {
        crate::Arch::X86_64 => blaze_codegen::Arch::X86_64,
        crate::Arch::Aarch64 => blaze_codegen::Arch::Aarch64,
        crate::Arch::Wasm32 => blaze_codegen::Arch::Wasm32,
    };
    let stencils = target.stencil().expect("a target with stencils");
    let library = blaze_stencil::library_id(&stencils).expect("the stencil library");
    Spec {
        toolchain: Toolchain {
            id: ContentId::default(),
            target: target.triple().to_owned(),
            arch,
            cpu: target.cpu().to_owned(),
            features: Vec::new(),
            cranelift: "0.136".to_owned(),
            llvm: None,
            stencils: Some(blaze_format::hex::encode(&library)),
            frame_pointers: crate::backend::frame_pointers(),
            relocation: Relocation::Static,
        },
        backend: Backend::Stencil,
        overflow_checks: false,
        debug_assertions: false,
        panic: Panic::Unwind,
        debuginfo: DebugInfo::None,
        fold: false,
        counted: false,
        mir_inline: false,
        pgo: Pgo::None,
        sanitize: Sanitize::default(),
    }
}

/// What the process compiles to: its code position-independent or static as its ISA is.
fn relocation() -> Relocation {
    match crate::target().expect("a code generator").flags().is_pic() {
        true => Relocation::Pic,
        false => Relocation::Static,
    }
}

/// A build's stencil unit of the same IR: counting, and as the process relocates.
fn built_spec() -> Spec {
    let mut spec = spec();
    spec.counted = true;
    spec.toolchain.relocation = relocation();
    spec
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

/// A build at -O0 of the process's toolchain, which Cranelift compiles.
fn policy() -> Policy {
    let mut toolchain = spec().toolchain;
    toolchain.relocation = relocation();
    Policy {
        toolchain,
        profile: Profile {
            opt_level: OptLevel::O0,
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
        pgo: Pgo::None,
    }
}

/// The fixture lives as long as the test process, as the frontend borrows it.
fn frontend() -> (&'static TestDB, HirFrontend<'static>) {
    let db: &'static TestDB = Box::leak(Box::new(fixture::db(PROGRAM).into_db()));
    let build_time = std::sync::Arc::new(BuildTime(Default::default()));
    let frontend = HirFrontend::new(Databases::new(db), HashSource::ById(ById), build_time);
    (db, frontend.with_policy(&policy()))
}

#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    hir_def::test_items::function(db, fixture::test_crate(db), name)
}

/// One target a reference, each another symbol.
fn refs_of(frontend: &HirFrontend<'_>, instance: &HirInstance) -> Vec<Target> {
    frontend.with(|db, homes| {
        let inlined = frontend.inlines(db, instance).expect("the policy");
        let walked = refs::walk(db, instance, inlined, homes).expect("the walk");
        (0..walked.len()).map(|at| Target::Extern(format!("ref{at}"))).collect()
    })
}

/// The IR `lowered` gives is the IR of the function `Prepared::Stencil` holds, lowered as
/// copy-and-patch lowers it, whatever the unit counts or how it relocates.
#[test]
fn the_ir_is_the_stencil_backends() {
    let (db, frontend) = frontend();
    for name in ["callee", "caller"] {
        let root = frontend.root(function(db, name));
        let refs = refs_of(&frontend, &root);
        let built = built_spec();
        let (_, prepared) = Backends::default()
            .prepare(&frontend, &unit(&built), &built, &root, &refs)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let Prepared::Stencil(function) = prepared else { panic!("{name}: a stencil unit") };
        let mut want = function.ir().expect("IR").clone();
        drop(crate::v2::backend::stencil_ir(&mut want));
        let tier = spec();
        let lowering = lowered(&frontend, &unit(&tier), &tier, &root, &refs).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let Lowering::Ir { func, .. } = lowering else { panic!("{name}: IR") };
        assert_eq!(func.display().to_string(), want.display().to_string(), "{name}");
    }
}

/// Each user name is a target of the build: a reference's, by its place, or the function's
/// own data, which is handed out with it.
#[test]
fn each_name_is_a_reference_or_the_functions_own_data() {
    let (db, frontend) = frontend();
    let tier = spec();
    let root = frontend.root(function(db, "caller"));
    let refs = refs_of(&frontend, &root);
    let Lowering::Ir { func, names, data } = lowered(&frontend, &unit(&tier), &tier, &root, &refs).expect("IR") else {
        panic!("IR")
    };
    let declared = func.params.user_named_funcs();
    assert_eq!(names.len(), declared.len());
    assert!(declared.values().any(|it| it.namespace == 0), "`caller` calls `callee`: {names:?}");
    assert!(declared.values().all(|it| it.namespace <= 1), "{declared:?}");
    for name in &names {
        let known = match name {
            Target::Data(id) => data.iter().any(|it| it.id == *id),
            other => refs.contains(other),
        };
        assert!(known, "{name:?} of {names:?}, refs {refs:?}, data {data:?}");
    }
}

/// A spec the tier does not compile is refused, naming what of it and the unit, before any
/// IR is built; so is a body with `asm!` and a naked function, by its name.
#[test]
fn what_the_tier_does_not_compile_is_refused_by_name() {
    let (db, frontend) = frontend();
    let root = frontend.root(function(db, "caller"));
    let refs = refs_of(&frontend, &root);
    let refused = |spec: Spec, what: &str| {
        let unit = unit(&spec);
        let Err(e) = lowered(&frontend, &unit, &spec, &root, &refs) else { panic!("{what}: lowered") };
        assert_eq!(e.unit, unit.instance.key(), "{e:?}");
        assert!(e.what.contains(what), "{what}: {e:?}");
    };
    refused(Spec { backend: Backend::Cranelift { speed: false }, ..spec() }, "for Cranelift");
    refused(Spec { counted: true, ..spec() }, "`counted`");
    refused(Spec { fold: true, ..spec() }, "`fold`");
    let mut pic = spec();
    pic.toolchain.relocation = Relocation::Pic;
    refused(pic, "position-independent");

    let tier = spec();
    for (name, what) in [("spins", "`asm!` statements"), ("bare", "a naked function")] {
        let root = frontend.root(function(db, name));
        let refs = refs_of(&frontend, &root);
        let Err(e) = lowered(&frontend, &unit(&tier), &tier, &root, &refs) else { panic!("{name}: lowered") };
        assert!(e.what.contains(what) && e.what.contains(name), "{name}: {e:?}");
    }
}

/// `Kind::Asm` and `Kind::Allocator` are handed out by their text and symbol.
#[test]
fn assembly_and_the_allocator_shim_are_named() {
    let (db, frontend) = frontend();
    let tier = spec();
    let text = HirInstance(Kind::Asm("nop".to_owned(), false));
    let Ok(Lowering::Asm(got)) = lowered(&frontend, &unit(&tier), &tier, &text, &[]) else { panic!("asm") };
    assert_eq!(got, "nop");
    let forward = crate::v2::allocator::Forward {
        symbol: "__rust_alloc",
        to: None,
        params: 2,
        returns: true,
        krate: fixture::test_crate(&db),
    };
    let shim = HirInstance(Kind::Allocator(forward));
    let Ok(Lowering::Allocator(symbol)) = lowered(&frontend, &unit(&tier), &tier, &shim, &[]) else {
        panic!("the allocator shim")
    };
    assert_eq!(symbol, "__rust_alloc");
}

struct Empty;

impl Lookup for Empty {
    fn read(&self, _: &Key) -> Result<Option<&[u8]>, String> {
        Ok(None)
    }
}

/// The walk alone reaches what a whole build of the same roots does, in its key order, and
/// each instance it reaches is handed out as IR the tier compiles.
#[test]
fn the_reach_of_a_root_is_the_drivers() {
    let (db, frontend) = frontend();
    let policy = policy();
    let root = frontend.root(function(db, "caller"));
    let origin = frontend.with(|hir, _| frontend.origin(hir, fixture::test_crate(&db)));
    let roots = [(root, origin)];
    let backends = Backends::default();
    let memo = Memo::new();
    let reached = driver::Build::new(&backends, &frontend, &policy, &memo).reach(&roots, &[]).expect("the walk");
    let built = driver::Build::new(&backends, &frontend, &policy, &memo).run(&roots, &Empty).expect("the build");
    let keys: Vec<InstanceKey> = reached.functions.iter().map(|(_, unit)| unit.instance.key()).collect();
    assert_eq!(keys, built.inputs.iter().map(|it| it.0).collect::<Vec<_>>());
    assert!(keys.len() >= 2, "`caller` and `callee`: {keys:?}");
    let marks = built.reached(&built.roots[0]);
    assert!(reached.functions.iter().all(|(_, unit)| marks.contains(&unit.instance.code)), "{marks:?}");
    assert_eq!(reached.refs.len(), reached.functions.len());

    let tier = spec();
    for ((instance, unit), refs) in reached.functions.iter().zip(&reached.refs) {
        let instance_of = UnitInstance { settings: tier.settings(), ..unit.instance };
        let unit = Unit { instance: instance_of, backend: Backend::Stencil, ..unit.clone() };
        match lowered(&frontend, &unit, &tier, instance, refs) {
            Ok(Lowering::Ir { names, .. }) => assert!(names.iter().all(|it| !matches!(it, Target::Counter(_)))),
            Ok(_) => {}
            Err(e) => panic!("{instance:?}: {e:?}"),
        }
    }
}
