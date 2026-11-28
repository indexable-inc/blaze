---
status: plan
summary: "Proper sans-IO for bstd: logic returns typed requests and is resumed with their answers; explicit state machines with their edges in each state's signature (`type Asks`, `type Next = A | B | Fail<E>`), handles in place of names, capability methods that build a request and never wait, one driver loop, and the order in which bstd and the IO-doing crates move."
date: 2026-11-27
---
# Proper sans-IO: typed requests and explicit state machines

The owner's decisions of 2026-11-27 (decision D234). Nothing here is built on main yet; the
interface crate, the typed requests and the first ports are being written on the branch
`dedup-1127`.

## Why

bstd's IO calls look like `file.read_at(cx, at, len, &mut into)`: the call pushes a
`blaze_core::Effect` and waits for its completion under the task's `Cx`. That is simulable and
bounded by a deadline, but the caller is blocked on IO, so the caller is not a sans-IO core. On
2026-11-27 the tree had 232 public bstd functions of that shape, 6 of bstd's 318 source files
marked `#![blaze::sans_io]`, and direct IO inside logic across `blaze-cli`, `blaze-proc-macro`,
`blaze-fetch`, `blaze-cas`, `blaze-world`, `blaze-remote`, `bgit`, `blaze-launch-rt`, the harness
daemon and `scripts`.

Sans-IO here means: logic never performs or waits for IO. It returns a request value and is
resumed with that request's answer. Only a driver performs requests.

Every tradeoff below is weighed for a model writing and reading the code, not a person: writing
text is free; what costs is state the code does not show, a mistake no type catches, and text a
later session must reload. So the explicit form is preferred even at several times the length.

## 1. The interface (`crates/blaze-machine`, `#![no_std]`, no allocation)

```rust
/// Something a machine may ask for. The answer's type is fixed by the request.
pub trait Request { type Answer<'s>; }

/// One state of an explicit machine. Its signature is its node in the graph.
pub trait State: Sized {
    type Asks: Request;   // or a union of requests: `Recv | Timer`
    type Next;            // every state it may go to: `Sending | ReadingHead | Fail<FetchError>`
    fn asks(&self) -> Self::Asks;
    #[blaze::consumes]
    fn resume(self, answer: <Self::Asks as Request>::Answer<'_>) -> Self::Next;
}

pub struct Done<T>(pub T);   // terminal members of a `Next`
pub struct Fail<E>(pub E);
```

No new syntax and no macro: a state is a type with an ordinary impl. Each of these is a
compile-time check:

| Property | How it is held |
|---|---|
| A state goes only where its `Next` says | the return type |
| A state reaches every member of its `Next` | a declared target no path produces is an error at the impl |
| A state is resumed only with its request's answer | `Request::Answer`; no "out of step" arm can be written |
| A transition cannot be forgotten | `resume` consumes the state (`#[blaze::consumes]`, the linearity check) |
| A request is performed or cancelled, never dropped | requests are `#[blaze::linear]` |
| Data that exists in one state is not visible in another | it is a field of that state's type |

The whole graph is recovered from the start state by compile-time reflection over `Next`: the
machine's enum, its size, a state nothing reaches, a state with no path to an end, and a
transition table for tools. Cancellation and the deadline are answers every state can receive,
with a default (`Fail(Stopped)`) that a state may override.

Until anonymous unions carry this on the front end, a state's `Next` is a hand-written enum named
`<State>Next`, one variant per target; the trait accepts both.

### What a machine looks like

```rust
struct Sending { conn: ConnId, sent: usize }

impl State for Sending {
    type Asks = Send;
    type Next = Sending | ReadingHead | Fail<FetchError>;

    fn asks(&self) -> Send { self.conn.send(&self.request[self.sent..]) }

    #[blaze::consumes]
    fn resume(self, answer: Result<usize, NetError>) -> Self::Next {
        match answer {
            Ok(n) if self.sent + n < self.request.len() => Sending { sent: self.sent + n, ..self },
            Ok(_)  => ReadingHead { conn: self.conn, head: FrameVec::new() },
            Err(e) => Fail(FetchError::Send(e)),
        }
    }
}
```

`match` is kept for decisions inside a state on data. It is not the dispatcher between states.

### The compiled form

A direct-style function may also be compiled into a machine: it suspends at each IO call and
carries a `suspend` effect in its type; a function without that effect provably yields no IO.
One rule keeps the result plain movable data with no `Pin`: **a compiled coroutine may not hold
a borrow of its own local across a suspension point.** What must survive a suspension is owned,
or lives in the task's arena. It is for short linear sequences; anything with branching, retries,
timeouts or more than one kind of input is an explicit machine. Specified here, built after the
explicit form.

Whether a paused machine may resume on another thread is a property of its type: only if what
it holds across the suspension is `Send`. Reading a thread-local or holding a lock across a
suspension is refused. Open: a task is its frame plus its paused state, which points into that
frame; the pair moved together is safe though neither half is. How the pair is typed for a
pooled driver is to be designed and proven by an interleaving test.

## 2. Handles (`blaze-core`, module `handle`)

An open resource is named by a handle, not by a path string in each request and not by an OS
descriptor: `Handle<K> { slot: u32, generation: u32 }`, 8 bytes, with `FileId`, `DirId`,
`ConnId`, `ListenerId`, `SocketId`, `ChildId`, `WatchId`, `MapId`, `LockId`. A slot's generation
rises on reuse, so a stale handle is refused by name. Slots are issued in the order opens
complete, which is deterministic in the simulation. A handle has no key encoding and is never
written to a record. User code never sees one: it is a private field of a capability
(`File<ReadOnly>`, `Dir`, `Conn`, `Child`), which is linear where it must be closed.

