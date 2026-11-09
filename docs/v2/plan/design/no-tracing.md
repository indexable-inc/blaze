---
status: plan
summary: blaze drops the `tracing` crate. Every span and event goes through blaze's own recorder in a new leaf crate `blaze-trace`: one registry for span and event kinds, `span!` and `event!` macros with static call sites, words written straight into the flight ring (no dispatcher, no visitor). The doc gives the grammar, the slot-reuse ordering, the cost budget with its instruction-count bench on the Linux bench runners, the mapping from each tracing form, vendor patch series for third-party crates, a lockfile ban in `blaze check`, and the lanes parallel agents take.
date: 2026-11-04
---
# No tracing: blaze's own spans and events

The owner has decided: blaze removes `tracing` entirely. Every span and event in blaze's code
goes to blaze's own recorder, with no `tracing` adapter. `blaze check` denies `tracing`, and
recording is as cheap as the hardware allows. This doc is the spec implementers follow
(`spec-then-build`). Each question gets one design, with the rejected alternative in one line.
An independent review (2026-11-04) is applied; where this doc disagrees with it, a line starting
"Reviewer:" says why.

It is grounded in the tree at `cf263f9545`. That tree has 319 `info_span!`, 198
`#[instrument]` (172 of them `level = "debug"` or `"trace"` in rustc- and rust-analyzer-derived
code), 127 `tracing::{info,debug,warn,error,trace}!`, 69 `.record("..")` and 101
`tracing::field` uses, over 35 crates counting vendor/. A `blaze_profile::Recorder` implements
`tracing::Subscriber`. Its id is already the flight recorder's (ring, slot, seq), and typed kinds
(`span_kinds!`, `Kind{..}.open()`) already skip `tracing` by calling `flight::open_said`. So this
is mostly deletion: the kinds path becomes the only path.

**Depends on**: pd-cost (self CPU from fiber switches) landing first, because no span here reads
a CPU clock; also pd-logs, pd-postmortem and pd-crashes, which edit `flight.rs` (section 9).

## 1. The crate: `blaze-trace`, a leaf

The recorder's write side cannot stay in `blaze-profile`. `blaze-profile` depends on
`blaze-graph`, `stdx`, arrow and parquet, while the crates that must record (`stdx`, `syntax`,
`parser`, `base-db`, `rustc_type_ir`, `blaze-graph`, `blaze-query`, `blaze-mem`) sit below it,
and `stdx -> blaze-profile` would be a cycle. So there is a new crate `crates/blaze-trace`
(`blaze_trace`). Its only dependencies are `parking_lot`, `blaze-hash`, `blaze-spin`, `libc` and
`windows-sys`.

