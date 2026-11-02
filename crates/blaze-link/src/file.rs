//! The end of a stage: the writer's synthetic regions and headers, then the byte ranges of
//! the file a commit writes, and which of the old file's ranges they overwrite (for a
//! rollback).
//!
//! A full link writes the file as runs of at most [`RUN`] bytes, built in parallel, which the
//! commit submits together. An incremental link writes what changed: each piece it wrote whose
//! bytes differ from before, each jump-table cell it set (8 bytes, not the cells array), the
//! zero length after the call frame entries a patch adds in its code region, the synthetic
//! regions' changed blocks and the changed headers, with ranges closer than [`GAP`]
//! merged into one write. Every range's bytes are copied once, from the regions' pages
//! straight into the write. The undo is only ranges: the state the history keeps holds the
//! old bytes, read when a rollback needs them ([`restore`]).

use std::sync::Arc;

use blaze_graph::par;
use blaze_ids::radix;
use smallvec::SmallVec;

use crate::arena::{FrameVec, collect_in};
use crate::cow::CowBytes;
use crate::{LinkError, Refusal};
use crate::model::{Class, Delta, Pages, Placed, Region, Writer};
use crate::stage::{Stage, error, refused};
use crate::state::{Space, State, What};
use crate::write::{Changes, next_synthesis};

/// What a stage leaves for its commit.
pub(crate) struct Finished {
    pub state: State,
    pub full: bool,
    pub len: u64,
    /// Ranges of the file and their bytes, by offset, none overlapping.
    pub writes: Vec<(u64, Vec<u8>)>,
    /// The ranges of the old file the writes overwrite, clipped to its length: empty for a
    /// full link, whose undo is the whole old file.
    pub undo: Vec<(u64, u64)>,
    pub old_len: u64,
    /// The encoded message for running programs, which only a commit into the code memory
    /// gives them ([`crate::Image::commit_into`]).
    pub message: Option<Vec<u8>>,
}

/// The longest write of a full link: large enough to keep a device's queue full in few
/// writes, small enough that the writes of one file run side by side.
const RUN: u64 = 8 << 20;
/// Ranges of an incremental link closer than this are one write, the bytes between included.
const GAP: u64 = 4096;
/// The block a synthetic region is compared in.
const BLOCK: usize = 4096;

