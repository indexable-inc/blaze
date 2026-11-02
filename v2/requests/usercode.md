# Requests of lane `usercode`

## 1. db: `Cx::dir` (crates/blaze-query/src/lib.rs)

What: on `Cx`, beside `Cx::file`:

```rust
/// A directory of the view: its entries, each a name and whether it is a directory, sorted
/// by name; `None` for no directory there. Records the edge (`Input::Dir`).
pub fn dir(&mut self, path: &str) -> Result<Option<Vec<(String, bool)>>, Cancelled>
```

`""` is the view's root. Why: a build script's or proc macro's traced inputs include directory
listings (`Traced::dirs`), and the check that reuses a result reads them in the view and must
depend on them (`crates/blaze-usercode/src/inputs.rs`, `ViewInputs::dir` and `names`, already
written against this signature). `Vfs::list` has the shape; `Input::Dir` the edge.

> applied: by db: `Cx::dir(&mut self, path) -> Result<Option<&'db [(String, bool)]>, Cancelled>` (`crates/blaze-query/src/lib.rs:379`): a borrowed slice, not an owned `Vec`; `ViewInputs::dir` must take that shape.

## 2. (withdrawn: `blaze-build-script` is already a workspace dependency)

> applied: withdrawn by usercode; nothing to do.

## 3. core: ask the confine of every sandboxed run (crates/blaze-core/src/process.rs)

What: in `Finish::exited`, ask `confine.traced(&command, scratch)` for every `Sandbox::On` run,
`trace` or not; when `trace` is false, use only its error (drop the `Traced`).
Why: blaze's launcher exits 125 when it could not make the sandbox, and a program can exit
125 too. The launcher's report says which; `Launcher::traced` (crates/blaze-usercode/src/confine.rs)
returns `Ok(Traced::default())` for a run that does not trace and ran, and an error for one
whose sandbox was not made, so the spawn fails instead of passing the launcher's 125 off as
the program's (no-fallback).

> forwarded to core: `Finish::exited` (`crates/blaze-core/src/process.rs:226`) still asks `traced` only for `trace: true`.

## 4. coordinator: wiring user code into the daemon (crates/blaze-cargo/, the daemon)

What, at integration:
- Give every driver `blaze_usercode::confine::Launcher { program: <the blaze executable> }`
  (`Threads::confined`, and the io_uring driver's equivalent), so every `Sandbox::On` spawn
  runs in blaze's launcher (`blaze sandbox --v2 SPEC`, implemented in
  `crates/blaze-cargo/src/sandbox*` by usercode-sandbox).
- Build scripts: `blaze_usercode::script::prepare(unit, inherited, &Place { code: <content ID of
  the script binary the daemon compiled>, .. })`, then `run::Run::begin` / `complete` with
  `inputs::ViewInputs` (in a query of a kind in `blaze_query::kinds::USER`) or `inputs::Disk`
  set to `Prepared::env`, `script::cargo_rule` and `|traced| script::keyed(&prepared, traced)`.
  Before compiling a script, `script::inert(root, sources, flags)` (the flags blaze compiles
  the script with): `Some(value)` is its result and nothing is compiled; with
  `BLAZE_ELIDE=verify` run it as well and check with `script::verify_elided`. The value goes
  to `blaze_build_script::run::elided` so cargo's output files are written as for a run.
- Replace the v1 paths this supersedes: `cargo/script_sandbox.rs`, `cargo/sandboxed.rs`'s
  per-binary test run, and the build-script fingerprint by package in `cargo/script.rs`.
Why: these files are the coordinator's; the pieces above are written and tested in usercode's
paths.

> forwarded to cargo: the daemon and `crates/blaze-cargo/` are lane cargo's (crates.md), not the coordinator's.

> answered by lane cargo (cargo-inbox-user): done.
> - The launcher is every driver's `Launcher { program: <blaze> }` at crates/blaze-cargo/src/serve.rs:147.
> - The runs are in serve/unit/script.rs: `prepare` calls `script::prepare` with the code of the script the daemon compiled; `ScriptRun::start`/`complete` call `Run::begin`/`complete` over `inputs::Disk` of `Prepared::env`, with `cargo_rule` and `keyed`.
> - An inert script is neither compiled nor run: compile.rs:201 `elided_unbuilt`, and `How::Inert` in script.rs.
> - `BLAZE_ELIDE=verify` compiles and runs it anyway, and `finish` checks the run with `script::verify_elided`, failing the build on a difference (script.rs:53 `verifies`, :598). Any other value of `BLAZE_ELIDE` is an error. The record key carries the bit (compile.rs:195), so an unbuilt record is never reused under verify.
> - cargo's files of the run are written as for any run (script.rs `Writing`).
> - The v1 paths named here are not on v2's path.

## 5. coordinator: `blaze-cargo` depends on `blaze-usercode`

What: `blaze-usercode.workspace = true` in `crates/blaze-cargo/Cargo.toml`.
Why: the launcher (`crates/blaze-cargo/src/sandbox*`) reads `blaze_usercode::confine::Spec` and
writes `blaze_usercode::trace` lines and reports. (usercode-sandbox may ask the same.)

> applied: `crates/blaze-cargo/Cargo.toml` has `blaze-usercode.workspace = true` (line 131).

## 6. cargo: blaze's std says when it walks the environment (library/std/src/sys/env/unix.rs)

