//! What a lazy build compiles ahead, and what waits for its first call.
//!
//! Every function of a lazy build is reached through its jump-table slot (codegen's
//! `Target::Function`), so its address, the one a function pointer, a vtable, a static, a
//! `#[no_mangle]` export or C code holds, is its slot's stub in blaze-link's stub region:
//! fixed when the image is laid out, the same before and after the function is compiled, and
//! the same address a whole live build gives it. Laziness only changes where the stub jumps.
//! So each case the program can observe is exact:
//!
//! - **Function pointers and their comparisons** (`fn` items cast to pointers, `ptr::fn_addr_eq`,
//!   callbacks handed to C, `qsort` comparators, signal handlers): the pointer is the stub;
//!   two pointers to one function compare equal because there is one stub per slot.
//! - **Vtable entries** are relocations to the stub, written when the vtable's data is placed.
//! - **Statics' initializers** holding function pointers: relocations to the stub, written
//!   when the static is placed, before the program starts.
//! - **TLS destructors** (`thread_local!` with drop, `__cxa_thread_atexit_impl`) and `atexit`
//!   handlers: registered as stub addresses; a first call during thread or process exit
//!   resolves like any other, since the run process's hook ([`crate::hook`]) uses no
//!   thread-local of its own. A first call from a signal handler resolves the same way only
//!   while the hook's ask is async-signal-safe, which waits on blaze-inproc's channel
//!   taking requests with no lock and no allocation (v2/requests/fastcompile.md,
//!   fastcompile-lazy 1).
//! - **What runs before `main` or around it**, which every run executes: the entry (the
//!   `main` symbol's shim and what it calls first) and every function a static in an
//!   initializer or finalizer section names (`.init_array`, `.ctors`, `.fini_array`,
//!   `.dtors`, Mach-O's `__mod_init_func` and `__mod_term_func`, PE's `.CRT$X*`, TLS
//!   callbacks among them) is compiled ahead ([`Ahead`]), so the start of a run never waits on
//!   a fetch and does not depend on when the channel is up.
//!
//! What a lazy build can't change, it doesn't: statics are all compiled and placed before
//! the run (they are data), and the front end's walk checks every instance a whole build
//! would compile (post-monomorphization const errors and layout errors among them) before
//! the run starts, so a program rustc rejects is rejected here too.

use blaze_codegen::{CompiledFunction, Init, InstanceKey, RelocKind, Static, Target};
use blaze_hash::HashMap;
use blaze_ids::names;
use blaze_items::{CodeHash, ItemKey};

use crate::{LazyId, MOST_LAZY};

/// A function of the build, as codegen's lazy walk lists it (`Built::walked`).
pub use blaze_codegen::driver::Walked as Function;
/// The rule codegen compiles ahead by, so [`Plan::ahead`] and what the build holds agree.
pub use blaze_codegen::driver::runs_at_start;

/// Why a function is compiled ahead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ahead {
    /// The program's entry, or what it calls first.
    Entry,
    /// Named by a static in an initializer or finalizer section.
    Start,
}

