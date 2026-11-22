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
use blaze_code::record::{CodeView, NO_TARGET, OWN_CODE, OWN_RELRO, SiteKind, Space, Symbol, Target};
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

/// Finds `symbol` in `members` through `by_symbol` (member indices sorted by symbol).
fn find(members: &[Member<'_>], by_symbol: &[u32], symbol: Symbol) -> Option<usize> {
    by_symbol.binary_search_by(|&i| members[i as usize].symbol.cmp(&symbol)).ok().map(|k| by_symbol[k] as usize)
}

pub(crate) fn find_data(data: &[Data<'_>], by_target: &[u32], target: Target) -> Option<usize> {
    by_target.binary_search_by(|&i| data[i as usize].target.cmp(&target)).ok().map(|k| by_target[k] as usize)
}

/// Pads `bytes` with zeros until `region_at + bytes.len()` is a multiple of `align`.
pub(crate) fn pad_to(bytes: &mut AVec<'_, u8>, region_at: u64, align: u64) {
    let len = up(region_at + bytes.len() as u64, align) - region_at;
    bytes.resize(len as usize, 0);
}

/// Checks every datum's form and returns their indices sorted by target.
pub(crate) fn check_data<'a>(data: &[Data<'_>], page: u64, alloc: Alloc<'a>) -> Result<AVec<'a, u32>, ExportError> {
    let mut by_target: AVec<'a, u32> = Vec::with_capacity_in(data.len(), alloc);
    for (i, d) in data.iter().enumerate() {
        if !matches!(d.target.space, Space::Static | Space::Vtable) {
            return Err(ExportError::DataSpace { target: d.target });
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
        if let Some(i) = find(members, by_symbol, symbol)
            && !placed[i]
        {
            placed[i] = true;
            order.push(i as u32);
        }
    }
    let mut stack: AVec<'_, u32> = Vec::new_in(alloc);
    let from_profile = hot.iter().filter_map(|&symbol| find(members, by_symbol, symbol));
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
                    && let Some(j) = find(members, by_symbol, t.symbol)
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
                    && let Some(j) = find(members, by_symbol, t.symbol)
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
    let by_target = check_data(data, page, alloc)?;
    let Some(entry_member) = find(members, &by_symbol, entry) else {
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

    // Every target a placed record names: thread-locals refused, C names gathered.
    let mut imports: AVec<'a, Import<'r>> = Vec::new_in(alloc);
    for (pos, &i) in order.iter().enumerate() {
        if rep[pos] as usize != pos {
            continue;
        }
        let m = &members[i as usize];
        for k in 0..m.record.targets() {
            let t = m.record.target(k);
            match t.space {
                Space::Tls => return Err(ExportError::ThreadLocal { from: m.symbol }),
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
    imports.sort_unstable_by(|a, b| a.symbol.cmp(&b.symbol));
    imports.dedup_by(|a, b| a.symbol == b.symbol);
    let import_of = |symbol: Symbol| imports.binary_search_by(|it| it.symbol.cmp(&symbol)).ok();

    // The const region: the writer's head, the GOT, relro parts, read-only data.
    let const_at = up(text_end + tail, page);
    let mut konst: AVec<'a, u8> = Vec::new_in(alloc);
    konst.resize(plan.const_head as usize, 0);
    pad_to(&mut konst, const_at, 8);
    let got_at = const_at + konst.len() as u64;
    konst.resize(konst.len() + 8 * imports.len(), 0);
    let mut relro_at: AVec<'_, u64> = Vec::from_elem_in(0, n, alloc);
    for (pos, &i) in order.iter().enumerate() {
        let r = rep[pos] as usize;
        if r != pos {
            relro_at[i as usize] = relro_at[order[r] as usize];
            continue;
        }
        let relro = members[i as usize].record.relro();
        if relro.is_empty() {
            continue;
        }
        pad_to(&mut konst, const_at, 16);
        relro_at[i as usize] = const_at + konst.len() as u64;
        konst.extend_from_slice(relro);
    }
    let mut data_home: AVec<'_, u64> = Vec::from_elem_in(0, data.len(), alloc);
    for &d in by_target.iter() {
        let datum = &data[d as usize];
        if datum.place == DataPlace::ReadOnly {
            pad_to(&mut konst, const_at, u64::from(datum.align));
            data_home[d as usize] = const_at + konst.len() as u64;
            konst.extend_from_slice(datum.bytes);
        }
    }
    // The data region: writable data, then zeroed data with no file bytes.
    let data_at = up(const_at + konst.len() as u64, page);
    let mut bytes: AVec<'a, u8> = Vec::new_in(alloc);
    for &d in by_target.iter() {
        let datum = &data[d as usize];
        if datum.place == DataPlace::Writable {
            pad_to(&mut bytes, data_at, u64::from(datum.align));
            data_home[d as usize] = data_at + bytes.len() as u64;
            bytes.extend_from_slice(datum.bytes);
        }
    }
    let zero_at = up(data_at + bytes.len() as u64, 16);
    let mut zero_end = zero_at;
    for &d in by_target.iter() {
        let datum = &data[d as usize];
        if let DataPlace::Zeroed { len } = datum.place {
            zero_end = up(zero_end, u64::from(datum.align));
            data_home[d as usize] = zero_end;
            zero_end += len;
        }
    }
    let end = up(zero_end, page);
    if end > IMAGE_LIMIT {
        return Err(ExportError::TooLarge { what: "adrp's 4 GiB and a chained fixup's 36-bit target", len: end });
    }
    let home_of = |t: Target| -> Option<u64> {
        match t.space {
            Space::Func => find(members, &by_symbol, t.symbol).map(|i| home[i]),
            Space::Static | Space::Vtable => find_data(data, &by_target, t).map(|d| data_home[d]),
            Space::Tls | Space::Outside => None,
        }
    };

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
                let to = relro_at[i as usize] + u64::from(site.addend);
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
                Space::Tls => return Err(ExportError::ThreadLocal { from: symbol }),
                Space::Outside => {
                    let Some(index) = import_of(t.symbol) else {
                        unreachable!("every outside target of a placed record was gathered above");
                    };
                    let word = got_at + 8 * index as u64;
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
        fixups.push(Fixup { at: got_at + 8 * k as u64, to: FixTo::Bind { import: k as u32 } });
    }
    let rebase = |from: Symbol, at: u64, target: i64| -> Result<FixTo, ExportError> {
        // `<=`: a pointer one past a region's last byte (a slice's end) is an address too.
        match target >= 0 && (target as u64) <= end {
            true => Ok(FixTo::Rebase { target: target as u64 }),
            false => Err(ExportError::WordOutside { from, at }),
        }
    };
    for (pos, &i) in order.iter().enumerate() {
        if rep[pos] as usize != pos {
            continue;
        }
        let m = &members[i as usize];
        for w in 0..m.record.words() {
            let word = m.record.word(w);
            let at = relro_at[i as usize] + u64::from(word.at);
            let to = match word.target {
                OWN_CODE => rebase(m.symbol, at, home[i as usize] as i64 + word.addend)?,
                OWN_RELRO => rebase(m.symbol, at, relro_at[i as usize] as i64 + word.addend)?,
                index => {
                    let t = m.record.target(index as usize);
                    match t.space {
                        Space::Tls => return Err(ExportError::ThreadLocal { from: m.symbol }),
                        Space::Outside if word.addend != 0 => return Err(ExportError::OutsideAddend { from: m.symbol, target: t }),
                        Space::Outside => match import_of(t.symbol) {
                            Some(import) => FixTo::Bind { import: import as u32 },
                            None => unreachable!("every outside target of a placed record was gathered above"),
                        },
                        Space::Func | Space::Static | Space::Vtable => match home_of(t) {
                            Some(to) => rebase(m.symbol, at, to as i64 + word.addend)?,
                            None => return Err(ExportError::OutOfReach { from: m.symbol, target: t }),
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
                None => rebase(datum.target.symbol, at, base as i64 + w.addend)?,
                Some(t) => match (t.space, import_of(t.symbol)) {
                    (Space::Outside, Some(import)) if w.addend == 0 => FixTo::Bind { import: import as u32 },
                    (Space::Func | Space::Static | Space::Vtable, _) => match home_of(t) {
                        Some(to) => rebase(datum.target.symbol, at, to as i64 + w.addend)?,
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
    drop(rep);
    drop(relro_at);
    Ok(Laid {
        page,
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
    })
}
