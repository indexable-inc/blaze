
# mem-codegen-stencil: the copy-and-patch backend and the fastcompile crates onto blaze-mem

From `mem-codegen-stencil` (child of `mem-codegen`, lane `mem`; report
`/tmp/blaze-lanes/v2-mem-codegen-stencil.md`). None of these paths is free (`fastcompile`,
`fastcompile-stencil`, its `-gen`, `-x64` and `-diff` children are in progress), so each change is
written here for its owner to apply:

- part A, `crates/blaze-stencil/`: lane `fastcompile-stencil` (patch, x64, a64 children touch it);
- part B, `crates/blaze-global/`: lane `fastcompile`;
- part C, `crates/blaze-intent/`: lane `fastcompile`;
- part D, `crates/blaze-premap/`: lane `fastcompile`;
- part E, `crates/blaze-history/`: lane `fastcompile` (budget tests only: its hot path already
  allocates nothing).

Every change keeps semantics and the emitted bytes exactly; where an order matters (labels,
constants, relocations, call sites) the new code makes the same order as the old. Line numbers are
as of 2026-10-29; the files are being edited, so each change also names the function.

The rules it follows (docs/v2/plan/design/memory.md): one task's temporaries go on the task's
`Frame` (`blaze_mem::frame::task(tag, |frame| ..)`, `Vec::new_in(frame)`); per-call buffers on
`blaze_mem::scratch`; tables that only grow on `RegionVec`/`AppendVec`/`Column`; every hot path gets
a `blaze-alloc-count` budget test, zero for hits, lookups, reads and the warm steady state. A
`Vec<T, &Frame>` cannot outlive the task's closure, so each struct that holds one gains a lifetime
parameter; each such struct and its callers are listed.

---

## A. blaze-stencil: every emit on the task's frame

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### Today

`lib.rs:123-157` keeps one `patch::Scratch { buf: Buf, plan: Plan, side: Side }` per thread in a
`thread_local!` `RefCell`; `patch::compile_in` (patch.rs:538-579) clears it, runs the passes, then
moves the output vectors out (`buf.take()`, `mem::take(&mut side.calls)` ...) into a `Code`, so the
next compile on the thread grows them again unless the consumer hands the `Code` back by
`Scratch::recycle`, which nothing outside the tests does. `Buf::take` (buf.rs:190-197) sorts the
relocations with the stable `sort_by_key`, which allocates a buffer above 20 entries;
`Side::finish_calls` (patch.rs:485-486) sorts the call sites the same way. The unwind information is
a fresh `Vec` per function (`arch/x64.rs:121-132`, `arch/aarch64.rs:142-149`,
`x64/frame.rs::win_raw`/`with_saves`, `aarch64/frame.rs::windows_codes`).

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### After

A compile runs inside `blaze_mem::frame::task(Tag::Codegen, ..)`: `Buf`, `Plan` and `Side` are
made on the frame each compile (their vectors are `Vec<T, &'f Frame>`, so they are bump allocations
that the task's end frees at once, and a warm thread's frame chunk is reused). The output is handed
to the caller as a borrowed `CodeRef<'_>` inside the task; the caller copies it once into its own
`Code` (`Code::copy_from`, reusing that `Code`'s vectors) or makes a fresh one (`CodeRef::to_code`,
what `compile` does today). The unwind information is written into the frame too (`Side::info`, and
the DWARF rows stay in `Side::rows`), so a warm compile into a reused `Code` allocates nothing. The
generated `gen/` tables and `table::Library` stay static.

Structs that gain a lifetime parameter (`'f`, the frame's):

| struct | becomes | who names it |
|---|---|---|
| `buf::Buf` | `Buf<'f>` | struct fields only: `arch::Cx`, `patch::emit::Out`, aarch64 tests' `Run`. The ~70 functions taking `&mut Buf` (x64/asm.rs, x64/frame.rs, x64/inst.rs, aarch64/glue.rs, aarch64/frame.rs, aarch64/inst.rs, patch.rs) keep compiling unchanged: an elided lifetime in a path is allowed (the workspace lints don't set `elided_lifetimes_in_paths`). |
| `patch::Plan` | `Plan<'f>` | `Cx`, `emit::Out`, `emit::function`, the tests' sink. Passes taking `&mut Plan`/`&Plan` are unchanged. |
| `patch::Side` | `Side<'f>` | `Cx`, `emit::Out`, `emit::function`. |
| `arch::Cx<'a, A>` | `Cx<'a, 'f, A>` | every `Cx<'_, X>` becomes `Cx<'_, '_, X>`: arch.rs (trait `Glue`), arch/x64.rs (8), arch/x64/inst.rs (14), arch/aarch64.rs (8), patch/emit.rs (1), patch/tests.rs (7). Two lifetimes because `&'a mut Buf<'f>` is invariant in `'f`. |
| aarch64 `trait Ctx<'f>` | `trait Ctx<'f, 'm>` | `impl Ctx for Cx`, the 9 glue functions in aarch64/glue.rs, aarch64 tests' `Run`. |
| aarch64 tests' `Run<'f>` | `Run<'f, 'm>` | aarch64/tests.rs only. |
| `patch::Scratch` | removed | `lib.rs`, patch/tests.rs, x64/tests.rs (both tests get a stand-in, below). |
| new `code::CodeRef<'a>`, `UnwindRef<'a>`, `SlotsRef<'a>` | borrowed output | `lib.rs::compile_to`, `patch::compile`, tests. |

Public API: `blaze_stencil::compile(func, target) -> Result<Code, Error>` keeps its signature and
result. `compile_with` and `patch::Scratch` go (no crate outside blaze-stencil names them:
`rg 'blaze_stencil::' crates` finds no caller). New: `compile_into` (into a reused `Code`) and
`compile_to` (hand the borrowed code to a closure).

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### A1. `crates/blaze-stencil/Cargo.toml`

Under `[dependencies]`, after `blaze-codegen.workspace = true`:

```toml
blaze-mem.workspace = true
```

`[dev-dependencies]` already has `blaze-alloc-count.workspace = true`, and the tests' allocator is
already declared once, in `src/tier/tests.rs:16-17`; nothing else changes there.

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29. `manifest_lint` clean.

### A2. `crates/blaze-stencil/src/lib.rs`

After `#![deny(unsafe_code)]` (line 28):

```rust
#![feature(allocator_api)]
```

Line 42, before: `use std::{cell::RefCell, fmt};` after: `use std::fmt;`

Line 46, before: `pub use code::Code;` after: `pub use code::{Code, CodeRef};`

Lines 123-157, before: the `thread_local! { static SCRATCH .. }`, `pub fn compile` and
`pub fn compile_with` (quoted in "Today"). After:

```rust
/// Compiles one function of Cranelift IR, after ABI lowering, for `target`.
pub fn compile(func: &ir::Function, target: &Target) -> Result<Code, Error> {
    compile_to(func, target, CodeRef::to_code)
}

/// [`compile`] into `out`, whose vectors are reused: a worker that keeps one `Code` allocates
/// only for a function bigger than any before it.
pub fn compile_into(func: &ir::Function, target: &Target, out: &mut Code) -> Result<(), Error> {
    compile_to(func, target, |code| out.copy_from(code))
}

/// [`compile`], handing the code to `sink` while it is still in the compile's frame: the
/// consumer copies what it keeps once, straight to where it goes.
pub fn compile_to<R>(
    func: &ir::Function,
    target: &Target,
    sink: impl FnOnce(&CodeRef<'_>) -> R,
) -> Result<R, Error> {
    let library = table::Library::get(table::Variant::of(target)?)?;
    match target.arch {
        blaze_codegen::Arch::X86_64 => patch::compile::<arch::x64::X64, R>(func, target, library, sink),
        blaze_codegen::Arch::Aarch64 => {
            patch::compile::<arch::aarch64::Aarch64, R>(func, target, library, sink)
        }
        blaze_codegen::Arch::Wasm32 => {
            Err(Error::new("wasm32 has no stencils: its code is written from the IR as a module"))
        }
    }
}
```

The old nested-compile case (a `try_borrow_mut` that failed got fresh columns) is gone: frame tasks
nest, so a compile inside another compile on the same thread just gets a frame of its own.

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29. `compile` is `compile_to(func, target, |code| code.to_code())`: a closure, since the path `CodeRef::to_code` binds the impl's lifetime early and misses the higher-ranked bound.

### A3. `crates/blaze-stencil/src/buf.rs`

Module doc, lines 3-4, before: `stencils and the glue's bounds, and reused across functions: writing
never allocates.` after: `stencils and the glue's bounds, on the compile's frame: writing never
allocates.`

Imports, after `use cranelift_codegen::binemit;`:

```rust
use blaze_mem::frame::Frame;
```

Lines 29-49, before:

```rust
#[derive(Default)]
pub struct Buf {
    bytes: Vec<u8>,
    /// By label: its offset, or `u32::MAX` while unbound.
    labels: Vec<u32>,
    fixups: Vec<Fixup>,
    relocs: Vec<Reloc>,
}

impl Buf {
    pub fn new() -> Buf {
        Buf::default()
    }

    /// Empties it for the next function, keeping every allocation.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.labels.clear();
        self.fixups.clear();
        self.relocs.clear();
    }
```

after:

```rust
pub struct Buf<'f> {
    bytes: Vec<u8, &'f Frame>,
    /// By label: its offset, or `u32::MAX` while unbound.
    labels: Vec<u32, &'f Frame>,
    fixups: Vec<Fixup, &'f Frame>,
    relocs: Vec<Reloc, &'f Frame>,
}

impl<'f> Buf<'f> {
    /// An empty buffer on `frame`, the compile's.
    pub fn new_in(frame: &'f Frame) -> Buf<'f> {
        Buf {
            bytes: Vec::new_in(frame),
            labels: Vec::new_in(frame),
            fixups: Vec::new_in(frame),
            relocs: Vec::new_in(frame),
        }
    }

    /// The frame the buffer is on, for the compile's other temporaries.
    pub fn temp(&self) -> &'f Frame {
        *self.bytes.allocator()
    }
```

`enc::write(form, &mut self.bytes, ..)` in `patch` and `resolve` keeps compiling: it takes
`&mut [u8]`, and `&mut Vec<u8, &Frame>` coerces to it.

Lines 186-210, before: `pub fn take(&mut self) -> (Vec<u8>, Vec<Reloc>)` and
`pub fn recycle(..)` (quoted in "Today"). After:

```rust
    /// The relocations, by offset once [`Buf::sort_relocs`] ran.
    pub fn relocs(&self) -> &[Reloc] {
        &self.relocs
    }

    /// Sorts the relocations by offset, keeping the order of those at one offset (what the
    /// stable `sort_by_key` did), in place.
    pub fn sort_relocs(&mut self) {
        sort_by_key_in_place(&mut self.relocs, |it| it.at);
    }
}

/// A stable sort that never allocates: an insertion sort, linear on what is already in order.
/// Relocations and call sites are recorded as the code is written, so they come nearly sorted.
pub(crate) fn sort_by_key_in_place<T: Copy, K: Ord>(items: &mut [T], key: impl Fn(&T) -> K) {
    if items.is_sorted_by_key(&key) {
        return;
    }
    for at in 1..items.len() {
        let item = items[at];
        let k = key(&item);
        let mut to = at;
        while to > 0 && key(&items[to - 1]) > k {
            items[to] = items[to - 1];
            to -= 1;
        }
        items[to] = item;
    }
}
```

