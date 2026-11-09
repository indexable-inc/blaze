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

## What is in it

bstd is batteries included, and most of the batteries exist. It is a **facade**: the pieces stay
separate crates (they build in parallel, and a build script can take one alone), bstd re-exports
them under one tree of names, and code outside bstd imports `bstd::..` only (a lint bans the
piece crates' own names, so there is one name per thing).

Something belongs in bstd when most programs need it, or a rule names it as the one structure for
a need, and it can obey bstd's rules (no ambient authority, explicit memory, no global state).
What is about building Rust (the cargo graph, the store, codegen) stays out.

| module | what | from |
|---|---|---|
| `mem` | allocators by lifetime: frames, arenas, pools, caches | `blaze-mem` |
| `bytes`, `text` | held bytes and UTF-8 text, lent and never copied | `blaze-bytes` |
| `map`, `set`, `hash` | the one hash map and set (fixed seed), the identity hasher | `blaze-hash` |
| `id` | content ids (BLAKE3), dense `u32` indices, and the one interner | `blaze-ids`, `intern`, `blaze-hashcons` merged |
| `sync`, `cell` | named locks, `Shared`, `Atomic`, token cells, the bounded spin, reclamation at task ends | `blaze_core::lock`, `blaze-spin`, `blaze-reclaim` |
| `chan` | the one queue and the one channel | the crossbeam types rule `standard-structures` names |
| `sort`, `phf` | the stable radix sort; perfect hashes of fixed name sets | `blaze-radix`, `blaze-phf` |
| `digest` | BLAKE3 and SHA-256 | `blaze-ids`' `b3`, `blaze-sha256` |
| `rng` | the seeded generator behind the `Rng` capability | `blaze-rng` |
| `path`, `time` | relative paths; durations and instants | `paths` (replaced), new |
| `data` | serialization by compile-time reflection (below) | new, replacing `blaze-serde` in blaze's crates |
| `record` | zero-copy records: bytes in memory are the bytes on disk | `blaze-format` |
| `report` | the `Report`: what a program says to a person or an agent, as data | `blaze-report` |
| `quic` | QUIC as a sans-IO core: datagrams and time in, datagrams and stream events out; the driver is a `Net` datagram socket and the `Clock` | `blaze-http`'s QUIC core ([http3-sans-io](http3-sans-io.md)) |
| `http` | sans-IO HTTP/1.1 over a `Conn`, and HTTP/3 and WebTransport over `quic` | `blaze-http` |
| `trace` | spans and events through `Cx` | `blaze-trace` (no-tracing) |
| `gpu` | the device behind the `Gpu` capability: buffers, launches, render targets | new, with [gpu](../../../future/gpu/plan.md) |
| `video`, `image` | codecs as sans-IO cores; image decode and encode as functions | `harness-video`'s encoder, moved; `png` |
| `test` | the simulation, property tests, the model checker for lock-free code, allocation counting | `blaze_core::sim`, hegel, `blaze-check`, `blaze-alloc-count` |
| `io`, `fs`, `net`, ... | the capabilities of the sections below | new, on `blaze-core` |

`stdx` (leftover helpers) is dissolved into the modules above or deleted. Each merge follows rule
`one-concept-one-home`: one type per concept, and the removed name banned.

### `bstd::data`: serialization by reflection

`Encode` and `Decode` are implemented for a type from its fields ([comptime](comptime.md)), in
ordinary typed code: no derive macro, no generated tokens, errors on real lines, and nothing to
compile to wasm and sandbox. A type opts in by naming a policy for its fields, not by deriving.
The formats are bstd's: deterministic CBOR (the agent API's), JSON, and `record` for zero-copy.
Decoding takes an allocator, as everything does, and a size limit, since input is untrusted.
A union decodes by a tag naming its member. A type that holds a capability or a `Secret` has no
`Decode`, and saying so is a compile error naming the field: bytes can describe what is wanted
(a `Path`, a `VarName`), and only code holding the authority can turn that description into a
capability by narrowing its own.
The same field walk gives tracing its encoders and `Secret` its redaction. `blaze-serde` remains
for crates from crates.io that speak serde; blaze's own crates move to `data` once reflection
lands.

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

