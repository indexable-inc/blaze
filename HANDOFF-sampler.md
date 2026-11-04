# Handoff: profile sampler lane (pd-sampler)

Worktree /Volumes/Projects/blaze-wt-pd-sampler, branch pd-sampler. Not committed. Nothing was built,
checked or tested: the main agent builds once. 22 files changed (+1621/-372), 2 new.

## Files changed

New:
- crates/ix-prof/src/jit.rs: a code map registry for code made at run time (`CodeMap`, `Function`,
  `register`, `name`, `any`). A map keeps its owner (the wasm `Module`) alive for the process's life.
- crates/ix-prof/src/images.rs: the images loaded in this process (`Image { path, build_id, base,
  size, slide, main }`, `loaded()`, `boot()`, `blake3(&Image)`). Linux reads them with
  `dl_iterate_phdr` plus PT_NOTE build ids; macOS reads dyld's image list plus `LC_UUID`. `blake3`
  maps the file, checks that its build id matches the loaded image's, hashes it once per process
  and image, and returns `Err(why)` otherwise.

Changed:
- crates/ix-prof/src/lib.rs: `pub mod images; pub mod jit;`.
- crates/ix-prof/src/symbols.rs: `functions(&[u64]) -> BTreeMap<u64, Vec<String>>` names each
  address by function with no offset. Frames are outermost first, inline frames come from DWARF.
  It uses one process-wide zero-copy `addr2line::Context` over the executable (`/proc/self/exe`,
  or the .dSYM on macOS). It also adds `names_kind()` (dwarf/symbols/none) and `NO_IMAGE`, plus
  two tests.
- crates/blaze-profile/src/schema.rs:
  - `Type::Pcs` (Arrow `List<UInt64 "pc">`).
  - `STACK_PCS`, a nullable last column on `samples` and `alloc_samples`.
  - A new `IMAGES` table (process, boot, seen_ns, path, build_id, blake3, unhashed, base, size,
    slide, names). `TABLES` grows to 24. No VERSION bump: the columns are new, nullable and last.
- crates/blaze-profile/src/arrow.rs: the `Pcs` arm.
- crates/blaze-profile/src/batches.rs:
  - `samples()` writes `stack_pcs` and takes a frames map keyed by lookup address.
  - New `ImageRow` and `images()`.
  - Two tests.
- crates/blaze-profile/src/symbols.rs, rewritten:
  - `lookup(i, pc)` looks a return address up at pc-1.
  - `frames(stacks)`.
  - `ImagesWritten`, which writes each image once per history.
  - Two tests.
- crates/blaze-profile/src/history.rs, kept minimal:
  - An `images: ImagesWritten` field.
  - One extra batch in flush, with IMAGES last in the tables array (14 entries).
  - `samples_batch` uses `symbols::frames`.
  - `dictionary()` treats `Pcs` as false.
- crates/blaze-profile/src/flight.rs: one line. The cfg of `rings()` is now `any(linux, all(macos,
  aarch64))`.
- crates/blaze-profile/src/sampler.rs:
  - Docs for both OSes.
  - `pub(crate) use linux::Thread` on Linux; the stub `Thread` is cfg'd off where a real one exists.
  - `taken()` names frames through `symbols::frames`.
- crates/blaze-profile/src/sampler/linux.rs: fully rewritten. perf_event is gone and the file is now
  a signal sampler (see Design). It has 3 tests.
- crates/blaze-profile/src/sampler/macos.rs: docs. A blocked thread with no open span
  (`innermost() == 0`) is now skipped, the same as on Linux.
- crates/blaze-profile/src/gate.rs:
  - `Unchecked { check, why }` and `Seen { workers, samples, off_cpu, waits }`.
  - `unchecked(Seen)` and `Verdict.unchecked`. `passed()` now also needs nothing unchecked, and
    Display prints "- X: not checked: why".
  - One SQL query computes `Seen`.
  - One test.
- crates/blaze-profile/sql/examples.sql: `raw_blocks` uses `samples.cpu = 'off'` instead of a regex
  over frame names.
