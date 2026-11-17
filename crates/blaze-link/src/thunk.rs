//! Range-extension thunks (branch islands): how an AArch64 `b` or `bl` reaches a target past
//! its ±128 MiB, as ld64's and lld's islands do.
//!
//! A thunk is a piece of the linker's own, `adrp x16, T; add x16, x16, :lo12:T; br x16`
//! ([`CODE`]), which reaches ±4 GiB. Its key is its target (the PLT stub for an import), its
//! island and the addend of the branches it serves ([`SymKey::Thunk`]). The branches keep their
//! fixups: the writer sends one out of reach to the nearest thunk of its target and addend in
//! reach ([`through`]), and fails naming the caller and the target when none is. A thunk is
//! gone with its target (`Stage::cells_of_removed`), as a PLT stub is.
//!
//! Where thunks go:
//! - A full link cuts its code class, in the laid-out order and without islands, into
//!   *segments* ([`segments`]): a segment ends before the piece that would take it past
//!   [`SEGMENT`] bytes, so an island comes before a piece that would carry the code past what
//!   the callers before it reach (a function over 128 MiB gets an island on each side). Island
//!   `k` is right after segment `k`. The cut reads only the pieces' sizes, so a segment's index
//!   is the same in every placement round. A far branch takes the island after its segment, or
//!   the one before it when only that one is in reach. A branch from a piece of another
//!   executable class, laid out outside the segments (ELF's `.init` and `.fini`, after the
//!   code), takes the island nearest it in reach: the last one, for `.init` after the code.
//! - An incremental link puts the thunks its written code needs in the patch region, after the
//!   code it places: one island for the stage, a new index ([`State::islands`]). A thunk of any
//!   island already in reach is used instead. New code is in the patch region, so in reach of
//!   that island. Code the stage writes again where it is (a direct caller, in the full
//!   link's code, of a function the stage moved to the patch region) may be past 128 MiB from
//!   the patch region, and no island of the full link holds a thunk of the moved function
//!   when the two were in reach before: no thunk can be put in that branch's reach without
//!   moving code, so the stage is refused, typed ([`Refusal::NoThunk`]), and the build links
//!   the image from scratch, saying so. That is the last resort, not the plan: a build whose
//!   edited code is reached through slots calls every function an edit can replace through
//!   its slot (`blaze_codegen::driver`'s `rebind`; docs/v2/plan/design/codegen.md, "Slots
//!   and a splice's reach"), so the callers of replaced code hold a branch to a slot's stub,
//!   which no stage moves, and are never written again. What still gets here is a direct
//!   caller of code the classification did not name, or an image of a build with no slots
//!   in its calls; `tests/thunks.rs` holds the invariant.
//!
//! [`Stage::thunks`] runs in each placement round (`Image::stage_with`) until it makes nothing,
//! and never removes a thunk within a stage after its first round, so the rounds settle: each
//! adds islands' bytes, which only moves code so far. A full link's first round drops the thunks
//! of an earlier link its layout no longer uses.
//!
//! [`State::islands`]: crate::state::State::islands

use std::alloc::Allocator;

use blaze_codegen::Arch;
use blaze_mem::align::align_up_u64;

use crate::arena::FrameVec;
use crate::model::{Apply, Class, Fixup, Region, Sym};
use crate::stage::{Content, Stage, error, function_of, named_in, refused};
use crate::state::{PieceId, PieceRef, State, What};
use crate::{LinkError, Refusal};
use crate::symbols::{SymKey, Symbols};
use crate::write::{Resolver, Value};

/// How far an AArch64 `b` or `bl` reaches either way: a 26-bit field of words.
pub(crate) const REACH: i64 = 1 << 27;
/// The most bytes a full link's segment of code spans, unless one piece alone is longer: what
/// any caller in it is from the island after it, with room to spare for the island.
pub(crate) const SEGMENT: u64 = 64 << 20;
/// What the planner keeps clear of a branch's reach, for islands to grow by in later rounds.
const MARGIN: i64 = 8 << 20;
/// `adrp x16, 0; add x16, x16, 0; br x16`: x16 (IP0) is the register AAPCS64 leaves to veneers.
pub(crate) const CODE: [u32; 3] = [0x9000_0010, 0x9100_0210, 0xd61f_0200];
/// The bytes of [`CODE`].
pub(crate) const LEN: u64 = 12;
static BYTES: [u8; LEN as usize] = bytes();