## The boundary: ops out, events in

Under every capability is one boundary between a program and whatever hosts it. The program
writes **ops** (open, read, send, connect, set a timer); the host answers with **events**
(opened, data, sent, accepted, a datagram, the timer fired). Both are fixed-layout records in two
rings, exchanged in batches, each op carrying a token its event returns. Nothing else crosses.

The io_uring driver, the kqueue and IOCP drivers, the simulation and a wasm host are all hosts of
this one boundary, which is why a program cannot tell them apart. It is `blaze-core`'s
`Effect`/`Completion` made flat, and the shape wasm-host's guest ABI already proved at speed
(`ix-abi`, `ix-worker`). Taken from there:

- **Records are plain words** with one layout on every target, so a ring can be shared memory
  between a guest and its host, and a batch costs one crossing however many ops it holds.
- **Ids are dense with a generation**: a stream id indexes a slab directly, no hash map, and a
  stale id is caught by its generation.
- **Events lend their bytes.** Data arrives as a view into a host buffer, valid for the event;
  `retain` turns it into an owned buffer from the pool, and dropping that queues its release.
  An op's body is static, owned or pool memory. Receives are zero-copy end to end.
- **The host terminates protocols it is better placed to** (TLS, HTTP/1.1, 2 and 3, WebSocket
  and WebTransport framing in a wasm host) and hands over protocol-neutral events: a request is a
  request whichever version carried it.

### Three kinds of code on it

| kind | takes | is |
|---|---|---|
| a **core** | nothing: no `Cx`, no capability | a state machine. Inputs (bytes, events, the time) go in, outputs (bytes, ops, the next deadline) come out. `quic`, `http`, TLS, a journal, the build graph. Testable with no host at all |
| a **driver** | a `Cx` and the capabilities it needs | a thin loop: feed the core what the capability delivers, do what the core asks. A few dozen lines per core per capability |
| **straight-line code** | a `Cx` and capabilities | ordinary functions that wait. The runtime turns each wait into an op and a park, and its event into a wake-up |

Straight-line code and a hand-written machine are the same thing at the boundary. Under the
`Frames` layout the compiler makes the state machine from the straight-line function, so "write a
function, get a machine" is the default, and a machine is written by hand only for a core: logic
worth testing with no host, or reused under several drivers.

A program can also be a machine at the top: `impl Worker { fn event(&mut self, cx, ev) }`, called
once per batch with no runtime underneath. That is the right shape for a wasm guest and for the
hottest paths.

### When a capability is missing

A host need not offer everything. A wasm worker has no sockets; a sandboxed child was handed two
directories; a test gives a fake.

- **What a host offers is known at start**, from a table it hands over before `main` (the
  manifest). Nothing is discovered by an op failing halfway through.
- **`main` says what it needs, and that is the program's manifest.** A required capability the
  host does not offer stops the program before `main` runs, naming it. An optional one is an
  `Option` in `main`'s hands: `io.net()` returns `Option<Net>`.
- **Code below `main` never asks whether a capability exists.** It takes `&Http` or it does not;
  absence is settled once, at the top, where the program chooses a provider:

  ```rust
  let http = match (io.http(), io.net()) {
      (Some(host), _) => host,                              // the host makes requests for us
      (None, Some(net)) => Http::over(&net, &clock, &roots), // we do, over its sockets
      (None, None) => return Err(Missing("http")),
  };
  ```

  `Http::best(&io)` is that match, for the common case.

- **Both arms give the same type.** `Http` is one struct whose provider is a field, so the match
  type-checks, and code below it is compiled once.
- **What the target can never offer is settled at compile time.** A target's possible
  capabilities are a constant (`bstd::host::OFFERS`). A provider the target cannot have is not a
  variant of the field at all, and `io.net()` on a target with no sockets is a constant `None`,
  so the match above folds to one arm and the call to one direct call: no dispatch is paid for a
  choice the build already made. A capability `main` requires that the target cannot offer is a
  compile error, not a failure at start.
