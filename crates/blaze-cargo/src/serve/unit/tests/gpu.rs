//! An `artifact = "gpu"` dependency end to end, as the daemon makes it: a shader crate built for
//! the device (`blaze_units::GPU_DEVICE`) on the device's `core` and a small `gpu` crate, its
//! `GPU_ENTRIES` lowered, validated by naga and written as MSL from the front end
//! (`blaze_shader::device::device_msl`, what `compile::assemble` writes at the unit's link), and a
//! host crate whose `include_bytes!(env!("BLAZE_GPU_MSL_SHADERS"))` reads exactly those bytes once
//! the writer applies them (`blaze_lower::Lowered::gpu_msl`), before any placement puts them on
//! disk. The plan that names the file to the host (`BLAZE_GPU_MSL_<CRATE>`, the device unit, the
//! gpu edge) is blaze-units' test (`load/tests.rs`); this one takes such a plan.

use blaze_units::{
    query::{BuildConfig, Command, Invocation, Mount, Origin, PackagePlan, UnitPlan, Units},
    schema::{Mode, Profile, Strip, Stripped, Target, Unit, UnitDep, UnitGraph},
};

use super::super::graph;
use super::{HOST, package, plan, target, unit};

/// What the device's and the host's `core` are made of: minicore with what the shader's
/// lowering reaches (blaze-shader's own tests' set) and the host's `include_bytes!` and `env!`.
const MINICORE: &[&str] = &[
    "sized",
    "copy",
    "clone",
    "derive",
    "add",
    "mul",
    "unary_ops",
    "ord",
    "eq",
    "option",
    "iterator",
    "deref",
    "index",
    "slice",
    "sync",
    "fn",
    "builtin_impls",
    "unsize",
    "coerce_unsized",
    "transmute",
    "env",
    "include_bytes",
];

/// The gpu crate's descriptor types, at the real crate's paths (the reader goes by path, field
/// and variant name), as blaze-shader's tests have them.
const GPU: &str = r#"#![no_std]
pub mod vec {
    #[derive(Copy, Clone)]
    #[repr(C)]
    pub struct Vec2 { pub x: f32, pub y: f32 }
    #[derive(Copy, Clone)]
    #[repr(C)]
    pub struct Vec4 { pub x: f32, pub y: f32, pub z: f32, pub w: f32 }
    pub const fn vec2(x: f32, y: f32) -> Vec2 { Vec2 { x, y } }
    pub const fn vec4(x: f32, y: f32, z: f32, w: f32) -> Vec4 { Vec4 { x, y, z, w } }
}
pub mod entry {
    #[derive(Copy, Clone)] pub enum Stage { Vertex, Fragment }
    #[derive(Copy, Clone)] pub enum ValueTy { F32, Vec2, Vec3, Vec4, U32, UVec4, I32, IVec2 }
    #[derive(Copy, Clone)] pub enum Interp { Perspective, Linear, Flat }
    #[derive(Copy, Clone)] pub enum FieldKind { Position, Value { ty: ValueTy, interp: Interp } }
    #[derive(Copy, Clone)] pub struct Field { pub name: &'static str, pub kind: FieldKind, pub location: Option<u32> }
    #[derive(Copy, Clone)] pub enum Kind { VertexIndex, InstanceIndex, Varyings(&'static [Field]), Uniform { size: u32 }, Storage { stride: u32 }, Texture2d, Sampler }
    #[derive(Copy, Clone)] pub enum Out { Varyings(&'static [Field]), Color }
    #[derive(Copy, Clone)] pub enum Table { None, Buffer, Texture, Sampler }
    #[derive(Copy, Clone)] pub struct Slot { pub table: Table, pub index: u32 }
    #[derive(Copy, Clone)] pub struct ParamDesc { pub name: &'static str, pub kind: Kind, pub slot: Slot }
    #[derive(Copy, Clone)] pub struct EntryDesc { pub name: &'static str, pub stage: Stage, pub params: &'static [ParamDesc], pub out: Out, pub sizes: Option<Slot> }
    #[derive(Copy, Clone)] pub struct VertexIndex(pub u32);
    #[derive(Copy, Clone)] pub struct Position(pub crate::vec::Vec4);
    #[derive(Copy, Clone)] pub struct Code(pub *const ());
    unsafe impl Sync for Code {}
    #[derive(Copy, Clone)] pub struct Entry { pub desc: &'static EntryDesc, pub code: Code }
    #[derive(Copy, Clone)] pub struct Entries(pub &'static [Entry]);
}
"#;

