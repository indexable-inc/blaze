---
status: plan
summary: A dev build's executable carries no debug info - it is a side file every debugger and std's backtrace find unaided (ELF `<name>.debug` by `.gnu_debuglink` with a CRC that is a function of the build id, Mach-O `<name>.dSYM` by `LC_UUID`), kept as per-function records patched in place; one program is one link however many paths it is placed at. Built - the side file, its model and writer, the profile default, one link per unhashed program, a build id that is not of the directory. Not built - the write moved after the answer, the side file as a store output, the writers' tables by entry, the maps as records. The 162 MB is mostly not debug info - at least 128 MiB is the two patch regions' room.
date: 2026-11-10
---
# Debug info beside the executable

Hello world's dev executable is 162,021,720 bytes, written, hashed and placed every edit, and
linked twice. Follows [item-hash.md](item-hash.md) section 13, [patch-in-place.md](patch-in-place.md).

## What the 162 MB is (from the code; the split needs measuring)

The debug info the linker makes is small: one DWARF 4 unit of line rows and one subprogram a
function (`crates/blaze-link/src/dwarf.rs`), no types, no variables. For 14,000 functions:
`.debug_info` 15 bytes a function (0.2 MB), `.debug_str` the mangled names (1 to 3 MB; `.strtab`
holds them again), `.debug_line` 14 bytes a function plus 1 to 4 a row. A few megabytes.

The file's length is the last file-backed region's end, room included (`elf/image.rs` `tail`).
An incremental image keeps two patch regions, `Class::Patch` and `Class::PatchData`, each at
least 64 MiB (`place.rs` `patch_room`), both in the file (`model.rs` `zeroed` is only `.bss`,
`.tbss` and a dev run's reservations). So **at least 134,217,728 of the 162,021,720 bytes
(82.8%) are the patch regions' room**, zeros until an edit lands there; the other 27.8 MB is
code, data, unwind tables, each growing region's doubled room, the symbol table and the debug
info. Taking the debug info out removes those few megabytes and no more.

To measure: `readelf -SW` / `otool -l` section sizes, `du` against `ls -l`, the side file's size.
The room is the next 134 MB to take (`DevRun`'s "all in the file" rule, the store's digest).

## Where debug info goes

`blaze_link::Debug::Beside`: the image carries no `.debug_*`; the side file is
`Image::debug_file()`'s. `codegen::debug_place` chooses: debug info with `split-debuginfo`
unset, `packed` or `unpacked` goes beside; `"off"` is `Debug::Lines`, the layout as before.

| Platform | Executable keeps | Side file | Found by |
|---|---|---|---|
| ELF | code, data, `.eh_frame`, `.symtab`, `.note.gnu.build-id`, a `.gnu_debuglink` (name, CRC-32) | `<name>.debug` beside it: the executable's sections with no bytes, its build id note, DWARF, a copy of `.symtab`/`.strtab` | `.gnu_debuglink` in the executable's directory |
| Mach-O | the same, `LC_UUID` | `<name>.dSYM/Contents/Resources/DWARF/<name>`: `LC_UUID`, the segments with no bytes, `__DWARF`, the symbol table | the bundle beside it, by UUID |
| PE, wasm | as before (wasm: the name section) | none: blaze writes no PDB | nothing changed |

How sure, with nothing configured (none of it run here; `harness/v2/debug_side.py` checks):

- **std's backtrace, Linux**: sure. `backtrace-rs` `gimli/elf.rs` tries the build id under
  `/usr/lib/debug/.build-id` (absent), then `.gnu_debuglink` in the executable's directory,
  with no CRC check, and then takes symbols from the side file, so it copies `.symtab`.
- **std's backtrace, macOS**: sure. `gimli/macho.rs` scans the directory for a `*.dSYM` of its UUID.
- **gdb**: likely. It finds `<dir>/<debuglink name>` and checks the CRC. Unsure: a side file
  with no program headers (objcopy keeps them), and the section mapping for a PIE.
- **lldb**: likely. Linux by `.gnu_debuglink` and build id; macOS `<exe>.dSYM` beside the
  executable by UUID (no `Info.plist`). No `.build-id/` directory: that needs a configured root.

## The CRC is of the build id

`.gnu_debuglink` holds the side file's CRC-32, and gdb refuses a file whose CRC differs. A CRC
of the side file's bytes would make the executable wait for its debug info. So the executable
holds `link_crc(id)`, the build id's first four bytes, and the side file's last four bytes are
chosen to make its CRC that (`beside::crc_fix`): a side file of another generation fails gdb's check.

## The side file is records (`crates/blaze-link/src/beside.rs`)

- `.debug_info`: the unit, then one 17-byte subprogram a function at a fixed slot (name offset,
  address, size). A slot whose function is gone is an empty range and is reused.
- `.debug_line`: one program; a function's rows are its own sequence, in any order. A changed
  function's sequence is appended and the old one overwritten with `DW_LNS_advance_pc 0`
  (`dwarf::dead`: no row, no register moved; gimli refuses a sequence whose address goes back).
- `.debug_str`: names, appended. The symbol table and its names: copied, by 4 KiB block.
- Every section has room (twice its size). Laid out whole, saying why (`DebugWrite::whole`):
  the first write, no room, a source file the line header does not name, more than half of
  `.debug_line` dead, a file on the disk that is not the last one written.

`Kept::update` holds a hash a function (address, size, name, encoded rows) and writes the
records whose hash changed: a body edit two (the function made and the one replaced), a line
edit above the functions of that file after it. It compares every function each update: CPU of
the program's rows (not yet by the stage's delta), bytes written of the edit. The build id is
the executable's, read from its file; a patch names the id of the file it is over
(`DebugWrite::base`) and `apply` refuses any other. A rollback needs nothing said.