- **What is left for run time** is only what truly varies there: a parent that granted a child
  less than its target allows, and a handler passed in a test.

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
| `Net` | `io.net()` | `connect_only(host)`, `listen_only(addr)` | `Conn`, `Listener`, `Datagrams` (UDP), name lookup |
| `Env` | `io.env()` | `only(names)` | environment variables |
| `Args` | `io.args()` | | the command line |
| `Host` | `io.host()` | | the process id, core count, host name, signals, and exiting with a status |
| `Clock` | `io.clock()` | | `Instant`, wall time, `Deadline` |
| `Rng` | `io.rng()` | `fork()` | random bytes; seeded in the simulation |
| `Sandboxes` | `io.sandboxes()` | a smaller budget | `OsSandbox`, `Vm`, `WasmSandbox`, and the `Runner` each makes (below) |
| `Out` | `io.out()` | | standard input, output and error |
| `Mem` | `io.mem()` | | named arenas and caches against the memory budget |
| `Gpu` | `io.gpu()` | `only(kernels)`, a memory budget | a device: buffers, kernel launches, offscreen render targets (below) |

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

### Capabilities are layered, and each has providers

A capability names what a program needs, not how the host supplies it. `Http` is one: "may make
requests to these origins". It has more than one provider:

| provider | how it answers a request |
|---|---|
| `Http::over(&net, &clock, &roots)` | bstd's sans-IO HTTP core over connections from that `Net`; never reaches further than the `Net` does |
| `io.http()` | the host does it: `fetch` in a wasm worker, where there are no sockets at all |
| `Http::handler(f)` | a function you pass: a fake for a test, a cache, a proxy |

All three are the same type, `Http`, so `fn sync(cx: &Cx, http: &Http)` runs natively, in a
worker and in a test unchanged. The provider is a field, chosen where the capability is made; a
request is already far more work than one indirect call. `http.only(origin)` narrows whichever
it is.

Three rules come out of this:

- **Ask for the highest capability that does the job.** A function that takes `&Http` runs
  anywhere a request can be made; one that takes `&Net` runs only where there are sockets. The
  same goes for `Streams` (WebTransport: `quic` natively, the browser's in a worker) over
  `Datagrams`.
- **`Io` has the shape of its host**, known at start (above): a native process offers `net`,
  `fs` and `process`; a wasm worker offers `http` and `streams` and no `net`.
- **A lower capability can always make the higher one**, never the reverse: `Net` gives `Http`,
  `Datagrams` gives `Streams`, a `Dir` gives a key-value store. That is narrowing: the higher one
  can do strictly less.

The protocol logic is the sans-IO core in every case; a provider is only its driver.

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
- **Finishing is explicit.** What must be closed, flushed or joined is a linear value with a
  call that takes `cx` ("Values that must be finished"); `Drop` waits only on the panic path.
- **A panic fails its task** with a typed error to its scope (rule `panics-fail-one-job`).
- **A program runs only in a sandbox** (below). There is no bare `spawn`.
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

## Values that must be finished

bstd is written with linear types ([linear](linear.md)): a `#[blaze::linear]` value cannot be
dropped or forgotten, only handed to a function that takes it by value. Rust's std cannot use
them (its generics would have to change), and much of its shape is a workaround for that. bstd
does not need the workarounds.

- **A scope is a value, not a closure.** `let s = cx.scope(); s.spawn(..); s.join(cx)?;`. Rust
  needs the closure form because a guard can be leaked, and a leaked guard would let a task
  outlive what it borrowed. A linear `Scope` cannot be leaked, so the code is straight-line and
  an early return has to go through `join`. The same holds for a GPU launch scope. The closure
  forms remain as conveniences.
- **An operation in flight borrows its buffer.** With completion-based IO the kernel writes into
  a buffer after the call returns, so today's libraries make every read take an owned buffer,
  since a forgotten future would free memory the kernel still writes. A linear
  `Pending<'buf>` cannot be forgotten: it must be `wait`ed or `cancel`led (which also waits), so
  `file.read_into(cx, &mut buf)` on a borrowed buffer is sound and zero-copy.
- **Closing is a call that can fail and can wait**: `file.close(cx)?`, `conn.finish(cx)?`,
  `vm.stop(cx)?`. A handle with unsynced writes, a connection with unsent data, a sandbox and a
  transaction are linear, so their last error cannot be lost in a destructor and nothing has to
  wait inside `Drop`.
- **A task's result must be taken**: `Task<T>` is linear and `join` returns its outcome, so a
  failure cannot be ignored by dropping the handle.
- **On a panic** the value's abandonment path runs instead (roll back, cancel and wait, close
  without syncing), as [linear](linear.md) says; that is the one place a destructor of these types
  runs.
