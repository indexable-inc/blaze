//! What a build tells whoever watches it of the functions it puts in the program: each one
//! once, as it is put in the object, with what it calls and where its code came from. The
//! build page draws the program's whole graph of functions from it (docs/web.md).
//!
//! Nothing of it runs until [`watch`] is given somewhere to say it.

use std::{
    hash::{Hash, Hasher},
    sync::{
        Arc, LazyLock, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use base_db::Crate;
use hir_def::{FunctionId, HasModule};
use hir_ty::db::HirDatabase;
use rustc_hash::{FxHashMap, FxHasher};

use crate::{Instance, InstanceDef, StoredParamEnvAndCrate};

/// A function of an object, as [`watch`]'s sink is told of it.
pub struct Placed {
    /// Its number in the object.
    pub id: u32,
    /// Which function it is, the same in every object of the process.
    pub key: u64,
    pub krate: String,
    pub name: String,
    /// The functions of the object it calls, by `id`.
    pub calls: Vec<u32>,
    pub how: How,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// Compiled by this build, in this much of a thread's time.
    Compiled { ns: u64 },
    /// Compiled with the others of its unit by LLVM.
    Unit,
    /// Compiled by an earlier build of the process, and unchanged since.
    Unchanged,
    /// Taken from the store.
    Stored,
}

/// What is told the functions of an object, a batch at a time: the number of the object,
/// whether it is of the whole program rather than a patch, and the batch.
pub type Sink = dyn Fn(u32, bool, Vec<Placed>) + Send + Sync;

static SINK: RwLock<Option<Arc<Sink>>> = RwLock::new(None);
static WATCHED: AtomicBool = AtomicBool::new(false);

/// Has the functions of every object from now on told to `sink`, or to nothing.
pub fn watch(sink: Option<Arc<Sink>>) {
    WATCHED.store(sink.is_some(), Ordering::Relaxed);
    *SINK.write().unwrap_or_else(|e| e.into_inner()) = sink;
}

/// A build begins: what the builds before it compiled is [`How::Unchanged`] in it.
pub fn begin() {
    if WATCHED.load(Ordering::Relaxed) {
        TOOK.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// What a compile is of, to tell the function it is put in the object as by.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Timed {
    Root(FunctionId),
    Instance(u64),
}

impl Timed {
    pub(crate) fn instance(instance: &Instance, env: &StoredParamEnvAndCrate) -> Timed {
        Timed::Instance(hash(&(instance, env)))
    }
}

/// How long each compile of the build took, of those that ran while it was watched.
static TOOK: LazyLock<Mutex<FxHashMap<Timed, u64>>> = LazyLock::new(Default::default);

/// Times a compile, from now until it is dropped, when the build is watched.
pub(crate) struct Timer(Option<(Timed, Instant)>);

impl Timer {
    pub(crate) fn of(what: impl FnOnce() -> Timed) -> Timer {
        Timer(WATCHED.load(Ordering::Relaxed).then(|| (what(), Instant::now())))
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some((what, start)) = self.0.take() {
            let ns = start.elapsed().as_nanos().try_into().unwrap_or(u64::MAX);
            TOOK.lock().unwrap_or_else(|e| e.into_inner()).insert(what, ns);
        }
    }
}

fn hash(value: &impl Hash) -> u64 {
    let mut hasher = FxHasher::default();
    value.hash(&mut hasher);
    hasher.finish()
}

static OBJECTS: AtomicU32 = AtomicU32::new(0);

/// The most functions told at once, and the longest one waits to be told.
const BATCH: usize = 4096;
const EVERY: Duration = Duration::from_millis(100);

/// What an object being built tells of its functions.
pub(crate) struct Watch {
    object: u32,
    whole: bool,
    sink: Arc<Sink>,
    batch: Vec<Placed>,
    since: Instant,
    crates: FxHashMap<Crate, String>,
}

/// What a function put in an object is.
pub(crate) enum Of<'a> {
    Root(FunctionId),
    Instance(&'a Instance, &'a StoredParamEnvAndCrate),
    /// An entry of the store whose instance the object does not know, by its name there.
    Named(&'a str),
}

impl Watch {
    /// The watch of a new object, of the whole program or a patch, when the build is watched.
    pub(crate) fn new(whole: bool) -> Option<Watch> {
        let sink = SINK.read().unwrap_or_else(|e| e.into_inner()).clone()?;
        Some(Watch {
            object: OBJECTS.fetch_add(1, Ordering::Relaxed),
            whole,
            sink,
            batch: Vec::new(),
            since: Instant::now(),
            crates: FxHashMap::default(),
        })
    }

    /// Tells of `id`, which is `of` and calls `calls`. `kept` is where its code came from
    /// when it was not compiled: when it was, the compile's time says so.
    pub(crate) fn placed(
        &mut self,
        db: &dyn HirDatabase,
        id: u32,
        of: Of<'_>,
        calls: Vec<u32>,
        kept: How,
    ) {
        let (key, timed, krate, name) = match of {
            Of::Root(func) => {
                let name = hir_ty_mir::mir::type_name::rustc_function_name(db, func, &[]);
                let name = name.unwrap_or_else(|| {
                    crate::FunctionSignature::of(db, func).name.as_str().to_owned()
                });
                (hash(&(0u8, func)), Some(Timed::Root(func)), Some(func.krate(db)), name)
            }
            Of::Instance(instance, env) => {
                let krate = match instance.def {
                    InstanceDef::Fn(func) => func.krate(db),
                    _ => env.krate,
                };
                let timed = Timed::instance(instance, env);
                let key = match instance.def {
                    // The instance of a function generic over no type or constant is the
                    // function.
                    InstanceDef::Fn(func)
                        if instance.args.as_ref().iter().all(|arg| arg.region().is_some()) =>
                    {
                        hash(&(0u8, func))
                    }
                    _ => hash(&(1u8, instance, env)),
                };
                (key, Some(timed), Some(krate), crate::backtrace_name(db, instance))
            }
            Of::Named(name) => (hash(&(2u8, name)), None, None, name.to_owned()),
        };
        let took =
            timed.and_then(|it| TOOK.lock().unwrap_or_else(|e| e.into_inner()).get(&it).copied());
        let how = match took {
            Some(ns) => How::Compiled { ns },
            None => kept,
        };
        let krate = match krate {
            Some(krate) => self
                .crates
                .entry(krate)
                .or_insert_with(|| crate::fncache::crate_name(db, krate))
                .clone(),
            None => String::new(),
        };
        self.batch.push(Placed { id, key, krate, name, calls, how });
        if self.batch.len() >= BATCH || self.since.elapsed() >= EVERY {
            self.flush();
        }
    }

    fn flush(&mut self) {
        self.since = Instant::now();
        if !self.batch.is_empty() {
            (self.sink)(self.object, self.whole, std::mem::take(&mut self.batch));
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.flush();
    }
}
