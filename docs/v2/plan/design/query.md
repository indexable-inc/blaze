---
status: plan
summary: blaze's own query engine for its build profile, so `blaze debug sql`, `blaze explain`, `blaze slow` and `blaze why` need no DuckDB. SQL text goes through blaze-sql's parser (its analytic grammar) to a small logical plan of relational operators and a fixpoint, then a columnar executor over a `Table` trait (Parquet today, blaze's records later). Crate crates/blaze-rel; the census of the workload; bounds; determinism; what a datalog front end would take.
date: 2026-11-15
---
# blaze-rel: the profile's query engine

## Why

Every why-question about a build is answered from the profile's tables (`debugging` skill,
rule 17). Today that means an external DuckDB 1.5.5 CLI, fetched by hash, spawned per command,
its answer read back as Arrow (`blaze_profile::sql`). The owner wants nothing from outside. The
workload is small and fixed: `crates/blaze-profile/sql/views.sql` (42 views) and `examples.sql`
(53 named queries), plus SQL strings inside blaze-profile's Rust. A build's profile fits in
memory. So the engine is a few thousand lines, not a database.

## The pipeline

```
SQL text ──blaze_sql::parse::parse_query──▶ query::Statement (blaze-sql's tree, analytic grammar)
   ──blaze_rel::plan──▶ Plan (relational operators + Fixpoint, names bound to column indices,
                        every expression typed)
   ──blaze_rel::exec──▶ Rel (typed columns) ──▶ Rows (blaze_data::Value) ──▶ text table | JSON lines
```

- **One parser.** blaze-sql's recursive-descent parser gains a second grammar, `Grammar::Query`
  (`parse::parse_query`, `parse::parse_script`): the same lexer and expression parser, plus
  comments, `::`, `@name`, `WITH [RECURSIVE]`, set operations, derived tables, the join forms,
  `FILTER`, `OVER`, `QUALIFY`, `GROUP BY ALL` and subqueries. Its tree is `blaze_sql::query`;
  its expressions are `ast::Expr` with one more variant, `ExprKind::Analytic`, which the portable
  subset's checker refuses by name (`TypeErrorKind::NotInSubset`). `Grammar::Subset` is
  unchanged: the typed surface (sql.md) still refuses comments and every construct outside it.
- **Binding is planning.** blaze-sql's `infer` types the portable subset against SQLite and
  Postgres storage; the engine types against its own columns (below). The planner resolves names
  to column indices and gives every expression a type in the same pass, so there is one binder
  for the engine and no second typer of the subset. When the typed surface takes subqueries, its
  `Select` folds into `query::Core` (one tree), noted as the follow-up in sql.md's "Later".
