# Relocation-table migration: `fastcompile` (blaze-premap, blaze-lazy, blaze-global, blaze-stencil)

Checked against `crates/blaze-codegen/src/relocs.rs`, `lib.rs` and `content.rs` as they are in the
worktree. Five files need edits: two in `blaze-premap` source, one in `blaze-lazy` source and two
test files in each crate. `blaze-global` and `blaze-stencil` need nothing: neither touches a
relocation of codegen's.

Name clash to watch: `blaze_codegen::settings::Relocation` (the relocation model, `Pic`/`Static`) is
imported in `blaze-premap/src/tests.rs` and `blaze-lazy/src/tests.rs`, so those two files name the
new row type by path, `blaze_codegen::Relocation`, rather than importing it.

No file in these crates hard-codes a content ID or store key of codegen's (`blaze v2 data`/`code`/
`template` domains, `BZT`/`BZS` magic). The hex strings in `blaze-global/src/tests.rs` (lines
703–726) are Cargo.lock checksums; `blaze-stencil`'s `BZSTENC1` is its own table format.

### crates/blaze-premap/src/key.rs

- **Line 19**, the import.
  Before:
  ```rust
  use blaze_codegen::{Arch, CompiledFunction, Init, InstanceKey, Reloc, RelocKind, Settings, Static, Target};
  ```
  After:
  ```rust
  use blaze_codegen::{Arch, CompiledFunction, Init, InstanceKey, RelocKind, Relocation, Relocs, Settings, Static, Target};
  ```
  Why: `relocs_into` now takes the table and destructures the row it yields; `Reloc` is no longer
  used here.

- **Lines 326–332**, `relocs_into`.
  Before:
  ```rust
  fn relocs_into(hasher: &mut Hasher, relocs: &[Reloc]) {
      count(hasher, relocs.len());
      for Reloc { at, kind, target, addend } in relocs {
          hasher.update(&at.to_le_bytes()).update(&[reloc_tag(*kind)]).update(&addend.to_le_bytes());
          target_into(hasher, target);
      }
  }
  ```
  After:
  ```rust
  fn relocs_into(hasher: &mut Hasher, relocs: &Relocs) {
      count(hasher, relocs.len());
      for Relocation { at, kind, target, addend } in relocs {
          hasher.update(&at.to_le_bytes()).update(&[reloc_tag(kind)]).update(&addend.to_le_bytes());
          target_into(hasher, target);
      }
  }
  ```
  Why: callers at lines 226 and 235 pass `&Relocs` (from `Init::Bytes { relocs }` and
  `Data { relocs }`), which no longer coerces to `&[Reloc]`. Iterating `&Relocs` yields
  `Relocation<&Target>` by value, so `kind` is a `RelocKind`, not a reference (no `*`), and
  `target` is already `&Target`. Each relocation hashes to the same bytes as before (`at` u32,
  tag, `addend` i64, target), so `STATIC_DOMAIN` needs no bump. The only difference is order:
  rows now come sorted by `at`, so one set of relocations given in two orders now gets one
  definition ID. That can only turn a miss into a hit for equal content, never a hit for
  different content.
  (Optional, faster: hash `relocs.rows()` as `row.to_le_bytes()`, then each of
  `relocs.targets()` once, then `relocs.wide()`, the way `blaze_codegen::content::relocs_into`
  does. Repeated targets are then hashed once each. That changes the bytes, so it would need
  `STATIC_DOMAIN` bumped to `"blaze v2 premap static 2"`.)

- **Lines 334–344**, `target_into`: this was already broken before the table change. `Target::Coverage(u32)` is in
  `HEAD`'s `blaze_codegen::Target`, and this match has no arm for it (E0004, non-exhaustive).
  Before:
  ```rust
          Target::Position(span) => hasher.update(&[6]).id(&span.anchor).update(&span.index.to_le_bytes()),
      };
  ```
  After:
  ```rust
          Target::Position(span) => hasher.update(&[6]).id(&span.anchor).update(&span.index.to_le_bytes()),
          Target::Coverage(array) => hasher.update(&[7]).update(&array.to_le_bytes()),
      };
  ```
  Why: needed for the crate to build at all. The relocation change doesn't cause it, but this
  file won't compile without it.

> forwarded to fastcompile (holds `crates/blaze-premap/`, `crates/blaze-lazy/`) (requests, pass 18; told by tmux).
> done (fastcompile-sol-premap): `relocs_into(&Relocs)` hashes the rows as stored, each target once, then the wide addends; `STATIC_DOMAIN` is `"blaze v2 premap static 2"`; `Target::Coverage` is hashed (tag 7). Not built here (no builds in this pass).

### crates/blaze-premap/src/split.rs

