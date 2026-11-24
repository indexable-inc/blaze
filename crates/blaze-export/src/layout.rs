//! The whole program laid out once (run.md, "Export (writer, 2026-11-16)"): every reached
//! function's record placed in one code region, every call between them a direct `bl` and every
//! address of a function, static or vtable an `adrp; add`, so nothing in reach goes through a
//! slot, a stub or an import word. Only a C name is reached through a word (the GOT), which the
//! platform's loader binds.
//!
//! Regions, in image order, each starting on a page of the platform ([`Plan`]):
//!
//! | region | holds | after the loader |
//! |---|---|---|
//! | text | the headers' room (`plan.text_at` bytes), then the code, each record 16-aligned | read, execute |
//! | const | `plan.const_head` bytes the writer fills (ELF's dynamic section), the GOT, the records' relro parts, read-only data | read only |
//! | data | writable statics, then zeroed ones (no file bytes) | read, write |
//!
//! The order of functions: 16 KB Apple Silicon page-aware 3-tier BOLT / Ext-TSP / C³ layout
//! ([`bolt_order_members`], shared with `optimized.rs`):
//! 1. **Hot tier** (profile-weighted functions when a [`Profile`] is supplied, or direct-call
//!    reachable non-trap functions from `entry` when none is supplied), merged into <= 16 KB
//!    call-affinity chains and ranked by execution density (`weight / bytes`).
//! 2. **Warm tier** (remaining direct-call reachable non-trap functions when a profile is supplied,
//!    or code-loaded non-trap callbacks when none is), merged by 16 KB Ext-TSP call affinity.
//! 3. **Cold tail** (noreturn trap/panic stubs, their exclusive callees, and relro/vtable/data-only
//!    targets never reached by direct calls), packed after all hot and warm code.
//! Equal records (equal bytes: code, relro part, targets, names) are placed once.
//!
//! Every address here is an image offset: the distance from the image's first byte (Mach-O's
//! header, ELF's first load), which the loader slides as a whole. The layout's bytes depend on
//! nothing but its input: no map order, no machine.

use blaze_code::a64::{ADD64, ADRP, BLR, LDR64, NOP, add_lo12, adrp, bl, expect, ldr_lo12, up, write32};
use blaze_code::record::{CodeView, NO_TARGET, OWN_CODE, OWN_RELRO, SiteKind, Space, Symbol, Target, Word};
use blaze_code::tls::{CAP as TLS_CAP, DESC_VARS, ROW_BYTES as TLS_ROW_BYTES, ROW_INIT, descriptor as tls_descriptor, var_row as tls_var_row};
use blaze_ids::Hasher;
use blaze_own::{AVec, Alloc, Vec};

use crate::error::ExportError;
use crate::input::{Data, DataPlace, Profile, Tier};

const FOLD_DOMAIN: &str = "blaze-export fold 1";

/// The image may not reach past 4 GiB: `adrp` reaches ±4 GiB and a chained fixup's target is
/// 36 bits; one bound for both keeps every encoding exact.
const IMAGE_LIMIT: u64 = 1 << 32;

/// A function to place: one of the program's, or a start record the export made (`tier: None`).
#[derive(Clone, Copy)]
pub struct Member<'r> {
    pub symbol: Symbol,
    pub record: CodeView<'r>,
    pub tier: Option<Tier>,
}

/// Where the platform's file puts the regions: its page, where the code may start (after the
/// file's headers), and the bytes it keeps at the head of the const region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Plan {
    pub page: u64,
    pub text_at: u64,
    pub const_head: u64,
}

/// A function placed: its home, the length of its code, its tier (`None`: made by the export),
/// and whether an equal record placed earlier holds its code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placed {
    pub symbol: Symbol,
    pub at: u64,
    pub len: u64,
    pub tier: Option<Tier>,
    pub folded: bool,
}

/// What the loader writes into an 8-byte word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FixTo {
    /// The image's runtime address plus `target`, an image offset.
    Rebase { target: u64 },
    /// The address of import `import`'s C name.
    Bind { import: u32 },
}

/// One word the loader fixes, at image offset `at` (8-aligned, in the const or data region).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fixup {
    pub at: u64,
    pub to: FixTo,
}

/// A C name the program calls or holds the address of, bound by the loader.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Import<'r> {
    pub symbol: Symbol,
    /// The name as the C library spells it, without a platform's prefix.
    pub name: &'r str,
}

/// The program laid out: each region's bytes at its image offset, and what the loader fixes.
pub struct Laid<'a, 'r> {
    pub page: u64,
    /// The code: image offsets `[text_at, text_at + text.len())`.
    pub text_at: u64,
    pub text: AVec<'a, u8>,
    /// The const region: `[const_at, const_at + konst.len())`; its first `const_head` bytes zero
    /// for the writer to fill. Empty when nothing is in it and the head is empty.
    pub const_at: u64,
    pub konst: AVec<'a, u8>,
    /// The GOT: import `i`'s word at `got_at + 8 * i`.
    pub got_at: u64,
    /// The data region's file bytes: `[data_at, data_at + data.len())`.
    pub data_at: u64,
    pub data: AVec<'a, u8>,
    /// Zeroed statics: `[zero_at, zero_at + zero_len)`, no file bytes.
    pub zero_at: u64,
    pub zero_len: u64,
    /// The image's end, on a page.
    pub end: u64,
    /// Sorted by symbol.
    pub imports: AVec<'a, Import<'r>>,
    /// Sorted by `at`, one per word.
    pub fixups: AVec<'a, Fixup>,
    /// The image offset the loader enters.
    pub entry: u64,
    /// In placement order.
    pub placed: AVec<'a, Placed>,
    /// The bytes kept after the code for the unwind tables: what eh_out.rs `eh_room` measured
    /// (the layout's `tail`). `eh_sections` refuses tables past it in every build.
    pub tables_room: u64,
    /// When `Some(at)`, an 8-byte `__mod_init_func` slot in `konst` (macOS TLS startup
    /// initializer, `S_MOD_INIT_FUNC_POINTERS`).
    pub mod_init_at: Option<u64>,
    /// Whether the program has a Rust thread-local runtime (`TlsRuntime`): Linux emits `PT_TLS`
    /// for the per-thread `ROOT` slot.
    pub has_tls: bool,
    /// Number of Rust thread-local statics (`Space::Tls`) laid out in `DESC.vars`.
    pub tls_count: u32,
}

impl Laid<'_, '_> {
    /// The end of the code.
    pub fn text_end(&self) -> u64 {
        self.text_at + self.text.len() as u64
    }

    /// The end of the const region's bytes.
    pub fn const_end(&self) -> u64 {
        self.const_at + self.konst.len() as u64
    }

    /// The end of the data region's file bytes.
    pub fn data_end(&self) -> u64 {
        self.data_at + self.data.len() as u64
    }
}

/// Common view over `bzcode01` [`Member`] and `bzcode02` [`crate::optimized::Member02`] for
/// the layout stages shared by [`lay_out`] and [`crate::optimized::lay_out_records`].
pub(crate) trait LayoutMember {
    fn symbol(&self) -> Symbol;
    fn code(&self) -> &[u8];
    fn relro(&self) -> &[u8];
    fn words(&self) -> usize;
    fn word(&self, i: usize) -> Word;
    fn target(&self, i: usize) -> Target;
    fn traps(&self) -> usize;
    fn is_made_start(&self) -> bool;
    /// Invokes `f` once per direct call/tail-call site targeting an in-program `Space::Func`,
    /// in code offset order (preserving callsite multiplicity).
    fn for_each_direct_call(&self, f: impl FnMut(Symbol));
    /// Invokes `f` once per code-site address load targeting an in-program `Space::Func`,
    /// in code offset order.
    fn for_each_code_func_load(&self, f: impl FnMut(Symbol));
    /// Invokes `f` once per relro word / `Abs64` targeting an in-program `Space::Func`,
    /// in deterministic record order.
    fn for_each_relro_func_ref(&self, f: impl FnMut(Symbol));
}

impl LayoutMember for Member<'_> {
    fn symbol(&self) -> Symbol {
        self.symbol
    }
    fn code(&self) -> &[u8] {
        self.record.code()
    }
    fn relro(&self) -> &[u8] {
        self.record.relro()
    }
    fn words(&self) -> usize {
        self.record.words()
    }
    fn word(&self, i: usize) -> Word {
        self.record.word(i)
    }
    fn target(&self, i: usize) -> Target {
        self.record.target(i)
    }
    fn traps(&self) -> usize {
        self.record.traps()
    }
    fn is_made_start(&self) -> bool {
        self.tier.is_none()
    }
    fn for_each_direct_call(&self, mut f: impl FnMut(Symbol)) {
        for s in 0..self.record.sites() {
            let site = self.record.site(s);
            if site.kind == SiteKind::Call
                && site.target != NO_TARGET
                && let Ok(idx) = usize::try_from(site.target)
            {
                let t = self.record.target(idx);
                if t.space == Space::Func {
                    f(t.symbol);
                }
            }
        }
    }
    fn for_each_code_func_load(&self, mut f: impl FnMut(Symbol)) {
        for s in 0..self.record.sites() {
            let site = self.record.site(s);
            if site.kind == SiteKind::Load
                && site.target != NO_TARGET
                && let Ok(idx) = usize::try_from(site.target)
            {
                let t = self.record.target(idx);
                if t.space == Space::Func {
                    f(t.symbol);
                }
            }
        }
    }
    fn for_each_relro_func_ref(&self, mut f: impl FnMut(Symbol)) {
        for w in 0..self.record.words() {
            let word = self.record.word(w);
            if word.target != OWN_CODE
                && word.target != OWN_RELRO
                && let Ok(idx) = usize::try_from(word.target)
            {
                let t = self.record.target(idx);
                if t.space == Space::Func {
                    f(t.symbol);
                }
            }
        }
    }
}

/// Finds `symbol` in `members` through `by_symbol` (member indices sorted by symbol).
pub(crate) fn find_member<M: LayoutMember>(members: &[M], by_symbol: &[u32], symbol: Symbol) -> Option<usize> {
    by_symbol.binary_search_by(|&i| members[i as usize].symbol().cmp(&symbol)).ok().map(|k| by_symbol[k] as usize)
}

fn find_data(data: &[Data<'_>], by_target: &[u32], target: Target) -> Option<usize> {
    by_target.binary_search_by(|&i| data[i as usize].target.cmp(&target)).ok().map(|k| by_target[k] as usize)
}

/// Thread-local layout inputs when the program uses Rust thread-locals (`Space::Tls`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct TlsPlan {
    /// The `Space::Static` symbol of the 40-byte `Desc` synthesized in writable data.
    pub descriptor: Symbol,
    /// On macOS, the `tls_init` function symbol placed in `__DATA_CONST,__mod_init_func`.
    /// `None` on Linux (where the start wrapper calls `tls_init` directly).
    pub mod_init_fn: Option<Symbol>,
    /// Initial value of `DESC.root`: `0` on macOS (written by `tls_init` at startup as
    /// `key * 8`); `TLS_ROOT_OFFSET | ROOT_OWN` on Linux (`PT_TLS` static TLS slot).
    pub initial_root: u64,
}

