//! From a laid image to its load plan: what has to happen when the image is placed at some
//! base, said without the base (docs/v2/plan/design/native.md, "Loading", steps 1 to 5).
//!
//! The image ([`Image`]) is blaze-native's: regions in address order from address 0, the bytes
//! from `file_start`, every pc-relative relocation already applied, and what only a loader can
//! know left as [`Fixup`](blaze_native::image::Fixup)s (a rebased pointer, an import plus an
//! addend, a Rust word) and, on ELF, [`TlsReloc`]s. The plan adds what a driver needs and the
//! image does not say in one place:
//!
//! - **Segments** with protections. The image's own split: `[file_start, Got)` is code and
//!   read-only data (Text, Const, EhFrame, UnwindInfo: one read-execute segment, as Mach-O's
//!   `__TEXT`); a non-empty Got is its own page run, written once and then read-only (RELRO); the
//!   rest (Data, Words, Inits, the thread-local regions, Zero) is read-write. Each boundary is a
//!   page of the platform (16 KiB on macOS arm64, 64 KiB on Linux arm64), or the plan refuses.
//! - **Cells**: every 64-bit store, sorted by address, each with what it becomes: a rebased
//!   pointer, an import plus its addend, a Rust word, a thread-local thunk or key, an ELF TLS
//!   descriptor's resolver or argument. A cell in the code segment, outside the image, not
//!   8-aligned, or stored twice is refused.
//! - **Imports** to look up, in the image's order (a [`Cell::Import`]'s index), each weak or
//!   strong; `__tlv_bootstrap` is never looked up: the launcher's own thunk takes its place
//!   (dyld's is a trap that dyld itself rewrites for an image it loads).
//! - **Initializers** in link order (the Inits region, every one a rebased pointer).
//! - **The thread-local template** (ThreadData, then ThreadZero's zeros) and **unwind ranges**.
//!
//! Precondition: the image is what `blaze_native::layout::lay` made. A broken image is refused
//! by name, never trusted (the Trust rule): the plan re-checks every address it uses.

use blaze_native::Platform;
use blaze_native::image::{FixTo, Image, Placed, Region, TlsReloc};
use blaze_native::layout::Layout;
use blaze_own::{AVec, Alloc, Vec};

/// The version of the plan's form ([`LoadPlan::encode`]); bump with any change to what a plan
/// holds or how it is encoded. A record keyed by an image folds it into its key.
pub const PLAN_VERSION: u32 = 1;
/// The encoding's first four bytes.
pub const MAGIC: [u8; 4] = *b"BNLP";
/// Mach-O `struct TLVDescriptor { thunk, key, offset }`: three 64-bit words.
pub const THREAD_VAR_LEN: u64 = 24;
/// The import every Mach-O thread-local descriptor's thunk names (C `_tlv_bootstrap`).
pub const TLV_BOOTSTRAP: &[u8] = b"__tlv_bootstrap";

/// The page of a platform: the unit of every segment boundary and of the base.
pub fn page_of(platform: Platform) -> u64 {
    match platform {
        Platform::MacosArm64 => Layout::MACOS_ARM64.page,
        Platform::LinuxArm64 => Layout::LINUX_ARM64.page,
    }
}

/// What a segment's pages allow once the driver has written them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prot {
    ReadExec,
    /// Written once while the image is placed, then read-only (the Got).
    Read,
    ReadWrite,
}

/// `[at, at + len)` at image addresses (a plan) or absolute ones ([`crate::bind::Writes`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub at: u64,
    pub len: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Segment {
    pub at: u64,
    pub len: u64,
    pub prot: Prot,
}

