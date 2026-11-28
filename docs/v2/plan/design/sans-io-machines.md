---
status: plan
summary: "Unified PL end state for sans-IO and effects in blaze: one-shot lexical algebraic effects and linear typestate at the surface, compiling to one zero-alloc, zero-Pin sans-IO Machine ABI (`Waiting` + `Ask<R, W>`, `Race`, `Batch`, `Call`, `TaskFrame`, `Handler`/`Script`), generational branded handles (`#[blaze::handle]` vs `#[blaze::capability]`), owned zero-copy DMA buffers, and staged waves across compiler, blaze-machine, blaze-core, and bstd."
date: 2026-11-27
---
# One-shot lexical algebraic effects and the sans-IO machine ABI

The owner's decisions of 2026-11-27 (decision D234, refined to the long-term PL end state).

## Why

bstd's IO calls look like `file.read_at(cx, at, len, &mut into)`: the call pushes a
`blaze_core::Effect` and waits for its completion under the task's `Cx`. That is simulable and
bounded by a deadline, but the caller is blocked on IO, so the caller is not a sans-IO core. On
2026-11-27 the tree had 232 public bstd functions of that shape, 6 of bstd's 318 source files
marked `#![blaze::sans_io]`, and direct IO inside logic across `blaze-cli`, `blaze-proc-macro`,
`blaze-fetch`, `blaze-cas`, `blaze-world`, `blaze-remote`, `bgit`, `blaze-launch-rt`, the harness
daemon and `scripts`.

From a PL perspective, **manual sans-IO is not the best surface programming model—it is the
compilation target (the runtime ABI) of one-shot lexical algebraic effects**:

- **Pure transforms & codecs** (`blaze-h1`, `blaze-vt`, `blaze-cbor`, `rustls`, compiler passes)
  have an empty effect row: plain functions and byte-in/byte-out cores that never name `Request`
  or `Handle`.
- **Protocols & durability cores** (`blaze-cas` journals, retry/backoff, watchdogs, schedulers,
  transport lifecycles) use **explicit state-graph machines** (`Waiting` + `Ask<R, W>` + typestate
  handles), where every state and legal transition is visible in the types for models and Lean
  refinement proofs.
- **Workflows & orchestration** (`bgit`'s 207 IO sites, `blaze-cli`, `dir walk`, `tree copy`,
  `blaze-fetch`, `scripts`) use **direct-style one-shot lexical algebraic effects** with explicit
  suspension markers at every yield site, effect rows reusing `A | B` unions (`EffectSet::SUSPEND`),
  and the no-local-borrow rule—compiled by `blaze-lir/coroutine.rs` into the **exact same
  zero-`Pin` sans-IO Machine ABI**.

Every tradeoff below is weighed for a model writing and reading the code, hardware-floor execution
(`io_uring`, Apple Silicon, zero global heap), deterministic simulation (`blaze-sim` / `Sim`), and
formal verification (Lean Interaction Trees + transition-system refinement).

## 1. The machine ABI (`crates/blaze-machine`, `#![no_std]`, zero global heap)

Every transition **consumes** the current state (or starting inputs) and returns an anonymous union
(`A | B`, or `Step<R, W, T, E>` in Rust-edition crates) of:

- `Ask<R, W>` — a linear typed request `R: Request` paired with the disjoint waiting state
  `W: Waiting<Ctx, Waits = R>`. Because the transition consumes resources into `R` directly (e.g.
  `file.close()` moves `File` into `Close`, and `ReadInto` takes an owned `DmaBuf`), there is no
  `asks(&self)` borrow trap, no self-referential state, and no `Pin`.
- `Call<Child, K>` — structured sub-machine delegation without flattening the child's internal
  states or requests into the parent.
- `Done<T>` / `Fail<E>` — terminal outcomes.

