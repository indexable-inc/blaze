//! A live build that compiles lazily (`blaze-lazy`): the walk describes every instance as a
//! whole build does, and every one that is not compiled ahead passes the same checks a whole
//! build's compile would put it through before the run starts, so a lazy build fails where a
//! whole build fails. Only the entry and the functions an initializer or finalizer section
//! names are compiled ahead; the rest wait for their first call, which [`Later`] (the walk,
//! owned past its build: [`Build::into_later`]) compiles on the run's own threads.
//!
//! A function's checks are its front end's: describing it (its MIR monomorphized, its
//! constants evaluated) and building its IR (its layouts). A function whose template the store
//! has passed them in the build that kept it, under the same key, so the store's bytes are
//! read and its IR is not built again. A function whose IR is built for its check keeps its
//! shape, so its first call takes the memo's template without building it again. What only the
//! backend's emission finds (a backend's own failure, never a program's) is found on the
//! function's first call, or by the lazy runner's idle compiles, and named then.
//!
//! A first call compiles against the front end as it stands then, which may be a later
//! revision than the image's: exact while the front end still describes the function with the
//! code and arguments of its key, and refused, naming both, when it doesn't. The store's and
//! the memo's templates are content-keyed, so only IR built at the caller's revision is checked.
//! A query engine's cancellation unwinds through it uncaught.
//!
//! Each compile a store missed is one build's of the memo ([`crate::flight`]), by its stored
//! key, as a whole build's are: two runs that first call one function compile it once.
//!
//! Folding (#1292) is a whole build's: a lazy function is compiled alone, as its own template.

use blaze_cas::Kind;
use blaze_graph::par;
use blaze_hash::{HashMap, HashSet};
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use blaze_mem::{Tag, frame};

use super::{
    Build, Built, FnNode, FrontInstance, FrontStatic, Lookup, Node, Out, Put, Sorted, StaticId,
    StaticNode, Whence, callee, error, placed, stopped, stored, stored_function, unfit,
    unkept_inline, unkept_message,
};
use crate::{
    Backend, CodegenError, CompiledFunction, Instance, InstanceKey, Unit,
    backend::{Frontend, Prepare},
    blob,
    flight::Asked,
    memo::Memo,
    plan::{Mode, Origin},
    query::keys,
    settings::Spec,
    template::Template,
};

/// A function of a lazy build, as its walk reached it: what `blaze_lazy::plan::Plan` is made
/// of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Walked {
    pub key: InstanceKey,
    pub code: CodeHash,
    pub item: ItemKey,
}

/// Whether a static in `section` runs code at the start or the end of the process or of a
/// thread: an initializer or finalizer array, as each object format names it. The functions
/// such a static names are compiled ahead.
pub fn runs_at_start(section: &str) -> bool {
    let elf = |name: &str| section == name || section.strip_prefix(name).is_some_and(|it| it.starts_with('.'));
    elf(".init_array")
        || elf(".preinit_array")
        || elf(".fini_array")
        || elf(".ctors")
        || elf(".dtors")
        || section.contains("__mod_init_func")
        || section.contains("__mod_term_func")
        || section.starts_with(".CRT$X")
}

/// A lazy build's walk, owned: every function it reached with its places, and the shape each
/// check recorded. It names no backend, front end or database (`I` and `S` are the front
/// end's instance and static), so a run's threads keep it for the run, and each compile is
/// given the backend, the front end, the memo and the store it reads.
pub struct Later<I, S> {
    pub(super) nodes: Sorted<InstanceKey, FnNode<I, S>>,
    /// The shape of each function whose IR was built for its check.
    pub(super) shapes: HashMap<InstanceKey, ContentId>,
}

impl<I, S> Default for Later<I, S> {
    fn default() -> Self {
        Later { nodes: Sorted::default(), shapes: HashMap::default() }
    }
}

const _: fn() = later_is_send_and_sync::<u32, u32>;

fn later_is_send_and_sync<I: Send + Sync, S: Send + Sync>() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Later<I, S>>();
}

/// What a lazy run leaves for its build and its [`Built`].
#[derive(Default)]
pub(super) struct Lazily {
    pub(super) walked: Vec<Walked>,
    pub(super) entry: Vec<InstanceKey>,
    /// The shape of each function whose IR was built for its check.
    pub(super) shapes: HashMap<InstanceKey, ContentId>,
}

