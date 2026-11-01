use blaze_cas::{Config, Store};
use blaze_codegen::driver::Digests;
use blaze_codegen::driver::digests::{debug_name_digest, function_digest};
use blaze_codegen::settings::{Coverage, Relocation, Sanitize, Sanitizers, Toolchain};
use blaze_codegen::{
    Arch, CompiledFunction, Init, InstanceKey, RelocKind, Relocation as Reloc, Settings, Static,
    Target,
};
use blaze_core::sim::{Faults, Sim};
use blaze_core::{Effect, FileName, run_until_idle};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use blaze_link::{Debug, DevRun, Format, Native, NativeKind, Output, OutputKind, Platform};

use crate::PremapError;
use crate::cache::{Images, Pick, Ready};
use crate::dev::{self, Dev, Plan};
use crate::image::View;
use crate::key::{
    DepUnit, GraphInputs, GraphKey, ImageInputs, ImageKey, KeyedFunction, KeyedStatic, NativeInput,
    static_definition,
};
use crate::model::{self, Libraries, Model, PAGE, TLS_BIAS};
use crate::prelink::{
    self, Bind, BindKind, Library, PlacedName, Prelink, Prelinked, Prot, Request, Segment,
};
use crate::split::{
    Built, Fit, Owned, SidedNative, Stale, WorkspacePart, image_part, workspace_part,
};
use crate::start::{self, Write};
use crate::store::Shelf;
use crate::table::{self, Def, Kind, Refs, Strength, SymbolId, TableView};

/// `prelink::base` of the tests' ELF x86-64 output.
const BASE: u64 = 0x40_0000;

fn id(n: u64) -> ContentId {
    ContentId::of("blaze v2 premap test 1", &n.to_le_bytes())
}

fn ikey(n: u64) -> InstanceKey {
    InstanceKey(id(n))
}

fn item(n: u64) -> ItemKey {
    ItemKey(id(1_000 + n))
}

fn instance(n: u64) -> ContentId {
    id(2_000 + n)
}

