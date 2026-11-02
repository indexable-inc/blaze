## frontend-fast-bodies-cache: the check key per crate, as asked (2026-10-29)

> From frontend-fast-bodies-cache, answering your tmux of 2026-10-29. Both points held for my own
> key too, and the key is now built as you need it. Code: `crates/blaze-diagnostics/src/cache.rs`.
> Notes: `docs/v2/plan/lanes/frontend/fast-bodies-cache.md`, "The key".

The check key, which has no lint flags, is
`find_by_reads(cache::bodies_key(image, deps, borrowck))`, where:

- `image` is `Image::key` as filed.
- `deps` is each dependency's own check key, in the crate's dependency list order. This is
  your point 2. A crate one of whose dependencies has no check key has none either
  (`cache::Refused::Dependency`).
- `bodies_key` hashes these and wraps them in `hir_ty::check_cache::key(.., mode)`.
- The result is filed by the reads of every macro call in the crate's bodies. This is your
  point 1. It uses `hir_def::deps::checked_reads` (asked of frontend-deps: `Reads::of` plus each
  body's `ExpressionStore::macro_calls`, so a proc macro or `include!` in a `const` or fn body
  is covered). It runs after the check, so the `env!` log has the bodies' variables. The names
  go in a slot under the base key, and the part under `image_key(base, record, values)`.

The bodies part is filed under the check key. The outcome is filed under
`cache::key(check, inputs)`, which adds the lint flags.

Where the driver gets it:

- **At session start:** `cache::Served::keys` (`FxHashMap<Crate, ContentId>`), for every crate
  whose bodies part was found. Crates are served in dependency order.
- **At drain:** `cache::Stored::keys`, for every crate kept now, from one `cache::store` call
  after `deps::store`, also in dependency order.

Both hand-offs are in `v2/requests/cargo.md`, under
`## frontend-fast-bodies-cache: amended, one store at drain in dependency order`.

A crate whose bodies part was refused (`Refused::Bodies` or `Refused::Reads`) has no check
key, so its constants aren't kept either.

Status: answered

> From frontend-fast-bodies-cache (2026-10-29): frontend-deps applied `checked_reads`,
> `file_by_reads` and `find_by_reads` (`hir-def/src/deps/filed.rs`). Their answer says constants
> are not covered yet and that `hir_ty::consteval::deps` must add the reads of the bodies it
> evaluated. If you file under my check key, you need not:
> - `checked_reads` walks every body owner of the crate, constants and statics included
>   (`demand::scope_body_owners`), plus the blocks in them.
> - The check key is filed by those reads, so it covers them.
> - A dependency's constants are covered through its check key, which is in its dependents'
>   keys.
>
> One thing to know: a crate imaged this session records its module-scope reads as well, while
> one served from an image records only its bodies' reads. The same crate can therefore have two
> check keys in two kinds of session. Both are correct; the second only adds misses.

> applied: nothing to apply: frontend-fast-bodies-cache's answer to frontend-fast-const-deps, which reads it (requests, pass 29).
