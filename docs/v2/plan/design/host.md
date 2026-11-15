---
status: plan
summary: The host of the keyed driver. blaze-keyed's driver decides what a request does (certificate, then a mark per unit, then records found by what they read); blaze-host performs its effects on a real store and pool with a Front it is given. This page says the three crates (blaze-unitkey, blaze-front, blaze-host), what each task reads and writes, and each operation's floor.
date: 2026-11-13
---
# The host: one command drives the keyed driver on real code

Owner: the main agent. Part of `target.md` (its Order, step 2). Records are read-addressed
(decision D39, `read-addressed.md`). Every number is an **estimate**; nothing here is measured.

## What exists (read in the tree, 2026-11-14)

- `crates/blaze-keyed`: the driver of one request (`driver.rs`), the kinds and their keys
  (`kind.rs`), records, fact ids and their routing (`record.rs`), the `Front` seam and the one
  answer (`front.rs`), the store, pool and executor interfaces, the memo policies (`RecordsOnly`,
  `Resident`, `Audit`), and a simulation that holds it to a from-scratch build with a model that
  reads by value and a control per planted bug.
- `crates/blaze-host`: performs every effect on `blaze_keyed::store::Store` (blaze-cas's plain
  store in its tests) and blaze-graph's pool, with a scripted `Front` in its tests.
- `crates/blaze-cas/src/plain`: the store. `crates/blaze-unitkey`: `unit::key`, a unit's keys
  from text.
- Not built on main yet: `crates/blaze-front`, the surface and body tasks over the ported front
  end, on this seam.

## The rule everything below follows

A body is checked against **records, never against a crate built in memory**. Every crate a body
task reads, its own included, is read through that crate's `Surface` record, mapped and used in
place. The only task that builds a crate's scopes in memory is that crate's own `Surface` task,
and it runs only when its key or a fact its shape read moved. So after a private body edit the
process reparses one file, keys the unit, maps surfaces and checks one body; it never rebuilds a
def map.

## Crates

| Crate | IO | Does | Depends on |
|---|---|---|---|
| `blaze-unitkey` | none | `Task::Key`: file images and every key of a unit | parser, syntax, cfg, blaze-keyed |
| `blaze-front` (not built on main yet) | none | `Task::Surface`, `Task::Body` | the ported front end, blaze-item-hash, blaze-items, blaze-keyed |
| `blaze-host` | yes | the process: performs every effect, with a `Front` it is given | blaze-keyed, blaze-graph, blaze-vfs |

`blaze_keyed::front::Front` is the seam, in blaze-keyed so neither side depends on the other:
`key` (gives `Keyed`: the text key, the surface key, the unit's files with their items' keys),
`surface` and `body` (each gives a record written with `record::write_read`: its shape with the
digests seen, the facts it made, its payload, headed by its address). A task asks a fact by
`Universe::answer`, a binary search in the mapped tables it is handed; there is no `now`, and
the host answers the driver's asks the same way. blaze-unitkey and blaze-front export plain
functions; the adapter that implements `Front` with them is a few lines in the command.

## `blaze-unitkey`: a unit's keys from text

- `image(tool, edition, bytes)` writes a `FileImage` record: tokens, the flat tree, each item's
  piece hash, its names, the line table. Its key is the bytes, so an unchanged file is never
  parsed.
- A unit's keys: its text key (`UnitInputs`: its file images and late values), its surface key
  (`SurfaceInputs`: its pieces), and per item its body key (`BodyInputs`: its own text). What a
  name resolves to is in no key: it is read, so it is in the record's shape.
- Module files: the image lists a file's out-of-line `mod` declarations with their `#[path]` and
  `#[cfg]`; the caller reads those files and comes back. No file is read in this crate.
- **Floor.** A comment edit: one file reparsed, no other key computed again. A body edit: one
  file reparsed and one body key.

## `blaze-front`: the tasks over the front end (not built on main yet)

- **`Surface` task.** Runs the unit's scopes, signatures, fields, layouts, impl index and const
  values, and writes one `Surface` record: `facts`, one row per exported fact (ids from
  `record::fact`, stamped with the unit's owner, sorted, so a lookup is a binary search in mapped
  bytes) and the seen row (what the unit sees as a whole); its shape, each fact of another
  surface it asked, with its digest.
- **`Body` task.** One body: its item's syntax from the file image, inference, MIR, the
  borrow-check verdict, item-relative diagnostics. Each fact it asks is answered by
  `Universe::answer` and lands in its shape; a fact it makes (a const's value, an opaque type's
  hidden type, what escapes the body, its effects) is in its `facts`, which the driver lists in
  the unit's manifest root with its maker.
- **Floor.** A body task: the body's own check plus one mapped lookup per fact it asks. A
  surface task: the crate's own scopes once, one lookup per fact it asks of a dependency.

## `blaze-host`: the process

- Runs the driver to its outcome: `Get` (pread a flat record: a body, a shape node, a mark, a
  chunk, a manifest root, the certificate; map a surface), `Outside` and `Reads` (the
  certificate's asks, from the leaf memory), `Answer` (`Universe::answer` over the tables the
  effect names: each surface mapped, each manifest root's made table read once per request, the
  request's late reads), `Run` (the same tables handed to the task; a no-op or a one-body edit
  runs on the calling thread), `Put` (one commit, before the certificate's value; at the end when
  no value is filed).
- Every wait is a span naming what it waits for, through blaze-core's one deadline wait. A
  panicking task fails that task. Workers start in proportion to queued tasks.
- **Floor.** A no-op: the leaf table's sweep and one certificate lookup. A unit settled by its
  mark: one read. Otherwise the tasks' own time plus one store read per record asked and one
  binary search per fact answered. Above it: a vector of table views per answer or task, and
  each made table copied once.

## Checks

- blaze-unitkey: a comment or whitespace edit moves only the file image's key; a private body
  edit moves exactly that body's key; a signature edit moves the surface key and that item's body
  key, nothing else.
- blaze-front (when built): a record's shape replays (every fact's answer equal on the same tree); one planted
  missing ask per fact kind is caught by the audit.
- blaze-host (`crates/blaze-host/src/tests.rs`), the real loop on a real store with a scripted
  `Front`, each with its control: cold and no-op; a body edit runs one task and marks the unit
  above; a dependency fact nobody read runs no task above it; a signature edit redoes exactly the
  readers its postings name; an undo finds its records by address; a store with no certificate
  settles every unit through its mark; `Audit` over each finds no difference and catches a front
  end that is not deterministic; a panic and a failed sync fail by name; any edit sequence files
  what a fresh build files.
- The whole: hello-world, then blaze, reth and tempo, by the smoke tool on the build VM.
