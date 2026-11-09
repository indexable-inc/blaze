---
status: plan
summary: blaze's own standard library, written from scratch on core and alloc. No ambient authority (main receives an Io and every function touches only what it is handed), sans-IO underneath (one Io over io_uring, kqueue, IOCP and the simulation), and every blaze crate moves onto it.
date: 2026-11-09
---
# bstd: blaze's standard library

Rust's `std` lets any function open a file, read the environment, dial a host, read the clock or
start a thread, and nothing in its signature says so. It also bakes in choices blaze has already
replaced: its own locks, its own hasher, a global allocator, blocking calls with no deadline, and
no way to run a program under a simulated world.

`bstd` is blaze's own standard library. It is written from scratch on `core` and `alloc` and does
not use Rust's `std`. Three properties define it:

1. **Capabilities.** `main` receives an `Io`. A function can touch only what it is handed, so its
   signature is the complete list of what it can do.
2. **Sans-IO.** Every operation is an effect handed to one driver. The same `Io` runs on
   io_uring, kqueue, IOCP and the deterministic simulation, and code above it cannot tell which.
3. **Explicit context and memory.** Every task has a `Cx` that says how long it may run, whether
   it was cancelled, what span it is and where its scratch memory comes from. Nothing allocates
   from a hidden global heap: memory comes from an allocator the code was handed.
4. **One of each.** One lock, one map, one channel, one clock, one wait, one error shape: the
   ones blaze's rules already name.

Every blaze crate moves onto it, and later it is the `std` of the blaze edition. Nothing here is
built except the driver layer it starts from (`crates/blaze-core`).

```rust
fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    let data: Dir<ReadWrite> = io.fs().open_dir(cx, path!("data"))?;
    let cfg: Dir<ReadOnly> = data.read_only();               // narrow
    let key = StripeKey::from_env(&io.env().only(&[var!("STRIPE_KEY")]))?;
    let api = io.net().connect_only(host!("api.stripe.com:443"));   // one host
    let cx = cx.until(io.clock().after(Duration::from_secs(5)));   // a deadline only shrinks
    charge(&cx, &key, &api, &cfg)
}

// charge cannot open files outside cfg, write, read other variables, dial other hosts,
// run past its deadline, or allocate anywhere but its task's memory.
fn charge(cx: &Cx, key: &StripeKey, api: &Net, cfg: &Dir<ReadOnly>) -> Result<(), Error> {
    let mut body = Vec::new_in(cx.frame());                  // freed when the task ends
    ..
}
```

## Layers

Each layer uses only the ones below it.

