# Requests from fix-names

## 1. Workspace dependency `blaze-phf` (root `Cargo.toml`)
- What: add to `[workspace.dependencies]`:
  `blaze-phf = { path = "crates/blaze-phf", version = "0.0.0" }`
- Where: root `Cargo.toml`. The crate itself (`crates/blaze-phf/`, new, no dependencies,
  `no_std`, `forbid(unsafe_code)`) is already written and is a member through `crates/*`.
- Why: the `const fn` perfect hash for fixed name sets (keywords, lint names, attribute and
  feature names read as bytes). The parser, `blaze-diagnostics`, `blaze-lints` and the
  front end use it as `blaze-phf.workspace = true`; `Cargo.lock` gains the path package.

> applied: `blaze-phf = { path = "crates/blaze-phf", version = "0.0.0" }` in the root `[workspace.dependencies]` (a path package new to the lock).

## 2. Keywords in `vendor/proc-macro-srv` (no owner in `docs/v2/plan/reference/crates.md`)
- What: give `vendor/proc-macro-srv/crates/parser/src/syntax_kind/generated.rs`
  (`from_keyword`, `from_contextual_keyword`) the same tables as `crates/parser`
  (`crates/parser/src/syntax_kind/keywords.rs`), with `blaze-phf` as a path dependency
  (`path = "../../../../crates/blaze-phf"`) of that workspace's parser.
- Why it is a request: `vendor/proc-macro-srv` is a workspace of its own (excluded from the
  root, its own `Cargo.lock`, built alone by the release workflow), so the change needs its
  lock regenerated, which no agent may run now; and it has no v2 owner to agree to diverging
  from the release's copy. Its lexer runs only on the token text of macro input and output in
  the server, so this is the smaller win of the two.

> forwarded to coordinator: `vendor/proc-macro-srv/` has no owner in crates.md (also asked by usercode-macros 3).
