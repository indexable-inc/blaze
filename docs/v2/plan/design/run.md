---
status: plan
summary: "`blaze run` and `blaze test` from checked bodies to a running program, with no executable written (bottom.md section 11). End state (corrected 2026-11-16): a code record per instance and tier, position-independent, every outside reference through an import word that is the run's slot word; a stub a slot until its first call, which places a known record in-process or asks the parent (`blaze run` itself) over a socket pair, the record made then if absent; one placement, one release store, every waiter woken; callees compiled behind on idle cores; dependency bundles mapped in place; a repeat places the last run's records ahead of `main` in about 5 ms. Then the gap and the pieces. Below: blaze-run's sans-IO machines (Parent, Slots, FirstCall, Heat, Tier, Registry, Tests), the run path's."
date: 2026-11-16
---
# Run and test

Andrew, 2026-11-15: no daemon; "make it exist properly all in one go"; "everything in JIT world".
There is no `blaze build` (bottom.md section 11). The end state below is the run path, and the
blaze-run machines after it (Parent, Slots, FirstCall) are its parts: decision R1 ("nothing on the
run path calls them") is withdrawn. Still later: an edit applied to a program while it runs
(blaze-launch's staged edits and gates); now an edit is a new record, published on the next run.

## End state (D113, 2026-11-16 evening: the whole run, redone)

Andrew, 19:30, after a hello world run took 15.9 s ("thought that libraries were cached? it should be sub
second"): "completely redesign blaze run to be sota": incremental time first (large crates and files, after a
restart, nothing running), then the remote cache, memory, cold time, disk cleanup that never blocks, disk use;
128 cores best, a typical Mac good; shared across worktrees with no setting; only changed tests rerun.

**The fault being removed.** Measured 18:12 on hello world: 16.2 s of 16.3 s is library work redone on every
run (names of core, alloc and std 5.1 s; 18,900 library bodies checked and lowered 9.3 s; instances built
1.8 s; the program's own code about 10 ms). The run path checked a body BEFORE it could compute the key of its
code, held a table of the whole program, and resolved every crate's names up front. `blaze check` on the same
tree repeats in 2 to 3 ms (28.5 M instructions, measured 19:0x) because its answer is found by key.

**One rule.** A run does work in proportion to what changed and to what the program then executes, never to
the size of the program, its crates or its files. Seven stages; the first four ARE `blaze check`'s request
machine (blaze-drive), not a second mechanism (rule 30); the last three are the child and the parent below.

1. **Sweep** (blaze-cli sweep.rs, built, as `check` has it). A repeat stats exactly the paths the LAST ANSWER
   READ (its leaf table in the checkout's pointer file: path, a 40-byte stamp of length, mtime, ctime, inode
   and device, and the content id); it never walks the checkout, so a `target/` of 374,012 entries beside the
   sources costs nothing (measured), and a locked dependency is not looked at. The stamp is a HINT that decides
   only whether a file is read and hashed again: every key is from content, and a task that later reads the
   file hashes what it read and stops with a typed `Changed` when that is not the id in its key (a command
   works on the tree its sweep saw, or says the tree moved; it never files new bytes under an old id). A
   stamp younger than the sweep by less than the volume's time grain is not kept, so that file is hashed again
   next time (git's racy-clean rule; the planted bug `RacyTrusted` is caught). The grain and the clock are the
   VOLUME's: a local volume's grain by its file system (10 ms where stamps are finer, 1 s or 2 s where they
   are not) against the machine's clock; a volume whose clock is not the machine's (a network or shared
   folder) against the stamp of an unnamed file made on it as the sweep starts. The device number is not in
   the stamp (it moves across restarts on Linux). Serial on purpose at today's sizes: tempo's 870
   read files cost 0.51 ms at a stat each; the pool and one bulk read a folder were measured slower. At 100k
   read files the same loop is about 60 ms, which is over the floor: there the stats go to the pool by folder
   (`Effect::StatMany`, owed) and, where the platform keeps a change journal across a restart (macOS FSEvents
   by event id; blaze-vfs watch.rs has the watcher, unwired), the journal narrows which rows are statted; a
   journal that says "history lost" narrows nothing. No daemon is needed; a helper with an expiry may hold a
   watch and narrow the same way (rule 70).
2. **Certificate** (blaze-drive `cert_key`, as built): the tool, the command and every input's digest. When it
   stands, the gate's answer is found and the start list (stage 7) is placed as it is: nothing is resolved,
   checked or built. Its key is the root of the leaf table kept as a tree by directory, recomputed along
   changed paths only, never over every file. Certificates, answers and pages are keyed by content in the
   user's one store (drive.md, "The store under the command"); only the leaf table (path, stamp, content id)
   is a checkout's, with the last answer's key for EACH request shape, so `check`, `run` and `test` in turn
   re-hash nothing. A NEW worktree hashes its files once (blake3, all cores) and then finds every record by
   key (D96): worktrees share with no setting. Owed, and designed: a file git tracks and its index calls
   clean is not read at all: its content id comes from a kept map (git blob id to content id; local, or an
   organisation's from its trusted builder), so a branch switch or a pull reads only what nobody has seen.
3. **Forward** (the drive's request machine, as `check`): from changed files to changed ITEMS, then to the
   tasks whose read lists name a changed fact. A body edit moves no other task's KEY; what the body MAKES is
   an output with its own digest, and its readers rerun when that digest moves: the hidden type behind an
   `impl Trait` or an `async fn` (its auto traits for a caller's check, its layout for a caller's code), a
   const's or static's value, an item written in the body that the whole crate sees (an `impl`, an exported
   macro), and the lines of the items below it (drive.md, "D114 rulings", "Facts a body makes"). So an edit
   that grows one future re-lays the states that hold it, and an edit that leaves every made fact equal moves
   nobody. No dependency graph is built or held.
4. **The gate**, BEFORE the start. Scope: the WORKSPACE bodies the program reaches (AGENTS.md, Scope): the
   request's root is `main`'s check; each `BodyCheck` record names the items its body calls or names, and the
   machine's ordinary need-walk follows them (a call through a trait or `dyn` reaches every impl the program's
   crates hold: a superset, front2 reach.rs's rule). There is no table of the program: the reach is the
   request's task set, kept with its answer, and after an edit only the changed bodies' new names extend it.
   Each check is found by key or made at item grain on every core. An error stops the run before the program
   starts: a program that does not type-check must not have run half way, and with checks found by key the
   gate costs milliseconds (cold, it is the workspace's reached bodies, once). `Gate::Beside` stays for
   `test` only, verdicts held until the check ends. A DEPENDENCY's bodies are not in the gate: their checks
   come by key from the shared cache; one that is missing is made at its code's first ask, and a refusal
   there is blaze's own failure (that crate builds on the pinned nightly), reported as one, naming the body.
   Starting at once with every effect held until the gate settles (output commit) was weighed and refused: it
   wins only while the gate is slow, which this design removes, and a held network send blocks the program
   anyway.
5. **Code is a drive task like a check** (D106, corrected). `Code(instance, tier)`: key = the instance's
   symbol, its body's text key, the builder's producer id (D105), target, tier, the profile's facts (the
   panic strategy among them); valid by that key, or else by its READ LIST checked against today: the
   `BodyCheck` record it was lowered from, each callee as a name (never its body, unless a tier inlined it:
   then that body's `BodyCheck` too), each signature, const value, static initialiser, vtable shape and
   hidden type whose layout or value it used, each by its own digest. No crate-wide fact is in it: adding a
   `pub fn` to a base crate moves no code record above it. `Code` TAKES `BodyCheck`'s record as its proof: the
   record holds the typed body (what lowering needs), so a code miss lowers and builds and never checks again
   (rule 74; `make_instance`'s second check and its second front-end entry are deleted). Code is asked for
   lazily, at a first call, or behind (stage 6); nothing is walked or built before the start except the start
   list's rows. A code record holds positions relative to its item; a panic's location is DATA placed from
   the file's `Lines` record (item to line), so lines moving above a function re-place a few words and
   rebuild nothing.
6. **Start before everything is built**: the child and the parent below, as corrected this morning: slots,
   stubs, first calls, compiled behind in the last run's order. Always in its own process (D103).
7. **After.** Two things, apart. THE START LIST: a hint, one mutable root a (program, request shape) and
   user: the certificate it was written under, then the instances first called inside the start window (a
   cap of 8,192 rows), each as (symbol, the code record's key then), in first-call order. Next start, if the
   certificate is the same one, the rows are placed as they are, nothing looked up; if not (an edit, an undo,
   another worktree), a row is only a symbol: its code is found through the drive like any first call, so an
   old list can cost time and never run old code. Last writer wins; it is never shared by content, because
   its order is a fact of one run. A TEST'S RESULT (test.md, "End state", which this follows and does not
   restate): a record whose asks are the functions the test ENTERED (mark stubs; placing ahead is off in a
   test's child), each answered by its code's identity today, and every outside read (a file through a
   grant, a variable, a host fact), traced at the seam and added to the checkout's leaf table so the sweep
   sees it. A test that started a thread is judged by its static reach. A plain `#[test]` on std is kept the
   same way under the hosted world: its entropy is seeded from its id, its opens and reads are traced; one
   that read the clock is kept (a pass is one valid sample of this code on these inputs), one that used a
   socket beyond loopback or an unsandboxed spawn is not, by name. A suite's asks are stored as one common set
   and each test's difference. A helper with an expiry tiers hot functions up from samples (backend-next.md)
   and writes records; it never builds on its own.

**Floors** (ms; "measured" where a run in the notes gave it; the rest derived and to be replaced by spans):

| Stage | hello world | 1M lines, 100k files | Today |
|---|---|---|---|
| blaze starts | 1.5 | 1.5 | target.md |
| sweep, nothing changed | under 0.1 | 60 serial (0.6 us a stat, measured on tempo's 870: 0.51); 2 to 8 with `StatMany` or a journal | tempo 0.51 measured |
| certificate, one read | 2 | 2 | hello 2 to 3 measured; tempo 13 against a floor of 2 (drive.md) |
| start to `main` (spawn 1, maps 0.3, placing behind) | about 5 | about 5 (placing is behind `main`) | 15,900 (ratio 3,000) |
| one body edited: + hash and split its file, 1 check, 1 Fast build | about +3 | about +3, plus 2 a MB of the edited file (target; the outline is 12.8 a MB today, tempo.md) | 110 on `check` (measured); run: 15,900 |
| a signature edited: + its readers' checks | readers x one check / cores | same | every body of the file (drive.md, D114) |
| a `pub` item added in a base crate | one outline, its askers' cell checks (1 us each) | same: no code record above it moves | every crate above re-solved; every code key above moved |
| another command after this one (`check`, then `run`, then `test`) | no file hashed again | same (one leaf table a checkout, a last answer a shape) | every file hashed, a full walk |
| a new worktree, equal content | hash once, then finds | 100k files hashed on all cores; none once the git-blob map is in | no memo |
| a branch switch of 5,000 files, each seen before by anyone | 5,000 finds: about 5 | same | not measured |
| after a restart, nothing changed | the stats, cold: to measure (macOS: the change journal narrows them; Linux: every stat, in batches) | same | not measured |
| after a pull CI certified | three round trips: the certificate, its pages in one batch, code by range | same | not built |
| cold, nothing in any cache | the reached workspace bodies / cores, plus names (5.1 s today for core, alloc and std alone) | bodies / cores | 16,300 on one core |

A body's check costs 0.35 ms mean in the run path's walk and 1.56 ms in the drive (both measured, on different
paths); its floor is not known until the checker has spans by phase (drive.md, D114 point 6).

The cold row is why std and the dependencies come built from the shared cache (remote-read): then cold is
the workspace's own bodies. The first run on a machine pulls by key; after `git pull`, a certificate for the
same inputs if CI made one, else records by key in batches, signed statements checked before bytes are mapped.

**Unknowns this design rests on, each owed a measurement**: a slot call's cost in call-bound code (below:
"about 1%, up to 5%"); the bulk directory read's cost after a restart with a cold cache; how many first calls
tempo makes before its first useful output (the wait compiled-behind must hide).

**The critic's findings (20:07, verdict REVISE) and where each is answered.** B1, keys that miss inputs
(a future's layout, a const's value, an impl in a body, lines): stages 3 and 5, and drive.md "Facts a body
makes". B2, a snapshot that was not one, and the wrong clock: stage 1 (verified bytes are in the tree since
20:33; the volume's grain and clock are owed). B3, test results kept unsoundly: stage 7 follows test.md
(entered, not bound; reach for a threaded test; outside reads traced; plain `#[test]` kept under the hosted
world). M1, the run record (an undo replaying old code, last writer, O(rows), per checkout): the start list,
a hint whose rows are symbols. M2, crate-wide facts in code keys and a body checked twice: stage 5. M3,
worktrees, command switches, restarts, pulls: stage 2 and the floor rows. M4, the gate said twice: stage 4
and "The gate" below; holding effects until the gate settles is refused there with its reason. M5, one
placing thread and a barrier a publish: "Who places" below. Minors: the floor table's numbers; a suite's asks
stored as a common set and differences; the parent's state dropped when idle; the pin and the run segment.

**Where the tree breaks the one rule today** (read 2026-11-16; each is a piece, in this order):
- A STALE CHECK CAN STAND: a signature edited in another file or crate that leaves its crate's exports equal
  moves no body's read (body_types.rs's own gap (a)). Correctness first: a body's read list names the
  signatures it used, each by its own digest.
- SIGNATURE GRAIN: a body's source is its item's hash and the file's text "around bodies", so one parameter
  edited re-checks every body of that file (100 in the drive's own test); the `Signature` kind is defined and
  not built. With it a signature edit costs its readers and nothing else.
- NAMES ARE PER CRATE: an added `pub fn` re-solves its whole crate (tempo: 1034 crates in 5.1 s; `CrateScope`
  runs on the main thread). The solver's cells and the per-module kinds (`ModuleScope`, `Group`: defined, not
  built) become the records, so an outline edit in an enormous crate costs its module's cells.
- THE COMMAND'S STORE IS A STAND-IN: blaze-cli store.rs `PackStore` writes one pack a command, whole, never
  synced, and merges past eight; blaze-cas's store (mapped segments read in place, generations, compaction
  with a grace period) is not what the command uses. Until it is, "read in place", crash safety and cleanup
  that never blocks are not true of what a person runs.
- `Code` answers `NotBuilt` in the drive: the run path's records are the ladder's own; stage 5 puts them under
  the drive's `Code` name with the key above.

### The child and the parent (the corrected end state of this morning, kept; D113's stage 6)

> Withdrawn from the architect's first draft (coordinator, 14:30): the eager build before `main`, the image bound
> ahead of time at fixed addresses with an edit rewriting posted words, and the patched clone. Kept: record, bundles.

Estimates unless a line says measured; every one is to be replaced by a profile's span on hello world and tempo.
**Code record**: blaze-code `record` as written (tag 27): one per instance and tier; position-independent code
and a relro part; targets by (space, symbol), never an address; sites `adrp; ldr` (Load), `adrp; ldr; blr x16`
(Call), `adrp; add` (Relro). Key: symbol, body digest, target, tier, settings, toolchain; read list for the rest.
**Slot table**: one 8-byte word a function instance, plus one word a static, vtable, thread-local and C name,
in one reservation with the code area (so every `adrp` reaches, 4 GiB). The emitter's import word IS the slot
word: placing a record points each Call/Load site at its target's word; nothing else is bound. A function's
address (a `fn` pointer, a vtable entry, a relro word) is its entry thunk (`ldr x16, word; br x16`), never a
body, so a later tier or a freed body never strands a pointer. Slots are numbered by the child as symbols are
first seen in placed records; a run has no list of functions made before it starts.
**Stub**: a stub a slot, 8 bytes on AArch64 (`adr x16, .; b head`; 16 on x86-64), in one constant segment
the same for every program (made once, mapped, never written per run), then one shared head and the resolver
(blaze-launch-rt arch.rs, as built). A shared stub with the index in a register was rejected: the caller would
pass the word's address, one more instruction at every call site for ever, to save 8 bytes a slot once. A
first call runs 3 (call site) + 2 (stub) + 3 (head) + about 24 (the resolver saves the argument registers)
instructions, then a queue push and a wait on the control thread.
**First call**: the control thread's `first_call::FirstCall` (blaze-run) owns each slot's first binding:
asked once, placed once, stored once, then every waiting thread woken to load the word again; a thread that
fires a slot already bound is sent back at once. Known (the record is named by the run record or was offered)
it is placed in-process, about 2 us (copy, relocate, cache maintenance, one store); unknown it is asked of the
**parent**: `blaze run` itself (it ran the check and holds the store), alive exactly as long as the program.
Channel: a socket pair on descriptor 3, blaze-launch's wire (deterministic CBOR, decoded with limits); the
answer names the record's place in a store pack, which the child maps read-only and reads in place: no record
byte crosses the socket. A miss's floor is one round trip, about 15 us (to measure) plus the lookup; absent,
the parent makes it then (blaze-run `Parent`: single flight however many threads or runs ask, a deadline).
**Publish**: copy through the write view, cache maintenance (macOS `sys_icache_invalidate`; Linux `dc cvau`,
`ic ivau`, `membarrier` SYNC_CORE), unwind entry registered, then one release store of the word, then wake.
Two threads on one stub: one ask, one placement, both proceed through the same word. A woken waiter returns
from a kernel wait (a context synchronization event); a fresh caller fetches bytes never fetched since their
invalidation, because a freed range is reused only after every thread passed an epoch (blaze-run `Slots`).
**Who places** (corrected: one control thread placing every record, with a barrier across all cores at each
publish, grows with the cores): a KNOWN record is placed by the thread that needs it, through the write view;
code space by an atomic bump; one flight a slot by a compare-and-swap of its word (unbound, placing, bound);
offers and asks of the parent stay with the control thread. The barrier across cores belongs to the REUSE of
a freed range, once an epoch, not to a publish at addresses never executed: the piece's writer quotes the
architecture manual's paragraph for that in the test's doc before relying on it; until then publishes are
batched, one barrier a batch.
**Executable memory**: workspace code is copied into two views of one memory, read+execute where it runs and
read+write where the control thread writes (macOS `mach_vm_remap`, Linux a memfd mapped twice; blaze-launch-rt
sys.rs; measured there 2026-11-15: runs ad hoc signed, killed under the hardened runtime even with `allow-jit`,
runs with `allow-unsigned-executable-memory`). `MAP_JIT` with `pthread_jit_write_protect_np` is rejected: its
switch is per thread and a program's own JIT flips it. Signed segment files (macOS `F_ADDFILESIGS_RETURN`,
about 50-100 us a file) are taken for bundles only: one per function would be a file and a signature each.
**Compiled behind**: each publish queues its record's unknown Func targets at `Callee` priority, ordered by
the run record's first-call order; the parent compiles them on idle workers (never the last free one) and
offers each unasked; the child places an offer at once, so a first call usually finds its word bound.
**Start list** (stage 7; it replaces this morning's "run record"): at exit the parent writes one mutable root
a (program, request shape) and user: the certificate it ran under, then the instances first called inside the
start window (at most 8,192 rows), each as (symbol, its code record's key then), in first-call order; equal
to the last one, it is not written. **Repeat, nothing changed**: blaze's start 1.5 ms + certificate 2 ms
(target.md) + spawn of the launcher 1 ms + bundle maps and binds 0.3 ms; the list's certificate is today's,
so its rows are placed as they are, ahead of `main` while `main` runs (placing is not compiling): about 5 ms
to `main`'s first instruction, 5.2 ms for hello world to finish. Floor: a linked file started after the same
certificate, 4.5 ms: ratio 1.15. **After an edit of one body**: the forward step has dirtied the `Code` tasks
that read what moved; a row whose task is dirty is compiled at start at `History` priority on idle cores and
published in this run's slot; every other row stands unlooked-at. Nothing is patched in a file, and no other
record changes (callers reach it through the slot). **Another certificate with no forward step from the
list's** (an undo, another worktree, a pull): each row is only a symbol, found through the drive (about 1 us a
row); old code is never placed. **Records made while the program runs** are appended by the parent to one
RUN SEGMENT (a file of this run, mapped by the child as it grows; no record byte crosses the socket), and
committed to the store as one segment at exit and at most every 10 s; a crash loses only what was not yet
committed. The parent pins the program's records under a named root while it lives, so compaction keeps them;
it drops its front-end state after a second without an ask and remakes it from records (rule 70).
**Panics** unwind (strategy in the key): records carry the unwind word and landing pads; an entry is registered
before its word is stored, taken back when the body is freed (launch-rt unwind.rs); one finder for the code
area where the system has one (macOS `__unw_add_find_dynamic_unwind_sections`), else an FDE each (rule 45).
**The gate** is BEFORE the start for `run` (stage 4): the child is spawned when the reached workspace bodies'
checks are answered clean. `Gate::Beside` (`StartProgram` at once, `Stop` on the first error, a program that
ends first reported only when the check has ended: bottom.md section 11) is `test`'s alone.
**Tiers**: fast records call through slots; the hot counter and `Tier` make an optimized record, published
with one word store (`Slots::publish`); the next run's run record names it while its read list stands.
**Bundles**: dependency crates at the top tier (std, core and alloc one bundle), laid out by blaze-code
`lay_out(Kind::Bundle)`: internal calls `bl`; mapped read-only and executable in place, signed by CI on macOS.
Their import words name other bundles by (bundle key, offset), bound by one add; C names by one `dlsym` each.
A bundle holds only instances whose type arguments its own closure names; one with a workspace type is a
workspace record.
**Prior art.** dyld's lazy stubs (`__stubs`, `__la_symbol_ptr`): our shape, but its binder only looks up
(dyld 4 now binds eagerly); ours may compile. V8: lazy compile on first call; its code flushing is our
retirement. HotSpot: tiers by counters and patched call sites; blaze keeps every tier's code across runs by
key, so warm-up is paid once. Julia: a code cache and pkgimages, invalidated by back-edges; blaze's keys and
read lists need no back-edges, and first-call latency hides behind idle cores. LuaJIT: no cache across runs.
Dioxus subsecond: a jump table plus a link; blaze has no link. Zig: patches a file in place; gone here.

**`blaze export`** (later; the architect's form stands where it is not a patch): Mach-O or ELF from the same
records at the optimized tier, ASLR on, one GOT, calls in reach `bl`, code by profile, written whole; no clone
patched after an edit. **Cost to the program**: a call through a slot is a load and an indirect branch, about
1%, up to 5% in call-bound loops (to measure on reth); calls inside a bundle and in `export` pay nothing.

### Gap (read 2026-11-16, corrected end state)

| End state needs | Today (file: function) | Missing |
|---|---|---|
| 1. A child process and a parent | blaze-cli main.rs: `Effect::StartProgram` calls `program::run` in-process; ladder place.rs `Placed::call` transmutes and calls on blaze's own thread, every function compiled and bound first ("Shortcuts") | spawn the launcher with a socket pair; the parent loop carrying `Parent`'s effects as drive `Code` jobs; `Exited`, `Stop` |
| 2. Slots made as symbols appear | blaze-run parent.rs `Parent::new(functions)`, slots.rs `Slots::new(slots)`: a fixed list before the start; run.rs `Reached::slot`: slot n is the nth reached | `FirstCall` numbers slots by symbol (written); `Parent::declare` (additive) |
| 3. The launcher on the new path | blaze-launcher main.rs: a sidecar file and the daemon's socket; launch-rt control.rs asks `Need` once a slot (no simulation), records.rs `Loaded` is the daemon's record form | descriptor 3, the run record, `FirstCall` and `Slots` in control.rs, placement of blaze-code records |
| 4. Placement of a code record against the run's words | blaze-code image.rs `lay_out` completes sites against a segment's own import table; `Image` binds fixed addresses (withdrawn) | blaze-code place.rs `place` (written); `Image` and its postings deleted |
| 5. Code records in the store, keyed fully | place.rs `Body` held in memory; blaze-build instance.rs `instance_key` lacks target, tier, settings, toolchain | put and read under the drive's `Code` name; the key |
| Reusable as built | launch-rt sys.rs (two views; measured), arch.rs (stubs, head, resolver, thunks), table.rs (`resolve`, safepoints), unwind.rs, tls.rs, threads.rs, retired.rs, waits.rs; blaze-launch wire.rs; blaze-run `Parent`, `Slots`, `Heat`, `Tier`, `Registry`; drive request.rs `Gate::Beside` with `StartProgram`/`Stop`/`Exited` | |
| Emitter | blaze-emit record.rs `emit_record`: import sites and relro (piece sent); pass.rs refuses `Invoke`/`Resume` | landing pads, unwind word |
| Bundles | none mapped; `lay_out(Kind::Bundle)` lays one out | the CI writer, macOS signature, binding by (bundle, offset) |
| Run record | `Parent::warm`, `image`, `WriteImage` (a startup image) | rows in first-call order; the child places them ahead of `main` |
| Tier-up, statics, symbolize | as the architect's gap read them (tier.rs, heat.rs without an optimizer; run.rs `static_image` in memory) | as before |

### Pieces (one writer each; "||" can be written at the same time; hello world first, then the repeat)

1. || `FirstCall` (NEW blaze-run first_call.rs): WRITTEN 2026-11-16, never compiled; simulation with two
   planted bugs (`AskTwice`, `WakeBeforeStore`) in sim/first_call.rs.
2. || `place` (NEW blaze-code place.rs): a record's code completed against the run's words at its address;
   WRITTEN, never compiled; tests in place_tests.rs.
3. || `blaze run` on the new front end (blaze-cli program.rs; the architect's piece 1) and `emit_record`'s
   sites on the run path (ladder run.rs). Test: the hello-world rung's records name targets by symbol.
4. || `Parent::declare(instance) -> SlotId` and the wire's `Need` by symbol (blaze-run parent.rs, blaze-launch
   wire.rs, additive). Test: a published record's new callee gets a slot once, whichever asks first.
   WRITTEN 2026-11-16, never compiled: NEW blaze-run parent/declare.rs (`declare`, `growing`, `need_symbol`,
   `looked_named`, `compiled_named`, `Resolve`; `Effect::Declare`/`Unresolved`), NEW blaze-launch wire/run.rs
   (the run wire, its own schema: `Up::Need` by slot or symbol, `Down::Declare`/`Publish`/`Failed`);
   simulation sim/declare.rs with planted `DeclareTwice`, `PublishBeforeTell`. The child's `FirstCall` still
   numbers its own slots: piece 5 makes it take the parent's `Declare` pairs.
5. The launcher's run mode (blaze-launcher main.rs, launch-rt control.rs, after 1, 2, 4): descriptor 3, the
   constant stub segment, `FirstCall` and `Slots` driving sys.rs. Test: a fake parent answering from a
   directory of records runs a rung that prints.
6. The parent (NEW blaze-cli run_parent.rs, after 4): spawn, the socket loop, `Lookup`/`Compile` as drive
   `Code` jobs, `Stop` kills, `Exited`. Test: the hello world prints under `blaze run`, compiled on first call.
7. || Code records in the store with the full key (blaze-build instance.rs, ladder; the architect's 6, 7).
   Test: a second run compiles 0.
8. The run record and placement ahead of `main` (blaze-run, launch-rt, after 6, 7). Test: a repeat run asks
   the parent nothing; the profile's span from start to `main` is quoted.
9. || The std bundle (lay_out `Bundle`, a CI writer, the macOS signature, binding by (bundle, offset)).
   Test: hello world's std calls never reach a stub.
10. Compiled behind (the parent's `Callee` queue fed to idle workers, offers placed). Test: tempo's start
    waits on fewer than 5% of its first calls.
11. || Unwinding (emitter pads, unwind word, registration before the store). Test: `catch_unwind` catches.
12. The gate before the start on the real path. Tests: a type error in a reached workspace body stops
    `blaze run` before the child is spawned; one in an unreached body does not (`blaze check` reports it);
    `blaze test` starts beside and holds verdicts until the check ends.
13. Delete: ladder place.rs's in-process run, blaze-code `Image` and its postings, the launcher's sidecar.
14. Tier-up, then `blaze export`, then symbolize and the debug object.

### The cut to the lazy parent (D124, 2026-11-17 02:20; the coordinator's spec)

Measured 02:08 on tree 5ab37c9156 (release r17): hello world runs in 16.6 s, then 12.3 s on a repeat. Read in
the code: a Rust program with std is built WHOLE in blaze's process (`run_program_given`), refused there
(`RunError::OwnProcess`), then built whole AGAIN before the child starts (run_parent.rs `lay_out`, the eager
walk `build_records_new` into a `MemoryStore`); every name of 79 crates is solved from source each time. None
of that is improved: it is deleted. The parent makes nothing before the start.

**The parent's loop** (run_parent.rs): the child is spawned at once; each `Need` is answered by ONE step of
the D106 record walk (body_ids.rs `Walk::one`): the instance's key from text and today's facts, found in the
home's one store (code_store.rs `HomeCodeStore`) or made by the maker and put, and what its entry declares
reached as rows with nothing done for them. A symbol the table does not hold is one of the run's special
records (the C entry shim, the thread-local getter and ender) or is refused by name. Nothing is walked ahead
of the start; compiled-behind (piece 10) is the same step on idle workers, later.

**Keys before any check** (the seam `BodyIds`, a NEW blaze-cli run_ids.rs): a workspace body by the drive's
`BodyCheck` key and read list; a toolchain crate's body (core, alloc, std) by the toolchain's content id and
its item, with the toolchain as its one fact (a coarse proof: the cut covers every input of that body's
check); a locked dependency's body by its unit's key the same way. So std's instances are found by key on
every run after the first, with no `BodyCheck` record needed for them.

**The maker** (make_one.rs): one owned session, made on the first miss and not before, over names READ FROM
RECORDS (front2 source.rs `Records`), never `resolve_crates` over source; `member_owner` by an index.

**The entry** (ladder): which process, the roots of a Rust program with std (the shim, `lang_start::<T>`,
`main`) and the special records, from names records, generic over `NamesSource`; no body read.

**The start list** (stage 7; NEW blaze-cli start_list.rs): written at exit under the certificate; when the
certificate is today's its rows are published as they are before `Begin`, nothing keyed or looked up.

**Every program runs in the child** (corrected 02:25, after r18 measured no change: 15.3 s, then 12.9 s). The
owner's hello world is a blaze-edition program (`fn main(cx: &Cx, out: Out)`), whose start is bstd's hosted
entry, and that one runs IN blaze's process today (`run_program_given`, the whole program built first): the
12 s. The launcher refuses a hosted entry by name. So: the launcher's run mode takes `hosted:<symbol>`, makes
the host's world in the child (bstd `Hosting` over the child's own `Io`, its arguments, and the grants the
command line gave, handed by the parent) and calls `bstd_hosted_start(table)`; `blaze run` routes every
program to the child, and the in-process run (ladder place.rs, `run_program_given`) is deleted (piece 13).
A program's crash, exit or stray write then cannot reach blaze's store, and there is one run path.

**The gate is before the start** (stage 4; the owner, 02:24: "we need both check AND run to be fast and
ideally run should require check happened first"). `run` asks with `Gate::Before` (the drive has it,
simulated: sim.rs "`Before` started a program over errors"); today main.rs asks `Gate::Beside` for every
command, and hello world runs while `check` says `main` was not checked. An error, or an item the check
could not check, stops the run before the child is spawned, with the check's own findings. Behind the gate
the parent takes the check as its proof: a WORKSPACE body with no clean `BodyCheck` record is refused by
name at its first ask (never checked a second way, never built unchecked); a toolchain or dependency body
is checked once, where its code is first made, and found by key after.

Pieces, one writer each, all at once: G the gate (drive request.rs, cli main.rs's ask); H the hosted start in
the child (launch-rt run_mode.rs, blaze-launcher); W `Walk::one` (ladder body_ids.rs); I `BodyIds` over the command's
records (cli run_ids.rs); M the maker session over records (ladder make_one.rs, front2 source.rs, lazy/);
E the entry and the special records (ladder run.rs, front2 start.rs, run_entry.rs, NEW run/special.rs);
S the start list and the parent's `Resolve` seam (cli start_list.rs, run_parent.rs); then P, the parent's
loop over W, I, M, E, S, and the deletions: `build_records_new`, `run_records`'s from-source front,
`run_program_given` for a Rust program, `Needs`' prebuilt index. Not in this cut, each its own piece after:
records read in place from store segments and the run segment (today a file a record in the records
folder), compiled-behind, the std bundle. Checks: hello world twice in one scratch home; the second run's
line reads `code records: reached R, found R, made 0, unkeyed 0` or names each unkeyed instance, its
profile holds no check or resolve span, and it ends inside 100 ms (floor 5 ms: ratio to be quoted).

### Statics in the run (lane HR's spec, 2026-11-17; rule 29: the spec, then the same lane builds it)

Nothing above placed a static: the launcher refused a record's `Static` target with "statics come with
piece 8", but piece 8 is the run record. Hello world's records name statics (green 515e6f7689: "record
8959dccaa982 names Static ee9c4409b9c9"). The Reference, "Static items": one allocation for the whole
program, its initializer evaluated at compile time, never dropped; `static mut` and a type with
`UnsafeCell` are written in place; every reference to a static is to that one allocation.

- **A static is a record of its own, a DATUM** (NEW blaze-code datum.rs, `bzdata01`, the form blaze-export's
  `Data` takes): its place (`ReadOnly`: immutable with no interior mutability; `Writable`; `Zeroed { len }`:
  writable and all zero, no bytes), its alignment (a power of two, at most 4 KiB), its bytes, its targets by
  (space, symbol) like a code record's (a fn's address is its entry thunk, another static's is its first
  byte, a vtable's its first word; a thread-local or a C name in a static's value is refused by name until
  each is built), and its words (`at`, target or the datum's own byte, addend). One datum a static: the
  value's other allocations (`&[1, 2]`, a string literal) are laid out after its first bytes, each at its
  own alignment, and a pointer to one is a word naming the datum's own byte. Sealed once, read in place.
- **Keyed like an instance, a row of the same walk** (body_ids.rs `Walk`): a code record's entry declares
  the statics its code names, by item key, beside the instances it calls; a datum's entry declares the
  statics and instances its words name. A static's row is keyed from text before the work: the static's
  symbol (symbols.rs `static_symbol`), its initializer's `BodyCheck` key and read list, and its crate's
  fact (rule 65), with the target, toolchain and forms as `record_key` writes them. Found, nothing is
  evaluated; missed, the maker evaluates that ONE static (front2 statics.rs, the constant evaluator) and
  puts the datum. An edit of an initializer moves that static's key alone; the code that names it keeps
  its key (code names a static's word, never its value), so an edit's work is that one datum.
- **Sent WITH the record that names it, never on demand.** A static has no stub: a load through its word
  must find its bytes. So when the parent answers a record, each static it declares that the child does
  not hold is stepped (found or made) and told first, in one `Down::Data { data: [(symbol, record)] }`
  after the `Declare` of every slot the record and its data name (a fn's address in a datum is its slot's
  thunk) and before the record's `Publish`; a datum's own statics come in the same message, so a cycle
  of statics (`static A: N = N(&B); static B: N = N(&A);`) is one batch. Each static is told once a run.
  The start list carries them (start_list.rs version 4, 2026-11-17): each first answer is a group, the
  data its `Publish` stood on as sent then (each datum once, in its first namer's group), the statics its
  record and data name, then its code row; a replay sends each group as the run did, through the one
  `Publish` writer. A group whose record or datum file is gone, or that names a static no group kept
  before it sent, is left out whole and resolved at its first call.
- **Placed by the child before anything names it** (launch-rt run_mode.rs): for one `Data` message, every
  datum is read and given its address first (so words inside the batch resolve, cycles included), then
  each is filled (blaze-code `datum::fill`: the bytes, then each word through the run's `Words`) and
  copied, and only then is its static word stored: one 8-byte word a static in the reservation, as the
  slot table says. A `ReadOnly` datum goes in the code area (read-only in the run view, written through
  the write view, as a relro part is); a `Writable` or `Zeroed` one in the run's writable data area. A
  code record whose `Static` target the child does not hold is refused by name (the parent's order was
  broken), never stubbed.
- **Thread-locals** stay refused by name until a program needs them: their template is a datum too, and
  the getter and descriptor are run/special.rs records already.

Steps, each admitted alone, each ending with hello world's next line: (1) a `ReadOnly` datum of plain
bytes: declared, keyed, made, sent, placed, read; (2) words: a static holding a fn's or another static's
address, cycles; (3) `Writable` and `Zeroed`; (4) thread-locals if hello world needs them.

## The live picture (the run path's wire; the parent is `blaze run`, the socket a pair on descriptor 3)

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

## The machines (crates/blaze-run; the run path's, with `first_call::FirstCall` in the control thread)

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

**Export** is `blaze export` (End state, above). What follows applies to it.

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

## Export (writer, 2026-11-16)

crates/blaze-export (`export::export`, sans-IO, bytes into a vector the command passes): green
(integrator, tree 4213e95e5d: 9 tests pass; `export_smoke` on the Mac printed "exported file
exited with status 42"). The AArch64 site encoders moved to one home, blaze-code `a64`.

- **Layout** (layout.rs): every reached record placed once in one code region; equal records
  (equal bytes) folded; order: the profile's hot functions, then a depth-first walk from the entry
  over sites in code order, then the rest by symbol. Every call in reach is a `bl`, every address
  of a function, static or vtable an `adrp; add`: no slot, stub or import word. A C name is one GOT
  word the platform binds. Relro parts and read-only data after the GOT (read-only after fixups),
  writable data, then zeroed data with no file bytes. A target nothing given places is an error
  (`OutOfReach`): an export never leaves a hole.
- **Entry** (start.rs): macOS `LC_MAIN` is `main` itself for `extern "C" fn main() -> i32`, else a
  made wrapper (`fn main()`: call, return 0; `lang_start`: `(main, argc, argv, 0)`). Linux
  `_start` calls glibc's `__libc_start_main` with that main. bstd's hosted entry is refused.
- **Mach-O** (macho.rs, sign.rs): `__PAGEZERO`, `__TEXT`, `__DATA_CONST` (`SG_READ_ONLY`),
  `__DATA`, `__LINKEDIT`; chained fixups (`DYLD_CHAINED_PTR_64_OFFSET`), libSystem as ordinal 1,
  `LC_MAIN`, `LC_BUILD_VERSION` 12.0, `LC_FUNCTION_STARTS`, undefined symbols with an indirect
  table for `__got`, an ad-hoc code directory of SHA-256 page hashes, last.
- **ELF** (elf.rs): `ET_DYN` with `DF_1_PIE`, `PT_INTERP` (`ld-linux-aarch64.so.1`), a build-id
  note, the dynamic section at the head of the RELRO load, `R_AARCH64_RELATIVE` then `GLOB_DAT`
  and `ABS64` in RELA, `BIND_NOW`, `PT_GNU_STACK`, the symbol tables in a read-only last load.
- **Deterministic**: the same records, data, entry and profile give the same bytes in any input
  order on any machine; `LC_UUID` and the build id are a digest of the content and the profile's.
- **Refused or said plainly**: landing pads and any call of `_Unwind_RaiseException`,
  `_Unwind_Resume` or `_Unwind_ForcedUnwind` are refused (no unwind tables are written, so a panic
  would abort where Rust's rule says it unwinds); a program that never calls the unwinder exports,
  and the report says it needs no tables. Thread-locals are refused. The report's first line says
  the tier: "exported at the fast tier: N functions; the optimized tier is not built yet".

Pieces left, one writer each:
1. The optimized tier's records (export takes them as given; the report already counts tiers).
2. Unwind rows: records carry compact unwind and LSDA rows; Mach-O `__unwind_info` and
   `__eh_frame`, ELF `.eh_frame` and `.eh_frame_hdr`; then lift the unwinder refusal.
3. Debug information beside the file (debug-side-file.md), and a symbol table for profilers.
4. The profile's use beyond order: hot/cold splitting, inlining by count, guarded direct calls.
5. Thread-locals (`__thread_vars` and `tlv_get_addr`; `PT_TLS`), and C names from libraries
   other than libSystem and libc (`#[link]`), and RELR for ELF's relative words.
6. Range-extension thunks when the code passes `bl`'s 128 MiB (refused today by name).
7. The Linux run test on blaze-vm (the facts elf.rs marks "to verify").

### The export command on the rebuilt path (the coordinator's design, 2026-11-16 20:55; not built)

Today `blaze export` answers `NotBuilt`: the exporter takes a `ProgramIn` and only its own tests and example
make one. The command is the same request machine as `check` and `run`, with one difference: an export
leaves no hole, so it CLOSES the program before it writes.

1. Stages 1 to 4 as `run` (sweep, certificate, forward, the gate before: the workspace bodies the program
   reaches are checked, and an error stops the export).
2. THE CLOSURE: the root task needs the entry's code; each code record found or built is followed to
   everything its declared section names (functions, statics, vtables, C names), on the pool, until no new
   name appears. Found by key, a record costs a lookup; only what is missing is lowered and built. The
   closed set is the request's task set, kept with its answer: after an edit, only dirty code is rebuilt and
   only new names extend it. ONE seam gives the closed set: until `Code` is a drive task (stage 5), the run
   path's own reach (the set it builds and binds today) stands behind that seam; nothing else in the command
   knows which.
3. The tier: the optimized record where one exists, else the fast one; the report's first line says which.
4. `ProgramIn` is assembled from the records IN PLACE, sorted by symbol: the functions and data of the
   closure, the entry, the panic strategy from the program's inputs, the personality, the exports list
   (name, symbol) from the items that export a C name, the native image record when the program has
   natives, the name from the manifest. A blaze-edition program is exported with bstd's ORDINARY start (its
   grants made from the process, their manifest baked in as data); the hosted entry stays refused.
5. The bytes are written whole to a temporary name beside the destination and renamed. An `Export` record
   under (certificate, target, tier, profile) holds the file's content id and the report: a repeat with
   nothing changed compares the destination's id and writes nothing.

Floors: nothing changed, 3 ms (certificate, one compare); one body edited: its code, then the layout and the
write of the whole file (about 25 ms a 50 MB file each for the write and the page hashes): an export is
written whole by design, and its floor is the file's size, not the program's source.

### What a run takes from the request, and the native image as a task (the coordinator's design, 21:20; not built)

Found by the NATIVE plan's last piece: `blaze run` reads only the source files again (blaze-cli program.rs
`run`); the build scripts' outputs stay inside the drive, each crate's `CrateLinks` (front2 links.rs) has
no caller, and nothing gives the parent a native image to send. RULE: a run takes its inputs from the
request's RECORDS, never from a second reading of the tree. Two kinds, in the same machine as the checks:

- `Links`, one a crate: what the crate says about C names: its `#[link]` lines, its `extern` blocks' names,
  the items that export a C name. Keyed from the crate's outlines; its out is that table.
- `Native`, one a program (root, target, profile): the native image (the BNL2 record). It Needs, for each
  crate of the program's closure (the crate list record's rows), that crate's `Links` and its `Script`
  record (the link lines and the archives a build script made). Found by key on a repeat: a hit links
  nothing. A program with no C input has a real record that links only the system library: there is no
  empty case.

`StartProgram` hands the run the names of those records; the parent sends `Down::Native` first, with the
image's place, then answers first calls. ORDER OF BUILDING: (1) `blaze run` through the parent (pieces 5
and 6 above; it waits on the ladder's L1b); (2) `Links` and `Native` as drive kinds; (3) the parent's send
with the launcher's half of it, one batch. Until (1) is in, the in-process run stays the path a hello world
is debugged on, and it links nothing.

### Run and test read the engine's answer (D147, the coordinator's design, 2026-11-19; not built)

MEASURED, 2026-11-19, the real project tempo, `blaze test -p tempo-dkg-onchain-artifacts` (a 171-line
crate with one test): 193 s, then 62 s warm, **no test started**. `check` is the engine
(main.rs: `"check" | "engine-check"` go to engine_check.rs); `run` and `test` still build the OLD
request, whose own check (per-FILE body jobs in a 64 MB frame) runs before any start. On tempo that
check runs out of its frames in dozens of files ("frames: its region of 67108864 reserved bytes is used
up"), checks the whole workspace to test one crate, and holds every test behind any failure anywhere.
The engine checks the same tree in 0.2 s when nothing moved and 2.6 s after a one-body edit.

RULE: `run` and `test` take every fact they need from the ENGINE's stored answer. The old request is
not built for them; when both commands pass their tests on the engine it is deleted with `check-old`.

The seams exist. run_ids.rs asks two questions of "the command's standing answer" and nothing else:
`trait Today` (a name's out, a cell's digest) and `trait Answer` (a named record's parts, then its
payload). The old `Request` answers both; so do the tests' tables. The engine gets its own answers
(rule 78: one seam, each instantiation tested equal while two exist). What each consumer needs, and
where the engine holds it (the facts: /Volumes/Projects/blaze-lanes-1117/_evidence/run-test-facts.txt):

| The run path needs | Old request | Engine |
|---|---|---|
| a name's out and cells today | `settled_input`, `cell_now` | the answer's row: `out`, `defines` |
| the walk Units -> FileItems -> BodyCheck parts | each record's parts | the Units and Outlines records' parts (the Bodies plan reads the same parts) |
| per body: checked or not, the items it covers, its key, its reads | the `BodyCheck` record (findings, `body_payload`), found by `body_check_key` | the row's key and reads; **nothing else is kept** (engine_check.rs: `Stage::Bodies => None`) |
| each crate's names and items | the `CrateScope` record at its `Input` | the Names stage's record at `NamesStage::input_of`: same reader, with the request's snapshot for expansion texts |
| the certificate that keys the start list and the tables | `req.certificate()` | the answer's root |
| `Owners` outs | part of `body_check_key` | not needed: the body's key is the row's |

So ONE thing is missing, and it is a record (rule 51): **a body's run facts**. The Bodies stage puts,
for each BodyCheck task, a small record: its class (clean; errors with the count and the first rule;
not finished with the rule) and the sorted item keys of the bodies it covers (what `work::body_payload`
holds today). Keyed as the other stages key theirs (`engine_store::record_key`); its out is the row's
verdict digest, so nothing new invalidates. Cost: one small put a body task on a first check (tempo:
about 16,600 beside 17,900 puts today), none on a no-op, one on a one-body edit. Findings stay in the
lines record alone (rule 30): the facts record holds their CLASS, never their text.

`Answer` gains the one question that hid the old key: `body(check: Name)` gives a body's key, class,
reads and covered items. The old answer computes it as today (`record_reads`, `verdict`); the engine's
reads its row and its facts record. `RunIds`, `Checked` and the parent's `step` do not change.

THE GATE IS THE CALL. There is no whole-request gate. A program or a test starts when the check has
answered, whatever it found; each workspace body is admitted at its first call by its own facts
(`Checked::runs`, which run_parent.rs `step` asks). THE GATE'S RULE (D151): a workspace body is run
when its types were checked whole and it has no error: `Clean`, or `Typed` (its borrow check refused
or not run: the borrow check never changes what a body does, its findings are notes until
`BORROWS_ARE_ERRORS`, and `check` answers "0 errors, incomplete: NOT checked: borrows" for it, the
line `run` prints before it starts). A body that has an error (`Errors`) or whose types were not
checked whole (`Incomplete`: no typed body to lower) stops the program, or fails the one test that
called it (`NOT RUN`, T1), naming the body and its class. A called fn the table has no row for is
refused with what the table holds beside it (run_ids.rs `NoRow`, rule 19): its workspace crate
rows by prefix, and whether the called prefix is one and how many rows it has. The two gaps it
tells apart: a body no check's covers list (the called crate has rows, none this fn's), and a call
under a unit no check covered (the prefix is none of the rows). Which unit the fn was checked
under is not said: an item's key hashes its parent's whole key, so it differs in every byte
between units, and naming it needs the item's place, which the table does not hold; D152 removes
that gap. The engine's facts record lists in each entry's covers every fn declared in a body its
check reached (front2 tables/task.rs `BodyReads::locals`), as the old path's `covered` did. Why: "a run compiles what is called" (AGENTS.md, scope); an error in a function no test
calls must not hide every test (tempo has 11 open false errors, and any one held all tests). The
check's summary line is printed before the start, so nothing is hidden. Owner's call if `run` should
also refuse to start over an error in the program's own crates: the table knows them (the walk lists
the closure's bodies), so it is one rule to add.

Selection follows the check: the engine checks every member; `run` picks its program and `test` its
units from the engine's unit graph (made with tests, so it holds the library, binary and test units).
The old `run` graph was made without tests: a library unit's crate key must not depend on that flag
(to verify before piece E3; if it does, that is a key that holds too much, rule 21).

PIECES, one writer each; E1 || E2, then E3; the real runs are the coordinator's:
- **E1** the facts record: engine_bodies.rs (where the row is built), engine_check.rs (the put), a new
  engine_body_facts.rs (the bytes, one reader). First tests: restore equals fresh in bytes (rule 26);
  a one-body edit puts one facts record; planted: a class of `clean` for a body with an error is
  caught by the reader's oracle test.
- **E2** the engine's `Today` and `Answer` (a new engine_run_answer.rs) and `Answer::body` in
  run_ids.rs. First tests: over a two-crate fixture the table built from the engine's answer holds
  the same bodies, classes and crate rows as the one built from the old request; a body the engine
  refused is `Incomplete`, never `Clean`.
- **E3** main.rs's `run` and `test` arms and run_names.rs's inputs: the engine check, then the start
  from E2's answer; the certificate is the root; the old request is no longer built for them.
- **The judge**: hello world prints through the new path in a fresh home and replays in 10 ms the
  second time; then `blaze test -p tempo-dkg-onchain-artifacts` runs its one test; then
  `tempo-hardfork` (17 tests); the first failure of each is the next piece.
- NOT in this cut: code as engine tasks and a typed body kept for codegen (D144; the parent still
  lowers a called body itself), `run` on a pool. They make the first run fast; this makes it happen.

THE CONTRACT OF E3 (2026-11-19 17:40; the pieces below are written at once, against these names):

```rust
// crates/blaze-cli/src/engine_check.rs (piece R1 owns the file): the check, kept open for a start.
/// The engine's check of the checkout, its answer printed, and what a start reads afterwards.
pub(crate) fn check_for_start<'r>(cx: &Cx, io: &Io, out: &Out, phases: &mut Phases, heap: &'static Heap, frame: &'r Frame)
    -> Result<Standing<'r>, CliError>;
pub(crate) struct Standing<'r> {
    /// The answer's root: the certificate of the start list and of the run tables.
    pub(crate) root: ContentId,
    /// The command's store, its puts committed.
    pub(crate) store: /* the command's store */,
    /// E2's answer over the stored rows and records (engine_run_answer.rs).
    pub(crate) answer: EngineAnswer<'r, ..>,
    /// The unit graph's row and each crate's names row, as the run's names reader takes them.
    pub(crate) graph: Input,
    pub(crate) names: &'r dyn Fn(Name) -> Option<Input>,   // `NamesStage::input_of`
    /// How many errors the check printed (the summary line is already out).
    pub(crate) errors: u32,
}

// crates/blaze-cli/src/engine_start.rs (new; piece R1): what main.rs's `run` and `test` arms call.
pub(crate) fn run_program(cx: &Cx, standing: Standing<'_>, picked: &run_package::Program, args: ..) -> Result<Status, CliError>;
pub(crate) fn run_tests(cx: &Cx, standing: Standing<'_>, packages: &[..], args: &test_args::Args) -> Result<Status, CliError>;

// crates/blaze-cli/src/engine_native.rs (new; piece R2): the program's native image, by key.
/// The native image (the BNL2 record native_image.rs writes) of the program whose crates are
/// `closure`: found by its key, else made now from each crate's link facts and its build script's
/// record and put. A program with no C input has a real image that links the system library.
pub(crate) fn image_for(cx: &Cx, standing: &mut Standing<'_>, closure: &[CrateAt]) -> Result<Key, NativeError>;
```

- **R1** engine_check.rs (`check_for_start`), engine_start.rs, main.rs's `run`/`test` arms,
  run_names.rs (its inputs from `Standing`, opened with the snapshot as the Bodies stage opens them).
  The start does what `Effect::StartProgram` / `StartTests` do today with the request's parts replaced
  by `Standing`'s; the old request is not built for these commands.
- **R2** engine_native.rs, native_image.rs: no new stage; the image is a record keyed by its inputs
  (the closure's crates' link facts, read from their names and outline records, and their Script
  records' outs), made at the first start that lacks it.
- **R3** selection: run_package.rs and test_args.rs. A command given a member's directory walks up
  to the workspace root (today: `error[manifest-not-read] ... the root's [workspace] does not offer
  it`); `-p` and `--bin` pick from the engine's unit graph for `run` and `test` alike.
- **R4** the typed body is kept (D144's first half, RV3's job 4): the Bodies stage leaves, beside
  the facts record, the checked generic body in the flat `blaze-body` form with the portable type
  table it indexes; the parent builds a called instance from it and lowers nothing again. Files:
  front2 tables/task.rs, blaze-body, blaze-body-lower, and a new blaze-cli engine_body_record.rs.
  Test first: a restored body builds the same instance as the fresh one (bytes of the code record).

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
records the last run of this manifest placed before `main`, in order, under `ReservedTag::Image` keyed by
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
field is an error); a bstd program reads its own through the grant `spec: Spec`. Its digest joins the program's
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
(`fn reads(cx: &Cx, tmp: Dir<ReadWrite>)`), and the runner fills it. D104: a whole `Io` cannot
be asked for; `fn main(cx: &Cx, io: Io)` is refused at the parameter with the kinds to name
instead. `Io` stays bstd's own world handle (the launcher makes grants from it) and a test's
simulated world.

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
is an ordinary start (only what fills itself is filled); a marked descriptor that does
not close within 5 seconds, or holds anything but the header and a spec, is a refusal.
`bstd manifest\n` instead prints the program's manifest as JSON and runs nothing.

Departures, as built: standard output and error are both `Out`, told apart by the parameter's
name (`out`, `err`); no `Io` parameter and no `everything` scope (D104: `main` names each grant);
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
