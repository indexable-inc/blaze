//! Export at the optimized tier (WAVE-BACKEND.md piece G; backend-next.md D100 points 3, 9, 10,
//! 11 and 13): the program's functions as `bzcode02` records (blaze-tier record/form.rs), laid
//! out as one whole program and written by the same platform writers as [`crate::export`]
//! (`macho::write`, `elf::write`), into one [`Laid`].
//!
//! # What is done with each relocation (D100 point 11)
//!
//! | kind | target placed in the image (function, static, vtable, own code or relro) | C name (outside) |
//! |---|---|---|
//! | `Branch26` | resolved in place: a direct `b`/`bl` to the target's home | a `bl` to the name's one STUB (`adrp x16; ldr x16; br x16` through its GOT word) at the end of the code |
//! | `Page21`, `PageOff12` | the target's address (`adrp`; `add` or load/store by its scale) | refused by name ([`ExportError::AddressOfOutside`]): only the loader knows a C name's address |
//! | `GotPage21`, `GotPageOff12` | RELAXED: the call triple `adrp xN; ldr xN; blr xN` (N 16 or 17) becomes `nop; nop; bl`; any other pair becomes `adrp; add` of the target's address | the GOT word the loader binds |
//! | `Abs64` (relro) | a rebase the loader writes | a bind |
//! | `Pc32`, `GotPc32` | refused by name: an export is for arm64 | |
//!
//! So nothing in reach goes through a slot, a stub or a word: every call between the program's
//! functions is a direct `bl`, whatever tier made the record. A `GotPage21` of a pair that is not
//! the canonical triple is kept as an `adrp` of the target's page (correct whichever `ldr` reads
//! its register, since every pair of one register names one target); ld64's GOT relaxation keeps
//! it the same way. The optimized tier's own code calls by `Branch26`, so its calls are at the
//! floor: one `bl`, no `adrp`.
//!
//! # Polls (D100 point 3)
//!
//! A poll is `adrp x16, poll@PAGE; ldr xzr, [x16, poll@PAGEOFF]` on a loop's back edge: it lets
//! a run's control thread stop a loop for tier-up. An exported file has no runtime and never
//! tiers up, so its poll page would never be armed: both instructions become `nop`, and the
//! poll's two relocations are not completed, so the poll page is neither placed nor bound. A poll
//! whose instructions are not that form is refused by name (the record's rows said a poll).
//!
//! # Layout (D100 point 13)
//!
//! The order of functions: the profile's hot functions first, in its order (the hot region),
//! then a depth-first walk from the entry over each record's words and relocations, so a caller
//! sits just before its first callee, then what is left by symbol: the same rule as
//! layout.rs. Equal records (equal code, relro part, targets, relocations, words and polls) are
//! placed once. The loop map, positions, inline frames and read list are a run's and are not
//! read here.
//!
//! # Unwinding (D100 point 10)
//!
//! From the rows: each record's unwind frame and rows ([`RecordView::unwind_frame`],
//! [`RecordView::unwind_row`]) are made one blaze-code `Unwind` by `unwind::of_rows` (checked by
//! the same rules as a `bzcode01` record's pads), and eh_out.rs writes the tables from them
//! (`eh_room_rows`, `eh_sections_rows`: the same writer `export` uses through its `Member`
//! entries). The record's `pads` section is not read: its rows say the same, and the rows are
//! what every tier fills.
//!
//! # Retirement
//!
//! When `bzcode01` is retired, `export` and layout.rs's `lay_out` are deleted and this is the one
//! layout: [`v1_to_v2`] already brings any `bzcode01` record (the Reference tier's, the start
//! records) into this path, and the bytes it writes for them are equal to `export`'s (the test
//! `optimized_equals_export_for_converted_records`).

use blaze_code::a64::{ADRP, BLR, LDR64, NOP, add_lo12, adrp, bl, expect, ldr_lo12, read32, write32};
use blaze_code::image::ImageError;
use blaze_code::place::PlaceError;
use blaze_code::record::{CodeRecord, CodeView, OWN_CODE, OWN_RELRO, Space, Symbol, Target, Word};
use blaze_code::unwind::{self, Row, Unwind};
use blaze_ids::Hasher;
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::record::{Declare, DeclaredKind, Declares, FormError, RecordView, Reloc, RelocKind, calls, declare, encode, from_v1, page_offset, read, seal};

