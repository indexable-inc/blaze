---
status: plan
summary: blaze-lints in v2: clippy's lints and blaze's own (`blaze::`), run per body as queries over `&Db` and the flat tree's views; hot_lint.py's rules as typed lints; the `clippy.toml` bans (`disallowed_*`) by resolution, with fixes `blaze check --fix` makes.
date: 2026-10-29
---
# Lane `lints`: blaze's clippy and blaze's own lints

`crates/blaze-lints` is blaze's clippy: clippy's lints, names, levels, groups, `clippy.toml` and
text, run by `blaze check` over the analysis it does. v2 moves it onto the
front end's concrete database (`base_db::Db`, [frontend/db.md](frontend/db.md)) and the flat
tree's borrowed views (`syntax::view`, [frontend/tree.md](frontend/tree.md)), runs every lint
per body as a query, and adds blaze's own typed lints in the `blaze::` tool namespace, which
replace `tools/v2/hot_lint.py`. No push for clippy parity now: a clippy lint is added only
where a v2 crate's `[lints]` sets one blaze lacks.

Design notes: [lints/cx.md](lints/cx.md) (the context every lint is written against),
[lints/blaze.md](lints/blaze.md) (the `blaze::` lints), [lints/speed-of-light.md](lints/speed-of-light.md).

## How a crate is linted

rustc's late lints read one body's HIR with its `TypeckResults` (`LateContext`); clippy's are
the same passes. v2 does that per body, as queries:

- **`body_lints(db, def, set)`** (`crates/blaze-lints/src/query.rs`): the lints of one body
  (`hir_def::DefWithBodyId`), over `hir_def::expr_store::body::body_syntax(db, def)`: the
  body's anchor parsed as a tree of its own, its ranges relative to the anchor. Edits elsewhere
  in the file move the anchor without changing that tree, so the query is not run again: only
  changed bodies are linted again. What it finds is relative to the anchor; the driver adds
  `Body::anchor_offset(db, def)`.
- **`item_lints(db, file, set)`**: the lints of what is not in a body (items, signatures, macro
  definitions, item-level macro calls), over the file's outline. An outline's bodies are lazy
  nodes, never entered here, so this reads no body and parses none.
- `set` is an interned `LintSet`: which lints may be at a level above `allow` in that body
  (the command line and the attributes of the items around the body, plus any lint the body's
  own text names) and the `clippy.toml` configuration. A body where every lint is allowed runs
  no query.
- **Types only when asked.** A lint asks for a type, a method's resolution or an adjustment
  through `hir::BodyAnalysis` (requested of lane `db`, see the request file), which infers the
  body the first time it is asked and not before. A lint that needs only syntax walks the views
  and never forces inference; a typed lint first filters on syntax (`.clone()` with no
  arguments) and asks for types only for what passes.
- **The driver** (`check.rs`, not a query) enumerates the crate's files and bodies, calls the
  queries, decides each finding's level as rustc does (the attributes around it, the command
  line, `warnings`, `#[expect]`), orders them as rustc emits them, and names files. Only the
  findings' levels are decided at the edge, so a crate with nothing found walks no attributes.

## Blaze's own lints (`blaze::`)