/// Where each byte of a state's file comes from: its regions in the file and the writer's
/// parts, each sorted by offset, and the slots the writer encodes, laid over the regions.
/// Parts are copied last, so they win where they overlap anything.
struct Layout<'s> {
    spaces: SmallVec<[(u64, u64, &'s CowBytes); 32]>,
    encoded: &'s [(u64, u64)],
    parts: SmallVec<[(u64, Part<'s>); 16]>,
}

/// A byte range of the file outside the regions: a writer's part, in pages it may share with
/// the parts before, or a debug section the core made ([`crate::model::File::debug`]).
#[derive(Clone, Copy)]
enum Part<'s> {
    Writer(&'s CowBytes),
    Debug(&'s [u8]),
}

impl Part<'_> {
    fn len(self) -> usize {
        match self {
            Part::Writer(bytes) => bytes.len(),
            Part::Debug(bytes) => bytes.len(),
        }
    }

    /// `self[at..at + out.len()]` copied into `out`.
    fn copy_to(self, at: usize, out: &mut [u8]) {
        match self {
            Part::Writer(bytes) => bytes.copy_to(at, out),
            Part::Debug(bytes) => out.copy_from_slice(&bytes[at..at + out.len()]),
        }
    }
}

impl<'s> Layout<'s> {
    fn of(state: &'s State) -> Layout<'s> {
        let mut spaces: SmallVec<[_; 32]> = SmallVec::new();
        spaces.extend(state.spaces.iter().filter_map(|space| {
            let offset = space.offset.filter(|_| !space.zero)?;
            let len = space.region.size.min(space.bytes.len() as u64);
            (len > 0).then_some((offset, len, &space.bytes))
        }));
        radix::sort_by_key(&mut spaces, |it| it.0);
        let mut parts: SmallVec<[_; 16]> = SmallVec::new();
        parts.extend(file_parts(state).filter(|it| it.1.len() != 0));
        radix::sort_copy_by_key(&mut parts, |it| it.0);
        Layout { spaces, encoded: &state.encoded, parts }
    }

    /// The file's bytes in `at..at + out.len()` copied into `out`, which is zeros.
    fn copy(&self, at: u64, out: &mut [u8]) {
        let end = at + out.len() as u64;
        let first = self.spaces.partition_point(|it| it.0 + it.1 <= at);
        for &(offset, len, bytes) in self.spaces[first..].iter().take_while(|it| it.0 < end) {
            let from = offset.max(at);
            let to = (offset + len).min(end);
            bytes.copy_to((from - offset) as usize, &mut out[(from - at) as usize..(to - at) as usize]);
        }
        let first = self.encoded.partition_point(|it| it.0 + 8 <= at);
        for &(offset, value) in self.encoded[first..].iter().take_while(|it| it.0 < end) {
            let from = offset.max(at);
            let to = (offset + 8).min(end);
            out[(from - at) as usize..(to - at) as usize]
                .copy_from_slice(&value.to_le_bytes()[(from - offset) as usize..(to - offset) as usize]);
        }
        let first = self.parts.partition_point(|it| it.0 + it.1.len() as u64 <= at);
        for &(offset, part) in self.parts[first..].iter().take_while(|it| it.0 < end) {
            let from = offset.max(at);
            let to = (offset + part.len() as u64).min(end);
            part.copy_to((from - offset) as usize, &mut out[(from - at) as usize..(to - at) as usize]);
        }
    }

    fn bytes(&self, at: u64, len: u64) -> Vec<u8> {
        // alloc: one buffer per write, which the commit owns (`Finished::writes`)
        let mut out = vec![0; len as usize];
        self.copy(at, &mut out);
        out
    }

    /// Every range of the file anything is in, merged where closer than `gap`.
    fn extents(&self, gap: u64) -> Vec<(u64, u64)> {
        let mut all = Vec::with_capacity(self.spaces.len() + self.parts.len());
        all.extend(self.spaces.iter().map(|it| (it.0, it.1)));
        all.extend(self.parts.iter().map(|it| (it.0, it.1.len() as u64)));
        merge(all, gap)
    }
}

/// The byte ranges of `state`'s file outside its regions: the writer's parts, then the debug
/// sections, borrowed from the lists ([`crate::model::File::debug`]).
fn file_parts(state: &State) -> impl Iterator<Item = (u64, Part<'_>)> {
    let debug = &state.lists.debug;
    (state.parts.iter().map(|(at, bytes)| (*at, Part::Writer(bytes))))
        .chain(state.debug.iter().filter_map(|&(at, nth)| Some((at, Part::Debug(&debug.get(nth)?.1[..])))))
}

/// Whether two parts hold the same bytes: at once when they are the same bytes, as a debug
/// section no function changed is.
fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    std::ptr::eq(a, b) || a == b
}

/// Sorts ranges and merges those that overlap or are closer than `gap`.
fn merge(mut ranges: Vec<(u64, u64)>, gap: u64) -> Vec<(u64, u64)> {
    ranges.retain(|it| it.1 > 0);
    radix::sort_copy_by_key(&mut ranges, |&(at, len)| radix::pack_64(at, len));
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (at, len) in ranges {
        match merged.last_mut() {
            Some(last) if at <= last.0 + last.1 + gap => last.1 = last.1.max(at + len - last.0),
            _ => merged.push((at, len)),
        }
    }
    merged
}

/// Splits ranges into runs of at most [`RUN`] bytes, cut at multiples of it.
fn runs(ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::with_capacity(ranges.len());
    for &(mut at, len) in ranges {
        let end = at + len;
        while at < end {
            let cut = ((at / RUN) + 1) * RUN;
            let to = cut.min(end);
            out.push((at, to - at));
            at = to;
        }
    }
    out
}

/// The bytes of `ranges` of `layout`'s file, built in parallel.
fn fill(layout: &Layout<'_>, ranges: &[(u64, u64)]) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::with_capacity(ranges.len());
    par::extend(&mut out, ranges, |&(at, len)| (at, layout.bytes(at, len)));
    out
}

/// Every byte of `state`'s file, as runs of at most [`RUN`] bytes. The gaps between regions
/// are zeros in a file just truncated, so runs span them up to a page.
pub(crate) fn whole(state: &State) -> Vec<(u64, Vec<u8>)> {
    let layout = Layout::of(state);
    fill(&layout, &runs(&layout.extents(GAP)))
}

/// The writes that put back `state`'s file where a commit overwrote `ranges` of it (the whole
/// file when it was a full link): a rollback's.
pub(crate) fn restore(state: &State, full: bool, ranges: &[(u64, u64)]) -> Vec<(u64, Vec<u8>)> {
    if full {
        return whole(state);
    }
    fill(&Layout::of(state), ranges)
}

/// A region's placed bytes for [`Placed::contents`], borrowed when they are one slice. Those
/// that are not (a code region whose pages a link wrote, a writer's kept table, patched) go
/// to [`Placed::paged`] in their pages instead, and `contents` holds none of them: nothing is
/// copied.
fn contents(space: &Space) -> (&[u8], Option<Pages<'_>>) {
    if space.zero {
        return (&[], None);
    }
    let size = space.region.size.min(space.bytes.len() as u64) as usize;
    match space.bytes.slice(0, size) {
        Some(flat) => (flat, None),
        None => (&[], Some(Pages { bytes: &space.bytes, len: size })),
    }
}

/// The file offsets of the slots a writer encoded at `slots`' addresses ([`Writer::encode`]),
/// `spaces` in address order and placed in the file. A slot out of order, overlapping
/// another, or not wholly inside a region in the file is an error naming its address.
///
/// [`Writer::encode`]: crate::model::Writer::encode
fn slots_in_file(spaces: &[Space], slots: &[(u64, u64)]) -> Result<Vec<(u64, u64)>, LinkError> {
    let mut out = Vec::with_capacity(slots.len());
    let mut at = 0;
    let mut next = 0;
    for &(addr, value) in slots {
        if addr < next {
            // alloc: error path
            return Err(error(None, format!("the writer encoded the slot at {addr:#x} out of order or overlapping another")));
        }
        next = addr.saturating_add(8);
        let end = |it: &Space| it.region.addr + it.region.size.min(it.bytes.len() as u64);
        while spaces.get(at).is_some_and(|it| end(it) <= addr) {
            at += 1;
        }
        let space = spaces
            .get(at)
            .filter(|it| it.region.addr <= addr && next <= end(it))
            // alloc: error path
            .ok_or_else(|| error(None, format!("the writer encoded a slot at {addr:#x}, outside every placed region")))?;
        let (Some(offset), false) = (space.offset, space.zero) else {
            // alloc: error path
            return Err(error(None, format!("the writer encoded a slot at {addr:#x}, in a region not in the file")));
        };
        out.push((offset + addr - space.region.addr, value));
    }
    Ok(out)
}

/// The 8-byte ranges whose encoded slot differs between `old` and `new` (each sorted by file
/// offset): changed, added or no longer encoded, the last only where the new file still is.
fn changed_slots(old: &[(u64, u64)], new: &[(u64, u64)], len: u64, ranges: &mut Vec<(u64, u64)>) {
    let (mut i, mut j) = (0, 0);
    loop {
        let at = match (old.get(i), new.get(j)) {
            (None, None) => break,
            (Some(a), None) => {
                i += 1;
                a.0
            }
            (None, Some(b)) => {
                j += 1;
                b.0
            }
            (Some(a), Some(b)) if a.0 < b.0 => {
                i += 1;
                a.0
            }
            (Some(a), Some(b)) if b.0 < a.0 => {
                j += 1;
                b.0
            }
            (Some(a), Some(b)) => {
                i += 1;
                j += 1;
                if a.1 == b.1 {
                    continue;
                }
                b.0
            }
        };
        if at + 8 <= len {
            ranges.push((at, 8));
        }
    }
}

impl<'a> Stage<'a> {
    pub fn finish(mut self) -> Result<Finished, LinkError> {
        let changes = self.update_lists()?;
        let mut parts = self.parts()?;
        let arena = self.arena;
        self.new.synthesis = next_synthesis();
        let written = self.written();
        let same_regions = same_regions(&self.old.spaces, &parts.regions);
        let (old, new) = (&self.old.resolved, &self.new.resolved);
        let same_names = Arc::ptr_eq(&old.imports, &new.imports)
            && Arc::ptr_eq(&old.libraries, &new.libraries)
            && self.old.exports[..] == parts.exports[..];
        let from = self.old.synthesis;
        let changes = changes.filter(|_| from != 0);
        // alloc: once per stage, the ranges this stage wrote
        let pieces = merge(written.iter().map(|it| (it.0, it.1)).collect(), 0);
        let contents = collect_in(self.new.spaces.iter().map(contents), arena);
        let synthetic = {
            let refs = collect_in(contents.iter().map(|it| it.0), arena);
            let paged = collect_in(contents.iter().map(|it| it.1), arena);
            let resolver = self.resolver(&parts.regions);
            let symbol = |name: &str| resolver.address_of(name);
            let delta = delta(changes.as_ref(), from, same_names, same_regions, &pieces);
            let placed = parts.placed(&self, &refs, &paged, &symbol, delta.as_ref(), &[]);
            self.writer.synthesize(&placed)?
        };
        let mut homes = Vec::with_capacity_in(synthetic.len(), arena);
        for (class, bytes) in &synthetic {
            let at = self
                .new
                .space_of(class)
                // alloc: error path
                .ok_or_else(|| error(None, format!("the writer filled {class:?}, which the image has no region for")))?;
            let capacity = self.new.spaces[at].region.capacity;
            if bytes.len() as u64 > capacity {
                return Err(refused(
                    None,
                    Refusal::NoRoom { need: bytes.len() as u64, room: capacity },
                    // alloc: error path
                    format!(
                        "{class:?} is {} bytes and has room for {capacity}: link the image from scratch",
                        bytes.len()
                    ),
                ));
            }
            parts.regions[at].size = bytes.len() as u64;
            homes.push(at);
        }
        // What the file's writer is told was written: the pieces, and the blocks of the
        // synthetic regions that differ from the last link's, a kept table's shared pages
        // skipped unread. A synthetic region's placed bytes are all of its bytes.
        // alloc: none until two copies differ in a page, then their page ranges once per stage
        let mut scratch = Vec::new();
        let mut all = Vec::with_capacity(pieces.len() + homes.len());
        all.extend_from_slice(&pieces);
        if changes.is_some() && same_regions {
            for (at, (_, bytes)) in homes.iter().zip(&synthetic) {
                let Some(old) = self.old.spaces.get(*at) else { continue };
                let old = &old.bytes;
                let addr = self.new.spaces[*at].region.addr;
                bytes.changed_blocks(old, BLOCK, &mut scratch, |start, len| all.push((addr + start as u64, len as u64)));
                if old.len() > bytes.len() {
                    all.push((addr + bytes.len() as u64, (old.len() - bytes.len()) as u64));
                }
            }
        }
        let written_all = merge(all, 0);
        let (offsets, file, encoded) = {
            let mut refs = collect_in(contents.iter().map(|it| it.0), arena);
            let mut paged = collect_in(contents.iter().map(|it| it.1), arena);
            for (at, (_, bytes)) in homes.iter().zip(&synthetic) {
                (refs[*at], paged[*at]) = match bytes.slice(0, bytes.len()) {
                    Some(flat) => (flat, None),
                    None => (&[][..], Some(Pages::all(bytes))),
                };
            }
            let resolver = self.resolver(&parts.regions);
            let symbol = |name: &str| resolver.address_of(name);
            let delta = delta(changes.as_ref(), from, same_names, same_regions, &written_all);
            let placed = parts.placed(&self, &refs, &paged, &symbol, delta.as_ref(), &[]);
            let offsets = collect_in(parts.regions.iter().map(|it| self.writer.offset(&placed, it)), arena);
            let encoded = self.writer.encode(&placed)?;
            let placed = Placed { encoded: &encoded, ..placed };
            (offsets, self.writer.file(&placed)?, encoded)
        };
        drop(contents);
        if let Some(&(at, nth)) = file.debug.iter().find(|it| it.1 >= self.new.lists.debug.len()) {
            // alloc: error path
            return Err(error(None, format!("the writer laid debug section {nth} at {at:#x}, of no debug section")));
        }
        for (at, (_, bytes)) in homes.into_iter().zip(synthetic) {
            let space = &mut Arc::make_mut(&mut self.new.spaces)[at];
            space.region.size = bytes.len() as u64;
            space.bytes = bytes;
        }
        let in_file = |space: &Space, offset: Option<u64>| if space.zero { None } else { offset };
        if self.new.spaces.iter().zip(offsets.iter()).any(|(space, &offset)| space.offset != in_file(space, offset)) {
            for (space, offset) in Arc::make_mut(&mut self.new.spaces).iter_mut().zip(offsets) {
                space.offset = in_file(&*space, offset);
            }
        }
        self.new.encoded = if encoded.is_empty() {
            std::sync::Arc::default()
        } else {
            // alloc: once per stage whose writer encodes slots; the state shares them
            std::sync::Arc::new(slots_in_file(&self.new.spaces, &encoded)?)
        };
        self.new.file_len = file.len;
        // alloc: once per stage; the state shares the writer's parts
        self.new.parts = Arc::new(file.parts);
        // alloc: once per stage, a few entries; the state shares them
        self.new.debug = Arc::new(file.debug);
        // alloc: once per stage; the next stage compares its exports with these
        self.new.exports = Arc::new(parts.exports);
        let message = self.output.live.is_some().then(|| self.message());
        let writes = if self.full { whole(&self.new) } else { self.dirty(&written, &mut scratch) };
        let (undo, old_len) = match (self.old.laid_out, self.full) {
            // alloc: none, an empty Vec holds no buffer
            (false, _) => (Vec::new(), 0),
            // alloc: none, an empty Vec holds no buffer
            (true, true) => (Vec::new(), self.old.file_len),
            (true, false) => {
                let len = self.old.file_len;
                let mut undo = Vec::with_capacity(writes.len());
                undo.extend(
                    writes.iter().filter(|(at, _)| *at < len).map(|(at, bytes)| (*at, (bytes.len() as u64).min(len - at))),
                );
                (undo, len)
            }
        };
        Ok(Finished { full: self.full, len: self.new.file_len, writes, undo, old_len, message, state: self.new })
    }

    /// What this stage wrote that differs from the last link, as (address, length, region):
    /// each piece it wrote whose place or bytes changed, and each jump-table cell it set.
    fn written(&self) -> FrameVec<'a, (u64, u64, u32)> {
        let mut out = Vec::with_capacity_in(self.write.len(), self.arena);
        for id in self.write.iter() {
            let Some(piece) = self.new.piece(id) else { continue };
            let Some(space) = self.new.spaces.get(piece.region as usize) else { continue };
            if space.zero || piece.zero() {
                continue;
            }
            if piece.what() == What::Cells {
                out.extend(self.dirty_cells.iter().map(|slot| (piece.addr + u64::from(slot) * 8, 8, piece.region)));
                continue;
            }
            if piece.size == 0 {
                continue;
            }
            let same_place = !self.fresh.contains(id)
                && self.old.piece(id).is_some_and(|old| old.addr == piece.addr && old.region == piece.region);
            if same_place && self.old.bytes(id) == self.new.bytes(id) {
                continue;
            }
            out.push((piece.addr, piece.size, piece.region));
        }
        out
    }

    /// The ranges of the file an incremental link changes, and their bytes. A region whose
    /// offset in the file moved is written whole, so `written` needs no check of it. A
    /// synthetic region's and a writer's part's pages shared with the last link's are
    /// skipped unread; `scratch` holds the page ranges that may differ.
    fn dirty(&self, written: &[(u64, u64, u32)], scratch: &mut Vec<std::ops::Range<usize>>) -> Vec<(u64, Vec<u8>)> {
        let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(written.len() + self.new.spaces.len());
        for &(addr, len, region) in written {
            let Some(space) = self.new.spaces.get(region as usize) else { continue };
            let Some(offset) = space.offset else { continue };
            ranges.push((offset + addr - space.region.addr, len));
        }
        // The zero length after the call frame entries is no piece's, and a running program
        // checks it in the code memory (`Stage::message`), so it is written as code is.
        if let Some(end) = self.frames_end
            && let Some(space) = self.new.space_of(&Class::Patch).map(|it| &self.new.spaces[it])
            && let Some(offset) = space.offset
        {
            ranges.push((offset + end - space.region.addr, 4));
        }
        for (at, space) in self.new.spaces.iter().enumerate() {
            let Some(offset) = space.offset else { continue };
            let old = self.old.spaces.get(at).filter(|it| it.region.class == space.region.class);
            let size = space.region.size;
            match old {
                Some(old) if old.offset == Some(offset) => {
                    if old.region.size > size {
                        ranges.push((offset + size, old.region.size - size));
                    }
                    if matches!(space.region.class, Class::Synthetic(_)) {
                        let ranges = &mut ranges;
                        space.bytes.changed_blocks(&old.bytes, BLOCK, scratch, |at, len| ranges.push((offset + at as u64, len as u64)));
                    }
                }
                _ => ranges.push((offset, size.max(old.map_or(0, |it| it.region.size)))),
            }
        }
        if !std::sync::Arc::ptr_eq(&self.old.encoded, &self.new.encoded) {
            changed_slots(&self.old.encoded, &self.new.encoded, self.new.file_len, &mut ranges);
        }
        let mut old_parts = collect_in(file_parts(self.old), self.arena);
        radix::sort_copy_by_key(&mut old_parts, |it| it.0);
        for (offset, part) in file_parts(&self.new) {
            match (old_parts.binary_search_by_key(&offset, |it| it.0).map(|at| old_parts[at].1), part) {
                (Ok(Part::Writer(old)), Part::Writer(new)) => {
                    let ranges = &mut ranges;
                    new.changed_blocks(old, BLOCK, scratch, |at, len| ranges.push((offset + at as u64, len as u64)));
                }
                (Ok(Part::Debug(old)), Part::Debug(new)) if same_bytes(old, new) => {}
                (Ok(Part::Debug(old)), Part::Debug(new)) => changed_blocks(offset, old, new, &mut ranges),
                _ => ranges.push((offset, part.len() as u64)),
            }
        }
        fill(&Layout::of(&self.new), &merge(ranges, GAP))
    }
}

/// Whether no region moved or changed its room, nor its size but for a synthetic one, whose
/// size the writer makes: `old` the last link's, `new` this one's, by index.
fn same_regions(old: &[Space], new: &[Region]) -> bool {
    old.len() == new.len()
        && old.iter().zip(new).all(|(old, new)| {
            let old = &old.region;
            old.class == new.class
                && old.addr == new.addr
                && old.capacity == new.capacity
                && (old.size == new.size || matches!(new.class, Class::Synthetic(_)))
        })
}

/// The [`Delta`] of `changes`, the runs a stage's splices replaced, from synthesis `from`.
fn delta<'d>(
    changes: Option<&'d Changes<'_>>,
    from: u64,
    same_names: bool,
    same_regions: bool,
    written: &'d [(u64, u64)],
) -> Option<Delta<'d>> {
    let changes = changes?;
    Some(Delta {
        functions: &changes.functions,
        rebase: &changes.rebase,
        got: &changes.got,
        binds: &changes.binds,
        same_names,
        same_regions,
        written,
        from,
    })
}