const fn bytes() -> [u8; LEN as usize] {
    let mut out = [0u8; LEN as usize];
    let mut at = 0;
    while at < CODE.len() {
        let word = CODE[at].to_le_bytes();
        let mut byte = 0;
        while byte < 4 {
            out[at * 4 + byte] = word[byte];
            byte += 1;
        }
        at += 1;
    }
    out
}

/// Whether a fixup is a branch a thunk can extend: `b` and `bl`.
#[inline]
pub(crate) fn extends(apply: Apply) -> bool {
    matches!(apply, Apply::A64Call26 | Apply::A64Jump26)
}

/// Where an AArch64 branch at `p` to a weak name nothing defines goes, `None` for a fixup that
/// is no AArch64 branch: the next instruction, as lld's (bfd writes a `nop` there instead), so
/// a call C guards with a test of the function's address (zstd's `ZSTD_trace_compress_begin`)
/// is linked as a step past it. Never address 0: a branch is relative, so in a PIE 0 is the
/// image's base rather than null, and past 128 MiB of code it is out of reach, where no thunk
/// helps (a thunk to 0 would jump to the base too).
#[inline]
pub(crate) fn weak_branch(apply: Apply, p: u64) -> Option<u64> {
    matches!(apply, Apply::A64Call26 | Apply::A64Jump26 | Apply::A64CondBr19 | Apply::A64TstBr14)
        .then(|| p.wrapping_add(4))
}

/// Whether a branch at `p` reaches `to` within `reach` either way.
#[inline]
pub(crate) fn reaches(p: u64, to: u64, reach: i64) -> bool {
    (-reach..reach).contains(&(to.wrapping_sub(p) as i64))
}

/// How far `to` is from a branch at `p`, as an error says it: signed (`-` is below the
/// branch) and in MiB to a tenth, the unit of a branch's reach. Never the difference as
/// hexadecimal: a backward one prints as a number near 2^64.
pub(crate) fn away(p: u64, to: u64) -> String {
    let by = to.wrapping_sub(p) as i64;
    let size = by.unsigned_abs();
    let sign = if by < 0 { '-' } else { '+' };
    // alloc: error path
    format!("{sign}{}.{} MiB", size >> 20, ((size & 0xf_ffff) * 10) >> 20)
}

/// Where a full link cuts `pieces` (each `(align, size)`, laid out from 0 in this order) into
/// segments: the index of each segment's end, the last being `pieces`' length. A segment ends
/// before a piece whose end would be more than [`SEGMENT`] past the segment's start, unless
/// the segment has no piece yet.
pub(crate) fn segments<A: Allocator>(
    pieces: impl Iterator<Item = (u64, u64)>,
    out: &mut Vec<usize, A>,
) {
    let (mut at, mut start, mut first) = (0u64, 0u64, 0usize);
    let mut count = 0;
    for (index, (align, size)) in pieces.enumerate() {
        let from = align_up_u64(at, align.max(1));
        if index > first && from + size - start > SEGMENT {
            out.push(index);
            (start, first) = (from, index);
        }
        at = from + size;
        count = index + 1;
    }
    out.push(count);
}

/// The address of the thunk of `target` and `addend` nearest the branch at `p` among those in
/// its reach, `None` when there is none.
pub(crate) fn through(
    symbols: &Symbols,
    state: &State,
    target: Sym,
    addend: i64,
    p: u64,
) -> Option<u64> {
    let addend = i32::try_from(addend).ok()?;
    let mut best: Option<u64> = None;
    for island in 0..state.islands {
        let Some(at) = placed(symbols, state, SymKey::Thunk(target, island, addend)) else {
            continue;
        };
        if reaches(p, at, REACH) && best.is_none_or(|it| at.abs_diff(p) < it.abs_diff(p)) {
            best = Some(at);
        }
    }
    best
}

