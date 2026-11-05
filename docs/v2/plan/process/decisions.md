---
status: plan
summary: blaze v2: decisions made, so no lane reopens them. Newest last.
date: 2026-10-29
---
# Decisions

Each line is settled. A lane that thinks one is wrong says why in its request file; it does not
work around it.

| date | decision | why |
|---|---|---|
| 10-29 | One warm VM per project (or tenant); branches, PRs and CI jobs are views, not VMs or forks | one copy of every result, shared by every view; forks only for tenants, scale-out, resume |
| 10-29 | One big integration branch, one PR; testing all at once by the coordinator | Andrew's call; the oracle and corpus exist before integration so failures come attributed |
| 10-29 | Nothing compiles or runs, by agents or the coordinator, until every lane is done; the manifest lint is the only check before then | Andrew's call; checking half-written crates only finds they are half-written |
| 10-29 | Finished code only: no `unimplemented!`, `todo!` or deferred parts | v2 lands complete or not at all |
| 10-29 | Everything is ours at every level: any crate, vendored dependency, format or interface changes when it makes blaze faster and keeps Rust's semantics | best performance possible |
| 10-29 | Our own query database, from `vendor/salsa`; no longer kept close to upstream | input-hash keys, fingerprints on edges, views, zero-copy image need it |
| 10-29 | The database is `Sync` and shared by reference; no per-thread handles; all parallel work is plane tasks | salsa's per-thread handles and threads spawned by queries bypass the scheduler |
| 10-29 | One memo of results, the database's; the plane schedules and joins equal keys but remembers nothing | two memos can disagree |
| 10-29 | One arena of views, `blaze_vfs::ViewId` | a view has one id everywhere |
| 10-29 | No IDE; the agent API is the only interface; the CLI is its client with cargo's behavior | LLMs are the users besides people at a terminal |
| 10-29 | Flat syntax everywhere, rowan removed | no IDE to keep |
| 10-29 | Data-oriented everywhere: tag plus word, CSR lists, hot and cold split, bitsets, size budgets | memory, cache misses, SIMD, compression |
| 10-29 | Hardware acceleration chosen at run time, every path bit-identical; no GPU | determinism; the work is irregular and each edit is tiny |
| 10-29 | Compression by tier: hot plain and mapped, warm bit-packed in place, cold per-column codecs plus zstd | zero-copy where it is read, small where it is not |
| 10-29 | Main frozen for the crates v2 replaces (#1540): bug fixes only, merged into `blaze-v2` at checkpoints | the branch can't rot the way blaze-next did |
| 10-29 | `rustc_type_ir` and the trait solver belong to db | types and the solver become `u32`-indexed tables |
| 10-29 | A new crate arrives with a `src/lib.rs`; every new workspace dependency goes through a request | one unloadable member stops the whole workspace from loading |
| 10-29 | The first compile checks each lane's crates on their own, in one VM with committed memory and a job count bounded by it | one broken crate can't hide the rest; never OOM |
| 10-29 | Reads never lock or write shared memory; values immutable once published; memory reclaimed at task boundaries; no build lock (plan/design/concurrency.md) | contention on one cache line costs more than the work on 100 cores |
| 10-29 | `unsafe` only where it buys measured speed, behind safe APIs, each block with `// SAFETY:`; proved by our own Miri (the MIR evaluator), sanitizers in our code generator, model checking and fuzzing, never rustc (plan/design/unsafe.md) | fastest code that is still sound; no-rustc rules out Miri as it is |
| 10-29 | Dev and test runs compile lazily per function with copy-and-patch codegen and no binary on disk; one global cache; semantic edits and declared intent on the agent API (plan/design/compile-time.md) | remove steps, not just make them incremental |
| 10-29 | It just works, and nothing binds us to rustc: its choices are defaults where following is free and departing costs users, each kept replaceable behind one interface; changes come opt-in or once the corpus shows nothing breaks; everything nobody can tell is free | Andrew: don't bind ourselves; it should just work, as fast as possible |
| 10-29 | Every component within 10% of its theoretical maximum (speed of light), bounds written per lane in `speed-of-light.md`, measured by proof's budgets; more than 10% off is a bug | Andrew: maximally performant, SOTA, within 10% of theoretical maximum |
| 10-29 | Memory managed like a game engine: allocators by lifetime (frame, request, revision, image), pools, reserved never-moving regions, generational handles, per-subsystem budgets; zero general-purpose allocations in the warm loop (plan/design/memory.md) | Andrew: allocate like a highly optimized game |
| 10-29 | One native I/O driver per target, chosen by `cfg` and never `dyn` (io_uring, kqueue, IOCP); where io_uring is unavailable (old kernel, `kernel.io_uring_disabled`, a seccomp policy such as Docker's default) blaze stops with an error naming the cause and the fix, never the threads driver | Andrew: no fallback; the driver is known when blaze is built |
| 10-29 | `&Db`, one concrete database, never `&dyn HirDatabase`; every other `dyn` on a hot path becomes a concrete type, a generic or an enum (lane dedyn) | static calls inline; a memo hit is a few loads |
| 10-29 | The front end's algorithms (name resolution, expansion, inference, method resolution, trait solving, const eval, MIR building, borrowck) may be redone completely wherever a faster design exists; the bar is rustc's behavior (same programs accepted, same meaning, same error codes and primary spans), proven by differential tests and fuzzing, not rust-analyzer's or rustc's code (lane frontend-fast) | Andrew: OK to completely redo things if there is a faster way |
| 10-29 | BLAKE3 is blake3-servil (johnservil/BLAKE3 `servil`, 0.8.0 on official 1.8.7) pinned at `c6bd679a0911cc233cb28297f2b25875381215a1`, behind `blaze_ids::b3`, the one module that names a BLAKE3 crate, on the targets its build script builds for (x86-64 Linux, AArch64 Linux and macOS); official `blake3` on the rest (Windows, x86-64 macOS), chosen by `cfg` as the I/O driver is. Kept only if the first compile shows identical output (`b3::tests`) and no shape of blaze's slower (`blaze-ids` bench `blake3` prints `verdict: adopt`); otherwise reverted by one line in `crates/blaze-ids/src/b3.rs` (`use servil as backend` to `use official as backend`) | Andrew: try BLAKE3-Servil, a fork with formally verified kernels (Lean spec, per-kernel proofs) and SME2 and integer + NEON kernels for AArch64; same hash function, so every hash, cache key and content ID stays the same bytes. Andrew chose the per-target split over official everywhere or porting the fork |
