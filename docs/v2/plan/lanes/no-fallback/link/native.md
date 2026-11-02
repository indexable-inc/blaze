---
status: plan
date: 2026-10-30
summary: lane no-fallback, blaze-link (2026-10-30): every fallback site in blaze-link classified as error, rule or dead, ranked with the edit-to-running path first, with what each becomes; the typed room refusal (`Refusal`, `LinkError::no_room`) the parent's link reports.
---
# no-fallback: link / blaze-link

Part of [../link.md](../link.md). Kinds: **error** (a real fallback, now an error the link stops
on), **rule** (the platform's linker or loader does the same; it stays, with why), **dead**
(nothing reaches it; deleted). Lines are as of this date; paths are under
`crates/blaze-link/src/`.

## The edit-to-running path (stage, place, write)

| file:line | kind | what it does | what it becomes |
|---|---|---|---|
| `place.rs:597`, `:602`, `:645`, `:668`, `:752`; `file.rs:328`; `stage.rs:1016`; `premap.rs:842` | error | an incremental image without room for a stage refused it with a string only ("link the image from scratch"), so the caller could not tell it from any other failure | done: `LinkError` has `refusal: Option<Refusal>`; `Refusal::NoRoom { need, room }` (a region or synthetic section past its capacity, the end of the call frames), `NoRegion` (the stage needs a region the image was laid out without), `TableFull { slots }` (jump table), `TlsMoved` (thread-locals would move in the run's area). `LinkError::no_room()` gives it; every other error has `None`. Made by `stage::refused`. Test: `tests.rs` `a_stage_out_of_room_is_a_refusal_the_caller_can_match`, `slots_need_a_jump_table`. What comes next (grow as a planned, reported step; else relink on an explicit user action; never a silent relayout): [below](#a-refused-stage-and-what-comes-after-it) |
| `write.rs:516` (fixup loop) | error | a native unwind record's field was zero-filled whenever its target did not resolve: undefined, an alias loop or a special symbol's error too, not only a piece the stage removed | done: zero-filled only when `Column::removed(at)` (the target's piece was removed, as ld drops an FDE of a collected function); anything else is the fixup's error. Test: `write.rs` `a_column_reads_back_values_cells_and_errors` |
| `counter.rs:66` "pairs, all given out" | error (later) | refuses as it should, with a string | a `Refusal` variant when the parent matches it; the counter area is sized at layout like the regions |
| `live.rs:73` `Message::Restart { why: String }` | rule (reported) | a live patch that can't apply tells the running program to restart, with a reason | stays an explicit, reported outcome; a typed reason is for blaze-live (`live.md`) |
| `elf/dynamic.rs:671` `patch_relative`, `:962` `patch_hdr`; `macho/unwind.rs:405` `patch` | rule | patch the kept table in place, or build it again when the patch does not apply | stays: both give the same bytes (a fast path, like SIMD and scalar code). Note for speed: expose the test-only `patched` counters so a bench can see which ran |
| `sanitize.rs:611` `sancov_class_of` `.ok()` | rule | a SanitizerCoverage section the writer refuses reads as "no such class" | stays: every piece in one went through `writer.section` when it was classified, so an error here means the image has no piece of it |
| `state.rs`, `native.rs` sparse columns `get().unwrap_or(default)` | rule | a column not grown to an index reads as its default | stays: the default is the column's meaning of "not set" |
| `lazy.rs:655`, `write.rs` `got_target` TLV lookup | rule | a thread-local's descriptor resolved through its kind | stays: the loader's own rule |
| `elf.rs:447`, `macho.rs:2195`, `pe/coff.rs:124`, `:184`, `target.rs`, `lazy.rs` poisoned `into_inner` | rule | a mutex poisoned by a panicking thread is used | stays: each holds a cache or log whose every write is whole |

## Native input reading

| file:line | kind | what it does | what it becomes |
|---|---|---|---|
| `elf/read.rs:181` `kind` | error | a symbol type blaze-link does not know (an OS- or processor-specific `STT_*`) read as `Other` | done: an error naming the file, symbol and type. Test: `elf/read/tests.rs` `a_symbol_of_a_type_elf_does_not_define_is_an_error` |
| `elf/read.rs:599` `section` | error | a loaded (`SHF_ALLOC`) section of any type was laid out as bytes (`SHT_GNU_ATTRIBUTES`, `SHT_GNU_HASH`, an `SHT_LOOS..` type) | done: loaded sections are `PROGBITS`, `NOBITS`, `INIT_ARRAY`, `FINI_ARRAY`, `PREINIT_ARRAY` (and `X86_64_UNWIND` on x86-64); any other is an error. Test: `a_loaded_section_of_a_type_blaze_does_not_lay_out_is_an_error` |
| `elf/read/shared.rs:62` | error | a `.dynsym` binding other than global and weak was skipped | done: `STB_LOCAL` is skipped (a shared object exports none), any other binding is an error; types go through `kind` |
| `macho/input.rs:109` load commands | error | every load command but the symbol table and segment was skipped | done: `LC_DYSYMTAB`, `LC_BUILD_VERSION`, `LC_VERSION_MIN_*`, `LC_DATA_IN_CODE`, `LC_LINKER_OPTIMIZATION_HINT` and `LC_ATOM_INFO` are skipped (ld64 reads them only for what blaze-link does not make); any other is an error. Test: `macho/input.rs` `refused_inputs` |
| `macho.rs:481` `classify` | error | a section type the writer has no rule for was laid out as a regular section (`_ => {}`): thread-local pointers, init offsets, zerofill in `__TEXT`, interposing outside `__DATA,__interpose` | done: an allowlist (regular, coalesced, literal pointers, C strings, 4/8/16-byte literals, interposing in `__DATA,__interpose`); any other is an error. Test: `refused_inputs` |
| `pe/coff.rs:836` `associated` | error | a COMDAT section whose definition record could not be read was taken as not associative (`.ok()?`) | done: the read's error. `selection()` (its only lenient twin) and `Marks::aux` deleted. Test: `pe/tests.rs` `what_blaze_does_not_link_is_named` |
| `pe/coff.rs:102` `text` | error | every COFF name (symbols, sections, COMDAT leaders, import members, DLLs, `.drectve`) was converted lossily, so a name that isn't UTF-8 named another symbol or DLL | done: an error naming the input ("the name .. is not UTF-8"), as ELF and Mach-O already do. Test: `a_name_that_is_not_utf8_is_an_error` |
| `pe/coff.rs:374` `descriptor_dll` | error | a section name that could not be read was taken as not `.idata$6` | done: the read's error |
| `pe/coff.rs` `long_import` thunk | error | a truncated `.idata$5` read its addend as 0 | done: "a truncated .idata$5" |
| `pe/coff.rs:1145` `/ALTERNATENAME`, weak externals (`SymbolDef::Fallback`) | rule | an undefined `from` resolves to `to` | stays: link.exe's and lld-link's rule, which MSVC's CRT depends on |
| `pe/coff.rs:719` `IMAGE_SYM_DEBUG`, non-linkable storage classes | rule | skipped | stays: link.exe's rule; a relocation naming one is an error (`pe/coff.rs` relocations) |
| `pe/coff.rs:1004` `IMAGE_REL_AMD64_ABSOLUTE`; `elf/read.rs:662`, `:700` `R_*_NONE` | rule | the relocation is skipped | stays: the ABI's no-op relocation |
| `elf/read.rs:564`, `:578`; `pe/coff.rs:1189` | rule | sections that are not loaded (debug info, `.comment`, `.note.*` not `SHF_ALLOC`, `.llvm_addrsig`, `.llvmbc`) are dropped, and relocations of dropped sections with them | stays: ld and lld-link drop them in a link without debug info. A loaded `.note.gnu.property` is dropped too: a behaviour difference noted for later (the output does not mark itself IBT/BTI-compatible) |
| `elf/read.rs:392` | rule | a section group not `GRP_COMDAT` is ignored | stays: ld's rule |
| `macho/input.rs:619`, `:1022` `N_STAB` | rule | stab entries are no symbol | stays: ld64's rule; a relocation naming one is an error |
| macho assembler-local labels, CIE augmentations `S`, `B`, `G`, the zero eh_frame terminator | rule | read as the platform reads them | stays |
| `elf/read/shared.rs:122` soname | rule | a shared object without `DT_SONAME` is named by its file name | stays: ld's rule |
| `elf/read/shared.rs` hidden versym | rule | a hidden version of a symbol is not linked against | stays: ld.so's rule |
| `elf.rs` `e_entry` 0 for a shared object | rule | no entry | stays: the ELF rule |
| `wasm.rs:64`, `macho.rs:1490`, `pe/image.rs:1982` stack size | rule | the platform linker's default when the build gives none | stays: what rustc's link gives the same program |
| `target.rs` `Line::find`, `apple`, `linux` (2026-11-01) | error | a library was searched for only in the link's `-L`s and the toolchain's own directories, never in those the C driver passes its linker (nix's `NIX_LDFLAGS`, `LIBRARY_PATH`): nix's libiconv on macOS, which rustc's link through `cc` finds, was "not found" | done: `linker_dirs` asks the C driver (`$CC_<triple>`, `$CC`, `cc`) with `-###` (the jobs printed, none run) and takes its linker job's `-L`s and `-F`s, searched after the link's own and before the toolchain's; `LIBRARY_PATH` and `NIX_LDFLAGS` are in the cache key. Not found is `Refusal::NotFound(NotFound { library, files, searched: [(dir, Searched::Link / CDriver / Toolchain)] })`, `LinkError::not_found()`, kept through a script's, `/DEFAULTLIB`'s or `LC_LINKER_OPTION`'s context. Test: `target/tests.rs` `a_library_only_on_a_path_the_c_driver_passes_its_linker_is_found`. A relative `-L` from the driver (`LIBRARY_PATH`'s empty entry gives `-L.`) is left out: it names the linker's working directory, which blaze's link has none of (rule) |
| `target.rs` `Line::reexports`, `native.rs` re-export check (2026-11-01) | error | a Mach-O library re-exporting another by its absolute install name (nix's `libiconv.dylib` re-exports `libcharset.1.dylib`) failed the link with "names .../libcharset.1.dylib, which the link line does not have": no `-L` names that library, and ld64 needs none, loading it with the re-exporting one | done: every Mach-O library on the line (dylib or `.tbd`) is read for its outside re-exports (`macho::reexported`: `LC_REEXPORT_DYLIB`s, a stub's re-exports that are no document of it; cached by content, arch and platform), each found as ld64 finds it (under the SDK first, then as it is, a `.tbd` before its `.dylib`; `@loader_path/` from the library's directory) and pushed after it as needed-when-used, transitively, once; one also asked for is loaded used or not. The engine's check takes a library's absolute install name as on the line too (an SDK `.tbd` standing for a `/usr/lib` dylib). Not found is the typed `NotFound` with `Searched::Toolchain` / `InstallName`; `@rpath/` and `@executable_path/` re-exports are an error naming them. Test: `target/tests.rs` `a_macos_library_is_followed_by_the_libraries_it_reexports`. Imports from a re-exported library bind to it rather than to the re-exporting one as ld64's two-level namespace does; the loader resolves both alike |
| `target.rs` `caseless`, newest kit | rule | a directory that can't be read, or a name not a version, is not a candidate | stays: the search ends in the not-found error naming what it looked for |
| AS_NEEDED | rule (noted) | a library given as needed is always needed | later: ld drops one nothing references; the output then has one more `DT_NEEDED` than ld's |
| wasm archive sizing, wasm function names | rule | the sizing pass ignores errors the reading pass raises; a missing name section gives no names | stays: the error comes from the read, names are optional in wasm |
| `elf/read/archive.rs:64`, `wasm/object.rs:125` member names | rule | lossy, for display in errors only | stays |

## A refused stage, and what comes after it

A failed stage never makes the next one a whole layout: `Image::stage` works on a clone of the
state, so after any refusal the next good change stages into the room the image kept
(`Staged::full()` false) and its commit moves the generation by one. Tests: `tests.rs`
`a_removed_function_still_called_through_its_slot_is_an_error` (a refused stage) and
`a_stage_out_of_room_is_a_refusal_the_caller_can_match` (a refusal for want of room).

A refusal for want of room (`err.no_room()` is `Some`) is different: the change itself can't fit,
so it is refused again on every try, and the caller is stuck until something changes the layout.
blaze-link never relays the image out by itself to get past it: a relayout moves every function,
so a running program can't be patched into it, and that is the fallback this lane removes. The
right design has two parts.

- **Grow as a planned step (first choice).** blaze-link gets `Image::grow(&self, refusal) ->
  Result<Growth, LinkError>`. It lays out a new image whose regions keep the old addresses
  and add room past the end of each region that ran out (a patch region, a synthetic section,
  the jump table: each is the last of its kind in its segment, or can be moved to be). Where a
  region can't grow in place (the next region is right after it, or a `TlsMoved` refusal), it
  says so with its own error and does not grow. `Growth` is staged and committed like any
  change, and is reported as its own kind of link (`Linking::Grown { refusal }` in the parent's
  `LinkMade`), so the bench and the user see it, and the bench times it as a grow, never as a
  splice. A live program is told to restart only when a grow moves something it uses.
- **Relink on an explicit user action (where a grow can't apply).** The parent turns the
  refusal into a diagnostic that names the region, `need` and `room`, and says how to link from
  scratch (`blaze build --relink`, or deleting the image). The next build is a first link only
  because the user asked for it, and says so.

Until `grow` exists, the refusal stops the link with the typed error and the parent reports it as
that diagnostic.

## For the parent

`blaze_link::LinkError` gained `pub refusal: Option<Refusal>` and `pub fn no_room(&self) ->
Option<Refusal>`; `pub enum Refusal { NoRoom { need: u64, room: u64 }, NoRegion, TableFull {
slots: u32 }, TlsMoved }` (`Clone, Copy, PartialEq, Eq, Debug`). No crate outside blaze-link
constructs or destructures `LinkError`.
