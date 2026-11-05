//! What `#[blaze::cache]` keeps the results of a function by: the behaviour identity of its
//! body and of everything that body can reach (`hir_ty_identity::code_id`). The attribute keeps the body
//! in a closure and has the function ask `blaze::__private::__blaze_cache_code` for the code
//! of that closure, which rustc answers with nothing, and so caches nothing. blaze compiles
//! the call into the answer: the hash, or why the function can't be cached.
//!
//! The hash is the closure's CodeId with the target, the nightly and whether it checks
//! overflow: comments, formatting and lines moved keep it, and an edit to anything the body
//! calls, directly or through others, changes it. There is no second hash of code here: what
//! code is and what makes two the same is `hir_ty_identity`'s alone. The walk below says only
//! whether what the body reaches is pure, and stays until lane pure's `#[blaze::pure]` check
//! takes those rules (docs/v2/plan/design/code-identity.md). A call through a `dyn` goes to a
//! vtable, and the code reached builds every vtable it can call through: the methods of those
//! vtables are reached as the rest is. The arguments can't be a `dyn`.
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

use std::borrow::Cow;
use std::hash::BuildHasher;
use std::sync::LazyLock;

use base_db::{Crate, Db, LangCrateOrigin};
use blaze_hash::{FixedState, HashMap, HashSet};
use blaze_query::db::{AsId, FromId};
use cranelift_codegen::ir::{InstBuilder, Value, types};
use hir_def::{
    AdtId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, HasModule, ItemContainerId, Lookup,
    StaticId, VariantId,
    signatures::{FunctionSignature, StaticSignature, StructSignature, TraitSignature},
};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind};
use std::alloc::Global;

use hir_ty::{
    InferBodyId,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    mir::{AggregateKind, MirSpan, Operand, OperandKind, Rvalue, StatementKind, TerminatorKind},
    next_solver::{DbInterner, GenericArgs, ParamEnv, Ty, TyKind},
    traits::{ParamEnvAndCrate, StoredParamEnvAndCrate},
};
use hir_ty_mir::mir::purity::{is_const_pure, operands, raw_use};

use super::{
    CodegenError, CompiledFunction, ConstBlob, DataRequest, FuncTarget, FunctionCx, Instance,
    InstanceDef, MEM, Requests, Result, Target, compiled_instance, compiled_root, compiled_static,
    is_thread_local, link_name_for,
};

/// The function whose instance names how the arguments and the result are written: reached
/// for the hash, though what it does is not the cached function's.
const CODEC: &str = "__blaze_codec";
/// Changes when what is hashed does, so no hash of before means anything now.
const FORMAT: u32 = 2;
/// The domain of a cached function's code: its closure's CodeId (`hir_ty_identity::code_id`),
/// the target and the nightly, of which the `Code` the call is answered with is the first
/// 16 bytes. The purity of what it reaches is the walk's to say until lane pure's
/// `#[blaze::pure]` check takes it (docs/v2/plan/design/code-identity.md).
const CACHED_CODE_DOMAIN: &str = "blaze v2 cached code 1";
/// Whether `BLAZE_FN_CACHE_TRACE` asks for what the walk goes through, read once a process.
static TRACE: LazyLock<bool> = LazyLock::new(|| std::env::var_os("BLAZE_FN_CACHE_TRACE").is_some());

/// Whether `func` is the function a cached function asks for the code of its body.
pub(super) fn is_code(db: &Db, func: FunctionId) -> bool {
    hir_ty_mir::mir::answered_by_codegen(db, func)
}

fn is_codec(db: &Db, func: FunctionId) -> bool {
    FunctionSignature::of(db, func).name.as_str() == CODEC
}

/// The closure that `#[blaze::cache]` put the body of `func` in, when it is a cached
/// function: the one it asks for the code of.
pub(super) fn cached_closure(db: &Db, func: FunctionId) -> Option<blaze_query::db::Id> {
    if !hir_ty::fn_cache::uses_fn_cache(db, func.krate(db)) {
        return None;
    }
    cached_closure_of(db, func).map(|it| it.as_id())
}

#[blaze_query::tracked]
fn cached_closure_of<'db>(db: &'db Db, func: FunctionId) -> Option<InternedClosureId<'db>> {
    let body = db.mir_body(func.into()).ok()?;
    body.basic_blocks.iter().find_map(|(_, block)| {
        let TerminatorKind::Call { func: callee, .. } = &block.terminator.as_ref()?.kind else {
            return None;
        };
        let (callee, args) = callee.fn_item()?;
        if !is_code(db, callee) {
            return None;
        }
        let TyKind::Closure(id, _) = args.type_at(0).kind() else { return None };
        Some(id.0)
    })
}

/// Why a cached function writes no hash of its body's code.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) enum Uncached {
    /// What the body reaches, or what it is given, can make another result for the same
    /// arguments: the program's doing. The function runs every time, and says why the first.
    Impure(String),
    /// blaze can't give the body a code identity. The build stops with this, naming the
    /// function and the refusal: running it uncached in its place is a fallback (rule 3).
    Refused(String),
    /// blaze can't compile something the body reaches, and nothing else it reaches is impure,
    /// so nothing tells whether it can be cached. The build stops with this: running it
    /// uncached in its place is a fallback (rule 3).
    Uncompiled(Uncompiled),
}

