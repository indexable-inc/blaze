//! Answering an edit without writing the whole object again: the object of the last full build
//! stays as it is, and a small one with the functions that changed is linked ahead of it.
//!
//! The base gives its instances and statics names the patch can refer to. What the patch
//! defines under a name of the base replaces it there, for the linker takes the first
//! definition it sees. Whatever the patch can't express starts a full build over.

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use base_db::{Crate, EditionedFileId, FileId};
use cranelift_module::{Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, FunctionId, HasModule, Lookup, StaticId,
    signatures::FunctionSignature,
};
use hir_expand::{FileRange, HirFileId};
use hir_ty::mir::location::line_and_column;
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
use span::ErasedFileAstId;

pub(super) type FxIndexMap<K, V> = indexmap::IndexMap<K, V, FxBuildHasher>;

use hir_ty::{
    db::HirDatabase,
    next_solver::{DbInterner, Ty, TyKind},
    traits::StoredParamEnvAndCrate,
};
use intern::Symbol;
use rustc_type_ir::{TypeFoldable, TypeFolder, TypeSuperFoldable, inherent::IntoKind as _};

use super::{
    CodegenError, ContentId, Fan, Instance, InstanceDef, ObjectCodegen, OwnStatics, Phases,
    Rejections, Result, Roots, SymbolResolver, TheirStatics, Walked, async_blocks,
    cached::WrittenItem,
    compiled_instance, compiled_root, compiled_static, declaration_location,
    fragment::{CompiledFunction, DataRequest, FuncTarget, Requests, TextPart},
    home::{body_file, body_files, instance_gone, mentioned_crates, type_gone, written_in},
    instance_serial, is_thread_local,
    itempush::{Asked, ItemPush, Settled},
    left_to_rustc,
    naming::Naming,
    root_serial, row_location, source_location,
    stable::StableNames,
    static_serial, target,
};

/// What tells the code of a function from any other in every run of the daemon: [`Mark`] by
/// what it refers to, written as stable names, with each crate of the checkout named by the
/// id `names` has for it. A base that has code from the store has this of it from the store,
/// and an edit compiles what it can have changed to compare, naming the crates of the
/// checkout by their ids in the base. `None` when something it refers to has no stable name.
pub(super) fn stable_mark(
    db: &dyn HirDatabase,
    names: &StableNames<'_, '_>,
    compiled: &CompiledFunction,
    debug: bool,
) -> Option<u128> {
    let mut hasher = rustc_stable_hash::StableSipHasher128::new();
    // Left out with the variables below: a full build and a later compile of the same code
    // can give different named locals a slot of their own.
    compiled.code.hash_with(&mut hasher, false);
    for request in &compiled.requests.funcs {
        request.sig.hash(&mut hasher);
        match &request.target {
            FuncTarget::Symbol(symbol) => (0u8, symbol).hash(&mut hasher),
            FuncTarget::Path(path) => (1u8, path).hash(&mut hasher),
            FuncTarget::Instance(instance, _) => (2u8, names.instance(instance)?).hash(&mut hasher),
            FuncTarget::Asm(text) => (3u8, text).hash(&mut hasher),
        }
    }
    let place = |at: Option<(FileId, u32, u32)>| {
        at.map(|(file, line, column)| (names.file_path(file), line, column))
    };
    for request in &compiled.requests.datas {
        match request {
            DataRequest::Blob { blob, identity } => (0u8, blob, identity).hash(&mut hasher),
            DataRequest::Vtable { ty, trait_ref, blob } => {
                let trait_ref = trait_ref.as_ref().map(|(id, args)| (*id, args.as_ref()));
                (1u8, names.vtable(ty.as_ref(), trait_ref)?, blob).hash(&mut hasher)
            }
            DataRequest::Static(id) => (2u8, names.static_name(*id)?).hash(&mut hasher),
            DataRequest::Location(at) => {
                let at = at.and_then(|(owner, span)| source_location(db, owner, span));
                (3u8, place(at)).hash(&mut hasher)
            }
            DataRequest::Declaration(func) => {
                (4u8, place(*declaration_location(db, *func))).hash(&mut hasher)
            }
            DataRequest::FilePath(file) => (5u8, names.file_path(*file)).hash(&mut hasher),
            DataRequest::Text(parts) => {
                6u8.hash(&mut hasher);
                for part in parts {
                    match part {
                        TextPart::Text(text) => (0u8, text).hash(&mut hasher),
                        TextPart::FilePath(file) => (1u8, names.file_path(*file)).hash(&mut hasher),
                    }
                }
            }
        }
    }
    // As [`Mark::of_function`] has them: the variables a debugger is told of are left out.
    if debug && let Some((owner, spans)) = &compiled.source {
        for row in &compiled.code.lines {
            let span = spans.get(row.span as usize);
            let at = span.and_then(|span| row_location(db, *owner, compiled.closure, *span));
            place(at).hash(&mut hasher);
        }
    }
    let super::type_id::Fingerprint(low, high) = hasher.finish();
    Some(u128::from(low) | u128::from(high) << 64)
}

/// The items of the checkout the code of an instance comes from, as they are written now:
/// the one its code is written in, with where it begins when `lines` (its mark has the lines
/// of its code then), and those of what it tells the place of, with where each begins; those
/// of the bodies it mentions a coroutine of, by their text alone (see
/// [`super::home::bodies_of`]). `None` when one is of no file of the checkout or a macro
/// wrote it. While nothing but bodies changes, code whose items read the same is the code it
/// was, or code nothing calls by its name any more.
pub(super) fn written_items(
    db: &dyn HirDatabase,
    names: &StableNames<'_, '_>,
    instance: &Instance,
    compiled: &CompiledFunction,
    lines: bool,
) -> Option<Vec<WrittenItem>> {
    let (written, mentioned) = super::home::bodies_of(db, instance)?;
    let mut placed = Vec::new();
    for request in &compiled.requests.datas {
        match request {
            DataRequest::Location(Some((owner, _))) => placed.push(*owner),
            DataRequest::Declaration(func) => placed.push((*func).into()),
            _ => {}
        }
    }
    let item = |body: DefWithBodyId, places: bool| {
        let (file, id, hash, at) = (*body_print(db, body))?;
        let file = names.file_path(file.file_id(db))?;
        Some(WrittenItem { file, id, hash, at: places.then_some(at) })
    };
    let placed = placed.into_iter().map(|it| item(it, true));
    let written = written.into_iter().map(|it| item(it, lines));
    let mentioned = mentioned.into_iter().map(|it| item(it, false));
    let mut items: Vec<WrittenItem> =
        placed.chain(written).chain(mentioned).collect::<Option<_>>()?;
    items.sort_unstable();
    items.dedup();
    Some(items)
}

/// [`item_print`] of the item a body is of, with its file and its id there.
#[salsa::tracked(returns(ref))]
fn body_print(
    db: &dyn HirDatabase,
    body: DefWithBodyId,
) -> Option<(EditionedFileId, u32, u64, (u32, u32))> {
    let (file, id) = match body {
        DefWithBodyId::FunctionId(it) => (it.lookup(db).id.file_id, it.lookup(db).id.value.erase()),
        DefWithBodyId::ConstId(it) => (it.lookup(db).id.file_id, it.lookup(db).id.value.erase()),
        DefWithBodyId::StaticId(it) => (it.lookup(db).id.file_id, it.lookup(db).id.value.erase()),
        DefWithBodyId::VariantId(it) => (it.lookup(db).id.file_id, it.lookup(db).id.value.erase()),
    };
    let file = file.file_id()?;
    let (hash, at) = item_print(db, file, id)?;
    Some((file, id.into_raw(), hash, at))
}

/// A hash of the text of the item `id` of `file`, and the line and column it begins on.
fn item_print(
    db: &dyn HirDatabase,
    file: EditionedFileId,
    id: ErasedFileAstId,
) -> Option<(u64, (u32, u32))> {
    let range = HirFileId::from(file).ast_id_map(db).try_get_erased(id)?.text_range();
    let text = db.file_text(file.file_id(db));
    let written = text.text(db).get(usize::from(range.start())..usize::from(range.end()))?;
    let mut hasher = rustc_stable_hash::StableSipHasher128::new();
    written.hash(&mut hasher);
    let super::type_id::Fingerprint(hash, _) = hasher.finish();
    let (_, line, column) = line_and_column(db, FileRange { file_id: file, range })?;
    Some((hash, (line, column)))
}

/// Whether each of `items` that is of a file in `edited`, by its name, reads as it did.
fn items_unchanged(
    db: &dyn HirDatabase,
    items: &[WrittenItem],
    edited: &FxHashMap<String, EditionedFileId>,
) -> bool {
    items.iter().all(|item| {
        let Some(&file) = edited.get(&item.file) else { return true };
        let Some((hash, at)) = item_print(db, file, ErasedFileAstId::from_raw(item.id)) else {
            return false;
        };
        hash == item.hash && item.at.is_none_or(|was| was == at)
    })
}

/// What a compiled function or static was when the base took it in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Mark {
    serial: u128,
    /// The source locations it reports, which stitching fills in.
    locations: u64,
    /// The lines its code is of, when a debugger is told them.
    lines: u64,
}

impl Mark {
    fn new(db: &dyn HirDatabase, serial: u128, requests: &Requests) -> Mark {
        let mut hasher = rustc_hash::FxHasher::default();
        for request in &requests.datas {
            match request {
                DataRequest::Location(Some((owner, span))) => {
                    source_location(db, *owner, *span).hash(&mut hasher)
                }
                DataRequest::Declaration(func) => declaration_location(db, *func).hash(&mut hasher),
                _ => {}
            }
        }
        Mark { serial, locations: hasher.finish(), lines: 0 }
    }

    /// The mark of a function, whose code is a patch's to describe again when `debug` is set
    /// and a line of it is another: an edit above it in the file moves them all.
    fn of_function(
        db: &dyn HirDatabase,
        serial: u128,
        compiled: &CompiledFunction,
        debug: bool,
    ) -> Mark {
        let mut hasher = rustc_hash::FxHasher::default();
        if debug && let Some((owner, spans)) = &compiled.source {
            for row in &compiled.code.lines {
                let span = spans.get(row.span as usize);
                span.map(|span| row_location(db, *owner, compiled.closure, *span))
                    .hash(&mut hasher);
            }
        }
        Mark { lines: hasher.finish(), ..Mark::new(db, serial, &compiled.requests) }
    }

    /// The mark of a root, `None` when it does not compile.
    fn of_root(db: &dyn HirDatabase, root: FunctionId, debug: bool) -> Option<Mark> {
        if debug {
            return root_mark(db, root);
        }
        let compiled = compiled_root(db, root).as_ref().ok()?;
        Some(Mark::of_function(db, root_serial(db, root), compiled, false))
    }

    /// The mark of an instance, `None` when it does not compile.
    fn of_instance(
        db: &dyn HirDatabase,
        instance: &Instance,
        env: &StoredParamEnvAndCrate,
        debug: bool,
    ) -> Option<Mark> {
        if debug {
            return instance_mark(db, instance.clone(), env.clone());
        }
        let compiled = compiled_instance(db, instance.clone(), env.clone()).as_ref().ok()?;
        let serial = instance_serial(db, instance.clone(), env.clone());
        Some(Mark::of_function(db, serial, compiled, false))
    }

    /// Whether the code is the same, wherever in the file it is: a copy of rustc's that no
    /// patch reaches does what it did, and only a debugger is told the lines it was at.
    fn same_code(self, other: Mark) -> bool {
        Mark { lines: 0, ..self } == Mark { lines: 0, ..other }
    }
}

/// The mark of a root with the lines of its code. Every edit asks it of every root the edit
/// can reach, and found again it would ask where each row of the code is; kept, asking again
/// verifies the places it read, which instances of one function share.
#[salsa::tracked(returns(copy))]
fn root_mark(db: &dyn HirDatabase, root: FunctionId) -> Option<Mark> {
    let compiled = compiled_root(db, root).as_ref().ok()?;
    Some(Mark::of_function(db, root_serial(db, root), compiled, true))
}

/// [`root_mark`], of an instance.
#[salsa::tracked(returns(copy))]
fn instance_mark(
    db: &dyn HirDatabase,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Option<Mark> {
    let compiled = compiled_instance(db, instance.clone(), env.clone()).as_ref().ok()?;
    let serial = instance_serial(db, instance, env);
    Some(Mark::of_function(db, serial, compiled, true))
}

/// Something in the base that an edit can change or take away.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum Node {
    Root(FunctionId),
    Instance(Instance),
    Static(StaticId),
}

impl Node {
    fn is_asked_for_by(&self, requests: &Requests) -> bool {
        match self {
            Node::Root(_) => false,
            Node::Instance(instance) => requests.funcs.iter().any(
                |it| matches!(&it.target, FuncTarget::Instance(asked, _) if asked == instance),
            ),
            Node::Static(id) => requests
                .datas
                .iter()
                .any(|it| matches!(it, DataRequest::Static(asked) if asked == id)),
        }
    }
}

/// What a query gives for something of the base, or `None` when it panics: the edit may have
/// taken away what the base knew it by, and only looking tells. A build that is interrupted
/// goes on unwinding: that says nothing about the base. One that waited for a thread whose
/// own look panicked has found what that thread found.
pub(super) fn probe<T>(query: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(query)) {
        Ok(found) => Some(found),
        Err(stopped) => match stopped.downcast_ref::<salsa::Cancelled>() {
            Some(salsa::Cancelled::PropagatedPanic) | None => None,
            Some(_) => std::panic::resume_unwind(stopped),
        },
    }
}

/// Whether `root` has a type or constant to be given, of its own or of its `impl` or trait
/// (`Self` of a trait's method).
pub(super) fn copied_by_rustc(db: &dyn HirDatabase, root: FunctionId) -> bool {
    let generics = hir_ty::generics::generics(db, root.into());
    generics.iter(false).any(|(id, _)| !matches!(id, hir_def::GenericParamId::LifetimeParamId(_)))
}

/// What a closure of the body of `root`, which is not generic, is compiled in.
fn closure_env(db: &dyn HirDatabase, root: FunctionId) -> StoredParamEnvAndCrate {
    let param_env = hir_ty::next_solver::ParamEnv::empty();
    hir_ty::traits::ParamEnvAndCrate { param_env, krate: root.krate(db) }.store()
}

fn closure_mark(
    db: &dyn HirDatabase,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
    debug: bool,
) -> Option<Mark> {
    Mark::of_instance(db, instance, env, debug)
}

#[derive(Clone)]
pub(super) struct BaseInstance {
    pub(super) env: StoredParamEnvAndCrate,
    pub(super) symbol: String,
    /// `None` when the base calls rustc's copy and compiling the instance failed.
    mark: Option<Mark>,
    forwarded: bool,
    /// The files whose bodies it comes from, or `None` when any may be one.
    files: Option<Box<[FileId]>>,
    /// What in the base refers to it.
    parents: Vec<Node>,
    /// The functions left to rustc reach it.
    theirs: bool,
}

/// An instance the functions left to rustc reach that the base has no code for.
#[derive(Clone)]
struct TheirInstance {
    instance: Instance,
    env: StoredParamEnvAndCrate,
    serial: Option<u128>,
    files: Option<Box<[FileId]>>,
}

/// A static the functions left to rustc reach that is rustc's.
#[derive(Clone)]
struct TheirStatic {
    id: StaticId,
    serial: Option<u128>,
    krate: Crate,
    file: FileId,
}

/// What the walk for [`TheirStatics`] went through, apart from the functions it starts at.
/// What it finds only changes with the code of one of these.
#[derive(Clone, Default)]
struct TheirWalk {
    instances: Vec<TheirInstance>,
    statics: Vec<TheirStatic>,
}

impl TheirWalk {
    /// Marks what of the base the walk went through, and keeps the rest.
    fn new(
        db: &dyn HirDatabase,
        found: &TheirStatics,
        walked: Walked,
        instances: &mut FxIndexMap<Instance, BaseInstance>,
        statics: &mut FxHashMap<StaticId, BaseStatic>,
    ) -> TheirWalk {
        instances.values_mut().for_each(|it| it.theirs = false);
        statics.values_mut().for_each(|it| it.theirs = false);
        let mut walk = TheirWalk::default();
        let mut seen = FxHashSet::default();
        for (instance, env) in walked {
            if let Some(known) = instances.get_mut(&instance)
                && known.env == env
            {
                known.theirs = true;
                continue;
            }
            if !seen.insert((instance.clone(), env.clone())) {
                continue;
            }
            let compiled = compiled_instance(db, instance.clone(), env.clone()).as_ref().ok();
            walk.instances.push(TheirInstance {
                serial: compiled.map(|_| instance_serial(db, instance.clone(), env.clone())),
                files: body_files(db, &instance),
                instance,
                env,
            });
        }
        for &id in &found.statics {
            if let Some(known) = statics.get_mut(&id) {
                known.theirs = true;
                continue;
            }
            walk.statics.push(TheirStatic {
                id,
                serial: compiled_static(db, id).as_ref().ok().map(|_| static_serial(db, id)),
                krate: id.krate(db),
                file: body_file(db, id.into()),
            });
        }
        walk
    }
}

/// A function rustc's copy of a function left to rustc calls rustc's copy of: an instance of
/// a generic one, which rustc puts in the crate that calls it, or one no copy of ours takes
/// the place of. rustc's copies are of the source the daemon started from, so the copy of
/// the function left to rustc is only good while each of these is what it was then.
#[derive(Clone)]
struct Reached {
    node: Node,
    env: StoredParamEnvAndCrate,
    /// The function left to rustc it was reached from.
    by: FunctionId,
    krate: Crate,
    files: Option<Box<[FileId]>>,
    /// Whether what it calls is among the others: one that does not compile calls nobody
    /// knows what.
    walked: bool,
}

