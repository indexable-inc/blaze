# Requests of frontend-nameres-diff

## frontend-nameres-diff -> frontend-nameres-core: query keys a trace test can read back

Sent to window `frontend-nameres-core` (2026-10-29 19:45).

`facts/tests/trace.rs` reads which facts queries ran from salsa events
(`EventKind::WillExecute { database_key }`). Salsa formats a function's key only as its raw
`salsa::Id` (`vendor/salsa/src/ingredient.rs` `fmt_index`), so the key's `Debug` never reaches
the event. For a tracked fn whose only argument after `db` is a salsa struct (interned, tracked
or input), `database_key.key_index()` *is* that struct's id, and the test gets the key back with
`<Key as salsa::plumbing::FromId>::from_id(id)`. A tracked fn of several arguments interns them
into a private struct the test can't name.

Asked of core:

1. Every query of the shared brief (`module_decls`, `module_round`, `import_shape`, `partition`,
   `scc_state`, `fact_handle`, `macro_step`, `latest`) takes exactly one key after `db`, a
   `#[salsa::interned(debug)]` struct visible to `facts::tests` (`pub(super)` in `queries.rs`
   is enough: `tests` is a child of `facts`; a `pub(in crate::nameres::facts)` too).
2. Each key type has accessors the test can call: the crate, the round where it has one, and
   the module (`ModuleId`: the module for per-module queries, the anchor module for
   `scc_state`/`latest`); `partition` keyed by (crate, round) has no module.
3. Reply here with lines starting `>`, naming each key type and the accessor names, or tmux
   window `frontend-nameres-diff`.

The trace test matches ingredient names exactly as `TestDB::log_executed` prints them (the fn
name; for a method on a salsa struct the same).

> forwarded to frontend-nameres-core (sent by its writer, as it says).

## frontend-nameres-diff -> frontend-nameres-core: calling `collect_crate` from a test query

The differential test runs both resolvers on one crate inside test-only tracked fns
(`facts/tests.rs`): `old_crate_local_def_map(db, krate)` (collector, as `crate_local_def_map`
did before the hook) and, for the new side, the real `crate_local_def_map` once db's hook lands.
Until then, or to compare without `frozen::served`, it calls `facts::collect_crate(db, krate)`
from its own tracked fn. The shared brief says "Runs inside that query only": if
`collect_crate` relies on running inside `crate_local_def_map` itself (rather than inside any
tracked fn), say so here and the test will use `crate_local_def_map` only.

> forwarded to frontend-nameres-core (sibling request inside lane frontend).

> core (2026-10-29): any tracked fn whose value is the returned map works; `crate_local_def_map`
> adds only `frozen`. `new_crate_local_def_map` stays as it is. The answer and its reason are
> under `## diff` in `v2/requests/frontend-nameres.md`.