/// Pushes each `BLOCK` of `new`, at file offset `offset`, that differs from `old`, the bytes the
/// last link wrote there.
fn changed_blocks(offset: u64, old: &[u8], new: &[u8], ranges: &mut Vec<(u64, u64)>) {
    for start in (0..new.len()).step_by(BLOCK) {
        let end = (start + BLOCK).min(new.len());
        if old.get(start..end) != Some(&new[start..end]) {
            ranges.push((offset + start as u64, (end - start) as u64));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::View;

    #[test]
    fn ranges_merge_across_small_gaps_only() {
        let merged = merge(vec![(100, 8), (0, 10), (8, 4), (5000, 1), (100, 0), (9000, 10)], GAP);
        assert_eq!(merged, [(0, 108), (5000, 4010)]);
        assert_eq!(merge(vec![(0, 10), (10 + GAP + 1, 1)], GAP), [(0, 10), (10 + GAP + 1, 1)]);
        assert_eq!(merge(vec![(0, 10), (10 + GAP, 1)], GAP), [(0, 11 + GAP)]);
    }

    #[test]
    fn a_changed_part_rewrites_only_its_changed_blocks() {
        let old = vec![7u8; 3 * BLOCK];
        let mut new = old.clone();
        new[BLOCK + 5] = 0;
        new.extend_from_slice(&[1, 2, 3]);
        let mut ranges = Vec::new();
        changed_blocks(100, &old, &new, &mut ranges);
        assert_eq!(ranges, [(100 + BLOCK as u64, BLOCK as u64), (100 + 3 * BLOCK as u64, 3)]);
        ranges.clear();
        changed_blocks(0, &new, &old, &mut ranges);
        assert_eq!(ranges, [(BLOCK as u64, BLOCK as u64)], "a shorter part rewrites none past its end");
    }

    #[test]
    fn runs_cut_at_run_boundaries() {
        assert_eq!(runs(&[(10, 20)]), [(10, 20)]);
        assert_eq!(runs(&[(RUN - 4, 8)]), [(RUN - 4, 4), (RUN, 4)]);
        let long = runs(&[(1, 3 * RUN)]);
        assert_eq!(long.len(), 4);
        assert_eq!(long.iter().map(|it| it.1).sum::<u64>(), 3 * RUN);
        assert!(long.windows(2).all(|it| it[0].0 + it[0].1 == it[1].0));
        assert!(long[1..].iter().all(|it| it.0 % RUN == 0));
    }

    /// A code region whose pages a link wrote goes to the writer in those pages, a region in
    /// one slice as that slice, and one that shrank as its first `size` bytes: none copied,
    /// nothing allocated.
    #[test]
    fn a_written_code_region_is_handed_over_in_its_pages() {
        let page = CowBytes::PAGE;
        let region = |size: u64| Region {
            class: Class::Text,
            addr: 0x1000,
            capacity: 4 * page as u64,
            size,
            prot: crate::model::Prot { read: true, write: false, exec: true },
            align: 16,
        };
        let bytes: Vec<u8> = (0..3 * page).map(|it| it as u8).collect();
        let written = || {
            let mut it = CowBytes::from_vec(bytes.clone());
            it.write(page + 7, &[0xcc; 4]);
            it
        };
        let spaces = [
            Space { region: region(3 * page as u64), bytes: CowBytes::from_vec(bytes.clone()), zero: false, offset: Some(0) },
            Space { region: region(3 * page as u64), bytes: written(), zero: false, offset: Some(0) },
            Space { region: region(2 * page as u64 + 5), bytes: written(), zero: false, offset: Some(0) },
        ];
        let handed = blaze_alloc_count::assert_at_most(0, "regions handed to the writer", || {
            [contents(&spaces[0]), contents(&spaces[1]), contents(&spaces[2])]
        });
        assert!(std::ptr::eq(handed[0].0.as_ptr(), spaces[0].bytes.slice(0, 1).unwrap().as_ptr()));
        assert_eq!((handed[0].0.len(), handed[0].1.is_none()), (3 * page, true));
        for (at, (flat, pages)) in handed.iter().enumerate().skip(1) {
            let pages = pages.unwrap_or_else(|| panic!("space {at} is in pages"));
            assert!(flat.is_empty() && std::ptr::eq(pages.bytes, &spaces[at].bytes), "space {at}");
            assert_eq!(pages.len, spaces[at].region.size as usize, "space {at}");
            let view = View::Pages(pages);
            let mut read = vec![0; view.len()];
            view.copy_to(0, &mut read);
            let mut want = bytes[..view.len()].to_vec();
            want[page + 7..page + 11].fill(0xcc);
            assert_eq!(read, want, "space {at}");
        }
    }

    /// The layout's copy against a reference that writes every region then every part into
    /// one buffer of the whole file.
    #[test]
    fn layout_reads_what_a_whole_buffer_holds() {
        let regions: Vec<(u64, CowBytes)> = vec![
            (0x1000, CowBytes::from_vec((0..300u32).map(|it| it as u8).collect())),
            (0x1200, CowBytes::from_vec(vec![7; 0x900])),
            (0x4000, CowBytes::from_vec(vec![9; 10])),
        ];
        let parts: Vec<(u64, CowBytes)> = vec![(0, CowBytes::from_vec(vec![1; 64])), (0x1100, CowBytes::from_vec(vec![2; 4]))];
        let debug = (0x5000u64, vec![3u8; 3]);
        let layout = Layout {
            spaces: regions.iter().map(|(at, bytes)| (*at, bytes.len() as u64, bytes)).collect(),
            encoded: &[],
            parts: (parts.iter().map(|(at, bytes)| (*at, Part::Writer(bytes))))
                .chain(std::iter::once((debug.0, Part::Debug(&debug.1[..]))))
                .collect(),
        };
        let mut reference = vec![0u8; 0x5003];
        for (at, bytes) in &regions {
            let mut all = vec![0; bytes.len()];
            bytes.copy_to(0, &mut all);
            reference[*at as usize..*at as usize + all.len()].copy_from_slice(&all);
        }
        for (at, bytes) in &parts {
            reference[*at as usize..*at as usize + bytes.len()].copy_from_slice(&bytes.contiguous());
        }
        reference[debug.0 as usize..debug.0 as usize + debug.1.len()].copy_from_slice(&debug.1);
        for (at, len) in [(0, 0x5003), (0x10f0, 0x20), (0x1fff, 2), (0x3000, 0x1001), (0x5002, 1), (0x5003, 5)] {
            let mut want = vec![0u8; len];
            let end = (at + len).min(reference.len());
            if at < end {
                want[..end - at].copy_from_slice(&reference[at..end]);
            }
            assert_eq!(layout.bytes(at as u64, len as u64), want, "{at:#x}+{len:#x}");
        }
        let extents = layout.extents(GAP);
        assert_eq!(extents, [(0, 0x1b00), (0x4000, 0x1003)]);
        let mut rebuilt = vec![0u8; 0x5003];
        for (at, bytes) in fill(&layout, &runs(&extents)) {
            rebuilt[at as usize..at as usize + bytes.len()].copy_from_slice(&bytes);
        }
        assert_eq!(rebuilt, reference);
    }
}