/// The shader crate: one vertex entry, its descriptor as `#[gpu::vertex]` writes it.
const SHADERS: &str = r#"#![no_std]
use gpu::entry::*;
use gpu::vec::*;
#[derive(Copy, Clone)]
#[repr(C)]
pub struct VOut { pub pos: Position, pub uv: Vec2 }
pub fn vs(i: VertexIndex) -> VOut {
    let corners = [vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0)];
    let c = corners[i.0 as usize];
    VOut { pos: Position(vec4(c.x, c.y, 0.0, 1.0)), uv: c }
}
pub mod vs {
    use gpu::entry::*;
    pub const FIELDS: [Field; 2] = [
        Field { name: "pos", kind: FieldKind::Position, location: None },
        Field { name: "uv", kind: FieldKind::Value { ty: ValueTy::Vec2, interp: Interp::Perspective }, location: Some(0) },
    ];
    pub const PARAMS: [ParamDesc; 1] = [
        ParamDesc { name: "i", kind: Kind::VertexIndex, slot: Slot { table: Table::None, index: 0 } },
    ];
    pub const ENTRY: EntryDesc = EntryDesc {
        name: "vs",
        stage: Stage::Vertex,
        params: &PARAMS,
        out: Out::Varyings(&FIELDS),
        sizes: None,
    };
}
pub static GPU_ENTRIES: Entries = Entries(&[Entry { desc: &vs::ENTRY, code: Code(vs as *const ()) }]);
"#;

/// The host crate: the shader crate's MSL, as Kip's pass takes harness-native-shaders'.
const APP: &str = "#![no_std]\npub static MSL: &[u8] = core::include_bytes!(core::env!(\"BLAZE_GPU_MSL_SHADERS\"));\n";

/// Where the device unit writes its MSL (`UnitPlan::gpu_msl`): in its own `deps`, named as its
/// outputs are.
const MSL_PATH: &str = "/w/target/naga-unknown-unknown/debug/deps/shaders-0002.msl";

/// The units, by index.
const DEVICE_CORE: usize = 0;
const GPU_CRATE: usize = 1;
const SHADERS_UNIT: usize = 2;
const HOST_CORE: usize = 3;
const APP_UNIT: usize = 4;

fn core_source() -> String {
    test_utils::MiniCore::from_flags(MINICORE.iter().copied())
        .source_code(test_utils::MiniCore::RAW_SOURCE)
}

fn dep(index: usize, name: &str, std: bool, gpu: bool) -> UnitDep {
    UnitDep {
        index,
        extern_crate_name: name.to_owned(),
        public: None,
        noprelude: std.then_some(true),
        nounused: None,
        dev: false,
        gpu,
    }
}

/// `pkg`'s lib `name` at `src`, built (`Mode::Build`) for `platform`, as a load plans it: a
/// device unit aborts on panic.
fn built(
    pkg: &str,
    name: &str,
    src: &str,
    platform: Option<&str>,
    is_std: bool,
    dependencies: Vec<UnitDep>,
) -> Unit {
    let mut profile =
        Profile { incremental: true, strip: Strip::Resolved(Stripped::None), ..Profile::dev() };
    if platform.is_some() {
        profile.panic = "abort".to_owned();
    }
    let lib: Target = target(name, "lib", src);
    Unit {
        profile,
        platform: platform.map(str::to_owned),
        is_std,
        dependencies,
        ..unit(pkg, lib, Mode::Build, &[])
    }
}

const CORE: &str = "path+file:///rustc/abc/library/core#0.0.0";
const GPU_PKG: &str = "path+file:///w/gpu#0.1.0";
const SHADERS_PKG: &str = "path+file:///w/shaders#0.1.0";
const APP_PKG: &str = "path+file:///w/app#0.1.0";