- crates/blaze-profile/sql/views.sql: an `images` view.
- crates/blaze-profile/src/slow.rs, slow/load.rs, slow/render.rs, slow/tests.rs:
  - `Finding.stacks: Stacks { on_cpu, off_cpu }`, holding the top `STACKS = 3` sampled stacks inside
    the finding's span (its descendants included, through a recursive ancestor walk in SQL).
  - Render prints "- sampled on the CPU|blocked inside it Nx: f < g < ..." with the innermost
    8 frames.
  - One test.
- crates/blaze-macro-eval/src/wasm/code.rs: `name_code(&module, what)` runs after each memo insert
  and registers a `jit::CodeMap` from `module.text()` and `module.functions()`. Safe code only.
- crates/blaze-macro-eval/Cargo.toml, Cargo.lock: the `ix-prof` dependency.
- docs/profiling.md: front matter kept (`status: plan`, dated 2026-11-04). Updated the samplers,
  symbolizer, `pcs` type, `samples.stack_pcs`, the new `images` section, `alloc_samples.stack_pcs`,
  the gate's not-checked output and the slow finding's `stacks`.

## Design

### Linux sampler (crates/blaze-profile/src/sampler/linux.rs)

It works the same way as the macOS sampler: one sampler thread asks each flight-recorder thread for
its stack.

- **Signal.** The signal is SIGRTMIN+6, sent with `tgkill`. The handler is installed with
  `SA_SIGINFO | SA_ONSTACK | SA_RESTART`. perf_event is removed, with no fallback.
- **Setup check.** `check()` refuses with `Unsampled("...")`, naming the cause, in these cases:
  - SIGRTMIN+6 is past SIGRTMAX.
  - The signal's disposition is not SIG_DFL (someone else owns it).
  - A `process_vm_readv` probe fails (for example under seccomp).
- **Slot.** Each ring has a leaked `'static` `Slot` of atomics: asked, done, collected, asked_ns,
  tid, on_cpu, ring, at_ns, depth, span, build, pcs[DEPTH].
- **Protocol.**
  - The sampler writes the request, then bumps `asked` (Release).
  - The handler answers only when `asked != done`: it writes the stack, then stores `done = asked`
    (Release).
  - The sampler collects when `done == asked && collected != done`.
  - It asks again after 1 s (`ASKED_AGAIN`) and drops answers later than 100 ms (`LATE`). Both
    counters show in the state string.
- **Thread lifetime.** The sampler holds `ring.sampled` (a Mutex) while it tgkills. `enter(tid)`
  runs under that lock in `claim()`, and `leave()` runs in `Held::drop` at thread exit. So the
  signal never goes to a tid that has exited and been reused.
- **On/off-CPU.** The sampler reads the thread's state from `/proc/self/task/<tid>/stat` (through a
  cached fd, `read_at`) before it signals: 'R' is on-CPU, anything else is off.
  - This matches macOS, where "on" means the thread was running or runnable when sampled and "off"
    means it was blocked.
  - A blocked thread with no open span is skipped on both OSes, since its stack only says "idle".
- **The walk.** The handler walks frame pointers from the registers in its ucontext: x86_64
  RIP/RBP/RSP, aarch64 pc/x29/sp (masked to 48 bits).
  - All memory is read with `process_vm_readv`, so a bad frame pointer is an `EFAULT` return,
    never a fault inside the handler.
  - It does one 8 KiB window read (2x4 KiB iovecs from sp's page), plus one call per frame record
    that falls outside the window (fiber stacks, deep frames).
  - It stops at DEPTH, a null or misaligned fp, an fp that does not move up, or a read failure.
- **Alternate stack.** If the thread's alternate stack is missing or smaller than 256 KiB (Wasmtime's
  MIN_STACK_SIZE), `enter` mmaps its own 256 KiB plus a guard page and saves the previous one.
  - `leave` puts back the previous one only if ours is still installed.
  - It munmaps ours unless the thread is running on it or the read failed; then it leaks it.

### Symbolization