- **Physical plan.** The logical plan is executed as written, with the strategy of each operator
  chosen as it runs: a join with equality keys is a hash join (built on the side without the
  fixpoint's working table, else the right), any other join a nested loop; aggregation is by hash;
  sort is a stable index sort. Pipelining between operators is the next step; every operator
  materializes its output today.

## The plan IR (`blaze_rel::plan`)

| Node | What |
|---|---|
| `Scan` | a base table by index; conjuncts `col op number` checked against each chunk's min/max statistics to skip the chunk, then the whole pushed filter row by row; every column is read (pruning to the columns the query names is not built yet) |
| `Filter` | rows where a predicate is true (null and false drop) |
| `Project` | scalar expressions over the input's columns |
| `Join` | inner, left, full, semi, anti, cross; equality keys (each null-safe or not: `IS NOT DISTINCT FROM` is a key that matches nulls) and a residual predicate |
| `Asof` | DuckDB's `ASOF [LEFT] JOIN`: equality keys plus one inequality; each left row meets the nearest right row |
| `Aggregate` | group keys and aggregate calls (below), `FILTER`, `DISTINCT` and an `ORDER BY` inside the call |
| `Window` | functions over a partition and order with a frame; appends a column each |
| `Sort`, `Limit` | stable; `NULLS LAST` by default for both directions (DuckDB's) |
| `Distinct`, `Union` | `UNION ALL`, `UNION` (distinct), `UNION ALL BY NAME` |
| `Shared` | a CTE or view, planned once and materialized once per query, read by every reference |
| `Fixpoint` | `WITH RECURSIVE`: a base plan and a step plan that reads the working table |

Expressions: literals, columns, parameters (`$name`, bound by value), the arithmetic, comparison,
`||`, `AND/OR/NOT` (Kleene), `IS [NOT] NULL`, `IS [NOT] DISTINCT FROM`, `[NOT] IN (list)`,
`BETWEEN`, `LIKE`, `CASE`, `CAST`/`::`, scalar functions, uncorrelated scalar subqueries and
`IN`/`EXISTS` subqueries (computed once before the operator that reads them). A correlated
`[NOT] EXISTS` whose correlation is equalities is planned as a semi or anti join; any other
correlated subquery is refused by name.

**The fixpoint** is evaluated semi-naively, which for SQL's linear recursion is the definition:
round 0 runs the base; round k+1 runs the step with the working table bound to round k's new
rows only; `UNION ALL` appends them, `UNION` keeps those not already in the result; it stops at
the first round with no new rows. Everything in the step that does not read the working table
(a join's other side, its hash table) is computed once, before round 1: an arrangement in
blaze-dbsp's sense, kept for the fixpoint's life. Two bounds stop it with a typed error naming
the CTE (rule 18): `Bounds::rounds` (default 1 000 000: `critical_path` walks up to 100 000
steps, one row a round) and `Bounds::rows` across all rounds (default 16 Mi). blaze-dbsp's
Z-sets and DRed closure are for keeping a result up to date under deltas; a query here runs once
over a fixed profile, so the engine takes its idea (the delta and the arrangement) and not its
types.

## Types and NULL

The engine's types are `Bool`, `Int`, `Float` and `Text`, every one nullable, and the `Null` of
an untyped `NULL`. schema.rs's `U16 U32 U64 I64` are `Int`, `F64` is `Float`, `Utf8` and `Dict`
are `Text`. `Int` is one exact integer of i128 range, stored as `i64`, `u64` or `i128` per
column as its values need, so `UBIGINT - UBIGINT` that goes below zero is a negative number, not
DuckDB's overflow error (rule 5: useful, not parity), and `CAST(x AS HUGEINT)` is a no-op. `/`
always divides as floats (DuckDB's, and the workload's `/ 1e6` and `verified / greatest(..)`
rely on it); integer `+ - * %` past i128 is an error naming the expression. Text is interned per
query: a symbol a value, so text equality and hashing are integer operations; order is by bytes.

Three-valued logic: a comparison with a null is null; `AND`/`OR`/`NOT` are Kleene's; a filter,
`QUALIFY`, `HAVING` and a join's condition keep only true; `x IN (.., NULL)` that matches
nothing is null; join keys never match null unless the key came from `IS NOT DISTINCT FROM`;
aggregates skip nulls, `count(*)` counts rows, `sum`/`min`/`max`/`avg` of no non-null value are
null and `count(x)` is 0; `greatest`/`least` skip nulls (DuckDB's). `GROUP BY` puts nulls in one
group; `DISTINCT` and `UNION` treat two nulls as equal.

Lists (`Stack`, `Pcs`) are not a type yet; `array_to_string(stack, ..)` and `unnest(range(..))`
are refused by name (the acceptance list says which views).

## The seam

`blaze_rel::run(sql, &catalog, alloc) -> Result<Rows<A>, QueryError>` runs one statement over a
`Catalog` of base tables (each a `dyn Table`, added with `Catalog::add(name, &table)`);
`run_with(sql, &session, &catalog, alloc)` adds a `Session`: the views of a `Prelude`
(`Prelude::parse(views_sql)`, which reads `CREATE [OR REPLACE] VIEW` statements; a view that is
`SELECT * FROM read_parquet(@t, ..)` is the base table `t` itself, so filters reach its scan),
the `$name` parameters, and the `Bounds` (`Bounds::PROFILE` by default).
`Rows::write_table` and `Rows::write_json` print what `blaze debug sql` prints today (the header,
a rule of dashes, two spaces between columns, trailing spaces trimmed, `N rows`; one JSON object
a line). A `QueryError` names the statement (its first line, or the view's name), the byte
offset, what was refused and the rule.

`trait Table`: a name, columns (name, type, nullable), and chunks (a Parquet row group, a
record); a chunk gives each column as typed slices (`&[i64]`, `&[u64]`, Arrow-style offsets and
bytes for text, dictionary keys and values) with an optional validity bitmap, and optional
min/max/null-count statistics. `MemTable` is the in-memory one the tests use; the blaze-parquet
reader and blaze's records implement the same trait.

Call sites that switch, each by replacing a DuckDB spawn with `run` over the history's tables:
`blaze-cargo/src/cli/observe.rs` (`run`, `rows_json`, `print`/`table`/`json`: `blaze debug sql`,
`blaze explain`), `blaze-profile/src/sql.rs` (`run`, `run_with`, `run_all`, deleted after),
`gate.rs` (5 calls), `locks.rs:392`, `diff.rs:204`, `time/load.rs` (2), `slow/load.rs:216`,
`memory/tests.rs:278`, and `views.rs` (`render` binds arguments as literals today; with the
engine they bind as parameters and `$no_build`'s `error()` stays). DuckDB's pin and fetch
(`cli/duckdb.rs`, `DUCKDB_PINS`) go last.

## Census of the workload

95 statements: 42 in views.sql (31 of them `SELECT * FROM read_parquet(@t, union_by_name =
true)`) and 53 in examples.sql. Counts are occurrences (statements in brackets). The parser
before this change had: `select`, `distinct`, inner and left `join .. on`, `where`, `group by`,
`having`, `order by` (no `nulls`), `limit`, `offset`, `case`, `cast` to five storages, `in
(list)`, `between`, `like`, `is [not] null`, `is`, and 10 functions.

| Feature | Count | Had |
|---|---|---|
| CTE (`WITH`), of which `WITH RECURSIVE` | 105 bodies in 29 [29], 3 [3] | no |
| scalar subquery / derived table / `IN (SELECT)` / `EXISTS` | 31 / 13 / 3 / 5 | no |
| `FILTER (WHERE ..)` on an aggregate | 78 [13] | no |
| `UNION ALL` / `UNION` / `UNION ALL BY NAME` | 33 [13] / 1 / 1 | no |
| window `OVER (..)`, with a `ROWS BETWEEN` frame | 17 [11], 3 | no |
| comma join / `CROSS JOIN` / `USING (..)` | 21 / 9 / 7 | no |
| `ASOF [LEFT] JOIN` / `FULL JOIN` | 7 [6] / 1 | no |
| `CAST(.. AS VARCHAR/BIGINT/UBIGINT/HUGEINT/DOUBLE)`, `::` | 88 [23], 3 | 5 storages only |
| `NULLS LAST/FIRST` / `IS [NOT] DISTINCT FROM` / `QUALIFY` / `GROUP BY ALL` | 9 / 4 / 1 / 1 | no |
| comments inside a statement, `@table`, `CREATE OR REPLACE VIEW` | most / 31 / 42 | no |
| `GROUP BY`, `ORDER BY`, `LIMIT`, `LEFT JOIN`, `JOIN`, `CASE`, `IN`, `LIKE`, `BETWEEN` | 55, 76, 81, 23, 23, 18, 15, 12, 7 | yes |
| `$param` | 130 [53] | yes |

Functions: `max` 91, `round` 84, `sum` 68, `coalesce` 67, `count` 34 (`count(*)` 25, `DISTINCT`
8), `greatest` 22, `any_value` 20, `min` 14, `least` 13, `arg_max` 10, `starts_with` 8,
`string_agg` 7 (with `ORDER BY` inside), `nullif` 5, `lead` 4, `array_to_string` 4, `row_number`
3, `median` 3, `avg` 3, `ends_with` 2, `arg_min` 2, `lag` 2, `abs`, `floor`, `split_part`,
`rank`, `replace`, `unnest`, `range` 1 each; and `error`, `concat` from the argument defaults
(views.rs). blaze-sql's function table (`funcs.rs`) stays the subset's; the engine's functions
are its own (`blaze_rel::plan::Func` and `AggFunc`), each with its NULL rule.

blaze-sql gains (this change): the `Grammar::Query` lexer mode and parser, `query::{Statement,
Query, SetExpr, Core, FromItem, Join, Order, Window, Frame, TypeName}`, `ExprKind::Analytic`, and
`TypeErrorKind::NotInSubset`. Nothing the subset accepted changes meaning.

## Memory

A query runs in one allocator, its frame (`run`'s `alloc`). The assumed size: a build's profile
of up to 4 Mi span rows at about 200 bytes a row in memory, ~1 GiB, with every operator's output
materialized; a typical edit build's profile is 10-100 times smaller. `Bounds::rows` (16 Mi rows
a relation) and `Bounds::text_bytes` (1 GiB of interned text) stop a query past them with a typed
error naming the operator, never a swap storm; an allocation the frame refuses is blaze-own's
`AllocFailure`, returned. Past the assumption the answer is the Table trait's chunks: the scan
already skips chunks by statistics, and the next step is streaming filters and aggregates chunk
by chunk so only the hash tables and the joined rows are held.

The floor (rule 67): reading each referenced column once. A scan copies the columns of the rows
its filter keeps (every column until column pruning is built), so a selective query is near the floor; an expression is interpreted
row by row (about 10-30 ns a node a row, measured on nothing yet: the first real-run profile
names the ratio). DuckDB's floor includes spawning a process and writing Arrow (~100 ms).

## Determinism

Equal inputs give equal output bytes, always. With `ORDER BY` the order is the keys', ties in
input order (a stable sort). Without it the stated order is: a scan in chunk and row order; a
filter, project and window keep their input's order; a hash join emits probe rows in order, each
probe row's matches in build order, then (left/full) its unmatched rows in place, then (full) the
unmatched build rows in build order; an aggregate's groups in order of first appearance; `UNION
ALL` its inputs in turn; a fixpoint its rounds in turn. No hash seed or address reaches an
output (rule 25): hash tables only find candidates, and iteration is always over row vectors.

## A datalog front end (not built)

A rule `reran(q, root) :- provenance(q, _, k), reran(k, root).` is a join of `provenance` with
`reran` on `k`, projected to `(q, root)`; several rules for one head are a `Union`; a recursive
head is a `Fixpoint` whose step is the recursive rules with the head's working table in place of
its own name, which is exactly the plan `WITH RECURSIVE` makes. It would take a parser (atoms,
variables, `:-`, negation as `not`), a binder that turns a shared variable into an equality key
and a constant into a filter, stratification (negation of a recursive head is refused, as SQL
refuses it) and mutual recursion (one `Fixpoint` over several working tables: the executor's
loop runs them all a round). Plans, executor, bounds and output are shared unchanged.

## How it is proven

- Each operator: hand-written tables and expected rows, null cases included (`tests/ops.rs`).
- The refinement: on random small tables (blaze-sim), the hash join equals a nested-loop join
  and the hash aggregate a sort-based one, as multisets and in the stated order; the control
  plants null-equal join keys (`Plant::NullKeysMatch`) and must be caught.
- The recursive chain of the debugging skill ("walk the chain by hand") over a provenance
  fixture, with the expected chain.
- Acceptance: every statement of views.sql and examples.sql parses and plans, or is listed with
  its reason in `tests/expected_unsupported.txt`; the test passes when the list is equal, so it
  can only shrink. The views that run are later held to DuckDB's answers on a recorded history
  (the `#[ignore]` tests of `blaze_profile::sql` are the model) before the call sites switch.
