//! The keys of a dependency image.
//!
//! - [`GraphKey`]: everything below the workspace members that is not the set of instances:
//!   the dependency units (package, target, features, cfg, rustflags), the toolchain, the
//!   codegen settings, the linker and the output it links for, the base address, the run
//!   loader's thread-local bias, the C objects and libraries of the link line and the span
//!   tables positions are filled from.
//!   Images of one graph differ only in which instances they hold.
//! - [`ImageKey`]: the graph key and the instances: every function by its instance key,
//!   code hash and what it inlined, every static by its item, instance and definition, every name the image
//!   defines. It is the store's key of the image and the name of its one memory object on
//!   the machine.
//!
//! Every input is hashed by destructuring its type, so a field added to one is a compile
//! error here until it is hashed. Lists are hashed sorted and length-prefixed, so the order
//! the caller found them in never changes a key.

use blaze_codegen::content::{arch_tag, target_into};
use blaze_codegen::settings::{Coverage, Sanitize, Toolchain};
use blaze_codegen::{CompiledFunction, InstanceKey, Settings, Target};
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemKey};
use blaze_link::{Debug, DevRun, Format, NativeKind, Output, OutputKind, Platform};
use blaze_mem::{Tag, frame, frame::Frame};

pub const GRAPH_DOMAIN: &str = "blaze v2 premap graph 2";
pub const IMAGE_DOMAIN: &str = "blaze v2 premap image 3";
pub const INLINED_DOMAIN: &str = "blaze v2 premap inlined 1";

/// The dependency graph an image is of: what every image of it shares.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct GraphKey(pub ContentId);

/// One image: its graph and the instances in it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ImageKey(pub ContentId);

/// A unit below the workspace members, as its bytes depend on it.
#[derive(Clone, Copy, Debug)]
pub struct DepUnit<'a> {
    /// cargo's package id: name, version and source.
    pub package: &'a str,
    pub target: &'a str,
    /// The features on, any order.
    pub features: &'a [String],
    /// The `--cfg`s it is compiled with (its build script's `rustc-cfg`s and the profile's),
    /// any order.
    pub cfg: &'a [String],
    /// The rustflags its profile gives it, in order (their order is rustc's meaning).
    pub rustflags: &'a [String],
}

/// A native input of the link line as the image's bytes depend on it: its contents, how it
/// is linked and where on the line it is (resolution takes the first definition in order).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NativeInput {
    pub id: ContentId,
    pub kind: NativeKind,
    pub order: u32,
}

/// Everything but the instances.
#[derive(Clone, Copy, Debug)]
pub struct GraphInputs<'a> {
    pub units: &'a [DepUnit<'a>],
    pub toolchain: &'a Toolchain,
    /// Every codegen settings ID the image's functions were compiled with, any order.
    pub settings: &'a [Settings],
    /// blaze-link's code identity: another linker places other bytes.
    pub linker: ContentId,
    pub output: &'a Output,
    pub base: u64,
    /// Where the image's thread-locals are in the workspace executable's own thread-local
    /// block, which the system loader sets up: the image's code reaches them at this bias
    /// from the thread pointer, and the executable's link puts the image's part there.
    pub tls_bias: i64,
    pub natives: &'a [NativeInput],
    /// The span side tables positions are filled from: each anchor and its table's content ID.
    pub spans: &'a [(ContentId, ContentId)],
}

/// A function of the image: its key, its generic's code hash and what it built in of other
/// functions ([`inlined_digest`]), which the code hash does not say.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct KeyedFunction {
    pub key: InstanceKey,
    pub code: CodeHash,
    pub inlined: Option<ContentId>,
}

/// A static of the image: which one, and the content ID of its definition
/// ([`static_definition`]): a static's value is not in its key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct KeyedStatic {
    pub item: ItemKey,
    pub instance: ContentId,
    pub definition: ContentId,
}

/// The instances of an image.
#[derive(Clone, Copy, Debug)]
pub struct ImageInputs<'a> {
    pub functions: &'a [KeyedFunction],
    pub statics: &'a [KeyedStatic],
    /// The names its Rust code defines (`#[no_mangle]`, `#[export_name]`) and what each is.
    pub names: &'a [&'a (String, Target)],
    /// The names for debuggers and backtraces, when the output carries debug info.
    pub debug_names: &'a [&'a (InstanceKey, String)],
}

impl GraphKey {
    /// On every dev build: its sorting is on a frame, so it allocates nothing of the heap.
    pub fn of(inputs: &GraphInputs<'_>) -> GraphKey {
        frame::task(Tag::Link, |frame| GraphKey::of_in(inputs, frame))
    }