/// Where a compile's template came from.
enum Made {
    Memo,
    Compiled,
    /// Another build of the memo compiled it ([`crate::flight`]).
    Flown,
}

fn nothing() -> (InstanceKey, ItemKey) {
    (InstanceKey(ContentId::default()), ItemKey(ContentId::default()))
}

/// Whether a function first compiled at `backend` may be compiled alone on its first call.
fn is_dev(backend: Backend) -> bool {
    match backend {
        Backend::Cranelift { .. } | Backend::Stencil => true,
        Backend::Llvm { .. } => false,
    }
}

/// `key`'s node, if a lazy build may compile it: a function of the walk at a dev backend.
fn lazy_node<I, S>(nodes: &Sorted<InstanceKey, FnNode<I, S>>, key: InstanceKey) -> Result<&FnNode<I, S>, CodegenError> {
    let Some(node) = nodes.get(&key) else {
        return Err(error(key, nothing().1, "compiled lazily, but no function of the build's walk"));
    };
    dev_only(key, node)?;
    Ok(node)
}

/// A lazy function is compiled alone on its first call, which a release unit can't be: its
/// imports are decided from the whole build's summaries.
fn dev_only<I, S>(key: InstanceKey, node: &FnNode<I, S>) -> Result<(), CodegenError> {
    if is_dev(node.spec.backend) {
        return Ok(());
    }
    let backend = node.spec.backend;
    Err(error(
        key,
        node.unit.item,
        // alloc: an error
        format!(
            "a lazy build compiles each function alone on its first call, and this one's first \
             code is {backend:?}'s, whose imports need the whole build; a live build starts it \
             at Cranelift"
        ),
    ))
}

/// The function for `key` from the store's template under `at`, if it has one.
fn stored_one<I, S, L: Lookup + ?Sized>(
    node: &FnNode<I, S>,
    at: InstanceKey,
    key: InstanceKey,
    store: &L,
) -> Result<Option<CompiledFunction>, CodegenError> {
    let at = keys::template(&at);
    match store.read(&at) {
        Ok(None) => Ok(None),
        Ok(Some(bytes)) => match stored_function(bytes, key, &node.refs) {
            Ok(Ok(function)) => Ok(Some(function)),
            Ok(Err(why)) => Err(unfit(key, node.unit.item, &node.described.name, Whence::Store(at), why)),
            // alloc: an error
            Err(why) => Err(error(key, node.unit.item, format!("the store's {at:?}: {why}"))),
        },
        // alloc: an error
        Err(why) => Err(error(key, node.unit.item, format!("reading the store's {at:?}: {why}"))),
    }
}

/// The store's value of `template`, kept under `at`.
fn put_of(at: &InstanceKey, template: &Template) -> Put {
    // alloc: the store's value of a template, once a function
    Put { key: keys::template(at), kind: Kind::Code, bytes: blob::encode(template) }
}

/// The function for `key` from the memo's `template` of `shape`, with the template's [`Put`]
/// under `at`.
fn memoized<I, S>(
    node: &FnNode<I, S>,
    key: InstanceKey,
    at: &InstanceKey,
    shape: ContentId,
    template: &Template,
) -> Result<(CompiledFunction, Vec<Put>), CodegenError> {
    let unfit = |why| unfit(key, node.unit.item, &node.described.name, Whence::Shape(shape), why);
    let function = template.instantiate(key, &node.refs).map_err(unfit)?;
    // alloc: the store's value of a memo hit, once a function
    Ok((function, vec![put_of(at, template)]))
}

