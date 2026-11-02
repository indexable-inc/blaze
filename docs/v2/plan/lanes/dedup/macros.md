---
status: plan
summary: dedup-macros (2026-10-29): the wasm and in-process proc-macro paths share one guest `proc_macro` text, one host wire, one lexer and one set of span operations, all in blaze-expand's bridge, and `LitKind`'s wire numbers live once in `tt`.
date: 2026-10-29
---
# dedup: the proc-macro paths

Part of [dedup](../dedup.md). The copies are between the wasm path (`crates/blaze-macro-wasm/`)
and the in-process bridge (`crates/blaze-expand/src/native/`), both lane usercode's.

## Which path survives

`blaze-expand` is "replaced" in [crates.md](../../reference/crates.md) in the sense that its
contents became v2's keyed expansions. The crate stays. Its `native/` directory is no longer an
expander cache (#1297 is superseded and its expanders are deleted). It is the proc-macro bridge
that change 20 ([compile-time.md](../../design/compile-time.md)) runs macros against, handed to
fastcompile in `v2/requests/usercode-bridge.md`. The wasm path stays until the evaluator runs
every macro, and then it is removed ([usercode/macros.md](../usercode/macros.md), last item).
So both paths live for now, the bridge outlives the wasm path, and the shared code belongs to
the bridge.

## Where the shared code lives, and why

`blaze-macro-wasm` depends on `blaze-expand`, so `blaze-expand` is the only one of the two that
both can use. It is also the survivor. A new crate would add a manifest and a root entry and buy
nothing: neither side needs the shared host code without `blaze-expand`. So:

| what | one copy at | used by |
|---|---|---|
| the host's wire (tt's column bytes, made relative, and `Parts`) | `blaze_expand::bridge::wire` (`crates/blaze-expand/src/bridge/wire.rs`) | `native/`, `blaze-macro-wasm` (`use blaze_expand::bridge::wire`) |
| the lexer for `ts_from_str` and `literal_from_str` | `blaze_expand::bridge::lex` | both |
| span operations the host answers (`join`, `subspan`) | `blaze_expand::bridge::spans` | `native/host.rs`, `blaze-macro-wasm`'s `add_host` |
| the guest `proc_macro` from `fn taken` to the end of `wire` | `crates/blaze-expand/src/native/proc_macro/api.rs` | the native crate's `lib.rs` (`include!("api.rs")`) and `blaze_macro_wasm::GUEST` (`concat!`) |
| `LitKind` to and from its number | `tt::LitKind::{code, from_code}` | `tt::storage`, `hir-def`'s `nameres/frozen.rs` (the wire's tables go with the move to tt's column bytes) |

### The guest: `include!` and `concat!`, not `#[path]` or a crate

The guest's text must stay a plain file. It is compiled inside the macro (wasm or native), so it
can't depend on blaze's crates, and the wasm side gives it to the database as the one text of
the `proc_macro` root (`GUEST`, overlaid by `blaze-engine`'s `macro_list.rs`), not as a
directory. So:

- `#[path] mod api;` would put the API under `proc_macro::api`. A `pub use api::*` would bring it
  back to the root, but the overlay is one file, so the wasm side would need a second overlaid
  file.
- A crate both build can't work, because the wasm guest is not built by cargo: it is the
  `proc_macro` root text.
- `include!("api.rs")` in item position keeps every item at the crate root. `concat!` of
  `include_str!`s makes the wasm root one text. `api.rs` carries no inner attributes or `//!`
  (neither may appear in an `include!`d file); each root keeps its own.

Each root keeps only what differs: its header, `mod sys` (wasm imports, or calls to
`__native::Host`), and its exports (the wasm module's `#[no_mangle]` entries, or `__native`).
The text-equality tests become one test that both `sys` modules declare the same functions with
the same parameters.

## Copies kept on purpose

- `proc-macro-api`'s `legacy_protocol/msg/flat.rs` numbers `LitKind` from 1, with `Err` as 0.
  That is rust-analyzer's `FlatTree` protocol, which an older server speaks, so it stays as is.
  It is not the column format's numbering.
- `native/tests.rs`'s `reference` module (the wasm host's two-pass reader) is the reference for
  the one-pass reader. It goes away with both readers when the wire becomes tt's column bytes,
  whose own differential tests are in `crates/tt/src/storage/`.
- The eight-argument `ProcMacroExpander::expand` signature, repeated by `Kept`,
  `blaze_macro_wasm::Expander` and `blaze-wasm`'s `Recorder`, is one trait's impls, not a copied
  primitive. The fix is an argument struct in `hir-expand` (lane db, requested), which also stops
  each wrapper from cloning `current_dir: String`.

## Speed of light

Nothing here adds work on a path: sharing a module is free at run time. `include!` and `concat!`
are resolved when the code is compiled. `LitKind::code` is a `const fn` match that compiles to
the same jump table or lookup as each copy. The bound and how the wire reaches it are lane
usercode's ([usercode/macros.md](../usercode/macros.md)); this part only makes the code exist
once, so the wire's speed has one implementation to keep at that bound.
