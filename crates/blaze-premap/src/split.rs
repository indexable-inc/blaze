//! Which of a dev build's pieces are the image's and which the workspace link's.
//!
//! An instance is the dependencies' when its item is a dependency's (or std's) and none of
//! its type arguments names a workspace member's type, closure or item: codegen says which
//! ([`Owned`], from `Built::owners`). Instances of dependency generics made for workspace
//! types are the workspace's. A static is not generic: it is its crate's.
//!
//! - [`image_part`] takes the dependencies' pieces for a new image, and checks the image is
//!   closed: a dependency instance that names a workspace instance or static is an error
//!   naming both (it can't be: codegen's ownership says otherwise). Names outside blaze's
//!   code (`Target::Extern`, `Target::Weak`) stay open: natives define them, or the workspace
//!   does (the global allocator's `__rg_alloc`), and the run binds them.
//! - [`workspace_part`] takes, against an image, everything the image does not hold: the
//!   workspace's pieces, and the dependencies' instances the image was built without
//!   (`spilled`: a workspace edit that calls a dependency function the image's build did not
//!   reach). Each piece is in exactly one of the two, so every static has one address and
//!   one value, as in a whole link. A piece the image holds must be the build's: a function's
//!   inlined callees and debug name, a static's definition, a Rust name's target, the image's
//!   Rust names, debug names and init pieces no more than the build's. Else the image is
//!   [`Fit::Stale`] for the build, which then keys its own.
//! - Natives: the dependencies' objects, archives and libraries go in the image. The
//!   workspace link gets its own natives, every archive not taken whole, and every shared
//!   library: a workspace reference resolves against the image's table first, then pulls an
//!   archive member the image did not pull, so no member is in both.

use blaze_codegen::driver::Digests;
use blaze_codegen::{CompiledFunction, Init, InstanceKey, Static, Target, Unwind};
use blaze_graph::par;
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use blaze_link::{Native, NativeKind};
use blaze_mem::{Tag, frame, frame::Frame};

use crate::PremapError;
use crate::image::View;
use crate::key::{KeyedFunction, KeyedStatic, inlined_digest};
use crate::table::SymbolId;

/// The sections whose pieces the run's start orders across both sides; any other named
/// section is one range per link.
const ORDERED_SECTIONS: [&str; 4] = [".init_array", ".fini_array", "__DATA,__mod_init_func", ".CRT$XCU"];

/// Pieces checked on one thread at a time: fewer are checked on the caller's.
const CHUNK: usize = 4096;

/// What codegen says is the dependencies', each list sorted.
#[derive(Clone, Copy, Debug)]
pub struct Owned<'a> {
    /// The dependencies' function instances, with their generics' code hashes, by key.
    pub functions: &'a [(InstanceKey, CodeHash)],
    pub statics: &'a [(ItemKey, ContentId)],
}

/// A dev build's output, as the linker takes it.
#[derive(Clone, Copy, Debug)]
pub struct Built<'a> {
    pub functions: &'a [CompiledFunction],
    /// Sorted by item and instance, each once.
    pub statics: &'a [Static],
    /// The names Rust code defines, and what each is.
    pub names: &'a [(String, Target)],
    pub debug_names: &'a [(InstanceKey, String)],
    /// Codegen's digest of each function, static and debug name, each list in the order of
    /// the one it is of.
    pub digests: &'a Digests,
}

/// A native of the link line and whose it is: a dependency unit's (its build script's
/// `rustc-link-lib`, its crate's `#[link]`) or a workspace member's. The C runtime's start
/// objects are the run process loader's, in neither.
#[derive(Clone, Copy, Debug)]
pub struct SidedNative<'a> {
    pub native: Native<'a>,
    pub dependency: bool,
}

