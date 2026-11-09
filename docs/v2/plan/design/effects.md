---
status: plan
summary: "#[blaze::nopanic], #[blaze::noalloc] and #[blaze::nonblocking]: effects checked over MIR on pure's walk, and where #[blaze::constant_time] would sit."
date: 2026-11-02
---
# Effect attributes: `nopanic`, `noalloc`, `nonblocking`

Three blaze extensions (AGENTS.md rule 4: opt-in, inert under rustc with
`#![feature(register_tool)] #![register_tool(blaze)]`, changing no code that blaze builds) that
each promise one thing about a function and everything it can call, and a deny-by-default lint
per attribute that holds the promise:

| attribute | promise | lint |
|---|---|---|
| `#[blaze::nopanic]` | it cannot panic or abort | `blaze::nopanic` |
| `#[blaze::noalloc]` | it never calls the global allocator | `blaze::noalloc` |
| `#[blaze::nonblocking]` | it never blocks the thread | `blaze::nonblocking` |

They are checked on the one walk that `#[blaze::pure]` uses ([pure-fns.md](pure-fns.md)): one
memoized summary per monomorphized instance, carrying an `EffectSet` (one bit per kind of
effect, `hir_ty_pure::effects::Rule`) with the shortest witness of each. An attribute is a mask
over those kinds (`EffectDef::refuses`); checking one reads the root's summary. These effects
add kinds to the vocabulary and data to the tables; the walk itself is pure's.

Code: `crates/hir-def/src/attrs.rs` (the attribute), `crates/hir-ty-pure/src/effect_attrs.rs`
(the definitions, the site rules and the tables), `crates/blaze-lints/src/blaze/effects.rs` (the
lints).

## The attributes

`#[blaze::nopanic]`, `#[blaze::noalloc]`, `#[blaze::nonblocking]`, each on a function, a method,
a foreign function or a trait, and each with one optional argument:

- A word: checked. On a trait: every method of every impl of the trait is checked as if marked.
- `(trusted = "why")`: not checked; the effect's kinds are taken off every call of it (and, on a
  trait, of every impl's method and every call through `dyn` of it). Its body is still walked
  for the other effects. The reason is recorded in the proof (`EffectProof::trusted`, sorted by path)
  so a reviewer reads every promise made by hand that a proof rests on.
- Anything else: refused by the lint, naming the text.

`AttrFlags::NOPANIC` is bit 97, `NOALLOC` 98, `NONBLOCKING` 99 (after `BLAZE_BITS`, 96).
`AttrFlags::effect_attr(db, owner, EffectWord)` reads the argument (one bit test for an item
without one); `PureAttr { Checked, Trusted(why), Malformed(text) }` is pure's. No codegen key or
signature reads the bits: a function without the attribute builds byte for byte as before,
and so does one with it.

## What each refuses

The kinds (`Rule`) the walk gives each site, and which attribute refuses which:

| kind | where the walk finds it | nopanic | noalloc | nonblocking |
|---|---|---|---|---|
| `Panic` | an `Assert` the MIR keeps (bounds, division, a UB check), the panic and abort entry points (`core::panicking::*`, `unwrap_failed`, `std::process::abort`, `handle_alloc_error`, ...), the `abort` intrinsic, the panic runtime's foreign symbols | yes | | |
| `Overflow` | an optional overflow `Assert` of a body that inherits the setting (`#[rustc_inherit_overflow_checks]`); one its own crate keeps is `Panic` | when the marked crate checks overflow | | |
| `Alloc` | `__rust_alloc*`, `alloc::alloc::{alloc, alloc_zeroed, dealloc, realloc}`, a `GlobalAlloc` impl's methods, `Global`'s and `System`'s `Allocator` methods, `malloc` and its family, `box_new` | | yes | |
| `AllocIn` | any other `Allocator` impl's method (`Vec::new_in(frame)`) | | | |
| `Block` | the blocking table (below) | | | yes |
| `ForeignUnwind` | a foreign function blaze knows nothing of whose ABI may unwind and that is not `#[rustc_nounwind]`; `asm!(.., options(may_unwind))` | yes | | |
| `ForeignUnknown` | a foreign function not in the foreign table; inline assembly | | yes | yes |
| `Unchecked` | what the walk can't follow (a `dyn` or function pointer from a parameter, a body that does not lower) | yes | yes | yes |

A refusal says the shortest call chain from the marked function to the site, each step with its
file and line, the rule, and what the last step does:
"`sched::Place::pick` is `#[blaze::nonblocking]`, but may block the thread: `sched::Place::pick`
(src/sched/mod.rs:860) → `Place::steal_for` (src/sched/mod.rs:915) → ..., which calls
`std::sync::Mutex::lock`, which waits for the lock". Then the fix: wait through the scheduler,
or mark the callee `trusted` with why.