/// The address of the placed, live thunk of `key`.
fn placed(symbols: &Symbols, state: &State, key: SymKey) -> Option<u64> {
    let (id, _) = state.home(symbols.find(key)?)?;
    let piece = state.piece(id)?;
    piece.placed().then_some(piece.addr)
}

/// A branch the planner found past its reach: its target (as the writer reaches it) and addend.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Need {
    target: Sym,
    island: u32,
    addend: i32,
}

/// What [`Stage::plan_thunk`] reads and fills in a full link's placement round.
struct Planning<'p, 'a> {
    /// Each island's first and end address as placed.
    spans: &'p [(u64, u64)],
    needs: &'p mut FrameVec<'a, Need>,
    used: &'p mut FrameVec<'a, PieceId>,
}

impl<'a> Stage<'a> {
    /// A full link's code class laid out as segments and their islands ([module docs](self)):
    /// `ids` (sorted) with each thunk moved after its segment, and what [`Stage::thunks`] reads
    /// recorded, with `outside`, the pieces of the image's other executable classes. Only an
    /// AArch64 image's [`Class::Text`].
    pub(crate) fn islands(&mut self, ids: &mut Vec<PieceId>, outside: FrameVec<'a, PieceId>) {
        let arena = self.arena;
        let mut code: FrameVec<'a, PieceId> = Vec::with_capacity_in(ids.len(), arena);
        let mut thunks: FrameVec<'a, (u32, PieceId)> = Vec::new_in(arena);
        for id in ids.iter() {
            match self.new.piece(*id).expect("live").what() {
                What::Thunk { island, .. } => thunks.push((island, *id)),
                _ => code.push(*id),
            }
        }
        let mut ends: FrameVec<'a, usize> = Vec::new_in(arena);
        segments(
            code.iter().map(|id| {
                let piece = self.new.piece(*id).expect("live");
                (piece.align(), piece.size)
            }),
            &mut ends,
        );
        // Stable: each island's thunks keep the class's sorted order.
        thunks.sort_by_key(|it| it.0);
        ids.clear();
        let mut next = 0;
        let mut start = 0;
        for (island, end) in ends.iter().enumerate() {
            ids.extend_from_slice(&code[start..*end]);
            start = *end;
            while let Some((_, id)) = thunks.get(next).filter(|it| it.0 as usize == island) {
                ids.push(*id);
                next += 1;
            }
        }
        // A thunk of an island this layout has no segment for: an earlier link's, which the
        // first round drops ([`Stage::thunks`]).
        ids.extend(thunks[next..].iter().map(|it| it.1));
        self.text = Some(crate::stage::Text { code, ends, thunks, outside });
    }

    /// Makes the thunks the placed image's far branches need ([module docs](self)), and in a
    /// full link's first round removes those of an earlier link it no longer uses. Whether it
    /// changed anything, so the image is placed again.
    pub(crate) fn thunks(&mut self) -> Result<bool, LinkError> {
        if self.arch() != Arch::Aarch64 || self.plan.indexed_code {
            return Ok(false);
        }
        let first = !self.thunked;
        self.thunked = true;
        // alloc: once per placement round of an AArch64 image, a few dozen regions
        let regions = self.new.regions();
        let code = regions.iter().filter(|it| it.prot.exec);
        let low = code.clone().map(|it| it.addr).min().unwrap_or(0);
        let high = code.map(|it| it.addr + it.capacity).max().unwrap_or(0);
        // Nothing in the image is out of a branch's reach (with the planner's margin).
        let near = high.saturating_sub(low) < (REACH - MARGIN) as u64;
        if near && !(self.full && first) {
            return Ok(false);
        }
        let span = tracing::info_span!(
            target: "blaze",
            "link.thunks",
            full = self.full,
            near,
            code_span = high.saturating_sub(low),
            changed = tracing::field::Empty,
            islands = tracing::field::Empty,
        );
        let _enter = span.enter();
        let changed = if self.full {
            self.full_thunks(first, near, &regions)?
        } else {
            self.patch_thunks(&regions)?
        };
        span.record("changed", changed);
        span.record("islands", self.new.islands);
        Ok(changed)
    }