/// Returns the image offset of an in-image function, static, or vtable target (including the
/// synthesized TLS descriptor static when `tls_desc` names it).
#[allow(clippy::too_many_arguments)]
pub(crate) fn target_home<M: LayoutMember>(
    members: &[M],
    by_symbol: &[u32],
    home: &[u64],
    data: &[Data<'_>],
    by_target: &[u32],
    data_home: &[u64],
    tls_desc: Option<(Symbol, u64)>,
    t: Target,
) -> Option<u64> {
    match t.space {
        Space::Func => find_member(members, by_symbol, t.symbol).map(|i| home[i]),
        Space::Static | Space::Vtable => {
            if t.space == Space::Static
                && let Some((sym, at)) = tls_desc
                && t.symbol == sym
            {
                return Some(at);
            }
            find_data(data, by_target, t).map(|d| data_home[d])
        }
        Space::Tls | Space::Outside => None,
    }
}

/// Checks that `target` falls within `[0, end]` and turns it into a [`FixTo::Rebase`].
pub(crate) fn rebase_target(from: Symbol, at: u64, target: i64, end: u64) -> Result<FixTo, ExportError> {
    // `<=`: a pointer one past a region's last byte (a slice's end) is an address too.
    match target >= 0 && (target as u64) <= end {
        true => Ok(FixTo::Rebase { target: target as u64 }),
        false => Err(ExportError::WordOutside { from, at }),
    }
}

/// The const and data regions laid out after the code, together with each member's relro offset,
/// each datum's image offset, and any synthesized Rust thread-local descriptor/index/row tables.
pub(crate) struct ConstAndData<'a> {
    pub(crate) const_at: u64,
    pub(crate) konst: AVec<'a, u8>,
    pub(crate) got_at: u64,
    pub(crate) mod_init_at: Option<u64>,
    pub(crate) tls_indices_at: u64,
    pub(crate) tls_rows_at: u64,
    pub(crate) tls_desc: Option<(Symbol, u64)>,
    /// Sorted by `Symbol`: `(symbol, datum_index)` for every `Space::Tls` datum; its position is
    /// the thread-local's 0-based index in `DESC.vars` and `tls_indices_at`.
    pub(crate) tls_targets: AVec<'a, (Symbol, u32)>,
    pub(crate) tls_plan: Option<TlsPlan>,
    pub(crate) relro_at: AVec<'a, u64>,
    pub(crate) data_home: AVec<'a, u64>,
    pub(crate) data_at: u64,
    pub(crate) data: AVec<'a, u8>,
    pub(crate) zero_at: u64,
    pub(crate) zero_end: u64,
    pub(crate) end: u64,
}

impl ConstAndData<'_> {
    /// Returns the 0-based index of thread-local `symbol`, or `None` if no `Space::Tls` datum
    /// places it.
    pub(crate) fn tls_index_of(&self, symbol: Symbol) -> Option<u32> {
        let idx = self.tls_targets.binary_search_by_key(&symbol, |&(s, _)| s).ok()?;
        u32::try_from(idx).ok()
    }

    /// Writes an 8-byte little-endian value directly into `konst` at image offset `at` (for
    /// relro words naming a `Space::Tls` index, which is an integer rather than a pointer).
    pub(crate) fn write_const_word(&mut self, from: Symbol, at: u64, value: u64) -> Result<(), ExportError> {
        let Some(rel) = at.checked_sub(self.const_at).and_then(|o| usize::try_from(o).ok()) else {
            return Err(ExportError::WordOutside { from, at });
        };
        let Some(slot) = rel.checked_add(8).and_then(|end| self.konst.get_mut(rel..end)) else {
            return Err(ExportError::WordOutside { from, at });
        };
        slot.copy_from_slice(&value.to_le_bytes());
        Ok(())
    }
}

/// Lays out the const region (writer's head, GOT, optional `__mod_init_func`, TLS index words,
/// TLS `VarRow` table, relro parts, read-only data & TLS templates) and the data region
/// (writable data, optional TLS `Desc`, then zeroed data), refusing images past [`IMAGE_LIMIT`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn lay_out_const_and_data<'a, M: LayoutMember>(
    members: &[M],
    order: &[u32],
    rep: &[u32],
    data: &[Data<'_>],
    by_target: &[u32],
    imports_len: usize,
    tls_plan: Option<TlsPlan>,
    text_end: u64,
    plan: Plan,
    tail: u64,
    alloc: Alloc<'a>,
) -> Result<ConstAndData<'a>, ExportError> {
    let n = members.len();
    let page = plan.page;
    // Gather all `Space::Tls` data in deterministic symbol order (already sorted in `by_target`).
    let mut tls_targets: AVec<'a, (Symbol, u32)> = Vec::new_in(alloc);
    for &d in by_target.iter() {
        if let Some(datum) = usize::try_from(d).ok().and_then(|idx| data.get(idx))
            && datum.target.space == Space::Tls
        {
            tls_targets.push((datum.target.symbol, d));
        }
    }
    let tls_count = u64::try_from(tls_targets.len()).unwrap_or(u64::MAX);
    if tls_count > TLS_CAP {
        return Err(ExportError::TooLarge { what: "the thread-local table's CAP (65536)", len: tls_count });
    }
    // The const region: the writer's head, the GOT, optional `__mod_init_func`, TLS index words,
    // TLS `VarRow` table, relro parts, read-only data and TLS templates.
    let const_at = up(text_end + tail, page);
    let mut konst: AVec<'a, u8> = Vec::new_in(alloc);
    konst.resize(plan.const_head as usize, 0);
    pad_to(&mut konst, const_at, 8);
    let got_at = const_at + konst.len() as u64;
    konst.resize(konst.len() + 8 * imports_len, 0);
    let mod_init_at = match tls_plan.and_then(|t| t.mod_init_fn) {
        Some(_) => {
            let at = const_at + u64::try_from(konst.len()).unwrap_or(u64::MAX);
            konst.resize(konst.len() + 8, 0);
            Some(at)
        }
        None => None,
    };
    // Keep `__const` (everything after `__got` and optional `__mod_init_func`) 16-byte aligned
    // when non-empty so Mach-O's `__const` section header (`align = 4`) matches its start address.
    let post_tables = konst.len();
    if !tls_targets.is_empty() {
        pad_to(&mut konst, const_at, 16);
    }
    let tls_indices_at = const_at + u64::try_from(konst.len()).unwrap_or(u64::MAX);
    for k in 0..tls_count {
        konst.extend_from_slice(&k.to_le_bytes());
    }
    let tls_rows_at = const_at + u64::try_from(konst.len()).unwrap_or(u64::MAX);
    let tls_rows_len = tls_targets.len().saturating_mul(TLS_ROW_BYTES);
    let tls_rows_offset = konst.len();
    konst.resize(konst.len() + tls_rows_len, 0);
    let mut relro_at: AVec<'a, u64> = Vec::from_elem_in(0, n, alloc);
    for (pos, &i) in order.iter().enumerate() {
        let r = rep[pos] as usize;
        if r != pos {
            relro_at[i as usize] = relro_at[order[r] as usize];
            continue;
        }
        let relro = members[i as usize].relro();
        if relro.is_empty() {
            continue;
        }
        pad_to(&mut konst, const_at, 16);
        relro_at[i as usize] = const_at + konst.len() as u64;
        konst.extend_from_slice(relro);
    }
    let mut data_home: AVec<'a, u64> = Vec::from_elem_in(0, data.len(), alloc);
    for &d in by_target.iter() {
        let Some(datum) = data.get(usize::try_from(d).unwrap_or(usize::MAX)) else {
            continue;
        };
        let in_const = match datum.target.space {
            Space::Tls => !matches!(datum.place, DataPlace::Zeroed { .. }) && !datum.bytes.is_empty(),
            Space::Static | Space::Vtable => datum.place == DataPlace::ReadOnly,
            Space::Func | Space::Outside => false,
        };
        if in_const {
            if konst.len() == post_tables {
                pad_to(&mut konst, const_at, 16);
            }
            pad_to(&mut konst, const_at, u64::from(datum.align));
            data_home[d as usize] = const_at + konst.len() as u64;
            konst.extend_from_slice(datum.bytes);
        }
    }
    // Populate each TLS `VarRow` (32 bytes: size, align, init=0 rebased in `finish`, init_len).
    for (k, &(_, d)) in tls_targets.iter().enumerate() {
        let Some(datum) = usize::try_from(d).ok().and_then(|idx| data.get(idx)) else {
            continue;
        };
        let (size, init_len) = match datum.place {
            DataPlace::Zeroed { len } => (len, 0u64),
            DataPlace::ReadOnly | DataPlace::Writable => {
                let len = u64::try_from(datum.bytes.len()).unwrap_or(u64::MAX);
                (len, len)
            }
        };
        // At least 8-byte alignment: macOS `aligned_alloc` refuses smaller alignments.
        let align = u64::from(datum.align).max(8);
        let row = tls_var_row(size, align, 0, init_len);
        let start = tls_rows_offset + k.saturating_mul(TLS_ROW_BYTES);
        if let Some(dst) = konst.get_mut(start..start + TLS_ROW_BYTES) {
            dst.copy_from_slice(&row);
        }
    }
    // The data region: writable data, optional TLS `Desc`, then zeroed data with no file bytes.
    let data_at = up(const_at + konst.len() as u64, page);
    let mut bytes: AVec<'a, u8> = Vec::new_in(alloc);
    for &d in by_target.iter() {
        let Some(datum) = data.get(usize::try_from(d).unwrap_or(usize::MAX)) else {
            continue;
        };
        if datum.target.space != Space::Tls && datum.place == DataPlace::Writable {
            pad_to(&mut bytes, data_at, u64::from(datum.align));
            data_home[d as usize] = data_at + bytes.len() as u64;
            bytes.extend_from_slice(datum.bytes);
        }
    }
    let tls_desc = match tls_plan {
        Some(t) => {
            pad_to(&mut bytes, data_at, 8);
            let at = data_at + u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            bytes.extend_from_slice(&tls_descriptor(0, t.initial_root, tls_count, tls_count, 0));
            Some((t.descriptor, at))
        }
        None => None,
    };
    let zero_at = up(data_at + bytes.len() as u64, 16);
    let mut zero_end = zero_at;
    for &d in by_target.iter() {
        let Some(datum) = data.get(usize::try_from(d).unwrap_or(usize::MAX)) else {
            continue;
        };
        if datum.target.space != Space::Tls
            && let DataPlace::Zeroed { len } = datum.place
        {
            zero_end = up(zero_end, u64::from(datum.align));
            data_home[d as usize] = zero_end;
            zero_end += len;
        }
    }
    let end = up(zero_end, page);
    if end > IMAGE_LIMIT {
        return Err(ExportError::TooLarge { what: "adrp's 4 GiB and a chained fixup's 36-bit target", len: end });
    }
    Ok(ConstAndData {
        const_at,
        konst,
        got_at,
        mod_init_at,
        tls_indices_at,
        tls_rows_at,
        tls_desc,
        tls_targets,
        tls_plan,
        relro_at,
        data_home,
        data_at,
        data: bytes,
        zero_at,
        zero_end,
        end,
    })
}

