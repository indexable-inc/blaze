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

use std::borrow::Cow;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::LazyLock;

use base_db::{Crate, Db, LangCrateOrigin};
use blaze_hash::{FixedState, HashMap, HashSet};
use blaze_query::db::{AsId, FromId};
use cranelift_codegen::ir::{InstBuilder, Value, types};
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup, StaticId, VariantId,
    signatures::{
        FunctionSignature, StaticFlags, StaticSignature, StructSignature, TraitSignature,
    },
};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind};

use hir_ty::{
    InferBodyId,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    intrinsic::Intrinsic,
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
    InstanceDef, MEM, Requests, Result, compiled_instance, compiled_root, compiled_static,
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
/// Whether `BLAZE_FN_CACHE_TRACE` asks for what the walk goes through, read once a process.
static TRACE: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("BLAZE_FN_CACHE_TRACE").is_some());

/// Whether `func` is the function a cached function asks for the code of its body.
pub(super) fn is_code(db: &Db, func: FunctionId) -> bool {
    FunctionSignature::of(db, func).name.as_str() == CODE
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
fn cached_closure_of<'db>(
    db: &'db Db,
    func: FunctionId,
) -> Option<InternedClosureId<'db>> {
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

/// The hash of the code of the closure a cached function keeps its body in and of all it
/// reaches, or why the function can't be cached.
#[blaze_query::tracked(returns(ref))]
pub(super) fn cache_code(
    db: &Db,
    closure: Instance,
    env: StoredParamEnvAndCrate,
) -> std::result::Result<u128, String> {
    let (trouble, counted) = arguments_trouble(db, &closure, &env);
    let mut walk = Walk::new(db, counted);
    // clone: the root node owns its key; a stored param env is a reference count
    let root = walk.reach(Node::Instance(closure, env.clone()));
    let mut next = 0;
    while next < walk.nodes.len() {
        walk.hash_node(next as u32);
        next += 1;
    }
    if let Some(why) = walk.impure(root).or(trouble) {
        let name = cached_name(db, &closure);
        // alloc: the error, why the function is not cached
        return Err(format!("`{name}` is not cached: {why}"));
    }
    let mut hasher = rustc_stable_hash::StableSipHasher128::new();
    // The nightly: what is outside the checkout is hashed by its stable name, which a
    // nightly's std can change the code of.
    let toolchain = crate::v2::toolchain::NIGHTLY;
    (FORMAT, super::target::Target::current().triple(), toolchain).hash(&mut hasher);
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

/// What `instance` of the cached function `func` writes as the code of its body
/// ([`FunctionCx::cache_code`]): the hash of all its closure reaches, or why it is not cached.
/// `None` for a function that asks for no closure's code. The function's own code hash takes
/// its closure by interface, so a key of its code must take this beside it.
pub(super) fn written_code<'db>(
    db: &'db Db,
    func: FunctionId,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
) -> Option<&'db std::result::Result<u128, String>> {
    let id = (*cached_closure_of(db, func))?;
    let closure = Instance {
        def: InstanceDef::Closure(id.as_id()),
        args: instance.args,
        checked: instance.checked,
    };
    let env = Homes::default().home(db, &closure, env);
    Some(cache_code(db, closure, env))
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

impl Found {
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
    hir_ty_mir::mir::type_name::rustc_function_name(db, func, args.as_slice())
        // alloc: a name is owned as `rustc_function_name` gives it: a leaf's key, or a message
        .unwrap_or_else(|| FunctionSignature::of(db, func).name.as_str().to_owned())
}

pub(crate) fn crate_name(db: &Db, krate: Crate) -> String {
    // alloc: the name of a static or a message; a lookup compares `crate_name_str`
    crate_name_str(db, krate).to_owned()
}

/// [`crate_name`], borrowed.
fn crate_name_str(db: &Db, krate: Crate) -> &str {
    krate.extra_data(db).display_name.as_ref().map_or("?", |it| it.as_str())
}

/// What the hash goes through: the functions and statics compiled, and what has no code of
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
    /// Its edges in [`Walk::edges`], pushed together while it is hashed.
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
    parent: HashMap<LibraryBody<'db>, LibraryBody<'db>>,
    seen: HashSet<LibraryBody<'db>>,
    queue: std::collections::VecDeque<LibraryBody<'db>>,
    calls: Vec<LibraryBody<'db>>,
}