/// `app`, whose `shaders = { path = "../shaders", artifact = "gpu" }` dependency is built for the
/// device on the device's `core` and `gpu`: the five units and plans a load makes of it.
fn units() -> Units {
    let device = Some(blaze_units::GPU_DEVICE);
    let core_src = "/rustc/abc/library/core/src/lib.rs";
    let units = vec![
        built(CORE, "core", core_src, device, true, Vec::new()),
        built(GPU_PKG, "gpu", "/w/gpu/src/lib.rs", device, false, vec![dep(
            DEVICE_CORE,
            "core",
            true,
            false,
        )]),
        built(SHADERS_PKG, "shaders", "/w/shaders/src/lib.rs", device, false, vec![
            dep(DEVICE_CORE, "core", true, false),
            dep(GPU_CRATE, "gpu", false, false),
        ]),
        built(CORE, "core", core_src, None, true, Vec::new()),
        built(APP_PKG, "app", "/w/app/src/lib.rs", None, false, vec![
            dep(HOST_CORE, "core", true, false),
            dep(SHADERS_UNIT, "shaders", false, true),
        ]),
    ];
    let shaders = UnitPlan {
        gpu_msl: Some(MSL_PATH.to_owned()),
        ..plan("-0002", &[
            "/w/target/naga-unknown-unknown/debug/deps/libshaders-0002.rlib",
            MSL_PATH,
        ])
    };
    let app = UnitPlan {
        env: vec![("BLAZE_GPU_MSL_SHADERS".to_owned(), MSL_PATH.to_owned())],
        ..plan("-0004", &["/w/target/debug/deps/libapp-0004.rlib"])
    };
    Units {
        graph: UnitGraph { version: 1, units, roots: vec![APP_UNIT] },
        plans: vec![plan("", &[]), plan("-0001", &[]), shaders, plan("", &[]), app],
        packages: vec![
            PackagePlan { is_member: false, ..package(CORE, "core", "/rustc/abc/library/core") },
            package(GPU_PKG, "gpu", "/w/gpu"),
            package(SHADERS_PKG, "shaders", "/w/shaders"),
            package(APP_PKG, "app", "/w/app"),
        ],
        lock: None,
        warnings: Vec::new(),
        target_dir: "/w/target".to_owned(),
        build_dir: "/w/target".to_owned(),
        finished: String::new(),
        link_dirs: Vec::new(),
        sandboxed_tests: false,
        native: Vec::new(),
    }
}

/// [`units`] lowered over files in memory, a fresh front end each call.
fn lowered(units: &Units) -> blaze_lower::Lowered {
    let tree = blaze_query::MemTree::default();
    let core = core_source();
    let library = tree.root(&[
        ("core/Cargo.toml", b"[package]\nname = \"core\"\n"),
        ("core/src/lib.rs", core.as_bytes()),
    ]);
    let view = tree.root(&[
        ("gpu/Cargo.toml", b"[package]\nname = \"gpu\"\n"),
        ("gpu/src/lib.rs", GPU.as_bytes()),
        ("shaders/Cargo.toml", b"[package]\nname = \"shaders\"\n"),
        ("shaders/src/lib.rs", SHADERS.as_bytes()),
        (
            "app/Cargo.toml",
            b"[package]\nname = \"app\"\n\n[dependencies]\nshaders = { path = \"../shaders\", artifact = \"gpu\" }\n",
        ),
        ("app/src/lib.rs", APP.as_bytes()),
    ]);
    let config = BuildConfig {
        invocation: Invocation {
            command: Command::Build,
            args: Vec::new(),
            root: "/w".to_owned(),
            cwd: "/w/app".to_owned(),
            env: Vec::new(),
            files: Vec::new(),
        },
        mounts: vec![
            Mount {
                at: "/rustc/abc/library".to_owned(),
                origin: Origin::Root(library),
                under: String::new(),
            },
            Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() },
        ],
        host: HOST.to_owned(),
        library: "/rustc/abc/library".to_owned(),
        std: "/s/std".to_owned(),
        builds: "/s/build".to_owned(),
        sources: "/s/sources".to_owned(),
        blaze: String::new(),
    };
    blaze_lower::lower(blaze_lower::Lower {
        units,
        config: &config,
        files: &tree,
        root: view,
        host_cpu: None,
    })
    .unwrap()
}

/// The device build of the shader crate in `lowered`, which must lower and validate.
fn device_build(lowered: &blaze_lower::Lowered) -> blaze_shader::device::DeviceBuild {
    let krate = lowered.krate(SHADERS_UNIT).expect("the shader crate's device unit has a crate");
    match blaze_shader::device::device_msl(lowered.db(), krate) {
        Ok(built) => built,
        Err(error) => panic!("the device build of `shaders` failed:\n{error}"),
    }
}