/// What one 64-bit cell becomes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cell {
    /// The base plus `target` (an image address).
    Rebase { target: u64 },
    /// Import `index`'s address plus `addend`; 0 for a weak import no library defines.
    Import { index: u32, addend: i64 },
    /// The function of Rust word `index` (its slot's entry thunk: one address a function).
    Word { index: u32 },
    /// Mach-O: a descriptor's thunk, the launcher's `tlv_get_addr`.
    TlvThunk,
    /// Mach-O: a descriptor's key, the launcher's thread-local key of this image.
    TlvKey,
    /// ELF: a TLS descriptor's resolver, the launcher's dynamic TLSDESC resolver.
    TlsDescResolver,
    /// ELF: a TLS descriptor's argument, the variable's offset in the template.
    TlsDescArg { offset: u64 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Patch {
    pub at: u64,
    pub cell: Cell,
}

/// One import as the driver looks it up: by `name` among the image's libraries (flat, in load
/// order), `weak` when every reference is weak. `lookup` is false for `__tlv_bootstrap`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Wanted<'a> {
    pub name: &'a [u8],
    pub weak: bool,
    pub lookup: bool,
}

/// A thread's block is `init` copied, then `zero_len` zeros, aligned to `1 << align_log2`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Template {
    pub init: Span,
    pub zero_len: u64,
    pub align_log2: u8,
}

/// What the unwinder is told: the code range, the merged `__eh_frame`/`.eh_frame`, and on
/// Mach-O the `__unwind_info` built from the members' compact unwind rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unwind {
    pub text: Span,
    pub eh_frame: Span,
    pub unwind_info: Span,
}

/// Everything a placement of the image needs, at image addresses.
pub struct LoadPlan<'a> {
    pub platform: Platform,
    pub page: u64,
    /// Bytes to reserve from the base: the image's end, rounded up to a page.
    pub span: u64,
    /// In address order, none empty, none overlapping.
    pub segments: AVec<'a, Segment>,
    /// Where the image's bytes are copied (their length is the image's `bytes.len()`).
    pub copy: Span,
    /// Sorted by `at`, one a cell.
    pub patches: AVec<'a, Patch>,
    /// In the image's import order.
    pub imports: AVec<'a, Wanted<'a>>,
    pub words: u32,
    /// Image addresses of the initializers, in link order.
    pub inits: AVec<'a, u64>,
    pub template: Option<Template>,
    pub unwind: Unwind,
    /// Sorted by name, as the image has them.
    pub exports: AVec<'a, (&'a [u8], u64)>,
}

/// Why an image cannot be planned, naming the address, region or name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanError<'a> {
    /// The image lists no `region` (`lay` lists all twelve).
    NoRegion { region: Region },
    RegionPastEnd { region: Region, end: u64 },
    /// `what` starts at `at`, which is not a multiple of the platform's page.
    Unaligned { what: &'static str, at: u64, page: u64 },
    /// A non-empty Got's last page holds the next region too, so it cannot become read-only.
    GotShared { got_end: u64, next: u64 },
    BytesPastEnd { bytes_end: u64, end: u64 },
    Overflow { at: u64 },
    CellOutside { at: u64 },
    CellInText { at: u64 },
    CellUnaligned { at: u64 },
    CellTwice { at: u64 },
    RebaseOutside { at: u64, target: u64 },
    ImportIndex { at: u64, index: u32 },
    WordIndex { at: u64, index: u32 },
    /// The Words region is not one 8-byte cell a word.
    WordsRegion { len: u64, words: u32 },
    InitCount { counted: u32, found: u32 },
    /// An initializer cell that is not a rebased pointer.
    InitNotLocal { at: u64 },
    /// `__thread_vars` is not a whole number of descriptors.
    ThreadVarsLen { len: u64 },
    /// The descriptor at `at` has no `__tlv_bootstrap` thunk.
    TlvThunkMissing { at: u64 },
    /// `__tlv_bootstrap` named by a cell that is not a descriptor's thunk.
    TlvBootstrapOutsideVars { at: u64 },
    /// ELF initial-exec TLS: the variable must sit at a fixed offset from every thread's thread
    /// pointer, which only the C library's static TLS can give (native.md, "Thread-locals").
    InitialExecTls { got_at: u64, offset: u64 },
    TlsOnMachO { got_at: u64 },
    ThreadVarsOnElf { len: u64 },
    ExportsUnsorted { name: &'a [u8] },
    Alloc,
}

fn push<'a, T>(v: &mut AVec<'_, T>, value: T) -> Result<(), PlanError<'a>> {
    v.try_push(value).map_err(|_| PlanError::Alloc)
}