use crate::eh_out::{eh_room_rows, eh_sections_rows};
use crate::error::ExportError;
use crate::export::{Exported, Panics};
use crate::input::{Data, Entry, Panic, Platform, Profile, Tier};
use crate::layout::{
    FixTo, Fixup, Import, Laid, LayoutMember, Placed, Plan, check_data, find_member, lay_out_const_and_data, pad_to, rebase_target, target_home,
};
use crate::start::{main_of, start};
use crate::{elf, macho};

const FOLD_DOMAIN: &str = "blaze-export fold bzcode02 1";

/// `b`/`bl`: bits 30..26 fixed, bit 31 the link.
const B_OR_BL: (u32, u32) = (0x7C00_0000, 0x1400_0000);
/// A poll's `adrp x16`.
const ADRP_X16_FORM: (u32, u32) = (0x9F00_001F, 0x9000_0010);
/// A poll's `ldr xzr, [x16, #imm]`.
const LDR_XZR_X16_FORM: (u32, u32) = (0xFFC0_03FF, 0xF940_021F);
/// A stub: `adrp x16, word@PAGE; ldr x16, [x16, word@PAGEOFF]; br x16`.
const STUB_ADRP_X16: u32 = 0x9000_0010;
const STUB_LDR_X16: u32 = 0xF940_0210;
const STUB_BR_X16: u32 = 0xD61F_0200;
/// A stub's bytes, its last word padding to 16.
const STUB_LEN: u64 = 16;
/// No stub for this import.
const NO_STUB: u64 = u64::MAX;

/// One reached function instance at this path: its symbol, its `bzcode02` record read in place
/// (checked once by blaze-tier's `read`), and the tier that made the record.
#[derive(Clone, Copy)]
pub struct Function02<'r> {
    pub symbol: Symbol,
    pub record: RecordView<'r>,
    pub tier: Tier,
}

/// One export's whole input at this path: [`crate::ProgramIn`] with `bzcode02` functions. Every
/// other field means what it means there.
#[derive(Clone, Copy)]
pub struct ProgramIn02<'r> {
    pub name: &'r str,
    /// Every function the program reaches, each once, in any order.
    pub functions: &'r [Function02<'r>],
    pub data: &'r [Data<'r>],
    pub entry: Entry,
    pub panic: Panic,
    /// The program's `eh_personality` routine, one of `functions` (as `ProgramIn::personality`).
    pub personality: Option<Symbol>,
    pub profile: Option<Profile<'r>>,
}

/// A function to place: one of the program's, or a start record the export made (`tier: None`).
#[derive(Clone, Copy)]
pub struct Member02<'r> {
    pub symbol: Symbol,
    pub record: RecordView<'r>,
    pub tier: Option<Tier>,
}

impl LayoutMember for Member02<'_> {
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

/// `view` (a `bzcode01` record) in the `bzcode02` form, encoded into `out`: rebuilt as a
/// blaze-code `CodeRecord` and converted by blaze-tier's `from_v1` (the one home of the
/// site-to-relocation rule), then sealed and encoded. Each outside target is given its name
/// through `CodeRecord::outside`, which adds the target a second time; the seal merges equal
/// targets. Its in-program targets are declared by `by` (bzcode03, D106).
pub fn v1_to_v2<'a>(view: CodeView<'_>, by: &dyn Declares, out: &mut AVec<'a, u8>, alloc: Alloc<'a>) -> Result<(), FormError> {
    view.require_positionless("optimized export").map_err(FormError::V1Record)?;
    let mut r = CodeRecord::new_in(alloc);
    r.code.extend_from_slice(view.code());
    r.relro.extend_from_slice(view.relro());
    r.frame = view.frame();
    for k in 0..view.targets() {
        r.targets.push(view.target(k));
    }
    for k in 0..view.sites() {
        r.sites.push(view.site(k));
    }
    for k in 0..view.words() {
        r.words.push(view.word(k));
    }
    for k in 0..view.pads() {
        r.pads.push(view.pad(k));
    }
    for k in 0..view.targets() {
        let target = view.target(k);
        if target.space != Space::Outside {
            continue;
        }
        let Some(name) = view.name(target.symbol) else {
            return Err(FormError::NameMissing { symbol: target.symbol });
        };
        r.outside(name);
    }
    let mut e = from_v1(r, alloc)?;
    declare(&mut e, by)?;
    let sealed = seal(e)?;
    encode(&sealed, out);
    Ok(())
}