    fn full_thunks(
        &mut self,
        first: bool,
        near: bool,
        regions: &[Region],
    ) -> Result<bool, LinkError> {
        let arena = self.arena;
        let mut needs: FrameVec<'a, Need> = Vec::new_in(arena);
        let mut used: FrameVec<'a, PieceId> = Vec::new_in(arena);
        let text = self.text.take();
        if let (Some(text), false) = (&text, near) {
            let resolver = self.resolver(regions);
            // Each island's first and end address as placed: after its segment's last piece.
            let mut spans: FrameVec<'a, (u64, u64)> = Vec::with_capacity_in(text.ends.len(), arena);
            let mut thunk = 0;
            let mut start = 0;
            for (island, end) in text.ends.iter().enumerate() {
                let after = text.code[start..*end].last().map_or(0, |id| {
                    let piece = self.new.piece(*id).expect("live");
                    align_up_u64(piece.addr + piece.size, 4)
                });
                let (mut low, mut high) = (after, after);
                while let Some((_, id)) =
                    text.thunks.get(thunk).filter(|it| it.0 as usize == island)
                {
                    let addr = self.new.piece(*id).expect("live").addr;
                    (low, high) = (low.min(addr), high.max(addr + LEN));
                    thunk += 1;
                }
                spans.push((low, high));
                start = *end;
            }
            let mut planning = Planning { spans: &spans, needs: &mut needs, used: &mut used };
            let mut start = 0;
            for (segment, end) in text.ends.iter().enumerate() {
                // The island after its segment, else the one before it.
                let segment = segment as u32;
                let islands = [Some(segment), segment.checked_sub(1)];
                for id in &text.code[start..*end] {
                    let piece = self.new.piece(*id).expect("live");
                    for fixup in piece.fixups().iter().filter(|it| extends(it.apply)) {
                        let p = piece.addr + fixup.at;
                        let islands = islands.iter().flatten().copied();
                        self.plan_thunk(&resolver, &mut planning, piece.sym, fixup, p, islands)?;
                    }
                }
                start = *end;
            }
            // A piece outside the segments: the islands nearest it first, whichever side.
            let mut order: FrameVec<'a, u32> = Vec::with_capacity_in(spans.len(), arena);
            for id in text.outside.iter() {
                let piece = self.new.piece(*id).expect("live");
                for fixup in piece.fixups().iter().filter(|it| extends(it.apply)) {
                    let p = piece.addr + fixup.at;
                    order.clear();
                    order.extend(0..spans.len() as u32);
                    order.sort_by_key(|it| {
                        let (low, high) = spans[*it as usize];
                        (low.saturating_sub(p).max(p.saturating_sub(high)), *it)
                    });
                    let islands = order.iter().copied();
                    self.plan_thunk(&resolver, &mut planning, piece.sym, fixup, p, islands)?;
                }
            }
            self.new.islands = self.new.islands.max(text.ends.len() as u32);
        }
        needs.sort_unstable();
        needs.dedup();
        let class = self.new.class_id(&Class::Text);
        let mut changed = false;
        for need in needs.iter() {
            if let Some(id) = self.make_thunk(*need, class) {
                used.push(id);
                changed = true;
            }
        }
        if first {
            used.sort_unstable();
            used.dedup();
            let stale = text
                .iter()
                .flat_map(|it| it.thunks.iter().map(|(_, id)| *id))
                .filter(|id| used.binary_search(id).is_err());
            let stale = crate::arena::collect_in(stale, arena);
            for id in stale.iter() {
                self.unmake(*id);
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Plans the thunk a full link's branch at `p` takes when it is far: one of `islands`'
    /// (in the order tried) already placed in reach is used, else the first island whose
    /// thunks are all in reach gets one. None in reach: the writer names the branch
    /// ([`through`]).
    fn plan_thunk(
        &self,
        resolver: &Resolver<'_>,
        planning: &mut Planning<'_, 'a>,
        caller: Sym,
        fixup: &Fixup,
        p: u64,
        islands: impl Iterator<Item = u32> + Clone,
    ) -> Result<(), LinkError> {
        let Some((target, _)) = self.far(resolver, fixup, p) else { return Ok(()) };
        let addend = self.thunk_addend(caller, target, fixup)?;
        let reused = islands.clone().find_map(|island| {
            let sym = self.symbols.find(SymKey::Thunk(target, island, addend))?;
            let (id, _) = self.new.home(sym)?;
            let piece = self.new.piece(id)?;
            (piece.placed() && reaches(p, piece.addr, REACH - MARGIN)).then_some(id)
        });
        if let Some(id) = reused {
            planning.used.push(id);
            return Ok(());
        }
        let fits = |(low, high): (u64, u64)| {
            reaches(p, low, REACH - MARGIN) && reaches(p, high + LEN, REACH - MARGIN)
        };
        let mut islands = islands;
        if let Some(island) = islands.find(|it| fits(planning.spans[*it as usize])) {
            planning.needs.push(Need { target, island, addend });
        }
        Ok(())
    }

    fn patch_thunks(&mut self, regions: &[Region]) -> Result<bool, LinkError> {
        let arena = self.arena;
        let mut needs: FrameVec<'a, Need> = Vec::new_in(arena);
        let resolver = self.resolver(regions);
        for id in self.write.iter() {
            let Some(piece) = self.new.piece(id) else { continue };
            if !piece.placed() {
                continue;
            }
            for fixup in piece.fixups().iter().filter(|it| extends(it.apply)) {
                let p = piece.addr + fixup.at;
                let Some((target, to)) = self.far(&resolver, fixup, p) else { continue };
                let addend = self.thunk_addend(piece.sym, target, fixup)?;
                if through(self.symbols, &self.new, target, i64::from(addend), p).is_some() {
                    continue;
                }
                // This stage's thunk of it, made in an earlier round and placed since, in the
                // patch region: out of this branch's reach, as any other this stage could
                // make there would be. A branch within the planner's margin of its reach
                // still reaches its target, and the writer sends it there.
                let made = self.island.and_then(|island| {
                    placed(self.symbols, &self.new, SymKey::Thunk(target, island, addend))
                });
                if let Some(thunk) = made {
                    if reaches(p, to, REACH) {
                        continue;
                    }
                    return Err(self.no_thunk(piece, fixup, target, p, to, thunk));
                }
                needs.push(Need { target, island: u32::MAX, addend });
            }
        }
        if needs.is_empty() {
            return Ok(false);
        }
        let island = match self.island {
            Some(it) => it,
            None => {
                let it = self.new.islands;
                self.new.islands += 1;
                self.island = Some(it);
                it
            }
        };
        needs.sort_unstable();
        needs.dedup();
        let class = self.new.class_id(&Class::Patch);
        let mut changed = false;
        for need in needs.iter() {
            changed |= self.make_thunk(Need { island, ..*need }, class).is_some();
        }
        Ok(changed)
    }

    /// The refusal of an incremental stage whose branch at `p` in `piece`, written again where
    /// it is, goes to `target` at `to`, with the stage's thunk of it at `thunk`: both out of
    /// reach ([`Refusal::NoThunk`]).
    fn no_thunk(
        &self,
        piece: PieceRef<'_>,
        fixup: &Fixup,
        target: Sym,
        p: u64,
        to: u64,
        thunk: u64,
    ) -> LinkError {
        // alloc: error path
        let branch = format!(
            "{:?} at {:#x} in {}, a branch to {} at {to:#x}, {} away",
            fixup.apply,
            fixup.at,
            named_in(self.symbols, &[&self.new], self.writer, piece.sym),
            named_in(self.symbols, &[&self.new], self.writer, target),
            away(p, to),
        );
        // alloc: error path
        let what = format!(
            "{branch}: past its 128 MiB reach, in code this link keeps where it is, and the thunk this link \
             made of it in the patch region, at {thunk:#x}, is {} away: only a link of the image from scratch \
             puts a thunk in its reach",
            away(p, thunk),
        );
        refused(function_of(piece), Refusal::NoThunk { branch }, what)
    }

    /// The target a branch at `p` reaches (its PLT stub for an import) and where the branch
    /// goes, when that is past what the planner lets it reach; `None` for one in reach, or one
    /// no thunk serves (a weak name nothing defines, an import with no stub), which the writer
    /// reports.
    fn far(&self, resolver: &Resolver<'_>, fixup: &Fixup, p: u64) -> Option<(Sym, u64)> {
        let (target, s) = if self.through_plt(fixup) {
            let stub = self.plt_stub(fixup.target)?;
            (stub, self.new.addr_of(stub)?)
        } else {
            match resolver.value(fixup.target) {
                Ok(Value::Image(at)) => (fixup.target, at),
                _ => return None,
            }
        };
        let to = s.wrapping_add(fixup.addend as u64);
        (!reaches(p, to, REACH - MARGIN)).then_some((target, to))
    }

    /// A far branch's addend as a thunk's key holds it.
    fn thunk_addend(&self, caller: Sym, target: Sym, fixup: &Fixup) -> Result<i32, LinkError> {
        i32::try_from(fixup.addend).map_err(|_| {
            let piece = self.new.home(caller).and_then(|(id, _)| self.new.piece(id));
            error(
                piece.and_then(function_of),
                // alloc: error path
                format!(
                    "{:?} at {:#x} in {}: a branch to {} with addend {:#x}, past a thunk's 32-bit addend, \
                     is out of its 128 MiB reach",
                    fixup.apply,
                    fixup.at,
                    named_in(self.symbols, &[&self.new], self.writer, caller),
                    named_in(self.symbols, &[&self.new], self.writer, target),
                    fixup.addend,
                ),
            )
        })
    }

    /// The thunk `need` names, made in `class` when there is none: its id when made.
    fn make_thunk(&mut self, need: Need, class: crate::state::ClassId) -> Option<PieceId> {
        let Need { target, island, addend } = need;
        let sym = self.sym(SymKey::Thunk(target, island, addend));
        if self.new.home(sym).is_some() {
            return None;
        }
        let addend = i64::from(addend);
        let fixups = self.fixup_slice([
            Fixup { at: 0, apply: Apply::A64Page21, target, addend, minus: None },
            Fixup { at: 4, apply: Apply::A64Lo12 { shift: 0 }, target, addend, minus: None },
        ]);
        let piece = Self::piece_in(sym, class, 4, LEN, fixups, What::Thunk { target, island });
        Some(self.make(&piece, Content::Bytes(&BYTES)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(pieces: &[(u64, u64)]) -> Vec<usize> {
        let mut out = Vec::new(); // alloc: test
        segments(pieces.iter().copied(), &mut out);
        out
    }

    #[test]
    fn segments_end_before_a_piece_that_would_pass_their_span() {
        const MIB: u64 = 1 << 20;
        assert_eq!(cut(&[]), [0]);
        assert_eq!(cut(&[(16, 10), (16, 10)]), [2], "small code is one segment");
        // 40 + 40 passes 64 MiB: the second starts a segment.
        assert_eq!(cut(&[(16, 40 * MIB), (16, 40 * MIB), (16, 20 * MIB)]), [1, 3]);
        // A 200 MiB function between two small ones: an island on each side of it.
        assert_eq!(cut(&[(16, 16), (16, 200 * MIB), (16, 16)]), [1, 2, 3]);
    }

    #[test]
    fn a_distance_is_said_signed_in_mib() {
        // The dogfood report's branch: from 0x12ef3a5a4 back to 0x10002ca70, which printed as
        // `0xffffffffd10f24cc bytes away`.
        assert_eq!(away(0x1_2ef3_a5a4, 0x1_0002_ca70), "-751.0 MiB");
        assert_eq!(away(0x1_0002_ca70, 0x1_2ef3_a5a4), "+751.0 MiB");
        assert_eq!(away(0, 200 << 20), "+200.0 MiB");
        assert_eq!(away(1 << 20, (1 << 19) - 1), "-0.5 MiB");
        assert_eq!(away(8, 8), "+0.0 MiB");
    }

    #[test]
    fn the_thunk_is_adrp_add_br_through_x16() {
        assert_eq!(BYTES[..4], 0x9000_0010u32.to_le_bytes());
        assert_eq!(BYTES[8..], 0xd61f_0200u32.to_le_bytes());
    }
}