/// The pieces of a new image.
pub struct ImagePart<'a> {
    pub functions: Vec<&'a CompiledFunction>,
    pub statics: Vec<&'a Static>,
    pub names: Vec<&'a (String, Target)>,
    pub debug_names: Vec<&'a (InstanceKey, String)>,
    pub natives: Vec<Native<'a>>,
    /// What the key holds of them.
    pub keyed_functions: Vec<KeyedFunction>,
    pub keyed_statics: Vec<KeyedStatic>,
    /// Codegen's digests of `functions`, `statics` and `debug_names`, in their order: what
    /// the image keeps to check a build's pieces against.
    pub function_digests: Vec<ContentId>,
    pub static_digests: Vec<ContentId>,
    pub debug_digests: Vec<ContentId>,
}

/// What an image is for a build.
pub enum Fit<'a> {
    /// It serves the build: the rest is linked with the workspace.
    Part(WorkspacePart<'a>),
    /// A piece it holds is not the build's.
    Stale(Stale),
}

/// The first piece of an image found not to be a build's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stale {
    /// Built with other callees inlined, or named otherwise for debuggers.
    Function(InstanceKey),
    /// Defined otherwise.
    Static(ItemKey, ContentId),
    /// The build's Rust name at this index of `Built::names` is not the image's, or not of
    /// the image's piece.
    Name(usize),
    /// The image defines Rust names, debug names or init pieces the build has not.
    Extra,
}

/// The pieces the workspace link places, given an image.
pub struct WorkspacePart<'a> {
    pub functions: Vec<&'a CompiledFunction>,
    pub statics: Vec<&'a Static>,
    pub names: Vec<&'a (String, Target)>,
    pub debug_names: Vec<&'a (InstanceKey, String)>,
    pub natives: Vec<Native<'a>>,
    /// The dependencies' functions and statics the image does not hold: linked here this
    /// time, and the reason to build the next image of the graph.
    pub spilled: Vec<InstanceKey>,
    pub spilled_statics: Vec<(ItemKey, ContentId)>,
}

impl WorkspacePart<'_> {
    /// Whether the image held every dependency piece this build has.
    pub fn complete(&self) -> bool {
        self.spilled.is_empty() && self.spilled_statics.is_empty()
    }
}

/// The dependencies' pieces of `built`, closed under what they refer to.
pub fn image_part<'a>(
    built: &Built<'a>,
    owned: &Owned<'_>,
    natives: &[SidedNative<'a>],
) -> Result<ImagePart<'a>, PremapError> {
    sorted(owned)?;
    frame::task(Tag::Link, |frame| image_part_in(built, owned, natives, frame))
}