struct Walk<'db> {
    db: &'db Db,
    homes: Homes,
    nodes: Vec<Seen>,
    /// Every node's edges, a node's together: [`Seen::edges`] says where.
    edges: Vec<Edge>,
    /// Where in `nodes` each node is, by the hash of the node there.
    index: hashbrown::HashTable<u32>,
    /// What each node is when hashed, in the order the nodes were found.
    segments: Vec<u128>,
    /// Hashing for [`tests_reach`]: nothing is told of purity, every function of a crate
    /// that can change is gone into, and a segment leaves out where its edges go, which
    /// each root hashes in an order of its own.
    tests: bool,
    /// The nodes blaze can't compile, whose code tells nothing of what they do.
    uncompiled: HashSet<u32>,
    /// For [`tests_reach`], what each node is that makes a test's end depend on more than its
    /// code: of which kind, what by name, and why.
    kinds: HashMap<u32, (Kind, String, String)>,
    /// Where a node refers to the next by a site, as [`Walk::site_location`] found it.
    locations: std::cell::RefCell<HashMap<(u32, Site), Option<String>>>,
    /// An argument holds a reference count, which what reads one gives and the key does not
    /// tell: two arguments can share a value or not.
    counted: bool,
    /// What each function of core and alloc known by its name does that is not pure.
    library: HashMap<FunctionId, Option<String>>,
    library_walk: LibraryWalk<'db>,
    /// Whether a static chooses a function by the CPU, as [`dispatch`] tells.
    dispatch: std::cell::RefCell<HashMap<StaticId, bool>>,
}

/// A body of core or alloc looked through: a function's, or a closure's.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum LibraryBody<'db> {
    Fn(FunctionId),
    Closure(InternedClosureId<'db>),
}

