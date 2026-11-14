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

## Content keys

2026-11-12, lane content-keys-1112. The rule every record key follows, what is built of it, and
what is not. Code: `crates/blaze-cargo/src/serve/unit/portable.rs`.

**The rule.** A record's key is the hash of three things and nothing else:

1. *The action*: which stage made it (front end, code generator, record maker, linker), by that
   stage's code identity.
2. *The content of each input*: a source by its content hash under its logical name (a
   workspace file by its path from the workspace's root, a std file under
   `/rustc/<commit>/library`, an `OUT_DIR` file by its path from the build directory); cfg,
   features, target, profile, the goal; flags as text. Never an absolute path of a checkout, a
   home or a machine directory.
3. *For each dependency, the digest of what this stage reads of it*, not the dependency's key.

**Paths.** The build names its directories by what they are (`Places`: workspace, build
directory, shared std directory, tools, launcher; the roots of "Portable keys" above, one
mechanism). A key holds each as its placeholder plus how they sit to each other
(`Roots::layout`); the real path is bound at use: a record's strings are written against the
making build's directories and read back against the finding build's. This is what rustc's
`--remap-path-prefix` does to the *output* only (cargo still keys on the absolute path, so
sccache needs identical paths), and what Bazel and Buck2 get by running actions in an exec root
with relative paths. blaze does the Buck2 thing for keys and the remap thing for values.