/// The start records' declarer: each fn they name is bound by this export (`main`,
/// `lang_start`, a start piece), never followed by a walk; they name nothing else in the
/// program, so any other target is refused by `declare`, by name.
pub struct StartBound;

impl Declares for StartBound {
    fn declared(&self, t: Target) -> Option<Declare<'_>> {
        (t.space == Space::Func).then_some(Declare { kind: DeclaredKind::Bound, item: 0, name: &[] })
    }
}

/// Every member's unwind rows, made one blaze-code `Unwind` each from the rows in `rows` (which
/// [`rows_of`] filled), in member order, paired with the member's symbol.
fn unwinds<'a, 'r>(members: &[Member02<'_>], rows: &'r [Row], alloc: Alloc<'a>) -> Result<AVec<'a, (Symbol, Unwind<'r>)>, ExportError> {
    let mut out: AVec<'a, (Symbol, Unwind<'r>)> = Vec::with_capacity_in(members.len(), alloc);
    let mut from = 0usize;
    for m in members {
        let r = m.record;
        let to = from + r.unwind_rows();
        let unwind = unwind::of_rows(r.unwind_frame(), &rows[from..to], r.code().len() as u32)
            .map_err(|error| ExportError::UnwindRows { symbol: m.symbol, error })?;
        out.push((m.symbol, unwind));
        from = to;
    }
    Ok(out)
}

/// Every member's unwind rows, in member order, into one list ([`unwinds`] takes them apart).
fn rows_of<'a>(members: &[Member02<'_>], alloc: Alloc<'a>) -> AVec<'a, Row> {
    let mut rows: AVec<'a, Row> = Vec::new_in(alloc);
    for m in members {
        for k in 0..m.record.unwind_rows() {
            rows.push(m.record.unwind_row(k));
        }
    }
    rows
}

/// Exports `program` for `platform` from its `bzcode02` records: the file's bytes replace `out`'s
/// contents. The same input gives the same bytes on every machine.
pub fn export_optimized<'a>(program: &ProgramIn02<'_>, platform: Platform, out: &mut AVec<'a, u8>, alloc: Alloc<'a>) -> Result<Exported, ExportError> {
    let has = |symbol: Symbol| program.functions.iter().any(|f| f.symbol == symbol);
    let main = main_of(program.entry);
    if !matches!(program.entry, Entry::Hosted { .. }) && !has(main) {
        return Err(ExportError::NoEntry { symbol: main });
    }
    if let Entry::LangStart { lang_start, .. } = program.entry
        && !has(lang_start)
    {
        return Err(ExportError::NoEntry { symbol: lang_start });
    }
    let start = start(program.entry, platform, alloc)?;
    let mut made: AVec<'a, (Symbol, AVec<'a, u8>)> = Vec::with_capacity_in(start.records.len(), alloc);
    for (symbol, view) in start.views() {
        let mut bytes: AVec<'a, u8> = Vec::new_in(alloc);
        v1_to_v2(view, &StartBound, &mut bytes, alloc).map_err(|error| ExportError::Record { symbol, error })?;
        made.push((symbol, bytes));
    }
    let mut members: AVec<'_, Member02<'_>> = Vec::with_capacity_in(program.functions.len() + made.len(), alloc);
    for f in program.functions {
        members.push(Member02 { symbol: f.symbol, record: f.record, tier: Some(f.tier) });
    }
    for (symbol, bytes) in made.iter() {
        let record = read(bytes).map_err(|error| ExportError::Record { symbol: *symbol, error })?;
        members.push(Member02 { symbol: *symbol, record, tier: None });
    }
    let plan = match platform {
        Platform::MacosArm64 => macho::plan(),
        Platform::LinuxArm64 => elf::plan(),
        Platform::Wasm64 => return Err(ExportError::Wasm64RequiresWasmProgram),
    };
    let rows = rows_of(&members, alloc);
    let unwinds = unwinds(&members, &rows, alloc)?;
    let room = eh_room_rows(&unwinds, program.personality, platform, alloc)?;
    let laid = lay_out_records(&members, program.data, start.entry, program.profile, plan, room, alloc)?;
    let eh = eh_sections_rows(&laid, &unwinds, program.personality, platform, alloc)?;
    let panics = match program.panic {
        Panic::Unwind => Panics::Unwind { functions: eh.functions },
        Panic::Abort => Panics::Abort { functions: eh.functions },
    };
    let seed = program.profile.map_or([0u8; 32], |p| p.digest);
    let identity = match platform {
        Platform::MacosArm64 => macho::write(&laid, &eh, program.name, &seed, out, alloc)?,
        Platform::LinuxArm64 => elf::write(&laid, &eh, &seed, out, alloc)?,
        Platform::Wasm64 => return Err(ExportError::Wasm64RequiresWasmProgram),
    };
    let (mut functions, mut folded, mut fast, mut optimized, mut made_count) = (0u32, 0u32, 0u32, 0u32, 0u32);
    for p in laid.placed.iter() {
        match p.tier {
            None => made_count += 1,
            Some(tier) => {
                functions += 1;
                folded += u32::from(p.folded);
                match tier {
                    Tier::Fast => fast += 1,
                    Tier::Optimized => optimized += 1,
                }
            }
        }
    }
    Ok(Exported {
        platform,
        functions,
        folded,
        fast,
        optimized,
        made: made_count,
        imports: laid.imports.len() as u32,
        len: out.len() as u64,
        identity,
        profile: program.profile.map(|p| p.digest),
        panics,
    })
}

