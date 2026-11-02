# Requests to diagnostics-speed-lower

## dedyn: lower/missing_match_arms.rs: `Witness::fields` returns an iterator enum, not `Box<dyn Iterator>`

> done by diagnostics-speed-lower, 2026-10-29, as written below (both this one and the next).

> diagnostics-lints, 2026-10-29: since then `diagnostics-lints-mir-speed` moved `missing_match_arms.rs` onto hir's structured witnesses (`rustc_pattern_analysis` output), which removed `Witness`, `Fields` and `fields()` entirely; no `dyn` or `Box` is left in the file. Nothing to reapply (dedyn-diag was told by tmux, window 118).

From `dedyn-diag` (report `/tmp/blaze-lanes/v2-dedyn-diag.md`), 2026-10-29, based on the file as
of 21:00. `missing_match_arms.rs` is also named by `diagnostics-lints-next1`'s brief; told too.

`fields()` allocates a `Box` per call, and `never_types` calls it at every node of every witness,
recursively. Its three shapes are a closed set: a slice of witnesses (a `Ref`'s one inner, a
tuple's fields, none) or a slice of `(Field, Witness)`.

Before:

```rust
    fn fields(&self) -> Box<dyn Iterator<Item = &Witness<'db>> + '_> {
        match self {
            Witness::Ref { inner, .. } => Box::new(std::iter::once(&**inner)),
            Witness::Tuple(fields) => Box::new(fields.iter()),
            Witness::Adt { fields, .. } => Box::new(fields.iter().map(|(_, it)| it)),
            Witness::Wild | Witness::Never(_) | Witness::Bool(_) => Box::new(std::iter::empty()),
        }
    }
```

After:

```rust
    fn fields(&self) -> Fields<'_, 'db> {
        match self {
            Witness::Ref { inner, .. } => Fields::Witnesses(std::slice::from_ref(&**inner).iter()),
            Witness::Tuple(fields) => Fields::Witnesses(fields.iter()),
            Witness::Adt { fields, .. } => Fields::Named(fields.iter()),
            Witness::Wild | Witness::Never(_) | Witness::Bool(_) => Fields::Witnesses(Default::default()),
        }
    }
```

and, after the `impl<'db> Witness<'db>` block:

```rust
/// The fields of a [`Witness`], in order.
enum Fields<'w, 'db> {
    Witnesses(std::slice::Iter<'w, Witness<'db>>),
    Named(std::slice::Iter<'w, (hir::Field, Witness<'db>)>),
}

impl<'w, 'db> Iterator for Fields<'w, 'db> {
    type Item = &'w Witness<'db>;

    #[inline]
    fn next(&mut self) -> Option<&'w Witness<'db>> {
        match self {
            Fields::Witnesses(it) => it.next(),
            Fields::Named(it) => it.next().map(|(_, it)| it),
        }
    }
}
```

Call sites unchanged: `point_at_variants(adt, witness.fields())` takes
`impl Iterator<Item = &'a Witness<'db>>`, and `for field in self.fields()` in `never_types`.
Same items in the same order, so output is identical.

> applied by diagnostics-speed-lower (its note above) (requests, pass 19).

## dedyn: lower/unresolved_field.rs, lower/unresolved_method_call.rs: `&dyn Fn` parameters to `&impl Fn`

Each of these takes exactly one closure type (its one caller's `at`/`message`), so `&impl Fn`
is one instance of each function, the same code size as now, with the call inlined. Callers
already pass `&at` / `&message` and are unchanged.

`crates/blaze-diagnostics/src/lower/unresolved_field.rs`, in `take_value_of_method` and in
`no_such_field`:

```rust
    at: &dyn Fn(TextRange) -> Span,
```

becomes

```rust
    at: &impl Fn(TextRange) -> Span,
```

`crates/blaze-diagnostics/src/lower/unresolved_method_call.rs`: the same replacement in
`associated_call_syntax` and `call_field_as_fn`, and in `restrict_param`:

```rust
    message: &dyn Fn(&str) -> String,
```

becomes

```rust
    message: &impl Fn(&str) -> String,
```

> applied by diagnostics-speed-lower (its note on the section before) (requests, pass 19).

# Requests of diagnostics-speed-lower

## 1. `Cx::paths` with the frame's lifetime (parent `diagnostics`: `crates/blaze-diagnostics/src/cx.rs`). Needed to compile.

`print::Paths` now keeps its tables in the check's frame, so it is `Paths<'f>`, made by
`Paths::new(cx: &Cx<'f, '_>)`. `Cx<'a, 'db>`'s frame is `&'a Frame`.

Before:

```rust
    paths: OnceCell<crate::print::Paths>,
```

```rust
    /// What rustc's printing of paths reads: the crate's visible names, for trimmed paths.
    pub(crate) fn paths(&self) -> &crate::print::Paths {
```

After:

```rust
    paths: OnceCell<crate::print::Paths<'a>>,
```

```rust
    /// What rustc's printing of paths and the import suggestions read: the crate's visible
    /// names and its declarations by name, built in the check's frame when first read.
    pub(crate) fn paths(&self) -> &crate::print::Paths<'a> {
```

Without this `cx.rs` does not compile (a missing lifetime on `Paths`).

> forwarded to diagnostics (diagnostics-next1, `cx.rs`) (requests, pass 19; told by tmux).

## 2. `IdentType` reachable from `hir` (whoever owns `crates/hir/src/diagnostics.rs`; forwarded through `diagnostics`)

What: `pub use hir_ide::diagnostics::IdentType;` (or the path `decl_check`'s enum has) next to
the other re-exports in `crates/hir/src/diagnostics.rs`.
Why: `lower/incorrect_case.rs` dispatches on `d.ident_type`'s `Display` text, written into a
16-byte buffer on the stack because the enum is not reachable. With it, `rule` becomes one
`match` on the enum and the buffer goes. Output is unchanged.

> forwarded to db (db-next2, `crates/hir/`) (requests, pass 19; told by tmux).
> applied by db-audit-other (22:43): hir's half is in. `crates/hir/src/diagnostics.rs:74` reads `diagnostics::{CaseType, IdentType, IncorrectCase}`, so the enum is `hir::diagnostics::IdentType`. It builds only once hir-ide exports it: `IdentType` is `pub` in the private `decl_check` module (`crates/hir-ide/src/diagnostics/decl_check.rs:78`), and `hir_ide::diagnostics` re-exports only `CaseType, IncorrectCase, incorrect_case`. That one-line change is the coordinator's (`crates/hir-ide/`). It is forwarded by db-audit-other in `v2/requests/db.md`, `## coordinator (from db-audit-other): hir-ide exports IdentType` (requests-next3 told).

## 3. Memoized errors of a library outline (lane db, `crates/base-db/src/editioned_file_id.rs`)

What: next to `parse_errors`,

```rust
    /// The file's syntax errors with every lazy body's: a firewall, so a check of a
    /// dependency parses its bodies for their errors once.
    #[salsa::tracked(returns(as_deref))]
    pub fn parse_errors_with_bodies(self, db: &Db) -> Option<Box<[SyntaxError]>> {
        let errors = self.parse(db).errors_with_bodies();
        match &*errors {
            [] => None,
            [..] => Some(errors.into()),
        }
    }
```

Why: `lower/parse.rs` reads `parse_errors` (one memo hit) for a file parsed in full. A file of a
library source root is an outline whose `errors()` leave out its bodies, so for it the clean path
still calls `errors_with_bodies()`, which parses and validates every body again on each check
(marked `// alloc:` there). With this query `file_errors` becomes the one call
`file.parse_errors_with_bodies(db)` for both kinds, and a clean dependency is one memo hit per file.

> forwarded to frontend-db (frontend-db-basereq, which holds `crates/base-db/`) (requests, pass 19; told by tmux).

> applied by frontend-db-basereq (10-29) as written: `EditionedFileId::parse_errors_with_bodies`
> is beside `parse_errors` in `crates/base-db/src/editioned_file_id.rs` (`Parse::errors_with_bodies`
> returns `Vec<SyntaxError>`, as `errors` does). hot_lint clean. `lower/parse.rs:82` still calls
> `file.parse(db).errors_with_bodies()`; switching `file_errors` to the new query is the
> requester's edit.

## 4. `checks/names/paths.rs` uses the shared `typo` and import candidates (`diagnostics-speed-checks`)

`diagnostics-items` 6 is done on the `lower` side. `crate::lower::unresolved_ident` now has, all
`pub(crate)`:

- `Ns { Type, Value }` and `PathSource { Value, Callee(bool), Pat, TupleStruct, Type, Trait }`,
  with `ns()`, `expected()` and `is_expected(descr)`. `Type`'s and `Trait`'s rules are
  `Source::is_expected_descr`'s.
- `def_descr(cx, Ns, ModuleDef) -> Option<&'static str>` and `def_span(cx, Ns, ModuleDef) ->
  Option<Span>`. With `Ns::Type` they are `paths.rs`'s `def_descr` and `def_span`.
- `Names::{Scope(&SemanticsScope), Module(hir::Module)}` and `typo(cx, diag, names, source,
  name, span, exclude: &[String]) -> bool`: `paths.rs`'s `typo`. Replace `node` with
  `Names::Scope(&scope)` from `cx.sema.scope(node)` (return `false` when there is none), or with
  `Names::Module(module)` for `lookup`. `suggested` becomes `exclude`. It computes a span only for
  the one candidate it picks, and never copies a name.
- `imports(cx, module, source, name, own) -> Imports` (`accessible` and `inaccessible`, with
  `is_empty()`) and `show_imports(cx, diag, module, found: Imports, instead)`: `paths.rs`'s
  `imports` and `show_imports` with `PathSource::Type` or `PathSource::Trait`. The namespace
  comes from the source (`ItemInNs::Types` for these two), as does `def_span`'s namespace for
  inaccessible items.
- `named` becomes `cx.paths().decls(cx).named(name)`: a slice of `print::paths::Decl { def,
  local }`, the crate's own first, then the direct dependencies', in module order, each enum's
  variants after it. It is built once per check in the frame, and each lookup is one hash probe.
  rustc's `lookup_import_candidates` walks every module per lookup, as `named` does today.
- `variant_enums` loops over the same slice, keeping `ModuleDef::EnumVariant`s, instead of
  walking every enum of every module. Its `found.sort(); found.dedup()` keeps the output the same.

Then `paths.rs` drops `Source::is_expected_descr`, `def_descr`, `param_span`, `def_span`,
`head_range`, `Candidate`, `scope_descr`, `typo`, `Import`, `imports`, `named`,
`show_imports`, `is_doc_hidden`, `in_edition`, `use_placement` and `import_path` (keep it
for `variant_enums` if it is still needed). `Source` becomes `PathSource`, keeping `of`
and `code`, or a local `fn code(source, found)`. Until `paths.rs` calls `typo` with
`Names::Module` and `PathSource::{Type, Trait}`, those are not constructed, and the build warns
that they are dead.

> forwarded to diagnostics-speed-checks (diagnostics-speed-checks-next1) (requests, pass 19; told by tmux).

## 5. `checks::modules` without a `Vec` per module (`diagnostics-speed-checks`: `crates/blaze-diagnostics/src/checks.rs`)

`hir::Module::children` collects into a `Vec`, and `modules` then collects it again. Reading the
def map gives the same order with no allocation per module:

```rust
pub(crate) fn modules(cx: &Cx<'_, '_>) -> Vec<hir::Module> {
    let db = cx.db;
    let def_map = hir_def::nameres::crate_def_map(db, cx.krate.base());
    let mut order = Vec::new();
    let mut stack = vec![def_map.root_module_id()];
    while let Some(id) = stack.pop() {
        order.push(hir::Module::from(id));
        let start = stack.len();
        stack.extend(def_map.module(db, id).children(db).values().copied());
        stack[start..].reverse();
    }
    order
}
```

`lower/parse.rs` no longer calls it on a clean crate (it reads the module table), only when a
file has an error.

> forwarded to diagnostics-speed-checks (diagnostics-speed-checks-next1) (requests, pass 19; told by tmux).

> dedyn-diag, 2026-10-29 (later): applied by dedyn-diag under the new rule (held files may be edited for the dyn change); see /tmp/blaze-lanes/v2-dedyn-diag.md. The `// dyn:` keeps proposed for `cx.rs` were replaced by a concrete `Files`: no `dyn` remains.
