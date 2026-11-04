# Handoff: panics and fatal crashes as rows (branch pd-crashes)

Nothing was built, checked or tested: the main agent runs `blaze check` and `blaze test` once. Not
committed.

## Files changed

New:
- `crates/blaze-profile/src/stack.rs`: the one frame-pointer walker (`walk`), shared by the macOS
  sampler, the panic hook and the signal handler. Also `frame_pointer()` (asm), `ready()`/`probe()`
  (a pipe-based readability probe), `here()` (walks the calling thread).
- `crates/blaze-profile/src/panics.rs`: the panic hook (`record(caught)`), the preallocated ring
  (64 slots), `claim()` (what the plane uses for `Panic::at`), `last_of_thread()` (for the signal
  handler), `since()`/`rows()` (history drain), `PanicRow`, `batch()`, `top_frames()`, `Kind`.
- `crates/blaze-profile/src/crash.rs`: fatal-signal handler (`install()`), crash record
  (`arm(dir)`), page layout, `read()`, `recover(dir)`, `of_process(dir, pid)`, `CrashError`.

Edited:
- `blaze-profile/src/lib.rs`: `pub mod crash; pub mod panics; mod stack;`.
- `blaze-profile/src/schema.rs`: `Type::Pcs` (list<u64>), `PANICS` table, `TABLES` 23 -> 24.
- `blaze-profile/src/arrow.rs`: `Type::Pcs` -> `List<UInt64 "pc", non-null>`.
- `blaze-profile/sql/views.sql`: `panics` view.
- `blaze-profile/src/history.rs` (kept local): `alive` made `pub(crate)`; `dictionary()` arm for
  `Pcs`; `History` fields `panics_on, panicked, raised, crashes`; `open()` runs
  `crash::recover(dir)` (warns each refusal, raises `last_build` past recovered build ids);
  `record_panics()` and `refused_crashes()`; `write()` drains panics, adds the `panics` table (13 ->
  14 tables), counts `Flushed::panics`, adds panics' skipped to `skipped`, names the flush past panic
  build ids, deletes recovered crash files once written.
- `blaze-profile/src/flight.rs`: `open_of_thread(tid, each)` (AS-safe walk of a live thread's
  open spans).
- `blaze-profile/src/identity.rs`: `binary_hash()` (cached `[u8; 32]`), `hex_of()`; `binary()`
  uses them.
- `blaze-profile/src/symbols.rs`: `names_slid(pcs, slide)`.
- `ix-prof/src/symbols.rs`: `names_slid`, `own_slide`, `own_load_base`.
- `blaze-profile/src/sampler/macos.rs`: walk now goes through `stack::walk`; `ADDRESS` from stack.rs.
- `blaze-profile/src/slow.rs`, `slow/load.rs`, `slow/render.rs`: `BuildFacts::panics:
  Vec<PanicFacts>`, loaded from `panics` (the build's rows, plus the `signal` row of the daemon
  that died in it), printed after the died block with top frames. `VERSION` 4 -> 5.
- `blaze-cargo/src/serve/plane.rs`: the duplicate `RAISED` store, `RAISED_KEPT`,
  `set_recording_hook` and the `parking_lot::Mutex` import are gone. `record_panics()` now calls
  `blaze_profile::panics::record(in_a_job)`; `panicked()` uses `blaze_profile::panics::claim`.
- `blaze-cargo/src/serve.rs`: `blaze_profile::crash::install()` right after
  `signals::record_ends()`.
- `blaze-cargo/src/serve/observe.rs`: after `History::open` succeeds, `history.record_panics()`
  and `blaze_profile::crash::arm(&at)` (warn plus eprintln on failure, like the sampler).
- `blaze-cargo/src/cli/daemon.rs`: `Died::crash: Result<Option<PanicRow>, String>`, read in place
  with `crash::of_process`. `Display` prints the signal, thread, last panic and top 12 frames, plus
  the open spans. All 6 constructors are updated.
- `docs/profiling.md`: a `### panics` section (columns, ring, crash record, recovery, refusal),
  the `blaze slow` paragraph, and the summary front matter (status kept as `plan`).

## Design

