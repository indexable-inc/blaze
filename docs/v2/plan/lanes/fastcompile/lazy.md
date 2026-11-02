---
status: plan
summary: blaze v2 lane fastcompile, idea 1 (lazy compilation, per function): crates/blaze-lazy, its stubs and resolver per arch, the daemon's single-flight fetcher, what is compiled ahead and why each case is exact, the release refusal, the hooks in inproc, link, codegen and cargo, and the state of the art it matches or beats.
date: 2026-10-30
---
# fastcompile: lazy compilation, per function (`crates/blaze-lazy`)

Idea 1 of [compile-time.md](../../design/compile-time.md). A dev run or test compiles only
the functions that execute. Every other function is a stub that asks the daemon for its code
on its first call, gets it patched in, and jumps. Release builds and `blaze build` outputs
are compiled whole.

## Pieces

| module | side | what |
|---|---|---|
| `mode` | daemon | `allow(&Purpose)`: `Refused` for a release profile (by name, `release` or `bench`, or by opt-level 2, 3, `s` or `z`), for `Mode::Output`, and for a build that writes a file. |
| `plan` | daemon | `Plan`: what is compiled ahead (`Ahead::Entry`, `Ahead::Start`), the lazy functions by dense `LazyId`, lookup by code hash for history, and a function's likely callees. |
| `stub` | daemon | One position-independent region per session: the hook's cell, one shared resolver, and 16-byte entries. Generated as bytes for x86-64 (SysV, Win64) and AArch64 (with or without SVE). |
| `fetch` | daemon | `Fetcher`: a deterministic state machine with single flight, four priorities and failure handling. |
| `runner` | daemon | `Runner`: the fetcher on worker threads, against `Compiler`, `Placer` and `Answers`. |
| `compile` | daemon | `Lazily`: the `Compiler` of a run, over a `Source` (codegen's `Later`, the front end and the store, as cargo holds them). |
| `session` | daemon | `Session`: refusal, plan and stub region together, plus what to fetch first. |
| `wire` | both | The frames `Ask`, `Ready` and `Failed`, carried in blaze-inproc's `kinds::LAZY` range. |
| `hook` | run process | `blaze_lazy_resolve(id) -> address`, which the resolver calls. |

The run process links the crate without its `daemon` feature, so it gets only `hook` and
`wire`.

## The stub ABI

An entry is `endbr64; push id; jmp resolver` on x86-64, and `bti c; movz/movk w16, id;
b resolver` on AArch64. The resolver saves every register a call can carry a value in:

- **x86-64**: `rax` (with `al`, the vector count of a variadic call), `rcx`, `rdx`, `rsi`,
  `rdi`, `r8` to `r10`. The vector state goes through `XSAVEC`, `XSAVE` or `FXSAVE`, with the
  mask and size read from `XCR0` and `CPUID` leaf 0xD (`xmm`, `ymm`, `zmm`, AVX-512 masks,
  APX `r16` to `r31`).
- **AArch64**: `x0` to `x15` (with `x8`, the indirect result register), and `q0` to `q31`.
  With SVE it saves `z0` to `z31`, `p0` to `p15` and `FFR` instead.

The resolver then calls the hook on a stack aligned for the C ABI, probing every 2 KiB.
It restores everything and tail-jumps with the caller's stack exactly as it was. A hook that
answers 0 traps (`ud2`, `brk #1`). Only `r11`, `x16` and `x17` are not kept: blaze-link's
slot stubs clobber them already.

## Single flight and failures

`Fetcher` keeps one state byte, one priority and one target per function, and waiters only
for functions someone is asking for. An ask of a published function is answered at once,
with no allocation. An ask of a function being compiled joins its waiters. A resent ask is
deduplicated. The priorities, most urgent first:

1. Asked.
2. A compiled function's callees (from its relocations, calls and tail calls only, not
   addresses taken).
3. History: usercode's `Reached` set.
4. Idle: everything else, when the runner's `idle` is on.

Background compiles leave one worker free for asks.

