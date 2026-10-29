//! What `#[blaze::cache]` keeps the results of a function by: a hash of the code of its body
//! and of everything that code can reach, as blaze compiled it. The attribute keeps the body
//! in a closure and has the function ask `blaze::__private::__blaze_cache_code` for the code
//! of that closure, which rustc answers with nothing, and so caches nothing. blaze compiles
//! the call into the answer: the hash, or why the function can't be cached.
//!
//! The hash is of the compiled code without what tells where it was written, so comments,
//! formatting and lines moved keep it, and an edit to anything the body calls, directly or
//! through others, changes it. Each function reached is hashed as the instance compiled, so
//! the instances of a generic function have hashes of their own. A call through a `dyn` goes
//! to a vtable, and the code reached builds every vtable it can call through: the methods of
//! those vtables are reached as the rest is. The arguments can't be a `dyn`.
//!
//! A function can be cached when what it gives depends on its arguments alone. It can't when
//! what it reaches reads a static that can change, a thread-local or a foreign static, calls a
//! foreign function other than the allocator, `mem*`, libm and the LLVM intrinsics that read
//! no counter, randomness or state of the machine, or the code of the workspace
//! dereferences a raw pointer, takes an address as a number or has assembly in it, or core's
//! formats, hashes or orders pointers by their addresses or gives one as a number. Neither can
//! it when an argument can change behind a shared reference, as a `Cell` or an atomic can, or
//! holds a raw pointer of a type of the workspace. The standard library's I/O, time,
//! randomness and environment are all foreign calls or statics that change, so they are seen.
//! The functions of core and alloc that aren't generic run as rustc built them and are known by
//! their names, not hashed; their MIR is looked through for the same instead.
//!
//! Two of those are let through where what they change is nothing the result can see. The
//! counts of an `Rc` or an `Arc` in an argument change behind a shared reference, so what reads
//! them is refused rather than the argument. The seed std's `HashMap` and `HashSet` make
//! orders their entries alone, so a seed made for one is pure and what gives the entries in
//! that order, or the seed, is not; `blaze` writes their entries sorted in a key for the
//! same reason.

use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex};

use base_db::{Crate, LangCrateOrigin};
use cranelift_codegen::ir::{InstBuilder, Value, types};
use cranelift_module::ModuleRelocTarget;
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup, StaticId, VariantId,
    signatures::{
        FunctionSignature, StaticFlags, StaticSignature, StructSignature, TraitSignature,
    },
};
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind};
use salsa::plumbing::{AsId, FromId};

use hir_def::CallableDefId;
use hir_ty::{
    InferBodyId,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    mir::{
        AggregateKind, MirBody, MirSpan, Operand, OperandKind, Rvalue, StatementKind,
        TerminatorKind,
    },
    next_solver::{DbInterner, GenericArgs, ParamEnv, Ty, TyKind},
    traits::{ParamEnvAndCrate, StoredParamEnvAndCrate},
};
use hir_ty_mir::mir::purity::{is_const_pure, operands, raw_use};

mod dispatch;

use super::{
    CodegenError, CompiledFunction, ConstBlob, DataRequest, FuncTarget, FunctionCx, Instance,
    InstanceDef, MEM, Requests, Result, SymbolResolver, compiled_instance, compiled_root,
    compiled_static,
    fragment::{Code, TextPart},
    home::Homes,
    is_thread_local, link_name, ptr, type_id,
};

/// The function a cached function asks for the code of its body.
const CODE: &str = "__blaze_cache_code";
/// The function whose instance names how the arguments and the result are written: reached
/// for the hash, though what it does is not the cached function's.
const CODEC: &str = "__blaze_codec";
/// Changes when what is hashed does, so no hash of before means anything now.
const FORMAT: u32 = 1;

/// Whether `func` is the function a cached function asks for the code of its body.
pub(super) fn is_code(db: &dyn HirDatabase, func: FunctionId) -> bool {
    FunctionSignature::of(db, func).name.as_str() == CODE
}

fn is_codec(db: &dyn HirDatabase, func: FunctionId) -> bool {
    FunctionSignature::of(db, func).name.as_str() == CODEC
}

/// The closure that `#[blaze::cache]` put the body of `func` in, when it is a cached
/// function: the one it asks for the code of.
pub(super) fn cached_closure(db: &dyn HirDatabase, func: FunctionId) -> Option<salsa::Id> {
    if !uses_cache(db, func.krate(db)) {
        return None;
    }
    cached_closure_of(db, func).map(|it| it.as_id())
}

/// Whether `krate` can have cached functions: the attribute calls into the `blaze` crate, so
/// a crate without it as a dependency has none, and costs nothing to look through.
pub(super) fn uses_cache(db: &dyn HirDatabase, krate: Crate) -> bool {
    krate.data(db).dependencies.iter().any(|dep| {
        dep.crate_id
            .extra_data(db)
            .display_name
            .as_ref()
            .is_some_and(|name| name.crate_name().symbol().as_str() == "blaze")
    })
}

#[salsa::tracked]
fn cached_closure_of<'db>(
    db: &'db dyn HirDatabase,
    func: FunctionId,
) -> Option<InternedClosureId<'db>> {
    let body = db.mir_body(func.into()).ok()?;
    body.basic_blocks.iter().find_map(|(_, block)| {
        let TerminatorKind::Call { func: callee, .. } = &block.terminator.as_ref()?.kind else {
            return None;
        };
        let (callee, args) = fn_item(callee)?;
        if !is_code(db, callee) {
            return None;
        }
        let TyKind::Closure(id, _) = args.type_at(0).kind() else { return None };
        Some(id.0)
    })
}

/// The function a constant operand names, with its generic arguments.
fn fn_item<'db>(operand: &Operand) -> Option<(FunctionId, GenericArgs<'db>)> {
    let ty: Ty<'db> = match &operand.kind {
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        OperandKind::Allocation { allocation } => allocation.as_ref().ty,
        _ => return None,
    };
    let TyKind::FnDef(def, args) = ty.kind() else { return None };
    match def.0 {
        CallableDefId::FunctionId(func) => Some((func, args)),
        _ => None,
    }
}

/// The hash of the code of the closure a cached function keeps its body in and of all it
/// reaches, or why the function can't be cached.
#[salsa::tracked(returns(ref))]
pub(super) fn cache_code(
    db: &dyn HirDatabase,
    closure: Instance,
    env: StoredParamEnvAndCrate,
) -> std::result::Result<u128, String> {
    let (trouble, counted) = arguments_trouble(db, &closure, &env);
    let mut walk = Walk::new(db, counted);
    let root = walk.reach(Node::Instance(closure.clone(), env.clone()));
    let mut next = 0;
    while let Some(node) = walk.nodes.get(next).map(|it| it.node.clone()) {
        walk.hash_node(next as u32, &node);
        next += 1;
    }
    let name = cached_name(db, &closure);
    if let Some(why) = walk.impure(root) {
        return Err(format!("`{name}` is not cached: {why}"));
    }
    if let Some(why) = trouble {
        return Err(format!("`{name}` is not cached: {why}"));
    }
    let mut hasher = rustc_stable_hash::StableSipHasher128::new();
    (FORMAT, super::target::Target::current().triple(), super::store::toolchain())
        .hash(&mut hasher);
    closure.checked.hash(&mut hasher);
    for arg in closure.args.as_ref().iter() {
        arg.ty()
            .and_then(|ty| hir_ty_mir::mir::type_name::rustc_type_name(db, ty))
            .hash(&mut hasher);
    }
    for segment in &walk.segments {
        segment.hash(&mut hasher);
    }
    let type_id::Fingerprint(low, high) = hasher.finish();
    Ok(u128::from(low) | u128::from(high) << 64)
}

/// For each of `roots`, the functions a test harness runs, a hash of the code of the function
/// and of everything it reaches, as [`cache_code`] hashes a cached body, for a test to be run
/// again only once that changed, and the first of each [`Kind`] of what makes its end depend
/// on more than that code, by the fewest calls ([`Reach::found`]): what blaze can't compile,
/// the world outside the program (files, the environment, processes, the network), foreign
/// code, assembly, a function pointer made from data, a static other tests can change, and
/// the clock, randomness and threads, which only make a test flaky.
///
/// The nodes are hashed once for all the roots; each root then hashes, in the order it finds
/// them, every node it reaches with where the node's edges go in that order, so that a hash
/// does not depend on the other roots.
///
/// A function of crates outside the checkout, instantiated with their types, is hashed by its
/// stable name and not gone into: the name stands for its code and for all it reaches, as it
/// does for the store's compiled functions ([`super::cached`]). What a test's hash says of
/// what is below it, what it reaches of each kind, is kept in the store by the name, so that
/// a daemon with the store compiles none of it.
pub fn tests_reach(
    db: &dyn HirDatabase,
    symbols: &dyn SymbolResolver,
    roots: &[FunctionId],
) -> Vec<Reach> {
    let header = (
        FORMAT,
        "tests by stable names",
        super::target::Target::current().triple(),
        super::store::toolchain(),
    );
    let started = std::time::Instant::now();
    let mut took = Vec::new();
    let names = super::stable::StableNames::new(db, symbols, FxHashSet::default(), true);
    took.push(("names", started.elapsed()));
    let store = super::store::Store::global();
    let mut walk = Walk::new(db, false);
    walk.tests = true;
    let starts: Vec<u32> = roots.iter().map(|&it| walk.reach(Node::Root(it))).collect();
    let mut sealed: FxHashMap<u32, Sealed> = FxHashMap::default();
    let mut spent = [std::time::Duration::ZERO; 5];
    let mut next = 0;
    // In waves of the nodes found and not yet hashed: each is named and looked up in the
    // store, the code of those to be hashed is compiled on every core, then they are hashed
    // in the order found, which is all a hash depends on.
    while next < walk.nodes.len() {
        let wave: Vec<Node> = walk.nodes[next..].iter().map(|it| it.node.clone()).collect();
        let mut looked = Vec::with_capacity(wave.len());
        for node in &wave {
            let before = std::time::Instant::now();
            let name = sealed_name(db, &names, node);
            spent[0] += before.elapsed();
            let Some(name) = name else {
                looked.push(None);
                continue;
            };
            let key = below_key(&header, &name);
            let before = std::time::Instant::now();
            let below = Below::known(store, &key);
            spent[1] += before.elapsed();
            looked.push(Some((name, key, below)));
        }
        let hashed: Vec<&Node> = wave
            .iter()
            .zip(&looked)
            .filter(|(_, looked)| matches!(looked, None | Some((_, _, None))))
            .map(|(node, _)| node)
            .filter(|node| !matches!(node, Node::Leaf(..)))
            .collect();
        let before = std::time::Instant::now();
        compile_ahead(symbols, &hashed);
        spent[4] += before.elapsed();
        for (node, looked) in wave.iter().zip(looked) {
            let at = next as u32;
            next += 1;
            let Some((name, key, below)) = looked else {
                let before = std::time::Instant::now();
                walk.hash_node(at, node);
                spent[2] += before.elapsed();
                continue;
            };
            if below.is_none() {
                let before = std::time::Instant::now();
                walk.hash_node(at, node);
                spent[3] += before.elapsed();
                walk.segments.pop();
            }
            let mut hasher = rustc_stable_hash::StableSipHasher128::new();
            (4u8, &name).hash(&mut hasher);
            let type_id::Fingerprint(low, high) = hasher.finish();
            walk.segments.push(u128::from(low) | u128::from(high) << 64);
            sealed.insert(at, Sealed { key, below });
        }
    }
    took.push(("nodes", started.elapsed()));
    // One found later is mostly below one found earlier, and is then taken whole by it.
    let mut walked: Vec<u32> =
        sealed.iter().filter(|(_, it)| it.below.is_none()).map(|(&at, _)| at).collect();
    walked.sort_unstable_by(|a, b| b.cmp(a));
    let added = walked.len();
    for at in walked {
        let below = Arc::new(below_of(&walk, &sealed, at));
        Below::keep(store, &sealed[&at].key, &below);
        sealed.get_mut(&at).unwrap().below = Some(below);
    }
    // What a daemon adds is left in the store's open pack, which the next daemon does not
    // read: sealed now, a daemon that starts later walks none of it again.
    if added >= SEAL_AFTER
        && let Some(store) = store
    {
        std::thread::spawn(move || store.seal());
    }
    took.push(("below", started.elapsed()));
    let mut local: FxHashMap<u32, u32> = FxHashMap::default();
    let mut way = Way::default();
    let reach = starts
        .into_iter()
        .map(|start| {
            local.clear();
            way.clear(start);
            local.insert(start, 0);
            let mut found = Vec::new();
            let mut at = 0;
            while let Some(&node) = way.order.get(at) {
                if let Some(Sealed { below: Some(below), .. }) = sealed.get(&node) {
                    for it in &below.found {
                        if !found.iter().any(|seen: &Found| seen.kind == it.kind) {
                            let path = joined(walk.path_to(&way, at), &it.path);
                            found.push(Found { path, ..it.clone() });
                        }
                    }
                    at += 1;
                    continue;
                }
                for (kind, what, why) in walk.reasons(node) {
                    if !found.iter().any(|seen| seen.kind == kind) {
                        found.push(Found { kind, what, why, path: walk.path_to(&way, at) });
                    }
                }
                for edge in &walk.nodes[node as usize].edges {
                    if let std::collections::hash_map::Entry::Vacant(it) = local.entry(edge.to) {
                        it.insert(way.order.len() as u32);
                        way.push(edge.to, at, edge.site);
                    }
                }
                at += 1;
            }
            let mut hasher = rustc_stable_hash::StableSipHasher128::new();
            header.hash(&mut hasher);
            for &node in &way.order {
                walk.segments[node as usize].hash(&mut hasher);
                let edges = match sealed.contains_key(&node) {
                    true => &[][..],
                    false => &walk.nodes[node as usize].edges[..],
                };
                edges.len().hash(&mut hasher);
                for edge in edges {
                    local[&edge.to].hash(&mut hasher);
                }
            }
            let type_id::Fingerprint(low, high) = hasher.finish();
            found.sort_by_key(|it| it.kind);
            Reach { hash: u128::from(low) | u128::from(high) << 64, found }
        })
        .collect();
    took.push(("roots", started.elapsed()));
    if std::env::var_os("BLAZE_STATS").is_some() {
        let steps: Vec<String> =
            took.iter().map(|(name, at)| format!("{name}={}", at.as_micros())).collect();
        eprintln!(
            "tests-reach-steps-us: {} (naming={} store={} compiling={} hashing={} sealed-hashing={}; {} roots, {} nodes, {} sealed)",
            steps.join(" "),
            spent[0].as_micros(),
            spent[1].as_micros(),
            spent[4].as_micros(),
            spent[2].as_micros(),
            spent[3].as_micros(),
            roots.len(),
            walk.nodes.len(),
            sealed.len()
        );
    }
    reach
}