- Names come from ix-prof's `functions()`:
  - A DWARF function plus its inlined frames.
  - Otherwise, an ELF/Mach-O symbol.
  - Otherwise, the `jit` map (wasm proc macros), as `name [wasm]` or `wasm function N of <module>`.
  - Otherwise, dladdr exports for shared libraries.
  - Otherwise, `[file]` or `[code in no image]`.
- Names never carry an offset, so all of a function's samples fold into one frame.
- Frames at index 1 and above are looked up at pc-1.
- Names are found at flush or dump time, never in the sampler.

### `stack_pcs` + `images` (shared with the panic-stack lane)

- `stack_pcs` is the raw addresses, innermost first, as sampled: no -1 adjustment, no slide removed.
- An `images` row is written once per (process, base, path) when a flush first holds an address in
  that image.
- To name a pc later:
  1. Find the image with `base <= pc < base + size` and the same process and boot.
  2. Check `build_id` or `blake3` against a file.
  3. Look up `pc - slide` (or `pc - slide - 1` for a return address) in that file.
- `unhashed` says why there is no `blake3`. `names` says how the row's frames were named
  (dwarf/symbols/none/exports).
- JIT addresses have no image row and name nothing after the process exits.

### Gate

- A check that saw no data is reported as "not checked: <why>", not passed:
  - "uncovered workers": no workers were seen.
  - "raw blocks": no samples, or no off-CPU samples.
  - "unsaid waits": no waits were seen.
- `passed()` is now false when anything is unchecked.

## Unsafe invariants (linux.rs unless noted)

- **The handler is async-signal-safe.** It only does atomic loads and stores on a `'static` Slot,
  `process_vm_readv` (a raw syscall, async-signal-safe), and reads of its own `ucontext`. It takes
  no lock, does no allocation and does no formatting. It saves and restores errno.
- **The handler's thread-local slot.** The handler finds its Slot through a `const` thread_local
  `Cell<*const Slot>` with no destructor, set in `enter`.
  - This is local-exec TLS in an executable, which is safe in a handler.
  - In a dylib it would be general-dynamic TLS (`__tls_get_addr` can allocate), so it is not safe
    there. blaze-profile is linked statically into the blaze binary.
- **A Slot is never freed.** It is leaked `'static` and reused with its ring, so a late signal
  always writes valid memory.
- **The sampler only signals live threads.** tgkill happens under `ring.sampled`, and
  `enter`/`leave` change the tid under the same lock.
- **The handler only answers a request.** It answers only when `asked != done` and stores
  `done = asked` last (Release), and the sampler reads `done` with Acquire. A stray SIGRTMIN+6 with
  no request writes nothing.
- **The altstack.** It is mmapped with a PROT_NONE guard page below it. It is unmapped only when
  `sigaltstack` reads back ours as current and `SS_ONSTACK` is clear; on any doubt it is leaked.
- **ucontext register access.** Register offsets are libc's `mcontext_t` fields, for x86_64 and
  aarch64 only. Linux on any other arch has no `registers` fn and does not compile. Add a cfg'd
  `check()` refusal if that ever matters.
- **images.rs `hash_file`.** It makes a read-only private mmap of a file it has open and munmaps it
  before the slice can be used again.
- **symbols.rs `static_dwarf_of`.** It leaks the mapped file and decompressed sections to get
  `'static` slices. This happens once per process.
- Every unsafe block has a `// SAFETY:` comment.

## Risks

1. **EINTR.** The signal wakes blocked syscalls. `epoll_wait`, `nanosleep`, `poll`, `futex` with a
   timeout and similar calls return EINTR even with SA_RESTART. Code that treats EINTR as an error
   would break. std retries its own calls, but check parking_lot and crossbeam parking, and blaze's
   own epoll and io loops. Only blocked threads with an open span are signalled.
2. **Signal masks.** blaze-core/src/process/spawn.rs:337-359 blocks every signal around vfork.
   Signals sent in that window pend; the answer comes late and is dropped (`LATE`). This is
   harmless but counted.
3. **Install order.** `signals::catch()` runs after `sampler::start` (blaze-cargo serve.rs:900).
   It must not take SIGRTMIN+6. It does not today.
