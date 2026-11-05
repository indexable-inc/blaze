---
status: plan
summary: blaze v2: the store.
date: 2026-11-01
---
# blaze v2: store

Part of the [v2 plan](../../plan.md). Section numbers refer to the plan as first written.

## 8. Store

- One content-addressed store on the VM's disk, keyed by BLAKE3 of exact inputs (source,
  cfg, features, target, toolchain, the layout facts read). A key that misses an input is a bug.
- **Packs**: aligned zero-copy records, mmapped. Hot objects uncompressed; cold objects zstd
  with a dictionary per kind. Large outputs content-defined-chunked, so near-identical binaries
  across views share bytes. Outputs placed with reflink (Linux) or `clonefile` (macOS).
- **Index**: an mmapped open-addressed table on BLAKE3 keys; one or two cache misses a lookup.
- **Verified**: a read checks the content hash; archive and path entries can't escape their
  directory.
- **The user's alone**: the store refuses a directory of another user or one others may write
  to, and reads no pack or file others may write to, as v1's did: a value is checked against its
  own hash, never against what its key says made it, and the store keeps machine code that is
  run (`Kind::Machine`). `Store::open` asks `blaze_core::Effect::Private` of its directory
  before reading anything in it. That effect makes the directory 0o700 if it is missing, refuses
  it if another user owns it or others may write it, closes it to the group and others (Unix),
  then refuses any file under it, links followed, that is another user's or others may write.
  A file's group write bit (a 002 umask) is refused only while the directory lets its group in.
  On Windows it reads the owner and the access list, as v1's `acl.rs` did. A layer is read-only
  and may be shared, so it is not checked; nothing outside tests configures one yet.