/// Compiles the code [`Walk::hash_node`] hashes of each of `nodes` on every core, for the
/// queries to have it when the walk asks.
fn compile_ahead(symbols: &dyn SymbolResolver, nodes: &[&Node]) {
    symbols.in_parallel(nodes.len(), &|db, _, at| match nodes[at] {
        Node::Root(func) => _ = compiled_root(db, *func),
        Node::Instance(instance, env) => _ = compiled_instance(db, instance.clone(), env.clone()),
        Node::Static(id) => _ = compiled_static(db, *id),
        Node::Leaf(..) => {}
    });
}

/// The nodes a walk from one node found, in the order found, each with where in `order` the
/// node that first reached it is and by what.
#[derive(Default)]
struct Way {
    order: Vec<u32>,
    from: Vec<u32>,
    via: Vec<Site>,
}

impl Way {
    fn clear(&mut self, start: u32) {
        self.order.clear();
        self.from.clear();
        self.via.clear();
        self.push(start, 0, Site::None);
    }

    fn push(&mut self, node: u32, from: usize, via: Site) {
        self.order.push(node);
        self.from.push(from as u32);
        self.via.push(via);
    }
}

/// The calls to a sealed node, then those `rest` has below it: the sealed node calls the next
/// where `rest`'s first, itself, says.
fn joined(mut path: Vec<Call>, rest: &[Call]) -> Vec<Call> {
    if let (Some(last), Some(first)) = (path.last_mut(), rest.first()) {
        last.at = first.at.clone();
    }
    path.extend(rest.iter().skip(1).cloned());
    path
}

/// A node [`tests_reach`] takes whole by its stable name.
struct Sealed {
    /// Where the store keeps what is below it.
    key: [u8; super::store::KEY],
    /// Read from the store, or found by going into it.
    below: Option<Arc<Below>>,
}

/// What a test's hash says of what a node reaches, the node itself too: the first of each
/// [`Kind`], with the calls to it from the node.
#[derive(Default)]
struct Below {
    found: Vec<Found>,
}

/// What is below each sealed node this process found or read, by its key: the store gives
/// only what its sealed packs have, and a pack is sealed once it is full, so what one build
/// adds is not found there by the next builds of the daemon.
static KNOWN: LazyLock<Mutex<FxHashMap<[u8; super::store::KEY], Arc<Below>>>> =
    LazyLock::new(Default::default);

/// How many sealed nodes a walk finds what is below of for the store's open pack to be sealed.
const SEAL_AFTER: usize = 256;

/// How many sealed nodes [`KNOWN`] keeps before it starts again.
const KNOWN_AT_MOST: usize = 1 << 20;

impl Below {
    const FORMAT: &str = "below 2";

    /// What is below the sealed node of `key`, as this process or the store has it.
    fn known(
        store: Option<&super::store::Store>,
        key: &[u8; super::store::KEY],
    ) -> Option<Arc<Below>> {
        if let Some(it) = KNOWN.lock().unwrap().get(key) {
            return Some(it.clone());
        }
        let below = Arc::new(Below::read(&store?.get(key)?)?);
        Below::remember(key, &below);
        Some(below)
    }

    /// Keeps what is below the sealed node of `key`, for this process and in the store.
    fn keep(
        store: Option<&super::store::Store>,
        key: &[u8; super::store::KEY],
        below: &Arc<Below>,
    ) {
        if let Some(store) = store {
            store.insert(key, below.write().as_bytes());
        }
        Below::remember(key, below);
    }

    fn remember(key: &[u8; super::store::KEY], below: &Arc<Below>) {
        let mut known = KNOWN.lock().unwrap();
        if known.len() >= KNOWN_AT_MOST {
            known.clear();
        }
        known.insert(*key, below.clone());
    }

    /// A line for each found: its kind, what, why and each call, all after tabs; where a
    /// call calls the next follows its name after a `\x1f`.
    fn write(&self) -> String {
        let field = |it: &str| it.replace(['\t', '\n', '\x1f'], " ");
        let mut out = format!("{}\n", Self::FORMAT);
        for it in &self.found {
            out.push_str(it.kind.name());
            for text in [&it.what, &it.why] {
                out.push('\t');
                out.push_str(&field(text));
            }
            for call in &it.path {
                out.push('\t');
                out.push_str(&field(&call.name));
                if let Some(at) = &call.at {
                    out.push('\x1f');
                    out.push_str(&field(at));
                }
            }
            out.push('\n');
        }
        out
    }

    fn read(bytes: &[u8]) -> Option<Below> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.lines();
        if lines.next()? != Self::FORMAT {
            return None;
        }
        let mut below = Below::default();
        for line in lines {
            let mut fields = line.split('\t');
            let kind = Kind::from_name(fields.next()?)?;
            let what = fields.next()?.to_owned();
            let why = fields.next()?.to_owned();
            let path = fields
                .map(|it| match it.split_once('\x1f') {
                    Some((name, at)) => Call { name: name.to_owned(), at: Some(at.to_owned()) },
                    None => Call { name: it.to_owned(), at: None },
                })
                .collect();
            below.found.push(Found { kind, what, why, path });
        }
        Some(below)
    }
}

/// The stable name `node` is taken whole by in a test's hash: that of a function, a closure
/// or a coroutine with nothing of the checkout in its name. `None` for anything else, and for
/// what has no stable name.
fn sealed_name(
    db: &dyn HirDatabase,
    names: &super::stable::StableNames<'_, '_>,
    node: &Node,
) -> Option<String> {
    let name = match node {
        Node::Root(func) => names.instance(&super::root_instance(db, *func))?,
        Node::Instance(instance, _) => match instance.def {
            InstanceDef::Fn(_)
            | InstanceDef::Closure(_)
            | InstanceDef::Coroutine(_)
            | InstanceDef::CoroutineByMove(_) => names.instance(instance)?,
            _ => return None,
        },
        Node::Static(_) | Node::Leaf(..) => return None,
    };
    (!super::stable::StableNames::of_checkout(&name)).then_some(name)
}

fn below_key(header: &impl std::fmt::Debug, name: &str) -> [u8; super::store::KEY] {
    use sha2::Digest;
    sha2::Sha256::digest(
        format!("tests reach below\n{}\n{header:?}\n{name}", Below::FORMAT).as_bytes(),
    )
    .into()
}

/// What `start`, a sealed node gone into, reaches, as [`Below`] has it: a sealed node below it
/// that has what is below it is not gone into again.
fn below_of(walk: &Walk<'_>, sealed: &FxHashMap<u32, Sealed>, start: u32) -> Below {
    let mut below = Below::default();
    let mut way = Way::default();
    way.clear(start);
    let mut seen = FxHashSet::from_iter([start]);
    let mut at = 0;
    while let Some(&node) = way.order.get(at) {
        let known = sealed.get(&node).and_then(|it| it.below.as_ref()).filter(|_| node != start);
        match known {
            Some(inner) => {
                for it in &inner.found {
                    if !below.found.iter().any(|seen| seen.kind == it.kind) {
                        let path = joined(walk.path_to(&way, at), &it.path);
                        below.found.push(Found { path, ..it.clone() });
                    }
                }
            }
            None => {
                for (kind, what, why) in walk.reasons(node) {
                    if !below.found.iter().any(|seen| seen.kind == kind) {
                        below.found.push(Found { kind, what, why, path: walk.path_to(&way, at) });
                    }
                }
                for edge in &walk.nodes[node as usize].edges {
                    if seen.insert(edge.to) {
                        way.push(edge.to, at, edge.site);
                    }
                }
            }
        }
        if below.found.len() == Kind::ALL.len() {
            break;
        }
        at += 1;
    }
    below.found.sort_by_key(|it| it.kind);
    below
}

/// What [`tests_reach`] says of a test.
pub struct Reach {
    /// Of its code and all it reaches; of what it reaches that can't be compiled, by name.
    pub hash: u128,
    /// The first of each [`Kind`] it reaches, by the fewest calls, in the order of the kinds.
    pub found: Vec<Found>,
}

impl Reach {
    /// Whether its end is a function of its code alone: nothing it reaches [`Kind::blocks`].
    pub fn hermetic(&self) -> bool {
        self.found.iter().all(|it| !it.kind.blocks())
    }
}

