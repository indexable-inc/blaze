# `blaze_codegen::Relocs` in `crates/blaze-link`

Every file of `crates/blaze-link` that names `blaze_codegen` (39 files, `rg -l blaze_codegen crates/blaze-link`)
was read at its relocation sites. Only four files touch codegen's relocations: `src/stage.rs` (the
one real consumer), `src/tests.rs`, `src/intern.rs` (tests) and, through `tests.rs`'s helpers,
`src/tests/ties.rs` (no edit). `src/write.rs`, `src/pe.rs`, `src/wasm/object.rs` have their own
`Reloc`/`Segment::Reloc` types, and `macho/input.rs`, `elf/tests.rs`, `tests/ties.rs` etc. use
`object`'s `Relocation`; none of those are affected.

`stage.rs` is changed to the better shape the brief proposes: each table's `targets()` is interned
once (one `Wide` key per distinct target, not per relocation) and each fixup indexes the symbols by
`row.target`. It's the same lines the minimum change would touch. It does the same work: every
target is still interned, weak symbols are still collected from the targets and deduplicated, and
each relocation still becomes one fixup with the same `at`, `apply`, symbol and whole addend. Two
things change. Fewer keys are interned when a target repeats. Fixups now come in offset order
instead of codegen's push order. That's safe: nothing in the crate reads a codegen piece's fixups by
position, and `write::split` (`src/write.rs:378`) only splits a piece's fixups across jobs when they
are in offset order, so sorted fixups make more pieces splittable. The bytes written are the same.