```rust
/// Something a machine may ask a handler to perform. The answer's type is fixed by the request.
pub trait Request: Sized {
    type Answer<'s>;
}

/// One waiting state of an explicit machine. Its signature is its node in the graph.
pub trait Waiting<Ctx = ()>: Sized {
    type Waits: Request;
    type Next; // e.g. Ask<SendFrom, Sending> | Ask<Close, ClosingOnErr> | Done<Resp> | Fail<FetchError>

    #[blaze::consumes]
    fn resume(self, answer: <Self::Waits as Request>::Answer<'_>, cx: &mut Ctx) -> Self::Next;

    /// Called on external cancellation or deadline expiry under a bounded grace budget.
    #[blaze::consumes]
    fn on_cancel(self, _cx: &mut Ctx) -> CancelAction<Self::Next> {
        CancelAction::Abandon
    }
}
```

### Concurrency, linking, and batching are first-class requests

A single-op ping-pong loop cannot saturate `io_uring` or handle duplex child pipes (`stdout` +
`stderr` + exit without deadlocking). `blaze-machine` defines structured concurrency and linking
combinators directly as `Request`s:

- **`Race<A, B>`** (product submitted, sum returned): submits both `A` and `B`, resumes on the
  first completion (`RaceOutcome::First` / `RaceOutcome::Second`), and cancels the loser—holding
  any owned DMA buffer lent to the loser until the kernel's cancel CQE drains.
- **`Chain<A, B>`** (linked pair, `IOSQE_IO_LINK`): submits `A` and `B` as one linked kernel
  chain (`WriteFrom -> Flush -> Close` in one syscall round-trip), returning `ChainOutcome::Both`
  or `ChainOutcome::FirstOnly` on linked short-circuit.
- **`Batch<R, const CAP: usize>`** (bounded-N fan-out): submits up to `CAP` requests into one
  `io_uring` batch (directory walks, tree copies, pipelined CAS/HTTP fetches) and yields
  completions (`BatchReady { index, answer }`) as they land.
- **Unified Machine Graph Verifier & Lean 4 Emitter (`blaze_machine::graph`):** verifies both
  explicit `Waiting` graphs and LIR-lowered `.do` graphs (`blaze_lir::coroutine_machine`) for
  reachability, dead ends, unmarked cycles, linear handle leak/double-close freedom on every
  `Done`/`Fail` edge, and bounded `GraceBudget` on every `CancelCleanup` chain—and emits Lean 4
  `inductive State` / `inductive Step` / `theorem verified_machine_no_leak` transition systems.

### Compile-time guarantees

| Property | How it is held |
|---|---|
| A state goes only where its `Next` says | the return type (`A \| B` or `Step`) |
| A state reaches every member of its `Next` | generic-body reachability check (excluding `!` arms) |
| Every state reaches an end or explicit loop | reflected graph check (`Done`, `Fail`, or `#[blaze::loop_state]`) |
| A state is resumed only with its request's answer | `Request::Answer<'_>`; no "out of step" arm can be written |
| A transition cannot be forgotten | `resume` consumes `self` (`#[blaze::consumes]`) |
| A request is performed or cancelled, never dropped | requests are `#[blaze::linear]` |
| Normal `Fail(e)` paths close their handles | linear handle has no `Drop`; only `Stopped` abandons |
| Data that exists in one state is invisible in another | field of that state's type |
| `resume` allocates only through an explicit arena | `&mut Ctx` carries `&BrandedBump<'id, Frame>` (`drop-with.md`) |

### `TaskFrame`: cross-thread migration without `Pin`