/// A way a test's end depends on more than its code.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Found {
    pub kind: Kind,
    /// What it reaches, by name: a symbol, a static, a function.
    pub what: String,
    /// What it reaches, and what that does.
    pub why: String,
    /// The fewest calls that take the test to it: the test, what it calls and so on.
    pub path: Vec<Call>,
}

/// A function on the way from a test to what it reaches.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Call {
    /// Its name without the types it is instantiated with.
    pub name: String,
    /// Where it calls the next one, `file:line:column`, when that is told.
    pub at: Option<String>,
}

/// `name` with each list of generic arguments after a `::` left out, as `…`.
fn without_arguments(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0usize;
    let mut previous = '\0';
    for c in name.chars() {
        match c {
            '<' if depth > 0 => depth += 1,
            '<' if out.ends_with("::") => {
                depth = 1;
                out.push_str("<…>");
            }
            '>' if depth > 0 && previous != '-' => depth -= 1,
            _ if depth > 0 => {}
            _ => out.push(c),
        }
        previous = c;
    }
    out
}

/// What a foreign function or static a test reaches does that the test's code does not
/// decide, by its symbol, and of which [`Kind`]: `None` for what gives the same for the same
/// input, as far as a test can tell. Locks are left out: a test that turns on how its threads
/// are scheduled is flaky whether it runs again or not.
fn outside(symbol: &str) -> Option<(Kind, &'static str)> {
    let name = symbol.trim_start_matches('_');
    let name = name.split('$').next().unwrap_or(name);
    let name = name.strip_suffix("64").unwrap_or(name);
    let kind = match name {
        "clock_gettime"
        | "gettimeofday"
        | "time"
        | "mach_absolute_time"
        | "mach_continuous_time"
        | "QueryPerformanceCounter"
        | "GetSystemTimePreciseAsFileTime"
        | "GetSystemTimeAsFileTime" => return Some((Kind::Clock, "reads the clock")),
        "getrandom"
        | "getentropy"
        | "arc4random"
        | "arc4random_buf"
        | "CCRandomGenerateBytes"
        | "SecRandomCopyBytes"
        | "BCryptGenRandom"
        | "ProcessPrng"
        | "RtlGenRandom" => {
            return Some((Kind::Random, "draws randomness"));
        }
        "pthread_create" | "CreateThread" => return Some((Kind::Thread, "starts a thread")),
        "exit" | "Exit" | "ExitProcess" => {
            return Some((Kind::Process, "ends the process, and the other tests with it"));
        }
        "getenv"
        | "secure_getenv"
        | "environ"
        | "NSGetEnviron"
        | "GetEnvironmentVariableW"
        | "GetEnvironmentStringsW"
        | "setenv"
        | "unsetenv"
        | "putenv"
        | "SetEnvironmentVariableW" => Kind::Env,
        "execve" | "execvp" | "execvpe" | "posix_spawn" | "posix_spawnp" | "fork" | "vfork"
        | "clone3" | "system" | "popen" | "CreateProcessW" => Kind::Process,
        "socket" | "connect" | "bind" | "accept" | "accept4" | "getaddrinfo" | "WSASocketW" => {
            Kind::Net
        }
        _ => Kind::Fs,
    };
    let what = match kind {
        Kind::Env => "reads or sets the environment",
        Kind::Process => "starts processes",
        Kind::Net => "uses the network",
        _ => file_call(name)?,
    };
    Some((kind, what))
}

/// What a foreign function of files does, by its name without `_` and `64`.
fn file_call(name: &str) -> Option<&'static str> {
    Some(match name {
        "open"
        | "openat"
        | "creat"
        | "opendir"
        | "fdopendir"
        | "readdir"
        | "stat"
        | "lstat"
        | "fstatat"
        | "statx"
        | "access"
        | "faccessat"
        | "faccessat2"
        | "readlink"
        | "readlinkat"
        | "realpath"
        | "getcwd"
        | "mkdir"
        | "mkdirat"
        | "unlink"
        | "unlinkat"
        | "rename"
        | "renameat"
        | "renameat2"
        | "rmdir"
        | "statfs"
        | "getattrlistbulk"
        | "CreateFileW"
        | "FindFirstFileW"
        | "GetFileAttributesExW" => "reads or writes files",
        _ => return None,
    })
}

/// What makes a test's end depend on more than its code, by kind, as `tests.ndjson` names
/// it. A test that reaches one that [`Kind::blocks`] is not hermetic.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Kind {
    Fs,
    Env,
    Net,
    Process,
    Clock,
    Random,
    Thread,
    Ffi,
    Asm,
    Indirect,
    Uncompiled,
    GlobalState,
}

impl Kind {
    pub const ALL: [Kind; 12] = [
        Kind::Fs,
        Kind::Env,
        Kind::Net,
        Kind::Process,
        Kind::Clock,
        Kind::Random,
        Kind::Thread,
        Kind::Ffi,
        Kind::Asm,
        Kind::Indirect,
        Kind::Uncompiled,
        Kind::GlobalState,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Fs => "fs",
            Kind::Env => "env",
            Kind::Net => "net",
            Kind::Process => "process",
            Kind::Clock => "clock",
            Kind::Random => "random",
            Kind::Thread => "thread",
            Kind::Ffi => "ffi",
            Kind::Asm => "asm",
            Kind::Indirect => "indirect",
            Kind::Uncompiled => "uncompiled",
            Kind::GlobalState => "global-state",
        }
    }

    pub fn from_name(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|it| it.name() == name)
    }

    /// Whether a test that reaches it is not hermetic, and runs every time. The clock,
    /// randomness and threads don't: a test whose pass turns on them is flaky, run again or
    /// not, so they are told (`nondeterministic`) and its pass is kept by its code.
    pub fn blocks(self) -> bool {
        !matches!(self, Kind::Clock | Kind::Random | Kind::Thread)
    }
}

/// The name of the function whose body `closure` is, as a backtrace has it.
pub(super) fn cached_name(db: &dyn HirDatabase, closure: &Instance) -> String {
    let InstanceDef::Closure(id) = closure.def else { return "?".to_owned() };
    match InternedClosureId::from_id(id).loc(db).owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => {
            function_name(db, func, closure.args.as_ref())
        }
        _ => "?".to_owned(),
    }
}

fn function_name(db: &dyn HirDatabase, func: FunctionId, args: GenericArgs<'_>) -> String {
    hir_ty_mir::mir::type_name::rustc_function_name(db, func, args.as_slice())
        .unwrap_or_else(|| FunctionSignature::of(db, func).name.as_str().to_owned())
}

pub(crate) fn crate_name(db: &dyn HirDatabase, krate: Crate) -> String {
    krate.extra_data(db).display_name.as_ref().map_or_else(|| "?".to_owned(), |it| it.to_string())
}

/// What the hash goes through: the functions and statics compiled, and what has no code of
/// ours, by its name.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Node {
    Root(FunctionId),
    Instance(Instance, StoredParamEnvAndCrate),
    Static(StaticId),
    /// A function or static known by its name alone, and what keeps it from being pure.
    Leaf(String, Option<String>),
}

/// What a node refers to the next by, to find where that is written.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Site {
    Func(FunctionId),
    Static(StaticId),
    None,
}

struct Edge {
    to: u32,
    site: Site,
    /// The edge is to how the arguments and result are written, which runs apart from the
    /// body, so what it does keeps nothing from being cached.
    codec: bool,
}

struct Seen {
    node: Node,
    edges: Vec<Edge>,
    why: Option<Why>,
}

/// What keeps a node from being pure.
struct Why {
    /// What the node does, as said of it: "dereferences a raw pointer". Of a foreign function
    /// or a static it is said of what uses it: "calls the foreign function `write`".
    text: String,
    of_user: bool,
    /// Where that is written, when that is told.
    place: Option<String>,
}

impl Why {
    fn own(text: impl Into<String>) -> Option<Why> {
        Some(Why { text: text.into(), of_user: false, place: None })
    }

    fn of_user(text: impl Into<String>) -> Option<Why> {
        Some(Why { text: text.into(), of_user: true, place: None })
    }
}

struct Walk<'db> {
    db: &'db dyn HirDatabase,
    homes: Homes,
    nodes: Vec<Seen>,
    index: FxHashMap<Node, u32>,
    /// What each node is when hashed, in the order the nodes were found.
    segments: Vec<u128>,
    /// Hashing for [`tests_reach`]: nothing is told of purity, every function of a crate
    /// that can change is gone into, and a segment leaves out where its edges go, which
    /// each root hashes in an order of its own.
    tests: bool,
    /// The nodes blaze can't compile, whose code tells nothing of what they do.
    uncompiled: FxHashSet<u32>,
    /// For [`tests_reach`], what each node is that makes a test's end depend on more than its
    /// code: of which kind, what by name, and why.
    kinds: FxHashMap<u32, (Kind, String, String)>,
    /// Where a node refers to the next by a site, as [`Walk::site_location`] found it.
    locations: std::cell::RefCell<FxHashMap<(u32, Site), Option<String>>>,
    /// An argument holds a reference count, which what reads one gives and the key does not
    /// tell: two arguments can share a value or not.
    counted: bool,
    /// What each function of core and alloc known by its name does that is not pure.
    library: FxHashMap<FunctionId, Option<String>>,
    /// Whether a static chooses a function by the CPU, as [`dispatch`] tells.
    dispatch: std::cell::RefCell<FxHashMap<StaticId, bool>>,
}

/// A body of core or alloc looked through: a function's, or a closure's.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum LibraryBody<'db> {
    Fn(FunctionId),
    Closure(InternedClosureId<'db>),
}