/// A cached function's body reaches code the code generator refuses ([`Uncached::Uncompiled`]).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) struct Uncompiled {
    /// The cached function, as a backtrace names it, with its generic arguments.
    pub function: String,
    /// What the code generator refused, by the shortest way from the body: "it reaches
    /// `a` → `b`, which can't be compiled by blaze, at src/lib.rs:4:5".
    pub reached: String,
    /// The code generator's refusal of it.
    pub refusal: CodegenError,
}

impl std::fmt::Display for Uncompiled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Uncompiled { function, reached, refusal } = self;
        write!(
            f,
            "`{function}` (`#[blaze::cache]`) can't be built: {reached}. The code generator \
             refused it: {refusal}. What blaze can't compile tells nothing of whether the \
             function can be cached, and running it uncached in its place would be a fallback \
             (rule 3, no-fallback)"
        )
    }
}

/// The hash of the code of the closure a cached function keeps its body in and of all it
/// reaches, or why the function can't be cached.
#[blaze_query::tracked(returns(ref))]
pub(super) fn cache_code(
    db: &Db,
    closure: Instance,
    env: StoredParamEnvAndCrate,
) -> std::result::Result<u128, Uncached> {
    let (trouble, counted) = arguments_trouble(db, &closure, &env);
    let target = super::target_of_crate(db, env.as_ref(db).krate).map_err(|e| {
        // alloc: the error's message
        Uncached::Refused(format!(
            "`{}` (`#[blaze::cache]`) has no target: {e}",
            cached_name(db, &closure)
        ))
    })?;
    let mut walk = Walk::new(db, target, counted);
    // clone: the root node owns its key; a stored param env is a reference count
    let root = walk.reach(Node::Instance(closure, env.clone()));
    let mut next = 0;
    while next < walk.nodes.len() {
        walk.look_at(next as u32);
        next += 1;
    }
    // A known impurity decides alone: what blaze can't compile can add to it, never take it
    // away. Without one, a body that reaches code blaze can't compile stops the build.
    let (impure, uncompiled) = walk.impure(root);
    if let Some(why) = impure.or(trouble) {
        let name = cached_name(db, &closure);
        // alloc: the error, why the function is not cached
        return Err(Uncached::Impure(format!("`{name}` is not cached: {why}")));
    }
    if let Some((reached, refusal)) = uncompiled {
        return Err(Uncached::Uncompiled(Uncompiled {
            function: cached_name(db, &closure),
            reached,
            refusal,
        }));
    }
    // What the closure can run is its behaviour identity: its code, its arguments, and the
    // CodeId of every instance it can reach, std's with them (docs/v2/plan/design/code-identity.md).
    // One it can't be given stops the build, never another hash or an uncached run in its place.
    let InstanceDef::Closure(id) = closure.def else {
        // alloc: the error's message
        return Err(Uncached::Refused(format!(
            "`{}` (`#[blaze::cache]`): its body is not a closure",
            cached_name(db, &closure)
        )));
    };
    let root = hir_ty_identity::Instance {
        subject: hir_ty_identity::Subject::Body(hir_ty_mir::mir::dispatch::Body::Closure(id)),
        args: closure.args.clone(), // clone: the query's key, a reference count
    };
    let code = hir_ty_identity::code_id(db, &root, &env).map_err(|why| {
        // alloc: the error's message
        Uncached::Refused(format!(
            "`{}` (`#[blaze::cache]`) has no code identity for its body: {why}",
            cached_name(db, &closure)
        ))
    })?;
    // The nightly: what is outside the checkout is linked as its pre-built std, whose code a
    // nightly can change.
    let mut hasher = blaze_ids::Hasher::new(CACHED_CODE_DOMAIN);
    hasher.update(&FORMAT.to_le_bytes()).part(target.triple().as_bytes());
    hasher
        .part(crate::v2::toolchain::NIGHTLY.as_bytes())
        .update(&[closure.checked as u8])
        .id(&code.0);
    let id = hasher.finish().0;
    Ok(u128::from_le_bytes(id[..16].try_into().expect("a content ID's first 16 bytes")))
}

/// What `instance` of the cached function `func` writes as the code of its body
/// ([`FunctionCx::cache_code`]): the hash of all its closure reaches, or why it is not cached
/// or can't be compiled.
/// `None` for a function that asks for no closure's code. The function's own code hash takes
/// its closure by interface, so a key of its code must take this beside it.
pub(super) fn written_code<'db>(
    db: &'db Db,
    func: FunctionId,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
) -> Option<&'db std::result::Result<u128, Uncached>> {
    let id = (*cached_closure_of(db, func))?;
    let closure = Instance {
        def: InstanceDef::Closure(id.as_id()),
        args: instance.args,
        checked: instance.checked,
    };
    let env = closure.home_env(db, env);
    Some(cache_code(db, closure, env))
}

/// What a foreign function or static a test reaches does that the test's code does not
/// decide, by its symbol, and of which [`Kind`]: `None` for what gives the same for the same
/// input, as far as a test can tell. Locks are left out: a test that turns on how its threads
/// are scheduled is flaky whether it runs again or not.
pub(crate) fn outside(symbol: &str) -> Option<(Kind, &'static str)> {
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
pub(super) fn cached_name(db: &Db, closure: &Instance) -> String {
    // alloc: the text the cached function is given, or its error, once a cached function
    let InstanceDef::Closure(id) = closure.def else { return "?".to_owned() };
    match InternedClosureId::from_id(id).loc(db).owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => {
            function_name(db, func, closure.args.as_ref())
        }
        // alloc: the text the cached function is given, or its error, once a cached function
        _ => "?".to_owned(),
    }
}