/// Why a plan could not be made.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PlanError {
    /// An entry function the walk did not describe.
    Unknown(InstanceKey),
    TooMany(usize),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Unknown(key) => write!(f, "the entry {key:?} is not among the build's functions"),
            PlanError::TooMany(count) => {
                write!(f, "{count} lazy functions, more than one session holds ({MOST_LAZY})")
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// A lazy build's functions: the ones compiled ahead, and the lazy ones by [`LazyId`], in
/// columns. Lookups by key hash once; by code hash binary-search a sorted column.
#[derive(Debug)]
pub struct Plan {
    ahead: Vec<(InstanceKey, Ahead)>,
    keys: Vec<InstanceKey>,
    items: Vec<ItemKey>,
    index: HashMap<InstanceKey, LazyId>,
    /// Every lazy function's code hash, sorted, with its id.
    by_code: Vec<(CodeHash, LazyId)>,
}

impl Plan {
    /// The plan of a build whose walk described `functions` (in the walk's order, which is
    /// deterministic), whose entry is `entry` and whose statics are `statics`.
    pub fn new(functions: &[Function], entry: &[InstanceKey], statics: &[Static]) -> Result<Plan, PlanError> {
        let mut ahead: HashMap<InstanceKey, Ahead> = HashMap::default();
        for key in entry {
            if !functions.iter().any(|it| it.key == *key) {
                return Err(PlanError::Unknown(*key));
            }
            ahead.insert(*key, Ahead::Entry);
        }
        for value in statics.iter().filter(|it| it.section.as_deref().is_some_and(runs_at_start)) {
            for key in named_functions(value) {
                ahead.entry(key).or_insert(Ahead::Start);
            }
        }
        let mut plan = Plan {
            ahead: Vec::with_capacity(ahead.len()),
            keys: Vec::with_capacity(functions.len().saturating_sub(ahead.len())),
            items: Vec::with_capacity(functions.len().saturating_sub(ahead.len())),
            index: HashMap::default(),
            by_code: Vec::with_capacity(functions.len().saturating_sub(ahead.len())),
        };
        plan.index.reserve(functions.len());
        for function in functions {
            if let Some(why) = ahead.get(&function.key) {
                if !plan.ahead.iter().any(|it| it.0 == function.key) {
                    plan.ahead.push((function.key, *why));
                }
                continue;
            }
            let next = LazyId(plan.keys.len() as u32);
            if let std::collections::hash_map::Entry::Vacant(slot) = plan.index.entry(function.key) {
                if plan.keys.len() as u64 >= u64::from(MOST_LAZY) {
                    return Err(PlanError::TooMany(functions.len()));
                }
                slot.insert(next);
                plan.keys.push(function.key);
                plan.items.push(function.item);
                plan.by_code.push((function.code, next));
            }
        }
        // Stable, and pushed in id order: sorted by code hash, then id.
        names::sort_by_name(&mut plan.by_code, |it| &it.0.0.0[..]);
        Ok(plan)
    }

    /// What is compiled before the run, and why.
    pub fn ahead(&self) -> &[(InstanceKey, Ahead)] {
        &self.ahead
    }

    /// The lazy functions, by id.
    pub fn lazy(&self) -> &[InstanceKey] {
        &self.keys
    }

    pub fn len(&self) -> u32 {
        self.keys.len() as u32
    }

    pub fn id(&self, key: &InstanceKey) -> Option<LazyId> {
        self.index.get(key).copied()
    }

    pub fn key(&self, id: LazyId) -> InstanceKey {
        self.keys[id.index()]
    }

    pub fn item(&self, id: LazyId) -> ItemKey {
        self.items[id.index()]
    }

    /// Appends to `out` the lazy functions whose code is one of `reached`: what history says a
    /// test runs (usercode's `Reached::reached`).
    pub fn reached(&self, reached: &[CodeHash], out: &mut Vec<LazyId>) {
        for code in reached {
            let first = self.by_code.partition_point(|it| it.0 < *code);
            out.extend(self.by_code[first..].iter().take_while(|it| it.0 == *code).map(|it| it.1));
        }
    }

    /// A clear bitmap of one bit per lazy function, for [`Plan::callees`].
    pub fn scratch(&self) -> Vec<u64> {
        vec![0; self.keys.len().div_ceil(64)] // alloc: once per caller, reused
    }

    /// Appends to `out` the lazy functions `function`'s code calls (a call or a tail call to
    /// the callee, not a reference to its address), each once, in the order of their first
    /// call: what it is likely to call next. `seen` is a clear [`Plan::scratch`], left clear.
    pub fn callees(&self, function: &CompiledFunction, seen: &mut [u64], out: &mut Vec<LazyId>) {
        let from = out.len();
        for reloc in &function.relocs {
            let (Target::Function(key) | Target::Direct(key)) = &reloc.target else { continue };
            if !is_call(function, reloc.at, reloc.kind) {
                continue;
            }
            if let Some(id) = self.id(key) {
                let (word, bit) = (id.index() / 64, 1u64 << (id.0 % 64));
                if seen[word] & bit == 0 {
                    seen[word] |= bit;
                    out.push(id);
                }
            }
        }
        for id in &out[from..] {
            seen[id.index() / 64] = 0;
        }
    }
}

/// Whether the relocation at `at` is a call's or a tail call's target: AArch64's `bl` and `b`
/// (`CALL26`, `JUMP26`), x86-64's `call rel32` (`e8`) and `jmp rel32` (`e9`).
fn is_call(function: &CompiledFunction, at: u32, kind: RelocKind) -> bool {
    match kind {
        RelocKind::Call26 => true,
        RelocKind::PcRel32 => {
            at.checked_sub(1)
                .and_then(|it| function.code.get(it as usize))
                .is_some_and(|op| matches!(op, 0xe8 | 0xe9))
        }
        _ => false,
    }
}

/// The functions a static's value and its data name.
fn named_functions(value: &Static) -> impl Iterator<Item = InstanceKey> + '_ {
    let own = match &value.init {
        Init::Bytes { relocs, .. } => Some(relocs),
        Init::Zeroed { .. } => None,
    };
    own.into_iter().flatten().chain(value.data.iter().flat_map(|it| it.relocs.iter())).filter_map(|it| match it.target {
        Target::Function(key) | Target::Direct(key) => Some(*key),
        _ => None,
    })
}