- **One table, `panics`, with `kind` = `panic | signal`.** A fatal signal is a panic the process
  could not survive. The questions are the same for both (thread, open spans, frames, build). An
  abort that a panic led to is one signal row, and its message includes the thread's last panic
  message and location. Signal-only columns (`signal`, `fault_addr`) are null for panics.
- **Raw addresses plus image identity (coordinator's request).** Each row keeps the named `stack`
  (list<dict>, outermost first) and also `stack_pcs list<u64>` (same frames, outermost first),
  `image_hash` (BLAKE3 hex of the executable, `identity::binary_hash`) and `image_base` (load
  address, `ix_prof::symbols::own_load_base`). That is the key shape of the `images` table the
  other lane is adding (path, build id, BLAKE3, load base), so the frames can be named again later.
  `Type::Pcs` is new in schema.rs/arrow.rs. If the images lane adds its own list<u64> type, merge
  the two into one variant.
- **The panic ring (flight-recorder pattern).** 64 fixed slots, each a seqlock of atomics. The
  writer sets seq=2n+1, fences with Release, does Relaxed stores, then sets seq=2n+2 with Release.
  Text fields are atomic u64 words, so the hook does no allocation and takes no lock. It records
  message (cut at 1 KiB, marked `...`), location, thread name, the stack (frame-pointer walk,
  probed), open spans, job, build and `caught` (the plane's `in_a_job` at hook time). The history
  drains with a cursor (`since`). `claim` replaces the plane's RAISED matching: newest unclaimed
  slot with the same message, claimed once.
- **The probe.** The hook and the handler don't know the stack bounds (fibers, altstack). Before
  each frame record is read, `write(pipe, addr, 16)` checks it: EFAULT means unmapped, and the
  bytes are drained after. This is async-signal-safe. The sampler keeps its own bounds check.
- **The crash record.** One 4096-byte page, fixed little-endian layout, mapped `MAP_SHARED` from
  `<history>/crash.<pid>`:
  - 0: magic `BLZCRSH\0`
  - 8: layout u32 (1)
  - 12: size u32
  - 16: BLAKE3 of the executable [32]
  - 48: pid
  - 56: load slide
  - 64: load base
  - 72: state u64 (0 armed, 1 writing, 2 whole)
  - 80: signal
  - 84: si_code
  - 88: fault address
  - 96: t_ns
  - 104: tid
  - 112: build
  - 120: innermost span
  - 128: depth
  - 136: pcs[64]
  - then the thread name, 16 open-span names, the last panic's message (1 KiB) and its location
    (256 bytes).

  The layout is position-independent and fits in a page, so it can be placed as a region of the
  other lane's mmap'd flight-ring file, with the same `read()`. The handler takes the record with a
  CAS on state 0 -> 1, so only the first fatal signal of a process is recorded. Depth is updated
  after each pc is stored, so a fault inside the walk leaves a partial record. Recovery reads state
  1 as "cut short" and says so in the message.
- **The handler.** `SA_SIGINFO | SA_ONSTACK | SA_RESETHAND | SA_NODEFER` on SEGV, BUS, ILL, FPE and
  ABRT. It is installed after `signals::record_ends` and saves the previous action. If the calling
  thread has no sigaltstack it gets a 64 KiB mmap'd one (std gives each thread it spawns one).
  Steps:
  1. Read pc/fp from the ucontext:
     - macOS arm64: hand-written `__darwin_ucontext`/`__darwin_mcontext64` structs, because libc
       has no apple ucontext_t.
     - Linux aarch64: `regs[29]`/`pc`.
     - Linux x86_64: `REG_RBP`/`REG_RIP`.
     - Other targets: no stack.
  2. Write the record.
  3. Chain to the previous handler (SA_SIGINFO style or plain). In the daemon that is
     `signals::ended` (log line, then raise) or `faulted` (log line, then std's handler).
  4. If the previous action was DFL or IGN, `raise(signal)`. RESETHAND already made the action the
     default, so this ends the process with the same signal.
- **Recovery.** `History::open` scans `crash.<digits>`, skipping its own pid and live pids
  (`history::alive`).
  - State 0 (nothing recorded): deleted at once.
  - Recorded: becomes a row whose pcs are named with `ix_prof::symbols::names_slid(pcs,
    dead_slide)`. That only names our executable's code, which is why the hash must match;
    library addresses stay bare hex. The row goes out with the next flush, and the file is
    deleted after that flush's writes succeed.
  - Refused (not a record, other layout, zero hash, other blaze hash, ours unhashable): typed
    `CrashError` naming the path and rule, renamed to `crash.<pid>.refused`, logged with
    `tracing::warn`, and kept on `History::refused_crashes()`.
- **Opt-in drain.** The history writes this process's panics only after `record_panics()` (the
  daemon calls it). Otherwise a test's flush would pick up another test's panic from the global
  ring and change file and row counts. Recovered crash rows are always written.
- **Reports.** `blaze slow` lists `panics: N (M caught by its job)`, then each with
  location/thread/message and up to 8 top frames (innermost first, past the panic machinery:
  `top_frames` anchors on the last core/std panicking, hook or walker frame among the innermost
  24). The CLI a daemon hung up on reads `crash.<pid>` in place (the next daemon recovers it) and
  prints signal, thread, last panic, 12 frames and open spans.

## Unsafe invariants (each block has a `// SAFETY:` comment)

- `stack::walk`: reads 16 bytes at an aligned fp only after `readable(fp)` vouched for it.
  - Probe: the kernel just copied them, and a thread's own stack is not unmapped under it.
  - Sampler: the bounds are the stopped thread's mapped stack, with the leave lock held.
  - Termination: fp must strictly increase, with at most 256 records.
- `stack::probe`: `write` from an arbitrary address is checked by the kernel (EFAULT). No user
  load happens.
- `frame_pointer`: register read, `nomem, nostack`.
- `crash::Page::put_*`: offsets are compile-time inside the page (const asserts on `END <= SIZE` and
  8-alignment). The page is never unmapped once published, and the state CAS gives one writer.
  `state()` makes an `&'static AtomicU64` at an 8-aligned offset of a page-aligned mapping that
  lives for the whole process.
- `crash::arm`: a new MAP_SHARED mapping of exactly SIZE bytes after `set_len(SIZE)`. It is
  published by CAS; the loser is unmapped and was never published.
- `on_fatal`: `info`/`context` are the kernel's live SA_SIGINFO arguments (null-checked). Chaining
  transmutes the saved `sa_sigaction` to the signature its saved `SA_SIGINFO` flag says. Calls are
  limited to atomics, plain stores, `write`, `read`, `clock_gettime`, `pthread_threadid_np` or
  `gettid`, `pthread_getname_np`, `raise` and `sigaction`: no allocation, no lock.
- `registers` (macOS): relies on `uc_mcontext` being a pointer at offset 48 and the arm64 thread
  state layout (`far, esr, exception, x[29], fp, lr, sp, pc`).
- `altstack`: private anon mapping, never unmapped.
- `panics::system_thread_name`: `pthread_getname_np` into a live buffer of its passed length.

## Risks

- **Frame pointers are assumed.** blaze keeps them on arm64 macOS. On Linux x86_64 a frame without
  rbp ends the walk early or gives a short or odd stack. The probe stops it faulting, not
  misreading.
- **macOS ucontext structs are hand-defined.** Check them against
  `<sys/_types/_ucontext.h>`/`<mach/arm/_structs.h>`. If they are wrong, crash stacks are wrong;
  the handler can't fault, because pc/fp only feed the probed walk.
- **Signal chain order.** `crash::install` must run after `signals::record_ends` (it does in
  serve.rs). If something installs a handler later without chaining, the record is skipped for
  that signal. std's stack-overflow handler aborts. That SIGABRT finds the state already taken,
  so it isn't recorded twice, and it chains to `ended`.
- **`raise` on DFL for real faults.** It is raised again, not left to re-fault, because macOS
  `SI_USER` is positive. A core dump then shows the raise site, not the faulting instruction.
- **The hook is not fully AS-pure.** It calls `std::thread::current()`, which can allocate once
  for an unnamed thread. The old hook allocated freely, so this is no regression.
- **Only the daemon is armed.** Client, worker, build-script and macro processes don't install
  the handler or arm a record. Their panics are not rows either, unless they call
  `panics::record`.
- **Refused files are kept** as `crash.<pid>.refused`, and nothing trims them yet.
- **Duplicate signal rows if a write fails.** If a crash row's flush write fails, its rows are
  dropped for this process but the file stays, and the next daemon writes it. If the delete after
  a successful write fails, the next daemon writes the row again (warned).
- **Build-id reuse.** A crash row's `build_id` comes from the dead daemon's numbering. `open`
  raises `last_build` past it so it isn't reused.
- **Older history directories.** The `panics` view needs `schema.panics.parquet`, which
  `History::open` writes. A directory last opened by an older blaze and read without opening (the
  same as for any new table) fails the view.
- **Merge conflicts.**
  - `history.rs`: the `tables` array, `Flushed` and `open`'s directory scan. The other lane edits
    file discovery.
  - `flight.rs`: `open_of_thread` sits next to `build_of`. The mmap-rings lane may change `Ring`.
    It uses `ring.live`, `ring.current()`, `ring.depth` and `ring.entered`.
  - `schema.rs`: `TABLES` length and `Type::Pcs`, which may collide with the images lane.
- **`Panic::at` for a payload with no hook location** now reads "an unknown place on thread X"
  instead of `None`. Before, the plane recorded nothing when `info.location()` was None.

## Tests written

- `panics::tests::a_caught_panic_is_one_row_with_its_message_location_and_stack`: one row, caught,
  location `panics.rs:`, stack names `panics_in_a_job`, batch matches the schema, `top_frames`
  starts at it, `claim` finds it.
- `panics::tests::a_long_message_is_cut_and_says_so_and_still_matches`.
- `stack::tests::a_walk_of_this_thread_finds_its_callers` and `a_wild_frame_pointer_ends_the_walk`.
- `crash::tests::a_crash_file_round_trips_through_recovery`: the handler's own `Page::record`
  with a fake fault (pc in `faults_here`, the real fp) into a file of dead pid `0x7fff_fff0` is
  recovered to one signal row. The row has SIGSEGV, addr 0x10, stack names `faults_here` and the
  test, `image_hash` is set, and the file is deleted only after `written()`.
- `crash::tests::only_the_first_fault_is_recorded_and_an_armed_record_alone_is_no_row`.
- `crash::tests::a_record_of_another_layout_or_blaze_is_refused`: layout mismatch, other-blaze
  hash and short file each give their typed error. On disk the file is renamed `.refused`.
- Existing plane tests (`a_task_that_panics_ends_with_its_message_and_where_it_was_raised_on_another_thread`,
  the alloc-failure test) now exercise the new ring via `record_panics`. The daemon.rs Died test
  has `crash: Ok(None)` added.

## What to check

1. `blaze check` on blaze-profile, blaze-cargo and ix-prof. Likely spots:
   - `crash.rs` libc field names per target (`si_addr()` on Linux vs the field on macOS,
     `stack_t`, `REG_RIP`/`REG_RBP` as `c_int`).
   - `sa_flags` types.
   - The `arm` error closure.
   - Unused-import or dead-code lints on non-unix.
2. `blaze test -p blaze-profile` (the crash, panics, stack and views tests, plus
   `every_table_names_each_column_once` for the new columns' docs). Then the plane tests and the
   daemon.rs Died test in blaze-cargo.
3. End to end on the Mac:
   - Start a daemon, send `kill -SEGV <pid>` (and `kill -ABRT`).
   - Confirm `<state>/history/crash.<pid>` is non-zero at state 2 and the CLI quotes it.
   - Start the next daemon. The file should be gone, `blaze debug sql "select kind, signal,
     message, stack[-3:] from panics"` should show the row, and `blaze slow` should list it.
4. The macOS `__darwin_mcontext64` layout. A real `SIGSEGV` (not `kill`) should give a stack whose
   innermost frame is the faulting function.
5. Whether the images lane's `images` table and raw-address samples use the same
   `image_hash`/`image_base` column names and the same list<u64> type. Rename to match if not.