impl Reached {
    /// From the functions left to rustc through what of rustc's they reach, stopping at a
    /// function of ours: rustc's code calls it by its symbol, which ours takes over. The
    /// functions of crates the build has none of are only rustc's, and no edit reaches them.
    fn walk(
        db: &dyn HirDatabase,
        from_rustc: &[FunctionId],
        compiled: &[(FunctionId, String)],
        skipped: &[(FunctionId, CodegenError)],
    ) -> Vec<Reached> {
        let ours: FxHashSet<FunctionId> = compiled.iter().map(|it| it.0).collect();
        let crates: FxHashSet<Crate> = compiled
            .iter()
            .map(|it| it.0)
            .chain(skipped.iter().map(|it| it.0))
            .map(|it| it.krate(db))
            .collect();
        let (mut stack, mut reached) = (Vec::new(), Vec::new());
        for &by in from_rustc {
            match compiled_root(db, by) {
                Ok(compiled) => stack.push((&compiled.requests, by)),
                Err(_) => reached.push(Reached {
                    node: Node::Root(by),
                    env: closure_env(db, by),
                    by,
                    krate: by.krate(db),
                    files: None,
                    walked: false,
                }),
            }
        }
        let mut seen = FxHashSet::default();
        while let Some((requests, by)) = stack.pop() {
            for request in &requests.funcs {
                let FuncTarget::Instance(instance, env) = &request.target else { continue };
                if !seen.insert((instance.clone(), env.clone())) {
                    continue;
                }
                let (node, krate, files, requests) = match instance.def {
                    InstanceDef::Fn(func) if instance.args.as_ref().is_empty() => {
                        if ours.contains(&func) || !crates.contains(&func.krate(db)) {
                            continue;
                        }
                        let compiled = compiled_root(db, func).as_ref().ok();
                        let files = Some(Box::from([body_file(db, func.into())]));
                        (Node::Root(func), func.krate(db), files, compiled.map(|it| &it.requests))
                    }
                    _ => {
                        let compiled = compiled_instance(db, instance.clone(), env.clone());
                        let krate = written_in(db, instance).map_or(env.krate, |it| it.krate(db));
                        let requests = compiled.as_ref().ok().map(|it| &it.requests);
                        (
                            Node::Instance(instance.clone()),
                            krate,
                            body_files(db, instance),
                            requests,
                        )
                    }
                };
                stack.extend(requests.map(|it| (it, by)));
                let walked = requests.is_some();
                reached.push(Reached { node, env: env.clone(), by, krate, files, walked });
            }
        }
        reached
    }

    fn mark(&self, db: &dyn HirDatabase) -> Option<Mark> {
        let (serial, compiled) = match &self.node {
            Node::Root(func) => (root_serial(db, *func), compiled_root(db, *func)),
            Node::Instance(instance) => (
                instance_serial(db, instance.clone(), self.env.clone()),
                compiled_instance(db, instance.clone(), self.env.clone()),
            ),
            Node::Static(_) => return None,
        };
        Some(Mark::new(db, serial, &compiled.as_ref().ok()?.requests))
    }

    /// The function whose code it is.
    fn func(&self, db: &dyn HirDatabase) -> Option<FunctionId> {
        match &self.node {
            Node::Root(func) => Some(*func),
            Node::Instance(instance) => written_in(db, instance),
            Node::Static(_) => None,
        }
    }
}

#[derive(Clone)]
pub(super) struct BaseStatic {
    pub(super) symbol: String,
    pub(super) writable: bool,
    mark: Mark,
    krate: Crate,
    /// The file its value is written in.
    file: FileId,
    parents: Vec<Node>,
    /// The functions left to rustc reach it.
    theirs: bool,
    /// rustc's copy, which has state that rustc's code shares with ours: a patch can't give
    /// it another first value.
    pub(super) shared: bool,
}

/// What a function left to rustc keeps the rest of the build from doing.
#[derive(Clone, PartialEq, Eq)]
struct Left {
    /// Its code, when the function compiles and something else keeps it from the object.
    /// A full build looks at it again once that changes.
    code: Option<Mark>,
    async_blocks: Option<FxHashSet<salsa::Id>>,
}

impl Left {
    fn of(db: &dyn HirDatabase, root: FunctionId) -> Left {
        let code = compiled_root(db, root).as_ref().ok();
        Left {
            code: code.map(|it| Mark::new(db, root_serial(db, root), &it.requests)),
            async_blocks: async_blocks(db, root).clone(),
        }
    }
}

/// A function left to rustc, or an instance our code calls rustc's copy of, whose copy is no
/// longer right for it.
pub struct OutOfDate {
    /// `None` for an instance written in no function, as drop glue is.
    pub func: Option<FunctionId>,
    /// Why ra-jit leaves it to rustc.
    pub why: String,
    /// Whether it was left to rustc in the build the daemon started from, when rustc compiled
    /// it from source that built: the reason is then ra-jit's, not an error of the source.
    pub left_from_the_start: bool,
    /// What rustc's copy calls a copy of rustc's of that changed, when it is not the function
    /// itself. `None` in it for code of no function of its own.
    pub calls: Option<Option<FunctionId>>,
}

/// The functions left to rustc as rustc compiled them, and what of rustc's they call: its
/// copies are not built again while the daemon runs, so one is only good for as long as its
/// function, and what it calls of rustc's, is what it was then.
#[derive(Clone, Default)]
pub(crate) struct Frozen {
    /// `None` for one an edit had reached by the time it was first left to rustc: what rustc
    /// compiled of it is not known.
    left: Option<FxHashMap<FunctionId, Option<Left>>>,
    /// What each of [`Reached`] compiled to when first reached, with `None` as for `left`, and
    /// `Some(None)` for one that did not compile.
    reached: FxHashMap<(Node, StoredParamEnvAndCrate), Option<Option<Mark>>>,
    /// The crates that have seen an edit since.
    edited: FxHashSet<Crate>,
    /// The files that have seen an edit to the bodies of functions alone, which can change
    /// no function written elsewhere.
    edited_bodies: FxHashSet<FileId>,
    /// The crates rustc compiled from other source than the daemon first saw them with: no
    /// copy of a function of theirs is good.
    distrusted: FxHashSet<Crate>,
    /// The first value rustc gave each static with state that a build asked about
    /// ([`RustcValues`]).
    statics: Arc<std::sync::Mutex<FxHashMap<StaticId, Option<Mark>>>>,
}

/// The first value rustc gave each static with state (a thread-local, or one written at run
/// time), as rustc compiled it: such a static stays rustc's copy only while its first value
/// is still that one (`docs/live.md#statics`). Taken the first time a build asks, when no
/// edit since the daemon started can have reached the static; `None` when one had, and
/// rustc's value is not known.
#[derive(Clone, Default)]
pub(crate) struct RustcValues {
    known: Arc<std::sync::Mutex<FxHashMap<StaticId, Option<Mark>>>>,
    /// The crates an edit since the start can have changed a value of: those edited and
    /// what depends on them, and those rustc compiled from other source.
    touched: Arc<FxHashSet<Crate>>,
}

impl RustcValues {
    /// Whether rustc's copy of `id` still starts from the value the source gives it now. One
    /// that does not compile has nothing of ours to tell by, and is rustc's.
    ///
    /// An edit of bodies alone is no edit of a value: those are bodies of functions that are
    /// not `const` ([`Rejections::touch_bodies`]), which no value is evaluated through.
    pub(crate) fn kept(&self, db: &dyn HirDatabase, id: StaticId) -> bool {
        let Ok(compiled) = compiled_static(db, id).as_ref() else { return true };
        let now = Mark::new(db, static_serial(db, id), &compiled.requests);
        let touched = self.touched.contains(&id.krate(db));
        let mut known = self.known.lock().unwrap_or_else(|e| e.into_inner());
        let was = *known.entry(id).or_insert((!touched).then_some(now));
        was == Some(now)
    }
}

impl Frozen {
    /// What a build asks of [`RustcValues`]: the values taken so far, shared with it, and the
    /// edits as they are now.
    pub(crate) fn rustc_values(&self) -> RustcValues {
        let touched = self.edited.iter().chain(&self.distrusted).copied().collect();
        RustcValues { known: self.statics.clone(), touched: Arc::new(touched) }
    }

    pub(crate) fn distrust(&mut self, crates: impl IntoIterator<Item = Crate>) {
        self.distrusted.extend(crates);
    }

    pub(crate) fn edit_bodies(&mut self, file: FileId) {
        self.edited_bodies.insert(file);
    }

    pub(crate) fn edit(&mut self, crates: impl IntoIterator<Item = Crate>) {
        self.edited.extend(crates);
    }

    /// Whether an edit since the daemon started can have changed what the root is.
    fn touched_root(&self, db: &dyn HirDatabase, root: FunctionId) -> bool {
        self.edited.contains(&root.krate(db))
            || self.edited_bodies.contains(&body_file(db, root.into()))
    }

    /// Whether an edit since the daemon started can have changed what rustc compiled of a
    /// function of `krate` that is generic over nothing: an edit of the crate, or of what it
    /// depends on, which is in `edited` too; or of the bodies of a file of the crate or of one
    /// it depends on, whose generic and inline functions rustc copies into it.
    fn touched_from(&self, db: &dyn HirDatabase, krate: Crate) -> bool {
        if self.edited.contains(&krate) {
            return true;
        }
        if self.edited_bodies.is_empty() {
            return false;
        }
        let crates: FxHashSet<Crate> = krate.transitive_deps(db).into_iter().collect();
        self.edited_bodies
            .iter()
            .any(|&file| base_db::relevant_crates(db, file).iter().any(|it| crates.contains(it)))
    }

    fn touched(&self, db: &dyn HirDatabase, reached: &Reached) -> bool {
        let bodies = match &reached.files {
            // What it calls is of its crates or of what they depend on: rustc copies the
            // generic functions of those into it.
            _ if !reached.walked => {
                let mut crates: FxHashSet<Crate> = FxHashSet::default();
                for krate in [reached.krate, reached.env.krate] {
                    crates.insert(krate);
                    crates.extend(krate.transitive_deps(db));
                }
                let in_crates = |file: FileId| {
                    base_db::relevant_crates(db, file).iter().any(|it| crates.contains(it))
                };
                self.edited_bodies.iter().any(|&it| in_crates(it))
            }
            Some(files) => files.iter().any(|it| self.edited_bodies.contains(it)),
            None => !self.edited_bodies.is_empty(),
        };
        bodies || self.edited.contains(&reached.krate) || self.edited.contains(&reached.env.krate)
    }

    /// A function or instance of `base` that rustc's copy is no longer right for, and why. The
    /// first call with a base of ours notes what rustc compiled: of what no edit has reached by
    /// then.
    pub(crate) fn out_of_date(&mut self, db: &dyn HirDatabase, base: &Base) -> Option<OutOfDate> {
        let now = || {
            let theirs = base.skipped.iter().filter(|it| !it.1.no_symbol());
            theirs.map(|(root, why)| (*root, why))
        };
        let distrusted = |root: FunctionId| self.distrusted.contains(&root.krate(db));
        let Some(left) = &self.left else {
            let other = now().find(|it| distrusted(it.0)).map(|(func, why)| OutOfDate {
                func: Some(func),
                why: why.to_string(),
                left_from_the_start: true,
                calls: None,
            });
            let left = now()
                .map(|(root, _)| (root, (!self.touched_root(db, root)).then(|| Left::of(db, root))))
                .collect();
            self.left = Some(left);
            return other.or_else(|| self.out_of_date(db, base));
        };
        let own = now().find_map(|(root, why)| {
            let fresh = match left.get(&root) {
                _ if distrusted(root) => false,
                // Nothing to tell a change by: any edit that it can see may be one.
                Some(Some(Left { code: None, .. })) => !self.touched_root(db, root),
                Some(Some(was)) => probe(|| Left::of(db, root)).as_ref() == Some(was),
                Some(None) => false,
                // Not left at first, as one compiled then: rustc's copy is still of the source
                // the daemon started with, when no edit since reaches it.
                None => !self.touched_from(db, root.krate(db)),
            };
            (!fresh).then(|| OutOfDate {
                func: Some(root),
                why: why.to_string(),
                left_from_the_start: left.contains_key(&root),
                calls: None,
            })
        });
        if own.is_some() {
            return own;
        }
        let why = |by| base.skipped.iter().find(|it| it.0 == by).map(|it| it.1.to_string());
        for reached in &base.reached {
            let key = (reached.node.clone(), reached.env.clone());
            let touched = self.touched(db, reached);
            let fresh = match self.reached.get(&key) {
                Some(Some(_)) if !touched => true,
                Some(Some(Some(was))) => probe(|| reached.mark(db)).flatten() == Some(*was),
                // Without code to tell a change by, any edit that reaches it may be one.
                Some(Some(None) | None) => false,
                None => {
                    let known = (!touched).then(|| probe(|| reached.mark(db)).flatten());
                    self.reached.insert(key, known);
                    !touched
                }
            };
            if !fresh {
                return Some(OutOfDate {
                    func: Some(reached.by),
                    why: why(reached.by).unwrap_or_default(),
                    left_from_the_start: true,
                    calls: Some(reached.func(db).filter(|&it| it != reached.by)),
                });
            }
        }
        // Our code calls rustc's copy of an instance that did not compile, which is of its
        // function as rustc compiled it.
        let edited = !self.edited.is_empty() || !self.edited_bodies.is_empty();
        let mut forwarded = base.instances.iter().filter(|it| edited && it.1.forwarded);
        forwarded.find_map(|(it, known)| {
            let (crates, files) = (mentioned_crates(db, it), known.files.as_deref());
            let untouched = match (crates, files) {
                (Some(crates), Some(files)) => {
                    crates.is_disjoint(&self.edited)
                        && files.iter().all(|it| !self.edited_bodies.contains(it))
                }
                // Of a function generic over nothing, rustc's copy has only what its crate
                // and those below it wrote.
                _ if it.args.as_ref().iter().all(|arg| arg.region().is_some()) => {
                    written_in(db, it).is_some_and(|func| !self.touched_from(db, func.krate(db)))
                }
                _ => false,
            };
            (!untouched).then(|| OutOfDate {
                func: written_in(db, it),
                why: "an instance of it does not compile".to_owned(),
                left_from_the_start: false,
                calls: None,
            })
        })
    }
}

/// The last object built in full, as far as patching it goes.
#[derive(Clone)]
pub(super) struct Base {
    /// Whether the object says which lines its code is of, as a patch of it then does.
    debug: bool,
    entries: Vec<FunctionId>,
    pub(super) compiled: Vec<(FunctionId, String)>,
    marks: Vec<Mark>,
    /// The crate of each of `compiled`, and the file it is written in.
    crates: Vec<Crate>,
    files: Vec<FileId>,
    touched: Touched,
    pub(super) skipped: Vec<(FunctionId, CodegenError)>,
    /// For each of `skipped` that rustc has a copy of.
    left: Vec<Option<Left>>,
    /// The skipped functions that rustc has a copy of.
    from_rustc: Vec<FunctionId>,
    their_statics: TheirStatics,
    their_walk: TheirWalk,
    /// What rustc's copies of `from_rustc` call of rustc's.
    reached: Vec<Reached>,
    pub(super) instances: FxIndexMap<Instance, BaseInstance>,
    homes: Homes,
    /// What the bodies of `compiled` and `instances` name.
    naming: Naming,
    pub(super) statics: FxHashMap<StaticId, BaseStatic>,
    /// The symbols of the functions built from entries of the store, by their stable names.
    /// No edit reaches what those are compiled from.
    pub(super) found: FxHashMap<Arc<str>, String>,
    /// What each of those is an instance of, by its symbol, for tiering to build it into the
    /// hot functions that call it.
    pub(super) found_instances: FxHashMap<String, (Instance, StoredParamEnvAndCrate)>,
    /// Some of `found` is code of the checkout, which an edit does reach, and nothing here
    /// tells what of it: the first edit takes a full build without any.
    untracked: bool,
    /// The rest of `found` that is code of the checkout, which an edit is looked for in by
    /// what the store told of it.
    hydrated: Vec<Hydrated>,
    hydrated_homes: HydratedHomes,
    /// The roots of `compiled` put in the object from the store, by their index there, with
    /// their [`stable_mark`]: what tells an edit of them, which `marks` does not.
    stored_marks: FxHashMap<u32, u128>,
    /// The items the code of those comes from, as the build that kept each saw them.
    stored_items: FxHashMap<u32, Box<[WrittenItem]>>,
    /// The ids the crates of the checkout were named by in the build, which a look names what
    /// the build had from the store by. Empty when it had nothing of the checkout from there.
    ids: FxHashMap<Crate, ContentId>,
    /// The crates whose code changes under the daemon, as the build had them.
    owned: FxHashSet<Crate>,
    /// What code from the store refers to among `instances` and `statics`: no `parents` say
    /// so, and a root can reach it through that code.
    reached_from_store: FxHashSet<Node>,
    outdated: bool,
    /// Why what rustc linked can't be patched any longer, when that is what this is.
    outdated_by: Option<&'static str>,
    /// The instances that did not compile when the base was built. What called them was left
    /// to rustc, and stays there for as long as the base does.
    rejected: Vec<(Instance, StoredParamEnvAndCrate)>,
    pub(super) bodies: usize,
    /// The roots and instances that are `#[ra_jit::cache]` functions, by their index in
    /// `compiled` and `instances`. The hash of what each reaches is in its code, and an edit
    /// anywhere can change that.
    cached_roots: Vec<u32>,
    cached_instances: Vec<u32>,
    /// Whether the base is the executable rustc linked, and not an object of ours: every
    /// function and static in it is rustc's. It knows the files that were about to change,
    /// in `homed`, as they were when rustc compiled them, and nothing else.
    pub(super) rustc: bool,
    homed: FxHashSet<FileId>,
    /// The functions of the `homed` files that rustc's symbols name no copy of, with what
    /// each compiled to then: the symbols may miss one rustc has, whose callers no patch
    /// reaches, so one that changes declines.
    unnamed: Vec<(FunctionId, Option<Mark>, FileId)>,
    /// The closures written in the functions of the `homed` files.
    closures: Vec<HomedClosures>,
    /// The generic functions of the modules of each `homed` file by name, and whether each of
    /// rustc's copies of those of the name is an instance of the base, which a patch replaces
    /// under rustc's symbol.
    copies_homed: FxHashMap<FileId, FxHashMap<String, bool>>,
    /// What the roots and instances refer to by item, for an edit that changes items: `None`
    /// for a base of rustc's, and once an edit left it unable to tell.
    items: Option<Box<ItemPush>>,
    /// Where a thread of its own puts [`Self::items`] once it made them, of the revision the
    /// base is of: they are taken before the next edit, and none are when they aren't ready.
    making: Option<Making>,
    /// Those an edit asked about by a name bound to another thing, whose items are to be
    /// taken again before the next edit.
    reitem: Option<Asked>,
    /// Where threads of their own put what [`Self::reitem`] refers to once the patch of the
    /// edit is made: the next edit waits for it, and asks itself when it is of another
    /// revision or the threads were stopped.
    settling: Option<Settling>,
    /// The revisions the database went through since the last edit by taking in files it did
    /// not have, each from one to the next: the edit they are of pushes them, so what was made
    /// of the items at one of them is of the next too.
    went: Vec<(salsa::Revision, salsa::Revision)>,
    /// Where a thread of its own puts the code of the functions of [`Self::items`] it wrote,
    /// which moves are found by: taken before the next edit if it is done by then.
    coding: Option<Coding>,
    /// The instances of the base whose function an edit moved to another place, which the
    /// base's symbol of each stands for still: what called it there calls it so.
    relocated: FxHashMap<Instance, Relocated>,
    /// The instances of `relocated` by the one each is now.
    relocated_now: FxHashMap<Instance, Instance>,
    /// The functions of `relocated` by the one each is now.
    relocated_funcs: FxHashMap<FunctionId, FunctionId>,
    /// The code of the instances of functions of the files an edit changes, from before it,
    /// by the function each is of then: what one the edit moves compiles to where it is now.
    snapshots: FxHashMap<FunctionId, (Instance, Arc<CompiledFunction>)>,
}

