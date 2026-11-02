# From requests-link-fuzz (child of `requests`)

## 1. `tools/v2/hot_lint.py`: the `crates` field can't be read

What: in `tools/v2/hot_lint.py`, `Input.crates` is `list[str]`; make it
`tuple[str, ...] = HOT`.

Where: `tools/v2/hot_lint.py` line 39 (`crates: list[str] = field(default_factory=lambda: list(HOT))`).

Why: `typed_input._check` reads `tuple[T, ...]` but not `list[T]`, so the input the coordinator
gave every lane, `{"root": ".", "crates": ["blaze-link"]}`, fails with
`TypeError: typed_input: crates has a type it does not read: list[str]`. Only `{"root": "."}`
runs (every hot crate at once).

> forwarded to the coordinator (`tools/`; the same as coordinator.md's hot_lint item), listed in requests' report.

## 2. `tools/v2/hot_lint.py`: code compiled only for tests or fuzzing counts as hot

What: treat an item under `#[cfg(any(test, feature = "fuzz"))]` or `#[cfg(feature = "fuzz")]`
as test code, the way `#[cfg(test)]` is treated. Also treat a file as test code when the
`mod` that declares it carries one of those cfgs, as in `crates/blaze-link/src/macho/tbd.rs`'s
`#[cfg(any(test, feature = "fuzz"))] mod reference;`.

Where: `in_test_module` and `lint_file` in `tools/v2/hot_lint.py`.

Why: the reference models the fuzz targets compare against (link.md item 13) are no normal
build's code. They are the old, allocating readers on purpose. Today they make 77 findings in
`crates/blaze-link/src/macho/tbd/reference.rs` and about 20 in the fuzz-only parts of
`macho/dylib.rs` and `macho/tbd.rs`. Marking each line `// alloc:` would mean about 100 comments
that all say "a test model".

> forwarded to the coordinator (`tools/`), listed in requests' report.

## 3. `tools/v2/hot_lint.py`: braces inside strings end a test module early

What: in `in_test_module`, count `{` and `}` only outside string literals, char literals and
comments.

Where: `in_test_module` in `tools/v2/hot_lint.py` (`opens, closes = line.count("{"), line.count("}")`).

Why: a test module whose strings hold unbalanced braces (YAML and format fixtures) is taken
to end early, and the tests after that point count as hot code.
`crates/blaze-link/src/macho/tbd.rs`'s tests module is taken to end at line 2475 of 2560, so
its `sdk_libsystem` helper makes 5 findings.

> forwarded to the coordinator (`tools/`), listed in requests' report.

## From speed-link-edge-sol-link (2026-10-29): Mach-O writer edits for its speed of light

For the holders of `macho.rs` (`requests-link-fuzz`) and `macho/{sign,linkedit}.rs`
(`requests-link-fuzz-macho-input-next1`). I make each edit myself once every holder is DONE;
until then they are here, exact. Bounds and arithmetic: `docs/v2/plan/lanes/link/speed-of-light.md`,
"Mach-O writer and readers". None changes an output byte except A2, which is opt-in by
deployment target.

> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> forwarded to requests-link-next (A1, A3 in; A2, A4, A5 with requests-link-next-writers; A6, A8 told) (requests, pass 18; told by tmux).

### A1. `macho.rs` `MachO::symbols`: the locals in parallel, and no hash per function

The local symbols (one per named function, 0.2 to 2 million) are written by one serial loop
(`entry` per local: a `section_at` search, the name copied into `strings`, 16 bytes into `nlist`),
and every function's name is first hashed into `exported`, a set that is empty or nearly so
in an executable. At 200k functions of 80-byte names that is about 16 MB copied and 200k hashes
on one core, 3 to 5 ms; the bound on 16 cores is about 0.2 ms. `elf/image.rs` already does this
part in parallel (`.symtab`/`.strtab` by prefix sums).

1. The filter: `let check = !exported.is_empty();` and `.filter(|it| !check || !exported.contains(it.1))`.
2. Replace `for &(addr, name) in &locals { entry(..) }` with prefix sums over chunks of 4,096
   locals: sizes per chunk `Σ spelled(name).0.len() + spelled(name).1.len() + 1` (in parallel,
   `locals.par_chunks(4096)`), then `strings.0.resize(2 + total, 0)` and
   `nlist.0.resize(locals.len() * NLIST, 0)`, split both into one disjoint `&mut` slice per chunk
   (as `elf/image.rs` lines ~424-435 do), and fill each chunk in a rayon task: `n_strx` (its
   running offset from the chunk's start), `N_SECT`, `section_at(addr, name)?`, desc 0, `addr`,
   and the name's two spelled parts then a NUL. The first error in chunk order is kept
   (`dynamic::first_error`'s rule). `defined` and `undefined` stay serial after it (they are the
   exports and imports, a few hundred), appending as now. Same bytes.

> applied by requests (child requests-link-next): checked, as requested (`locals_into` fills the locals in parallel, first error in order); unchanged but for `symbols` now taking the section ranges and recording `locals_of` for 19.5's patch.

### A2. `macho.rs`: `Writer::encode` for chained fixups (`model.rs` has it now; `file.rs` lays the slots)

The core side of `v2/requests/link.md` item 6 is in: `Writer::encode` returns (address, value)
slots that `file.rs` lays over the regions and, on a relink, writes only where they changed
(`changed_slots`). Mach-O does not implement it, so it always writes `LC_DYLD_INFO_ONLY` opcodes,
which is right for rustc's default deployment targets (macOS 11.0 arm64, 10.12 x86-64), where
ld64 and ld-prime write opcodes too. For `self.platform.minimum >= 0x000c_0000` (macOS 12,
iOS 15), ld-prime's default is chained fixups, and they are also the relink bound (a changed
fixup rewrites its 8-byte slot and its predecessor's, where an opcode stream shifts after the
change). The edit, as ld64 writes `DYLD_CHAINED_PTR_64_OFFSET` (format 6):

- `fn encode`: when chained, every rebase (`placed.rebase`, and `Holds::Local` GOT cells) and
  bind (`placed.binds`, `Holds::Import` cells) slot in address order (merge the sorted lists),
  each 8-aligned (else an error naming it). Per 16 KiB page of a segment, chain them: a rebase
  is `target = vmaddr - base` (bits 0..36, an error past 2^36), `high8` 0, `next = (next slot
  - slot) / 4` (bits 51..63, 0 for the page's last), `bind = 0` (bit 63); a bind is `ordinal`
  (bits 0..24, the import's index in the imports table), `addend` (bits 24..32, signed 8-bit;
  past that, the import table's `DYLD_CHAINED_IMPORT_ADDEND` form), `next` as above, `bind = 1`.
  The core already wrote the unslid value into each slot, so `target` is read from
  `placed.bytes` there.
- `fn image`: in `__LINKEDIT` before the trie, the `dyld_chained_fixups_header` (version 0,
  starts, imports and symbols offsets, imports count, `DYLD_CHAINED_IMPORT` = 1 or
  `_ADDEND` = 2, symbols format 0), `dyld_chained_starts_in_image` (one offset per segment, 0
  for one with no slot), a `dyld_chained_starts_in_segment` per segment with slots (page size
  0x4000, pointer format 6, segment offset, page count, each page's first slot offset or
  `DYLD_CHAINED_PTR_START_NONE` 0xffff), the imports (lib ordinal 8 bits, weak 1, name offset
  23) and their names (borrowed from `symbols.strings` as the binds' are now). The load
  commands become `LC_DYLD_CHAINED_FIXUPS` and `LC_DYLD_EXPORTS_TRIE` (16 bytes each) in place of
  `LC_DYLD_INFO_ONLY`; the rebase and bind opcode streams are not written.
- Sizes are known from counts before placement (`Counts::absolute`, `imports`,
  `import_names`), so nothing is laid out twice. Tests: an arm64 executable at minimum 12.0 read
  back with `object`'s chained-fixup reader, every slot decoded to the address or import the
  opcode form gives at 11.0; and a relink that adds one bind rewrites two slots.

> applied by requests (child requests-link-next): `macho/chained.rs` + `MachO::encode`: `DYLD_CHAINED_PTR_64_OFFSET` slots, the `LC_DYLD_CHAINED_FIXUPS` table (IMPORT, or IMPORT_ADDEND past a signed byte) and `LC_DYLD_EXPORTS_TRIE` in place of `LC_DYLD_INFO_ONLY`; the signature hashes the slots. Differs: chosen per platform as ld-prime does (macOS 12, iOS/tvOS/Catalyst 15, watchOS 8, DriverKit 21, visionOS) rather than one `minimum >= 0x000c_0000`; GOT `Holds::Local` cells are already in `placed.rebase`, so not added again; the workspace's `object` 0.37 has no chained-fixup reader, so `chained_fixups_slide_and_bind_what_the_opcodes_do` decodes the chains by hand against the opcodes at 11.0; `a_bind_added_rewrites_two_slots`. Default target 11.0: no default bytes change. Not built.

### A3. `macho.rs` `image`: the signature as one part (after `link.md` item 19.1)

`parts.push((at, piece.to_vec()))` per 4 KiB of signature (one `Vec` per page, 25,000 for a
100 MB image) exists so a relink rewrites only the changed hashes. Once `file.rs` diffs parts by
block (`v2/requests/link.md` item 19.1), push the signature once:
`parts.push((code_limit, signature))`, with the same writes on a relink and no allocation per page.

> applied by requests (child requests-link-next): checked, as requested: the signature is one part at `code_limit`.

### A4. `macho/sign.rs` `hash_cached`: hash what the core wrote, not what compares unequal (after `link.md` item 19.5)

Today a relink compares every page with the cached copy (100 MB read twice, about 0.5 ms on 16
cores, and the cache holds a second copy of the image) to find the few that changed. With
`Placed::delta`'s `written` ranges, a page is hashed again when it overlaps a written range, or
lies in the headers or `__LINKEDIT` and compares unequal; every other page keeps its hash without
being read. `Cache::pages` then holds only the headers' and `__LINKEDIT`'s pages. A relink's
signature costs the pages it changed (2 µs each at 2 GB/s) plus `__LINKEDIT`'s compare.

Budget it must keep (`macho/tests.rs` `signing_allocates_nothing_per_page`, on a 1-thread rayon
pool): an uncached signature is 1 allocation of exactly `sign::len` bytes (the output); a warm
cached re-sign into a buffer with room is 0 allocations and the same bytes. With `Cache::pages`
holding only the headers' and `__LINKEDIT`'s pages, the warm path still allocates nothing: the
store is sized on the first sign and reused, never grown on a re-sign of the same layout.

> requests-link-fuzz-macho-input (2026-10-29 20:23): DONE; `macho/sign.rs` is
> speed-link-edge-sol-link's for A4 once `Placed::delta` (`link.md` 19.5) exists. It doesn't yet.
> applied by requests (child requests-link-next): `sign::Cache` keeps every hash and only the headers' and `__LINKEDIT`'s pages; a page is rehashed when it overlaps `Since::written` (the delta's `written` in file offsets, plus the pages of chained slots that changed and of debug sections that aren't the same buffers) or is kept and differs, only for `Since::from` equal to the cache's synthesis. `signing_allocates_nothing_per_page` keeps its budget with a delta; `the_signing_cache_writes_the_same_file` relinks with deltas at macOS 11 and 12 against an uncached writer. Not built.

### A5. `macho.rs` `image`: the debug info not copied (after `link.md` item 19.2)

`dwarf.extend_from_slice(bytes)` copies every debug section into one `Vec` per link (the whole
`__DWARF` segment). With `File::debug` the writer pushes `(offset, nth)` per section and the
signature's `View` takes `(offset, &placed.debug[nth].1[..])` in place of `&dwarf[..]`.

> Told by tmux: requests-link-fuzz, requests-link-fuzz-macho-input-next1.

> requests-link-fuzz (2026-10-29 20:15): `macho.rs` released to speed-link-edge-sol-link for A1 to A5.
> The file must stay at 0 hot_lint findings outside the fuzz-gated `core_name`, and the existing
> `// alloc:` and `// clone:` lines must stay true. core-model-next1 may change `Class::Named`
> constructions in it at the same time. `macho/{sign,linkedit}.rs`: macho-input is DONE.

> speed-link-edge-sol-link (2026-10-29): A1 made. `macho.rs` `locals_into` fills the locals in
> parallel (same bytes, first error in order), called from `MachO::symbols` before the serial
> `entry`; tested by `macho/tests.rs` `the_locals_in_parallel_are_the_locals_one_by_one` against
> the old loop. Its second half (skip the `exported` probe) is not needed: hashbrown's `contains`
> returns before hashing on an empty table. hot_lint: `macho.rs` at 0 outside `core_name`.
> Nothing compiled. A2 is left for a lane that can build and run `object`'s chained-fixup reader;
> A3 to A5 wait on `link.md` 19.1, 19.2, 19.5.

> speed-link-edge-sol-link (2026-10-29 20:25): A3 made, after core-stage landed 19.1
> (`file.rs` `changed_blocks`). `MachO::image` pushes the signature as one part at
> `code_limit`; the per-page `to_vec` and its `// alloc:` line are gone. Same bytes (the image
> tests assemble parts generically), and a relink writes the same 4 KiB blocks, cut from the
> part's start as the pieces were. hot_lint: `macho.rs` at 0 outside `core_name`. A4 and A5
> wait on core-model-next1's `Placed::delta` and `File::debug`.
> applied by requests (child requests-link-next): `image` pushes `(offset, nth)` per non-empty debug section to `File::debug` (same offsets) and the signature's `View` borrows `placed.debug`; no `dwarf` buffer. Test `macho/tests.rs` `the_debug_sections_are_the_cores_not_copies`. Not built.

### A6. `macho/tbd.rs`: the documents in parallel

`libSystem.tbd` is about 1 MB in about 40 YAML documents (`--- !tapi-tbd` to `...`), parsed one
after another, about 0.3 ms of one core. Find the document boundaries with one `memchr` pass
for `\n--- ` and `\n...`, parse each document in a rayon task into its own `Names` (reserved by
the document's length), then join them in document order with `Names::suffix` offsets, so the
symbols and their order are the old ones. The bound is 0.3 ms ÷ 16 ≈ 0.02 ms. Test: every
existing `.tbd` test and the fuzz differential against `tbd/reference.rs` on the joined result.

### A8. `macho/unwind.rs` `fdes`: the split parallel scan, shared with ELF

`elf/dynamic.rs` now walks `.eh_frame` in pieces split at every 4,096th `Function::fde`, in
parallel, with a hand decoder tested against gimli (`fdes`, `walk`, `Cursor`). `macho/unwind.rs`
`fdes` is the same serial walk. Move `fdes`, `walk`, `entry`, `cie_at`, `cie_encoding`,
`pointer_encoding` and `Cursor` into the core's `unwind.rs` as one function both writers call
(a request to core-stage, which owns `unwind.rs`), then build `__unwind_info`'s second-level
pages in parallel, one task a page of 1,021 regular or 1,000 compressed entries, joined in page
order. The bound is about 0.06 ms for 200,000 functions.

> requests-link-fuzz (20:30): A1 reviewed and accepted (same bytes; step 1's empty-set skip not
> yet in; `n_strx` as `u32` truncates past 4 GiB as before). A2 to A5 stay with
> speed-link-edge-sol-link, which holds `macho.rs`. A6: `macho/tbd.rs` released to it; every
> `.tbd` test and the differential against `tbd/reference.rs` must still pass, and
> `tbd/reference.rs` is not to be touched. A8: both holders are DONE, so it makes A8 itself.

> speed-sol2-link-writers (2026-10-30): A8 made, not yet built. The walk is `unwind.rs` `scan`
> (with `entry`, `frame` and `rewalk`), and both `elf/dynamic.rs` and `macho/unwind.rs` `fdes`
> call it. The `__unwind_info` merge and its regular pages are parallel; compressed pages stay
> serial (O(native entries)). See "`__unwind_info` assembly" in
> `docs/v2/plan/lanes/link/speed-of-light.md`.

## dedup: `macho/dylib.rs` LEB128 and RNG onto `blaze_format::leb` and `blaze_ids::rng`

From `dedup-codec-leb` (report `/tmp/blaze-lanes/v2-dedup-codec-leb.md`); the rest of
`blaze-link` went to `v2/requests/mem-backend-link.md` under the same heading, which describes
the shared API. In `crates/blaze-link/src/macho/dylib.rs`:

- `fn uleb` becomes a wrapper keeping its messages (`malformed_tries` checks "too long" and
  "runs past the end"):

  ```rust
  fn uleb(data: &[u8], at: &mut usize) -> Result<u64, String> {
      let start = *at;
      // alloc: error path
      blaze_format::leb::read_u64(data, at).map_err(|error| match error {
          LebError::Truncated => format!("a ULEB128 at {start:#x} runs past the end"),
          LebError::Overflow => format!("a ULEB128 at {start:#x} is too long"),
      })
  }
  ```

  with `use blaze_format::leb::LebError;` among the imports. (An 11-byte `0x80...` run and a
  10th byte past bit 63 are both `Overflow`, as now.)
- In `mod tests`: delete `fn encode_uleb` and `fn uleb_len`; their calls become
  `blaze_format::leb::write_u64(` and `blaze_format::leb::len_u64(`. `uleb_limits` stays
  (it tests the wrapper).
- In `mod tests`: `Rng` is SplitMix64, same stream: `struct Rng(blaze_ids::rng::SplitMix64);`,
  `next` becomes `self.0.next_u64()`, `below` becomes `self.0.index(n)`, and each `Rng(x)`
  becomes `Rng(blaze_ids::rng::SplitMix64::new(x))`. Outputs are bit for bit the same.

> requests-link-fuzz (20:30): applied to `macho/dylib.rs`. `uleb` wraps `blaze_format::leb::read_u64`
> with its two messages, and the tests use `write_u64`/`len_u64` and `SplitMix64`. Checked
> against the old reader: it accepts the same bytes, including padded ULEB128s. The one
> difference is an input that ends right after a 10th byte with the continuation bit set: the old
> reader said "runs past the end", the new one says "too long". Both are errors, and neither test
> sees it. The RNG gives the same stream for every `n > 0`, which is all the tests use.
> taken by requests (link and requests-link-fuzz are DONE): child `requests-link-next`; the outcome follows.
> applied by requests (child requests-link-next): checked; requests-link-fuzz's edit above is in the tree as written. `trie_exports`' `seen` is now a `BitSet` (`link.md` dedup bits 4).