impl<'a> ConstAndData<'a> {
    /// Emits every placed record's word fixups, every datum's word fixups, and the TLS descriptor,
    /// `VarRow` `init` and `__mod_init_func` fixups into `fixups`, sorts them by offset, and
    /// assembles the final [`Laid`] image.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish<'r, M: LayoutMember>(
        self,
        members: &[M],
        by_symbol: &[u32],
        order: &[u32],
        rep: &[u32],
        home: &[u64],
        data: &[Data<'r>],
        by_target: &[u32],
        entry_member: usize,
        plan: Plan,
        tail: u64,
        text: AVec<'a, u8>,
        imports: AVec<'a, Import<'r>>,
        mut fixups: AVec<'a, Fixup>,
        placed: AVec<'a, Placed>,
    ) -> Result<Laid<'a, 'r>, ExportError> {
        let Self {
            const_at,
            mut konst,
            got_at,
            mod_init_at,
            tls_indices_at: _,
            tls_rows_at,
            tls_desc,
            tls_targets,
            tls_plan,
            relro_at,
            data_home,
            data_at,
            data: bytes,
            zero_at,
            zero_end,
            end,
        } = self;
        let import_of = |symbol: Symbol| imports.binary_search_by(|it| it.symbol.cmp(&symbol)).ok();
        let home_of = |t: Target| target_home(members, by_symbol, home, data, by_target, &data_home, tls_desc, t);
        let tls_index_of = |symbol: Symbol| -> Option<u32> {
            let idx = tls_targets.binary_search_by_key(&symbol, |&(s, _)| s).ok()?;
            u32::try_from(idx).ok()
        };
        // Emit `__mod_init_func` rebase (macOS TLS startup initializer).
        if let (Some(at), Some(init_sym)) = (mod_init_at, tls_plan.and_then(|t| t.mod_init_fn))
            && let Some(target) = home_of(Target { space: Space::Func, symbol: init_sym })
        {
            fixups.push(Fixup { at, to: FixTo::Rebase { target } });
        }
        // Emit `DESC.vars` and each `VarRow.init` rebase.
        if let Some((_, desc_at)) = tls_desc
            && !tls_targets.is_empty()
        {
            let desc_vars_off = u64::try_from(DESC_VARS).unwrap_or(0);
            fixups.push(Fixup { at: desc_at + desc_vars_off, to: FixTo::Rebase { target: tls_rows_at } });
            let row_init_off = u64::try_from(ROW_INIT).unwrap_or(0);
            let row_bytes = u64::try_from(TLS_ROW_BYTES).unwrap_or(0);
            for (k, &(_, d)) in tls_targets.iter().enumerate() {
                let row_at = tls_rows_at + u64::try_from(k).unwrap_or(0).saturating_mul(row_bytes);
                let target = usize::try_from(d)
                    .ok()
                    .and_then(|idx| data.get(idx).zip(data_home.get(idx)))
                    .map_or(desc_at, |(datum, &h)| {
                        if matches!(datum.place, DataPlace::Zeroed { .. }) || datum.bytes.is_empty() {
                            desc_at
                        } else {
                            h
                        }
                    });
                fixups.push(Fixup { at: row_at + row_init_off, to: FixTo::Rebase { target } });
            }
        }
        for (pos, &i) in order.iter().enumerate() {
            if rep[pos] as usize != pos {
                continue;
            }
            let m = &members[i as usize];
            let symbol = m.symbol();
            for w in 0..m.words() {
                let word = m.word(w);
                let at = relro_at[i as usize] + u64::from(word.at);
                let to = match word.target {
                    OWN_CODE => rebase_target(symbol, at, home[i as usize] as i64 + word.addend, end)?,
                    OWN_RELRO => rebase_target(symbol, at, relro_at[i as usize] as i64 + word.addend, end)?,
                    index => {
                        let t = m.target(index as usize);
                        match t.space {
                            Space::Tls => {
                                let Some(idx) = tls_index_of(t.symbol) else {
                                    return Err(ExportError::OutOfReach { from: symbol, target: t });
                                };
                                let value = u64::from(idx).wrapping_add_signed(word.addend);
                                let Some(rel) = at.checked_sub(const_at).and_then(|o| usize::try_from(o).ok()) else {
                                    return Err(ExportError::WordOutside { from: symbol, at });
                                };
                                let Some(slot) = rel.checked_add(8).and_then(|e| konst.get_mut(rel..e)) else {
                                    return Err(ExportError::WordOutside { from: symbol, at });
                                };
                                slot.copy_from_slice(&value.to_le_bytes());
                                continue;
                            }
                            Space::Outside if word.addend != 0 => return Err(ExportError::OutsideAddend { from: symbol, target: t }),
                            Space::Outside => match import_of(t.symbol) {
                                Some(import) => FixTo::Bind { import: import as u32 },
                                None => unreachable!("every outside target of a placed record was gathered above"),
                            },
                            Space::Func | Space::Static | Space::Vtable => match home_of(t) {
                                Some(to) => rebase_target(symbol, at, to as i64 + word.addend, end)?,
                                None => return Err(ExportError::OutOfReach { from: symbol, target: t }),
                            },
                        }
                    }
                };
                fixups.push(Fixup { at, to });
            }
        }
        for &d in by_target.iter() {
            let datum = &data[d as usize];
            let base = data_home[d as usize];
            for w in datum.words {
                let at = base + u64::from(w.at);
                let to = match w.target {
                    None if datum.target.space == Space::Tls => return Err(ExportError::DataTlsTarget { from: datum.target, target: datum.target }),
                    None => rebase_target(datum.target.symbol, at, base as i64 + w.addend, end)?,
                    Some(t) => match (t.space, import_of(t.symbol)) {
                        (Space::Tls, _) => return Err(ExportError::DataTlsTarget { from: datum.target, target: t }),
                        (Space::Outside, Some(import)) if w.addend == 0 => FixTo::Bind { import: import as u32 },
                        (Space::Func | Space::Static | Space::Vtable, _) => match home_of(t) {
                            Some(to) => rebase_target(datum.target.symbol, at, to as i64 + w.addend, end)?,
                            None => return Err(ExportError::DataOutOfReach { from: datum.target, target: t }),
                        },
                        _ => return Err(ExportError::DataOutOfReach { from: datum.target, target: t }),
                    },
                };
                fixups.push(Fixup { at, to });
            }
        }
        fixups.sort_unstable_by_key(|f| f.at);

        let entry_at = home[entry_member];
        let tls_count = u32::try_from(tls_targets.len()).unwrap_or(u32::MAX);
        let has_tls = tls_desc.is_some();
        drop(relro_at);
        drop(data_home);
        drop(tls_targets);
        Ok(Laid {
            page: plan.page,
            text_at: plan.text_at,
            text,
            const_at,
            konst,
            got_at,
            data_at,
            data: bytes,
            zero_at,
            zero_len: zero_end - zero_at,
            end,
            imports,
            fixups,
            entry: entry_at,
            placed,
            tables_room: tail,
            mod_init_at,
            has_tls,
            tls_count,
        })
    }
}

/// Pads `bytes` with zeros until `region_at + bytes.len()` is a multiple of `align`.
pub(crate) fn pad_to(bytes: &mut AVec<'_, u8>, region_at: u64, align: u64) {
    let len = up(region_at + bytes.len() as u64, align) - region_at;
    bytes.resize(len as usize, 0);
}

/// Checks every datum's form and returns their indices sorted by target.
pub(crate) fn check_data<'a>(data: &[Data<'_>], tls_desc: Option<Symbol>, page: u64, alloc: Alloc<'a>) -> Result<AVec<'a, u32>, ExportError> {
    let mut by_target: AVec<'a, u32> = Vec::with_capacity_in(data.len(), alloc);
    for (i, d) in data.iter().enumerate() {
        if !matches!(d.target.space, Space::Static | Space::Vtable | Space::Tls) {
            return Err(ExportError::DataSpace { target: d.target });
        }
        if d.target.space == Space::Tls && tls_desc.is_none() {
            return Err(ExportError::NoTlsRuntime { target: d.target });
        }
        if let Some(desc) = tls_desc
            && d.target == (Target { space: Space::Static, symbol: desc })
        {
            return Err(ExportError::TwoData { target: d.target });
        }
        if !d.align.is_power_of_two() || u64::from(d.align) > page {
            return Err(ExportError::DataAlign { target: d.target, align: d.align });
        }
        if matches!(d.place, DataPlace::Zeroed { .. }) && (!d.bytes.is_empty() || !d.words.is_empty()) {
            return Err(ExportError::ZeroedWithBytes { target: d.target });
        }
        for w in d.words {
            if w.at % 8 != 0 || w.at as usize + 8 > d.bytes.len() {
                return Err(ExportError::DataWordOutside { target: d.target, at: w.at });
            }
        }
        by_target.push(i as u32);
    }
    by_target.sort_unstable_by(|&a, &b| data[a as usize].target.cmp(&data[b as usize].target));
    for pair in by_target.windows(2) {
        if data[pair[0] as usize].target == data[pair[1] as usize].target {
            return Err(ExportError::TwoData { target: data[pair[0] as usize].target });
        }
    }
    Ok(by_target)
}

/// Apple Silicon virtual-memory / iTLB page size (`hw.pagesize == 16384`).
const APPLE_PAGE_BYTES: u64 = 16_384;
/// Minimum cluster budget for 16 KB-page-aware Ext-TSP / C³ chain merging.
const MIN_CLUSTER_PAGE_BYTES: u64 = 4_096;
/// Code alignment between placed functions in `__text` (16-byte AArch64 fetch-group alignment).
const CODE_ALIGN_BYTES: u64 = 16;

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn at_bool(s: &[bool], i: usize) -> bool {
    s.get(i).copied().unwrap_or(false)
}

#[inline]
fn at_u32(s: &[u32], i: usize) -> u32 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u64(s: &[u64], i: usize) -> u64 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_usize(s: &[usize], i: usize) -> usize {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn align_up_u64(offset: u64, align: u64) -> u64 {
    let a = align.max(1);
    let rem = offset % a;
    if rem == 0 { offset } else { offset.saturating_add(a - rem) }
}

/// Finds `symbol`'s canonical index in `0..by_symbol.len()` (invariant under permutations of
/// `members`, Rule 25).
fn find_sym_id<M: LayoutMember>(members: &[M], by_symbol: &[u32], symbol: Symbol) -> Option<usize> {
    by_symbol
        .binary_search_by(|&raw| {
            members
                .get(to_idx(raw))
                .map_or(core::cmp::Ordering::Greater, |m| m.symbol().cmp(&symbol))
        })
        .ok()
}

/// Returns `true` when `m` contains a trap (`traps() > 0` or an AArch64 `brk` instruction) and has
/// no normal return (`ret`/`retaa`/`retab`) or tail-call branch (`b`/`br`), identifying noreturn
/// panic/trap stubs that belong in the cold tail.
fn is_trap_stub<M: LayoutMember>(m: &M) -> bool {
    let code = m.code();
    if code.len() < 4 || code.len() % 4 != 0 {
        return false;
    }
    let mut has_brk = m.traps() > 0;
    let mut has_return_or_tail = false;
    for chunk in code.chunks_exact(4) {
        let Ok(bytes) = <[u8; 4]>::try_from(chunk) else {
            continue;
        };
        let inst = u32::from_le_bytes(bytes);
        // `ret` / `ret xN` (`0xD65F_0000` mask `0xFFFF_FC1F`), `retaa` / `retab`
        // (`0xD65F_0BFF` / `0xD65F_0FFF`), unconditional tail branch `b imm26`
        // (`0x1400_0000` mask `0xFC00_0000`), indirect tail branch `br xN`
        // (`0xD61F_0000` mask `0xFFFF_FC1F`).
        if (inst & 0xFFFF_FC1F) == 0xD65F_0000
            || (inst & 0xFFFF_FBFF) == 0xD65F_0BFF
            || (inst & 0xFC00_0000) == 0x1400_0000
            || (inst & 0xFFFF_FC1F) == 0xD61F_0000
        {
            has_return_or_tail = true;
        }
        // `brk #imm16` (`0xD420_0000` mask `0xFFE0_001F`).
        if (inst & 0xFFE0_001F) == 0xD420_0000 {
            has_brk = true;
        }
    }
    has_brk && !has_return_or_tail
}

#[derive(Clone, Copy)]
struct RawCallEdge {
    to: u32,
    mult: u32,
    first_site: u32,
}

struct BoltChain<'a> {
    nodes: AVec<'a, usize>,
    total_bytes: u64,
    total_weight: u64,
    min_rank: u32,
    alive: bool,
}