**Where a crate really embeds a path, the key covers it; this is read, never guessed.** After a
record and each output are written against the directories, their bytes are searched for every
directory in any spelling (`Roots::embedded`). One that still holds a directory (a constant
from `env!("CARGO_MANIFEST_DIR")`, a diagnostic quoting an absolute name, a search path linked
in) is filed under its key *here* (the key and each directory's real path), which only a build
with the same directories asks. A dependence that leaves no bytes (a path's length) can only
come from a crate's own path-valued variable read by `env!` (the `env!` log names it) or given
to a proc macro (no trace names it): a closure with either is filed here too.

**Interface, per stage** (what item 3 means):

| Reader | Reads of a dependency | Digest today |
|---|---|---|
| a dependent's check | signatures, types, generic, `const`, `#[inline]` and `impl Trait` bodies, positions | `Narrow::interface` (interface text; a plain body is its shape) |
| a dependent library's codegen | at `opt-level` 0 what its front end reads and `#[inline(always)]` bodies; optimized, plus what it inlines | `Narrow::built` (the surfaces) at `opt-level` 0, "Early cutoff between crates" below; `Narrow::code` (every body) optimized |
| a link | every function's code record | each `Code` record's key; the unit key holds `Narrow::code` |
| a build script's or macro's user | what the run printed and wrote, by value | `blaze_usercode` shapes (already by output) |

So early cutoff holds for check-only units, for runs and (2026-11-12, lane cutoff-1112) for a
library's build at `opt-level` 0, whose record no longer lists what its walk finds in other
crates.

**Determinism.** A portable key promises that two builds of one key make the same bytes. Tested
by building in two directories under two homes and asking for zero misses with the first's store
(`crates/blaze-engine/tests/revisions`), and in process
(`serve/unit/tests/portable.rs`, with the path planted back as the control). `BLAZE_VERIFY_STORE`
should also rebuild a found record and compare; not built.

**Not built (each is a key that still says where something is, so a miss, never a wrong hit):**
fetched packages (`<home>/sources`; their `file!()` names are real paths until named
`<name>-<version>/..`); an image patched in place (`Written::Derived`, not read again, so filed
here); doctest image records and `code_identity` (`own_code`); the daemon image; a crate that
calls any proc macro (until macros are given virtual paths); Windows (no roots there).

**The reuse mechanisms, and which collapse.** Counted from the code:

| Mechanism | Key | Under this rule |
|---|---|---|
| unit record (whole key) | identity + plan + makers | the record, keyed as above |
| unit record (narrow, quoted keys) | the same over code text | the same record under a narrower input digest: stays, as item 2's "content" of a source |
| check entry | the same over `checked` | collapses into the unit record once a unit is two actions (check, build) each with its own record |
| library narrow (`library_key`) | crate settings + files | an input digest's memo: stays |
| build-script run, pure script, expansion | `RunKey` against roots | already this rule (by content, verified before shared) |
| code templates | code identity | already this rule |
| dependency image, check outcome | `image_key` with path markers | already this rule; its `Base` markers and `Places` are two homes for one concept, to merge into roots |
| doctest image record | unit part + `code_identity` | collapses into unit records once a doctest is a unit |
| no-op lane's answer | request + view, in memory | not a store record: stays |
| daemon image | the database, by path | not content-keyed: stays, one checkout's |

## Describe records

2026-11-12, lane describes-1112. Written, not yet compiled or measured: every number here that
is not quoted from another lane is a prediction, and the checks are listed at the end. Code:
`crates/blaze-codegen/src/driver/records.rs` (the walk's side),
`crates/hir-ty-codegen/v2/frontend/described.rs` (the key and the record),
`crates/hir-ty/src/check_cache/port.rs` (types and ids by content).

**What a describe is.** The code generator's walk asks the front end one question of each
function instance it reaches (`Frontend::describe`, `HirFrontend::described`): what it is
without compiling it. The answer (`blaze_codegen::backend::Described`) is its item's key, its
code hashes (`code`, `template_code`, `built_in_code`), the digest of its arguments' facts, whose
code it is (`origin`, `owner`), a few flags, its symbol and debug name, and its references in
the order of its MIR, which are the places of its template and what the walk follows. From it
the walk makes the function's instance key and its template's store key, so a template lookup
needs the describe first.

**What it reads.** The instance's MIR as its origin's units build it (for a body of the
library: level 2, inlined, with the effect summaries of what it inlines), the signatures and
layouts its arguments name, the impls its calls resolve to, and of the build: how each origin's
units build bodies (`Units`: inlines, tells locals, names, the MIR options), whether the build
links an image, whether it is build-time code, and for a body walked against them a program's
dyn facts. For a body that calls `file!()`, what the build calls the file.

**What was local.** The describe was `described_memo(db, HirInstance, Origin, Facing)`: a memo
of one database, keyed by interned ids (a `FunctionId`, the row of its `GenericArgs`, a `Crate`
in its environment), with its units named by their place in a list of the process
(`Facing::units`, `UNITS`), and its value naming each callee as another `HirInstance`. The walk
kept over it (`driver::Kept`) is by database nonce. So a new project's database, a restarted
daemon and a second home had none: 14,233 describes for hello-world, 44.5 s of CPU (lane
launch3-1111's profile), all of the standard library's.

**The content key.** The rule of "Content keys" above, for one function:

| part of the rule | here |
|---|---|
| the action | the code generator's code identity (`Toolchain::id`) as the key's producer, and the record's schema (its layout and the type codec's) |
| the inputs' content | the toolchain whole (target, CPU, features); `Units` by content, never its place in `UNITS`; the image and build-time flags; what the units give *whoever asks* (the requester by what it is compiled as, never its package's name, so a second project of another name makes the same key); the instance: its body by its item's key (`blaze_items::item_key`), a closure by its owner and its place there, its arguments as types written by content |
| each dependency by what is read of it | the image key (`hir_def::deps::Image::key`) of the crate the instance is worked out in, its home (`hir_ty_mir::mir::home`): that key is the content of the crate and, by its dependencies' keys, of every crate it loads |

One image key is enough because of what a home is: `home.rs` shows that an instance worked out
in its home reads only crates among the home's dependencies. That is checked where the key is
made, never assumed: every item and crate the instance names must be the home's or one of its
dependencies' (`described::Closure`), or it has no key. This is coarser than
`fn_info(&f).code_id` (an edit anywhere in a crate rekeys every function of it) and is what can
be made *before* the describe: the function's own code hash needs its MIR (`hir_ty_share::code`
reads the spans of the lowered body), which is a third of what a describe costs. A crate read
from an image is one no edit reaches, so the coarseness costs nothing there; the function's
own code identity is the key to move to once it can be had without the body (a sealed crate's
per-function identities as a record of the crate: lane sealed-1112).

**Which describes have a key.** An instance whose home is read from an image, in the empty
environment, with a policy, by the item lane's hashes, walked against no program's dyn facts:
the standard library's own functions, a dependency's, and their generics over each other's
types. Not: a workspace crate's function or a generic over its types (the database's memo, as
before: an edit changes it), a body that names a file (described each build, and never written
as a record, so its key is always a miss), a release build's bodies walked against the
program's dyn facts, the C `main`, the allocator shim, an `asm!` function, statics.

**The record.** `Kind::Query`, under `Key::of(schema, producer, [inputs])`: the `Described` with
its origin as "the requester's" or its crate's own (the one rule, `compiled_for`), and each
reference by content: a function as its body, arguments and home, a static as its item, an
outside symbol by name. Types and ids go through the one codec a dependency's checked bodies
use (`hir_ty::check_cache::ids`, `next_solver::persist`), with each item named by its item key
in place of an index in one crate's table (`check_cache::port`). With each function it refers
to, the record holds what the recursion limit reads of it (`Counted`): the walk asks that of a
callee before describing it, and working it out runs `compiled_by_user`, which builds the
callee's optimized body (2.7 s of the profile above). No database id is in the bytes.