(The closing `}` of `impl Buf` moves above the free function, as shown.) `labels` and `fixups` no
longer need clearing at the end: the frame drops them.

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29. `Buf<'_>` is written out at every use: the workspace warns on `elided_lifetimes_in_paths`, so the elided form this request allowed would warn. Relocations are sorted by `buf::sort_by_key_in_place` (stable insertion sort, no buffer) through `Buf::sort_relocs`.

### A4. `crates/blaze-stencil/src/code.rs`

**A bug to fix first**: `CallSite` (lines 83-88) derives `Copy` but holds a
`std::ops::Range<u32>`, which is not `Copy` (E0204). Before: `#[derive(Clone, Copy, PartialEq, Eq,
Debug)]` after: `#[derive(Clone, PartialEq, Eq, Debug)]`. Nothing copies a `CallSite` implicitly
(`rg 'CallSite' crates/blaze-stencil/src`: built by `finish_calls`, compared in tests).

Append after `Slots` (line 132):

```rust
/// What a compile gives, still in the compile's frame: [`Code`]'s fields as slices, handed to
/// the caller of [`crate::compile_to`] for it to copy what it keeps once.
#[derive(Clone, Copy, Debug)]
pub struct CodeRef<'a> {
    pub bytes: &'a [u8],
    /// A power of two.
    pub align: u32,
    pub relocs: &'a [Reloc],
    pub unwind: Option<UnwindRef<'a>>,
    pub calls: &'a [CallSite],
    pub handlers: &'a [Handler],
    pub lines: &'a [(u32, ir::SourceLoc)],
    pub traps: &'a [(u32, ir::TrapCode)],
    pub slots: SlotsRef<'a>,
}

/// [`Unwind`], borrowed.
#[derive(Clone, Copy, Debug)]
pub enum UnwindRef<'a> {
    Dwarf { len: u32, rows: &'a [(u32, Rule)] },
    Windows { len: u32, info: &'a [u8] },
}

/// [`Slots`], borrowed.
#[derive(Clone, Copy, Debug)]
pub struct SlotsRef<'a> {
    pub base: u16,
    pub offsets: &'a [i32],
}

/// Which unwind information the glue wrote ([`crate::arch::Glue::unwind`]): the DWARF rows the
/// compile recorded, or the Windows `UNWIND_INFO` it wrote into [`crate::arch::Cx::windows_info`];
/// `len` is the bytes of code it covers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnwindKind {
    Dwarf { len: u32 },
    Windows { len: u32 },
}

impl CodeRef<'_> {
    /// An owned copy.
    pub fn to_code(&self) -> Code {
        // alloc: the code the caller keeps, once per function; `compile_into` reuses a `Code`.
        let mut code = Code::default();
        code.copy_from(self);
        code
    }
}

impl UnwindRef<'_> {
    fn to_unwind(self) -> Unwind {
        // alloc: a function's unwind information, only when the `Code` held none of its kind.
        match self {
            UnwindRef::Dwarf { len, rows } => Unwind::Dwarf { len, rows: rows.to_vec() },
            UnwindRef::Windows { len, info } => Unwind::Windows { len, info: info.to_vec() },
        }
    }
}

impl Code {
    /// Makes this `code`, writing into this one's vectors: nothing is allocated unless a vector
    /// is too small, or the unwind information changes kind.
    pub fn copy_from(&mut self, code: &CodeRef<'_>) {
        let CodeRef { bytes, align, relocs, unwind, calls, handlers, lines, traps, slots } = *code;
        refill(&mut self.bytes, bytes);
        self.align = align;
        refill(&mut self.relocs, relocs);
        match (&mut self.unwind, unwind) {
            (Some(Unwind::Dwarf { len, rows }), Some(UnwindRef::Dwarf { len: new, rows: theirs })) => {
                *len = new;
                refill(rows, theirs);
            }
            (Some(Unwind::Windows { len, info }), Some(UnwindRef::Windows { len: new, info: theirs })) => {
                *len = new;
                refill(info, theirs);
            }
            (mine, theirs) => *mine = theirs.map(UnwindRef::to_unwind),
        }
        refill(&mut self.calls, calls);
        refill(&mut self.handlers, handlers);
        refill(&mut self.lines, lines);
        refill(&mut self.traps, traps);
        self.slots.base = slots.base;
        refill(&mut self.slots.offsets, slots.offsets);
    }
}

fn refill<T: Clone>(mine: &mut Vec<T>, theirs: &[T]) {
    mine.clear();
    mine.extend_from_slice(theirs);
}
```