impl<'db> Walk<'db> {
    fn new(db: &'db dyn HirDatabase, counted: bool) -> Walk<'db> {
        Walk {
            db,
            homes: Homes::default(),
            nodes: Vec::new(),
            index: FxHashMap::default(),
            segments: Vec::new(),
            tests: false,
            uncompiled: FxHashSet::default(),
            kinds: FxHashMap::default(),
            locations: Default::default(),
            counted,
            library: FxHashMap::default(),
            dispatch: Default::default(),
        }
    }

    fn reach(&mut self, node: Node) -> u32 {
        if let Some(&at) = self.index.get(&node) {
            return at;
        }
        let at = self.nodes.len() as u32;
        let why = match &node {
            Node::Leaf(_, Some(why)) => Why::of_user(why.clone()),
            _ => None,
        };
        self.nodes.push(Seen { node: node.clone(), edges: Vec::new(), why });
        self.index.insert(node, at);
        at
    }

    fn edge(&mut self, from: u32, node: Node, site: Site, codec: bool) -> u32 {
        let to = self.reach(node);
        self.nodes[from as usize].edges.push(Edge { to, site, codec });
        to
    }

    /// What a function an instance names is in the walk, when the node `from` calls it.
    fn function(
        &mut self,
        from: u32,
        instance: &Instance,
        env: &StoredParamEnvAndCrate,
    ) -> (Node, Site, bool) {
        let db = self.db;
        let InstanceDef::Fn(func) = instance.def else {
            let site = match instance.def {
                InstanceDef::VtableShim(func) => Site::Func(func),
                _ => Site::None,
            };
            return (Node::Instance(instance.clone(), env.clone()), site, false);
        };
        let site = Site::Func(func);
        if let Some(closure) = cached_closure(db, func) {
            // Another cached function, or this one called again: its body is what it runs
            // when its result is not kept, and what it reaches from there is the body's.
            let closure = Instance { def: InstanceDef::Closure(closure), ..instance.clone() };
            let env = self.homes.home(db, &closure, env);
            return (Node::Instance(closure, env), site, false);
        }
        let name = || function_name(db, func, instance.args.as_ref());
        if let Some(symbol) = link_name(db, func) {
            let why =
                (!pure_foreign(&symbol)).then(|| format!("calls the foreign function `{symbol}`"));
            return (Node::Leaf(symbol, why), site, false);
        }
        if taken_from_rustc(db, func) {
            return (Node::Leaf(name(), None), site, false);
        }
        if self.tests {
            // A seed is randomness, which is no more outside a test than the time is. A
            // backtrace is shown, not a result: what it reads of the environment and the
            // working directory changes what a failure prints.
            // The stack a thread is started with is sized by `RUST_MIN_STACK`, which a test
            // can't tell but by running out of it.
            if makes_seed(db, func)
                || of_backtrace(db, func)
                || sizes_stack(db, func)
                || (self.sizes_stack_at(from) && is_std_function(db, func, "var_os"))
            {
                return (Node::Leaf(name(), None), site, false);
            }
            // What a test reaches is all that decides how it ends, so only the toolchain's
            // code, which the hash has by its commit, is known by name.
            let named = instance.args.as_ref().iter().all(|it| it.region().is_some());
            if !named {
                return (Node::Instance(instance.clone(), env.clone()), site, false);
            }
            return match func.krate(db).data(db).origin {
                base_db::CrateOrigin::Lang(LangCrateOrigin::Core | LangCrateOrigin::Alloc) => {
                    (Node::Leaf(name(), None), site, false)
                }
                // A panic ends the test alone; `exit` ends every test of the process.
                base_db::CrateOrigin::Lang(_)
                    if diverges(db, func) && !is_std_function(db, func, "exit") =>
                {
                    (Node::Leaf(name(), None), site, false)
                }
                _ => (Node::Root(func), site, false),
            };
        }
        if diverges(db, func) {
            // A panic: what it prints is no result.
            return (Node::Leaf(name(), None), site, false);
        }
        let told = |what: &str| {
            let name = name();
            let why = format!("calls `{name}`, which {what}");
            (Node::Leaf(name, Some(why)), site, false)
        };
        if let Some(what) = gives_address(db, func, self.is_own(from)) {
            return told(what);
        }
        if makes_seed(db, func) {
            // A map's seed orders its entries alone, and what gives them in that order is
            // refused (`seeded_order`).
            if self.in_a_map(from) {
                return (Node::Leaf(name(), None), site, false);
            }
            return told("makes a random seed");
        }
        if let Some(what) = seeded_order(db, func, instance.args.as_ref()) {
            return told(what);
        }
        if self.counted
            && let Some(what) = reads_count(db, func)
        {
            return told(what);
        }
        let codec = is_codec(db, func);
        let named = instance.args.as_ref().iter().all(|it| it.region().is_some());
        if !named {
            return (Node::Instance(instance.clone(), env.clone()), site, codec);
        }
        match func.krate(db).data(db).origin {
            base_db::CrateOrigin::Lang(LangCrateOrigin::Core | LangCrateOrigin::Alloc) => {
                let why =
                    self.library(func).map(|what| format!("calls `{}`, which {what}", name()));
                (Node::Leaf(name(), why), site, codec)
            }
            _ => (Node::Root(func), site, codec),
        }
    }

    /// What a function of core or alloc, known by its name and not hashed, does that is not
    /// pure, as its MIR and that of the core and alloc it calls tell: a foreign function, a
    /// static that changes, a thread-local or assembly. Not looked through are what a generic
    /// body calls by a trait of its parameters, what a `dyn` calls, which the code reached
    /// makes the vtable of, and what dropping a value does.
    fn library(&mut self, func: FunctionId) -> Option<String> {
        if let Some(it) = self.library.get(&func) {
            return it.clone();
        }
        let db = self.db;
        let start = LibraryBody::Fn(func);
        let mut parent = FxHashMap::default();
        let mut seen = FxHashSet::from_iter([start]);
        let mut queue = std::collections::VecDeque::from([start]);
        let mut calls = Vec::new();
        let mut found = None;
        while let Some(at) = queue.pop_front() {
            calls.clear();
            if let Some(what) = library_body(db, at, &mut calls) {
                found = Some((at, what));
                break;
            }
            for &next in &calls {
                if seen.insert(next) {
                    parent.insert(next, at);
                    queue.push_back(next);
                }
            }
        }
        let Some((at, what)) = found else {
            // What it reaches is as pure as it is.
            for body in seen {
                if let LibraryBody::Fn(it) = body {
                    self.library.insert(it, None);
                }
            }
            return None;
        };
        let mut chain = vec![at];
        while let Some(&up) = parent.get(chain.last().unwrap()) {
            chain.push(up);
        }
        chain.pop();
        chain.reverse();
        let told = match chain.as_slice() {
            [] => what,
            _ => {
                let names: Vec<String> = chain.iter().map(|&it| library_name(db, it)).collect();
                let shown = match names.len() {
                    ..=3 => names.join(" → "),
                    _ => format!("{} → … → {}", names[0], names[names.len() - 1]),
                };
                format!("reaches {shown}, which {what}")
            }
        };
        self.library.insert(func, Some(told.clone()));
        Some(told)
    }

    fn hash_node(&mut self, at: u32, node: &Node) {
        let db = self.db;
        if std::env::var_os("BLAZE_FN_CACHE_TRACE").is_some() {
            eprintln!("fn-cache: {at} {}", describe(db, node));
        }
        let mut hasher = rustc_stable_hash::StableSipHasher128::new();
        match node {
            Node::Root(func) => {
                0u8.hash(&mut hasher);
                match compiled_root(db, *func) {
                    Ok(compiled) => {
                        self.function_code(at, &mut hasher, compiled);
                        self.look_into(at, InferBodyId::DefWithBodyId((*func).into()), None);
                    }
                    Err(e) => self.cannot(at, &mut hasher, e),
                }
            }
            Node::Instance(instance, env) => {
                1u8.hash(&mut hasher);
                match compiled_instance(db, instance.clone(), env.clone()) {
                    Ok(compiled) => {
                        self.function_code(at, &mut hasher, compiled);
                        match instance.def {
                            InstanceDef::Fn(func) => {
                                self.look_into(at, InferBodyId::DefWithBodyId(func.into()), None)
                            }
                            InstanceDef::Closure(id) => {
                                let closure = InternedClosureId::from_id(id);
                                self.look_into(at, closure.loc(db).owner, Some(closure));
                            }
                            _ => {}
                        }
                    }
                    Err(e) => self.cannot(at, &mut hasher, e),
                }
            }
            Node::Static(id) => {
                2u8.hash(&mut hasher);
                match compiled_static(db, *id) {
                    Ok(compiled) => {
                        if compiled.writable {
                            let why = format!("reads {}, which can change", describe(db, node));
                            self.nodes[at as usize].why = Why::of_user(why);
                        }
                        if self.tests
                            && let Some(reason) = self.static_reason(*id, compiled.writable)
                        {
                            self.kinds.entry(at).or_insert(reason);
                        }
                        (&compiled.blob, compiled.writable).hash(&mut hasher);
                        let own = self.is_own(at);
                        self.requests(at, &mut hasher, &compiled.requests, own);
                    }
                    Err(e) => self.cannot(at, &mut hasher, e),
                }
            }
            Node::Leaf(name, _) => (3u8, name).hash(&mut hasher),
        }
        let type_id::Fingerprint(low, high) = hasher.finish();
        self.segments.push(u128::from(low) | u128::from(high) << 64);
    }

    /// Notes that the node did not compile: nothing tells what it does then.
    fn cannot(&mut self, at: u32, hasher: &mut impl Hasher, e: &CodegenError) {
        self.uncompiled.insert(at);
        self.nodes[at as usize].why = Why::own(format!("can't be compiled by blaze ({e})"));
        e.to_string().hash(hasher);
    }

    fn function_code(&mut self, at: u32, hasher: &mut impl Hasher, compiled: &CompiledFunction) {
        hash_code(hasher, &compiled.code);
        let own = self.is_own(at);
        self.requests(at, hasher, &compiled.requests, own);
    }

    /// Whether the node is code of a crate of the workspace, which is held to more than the
    /// rest: it is the code the user is told to look at.
    fn is_own(&self, at: u32) -> bool {
        let db = self.db;
        let krate = match &self.nodes[at as usize].node {
            Node::Root(func) => func.krate(db),
            Node::Instance(instance, _) => match instance.def {
                InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => func.krate(db),
                InstanceDef::Closure(id) => {
                    InternedClosureId::from_id(id).loc(db).owner.module(db).krate(db)
                }
                InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
                    InternedCoroutineId::from_id(id).loc(db).owner.module(db).krate(db)
                }
                _ => return false,
            },
            Node::Static(id) => id.krate(db),
            Node::Leaf(..) => return false,
        };
        krate.data(db).origin.is_local()
    }

    /// Whether the node is code of std's `HashMap` or `HashSet`, or of the sysroot's
    /// `hashbrown` they are made of: a seed made there is the seed of a map.
    fn in_a_map(&self, at: u32) -> bool {
        let db = self.db;
        let func = match &self.nodes[at as usize].node {
            Node::Root(func) => *func,
            Node::Instance(instance, _) => match instance.def {
                InstanceDef::Fn(func) => func,
                _ => return false,
            },
            _ => return false,
        };
        let krate = func.krate(db);
        match krate.data(db).origin {
            base_db::CrateOrigin::Lang(LangCrateOrigin::Std) => {
                let ItemContainerId::ImplId(id) = func.lookup(db).container else { return false };
                hasher_of(db, db.impl_self_ty(id).skip_binder()).is_some()
            }
            base_db::CrateOrigin::Lang(_) => crate_name(db, krate) == "hashbrown",
            _ => false,
        }
    }

    fn requests(&mut self, at: u32, hasher: &mut impl Hasher, requests: &Requests, own: bool) {
        let db = self.db;
        requests.funcs.len().hash(hasher);
        for request in &requests.funcs {
            request.sig.hash(hasher);
            match &request.target {
                FuncTarget::Symbol(symbol) => (0u8, symbol).hash(hasher),
                FuncTarget::Path(path) => (1u8, path).hash(hasher),
                FuncTarget::Asm(text) => {
                    (2u8, text).hash(hasher);
                    if own && self.nodes[at as usize].why.is_none() {
                        self.nodes[at as usize].why = Why::own("has assembly in it");
                    }
                    if self.tests && !self.in_toolchain(at) {
                        let what = describe(db, &self.nodes[at as usize].node);
                        let why = format!("it reaches {what}, which has assembly in it");
                        let what = without_arguments(what.trim_matches('`'));
                        self.kinds.entry(at).or_insert((Kind::Asm, what, why));
                    }
                }
                FuncTarget::Instance(instance, env) => {
                    let (node, site, codec) = self.function(at, instance, env);
                    let to = self.edge(at, node, site, codec);
                    3u8.hash(hasher);
                    if !self.tests {
                        to.hash(hasher);
                    } else if let Some(reason) = self.called_reason(instance) {
                        self.kinds.entry(to).or_insert(reason);
                    }
                }
            }
        }
        requests.datas.len().hash(hasher);
        for request in &requests.datas {
            match request {
                DataRequest::Blob { blob, identity } => (0u8, blob, identity).hash(hasher),
                DataRequest::Vtable { ty, trait_ref, blob } => {
                    1u8.hash(hasher);
                    hir_ty_mir::mir::type_name::rustc_type_name(db, ty.as_ref()).hash(hasher);
                    trait_ref
                        .as_ref()
                        .map(|(id, args)| {
                            let name = TraitSignature::of(db, *id).name.as_str().to_owned();
                            let args: Vec<_> = args
                                .as_ref()
                                .iter()
                                .map(|it| {
                                    it.ty().and_then(|ty| {
                                        hir_ty_mir::mir::type_name::rustc_type_name(db, ty)
                                    })
                                })
                                .collect();
                            (crate_name(db, id.krate(db)), name, args)
                        })
                        .hash(hasher);
                    blob.hash(hasher);
                }
                DataRequest::Static(id) => {
                    let node = self.static_node(*id);
                    let to = self.edge(at, node, Site::Static(*id), false);
                    2u8.hash(hasher);
                    if !self.tests {
                        to.hash(hasher);
                    } else if foreign_static(db, *id).is_some()
                        && let Some(reason) = self.static_reason(*id, false)
                    {
                        self.kinds.entry(to).or_insert(reason);
                    }
                }
                // Where code is written, which the hash leaves out.
                DataRequest::Location(_) => 3u8.hash(hasher),
                DataRequest::Declaration(_) => 4u8.hash(hasher),
                DataRequest::FilePath(_) => 5u8.hash(hasher),
                DataRequest::Text(parts) => {
                    6u8.hash(hasher);
                    for part in parts {
                        match part {
                            TextPart::Text(text) => (0u8, text).hash(hasher),
                            TextPart::FilePath(_) => 1u8.hash(hasher),
                        }
                    }
                }
            }
        }
    }