fn image_part_in<'a>(
    built: &Built<'a>,
    owned: &Owned<'_>,
    natives: &[SidedNative<'a>],
    frame: &Frame,
) -> Result<ImagePart<'a>, PremapError> {
    let function_slots = Keys::slots(owned.functions.iter().map(|it| it.0.0.0), frame);
    let deps = Keys::<32>::new(&function_slots);
    let static_slots = Keys::slots(owned.statics.iter().map(|(item, instance)| pair(item, instance)), frame);
    let dep_statics = Keys::<64>::new(&static_slots);
    let is_dep = |key: &InstanceKey| deps.get(&key.0.0).is_some();
    let is_dep_static = |item: &ItemKey, instance: &ContentId| dep_statics.get(&pair(item, instance)).is_some();

    lined_up(built)?;
    let mut functions = Vec::with_capacity(owned.functions.len());
    let mut keyed_functions = Vec::with_capacity(owned.functions.len());
    let mut function_digests = Vec::with_capacity(owned.functions.len());
    for (function, (key, digest)) in built.functions.iter().zip(&built.digests.functions) {
        if function.key != *key {
            return Err(misaligned("functions"));
        }
        let Some(at) = deps.get(&function.key.0.0) else { continue };
        functions.push(function);
        keyed_functions.push(KeyedFunction {
            key: function.key,
            code: owned.functions[at as usize].1,
            inlined: inlined_digest(&function.inlined),
        });
        function_digests.push(*digest);
    }
    let refers = |function: &CompiledFunction| -> Option<PremapError> {
        let refers = |target: &Target| match target {
            Target::Function(key) | Target::Direct(key) if !is_dep(key) => Some(PremapError::of(format_args!(
                "dependency instance {:?} refers to {key:?}, which is the workspace's: an image can't \
                 call into the workspace but by name",
                function.key
            ))),
            Target::Static { item, instance } if !is_dep_static(item, instance) => Some(PremapError::of(format_args!(
                "dependency instance {:?} refers to the static {item:?} ({instance:?}), which is the workspace's",
                function.key
            ))),
            _ => None,
        };
        let personality = match &function.unwind {
            Some(Unwind::Dwarf { personality: Some(target), .. }) | Some(Unwind::Windows { handler: Some(target), .. }) => {
                Some(target)
            }
            _ => None,
        };
        let data = function.data.iter().flat_map(|it| it.relocs.targets());
        function.relocs.targets().iter().chain(data).chain(personality).find_map(refers)
    };
    let error = match functions.len() <= CHUNK {
        true => functions.iter().find_map(|it| refers(it)),
        false => par::find_map_first(&functions, |it| refers(it)),
    };
    if let Some(error) = error {
        return Err(error);
    }
    if functions.len() != owned.functions.len() {
        let missing = owned
            .functions
            .iter()
            .find(|(key, _)| !built.functions.iter().any(|it| it.key == *key))
            .map(|it| it.0);
        return Err(PremapError::of(format_args!(
            "codegen says {missing:?} is a dependency instance of this build, and the build has no code for it"
        )));
    }

    let mut statics = Vec::with_capacity(owned.statics.len());
    let mut keyed_statics = Vec::with_capacity(owned.statics.len());
    let mut static_digests = Vec::with_capacity(owned.statics.len());
    for (value, (id, definition)) in built.statics.iter().zip(&built.digests.statics) {
        if (value.item, value.instance) != *id {
            return Err(misaligned("statics"));
        }
        if !is_dep_static(&value.item, &value.instance) {
            continue;
        }
        let relocs = match &value.init {
            Init::Bytes { relocs, .. } => relocs.targets(),
            Init::Zeroed { .. } => &[],
        };
        for target in relocs.iter().chain(value.data.iter().flat_map(|it| it.relocs.targets())) {
            match target {
                Target::Function(key) | Target::Direct(key) if !is_dep(key) => {
                    return Err(PremapError::of(format_args!(
                        "the dependency static {:?} points at {key:?}, which is the workspace's",
                        value.item
                    )));
                }
                Target::Static { item, instance } if !is_dep_static(item, instance) => {
                    return Err(PremapError::of(format_args!(
                        "the dependency static {:?} points at the static {item:?} ({instance:?}), which is the workspace's",
                        value.item
                    )));
                }
                _ => {}
            }
        }
        statics.push(value);
        keyed_statics.push(KeyedStatic { item: value.item, instance: value.instance, definition: *definition });
        static_digests.push(*definition);
    }
    for value in built.statics {
        let Some(section) = value.section.as_deref() else { continue };
        if is_dep_static(&value.item, &value.instance) || ORDERED_SECTIONS.contains(&section) {
            continue;
        }
        if let Some(dep) = statics.iter().find(|it| it.section.as_deref() == Some(section)) {
            return Err(PremapError::of(format_args!(
                "the section `{section}` has the dependency static {:?} and the workspace static {:?}: \
                 its `__start_`/`__stop_` range would span two images",
                dep.item, value.item
            )));
        }
    }
    if statics.len() != owned.statics.len() {
        return Err(PremapError::of(format_args!(
            "codegen names {} dependency statics and the build defines {} of them",
            owned.statics.len(),
            statics.len()
        )));
    }

    let in_image = |target: &Target| match target {
        Target::Function(key) | Target::Direct(key) => is_dep(key),
        Target::Static { item, instance } => is_dep_static(item, instance),
        _ => false,
    };
    let mut names = Vec::with_capacity(built.names.len());
    names.extend(built.names.iter().filter(|it| in_image(&it.1)));
    let mut debug_names = Vec::with_capacity(built.debug_names.len());
    let mut debug_digests = Vec::with_capacity(built.debug_names.len());
    for (it, (key, digest)) in built.debug_names.iter().zip(&built.digests.debug_names) {
        if it.0 != *key {
            return Err(misaligned("debug names"));
        }
        if is_dep(&it.0) {
            debug_names.push(it);
            debug_digests.push(*digest);
        }
    }
    let mut natives_in = Vec::with_capacity(natives.len());
    natives_in.extend(natives.iter().filter(|it| it.dependency).map(|it| it.native));
    let natives = natives_in;
    Ok(ImagePart {
        functions,
        statics,
        names,
        debug_names,
        natives,
        keyed_functions,
        keyed_statics,
        function_digests,
        static_digests,
        debug_digests,
    })
}