fn function(n: u64, relocs: Vec<Reloc>) -> CompiledFunction {
    CompiledFunction {
        key: ikey(n),
        arch: Arch::X86_64,
        align: 16,
        code: vec![0xcc; 32],
        relocs: relocs.into(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        coverage: None,
        profile: None,
    }
}

fn reloc(at: u32, kind: RelocKind, target: Target) -> Reloc {
    Reloc { at, kind, target, addend: 0 }
}

fn of_static(n: u64) -> Target {
    Target::Static { item: item(n), instance: instance(n) }
}

fn value(n: u64, init: Init) -> Static {
    Static {
        item: item(n),
        instance: instance(n),
        align: 8,
        init,
        data: Vec::new(),
        mutable: false,
        thread_local: false,
        section: None,
        imports: Vec::new(),
    }
}

fn pointer_to(target: Target) -> Init {
    Init::Bytes { bytes: vec![0; 8], relocs: vec![reloc(0, RelocKind::Abs64, target)].into() }
}

fn toolchain() -> Toolchain {
    Toolchain {
        id: id(9_000),
        target: "x86_64-unknown-linux-gnu".to_owned(),
        arch: Arch::X86_64,
        cpu: "x86-64".to_owned(),
        features: vec!["+sse2".to_owned()],
        cranelift: "0.120".to_owned(),
        llvm: None,
        stencils: None,
        frame_pointers: true,
        relocation: Relocation::Pic,
    }
}

fn dev_output() -> Output {
    Output {
        format: Format::Elf,
        arch: Arch::X86_64,
        kind: OutputKind::Executable { entry: "_start".to_owned() },
        interpreter: Some("/lib64/ld-linux-x86-64.so.2".to_owned()),
        platform: None,
        incremental: true,
        debug: Debug::Lines,
        live: None,
        tags: 1 << 48,
        stack: None,
        windows_subsystem: false,
        dev_run: None,
        triple: "x86_64-unknown-linux-gnu".to_owned(),
        exec_log: false,
        sanitize: Sanitize::default(),
        rpaths: Vec::new(),
    }
}

/// The inputs a graph key is made from, owned, so a test can change one.
#[derive(Clone)]
struct Graph {
    units: Vec<(String, String, Vec<String>, Vec<String>, Vec<String>)>,
    toolchain: Toolchain,
    settings: Vec<Settings>,
    linker: ContentId,
    output: Output,
    base: u64,
    tls_bias: i64,
    natives: Vec<NativeInput>,
    spans: Vec<(ContentId, ContentId)>,
}

impl Graph {
    fn new() -> Graph {
        Graph {
            units: vec![
                ("serde 1.0.200 (registry)".to_owned(), "lib".to_owned(), vec!["std".to_owned()], Vec::new(), Vec::new()),
                ("libc 0.2.150 (registry)".to_owned(), "lib".to_owned(), Vec::new(), vec!["freebsd11".to_owned()], Vec::new()),
            ],
            toolchain: toolchain(),
            settings: vec![Settings(id(9_100))],
            linker: id(9_200),
            output: dev_output(),
            base: BASE,
            tls_bias: TLS_BIAS,
            natives: vec![NativeInput { id: id(9_300), kind: NativeKind::Shared { as_needed: true }, order: 1 }],
            spans: vec![(id(9_400), id(9_401))],
        }
    }

    fn key(&self) -> GraphKey {
        let units: Vec<DepUnit<'_>> = self
            .units
            .iter()
            .map(|(package, target, features, cfg, rustflags)| DepUnit { package, target, features, cfg, rustflags })
            .collect();
        GraphKey::of(&GraphInputs {
            units: &units,
            toolchain: &self.toolchain,
            settings: &self.settings,
            linker: self.linker,
            output: &self.output,
            base: self.base,
            tls_bias: self.tls_bias,
            natives: &self.natives,
            spans: &self.spans,
        })
    }
}

#[test]
fn graph_key_changes_with_every_input() {
    let base = Graph::new().key();
    let changes: Vec<(&str, Box<dyn Fn(&mut Graph)>)> = vec![
        ("a unit's package", Box::new(|it: &mut Graph| it.units[0].0.push('x'))),
        ("a unit's target", Box::new(|it: &mut Graph| it.units[0].1 = "bin".to_owned())),
        ("a unit's features", Box::new(|it: &mut Graph| it.units[0].2.push("derive".to_owned()))),
        ("a unit's cfg", Box::new(|it: &mut Graph| it.units[1].3.clear())),
        ("a unit's rustflags", Box::new(|it: &mut Graph| it.units[1].4.push("-Cdebug-assertions".to_owned()))),
        ("a unit more", Box::new(|it: &mut Graph| it.units.push(("cc 1.0 (registry)".to_owned(), "lib".to_owned(), vec![], vec![], vec![])))),
        ("the toolchain's id", Box::new(|it: &mut Graph| it.toolchain.id = id(1))),
        ("the target triple", Box::new(|it: &mut Graph| it.toolchain.target = "x86_64-unknown-linux-musl".to_owned())),
        ("the toolchain's arch", Box::new(|it: &mut Graph| it.toolchain.arch = Arch::Aarch64)),
        ("the cpu", Box::new(|it: &mut Graph| it.toolchain.cpu = "znver4".to_owned())),
        ("target features", Box::new(|it: &mut Graph| it.toolchain.features.push("+avx2".to_owned()))),
        ("cranelift", Box::new(|it: &mut Graph| it.toolchain.cranelift = "0.121".to_owned())),
        ("llvm", Box::new(|it: &mut Graph| it.toolchain.llvm = Some("20".to_owned()))),
        ("the stencils", Box::new(|it: &mut Graph| it.toolchain.stencils = Some("00".to_owned()))),
        ("frame pointers", Box::new(|it: &mut Graph| it.toolchain.frame_pointers = false)),
        ("relocation", Box::new(|it: &mut Graph| it.toolchain.relocation = Relocation::Static)),
        ("codegen settings", Box::new(|it: &mut Graph| it.settings.push(Settings(id(2))))),
        ("the linker", Box::new(|it: &mut Graph| it.linker = id(3))),
        ("the format", Box::new(|it: &mut Graph| it.output.format = Format::MachO)),
        ("the output's arch", Box::new(|it: &mut Graph| it.output.arch = Arch::Aarch64)),
        ("the output kind", Box::new(|it: &mut Graph| it.output.kind = OutputKind::Executable { entry: "main".to_owned() })),
        ("the interpreter", Box::new(|it: &mut Graph| it.output.interpreter = None)),
        ("the platform", Box::new(|it: &mut Graph| it.output.platform = Some(Platform { platform: 1, minimum: 0x000e_0000, sdk: 0x000f_0000 }))),
        ("incremental", Box::new(|it: &mut Graph| it.output.incremental = false)),
        ("debug info", Box::new(|it: &mut Graph| it.output.debug = Debug::None)),
        ("tags", Box::new(|it: &mut Graph| it.output.tags = 2 << 48)),
        ("the stack", Box::new(|it: &mut Graph| it.output.stack = Some(8 << 20))),
        ("the windows subsystem", Box::new(|it: &mut Graph| it.output.windows_subsystem = true)),
        ("a dev run of no reservations", Box::new(|it: &mut Graph| it.output.dev_run = Some(DevRun { arena: 0, cells: 0, fixed: 0 }))),
        ("a dev run's reservation", Box::new(|it: &mut Graph| it.output.dev_run = Some(DevRun { arena: 1 << 30, cells: 0, fixed: 0 }))),
        ("the output's triple", Box::new(|it: &mut Graph| it.output.triple = "x86_64-unknown-linux-musl".to_owned())),
        ("the execution log", Box::new(|it: &mut Graph| it.output.exec_log = true)),
        ("a sanitizer", Box::new(|it: &mut Graph| it.output.sanitize.sanitizers = Sanitizers::ADDRESS)),
        ("a recovering sanitizer", Box::new(|it: &mut Graph| it.output.sanitize.recover = Sanitizers::ADDRESS)),
        ("origin tracking", Box::new(|it: &mut Graph| it.output.sanitize.track_origins = 2)),
        ("the coverage level", Box::new(|it: &mut Graph| it.output.sanitize.coverage.level = 3)),
        ("the coverage flags", Box::new(|it: &mut Graph| it.output.sanitize.coverage.flags = Coverage::TRACE_PC_GUARD)),
        ("an rpath", Box::new(|it: &mut Graph| it.output.rpaths.push("/opt/blaze/runtime".to_owned()))),
        ("the base", Box::new(|it: &mut Graph| it.base += 1 << 20)),
        ("the thread-local bias", Box::new(|it: &mut Graph| it.tls_bias -= 64)),
        ("a native's contents", Box::new(|it: &mut Graph| it.natives[0].id = id(4))),
        ("a native's kind", Box::new(|it: &mut Graph| it.natives[0].kind = NativeKind::Shared { as_needed: false })),
        ("a native's order", Box::new(|it: &mut Graph| it.natives[0].order = 2)),
        ("a native more", Box::new(|it: &mut Graph| it.natives.push(NativeInput { id: id(5), kind: NativeKind::Object, order: 0 }))),
        ("a span table", Box::new(|it: &mut Graph| it.spans[0].1 = id(6))),
        ("a span anchor", Box::new(|it: &mut Graph| it.spans[0].0 = id(7))),
    ];
    let mut seen = vec![base];
    for (what, change) in changes {
        let mut graph = Graph::new();
        change(&mut graph);
        let key = graph.key();
        assert!(!seen.contains(&key), "changing {what} left the graph key as another one");
        seen.push(key);
    }
}

#[test]
fn graph_key_ignores_order_of_sets() {
    let mut graph = Graph::new();
    graph.units[0].2 = vec!["a".to_owned(), "b".to_owned()];
    let one = graph.key();
    graph.units[0].2 = vec!["b".to_owned(), "a".to_owned()];
    graph.units.reverse();
    assert_eq!(one, graph.key());
    graph.units[0].4 = vec!["-Ca".to_owned(), "-Cb".to_owned()];
    let two = graph.key();
    graph.units[0].4.reverse();
    assert_ne!(two, graph.key(), "rustflags' order is their meaning");
}

#[test]
fn image_key_changes_with_every_instance() {
    let graph = Graph::new().key();
    let functions = vec![KeyedFunction { key: ikey(1), code: CodeHash(id(11)), inlined: None }];
    let static_value = value(1, Init::Zeroed { size: 8 });
    let statics = vec![KeyedStatic { item: item(1), instance: instance(1), definition: static_definition(&static_value) }];
    let names = vec![("ffi_entry".to_owned(), Target::Function(ikey(1)))];
    let debug_names = vec![(ikey(1), "dep::f".to_owned())];
    let key = |functions: &[KeyedFunction], statics: &[KeyedStatic], names: &[(String, Target)], debug: &[(InstanceKey, String)]| {
        let names: Vec<&(String, Target)> = names.iter().collect();
        let debug: Vec<&(InstanceKey, String)> = debug.iter().collect();
        ImageKey::of(graph, &ImageInputs { functions, statics, names: &names, debug_names: &debug })
    };
    let base = key(&functions, &statics, &names, &debug_names);
    let mut seen = vec![base];
    let mut push = |what: &str, it: ImageKey| {
        assert!(!seen.contains(&it), "changing {what} left the image key as another one");
        seen.push(it);
    };
    push("a function's key", key(&[KeyedFunction { key: ikey(2), ..functions[0] }], &statics, &names, &debug_names));
    push("a function's code hash", key(&[KeyedFunction { code: CodeHash(id(12)), ..functions[0] }], &statics, &names, &debug_names));
    push("a function more", key(&[functions[0], KeyedFunction { key: ikey(3), code: CodeHash(id(13)), inlined: None }], &statics, &names, &debug_names));
    let inlined = crate::key::inlined_digest(&[CodeHash(id(14))]);
    push("what a function inlined", key(&[KeyedFunction { inlined, ..functions[0] }], &statics, &names, &debug_names));
    let changed = value(1, Init::Zeroed { size: 16 });
    push("a static's value", key(&functions, &[KeyedStatic { definition: static_definition(&changed), ..statics[0] }], &names, &debug_names));
    let mutable = Static { mutable: true, ..value(1, Init::Zeroed { size: 8 }) };
    push("a static's mutability", key(&functions, &[KeyedStatic { definition: static_definition(&mutable), ..statics[0] }], &names, &debug_names));
    let section = Static { section: Some(".init_array".to_owned()), ..value(1, Init::Zeroed { size: 8 }) };
    push("a static's section", key(&functions, &[KeyedStatic { definition: static_definition(&section), ..statics[0] }], &names, &debug_names));
    push("a static's instance", key(&functions, &[KeyedStatic { instance: instance(2), ..statics[0] }], &names, &debug_names));
    push("a name", key(&functions, &statics, &[("other".to_owned(), Target::Function(ikey(1)))], &debug_names));
    push("what a name is", key(&functions, &statics, &[("ffi_entry".to_owned(), Target::Direct(ikey(1)))], &debug_names));
    push("a debug name", key(&functions, &statics, &names, &[(ikey(1), "dep::g".to_owned())]));
    let (names, debug_names): (Vec<_>, Vec<_>) = (names.iter().collect(), debug_names.iter().collect());
    let other_graph = ImageKey::of(GraphKey(id(1)), &ImageInputs { functions: &functions, statics: &statics, names: &names, debug_names: &debug_names });
    push("the graph", other_graph);
}

#[test]
fn table_finds_every_entry_and_refuses_corruption() {
    let entries: Vec<(SymbolId, Def)> = (0..100)
        .map(|n| {
            let def = Def { value: BASE + n * 16, size: 16, kind: Kind::Function, strength: Strength::Strong, order: u32::MAX };
            (SymbolId::function(&ikey(n)), def)
        })
        .collect();
    let bytes = table::build(&entries).unwrap();
    let view = TableView::parse(&bytes).unwrap();
    assert_eq!(view.len(), entries.len());
    for (id, def) in &entries {
        assert_eq!(view.get(id), Some(*def));
    }
    assert_eq!(view.get(&SymbolId::function(&ikey(100))), None);
    assert_ne!(SymbolId::function(&ikey(1)).0, SymbolId::name("f").0);

    let mut twice = entries.clone();
    twice.push(entries[0]);
    assert!(table::build(&twice).is_err(), "a symbol twice is refused");

    let slots: Vec<&[u8]> = bytes.chunks(64).collect();
    let used = slots.iter().position(|it| it[..32] != [0; 32]).unwrap();
    let empty = slots.iter().position(|it| it[..32] == [0; 32]).unwrap();
    let mut corrupt = bytes.clone();
    corrupt[used * 64 + 52] = 0xff;
    assert!(TableView::parse(&corrupt).is_err(), "a slot of no kind is refused");
    let mut corrupt = bytes.clone();
    corrupt[empty * 64 + 40] = 1;
    assert!(TableView::parse(&corrupt).is_err(), "an empty slot with bytes is refused");
    assert!(TableView::parse(&bytes[..bytes.len() - 1]).is_err(), "a cut table is refused");

    let mut referring = bytes.clone();
    table::set_content(&mut referring, &entries[3].0, 2).unwrap();
    table::set_debug(&mut referring, &entries[5].0, 1).unwrap();
    assert!(TableView::parse(&referring).is_err(), "a ref past what the image has is refused");
    assert!(TableView::parse_with(&referring, 1, 1).is_err(), "a content ref one past the last");
    let view = TableView::parse_with(&referring, 2, 1).unwrap();
    assert_eq!(view.entry(&entries[3].0).unwrap().refs, Refs { content: 2, debug: 0 });
    assert_eq!(view.entry(&entries[5].0).unwrap().refs, Refs { content: 0, debug: 1 });
    assert_eq!(view.get(&entries[5].0), Some(entries[5].1), "a ref leaves the definition as it was");
    assert!(table::set_content(&mut referring, &SymbolId::function(&ikey(100)), 1).is_err(), "a symbol it has not");
}

/// A small dependency graph: two dependency functions, a static pointing at one, a
/// thread-local and an init function; two workspace functions (one the dependencies' hook
/// by name), a thread-local of its own and an init function.
#[derive(Clone)]
struct Small {
    functions: Vec<CompiledFunction>,
    statics: Vec<Static>,
    names: Vec<(String, Target)>,
    debug_names: Vec<(InstanceKey, String)>,
    owned_functions: Vec<(InstanceKey, CodeHash)>,
    owned_statics: Vec<(ItemKey, ContentId)>,
}

const LIBRARIES: Libraries<'static> = &[("malloc", 0x7000_0000), ("puts", 0x7000_1000)];

impl Small {
    fn new() -> Small {
        let d1 = function(1, vec![reloc(0, RelocKind::PcRel32, Target::Direct(ikey(2))), reloc(8, RelocKind::Abs64, of_static(1))]);
        let d2 = function(
            2,
            vec![
                reloc(0, RelocKind::Abs64, Target::Extern("malloc".to_owned())),
                reloc(8, RelocKind::Abs64, Target::Extern("__rg_alloc".to_owned())),
                reloc(16, RelocKind::Abs64, Target::Weak("absent".to_owned())),
                reloc(24, RelocKind::TlsOffset, of_static(2)),
            ],
        );
        let w1 = function(
            11,
            vec![
                reloc(0, RelocKind::PcRel32, Target::Function(ikey(1))),
                reloc(4, RelocKind::TlsOffset, of_static(12)),
                reloc(8, RelocKind::Abs64, of_static(1)),
                reloc(16, RelocKind::Abs64, Target::Extern("puts".to_owned())),
                reloc(24, RelocKind::TlsOffset, of_static(2)),
            ],
        );
        let w2 = function(12, Vec::new());
        let s1 = value(1, pointer_to(Target::Function(ikey(2))));
        let t1 = Static { thread_local: true, align: 16, ..value(2, Init::Zeroed { size: 16 }) };
        let i1 = Static { section: Some(".init_array".to_owned()), ..value(3, pointer_to(Target::Function(ikey(1)))) };
        let i2 = Static { section: Some(".init_array".to_owned()), ..value(13, pointer_to(Target::Function(ikey(11)))) };
        let t2 = Static { thread_local: true, align: 4, ..value(12, Init::Bytes { bytes: vec![1, 2, 3, 4], relocs: Vec::new().into() }) };
        let mut owned_functions = vec![(ikey(1), CodeHash(id(101))), (ikey(2), CodeHash(id(102)))];
        owned_functions.sort();
        let mut owned_statics = vec![(item(1), instance(1)), (item(2), instance(2)), (item(3), instance(3))];
        owned_statics.sort();
        let mut statics = vec![s1, i2, t1, i1, t2];
        statics.sort_by_key(|it| (it.item, it.instance));
        Small {
            functions: vec![d1, w1, d2, w2],
            statics,
            names: vec![("__rg_alloc".to_owned(), Target::Function(ikey(12)))],
            debug_names: Vec::new(),
            owned_functions,
            owned_statics,
        }
    }

    /// The build as codegen gives it, its digests made from what the test left in it. They
    /// are leaked so a test's `Built` borrows from `self` alone.
    fn built(&self) -> Built<'_> {
        let digests = Digests {
            functions: self.functions.iter().map(|it| (it.key, function_digest(CodeHash(it.key.0), &it.inlined))).collect(),
            statics: self.statics.iter().map(|it| ((it.item, it.instance), static_definition(it))).collect(),
            debug_names: self.debug_names.iter().map(|(key, name)| (*key, debug_name_digest(name))).collect(),
        };
        let digests: &'static Digests = Box::leak(Box::new(digests));
        Built { functions: &self.functions, statics: &self.statics, names: &self.names, debug_names: &self.debug_names, digests }
    }

    fn owned(&self) -> Owned<'_> {
        Owned { functions: &self.owned_functions, statics: &self.owned_statics }
    }
}

