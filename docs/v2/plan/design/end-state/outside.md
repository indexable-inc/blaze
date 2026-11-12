---
status: plan
summary: End state for build scripts, proc macros, the environment and outside files: one read form (ask, answer digest) over one Merkle tree kept by one watcher; runs and macro calls are records keyed before the work; no untraced class.
date: 2026-11-12
---
# Outside: build scripts, proc macros, the environment, files outside the workspace

Designer: lane `outside`, 2026-11-12, tree `/Volumes/Projects/blaze-int-1112` (read only). Status words: **measured** (given in the brief or a doc, with source), **read** (from code, file:line), **design** (not run), **memory** (a system cited from memory; its source was not opened in this session).

## 1. The job and the invariant

1. Run code blaze does not own (a build script and the programs it starts, a proc macro) so that everything it gets from outside its own code is known when it ends: files, listings, absences, links, variables, the host, earlier calls' state.
2. Keep each run as a record `(key, reads, value)` in the store and give the value again while every read answers the same.
3. Keep the answers current (a file system layer for every place a step read, inside or outside the checkout) so a check is a compare of ids, with no look at the disk on the request path.
4. Let the environment and the machine into a key only through a read some step made, by name.
5. Refuse loudly what cannot be known (no-fallback), naming the unit and the call.

**The invariant.** A kept value is given again only if a fresh run from the same key would get, for every question it asks of the outside, the answer the record holds; and every question a run can ask is either answered by blaze (so recorded by construction) or refused. There is no third class.

## 2. How the best systems do it