/// Whether codegen gave one digest per function, static and debug name.
fn lined_up(built: &Built<'_>) -> Result<(), PremapError> {
    let digests = built.digests;
    if digests.functions.len() != built.functions.len() {
        return Err(misaligned("functions"));
    }
    if digests.statics.len() != built.statics.len() {
        return Err(misaligned("statics"));
    }
    if digests.debug_names.len() != built.debug_names.len() {
        return Err(misaligned("debug names"));
    }
    Ok(())
}

fn misaligned(what: &str) -> PremapError {
    PremapError::of(format_args!("codegen's digests of the build's {what} are not one of each, in their order"))
}

fn pair(item: &ItemKey, instance: &ContentId) -> [u8; 64] {
    let mut out = [0; 64];
    out[..32].copy_from_slice(&item.0.0);
    out[32..].copy_from_slice(&instance.0);
    out
}

/// A set of IDs (or pairs of IDs), each with its index in the list it was made of,
/// open-addressed from the IDs' first bytes, which are uniform.
#[derive(Clone, Copy)]
struct Keys<'s, const N: usize> {
    slots: &'s [([u8; N], u32)],
    mask: usize,
}

impl<'s, const N: usize> Keys<'s, N> {
    /// The slots of a set of distinct `keys`, at most half full.
    fn slots<'f>(keys: impl ExactSizeIterator<Item = [u8; N]>, frame: &'f Frame) -> Vec<([u8; N], u32), &'f Frame> {
        let len = (keys.len() * 2).next_power_of_two().max(2);
        let mut slots = Vec::with_capacity_in(len, frame);
        slots.resize(len, ([0; N], u32::MAX));
        for (index, key) in keys.enumerate() {
            let mut at = Keys::start(&key) & (len - 1);
            while slots[at].1 != u32::MAX {
                at = (at + 1) & (len - 1);
            }
            slots[at] = (key, index as u32);
        }
        slots
    }

    fn new(slots: &'s [([u8; N], u32)]) -> Keys<'s, N> {
        Keys { slots, mask: slots.len() - 1 }
    }

    fn get(&self, key: &[u8; N]) -> Option<u32> {
        let mut at = Keys::start(key) & self.mask;
        loop {
            let (slot, index) = &self.slots[at];
            if *index == u32::MAX {
                return None;
            }
            if slot == key {
                return Some(*index);
            }
            at = (at + 1) & self.mask;
        }
    }

    fn start(key: &[u8; N]) -> usize {
        let word = |at: usize| u64::from_le_bytes(key[at..at + 8].try_into().expect("8 bytes"));
        (if N > 32 { word(0) ^ word(32) } else { word(0) }) as usize
    }
}

/// The image's, and the build's.
const IMAGE: u8 = 0;
/// The workspace's.
const WORKSPACE: u8 = 1;
/// The dependencies', not in the image.
const SPILLED: u8 = 2;
/// The image's, and not the build's.
const STALE: u8 = 3;
/// Its digest is of another piece: codegen's lists don't line up.
const MISALIGNED: u8 = 4;

/// What `image` is for `built`, and when it serves it, what of `built` it does not hold.
pub fn workspace_part<'a>(
    built: &Built<'a>,
    owned: &Owned<'_>,
    image: &View<'_>,
    natives: &[SidedNative<'a>],
) -> Result<Fit<'a>, PremapError> {
    sorted(owned)?;
    lined_up(built)?;
    frame::task(Tag::Link, |frame| workspace_part_in(built, owned, image, natives, frame))
}