fn natives() -> Vec<SidedNative<'static>> {
    vec![SidedNative {
        native: Native { id: id(9_300), name: "libc.so.6", bytes: Box::leak(Box::new(std::sync::Arc::<[u8]>::from([]))), kind: NativeKind::Shared { as_needed: true }, order: 1 },
        dependency: true,
    }]
}

fn part_of(fit: Fit<'_>) -> WorkspacePart<'_> {
    match fit {
        Fit::Part(part) => part,
        Fit::Stale(stale) => panic!("the image is stale for the build that made it: {stale:?}"),
    }
}

fn prelink_request<'a>(output: &'a Output, part: &'a crate::split::ImagePart<'a>) -> Request<'a> {
    Request {
        link: blaze_link::premap::Request {
            output,
            base: BASE,
            tls_bias: TLS_BIAS,
            functions: &part.functions,
            statics: &part.statics,
            names: &part.names,
            debug_names: &part.debug_names,
            natives: &part.natives,
            spans: &[],
        },
        function_digests: &part.function_digests,
        static_digests: &part.static_digests,
        debug_digests: &part.debug_digests,
    }
}

#[test]
fn prelinked_and_workspace_link_run_as_the_whole_link() {
    let small = Small::new();
    let natives = natives();
    let part = image_part(&small.built(), &small.owned(), &natives).unwrap();
    let output = prelink::output(&dev_output(), "blaze-premap-test");
    let key = ImageKey(id(77));
    let bytes = prelink::build(&Model, &prelink_request(&output, &part), key).unwrap();
    let image = View::parse(&bytes, key).unwrap();
    assert_eq!(image.libraries().map(|it| it.name).collect::<Vec<_>>(), ["libc.so.6"]);

    let workspace = part_of(workspace_part(&small.built(), &small.owned(), &image, &natives).unwrap());
    assert!(workspace.complete());
    assert_eq!(workspace.functions.iter().map(|it| it.key).collect::<Vec<_>>(), [ikey(11), ikey(12)]);
    assert!(workspace.natives.iter().all(|it| matches!(it.kind, NativeKind::Shared { .. })), "a shared library is in both");

    let layout = model::link_workspace(&workspace, &image).unwrap();
    let names = model::workspace_names(&layout, &workspace).unwrap();
    let start = model::start_of(&image, &names).unwrap();
    assert_eq!(start.lookups.iter().map(|it| it.name).collect::<Vec<_>>(), ["malloc", "absent"], "only names no side defines are looked up");

    let whole_image: Vec<&CompiledFunction> =
        small.functions.iter().filter(|it| small.owned_functions.iter().any(|o| o.0 == it.key)).collect();
    let whole_statics: Vec<&Static> =
        small.statics.iter().filter(|it| small.owned_statics.contains(&(it.item, it.instance))).collect();
    let whole = model::whole(
        (&whole_image, &whole_statics, &part.names),
        (&workspace.functions, &workspace.statics, &workspace.names),
        BASE,
        LIBRARIES,
    )
    .unwrap();

    let keys: Vec<InstanceKey> = small.functions.iter().map(|it| it.key).collect();
    let statics: Vec<(ItemKey, ContentId)> = small.statics.iter().map(|it| (it.item, it.instance)).collect();
    let split = model::split(&image, &layout, &names, &start, LIBRARIES, (&keys, &statics)).unwrap();
    assert_eq!(split.functions, whole.functions, "every function at the whole link's address");
    assert_eq!(split.statics, whole.statics, "every static at the whole link's address or offset");
    assert_eq!(split.init, whole.init, "init functions run in the whole link's order");
    let mut pieces = [(item(3), instance(3), ikey(1)), (item(13), instance(13), ikey(11))];
    pieces.sort_by_key(|(item, instance, _)| blaze_link::premap::InitEntry::tie_of(item, instance));
    assert_eq!(split.init, pieces.map(|(_, _, function)| whole.functions[&function]), "unranked pieces run by their ties");
    assert_eq!(split.tls, whole.tls, "the thread-local area as the whole link lays it out");
    assert_eq!(split.memory, whole.memory, "every byte the program starts with");
}