- **Lines 151–156**, a static's own relocations.
  Before:
  ```rust
          let relocs = match &value.init {
              blaze_codegen::Init::Bytes { relocs, .. } => relocs.as_slice(),
              blaze_codegen::Init::Zeroed { .. } => &[],
          };
          for reloc in relocs.iter().chain(value.data.iter().flat_map(|it| it.relocs.iter())) {
              match &reloc.target {
  ```
  After:
  ```rust
          let relocs = match &value.init {
              blaze_codegen::Init::Bytes { relocs, .. } => Some(relocs),
              blaze_codegen::Init::Zeroed { .. } => None,
          };
          for reloc in relocs.into_iter().flatten().chain(value.data.iter().flat_map(|it| it.relocs.iter())) {
              match reloc.target {
  ```
  Why: `Relocs` has no `as_slice`, and an empty `&[]` is not a table. `Option<&Relocs>`
  flattened yields `Relocation<&Target>`, the same item as `Data::relocs.iter()`, so the chain
  type-checks. `match reloc.target` on `&Target` binds `key: &InstanceKey` and
  `item, instance` by reference, as `is_dep`/`is_dep_static` take them. The old
  `match &reloc.target` would also compile (default binding through `&&`); the new form is just
  clearer.

- **Line 123**, optional:
  Before: `            refers(&reloc.target)?;`
  After: `            refers(reloc.target)?;`
  Why: `reloc.target` is already `&Target`. The old `&&Target` still compiles, because the
  closure's annotated `&Target` parameter deref-coerces, but the extra `&` is noise.

> forwarded to fastcompile (requests, pass 18; told by tmux).
> done (fastcompile-sol-premap): `image_part` checks each function's and static's `Relocs::targets()` (each target once) against hashed sets of codegen's ownership, not `as_slice`.

### crates/blaze-premap/src/tests.rs

- **Line 3**, the import.
  Before:
  ```rust
  use blaze_codegen::{Arch, CompiledFunction, Init, InstanceKey, Reloc, RelocKind, Settings, Static, Target};
  ```
  After:
  ```rust
  use blaze_codegen::{Arch, CompiledFunction, Init, InstanceKey, RelocKind, Relocs, Settings, Static, Target};
  ```
  Why: `Reloc` is now the stored row, built only by `Reloc::new`. `Relocation` can't be imported
  because line 2 imports `settings::Relocation`, so the helpers below name it by path.

- **Lines 43 and 49**, `function`.
  Before:
  ```rust
  fn function(n: u64, relocs: Vec<Reloc>) -> CompiledFunction {
  ```
  ```rust
          relocs,
  ```
  After:
  ```rust
  fn function(n: u64, relocs: Vec<blaze_codegen::Relocation>) -> CompiledFunction {
  ```
  ```rust
          relocs: relocs.into(),
  ```
  Why: `CompiledFunction::relocs` is `Relocs`, and the `From<Vec<Relocation>>` impl builds it.
  Every caller (`function(n, vec![reloc(..), ..])`, `function(n, Vec::new())`, lines 311–331
  and 758) stays as it is.

- **Lines 44–55**, same literal: this was already broken before the table change. `CompiledFunction` in `HEAD` has
  `coverage: Option<Covered>`, and this literal doesn't set it (E0063).
  Before:
  ```rust
          lines: Vec::new(),
      }
  }
  ```
  After:
  ```rust
          lines: Vec::new(),
          coverage: None,
      }
  }
  ```

- **Lines 58–60**, `reloc`.
  Before:
  ```rust
  fn reloc(at: u32, kind: RelocKind, target: Target) -> Reloc {
      Reloc { at, kind, target, addend: 0 }
  }
  ```
  After:
  ```rust
  fn reloc(at: u32, kind: RelocKind, target: Target) -> blaze_codegen::Relocation {
      blaze_codegen::Relocation { at, kind, target, addend: 0 }
  }
  ```
  Why: a struct literal of the old four-field `Reloc` is now `Relocation`.

- **Line 80**, `pointer_to`.
  Before:
  ```rust
      Init::Bytes { bytes: vec![0; 8], relocs: vec![reloc(0, RelocKind::Abs64, target)] }
  ```
  After:
  ```rust
      Init::Bytes { bytes: vec![0; 8], relocs: vec![reloc(0, RelocKind::Abs64, target)].into() }
  ```
  Why: `Init::Bytes::relocs` is `Relocs`.

- **Line 336**, the `t2` thread-local.
  Before:
  ```rust
          let t2 = Static { thread_local: true, align: 4, ..value(12, Init::Bytes { bytes: vec![1, 2, 3, 4], relocs: Vec::new() }) };
  ```
  After:
  ```rust
          let t2 = Static { thread_local: true, align: 4, ..value(12, Init::Bytes { bytes: vec![1, 2, 3, 4], relocs: Relocs::new() }) };
  ```
  Why: same field type.

