---
status: plan
summary: The driver of one command on the new item layer (crates/blaze-drive, crates/blaze-cli): a sans-IO request machine whose effects are store gets and puts, task runs and printing; the certificate first; tasks keyed from text that return their record or Need; one flight per key; validity by read list, checked forward from what changed through a reverse index kept in the answer pages, with no dependency graph in memory; dependencies as records only; run handed to blaze-run's Parent; findings placed when printed; a profile on every command.
date: 2026-11-15
---
# The driver

bottom.md section 9 says what the driver is; this is how it is built. Two crates:
`crates/blaze-drive` (`#![no_std]`, no `alloc`, sans-IO: blaze-own's collections, the allocator
passed in) and `crates/blaze-cli` (the `blaze` binary, the one place that touches the system,
through bstd only).

## The request machine

`Request::new_in(config, ask, alloc)`, then `step(event, out)`, which pushes effects into a
caller-owned vector. The caller performs each effect and feeds back its event; it may perform them
in any order and in parallel. The machine never waits.

| Event | Effect it answers |
|---|---|
| `Start { files, gone, last }` | (first) |
| `Got { key, bytes }` | `Get { key }` |
| `Ran { ticket, ran: Done(outputs) \| Need(wants) \| Failed(why) }` | `Run { ticket, task }` |

Other effects need no answer: `Put { key, bytes }`, `Print { .. }`, `WriteLast { bytes }`,
`Done(answer)`.

1. **Start.** The cli has swept the checkout (below) and hands every file as (path, content id,
   changed since the last answer), the files gone since, and the per-checkout `last` file. The
   machine computes the **certificate key**: the tool, the command and its arguments, and every
   (path, content id). One `Get`.
2. **Certificate stands**: its record holds the findings, each with its file and item start.
   `Print` each, `Done`. That is the whole repeat: one store read.
3. **Miss.** Get the last answer's certificate (its findings stand for what this run does not
   redo) and the last answer's root page. Then choose the walk:
   - *Forward* when `last` names the same tool, the same command shape and the same manifests:
     seed one `FileItems` job per changed or gone file.
   - *Full* otherwise (first run, a manifest edit, a new tool): seed the `Units` job; every part
     of every record is a job. Old rows still short-cut every job whose key did not move.
4. **Jobs** (one per name) advance as data arrives; a job that lacks a page, a name or a home
   waits on it by name, never by blocking.
5. **Finish** when no effect is outstanding and every job is settled: update the reverse index,
   write the touched answer pages and the root, the new certificate, `WriteLast`, then `Print`
   the findings sorted by (path, position) and `Done`.

## Names, sources and keys

A **name** is what a task is about: `(kind, subject)`. Subjects: a file's is the hash of its path;
an item's is its item key (identity.md); `Units`' is fixed. A **source** is the text the task is
keyed by, known before the work:

- `FileItems(file)`: the file's content id and the edition (from the part that listed it).
- every item task: its part digest in its file's `FileItems` record (signature text, body text,
  the module's declarations).
- `Fetch(crate)`'s parts carry the record key itself (a crate with no text here).

**Key** = H(tool, kind, subject, source). A key never holds another record's key (bottom.md 3);
what a task read of another record is its read list: (name, out digest) pairs.

Task kinds: `Units` (the workspace's crates: manifests' content), `Fetch(crate)`,
`FileItems(file)`, `ModuleScope(module)`, `Group(modules)` (a cycle of scopes, one task),
`Expansion(call)`, `Signature(item)`, `ImplBucket(bucket)`, `BodyCheck(body)`,
`Code(function, tier, hooks)`.

## Records and validity

The drive's envelope is one buffer: name, source, **out digest** (what a reader sees of it), read
list, parts (name, how to locate it, its start in the file), findings (rule, item-relative range,
message), and the kind's own payload (blaze-surface's record bytes, a checked body, code).

