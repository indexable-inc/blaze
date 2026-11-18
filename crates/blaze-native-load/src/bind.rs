//! A load plan bound at a base: the exact writes a driver performs (native.md, "Loading",
//! step 5). Pure: the driver looks the imports up (by name, flat, among the image's libraries
//! in load order: `dlsym` over each library's handle), creates the thread-local key, reserves
//! the base, and hands all of it in as a [`Site`]; [`bind`] answers every 64-bit store.
//!
//! What the driver then does, in this order (each step is an effect; the order is the
//! contract):
//!
//! 1. reserve `[base, base + span)` (on macOS the read-execute segment is `MAP_JIT` memory,
//!    as the launcher's code area is: an unsigned image's pages cannot be mapped executable);
//! 2. copy the image's bytes to [`Writes::copy`];
//! 3. make every [`Writes::stores`] entry (8 bytes, little-endian);
//! 4. give each [`Writes::segments`] entry its protection (the Got becomes read-only here);
//! 5. make the code visible to instruction fetch;
//! 6. tell the unwinder about [`Writes::unwind`] (macOS: one callback over the whole range
//!    answering `dso_base = base`, the `__unwind_info` and the `__eh_frame`; Linux: the
//!    `.eh_frame` once), before any of the image's code can run;
//! 7. publish the exports ([`export`]) to the code records that call C;
//! 8. on the program's first thread, before the entry, call [`Writes::inits`] in order.
//!
//! A refusal comes before step 1: an import no library defines and no weak reference allows
//! is [`BindError::Unresolved`], never a null pointer found by a call (rule 3).

use blaze_own::{AVec, Alloc, Vec};

use crate::plan::{Cell, LoadPlan, Segment, Span, Template, Unwind};

/// What the driver found and made, for one placement of one plan.
pub struct Site<'s> {
    /// The reserved base, a multiple of the plan's page.
    pub base: u64,
    /// One answer per [`LoadPlan::imports`] entry, in order: the address, or `None` where no
    /// library defines the name (an entry with `lookup: false` is not looked at).
    pub imports: &'s [Option<u64>],
    /// The entry thunk of each Rust word's slot, in the image's word order.
    pub words: &'s [u64],
    /// Mach-O: the launcher's `tlv_get_addr` (it keeps every register but x0, as dyld's does).
    pub tlv_thunk: Option<u64>,
    /// Mach-O: the thread-local key the driver created for this image.
    pub tlv_key: Option<u64>,
    /// ELF: the launcher's dynamic TLSDESC resolver.
    pub tlsdesc_resolver: Option<u64>,
}

/// One 64-bit store at an absolute address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Store {
    pub at: u64,
    pub value: u64,
}

/// Everything the driver does, at absolute addresses.
pub struct Writes<'a> {
    pub base: u64,
    pub span: u64,
    pub segments: AVec<'a, Segment>,
    pub copy: Span,
    /// Sorted by `at`, one a cell.
    pub stores: AVec<'a, Store>,
    pub inits: AVec<'a, u64>,
    pub template: Option<Template>,
    pub unwind: Unwind,
}

/// Why a plan cannot be placed at a site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BindError<'a> {
    BaseUnaligned { base: u64, page: u64 },
    /// `base + span` passes the end of the address space.
    Overflow { base: u64, span: u64 },
    ImportCount { want: usize, given: usize },
    WordCount { want: u32, given: usize },
    /// A strong import that no library of the image defines.
    Unresolved { name: &'a [u8] },
    NoTlvThunk,
    NoTlvKey,
    NoTlsDescResolver,
    Alloc,
}

fn push<'a, T>(v: &mut AVec<'_, T>, value: T) -> Result<(), BindError<'a>> {
    v.try_push(value).map_err(|_| BindError::Alloc)
}

fn moved(s: Span, base: u64) -> Span {
    Span { at: base + s.at, len: s.len }
}

/// The writes of `plan` placed at `site`, in `alloc`.
pub fn bind<'a>(plan: &LoadPlan<'a>, site: &Site<'_>, alloc: Alloc<'a>) -> Result<Writes<'a>, BindError<'a>> {
    let base = site.base;
    if base % plan.page != 0 {
        return Err(BindError::BaseUnaligned { base, page: plan.page });
    }
    base.checked_add(plan.span).ok_or(BindError::Overflow { base, span: plan.span })?;
    if site.imports.len() != plan.imports.len() {
        return Err(BindError::ImportCount { want: plan.imports.len(), given: site.imports.len() });
    }
    if site.words.len() != plan.words as usize {
        return Err(BindError::WordCount { want: plan.words, given: site.words.len() });
    }
    // Every strong import first: nothing is placed while one is missing.
    for (w, got) in plan.imports.iter().zip(site.imports.iter()) {
        if w.lookup && !w.weak && got.is_none() {
            return Err(BindError::Unresolved { name: w.name });
        }
    }

    let mut stores: AVec<'a, Store> = Vec::try_with_capacity_in(plan.patches.len(), alloc).map_err(|_| BindError::Alloc)?;
    for p in plan.patches.iter() {
        let value = match p.cell {
            Cell::Rebase { target } => base + target,
            Cell::Import { index, addend } => {
                let i = index as usize;
                if plan.imports[i].lookup {
                    // A weak import no library defines is 0, whatever its addend (dyld, ld.so).
                    site.imports[i].map_or(0, |at| at.wrapping_add(addend as u64))
                } else {
                    site.tlv_thunk.ok_or(BindError::NoTlvThunk)?.wrapping_add(addend as u64)
                }
            }
            Cell::Word { index } => site.words[index as usize],
            Cell::TlvThunk => site.tlv_thunk.ok_or(BindError::NoTlvThunk)?,
            Cell::TlvKey => site.tlv_key.ok_or(BindError::NoTlvKey)?,
            Cell::TlsDescResolver => site.tlsdesc_resolver.ok_or(BindError::NoTlsDescResolver)?,
            Cell::TlsDescArg { offset } => offset,
        };
        push(&mut stores, Store { at: base + p.at, value })?;
    }

    let mut segments: AVec<'a, Segment> = Vec::new_in(alloc);
    for s in plan.segments.iter() {
        push(&mut segments, Segment { at: base + s.at, len: s.len, prot: s.prot })?;
    }
    let mut inits: AVec<'a, u64> = Vec::new_in(alloc);
    for &i in plan.inits.iter() {
        push(&mut inits, base + i)?;
    }
    let template = plan.template.map(|t| Template { init: moved(t.init, base), ..t });
    let unwind = Unwind {
        text: moved(plan.unwind.text, base),
        eh_frame: moved(plan.unwind.eh_frame, base),
        unwind_info: moved(plan.unwind.unwind_info, base),
    };
    Ok(Writes { base, span: plan.span, segments, copy: moved(plan.copy, base), stores, inits, template, unwind })
}

/// The absolute address of the image's export `name` (platform-mangled: `_f` on Mach-O) when
/// the plan is placed at `base`; `None` when the image does not define it.
pub fn export(plan: &LoadPlan<'_>, base: u64, name: &[u8]) -> Option<u64> {
    let k = plan.exports.binary_search_by(|&(n, _)| n.cmp(name)).ok()?;
    Some(base + plan.exports[k].1)
}
