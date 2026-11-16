---
status: current
summary: How to run identity-census.astlog, the work list for the item-identity change, and how to read its rows.
date: 2026-11-15
---
# identity-census

The query behind step 1 of [the identity design](../../../docs/v2/plan/design/identity.md): every
site in blaze's Rust that depends on an item id being an arena number. It only reads; the
rewrites come after its rows are read.

## Run

From the checkout's root, with a blaze whose daemon views this checkout:

```sh
blaze check --query tools/astlog/rules/identity-census.astlog --message-format json > census.jsonl
```

`--query` reads the whole checkout and refuses `-p` and other selection flags
(astlog-sema.md, "Not built"), so the per-crate split is in the rows: the `census` relation is
`(crate, lint, count)`, and every finding's place names its file. Without
`--message-format json` the findings print one per line with their place.

It types every body that has a candidate site and expands every macro call (the `{:?}`
census reads `format_args!`'s expansion), so expect a full-workspace check's time. A run
holds at most 10,000,000 rows (`MAX_ROWS`); a stop naming a relation means that relation
needs a tighter first atom.

## Read

`Response::Rows` holds every relation, the helpers too (`holder`, `seen-type`, `ty-part`, ...).
Read the findings and `census`; the helpers are there to check a row's why.

| Lint | Row | Under the design |
|---|---|---|
| `id-bits` | a call reading an id's or a `Gid`'s bits; `what` the method, `via` the type or the blaze-memo fn that made it a row | records and carried tables go through `blaze_id::key_of`/`resolve`; in-process code keeps the id |
| `id-cast` | an `as` cast whose operand reads an id; `y` the id-typed part | as `id-bits` |
| `id-order` | a sort, search, cmp/min/max, comparison, BTreeMap/BTreeSet key or derived `Ord` over ids | order by item key, or a hash map |
| `id-in-record` | a Record, Persist or serde impl or derive on a type holding an id; `id-in-record-field` each such field | the field becomes an `ItemKey` |
| `id-in-carried-table` | a field of shared type (lock, papaya or dash map) or a `static` holding an id | needs a human read: keyed by `ItemKey` if it outlives a snapshot |
| `id-in-table` | a plain map or Vec field holding an id | a human read, as above, of its owner |
| `id-debug-in-output` | a `format_args!` argument holding an id under a `{:?}` template | print the path or key |

Each has a `-in-test` twin (except `id-in-table`): test code is under `#[cfg(test)]` or a test
attribute, or in a file named `tests.rs`/`*_tests.rs` or under `tests/`/`benches/`.

"Holds an id" is wider than "is an id": a holder is any struct, enum, union or module-level
alias whose field types name an id or another holder at an owning position (not behind `&`,
`*`, `fn` or `dyn`), so a `hir::Function { id: FunctionId }` and a key struct with a
`DefWithBodyId` field count. A row's `d` names the holder it came through; a long chain means
the id is far away and the row may be fine.

## What it cannot say, and how each is approximated

- **"Outlives a snapshot."** No relation follows ownership from a static, the store or the
  database to a field, so `id-in-carried-table` lists every shared table holding an id and
  `id-in-table` every map and Vec. Each row needs a human read.
- **A field's type.** `type-of` takes expressions and patterns only, so fields are read from
  their written type by name resolution: a generic struct counts where it is written with an
  id, and a type that holds an id only through a parameter filled elsewhere is missed.
- **Peeling.** `holds-id` peels references, Option, Result, Box, Vec, VecDeque, SmallVec,
  ThinVec, slices, arrays, tuples up to four, the std, blaze_hash, papaya and index maps and
  sets, Arc, Rc, Cell, RefCell and the locks, plus any holder ADT. It does not peel raw or fn
  pointers, `dyn`, closures, iterators or other generic ADTs.
- **Relations over types are demand-driven.** `item-id-type` and `holds-id` hold only over
  `seen-type`, the types at the census sites and what peeling reaches.
- **Calls.** A turbofish callee (`from_gid::<T>`) and a sort key given as a fn path are
  missed. A BTreeMap is found by its written name, so a fully qualified spelling and an
  inferred one are missed.
- **Debug output.** Only the first inline capture (`{id:?}`) of a template is read, matched to
  a binding of that name in its fn with shadowing ignored. `tracing` fields (`?id`), `dbg!`
  and a Debug impl that forwards an id are missed.
- **Derives.** A derive's impl is not expanded for queries, so `#[derive(Serialize|Record|Ord
  ..)]` is matched by its text.

## Paths it pins, and what to do if one fails to load

The ids are pinned by def path (`hir_def::FunctionId`, ..., `base_db::Crate`,
`blaze_memo::Gid`), and so are the record traits (`blaze_format::Record`,
`blaze_format::Persist`, `serde::ser::Serialize`, `serde::de::Deserialize`). A path that names
nothing, or a crate name two crates of the build share, stops the run naming the path and
the line. The serde ones are the likeliest; drop those two `record-trait` rules and the serde
derives are still found by the derive rule.
