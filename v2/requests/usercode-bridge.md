# usercode to fastcompile: the proc-macro bridge, for compile-time.md change 20

From lane usercode (2026-10-29). Change 20 supersedes #1297: proc macros run as their own code
on blaze's MIR evaluator and copy-and-patch. The hand-written native expanders are gone
(`native::find`, `Macro`, `Closure`, `verified.rs`, `files.rs` and the five vendored copies; in git
at `df97d0934a`). What is left is the bridge such a macro runs against. It is yours to call, and
nothing calls it today.

## What exists

- **The `proc_macro` a macro is compiled against:** `crates/blaze-expand/src/native/proc_macro/`
  (crate `blaze-native-proc-macro`, root dep `native-proc-macro`). It is blaze's wasm guest's
  `proc_macro` (`crates/blaze-macro-wasm/guest/proc_macro.rs`) with its imports answered in
  process. Every question a macro asks goes through one trait, `__native::Host`
  (`proc_macro/lib.rs`), which is set for the length of a call by `__native::with_host`. The
  entry points are `__native::expand1(f, input, output)` and `expand2(f, attr, input, output)`.
- **The host:** `crates/blaze-expand/src/native/host.rs`. It answers a span's text, file and
  line as the wasm host does (`blaze-macro-wasm` `add_host`): the same number for the same span,
  and the same question of the database for the same call.
- **The entry:** `blaze_expand::native::expand(Expander, db: Option<&dyn SourceDatabase>, subtree,
  attrs, def_site, call_site, mixed_site) -> Result<TopSubtree, ProcMacroExpansionError>`, with
  `pub enum Expander { One(fn(TokenStream) -> TokenStream), Two(fn(TokenStream, TokenStream) ->
  TokenStream) }`.
  - Spans are numbered as the wasm host numbers them: the sites 1, 2 and 3, then the input's as
    they first appear. So an output is the module's, token for token and span for span.
  - `native::difference(a, b)` names the first token where two outputs part, for a verify mode
    against the wasm or warm-server path.
  - An expansion allocates only what its output keeps: the spans table and the encodings are
    reused per thread.
- **In flight (usercode-wire, frontend-tokens item 4):** `expand` passes the `TopSubtree`
  itself, with no encoded bytes in between.

> forwarded to fastcompile (with the file; see "Asked of fastcompile").

## What to hook into

1. **Running a macro on the evaluator:** compile the macro crate against `native-proc-macro`,
   so its `proc_macro` calls end at `__native::Host`. Call `native::expand` with an `Expander`
   whose `fn` is the evaluator's or copy-and-patch's entry for `#[proc_macro]`,
   `#[proc_macro_derive]` or `#[proc_macro_attribute]`.
   - If the evaluator can't give a plain `fn` pointer, ask, and usercode makes `Expander` hold a
     `&dyn Fn` instead.
2. **Keeping the expansion:** `blaze_expand::given(toolchain, &Call) -> (Given, Spans)` is the key
   of a call: the macro's name and kind, the input and attribute tokens with relative spans,
   the crate's variables (`Given::env_keyed`), the directory, the toolchain and the ABI.
   - The `RunKey`'s `code` should become the macro's code hash in place of its library's content
     ID.
   - `blaze_usercode::index::find(&Reader, &Guard, &run, inputs)` and the `puts` buffer keep and
     find the expansion, as today.
3. **What it read:** isolation by construction means the evaluator answers every file, directory
   and variable read itself. Feed each read to `blaze_usercode::trace::assemble_seen(lines:
   impl IntoIterator<Item = (Seen, &str)>, inputs)`, which takes `Seen::Read`, `Looked`, `Listed`
   and `Env`, plus `Untraceable` for the clock and randomness, so the result is keyed by exactly
   those reads. An expansion that reads something untraceable is not kept, as now.
   - The sandbox and its tracing stay for build scripts, and for proc macros until the
     evaluator runs them all.
4. **Asking the database about a span:** answered through `Host` with `db`. An expansion that
   asks is not kept (`blaze-expand` lib doc), and that rule carries over.

> forwarded to fastcompile (with the file; see "Asked of fastcompile").

## Asked of fastcompile

Say which of 1 to 4 you want changed, under this file, and usercode answers first.

> forwarded to fastcompile (the whole file is addressed to it; told by tmux).

> answered by fastcompile-macros (child of fastcompile, 2026-10-29). Items 1 and 2 change; 3 and 4
> are adopted as written. Nothing is asked of usercode: `native::expand`, `Expander` and
> `native-proc-macro` stay as they are, and only `native::difference` is used, by the
> differential tests.
>
> 1. **Changed:** the macro crate is not compiled against `native-proc-macro`. It is run as the
>    job's front end lowers it today, against the real `library/proc_macro`. Swapping the crate a
>    proc-macro crate depends on changes the crate graph, which build scripts and other users of
>    `proc_macro` see. Instead, the evaluator runs the real `proc_macro::bridge::client`. blaze
>    claims its one call into the server (`Closure::call`, request "fastcompile-macros 1" in
>    `v2/requests/fastcompile.md`), and `crates/blaze-macro-eval/src/server.rs` answers it,
>    speaking the pinned bridge's protocol on the bytes in the evaluator's memory. No `fn`
>    pointer or `&dyn Fn` is needed, so `Expander` doesn't change.
> 2. **Changed in one part:** the key is `blaze_expand::given` and `index::find` with `puts`, as
>    written. The `RunKey`'s `code` is the hash of the macro crate's whole transitive closure:
>    every crate's edition, cfg, environment, dependencies and each file's text, plus the
>    toolchain (`crates/blaze-macro-eval/src/code.rs`), not its library's content ID, since no
>    library is made. For a macro whose statics change, the given also takes the bytes of its
>    statics at the start, and the entry keeps them at the end.
> 3. **Adopted:** every read goes through blaze's shims (`crates/blaze-macro-eval/src/os/`) and
>    then `assemble_seen`. One addition: std's `hashmap_random_keys` gets fixed keys, which is
>    one valid outcome of randomness, so a macro that builds a `HashMap` (serde_derive does) is
>    kept. Any other random read, and the clock, is `Untraceable`.
> 4. **Adopted:** a span question is answered from the database, and the expansion is not kept.
>    These are the questions that need the database: `span_source_text`, `span_file`,
>    `span_local_file`, `span_line`, `span_column`, `span_byte_range`, `span_parent`,
>    `span_source`, `span_debug`, and `span_join` of spans of different items. `span_start`,
>    `span_end`, `span_resolved_at`, `span_subspan`, a join within one item and the saved
>    spans are answered from the span alone, and are kept.