- **Lines 428 and 759** (`small.functions[2].relocs.push(reloc(28, ..))`,
  `more.functions[1].relocs.push(reloc(28, ..))`): no change. `Relocs::push(Relocation)` exists
  and `reloc` now returns a `Relocation`. Both push at 28, after every existing row (0–24), so
  their sorted place is the same as their push place.

Order check: every `vec![reloc(..)]` in `Small::new` (lines 311–329) is already in ascending
`at`, so sorting changes nothing the model linker sees. The order of `Site`s and the extern
cells' order in `model::place` stay the same.

> forwarded to fastcompile (requests, pass 18; told by tmux).
> done (fastcompile-sol-premap): `tests.rs` imports `Relocation as Reloc`, builds `Relocs` with `.into()`, sets `coverage: None`.

### crates/blaze-lazy/src/plan.rs

- **Lines 218–227**, `named_functions`.
  Before:
  ```rust
  fn named_functions(value: &Static) -> impl Iterator<Item = InstanceKey> + '_ {
      let own = match &value.init {
          Init::Bytes { relocs, .. } => relocs.as_slice(),
          Init::Zeroed { .. } => &[],
      };
      own.iter().chain(value.data.iter().flat_map(|it| it.relocs.iter())).filter_map(|it| match it.target {
          Target::Function(key) | Target::Direct(key) => Some(key),
          _ => None,
      })
  }
  ```
  After:
  ```rust
  fn named_functions(value: &Static) -> impl Iterator<Item = InstanceKey> + '_ {
      let own = match &value.init {
          Init::Bytes { relocs, .. } => Some(relocs),
          Init::Zeroed { .. } => None,
      };
      own.into_iter().flatten().chain(value.data.iter().flat_map(|it| it.relocs.iter())).filter_map(|it| match it.target {
          Target::Function(key) | Target::Direct(key) => Some(*key),
          _ => None,
      })
  }
  ```
  Why: `Relocs` has no `as_slice` (see split.rs). `it.target` is now `&Target`, so the match
  binds `key: &InstanceKey`, and the item type `InstanceKey` needs `*key` (`InstanceKey` is
  `Copy`).

- **Lines 175–185**, `Plan::callees`: no change. `for reloc in &function.relocs` yields
  `Relocation<&Target>`. `let (Target::Function(key) | Target::Direct(key)) = &reloc.target`
  binds `key: &InstanceKey` through `&&Target`, which `self.id(key)` takes. `reloc.at` and
  `reloc.kind` are `Copy` values, as `is_call` takes them. Behavior: `out` is now in code-offset
  order rather than codegen's push order. Both backends emit relocations in offset order, and
  `tests/plan.rs`'s expectations hold (see below).

> forwarded to fastcompile (requests, pass 18; told by tmux).

### crates/blaze-lazy/src/tests.rs

- **Line 15**, the import.
  Before:
  ```rust
      Arch, CompiledFunction, InstanceKey, Reloc, RelocKind, Target,
  ```
  After:
  ```rust
      Arch, CompiledFunction, InstanceKey, RelocKind, Target,
  ```
  Why: the old `Reloc` literal is gone. `Relocation` is named by path below because line 17
  imports `settings::Relocation`.

- **Lines 73 and 78**, the pushes in `compiled`.
  Before:
  ```rust
          relocs.push(Reloc { at: code.len() as u32, kind: RelocKind::PcRel32, target: Target::Function(key(callee)), addend: -4 });
  ```
  ```rust
          relocs.push(Reloc { at: code.len() as u32, kind: RelocKind::PcRel32, target: Target::Function(key(other)), addend: -4 });
  ```
  After:
  ```rust
          relocs.push(blaze_codegen::Relocation { at: code.len() as u32, kind: RelocKind::PcRel32, target: Target::Function(key(callee)), addend: -4 });
  ```
  ```rust
          relocs.push(blaze_codegen::Relocation { at: code.len() as u32, kind: RelocKind::PcRel32, target: Target::Function(key(other)), addend: -4 });
  ```
  Why: the struct literal is `Relocation` now. `relocs` (line 70, `Vec::new()`) is inferred
  `Vec<blaze_codegen::Relocation>`.

- **Line 87**, the literal's field.
  Before: `        relocs,`
  After: `        relocs: relocs.into(),`
  Why: `CompiledFunction::relocs` is `Relocs`.

- **Lines 82–93**, same literal: this was already broken before the table change. It is missing `coverage` (E0063).
  Before:
  ```rust
          lines: Vec::new(),
      }
  }
  ```
  After:
  ```rust
          lines: Vec::new(),
          coverage: None,
      }
  }
  ```