fn function_name(db: &Db, func: FunctionId, args: GenericArgs<'_>) -> String {
    hir_ty_print::rustc_function_name(db, func, args.as_slice())
        // alloc: a name is owned as `rustc_function_name` gives it: a leaf's key, or a message
        .unwrap_or_else(|| FunctionSignature::of(db, func).name.as_str().to_owned())
}

/// What the walk goes through: the functions and statics compiled, and what has no code of
/// ours, by its name.
#[derive(PartialEq, Eq, Hash)]
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
    /// Its edges in [`Walk::edges`], pushed together while it is looked at.
    edges: std::ops::Range<u32>,
    /// What keeps it from being pure, a leaf's apart: that is in its [`Node::Leaf`].
    why: Option<Why>,
}

/// What keeps a node from being pure.
struct Why {
    /// What the node does, as said of it: "dereferences a raw pointer". Of a foreign function
    /// or a static it is said of what uses it: "calls the foreign function `write`".
    text: Cow<'static, str>,
    of_user: bool,
    /// Where that is written, when that is told.
    place: Option<String>,
}

impl Why {
    fn own(text: impl Into<Cow<'static, str>>) -> Option<Why> {
        Some(Why { text: text.into(), of_user: false, place: None })
    }

    fn of_user(text: impl Into<Cow<'static, str>>) -> Option<Why> {
        Some(Why { text: text.into(), of_user: true, place: None })
    }
}

/// What [`Walk::library`] goes through, kept between the functions it looks into.
#[derive(Default)]
struct LibraryWalk<'db> {
    parent: HashMap<LibraryBody<'db>, LibraryBody<'db>, Global>,
    seen: HashSet<LibraryBody<'db>, Global>,
    queue: std::collections::VecDeque<LibraryBody<'db>>,
    calls: Vec<LibraryBody<'db>>,
}

struct Walk<'db> {
    db: &'db Db,
    /// The target of the cached function's crate, which the symbols it reaches are of.
    target: &'static Target,
    nodes: Vec<Seen>,
    /// Every node's edges, a node's together: [`Seen::edges`] says where.
    edges: Vec<Edge>,
    /// Where in `nodes` each node is, by the hash of the node there.
    index: hashbrown::HashTable<u32>,
    /// The nodes blaze can't compile, whose code tells nothing of what they do, with the
    /// code generator's refusal of each.
    uncompiled: HashMap<u32, CodegenError, Global>,
    /// An argument holds a reference count, which what reads one gives and the key does not
    /// tell: two arguments can share a value or not.
    counted: bool,
    /// What each function of core and alloc known by its name does that is not pure.
    library: HashMap<FunctionId, Option<String>, Global>,
    library_walk: LibraryWalk<'db>,
}

/// A body of core or alloc looked through: a function's, or a closure's.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum LibraryBody<'db> {
    Fn(FunctionId),
    Closure(InternedClosureId<'db>),
}