fn image_from(small: &Small, key: ImageKey) -> Vec<u8> {
    let natives = natives();
    let part = image_part(&small.built(), &small.owned(), &natives).unwrap();
    let output = prelink::output(&dev_output(), "blaze-premap-test");
    prelink::build(&Model, &prelink_request(&output, &part), key).unwrap()
}

#[test]
fn an_image_serves_only_a_build_whose_pieces_it_holds_are_its_own() {
    let mut small = Small::new();
    small.names.push(("dep_entry".to_owned(), Target::Function(ikey(2))));
    small.debug_names = vec![(ikey(1), "dep::one".to_owned()), (ikey(11), "app::main".to_owned())];
    let key = ImageKey(id(82));
    let bytes = image_from(&small, key);
    let image = View::parse(&bytes, key).unwrap();
    assert_eq!((image.names(), image.debug_names()), (1, 1));
    let natives = natives();
    let stale = |build: &Small| match workspace_part(&build.built(), &build.owned(), &image, &natives).unwrap() {
        Fit::Part(_) => None,
        Fit::Stale(it) => Some(it),
    };
    let workspace = part_of(workspace_part(&small.built(), &small.owned(), &image, &natives).unwrap());
    assert_eq!(workspace.names, [&small.names[0]], "the image's name is not linked again");
    assert_eq!(workspace.debug_names, [&small.debug_names[1]]);

    let changes: Vec<(&str, Box<dyn Fn(&mut Small)>, Option<Stale>)> = vec![
        ("a workspace function's inlined callees", Box::new(|it: &mut Small| it.functions[1].inlined = vec![CodeHash(id(500))]), None),
        (
            "a dependency function's inlined callees",
            Box::new(|it: &mut Small| it.functions[0].inlined = vec![CodeHash(id(500))]),
            Some(Stale::Function(ikey(1))),
        ),
        (
            "a dependency static's value",
            Box::new(|it: &mut Small| it.statics[0] = value(1, pointer_to(Target::Function(ikey(1))))),
            Some(Stale::Static(item(1), instance(1))),
        ),
        ("what a name is", Box::new(|it: &mut Small| it.names[1].1 = Target::Function(ikey(1))), Some(Stale::Name(1))),
        ("a name", Box::new(|it: &mut Small| it.names[1].0 = "dep_other".to_owned()), Some(Stale::Name(1))),
        ("a name the image has, gone", Box::new(|it: &mut Small| drop(it.names.pop())), Some(Stale::Extra)),
        ("a debug name", Box::new(|it: &mut Small| it.debug_names[0].1 = "dep::uno".to_owned()), Some(Stale::Function(ikey(1)))),
        ("a debug name the image has, gone", Box::new(|it: &mut Small| drop(it.debug_names.remove(0))), Some(Stale::Extra)),
        (
            "an init static the image has, gone",
            Box::new(|it: &mut Small| {
                it.statics.retain(|it| it.item != item(3));
                it.owned_statics.retain(|it| it.0 != item(3));
            }),
            Some(Stale::Extra),
        ),
    ];
    for (what, change, expected) in changes {
        let mut build = small.clone();
        change(&mut build);
        assert_eq!(stale(&build), expected, "{what}");
    }
}