    fn static_node(&mut self, id: StaticId) -> Node {
        let db = self.db;
        if let Some(name) = foreign_static(db, id) {
            let why = (!name.starts_with("__rust_"))
                .then(|| format!("reads the foreign static `{name}`"));
            return Node::Leaf(name, why);
        }
        if is_thread_local(db, id) {
            let name = static_name(db, id);
            return Node::Leaf(name.clone(), Some(format!("uses the thread-local `{name}`")));
        }
        Node::Static(id)
    }

    /// Looks through the MIR of code of the workspace for what its machine code does not
    /// tell: a raw pointer dereferenced, or an address taken as a number.
    fn look_into(
        &mut self,
        at: u32,
        owner: InferBodyId<'db>,
        closure: Option<InternedClosureId<'db>>,
    ) {
        let looked = match self.tests {
            true => !self.in_toolchain(at),
            false => self.is_own(at) && self.nodes[at as usize].why.is_none(),
        };
        if !looked {
            return;
        }
        let db = self.db;
        let body = match closure {
            Some(closure) => db.mir_body_for_closure(closure),
            None => db.mir_body(owner),
        };
        let Ok(body) = body else { return };
        if self.tests {
            let cache = |id| self.dispatch_cache(id);
            if let Some(span) = fn_from_data(db, body, &cache) {
                let place = location(db, body.owner, span)
                    .map(|it| format!(", at {it}"))
                    .unwrap_or_default();
                let what = describe(db, &self.nodes[at as usize].node);
                let why = format!(
                    "it reaches {what}, which calls a function pointer made of data (a \
                     `transmute`){place}"
                );
                let what = without_arguments(what.trim_matches('`'));
                self.kinds.entry(at).or_insert((Kind::Indirect, what, why));
            }
            return;
        }
        if let Some((why, span)) = raw_use(body) {
            let place = location(db, body.owner, span);
            self.nodes[at as usize].why = Some(Why { text: why.to_owned(), of_user: false, place });
        }
    }

    /// What node `at` is that makes a test's end depend on more than its code, for
    /// [`tests_reach`]: of which kind, what by name, and why.
    fn reasons(&self, at: u32) -> Vec<(Kind, String, String)> {
        let mut reasons = Vec::new();
        if let Some(it) = self.kinds.get(&at) {
            reasons.push(it.clone());
        }
        if self.uncompiled.contains(&at) {
            let what = describe(self.db, &self.nodes[at as usize].node);
            let why = format!("it reaches {what}, which blaze can't compile");
            reasons.push((Kind::Uncompiled, what.trim_matches('`').to_owned(), why));
        }
        reasons
    }

    /// The fewest calls from the root of `way` to its node at `at`, each by its name without
    /// the types it is instantiated with, and where it calls the next.
    fn path_to(&self, way: &Way, at: usize) -> Vec<Call> {
        let mut path = vec![at];
        while path.last().is_some_and(|&it| it != 0) {
            path.push(way.from[path[path.len() - 1]] as usize);
        }
        path.reverse();
        (0..path.len())
            .map(|i| {
                let node = way.order[path[i]];
                let name = describe(self.db, &self.nodes[node as usize].node);
                let at = path.get(i + 1).and_then(|&next| {
                    let site = way.via[next];
                    let mut locations = self.locations.borrow_mut();
                    locations
                        .entry((node, site))
                        .or_insert_with(|| self.site_location(node, site))
                        .clone()
                });
                Call { name: without_arguments(name.trim_matches('`')), at }
            })
            .collect()
    }

    /// Whether the node is code of the toolchain (std, core, alloc and the crates they are
    /// built of), whose foreign calls and assembly are told only where they are known.
    fn in_toolchain(&self, at: u32) -> bool {
        let db = self.db;
        let krate = match &self.nodes[at as usize].node {
            Node::Root(func) => func.krate(db),
            Node::Instance(instance, _) => match instance.def {
                InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => func.krate(db),
                InstanceDef::Closure(id) => {
                    InternedClosureId::from_id(id).loc(db).owner.module(db).krate(db)
                }
                InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
                    InternedCoroutineId::from_id(id).loc(db).owner.module(db).krate(db)
                }
                _ => return true,
            },
            Node::Static(id) => id.krate(db),
            Node::Leaf(..) => return true,
        };
        krate.data(db).origin.is_lang()
    }

    /// Whether the node is the closure std's `spawn_unchecked` sizes a thread's stack in, by
    /// `RUST_MIN_STACK` (what `min_stack` was).
    fn sizes_stack_at(&self, at: u32) -> bool {
        let db = self.db;
        let Node::Instance(instance, _) = &self.nodes[at as usize].node else { return false };
        let InstanceDef::Closure(id) = instance.def else { return false };
        InternedClosureId::from_id(id)
            .loc(db)
            .owner
            .as_function()
            .is_some_and(|owner| is_std_function(db, owner, "spawn_unchecked"))
    }

    /// What a call a test reaches makes its end depend on, by kind, for [`tests_reach`]: a
    /// foreign function the test's code does not decide, or a seed.
    fn called_reason(&self, instance: &Instance) -> Option<(Kind, String, String)> {
        let db = self.db;
        let InstanceDef::Fn(func) = instance.def else { return None };
        if let Some(symbol) = link_name(db, func) {
            if let Some((kind, what)) = outside(&symbol) {
                let why = format!("it reaches `{symbol}`, which {what}");
                return Some((kind, symbol, why));
            }
            if pure_foreign(&symbol)
                || locates_errno(&symbol)
                || func.krate(db).data(db).origin.is_lang()
            {
                return None;
            }
            let why =
                format!("it calls the foreign function `{symbol}`, whose code blaze can't see");
            return Some((Kind::Ffi, symbol, why));
        }
        if makes_seed(db, func) {
            let name = function_name(db, func, instance.args.as_ref());
            let why = format!("it reaches `{name}`, which makes a random seed (a `HashMap`'s)");
            return Some((Kind::Random, name, why));
        }
        None
    }

    fn dispatch_cache(&self, id: StaticId) -> bool {
        if let Some(&it) = self.dispatch.borrow().get(&id) {
            return it;
        }
        let it = dispatch::is_cache(self.db, id);
        self.dispatch.borrow_mut().insert(id, it);
        it
    }

    /// What a static a test reaches makes its end depend on, by kind: a foreign static, or one
    /// of a crate outside the toolchain that other tests in the process can change.
    fn static_reason(&self, id: StaticId, writable: bool) -> Option<(Kind, String, String)> {
        let db = self.db;
        let toolchain = id.krate(db).data(db).origin.is_lang();
        if let Some(symbol) = foreign_static(db, id) {
            if let Some((kind, what)) = outside(&symbol) {
                return Some((
                    kind,
                    symbol.clone(),
                    format!("it reaches `{symbol}`, which {what}"),
                ));
            }
            if toolchain || symbol.starts_with("__rust_") {
                return None;
            }
            let why =
                format!("it reads the foreign static `{symbol}`, which foreign code can change");
            return Some((Kind::Ffi, symbol, why));
        }
        let mutable = StaticSignature::of(db, id).flags.contains(StaticFlags::MUTABLE);
        if toolchain
            || !writable
            || is_thread_local(db, id)
            || !(mutable || shared(db, db.type_for_static(id).skip_binder()))
            || self.dispatch_cache(id)
        {
            return None;
        }
        let name = static_name(db, id);
        let why = format!(
            "it uses the static `{name}`, which can change, and other tests of the process with it"
        );
        Some((Kind::GlobalState, name, why))
    }

    /// Why what `from` reaches is not pure, by the shortest way to the first that is not:
    /// the edges to the codec are left out.
    fn impure(&self, from: u32) -> Option<String> {
        let mut parent: FxHashMap<u32, (u32, usize)> = FxHashMap::default();
        let mut seen = FxHashSet::default();
        let mut queue = std::collections::VecDeque::from([from]);
        seen.insert(from);
        while let Some(at) = queue.pop_front() {
            if let Some(why) = &self.nodes[at as usize].why {
                return Some(self.told(at, &parent, why));
            }
            // A `const fn` with no generic arguments that is const-pure reaches nothing that is
            // not: what a generic one calls by a trait can be a non-const `impl` at run time.
            if let Node::Root(func) = self.nodes[at as usize].node
                && is_const_pure(self.db, func)
            {
                continue;
            }
            for (i, edge) in self.nodes[at as usize].edges.iter().enumerate() {
                if !edge.codec && seen.insert(edge.to) {
                    parent.insert(edge.to, (at, i));
                    queue.push_back(edge.to);
                }
            }
        }
        None
    }

    /// Says why `at`, found by way of `parent`, is not pure: "it reaches `a` → `b`, which
    /// calls the foreign function `write`, at src/lib.rs:4:5".
    fn told(&self, at: u32, parent: &FxHashMap<u32, (u32, usize)>, why: &Why) -> String {
        let db = self.db;
        let mut chain = vec![at];
        while let Some(&(up, _)) = parent.get(chain.last().unwrap()) {
            chain.push(up);
        }
        chain.reverse();
        // Where the last code of the workspace on the way refers to the next.
        let place = why.place.clone().or_else(|| {
            chain.windows(2).rev().find_map(|pair| {
                if !self.is_own(pair[0]) {
                    return None;
                }
                let (_, edge) = parent[&pair[1]];
                let site = self.nodes[pair[0] as usize].edges[edge].site;
                self.site_location(pair[0], site)
            })
        });
        // The cached function is "it", and what a foreign function or a static is said of
        // is what uses it.
        let named = &chain[1..chain.len() - usize::from(why.of_user)];
        let mut told = match named {
            [] => format!("it {}", why.text),
            _ => {
                let names: Vec<String> =
                    named.iter().map(|&it| describe(db, &self.nodes[it as usize].node)).collect();
                let shown = match names.len() {
                    ..=4 => names.join(" → "),
                    _ => format!("{} → … → {}", names[..2].join(" → "), names[names.len() - 1]),
                };
                format!("it reaches {shown}, which {}", why.text)
            }
        };
        if let Some(place) = place {
            told.push_str(&format!(", at {place}"));
        }
        told
    }

    fn site_location(&self, at: u32, site: Site) -> Option<String> {
        let db = self.db;
        let body = match &self.nodes[at as usize].node {
            Node::Root(func) => db.mir_body((*func).into()).ok()?,
            Node::Instance(instance, _) => match instance.def {
                InstanceDef::Fn(func) => db.mir_body(func.into()).ok()?,
                InstanceDef::Closure(id) => {
                    db.mir_body_for_closure(InternedClosureId::from_id(id)).ok()?
                }
                _ => return None,
            },
            _ => return None,
        };
        let refers = |operand: &Operand| match (site, &operand.kind) {
            (Site::Func(func), _) => fn_item(operand).is_some_and(|(it, _)| it == func),
            (Site::Static(id), OperandKind::Static(it)) => *it == id,
            _ => false,
        };
        for (_, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                if let StatementKind::Assign(_, rvalue) = &statement.kind
                    && operands(rvalue).into_iter().any(refers)
                {
                    return location(db, body.owner, statement.span);
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            if let TerminatorKind::Call { func, args, .. } = &terminator.kind
                && (refers(func) || args.iter().any(refers))
            {
                return location(db, body.owner, terminator.span);
            }
        }
        None
    }
}

