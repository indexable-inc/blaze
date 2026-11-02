---
status: superseded
summary: How blaze v2's native proc-macro expanders (#1297) were to be built, when they were to be used, why bitflags has none, and what of them lane fastcompile can take for the proc-macro bridge.
date: 2026-10-29
superseded_by: ../../design/compile-time.md#less-work-in-every-build
---
# Native expanders (#1297)

Superseded on 2026-10-29 by change 20 of
[compile-time.md](../../design/compile-time.md#less-work-in-every-build). Under change 20, proc
macros run as their own code on blaze's MIR evaluator and copy-and-patch tier, isolated by
construction, and lane fastcompile builds that. No expander is vendored or written for #1297.
What carries over to fastcompile's proc-macro bridge is in
[For fastcompile](#for-fastcompile-the-proc-macro-bridge) at the end. The rest is the record of
the plan.

Part of [usercode](../usercode.md). Code: `crates/blaze-expand/src/native/`; requests:
`v2/requests/usercode-macros-native.md`.

## What runs

The expander of a macro is the macro's own code, not a rewrite of it: the published sources
of serde_derive 1.0.229, thiserror-impl 1.0.69 and 2.0.21, tokio-macros 2.7.2, and of what they
are built with (syn 2.0.119 and 3.0.9, quote 1.0.47, proc-macro2 1.0.107, unicode-ident 1.0.27),
vendored under `vendor/native-macros/` and compiled into blaze as ordinary libraries. The only
source change is in the four macro crates' `src/lib.rs`: the lines of their
`#[proc_macro_derive]`, `#[proc_macro_attribute]` and `#[proc_macro]` attributes are deleted, so
the functions are plain `pub fn`s blaze calls. Each `Cargo.toml` is blaze's (renamed packages,
path dependencies, no proc-macro target); the published bytes of both files are kept under
`.published/`, and they are what the table of verified files holds.

The crates are built against `native/proc_macro/` (a package whose library is named
`proc_macro`), which is blaze's WebAssembly guest `proc_macro`
(`crates/blaze-macro-wasm/guest/proc_macro.rs`), the same text from `fn taken` to the end of its
`wire` module, with each wasm import turned into a call to a thread-local host
(`__native::Host`). `native/host.rs` answers those calls as the wasm host does
(`crates/blaze-macro-wasm/src/lib.rs`, `add_host`): spans are numbers the host gives out, the
def, call and mixed sites first, then the input's in the order they first appear; joins,
parents, sources, line and column, file and source text are asked of the database through
`load_cargo::answer`, as for a module. `native/wire.rs` and `native/lex.rs` are the wasm host's
`wire.rs` and `lex.rs`, text for text (blaze-macro-wasm depends on blaze-expand, so the two are
copies held equal by a test rather than one shared module).

### Why this and not hand-written expanders

Span exactness is the hard part (the #1297 comment's analysis): the output of serde_derive is a
function of how syn parses the input, of where quote's `quote_spanned!` puts call-site and
input spans, and of what proc-macro2 does with a join that fails. A hand-written expander must
reproduce every one of those choices for every input, forever, and drift silently with each
release. Running the published code against a `proc_macro` that is the one the wasm module
already uses leaves one place where the two can differ: the host's answers, which are the same
code paths. Supporting a new version is vendoring it and rerunning the tests, not porting it.

The cost is compile time of blaze (two syns, compiled once) and that blaze links code it
did not write. That code is what the user's build would run anyway, and none of the vendored
packages reads a file or an environment variable as it expands, so it needs no sandbox: what it
sees is its input and the host's answers.

## When it is used: an exact-match cache

`native::find(dylib, name, kind)` reads the library's closure from the hook registered with
`describe_with` (from the `rust-project.json` blaze wrote to build the library's module,
`closure_of_description`): for the macro's crate and every package it depends on but the
standard library, its name, version, the BLAKE3 of every file of its directory (but cargo's
`.cargo-ok` and `.cargo-checksum.json`), its cfg (features and what its build script set), and
its dependencies' names. It is the verified copy when, from the macro's crate down its
dependencies, every package has the same name and version, the same set of files with the same
hashes, the same features, the same dependency names pointing at the same verified packages,
and the same cfgs of those any source of it or its dependencies names. A cfg no compiled source
of the package or its dependencies names as a word (as `debug_assertions`) can't change the code
and is not compared. Target cfgs are not in a description; the native code runs on the same host
the real macro does.

Anything else (one byte, one more feature, another syn) is `Ok(None)`: the macro is built and
expanded as it would be with no native expander. That is a miss of a cache keyed by the whole
closure, not a fallback: the result is the same expansion either way. A failure to read or
describe the library is `Err`, which fails the expansion. `native::report()` counts hits and,
for each library of a verified crate's name and version that missed, its first difference.

A known limit of hit rate, not of correctness: cargo unifies features over the whole build, so
syn 3 is verified with the union serde_derive, thiserror-impl 2 and tokio-macros ask for
(clone-impls, default, derive, full, parsing, printing, proc-macro). A build whose other proc
macros ask syn for more (`extra-traits`, `visit-mut`) or that has none of tokio-macros' `full`
misses. Adding a configuration is vendoring the same sources again as a package built with other
features; which to add is a question for the corpus's miss report.

`Macro::identity` is a hash of the verified closure (every file, cfg and dependency), of the
native code (`native/*.rs`, `proc_macro/`), and of the macro's name and kind: what its
expansions are a function of besides their input and the database's answers.

## Proving it: verify mode

`BLAZE_NATIVE_MACROS=verify` (read by blaze-expand's `lib.rs`) expands every call with a native
expander both ways and fails the expansion, naming the macro, at `native::difference`'s first
difference: token by token, each delimiter kind, character, spacing, identifier text and
rawness, literal kind, text and suffix, and every span's range, anchor and syntax context.
Running the corpus with it set is the check that the expanders are exact; the log line
`native proc macros: N found; …` says how many were used and why the rest missed.

Tests beside the code (`native/tests.rs`) check: the vendored files are the table's and the
built `lib.rs` is the published one less its attribute lines; the verified closure matches
itself; a changed byte, a file more or less, a feature more or less, another syn, a dependency
less each miss with the message naming it; unnamed cfgs are not compared; proc-macro2's
build is what its code shows; `find` gives an expander only for the verified copy and errs
for an undeclared macro or an undescribed library; descriptions are read as closures;
`difference` is `None` for equal trees and names a changed text, rawness, spacing, range,
anchor, context or suffix, and a shorter tree; and serde's `Serialize` and `Deserialize`
(structs, enums, generics, `#[serde(rename, skip)]`, a union's error), thiserror 1 and 2
(`#[error("…")]`, `transparent`, `#[from]`, `#[source]`, a missing attribute's error),
tokio's `main` (default and `flavor = "current_thread"`, a non-async error), `test`, and its
`select!` helpers (including the panic they raise) expand to what they should. The one-pass
reader reads every written tree, negative literals included, as the wasm host's two-pass
reader does; `find` follows the files on disk (a kept snapshot is the same one, a changed byte
of syn 3 misses with its file named, the restored bytes hit again, an extra file misses); and a
stamp changed in the tick it was read in is not trusted.

## Speed: the fastest known technique per part

**Matching a library (`find`).** The technique is git's index: a stat cache of every file of the
closure with git's racily-clean rule, which is the fastest known way to know a tree is unchanged
without a file watcher. Per library, after the first load, a hit is one `stat` of the
description and one of each file and directory of its closure, compared with a 32-byte stamp
(length, inode, mtime and ctime in nanoseconds, `const`-asserted) walked down one column of a
struct-of-arrays snapshot. It reads no file, takes no lock, writes no shared memory (one
`AtomicU32` "last snapshot", stored only when it changes) and allocates nothing (the
description path is a per-thread buffer the hook fills). The verdict, the macro table and each
macro's identity are computed once per snapshot (`OnceLock`) and kept by content id in
`blaze_ids::Sharded` tables, append-only with lock-free reads: descriptions by (path, stamp),
snapshots by their stamps and hashes, file hashes by (path, settled stamp). A stamp within
`SETTLE_NS` (2 s, FAT's coarsest clock) of the moment it was read is not trusted: such a
snapshot is used once and not kept, and such a file is hashed again next time. A miss hashes
only the files whose stamps changed, with BLAKE3 streamed from the file (`update_reader`, its
SIMD paths chosen at run time). Cfg matching is a merge of sorted lists against
precomputed identifier-word sets (FxHash) of the compiled sources, and each closure's package
indices and pairing are precomputed.

What would beat it, and why it isn't used yet: Merkle directory hashes kept by the VFS would
make a hit one 32-byte compare per package with no `stat`, and a watcher (FSEvents, inotify, or
watchman's clock) would make it cost in proportion to changes. Both belong to the VFS, which
blaze-expand does not see; using them means the hook giving a directory hash beside the
description path. Batched stats
(`getattrlistbulk` on macOS, `statx` through io_uring on Linux) would cut the syscall count of
the walk; they are the next step if a load profile shows the walk.

**Expanding.** Each thread keeps its span table and its input, attribute and output buffers
(`Scratch`, taken out of a `Cell` for the call, so a reentrant expansion gets fresh ones) and
reuses them: a hit allocates nothing for the wire. The input is written once in the wasm wire
encoding, and the output is read in one pass straight into rust-analyzer's flat token buffer
(`TopSubtree::from_serialized`), with no tree per group in between. Inside the expander the
crates' own code runs as compiled native code, with no wasm boundary or copy into linear memory.

What isn't matched: the shim's `TokenStream` is the wasm guest's (`Rc<Vec<TokenTree>>`, `Rc<str>`
text), and it is the wasm lane's to change. The reference is rustc's proc-macro bridge, whose
handles are `u32`s into server-side interned symbols and spans; moving the guest to handles into
an arena would remove the per-token allocations and the encoding.

## bitflags: no native expander

`bitflags!` 2.x is a `macro_rules!` macro. Its expansion is not a run of user code: it is
`mbe` matching and transcribing in process, already as native as it gets, and its cost is per
step. Its call expands to calls of the crate's internal macros (`__declare_public_bitflags!`,
`__impl_public_bitflags!`, `__impl_bitflags!` and the rest), each a separate expansion with its
own `MacroCallId`. The syntax context of every token is the chain of marks of the expansions
that produced it, with `$crate` resolved through those marks and each step's def-site tokens
anchored to the bitflags crate's own source.

Replacing the first step alone could be made exact (mbe's transcription rules for spans and
`SemiTransparent` marks, given the definition's token spans), but it would save nothing: mbe
does the same work. Replacing the whole recursion with one step is what would save time, and it
can't be exact: one expansion has one mark, so the output's syntax contexts would lack the
marks of every inner expansion, which changes hygiene (local names and `$crate` across steps)
and every span's expansion parent. So there is none, and no hook in `hir-expand` is requested.
Making bitflags cheaper belongs in mbe (cached matching per definition), which is the format
and db lanes'.

## For fastcompile: the proc-macro bridge

What this lane wrote that change 20's bridge can use. It is in `crates/blaze-expand/src/native/`,
which now holds only the bridge, written and not built. The expander matching (`closure.rs`,
`files.rs`, `verified.rs`, `find`, `Macro`) is deleted, and is in git at `df97d0934a`. The
entry is `native::expand(Expander, db: Option<&dyn SourceDatabase>, input, attr, def_site,
call_site, mixed_site)`, where `Expander` is a macro's `fn(TokenStream) -> TokenStream` or `fn(TokenStream, TokenStream) -> TokenStream`
compiled against the shim. Tests run small macros through it: an echo, a parsed stream, the
sites, an attribute, a panic, and an expansion within an expansion.

- **The bridge's whole surface is 20 functions.** `proc_macro/lib.rs` is the wasm guest's
  `proc_macro` (`crates/blaze-macro-wasm/guest/proc_macro.rs`, held equal by a test). Every one
  of its imports goes to one trait, `__native::Host`:
  - the three sites;
  - `span_join`, `span_resolved_at`, `span_located_at`, `span_parent`, `span_source`,
    `span_start`, `span_end`, `span_subspan`, `span_byte_range`, `span_line`, `span_column`;
  - `span_debug`, `span_file`, `span_local_file`, `span_source_text`;
  - `ts_from_str`, `literal_from_str`.

  Those 20 are 20 of the guest's 24 `sys` imports. The other four are:
  - `take`, which copies out the bytes of the last answer that gave text or a stream;
  - `track_path`, `track_env_var` and `diagnostic`, which are no-ops in the shim as they stand.

  Token streams cross only as whole inputs and outputs, in the wasm wire encoding. These are the
  shims change 20 names as "the only way out", and each is a pure question of the database or
  of the lexer.
- **How questions are recorded, in the wasm host.** `crates/blaze-macro-wasm/src/lib.rs`
  `State::ask_value` records each question a macro asks of a span: line and column, source
  text, byte range, file, local file. The question is recorded as `Value::read`, a NUL, a
  letter, then the span as start, end, and the index of its anchor and context among the
  input's. The answer is recorded as `Value::said`. `what_it_read` puts each pair into the
  expansion's `Traced` as an environment variable, so an output is found again only while the
  database answers the same; `find_answered` answers those questions outside the store's lock.
  A question it can't record sets `asked`, and the output is then not kept: a span with an
  anchor or context the input lacks, `span_debug`, or a question answered with a span (a join
  across anchors, `span_parent`, `span_source`). The bridge's
  `host.rs` answers the same questions the same way but records none, because nothing keeps
  its outputs yet. A cache of change 20's outputs has to record them as the wasm host does.
- **The answers, exactly as the wasm host gives them.** `host.rs` implements `Host` against
  `load_cargo::answer`. It numbers spans the same way the wasm host does: the def, call and
  mixed sites are 1, 2 and 3, then the input's spans in order of first appearance. It also
  handles the same fix-up-span and cross-anchor join cases, and gives the same defaults where
  nothing can be asked (line and column 1, span 0). So a macro gives the same tokens under
  either host. An evaluator-backed bridge can answer its shims with this code as it stands.
- **Reentrancy and panics.** `with_host` sets a thread-local host. It restores the previous
  host on drop, so an answer that expands another macro on the same thread is sound; the
  `SAFETY` comment states the invariant. `caught` turns a macro's panic into its message
  without printing it (one panic hook, installed once). Per-thread buffers (`Scratch`) are
  taken out of a `Cell` for each call, so a nested expansion gets fresh ones.
- **Reading the output with no tree per group.** `wire::Reader` reads the encoding in one
  pass, straight into rust-analyzer's flat `TopSubtree`, and splits a negative literal into
  `-` and the literal. A test checks it against the wasm host's two-pass reader.
- **A differential check against the wasm host.** `difference(a, b)` names the first
  differing token: text, rawness, spacing, kind, suffix, or a span's range, anchor or context.
  The verify mode that used it (`BLAZE_NATIVE_MACROS=verify`, expanding each call both ways)
  is gone with the expanders. Change 20's tier can be held to the wasm modules, and to rustc's
  outputs, the same way until it replaces them.
- **A key for the macro that misses nothing.** Change 20 caches outputs "by input tokens and
  macro version". A version string misses inputs the output depends on. The deleted
  `closure.rs` (in git at `df97d0934a`) computed the identity of a macro library's whole
  closure, from the `rust-project.json` that
  `own_macro.rs` writes beside each module. For each package that identity covers:
  - name and version;
  - the BLAKE3 of every file;
  - features;
  - the cfgs its sources name;
  - its dependencies' names.

  It read only libraries whose first crate was a verified one (`ROOTS`); a key for every
  library drops that filter. Repeat loads checked it with a git-index-style stat cache, with
  git's rule for files changed in the same clock tick they were read. A hit cost one `stat` per
  file and allocated nothing. Two more inputs belong in the key:
  - the environment variables the macro read (the guest's `tracked::env_var` calls
    `std::env::var` in process, so the bridge must answer it from the expansion's traced
    environment instead);
  - the files it read.
- **Where the bridge can be faster than all of this.** Change 20 runs the macro and blaze in
  the same address space, so the wire encoding is optional. Streams can cross as `u32`
  handles into a host arena of interned trees, as rustc's own bridge does, instead of being
  encoded and rebuilt. The guest's `Rc<Vec<TokenTree>>` and `Rc<str>` would then become
  handles, and there would be no allocation per token.