#[test]
fn a_build_the_newest_image_does_not_serve_makes_its_own() {
    let mut images = Images::default();
    let mut shelf = Shelved::default();
    let natives = natives();
    let first = with_dev(&Small::new(), &natives, |dev| {
        let Plan::Make(job) = dev::plan(&mut images, dev).unwrap() else { panic!() };
        let made = job.make(&Model, &mut shelf);
        images.finish(job.key, made).unwrap().key
    });
    let mut edited = Small::new();
    edited.functions[0].inlined = vec![CodeHash(id(500))];
    with_dev(&edited, &natives, |dev| {
        let Plan::Make(job) = dev::plan(&mut images, dev).unwrap() else { panic!("a stale image is never linked against") };
        assert_ne!(job.key, first);
        assert!(matches!(dev::plan(&mut images, dev).unwrap(), Plan::Wait), "a second build waits for it");
        let made = job.make(&Model, &mut shelf);
        images.finish(job.key, made).unwrap();
        let Plan::Link { image, workspace, next } = dev::plan(&mut images, dev).unwrap() else { panic!() };
        assert_eq!(image.key, job.key);
        assert!(workspace.complete() && next.is_none());
    });
}

#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[test]
fn a_graph_key_allocates_nothing_of_the_heap() {
    let graph = Graph::new();
    let units: Vec<DepUnit<'_>> = graph
        .units
        .iter()
        .map(|(package, target, features, cfg, rustflags)| DepUnit { package, target, features, cfg, rustflags })
        .collect();
    let inputs = GraphInputs {
        units: &units,
        toolchain: &graph.toolchain,
        settings: &graph.settings,
        linker: graph.linker,
        output: &graph.output,
        base: graph.base,
        tls_bias: graph.tls_bias,
        natives: &graph.natives,
        spans: &graph.spans,
    };
    let warm = GraphKey::of(&inputs);
    let (key, used) = blaze_alloc_count::measure(|| GraphKey::of(&inputs));
    assert_eq!(key, warm);
    assert_eq!(used, blaze_alloc_count::Used::default());
}

#[test]
fn an_image_key_allocates_nothing_of_the_heap() {
    let small = Small::new();
    let natives = natives();
    let part = image_part(&small.built(), &small.owned(), &natives).unwrap();
    let inputs = ImageInputs {
        functions: &part.keyed_functions,
        statics: &part.keyed_statics,
        names: &part.names,
        debug_names: &part.debug_names,
    };
    let warm = ImageKey::of(GraphKey(id(1)), &inputs);
    let (key, used) = blaze_alloc_count::measure(|| ImageKey::of(GraphKey(id(1)), &inputs));
    assert_eq!(key, warm);
    assert_eq!(used, blaze_alloc_count::Used::default());
}

#[test]
fn splitting_a_build_copies_no_name() {
    let mut small = Small::new();
    small.names.push(("dep_entry".to_owned(), Target::Function(ikey(2))));
    let natives = natives();
    let built = small.built();
    drop(image_part(&built, &small.owned(), &natives).unwrap());
    let (part, used) = blaze_alloc_count::measure(|| image_part(&built, &small.owned(), &natives).unwrap());
    assert_eq!(part.names.len(), 1);
    assert!(part.names.iter().all(|it| built.names.iter().any(|b| std::ptr::eq(b, *it))), "the part's names are the build's");
    assert!(used.allocations <= 10, "the part's ten lists, no `String`: {used:?}");
}

#[test]
fn a_hit_allocates_its_lists_and_nothing_per_piece() {
    let mut small = Small::new();
    small.debug_names = vec![(ikey(1), "dep::one".to_owned()), (ikey(11), "app::main".to_owned())];
    let key = ImageKey(id(83));
    let bytes = image_from(&small, key);
    let image = View::parse(&bytes, key).unwrap();
    let natives = natives();
    let built = small.built();
    let check = || part_of(workspace_part(&built, &small.owned(), &image, &natives).unwrap());
    drop(check());
    let (workspace, used) = blaze_alloc_count::measure(check);
    let lists = [
        workspace.functions.is_empty(),
        workspace.statics.is_empty(),
        workspace.names.is_empty(),
        workspace.debug_names.is_empty(),
        workspace.natives.is_empty(),
        workspace.spilled.is_empty(),
        workspace.spilled_statics.is_empty(),
    ];
    let filled = lists.iter().filter(|it| !**it).count() as u64;
    assert_eq!(used.allocations, filled, "one exact allocation per list the workspace link gets, none per piece");
}

#[test]
fn image_refuses_a_dependency_that_reaches_into_the_workspace() {
    let mut small = Small::new();
    small.functions[2].relocs.push(reloc(28, RelocKind::PcRel32, Target::Function(ikey(11))));
    let error = image_part(&small.built(), &small.owned(), &natives()).err().unwrap();
    assert!(error.what.contains("workspace"), "{error}");

    let mut small = Small::new();
    small.statics[0] = value(1, pointer_to(of_static(13)));
    assert!(image_part(&small.built(), &small.owned(), &natives()).is_err());

    let mut small = Small::new();
    small.statics[0].section = Some("linkme_SLOTS".to_owned());
    assert!(image_part(&small.built(), &small.owned(), &natives()).is_ok(), "a section only the image has");
    small.statics.iter_mut().find(|it| it.item == item(13)).unwrap().section = Some("linkme_SLOTS".to_owned());
    let error = image_part(&small.built(), &small.owned(), &natives()).err().unwrap();
    assert!(error.what.contains("linkme_SLOTS"), "{error}");

    let mut small = Small::new();
    small.owned_functions.push((ikey(99), CodeHash(id(199))));
    small.owned_functions.sort();
    assert!(image_part(&small.built(), &small.owned(), &natives()).is_err(), "an owned instance with no code");
}

#[test]
fn image_round_trips_and_refuses_damage() {
    let small = Small::new();
    let natives = natives();
    let part = image_part(&small.built(), &small.owned(), &natives).unwrap();
    let output = prelink::output(&dev_output(), "blaze-premap-test");
    let key = ImageKey(id(78));
    let bytes = prelink::build(&Model, &prelink_request(&output, &part), key).unwrap();
    let image = View::parse(&bytes, key).unwrap();
    assert_eq!(image.meta().base, BASE);
    assert_eq!(prelink::base(Format::Elf, Arch::X86_64).unwrap(), BASE);
    assert_eq!(image.meta().page, prelink::page(Format::Elf, Arch::X86_64));
    assert!(image.segments().all(|it| it.addr % PAGE == 0));
    assert_eq!(image.segments().map(|it| it.prot).collect::<Vec<_>>(), [Prot::Code, Prot::ReadOnly, Prot::Data]);
    for (key, _) in &small.owned_functions {
        assert!(image.get(&SymbolId::function(key)).is_some());
    }
    assert!(image.get(&SymbolId::function(&ikey(11))).is_none());
    let names: Vec<&str> = image.binds().map(|it| it.name).collect();
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(names.contains(&"malloc") && names.contains(&"__rg_alloc") && names.contains(&"absent"));
    assert_eq!(image.tls().map(|it| it.size), Some(16));
    assert_eq!(image.init().count(), 1);

    let reopened = View::reopen(&bytes, key);
    assert_eq!(reopened.table().len(), image.table().len());
    assert!(image.whole());
    let meta = View::reopen(&bytes[..image.meta_len()], key);
    assert!(!meta.whole(), "the metadata alone");
    assert_eq!(meta.table().len(), image.table().len());
    assert_eq!(meta.binds().count(), 3);
    assert!(meta.segments().all(|it| it.bytes.is_empty()));
    assert!(crate::map::Memory::of(&meta).is_err(), "no memory object of the metadata alone");
    for (function, _) in &small.owned_functions {
        let entry = image.entry(&SymbolId::function(function)).unwrap();
        let inlined = &small.functions.iter().find(|it| it.key == *function).unwrap().inlined;
        assert_eq!(image.content(entry.refs.content), Some(function_digest(CodeHash(function.0), inlined)), "codegen's digest");
    }
    let dep_statics: Vec<&Static> =
        small.statics.iter().filter(|it| small.owned_statics.contains(&(it.item, it.instance))).collect();
    assert_eq!(image.statics(), dep_statics.len());
    for (at, value) in dep_statics.iter().enumerate() {
        let entry = image.entry(&SymbolId::of_static(&value.item, &value.instance)).unwrap();
        assert_eq!(image.content(entry.refs.content), Some(static_definition(value)));
        assert_eq!(image.static_record(at), ((value.item, value.instance), static_definition(value)), "sorted");
    }
    assert!(View::parse(&bytes, ImageKey(id(79))).is_err(), "an image under another key");
    assert!(View::parse(&bytes[..bytes.len() - 1], key).is_err(), "a cut image");
    for at in [0, 8, 16, 40, 48, 64, 72] {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0x55;
        assert!(View::parse(&damaged, key).is_err(), "damage at {at} is refused");
    }
}