fn workspace_part_in<'a>(
    built: &Built<'a>,
    owned: &Owned<'_>,
    image: &View<'_>,
    natives: &[SidedNative<'a>],
    frame: &Frame,
) -> Result<Fit<'a>, PremapError> {
    let digests = built.digests;
    let function_class = |at: usize| {
        let (key, digest) = &digests.functions[at];
        if built.functions[at].key != *key {
            return MISALIGNED;
        }
        match image.entry(&SymbolId::function(key)) {
            Some(entry) if image.content(entry.refs.content) == Some(*digest) => IMAGE,
            Some(_) => STALE,
            None if owned.functions.binary_search_by(|it| it.0.cmp(key)).is_ok() => SPILLED,
            None => WORKSPACE,
        }
    };
    let functions_of = classes(built.functions.len(), function_class, frame);
    if functions_of.contains(&MISALIGNED) {
        return Err(misaligned("functions"));
    }
    if let Some(at) = functions_of.iter().position(|it| *it == STALE) {
        return Ok(Fit::Stale(Stale::Function(built.functions[at].key)));
    }
    let statics_of = match static_classes(built, owned, image, frame)? {
        Ok(classes) => classes,
        Err(stale) => return Ok(Fit::Stale(stale)),
    };
    let debug_class = |at: usize| {
        let (key, digest) = &digests.debug_names[at];
        if built.debug_names[at].0 != *key {
            return MISALIGNED;
        }
        match image.entry(&SymbolId::function(key)) {
            Some(entry) if image.debug_name(entry.refs.debug) == Some(*digest) => IMAGE,
            Some(_) => STALE,
            None => WORKSPACE,
        }
    };
    let debug_of = classes(built.debug_names.len(), debug_class, frame);
    if debug_of.contains(&MISALIGNED) {
        return Err(misaligned("debug names"));
    }
    if let Some(at) = debug_of.iter().position(|it| *it == STALE) {
        return Ok(Fit::Stale(Stale::Function(built.debug_names[at].0)));
    }
    if debug_of.iter().filter(|it| **it == IMAGE).count() != image.debug_names() {
        return Ok(Fit::Stale(Stale::Extra));
    }

    let target_of = |target: &Target| match target {
        Target::Function(key) | Target::Direct(key) => image.entry(&SymbolId::function(key)),
        Target::Static { item, instance } => image.entry(&SymbolId::of_static(item, instance)),
        _ => None,
    };
    let mut names_of = Vec::with_capacity_in(built.names.len(), frame);
    let mut matched = 0;
    for (at, (name, target)) in built.names.iter().enumerate() {
        let Some(target) = target_of(target) else {
            names_of.push(WORKSPACE);
            continue;
        };
        match image.get(&SymbolId::name(name)) {
            Some(def) if def.value == target.def.value && def.kind == target.def.kind => {
                names_of.push(IMAGE);
                matched += 1;
            }
            _ => return Ok(Fit::Stale(Stale::Name(at))),
        }
    }
    if matched != image.names() {
        return Ok(Fit::Stale(Stale::Extra));
    }

    let mut ordered = Vec::new_in(frame);
    for (value, class) in built.statics.iter().zip(statics_of.iter()) {
        if *class == IMAGE && value.section.as_deref().is_some_and(|it| ORDERED_SECTIONS.contains(&it)) {
            ordered.push(pair(&value.item, &value.instance));
        }
    }
    ordered.sort_unstable();
    let rust = [u32::MAX; 3];
    if image.init().chain(image.fini()).any(|it| it.order == rust && ordered.binary_search(&it.tie).is_err()) {
        return Ok(Fit::Stale(Stale::Extra));
    }

    let functions = pick(built.functions, &functions_of, |it| *it != IMAGE);
    let spilled = functions.iter().zip(functions_of.iter().filter(|it| **it != IMAGE));
    let spilled = spilled.filter(|(_, class)| **class == SPILLED).map(|(it, _)| it.key).collect(); // alloc: edge: none but when the image lacks pieces
    let statics = pick(built.statics, &statics_of, |it| *it != IMAGE);
    let spilled_statics = statics.iter().zip(statics_of.iter().filter(|it| **it != IMAGE));
    let spilled_statics = spilled_statics.filter(|(_, class)| **class == SPILLED).map(|(it, _)| (it.item, it.instance));
    let spilled_statics = spilled_statics.collect(); // alloc: edge: none but when the image lacks pieces
    let names = pick(built.names, &names_of, |it| *it == WORKSPACE);
    let debug_names = pick(built.debug_names, &debug_of, |it| *it == WORKSPACE);
    let workspace_links = |it: &&SidedNative<'a>| match it.native.kind {
        _ if !it.dependency => true,
        NativeKind::Object | NativeKind::Archive { whole: true } => false,
        NativeKind::Archive { whole: false } | NativeKind::Shared { .. } => true,
    };
    let mut kept = Vec::with_capacity(natives.iter().filter(workspace_links).count());
    kept.extend(natives.iter().filter(workspace_links).map(|it| it.native));
    let natives = kept;
    Ok(Fit::Part(WorkspacePart { functions, statics, names, debug_names, natives, spilled, spilled_statics }))
}