/// The foreign functions that give what they are given alone: the allocator, Rust's own
/// shims, libm and LLVM's intrinsics but those that read a counter, randomness, an address
/// or state of the machine, or set it.
fn pure_foreign(symbol: &str) -> bool {
    const MACHINE: &[&str] = &[
        "rdtsc",
        "rdpmc",
        "rdpid",
        "rdpru",
        "rdrand",
        "rdseed",
        "rndr",
        "irg",
        "readcyclecounter",
        "readsteadycounter",
        "xgetbv",
        "xsave",
        "xrstor",
        "mxcsr",
        "fpcr",
        "fpsr",
        "fpenv",
        "rounding",
        "flags.read",
        "flags.write",
        "read_register",
        "write_register",
        "fsbase",
        "gsbase",
        "pkru",
        "rdssp",
        "address",
        "thread.pointer",
        "stacksave",
        "sponentry",
    ];
    const LIBM: &[&str] = &[
        "acos",
        "asin",
        "atan",
        "atan2",
        "cbrt",
        "ceil",
        "cos",
        "cosh",
        "exp",
        "exp2",
        "expm1",
        "fabs",
        "fdim",
        "floor",
        "fma",
        "fmax",
        "fmin",
        "fmod",
        "hypot",
        "log",
        "log10",
        "log1p",
        "log2",
        "pow",
        "round",
        "sin",
        "sinh",
        "sqrt",
        "tan",
        "tanh",
        "tgamma",
        "lgamma_r",
        "trunc",
        "erf",
        "erfc",
        "ldexp",
        "frexp",
        "nextafter",
        "remainder",
    ];
    symbol.starts_with("__rust_")
        || symbol.starts_with("__rdl_")
        || symbol.starts_with("__rg_")
        || matches!(symbol, "memcpy" | "memmove" | "memset" | "memcmp" | "bcmp" | "strlen")
        || LIBM.iter().any(|it| symbol == *it || symbol.strip_suffix('f') == Some(it))
        || symbol.strip_prefix("llvm.").is_some_and(|it| !MACHINE.iter().any(|m| it.contains(m)))
}

/// The C library's functions that give where this thread's `errno` is: what is read there was
/// set by the foreign calls before it, each of which is told by what it is. Not pure for a
/// cached function, whose result would keep the address.
fn locates_errno(symbol: &str) -> bool {
    matches!(symbol, "__errno_location" | "__error" | "__errno" | "_errno")
}

/// What a function of core does that gives another result in another run, where the same
/// values are at other addresses: an address as a number, or pointers formatted, hashed or
/// ordered by theirs. Whether two pointers are equal is the same in every run, so equality is
/// not one. core is otherwise trusted, and its MIR not looked through: in it addresses are
/// also numbers where what it gives doesn't depend on them, as in slice iteration and `memchr`.
/// So `addr` and `expose_provenance` count where code of the workspace calls them (`own`), as
/// its casts do (`raw_use`): core and alloc call them to align and to tell a dangling pointer.
fn gives_address(db: &dyn HirDatabase, func: FunctionId, own: bool) -> Option<&'static str> {
    if !matches!(func.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Core))
    {
        return None;
    }
    let signature = FunctionSignature::of(db, func);
    let name = signature.name.as_str();
    match func.lookup(db).container {
        ItemContainerId::ModuleId(_) => {
            matches!(name, "pointer_fmt_inner" | "wide_pointer_fmt_inner")
                .then_some("formats an address")
        }
        ItemContainerId::ImplId(id) => {
            if !matches!(db.impl_self_ty(id).skip_binder().kind(), TyKind::RawPtr(..)) {
                return None;
            }
            let Some(trait_ref) = db.impl_trait(id) else {
                return (own && matches!(name, "addr" | "expose_provenance"))
                    .then_some("gives an address as a number");
            };
            match TraitSignature::of(db, trait_ref.skip_binder().def_id.0).name.as_str() {
                "Hash" => Some("hashes an address"),
                "PartialOrd" | "Ord" => Some("orders pointers by their addresses"),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether a call of `func` is to stay a call, not have the callee built in where it is
/// called: the walk knows what it does by the function called, and a body built into its
/// caller is not called. `own` is whether the caller is code of the workspace.
pub(super) fn kept_a_call(
    db: &dyn HirDatabase,
    func: FunctionId,
    args: GenericArgs<'_>,
    own: bool,
) -> bool {
    gives_address(db, func, own).is_some()
        || makes_seed(db, func)
        || seeded_order(db, func, args).is_some()
        || reads_count(db, func).is_some()
}

/// Whether `func` makes a `RandomState`, whose seed is another in every run.
fn makes_seed(db: &dyn HirDatabase, func: FunctionId) -> bool {
    if !matches!(func.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Std)) {
        return false;
    }
    let ItemContainerId::ImplId(id) = func.lookup(db).container else { return false };
    if !is_std_struct(db, db.impl_self_ty(id).skip_binder(), "RandomState") {
        return false;
    }
    let name = &FunctionSignature::of(db, func).name;
    match db.impl_trait(id) {
        None => name.as_str() == "new",
        Some(trait_ref) => {
            TraitSignature::of(db, trait_ref.skip_binder().def_id.0).name.as_str() == "Default"
        }
    }
}

/// What a function of std's `HashMap` or `HashSet` of `RandomState` does that tells the order
/// of its entries, which the seed decides, or gives the seed itself. What only looks up,
/// inserts or counts gives the same whatever the seed.
fn seeded_order(
    db: &dyn HirDatabase,
    func: FunctionId,
    args: GenericArgs<'_>,
) -> Option<&'static str> {
    if !matches!(func.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Std)) {
        return None;
    }
    let ItemContainerId::ImplId(id) = func.lookup(db).container else { return None };
    let interner = DbInterner::new_no_crate(db);
    let parent = hir_ty::generics::generics(db, func.into()).len_parent();
    let parent = GenericArgs::new_from_slice(args.as_slice().get(..parent)?);
    let self_ty = db.impl_self_ty(id).instantiate(interner, parent).skip_norm_wip();
    let hasher = hasher_of(db, self_ty)?;
    if !is_std_struct(db, hasher, "RandomState") {
        return None;
    }
    let name = &FunctionSignature::of(db, func).name;
    let in_order = match db.impl_trait(id) {
        None => matches!(
            name.as_str(),
            "iter"
                | "iter_mut"
                | "keys"
                | "values"
                | "values_mut"
                | "into_keys"
                | "into_values"
                | "drain"
                | "retain"
                | "extract_if"
                | "difference"
                | "symmetric_difference"
                | "intersection"
                | "union"
        ),
        // A `&HashMap`'s is the map's `iter`, reached as that.
        Some(trait_ref) => {
            TraitSignature::of(db, trait_ref.skip_binder().def_id.0).name.as_str() == "IntoIterator"
        }
    };
    if in_order {
        return Some("gives the entries of a map in the order of its random seed");
    }
    (db.impl_trait(id).is_none() && name.as_str() == "hasher")
        .then_some("gives a map's random seed")
}

/// The hasher of std's `HashMap` or `HashSet` when `ty` is one.
fn hasher_of<'db>(db: &dyn HirDatabase, ty: Ty<'db>) -> Option<Ty<'db>> {
    let TyKind::Adt(def, args) = ty.kind() else { return None };
    let AdtId::StructId(id) = def.def_id() else { return None };
    if !matches!(id.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Std)) {
        return None;
    }
    let at = match StructSignature::of(db, id).name.as_str() {
        "HashMap" => 2,
        "HashSet" => 1,
        _ => return None,
    };
    args.as_slice().get(at)?.ty()
}

/// Whether `func` is a method of std's `Backtrace`, or of a trait for it.
fn of_backtrace(db: &dyn HirDatabase, func: FunctionId) -> bool {
    if !matches!(func.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Std)) {
        return false;
    }
    let ItemContainerId::ImplId(id) = func.lookup(db).container else { return false };
    is_std_struct(db, db.impl_self_ty(id).skip_binder(), "Backtrace")
}

/// Whether a static of `ty` that can change is state other tests of the process can change:
/// all but a cell set once (`LazyLock`, `OnceLock`, once_cell's and lazy_static's), which code
/// the test reaches sets to what that code gives, unless it holds a lock, a cell or an atomic.
fn shared<'db>(db: &dyn HirDatabase, ty: Ty<'db>) -> bool {
    const ONCE: &[&str] = &["LazyLock", "OnceLock", "LazyCell", "OnceCell", "Lazy", "Once"];
    const CHANGES: &[&str] =
        &["Mutex", "RwLock", "ReentrantMutex", "RefCell", "Cell", "UnsafeCell", "SyncUnsafeCell"];
    let struct_name = |ty: Ty<'db>| {
        let TyKind::Adt(def, args) = ty.kind() else { return None };
        let AdtId::StructId(id) = def.def_id() else { return None };
        Some((StructSignature::of(db, id).name.as_str().to_owned(), args))
    };
    let Some((name, args)) = struct_name(ty) else { return true };
    if !ONCE.contains(&name.as_str()) {
        return true;
    }
    let Some(inner) = args.iter().find_map(|it| it.ty()) else { return false };
    struct_name(inner)
        .is_some_and(|(name, _)| CHANGES.contains(&name.as_str()) || name.starts_with("Atomic"))
}

/// Where `body` makes a function pointer of data, with a `transmute`: what it calls through
/// it is no function the code reached names, and can be any. Not one of what it loaded from a
/// static `cache` says chooses a function ([`dispatch`]).
fn fn_from_data(
    db: &dyn HirDatabase,
    body: &MirBody<'_>,
    cache: &dyn Fn(StaticId) -> bool,
) -> Option<MirSpan> {
    for (_, block) in body.basic_blocks.iter() {
        let Some(terminator) = &block.terminator else { continue };
        let TerminatorKind::Call { func, args, destination, .. } = &terminator.kind else {
            continue;
        };
        let Some((callee, _)) = fn_item(func) else { continue };
        let name = FunctionSignature::of(db, callee).name.as_str().to_owned();
        if FunctionSignature::is_intrinsic(db, callee)
            && matches!(name.as_str(), "transmute" | "transmute_unchecked")
            && destination.projection.as_slice().is_empty()
            && matches!(body.locals[destination.local].ty.as_ref().kind(), TyKind::FnPtr(..))
            && !args.first().is_some_and(|it| dispatch::loaded_from(db, body, it, cache))
        {
            return Some(terminator.span);
        }
    }
    None
}