#[test]
fn an_image_is_the_outside_of_the_workspace_link_beside_it() {
    use blaze_link::Outside;
    use blaze_link::premap::StaticAt;
    let small = Small::new();
    let natives = natives();
    let part = image_part(&small.built(), &small.owned(), &natives).unwrap();
    let output = prelink::output(&dev_output(), "blaze-premap-test");
    let key = ImageKey(id(79));
    let bytes = prelink::build(&Model, &prelink_request(&output, &part), key).unwrap();
    let image = View::parse(&bytes, key).unwrap();
    for (key, _) in &small.owned_functions {
        assert_eq!(Outside::function(&image, key), image.get(&SymbolId::function(key)).map(|it| it.value));
    }
    assert_eq!(Outside::function(&image, &ikey(11)), None, "a workspace function is not the image's");
    for it in small.statics.iter().filter(|it| small.owned_statics.contains(&(it.item, it.instance))) {
        let value = image.get(&SymbolId::of_static(&it.item, &it.instance)).unwrap().value;
        let want = if it.thread_local { StaticAt::ThreadLocal(value) } else { StaticAt::Addr(value) };
        assert_eq!(Outside::static_at(&image, &it.item, &it.instance), Some(want));
    }
    assert_eq!(Outside::tls_end(&image), 16);
    assert_eq!(Outside::end(&image), image.end());
    assert_eq!(Outside::name(&image, "not defined"), None);
}

#[test]
fn prelink_refuses_what_it_was_not_asked() {
    struct Fixed(Prelinked);
    impl Prelink for Fixed {
        type Placed = Prelinked;

        fn prelink(&self, _: &Request<'_>) -> Result<Prelinked, PremapError> {
            Ok(self.0.clone())
        }
    }
    let small = Small::new();
    let natives = natives();
    let part = image_part(&small.built(), &small.owned(), &natives).unwrap();
    let output = prelink::output(&dev_output(), "blaze-premap-test");
    let request = prelink_request(&output, &part);
    let good = Model.prelink(&request).unwrap();

    let mut missing = good.clone();
    missing.functions.pop();
    assert!(prelink::build(&Fixed(missing), &request, ImageKey(id(1))).is_err(), "a function not placed");

    let mut unaligned = good.clone();
    unaligned.segments[1].addr += 8;
    assert!(prelink::build(&Fixed(unaligned), &request, ImageKey(id(1))).is_err(), "a segment off its page");

    let mut bind = good.clone();
    bind.binds[0].place = BASE;
    assert!(prelink::build(&Fixed(bind), &request, ImageKey(id(1))).is_err(), "a bind in code");

    let incremental = dev_output();
    let mut request = prelink_request(&output, &part);
    request.link.output = &incremental;
    assert!(prelink::build(&Model, &request, ImageKey(id(1))).is_err(), "an incremental image");

    let mut below = prelink_request(&output, &part);
    below.link.base = BASE - PAGE;
    assert!(prelink::build(&Model, &below, ImageKey(id(1))).is_err(), "a base below the executable's reservation");
    let mut past = prelink_request(&output, &part);
    past.link.base = prelink::END;
    assert!(prelink::build(&Model, &past, ImageKey(id(1))).is_err(), "a base past the smallest address space");
}

#[test]
fn a_prelink_that_writes_its_own_segments_makes_the_same_image() {
    struct Placing {
        linked: Prelinked,
        bytes: Vec<Vec<u8>>,
    }
    impl prelink::Placed for Placing {
        fn linked(&self) -> &Prelinked {
            &self.linked
        }
        fn len(&self, at: usize) -> u64 {
            self.bytes[at].len() as u64
        }
        fn write_into(&self, out: &mut [u8], offsets: &[usize]) -> Result<(), PremapError> {
            assert_eq!(offsets.len(), self.bytes.len());
            for (bytes, offset) in self.bytes.iter().zip(offsets) {
                out[*offset..offset + bytes.len()].copy_from_slice(bytes);
            }
            Ok(())
        }
    }
    struct Split;
    impl Prelink for Split {
        type Placed = Placing;

        fn prelink(&self, request: &Request<'_>) -> Result<Placing, PremapError> {
            let mut linked = Model.prelink(request)?;
            let bytes = linked.segments.iter_mut().map(|it| std::mem::take(&mut it.bytes)).collect();
            Ok(Placing { linked, bytes })
        }
    }
    let small = Small::new();
    let natives = natives();
    let part = image_part(&small.built(), &small.owned(), &natives).unwrap();
    let output = prelink::output(&dev_output(), "blaze-premap-test");
    let request = prelink_request(&output, &part);
    let whole = prelink::build(&Model, &request, ImageKey(id(1))).unwrap();
    let split = prelink::build(&Split, &request, ImageKey(id(1))).unwrap();
    assert_eq!(whole, split);
}

#[test]
fn a_prelink_refuses_a_segment_written_outside_the_image() {
    let segment = Segment { addr: BASE, size: 4096, prot: Prot::Code, bytes: vec![1; 64] };
    let linked = Prelinked { segments: vec![segment], ..Prelinked::default() };
    let mut out = vec![0u8; 4096];
    assert!(prelink::Placed::write_into(&linked, &mut out, &[4096 - 32]).is_err(), "past its end");
    assert!(prelink::Placed::write_into(&linked, &mut out, &[]).is_err(), "no offset");
    prelink::Placed::write_into(&linked, &mut out, &[0]).unwrap();
    assert_eq!(&out[..64], &[1; 64][..]);
}