/// An instance of the base whose function an edit moved.
#[derive(Clone)]
struct Relocated {
    /// The instance it is now.
    now: Instance,
    /// What it compiles to now, when that is known without compiling it: the edit that moved
    /// it changed nothing its code is of, so that is the code from before with what the edit
    /// moved of it where it is now. Each edit after compiles it.
    code: Option<Arc<CompiledFunction>>,
}

/// `code` from before an edit, of the function where the edit moved it, with what else of it
/// the edit moved (`moved`) where that is now. `None` when it refers to anything else the
/// edit took away, is generic over anything, or has where something is written in it: `named`
/// has each item moved as a path ends in it.
fn relocated(
    db: &dyn HirDatabase,
    code: &CompiledFunction,
    moved: &FxHashMap<FunctionId, FunctionId>,
    adts: &FxHashMap<AdtId, AdtId>,
    named: &[Vec<u8>],
    live: &blaze_items::Live,
) -> Option<CompiledFunction> {
    use blaze_items::ItemId;
    let func = |it: FunctionId| match moved.get(&it) {
        Some(&now) => Some(now),
        None => live.has(db, ItemId::Function(it)).then_some(it),
    };
    let body = |it: DefWithBodyId| match it {
        DefWithBodyId::FunctionId(it) => func(it).map(Into::into),
        other => live.has_body(db, other).then_some(other),
    };
    let plain = |env: &StoredParamEnvAndCrate| {
        let param_env = hir_ty::next_solver::ParamEnv::empty();
        *env == hir_ty::traits::ParamEnvAndCrate { param_env, krate: env.krate }.store()
    };
    // A constant can have the path of an item in it, as `type_name` writes it in a `const`
    // block: one that has the name of an item moved anywhere in it is compiled again.
    let names = |bytes: &[u8]| named.iter().any(|it| bytes.windows(it.len()).any(|w| w == &it[..]));
    let blob_named = code.requests.datas.iter().any(|it| match it {
        DataRequest::Blob { blob, .. } => names(&blob.bytes),
        _ => false,
    });
    if code.requests.placed || blob_named {
        return None;
    }
    let mut types = Moves { interner: DbInterner::new_no_crate(db), funcs: moved, adts };
    let mut now = code.clone();
    for request in &mut now.requests.funcs {
        let FuncTarget::Instance(instance, env) = &mut request.target else { continue };
        match &mut instance.def {
            InstanceDef::Fn(it) | InstanceDef::VtableShim(it) => *it = func(*it)?,
            _ => {}
        }
        instance.args = instance.args.as_ref().fold_with(&mut types).store();
        if !plain(env) || instance_gone(db, instance, live) {
            return None;
        }
    }
    for request in &mut now.requests.datas {
        match request {
            DataRequest::Vtable { ty, trait_ref, .. } => {
                *ty = ty.as_ref().fold_with(&mut types).store();
                if let Some((_, args)) = trait_ref {
                    *args = args.as_ref().fold_with(&mut types).store();
                }
                let trait_gone = trait_ref.as_ref().is_some_and(|(id, args)| {
                    !live.has(db, ItemId::Trait(*id)) || type_gone(db, &args.as_ref(), live)
                });
                if trait_gone || type_gone(db, &ty.as_ref(), live) {
                    return None;
                }
            }
            DataRequest::Static(id) if !live.has(db, ItemId::Static(*id)) => return None,
            DataRequest::Location(Some((owner, _))) => *owner = body(*owner)?,
            DataRequest::Declaration(it) => *it = func(*it)?,
            _ => {}
        }
    }
    if let Some((owner, _)) = &mut now.source {
        *owner = body(*owner)?;
    }
    if let Some((env, variables)) = &mut now.variables {
        for it in variables.iter_mut() {
            it.ty = it.ty.as_ref().fold_with(&mut types).store();
        }
        if !plain(env) || variables.iter().any(|it| type_gone(db, &it.ty.as_ref(), live)) {
            return None;
        }
    }
    Some(now)
}

/// The name an item is written with, which a path to it ends in.
fn item_name(db: &dyn HirDatabase, id: blaze_items::ItemId) -> Option<String> {
    use blaze_items::ItemId;
    use hir_def::signatures::{
        ConstSignature, EnumSignature, StaticSignature, StructSignature, TraitSignature,
        TypeAliasSignature, UnionSignature,
    };
    let name = match id {
        ItemId::Function(it) => FunctionSignature::of(db, it).name.clone(),
        ItemId::Adt(AdtId::StructId(it)) => StructSignature::of(db, it).name.clone(),
        ItemId::Adt(AdtId::EnumId(it)) => EnumSignature::of(db, it).name.clone(),
        ItemId::Adt(AdtId::UnionId(it)) => UnionSignature::of(db, it).name.clone(),
        ItemId::Trait(it) => TraitSignature::of(db, it).name.clone(),
        ItemId::Const(it) => ConstSignature::of(db, it).name.clone()?,
        ItemId::Static(it) => StaticSignature::of(db, it).name.clone(),
        ItemId::Alias(it) => TypeAliasSignature::of(db, it).name.clone(),
        ItemId::Impl(_) | ItemId::Macro(_) => return None,
    };
    Some(name.as_str().to_owned())
}

/// Types with what an edit moved of them where that is now: each is written as it was, so
/// is laid out as it was.
struct Moves<'db, 'a> {
    interner: DbInterner<'db>,
    funcs: &'a FxHashMap<FunctionId, FunctionId>,
    adts: &'a FxHashMap<AdtId, AdtId>,
}

impl<'db> TypeFolder<DbInterner<'db>> for Moves<'db, '_> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        match ty.kind() {
            TyKind::Adt(adt, args) if let Some(&now) = self.adts.get(&adt.def_id()) => {
                Ty::new_adt(self.interner, now, args.fold_with(self))
            }
            TyKind::FnDef(def, args) => {
                let def = match def.0 {
                    CallableDefId::FunctionId(it) => {
                        CallableDefId::FunctionId(*self.funcs.get(&it).unwrap_or(&it))
                    }
                    CallableDefId::StructId(it) => match self.adts.get(&AdtId::StructId(it)) {
                        Some(&AdtId::StructId(now)) => CallableDefId::StructId(now),
                        _ => def.0,
                    },
                    other => other,
                };
                Ty::new(self.interner, TyKind::FnDef(def.into(), args.fold_with(self)))
            }
            _ => ty.super_fold_with(self),
        }
    }
}

/// See [`Base::making`].
type Making = Arc<std::sync::Mutex<Option<(salsa::Revision, Box<ItemPush>)>>>;

/// See [`Base::coding`]: the code of each function, of the index after so many pushes.
type Coding = Arc<std::sync::Mutex<Option<(u64, Vec<FunctionId>, Vec<blaze_items::Code>)>>>;

/// The code of the functions of `index` without it, on the threads `fan` has, for `coding`.
fn write_codes(
    db: &dyn HirDatabase,
    fan: &Fan<'_>,
    (pushes, funcs): (u64, Vec<FunctionId>),
    live: blaze_items::Live,
    coding: &Coding,
) {
    let start = std::time::Instant::now();
    let stats = std::env::var_os("RA_JIT_STATS").is_some();
    if stats {
        eprintln!("items-coding: {} functions", funcs.len());
    }
    // A revision the daemon goes to meanwhile, as it takes in a file, stops the queries, and
    // nothing here can go on before the thread lets go of the database: what was written by
    // then is kept, and an edit of two files writes what its files lack.
    let mut stopped = false;
    let mut codes = Vec::with_capacity(funcs.len());
    for part in funcs.chunks(1024) {
        let write = || blaze_items::codes_of(db, fan, part, Some(&live));
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(write)) {
            Ok(written) => codes.extend(written),
            Err(why) if why.is::<salsa::Cancelled>() => {
                stopped = true;
                break;
            }
            Err(why) => std::panic::resume_unwind(why),
        }
    }
    let mut funcs = funcs;
    funcs.truncate(codes.len());
    drop(live);
    if stats {
        let written = codes.iter().filter(|it| it.is_some()).count();
        eprintln!(
            "items-codes: {written} of {} functions in {} us{}",
            funcs.len(),
            start.elapsed().as_micros(),
            if stopped { ", stopped by a revision" } else { "" }
        );
    }
    *coding.lock().unwrap_or_else(|e| e.into_inner()) = Some((pushes, funcs, codes));
}

/// See [`Base::settling`].
type Settling =
    Arc<std::sync::Mutex<Option<std::sync::mpsc::Receiver<(salsa::Revision, Settled)>>>>;

/// A function of the checkout the base has from the store: no build here compiled it, so
/// nothing but its name says what it is an instance of. A look finds that out from the code
/// of what calls it, when an edit can have changed it.
#[derive(Clone)]
struct Hydrated {
    name: Arc<str>,
    symbol: String,
    /// Its [`stable_mark`], as the build that kept it computed it.
    mark: u128,
    /// The crate it is compiled as, when that is of the checkout.
    krate: Option<Crate>,
    /// The files its code comes from, by the names [`StableNames::file_path`] gives them, or
    /// `None` when any file can be one.
    files: Option<Box<[Box<str>]>>,
    /// The items its code comes from, as the build that kept it saw them.
    items: Option<Box<[WrittenItem]>>,
    /// What calls it in the base, which its instance is found through.
    callers: Vec<Caller>,
    /// Its instance, when code the build compiled called it. Else it is found again at each
    /// look: an edit can give what its callers call the same name and another meaning.
    known: Option<(Instance, StoredParamEnvAndCrate)>,
}

/// What one look found of [`Base::hydrated`], by the index there: the instance each is, or
/// that none is, and whether each is left uncalled.
#[derive(Default)]
struct FoundHydrated {
    instances: FxHashMap<usize, Option<(Instance, StoredParamEnvAndCrate)>>,
    dropped: FxHashMap<usize, bool>,
}

/// A root from the store by its index in [`Base::compiled`], another of [`Base::hydrated`],
/// or what the build compiled.
#[derive(Clone)]
enum Caller {
    Root(u32),
    Hydrated(u32),
    Base(Node),
}

/// Where in [`Base::hydrated`] the code of each crate and of each file is.
#[derive(Clone, Default)]
struct HydratedHomes {
    of_crate: FxHashMap<Crate, Vec<u32>>,
    /// By the names [`StableNames::file_path`] gives the files.
    of_file: FxHashMap<Box<str>, Vec<u32>>,
    of_any_file: Vec<u32>,
    /// The code whose crate is not one of the checkout's, which any edit can reach.
    of_any_crate: Vec<u32>,
}

/// The closures rustc compiled of the body of a function of a file an edit changes. rustc's
/// code that takes one as an argument is of its type then: one that captures or is called
/// otherwise declines, as does one rustc's symbols name no copy of that changed.
#[derive(Clone)]
struct HomedClosures {
    root: FunctionId,
    file: FileId,
    /// As [`super::closures_of`] has them.
    closures: Vec<Option<(Instance, u64)>>,
    /// Those rustc's symbols name no copy of, or that did not compile, with what each
    /// compiled to then. The others are among the instances of the base.
    unnamed: Vec<(Instance, Option<Mark>)>,
    /// [`super::written_print`] of the root, for one with an `async` block, which no mark
    /// here says a change to.
    written: Option<u64>,
}

/// Where in the base the code of each crate is, and the code that comes from the bodies of
/// each file: an edit is looked for there, which is far less than the base.
#[derive(Clone, Default)]
struct Homes {
    root: FxHashMap<FunctionId, u32>,
    roots_of_crate: FxHashMap<Crate, Vec<u32>>,
    roots_of_file: FxHashMap<FileId, Vec<u32>>,
    instances_of_crate: FxHashMap<Crate, Vec<u32>>,
    instances_of_file: FxHashMap<FileId, Vec<u32>>,
    /// The instances that the bodies of any file may be part of.
    instances_of_any_file: Vec<u32>,
}

impl Homes {
    fn new(
        compiled: &[(FunctionId, String)],
        crates: &[Crate],
        files: &[FileId],
        instances: &FxIndexMap<Instance, BaseInstance>,
    ) -> Homes {
        let mut homes = Homes::default();
        for (i, &(root, _)) in compiled.iter().enumerate() {
            homes.root.insert(root, i as u32);
            homes.roots_of_crate.entry(crates[i]).or_default().push(i as u32);
            homes.roots_of_file.entry(files[i]).or_default().push(i as u32);
        }
        for (i, was) in instances.values().enumerate() {
            homes.instances_of_crate.entry(was.env.krate).or_default().push(i as u32);
            match &was.files {
                Some(files) => {
                    for &file in files {
                        homes.instances_of_file.entry(file).or_default().push(i as u32);
                    }
                }
                None => homes.instances_of_any_file.push(i as u32),
            }
        }
        homes
    }
}

/// `all` in order, each once.
fn sorted(mut all: Vec<u32>) -> Vec<usize> {
    all.sort_unstable();
    all.dedup();
    all.into_iter().map(|it| it as usize).collect()
}

/// What [`Base::stale`] looks at, handed to the threads that look a batch at a time. A
/// question takes from microseconds to most of an edit, and a thread that is done with its
/// batch takes the next of what is left rather than wait for the others to finish their share.
/// A batch is a quarter of a thread's share of what is left: taking one is an atomic add on a
/// line the others write too, hundreds of nanoseconds across the sockets, against the tens
/// of microseconds of the question it hands out.
pub struct Handout<'a> {
    parts: usize,
    check_all: bool,
    roots: Vec<usize>,
    instances: Vec<usize>,
    statics: Vec<(&'a StaticId, &'a BaseStatic)>,
    next: [std::sync::atomic::AtomicUsize; Look::COUNT],
}

/// The kinds of things [`Base::stale`] looks at, one after another.
#[derive(Clone, Copy)]
enum Look {
    Rejected,
    Roots,
    Unnamed,
    Closures,
    Instances,
    Hydrated,
    Statics,
    Skipped,
    TheirInstances,
    TheirStatics,
}

impl Look {
    const COUNT: usize = Look::TheirStatics as usize + 1;
}

impl Handout<'_> {
    /// The indices of the `len` things of the kind that this thread takes, as it gets to them.
    fn each(&self, look: Look, len: usize) -> impl Iterator<Item = usize> + '_ {
        use std::sync::atomic::Ordering::Relaxed;
        let next = &self.next[look as usize];
        let mut taken = 0..0;
        std::iter::from_fn(move || {
            if let Some(it) = taken.next() {
                return Some(it);
            }
            let seen = next.load(Relaxed);
            if seen >= len {
                return None;
            }
            let batch = ((len - seen) / (self.parts * 4)).max(1);
            let at = next.fetch_add(batch, Relaxed);
            taken = at..(at + batch).min(len);
            taken.next()
        })
    }
}

/// What an edit since the base was last looked at can have changed the code of, and what
/// differed from the base then, which does until an edit changes it back. Code compiled as a
/// crate depends on nothing but the files of that crate and of the ones it depends on. Of a
/// file where only the bodies of functions changed, and none of them is one that the rest of
/// the program sees into, it depends on no more than the bodies it comes from.
#[derive(Clone, Default)]
struct Touched {
    /// The crates that see a file that changed.
    crates: FxHashSet<Crate>,
    /// The files that changed in the bodies of functions alone.
    bodies: FxHashSet<FileId>,
    /// A file changed that we can't tell the crates of.
    any: bool,
    /// The crates that see a file of which only the signatures of functions changed that
    /// nothing else sees into, and the roots and instances of those whose bodies name one of
    /// them, by their index in the base. Of what else those crates have, everything.
    around: FxHashSet<Crate>,
    named_roots: FxHashSet<u32>,
    named_instances: FxHashSet<u32>,
    /// What the last look found changed.
    roots: FxHashSet<FunctionId>,
    instances: FxHashSet<Instance>,
    statics: FxHashSet<StaticId>,
    /// By their index in [`Base::hydrated`].
    hydrated: FxHashSet<u32>,
}

impl Touched {
    fn reaches(&self, krate: Crate) -> bool {
        self.any || self.crates.contains(&krate)
    }

    /// [`Touched::reaches`], or one of the crates around signatures that changed.
    fn reaches_around(&self, krate: Crate) -> bool {
        self.reaches(krate) || self.around.contains(&krate)
    }

    /// Whether a file changed since the last look.
    fn edited(&self) -> bool {
        self.any || !self.crates.is_empty() || !self.bodies.is_empty() || !self.around.is_empty()
    }

    /// Starts over from what a look at the whole base found: the edits before it can have
    /// changed nothing else.
    fn settle(&mut self, found: &[Stale]) {
        // A look that gave up, or that a thread did not come back from, did not get to
        // everything.
        if found.is_empty() || found.iter().any(|it| it.declined.is_some()) {
            return;
        }
        *self = Touched::default();
        for stale in found {
            self.roots.extend(&stale.roots);
            // One taken away differs from the base until an edit brings it back, which a look
            // tells only by asking again.
            self.roots.extend(&stale.gone);
            self.instances.extend(stale.instances.iter().cloned());
            self.statics.extend(&stale.statics);
            self.hydrated.extend(stale.hydrated.iter().map(|(at, ..)| *at));
            for (node, _) in &stale.broken {
                match node {
                    Node::Instance(instance) => _ = self.instances.insert(instance.clone()),
                    Node::Static(id) => _ = self.statics.insert(*id),
                    Node::Root(_) => {}
                }
            }
        }
    }

    fn reaches_bodies(&self, files: Option<&[FileId]>) -> bool {
        if self.bodies.is_empty() {
            return false;
        }
        files.is_none_or(|files| files.iter().any(|it| self.bodies.contains(it)))
    }
}