| moves to `blaze-trace` | stays in `blaze-profile` |
|---|---|
| `flight.rs`'s write side: rings, slots, sites, names (`intern`), folding, the row codec (encode and decode one row's words), a raw snapshot (`snapshot(ring) -> RawRows`, the ring's words copied whole, undecoded), `threads_now`, `progress` | the ring readers `Read`, `Cursor`, `read`, `window`, `take`, decoding a raw snapshot into `SpanRow`/`EventRow`/... with the leaf's codec |
| `clock.rs` (`ticks`, `Scale`, `mono_ns`) | `batches.rs`, `arrow.rs`, `history.rs`, `sql.rs`, `views.rs`, `time*`, `slow*`, `gate.rs`, `live.rs`, `watchdog.rs`, `sampler.rs` (its per-thread state type moves beside `Ring`), `part`/`dump` of `BLAZE_PROFILE` |
| `span.rs`'s write half: `Name`, `Value`, `FieldOf`, `Said`, `Column`, `Fields`, the registry macro, guards | `span.rs`'s arrow half (`read_batch`, `text_at`, `views`), renamed `kinds_read.rs` |
| the value types of typed columns: `Outcome`, `ModuleFrom` (from `span.rs`), `blaze_graph::wait::WaitKind` | `schema.rs` tables, built from `blaze_trace::schema::{Type, Column}` (which move) |
| `hot/record.rs` (the per-ring sketches the closer writes) | `hot/read.rs` |
| the postmortem writer (`Store`, `Mapping`, `persist`) of pd-postmortem | the postmortem reader (`dead`, `Found::read`, `recover_into`) |
| `testing::{capture, print_tree}` (feature `testing`) | |

**wasm32-wasip1.** `blaze-trace` builds for `wasm32-wasip1`, because crates below it are built
for proc macros on wasm. There `ticks()` reads `std::time::Instant`, the barrier is a compiler
fence (one thread), the owner check is always true, and there is no inline asm, no mmap and no
postmortem store. A CI check builds it for that target (lane 0.1).

`blaze-graph` depends on `blaze-trace` and calls the `Wait`, `Hold`, `Job` and `Worker` kinds and
the fiber enter-stack carry directly. That deletes the function-pointer hooks
`pump::span_waits`/`WaitSpans`, `pool::span_jobs`/`JobSpans` and
`fiber::carry(Carried{take,put,end})`, which saves an indirect call on every wait, job and fiber
switch (`one-concept-one-home`). `blaze_profile::install` keeps only `from_env()`, because rings
are claimed lazily and nothing needs installing.

Rejected: `blaze_profile::span!`. Only a leaf crate can be reached by `stdx` and the front end
without a cycle.

## 2. API

### 2.1 One registry: `kinds!`

`span_kinds!` becomes `kinds!` in `blaze_trace::kinds`, with a fourth section, `events`. Kinds
are the spans and events whose fields a reader reads by name: a SQL view, `blaze slow`, the
critical path, a test, the watchdog. Everything else is ad hoc (2.2). Both share one `Site`
table (2.5). The registry is one invocation in `blaze-trace`, because its columns are global.

```rust
kinds! {
    generic {   // one field name per column: the aliases `crate`, `subject`, `cgu` are gone
        Crate = "crate" (Dict) field krate, "...";
        Function = "function" (Utf8) field function, "...";
        Unit = "unit" (Utf8) field unit, "...";
    }
    typed { Awaits = "awaits" (Utf8): Name, "..."; /* ...as today... */ }
    spans {
        UnitNames = "unit names" { krate: Name => Crate, unit: Name => Unit }
        MacroExpand = "macro expand" { path: Name => Function, krate: Name => Crate }
            => MacroExpanded { steps: Option<u64> => Steps, peak_bytes: Option<u64> => PeakBytes, outcome: Outcome => Outcome }
        Wait = "wait" { awaits: Name => Awaits, function: Name => Function, owner: Option<u64> => Owner, on: WaitKind => WaitsOn }
        // ...every kind of today's registry, unchanged
    }
    events {
        /// A unit's record was not kept, and why.
        RecordNotKept = "unit record not kept" (warn) { unit: Name, why: Name }
        /// What devirtualization did at one call (hir-ty-mir devirt): no message, the kind is the identity.
        Devirt = "devirt" (info) { block: u64, outcome: Name, object: Option<Name>, reason: Option<Name>, targets: Option<u64> }
    }
}
```

An event kind is a struct literal with `.emit()` and has no message: its kind name is what it
is. Its fields are written as pd-logs' typed field rows, keyed by the field's static name. Each
event kind gets a SQL view `event_<name>` with its fields as columns, made like `span_<name>`.
Those views replace pd-logs' fixed `events_wide` pivot list (its risk 7). An event kind's fields
fill no `spans` column, so they add none to `WRITTEN`.

### 2.2 Ad hoc spans and events: the grammar

```
span!(  [trace,] [held,] [root, | parent: <SpanId>,] "<name>" [; field [, field]*] )
event!( <level>, [parent: <SpanId>,] ["<fmt>" [, <arg>]*] [; field [, field]*] )
level := error | warn | info | debug | trace
field := ident | ident = <expr> | ident = %<expr> | ident = ?<expr> | %ident | ?ident
```

- `;` separates the message's `format_args!` arguments from the fields, in both macros, so a
  positional argument (`"{}", path.display()`) is never mistaken for a field. A span's name is a
  plain literal and takes no arguments.
- The message is optional when there is at least one field (`event!(debug; ?expected_ty)`). A
  message-less event's row has message word 0 and an empty site literal; readers show its fields
  and `file:line`.
- Shorthands, as tracing's: `x` is `x = x`, `%x` is `x = %x`, `?x` is `x = ?x`.
- `span!` returns a `Guard`, already entered, closing on drop. With `held` it returns a `Held`
  (2.4). With `trace` it compiles to nothing unless the feature is on (2.6).
- The name and the message literal live in the static site. The macro passes the message and its
  arguments to `format_args!`, and `Arguments::as_str()` decides the rest. `Some` (no captures,
  no arguments) means the row's message word is 0, "the site's literal", and nothing is
  formatted. `None` means the message is formatted through the text path (2.7) and interned.
  Events are rare (hundreds per build), so formatting on capture is fine.
- A field called `krate`, `function` or `unit` fills that generic column, which today's readers
  and the hot sketch key read. The match against the site's static field list happens once, when
  the site registers. An ad hoc span may have at most 6 other fields and an event at most 15;
  beyond that a `const` assert names the site.

How each existing form converts:

```rust
// blaze-cas:2004, `blaze` target with shorthands
tracing::info!(target: "blaze", %dir, kept, kept_bytes, swept, held, "placed files found at open");
blaze_trace::event!(info, "placed files found at open"; %dir, kept, kept_bytes, swept, held);

// stdx tempfile.rs:43, positional arguments
tracing::info!(target: "blaze", "cannot remove temporary file {}", self.path.display());
blaze_trace::event!(info, "cannot remove temporary file {}", self.path.display());

// devirt.rs:114 etc., message-less events with fields: an event kind (2.1)
tracing::info!(target: "blaze", block = block_index(block), outcome = "kept", reason = "...");
blaze_trace::kinds::Devirt { block: block_index(block) as u64, outcome: n!("kept"), object: None, reason: Some(n!("...")), targets: None }.emit();

// hir-ty infer/expr.rs:888, solver debug (section 5.1: a trace site)
tracing::debug!(?expected_ty);
blaze_trace::event!(trace; ?expected_ty);

// span with fields
let _span = blaze_trace::span!("lease of a build dir"; dir = %dir.display());
let _p    = blaze_trace::span!("crate_def_map_query"; krate = %crate_display_name(db, id));
```

`never!` and `always!` (`crates/stdx/src/assert.rs`, 76 uses counting the file's own) keep their
grammar, `$fmt:literal $($arg)*`, which is a `format_args!` tail. Only the re-export changes:
`pub use tracing::error as __tracing_error;` becomes `pub use blaze_trace::event as __event;` and
the two call lines become `$crate::assert::__event!(error, $fmt $($arg)*)`. No use site changes.

The macro expands to this (a sketch; the real macro is `macro_rules!` and the names are the
implementer's):

```rust
// span!("lease of a build dir"; dir = %dir.display())
{
    static SITE: $crate::Site = $crate::Site::span("lease of a build dir", module_path!(), file!(), line!(), &["dir"]);
    let words = [$crate::Field::word(&$crate::Shown(&dir.display()))];
    let kinds = $crate::kinds_of(&[$crate::kind_of(&$crate::Shown(&dir.display()))]); // folds to an immediate
    $crate::Guard::open(&SITE, &words, kinds)
}
```

`kind_of` reads `<T as Field>::KIND` through a generic fn, so the optimizer folds `kinds` to a
constant store. Nothing is decided by type at run time.

Rejected: tracing's grammar with `target:` and fields before the message. Every site is
blaze's, so a target filters nothing (`module_path!()` fills the `target` column), and without a
separator a positional argument and a shorthand field are ambiguous.

### 2.3 What replaces `#[instrument]`: an explicit guard

```rust
fn probe_op(&mut self, ...) -> ... {
    let _span = blaze_trace::span!(trace, "probe_op"; ?goal);
    ...
}
```

There is no attribute macro. Debug- and trace-level `#[instrument]`s in hir-ty, its split-out
`hir-ty-*` crates and the solver become `span!(trace, ..)` (5.1); elsewhere they are deleted. The
26 info-level ones become `span!("<fn>"; ..)` with their recorded fields kept. An instrumented
function with `ret` gets an explicit return event (5.1, section 8.4), so no recorded value is
lost.

Rejected: a `#[blaze_trace::span]` proc macro. blaze's proc macros compile to wasm and run in
Wasmtime on every expansion (`proc-macros-on-wasm`), which adds expansion cost to blaze's own
build for one line of savings; stage 0 would have to build it first; and it hides a span's cost
and field formatting in an attribute.

### 2.4 Guards, held spans, ids

```rust
#[must_use] pub struct Guard { ring: &'static Ring, slot: u32, id: u64, _here: PhantomData<*const ()> } // !Send
#[must_use] pub struct Held  { ring: &'static Ring, slot: u32, id: u64 }   // Send, not Clone
pub struct Entry<'a>(&'a Held);                                           // entered for one step
#[derive(Clone, Copy)] pub struct SpanId(u64);                            // a reference, for parents across threads

impl Held { pub fn enter(&self) -> Entry<'_>; pub fn in_scope<T>(&self, f: impl FnOnce() -> T) -> T; }
pub fn current() -> Option<SpanId>;   // the innermost span this thread is inside: one TLS read
```

- No refcount on blaze-trace spans: a `Guard` or `Held` is unique, so close does no
  read-modify-write. (`Slot::refs` stays only for spans the phase-0 `Recorder` makes from cloned
  tracing spans, and goes in phase 2: section 8.)
- `Option<Guard>` replaces `Span::none()`, and a block holding a guard replaces `in_scope`.
- `held` is a site flag. Each `Entry` writes an enter row and a leave row (a new row byte, 10).
  pd-cost derives a held span's CPU from those rows and the job's switches.
- `Guard` stays `!Send` but survives fiber migration: the fiber carries its enter stack
  (`take_open`/`put_open`), and the guard names its ring and slot, so it closes from whichever
  thread drops it.

### 2.5 Call sites without a hash map

```rust
pub struct Site {
    pub name: &'static str,            // span name, or the event's message literal ("" for none)
    pub target: &'static str,          // module_path!()
    pub file: &'static str, pub line: u32,
    pub level: Level, pub what: What,  // Span | HeldSpan | Event | SpanKind | EventKind
    pub fields: &'static [&'static str],
    id: AtomicU16,                     // 0: not registered; REFUSED: past the table
}
impl Site {
    #[inline(always)] pub fn id(&'static self) -> u16 {
        match self.id.load(Relaxed) { 0 => self.register(), id => id }  // one relaxed load
    }
    #[cold] fn register(&'static self) -> u16; // lock, push into SITES[n], compute the generic map, store id
}
static SITES: [AtomicPtr<Site>; 4095];  // indexed by id - 1: no probing, no hashing
```

The table keyed by `tracing::Metadata` addresses goes away, with `home()`, `site(meta)` and the
half-full rule. When the table is full, `register` stores `REFUSED`, the site records nothing,
and every call adds to `Plot::ProfileSpansLost`, a counted and named loss as today. A test keeps
the static site count under 2048. pd-postmortem's persisted site records (name, target, level)
come straight from `Site`. During phases 0 and 1 the `Recorder` registers a leaked `Site` per
tracing callsite (section 8.1).

Rejected: link-time site ids (a distributed-slice section). The lazy id costs one load, and
sections differ on Mach-O, ELF, PE and wasm.

### 2.6 Levels and compile-time filtering

`Level = Error | Warn | Info | Debug | Trace`. The rule depends only on the level:

| level | compiled | recorded |
|---|---|---|
| error, warn, info | always | always, to the ring and the history |
| debug (events only) | always | always to the ring; the history keeps them only for a build in trouble (pd-logs' `Hold`) |
| trace (spans and events) | only with the `blaze-trace/trace` feature | when compiled |
| spans (no level) | always | always (short ones folded, as today) |

```rust
// in blaze-trace: the const, not the caller's cfg (a cfg! in the macro would read the caller's features)
pub const TRACE: bool = cfg!(feature = "trace");
// event!(trace, ...) expands to:
if $crate::TRACE { /* site + write */ }
```

The branch folds away at compile time. The arguments are not evaluated but still type-check, so
trace sites don't rot. There is no runtime level filter: the recorder is always on, and a filter
nobody turns would be a branch on every call. Target no longer enters the keep rule (pd-logs'
`keeps_event`), because every site is blaze's.

**The `trace` feature comes only from the command line** (`blaze test -p hir-ty --features
blaze-trace/trace`), never from a manifest. `blaze::disallowed_crates` (7.2) refuses any
manifest in the workspace whose dependency entry or `[features]` table turns on
`blaze-trace/trace`, naming the manifest and line. A release build therefore never has it.

Rejected: tracing's `max_level_*` features plus a runtime interest cache. A level would cost an
atomic load and a cached-interest check at every compiled site.

### 2.7 Fields as words, with no visitor

```rust
pub trait Field { const KIND: FieldKind; fn word(&self) -> u64; }   // FieldKind: I64 U64 F64 Bool Text
// ints, bool, char, f64 (to_bits): the value itself
// Name: its Sym. &str and String: intern_cached(text)
pub struct Shown<'a, T: ?Sized + fmt::Display>(pub &'a T); // %x
pub struct Dbg<'a, T: ?Sized + fmt::Debug>(pub &'a T);     // ?x
```

- A number is a word, with no formatting and no allocation.
- A string literal value (`k = "the doctests' runner"`) matches the macro's `$v:literal` arm and
  gets a per-site `static NameOnce`, interned once as a kept name: a relaxed load per call. A
  `LitWord` trait sends integer literals to the plain `Field` path.
- **Text** (`&str`, `%x`, `?x`, a formatted message): formatted into a 256-byte stack buffer that
  is a `fmt::Write`. Text is **never truncated**. When a write would pass 256 bytes, the writer
  calls a `#[cold] #[inline(never)]` function that copies what is there into the ring's growable
  text buffer (an owner cell, a `Vec<u8>` that keeps its capacity: `// alloc: grows to the longest
  text a ring has formatted`) and continues there. Then `intern_cached` looks the bytes up in a
  64-entry direct-mapped cache in the ring: an entry holds the interned `Arc<str>` and its `Sym`,
  compared by bytes, valid while its shard's epoch is the current or the previous one (one
  relaxed load). A miss takes today's shard lock. Allocation happens only the first time a name is
  seen in a generation, as today. The global `TEXT: RefCell<String>` goes away.
- Where words go: generic fields go into the slot's `of` and `unit` words, kind fields and an ad
  hoc span's other fields into `typed`, and a slot `mask` word says which typed words are valid,
  so freeing a slot never zeroes them. A span written whole writes its non-generic fields as field
  rows after its row. Event fields are pd-logs' `EVENT_FIELDS_ROW` rows, 5 values per row, from
  the word array, with no `Logged` buffer. Folded spans drop their fields.

Rejected: lazy formatting at read time (defmt-style). A `Display` value is gone by the time the
row is read. Rejected: truncating at 256 bytes. A path or a type name past 256 bytes is exactly
the one a reader needs whole.

## 3. Cost model

### 3.1 The path

**TLS model.** The thread's state is one `const`-initialized `thread_local!` block (`Local`: the
ring pointer, the enter stack top, the fold pages, the text cache) with no destructor, so there
is no lazy-init check. Each operation reads it once and passes `&Local` down; no callee reads it
again. The budget assumes the local-exec model in a release executable on Linux (aarch64: `mrs
tpidr_el0` plus an add; x86_64: a `%fs:`-relative load). macOS goes through `_tlv_get_addr` (a
call) and is measured, not budgeted (3.4). A dev build whose code is loaded as a patch or dylib
uses TLSDESC, and blaze-link does not relax TLSDESC to IE or LE in executables
(`crates/blaze-link/src/reloc.rs` ~line 361 resolves `A64TlsDesc*` through the GOT and leaves the
call). That is a separate perf issue, owned by the blaze-link lane: lane 0.1 files it as
"blaze-link: relax TLSDESC in executables" with the bench numbers attached.

**The owner check.** Each ring stores its owner's thread pointer at claim (`tpidr_el0` on Linux
aarch64, `tpidrro_el0` on macOS aarch64, `%fs:0` on x86_64 Linux, `%gs:0x30` on Windows x64,
read with one inline-asm instruction; wasm has one thread). A guard names its ring, so close
compares the current thread pointer with `ring.owner_tp`: equal means the closer owns the ring,
and close does **no TLS read at all**. Only a migrated fiber's close reads the TLS block.

**Open** (`Guard::open` then the steady-state `open_on(&Local, ..)`): `site.id()` (one relaxed
load); one TLS read; pop a free slot from the ring's owner-only stack (2 plain loads, 1 store);
the store-store barrier (3.2); one clock read; relaxed stores of `start`, `parent` (top of the
enter stack, in the ring's line), `what` (site, depth, build), `of`/`unit` or the field words,
`mask`, `kinds`, `owner` (lease and thread, one word made at claim); add 1 to the parent's
`kids_local` (plain load and store, same ring) or `kids_remote` (`fetch_add`, only under a parent
from another ring); push the id on the enter stack (2 stores); a release store of `slot.id`.

**Close** (`Drop for Guard`): the owner check; one clock read; about 10 relaxed loads of the slot;
then fold or write the row. **Fold**: the ring's fold table is indexed directly by the dense site
id, `folds[id >> 6][id & 63]`: 64 pages of 64 entries per ring, each page allocated on the first
fold of a site in it (cold, `// alloc: once per ring per 64 sites`). One probe, no hash.
**Write**: 8 relaxed stores, the store-store barrier, a release store of `head`, and the hot
sketch. Then pop the enter stack and free the slot (3.2).

**Event**: `site.id()`, one TLS read, one clock read, one row (plus a field row per 5 fields), and
`keep(span)` on the innermost span, using the same `kids_local`/`kids_remote` split.

**Allocation and frame-copy counts are test-only.** The slot's allocation and frame-copy words
are written only with the `testing` feature (`blaze_trace::COUNTS`, a const like `TRACE`). In a
release build those `spans` columns are null; allocation by stack is ix-prof's allocator profile
and the memory counters' job. The `OnceLock<fn>` hooks go away.

### 3.2 Slot reuse ordering

Readers (the sampler, the watchdog, `opened`) read a slot as a seqlock: acquire-load `id`, read
the words relaxed, `fence(Acquire)`, reload `id` and drop the read if it changed (today's
`Ring::opened`, `flight.rs` ~line 1350). Slot reuse must keep that check sound:

- **Close**: store `slot.id = 0` (release) before the slot goes on any free list. A local free is
  a plain push after it; a remote free is a Treiber push (release CAS) onto the maker ring's
  remote list, taken whole by the owner with one acquire `swap` when its local stack is empty.
  Today's `Ring::free` (~line 1342) already stores id 0 before clearing the `taken` bit.
- **Open**: pop, then a store-store barrier (`dmb ishst` on aarch64, a compiler fence on x86_64
  and wasm), then the field stores, then `slot.id = new` with release, last. The barrier orders
  the earlier `id = 0` before the new field words, so a reader that saw the old id and then a new
  word sees `id != old` on its reload.
- The barrier is counted in every budget row that opens a span. A written span pays a second one
  before `head`.
- **Test**: `crates/blaze-trace/src/tests/reuse.rs`, a stress test run on the aarch64 bench
  runner and in an aarch64 ix VM: N threads open, close and remote-free spans in a tight loop
  while M reader threads run the seqlock read and assert that no read returns words from two
  spans (each span writes its id into every word it owns). It runs under `--release`, 10 s,
  and is the hegel property test of risk 2 with a real memory model.

### 3.3 Against today

What changes against today's path (docs/v2/plan.md, "A span"):

| | today | here |
|---|---|---|
| dispatch | 4 dynamic calls through tracing's dispatcher, a cached-interest check, the `Metadata` hash probe | none, all inline |
| TLS reads | ring, `BUILD`, `RUNNING`, `FOLDS`, `TEXT`, `KEPT` | 1 per open, 0 per close on the owner thread, 1 per event |
| read-modify-writes | `taken.fetch_or`, `refs.fetch_sub(AcqRel)`, `taken.fetch_and`, parent `kids.fetch_add` | none on a span that opens and closes on one ring |
| barriers | `fence(Release)` per row (`dmb ish` on aarch64) | `dmb ishst` at open and per written row; a compiler fence on x86_64 |
| fold lookup | hash of the site in `FOLDS` | `folds[id >> 6][id & 63]` |
| indirect calls | `allocated()` and `frame_copied()` through `OnceLock<fn>` | none; counts only with `testing` |
| typed words | all `TYPED` stored | only those said, plus `mask` |
| hot sketch | count-min, top keys and DDSketch on every close | only on spans written whole (20 µs or longer) and once per fold flush; folded waits go in the DDSketch's lowest bucket |

### 3.4 Budgets and the bench

Per operation, steady state, warm caches. Instructions are user-mode instructions retired,
checked on the Linux runners; macOS is time only and has no budget.

| case | Linux aarch64 (Neoverse) | Linux x86_64 | instructions | macOS arm64 |
|---|---|---|---|---|
| `span_open_close_folded` (ad hoc, no fields) | ≤ 12 ns | ≤ 20 ns | ≤ 100 | timed |
| `span_open_close_written` (forced whole: row, barrier, hot sketch) | ≤ 40 ns | ≤ 55 ns | ≤ 320 | timed |
| `span_kind_wait` (4 typed words, folded) | ≤ 14 ns | ≤ 22 ns | ≤ 120 | timed |
| `span_text_field_hit` (one `&str`, cache hit) | ≤ +6 ns | ≤ +8 ns | ≤ +60 | timed |
| `span_shown_field_hit` (one `%` of a 16-byte `Display`, cache hit) | ≤ +25 ns | ≤ +35 ns | ≤ +250 | timed |
| `event_static` (no fields, no captures) | ≤ 5 ns | ≤ 8 ns | ≤ 40 | timed |
| `event_fields3` (3 numbers) | ≤ 8 ns | ≤ 12 ns | ≤ 70 | timed |
| `span_trace_compiled_out` | 0 | 0 | equal to `empty` | timed |
| `span_ringless` (thread past `RINGS_MAX`) | ≤ 1.5 ns | ≤ 2 ns | ≤ 10 | timed |

Two clock reads per span are the floor. On x86_64 `rdtsc` is about 25 cycles each, which is why
the x86 budget is higher. Text overflow (2.7) is cold and has no row.

**The bench**: `crates/blaze-bench/benches/trace.rs`, the cases above plus `empty` and, until
phase 2, `tracing_info_span` (today's path, the before number), and the ablations that prove the
causes (`test-the-cause`): `span_open_close_no_sketch`, `span_open_close_full_fence`,
`span_fold_hashed` (the old fold lookup).

The gate does not exist yet, so phase 0 builds it. Reviewer: `.github/workflows/bench-counts.yaml`
does exist, but it counts `harness/bench.py` projects, not `blaze_bench` cases, so the point
stands.

- `blaze_bench` (`crates/blaze-bench/src/lib.rs`) gains instruction counts: on Linux, a
  `perf_event_open` counter of `PERF_COUNT_HW_INSTRUCTIONS` with `exclude_kernel`, read around
  each sample's iterations, reported per iteration. `Benchmark` gains an optional per-case
  `instructions` budget, and a run with `--instructions` fails a case whose median exceeds it,
  naming the case, the budget and the count. A machine that refuses the counter fails the run
  (`no-fallback`); macOS never asks and prints times only.
- `.github/workflows/bench.yaml` gains a job `trace` on the self-hosted `bench-arm64` and
  `bench-x86` runners (the `linux-aarch64` and `linux-x86-64` entries of
  `harness/benchplatforms.py`), on pull requests and main: the `perf_event_paranoid` step of
  `bench-counts.yaml`, then `blaze build --release --locked -p blaze-bench --bench trace` and the
  binary with `--instructions`. It shares those runners' queue with bench-counts and bench-readme.
- The PR that lands phase 0 quotes before and after from these numbers and from self-build spans
  (`slow-is-p0-traced`).

**Effects.** The steady-state functions, `open_on(&Local, ..)`, the owner-path close and the event
writer, carry `#[blaze::noalloc]` and `#[blaze::nonblocking]` (`blaze-uses-its-extensions`).
`Site::register`, the first allocation of a slot chunk or fold page, the intern miss and text
overflow are `#[cold]` functions outside them, and `Guard::open` is the thin wrapper that calls
those before `open_on`.

## 4. Context propagation

- **Within a thread**: the ring's enter stack (`entered[OPEN_MAX]`, `depth`). A span's parent is
  the top of the stack unless it says `root` or `parent:`.
- **Fibers**: as today, a parked fiber's enter stack goes with it (`take_open`, `put_open`,
  `end_open`), and its open slots stay on the ring that made them. `blaze-graph` calls
  `blaze_trace::fiber::{take, put, end}` directly. Nothing tied to a thread is held across a
  wait: a guard holds a `&'static Ring`, a slot index and an id, and every TLS read happens in a
  call that ends before any wait (`no-wait-across-thread-locals`).
- **Threads**: `let parent = blaze_trace::current();` then, on the other thread,
  `span!(parent: parent?, "wasm expansion run")`. `SpanId` is `Copy` and `Send`.
- **Follows-from** is the `Job` kind's `creator` column (`giving_span`), which the critical path
  already crosses. Rejected: a `follows:` link per span; it needs another slot word and nothing
  reads one.
- **Processes**: `blaze_profile::child_env` has no callers today, so nothing propagates yet.
  The one place a blaze process starts another blaze to work for it is the client starting the
  daemon (`crates/blaze-cargo/src/cli/daemon.rs` ~line 704, `blaze serve --ready-fd`). There the
  client sets `BLAZE_TRACE_PARENT=<pid>.<span_id>.<build>` from `blaze_trace::child_env()`, and
  `BLAZE_PROFILE` from `blaze_profile::child_env()`. The daemon reads it once when its first ring
  is claimed, its startup root spans take that span as parent, and it writes the parent's pid
  into a new nullable column `spans.parent_process`, appended so `VERSION` is kept. Every other
  spawn strips it: the `__launch` trampoline (`crates/blaze-cargo/src/cli/run.rs`,
  `through_trampoline` and `trampoline`), every sandboxed spawn (`crates/blaze-cargo/src/sandbox.rs`
  and its platform modules, the launcher of `serve.rs` ~line 816) and `daemon install`'s service
  unit (~line 808). It removes it with `env_remove` even where the environment is built fresh, so
  a test pins it. It is in no action key: lane 1A adds it to the variables the key builder refuses
  to read, with a test that two builds differing only in it share keys. Build scripts and test
  binaries are user code and link no recorder; their record is the daemon's spans around them.

## 5. Migration

### 5.1 Mapping rules

"Recorded today" means the `Recorder` keeps it: info level or above, with a `blaze*` target or a
`*_query` name for spans, plus pd-logs' event rule. Three groups of code differ:

- **blaze's own crates**: the table below.
- **hir-ty, its split-out `hir-ty-*` crates, and the solver** (`vendor/rustc_next_trait_solver`,
  `vendor/rustc_type_ir`, `crates/hir-ty/src/next_solver`): by the owner's decision, every
  `debug!`, `trace!`, `debug_span!`, `trace_span!` and debug- or trace-level `#[instrument]`
  becomes a `trace`-level site (`event!(trace, ..)`, `span!(trace, ..)`), zero cost unless the
  feature is on. `#[instrument]` arguments not skipped become `?arg` fields.
- **Other vendored and third-party crates** (charon, rustc_transmute, the quinn stack,
  pattern_analysis): every tracing site is deleted, `warn!` and `error!` included, because blaze
  does not record third-party logs. This settles the old contradiction between this table's
  "warn/error with any target" row and the deleting patches of 5.3: that row is for blaze's own
  crates only.

| tracing form (blaze's own crates) | becomes |
|---|---|
| `info_span!(target: "blaze", "n", f..).entered()` | `span!("n"; f..)` |
| `info_span!("n", ..)` with no target, in a `blaze*` crate | `span!("n"; ..)` |
| `info_span!("x_query", ..)` with no target, front-end crates | `span!("x_query"; ..)` (recorded today by name) |
| `info_span!` with no target, name not `*_query`, rust-analyzer-derived crate | **delete** (never recorded) |
| other targets (`"perf"`, `"prop"`, `"hir_ty"`, ...) | `blaze*` prefix: convert; otherwise delete |
| `debug_span!`, `trace_span!`, debug/trace `#[instrument]` (outside hir-ty and the solver) | **delete** (compiled out of release by `release_max_level_info`) |
| `#[instrument(target = "blaze", ..)]` (info) | `let _span = span!("<fn>"; <fields>);` as the first statement |
| `#[instrument(target = "blaze", ret)]` | the span, plus `event!(info; ret = ?out)` before each return (section 8.4) |
| `info!/warn!/error!(target: "blaze", f.., "msg {x}")` | `event!(info/warn/error, "msg {x}"; f..)` |
| `warn!/error!` with any target | `event!(warn/error, ..)` |
| `info!` with a non-`blaze` target, rust-analyzer-derived | **delete** |
| `debug!(target: "blaze", ..)` | `event!(debug, ..)` |
| `debug!`/`trace!` with no target, outside hir-ty and the solver | **delete** (pd-logs keeps debug only for target `blaze`) |
| span field not `krate/crate`, `function/subject`, `unit/cgu` | **dropped** (never recorded), listed in the lane handoff |
| `"crate"`/`krate`, `subject`, `cgu` | `krate`, `function`, `unit` |
| `f = field::Empty` then `.record("f", v)` | a kind with `=> Ended { f }` if a reader reads `f`, else `event!(info, "<span> ended"; f = v)` inside the span |
| `tracing::Span::current()` | `blaze_trace::current()` |
| `parent: &span` / `parent: None` | `parent: id` / `root` |
| `Span::none()`, `Span` in a struct | `Option<Guard>`, or `Held` / `Option<Held>` |
| `span.in_scope(f)` | `{ let _g = span!(..); f() }`, or `held.in_scope(f)` |
| `held = true` field | `span!(held, ..)` |
| `Span::current().record("ahead", true)` (blaze-macro-eval) | an `Ended` field on that span's kind |
| `event_enabled!` / `enabled!` (blaze-link `shape.rs`) | delete the check |
| test `impl tracing::Subscriber` | `blaze_trace::testing::capture` (section 8.4) |
| hir-ty `setup_tracing` (29 call sites) | `blaze_trace::testing::print_tree()` under `SOLVER_DEBUG` (section 8.4) |
| ix-prof `layer.rs` | moves to its own workspace (section 6.3) |

### 5.2 By name resolution, not by text

The move goes through `.clippy.toml` bans with replacements, enforced by blaze-lints'
`disallowed_macros`, `disallowed_types` and `disallowed_methods`, and applied with `blaze check
--fix` (`fix-through-blaze-lints`). The forms differ in shape (a `target:` argument, `.entered()`,
field order, deletions), so the fix gets a call rewriter of its own,
`crates/blaze-lints/src/lints/disallowed/tracing.rs`, like the locks rewriter (std to
parking_lot): it resolves the call's path to tracing's item, then rewrites the arguments as
syntax, following 5.1:

- a span or event macro becomes `blaze_trace::span!` / `event!` (`target:` dropped, fields moved
  after `;`, aliases renamed, `.entered()` dropped), a `trace` site in hir-ty and the solver, or
  is deleted (a statement removal; an expression position gets `()`);
- `#[instrument]`: delete it, or move it into the body as `let _span = ...;`;
- `tracing::Span::current` becomes `blaze_trace::current`, `tracing::Level::X` becomes
  `blaze_trace::Level::X`.

Anything else gets a finding without a fix for the lane to handle by hand: `field::Empty` with
`.record`, `Span` in a struct, `Span::none`, `in_scope` on a stored span, `held`, `ret`, test
subscribers. Resolution matters because the tree has `use tracing::{debug, info}`, glob imports
and bare `debug!`. `disallowed_macros` must resolve attribute macro paths (`#[instrument]`,
`#[tracing::instrument]`) as it resolves `#[derive]`; that is phase 0.

The released blaze knows neither the rewriter nor the new keys, so **lanes run `--fix` with the
blaze their tree builds** (`target/release/blaze check --fix -p <crate>`, built once by the
integrator after phase 0 lands), never the stage-0 release. Finding counts per crate go in
`crates/blaze-lints/baselines/clippy.disallowed_macros.txt` (and `_types`, `_methods`); they only
shrink and the files are deleted in phase 2.

The vendored solver crates and charon keep a patch series against upstream, so `--fix` does not
touch them. Their lanes add one patch per crate: `NNNN-blaze-trace` for the solver (sites to
`trace`, the `tracing` dependency to `blaze-trace`) and `NNNN-no-tracing` elsewhere (deletion),
made with astlog's syntax-only rewriter and checked by a build. `tools/v2/solver_sync.py` replays
it on every toolchain bump; `vendor/charon/blaze.diff` takes charon's, including deleting
`logger.rs`'s subscriber.

## 6. Third-party crates

`Cargo.lock` at the base has these crates depending on `tracing`:

| crate | who pulls it | action |
|---|---|---|
| hyper-util 0.1.21 | blaze-registry (`client-legacy`), hyper-rustls 0.27 | **dropped**, with hyper-rustls and hyper (6.1) |
| quinn 0.11.12, quinn-proto 0.11.19 | wtransport, from blaze-explore | quinn-proto vendored with a patch series (6.2); quinn dropped (2026-11-04: http3-sans-io.md) |
| quinn-udp 0.5.16 | quinn | dropped with quinn |
| wtransport 0.7.2 | blaze-explore | dropped (2026-11-04): blaze-http's h3 core drives quinn-proto (http3-sans-io.md) |
| ra-ap-rustc_pattern_analysis 0.179.0 | hir-ty-matchck | vendored with a patch series (6.2) |
| charon (vendor) | hir-ty-charon | `blaze.diff` (5.2) |

### 6.1 One HTTP/1 client, sans-IO, no hyper

blaze-registry's `HyperHttp` (`crates/blaze-registry/src/http/client.rs`) is the only user of
hyper-util's `client::legacy::Client`, of hyper-rustls, and of hyper itself. It moves onto a new
crate, `crates/blaze-http`: a sans-IO HTTP/1.1 client core (`blaze_http::Client`, a synchronous
state machine on an injected clock that owns no socket, runtime or timer) and a thin tokio driver
(`blaze_http::tokio_driver`, sockets, DNS and timers only). The core writes requests, parses
answers with `httparse` (`Content-Length`, `chunked`, read to close), keeps connections between
requests in a pool keyed by (scheme, host, port) with hyper-util's limits (64 idle, 90 seconds),
follows at most 10 redirects, decodes gzip with `flate2`, holds bodies to their bound, speaks TLS
through rustls's own sans-IO `ClientConnection` (ring, the machine's roots, no ALPN), and turns
every timeout into a deadline. Its IO contract (connect, send, receive, close, each with its
completion) is the one an io_uring driver is written against later, with no change to the core.
`HyperHttp`'s API, redirect rules, bounds and timeouts stay as they are. `hyper`, `hyper-util`,
`hyper-rustls`, `http-body-util` and `tokio-rustls` leave the manifests and the lock. Rejected:
vendoring hyper-util to cut `dep:tracing` (126 lines of patch kept forever for a pool of about
150 lines), and a pool on `hyper::client::conn::http1` with `tokio-rustls` (the first pass: it
ties the client to tokio's IO traits, so an io_uring driver would be a rewrite).

### 6.2 Vendored patch series

quinn-proto and ra-ap-rustc_pattern_analysis each get `vendor/<crate>/` (quinn and wtransport
had copies too until blaze-http's h3 core replaced them, docs/v2/plan/design/http3-sans-io.md)
laid out as `vendor/rustc_next_trait_solver` is: `src/` is upstream at a pinned version with the
series applied, `patches/NNNN-name.patch` one per change, `PATCHES.md` with a `### NNNN-name`
heading per patch (what, why), `Cargo.toml.orig`, and a line under `[patch.'crates-io']` in the
root `Cargo.toml`. The first patch of each is `0001-no-tracing`: delete tracing macro statements,
`#[instrument]`, `.instrument(..)`, `Span` fields and the dependency (quinn's also drops
`quinn-udp/tracing`). `tools/vendor/sync.py`, in Python, taking one JSON value `{crate, version}`
(`scripts-take-typed-input`), replaces the base on a version bump and replays the series,
stopping at the first patch that no longer applies. **Gate**: each crate's own tests built and run
by blaze (`blaze test -p quinn-proto` and so on) pass on the patched copy, in lane T2 and on every
bump. Their dev-dependencies stay out of blaze's workspace as `vendor/sonic-rs`'s do.

**No shim.** A no-op crate named `tracing` under `[patch]` would cost nothing at run time, but it
would be an adapter: the name would stay in `Cargo.lock`, the lockfile ban could not tell it
apart, and an upgrade could silently bring back a real dependency. Rejected: a `tracing` shim
for third-party code only.

### 6.3 ix-prof

ix-prof is a profiler for any Rust program (`docs/ix-prof.md`), so it keeps a `tracing` layer for
other programs, and it must not depend on `blaze-trace`. The layer (`crates/ix-prof/src/layer.rs`,
feature `layer`) moves to `crates/ix-prof-tracing/`, a workspace of its own with its own
`Cargo.lock`, excluded from the root workspace like `crates/harness`. ix-prof exposes the span
calls the layer uses (`span_open`, `span_record`, `span_enter`, `span_exit`, `span_close`,
`recording`, `alloc::quiet`) as a public `ix_prof::spans` module. ix-prof's own waits (its
`join` and `accept_backoff` spans) are recorded through `ix_prof::spans` into its own profile,
not blaze's.

The end state: `tracing`, `tracing-core`, `tracing-attributes`, `tracing-subscriber`,
`tracing-log` and `tracing-tree` are absent from the root `Cargo.lock`. `harness/` and
`crates/ix-prof-tracing/` have their own locks, outside blaze's build. Removing
`tracing-attributes` also removes a `syn`-based proc macro blaze ran on wasm 198 times per build
of itself.

## 7. Enforcement

### 7.1 `.clippy.toml` (during migration)

In the root `.clippy.toml`, mirrored in the hot crates' `clippy.toml` (blaze-query, blaze-cas,
intern, blaze-history, blaze-hashcons, blaze-reclaim, blaze-graph, blaze-ids, blaze-mem), all with
`allow-invalid = true`:

```toml
disallowed-macros = [
  { path = "tracing::info_span", replacement = "blaze_trace::span", reason = "blaze records through its own recorder (docs/v2/plan/design/no-tracing.md)", allow-invalid = true },
  # likewise: tracing::{span, debug_span, trace_span, warn_span, error_span} -> blaze_trace::span
  # tracing::{event, info, warn, error, debug, trace} -> blaze_trace::event
  # tracing::{enabled, event_enabled, span_enabled} and tracing::instrument: no replacement
]
disallowed-types = [
  { path = "tracing::Span", reason = "a span is a blaze_trace::Guard (entered) or Held (open across steps); Option<_> for none", allow-invalid = true },
  # tracing::span::{Entered, EnteredSpan, Attributes, Record, Id}, tracing::Level (-> blaze_trace::Level),
  # tracing::Metadata, tracing::Subscriber, tracing::Instrument, tracing::field::{Empty, Visit, Field},
  # tracing_core::*, tracing_subscriber::{Registry, Layer, fmt::Layer, filter::Targets, EnvFilter}
]
disallowed-methods = [
  { path = "tracing::Span::current", replacement = "blaze_trace::current", reason = "...", allow-invalid = true },
  # tracing::Span::{none, record, in_scope, enter, entered, follows_from},
  # tracing::subscriber::{set_global_default, set_default, with_default}, tracing::dispatcher::*
]
```

Once phase 2 removes tracing from the lock these resolve to nothing, so phase 2 deletes them.
The lasting ban is 7.2.

### 7.2 `blaze::disallowed_crates`: the lockfile ban, in `blaze check`

A new blaze lint in `crates/blaze-lints`, deny by default, configured in the workspace root's
`.clippy.toml` (a workspace-level key, read only from the root):

```toml
disallowed-crates = [
  { name = "tracing", reason = "blaze records through blaze_trace (docs/v2/plan/design/no-tracing.md)" },
  { name = "tracing-core", reason = "..." }, { name = "tracing-attributes", reason = "..." },
  { name = "tracing-subscriber", reason = "..." }, { name = "tracing-log", reason = "..." },
  { name = "tracing-tree", reason = "..." },
]
```

- It runs once per `blaze check` invocation, whatever packages are named, over the lockfile's
  **whole package set**: every `[[package]]` of the root `Cargo.lock`, which covers all
  features, targets, dev- and build-dependencies, not the resolve for the host and the features
  this build asked for. Matching is by package name, any version, any source, path packages
  included.
- A finding names the banned package and version and the chain from a workspace member to it
  over the lock's dependency edges (`blaze-explore -> blaze-http -> quinn-proto 0.11.19 ->
  tracing 0.1.44`, were the patch lost), points at the member manifest's dependency line, and quotes the `reason`
  (`errors-name-the-thing`).
- The same walk refuses a manifest that turns on `blaze-trace/trace` (2.6).
- `crates/blaze-lints/src/conf.rs` gets a `DisallowedCrate { name, reason }` list parsed and
  checked in `checked()`.

**The released blaze ignores it.** `checked()` validates known keys and ignores unknown top-level
keys, so the stage-0 release (`.github/blaze-version`, 0.20261030.0) reads `disallowed-crates` as
nothing. Until a release has the lint, CI enforces it with the tree's blaze: a step in
`.github/workflows/blaze-platform.yaml`'s `build` job, right after "blaze build --release, of
what the unit tests are built on", runs `target/release/blaze check -p blaze-trace --message-format
short`, which runs the workspace-level lint once. After phase 2 lands and a release is cut,
`.github/blaze-version` is bumped to it (integrator, a follow-up PR), and contributors' own
`blaze check` enforces it too.

It goes in `blaze check` rather than a CI script because the owner said `blaze check` denies
tracing, and an agent's loop runs `blaze check`. Rejected: a Python check of `Cargo.lock` in
`.github/scripts`, which an agent would only hit in CI.

### 7.3 The lints that knew tracing

- `crates/blaze-lints/src/blaze/sans_io_core.rs` (~line 161) and `syscall_in_loop.rs` (~line 61)
  keep a stop list of crates whose calls the walk does not follow. `"tracing", "tracing_core"`
  becomes `"blaze_trace"` in both.
- `blaze::unrecorded_span` is deleted whole in phase 0 (no site can be unrecorded once the keep
  rule depends on level only): `crates/blaze-lints/src/blaze/unrecorded_span.rs`, its entries in
  `blaze.rs` and `exempt.rs`, `Lint::UnrecordedSpan` and its tests, the root `Cargo.toml`'s
  `unrecorded_span = "deny"` (~line 159), `crates/blaze-lint-defs/src/names.rs` (~line 164) and
  `lints.rs` (~line 945), blaze-lints' `blaze-profile` dependency (used only by that lint), and its
  mention in `docs/profiling.md`. The integrator regenerates `tools/nix/units/*.json`
  (`tools/nix/unit_graph.py`), since the dependency graph changes.
- After blaze-graph's hooks become direct calls (lane 0.2), the integrator re-runs the effects
  lint over blaze-graph (`target/release/blaze check -p blaze-graph`). Any new finding on a wait,
  job or fiber path is fixed or its cold callee marked, not allowed.

## 8. Deletions and tests

### 8.1 Phase 0 coexistence

Phase 0 keeps the `Recorder` so unconverted sites keep recording while both paths write the same
rings. That needs two things, both deleted in phase 2:

- **A leaked `Site` per tracing callsite.** `register_callsite` builds a `Site` from the
  `Metadata` (name, target, file, line, level, field names) and leaks it (`Box::leak`, `// alloc:
  once per tracing callsite, until phase 2`), storing the pointer in the old metadata table, so
  tracing spans get a dense id like any site.
- **`Slot::refs` for cloned tracing spans.** A tracing `Span` is `Clone`, so slots the `Recorder`
  opens keep the refcount and close on the last `try_close`; blaze-trace guards never touch it.
  A site flag (`What::Tracing`) says which close path a slot takes.

### 8.2 Phase 2 deletions (exact list)

All of the following are **phase 2**, in the same batch as phase 1.

`crates/blaze-profile/src/lib.rs`: `struct Recorder`, `Recorder::wanted`, `impl tracing::Subscriber
for Recorder`; `TARGET`, `LEVEL`, `keeps`, `keeps_level`, `wants`, `opened`, `ours`, `struct
Subject` and its `Visit`; `set_global_default(Recorder)` in `install()` and
`rebuild_interest_cache()` in `start()`; in `profiled`, `flight::meta` becomes
`blaze_trace::site(id)`; `link_output`'s and the tests' `info_span!` are converted.

`blaze-trace`'s flight code: `type Meta`, the metadata table, `home`, `site(meta)`,
`register(meta)`, `meta(site)`, `field_of`, `HELD_FIELD` and its test, `TAKEN`; `struct Fields`
and its `Visit`, `opened(&Attributes)`, `cloned`, `recorded`, `Slot::refs`, `What::Tracing` and
the leaked sites (8.1); `evented(&tracing::Event)`, `struct Message` and its `Visit` (pd-logs'
`Logged` visitor too). Already gone in phase 0 by the new design, listed so nothing is missed:
the `TEXT`, `BUILD`, `RUNNING`, `FOLDS`, `KEPT` thread-locals (now `Local` and ring owner cells),
the `taken` bitmap (now free lists), the `OnceLock<fn>` count hooks, `fence(Release)` in
`Ring::put` (now the store-store barrier).

`span.rs`: `KindCallsite`, `kind_meta`, `SiteOnce` (now `Site::id`); `generic_of`'s aliases
(`"crate"`, `"subject"`, `"cgu"`); the module doc's paragraph on ad hoc `tracing::info_span!`. The
generic columns `crate`, `function` and `unit` stay: kinds and ad hoc fields fill them, and the
hot sketch key, `blaze slow` and the views read them.

pd-logs: `logs::keeps_event`, `keeps_event_named`, `blaze_debug`; the `release_max_level_debug`
line; `events_wide`'s fixed pivot (now the `event_<name>` views).

blaze-graph (phase 0, lane 0.2): `pump::WaitSpans`/`span_waits`, `pool::JobSpans`/`span_jobs`,
`fiber::Carried`/`carry`, and `wait::WaitKind` (moved).

Tests: the three test subscribers; hir-ty's `setup_tracing` and its `tracing-subscriber` and
`tracing-tree` dependencies; `crates/hir-ty-mir/src/tests/variance.rs`'s commented subscriber.

Manifests: the workspace's `tracing`, `tracing-subscriber` and `tracing-tree` entries; every
crate's `tracing.workspace = true` (in each crate's own phase 1 lane); the solver crates'
`tracing = "0.1"` (by patch).

Config: the tracing entries of `.clippy.toml` and the hot crates' `clippy.toml`.

Docs: `docs/profiling.md` (tracing targets, the keep table, column docs that say "tracing
target" or "tracing event"); `crates/blaze-profile/src/schema.rs` docs; `docs/cargo.md` (one
mention); `docs/v2/plan.md`; `tools/agents/rules.ts` rule 16's
`tracing::info_span!(target: "blaze", ...)` becomes `blaze_trace::span!(...)`, then `bun
tools/agents/rules.ts --write` regenerates `AGENTS.md` and the skills. fed1eb428d:docs/v2/requests and lane
plans are dated records or plans and stay as written.

### 8.3 What the deletions cost nothing of

Every deleted site was never recorded (5.1), so deleting it removes a cost without losing data.
The exception is third-party `warn!`/`error!` (5.1), which blaze never chose to record either.

### 8.4 Tests that change

- **`blaze_trace::testing::capture(|| ..) -> Captured`** (feature `testing`, a dev-dependency)
  opens a root span, runs the closure, and collects every row under that root **across all
  rings** (a fiber may migrate, a job may run on a pool thread), decoded with names and fields.
  It reads each ring's `progress` counters before and after and **fails** if any ring skipped
  rows (`profile rows skipped` rose) or lost spans while it ran, naming the ring, so a test never
  passes on a partial capture.
- **devirt fixtures** (`crates/hir-ty-mir/src/mir/optimize/devirt/fixtures.rs`) capture the
  `Devirt` event kind (2.1) and print its fields in the order today's lines have; the
  `tracing::Subscriber` impl goes.
- **hir-def `naming_a_span_reads_nothing_into_its_query`** (`crates/hir-def/src/span_names/tests.rs`
  ~line 245) toggled a subscriber, which no longer exists. It compares the query's reads against
  a twin query identical but for the span naming, both in the test module, so the property it
  pins (naming a span adds no read) still has a removed cause to compare against
  (`test-the-cause`). The functions-named check uses `capture`.
- **hir-ty `tests/query_span_names.rs`** uses `capture`.
- **The four `ret` sites**, all `#[tracing::instrument(target = "blaze", .., ret)]`, recorded an
  info event with the return value on every call: `crates/hir-ty/src/lower.rs:3544`
  (`resolve_type_param_assoc_type_shorthand`), `lower.rs:4675` (`generic_predicates`),
  `crates/hir-ty/src/lower/path.rs:537` (`select_associated_type`),
  `crates/hir-ty/src/infer/opaques.rs:127` (`consider_opaque_type_use`). Each becomes
  `span!("<fn>")` plus `event!(info; ret = ?out)` before each return, so the value is still
  recorded.
- **`SOLVER_DEBUG`**: hir-ty's `setup_tracing()` (29 call sites) keeps its name and call sites and
  returns `Option<blaze_trace::testing::TreeGuard>`. With `SOLVER_DEBUG` set (a comma-separated
  list of target prefixes; empty means all) it starts `print_tree`, which on drop writes the rows
  captured under it to stderr as an indented tree like tracing-tree's, with fields. It needs the
  `trace` feature (`--features blaze-trace/trace`) to show trace sites, and says so on stderr
  when the feature is off.

## 9. Lanes

Each lane owns exactly the files listed; no two lanes in one phase share a file. Lanes write code
only; the integrator (the main agent) applies a phase's lanes to one tree, owns the shared files
below, and runs `blaze check` and `blaze test` once per phase (`fan-out-write-only`). Each lane
ends with a handoff: what changed, fields dropped (5.1), findings left.

**Integrator-owned in every phase**: root `Cargo.toml` (workspace deps, `[patch.'crates-io']`,
`exclude`, lint levels), `Cargo.lock`, `tools/nix/units/*.json`, `.github/blaze-version`. Lanes
hand their lines over in the handoff.

**Order**: pd-logs, pd-cost, pd-postmortem and pd-crashes land first (all edit `flight.rs`; phase
0's self CPU comes from pd-cost's switches). Then phase 0 as one batch. Then phases 1 and 2 as
one batch, so the tree never has unconverted sites with the `Recorder` gone. T lanes are
independent of blaze-trace and land on their own, any time.

| lane | phase | files | what |
|---|---|---|---|
| T1 `http1-client` | any | `crates/blaze-http/**` (new), `crates/blaze-registry/**` | 6.1: hyper, hyper-util, hyper-rustls, http-body-util and tokio-rustls out; `HyperHttp` on blaze-http's sans-IO core and tokio driver |
| T2 `vendor-third-party` | any | `vendor/quinn-proto/**`, `vendor/ra-ap-rustc_pattern_analysis/**`, `tools/vendor/**` | 6.2: patch series, `sync.py`, each crate's own tests under blaze |
| 0.1 `trace-core` | 0 | `crates/blaze-trace/**` (new), `crates/blaze-profile/**` | sections 1-3, 2.1 registry with `events` and `Devirt`, 3.2 ordering and its stress test, 4's read side and `child_env`, 8.1 coexistence, `testing::{capture, print_tree}`, the wasm32-wasip1 build; blaze-profile's own sites converted; files the TLSDESC issue |
| 0.2 `trace-graph` | 0 | `crates/blaze-graph/**` except `clippy.toml` | hooks become direct `blaze_trace` calls; `WaitKind` moves out |
| 0.3 `trace-bench` | 0 | `crates/blaze-bench/**`, `.github/workflows/bench.yaml`, `docs/benchmarks.md` | 3.4: instruction counts and budgets in `blaze_bench`, `benches/trace.rs` with ablations, the `trace` job on `bench-arm64`/`bench-x86` |
| 0.4 `trace-lints` | 0 | `crates/blaze-lints/**`, `crates/blaze-lint-defs/**`, `.clippy.toml`, the hot crates' `clippy.toml`, `.github/workflows/blaze-platform.yaml`, `docs/profiling.md` (the `unrecorded_span` lines only) | 5.2 rewriter and attribute-path resolution, 7.1 bans, 7.2 `disallowed_crates` and its CI step, 7.3 stop lists and `unrecorded_span` deletion |
| 1A `conv-cargo` | 1 | `crates/blaze-cargo/**` | 5.1 table (the most residue: `field::Empty`, `Span::none`, held, `in_scope`); 4's spawn sites, strips and the action-key exclusion |
| 1B `conv-hir-ty` | 1 | `crates/hir-ty/**` | debug/trace to `trace` sites, the four `ret` sites, `setup_tracing` to `print_tree`, `query_span_names` |
| 1C `conv-hir-ty-family` | 1 | `crates/hir-ty-mir/**`, `crates/hir-ty-codegen/**`, `crates/hir-ty-borrowck/**`, `crates/hir-ty-contracts/**`, `crates/hir-ty-specialization/**`, `crates/hir-ty-linear/**`, `crates/hir-ty-coherence/**`, `crates/hir-ty-verus/**`, `crates/rustc_abi/**` | debug/trace to `trace` sites; devirt to the `Devirt` kind and its fixtures |
| 1D `conv-front` | 1 | `crates/hir-def/**`, `crates/hir/**`, `crates/hir-expand/**`, `crates/base-db/**`, `crates/syntax/**`, `crates/parser/**`, `crates/cfg/**`, `crates/stdx/**`, `crates/blaze-query/**` | `*_query` spans, `never!`/`always!` re-export, tempfile events, `span_names` tests and the twin query |
| 1E `conv-blaze` | 1 | `crates/blaze-codegen/**`, `crates/blaze-gc/**`, `crates/blaze-resolve/**`, `crates/blaze-macro-eval/**`, `crates/blaze-link/**`, `crates/blaze-fetch/**`, `crates/blaze-cas/**`, `crates/blaze-fmt/**`, `crates/blaze-items/**`, `crates/blaze-lints/src/blaze/profile_tests.rs` and blaze-lints' other tracing fixtures | 5.1 table |
| 1F `conv-ix-prof` | 1 | `crates/ix-prof/**`, `crates/ix-prof-tracing/**` (new), `docs/ix-prof.md`, `docs/ix-prof-format.md` | 6.3 |
| 1G `conv-solver` | 1 | `vendor/rustc_next_trait_solver/**`, `vendor/rustc_type_ir/**`, `tools/v2/solver_sync.py` | `NNNN-blaze-trace` patches: sites to `trace` |
| 1H `conv-vendor-rest` | 1 | `vendor/rustc_transmute/**`, `vendor/charon/**` | `NNNN-no-tracing` and `blaze.diff`: deletion |
| 2A `delete-recorder` | 2 | `crates/blaze-profile/**` (with `schema.rs`'s docs), `crates/blaze-trace/**`, `crates/blaze-bench/benches/trace.rs` | 8.1 coexistence and 8.2's code deletions; `tracing_info_span` out of the bench |
| 2B `docs-config` | 2 | `.clippy.toml`, the hot crates' `clippy.toml`, `docs/profiling.md`, `docs/cargo.md`, `docs/v2/plan.md`, `tools/agents/rules.ts`, `AGENTS.md`, `.agents/skills/**`, `.claude/skills/**`, this doc | 7.1 entries out, docs true, rule 16, this doc to `current` |

Lanes 1A to 1H run `--fix` with the integrator's tree-built blaze (5.2). After phase 2 lands, the
integrator cuts the release and bumps `.github/blaze-version` (7.2).

## 10. Fit with work in flight

- **pd-logs**: its schema stays and is what `event!` writes: `events.event_id`,
  `EVENT_FIELDS_ROW`, typed field rows, `Hold`, `BLAZE_HISTORY_LEVEL`. Its visitor, target-based
  keep rule and level change go (8.2). Span field rows reuse its row format under a new row byte.
  Its `event_fields` table becomes `fields`, holding `event_id` or `span_id` (the other null),
  with `VERSION` bumped to 5: one table for one concept.
- **pd-cost**: this spec's budgets assume it. `Held` entries write enter and leave rows (new row
  byte) for its derivation of held CPU. `held_cpu`, `job_cpu` and `CPU_CLOCKS` go with it.
- **pd-postmortem**: `Store`, `Mapping` and `persist` move into blaze-trace; the recovery reader
  stays in blaze-profile. The new row bytes go into `ROWS_FINGERPRINT`. Site records come from
  `Site`, so `foreign_site` no longer needs tracing `Metadata`.

## 11. Risks and what to measure

1. **Per-op cost on macOS.** Mach-O TLS goes through `_tlv_get_addr`, a call. The owner check
   removes it from close; open and events still pay it. Measure `span_open_close_folded` on the
   macOS arm64 bench machine against Linux aarch64.
2. **Free lists and the kids split under fiber migration.** A hegel property test: random open,
   close, park and resume of fibers over N threads; every slot freed once, no id reused while
   open, kids sums exact, fold decisions equal to a single-threaded model. Plus the aarch64 stress
   test of 3.2.
3. **Formatting on hot spans.** `%`/`?` on a span opened per query formats and interns on every
   call. Non-generic span fields are dropped at the switch (5.1), so a self-build's cost is
   unchanged; the `ret` events cost what they cost today. Measure `span_shown_field_hit` and a
   cold self-build's per-query span time, before and after.
4. **The site table at 4095.** Expected at the switch: about 350 span sites, 130 event sites, the
   kinds, plus the hir-ty and solver `trace` sites when the feature is on (about 400). A test
   counts `Site` statics in a `trace` build and fails at 2048.
5. **Solver debugging.** Solver and inference authors keep their debug sites as `trace` sites and
   get the tree back with `SOLVER_DEBUG` and `--features blaze-trace/trace`.
6. **Vendor patch upkeep.** Each bump of quinn-proto or pattern_analysis replays its series
   with `tools/vendor/sync.py` and runs the crate's own tests. The lockfile ban catches a bump that
   brings tracing back.
7. **The `log` crate stays** (cranelift, wasmtime). Out of scope; the same ban can take it later.
8. **What to measure**, all on the exact tree (`measure-the-exact-tree`): the bench cases against
   their budgets; warm no-op and cold self-build wall time and spans per second, before and after;
   `profile spans lost` and `profile rows skipped` (they should not rise); blaze binary size and
   build time; the site count; `blaze check` time for `disallowed_crates`.
