---
status: plan
summary: Const macros: proc macros written inline as const fn, run by the const evaluator. Stage 0 is experimental.
date: 2026-10-28
---
# Const macros: proc macros written inline as `const fn`

Status: design. Stage 0 of the prototype ([below](#the-prototype-stage-0)) is on main behind
`experimental = ["const-macros"]`: the evaluator call and expander (#915), and the manifest key
and loading (#959). Of stage 2 ([below](#stage-2-the-item-as-a-flat-tree-through-const-views),
#1441), the const views are on main, and a macro that takes `&const_ast::Item` is given its
input through them. Nothing here is on by default. Issues: #859 (hermetic build graph), #860 (purity as a front-end query), #867
(const traits, const closures, const heap, step budget), #863 (the build graph's `map`).

## What it is

A const macro is a procedural macro whose body is a `const fn` over token trees, written in
the package that uses it, and expanded by blaze's own const evaluator inside the compiler's
process:

```toml
# Cargo.toml of the workspace root
[workspace.metadata.blaze]
experimental = ["const-macros"]

# Cargo.toml of the package
[package.metadata.blaze]
const-macros = "src/macros.rs"
```

```rust
// src/macros.rs
use proc_macro::{TokenStream, TokenTree};

#[proc_macro_derive(Named)]
pub const fn named(input: TokenStream) -> TokenStream {
    let name = item_name(&input);
    quote_const!(impl crate::Named for #name { const NAME: &str = stringify!(#name); })
}
```

```rust
// src/lib.rs
pub trait Named { const NAME: &'static str; }

#[derive(macros::Named)]
pub struct Point { x: i32, y: i32 }
```

There is no proc-macro crate, no second package, no dylib, no WebAssembly module and no
process. Expanding `#[derive(macros::Named)]` is a call of `named` by the evaluator in
`crates/hir-ty-mir/src/mir/eval.rs`, made from the proc-macro expansion query of `hir-expand`,
and its result is a salsa value like any other expansion.

The direction this comes from: const is purity (`/tmp/blaze-lanes/const-sketch.md`, Andrew,
2026-10-28). A const fn reads its arguments and rodata, writes only its own scratch memory,
and touches no state, files, clock or network. That is exactly what a cached macro needs to
be, so blaze checks it once in the front end instead of building a sandbox around it.

## Why not just today's proc macros

Today a proc macro is a crate of its own (`proc-macro = true`), compiled for the host, or for
`wasm32-wasip1` and run under Wasmtime (`crates/blaze-macro-wasm`). That costs:

- A second package, with its own manifest, for every crate that wants one derive.
- Compiling it, and usually syn and quote, before the first expansion: seconds cold, on the
  critical path of every crate that uses it.
- A boundary per expansion: a request to the proc-macro server, or a fresh wasm instance, with
  the input and output encoded across it.
- Purity by containment. A proc macro can read files and the environment; the sandbox
  (`macro_sandbox.rs`) and the store's key (`crates/blaze-expand`, `blaze-macro-wasm/keyed.rs`)
  record what it read so its expansion can be kept. That works, but it is checked at run time
  and guessed for function-like macros (`BLAZE_PROC_MACRO_CACHE=all` is opt-in).

A const macro has none of these: nothing to compile but the macro's own MIR, which the front
end lowers anyway; nothing to cross; and purity proven before it runs, so every expansion, of
every kind, is a function of its input and can always be kept.

What it does not replace: macros that need what const can't do (read a file, run a program,
call C) stay ordinary proc macros. The two use the same expander interface
(`hir_expand::proc_macro::ProcMacroExpander`), so a crate can use both.

## Spelling, and what plain rustc says

Everything blaze-only is in blaze's own namespace, so a project that uses const macros is a
project only blaze builds, and rustc says so at the first use, not by doing something else:

- `[workspace.metadata.blaze] experimental` and `[package.metadata.blaze] const-macros` are
  keys cargo ignores.
- `src/macros.rs` is not a module of the crate (no `mod macros;`), so rustc never reads it.
  blaze makes it the root of a crate of its own (below), named for the file's stem, in the
  extern prelude of every target of the package.
- A use is `#[derive(macros::Named)]` or `use macros::Named;`. Under cargo and rustc it is
  `error[E0433]: failed to resolve: use of unresolved module or unlinked crate `macros``.
- To make that error name blaze, a crate can add:

  ```rust
  #[cfg(not(blaze_const_macros))]
  compile_error!("this crate's macros are blaze const macros (src/macros.rs): build it with blaze");
  ```

  For this, blaze sets the cfg `blaze_const_macros` for a package whose `const-macros` key
  it honours. rustc's `unexpected_cfgs` lint warns about the name unless the package lists it
  (`[lints.rust] unexpected_cfgs = { level = "warn", check-cfg = ["cfg(blaze_const_macros)"] }`).

The superset rule (AGENTS.md, superset-of-rust) is intact: no program rustc accepts changes
meaning. A package without `const-macros` is built exactly as before, and the key itself is
blaze's. `const-macros` without the experiment is an error naming the line to add
(`Experiments::how_to`), never a silent ignore (no-fallback).

The macro file is also a valid proc-macro crate root: its token API has `proc_macro`'s names
and signatures (below). Moving a const macro to a real proc-macro crate, for users who must
build with rustc, is copying the file and dropping `const`.

## Why the macros are a file of their own (v1)

A macro used in the crate that defines it is a cycle. The crate's def map is what expanding
`#[derive(Named)]` adds items to; evaluating `named` needs `named`'s MIR, which needs its
types, which need the same crate's def map. rustc forbids it outright (proc macros can't be
used in their own crate). Salsa would report it as a cycle.

v1 breaks it the way the proc-macro crate does, without making the user write the second
package: blaze adds the macro file to the analysis crate graph as a crate of its own.

- Its root is `const-macros`'s file. Its dependencies are the package's normal dependencies
  (not dev or build), its edition and cfgs the package's.
- It is a proc-macro crate to the def map collector (`target_kind` proc-macro), so its
  `#[proc_macro_derive]`, `#[proc_macro_attribute]` and `#[proc_macro]` functions are exported
  as macros by the code that does so for every proc-macro crate (`export_proc_macro` in
  `hir-def/src/nameres/collector.rs`). Its `ProcMacros` entry lists them with the const
  expander.
- It is in the analysis graph only. Cargo knows nothing of it, codegen never compiles it, and
  no artifact is made: the evaluator runs its MIR.
- It can't name the package's own items (`crate::` in it is itself), which is the rule that
  makes the graph acyclic. The generated code can: an expansion is tokens, and `crate::Named`
  in them resolves at the call site.
- A library that offers its const derives to other crates re-exports them
  (`pub use macros::Named;`), as serde re-exports serde_derive.

### v2: anywhere in the crate

The long-term form is a `#[proc_macro_derive]` const fn in any module of the crate, with a
stage rule instead of a file rule: a const macro, and everything it reaches, may not use
items that const macros produce. blaze would collect the crate's def map twice, a stage-0 map
with const-macro calls left unexpanded, against which macro bodies are typed and evaluated,
and the full map. That needs item ids that are the same in both maps (today a `FunctionId`
is interned under its def map's module), so it waits until v1 has users. The file of v1
stays valid under v2.

## The token API in const

The macro is written against `proc_macro`'s API: `TokenStream`, `TokenTree`, `Group`,
`Ident`, `Punct`, `Literal`, `Span`, `Delimiter`, `Spacing`. In a const-macro crate blaze puts
its own `proc_macro` in place of the standard library's, as it does for wasm macros: the value
implementation in `crates/blaze-macro-wasm/guest/proc_macro.rs`, where a stream is a vector of
token trees in the macro's memory and a span is a number the host hands out, with:

- every function `const`, and every trait impl the API has (`FromIterator`, `Extend`,
  `IntoIterator`, `Display`, `FromStr`, `Clone`) an `impl const`;
- the host calls (span joins, `Span::call_site()`, a string lexed by `FromStr`) intrinsics the
  evaluator answers, where the wasm build has imports.

That is what needs the other const lanes:

| Needs | Lane | Why |
|---|---|---|
| const heap: `Vec`, `Rc`/`Box`, `String` in const fn, freed or made global at the end | cheap | a stream is a vector; output outlives the call |
| const traits: `impl const FromIterator`, `[const] Iterator`, `const Drop` | ctraits | `collect`, `for` loops, `quote`'s `Extend` |
| blaze's own check_consts | ccheck | the purity proof; refuses statics, atomics, FFI, pointer-to-int |
| step budget, gaps in the evaluator | cgaps / #867 | a looping macro ends with an error, not a hang |

Until those land the prototype uses a stage-0 interface that needs none of them (below).

`quote!` itself is a macro_rules macro over `proc_macro2`; once the API is const, a const
`quote` is a port of its macro_rules part with `proc_macro` paths. syn is a larger question:
it is not const today, and running it in the evaluator is the "non-const proc macros in the
evaluator" item under Later.

### Spans and hygiene

A span in the macro is an index into the call's span table, the same table
`crates/blaze-expand` keys expansions by: `def_site`, `call_site`, `mixed_site`, then each
distinct span of the input, in order of first appearance. The macro can copy spans, compare
them, set them on new tokens and ask for the three sites; it cannot read a span's file, line
or text in v1 (`Span::source_text`, `line`, `file` are refused at evaluation with an error
naming the call), because those would make the expansion depend on more than its input.

After evaluation, each output span index is mapped back to the real `Span` of the call, so
hygiene is exactly that of a proc macro today: input tokens keep their contexts, tokens made
with `Span::call_site()` resolve at the call, `mixed_site()` gives macro_rules hygiene for
locals, `def_site()` is the macro's own (unstable upstream, as for proc macros). A const macro
can't make spans another call couldn't be given back, so every expansion is keepable, which
blaze-expand can't promise for proc macros.

### Errors

- The macro panics: the expansion fails with the panic's message at the call site, with the
  evaluator's stack of const fn frames in `macros.rs` as notes. Same as a proc macro's panic
  today (`ProcMacroExpansionError::Panic`), more precise.
- The macro returns tokens with `compile_error!`: as for any proc macro.
- The body is not const-callable (reads a static, calls a non-const fn, uses an atomic): a
  const-check error in `macros.rs` at the offending call, from ccheck's check, before any
  expansion. This is the purity proof failing, and it fails where the code is.
- The evaluator runs out of steps or memory: "the const macro `Named` stopped after N steps
  expanding `Point`", at the call site. Deterministic, so it is kept like a success.
- The evaluator can't do something (an intrinsic it lacks): an error naming it, never a
  fallback to running the macro another way (no-fallback). It is a cgaps bug.

## Caching

Within a daemon, the expansion is salsa's: `expand_proc_macro` for the call reads the call's
tokens and, through the evaluator, the MIR of every function the macro reaches. Editing the
macro re-expands each call (and only those calls) whose result changes; editing an item
re-expands that item's derives. Nothing is keyed by hand.

Across daemons and machines, an expansion is kept in blaze's store under (code hash, input
hash), the shape blaze-expand uses with the dylib's content replaced by the code hash:

- code hash: fn-cache's `cache_code` of the macro function, which covers every function and
  constant it reaches (docs/fn-cache.md), plus the evaluator's version;
- input hash: the input tokens, and the attribute's tokens for an attribute, with spans
  replaced by their index in the span table (as above), and the macro's name and kind.

No environment, current directory or file reads are in the key, because a const fn can't
reach them. Unlike today's proc-macro cache this holds for function-like macros too.

## How it compares

| | macro_rules | proc macro (dylib/wasm) | const macro |
|---|---|---|---|
| where written | anywhere | its own crate | `const-macros` file of the package (v1), anywhere (v2) |
| language | patterns | Rust, anything | Rust, const fn |
| runs | blaze's mbe | proc-macro server or Wasmtime | blaze's const evaluator, in-process |
| compiled first | no | yes, with its dependencies | no (MIR only) |
| pure | yes | by sandbox and recorded reads | by const check |
| kept across machines | n/a (cheap) | derives and attributes; bang opt-in | every kind |
| plain rustc | yes | yes | no: clear error |

## The prototype (stage 0)

Behind `experimental = ["const-macros"]`, the smallest working path, so the plumbing exists
before the const heap and const traits do:

- `[package.metadata.blaze] const-macros = "<file>"` adds the macro crate to the analysis
  graph as described, and its `ProcMacros` entry lists the file's `#[proc_macro_derive]`,
  `#[proc_macro_attribute]` and `#[proc_macro]` functions (found by parsing the file at load).
- The stage-0 signature is text: `fn(&str) -> String` for a derive or function-like macro,
  `fn(&str, &str) -> String` for an attribute. The input is the token stream printed; the
  output is lexed back into tokens, every one with the call-site span (as `quote!` with
  `Span::call_site()` gives). The evaluator is called directly, the way
  `render_const_using_debug_impl` calls `std::fmt::format`: the input written into its heap,
  the `String` read back from the result's three words.
- The expander downcasts the `&dyn SourceDatabase` it is given to `&dyn HirDatabase` through
  salsa's views; this is sound because the macro crate never depends on the crate being
  expanded.

Stage 0 is not the API. It is deleted when the const `proc_macro` lands, and macros written
for it move by changing their signature.

What stage 0 does not do yet:

- It doesn't require `const`. The evaluator runs the function whatever its signature says, so a
  stage-0 macro can use `format!` and `String::push_str`. Requiring const, and so purity, is
  the front-end purity query's job (#860, lane ccheck). Until then, the evaluator's
  sandbox (no files, no FFI, no clock) is the only guarantee.
- The list of macros is read when the workspace loads. Editing a macro's body re-expands
  everything that uses it, because the expander reads the function through the database. A
  new `#[proc_macro_derive]` function needs a reload.
- Every output token gets the call site's span. Errors point at the derive, not into the
  macro's output.

## Stage 2: the item as a flat tree, through const views

#1441. A const macro is given the item as blaze's own flat syntax tree (`syntax-tree`'s
`flat.rs`) and reads it through typed views, instead of a token stream it parses itself. The
plan, a pull request a step:

1. **Const views** (on main): `crates/const-ast`, a `no_std` crate with no dependencies. A
   `Tree` is four slices, each element's kind (with a token bit), the number after its
   subtree, its span index and a token's text: plain data a host writes into the evaluator's
   heap once (`repr(C)`). The views (`Item::as_struct`, `Struct::field_list`,
   `RecordField::ty`, `Variant::const_arg`, `Attr::path`, `Attr::as_simple_call`,
   `GenericParamList::generic_params`, `WherePred::ty`, `Type::as_ref`, `Path::segment`, and
   the rest of items, structs, enums, unions, fields, variants, attributes, generics, types
   and paths) are `syntax::ast`'s nodes and accessors of the same names, as `const fn`s over
   the slices. `HasName`, `HasAttrs`, `HasVisibility` and `HasGenericParams` are inherent
   methods, since a const fn can't call a trait's method yet, and children are cursors with a
   `const fn next` (`while let Some(field) = fields.next()`), which implement `Iterator` too.
   A macro never sees a kind's number: they are in `kinds.rs`, private, checked against
   `SyntaxKind` by a test that rewrites them (`UPDATE_EXPECT=1`), so a grammar change changes
   the views, and through `cache_code` every expansion that used them. Tested two ways: every
   view and accessor against `syntax::ast`'s on every node of `library/core`, `library/alloc`,
   salsa and the crate itself (`const_ast::tests`), and what a derive reads through them, folded
   into one number, evaluated by blaze's const evaluator against the same code compiled, on real
   files and on one with every kind of node the views read (`hir-ide`'s
   `consteval::const_ast`). Running them in the evaluator found it laying out an overflowing
   `u128` operation's `(u128, bool)` as twice a `u128` where the target aligns `u128` to 8;
   it now pads to the alignment.
2. **The item as input** (on main): each parameter of a const macro's function is `&str`, the
   tokens as text as in stage 0, or `&const_ast::Item`, the tokens parsed back to an item as a
   flat tree (so it is what a proc macro is given: cfg-stripped, the derive itself taken
   out). For a derive:

   ```rust
   #[proc_macro_derive(Fields)]
   pub const fn fields(item: &const_ast::Item) -> String {
       match item.as_struct() { Some(adt) => /* adt.name(), adt.record_field_list() */, None => .. }
   }
   ```

   Each token's span is its index in the call's span table: `def_site` 0, `call_site` 1,
   `mixed_site` 2, then each distinct span of the input in the order it first appears. The
   expander (`hir-ide`'s `const_macro.rs`) writes the arrays, the `Tree` and the `Item` into the
   evaluator's heap (`call_const_macro`, `hir-ty-mir`'s `eval/macro_call.rs`) and passes a
   reference. Every macro crate depends on `const_ast`, blaze's own copy of `crates/const-ast`
   (its files are `const_ast::SOURCE`, which the loader gives the VFS at
   `<sysroot>/lib/rustlib/blaze/const-ast`, a directory not on disk), so the layout and kinds
   the host writes are the ones the views read; a `const_ast` whose root is not blaze's text is
   refused. A parameter of any other type, a count of parameters that is not the macro kind's,
   or an input that does not parse to one item is an error naming it. The output is still
   text, every token with the call site's span; giving output tokens spans from the table waits
   for the const `proc_macro`.
3. The evaluator's call memo, keyed by (`cache_code` of the callee, the hashes of its
   arguments; a view's is its slice's and spans'), for calls whose arguments include a view
   and whose callee is in the macro file, measured on a derive over a large struct: edit one
   field and count the evaluations.
4. A real derive ported, cold, warm and edit times against its proc-macro version, as in the
   stage-0 table below.

## Later

- v2 (anywhere in the crate), above.
- Non-const proc macros in the evaluator: the evaluator is itself a sandbox (no files, no
  FFI, deterministic), so an existing syn-based proc macro could run there with no wasm
  build. The open question is speed: the MIR interpreter is far slower than Wasmtime's
  compiled code, and syn parses every token. Worth measuring on serde_derive before choosing.
- Const macros in dependencies: a crates.io package that declares `const-macros` is built
  the same way; only a workspace that opts in can use one.
- #863's `Graph::map(inputs, args, f)` with `f` a `const fn` or a const closure runs through
  the same call path: the evaluator, keyed by `cache_code`, with no wasm instance
  ([below](#graphmap-with-a-const-fn)).

### `Graph::map` with a const fn

#863's executor runs a `map` node as `f(inputs, args)`. When `f` is const, it doesn't need a
wasm instance:

- What `f` is: when the graph-building function itself ran in the evaluator, a fn pointer
  in its result is already a `FunctionId` (or a closure's id and its captures). When it ran
  as wasm, the pointer is a table slot. blaze built the module, so it maps the slot to the
  function itself, as #863 already requires.
- The call: `call_const_macro`, generalised from text to the graph's value encoding (bytes in,
  bytes out). Each input is written into the evaluator's heap, and the result is read back
  from the returned words. Whether `f` is const is the purity query's answer, not its
  signature's.
- The key: (`cache_code` of `f`, the digests of the inputs, the digest of `args`).
  `cache_code` is fn-cache's hash of `f`'s code and everything it reaches
  (`crates/hir-ty-codegen/fncache.rs`), so editing a helper `f` calls changes the key, and
  reformatting doesn't.
- The result: in blaze's store, like any node's, not in salsa. It outlives the daemon, and a
  second machine with the same key gets a hit.

This waits on #863's `Graph` crate and executor, which don't exist yet. Until then there is
no `map` node to run.

## Performance

"Faster than cargo, always" applies. The measures for a const macro against the same derive
as a proc-macro crate, on the same command:

- cold build of the using crate: the proc-macro path compiles the macro crate (and syn,
  quote); the const path compiles nothing extra. Expected to win by seconds.
- no-op and edit: expansions are salsa-cached either way; a re-expansion is one evaluation
  versus one wasm instance or server round trip. The evaluator is the risk here and is
  measured per expansion before the experiment is proposed as a default.
- load of a fresh daemon: both are kept in the store.

Measured with stage 0 (2026-10-28, ix VM, arm64, 16 CPUs, three alternating rounds, a fresh
`BLAZE_HOME` each): harness/constmacros's derive, against the same derive (same body, taking
`proc_macro::TokenStream`) as a proc-macro crate of the workspace. Each cell is `blaze run`'s
time, and both print the same output at every step:

| | cold | warm | the macro's output edited |
|---|---|---|---|
| proc-macro crate | 20.15-22.93 s | 0.01 s | 0.97-1.01 s |
| const macro | 13.71-14.96 s | 0.01-0.03 s | 0.04-0.05 s |

Nothing in this design touches a package that does not declare `const-macros`.