fn region<'i, 'e>(image: &'i Image<'_>, region: Region) -> Result<&'i Placed, PlanError<'e>> {
    image.region(region).ok_or(PlanError::NoRegion { region })
}

fn span_of(p: &Placed) -> Span {
    Span { at: p.at, len: p.len }
}

fn page_up<'e>(at: u64, page: u64) -> Result<u64, PlanError<'e>> {
    at.checked_add(page - 1).map(|x| x & !(page - 1)).ok_or(PlanError::Overflow { at })
}

/// The load plan of `image`, in `alloc`.
pub fn plan<'a>(image: &Image<'a>, alloc: Alloc<'a>) -> Result<LoadPlan<'a>, PlanError<'a>> {
    let platform = image.platform;
    let page = page_of(platform);
    let end = image.end;
    for p in image.regions.iter() {
        if p.at.checked_add(p.len).is_none_or(|e| e > end) {
            return Err(PlanError::RegionPastEnd { region: p.region, end });
        }
    }
    let file_start = image.file_start;
    if file_start % page != 0 {
        return Err(PlanError::Unaligned { what: "the image's bytes (file_start)", at: file_start, page });
    }
    let bytes_end = file_start.checked_add(image.bytes.len() as u64).ok_or(PlanError::Overflow { at: file_start })?;
    if bytes_end > end {
        return Err(PlanError::BytesPastEnd { bytes_end, end });
    }

    // Segments: code and read-only data, the Got (RELRO), then everything writable.
    let got = region(image, Region::Got)?;
    if got.at % page != 0 {
        return Err(PlanError::Unaligned { what: "the writable part (the Got region)", at: got.at, page });
    }
    let span = page_up(end, page)?;
    let mut segments: AVec<'a, Segment> = Vec::new_in(alloc);
    if got.at > file_start {
        push(&mut segments, Segment { at: file_start, len: got.at - file_start, prot: Prot::ReadExec })?;
    }
    let mut rw_start = got.at;
    if got.len > 0 {
        let got_end = page_up(got.at + got.len, page)?;
        let next = region(image, Region::Data)?.at;
        if next < got_end {
            return Err(PlanError::GotShared { got_end, next });
        }
        push(&mut segments, Segment { at: got.at, len: got_end - got.at, prot: Prot::Read })?;
        rw_start = got_end;
    }
    if span > rw_start {
        push(&mut segments, Segment { at: rw_start, len: span - rw_start, prot: Prot::ReadWrite })?;
    }

    let vars = span_of(region(image, Region::ThreadVars)?);
    let in_vars = |at: u64| at >= vars.at && at - vars.at < vars.len;
    match platform {
        Platform::MacosArm64 if vars.len % THREAD_VAR_LEN != 0 => return Err(PlanError::ThreadVarsLen { len: vars.len }),
        Platform::LinuxArm64 if vars.len != 0 => return Err(PlanError::ThreadVarsOnElf { len: vars.len }),
        _ => {}
    }

    // Every cell: the image's fixups, then what the plan adds.
    let mut patches: AVec<'a, Patch> = Vec::new_in(alloc);
    for f in image.fixups.iter() {
        let cell = match f.to {
            FixTo::Rebase { target } => {
                if target > end {
                    return Err(PlanError::RebaseOutside { at: f.at, target });
                }
                Cell::Rebase { target }
            }
            FixTo::Import { index, addend } => {
                let import = image.imports.get(index as usize).ok_or(PlanError::ImportIndex { at: f.at, index })?;
                if import.name == TLV_BOOTSTRAP {
                    if !in_vars(f.at) || (f.at - vars.at) % THREAD_VAR_LEN != 0 {
                        return Err(PlanError::TlvBootstrapOutsideVars { at: f.at });
                    }
                    Cell::TlvThunk
                } else {
                    Cell::Import { index, addend }
                }
            }
            FixTo::Word { index } => {
                if index as usize >= image.words.len() {
                    return Err(PlanError::WordIndex { at: f.at, index });
                }
                Cell::Word { index }
            }
        };
        push(&mut patches, Patch { at: f.at, cell })?;
    }
    let words = u32::try_from(image.words.len()).map_err(|_| PlanError::Overflow { at: 0 })?;
    let words_region = region(image, Region::Words)?;
    if words_region.len != 8 * u64::from(words) {
        return Err(PlanError::WordsRegion { len: words_region.len, words });
    }
    for i in 0..words {
        push(&mut patches, Patch { at: words_region.at + 8 * u64::from(i), cell: Cell::Word { index: i } })?;
    }
    // Each descriptor's thunk must be there (a sorted search); its key cell is the plan's.
    patches.sort_unstable_by_key(|p| p.at);
    let thunks = patches.len();
    let mut d = 0;
    while d < vars.len / THREAD_VAR_LEN {
        let at = vars.at + d * THREAD_VAR_LEN;
        match patches[..thunks].binary_search_by_key(&at, |p| p.at) {
            Ok(k) if patches[k].cell == Cell::TlvThunk => {}
            _ => return Err(PlanError::TlvThunkMissing { at }),
        }
        push(&mut patches, Patch { at: at + 8, cell: Cell::TlvKey })?;
        d += 1;
    }
    for t in image.tls.iter() {
        match (platform, *t) {
            (Platform::MacosArm64, TlsReloc::Desc { got_at, .. } | TlsReloc::TpRel { got_at, .. }) => {
                return Err(PlanError::TlsOnMachO { got_at });
            }
            (Platform::LinuxArm64, TlsReloc::TpRel { got_at, offset }) => return Err(PlanError::InitialExecTls { got_at, offset }),
            (Platform::LinuxArm64, TlsReloc::Desc { got_at, offset }) => {
                push(&mut patches, Patch { at: got_at, cell: Cell::TlsDescResolver })?;
                let arg = got_at.checked_add(8).ok_or(PlanError::Overflow { at: got_at })?;
                push(&mut patches, Patch { at: arg, cell: Cell::TlsDescArg { offset } })?;
            }
        }
    }
    patches.sort_unstable_by_key(|p| p.at);
    let mut last: Option<u64> = None;
    for p in patches.iter() {
        if last == Some(p.at) {
            return Err(PlanError::CellTwice { at: p.at });
        }
        last = Some(p.at);
        if p.at % 8 != 0 {
            return Err(PlanError::CellUnaligned { at: p.at });
        }
        if p.at < file_start || p.at.checked_add(8).is_none_or(|e| e > end) {
            return Err(PlanError::CellOutside { at: p.at });
        }
        if p.at < got.at {
            return Err(PlanError::CellInText { at: p.at });
        }
    }

    // Initializers: the Inits region's cells, each a rebased pointer, in address (link) order.
    let inits_region = span_of(region(image, Region::Inits)?);
    let mut inits: AVec<'a, u64> = Vec::new_in(alloc);
    for p in patches.iter().filter(|p| p.at >= inits_region.at && p.at - inits_region.at < inits_region.len) {
        match p.cell {
            Cell::Rebase { target } => push(&mut inits, target)?,
            _ => return Err(PlanError::InitNotLocal { at: p.at }),
        }
    }
    let found = u32::try_from(inits.len()).map_err(|_| PlanError::Overflow { at: inits_region.at })?;
    if found != image.inits || inits_region.len != 8 * u64::from(found) {
        return Err(PlanError::InitCount { counted: image.inits, found });
    }

    let mut imports: AVec<'a, Wanted<'a>> = Vec::new_in(alloc);
    for i in image.imports.iter() {
        push(&mut imports, Wanted { name: i.name, weak: i.weak, lookup: i.name != TLV_BOOTSTRAP })?;
    }

    let data = region(image, Region::ThreadData)?;
    let zero = region(image, Region::ThreadZero)?;
    let template = if data.len + zero.len > 0 {
        Some(Template { init: span_of(data), zero_len: zero.len, align_log2: data.align_log2.max(zero.align_log2) })
    } else {
        None
    };
    let unwind = Unwind {
        text: span_of(region(image, Region::Text)?),
        eh_frame: span_of(region(image, Region::EhFrame)?),
        unwind_info: span_of(region(image, Region::UnwindInfo)?),
    };

    let mut exports: AVec<'a, (&'a [u8], u64)> = Vec::new_in(alloc);
    for &(name, at) in image.exports.iter() {
        if exports.last().is_some_and(|&(prev, _)| prev >= name) {
            return Err(PlanError::ExportsUnsorted { name });
        }
        push(&mut exports, (name, at))?;
    }

    Ok(LoadPlan {
        platform,
        page,
        span,
        segments,
        copy: Span { at: file_start, len: image.bytes.len() as u64 },
        patches,
        imports,
        words,
        inits,
        template,
        unwind,
        exports,
    })
}

