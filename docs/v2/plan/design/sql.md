---
status: plan
summary: blaze-sql, typed SQL with no injection. A query is a const string checked at compile time against a schema made of Rust structs; its parameters are named (`$name`), bound by a record, and its row type is computed (`type_of!`). One sans-IO checker (parse, infer, render) in `crates/blaze-sql`, drivers for C SQLite (`crates/blaze-sqlite`) and Postgres (`crates/blaze-pg`, a sans-IO wire protocol). The checker and the drivers are being written; the typed surface waits on three language pieces, each a ledger row.
date: 2026-11-13
---
# blaze-sql: typed SQL

## What

```rust
struct Person { id: PersonId, name: String, age: i64, city: Option<String> }
const APP: Schema = schema_of::<(Person, Order)>();          // by reflection, no macro

let db: Db<APP> = Db::open(sqlite::at(dir, "app.db"), cx)?;   // refuses a file whose schema differs
let rows = db.query::<"select name, age from person where age > $min_age and city is $city">(
    { min_age: 18, city }, cx)?;
// rows yields { name: String, age: i64 }; the parameters are { min_age: i64, city: Option<&str> }
let ages = table.col(field_of!(Person, age));                 // Col<i64>, no string, no check
```

- **A query is a const generic string.** Its row type and its parameter record are computed from
  the text and the schema at compile time. A wrong column, a type mismatch, a missing, extra or
  misnamed parameter is a compile error at the call, naming the thing.
- **No injection, by construction.** No public function takes SQL text at run time; a value
  reaches the engine only as a bind; a text holds one statement; no function renders a value as
  SQL. See "Injection".
- **Parameters are named.** `$name` only. `?`, `?1`, `$1`, `:name` and `@name` are refused by
  name, saying to name it. A name used twice binds once.
- **A table is a struct, a column is a field.** The schema is read by reflection
  (`for const f in fields_of::<T>()`); `create table` text is generated from it. A column's Rust
  type (a newtype such as `PersonId`) is the type of every result column and parameter that
  carries it, so two ids cannot be swapped.

## Why

sqlx's macros ask a live database at compile time, or read a cache made from one, so a build
depends on a server and a proc macro that opens sockets; its parameters are positional. rusqlite
and tokio-postgres take text at run time and name row types by hand. Neither fits blaze: proc
macros are sandboxed (`sandbox-everything`), builds are hermetic (`deterministic-output`), and
reflection is preferred over macros (`reflection-first`).

## The design

Three layers, one home each.

1. **The checker**, `crates/blaze-sql`: `#![no_std]`, sans-IO, pure. `check(schema, text, alloc)`
   lexes, parses one statement of the subset into `ast::Statement`, infers a `Description` (each
   parameter's name, type and nullability; each result column's name, type and nullability; what
   the statement returns), and `render(statement, backend)` writes the engine's text with the
   engine's placeholders and the order parameters bind in. It is ordinary code: the compiler runs
   it in const evaluation, a test runs it at run time.
2. **Drivers.** `crates/blaze-sqlite`: C SQLite through `libsqlite3-sys`'s bundled amalgamation,
   nothing of rusqlite. `crates/blaze-pg`: the Postgres v3 protocol as a sans-IO state machine
   (bytes in, bytes and events out, deadlines on an injected clock), with a thin driver on bstd's
   `net` later. A driver prepares a checked statement once per connection, compares the engine's
   own description with ours, and stops on a difference with an error naming the statement and
   the column or parameter: the checker's inference is never trusted, and nothing is coerced.
3. **The typed surface**, an edition package over 1 and 2: `Db<SCHEMA>`,
   `query::<"..">(params)`, `Col<T>`. It needs the language rows below and is not written until
   they land; nothing weaker stands in for it.

Losing alternatives: wrapping sqlx (its typed layer is what this replaces; its drivers are async
on tokio and not sans-IO); forking Turso onto bstd (owning a database engine, and it is not
production-ready); SQLite compiled to wasm (sandboxes code already trusted, and WASI has no shared
memory for WAL); a query builder (`diesel`): SQL is the language people know, and a builder needs
a second vocabulary.

### The SQL subset

