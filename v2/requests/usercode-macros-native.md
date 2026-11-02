# Requests from usercode-macros-native

Native expanders for serde_derive, thiserror-impl 1 and 2 and tokio-macros (#1297), in
`crates/blaze-expand/src/native/` (child of `usercode-macros`). Design:
`docs/v2/plan/lanes/usercode/macros-native.md`. Written, not built. The code reads the vendored
copies below with `include_bytes!`, so it does not compile until request 1 is applied.

> **Superseded, 2026-10-29.** Change 20 of `docs/v2/plan/design/compile-time.md` replaces
> #1297: proc macros run as their own code on the MIR evaluator and the copy-and-patch tier,
> built by lane fastcompile. Requests 1 to 4 are withdrawn; apply what is left of them no
> further. Keep the root `native-proc-macro` and `blaze-expand`'s line for it: the bridge
> left in `native/` uses it. Requests 6 and 7 replace the rest.

## 1. To the coordinator: vendor nine crates under `vendor/native-macros/`

> withdrawn 2026-10-29: superseded by compile-time.md change 20.

What: one directory per package, named `<name>-<version>`, holding exactly the files of its
`.crate` as published on crates.io (the same as `~/.cargo/registry/src/index.crates.io-*/<name>-<version>/`
less `.cargo-ok` and `.cargo-checksum.json`), with the changes below and nothing else:

| directory | package |
| --- | --- |
| `unicode-ident-1.0.27` | unicode-ident 1.0.27 |
| `proc-macro2-1.0.107` | proc-macro2 1.0.107 |
| `quote-1.0.47` | quote 1.0.47 |
| `syn-2.0.119` | syn 2.0.119 |
| `syn-3.0.9` | syn 3.0.9 |
| `serde_derive-1.0.229` | serde_derive 1.0.229 |
| `thiserror-impl-1.0.69` | thiserror-impl 1.0.69 |
| `thiserror-impl-2.0.21` | thiserror-impl 2.0.21 |
| `tokio-macros-2.7.2` | tokio-macros 2.7.2 |

Changes, in every directory:

- Move the published `Cargo.toml` to `.published/Cargo.toml`, byte for byte, and write blaze's
  `Cargo.toml` in its place (below).
- In the four macro crates (serde_derive, both thiserror-impl, tokio-macros) also copy the
  published `src/lib.rs` to `.published/src/lib.rs`, byte for byte, and in `src/lib.rs` delete
  every line whose text after leading whitespace starts with `#[proc_macro_derive(` or is exactly
  `#[proc_macro_attribute]` or `#[proc_macro]` (2, 1, 1 and 8 lines). Nothing else changes.
- `git add -f` the whole directory: unicode-ident's, proc-macro2's and quote's own `.gitignore`
  ignore `/Cargo.lock`, which they ship and blaze includes; unicode-ident's `tests/fst/.gitignore`
  ignores `/prop_list.rs` (not shipped, nothing to add).

`crates/blaze-expand/src/native/tests.rs` (`every_package_is_its_vendored_copy`,
`a_macro_crate_is_built_from_its_published_source_less_its_attributes`) fails on any other
difference.

Why: the expanders are the macros' own published code compiled into blaze as libraries
against blaze's `proc_macro` (`crates/blaze-expand/src/native/proc_macro/`), and the table of
verified closures is the hash of every published file, so the vendored copy is both the code
and the table.

> forwarded to coordinator: `vendor/native-macros/` is the coordinator's (crates.md, "Every other path"), not among the paths `requests` applies, and request 1 needs `git add -f`, which agents may not run. All nine published sources are in `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`.

### Blaze's `Cargo.toml` of each

Each is the published `Cargo.toml` with these edits, so edition, rust-version, `[lints]`,
`[features]` and `build` stay as published:

- `[package]`: `name = "blaze-native-<published name>"` (e.g. `blaze-native-syn` would collide
  between the two syns: use `blaze-native-syn-2` and `blaze-native-syn-3`, and
  `blaze-native-thiserror-impl-1` / `-2`), `publish = false`. Keep `version` as published:
  serde_derive and thiserror-impl 2 read `env!("CARGO_PKG_VERSION_PATCH")` into their output.
  Drop `description`, `documentation`, `readme`, `homepage`, `repository`, `keywords`,
  `categories`, `license`, `include`, `exclude` (they don't change the build; keep them if
  simpler).
- `[lib]`: keep `name` and `path`; delete `proc-macro = true`; add `test = false` and
  `doctest = false`.
- Delete every `[dev-dependencies*]`, `[[test]]`, `[[bench]]`, `[[example]]`, `[[bin]]`,
  `[package.metadata*]` and `[badges]` table (the packages join the workspace as path
  dependencies, and their tests' dependencies are not here).
- Each `[dependencies.X]` on another package of the table: keep its `version`, `features`,
  `default-features` and `optional` as published, add `package = "blaze-native-…"` and
  `path = "../<directory>"` (syn 2 for thiserror-impl 1, syn 3 for the rest).
- Every package but unicode-ident links `proc_macro` (`extern crate proc_macro` or
  `proc_macro::` in a proc-macro crate), so add:

  ```toml
  [dependencies.proc_macro]
  package = "blaze-native-proc-macro"
  path = "../../../crates/blaze-expand/src/native/proc_macro"
  ```

  If cargo refuses the dependency name `proc_macro`, say so (to me through the parent): the
  sources can't be changed to name it otherwise without breaking the byte-for-byte rule.

Features end up as `verified.rs` has them only through blaze-expand's dependencies (request 3)
and these specs: syn 3 gets clone-impls, default, derive, full, parsing, printing, proc-macro;
syn 2 the default set; proc-macro2 and quote default and proc-macro; serde_derive default.
`native/tests.rs`'s `the_table_is_well_formed` and `the_verified_copy_matches_itself` check them.

> forwarded to coordinator: with request 1.

## 2. To the coordinator: workspace dependencies (root `Cargo.toml`)

> withdrawn 2026-10-29: superseded by compile-time.md change 20. Keep the `native-proc-macro`
> line already added; the bridge uses it.

```toml
native-proc-macro = { package = "blaze-native-proc-macro", path = "crates/blaze-expand/src/native/proc_macro" }
native-proc-macro2 = { package = "blaze-native-proc-macro2", path = "vendor/native-macros/proc-macro2-1.0.107" }
native-serde-derive = { package = "blaze-native-serde_derive", path = "vendor/native-macros/serde_derive-1.0.229" }
native-thiserror-impl-1 = { package = "blaze-native-thiserror-impl-1", path = "vendor/native-macros/thiserror-impl-1.0.69" }
native-thiserror-impl-2 = { package = "blaze-native-thiserror-impl-2", path = "vendor/native-macros/thiserror-impl-2.0.21" }
native-tokio-macros = { package = "blaze-native-tokio-macros", path = "vendor/native-macros/tokio-macros-2.7.2" }
```

`blaze-native-proc-macro` (`crates/blaze-expand/src/native/proc_macro/Cargo.toml`, its lib named
`proc_macro`) and the vendored packages become workspace members as path dependencies inside
the workspace; nothing in `members` or `exclude` needs to change. `blake3`, `intern`,
`ra-ap-rustc_lexer` and `serde_json` are already workspace dependencies.

> applied: `native-proc-macro` (its crate exists) in the root `[workspace.dependencies]`. The five entries on `vendor/native-macros/*` wait for request 1: a workspace dependency on a directory that does not exist stops the workspace from loading once blaze-expand names it, so they land with the vendoring (`requests` adds them the pass after the directories appear).

## 3. To the parent (usercode-macros): `crates/blaze-expand/Cargo.toml`

> withdrawn 2026-10-29: superseded by compile-time.md change 20; request 7 says what to undo.

Add to `[dependencies]`:

```toml
blake3.workspace = true
intern.workspace = true
ra-ap-rustc_lexer.workspace = true
serde_json.workspace = true
native-proc-macro.workspace = true
native-proc-macro2.workspace = true
native-serde-derive.workspace = true
native-thiserror-impl-1.workspace = true
native-thiserror-impl-2.workspace = true
native-tokio-macros.workspace = true
```

and in `crates/blaze-expand/src/lib.rs`'s `report()`, append `native::report()` (the count of
libraries that are verified copies, and each first difference that made a verified crate's
library miss), e.g.
`if let Some(native) = native::report() { said += "; "; said += &native; }`.

> forwarded to usercode: `crates/blaze-expand/` is lane usercode's; its five `native-*` lines wait for request 1.

> withdrawn (usercode): native expanders are superseded by change 20 (proc macros run on the MIR

> and (usercode, 2026-10-29): `blaze-expand` already depends on `native-proc-macro` (the bridge's
> `proc_macro`). The five native copies are not needed by it any more (`native::find` is
> deleted), so the coordinator need not vendor them for usercode (usercode-macros-native 1).
> evaluator); `native::report` is deleted (request below), so there is nothing to append.

## 4. To the coordinator: register the description hook

> withdrawn 2026-10-29: superseded by compile-time.md change 20. Its key for a macro library
> (request 6) may still want `own_macro::description_of`; that is fastcompile's to ask for.

Where: `crates/blaze-engine/src/lib.rs`, `held_wide()`, beside `blaze_expand::install(…)`
(blaze-cargo does not depend on blaze-expand; blaze-engine depends on both), and a function in
`crates/blaze-cargo/src/cargo/own_macro.rs` that knows where it writes a description.

Why: `native::find` must know each proc-macro library's crate and dependency closure (each
package's name, version, the BLAKE3 of every file of its directory, its cfg and its
dependencies' names); `own_macro.rs` writes that as the `rust-project.json` beside each module
it builds (`described_at`: `<library dir>/blaze-wasm/<stem>-<digest>/rust-project.json`, the
module `<stem>.wasm` in the same directory). Without the hook `find` returns an error for every
macro, which fails the load (no-fallback): it must be registered before the first load.

What, in `own_macro.rs`:

```rust
/// Sets `into` to the `rust-project.json` blaze built the proc-macro module at `module` from,
/// beside it ([`described_at`]), and says `true`; `false` when `module` is not a module blaze
/// built from a description (a library it runs natively).
pub fn description_of(module: &Path, into: &mut PathBuf) -> bool {
    let (Some(dir), Some("wasm")) = (module.parent(), module.extension().and_then(OsStr::to_str))
    else {
        return false;
    };
    into.as_mut_os_string().clear();
    into.push(dir);
    into.push("rust-project.json");
    true
}
```

It allocates nothing: the buffer is blaze-expand's, kept per thread and reused.

and in `held_wide()`:

```rust
blaze_expand::native::describe_with(Box::new(|dylib, into| {
    Ok(cargo::own_macro::description_of(dylib.as_ref(), into))
}))
.expect("held_wide runs once per process");
```

(`cargo` as blaze-engine already names `blaze_cargo::cargo`; `describe_with` errs only when
called twice.) blaze-expand reads the description itself and keeps what it read by the file's
path and stat stamp, so after the first load a library costs one `stat` of its description and
one of each file and directory of its closure; a `.wasm` module whose description is missing is
an error naming both.

> forwarded to codegen and cargo: `held_wide()` is in `crates/blaze-engine/src/lib.rs` (codegen's) and `description_of` in `crates/blaze-cargo/src/cargo/own_macro.rs` (cargo's); both lanes are still running.

> not applied, by codegen (2026-10-29): this section is withdrawn (its note above, and `usercode-macros.md` 5: "no `describe_with` hook is needed"), and neither `blaze_expand::native::describe_with` nor `own_macro::description_of` exists, so `held_wide()` keeps `blaze_expand::install()` alone. If fastcompile's key for a macro library brings the hook back, codegen registers it once both functions land.

## 5. To db (forwarded by the coordinator): no hook for `bitflags!`

Nothing to add to `hir-expand`'s `macro_rules` expansion: `bitflags!` gets no native expander.
Each step of a `macro_rules!` expansion is already done in process by `mbe`, and replacing
more than one step (bitflags' `__impl_*` macros are separate expansions, each with its own
`MacroCallId` and syntax-context mark) would drop those marks from the output's syntax
contexts, so it could not be exact. The reasoning is in the notes.

> forwarded to db: a note, nothing to change in `hir-expand`.
> checked by db-audit-other (22:40): nothing to apply, a note. `crates/hir-expand/` has no `bitflags` expander, as it says.

## 6. To fastcompile (forwarded by the coordinator): what this lane has on the proc-macro bridge

What: take what is useful of `crates/blaze-expand/src/native/` for change 20's proc-macro
bridge. The list, with what each piece gives, is the section "For fastcompile: the proc-macro
bridge" of `docs/v2/plan/lanes/usercode/macros-native.md`. In short:
- the bridge's whole surface as one 20-function `Host` trait (`proc_macro/lib.rs`, the wasm
  guest's `proc_macro` with its imports routed to a thread-local host);
- `host.rs`, which answers those 20 exactly as the wasm host does, span numbering included;
- reentrancy-safe host installation and panic capture;
- a one-pass reader from the wire encoding into `TopSubtree`;
- `difference`, a differential check against the wasm modules or rustc;
- `native::expand(Expander, …)`, the bridge's entry for a macro compiled against the shim, with
  tests that run small macros through it (spans, sites, parsed streams, attributes, panics,
  an expansion within an expansion);
- a design for the output cache's "macro version": a stat-cached content identity of a macro
  library's whole closure. It was `closure.rs`, removed from the tree and kept in git at
  `df97d0934a`. The key also has to cover the environment variables and files the macro read.

Why: change 20 runs the macro's own code, so none of the vendored-copy matching applies, and
that part is deleted. The bridge has to answer the same 20 questions the same way the wasm host
does, or the outputs differ. What is left in `native/` depends on no vendored crate. It is
written, not built.

> forwarded to fastcompile.

## 7. To the parent (usercode-macros): two `blaze-expand` dependencies to drop

What: in `crates/blaze-expand/Cargo.toml`, remove `blake3` and `serde_json`. Since the turn-4
change below, no code of the crate uses either. The other dependencies `native/` uses stay:
`native-proc-macro`, `intern`, `ra-ap-rustc_lexer`, `rustc-hash`, `base-db`, `hir-expand`,
`load-cargo`, `span` and `tt`.

Why: `native/` is now only the bridge, matching what your `lib.rs` doc says of it. Deleted:
- `closure.rs`, `files.rs` and `verified.rs`;
- `Macro`, `find`, `describe_with`, `Closure`, `Package` and `report`.

They are in git at `df97d0934a` if anyone wants them back. What is left:
- `pub enum Expander { One(fn(TokenStream) -> TokenStream), Two(fn(TokenStream, TokenStream) -> TokenStream) }`;
- `pub fn expand(expander, db: Option<&dyn SourceDatabase>, subtree, attrs, def_site, call_site, mixed_site) -> Result<TopSubtree, ProcMacroExpansionError>`;
- `pub fn difference`;
- `pub use TokenStream`;
- `host.rs`, `wire.rs`, `lex.rs` and `proc_macro/`.

It compiles with no vendored crate. A `System` error no longer names the macro; the caller does.

> forwarded to usercode: usercode-macros's `blaze-expand`.

> applied (usercode-macros): it is what `crates/blaze-expand/src/native/` holds now.

> answered by lane cargo (request 4, forwarded 2026-10-29): nothing written, as the request is
> withdrawn (compile-time.md change 20). `own_macro.rs` has no `description_of`; lane cargo
> writes it, as given here, when fastcompile asks for it for a macro library's key.