#[derive(Clone, Copy)]
struct TierEdge {
    u_slot: usize,
    v_slot: usize,
    u_sym: usize,
    v_sym: usize,
    weight: u64,
    callee_dfs: u32,
    caller_dfs: u32,
    first_site: u32,
}

/// 16 KB-page-aware Ext-TSP / C³ greedy chain merging over one tier (`tier_nodes`), running in
/// `O(E log E)` with a hard `page_cap` (`<= 16384` bytes) merge budget so callers and their hottest
/// callees pack into the fewest Apple Silicon 16 KB iTLB pages. All tie-breakers use canonical
/// symbol order (Rule 25).
#[allow(clippy::too_many_arguments)]
fn pack_tier_ext_tsp<'a>(
    tier_nodes: &[usize],
    fn_bytes: &[u64],
    node_weight: &[u64],
    node_rank: &[u32],
    dfs_pre: &[u32],
    call_head: &[usize],
    call_edges: &[RawCallEdge],
    use_profile_edge_weight: bool,
    page_cap: u64,
    out: &mut AVec<'a, usize>,
    alloc: Alloc<'a>,
) {
    let t_len = tier_nodes.len();
    if t_len == 0 {
        return;
    }
    if t_len == 1 {
        if let Some(&sym_id) = tier_nodes.first() {
            out.push(sym_id);
        }
        return;
    }

    let n = fn_bytes.len();
    let mut slot_of: AVec<'_, usize> = Vec::from_elem_in(usize::MAX, n, alloc);
    let mut chains: AVec<'_, BoltChain<'a>> = Vec::with_capacity_in(t_len, alloc);
    let mut chain_of: AVec<'_, usize> = Vec::with_capacity_in(t_len, alloc);
    let mut offset_in_chain: AVec<'_, u64> = Vec::from_elem_in(0, t_len, alloc);

    for (slot, &sym_id) in tier_nodes.iter().enumerate() {
        set_at(&mut slot_of, sym_id, slot);
        let bytes = at_u64(fn_bytes, sym_id).max(CODE_ALIGN_BYTES);
        let weight = at_u64(node_weight, sym_id).max(1);
        let rank = at_u32(node_rank, sym_id);
        let mut nodes: AVec<'a, usize> = Vec::with_capacity_in(1, alloc);
        nodes.push(slot);
        chains.push(BoltChain { nodes, total_bytes: bytes, total_weight: weight, min_rank: rank, alive: true });
        chain_of.push(slot);
    }

    // Gather directed edges whose endpoints both live in this tier.
    let mut edges: AVec<'_, TierEdge> = Vec::new_in(alloc);
    for (u_slot, &u_sym) in tier_nodes.iter().enumerate() {
        let start = at_usize(call_head, u_sym);
        let end = at_usize(call_head, u_sym.saturating_add(1));
        let Some(slice) = call_edges.get(start..end) else {
            continue;
        };
        let u_rank_w = u64::try_from(t_len.saturating_sub(usize::try_from(at_u32(node_rank, u_sym)).unwrap_or(t_len))).unwrap_or(1).max(1);
        let caller_dfs = at_u32(dfs_pre, u_sym);
        for edge in slice {
            let v_sym = to_idx(edge.to);
            if v_sym == u_sym {
                continue;
            }
            let v_slot = at_usize(&slot_of, v_sym);
            if v_slot >= t_len {
                continue;
            }
            let mult = u64::from(edge.mult).max(1);
            let weight = if use_profile_edge_weight {
                let v_rank_w = u64::try_from(t_len.saturating_sub(usize::try_from(at_u32(node_rank, v_sym)).unwrap_or(t_len))).unwrap_or(1).max(1);
                u_rank_w.saturating_mul(mult).saturating_mul(64).saturating_add(v_rank_w)
            } else {
                let v_bonus = at_u64(node_weight, v_sym).min(63);
                mult.saturating_mul(64).saturating_add(v_bonus)
            };
            edges.push(TierEdge {
                u_slot,
                v_slot,
                u_sym,
                v_sym,
                weight,
                callee_dfs: at_u32(dfs_pre, v_sym),
                caller_dfs,
                first_site: edge.first_site,
            });
        }
    }

    // Sort edges once in `O(E log E)`: heaviest call edges first; equal-weight ties follow DFS
    // callee preorder (preserving caller-before-first-callee on simple trees) and canonical symbol
    // ids (Rule 25).
    edges.sort_unstable_by(|a, b| {
        b.weight
            .cmp(&a.weight)
            .then_with(|| a.callee_dfs.cmp(&b.callee_dfs))
            .then_with(|| a.caller_dfs.cmp(&b.caller_dfs))
            .then_with(|| a.first_site.cmp(&b.first_site))
            .then_with(|| a.u_sym.cmp(&b.u_sym))
            .then_with(|| a.v_sym.cmp(&b.v_sym))
    });

    // Single-pass C³ / Ext-TSP greedy chain merge bounded by `page_cap` (16 KB on Apple Silicon).
    let max_merges = t_len.saturating_sub(1);
    let mut merges = 0usize;
    for e in edges.iter() {
        if merges >= max_merges {
            break;
        }
        let ca = at_usize(&chain_of, e.u_slot);
        let cb = at_usize(&chain_of, e.v_slot);
        if ca == cb {
            continue;
        }
        let (Some(chain_a), Some(chain_b)) = (chains.get(ca), chains.get(cb)) else {
            continue;
        };
        if !chain_a.alive || !chain_b.alive {
            continue;
        }
        if chain_a.total_bytes.saturating_add(chain_b.total_bytes) > page_cap {
            continue;
        }

        // Choose concatenation order:
        // - When `v` is the head of `cb` (`off_v == 0`), append `cb` after `ca` (`ca ++ cb`),
        //   keeping callers before their primary callees for sequential L1i prefetch.
        // - When `v` is already inside `cb` behind a hotter caller (`off_v > 0`), append `ca`
        //   after `cb` (`cb ++ ca`) so the hotter chain stays intact at the front of the 16 KB
        //   page while `u` still lands adjacent to `v` on the same page.
        let off_v = at_u64(&offset_in_chain, e.v_slot);
        let (dst_c, src_c) = if off_v == 0 { (ca, cb) } else { (cb, ca) };

        let (src_nodes, src_bytes, src_weight, src_min_rank) = if let Some(sc) = chains.get_mut(src_c) {
            sc.alive = false;
            let moved = core::mem::replace(&mut sc.nodes, Vec::new_in(alloc));
            (moved, sc.total_bytes, sc.total_weight, sc.min_rank)
        } else {
            continue;
        };
        if let Some(dc) = chains.get_mut(dst_c) {
            let base_off = dc.total_bytes;
            for &node_slot in src_nodes.iter() {
                set_at(&mut chain_of, node_slot, dst_c);
                let prev_off = at_u64(&offset_in_chain, node_slot);
                set_at(&mut offset_in_chain, node_slot, base_off.saturating_add(prev_off));
                dc.nodes.push(node_slot);
            }
            dc.total_bytes = dc.total_bytes.saturating_add(src_bytes);
            dc.total_weight = dc.total_weight.saturating_add(src_weight);
            dc.min_rank = dc.min_rank.min(src_min_rank);
            merges = merges.saturating_add(1);
        }
    }

    // Order surviving chains by descending execution density (`total_weight / total_bytes`, exact
    // `u128` cross-multiplication), then descending `total_weight`, then ascending `min_rank`,
    // then ascending first node's canonical symbol id (Rule 25).
    let mut alive_chains: AVec<'_, usize> = Vec::with_capacity_in(t_len, alloc);
    for (ci, ch) in chains.iter().enumerate() {
        if ch.alive && !ch.nodes.is_empty() {
            alive_chains.push(ci);
        }
    }
    alive_chains.sort_unstable_by(|&a, &b| {
        let (Some(ca), Some(cb)) = (chains.get(a), chains.get(b)) else {
            return a.cmp(&b);
        };
        let lhs = u128::from(cb.total_weight).saturating_mul(u128::from(ca.total_bytes.max(1)));
        let rhs = u128::from(ca.total_weight).saturating_mul(u128::from(cb.total_bytes.max(1)));
        lhs.cmp(&rhs)
            .then_with(|| cb.total_weight.cmp(&ca.total_weight))
            .then_with(|| ca.min_rank.cmp(&cb.min_rank))
            .then_with(|| {
                let fa = ca.nodes.first().copied().and_then(|s| tier_nodes.get(s).copied()).unwrap_or(usize::MAX);
                let fb = cb.nodes.first().copied().and_then(|s| tier_nodes.get(s).copied()).unwrap_or(usize::MAX);
                fa.cmp(&fb)
            })
    });

    for &ci in alive_chains.iter() {
        let Some(ch) = chains.get(ci) else {
            continue;
        };
        for &slot in ch.nodes.iter() {
            if let Some(&sym_id) = tier_nodes.get(slot) {
                out.push(sym_id);
            }
        }
    }
}