A compile that fails or panics is caught and answered `Failed`, naming the instance and the
item. The run is stopped once, through `Answers::stop`. A failure nobody called stops nothing,
and `Runner::close` reports it. In the run process, a `Failed` answer, a broken channel or an
answer to another ask writes the reason to stderr and aborts. The resolver never jumps to
anything the daemon did not publish.

The waiters live in one `blaze_mem` slab per fetcher, a list per function in ask order, with
room for every inproc ask slot of 4096 run processes. A warm runner's first ask allocates nothing. An ask past
that room fails, naming the limit, and stops the run.

## The compiler of a run

`Runner` needs a `Compiler` that is `Send + Sync + 'static` and serves every worker at once,
for the whole run, while the user edits. Codegen's `Build<'a, B>` borrows the front end and
the backend, and the front end's reader blocks every write (`base_db::Shared::write` waits for
readers; `Db` has no fork, [db.md](../frontend/db.md)). So no reader is held for the run:

- **Codegen** turns the walk into an owned `Later<I, S>` (`Build::into_later`, queue 01038):
  the nodes and shapes, generic over the front end's instance and static, with the backend
  and the front end passed per call.
- **blaze-lazy** (`compile::Lazily`) takes a reader per compile from its `Source` and lets it
  go before placement. A write that stops the reader (`Cancelled::PendingWrite`) has the
  compile run again on the next reader, at most 64 times in a row, then fails naming why.
  A memo hit takes no reader.
- **The key decides, not the revision.** A lazy slot never takes a patch's code
  (`blaze-link`'s `lazy.rs`), so a lazy function's code is the image's for the whole run. A
  compile at a later revision is exact while the front end still describes the same code and
  arguments for the function's key. Otherwise codegen refuses it with an error naming the
  function and both code hashes, and the run stops: never a new body in the old image.
- **Cargo** (queue 01039) implements `Source` over its unit's front end and the store,
  launches the run with `Launch::Lazy`, and routes inproc's `kinds::LAZY` frames to
  `Runner::frame`. There is no lazy flag: a dev `run` or `test` is lazy, a release one is
  refused (`mode`).

What a compile makes for the store goes to `Source::keep`, so the next build's check reads it.
A memo hit answered by the reading thread keeps its template only after the askers are
answered (`Compiler::hit_answered`), off their path.

## What is compiled ahead, and why each case is exact

Every function, lazy or not, is reached through its jump-table slot. Its address, the one a
program can observe, is its slot stub's, which is fixed when the image is laid out. That is
the same address a whole live build gives it, so laziness only changes where the stub jumps:

- **Function pointers and their comparisons, callbacks handed to C, signal handlers.** The
  pointer is the slot stub, one per function, so two pointers to one function compare equal
  whether it was compiled or not.
- **Vtable entries and statics holding function pointers.** These are relocations to the
  slot stub, written when the data is placed, before `main`.
- **TLS destructors and `atexit` handlers.** They are registered as slot stub addresses. A
  first call at thread or process exit resolves like any other, because the hook keeps no
  thread-local of its own.
- **The entry and what runs before or after `main`.** This covers the `main` shim and
  everything a static in `.init_array`, `.preinit_array`, `.fini_array`, `.ctors`, `.dtors`,
  `__mod_init_func`, `__mod_term_func` or `.CRT$X*` names. These are compiled ahead
  (`Plan::ahead`), so starting a run never waits on a fetch and never depends on when the
  channel is up.
- **Statics.** Statics are data, placed whole before the run.
- **Errors.** The front end's walk checks every instance a whole build would compile,
  including post-monomorphization const errors and layout errors, so a program rustc rejects
  is rejected here too (fastcompile-lazy 4).

One case is not yet exact: a lazy signal handler whose first call interrupts its own thread
inside the hook's ask. That waits on inproc's signal-safe asks (fastcompile-lazy 1).

## State of the art

- **glibc's `_dl_runtime_resolve_xsavec`** (lazy PLT binding): the same shape. One shared
  trampoline, the index pushed by a per-entry stub, and the vector state saved with
  `XSAVEC` sized from CPUID. blaze matches it, and adds AArch64 SVE state and Win64. It also
  resolves to compiled code, not to an existing symbol.
- **LLVM ORC's lazy reexports and `LazyCallThroughManager`**: per-function stubs that compile
  on first call. ORC's x86-64 resolver saves only the argument registers that fixed calling
  conventions use. blaze's saves the whole enabled vector state and `al`, so variadic and
  vector calls are exact. ORC has no single flight across processes. blaze's fetcher compiles
  once per function however many threads ask.
- **V8's lazy compilation and Julia's JIT**: compile on first call, then tier up. blaze does
  the same, with the copy-and-patch backend for the first call (`blaze-stencil`) and hot
  functions tiered up later. Its prefetch of callees and history keeps first-call waits off
  the critical path.
- **Allocation**: `Ready` and `Ask` frames are fixed-size stack arrays. Per-function state is
  three columns indexed by `LazyId`, and lookups by code hash binary-search a sorted column.

## Tests (`crates/blaze-lazy/src/tests/`)

- `stubs`: region layout, the entries, the resolver's cell load and trap, Win64 shadow space,
  the AArch64 encoders checked against `llvm-mc`, and the size limits.
- `abi` (on the host): calls through an entry, whose hook overwrites every argument
  register, match direct calls. It covers integers and floats past the registers, mixed
  arguments, a large aggregate with its indirect result, register pairs, homogeneous float
  aggregates, whole vector registers, a variadic `snprintf` call, and the trap on 0.
- `flight`: 32 threads asking 8 functions at once compile each once. Failures and panics
  answer every asker and stop the run once. Idle compiles report unasked failures. On the
  host, 16 threads call through real entries into a runner.
- `sim`: a deterministic simulation of the protocol, with dropped, duplicated and reordered
  frames, compiles finishing out of order, failures and resends, over 400 seeds. It also
  checks the fetcher's priorities.
- `wire`: round trips, malformed frames, and random bytes, which never panic and re-encode
  identically.
- `plan` and `refuse`: what is compiled ahead, ids, history, callees, and the release and
  output refusals.
- `budget`: an ask of a published function, a lone hit, the frames, the plan's lookups, a
  first ask of a warm runner and a warm round of asks and answers allocate nothing.
- `sim` also checks the slab's waiter lists against a list per function, over 200 seeds.
- `compile`: each compile takes its own reader and lets it go before it returns; a write that
  stops it has it compiled again, and 64 in a row fail naming why; a refusal keeps nothing; a
  memo hit takes no reader and keeps its template only once answered; a tier-up follows the
  same rule; in a run, a function edited before its first call fails and stops the run.
- `hit`: a lone hit is answered before `Runner::frame` returns and is never compiled. Its
  callees are still prefetched, a miss goes to a worker, and a probe that panics fails the
  function, naming it, and stops the run once. Many threads asking for hits at once start
  each function once.
- `wake`: asks while a worker spins, after every worker went to sleep, and in a burst are
  each compiled once and answered; prefetches wake sleeping workers; a close ends spinning and
  sleeping workers at once.

## Hooks (v2/requests/fastcompile.md)

- fastcompile-lazy 1 (inproc): concurrent, allocation-free, signal-safe asks.
- fastcompile-lazy 2 (inproc): a run channel split into a reader and a shared sender.
- fastcompile-lazy 3 (link): slots that start at entries, the stub region, the `Placer`,
  and the runtime linked into the image.
- fastcompile-lazy 4 (codegen): a "compile later" walk and `compile_one`.
- fastcompile-lazy 5 (cargo): dev `run` and `test` take the lazy path, with no flag. A lazy
  release is an error.
- queue 01038 (codegen): `Build::into_later`, an owned `Later` whose compiles check the key
  against the front end before they build.
- queue 01039 (cargo): `Source` over the unit, `Launch::Lazy`, and inproc's lazy frames routed
  to the runner.
- queue 01041 (codegen): `Later::memo_hit` with nothing encoded, and `Later::memo_put` apart.
- fastcompile-sol-lazy 1 (inproc, done): an asker and the reader spin briefly before they
  sleep.
- fastcompile-sol-lazy 2 (cargo): `Compiler::hit`, a probe of codegen's memo and the store
  (now `Lazily::hit`, through `Source::memo_hit`).
- fastcompile-sol-lazy 3 (codegen): `Build::memo_hit`, the probe `hit` wraps.

## Speed of light

Nothing here was run: costs are counted from the code and the machine. The machine is
[link's](../link/speed-of-light.md#the-machine): 3.5 GHz, a system call on an open descriptor
0.15 µs. A first call waits mostly on threads waking, which that table lacks, so three figures
are added below. They are estimates from the hardware and the kernel, not measurements. The
channel is blaze-inproc's ([inproc.md](inproc.md), "The asks"). An asker spins up to 3 µs on
its answer, then sleeps on its slot's futex. The daemon's reader rescans the slots for 50 µs
after its last ask, then sleeps on the doorbell, one byte on the socket.

There are three hot paths: a warm call, a first call, and many threads first-calling at once.

### A warm call

Once bound, a call goes from the caller's `call` to the function's slot stub, which holds the
function's code. On x86-64 the stub is `mov r11, [rip + cell]; jmp [r11 + disp32]`; on
AArch64 it is six instructions ending in `br x16` (blaze-link's `table.rs`). A whole live
build calls through the same stub, so laziness adds no instruction, load or branch. Once the
slot is stored, neither the entry nor the resolver is on the path.

What the slot costs over a direct `call rel32` (two L1 loads and one predicted indirect
branch) is the price of the jump table, which lets a running program be patched. It is paid
in every live build, lazy or not.

**Bound \(0\) over the whole live build. Code \(0\).**

### A first call

The work that can't be skipped when a function has no code yet:

1. Keep the caller's argument state across the ask: save and restore every register a call
   can carry a value in.
2. Tell the daemon. The asker can't run code the daemon hasn't made, and the daemon learns of
   the ask only through a signal across processes: a wake of a sleeping thread, or a cache
   line read by a spinning one.
3. Compile (\(T_C\)) and place (\(T_P\)).
4. Tell the asker, the same way.

| step | cost |
|---|---|
| `XSAVEC` and `XRSTOR` of the AVX-512 state (about 2.7 KiB, components in their initial state skipped) and the resolver's 40 other instructions; on AArch64, 640 bytes stored and loaded by `stp` and `ldp` | about 0.1 µs (\(S\)) |
| a wake: a thread asleep on a futex or a socket read, on another idle core, running again. This is the waker's system call (about 1 µs, \(W_s\)), the IPI, a shallow C-state exit and the schedule; half of a `perf bench sched pipe` round trip | about 3 µs (\(W\)) |
| a cache line moved to a core that spins on it | about 0.1 µs (\(L\)) |
| compile, a memo or store hit | about 1 µs |
| compile, blaze-stencil, a 100-instruction function (30 to 60 ns an instruction, [stencil.md](stencil.md)) | 3 to 6 µs |
| compile, Cranelift `opt_level=none`, the same function ([stencil.md](stencil.md)) | 10 to 30 µs |
| place: a TLSF take, the bytes and relocations, `publish` (nothing on x86-64; `dc cvau`, `ic ivau` per line, one `dsb` and `isb` on AArch64) and the slot's release store | about 0.2 µs (\(T_P\)) |
| an uncontended lock and unlock, a hash probe, a short `Vec` push | about 20 ns each |

Both ends of the channel are asleep before an isolated ask, so the bound is

\[ T^{*} = S + 2W + T_C + T_P \approx 6.3\ \mu s + T_C. \]

That is 7.3 µs with a memo hit, 11.3 µs with blaze-stencil (5 µs) and 36.3 µs with Cranelift
(30 µs), plus 0.1 µs of bookkeeping (\(B\), below).

**What the code does,** counted from it:

- **In the run process** (the entry, the resolver and `hook::fetch`): `push` and `jmp`; then
  \(S\); one load for `attach`; the request counter's `fetch_add`; a 4 KiB answer buffer
  zeroed (about 20 ns); one compare-and-swap that claims a slot; 16 bytes written; the
  doorbell's `send`, the first \(W\); and the futex wait. After the answer: three words read,
  the slot freed with three atomics, and the frame decoded. That is about 0.2 µs on top of
  \(S\) and the wakes.
- **The reader** (inproc's) wakes on the doorbell, scans 64 state words, takes the slot with a
  compare-and-swap and calls `Runner::frame`. That decodes 16 bytes, takes the fetcher's lock,
  runs `Fetcher::ask` (a hash insert and a slot taken in the waiters' slab, with no
  allocation on a warm runner, `budget.rs`), counts the compiles it queued and lets the lock
  go.
- **The hand-off to a worker**, changed by this pass:
  - *Before*, it was `notify_all` on the condvar. Every sleeping worker woke, which put a third
    \(W\) on the critical path, and the woken workers all contended for the fetcher's lock.
    The reader also made that system call on every ask, duplicates included.
  - *Now*, a worker that runs out of compiles spins on `Shared::jobs` for 50 µs (`SPIN`), one
    worker at a time. The reader bumps `jobs` under the lock, so the spinner sees the bump
    within about \(L\) and takes the compile. A sleeping worker is woken (`notify_one`) only
    for a compile no spinner covers.
- **The worker** takes the lock, calls `Fetcher::next`, lets the lock go, compiles (a memo
  probe with no reader first, then a reader taken and let go around the compile, about
  0.1 µs: an atomic check that no write is pending and the front end's handle), and places
  under the placer's lock (held for that one placement). It then takes the lock again for
  `Fetcher::done` and sends the answers.
  - *Before*, the answers waited on the callee scan (with a dedupe quadratic in the number of
    callees), the queuing of the callees and its wakes, and the tier runner's bind.
  - *Now*, the answers go first, and all of that comes after them, off the asker's path.
- **The answer** goes through `Runs::send`: a lock, a map probe, and the `Sender`'s `Arc`
  cloned and dropped. Then three words go into the slot, the state is stored with release,
  and the futex wake is the second \(W\). inproc makes the `FUTEX_WAKE` on every answer, after
  the store, so an asker still spinning sees the store within \(L\), and the wake's system call
  (about 0.3 µs with no sleeper) is the answering thread's, not the asker's.

- **A lone hit skips the worker** (`Compiler::hit`, request `fastcompile-sol-lazy 2`). An ask
  more than 1 µs after the last one (`ALONE`) probes the compiler for code it already has. On
  a hit, the reading thread claims the function (`Fetcher::start`), places it, and answers its
  asker directly, never as a waiter. Nothing wakes, no cache line moves to a worker, and
  nothing is allocated (`budget.rs`). A miss costs the probe and one more hold of the lock,
  then goes to a worker as before. Asks closer together than 1 µs are a burst from several
  threads, and stay with the workers so that they place in parallel. What the hit keeps for
  the store (its template's put, about 0.15 µs to encode) comes after the answer
  (`Compiler::hit_answered`); before it did, a hit was 0.15 µs over the bound below.

**How far it is.** The bound also carries the bookkeeping no design avoids, \(B \approx
0.1\ \mu s\): claiming a slot, recording the compile once for single flight, and routing the
answer to its process. On top of that, the code spends about 0.1 µs: the 4 KiB answer buffer
zeroed, the frame decoded, the clock read, two more holds of the fetcher's lock and the
answer's route in `Runs::send`. Handing an ask to a spinning worker adds about 0.2 µs: the
`jobs` line and the fetcher's lock line each move to the worker's core. Handing it to a
sleeping worker adds \(W\).

- **In a burst**, the reader rescans for 50 µs after its last ask and an asker spins for 3 µs
  (`fastcompile-sol-lazy 1`), and a worker spins. The bound is \(S + 2L + B + T_C + T_P\)
  while the answer comes within the asker's 3 µs. A longer compile adds the asker's wake:
  \(S + L + B + T_C + T_P + W\).
- **Isolated**, nothing has asked or compiled for 50 µs, so the channel and the workers are
  asleep: \(T^{*} + B = S + 2W + B + T_C + T_P\). "Before this pass" is the herd and the
  answer behind the callee scan and bind: \(W + 0.5\ \mu s\) more.

| compile | in a burst: bound | handed to a worker | now | isolated: bound | before this pass | now |
|---|---|---|---|---|---|---|
| memo hit (1 µs) | 1.6 µs | 1.9 µs (+19%) | 1.7 µs (+6%) | 7.4 µs | 10.9 µs (+47%) | 7.5 µs (+1%) |
| blaze-stencil (5 µs) | 8.5 µs | 8.8 µs (+4%) | 8.9 µs (+5%) | 11.4 µs | 14.9 µs (+31%) | 14.7 µs (+29%) |
| Cranelift (30 µs) | 33.5 µs | 33.8 µs (+1%) | 33.9 µs (+1%) | 36.4 µs | 39.9 µs (+10%) | 39.7 µs (+9%) |

A memo hit is answered by the reading thread in both regimes; a compile is a worker's.

Recounted on 2026-10-30, with inproc spinning (`fastcompile-sol-lazy 1`) and the compiler of
a run (above) in place:

- The asker always rings the doorbell, after it marks its slot asked, so the `send` runs
  alongside the reader's scan and is off the critical path.
- A worker's compile now takes a reader and lets it go, about 0.1 µs, which is the 0.1 µs
  added to the blaze-stencil and Cranelift rows. A memo hit takes none, on either thread.
- The answer's unconditional `FUTEX_WAKE` is off the asker's path (above). It delays the
  answering thread's next work by about 0.3 µs.
- A memo hit's template is kept after the answer. Until codegen splits the put from the hit
  (queue 01041), `Later::memo_hit` encodes it first, and the burst's memo hit is 1.85 µs
  (+16%). That gap is codegen's.

An isolated compile still waits on a worker's wake. Every worker is asleep only when no
compile ended in the 50 µs before the ask, which makes that ask the first of a burst. A run's first calls do come in bursts: its start, each new code
path, and chains in which one thread first-calls one function after another. Each call of a
chain starts \(W\) plus the program's work between the two calls after the last answer, which
is under 50 µs unless the program computes in between.

So the third wake is paid once per burst, adding \(W/n\) to each of a burst's \(n\) first
calls. That is within 10% of a memo hit's bound from \(n = 7\) on, and of blaze-stencil's from
\(n = 4\) on. The isolated compile itself stays \(W\) over its bound, and there are two ways
to close that gap, neither taken:

- the reader compiles a miss itself, which leaves every ask that arrives meanwhile unread for
  the whole compile (30 µs with Cranelift, to save 3 µs). A hit takes only its placement, so
  hits are the ones the reader serves;
- a worker spins for the whole run, which burns a core for the run.

Prefetch takes many first calls off this path entirely. A callee queued from its caller's
relocations, or from history, is usually published before it is called. Its late ask is then
answered by the reader at once, with no worker and no allocation (`Shared::ask`'s fast path,
`budget.rs`): \(S + 2W\) plus about 0.3 µs, with \(T_C\) gone.

The spinning costs at most one core for 50 µs after a burst's last compile, on the daemon's
side, and nothing while compiles run. It is the competitive spin-then-park of rayon's sleep
module (an event counter, checked before parking), with one spinner at a time as Go's scheduler
and Tokio limit theirs. A wake missed by a race costs parallelism, never a compile: a worker
that finishes a compile takes the next one itself.

What is left of a burst's memo hit, the 0.1 µs over \(B\), is spread over about ten steps of
20 ns each in blaze-inproc, the daemon's `Runs` and here. Nothing in it is a wake, a system
call or an allocation. The asker's doorbell `send` is still a system call while the reader
spins, but it runs after the slot is marked asked, alongside the daemon's work, so it is off
the critical path.

### Many threads first-calling at once

\(N\) threads ask for \(F \le N\) distinct functions at once, with \(n_w\) workers and one
reader per run process. What can't be skipped:

- each function is compiled once, at most \(n_w\) at a time;
- the one reader reads each ask, at a cost \(r\) (the slot, the decode, the fetcher's lock,
  `Fetcher::ask`) of about 0.3 µs;
- each asker is woken once, by its own slot's futex, at \(W_s\) to whoever wakes it;
- a function's waiters are answered by the worker that compiled it.

So

\[ T^{*}_{N} \approx 2W + \max\Big(N r,\ \Big\lceil \tfrac{F}{n_w} \Big\rceil (T_C + T_P) + \Big\lceil \tfrac{N}{\min(F, n_w)} \Big\rceil W_s\Big). \]

Take 32 threads, 8 functions, 4 workers and blaze-stencil compiles. The bound is
\(6 + \max(9.6,\ 2 \cdot 5.2 + 8 \cdot 1) = 24.4\) µs.

- **Before this pass:** the reader's `notify_all` was a system call on every ask, because std's
  futex condvar wakes unconditionally. It also woke every idle worker, so \(r \approx 1\) µs.
  That gives \(6 + \max(32,\ 18.4) + 3 \approx 41\) µs, 68% over.
- **Now:** the reader makes a system call only for an ask that wakes a sleeping worker, at most
  \(n_w - 1\) of them per burst. That gives \(r \approx 0.3\) µs plus \(3 W_s\) once. The three
  sleeping workers start their first compiles \(W\) late, so the total is
  \(6 + \max(12.6,\ 21.4) = 27.4\) µs, 12% over.
  - A burst that arrives while compiles run is at the bound: its workers are running and take
    the next compile themselves.
  - The 12% is a constant \(W\), so it falls under 10% for any burst with more than about
    30 µs of compile work.

Asks in a burst come closer together than 1 µs (\(r\)), so they stay with the workers. Only
a burst's first ask can be served by the reader as a lone hit, which delays the rest by its
placement, its answer's wake and its kept template: about 0.2 + 0.3 + 0.15 µs, under 0.7 µs
once per burst.

Single flight costs an extra asker only its waiter push and its own answer. `flight.rs`
checks that 32 threads asking for 8 functions get 8 compiles.

**What is left, and whose it is:** the answer term
\(\lceil N / \min(F, n_w) \rceil W_s\). Thirty-two askers of one function cost its worker 32
wake system calls, about 32 µs. One futex per function in flight, woken once with
`FUTEX_WAKE` of all, would bring that to the kernel's per-thread wake, about 0.3 µs each.
That is blaze-inproc's slot design (one futex per ask, model-checked in `ask/model.rs`), so it
is named here and not changed.

### Setup, once per run

- `Plan::new` makes one hash insert and one row per function. It now sorts the code-hash
  column with `blaze_ids::names::sort_by_name`, a stable radix sort on an 8-byte prefix, in
  place of \(n \log n\) compares of 32-byte keys (about 3.5 million at 200,000 functions).
- `Plan::callees` dedupes through a bitmap kept by its caller (`Plan::scratch`), in time
  linear in the relocations. It was `Vec::contains`, quadratic in a function's callees: about
  a million compares for a function that dispatches to a thousand others.
- `stub::generate` is one pass of 16 bytes per function: 3.2 MB for 200,000 functions, about
  0.3 ms at the page-cache copy rate.

### Against the state of the art

- **glibc's lazy binding** (`_dl_runtime_resolve_xsavec`) is the same trampoline at the same
  cost \(S\), and resolves in-process by a symbol lookup. A first call here compiles in the
  daemon instead, so it adds \(2W\) (\(2L\) once the channel spins).
- **V8's lazy compilation and LLVM ORC's lazy call-throughs** compile on the calling thread,
  so their bound is \(S + T_C\). Here the compiler, memo, store and code memory are the
  daemon's: a compiler in the program would mean linking the compiler into every dev
  executable. The cross-process signal is paid for, and prefetch keeps most first calls from
  waiting on a compile.
- **ORC has no single flight across processes.** The fetcher compiles once however many
  threads and processes ask, at one waiter push per extra asker.