impl<'db> Walk<'db> {
    fn new(db: &'db Db, target: &'static Target, counted: bool) -> Walk<'db> {
        Walk {
            db,
            target,
            // alloc: the walk's table of nodes, grown once a node found (a walk is a miss)
            nodes: Vec::new(),
            // alloc: the walk's edges, grown once an edge found (a walk is a miss)
            edges: Vec::new(),
            index: hashbrown::HashTable::new(),
            uncompiled: HashMap::new_in(Global),
            counted,
            library: HashMap::new_in(Global),
            library_walk: LibraryWalk::default(),
        }
    }

    /// The edges of node `at`, in the order found.
    fn edges_of(&self, at: u32) -> &[Edge] {
        let range = &self.nodes[at as usize].edges;
        &self.edges[range.start as usize..range.end as usize]
    }

    fn reach(&mut self, node: Node) -> u32 {
        let nodes = &self.nodes;
        let hash = FixedState.hash_one(&node);
        let entry = self.index.entry(
            hash,
            |&at| nodes[at as usize].node == node,
            |&at| FixedState.hash_one(&nodes[at as usize].node),
        );
        match entry {
            hashbrown::hash_table::Entry::Occupied(found) => *found.get(),
            hashbrown::hash_table::Entry::Vacant(slot) => {
                let at = self.nodes.len() as u32;
                slot.insert(at);
                self.nodes.push(Seen { node, edges: 0..0, why: None });
                at
            }
        }
    }

    fn edge(&mut self, from: u32, node: Node, site: Site, codec: bool) -> u32 {
        let to = self.reach(node);
        let edges = &mut self.nodes[from as usize].edges;
        debug_assert_eq!(edges.end as usize, self.edges.len(), "a node's edges are together");
        self.edges.push(Edge { to, site, codec });
        edges.end += 1;
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
            // clone: the node owns its key; a stored param env is a reference count
            return (Node::Instance(*instance, env.clone()), site, false);
        };
        let site = Site::Func(func);
        if let Some(closure) = cached_closure(db, func) {
            // Another cached function, or this one called again: its body is what it runs
            // when its result is not kept, and what it reaches from there is the body's.
            let closure = Instance { def: InstanceDef::Closure(closure), ..*instance };
            let env = closure.home_env(db, env);
            return (Node::Instance(closure, env), site, false);
        }
        let name = || function_name(db, func, instance.args.as_ref());
        if let Some(symbol) = link_name_for(db, self.target, func) {
            let why =
                // alloc: why the function is not pure, a message, once a leaf
                (!pure_foreign(&symbol)).then(|| format!("calls the foreign function `{symbol}`"));
            return (Node::Leaf(symbol, why), site, false);
        }
        if taken_from_rustc(db, func) {
            return (Node::Leaf(name(), None), site, false);
        }
        if diverges(db, func) {
            // A panic: what it prints is no result.
            return (Node::Leaf(name(), None), site, false);
        }
        let told = |what: &str| {
            let name = name();
            // alloc: why the function is not pure, a message, once a leaf
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
            // clone: the node owns its key; a stored param env is a reference count
            return (Node::Instance(*instance, env.clone()), site, codec);
        }
        match func.krate(db).data(db).origin {
            base_db::CrateOrigin::Lang(LangCrateOrigin::Core | LangCrateOrigin::Alloc) => {
                let why =
                    // alloc: why the function is not pure, a message, once a leaf
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
    fn library(&mut self, func: FunctionId) -> Option<&str> {
        if self.library.contains_key(&func) {
            return self.library[&func].as_deref();
        }
        let db = self.db;
        let start = LibraryBody::Fn(func);
        let mut walk = std::mem::take(&mut self.library_walk);
        walk.parent.clear();
        walk.seen.clear();
        walk.queue.clear();
        walk.seen.insert(start);
        walk.queue.push_back(start);
        let mut found = None;
        while let Some(at) = walk.queue.pop_front() {
            walk.calls.clear();
            if let Some(what) = library_body(db, self.target, at, &mut walk.calls) {
                found = Some((at, what));
                break;
            }
            for &next in &walk.calls {
                if walk.seen.insert(next) {
                    walk.parent.insert(next, at);
                    walk.queue.push_back(next);
                }
            }
        }
        let told = match found {
            Some((at, what)) => Some(library_told(db, &walk.parent, at, what)),
            None => {
                // What it reaches is as pure as it is.
                for &body in &walk.seen {
                    if let LibraryBody::Fn(it) = body {
                        self.library.insert(it, None);
                    }
                }
                None
            }
        };
        self.library_walk = walk;
        let told = told?;
        self.library.entry(func).insert(Some(told)).into_mut().as_deref()
    }

    /// Finds the edges of node `at`, the next not looked at, and what of it is not pure.
    fn look_at(&mut self, at: u32) {
        let db = self.db;
        if *TRACE {
            eprintln!("fn-cache: {at} {}", describe(db, &self.nodes[at as usize].node));
        }
        let start = self.edges.len() as u32;
        self.nodes[at as usize].edges = start..start;
        match &self.nodes[at as usize].node {
            Node::Root(func) => {
                let func = *func;
                match compiled_root(db, func) {
                    Ok(compiled) => {
                        self.function_code(at, compiled);
                        self.look_into(at, InferBodyId::DefWithBodyId(func.into()), None);
                    }
                    Err(e) => self.cannot(at, e),
                }
            }
            Node::Instance(instance, env) => {
                // clone: the query takes its key by value; a stored param env is a reference count
                let (instance, env) = (*instance, env.clone());
                match compiled_instance(db, instance, env) {
                    Ok(compiled) => {
                        self.function_code(at, compiled);
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
                    Err(e) => self.cannot(at, e),
                }
            }
            Node::Static(id) => {
                let id = *id;
                match compiled_static(db, id) {
                    Ok(compiled) => {
                        if compiled.writable {
                            let what = describe(db, &self.nodes[at as usize].node);
                            // alloc: why the static is not pure, a message, once a static
                            let why = format!("reads {what}, which can change");
                            self.nodes[at as usize].why = Why::of_user(why);
                        }
                        let own = self.is_own(at);
                        self.requests(at, &compiled.requests, own);
                    }
                    Err(e) => self.cannot(at, e),
                }
            }
            Node::Leaf(..) => {}
        }
    }

    /// Notes that the node did not compile: nothing tells what it does then. That is no
    /// impurity: unless one is found apart from it, the build stops on it ([`Uncompiled`]).
    fn cannot(&mut self, at: u32, e: &CodegenError) {
        // clone: the walk keeps the refusal; the query keeps its own
        self.uncompiled.insert(at, e.clone());
    }

    fn function_code(&mut self, at: u32, compiled: &CompiledFunction) {
        let own = self.is_own(at);
        self.requests(at, &compiled.requests, own);
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
            base_db::CrateOrigin::Lang(_) => {
                krate.rust_name(db).is_ok_and(|it| it.as_str() == "hashbrown")
            }
            _ => false,
        }
    }

    fn requests(&mut self, at: u32, requests: &Requests, own: bool) {
        for request in &requests.funcs {
            match &request.target {
                FuncTarget::Symbol(_) | FuncTarget::Path(_) => {}
                FuncTarget::Asm(_) => {
                    if own && self.nodes[at as usize].why.is_none() {
                        self.nodes[at as usize].why = Why::own("has assembly in it");
                    }
                }
                FuncTarget::Instance(instance, env) => {
                    let (node, site, codec) = self.function(at, instance, env);
                    self.edge(at, node, site, codec);
                }
            }
        }
        for request in &requests.datas {
            match request {
                DataRequest::Static(id) => {
                    let node = self.static_node(*id);
                    self.edge(at, node, Site::Static(*id), false);
                }
                DataRequest::Blob { .. }
                | DataRequest::Vtable { .. }
                | DataRequest::Location(_)
                | DataRequest::Declaration(_)
                | DataRequest::FilePath(_)
                | DataRequest::Text(_) => {}
            }
        }
    }

    fn static_node(&mut self, id: StaticId) -> Node {
        let db = self.db;
        if let Some(name) = foreign_static(db, self.target, id) {
            let why = (!name.starts_with("__rust_"))
                // alloc: why the static is not pure, a message, once a leaf
                .then(|| format!("reads the foreign static `{name}`"));
            return Node::Leaf(name, why);
        }
        if is_thread_local(db, id) {
            let name = static_name(db, id);
            // alloc: why the static is not pure, a message, once a leaf
            let why = format!("uses the thread-local `{name}`");
            return Node::Leaf(name, Some(why));
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
        if !self.is_own(at) || self.nodes[at as usize].why.is_some() {
            return;
        }
        let db = self.db;
        let body = match closure {
            Some(closure) => db.mir_body_for_closure(closure),
            None => db.mir_body(owner),
        };
        let Ok(body) = body else { return };
        if let Some((why, span)) = raw_use(body) {
            let place = location(db, body.owner, span);
            self.nodes[at as usize].why = Some(Why { text: why.into(), of_user: false, place });
        }
    }

    /// Why what `from` reaches is not pure, by the shortest way to the first that is not, and
    /// when nothing it reaches is known not to be, the first it reaches that blaze can't
    /// compile with the code generator's refusal: the edges to the codec are left out.
    fn impure(&self, from: u32) -> (Option<String>, Option<(String, CodegenError)>) {
        let mut parent: HashMap<u32, (u32, usize), Global> = HashMap::new_in(Global);
        let mut seen = HashSet::new_in(Global);
        let mut queue = std::collections::VecDeque::from([from]);
        seen.insert(from);
        let mut uncompiled = None;
        while let Some(at) = queue.pop_front() {
            let seen_at = &self.nodes[at as usize];
            if let Node::Leaf(_, Some(text)) = &seen_at.node {
                return (Some(self.told(at, &parent, text, true, None)), None);
            }
            if let Some(why) = &seen_at.why {
                let told = self.told(at, &parent, &why.text, why.of_user, why.place.as_deref());
                return (Some(told), None);
            }
            // The first found is the nearest; it has no edges, since nothing compiled tells them.
            if uncompiled.is_none()
                && let Some(refusal) = self.uncompiled.get(&at)
            {
                let told = self.told(at, &parent, "can't be compiled by blaze", false, None);
                // clone: the error owns the refusal; the walk keeps its own
                uncompiled = Some((told, refusal.clone()));
            }
            // A `const fn` with no generic arguments that is const-pure reaches nothing that is
            // not: what a generic one calls by a trait can be a non-const `impl` at run time.
            if let Node::Root(func) = self.nodes[at as usize].node
                && is_const_pure(self.db, func)
            {
                continue;
            }
            for (i, edge) in self.edges_of(at).iter().enumerate() {
                if !edge.codec && seen.insert(edge.to) {
                    parent.insert(edge.to, (at, i));
                    queue.push_back(edge.to);
                }
            }
        }
        (None, uncompiled)
    }

    /// Says why `at`, found by way of `parent`, is not pure: "it reaches `a` → `b`, which
    /// calls the foreign function `write`, at src/lib.rs:4:5". `text` is what [`Why`] says,
    /// of the user when `of_user`, at `place` when that is told.
    fn told(
        &self,
        at: u32,
        parent: &HashMap<u32, (u32, usize), Global>,
        text: &str,
        of_user: bool,
        place: Option<&str>,
    ) -> String {
        let db = self.db;
        // alloc: the error, why the function is not cached
        let mut chain = vec![at];
        while let Some(&(up, _)) = parent.get(chain.last().unwrap()) {
            chain.push(up);
        }
        chain.reverse();
        // Where the last code of the workspace on the way refers to the next.
        let place = place.map(str::to_owned).or_else(|| {
            chain.windows(2).rev().find_map(|pair| {
                if !self.is_own(pair[0]) {
                    return None;
                }
                let (_, edge) = parent[&pair[1]];
                let site = self.edges_of(pair[0])[edge].site;
                self.site_location(pair[0], site)
            })
        });
        // The cached function is "it", and what a foreign function or a static is said of
        // is what uses it.
        let named = &chain[1..chain.len() - usize::from(of_user)];
        let mut told = match named {
            // alloc: the error, why the function is not cached
            [] => format!("it {text}"),
            _ => {
                let names: Vec<String> =
                    // alloc: the error, why the function is not cached
                    named.iter().map(|&it| describe(db, &self.nodes[it as usize].node)).collect();
                let shown = match names.len() {
                    ..=4 => names.join(" → "),
                    // alloc: the error, why the function is not cached
                    _ => format!("{} → … → {}", names[..2].join(" → "), names[names.len() - 1]),
                };
                // alloc: the error, why the function is not cached
                format!("it reaches {shown}, which {text}")
            }
        };
        if let Some(place) = place {
            told.push_str(", at ");
            told.push_str(&place);
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
            (Site::Func(func), _) => operand.fn_item().is_some_and(|(it, _)| it == func),
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

/// What a function of core does that gives another result in another run, where the same
/// values are at other addresses: an address as a number, or pointers formatted, hashed or
/// ordered by theirs. Whether two pointers are equal is the same in every run, so equality is
/// not one. core is otherwise trusted, and its MIR not looked through: in it addresses are
/// also numbers where what it gives doesn't depend on them, as in slice iteration and `memchr`.
/// So `addr` and `expose_provenance` count where code of the workspace calls them (`own`), as
/// its casts do (`raw_use`): core and alloc call them to align and to tell a dangling pointer.
fn gives_address(db: &Db, func: FunctionId, own: bool) -> Option<&'static str> {
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
pub(super) fn kept_a_call(db: &Db, func: FunctionId, args: GenericArgs<'_>, own: bool) -> bool {
    gives_address(db, func, own).is_some()
        || makes_seed(db, func)
        || seeded_order(db, func, args).is_some()
        || reads_count(db, func).is_some()
}

/// Whether `func` makes a `RandomState`, whose seed is another in every run.
fn makes_seed(db: &Db, func: FunctionId) -> bool {
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
fn seeded_order(db: &Db, func: FunctionId, args: GenericArgs<'_>) -> Option<&'static str> {
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
fn hasher_of<'db>(db: &Db, ty: Ty<'db>) -> Option<Ty<'db>> {
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

fn is_std_struct(db: &Db, ty: Ty<'_>, name: &str) -> bool {
    let TyKind::Adt(def, _) = ty.kind() else { return false };
    let AdtId::StructId(id) = def.def_id() else { return false };
    matches!(id.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Std))
        && StructSignature::of(db, id).name.as_str() == name
}

/// What a function of alloc's `Rc`, `Arc` or their `Weak` does that depends on how many hold
/// the value, or on whether two hold the same one.
fn reads_count(db: &Db, func: FunctionId) -> Option<&'static str> {
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
fn holds_counts(db: &Db, adt: AdtId) -> bool {
    let AdtId::StructId(id) = adt else { return false };
    matches!(id.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Alloc))
        && matches!(StructSignature::of(db, id).name.as_str(), "RcInner" | "ArcInner")
}

/// Whether a call of `func` is to code the code generator takes from rustc, as `callee` does,
/// and never compiles: `TypeId::of`, a hash only rustc computes, and the methods of `dyn Any`,
/// which compare it. What they give is decided by the type they are instantiated with, which
/// their name has.
fn taken_from_rustc(db: &Db, func: FunctionId) -> bool {
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

fn diverges(db: &Db, func: FunctionId) -> bool {
    let output = db.callable_item_signature(func.into()).skip_binder().skip_binder().output();
    matches!(output.kind(), TyKind::Never)
}

pub(crate) fn static_name(db: &Db, id: StaticId) -> String {
    let mut modules = smallvec::SmallVec::<[_; 8]>::new();
    // A static in an `extern` block is named by the module the block is in.
    let module = match id.lookup(db).container {
        ItemContainerId::ModuleId(module) => Some(module),
        ItemContainerId::ExternBlockId(block) => Some(block.lookup(db).container),
        _ => None,
    };
    if let Some(module) = module {
        let mut module = Some(module);
        while let Some(at) = module {
            if let Some(name) = at.name(db) {
                modules.push(name);
            }
            module = at.containing_module(db);
        }
    }
    // alloc: a static's name, a leaf's key or what a message says
    let mut path = id.krate(db).named(db).to_string();
    for name in modules.iter().rev() {
        path.push_str("::");
        path.push_str(name.as_str());
    }
    path.push_str("::");
    path.push_str(StaticSignature::of(db, id).name.as_str());
    path
}

fn describe(db: &Db, node: &Node) -> String {
    // alloc: the text of a message, a trace, or what a test is told
    let mut out = String::new();
    let mut push = |parts: &[&str]| parts.iter().for_each(|it| out.push_str(it));
    match node {
        Node::Root(func) => {
            push(&["`", function_name(db, *func, GenericArgs::new_from_slice(&[])).as_str(), "`"])
        }
        Node::Instance(instance, _) => {
            let args = instance.args.as_ref();
            match instance.def {
                InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => {
                    push(&["`", function_name(db, func, args).as_str(), "`"])
                }
                InstanceDef::Closure(id) => {
                    let at = InternedClosureId::from_id(id).loc(db);
                    match at.owner {
                        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func))
                            if cached_closure(db, func) == Some(id) =>
                        {
                            push(&["`", function_name(db, func, args).as_str(), "`"])
                        }
                        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => {
                            push(&["a closure in `", function_name(db, func, args).as_str(), "`"])
                        }
                        _ => push(&["a closure"]),
                    }
                }
                InstanceDef::Coroutine(_) | InstanceDef::CoroutineByMove(_) => {
                    push(&["an `async` block"])
                }
                InstanceDef::DropGlue => {
                    let ty = args.as_slice().first().and_then(|it| it.ty());
                    let ty = ty.and_then(|ty| hir_ty_print::rustc_type_name(db, ty));
                    push(&["the drop of `", ty.as_deref().unwrap_or_default(), "`"])
                }
                _ => push(&["a shim"]),
            }
        }
        Node::Static(id) => push(&["the static `", static_name(db, *id).as_str(), "`"]),
        Node::Leaf(name, _) => push(&["`", name.as_str(), "`"]),
    }
    out
}

/// File, line and column of a place in a body, the file relative to the workspace.
pub(super) fn location(db: &Db, owner: InferBodyId<'_>, span: MirSpan) -> Option<String> {
    let owner = match owner {
        InferBodyId::DefWithBodyId(owner) => owner,
        // An inline constant or an array length in a body is written in the store of that body,
        // and its spans are places in it. One in a signature has no body to find them in.
        InferBodyId::AnonConstId(id) => match id.loc(db).owner {
            ExpressionStoreOwnerId::Body(owner) => owner,
            _ => return None,
        },
    };
    let (file, line, column) = super::source_location(db, owner, span)?;
    let root = db.file_source_root(file).source_root_id(db);
    let root = db.source_root(root).source_root(db);
    let path = root.path_for_file(&file)?.as_path()?.as_str();
    let workspace = owner.module(db).krate(db).data(db).proc_macro_cwd.as_str();
    let path = path.strip_prefix(workspace).map_or(path, |it| it.trim_start_matches('/'));
    // alloc: the place a message, or what a test is told, shows
    Some(format!("{path}:{line}:{column}"))
}

/// What `body`, of core or alloc, does itself that is not pure; else the bodies of core and
/// alloc it calls, or makes a closure or a function pointer of, are pushed to `calls`.
fn library_body<'db>(
    db: &'db Db,
    target: &Target,
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
        // alloc: why the function is not pure, a message
        Err(e) => return Some(format!("has MIR blaze can't read ({e:?})")),
    };
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate };
    for (_, block) in mir.basic_blocks.iter() {
        for statement in &block.statements {
            match &statement.kind {
                // alloc: why the function is not pure, a message
                StatementKind::InlineAsm(_) => return Some("has assembly in it".to_owned()),
                StatementKind::Assign(_, rvalue) => {
                    if let Rvalue::Aggregate(AggregateKind::Closure(ty), _) = rvalue
                        && let TyKind::Closure(id, _) = ty.as_ref().kind()
                    {
                        calls.push(LibraryBody::Closure(id.0));
                    }
                    for operand in operands(rvalue) {
                        if let Some(what) = library_operand(db, target, env, operand, calls) {
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
                if let Some(what) = library_operand(db, target, env, operand, calls) {
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
    db: &'db Db,
    target: &Target,
    env: ParamEnvAndCrate<'db>,
    operand: &Operand,
    calls: &mut Vec<LibraryBody<'db>>,
) -> Option<String> {
    if let OperandKind::Static(id) = &operand.kind {
        let id = *id;
        if let Some(name) = foreign_static(db, target, id) {
            return (!name.starts_with("__rust_"))
                // alloc: why the function is not pure, a message
                .then(|| format!("reads the foreign static `{name}`"));
        }
        let name = || static_name(db, id);
        if is_thread_local(db, id) {
            // alloc: why the function is not pure, a message
            return Some(format!("uses the thread-local `{}`", name()));
        }
        return match compiled_static(db, id) {
            Ok(it) => it
                .writable
                // alloc: why the function is not pure, a message
                .then(|| format!("reads the static `{}`, which can change", name())),
            Err(e) => {
                let name = name();
                // alloc: why the function is not pure, a message
                Some(format!("reads the static `{name}`, which blaze can't compile ({e})"))
            }
        };
    }
    let (func, args) = operand.fn_item()?;
    if diverges(db, func) {
        // A panic, the handler core declares as foreign among them: what it prints is no
        // result.
        return None;
    }
    if let Some(symbol) = link_name_for(db, target, func) {
        // alloc: why the function is not pure, a message
        return (!pure_foreign(&symbol)).then(|| format!("calls the foreign function `{symbol}`"));
    }
    if FunctionSignature::is_intrinsic(db, func) {
        return None;
    }
    if let Some(what) = gives_address(db, func, false) {
        // alloc: why the function is not pure, a message
        return Some(format!("calls `{}`, which {what}", function_name(db, func, args)));
    }
    let func = match func.lookup(db).container {
        ItemContainerId::TraitId(_) => {
            if rustc_type_ir::TypeVisitableExt::has_param(&args) {
                return None;
            }
            let (either::Either::Left(it), _) = (match crate::impl_method(db, env, func, args) {
                Ok(found) => found,
                Err(why) => return Some(why),
            }) else {
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

fn is_library(db: &Db, krate: Crate) -> bool {
    matches!(
        krate.data(db).origin,
        base_db::CrateOrigin::Lang(LangCrateOrigin::Core | LangCrateOrigin::Alloc)
    )
}

fn library_name(db: &Db, body: LibraryBody<'_>) -> String {
    let no_args = GenericArgs::new_from_slice(&[]);
    match body {
        // alloc: why a function is not pure, a message
        LibraryBody::Fn(func) => format!("`{}`", function_name(db, func, no_args)),
        LibraryBody::Closure(closure) => match closure.loc(db).owner.as_function() {
            // alloc: why a function is not pure, a message
            Some(func) => format!("a closure in `{}`", function_name(db, func, no_args)),
            // alloc: why a function is not pure, a message
            None => "a closure".to_owned(),
        },
    }
}

/// Says what `at`, found from a function of core or alloc by way of `parent`, does that is
/// not pure: `what`, or "reaches `a` → `b`, which" it, by the bodies on the way.
fn library_told<'db>(
    db: &'db Db,
    parent: &HashMap<LibraryBody<'db>, LibraryBody<'db>, Global>,
    at: LibraryBody<'db>,
    what: String,
) -> String {
    // alloc: why a function is not pure, a message
    let mut chain = vec![at];
    while let Some(&up) = parent.get(chain.last().unwrap()) {
        chain.push(up);
    }
    chain.pop();
    chain.reverse();
    match chain.as_slice() {
        [] => what,
        _ => {
            // alloc: why a function is not pure, a message
            let names: Vec<String> = chain.iter().map(|&it| library_name(db, it)).collect();
            let shown = match names.len() {
                ..=3 => names.join(" → "),
                // alloc: why a function is not pure, a message
                _ => format!("{} → … → {}", names[0], names[names.len() - 1]),
            };
            // alloc: why a function is not pure, a message
            format!("reaches {shown}, which {what}")
        }
    }
}

/// The symbol a foreign static is linked by, if `id` is one.
fn foreign_static(db: &Db, target: &Target, id: StaticId) -> Option<String> {
    super::static_link_name_for(db, target, id)
}

/// Why the arguments of the closure can't be what a result is kept by, and whether one holds
/// the reference counts of an `Rc` or an `Arc`.
fn arguments_trouble(
    db: &Db,
    closure: &Instance,
    env: &StoredParamEnvAndCrate,
) -> (Option<String>, bool) {
    // clone: the query takes its key by value; a stored param env is a reference count
    let Ok(types) = super::body_types(db, *closure, env.clone()).as_ref() else {
        return (None, false);
    };
    let interner = DbInterner::new_with(db, env.krate);
    let mut seen = HashSet::new_in(Global);
    let mut counted = false;
    // The first parameter is the closure itself, which captures nothing.
    for ty in types.params.iter().skip(1) {
        let ty = ty.as_ref();
        if let Some(why) = trouble(db, interner, ty, false, &mut seen, &mut counted) {
            let name = hir_ty_print::rustc_type_name(db, ty).unwrap_or_default();
            // alloc: the error, why the function is not cached
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
    db: &'db Db,
    interner: DbInterner<'db>,
    ty: Ty<'db>,
    in_library: bool,
    seen: &mut HashSet<Ty<'db>, Global>,
    counted: &mut bool,
) -> Option<&'static str> {
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
        TyKind::RawPtr(..) => Some("holds a raw pointer"),
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
            let mut variants = match def.def_id() {
                AdtId::StructId(id) if Some(id) == lang.UnsafeCell => {
                    return Some(
                        "can change behind a shared reference: it holds an `UnsafeCell`, as \
                         `Cell`, `RefCell`, `Mutex` and atomics do",
                    );
                }
                AdtId::StructId(id) => either::Either::Left(std::iter::once(VariantId::from(id))),
                AdtId::EnumId(id) => either::Either::Right(
                    id.enum_variants(db).variants.iter().map(|(_, it)| VariantId::from(it.0)),
                ),
                AdtId::UnionId(_) if !library => return Some("holds a union"),
                AdtId::UnionId(id) => either::Either::Left(std::iter::once(VariantId::from(id))),
            };
            let in_fields = variants.find_map(|variant| {
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
        TyKind::Dynamic(..) => Some("is a trait object"),
        TyKind::Alias(..) if in_library => None,
        _ => Some("is of a type blaze can't see into"),
    }
}

impl<'db> FunctionCx<'_, 'db> {
    /// A call of `__blaze_cache_code::<F>`: writes the code of the closure `F` to `dst`, as
    /// the `Code` of `blaze::__private` has it. That is two words of hash, then the name of
    /// the function, or why it can't be cached when the hash is zero. A body blaze can't give
    /// a code identity is an error naming the function ([`Uncached::Refused`]), and so is one
    /// that reaches code blaze can't compile and nothing impure ([`Uncached::Uncompiled`]).
    pub(super) fn cache_code(&mut self, generic_args: GenericArgs<'db>, dst: Value) -> Result<()> {
        let db = self.cg.db;
        let ty = generic_args.type_at(0);
        let TyKind::Closure(id, args) = ty.kind() else {
            // alloc: the error's message
            let code = hir_ty_mir::mir::CACHE_CODE;
            return Err(CodegenError::Unsupported(format!("`{code}` of `{ty:?}`")));
        };
        let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
        let parent = super::erase_regions(self.interner, parent);
        let closure = Instance {
            def: InstanceDef::Closure(id.0.as_id()),
            args: parent.store(),
            checked: self.cg.checked,
        };
        let env = closure.home_env(db, &self.env.store());
        let (hash, text) = match cache_code(db, closure, env) {
            Ok(hash) => (*hash, cached_name(db, &closure)),
            Err(Uncached::Impure(why)) => {
                tracing::info!(target: "blaze", "{why}");
                // clone: the constant's bytes own the text; the query keeps its own
                (0, why.clone())
            }
            // clone: the error owns its text; the query keeps its own
            Err(Uncached::Refused(why)) => return Err(CodegenError::Refused(why.clone())),
            // alloc: the error's message
            Err(Uncached::Uncompiled(it)) => return Err(CodegenError::Refused(it.to_string())),
        };
        let words = [hash as u64, (hash >> 64) as u64];
        for (i, word) in words.into_iter().enumerate() {
            let word = self.builder.ins().iconst(types::I64, word as i64);
            self.builder.ins().store(MEM, word, dst, 8 * i as i32);
        }
        let len = self.builder.ins().iconst(self.cg.target.pointer(), text.len() as i64);
        let data = self.cg.define_const(ConstBlob {
            bytes: text.into_bytes(),
            // alloc: empty, allocates nothing
            relocs: Vec::new(),
            // alloc: empty, allocates nothing
            fn_relocs: Vec::new(),
            align: 1,
            // alloc: empty, allocates nothing
            type_ids: Vec::new(),
        })?;
        let data = self.cg.declare_data_in_func(data, self.builder.func);
        let text = self.builder.ins().symbol_value(self.cg.target.pointer(), data);
        let width = self.cg.target.pointer().bytes() as i32;
        self.builder.ins().store(MEM, text, dst, 16);
        self.builder.ins().store(MEM, len, dst, 16 + width);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!pure_foreign("__errno_location"));
    }
}