What is under a panic entry point is not counted for any effect: the panic machinery (the hook's
lock, the payload's box) runs only while the job is failing (rule 17), and `nopanic` refuses the
call itself. Likewise the allocator's own body is not walked: `noalloc` refuses the call, and
`nonblocking` does not count the locks inside an allocator (the global allocator is treated as
non-blocking; code that must not even take malloc's lock is `noalloc`).

### Which MIR, which profile

The walk reads the monomorphized runtime MIR (`monomorphized_runtime_mir_body`: after drop
elaboration, before optimization). Its optional overflow `Assert`s are built per the crate the
body is written in (`transform::asserts`): a body of a crate built with `overflow-checks` has
them and they run, so they are `Rule::Panic` like a bounds check; a body of a crate built
without (std, a release profile) has none. The one case the body's crate doesn't decide is a
body that inherits the setting from the crate it is compiled in
(`#[rustc_inherit_overflow_checks]`: `u32::pow`, `<u32 as Add>::add` called from generic code):
its checks are `Rule::Overflow`, and `nopanic` takes the profile of the marked function's crate
(`hir_ty_mir::mir::overflow::crate_checks_overflow`) for them: with overflow checks (a dev
profile, or `overflow-checks = true`) it is `NOPANIC` and they are refused; without (release) it
is `NOPANIC_NO_OVERFLOW_CHECKS` and they pass. A lang crate's setting is `None` and is never
guessed: a marked function in one is a finding saying so. The same marked function can pass in
release and be refused in dev; the lint says which profile it checked. Known approximation: an
inherited body reached through a dependency whose `[profile.*.package]` overflow setting differs
from the marked crate's is judged by the marked crate's setting (cargo's default profiles set it
for every crate alike).

Constant-folded asserts are fine: the runtime MIR is not folded, so the check folds the one shape
that matters itself (`assert_rule`): an `Assert` whose condition is a constant, or a local the
block last assigns a comparison of two constant unsigned integers (what `arr[2]` on a `[T; 4]`
lowers to), is dropped when the constant holds. Any other `Assert` stays a refusal naming its
line.

No unwinding call into code not proven nopanic: every Rust callee is walked, so its panics are
its own refusal; a foreign callee is `Panic` or clean by the table, and one the table does not
know is `ForeignUnwind` when its ABI may unwind (`"Rust"`, `"C-unwind"`, `"system-unwind"`, ...).

### How `noalloc` tells an explicit allocator from the global one

By type, in MIR, at the call: a call that dispatches to a method of an `Allocator` impl is
`AllocIn` unless the impl's `Self`, the instance's arguments substituted and references taken
off, is `alloc::alloc::Global` or `std::alloc::System` (the global heap), which is `Alloc`; every
method of a `GlobalAlloc` impl is `Alloc`. So `Vec::new_in(frame).push(x)` reaches
`<&Frame as Allocator>::allocate`, `AllocIn`, which `noalloc` allows: AGENTS.md rule 9's
`Vec::new_in(frame)` / `Box::new_in(.., frame)` is exactly what a noalloc function may do. And
`Vec::new().push(x)` reaches `<Global as Allocator>::allocate`, `Alloc`, refused. The allocator
impl's own body is not walked (where a frame gets its next chunk is its business, not the
caller's). No data flow is needed: `Global` is a type argument of every collection that uses it.

### The blocking set, as data

`effect_attrs::ROWS` (functions by path, resolved once per crate through its def map) and
`effect_attrs::FOREIGN` (foreign functions by link symbol):

- std: `thread::{sleep, sleep_ms, sleep_until, park, park_timeout, park_timeout_ms}`,
  `JoinHandle::join`, `sync::{Mutex::lock, RwLock::{read, write}, Condvar::wait*, Barrier::wait,
  Once::{call_once, call_once_force, wait, wait_force}, OnceLock::{get_or_init, get_or_try_init,
  wait}, LazyLock::force}`, `mpsc::{Receiver::recv*, SyncSender::send}`,
  `process::{Child::wait*, Command::{output, status, spawn}}`,
  `net::{TcpStream::connect*, TcpListener::accept}`, `io::Stdin::{read_line, lock}`.
- registry crates, by crates.io name and only with a registry origin (a workspace crate named
  `lock_api` is not taken for it): `parking_lot_core::park`, `lock_api::{Mutex::lock,
  ReentrantMutex::lock, RwLock::{read, write, upgradable_read, read_recursive}}`,
  `parking_lot::{Condvar::wait*, Once::call_once*}`, `crossbeam_channel::{Receiver::recv*,
  Sender::send*, Select::{select, ready, ..}}`, `crossbeam_utils::sync::{Parker::park*,
  WaitGroup::wait}`.
- foreign: the blocking syscalls and libc calls (`read`/`write` and their vector and positional
  forms, `recv*`/`send*`, `accept`, `connect`, `poll`/`ppoll`/`select`/`epoll_wait`/`kevent`,
  `nanosleep` and the other sleeps, `wait*`, `syscall` (std's futex wait on Linux), `__ulock_wait`,
  `os_sync_wait_on_address*`, the `pthread_*` waits and locks, `sem_wait`, `open*`, `close`,
  `fsync`, `flock`, `fcntl`, `getrandom`, `getaddrinfo`, and the Windows waits, sleeps, SRW
  lock acquires and file and socket IO); the allocator's and the panic runtime's symbols; and the
  clean ones (memory, the clock, ids, thread-local keys, wakes, yields, page mapping), which only
  `pure` refuses.

`read` and `write` on a descriptor are blocking whatever the descriptor: the check can't prove an
`fd` is `O_NONBLOCK`. A wrapper that knows it says so: `#[blaze::nonblocking(trusted = "the fd
is O_NONBLOCK, set in Poller::new")]`. A `try_lock`, `try_recv` or `try_send` is not in the
table and is walked like any function: its body does not wait.

`check_rows_resolved(db, krate)` names every row of a crate's table that resolves to nothing; a
test over the pinned std, core and alloc holds the table to the toolchain, as `models` does.

## Where blaze uses them

Marked after reading the code (each holds by the rules above; the lint holds them from here on):

- `blaze_graph::sched::Place::{pick, take_own, steal_for, steal_half, nearest_parked,
  search_parked}`: `#[blaze::nonblocking]`. A free core's pick and steal, the path every pool
  thread takes between two jobs; the scheduler is a sans-IO core (rule 12) and must never wait.
  They allocate (their queues grow), so not `noalloc`.
- `blaze_graph::sched::cores::first_from`: `#[blaze::noalloc]`, `#[blaze::nonblocking]`. The
  bitmap scan under every steal and wake.
- `hir_def::nameres::facts::graph::ImportGraph::from_edges`: `#[blaze::noalloc]`. The import
  graph of every expansion round, built in the query's frame.

Not marked: `pool.rs`'s injector takes a `parking_lot` lock (`Inject::push`, `pop`,
`steal_into`), so a worker's search is not `nonblocking` and is not marked; the query loops
that use a frame also call the database, which allocates its memos from the global heap.

## Tests

Per effect, one crate that passes and one per kind of refusal, named for the property
(`crates/blaze-lints/src/blaze/effects_tests.rs`), plus `an_effect_attribute_changes_no_signature`
in hir-def: a function's signature and flags other than the effect bits are the same with the
attribute and without, so an unmarked crate builds byte for byte the same.

## `#[blaze::constant_time]` on the same seam (later, not this lane)

`constant_time` promises that the time a function takes, and the memory it touches, do not depend
on its secret inputs: no branch, index or variable-time instruction on a secret. It is dataflow,
not a call-graph effect: whether a `SwitchInt` or an index is refused depends on whether its
operand is tainted by a secret, which this walk does not track. How it sits on the seam:

- The attribute and its `trusted` door are the same: a bit in `AttrFlags`, `effect_attr`, an
  `EffectDef` in `EFFECTS`, a lint by the same `EFFECTS`-generic loop, and the same `Refusal`
  with its chain and `said()`. Secrets are named by a second attribute on parameters and fields
  (`#[blaze::secret]`).
- The kinds are new `Rule`s (`SecretBranch`, `SecretIndex`, `SecretDivide`, `VariableTime`),
  but the summary that gives them is not the walk's `EffectSet`: it is a per-instance taint
  summary, which arguments' secrecy flows to which results and which of them reach a branch,
  an index, a division or a call of a variable-time intrinsic. A summary per instance, memoized
  the same way, composed at each call site by substituting the callee's parameter taints. A
  call's summary is a function of argument taints, not a set, so it is its own query beside
  `instance_effects`, keyed the same (`(krate, body, args)`), and reuses the walk's resolution of
  each call site (dispatch, drop glue, closures) rather than walking again.
- It is checked on the optimized MIR the code generator lowers, never on the runtime MIR: a
  branch the optimizer turns into a `select`, or one it makes from a `select`, changes the
  answer; and LLVM can do the same below MIR, so the proof also pins the backend's flags for
  marked functions (`-x86-cmov-converter=false` and the like), recorded in the proof.

## Not done

- The blocking set is rows of the toolchain and five registry crates; any other crate's blocking
  wrapper over a foreign call is found through the foreign table (its `read`, `futex`), so the
  table is complete through its foreign leaves, but the message names the leaf.
- A trusted mark is per effect: a `pure(trusted)` function is still walked for blocking,
  allocation and panics.