    fn of_in(inputs: &GraphInputs<'_>, frame: &Frame) -> GraphKey {
        let GraphInputs { units, toolchain, settings, linker, output, base, tls_bias, natives, spans } = inputs;
        let mut hasher = Hasher::new(GRAPH_DOMAIN);

        let mut order: Vec<&DepUnit<'_>, _> = Vec::with_capacity_in(units.len(), frame);
        order.extend(units.iter());
        order.sort_unstable_by(|a, b| (a.package, a.target).cmp(&(b.package, b.target)));
        count(&mut hasher, units.len());
        for DepUnit { package, target, features, cfg, rustflags } in order {
            hasher.part(package.as_bytes()).part(target.as_bytes());
            set(&mut hasher, features, frame);
            set(&mut hasher, cfg, frame);
            count(&mut hasher, rustflags.len());
            for flag in rustflags.iter() {
                hasher.part(flag.as_bytes());
            }
        }

        toolchain_into(&mut hasher, toolchain, frame);

        let mut ids = Vec::with_capacity_in(settings.len(), frame);
        ids.extend(settings.iter().map(|it| it.0.0));
        ids.sort_unstable();
        ids.dedup();
        count(&mut hasher, ids.len());
        hasher.update(ids.as_flattened());

        hasher.id(linker);
        output_into(&mut hasher, output);
        hasher.update(&base.to_le_bytes()).update(&tls_bias.to_le_bytes());

        let mut sorted: Vec<&NativeInput, _> = Vec::with_capacity_in(natives.len(), frame);
        sorted.extend(natives.iter());
        sorted.sort_unstable_by_key(|it| it.order);
        count(&mut hasher, sorted.len());
        for NativeInput { id, kind, order } in sorted {
            hasher.id(id).update(&order.to_le_bytes());
            match kind {
                NativeKind::Object => hasher.update(&[0]),
                NativeKind::Archive { whole } => hasher.update(&[1, *whole as u8]),
                NativeKind::Shared { as_needed } => hasher.update(&[2, *as_needed as u8]),
            };
        }

        let mut sorted: Vec<&(ContentId, ContentId), _> = Vec::with_capacity_in(spans.len(), frame);
        sorted.extend(spans.iter());
        sorted.sort_unstable();
        count(&mut hasher, sorted.len());
        for (anchor, table) in sorted {
            hasher.id(anchor).id(table);
        }
        GraphKey(hasher.finish())
    }
}

impl ImageKey {
    /// Only for a build its graph's newest image does not serve; its sorting is on a frame.
    pub fn of(graph: GraphKey, inputs: &ImageInputs<'_>) -> ImageKey {
        frame::task(Tag::Link, |frame| ImageKey::of_in(graph, inputs, frame))
    }

    fn of_in(graph: GraphKey, inputs: &ImageInputs<'_>, frame: &Frame) -> ImageKey {
        let ImageInputs { functions, statics, names, debug_names } = inputs;
        let mut hasher = Hasher::new(IMAGE_DOMAIN);
        hasher.id(&graph.0);

        // One flat buffer of records, sorted, hashed in one call: BLAKE3 hashes a long input
        // on all its SIMD lanes at once.
        let mut flat = Vec::with_capacity_in(functions.len(), frame);
        flat.extend(functions.iter().map(|KeyedFunction { key, code, inlined }| {
            let mut it = [0u8; 96];
            it[..32].copy_from_slice(&key.0.0);
            it[32..64].copy_from_slice(&code.0.0);
            if let Some(inlined) = inlined {
                it[64..].copy_from_slice(&inlined.0);
            }
            it
        }));
        flat.sort_unstable();
        flat.dedup();
        count(&mut hasher, flat.len());
        hasher.update(flat.as_flattened());

        let mut flat = Vec::with_capacity_in(statics.len(), frame);
        flat.extend(statics.iter().map(|KeyedStatic { item, instance, definition }| {
            let mut it = [0u8; 96];
            it[..32].copy_from_slice(&item.0.0);
            it[32..64].copy_from_slice(&instance.0);
            it[64..].copy_from_slice(&definition.0);
            it
        }));
        flat.sort_unstable();
        flat.dedup();
        count(&mut hasher, flat.len());
        hasher.update(flat.as_flattened());

        let mut sorted: Vec<&&(String, Target), _> = Vec::with_capacity_in(names.len(), frame);
        sorted.extend(names.iter());
        sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        count(&mut hasher, sorted.len());
        for (name, target) in sorted {
            hasher.part(name.as_bytes());
            target_into(&mut hasher, target);
        }

        let mut sorted: Vec<&&(InstanceKey, String), _> = Vec::with_capacity_in(debug_names.len(), frame);
        sorted.extend(debug_names.iter());
        sorted.sort_unstable();
        count(&mut hasher, sorted.len());
        for (key, name) in sorted {
            hasher.id(&key.0).part(name.as_bytes());
        }
        ImageKey(hasher.finish())
    }
}

/// The content ID of a static's definition: codegen's, the one hash of it, so an image's
/// key and its check can't drift.
pub use blaze_codegen::driver::static_definition;