/// Whether `func` is std's `min_stack`, which reads `RUST_MIN_STACK` for a thread's stack.
fn sizes_stack(db: &dyn HirDatabase, func: FunctionId) -> bool {
    is_std_function(db, func, "min_stack")
}

fn is_std_function(db: &dyn HirDatabase, func: FunctionId, name: &str) -> bool {
    matches!(func.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Std))
        && FunctionSignature::of(db, func).name.as_str() == name
}

fn is_std_struct(db: &dyn HirDatabase, ty: Ty<'_>, name: &str) -> bool {
    let TyKind::Adt(def, _) = ty.kind() else { return false };
    let AdtId::StructId(id) = def.def_id() else { return false };
    matches!(id.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Std))
        && StructSignature::of(db, id).name.as_str() == name
}

/// What a function of alloc's `Rc`, `Arc` or their `Weak` does that depends on how many hold
/// the value, or on whether two hold the same one.
fn reads_count(db: &dyn HirDatabase, func: FunctionId) -> Option<&'static str> {
    if !matches!(func.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Alloc))
    {
        return None;
    }
    let ItemContainerId::ImplId(id) = func.lookup(db).container else { return None };
    if db.impl_trait(id).is_some() {
        return None;
    }
    let TyKind::Adt(def, _) = db.impl_self_ty(id).skip_binder().kind() else { return None };
    let AdtId::StructId(adt) = def.def_id() else { return None };
    if !matches!(StructSignature::of(db, adt).name.as_str(), "Rc" | "Arc" | "Weak") {
        return None;
    }
    match FunctionSignature::of(db, func).name.as_str() {
        "strong_count" | "weak_count" | "get_mut" | "make_mut" | "try_unwrap" | "into_inner"
        | "unwrap_or_clone" | "is_unique" | "upgrade" => Some("reads the reference count"),
        "ptr_eq" => Some("tells whether two share their value"),
        _ => None,
    }
}

/// Whether `adt` is what an `Rc` or an `Arc` points to: its value with the counts.
fn holds_counts(db: &dyn HirDatabase, adt: AdtId) -> bool {
    let AdtId::StructId(id) = adt else { return false };
    matches!(id.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Alloc))
        && matches!(StructSignature::of(db, id).name.as_str(), "RcInner" | "ArcInner")
}

/// Whether a call of `func` is to code the code generator takes from rustc, as `callee` does,
/// and never compiles: `TypeId::of`, a hash only rustc computes, and the methods of `dyn Any`,
/// which compare it. What they give is decided by the type they are instantiated with, which
/// their name has.
fn taken_from_rustc(db: &dyn HirDatabase, func: FunctionId) -> bool {
    use rustc_type_ir::inherent::BoundExistentialPredicates as _;
    if !matches!(func.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Core))
    {
        return false;
    }
    let ItemContainerId::ImplId(id) = func.lookup(db).container else { return false };
    if db.impl_trait(id).is_some() {
        return false;
    }
    match db.impl_self_ty(id).skip_binder().kind() {
        TyKind::Adt(def, _) => {
            let interner = DbInterner::new_with(db, func.krate(db));
            interner.lang_items().TypeId.is_some_and(|it| def.def_id() == AdtId::StructId(it))
                && FunctionSignature::of(db, func).name.as_str() == "of"
        }
        TyKind::Dynamic(bounds, _) => bounds.principal_def_id().is_some_and(|it| {
            matches!(
                it.0.krate(db).data(db).origin,
                base_db::CrateOrigin::Lang(LangCrateOrigin::Core)
            ) && TraitSignature::of(db, it.0).name.as_str() == "Any"
        }),
        _ => false,
    }
}

fn diverges(db: &dyn HirDatabase, func: FunctionId) -> bool {
    let output = db.callable_item_signature(func.into()).skip_binder().skip_binder().output();
    matches!(output.kind(), TyKind::Never)
}

fn static_name(db: &dyn HirDatabase, id: StaticId) -> String {
    let mut path = vec![StaticSignature::of(db, id).name.as_str().to_owned()];
    if let ItemContainerId::ModuleId(module) = id.lookup(db).container {
        let mut module = Some(module);
        while let Some(at) = module {
            if let Some(name) = at.name(db) {
                path.push(name.as_str().to_owned());
            }
            module = at.containing_module(db);
        }
    }
    path.push(crate_name(db, id.krate(db)));
    path.reverse();
    path.join("::")
}

fn describe(db: &dyn HirDatabase, node: &Node) -> String {
    match node {
        Node::Root(func) => {
            format!("`{}`", function_name(db, *func, GenericArgs::new_from_slice(&[])))
        }
        Node::Instance(instance, _) => {
            let args = instance.args.as_ref();
            match instance.def {
                InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => {
                    format!("`{}`", function_name(db, func, args))
                }
                InstanceDef::Closure(id) => {
                    let at = InternedClosureId::from_id(id).loc(db);
                    match at.owner {
                        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func))
                            if cached_closure(db, func) == Some(id) =>
                        {
                            format!("`{}`", function_name(db, func, args))
                        }
                        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => {
                            format!("a closure in `{}`", function_name(db, func, args))
                        }
                        _ => "a closure".to_owned(),
                    }
                }
                InstanceDef::Coroutine(_) | InstanceDef::CoroutineByMove(_) => {
                    "an `async` block".to_owned()
                }
                InstanceDef::DropGlue => {
                    let ty = args.as_slice().first().and_then(|it| it.ty());
                    let ty = ty.and_then(|ty| hir_ty_mir::mir::type_name::rustc_type_name(db, ty));
                    format!("the drop of `{}`", ty.unwrap_or_default())
                }
                _ => "a shim".to_owned(),
            }
        }
        Node::Static(id) => format!("the static `{}`", static_name(db, *id)),
        Node::Leaf(name, _) => format!("`{name}`"),
    }
}

/// File, line and column of a place in a body, the file relative to the workspace.
pub(super) fn location(
    db: &dyn HirDatabase,
    owner: InferBodyId<'_>,
    span: MirSpan,
) -> Option<String> {
    let InferBodyId::DefWithBodyId(owner) = owner else { return None };
    let (file, line, column) = super::source_location(db, owner, span)?;
    let root = db.file_source_root(file).source_root_id(db);
    let root = db.source_root(root).source_root(db);
    let path = root.path_for_file(&file)?.as_path()?.to_string();
    let workspace = owner.module(db).krate(db).data(db).proc_macro_cwd.to_string();
    let path = path
        .strip_prefix(&workspace)
        .map(|it| it.trim_start_matches('/').to_owned())
        .unwrap_or(path);
    Some(format!("{path}:{line}:{column}"))
}

