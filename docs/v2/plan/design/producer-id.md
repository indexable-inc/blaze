---
status: plan
summary: A producer's identity (decision D105) is a digest handed down from the entry in one table, never a version string a person bumps. Subject keys say what is asked; the producer enters where a result is cached (the drive's task key, the kept-expansion key, the code key), taken as an argument. Piece 1 derives every row from the binary's build id; piece 2 from the source of the crates each producer is made of, found from the manifests at build time; when blaze builds blaze, from the producer function's code identity by reflection. A ratchet counts typed-in producer strings and only falls.
date: 2026-11-16
---
# Producer identity

Decision D105. Rules 21 (keys cover every input), 52 (cache by code identity, nothing
invalidates by hand), 75 (nothing ambient), 73 (a type, then a lint, then a rule).

**2026-11-17: piece 2 is undone; every row is the binary's id again.** A `blaze check` stopped
with two values under one key: two builds of blaze, with equal Names-row sources and other code
elsewhere, wrote different `CrateScope` bytes under one key. Every producing function is in
blaze-cli (work.rs, names/scope.rs, macros.rs, body_types.rs, program.rs), whose rows held its
own files alone (`producer-own`) while its code called crates in no row (blaze-ladder's
`native_links::table_digest` is in the `CrateScope` record; body_types.rs maps verdicts through
blaze-lint) and its features (`sandbox`) change what the producers make. The source closure of
the crate that holds the function is the binary, and only the binary's linker id also holds its
features and profile, so the entry keys every row by it (blaze-cli `src/producers.rs`, the table
in `home.rs` `row_of`); `of_sources`, `Sources` and the entry's build step are gone, and
blaze-producers refuses `producer-own` by name. A row takes a source digest again only when its
producing function moves into a crate that names the row, whose closure is then what it runs
(blaze-producers' `a_row_covers_every_crate_its_producer_calls`). The text below is the piece 2
design as it was.

## The problem

On 2026-11-16 three typed-in strings stood for "the code that made this value":
`blaze_lower::BODY_KEY_DOMAIN` ("blaze v2 lower body 10, parser 1", with its two siblings for
signatures and anonymous constants), `blaze_expand::EXPANDER` ("blaze-expand expander 4", mixed
into every call key) and `blaze-cli body_types::BODY_CHECKER` ("blaze body checker
2026-11-16.8", mixed into the drive's tool). Each was bumped by hand about ten times that day,
each bump a message between integrators. A missed bump is a record made by older code read as a
fresh answer: the class of rule 21.

## Two kinds of key

- A **subject key** says what is asked: a call's definition and tokens ([`call_key`]), an item's
  text and edition ([`body_key`]), a task's name and source. It holds no producer. Its domain may
  end in a number that is the key's own LAYOUT (how the inputs are encoded), which changes only
  when the encoding does.
- A **result key** says what answers, by whom: it is a subject key and the producer's id. Every
  place a result is filed or found by a key alone takes a `ProducerId` as an argument there:
  - the drive's `task_key(tool, name, source)`: the tool of the task's kind (below);
  - the kept expansions (blaze-scope `KeptExpansions`): `blaze_expand::kept_key(expander, call)`;
  - the code records (blaze-ladder `code_key`): the code builder's id (with decision D106's
    writer, who changes that key now);
  - any later cache by call key (pipeline runner 4's expansion texts): `kept_key` again.

So "a key function that mixed in a producer string takes the producer's id" holds at the result
key, which is where the code identity belongs (rule 52: results are cached by code identity AND
arguments). It does not hold at the subject keys: threading an id into `call_key` would reach
every hygiene mark, error and body reference that names a call (about forty sites in six crates)
for no gain, because none of them is a cache. `kept_key` returns a `CallKey` for now because the
kept set's entries are typed so (blaze-expand kept_set.rs); the set's own key type is the next
step of rule 76, taken when the expansion-key lane next changes that file.

A FORMAT number (a record's layout, `ENTRY_FORM`, a magic) stays a typed integer constant: it
names a contract between a writer and a reader, not code.

## The types (crates/blaze-ids/src/producer.rs)

blaze-ids is the lowest crate every producer names (it holds `ContentId` and `Hasher`), so it is
the one home (rule 30).

- `Row`: one per producer: `Expander`, `Lowering` (the body, signature and anonymous-constant
  lowering and the parser under it), `BodyChecker`, `Names` (the names solver), `CodeBuilder`,
  and `Whole` (the binary: every kind of value no row is split out for yet).
- `ProducerId<P>`: the id of producer `P`, a `ContentId` with a marker type (`Expander`,
  `Lowering`, ...): passing the lowering's id where the expander's is asked does not compile
  (rule 76).
- `Producers`: the table, one `ContentId` per row. Made ONCE, in the entry (`blaze-cli`
  `main.rs`, beside `Host::build_id`), and handed down; no `static`, no global, no default.
  Constructors: `of_build(build_id)` (piece 1), `of_sources(build_id, &Sources)` (piece 2),
  `for_tests(seed)` and `planted_for_tests(row, seed)` (tests only: a fixed table, and one row
  moved for the causal test).

## Piece 1: correct at once

Every row is `H(row name, build id)`: any rebuilt blaze has new ids for everything, so nothing is
ever stale. The three strings are deleted in one batch:

- blaze-expand: `EXPANDER` goes; `call_key` is a subject key; `kept_key(expander, call)` is new.
- blaze-lower: the three domains lose their producer numbers ("lower body 10, parser 1" becomes
  the layout-only "blaze v2 lower body key 1"); `body_key`, `signature_key`, `anon_const_key`
  are subject keys (their one caller, `lower_file`, hands them to a `Sink`: the sink is the cache
  and takes the producer if it files them).
- blaze-scope: `Kept::keeps_expansions` returns the expander's id (`Option<ProducerId<Expander>>`
  in place of `bool`): a store that keeps expansions names the code that made them;
  `KeptExpansions` keys its set by `kept_key`.
- blaze-cli: `BODY_CHECKER` and `home::tool_of` go; `home::tool(&Producers)`; the producer test
  plants one row (`Producers::planted_for_tests(Row::BodyChecker, ..)`).

## Piece 2: one row per producer

**What a row is.** The digest of the source of the crates the producer is made of: its root
crates and every workspace crate they depend on, transitively (normal and build dependencies;
dev-dependencies are not in the binary); for a dependency from outside the workspace, its name,
version and checksum from `Cargo.lock`; every file under the pins folder (`pins/`, the pinned toolchain's facts, which blaze-pins
includes from outside its own directory).

**Which crates are roots.** Declared in the root crate's own manifest, never in a list kept
elsewhere:

```toml
[package.metadata.blaze]
# The producers this crate's code makes values for (docs/v2/plan/design/producer-id.md).
producer = ["expander"]
```

`producer-own = [..]` adds a crate's own files without its closure: every kind's task runner is
in the entry today (work.rs, names.rs, macros.rs, body_types.rs), so every row holds blaze-cli's
own files until the runners move into their producers' crates (remote-read.md piece 6). Roots
on 2026-11-16: blaze-expand and blaze-proc-macro (`expander`: an expansion runs a macro's wasm
there), blaze-lower (`lowering`), blaze-ladder (`body-checker`, `code-builder`), blaze-scope and
blaze-world (`names`), blaze-drive (every row: it writes every record's envelope and read list),
blaze-cli (`producer-own`, every row). A row with no root, or a name that is no row (or
`whole`), is a typed refusal of the build. The build setting that changes what code computes
(debug assertions, with overflow checks beside it under the workspace's profiles) is mixed into
every row at the entry (blaze-cli `src/producers.rs`).

This doc and remote-read.md's "End state: what a record's key says about its producer" are one
concept: its world (a) stage ids (pieces 2 to 4: a script, a stamp crate, a build entry) are
this build step and table; its piece 1 (producers in the drive) is "The drive" below.

**How it is computed.** A build script of the entry crate (crates/blaze-cli/build.rs calls
crates/blaze-cli/build/producers.rs, a thin driver over the sans-IO core in the new crate
`blaze-producers`, a build-dependency of blaze-cli alone): it finds the workspace root (the
nearest `Cargo.toml` above with `[workspace]`), reads every member's manifest, follows `path`
and `workspace = true` dependencies through `[workspace.dependencies]`, and digests each row:
for each crate in the closure, sorted by directory, every file under its directory except its
top-level `tests/`, `benches/`, `examples/` and `target/` and any name starting with `.` (a
`.DS_Store` is the machine's), sorted by relative path, each as (path relative to the workspace
root, length, bytes); then the `[workspace.dependencies]` entries the closure names (each value
whole, so a header-form entry counts), `[workspace.package] edition`, the lock rows (name,
version, source, checksum) of every outside crate reached and, transitively, of theirs, and the
files under `pins/`. A lock row with no source is a path crate (a `[patch]` under vendor/, which the
walk reads like a path dependency): it joins the closure and is digested by its files. An
`optional = true` dependency the lock lacks is one no feature turned on: not built, not an input.
A name a closure reaches that the lock lacks (the lock and the sources of different moments, as
a snapshot mix builds) refuses that row alone: the step warns naming the crate and the row,
writes it `None`, and the entry keys it by the binary's build id (piece 1's key: made again by
every rebuilt blaze, never read stale); the other rows stand. Only the disk failing under the
step stops the build; nothing in it panics. Nothing of the machine (rule 25). The core is a state machine (`Walk`: it asks
for a file or a listing by workspace-relative path, the driver answers), tested on an in-memory
tree. It writes `$OUT_DIR/producer_sources.rs` (a `Sources` value of row names and digests,
rewritten only when its bytes change) and `cargo::rerun-if-changed` for the root manifest, the
lock, the pins folder, every crate manifest it read, and each top-level entry (`src`,
`build.rs`, ...) of each crate in a closure, never their `tests/`. The entry includes it
(blaze-cli `src/producers.rs`) and calls `Producers::of_sources(build_id, &SOURCES)`.

**How often it reruns.** Every crate in a closure is already a dependency of blaze-cli, so an
edit to its source recompiles blaze-cli anyway; the step adds a recompile only for a non-Rust
file under a watched `src` and for the manifest of a crate outside every closure. Its run is one
listing and one BLAKE3 pass over the closures' files (about 8 MB of source on 2026-11-16).

Why a build script: it is the one step that sees the source as the binary is built, and only
the entry depends on its output, so an edit to any crate reruns one script and recompiles only
the crate that is recompiled anyway. Under cargo it runs as any build script. When blaze builds
blaze it runs in blaze's deterministic sandbox, which serves a workspace member the workspace
above `/workspace/<member dir>` (decision D96); every file it reads is in its read list, so it
reruns exactly when a file of a closure changes.

**What it cannot do at crate grain** (said, not written around): blaze-lower depends on
blaze-typeck and blaze-scope, so an edit to the checker moves the lowering's row too; blaze-ladder
holds both the front end (front2) and the run path, so the body checker's and the code builder's
rows both move with either; the `#[cfg(test)]` files in `src/` count. Each over-approximates (a
needless miss, never a stale answer). Splitting a crate sharpens its row with no change here.

**The drive.** The drive's `Tool` stops holding the build id for kinds that have a row: the
command's config holds a tool per kind (`tools: [Tool; KINDS]`, each the record forms' magics and
that kind's row; `Code` takes `CodeBuilder`, `BodyCheck` takes `BodyChecker`, `CrateScope`,
`ModuleScope` and `Group` take `Names`, `Expansion` takes `Expander`; every other kind `Whole`).
The kind-to-row map is one exhaustive `match` in the entry, so a new kind must be placed. The
certificate and the walk keep the whole tool: they speak for the whole command.

## When blaze builds blaze

The row becomes the code identity of the producer's entry function by reflection (rule 56;
code-identity.md, "Code that reads its own identity": `fn_info(f).code_id` is a constant):
`Producers::of_code(&[(Row, ContentId)])`, the ids made in the entry by
`fn_info(blaze_expand::expand).code_id` and its siblings. The seam is the one call in
blaze-cli `src/producers.rs` that makes the table: the build script's `SOURCES` go and that call
changes, nothing else. That is finer than crate grain (a function's identity is its callees'
Merkle graph), and the over-approximations above go.

## The gate

1. The type: a result key cannot be made without a `ProducerId<P>` of the right `P`.
2. The ratchet (`scripts/src/bin/ambient/producers.rs`, rows in
   tools/ratchets/producer-strings.txt, only falls): it counts, outside test code, each typed-in
   producer string in key code. Key code is a string literal passed first to a hash call
   (`Hasher::new`, `ContentId::of`, `digest`, `derive_key`, `new_derive_key`, `producer`) and a
   string constant that is a domain (passed first to such a call in its file, or named `*DOMAIN*`);
   one lexer pass skips comments, strings that hold a call's text, and `fn` definitions. A
   producer string is told from a domain separator and a format number by what it is:
   - a format number is an integer constant: never read;
   - a domain separator names a kind of key and at most one layout number ("blaze-drive task
     key 1"): not counted;
   - a producer string is any of: a literal passed to `producer(..)` (blaze-units' and
     blaze-tree's `Query::PRODUCER`, "the number raised with every change to what the query
     computes": sixteen, in crates outside the workspace's members, which the walk skips, so
     the list held 2 on 2026-11-16: blaze-cond `KEY_CONTEXT` and blaze-run `read_name`, both
     dated); a date in the text ("2026-11-16.8"); two or more
     whole-word numbers ("lower body 10, parser 1"; `v2`, `x86-64`, `blake3` are words); a domain
     constant named for code (`CHECKER`, `EXPANDER`, `PRODUCER`, `LOWERING`, `SOLVER`, `BUILDER`
     as a `_` word) in a file that hashes (DWARF's `PRODUCER = b"blaze\0"` is in one that does
     not).

   What it cannot see: a domain or an integer `VERSION` with ONE number bumped for a code change
   (blaze-scope `crate_scope.rs` `const VERSION: u32 = 2`, "// 2: a body block holding a
   statement-position macro call is a block module"). A number's logged history does not tell
   code from layout (the record forms log their layout changes the same way), so no rule here
   reads it; that class is closed by part 1 (a result key holds the producer's id, so no bump is
   ever needed), and the scope's `VERSION` goes when its record's key takes `ProducerId<Names>`.
   The selftest (`ambient selftest`) shows a planted `const CHECKER: &str = "blaze body checker
   2026-11-16.8"` passed to `ContentId::of` refused with its line, and passing once removed.
