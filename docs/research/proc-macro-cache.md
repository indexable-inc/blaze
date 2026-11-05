---
status: record
summary: rustc's derive cache and earlier attempts, and what blaze's proc-macro expansion cache keys and why.
date: 2026-10-21
---
# Caching proc-macro expansions: rustc's derive cache and what blaze keeps

2026-10-21. For issue #38 (Glancer item 3, [docs/glancer.md](../glancer.md)). Sources under
[docs/upstream/proc-macro-cache/](../upstream/proc-macro-cache/).

## rustc

- **read** rustc caches derive expansions only, behind `-Zcache-proc-macros` and only in an
  incremental session (`ecx.sess.opts.incremental.is_some() &&
  ecx.sess.opts.unstable_opts.cache_proc_macros`, `DeriveProcMacro::expand` in
  `compiler_rustc_expand_src_proc_macro.rs`). Attribute and function-like macros are expanded
  every time.
- **read** The cache is a query, `derive_macro_expansion`, keyed by the expansion id and the
  input `TokenStream`; the macro runs inside the query with its context passed through a
  scoped thread-local (`QueryDeriveExpandCtx`, `provide_derive_macro_expansion`). The query
  system, not the macro, decides staleness: the key's hash and the incremental graph.
- **read** It merged on 2026-01-16 as rust-lang/rust#145354 (commit `81cc29a425`), a revival
  of futile's #129102. The PR reports a no-op re-check of `octocrab 0.49` going from about 6 s
  to 5.4 s (`pr-145354.md`).
- **reported** #129102's ad-hoc numbers: bevy `touch crates/bevy_ecs/src/lib.rs && cargo
  check` 6.4 s to 5.3 s; polkadot-sdk incremental checks 99.4 s to 67.5 s. Its list of what
  was missing: attribute and function-like macros, and a per-macro opt-out (Exa excerpt).
- **reported** #128747 tried the same for declarative macros and found it a net loss in
  rustc-perf except html5ever: the cost of a query per expansion exceeds re-expanding (Exa
  excerpt).
- **reported** The internals thread on proc-macro idempotence: rustc cannot tell which
  macros are pure, and cargo has the same question for caching their dependents (Exa
  excerpt).

## blaze

- **read** rust-analyzer expands through its proc-macro server, which is the toolchain's
  `rust-analyzer-proc-macro-srv` here. A request carries the input token trees with spans as
  indices into a table of the call's spans, the environment and the current directory
  (`legacy_protocol.rs`, `bidirectional_protocol.rs` in `proc-macro-api`). The response
  carries a span table of its own: the server can make spans the input has not.
- **read** (measured, `harness/expansions.py`, 2026-10-21) Tokens a macro parses from a string
  at the call site (`quote!`'s output, `str::parse::<TokenStream>()`) come back with byte
  ranges inside that string, the call site's anchor and its context. A first version that
  refused outputs with spans not in the input's table kept none of 61 expansions.
- **read** (measured, same test) The toolchain's server answers no question of a macro
  during an expansion: `Span::source_text` returns `None` under blaze where rustc returns
  the text, and the callback counter saw no request. A macro that relies on it expands
  otherwise than under rustc; that is not the cache's doing.
- **unverified** blaze's key covers what a macro can read without asking: every span's
  byte range within the item it is anchored to, and which of the call's anchors and contexts
  it has, numbered as they first appear. A macro that reads files or the environment by
  itself is outside what any key can see, as in rustc; the skip list and
  `BLAZE_PROC_MACRO_CACHE=check` are for those.