/// 16 KB Apple Silicon BOLT / Ext-TSP / C³ function ordering shared by [`lay_out`] (`bzcode01`)
/// and [`crate::optimized::lay_out_records`] (`bzcode02`).
///
/// Partitions functions into three tiers — **Hot**, **Warm**, and **Cold tail** — applies
/// 16 KB-page-aware Ext-TSP / C³ greedy chain merging inside Hot and Warm, and packs cold trap
/// stubs and indirect/relro/vtable-only targets into the cold tail. Every internal index is in
/// canonical symbol space (`0..by_symbol.len()`), guaranteeing identical placement regardless of
/// input `members` permutation (Rule 25).
pub(crate) fn bolt_order_members<'a, M: LayoutMember>(
    members: &[M],
    by_symbol: &[u32],
    entry: usize,
    profile: Option<Profile<'_>>,
    page: u64,
    alloc: Alloc<'a>,
) -> AVec<'a, u32> {
    let n = members.len();
    if n == 0 {
        return Vec::new_in(alloc);
    }
    let page_cap = page.clamp(MIN_CLUSTER_PAGE_BYTES, APPLE_PAGE_BYTES);
    let entry_sym_id = members
        .get(entry)
        .and_then(|m| find_sym_id(members, by_symbol, m.symbol()))
        .unwrap_or(0);

    // 1. Build per-function canonical metadata and flat CSR direct call graph in `0..n` symbol
    //    order so nothing below ever observes the raw input slice permutation.
    let mut fn_bytes: AVec<'_, u64> = Vec::with_capacity_in(n, alloc);
    let mut trap_stub: AVec<'_, bool> = Vec::with_capacity_in(n, alloc);
    let mut is_start: AVec<'_, bool> = Vec::with_capacity_in(n, alloc);
    let mut call_head: AVec<'_, usize> = Vec::with_capacity_in(n.saturating_add(1), alloc);
    let mut call_edges: AVec<'_, RawCallEdge> = Vec::new_in(alloc);
    let mut in_calls: AVec<'_, u64> = Vec::from_elem_in(0, n, alloc);

    for sym_id in 0..n {
        call_head.push(call_edges.len());
        let Some(m) = members.get(to_idx(at_u32(by_symbol, sym_id))) else {
            fn_bytes.push(CODE_ALIGN_BYTES);
            trap_stub.push(false);
            is_start.push(false);
            continue;
        };
        let code_len = u64::try_from(m.code().len()).unwrap_or(u64::MAX);
        fn_bytes.push(align_up_u64(code_len, CODE_ALIGN_BYTES).max(CODE_ALIGN_BYTES));
        let start_flag = m.is_made_start();
        is_start.push(start_flag);
        let stub_flag = sym_id != entry_sym_id && !start_flag && is_trap_stub(m);
        trap_stub.push(stub_flag);

        let edge_start = call_edges.len();
        let mut site_ord = 0u32;
        m.for_each_direct_call(|target_sym| {
            if let Some(to_id) = find_sym_id(members, by_symbol, target_sym)
                && to_id != sym_id
            {
                call_edges.push(RawCallEdge { to: to_u32(to_id), mult: 1, first_site: site_ord });
            }
            site_ord = site_ord.saturating_add(1);
        });
        // Coalesce duplicate edges `(sym_id -> to)` while preserving callsite multiplicity and
        // earliest callsite index, then restore code order (`first_site`).
        if let Some(slice) = call_edges.get_mut(edge_start..) {
            slice.sort_unstable_by(|a, b| a.to.cmp(&b.to).then_with(|| a.first_site.cmp(&b.first_site)));
            let mut w = 0usize;
            for r in 0..slice.len() {
                let Some(cur) = slice.get(r).copied() else {
                    break;
                };
                let merged = if w > 0
                    && let Some(prev) = slice.get_mut(w - 1)
                    && prev.to == cur.to
                {
                    prev.mult = prev.mult.saturating_add(cur.mult);
                    true
                } else {
                    false
                };
                if !merged {
                    set_at(slice, w, cur);
                    w = w.saturating_add(1);
                }
            }
            if let Some(dedup) = slice.get_mut(..w) {
                dedup.sort_unstable_by(|a, b| a.first_site.cmp(&b.first_site).then_with(|| a.to.cmp(&b.to)));
            }
            call_edges.truncate(edge_start.saturating_add(w));
        }
        if !stub_flag
            && let Some(slice) = call_edges.get(edge_start..)
        {
            for edge in slice {
                let dst = to_idx(edge.to);
                let prev = at_u64(&in_calls, dst);
                set_at(&mut in_calls, dst, prev.saturating_add(u64::from(edge.mult)));
            }
        }
    }
    call_head.push(call_edges.len());

    // 2. Gather profile hot symbols (deduplicated, preserving hottest-first rank).
    let hot = profile.map_or(&[][..], |p| p.hot);
    let mut hot_rank: AVec<'_, u32> = Vec::from_elem_in(u32::MAX, n, alloc);
    let mut profile_hot_nodes: AVec<'_, usize> = Vec::with_capacity_in(hot.len().min(n), alloc);
    for (rank, &sym) in hot.iter().enumerate() {
        if let Some(sym_id) = find_sym_id(members, by_symbol, sym)
            && at_u32(&hot_rank, sym_id) == u32::MAX
        {
            set_at(&mut hot_rank, sym_id, to_u32(rank));
            profile_hot_nodes.push(sym_id);
        }
    }
    let has_profile_hot = !profile_hot_nodes.is_empty();

    // 3. Direct-call reachability from `entry` (and profile hot roots when present), never
    //    propagating through noreturn trap/panic stubs so panic formatting callees sink to Cold.
    let mut direct_reach: AVec<'_, bool> = Vec::from_elem_in(false, n, alloc);
    let mut bfs_queue: AVec<'_, usize> = Vec::with_capacity_in(n, alloc);
    let enqueue_direct = |sym_id: usize, direct_reach: &mut [bool], bfs_queue: &mut AVec<'a, usize>| {
        if sym_id < n && !at_bool(direct_reach, sym_id) {
            set_at(direct_reach, sym_id, true);
            bfs_queue.push(sym_id);
        }
    };
    enqueue_direct(entry_sym_id, &mut direct_reach, &mut bfs_queue);
    for &u in profile_hot_nodes.iter() {
        enqueue_direct(u, &mut direct_reach, &mut bfs_queue);
    }
    let mut head = 0usize;
    while head < bfs_queue.len() {
        let u = at_usize(&bfs_queue, head);
        head = head.saturating_add(1);
        let start = at_usize(&call_head, u);
        let end = at_usize(&call_head, u.saturating_add(1));
        if let Some(slice) = call_edges.get(start..end) {
            for edge in slice {
                let v = to_idx(edge.to);
                if !at_bool(&trap_stub, v) || at_u32(&hot_rank, v) != u32::MAX {
                    enqueue_direct(v, &mut direct_reach, &mut bfs_queue);
                }
            }
        }
        // Start records (`_start` on Linux, `LangStart` wrapper) pass `main` via code-site `Load`.
        if (u == entry_sym_id || at_bool(&is_start, u))
            && let Some(m) = members.get(to_idx(at_u32(by_symbol, u)))
        {
            m.for_each_code_func_load(|target_sym| {
                if let Some(v) = find_sym_id(members, by_symbol, target_sym)
                    && !at_bool(&trap_stub, v)
                {
                    enqueue_direct(v, &mut direct_reach, &mut bfs_queue);
                }
            });
        }
    }

    // 4. Secondary code-load reachability (non-trap callbacks taken by address in direct-reachable
    //    code, plus their non-trap direct callees) for the Warm tier when no profile is present.
    let mut code_load_reach: AVec<'_, bool> = Vec::from_elem_in(false, n, alloc);
    if !has_profile_hot {
        let mut warm_q: AVec<'_, usize> = Vec::new_in(alloc);
        for &u in bfs_queue.iter() {
            if let Some(m) = members.get(to_idx(at_u32(by_symbol, u))) {
                m.for_each_code_func_load(|target_sym| {
                    if let Some(v) = find_sym_id(members, by_symbol, target_sym)
                        && !at_bool(&direct_reach, v)
                        && !at_bool(&trap_stub, v)
                        && !at_bool(&code_load_reach, v)
                    {
                        set_at(&mut code_load_reach, v, true);
                        warm_q.push(v);
                    }
                });
            }
        }
        let mut wh = 0usize;
        while wh < warm_q.len() {
            let u = at_usize(&warm_q, wh);
            wh = wh.saturating_add(1);
            let start = at_usize(&call_head, u);
            let end = at_usize(&call_head, u.saturating_add(1));
            if let Some(slice) = call_edges.get(start..end) {
                for edge in slice {
                    let v = to_idx(edge.to);
                    if !at_bool(&direct_reach, v) && !at_bool(&trap_stub, v) && !at_bool(&code_load_reach, v) {
                        set_at(&mut code_load_reach, v, true);
                        warm_q.push(v);
                    }
                }
            }
        }
    }

    // 5. Canonical iterative DFS preorder (`dfs_pre`) over direct calls so equal-weight tie-breaks
    //    in Ext-TSP preserve caller-before-first-callee locality on simple trees.
    let mut dfs_pre: AVec<'_, u32> = Vec::from_elem_in(u32::MAX, n, alloc);
    let mut dfs_stack: AVec<'_, usize> = Vec::with_capacity_in(n, alloc);
    let mut next_pre = 0u32;
    let mut dfs_seeds: AVec<'_, usize> = Vec::with_capacity_in(n.saturating_add(1), alloc);
    dfs_seeds.push(entry_sym_id);
    for &u in profile_hot_nodes.iter() {
        dfs_seeds.push(u);
    }
    for u in 0..n {
        dfs_seeds.push(u);
    }
    for &seed in dfs_seeds.iter() {
        if at_u32(&dfs_pre, seed) != u32::MAX {
            continue;
        }
        dfs_stack.push(seed);
        while let Some(u) = dfs_stack.pop() {
            if at_u32(&dfs_pre, u) != u32::MAX {
                continue;
            }
            set_at(&mut dfs_pre, u, next_pre);
            next_pre = next_pre.saturating_add(1);
            // Push start-wrapper loads first (so direct calls pop ahead of them), then direct calls
            // in reverse code order so the first callee in code pops first.
            if (u == entry_sym_id || at_bool(&is_start, u))
                && let Some(m) = members.get(to_idx(at_u32(by_symbol, u)))
            {
                let mut loaded: AVec<'_, usize> = Vec::new_in(alloc);
                m.for_each_code_func_load(|target_sym| {
                    if let Some(v) = find_sym_id(members, by_symbol, target_sym)
                        && at_u32(&dfs_pre, v) == u32::MAX
                    {
                        loaded.push(v);
                    }
                });
                for &v in loaded.iter().rev() {
                    dfs_stack.push(v);
                }
            }
            let start = at_usize(&call_head, u);
            let end = at_usize(&call_head, u.saturating_add(1));
            if let Some(slice) = call_edges.get(start..end) {
                for edge in slice.iter().rev() {
                    let v = to_idx(edge.to);
                    if at_u32(&dfs_pre, v) == u32::MAX {
                        dfs_stack.push(v);
                    }
                }
            }
        }
    }

    // 6. Partition every canonical node into Hot / Warm / Cold and assign per-node weights & ranks.
    let mut hot_tier: AVec<'_, usize> = Vec::new_in(alloc);
    let mut warm_tier: AVec<'_, usize> = Vec::new_in(alloc);
    let mut cold_tier: AVec<'_, usize> = Vec::new_in(alloc);
    let mut node_weight: AVec<'_, u64> = Vec::from_elem_in(1, n, alloc);
    let mut node_rank: AVec<'_, u32> = Vec::from_elem_in(u32::MAX, n, alloc);

    if has_profile_hot {
        let h_len = profile_hot_nodes.len();
        for (pos, &u) in profile_hot_nodes.iter().enumerate() {
            hot_tier.push(u);
            let w = u64::try_from(h_len.saturating_sub(pos)).unwrap_or(1).max(1);
            set_at(&mut node_weight, u, w.saturating_mul(w));
            set_at(&mut node_rank, u, to_u32(pos));
        }
        for u in 0..n {
            if at_u32(&hot_rank, u) != u32::MAX {
                continue;
            }
            if at_bool(&direct_reach, u) && !at_bool(&trap_stub, u) {
                warm_tier.push(u);
                let w = 1u64.saturating_add(at_u64(&in_calls, u).saturating_mul(4));
                set_at(&mut node_weight, u, w);
                set_at(&mut node_rank, u, at_u32(&dfs_pre, u));
            } else {
                cold_tier.push(u);
                set_at(&mut node_rank, u, at_u32(&dfs_pre, u));
            }
        }
    } else {
        for u in 0..n {
            if at_bool(&direct_reach, u) && !at_bool(&trap_stub, u) {
                hot_tier.push(u);
                let w = 1u64.saturating_add(at_u64(&in_calls, u).saturating_mul(4));
                set_at(&mut node_weight, u, w);
                set_at(&mut node_rank, u, at_u32(&dfs_pre, u));
            } else if at_bool(&code_load_reach, u) && !at_bool(&trap_stub, u) {
                warm_tier.push(u);
                let w = 1u64.saturating_add(at_u64(&in_calls, u).saturating_mul(4));
                set_at(&mut node_weight, u, w);
                set_at(&mut node_rank, u, at_u32(&dfs_pre, u));
            } else {
                cold_tier.push(u);
                set_at(&mut node_rank, u, at_u32(&dfs_pre, u));
            }
        }
        hot_tier.sort_unstable_by(|&a, &b| at_u32(&dfs_pre, a).cmp(&at_u32(&dfs_pre, b)).then_with(|| a.cmp(&b)));
    }
    warm_tier.sort_unstable_by(|&a, &b| at_u32(&dfs_pre, a).cmp(&at_u32(&dfs_pre, b)).then_with(|| a.cmp(&b)));

    // 7. Pack Hot and Warm tiers via 16 KB-page-aware Ext-TSP / C³ greedy chain merging.
    let mut canonical_order: AVec<'_, usize> = Vec::with_capacity_in(n, alloc);
    pack_tier_ext_tsp(
        &hot_tier,
        &fn_bytes,
        &node_weight,
        &node_rank,
        &dfs_pre,
        &call_head,
        &call_edges,
        has_profile_hot,
        page_cap,
        &mut canonical_order,
        alloc,
    );
    pack_tier_ext_tsp(
        &warm_tier,
        &fn_bytes,
        &node_weight,
        &node_rank,
        &dfs_pre,
        &call_head,
        &call_edges,
        false,
        page_cap,
        &mut canonical_order,
        alloc,
    );

    // 8. Emit Cold tail deterministically: first cold functions referenced by Hot/Warm (in
    //    iterative DFS affinity order over calls/loads/relro refs), then any remaining cold
    //    functions (e.g., data/vtable-only targets) in canonical symbol order.
    if !cold_tier.is_empty() {
        let mut emitted: AVec<'_, bool> = Vec::from_elem_in(false, n, alloc);
        for &u in canonical_order.iter() {
            set_at(&mut emitted, u, true);
        }
        let mut cold_roots: AVec<'_, usize> = Vec::with_capacity_in(cold_tier.len(), alloc);
        let placed_hot_warm = canonical_order.len();
        for idx in 0..placed_hot_warm {
            let u = at_usize(&canonical_order, idx);
            let start = at_usize(&call_head, u);
            let end = at_usize(&call_head, u.saturating_add(1));
            if let Some(slice) = call_edges.get(start..end) {
                for edge in slice {
                    let v = to_idx(edge.to);
                    if !at_bool(&emitted, v) {
                        cold_roots.push(v);
                    }
                }
            }
            if let Some(m) = members.get(to_idx(at_u32(by_symbol, u))) {
                m.for_each_code_func_load(|target_sym| {
                    if let Some(v) = find_sym_id(members, by_symbol, target_sym)
                        && !at_bool(&emitted, v)
                    {
                        cold_roots.push(v);
                    }
                });
                m.for_each_relro_func_ref(|target_sym| {
                    if let Some(v) = find_sym_id(members, by_symbol, target_sym)
                        && !at_bool(&emitted, v)
                    {
                        cold_roots.push(v);
                    }
                });
            }
        }
        for &u in cold_tier.iter() {
            cold_roots.push(u);
        }

        let mut cold_stack: AVec<'_, usize> = Vec::with_capacity_in(cold_tier.len(), alloc);
        let mut scratch_refs: AVec<'_, usize> = Vec::new_in(alloc);
        for &root in cold_roots.iter() {
            if at_bool(&emitted, root) {
                continue;
            }
            cold_stack.push(root);
            while let Some(u) = cold_stack.pop() {
                if at_bool(&emitted, u) {
                    continue;
                }
                set_at(&mut emitted, u, true);
                canonical_order.push(u);
                // Push relro refs and code loads first, then direct calls in reverse so direct
                // callees pop first.
                scratch_refs.clear();
                if let Some(m) = members.get(to_idx(at_u32(by_symbol, u))) {
                    m.for_each_relro_func_ref(|target_sym| {
                        if let Some(v) = find_sym_id(members, by_symbol, target_sym)
                            && !at_bool(&emitted, v)
                        {
                            scratch_refs.push(v);
                        }
                    });
                    m.for_each_code_func_load(|target_sym| {
                        if let Some(v) = find_sym_id(members, by_symbol, target_sym)
                            && !at_bool(&emitted, v)
                        {
                            scratch_refs.push(v);
                        }
                    });
                }
                for &v in scratch_refs.iter().rev() {
                    cold_stack.push(v);
                }
                let start = at_usize(&call_head, u);
                let end = at_usize(&call_head, u.saturating_add(1));
                if let Some(slice) = call_edges.get(start..end) {
                    for edge in slice.iter().rev() {
                        let v = to_idx(edge.to);
                        if !at_bool(&emitted, v) {
                            cold_stack.push(v);
                        }
                    }
                }
            }
        }
    }

    // 9. Translate canonical symbol indices back into original `members` indices.
    let mut order: AVec<'a, u32> = Vec::with_capacity_in(n, alloc);
    for &sym_id in canonical_order.iter() {
        if let Some(&raw_idx) = by_symbol.get(sym_id) {
            order.push(raw_idx);
        }
    }
    order
}