/// What a look at a part of the base found changed.
#[derive(Clone, Default)]
pub struct Stale {
    roots: Vec<FunctionId>,
    instances: Vec<Instance>,
    /// By their index in [`Base::hydrated`], with the instance each was found to be.
    hydrated: Vec<(u32, Instance, StoredParamEnvAndCrate)>,
    /// The statics with another value, which a patch defines again under their name.
    statics: Vec<StaticId>,
    /// What a patch can't replace, and why. That only matters for what the program still
    /// reaches.
    broken: Vec<(Node, &'static str)>,
    /// The functions of the base an edit took away: what rustc or the base has of them stays
    /// where nothing calls it any more.
    gone: Vec<FunctionId>,
    /// Why no patch can hold the change.
    declined: Option<&'static str>,
    /// Something the functions left to rustc reach changed, so what they reach may have.
    their_walk_changed: bool,
    pub steps: StaleSteps,
}

/// How long looking at each kind of thing in the base took, in the order they are looked at.
#[derive(Default, Clone, Copy)]
pub struct StaleSteps {
    pub roots: std::time::Duration,
    pub instances: std::time::Duration,
    pub statics: std::time::Duration,
    /// The functions left to rustc.
    pub left: std::time::Duration,
    /// How many roots and instances an edit could have changed, and so were asked about.
    pub roots_asked: usize,
    pub instances_asked: usize,
    /// How many of those from the store were told unchanged by their items, not compiled.
    pub roots_unchanged: usize,
    pub instances_unchanged: usize,
    /// The longest any one of them took to answer.
    pub slowest: std::time::Duration,
    /// How many took how long: the `n`th counts those under 2^n ns and not under half that.
    pub sizes: [u32; 32],
}

impl StaleSteps {
    fn asked(&mut self, took: std::time::Duration) {
        self.slowest = self.slowest.max(took);
        let ns = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        self.sizes[(64 - ns.leading_zeros() as usize).min(31)] += 1;
    }
}

impl Stale {
    /// Declines when the functions the base left to rustc reach other statics than they did.
    fn declined(mut self, why: &'static str) -> Stale {
        self.declined = Some(why);
        self
    }
}

/// What of the base an edit left as it was: everything but these.
#[derive(Default)]
pub(super) struct Unchanged {
    pub(super) changed: FxHashSet<Node>,
    pub(super) broken: FxHashSet<Node>,
    /// The roots the edit took away, which refer to nothing any more.
    pub(super) gone: FxHashSet<FunctionId>,
}

pub(super) struct Patch {
    pub(super) object: Vec<u8>,
    pub(super) bodies: usize,
    pub(super) phases: Phases,
}

/// An object that defines nothing.
pub(super) fn empty_object() -> Result<Vec<u8>> {
    let builder =
        ObjectBuilder::new(target()?.clone(), "ra-jit", cranelift_module::default_libcall_names())?;
    let object = ObjectModule::new(builder)
        .finish()
        .emit()
        .map_err(|e| CodegenError::Backend(e.to_string()))?;
    if super::Target::current().writes_wasm_objects() {
        return super::wasm_object::from_elf(&object, &Default::default())
            .map_err(CodegenError::Backend);
    }
    Ok(object)
}

/// What a full build had of the checkout from the store, as its base keeps track of it.
#[derive(Default)]
struct FromStore {
    /// Something of it the store told nothing of an edit by.
    untracked: bool,
    hydrated: Vec<Hydrated>,
    homes: HydratedHomes,
    ids: FxHashMap<Crate, ContentId>,
    /// What the roots put in the object from the store refer to, as `parents` has it.
    root_edges: Vec<(Node, Node)>,
    /// What the rest of it refers to.
    reached: FxHashSet<Node>,
    /// How many of the untracked were told of.
    untracked_seen: usize,
}

impl FromStore {
    fn new(
        db: &dyn HirDatabase,
        cg: &ObjectCodegen<'_, '_>,
        compiled: &[(FunctionId, String)],
        stored_marks: &FxHashMap<u32, u128>,
    ) -> FromStore {
        use super::cached::{CachedData, CachedTarget, Entry};
        let mut from = FromStore::default();
        let of_checkout: Vec<_> =
            cg.found.iter().filter(|(name, _)| StableNames::of_checkout(name)).collect();
        if of_checkout.is_empty() && stored_marks.is_empty() {
            return from;
        }
        let Some(entries) = cg.entries.as_deref().filter(|_| cg.mixed.is_none()) else {
            from.untracked = !of_checkout.is_empty();
            return from;
        };
        let owned = &cg.own_statics.crates;
        from.ids = super::linked_crates(db, owned.iter().copied())
            .into_iter()
            .filter(|krate| owned.contains(krate) || krate.data(db).origin.is_local())
            .filter_map(|krate| Some((krate, cg.names.content_id(krate)?)))
            .collect();
        let crate_of: FxHashMap<ContentId, Crate> =
            from.ids.iter().map(|(&krate, &id)| (id, krate)).collect();
        let decls = cg.module.declarations();
        let mut index: FxHashMap<&str, u32> = FxHashMap::default();
        let mut kept = Vec::with_capacity(of_checkout.len());
        for (name, id) in of_checkout {
            let entry = entries.get(name);
            let tracked = entry.as_ref().and_then(|it| it.tracked.clone());
            // One compiled as a crate from outside the checkout is of it by its generic
            // arguments, and one compiled as a crate of another checkout, whose id is of that
            // one's paths, by a crate its name need not say: any edit is looked at it for.
            let krate = tracked.as_ref().and_then(|it| crate_of.get(&ContentId(it.home?)));
            let why = if entry.is_none() { "no entry" } else { "no mark" };
            let (Some(entry), Some(tracked)) = (entry, tracked) else {
                from.untracked = true;
                if std::env::var_os("RA_JIT_STATS").is_some() && from.untracked_seen < 3 {
                    from.untracked_seen += 1;
                    eprintln!("base from the store: `{name}` untracked: {why}");
                }
                continue;
            };
            let at = from.hydrated.len() as u32;
            match krate {
                Some(&krate) => from.homes.of_crate.entry(krate).or_default().push(at),
                None => from.homes.of_any_crate.push(at),
            }
            match &tracked.files {
                Some(files) => {
                    for file in files {
                        from.homes.of_file.entry(file.as_str().into()).or_default().push(at);
                    }
                }
                None => from.homes.of_any_file.push(at),
            }
            index.insert(name, at);
            from.hydrated.push(Hydrated {
                name: name.clone(),
                symbol: decls.get_function_decl(*id).linkage_name(*id).into_owned(),
                mark: tracked.mark,
                krate: krate.copied(),
                files: tracked.files.map(|it| it.into_iter().map(Box::from).collect()),
                items: tracked.items.map(Box::from),
                callers: Vec::new(),
                known: cg.found_instances.get(id).cloned(),
            });
            kept.push(entry);
        }
        let instance_of: FxHashMap<_, _> = cg.instances.iter().map(|(it, &id)| (id, it)).collect();
        // What an entry calls among those, and what else of the base it refers to.
        let refers = |entry: &Entry| {
            let (mut hydrated, mut nodes) = (Vec::new(), Vec::new());
            for func in &entry.funcs {
                let CachedTarget::Named(name) = &func.target else { continue };
                if let Some(&at) = index.get(name.as_str()) {
                    hydrated.push(at);
                } else if let Some(instance) =
                    cg.named.get(name.as_str()).and_then(|id| instance_of.get(id))
                {
                    nodes.push(Node::Instance((*instance).clone()));
                }
            }
            for data in &entry.datas {
                if let CachedData::OwnStatic(name) = data
                    && let Some(id) = cg.names.static_named(name)
                {
                    nodes.push(Node::Static(id));
                }
            }
            (hydrated, nodes)
        };
        for (at, entry) in kept.iter().enumerate() {
            let (callees, nodes) = refers(entry);
            for callee in callees {
                from.hydrated[callee as usize].callers.push(Caller::Hydrated(at as u32));
            }
            from.reached.extend(nodes);
        }
        for &i in stored_marks.keys() {
            let root = compiled[i as usize].0;
            let name = cg.stored_roots.get(&root).cloned().flatten();
            let Some(entry) = name.and_then(|it| entries.get(&it)) else { continue };
            let (callees, nodes) = refers(&entry);
            for callee in callees {
                from.hydrated[callee as usize].callers.push(Caller::Root(i));
            }
            from.root_edges.extend(nodes.into_iter().map(|it| (it, Node::Root(root))));
        }
        from
    }
}

impl Base {
    /// The ids a patch names what the base has from the store by, when it has any of it.
    pub(super) fn frozen_ids(&self) -> Option<&FxHashMap<Crate, ContentId>> {
        Some(&self.ids).filter(|it| !it.is_empty())
    }

    /// `None` when the build has something in it a patch could not keep track of.
    pub(super) fn record(
        db: &dyn HirDatabase,
        cg: &ObjectCodegen<'_, '_>,
        entries: &[FunctionId],
        compiled: &[(FunctionId, String)],
        skipped: &[(FunctionId, CodegenError)],
    ) -> Option<Base> {
        let decls = cg.module.declarations();
        let debug = cg.debug.is_some();
        // What the store told of a root put in the object from there: a program with code of
        // rustc's settles its instances by its build, and is compiled to tell.
        let stored_entry = |root: FunctionId| {
            let name = cg.stored_roots.get(&root)?.as_ref()?;
            cg.entries.as_ref()?.get(name).filter(|_| cg.mixed.is_none())
        };
        let stored_tracked: FxHashMap<u32, super::cached::Tracked> = (0..compiled.len())
            .filter_map(|i| Some((i as u32, stored_entry(compiled[i].0)?.tracked.clone()?)))
            .collect();
        let stored_marks: FxHashMap<u32, u128> =
            stored_tracked.iter().map(|(&i, it)| (i, it.mark)).collect();
        let stored_items: FxHashMap<u32, Box<[WrittenItem]>> = (stored_tracked.into_iter())
            .filter_map(|(i, it)| Some((i, Box::from(it.items?))))
            .collect();
        let stored = |i: usize| stored_marks.contains_key(&(i as u32));
        let root_mark = |db: &dyn HirDatabase, root| Mark::of_root(db, root, debug);
        let instance_mark =
            |db: &dyn HirDatabase, (instance, env): &(Instance, StoredParamEnvAndCrate)| {
                (Mark::of_instance(db, instance, env, debug), body_files(db, instance))
            };
        // The marks are a hash of each function's code and of its lines, which every thread
        // works out: the object waits for this.
        let (envs, roots) = (cg.envs.as_slice(), compiled.len());
        let root_marks: Vec<_> = (0..roots).map(|_| std::sync::OnceLock::new()).collect();
        let instance_marks: Vec<_> = envs.iter().map(|_| std::sync::OnceLock::new()).collect();
        cg.symbols.in_parallel(roots + envs.len(), &|db, _, at| match at.checked_sub(roots) {
            Some(at) => _ = instance_marks[at].set(instance_mark(db, &envs[at])),
            None if stored(at) => {}
            None => _ = root_marks[at].set(root_mark(db, compiled[at].0)),
        });
        let nothing = Mark { serial: 0, locations: 0, lines: 0 };
        let marks = compiled
            .iter()
            .zip(root_marks)
            .enumerate()
            .map(|(i, (&(root, _), mark))| match stored(i) {
                true => Some(nothing),
                false => mark.into_inner().unwrap_or_else(|| root_mark(db, root)),
            })
            .collect::<Option<Vec<_>>>()?;
        let mut instances = envs
            .iter()
            .zip(instance_marks)
            .map(|(of, mark)| {
                let (instance, env) = of;
                let id = *cg.instances.get(instance)?;
                let (mark, files) = mark.into_inner().unwrap_or_else(|| instance_mark(db, of));
                let known = BaseInstance {
                    env: env.clone(),
                    symbol: decls.get_function_decl(id).linkage_name(id).into_owned(),
                    mark,
                    forwarded: cg.forwarded.contains(instance),
                    files,
                    parents: Vec::new(),
                    theirs: false,
                };
                (known.mark.is_some() || known.forwarded).then(|| (instance.clone(), known))
            })
            .collect::<Option<FxIndexMap<_, _>>>()?;
        // One that does not compile is never what it was: a change to the file declines.
        let nothing = Mark { serial: 0, locations: 0, lines: 0 };
        let base_static = |id: StaticId,
                           data,
                           compiled: Option<&super::fragment::CompiledStatic>,
                           shared| BaseStatic {
            symbol: decls.get_data_decl(data).linkage_name(data).into_owned(),
            writable: compiled.is_some_and(|it| it.writable),
            mark: compiled.map_or(nothing, |it| Mark::new(db, static_serial(db, id), &it.requests)),
            krate: id.krate(db),
            file: body_file(db, id.into()),
            parents: Vec::new(),
            theirs: false,
            shared,
        };
        let mut statics = cg
            .statics
            .iter()
            .chain(&cg.frozen_statics)
            .map(|(&id, &data)| {
                let compiled = compiled_static(db, id).as_ref().ok()?;
                Some((id, base_static(id, data, Some(compiled), false)))
            })
            .collect::<Option<FxHashMap<_, _>>>()?;
        // rustc's copies with state, so that an edit to a first value is seen.
        for (&id, &data) in &cg.shared_statics {
            let compiled = compiled_static(db, id).as_ref().ok();
            statics.insert(id, base_static(id, data, compiled, true));
        }
        let mut edges = Vec::new();
        let mut note = |parent: Node, requests: &Requests| {
            for request in &requests.funcs {
                if let FuncTarget::Instance(instance, _) = &request.target {
                    edges.push((Node::Instance(instance.clone()), parent.clone()));
                }
            }
            for request in &requests.datas {
                if let DataRequest::Static(id) = request {
                    edges.push((Node::Static(*id), parent.clone()));
                }
            }
        };
        let mut from_store = FromStore::new(db, cg, compiled, &stored_marks);
        if std::env::var_os("RA_JIT_STATS").is_some() {
            let known = from_store.hydrated.iter().filter(|it| it.known.is_some()).count();
            eprintln!(
                "base from the store: {} roots, {} other functions of the checkout ({known} called \
                 by code compiled here){}",
                stored_marks.len(),
                from_store.hydrated.len(),
                if from_store.untracked { ", some untracked" } else { "" },
            );
        }
        for (i, &(root, _)) in compiled.iter().enumerate() {
            if !stored(i) {
                note(Node::Root(root), &compiled_root(db, root).as_ref().ok()?.requests);
            }
        }
        for (instance, known) in &instances {
            if let Ok(compiled) = compiled_instance(db, instance.clone(), known.env.clone()) {
                note(Node::Instance(instance.clone()), &compiled.requests);
            }
        }
        for &id in statics.keys() {
            note(Node::Static(id), &compiled_static(db, id).as_ref().ok()?.requests);
        }
        edges.extend(from_store.root_edges.iter().cloned());
        let known_at: FxHashMap<&Instance, u32> = (from_store.hydrated.iter().enumerate())
            .filter_map(|(at, it)| Some((&it.known.as_ref()?.0, at as u32)))
            .collect();
        let mut called = Vec::new();
        for (child, parent) in &edges {
            if let Node::Instance(instance) = child
                && let Some(&at) = known_at.get(instance)
            {
                called.push((at, parent.clone()));
            }
        }
        for (at, parent) in called {
            from_store.hydrated[at as usize].callers.push(Caller::Base(parent));
        }
        for (child, parent) in edges {
            let parents = match &child {
                Node::Root(_) => None,
                Node::Instance(instance) => instances.get_mut(instance).map(|it| &mut it.parents),
                Node::Static(id) => statics.get_mut(id).map(|it| &mut it.parents),
            };
            if let Some(parents) = parents {
                parents.push(parent);
            }
        }
        let left =
            skipped.iter().map(|(root, e)| (!e.no_symbol()).then(|| Left::of(db, *root))).collect();
        let from_rustc: Vec<FunctionId> = left_to_rustc(db, cg.symbols, skipped).collect();
        let (their_statics, walked) = TheirStatics::walk(db, from_rustc.iter().copied());
        let their_walk = TheirWalk::new(db, &their_statics, walked, &mut instances, &mut statics);
        let reached = Reached::walk(db, &from_rustc, compiled, skipped);
        let rejected = cg
            .rejected
            .iter()
            .filter(|(instance, it)| {
                compiled_instance(db, (*instance).clone(), it.env.clone()).is_err()
            })
            .map(|(instance, it)| (instance.clone(), it.env.clone()))
            .collect();
        let crates: Vec<Crate> = compiled.iter().map(|&(root, _)| root.krate(db)).collect();
        let files: Vec<FileId> =
            compiled.iter().map(|&(root, _)| body_file(db, root.into())).collect();
        let cached = |func| super::fncache::cached_closure(db, func).is_some();
        let cached_roots =
            (0..compiled.len() as u32).filter(|&i| cached(compiled[i as usize].0)).collect();
        let cached_instances = (0..instances.len() as u32)
            .filter(|&i| {
                let (instance, _) = instances.get_index(i as usize).unwrap();
                matches!(instance.def, InstanceDef::Fn(func) if cached(func))
            })
            .collect();
        Some(Base {
            debug,
            rejected,
            entries: entries.to_vec(),
            compiled: compiled.to_vec(),
            marks,
            homes: Homes::new(compiled, &crates, &files, &instances),
            naming: {
                let start = std::time::Instant::now();
                let naming = Naming::new(db, compiled, &instances);
                if std::env::var_os("RA_JIT_STATS").is_some() {
                    eprintln!("naming-us: {}", start.elapsed().as_micros());
                }
                naming
            },
            crates,
            files,
            touched: Touched::default(),
            skipped: skipped.to_vec(),
            left,
            from_rustc,
            their_statics,
            their_walk,
            reached,
            instances,
            statics,
            found: cg
                .found
                .iter()
                .map(|(name, id)| {
                    (name.clone(), decls.get_function_decl(*id).linkage_name(*id).into_owned())
                })
                .collect(),
            found_instances: cg
                .found_instances
                .iter()
                .map(|(&id, it)| {
                    (decls.get_function_decl(id).linkage_name(id).into_owned(), it.clone())
                })
                .collect(),
            untracked: from_store.untracked,
            hydrated: from_store.hydrated,
            hydrated_homes: from_store.homes,
            stored_marks,
            stored_items,
            ids: from_store.ids,
            owned: cg.own_statics.crates.clone(),
            reached_from_store: from_store.reached,
            outdated: false,
            outdated_by: None,
            bodies: cg.bodies,
            cached_roots,
            cached_instances,
            rustc: false,
            homed: FxHashSet::default(),
            unnamed: Vec::new(),
            closures: Vec::new(),
            copies_homed: FxHashMap::default(),
            items: None,
            making: None,
            reitem: None,
            settling: None,
            went: Vec::new(),
            coding: None,
            relocated: FxHashMap::default(),
            relocated_now: FxHashMap::default(),
            relocated_funcs: FxHashMap::default(),
            snapshots: FxHashMap::default(),
        })
        .map(|mut base| {
            base.make_items(db, cg.symbols);
            base
        })
    }