/// `unit`, `node`'s function at its own or another tier, where neither the store nor a
/// recorded shape has it: its IR built, then the memo's template of its shape, else compiled.
/// With the shape its template is kept under in the memo.
fn compiled<B: Prepare>(
    backend: &B,
    frontend: &B::Frontend,
    memo: &Memo,
    nodes: &Sorted<InstanceKey, Node<B>>,
    node: &Node<B>,
    unit: &Unit,
    spec: &Spec,
) -> Result<(CompiledFunction, Vec<Put>, Made, ContentId), CodegenError> {
    let (key, item) = (unit.instance.key(), unit.item);
    let at = placed(stored(key, &node.built_in), &node.refs);
    let (shape, made) = backend.prepare(frontend, unit, spec, &node.instance, &node.refs)?;
    let shape = shape.key();
    if let Some(template) = memo.get(&shape) {
        let (function, puts) = memoized(node, key, &at, shape, template)?;
        return Ok((function, puts, Made::Memo, shape));
    }
    let emitted = backend.emit(unit, spec, made, &node.refs, None)?;
    if emitted.function.key != key {
        // alloc: an error
        return Err(error(key, item, format!("compiled as {:?}", emitted.function.key)));
    }
    if let Some(it) = unkept_inline(nodes, &emitted.function, node.built_in.iter().copied()) {
        return Err(error(key, item, unkept_message(nodes, node, it)));
    }
    // alloc: the store's values of a compile: its template and its side table
    let mut puts = Vec::with_capacity(2);
    match (emitted.side, emitted.function.side) {
        (Some(bytes), Some(id)) => puts.push(Put { key: keys::side(&id), kind: Kind::Debug, bytes }),
        (None, None) => {}
        _ => return Err(error(key, item, "the compile's side table and the function's record of it disagree")),
    }
    let template = Template::of_owned(emitted.function, &node.refs, &emitted.extra)
        // alloc: an error
        .map_err(|why| error(key, item, why.to_string()))?;
    let kept = memo.put(shape, template);
    puts.push(put_of(&at, kept));
    let unfit = |why| unfit(key, item, &node.described.name, Whence::Shape(shape), why);
    let function = kept.instantiate(key, &node.refs).map_err(unfit)?;
    Ok((function, puts, Made::Compiled, shape))
}

/// [`compiled`], by one build of the memo ([`crate::flight`]), keyed by what the function is
/// stored under: this call's when it claims it first, after `check`; else the template
/// another build compiled, taken once it landed. No claim is held while waiting. With the
/// shape its template is kept under in the memo.
#[allow(clippy::too_many_arguments)]
fn claimed<B: Prepare>(
    backend: &B,
    frontend: &B::Frontend,
    memo: &Memo,
    nodes: &Sorted<InstanceKey, Node<B>>,
    node: &Node<B>,
    unit: &Unit,
    spec: &Spec,
    check: impl Fn() -> Result<(), CodegenError>,
) -> Result<(CompiledFunction, Vec<Put>, Made, ContentId), CodegenError> {
    let (key, at) = (unit.instance.key(), placed(stored(unit.instance.key(), &node.built_in), &node.refs));
    let flights = memo.flights();
    loop {
        let mut claims = flights.claims("a function compiled at its first call");
        let shape = match claims.claim(&[at])[0] {
            Asked::Landed(shape) => shape,
            Asked::Ours => {
                check()?;
                let made = compiled(backend, frontend, memo, nodes, node, unit, spec)?;
                claims.land(&[(at, made.3)]);
                return Ok(made);
            }
            Asked::Theirs => {
                drop(claims);
                let waited = flights.wait(&[at], || frontend.stopped()).ok_or_else(stopped)?;
                match waited[0] {
                    Some(shape) => shape,
                    // Let go of by the build that claimed it, which did not compile it.
                    None => continue,
                }
            }
        };
        return match memo.get(&shape) {
            Some(template) => {
                memoized(node, key, &at, shape, template).map(|(function, puts)| (function, puts, Made::Flown, shape))
            }
            // alloc: an error
            None => Err(error(key, unit.item, format!("its compile landed under shape {shape:?}, which the memo has no template of"))),
        };
    }
}

/// Refuses `key`'s compile unless the front end still describes its instance with the code and
/// arguments of its key: IR built from another body would be placed in the image's slot.
fn same_code<F: Frontend>(frontend: &F, node: &FnNode<F::Instance, F::Static>, key: InstanceKey) -> Result<(), CodegenError> {
    let now = frontend.describe(&node.instance, &node.requester)?;
    let was = node.unit.instance;
    if now.code == was.code && now.args == was.args {
        return Ok(());
    }
    Err(error(
        key,
        node.unit.item,
        // alloc: an error
        format!(
            "changed before its first call: the front end now describes it as code {:?} with \
             arguments {:?}, and the running image has it as code {:?} with arguments {:?}; its \
             old body can't be compiled into the running image (the edit's patch gives the run \
             its new code)",
            now.code, now.args, was.code, was.args
        ),
    ))
}