A paused machine paired with its bump arena is safe to move across worker threads without `Pin`:
under [drop-with.md](drop-with.md), `FrameVec<'id, T>` and `Ref32<'id, T>` store `NonNull<T>` into
heap-allocated arena chunks (or relative 32-bit offsets) and **zero** back-pointers to the `Frame`
header struct itself. `TaskFrame` pairs the owned arena with an existentially branded machine state
(`Send` iff the state's payload is `Send`) and tracks `in_flight_dma: u32`: resetting or dropping
the arena while a cancelled kernel DMA operation is still in flight is refused until all cancelled
CQEs drain (preventing kernel DMA use-after-free by construction).

### One-shot lexical algebraic effects (the compiled direct-style form)

A direct-style function over lexical branded capabilities (`fs: Fs<'h>`, `net: Net<'h>`,
`clk: Clock<'h>`) carries `EffectSet::SUSPEND` in `FnHeader` (bit 15, packed beside
`PANIC | ALLOC | BLOCK | DIVERGE` with zero ABI overhead) and an effect row on `A | B`:

- **Tail-resumptive operations** (`Clock::now`, `Rng::next`, `Alloc`, and non-blocking syscalls)
  specialize via `LoweringMode::PlainFn` (`blaze-ty::effect_ty`) with zero suspension frame.
- **Suspending operations** use **postfix `.do`** (`dir.open_write(..).do?.flush().do?`), so every
  yield, interleaving, cancellation, and crash point stays visible in source, paired with
  context-sensitive lexical **`defer { ... }`** (`DEFER_EXPR` / `Expr::Defer`) for ergonomic linear
  handle cleanup across `.do?` early returns—and obey the **no-local-borrow across `.do`** rule
  (`local-borrow-across-do` in `blaze-typeck` + `LocalSlotAddrAcrossSuspend` in
  `blaze-lir::coroutine_machine`).
- `blaze-lir::coroutine_machine::split_machine` lowers suspending functions to the **exact same
  zero-`Pin` `Machine` ABI** as explicit state-graph machines, driven by the same `Handler`
  (`IoUring`, `Kqueue`, `Sim`, `Script`).
- Effect polymorphism (`EffectRowSet` in `blaze-ty::effect_ty`: `fn map<T, U, / E>(.., f: fn(T) -> U / E) -> .. / E`)
  removes `sync`/`async` function coloring across `bstd`. Suspending effects are monomorphized
  statically (`SuspendBehindDyn` refused behind unboxed `dyn Fn`); tail-resumptive effects work
  behind `dyn` freely.
- Continuations are strictly **one-shot linear**, so linear handles and typestate protocols
  (`UnflushedFile -> flush() -> FlushedFile -> rename()`) hold across suspension points.
- **Fine-grained Syscall Effects (`SyscallSet`):** kernel authority in `blaze-sys` partitions into
  tail-resumptive (`sys::clock`, `sys::rng`, `sys::vm`, `sys::stat`) and suspending (`sys::fs`,
  `sys::net`, `sys::proc`, `sys::sync`) domains, enabling compile-time minimal `seccomp-bpf` /
  Seatbelt sandbox synthesis, zero-`#[cfg]` unikernel/wasm64 handler portability, and sound
  `#![blaze::sans_io]` enforcement (`effects ∩ (SYSCALL_ALL | SUSPEND) == ∅`).

## 2. Two-tier capabilities and generational handles (`blaze-core`, module `handle`)

The compiler enforces a clean two-tier boundary ([drop-with.md](drop-with.md) §6):

- **`#[blaze::capability]`** — executing/ambient authority (`Io`, `Cx`, `Alloc`, `BrandedBump`,
  `BrandedAlloc`, `RankedLock`). Forbidden in `#![blaze::sans_io]` ADT fields
  (`blaze::capability_in_sans_io`). Passed only as method arguments or held by drivers.
- **`#[blaze::handle]`** — inert 8-byte generational tokens (`File<'io, Mode>`, `Dir<'io, Mode>`,
  `Conn<'io>`, `Listener<'io>`, `Socket<'io>`, `Child<'io>`, `Watch<'io>`, `Map<'io>`,
  `Lock<'io>`). Permitted inside `#![blaze::sans_io]` state structs; themselves checked to store no
  `#[blaze::capability]` fields.

```rust
#[blaze::handle]
pub struct Handle<'io, K> {
    slot: u32,
    generation: NonZeroU32, // starts at 1; slot permanently retired at u32::MAX; Option<Handle> == 8 B
    _brand: Brand<'io>,
    _kind: PhantomData<K>,
}
```

- **Deterministic slots:** issued in open-completion order in `Sim`; stale handles are refused by
  name with `(slot, expected_gen, found_gen)`; handles are never serialized to records.
- **Typestate durability:** writable files start as `File<'io, Unflushed>`; `flush()` consumes
  `Unflushed` and returns `File<'io, Flushed>` in its answer, making durability ordering
  (`write -> flush -> rename`) checkable in types.
- **No `Drop` that does IO:** closing is `file.close() -> Close`, consuming the linear `File` and
  returning `Result<(), FsError>`.
  - **Normal `Fail(e)` paths** must close their handles (or return them in `Fail` to the caller).
  - **External cancellation (`Stopped`)** runs `on_cancel` under a bounded `GraceBudget` or
    abandons handles to `HandleTable::reclaim_task(task_id, normal_exit)`.
  - **Loud leak detection:** reclaiming any handle on a normal exit (`Done`/`Fail`) returns a typed
    `LeakedHandles` error (hard failure in `Sim` and tests, never a silent counter).

## 3. Zero-alloc typed requests (`blaze-core`, module `request`)

One linear `#[blaze::linear]` type per operation replaces the 63-variant `Effect` and `Done` enums
at the surface, using `#[blaze::args]` `*Opts` structs (rule 83) and strong newtypes (`ByteOffset`,
`ByteLen`, `MaxBytes`; rule 73):

- **Owned DMA buffers (`DmaBuf`):** `ReadInto`, `WriteFrom`, `ReadAll`, `SendFrom`, `RecvInto`,
  `List`, and `Spawn` output take caller-owned buffers (from `Frame` or registered `Spans`) with
  explicit byte caps (`MaxBytes`) and return the buffer in `Answer` (`FilledBuf` / `BufError`). A
  driver **never** allocates on the global heap for a caller (`Done::Read(Vec<u8>)` is deleted,
  not wrapped).
- **Capped lent reads:** `Read` returns `Result<&'s [u8], FsError>` backed by the driver's
  per-worker scratch ring and statically/explicitly capped to `LENT_READ_MAX`.
- **Absent is never empty:** `FsError::NotFound` is always an explicit error (`ReadAll` never maps
  missing files to `[]`).

Groups: `file` (`Open`, `Close`, `Read`, `ReadInto`, `ReadMany`, `WriteFrom`, `Flush`, `Barrier`,
`Truncate`, `Lock`, `Unlock`, `Map`, `MapWrite`), `path` (`ReadAll`, `Write`, `Rename`, `Remove`,
`MakeDir`, `List`, `Stat`, `Link`, `CopyData`, `Mode`, `Times`, `SyncDir`), `watch`, `net`, `proc`,
`stdio`, `time`, `random`, `trust`, `host`.

## 4. Capabilities, handlers, and drivers (`bstd`)

- Capability methods build requests and take no `cx`: `file.read_into(ReadIntoOpts { .. })`.
- Outside a machine/effectful function, `cx.run(request)` performs one request under the task's
  deadline; refused in `#![blaze::sans_io]` code.
- `Handler` + `drive(handler, cx, machine)` is the one execution loop for `IoUring`, `Kqueue`,
  `SimHandler` (over `blaze_core::sim::Sim`), and `Script` (deterministic unit-test transcripts
  with zero simulator).

## 5. Order (compiler & ABI first, workflows on lexical effects)

| Wave | What | Effort |
|---|---|---|
| 1 | **ABI & compiler foundation (landed in #2850):** `EffectSet::SUSPEND` (`FnHeader` bit 15); `#[blaze::handle]` two-tier firewall (`blaze-surface`, `blaze-typeck`, `blaze-ladder`); `crates/blaze-machine` (`Request`, `Waiting`, `Ask`, `Race`, `Batch`, `Call`, `Machine`, `TaskFrame`, `Handler`, `Script`); `crates/blaze-core/{handle,request}` (`Handle<'io, K>`, `HandleTable`, typestate `File`, `DmaBuf`, `SimHandler` + DSTs) | `[████░░░░░░]` |
| 2 | **Compiler `.do` + `defer`, effect polymorphism, LIR Machine pass & generic `DriverHandler` (landed):** postfix `.do` (`DO_EXPR`/`Expr::Do`) + `local-borrow-across-do`; context-sensitive lexical `defer { ... }` (`DEFER_EXPR`/`Expr::Defer`); `EffectRowSet` + `LoweringMode::{PlainFn, MachineCoroutine}` + `SuspendBehindDyn`; `blaze-lir::coroutine_machine` zero-`Pin` splitter; `Chain<A, B>` + `blaze-machine::graph` verifier & Lean 4 emitter; non-monomorphized `DriverHandler<D>` + `FixedBufIndex`/`FixedFileSlot`; `bstd::machine`, `Cx::drive_machine`, `HttpExchangeMachine` | `[███████░░░]` |
| 3 | **Syscall Effects, live `.do`/`defer` codegen, `Join`/`LeasedBuf`/`RecordHandler` & first real ports (landed):** 8-domain `SyscallSet` (`blaze-ty`) + 0-dep `blaze-sys::effects` Seatbelt SBPL / Linux seccomp policy synthesizer + sound `blaze-typeck`/`blaze-lint` sans-IO checks; `Join<A, B>` + `ScopeBudget`, linear `LeasedBuf<'io>`/`BufferRing`, `RecordHandler<H>` -> `Script` reproducer; `MachineBudget::max_bytes` state-size bound (`blaze-lir`); `Chain`/`Batch` + zero-`Effect`-bridge `DirectFsHandler` + `UringSqePlan` (`blaze-core`); `Cx::run`/`run_with`, `bstd::fs::req` + `AtomicPublishMachine`, `bstd::net::req`; `FetchUnitMachine` (`blaze-fetch`) + `CasRecordPublishMachine` (`blaze-cas`); `blaze-build` LIFO `defer` + `.do` -> `split_machine` codegen | `[████████░░]` |
| 4 | **Whole-tree port (`bstd` 232-call codemod + remaining IO crates):** implement native `UringHandler`/`KqueueHandler` directly over `UringSqePlan`/`kqueue` and delete the 63-variant `Effect`/`Done` enum; `astlog` whole-port of `bstd`'s remaining blocking methods; port `bgit`, `blaze-cli`, `blaze-remote`, `blaze-world`, `blaze-proc-macro`, `blaze-launch-rt`, harness daemon, and `scripts` | `[█████████░]` |

## 6. How it is proven

- **Compiler checks:** unit tests in `blaze-ty` (`SUSPEND` subtyping, `FnHeader` packing, `EffectRow`),
  `blaze-surface` (`handle` tool attr), and `blaze-typeck` (two-tier `#[blaze::capability]` vs
  `#[blaze::handle]` firewall, including rejecting a handle that tries to smuggle a capability).
- **Machine ABI & `Script`:** explicit transport machine (`HttpExchange` with partial writes/reads,
  `Race<Recv, Timer>` timeout, mandatory `Close` on error) plus planted-bug controls (short-read
  mishandling and skipped close caught by `Script`).
- **Handle table & DMA cancellation DST (`blaze-sim`):** interleaved multi-task open/close/cancel
  with slot reuse, generation exhaustion retirement (`u32::MAX`), task reclamation isolation, leak
  failure on normal exit, and in-flight DMA drain before `TaskFrame` reset—each with a planted-bug
  control the oracle catches.
- **Durability & crash DST (`SimHandler` over `blaze_core::sim::Sim`):** typed `WriteFrom` +
  `Flush` + `Rename` machine under faults, short writes, ENOSPC, and power-cut crashes verifying
  typestate durability ordering and `NotFound` != empty.
- **Formal verification path:** Lean Interaction Trees for effectful workflows and Lean transition
  system refinement for explicit protocol/durability `Machine`s.

## 7. Losing alternatives

- **Rust `async`/`await` (`Future` + `Waker` + `Pin`):** ambient reactor, viral function coloring,
  self-referential `Pin`, invisible state graphs, and linear values cannot survive dropped futures.
- **Manual sans-IO state structs for every workflow:** great runtime ABI and protocol form, but
  hand-lowering 200+ IO sites in `bgit`/`blaze-cli` before building coroutine lowering creates
  unnecessary boilerplate where one-shot lexical algebraic effects compile to the exact same ABI.
- **Stackful fibers / multi-shot continuations:** per-task stacks hurt `one-memory-measure`, hide
  yield/crash points, and multi-shot breaks linear types (`File` could be closed twice).