fn put(out: &mut AVec<'_, u8>, bytes: &[u8]) -> Result<(), PlanError<'static>> {
    out.try_extend_from_slice(bytes).map_err(|_| PlanError::Alloc)
}

fn put_u64(out: &mut AVec<'_, u8>, x: u64) -> Result<(), PlanError<'static>> {
    put(out, &x.to_le_bytes())
}

fn put_span(out: &mut AVec<'_, u8>, s: Span) -> Result<(), PlanError<'static>> {
    put_u64(out, s.at)?;
    put_u64(out, s.len)
}

fn put_name(out: &mut AVec<'_, u8>, name: &[u8]) -> Result<(), PlanError<'static>> {
    put_u64(out, name.len() as u64)?;
    put(out, name)
}

impl LoadPlan<'_> {
    /// The plan as bytes, a function of the plan alone (every table in its sorted order, every
    /// integer little-endian, every name length-first): what a record beside the image's bytes
    /// holds. Nothing of a base, a process or a machine is in it.
    pub fn encode(&self, out: &mut AVec<'_, u8>) -> Result<(), PlanError<'static>> {
        put(out, &MAGIC)?;
        put(out, &PLAN_VERSION.to_le_bytes())?;
        put(out, &[match self.platform {
            Platform::MacosArm64 => 1,
            Platform::LinuxArm64 => 2,
        }])?;
        put_u64(out, self.page)?;
        put_u64(out, self.span)?;
        put_u64(out, self.segments.len() as u64)?;
        for s in self.segments.iter() {
            put_u64(out, s.at)?;
            put_u64(out, s.len)?;
            put(out, &[match s.prot {
                Prot::ReadExec => 5,
                Prot::Read => 1,
                Prot::ReadWrite => 3,
            }])?;
        }
        put_span(out, self.copy)?;
        put_u64(out, self.patches.len() as u64)?;
        for p in self.patches.iter() {
            put_u64(out, p.at)?;
            let (tag, a, b): (u8, u64, u64) = match p.cell {
                Cell::Rebase { target } => (1, target, 0),
                Cell::Import { index, addend } => (2, u64::from(index), addend as u64),
                Cell::Word { index } => (3, u64::from(index), 0),
                Cell::TlvThunk => (4, 0, 0),
                Cell::TlvKey => (5, 0, 0),
                Cell::TlsDescResolver => (6, 0, 0),
                Cell::TlsDescArg { offset } => (7, offset, 0),
            };
            put(out, &[tag])?;
            put_u64(out, a)?;
            put_u64(out, b)?;
        }
        put_u64(out, self.imports.len() as u64)?;
        for w in self.imports.iter() {
            put_name(out, w.name)?;
            put(out, &[u8::from(w.weak) | (u8::from(w.lookup) << 1)])?;
        }
        put(out, &self.words.to_le_bytes())?;
        put_u64(out, self.inits.len() as u64)?;
        for &i in self.inits.iter() {
            put_u64(out, i)?;
        }
        match self.template {
            None => put(out, &[0])?,
            Some(t) => {
                put(out, &[1, t.align_log2])?;
                put_span(out, t.init)?;
                put_u64(out, t.zero_len)?;
            }
        }
        put_span(out, self.unwind.text)?;
        put_span(out, self.unwind.eh_frame)?;
        put_span(out, self.unwind.unwind_info)?;
        put_u64(out, self.exports.len() as u64)?;
        for &(name, at) in self.exports.iter() {
            put_name(out, name)?;
            put_u64(out, at)?;
        }
        Ok(())
    }
}