    /// Has [`Self::items`] made on threads of their own, of the revision the database is at,
    /// when edits are to follow.
    fn make_items(&mut self, db: &dyn HirDatabase, symbols: &dyn SymbolResolver) {
        if self.rustc
            || !super::do_edits_follow()
            || std::env::var_os("RA_JIT_ITEM_PUSH").is_some_and(|it| it == "0")
        {
            return;
        }
        let making = Making::default();
        let (made, at) = (making.clone(), salsa::plumbing::current_revision(db));
        let (compiled, instances) =
            (self.compiled.clone(), self.instances.keys().cloned().collect::<Vec<_>>());
        let coding = Coding::default();
        let codes = coding.clone();
        let work = move |db: &dyn HirDatabase, fan: &Fan<'_>| {
            let start = std::time::Instant::now();
            let items = ItemPush::new(db, fan, &compiled, &instances);
            if std::env::var_os("RA_JIT_STATS").is_some() {
                eprintln!(
                    "items-us: {} for {} items",
                    start.elapsed().as_micros(),
                    items.index.items()
                );
            }
            let (wanting, live) = (items.index.wanting_code(), items.index.live());
            *made.lock().unwrap_or_else(|e| e.into_inner()) = Some((at, Box::new(items)));
            write_codes(db, fan, wanting, live, &codes);
        };
        symbols.when_idle(db, Box::new(work));
        self.making = Some(making);
        self.coding = Some(coding);
    }

    /// The executable rustc linked as a base, before anything of it is known: `home` takes
    /// in the files an edit is about to change. `debug` is whether a patch says which lines
    /// its code is of.
    pub(super) fn of_rustc(debug: bool) -> Base {
        Base {
            debug,
            entries: Vec::new(),
            compiled: Vec::new(),
            marks: Vec::new(),
            crates: Vec::new(),
            files: Vec::new(),
            touched: Touched::default(),
            skipped: Vec::new(),
            left: Vec::new(),
            from_rustc: Vec::new(),
            their_statics: TheirStatics::default(),
            their_walk: TheirWalk::default(),
            reached: Vec::new(),
            instances: FxIndexMap::default(),
            homes: Homes::default(),
            naming: Naming::default(),
            statics: FxHashMap::default(),
            found: FxHashMap::default(),
            found_instances: FxHashMap::default(),
            untracked: false,
            hydrated: Vec::new(),
            hydrated_homes: HydratedHomes::default(),
            stored_marks: FxHashMap::default(),
            stored_items: FxHashMap::default(),
            ids: FxHashMap::default(),
            owned: FxHashSet::default(),
            reached_from_store: FxHashSet::default(),
            outdated: false,
            outdated_by: None,
            rejected: Vec::new(),
            bodies: 0,
            cached_roots: Vec::new(),
            cached_instances: Vec::new(),
            rustc: true,
            homed: FxHashSet::default(),
            unnamed: Vec::new(),
            closures: Vec::new(),
            copies_homed: FxHashMap::default(),
            items: None,
            making: None,
            reitem: None,
            settling: None,
            went: Vec::new(),
            coding: None,
            relocated: FxHashMap::default(),
            relocated_now: FxHashMap::default(),
            relocated_funcs: FxHashMap::default(),
            snapshots: FxHashMap::default(),
        }
    }

    /// Takes in what rustc compiled of `file` before an edit changes it: the `functions`
    /// written in it that rustc has a symbol for, and the `statics` declared in it or in their
    /// bodies. The source is what rustc compiled until then, so the marks are of rustc's copies.
    pub(super) fn home(
        &mut self,
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        file: FileId,
        functions: &[FunctionId],
        statics: &[StaticId],
        callers: &[FunctionId],
    ) {
        if !self.homed.insert(file) {
            return;
        }
        for &root in functions {
            let known = self.homes.root.contains_key(&root)
                || self.skipped.iter().any(|it| it.0 == root)
                || self.unnamed.iter().any(|it| it.0 == root);
            // rustc copies a generic one into what calls it: a change to one is a change to
            // the file's copies, which no patch of what rustc linked is made for.
            if known || copied_by_rustc(db, root) {
                continue;
            }
            self.home_closures(db, symbols, root);
            let Some(symbol) = symbols.function(root) else {
                let mark = Mark::of_root(db, root, self.debug);
                self.unnamed.push((root, mark, body_file(db, root.into())));
                continue;
            };
            match compiled_root(db, root) {
                Ok(_) => {
                    let mark = Mark::of_root(db, root, self.debug);
                    self.marks.push(mark.expect("a root that compiles has a mark"));
                    self.compiled.push((root, symbol));
                    self.crates.push(root.krate(db));
                    self.files.push(body_file(db, root.into()));
                }
                Err(e) => {
                    self.skipped.push((root, e.clone()));
                    self.left.push(Some(Left::of(db, root)));
                }
            }
        }
        for &id in statics {
            if self.statics.contains_key(&id) {
                continue;
            }
            let compiled = compiled_static(db, id).as_ref().ok();
            // One that does not compile is never what it was: a change to the file declines.
            let mark = compiled.map_or(Mark { serial: 0, locations: 0, lines: 0 }, |it| {
                Mark::new(db, static_serial(db, id), &it.requests)
            });
            let known = BaseStatic {
                symbol: symbols.static_(id).unwrap_or_default(),
                writable: compiled.is_some_and(|it| it.writable),
                mark,
                krate: id.krate(db),
                file: body_file(db, id.into()),
                parents: Vec::new(),
                theirs: false,
                // rustc's copy, which rustc's code writes when the static has state.
                shared: is_thread_local(db, id) || compiled.is_some_and(|it| it.writable),
            };
            self.statics.insert(id, known);
        }
        let homed = self.home_copies(db, symbols, functions, callers);
        self.copies_homed.insert(file, homed);
        self.homes = Homes::new(&self.compiled, &self.crates, &self.files, &self.instances);
    }

    /// Whether [`Self::home`] noted `file`.
    pub(super) fn homed(&self, file: FileId) -> bool {
        self.homed.contains(&file)
    }

    /// Whether [`Self::home`] took in each of rustc's copies of the generic functions of the
    /// modules of `file` named `names`.
    pub(super) fn copies_homed(&self, file: FileId, names: &[String]) -> bool {
        let Some(homed) = self.copies_homed.get(&file) else { return false };
        names.iter().all(|it| homed.get(it).copied().unwrap_or(false))
    }

    /// Takes in rustc's copies of the generic functions of a file's modules, `functions` being
    /// what is written in it: the instances its other functions and `callers` reach, through
    /// the generic ones of `callers` too, and those reach in turn, that rustc's executable
    /// has, and the rest whose arguments rustc's path of them spells in types read back. A
    /// patch replaces one under rustc's symbol, as it does a closure. Whether those are all
    /// the copies the executable has of the functions of each name.
    fn home_copies(
        &mut self,
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        functions: &[FunctionId],
        callers: &[FunctionId],
    ) -> FxHashMap<String, bool> {
        // Of a module or an `impl` that is not generic, as the outline of a file tells them.
        let of_modules = |func: FunctionId| match func.lookup(db).container {
            hir_def::ItemContainerId::ModuleId(_) => true,
            hir_def::ItemContainerId::ImplId(imp) => {
                let generics = hir_ty::generics::generics(db, imp.into());
                !generics
                    .iter(false)
                    .any(|(id, _)| !matches!(id, hir_def::GenericParamId::LifetimeParamId(_)))
            }
            _ => false,
        };
        let generic: FxHashSet<FunctionId> = functions
            .iter()
            .copied()
            .filter(|&it| copied_by_rustc(db, it) && of_modules(it))
            .collect();
        let mut homed: FxHashMap<String, bool> = FxHashMap::default();
        if generic.is_empty() {
            return homed;
        }
        // rustc's copies by their paths, with the function each is of.
        let mut copies: FxHashMap<String, (FunctionId, String)> = FxHashMap::default();
        let mut unknown: FxHashSet<FunctionId> = FxHashSet::default();
        for &func in &generic {
            let Some(found) = symbols.rustc_instances(func) else {
                unknown.insert(func);
                continue;
            };
            for (path, symbol) in found {
                if let Some((other, _)) = copies.insert(path, (func, symbol)) {
                    unknown.extend([func, other]);
                }
            }
        }
        let through: FxHashSet<FunctionId> = generic
            .iter()
            .copied()
            .chain(callers.iter().copied().filter(|&it| copied_by_rustc(db, it)))
            .collect();
        let note = |requests: &Requests, queue: &mut Vec<(Instance, StoredParamEnvAndCrate)>| {
            for request in &requests.funcs {
                if let FuncTarget::Instance(instance, env) = &request.target
                    && let InstanceDef::Fn(func) = instance.def
                    && through.contains(&func)
                {
                    queue.push((instance.clone(), env.clone()));
                }
            }
        };
        let mut queue = Vec::new();
        let roots = functions.iter().chain(callers).copied().filter(|&it| !copied_by_rustc(db, it));
        for root in roots {
            let Ok(compiled) = compiled_root(db, root) else { continue };
            note(&compiled.requests, &mut queue);
            let env = closure_env(db, root);
            for (_, closure) in super::closures_of(db, root).into_iter().flatten() {
                let Some((instance, _)) = closure else { continue };
                let compiled = compiled_instance(db, instance, env.clone());
                if let Ok(compiled) = compiled.as_ref() {
                    note(&compiled.requests, &mut queue);
                }
            }
        }
        let mut found: FxIndexMap<Instance, StoredParamEnvAndCrate> = FxIndexMap::default();
        while let Some((instance, env)) = queue.pop() {
            if found.contains_key(&instance) {
                continue;
            }
            if let Ok(compiled) = compiled_instance(db, instance.clone(), env.clone()).as_ref() {
                note(&compiled.requests, &mut queue);
            }
            found.insert(instance, env);
        }
        let spelling = super::Spelling { db, symbols };
        let mut homed_ones = Vec::new();
        for (instance, env) in found {
            let InstanceDef::Fn(func) = instance.def else { continue };
            // One the executable has no copy of under its path is left to what calls it.
            let Some(path) = spelling.instance_path(func, instance.args.as_ref()) else {
                continue;
            };
            let Some((_, symbol)) = copies.remove(&path) else { continue };
            homed_ones.push((instance, env, symbol));
        }
        // One reached only through a `dyn`, or a generic caller past the search, is read off
        // rustc's path of it.
        copies.retain(|path, (func, symbol)| {
            if unknown.contains(func) {
                return true;
            }
            let Some(args) = spelling.instance_args(*func, path) else { return true };
            let checked = super::crate_checks_overflow(db, func.krate(db)).unwrap_or(false);
            let instance = Instance { def: InstanceDef::Fn(*func), args: args.store(), checked };
            homed_ones.push((instance, closure_env(db, *func), std::mem::take(symbol)));
            false
        });
        for (instance, env, symbol) in homed_ones {
            let InstanceDef::Fn(func) = instance.def else { continue };
            let mark = closure_mark(db, &instance, &env, self.debug);
            if mark.is_none() {
                unknown.insert(func);
                continue;
            }
            let files = body_files(db, &instance);
            let known = BaseInstance {
                env,
                symbol,
                mark,
                forwarded: false,
                files,
                parents: Vec::new(),
                theirs: false,
            };
            self.instances.entry(instance).or_insert(known);
        }
        unknown.extend(copies.into_values().map(|it| it.0));
        for func in generic {
            let name = FunctionSignature::of(db, func).name.as_str().to_owned();
            *homed.entry(name).or_insert(true) &= !unknown.contains(&func);
        }
        homed
    }

    /// Takes in the closures rustc compiled of the body of `root`: those it has a symbol of
    /// are instances of the base, which a patch replaces under it.
    fn home_closures(
        &mut self,
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        root: FunctionId,
    ) {
        let Some(found) = super::closures_of(db, root) else { return };
        if found.is_empty() {
            return;
        }
        let file = body_file(db, root.into());
        let env = closure_env(db, root);
        let mut unnamed = Vec::new();
        for (expr, closure) in &found {
            let Some((instance, _)) = closure else { continue };
            let mark = closure_mark(db, instance, &env, self.debug);
            match symbols.closure(root, *expr).filter(|_| mark.is_some()) {
                Some(symbol) => {
                    let known = BaseInstance {
                        env: env.clone(),
                        symbol,
                        mark,
                        forwarded: false,
                        files: Some(Box::new([file])),
                        parents: Vec::new(),
                        theirs: false,
                    };
                    self.instances.entry(instance.clone()).or_insert(known);
                }
                None => unnamed.push((instance.clone(), mark)),
            }
        }
        let closures: Vec<_> = found.into_iter().map(|it| it.1).collect();
        let written = match closures.iter().any(Option::is_none) {
            true => super::written_print(db, root),
            false => None,
        };
        self.closures.push(HomedClosures { root, file, closures, unnamed, written });
    }

    /// Whether an edit changed code that the base has from the store, which no patch can
    /// replace.
    pub(super) fn outdated(&self) -> bool {
        self.outdated
    }