/// An image of hand-made binds and names, for the start's rules, or the prelink's refusal.
fn try_image_of(binds: Vec<Bind>, names: Vec<PlacedName>, reached_directly: Vec<String>) -> Result<(Vec<u8>, ImageKey), PremapError> {
    struct Fixed(Prelinked);
    impl Prelink for Fixed {
        type Placed = Prelinked;

        fn prelink(&self, _: &Request<'_>) -> Result<Prelinked, PremapError> {
            Ok(self.0.clone())
        }
    }
    let segments = vec![
        Segment { addr: BASE, size: PAGE, prot: Prot::Code, bytes: vec![0xcc; 64] },
        Segment { addr: BASE + PAGE, size: PAGE, prot: Prot::Data, bytes: Vec::new() },
    ];
    let linked = Prelinked {
        segments,
        libraries: vec![Library { name: "libc.so.6".to_owned(), order: 3, as_needed: false }],
        binds,
        names,
        reached_directly,
        ..Prelinked::default()
    };
    let output = prelink::output(&dev_output(), "blaze-premap-test");
    let request = Request {
        link: blaze_link::premap::Request {
            output: &output,
            base: BASE,
            tls_bias: TLS_BIAS,
            functions: &[],
            statics: &[],
            names: &[],
            debug_names: &[],
            natives: &[],
            spans: &[],
        },
        function_digests: &[],
        static_digests: &[],
        debug_digests: &[],
    };
    let key = ImageKey(id(80));
    Ok((prelink::build(&Fixed(linked), &request, key)?, key))
}

fn image_of(binds: Vec<Bind>, names: Vec<PlacedName>) -> (Vec<u8>, ImageKey) {
    try_image_of(binds, names, Vec::new()).unwrap()
}

fn bind(n: u64, name: &str, kind: BindKind, weak: bool) -> Bind {
    Bind { place: BASE + PAGE + n * 8, name: name.to_owned(), addend: 0, kind, weak }
}

fn def(value: u64, kind: Kind, strength: Strength, order: u32) -> Def {
    Def { value, size: 8, kind, strength, order }
}

#[test]
fn start_binds_as_a_whole_link_resolves() {
    let image_names = vec![
        PlacedName { name: "hook".to_owned(), def: def(BASE + 16, Kind::Function, Strength::Weak, 5) },
        PlacedName { name: "both_weak".to_owned(), def: def(BASE + 32, Kind::Function, Strength::Weak, 5) },
    ];
    let binds = vec![
        bind(0, "hook", BindKind::Addr, false),
        bind(1, "both_weak", BindKind::Addr, false),
        bind(2, "only_workspace", BindKind::Addr, false),
        bind(3, "write", BindKind::Addr, false),
        bind(4, "maybe", BindKind::Addr, true),
    ];
    let (bytes, key) = image_of(binds, image_names);
    let image = View::parse(&bytes, key).unwrap();
    let mut names = vec![
        ("hook".to_owned(), def(0x9000, Kind::Function, Strength::Strong, u32::MAX)),
        ("both_weak".to_owned(), def(0x9100, Kind::Function, Strength::Weak, 2)),
        ("only_workspace".to_owned(), def(0x9200, Kind::Data, Strength::Strong, u32::MAX)),
    ];
    names.sort_by(|a, b| a.0.cmp(&b.0));
    let start = start::plan(&image, &names).unwrap();
    let writes: HashMap<u64, u64> = start.writes.iter().map(|it| (it.place, it.value)).collect();
    assert_eq!(writes[&(BASE + PAGE)], 0x9000, "a strong definition beats a weak one");
    assert_eq!(writes[&(BASE + PAGE + 8)], 0x9100, "of two weak ones the first on the line");
    assert_eq!(writes[&(BASE + PAGE + 16)], 0x9200);
    assert!(start.writes.iter().all(|it| it.address), "an address moves with the run's slide");
    assert_eq!(start.lookups.iter().map(|it| (it.name, it.weak)).collect::<Vec<_>>(), [("write", false), ("maybe", true)]);

    let strong = vec![("hook".to_owned(), def(0x9000, Kind::Function, Strength::Strong, u32::MAX))];
    let (bytes, key) = image_of(
        vec![bind(0, "hook", BindKind::Addr, false)],
        vec![PlacedName { name: "hook".to_owned(), def: def(BASE + 16, Kind::Function, Strength::Strong, 5) }],
    );
    let image = View::parse(&bytes, key).unwrap();
    assert!(start::plan(&image, &strong).is_err(), "two strong definitions");

    let (bytes, key) = image_of(vec![bind(0, "errno_tls", BindKind::TpOff, false)], Vec::new());
    let image = View::parse(&bytes, key).unwrap();
    assert!(start::plan(&image, &[]).is_err(), "a fixed offset into a library's thread-locals");
    let tls = vec![("errno_tls".to_owned(), def(16, Kind::ThreadLocal, Strength::Strong, u32::MAX))];
    let start = start::plan(&image, &tls).unwrap();
    assert_eq!(start.writes, [Write { place: BASE + PAGE, value: 16, address: false }], "an offset from the thread pointer never slides");
    let data = vec![("errno_tls".to_owned(), def(0x9000, Kind::Data, Strength::Strong, u32::MAX))];
    assert!(start::plan(&image, &data).is_err(), "a thread-local bound to an address");
}

#[test]
fn start_refuses_workspace_names_out_of_order() {
    let (bytes, key) = image_of(vec![bind(0, "a", BindKind::Addr, false)], Vec::new());
    let image = View::parse(&bytes, key).unwrap();
    let names = vec![
        ("b".to_owned(), def(0x9000, Kind::Function, Strength::Strong, u32::MAX)),
        ("a".to_owned(), def(0x9100, Kind::Function, Strength::Strong, u32::MAX)),
    ];
    assert!(start::plan(&image, &names).is_err(), "out of order");
    let twice = vec![names[1].clone(), names[1].clone()];
    assert!(start::plan(&image, &twice).is_err(), "a name twice");
}

#[test]
fn an_image_marks_the_weak_names_its_code_reaches_directly() {
    use blaze_link::Outside;
    let names = vec![
        PlacedName { name: "weak_hook".to_owned(), def: def(BASE + 16, Kind::Function, Strength::Weak, 5) },
        PlacedName { name: "other_weak".to_owned(), def: def(BASE + 32, Kind::Function, Strength::Weak, 5) },
        PlacedName { name: "strong".to_owned(), def: def(BASE + 48, Kind::Function, Strength::Strong, 5) },
    ];
    let (bytes, key) = try_image_of(Vec::new(), names.clone(), vec!["weak_hook".to_owned()]).unwrap();
    let image = View::parse(&bytes, key).unwrap();
    assert!(Outside::reached_directly(&image, "weak_hook"));
    assert!(!Outside::reached_directly(&image, "other_weak"));
    assert!(!Outside::reached_directly(&image, "strong"));
    assert!(!Outside::reached_directly(&image, "absent"));
    assert!(try_image_of(Vec::new(), names.clone(), vec!["strong".to_owned()]).is_err(), "a strong name");
    assert!(try_image_of(Vec::new(), names, vec!["absent".to_owned()]).is_err(), "a name with no slot");

    let table_at = |bytes: &[u8]| {
        let at = |n: usize| u64::from_le_bytes(bytes[n..n + 8].try_into().unwrap()) as usize;
        (at(80 + 16), at(80 + 16 + 8))
    };
    let slot_of = |bytes: &[u8], name: &str| {
        let (offset, len) = table_at(bytes);
        let id = SymbolId::name(name).0.0;
        offset + bytes[offset..offset + len].chunks(table::SLOT).position(|it| it[..32] == id).unwrap() * table::SLOT
    };
    let mut damaged = bytes.clone();
    damaged[slot_of(&bytes[..], "weak_hook") + 54] = 2;
    assert!(View::parse(&damaged, key).is_err(), "a flag of no value there is");
    let mut damaged = bytes.clone();
    damaged[slot_of(&bytes[..], "strong") + 54] = 1;
    assert!(View::parse(&damaged, key).is_err(), "a strong name reached directly");
}