- **bstd's containers say whether they admit linear elements.** `Vec`, `Option`, `Result` and
  the maps do (and are then linear themselves); `Shared` and `LocalShared` do not, since a cycle
  would leak one; there is no `forget`.

## Sandboxes: the one way to run a program

There is no "start a process" call. A program runs in a sandbox, and a sandbox is a value with a
definition, so what a command could see and touch is written down before it runs.

The three ways to isolate take different things and can do different things, so they are three
types, each with exactly the spec that is valid for it and the operations it really has:

| type | made with | the program is | besides running, it can |
|---|---|---|---|
| `OsSandbox` | `Dir`s at chosen paths, an `Env`, a net policy, limits | a host process behind the OS's sandbox (Landlock, seccomp and namespaces; a sandbox profile on macOS; AppContainer on Windows) | signal a process, see its resource use |
| `Vm` | an image or a view of the file tree, cores and memory from the host's menu, a lifetime, a net policy | a guest in a micro VM | stop, resume, snapshot, fork |
| `WasmSandbox` | a module, `Dir`s as preopens, an `Env`, fuel and memory limits | a WASI module in the runtime | call an export, meter fuel |

```rust
let sb: OsSandbox = io.sandboxes().open(cx, &OsSpec {
    dirs: &[(path!("src"), &src.read_only()), (path!("out"), &out)],
    env: &env.only(&[var!("CC")]),
    net: NetPolicy::None,
    limits: Limits { cores: 4, memory: 2 << 30, wall: Duration::from_secs(60) },
})?;
compile(cx, &sb.runner())?;

// Runs anywhere a command can be run; cannot tell which kind it was given.
fn compile(cx: &Cx, run: &Runner) -> Result<(), Error> {
    run.run(cx, &Command::new(prog!("cc")).args(&["-c", "main.c"]))?; ..
}
```

- **One `open`; the spec's type picks the sandbox's type.** Each spec names its sandbox
  (`trait Spec { type Sandbox; }`), so `open(cx, &VmSpec { .. })` returns a `Vm` and
  `open(cx, &OsSpec { .. })` an `OsSandbox`. A spec for one kind cannot hold a field of another,
  and an operation a kind lacks does not exist on its type.
- **`Runner` is what they share**: run a command, get its status and its output streams. It is
  the higher capability each of the three can make (as `Net` makes `Http`), one concrete type
  with the provider inside. Code that only runs commands takes `&Runner`; code that snapshots
  takes `&Vm`. Nothing asks a sandbox at run time what kind it is.
- **A spec holds only capabilities and limits**, and never more than its maker held. The file
  system is the `Dir`s handed in; the environment is an `Env`; the network is nothing, a narrowed
  `Net`, or a proxy the parent runs for host-level rules.
- **A host that cannot build the isolation refuses**, naming what it could not enforce. Which of
  the three a host offers is known at start, like any capability.
- **A sandbox is reused.** Making one is the expensive part; a run is cheap, so a build makes one
  per kind of work and runs many commands in it. It ends with its scope, like a task.
- **Every run is recorded.** The sandbox traces what the program read and wrote, so a run is a
  function of its command, its spec and the content it read. That is a cache key: the same
  command over the same inputs is a store hit ([caching](caching.md)), which is how blaze already
  treats build scripts. A `Vm` over a view of the Merkle tree ([vfs](vfs.md)) gives its output as
  the difference between two roots.
- **A bstd program inside one** gets an `Io` built from the spec: its `main` sees those
  capabilities and no others. Any other program sees an ordinary system that happens to contain
  only what was granted.

This is one design for what blaze has three of today (the build-script sandbox, the proc-macro
host, `ix` VMs for lanes), and the shape of wasm-host's sandbox ops (a spec, a handle, `exec`,
events) with capabilities as the spec.

## Gpu, video and images