/// What `body`, of core or alloc, does itself that is not pure; else the bodies of core and
/// alloc it calls, or makes a closure or a function pointer of, are pushed to `calls`.
fn library_body<'db>(
    db: &'db dyn HirDatabase,
    body: LibraryBody<'db>,
    calls: &mut Vec<LibraryBody<'db>>,
) -> Option<String> {
    let (mir, krate) = match body {
        LibraryBody::Fn(func) if FunctionSignature::is_intrinsic(db, func) => return None,
        LibraryBody::Fn(func) => (db.mir_body(func.into()), func.krate(db)),
        LibraryBody::Closure(closure) => {
            (db.mir_body_for_closure(closure), closure.loc(db).owner.module(db).krate(db))
        }
    };
    let mir = match mir {
        Ok(it) => it,
        Err(e) => return Some(format!("has MIR blaze can't read ({e:?})")),
    };
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate };
    for (_, block) in mir.basic_blocks.iter() {
        for statement in &block.statements {
            match &statement.kind {
                StatementKind::InlineAsm(_) => return Some("has assembly in it".to_owned()),
                StatementKind::Assign(_, rvalue) => {
                    if let Rvalue::Aggregate(AggregateKind::Closure(ty), _) = rvalue
                        && let TyKind::Closure(id, _) = ty.as_ref().kind()
                    {
                        calls.push(LibraryBody::Closure(id.0));
                    }
                    for operand in operands(rvalue) {
                        if let Some(what) = library_operand(db, env, operand, calls) {
                            return Some(what);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(terminator) = &block.terminator
            && let TerminatorKind::Call { func, args, .. } = &terminator.kind
        {
            for operand in std::iter::once(func).chain(args.iter()) {
                if let Some(what) = library_operand(db, env, operand, calls) {
                    return Some(what);
                }
            }
        }
    }
    None
}

/// What an operand of a body of core or alloc reads or names that is not pure; else the
/// function of core or alloc it names, if one, is pushed to `calls`.
fn library_operand<'db>(
    db: &'db dyn HirDatabase,
    env: ParamEnvAndCrate<'db>,
    operand: &Operand,
    calls: &mut Vec<LibraryBody<'db>>,
) -> Option<String> {
    if let OperandKind::Static(id) = &operand.kind {
        let id = *id;
        if let Some(name) = foreign_static(db, id) {
            return (!name.starts_with("__rust_"))
                .then(|| format!("reads the foreign static `{name}`"));
        }
        let name = static_name(db, id);
        if is_thread_local(db, id) {
            return Some(format!("uses the thread-local `{name}`"));
        }
        return match compiled_static(db, id) {
            Ok(it) => it.writable.then(|| format!("reads the static `{name}`, which can change")),
            Err(e) => Some(format!("reads the static `{name}`, which blaze can't compile ({e})")),
        };
    }
    let (func, args) = fn_item(operand)?;
    if diverges(db, func) {
        // A panic, the handler core declares as foreign among them: what it prints is no
        // result.
        return None;
    }
    if let Some(symbol) = link_name(db, func) {
        return (!pure_foreign(&symbol)).then(|| format!("calls the foreign function `{symbol}`"));
    }
    if FunctionSignature::is_intrinsic(db, func) {
        return None;
    }
    if let Some(what) = gives_address(db, func, false) {
        return Some(format!("calls `{}`, which {what}", function_name(db, func, args)));
    }
    let func = match func.lookup(db).container {
        ItemContainerId::TraitId(_) => {
            if rustc_type_ir::TypeVisitableExt::has_param(&args) {
                return None;
            }
            let (either::Either::Left(it), _) = db.lookup_impl_method(env, func, args) else {
                return None;
            };
            if !FunctionSignature::of(db, it).has_body() {
                return None;
            }
            it
        }
        _ => func,
    };
    if is_library(db, func.krate(db)) {
        calls.push(LibraryBody::Fn(func));
    }
    None
}

fn is_library(db: &dyn HirDatabase, krate: Crate) -> bool {
    matches!(
        krate.data(db).origin,
        base_db::CrateOrigin::Lang(LangCrateOrigin::Core | LangCrateOrigin::Alloc)
    )
}

fn library_name(db: &dyn HirDatabase, body: LibraryBody<'_>) -> String {
    match body {
        LibraryBody::Fn(func) => {
            format!("`{}`", function_name(db, func, GenericArgs::new_from_slice(&[])))
        }
        LibraryBody::Closure(closure) => match closure.loc(db).owner.as_function() {
            Some(func) => {
                format!(
                    "a closure in `{}`",
                    function_name(db, func, GenericArgs::new_from_slice(&[]))
                )
            }
            None => "a closure".to_owned(),
        },
    }
}

/// The symbol a foreign static is linked by, if `id` is one.
fn foreign_static(db: &dyn HirDatabase, id: StaticId) -> Option<String> {
    super::static_link_name(db, id)
}

/// Why the arguments of the closure can't be what a result is kept by, and whether one holds
/// the reference counts of an `Rc` or an `Arc`.
fn arguments_trouble(
    db: &dyn HirDatabase,
    closure: &Instance,
    env: &StoredParamEnvAndCrate,
) -> (Option<String>, bool) {
    let Ok(types) = super::body_types(db, closure.clone(), env.clone()).as_ref() else {
        return (None, false);
    };
    let interner = DbInterner::new_with(db, env.krate);
    let mut seen = FxHashSet::default();
    let mut counted = false;
    // The first parameter is the closure itself, which captures nothing.
    for ty in types.params.iter().skip(1) {
        let ty = ty.as_ref();
        if let Some(why) = trouble(db, interner, ty, false, &mut seen, &mut counted) {
            let name = hir_ty_mir::mir::type_name::rustc_type_name(db, ty).unwrap_or_default();
            return (Some(format!("an argument of type `{name}` {why}")), counted);
        }
    }
    (None, counted)
}

/// Why a value of `ty` can't be what a result is kept by: it can change behind a shared
/// reference, or it holds something the bytes it is written as don't tell all of. The counts
/// of an `Rc` or an `Arc` change behind one, but only what reads them sees that, and the walk
/// refuses that instead (`reads_count`): `counted` is set when `ty` holds some.
fn trouble<'db>(
    db: &'db dyn HirDatabase,
    interner: DbInterner<'db>,
    ty: Ty<'db>,
    in_library: bool,
    seen: &mut FxHashSet<Ty<'db>>,
    counted: &mut bool,
) -> Option<String> {
    if !seen.insert(ty) {
        return None;
    }
    match ty.kind() {
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Str
        | TyKind::Never
        | TyKind::FnDef(..)
        | TyKind::FnPtr(..) => None,
        TyKind::Ref(_, pointee, _) => trouble(db, interner, pointee, in_library, seen, counted),
        TyKind::RawPtr(pointee, _) if in_library => {
            trouble(db, interner, pointee, true, seen, counted)
        }
        TyKind::RawPtr(..) => Some("holds a raw pointer".to_owned()),
        TyKind::Array(element, _) | TyKind::Slice(element) | TyKind::Pat(element, _) => {
            trouble(db, interner, element, in_library, seen, counted)
        }
        TyKind::Tuple(fields) => {
            fields.iter().find_map(|field| trouble(db, interner, field, in_library, seen, counted))
        }
        TyKind::Adt(def, args) => {
            let lang = interner.lang_items();
            let library = def.def_id().module(db).krate(db).data(db).origin.is_lang();
            let counts = library && holds_counts(db, def.def_id());
            let variants: Vec<VariantId> = match def.def_id() {
                AdtId::StructId(id) if Some(id) == lang.UnsafeCell => {
                    return Some(
                        "can change behind a shared reference: it holds an `UnsafeCell`, as \
                         `Cell`, `RefCell`, `Mutex` and atomics do"
                            .to_owned(),
                    );
                }
                AdtId::StructId(id) => vec![id.into()],
                AdtId::EnumId(id) => {
                    id.enum_variants(db).variants.iter().map(|(_, it)| it.0.into()).collect()
                }
                AdtId::UnionId(_) if !library => return Some("holds a union".to_owned()),
                AdtId::UnionId(id) => vec![id.into()],
            };
            let in_fields = variants.into_iter().find_map(|variant| {
                let fields = variant.fields(db).fields();
                db.field_types(variant).iter().find_map(|(id, field)| {
                    if counts && matches!(fields[id].name.as_str(), "strong" | "weak") {
                        *counted = true;
                        return None;
                    }
                    let ty = field.ty().instantiate(interner, args).skip_norm_wip();
                    trouble(db, interner, ty, library, seen, counted)
                })
            });
            in_fields.or_else(|| {
                // What a marker holds a type for.
                args.iter()
                    .filter_map(|it| it.ty())
                    .find_map(|ty| trouble(db, interner, ty, library, seen, counted))
            })
        }
        TyKind::Dynamic(..) => Some("is a trait object".to_owned()),
        TyKind::Alias(..) if in_library => None,
        _ => Some("is of a type blaze can't see into".to_owned()),
    }
}

/// Hashes compiled code without what tells where it was written: the lines it is of and
/// where its variables are.
fn hash_code(hasher: &mut impl Hasher, code: &Code) {
    let Code { signature, bytes, align, relocs, elf_types, unwind, lines: _, slots: _ } = code;
    (signature, bytes, align, elf_types).hash(hasher);
    format!("{unwind:?}").hash(hasher);
    relocs.len().hash(hasher);
    for reloc in relocs {
        (reloc.offset, std::mem::discriminant(&reloc.kind), reloc.addend).hash(hasher);
        std::mem::discriminant(&reloc.name).hash(hasher);
        match &reloc.name {
            ModuleRelocTarget::User { namespace, index } => (namespace, index).hash(hasher),
            ModuleRelocTarget::LibCall(call) => call.hash(hasher),
            ModuleRelocTarget::KnownSymbol(symbol) => symbol.hash(hasher),
            ModuleRelocTarget::FunctionOffset(func, offset) => (func, offset).hash(hasher),
        }
    }
}

impl<'db> FunctionCx<'_, 'db> {
    /// A call of `__blaze_cache_code::<F>`: writes the code of the closure `F` to `dst`, as
    /// the `Code` of `blaze::__private` has it. That is two words of hash, then the name of
    /// the function, or why it can't be cached when the hash is zero.
    pub(super) fn cache_code(&mut self, generic_args: GenericArgs<'db>, dst: Value) -> Result<()> {
        let db = self.cg.db;
        let ty = generic_args.type_at(0);
        let TyKind::Closure(id, args) = ty.kind() else {
            return Err(CodegenError::Unsupported(format!("`{CODE}` of `{ty:?}`")));
        };
        let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
        let parent = super::erase_regions(self.interner, parent);
        let closure = Instance {
            def: InstanceDef::Closure(id.0.as_id()),
            args: parent.store(),
            checked: self.cg.checked,
        };
        let env = Homes::default().home(db, &closure, &self.env.store());
        let (hash, text) = match cache_code(db, closure.clone(), env) {
            Ok(hash) => (*hash, cached_name(db, &closure)),
            Err(why) => {
                tracing::info!(target: "blaze", "{why}");
                (0, why.clone())
            }
        };
        let words = [hash as u64, (hash >> 64) as u64];
        for (i, word) in words.into_iter().enumerate() {
            let word = self.builder.ins().iconst(types::I64, word as i64);
            self.builder.ins().store(MEM, word, dst, 8 * i as i32);
        }
        let len = self.builder.ins().iconst(ptr(), text.len() as i64);
        let data = self.cg.define_const(ConstBlob {
            bytes: text.into_bytes(),
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: 1,
            type_ids: Vec::new(),
        })?;
        let data = self.cg.declare_data_in_func(data, self.builder.func);
        let text = self.builder.ins().symbol_value(ptr(), data);
        let width = ptr().bytes() as i32;
        self.builder.ins().store(MEM, text, dst, 16);
        self.builder.ins().store(MEM, len, dst, 16 + width);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use cranelift_codegen::{ir::Signature, isa::CallConv};

    use super::super::debuginfo::{LineRow, SlotPlaces};
    use super::*;

    fn code(bytes: &[u8], lines: Vec<LineRow>) -> Code {
        Code {
            signature: Signature::new(CallConv::SystemV),
            bytes: bytes.to_vec(),
            align: 16,
            relocs: Vec::new(),
            elf_types: Vec::new(),
            unwind: None,
            lines,
            slots: SlotPlaces::default(),
        }
    }

    fn hash(code: &Code) -> u128 {
        let mut hasher = rustc_stable_hash::StableSipHasher128::new();
        hash_code(&mut hasher, code);
        let type_id::Fingerprint(low, high) = hasher.finish();
        u128::from(low) | u128::from(high) << 64
    }

    #[test]
    fn the_hash_of_code_is_the_same_in_every_run() {
        // A hash of no state of the process: the one of this code is the one every run, and
        // every daemon, has worked out.
        assert_eq!(
            hash(&code(&[0x90, 0xc3], Vec::new())),
            0x98ba_8c38_257c_5f9e_e4b6_1b43_6ca7_2516
        );
    }

    #[test]
    fn code_moved_to_other_lines_has_the_same_hash_and_other_code_another() {
        let here = code(&[0x90, 0xc3], vec![LineRow { offset: 0, span: 3 }]);
        let moved = code(&[0x90, 0xc3], vec![LineRow { offset: 0, span: 9 }]);
        let other = code(&[0x90, 0x90, 0xc3], Vec::new());
        assert_eq!(hash(&here), hash(&moved));
        assert_ne!(hash(&here), hash(&other));
    }

    #[test]
    fn what_is_below_a_sealed_node_reads_back_as_it_was_written() {
        let call = |name: &str, at: Option<&str>| Call {
            name: name.to_owned(),
            at: at.map(str::to_owned),
        };
        let found = |kind, what: &str, why: &str, path| Found {
            kind,
            what: what.to_owned(),
            why: why.to_owned(),
            path,
        };
        let below = Below {
            found: vec![
                found(
                    Kind::Fs,
                    "open",
                    "it reaches `open`, which reads or writes files",
                    vec![
                        call("std::fs::read", Some("library/std/src/fs.rs:300:5")),
                        call("std::fs::File::open", None),
                        call("open", None),
                    ],
                ),
                found(
                    Kind::GlobalState,
                    "c::N",
                    "it uses the static `c::N`",
                    vec![call("f", None)],
                ),
            ],
        };
        let read = Below::read(below.write().as_bytes()).unwrap();
        assert_eq!(read.found, below.found);
        let nothing = Below::read(Below::default().write().as_bytes()).unwrap();
        assert!(nothing.found.is_empty());
        // A tab in what is said would start a call of its own.
        let tabbed = Below {
            found: vec![found(Kind::Uncompiled, "f", "a\tb", vec![call("f", Some("x\ty"))])],
        };
        let read = Below::read(tabbed.write().as_bytes()).unwrap();
        assert_eq!(read.found[0].why, "a b");
        assert_eq!(read.found[0].path, vec![call("f", Some("x y"))]);
        assert!(Below::read(b"below 1\n").is_none());
    }

    #[test]
    fn every_kind_has_a_name_it_is_read_back_by() {
        for kind in Kind::ALL {
            assert_eq!(Kind::from_name(kind.name()), Some(kind));
        }
        let flaky: Vec<Kind> = Kind::ALL.into_iter().filter(|it| !it.blocks()).collect();
        assert_eq!(flaky, [Kind::Clock, Kind::Random, Kind::Thread]);
    }

    #[test]
    fn foreign_symbols_are_told_by_kind() {
        let kind = |symbol| outside(symbol).map(|(kind, _)| kind);
        assert_eq!(kind("open64"), Some(Kind::Fs));
        assert_eq!(kind("statx"), Some(Kind::Fs));
        assert_eq!(kind("getenv"), Some(Kind::Env));
        assert_eq!(kind("posix_spawnp"), Some(Kind::Process));
        assert_eq!(kind("_exit"), Some(Kind::Process));
        assert_eq!(kind("connect"), Some(Kind::Net));
        assert_eq!(kind("clock_gettime"), Some(Kind::Clock));
        assert_eq!(kind("getrandom"), Some(Kind::Random));
        assert_eq!(kind("pthread_create"), Some(Kind::Thread));
        assert_eq!(kind("write"), None);
        assert_eq!(kind("memcpy"), None);
        assert_eq!(kind("__errno_location"), None);
        assert!(locates_errno("__errno_location") && !pure_foreign("__errno_location"));
    }
}
