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
//! The order of functions: a profile's hot functions first, in its order; then a depth-first
//! walk from the entry over each record's sites in code order and its words, so a caller sits
//! just before its first callee; then whatever is left (reached only through data) by symbol.
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
    fn relro(&self) -> &[u8];
    fn words(&self) -> usize;
    fn word(&self, i: usize) -> Word;
    fn target(&self, i: usize) -> Target;
}

impl LayoutMember for Member<'_> {
    fn symbol(&self) -> Symbol {
        self.symbol
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

/// The placement order: profile first, then a depth-first walk from the entry, then the rest by
/// symbol. A work list, no recursion.
fn order<'a>(members: &[Member<'_>], by_symbol: &[u32], entry: usize, profile: Option<Profile<'_>>, alloc: Alloc<'a>) -> AVec<'a, u32> {
    let n = members.len();
    // `placed`: in `order`; `walked`: its callees pushed. A profile's function is placed before
    // any walk and walked when the walk meets it.
    let mut placed: AVec<'_, bool> = Vec::from_elem_in(false, n, alloc);
    let mut walked: AVec<'_, bool> = Vec::from_elem_in(false, n, alloc);
    let mut order: AVec<'a, u32> = Vec::with_capacity_in(n, alloc);
    let hot = profile.map_or(&[][..], |p| p.hot);
    for &symbol in hot {
        if let Some(i) = find_member(members, by_symbol, symbol)
            && !placed[i]
        {
            placed[i] = true;
            order.push(i as u32);
        }
    }
    let mut stack: AVec<'_, u32> = Vec::new_in(alloc);
    let from_profile = hot.iter().filter_map(|&symbol| find_member(members, by_symbol, symbol));
    let roots = core::iter::once(entry).chain(from_profile).chain(by_symbol.iter().map(|&i| i as usize));
    for root in roots {
        stack.push(root as u32);
        while let Some(i) = stack.pop() {
            let i = i as usize;
            if walked[i] {
                continue;
            }
            walked[i] = true;
            if !placed[i] {
                placed[i] = true;
                order.push(i as u32);
            }
            let r = members[i].record;
            // Words first, so they pop after the sites; sites in reverse, so the first call in
            // the code is walked first.
            for w in (0..r.words()).rev() {
                let word = r.word(w);
                if word.target == OWN_CODE || word.target == OWN_RELRO {
                    continue;
                }
                let t = r.target(word.target as usize);
                if t.space == Space::Func
                    && let Some(j) = find_member(members, by_symbol, t.symbol)
                    && !walked[j]
                {
                    stack.push(j as u32);
                }
            }
            for s in (0..r.sites()).rev() {
                let site = r.site(s);
                if site.target == NO_TARGET {
                    continue;
                }
                let t = r.target(site.target as usize);
                if t.space == Space::Func
                    && let Some(j) = find_member(members, by_symbol, t.symbol)
                    && !walked[j]
                {
                    stack.push(j as u32);
                }
            }
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
    let order = order(members, &by_symbol, entry_member, profile, alloc);

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