/// The function a word or relocation names, if it names one: not the record's own code or relro.
fn named(r: &RecordView<'_>, target: u32) -> Option<Target> {
    match target {
        OWN_CODE | OWN_RELRO => None,
        index => Some(r.target(index as usize)),
    }
}

/// Whether `at` is inside one of `r`'s polls (two instructions each). The polls are sorted by
/// offset (the seal refuses them otherwise): a binary search for the last poll at or before `at`.
fn in_poll(r: &RecordView<'_>, at: u32) -> bool {
    let (mut lo, mut hi) = (0usize, r.polls());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match r.poll(mid).at <= at {
            true => lo = mid + 1,
            false => hi = mid,
        }
    }
    lo > 0 && at < r.poll(lo - 1).at + 8
}

/// The placement order: profile first, then a depth-first walk from the entry, then the rest by
/// symbol. A work list, no recursion.
fn order<'a>(members: &[Member02<'_>], by_symbol: &[u32], entry: usize, profile: Option<Profile<'_>>, alloc: Alloc<'a>) -> AVec<'a, u32> {
    let n = members.len();
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
            // Words first, so they pop after the relocations; relocations in reverse, so the
            // first call in the code is walked first.
            let words = (0..r.words()).rev().map(|w| r.word(w).target);
            let relocs = (0..r.relocs()).rev().filter(|&k| !in_poll(&r, r.reloc(k).at) || r.reloc(k).in_relro).map(|k| r.reloc(k).target);
            for target in words.chain(relocs) {
                if let Some(t) = named(&r, target)
                    && t.space == Space::Func
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

/// Whether two records lay out to the same bytes wherever they are placed.
fn same(a: &RecordView<'_>, b: &RecordView<'_>) -> bool {
    a.code() == b.code()
        && a.relro() == b.relro()
        && a.targets() == b.targets()
        && (0..a.targets()).all(|k| a.target(k) == b.target(k))
        && a.relocs() == b.relocs()
        && (0..a.relocs()).all(|k| a.reloc(k) == b.reloc(k))
        && a.words() == b.words()
        && (0..a.words()).all(|k| a.word(k) == b.word(k))
        && a.polls() == b.polls()
        && (0..a.polls()).all(|k| a.poll(k) == b.poll(k))
}

/// Whether the GOT pair whose `adrp` is relocation `k` of `r` is the call triple `adrp xN, word;
/// ldr xN, [xN, word]; blr xN` (N 16 or 17, adjacent, the `ldr` the next relocation, of the same
/// target). Its `adrp` then becomes a `nop`: the `ldr` overwrites xN and the call clobbers it, so
/// nothing reads the page it made.
fn call_triple(r: &RecordView<'_>, k: usize) -> bool {
    let code = r.code();
    let page = r.reloc(k);
    let Some(off) = (k + 1 < r.relocs()).then(|| r.reloc(k + 1)) else { return false };
    if off.kind != RelocKind::GotPageOff12 || off.in_relro || off.at != page.at + 4 || off.target != page.target {
        return false;
    }
    let rd = read32(code, u64::from(page.at)) & 0x1F;
    let ldr = read32(code, u64::from(off.at));
    ldr & LDR64.0 == LDR64.1 && (ldr >> 5) & 0x1F == rd && ldr & 0x1F == rd && calls(code, off.at)
}

/// `inst`, a `b` or `bl` at `pc`, made to reach `to`.
fn branch(inst: u32, pc: u64, to: u64, symbol: Symbol) -> Result<u32, ExportError> {
    let delta = to as i64 - pc as i64;
    if delta % 4 != 0 || !(-(1i64 << 27)..(1i64 << 27)).contains(&delta) {
        return Err(ExportError::Patch(ImageError::OutOfReach { symbol, at: pc, what: "b/bl (128 MiB)" }));
    }
    Ok((inst & 0xFC00_0000) | (((delta >> 2) as u32) & 0x03FF_FFFF))
}

/// Lays the program out from `bzcode02` records. `members` holds every function (the program's
/// and the start's), in any order; `entry` is the one the loader enters; `tail` bytes are kept
/// after the code (the stubs included) for the unwind tables (eh_out.rs `eh_room_rows`), as
/// layout.rs's `lay_out` keeps them.
pub fn lay_out_records<'a, 'r>(
    members: &[Member02<'r>],
    data: &[Data<'r>],
    entry: Symbol,
    profile: Option<Profile<'_>>,
    plan: Plan,
    tail: u64,
    alloc: Alloc<'a>,
) -> Result<Laid<'a, 'r>, ExportError> {
    let n = members.len();
    let page = plan.page;
    let mut by_symbol: AVec<'_, u32> = Vec::with_capacity_in(n, alloc);
    for i in 0..n as u32 {
        by_symbol.push(i);
    }
    by_symbol.sort_unstable_by(|&a, &b| members[a as usize].symbol.cmp(&members[b as usize].symbol));
    for pair in by_symbol.windows(2) {
        if members[pair[0] as usize].symbol == members[pair[1] as usize].symbol {
            return Err(ExportError::TwoRecords { symbol: members[pair[0] as usize].symbol });
        }
    }
    let by_target = check_data(data, page, alloc)?;
    let Some(entry_member) = find_member(members, &by_symbol, entry) else {
        return Err(ExportError::NoEntry { symbol: entry });
    };
    let order = order(members, &by_symbol, entry_member, profile, alloc);

    // Folding: each position's representative is the first position with an equal record.
    let mut digests: AVec<'_, ([u8; 32], u32)> = Vec::with_capacity_in(n, alloc);
    for (pos, &i) in order.iter().enumerate() {
        let r = members[i as usize].record;
        let mut h = Hasher::new(FOLD_DOMAIN);
        h.update(r.code());
        h.update(r.relro());
        digests.push((h.finish().0, pos as u32));
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
        let equal = same(&members[order[first as usize] as usize].record, &members[order[pos as usize] as usize].record);
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

    // Every target a placed record USES (a poll's page is not used: see the module's
    // documentation): thread-locals refused, C names gathered.
    let mut imports: AVec<'a, Import<'r>> = Vec::new_in(alloc);
    for (pos, &i) in order.iter().enumerate() {
        if rep[pos] as usize != pos {
            continue;
        }
        let m = &members[i as usize];
        let r = m.record;
        let words = (0..r.words()).map(|w| r.word(w).target);
        let relocs = (0..r.relocs()).filter(|&k| r.reloc(k).in_relro || !in_poll(&r, r.reloc(k).at)).map(|k| r.reloc(k).target);
        for target in words.chain(relocs) {
            let Some(t) = named(&r, target) else { continue };
            match t.space {
                Space::Tls => return Err(ExportError::ThreadLocal { from: m.symbol }),
                Space::Outside => {
                    let Some(name) = r.name(t.symbol) else {
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

    // The stubs: one per C name some `b`/`bl` reaches, after the code, in import order.
    let mut stub_at: AVec<'_, u64> = Vec::from_elem_in(NO_STUB, imports.len(), alloc);
    for (pos, &i) in order.iter().enumerate() {
        if rep[pos] as usize != pos {
            continue;
        }
        let r = members[i as usize].record;
        for k in 0..r.relocs() {
            let reloc = r.reloc(k);
            if reloc.kind != RelocKind::Branch26 || in_poll(&r, reloc.at) {
                continue;
            }
            if let Some(t) = named(&r, reloc.target)
                && t.space == Space::Outside
                && let Some(index) = import_of(t.symbol)
            {
                stub_at[index] = 0;
            }
        }
    }
    for k in 0..imports.len() {
        if stub_at[k] == NO_STUB {
            continue;
        }
        pad_to(&mut text, plan.text_at, STUB_LEN);
        stub_at[k] = plan.text_at + text.len() as u64;
        for inst in [STUB_ADRP_X16, STUB_LDR_X16, STUB_BR_X16, NOP] {
            text.extend_from_slice(&inst.to_le_bytes());
        }
    }
    let text_end = plan.text_at + text.len() as u64;

    let regions = lay_out_const_and_data(members, &order, &rep, data, &by_target, imports.len(), text_end, plan, tail, alloc)?;
    let home_of = |t: Target| target_home(members, &by_symbol, &home, data, &by_target, &regions.data_home, t);
    let word_of = |symbol: Symbol| -> u64 {
        match import_of(symbol) {
            Some(index) => regions.got_at + 8 * index as u64,
            None => unreachable!("every outside target a placed record uses was gathered above"),
        }
    };

    // The stubs' GOT pairs.
    for k in 0..imports.len() {
        let at = stub_at[k];
        if at == NO_STUB {
            continue;
        }
        let word = regions.got_at + 8 * k as u64;
        let x = at - plan.text_at;
        let stub = imports[k].symbol;
        write32(&mut text, x, adrp(STUB_ADRP_X16, at, word, stub)?);
        write32(&mut text, x + 4, ldr_lo12(STUB_LDR_X16, word));
    }

    // Every word the loader fixes: the GOT's first, then the relocations' and words' of each
    // record, then the data's; sorted by offset at the end.
    let mut fixups: AVec<'a, Fixup> = Vec::new_in(alloc);
    for k in 0..imports.len() {
        fixups.push(Fixup { at: regions.got_at + 8 * k as u64, to: FixTo::Bind { import: k as u32 } });
    }

    // Complete every placed record's relocations and polls.
    for (pos, &i) in order.iter().enumerate() {
        if rep[pos] as usize != pos {
            continue;
        }
        let m = &members[i as usize];
        let (r, symbol) = (m.record, m.symbol);
        let at_home = home[i as usize];
        let own_relro = regions.relro_at[i as usize];
        // The address an address relocation names, its addend added.
        let address = |reloc: Reloc| -> Result<u64, ExportError> {
            let base = match reloc.target {
                OWN_CODE => at_home,
                OWN_RELRO => own_relro,
                index => {
                    let t = r.target(index as usize);
                    match t.space {
                        Space::Tls => return Err(ExportError::ThreadLocal { from: symbol }),
                        Space::Outside => return Err(ExportError::AddressOfOutside { from: symbol, target: t, at: reloc.at }),
                        Space::Func | Space::Static | Space::Vtable => match home_of(t) {
                            Some(to) => to,
                            None => return Err(ExportError::OutOfReach { from: symbol, target: t }),
                        },
                    }
                }
            };
            Ok(base.wrapping_add_signed(reloc.addend))
        };
        for k in 0..r.relocs() {
            let reloc = r.reloc(k);
            if reloc.kind == RelocKind::Abs64 {
                let at = own_relro + u64::from(reloc.at);
                let to = match named(&r, reloc.target) {
                    Some(t) if t.space == Space::Outside => match import_of(t.symbol) {
                        Some(import) => FixTo::Bind { import: import as u32 },
                        None => unreachable!("every outside target a placed record uses was gathered above"),
                    },
                    _ => rebase_target(symbol, at, address(reloc)? as i64, regions.end)?,
                };
                fixups.push(Fixup { at, to });
                continue;
            }
            if in_poll(&r, reloc.at) {
                continue;
            }
            let pc = at_home + u64::from(reloc.at);
            let x = pc - plan.text_at;
            match reloc.kind {
                RelocKind::Branch26 => {
                    let inst = expect(&text, x, B_OR_BL, symbol, "b or bl")?;
                    let to = match named(&r, reloc.target) {
                        None if reloc.target == OWN_RELRO => return Err(ExportError::CallOfData { from: symbol, at: reloc.at }),
                        None => address(reloc)?,
                        Some(t) => match t.space {
                            Space::Func => address(reloc)?,
                            Space::Outside => match import_of(t.symbol) {
                                Some(index) if stub_at[index] != NO_STUB => stub_at[index],
                                _ => unreachable!("every outside target a b/bl reaches was given a stub above"),
                            },
                            Space::Static | Space::Vtable => return Err(ExportError::CallOfData { from: symbol, at: reloc.at }),
                            Space::Tls => return Err(ExportError::ThreadLocal { from: symbol }),
                        },
                    };
                    write32(&mut text, x, branch(inst, pc, to, symbol)?);
                }
                RelocKind::Page21 => {
                    let inst = expect(&text, x, ADRP, symbol, "adrp")?;
                    let to = address(reloc)?;
                    write32(&mut text, x, adrp(inst, pc, to, symbol)?);
                }
                RelocKind::PageOff12 { scale } => {
                    let inst = read32(&text, x);
                    let to = address(reloc)?;
                    write32(&mut text, x, page_offset(inst, scale, to, u64::from(reloc.at), symbol).map_err(ExportError::Place)?);
                }
                RelocKind::GotPage21 => {
                    let inst = expect(&text, x, ADRP, symbol, "adrp")?;
                    // The seal refused an import-word kind on the record's own code or relro.
                    let t = r.target(reloc.target as usize);
                    let to = match t.space {
                        Space::Outside => word_of(t.symbol),
                        Space::Tls => return Err(ExportError::ThreadLocal { from: symbol }),
                        Space::Func if call_triple(&r, k) => {
                            if home_of(t).is_none() {
                                return Err(ExportError::OutOfReach { from: symbol, target: t });
                            }
                            write32(&mut text, x, NOP);
                            continue;
                        }
                        Space::Func | Space::Static | Space::Vtable => match home_of(t) {
                            Some(to) => to,
                            None => return Err(ExportError::OutOfReach { from: symbol, target: t }),
                        },
                    };
                    write32(&mut text, x, adrp(inst, pc, to, symbol)?);
                }
                RelocKind::GotPageOff12 => {
                    let inst = expect(&text, x, LDR64, symbol, "ldr")?;
                    let t = r.target(reloc.target as usize);
                    let call = calls(r.code(), reloc.at);
                    match t.space {
                        Space::Outside => write32(&mut text, x, ldr_lo12(inst, word_of(t.symbol))),
                        Space::Tls => return Err(ExportError::ThreadLocal { from: symbol }),
                        Space::Func | Space::Static | Space::Vtable => {
                            let Some(to) = home_of(t) else {
                                return Err(ExportError::OutOfReach { from: symbol, target: t });
                            };
                            match (call, t.space) {
                                (true, Space::Func) => {
                                    expect(&text, x + 4, BLR, symbol, "blr")?;
                                    write32(&mut text, x, NOP);
                                    write32(&mut text, x + 4, bl(pc + 4, to, symbol)?);
                                }
                                (true, _) => return Err(ExportError::CallOfData { from: symbol, at: reloc.at }),
                                (false, _) => write32(&mut text, x, add_lo12(inst, to)),
                            }
                        }
                    }
                }
                RelocKind::Pc32 | RelocKind::GotPc32 => {
                    return Err(ExportError::Place(PlaceError::Site(ImageError::NotInstruction {
                        symbol,
                        at: u64::from(reloc.at),
                        want: "arm64 relocation (Pc32 and GotPc32 are x86-64's; an export is for arm64)",
                    })));
                }
                RelocKind::Abs64 => unreachable!("Abs64 was completed above"),
            }
        }
        for p in 0..r.polls() {
            let x = at_home + u64::from(r.poll(p).at) - plan.text_at;
            expect(&text, x, ADRP_X16_FORM, symbol, "adrp x16 (a poll's first instruction)")?;
            expect(&text, x + 4, LDR_XZR_X16_FORM, symbol, "ldr xzr, [x16] (a poll's second instruction)")?;
            write32(&mut text, x, NOP);
            write32(&mut text, x + 4, NOP);
        }
    }

    regions.finish(members, &by_symbol, &order, &rep, &home, data, &by_target, entry_member, plan, tail, text, imports, fixups, placed)
}