**`Gpu` is a capability.** A program can use a graphics device only if it was handed one, like a
socket. It is the `Device` of the GPU plan ([gpu](../../../future/gpu/plan.md)) moved under `Io`:

- `io.gpu()` is `None` on a host with no device; `Gpu::cpu()` is always available and runs the
  same kernels as host code, which is what tests and the simulation use.
- **Running Rust on it is the GPU plan's job, not bstd's**: a `#[gpu::kernel]` function in the
  same crate is compiled by blaze for Metal, Vulkan, CUDA and the CPU, and
  `gpu.scope(cx, |s| s.launch(&kernel, grid, args))` is type-checked against its signature. bstd
  supplies what that plan called the runtime crate: the device, `Buffer<T>` made in an allocator
  the caller hands over and counted against the memory budget, the launch that goes through the
  one wait (so it has a deadline, and host buffers stay borrowed until the device finishes), and
  offscreen render targets.
- Narrowing: `gpu.only(&[kernels])`, and a smaller memory budget.
- The providers are Metal, Vulkan, CUDA and the CPU, one `Gpu` type, chosen as any provider is.

**Getting data to the device, and unified memory.** A kernel's arguments are `Buffer<T>`s, never
plain slices, so a copy is never hidden. What a buffer costs depends on the machine, and the same
code is right on both kinds:

| | unified memory (Apple silicon, integrated GPUs) | separate memory (a discrete card) |
|---|---|---|
| `Vec::new_in(gpu.alloc())`, `gpu.buffer(n)` | page-aligned memory both processors address | pinned host memory, with device memory behind it |
| the host writes it (`buf.host_mut()`) | writes the bytes the device will read | writes host memory and marks the range dirty |
| `launch` | no copy | uploads the dirty ranges first |
| the host reads a result (`buf.host(cx)`) | ready at once, no copy | a wait: downloads what the device changed |
| a file read into it | disk to device-visible pages, no copy | then an upload |

- **Allocate where the data will be used.** Because every collection takes its allocator, data
  built in `gpu.alloc()` is a device buffer from its first byte. That is the whole optimization
  on unified memory: nothing is ever copied in either direction.
- **The borrow checker carries the rule that makes it safe on both.** A launch borrows its inputs
  shared and its outputs uniquely until the device finishes (the scope), so the host cannot read
  a buffer the device is writing. On unified memory that prevents a race; on separate memory it
  is the same rule, and it is why the dirty ranges are always known.
- **Reading back is a wait** (`host(cx)`), so it has a deadline everywhere and costs nothing where
  nothing needs copying.
- **`Buffer<T, DeviceOnly>` has no host view at all**, for intermediates between kernels. On a
  discrete card it never crosses the bus; on unified memory the driver may keep it private.
- **Which kind a device is**: a constant where the target decides it (Metal on Apple silicon, so
  the dirty tracking compiles out), otherwise `gpu.memory()` at run time for a program that wants
  to pick a strategy, such as keeping data on the device across many launches.
- `gpu.upload(cx, &slice)` is the explicit copy from ordinary memory, for data that already
  exists somewhere else.

**A codec is a sans-IO core.** `bstd::video::Encoder` takes frames and returns packets;
`Decoder` the reverse; a `Muxer` takes packets and returns the bytes of a container. None of
them takes a capability or reads a clock: frame times are data. Where the work is done is a
provider:

| provider | made from | |
|---|---|---|
| the platform's hardware encoder (VideoToolbox; VA-API and NVENC later) | a `Gpu` | fast, HDR; what the agent app's video tool uses today |
| a software encoder | nothing | anywhere, including the simulation |

Writing a file is the caller's: packets go to a `File` it holds. So rendering a video is
`Gpu` renders a frame into a buffer, `Encoder` turns frames into packets, `Muxer` into bytes,
`File` takes the bytes, and each step can be tested without the others.

**Images are functions.** `bstd::image::{decode, encode}` take bytes and an allocator and return
pixels, or the reverse, with a size limit on decode since input is untrusted. PNG first; no
capability, no IO.

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

## Tracing