- **Buck2 dep files** (memory: `app/buck2_action_impl/src/actions/impls/run/dep_files.rs`) and **Build Systems à la Carte** (Mokhov, Mitchell, Peyton Jones, ICFP 2018; memory): an action has a key over what it may read and a second lookup over what it did read. That is blaze's `RunKey -> shape -> traced key` (`blaze-usercode/src/index.rs:1-13`, read). Take: the two-level lookup as the one record form. Do not take: declared inputs as the default; cargo build scripts declare nothing reliable.
- **Rattle** (Spall, Mitchell, Tobin-Hochstadt, "Build Scripts with Perfect Dependencies", OOPSLA 2020; memory): reads come from tracing, not declarations; a write to something another command read is a *hazard* with a typed outcome. Take: the hazard as a state (`Raced`), not a silent hash-at-end.
- **Nix** (memory: `libstore/build/local-derivation-goal.cc`): the environment is constructed, not inherited; inputs are immutable store paths; the network is allowed only to a step whose output hash is declared first. Take: constructed environment, immutable content-addressed roots for everything blaze fetched, network only for fetch. Do not take: the derivation grain (per package) or a cleared `PATH` (a C build needs the user's compiler; it is a traced read instead).
- **watchman / EdenFS** (memory: watchman's content-hash cache keyed by path, size, mtime): stat is an implementation detail of keeping a tree's content ids current, never part of a client's key. Take exactly that split.
- **Relax** (read): build scripts follow cargo's rule, the whole environment in the key and `rerun-if-changed` by newest mtime (`front/src/unitgraph.rs:942-960, 1061`); `include_str!`/`env!` are "late inputs" re-checked by content hash (`session/src/sip.rs:138-146`); proc macros are either compiled into the compiler and matched against embedded sources, or JIT-run in a "macro world" that "lives for the process", with no expansion cache (`compiler/src/pmhost.rs:1-20`, `native_macros/README.md`); C needs `--allow-c` and cargo. `KNOWN_GAPS.md` item 1: macros resolve by bare name across the graph. Take: existence probes as first-class inputs (`front/src/lazy.rs` `probes`). Do not take: mtimes, whole-environment keys, process-lived macro state, native macros matched by a hand list.
- **Bazel** (memory: `--incompatible_strict_action_env`, `--action_env`): the variables an action inherits are a declared allow-list. blaze's walk set is the same idea; the end state keeps it but as data in the key, one read per variable.

## 3. blaze today

Shape (read). A sandboxed run leaves `Traced { files, dirs, env, untraceable }` (`blaze-core/src/lib.rs:1029`); results are found through shapes (`index.rs`). Paths outside every view are hashed off the disk by `inputs::Files` and "vouched" by `blaze_vfs::outside::Dirs` generations. Path dependencies outside the checkout are read whole into memory-only mounts (`serve/mounts.rs`, `fetch/local.rs`). Proc macros are wasm modules run by `blaze-macro-eval/src/expander.rs` with a per-crate store verdict; their expanders are an engine input (`hir-expand/src/proc_macro.rs:261`).

Smells under rule 66, each with where:

- **Fast path over a slow default.** `Files::file` returns a vouched answer else looks at the disk (`inputs.rs:374-399`); listings the same (`inputs.rs:700-757`); "a directory the driver's root cannot name is never confirmed: each check of it looks at the disk" (`outside.rs:33-35`). Measured (brief): hello-world walked every run because of `blake3/c/dependencies/tbb`, ring's `crypto/**`, zstd-sys's `zstd/lib/*`; patched 2026-11-12 with three more caches and three planted-bug switches (`inputs.rs:141-177`).
- **Two mechanisms for one job, several times.** Two watchers: `Watcher` for views (`blaze-vfs/src/watch.rs`) and `Outside` for everything else (`outside.rs:1068`). Four caches of one fact in `Files` (`stamped`, `vouched`, `entries`, `listings`, `inputs.rs:138-156`). Three "find by reads" chains: `usercode/index.rs` slots, `serve/unit/reads.rs:141` `SLOTS`, `hir-def/src/deps/reads.rs:50`. Two keys for one script result: the traced key and the pure key, "kept a second time" (`pure.rs:1-5, 57`). A proc-macro crate planned as two units, with `ProcMacroExpander::replaces` to hide it (`proc_macro.rs:275-280`).
- **Special-case lists.** `stale_locals`' three cases: watched directory, lone file, absent path (`mounts.rs:154-173`); "One file, or a path that is not there, is read again by every job" (`fetch/local.rs:11-12`; measured: the root `Cargo.toml` of a path dependency's workspace was always stale). `WALK_NAMES`/`WALK_PREFIXES` (`confine.rs:56, 132`) and `read_unasked`'s `BLAZE_` prefix "with no trace" (`job.rs:1475-1488`). `pure::RUNTIME` loader paths per OS (`pure.rs:196-230`). Measured: setting `LIBCLANG_PATH` once invalidated the whole cache.
- **Stat in the answer.** `Stamp`, `racy`, `RACY_NS = 2 s` (`inputs.rs:55-119`); a canonical-path condition decides whether a listing is kept (`inputs.rs:748`).
- **A learned, process-wide verdict that changes old answers.** `Verdict::{Unseen, Stateless, Settles, Stateful}` per module in the store (`proc-macro-api/src/kept/verdict.rs:15`), `state_use` (`expander.rs:626`), a second run to prove a call (`same_from_either_state`, `expander.rs:1260`), sessions restarted each revision. Measured: the second run after a cold build re-gave seven crates' expanders (Unseen -> Settles) and re-ran 86 expansions; reth re-gave `ProcMacros` every build, 53 writes and 23 M verifications (patched in `ProcMacros::give`, with a planted-bug switch, `proc_macro.rs:243-297`); unit records unkept when a macro "asked the database" (`expander.rs:8, 1617`).
- **A fallback class.** `Untraceable::{Clock, Random, Network, Descriptor}` and `Seen::Incomplete` mean "not kept, keyed as cargo keys it" (`core/lib.rs:1037-1048`, `trace.rs:476-488`); a script outside the sandbox "runs every time, and nothing of it is kept" (`unit/script.rs:9-12, 984`). Measured: a no-op answer is never kept then; a `vergen` script reran on every edit (which read caused it: not measured).
- **Unsound today.** Reads are hashed "as it is when the run ends" (`trace.rs:7-8`, `confine.rs:6-8`), not as read; `certificate.md:193` says the same.
- **Unbounded or process-wide state.** `Files::shared()` and its global-heap maps (`inputs.rs:149-155, 342`); `reads.rs:57` `NAMED`; `CHANGES_KEPT = 4096` with "told no" past it (`outside.rs:72`); locals "never in the journal" so a restart reads them again (`mounts.rs:41-45`); a path dependency read whole up to 200,000 files (`local.rs:27`).
- **A per-place key.** Roots are ported, then a result is shared with another checkout only after a second checkout ran it (`roots.rs:33-36`). Measured: 80 registry crates of a hello-world refused stored images because their path is under the daemon's home.
- **I/O in logic.** `Files::look`, `Now::names`, `local::read`, `local::absent` call `std::fs` from the code that decides (`inputs.rs:430`, `local.rs:31, 52`).

## 4. The end state (design)

**One read form.** A read is `(Ask, Answer)`; `Answer` is a 32-byte digest, with one value for "nothing".

| `Ask` | Answer | Replaces |
|---|---|---|
| `Node(root, path)` | file: content id and mode; link: its target; directory: a fixed tag; absent: nothing | traced files, looks, absences, lone files, `include_str!` files |
| `List(root, path)` | digest of the sorted `(name, kind)` of direct entries | traced listings |
| `Var(name)` | hash of the value, or nothing | `E` lines, `env!`, the walk, `BLAZE_*` |
| `Spelling(role)` | hash of a root's absolute path | `Roots::mark` |
| `Span(question, position relative to the call)` | hash of what the database said | "asked the database" |
| `State(module, calling crate)` | digest of the macro's statics at the call's start | verdicts and sessions |
| `Step(record key)` | the nested step's value id | a macro or script that starts a program |

A path through a link is a `Node` read of the link, then reads along where it points; there is no "canonical or unvouched". A tool's version is not a kind: it is the `Node` reads of the program and what it loaded. What no file shows (OS build standing for the dyld shared cache, architecture, CPU feature set) is one `host` digest in every native step's **key**. The clock and randomness are answered from the key (`provider.rs`, exists) and are not reads. The network is refused, naming the address.

**One tree for every place.** `root` is a head of the one VFS Merkle tree: a checkout view, a registry package (keyed by its content, mounted at a place derived from that content, so its spelling is never a per-home fact), a path dependency, the toolchain, and `machine` (absolute paths under none of these). `machine` and path dependencies are **sparse** heads: only nodes some step asked for exist, with their absences; a directory node has two ids, `names` (for `List`) and `tree` (the coarse read). Sparse heads are journaled like any head, so a restart recovers them.

**One watcher.** `Watcher` gains sparse heads; `Outside` and `Dirs` go. Per watched directory: `Wanted -> Watching(since) -> Lost -> Wanted`; a place no watch can cover is `Polled`: re-examined at each request's barrier by the scan's own stamp cache. Stats and stamps live only inside the scan (as watchman keeps them); no caller sees a generation, a stamp or "vouched". The barrier is the one flush per request the daemon already makes. After it, `Places::id(root, path)` is a lock-free read of the tree.

**Records** (rule 51; in the store, written once):

- `StepRecord`: key = the script unit's own source key (never the program's bytes, so no compile is needed to look it up), the given variables from the plan with roots as placeholders, arguments, `host`, sandbox version, walk-set id. Reads = the table above. Value = printed directives (ported), `OUT_DIR` as a tree id whose files are store blobs (mounted into views; written to disk only for a consumer that needs a path), stderr, exit status.
- `ExpansionRecord`: key = module content id, export, kind, input and attribute tokens with call-relative spans (`Shapes`, exists). Reads = `Var`, `Node`, `List` (virtual paths under the package), `Span`, `State`, `Step`. Value = output tokens, diagnostics and a panic's message: a failed or warning expansion is a value too.
- Lookup is the one record form of `end-state.md`: key -> the read sets kept under it -> answers now -> value. Slots are bounded; past the bound is a typed refusal naming the key (rule 18).
- The certificate's coarse reads for this area are two ids, the `machine` head's `tree` id and the request's environment id (the environment interned as a flat directory of name -> value hash); fine reads are the unit and step records' own (answers open question 2 of `end-state.md`).