/// `node`'s unit and settings at `backend`, counting into its cells when `counted`.
fn at_tier<I, S>(key: InstanceKey, node: &FnNode<I, S>, backend: Backend, counted: bool) -> Result<(Unit, Spec), CodegenError> {
    // clone: per tier-up, the settings it compiles at
    let spec = Spec { backend, counted, fold: false, ..node.spec.clone() };
    let missing = match backend {
        Backend::Llvm { .. } if spec.toolchain.llvm.is_none() => Some("LLVM, and this blaze was built without it"),
        Backend::Stencil if spec.toolchain.stencils.is_none() => Some("stencils, and the target has no stencil library"),
        _ => None,
    };
    if let Some(why) = missing {
        // alloc: an error
        return Err(error(key, node.unit.item, format!("tiered up to {why}")));
    }
    let instance = Instance { settings: spec.settings(), ..node.unit.instance };
    // clone: per tier-up, its unit (plain IDs)
    let unit = Unit { instance, backend, ..node.unit.clone() };
    Ok((unit, spec))
}

impl<I, S> Later<I, S> {
    /// `key`'s node and the memo's template of the shape its check recorded: no allocation,
    /// no lock but the memo's.
    fn recorded<'m>(&self, memo: &'m Memo, key: InstanceKey) -> Option<(&FnNode<I, S>, &'m Template, ContentId)> {
        let node = self.nodes.get(&key).filter(|it| is_dev(it.spec.backend))?;
        let shape = *self.shapes.get(&key)?;
        Some((node, memo.get(&shape)?, shape))
    }

    /// The memo's function for `key`, as [`Later::compile_kept`] gives it from the memo, with
    /// nothing encoded for the store ([`Later::memo_put`] is that). `None` when `key` is no
    /// function of the walk at a dev backend, its check recorded no shape (it was compiled
    /// ahead or the store had it), or the memo has no template of that shape. A probe for the
    /// thread that reads a lazy run's asks: it reads no store, builds no IR, takes no front
    /// end and no lock but the memo's. An `Err` is the instantiation's, which `compile_kept`
    /// gives too.
    pub fn memo_hit(&self, memo: &Memo, key: InstanceKey) -> Option<Result<CompiledFunction, CodegenError>> {
        let (node, template, shape) = self.recorded(memo, key)?;
        let unfit = |why| unfit(key, node.unit.item, &node.described.name, Whence::Shape(shape), why);
        Some(template.instantiate(key, &node.refs).map_err(unfit))
    }

    /// The store's value of [`Later::memo_hit`]'s template, under `key`'s stored key: `None`
    /// where `memo_hit` is.
    pub fn memo_put(&self, memo: &Memo, key: InstanceKey) -> Option<Put> {
        let (node, template, _) = self.recorded(memo, key)?;
        Some(put_of(&node.stored(), template))
    }

    /// One function of the walk, as a lazy run asks for it on its first call: the store's
    /// template, else the memo's by the shape its check recorded, else compiled at its dev
    /// backend against `frontend`, under the key check (refused unless the front end still
    /// describes it with its key's code and arguments). With what the caller puts in the
    /// store: the template under the function's key and its side table, none for a template
    /// the store had.
    pub fn compile_kept<B, L>(
        &self,
        backend: &B,
        frontend: &B::Frontend,
        memo: &Memo,
        key: InstanceKey,
        store: &L,
    ) -> Result<(CompiledFunction, Vec<Put>), CodegenError>
    where
        B: Prepare,
        B::Frontend: Frontend<Instance = I, Static = S>,
        L: Lookup + ?Sized,
    {
        let node = lazy_node(&self.nodes, key)?;
        if let Some(function) = stored_one(node, node.stored(), key, store)? {
            // alloc: none, empty
            return Ok((function, Vec::new()));
        }
        if let Some((_, template, shape)) = self.recorded(memo, key) {
            return memoized(node, key, &node.stored(), shape, template);
        }
        let check = || same_code(frontend, node, key);
        claimed(backend, frontend, memo, &self.nodes, node, &node.unit, &node.spec, check).map(|it| (it.0, it.1))
    }

    /// `key`'s function compiled again at `backend` (a tier's, `blaze_stencil::tier::Tier::backend`),
    /// counting into its cells when `counted` (`Tier::counts`), alone (no imports, LLVM's
    /// too), under [`Later::compile_kept`]'s key check: the store's template, else the memo's
    /// by its shape, else compiled. The function's key is the tier's instance; the caller
    /// places it in `key`'s slot.
    #[allow(clippy::too_many_arguments)]
    pub fn tier_up<B, L>(
        &self,
        backend: &B,
        frontend: &B::Frontend,
        memo: &Memo,
        key: InstanceKey,
        to: Backend,
        counted: bool,
        store: &L,
    ) -> Result<(CompiledFunction, Vec<Put>), CodegenError>
    where
        B: Prepare,
        B::Frontend: Frontend<Instance = I, Static = S>,
        L: Lookup + ?Sized,
    {
        let node = lazy_node(&self.nodes, key)?;
        let (unit, spec) = at_tier(key, node, to, counted)?;
        let tiered = unit.instance.key();
        if let Some(function) = stored_one(node, placed(stored(tiered, &node.built_in), &node.refs), tiered, store)? {
            // alloc: none, empty
            return Ok((function, Vec::new()));
        }
        let check = || same_code(frontend, node, key);
        claimed(backend, frontend, memo, &self.nodes, node, &unit, &spec, check).map(|it| (it.0, it.1))
    }
}