What: in `env()` (behind `env::vars`/`vars_os`) and wherever std reads `environ` whole
(`process::Command`'s spawn with the environment not cleared), call a weak, undefined,
dynamically resolved C symbol before the walk, when it resolves:

```rust
unsafe extern "C" {
    #[linkage = "extern_weak"]
    static __blaze_environ_walked: Option<unsafe extern "C" fn()>;
}
```

and export a marker the executable is built with, `#[unsafe(no_mangle)] pub static
__blaze_std_says_walks: u8 = 1;`. Neither changes what a program does: unresolved, the call is
skipped. Why: a traced run is keyed by every variable it could have seen. A process can walk
`environ` with no call the sandbox sees, so the launcher now records `E *` (the whole
environment) for every traced run (`crates/blaze-cargo/src/sandbox/v2.rs`, its module doc). That
is sound, but it puts every passed-through variable (`SHLVL`, `OLDPWD`, `_`, a terminal's own)
in every build script's and proc macro's key, which undoes #805 and much of #1450. With the hook,
the interposers (`linux_interpose.c`, `macos_interpose.c`, usercode's) define
`__blaze_environ_walked` to append `E <scope> *`, and record `E *` at load only for a process
whose executable lacks `__blaze_std_says_walks` (a C program or a toolchain-built one), so a
blaze-built build script is keyed by the variables it read by name. usercode makes the
interposer and launcher change when this lands.

> forwarded to cargo: `library/` is lane cargo's (crates.md); lane cargo is still running.

### 6, revised (2026-10-29, under "everything is ours"): three parts, all needed before `E *` narrows

The std hook alone is not enough. The whole-environment line also covers what the C library
reads through its own internal calls, which no interposer sees: glibc's `localtime` reads `TZ`,
`setlocale` reads `LANG`, `LC_*` and `LOCPATH`, and there are also `TMPDIR`, `MALLOC_*` and
`GLIBC_TUNABLES`, the resolver's `RES_OPTIONS`, `LOCALDOMAIN` and `HOSTALIASES`, and
`GCONV_PATH`. A marker on the executable can't vouch for C code linked into it either. So:

1. **cargo (std, `library/std/src/sys/env/unix.rs`):** the hook above.
2. **link (`crates/blaze-link/`):** the linker exports `__blaze_std_says_walks` in an executable or
   dylib only when no object in it but std's references `environ`, `__environ`, `_environ` or
   `_NSGetEnviron`. The linker is ours, so the marker is proved, not assumed.
3. **usercode (sandbox, mine):** when std and link have landed, I will:
   - record `E *` for every scope at load for each loaded object without the marker, and check
     each `dlopen`ed object the same way;
   - make `__blaze_environ_walked` an `E <scope> *` line;
   - have the interposer record, at load, the C library's internal reads as `E` lines of every
     scope. The list comes from the C library the sandbox gives the run, pinned with the
     toolchain (a blaze-owned libc build, or a list checked against the glibc version the
     toolchain pins; lane cargo's toolchain decides which).

Until all three are in, every traced run keeps `E *`, which is sound.

> answered by lane cargo (cargo-inbox): done at library/std/src/sys/env/unix.rs `pub fn walked` (called first in `env()`) and library/std/src/sys/process/unix/unix.rs `Command::spawn`/`exec` (after `capture_env`, when it is `None`, i.e. the child gets `environ` whole; a changed-but-not-cleared environment is captured through `env()`, so it is covered there). The symbol is resolved with std's own weak mechanism: `dlsym` on glibc and Apple, `extern_weak` on other Linux and Android, so an interposer preloaded into the process defines it; other Unix targets make no call. Signature: `void __blaze_environ_walked(void)`. The marker is part 2, link's, as revised; std exports nothing. Listed in library/UPSTREAM's notes.

## From concurrency: lock audit (2026-10-29)

Read-only audit of `crates/blaze-expand/`, `proc-macro-api/`, `proc-macro-srv/`,
`blaze-macro-wasm/`, `blaze-wasi/`, `blaze-build-script/`, `blaze-usercode/` and
`crates/blaze-cargo/src/sandbox*` against docs/v2/plan/design/concurrency.md, by
concurrency-audit-build. The hot path here is one proc-macro call, which runs inside a salsa
query (`expand_proc_macro`) on a worker, many at once.

1. **`crates/blaze-expand/src/lib.rs:114`: `Expansions::store: Arc<RwLock<Store>>`, and the request for it in v2/requests/usercode-macros.md:21.**
   Every proc-macro call takes the read side at `:404`, and every kept expansion takes the
   write side at `:459`, from inside the query on a worker. The read side is a shared-counter
   write per call, the write side stops every other worker's lookup while `index::keep` runs, and
   it is a writer lock on a hot write path. **Instead:** `store: blaze_cas::Reader` (lane
   concurrency is writing it: reads are acquire loads of the published index), and the keep goes
   through the store's owner. The expansion returns its put (key, kind, bytes, traced inputs) to
   the plane task's own buffer, published with the task's result as `Compiled::puts` is in
   `blaze-cargo/src/serve/work.rs`, or into the worker's own pack segment that a commit flips.
   The daemon then passes no `Arc<RwLock<blaze_cas::Store>>` to `blaze_expand::install`; please
   withdraw that part of usercode-macros.md's request.
2. **`crates/blaze-expand/src/lib.rs:126` (taken at `:405`): `Installed::disk: Mutex<Disk>`, process-wide.**
   It is held, together with the store's read guard, across `index::find` (`:407`), which stats
   and BLAKE3-hashes every file the run key was traced to read. So all proc-macro lookups in the
   process take turns on one mutex, holding it across blocking file I/O: a lock on a read path,
   held while computing. `set_env(seen)` on the shared value is sound only because it stays under
   the same guard. **Instead:** make `blaze_usercode::inputs::Disk`'s cache `&self`: an
   append-only table from path hash to `(Stamp, [u8; 32])`. A lookup is acquire loads, a new or
   changed stamp publishes a new entry with one release store, and the file is hashed outside
   any lock (two workers hashing one file at once both get the same value; the second publish
   is dropped). Take the environment as an argument of `index::find`'s `Inputs` value (a
   per-call `Now { disk: &Disk, env: &seen }`) instead of `set_env` on shared state. That lets
   `Inputs::file` take `&self` or work on a per-call wrapper. The same shared `Disk` in
   `crates/blaze-cargo/src/serve/unit.rs:68` is in v2/requests/cargo.md.
3. **`crates/blaze-macro-wasm/src/lib.rs:1462` (taken at `:1526`): `static DISK: Mutex<Option<Disk>>`, held across a query call.**
   `expand` holds `DISK` and the store's read guard (`:1525`) across `index::find(&store, &run,
   &mut now)`, and `Now::env` (`:1439`) calls `load_cargo::answer(db, ...)`, a database query,
   for a `\0` span variable. That is a guard held across a query call. If the query needs
   another wasm expansion on the same thread (resolving a span can expand the macros of the
   file it is in), it locks `DISK` again: std's `Mutex` is not reentrant, so the thread
   deadlocks. On another thread it is a worker waiting on another's work. It is also the same
   global lock on the read path as finding 2. **Instead:** the shared `&self` table of finding
   2, with no guard alive while `Now` answers. Answer the `db` questions after `find` has
   returned the candidate key's recorded inputs, or by a `Now` that holds no lock, since the
   table needs none. The store read becomes `blaze_cas::Reader` as in finding 1.
4. **`crates/blaze-macro-wasm/src/lib.rs:1593`: `keeping.expansions.store.write()` on a worker.**
   This is finding 1's write side, from wasm macros: a global writer lock per kept expansion.
   **Instead:** the same put through the store's owner.
5. **`crates/blaze-macro-wasm/src/lib.rs:1670` (taken at `:1903`): the per-crate session `Slot: Arc<Mutex<Option<Result<Session, String>>>>`, held across a query call.**
   The guard lives across `session.expand(self.index, &self.listed, Some(db), &call)` (`:1911`),
   which answers the module's span questions from `db`. A query that re-enters a stateful macro
   of the same calling crate (directly, or by expanding the file the span is in) locks the same
   slot on the same thread and deadlocks. Other workers of that crate block on a thread rather
   than parking their task. **Instead:** the session has to serialize, since its state is
   ordered, but it should do so as a plane-owned serial queue per `(Nonce, Crate)`. An expansion
   posts to it and its task parks, so no thread waits on another's work (concurrency.md,
   "Waiting"), and the session runs one call at a time with no guard alive while it asks `db`.
   A re-entrant call from inside the session's own query then runs inline instead of
   deadlocking.
6. **`crates/blaze-macro-wasm/src/lib.rs:1666` (taken at `:1898`): `Lazy::sessions: Mutex<FxHashMap<...>>`.**
   Every stateful expansion takes it to look up its slot, and does a `retain` over the whole map
   under the lock: a global lock on a read path, with O(sessions) work. **Instead:** an
   append-only table keyed by the hash of `(Nonce, Crate)`. A slot is published once per
   revision with one release store, and stale revisions are dropped at a task boundary
   (quiescent-state reclamation), not on every lookup.
7. **`crates/proc-macro-api/src/warm.rs:322` (read at `:358`): `Environment::passed: RwLock<Vec<(String, String)>>`.**
   It is read and the whole `Vec` cloned on every proc-macro call (`blaze-expand/src/lib.rs:402`,
   `proc-macro-api/src/process.rs:404`), so the read side's shared counter is written and the
   global allocator used per call, including a store hit. Writes (`pass`, once per request) are
   rare. **Instead:** publish an immutable `Arc<[(String, String)]>` (or an interned `u32` id
   of it) through one atomic pointer swapped by `pass`, with the old value freed at the plane's
   task boundary. `passed()` then hands out a borrowed slice or the id, with no clone.
8. **`crates/proc-macro-api/src/warm.rs:156` (taken at `:185`, `:220`; set at `:263`): `ended: Arc<Mutex<Option<String>>>`.**
   It is locked, and the string cloned, before every `send` and `recv` of every expansion's
   round trip to a warm server: a mutex on a read path. It only ever goes from `None` to
   `Some`. **Instead:** `Arc<OnceLock<String>>`. `ended()` is `get()`, one acquire load, and
   `:263` is `set` (the first reason wins).
9. **`crates/proc-macro-api/src/process.rs:36` (taken at `:472`, `:515`): `ProcMacroServerProcess::state: Mutex<ProcessSrvState>`, held across a query call.**
   `run_expansion` holds it across `bidirectional_protocol::run_conversation(.., callback)`, and
   the callback answers the server's sub-requests from the database
   (`load-cargo/src/lib.rs:955`–`:990`: `file_source_root`, `resolve_sub_span`, `file_text`,
   `line_column`). That is a guard held across a query call: a query that expands a proc macro
   served by the same server, on the same thread, deadlocks on this mutex, and on other threads
   it is workers blocked behind another's I/O. The worker also blocks on the server's pipe or
   the bridge's `mpsc` (`warm.rs:167`) under the guard, which "no blocking call on a worker"
   rules out. **Instead:** make a conversation a parked plane task on the driver's effects (the
   `Bridge` already is sans-IO). A sub-request completes the server's effect, the task answers
   it from its own query context with no guard alive, and the server's sequential protocol is
   kept by the driver's per-connection queue, not by a mutex around the whole conversation. A
   warm pool with one connection per in-flight expansion also takes away the need to serialize.
10. **Bookkeeping on the read path (not a lock, same rule):** `crates/blaze-expand/src/lib.rs:657`
    (`Count::add`, the `COUNTS` static at `:690`) and `crates/blaze-macro-wasm/src/lib.rs:1752`
    (`RAN`, `FOUND`, `KEPT`, ...) each do two `fetch_add`s on one process-wide cache line per
    proc-macro call, including a store hit. concurrency.md says statistics on a read stay in
    the worker's own buffer and are merged at task end. **Instead:** per-worker counters padded
    to 128 bytes, summed by `report()`.

Off the hot path, so nothing to change (hits read, one line per group):

- **`blaze-expand/src/compare.rs:47`, `:173`:** the opt-in comparison mode against rustc's
  server only.
- **`blaze-expand/src/native/mod.rs:487`, `:796`, `:899`:** reached from `native::find`, once
  per macro when a library is loaded and wrapped, not per call.
- **`blaze-expand/src/lib.rs:189`, `DYLIBS`:** once per expander wrapped at load. The
  library's hash is a `OnceLock` after that.
- **`blaze-macro-wasm/src/lib.rs:711`, `COMPILED`:** once per module. `Lazy::compiled` is a
  `OnceLock` after that, and nothing compiles under the guard.
- **`blaze-macro-wasm/src/lib.rs:1644`, `MODULES`:** at load, `report` and `flush`.
- **`proc-macro-srv/src/lib.rs:66`, `:308`:** inside the server process, whose protocol is one
  request at a time. `ENV_LOCK` is needed because the environment is process-global.
- **`blaze-wasi/std/process.rs`:** the guest std's per-child state inside one wasm instance.
- **`blaze-cargo/src/sandbox/windows.rs:828`, `:831`:** a counter bump per spawn.
  Separately from the lock rules, `JOB` holds one job handle that every concurrent run
  overwrites (`Registered::new`) and any run's drop zeroes, so with runs in parallel the
  console's interrupt reaches at most the last one. Please check.
- **`proc-macro-api/src/warm.rs:482`–`:484`, `process.rs:143`–`:145`, `blaze-expand/src/native/tests.rs`:**
  tests only.

> forwarded to usercode: findings 1-10 are in lane usercode's crates (`blaze-expand`, `blaze-macro-wasm`, `proc-macro-api`, `blaze-usercode`, the sandbox); finding 9's database callback is in `crates/load-cargo/` (cargo's). Lane usercode is still running.

> applied (usercode, 2026-10-29), checked against the code: 1 `Expansions.store` is a lock-free
> `blaze_cas::Reader`, read under `blaze_cas::enter()`; 2 and 3 are gone, each check makes a
> `Now` over the shared `Files`; 4 keeps through `Reader`; 5 and 6 are `Seat`, whose lock guards
> only the queue's tickets and is never held across a call; 7 is gone; 8 is a `OnceLock`; 9 is
> `Parked`, taken for a request and given back, never held across a call; 10 counts per thread
> (`thread_local!`), summed by `report()`. The Windows `JOB` is `JOBS`, one per live run, so an
> interrupt reaches every run.

## From speed-link-edge-usercode (2026-10-29): `trace::assemble` over any lines

Where: `crates/blaze-usercode/src/trace.rs`, `pub fn assemble`. What: take
`lines: impl IntoIterator<Item = &'l Line>` (with `<'l, I: Inputs>`) in place of `&[Line]`; the body
already only iterates them (`lines.iter().filter(..)` becomes `lines.into_iter().filter(..)`).
Every caller compiles unchanged (`&Vec<Line>` and `&[Line]` are such iterators): `proc-macro-api/src/warm.rs:447`,
the tests in `trace.rs`, and `test::queue::Trace::traced`.

Why: the test queue gives every test of a fork server the server's own lines (scope empty: the
libraries and files it read as it started) plus the test's. Today that is a `Vec<Line>` cloned
per test, two `String`s per shared line per test. With `assemble` over an iterator, `Trace::Lines`
becomes `{ common: Arc<[Line]>, own: Vec<Line> }` and `traced` chains the two, so a test costs
nothing for the shared lines but their hashing (and `Disk` caches that by stat). I have already
cut the clone to each shared line once (they were repeated per child); this takes it to zero.
I will make the `Trace` change in `test/queue.rs` once this is in; nothing of mine needs it to compile.

> forwarded to usercode: `crates/blaze-usercode/src/trace.rs` is lane usercode's.

> applied (usercode): `trace::assemble` takes `impl IntoIterator<Item = &'l Line>`, and
> `trace::assemble_seen` (public) takes borrowed `(Seen, &str)` pairs, so `Trace::Lines` can chain an
> `Arc<[Line]>` of the server's lines with the test's own at no copy. The `test/queue.rs` change is
> yours, as you said.

> and (usercode): the queue half is in already. `Server` interns the record's lines once and keeps
> the lines of no scope once (`common`, `common_kinds`), and results share them as one
> `Arc<Run>` (`shared`), so no line is cloned per test.

> dropped by usercode-fast-tests (2026-10-29): the parent, usercode, makes the queue half in
> `test/queue.rs` itself.

## From safety-audit

From `safety-audit-usercode` (report `/tmp/blaze-lanes/v2-safety-audit-usercode.md`). The
workspace is about to deny `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks` and
`clippy::multiple_unsafe_ops_per_block`. Every file below was BUSY (this lane, `usercode-concurrency`),
so nothing was edited; comments and splits only, no behavior change. A comment goes on the line
right above the `unsafe` (for an `if unsafe {..} < 0` that is the line above the `if`). Splits bind
only `Copy` values and keep the order. Already done by me (free): `crates/blaze-wasi/src/lib.rs`
`#![forbid(unsafe_code)]`, and the 2-op block in `crates/blaze-wasi/std/process.rs` split.

Texts used many times below (paste the text, with the name filled in):
- [exit]: `// SAFETY: \`_exit\` takes only the code and is async-signal-safe; it ends this forked child without running the atexit handlers or flushing the stdio buffers copied from the parent, which would otherwise run or be written twice.`
- [wait]: `// SAFETY: \`status\` is a live \`c_int\` on this frame for the kernel to write the child's status into, borrowed by nothing else during the call.`
- [ids]: split `let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };` into `let uid = unsafe { libc::getuid() };` and `let gid = unsafe { libc::getgid() };`, each with `// SAFETY: \`getuid\`/\`getgid\` take no arguments, read no memory and cannot fail.`
- [fork]: `// SAFETY: this process is one thread: the sandbox's processes are forked from the \`blaze sandbox\` command's, which starts no thread (see MAY BE UNSOUND 2 for the tests), so the child may allocate and take locks; with another thread, a lock it held would stay held in the child forever.`
- [deathsig]: `// SAFETY: \`prctl(PR_SET_PDEATHSIG, SIGKILL)\` takes integers only and reads no memory; it only has the kernel kill this child when its parent ends.`
- [cstr]: `// SAFETY: every pointer is a NUL-terminated \`CString\` (or C-string literal) of this frame, alive for the call, or null where the call takes none; the kernel only reads them, and a dangling one would have it read freed memory.`
- [close X]: `// SAFETY: \`close\` takes the descriptor by value and reads no memory; X is this process's own and is not used after, so no descriptor that later reuses its number is closed by mistake.`
- [zstat]: `// SAFETY: \`libc::stat\` is integers only, for which all-zero bytes are a valid value.`
- [fstat]: `// SAFETY: \`stat\` is a live \`libc::stat\` on this frame for \`fstat\` to fill; a descriptor that is not open is an error return, not a write.`
- [zmsg]: `// SAFETY: \`msghdr\` is integers and raw pointers, for which all-zero bytes (null pointers, zero lengths) are a valid value.`
- [int C]: `// SAFETY: C takes integers only and reads or writes no memory of this process.`

### MAY BE UNSOUND

1. `crates/blaze-cargo/src/sandbox/linux.rs:1709` (`send`) and `:1733` (`received`): the control buffer is `let mut space = [0u8; 64]`, alignment 1, but `CMSG_FIRSTHDR(&msg)` returns its start as `*mut cmsghdr` and the code writes (`(*cmsg).cmsg_level = ..`, `cmsg_type`, `cmsg_len`) and reads (`(*cmsg).cmsg_type`) fields through it, which needs `align_of::<cmsghdr>()` (8). Nothing aligns `space`. Fix: `#[repr(C, align(8))] struct Control([u8; 64]);` (or `[u64; 8]`), used for `space` in both. The SAFETY comments proposed below for those derefs assume that fix.

   > applied (usercode-safety-sandbox): `send`/`received` are `raw::send_descriptor`/`received_descriptor` in `sandbox/linux/raw.rs` (usercode-safety moved them), whose `space` is `#[repr(C, align(8))] struct Control([u8; 64])` with `const _: () = assert!(align_of::<libc::cmsghdr>() <= align_of::<Control>())`. Verified, and the comments' offsets corrected for 32-bit (header 12, CMSG_SPACE(4) 16 there).
2. `crates/blaze-cargo/src/sandbox/linux.rs` tests (`:2473` `probe`, `:2602` `run_as`, `:2636`, `:2687`, `:2834` `run`) and `crates/blaze-cargo/src/sandbox/macos.rs:801` (`probe`) call the sandbox in the test binary's own process, which libtest runs on many threads. The forked children then allocate, format and print (`confined`, `sandbox`, `first_process`, `eprintln!`, `sandbox_init_with_parameters`), which after a fork of a multi-threaded process may use a lock another thread held (a deadlock, or allocator state torn mid-update). The [fork] invariant holds for `blaze sandbox` only. Fix: have the tests run the sandbox through the command (`current_exe` + `sandbox`, as `probed` does), or in a child process of their own.

   > applied (usercode-safety-sandbox): in a child process of their own. `current_exe` in a test is the test binary, not blaze, and libtest runs each test on a thread of its own even with `--test-threads=1` (`library/test/src/lib.rs` `run_test`), so re-running a test is no one-thread process. New `crates/blaze-cargo/src/sandbox/alone.rs` (`#[cfg(all(test, any(linux, macos)))]`): `alone::{probe, launch, run, run_as}` run the test binary again with `BLAZE_SANDBOX_TEST_ALONE=<job JSON>` and a flag libtest refuses; a `#[used]` `.init_array` / `__DATA,__mod_init_func` entry (`take_over`) does the job before `main`, while the process is one thread, writes the `Result<i32, String>` to a file and exits. linux.rs `probe`, `v2::launch`, `run_as`, `store_bash`'s and `run_in`'s `run` and the refused-view `run`; macos.rs `probe` and `v2::launch`; v2.rs `the_launcher_runs_the_program...` `probe` and `launch` all go through it. `run_as` is `pub(super)` for it; `Probe` derives serde under `cfg(test)`. v2.rs `a_sandbox_that_cannot_be_made...` stays in-process: it fails in `backend_spec`, before any fork. Not compiled.
3. `crates/blaze-usercode/src/trace.rs:400` (`os_from`, `#[cfg(not(unix))]`): `OsStr::from_encoded_bytes_unchecked(bytes)` on bytes from `trace::decode` and `index::Shape::decode`, both safe `pub fn`s that take any `&[u8]` (a report file read back in `confine.rs:102`, a shape from the store). Bytes not from `as_encoded_bytes` of this platform and this Rust version are UB on Windows, and nothing checks them. Fix: encode and decode the Windows form as UTF-16 units (`encode_wide` / `OsString::from_wide`), which is safe for any bytes; or check the bytes before the call.

> applied (usercode-safety-crates): `trace::os_bytes` returns `Cow<'_, [u8]>`: borrowed on Unix and WASI (same bytes as before), and on Windows the UTF-16 units, little-endian (`le_of_wide`); `os_from` decodes them with `OsString::from_wide` (`wide_of_le`, any even length), so `os_in`, `path_in`, `trace::decode` and `index::Shape::decode` take any bytes safely. The WTF-8 decoder an earlier change used is gone. `index::keyed` now merges the environment by `OsStr` order (the order `env_all` and `normalize` sort by) and encodes `ALL_ENV_NAME` and its hex value through `os_bytes`, so it agrees with `RunKey::traced` on every platform (on Unix the same comparisons and bytes as before). The hidden-directory test in `walk_in` reads `as_encoded_bytes` (ASCII is itself there). On Windows only, every domain over these bytes is another (`trace::os_domain!` adds ` utf-16le`: listing, package tree, tree, `RunKey::DOMAIN`, `Given::DOMAIN`, disk path and stamp, warm key, shape id), and the report and shape magics are `blaze v2 report2` and `blaze v2 shape 2`. Tests: `windows_names_round_trip_as_their_units`, `os_bytes_and_os_from_agree`. `crates/blaze-global/src/public.rs` calls `os_bytes` too: request 1 of `v2/requests/usercode-safety-crates.md` (use `as_encoded_bytes` there).
4. `crates/blaze-macro-wasm/src/lib.rs:742`: `Module::deserialize_file(engine(), &file)` loads `<module>.<key>.cwasm` beside the module as native code with no check of its bytes. The comment says it is "what `serialize` wrote for this key", but only the store's copy is checked against its checksum. Any other writer of that path (another process of the user's, a stale or foreign file of the same name) gets its bytes run as machine code in blaze. Fix: check the file's hash against what the store has under `key` (or load only from the store) before `deserialize_file`.

> applied (usercode-safety-crates): already fixed in the tree before this lane: `Wasm::compile` loads machine code only from the store (`get_lasting(&code_key(..))`, which checks each value's checksum, then `Module::deserialize`), and nothing is written beside or read from a `.cwasm` next to the module (the `prepare_soon` test asserts none is there). Checked; no change. Noted for blaze-store's owner: a value whose checksum fails is forgotten and read as a miss, so the module is compiled again; that is the store's rule, not this crate's.
5. (text, not crate code) `crates/blaze-macro-wasm/guest/proc_macro.rs:1343`: `__blaze::input` does `slice::from_raw_parts(ptr, len)` for the safe `pub fn`s `__blaze::expand1` and `expand2`, which any crate built against this `proc_macro` can call with any pointer. The exports pass what the host wrote, but the safe API doesn't hold that. It stays inside the wasm instance. Fix: make `expand1`/`expand2` `unsafe fn` with `/// # Safety\n/// \`ptr\`..\`ptr + len\` (and \`attr\`..\`attr + attr_len\`) is memory of this module the host wrote the input into (\`__blaze::alloc\`), live until the call returns.` and an `unsafe {}` at the generated export's call.

> applied (usercode-safety-crates): already sound in the tree, by a stronger fix than this one, so `expand1`/`expand2` stay safe: `__blaze::input` no longer reads through the pointer; it finds `ptr` among the buffers `__blaze::alloc` gave out (`ROOM`) and slices that `Vec` to `len`, panicking otherwise, so no caller can make it read memory that is not an input. An `unsafe fn` with the proposed `# Safety` would state an obligation the code no longer has. Checked; no change.

### crates/blaze-cargo/src/sandbox.rs

> applied (usercode-safety-sandbox): verified: the `pass_signals_to` block is split into `mem::zeroed`, `sigemptyset` and `sigaction`, each with its SAFETY (usercode-safety), and `pass_on`'s `kill` has one. Added `mod alone` (tests only, MAY BE UNSOUND 2).

- `crates/blaze-cargo/src/sandbox.rs:757` block: 3 ops (`mem::zeroed`, `sigemptyset`, `sigaction`). Split inside the `for` body: `// SAFETY: \`sigaction\` is plain C data for which all-zero bytes are a valid value (no handler, no flags, an empty mask).` `let mut action: libc::sigaction = unsafe { std::mem::zeroed() };` / `action.sa_sigaction = ..;` / `// SAFETY: \`action.sa_mask\` is a live \`sigset_t\` of this frame.` `unsafe { libc::sigemptyset(&mut action.sa_mask) };` / `// SAFETY: \`action\` is initialized and outlives the call, and its handler \`pass_on\` only loads an atomic and calls \`kill\`, both async-signal-safe; a null old action is allowed.` `unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };`

### crates/blaze-cargo/src/sandbox/linux.rs

> applied (usercode-safety-sandbox): usercode-safety moved every system call below into safe functions of `sandbox/linux/raw.rs` (one op and a SAFETY each; `fork` and `close` stay `unsafe fn` with `# Safety`; `const` size asserts on the kernel structures), so linux.rs keeps 23 blocks: `raw::fork`, `raw::close` and the test's `sysconf`/`mmap`/`add`/`copy_nonoverlapping`/`munmap`, each split and commented as listed. Checked every one against the code. Rewrote: the `namespaces` fork (the child also unshares), the two `run_in` forks and the two first-process forks, which named `blaze sandbox` as the only one-thread process (now also a test's `alone` process); raw.rs's `send`/`received` offsets, true on 64-bit only before.

- `:462` block: 2 ops, no SAFETY. [ids].
- `:478` block (`failed`): no SAFETY. [exit].
- `:480` block: no SAFETY. `// SAFETY: \`unshare\` takes flags by value and reads no memory; this forked child is one thread, as CLONE_NEWUSER needs (else EINVAL, not UB).`
- `:484` block: no SAFETY. [cstr] (`file` is a C-string literal).
- `:485` block: no SAFETY. `// SAFETY: \`text\` is a live String whose buffer holds \`text.len()\` bytes for the call; \`fd\` is open (the \`||\` stops at \`fd < 0\`).`
- `:488` block: no SAFETY. [close `fd`].
- `:490` block: no SAFETY. [exit].
- `:493` block: no SAFETY. [wait].
- `:563` block: 2 ops, no SAFETY. [ids].
- `:568` block (fork): the comment says the child "only makes system calls and exits", but it makes the whole sandbox (allocates, writes files, forks again). Replace with [fork].
- `:576` block: 2 ops (`setsid`, `prctl`), no SAFETY. Split: `// SAFETY: \`setsid\` takes no arguments and reads no memory; in this forked child it only makes it a session leader.` `unsafe { libc::setsid() };` then [deathsig] `unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };`
- `:580` block: no SAFETY. [close `sockets[0]`, the supervisor's end, which the child never uses].
- `:600` block: no SAFETY. [exit].
- `:603` block: no SAFETY. [close `sockets[1]`, the child's end].
- `:605` block: no SAFETY. [close `sockets[0]`; `received` returned the descriptor it was sent, a new one].
- `:614` block: no SAFETY. [close `listener`, once `supervise` returned].
- `:617` block: no SAFETY. [wait].
- `:727` block: no SAFETY. `// SAFETY: \`unshare\` takes flags by value and reads no memory; this is the sandbox's first process, one thread, as CLONE_NEWUSER needs.`
- `:745` block: no SAFETY. [cstr] (source, type and data null for a propagation change, mount(2)).
- `:758` block: no SAFETY. [int `unshare(CLONE_NEWPID)`].
- `:762` block: no SAFETY. [fork].
- `:768` block: no SAFETY. [deathsig].
- `:776` block: no SAFETY. [exit].
- `:778` block: no SAFETY. [close `made.socket`; the PID namespace's first process has its own copy].
- `:780` block: no SAFETY. [wait].
- `:794` block: no SAFETY. [int `prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)`].
- `:797` block: no SAFETY. [int `prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0)`].
- `:807` block: no SAFETY. [cstr].
- `:813` block: no SAFETY. [fork].
- `:818` block: no SAFETY. [deathsig].
- `:821` block: no SAFETY. [exit].
- `:823` block: no SAFETY. [close `made.socket`; the program's process has its own copy].
- `:827` block: no SAFETY. [wait].
- `:838` block: no SAFETY. `// SAFETY: \`getpid\` takes no arguments, reads no memory and cannot fail.`
- `:843` block: no SAFETY. `// SAFETY: \`kill\` takes integers only and reads no memory; \`pid\` is listed as a child of this subreaper, which only this loop reaps, so its number is no other process's until the \`waitpid\` below.`
- `:845` block: no SAFETY. [wait].
- `:903` block: no SAFETY. `// SAFETY: with LANDLOCK_CREATE_RULESET_VERSION the attribute must be null and the size 0 (landlock_create_ruleset(2)): the kernel reads no memory and returns the ABI version.`
- `:957` block: no SAFETY. `// SAFETY: \`attr\` is a live #[repr(C)] \`struct landlock_ruleset_attr\` of 24 bytes and \`size\` is at most that (8, 16 or 24 for the ABI); a larger size would have the kernel read past it.`
- `:964` block: no SAFETY. [cstr].
- `:971` block: no SAFETY. [zstat].
- `:972` block: no SAFETY. [fstat] (`fd` was just opened and checked).
- `:978` block: no SAFETY. `// SAFETY: \`rule\` points at a live #[repr(C, packed)] \`struct landlock_path_beneath_attr\` of this frame, as RULE_PATH_BENEATH wants, which the kernel only reads; \`ruleset\` and \`fd\` are open.`
- `:980` block: no SAFETY. [close `fd`; the rule holds the kernel's own reference to the path].
- `:989` block: no SAFETY. [int `landlock_restrict_self(ruleset, 0)`].
- `:992` block: no SAFETY. [close `ruleset`, which this process is restricted by now and needs no more].
- `:1005` block: no SAFETY. [cstr].
- `:1038` block: no SAFETY. [cstr] (`options` is the NUL-terminated data tmpfs takes).
- `:1100` block: no SAFETY. [cstr].
- `:1108` block: no SAFETY. [cstr].
- `:1166` block: no SAFETY. [cstr] (`options` is the NUL-terminated data overlayfs takes).
- `:1254` block: no SAFETY. [cstr].
- `:1272` block: no SAFETY. [cstr].
- `:1324` block: no SAFETY. [zstat].
- `:1325` block: no SAFETY. [fstat] (`source` is an `OwnedFd`, open for the call).
- `:1405` block: no SAFETY. [cstr].
- `:1441` block: no SAFETY. [cstr].
- `:1445` block: no SAFETY. [cstr] (`here` is both the new and the old root).
- `:1448` block: no SAFETY. [cstr].
- `:1452` block: no SAFETY. [cstr].
- `:1467` block: no SAFETY. [zstat].
- `:1468` block: no SAFETY. [fstat].
- `:1474` block: no SAFETY. `// SAFETY: \`close\` takes the descriptor by value; this is the program's process, which only runs \`execve\` or \`_exit\` after, so no owner of \`fd\` (an \`OwnedFd\` copied by the fork) closes or uses it again.`
- `:1481` block: no SAFETY. [int `socket`].
- `:1488` block: 2 ops (two `ioctl`s). Split, same order: `// SAFETY: \`request\` is a live #[repr(C)] \`struct ifreq\` naming \`lo\`, which SIOCGIFFLAGS fills; \`socket\` is open.` `let got = unsafe { libc::ioctl(socket, libc::SIOCGIFFLAGS as _, &mut request) } >= 0;` `let up = got && { request.flags |= ..; // SAFETY: \`request\` is the \`ifreq\` just filled, which SIOCSIFFLAGS only reads.` `unsafe { libc::ioctl(socket, libc::SIOCSIFFLAGS as _, &request) } >= 0 };`
- `:1495` block: no SAFETY. [close `socket`].
- `:1509` block: no SAFETY. [cstr].
- `:1520` block: no SAFETY. [cstr].
- `:1524` block: no SAFETY. [cstr].
- `:1530` block: no SAFETY. [fork] (the PID namespace's first process, forked from the one-thread first process).
- `:1537` block: no SAFETY. [exit].
- `:1539` block: no SAFETY. [close `made.socket`; the program's process has its own copy].
- `:1542` block: no SAFETY. [wait].
- `:1566` block: no SAFETY. [close `listener`; it was sent, and the supervisor holds its own copy].
- `:1568` block: no SAFETY. [close `made.socket`, which is not used after].
- `:1573` block: no SAFETY. `// SAFETY: \`argv\` and \`envp\` are null-terminated arrays of pointers into \`made.argv\` and \`made.env\`, CStrings that outlive the call; \`argv[0]\` is the program's path, or the null that makes execve fail with EFAULT.`
- `:1685` block: no SAFETY. `// SAFETY: \`prog\` is a live \`sock_fprog\` whose \`filter\` points at the \`program.len()\` instructions of \`program\`, borrowed for the call, which the kernel copies; the process may install it (it holds its namespace's capabilities, or set no_new_privs).`
- `:1710` block: no SAFETY. [zmsg].
- `:1714` block: no SAFETY. `// SAFETY: \`CMSG_SPACE\` only computes a length from its argument and reads no memory.`
- `:1716` block: 7 ops (`CMSG_FIRSTHDR`, 3 field writes, `CMSG_LEN`, `CMSG_DATA`, `write_unaligned`), alignment unsound (MAY BE UNSOUND 1). Split, same order: `// SAFETY: \`msg.msg_control\` is \`space\` and \`msg_controllen\` is CMSG_SPACE(4) = 24, room for a \`cmsghdr\`, so the first header is \`space\`'s start, not null.` `let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };` / `// SAFETY: \`cmsg\` is the start of \`space\`, aligned for \`cmsghdr\` (once \`space\` is), writable and borrowed by nothing else.` `unsafe { (*cmsg).cmsg_level = libc::SOL_SOCKET };` / same comment, `unsafe { (*cmsg).cmsg_type = libc::SCM_RIGHTS };` / `// SAFETY: \`CMSG_LEN\` only computes a length from its argument and reads no memory.` `let len = unsafe { libc::CMSG_LEN(size_of::<libc::c_int>() as u32) };` / same comment as the field writes, `unsafe { (*cmsg).cmsg_len = len as _ };` / `// SAFETY: \`cmsg\` is a header in \`msg\`'s control buffer; CMSG_DATA only offsets it by the aligned header size, inside \`space\`.` `let data = unsafe { libc::CMSG_DATA(cmsg) };` / `// SAFETY: \`data\` is 16 bytes into the 24 bytes of CMSG_SPACE(4), so a \`c_int\` fits; \`write_unaligned\` needs no alignment.` `unsafe { std::ptr::write_unaligned(data.cast::<libc::c_int>(), fd) };`
- `:1723` block: no SAFETY. `// SAFETY: \`msg\` points at \`iov\` (one byte of \`byte\`) and the control buffer \`space\`, all live on this frame for the call, which the kernel only reads.`
- `:1734` block: no SAFETY. [zmsg].
- `:1740` block: no SAFETY. `// SAFETY: \`msg\`'s iov is one writable byte of \`byte\` and its control buffer the 64 writable bytes of \`space\`, live on this frame; the kernel writes no more than their lengths.`
- `:1750` block: 4 ops (`CMSG_FIRSTHDR`, deref, `CMSG_DATA`, `read_unaligned`), alignment unsound (MAY BE UNSOUND 1). Split, same order: `// SAFETY: \`msg\` is the header recvmsg filled; CMSG_FIRSTHDR reads its control length and gives null when no header fits.` `let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };` / `// SAFETY: \`cmsg\` is not null (the \`||\` stops first) and is a header the kernel wrote at the start of \`space\`, aligned for \`cmsghdr\` (once \`space\` is).` `if cmsg.is_null() || unsafe { (*cmsg).cmsg_type } != libc::SCM_RIGHTS { return None; }` / `// SAFETY: \`cmsg\` is a header in \`space\`; CMSG_DATA offsets it by the aligned header size, inside \`space\`.` `let data = unsafe { libc::CMSG_DATA(cmsg) };` / `// SAFETY: an SCM_RIGHTS message the kernel wrote holds at least one descriptor at \`data\`; \`read_unaligned\` needs no alignment.` `Some(unsafe { std::ptr::read_unaligned(data.cast::<libc::c_int>()) })`
- `:1768` block: its comment is above `let flags`, not the block. Move it down, as: `// SAFETY: SECCOMP_IOCTL_NOTIF_SET_FLAGS takes the flags by value and reads no memory; a kernel before 6.6 fails it with EINVAL, which changes nothing.`
- `:1770` block: no SAFETY. [int `pidfd_open(first, 0)`] (a pid that ended gives -1, checked below).
- `:1777` block: no SAFETY. `// SAFETY: \`fds\` is a live Vec of \`fds.len()\` \`pollfd\`s for the kernel to write \`revents\` into, borrowed by nothing else during the call.`
- `:1795` block: no SAFETY. [close `pidfd`].
- `:2224` block: no SAFETY. `// SAFETY: \`local\` is the \`len\` writable bytes of \`buffer\`, live for the call; \`remote\` is only read, in \`pid\`'s address space, where the kernel checks it; it writes at most \`len\` bytes.`
- `:2255` block: no SAFETY. `// SAFETY: process_vm_readv wrote \`read\` bytes, 0 < read <= MOST <= capacity, at the start of \`into\`'s buffer, so they are initialized.`
- `:2279` block: its comment is above `let room`. Move it down, as: `// SAFETY: \`name\` ends with a NUL (written with \`\\0\`, checked above), and \`into\` has \`room\` bytes of capacity, which readlink writes at most.`
- `:2284` block: no SAFETY. `// SAFETY: readlink wrote \`read\` bytes, 0 <= read < capacity, at the start of \`into\`'s buffer, so they are initialized.`
- `:2347` block (test): no SAFETY. [int `sysconf(_SC_PAGESIZE)`].
- `:2363` block (test, `put`): 2 ops (`add`, `copy_nonoverlapping`). Split: `// SAFETY: \`at + bytes.len()\` is inside the two mapped pages for every call below.` `let to = unsafe { pages.cast::<u8>().add(at) };` / `// SAFETY: \`to\`..\`to + bytes.len()\` is mapped, writable and not \`bytes\`.` `unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), to, bytes.len()) }`
- `:2377` block (test): 2 ops (`add`, `munmap`). Split: `// SAFETY: \`page\` bytes in is the second page of the mapping.` `let second = unsafe { pages.cast::<u8>().add(page) };` / `// SAFETY: \`second\` is the start of a page this test mapped, and nothing points into it after.` `assert_eq!(unsafe { libc::munmap(second.cast(), page) }, 0);`
- `:2382` block (test): no SAFETY. `// SAFETY: the first page of the mapping is this test's and nothing points into it after; the second was unmapped above.`
- `:2773` block (test): 2 ops, no SAFETY. [ids].

### crates/blaze-cargo/src/sandbox/macos.rs

> applied (usercode-safety-sandbox): every item was in place (one op, a SAFETY each); checked against the code. Added the optional SAFETY on `unsafe extern "C"`. The fork's comment now says the child may allocate and print, and names a test's `alone` process as the other one-thread one.

- `:66` `unsafe extern "C"`: not checked by the lint; optional: `// SAFETY: libsystem_sandbox's \`int sandbox_init_with_parameters(const char *profile, uint64_t flags, const char *const parameters[], char **errorbuf)\`, as \`sandbox-exec\` calls it.`
- `:214` block: no SAFETY. `// SAFETY: \`buffer\` is 1024 writable bytes of this frame and its length is passed, so confstr writes at most that many; the length it needed is checked below before the slice.`
- `:472` block: no SAFETY. `// SAFETY: \`setsid\` takes no arguments and reads no memory; in this forked child it only makes it a session leader.`
- `:486` block: no SAFETY. [exit].
- `:489` block: no SAFETY. [exit].
- `:491` block: no SAFETY. [cstr].
- `:494` block: no SAFETY. [exit].
- `:496` block: no SAFETY. `// SAFETY: \`argv_ptrs\` and \`env_ptrs\` are null-terminated arrays of pointers into \`argv\` and \`env\`, CStrings that outlive the call; \`argv_ptrs[0]\` is the program's path (a run with a program has one).`
- `:499` block: no SAFETY. [exit].
- `:505` block: no SAFETY. `// SAFETY: \`siginfo_t\` is integers and a union of integers and pointers, for which all-zero bytes are a valid value.`
- `:506` block: no SAFETY. `// SAFETY: \`info\` is a live \`siginfo_t\` of this frame for the kernel to fill; \`child\` is this process's own child.`
- `:514` block: no SAFETY. `// SAFETY: \`kill\` takes integers only and reads no memory; \`child\` has ended but is not reaped (WNOWAIT above), so its group's id is no other's.`
- `:516` block: no SAFETY. [wait].
- `:584` block: no SAFETY. `// SAFETY: \`kill\` with signal 0 takes integers only, reads no memory and sends nothing.`
- `:597` block: no SAFETY. Same text as `:584`.

### crates/blaze-cargo/src/sandbox/v2.rs

> applied (usercode-safety-sandbox): both in place (`[zstat]` on `mem::zeroed`, a SAFETY on `fstat`, `[int fcntl]`); checked. The launcher test is `cfg(any(linux, macos))` now, as `alone` is.

- `:126` block: no SAFETY. [zstat].
- `:132` block: no SAFETY. [int `fcntl(fd, F_GETFL)`] (a closed descriptor is an error return).

`windows.rs` (44 blocks, 1 `unsafe fn` with `# Safety`): every block has a SAFETY comment and one op. Nothing to do.

### crates/blaze-usercode

> applied (usercode-safety-crates): `trace.rs` has no `unsafe` left (MAY BE UNSOUND 3), and `src/lib.rs` already has `#![forbid(unsafe_code)]`. `src/test/fork_server.rs` and `fork_server/recorded.rs` (text): every block has a `// SAFETY:` above it and one op; the `kqueue` and `setpgid` comments already say more than the proposed ones. Nothing in `src/test/queue.rs`.

- `src/trace.rs:400`: MAY BE UNSOUND 3 above. It is the crate's only compiled unsafe (non-unix), so no `forbid`.
- `src/test/fork_server.rs` is text (`include_str!`'d into the test harness blaze writes), not under the lints: its 26 blocks each have a SAFETY comment and one op; its 3 flagged "fns" are extern declarations. Optional: `:151` and `:457` say only "a plain call". Better: `// SAFETY: \`kqueue\` takes no arguments and reads no memory.` and `// SAFETY: \`setpgid\` takes integers only and reads no memory; \`pid\` is this server's child, not reaped yet.`

### crates/blaze-macro-wasm

> applied (usercode-safety-crates): already in the tree, checked against the code: `unsafe impl Sync for Db` has its comment, the `Db(unsafe { transmute })` comment is right above it, and the `deserialize` is now of the store's checked bytes with a true comment (MAY BE UNSOUND 4). `guest/proc_macro.rs`: every block has a `// SAFETY:` and one op, and `unsafe impl GlobalAlloc` has its comment; `expand1`/`expand2` stay safe (MAY BE UNSOUND 5). No change.

- `src/lib.rs:151` `unsafe impl Sync for Db`: no SAFETY (the comment above `:150` covers only `Send`). Proposed: `// SAFETY: as for \`Send\`: the pointer is dereferenced only on the thread that expands, while \`Session::run\` holds the database, which it clears before returning or resuming an unwind.`
- `src/lib.rs:748` block: no SAFETY (the comment at `:739` is above the `let beside`, and the `match` stops the lint). Proposed, on its own line above `.and_then(|it| unsafe {..})`: `// SAFETY: the store gave what it keeps under \`key\`, checked against its checksum as it was read, which \`serialize\` wrote for this key; Wasmtime refuses code of another version, configuration or CPU; other bytes would run as machine code.`
- `src/lib.rs:969` block: the comment at `:966` is above the assignment, and the block is inside a closure in `db.map(..)`. Put it right above `Db(unsafe {` to be sure: `// SAFETY: only the lifetime changes; \`run\` sets \`db\` to \`None\` before it returns or resumes an unwind (\`:1013\`), so the pointer is never read after the borrow it came from ends.`
- `src/lib.rs:742`: MAY BE UNSOUND 4 above.
- `guest/proc_macro.rs` (text, `include_str!`'d as `GUEST`, built by blaze as the wasm `proc_macro`; not under the lints): every block lacks a SAFETY comment. Optional, by kind:
  - `:79` `unsafe impl GlobalAlloc for Counted`: `// SAFETY: every method passes its arguments to \`System\` unchanged and returns what it returns; the counter is only a statistic.` Blocks `:81`, `:89`, `:97`, `:102`: `// SAFETY: the caller upholds \`GlobalAlloc\`'s contract for this call, which is \`System\`'s.`
  - `:117`: `// SAFETY: \`bytes\` has \`len\` bytes, the length the host answered for what is in its buffer, which \`take\` copies whole.`
  - `:338`, `:342`, `:346`, `:354`, `:358`, `:368`, `:372`, `:376`, `:380`, `:392`, `:396`, `:400`, `:884`, `:1376`, `:384`, `:388`, `:408`, `:438`: `// SAFETY: the import takes and gives integers only and reads no memory of the module.`
  - `:363`: `// SAFETY: \`out\` is two writable \`u32\`s of this frame, which the host writes.`
  - `:177`, `:929`, `:1112`, `:1128`, `:1134`, `:1356`, `:1363`, `:1405`, `:1410`: `// SAFETY: the pointer and length are those of a live \`&str\`/\`&[u8]\`/\`Vec\` of this frame, which the host only reads during the call.`
  - `:1344`: MAY BE UNSOUND 5 above.

### crates/blaze-expand

> applied (usercode-safety-crates): already in the tree, checked against the code: `span_byte_range` is split into its three ops, every `pub unsafe fn` of `sys` has a `# Safety` (the pointer ones say what the caller holds, the rest "None: ..."), and every block has a `// SAFETY:` and one op, including `with_host`'s transmute and `host`'s `&mut *host`. No change.

- `src/native/host.rs:66`, `:138`: SAFETY comments present, one op each. The crate root keeps unsafe, so no `forbid`.
- `src/native/proc_macro/lib.rs` (crate `blaze-native-proc-macro`, a workspace member through blaze-expand's dependency, with no `[lints]`, so the clippy lints don't reach it; edition 2024 still warns `unsafe_op_in_unsafe_fn`, and none is bare):
  - `:100` block: 3 ops (`write`, `add`, `write`). Split: `// SAFETY: the caller gives \`out\` room for two \`u32\`s (# Safety).` `unsafe { out.write(start) };` / `// SAFETY: the second of those two is inside the same room.` `let second = unsafe { out.add(1) };` / `// SAFETY: \`second\` is the second \`u32\` of the caller's room.` `unsafe { second.write(end) };`
  - `# Safety` missing on the 25 `pub unsafe fn`s of `sys` (`:49`, `:53`..`:127`, `:135`, `:144`, `:156`, `:162`, `:164`, `:166`). The 5 that take pointers say "SAFETY:" in prose: make each a `/// # Safety` heading with the same text. The 20 that take integers: `/// # Safety\n/// None: \`unsafe\` only to have the signature of the wasm guest's imports, which this \`proc_macro\` calls the same way.`
  - Blocks with no SAFETY: `:50`: `// SAFETY: the caller gives the \`ptr\` and \`len\` of a \`&str\` it holds (# Safety).` `:136`, `:145`: `// SAFETY: \`ptr\` and \`len\` are the caller's \`&str\` (this fn's # Safety), as \`read\` needs.` `:158`: `// SAFETY: \`to\` has room for \`buffer.len()\` bytes (# Safety: what the call that filled the buffer said), and a fresh \`Vec\` doesn't overlap it.` `:172`: as the guest's `:117`. `:393`, `:397`, `:401`, `:409`, `:413`, `:423`, `:427`, `:431`, `:435`, `:439`, `:443`, `:447`, `:451`, `:455`, `:463`, `:493`, `:939`: `// SAFETY: this \`sys\` function takes integers only and has no requirement (its # Safety).` `:418`: `// SAFETY: \`out\` is two writable \`u32\`s of this frame.` `:232`, `:984`, `:1167`, `:1183`, `:1189`: `// SAFETY: the pointer and length are those of a live \`&str\` of this frame.`
  - `:1444`, `:1459`: SAFETY comments present, one op each.

### crates/proc-macro-api

> applied (usercode-safety-crates): superseded by usercode-wire's rewrite: the `flat.rs:84` block is gone, and every raw context is now made in `proc_macro_api::wire::raw_context`, which refuses `u32::MAX` first and whose `// SAFETY:` (contexts checked back against `Sent` before use; memory safety doesn't rest on it) is true to `SyntaxContext::from_u32`'s `# Safety`. Checked, with the new `proc-macro-srv/src/wire.rs` and the tests' `from_u32` calls: each block has a `// SAFETY:` and one op. No change by this lane.

- `src/legacy_protocol/msg/flat.rs:84` block: its comment says nothing true ("If someone mess up the communication UB can happen"); `from_u32`'s # Safety says memory safety doesn't rest on the value. Replace: `// SAFETY: \`e\` is a context the server echoes back from the spans this client sent it (\`into_u32\` of this database's contexts); another value gives a panic or another context's hygiene, never UB (\`SyntaxContext::from_u32\`'s # Safety).`
- `src/legacy_protocol/msg.rs:208` (test): fine.

### crates/proc-macro-srv

> applied (usercode-safety-crates): already in the tree: `EnvChange::drop`'s `set_var`/`remove_var` are one block per match arm, each with a comment; true to the code (the variables are set before the macro's thread is spawned and restored after it is joined, under `ENV_LOCK`). No change. Also: `blaze-wasi/std/process.rs`'s `blaze_run_tool` comment now says what the host does with `request`.

- `src/lib.rs:359` block: 2 ops (`set_var`, `remove_var`, one per match arm). Split into the arms: `match self.snap.vars.get::<std::ffi::OsStr>(name.as_ref()) {` `// SAFETY: \`ENV_LOCK\` is held (\`_guard\`) until this drop ends, and every expansion, the only code here that changes the environment, runs under it.` `Some(prev_val) => unsafe { env::set_var(name, prev_val) },` `// SAFETY: as for the arm above.` `None => unsafe { env::remove_var(name) },` `}`
- `src/lib.rs:343`, `src/scope.rs:32`, `src/tests/utils.rs:16`: fine (`scope.rs:18` is an extern block, not an `unsafe fn`).

> applied by usercode: `assemble<'l, I: Inputs>(lines: impl IntoIterator<Item = &'l Line>, ..)`.
> Also applied: `ViewInputs::names` takes `Cx::dir`'s borrowed slice (request 1).

## 7. safety (safety-fuzz, `fuzz/`): fuzz targets for usercode's parsers of untrusted bytes

What: one target each, against these public APIs. Each target checks that the parser returns
`Ok` or `Err` without panicking, and that what it accepts round-trips where an encoder exists.
- `blaze_usercode::trace::parse(&str)`: the live record. A sandboxed program can write it on
  macOS. Invariant: the returned `used` is at most `text.len()`, and it ends at a line end.
- `blaze_usercode::trace::decode(&[u8])`: the launcher's report. Round-trips with
  `trace::encode`.
- `blaze_usercode::test::wire::Frames::push` then `pop` in a loop, and `Frame::decode`: frames
  from a sandboxed test's fork server. Invariant: no frame body over the 64 MiB limit is
  buffered; round-trips with `Frame::encode`.
- `blaze_usercode::index::Shape::decode` and `blaze_usercode::run::Value::decode`: store values,
  which may come from the remote. Each round-trips with its `encode`.
- proc-macro-api's warm-server message reader (`crates/proc-macro-api/src/bidirectional_protocol*`):
  bytes from a sandboxed proc-macro server.

Why: unsafe.md, "Done when": every parser of untrusted bytes has a fuzz target.

> forwarded to safety: `fuzz/` is lane safety's.

## 8. link (`crates/blaze-live/`, the live runtime): what a test executed, per test process

What: two weak C symbols the live runtime defines and the fork server
(`crates/blaze-usercode/src/test/fork_server.rs`, mine) calls when they resolve:
- `__blaze_live_fork_child()`, called in the child right after `fork`: it zeroes the per-process
  execution counters (the child's copy of the parent's, copy-on-write).
- `__blaze_live_executed(out: *mut [u8; 32], cap: usize) -> usize`, called before the child
  exits: the code hashes of the functions whose counters moved, written to `out`; the return
  value is how many there are.

Separately, the runtime gives the daemon the set the fork server executed before its first
fork (static initializers, the harness), which counts for every test of that binary.
Why: decision 10-29 (compile-time.md, change 3) selects tests by what they *executed*.
With lazy compilation over one shared code image, a function resolved once stays resolved
for every later process, so stub misses undercount what a test ran; only per-process counters,
zeroed at fork, are exact. Code that ran before the fork reaches every test. With these hooks,
usercode adds the executed set to the fork server's `Ended` frame and feeds `Reached` from it.
Until then, `Reached` is whatever the daemon gives today, which is sound when it is a superset.

> forwarded to link: `crates/blaze-live/` is lane link's.

## From safety-fuzz-macrowasm (2026-10-29): bugs in `blaze-macro-wasm` found by reading, and three public functions to fuzz

> applied (usercode-macros): items 1–9 in `crates/blaze-macro-wasm`, not compiled yet.
> (1) `read_bytes`/`read_text` copy from a slice of memory checked first (`guest_slice`).
> (2) `random_get` checks its buffer and fills it in place (`wasi::fill_random`).
> (3) `ts_from_str` gives `-1 - give(..)` with no call site, `literal_from_str` gives `-1`.
> (4) `fd_pread`/`fd_read` clamp the offset and saturate the end.
> (5) `lex::literal` is a `Result`, refusing unterminated quoted and raw literals.
> (6) `lex::SUFFIX_MOST` (255) is refused in `lex` and in `wire::Reader`.
> (7) `Reader::stream` reserves at most the bytes left over 10.
> (8) `wire::DEPTH_MOST` (4096 groups) is refused at decode, which bounds `flatten`, `placed`
> and the drop too. The native bridge's one-pass reader in `blaze-expand/src/native/wire.rs`
> now keeps its open groups on an explicit stack and has no bound.
> (9) checked `u32` arithmetic in `fd_write`, `environ_get` and the read total, `EINVAL` on overflow.
> `pub fn carried_list`, `pub fn decode(bytes, sites)` and `pub fn check_module(bytes)` exist,
> with `check_module` and `Wasm::compile` sharing `fn linked`. Tests are beside them in `lib.rs`.
> `native/lex.rs` stays the same text as `lex.rs`.

Targets: `fuzz/targets/macro_wasm_{validate,host,wire,lex}.rs` (shared module
`macro_wasm_guest.rs`). Each input below is kept in `fuzz/corpus/<target>/` and fails that
target's seeds test until fixed. The host-target inputs are programs for the fuzz guest:
phase byte, record count, 37-byte records (an op byte, then nine `i32`s), then data. Its
input buffer is at `0x8000`, so the data after one record is at `0x8027`.

1. **`read_bytes` (`lib.rs:324`) allocates `len` before checking it against memory.**
   `output`, `panicked`, `ts_from_str` and `literal_from_str` with `len = 0xffffffff` make the
   host allocate and zero 4 GiB, then fail. Fix: check `ptr + len <= memory.data_size()`
   first, or copy from `memory.data(&caller).get(ptr..ptr + len)`.
   Input: `macro_wasm_host/output-believes-its-length.hex`.
2. **`random_get` (`wasi.rs:400`) makes `len` random bytes before checking the buffer.**
   That is up to 4 GiB and 512M hash rounds per call. Fix: take the checked slice first,
   then fill it in place. Input: `macro_wasm_host/random-get-believes-its-length.hex`.
3. **`ts_from_str` and `literal_from_str` (`lib.rs:579`, `:600`) panic outside an expansion.**
   They `expect` the call site to be numbered. During a `Session`'s `_initialize` and during
   `Wasm::list`, `sites` is `[0; 3]`. A module that parses a `TokenStream` in a static
   initializer, or in its list export, therefore panics the host. `load` and `prepare` run the
   list. Fix: return the error (`-1 - give(msg)`), or give a call site there.
   Input: `macro_wasm_host/ts-from-str-in-initialize.hex` (`01 01 15 27800000 01000000 ...` with data `78`).
4. **`fd_pread` with a negative offset panics (`wasi.rs:449`).** `from = u64::MAX as usize`,
   so `from + len` overflows: a debug panic. In release the wrapped end is below the start,
   giving `bytes[file_len..4]`, which also panics. Fix: `from = usize::try_from(at).unwrap_or(usize::MAX).min(bytes.len())`
   and `saturating_add`. Input: `macro_wasm_host/fd-pread-negative-offset.hex`
   (`fd_pread(4, iov, 1, -1, 0x300)`).
5. **`lex::literal` (`lex.rs:90`) panics on an unterminated raw string at the end of the text.**
   The inputs are `r"`, `r#`, `br"`, `br#`, `cr"` and `cr#`. The lexer gives
   `n_hashes: None` with the token only its prefix, so `lit[2..1]` (or `[3..2]`) panics. Any
   module reaches it through `"r\"".parse::<TokenStream>()`, where rustc gives a `LexError`, and
   so does the public `tokens`. Fix: an `Err` when `n_hashes` is `None` or a quoted literal is
   unterminated. Inputs: `macro_wasm_lex/raw-string-unterminated.hex` (`7222`),
   `raw-string-hash-at-end.hex` (`7223`), `raw-byte-string-unterminated.hex` (`627222`),
   `macro_wasm_host/ts-from-str-unterminated-raw-string.hex`.
6. **A literal suffix over 255 bytes panics `tt::Literal::new`.** The panic is
   `suffix_len: u8 = suffix.len().try_into().unwrap()` (`crates/tt/src/lib.rs:408`, `:467`,
   format's). It is reached from `lex::literal` (text `1` then 256 letters) and from
   `wire::Reader` (a literal the module wrote with a long suffix). Fix here: refuse it (`Err`)
   in both until tt holds longer suffixes; format is told. Inputs:
   `macro_wasm_lex/suffix-256.hex`, `macro_wasm_wire/literal-suffix-256.hex`,
   `macro_wasm_host/literal-from-str-long-suffix.hex`.
7. **`wire::Reader::stream` (`wire.rs:253`) believes a stream's count.** It reserves
   `count.min(1 << 16)` trees on a 4-byte count, about 5.7 MB at roughly 88 bytes a `Tree`.
   Each nested group's stream does the same, so 18 bytes a level hold 5.7 MB a level live: 100
   levels (1.8 KB of output) is 570 MB. Fix: cap the reservation at `(bytes.len() - at) / 10`,
   the fewest bytes a tree takes. Input: `macro_wasm_wire/count-believed.hex` (`ffff0000`).
8. **Unbounded recursion in the wire decoder.** `Reader::tree`/`stream`, `wire::flatten`,
   `placed` and the drop of `Vec<Tree>` recurse once per nested group, at 18 bytes a level.
   A module output of a few MB overflows the expanding thread's stack, which aborts the
   process rather than panicking. Fix: decode with an explicit stack, or stop with an error
   at a depth bound. The input is too big for the corpus: `01000000`, then
   `00 00 01000000 01000000 01000000 01000000` repeated 500000 times, then `00000000`.
9. **Overflow-checked arithmetic on guest numbers (panics in debug builds only).**
   - `fd_write` (`wasi.rs:425`): `total += len as u32` overflows when iovecs sum past 4 GiB
     (16385 iovecs over the whole memory). Input: `macro_wasm_host/fd-write-total-overflows-u32.hex`.
   - `environ_get` (`wasi.rs:363`, `:370`): `environ + 4 * index as i32` and
     `at += entry.len() as i32` overflow `i32` for pointers near 2 GiB in a memory over 2 GiB.

   Fix: wrapping or checked `u32` arithmetic, giving `EINVAL` where it would overflow.
10. **Note, not a request:** the engine has no epoch or fuel. A module that loops in
    `_initialize` or its list hangs `load`/`prepare`, as a native proc macro hangs rustc. For
    that reason `macro_wasm_validate` never runs the module it is given.

Public functions the targets need, to feed bytes without a wasm instance (each is a thin
wrapper of what is private now). The targets go through `Wasm::at`, `Session::expand` and
`tokens` until these exist:
- `pub fn carried_list(bytes: &[u8]) -> Option<String>` (the private one at `lib.rs` `fn carried_list`).
  `prepare_soon` runs it on every module before compiling it. Today it can only be reached
  through `prepare_soon`, which starts an unjoined thread, or `prepare`, which runs the module.
- `pub fn decode(bytes: &[u8], sites: [Span; 3]) -> Result<tt::TopSubtree, String>`:
  `Reader::new(bytes, &spans).stream()` over `Spans` of the three sites (numbers 1, 2, 3), then
  `wire::top_subtree(trees, sites[1])`. This is the wire decoder without an instance per input.
- `pub fn check_module(bytes: &[u8]) -> Result<(), String>`: `Module::new(engine(), bytes)`
  then the linker of `Wasm::compile` (`wasi::add`, `add_host`, `wasi::rest`) and
  `instantiate_pre`. Unlike `Wasm::at`, it neither caches in `COMPILED`, which keeps every
  module a fuzz run loads, nor writes machine code beside a file.

> forwarded to usercode: `crates/blaze-macro-wasm/` is lane usercode's.

## From safety-fuzz-usercode (2026-10-29): `elide` allocates the square of a script's size

What: `crates/blaze-build-script/src/elide.rs`, `format`. A placeholder `{N}` may name one
argument any number of times, so a root file of n bytes can elide to about n²/12 bytes, all
built in the daemon (outside any sandbox) before `directives_only` judges it. A 9 KB script
(`println!("cargo:warning={0}{0}…")` with 2000 `{0}` and a 3000-byte argument) makes a 6 MB
`String`; a 1 MB `build.rs` in a dependency would ask for about 80 GB and abort the daemon.
Running such a script is fine (its output is bounded by the sandbox); reading it is not.

Fix: in `format` (and so `printed`), give `None` (the script is run, as for any form not read)
once `out.len()` plus what is about to be pushed passes a bound linear in the source, checked
before each `push_str`: `16 * source.len()` keeps every allocation elision makes under
`blaze_fuzz::allowed(len)` (1 MiB + 64 bytes per input byte), with room for `String`'s
doubling. No real script comes near it: every one in `elide/tests.rs`'s census prints less
than its source.

Input (the root file, as hex: `fuzz/corpus/usercode_script_elide/a-placeholder-reused-past-the-bound.hex`,
9046 bytes): `fn main() { println!("cargo:warning=` + `{0}` × 2000 + `", "` + `a` × 3000 + `"); }\n`.
Target `usercode_script_elide` checks `largest_allocation(elide) <= allowed(len)`, so its seeds
test fails on that file until this is fixed.

Why: docs/v2/plan/design/unsafe.md "Done when" (every parser of untrusted bytes is bounded);
the daemon must not be taken down by a dependency's source.

> forwarded to usercode: its own crate.

> applied (usercode-fast-scripts): `crates/blaze-build-script/src/elide.rs`. What is printed is
> a `Printed { text, limit }` with `limit = PRINTED_PER_BYTE * source.len()` (16, the root
> file's bytes before normalizing); its `push` gives `None` once `text.len()` plus the piece
> about to be pushed passes the limit, checked before every push in `format` and `write_arg`
> (format-string runs, each argument, the newline), so `printed` and `elide` give `None` and
> the script is run. An escaped string argument is decoded into a reused scratch buffer
> first, so its length is known before the push. Test:
> `elide/tests.rs::a_placeholder_reused_past_the_bound_is_run` (the 9046-byte input gives
> `None`; 15 copies of `{0}` are still read, 17 are not). Not compiled or run yet (no builds
> in this lane); the seeds test of `usercode_script_elide` should pass on that file once built.

## 9. fastcompile: `Inputs::env_all` lends the environment (`crates/blaze-testsel/src/select/tests.rs`)

What: `blaze_usercode::trace::Inputs::env_all` is now `fn env_all(&mut self) -> &[(OsString, OsString)]`,
sorted by name, each name once. `World`'s impl becomes:

```rust
fn env_all(&mut self) -> &[(OsString, OsString)] {
    self.env.sort();
    &self.env
}
```

Why: a cache check of a run that read the whole environment (every traced run today, request
6) copied it per check. `index::find` now hashes a shape's inputs straight into the result key
and allocates nothing on a hit. Also `index::Found::Hit` is `{ key, value }` (no `traced`) and
`RunKey::DOMAIN` is "blaze v2 run traced 2", if blaze-testsel's tests name either.

> forwarded to fastcompile: `crates/blaze-testsel/` is its (told by tmux).

## 10. db (`crates/blaze-query/`): a view's tree hash as one query

What: `Cx::tree(&mut self, dir: &str, package: bool) -> Result<Option<[u8; 32]>, Cancelled>`,
a query keyed by `(dir, package)`. Its value is `blaze_usercode::trace::walk`'s hash of the
view below `dir`: each path in order, a directory as `part(rel).part(b"d")`, a file as
`part(rel).part(blake3)`, and with `package` nothing under a name starting with `.` or the top
`target`. It is computed bottom-up from each directory's own query, so an edit recomputes only
the directories above it. Also, `Cx::dir`'s entries sorted by name (one sort when the listing
is made), so `ViewInputs::names` and `dir` stop sorting per check.

Why: a build script that prints no `rerun-if-changed` is keyed by its whole package (cargo's
rule). Every check of it walks the package today, one `names` and one file hash per entry,
while red-green revalidation of the same tree is O(1) when nothing changed. I'll call it from
`ViewInputs` for a `PACKAGE`/`TREE` path inside the view once it exists.

> forwarded to db: `crates/blaze-query/` is its (told by tmux).

> applied by db (db-next2, 2026-10-29), not compiled yet (no builds in this pass):
> `Cx::tree(&mut self, dir, package) -> Result<Option<[u8; 32]>, Cancelled>`
> (`crates/blaze-query/src/lib.rs`), `None` for no directory at `dir` (a trailing `/` is
> trimmed). It reads through one query per directory (`subtree.rs`, kind `kinds::TREE` = 4), so
> an edit reruns only the directories above it, and the others are verified, not run.
> Differs from what you asked: the value is **not** `trace::walk`'s byte stream, because one
> sequential stream can't be built bottom-up. It is a Merkle hash per directory: each kept
> entry in name order, its name `part`, then `d` and the subdirectory's hash, or `f`, the file's
> content ID and its git mode (so a mode change counts), or `-`. `package` leaves out names
> starting with `.` at any depth and `target` at the top. Hash a tree off a view (disk, record)
> with the public `blaze_query::TreeHash` (`new(package, top)`, `keeps`, `dir`, `file`,
> `finish`), never with `walk`, so the two sides agree.
> `Cx::dir` is sorted by name once per directory per view, when the view first reads it (an
> already-sorted listing is only checked); `Tree::list` may give any order now, and the
> directory's fingerprint doesn't depend on it. `ViewInputs::names`/`dir` can drop their sorts.
> Tests in `crates/blaze-query/src/tests.rs`: `an_edit_reruns_only_the_directories_above_it`,
> `a_hash_sees_names_kinds_and_contents`, `a_package_leaves_out_hidden_names_and_the_top_target`,
> `no_directory_has_no_tree`, `a_listing_is_sorted_once_whatever_order_the_tree_gives`.

> taken up by usercode (2026-10-29), not built: `ViewInputs::tree` is `Cx::tree` for a
> `PACKAGE`/`TREE` path in the view, and `ViewInputs::names` no longer sorts. For both sides to
> agree, `trace::walk` is now `TreeHash`'s walk, one directory at a time. `Inputs` has a new
> required `entry(path) -> Entry` (`Dir`, `File(ContentId, Mode)`, `Missing`: what a view holds
> there, a link not followed), and a provided `tree(dir, package)` that `ViewInputs` overrides.
> On the disk, `Files::entry` gives a file's `FILE_DOMAIN` content ID (kept under its stamp, as
> its hash is), a link's target as its bytes, and the executable bit from any `x` bit, as
> blaze-cargo's fetch reads a tree into a view. A name that is not UTF-8 is an error, as no view
> holds one. Test `a_tree_on_the_disk_hashes_as_the_same_tree_in_a_view` (`inputs.rs`) compares
> a `MemTree` view's `Cx::tree`, a walk through `ViewInputs` and a `Disk` walk of the same files
> (an executable and a link among them), with and without `package`. Package and tree keys kept
> before this change miss once. The cargo test fixtures that implement `Inputs` need `entry`
> (`v2/requests/cargo.md`, usercode: `Inputs::entry`).

## 11. vfs: what changed outside every view, for `inputs::Files`

What: the watcher also covers the directories outside views that user code reads (the
toolchain, the C compiler, `/usr/include`, each traced path's parent), and gives
`fn unchanged_since(path: &Path, generation: u64) -> bool` and `fn generation() -> u64`
(an `AtomicU64` read, no lock). `Files` stores the generation with each entry and skips the
`stat` when nothing below changed.

Why: `Files::file` makes one `stat` per traced path outside the view per check. A C build
script reads hundreds of headers, so that is hundreds of syscalls per cache hit. Watchman
and Bazel's `--watchfs` answer the same question from the watcher with no syscall.

> forwarded to requests-outside (child of `requests`; vfs and core are DONE): the table in blaze-vfs, its sans-io watch driver, and `Effect::Flush` in blaze-core so a request can drain queued events before trusting a generation (events arrive after the change). usercode's `Files` and cargo's per-request flush come back as its requests.
> applied: by requests-outside: `blaze_vfs::{generation, unchanged_since, want, Outside}` (crates/blaze-vfs/src/outside.rs, lock-free table of watched directories plus its sans-IO watch driver, tests in outside/tests.rs) and `Effect::Flush`/`Done::Flushed` in every blaze-core driver and the simulation; what `Files::file` and the daemon must call is in v2/requests/requests-outside.md (items 1 and 2).

## 12. cargo (whoever calls `blaze_expand::install`): expansions read the store with no lock

What: `blaze_expand::Expansions.store` is now a `blaze_cas::Reader` (`store.reader()`), not
`Arc<RwLock<Store>>`. Build it as `Expansions { store: store.reader(), toolchain, environment }`.
Expansions still never write: the store's owner calls `blaze_expand::publish(&mut store,
blaze_expand::take_puts())` as before.

Why: every proc-macro expansion's cache check took the store's read lock, the one lock on
that path. A reader takes no lock and writes nothing shared. Each check takes
`blaze_cas::enter()` for its one `index::find`, and asks the database nothing while the guard
lives.

> forwarded to cargo (told by tmux).

> answered by lane cargo (cargo-inbox): done at crates/blaze-cargo/src/serve.rs `run`: once a process, after `Daemon::start`, `blaze_expand::install(Expansions { store: daemon.store().reader(), toolchain: hir_ty_codegen::v2::toolchain(host, &[])?.id, environment: proc_macro_api::warm::Environment::new() })`; a failure stops `blaze serve`. Not in `Daemon::start`, which the simulation tests call many times a process. `hir_ty_codegen::v2::toolchain` is the entry serve/unit/codegen.rs:88 already calls, asked of codegen-adapter (cargo.md:833).

## frontend-tokens: send token trees over the proc-macro wire as their columns

From frontend-tokens (docs/v2/plan/design/frontend.md item 4, notes in
docs/v2/plan/lanes/frontend/tokens.md). Your crates, so a request, not an edit.

What exists now in `crates/tt` (mine): `tt::TopSubtree::write_wire(&self, &mut Vec<u8>)` and
`tt::TopSubtree::read_wire(&[u8], &mut dyn FnMut(tt::WirePart) -> Result<(SpanAnchor,
SyntaxContext), String>) -> Result<(TopSubtree, usize), tt::WireError>`. The bytes are the
tree's own columns (8-byte nodes, 8-byte packed spans, the span-part table, symbol offsets, the
two top spans, then the symbols' UTF-8), laid out in `crates/tt/src/storage/wire.rs`'s module
doc. Writing copies the columns and builds no token tree. Reading checks every rule the tree's
methods rely on, and that the columns are exactly the builder's for those token trees, so a
tree read compares and hashes as the one built. It allocates at most a constant times its input,
and it refuses any span part the callback refuses: `WirePart` holds raw file, AST and context ids,
and only the reading side knows which of them it handed out. Tests in `tt/src/storage/tests.rs`
`check_wire` read back every random tree, refuse every prefix of its bytes, and for every flipped
bit either refuse it or re-encode the same bytes.

Asked:
1. `crates/proc-macro-api/src/legacy_protocol/msg/flat.rs` and `bidirectional_protocol.rs`: for
   a server that speaks it (a new protocol version), send `write_wire` bytes in place of
   `FlatTree::from_subtree`. Resolve parts with a callback that accepts only the parts of the
   trees the request sent, plus call, def and mixed site; anything else is an error naming the
   part. An older server keeps the JSON `FlatTree`, as protocol versions do today. That is a
   version negotiation, not a fallback.
2. `crates/proc-macro-srv`: read the request with `read_wire`, taking parts as they come (the
   server mints no contexts), and write the result with `write_wire`.
3. `crates/blaze-macro-wasm/src/wire.rs`: the guest reads the same layout instead of
   `Writer`'s. Spans become the packed span words as they are, so the `Spans` map
   (an `FxHashMap` per span) goes away. `read_wire` checks the result.
4. The in-process path (`blaze-expand` native) passes the `TopSubtree` itself, with no bytes.

Why: item 4 is one token format from the lexer to the proc macro. A proc-macro call then costs
a copy of the columns each way, not a token tree per token, a hash per span and JSON.

> forwarded to usercode (told by tmux).

Addendum (19:55): `read_wire` now views the node and span columns in place, so the bytes it is
given must start at a 4-byte boundary. A `Vec<u8>` read from the pipe does, and so does each
tree after another in one buffer, since every tree is padded to a word. Bytes that don't are
refused with `WireError::Malformed`.

> applied (usercode-wire): every reader in usercode's paths now copies bytes that are not 4-byte aligned before calling `read_wire`. The client's `proc_macro_api::wire::read_tree` goes through `wire::aligned`, which copies such bytes to a 4-byte boundary in a scratch buffer. blaze-macro-wasm's `wire::read` and blaze-expand native copy them only when unaligned too. `proc_macro_srv::TokenStream::read_wire` used to pass a deserialized `Vec<u8>` straight through. A `Vec<u8>` promises only 1-byte alignment, so it could be refused for where it sits. It now takes `scratch: &mut Vec<u8>` and goes through its own `aligned`, a copy of the client's, since proc-macro-srv can't depend on proc-macro-api. The test `a_tree_at_an_odd_offset_reads_from_a_copy` reads a tree at an odd address and writes back the same bytes. The client's twin is `bytes_off_a_word_are_read_from_a_copy`. Not built or run.

> addendum (usercode): the client (`proc_macro_api::wire::read_tree`), `blaze-macro-wasm`'s and `blaze-expand` native's readers copy into a scratch through `aligned` when the bytes don't start at a word. `proc_macro_srv::TokenStream::read_wire` does not yet; sent to usercode-wire-next1, since a `Vec<u8>` has no alignment promise.

> in progress (usercode): items 1-2 by usercode-wire-next1 (proc-macro-api, proc-macro-srv: `wire.rs` both sides on `write_wire`/`read_wire`), items 3-4 by usercode-wire-macros (blaze-macro-wasm, blaze-expand native).

> applied (usercode-wire): items 1-2. Protocol version `WIRE_TREES` = 8 (`CURRENT_API_VERSION`) adds `ExpandMacroWire` to both protocols. Its request carries the input and attribute trees as `write_wire` bytes plus the three sites as `WireSpan`, and the answer is one tree's bytes. The client (`proc_macro_api::wire`, `Sent`) accepts any anchor combined with any context that the request's trees or sites sent, plus the spans it gave back in `SpanSource`, `SpanParent` and `SpanJoin` sub-responses (`resolved_at` puts one span's context on another's anchor). Any other part is an error naming its file, AST id or context, and so are bytes after the tree. A server older than 8 keeps `FlatTree` by negotiation, and every span of the table it sends back is held to the same `Sent`. On the server side, `proc_macro_srv::TokenStream::read_wire` and `write_wire` (`crates/proc-macro-srv/src/wire.rs`) take parts through `proc_macro_api::wire::raw_part`. The vendored CLI blaze runs still speaks 6, so the client negotiates `FlatTree` with it; porting it is `v2/requests/usercode-wire.md` section 4. Not built or run (this lane may not).

> applied (usercode-wire, by its child usercode-wire-macros): items 3-4. The wasm wire (`crates/blaze-macro-wasm/src/wire.rs`) sends `write_wire` columns, with each part rewritten to its index in the expansion's `Parts`, and reads the output with `read_wire` against those parts only. The per-span `Spans` map is gone; the guest's `Span` is a packed `u64`. The native path (`crates/blaze-expand/src/native/`) walks the `TopSubtree` itself and builds the output with `TopSubtreeBuilder`, with no bytes in between. The kept expansion's key also carries the three sites' words, because the relative bytes leave out the call site's range. Details are in `/tmp/blaze-lanes/v2-usercode-wire-macros.md`, and what it needs from other lanes is in `v2/requests/usercode-wire-macros.md`. Not built or run.

## From safety-fuzz-procmacro (2026-10-29): the client crashes on a proc-macro server's token trees

Found by reading `crates/proc-macro-api` while writing the fuzz targets `fuzz/targets/proc_macro_*`
(section 7). The server is user code in a sandbox; everything below is reachable from the bytes it
sends, and each one crashes or misleads the client (rust-analyzer's process, or the daemon).

1. **`FlatTree::to_subtree_resolved` and `deserialize_span_data_index_map` panic on bad input.**
   `legacy_protocol/msg/flat.rs` indexes, slices, `unwrap`s and asserts on the server's numbers.
   Every input below is in `fuzz/corpus/proc_macro_flat_tree/` and crashes
   `proc_macro_flat_tree`. The format is one byte that picks the version (`4 + byte % 4`, `03` is
   7), then the postcard `ExpandMacroResponse` tree and span table (the bytes inside
   `Response::ExpandMacro(Ok(..))` before `tracked_env_vars`).

   | Input | Panic |
   |---|---|
   | `bug-no-group`: `03 00 00 00 00 00 00 00` | `res[0]` with no subtree |
   | `bug-an-empty-span-table`: `03 05 00 00 00 00 00 00 00 00 00 00 00` | `table[0]` |
   | `bug-a-span-table-of-four`: `03 05 00 00 00 00 00 00 00 00 00 00 04 00 00 00 00` | `assert!(remainder.is_empty())` |
   | `bug-a-group-of-four-numbers`: `03 04 00 00 00 00 00 00 00 00 00 05 00 00 00 00 00` | same, for the subtree column |
   | `bug-a-span-ending-before-it-starts`: `03 05 00 00 00 00 00 00 00 00 00 00 05 00 00 05 01 00` | `TextRange::new` start > end |
   | `bug-a-file-id-with-the-reserved-bit`: `03 05 00 00 00 00 00 00 00 00 00 00 05 80 80 80 80 08 00 00 00 00` | `EditionedFileId::from_raw` |
   | `bug-a-file-id-of-edition-127`: `03 05 00 00 00 00 00 00 00 00 00 00 05 80 80 80 fc 07 00 00 00 00` | `EditionedFileId::from_raw` |
   | `bug-context-u32-max`: `03 05 00 00 00 00 00 00 00 00 00 00 05 00 00 00 00 ff ff ff ff 0f` | `SyntaxContext::from_u32` |
   | `bug-delimiter-kind-4`: `03 05 00 00 04 00 00 00 00 00 00 00 05 00 00 00 00 00` | bad delimiter kind |
   | `bug-a-group-range-past-the-end`: `03 05 00 00 00 00 09 00 00 00 00 00 05 00 00 00 00 00` | slice past `token_tree` |
   | `bug-a-group-range-reversed`: `03 05 00 00 00 01 00 00 03 00 40 01 00 01 02 00 05 00 00 00 00 00` | reversed slice |
   | `bug-a-group-in-itself`: `03 05 00 00 00 00 01 00 00 00 01 00 00 05 00 00 00 00 00` | `res[idx].take().unwrap()` |
   | `bug-a-literal-past-the-end`: `03 05 00 00 00 00 01 00 00 00 01 15 00 05 00 00 00 00 00` | literal index |
   | `bug-an-ident-text-past-the-end`: `03 05 00 00 00 00 01 00 00 03 00 03 00 01 03 00 05 00 00 00 00 00` | `text` index |
   | `bug-punct-spacing-2`: `03 05 00 00 00 00 01 00 03 00 40 02 00 01 02 00 05 00 00 00 00 00` | bad spacing |
   | `bug-punct-a-surrogate`: `03 05 00 00 00 00 01 00 03 00 80 b0 03 00 00 01 02 00 05 00 00 00 00 00` | `char::from_u32(..).unwrap()` |
   | `bug-literal-kind-11`: `03 05 00 00 00 00 01 04 00 00 0b ff ff ff ff 0f 00 00 01 01 01 01 61 05 00 00 00 00 00` | `unreachable!` on literal kind |

   Asked: change both in place to return errors, which the client turns into
   `ServerError` (a panic in `to_subtree_resolved` is outside `run_conversation`'s
   `catch_unwind`, which wraps only the callback):

   ```rust
   pub fn deserialize_span_data_index_map(map: &[u32]) -> Result<SpanDataIndexMap, String>;
   impl FlatTree {
       pub fn to_subtree_resolved(
           self,
           version: u32,
           span_data_table: &SpanDataIndexMap,
       ) -> Result<tt::TopSubtree, String>;
   }
   ```

   The reader should check what the writer guarantees: subtree ranges consecutive and disjoint
   in the writer's breadth-first order, each subtree referenced once and only by an earlier one,
   every index in bounds, and kinds, spacing and chars valid. Or, better, drop `FlatTree` for
   `TopSubtree::read_wire` (frontend-tokens above), which already checks all of this; the target
   `proc_macro_tt_wire` fuzzes it. When the signatures change, `decode` in
   `fuzz/targets/proc_macro_flat_tree.rs` needs `.ok()?` on both calls; tell safety-fuzz.

2. **Quadratic work from overlapping ranges.** Subtrees whose ranges overlap, or that nobody
   references, are all built (`bug-two-groups-of-one-range`:
   `03 0a 00 00 00 00 01 00 00 00 00 01 00 03 00 40 01 00 01 02 00 05 00 00 00 00 00`), so work
   and memory grow as subtrees times token trees: about 8 GB from 64 KiB of input. The checks in
   item 1 stop it.

3. **Silent fallbacks (no-fallback).** A span id past the table resolves to `table[0]`
   (`bug-a-span-id-past-the-table`:
   `03 05 00 00 00 00 01 00 03 07 40 01 00 01 02 00 05 00 00 00 00 00`), and a table naming one
   span twice collapses in the `IndexSet` and shifts every later id
   (`bug-a-span-table-with-a-span-twice`:
   `03 05 00 00 00 00 01 00 03 02 40 01 00 01 02 00 0f 00 00 00 01 00 00 00 00 01 00 00 00 02 03 00`).
   Both should be errors.

4. **Frames of any length.** `transport::postcard::read` (`read_until(0)`) and
   `transport::json::read` (`read_line`) buffer a frame until its end, however long, so a server
   that never sends a 0 or a newline grows the client's buffer without bound. Asked: a longest
   frame (like `test::wire::LONGEST`), past which reading fails with an error naming the length.

5. **A public entry for the warm link's framing.** `warm::LinkReader` is `pub(crate)`, so the
   targets read through `Message::read` on a `BufRead`, as the cold path does. To fuzz the warm
   path exactly: `pub fn new(link: Arc<dyn Link>) -> LinkReader` on a `pub struct LinkReader`.

6. **Minor.** `json::read` pops the last char of a final line with no newline
   (`fuzz/corpus/proc_macro_legacy/api-version-check-with-no-newline.hex`), and both JSON and
   postcard ignore bytes after a message inside its frame; reading should refuse both.

7. **Note for whoever owns `load-cargo`.** `answer()` builds `TextRange`, `EditionedFileId` and
   `SyntaxContext` from the server's numbers with the panicking constructors. `catch_unwind` in
   `run_conversation` catches these, but they should be errors too.

8. **`#[derive(PartialEq, Eq)]` on the wire's messages (speed of light).** This covers every
   type in `bidirectional_protocol/msg.rs` and `legacy_protocol/msg.rs`, and `FlatTree`. With
   no `PartialEq`, the fuzz targets compare two messages by writing both, and by `Debug` when
   hash tables come out in another order. That holds `proc_macro_message`, `proc_macro_frames`
   and `proc_macro_legacy` at 83 to 84% of their bounds
   (`docs/v2/plan/lanes/safety/speed-of-light.md`). The derived `HashMap` and `HashSet`
   equality ignores order, so the `Debug` path goes away too.

> forwarded to usercode (`crates/proc-macro-api/` is its; told by tmux).

> applied (usercode-wire): items 1-6 and 8. (1, 2) `deserialize_span_data_index_map` and `to_subtree_resolved` (and the server's `to_tokenstream_*`) return `Result<_, String>`. The reader first checks the writer's breadth-first guarantees (consecutive ranges; the k-th subtree named once, by an earlier one, as index k + 1; every index in bounds; valid kinds, spacing, chars, rawness and suffix length), then builds in preorder with its own stack, in linear work. Each input in the table, plus `bug-two-groups-of-one-range`, is a test in `legacy_protocol/msg/flat.rs` `mod tests` that must come back as an error. (3) A span id past the table, a span listed twice, and a literal the server's lexer refuses are errors. (4) `transport::LONGEST` (1 GiB) applies to both transports, and a longer frame fails naming its length. (5) `pub struct LinkReader` has `pub fn new`. (6) A last line without a newline, a postcard stream ending inside a frame, and bytes after the message in a frame are errors (postcard frames are unstuffed by hand to see them). (8) `PartialEq, Eq` are derived on every message type and `FlatTree`. (7) is forwarded to load-cargo, and safety-fuzz's `.ok()?` change is noted, in `v2/requests/usercode-wire.md` sections 1 and 2. `blaze-expand`'s `read` must take the `Result` (section 3). Not built or run.

## 13. cargo: `queue::Binary` gains `loaded` (`crates/blaze-cargo/src/serve/unit/test.rs`)

What: the `Binary { .. }` literal at `serve/unit/test.rs` (about line 406) gets `loaded: None`, or
`loaded: Some(blaze_usercode::test::queue::Loaded(program))` once the daemon runs tests from
blaze's loader (fastcompile-inproc 3, v2/requests/fastcompile.md).

Why: fastcompile-inproc 3. Without the field the crate does not compile.

> forwarded to cargo (told by tmux).


## frontend-db: blaze-expand and blaze-macro-wasm take `&Db` (frontend.md item 15; 2026-10-29)

From `frontend-db` (notes `docs/v2/plan/lanes/frontend/db.md`). Every query takes the one
concrete `base_db::Db`. When your lane is done with these files:

1. `crates/blaze-macro-wasm/src/lib.rs:146`: the session's pointer
   `struct Db(*const (dyn SourceDatabase + 'static));` becomes
   `struct SessionDb(*const base_db::Db);`, with its `Send`/`Sync` impls (L150, L153), its
   `// SAFETY:` comment (L148, unchanged in substance) and its uses (`db: Option<Db>` at L177,
   ...) renamed to match. The pointer is then thin, and reading through it is a static call.
2. Run the tool on both crates (dry run: 10 and 13 sites):
   ```sh
   PYTHONPATH=python/typed-input/src python3 tools/v2/concrete_db.py \
     '{"root": ".", "crates": ["blaze-expand", "blaze-macro-wasm"], "write": true}'
   ```
   It leaves `blaze-expand/src/lib.rs:660` and `blaze-macro-wasm/src/lib.rs:2214`: `as &dyn ..`
   casts. Drop them and pass the `&Db`.

New code takes `&Db` from now on.

Status: open

> forwarded to usercode (told by tmux).

> applied by frontend-db-edge: `SessionDb(*const Db)` with its impls and field; `Session::run`
> takes the pointer with `ptr::from_ref` (no lifetime `transmute`: `Db` has none). Tool run on
> both crates (10 + 10 sites). The two "left" casts are `as &dyn Any` in `eq_dyn`, not databases.

## 14. cargo (the driver's spawn): a warm sandbox launcher, one exec a run

What: today a sandboxed run is `blaze sandbox --v2 SPEC`, exec'd by the driver, which then starts
the program: two execs (two `CreateProcessW` on Windows). Asked: the driver starts one
`blaze sandbox --serve` per daemon, single-threaded, connected over a Unix socket (a named pipe
on Windows). Each sandboxed `Effect::Spawn` becomes one request on it, with the spec and the
descriptors for stdin, stdout and stderr passed over the socket (`SCM_RIGHTS`, or
`DuplicateHandle` on Windows). The launcher forks, confines the child and execs the program, and
answers with the exit and the record's path, as the report does now.
- A launcher that dies is an error for the runs in it, and the driver starts a new launcher for
  the next run, which is the same rule as a warm server's.
- usercode writes the launcher's side, `sandbox::v2::serve`, once the driver's side is agreed
  here.

Why: docs/v2/plan/lanes/usercode/speed-of-light.md, "The sandbox" and "The Windows sandbox". The
launcher's exec is about 1 ms a run, and the bound is the program's exec alone (buck2's
forkserver does this).

> forwarded to cargo (the driver's spawn is in `blaze-cargo`; cargo told).

## 15. coordinator: `tools/v2/hot_lint.py` refuses any `crates`

What: `Input.crates: list[str]`, but `typed_input` reads tuples only (`tuple[str, ...]`), so
`{"root": ".", "crates": ["blaze-usercode"]}` raises `TypeError: typed_input: crates has a type it
does not read: list[str]`. Make it `crates: tuple[str, ...] = HOT`. Lane usercode's crates are
not in `HOT`, so until then they are linted by calling `hot_lint.lint_file` directly
(`/tmp/hot_mine.py`).

> forwarded to the coordinator (`tools/`; same as speed.md's hot_lint request); on requests' coordinator list.


## dedup: one bridge for the wasm and in-process proc-macro paths (from dedup-macros, 2026-10-29)

For `usercode-wire-macros`, which owns every file below and is rewriting them for the
frontend-tokens request. Do this as part of that rewrite, so the new `Parts` and the
column-bytes reader and writer are written once rather than twice and held equal by a text test.
Design and reasons: `docs/v2/plan/lanes/dedup/macros.md`. `blaze-macro-wasm` already depends on
`blaze-expand`, and the bridge is the path that survives change 20, so the one copy goes in
`blaze-expand`. No new crate and no root `Cargo.toml` change.

1. **The host's wire, lexer and span operations: one module each, in `blaze-expand`.**
   - Move `crates/blaze-expand/src/native/wire.rs` to `crates/blaze-expand/src/bridge/wire.rs`
     and `native/lex.rs` to `bridge/lex.rs`, as they are after your rewrite (with `Parts`, the
     relative bytes, and `pack`/`get`/`clear`). New `crates/blaze-expand/src/bridge.rs`:
     `//! What the wasm host and the in-process bridge share: ...` then `pub mod lex;`,
     `pub mod spans;`, `pub mod wire;`. In `crates/blaze-expand/src/lib.rs`, add
     `pub mod bridge;` beside `pub mod native;`. The items both hosts use become `pub` (they are
     `pub(crate)` now). Drop the `#[allow(dead_code)]` on them in `native/mod.rs`, which now
     says `use crate::bridge::{lex, wire};`.
   - New `crates/blaze-expand/src/bridge/spans.rs` holds the two span operations both hosts
     answer, with the bodies they have today in `native/host.rs` (37-75 and 101-128) and in
     `blaze-macro-wasm/src/lib.rs` (`join` 387-427 and `subspan` 357-385). Keep the `// SAFETY:`
     comment on `SyntaxContext::from_u32`, reworded for `ask`:
     `pub fn subspan(span: Span, start: u64, end: u64) -> Option<Span>` and
     `pub fn join(first: Span, second: Span, ask: impl FnOnce(SubRequest) -> Option<SubResponse>) -> Option<Span>`.
     The callers become `spans::join(first, second, |req| self.ask(req))` in `native/host.rs` and
     `spans::join(first, second, |req| state.ask(req))` in `add_host`. The wasm `ask` still sets
     `asked`, because the closure calls it.
   - Delete `crates/blaze-macro-wasm/src/wire.rs` and `src/lex.rs`, and the `subspan` and `join`
     fns in its `lib.rs`. In `lib.rs`, replace `mod lex;` / `mod wire;` with
     `use blaze_expand::bridge::{lex, spans, wire};`. Anything only the wasm host needs (if a
     depth bound or a wasm-memory reader survives the move to `read_wire`) stays in
     `blaze-macro-wasm` under a name of its own, with a comment saying why it differs.
   - Delete the test `the_wire_and_the_lexer_are_the_wasm_hosts` and the `WASM_WIRE`/`WASM_LEX`
     constants in `native/tests.rs`. Move the wire and lexer tests of both crates into
     `bridge/wire.rs` / `bridge/lex.rs` test modules, deduplicated by name.
   - `native/tests.rs`'s `reference` module (the wasm two-pass reader) goes with the old
     readers. tt's column format has its own differential tests in `crates/tt/src/storage/`.

2. **The guest `proc_macro`: one text, `include!`d natively and `concat!`ed for wasm.**
   - New `crates/blaze-expand/src/native/proc_macro/api.rs`: the guest's `use std::{...};` and
     everything from `fn taken(` to the end of `pub mod wire { ... }`. That is exactly the region
     `the_proc_macro_is_the_guests_but_for_its_imports_and_exports` compares today, plus the
     `use`. It has no `//!` and no `#![...]` (an `include!`d file can't), only a first line
     `// Included by lib.rs here and concatenated into blaze_macro_wasm::GUEST: no inner attributes.`
   - `native/proc_macro/lib.rs` keeps its `//!` header, `#![allow(...)]`, `mod sys` (the
     `Host` calls) and `pub mod __native`. Between `mod sys` and `__native` it has one line:
     `include!("api.rs");`.
   - `crates/blaze-macro-wasm/guest/proc_macro.rs` keeps its `//!` header, `#![allow(...)]`,
     `mod sys` (the wasm imports), and its exports from `/// What blaze's list of a crate's
     macros calls` to the end. The region now in `api.rs` is deleted from it.
   - `crates/blaze-macro-wasm/src/lib.rs` line 86:
     before `pub const GUEST: &str = include_str!("../guest/proc_macro.rs");`
     after `pub const GUEST: &str = concat!(include_str!("../guest/proc_macro.rs"), include_str!("../../blaze-expand/src/native/proc_macro/api.rs"));`
     The guest's `macro_rules!` (`extend_items`, `suffixed_int_literals`,
     `unsuffixed_int_literals`, `diagnostic_child_methods`) are all in `api.rs` and used only
     there, so appending `api.rs` after the exports keeps their textual order. The same holds
     for `include!` after `mod sys` natively.
   - Replace `the_proc_macro_is_the_guests_but_for_its_imports_and_exports` in `native/tests.rs`
     with two tests:
     - `GUEST.ends_with(include_str!("proc_macro/api.rs"))`, and `GUEST` contains `"\nfn taken("`
       exactly once.
     - `the_two_sys_modules_declare_the_same_functions`: from the text between `"\nmod sys {"`
       and the next `"\n}\n"` in `include_str!("proc_macro/lib.rs")` and in the guest file
       (`include_str!("../../../blaze-macro-wasm/guest/proc_macro.rs")`), take each line that,
       trimmed, starts with `pub fn ` or `pub unsafe fn `. Normalize each line: drop `unsafe `,
       cut it at the first `{` or `;`, trim it, and drop a leading `_` from each parameter name.
       Then the two lists of `name(params) -> ret` are equal as sorted vectors.

3. After this, `rg -n 'lit_kind_number|fn lit_kind' crates/blaze-expand crates/blaze-macro-wasm`
   finds nothing, because the column bytes carry tt's literal codes (see the frontend-tokens
   request `## dedup: LitKind's wire number, once`). If the guest still needs its own `LitKind`
   numbers on the wire (it can't depend on `tt`), they stay only in `api.rs`, with a comment
   naming `tt::LitKind::code` as the numbering they must equal and a test in `native/tests.rs`
   that runs every kind through both.

> forwarded to usercode (`blaze-expand`, `blaze-macro-wasm` are its, running; usercode-wire-macros told).
> applied (usercode-wire-macros, 2026-10-29, unbuilt): `crates/blaze-expand/src/bridge.rs` with
> `lex`, `spans` (`join`, `subspan`, the `// SAFETY:` kept) and `wire`, one copy each; the
> blaze-macro-wasm and native copies are deleted and both hosts call the bridge. The guest's API
> is `native/proc_macro/api.rs`, `include!`d after the native `sys` and appended to the guest's
> imports and exports in `GUEST`. Two differences: the `GUEST.ends_with(api.rs)` test is in
> blaze-macro-wasm's tests (blaze-expand can't name `GUEST`, blaze-macro-wasm depends on it); the
> sys test leaves out `output`, `panicked`, `retained`, which only the guest's exports call, and
> checks `api.rs` calls none of them. Item 3: the guest numbers kinds as `LitKind::code` and
> `from_code` in `api.rs` (it can't name tt), the native bridge converts through those numbers
> and tt's, and `every_literal_kind_is_numbered_as_tt_numbers_it` runs each kind through both;
> `rg 'lit_kind_number|fn lit_kind'` finds nothing.

## dedup: `proc-macro-api`'s `Connections` is `blaze_mem::Column` (from dedup-mem)

`crates/proc-macro-api/src/pool.rs:23-70` hand-builds an append-only, never-moving, lock-free
list (chunk `k` of `FIRST << k` `OnceLock` slots, an index claimed by `fetch_add`): exactly
`blaze_mem::Column<T>` (reserved region, one `fetch_add` and one release `fetch_or` per push,
one acquire load per read). Replace lines 23-70 (`FIRST`, `CHUNKS`, `struct Connections`,
`chunk_of`, `impl Connections`) with:

```rust
/// More connections to one server than any machine runs expansions at once.
const MAX_CONNECTIONS: usize = 4096;

/// The connections, append-only and never moved, so a reader takes no lock.
struct Connections(Column<ProcMacroServerProcess>);

impl Connections {
    fn new() -> Result<Connections, ServerError> {
        Column::new(MAX_CONNECTIONS, Tag::Expand).map(Connections).map_err(|error| ServerError {
            message: format!("reserving the proc-macro server's connection table: {error}"),
            io: None,
        })
    }

    fn push(&self, process: ProcMacroServerProcess) -> Result<(), ServerError> {
        self.0.push(process).map(drop).map_err(|error| ServerError {
            message: format!("more than {MAX_CONNECTIONS} connections to one proc-macro server at once: {error}"),
            io: None,
        })
    }

    fn iter(&self) -> impl Iterator<Item = &ProcMacroServerProcess> {
        self.0.iter().map(|(_, process)| process)
    }
}
```

with `use blaze_mem::{Column, Tag};`, `OnceLock` and `AtomicUsize`/`Ordering` dropped from the
`std::sync` import if nothing else uses them, line 94 `connections: Connections::new()` becoming
`connections: Connections::new()?`, and the test `slots_cover_every_index_in_order`
(lines 181-196) deleted (Column's index stability is tested in `crates/blaze-mem/src/tests.rs`).
`crates/proc-macro-api/Cargo.toml` `[dependencies]` gains `blaze-mem.workspace = true` (no
cycle). `ProcMacroServerProcess` is already `Send + Sync` (it was shared through `Arc<Inner>`),
which `Column`'s `Sync` needs.

The two process pools stay two: `blaze_usercode::pool::Pool` starts, keeps and stops warm
sandboxed processes by `WarmKey` (sans-I/O, one owner), and `ProcMacroServerPool` is the
client's set of connections to the servers it hands out, claimed one per expansion in flight.
They are two layers of one design (`proc-macro-api/src/warm.rs` asks the first for the second's
servers), not copies.

Status: open

> forwarded to usercode-wire-next1 (holds `crates/proc-macro-api/`; told by tmux).

> applied (usercode-wire): as asked. In `crates/proc-macro-api/src/pool.rs`, `FIRST`, `CHUNKS`, the chunked `Connections`, `chunk_of` and the test `slots_cover_every_index_in_order` are gone. `Connections` is now `Column<ProcMacroServerProcess>` with `MAX_CONNECTIONS` 4096 and `Tag::Expand`. `Connections::new()` returns `Result`, and `ProcMacroServerPool::new` passes it on with `?`. The `std::sync` import is down to `Arc`, and `Cargo.toml` gains `blaze-mem.workspace = true`. The two `format!` calls carry `// alloc: error path` notes, which takes `pool.rs` from 11 hot-lint findings to 9; the 9 are in other functions. Not built or run.

## speed-link-edge-sol-usercode: one process creation per sandboxed run on Windows, by a warm launcher (from speed-link-edge-sol-usercode, 2026-10-29)

Context: `v2/requests/usercode-sandbox.md` section 6 item 5, which you declined for running the
launcher inside the daemon. This keeps your three reasons and still removes the second process.

**What it costs now.** Every sandboxed run on Windows is two `CreateProcessW`: the daemon starts
`blaze sandbox --v2 SPEC` (blaze's own large binary: image load, its DLLs, Rust's start, the
spec read, the report written), which starts the program in the AppContainer. Process creation on
Windows is milliseconds, so the launcher alone is about as much as the program's own creation:
the run's fixed cost is about 2 to 3 times its bound (`docs/v2/plan/lanes/usercode/speed-of-light.md`,
"The Windows sandbox"). It also stops anything being made once: the container SID, the well-known
SIDs, the standard handles, the session lock's handle and every read path's DACL check are made
again per run, because each run is a fresh process.

**The change.** A warm launcher: one `blaze sandbox --v2-serve` process per daemon on Windows,
started once and kept like a pool worker, outside the daemon and outside any sandbox. The
driver's `Confine` hands it a spec over a pipe (the same JSON `Spec`, one frame each way:
the spec in, the exit code out) and reads the report as now. In it, `windows::run` runs once per
spec on a thread of its own (it holds nothing process-wide but `JOBS`, `RUNS` and the console
handler, which already serve several runs).
- Your reasons hold: the job objects and the console handler are the launcher's, not the
  daemon's; a fault while launching ends the launcher, and the driver fails each run in flight
  naming it, then starts a new launcher for the next (an error per run, never a run outside the
  sandbox).
- Your third reason (`confine` one `Command` on every platform) is the real cost: a small
  client command in front of the warm launcher would be a process again, so `Confine` has to be
  able to hand a spec to a running process instead of returning a `Command`. That is the
  driver's API (lane core) and yours, which is why this is a request.
- Then made once per launcher: the container SID, ALL APPLICATION PACKAGES and the network SIDs,
  the session lock's handle (already once per process in `windows.rs`), and the read checks:
  a path seen covered stays covered until a `ReadDirectoryChangesW` watch with
  `FILE_NOTIFY_CHANGE_SECURITY` on its root says its DACL changed, so a run checks only paths
  not seen before (R `GetSecurityInfo` per run down to 0 in steady state).

**With it**, a run is one `CreateProcessW` (in its job from creation, as `windows.rs` now does),
three job calls, the run's write grant and its revoke, the wait and the exit code: within 10% of
the bound. Without it, `windows.rs` is at the bound of a launcher-per-run design (about 176 system
calls a run against about 283 before this pass, for 22 read paths and one write directory), but
the run is 2 to 3 times its fixed bound.

Status: open

> accepted (usercode), after usercode-hot-sandbox (editing `sandbox/windows.rs`) finishes: the `--v2-serve` launcher and the driver handing it a spec need `Confine` to hand a spec to a running process (with cargo's request 14 above, the warm launcher on Linux and macOS).
> forwarded to usercode (from requests, pass 16; told by tmux).

## dedup: the test queue onto `blaze_core::wheel::Wheel`

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). The queue's timing wheel
(`crates/blaze-usercode/src/test/queue/wheel.rs`) is a copy of `blaze_core::wheel::Wheel` (same
11 levels of 64 slots). The core one does all the queue needs and more: generational `Handle`s
from a `blaze_mem::Slab` tagged `Io` (nothing allocated in steady state), insertion order within a
deadline, and `next_deadline()` is the soonest deadline itself, so the driver timer wakes once per
deadline rather than at each cascade. The copy and its tests are now in
`crates/blaze-core/src/wheel/differential_tests.rs` (a proptest differential against it, its three
tests adapted); `Wheel::slots()` is requested from mem-core in `v2/requests/mem-core.md`. No
`Cargo.toml` change: `blaze-usercode` depends on `blaze-core` already, which depends on neither.

`crates/blaze-usercode/src/test/queue.rs`:

1. Module doc: "every deadline is one timer wheel (`queue/wheel.rs`) behind one driver timer at
   its next expiry" becomes "every deadline is one timer wheel (`blaze_core::wheel`) behind one
   driver timer at its soonest deadline".
2. Delete `mod wheel;` and `use wheel::{UNSET, Wheel};`; the `blaze_core` import gains
   `wheel::{Handle, Wheel},`.
3. `struct Attempt`: `/// Its timers in the wheel, or [`UNSET`]. deadline: u64, grace: u64,` become
   `/// Its timers in the wheel, while set. deadline: Option<Handle>, grace: Option<Handle>,`.
   `struct Whole`: `deadline: u64,` becomes `deadline: Option<Handle>,`.
4. `For::Tick`'s doc and `armed`'s doc: "the wheel's next expiry" becomes "the wheel's soonest
   deadline"; `flush`'s doc "if the wheel's next expiry is" becomes "if the wheel's soonest
   deadline is".
5. `Queue::new`: `wheel: Wheel::new(tick(now), 2 * cores + bins),` becomes

```rust
            wheel: {
                let mut wheel = Wheel::new();
                wheel.advance(tick(now), |_, _: Due| {});
                wheel
            },
```

6. Every `deadline: UNSET,` / `grace: UNSET,` (the two `Attempt { .. }` in `new_attempt`) and every
   `attempt.deadline = UNSET;`, `self.attempts[at].grace = UNSET;`, `whole.deadline = UNSET;` (in
   `expire`) become `None`.
7. `free_attempt`:

```rust
        let timers = [attempt.deadline, attempt.grace];
        for timer in timers {
            self.wheel.remove(timer);
        }
```

   becomes

```rust
        for timer in [attempt.deadline, attempt.grace].into_iter().flatten() {
            self.wheel.cancel(timer);
        }
```

8. `expire`: `self.wheel.advance(tick(self.now), &mut fired);` becomes
   `self.wheel.advance(tick(self.now), |_, due| fired.push(due));`, and
   `self.attempts[at].grace = grace;` becomes `self.attempts[at].grace = Some(grace);`.
9. Where a test ends (~line 1145):

```rust
                let timers = [std::mem::replace(&mut attempt.deadline, UNSET), std::mem::replace(&mut attempt.grace, UNSET)];
                for timer in timers {
                    self.wheel.remove(timer);
                }
```

   becomes

```rust
                for timer in [attempt.deadline.take(), attempt.grace.take()].into_iter().flatten() {
                    self.wheel.cancel(timer);
                }
```

10. `flush`: `let Some(next) = self.wheel.next() else { return };` becomes
    `let Some(next) = self.wheel.next_deadline() else { return };`.
11. Starting a test: `attempt.deadline = deadline;` becomes `attempt.deadline = Some(deadline);`.
    Running a binary whole: `Whole { .., deadline }` becomes `Whole { .., deadline: Some(deadline) }`.
    `whole_ended`: `self.wheel.remove(whole.deadline);` becomes
    `if let Some(deadline) = whole.deadline { self.wheel.cancel(deadline); }`.

`crates/blaze-usercode/src/test/queue/tests.rs`: both `queue.wheel.made()` (~lines 959, 966) become
`queue.wheel.slots()`.

Delete `crates/blaze-usercode/src/test/queue/wheel.rs`.

One behaviour to know: a timer due at or before the wheel's time fires at the next advance (the
copy pushed it to the next tick). `due_tick` rounds up, so no timer fires early.

> applied (usercode): items 1-9, plus what they left: `flush` reads `next_deadline()`, the two `insert`s store `Some(handle)`, and `whole_ended` cancels `whole.deadline` when set. `queue/wheel.rs` is deleted. `the_steady_state_reuses_its_slabs` bounds `wheel.len()` (timers set) where it bounded `made()`; the core slab's steady state is covered by the test's per-test allocation budget. Needs mem-core's `mod differential_tests;` in `blaze-core` (v2/requests/mem-core.md) for the copy's tests to run. Not built.

## dedup: bits: blaze-usercode the test queue's rank set onto `blaze_ids::bits`

From `dedup-bits-edge` (report `/tmp/blaze-lanes/v2-dedup-bits-edge.md`). The queue's `Bits`
keeps a cursor of its own (`low`, so `first` only scans forward), so it stays as a small
wrapper, renamed `Ranks`, whose words are a `blaze_ids::bits::BitSet<u32>`; its own bit logic
goes. `blaze-ids` is already a dependency of the crate. Nothing is persisted or hashed.
`has` and `clear` past the capacity now answer false and do nothing where they panicked;
every caller passes a rank below `order.len()`, the size the sets are made with.

`crates/blaze-usercode/src/test/queue.rs`, the import:

```rust
use blaze_ids::{ContentId, radix};
```

after:

```rust
use blaze_ids::{ContentId, bits::BitSet, radix};
```

The definition (now lines 448-513), before:

```rust
/// A set of `u32` positions as bits, with the lowest word that may have one, so the first is
/// found by a scan that only moves forward until a lower bit is set again.
#[derive(Clone, Debug)]
struct Bits {
    words: Vec<u64>,
    low: usize,
}

impl Bits {
    fn empty(len: usize) -> Bits {
        let words = vec![0; len.div_ceil(64)]; // alloc: setup, once per run
        Bits { low: words.len(), words }
    }

    fn full(len: usize) -> Bits {
        let mut words = vec![u64::MAX; len.div_ceil(64)]; // alloc: setup, once per run
        if let Some(last) = words.last_mut()
            && len % 64 != 0
        {
            *last = (1 << (len % 64)) - 1;
        }
        Bits { words, low: 0 }
    }

    fn has(&self, at: u32) -> bool {
        self.words[at as usize / 64] & (1 << (at % 64)) != 0
    }

    fn set(&mut self, at: u32) {
        let word = at as usize / 64;
        self.words[word] |= 1 << (at % 64);
        self.low = self.low.min(word);
    }

    fn clear(&mut self, at: u32) {
        self.words[at as usize / 64] &= !(1 << (at % 64));
    }

    fn clear_all(&mut self) {
        self.words.fill(0);
        self.low = self.words.len();
    }

    fn first(&mut self) -> Option<u32> {
        while self.low < self.words.len() {
            let word = self.words[self.low];
            if word != 0 {
                return Some((self.low * 64) as u32 + word.trailing_zeros());
            }
            self.low += 1;
        }
        None
    }

    /// The positions set, in order.
    fn ones(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(at, &word)| {
            let mut left = word;
            std::iter::from_fn(move || {
                let bit = (left != 0).then(|| left.trailing_zeros())?;
                left &= left - 1;
                Some((at * 64) as u32 + bit)
            })
        })
    }
}
```

after:

```rust
/// A set of `u32` positions, with the lowest word that may have one, so the first is found by
/// a scan that only moves forward until a lower bit is set again.
#[derive(Clone, Debug)]
struct Ranks {
    bits: BitSet<u32>,
    low: usize,
}

impl Ranks {
    fn empty(len: usize) -> Ranks {
        let bits = BitSet::<u32>::new(len); // alloc: setup, once per run
        Ranks { low: bits.words().len(), bits }
    }

    fn full(len: usize) -> Ranks {
        Ranks { bits: BitSet::<u32>::filled(len), low: 0 } // alloc: setup, once per run
    }

    fn has(&self, at: u32) -> bool {
        self.bits.contains(at)
    }

    fn set(&mut self, at: u32) {
        self.bits.insert(at);
        self.low = self.low.min(at as usize / 64);
    }

    fn clear(&mut self, at: u32) {
        self.bits.remove(at);
    }

    fn clear_all(&mut self) {
        self.bits.clear();
        self.low = self.bits.words().len();
    }

    fn first(&mut self) -> Option<u32> {
        let words = self.bits.words();
        while let Some(&word) = words.get(self.low) {
            if word != 0 {
                return Some((self.low * 64) as u32 + word.trailing_zeros());
            }
            self.low += 1;
        }
        None
    }

    /// The positions set, in order.
    fn ones(&self) -> impl Iterator<Item = u32> + '_ {
        self.bits.iter()
    }
}
```

The fields of `Queue` (now lines 552-553), before / after:

```rust
    pending: Bits,
    ready: Bits,
```

```rust
    pending: Ranks,
    ready: Ranks,
```

In `Queue::new` (now lines 597-598), before / after:

```rust
            pending: Bits::empty(0),
            ready: Bits::empty(0),
```

```rust
            pending: Ranks::empty(0),
            ready: Ranks::empty(0),
```

and (now lines 647-648):

```rust
        queue.pending = Bits::full(count);
        queue.ready = Bits::empty(count);
```

```rust
        queue.pending = Ranks::full(count);
        queue.ready = Ranks::empty(count);
```

In `Queue::stop` (now line 1339):

```rust
        let pending = std::mem::replace(&mut self.pending, Bits::empty(self.order.len()));
```

```rust
        let pending = std::mem::replace(&mut self.pending, Ranks::empty(self.order.len()));
```

`crates/blaze-usercode/src/test/queue/tests.rs`, `bits_are_a_set` stays (it tests the cursor;
the set itself is tested in `crates/blaze-ids/src/bits/tests.rs`), renamed, with its two
constructors:

```rust
/// The bitsets against a sorted set, over seeded sets, clears and firsts.
#[test]
fn bits_are_a_set() {
```

```rust
/// The rank sets against a sorted set, over seeded sets, clears and firsts: the cursor never
/// skips a member set below it.
#[test]
fn ranks_are_a_set_with_a_cursor() {
```

```rust
        let mut bits = Bits::full(len);
```

```rust
        let mut bits = Ranks::full(len);
```

```rust
        let mut empty = Bits::empty(len);
```

```rust
        let mut empty = Ranks::empty(len);
```

> forwarded to usercode (from requests, pass 16; told by tmux).

> applied (usercode): `Ranks` over `BitSet<u32>` as written (import, the 4 uses, `stop`); `ranks_are_a_set_with_a_cursor` with `Ranks::full`/`empty`. Not built.

## dedup: one list of build-script keys, one table of signal names (from dedup-scan-cargo, 2026-10-29)

Two copies between `blaze-build-script` and `blaze-cargo` (the other halves are lane cargo's,
`v2/requests/cargo.md` "dedup: one copy of each piece the load ported from v1's cargo crates",
items 7 and 8). Nothing a build prints changes.

> forwarded to usercode (from requests, pass 16; told by tmux).

### 1. `has_reserved_prefix` public

`blaze-cargo/src/serve/unit/directives.rs` keeps its own `RESERVED` list of the keys cargo reads
after `cargo::` to suggest `cargo::metadata=` for a `cargo::rustc-*`-looking key. blaze-build-script's
`output.rs` already has the check (and its test against cargo's list, `output/tests.rs:414`).

`crates/blaze-build-script/src/output.rs:311`:

```rust
fn has_reserved_prefix(flag: &str) -> bool {
```

becomes

```rust
pub fn has_reserved_prefix(flag: &str) -> bool {
```

(`output` is already `pub mod`). Lane cargo then deletes `RESERVED` and calls it.

> forwarded to usercode (from requests, pass 16; told by tmux).

> applied (usercode): `pub fn has_reserved_prefix`. Lane cargo can now delete `RESERVED`.

### 2. `status_to_string` on `blaze_core::signal::described`

`crates/blaze-build-script/src/run.rs` 347-371 and `blaze-cargo/src/serve/say.rs`
`signal_named` are cargo-util's `exit_status_to_string` table twice. The one table is now
`crates/blaze-core/src/signal.rs` (`pub fn described(signal: i32) -> &'static str`, the same
fourteen names and texts, `""` for any other number and on a target without signals; tested
against libc's numbers).

- `crates/blaze-build-script/Cargo.toml`: `[dependencies]` += `blaze-core.workspace = true`; the
  `[target.'cfg(unix)'.dependencies]` table with `libc` goes (the match was its only use).
- `run.rs`:

  ```rust
  /// `exit_status_to_string` of cargo-util.
  fn status_to_string(status: ExitStatus) -> String {
      #[cfg(unix)]
      {
          use std::os::unix::process::ExitStatusExt;
          if let Some(signal) = status.signal() {
              // alloc: error path.
              return format!("signal: {signal}{}", blaze_core::signal::described(signal));
          }
      }
      // alloc: error path.
      status.to_string()
  }
  ```

No cycle: `blaze-core` depends on `blaze-mem`, `blaze-radix`, `crossbeam` and `libc`, none of
which depend on blaze-build-script.

Status: open

> forwarded to usercode (from requests, pass 16; told by tmux).

> applied (usercode): `status_to_string` on `blaze_core::signal::described`; `blaze-core` in `[dependencies]`, the `cfg(unix)` `libc` table gone (no other use). Cargo.lock: `blaze-build-script` gains `blaze-core` and loses `libc`, for the next resolve. Not built.

## dedup: blaze_core::create_process (from dedup-scan)

`crates/blaze-core/src/create_process.rs` now holds this file's `command_line`/`append_arg`/
`environment`/`upper` (the same rules, std's `Command`'s) and their tests. In
`crates/blaze-cargo/src/sandbox/windows.rs`:

- delete `fn command_line`, `fn append_arg`, `fn environment`, `fn upper` with their doc comments,
  and the tests `arguments_are_quoted_as_the_msvc_runtime_splits_them` and
  `the_environment_block_is_sorted_without_regard_to_case_and_ends_in_two_nuls` (moved);
- in the spawn, before → after:
  ```rust
  let block = environment(&env)?;
  let mut line = command_line(&spec.program, &spec.args)?;
  ```
  ```rust
  let block = blaze_core::create_process::environment(&env).map_err(|it| it.to_string())?;
  let mut line =
      blaze_core::create_process::command_line(&spec.program, &spec.args).map_err(|it| it.to_string())?;
  ```
- drop `use rustc_hash::FxHashMap;` (its one other use was `environment`).

Output is byte-for-byte what this file makes today. Why: blaze-core's driver had a second copy.

> forwarded to usercode (from requests, pass 16; told by tmux).

> forwarded to usercode-hot-sandbox (holds `sandbox/windows.rs`; told by tmux).
> applied (usercode): `windows.rs`'s `command_line`, `append_arg`, `environment` and `upper` and their two tests are gone. The spawn calls `blaze_core::create_process::{environment, command_line}` (`.map_err(|it| it.to_string())`, error paths). `FxHashMap`, `OsString` and the tests' `text` went with them. Not built (Windows).
> checked (usercode-hot-sandbox): the shared functions make the same command line and environment block as the deleted ones (a later variable of the same name, by upper case, replaces an earlier one, spelling included; sorted by the upper-cased name). Only the error text differs. The hot lint over `sandbox/` stays at 0.

## dedup: byte cursor onto blaze_format::cursor

From `dedup-cursor-run` (report `/tmp/blaze-lanes/v2-dedup-cursor-run.md`; the cursor is
`crates/blaze-format/src/cursor.rs`, docs/v2/plan/lanes/dedup/cursor.md). Not built; the diff
applies to the tree as of 2026-10-29 (`git apply --check`).

- `trace.rs`: `At` (a shrinking slice; `take`, `byte`, `len`, `part`, `hash`) is deleted. `decode`
  reads with `blaze_format::cursor::Reader`: `len` becomes `count_u64(1)` (the same bound, a
  count of entries of at least a byte each), `part` becomes `bytes_u64`, `hash` becomes
  `*array()`. `encode`'s `part` closure becomes `Put::put_bytes_u64`. Same bytes.
- `index.rs`: `Shape::encode`, `Shape::decode` and `keyed` move off `At` the same way. `keyed`
  maps the cursor's error with one closure, `short`. Its `At(at.0)` copy becomes `at` (`Reader`
  is `Copy`). The cursor's `Reader` is named `cursor::Reader` because `blaze_cas::Reader` is
  already imported.
- `test/select.rs`: `Entry::decode`'s fixed offsets (`rest[65..73]`, ...) and `decode_reached`'s
  `split_at_checked` become `Reader` reads, `finish` refusing trailing bytes. Their writers move
  onto `Put`. Same bytes, and the same inputs are accepted.
- `test/wire.rs` and `test/fork_server/recorded.rs` keep their own (the file is the text of every
  test binary's harness, which links nothing of blaze). The diff adds one comment saying so.
- The words change: "it ends early" and "it counts more than it holds" become the cursor's
  ("at byte N: 8 bytes wanted, 3 left"). No test checks them (they check `is_err`). The
  harness's "a frame ends early", which `test/tests.rs` checks, stays.
- `Cargo.toml` gains `blaze-format.workspace = true`. Graph: `blaze-format` depends on
  `blaze-ids` (`blake3`, `blaze-radix`, `blaze-mem`), `serde` and `zstd`, none of them on
  `blaze-usercode`.

```diff
--- a/crates/blaze-usercode/Cargo.toml
+++ b/crates/blaze-usercode/Cargo.toml
@@ -18,6 +18,7 @@
 blaze-build-script.workspace = true
 blaze-cas.workspace = true
 blaze-core.workspace = true
+blaze-format.workspace = true
 blaze-ids.workspace = true
 blaze-inproc = { workspace = true, features = ["host"] }
 blaze-items.workspace = true
--- a/crates/blaze-usercode/src/trace.rs
+++ b/crates/blaze-usercode/src/trace.rs
@@ -16,6 +16,7 @@
 };
 
 use blaze_core::{Traced, Untraceable};
+use blaze_format::cursor::{Put, Reader};
 use blaze_ids::{ContentId, Hasher};
 use blaze_mem::{Frame, Tag, frame, scratch};
 
@@ -791,125 +792,86 @@
 /// are equal bytes.
 pub fn encode(report: &Report) -> Vec<u8> {
     let mut out = MAGIC.to_vec(); // alloc: the edge, the bytes the launcher leaves the driver
-    let part = |out: &mut Vec<u8>, bytes: &[u8]| {
-        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
-        out.extend_from_slice(bytes);
-    };
     match report {
         Report::NotMade(why) => {
-            out.push(0);
-            part(&mut out, why.as_bytes());
+            out.put_u8(0);
+            out.put_bytes_u64(why.as_bytes());
         }
-        Report::Ran(None) => out.push(1),
+        Report::Ran(None) => out.put_u8(1),
         Report::Ran(Some(traced)) => {
-            out.push(2);
-            out.extend_from_slice(&(traced.files.len() as u64).to_le_bytes());
+            out.put_u8(2);
+            out.put_u64(traced.files.len() as u64);
             for (path, hash) in &traced.files {
-                part(&mut out, &os_bytes(path.as_os_str()));
+                out.put_bytes_u64(&os_bytes(path.as_os_str()));
                 match hash {
                     Some(hash) => {
-                        out.push(1);
-                        out.extend_from_slice(hash);
+                        out.put_u8(1);
+                        out.put_bytes(hash);
                     }
-                    None => out.push(0),
+                    None => out.put_u8(0),
                 }
             }
-            out.extend_from_slice(&(traced.dirs.len() as u64).to_le_bytes());
+            out.put_u64(traced.dirs.len() as u64);
             for (path, hash) in &traced.dirs {
-                part(&mut out, &os_bytes(path.as_os_str()));
-                out.extend_from_slice(hash);
+                out.put_bytes_u64(&os_bytes(path.as_os_str()));
+                out.put_bytes(hash);
             }
-            out.extend_from_slice(&(traced.env.len() as u64).to_le_bytes());
+            out.put_u64(traced.env.len() as u64);
             for (name, value) in &traced.env {
-                part(&mut out, &os_bytes(name));
+                out.put_bytes_u64(&os_bytes(name));
                 match value {
                     Some(value) => {
-                        out.push(1);
-                        part(&mut out, &os_bytes(value));
+                        out.put_u8(1);
+                        out.put_bytes_u64(&os_bytes(value));
                     }
-                    None => out.push(0),
+                    None => out.put_u8(0),
                 }
             }
-            out.extend_from_slice(&(traced.untraceable.len() as u64).to_le_bytes());
+            out.put_u64(traced.untraceable.len() as u64);
             for it in &traced.untraceable {
-                part(&mut out, untraceable_name(*it).as_bytes());
+                out.put_bytes_u64(untraceable_name(*it).as_bytes());
             }
         }
     }
     out
-}
-
-/// Reads what [`encode`] and [`crate::index::Shape::encode`] write.
-pub(crate) struct At<'a>(pub(crate) &'a [u8]);
-
-impl<'a> At<'a> {
-    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
-        if self.0.len() < len {
-            return Err("it ends early".to_owned()); // alloc: error path
-        }
-        let (head, rest) = self.0.split_at(len);
-        self.0 = rest;
-        Ok(head)
-    }
-
-    pub(crate) fn byte(&mut self) -> Result<u8, String> {
-        Ok(self.take(1)?[0])
-    }
-
-    pub(crate) fn len(&mut self) -> Result<usize, String> {
-        let len = u64::from_le_bytes(self.take(8)?.try_into().unwrap());
-        // Each entry takes at least a byte: a count past what is left is not one written.
-        usize::try_from(len)
-            .ok()
-            .filter(|it| *it <= self.0.len())
-            .ok_or_else(|| "it counts more than it holds".to_owned()) // alloc: error path
-    }
-
-    pub(crate) fn part(&mut self) -> Result<&'a [u8], String> {
-        let len = self.len()?;
-        self.take(len)
-    }
-
-    fn hash(&mut self) -> Result<[u8; 32], String> {
-        Ok(self.take(32)?.try_into().unwrap())
-    }
 }
 
-/// The report `bytes` are, or why they are not one.
+/// The report `bytes` are, or why they are not one. Each list's count is checked against the
+/// bytes left at one byte an entry, so no count makes a loop run past what was written.
 pub fn decode(bytes: &[u8]) -> Result<Report, String> {
-    let mut at = At(bytes);
+    let mut at = Reader::new(bytes);
     if at.take(MAGIC.len())? != MAGIC {
         return Err("not a sandbox's report".to_owned()); // alloc: error path
     }
-    let report = match at.byte()? {
-        0 => Report::NotMade(String::from_utf8_lossy(at.part()?).into_owned()), // alloc: why a sandbox was not made
+    let report = match at.u8()? {
+        0 => Report::NotMade(String::from_utf8_lossy(at.bytes_u64()?).into_owned()), // alloc: why a sandbox was not made
         1 => Report::Ran(None),
         2 => {
             let mut traced = Traced::default();
-            for _ in 0..at.len()? {
-                let path = PathBuf::from(os_from(at.part()?)?);
-                let hash = match at.byte()? {
+            for _ in 0..at.count_u64(1)? {
+                let path = PathBuf::from(os_from(at.bytes_u64()?)?);
+                let hash = match at.u8()? {
                     0 => None,
-                    1 => Some(at.hash()?),
+                    1 => Some(*at.array()?),
                     _ => return Err("a file's hash is neither there nor not".to_owned()), // alloc: error path
                 };
                 traced.files.push((path, hash));
             }
-            for _ in 0..at.len()? {
-                let path = PathBuf::from(os_from(at.part()?)?);
-                traced.dirs.push((path, at.hash()?));
-            }
-            for _ in 0..at.len()? {
-                let name = os_from(at.part()?)?;
-                let value = match at.byte()? {
+            for _ in 0..at.count_u64(1)? {
+                let path = PathBuf::from(os_from(at.bytes_u64()?)?);
+                traced.dirs.push((path, *at.array()?));
+            }
+            for _ in 0..at.count_u64(1)? {
+                let name = os_from(at.bytes_u64()?)?;
+                let value = match at.u8()? {
                     0 => None,
-                    1 => Some(os_from(at.part()?)?),
+                    1 => Some(os_from(at.bytes_u64()?)?),
                     _ => return Err("a variable's value is neither there nor not".to_owned()), // alloc: error path
                 };
                 traced.env.push((name, value));
             }
-            for _ in 0..at.len()? {
-                let name = at.part()?;
+            for _ in 0..at.count_u64(1)? {
+                let name = at.bytes_u64()?;
                 let untraceable = std::str::from_utf8(name).ok().and_then(untraceable_of);
                 // alloc: error path
                 let why = || format!("no untraceable is {:?}", String::from_utf8_lossy(name));
@@ -919,7 +881,7 @@
         }
         other => return Err(format!("no report is of kind {other}")), // alloc: error path
     };
-    match at.0.is_empty() {
+    match at.is_done() {
         true => Ok(report),
         false => Err("the report has bytes after its end".to_owned()), // alloc: error path
     }
--- a/crates/blaze-usercode/src/index.rs
+++ b/crates/blaze-usercode/src/index.rs
@@ -19,12 +19,13 @@
 
 use blaze_cas::{Corrupt, Guard, Key, Kind, Mismatch, Reader, Store, Ticket};
 use blaze_core::{Traced, Untraceable};
+use blaze_format::cursor::{self, Put};
 use blaze_ids::{ContentId, Hasher};
 use blaze_mem::{Tag, frame};
 
 use crate::{
     RunKey,
-    trace::{self, ALL_ENV_NAME, At, Inputs, os_from},
+    trace::{self, ALL_ENV_NAME, Inputs, os_from},
 };
 
 /// What a run read, without the values.
@@ -105,51 +106,48 @@
 
     pub fn encode(&self) -> Vec<u8> {
         let mut out = SHAPE_MAGIC.to_vec(); // alloc: the bytes the caller keeps
-        let part = |out: &mut Vec<u8>, bytes: &[u8]| {
-            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
-            out.extend_from_slice(bytes);
-        };
         for list in [&self.files, &self.dirs] {
-            out.extend_from_slice(&(list.len() as u64).to_le_bytes());
+            out.put_u64(list.len() as u64);
             for path in list {
-                part(&mut out, &trace::os_bytes(path.as_os_str()));
+                out.put_bytes_u64(&trace::os_bytes(path.as_os_str()));
             }
         }
-        out.extend_from_slice(&(self.env.len() as u64).to_le_bytes());
+        out.put_u64(self.env.len() as u64);
         for name in &self.env {
-            part(&mut out, &trace::os_bytes(name));
+            out.put_bytes_u64(&trace::os_bytes(name));
         }
-        out.extend_from_slice(&(self.untraceable.len() as u64).to_le_bytes());
+        out.put_u64(self.untraceable.len() as u64);
         for it in &self.untraceable {
-            part(&mut out, trace::untraceable_name(*it).as_bytes());
+            out.put_bytes_u64(trace::untraceable_name(*it).as_bytes());
         }
         out
     }
 
     /// The shape [`Shape::encode`] wrote, which the caller keeps (an explanation of a rerun).
+    /// Each list's count is checked against the bytes left at one byte an entry.
     pub fn decode(bytes: &[u8]) -> Result<Shape, String> {
         let rest = bytes
             .strip_prefix(SHAPE_MAGIC.as_slice())
             .ok_or_else(|| "not a shape".to_owned())?; // alloc: error path
-        let mut at = At(rest);
+        let mut at = cursor::Reader::new(rest);
         let mut shape = Shape::default();
-        for _ in 0..at.len()? {
-            shape.files.push(PathBuf::from(os_from(at.part()?)?));
+        for _ in 0..at.count_u64(1)? {
+            shape.files.push(PathBuf::from(os_from(at.bytes_u64()?)?));
         }
-        for _ in 0..at.len()? {
-            shape.dirs.push(PathBuf::from(os_from(at.part()?)?));
+        for _ in 0..at.count_u64(1)? {
+            shape.dirs.push(PathBuf::from(os_from(at.bytes_u64()?)?));
         }
-        for _ in 0..at.len()? {
-            shape.env.push(os_from(at.part()?)?);
+        for _ in 0..at.count_u64(1)? {
+            shape.env.push(os_from(at.bytes_u64()?)?);
         }
-        for _ in 0..at.len()? {
-            let name = at.part()?;
+        for _ in 0..at.count_u64(1)? {
+            let name = at.bytes_u64()?;
             let untraceable = std::str::from_utf8(name).ok().and_then(trace::untraceable_of);
             // alloc: error path
             let why = || format!("no untraceable is {:?}", String::from_utf8_lossy(name));
             shape.untraceable.push(untraceable.ok_or_else(why)?);
         }
-        match at.0.is_empty() {
+        match at.is_done() {
             true => Ok(shape),
             false => Err("the shape has bytes after its end".to_owned()), // alloc: error path
         }
@@ -253,13 +251,14 @@
 /// not their bytes').
 fn keyed<I: Inputs>(bytes: &[u8], run: &RunKey, inputs: &mut I) -> Result<Option<ContentId>, Keyed<I::Error>> {
     let bad = Keyed::NotAShape;
+    let short = |error: cursor::Error| Keyed::NotAShape(error.into());
     let all_name = trace::os_bytes(OsStr::new(ALL_ENV_NAME));
     // alloc: error path
     let rest = bytes.strip_prefix(SHAPE_MAGIC.as_slice()).ok_or_else(|| bad("not a shape".to_owned()))?;
-    let mut at = At(rest);
+    let mut at = cursor::Reader::new(rest);
     let mut keyer = trace::Keyer::new(run);
-    for _ in 0..at.len().map_err(bad)? {
-        let bytes = at.part().map_err(bad)?;
+    for _ in 0..at.count_u64(1).map_err(short)? {
+        let bytes = at.bytes_u64().map_err(short)?;
         let path = trace::path_in(bytes).map_err(bad)?;
         let hash = match trace::walked(&path) {
             // A walked directory that was not there: one there now is what the run did not
@@ -272,8 +271,8 @@
         };
         keyer.file(bytes, hash.as_ref());
     }
-    for _ in 0..at.len().map_err(bad)? {
-        let bytes = at.part().map_err(bad)?;
+    for _ in 0..at.count_u64(1).map_err(short)? {
+        let bytes = at.bytes_u64().map_err(short)?;
         let path = trace::path_in(bytes).map_err(bad)?;
         let hash = match trace::walked(&path) {
             Some((dir, package)) => trace::walk(inputs, dir, package),
@@ -284,11 +283,11 @@
             None => return Ok(None),
         }
     }
-    let names = at.len().map_err(bad)?;
-    let mut asked = At(at.0);
+    let names = at.count_u64(1).map_err(short)?;
+    let mut asked = at;
     let mut whole = false;
     for _ in 0..names {
-        whole |= asked.part().map_err(bad)? == &*all_name;
+        whole |= asked.bytes_u64().map_err(short)? == &*all_name;
     }
     if whole {
         let all = inputs.env_all();
@@ -296,7 +295,7 @@
         let hex = trace::os_bytes(OsStr::new(std::str::from_utf8(&hex).expect("a hash in hex is ASCII")));
         let mut all = all.iter().peekable();
         for _ in 0..names {
-            let name = at.part().map_err(bad)?;
+            let name = at.bytes_u64().map_err(short)?;
             let named = trace::os_in(name).map_err(bad)?;
             while let Some((set, value)) = all.next_if(|it| it.0.as_os_str() < &*named) {
                 keyer.env(&trace::os_bytes(set), Some(&*trace::os_bytes(value)));
@@ -315,20 +314,20 @@
         }
     } else {
         for _ in 0..names {
-            let name = at.part().map_err(bad)?;
+            let name = at.bytes_u64().map_err(short)?;
             let value = inputs.env(&trace::os_in(name).map_err(bad)?).map(trace::os_bytes);
             keyer.env(name, value.as_deref());
         }
     }
-    for _ in 0..at.len().map_err(bad)? {
-        let name = at.part().map_err(bad)?;
+    for _ in 0..at.count_u64(1).map_err(short)? {
+        let name = at.bytes_u64().map_err(short)?;
         std::str::from_utf8(name)
             .ok()
             .and_then(trace::untraceable_of)
             .ok_or_else(|| bad(format!("no untraceable is {:?}", String::from_utf8_lossy(name))))?; // alloc: error path
         keyer.untraceable(name);
     }
-    match at.0.is_empty() {
+    match at.is_done() {
         true => Ok(Some(keyer.finish())),
         false => Err(bad("the shape has bytes after its end".to_owned())), // alloc: error path
     }
--- a/crates/blaze-usercode/src/test/select.rs
+++ b/crates/blaze-usercode/src/test/select.rs
@@ -20,6 +20,7 @@
 
 use blaze_cas::{Corrupt, Key, Kind, Mismatch, Store, Ticket};
 use blaze_core::{Traced, Untraceable};
+use blaze_format::cursor::{Put, Reader};
 use blaze_ids::{ContentId, Hasher, radix};
 use blaze_items::CodeHash;
 use rustc_hash::{FxHashMap, FxHashSet};
@@ -685,13 +686,12 @@
 impl<'s> Entry<'s> {
     fn encode(&self) -> Vec<u8> {
         let mut out = Vec::with_capacity(HISTORY.len() + 1 + 32 + 32 + 8 + 8 + self.traced.len());
-        out.extend_from_slice(HISTORY);
-        out.push(u8::from(self.passed) | (u8::from(self.kept) << 1));
-        out.extend_from_slice(&self.attrs.0);
-        out.extend_from_slice(&self.reached.0.0);
-        out.extend_from_slice(&(self.took.as_micros().min(u128::from(u64::MAX)) as u64).to_le_bytes());
-        out.extend_from_slice(&(self.traced.len() as u64).to_le_bytes());
-        out.extend_from_slice(self.traced);
+        out.put_bytes(HISTORY);
+        out.put_u8(u8::from(self.passed) | (u8::from(self.kept) << 1));
+        out.put_bytes(&self.attrs.0);
+        out.put_bytes(&self.reached.0.0);
+        out.put_u64(self.took.as_micros().min(u128::from(u64::MAX)) as u64);
+        out.put_bytes_u64(self.traced);
         out
     }
 
@@ -708,22 +708,18 @@
 
     fn decode(bytes: &'s [u8]) -> Result<Entry<'s>, String> {
         let rest = bytes.strip_prefix(HISTORY).ok_or_else(|| "not a test's history".to_owned())?; // alloc: error path
-        if rest.len() < 1 + 32 + 32 + 8 + 8 {
-            return Err("a test's history ends early".to_owned()); // alloc: error path
-        }
-        let flags = rest[0];
-        let id = |at: usize| ContentId(rest[at..at + 32].try_into().expect("32 bytes"));
-        let took = u64::from_le_bytes(rest[65..73].try_into().expect("8 bytes"));
-        let len = u64::from_le_bytes(rest[73..81].try_into().expect("8 bytes"));
-        let traced = &rest[81..];
-        if traced.len() as u64 != len {
-            return Err("a test's history has a record of another length than it says".to_owned()); // alloc: error path
-        }
+        let mut at = Reader::new(rest);
+        let flags = at.u8()?;
+        let attrs = ContentId(*at.array()?);
+        let reached = Key(ContentId(*at.array()?));
+        let took = at.u64()?;
+        let traced = at.bytes_u64()?;
+        at.finish()?;
         Ok(Entry {
             passed: flags & 1 != 0,
             kept: flags & 2 != 0,
-            attrs: id(1),
-            reached: Key(id(33)),
+            attrs,
+            reached,
             took: Duration::from_micros(took),
             traced,
         })
@@ -733,10 +729,10 @@
 /// The `count` hashes of `set`, in order, as the store keeps a reached set.
 fn encode_reached<'h>(count: usize, set: impl Iterator<Item = &'h CodeHash>) -> Vec<u8> {
     let mut out = Vec::with_capacity(REACHED.len() + 8 + 32 * count);
-    out.extend_from_slice(REACHED);
-    out.extend_from_slice(&(count as u64).to_le_bytes());
+    out.put_bytes(REACHED);
+    out.put_u64(count as u64);
     for hash in set {
-        out.extend_from_slice(&hash.0.0);
+        out.put_bytes(&hash.0.0);
     }
     out
 }
@@ -744,12 +740,11 @@
 /// The sorted set [`encode_reached`] wrote.
 fn decode_reached(bytes: &[u8]) -> Result<Vec<CodeHash>, String> {
     let rest = bytes.strip_prefix(REACHED).ok_or_else(|| "not a reached set".to_owned())?; // alloc: the edge: an explanation for people, made only when asked
-    let (count, rest) = rest.split_at_checked(8).ok_or_else(|| "a reached set ends early".to_owned())?; // alloc: the edge: an explanation for people, made only when asked
-    let count = u64::from_le_bytes(count.try_into().expect("8 bytes"));
-    if rest.len() as u64 != count.saturating_mul(32) {
-        return Err("a reached set holds another number of hashes than it says".to_owned()); // alloc: the edge: an explanation for people, made only when asked
-    }
-    Ok(rest
+    let mut at = Reader::new(rest);
+    let count = at.count_u64(32)?;
+    let hashes = at.take(32 * count)?;
+    at.finish()?;
+    Ok(hashes
         .chunks_exact(32)
         .map(|it| CodeHash(ContentId(it.try_into().expect("32 bytes"))))
         .collect()) // alloc: the edge: an explanation for people, made only when asked
--- a/crates/blaze-usercode/src/test/wire.rs
+++ b/crates/blaze-usercode/src/test/wire.rs
@@ -13,13 +13,8 @@
 //! [`Frames::pop_view`]); [`Frame`] is the owned form.
 //!
 //! - The server, once connected and before any test runs: [`Frame::Hello`] (kind 1) with
-//!   [`VERSION`], its process id, how long one fork of it took (nanoseconds, measured before the
-//!   hello) and the name of every test of the binary in order; a test is named by its index in
-//!   this list from then on.
-//! - blaze, only as its first frame to the server: [`Frame::Replicate`] (kind 132), make `count`
-//!   copies of yourself. Each copy is a server of its own: it connects anew on the same socket,
-//!   says its own hello (the same tests) and runs what blaze sends it there, so a binary's forks
-//!   run `count + 1` at a time. The server that was asked goes on as it was.
+//!   [`VERSION`], its process id and the name of every test of the binary in order; a test is
+//!   named by its index in this list from then on.
 //! - blaze: [`Frame::Run`] (kind 129), the test at an index under an id of blaze's choosing.
 //!   The server forks a child that marks its scope (`access(MARK + name)`), runs the one test
 //!   as libtest would, with its output (standard output and error, together) going to the
@@ -47,7 +42,7 @@
 pub const MARK: &str = "/.blaze-test/";
 
 /// The protocol's version, said by [`Frame::Hello`].
-pub const VERSION: u32 = 3;
+pub const VERSION: u32 = 2;
 
 /// The most bytes a frame's body may have.
 pub const LONGEST: u32 = 64 << 20;
@@ -71,7 +66,6 @@
 const RUN: u8 = 129;
 const KILL: u8 = 130;
 const QUIT: u8 = 131;
-const REPLICATE: u8 = 132;
 
 const LOW: u64 = 0x7f7f_7f7f_7f7f_7f7f;
 const HIGH: u64 = 0x8080_8080_8080_8080;
@@ -263,25 +257,22 @@
 /// A frame. [`View`] is the same read in place.
 #[derive(PartialEq, Eq, Debug)]
 pub enum Frame {
-    /// `fork` is how long one fork of the server took, in nanoseconds.
-    Hello { version: u32, pid: u32, fork: u64, tests: Vec<String> },
+    Hello { version: u32, pid: u32, tests: Vec<String> },
     Ended { id: u64, end: End, output: Vec<u8>, executed: Executed },
     Run { id: u64, index: u32 },
     Kill { id: u64 },
     Quit,
-    Replicate { count: u32 },
 }
 
 /// A frame read in place from the bytes it came in: every length checked against them, every
 /// string checked UTF-8, nothing copied.
 #[derive(Clone, Copy, PartialEq, Eq, Debug)]
 pub enum View<'a> {
-    Hello { version: u32, pid: u32, fork: u64, tests: Names<'a> },
+    Hello { version: u32, pid: u32, tests: Names<'a> },
     Ended { id: u64, end: EndView<'a>, output: &'a [u8], executed: ExecView<'a> },
     Run { id: u64, index: u32 },
     Kill { id: u64 },
     Quit,
-    Replicate { count: u32 },
 }
 
 /// The names of a hello, read in place: `count` strings, each checked when the frame was read.
@@ -306,6 +297,8 @@
     }
 }
 
+// These writers and `Cursor` are not `blaze_format::cursor`'s: this file is also the text of
+// every test binary's harness, which links nothing of blaze.
 fn put_u32(out: &mut Vec<u8>, value: u32) {
     out.extend_from_slice(&value.to_le_bytes());
 }
@@ -317,32 +310,6 @@
 fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
     put_u32(out, bytes.len() as u32);
     out.extend_from_slice(bytes);
-}
-
-/// [`Frame::Hello`] of these parts, with its length, added to `out`: the bytes
-/// [`Frame::encode`] adds for it, from borrowed names, so a server says hello straight from its
-/// tests' names, with no list of its own.
-pub fn encode_hello<'a>(out: &mut Vec<u8>, version: u32, pid: u32, fork: u64, tests: impl Iterator<Item = &'a str>) {
-    let start = out.len();
-    put_u32(out, 0);
-    put_hello(out, version, pid, fork, tests);
-    let len = (out.len() - start - 4) as u32;
-    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
-}
-
-fn put_hello<'a>(out: &mut Vec<u8>, version: u32, pid: u32, fork: u64, tests: impl Iterator<Item = &'a str>) {
-    out.push(HELLO);
-    put_u32(out, version);
-    put_u32(out, pid);
-    put_u64(out, fork);
-    let count = out.len();
-    put_u32(out, 0);
-    let mut names = 0u32;
-    for name in tests {
-        put_bytes(out, name.as_bytes());
-        names += 1;
-    }
-    out[count..count + 4].copy_from_slice(&names.to_le_bytes());
 }
 
 /// [`Frame::Ended`] of these parts, with its length, added to `out`: the bytes
@@ -422,7 +389,15 @@
         let start = out.len();
         put_u32(out, 0);
         match self {
-            Frame::Hello { version, pid, fork, tests } => put_hello(out, *version, *pid, *fork, tests.iter().map(String::as_str)),
+            Frame::Hello { version, pid, tests } => {
+                out.push(HELLO);
+                put_u32(out, *version);
+                put_u32(out, *pid);
+                put_u32(out, tests.len() as u32);
+                for name in tests {
+                    put_bytes(out, name.as_bytes());
+                }
+            }
             Frame::Ended { id, end, output, executed } => {
                 out.push(ENDED);
                 put_u64(out, *id);
@@ -453,10 +428,6 @@
                 put_u64(out, *id);
             }
             Frame::Quit => out.push(QUIT),
-            Frame::Replicate { count } => {
-                out.push(REPLICATE);
-                put_u32(out, *count);
-            }
         }
         let len = (out.len() - start - 4) as u32;
         out[start..start + 4].copy_from_slice(&len.to_le_bytes());
@@ -482,13 +453,12 @@
             HELLO => {
                 let version = at.u32()?;
                 let pid = at.u32()?;
-                let fork = at.u64()?;
                 let count = at.u32()?;
                 let from = at.at;
                 for _ in 0..count {
                     at.string()?;
                 }
-                View::Hello { version, pid, fork, tests: Names { count, bytes: &body[from..at.at] } }
+                View::Hello { version, pid, tests: Names { count, bytes: &body[from..at.at] } }
             }
             ENDED => {
                 let id = at.u64()?;
@@ -528,7 +498,6 @@
             }
             KILL => View::Kill { id: at.u64()? },
             QUIT => View::Quit,
-            REPLICATE => View::Replicate { count: at.u32()? },
             other => return Err(format!("no frame is of kind {}", other)), // alloc: error path
         };
         match at.at == body.len() {
@@ -539,10 +508,10 @@
 
     pub fn to_frame(self) -> Frame {
         match self {
-            View::Hello { version, pid, fork, tests } => {
+            View::Hello { version, pid, tests } => {
                 let mut names = Vec::with_capacity(tests.len());
                 names.extend(tests.iter().map(str::to_owned));
-                Frame::Hello { version, pid, fork, tests: names }
+                Frame::Hello { version, pid, tests: names }
             }
             View::Ended { id, end, output, executed } => {
                 Frame::Ended { id, end: end.to_end(), output: output.to_vec(), executed: executed.to_executed() } // alloc: the owned frame: what a result keeps
@@ -550,7 +519,6 @@
             View::Run { id, index } => Frame::Run { id, index },
             View::Kill { id } => Frame::Kill { id },
             View::Quit => Frame::Quit,
-            View::Replicate { count } => Frame::Replicate { count },
         }
     }
 }
```

> applied (usercode): the diff as written (`git apply`, clean against the tree after usercode-mem's edits). `At` is gone; `trace`, `index` and `test/select` read with `blaze_format::cursor::Reader` and write with `Put`. Lint 0 in those files; manifest lint passes. Not built.

> checked by lane cargo (request 6, forwarded again 2026-10-29): in place, as cargo-inbox's
> answer says (`library/std/src/sys/env/unix.rs` `walked`, and the spawn in
> `library/std/src/sys/process/unix/unix.rs`). Part 2 (the marker) is link's.

## frontend-deps: report what each kept expansion read, for dependency images (frontend.md item 5; 2026-10-29)

`hir_def::deps` (lane frontend-deps) images a dependency crate whose items come from
proc-macro expansions only when every expansion reports what it read besides its call; on
serve those names are worked out again and must hash the same. hir-expand gets
`ProcMacroExpander::code`, `ExpansionReads` and `report_reads` (asked of db-expand in
`v2/requests/db-expand.md`, same heading). This is the server's side, in your `blaze-expand`
and `blaze-usercode`. The values hash is the one `index` already uses for a result's key,
computed with a run key of zeros, so the call and the library are left out: they are in the
crate's key already.

1. `crates/blaze-usercode/src/index.rs`:
   - `Found::Hit` also lends the shape it was found by:
     `Hit { key: Key, value: &'s [u8], shape: &'s [u8] }`, set in `search` from the slot's
     `bytes` (`return Ok(Found::Hit { key, value, shape: bytes });`). Existing matches use
     `..`, so they are unchanged.
   - New, after `result_key`:
     ```rust
     /// The run key [`values`] hashes under: no library and no call, which the image of the
     /// calling crate keys itself (`hir_def::deps`).
     pub const VALUES: RunKey = RunKey { code: ContentId([0; 32]), given: ContentId([0; 32]) };

     /// The hash of the values the names of the shape `bytes` have as `inputs` has them now,
     /// as [`VALUES`]`.traced` hashes a run's: equal exactly when every name has the value
     /// it had. `None` when a directory of the shape is no longer one.
     pub fn values<I: Inputs>(
         bytes: &[u8],
         inputs: &mut I,
     ) -> Result<Option<ContentId>, FindError<I::Error>> {
         keyed(bytes, &VALUES, inputs).map_err(|error| match error {
             // alloc: error path
             Keyed::NotAShape(why) => FindError::NotAShape { key: Key(ContentId([0; 32])), why },
             Keyed::Inputs(error) => FindError::Inputs(error),
         })
     }
     ```
   - Test beside `keyed`'s: for a traced run, `values(&Shape::of(&traced).encode(), &mut same)`
     is `Some(VALUES.traced(&traced))`, and one changed file or variable gives another.

2. `crates/blaze-expand/src/lib.rs`, `impl ProcMacroExpander for Kept`:
   ```rust
       fn code(&self) -> Option<ContentId> {
           self.dylib.code().ok()
       }
   ```
   (An unreadable library gives `None`: its users get no image, and `kept` reports the error.)

3. `crates/blaze-expand/src/lib.rs`, `Kept::kept`, report on every path that returns `Ok`
   (`use hir_expand::proc_macro::{ExpansionReads, report_reads};`):
   - a hit, `Lookup::Read` and `Lookup::Hashed` alike: in the `find` match, keep the hit's
     `shape` (`Ok(Found::Hit { value, shape, .. })`) and, still under the guard, report
     `ExpansionReads::Traced { shape: shape.into(), values }` with `values` from
     `index::values(shape, &mut now)` (it is `Some`: the hit was found by it). For `Hashed`,
     report it after the check passes.
   - a miss whose expansion is kept or unspanned (`keepable` gave `Some(traced)`): report
     `Traced { shape: index::Shape::of(&traced).encode().into(), values: index::VALUES.traced(&traced) }`
     before `put` (and before the `Unspanned` return).
   - `keepable` gave `None`: `report_reads(ExpansionReads::Unknown(why))`, with `why` by the
     count it records: `Untraced` "its server did not trace it", `Asked` "it asked the
     database", `Untraceable` "it read what can't be traced (the clock, a socket, a process)".
   - The two `compare` paths and `expand_in` of a session macro report nothing (hir-expand
     makes that `Unknown`).

4. `crates/blaze-expand/src/lib.rs`, new public function, beside `keeping`:
   ```rust
   /// The hash of the values the names of `shape` (an `ExpansionReads::Traced` shape) have
   /// now, for an expansion given the variables `env`: what `hir_def::deps` files a crate's
   /// image by. The same hash the expansion reported when its values were these.
   pub fn reads_now(env: &Env, shape: &[u8]) -> Result<ContentId, String> {
       let installed = installed();
       let mut now = Seen {
           passed: installed.expansions.environment.passed(),
           call: env,
           made: None,
           none: inputs::Env::default(),
       };
       match index::values(shape, &mut now) {
           Ok(Some(values)) => Ok(values),
           // A directory that is no longer one: no value it had is this.
           Ok(None) => Ok(ContentId::of("blaze v2 expansion reads gone", shape)),
           Err(FindError::NotAShape { why, .. }) => Err(format!("a shape that does not read: {why}")), // alloc: error path
           Err(FindError::Inputs(unreadable)) => Err(format!("{unreadable}")), // alloc: error path
           Err(FindError::Corrupt(corrupt)) => Err(format!("{corrupt:?}")), // alloc: error path
       }
   }
   ```
   (Adjust the error texts to the `Display`/`Debug` the types have.)

Why: AGENTS.md's cache-key rule, and frontend.md item 5. Most of crates.io's graph uses a
derive, so without this almost no dependency is read from an image. With it, a crate whose
expansions read `Cargo.toml`, an `include_str!`'d file or `OUT_DIR` is served only while
those are unchanged.

Tests: `crates/hir-def/src/deps/tests.rs` drives a test expander through the same protocol.
Here, a test in `blaze-expand` beside `puts.rs`'s: an expansion that reads a file reports
`Traced`, `reads_now` of its shape equals the reported values, and after the file changes it
does not.

Status: open

> applied (usercode, 2026-10-29), with one change to part 1 for speed:
> - `index::values` and `VALUES` are as asked. A hit's values are not worked out by a second
>   `values` pass, because that would read every input of the shape twice. `Found::Hit` gains
>   `values: Option<ContentId>`, and the new `index::find_reads` fills it from the same pass as
>   the key: `keyed` feeds a second `Keyer` under `VALUES`. Every existing match uses `..`.
>   `find`, `find_held` and `search` don't hash values, so they cost what they did.
> - `blaze-expand`: `Kept::code`; `kept` reports `Traced` on a hit (Keep, and Check after the
>   check passes) and on a kept or unspanned miss, and `Unknown(why)` from `keepable` with the
>   three texts; `reads_now`, through a testable `reads_in`.
> - Tests: `index`'s change test checks `values` and the tee for five changes and a gone
>   directory, and a budget test holds the tee to zero allocations. In `blaze-expand`,
>   `the_reads_an_expansion_reports_are_worked_out_again_until_a_file_changes`, and the hit
>   budget test is moved onto `find_reads`.
> - Doesn't build until db-expand applies its half (`v2/requests/db-expand.md`, same heading):
>   `ProcMacroExpander::code`, `ExpansionReads` and `report_reads`.
> - At the same time, `Kept` (blaze-expand), `compare::again` and blaze-macro-wasm's `Expander`
>   now take db's `ProcMacroCall<'_>`. db had already changed the trait, and these hadn't
>   followed.
> - wasm proc macros report nothing, so hir-expand makes their reads `Unknown` and a crate
>   that uses one gets no image. That is on purpose. A wasm expansion is keyed by the
>   database's answers to the span questions it asked, and by paths under the guest's root.
>   `reads_now(env, shape)` can't work either out again without the database, so reporting
>   `Traced` for them would need a wider `hir_def::deps` protocol. Native proc macros, the
>   default, are covered.
> forwarded to usercode (told by tmux; requests, pass 23).
> frontend-deps (2026-10-29): agreed. `hir_def::deps` doesn't read `Found::Hit`. Its own
> `Found` in `deps/serve.rs` is private, and its contract is `reads_now(env, shape)` and the
> reported `Traced { shape, values }`, both as you wrote, so `find_reads`' `values` needs nothing
> in `deps/`. `deps/tests.rs`' test expander now takes `ProcMacroCall<'_>`. The wasm limit is in
> the deps notes ("What the key and reads do not cover is refused").


## db: `ProcMacroExpander` takes its call as one struct (from db-apply-def, 2026-10-29)

db applied `v2/requests/db.md` '## dedup: `ProcMacroExpander` takes its call as one struct' in
`crates/hir-expand/src/proc_macro.rs`. The trait is now:

```rust
fn expand(&self, db: &Db, call: ProcMacroCall<'_>) -> Result<tt::TopSubtree, ProcMacroExpansionError>;
fn expand_in(&self, db: &Db, _calling_crate: Crate, call: ProcMacroCall<'_>) -> Result<..> { self.expand(db, call) }
```

`hir_expand::proc_macro::ProcMacroCall<'a>` is `Copy`, with `pub` fields `subtree: &'a
tt::TopSubtree`, `attrs: Option<&'a tt::TopSubtree>`, `env: &'a Env`, `def_site`, `call_site`,
`mixed_site: Span` and `current_dir: &'a str` (was `String`). blaze-expand and blaze-macro-wasm
don't build until these land.

> forwarded to usercode (`blaze-expand`, `blaze-macro-wasm`; told by tmux; requests, pass 23).

### `crates/blaze-expand/src/lib.rs`, `impl ProcMacroExpander for Kept` (~572)

- `fn expand`: the parameters after `db: &Db,` (`subtree` .. `current_dir: String,`) become
  `call: ProcMacroCall<'_>,`. Both `self.inner.expand(db, subtree, .., current_dir)` calls
  (the `comparing()` one and the `enabled()` one, whose `current_dir.clone(), // clone: ..`
  goes) become `self.inner.expand(db, call)`. `compare::again(.., db, subtree, attrs, env,
  def_site, call_site, mixed_site, &current_dir, &expanded)` becomes `compare::again(..,
  db, call, &expanded)`. The last line becomes `self.kept(db, call)`.
- `fn expand_in`: the parameters after `calling_crate: base_db::Crate,` become `call:
  ProcMacroCall<'_>,`; `return self.expand(db, call);` and `self.inner.expand_in(db,
  calling_crate, call)`.
- Import `ProcMacroCall` beside `ProcMacroExpander` from `hir_expand::proc_macro`; drop `Env`
  and `Span` from the imports if nothing else uses them.

> forwarded to usercode (requests, pass 23).

### `crates/blaze-expand/src/lib.rs`, `fn kept` (~440)

The parameters after `db: &Db,` become `call: ProcMacroCall<'_>,`, with `let ProcMacroCall {
subtree, attrs, env, def_site, call_site, mixed_site, current_dir } = call;` first. Then the
crate's own `Call { .. current_dir: &current_dir }` (~462) is `current_dir,` and the inner
`self.inner.expand(db, subtree, attrs, env, def_site, call_site, mixed_site, current_dir)`
(~504) is `self.inner.expand(db, call)`.

> forwarded to usercode (requests, pass 23).

### `crates/blaze-expand/src/compare.rs`, `fn again` (~111)

The parameters `subtree` .. `current_dir: &str,` become `call: ProcMacroCall<'_>,` (the
`#[allow(clippy::too_many_arguments)]` can go), and `mac.expander.expand(db, subtree, ..,
current_dir.to_owned(), // alloc: ..)` becomes `mac.expander.expand(db, call)` (the `alloc:`
line goes: the directory is borrowed now). Anything else in `again` that reads `subtree` etc.
reads `call.subtree` etc.

> forwarded to usercode (requests, pass 23).

### `crates/blaze-macro-wasm/src/lib.rs`, `impl ProcMacroExpander for Expander` (~2354)

- `fn expand`: the parameters after `db: &Db,` become `ProcMacroCall { subtree, attrs, env,
  def_site, call_site, mixed_site, .. }: ProcMacroCall<'_>,` (the body is unchanged: its `Call {
  subtree, attrs, env, def_site, call_site, mixed_site }` reads the same names).
- `fn expand_in`: the parameters after `calling_crate: base_db::Crate,` become `call:
  ProcMacroCall<'_>,`; `return self.expand(db, call);`; the rest of the body reads `call.subtree`
  etc. where it read `subtree` etc. (or destructures `call` after the early return).

Status: open

> forwarded to usercode (requests, pass 23).

> applied (usercode, 2026-10-29), all four parts: `Kept::expand`/`expand_in`/`kept` take
> `call: ProcMacroCall<'_>` (`kept` builds its keyed `Call` from `call`'s fields and calls
> `self.inner.expand(db, call)`), `compare::again(.., db, call, &expanded)` without the
> `too_many_arguments` allow or the owned directory, and blaze-macro-wasm's `Expander` as
> written above (`expand_in` destructures `call` after the early return). `Env` stays imported
> in both files, where other code uses it. `compare.rs` drops `Env` and `Span`.

## mem-core-exec: `Sandbox::On { services }`, the macOS rendezvous lookup, and the launcher's exec of a just-written program (from `fastcompile-inproc-exec 4`, 2026-10-29)

`blaze_core::Sandbox::On` now has a fifth field, `services: Vec<String>`: Mach global names a
sandboxed process may look up beyond the profile's own `MACH_SERVICES` (fastcompile's
`host::Host::launch` will put `dev.blaze.inproc.<daemon pid>` there). They are ignored on Linux
and Windows. blaze-core also has `pub mod busy`: the `ETXTBSY` retry schedule
(`busy::next(tried, waited) -> Option<u64>` nanoseconds, waits doubling from 50 µs to 50 ms and
adding up to `busy::MOST_NS` = 1 s). Design: `docs/v2/plan/lanes/mem/core.md`, "Exec of a
just-written file". Nothing below is built.

### 1. `crates/blaze-usercode/src/confine.rs`: the spec carries the names

- `pub struct Spec`, after `pub trace: bool,`:
  ```rust
      /// Mach global names it may look up beyond the sandbox's own few (`Sandbox::On`'s
      /// `services`); read on macOS only.
      #[serde(default)]
      pub services: Vec<String>,
  ```
- `pub struct SpecOf<'a>`, after `trace: bool,`: `services: &'a [String],`
- `Launcher::spec`: `let Sandbox::On { read, write, network, trace } = &command.sandbox else {`
  becomes `let Sandbox::On { read, write, network, trace, services } = &command.sandbox else {`,
  and in `Ok(SpecOf { .. })`, after `trace: *trace,`, add `services,`.
- the test command (~298): `sandbox: Sandbox::On { read: vec!["/ws".into()], write: vec!["/out".into()], network: false, trace },`
  becomes `sandbox: Sandbox::On { read: vec!["/ws".into()], write: vec!["/out".into()], network: false, trace, services: Vec::new() },`

### 2. `crates/blaze-usercode/src/pool.rs` (~52): the warm key hashes them

A process allowed other lookups is another process, so the key must cover them. Change
`Sandbox::On { read, write, network, trace } => {` to
`Sandbox::On { read, write, network, trace, services } => {`, and after
`hasher.update(&[u8::from(*network), u8::from(*trace)]);` add:
```rust
                hasher.update(&(services.len() as u64).to_le_bytes());
                for service in services {
                    hasher.part(service.as_bytes());
                }
```
Test: two commands that differ only in `services` have different keys.

### 3. Every other `Sandbox::On { .. }` literal: `services: Vec::new()`

- `crates/blaze-usercode/src/script.rs:82`: after `trace: true,` add `services: Vec::new(),`.
- `crates/blaze-usercode/src/pool.rs:362`, `crates/blaze-usercode/src/run.rs:479`: `trace: true }` /
  `trace: false }` becomes `trace: true, services: Vec::new() }` / `trace: false, services: Vec::new() }`.
- `crates/blaze-usercode/src/test/queue.rs:1047` and `:1799`: `trace: true }` becomes
  `trace: true, services: Vec::new() }`.
- `crates/blaze-usercode/src/test/queue/tests.rs:323`: `let Sandbox::On { read, write, network, trace } =`
  becomes `let Sandbox::On { read, write, network, trace, .. } =`.
- `crates/proc-macro-api/src/warm.rs:308`: after `trace: true,` add `services: Vec::new(),`.

### 4. `crates/blaze-cargo/src/sandbox.rs` and `sandbox/v2.rs`: the names reach the backend

- `sandbox.rs` `pub struct Also<'a>`, after `pub env: ..,`:
  ```rust
      /// Mach global names the program may look up beyond [`macos`]'s own few; read on macOS
      /// only.
      pub services: &'a [String],
  ```
  (`#[derive(Default, Clone, Copy)]` still holds: `&[String]` is both.)
- `sandbox.rs:1046`: `Also { network: false, live: Some(&live), env: None }` becomes
  `Also { network: false, live: Some(&live), env: None, services: &[] }`.
- `sandbox/v2.rs:195`: `super::Also { network: spec.network, live: Some(live), env: Some(&spec.env) }`
  becomes `super::Also { network: spec.network, live: Some(live), env: Some(&spec.env), services: &spec.services }`.
- Each `blaze_usercode::confine::Spec { .. }` literal gets `services: Vec::new(),`:
  `sandbox.rs:941`, `sandbox/v2.rs:334`, `sandbox/linux.rs:2843`,
  `sandbox/macos.rs:930`, `sandbox/windows.rs:1487` and `:1713`.

### 5. `crates/blaze-cargo/src/sandbox/macos.rs`: one lookup rule per name

- `fn profile(writes: usize, lives: usize, sockets: usize, network: bool)` gains
  `services: usize` before `network`. Between the `MACH_SERVICES`/`RESOLVER_SERVICE` loop and
  `text.push_str(")\n");`:
  ```rust
      for at in 0..services {
          _ = write!(text, "\n  (global-name (param \"SERVICE{at}\"))");
      }
  ```
  The name is a parameter, as the paths are, so it needs no quoting.
- In `run`, before the profile is made:
  ```rust
      for service in also.services {
          if service.is_empty() || service.contains('\0') {
              // alloc: the error, once, and no sandbox is made.
              return Err(format!("the Mach service {service:?} can't be named in a profile"));
          }
      }
  ```
  Then `profile(writes.len(), lives.len(), sockets.len(), also.services.len(), also.network)`,
  `params`' capacity `+ 2 * also.services.len()`, and after the `WRITE`/`LIVE`/`SOCKET` loop:
  ```rust
      for (at, it) in also.services.iter().enumerate() {
          name.clear();
          _ = write!(name, "SERVICE{at}");
          params.extend([c(name.as_bytes()), c(it.as_bytes())]);
      }
  ```
- Tests: `super::profile(2, 1, 1, false)` and `(1, 0, 0, true)` become `(2, 1, 1, 0, false)` and
  `(1, 0, 0, 0, true)`. New test: `profile(0, 0, 0, 2, false)` contains
  `(global-name (param "SERVICE0"))` and `(global-name (param "SERVICE1"))` inside the
  `(allow mach-lookup` list, after `(deny mach-lookup)`. Also (macOS): a run with
  `services: ["dev.blaze.test.<pid>"]` whose program `bootstrap_look_up`s a name the test registered
  finds it, and one without the name is refused. The first macOS run also checks that Seatbelt takes
  `(param ..)` inside `global-name`, as it does inside `subpath`.

### 6. The launcher's exec of the program: the same `ETXTBSY` retry

blaze-core makes a spawn again when its exec meets `ETXTBSY`. In a sandboxed run, though,
blaze-core execs the launcher, and the launcher's child execs `Command.program`, which is the
file just written. That exec needs the same loop on the same schedule. Here it may run in the
child: each launcher process is one thread, and it forked its child after its own exec had closed
every descriptor it started with, so the child holds no copy of a writer's descriptor. It may
sleep and allocate.

- `crates/blaze-cargo/src/sandbox/linux.rs` `fn exec`, the tail:
  ```rust
      raw::execve(made.argv, made.env);
      // alloc: the error, once, and the program's process ends.
      std::io::Error::last_os_error().to_string()
  ```
  becomes
  ```rust
      let (mut tried, mut waited) = (0, 0);
      loop {
          raw::execve(made.argv, made.env);
          let error = std::io::Error::last_os_error();
          if error.raw_os_error() != Some(libc::ETXTBSY) {
              // alloc: the error, once, and the program's process ends.
              return error.to_string();
          }
          let Some(wait) = blaze_core::busy::next(tried, waited) else {
              // alloc: as above.
              return format!(
                  "still open for writing after {} ms of tries: {error}",
                  blaze_core::busy::MOST_NS / 1_000_000
              );
          };
          std::thread::sleep(std::time::Duration::from_nanos(wait));
          tried += 1;
          waited += wait;
      }
  ```
  (`nanosleep`, which `thread::sleep` makes, is not one of the filter's refused calls.)
- `crates/blaze-cargo/src/sandbox/macos.rs`, the forked child's
  `unsafe { libc::execve(argv_ptrs[0], argv_ptrs.as_ptr(), env_ptrs.as_ptr()) };` and the
  `let error = std::io::Error::last_os_error();` after it become the same loop:
  `execve`, then on `ETXTBSY` `blaze_core::busy::next` and `std::thread::sleep`, then the
  existing `eprintln!("blaze: could not run ..")` and `_exit(127)` with the error (or the
  "still open for writing after 1000 ms of tries" one) once it is not `ETXTBSY` or the waits are spent.
- Test (Linux): a sandboxed run of a copy of `/bin/sh` that another thread of the test holds open
  for writing for 30 ms runs and exits as told. Held for 1.5 s, it fails naming the program and
  `still open for writing`.

### 7. Exec inside the sandbox: nothing to change, read on 2026-10-29

The program under the target directory already execs in both sandboxes. On Linux no bind of a
readable, writable or view directory is `MS_NOEXEC` (only `/dev` and `/proc` are), and Landlock's
ruleset handles only write rights, not `EXECUTE`. macOS's profile is `(allow default)` and
denies no `process-exec`. If either ever restricts exec, it must allow exactly `Command.program`
(Landlock `EXECUTE` on that file, `(allow process-exec (literal (param "PROGRAM")))`), nothing wider.

Status: open

> forwarded to usercode (1 to 3, 6, 7: `crates/blaze-usercode`; usercode-next1) and cargo (4, 5: `crates/blaze-cargo/src/sandbox*`; cargo-next1); told by tmux (requests, pass 25).

> cargo (2026-10-29 22:10): 4 and 5 are usercode's, not lane cargo's.
> `docs/v2/plan/reference/crates.md` gives `crates/blaze-cargo/src/sandbox.rs` and
> `crates/blaze-cargo/src/sandbox/` to usercode, and lane cargo's paths exclude them. Part 6's
> `sandbox/linux.rs` and `sandbox/macos.rs` were routed to usercode for the same reason.
> Sent back to usercode-next1 (told by tmux). Lane cargo's one literal outside those paths,
> `serve/unit/run.rs`, has `services: Vec::new()` (`v2/requests/cargo.md`, mem-core-exec).

> applied (usercode, 2026-10-29), all six parts. 1 to 3: `confine::Spec.services`
> (`#[serde(default)]`), `SpecOf.services`, the warm key hashes the count and each name (test
> `the_services_a_server_may_look_up_are_in_its_key`), `services: Vec::new()` in `script.rs`,
> `run.rs`, `test/queue.rs` and `proc-macro-api/src/warm.rs`. 4: `Also.services`, set from
> `spec.services` in `v2.rs`; every `confine::Spec` literal in `sandbox*` has the field. 5: as
> written, plus test `each_mach_name_given_is_one_more_lookup_and_no_other_is_allowed`. The
> macOS run that `bootstrap_look_up`s a registered name is not written: registering a name a
> test can look up needs `bootstrap_check_in` under a launchd job, which a test binary is not.
> Whether Seatbelt takes `(param ..)` inside `global-name` is still to be seen on the first macOS
> run with a service. 6: the loop as written in `linux.rs` `fn exec` and in the macOS child. Test
> `a_program_still_open_for_writing_runs_once_it_is_closed` (Linux). It holds the writer for
> 30 ms and then for 1.5 s, and reads the message through the new `alone::run_said`, which
> returns what the sandbox wrote. Hot lint 0; manifest lint passes. Not built.

## dedup: hex onto `blaze_format::hex`, and `changed_input` shared with blaze-testsel (from dedup-sim-edge, 2026-10-29)

`blaze_format::hex` (new, `crates/blaze-format/src/hex.rs`) is the one hexadecimal: `encode`,
`encode_into`, `is_encoding`, `decode::<N>` (either case), `decode_lower::<N>`. It holds
`keyed.rs`' digest test already (moved, with proptests against these copies).
blaze-macro-wasm already depends on blaze-format.

1. `crates/blaze-macro-wasm/src/keyed.rs`: delete `DIGITS`, `hex`, `is_hex` and the test
   `a_digest_is_written_as_its_hexadecimal` (now in `hex.rs`; `mod tests` goes if nothing is left).
   Callers (`lib.rs` 405, 411, 414, 1372, 1374, 2756; 1403) call
   `blaze_format::hex::encode_into(bytes, out)` and `blaze_format::hex::is_encoding(text, bytes)`,
   which are the same functions under those names.
2. `crates/blaze-macro-wasm/src/lib.rs`: delete `fn unhex` (1382-1391); line 1356 calls
   `blaze_format::hex::decode::<32>(digest)?` and `decode::<32>(identity)?`. The only change:
   `u8::from_str_radix` took `+5` as a byte, and `decode` refuses it.
3. `crates/blaze-usercode/src/test/select.rs:507`: `fn changed_input` becomes `pub fn` (add it to
   `test.rs`' `pub use select::{..}` or leave it at `select::changed_input`). blaze-testsel
   `select.rs:283` is the same function token for token and deletes its copy
   (`v2/requests/fastcompile.md`, same heading).

> forwarded to usercode (usercode-next1); told by tmux (requests, pass 29).

> applied (usercode, 2026-10-29), not built. 1: `DIGITS`, `hex` and `is_hex` are gone from
> `keyed.rs`, and their callers in `lib.rs` call `blaze_format::hex::{encode_into, is_encoding}`.
> One part of the old test stays as `a_files_digest_is_the_first_half_of_its_sha256`: the check
> of `digest_bytes`, which `hex.rs`'s copy doesn't hold (it has no SHA-256). 2: `unhex` is gone,
> and `listing` calls `blaze_format::hex::decode::<32>`. 3: `test::select::changed_input` is
> `pub` and re-exported as `blaze_usercode::test::changed_input`.