**Between the answer and the write** a debugger sees the previous side file: on ELF gdb refuses
it by CRC and std reads it (the edited function's lines stale until the write); on macOS both
refuse it by UUID and use symbols. It is replaced by an unsynced write of a clone and a rename.

## One link for one program

Two images existed because `artifact::made` listed both `build/<pkg>/<hash>/out/<name>` and
`<profile>/<name>`: `rustc_writes` took every output of a unit with no `extra_filename` (cargo's
new layout) as rustc's own, so `uplifted` found no copy. Now an output outside the unit's output
directory with a same-named output inside is its copy (`copy_of_unhashed`): one made file, one
kept image, one link; the profile directory's is a placement of the same key (`compile.rs`
finds it by `uplifted`), the dev run held under both by `runs_from`. The kept image is still
found by its made path: it is what a link patches; what it holds is `Rows::given`'s to check.

The build id was of the path: `lineage` hashed `to`. Only the file name is in the file (a Mach-O
signature's identifier, an import library's DLL, the `.gnu_debuglink`); the directory is not
(`DW_AT_comp_dir` is `.`; rpaths are the output's own field, hashed). `lineage` now hashes the
name alone, so the same program for two directories is the same bytes.

## Not built, and why

- **The write after the answer.** `codegen::debug_side` runs in the link, in the span
  `link debug file` (`bytes written ranges records whole`): the settling path is `job.rs`'s.
  It reads only the committed image, so the job calls `Image::debug_file()` on the kept image
  after the answer and writes as `write_side` does.
- **The side file in the store.** It is written beside the executable and its copy, not put: a
  build that places the executable from the store without linking writes none, and backtraces
  name functions without lines until the next link. It should be a unit output (`compile.rs`).
- **The writers' tables by entry** (`file.rs`: each synthetic region made again a stage and
  compared in 4 KiB blocks): unchanged. What is left to compare is `.symtab`, `.strtab` (ELF
  rewrites only the spliced functions' entries), `.eh_frame_hdr` and the hash tables; whether
  that costs needs `link patch`'s span split by region.
- **Address and perf maps as records** (`layout.rs`, `perf.rs`; patch-in-place.md). **PDB**.

## Tests

- `crates/blaze-link/src/beside/tests.rs`: the refinement
  `a_side_file_kept_by_patches_resolves_as_debug_info_inside` (random edit sequences against
  `dwarf::sections` of the same program, read with gimli; build id, symbols, CRC); controls
  `the_oracle_catches_a_side_file_of_another_build_id`,
  `the_oracle_catches_a_line_row_left_for_a_moved_function`,
  `the_oracle_catches_a_debuglink_crc_not_updated`; counter
  `a_one_function_edit_rewrites_its_own_records` (100 and 10,000 functions); and the whole file
  twice, a patch of another file refused, compaction, dead rows, the CRC, Mach-O, no build id.
- `crates/blaze-link/src/tests/beside.rs`: the refinement at the image
  `an_image_and_its_side_file_resolve_as_debug_info_inside` (first link, edit, rollback, a side
  file gone); counter `the_executable_has_no_section_that_grows_with_debug_info`;
  `debug_info_inside_is_the_same_file_twice`;
  `an_images_build_id_is_of_the_program_not_of_its_directory`.
- `crates/blaze-cargo`: `debug_info_goes_beside_the_image_unless_the_profile_says_off`,
  `a_program_with_no_hash_in_its_name_is_made_once`. `harness/v2/debug_side.py`: hello world
  both ways: sizes, build id and CRC, the same backtrace, gdb and lldb or "not checked: no gdb".