No test in `crates/blaze-link` hard-codes a content ID, a store key or the template blob magic
(`rg 'blaze v2 (data|code|template)|BZT|BZS|code_id|data_id' crates/blaze-link`: only
`position.rs`'s own `BZSPANS1`, unrelated).

### crates/blaze-link/src/stage.rs

1. Line 18. Before:
   ```rust
   use blaze_codegen::{Arch, CompiledFunction, Data, Init, InstanceKey, Reloc, Static, Target, Unwind};
   ```
   After:
   ```rust
   use blaze_codegen::{Arch, CompiledFunction, Data, Init, InstanceKey, Relocs, Static, Target, Unwind};
   ```
   Why: `Reloc` is no longer named here; the functions below take `&Relocs`.

2. After line 35 (`const CIE_DOMAIN: &str = "blaze v2 link cie 1";`), add:
   ```rust

   /// The relocations of a zeroed static.
   static NO_RELOCS: Relocs = Relocs::new();
   ```
   Why: `add_static` (item 7) needs a `&'a Relocs` for `Init::Zeroed`, where it used `&[][..]`.
   `Relocs::new` is a `const fn` and `Relocs<Target>` is `Sync`.

3. Lines 183–205. Before:
   ```rust
   /// The keys of a function's symbols, in the order [`Stage::add_function`] reads them: its
   /// body, its relocations' targets, then each datum and its relocations' targets.
   fn function_keys<'f>(function: &'f CompiledFunction, table: bool, out: &mut [Wide<'f>]) {
       let mut at = 0;
       let mut put = |key: Wide<'f>| {
           out[at] = key;
           at += 1;
       };
       put(Wide::Body(&function.key));
       for reloc in &function.relocs {
           put(wide(&reloc.target, table).0);
       }
       for data in &function.data {
           put(Wide::Data(&data.id));
           for reloc in &data.relocs {
               put(wide(&reloc.target, table).0);
           }
       }
   }

   fn key_count(function: &CompiledFunction) -> usize {
       1 + function.relocs.len() + function.data.iter().map(|it| 1 + it.relocs.len()).sum::<usize>()
   }
   ```
   After:
   ```rust
   /// The keys of a function's symbols, in the order [`Stage::add_function`] reads them: its
   /// body, its relocations' targets (each once, by index), then each datum and its
   /// relocations' targets.
   fn function_keys<'f>(function: &'f CompiledFunction, table: bool, out: &mut [Wide<'f>]) {
       let mut at = 0;
       let mut put = |key: Wide<'f>| {
           out[at] = key;
           at += 1;
       };
       put(Wide::Body(&function.key));
       for target in function.relocs.targets() {
           put(wide(target, table).0);
       }
       for data in &function.data {
           put(Wide::Data(&data.id));
           for target in data.relocs.targets() {
               put(wide(target, table).0);
           }
       }
   }

   fn key_count(function: &CompiledFunction) -> usize {
       1 + function.relocs.targets().len()
           + function.data.iter().map(|it| 1 + it.relocs.targets().len()).sum::<usize>()
   }
   ```
   Why: one key per distinct target, matching `fixups`' indexing by `row.target`. `targets()`
   returns `&'f [Target]`, so `Wide<'f>` borrows from the function as before.

4. Lines 229–236. Before:
   ```rust
       /// The symbols of relocations' targets, in the stage's arena.
       fn reloc_syms(&mut self, relocs: &[Reloc]) -> &'a [Sym] {
           let mut out = Vec::with_capacity_in(relocs.len(), self.arena);
           for reloc in relocs {
               out.push(self.target_sym(&reloc.target));
           }
           out.leak()
       }
   ```
   After:
   ```rust
       /// The symbols of a table's targets, by target index, in the stage's arena.
       fn reloc_syms(&mut self, relocs: &Relocs) -> &'a [Sym] {
           let mut out = Vec::with_capacity_in(relocs.targets().len(), self.arena);
           for target in relocs.targets() {
               out.push(self.target_sym(target));
           }
           out.leak()
       }
   ```
   Why: `&[Reloc]` is gone. This resolves each distinct target once, in the layout `fixups`
   indexes. The callers at lines 782 (`self.reloc_syms(&data.relocs)`) and 789
   (`self.reloc_syms(relocs)`) are unchanged in text: `&data.relocs` is now `&Relocs`, and item 7
   makes `relocs` a `&'a Relocs`.

5. Lines 568–587. Before:
   ```rust
       fn fixups(
           &self,
           relocs: &[Reloc],
           syms: &[Sym],
           function: Option<InstanceKey>,
           in_code: bool,
       ) -> Result<(&'a [Fixup], &'a [Sym]), LinkError> {
           let mut fixups = Vec::with_capacity_in(relocs.len(), self.arena);
           let mut weak = Vec::new_in(self.arena);
           for (reloc, sym) in relocs.iter().zip(syms) {
               let apply = reloc::of_kind(reloc.kind, self.output.format, self.arch(), in_code).map_err(|it| error(function, it))?;
               if matches!(reloc.target, Target::Weak(_)) {
                   weak.push(*sym);
               }
               fixups.push(Fixup { at: u64::from(reloc.at), apply, target: *sym, addend: reloc.addend, minus: None });
           }
           weak.sort_unstable();
           weak.dedup();
           Ok((fixups.leak(), weak.leak()))
       }
   ```
   After:
   ```rust
       /// `relocs` as fixups, `syms` being the symbols of its targets by index
       /// ([`Stage::reloc_syms`]), and the weak ones among them.
       fn fixups(
           &self,
           relocs: &Relocs,
           syms: &[Sym],
           function: Option<InstanceKey>,
           in_code: bool,
       ) -> Result<(&'a [Fixup], &'a [Sym]), LinkError> {
           let mut fixups = Vec::with_capacity_in(relocs.len(), self.arena);
           let mut weak = Vec::new_in(self.arena);
           for (target, sym) in relocs.targets().iter().zip(syms) {
               if matches!(target, Target::Weak(_)) {
                   weak.push(*sym);
               }
           }
           for (row, reloc) in relocs.rows().iter().zip(relocs) {
               let apply = reloc::of_kind(reloc.kind, self.output.format, self.arch(), in_code).map_err(|it| error(function, it))?;
               let target = syms[row.target as usize];
               fixups.push(Fixup { at: u64::from(reloc.at), apply, target, addend: reloc.addend, minus: None });
           }
           weak.sort_unstable();
           weak.dedup();
           Ok((fixups.leak(), weak.leak()))
       }
   ```
   Why: `&[Reloc]` is gone. Each target is checked for weakness once, and each row takes its
   symbol by `row.target`. `relocs.rows()` and `relocs.iter()` walk the same rows in the same
   order, so `reloc` is `row` with its whole addend (narrow or from `wide()`). `sort_unstable` and
   `dedup` stay because two distinct targets can intern to one symbol (`Function(k)` and
   `Direct(k)` without a table).

6. Lines 592, 599, 602–603 (`add_function`). Before:
   ```rust
           let (fixups, weak) = self.fixups(&function.relocs, &syms[1..=function.relocs.len()], key, true)?;
   ```
   ```rust
           let mut at = 1 + function.relocs.len();
           for data in &function.data {
               let sym = syms[at];
               let relocs = &syms[at + 1..at + 1 + data.relocs.len()];
               at += 1 + data.relocs.len();
   ```
   After:
   ```rust
           let (fixups, weak) = self.fixups(&function.relocs, &syms[1..=function.relocs.targets().len()], key, true)?;
   ```
   ```rust
           let mut at = 1 + function.relocs.targets().len();
           for data in &function.data {
               let sym = syms[at];
               let relocs = &syms[at + 1..at + 1 + data.relocs.targets().len()];
               at += 1 + data.relocs.targets().len();
   ```
   Why: `syms` now holds one symbol per distinct target (item 3), so the slices are measured by
   `targets().len()`, not by the number of relocations. `add_data`'s call at line 763,
   `self.fixups(&data.relocs, relocs, function, false)`, is unchanged in text: `&data.relocs` is
   `&Relocs`, and `relocs` is that datum's per-target slice.

7. Lines 785–787 (`add_static`). Before:
   ```rust
           let (bytes, relocs, size, zero): (&'a [u8], &'a [Reloc], u64, bool) = match &it.init {
               Init::Bytes { bytes, relocs } => (&bytes[..], &relocs[..], bytes.len() as u64, false),
               Init::Zeroed { size } => (&[][..], &[][..], *size, true),
           };
   ```
   After:
   ```rust
           let (bytes, relocs, size, zero): (&'a [u8], &'a Relocs, u64, bool) = match &it.init {
               Init::Bytes { bytes, relocs } => (&bytes[..], relocs, bytes.len() as u64, false),
               Init::Zeroed { size } => (&[][..], &NO_RELOCS, *size, true),
           };
   ```
   Why: `Init::Bytes { relocs }` is `Relocs` and can't be sliced. Lines 789–790
   (`self.reloc_syms(relocs)`, `self.fixups(relocs, syms, None, false)`) then build unchanged.

> forwarded to requests-link-next (holds `crates/blaze-link/`; link is DONE) (requests, pass 18; told by tmux).
> applied by requests (child requests-link-next): see codegen.md "To link: crates/blaze-link onto Relocs".

### crates/blaze-link/src/tests.rs

1. Line 7. Before:
   ```rust
   use blaze_codegen::{Arch, CompiledFunction, Init, InstanceKey, Line, Reloc, RelocKind, Static, Target};
   ```
   After:
   ```rust
   use blaze_codegen::{Arch, CompiledFunction, Init, InstanceKey, Line, RelocKind, Relocs, Static, Target};
   ```
   Why: the helpers build `blaze_codegen::Relocation`. It is written by its full path below because
   line 14 already imports `object::write::Relocation`, and the two names would clash.

2. Lines 57 and 63, plus a field after line 68 (`function`). Before:
   ```rust
   fn function(n: u8, code: &[u8], relocs: Vec<Reloc>) -> CompiledFunction {
   ```
   ```rust
           relocs,
   ```
   ```rust
           lines: Vec::new(),
       }
   ```
   After:
   ```rust
   fn function(n: u8, code: &[u8], relocs: Vec<blaze_codegen::Relocation>) -> CompiledFunction {
   ```
   ```rust
           relocs: relocs.into(),
   ```
   ```rust
           lines: Vec::new(),
           coverage: None,
       }
   ```
   Why: `CompiledFunction::relocs` is `Relocs`, built from a `Vec<Relocation>` with `From`. Every
   caller keeps compiling as written: `vec![rel32(..)]`, `Vec::new()` (lines 87, 371, 404, 426,
   455, 464, 476, 526–530, 558, 578–585, 606, 655–656, 687, 741, 1051–1052, 1080, 1089, 1099, 1113,
   1135, 1155), `calls`' pushed `Vec` (lines 723–729) and `tests/ties.rs`'s `start` (lines
   121–131). `coverage: None` is not part of this change: the field is already in
   `CompiledFunction` at HEAD and this literal lacks it, so the test crate doesn't build without it.

3. Lines 73–74 (`rel32`). Before:
   ```rust
   fn rel32(at: u32, target: Target) -> Reloc {
       Reloc { at, kind: RelocKind::PcRel32, target, addend: -4 }
   }
   ```
   After:
   ```rust
   fn rel32(at: u32, target: Target) -> blaze_codegen::Relocation {
       blaze_codegen::Relocation { at, kind: RelocKind::PcRel32, target, addend: -4 }
   }
   ```
   Why: the old four-field struct is now `Relocation`. `Reloc` is the stored row, built only by
   `Reloc::new`.

4. Line 569. Before:
   ```rust
           init: Init::Bytes { bytes: vec![value; 8], relocs: Vec::new() },
   ```
   After:
   ```rust
           init: Init::Bytes { bytes: vec![value; 8], relocs: Relocs::new() },
   ```
   Why: `Init::Bytes { relocs }` is `Relocs`.

No test here reads a relocation by position (`relocs[i]`, "the last pushed"). The tests read
relocated fields by code offset (`linked.rel32(s + 3)`), so sorting by `at` changes none of them.

> forwarded to requests-link-next (requests, pass 18; told by tmux).
> applied by requests (child requests-link-next): see codegen.md "To link: crates/blaze-link onto Relocs".

### crates/blaze-link/src/intern.rs

1. Line 351 (test module). Before:
   ```rust
       use blaze_codegen::{Arch, CompiledFunction, InstanceKey, Target};
   ```
   After:
   ```rust
       use blaze_codegen::{Arch, CompiledFunction, InstanceKey, Relocs, Target};
   ```
   Why: for `Relocs::new()` below.

2. Line 499, plus a field after line 504 (`function`). Before:
   ```rust
               relocs: Vec::new(),
   ```
   ```rust
               lines: Vec::new(),
           }
   ```
   After:
   ```rust
               relocs: Relocs::new(),
   ```
   ```rust
               lines: Vec::new(),
               coverage: None,
           }
   ```
   Why: `CompiledFunction::relocs` is `Relocs`. As in `tests.rs`, `coverage: None` fills a field that
   was already missing at HEAD.

> forwarded to requests-link-next (requests, pass 18; told by tmux).
> applied by requests (child requests-link-next): see codegen.md "To link: crates/blaze-link onto Relocs".

### Build break found here, not part of this change

- `crates/blaze-link/src/stage.rs` lines 171–181: `wide`'s `match target` has no arm for
  `Target::Coverage(u32)`, which is already in `blaze_codegen::Target` at HEAD. So the crate fails
  with E0004 whatever happens to relocations. The linker has no support for a codegen function's
  own SanitizerCoverage arrays (`CompiledFunction::coverage`): there is no `Wide`/`SymKey` for them
  and no pieces are made. This is a missing feature for the link lane (or `codegen-v2`) to design,
  not something to guess an arm for. Under no-fallback it must not be mapped to some other symbol.

> forwarded to requests-link-next (its child requests-link-next-core has codegen.md Z1/Z2 item 4, coverage, which is this arm) (requests, pass 18; told by tmux).
> applied by requests (child requests-link-next): see codegen.md "To link: crates/blaze-link onto Relocs".

### Files checked that need no change

- `src/tests/ties.rs`: builds `Vec<Relocation>` through `tests.rs`'s `rel32` and `function` (lines
  121–131). It imports `object::write::Relocation` only for its own object builder.