One subset that means the same on SQLite and Postgres; anything outside it is a compile error
naming the construct. v1: `select` (columns, `*`, `t.*`, `from`, `join`/`left join` with `on`,
`where`, `group by`, `having`, `order by`, `limit`, `offset`, `distinct`), `insert .. values`,
`update .. set`, `delete`, each with `returning`; expressions: literals, columns, parameters,
`+ - * / %`, `||`, comparisons, `and or not`, `is [not] null`, `is [not]` (null-safe equality),
`[not] in (list | $list)`, `[not] between`, `[not] like`, `case`, `cast`, and the functions of
`funcs.rs` (`count sum min max avg coalesce lower upper length abs`). `create table` is generated,
not written. A run of `and`, `or`, `+ -`, `* / %` or `||` is one flat node, so its length
costs no nesting; nesting (parentheses, calls, `case`) is bounded (`parse::MAX_DEPTH`), and the
stack a statement at the bound may use in an unoptimized build is `parse::STACK_MOST`, which a
test measures for parsing, printing, typing and rendering. Later: subqueries, `union`, window functions, `with`, a column chosen at run time
(`order by $col` over an enum of the legal columns), migrations.

### Typing rules

- Storage types: `Integer` (i64, bool, integer newtypes), `Real` (f64), `Text`, `Blob`; a column
  also carries its Rust type's name, which is what a result or parameter of that column is.
- Arithmetic and `||` take and give their storage type, nullable when an operand is. A comparison
  gives a nullable bool; `is null`, `is`, `in`, `between`, `like` with non-null operands give
  bool. `count` is non-null; `sum min max avg` are nullable; `coalesce` is non-null when any
  argument is.
- A `left join` makes every column of its right side nullable.
- With `group by` or an aggregate, a result column is an aggregate or a grouped column, else an
  error (Postgres's rule; SQLite's laxer one is not offered).
- A result column that is not a plain column needs `as name`; two columns of one name are an error.
- **A parameter's type is its position's**: compared with or assigned to a column, it has that
  column's type. Its nullability too: an assignment to a nullable column and an operand of `is`
  take `Option<T>`; every other position takes `T`, because `x = null` is never true. A parameter
  whose position gives no type (`select $x`) needs `cast($x as type)`. `in $list` takes a slice.
- `%` takes integers only: the engines disagree on a real remainder.
- `like` is case-sensitive on both engines (the SQLite driver turns it on at open). `lower` and
  `upper` agree on ASCII text only: SQLite folds ASCII, Postgres folds by the database's locale.
- A literal types to a column of its storage whatever the column's Rust type (`id = 1`,
  `values (1, ..)`); any other expression assigned to a column needs the column's Rust type.
- `offset` needs a `limit`; `order by 1` and `group by 1` are refused.
- `cast(x as boolean)` takes a Boolean or a parameter; a number is refused, saying to write
  `x <> 0` (SQLite has no boolean to cast to, and Postgres refuses a `bigint`).
- Integer arithmetic that overflows is an error on both engines, not a typing matter: Postgres
  raises it, and SQLite answers a real, which the driver refuses at the read naming the column.
- A statement returns rows (`select`, `returning`) or a count.

### Injection

The class is "text decided at run time reaches the engine as SQL". What removes it:

- The surface takes SQL only as a const generic argument, and `Db` has no method taking `&str`.
- `blaze_sql::check` takes text because const code must call it; `blaze::sql_text_at_run_time`
  (deny) refuses a call whose text is not a constant.
- A driver's `prepare` takes a `Checked` statement, which only `check` makes.
- The parser refuses a second statement after `;`, and comments are not part of the subset.
- A list parameter is rendered as numbered placeholders (`?1`, `?2`, ..) or an array bind, never as values.

### Inline capture

The call-site form most queries use: a hole names a variable in scope, as `format!` does, and it
is bound, never spliced.

```rust
let rows = db.query(template!("select name, age from person where age > {min_age} and city is {city}"), cx)?;
```

- It is sugar for the named form: `{min_age}` is the parameter `$min_age` bound to the local
  `min_age`, so everything above (types from the column, nullability by position, one bind for
  a name used twice, lists) holds unchanged. `$name` with a record stays for a query that is a
  const, shared, or read with `include_str!`, where there is no scope to capture.
- A hole is a plain identifier, as in `format!`; anything else is a `let` first.
- A hole inside a quoted literal (`like '%{q}%'`) is a compile error saying to write
  `'%' || {q} || '%'`: text inside quotes is never a parameter.
- It is safe to look like `format!` only because `query` cannot take a `String`: writing
  `format!` in its place does not compile.