| layer | module | what it is |
|---|---|---|
| 4 | `bstd::{fs, net, env, clock, rng, task, process, out}` | the capability types people use |
| 3 | `bstd::io` | `Io`: one handle to the process's driver; turns a call into an effect and parks the caller until its completion |
| 2 | `bstd::driver` | `Effect`, `Completion`, `Driver`, and the drivers: `Uring`, `Kqueue`, `Iocp`, `Sim` (today's `blaze-core`) |
| 1 | `bstd::sys` | raw system calls per target, the only home of `unsafe`; Linux by syscall number with no libc, macOS through libSystem, Windows through ntdll and kernel32 |
| 0 | `core`, `alloc` | the language's own libraries, unchanged |

Beside them, with no IO: `bstd::{lock, map, chan, mem, hash, text, path, time}`, which are the one
standard structure per need (named locks, blaze_hash maps, the queues and channels of rule
`standard-structures`, allocators by lifetime, BLAKE3, UTF-8 text, relative paths, durations).

The runtime (`bstd::rt`) is what `std` hides: the entry that builds the `Io` and calls
`main(io)`, the panic handler and unwinding (on `library/unwind`), the allocator, thread-local
storage, and arguments and environment read once from the process's start block.

## Io

`Io` is one concrete type, not a trait and not a generic parameter: code is not monomorphized per
driver and no signature carries `<I: Io>`. Inside is the driver the target's `cfg` chose, or the
simulation, picked when the `Io` is made. `Io::sim(seed)` gives a whole simulated world (an
in-memory file tree, a virtual clock, a seeded generator, scripted peers, injected failures and
crashes), so any program on bstd is a deterministic simulation test with no mocking.

Calls read as blocking code and never block a thread. A call pushes an effect, the calling fiber
parks on the scheduler, and the completion resumes it. There is no `async`, no function colour
and no second copy of each API.

A sans-IO core takes no `Io` at all. It takes bytes and returns bytes and effects, as rule
`sans-io-cores` already asks; with capabilities that is a fact of its signature, not a lint.

## Cx: the task's context

A capability says what code may touch. `Cx` says on whose behalf it runs. Every task has one, and
every call that can wait or allocate scratch takes `cx: &Cx` as its first parameter. It holds:

- **the deadline**, an instant on the task's `Clock`. `cx.until(deadline)` makes a child context
  whose deadline is the earlier of the two: like a capability, it only narrows.
- **cancellation.** A scope that fails or is dropped cancels its tasks; a wait under a cancelled
  `Cx` ends with `Stopped::Cancelled`. `cx.check()?` is the one line a long computation calls.
- **the span.** The task's name and its place in the trace. `cx.span("what")` opens a child, so
  every wait is named by construction (rule `every-wait-says-what`).
- **the frame.** `cx.frame()` is the task's bump allocator, freed whole when the task ends.

A sans-IO core takes no `Cx` and no `Io`: it cannot wait, so it needs neither. A function that
takes `&Cx` and no capability can wait and allocate scratch but cannot touch the world. `Cx` is
not ambient and is never fetched from a thread-local; `main` receives the root one.

## Capabilities

| type | made from | narrowing | gives |
|---|---|---|---|
| `Fs` | `io.fs()` | `open_dir(path)` | directories |
| `Dir<P>` | `Fs`, a `Dir` | `subdir`, `read_only` | files beneath it, by relative path only |
| `File<P>` | `Dir` | `read_only` | positional reads and writes, sync, mapping |
| `Net` | `io.net()` | `connect_only(host)`, `listen_only(addr)` | `Conn`, `Listener`, name lookup |
| `Env` | `io.env()` | `only(names)` | environment variables |
| `Args` | `io.args()` | | the command line |
| `Host` | `io.host()` | | the process id, core count, host name, signals, and exiting with a status |
| `Clock` | `io.clock()` | | `Instant`, wall time, `Deadline` |
| `Rng` | `io.rng()` | `fork()` | random bytes; seeded in the simulation |
| `Process` | `io.process()` | `only(programs)` | child processes |
| `Out` | `io.out()` | | standard input, output and error |
| `Mem` | `io.mem()` | | named arenas and caches against the memory budget |

Rules that hold for all of them:

- **Unforgeable.** Private fields; only `main`'s `Io` or narrowing makes one.
- **Narrowing never widens**, and it happens in the OS too: `read_only()` reopens the handle with
  fewer rights, so trusted `unsafe` code cannot write through it.
- **Permissions are types.** `P` is `ReadOnly`, `ReadWrite` or `ReadWriteExec`; methods are gated
  by `P: CanWrite`.
- **Paths are relative.** `Path` cannot hold a root or `..`; every open is beneath its `Dir`
  (`openat2` with `RESOLVE_BENEATH`, and its equal elsewhere). There is no current directory.
- **Never in a static.** A capability cannot be stored in a `static` or a thread-local, or the
  signature would stop being the whole truth. The `ambient` check refuses it.
- **Clone is allowed.** Copying authority you hold adds none.

**Nothing is global.** Everything a program can reach outside its own values comes down from
`main`'s `Io` and `Cx`:

- There is no `std::env::var`, no arguments function, no current directory, no process-wide
  clock, random source, exit, signal handler or standard stream: each is a capability above.
- **No global mutable state.** A `static` may hold only immutable data. A `static` with interior
  mutability (an atomic, a lock, a cell), a `static mut` and a thread-local are refused in a
  capabilities crate (the effect `global_state`): state shared across calls is a value someone
  owns and passes, or a context.
- **Spans and events go through `Cx`** (`cx.span(..)`, `cx.event(..)`); the recorder is reached
  from the root `Cx`, not from a global.
- Allocation is not ambient (below).

The one thing left ambient is a panic, which is the language's, and it fails its task.

### What replaces a thread-local

A thread-local is the wrong tool on fibers anyway: a task can resume on another thread after any
wait (rule `no-wait-across-thread-locals`). Each thing people use one for has an owner here:

| used for | instead |
|---|---|
| the "current" something (span, deadline, database, runtime) | `Cx`, or a `with` context |
| scratch buffers reused across calls | `cx.frame()` |
| state that belongs to one task | a local the task owns, passed down |
| per-thread caches and counters that avoid contention (allocator caches, statistics, shards of an interner) | `PerWorker<T>` |

`PerWorker<T>` is an ordinary value: one cache-line-padded slot per scheduler worker, made with
`PerWorker::new_in(mem, ..)` and owned and passed like anything else. `slots.with(cx, |slot| ..)`
gives `&mut T` for the worker the task is on. The closure gets no `Cx`, so it cannot wait, so the
task cannot move to another worker while it holds the slot (the `nonblocking` check enforces it).
`slots.iter()` reads every slot, for summing counters. This is the per-CPU variable of a kernel
and the shard of a shared-nothing runtime, without a global.

The runtime keeps one real thread-local, the pointer to the current worker, inside `bstd::rt`
(trusted); nothing else may.

**Secrets.** `#[derive(bstd::Secret)]` gives a type no `Debug`, `Display` or `Clone`, zeroes it on
drop, and hands its bytes only to functions of its own module.

## Tasks and waiting

There is no `async` and no `.await` in a capabilities crate. A function that waits is an ordinary
function. ([may-suspend](may-suspend.md) is the analysis underneath.)

- **One wait.** `Cx` has the single wait primitive: wait for this, until the deadline, with a
  typed outcome (`Ready`, `Stopped::Deadline`, `Stopped::Cancelled`). Locks, channels, sleeps and
  every IO call are built on it, which is why each takes a `Cx`. Nothing can wait for ever (rules
  `bounded-progress`, `deadline-waits`).
- **Structured only.** `cx.scope(|s| s.spawn("name", |cx| ..))`. The child's `cx` is borrowed
  from the scope, so a task cannot outlive it; children may borrow from the parent, as
  `std::thread::scope` allows. There is no detached task and no `io.tasks()`: starting work is
  something a `Cx` does.
- **`spawn` means "may run in any order"; `spawn_concurrent` means "must overlap"** and is a
  compile error where the layout cannot overlap.
- **Spawn is eager, and cancellation is a value.** A cancelled task learns it only as
  `Stopped::Cancelled` from its next wait. Nothing is cancelled by being dropped, and user code
  never holds a task that has not started or is half run.
- **`Drop` may wait.** A value holds the capability it was made with, so closing a file or
  flushing a connection on drop goes through the same wait.
- **A panic fails its task** with a typed error to its scope (rule `panics-fail-one-job`).
- **A child process is handed capabilities.** `process.spawn(cx, program, dirs, env)` gives the
  child exactly those; the driver's sandbox enforces it. This is the sandbox blaze already builds
  for build scripts, as one API.
- **Code that uses `async` still builds** as the pinned nightly builds it. The bridge is one
  function: wait on a `Future` (poll it, park the task while it is pending, its waker unparks).

### Layouts

How a waiting task's frames are stored is the **layout**. Programs behave the same in every
layout they compile under: same outputs, same order of cancellation, drops and panics.

| layout | a task is | a wait |
|---|---|---|
| `Inline` | run to completion at `spawn` | nothing else runs; no runtime at all |
| `Threads` | a pool thread | blocks the thread |
| `Fibers` | its own stack (the scheduler of section 6) | switches stacks |
| `Frames` | a bump slab holding the frames of its functions that can wait, each made on call and freed on return; code that cannot wait runs on the worker's stack | returns to the scheduler |

`Frames` is the long-term one: no stack per task, and recursion and `dyn` calls work because the
callee makes its own frame. Frames never move, so there is no `Pin`. Fusing a known,
non-recursive callee's frame into its caller's is a later optimization, not another layout. A
panic unwinds across slab frames by a second return path each may-suspend call checks.

The simulation is not a layout: `Io::sim(seed)` picks a seeded scheduler and world at run time,
under any layout.

**The layout is chosen per build, not per signature.** `Cx` and the capabilities are concrete
types; the build configuration says which layout they are, as it says which IO driver a target
gets. Functions are compiled once, and no signature carries a type parameter for it. (The
alternative, `cx: &impl Tasks` with functions that wait monomorphized over the layout type, lets
two layouts live in one binary at the price of a generic on every waiting signature and two
copies of the code. The differential test below builds twice instead.)

### What the compiler checks

- A `!Send` or `#[must_not_suspend]` value live across a call that may wait is an error naming
  the value, the call and the path to the wait, in every layout.
- May-suspend is inferred exactly, from the wait primitive; "takes a `Cx`" is only the usual
  sign, since a value can hold its capability out of sight. The `#[blaze::may_suspend]` mark on
  `dyn` methods and function-pointer fields is also the calling-convention fact `Frames` needs.
- What `Frames` cannot do is an error with the call path, never a quiet switch to a fiber (rule
  `no-fallback`): for one, a foreign (C) frame live across a wait.
- `spawn_concurrent` under `Inline` is an error.
- A task cannot outlive its scope: the lifetime on the child's `cx` enforces it.
- In ordinary crates, where thread-locals still exist, their address is reloaded after any call
  that may wait.

### Proof

Deterministic simulation tests of the scheduler, scopes and cancellation with an oracle and a
planted bug; a differential test that runs the same programs under `Fibers` and `Frames` and
compares outputs and the order of cancellations, drops and panics; and a UI test for each error
above.

## Shared mutation

One naming rule: **the plain name is safe across threads; `Local` marks the cheaper version for
one thread.** Docs and diagnostics say "shared" and "unique" for `&` and `&mut`, which is what
they mean.

| bstd | Rust's std | |
|---|---|---|
| `Shared<T>` / `LocalShared<T>` | `Arc` / `Rc` | both are `Handle`s ([edition](edition.md), change 1) |
| `Lock<T>`, `RwLock<T>` / `LocalLock<T>` | `Mutex`, `RwLock` / `RefCell` | the first waits, the second panics; the same operation |
| `Once<T>`, `Lazy<T>` / `LocalOnce<T>`, `LocalLazy<T>` | `OnceLock`, `LazyLock` / `OnceCell`, `LazyCell` | |
| `Atomic<T>` | `AtomicU32`, `AtomicBool`, ... | one generic type |
| `Cell<T>` | `Cell` | unchanged |
| `RawCell<T>` | `UnsafeCell` | trusted code only |

- **These are the named locks of rule `standard-structures`**, in their one home: a `Lock` is
  made with its name, records who waited behind whom, and its `lock(cx)` is the one wait, so it
  has a deadline and can be cancelled. The removed names are banned by lint.
- **No poisoning.** `lock` returns the guard, not a `Result`. A panic while holding a lock fails
  that task and the lock unlocks.
- **`Freeze` is a public bound**: no shared mutation anywhere inside the type. `&T where T:
  Freeze` is truly immutable, which `pure` and every data-flow analysis want to say. A `static`
  must be `Freeze` (the `global_state` check above); the only way around it is the trusted list,
  not a mark on the item.
- **Token cells.** `TokenCell<T, K>` holds data; a `Token<K>` is the permission to touch every
  cell of its kind. `cell.get(&token)` and `cell.get_mut(&mut token)` are checked by the borrow
  checker, with no run-time flag and no panic. Put the token behind one `Lock` and that lock
  guards a whole graph of cells. It is the idea of the rest of bstd: authority is a value you
  pass. `LocalLock` stays for what a token cannot express.
- **Projection through cells** (`&Cell<Struct>` to `&Cell<Field>`) is a library method built on
  reflection ([comptime](comptime.md)) before any syntax is considered.
- Kept from Rust: a lock owns its data, the `Send`/`Sync` split, and shared mutation as an
  opt-in per type.

## Names are typed, and authority is stored

- **A capability's methods take specific types, never strings**: `dir.open(cx, &Path)`,
  `net.connect(cx, &HostPort)`, `env.get(&VarName)`. A field's type then says which capability it
  is for, and the set of places a name meets authority is closed, so a later analysis can follow
  a value from a field to the capability call it reaches.
- **Store the narrowed capability, not the name.** A struct holds `file: File<ReadOnly>`, not a
  `path` to be combined with an `Fs` later. A name joined to authority far from where it was
  checked is where confused-deputy bugs come from.

## Errors

One error type per area (`FsError`, `NetError`, ...), each naming the operation, the thing (the
path, the host) and the kind. No catch-all `io::Error`, no error without its subject (rule
`errors-name-the-thing`).

## Memory is explicit

There is no global heap in bstd. Every collection is made in an allocator its code was handed
(`Vec::new_in(a)`, `Map::new_in(a)`, `String::new_in(a)`); `Vec::new()` does not exist. This is
Zig's rule, and it buys what the plan needs: every byte belongs to a task or a named cache
(section 6), `noalloc` is visible in a signature, and tests can count and fail allocations.

| allocator | from | lives |
|---|---|---|
| `Frame` | `cx.frame()` | until the task ends; a bump pointer, freed whole |
| `Arena` | `mem.arena("name", limit)` | until dropped; for a result that outlives its task |
| `Cache` | `mem.cache("name", limit)` | until evicted; the one evictor may take entries back |

`Mem` is a capability from `io.mem()`: it hands out named arenas and caches against the process's
one memory budget ([memory](memory.md), [memory-budget](../../../memory-budget.md)). Every arena has
a name and a limit, so "what holds the memory" always has an answer.

**Running out is an outcome, not a crash.** An allocation over its limit or the budget fails the
task with `Stopped::OutOfMemory { arena, asked }` through the same path as a panic (rule
`panics-fail-one-job`); the process and the other tasks go on. Code that wants to handle it itself
uses the `try_` forms (`try_push`, `try_reserve`), which return the error.

Until S4, `alloc`'s global allocator still exists underneath; the `ambient` check treats a use of
it in a capabilities crate as an effect (`global_alloc`), which replaces the `// alloc:` comment.

Strings are UTF-8; a path is bytes on Unix and WTF-8 on Windows behind one `Path`.

## The checks

Two effects join the existing MIR walk (`crates/hir-ty-pure`, with pure, nopanic, noalloc and
nonblocking; [effects](effects.md)):

- **`ambient`**: reaches a system call, or Rust `std`'s file, environment, network, clock, random,
  process or thread API, outside the trusted list.
- **`unsafe`**: reaches an `unsafe` block outside the trusted list.
- **`global_alloc`**: allocates from the global heap instead of a handed allocator.
- **`global_state`**: reads or writes a `static` with interior mutability, a `static mut` or a
  thread-local.

The trusted list is in the workspace manifest (`[blaze.trusted]`, each entry with an owner and a
reason); `bstd` is on it. `blaze explain trusted <binary>` prints what a binary rests on, and
`blaze explain caps <item>` follows a capability or a secret through the call graph. A crate
opts in with `#![blaze::capabilities]`, which makes both effects errors in it and its
dependencies' reached code.

## Moving blaze onto it

Interface first, bottom last, so nothing waits on the runtime and every step lands on its own.

| step | what | done when |
|---|---|---|
| S1 | The crate `crates/blaze-std` (`bstd`): `Cx`, `Io`, every capability above, the allocators, the scopes and deadlines, the errors, over `blaze-core`'s drivers and its simulation | its simulation tests pass; one planted bug per invariant is caught |
| S2 | The `ambient` and `unsafe` effects, `#![blaze::capabilities]`, `blaze explain trusted` | a crate with the attribute that calls `std::fs` is refused, naming the call path |
| S3 | blaze's crates move, leaves first: a sans-IO core takes nothing, a driver takes the piece it needs, `blaze`'s `main` takes `Io`. Each move is a `.clippy.toml` ban and `blaze check --fix` (rule `fix-through-blaze-lints`) | every crate in `crates/` has `#![blaze::capabilities]` |
| S1b | `bstd::{sync, cell}`: the table of "Shared mutation", token cells, `Freeze` as a bound, typed names (`HostPort`, `VarName`) in place of strings, and bans on the removed names | misuse tests: two live `get_mut` on one token do not compile; a static that is not `Freeze` is refused |
| S3b | The `Frames` layout: the calling convention, the slab, unwinding, and the profiler and backtraces walking the frame chain | the differential test against `Fibers` passes |
| S4 | The bottom: `bstd::sys` and `bstd::rt`, `blaze-core`'s drivers move in as `bstd::driver`, and bstd is `#![no_std]` | no blaze crate links Rust's `std`; C1 still passes |
| S5 | The edition: `edition = "blaze-2026.11"` makes `std` name bstd, `fn main(io: Io)` is the entry, and the OS sandbox of a program is derived from what `main` hands out | a program of the edition that never opens the network cannot, checked by the sandbox |
| S6 | C dependencies as wasm, the registry, the migration (`blaze fix --edition`) | each has its own row when S5 is met |

S1 and S2 do not depend on each other. S3 is the long one: about 270 files name `std::fs`, 280
`std::env`, 200 `std::process`, 200 `std::thread`, 160 the clock, and 410 `std::sync`.

Ordinary-edition dependencies stay usable: the walk proves, from source, that what the program
reaches in them has neither effect. Most pure libraries pass unchanged; one that fails is refused
with its path, or put on the trusted list with a reason.

## Decided

- **A new library, not Rust's `std` with parts hidden.** The parts blaze wants gone (ambient
  calls, blocking without deadlines, its locks and maps) are most of it, and the bottom has to be
  the driver either way.
- **`Io` is a concrete value**, as Zig's `std.Io` is passed explicitly, without Zig's vtable in
  every signature and without a generic.
- **No `async`.** One kind of function; fibers first, task frames after.
- **The layout is a build setting**, not a type parameter.
- **Context is a parameter**, as Go's `context` and Zig's allocator are, never a thread-local: a
  thread-local context is ambient authority over time and memory.
- **No global heap.** Out of memory fails a task, never the process.
- **`fn main(cx: &Cx, io: Io)`.**
- **Structured tasks with deadlines**, as Trio's scopes.
- **Standard error is not a capability; standard output is.**
- **No revocation** until something needs it.

## Prior art

cap-std and WASI preopens (directories with no ambient authority), Zig's `std.Io` (IO passed
explicitly, swappable for tests), Trio and Kotlin (structured concurrency, cancel scopes),
TigerBeetle and FoundationDB (one IO interface, deterministic simulation), Austral and Pony
(capabilities in the language), E and Fuchsia (narrowing, handles with rights), Capsicum and
Landlock (the OS side).