4. **Seccomp.** Docker's default seccomp profile refuses `process_vm_readv` without
   CAP_SYS_PTRACE. In that case the sampler stays off with a named reason. There is no fallback, by
   design.
5. **Frame-pointer walks.** In a prologue or epilogue the walk skips the caller, or names the
   caller twice. Code built without frame pointers yields short stacks.
6. **Wasmtime and std altstacks.** We keep any existing altstack of at least 256 KiB. Check that
   Wasmtime's per-thread altstack setup does not later replace ours while the Slot still points
   there (that is harmless: the handler runs on whichever is current).
7. **What on-CPU means.** Linux "on" is state 'R' at the moment the sampler reads it (running or
   runnable). A thread in 'D' (IO wait) counts as off. The stat read happens before the signal, so
   the state can change in between.
8. **Symbolization cost.** Parsing the executable's DWARF costs memory on first use, and so does
   BLAKE3 of the executable on the first flush (a few hundred MB file).
9. **Inline test.** `a_return_address_names_its_function_and_the_function_it_was_inlined_into`
   accepts either a separate inline frame or none, depending on whether blaze's debug codegen
   inlines `#[inline(always)]`. Check what it actually prints.
10. **Wasm names.** If wasm modules carry no name section, frames read "wasm function N of
    <module>".
11. **Duplicate compiles.** Two compiles of one module at different addresses register two maps,
    and the losing module is kept alive. That is one module leaked per race.
12. **No writer for `alloc_samples`.** The table has `stack_pcs`, but nothing writes the table yet
    (the docs say so).
13. **Nix unit graphs.** The new blaze-macro-eval -> ix-prof dependency may need the nix unit graphs
    regenerated.
14. **Untested SQL.** The slow `span_stacks_sql` (recursive CTE, QUALIFY) and the gate `Seen` query
    have never been run against DuckDB.

## Tests added

- ix-prof/src/symbols.rs:
  - `a_return_address_names_its_function_and_the_function_it_was_inlined_into`
  - `an_address_in_no_image_is_said_so`
- ix-prof/src/jit.rs: `an_address_in_a_code_map_is_named_by_its_function`
- blaze-profile/src/symbols.rs:
  - `an_address_in_a_function_is_named_by_it_without_an_offset`
  - `an_image_is_written_once_a_process`
- blaze-profile/src/batches.rs:
  - `a_sample_row_carries_its_stack_addresses_span_and_build`
  - `a_sample_outside_any_span_has_no_span_or_build`
- blaze-profile/src/gate.rs: `a_check_with_no_rows_is_not_checked_and_does_not_pass`
- blaze-profile/src/slow/tests.rs:
  `a_finding_about_a_span_carries_its_top_sampled_stacks_on_and_off_the_cpu`
- blaze-profile/src/sampler/linux.rs (Linux only):
  - the frame walk over a built chain
  - an unmapped read failing without a fault
  - the reading thread being on-CPU

## What to check

On the Mac:
- `blaze check` and `blaze test -p ix-prof -p blaze-profile -p blaze-macro-eval`.
- Watch for type and borrow errors in the slow and gate SQL plumbing, the `Pcs` Arrow arm and
  `history.rs` merge conflicts with the other lanes.
- Run a profiled build. Confirm that `samples.stack_pcs` is filled, that `images` holds the
  executable with a build_id and blake3, that frames carry no `+0x`, and that proc-macro frames
  show `[wasm]`.
- Check that `blaze slow` prints the stacks under a finding.
- Check that the gate on an empty profile prints "not checked".

Needs a Linux ix VM (`ix new --name pd-sampler`, x86_64 and aarch64 if possible):
- The three linux.rs tests, and a real profiled build with the sampler on. Confirm that the state
  string shows nonzero samples and small `LATE` and `ASKED_AGAIN` counts.
- Confirm that both on- and off-CPU samples appear.
- Confirm that the images build_id is read through `dl_iterate_phdr` and that `/proc/self/exe` is
  hashed.
- Look for EINTR fallout in the daemon (risk 1) under load.
- Run it in a container with the default seccomp profile, and confirm the refusal names
  `process_vm_readv`.