/// Lays the program out. `members` holds every function (the program's and the start's), in any
/// order; `entry` is the one the loader enters.
#[allow(clippy::too_many_arguments)]
pub fn lay_out<'a, 'r>(
    members: &[Member<'r>],
    data: &[Data<'r>],
    entry: Symbol,
    profile: Option<Profile<'_>>,
    plan: Plan,
    // Bytes kept after the code, before the const region's page: the unwind tables
    // (eh_out.rs `eh_room` measures them; `eh_sections` places them there).
    tail: u64,
    alloc: Alloc<'a>,
) -> Result<Laid<'a, 'r>, ExportError> {
    lay_out_with_tls(members, data, entry, profile, plan, tail, None, alloc)
}

/// [`lay_out`] with an optional Rust thread-local runtime plan ([`TlsPlan`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn lay_out_with_tls<'a, 'r>(
    members: &[Member<'r>],
    data: &[Data<'r>],
    entry: Symbol,
    profile: Option<Profile<'_>>,
    plan: Plan,
    tail: u64,
    tls_plan: Option<TlsPlan>,
    alloc: Alloc<'a>,
) -> Result<Laid<'a, 'r>, ExportError> {
    let n = members.len();
    let page = plan.page;
    let mut by_symbol: AVec<'_, u32> = Vec::with_capacity_in(n, alloc);
    for (i, member) in members.iter().enumerate() {
        if member.record.positions() != 0 {
            return Err(blaze_code::image::ImageError::UnresolvedPosition { symbol: member.symbol, row: member.record.position(0) }.into());
        }
        by_symbol.push(i as u32);
    }
    by_symbol.sort_unstable_by(|&a, &b| members[a as usize].symbol.cmp(&members[b as usize].symbol));
    for pair in by_symbol.windows(2) {
        if members[pair[0] as usize].symbol == members[pair[1] as usize].symbol {
            return Err(ExportError::TwoRecords { symbol: members[pair[0] as usize].symbol });
        }
    }
    let by_target = check_data(data, tls_plan.map(|t| t.descriptor), page, alloc)?;
    let Some(entry_member) = find_member(members, &by_symbol, entry) else {
        return Err(ExportError::NoEntry { symbol: entry });
    };
    let order = bolt_order_members(members, &by_symbol, entry_member, profile, page, alloc);

    // Folding: each position's representative is the first position with equal bytes.
    let mut digests: AVec<'_, ([u8; 32], u32)> = Vec::with_capacity_in(n, alloc);
    for (pos, &i) in order.iter().enumerate() {
        let bytes = members[i as usize].record.bytes();
        digests.push((Hasher::new(FOLD_DOMAIN).update(bytes).finish().0, pos as u32));
    }
    digests.sort_unstable();
    let mut rep: AVec<'_, u32> = Vec::from_elem_in(0, n, alloc);
    let mut run = 0usize;
    for k in 0..digests.len() {
        let (digest, pos) = digests[k];
        if digests[run].0 != digest {
            run = k;
        }
        let first = digests[run].1;
        let equal = members[order[first as usize] as usize].record.bytes() == members[order[pos as usize] as usize].record.bytes();
        rep[pos as usize] = if equal { first } else { pos };
    }
    drop(digests);

    // The code, in order; `home[member]` is its function's image offset.
    let mut home: AVec<'_, u64> = Vec::from_elem_in(0, n, alloc);
    let mut text: AVec<'a, u8> = Vec::new_in(alloc);
    let mut placed: AVec<'a, Placed> = Vec::with_capacity_in(n, alloc);
    for (pos, &i) in order.iter().enumerate() {
        let m = &members[i as usize];
        let r = rep[pos] as usize;
        let folded = r != pos;
        if folded {
            home[i as usize] = home[order[r] as usize];
        } else {
            pad_to(&mut text, plan.text_at, 16);
            home[i as usize] = plan.text_at + text.len() as u64;
            text.extend_from_slice(m.record.code());
        }
        let len = m.record.code().len() as u64;
        placed.push(Placed { symbol: m.symbol, at: home[i as usize], len, tier: m.tier, folded });
    }
    let text_end = plan.text_at + text.len() as u64;

    // Every target a placed record names: C names gathered, thread-locals checked for runtime.
    let mut imports: AVec<'a, Import<'r>> = Vec::new_in(alloc);
    for (pos, &i) in order.iter().enumerate() {
        if rep[pos] as usize != pos {
            continue;
        }
        let m = &members[i as usize];
        for k in 0..m.record.targets() {
            let t = m.record.target(k);
            match t.space {
                Space::Tls => {
                    if tls_plan.is_none() {
                        return Err(ExportError::NoTlsRuntime { target: t });
                    }
                    if find_data(data, &by_target, t).is_none() {
                        return Err(ExportError::OutOfReach { from: m.symbol, target: t });
                    }
                }
                Space::Outside => {
                    let Some(name) = m.record.name(t.symbol) else {
                        return Err(ExportError::OutsideNameMissing { from: m.symbol, target: t });
                    };
                    imports.push(Import { symbol: t.symbol, name });
                }
                Space::Func | Space::Static | Space::Vtable => {}
            }
        }
    }
    for &d in by_target.iter() {
        let Some(datum) = data.get(usize::try_from(d).unwrap_or(usize::MAX)) else {
            continue;
        };
        for w in datum.words {
            if let Some(t) = w.target
                && t.space == Space::Outside
                && let Some(&(_, name)) = datum.outside.iter().find(|&&(sym, _)| sym == t.symbol)
            {
                imports.push(Import { symbol: t.symbol, name });
            }
        }
    }
    imports.sort_unstable_by(|a, b| a.symbol.cmp(&b.symbol));
    imports.dedup_by(|a, b| a.symbol == b.symbol);
    let import_of = |symbol: Symbol| imports.binary_search_by(|it| it.symbol.cmp(&symbol)).ok();

    let regions = lay_out_const_and_data(members, &order, &rep, data, &by_target, imports.len(), tls_plan, text_end, plan, tail, alloc)?;
    let home_of = |t: Target| target_home(members, &by_symbol, &home, data, &by_target, &regions.data_home, regions.tls_desc, t);

    // Complete every placed record's sites.
    for (pos, &i) in order.iter().enumerate() {
        if rep[pos] as usize != pos {
            continue;
        }
        let m = &members[i as usize];
        let symbol = m.symbol;
        let at_home = home[i as usize];
        for s in 0..m.record.sites() {
            let site = m.record.site(s);
            let pc = at_home + u64::from(site.at);
            let x = pc - plan.text_at;
            if site.kind == SiteKind::Relro {
                let to = regions.relro_at[i as usize] + u64::from(site.addend);
                let first = expect(&text, x, ADRP, symbol, "adrp")?;
                let second = expect(&text, x + 4, ADD64, symbol, "add")?;
                write32(&mut text, x, adrp(first, pc, to, symbol)?);
                write32(&mut text, x + 4, add_lo12(second, to));
                continue;
            }
            let t = m.record.target(site.target as usize);
            let first = expect(&text, x, ADRP, symbol, "adrp")?;
            let second = expect(&text, x + 4, LDR64, symbol, "ldr")?;
            if site.kind == SiteKind::Call {
                expect(&text, x + 8, BLR, symbol, "blr")?;
            }
            match t.space {
                Space::Tls => {
                    if site.kind == SiteKind::Call {
                        return Err(ExportError::CallOfData { from: symbol, at: site.at });
                    }
                    let Some(index) = regions.tls_index_of(t.symbol) else {
                        return Err(ExportError::OutOfReach { from: symbol, target: t });
                    };
                    let word = regions.tls_indices_at + 8 * u64::from(index);
                    write32(&mut text, x, adrp(first, pc, word, symbol)?);
                    write32(&mut text, x + 4, ldr_lo12(second, word));
                }
                Space::Outside => {
                    let Some(index) = import_of(t.symbol) else {
                        unreachable!("every outside target of a placed record was gathered above");
                    };
                    let word = regions.got_at + 8 * index as u64;
                    write32(&mut text, x, adrp(first, pc, word, symbol)?);
                    write32(&mut text, x + 4, ldr_lo12(second, word));
                }
                Space::Func | Space::Static | Space::Vtable => {
                    let Some(to) = home_of(t) else {
                        return Err(ExportError::OutOfReach { from: symbol, target: t });
                    };
                    match site.kind {
                        SiteKind::Call if t.space != Space::Func => {
                            return Err(ExportError::CallOfData { from: symbol, at: site.at });
                        }
                        SiteKind::Call => {
                            write32(&mut text, x, NOP);
                            write32(&mut text, x + 4, NOP);
                            write32(&mut text, x + 8, bl(pc + 8, to, symbol)?);
                        }
                        _ => {
                            write32(&mut text, x, adrp(first, pc, to, symbol)?);
                            write32(&mut text, x + 4, add_lo12(second, to));
                        }
                    }
                }
            }
        }
    }

    // Every word the loader fixes: the GOT's, the relro parts', the data's.
    let mut fixups: AVec<'a, Fixup> = Vec::new_in(alloc);
    for k in 0..imports.len() {
        fixups.push(Fixup { at: regions.got_at + 8 * k as u64, to: FixTo::Bind { import: k as u32 } });
    }
    regions.finish(members, &by_symbol, &order, &rep, &home, data, &by_target, entry_member, plan, tail, text, imports, fixups, placed)
}

#[cfg(test)]
mod bolt_tests {
    use blaze_code::record::{CodeRecord, CodeView, Site, SiteKind, Space, Symbol, Target, Word};
    use blaze_own::{AVec, Alloc, Vec};
    use blaze_tier::record::{Declare, DeclaredKind, Declares, read as read_v2};

    use super::{APPLE_PAGE_BYTES, Member, lay_out};
    use crate::input::{Profile, Tier};
    use crate::optimized::{Member02, lay_out_records, v1_to_v2};
    use crate::{elf, macho};

    const STP_FP_LR: u32 = 0xA9BF_7BFD;
    const MOV_FP_SP: u32 = 0x9100_03FD;
    const LDP_FP_LR: u32 = 0xA8C1_7BFD;
    const RET: u32 = 0xD65F_03C0;
    const ADRP_X16: u32 = 0x9000_0010;
    const LDR_X16: u32 = 0xF940_0210;
    const BLR_X16: u32 = 0xD63F_0200;
    const ADRP_X0: u32 = 0x9000_0000;
    const LDR_X0: u32 = 0xF940_0000;
    const BRK_1: u32 = 0xD420_0020;
    const NOP: u32 = 0xD503_201F;

    fn sym(name: &str) -> Symbol {
        Symbol::of_name(name)
    }

    fn func(name: &str) -> Target {
        Target { space: Space::Func, symbol: sym(name) }
    }

    fn mov_w0(imm: u16) -> u32 {
        0x5280_0000 | (u32::from(imm) << 5)
    }

    struct TestDeclares;

    impl Declares for TestDeclares {
        fn declared(&self, t: Target) -> Option<Declare<'_>> {
            match t.space {
                Space::Func => Some(Declare { kind: DeclaredKind::Instance, item: 1, name: b"fn" }),
                Space::Static => Some(Declare { kind: DeclaredKind::Static, item: 1, name: b"" }),
                Space::Vtable => Some(Declare { kind: DeclaredKind::Vtable, item: 1, name: b"vt" }),
                Space::Tls => Some(Declare { kind: DeclaredKind::ThreadLocal, item: 1, name: b"" }),
                Space::Outside => None,
            }
        }
    }

    /// Builds a distinct sealed `bzcode01` record with direct calls, code loads, relro function
    /// pointer words, optional noreturn `brk` trap epilogue, and `NOP` padding to `target_bytes`.
    #[allow(clippy::too_many_arguments)]
    fn make_fn<'a>(
        tag: u16,
        calls: &[&str],
        loads: &[&str],
        relro_refs: &[&str],
        trap_stub: bool,
        target_bytes: usize,
        alloc: Alloc<'a>,
    ) -> AVec<'a, u8> {
        let mut r = CodeRecord::new_in(alloc);
        let push_inst = |code: &mut AVec<'a, u8>, inst: u32| {
            code.extend_from_slice(&inst.to_le_bytes());
        };
        push_inst(&mut r.code, STP_FP_LR);
        push_inst(&mut r.code, MOV_FP_SP);
        push_inst(&mut r.code, mov_w0(tag));
        for &callee in calls {
            let at = u32::try_from(r.code.len()).unwrap();
            push_inst(&mut r.code, ADRP_X16);
            push_inst(&mut r.code, LDR_X16);
            push_inst(&mut r.code, BLR_X16);
            let target = r.target(func(callee));
            r.sites.push(Site { at, kind: SiteKind::Call, target, addend: 0 });
        }
        for &loaded in loads {
            let at = u32::try_from(r.code.len()).unwrap();
            push_inst(&mut r.code, ADRP_X0);
            push_inst(&mut r.code, LDR_X0);
            let target = r.target(func(loaded));
            r.sites.push(Site { at, kind: SiteKind::Load, target, addend: 0 });
        }
        let epilogue_bytes = if trap_stub { 4 } else { 8 };
        while r.code.len() + epilogue_bytes < target_bytes {
            push_inst(&mut r.code, NOP);
        }
        if trap_stub {
            push_inst(&mut r.code, BRK_1);
        } else {
            push_inst(&mut r.code, LDP_FP_LR);
            push_inst(&mut r.code, RET);
        }
        if !relro_refs.is_empty() {
            r.relro.resize(relro_refs.len() * 8, 0);
            for (k, &name) in relro_refs.iter().enumerate() {
                let target = r.target(func(name));
                r.words.push(Word { at: u32::try_from(k * 8).unwrap(), target, addend: 0 });
            }
        }
        let mut out = Vec::new_in(alloc);
        r.seal().unwrap().encode(&mut out);
        out
    }

    struct SynthGraph<'a> {
        names: AVec<'a, &'static str>,
        records: AVec<'a, AVec<'a, u8>>,
    }

    /// Builds a 20-function synthetic call graph:
    /// - `entry` (128 B): calls `cold_panic_0` FIRST, then `hot_loop`.
    /// - 6 hot functions (`hot_loop`, `hot_a..hot_e`, 1_536 B each -> 9 KiB total <= 16 KB page):
    ///   `hot_loop` calls `cold_panic_1` FIRST, references 4 cold vtable fns in relro, then calls
    ///   `hot_a` (3x), `hot_b` (3x), `hot_c` (2x); `hot_a` calls `cold_panic_2` FIRST then `hot_d`
    ///   (3x); `hot_b` calls `hot_e` (2x).
    /// - 3 noreturn panic trap stubs (`cold_panic_0`, `cold_panic_1`, `cold_panic_2`) calling
    ///   `cold_fmt_0 -> cold_fmt_1 -> cold_fmt_2`, `cold_err_0 -> cold_err_1`, and `cold_err_2`
    ///   (16 KiB per cold chain, scattering naive DFS hot functions across 3 distinct 16 KB pages).
    /// - 4 relro/vtable-only cold functions (`cold_vtable_0..3`, 4_096 B each).
    fn synth_20_graph<'a>(alloc: Alloc<'a>) -> SynthGraph<'a> {
        let mut names: AVec<'a, &'static str> = Vec::with_capacity_in(20, alloc);
        let mut records: AVec<'a, AVec<'a, u8>> = Vec::with_capacity_in(20, alloc);
        let mut add = |name: &'static str, tag: u16, calls: &[&str], loads: &[&str], relro: &[&str], trap: bool, bytes: usize| {
            names.push(name);
            records.push(make_fn(tag, calls, loads, relro, trap, bytes, alloc));
        };

        // Interleave hot and cold definitions in the input slice.
        add("entry", 1, &["cold_panic_0", "hot_loop"], &[], &[], false, 128);
        add("cold_panic_0", 2, &["cold_fmt_0"], &[], &[], true, 4_096);
        add("cold_fmt_0", 3, &["cold_fmt_1"], &[], &[], false, 4_096);
        add("cold_vtable_0", 4, &[], &[], &[], false, 4_096);
        add(
            "hot_loop",
            5,
            &["cold_panic_1", "hot_a", "hot_a", "hot_a", "hot_b", "hot_b", "hot_b", "hot_c", "hot_c"],
            &[],
            &["cold_vtable_0", "cold_vtable_1", "cold_vtable_2", "cold_vtable_3"],
            false,
            1_536,
        );
        add("cold_fmt_1", 6, &["cold_fmt_2"], &[], &[], false, 4_096);
        add("cold_vtable_1", 7, &[], &[], &[], false, 4_096);
        add("hot_a", 8, &["cold_panic_2", "hot_d", "hot_d", "hot_d"], &[], &[], false, 1_536);
        add("cold_fmt_2", 9, &[], &[], &[], false, 4_096);
        add("cold_panic_1", 10, &["cold_err_0"], &[], &[], true, 4_096);
        add("cold_vtable_2", 11, &[], &[], &[], false, 4_096);
        add("hot_b", 12, &["hot_e", "hot_e"], &[], &[], false, 1_536);
        add("cold_err_0", 13, &["cold_err_1"], &[], &[], false, 6_144);
        add("cold_vtable_3", 14, &[], &[], &[], false, 4_096);
        add("hot_c", 15, &[], &[], &[], false, 1_536);
        add("cold_err_1", 16, &[], &[], &[], false, 6_144);
        add("hot_d", 17, &[], &[], &[], false, 1_536);
        add("cold_panic_2", 18, &["cold_err_2"], &[], &[], true, 8_192);
        add("cold_err_2", 19, &[], &[], &[], false, 8_192);
        add("hot_e", 20, &[], &[], &[], false, 1_536);

        assert_eq!(names.len(), 20);
        SynthGraph { names, records }
    }

    fn members_of<'a, 'r>(graph: &'r SynthGraph<'a>, perm: &[usize], alloc: Alloc<'a>) -> AVec<'a, Member<'r>> {
        let mut out: AVec<'a, Member<'r>> = Vec::with_capacity_in(perm.len(), alloc);
        for &i in perm {
            out.push(Member {
                symbol: sym(graph.names[i]),
                record: CodeView::read(&graph.records[i]).unwrap(),
                tier: Some(Tier::Fast),
            });
        }
        out
    }

    /// Naive DFS walk from `entry` (pre-BOLT baseline) to measure how many 16 KB pages the hot
    /// call graph would span without 16 KB page packing.
    fn naive_dfs_pages(members: &[Member<'_>], entry: Symbol, hot_syms: &[Symbol], text_at: u64, alloc: Alloc<'_>) -> usize {
        let n = members.len();
        let find = |s: Symbol| members.iter().position(|m| m.symbol == s);
        let mut visited: AVec<'_, bool> = Vec::from_elem_in(false, n, alloc);
        let mut order: AVec<'_, usize> = Vec::with_capacity_in(n, alloc);
        let mut stack: AVec<'_, usize> = Vec::new_in(alloc);
        let entry_idx = find(entry).unwrap();
        for root in core::iter::once(entry_idx).chain(0..n) {
            if visited[root] {
                continue;
            }
            stack.push(root);
            while let Some(i) = stack.pop() {
                if visited[i] {
                    continue;
                }
                visited[i] = true;
                order.push(i);
                let r = members[i].record;
                for w in (0..r.words()).rev() {
                    let word = r.word(w);
                    if word.target != super::OWN_CODE && word.target != super::OWN_RELRO {
                        let t = r.target(usize::try_from(word.target).unwrap());
                        if t.space == Space::Func
                            && let Some(j) = find(t.symbol)
                            && !visited[j]
                        {
                            stack.push(j);
                        }
                    }
                }
                for s in (0..r.sites()).rev() {
                    let site = r.site(s);
                    if site.target != super::NO_TARGET {
                        let t = r.target(usize::try_from(site.target).unwrap());
                        if t.space == Space::Func
                            && let Some(j) = find(t.symbol)
                            && !visited[j]
                        {
                            stack.push(j);
                        }
                    }
                }
            }
        }
        let mut cur = text_at;
        let mut pages: AVec<'_, u64> = Vec::new_in(alloc);
        for &i in order.iter() {
            cur = super::align_up_u64(cur, 16);
            let start = cur;
            let len = u64::try_from(members[i].record.code().len()).unwrap();
            let end = start + len.saturating_sub(1);
            if hot_syms.contains(&members[i].symbol) {
                for pg in (start / APPLE_PAGE_BYTES)..=(end / APPLE_PAGE_BYTES) {
                    if !pages.contains(&pg) {
                        pages.push(pg);
                    }
                }
            }
            cur = start + len;
        }
        pages.len()
    }

    fn hot_pages_of(placed: &[super::Placed], hot_syms: &[Symbol], alloc: Alloc<'_>) -> usize {
        let mut pages: AVec<'_, u64> = Vec::new_in(alloc);
        for p in placed {
            if hot_syms.contains(&p.symbol) {
                let end = p.at + p.len.saturating_sub(1);
                for pg in (p.at / APPLE_PAGE_BYTES)..=(end / APPLE_PAGE_BYTES) {
                    if !pages.contains(&pg) {
                        pages.push(pg);
                    }
                }
            }
        }
        pages.len()
    }

    #[test]
    fn bolt_order_is_deterministic_across_input_permutations() {
        let alloc = blaze_own::testing::heap().alloc();
        let graph = synth_20_graph(alloc);
        let id_perm: [usize; 20] = core::array::from_fn(|i| i);
        let rev_perm: [usize; 20] = core::array::from_fn(|i| 19 - i);
        let rot_perm: [usize; 20] = core::array::from_fn(|i| (i * 7 + 3) % 20);
        let hot = [sym("hot_loop"), sym("hot_a"), sym("hot_b"), sym("hot_c"), sym("hot_d"), sym("hot_e")];
        let profile = Profile { digest: [0x42; 32], hot: &hot };

        for prof in [None, Some(profile)] {
            let m0 = members_of(&graph, &id_perm, alloc);
            let m1 = members_of(&graph, &rev_perm, alloc);
            let m2 = members_of(&graph, &rot_perm, alloc);
            let l0 = lay_out(&m0, &[], sym("entry"), prof, macho::plan(), 0, alloc).unwrap();
            let l1 = lay_out(&m1, &[], sym("entry"), prof, macho::plan(), 0, alloc).unwrap();
            let l2 = lay_out(&m2, &[], sym("entry"), prof, macho::plan(), 0, alloc).unwrap();
            assert_eq!(l0.placed.as_slice(), l1.placed.as_slice(), "reversed input permutation must yield identical placement");
            assert_eq!(l0.placed.as_slice(), l2.placed.as_slice(), "rotated input permutation must yield identical placement");
            assert_eq!(l0.text.as_slice(), l1.text.as_slice(), "reversed input permutation must yield identical text bytes");
            assert_eq!(l0.text.as_slice(), l2.text.as_slice(), "rotated input permutation must yield identical text bytes");
            assert_eq!(l0.konst.as_slice(), l1.konst.as_slice());
            assert_eq!(l0.fixups.as_slice(), l2.fixups.as_slice());
        }
    }

    #[test]
    fn bolt_packs_hot_call_graph_into_fewer_16kb_pages_and_sinks_cold_tail() {
        let alloc = blaze_own::testing::heap().alloc();
        let graph = synth_20_graph(alloc);
        let perm: [usize; 20] = core::array::from_fn(|i| i);
        let members = members_of(&graph, &perm, alloc);
        let plan = macho::plan();

        let hot_syms = [sym("hot_loop"), sym("hot_a"), sym("hot_b"), sym("hot_c"), sym("hot_d"), sym("hot_e")];
        let cold_syms = [
            sym("cold_panic_0"),
            sym("cold_fmt_0"),
            sym("cold_fmt_1"),
            sym("cold_fmt_2"),
            sym("cold_panic_1"),
            sym("cold_err_0"),
            sym("cold_err_1"),
            sym("cold_panic_2"),
            sym("cold_err_2"),
            sym("cold_vtable_0"),
            sym("cold_vtable_1"),
            sym("cold_vtable_2"),
            sym("cold_vtable_3"),
        ];

        let baseline_pages = naive_dfs_pages(&members, sym("entry"), &hot_syms, plan.text_at, alloc);
        assert!(baseline_pages >= 3, "naive DFS scatters hot call graph across >= 3 16KB pages (got {baseline_pages})");

        let profile = Profile { digest: [0x99; 32], hot: &hot_syms };
        for prof in [None, Some(profile)] {
            let laid = lay_out(&members, &[], sym("entry"), prof, plan, 0, alloc).unwrap();
            assert_eq!(laid.placed.len(), 20);
            let packed_pages = hot_pages_of(&laid.placed, &hot_syms, alloc);
            assert_eq!(packed_pages, 1, "BOLT 16KB packing must fit all 6 hot functions onto 1 single 16KB page");
            assert!(
                packed_pages < baseline_pages,
                "BOLT packing ({packed_pages} pages) must strictly beat naive DFS ({baseline_pages} pages)"
            );

            // Every hot function ends strictly before every cold function begins.
            let max_hot_end = laid
                .placed
                .iter()
                .filter(|p| hot_syms.contains(&p.symbol))
                .map(|p| p.at + p.len)
                .max()
                .unwrap();
            let min_cold_start = laid
                .placed
                .iter()
                .filter(|p| cold_syms.contains(&p.symbol))
                .map(|p| p.at)
                .min()
                .unwrap();
            assert!(
                max_hot_end <= min_cold_start,
                "every cold function ({min_cold_start:#x}) must sink to the cold tail after all hot functions ({max_hot_end:#x})"
            );
        }
    }

    #[test]
    fn bolt_layout_and_optimized_agree_byte_for_byte() {
        let alloc = blaze_own::testing::heap().alloc();
        let graph = synth_20_graph(alloc);
        let perm: [usize; 20] = core::array::from_fn(|i| i);
        let v1_members = members_of(&graph, &perm, alloc);

        let mut v2_bytes: AVec<'_, AVec<'_, u8>> = Vec::with_capacity_in(v1_members.len(), alloc);
        for m in v1_members.iter() {
            let mut buf = Vec::new_in(alloc);
            v1_to_v2(m.record, &TestDeclares, &mut buf, alloc).unwrap();
            v2_bytes.push(buf);
        }
        let mut v2_members: AVec<'_, Member02<'_>> = Vec::with_capacity_in(v1_members.len(), alloc);
        for (k, m) in v1_members.iter().enumerate() {
            v2_members.push(Member02 { symbol: m.symbol, record: read_v2(&v2_bytes[k]).unwrap(), tier: m.tier });
        }

        let hot = [sym("hot_loop"), sym("hot_a"), sym("hot_b"), sym("hot_c"), sym("hot_d"), sym("hot_e")];
        let profile = Profile { digest: [0xAB; 32], hot: &hot };

        for plan in [macho::plan(), elf::plan()] {
            for prof in [None, Some(profile)] {
                let laid_v1 = lay_out(&v1_members, &[], sym("entry"), prof, plan, 0, alloc).unwrap();
                let laid_v2 = lay_out_records(&v2_members, &[], sym("entry"), prof, plan, 0, alloc).unwrap();
                assert_eq!(laid_v1.placed.as_slice(), laid_v2.placed.as_slice());
                assert_eq!(laid_v1.text.as_slice(), laid_v2.text.as_slice());
                assert_eq!(laid_v1.konst.as_slice(), laid_v2.konst.as_slice());
                assert_eq!(laid_v1.data.as_slice(), laid_v2.data.as_slice());
                assert_eq!(laid_v1.fixups.as_slice(), laid_v2.fixups.as_slice());
                assert_eq!(laid_v1.entry, laid_v2.entry);
                assert_eq!(laid_v1.const_at, laid_v2.const_at);
                assert_eq!(laid_v1.data_at, laid_v2.data_at);
                assert_eq!(laid_v1.end, laid_v2.end);
            }
        }
    }
}
