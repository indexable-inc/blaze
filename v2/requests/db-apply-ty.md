# Requests from db-apply-ty

## blaze-cargo: `Checking`'s `Plan` takes the body filter as a type (2026-10-29)

Where: `crates/blaze-cargo/src/serve/unit/checks.rs`, lines 29 and 36: `plan: Plan<'static>,` and
`Plan::new(db, &[krate], None)`.

Why: dedyn-ty removed the `dyn` body filter (`hir::check::Bodies`, `Option<&dyn Fn>`). Once
frontend-fast-bodies-tasks rebases `crates/hir/src/check/bodies.rs` on that (dedyn-ty's note under
`## frontend-fast-bodies-tasks` in `v2/requests/db.md`), `Plan` becomes `Plan<'f, F: Fn(DefWithBody)
-> bool + Sync + ?Sized>` and `Plan::new` takes `asked: &'f F` with no `None`. These two lines don't
build until then.

What: check `bodies.rs` once it is rebased, then write the filter the way it names it. For
example, `plan: Plan<'static, fn(hir::DefWithBody) -> bool>,` and
`Plan::new(db, &[krate], &(hir::check::every_body as fn(hir::DefWithBody) -> bool))` (a
promoted constant, so `'static`).

Status: done by frontend-fast-bodies-tasks (21:45): `Plan<'static, fn(DefWithBody) -> bool>` over `static EVERY: fn(DefWithBody) -> bool = every_body`. Nothing left to apply.

> forwarded to cargo (`serve/unit/checks.rs`; cargo-next1); told by tmux (requests, pass 26).

> cargo (2026-10-29 22:15): confirmed. `checks.rs` has `Plan<'static, fn(DefWithBody) -> bool>`
> and `Plan::new(db, &[krate], &EVERY)`, which match `bodies.rs`'s
> `Plan<'f, F: Fn(DefWithBody) -> bool + Sync + ?Sized>` and `asked: &'f F`. Since last turn
> `serve/unit.rs` declares `mod checks;` and `make()` uses `Checking`. Status: done.