- `template!` is a general built-in, not a SQL feature and not a proc macro: a literal with holes
  becomes its constant text and a record of the captured values, for any callee that must tell
  text from data (SQL, a shell command, HTML). Row Q8.

### Composition

A query is a structured value, not text: `Query<Row>` is a checked tree with the values it
captured, and text is rendered last, per engine. SQL stays the one notation; there is no builder
vocabulary beside it. What composes is a hole whose variable is a fragment instead of a value:

```rust
let adults = query(template!("select id, name, city from person where age >= {min_age}"));
let by_city = query(template!("select city, count(*) as n from {adults} a group by city"));
let recent: Cond<Person> = if only_new { cond(template!("joined > {since}")) } else { Cond::TRUE };
let rows = db.run(query(template!("select name from person where {recent} order by {order}")), cx)?;
```

- **Fragment kinds**: a relation (`Query<Row>`, usable after `from` or `join`), a condition
  (`Cond<Scope>`), an ordering (`Order<Scope>`), a column or expression of one type
  (`Expr<Scope, T>`). A fragment is checked where it is written, against the scope it names,
  and spliced as a tree with its captures; no fragment is ever text.
- **The rule for what may vary at run time**: anything that leaves the row type alone. Which of
  several fragments of one type fills a hole is a run-time value; each alternative is a constant
  checked at compile time, so the texts a program can send are a finite set known when it is
  built. A choice that changes the row type (grouping by different columns of different types)
  is two queries of two types, in this design or any typed one.
- **Cost**: a query whose fragments are all fixed keeps its static statement id. One with a
  run-time choice finds its prepared statement by hashing the ids of the fragments chosen.
- Fragments are holes in the tree (`ast` gains a hole node for a relation, a condition, an
  ordering and an expression); the checker, the description and `render` are otherwise as built.
  Row Q9.

### Language pieces (`build-the-missing-piece`)

| Row | Piece | State |
|---|---|---|
| Q5 | `&'static str` as a const generic argument, end to end | hir-ty refuses it without the feature; not checked with it |
| Q6 | `field_of!(T, f)` in type position | not parsed (docs/const.md) |
| Q7 | `type_of!(desc)`: a type from const code (comptime phase 3) | design only |
| Q8 | `template!`: a literal with holes as constant text and a record of captures | designed here |
| Q9 | fragments: relation, condition, ordering and expression holes | designed here |
| E5 | records | parsed and lowered; fixtures missing |

## Cost (`within-ten-percent-of-the-floor`)

| Operation | Floor | This design |
|---|---|---|
| Compile: one query's check | parse and infer that text once | the same, memoized by (table descriptions read, text) |
| Compile: edit one query | recheck that query | the same |
| Compile: edit a table's struct | recheck the queries naming that table | the same: each table's description is its own const, so a check reads only the tables it names |
| Run: execute | the engine's bind, step and column calls | plus an index into the connection's statement array by a static id; no hash, no text compare, no allocation beyond the row's owned values |
| Run: first use on a connection | the engine's prepare | plus one comparison of descriptions |
| Memory | one prepared handle per statement used per connection | the same |

What no design avoids: the engine parses the text once per connection; a SQLite value's storage
class is known only when read, so each value read costs one compare.

## How it is proven

- **Parser**: a Hegel round trip, `parse(print(s)) == s`, and a fuzz target (hostile bytes).
- **Inference** (the one that matters). The refinement: on generated schemas, rows and statements
  of the subset, every row real SQLite returns is admitted by the inferred description (column
  count, names, storage class, null only where nullable), and a statement the checker accepts is
  one SQLite prepares. Control: a planted checker that keeps a left join's right side non-null.
  The same test runs against Postgres when its driver exists.
- **Injection**: for generated parameter values (quotes, `;`, comment markers, NUL), the table
  afterwards equals a model's, and the text prepared is byte-equal to the rendered constant.
- **blaze-pg**: a DST on `blaze_core::sim::Sim`: split and coalesced reads, a peer that closes
  mid-message, errors mid-pipeline. The invariant: every query ends in exactly one outcome, and
  after an error the next query's rows are its own. Control: a planted missing `Sync`.
- **Real use**: `harness/apps/websvc` moved off sqlx, its tests passing under blaze.

## Not decided here

How a fiber runs a blocking call into C SQLite (bstd's rule for blocking calls decides); a SQLite
VFS on blaze-core's effects so the simulation can tear its writes; migrations.
