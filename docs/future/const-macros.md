---
status: plan
summary: Const macros: proc macros written inline as const fn over flat syntax trees, run by the const evaluator. Experimental.
date: 2026-11-12
---
# Const macros: proc macros written inline as `const fn`

Status: design, partly built, behind `experimental = ["const-macros"]`: the evaluator call and
expander (#915), and the manifest key and loading (#959). Of stage 2
([below](#stage-2-the-item-as-a-flat-tree-through-const-views), #1441), the const views are on
main: a macro is given `&const_ast::Item` and returns `const_ast::Output`, a flat tree it builds
with `const_ast::Builder` or `const_ast::quote!`. Stage 0's text interface is removed. Nothing
here is on by default. Issues: #859 (hermetic build graph), #860 (purity as a front-end query),
#867 (const traits, const closures, const heap, step budget), #863 (the build graph's `map`).

## What it is

A const macro is a procedural macro whose body is a `const fn` from syntax trees to a syntax
tree, written in the package that uses it, and expanded by blaze's own const evaluator inside
the compiler's process:

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
#[proc_macro_derive(Named)]
pub const fn named(item: &const_ast::Item) -> const_ast::Output {
    let mut built = const_ast::Builder::new(const_ast::Span::mixed_site());
    let b = &mut built;
    let Some(name) = (match item.as_adt() { Some(adt) => adt.name(), None => None }) else {
        panic!("Named derives a struct, an enum or a union")
    };
    const_ast::quote!(b, items =>
        impl crate::Named for #name { const NAME: &str = stringify!(#name); }
    );
    built.finish(const_ast::Entry::Items)
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

The macro file is not a `proc_macro` crate root under rustc: its API is `const_ast`'s trees
(below), not `proc_macro`'s token streams. Moving a const macro to a real proc-macro crate,
for users who must build with rustc, is rewriting its signature over `syn` and `quote`; the
quoted tokens move unchanged.

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
  makes the graph acyclic. The generated code can: an expansion is a tree of tokens, and `crate::Named`
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

## The tree API in const

A const macro reads and writes blaze's own flat syntax tree (`syntax-tree`'s `flat.rs`), not
token streams. `crates/const-ast` is the whole API, a `no_std` crate with no dependencies that
every macro crate depends on as `const_ast`:

- **Input**: the item (`&const_ast::Item`) or the tokens (`&const_ast::TokenTree`), read
  through typed views with `syntax::ast`'s names (stage 2 step 1, below).
- **Output**: `const_ast::Output`, four parallel arrays (each element's kind, its size, its
  span index and a token's text) and which entry it is (items, statements, an expression, a
  type, a pattern). Sizes are relative, so a range of elements means the same wherever it is.
- **The builder**, the primitive: `const_ast::Builder` opens a node of an opaque `Kind`, writes
  a token (its text, at the builder's site or a span given), splices an input node or token
  by reference (one element, however large the node), and closes. It grows on the const heap
  (`Builder::new`) or writes into arrays the macro gives it (`Builder::over`).
- **The quote**, the library on it and what authors write:
  `const_ast::quote!(b, ENTRY => TOKENS)` with `#x` (splice `x`, any value with a `const fn
  splice`), `#{ code }` (run statements there), `#for x in (e) { .. }` (repeat over an
  iterator) and `##` (a literal `#`). Its tokens are parsed by blaze's parser at `ENTRY` when
  the macro crate is analyzed and lowered to builder calls (`crates/hir-ty-mir`'s
  `const_macro/quote.rs`, the host expander of `const_ast_quote`, a proc-macro twin of
  `const_ast` on the same root file). A quote that is not Rust fails where the macro is
  written, naming what it is not; a repetition that would cut a node in half is refused
  naming the node.

There is no constructor per node (`make::impl_`, `make::fn_`). The views do not reach impls,
functions or expressions, so it would be about 186 hand-written constructors, a third copy of
the grammar after the parser and the views (AGENTS.md, one-concept-one-home). The quote gets
the same guarantee from the one parser, and the host's check of every output (below) catches
what a builder written by hand gets wrong.

**Reflection first** (AGENTS.md, reflection-first). A derive that only reads a type's fields to
implement a trait (`Debug`, `Hash`, a key, a schema) is `for const` over the type
(docs/v2/plan/design/comptime.md), with no macro at all. Const macros are for what reflection
cannot do: new items and names, attribute macros that rewrite an item, function-like macros.

What the macros still need from the other const lanes:

| Needs | Lane | Why |
|---|---|---|
| const heap: growing arrays in const fn, made global at the end | cheap | `Builder::new`; the output outlives the call |
| const traits: `[const] Iterator`, `const Drop` | ctraits | `for` loops and adapters over views; until then a view's cursor has an inherent `const fn next`, which `#for` calls |
| blaze's own check_consts | ccheck | the purity proof; refuses statics, atomics, FFI, pointer-to-int |
| step budget, gaps in the evaluator | cgaps / #867 | a looping macro ends with an error, not a hang |

syn is not const, and running it in the evaluator is the "non-const proc macros in the
evaluator" item under Later.

### Spans and hygiene

rustc's rule holds: a token taken from the input keeps its span; a token the macro writes has
`call_site` (resolves at the call), `mixed_site` (macro_rules hygiene: locals and labels at the
definition, the rest at the call) or `def_site` (the macro's own). blaze keeps the three sites
and differs in four ways:

- A `Builder` is made with its site, and `token` writes at it: there is no silent `call_site`
  (`proc_macro`'s `quote!` writes `call_site`). `mixed_site` is the one to use unless an item
  must be nameable at the call.
- A spliced node is a reference to the input's element, so its tokens keep their spans without
  the macro copying them.
- A span is an index into the call's span table (`def_site` 0, `call_site` 1, `mixed_site` 2,
  then each distinct span of the input in order of first appearance). The macro cannot invent,
  join or inspect a span (no file, line or text, which would make the expansion depend on more
  than its input); an index outside the table is refused by name.
- `def_site` is allowed (unstable upstream).

After evaluation each index is mapped back to the call's real `Span`, so a const macro can't
make spans another call couldn't be given back, and every expansion is keepable, which
blaze-expand can't promise for proc macros.

### Errors

- The macro panics: the expansion fails with the panic's message at the call site, with the
  evaluator's stack of const fn frames in `macros.rs` as notes. Same as a proc macro's panic
  today (`ProcMacroExpansionError::Panic`), more precise.
- The macro returns a tree with `compile_error!`: as for any proc macro.
- The macro returns a tree that is not Rust: the host parses the output's tokens at its entry
  and refuses a tree that is not that parse, node for node, naming the macro and the output
  element ("built `PATH_TYPE` at output element 2 where its tokens parse to `NAME`").
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

## The prototype (stage 0, removed)

Stage 0 was the smallest working path, so the plumbing existed before the const heap and const
traits did. Its text interface is removed; what replaced each part:

- The manifest key and the macro crate in the analysis graph, its `ProcMacros` entry listing
  the file's `#[proc_macro_derive]`, `#[proc_macro_attribute]` and `#[proc_macro]` functions:
  kept as they were.
- The text signature, `fn(&str) -> String` (and `fn(&str, &str) -> String` for an attribute),
  the input printed and the output lexed back with the call site's span on every token:
  replaced by the trees of stage 2, `&const_ast::Item` or `&const_ast::TokenTree` in and
  `const_ast::Output` out, read from the evaluator's heap as four arrays. A `&str` parameter
  or a `String` return is an error naming the macro.
- The expander downcasting the `&dyn SourceDatabase` it is given to `&dyn HirDatabase`: kept;
  sound because the macro crate never depends on the crate being expanded.
- Not requiring `const` (`format!` and `push_str` ran): still the purity query's job (#860,
  lane ccheck); the builder needs neither, and the evaluator's sandbox is the guarantee until
  then.
- The list of macros read at load (a new `#[proc_macro_derive]` needs a reload): unchanged.
- Every output token at the call site's span: replaced by the rules of "Spans and hygiene",
  so a spliced name keeps the input's span and errors point into the macro's output.

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
2. **The item or the tokens as input, a tree as output**: each parameter of a const macro's
   function is `&const_ast::Item`, the tokens parsed back to an item as a flat tree (so it is
   what a proc macro is given: cfg-stripped, the derive itself taken out), or
   `&const_ast::TokenTree`, the tokens as the parser's own token-tree nodes. A derive is
   `const fn(&const_ast::Item) -> const_ast::Output`, an attribute
   `(&const_ast::TokenTree, &const_ast::Item)`, a function-like macro
   `(&const_ast::TokenTree)`:

   ```rust
   #[proc_macro_derive(Fields)]
   pub const fn fields(item: &const_ast::Item) -> const_ast::Output {
       let mut built = const_ast::Builder::new(const_ast::Span::mixed_site());
       let b = &mut built;
       let Some(name) = (match item.as_struct() { Some(adt) => adt.name(), None => None }) else {
           panic!("Fields derives a struct")
       };
       const_ast::quote!(b, items => impl Fields for #name { const NAME_LEN: usize = 3; });
       built.finish(const_ast::Entry::Items)
   }
   ```

   Each token's span is its index in the call's span table: `def_site` 0, `call_site` 1,
   `mixed_site` 2, then each distinct span of the inputs in the order it first appears, one
   table a call, shared by its inputs. The expander (`hir-ty-mir`'s `const_macro.rs`) writes
   the arrays, the `Tree` and the view into the evaluator's heap (`call_const_macro`,
   `eval/macro_call.rs`) and passes a reference. Every macro crate depends on `const_ast`,
   blaze's own copy of `crates/const-ast` (its files are `const_ast::SOURCE`, which the loader
   gives the VFS at `<sysroot>/lib/rustlib/blaze/const-ast`, a directory not on disk), so the
   layout and kinds the host writes are the ones the views read; a `const_ast` whose root is
   not blaze's text is refused. A parameter of any other type, a count of parameters that is
   not the macro kind's, a return type that is not `const_ast::Output`, or an input that does
   not parse to one item is an error naming it.

   The host reads the output's four arrays from the evaluator's heap, builds a flat tree (a
   splice is copied from the host's own input arrays) and turns it into tokens with each
   token's span from the table (`syntax_bridge::call_input`): no text is printed, and none is
   lexed but one built token at a time, to find its kind. Then the check: the tokens are
   parsed once at the output's entry and the built tree must be that parse, node for node, or
   the expansion is refused naming the macro and the first element that differs. The parser is
   the grammar's one home, so this replaces a table of kinds legal under each parent (a second
   grammar), and it is complete, precedence included.
3. The evaluator's call memo, for calls whose arguments include a view and whose callee is in
   the macro file. Its key is the `cache_code` of the callee (which covers the builder
   functions it reaches), each view argument's element slice and span indices, and the scalar
   arguments. A callee given `&mut Builder` is memoized by what it appended: the range is
   position-independent because sizes are relative and splices are references. Measured on a
   derive over a large struct: edit one field and count the evaluations.
4. A real derive ported, cold, warm and edit times against its proc-macro version, as in the
   stage-0 table below.

## Later

- v2 (anywhere in the crate), above.
- hir-expand takes the checked tree: today the expander parses an expansion's tokens once as
  its check and hir-expand parses them again (`Expanded` carries tokens); passing it the
  checked tree drops the second parse.
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

That table is stage 0's, with text output, and is the baseline the trees are measured against
once built. Estimates, not measured, for a derive writing one method per field over 20 fields:

- text: about 1,400 output bytes at about 25 MIR steps a byte (`push_str`'s loop), about
  35,000 steps, then a lex and a parse of 1.4 KB on the host;
- builder: about 30 elements a field (a spliced type is one element) and 60 fixed, about 660
  pushes at about 35 steps, about 23,000 steps with no growth; on the host one tree build, one
  bridge walk and one parse as the check, and hir-expand's parse as before.

Nothing in this design touches a package that does not declare `const-macros`.