The recorder is the one [no-tracing](no-tracing.md) specifies (`blaze-trace`: per-thread rings,
fixed-layout rows, fields as words, no formatting on the hot path) feeding the profile history
([profiling](../../../profiling.md), [ix-prof-format](../../../ix-prof-format.md)). bstd does not
add a second one. It changes where the span lives, what is recorded without being asked, and how
a site is described. Where this section and no-tracing differ, this section is the newer.

- **The span is in `Cx`.** `let cx = cx.span(FETCH, &url);` returns a child `Cx`; the span ends
  when it drops; everything called with it is inside. A spawned task's `cx` comes from its scope,
  so the trace tree is the task tree and nothing needs attaching by hand. This replaces the
  thread-local enter stack: a task can change threads at any wait, and nothing here is global.
  The ring is the current worker's, reached through `Cx`.
- **The runtime records by itself**, with no instrumentation in user code:
  - per task, time split three ways: running, parked (on what, from the one wait), and ready but
    not running (queue delay);
  - wake edges, which task woke which. A lock is built on the wait and records its holder, so a
    slow wait reads "lock L, held by task N in span S";
  - the task's logical stack for the sampler: the slab's frame chain under `Frames`, the fiber's
    stack under `Fibers`.
- **A site is described at compile time.** blaze is the compiler and the linker, so each span and
  event site gets its id and a static schema (name, field names and types, file and line) in a
  table beside the binary. Nothing about a site is formatted, hashed or registered at run time.
  (no-tracing rejected link-time ids because a distributed slice was the only way; it is not.)
- **An event is fixed bytes**: site id, timestamp, task id, span id, raw field bytes, copied into
  the worker's ring. No formatting, allocation, lock, system call or write to another thread's
  memory. A drainer moves full chunks out; decoding happens offline against the schema table,
  straight into the history's typed columns.
- **Field encoders come from reflection** ([comptime](comptime.md)), not a macro and not `Debug`.
  A field whose type has no encoder is an error at the site, naming the type; there is no
  format-as-text path.
- **Secrets cannot be recorded.** A `Secret` field of a struct encodes as nothing; recording one
  directly is a compile error naming the field.
- **Time comes from the injected clock**: the CPU's counter read directly in production, virtual
  time under `Io::sim(seed)`. A seeded run gives a byte-identical trace, so a trace can be a
  test's oracle.
- **The tracer never blocks a worker.** A full ring drops and counts, per worker, and the count
  is recorded.
- **Keep or drop at the end.** A task's detailed events are buffered and kept only when the task
  was slow or failed; its cheap summary is always kept.
- **A disabled site**: patched out by the live-patch engine so it costs nothing, or one branch on
  a per-site flag. Chosen by measurement, and the number is written here.

Checked: the emit path is `nopanic`, `noalloc` and `nonblocking`, enforced, so tracing stays on
in release. Proved: a bound for the emit path in [bounds.md](../bounds.md) (a counter read and a
few stores into a hot buffer) with a budget on ns and bytes per event and zero allocations; a
simulation test of ring and drainer whose oracle is "every event is delivered once, in order per
worker, or counted as dropped", with a planted bug; and an encode-decode round trip.

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
| S1c | Tracing on bstd: the span in `Cx` and the scheduler's own events first; then the compile-time site table and the ring; then reflection encoders and redaction; disabled-site patching and keep-or-drop last | the emit budget holds; the ring's simulation test and the round trip pass |
| S1d | The facade: bstd re-exports the crates of "What is in it", the interner and helper crates are merged, and the piece crates' names are banned outside bstd | `blaze check` with the bans on; no blaze crate outside bstd names a piece crate |
| S1e | `bstd::image` (PNG), `bstd::video` (the encoder core, the muxer, the VideoToolbox provider moved out of `harness-video`, which then uses bstd's), and `Gpu` as a capability with the CPU provider and offscreen targets | a round trip of an image; a clip encoded from fixed frames is byte-identical twice; `harness-video` builds on it |
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
TigerBeetle and FoundationDB (one IO interface, deterministic simulation), quinn-proto and wasm-host's guest ABI (protocols and programs as explicit state machines over ops and events), Austral and Pony
(capabilities in the language), E and Fuchsia (narrowing, handles with rights), Capsicum and
Landlock (the OS side).