`Described::abi` (2026-11-12, for the launcher lane; launcher.md, "More than one changed body,
and a changed signature") is a fact of the describe like any other, so it is a field of the
record and nothing beside it: what a caller's code assumes of the function beyond its address
(its convention, whether it is variadic or takes its caller's location, and the identity of
each argument's and the result's type, which holds every layout the signature names behind
references too, with the interface of each trait a `dyn` in it names;
`hir-ty-codegen/v2/frontend/abi.rs`). The walk carries it to `Built::abis`, by key, beside
`identities`.

**The walk.** `Build::recording(store)`: for a function no kept walk holds, the driver asks the
front end for the key, the store for the record (on whichever core visits it, under its own
guard), and the front end to read it; only a miss is described, and the record of each describe
made is among the build's puts. `Walk::found` counts the records read beside `described` and
`verified` (the `codegen walk` span's `found`). A record that is there and does not read is the
build's error naming the function and the key (rule no-fallback). Two describes of one key
with other bytes refuse the build (rule 21). The frontier form (`Build::near`,
launcher.md "The lazy path") goes through the same `Build::described`, so a launched
program's first call of a library function reads records too: that is where the lazy path
becomes cheap, with no change to it.

**What stands in for the memo afterwards.** A describe read from a record was never run in
this database, and three later asks would run it after all. Each is answered from what the
database read (`described::Found`, by database, dropped at a write of a library's input):
whether a kept describe still holds (`redescribe`: yes while the home's image key and what the
build gives the requester are what they were when it was read), whether it is asked each build
(no: a describe that names a file is never a record), and a callee's count.

**Determinism.** The bytes hold item keys, crate keys, content IDs, text and a type table whose
order is the order of the walk over the describe. One known hole, stated in `port.rs`: a `dyn`
type's projections and auto traits are written in the writing database's id order, so two
databases that numbered two auto traits the other way round make other bytes and other keys
for an instance naming such a type: a miss (and two records of one describe), never a wrong
hit. To close: write them sorted by item key in `next_solver::persist`.

**Not built.** Statics' describes as records (each is a constant evaluation; hello-world has a
few hundred); the function's own code identity as the key; a verify mode that describes a found
record again and compares (`BLAZE_VERIFY_STORE` for describes); the `dyn` order above; a bound
on `Found` other than its database's life.

**Checks (none run yet).** `blaze-codegen` `driver::tests::recorded`: a second database
describes only what has no record and builds what the fresh walk builds; the same walk with no
store describes everything (the cause removed); a key that leaves the body out is caught by
the same oracle (the planted bug); a record that does not read refuses the build; two records
of one key. Still to write once the tree compiles: the record's round trip and "two databases,
identical bytes" over a real database with a dependency read from an image (the fixture of
`hir-ty`'s `check_cache::tests`, whose second database numbers its ids differently), and the
five measurements of the lane's brief (second project, restart, second home, wall time, the
edit loop).

## What a step reads

2026-11-12, lane envkeys-1112 (decision D21). "Content keys" says how an input is written into
a key; this says which inputs a key may hold at all: what the step that made the value read,
and nothing else. An input nothing read is not an input (rule keys-cover-every-input, both
ways): a key that holds one rebuilds for a change that cannot matter. Written, not yet run:
the tests are named, their results are the round's.

| Step | Reads | How each read is known |
|---|---|---|
| the plan (the workspace's load) | manifests, lock, cargo's config files; of the environment `CARGO_*`, `RUSTFLAGS`, `RUSTDOCFLAGS`, `RUSTC_WORKSPACE_WRAPPER`, `CLIPPY_CONF_DIR`, `HOME`, `USERPROFILE`, three `__CARGO_*`, `CI`, `TF_BUILD`, `BLAZE_LOCK`, `BLAZE_IGNORE_PIN_FOR` | one list, `blaze_units::query::read_by_plan`; the load's first query asserts each name it reads is on it; the view is opened with, and the plan kept by, `BuildConfig::planned` (those variables alone) |
| a crate's front end | its files, cfgs, edition, target, the variables cargo derives for it (`CARGO_PKG_*`, `CARGO_MANIFEST_DIR`, `OUT_DIR`: from the unit, never the shell), its dependencies | its identity (`identity_of`); `env!`/`option_env!` of any other variable is a tracked read by name, and the record is filed under the names and values read (`serve/unit/reads.rs`) |
| a crate's codegen | the front end's output, the unit's profile, target, flags, goal, PGO and layout profiles by content | the unit key's own fields (`KeyedUnit`, `KeyedPlan`) |
| a link | each function's code record, the link arguments its build scripts printed, the `-l`/`-L` of the crates it links | by value, in the unit key |
| a build script's run | what blaze sets from the unit (given: in the run's key); any variable it or a program it runs asks for *by name* (traced, keyed by name and value, set or not); when it or a child *walks* the environment, the walk set; files and listings (traced); `rerun-if-env-changed` (cargo's rule, where the trace is not whole) | the sandbox (`sandbox/v2.rs`, the interposers) |
| a proc macro's expansion | its input tokens, the calling crate's own variables, files it opens | the expansion server's trace (`reads.rs`, expansions) |

**The environment.** Nothing of the request's environment is in a unit's key or a plan's key
except the rows above. The request's whole environment still travels with the job
(`JobInput::config`), because a build script may ask for any variable by name; it is keyed by
what was asked.

**The walk set.** A script that enumerates its environment (`env::vars`, a `Command` whose
environment it changes, `system`), or runs a program that takes its environment whole (a C
compiler, a shell, `configure`, `cmake`), used to be lent the user's whole shell and keyed by
all of it: `TERM`, `COLUMNS`, `SHLVL`, systemd's `INVOCATION_ID`, a new `LIBCLANG_PATH`, each
reran every such script. Now a walk sees a fixed set (`blaze_usercode::confine::WALK_NAMES`,
`WALK_PREFIXES`) beside the given variables, and "it read them all" is that set:

- where things are: `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, the temporary directory names
  (the sandbox's own);
- locale and time: `LANG`, `LANGUAGE`, `LC_*`, `TZ`, `TZDIR`;
- the C toolchain and its tools: `CC`, `CXX`, `CPP`, `AR`, `AS`, `LD`, `NM`, `RANLIB`, `STRIP`,
  `OBJCOPY`, `OBJDUMP`, their flags, `CC_<triple>`/`CFLAGS_<triple>`/`HOST_*`/`TARGET_*`, the
  compilers' own (`CPATH`, `C_INCLUDE_PATH`, `LIBRARY_PATH`, `SDKROOT`, `DEVELOPER_DIR`,
  `*_DEPLOYMENT_TARGET`, `SOURCE_DATE_EPOCH`), `MAKE`, `MAKEFLAGS`, `CMAKE_*`, `PKG_CONFIG*`,
  the dynamic loader's paths;
- cargo's and rustc's: `CARGO*`, `RUST*`, `DEP_*`;
- a nix shell's compiler wrapper: `NIX_*` (a bash script, which only walks).

A variable outside the set reaches a program only by a read of its name (`getenv`,
`env::var`): the interposer answers it from an entry `__BLAZE_NAMED=NAME=VALUE` and traces
the read, in the script and in a program it runs with its environment untouched. What this
refuses on purpose: a variable outside the set that only a walking child reads (`$FOO` in a
`configure`) when its parent rebuilt the environment; the script must read it by name or the
set must grow. Windows lends nothing, so a walk there is still all of it. Tests:
`blaze_usercode::script::tests::a_walk_of_the_environment_is_keyed_by_the_walk_set_and_named_reads_alone`
(control: the walk made whole again), `confine`'s and `sandbox::v2`'s; revision cases
`a_variable_no_step_reads`, `a_variable_a_step_reads`, `a_build_script_that_wanted_a_variable`.

**The plan in a unit's key.** A unit's key takes of its `UnitPlan` what making it reads
(`KeyedPlan`: every field but `metadata`, which nothing reads), how its program is run only
when it makes one (`keyed_run_env`), and of the job's kind only whether it checks
(`keyed_kind`: build, test and run key a library alike). It never held another unit's plan
or which packages were asked for. What of the request's shape is still in it, through the
plan: a `cdylib`'s or `staticlib`'s copy to the profile directory (roots only) and LTO
(from the roots); both are in the record's own output list, so they leave the key only when
a hit places outputs from the finding build's plan (not built). A test build and a plain
build share a library's record when its unit is the same; today cargo names the profile
`test` there and the front end's identity may hold the plan (lane sealed-1112), so the
kind byte is necessary and not sufficient.

**The toolchain.** A key holds the code identity of each stage that made its value. What
exists: `blaze_codegen_code::{FRONT_END, CODEGEN, RECORD}`, each the hash of a package
closure (FRONT_END: hir-def, hir-ty, diagnostics, items, lower, macro-eval; CODEGEN: that
plus hir-ty-codegen and blaze-codegen; RECORD: blaze-cargo's closure, which holds both and
blaze-link and blaze-lints), and per-function identities (`fn_info(&f).code_id`, `code_id`
of a compiled function) that key code records, not units. Every unit key holds all three,
so today any edit under blaze-cargo's closure, the linker's among them, misses every unit
record (check-only ones too), though function code records keyed by code identity survive.
The split the rule asks for, not built: a check record keyed by FRONT_END and a
`CHECK_RECORD` identity (lints, diagnostics, the record's encoding); a library's by those and
CODEGEN; only a linking unit's by a `LINK` identity (blaze-link's closure). It needs the
record's making moved out of blaze-cargo's closure or a file-level cut: `Cut` refuses to cut
blaze-link today because `blaze-launch-host` is built from it and `serve/unit/*.rs` name it.
Whether a build script run's key holds the sandbox's own code identity (the launcher and the
interposers, which decide what a run can read) was not checked by this lane.

## Early cutoff between crates

2026-11-12, lane cutoff-1112 (D21). The rule of "What a step reads", for a unit's
dependencies: **a dependent's key covers exactly what it read of the crates below it, and
nothing else.** Not one digest a crate that every dependent takes whole: a part per thing read,
recorded when the unit is made, checked as traced reads are. Three layers. The first two are
written and not yet compiled or run (what compiled, at b16074567d, is their first form: a
library keyed by its dependencies' positioned interface texts); the third has its record and
check written and its trace not.

**The parts**, in the item store's one form (`FactRef`: a kind, a subject, an object; lane
items' kinds `blaze_items::kinds`, the item store's own `blaze_item_hash::values::kinds`;
value `blaze_item_hash::values::value(db, crate, fact)`), plus positions. A body's entry and
a unit's parts are one vocabulary:

| The dependent | reads | part | negative read |
|---|---|---|---|
| resolves a path, an import, a glob | what a name is in a scope | `Resolves { scope, name }` | yes: asked of the scope's index, so a name absent (the glob gave none, the dependent's own item won) is a value, and an item added under it moves it |
| expands a macro | the macro's definition tokens, no position (found by `Resolves`, whose value holds a name's textual-scope macros beside its three namespaces) | `Signature` of the macro item | through `Resolves` |
| resolves a method or an operator through traits | the traits in scope of a module | `TRAITS_IN_SCOPE(module)` (the item store's kind 0x1806): the traits' keys, sorted | - |
| iterates a scope (a glob import's set) | every name of the scope | `WHOLE_SCOPE(module)` (0x1807): every name with its `Resolves` value, so any name added moves it; a lookup alone is `Resolves` | - |
| reads a crate's def map itself | the prelude, the extern prelude, the `#[macro_use]` prelude, the root's flags | `CRATE_NAMES(root module)` (0x1808). Not in its value, and unchecked: the crate's exported derives, its proc-macro functions' mapping and the derive helpers in scope; a unit that reads one of those through the def map is not covered by this part and keeps its surface key | - |
| checks a call, a use of a type | the callee's signature, generics, bounds; a type's fields | `Signature(item)`, `Field { ty, field }` | no |
| holds or moves a value | size, alignment, offsets, niches | `Layout(type)` | no |
| selects an impl, resolves a method | which impls a trait has for a type; what `x.m()` is | `Implements { trait, self type }`, `Method { self type, name }`, `Vtable`, `Coherence`, `Specializes` | yes: asked of the impl index by simplified self type, so "no impl" is a value and an impl added moves it |
| evaluates a constant, a discriminant, an array's length | the value, not the expression | `ConstValue(item)` | no |
| types through a body | an `impl Trait`'s hidden type, auto-trait leakage, a closure's captures | `HiddenTypes`, `AutoTraits`, `RpititTys`, `Captures` | no |
| compiles a copy (a generic, `#[inline]`, a default method), or inlines when optimized | the body | `Code(item)` | no |
| names an item where the program can see it (`type_name`, a symbol) | its path | `Name(item)` | no |
| holds a copy's panic location or debug info, or links | where the item's code is written | `Spans(item)` (new, `compile/parts.rs`) | no |
| anything of a crate lowered as one batch | the crate | `Crate(crate)` | - |

No part's value but `Spans` has a position in it, so a line added to a private body moves no
front-end part of any dependent, and only the units that hold a copy of code below the line,
or link it, read a `Spans` that moved.

**Layer 1, the quick check (built): the surface.** `blaze_lower`'s `CodeText::surface`: a
file's tokens, each with whether whitespace parts it from the one before; no comment; every
body no other crate's front end reads as a mark (a plain body, and a generic, `#[inline]` or
default-method body: a front end checks a call against the signature); no line count. A
position stays only where a constant could hold it: the line and the line's text before each
`line!`, `column!` and `Location`; a macro's definition, a call of a macro that is not std's
and an `#[inline(always)]` function (built into its callers' own code even unoptimized), each
with where it starts and its bytes. A crate that calls a function where a constant is
evaluated, with a `#[track_caller]` `const fn` that can read a `Location` at or below it, has
no surface (the call's position is in no token) and its positioned interface text stands in.
Equal surfaces mean no front-end part of the crate moved.

**Layer 2, the key today (built): surfaces as the key, for the units whose record holds
nothing else.** A check-only unit (`Narrowed::checked`) and a library built at `opt-level` 0
for tier 0 code (`Narrowed::built`, `cut_at_crates`) hold their own code text and the surface
of each crate below. Sound for the library because its record holds its check's outcome, its
file and its own roots (`owned_inputs`, record format 14): a copy of a generic is a function
under its own key, made by whichever unit walks to it (the program that links it always
does), in no library's record. A diagnostic that points into another crate's file is kept by
what it quotes, never as places (`in_own_files`). A program, a test, a proc macro, a build
script and an optimized library hold the code of every crate below (`Narrowed::code`).

This is a superset of what was read, so it is sound, and it is not yet the rule: a signature
changed in `a` still makes every unit above `a` again, read or not. That is layer 3.

**Layer 3, the rule (record and check built, trace not): parts.** `compile/parts.rs`: a
unit's `Parts` are its reads `(crate, part, value)` and, per crate, a quick digest with what
it covers (`Cover::Surface`, or `Cover::Code` when a `Code` or `Spans` part was read; a record
whose digest covers less than its parts is refused, `QuickTooNarrow`). The record is filed
under the unit's base key (its own code text and which crates it depends on, by name) and
`Parts::filed`, the parts with their values; the list is a slot under the base key, as a
build script's reads are. A lookup reads the slot and asks `Parts::check`: a crate whose
quick digest stands is passed over; of any other, each part's value is worked out
(`blaze_items::facts::value`) and the record is the unit's only if none moved.

The trace that fills it is the engine's: a crate's rows record every row and input field
outside the crate that they read (lane sealgroups' imports, `blaze_query::db::seal`), and
`blaze_item_hash::reads` already maps such a read to the fact it stands for, or refuses it.
A unit's parts are its crate's imports mapped so; an import that maps to nothing keeps the
unit on layer 2, named. Sealing uses the same set by stamp ("a dependent stays sealed while
the parts it read are unchanged"); the store uses it by value, so another machine finds the
record. Not built: that mapping for imports (today it is per body step), `Spans` values from
the code generator's span tables, the slot in `compile.rs`.

**Expected.** reth, a line added in a private body of `reth-chainspec` (54 units missed):
with layer 2, every library above is found provided no position kept by the surface lies
below the edit in that file (a macro's definition, a call of a non-std macro outside a
left-out body, `line!`); if one does, the libraries above miss and `blaze why` names "the
interface its narrow key holds of the crate reth_chainspec". Not measured; which of the two
`spec.rs` is, is the run's to say. With layer 3: 0 libraries either way. The units that link
the crate are made again in both (the edit changes a function's code).

**Others.** cargo's fingerprint of a dependent holds its dependencies' fingerprints (source
mtimes or hashes), so any edit rebuilds every dependent; rustc's `.rmeta` and its SVH hold
spans and private items. Bazel's `ijar` and Buck2's metadata actions cut a dependent off when
the interface file's bytes are equal, per crate, positions included. Per item and per read is
what none of them has.

**Not built.** Layer 3's trace and slot. A crate with an item-level call of an unknown macro
has no surface. Optimized libraries. The walk stopping at the crate boundary (a library that
misses still walks into its dependencies).

**Checks.** `blaze-lower`, `code::tests`: `the_surface_text_moves_exactly_with_what_a_front_end_observes`
(blaze-sim) with controls `the_oracle_catches_a_surface_that_holds_a_bodys_lines` and
`a_position_a_constant_holds_is_in_the_surface` (`Plant::Unstamped`); `tests`:
`a_dependents_key_moves_only_with_what_it_observes_of_a_dependency`,
`a_library_built_over_a_crate_is_keyed_by_what_it_compiles_of_it`, controls
`the_oracle_catches_a_library_built_under_its_dependencies_code` and
`the_oracle_catches_a_dependents_key_that_holds_a_bodys_lines`. `blaze-cargo`,
`compile::parts::tests`: a model crate, units and edits under blaze-sim
(`no_unit_is_found_stale_after_any_edit_of_the_crate_below`), the directed cases (a line or a
body below, a signature one unit reads, an item that shadows, an impl that changes
selection), controls `the_oracle_catches_a_trace_without_its_negative_reads` and
`the_quick_digest_covers_every_part_or_the_record_is_refused`. Revision cases
`a_body_edit_makes_its_crate_and_nothing_above_it`, `a_line_added_below_makes_nothing_above_it`,
`a_generic_body_edit_reaches_its_instance`, control
`a_body_edit_below_with_libraries_keyed_by_code` (`BLAZE_UNIT_KEY_DEEP=1`). The end check is
the soundness oracle (`crates/blaze-engine/tests/revisions/oracle.rs`): a key that misses a
read is a stale program. It draws no edit below a library today (its `up` is called from
`main.rs` alone).

## Cold start as a fetch: the standard library (F4)

2026-11-11. A machine that has never built std spends its first build on it (10 to 12 s for
hello-world); every input of that work is public and the same on every machine of one blaze.

- **What it is keyed by.** One std is `(record makers, StdKey::placement)`: the library's
  root, the triple, the profile's settings, std's features, the rustflags, where its code runs
  (`crates/blaze-units/src/query.rs`). The crates asked are not in it, so a program's build and
  its tests' build name the same one.
- **What is fetched.** For each std crate: its dependency image (def map, lang items,
  signatures), whose key has no path in it and is already one `blaze_global::Rule::image`
  accepts for `Origin::Toolchain`; its checked bodies and constants; and each unit's record with
  its tier-0 code (`Kind::Machine` and the unit record in the shared std store).
- **What exists.** `blaze_global`: the rule for what is public, Ed25519 records under pinned
  keys, the client as a `blaze_cas::Remote` (a fetched value enters the store only after its
  signature and content hash), and `bundle`: one signed manifest of every key a cold build of a
  lock file's crates.io dependencies computes, fetched in parallel. A dependency image's
  store can read through to it (`hir_def::deps::ReadThrough`).
- **What is missing.** (1) A bundle of a std: `BundleId` from the makers and the placement,
  its manifest listing the keys above, made by the builders for each target and profile blaze
  ships. (2) A unit record's key hashes its whole `UnitPlan` (`RecordInputs::key`,
  `crates/blaze-cargo/src/serve/unit/compile.rs`), whose `outputs` and `OUT_DIR` are absolute
  paths under this machine's `<machine>/std/<makers>/<placement>`: two machines with other home
  directories make other keys. The std directory has to be written as a root in the key, as
  build scripts' roots are (Portable keys, above), before any std record is another machine's.
  (3) The ask: the `std library` task fetches the bundle when the shared std store has no record
  of the placement, before the plan's units are posted, and says a miss by name (no bundle
  for this target, a signature refused) with what will be compiled instead. (4) F2 of
  [restore.md](restore.md): a fetched image is only worth its bytes if no crate is resolved
  before the store is asked.

## Item sets: one value an item (rule granular-caching)

2026-11-12, lane storegc-1112. Designed, not built; the order to build is at the end. Sizes are
from the code's layouts, none measured.

**Why.** The front end keeps one value an item (attribute flags, an impl's items, a body's
inference, MIR): 16 to 200 bytes for the small kinds, 0.5 to 20 KiB for bodies, 10^4 to 10^5 of
them a crate, each under a 32-byte key made of the item's own content and what it read, never
of its crate's image key (so an unchanged item under a new version of its crate is the same
value). The store as it is holds each as a record: a 96-byte header and up to 15 bytes of
padding on disk, a 64-byte index row at 7/8 load (74 bytes) and a byte of flags, 170 to 290
bytes over the value. Records are already packed one after another into shared packs (13 to 35
small ones a 4 KiB page), and a get is one index probe and one pool page. What does not hold up
at these counts: the index table is rewritten whole on the daemon's loop every 65,536 new
records; a period's scan walks every row and lists them all in memory; the cold tier rewrites
each unread value on its own; a pin lists keys (32 bytes each, rewritten whole when one is
added, each looked up at every scan).

**One pair.** `put(set, key, bytes)` and `get(set, key)` (`hir_def::nameres::frozen::entries`'s
`Entries` is the front end's side of it). `key` is the item's; `set` says which items are kept
together (a crate's, or a file's items within it) and is a hint for where to look, never part
of what finds the value: a value is found by `key` under any `set`. A get looks, in order, at
the record under `key`, the sealed set under `set`, and the sealed sets the crate's root names
as earlier ones (below). Two forms underneath.

| Form | For | An item costs over its bytes | What changes on one item's edit |
|---|---|---|---|
| live | a crate being edited (every workspace crate, bstd among them) | a record: 96 + padding, 74 in the index, 1 of flags, and 32 in a leaf of its set's index value: about 210 | one record, one leaf (4 KiB) and the root (32 bytes a leaf) |
| sealed | a crate never edited under its version (std, registry crates), and a live set whose crate has been quiet | 16 in a directory at 7/8 load, its 32-byte key before the value for the check, padding to 8: about 50 | nothing: a sealed set is written once |

**Live set.** Each item is a record under its own key, as today. The set is an index value, a
record of `Kind::Query` in the form `Listing`: a root that lists the keys of its leaves, each
leaf a record listing up to 128 item keys, sorted by key, so one item's change rewrites one leaf
and the root, and two sets that hold the same item name one record. The root also names the
sealed sets its crate's earlier versions were compacted to. The collector follows a listing as
it follows a chunked output's chunks (`collect.rs`, `named_by`): what a kept root lists is
kept. A listing is immutable, so what it names is read once and remembered by its key, never
read again at each scan.

**Sealed set.** One store value under the set's key: a header, a directory (open addressing
on the first 8 bytes of the item key: offset and length, 16 bytes a slot), then each item's
32-byte key and bytes end to end. A get reads the directory's slot and the value a part at a
time (`Reader::part`): two pool pages, three when the value straddles. It is one index row, one
record for the collector, one key in a pin. std and registry crates are written sealed at once;
that gives up sharing an item between two versions of such a crate, which is accepted: they
are not edited under a version. A live set whose crate was quiet (lane sealgroups' rule) is
compacted to a sealed one in the background, on a worker, as a pack is collected; the next
version's root names it, so the unchanged items of an edited crate are found there and only
what changed is written as records.

**Pins and keeps.** A pin names set keys (roots and sealed sets), not items: `toolchain` and
`bstd` list a key a crate, not 50,000. `serve::collect::front_keys` gives a crate's set keys.
Growing a pin still rewrites it, at a key a crate.

**The scan of a period's end** becomes bounded and incremental: it walks a fixed number of
rows a step from a cursor and keeps no list of them all; flags rotate when the cursor wraps.
With sets, the rows are the records of live sets and one row a sealed set.

**The index table** is built on a worker from a `Reader` and handed back for the write, as the
period's scan is; the loop only starts it and takes the bytes. This is first: it is the worst
cost at these counts and is needed whichever form an item is in.

**Tests** (blaze-cas, the simulation): 50,000 items put as a live set, then one changed: one
record, one leaf and the root are written and nothing else (counted in bytes appended); the
collector, with the root kept, keeps exactly what the listing names and forgets an item no
listing names; the control is a pin naming the items directly, which is 1.6 MB rewritten and
50,000 lookups a scan. A sealed set: every item reads back by key under its set and under
another set whose root names it; a crash at any point of a compaction leaves the live records
or the sealed set, never neither. The index built on a worker: a commit that would write it
holds the loop for no walk of the rows (counted in rows visited on the store's thread).

**Order to build.** (1) the index table on a worker; (2) the `Listing` form, the collector
following it, pins by set key; (3) the `put`/`get` pair with live sets, and the front end's
`Entries` on it; (4) sealed sets and `Reader::part` reads, std and registry crates written
sealed; (5) compaction of a quiet live set; (6) the incremental scan. `hir_ty::check_cache`'s
own container of pages goes once (4) is in.

**State, 2026-11-12 (lane storegc2; written, not yet compiled or run).**

- Built: (1) the index table off the loop (`crates/blaze-cas/src/rebuild.rs`). The store's
  thread does O(packs) at a take and at a put; the swap after a table is written still walks
  the delta there (65,536 rows and what was put while the build ran, never the table's rows).
  A daemon with no collector still builds its table in the commit. The head that names a
  built table names each pack's end at the take as indexed. Simulation: `tests::rebuild`.
- Built: the live form (`crates/blaze-cas/src/sets.rs`, `Kind::Set`), not as first drawn:
  a listing is keyed by its own bytes, and leaves end at keys chosen by the key itself (one in
  128), not at every 128th, so one item more changes one leaf (two when its key ends one)
  and the root. A daemon's `Sets` holds each set's members (what was got or put under it)
  and lists the sets a build touched at its end; `serve::collect::StoreEntries` is the front
  end's `Entries` on it. A get finds an item by its key alone, so a file-sized set whose
  file was edited still finds the unchanged items: no key → set step is needed for the live
  form. The collector's scan follows a listing (`collect::named_by`) and reads it again at
  every scan (not yet remembered by key).
- Built (lane storegc3; written, not yet compiled or run): the sealed form
  (`crates/blaze-cas/src/sealed.rs`, `Sets::seal_at_once`, `seal`, `quiet`, `adopt`,
  `follows`; simulation `tests::sealed`). As built it differs from the table above:
  - A sealed set is keyed by its own bytes, never by its set: which items a set holds
    depends on what was asked of the crate, so two machines would put different values
    under a key made of the set (rule keys-cover-every-input). What finds a set's sealed
    sets is its root, which lists them; a daemon started again learns a set's root from
    what names it durably (a pin's keys, a build's record) through `Sets::adopt`. A set
    whose root nothing durable names is not found after a restart: its crate's items are
    made again. Open: registry crates have no such name yet.
  - The directory's load is 3/4, not 7/8 (a lookup that finds reads two or three slots,
    one that misses eight or nine, in runs of eight slots a part): an item costs its
    32-byte key, 21 bytes of slots and padding to 8, about 57 bytes over its own.
  - Given up, and accepted by the owner: an item of a set written sealed at once (std, a
    registry crate) is found only under that set and the sets that follow it, never by
    its key alone, so two versions of such a crate share no item unless the driver says
    the new one follows the old (`Sets::follows`). Until its seal such an item is in
    memory only: a crash before the seal loses it and it is made again.
  - A set has at most four sealed sets; the seal that would make a fifth merges them.
    A compaction drops nothing: the records it sealed go when the collector finds no kept
    root naming them, so the caller moves its pin to the new root once the seal is durable.
- Built (storegc3, not compiled): pins by index value. `Store::pin_change` adds keys and
  lets others go in one write; `StoreEntries::keep` queues a set's root in place of the
  root the pin named for it before, and `StoreEntries::adopt_pin` learns those roots from
  a pin after a restart. A pin is a key a set and is rewritten (a key a set, not a key an
  item) only when a set's root changes.
- Built (storegc3, not compiled): a period's scan walks the rows twice through
  `Look::each_row` and lists none of them. It is still one run on a worker under one
  guard, not steps from a cursor, and it still holds every rooted key (32 bytes each).
- Decided, not built: a scan reads a kept listing again each time. Remembering what a
  listing names would hold 32 bytes an item in memory to save one page read in 128 item
  marks; to be revisited only if the scan's span says the reads cost.
- Not built: the driver's calls (`StoreEntries::seal_at_once` for std and registry sets,
  `seal` and `quiet` at a build's end on a worker, `adopt_pin` once the pins are read,
  `keep(TOOLCHAIN|BSTD, sets)` in place of item keys; no build calls `StoreEntries::list`
  or `entries::set` yet), the scan in steps from a cursor, the cold tier in groups, a
  collection (not only a scan) under a pinned root in the collector's simulation.

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
