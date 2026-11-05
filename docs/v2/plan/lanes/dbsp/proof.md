---
status: plan
summary: Lane dbsp's proof on blaze's own builds and traces: edit sequences built incrementally and from nothing by blaze, the cost of one impl, and what a build after each kind of edit may rerun.
date: 2026-10-30
---
# dbsp: the proof

Design section 12 ([../../design/dbsp.md](../../design/dbsp.md)) lists what proves the
maintained resolution, impl tables and trait candidate sets. Three of its rows are checked from
outside blaze, by one harness and one module of the oracle:

| check | where | what it proves |
|---|---|---|
| the same as from nothing | `harness/v2/dbsp/edits.py` | after every edit of a sequence, blaze's incremental build says what blaze's build of the same files from nothing says |
| one impl costs its facts | `edits.py`'s cost check | design section 10's counts hold on every project, whatever its size |
| a reader sees only its slice | `crates/blaze-v2-proof/src/dbsp.rs` | after each kind of edit, the build reran only what that edit can reach |

## Edit sequences, incremental against from nothing

`edits.py` takes a project and an edit script. A project is the oracle's fixture workspace
(`harness/v2/fixture.py`), a project of `harness/blaze_cli.py`'s `PROJECTS`, any cargo
workspace, or a directory of cases (`harness/v2/frontend/cases/`). An
edit script is generated from a seed or saved in `harness/v2/dbsp/scripts/`. The script's
edits change a scaffold appended to one file of the project, between two marker lines. The
scaffold holds modules (inline, or files of their own), structs, traits with methods, aliases,
`use`s, globs, impls, `macro_rules!`, derives and probe functions. It names nothing of the
project, so the same script runs on any of them.

blaze builds every step in one workspace under one daemon, so each step after the base goes
through the maintainers. Each step is also copied into a workspace of its own and built from
nothing: a daemon with an empty home and a store holding only the standard library
(`oracle.std_seed`). The two are compared (`edits.compare`): accepted or rejected, the keys of
the errors (`cases.key_of`), and the program's run where there is one. Since 2026-10-30 no step
is built by rustc or cargo (Andrew: no harness compares blaze with them). About one edit in four
breaks the program on purpose (a duplicate impl, a dangling path, a derive missing what it
needs, a method still called removed), so the reject paths are compared as well as the accept
paths.

The first step that does not match ends its script. The script is then shrunk (`edits.shrink`,
edit lines deleted) to the fewest edits that still give a mismatch of the same signature, and written to
`scripts/`, where every later run replays it on every project.

What it proves: for the edits it makes, the incremental path has the same observable result as
a build from nothing, including the steps where the maintainers have to retract (removed impls, removed
modules, renames, repointed `use`s). What it does not prove: anything about edits outside the
scaffold's vocabulary (attribute macros, proc macros, `cfg` changes). Those are design section
12's first row, the in-crate random tests.

## The cost of one impl

After each `add_impl` step, the daemon has to print one line per crate, maintainer and write,
of the form `{"reason": "blaze-maintained", "write", "maintainer", "crate", "work": {"ops",
"woken", "rebuilt", "region", "slices", "keys"}}` (`blaze_dbsp::Work`). The check sums the lines
over the request's writes. It then requires that, for the edited crate, each maintainer applied
one op and set at most two slices, and that no other crate did any work. A step that prints no
such line fails and names the requests that add it (queue 00819 for the line, 00820 for the
per-write records). The check is never skipped. Running it on every harness project is how the
"1000 modules cost what 10 do" claim of section 10 is tested on real crates.

## What a build after an edit reruns

With `"trace": true`, the daemon's trace is taken after every step. Each pair of traces, before
and after an edit, goes to `blaze-v2-proof '{"dbsp": KIND, "traits": T, "buckets": B, "before",
"after"}'`. `KIND` is the kind of the edit: `impl`, `derive`, `alias`, `repoint`, `method`, `mod`
or `item`. `T` and `B` are how many traits' impl sets, and how many (trait, self type) buckets,
the edit changed, by `edits.py`'s model of what it wrote. The case of that kind requires that:

- no round query reruns, whatever the edit (the round driver is what the maintainers replace);
- a trait's slice reruns at most `T` times and a bucket at most `B` times, and no inherent
  slice reruns: after an impl edit, no query of another trait's slice reruns;
- no sliced query reads a whole crate's impl table or a module's impl list;
- no resolver or slice query reruns though nothing it read changed, or reruns to read and make
  what it did before: after an item edit, no resolver query of another module reruns.

A rerun is a key that ran after the edit and has a result in the build before it. A key first
asked after the edit is not one. A case whose kind neither trace holds (for impl edits, the
trait slices; for the rest, the modules' declarations) is unmeasured, never a pass.
`dbsp.rs`'s unit tests hold each rule to small traces, and check that its cases are exactly
`edits.py`'s edit kinds.

## How the coordinator runs it

- Selftest (no blaze): `uv run --frozen python harness/v2/dbsp/edits.py selftest`.
  It checks that the generator is deterministic per seed and covers every op and kind, that the
  rendering and the model are right, that the shrinker reduces a planted mismatch, the
  comparison of two builds and a mismatch's signature, the cost check, the saved
  scripts, and that the trace cases match.
- The oracle's tests: `blaze test -p blaze-v2-proof` (the `dbsp` module's).
- A run: `harness/v2/proof.py` runs it as the check `dbsp` with its defaults (the fixture, three
  generated scripts and the saved ones). For more, use `uv run --frozen python
  harness/v2/dbsp/edits.py '{"engine": {"kind": "v2", "blaze": "..."}, "out": "/tmp/edits.json",
  "projects": [{"kind": "fixture"}, {"kind": "harness", "name": "ripgrep"}, {"kind": "cases",
  "path": "harness/v2/frontend/cases/nameres"}], "scripts": 20, "trace": true}'`.

Until the lane's maintainers and the `blaze-maintained` line exist, the cost check fails on
every `add_impl` step, and the trace cases fail on the round queries. Both failures are
expected and say what is missing.