> forwarded to fastcompile (requests, pass 18; told by tmux).

### crates/blaze-lazy/src/tests/plan.rs

- **Line 4**, the import.
  Before:
  ```rust
  use blaze_codegen::{Data, Init, InstanceKey, Reloc, RelocKind, Static, Target};
  ```
  After:
  ```rust
  use blaze_codegen::{Data, Init, InstanceKey, RelocKind, Relocation, Static, Target};
  ```
  Why: no `settings::Relocation` in this file, so the row type imports cleanly.

- **Line 15**, the `reloc` closure in `starts`.
  Before:
  ```rust
      let reloc = |n: &u32| Reloc { at: 8 * *n, kind: RelocKind::Abs64, target: Target::Function(key(*n)), addend: 0 };
  ```
  After:
  ```rust
      let reloc = |n: &u32| Relocation { at: 8 * *n, kind: RelocKind::Abs64, target: Target::Function(key(*n)), addend: 0 };
  ```
  Why: the struct literal. Lines 20 and 25 (`names.iter().map(reloc).collect()` into
  `Init::Bytes::relocs` and `Data::relocs`) stay as they are: `Relocs: FromIterator<Relocation>`.
  The `Data { id, .. }` literal at line 21 sets its own test ID, not `data_id`, so nothing there
  depends on the new data domain.

- **Lines 112–113**, the pushes onto `caller.relocs`.
  Before:
  ```rust
      caller.relocs.push(Reloc { at: jump + 1, kind: RelocKind::PcRel32, target: Target::Direct(key(4)), addend: -4 });
      caller.relocs.push(Reloc { at: 0, kind: RelocKind::Abs64, target: Target::Function(key(5)), addend: 0 });
  ```
  After:
  ```rust
      caller.relocs.push(Relocation { at: jump + 1, kind: RelocKind::PcRel32, target: Target::Direct(key(4)), addend: -4 });
      caller.relocs.push(Relocation { at: 0, kind: RelocKind::Abs64, target: Target::Function(key(5)), addend: 0 });
  ```
  Why: `Relocs::push` takes a `Relocation`. Ordering: the `at: 0` row now sorts first instead of
  last. It is `Abs64`, so `is_call` rejects it either way. The calls come out in offset order
  (key 1 at 1, key 2 at 6, key 1 again at 11, the `lea` of key 3, then the `jmp` to key 4 at
  `jump + 1`), so `out == ids(&[1, 2, 4])` still holds.

> forwarded to fastcompile (requests, pass 18; told by tmux).

> applied by fastcompile-sol-lazy, 2026-10-29: the three blaze-lazy sections (`plan.rs`
> `named_functions`, `tests.rs`, `tests/plan.rs`), exactly as written, `coverage: None`
> included. Not built. blaze-premap's sections are not mine.

## Checked, no change needed

- `crates/blaze-premap/src/model.rs`: lines 75–76, 92–93 and 131–134 iterate `&function.relocs`,
  `&blob.relocs` and `relocs` (a `&Relocs` bound by `match &value.init`) and build an owned
  `Site` with `reloc.target.clone()` (a `Target`), `reloc.at`, `reloc.kind` and `reloc.addend`.
  All build as they are. Line 105 is `relocs.is_empty()`. (`Site` resolution per relocation is a
  test-only model linker, so I don't propose the resolve-each-target-once shape here.)
- `crates/blaze-premap/src/prelink.rs`, `image.rs`, `table.rs`: use `Arch`, `CompiledFunction`,
  `InstanceKey`, `Static` and `Target` but no relocation.
- `crates/blaze-lazy/src/mode.rs`, `runner.rs`, `session.rs`, `tests/refuse.rs`,
  `tests/flight.rs`: `plan::{Mode, OptLevel, Policy}`, `CompiledFunction` and `InstanceKey` only.
  Their `CompiledFunction`s come from `tests::compiled`, fixed above.
- `crates/blaze-global/src/public.rs`: `Backend`, `Instance`, `InstanceKey`, `query::keys`,
  `settings`, `summary::Imports` and `driver::stored`. No relocation.
- `crates/blaze-stencil/src/` (`tier.rs`, `table.rs`, `lib.rs`, `arch.rs`, `arch/x64.rs`,
  `arch/aarch64.rs`, `arch/*/tests.rs`, `table/tests.rs`, `patch/tests.rs`, `tests/host.rs`,
  `tests/func.rs`, `tests/fuzz.rs`, `tests/fuzz/program.rs`): only `blaze_codegen::Arch` and
  `blaze_codegen::Backend`. Its relocations are its own `code::Reloc` and Cranelift's
  `binemit::Reloc`, which this change doesn't touch.

> applied: nothing to apply (a list of files checked) (requests, pass 18).