**A step's life** (sans-IO machine in `blaze-usercode`; the sandbox driver does the IO): `Keyed -> Found | Run(pin) -> Ingested -> Kept | Raced(place) | Refused(call)`. `Run` pins the roots' ids; reads are answered from the pin; after the run and a barrier, each read's node must equal the pinned one, else `Raced`: no record is written and the request continues from the new root (the edit is a new input; nothing is retried on the old one).

**Can a step be untraced? No.** A read set is `Traced` (a wasm macro: every import is a host function; a native script: the sandbox) or `Declared`. `Incomplete` and `Descriptor` become `Refused`, an error naming the script, the program and the call. The jobserver is answered by blaze like randomness. A script taken out of the sandbox by the documented opt-out must carry declared inputs (its package tree, its `rerun-if-*` lines, the given variables); its record is marked `Declared`, is reused under those reads, keeps the certificate, and the build says once that the unit is trusted on its word. This is where `end-state.md` level 1 is wrong for this area: "an untraced step keeps no certificate" keeps a third class alive.

**The environment.** A step's environment is constructed: the plan's given variables in the key; every other variable reachable only by name (`Var`, absent included); a walk of `environ` is one `Var` read per name of the walk set, which is one sorted value shipped with the toolchain, extendable in the workspace's config, its id in the key. blaze's own settings are parsed once at the client into the typed request; a step reads fields, never `BLAZE_*` by prefix. No request identity over the environment.