/// The host crate's one static, `MSL`, evaluated: its `include_bytes!` read through the front
/// end, which notes the file as read by the crate (`base_db::Files::included_by`).
fn host_reads(lowered: &blaze_lower::Lowered) -> Vec<(base_db::AbsPathBuf, blaze_ids::ContentId)> {
    use hir_ty::db::HirDatabase as _;
    let db = lowered.db();
    let krate = lowered.krate(APP_UNIT).expect("the host crate has a crate");
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    let statik = def_map
        .module(db, def_map.crate_root(db))
        .scope(db)
        .declarations()
        .find_map(|def| match def {
            hir_def::ModuleDefId::StaticId(it) => Some(it),
            _ => None,
        })
        .expect("the host crate's root has the static `MSL`");
    // Its value is not what is tested, only what its initializer read.
    let _ = db.const_eval_static(statik);
    db.files().included_by(db, krate)
}

/// The host crate waits on the device unit's MSL as it would on a build script's run: until
/// the writer applies it, its crate is not final (`graph::flag::GPU_MSL`), and a device unit is
/// linked into no host image.
#[test]
fn a_hosts_crate_waits_on_its_gpu_dependencys_msl() {
    let units = units();
    let graph = graph::Graph::new(&units).unwrap();
    assert!(graph.has(SHADERS_UNIT as u32, graph::flag::GPU_MSL));
    assert!(graph.has(SHADERS_UNIT as u32, graph::flag::DEVICE));
    assert!(!graph.has(GPU_CRATE as u32, graph::flag::GPU_MSL), "only the gpu edge's unit writes MSL");
    assert!(graph.has(GPU_CRATE as u32, graph::flag::DEVICE));
    assert!(!graph.has(APP_UNIT as u32, graph::flag::DEVICE));
    let mut waits = graph::Waits::new(&graph);
    assert!(waits.ready(SHADERS_UNIT as u32) && !waits.ready(APP_UNIT as u32));
    let mut ready = Vec::new();
    waits.applied(SHADERS_UNIT as u32, &mut ready);
    assert_eq!(ready, [APP_UNIT as u32]);
}

/// The MSL a device unit writes is naga-validated (`device_msl` validates the module before it
/// writes), holds its entry under its own name, and is the same bytes from two front ends made
/// apart (rule 25); applied, it is what the host's `include_bytes!` of the file
/// `BLAZE_GPU_MSL_SHADERS` names reads, with nothing on disk at that path.
#[test]
fn a_gpu_dependencys_msl_is_validated_deterministic_and_read_by_its_dependent() {
    let units = units();
    let mut first = lowered(&units);
    let built = device_build(&first);
    assert_eq!(built.entries.len(), 1);
    assert_eq!(built.entries[0].name, "vs");
    assert!(built.msl.contains("vertex"), "a vertex entry point:\n{}", built.msl);
    assert!(built.msl.contains("vs"), "the entry under its own name:\n{}", built.msl);
    let again = device_build(&lowered(&units));
    assert_eq!(built.msl.as_bytes(), again.msl.as_bytes(), "two device builds of one tree differ");

    assert!(!std::path::Path::new(MSL_PATH).exists(), "the test reads nothing from disk");
    let msl = built.msl.as_bytes();
    first.gpu_msl(SHADERS_UNIT, msl).unwrap();
    let path = base_db::AbsPathBuf::try_from(MSL_PATH).unwrap();
    let read = host_reads(&first);
    let content = base_db::IncludedFile::content_of(Ok(msl));
    assert_eq!(read, [(path.clone(), content)], "the host read the device build's MSL");

    // Applied again with other bytes (an edit of the shader), the host reads those.
    let edited = [msl, b"\n// edited\n".as_slice()].concat();
    first.gpu_msl(SHADERS_UNIT, &edited).unwrap();
    let content = base_db::IncludedFile::content_of(Ok(edited.as_slice()));
    assert_eq!(host_reads(&first), [(path, content)]);
}

/// A unit that writes no MSL is refused naming it, never applied: the host's and the gpu
/// crate's alike.
#[test]
fn only_a_gpu_dependencys_unit_has_msl_to_apply() {
    let mut lowered = lowered(&units());
    for unit in [GPU_CRATE, APP_UNIT] {
        let error = lowered.gpu_msl(unit, b"").unwrap_err().to_string();
        assert!(error.contains("writes no MSL"), "{error}");
        assert!(error.contains(lowered.what(unit)), "{error}");
    }
    assert!(lowered.check_gpu_msl(SHADERS_UNIT).is_ok_and(|it| it == MSL_PATH));
}