#[test]
fn a_bind_carries_the_slot_of_the_images_own_definition() {
    let names = vec![PlacedName { name: "hook".to_owned(), def: def(BASE + 16, Kind::Function, Strength::Weak, 5) }];
    let binds = vec![bind(0, "hook", BindKind::Addr, true), bind(8, "absent", BindKind::Addr, true)];
    let (bytes, key) = image_of(binds, names);
    let image = View::parse(&bytes, key).unwrap();
    let own: Vec<_> = image.binds().map(|it| (it.name, it.own)).collect();
    assert_eq!(own, [("hook", image.get(&SymbolId::name("hook"))), ("absent", None)]);
    assert!(own[0].1.is_some());

    let binds_at = u64::from_le_bytes(bytes[80 + 2 * 16..80 + 2 * 16 + 8].try_into().unwrap()) as usize;
    for (record, slot) in [(0, 0u32), (1, 1), (0, u32::MAX)] {
        let mut damaged = bytes.clone();
        let at = binds_at + record * 32 + 28;
        damaged[at..at + 4].copy_from_slice(&slot.to_le_bytes());
        assert!(View::parse(&damaged, key).is_err(), "bind {record} naming slot {slot}");
    }
}

fn drive(sim: &mut Sim, store: &mut Store, effects: &mut Vec<Effect>) {
    run_until_idle(sim, effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
    });
}

fn open_store(sim: &mut Sim) -> Store {
    let mut effects = Vec::new();
    let config = Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 40 };
    let mut store = Store::open(config, &mut effects);
    drive(sim, &mut store, &mut effects);
    assert!(store.ready(), "{:?}", store.broken());
    store
}

#[test]
fn store_round_trip() {
    let small = Small::new();
    let natives = natives();
    let part = image_part(&small.built(), &small.owned(), &natives).unwrap();
    let output = prelink::output(&dev_output(), "blaze-premap-test");
    let key = ImageKey(id(81));
    let bytes = prelink::build(&Model, &prelink_request(&output, &part), key).unwrap();

    let mut sim = Sim::new(1, Faults::default());
    let mut store = open_store(&mut sim);
    assert_eq!(store.get_image(&key).unwrap(), None);
    store.put_image(&key, bytes.clone()).unwrap();
    assert_eq!(store.get_image(&key).unwrap().as_deref(), Some(&bytes[..]), "read back before it is durable");
    let mut effects = Vec::new();
    store.flush(&mut effects);
    drive(&mut sim, &mut store, &mut effects);
    drop(store);

    let store = open_store(&mut sim);
    let kept = store.get_image(&key).unwrap().expect("kept across a reopen");
    assert_eq!(&kept[..], &bytes[..]);
    View::parse(&kept, key).unwrap();
    let mut store = store;
    assert!(store.put_image(&key, vec![1, 2, 3]).is_err(), "other bytes under one key");
}

#[derive(Default)]
struct Shelved(HashMap<ImageKey, Vec<u8>>, u32);

impl Shelf for Shelved {
    fn get_image(&self, key: &ImageKey) -> Result<Option<&[u8]>, PremapError> {
        Ok(self.0.get(key).map(Vec::as_slice))
    }

    fn put_image(&mut self, key: &ImageKey, bytes: Vec<u8>) -> Result<(), PremapError> {
        self.1 += 1;
        self.0.insert(*key, bytes);
        Ok(())
    }
}

fn with_dev<R>(small: &Small, natives: &[SidedNative<'_>], f: impl FnOnce(&Dev<'_>) -> R) -> R {
    let graph = Graph::new();
    let units: Vec<DepUnit<'_>> = graph
        .units
        .iter()
        .map(|(package, target, features, cfg, rustflags)| DepUnit { package, target, features, cfg, rustflags })
        .collect();
    let dev = Dev {
        graph: GraphInputs {
            units: &units,
            toolchain: &graph.toolchain,
            settings: &graph.settings,
            linker: graph.linker,
            output: &graph.output,
            base: graph.base,
            tls_bias: graph.tls_bias,
            natives: &graph.natives,
            spans: &graph.spans,
        },
        built: small.built(),
        owned: small.owned(),
        natives,
        spans: &[],
    };
    f(&dev)
}

#[test]
fn an_image_is_made_once_per_key_and_again_only_when_a_build_needs_more() {
    let mut images = Images::default();
    let mut shelf = Shelved::default();
    let natives = natives();
    let small = Small::new();
    let first = with_dev(&small, &natives, |dev| {
        let Plan::Make(job) = dev::plan(&mut images, dev).unwrap() else { panic!("a graph with no image makes one") };
        assert!(matches!(dev::plan(&mut images, dev).unwrap(), Plan::Wait), "a second build waits for it");
        let made = job.make(&Model, &mut shelf);
        let ready = images.finish(job.key, made).unwrap();
        let Plan::Link { image, workspace, next } = dev::plan(&mut images, dev).unwrap() else { panic!("links once made") };
        assert!(workspace.complete() && next.is_none());
        assert_eq!(image.key, ready.key);
        ready.key
    });
    assert_eq!(shelf.1, 1);

    let mut more = Small::new();
    more.functions.push(function(4, Vec::new()));
    more.functions[1].relocs.push(reloc(28, RelocKind::PcRel32, Target::Function(ikey(4))));
    more.owned_functions.push((ikey(4), CodeHash(id(104))));
    more.owned_functions.sort();
    with_dev(&more, &natives, |dev| {
        let Plan::Link { image, workspace, next } = dev::plan(&mut images, dev).unwrap() else { panic!("links the graph's image") };
        assert_eq!(image.key, first, "the graph's newest image, while the next is made");
        assert_eq!(workspace.spilled, [ikey(4)], "the instance the image lacks is linked with the workspace");
        let job = next.expect("the image of the build's key is made next");
        assert_ne!(job.key, first);
        let Plan::Link { next, .. } = dev::plan(&mut images, dev).unwrap() else { panic!() };
        assert!(next.is_none(), "made once");
        let made = job.make(&Model, &mut shelf);
        images.finish(job.key, made).unwrap();
        assert!(images.get(&first).is_none(), "the image it replaces is closed");
        let Plan::Link { image, workspace, next } = dev::plan(&mut images, dev).unwrap() else { panic!() };
        assert_eq!(image.key, job.key);
        assert!(workspace.complete() && next.is_none());
    });
    assert_eq!(shelf.1, 2);

    let mut fresh = Images::default();
    with_dev(&small, &natives, |dev| {
        let Plan::Make(job) = dev::plan(&mut fresh, dev).unwrap() else { panic!() };
        let made = job.make(&Model, &mut shelf);
        fresh.finish(job.key, made).unwrap();
    });
    assert_eq!(shelf.1, 2, "an image the store keeps is opened, not linked again");
}

#[test]
fn a_failed_image_fails_again_and_never_falls_back() {
    let mut images = Images::default();
    let graph = GraphKey(id(1));
    let key = ImageKey(id(2));
    assert!(images.newest(graph).is_none());
    assert!(matches!(images.want(key), Pick::Make));
    let error = PremapError::new("the prelink of the dependency image placed no function".to_owned());
    assert!(images.finish(key, Err::<Ready, _>(error.clone())).is_err());
    assert!(matches!(images.want(key), Pick::Failed(it) if it == error));
    assert!(images.newest(graph).is_none(), "a failed image is no graph's newest");
    assert!(!images.claim(key));
}