/// What the key holds of a function: its key, its code hash (given by the caller from the
/// instance it compiled) and what it inlined.
pub fn keyed(function: &CompiledFunction, code: CodeHash) -> KeyedFunction {
    KeyedFunction { key: function.key, code, inlined: inlined_digest(&function.inlined) }
}

/// The code hashes of the callees whose bodies a function inlined, in its order: `None` when
/// it inlined none, as a dev build's functions mostly do.
pub fn inlined_digest(inlined: &[CodeHash]) -> Option<ContentId> {
    if inlined.is_empty() {
        return None;
    }
    let mut hasher = Hasher::new(INLINED_DOMAIN);
    count(&mut hasher, inlined.len());
    for code in inlined {
        hasher.id(&code.0);
    }
    Some(hasher.finish())
}

fn count(hasher: &mut Hasher, n: usize) {
    hasher.update(&(n as u64).to_le_bytes());
}

/// A set of strings, sorted and deduplicated.
fn set(hasher: &mut Hasher, items: &[String], frame: &Frame) {
    let mut sorted: Vec<&str, _> = Vec::with_capacity_in(items.len(), frame);
    sorted.extend(items.iter().map(String::as_str));
    sorted.sort_unstable();
    sorted.dedup();
    count(hasher, sorted.len());
    for item in sorted {
        hasher.part(item.as_bytes());
    }
}

fn toolchain_into(hasher: &mut Hasher, toolchain: &Toolchain, frame: &Frame) {
    let Toolchain { id, target, arch, cpu, features, cranelift, llvm, stencils, frame_pointers, relocation } = toolchain;
    hasher.id(id).part(target.as_bytes()).update(&[arch_tag(*arch)]).part(cpu.as_bytes());
    let mut sorted: Vec<&str, _> = Vec::with_capacity_in(features.len(), frame);
    sorted.extend(features.iter().map(String::as_str));
    sorted.sort_unstable();
    count(hasher, sorted.len());
    for feature in sorted {
        hasher.part(feature.as_bytes());
    }
    hasher.part(cranelift.as_bytes());
    match llvm {
        Some(llvm) => hasher.update(&[1]).part(llvm.as_bytes()),
        None => hasher.update(&[0]),
    };
    match stencils {
        Some(stencils) => hasher.update(&[1]).part(stencils.as_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.update(&[*frame_pointers as u8, *relocation as u8]);
}

fn output_into(hasher: &mut Hasher, output: &Output) {
    let Output {
        format,
        arch,
        kind,
        interpreter,
        platform,
        incremental,
        debug,
        live,
        tags,
        stack,
        windows_subsystem,
        dev_run,
        triple,
        exec_log,
        sanitize,
        rpaths,
    } = output;
    hasher.update(&[format_tag(*format), arch_tag(*arch)]);
    match kind {
        OutputKind::Executable { entry } => {
            hasher.update(&[0]).part(entry.as_bytes());
        }
        OutputKind::Shared { name, exports } => {
            hasher.update(&[1]).part(name.as_bytes());
            count(hasher, exports.len());
            for export in exports {
                hasher.part(export.as_bytes());
            }
        }
    }
    match interpreter {
        Some(it) => hasher.update(&[1]).part(it.as_bytes()),
        None => hasher.update(&[0]),
    };
    match platform {
        Some(Platform { platform, minimum, sdk }) => hasher
            .update(&[1])
            .update(&platform.to_le_bytes())
            .update(&minimum.to_le_bytes())
            .update(&sdk.to_le_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.update(&[*incremental as u8, matches!(debug, Debug::Lines) as u8]);
    // Where running programs take live patches: an image is never patched, so a live output
    // is refused before it is keyed ([`crate::prelink`]); hashed all the same.
    match live {
        Some(_) => hasher.update(&[1]),
        None => hasher.update(&[0]),
    };
    hasher.update(&tags.to_le_bytes());
    match stack {
        Some(stack) => hasher.update(&[1]).update(&stack.to_le_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.update(&[*windows_subsystem as u8]);
    match dev_run {
        Some(DevRun { arena, cells, fixed }) => {
            hasher.update(&[1]).update(&arena.to_le_bytes()).update(&cells.to_le_bytes()).update(&fixed.to_le_bytes())
        }
        None => hasher.update(&[0]),
    };
    hasher.part(triple.as_bytes()).update(&[*exec_log as u8]);
    let Sanitize { sanitizers, recover, track_origins, coverage: Coverage { level, flags } } = sanitize;
    hasher
        .update(&[sanitizers.0, recover.0, *track_origins, *level])
        .update(&flags.to_le_bytes());
    count(hasher, rpaths.len());
    for rpath in rpaths {
        hasher.part(rpath.as_bytes());
    }
}

pub(crate) fn format_tag(format: Format) -> u8 {
    match format {
        Format::Elf => 0,
        Format::MachO => 1,
        Format::Pe => 2,
        Format::Wasm => 3,
    }
}