**Proc macros.** One unit per macro crate, wasm (rule 10); the host build exists only for the crate's own tests and never reaches the front end. The engine input `ProcMacros` is replaced by a fact a name resolution reads: `MacroDef(crate, name) -> (kind, module id, export)`, the value of the wasm unit's record. Compiled machine code and `InstancePre` are a cache by module id, bounded by footprint (rule 50), not an input. State is a read, not a verdict: blaze owns the guest's allocator, so a macro's state is its statics range; a call that starts from the image has `State = nothing`; a call's value includes `state_out`. A call that leaves state as it found it commutes with every other; calls that move it are chained in the calling crate's source order of invocations (the order rustc's expansion gives is the reference). Calls run speculatively from the predicted state (`Speculated(guess) -> Confirmed | Rerun at its place`, at most one rerun). A stateless crate never has a `State` read; a "settling" one has two read sets per key (from fresh, from settled) and hits on both; a counter is correct and sequential, as under rustc. Real impure macros: file and variable reads are traced reads; clock and randomness come from the key; a span question is a `Span` read re-asked without running the macro; a macro that starts a program gets a nested sandboxed step through one host import; network (sqlx online mode) is refused, naming the macro.

**Rules.** 3: no untraced class, `Raced` and `Refused` are typed ends, no retry. 9/10: unchanged, and now the reason every read is known. 11/50: tree nodes in the VFS store's columns, read sets in the asking task's frame, no process-wide maps. 14: the step machine, the watcher and lookup are sans-IO; `std::fs` only in drivers. 21: every `Ask` is in the reads or the key by construction; `host` covers what no file shows. 25: answers are content; read sets sorted; no mtime, generation or address in a record. 43: DST over the watcher and step machine (below). 51: both records and `OUT_DIR` trees are store records. 65: per step and per call, never per crate; `tree` and environment ids are first checks only.

## 5. Deleted, and what survives

Deleted: `inputs::Files` with `Stamp`, `Vouched`, `entries`, `listings` and the three planted switches; `blaze_vfs::outside::{Dirs, Outside, Cover}`, generations, `only_since`, `CHANGES_KEPT`, `looks`; `Mounts::locals`, `stale_locals`, `vouched_local`, `LocalRead.{watched, generation}`, `fetch::local::{absent, read}` as whole-directory reads, `MOST_FILES`; `read_unasked`; `Traced.untraceable`, `Untraceable`, `Seen::Incomplete` as a keepable state; `script::outside` as "runs every time"; the pure key (`pure::run_key`, `find`, `keep`, `retract`) and `RUNTIME`; `roots::verify` and sharing by second run; `Verdict`, `Seen`, `Left`, `Session*`, `state_use`, `same_from_either_state`, `UNSETTLED`, `UNPROVEN`; `ProcMacros::give`, `note_proc_macros`, `replaces`, the `MacroWasm`/host pair in the front end; `unit/reads.rs` slots and `usercode/index.rs` chains as separate forms; the home-path refusal.

Survives: the platform sandboxes and the launcher; the trace line format (minus `T`, `X`); `Roots::port`/`unport`; `provider.rs`; the wasm host, wire format and `Shapes`; `hir_ty_pure` as a checker (a proof the trace contradicts is still an error) and as the licence to drop a `Spelling` read; the VFS tree, journal and `Watcher`.

## 6. Order of building

| Step | What | Judged by (measured how) |
|---|---|---|
| 1 | `Ask`/`Answer`; sparse `machine` and path-dependency heads in the VFS; one watcher; `Files`, `Dirs`, `locals` deleted | hello-world repeated `blaze run`: runs 2 to 5 all 0 jobs and 0 disk looks, each at or under 50 ms (today 160-700 ms before the fifth, measured); the first no-op after a restart hashes 0 files (profile spans) |
| 2 | Environment: per-variable reads, walk set as data, typed request settings | reth: set `LIBCLANG_PATH`, build: units remade = the scripts that read it and nothing whose inputs are equal (today: whole cache, measured); `TERM`/`COLUMNS` changes: 0 |
| 3 | Proc macros: `MacroDef` fact, state as a read, diagnostics and spans kept | second run after a cold hello-world: 0 expansions (today 86, measured); reth no-op: 0 writes of macro inputs, 0 rows verified from them (was 53 and 23 M); 0 unit records unkept for a span question |
| 4 | Step machine: source-keyed, pinned reads, `Raced`, `OUT_DIR` as a tree, `Declared`, refusals | reth: edit a file `vergen` did not read: 0 scripts run; edit one it read: 1 script, 0 dependents when its value is equal; count of untraced units = 0 (profile) |
| 5 | Registry and toolchain roots at content-derived places; `roots::verify` deleted | hello-world: 0 of 80 registry crates refused; a second checkout's cold build runs 0 registry scripts |

## 7. Unsound, unknown, to measure first

1. Why did the no-op need five runs? Nothing read here explains it. Ask the profile of runs 1 to 5 (`blaze explain`).
2. How many units of reth and tempo are untraced today, and by which cause (clock, descriptor, incomplete, opt-out)? A profile count; it sizes step 4's refusals.
3. Which read makes `vergen` rerun? If it asks whether the tree is dirty it truly reads every file; then the win is only that its equal value stops there. Whether a script's equal value already stops its dependents today was not checked.
4. `stat` shows mtime and inode, which are not content. Which scripts or tools they start decide on them (`make`, git)? Count stat-only looks in reth's traces; decide whether the Linux sandbox normalizes mtimes; on macOS files are read in place and it cannot.
5. A child that reads the clock without a system call (vDSO, commpage) or CPUID is not seen. Is `host` in the key plus a fixed `SOURCE_DATE_EPOCH` enough? List it as trusted either way.
6. State as statics range: needs the guest allocator's state outside that range. How many distinct start states per key appear on reth and tempo, and how many crates move state? Count with today's `Left` before deleting it.
7. Source order for state-moving calls conflicts with lazy body expansion; is there a real crate that needs it? Find state-moving macros in reth's and tempo's closures first.
8. How many macros in those closures start a process or open a socket (wasip1 has neither)? This decides whether the nested-step import is step 3 or later.
9. How often does `roots::verify` share a native script's result across checkouts today? If often on macOS, `Spelling` reads cost cold builds there.
10. The barrier's cost per request with a few hundred sparse directories (FSEvents flush, inotify drain): measure before promising single-digit ms.
11. The `Raced` window on macOS (files read in place): the DST must show that no record is written for a mixed state.

## 8. How to cut it

One mind: the `Ask`/`Answer` type and the record lookup form (shared with the owners of levels 1 and 2), and the macro state rule (chain order, speculation). Everything else follows from those two pages.

Pieces, each whole in a few files:

- A. `blaze-vfs`: sparse heads, two directory ids, `Watcher` over them; delete `outside.rs`. Files: `watch.rs`, `tree.rs`, `outside.rs`, their tests. DST: crashes, lost events, a link re-pointed, a planted "stale node survives a barrier" bug.
- B. `blaze-usercode/src/inputs.rs`, `trace.rs`: `Inputs` as a thin reader of `Places`; assemble a trace into reads against a pin.
- C. `blaze-cargo/src/serve/{mounts.rs, fetch/local.rs}` and the three functions in `job.rs`: path dependencies and lone files as sparse heads.
- D. `confine.rs`, `unit/script.rs` environment half, the client's request type: walk set as data, typed settings.
- E. `blaze-macro-eval/src/{expander.rs, guest.rs, wasm.rs}`: state as a read, fresh image per call, allocator outside statics; delete `proc-macro-api/src/kept/verdict.rs`.
- F. `hir-expand/src/proc_macro.rs` and its readers: `MacroDef` fact; one unit per macro crate in `blaze-units/src/graph.rs`.
- G. `blaze-usercode/src/{run.rs, script.rs, pure.rs, roots.rs}`, `unit/script.rs`: the step machine, source key, `OUT_DIR` tree, `Declared`, `Raced`.
- H. `unit/reads.rs`, `usercode/index.rs`, `hir-def/src/deps/reads.rs`: one lookup form; depends on the one-mind page.

A, D, E and F do not touch each other and can run at once; B and C follow A; G follows B; H last.