- **Remote** is a read-through tier for machines outside the pool (a laptop) only, never on the
  hot path of the VM. Supersedes the per-job, per-pin remote naming (#1481).
- **Compute steps from MIR down** keep their values as [kernel](kernel.md) entries: ordinary
  values under their identity, their step's code and the hashes of the narrow reads they made,
  with a bounded chain of shapes per identity. Codegen's dev templates are the first.

## Portable keys

2026-11-01. Build-script runs, pure build scripts and proc-macro expansions are kept under keys
that don't say where a checkout is, so two checkouts, or two workspaces with one dependency,
find each other's results (`crates/blaze-usercode/src/roots.rs`).

- **Roots.** A run names the directories its paths are under by role: its package (`\0p`),
  its `OUT_DIR` (`\0o`), the target directory (`\0t`), the workspace (`\0w`), blaze's tools
  (`\0b`) and its launcher (`\0l`). A build script has all six (`unit::script::run_roots`);
  an expansion has its calling crate's `CARGO_MANIFEST_DIR` and `OUT_DIR`
  (`proc_macro_api::kept::roots`). A root must be absolute, UTF-8 and not `/`. On Windows
  nothing is a root (paths there are UTF-16 units), so nothing is shared between checkouts.
- **Keys.** Every path a key, a trace or a shape holds is written against the nearest root it
  is under by whole names (`/ws/p/build.rs` is `\0p/build.rs`). In a variable's value and in
  what a run printed, a root that ends a name and starts a path (at the string's start or
  after a byte no path has: a space, `=`, `:`, a quote) becomes its placeholder, and each `\0`
  is doubled, so the port is reversible. A root with any path byte before it (`/src` in
  `/usr/src`, `vendor/src/lib` or `-I/src`) stays as it is, and ties the run to its checkout
  (below). The key is given the roots' layout (`Roots::layout`): each role, and which other
  root it is under and where (of two equal roots, a package that is its workspace, only the
  lower role is under the other), so checkouts whose roots sit otherwise to each other (a
  target directory inside the workspace in one, elsewhere in the other) don't share a key. The
  script binary is keyed by its content ID; its path, which it sees as `argv[0]`, is ported.
- **Checks.** A kept shape is checked through `roots::Rooted`: placeholders become this run's
  roots, values are ported as the kept ones were. What a run printed is written back with this
  run's roots when it is reused (`Roots::restored`).
- **Marks** (`Roots::kept`). Every run is kept with a mark per root: a file entry
  `\0<tag>/\0root` whose hash is the root's path, so it is found only where every root is
  where it was. A mark is a trace entry, so it covers what the key holds (a given string with a
  root left in it) as well as what the run printed and wrote.
- **Verified before shared** (`roots::verify`). A run is given to another checkout only once
  a second checkout ran it and got the same. Its first checkout keeps, beside the marked value,
  an unverified copy: under a run key of its own (`verify::copies` of the run and its unmarked
  trace), one entry per checkout, named by its origin root by root (`verify::Origin`: each
  role's root hashed as its mark is). A copy is a small record naming the key of the
  checkout's own marked value, never the value again, so a copy no one verifies costs a few
  dozen bytes. Lookups never read copies. A checkout that misses runs anyway, and as it keeps
  the result it reads the copies under the same unmarked trace, which is the same key only
  when it read the same things, and compares each other checkout's ported value with its own
  (`run::differs`, the comparison `Mode::Verify` makes, plus stderr; a macro's output byte for
  byte):
  - equal to every one: the value is put under the run key with the unmarked trace, verified,
    once for each pair compared's set of differing roots (`Origin::differing`), with a mark for
    every root that was the same in that pair. Only a root seen to differ, with the value the
    same, goes unmarked, and only together with the roots that differed in the same pair:
    checkouts that differ in their target directory alone verify nothing about their tools or
    package, and two pairs, one differing in the target directory and one in the tools, say
    nothing of a checkout where both differ. The value is
    kept with the key of its unmarked result before it (`index::verified_value`), which
    `index::find` asks the record below of;
  - any differs: a record that the run depends on where its roots are is put
    (`index::depends`), and from then on every checkout keeps its own, marked; `index::find`
    asks for that record on every verified value it finds (a value kept here, marked, costs no
    second read), so a value verified before is not given again.
  A checkout that reruns a verified value and gets otherwise (`BLAZE_SCRIPT_CACHE=check` for a
  build script, its pure key's hit among it, `BLAZE_PROC_MACRO_CACHE=check` for an expansion)
  puts the same record (`verify::retraction`), keeps its own value here and fails with the
  difference; the rerun compares stderr too, as sharing does. Every such record, and the one a
  differing copy puts, is a durable put (`verify::Put::durable`, the expander's `Keep::Durable`):
  the job is not reported done until it is durable (`Job::durable`, the store flushed for it),
  so a crash can't forget it, and a store that broke fails the job naming why rather than
  waiting. A durable put goes to its daemon's own queue (`Keeping::put`), never to a worker's
  buffer nor to the process-wide queue any task's take drains (a task of another job could
  take it there and hold it past the maker's end); only the store's owner takes it
  (`Keeping::publish`, from `Jobs::kept` and `plane_finished`), and every job not ended then
  waits for it (`Jobs::wait_durable`). It is put before the maker's task ends, so before the
  maker's `plane_finished`, whose publish takes the queue, and a job ends only after that: the
  job that made it is never reported done first. Being the daemon's, another daemon in the
  process (the simulation) never publishes it into its own store. A check run that differs retracts both keys it
  was verified under: its sandboxed key in `Run::complete`, and its pure key in the build
  script's error arm (`RunError::Differs` hands back what the rerun gave), which keeps the
  rerun's value under the pure key here. An expansion found verified reports its reads with
  whether that record is in the store (`proc_macro_api::kept::verified_reads`), so what was
  filed by it is made again once it is retracted. When that is asked: a unit's record is
  looked up by its reads on every build (`serve::unit::compile::fresh` through `reads::find`,
  which works each expansion's values out again), so the next build after a retraction misses
  it; a fresh database is served crate images (`hir_def::deps::serve`) by those reads too. A
  warm daemon's front end keeps what it was given (a tracked expansion, a served image)
  without asking the store again. The store of expansions has one writer, its daemon, so every
  record is one this daemon publishes: each publish of one moves an epoch once the record is
  readable in the store (`kept::Retractions`, also moved for a build script's record, after
  its put), every lowering reads it before it reads the store (`Keeping::retractions_now`,
  one atomic load), and a kept front end is lowered into only at the epoch it was made at,
  like its units and config: after a retraction the next build lowers afresh and looks the
  expansion up again. So a front end stamped `e` has seen every record whose bump is at most
  `e`; bumping before the record was readable would let a lowering stamped with the new
  epoch be given the retracted value. No remote layer feeds this store (it is opened with
  none, and nothing fetches into it); one that did would have to bump for each record. Whether that front end was given the
  retracted value is not asked; records are rare, and nothing grows. A store shared by two
  writers would need each to see the other's records; none is.
- **A dependency's reads are its dependents'** (rule 18). A crate's image holds what its
  dependencies' expansions made (a name imported from a derive's output, an item's location
  in one), and its key covers only its dependencies' keys, which cover what each was given,
  not what its expansions read. So a crate's reads (`hir_def::deps::reads::Reads::deps`) hold
  each dependency's expansions' names, by its index in the crate's closure: a served
  dependency's as serving it noted (`Files::expansions_read_by`), a resolved one's as its def
  map ran them; their values are worked out with that dependency's variables. Once a
  dependency's expansion reads another value (a file it read changed, a verified value
  retracted), the dependent's image is not served either. A dependency with an expansion
  whose reads are not known refuses its dependents an image too. The check caches
  (`checked_reads`, `outside_reads`) still use a crate's own reads; that is open.
- **Not in the global cache.** A value verified across checkouts is never public: its
  record would be in one machine's store alone, so another machine would be given it after it
  was retracted. The image rule refuses a crate filed by one (`ImageRead::Verified`), and the
  cache's client refuses one it is sent before the store keeps it
  (`blaze_global::public::fetchable` as the `admit` every `Store::fetch` names), so the fetch is an
  error naming its key and nothing reads the value. A value this machine verified is never
  fetched (the store has it), so it is never refused. Today nothing in the tree publishes expansions or images (blaze's
  builders do); a driver that does maps a verified shape to `ImageRead::Verified`. The copies, the verified values and the records are all
  ordinary store entries, so they are kept, read and restored as the store is (a test flushes,
  drops and reopens a real store between the steps of the exchange). Same-checkout hits find
  the marked value as before. Every chain is walked to a bound (`index::MOST_SHAPES`,
  `verify::MOST_COPIES`) that ends in a typed error or verdict.
- The job's own variables that steer these checks (`BLAZE_SCRIPT_CACHE`, `BLAZE_PURE`,
  `BLAZE_ELIDE`, `BLAZE_PROC_MACRO_CACHE`) are never in a script's environment, so a script that
  reads its whole environment is keyed the same with and without them.
- **Kept here alone** (no copy is made) when something ties the run to its roots' strings
  already: a root left after porting, in any spelling (its links resolved, `/private` on macOS,
  separators doubled, turned or escaped, any case, `%`-escaped, UTF-16), in what it printed, in
  a variable it read (`CFLAGS=-I/ws/p/include`: another checkout's run would read this one's
  files), or in a string its key was given (`RootMap::bind`, `proc_macro_api::kept::left`); a
  printed relative path between roots that the layout doesn't fix (roots of two tops); a read
  outside its roots and the machine-wide places (`roots::MACHINE`, the cargo home's registry and
  git checkouts, except one that holds the run's package, `OUT_DIR`, target directory or
  workspace); or a root whose links resolve elsewhere at keep time than when its map was made
  (`Roots::relinked`; an expansion's memoized map is then made again, and a build script's is
  compared with the map its run was prepared with); or a root in a file the run wrote (its name
  or bytes, `Roots::stored`): comparing two checkouts can't see a root they share (the tools, a
  target directory both use), and this scan does. It reads each file once for every spelling
  at once (one Aho-Corasick automaton over the bytes with separator runs folded, one for
  UTF-16) and stops at the first root found, since one is enough to keep the run here alone.
  It runs where the run's files are collected, on the store's loop; moving both off it is
  open.
- **Soundness (rule 18).** Every file, listing and variable a run read is still in its key by
  its value, and every path it read is under a root (ported, resolved per checkout), in a
  machine-wide place (the same file in every checkout), or the run is kept here alone. A root's
  string is dropped from the key only once a second checkout whose root of that role is
  another string read the same inputs and gave the same ported value; every root that was the
  same string in the checkouts compared stays in the key as a mark, and a value that holds a
  root in any spelling, printed or written, is never shared at all. Before round 5 of this lane
  the origin hashed every root at once, so two checkouts differing in one root dropped every
  root from the key: a file holding the tools' path, the same in both, was served to a third
  checkout with its tools elsewhere. That equal-root gap is closed (per-root origin, the marks
  above, and the written-file scan). What remains is a dependence on a part of a root that
  differed but agreed in that part in the checkouts that verified it: the same package
  directory name, the same parent, the same depth, or a root's string transformed (hashed)
  where those parts agree. A third checkout whose root differs in that part is served the
  verified value without running; if it reruns it (`BLAZE_SCRIPT_CACHE=check`,
  `BLAZE_PROC_MACRO_CACHE=check`, or a miss elsewhere) and differs, the record retracts the
  value for every later lookup, durably before that job is done (its sandboxed and pure keys
  both), and for a warm daemon's kept front ends from its next build on; a lookup served before
  that was served wrong, as was a build already running when the record was put. That is the
  one gap, and it is named here, not accepted silently.
- **Next step: virtual roots for wasm macros** (a lane of its own, queued). A wasm macro sees
  only what the WASI host gives it, so its `CARGO_MANIFEST_DIR` and `OUT_DIR` could be fixed
  virtual paths mapped to the real ones. Its output could then not depend on any part of a
  root's string, closing the gap above for expansions without a verify. Not built.
- **Still checkout-local:** path-dependency macro crates and build scripts whose code ID holds
  their source paths; an `OUT_DIR` whose unit hash depends on the checkout; every run kept
  here alone (above), and every run until a second checkout verifies it, including one whose
  written files differ between checkouts (a C object whose debug info names its directory) or
  hold a root (a generated file holding its `OUT_DIR`); expansions of a workspace member, whose directory is
  the workspace root, which is not one of an expansion's roots; test runs
  (`blaze_usercode::test`); everything on Windows.
- **Not built yet: the machine store.** A store at `<machine>/shared/store`, opened with
  `Store::open_shared` like std's, read after a home's own store and written by values whose
  key has no workspace-local root (registry and git dependencies), so homes on one machine
  share them. Portable keys are what makes it useful. It needs the daemon to open it, route its
  completions, flush it and lend it to jobs (`JobInput`) and to `Expansions`, and puts to say
  which store they go to.

## Issues this lane takes in

- **#1439**: the store keys every value by a hash of all of blaze's sources, so any release
  turns CI cold. v2 keys a value by the schema and code of what produced it (section 5,
  Schema). (`v2-store`)
- **#686**: every process indexes the whole store (1.6-1.8 GB each, OOM at 117 GB). In v2 only
  the daemon holds the store, and its index is the mmapped table. (`v2-store`)
- **#1480**: two store inputs not hashed (unreadable sources skipped, the C compiler of build
  scripts). Traced inputs hash both. (`v2-usercode`, `v2-store`)
- **#1298 store as one mapped file per crate version**: append payloads, write the index into
  the inactive of two slots, flip the header; one barrier, not a sync per entry (anim cold
  wrote 77,481 entries serially). (`v2-store`)

## Done when

- one mapped file per crate version with the index flip; the mmapped BLAKE3 index
- hot, warm and cold tiers with background moves; every read verified; a store others can
  write to refused
- keys by schema and producing code; the remote as an off-path tier
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