`to_code` gives a `Code` equal (`PartialEq`) to the one `compile_in` built before: same fields, same
order (the capacities may differ, which `PartialEq` doesn't see).

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29. `Unwind` had gained a third variant, `WindowsArm64 { len, codes }`, which the request did not know: `UnwindRef` and `UnwindKind` have it too, and `copy_from` refills it. `Code`, `Unwind` and `Slots` stay not `Clone`.

### A5. `crates/blaze-stencil/src/arch.rs`

Imports: add `use blaze_mem::frame::Frame;`, and `code::UnwindKind` beside the existing `code`
import.

Line 103 (trait `Glue`), before:

```rust
    /// The function's unwind information, from the rows recorded ([`Cx::rows`]) once all of
    /// it is written.
    fn unwind(cx: &mut Cx<'_, Self>) -> Result<Option<code::Unwind>, Error>;
```

after:

```rust
    /// The function's unwind information, once all of it is written: the rows recorded
    /// ([`Cx::rows`], trimmed by [`Cx::retain_rows`]) for DWARF, or Windows' `UNWIND_INFO`
    /// written into [`Cx::windows_info`].
    fn unwind(cx: &mut Cx<'_, '_, Self>) -> Result<Option<UnwindKind>, Error>;
```

Every other `Cx<'_, Self>` in the trait (`count_back_edge`, `relay`, and the rest) becomes
`Cx<'_, '_, Self>`.

Lines 111-120, before:

```rust
pub struct Cx<'a, A: Glue> {
    pub buf: &'a mut Buf,
    pub func: &'a ir::Function,
    pub target: &'a Target,
    pub frame: &'a A::Frame,
    pub(crate) plan: &'a patch::Plan,
    pub(crate) side: &'a mut patch::Side,
}

impl<A: Glue> Cx<'_, A> {
```

after:

```rust
pub struct Cx<'a, 'f, A: Glue> {
    pub buf: &'a mut Buf<'f>,
    pub func: &'a ir::Function,
    pub target: &'a Target,
    pub frame: &'a A::Frame,
    pub(crate) plan: &'a patch::Plan<'f>,
    pub(crate) side: &'a mut patch::Side<'f>,
}

impl<'f, A: Glue> Cx<'_, 'f, A> {
    /// The compile's frame, for the glue's temporaries.
    pub fn temp(&self) -> &'f Frame {
        self.buf.temp()
    }

    /// Keeps the recorded rows `keep` says, in order: what a glue drops from the DWARF rows.
    pub fn retain_rows(&mut self, keep: impl FnMut(&(u32, Rule)) -> bool) {
        self.side.retain_rows(keep);
    }

    /// Where the glue writes Windows' `UNWIND_INFO`, empty until it does.
    pub fn windows_info(&mut self) -> &mut Vec<u8, &'f Frame> {
        self.side.windows_info()
    }
```

(the methods already in the `impl` follow unchanged).

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29. `Cx<'a, 'f, A>`; the frame is named `blaze_mem::Frame` in place. `Cx::windows_info` gives the glue `Side::info`, which holds both Windows x64 info and Windows ARM64 codes.

### A6. `crates/blaze-stencil/src/patch.rs`

Module doc, lines 8-9, before: `Every column is in [`Scratch`], indexed by the IR's entity numbers
and cleared, not freed, between functions: a compile allocates nothing per instruction.` after:
`Every column is on the compile's frame (`blaze_mem::frame::task`), indexed by the IR's entity
numbers: a compile allocates nothing of the heap, and the frame is freed at once at its end.`

Imports (lines 33-42), before:

```rust
use cranelift_codegen::ir;

use crate::{
    Error, Target,
    arch::{Area, Cx, Glue, Loc},
    buf::{Buf, Label},
    code::{CallSite, Code, Handler, Rule, Slots},
```

after:

```rust
use blaze_mem::{
    Tag,
    frame::{self, Frame},
};
use cranelift_codegen::ir;

use crate::{
    Error, Target,
    arch::{Area, Cx, Glue, Loc},
    buf::{self, Buf, Label},
    code::{CallSite, CodeRef, Handler, Rule, SlotsRef, UnwindKind, UnwindRef},
```

Lines 49-69: delete `pub struct Scratch`, its `Default` derive and `impl Scratch` (`new`,
`recycle`).

Lines 202-248, `Plan`: drop `#[derive(Default)]`; `pub(crate) struct Plan {` becomes
`pub(crate) struct Plan<'f> {`; every `Vec<X>` field becomes `Vec<X, &'f Frame>` (`ord`, `bix`,
`uses`, `last`, `def_block`, `konst`, `dead`, `sels`, `kslot`, `slot`, `released`, `places_of`,
`places`, `pseudo`, `stack`, `blocks`, `edges`, `edges_of`), and
`free: [Vec<u32>; 2]` becomes `free: [Vec<u32, &'f Frame>; 2]` (its doc, `slots`' free lists, kept
for their allocations, becomes `slots`' free lists). `base`, `relays`, `area` stay. Line 255,
`impl Plan {` becomes `impl<'f> Plan<'f> {`, and gets first:

```rust
    pub(crate) fn new_in(frame: &'f Frame) -> Plan<'f> {
        Plan {
            ord: Vec::new_in(frame),
            bix: Vec::new_in(frame),
            uses: Vec::new_in(frame),
            last: Vec::new_in(frame),
            def_block: Vec::new_in(frame),
            konst: Vec::new_in(frame),
            dead: Vec::new_in(frame),
            sels: Vec::new_in(frame),
            kslot: Vec::new_in(frame),
            slot: Vec::new_in(frame),
            released: Vec::new_in(frame),
            places_of: Vec::new_in(frame),
            places: Vec::new_in(frame),
            pseudo: Vec::new_in(frame),
            stack: Vec::new_in(frame),
            base: 0,
            relays: false,
            area: (0, 0),
            blocks: Vec::new_in(frame),
            edges: Vec::new_in(frame),
            edges_of: Vec::new_in(frame),
            free: [Vec::new_in(frame), Vec::new_in(frame)],
        }
    }
```

A fresh `Plan` per compile is what the old one was after its passes reset each column
(`scan::reset`, `free.clear()` in `slots::run`), so the passes see the same input.

Lines 340-365, `Side`: drop `#[derive(Default)]`; `pub(crate) struct Side {` becomes
`pub(crate) struct Side<'f> {`; every `Vec<X>` field becomes `Vec<X, &'f Frame>` (`rows`, `sites`,
`traps`, `lines`, `stubs`, `pool_of`, `imm_of`, `const_order`, `rodata`, `moves`, `calls`,
`handlers`, `offsets`); `trap_of: TrapOf` stays inline. Add after `offsets`:

```rust
    /// Windows' `UNWIND_INFO`, when the glue writes it ([`Cx::windows_info`]).
    info: Vec<u8, &'f Frame>,
```

`TrapOf` and its `Default` (lines 367-374) stay.

Lines 376-414, before: `impl Side {` with `fn clear` and `fn recycle`. After:

```rust
impl<'f> Side<'f> {
    fn new_in(frame: &'f Frame) -> Side<'f> {
        Side {
            rows: Vec::new_in(frame),
            sites: Vec::new_in(frame),
            traps: Vec::new_in(frame),
            trap_of: TrapOf::default(),
            lines: Vec::new_in(frame),
            stubs: Vec::new_in(frame),
            pool_of: Vec::new_in(frame),
            imm_of: Vec::new_in(frame),
            const_order: Vec::new_in(frame),
            rodata: Vec::new_in(frame),
            moves: Vec::new_in(frame),
            calls: Vec::new_in(frame),
            handlers: Vec::new_in(frame),
            offsets: Vec::new_in(frame),
            info: Vec::new_in(frame),
        }
    }

    pub(crate) fn retain_rows(&mut self, keep: impl FnMut(&(u32, Rule)) -> bool) {
        self.rows.retain(keep);
    }

    pub(crate) fn windows_info(&mut self) -> &mut Vec<u8, &'f Frame> {
        &mut self.info
    }
```

(`rule`, `rows`, `call_site`, `trap`, `line`, `konst`, `rodata` and `finish_calls` follow
unchanged in body; their `buf: &mut Buf` parameters are fine as they are.) `konst`'s lazy
`table.resize(at + 1, NONE)` stays: a frame `Vec` grows like any other, and the order of labels
(`buf.label()` at first use) and of `const_order` is unchanged, so the constant area is byte for
byte the same.

`finish_calls`, line 486, before: `self.sites.sort_by_key(|it| it.0);` after:
`buf::sort_by_key_in_place(&mut self.sites, |it| it.0);` (same stable order, no allocation; `sites`
holds `(u32, ir::Inst)`, which is `Copy`). Its signature `fn finish_calls(&mut self, func, plan:
&Plan, buf: &Buf)` is unchanged.

Lines 527-579, before: `pub fn compile<A: Glue>(scratch, func, target, library) -> Result<Code,
Error>` and `pub(crate) fn compile_in<A: Glue>(scratch, func, target, library: &Reader<'_>) ->
Result<Code, Error>` (quoted in "Today"). After:

```rust
/// Compiles `func` for `target` with `A`'s glue and `library`'s stencils, handing the code to
/// `sink` while it is in the compile's frame.
pub fn compile<A: Glue, R>(
    func: &ir::Function,
    target: &Target,
    library: &Library,
    sink: impl FnOnce(&CodeRef<'_>) -> R,
) -> Result<R, Error> {
    compile_in::<A, R>(func, target, library.reader(), |code, _| sink(code))
}

/// [`compile`] from a library's reader, also showing `sink` the plan: the tests' own libraries
/// come in here, and the tests read where values went.
pub(crate) fn compile_in<A: Glue, R>(
    func: &ir::Function,
    target: &Target,
    library: &Reader<'_>,
    sink: impl FnOnce(&CodeRef<'_>, &Plan<'_>) -> R,
) -> Result<R, Error> {
    if func.layout.entry_block().is_none() {
        return Err(Error::new("a function with no blocks"));
    }
    frame::task(Tag::Codegen, |temp| {
        let (mut buf, mut plan, mut side) = (Buf::new_in(temp), Plan::new_in(temp), Side::new_in(temp));
        let (buf, plan, side) = (&mut buf, &mut plan, &mut side);
        scan::run(plan, func)?;
        select::run::<A>(plan, func, target, library)?;
        slots::run(plan, func, library, A::NEAR)?;
        let frame = A::frame(func, target, plan.area())?;
        emit::edges(plan, func, library)?;
        let size = emit::size::<A>(plan, func, target, &frame, library)?;
        buf.reserve(size.bytes, size.labels, size.fixups);
        side.lines.reserve(size.lines);
        emit::labels(plan, func, buf);
        emit::function::<A>(buf, plan, side, func, target, &frame, library)?;
        buf.resolve()?;
        let unwind = A::unwind(&mut Cx { buf, func, target, frame: &frame, plan, side })?;
        side.finish_calls(func, plan, buf)?;
        let (base, delta) = A::slot_base(&frame);
        side.offsets.extend(
            func.sized_stack_slots.keys().map(|slot| delta + plan.stack_slot(slot) as i32),
        );
        buf.sort_relocs();
        let unwind = unwind.map(|kind| match kind {
            UnwindKind::Dwarf { len } => UnwindRef::Dwarf { len, rows: &side.rows },
            UnwindKind::Windows { len } => UnwindRef::Windows { len, info: &side.info },
        });
        let code = CodeRef {
            bytes: buf.bytes(),
            align: emit::ALIGN,
            relocs: buf.relocs(),
            unwind,
            calls: &side.calls,
            handlers: &side.handlers,
            lines: &side.lines,
            traps: &side.stubs,
            slots: SlotsRef { base, offsets: &side.offsets },
        };
        Ok(sink(&code, plan))
    })
}
```

The `frame` name stays the glue's frame layout, as before; the memory frame is `temp`. The order of
every step is the old one; `buf.sort_relocs()` is where `buf.take()` sorted.

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29. The side table is `Side::trap_sites` (the request called it `stubs`). `compile_in` checks for an entry block before the task, so a blockless function fails before touching a frame, as before.

### A7. `crates/blaze-stencil/src/patch/scan.rs`

Line 81, before: `pub(super) fn reset<T: Clone>(column: &mut Vec<T>, len: usize, fill: T) {` after:

```rust
pub(super) fn reset<T: Clone, A: std::alloc::Allocator>(column: &mut Vec<T, A>, len: usize, fill: T) {
```

The callers (scan.rs, select.rs, slots.rs, emit.rs) are unchanged.

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29.

### A8. `crates/blaze-stencil/src/patch/slots.rs`

Lines 152 and 163, before: `free: &mut [Vec<u32>; 2]` after: `free: &mut [Vec<u32, &Frame>; 2]`,
with `use blaze_mem::frame::Frame;` added to the imports. The bodies are unchanged.

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29.

### A9. `crates/blaze-stencil/src/patch/emit.rs`

Imports: add `use blaze_mem::frame::Frame;`.

`emit::labels` (339-349) is unchanged: `blocks.clear(); blocks.resize(..)` works on a frame `Vec`.

Lines 351-385, before:

```rust
pub(super) fn function<A: Glue>(
    buf: &mut Buf,
    plan: &Plan,
    side: &mut Side,
    ...
struct Out<'a, A: Glue> {
    buf: &'a mut Buf,
    plan: &'a Plan,
    side: &'a mut Side,
    ...
impl<A: Glue> Out<'_, A> {
    fn cx(&mut self) -> Cx<'_, A> {
```

after:

```rust
pub(super) fn function<'f, A: Glue>(
    buf: &mut Buf<'f>,
    plan: &Plan<'f>,
    side: &mut Side<'f>,
    ...
struct Out<'a, 'f, A: Glue> {
    buf: &'a mut Buf<'f>,
    plan: &'a Plan<'f>,
    side: &'a mut Side<'f>,
    ...
impl<'f, A: Glue> Out<'_, 'f, A> {
    fn cx(&mut self) -> Cx<'_, 'f, A> {
```

(the other fields and the bodies unchanged; `Out::<A> { .. }.function()` in `function` infers both
lifetimes).

`body`, line 657, before: `let mut moves = std::mem::take(&mut self.side.moves);` after:
`let mut moves = std::mem::replace(&mut self.side.moves, Vec::new_in(self.buf.temp()));`
(a frame `Vec` has no `Default`; `Vec::new_in` doesn't allocate, and `self.side.moves = moves;`
gives it back as before).

`parallel`, line 703, before: `moves: &mut Vec<Move>` after: `moves: &mut Vec<Move, &Frame>`.

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29.

### A10. `crates/blaze-stencil/src/arch/aarch64.rs`, `aarch64/glue.rs`, `aarch64/frame.rs`

aarch64.rs lines 28-29, before:

```rust
trait Ctx<'f> {
    fn buf(&mut self) -> &mut Buf;
```

after (`'f` the function's borrows, `'m` the compile's memory frame):

```rust
trait Ctx<'f, 'm> {
    fn buf(&mut self) -> &mut Buf<'m>;
```

Line 42-43, before: `impl<'a> Ctx<'a> for Cx<'a, Aarch64> {` / `fn buf(&mut self) -> &mut Buf {`
after: `impl<'a, 'm> Ctx<'a, 'm> for Cx<'a, 'm, Aarch64> {` / `fn buf(&mut self) -> &mut Buf<'m> {`.
The other `Cx<'_, Aarch64>` in aarch64.rs become `Cx<'_, '_, Aarch64>`.

aarch64/glue.rs, the 9 glue bodies (`prologue` 105, `inst` 189, `result` 240, `ret` 362, `call`
392, `landing` 475, `br_table` 486, `address` 555, `tls` 592): before `fn x<'f>(cx: &mut impl
Ctx<'f>, ..)` after `fn x<'f, 'm>(cx: &mut impl Ctx<'f, 'm>, ..)`. Their bodies are unchanged.

aarch64.rs lines 142-149, `unwind`, before:

```rust
    fn unwind(cx: &mut Cx<'_, Aarch64>) -> Result<Option<code::Unwind>, Error> {
        let len = cx.buf.len();
        if cx.frame.windows {
            return Ok(Some(code::Unwind::Windows { len, info: frame::windows_codes(cx.frame) }));
        }
        // alloc: once per function, the unwind information it hands back.
        Ok(Some(code::Unwind::Dwarf { len, rows: cx.rows().to_vec() }))
    }
```

after:

```rust
    fn unwind(cx: &mut Cx<'_, '_, Aarch64>) -> Result<Option<code::UnwindKind>, Error> {
        let len = cx.buf.len();
        if cx.frame.windows {
            let frame = cx.frame;
            frame::windows_codes(frame, cx.windows_info());
            return Ok(Some(code::UnwindKind::Windows { len }));
        }
        Ok(Some(code::UnwindKind::Dwarf { len }))
    }
```

aarch64/frame.rs line 331-333, before:

```rust
pub(super) fn windows_codes(frame: &Frame) -> Vec<u8> {
    // alloc: once per function, the unwind information it hands back.
    let mut codes: Vec<u8> = Vec::with_capacity(32);
```

after:

```rust
pub(super) fn windows_codes(frame: &Frame, codes: &mut Vec<u8, impl std::alloc::Allocator>) {
    codes.reserve(32);
```

and the last line, before: `codes` (the return) after: nothing (delete it). The pushes, the
`extend_from_slice`s and the final `codes.resize(words * 4, 0)` are unchanged: `codes` starts empty
(`Side::info` is fresh each compile), so `codes.len()` means what it did.

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29. `windows_codes` writes into `Cx::windows_info` and returns nothing; `unwind` returns `UnwindKind::WindowsArm64 { len }` there.

### A11. `crates/blaze-stencil/src/arch/x64.rs`, `x64/frame.rs`, `x64/inst.rs`

x64.rs lines 121-132, `unwind`, before:

```rust
    fn unwind(cx: &mut Cx<'_, X64>) -> Result<Option<code::Unwind>, Error> {
        let len = cx.buf.len();
        if cx.frame.win_unwind {
            let raw = frame::win_raw(cx.rows(), cx.frame.clobber)?;
            let info = frame::with_saves(&raw)?;
            return Ok(Some(code::Unwind::Windows { len, info }));
        }
        // An epilogue at the very end leaves a row at `len`, past the code.
        // alloc: once per function, the unwind information it hands back.
        let rows = cx.rows().iter().filter(|row| row.0 < len).copied().collect();
        Ok(Some(code::Unwind::Dwarf { len, rows }))
    }
```

after:

```rust
    fn unwind(cx: &mut Cx<'_, '_, X64>) -> Result<Option<code::UnwindKind>, Error> {
        let len = cx.buf.len();
        if cx.frame.win_unwind {
            let mut raw = Vec::new_in(cx.temp());
            frame::win_raw(cx.rows(), cx.frame.clobber, &mut raw)?;
            frame::with_saves(&raw, cx.windows_info())?;
            return Ok(Some(code::UnwindKind::Windows { len }));
        }
        // An epilogue at the very end leaves a row at `len`, past the code.
        cx.retain_rows(|row| row.0 < len);
        Ok(Some(code::UnwindKind::Dwarf { len }))
    }
```

(`retain` keeps the rows in order, as the `filter`/`collect` did.) The other `Cx<'_, X64>` in x64.rs
(8 in all) and x64/inst.rs (14) become `Cx<'_, '_, X64>`.

x64/frame.rs `win_raw` (line 652), before: `pub fn win_raw(rows: &[(u32, Rule)], clobber: u32) ->
Result<Vec<u8>, Error> {`, and at line 672-673:

```rust
    // alloc: once per function, the unwind information it hands back.
    let mut out = vec![0u8; size];
```

and its last line `Ok(out)`. After:

```rust
pub fn win_raw(rows: &[(u32, Rule)], clobber: u32, out: &mut Vec<u8, impl std::alloc::Allocator>) -> Result<(), Error> {
```

```rust
    out.clear();
    out.resize(size, 0);
```

and the last line `Ok(())`. The writes `out[0] = 1;` ... `out[end..end + len].copy_from_slice(..)`
are unchanged.

x64/frame.rs `with_saves` (line 731), before: `pub fn with_saves(info: &[u8]) -> Result<Vec<u8>,
Error> {`, with:

```rust
    if saved == 0 {
        // alloc: once per function, the unwind information it hands back.
        return Ok(info.to_vec());
    }
    ...
    // alloc: once per function, the unwind information it hands back.
    let mut out = Vec::with_capacity(info.len() + 4 + 4);
    ...
    Ok(out)
```

after:

```rust
pub fn with_saves(info: &[u8], out: &mut Vec<u8, impl std::alloc::Allocator>) -> Result<(), Error> {
```

```rust
    if saved == 0 {
        out.extend_from_slice(info);
        return Ok(());
    }
    ...
    out.reserve(info.len() + 4 + 4);
    ...
    Ok(())
```

`out` arrives empty (a fresh `Side::info`), so `out.len()` in `out[2] = ((out.len() - 4) / 2) as u8;`
and `out.resize(out.len().next_multiple_of(4), 0)` mean what they did. The `extend`s are unchanged.

> fastcompile-stencil-x64 (owner of `arch/x64.rs`, `arch/x64/`): reviewed A11 and the x64 half
> of A12; both are right as written for the current files (`unwind` at x64.rs:121-132, `win_raw`,
> `with_saves`, the tests' `compile`, `enc`, `windows_codes_as_cranelift`,
> `win_raw_is_cranelifts_encoding`). They only compile together with A3-A6 (`Buf<'f>`,
> `Cx<'a, 'f, A>`, `UnwindKind`, `Cx::temp`/`retain_rows`/`windows_info`, the new `compile_in`),
> none of which has landed yet, so whoever applies A3-A6 may apply A11 and the x64 tests in the
> same change: no need to wait for me. If they land without it, tell `fastcompile-stencil-x64-next2`
> and I apply A11 at once.
> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29. `win_raw` writes into an out-vector on the frame (`Cx::temp`), and `with_saves` into `Cx::windows_info`; both return `Result<(), Error>`.

### A12. The tests that name what changed

**`crates/blaze-stencil/src/patch/tests.rs`.** Line 19, before: `use super::{Kind, NONE, Scratch,
compile_in, slots::GLOBAL};` after: `use super::{Kind, NONE, Plan, Sel, compile_in,
slots::GLOBAL};`. The tests read the plan through `scratch.plan.{slot, base, sels, last, relays,
stack_slot(), area()}`; a test-local `Scratch` holding a copy of those keeps every test body as it
is. Add after the imports' block:

```rust
/// What the tests read of a compile's plan, copied out of its frame.
#[derive(Default)]
struct Seen {
    slot: Vec<u32>,
    last: Vec<u32>,
    sels: Vec<Sel>,
    stack: Vec<u32>,
    base: u32,
    relays: bool,
    area: (u32, u32),
}

impl Seen {
    fn of(plan: &Plan<'_>) -> Seen {
        Seen {
            slot: plan.slot.to_vec(),
            last: plan.last.to_vec(),
            sels: plan.sels.to_vec(),
            stack: plan.stack.to_vec(),
            base: plan.base,
            relays: plan.relays,
            area: plan.area,
        }
    }

    fn stack_slot(&self, slot: ir::StackSlot) -> u32 {
        self.stack[slot.as_u32() as usize]
    }

    fn area(&self) -> Area {
        Area { size: self.area.0, align: self.area.1 }
    }
}

/// The plan of the last compile, where the tests used to find the patcher's scratch.
#[derive(Default)]
struct Scratch {
    plan: Seen,
}

impl Scratch {
    fn new() -> Scratch {
        Scratch::default()
    }
}
```

`fn compile` (line 360-362), before:

```rust
fn compile(scratch: &mut Scratch, func: &ir::Function) -> Code {
    compile_in::<Mock>(scratch, func, &TARGET, &library()).unwrap_or_else(|e| panic!("{func}\n{e}"))
}
```

after:

```rust
fn compile(scratch: &mut Scratch, func: &ir::Function) -> Code {
    try_compile(scratch, func, &library()).unwrap_or_else(|e| panic!("{func}\n{e}"))
}

/// A compile against `library`, keeping what the tests read of its plan.
fn try_compile(scratch: &mut Scratch, func: &ir::Function, library: &Reader<'_>) -> Result<Code, Error> {
    compile_in::<Mock, _>(func, &TARGET, library, |code, plan| {
        scratch.plan = Seen::of(plan);
        code.to_code()
    })
}
```

In `what_no_stencil_and_no_glue_takes_is_an_error_naming_it`: `compile_in::<Mock>(&mut scratch,
&func, &TARGET, &library()).unwrap_err()` becomes `try_compile(&mut scratch, &func,
&library()).unwrap_err()`, and `compile_in::<Mock>(&mut scratch, &func, &TARGET,
&Reader::new(bytes).unwrap()).unwrap_err()` becomes `try_compile(&mut scratch, &func,
&Reader::new(bytes).unwrap()).unwrap_err()`. In
`without_accumulator_shapes_every_value_goes_through_its_slot`: `compile_in::<Mock>(&mut scratch,
&func, &TARGET, &Reader::new(bytes).unwrap()).unwrap()` becomes `try_compile(&mut scratch, &func,
&Reader::new(bytes).unwrap()).unwrap()`.

Mock glue, line 206, before: `fn unwind(_: &mut Cx<'_, Mock>) -> Result<Option<code::Unwind>,
Error> {` after: `fn unwind(_: &mut Cx<'_, '_, Mock>) -> Result<Option<code::UnwindKind>, Error>
{`. The other six `Cx<'_, Mock>` become `Cx<'_, '_, Mock>`.

Replace `a_warm_compile_allocates_only_what_the_code_does_not_give_back` (lines ~902-927) with
the budget test of this change (same function as before: a loop with block arguments, a source
location, a trap stub and a constant):

```rust
#[test]
fn a_warm_compile_into_a_reused_code_allocates_nothing() {
    let func = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let (body, exit) = (b.create_block(), b.create_block());
        let x = b.append_block_param(body, types::I64);
        let y = b.append_block_param(body, types::I64);
        b.ins().jump(body, &[BlockArg::Value(p[0]), BlockArg::Value(p[1])]);
        b.switch_to_block(body);
        b.set_srcloc(SourceLoc::new(3));
        let z = b.ins().iadd(x, y);
        let q = b.ins().udiv(z, y);
        b.ins().brif(q, body, &[BlockArg::Value(y), BlockArg::Value(x)], exit, &[]);
        b.switch_to_block(exit);
        b.ins().return_(&[z]);
    });
    let library = library();
    let mut code = Code::default();
    let into = |code: &mut Code| compile_in::<Mock, _>(&func, &TARGET, &library, |it, _| code.copy_from(it)).unwrap();
    into(&mut code);
    let ((), used) = blaze_alloc_count::measure(|| into(&mut code));
    assert_eq!(used, blaze_alloc_count::Used::default(), "a warm compile: its columns on the frame, its code into the vectors of the last");
    assert!(!code.bytes.is_empty());
    assert!(blaze_mem::frame::spare_frames() > 0, "the compile's frame went back to the thread's spares");
}

#[test]
fn copying_into_a_reused_code_gives_the_fresh_code() {
    let small = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let q = b.ins().udiv(p[0], p[1]);
        b.ins().return_(&[q]);
    });
    let big = function(&[types::I64, types::I64], &[types::I64], |b, p| {
        let c = b.ins().icmp(IntCC::Equal, p[0], p[1]);
        let m = b.ins().select(c, p[0], p[1]);
        let q = b.ins().udiv(m, p[1]);
        let r = b.ins().iadd(q, p[0]);
        b.ins().return_(&[r]);
    });
    let library = library();
    let mut reused = Code::default();
    for func in [&big, &small, &big] {
        compile_in::<Mock, _>(func, &TARGET, &library, |it, _| reused.copy_from(it)).unwrap();
        assert_eq!(reused, compile(&mut Scratch::new(), func), "{func}");
    }
}

#[test]
fn many_constants_are_each_placed_once_in_first_use_order() {
    let values: Vec<u64> = (0..20).map(|i| 0x0101_0101_0101_0101u64.wrapping_mul(i + 1)).collect();
    let func = function(&[], &[types::I64], |b, _| {
        let mut sum = b.ins().iconst(types::I64, 0);
        for i in 0..40 {
            let v = b.ins().f64const(f64::from_bits(values[i % 20]));
            let bits = b.ins().bitcast(types::I64, ir::MemFlags::new(), v);
            sum = b.ins().iadd(sum, bits);
        }
        b.ins().return_(&[sum]);
    });
    let first = compile(&mut Scratch::new(), &func);
    let again = compile(&mut Scratch::new(), &func);
    assert_eq!(first, again, "the constant area is laid out the same each compile");
}
```

(The last test only holds if the mock library has `F64const`/`Bitcast` shapes; if it doesn't,
drop the `bitcast` and return the `f64` sum with `fadd`. What it pins is that labels and
`const_order` come out the same each compile, which the old `FxHashMap`-free `pool_of`/`imm_of`
already do; it guards the frame move.)

**`crates/blaze-stencil/src/arch/x64/tests.rs`.** Line 39, before: `patch::{Scratch,
compile_in},` after: `patch::compile_in,`. Lines 201-203, before:

```rust
fn compile(func: &ir::Function, target: &Target) -> Result<Code, Error> {
    compile_in::<X64>(&mut Scratch::new(), func, target, &mock())
}
```

after:

```rust
fn compile(func: &ir::Function, target: &Target) -> Result<Code, Error> {
    compile_in::<X64, _>(func, target, &mock(), |code, _| code.to_code())
}
```

`fn enc` (line 274-), before: `let mut buf = Buf::new();` inside; after, its body runs in a frame:

```rust
fn enc(f: impl FnOnce(&mut Buf)) -> Vec<u8> {
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |temp| {
        let mut buf = Buf::new_in(temp);
        f(&mut buf);
        buf.bytes().to_vec()
    })
}
```

(keep whatever the current body does after `f(&mut buf)`; if it returns `buf.bytes().to_vec()` as
above, that is it.) `windows_codes_as_cranelift` (line ~427), before:
`assert_eq!(frame::win_raw(&rows, clobber).unwrap(), want);` after:

```rust
    let mut raw = Vec::new();
    frame::win_raw(&rows, clobber, &mut raw).unwrap();
    assert_eq!(raw, want);
```

`win_raw_is_cranelifts_encoding` (lines ~792-794), before: `let raw = frame::win_raw(&rows,
32).unwrap();` ... `let fixed = frame::with_saves(&raw).unwrap();` after:

```rust
    let mut raw = Vec::new();
    frame::win_raw(&rows, 32, &mut raw).unwrap();
    ...
    let mut fixed = Vec::new();
    frame::with_saves(&raw, &mut fixed).unwrap();
```

The other `Cx<'_, X64>` in the tests, if any, become `Cx<'_, '_, X64>`.

**`crates/blaze-stencil/src/arch/aarch64/tests.rs`.** `Run` (lines 142-158), before:

```rust
struct Run<'f> {
    buf: Buf,
    ...
impl<'f> Run<'f> {
    fn new(func: &'f ir::Function, target: &'f Target, frame: &'f Frame) -> Run<'f> {
        let mut buf = Buf::new();
```

after:

```rust
struct Run<'f, 'm> {
    buf: Buf<'m>,
    ...
impl<'f, 'm> Run<'f, 'm> {
    fn new(func: &'f ir::Function, target: &'f Target, frame: &'f Frame, temp: &'m blaze_mem::frame::Frame) -> Run<'f, 'm> {
        let mut buf = Buf::new_in(temp);
```

Line 160-161, before: `impl<'f> Ctx<'f> for Run<'f> {` / `fn buf(&mut self) -> &mut Buf {`
after: `impl<'f, 'm> Ctx<'f, 'm> for Run<'f, 'm> {` / `fn buf(&mut self) -> &mut Buf<'m> {`.
Each test that makes a `Run` (and the three bare `let mut buf = Buf::new();` at ~450, ~479, ~662)
wraps its body in `blaze_mem::frame::task(blaze_mem::Tag::Codegen, |temp| { .. })` and passes
`temp` to `Run::new` / uses `Buf::new_in(temp)`. `windows_codes_are_in_cranelifts_order` (~508),
before: `assert_eq!(frame::windows_codes(&frame), codes);` after:

```rust
    let mut written = Vec::new();
    frame::windows_codes(&frame, &mut written);
    assert_eq!(written, codes);
```

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29. The constants test uses 20 distinct `vconst` I8X16 values through `band` (the mock library has no other constant-pool user) and checks the pool's bytes and order. The x86-64 warm test (name kept) now covers Linux and Windows. Added an AArch64 mock library and `a_warm_compile_through_the_glue_into_a_reused_code_allocates_nothing` over Linux, macOS and Windows. `src/tests/` needed no change.

### A13. `crates/blaze-stencil/src/tier/instrument.rs` (tier 1, per function)

Line 42-43, before:

```rust
    // alloc: one per function compiled at tier 1, beside Cranelift's own per-function tables.
    let mut order = vec![u32::MAX; func.dfg.num_blocks()];
```

after: the rest of `instrument` from here to its end runs in a frame task; `order` is a frame slice:

```rust
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |temp| {
        let order = temp.alloc_slice_fill_with(func.dfg.num_blocks(), |_| u32::MAX);
        // ... the rest of the function body, unchanged, ending in its `Ok(())` ...
    })
```

The `SmallVec<[Edge; 16]>` stays. No budget test: the same function adds a global value, an
imported function and instructions to `func`'s Cranelift tables, which allocate by nature; the
change takes the one temporary of blaze-stencil's own off the heap.

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29. The body from `order` on runs in one `frame::task(Tag::Codegen, ..)`, `order` on the frame.

### A14. Left as is, and why

- `tier.rs` (`Tiering`, `Counters` setup): once per unit; `Driver` already reuses its `read`
  and `promotes` buffers.
- `table/format.rs` `check_index` (once per library load) and `Writer` (build time, `gen/`).
- `shape.rs:1289` (tests only).
- The `Error` messages (`format!` with `// alloc: the error's message`): a failed compile only.

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29: nothing changed there; `hot_lint` over `src/` (outside tests) reports 0.

### Budget tests of part A

- `patch::tests::a_warm_compile_into_a_reused_code_allocates_nothing`: the same function compiled
  twice on one thread, the second measured, is zero allocations.
- `patch::tests::copying_into_a_reused_code_gives_the_fresh_code`: `copy_from` over a bigger,
  then smaller, then bigger function equals a fresh compile each time.
- `patch::tests::many_constants_are_each_placed_once_in_first_use_order`.
- `patch::tests::compiling_the_same_function_twice_gives_the_same_code` (kept, now across frames).

---

> forwarded to fastcompile (from requests, pass 16; told by tmux).
> applied by fastcompile-stencil-mem, 2026-10-29 (part A, and its budget tests): all four budget tests are in `patch/tests.rs`, plus the x86-64 and AArch64 glue tests named in A12.

## B. blaze-global: the fetch and publish paths' temporaries

There are no consumers outside the crate (`rg blaze_global crates` finds only itself and its
`Remote` impl of `blaze-cas`'s one-method trait).

> forwarded to fastcompile (from requests, pass 16; told by tmux).

> applied by fastcompile-sol-global, 2026-10-29 (part B, B1 to B7, and its three budget tests in
> `wire.rs`). Where it differs: B3's `pump` is now `pump_all`, `ask` and `flush` (one request a
> connection in turn, and the connections opened together). Its request buffers are lent from
> `scratch::lend_bytes`, and the keys a request carries stay a `Vec` marked `// alloc:`,
> because the request keeps them until it is answered. B4 also drops the client's own content
> hash: the store's is the one check.

### B1. `crates/blaze-global/Cargo.toml`

```toml
[dependencies]
blaze-mem.workspace = true

[dev-dependencies]
blaze-alloc-count.workspace = true
```

(each in its section, sorted with the others).

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### B2. `crates/blaze-global/src/lib.rs`

After `#![forbid(unsafe_code)]` (line 31): `#![feature(allocator_api)]`. Before `#[cfg(test)] mod
tests;` (line 41):

```rust
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
```

(`blaze-items` and `blaze-units` declare it the same way under `forbid(unsafe_code)`.)

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### B3. `client.rs` `Client::pump` (lines 481-540): request buffers from the thread's shelf, the batch's lists on a frame

Before (excerpts):

```rust
        let mut head = Vec::new();
        while up.asked.len() < PIPELINE {
            let mut body = Vec::new();
            ...
            } else if !publishes.is_empty() {
                let mut bytes = 0;
                let mut items = Vec::new();
                while let Some((_, _, value)) = publishes.front() {
                    ...
                    items.push(publishes.pop_front().expect("a front"));
                }
                wire::header(wire::PUBLISH_ASK, &mut body);
                body.extend_from_slice(&(items.len() as u32).to_le_bytes());
                for (_, signed, value) in &items {
                    body.extend_from_slice(&signed.encode());
                    up.send_window.write(value, &mut body);
                }
                let keys = items.into_iter().map(|(tag, signed, _)| (tag, signed.record.key)).collect();
                (PUBLISH_PATH, Asked::Publish(keys))
            } else if !fetches.is_empty() {
                ...
                let names: Vec<Key> = keys.iter().map(|(_, key)| *key).collect();
                wire::fetch_ask(&names, &mut body);
                (FETCH_PATH, Asked::Fetch(keys))
```

After:

```rust
        let mut head = scratch::lend_bytes();
        let mut body = scratch::lend_bytes();
        while up.asked.len() < PIPELINE {
            body.clear();
            ...
            } else if !publishes.is_empty() {
                let keys = frame::task(Tag::Store, |temp| {
                    let mut bytes = 0;
                    let mut items = Vec::new_in(temp);
                    while let Some((_, _, value)) = publishes.front() {
                        let len = value.len() as u64;
                        if !items.is_empty() && (!wire::takes(bytes, len) || items.len() == BATCH_MOST) {
                            break;
                        }
                        bytes += len;
                        items.push(publishes.pop_front().expect("a front"));
                    }
                    wire::header(wire::PUBLISH_ASK, &mut body);
                    body.extend_from_slice(&(items.len() as u32).to_le_bytes());
                    for (_, signed, value) in &items {
                        body.extend_from_slice(&signed.encode());
                        up.send_window.write(value, &mut body);
                    }
                    // alloc: what the batch published, kept with its request until the answer.
                    items.into_iter().map(|(tag, signed, _)| (tag, signed.record.key)).collect()
                });
                (PUBLISH_PATH, Asked::Publish(keys))
            } else if !fetches.is_empty() {
                ...
                frame::task(Tag::Store, |temp| {
                    let mut names = Vec::with_capacity_in(keys.len(), temp);
                    names.extend(keys.iter().map(|(_, key)| *key));
                    wire::fetch_ask(&names, &mut body);
                });
                (FETCH_PATH, Asked::Fetch(keys))
```

with `use blaze_mem::{Tag, frame, scratch};` added. The fetch branch's `let mut keys = Vec::new();`
stays and gets `// alloc: what the batch asks, kept with its request until the answer.` The
`&mut body`/`&mut head` arguments are `&mut Lent<Vec<u8>>`, which coerce to the `&mut Vec<u8>` that
`wire::header`, `manifest.encode`, `SendWindow::write`, `wire::fetch_ask` and
`http::write_request_head` take; `up.channel.send(&head)` / `send(&body)` coerce to `&[u8]`. Same
bytes on the wire: the body is cleared at the top of each request as the fresh `Vec` was empty.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### B4. `client.rs` `fetched` (lines 662-725): the deferred keys on a frame

Before:

```rust
    let mut deferred = Vec::new();
    for &(tag, key) in keys {
        ...
    }
    reader.end("a fetch's answer")?;
    for it in deferred.into_iter().rev() {
        fetches.push_front(it);
    }
    Ok(())
```

After (the loop body unchanged):

```rust
    frame::task(Tag::Store, |temp| {
        let mut deferred = Vec::new_in(temp);
        for &(tag, key) in keys {
            ...
        }
        reader.end("a fetch's answer")?;
        for it in deferred.into_iter().rev() {
            fetches.push_front(it);
        }
        Ok(())
    })
```

The completions' bytes (`Answer::..encode_framed()`) and `RecvWindow::read`'s kept value outlive
the call (they go to the store), so they stay.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### B5. `wire.rs` `SendWindow::write`: no list for a value of one piece

`pieces` (lines 172-181) allocates a one-entry `Vec` for every value under `CHUNKED_FROM`. Before:

```rust
/// The pieces `value` travels as.
pub(crate) fn pieces(value: &[u8]) -> Vec<std::ops::Range<usize>> {
    if value.is_empty() {
        Vec::new()
    } else if value.len() < CHUNKED_FROM {
        vec![0..value.len()]
    } else {
        blaze_cas::chunk::chunks(value)
    }
}
```

After:

```rust
/// `f` of the pieces `value` travels as: none when empty, the whole of it under
/// [`CHUNKED_FROM`] bytes, else blaze-cas's chunks.
pub(crate) fn with_pieces<R>(value: &[u8], f: impl FnOnce(&[std::ops::Range<usize>]) -> R) -> R {
    if value.is_empty() {
        f(&[])
    } else if value.len() < CHUNKED_FROM {
        f(&[0..value.len()])
    } else {
        // alloc: blaze-cas's chunk list, once per value of `CHUNKED_FROM` bytes or more.
        f(&blaze_cas::chunk::chunks(value))
    }
}
```

`SendWindow::write` (194-219), before: `let pieces = pieces(value);` and `for range in pieces {`;
after, the body in the closure:

```rust
    pub(crate) fn write(&mut self, value: &[u8], out: &mut Vec<u8>) {
        with_pieces(value, |pieces| {
            out.extend_from_slice(&(pieces.len() as u32).to_le_bytes());
            for range in pieces.iter().cloned() {
                // ... the loop body, unchanged ...
            }
        })
    }
```

The tests: `wire.rs:444` `pieces(&value).len()` becomes `with_pieces(&value, <[_]>::len)`;
`wire.rs:490` `for range in pieces(&value) {` becomes `for range in with_pieces(&value, <[_]>::to_vec) {`.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### B6. `wire.rs` `RecvWindow`: one buffer for the pieces held, not a `Box` each

`hold` (282-290) makes a `Box<[u8]>` per piece received; at the steady state each new piece is an
allocation and each one dropped a free. Before (223-228):

```rust
#[derive(Default)]
pub(crate) struct RecvWindow {
    held: VecDeque<Box<[u8]>>,
    first: u64,
    bytes: u64,
}
```

After:

```rust
#[derive(Default)]
pub(crate) struct RecvWindow {
    /// The pieces held, oldest first, end to end; `kept[0]` is byte `base` of all the pieces
    /// this side received.
    kept: Vec<u8>,
    base: u64,
    /// Each piece held: where it starts among all received, and its length.
    held: VecDeque<(u64, u32)>,
    first: u64,
    bytes: u64,
}
```

In `read`, the `SEEN` arm, before: `&*self.held[at as usize]` after:

```rust
                    let (start, len) = self.held[at as usize];
                    &self.kept[(start - self.base) as usize..][..len as usize]
```

`hold`, before:

```rust
    fn hold(&mut self, bytes: &[u8]) {
        self.held.push_back(bytes.into());
        self.bytes += bytes.len() as u64;
        while self.bytes > WINDOW {
            let old = self.held.pop_front().expect("a window over its bytes holds a piece");
            self.bytes -= old.len() as u64;
            self.first += 1;
        }
    }
```

after:

```rust
    fn hold(&mut self, bytes: &[u8]) {
        let end = self.base + self.kept.len() as u64;
        self.kept.extend_from_slice(bytes);
        self.held.push_back((end, bytes.len() as u32));
        self.bytes += bytes.len() as u64;
        while self.bytes > WINDOW {
            let (_, len) = self.held.pop_front().expect("a window over its bytes holds a piece");
            self.bytes -= u64::from(len);
            self.first += 1;
        }
        // The pieces dropped are the buffer's front: cut them once they are half of it, so a
        // byte moves at most once per byte received and the buffer stays under twice the window.
        let live = self.held.front().map_or(self.base + self.kept.len() as u64, |it| it.0);
        let dead = (live - self.base) as usize;
        if dead * 2 > self.kept.len() {
            self.kept.drain(..dead);
            self.base = live;
        }
    }
```

The pieces' bytes, numbers and window accounting are unchanged, so every reference resolves to the
same bytes as before.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### B7. `wire.rs` `parse_fetch_ask` and `server.rs` `fetch`: the request's keys and answers on a frame

wire.rs line 303, before: `pub(crate) fn parse_fetch_ask(body: &[u8]) -> Result<Vec<Key>, String>
{` and `let mut keys = Vec::with_capacity(count);`. After:

```rust
pub(crate) fn parse_fetch_ask(body: &[u8]) -> Result<Vec<Key>, String> {
    parse_fetch_ask_in(body, std::alloc::Global)
}

/// [`parse_fetch_ask`] into `alloc`: the server's on the request's frame.
pub(crate) fn parse_fetch_ask_in<A: std::alloc::Allocator>(body: &[u8], alloc: A) -> Result<Vec<Key, A>, String> {
```

with `let mut keys = Vec::with_capacity_in(count, alloc);` (the rest unchanged). The tests keep
calling `parse_fetch_ask`.

server.rs `fetch` (394-431), before:

```rust
fn fetch(body: &[u8], store: &mut Store, window: &mut SendWindow) -> Result<Response, Response> {
    let keys = wire::parse_fetch_ask(body).map_err(|why| Response::refused(400, &why))?;
    let mut answers = Vec::with_capacity(keys.len());
```

after (the rest of the body unchanged, inside the closure):

```rust
fn fetch(body: &[u8], store: &mut Store, window: &mut SendWindow) -> Result<Response, Response> {
    frame::task(Tag::Store, |temp| {
        let keys = wire::parse_fetch_ask_in(body, temp).map_err(|why| Response::refused(400, &why))?;
        let mut answers = Vec::with_capacity_in(keys.len(), temp);
        // ... unchanged, down to `Ok(Response::new(200, out, Vec::new()))` ...
    })
}
```

with `use blaze_mem::{Tag, frame};`. `out` stays a heap `Vec` (`// alloc: the response's body,
which the connection sends after the request`): it is the response.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### B8. Left as is

- `complete()`/`close()` return `Vec<Completion>` by `mem::take(&mut self.undelivered)`: one per
  completion batch with answers, and it is the caller's to keep (the store takes it); changing it to
  an out-parameter changes `Client`'s public API for every test, so it waits for the owner.
- `Effect::Send { bytes: mem::take(&mut up.staged) }`: the bytes go to the I/O layer
  (`blaze-core`'s `Effect`), whose API owns them.
- Connection setup, TLS, config and bundle manifests: once per link or per bundle.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### Budget tests of part B (in `wire.rs`'s tests)

```rust
#[test]
fn a_value_the_window_holds_travels_with_no_allocation() {
    let value = vec![7u8; 1000];
    let (mut send, mut recv) = (SendWindow::default(), RecvWindow::default());
    let mut wire = Vec::new();
    send.write(&value, &mut wire);
    recv.read(&mut Reader::new(&wire), value.len() as u64, false).unwrap();
    wire.clear();
    let ((), used) = blaze_alloc_count::measure(|| send.write(&value, &mut wire));
    assert_eq!(used, blaze_alloc_count::Used::default(), "a piece the receiver holds goes as its number");
    let (read, used) =
        blaze_alloc_count::measure(|| recv.read(&mut Reader::new(&wire), value.len() as u64, false).unwrap());
    assert_eq!(read, None);
    assert_eq!(used, blaze_alloc_count::Used::default(), "a value refused is read past with no copy");
    let (read, used) =
        blaze_alloc_count::measure(|| recv.read(&mut Reader::new(&wire), value.len() as u64, true).unwrap());
    assert_eq!(read.as_deref(), Some(&value[..]));
    assert_eq!(used.allocations, 1, "a value kept is one buffer of its length");
}

#[test]
fn a_warm_window_holds_new_pieces_with_no_allocation() {
    // Under `CHUNKED_FROM`, so each value is one piece; each distinct, so each is new.
    const LEN: usize = 60 << 10;
    let piece = |n: u64| {
        let mut it = vec![0u8; LEN];
        it[..8].copy_from_slice(&n.to_le_bytes());
        it
    };
    let (mut send, mut recv) = (SendWindow::default(), RecvWindow::default());
    let mut wire = Vec::new();
    // Four windows' worth: the buffer has cut its front more than once and reached its
    // steady size (under twice the window and a piece).
    let rounds = 4 * (WINDOW as usize / LEN) + 2;
    for n in 0..rounds as u64 {
        wire.clear();
        send.write(&piece(n), &mut wire);
        recv.read(&mut Reader::new(&wire), LEN as u64, false).unwrap();
    }
    wire.clear();
    send.write(&piece(u64::MAX), &mut wire);
    let (read, used) =
        blaze_alloc_count::measure(|| recv.read(&mut Reader::new(&wire), LEN as u64, false).unwrap());
    assert_eq!(read, None);
    assert_eq!(used, blaze_alloc_count::Used::default(), "a new piece in a warm window");
}

#[test]
fn a_fetch_parsed_onto_a_frame_allocates_nothing() {
    let keys: Vec<Key> = (0..64u8).map(|i| Key(ContentId([i; 32]))).collect();
    let mut body = Vec::new();
    fetch_ask(&keys, &mut body);
    blaze_mem::frame::task(blaze_mem::Tag::Store, |temp| parse_fetch_ask_in(&body, temp).map(drop)).unwrap();
    let (parsed, used) = blaze_alloc_count::measure(|| {
        blaze_mem::frame::task(blaze_mem::Tag::Store, |temp| parse_fetch_ask_in(&body, temp).map(|it| it.to_vec()))
    });
    assert_eq!(parsed.unwrap(), keys);
    assert_eq!(used.allocations, 1, "only the test's copy out of the frame");
}
```

(`Key(ContentId([n; 32]))` as `wire.rs`'s `fetch_ask` test builds keys. The second test moves
about 128 MiB through the window; it is the one that holds the steady state of B6.)

---

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## C. blaze-intent: a path lookup with no allocation

`path::find` (path.rs 267-306) is the lookup every edit starts with. Today it allocates the split
parts, a `members` list per container, a `String` per member compared (`name_of`'s `to_owned`),
two squeezed `String`s per impl compared (`impl_of`), and the `found` list per step. The tree
itself (`syntax-tree`'s flat tree) walks without allocating once a lazy part is made
(`cursor.rs::part_root` makes each once and keeps it).

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### C1. `crates/blaze-intent/Cargo.toml`

```toml
[dependencies]
blaze-mem.workspace = true

[dev-dependencies]
blaze-alloc-count.workspace = true
```

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### C2. `crates/blaze-intent/src/lib.rs`

After `#![forbid(unsafe_code)]` (line 16): `#![feature(allocator_api)]`. Before `#[cfg(test)] mod
tests;` (line 24):

```rust
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
```

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### C3. `crates/blaze-intent/src/path.rs`

Revised by mem-codegen (2026-10-29 21:19) for the coordinator's rule that new code walks syntax
through `syntax::view`, never `SyntaxNode` (common.md, lane frontend-tree): the lookup (`find`,
`matches`, `name_is`, `same_squeezed`, `describe`) is written on views, which are `Copy` and give
a node's text as one `&'t str`. The handle functions that `edit.rs`, `rename.rs` and
`tests/edits.rs` still call (`members`, `name_of`, `impl_of`, `of`, `is_top`, `top_of`,
`scope_of`) are left as they are, for frontend-tree to move one at a time.

Imports: add `use std::alloc::Allocator;`, `use blaze_mem::{Tag, frame};` and
`use syntax::view::{self, AstNode as _};`.

`Step::Impl` (line 95), before: `Impl { ty: String, of: Option<String>, nth: Option<usize> },`
after (the doc's "read `ty` and `of` with whitespace removed" still holds: the comparison squeezes
both sides):

```rust
    Impl { ty: &'a str, of: Option<&'a str>, nth: Option<usize> },
```

`step` lines 159-167, before:

```rust
        let (ty, of) = match top_level_as(body) {
            Some(at) => (&body[..at], Some(squeeze(&body[at + 4..]))),
            None => (body, None),
        };
        let ty = squeeze(ty);
        if ty.is_empty() || of.as_deref() == Some("") {
            return Err(format!("`{part}` in `{path}` names no type"));
        }
        return Ok(Step::Impl { ty, of, nth });
```

after:

```rust
        let (ty, of) = match top_level_as(body) {
            Some(at) => (&body[..at], Some(&body[at + 4..])),
            None => (body, None),
        };
        let blank = |text: &str| text.chars().all(char::is_whitespace);
        if blank(ty) || of.is_some_and(blank) {
            return Err(format!("`{part}` in `{path}` names no type"));
        }
        return Ok(Step::Impl { ty, of, nth });
```

`split` (99-129), before: `fn split(path: &str) -> Result<Vec<&str>, String> {` /
`let mut parts = Vec::with_capacity(4);` / `Ok(parts)`. After:

```rust
fn split<'p>(path: &'p str, parts: &mut Vec<&'p str, impl Allocator>) -> Result<(), String> {
```

with the `let mut parts = ..` line deleted and the last line `Ok(())` (the pushes unchanged).

`members` (199-224) stays as it is (its callers in `edit.rs` and `tests/edits.rs` hold handles).
After it, add its view form, which `find` walks; when the last handle caller has moved,
`members` goes and `members_of` takes its name:

```rust
/// [`members`] of a view: lent from the tree, nothing collected.
fn members_of<'t>(container: view::Node<'t>) -> impl Iterator<Item = view::Node<'t>> {
    let list = match container.kind() {
        SyntaxKind::SOURCE_FILE => Some(container),
        SyntaxKind::MODULE => container.child_where(|it| it == SyntaxKind::ITEM_LIST),
        SyntaxKind::IMPL | SyntaxKind::TRAIT => {
            container.child_where(|it| it == SyntaxKind::ASSOC_ITEM_LIST)
        }
        SyntaxKind::STRUCT | SyntaxKind::UNION | SyntaxKind::VARIANT => container.child_where(|it| {
            it == SyntaxKind::RECORD_FIELD_LIST || it == SyntaxKind::TUPLE_FIELD_LIST
        }),
        SyntaxKind::ENUM => container.child_where(|it| it == SyntaxKind::VARIANT_LIST),
        _ => None,
    };
    list.into_iter()
        .flat_map(view::Node::children)
        .flat_map(|child| {
            let extern_block = child.kind() == SyntaxKind::EXTERN_BLOCK;
            let items = extern_block
                .then(|| child.child_where(|it| it == SyntaxKind::EXTERN_ITEM_LIST))
                .flatten();
            (!extern_block).then_some(child).into_iter().chain(items.into_iter().flat_map(view::Node::children))
        })
        .filter(|it| Kind::of(it.kind()).is_some())
}
```

The same nodes in the same order as `members`: a list's children of a path's kinds, with an extern
block's items in its place and the block itself left out.

`matches` (248-258), before:

```rust
fn matches(member: &SyntaxNode, step: &Step<'_>) -> bool {
    match step {
        Step::Named { kind, name } => {
            kind.is_none_or(|kind| Kind::of(member.kind()) == Some(kind))
                && member.kind() != SyntaxKind::IMPL
                && name_of(member).is_some_and(|it| strip_raw(&it) == strip_raw(name))
        }
        Step::Impl { ty, of, .. } => {
            impl_of(member).is_some_and(|(its_ty, its_of)| its_ty == *ty && its_of == *of)
        }
    }
}
```

after (on views):

```rust
fn matches(member: view::Node<'_>, step: &Step<'_>) -> bool {
    match step {
        Step::Named { kind, name } => {
            kind.is_none_or(|kind| Kind::of(member.kind()) == Some(kind))
                && member.kind() != SyntaxKind::IMPL
                && name_is(member, name)
        }
        Step::Impl { ty, of, .. } => {
            let Some(imp) = view::Impl::cast(member) else { return false };
            let Some(self_ty) = imp.self_ty() else { return false };
            same_squeezed(self_ty.syntax().text(), ty)
                && match (imp.trait_(), of) {
                    (None, None) => true,
                    (Some(it), Some(of)) => same_squeezed(it.syntax().text(), of),
                    _ => false,
                }
        }
    }
}

/// Whether `member` goes by `name`: [`name_of`] compared without making its `String`.
fn name_is(member: view::Node<'_>, name: &str) -> bool {
    let name = strip_raw(name);
    if member.kind() == SyntaxKind::TUPLE_FIELD {
        let Some(list) = member.parent() else { return false };
        let at = list.children().filter(|it| it.kind() == SyntaxKind::TUPLE_FIELD).position(|it| it == member);
        return at.is_some_and(|at| is_decimal(name, at));
    }
    member
        .child_where(|it| it == SyntaxKind::NAME)
        .and_then(view::Name::cast)
        .is_some_and(|it| strip_raw(it.text()) == name)
}

/// Whether `text` is `n` as `usize`'s `to_string` writes it.
fn is_decimal(text: &str, n: usize) -> bool {
    !text.is_empty()
        && text.bytes().all(|it| it.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'))
        && text.parse::<usize>() == Ok(n)
}

/// Whether `text` with its whitespace removed reads as `want` with its whitespace removed
/// ([`squeeze`] of both, compared without making either).
pub(crate) fn same_squeezed(text: &str, want: &str) -> bool {
    let squeezed = |text: &str| text.chars().filter(|it| !it.is_whitespace());
    squeezed(text).eq(squeezed(want))
}
```

These are the old comparisons exactly: `name_of` gives a tuple field's index as `to_string` (no
sign, no leading zero) and a name's text, both then compared after `strip_raw`; `impl_of` gave the
squeezed self type and trait, `None` without a self type.

`describe` (261-264) takes the view, its body unchanged:
`fn describe(node: view::Node<'_>) -> String {` (its one caller is `find`'s error arm, as
`found.iter().copied().map(describe)`).

`find` (266-306), before: as quoted in "Today" above. After:

```rust
/// The one node `path` names in the file whose tree's root is `root`.
pub fn find<'t>(root: view::Node<'t>, path: &str) -> Result<view::Node<'t>, String> {
    frame::task(Tag::Api, |temp| -> Result<view::Node<'t>, String> {
        let mut parts = Vec::with_capacity_in(4, temp);
        split(path, &mut parts)?;
        let (mut frontier, mut found) = (Vec::new_in(temp), Vec::new_in(temp));
        frontier.push(root);
        for (at, part) in parts.iter().enumerate() {
            let step = step(part, path)?;
            found.clear();
            found.extend(frontier.iter().flat_map(|it| members_of(*it)).filter(|it| matches(*it, &step)));
            if let Step::Impl { nth: Some(nth), .. } = step {
                let count = found.len();
                if nth >= count {
                    return Err(format!("`{path}` asks for impl {nth} of `{part}`, and the file has {count}"));
                }
                found.swap(0, nth);
                found.truncate(1);
            }
            let last = at + 1 == parts.len();
            match (found.len(), &step) {
                // ... the three arms, unchanged ...
            }
            std::mem::swap(&mut frontier, &mut found);
        }
        Ok(frontier[0])
    })
}
```

The error arms keep their `join`/`format!` (a failed lookup only): `parts[..at].join("::")` works on
the frame `Vec`'s slice. `swap(0, nth)` then `truncate(1)` keeps the `nth` match alone, as
`into_iter().nth(nth)` did. No node is cloned or counted: a view is a borrow of the tree and an
index, and the frame's vectors of them go with the task.

`squeeze`, `impl_of` and `name_of` stay for `path::of`, `edit.rs` and `rename.rs`.

`find`'s three callers hold handles today, so each takes the view's node back as one through a
pointer, until frontend-tree moves it to views (when the `.map(..)` goes):

- `rename.rs:66`: `path::find(&file.base, &target.item)` becomes
  `path::find(file.parse.root(), &target.item).map(|it| view::ptr(it).to_node(&file.base))`
  (`base` is `parse.syntax_node()`, the same tree).
- `edit.rs:50`: `path::find(&root, item)` becomes
  `path::find(file.parse.root(), item).map(|it| view::ptr(it).to_node(&root))`.
- `edit.rs:223`: `path::find(&file.root(), into)` becomes
  `path::find(file.parse.root(), into).map(|it| view::ptr(it).to_node(&file.root()))`.

Each file adds `use syntax::view;`. `SyntaxNodePtr::to_node` makes the handle the caller made
before; the zero budget below is `find`'s own.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### C4. `crates/blaze-intent/src/edit.rs` `AddUse`: compare without the strings

Lines 146-153, before:

```rust
            let wanted: String = used.chars().filter(|it| !it.is_whitespace()).collect();
            let present = items_of(&scope).into_iter().any(|it| {
                ast::Use::cast(it)
                    .and_then(|it| it.use_tree())
                    .is_some_and(|tree| {
                        tree.syntax().text().to_string().chars().filter(|it| !it.is_whitespace()).collect::<String>()
                            == wanted
                    })
            });
```

after: this waits for frontend-tree to move `AddUse`'s scope walk (`items_of`) to views. On a
handle the text is a `SyntaxText`, and comparing it without a `String` would be new handle code.
On views it is:

```rust
            let present = items_of(scope).any(|it| {
                view::Use::cast(it)
                    .and_then(|it| it.use_tree())
                    .is_some_and(|tree| path::same_squeezed(tree.syntax().text(), used))
            });
```

The rest of an edit's application (the splice's text, `Working`'s reparse, the `Applied` it
returns) makes what outlives the request, so it is left as it is.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### Budget test of part C (`crates/blaze-intent/src/tests/budget.rs`, `mod budget;` in `tests/mod.rs`)

```rust
use syntax::Edition;

use crate::{file::Working, path};

#[test]
fn a_warm_find_allocates_nothing() {
    let file = Working::parsed(
        "lib.rs",
        Edition::CURRENT,
        "mod m {\n    pub struct S(u8, u16);\n    impl Clone for S {\n        fn clone(&self) -> S { S(self.0, self.1) }\n    }\n}\nfn f() {}\n",
    );
    let root = file.parse.root();
    for wanted in ["f", "m::S::1", "m::<S as Clone>::clone", "m::struct S", "m::< S as  Clone >::clone"] {
        let first = path::find(root, wanted).unwrap();
        let (again, used) = blaze_alloc_count::measure(|| path::find(root, wanted).unwrap());
        assert_eq!(again, first);
        assert_eq!(used, blaze_alloc_count::Used::default(), "a warm find of `{wanted}`");
    }
}
```

The existing `tests/edits.rs` cases (every path of every fixture found and rebuilt by `path::of`)
pin that `find` finds what it found before.

---

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## D. blaze-premap: the image key's temporaries, and no copy of every name

> done (fastcompile-sol-premap), D1–D4 and the budget tests: `blaze-mem` and `blaze-alloc-count` (dev) in `Cargo.toml`, `allocator_api`, both keys' records on a `Tag::Link` frame, the parts borrow names and debug names; `an_image_key_allocates_nothing_of_the_heap` and `splitting_a_build_copies_no_name` in `tests.rs`. Not built here (no builds in this pass).

premap's reads (`View::parse`/`reopen`, `TableView::get`/`find`, `segments()` and the iterators)
allocate nothing already. `Images::pick` is FxHashMap lookups and an `Arc` clone. What does allocate
per build is the image key (`ImageKey::of`: a 64-byte record per function and 96 per static, sorted)
and the split, which clones every `(String, Target)` name and every `(InstanceKey, String)` debug
name of the build into the two parts, a `String` per function.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### D1. `crates/blaze-premap/Cargo.toml`

```toml
[dependencies]
blaze-mem.workspace = true

[dev-dependencies]
blaze-alloc-count.workspace = true
```

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### D2. `crates/blaze-premap/src/lib.rs`

After `#![forbid(unsafe_code)]` (line 18): `#![feature(allocator_api)]`; before the `#[cfg(test)]
mod tests;` (line 35):

```rust
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
```

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### D3. `key.rs` `ImageKey::of` (163-214): its sorted records on a frame

Before: `let mut flat: Vec<[u8; 64]> = functions.iter().map(..).collect();`, `let mut flat:
Vec<[u8; 96]> = statics.iter().map(..).collect();`, `let mut sorted: Vec<&(String, Target)> =
names.iter().collect();`, `let mut sorted: Vec<&(InstanceKey, String)> =
debug_names.iter().collect();`. After: the body from `let mut hasher` on runs in
`blaze_mem::frame::task(blaze_mem::Tag::Link, |temp| { .. ImageKey(hasher.finish()) })`, and each of
the four becomes a frame `Vec` filled the same way:

```rust
        let mut flat = Vec::with_capacity_in(functions.len(), temp);
        flat.extend(functions.iter().map(|KeyedFunction { key, code }| {
            let mut it = [0u8; 64];
            it[..32].copy_from_slice(&key.0.0);
            it[32..].copy_from_slice(&code.0.0);
            it
        }));
        flat.sort_unstable();
        flat.dedup();
        count(&mut hasher, flat.len());
        hasher.update(flat.as_flattened());
```

(the same for the 96-byte statics), and

```rust
        let mut sorted = Vec::with_capacity_in(names.len(), temp);
        sorted.extend(names.iter().copied());
        sorted.sort_unstable_by(|a: &&(String, Target), b| a.0.cmp(&b.0));
```

```rust
        let mut sorted = Vec::with_capacity_in(debug_names.len(), temp);
        sorted.extend(debug_names.iter().copied());
        sorted.sort_unstable();
```

(`names`/`debug_names` are slices of references after D4, hence `.copied()`.) The hashed bytes are
the same: the same records in the same sorted order.

`GraphKey::of`, `set`, `toolchain_into`: left as they are, once per run over units (hundreds) and
features (tens); `natives` is sorted by `radix::sort_copy_by_key`, whose own scratch is
blaze-radix's.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### D4. `split.rs`: the parts borrow the names instead of cloning them

`ImagePart` and `WorkspacePart` (split.rs 64-87), before:

```rust
    pub names: Vec<(String, Target)>,
    pub debug_names: Vec<(InstanceKey, String)>,
```

after (in both structs; their lifetime `'a` is the build's, which already holds the names, as
`functions: Vec<&'a CompiledFunction>` beside them does):

```rust
    pub names: Vec<&'a (String, Target)>,
    pub debug_names: Vec<&'a (InstanceKey, String)>,
```

`image_part`, lines 205-206, before:

```rust
    let names: Vec<(String, Target)> = built.names.iter().filter(|it| in_image(&it.1)).cloned().collect();
    let debug_names = built.debug_names.iter().filter(|it| is_dep(&it.0)).cloned().collect();
```

after:

```rust
    let names: Vec<&(String, Target)> = built.names.iter().filter(|it| in_image(&it.1)).collect();
    let debug_names = built.debug_names.iter().filter(|it| is_dep(&it.0)).collect();
```

`workspace_part`, lines 249-250, the same: drop the two `.cloned()`.

The types that take them, each `&'a [(String, Target)]` becoming `&'a [&'a (String, Target)]` and
`&'a [(InstanceKey, String)]` becoming `&'a [&'a (InstanceKey, String)]`:

- `key.rs` `ImageInputs::names` (101) and `::debug_names` (103);
- `prelink.rs` `Request::names` (33) and `::debug_names` (34);
- `model.rs` `Layout::names` (217): `fn names(&self, names: &[&(String, Target)])`, body unchanged
  (`for (name, target) in names` binds through the reference: write it `for &(ref name, ref target)
  in names` or `for it in names { let (name, target) = *it; ..` if the pattern doesn't bind);
- `model.rs` `whole` (319-320): `image: (&[&CompiledFunction], &[&Static], &[&(String, Target)])`
  and the same for `workspace`.

`dev.rs` (61-62, 102-103) passes `&part.names` / `&part.debug_names` as today: they are now
`&Vec<&(..)>`, which coerce to the new slice types. `model::workspace_names` (418-419) is
unchanged.

Tests (`crates/blaze-premap/src/tests.rs`): `image_key_changes_with_every_instance` (228-259) keeps
owned fixtures and borrows them in its `key` closure:

```rust
    let key = |functions: &[KeyedFunction], statics: &[KeyedStatic], names: &[(String, Target)], debug: &[(InstanceKey, String)]| {
        let names: Vec<&(String, Target)> = names.iter().collect();
        let debug: Vec<&(InstanceKey, String)> = debug.iter().collect();
        ImageKey::of(graph, &ImageInputs { functions, statics, names: &names, debug_names: &debug })
    };
```

and line 259's direct `ImageKey::of(GraphKey(id(1)), &ImageInputs { .., names: &names,
debug_names: &debug_names })` borrows the same way (two `Vec<&_>` made first). Lines 373-374 and
407-408 pass `&part.names`/`&workspace.names`, which coerce. Line 544's `debug_names: &[]` is
unchanged.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### D5. Left as is

`Ready::open`, `Memory::of`, `image::encode`, `prelink::build`, `table::build`, `Job::make`,
`start::plan`: once per image made or per run, and what they make is the image.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### Budget tests of part D (in `tests.rs`)

```rust
#[test]
fn an_image_key_allocates_nothing_of_the_heap() {
    let graph = Graph::new().key();
    let functions: Vec<KeyedFunction> =
        (0..10_000u32).map(|i| KeyedFunction { key: ikey(i as u64 + 1), code: CodeHash(id(i as u64 + 20_000)) }).collect();
    let owned = [("ffi_entry".to_owned(), Target::Function(ikey(1)))];
    let names: Vec<&(String, Target)> = owned.iter().collect();
    let inputs = ImageInputs { functions: &functions, statics: &[], names: &names, debug_names: &[] };
    let first = ImageKey::of(graph, &inputs);
    let (again, used) = blaze_alloc_count::measure(|| ImageKey::of(graph, &inputs));
    assert_eq!(again, first);
    assert_eq!(used, blaze_alloc_count::Used::default(), "a warm image key: its records on the frame");
}

#[test]
fn splitting_a_build_copies_no_name() {
    let small = Small::new();
    let natives = natives();
    let built = small.built();
    let (part, used) = blaze_alloc_count::measure(|| image_part(&built, &small.owned(), &natives).unwrap());
    assert!(part.names.iter().all(|it| built.names.iter().any(|b| std::ptr::eq(b, *it))), "the part's names are the build's");
    assert!(used.allocations <= 7, "the part's seven lists, no `String`: {used:?}");
}
```

(`ikey`, `id`, `Graph`, `Small`, `natives` are the file's own helpers; adjust `ikey`'s argument
type to theirs.)

---

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## E. blaze-history: budget tests only

`History::predict`, `observe` and `record` already allocate nothing: the key table is a fixed
`Box<[Slot]>` (`table.rs::Table::new`, once), `Writer::observe` writes a slot in place, and
`Times::observe` grows its `FxHashMap` only for a kind it has not seen (bounded by the number of
task kinds). `codec::encode`/`decode` run once per save and load, so they are left as they are.
`Estimates` is blaze-graph's.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### E1. `crates/blaze-history/Cargo.toml`

```toml
[dev-dependencies]
blaze-alloc-count.workspace = true
```

(blaze-history needs no `blaze-mem`.)

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### E2. `crates/blaze-history/src/lib.rs`

Before `#[cfg(test)] mod tests;` (line 26):

```rust
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
```

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### E3. The budget test (`tests.rs`)

```rust
#[test]
fn predicting_and_learning_a_seen_kind_allocate_nothing() {
    let mut history = History::new(config(64));
    let kind = KIND;
    let key = same_bucket(3);
    history.observe(kind, Some(&key), 1_000, 4_096);
    history.observe(kind, None, 2_000, 8_192);
    let reader = history.reader();
    let (_, used) = blaze_alloc_count::measure(|| {
        for i in 0..1_000u64 {
            let _ = history.predict(kind, Some(&key));
            let _ = history.predict(kind, None);
            let _ = history.record(kind, &key);
            let _ = reader.get(kind, &key);
            history.observe(kind, Some(&key), 1_000 + i, 4_096);
            history.observe(kind, None, 2_000 + i, 8_192);
        }
    });
    assert_eq!(used, blaze_alloc_count::Used::default(), "the steady state of a kind and key seen before");
}
```

(`config`, `KIND` and `same_bucket` are the file's own helpers, lines 31-41. If blaze-graph's
`Estimates::observe` turns out to allocate per call, this assertion fails on it, and that change
goes to blaze-graph's owner.)

> forwarded to fastcompile (from requests, pass 16; told by tmux).