    /// Why what rustc linked can't be patched any longer.
    pub(super) fn why_outdated(&self) -> Option<&'static str> {
        self.outdated_by
    }

    fn outdate_rustc(&mut self, why: &'static str) {
        if self.rustc {
            self.outdated = true;
            self.outdated_by.get_or_insert(why);
        }
    }

    /// Notes that a file changed: one of `crates`, or with `None` one we can't place.
    pub(super) fn touch(&mut self, db: &dyn HirDatabase, crates: Option<&[Crate]>) {
        // rustc's copies of what sees the file are of before, and nothing here says which.
        self.outdate_rustc("an edit that is more than the bodies of functions");
        match crates {
            Some(_) => self.bindings_edited(),
            None => (self.items, self.making) = (None, None),
        }
        let Some(crates) = crates else {
            self.touched.any = true;
            self.outdated |= self.untracked;
            return;
        };
        for &krate in crates {
            if !self.touched.crates.contains(&krate) {
                self.touched.crates.extend(krate.transitive_rev_deps(db));
            }
        }
        self.outdated |=
            self.untracked && self.crates.iter().any(|it| self.touched.crates.contains(it));
    }

    /// Notes that functions were added to a file of `crates`, or taken away, and nothing else of
    /// it changed but bodies that nothing else sees into, where no other crate depends on
    /// `crates`, no file of them named the new functions before (or names those taken away
    /// after), and `files` are their files. What rustc compiled of them is then known when
    /// [`Self::home`] took in every one of those files.
    pub(super) fn touch_added(&mut self, db: &dyn HirDatabase, crates: &[Crate], files: &[FileId]) {
        if !files.iter().all(|it| self.homed.contains(it)) {
            return self.touch(db, Some(crates));
        }
        self.bindings_edited();
        self.touched.crates.extend(crates.iter().copied());
        self.outdated |=
            self.untracked && self.crates.iter().any(|it| self.touched.crates.contains(it));
    }

    /// Notes that a file of `crates` changed in the signatures of the functions named `names`
    /// and in bodies alone: functions of its modules and plain `impl`s, not generic, and none of
    /// which the rest of the program sees into. What can have changed is their own code and
    /// that of what names them, and of what is seen into that names them, and so on.
    pub(super) fn touch_signatures(
        &mut self,
        db: &dyn HirDatabase,
        crates: &[Crate],
        file: FileId,
        names: &[Symbol],
    ) {
        let reached = self.naming.made.then(|| self.naming.reach(names)).flatten();
        let Some(reached) = reached.filter(|_| !self.rustc) else {
            return self.touch(db, Some(crates));
        };
        self.bindings_edited();
        // Its bodies too, as a body edit's.
        self.touched.bodies.insert(file);
        for &krate in crates {
            if !self.touched.around.contains(&krate) {
                self.touched.around.extend(krate.transitive_rev_deps(db));
            }
        }
        let around = &self.touched.around;
        let roots =
            self.naming.roots(&reached).filter(|&i| around.contains(&self.crates[i as usize]));
        self.touched.named_roots.extend(roots.collect::<Vec<_>>());
        let instances = self.naming.instances(&reached).filter(|&i| {
            self.instances
                .get_index(i as usize)
                .is_some_and(|(_, was)| around.contains(&was.env.krate))
        });
        self.touched.named_instances.extend(instances.collect::<Vec<_>>());
        // What came from the store is not in `naming`: all of it that can name them is asked.
        let hydrated = self.hydrated.iter().enumerate().filter_map(|(i, was)| {
            was.krate.is_none_or(|it| around.contains(&it)).then_some(i as u32)
        });
        self.touched.hydrated.extend(hydrated.collect::<Vec<_>>());
        if std::env::var_os("RA_JIT_STATS").is_some() {
            eprintln!(
                "signatures: {} names reach {} names, {} roots and {} instances",
                names.len(),
                reached.len(),
                self.touched.named_roots.len(),
                self.touched.named_instances.len()
            );
        }
        self.outdated |=
            self.untracked && self.crates.iter().any(|it| self.touched.around.contains(it));
    }

    /// The items [`Self::making`] has, when they are of the revision the database is at.
    fn take_made(&mut self, db: &dyn HirDatabase) {
        let Some(making) = self.making.take() else { return };
        let made = making.lock().unwrap_or_else(|e| e.into_inner()).take();
        let now = salsa::plumbing::current_revision(db);
        let why = match &made {
            None => "not made before the edit",
            Some(_) => "made of another revision",
        };
        self.items = made.filter(|(at, _)| self.of_now(*at, now)).map(|(_, items)| items);
        if self.items.is_none() && std::env::var_os("RA_JIT_STATS").is_some() {
            eprintln!("items: {why}");
        }
    }

    /// Whether what was made at revision `at` is of `now`: the database went from one to the
    /// other by taking in files alone, which the next edit pushes.
    fn of_now(&self, mut at: salsa::Revision, now: salsa::Revision) -> bool {
        while at != now {
            match self.went.iter().find(|(from, _)| *from == at) {
                Some(&(_, to)) => at = to,
                None => return false,
            }
        }
        true
    }

    /// Notes that the database went from revision `from` to `to` by taking in files it did
    /// not have, which the next edit is of.
    pub(super) fn files_added(&mut self, from: salsa::Revision, to: salsa::Revision) {
        self.went.push((from, to));
    }

    /// Once the patch of an edit is made: what the edit asked about refers to now is asked on
    /// threads of their own, of the revision the patch is of, for the next edit to take; with
    /// no items, those are made again, as an edit stopped the threads making them or left
    /// them unable to tell.
    pub(super) fn after_patch(&mut self, db: &dyn HirDatabase, symbols: &dyn SymbolResolver) {
        if self.items.is_none() {
            self.reitem = None;
            return self.make_items(db, symbols);
        }
        let Some(asked) = &self.reitem else { return };
        let (roots, instances) = Settled::asked(asked, &self.compiled, &self.instances);
        let live = self.items.as_ref().map(|it| it.index.live());
        let (send, settling) = std::sync::mpsc::channel();
        let at = salsa::plumbing::current_revision(db);
        let work = move |db: &dyn HirDatabase, fan: &Fan<'_>| {
            let start = std::time::Instant::now();
            let Some(live) = live else { return };
            let settled = Settled::new(db, fan, &roots, &instances, &live);
            // The index is written again at the next edit, which needn't copy what it has.
            drop(live);
            if std::env::var_os("RA_JIT_STATS").is_some() {
                eprintln!(
                    "items-settled: {} roots and {} instances in {} us",
                    roots.len(),
                    instances.len(),
                    start.elapsed().as_micros()
                );
            }
            _ = send.send((at, settled));
        };
        symbols.in_background(db, Box::new(work));
        self.settling = Some(Arc::new(std::sync::Mutex::new(Some(settling))));
        self.write_codes(db, symbols);
    }

    /// Writes the code of the functions of the items whose code is not written yet, on
    /// threads of their own.
    fn write_codes(&mut self, db: &dyn HirDatabase, symbols: &dyn SymbolResolver) {
        let Some(items) = &self.items else { return };
        let wanting = items.index.wanting_code();
        if wanting.1.is_empty() {
            return;
        }
        let (live, coding) = (items.index.live(), Coding::default());
        let codes = coding.clone();
        let work = move |db: &dyn HirDatabase, fan: &Fan<'_>| {
            write_codes(db, fan, wanting, live, &codes);
        };
        symbols.when_idle(db, Box::new(work));
        self.coding = Some(coding);
    }

    fn bindings_edited(&mut self) {
        self.making = None;
        if let Some(items) = &mut self.items {
            items.index.bindings_edited();
        }
    }

    /// Before an edit of `files`: what the edits before it left of the items is taken in, of
    /// the revision they left.
    pub(super) fn before_edit(
        &mut self,
        db: &dyn HirDatabase,
        files: &[FileId],
        fan: &Fan<'_>,
        moves: bool,
    ) {
        let start = std::time::Instant::now();
        self.take_made(db);
        // Nothing changes: what the last edit asked about is taken before one that does.
        if files.is_empty() {
            return;
        }
        let moves = moves && files.len() >= 2;
        self.snapshot(db, files, fan, moves);
        let now = salsa::plumbing::current_revision(db);
        let settled = (self.settling.take().filter(|_| self.items.is_some()))
            .and_then(|it| it.lock().unwrap_or_else(|e| e.into_inner()).take())
            .and_then(|it| it.recv().ok())
            .filter(|(at, _)| self.of_now(*at, now));
        self.went.clear();
        let Some(items) = &mut self.items else { return };
        let coded =
            self.coding.take().and_then(|it| it.lock().unwrap_or_else(|e| e.into_inner()).take());
        if let Some((pushes, funcs, codes)) = coded {
            items.index.take_codes(pushes, &funcs, codes);
        }
        let (reitem, compiled, instances) = (self.reitem.take(), &self.compiled, &self.instances);
        let took = start.elapsed();
        let asked = reitem.clone();
        let settling = probe(|| {
            hir_ty::attach_db(db, || match (reitem, settled) {
                (Some(_), Some((_, settled))) => items.take_settled(settled),
                (Some(asked), None) => items.settle(db, fan, &asked, compiled, instances),
                (None, _) => {}
            })
        });
        // What could not be settled is asked about at every edit until it is: the threads
        // building the last patch, stopped by this edit, can have left what it reads unasked.
        if settling.is_none()
            && let Some(asked) = &asked
        {
            items.forget(asked);
        }
        let settled = start.elapsed();
        let written = probe(|| hir_ty::attach_db(db, || items.index.before_edit(db, files, fan)));
        let rewritten = start.elapsed();
        // What a move out of the files is found by, when the threads writing it were stopped.
        let coded = match written {
            Some(()) if moves => {
                probe(|| hir_ty::attach_db(db, || items.index.write_codes(db, fan, files)))
            }
            _ => Some(0),
        };
        if std::env::var_os("RA_JIT_STATS").is_some() {
            eprintln!(
                "items-before: taken {} us, settled {} us{}, written {} us, {} codes in {} us",
                took.as_micros(),
                (settled - took).as_micros(),
                if settling.is_none() { " (unsettled)" } else { "" },
                (rewritten - settled).as_micros(),
                coded.unwrap_or(0),
                (start.elapsed() - rewritten).as_micros()
            );
        }
        let written = written.zip(coded).map(|_| ());
        if written.is_none() {
            self.items = None;
        }
    }

    /// Whether an edit that moves items between files keeps their code: the base is its own and
    /// its items are made.
    pub(super) fn keeps_moves(&self) -> bool {
        !self.rustc && self.items.is_some()
    }

    /// Whether the items refuse a push of an edit of `files` before it is made: one of them
    /// could not be hashed at the last build.
    pub(super) fn unhashed_in(&self, files: &[FileId]) -> bool {
        (self.items.as_ref()).is_some_and(|it| it.index.unhashed_in(files).is_some())
    }

    /// Before an edit of `files`, of the revision before it: the code of the instances of the
    /// base, or of what it relocated, of functions written in them, for [`Self::relocate`].
    /// Only an edit of two files or more moves a function, and the items tell which it moved.
    fn snapshot(&mut self, db: &dyn HirDatabase, files: &[FileId], fan: &Fan<'_>, moves: bool) {
        let start = std::time::Instant::now();
        self.snapshots.clear();
        let mut wanted: Vec<(Instance, Instance, StoredParamEnvAndCrate, Option<_>)> = Vec::new();
        let moves = self.keeps_moves() && moves;
        let edited: FxHashSet<FileId> = files.iter().copied().collect();
        for (key, it) in &mut self.relocated {
            let code = it.code.take();
            let InstanceDef::Fn(func) = it.now.def else { continue };
            if moves && probe(|| edited.contains(&body_file(db, func.into()))) == Some(true) {
                wanted.push((key.clone(), it.now.clone(), self.instances[key].env.clone(), code));
            }
        }
        if !moves {
            return;
        }
        for (instance, was) in &self.instances {
            let written =
                was.files.as_deref().is_some_and(|it| it.iter().any(|f| edited.contains(f)));
            if !written
                || was.mark.is_none()
                || was.forwarded
                || self.relocated.contains_key(instance)
                || !matches!(instance.def, InstanceDef::Fn(_))
                || instance.args.as_ref().iter().any(|it| it.region().is_none())
            {
                continue;
            }
            wanted.push((instance.clone(), instance.clone(), was.env.clone(), None));
        }
        let found: Vec<std::sync::OnceLock<Option<Arc<CompiledFunction>>>> =
            wanted.iter().map(|_| std::sync::OnceLock::new()).collect();
        let slowest = std::sync::atomic::AtomicU64::new(0);
        let work = |db: &dyn HirDatabase, at: usize| {
            found[at].get_or_init(|| {
                let (_, now, env, code) = &wanted[at];
                if let Some(code) = code {
                    return Some(Arc::clone(code));
                }
                let one = std::time::Instant::now();
                let compiled =
                    || compiled_instance(db, now.clone(), env.clone()).as_ref().ok().cloned();
                let compiled = probe(|| hir_ty::attach_db(db, compiled)).flatten().map(Arc::new);
                let took = one.elapsed().as_micros() as u64;
                slowest.fetch_max(took, std::sync::atomic::Ordering::Relaxed);
                compiled
            });
        };
        fan(wanted.len(), &work);
        (0..wanted.len()).for_each(|at| work(db, at));
        let asked = wanted.len();
        for ((key, now, ..), code) in wanted.into_iter().zip(found) {
            if let (InstanceDef::Fn(func), Some(code)) = (now.def, code.into_inner().flatten()) {
                self.snapshots.insert(func, (key, code));
            }
        }
        if std::env::var_os("RA_JIT_STATS").is_some() {
            eprintln!(
                "relocate-snapshot: {} of {asked} instances in {} us, the slowest {} us",
                self.snapshots.len(),
                start.elapsed().as_micros(),
                slowest.into_inner()
            );
        }
    }

    /// After an edit is pushed through the items, `pushed` when it is: each instance of the base
    /// whose function the edit moved within its crate is the one of where it is now, under
    /// the base's symbol. Its code is that from before the edit when nothing it is of changed
    /// but the items moved, and is compiled otherwise.
    fn relocate(&mut self, db: &dyn HirDatabase, pushed: Option<&blaze_items::Pushed>) {
        use blaze_items::ItemId;
        let snapshots = std::mem::take(&mut self.snapshots);
        let Some(items) = &self.items else {
            self.relocated.clear();
            self.relocated_now.clear();
            self.relocated_funcs.clear();
            return;
        };
        let start = std::time::Instant::now();
        let live = items.index.live();
        let index_moved = items.index.moved();
        let moved: FxHashMap<FunctionId, FunctionId> = (index_moved.iter())
            .filter_map(|it| match it {
                (ItemId::Function(was), ItemId::Function(now)) => Some((*was, *now)),
                _ => None,
            })
            .collect();
        let adts: FxHashMap<AdtId, AdtId> = (index_moved.iter())
            .filter_map(|it| match it {
                (ItemId::Adt(was), ItemId::Adt(now)) => Some((*was, *now)),
                _ => None,
            })
            .collect();
        let named: Vec<Vec<u8>> = (index_moved.values())
            .filter_map(|it| probe(|| item_name(db, *it)).flatten())
            .map(|it| format!("::{it}").into_bytes())
            .collect();
        let asked: Option<FxHashSet<u32>> =
            pushed.map(|it| items.asked(&it.unmoved(index_moved)).instances.into_iter().collect());
        let (mut kept, mut compiled) = (0, 0);
        let mut why: FxHashMap<&'static str, usize> = FxHashMap::default();
        for (was, now) in &moved {
            let Some((key, code)) = snapshots.get(was) else { continue };
            let base = &self.instances[key];
            if key.def == InstanceDef::Fn(*now) || probe(|| now.krate(db)) != Some(base.env.krate) {
                self.relocated.remove(key);
                continue;
            }
            let at = self.instances.get_index_of(key).expect("a key of the base") as u32;
            let renamed = items.index.renamed().contains(&ItemId::Function(*was));
            let unasked = !renamed && asked.as_ref().is_some_and(|it| !it.contains(&at));
            let relocate =
                || hir_ty::attach_db(db, || relocated(db, code, &moved, &adts, &named, &live));
            let code = unasked.then(|| probe(relocate)).flatten().flatten().map(Arc::new);
            if code.is_some() {
                kept += 1;
            } else {
                compiled += 1;
                let reason = match () {
                    _ if renamed => "renamed",
                    _ if asked.is_none() => "refused",
                    _ if !unasked => items.instance_unknown(at).unwrap_or("asked"),
                    _ => "not remapped",
                };
                *why.entry(reason).or_default() += 1;
            }
            let now = Instance { def: InstanceDef::Fn(*now), ..key.clone() };
            self.relocated.insert(key.clone(), Relocated { now, code });
        }
        // A push refused leaves the index as it was, and a patch finds one gone uncalled.
        if pushed.is_some() {
            self.relocated.retain(|_, it| match it.now.def {
                InstanceDef::Fn(func) => {
                    probe(|| live.has(db, ItemId::Function(func))) == Some(true)
                }
                _ => false,
            });
        }
        self.relocated_now =
            self.relocated.iter().map(|(key, it)| (it.now.clone(), key.clone())).collect();
        self.relocated_funcs = (self.relocated.iter())
            .filter_map(|(key, it)| match (key.def, it.now.def) {
                (InstanceDef::Fn(was), InstanceDef::Fn(now)) => Some((now, was)),
                _ => None,
            })
            .collect();
        if std::env::var_os("RA_JIT_STATS").is_some() && !moved.is_empty() {
            eprintln!(
                "relocated: {} instances, {kept} moved now by their code and {compiled} to compile {why:?}, of {} functions moved, in {} us",
                self.relocated.len(),
                moved.len(),
                start.elapsed().as_micros()
            );
        }
    }

    /// The mark of `compiled` with what it calls of [`Self::relocated`] called as the base
    /// does: the base's code that is the same calls the base's symbol, which stands for it.
    fn moved_mark(&self, db: &dyn HirDatabase, compiled: &CompiledFunction) -> Mark {
        let func = |it: FunctionId| self.relocated_funcs.get(&it).copied().unwrap_or(it);
        let mut was = compiled.clone();
        for request in &mut was.requests.funcs {
            if let FuncTarget::Instance(instance, _) = &mut request.target
                && let Some(key) = self.relocated_now.get(instance)
            {
                *instance = key.clone();
            }
        }
        for request in &mut was.requests.datas {
            match request {
                DataRequest::Location(Some((DefWithBodyId::FunctionId(it), _)))
                | DataRequest::Declaration(it) => *it = func(*it),
                _ => {}
            }
        }
        if let Some((DefWithBodyId::FunctionId(it), _)) = &mut was.source {
            *it = func(*it);
        }
        Mark::of_function(db, super::print_of(&was), compiled, self.debug)
    }

    /// Notes that `files` of `crates` changed in their items: what can have changed is what
    /// refers to an item that changed, through its interface, or calls something by a name
    /// bound to another thing now. All of what sees the files when that can't be told.
    pub(super) fn touch_items(
        &mut self,
        db: &dyn HirDatabase,
        crates: &[Crate],
        files: &[FileId],
        fan: &Fan<'_>,
    ) {
        let stats = std::env::var_os("RA_JIT_STATS").is_some();
        let start = std::time::Instant::now();
        let pushed = match &mut self.items {
            Some(items) if !self.rustc => {
                let push = || hir_ty::attach_db(db, || items.index.push(db, files, fan));
                probe(push).unwrap_or(Err("a query that panicked"))
            }
            Some(_) => Err("a base of rustc's"),
            None => Err("no items"),
        };
        let pushed = match pushed {
            Ok(it) => it,
            Err(why) => {
                // What a panic left of the index is of no revision.
                if why == "a query that panicked" {
                    self.items = None;
                }
                if stats {
                    eprintln!("items: whole crates: {why}");
                }
                let refused = start.elapsed();
                self.relocate(db, None);
                self.touch(db, Some(crates));
                if stats {
                    eprintln!(
                        "items-whole-us: refused at {}, all touched at {}",
                        refused.as_micros(),
                        start.elapsed().as_micros()
                    );
                }
                return;
            }
        };
        self.relocate(db, Some(&pushed));
        let items = self.items.as_ref().expect("pushed through them");
        let asked = items.asked(&pushed);
        // Their bodies too, as a body edit's.
        self.touched.bodies.extend(pushed.files.iter().copied());
        for &krate in crates {
            if !self.touched.around.contains(&krate) {
                self.touched.around.extend(krate.transitive_rev_deps(db));
            }
        }
        let around = &self.touched.around;
        let roots =
            asked.roots.iter().copied().filter(|&i| around.contains(&self.crates[i as usize]));
        self.touched.named_roots.extend(roots);
        let instances = asked.instances.iter().copied().filter(|&i| {
            self.instances
                .get_index(i as usize)
                .is_some_and(|(_, was)| around.contains(&was.env.krate))
        });
        self.touched.named_instances.extend(instances);
        // What came from the store is not in `items`: all of it that can see them is asked.
        let hydrated = self.hydrated.iter().enumerate().filter_map(|(i, was)| {
            was.krate.is_none_or(|it| around.contains(&it)).then_some(i as u32)
        });
        self.touched.hydrated.extend(hydrated.collect::<Vec<_>>());
        if stats {
            eprintln!(
                "items: {} written, {} changed, {} names, {} roots and {} instances asked in {} us",
                pushed.written,
                pushed.changed.len(),
                pushed.names(),
                asked.roots.len(),
                asked.instances.len(),
                start.elapsed().as_micros()
            );
        }
        self.reitem = Some(match self.reitem.take() {
            Some(mut was) => {
                was.roots.extend(asked.roots);
                was.instances.extend(asked.instances);
                was
            }
            None => asked,
        });
        self.outdated |=
            self.untracked && self.crates.iter().any(|it| self.touched.around.contains(it));
    }

    /// Notes that a file changed in the bodies of functions alone, none of which the rest of
    /// the program sees into.
    pub(super) fn touch_bodies(&mut self, db: &dyn HirDatabase, file: FileId) {
        self.touched.bodies.insert(file);
        if !self.homed.contains(&file) {
            self.outdate_rustc("a file whose functions were not looked at before it changed");
        }
        self.outdated |= self.untracked
            && base_db::relevant_crates(db, file).iter().any(|it| self.crates.contains(it));
    }

    /// Makes the next build compile what this one has of the checkout from the store, as the
    /// first edit of it would. Whether there was any, that no edit had made the next build do.
    pub(super) fn dehydrate(&mut self) -> bool {
        let fresh = self.untracked && !self.outdated;
        self.outdated |= self.untracked;
        fresh
    }

    /// Takes what every part of [`Base::stale`] found as where the next edit starts from.
    pub(super) fn settle(&mut self, db: &dyn HirDatabase, found: &mut [Stale]) {
        self.their_statics(db, found);
        self.touched.settle(found);
    }

    /// Declines when the functions the base left to rustc reach other statics than they did.
    /// Walking to them is slow, and nothing changes where the walk goes but the code of what
    /// it went through.
    fn their_statics(&mut self, db: &dyn HirDatabase, found: &mut [Stale]) {
        let check_all = std::env::var_os("RA_JIT_CHECK_ALL").is_some();
        let changed = found.iter().any(|it| it.their_walk_changed);
        let Some(first) = found.first_mut() else { return };
        if !changed && !check_all {
            return;
        }
        let start = stdx::time::Instant::now();
        let now = probe(|| TheirStatics::walk(db, self.from_rustc.iter().copied()));
        match now {
            Some((now, walked)) if now == self.their_statics => {
                if changed {
                    self.their_walk =
                        TheirWalk::new(db, &now, walked, &mut self.instances, &mut self.statics);
                }
            }
            _ if !changed => eprintln!("check-all: MISSED statics of the functions left to rustc"),
            _ => first.declined = Some("the statics of a function left to rustc changed"),
        }
        first.steps.left += start.elapsed();
    }

    /// What of the base an edit can have changed, for `parts` threads to look at together.
    pub(super) fn handout(&self, parts: usize) -> Handout<'_> {
        let check_all = std::env::var_os("RA_JIT_CHECK_ALL").is_some();
        Handout {
            parts: parts.max(1),
            check_all,
            roots: self.roots_to_ask(check_all),
            instances: self.instances_to_ask(check_all),
            statics: self.statics.iter().collect(),
            next: Default::default(),
        }
    }

    /// The roots an edit can have changed, or all of them.
    fn roots_to_ask(&self, all: bool) -> Vec<usize> {
        let touched = &self.touched;
        if all || touched.any {
            return (0..self.compiled.len()).collect();
        }
        let homes = &self.homes;
        let of_crates = touched.crates.iter().filter_map(|it| homes.roots_of_crate.get(it));
        let of_files = touched.bodies.iter().filter_map(|it| homes.roots_of_file.get(it));
        let mut asked: Vec<u32> = of_crates.chain(of_files).flatten().copied().collect();
        asked.extend(touched.roots.iter().filter_map(|it| homes.root.get(it)));
        asked.extend(&touched.named_roots);
        if touched.edited() {
            asked.extend(&self.cached_roots);
        }
        sorted(asked)
    }

    /// The instances an edit can have changed, or all of them.
    fn instances_to_ask(&self, all: bool) -> Vec<usize> {
        let touched = &self.touched;
        if all || touched.any {
            return (0..self.instances.len()).collect();
        }
        let homes = &self.homes;
        let of_crates = touched.crates.iter().filter_map(|it| homes.instances_of_crate.get(it));
        let of_files = touched.bodies.iter().filter_map(|it| homes.instances_of_file.get(it));
        let mut asked: Vec<u32> = of_crates.chain(of_files).flatten().copied().collect();
        if !touched.bodies.is_empty() {
            asked.extend(&homes.instances_of_any_file);
        }
        asked.extend(&touched.named_instances);
        if touched.edited() {
            asked.extend(&self.cached_instances);
        }
        asked.extend(
            touched
                .instances
                .iter()
                .filter_map(|it| self.instances.get_index_of(it))
                .map(|it| it as u32),
        );
        sorted(asked)
    }

    /// The code from the store an edit can have changed, or all of it. `edited` names the
    /// files whose bodies changed, `None` when one has no name.
    fn hydrated_to_ask(&self, all: bool, edited: Option<&[String]>) -> Vec<usize> {
        let touched = &self.touched;
        if all || touched.any || edited.is_none() {
            return (0..self.hydrated.len()).collect();
        }
        let homes = &self.hydrated_homes;
        let of_crates = touched.crates.iter().filter_map(|it| homes.of_crate.get(it));
        let of_files = edited.into_iter().flatten().filter_map(|it| homes.of_file.get(&**it));
        let mut asked: Vec<u32> = of_crates.chain(of_files).flatten().copied().collect();
        if !touched.bodies.is_empty() {
            asked.extend(&homes.of_any_file);
        }
        if touched.edited() {
            asked.extend(&homes.of_any_crate);
        }
        asked.extend(&touched.hydrated);
        sorted(asked)
    }

    /// The files edited since the last look, by the names a look names them by, when nothing
    /// changed but bodies that nothing else sees into: code from the store whose items read
    /// as they did when it was compiled is then the code it was, and is not compiled to tell.
    fn bodies_edited(
        &self,
        db: &dyn HirDatabase,
        names: &StableNames<'_, '_>,
    ) -> Option<FxHashMap<String, EditionedFileId>> {
        let touched = &self.touched;
        if touched.any || !touched.crates.is_empty() || touched.bodies.is_empty() {
            return None;
        }
        (touched.bodies.iter())
            .map(|&file| {
                let edition = base_db::relevant_crates(db, file).first()?.data(db).edition;
                Some((names.file_path(file)?, EditionedFileId::new(db, file, edition)))
            })
            .collect()
    }

    /// Looks at what this thread takes from `handout` of the code the base has from the store,
    /// for what an edit changed, comparing what it compiles to now with the mark the store had
    /// of it.
    fn stale_hydrated(
        &self,
        db: &dyn HirDatabase,
        names: &StableNames<'_, '_>,
        handout: &Handout<'_>,
        stale: &mut Stale,
    ) -> std::result::Result<(), &'static str> {
        let check_all = handout.check_all;
        if self.hydrated.is_empty() {
            return Ok(());
        }
        let edited: Option<Vec<String>> =
            self.touched.bodies.iter().map(|&file| names.file_path(file)).collect();
        let edited_paths: FxHashSet<&str> = edited.iter().flatten().map(String::as_str).collect();
        let bodies_edited = self.bodies_edited(db, names);
        let mut homes = super::home::Homes::default();
        let mut found = FoundHydrated::default();
        let asked = self.hydrated_to_ask(check_all, edited.as_deref());
        for i in handout.each(Look::Hydrated, asked.len()).map(|it| asked[it]) {
            let was = &self.hydrated[i];
            let unchanged = !self.touched.hydrated.contains(&(i as u32))
                && bodies_edited.as_ref().is_some_and(|edited| {
                    was.items.as_deref().is_some_and(|items| items_unchanged(db, items, edited))
                });
            if unchanged && !check_all {
                stale.steps.instances_unchanged += 1;
                continue;
            }
            let in_edited = || {
                edited.is_none()
                    || was
                        .files
                        .as_ref()
                        .is_none_or(|files| files.iter().any(|it| edited_paths.contains(&**it)))
            };
            let touched = was.krate.map_or(self.touched.edited(), |it| self.touched.reaches(it))
                || self.touched.hydrated.contains(&(i as u32))
                || !self.touched.bodies.is_empty() && in_edited();
            if !touched && !check_all {
                continue;
            }
            let asked = stdx::time::Instant::now();
            let instance =
                self.found_instance(db, names, &mut homes, &mut found, i, &mut Vec::new());
            let same = instance.clone().and_then(|(instance, env)| {
                probe(|| {
                    let compiled = compiled_instance(db, instance, env).as_ref().ok()?;
                    Some(stable_mark(db, names, compiled, self.debug) == Some(was.mark))
                })
            });
            stale.steps.instances_asked += 1;
            stale.steps.slowest = stale.steps.slowest.max(asked.elapsed());
            match (same, instance) {
                (Some(Some(true)), _) => {}
                _ if !touched => eprintln!("check-all: MISSED `{}` from the store", was.name),
                (None, _)
                    if unchanged
                        && self.dropped(db, names, &mut homes, &mut found, i, &mut Vec::new()) => {}
                _ if unchanged => {
                    eprintln!(
                        "check-all: MISSED `{}` from the store, its items unchanged",
                        was.name
                    )
                }
                (Some(Some(false)), Some((instance, env))) => {
                    stale.hydrated.push((i as u32, instance, env))
                }
                (Some(_), _) => {
                    return Err("code of the checkout from the store no longer compiles");
                }
                // Its callers changed, and what they call now is compiled as new code.
                (None, _)
                    if self.dropped(db, names, &mut homes, &mut found, i, &mut Vec::new()) => {}
                (None, _) => {
                    eprintln!("patch: nothing calls `{}` from the store as it did", was.name);
                    return Err("code of the checkout from the store is not called as it was");
                }
            }
        }
        Ok(())
    }

    /// What the code from the store at `i` is an instance of, from the code of what calls it
    /// now. `None` when nothing that called it does any more.
    fn found_instance(
        &self,
        db: &dyn HirDatabase,
        names: &StableNames<'_, '_>,
        homes: &mut super::home::Homes,
        found: &mut FoundHydrated,
        i: usize,
        asking: &mut Vec<usize>,
    ) -> Option<(Instance, StoredParamEnvAndCrate)> {
        let was = &self.hydrated[i];
        if let Some(found) = found.instances.get(&i) {
            return found.clone();
        }
        // What only the build's own code calls, by nothing a caller says: a vtable's slot.
        if was.callers.is_empty() {
            return was.known.clone();
        }
        if asking.contains(&i) {
            return None;
        }
        asking.push(i);
        let mut instance = None;
        for caller in &was.callers {
            let Some(compiled) = self.caller_code(db, names, homes, found, caller, asking) else {
                continue;
            };
            instance = compiled.requests.funcs.iter().find_map(|request| {
                let FuncTarget::Instance(callee, env) = &request.target else { return None };
                let named = names.instance(callee)?;
                (*named == *was.name).then(|| (callee.clone(), homes.home(db, callee, env)))
            });
            if instance.is_some() {
                break;
            }
        }
        asking.pop();
        // One not found while a caller is being looked for can be found once that is.
        if instance.is_some() || asking.is_empty() {
            found.instances.insert(i, instance.clone());
        }
        instance
    }

    /// The code of a caller of one of [`Self::hydrated`] as it compiles now.
    fn caller_code<'q>(
        &self,
        db: &'q dyn HirDatabase,
        names: &StableNames<'_, '_>,
        homes: &mut super::home::Homes,
        found: &mut FoundHydrated,
        caller: &Caller,
        asking: &mut Vec<usize>,
    ) -> Option<&'q CompiledFunction> {
        let compiled = match caller {
            Caller::Root(at) => {
                let root = self.compiled[*at as usize].0;
                probe(|| compiled_root(db, root).as_ref().ok())
            }
            Caller::Base(Node::Root(root)) => probe(|| compiled_root(db, *root).as_ref().ok()),
            Caller::Base(Node::Instance(instance)) => {
                let env = self.instances.get(instance)?.env.clone();
                probe(|| compiled_instance(db, instance.clone(), env).as_ref().ok())
            }
            Caller::Base(Node::Static(_)) => None,
            Caller::Hydrated(at) => {
                let (instance, env) =
                    self.found_instance(db, names, homes, found, *at as usize, asking)?;
                probe(|| compiled_instance(db, instance, env).as_ref().ok())
            }
        };
        compiled.flatten()
    }

    /// Whether nothing calls one of [`Self::hydrated`] as the base did: no caller calls it by
    /// its name any more, so each changed, and a caller from the store that is not found is
    /// left uncalled itself. A closure is named by its place among the closures of its body
    /// and by its source, so a closure added above it in that body, or an edit of it, gives it
    /// and what is generic over it new names, and the code under the old ones is left
    /// uncalled, as an instance no caller reaches any more is.
    /// Each changed caller is compiled again, and what it calls now with it.
    fn dropped(
        &self,
        db: &dyn HirDatabase,
        names: &StableNames<'_, '_>,
        homes: &mut super::home::Homes,
        found: &mut FoundHydrated,
        i: usize,
        asking: &mut Vec<usize>,
    ) -> bool {
        let was = &self.hydrated[i];
        if was.callers.is_empty() {
            return false;
        }
        // Code that only calls itself, as a recursive sort does, is left uncalled with it.
        if asking.contains(&i) {
            return true;
        }
        if let Some(&dropped) = found.dropped.get(&i) {
            return dropped;
        }
        asking.push(i);
        let dropped = was.callers.iter().all(|caller| match *caller {
            Caller::Hydrated(at) => {
                let at = at as usize;
                self.found_instance(db, names, homes, found, at, &mut Vec::new()).is_some()
                    || self.dropped(db, names, homes, found, at, asking)
            }
            Caller::Root(_) | Caller::Base(_) => true,
        });
        asking.pop();
        // What took a caller still being looked at as uncalled is known only once that is.
        if !dropped || asking.is_empty() {
            found.dropped.insert(i, dropped);
        }
        dropped
    }

    /// What of the base this thread takes from `handout` no longer is what it was. Looking is
    /// most of the work of a patch, and threads look at it together.
    /// `symbols` names what the base has from the store, which nothing else tells an edit of.
    pub(super) fn stale(
        &self,
        db: &dyn HirDatabase,
        symbols: Option<&dyn SymbolResolver>,
        handout: &Handout<'_>,
    ) -> Stale {
        let start = stdx::time::Instant::now();
        let mut stale = Stale::default();
        let names = symbols.filter(|_| !self.ids.is_empty()).map(|symbols| {
            StableNames::new(db, symbols, self.owned.clone(), true).frozen(self.ids.clone())
        });
        if names.is_none() && (!self.stored_marks.is_empty() || !self.hydrated.is_empty()) {
            return stale.declined("code from the store, and nothing to name it by");
        }
        // A check of the skipping below: looks at what no edit can have changed too, and says
        // so if one did.
        let check_all = handout.check_all;
        for i in handout.each(Look::Rejected, self.rejected.len()) {
            let (instance, env) = &self.rejected[i];
            let compiles = probe(|| compiled_instance(db, instance.clone(), env.clone()).is_ok());
            if compiles != Some(false) {
                return stale.declined("an instance that did not compile is another now");
            }
        }
        let bodies_edited = names.as_ref().and_then(|names| self.bodies_edited(db, names));
        for i in handout.each(Look::Roots, handout.roots.len()).map(|it| handout.roots[it]) {
            let root = self.compiled[i].0;
            let touched = self.touched.reaches(self.crates[i])
                || self.touched.reaches_bodies(Some(&[self.files[i]]))
                || self.touched.roots.contains(&root)
                || self.touched.named_roots.contains(&(i as u32))
                || self.touched.edited() && self.cached_roots.binary_search(&(i as u32)).is_ok();
            if !touched && !check_all {
                continue;
            }
            let unchanged = !self.touched.roots.contains(&root)
                && bodies_edited.as_ref().is_some_and(|edited| {
                    let items = self.stored_items.get(&(i as u32));
                    items.is_some_and(|items| items_unchanged(db, items, edited))
                });
            if unchanged && !check_all {
                stale.steps.roots_unchanged += 1;
                continue;
            }
            let asked = stdx::time::Instant::now();
            let same = match (self.stored_marks.get(&(i as u32)), &names) {
                (Some(&was), Some(names)) => probe(|| {
                    let compiled = compiled_root(db, root).as_ref().ok()?;
                    Some(stable_mark(db, names, compiled, self.debug) == Some(was))
                }),
                _ => probe(|| {
                    let now = Mark::of_root(db, root, self.debug)?;
                    if now == self.marks[i] || self.relocated.is_empty() {
                        return Some(now == self.marks[i]);
                    }
                    let compiled = compiled_root(db, root).as_ref().ok()?;
                    Some(self.moved_mark(db, compiled) == self.marks[i])
                }),
            };
            stale.steps.roots_asked += 1;
            stale.steps.asked(asked.elapsed());
            match same {
                Some(Some(true)) => {}
                _ if !touched || unchanged => {
                    let (crate_of, name) = (self.crates[i], &self.compiled[i].1);
                    eprintln!("check-all: MISSED root {root:?} `{name}` of {crate_of:?}");
                    continue;
                }
                Some(Some(false)) => stale.roots.push(root),
                Some(None) => return stale.declined("a root of the base no longer compiles"),
                None => stale.gone.push(root),
            }
        }
        for (root, was, file) in
            handout.each(Look::Unnamed, self.unnamed.len()).map(|it| &self.unnamed[it])
        {
            let touched = probe(|| root.krate(db)).is_none_or(|it| self.touched.reaches_around(it))
                || self.touched.reaches_bodies(Some(&[*file]))
                || self.touched.roots.contains(root);
            if !touched {
                continue;
            }
            let mark = probe(|| Mark::of_root(db, *root, self.debug));
            let Some(mark) = mark else {
                stale.gone.push(*root);
                continue;
            };
            // One that did not compile then can't be told to be the same.
            if !was.zip(mark).is_some_and(|(was, now)| was.same_code(now)) {
                return stale.declined("a function that rustc's symbols name no copy of changed");
            }
        }
        for homed in handout.each(Look::Closures, self.closures.len()).map(|it| &self.closures[it])
        {
            let krate = probe(|| homed.root.krate(db));
            let touched = krate.is_none_or(|it| self.touched.reaches_around(it))
                || self.touched.reaches_bodies(Some(&[homed.file]))
                || self.touched.roots.contains(&homed.root);
            if !touched {
                continue;
            }
            let now = probe(|| {
                super::closures_of(db, homed.root).map(|it| it.into_iter().map(|it| it.1).collect())
            });
            let Some(now) = now else {
                stale.gone.push(homed.root);
                continue;
            };
            // An `async` block has no mark here to tell a change by, but where and how the
            // root is written.
            let asynchronous = homed.closures.iter().any(Option::is_none);
            let rewritten = || {
                homed.written.is_none()
                    || probe(|| super::written_print(db, homed.root)).flatten() != homed.written
            };
            if now.as_ref() != Some(&homed.closures) || (asynchronous && rewritten()) {
                return stale.declined("a closure in what rustc linked is another now");
            }
            let env = closure_env(db, homed.root);
            for (instance, was) in &homed.unnamed {
                let mark = probe(|| closure_mark(db, instance, &env, self.debug));
                if !was.zip(mark.flatten()).is_some_and(|(was, now)| was.same_code(now)) {
                    return stale
                        .declined("a closure that rustc's symbols name no copy of changed");
                }
            }
        }
        stale.steps.roots = start.elapsed();
        let instances = &handout.instances;
        for i in handout.each(Look::Instances, instances.len()).map(|it| instances[it]) {
            let Some((instance, was)) = self.instances.get_index(i) else { continue };
            // What it is now is the patch's to tell.
            if self.relocated.contains_key(instance) {
                continue;
            }
            let touched = self.touched.reaches(was.env.krate)
                || self.touched.reaches_bodies(was.files.as_deref())
                || self.touched.instances.contains(instance)
                || self.touched.named_instances.contains(&(i as u32))
                || self.touched.edited()
                    && self.cached_instances.binary_search(&(i as u32)).is_ok();
            if !touched && !check_all {
                continue;
            }
            let asked = stdx::time::Instant::now();
            let mut mark = probe(|| Mark::of_instance(db, instance, &was.env, self.debug));
            if !self.relocated.is_empty()
                && let Some(Some(now)) = mark
                && Some(now) != was.mark
            {
                let moved = probe(|| {
                    let compiled = compiled_instance(db, instance.clone(), was.env.clone());
                    compiled.as_ref().ok().map(|it| self.moved_mark(db, it))
                });
                if let Some(Some(moved)) = moved
                    && Some(moved) == was.mark
                {
                    mark = Some(Some(moved));
                }
            }
            stale.steps.instances_asked += 1;
            stale.steps.asked(asked.elapsed());
            stale.their_walk_changed |= touched
                && was.theirs
                && mark.flatten().map(|it| it.serial) != was.mark.map(|it| it.serial);
            match mark {
                Some(mark) if mark == was.mark => {}
                _ if !touched => {
                    eprintln!("check-all: MISSED instance {instance:?} of {:?}", was.env.krate);
                    continue;
                }
                Some(Some(_)) if !was.forwarded => stale.instances.push(instance.clone()),
                // One that had no code of its own might have now.
                _ => stale.broken.push((
                    Node::Instance(instance.clone()),
                    "an instance of the base gained or lost code of its own",
                )),
            }
        }
        if let Some(names) = &names
            && let Err(why) = self.stale_hydrated(db, names, handout, &mut stale)
        {
            return stale.declined(why);
        }
        stale.steps.instances = start.elapsed() - stale.steps.roots;
        let statics = &handout.statics;
        for (&id, was) in handout.each(Look::Statics, statics.len()).map(|it| statics[it]) {
            let touched = self.touched.reaches_around(was.krate)
                || self.touched.reaches_bodies(Some(&[was.file]))
                || self.touched.statics.contains(&id);
            if !touched && !check_all {
                continue;
            }
            let mark = probe(|| {
                let compiled = compiled_static(db, id).as_ref().ok()?;
                Some(Mark::new(db, static_serial(db, id), &compiled.requests))
            });
            if mark == Some(Some(was.mark)) {
                continue;
            }
            if !touched {
                eprintln!("check-all: MISSED static {id:?} of {:?}", was.krate);
                continue;
            }
            // rustc's code reads rustc's copy, which a patch has no copy of its own beside.
            if self.rustc {
                return stale.declined("a static changed, and rustc's copy is the program's");
            }
            let writable = probe(|| compiled_static(db, id).as_ref().ok().map(|it| it.writable));
            // rustc's copy holds what the program wrote, from the first value it had: one of
            // ours beside it would split the state, and it can't start again from another. A
            // static the edit took away (or left with no value) has no copy of ours to split
            // it with: nothing compiled now names it.
            if was.shared && matches!(writable, Some(Some(_))) {
                return stale.declined(
                    "the first value changed of a static with state that rustc's code shares",
                );
            }
            stale.their_walk_changed |= was.theirs;
            // rustc's code has rustc's copy, and a thread's copy is laid out by the linker.
            if writable == Some(Some(was.writable)) && !was.theirs && !is_thread_local(db, id) {
                stale.statics.push(id);
            } else {
                stale.broken.push((Node::Static(id), "a static of the base changed"));
            }
        }
        stale.steps.statics = start.elapsed() - stale.steps.roots - stale.steps.instances;
        for i in handout.each(Look::Skipped, self.skipped.len()) {
            let Some(was) = &self.left[i] else { continue };
            let root = self.skipped[i].0;
            let at = probe(|| (root.krate(db), body_file(db, root.into())));
            let touched = at.is_none_or(|(krate, file)| {
                self.touched.reaches_around(krate) || self.touched.reaches_bodies(Some(&[file]))
            }) || self.touched.roots.contains(&root);
            if !touched && !check_all {
                continue;
            }
            let Some(now) = probe(|| Left::of(db, root)) else {
                stale.gone.push(root);
                continue;
            };
            // Without code to tell a change by, any edit that reaches it may be one.
            if was.code.is_none() && touched || now != *was {
                if !touched {
                    eprintln!("check-all: MISSED function left to rustc {root:?}");
                    continue;
                }
                return stale.declined("a function left to rustc changed");
            }
        }
        for i in handout.each(Look::TheirInstances, self.their_walk.instances.len()) {
            let was = &self.their_walk.instances[i];
            if !self.touched.reaches_around(was.env.krate)
                && !self.touched.reaches_bodies(was.files.as_deref())
            {
                continue;
            }
            let serial = probe(|| {
                let compiled = compiled_instance(db, was.instance.clone(), was.env.clone());
                compiled
                    .as_ref()
                    .ok()
                    .map(|_| instance_serial(db, was.instance.clone(), was.env.clone()))
            });
            stale.their_walk_changed |= serial != Some(was.serial);
        }
        for i in handout.each(Look::TheirStatics, self.their_walk.statics.len()) {
            let was = &self.their_walk.statics[i];
            if !self.touched.reaches_around(was.krate)
                && !self.touched.reaches_bodies(Some(&[was.file]))
            {
                continue;
            }
            let serial = probe(|| {
                compiled_static(db, was.id).as_ref().ok().map(|_| static_serial(db, was.id))
            });
            stale.their_walk_changed |= serial != Some(was.serial);
        }
        stale.steps.left =
            start.elapsed() - stale.steps.roots - stale.steps.instances - stale.steps.statics;
        stale
    }

    /// Whether a root still refers to the node, directly or through others.
    fn reaches(&self, db: &dyn HirDatabase, node: &Node, unchanged: &Unchanged) -> bool {
        let mut seen = FxHashSet::default();
        let mut stack = vec![node];
        while let Some(node) = stack.pop() {
            if self.reached_from_store.contains(node) {
                return true;
            }
            let parents = match node {
                Node::Root(_) => return true,
                Node::Instance(instance) => self.instances.get(instance).map(|it| &it.parents),
                Node::Static(id) => self.statics.get(id).map(|it| &it.parents),
            };
            for parent in parents.into_iter().flatten() {
                if !seen.contains(parent) && self.still_asks_for(db, parent, node, unchanged) {
                    seen.insert(parent);
                    stack.push(parent);
                }
            }
        }
        false
    }

    /// Whether what referred to the node in the base does so after the edit.
    fn still_asks_for(
        &self,
        db: &dyn HirDatabase,
        parent: &Node,
        node: &Node,
        unchanged: &Unchanged,
    ) -> bool {
        // A broken parent that is reached stops the patch by itself.
        if unchanged.broken.contains(parent) {
            return false;
        }
        if matches!(parent, Node::Root(root) if unchanged.gone.contains(root)) {
            return false;
        }
        if !unchanged.changed.contains(parent) {
            return true;
        }
        let requests = match parent {
            Node::Root(root) => compiled_root(db, *root).as_ref().ok().map(|it| &it.requests),
            Node::Instance(instance) => self.instances.get(instance).and_then(|known| {
                let compiled = compiled_instance(db, instance.clone(), known.env.clone());
                compiled.as_ref().ok().map(|it| &it.requests)
            }),
            Node::Static(id) => compiled_static(db, *id).as_ref().ok().map(|it| &it.requests),
        };
        requests.is_some_and(|requests| node.is_asked_for_by(requests))
    }

    /// The futures that functions left to rustc handle, or `None` when that can be any.
    fn their_async_blocks(&self) -> Option<Option<FxHashSet<salsa::Id>>> {
        let mut theirs = Some(FxHashSet::default());
        let mut any = false;
        for left in self.left.iter().flatten() {
            any = true;
            theirs = theirs.zip(left.async_blocks.as_ref()).map(|(mut all, blocks)| {
                all.extend(blocks);
                all
            });
        }
        any.then_some(theirs)
    }

    /// An object with what changed since the base was built, or why the change is more than
    /// a patch can hold.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn patch(
        &self,
        db: &dyn HirDatabase,
        symbols: &dyn SymbolResolver,
        roots: Roots<'_>,
        on_demand: &FxHashSet<FunctionId>,
        denied: &FxHashSet<FunctionId>,
        known: &Rejections,
        own_statics: OwnStatics,
        stale: Vec<Stale>,
    ) -> Result<std::result::Result<Patch, &'static str>> {
        let start = stdx::time::Instant::now();
        // A function added where every function is compiled whether called or not, as all of
        // a program's are when its `main` is C's, is compiled into the patch with the rest.
        let added: FxHashSet<FunctionId> = match self.rustc {
            true => FxHashSet::default(),
            false => {
                let base: FxHashSet<FunctionId> = self.entries.iter().copied().collect();
                let now: FxHashSet<FunctionId> = roots.entries.iter().copied().collect();
                if !base.is_subset(&now) {
                    return Ok(Err("a root compiled whether called or not is gone"));
                }
                now.difference(&base).copied().collect()
            }
        };
        // Every function is in rustc's executable: one a patch calls is called there, and only
        // what changed is compiled.
        let none = FxHashSet::default();
        let on_demand = if self.rustc { &none } else { on_demand };
        let all: FxHashSet<FunctionId> =
            roots.entries.iter().chain(roots.on_demand).copied().collect();
        let known_roots =
            || self.compiled.iter().map(|it| it.0).chain(self.skipped.iter().map(|it| it.0));
        // One the look found gone is one the program no longer lists, and the other way round.
        let gone: FxHashSet<FunctionId> =
            stale.iter().flat_map(|it| it.gone.iter().copied()).collect();
        if gone.iter().any(|root| all.contains(root)) {
            return Ok(Err("a root of the base no longer compiles"));
        }
        if !known_roots().all(|root| all.contains(&root) || gone.contains(&root)) {
            return Ok(Err("a root of the base is gone"));
        }

        let mut changed_roots = Vec::new();
        let mut changed_instances = Vec::new();
        let mut changed_hydrated = Vec::new();
        let mut changed_statics = Vec::new();
        let mut broken = Vec::new();
        for part in stale {
            if let Some(why) = part.declined {
                return Ok(Err(why));
            }
            changed_roots.extend(part.roots);
            changed_instances.extend(part.instances);
            changed_hydrated.extend(part.hydrated);
            changed_statics.extend(part.statics);
            broken.extend(part.broken);
        }
        // A function at a level above 1 differs from the base, whose code is Cranelift's.
        for node in known.tiers.nodes() {
            match &node.0 {
                super::tier::Kind::Root(root) => {
                    if self.homes.root.contains_key(root)
                        && !changed_roots.contains(root)
                        && !gone.contains(root)
                    {
                        changed_roots.push(*root);
                    }
                }
                super::tier::Kind::Instance(instance, env) => {
                    let known = self.instances.get(instance);
                    if known.is_some_and(|it| it.env == *env && it.mark.is_some() && !it.forwarded)
                        && !changed_instances.contains(instance)
                        && !self.relocated.contains_key(instance)
                    {
                        changed_instances.push(instance.clone());
                    }
                }
            }
        }
        // What the edit took away is still in the base, where nothing refers to it any more.
        let unchanged = Unchanged {
            changed: changed_roots
                .iter()
                .map(|&root| Node::Root(root))
                .chain(changed_instances.iter().cloned().map(Node::Instance))
                .chain(changed_statics.iter().map(|&id| Node::Static(id)))
                .collect(),
            broken: broken.iter().map(|it| it.0.clone()).collect(),
            gone,
        };
        // Nothing tells what in rustc's code refers to what.
        if let Some((_, why)) =
            broken.iter().find(|it| self.rustc || self.reaches(db, &it.0, &unchanged))
        {
            return Ok(Err(why));
        }
        // rustc's code may call any of them still.
        if !self.rustc {
            changed_instances
                .retain(|it| self.reaches(db, &Node::Instance(it.clone()), &unchanged));
            changed_statics.retain(|&it| self.reaches(db, &Node::Static(it), &unchanged));
        }
        let verify = start.elapsed();

        let mut cg = ObjectCodegen::new(
            db,
            symbols,
            &known.rejected,
            own_statics,
            on_demand,
            Some(self),
            false,
        )?;
        cg.phases.compile = verify;
        cg.phases.setup = start.elapsed() - verify;
        cg.rustc_futures = known.futures.clone();
        cg.rustc_values = known.frozen.rustc_values();
        cg.tiers = Some(&known.tiers);
        // What nothing reached any more can be reached again by the code compiled now.
        cg.replaced = unchanged;
        // rustc's copies are the ones the base was built against: what they call is in it.
        for instance in &changed_instances {
            let was = &self.instances[instance];
            let Ok(compiled) = compiled_instance(db, instance.clone(), was.env.clone()).as_ref()
            else {
                return Ok(Err("an instance of the base no longer compiles"));
            };
            let id = cg.module.declare_function(
                &was.symbol,
                Linkage::Hidden,
                &compiled.code.signature,
            )?;
            cg.instances.insert(instance.clone(), id);
            cg.pending.push((instance.clone(), was.env.clone(), id));
        }
        // What a moved function's instance is called by is the base's symbol, which the patch
        // defines again when the code under it is other code or tells of other places.
        let mut relocated = 0;
        let check_all = std::env::var_os("RA_JIT_CHECK_ALL").is_some();
        let compiling: Vec<(&Instance, &StoredParamEnvAndCrate)> = (self.relocated.iter())
            .filter(|(_, it)| it.code.is_none() || check_all)
            .map(|(key, it)| (&it.now, &self.instances[key].env))
            .collect();
        symbols.in_parallel(compiling.len(), &|db, _, at| {
            let (now, env) = compiling[at];
            probe(|| compiled_instance(db, now.clone(), env.clone()).is_ok());
        });
        for (key, it) in &self.relocated {
            let was = &self.instances[key];
            let compiled = || compiled_instance(db, it.now.clone(), was.env.clone()).as_ref();
            // A check of the items: what the code from before is taken for is what compiles.
            if check_all
                && let Some(code) = &it.code
                && probe(compiled).is_some_and(|now| !matches!(now, Ok(now) if now == &**code))
            {
                eprintln!("check-all: MISSED relocated {:?} as {:?}", key, it.now);
            }
            let code: &CompiledFunction = match &it.code {
                Some(code) => code,
                None => match probe(compiled) {
                    Some(Ok(code)) => code,
                    Some(Err(_)) => return Ok(Err("a function moved no longer compiles")),
                    // Gone since, with nothing that calls it.
                    None => continue,
                },
            };
            if Some(self.moved_mark(db, code)) == was.mark {
                let id = cg.import(&was.symbol, &code.code.signature)?;
                cg.instances.insert(it.now.clone(), id);
                continue;
            }
            relocated += 1;
            let id =
                cg.module.declare_function(&was.symbol, Linkage::Hidden, &code.code.signature)?;
            cg.instances.insert(it.now.clone(), id);
            if let Some(code) = &it.code {
                cg.given.insert(it.now.clone(), Arc::clone(code));
            }
            cg.pending.push((it.now.clone(), was.env.clone(), id));
        }
        // What calls code from the store calls it by the base's symbol, which the patch takes.
        for (i, instance, env) in changed_hydrated {
            let was = &self.hydrated[i as usize];
            let Ok(compiled) = compiled_instance(db, instance.clone(), env.clone()).as_ref() else {
                return Ok(Err("code of the checkout from the store no longer compiles"));
            };
            if cg.instances.contains_key(&instance) {
                continue;
            }
            let id = cg.module.declare_function(
                &was.symbol,
                Linkage::Hidden,
                &compiled.code.signature,
            )?;
            cg.instances.insert(instance.clone(), id);
            cg.pending.push((instance, env, id));
        }
        for &id in &changed_statics {
            match cg.static_data(id) {
                Ok(_) => {}
                Err(e @ CodegenError::Refused(_)) => return Err(e),
                Err(_) => return Ok(Err("a static of the base no longer has a value")),
            }
        }
        // rustc's callers of one that hands out a value of a type only its body names go on
        // calling or polling rustc's code for that type, which is of the source before.
        if self.rustc && changed_roots.iter().any(|&root| super::hands_out_opaque(db, root)) {
            return Ok(Err("a function that hands rustc's code an `impl Trait` changed or moved"));
        }
        let mut queued: FxHashSet<FunctionId> = known_roots().collect();
        let mut queue = changed_roots;
        queue.extend(added.iter().copied().filter(|&func| queued.insert(func)));
        queue.extend(cg.wanted.drain(..).filter(|&func| queued.insert(func)));
        let mut next = 0;
        loop {
            while let Some(&root) = queue.get(next) {
                next += 1;
                if denied.contains(&root) || known.left.contains(&root) {
                    return Ok(Err("a root can't be compiled"));
                }
                match cg.define_function(root) {
                    Ok(_) => {}
                    // As in a full build, a function added to a crate whose symbols are
                    // rustc's has none to be put in the object under: what calls it has a
                    // copy of its own.
                    Err(e) if e.no_symbol() && added.contains(&root) => {}
                    Err(e @ CodegenError::Refused(_)) => return Err(e),
                    Err(_) => return Ok(Err("a root can't be compiled")),
                }
                queue.extend(cg.wanted.drain(..).filter(|&func| queued.insert(func)));
            }
            let failed = cg.define_instances();
            if let Some(refused) = cg.refused.take() {
                return Err(refused);
            }
            if !failed.is_empty() {
                return Ok(Err("an instance can't be compiled"));
            }
            queue.extend(cg.wanted.drain(..).filter(|&func| queued.insert(func)));
            if next == queue.len() {
                break;
            }
        }
        // A future is laid out by one compiler: a function that now handles one of rustc's
        // has to be left to rustc as well.
        if let Some(theirs) = self.their_async_blocks() {
            let shares = |root| match (async_blocks(db, root).clone(), &theirs) {
                (Some(ours), Some(theirs)) => !ours.is_disjoint(theirs),
                (Some(ours), None) => !ours.is_empty(),
                (None, _) => true,
            };
            if queue.iter().copied().any(shares) {
                return Ok(Err("a function now shares an `async` block with one left to rustc"));
            }
        }
        if std::env::var_os("RA_JIT_STATS").is_some() {
            let known: FxHashSet<FunctionId> = known_roots().collect();
            let new = queue.iter().filter(|it| !known.contains(it)).count();
            eprintln!(
                "patch-made: {} roots changed and {new} new, {} gone; {} instances, {} of them of the base and {relocated} relocated of {}",
                queue.len() - new,
                cg.replaced.gone.len(),
                cg.instances.len(),
                changed_instances.len(),
                self.relocated.len()
            );
            let decls = cg.module.declarations();
            let anew = (cg.instances.values())
                .filter(|&&id| {
                    decls.get_function_decl(id).linkage_name(id).starts_with("ra-jit.patch::")
                })
                .count();
            eprintln!("patch: {anew} instances the base has no code for");
        }
        let (bodies, mut phases) = (cg.bodies, cg.phases);
        let start = stdx::time::Instant::now();
        let (object, _) = cg.finish()?;
        phases.object = start.elapsed();
        Ok(Ok(Patch { object, bodies, phases }))
    }
}

#[cfg(test)]
mod tests {
    use super::probe;

    #[test]
    fn a_program_started_again_keeps_what_rustcs_copies_are_of() {
        let file = base_db::FileId::from_raw(7);
        let mut known = crate::Rejections { patching: true, full: true, ..Default::default() };
        known.frozen.edit_bodies(file);
        known.start_again();
        assert!(known.patching && !known.full);
        assert!(known.frozen.edited_bodies.contains(&file));
    }

    #[test]
    fn a_probe_that_is_interrupted_goes_on_unwinding_and_one_that_panics_finds_nothing() {
        let gone: Option<()> = probe(|| panic!("the base no longer has it"));
        assert_eq!(gone, None);
        assert_eq!(probe(|| 7), Some(7));
        let waited: Option<()> =
            probe(|| std::panic::resume_unwind(Box::new(salsa::Cancelled::PropagatedPanic)));
        assert_eq!(waited, None);
        let interrupted = std::panic::catch_unwind(|| {
            probe(|| std::panic::resume_unwind(Box::new(salsa::Cancelled::PendingWrite)))
        });
        let stopped: Box<dyn std::any::Any + Send> = interrupted.unwrap_err();
        assert!(stopped.is::<salsa::Cancelled>());
    }
}