/// The class of each of `built.statics`, by one merge of codegen's sorted digests with the
/// image's sorted statics and the dependencies' sorted statics, hashing nothing; or the first
/// the image holds otherwise.
fn static_classes<'f>(
    built: &Built<'_>,
    owned: &Owned<'_>,
    image: &View<'_>,
    frame: &'f Frame,
) -> Result<Result<Vec<u8, &'f Frame>, Stale>, PremapError> {
    let mut out = Vec::with_capacity_in(built.statics.len(), frame);
    let (mut held, mut deps) = (0, 0);
    let mut last = None;
    for (value, (id, definition)) in built.statics.iter().zip(&built.digests.statics) {
        if (value.item, value.instance) != *id || last.is_some_and(|it| it >= *id) {
            return Err(misaligned("statics"));
        }
        last = Some(*id);
        while held < image.statics() && image.static_record(held).0 < *id {
            held += 1;
        }
        let class = match held < image.statics() && image.static_record(held).0 == *id {
            true if image.static_record(held).1 == *definition => IMAGE,
            true => return Ok(Err(Stale::Static(id.0, id.1))),
            false => {
                while deps < owned.statics.len() && owned.statics[deps] < *id {
                    deps += 1;
                }
                match owned.statics.get(deps) == Some(id) {
                    true => SPILLED,
                    false => WORKSPACE,
                }
            }
        };
        out.push(class);
    }
    Ok(Ok(out))
}

/// The class of each index below `len`, on the pool's threads for a long list, in a frame buffer.
fn classes<'f>(len: usize, class: impl Fn(usize) -> u8 + Sync, frame: &'f Frame) -> Vec<u8, &'f Frame> {
    let mut out = Vec::with_capacity_in(len, frame);
    out.resize(len, WORKSPACE);
    let run = |(chunk, slots): (usize, &mut [u8])| {
        for (at, slot) in slots.iter_mut().enumerate() {
            *slot = class(chunk * CHUNK + at);
        }
    };
    if len <= CHUNK {
        run((0, out.as_mut_slice()));
    } else {
        par::for_each(par::enumerate(par::chunks_mut(out.as_mut_slice(), CHUNK)), run);
    }
    out
}

/// The items whose class is `of`, in their order, in a list of exactly their count.
fn pick<'a, T>(items: &'a [T], classes: &[u8], of: impl Fn(&u8) -> bool) -> Vec<&'a T> {
    let mut out = Vec::with_capacity(classes.iter().filter(|it| of(it)).count());
    out.extend(items.iter().zip(classes).filter(|(_, class)| of(class)).map(|(item, _)| item));
    out
}

fn sorted(owned: &Owned<'_>) -> Result<(), PremapError> {
    if !owned.functions.is_sorted_by(|a, b| a.0 < b.0) || !owned.statics.is_sorted_by(|a, b| a < b) {
        return Err(PremapError::of(format_args!("codegen's dependency instances are not sorted by key, each once")));
    }
    Ok(())
}