impl<B: Prepare> Build<'_, B> {
    /// [`Build::run_with_statics`] for a live build that compiles lazily (`later`): every
    /// instance the roots reach is walked and checked, the statics are compiled, and of the
    /// functions only the entry (the roots at the places `entry` gives) and what a static in
    /// an initializer or finalizer section names ([`runs_at_start`]). [`Built::walked`] lists
    /// every function in the walk's order, [`Built::entry`] the entry's keys; the rest are
    /// compiled by [`Build::into_later`]'s [`Later`] on their first call. A build whose output
    /// is a file is compiled whole, so with [`Mode::Output`] it is an error.
    pub fn run_later(
        &mut self,
        roots: &[(FrontInstance<B>, Origin)],
        static_roots: &[FrontStatic<B>],
        entry: &[usize],
        store: &impl Lookup,
    ) -> Result<Built, Vec<CodegenError>> {
        if self.policy.mode == Mode::Output {
            let (unit, item) = nothing();
            let why = "a lazy build (`later`) of a build whose output is a file, which is compiled whole";
            return Err(vec![error(unit, item, why)]); // alloc: an error
        }
        self.run_as(roots, static_roots, Some(entry), store)
    }

    /// The last run's walk, owned, for a lazy run's first calls.
    pub fn into_later(self) -> Later<FrontInstance<B>, FrontStatic<B>> {
        self.last
    }

    /// [`Later::compile_kept`] of the last run's walk, the function alone.
    pub fn compile_one<L: Lookup + ?Sized>(&self, key: InstanceKey, store: &L) -> Result<CompiledFunction, CodegenError> {
        self.compile_kept(key, store).map(|it| it.0)
    }

    /// [`Later::compile_kept`] of the last run's walk, against this build's front end.
    pub fn compile_kept<L: Lookup + ?Sized>(
        &self,
        key: InstanceKey,
        store: &L,
    ) -> Result<(CompiledFunction, Vec<Put>), CodegenError> {
        self.last.compile_kept(self.backend, self.frontend, self.memo, key, store)
    }

    /// [`Later::memo_hit`] of the last run's walk, with its template's [`Put`]
    /// ([`Later::memo_put`]): what [`Build::compile_kept`] gives from the memo.
    pub fn memo_hit(&self, key: InstanceKey) -> Option<Result<(CompiledFunction, Vec<Put>), CodegenError>> {
        let hit = self.last.memo_hit(self.memo, key)?;
        // alloc: the store's value of a memo hit, once a function
        Some(hit.map(|function| (function, self.last.memo_put(self.memo, key).into_iter().collect())))
    }

    /// A lazy run's functions, once the walk ended with no error: each checked, the entry's
    /// and the start sections' compiled into `out`. `order` is every function's key in the
    /// walk's order; `root_keys` the roots', in their order.
    pub(super) fn later(
        &self,
        nodes: &Sorted<InstanceKey, Node<B>>,
        statics: &Sorted<StaticId, StaticNode<B>>,
        root_keys: &[InstanceKey],
        entry: &[usize],
        order: &[InstanceKey],
        store: &impl Lookup,
        out: &mut Out,
    ) -> Lazily {
        let mut entry_keys = Vec::with_capacity(entry.len());
        for &at in entry {
            match root_keys.get(at) {
                Some(key) => entry_keys.push(*key),
                None => {
                    let (unit, item) = nothing();
                    // alloc: an error
                    out.errors.push(error(unit, item, format!("the entry is root {at}, of {} roots", root_keys.len())));
                }
            }
        }
        for (key, node) in nodes.iter() {
            if let Err(why) = dev_only(*key, node) {
                out.errors.push(why);
            }
        }
        if !out.errors.is_empty() {
            return Lazily::default();
        }

        let started = statics
            .values()
            .filter(|it| it.described.section.as_deref().is_some_and(runs_at_start))
            .flat_map(|it| it.refs.iter().filter_map(callee));
        // alloc: per build, its entry and start functions
        let ahead: HashSet<InstanceKey> = entry_keys.iter().copied().chain(started).collect();
        // alloc: per build, its entry and start functions
        let mut ahead: Vec<InstanceKey> = ahead.into_iter().collect();
        ahead.sort_unstable();

        // The checks of every function compiled later: a template the store has, else its IR.
        // The store is read on this thread, its bytes decoded in place on all cores.
        // alloc: per build, sized by its units
        let (mut unchecked, mut kept) = (Vec::new(), Vec::new());
        for key in order.iter().filter(|it| ahead.binary_search(*it).is_err()) {
            let node = &nodes[key];
            let at = keys::template(&node.stored());
            match store.read(&at) {
                Ok(Some(bytes)) => kept.push((*key, node, at, bytes)),
                Ok(None) => unchecked.push(*key),
                // alloc: an error
                Err(why) => out.errors.push(error(*key, node.unit.item, format!("reading the store's {at:?}: {why}"))),
            }
        }
        // alloc: none but for the errors
        let unread: Vec<CodegenError> = par::filter_map(&kept, |(key, node, at, bytes)| {
            let why = frame::task(Tag::Codegen, |frame| blob::decode_view(bytes, frame).err())?;
            // alloc: an error
            Some(error(*key, node.unit.item, format!("the store's {at:?}: {why}")))
        });
        out.errors.extend(unread);
        let checked: Vec<(InstanceKey, Result<ContentId, CodegenError>)> =
            self.until_stopped(&unchecked, &mut out.errors, |key| {
                let node = &nodes[key];
                let made = self.backend.prepare(self.frontend, &node.unit, &node.spec, &node.instance, &node.refs);
                // The IR is let go here, on the thread that built it: only its shape is kept.
                (*key, made.map(|(shape, _)| shape.key()))
            });
        let mut shapes = HashMap::with_capacity_and_hasher(checked.len(), Default::default());
        for (key, shape) in checked {
            match shape {
                Ok(shape) => {
                    shapes.insert(key, shape);
                }
                Err(why) => out.errors.push(why),
            }
        }

        // The functions compiled ahead: the store's on this thread, the rest on all cores, each
        // by one build of the memo. Compiled at the walk's revision, so with no key check.
        // alloc: per build, its entry and start functions
        let mut missing = Vec::new();
        for key in &ahead {
            let read = lazy_node(nodes, *key).and_then(|node| Ok((node, stored_one(node, node.stored(), *key, store)?)));
            match read {
                Ok((node, Some(function))) => {
                    out.stats.stored += 1;
                    out.push(node.unit.instance.code, function);
                }
                Ok((node, None)) => missing.push(node),
                Err(why) => out.errors.push(why),
            }
        }
        let compiled = self.until_stopped(&missing, &mut out.errors, |node| {
            let made = claimed(self.backend, self.frontend, self.memo, nodes, node, &node.unit, &node.spec, || Ok(()));
            (node.unit.instance.code, made)
        });
        for (code, made) in compiled {
            match made {
                Ok((function, puts, how, shape)) => {
                    out.used(shape);
                    match how {
                        Made::Memo => out.stats.memo += 1,
                        Made::Compiled => out.stats.compiled += 1,
                        Made::Flown => out.stats.flown += 1,
                    }
                    out.push(code, function);
                    out.puts.extend(puts);
                }
                Err(why) => out.errors.push(why),
            }
        }

        let walked = order
            .iter()
            .map(|key| {
                let node = &nodes[key];
                Walked { key: *key, code: node.unit.instance.code, item: node.unit.item }
            })
            // alloc: per build, what it gives the lazy runner
            .collect();
        Lazily { walked, entry: entry_keys, shapes }
    }
}
