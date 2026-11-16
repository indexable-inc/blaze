---
status: plan
summary: "`blaze run` and `blaze build` from checked bodies to a running program. End state (2026-11-16): a code record per instance and tier, position-independent, every outside reference through an import word; records grouped in segments mapped read-only and executable in place (dependency bundles from the shared cache, one delta segment per command); the program a manifest of rows over records; the image its prebound import tables at fixed addresses; `blaze run` execs the launcher, which maps, binds the C library's names and starts `main` in about 5 ms; an edit writes the new record, one manifest chunk and the words that name the edited function; `blaze build` writes the same records as one signed Mach-O or ELF. Then the gap and the pieces. Below it, kept for live patching later: blaze-run's sans-IO machines (Parent, Slots, Heat, Tier, Registry, Tests)."
date: 2026-11-16
---
# Run and test

Andrew, 2026-11-15: no daemon; "make it exist properly all in one go". AGENTS.md (2026-11-16)
puts `blaze build` back in scope and live patching out ("now: patch the file, rerun"). The end
state below is the run path. The sections after it (the live picture, the blaze-run machines)
describe live patching, kept for later: nothing on the run path calls them (decision R1).

## End state (architect, 2026-11-16)

Estimates, not measured. Every rule of the design holds by name; no daemon, no fallback backend.
**The code record** (blaze-keyed `CodeRecord`, tag 27; value form `crates/blaze-code` `record`),
one per instance and tier. *Key*: the symbol (item's stable id, type arguments by content), the
body record's result digest, target, tier, code settings (panic strategy, overflow checks) and
toolchain; read-addressed (target.md, "the shape") by the layouts, impls and constants it read.
*Value*, read in place: position-independent code and pure read-only data; a relro part (data
holding addresses); targets, sorted, each a (space, symbol) (function, static, vtable,
thread-local, C name), never an address, slot or path; sites (import load, import call, own relro
address), sorted; a compact unwind word and landing pads; positions item-relative. Debug rows are
a record of the same key. Every outside reference goes through an import word (`adrp x16; ldr
x16; blr x16` for a call): the code holds no address and serves every program at every base.
**The segment**: records mapped read-only and executable in place: a bundle (one dependency crate
at the top tier, built and signed by CI, std among them) or a command's delta segment (what it
made). Records 16-aligned in manifest order; on the next page the import table (a word per
distinct target) and relro parts, at a fixed distance from the code. In a bundle a call within
the bundle is relaxed to `bl`; a delta segment never relaxes. macOS: a minimal Mach-O with an
ad-hoc code directory, registered by `fcntl(F_ADDFILESIGS_RETURN)` and mapped as dyld maps a
library; Linux: the file mapped `PROT_EXEC`. Equal records (bytes and targets) are placed once:
folding at record grain (icf.md stands for folding by key before codegen).
**The program**: blaze-program's `Program` (stands: the row rule, `Chunks`): rows by symbol
(symbol, tier, record key, segment), static and vtable rows, entry, settings; chunks of 64 rows
under a root. Programs share every record and segment in common. **The image**: a record keyed by
manifest, target and address plan: each segment's fixed base (dev runs give up ASLR), its bound
import table and relro words in page chunks, and postings from each symbol to the words naming
it. C names are bound at start, one `dlsym` each.
**`blaze run`** checks what the program reaches, builds each reached instance with no record
(eagerly: the walk is sound, so no stub waits on a compiler; a first run gets std, core, alloc as
bundles), files manifest and image, and `execve`s the launcher: no parent stays. The launcher maps
each segment at its base and the image's pages private (no code byte copied), binds C names,
registers one unwind finder, applies the run spec's sandbox and calls the entry.
- *Repeat, nothing changed*: blaze's start (1.5 ms) + certificate (2 ms, target.md) + `execve`
  (1 ms) + ten `mmap`s and signature registrations (0.2 ms) + 100 `dlsym` (0.1 ms): 5 ms to
  `main`'s first instruction; a linked file started after the certificate: 4.5 ms (ratio 1.1).
- *Edit of one body*: it is checked; its instances' records are rebuilt (usually one); callers
  are untouched. The delta segment holds the new record; the manifest rewrites one chunk and its
  root; the image appends the segment at the next free address and rewrites the words the
  postings name (8 bytes each) and their chunks. A grown body is placed new, never in place: no
  slack, nothing moves. About 0.3 ms past the check. Dead bodies go when workspace segments are
  re-laid after an answer, never on the edit path.