- `src/write.rs`: `Reloc<'s>` (line 401) is its own relocation context, not codegen's.
- `src/pe.rs`, `src/pe/image.rs`: `Segment::Reloc` is the `.reloc` section.
- `src/wasm/object.rs`: its own `struct Reloc` (line 541) for wasm object relocations.
- `src/native.rs`, `src/state.rs`, `src/live.rs`, `src/symbols.rs`, `src/model.rs`, `src/reloc.rs`,
  `src/position.rs`, `src/table.rs`, `src/unwind.rs`, `src/output.rs`, `src/lib.rs`, `src/elf.rs`,
  `src/elf/*.rs`, `src/elf/read/*.rs`, `src/macho.rs`, `src/macho/*.rs`, `src/macho/tbd/*.rs`,
  `src/wasm.rs`, `src/wasm/{code,module,reloc}.rs`, `src/native/ties_tests.rs`, `src/pe/tests.rs`:
  they use `Arch`, `Target` (names, unwind personality and handler, not relocations), `RelocKind`,
  `InstanceKey`, `CompiledFunction` fields other than `relocs`, or `object`'s own relocation
  types. None reads `.relocs`, `Data::new`, `Init::Bytes { relocs }` or `blaze_codegen::Reloc`.

> applied: nothing to apply (requests, pass 18).
