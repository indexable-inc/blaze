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
3. **One of each.** One lock, one map, one channel, one clock, one wait, one error shape: the
   ones blaze's rules already name.

Every blaze crate moves onto it, and later it is the `std` of the blaze edition. Nothing here is
built except the driver layer it starts from (`crates/blaze-core`).

```rust
fn main(io: Io) -> Result<(), Error> {
    let data: Dir<ReadWrite> = io.fs().open_dir("data")?;
    let cfg: Dir<ReadOnly> = data.read_only();               // narrow
    let key = StripeKey::from_env(&io.env().only(&["STRIPE_KEY"]))?;
    let api = io.net().connect_only("api.stripe.com:443");   // one host
    charge(&key, &api, &cfg)
}

// charge cannot open files outside cfg, write, read other variables or dial other hosts.
fn charge(key: &StripeKey, api: &Net, cfg: &Dir<ReadOnly>) -> Result<(), Error> { .. }
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

## Capabilities

| type | made from | narrowing | gives |
|---|---|---|---|
| `Fs` | `io.fs()` | `open_dir(path)` | directories |
| `Dir<P>` | `Fs`, a `Dir` | `subdir`, `read_only` | files beneath it, by relative path only |
| `File<P>` | `Dir` | `read_only` | positional reads and writes, sync, mapping |
| `Net` | `io.net()` | `connect_only(host)`, `listen_only(addr)` | `Conn`, `Listener`, name lookup |
| `Env` | `io.env()` | `only(names)` | variables; `Args` beside it |
| `Clock` | `io.clock()` | | `Instant`, wall time, `Deadline` |
| `Rng` | `io.rng()` | `fork()` | random bytes; seeded in the simulation |
| `Tasks` | `io.tasks()` | | scopes of named tasks |
| `Process` | `io.process()` | `only(programs)` | child processes |
| `Out` | `io.out()` | | standard output and input |

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

Ambient on purpose: allocation, panics, and the span stream (logs and traces go to the recorder,
not through a parameter). A logger threaded through every function would be worked around with a
global, which is worse than an honest exception.

**Secrets.** `#[derive(bstd::Secret)]` gives a type no `Debug`, `Display` or `Clone`, zeroes it on
drop, and hands its bytes only to functions of its own module.

## Time, waiting and tasks

- **Every wait has a deadline.** A task runs in a scope with a `Deadline` on its `Clock`; every
  blocking call ends in a typed outcome that includes `Stopped::Deadline` and
  `Stopped::Cancelled`. There is no call that can wait for ever (rules `bounded-progress`,
  `deadline-waits`).
- **Concurrency is structured.** `tasks.scope(|s| s.spawn("name", ..))`: a task cannot outlive
  its scope, a scope ends when its tasks have, an error or a deadline cancels the siblings. There
  is no detached thread. Every task has a name, which is its span (rule `every-wait-says-what`).
- **A panic fails its task** with a typed error to the scope (rule `panics-fail-one-job`).
- **A child process is handed capabilities.** `process.spawn(program, dirs, env)` gives the child
  exactly those; the driver's sandbox enforces it. This is the sandbox blaze already builds for
  build scripts, as one API.

## Errors

One error type per area (`FsError`, `NetError`, ...), each naming the operation, the thing (the
path, the host) and the kind. No catch-all `io::Error`, no error without its subject (rule
`errors-name-the-thing`).

## Memory and data

Collections take their allocator (`Vec::new_in(frame)`); bstd gives the allocators by lifetime
(frame, task, image) of [memory](memory.md). A global-heap allocation still needs its `// alloc:`
line. Strings are UTF-8; a path is bytes on Unix and WTF-8 on Windows behind one `Path`.

## The checks

Two effects join the existing MIR walk (`crates/hir-ty-pure`, with pure, nopanic, noalloc and
nonblocking; [effects](effects.md)):

- **`ambient`**: reaches a system call, or Rust `std`'s file, environment, network, clock, random,
  process or thread API, outside the trusted list.
- **`unsafe`**: reaches an `unsafe` block outside the trusted list.