- A record with no reads is stored **whole** at its key.
- A record with reads is stored at its **value key** H(key, reads' digests), and a **shape** at the
  key lists the read names, so a lookup that knows only the key reads the shape, computes the
  value key from today's digests and reads that. Deviation from blaze-keyed's shape trie: a second
  read set under one key is stored at its own value key and found by its row, not by the shape
  (first writer of the shape wins; rule 3 holds because a shape names reads, it never answers).

**The answer table** is the checkout's index, stored as content-addressed pages under a root:
row per name, sorted by name, paged by the name's first bits (the root holds the bit count).
A row holds the key, where its record is, the source, the out digest, its home and start, its
read list, its parts, and its **readers** (the reverse index). Rows are records; nothing of them
is kept in memory but the pages a run touches.

**Forward check.** A job whose key equals its row's key and is not dirtied stands with no store
read. When a job settles with an out digest unlike its row's, every reader (the row's readers and
this run's new readers) is dirtied: re-decided at its value key from today's digests, run if
absent. A reader taken as standing before its read settled is corrected the same way, since it is
posted as a reader. Every job settles at most `most_entries` times. Nothing else is visited.

**Store reads designed** (p = answer pages touched, k = records the re-run task reads):

| case | reads | tasks |
|---|---|---|
| repeat, nothing changed | 1 (the certificate) | 0 |
| one-body edit | 4 + p + 2 + k: certificate miss, last certificate, root, p pages, the file's new `FileItems` key, the body's new key, its k inputs | `FileItems` 1, `BodyCheck` 1 |
| manifest edit | Full walk: one read per page and per re-made record | the moved ones |

The leaf table is a file of the checkout, not a store read. Floor for the edit: the two task runs
and k reads; the extra 4 + p are a certificate, an index root and its pages (ratio 1.0 on tasks).

**The early record after a restart** (adapted from Relax's `check_reuse.rs`): the repeat path
loads nothing but the `last` file, the leaf table and the certificate. Its key is computed from
the leaf table's content ids, which the stat sweep proves current without reading a source, so
with the store as target.md's segments it touches **3 pages**: the leaf table's (one per 4 KiB of
it; one for hello-world, about 40 for reth's 4,000 files), one page of the segment index (the
probe for the certificate's key) and the certificate's own page (findings inline; a certificate
past one page spans more). Nothing else is mapped before the findings print.

## Tasks and Need

A task is a pure function of its inputs: `Done(outputs)` or `Need(wants)`. A want is a name and
its **home** (the record that lists it: its file's `FileItems`, or `Fetch(crate)`). The drive
makes what is wanted and enters the task again from the top with the inputs it lacked. Rules:

- A job's old read list is its **hints**: its inputs from the first entry.
- **One flight per key**: a name has one job; a job dirtied while running is redone after.
- A job entered more than `most_entries` times is `DriveError::Reentries` naming its name, key and
  last needs.
- A wait cycle among `ModuleScope` jobs becomes one `Group` task; any other cycle is
  `DriveError::NeedCycle` naming its names.
- Work is parallel in the cli's pool; the drive keeps at most `workers` runs outstanding and
  orders the queue by kind (files, then scopes, signatures, bodies, code).
- **Two classes** (adapted from Relax's `cpu_budget`): a task is `Foreground` (what the command
  was asked) or `Background` (speculative code, the optimizing tier). A background task is
  emitted only when no foreground task is queued and fewer than `workers` runs are out, so the
  person's work never waits behind it; `workers` is the machine's (one code path, 128 cores to a
  Mac).
- An output for a name no job asked for, or a read whose digest is not today's, is a typed error.

## Dependencies and std

A crate with no text here is records only: its `Fetch(crate)` record lists its parts by record
key; a missing record is `DriveError::Missing { name, key, builder }`, the builder being
`Builder::Pack(crate)` (fetch blaze-pack's pack) or `Builder::PrepareStd` for the sysroot.
`blaze prepare-std` is the same request over `library/` with the sysroot crates as text crates:
their tasks run here once, filling the local store; later requests find them by key and never
read std's text. CI runs the same command and publishes the pack.

## Run

`run` checks what `check` checks (until reachability from the entry is built), then hands the
unit record to the cli, which is blaze-run's `Parent` for the program's life: `Compile` and
`Make` become drive `Code` names (`run::code_name`), answered as tasks. Today `Code` answers
`NotBuilt`, so `blaze run` stops with that error, by name.

**Tests.** A test's result is a `Test(test)` record whose read list is the code identities its
run entered (the run side's entry marks). It is a job like any other: after an edit, a test whose
read list holds a changed function is dirtied by the forward check and runs again; every other
test's record stands and its verdict is printed marked as reused. The answer counts both
(`Answer::tests_run`, `tests_reused`). blaze-run's `Tests::affected` is the same postings as a
run-side index; the drive's readers column is the store form of it.

## The system side (blaze-cli)

- **Sweep.** Behind one trait (`sweep::Sweeper`), so blaze-solo's parallel stat sweep (its lane)
  plugs in: it answers every file's (path, content id, changed) and the gone ones. Today's
  implementation is bstd's `Dir::list` and a read and hash of every file: bstd's `Meta` has no
  stamp, so Relax's stat rule cannot run yet (gap). The leaf table is written after `WriteLast`, so a
  crash between them reports more files changed, never fewer.
- **Store.** One file per record under `.blaze/v2/records/` written whole: target.md's segment
  store over bstd does not exist yet (gap).
- **Units.** A small reader of `[package] name`, `edition` and the default targets; anything else
  in a manifest is refused by line (blaze-manifest is std and third-party).
- **Printing.** `file:line:col` from the file's line table at print time (`lines.rs`).
- **Profile.** Per kind: standing, found, made, fetched, entries, time; gets, hits, puts, pages.
  Written to `.blaze/v2/profile.txt` and summarised on stderr.
- `export` and `build` say they are not built.

## Not decided here

- The certificate's read list (bottom.md 9 keeps files in it; here files are in its key, so a
  changed file is a miss, then forward). Equal in reads on a repeat; to revisit with FSEvents.
- Pages by name prefix against pages by module: measure on reth.