### Closing: there is no `Drop` that does IO

Dropping a value cannot perform IO in this world, because nothing but a driver performs IO. So
closing is a request like any other, and the capability that holds a handle is linear:

```rust
file.close() -> Close        // consumes the `File`; Answer = Result<(), FsError>
```

- A state that holds a `File` must end by going to a state that asks `Close`, or hand the file
  on in its `Next`. Letting it fall out of scope is a compile error, the same rule as an
  `UnmanagedVec` that must be given its allocator to free (`drop-with.md`).
- The error of a close is an answer the machine sees. `Drop` could only have ignored it.
- Cancellation is the one case logic cannot handle itself: a cancelled or failed machine is
  discarded with its handles still open. The driver's table records which task owns each handle
  and closes them when the task ends. That is the backstop, not the path: a handle the driver
  had to reclaim from a task that ended normally is counted and reported as a leak.
- Buffers lent to a pending request (`ReadInto`) return in its answer; on cancellation the
  driver returns them when the cancelled operation completes, never before.

## 3. Requests (`blaze-core`, module `request`)

One type per thing a driver can do, replacing the single `Effect` enum (63 variants) and `Done`
enum at the surface. Each is linear, has private fields, is made only by a method on the
capability that authorises it, and has a typed answer and a typed error.

```rust
#[blaze::linear]
pub struct Read { file: FileId, at: u64, len: u32 }
impl Request for Read { type Answer<'s> = Result<&'s [u8], FsError>; }   // lent for one step

#[blaze::linear]
pub struct ReadInto<'a> { file: FileId, at: u64, into: &'a mut [u8] }
impl<'a> Request for ReadInto<'a> { type Answer<'s> = Result<Filled<'a>, FsError>; }
```

- An answer lends the driver's bytes for one step, or the request carries the caller's arena
  buffer and the answer returns it filled. A driver never allocates on the global heap for a
  caller (today `Done::Read(Vec<u8>)` does).
- Every size is stated (`len: u32`, `most: u64`).
- "Absent" is never read as "empty": `NotFound` is an error the caller matches (today
  `Effect::ReadAll` reads a missing file as empty).

Groups: `file` (by handle: `Open`, `Close`, `Read`, `ReadInto`, `ReadMany`, `WriteAt`,
`WriteFrom`, `Flush`, `Truncate`, `Lock`, `Unlock`, `Map`, `MapWrite`), `path` (one-shot by `Dir`
and relative path: `ReadAll`, `Write`, `Append`, `Rename`, `Remove`, `MakeDir`, `List`, `Stat`,
`Link`, `CopyData`, `Mode`, `Times`, `SyncDir`, and the rest of today's by-name effects),
`watch`, `net`, `proc`, `stdio`, `time`, `random`, `trust`, `host`. Each request converts to the
existing `Effect` and builds its answer from `Done`, so the native driver, the simulation and
every existing effect test keep working underneath.

## 4. Capabilities and drivers (bstd)

A capability method builds a request; it does not wait and takes no `cx`:
`file.read_at(at, len) -> Read`. Outside a machine, the one way to perform a request is
`cx.run(request)`, a driver-side call that waits under the task's deadline; the lint refuses it
in any crate marked `#![blaze::sans_io]`. bstd's call-and-wait methods are replaced by these two
halves in one batch by a rewriter, not kept beside them.

`drive(io, cx, machine)` is the one loop (ask, perform, answer, repeat) for the native driver and
the simulation. `Script` drives a machine in a test from a list of expected requests and the
answers to give; a mismatch is a typed failure naming the step, and no simulator is needed.

## 5. Order

| Step | What | Effort |
|---|---|---|
| 1 | `blaze-machine`: the traits, `Script`, two real machines ported (`blaze-h1`, `blaze-fetch`) | `[███░░░░░░░]` |
| 2 | Handles and typed requests for every effect, with the caller's buffer | `[████░░░░░░]` |
| 3 | Capability methods build requests; `cx.run`; `drive`; the rewriter over bstd's 232 calls and their callers | `[█████░░░░░]` |
| 4 | bstd's multi-step operations as machines: directory walk, tree copy, spawn-and-collect, HTTP exchange, git fetch, the start-up spec read | `[████████░░]` |
| 5 | The crates that do IO: logic into machines, one thin driver each, ported once onto the arena collections of `drop-with.md` in the same pass | `[██████████]` |
| 6 | The lint becomes an error: no waiting call in `#![blaze::sans_io]` code | `[██░░░░░░░░]` |
| 7 | The compiled form: the `suspend` effect, coroutine lowering, the no-local-borrow check | `[████████░░]` |

Steps 1 to 3 are one wave. STEP 5 IS A MULTI-DAY TASK: `bgit` alone has 207 direct IO sites.

## 6. How it is proven

- The interface: a simulation that drives each ported machine with an oracle and a planted bug
  (a machine that mishandles a short read is caught by its script).
- Requests: each one's round trip through `Effect` and `Done` under the existing simulation, so
  byte-for-byte the driver sees what it saw before.
- Handles: a stale handle after close is refused; ids replay identically from one seed.
- The rewriter: bstd's own tests and the hello-world smoke on the exact tree.
- Each ported crate: its tests driven by `Script` with no simulated world, plus one run under the
  simulation with failures injected.

## 7. The losing alternative

`async`/`await` on a blaze executor: familiar, and the compiler already makes the state machines.
Rejected because `Future` and `Waker` bring an ambient reactor back, self-referential futures
need `Pin`, and a future's states and edges are invisible in its signature, which is the
opposite of what a model reading the code needs.