**`blaze build`** writes the same records as one standalone file: Mach-O (`LC_MAIN`, libSystem,
chained fixups, `__unwind_info`, `LC_FUNCTION_STARTS`, `LC_UUID` of the content, ad-hoc
`LC_CODE_SIGNATURE`) or ELF (PIE, `PT_INTERP`, RELR, `.eh_frame_hdr`, `PT_GNU_RELRO`, build id).
Unlike run's image: ASLR on, one GOT with every import site retargeted to it, calls in reach
relaxed to `bl`, code ordered by profile (code-layout.md; "Export takes what runs observed"
below: both stand), debug info in the side file (debug-side-file.md stands). An edit clones the
last file (`clonefile`, `FICLONE`) and writes the delta's code into room kept before
`__LINKEDIT`, the changed GOT words, their fixup pages and changed code-directory slots.
**Cost to the program**: a call leaving its segment is a load and an indirect branch: about 1%,
up to 5% in call-bound loops (to measure on reth); bundles and `blaze build` pay nothing.
**Unwinding.** Panics unwind (the strategy is in the key). Each segment has an unwind index made
when written; the launcher registers one finder (macOS `__unw_add_find_dynamic_unwind_sections`,
macOS 14+, rule 45; Linux `__register_frame_info` per segment). Backtraces are symbolized by the
hosted table's `symbolize(pc)` from the debug rows; `--debug` registers one debug object for the
image through the GDB JIT interface (a perf map on Linux).
**Tiers.** One fast backend (blaze-emit's baseline; stencils when they return), named by the
tier in the key. Dependencies at the top tier (LLVM from LIR), keyed with their inlinees
(blaze-program `key`, stands). Tier-up continues the command's work after the answer at utility
priority, with an expiry: the program's workspace instances, hottest first by the launcher's 1 kHz
samples of the last run. It swaps in at the next start (a row whose optimized record exists for
its current key is top tier); an edit of an inlined callee moves its callers back to fast.
**The launcher**: `blaze-launcher`, a small bstd program beside `blaze`, signed at release
(hardened runtime, `disable-library-validation`, no JIT entitlement: no page is writable and
executable). It maps, binds, registers, sandboxes and starts; then it is the program.
**Best systems.** mold, wild: parallel links; run links nothing, build patches a clone in
proportion. Zig: in-place patching with padding; blaze needs none and shares records across
programs and machines. Cranelift JIT, Dioxus subsecond: a jump table in a live process; blaze
restarts prebound in 5 ms, its import words ready for live patching. Julia pkgimages, LuaJIT:
local caches; blaze's are content-keyed and shared. Makepad, Bevy: reload keeping state: later.
dyld: bundles are its shared cache, the image a prebuilt launch closure; build writes chained fixups.

### Gap (read 2026-11-16)

| End state needs | Today (file: function) | Missing | Reached by |
|---|---|---|---|
| The new front end on `blaze run` | blaze-cli program.rs `run` calls `blaze_ladder::run::run`, which is `run_with(.., FrontChoice::Old)` (run.rs) | `FrontChoice::New` with package, edition, shelf | only tests (ladder_front2.rs, front2_rungs.rs, front2_main.rs) |
| Hello world's `main` builds | 1,371 of 2,344 instances; `main` stops at `UsizeNoHighBit` | front-end and builder lanes' pieces | only tests |
| Code records in the store | place.rs `Body { code: blaze_emit::Code, targets }`, held in memory; blaze-keyed `CodeRecord` has no writer | the value form, the put, the read | the command, in memory only |
| Targets by symbol | `Target::Slot(u32)`: slot n is the nth reached (run.rs `Reached::slot`) | (space, symbol) per target | the command |
| Code key covers its inputs | blaze-build instance.rs `instance_key`: item key and text (front2 program.rs `code_of`) and arguments; an ADT argument hashed by item key only | target, tier, settings, toolchain; layouts and impls read as its shape | the command |
| Code with no address in it | blaze-emit: literal words after the code, filled per run by place.rs `Table::code` (copy, then patch) | import sites (`adrp`/`ldr`) and a relro part, as relocations | the command (old form) |
| Reached walk at scale | run.rs `Reached::slot`: a linear `position` per call | a map by key | the command |
| Segments mapped in place, signed | none; blaze-launch-rt maps an anonymous area and copies | writer, signature, mapping | nothing |
| Manifest | blaze-program `Program`, `Chunks`, `RowChange`: no crate depends on it | wiring from the walk | only tests |
| Image and its patch | none (blaze-link's patch room is the parked linker's) | the record, postings, patch | nothing |
| The program's own process | `Placed::call` transmutes and calls on blaze's thread; blaze-launcher main.rs is the daemon-era socket client | the exec, the launcher's map-and-bind mode | the command (in-process) |
| C names bound once | place.rs `symbol`: a `dlsym` for every reference of every body | one per distinct name | the command |
| Unwinding | blaze-emit refuses `Invoke`/`Resume` ("unwinding"); blaze-launch-rt unwind.rs registers single FDEs | records' unwind rows, segment index, finder | nothing |
| `blaze build` | blaze-cli main.rs: `Some("build") => NotBuilt` | the writer; blaze-link's macho/sign.rs and macho/chained.rs are parked code to take from | nothing |
| Tier-up | blaze-run tier.rs, heat.rs (sans-IO); no LLVM-from-LIR backend | the optimizer, the samples, the queue | nothing |
| Statics and vtables as records | run.rs `static_image`: every static evaluated before the walk, in memory | data records, homes in the image | the command |
| Symbolize, debug object | none on this path | symbol column, debug rows, `symbolize` entry | nothing |
| Code name without hooks | blaze-drive run.rs `code_name` hashes `hooks` into a `Code` name | drop it (nothing observed is in a key) | nothing |

### Pieces (one writer each; "||" marks pieces that can be written at the same time)

1. || `blaze run` takes the new front end (blaze-cli program.rs): package and edition from the
   manifest, the shelf the check resolved. Test: the hello-world rung prints through the command.
2. || `Reached::slot` by map (blaze-ladder run.rs). Test: 65,536 synthetic instances in linear time.
3. || The code record's value form (NEW crates/blaze-code record.rs): WRITTEN by the architect.
4. || Segment layout, image binding and the edit patch (NEW crates/blaze-code image.rs): WRITTEN,
   with five tests in tests.rs; check and tests green (integrator, 2026-11-16 13:43).
5. blaze-emit emits import sites and a relro part instead of literal words (after 3). Test: no
   literal word in the code; every outside reference is a site.
6. The run path writes and reads code records (ladder place.rs, run.rs, after 3 and 5): targets
   by symbol, put under the drive's `Code` name. Test: a second run builds nothing.
7. || The code key gains target, tier, settings, toolchain and its shape (blaze-build instance.rs,
   after 6). Test: a field edit moves the instance's key; a comment edit does not.
8. || Signed segment files and their mapping (NEW blaze-launch-rt module, after 4). Test: a
   signed segment returning 42 runs; an unsigned one is refused by name (rule 45).
9. The launcher's map-and-bind mode (blaze-launcher, after 4 and 8): image on descriptor 3,
   `dlsym` once per name, the hosted table, the entry. Test: the hello-world image prints.
10. `blaze run` files the manifest (blaze-program `Program`) and the image and execs the launcher
    (blaze-cli, after 6 and 9). Test: a repeat run builds 0 and reaches `main` from records.
11. The edit patch on the command path (after 10): the image's postings rewrite the words.
    Test: one body edit writes the record, one manifest chunk and only the words naming it.
12. || Unwinding (blaze-emit landing pads, record rows, segment index, finder; after 5).
    Test: a rung's `catch_unwind` catches a panic.
13. || `blaze build` writes Mach-O and ELF from segments (NEW crate from blaze-link's macho and
    elf writers, after 4). Test: the built hello world runs and its signature verifies.
14. `blaze build` patches a clone after an edit (after 13).
15. Symbolize and the debug object (after 9). 16. Tier-up: samples, queue, LLVM records (after
    lir.md's LLVM path).

## The live picture (live patching, later; not the run path)

```
blaze run (parent: alive exactly as long as the program)      the program's process (blaze-launcher)
  certificate stands? -> manifest of the program (records)       control thread: Slots, Heat
  Registry: entry in <state>/runs/, events stream                 program threads: load slot word,
  Parent: lookups, Compiler on workers, single flight             jump; a need stub fires -> queue
  Tier: Optimizer on background workers                           -> control thread -> Need
        <==== Unix socket, blaze_launch::wire (deterministic CBOR), one per run ====>
  Welcome{slots, known: slot -> code record}  ------------->  maps table, area; places entry + pre-main
  Begin                                      ------------->  runs initializers, then main
                                             <-------------  Need{slot, thread}   (no known record)
  Publish{slot, tier, record} / Failed{slot, error} ------>  reads record from the store, places,
                                                             stores the words, wakes the thread
  Hooks{slot, hooks}                         ------------->  hook stub in or out of the slot word
                                             <-------------  Hot{slot}  (a count stub reached 0)
                                             <-------------  TestEnded{test, verdict, entered}
                                             <-------------  Exited{status}  -> parent unregisters, exits
```

No process outlives the program. What the next run needs is in records: the code (by instance
key), the optimized code (read-addressed by what it inlined), the program's manifest and what it
ran (by the checkout's certificate), each test's record. A run started while another run of the
same checkout lives is its own process with its own parent; they share only the store.

## The machines (crates/blaze-run; live patching, later: nothing on the run path calls them)

Each takes an event and pushes effects into a caller's buffer; time is an event field
(`Nanos`); process liveness is a trait; every table is in the allocator its maker passes.

**Parent** (`parent::Parent`, in `blaze run`). Per slot: `Unasked`, `Looking{priority}`,
`Queued{priority}`, `Compiling{priority, deadline}`, `Ready{tier}`, `Failed{error}`.
In: `warm(image)`, `start(needed, history)`, `need(thread, slot)`, `looked(slot, found)`,
`compiled(slot, result)`, `toggle`, `tiered`, `tick(now)`, `ended`. Out: `Lookup`, `Compile`,
`Publish`, `Hooks`, `Answer`, `Begin`, `NoStart`, `WriteImage`, `Say(RunEvent)`. A need is looked up first (another run may have made the record),
compiled only on a miss, at most once however many threads ask; every asker is answered after
the publish, with the same answer. Priorities: `Asked` (a thread or the start waits), `Callee`
(a published function calls it), `History` (the last run ran it). Background work never takes
the last free worker (workers >= 2), so a thread waits only on its own function. A failed
compile answers every caller `Failed(CompileError)`: in the program, the call panics on that
thread with that error (rule 20) and every other function goes on. A compile has a deadline on
the driver's clock; past it the askers fail with `Why::Overran`, and a late success still
serves later callers. The `Compiler` trait: `CodeKey { function, tier }` -> `Made::Done(FastRecord
{ code, callees, reads })`, `Made::Need(keys)` or `Why`; its task writes the record before it
answers. No compile waits on another (bottom.md, "How a task waits"): on `Need` the slot is
`Blocked{left}` and holds no worker, each lacking record is made once however many compiles
lack it (`Make`, `made`), and the compile is entered again from the top when it lacks nothing;
one deadline covers all its entries. The `Optimizer` returns the same shape and `Tier` blocks
and re-enters its jobs the same way.

**Observing is never compiled in** (bottom.md, "Decide at run time"). A function's code, fast or
optimized, holds no counter, no mark and no hook, so its bytes are the same whatever any run
observes: a code record's key is `CodeKey { function, tier }`, with no hooks field, and one
record is shared by every run. What is observed is a stub in the slot word, in front of the
code, chosen at run time and gone when not wanted: a hot function, a function outside a test
run and a function nobody watches is called straight into its body and pays nothing. A slot's
watch (`Watch`) is a set of the run's two observers and the viewer's hooks:

- *Heat*: a count stub while the function is fast (below).
- *Mark*: a mark stub while tests run (Tests, below).
- *Hooks*: trace (calls and returns, the return through a trampoline the stub puts in the
  return address), exact counts and times, a breakpoint that parks the thread with its stack,
  sampling one call in N. Effects and allocations are traced by hooking the functions that make
  them (the syscall wrappers, the allocator's entries). What only changed code can see (each
  block entered, overflow and bounds checks inside one body) is not a hook: it is another
  program, keyed as such, outside this design.

`toggle(slots, hooks, on)` from the viewer sets each slot's hooks in the parent and sends
`Hooks{slot, hooks}` (the whole set); the control thread turns the hook stub in or out. Nothing
is compiled, so a toggle cannot fail, waits for nothing, and holds even before the function has
code (the stub then jumps to the need stub). `tiered` publishes optimized code whatever is
watched: a stub stays in front of the new body.

**Slots** (`slots::Slots`, in the program's control thread). Two words a function, each one
`u64`. The *slot word* is what callers load and jump through: the need stub, the function's
body, or the slot's watch stub. The *body word* is the current code: the need stub or a body;
a watch stub does its work, then jumps through it. (In the core a word is 0 for the need stub,
a stub bit with the watch, or body + 1 with a tier bit; in the real table an address.)
`publish(slot, tier, code)` writes `Place`, then `Body` (the body word), then `Slot` only when
the slot word held the old body word (no stub in it), then, if a body was replaced, `Epoch`;
publishing the code the body word already names is refused (`Same`). `observe(slot, watch)` and
`plain(slot, watch)` change the slot's watch and write `Slot`: the stub for what is still
watched, or with nothing left the body word as it is now, never one remembered from when the
stub went in. The control thread is the one writer of both words: a stub's runtime call (a
count reaching zero) queues its slot and stores nothing, so a publish and an observe or plain
never race as two writers on a word, in whatever order their requests come; writes are carried
in order, and an observe or plain may be issued while a publish's writes are still on their way.
The simulation holds that neither loses the other's store (with every write carried, each slot
word is exactly the stub of the asked watch or the body word, and each body word the newest
publish), with a planted `PublishOverStub` caught. Retirement is blaze-launch's protocol with
one body per batch (a first call and a promotion mean what they replace, so no gate): a thread
reports the epoch it loaded before walking its stack; it clears each retired body stamped at or
before that epoch that no frame of its stack is in; a body is freed once every thread alive at
its stamp cleared it or ended; a thread born later is told the epoch its parent loaded. A stub
is no safepoint: a thread between its load of the body word and its jump is held as a caller
between its load of the slot word and its call is.

**Heat** (`heat::Heat`, in the control thread). While a function is fast its slot word is a
count stub, which counts down a per-function `u32` in a data table beside the words, set to
`bound` when the stub goes in (a plain decrement: a lost one only delays the function a call).
At zero the stub's runtime call queues the slot; the control thread hands it to
`reached(slot)`, which makes the function hot exactly once per run (a stub that fires again
before its word is gone, or a late event, is nothing), then turns the count stub off
(`plain(slot, HEAT)`): a hot function stops paying for being counted, at most `bound` calls
after its first. Relax measured about 5% of cycles in count stubs before removing them at its
last tier; here they stop at hot, before the optimized tier. The count is of calls, not decayed
by time: a function called `bound` times is worth one background compile whenever the calls
came, and that compile never takes the worker kept for a thread's own function. `bound` is a
`Config`, to be measured on reth and tempo.

**Tier** (`tier::Tier`, in the parent). Per slot: `Fast`, `Queued`, `Optimizing{snapshot}`,
`Optimized`, `Failed`, `Dropped`. `hot(slot, instance)` -> `Optimize(job)`; the `Optimizer`
trait: instance + the callees it may inline (instance and code identity) -> `Optimized { code,
inlined: [(instance, code identity)], transitively }`; `done` -> `File{slot}` and
`Publish{slot, code}`. A job begun before a newer snapshot (another run of the checkout began on
newer text) is dropped when it ends; what is queued then never starts. `callers_of(code
identity)` is the reverse map `inlined_into`.

**Registry** (`registry::Registry`, in the parent). An entry file `run-<pid>-<start>` (pid and
the OS's start time of the process, so a reused pid is another name), 88 bytes, written whole by
rename; removed at the run's end. `read(listing, processes)` lists the live runs and removes
each entry whose pid has no process or a process of another start: a read never lists a dead
run. Beside it, `<name>.events`: the `RunEvent`s (began, compiling X, published X, tiered X,
failed X, test passed or failed, memory, ended) the machines push as `Append`, written each
flush as one blaze-format `events::Batch` in columns (`kinds`, `at`, `values`, `keys`, and
`checkouts` for the `Began` rows), each buffer at an `ALIGN` multiple so a viewer maps it in
place; opening one is O(1) in its rows. Keys and numbers only, names coming from records.

**Tests** (`suite::Tests`, in a test run's parent). A test run is a program run whose entry calls
each test. While tests run, each slot word is a mark stub (`MARK`), which stores 1 to the slot's
byte in the calling thread's bitmap and jumps through the body word: one store per call, in the
stub, only in a test run. At a test's end the control thread reads and clears the bytes set in
the bitmaps of the test's threads (its worker, and the threads it started, which the launcher's
`pthread_create` ties to it); `entered(marked, ids, inlined)` takes those slots and adds what the
optimizer inlined into each optimized body among them, since an inlined callee is never called
through its own slot. The cost is a scan of one byte a slot per test thread (a few microseconds
for 100k slots). `record(test, verdict, entered)` keeps postings from code identity to tests;
`affected(changed)` is a lookup per changed identity, plus every test with no record.

**Export** is `blaze build` (End state, above). What follows applies to it.

**Export takes what runs observed** (the owner, 2026-11-15: "blaze export ideally should take PGO
data from the previous blaze run"; decided, not built). A run or a test run leaves a profile
record: for each function instance it ran, how often (the count stub's count up to `bound`, then
hot), at each indirect call the callees seen and their share (the value hooks, one call in N),
and branch counts where the fast tier counts them. It is keyed per instance by code identity, so:

- after an edit every unchanged function keeps its profile: the stale profile of whole-program
  profile-guided builds does not arise;
- profiles from several runs and from tests add (with a decay per run), and a profile is a record
  like any other, so CI's benchmark run can publish one that every export reads;
- collecting it costs nothing new: the counts come from the fast tier's stubs, which is the code
  a program runs while it is being worked on. Sampling the program counter among functions
  already optimized, for their relative weight, is an option (`blaze run --profile`), off by
  default.

`blaze export` uses the merged profile for: which functions get the costly optimizations and
which are built small and placed apart; inlining, hot edges first within a size budget; a direct
call behind a guard where one callee dominates an indirect call; block order and the split of
hot from cold inside a function; the order of functions in the file, callers beside their hot
callees, so a start faults fewer pages. The profile is an input like any other: the exported
file's key holds its digest, the export says which profile it used, `--no-profile` exports
without one, and a release names a profile record by digest or uses none, so the same inputs
give the same bytes (rule 25). The optimized tier a RUN publishes stays as it is, with nothing
observed in its key: one record shared by every run.

## Records, keys and read lists

| Record | Key | Read list / validity |
|---|---|---|
| Fast code (`CodeRecord`, blaze-keyed tag 27) | `CodeKey`: the instance's template identity and the fast tier; nothing observed (stubs are the run's) | none beyond the key: a callee's edit moves no caller's fast code (calls are through slots) |
| Optimized code | the instance at the optimized tier | read-addressed: its shape is the inlined list, each fact a callee's code identity; a changed callee is a new address, so the stale record is never found and costs nothing |
| Program manifest | the checkout's certificate and the entry | slot -> (instance, code identity, fast record key); what the run reached last (`history`) |
| Test result (`TestRun`, tag 8) | the test's base key | its asks: the code identities it entered, and its outside reads |
| Findings of a refused compile | as the compile's | as the compile's |
| Registry entry, event batches | not in the store: a live run's state, gone with it (batches are blaze-format buffers) | — |

## What crosses the boundary, and how

The need stub that exists is blaze-launch-rt's: a slot's word names it (`arch`: `adr x16, .;
b head` on AArch64), whose resolver saves the argument registers and calls `table::resolve`,
which pushes `Event::Need { slot, thread, reply }` on the control thread's queue (a lock-free
`SegQueue`, not a channel, since a stub can fire while its thread ends) and waits on a
`sys::Handoff<Bound>`; the control thread speaks `blaze_launch::wire` (deterministic CBOR) over
a Unix socket. That stays: the parent takes the daemon's place at the socket's other end. On
`Bound::Code` the resolver jumps through the word again, never to an address it was handed;
`Bound::Refused` becomes the caller's panic. blaze-lazy's hook (`blaze_lazy_resolve` over
blaze-inproc's shared-memory asks to the daemon) is the daemon's and goes. The count, mark and
hook stubs, the body word and the swap steps are blaze-launch's and blaze-launch-rt's (another
lane); they keep the rule above: a stub's runtime call queues and never stores a word.

The control thread places a known record without the parent: `Welcome` carries, for each slot
whose fast record the manifest names and the store has, its key. Only a slot with no record
crosses the socket. The warm start is the startup image (blaze-cas `plain::image`): the code
records the last run of this manifest placed before `main`, in order, under `IMAGE_TAG` keyed by
`StartupImage::key_for(manifest)`. Before the start the driver looks it up and hands it to
`Parent::warm(Some(rows) | None)`; every row's slot is looked up in that one call, the driver
reads them in one pass in the store's `read_order` and places them before the child's first ask,
which fills the Welcome's known slots. It is a hint, never a proof: a row is found by its slot's
key as any need is, and one the store lacks is an ordinary miss. The parent keeps what it placed
before `main` (`Parent::image`); when `main` returns, `ended` pushes `WriteImage` if the set or
order changed, put with the run's other records. The simulation holds that a run with an image
asks for exactly the records a run without one does, each once, the image's all first and in one
pass. New messages on the wire: `Hot{slot}`, `TestEnded{test, verdict, entered}`,
`Publish{slot, tier, record}`, `Hooks{slot, hooks}`, `Failed{slot, error}`; from the viewer to
the parent,
`Toggle{slots, hooks, on}` (over the parent's own socket `<name>.sock`, mode 0600, beside the
registry entry; `viewer`: the array `[1, 0, hooks, on, [slot...]]` in deterministic CBOR, a
non-shortest head, a hook bit no hook has, a trailing byte or slots past the caller's cap
refused by name).

## Floors of the live path (live patching, later)

The run path's floors are the End state's. These are the live machines'.

- **One stub miss with the record present** (known at Welcome): resolver, queue, wake of the
  control thread (a futex: 2 to 5 us), one `pread`, copy and fixups (about 1 us a KB), `Store`,
  wake of the thread: floor about 10 us. Without a record it is that plus a socket round trip
  (10 to 20 us) plus the compile. The daemon-era path was a round trip on every miss.
- **One publish.** The bytes written into the area once, an icache flush of their lines (and
  `membarrier` on Linux AArch64), two atomic stores (one under a stub), an epoch store when a
  body is replaced. Floor: under 1 us past the copy.
- **A call.** Into a function nobody observes: a load of the slot word and an indirect jump, as
  before. Through a stub: that, plus the stub's work (a decrement and a branch; a byte store)
  and a second load and jump through the body word. The floor of observing nothing is nothing.

## Reused, mirrored, deleted

- Mirrored from `blaze_lazy::fetch::Fetcher`: single flight, waiters in ask order, priorities
  (`Asked`, `Callee`, `History`), a worker kept for asks, a failure answered to every asker. Not
  kept: its daemon waiter (client, seq), and `Stop` on the first failed ask (rule 20 now).
- Mirrored from `blaze_launch::launch::Launch`: the epoch protocol, its error names, the planted
  `FreeEarly` control. `Slots` drops the gate and multi-body batches (edits while running are
  not now); when live patching comes, `Launch` is the core and `Slots` folds into it.
- Reused as is: blaze-launch-rt (stubs, resolver, queue, `Handoff`, `drive`, `records`, `tls`,
  `unwind`, `symbols`), blaze-launch's area and placement, blaze-keyed's `CodeRecord` and
  `TestRun` kinds and read-addressing.
- Deleted when the drivers land: blaze-lazy (hook, wire, stubs, fetcher, runner, session, plan),
  blaze-inproc's code memory and host for runs, blaze-launch-host's daemon side, the
  `BLAZE_LAUNCH` switch, blaze-stencil's count-driven tier ladder (`Heat` and `Tier` replace it).

## What a program is given (decided 2026-11-15, not built)

The owner's direction: `blaze run` finds the top of the repository as Bazel does, gives a program
a standard environment, and a program gets a capability only when it is granted, as in Deno; a
program on Rust's std has to be let through by name.

**The root.** blaze walks up from where it was started to the workspace root: the nearest
`blaze.lock` or workspace `Cargo.toml`, never past the top of the git checkout. Every path in a
key, a record, a finding and the program's own view is relative to that root, so a command gives
the same answer from any folder inside it. With no argument the target is the package the start
folder is in; a target can be named from anywhere by its path from the root. (This replaces
"name the checkout from the folder above": there is no such argument.)

**The environment.** A program started by `blaze run` or `blaze test` gets a fixed one, not the
caller's: its current directory is its package's folder; its environment is empty but for three
names, `BLAZE_ROOT` (the workspace root), `BLAZE_PACKAGE` (the package's folder from the root) and
`BLAZE_START` (where the person stood, from the root); a caller's variable reaches it only by name
(`--env NAME`, or the manifest). Reason: what a run or a test read is its read list, and a test is
reused only if that list is complete; an inherited environment is an input nobody listed.

**One spec, written down.** Everything a started program is given is one value, the run spec:
its arguments, every environment variable by name and value (the three above are in it like any
other; nothing is in the environment that the spec does not list), its current directory, its
grants with their scopes, its limits (memory, time), and where its input and output go. It is the
sandbox spec of std.md's "Sandboxes", so there is one such type. `blaze run --spec` prints it as
JSON without starting anything; `blaze run --spec-from FILE` takes one (read strictly: an unknown
field is an error); a bstd program can read its own through `Io`. Its digest joins the program's
identity in the key of a run or a test result, so a test is reused only under the same spec, and
"why did this rerun" can answer "the spec changed: this variable".

**A program says what it takes, in `main`'s signature.** Not a bag of everything: each capability
is a parameter, its type the kind and its name the grant's name:

    #[bstd::main]
    fn main(cx: &Cx, out: Out, data: Dir<ReadOnly>, api: Net) -> Result<(), Error>

The signature is the program's manifest: blaze reads it at compile time, without running anything
and without an analysis. A parameter with a default (arguments, input, output and error, the
clock, memory, the core count: nothing that reaches outside the process) is filled by itself; any
other is filled by a grant of that name, from the command line (`--grant data=fixtures`,
`--grant api=api.example.com:443`) or the package's manifest, where a reviewer reads it. A
parameter with no grant is a refusal before the run that prints the line to add; never a prompt,
never a failure halfway. A test says what it takes the same way
(`fn reads(cx: &Cx, tmp: Dir<ReadWrite>)`), and the runner fills it. `Io`, the bag `main` takes
today, stays as the grant of everything, for a script, and `blaze run` names it as that.

**Capabilities are values.** A capability is narrowed and handed on (`data.subdir(..)`,
`.read_only()`), never widened and never made from nothing, so a library, a dependency included,
reaches only what a caller hands it: authority is per value, not per process. blaze can print, per
crate, which kinds of capability flow into it.

**How a run is held.** A capability is a value with a provider behind it, and every reach outside
the process is one typed effect handed to bstd's driver, so a run can be held at that one seam
with no change to the program (decided 2026-11-15, not built):

| command | what the program's capabilities are | for |
|---|---|---|
| `blaze run` | real, as granted | the ordinary run |
| `blaze run --dry` | real to read; every write, send and spawn goes to an overlay the program sees and the world does not; the changes are listed at the end, and `--apply` makes them | seeing what a program would do |
| `blaze run --fake` | the simulated world the tests use: files in memory (seeded from a folder if named), a network that answers from a script or refuses, a virtual clock, a seeded generator | the same output every time; no grants needed |
| `blaze run --record FILE` | real, and every effect and its answer is written down | an audit; a bug report that carries its inputs |
| `blaze replay FILE` | every effect answered from the record; a different effect stops with the first one that diverged | reproducing a run exactly, anywhere |
| `blaze run --ask` | real, but each new kind and scope of reach asks first: once, always (it prints the grant line to keep), or no (the call gets a typed refusal) | running a program not yet trusted |

Each is a field of the run spec (per grant: its provider and its layers), so `--spec` shows it and
a result's key holds it. A test's parameters are fake by default: a folder in memory, a virtual
clock, a seeded generator, no network; that is what makes a test's result a function of its key.
An ambient program (below) cannot be held this way: there is no seam inside it; for it the
operating system can refuse and log file paths and connections, and nothing more.

**Record and replay are built for a hosted run (2026-11-16).** The host wraps them around its
table (crates/blaze-std/src/hosting.rs, `Hold`), so the program does not change. A call that
fails the host's range or handle checks never reaches the world and is not held. `Record`
writes down each call that passes, with its answer and the bytes the answer put in the
exchange. The file is deterministic CBOR, `["bstd.record", 1, [call..]]`, and it is read
strictly: every length is bounded, a record of another version is refused, and nothing may
follow the record. `Replay` answers each call from the record and asks the world nothing. It
stops at the first call that is not the record's next one; `Diverged` names that call, what
the record held there and what the program made, and every later call is refused.
`Holding::of_spec` reads the spec (below). Provider `replay` needs a record handed in; the
`record` layer gives `Record`. A grant's own provider or layers are refused by name until the
wire carries grants' effects apart.

**Dry, ask and fake are built for a hosted run (2026-11-16).** All three sit at the same seam as
record and replay, written once over the effect stream in a sans-IO module
(crates/blaze-std/src/layers.rs) that the host drives from `Hosting::through`. None is a mode of
a capability, and the program does not change. The escape now carries the file effects
`ReadAll`, `Write`, `Append`, `Remove`, `Rename` and `MakeDir` beside the timer and randomness;
the host reads each name with `FileName::new` (no absolute name, no `..`) and bounds every
length.

What counts as changing is one function, `layers::changes`, written over every `Effect` by name,
so a new effect does not compile until it is placed. An effect changes the world when it leaves
something outside the run that was not there before, or takes something away: a file's bytes,
names, modes, times or durability, a directory, a lock, a process, a socket listening or
connected, bytes sent. Reads, lists, watches, the clocks, timers, randomness and name lookups do
not. Standard output and error do not either: they are the run's report to the person who
started it, so a dry run still prints what the program prints.

| CLI | spec | the host's `Holding` |
|---|---|---|
| `blaze run --dry` | layer `overlay` | `dry: true` |
| `blaze run --ask` | layer `ask` | `ask: Some(decider)` (the driver's terminal prompt) |
| `blaze run --fake name=provider` | provider `fake` | `fakes`: `clock=fixed:NS`, `fs=memory[:DIR]`, `CAP=recorded:FILE` |
| `blaze run --record FILE` | layer `record` | `hold: Hold::Record`; the host's `record()` written to `FILE` |
| `blaze replay FILE` | provider `replay` | `hold: Hold::Replay(Record::read(FILE))` |

- `--dry` withholds every changing effect: it is not performed, it is answered with the
  completion of its success (`layers::performed`: `Written`, `Removed`, ...), so the program goes
  on as it would, and it is listed in `Hosting::withheld()` in order, with its call's place. An
  effect whose success holds what only the world can say (a spawned process, a connection) is
  answered `Failed`, naming why. Reads are real. `Hosting::report()` is what the run prints at
  its end (`layers::Report`): one line per effect not performed, and, when a read followed a
  pretended change of its name, a first line "N later reads may differ from a real run".
- `--ask` hands each changing effect to the host's `layers::Decide` before it is performed:
  `fn decide(&mut self, question: &Question<'_>) -> Decision`, the question holding the call's
  place, the effect's kind (`write`, `rename`, ...), its scope (the name, or `from -> to`) and
  the effect. There are exactly three answers, and the prompt offers no default on Enter:

  | answer | the effect | the program is told | the report |
  |---|---|---|---|
  | `yes` | performed | what the world answered | nothing |
  | `no` | not performed | the wire's typed refusal (`Refusal::Refused`, the shape an ungranted capability gives), never a success | `refused` |
  | `pretend` | not performed | it succeeded, as `--dry` tells it | `not performed (pretended)` |

  So `pretend` throughout is the dry run, `yes` throughout is the plain run, and a person who
  answers `no` to "remove this file?" never leaves the program believing it is gone. The CLI's
  decider asks on the terminal and refuses `--ask` with no terminal; a test's is a script.

Written gaps, not to build now: there is no overlay, so a read after a pretended write sees
the world, not the write (the report's first line says how many reads that was); `--apply`,
which would perform a dry run's list, is not built.
- `--fake` puts a stand-in behind a capability (`layers::Fakes`): `clock=fixed:NS` answers both
  clocks with `NS` and fires every timer at once; `fs=memory[:DIR]` answers the file effects from
  a `layers::Folder` in memory, filled from `DIR` by the CLI, and the world's files are never
  touched (`Hosting::folder()` gives it back at the end); `CAP=recorded:FILE` answers the escaped
  effects of one capability from a record, each by the next recorded call with the same bytes,
  and refuses a call the record lacks. The network is the capability this is for; the wire does
  not carry its effects yet, so today it serves `rng`, `clock` and `fs`.

Layers stack: record outside, then dry and ask, then the provider (a fake or the world). So a
faked run, dry or not, can be recorded, and the record holds what the program saw. A replay
answers from the record alone: it reaches no layer, no fake and no host effect, and
`Holding::of_spec` refuses a replay with a layer or a fake (`ReplayHasNoLayers`), a fake
provider with no stand-in named (`NoFake`), stand-ins under a real provider (`FakeNotAsked`) and
an `ask` layer with no decider (`NoDecider`).

The tests (crates/blaze-std/src/tests/layers.rs) make seeded plans of any calls a program can
make through the table, as a hostile program would. A dry run leaves the world as an untouched
twin, and reports every changing call in order, answered as performed. A run answered `No`
throughout equals the dry run, answer for answer. A scripted run is asked exactly the changing
calls no earlier `Always` covered, withholds those answered `No`, and leaves the world that a
real run of the rest leaves. A faked run, dry or not, leaves the world untouched and replays
byte for byte on another world, which is asked nothing. Each layer has a planted bug the oracle
catches: dry lets one change through, ask performs one unasked, a `no` reaches the host, a
`no` is answered as a success, a replay reaches the host.

**For a successor (2026-11-16).** Built in bstd, uncommitted at this writing in the
rebuild-1115 tree: `layers.rs` (the rule, the gate, the report, the fakes), the host's seam in
`hosting.rs` (`through` then `layered` then `faked`), the escape's file effects in `guest.rs`
(`codec`, `Escaped`), tests in `tests/layers.rs` and `tests/hold.rs`; `lanecheck.sh blaze-std
test` passed 252. The CLI side (the flags, the three-answer prompt, the printed report) belongs
to the blaze-cli driver lane, wired through `Holding::of_spec` then `Hosting::holding`; tell it
when either signature moves. Next, in order: the hosted wire's other effects (a native run
holds every `Effect`, a hosted one only those the escape carries; `changes` already places
them all), the network's effects so `--fake net=recorded:FILE` serves its purpose, a grant's
own provider and layers, then the two gaps above. The test plants borrow unrelated `Plant`
variants as stand-ins (named in the test file's header); a `Plant` of their own would need the
long match in `Cx::until` touched.

**The types are the enforcement, for a checked program.** A program is checked when no code `main`
can reach is `unsafe`, foreign (`extern`, linked C, inline assembly) or on Rust's std, outside the
crates the workspace trusts by digest (bstd and blaze's own collections first). blaze decides that
at compile time, item by item. In a checked program nothing can forge or widen a capability, so
its grants hold by the language alone: no operating-system sandbox, no cost at start, the same on
every system. (The owner's correction, 2026-11-15: the capability model is for programs with no
`unsafe` and no C, and those need no sandbox.)

**Every other program is ambient.** `unsafe`, C or Rust's std outside the trusted crates means the
program can make any system call: blaze cannot say what it needs, or hold it to grants by type.
It runs only when that is said by name: `--ambient` on the command line or `ambient = true` in
the manifest (working name; `rust` and `suggest` projects in the adoption steps carry the manifest
line). Without it, `blaze run` refuses and names the items that made the program ambient. Build
scripts, proc macros and sandboxed tests keep the operating system's sandbox (rule 9); whether an
ambient `blaze run` also gets one is open.

**How the spec reaches the program: a pipe on file descriptor 3** (built 2026-11-15 in bstd:
`grant`, `spec`, `rt/launch.rs`). The parent writes `bstd spec\n` and the spec's JSON into a pipe,
closes its end, puts the read end at descriptor 3 and sets `BSTD_RUN_SPEC=fd3`. Why: an argument
is visible to `ps` and bounded in length, and it shifts the program's own arguments; a variable
is inherited by every child the program starts and is part of the environment the spec describes;
a file needs a path the program can see and someone to delete it. A pipe holds any length, is gone
when read, reaches only this process, and needs no `unsafe` (opened as `/dev/fd/3`). The variable
says the descriptor is the parent's: an inherited 3 (a jobserver's pipe) is never read. No mark
is an ordinary start (only what fills itself, and `Io`, is filled); a marked descriptor that does
not close within 5 seconds, or holds anything but the header and a spec, is a refusal.
`bstd manifest\n` instead prints the program's manifest as JSON and runs nothing.

Departures, as built: an `Io` parameter fills itself on an ordinary start (so every
`fn main(cx: &Cx, io: Io)` runs from a shell as before) and needs `--grant io=everything` under a
spec; standard output and error are both `Out`, told apart by the parameter's name (`out`, `err`);
input and the core count are not parameters yet (bstd has no types for them).

Owed: the grant's place in the manifest; the trusted-crate list (who may add to it, pinned how);
the `record`, `overlay` and `ask` layers and the `fake` and `replay` providers for a native run
(built for a hosted one, above; the native launcher refuses them as not built); the overlay
that lets a dry run read its own writes, and `--apply`; what a test's parameters default to.

## The hosted world (decided 2026-11-16; wire, program side and host side built)

Under `blaze run` the parent process already holds a complete driver: blaze is a bstd program. So
a program blaze compiled carries none of its own. Its `Io` is backed by a provider that forwards
each reach across a small C table to the host's driver (`bstd::wire`, `bstd::guest`,
`bstd::hosting`). The program's side is then small, all of it compiled by blaze from bstd's
source. Every way of holding a run (record, replay, fake, dry, ask) is the host wrapping its
driver, with no change to the program. A program exported to run alone (`blaze export`, later)
links the native backend instead.

- **The start.** `#[bstd::main]` also makes
  `extern "C" fn bstd_hosted_start(table: *const HostTable) -> i32`. It checks the table's
  version, size and exchange before calling any entry. Then it asks the host for each of `main`'s
  parameters by kind and name (`grant`), and ends as refused (77) at the first one withheld. Then
  it runs `main` and returns its status.
- **The table** (`wire::HostTable`, version 1, `#[repr(C)]`) has these entries:
  - `write(stream, at, len)`
  - `clock(which)`
  - `arg(index)`
  - `grant(kind, name)`
  - `effect(at, len, budget)`, the escape
  - `exit(status)`, which does not return

  Each entry answers with two integers, a code and a value. A hot reach is one indirect call, with
  no allocation and no encoding.
- **The escape.** Any other effect is sent as deterministic CBOR (`guest::codec`). Version 1
  carries `Timer` and `Random`. Any other effect is failed on the program's side, naming it,
  before it crosses.
- **Trust.** Every byte crosses through one exchange buffer that the host owns and lends. Each
  byte argument is a range in it, and the host checks each range against the buffer it made
  before reading a byte. It checks every handle too. Anything else gets a typed code
  (`UNKNOWN`, `OUT_OF_RANGE`, `BAD_BYTES`, ...), and an entry never panics across the boundary.
- **Waiting.** A program is one thread for now, and its side of each entry is a plain call. A
  blocking effect is waited for by the host inside the call, under the call's budget. When
  programs get tasks of their own, the escape splits into `submit`, which returns a tag, and
  `poll` under a deadline, which parks the worker on the host. Each worker then gets its own
  exchange.

Owed: the environment, standard input and host facts on the wire. The rest of the effects on the
escape. Grants of folders and the network (only `io`, `out`, `err`, `args` and `clock` are made
from a hosted world today). The loader in `blaze run` that calls `bstd_hosted_start`.

## Open questions

1. Who owns the slot table across runs: slots are fixed for a run; a later run of an edited
   program numbers its own. A stable numbering (by instance) would let the history record be
   slots; today it is instances, mapped at start.
2. Whether a newer snapshot should also stop the parent's speculative compiles of this run.
3. Live patching (not now): an edit while running makes `callers_of` the set to take back to
   fast code, through `Launch`'s gate.
4. A thread that never reaches a safepoint holds every retired body (blaze-launch-rt has no
   scan from outside yet): with one run per program and no edits, only promotions retire, so
   the cost is the old fast bodies' memory until the program ends.
5. The parent's own footprint with the front end resident for compiles; rule 50.