The trusted list is in the workspace manifest (`[blaze.trusted]`, each entry with an owner and a
reason); `bstd` is on it. `blaze explain trusted <binary>` prints what a binary rests on, and
`blaze explain caps <item>` follows a capability or a secret through the call graph. A crate
opts in with `#![blaze::capabilities]`, which makes both effects errors in it and its
dependencies' reached code.

## Moving blaze onto it

Interface first, bottom last, so nothing waits on the runtime and every step lands on its own.

| step | what | done when |
|---|---|---|
| S1 | The crate `crates/blaze-std` (`bstd`): `Io`, every capability above, the scopes and deadlines, the errors, over `blaze-core`'s drivers and its simulation | its simulation tests pass; one planted bug per invariant is caught |
| S2 | The `ambient` and `unsafe` effects, `#![blaze::capabilities]`, `blaze explain trusted` | a crate with the attribute that calls `std::fs` is refused, naming the call path |
| S3 | blaze's crates move, leaves first: a sans-IO core takes nothing, a driver takes the piece it needs, `blaze`'s `main` takes `Io`. Each move is a `.clippy.toml` ban and `blaze check --fix` (rule `fix-through-blaze-lints`) | every crate in `crates/` has `#![blaze::capabilities]` |
| S4 | The bottom: `bstd::sys` and `bstd::rt`, `blaze-core`'s drivers move in as `bstd::driver`, and bstd is `#![no_std]` | no blaze crate links Rust's `std`; C1 still passes |
| S5 | The edition: `edition = "blaze-2026.11"` makes `std` name bstd, `fn main(io: Io)` is the entry, and the OS sandbox of a program is derived from what `main` hands out | a program of the edition that never opens the network cannot, checked by the sandbox |
| S6 | C dependencies as wasm, the registry, the migration (`blaze fix --edition`) | each has its own row when S5 is met |

S1 and S2 do not depend on each other. S3 is the long one: about 270 files name `std::fs`, 280
`std::env`, 200 `std::process`, 200 `std::thread`, 160 the clock, and 410 `std::sync`.

**S3's worklist is a lint.** `blaze::std_use` ([lints.md](../../../lints.md#std_use)) finds
each use of an item of Rust's `std` that bstd replaces, by what the use resolves to, once per
item per function, and names what is written instead from one table of rows (ambient
authority, hidden cost, global-heap allocation, renamed) that is revised as bstd's API
settles. `harness/v2/std_uses.py` runs it over the workspace and writes, per crate, the
findings by kind and item, how many have a fix, and the number of functions whose signature
changes, with the crates in layers: a layer depends on no findings of its own, so it is one
batch, leaves first. S3 is met when that script reports no finding and every crate has the
attribute. Only the renames are mechanical (`blaze check --fix` writes `bstd::sync::Shared`
for `Arc`, and the like); taking a capability is a change of signatures, made by hand a layer
at a time. The lint is `allow` until a crate has `#![blaze::capabilities]`; blaze's own
workspace has it at `warn` from the day `BSTD_IN_TREE` (`crates/blaze-lint-defs/src/ready.rs`)
is turned on, which is when S1 compiles on a stage-1 blaze and S3 starts.

Ordinary-edition dependencies stay usable: the walk proves, from source, that what the program
reaches in them has neither effect. Most pure libraries pass unchanged; one that fails is refused
with its path, or put on the trusted list with a reason.

## Decided

- **A new library, not Rust's `std` with parts hidden.** The parts blaze wants gone (ambient
  calls, blocking without deadlines, its locks and maps) are most of it, and the bottom has to be
  the driver either way.
- **`Io` is a concrete value**, as Zig's `std.Io` is passed explicitly, without Zig's vtable in
  every signature and without a generic.
- **Fibers, not `async`.** One API, no colour.
- **Structured tasks with deadlines**, as Trio's scopes.
- **Standard error is not a capability; standard output is.**
- **No revocation** until something needs it.

## Prior art

cap-std and WASI preopens (directories with no ambient authority), Zig's `std.Io` (IO passed
explicitly, swappable for tests), Trio and Kotlin (structured concurrency, cancel scopes),
TigerBeetle and FoundationDB (one IO interface, deterministic simulation), Austral and Pony
(capabilities in the language), E and Fuchsia (narrowing, handles with rights), Capsicum and
Landlock (the OS side).