`hot_clone`, `hot_alloc`, `hot_alloc_call`, `hot_dyn` and `std_hashmap` (all five the group
`blaze::hot`, as hot_lint.py checks SipHash maps in hot crates too), typed, in
`crates/blaze-lints/src/blaze/` ([lints/blaze.md](lints/blaze.md)). They are `allow` by
default, so a crate that doesn't opt in sees nothing (superset-of-rust). A workspace turns one
on for every member in its root manifest's `[workspace.metadata.blaze.lints]` (`hot_alloc =
"warn"`), which blaze gives each member as `--warn=blaze::hot_alloc`; not in `[lints.blaze]`,
which cargo hands rustc, which stops at a tool it doesn't know (E0602), so every cargo build of
the workspace breaks. A crate sets its own level in its root, which rustc reads too:
`#![feature(register_tool)] #![register_tool(blaze)] #![deny(blaze::hot_alloc)]`. **Hot is
where the lint is on**: a crate is declared hot by turning a `blaze::hot*` lint on for the
crate (the workspace's table or `#![deny(blaze::hot)]`), and a function is marked hot by `#[deny(blaze::hot)]` on it or on an
item around it; `#[allow(blaze::hot_alloc)]` marks a cold path inside a hot crate. This is
rustc's lint-level scoping, so no new attribute and no name that can clash with the `blaze`
crate of `#[blaze::cache]`. A line keeps the cost with a reason: `// clone: <why>`,
`// alloc: <why>` or `// dyn: <why>` on it or on the line above, as hot_lint.py has it.

`tools/v2/hot_lint.py` is deleted at integration once these build and its fixtures pass as
their tests.

**One table.** The compile's lint registry, `crates/blaze-lint-defs/src/lints.rs`'s `BLAZE`,
has every `blaze::Lint` of `Lint::ALL` (in that order, after the front end's
`blaze::const_impure`) and `names.rs`'s `BLAZE_GROUPS` has `blaze::hot` as `HOT` has it; a test
of blaze-lints (`the_registry_has_blazes_lints_and_group_as_here`) holds the two to each other.
So a level the build sets for one is known to the compile, not an unknown lint (E0602). Those
lints are `is_externally_loaded`, as rustc marks clippy's: the compile leaves an `#[expect]`
only of them to blaze-lints, which says under `blaze check` whether it was fulfilled, and a
build says nothing of it. A name in `[workspace.metadata.blaze.lints]` or a package's
`[lints.blaze]` that is no lint or group of the registry stops the load with an error naming it
and the manifest (`crates/blaze-units/src/load/plan/flags.rs`).

`parallelizable` and `serial_dependence` (child `lints-parallel`,
[lints/parallel.md](lints/parallel.md)) are judged on MIR, in `crates/blaze-lints/src/blaze/parallel*`.
They are the same kind of `blaze::Lint` (in `Lint::ALL`, with a bit of `On` past the hot lints',
`allow` by default, opted in by `[workspace.metadata.blaze.lints]`), but in no group:
`blaze::hot` stays the hot lints.

## `clippy.toml` bans: `disallowed_types`, `disallowed_methods`, `disallowed_macros`

v2 runs no stock clippy, so these three, ported in `crates/blaze-lints/src/lints/disallowed.rs`,
are what enforce the `disallowed-*` lists of `.clippy.toml` and the hot crates' `clippy.toml`
(clippy's rule: a crate's own file replaces the root's). Each entry is an item's path, or a
table with clippy's keys `path`, `reason`, `replacement`, `allow-invalid` and two of blaze's:

- `allow-in = ["crate::driver", ...]`: the modules (and those inside them) where the item is
  allowed, by module path, never by file.
- `lint = "blaze::wall_clock"`: report under that blaze lint instead, so an
  `#[expect(blaze::wall_clock, reason = "..")]` reads well.

Stock clippy refuses both keys, failing `cargo clippy` for the crate, so the repo's own
`clippy.toml` files use neither. `blaze::wall_clock` is instead three entries built into the
query, after the configured ones: `std::time::Instant::now`, `std::time::SystemTime::now` and
`std::time::Instant::elapsed`, reported under that lint, in every crate whether or not it has a
`clippy.toml`. A configured entry for the same item wins.

An unknown key or a value of the wrong type is an error as the file loads. A crate resolves the
entries once per configuration (the query `disallowed_defs`): an entry's path walks from a
dependency's (or `crate`'s) root through modules, then a type's inherent impls or a trait's
items, as clippy's `def_path_res` does; one that resolves to nothing, or to an item of another
kind than its list's, is an error at the crate's root unless `allow-invalid`.

A finding is any written path that resolves to an entry's item (in a `use`, a type, an
expression, a pattern, a macro call), after re-exports, renames and globs, and for a method a
method call resolved to it. The message names the item ("use of a disallowed type
`std::sync::Mutex`"); the suggestion's message is the reason, else "use". An entry whose
`replacement` is a path the crate can name has a machine-applicable fix:

- a `use` is rewritten whole: the moved items get a line of their own, the rest stay, a renamed
  item keeps its alias, and an item whose name changes is imported `as` the old name, so no other
  line changes;
- a qualified path is rewritten to the replacement;
- a bare name follows the `use` of the file that brings it in; a glob stays, with the moved
  names imported beside it, which shadows them;
- std's `Mutex` and `RwLock` to `parking_lot`'s: the `.unwrap()` or `.expect(..)` after `lock`,
  `read`, `write`, `into_inner` or `get_mut` is dropped (they never poison); `Condvar`:
  `g = cv.wait(g).unwrap()` (or `wait_while`) becomes `cv.wait(&mut g)`.

With no fix the finding says why in a note: the replacement is not a path, its crate is no
dependency, a method call (which the fix would have to rewrite as a call), a lock's poisoning
result used other than unwrapped, a `use` with a comment inside its tree, or code a macro made.

## Clippy ports for AGENTS.md's rules

Clippy lints ported in `crates/blaze-lints/src/lints/`, one file each, because each finds code
that breaks one of AGENTS.md's rules. Each module's doc names the clippy source file it follows
(indexable-inc's fork at c2308e38, the nearest checkout to the pinned nightly) and where it
differs: messages name the item, the type or the function, and a fix is offered only where it
is right. In blaze's own workspace each is at `warn`, not `deny`, until the tree is clean of it.

- `allow_attributes` (rule 3): `#[allow(..)]` outside a macro's own tokens, MSRV 1.81. The fix
  rewrites `allow` to `expect` (machine-applicable); the `reason` is still written by hand.
- `allow_attributes_without_reason` (rule 3): `#[allow]` or `#[expect]`, inner or outer, `cfg_attr`
  included, whose last item is not `reason = ".."`. No fix.
- `map_err_ignore` (rules 3, 14): `.map_err(|_| ..)` on a `Result`: the new error can't say what
  failed. Not `|_e|`, a `move` closure or a function path.
- `unused_result_ok` (rule 3): the statement `result.ok();`, which only silences `#[must_use]`.
  In a function returning a `Result` it suggests `?` (maybe incorrect).
- `let_underscore_must_use` (rule 3): `let _ =` of a `#[must_use]` type (through references,
  tuples, arrays, a `Result` with an uninhabited error) or of a `#[must_use]` function's result;
  a lock guard is one, so clippy's `let_underscore_lock` is not separate.
- `let_underscore_future` (rule 3): `let _ =` of a future, which never runs it.
- `unwrap_in_result` (rules 3, 15): `unwrap` or `expect` of an `Option` or `Result`, as a method or
  by path, in a function returning one; closures, constants and `async` blocks are their own
  bodies. Same-kind `unwrap` is fixed to `?` (maybe incorrect).
- `panic_in_result_fn` (rule 15): `panic!`, `assert!`, `assert_eq!` or `assert_ne!` in a function
  returning a `Result`, including those a crate's own macro expands to; `todo!` and
  `unreachable!` are not counted.
- `infinite_loop` (rule 13): a `loop` nothing leaves in a function not returning `!`; the walk
  goes into macro expansions. `-> !` is suggested where the loop is the function's tail.

- `mutex_atomic`, `mutex_integer` (rule 9): a `static` or `let` of std's or `lock_api`'s `Mutex`
  around a `bool`, an integer up to 64 bits, a `usize`/`isize` or a `*mut T` (`mutex_integer`
  for the fixed-size integers). `Mutex::new(x)` becomes `AtomicX::new(x)`; for a local whose
  every use is `*m.lock()` read or assigned, the uses become `load`/`store` with `SeqCst` and
  the fix is machine-applicable, otherwise maybe incorrect with the other uses named.
- `rc_mutex` (rule 9): the type `Rc<Mutex<_>>` (std's or `lock_api`'s lock), in `box_collection`'s
  walk of written types.
- `arc_with_non_send_sync` (rule 9): `Arc::new(x)` of a concrete type not `Send`, not `Sync` or
  neither.
- `iter_over_hash_type` (rule 20): a `for` over a hash map or set (std's, `hashbrown`'s,
  `papaya`'s, `dashmap`'s, `blaze_hash`'s) or one of std's iterators of one. Its check,
  `lints::iter_over_hash_type::hash_iterated`, is what `blaze::hash_order_output` narrows to
  loops whose order reaches output.
- `large_stack_arrays` (rule 9: work runs on fibers): an array expression of a function's or
  closure's body, not of a `const` or `static`, whose layout passes 1/1024 of a pool fiber's
  stack (`blaze_graph::sched::FIBER_STACK`, 16 MiB, so 16 KiB); `array-size-threshold` sets it.
  An array whose layout the front end can't compute (a generic length or element) is left alone.
- `large_stack_frames` (rule 9): a function, or a closure in it, whose frame passes 1/32 of a
  pool fiber's stack (512 KiB); `stack-size-threshold` sets it. The frame is the runtime MIR's
  locals as the front end lays them out, those that never hold storage at once sharing a slot as
  the code generator shares them (`FrameStorage::frame_storage_conflicts`), not clippy's sum of
  every local; the note names the largest. A body with a local of unknown layout is left alone.
- `significant_drop_in_scrutinee` (rule 9): a lock guard (std's, by its diagnostic item, or
  `lock_api`'s) returned by a call in a `match`, `if let` or `while let` scrutinee and used there
  in place (a borrowing or dereferencing method, a field, an index, `&`, `*`), so it lives through
  every arm. A guard moved on or matched on itself is not one; closures and blocks in the
  scrutinee are not looked in. For a `match` or `if let` statement of a scalar value the fix is
  `let value = ..;` first (maybe incorrect: the arms then run without the lock).
- `significant_drop_tightening` (rule 9): a named lock guard, used, then held through a call
  written in the source on a path where it is used on no path after, until its scope ends. Uses
  are read from the runtime MIR (the guard, references into it, call results that may borrow
  from it), with liveness across branches and loops, not clippy's statement walk; a guard never
  used (a critical section) is left alone. No fix.
- `await_holding_lock`, `await_holding_refcell_ref` (rule 9): a local an `async` body keeps
  across an `.await` (`MirBody::held_locals_in`, the front end's coroutine layout) holding a
  lock guard, or a `RefCell`'s `Ref`/`RefMut`, itself or in its arguments or fields. A guard
  dropped or moved away before the `.await` is not held; each `.await` it is held across is
  noted.

## `blaze_lints::fix`

`blaze_lints::fix::fix` makes every machine-applicable suggestion of a crate's findings, of the
lints the command line turns on. Each pass splices the fixes into the front end's trees with
blaze-intent's `Working::splice`, which reparses only what they touched, sets the new texts with
those trees seeded, and lints again, which recomputes only the bodies that changed. It ends
when no finding has a fix left (one pass for the `disallowed_*` fixes), and fails, naming the
file, line and lint, when two fixes still rewrite the same text after the last of 4 passes,
findings with a fix are left after it, or a fix's text does not fit the file. The crate's errors
are checked before and after: fixes that leave an error it did not have are refused, the texts
restored, and the error named with the lints whose fixes were made. `blaze check --replace
FROM=TO` is the same move of one path with no lint, through the same rewrite
(`blaze_intent::retarget`): the client (`crates/blaze-cargo/src/cli/replace.rs`) has the daemon
make blaze-intent's `Edit::Replace` in its front end and writes the edits to the checkout, and
checks nothing. The command that makes the lints' fixes is `blaze check --fix`
([below](#blaze-check-lints)).

## Baselines

A lint at `warn` whose findings are being moved off has a baseline,
`crates/blaze-lints/baselines/<tool>.<lint>.txt`: one `path<TAB>count` line per file with
findings, sorted. `blaze_lints::baseline::check` takes a run's counts (`baseline::counts` of its
findings) and names, by lint and file, each count over its baseline and each file the baseline
does not have; `baseline::bless` lowers the baselines to a run's counts, never raising one or
adding a file, and is only right for a run over the whole workspace. A file in the directory that
is not of a clippy or blaze lint blaze knows is an error. A lint whose baseline is empty moves to
`deny` and its file is deleted.

`blaze check --baselines DIR` (`crates/blaze-cargo/src/cli/baselines.rs`) counts the run's
`clippy::` and `blaze::` findings by lint and file, a finding that two units of a crate give (its
lib and its lib test) once, and fails naming each count over the baselines in `DIR` (from the
checkout's root). `--bless-baselines` lowers them to the run's instead, and `--new-baseline
LINT[,LINT...]` (given any number of times) starts each `LINT`'s at the run's counts, writing
none when any is not a lint blaze knows or has a baseline already. Both are only right for a run
over the whole workspace (`--workspace --all-targets`).

## `blaze check` lints

There is no `blaze clippy` and no `blaze fix` (Andrew, 2026-11-01): `blaze check` runs every
lint, rustc's, clippy's and blaze's, at the levels the manifests set, as `cargo clippy` with
clippy's driver would. The load (`crates/blaze-units/src/load/plan.rs`, `PlanInput::lints`) plans
each crate of the workspace's packages with `--cfg clippy` and the configuration file clippy's
search finds (`CLIPPY_CONF_DIR` or the package's directory, then up; the client sends those
above the checkout), all in `UnitPlan`, so the unit's record key has them. The unit's check
(`crates/blaze-cargo/src/serve/unit/compile.rs`, `linted`) then runs `blaze_lints::check` over
the crate with the unit's lint flags and that file, and puts what it finds before rustc's
closing lines, made again to count them. A registry crate is checked, not linted. The `-C
metadata` of a linted crate does not hash the lints as cargo's wrapper does, so its output files
keep the names a check gives them; that is names only: the unit's record key hashes its whole
`UnitPlan` (`work_out_key`), `--cfg clippy`, the configuration file's text and
`UnitPlan::clippy` included.

`--fix` (`crates/blaze-cargo/src/cli/fix.rs`) checks every target when none is named. The check
runs on the checkout's head pinned to its root, and every machine-applicable suggestion it gives
(of any lint or error; with `--lint NAME`, given any number of times, of those lints' alone) is
made as ops on that root: whole, inside the checkout, and not overlapping one made before it, a
suggestion two units gave once. The ops are checked again as a what-if view of the root, up to
four passes, each making the suggestions of the one before. An error with a primary span (code,
file, message) the first check did not have stops it with nothing written. Else the ops are
committed to the head, refused if the checkout moved meanwhile, and written to the files as
`--replace` writes. It says how many suggestions it made in which files and how many it left
(not machine-applicable, or outside the checkout), and fails when errors remain. A deny lint's
finding is an error the check fails on, and `--fix` still makes its suggestion. blaze reads no
version control. `--fix` with `--baselines`, `--lint` without `--fix`, and `--replace` with
either are refused.

## Done when

- `rg 'ide_db|RootDatabase|FamousDefs' crates/blaze-lints` finds nothing; every query takes
  `&Db`; no `SyntaxNode` handle is walked (one handle per query is held to borrow its tree).
- An edit of one body lints that body again and no other (a test over `Db::for_tests` with
  salsa's event log); a crate whose lints are all syntactic runs no `infer` (the same log).
- The four `blaze::` lints, each with hot_lint.py's patterns as fixtures, and the exemption
  comments.
- The 24 clippy lints give what they gave (the existing `tests.rs` fixtures, unchanged in
  expectation).
- The workspace's `[workspace.lints.clippy]` list checked against what is implemented; every
  lint it sets that blaze lacks either implemented or listed in the report.