impl<'db> Walk<'db> {
    fn new(db: &'db Db, counted: bool) -> Walk<'db> {
        Walk {
            db,
            homes: Homes::default(),
            // alloc: the walk's table of nodes, grown once a node found (a walk is a miss)
            nodes: Vec::new(),
            // alloc: the walk's edges, grown once an edge found (a walk is a miss)
            edges: Vec::new(),
            index: hashbrown::HashTable::new(),
            // alloc: a hash a node, grown once a node hashed (a walk is a miss)
            segments: Vec::new(),
            tests: false,
            uncompiled: HashSet::default(),
            kinds: HashMap::default(),
            locations: Default::default(),
            counted,
            library: HashMap::default(),
            library_walk: LibraryWalk::default(),
            dispatch: Default::default(),
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
            let env = self.homes.home(db, &closure, env);
            return (Node::Instance(closure, env), site, false);
        }
        let name = || function_name(db, func, instance.args.as_ref());
        if let Some(symbol) = link_name(db, func) {
            let why =
                // alloc: why the function is not pure, a message, once a leaf
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
                // clone: the node owns its key; a stored param env is a reference count
                return (Node::Instance(*instance, env.clone()), site, false);
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
            if let Some(what) = library_body(db, at, &mut walk.calls) {
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
        self.library.entry(func).insert_entry(Some(told)).into_mut().as_deref()
    }

    /// Hashes node `at`, the next not hashed, and finds its edges.
    fn hash_node(&mut self, at: u32) {
        let db = self.db;
        if *TRACE {
            eprintln!("fn-cache: {at} {}", describe(db, &self.nodes[at as usize].node));
        }
        let start = self.edges.len() as u32;
        self.nodes[at as usize].edges = start..start;
        let mut hasher = rustc_stable_hash::StableSipHasher128::new();
        match &self.nodes[at as usize].node {
            Node::Root(func) => {
                let func = *func;
                0u8.hash(&mut hasher);
                match compiled_root(db, func) {
                    Ok(compiled) => {
                        self.function_code(at, &mut hasher, compiled);
                        self.look_into(at, InferBodyId::DefWithBodyId(func.into()), None);
                    }
                    Err(e) => self.cannot(at, &mut hasher, e),
                }
            }
            Node::Instance(instance, env) => {
                // clone: the query takes its key by value; a stored param env is a reference count
                let (instance, env) = (*instance, env.clone());
                1u8.hash(&mut hasher);
                match compiled_instance(db, instance, env) {
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
                let id = *id;
                2u8.hash(&mut hasher);
                match compiled_static(db, id) {
                    Ok(compiled) => {
                        if compiled.writable {
                            let what = describe(db, &self.nodes[at as usize].node);
                            // alloc: why the static is not pure, a message, once a static
                            let why = format!("reads {what}, which can change");
                            self.nodes[at as usize].why = Why::of_user(why);
                        }
                        if self.tests
                            && !self.kinds.contains_key(&at)
                            && let Some(reason) = self.static_reason(id, compiled.writable)
                        {
                            self.kinds.insert(at, reason);
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
        // alloc: the error's message, on the path of a node that did not compile
        self.nodes[at as usize].why = Why::own(format!("can't be compiled by blaze ({e})"));
        // alloc: the error's text, on the path of a node that did not compile
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
            base_db::CrateOrigin::Lang(_) => crate_name_str(db, krate) == "hashbrown",
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
                    if self.tests && !self.in_toolchain(at) && !self.kinds.contains_key(&at) {
                        let what = describe(db, &self.nodes[at as usize].node);
                        // alloc: what the test is told, once a node with assembly
                        let why = format!("it reaches {what}, which has assembly in it");
                        let what = without_arguments(what.trim_matches('`'));
                        self.kinds.insert(at, (Kind::Asm, what, why));
                    }
                }
                FuncTarget::Instance(instance, env) => {
                    let (node, site, codec) = self.function(at, instance, env);
                    let to = self.edge(at, node, site, codec);
                    3u8.hash(hasher);
                    if !self.tests {
                        to.hash(hasher);
                    } else if !self.kinds.contains_key(&to)
                        && let Some(reason) = self.called_reason(instance)
                    {
                        self.kinds.insert(to, reason);
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
                            let name = TraitSignature::of(db, *id).name.as_str();
                            // alloc: `rustc_type_name` gives a name owned, once a vtable request
                            let args: smallvec::SmallVec<[_; 4]> = args
                                .as_ref()
                                .iter()
                                .map(|it| {
                                    it.ty().and_then(|ty| {
                                        hir_ty_mir::mir::type_name::rustc_type_name(db, ty)
                                    })
                                })
                                // alloc: inline up to four; the names are owned as given
                                .collect();
                            (crate_name_str(db, id.krate(db)), name, args)
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
                    } else if !self.kinds.contains_key(&to)
                        && foreign_static(db, *id).is_some()
                        && let Some(reason) = self.static_reason(*id, false)
                    {
                        self.kinds.insert(to, reason);
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
            if self.kinds.contains_key(&at) {
                return;
            }
            let cache = |id| self.dispatch_cache(id);
            if let Some(span) = fn_from_data(db, body, &cache) {
                let place = location(db, body.owner, span)
                    // alloc: what the test is told, once a node that calls such a pointer
                    .map(|it| format!(", at {it}"))
                    .unwrap_or_default();
                let what = describe(db, &self.nodes[at as usize].node);
                // alloc: what the test is told, once a node that calls such a pointer
                let why = format!(
                    "it reaches {what}, which calls a function pointer made of data (a \
                     `transmute`){place}"
                );
                let what = without_arguments(what.trim_matches('`'));
                self.kinds.insert(at, (Kind::Indirect, what, why));
            }
            return;
        }
        if let Some((why, span)) = raw_use(body) {
            let place = location(db, body.owner, span);
            self.nodes[at as usize].why = Some(Why { text: why.into(), of_user: false, place });
        }
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
                // alloc: what a test is told, once a node
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
                // alloc: what a test is told, once a node
                format!("it calls the foreign function `{symbol}`, whose code blaze can't see");
            return Some((Kind::Ffi, symbol, why));
        }
        if makes_seed(db, func) {
            let name = function_name(db, func, instance.args.as_ref());
            // alloc: what a test is told, once a node
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
                // alloc: what a test is told, once a node
                let why = format!("it reaches `{symbol}`, which {what}");
                return Some((kind, symbol, why));
            }
            if toolchain || symbol.starts_with("__rust_") {
                return None;
            }
            let why =
                // alloc: what a test is told, once a node
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
        // alloc: what a test is told, once a node
        let why = format!(
            "it uses the static `{name}`, which can change, and other tests of the process with it"
        );
        Some((Kind::GlobalState, name, why))
    }

    /// Why what `from` reaches is not pure, by the shortest way to the first that is not:
    /// the edges to the codec are left out.
    fn impure(&self, from: u32) -> Option<String> {
        let mut parent: HashMap<u32, (u32, usize)> = HashMap::default();
        let mut seen = HashSet::default();
        let mut queue = std::collections::VecDeque::from([from]);
        seen.insert(from);
        while let Some(at) = queue.pop_front() {
            let seen_at = &self.nodes[at as usize];
            if let Node::Leaf(_, Some(text)) = &seen_at.node {
                return Some(self.told(at, &parent, text, true, None));
            }
            if let Some(why) = &seen_at.why {
                return Some(self.told(at, &parent, &why.text, why.of_user, why.place.as_deref()));
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
        None
    }

    /// Says why `at`, found by way of `parent`, is not pure: "it reaches `a` → `b`, which
    /// calls the foreign function `write`, at src/lib.rs:4:5". `text` is what [`Why`] says,
    /// of the user when `of_user`, at `place` when that is told.
    fn told(
        &self,
        at: u32,
        parent: &HashMap<u32, (u32, usize)>,
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
pub(super) fn kept_a_call(
    db: &Db,
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
fn seeded_order(
    db: &Db,
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

/// Whether `func` is a method of std's `Backtrace`, or of a trait for it.
fn of_backtrace(db: &Db, func: FunctionId) -> bool {
    if !matches!(func.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Std)) {
        return false;
    }
    let ItemContainerId::ImplId(id) = func.lookup(db).container else { return false };
    is_std_struct(db, db.impl_self_ty(id).skip_binder(), "Backtrace")
}

/// Whether a static of `ty` that can change is state other tests of the process can change:
/// all but a cell set once (`LazyLock`, `OnceLock`, once_cell's and lazy_static's), which code
/// the test reaches sets to what that code gives, unless it holds a lock, a cell or an atomic.
fn shared<'db>(db: &Db, ty: Ty<'db>) -> bool {
    const ONCE: &[&str] = &["LazyLock", "OnceLock", "LazyCell", "OnceCell", "Lazy", "Once"];
    const CHANGES: &[&str] =
        &["Mutex", "RwLock", "ReentrantMutex", "RefCell", "Cell", "UnsafeCell", "SyncUnsafeCell"];
    let struct_name = |ty: Ty<'db>| {
        let TyKind::Adt(def, args) = ty.kind() else { return None };
        let AdtId::StructId(id) = def.def_id() else { return None };
        Some((StructSignature::of(db, id).name.as_str(), args))
    };
    let Some((name, args)) = struct_name(ty) else { return true };
    if !ONCE.contains(&name) {
        return true;
    }
    let Some(inner) = args.iter().find_map(|it| it.ty()) else { return false };
    struct_name(inner)
        .is_some_and(|(name, _)| CHANGES.contains(&name) || name.starts_with("Atomic"))
}

/// Where `body` makes a function pointer of data, with a `transmute`: what it calls through
/// it is no function the code reached names, and can be any. Not one of what it loaded from a
/// static `cache` says chooses a function ([`dispatch`]).
fn fn_from_data(
    db: &Db,
    body: &MirBody<'_>,
    cache: &impl Fn(StaticId) -> bool,
) -> Option<MirSpan> {
    for (_, block) in body.basic_blocks.iter() {
        let Some(terminator) = &block.terminator else { continue };
        let TerminatorKind::Call { args, destination, intrinsic, .. } = &terminator.kind else {
            continue;
        };
        if matches!(intrinsic, Some(Intrinsic::Transmute | Intrinsic::TransmuteUnchecked))
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
fn sizes_stack(db: &Db, func: FunctionId) -> bool {
    is_std_function(db, func, "min_stack")
}

fn is_std_function(db: &Db, func: FunctionId, name: &str) -> bool {
    matches!(func.krate(db).data(db).origin, base_db::CrateOrigin::Lang(LangCrateOrigin::Std))
        && FunctionSignature::of(db, func).name.as_str() == name
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

fn static_name(db: &Db, id: StaticId) -> String {
    let mut modules = smallvec::SmallVec::<[_; 8]>::new();
    if let ItemContainerId::ModuleId(module) = id.lookup(db).container {
        let mut module = Some(module);
        while let Some(at) = module {
            if let Some(name) = at.name(db) {
                modules.push(name);
            }
            module = at.containing_module(db);
        }
    }
    // alloc: a static's name, a leaf's key or what a message says
    let mut path = String::from(crate_name_str(db, id.krate(db)));
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
                    let ty = ty.and_then(|ty| hir_ty_mir::mir::type_name::rustc_type_name(db, ty));
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
pub(super) fn location(
    db: &Db,
    owner: InferBodyId<'_>,
    span: MirSpan,
) -> Option<String> {
    let InferBodyId::DefWithBodyId(owner) = owner else { return None };
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
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    operand: &Operand,
    calls: &mut Vec<LibraryBody<'db>>,
) -> Option<String> {
    if let OperandKind::Static(id) = &operand.kind {
        let id = *id;
        if let Some(name) = foreign_static(db, id) {
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
    if let Some(symbol) = link_name(db, func) {
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
    parent: &HashMap<LibraryBody<'db>, LibraryBody<'db>>,
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
fn foreign_static(db: &Db, id: StaticId) -> Option<String> {
    super::static_link_name(db, id)
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
    let mut seen = HashSet::default();
    let mut counted = false;
    // The first parameter is the closure itself, which captures nothing.
    for ty in types.params.iter().skip(1) {
        let ty = ty.as_ref();
        if let Some(why) = trouble(db, interner, ty, false, &mut seen, &mut counted) {
            let name = hir_ty_mir::mir::type_name::rustc_type_name(db, ty).unwrap_or_default();
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
    seen: &mut HashSet<Ty<'db>>,
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

/// Hashes compiled code without what tells where it was written: the lines it is of and
/// where its variables are.
fn hash_code(hasher: &mut impl Hasher, code: &Code) {
    use std::cell::RefCell;
    use std::fmt::Write as _;
    thread_local! {
        /// The text of an unwind table, hashed as text, a buffer reused by every function.
        // alloc: one buffer a thread, grown to the longest text and kept
        static UNWIND: RefCell<String> = const { RefCell::new(String::new()) };
    }
    let Code { signature, bytes, align, relocs, unwind, lines: _, slots: _ } = code;
    (signature, bytes, align).hash(hasher);
    relocs.hash_elf_types(hasher);
    UNWIND.with_borrow_mut(|text| {
        text.clear();
        write!(text, "{unwind:?}").expect("a `Debug` into a `String` does not fail");
        text.as_str().hash(hasher);
    });
    relocs.hash_as_module(hasher);
}

impl<'db> FunctionCx<'_, 'db> {
    /// A call of `__blaze_cache_code::<F>`: writes the code of the closure `F` to `dst`, as
    /// the `Code` of `blaze::__private` has it. That is two words of hash, then the name of
    /// the function, or why it can't be cached when the hash is zero.
    pub(super) fn cache_code(&mut self, generic_args: GenericArgs<'db>, dst: Value) -> Result<()> {
        let db = self.cg.db;
        let ty = generic_args.type_at(0);
        let TyKind::Closure(id, args) = ty.kind() else {
            // alloc: the error's message
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
        let (hash, text) = match cache_code(db, closure, env) {
            Ok(hash) => (*hash, cached_name(db, &closure)),
            Err(why) => {
                tracing::info!(target: "blaze", "{why}");
                // clone: the constant's bytes own the text; the query keeps its own
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
            // alloc: empty, allocates nothing
            relocs: Vec::new(),
            // alloc: empty, allocates nothing
            fn_relocs: Vec::new(),
            align: 1,
            // alloc: empty, allocates nothing
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

    use super::super::{
        debuginfo::{LineRow, SlotPlaces},
        fragment::Relocs,
    };
    use super::*;

    fn code(bytes: &[u8], lines: Vec<LineRow>) -> Code {
        Code {
            signature: Signature::new(CallConv::SystemV),
            bytes: bytes.to_vec(),
            align: 16,
            relocs: Relocs::default(),
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
