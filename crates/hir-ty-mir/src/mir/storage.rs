//! Which locals of a body may hold storage at the same time: what LLVM's stack colouring finds
//! from the lifetime markers rustc writes. Two locals that never do can share memory in the
//! frame.
//!
//! A local whose address is never taken holds a value from where it is assigned whole to where
//! it is last read (liveness), whatever its markers say: they are too coarse to share by (in
//! hir-ty's `hir_fmt` they had each of 724 locals live with 722 others at some point). A
//! borrowed local has storage where `StorageLive` and `StorageDead` say it
//! does (`MaybeStorageLive` of `rustc_mir_dataflow`), and all through the body when they do not
//! mention it or it is mentioned where they say it has none. So does a local a debugger is told
//! of: it reads the local's memory all through the local's scope, past its last read.
//!
//! Codegen asks this once per function. Everything but the result is in the call's frame, and
//! the sets are reused across blocks, so the result's two boxes are all it allocates.

use blaze_ids::{
    bits::{BitMatrix, Words, words_for},
    radix,
};
use blaze_mem::{Frame, Tag, frame};
use either::Either;

use super::{
    BasicBlockId, LocalId, MirBody, ProjectionElem, Rvalue, StatementKind, StoredPlace,
    TerminatorKind,
    framed::{Bits, bits_in},
    return_slot,
};

/// Whether two of a list of locals may hold storage at once, by their index in the list.
#[derive(Debug)]
pub struct StorageConflicts {
    /// The ones that have storage all through the body, as words.
    always: Box<[u64]>,
    /// Row `a` is `words` words from `a * words`.
    conflicts: Box<[u64]>,
    words: usize,
}

impl StorageConflicts {
    /// Whether the `a`th and the `b`th local may hold storage at once.
    pub fn conflict(&self, a: usize, b: usize) -> bool {
        let bit = |words: &[u64], at: usize| words[at / 64] & (1 << (at % 64)) != 0;
        a == b
            || bit(&self.always, a)
            || bit(&self.always, b)
            || bit(&self.conflicts[a * self.words..(a + 1) * self.words], b)
    }

    /// [`Self::conflict`] of the first `count` locals as a matrix in `frame`, for
    /// [`share_greedily`]: no local conflicts with itself there.
    pub fn matrix_in<'f>(&self, count: usize, frame: &'f Frame) -> BitMatrix<usize, Vec<u64, &'f Frame>> {
        let mut data = Vec::with_capacity_in(count * words_for(count), frame);
        data.resize(count * words_for(count), 0);
        let mut matrix = BitMatrix::from_words(count, count, data);
        for a in 0..count {
            for b in (a + 1)..count {
                if self.conflict(a, b) {
                    matrix.insert(a, b);
                    matrix.insert(b, a);
                }
            }
        }
        matrix
    }
}

/// Locals put together by [`share_greedily`], each group to share one piece of the frame.
pub struct SharedSlots<'f> {
    /// The group of each member, in the order the members were given.
    pub group_of: Vec<u32, &'f Frame>,
    /// Each group's first member, by its place among the members: the group's largest, the
    /// one whose memory the group keeps.
    pub first: Vec<u32, &'f Frame>,
}

/// Groups `members` so that no two of a group may hold storage at once, greedily as LLVM's
/// stack colouring does: the largest first, ties by row, each into the first group none of
/// whose members conflicts with it. A member is a row of `conflicts` with its size. Whether one
/// more fits a group is one bit: a group keeps the rows of its members, or'ed.
pub fn share_greedily<'f, S: Words>(
    members: &[(usize, u64)],
    conflicts: &BitMatrix<usize, S>,
    frame: &'f Frame,
) -> SharedSlots<'f> {
    let count = members.len();
    let mut order: Vec<u32, &Frame> = Vec::with_capacity_in(count, frame);
    order.extend(0..count as u32);
    radix::sort_by_key(&mut order, |&at| {
        let (row, size) = members[at as usize];
        radix::pack_64(!size, row as u64)
    });
    let row_words = conflicts.row_words();
    let mut words = Vec::with_capacity_in(count * row_words, frame);
    words.resize(count * row_words, 0);
    let mut groups: BitMatrix<usize, Vec<u64, &Frame>> = BitMatrix::from_words(count, row_words * 64, words);
    let mut group_of = Vec::with_capacity_in(count, frame);
    group_of.resize(count, 0);
    let mut first = Vec::new_in(frame);
    for at in order {
        let row = members[at as usize].0;
        let group = match (0..first.len()).find(|&group| !groups.contains(group, row)) {
            Some(group) => group,
            None => {
                first.push(at);
                first.len() - 1
            }
        };
        groups.union_row_with(group, &conflicts.row(row));
        group_of[at as usize] = group as u32;
    }
    SharedSlots { group_of, first }
}

fn mentioned(place: &StoredPlace, f: &mut impl FnMut(LocalId)) {
    f(place.local);
    for elem in place.projection.as_slice() {
        if let ProjectionElem::Index(local) = elem {
            f(*local);
        }
    }
}

/// Whether `place` is its local's own memory, not memory it points to.
fn in_frame(place: &StoredPlace) -> bool {
    !matches!(place.projection.as_slice().first(), Some(ProjectionElem::Deref))
}

/// The blocks `kind` goes on to and unwinds to. A `SwitchInt`'s are read from its targets:
/// `successors` copies them into a `SmallVec` that spills past two.
fn next_blocks(kind: &TerminatorKind) -> impl Iterator<Item = BasicBlockId> + '_ {
    let next = match kind {
        TerminatorKind::SwitchInt { targets, .. } => {
            Either::Left(targets.all_targets().iter().copied())
        }
        kind => Either::Right(kind.successors().into_iter()),
    };
    next.chain(kind.unwind())
}

/// What a body's storage conflicts are, for the code generator's frame layout.
pub trait FrameStorage {
    /// Which of `locals` may hold storage at the same point of the body. Those of `scoped` hold
    /// it where their markers say, as borrowed ones do.
    fn frame_storage_conflicts(
        &self,
        locals: &[LocalId],
        scoped: impl Fn(LocalId) -> bool,
    ) -> StorageConflicts;
}

impl FrameStorage for MirBody<'_> {
    fn frame_storage_conflicts(
        &self,
        locals: &[LocalId],
        scoped: impl Fn(LocalId) -> bool,
    ) -> StorageConflicts {
        frame::task(Tag::Mir, |frame| conflicts_in(self, locals, &scoped, frame))
    }
}

fn conflicts_in<'f>(
    body: &MirBody<'_>,
    locals: &[LocalId],
    scoped: &impl Fn(LocalId) -> bool,
    frame: &'f Frame,
) -> StorageConflicts {
    let count = locals.len();
    let blocks = body.basic_blocks.len();
    let mut index = Vec::with_capacity_in(body.locals.len(), frame);
    index.resize(body.locals.len(), None);
    for (at, &local) in locals.iter().enumerate() {
        index[local.into_raw().into_u32() as usize] = Some(at);
    }
    let index_of = |local: LocalId| index[local.into_raw().into_u32() as usize];
    let block_index = |block: BasicBlockId| block.into_raw().into_u32() as usize;
    let set = || -> Bits<'f> { bits_in(count, frame) };

    // The locals with markers, and of the others the ones liveness tells of.
    let mut marked = set();
    let mut borrowed = set();
    for (at, &local) in locals.iter().enumerate() {
        if scoped(local) {
            borrowed.insert(at);
        }
    }
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            match &statement.kind {
                StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                    if let Some(at) = index_of(*local) {
                        marked.insert(at);
                    }
                }
                StatementKind::Assign(_, Rvalue::Ref(_, place)) if in_frame(place) => {
                    if let Some(at) = index_of(place.local) {
                        borrowed.insert(at);
                    }
                }
                _ => (),
            }
        }
    }
    // A local whose address is never taken holds a value only from where it is assigned
    // whole to where it is last read, whatever its markers say; one that is borrowed has
    // storage where its markers say, or all through the body when it has none.
    let mut always = set();
    always.insert_range(0..count);
    always.subtract(&marked);
    always.intersect_with(&borrowed);
    for local in body.entry_locals() {
        if let Some(at) = index_of(local) {
            always.insert(at);
        }
    }
    let mut tracked = set();
    tracked.insert_range(0..count);
    tracked.subtract(&borrowed);
    tracked.subtract(&always);
    marked.intersect_with(&borrowed);
    marked.subtract(&always);

    // Marked locals: the ones that may have storage where each block starts, for the
    // blocks reached.
    let mut entry: Vec<Option<Bits<'f>>, &Frame> = Vec::with_capacity_in(blocks, frame);
    entry.resize_with(blocks, || None);
    entry[block_index(body.start_block)] = Some(set());
    let mut worklist = Vec::with_capacity_in(blocks, frame);
    worklist.push(body.start_block);
    let mut live = set();
    while let Some(id) = worklist.pop() {
        let block = &body.basic_blocks[id];
        live.copy_from(entry[block_index(id)].as_ref().expect("a reached block"));
        for statement in &block.statements {
            match statement.kind {
                StatementKind::StorageLive(local) => {
                    if let Some(at) = index_of(local).filter(|&at| marked.contains(at)) {
                        live.insert(at);
                    }
                }
                StatementKind::StorageDead(local) => {
                    if let Some(at) = index_of(local).filter(|&at| marked.contains(at)) {
                        live.remove(at);
                    }
                }
                _ => (),
            }
        }
        let Some(terminator) = &block.terminator else { continue };
        for to in next_blocks(&terminator.kind) {
            let changed = match &mut entry[block_index(to)] {
                Some(state) => state.union_with(&live),
                state @ None => {
                    let mut reached = set();
                    reached.copy_from(&live);
                    *state = Some(reached);
                    true
                }
            };
            if changed {
                worklist.push(to);
            }
        }
    }

    // Tracked locals: the ones that hold a value where each block starts.
    let read = |live: &mut Bits<'f>, place: &StoredPlace| {
        mentioned(place, &mut |local| {
            if let Some(at) = index_of(local)
                && tracked.contains(at)
            {
                live.insert(at);
            }
        })
    };
    let write = |live: &mut Bits<'f>, place: &StoredPlace| {
        if place.projection.as_slice().is_empty() {
            if let Some(at) = index_of(place.local) {
                live.remove(at);
            }
        } else {
            read(live, place);
        }
    };
    let through_statement = |live: &mut Bits<'f>, kind: &StatementKind| match kind {
        StatementKind::Assign(place, _) => {
            write(live, place);
            let mut first = true;
            MirBody::visit_statement_places(kind, &mut |it| {
                // The first place is the assigned one.
                if !std::mem::take(&mut first) {
                    read(live, it);
                }
            });
        }
        kind => MirBody::visit_statement_places(kind, &mut |it| read(live, it)),
    };
    // What holds a value before a terminator, given what does where its successors start,
    // written into `live`.
    let through_terminator = |holding: &[Bits<'f>], kind: &TerminatorKind, live: &mut Bits<'f>| {
        live.clear();
        match kind {
            TerminatorKind::Call { destination, target, cleanup, .. } => {
                // The destination is written when the call returns, not when it unwinds.
                if let Some(target) = target {
                    live.union_with(&holding[block_index(*target)]);
                    write(live, destination);
                }
                if let Some(cleanup) = cleanup {
                    live.union_with(&holding[block_index(*cleanup)]);
                }
                // The last place is the destination.
                let mut places = 0;
                MirBody::visit_terminator_places(kind, &mut |_| places += 1);
                let mut seen = 0;
                MirBody::visit_terminator_places(kind, &mut |it| {
                    seen += 1;
                    if seen < places {
                        read(live, it);
                    }
                });
            }
            TerminatorKind::Return => {
                if let Some(at) = index_of(return_slot())
                    && tracked.contains(at)
                {
                    live.insert(at);
                }
            }
            _ => {
                for to in next_blocks(kind) {
                    live.union_with(&holding[block_index(to)]);
                }
                MirBody::visit_terminator_places(kind, &mut |it| read(live, it));
            }
        }
    };
    let mut holding: Vec<Bits<'f>, &Frame> = Vec::with_capacity_in(blocks, frame);
    holding.resize_with(blocks, set);
    let mut changed = true;
    while changed {
        changed = false;
        for (id, block) in body.basic_blocks.iter().rev() {
            match &block.terminator {
                Some(terminator) => through_terminator(&holding[..], &terminator.kind, &mut live),
                None => live.clear(),
            }
            for statement in block.statements.iter().rev() {
                through_statement(&mut live, &statement.kind);
            }
            if holding[block_index(id)] != live {
                holding[block_index(id)].copy_from(&live);
                changed = true;
            }
        }
    }

    // Every two locals with storage at the same point conflict. A marked local mentioned
    // where it may have no storage has it all through the body.
    let words = count.div_ceil(64);
    let mut matrix = Vec::with_capacity_in(count * words, frame);
    matrix.resize(count * words, 0u64);
    let all_pairs = |matrix: &mut [u64], at_once: &Bits<'f>| {
        for at in at_once.iter() {
            let row = &mut matrix[at * words..(at + 1) * words];
            for (word, &with) in row.iter_mut().zip(at_once.words()) {
                *word |= with;
            }
        }
    };
    // What is marked live before each statement and before the terminator, reused across
    // blocks and grown to the longest.
    let mut storage_at: Vec<Bits<'f>, &Frame> = Vec::new_in(frame);
    let mut storage = set();
    let mut after = set();
    let mut before = set();
    let mut at_once = set();
    for (id, block) in body.basic_blocks.iter() {
        let Some(reached) = &entry[block_index(id)] else { continue };
        storage.copy_from(reached);
        let statements = block.statements.len();
        if storage_at.len() <= statements {
            storage_at.resize_with(statements + 1, set);
        }
        for (at, statement) in block.statements.iter().enumerate() {
            storage_at[at].copy_from(&storage);
            match statement.kind {
                StatementKind::StorageLive(local) => {
                    if let Some(at) = index_of(local).filter(|&at| marked.contains(at)) {
                        storage.insert(at);
                    }
                }
                StatementKind::StorageDead(local) => {
                    if let Some(at) = index_of(local).filter(|&at| marked.contains(at)) {
                        storage.remove(at);
                    }
                }
                ref kind => MirBody::visit_statement_places(kind, &mut |place| {
                    mentioned(place, &mut |local| {
                        if let Some(at) = index_of(local)
                            && marked.contains(at)
                            && !storage.contains(at)
                        {
                            always.insert(at);
                        }
                    })
                }),
            }
            storage_at[at].union_with(&storage);
        }
        if let Some(terminator) = &block.terminator {
            MirBody::visit_terminator_places(&terminator.kind, &mut |place| {
                mentioned(place, &mut |local| {
                    if let Some(at) = index_of(local)
                        && marked.contains(at)
                        && !storage.contains(at)
                    {
                        always.insert(at);
                    }
                })
            });
        }
        storage_at[statements].copy_from(&storage);

        match &block.terminator {
            Some(terminator) => {
                let kind = &terminator.kind;
                through_terminator(&holding[..], kind, &mut after);
                // What the terminator writes holds a value while what it reads does.
                at_once.copy_from(&after);
                MirBody::visit_terminator_places(kind, &mut |place| read(&mut at_once, place));
                for to in next_blocks(kind) {
                    at_once.union_with(&holding[block_index(to)]);
                }
                at_once.union_with(&storage_at[statements]);
                all_pairs(&mut matrix[..], &at_once);
            }
            None => after.clear(),
        }
        for (at, statement) in block.statements.iter().enumerate().rev() {
            before.copy_from(&after);
            through_statement(&mut before, &statement.kind);
            at_once.copy_from(&before);
            at_once.union_with(&after);
            MirBody::visit_statement_places(&statement.kind, &mut |place| {
                read(&mut at_once, place)
            });
            at_once.union_with(&storage_at[at]);
            all_pairs(&mut matrix[..], &at_once);
            std::mem::swap(&mut after, &mut before);
        }
    }
    // alloc: the result, kept by codegen for the function's frame layout
    StorageConflicts { always: Box::from(always.words()), conflicts: Box::from(&matrix[..]), words }
}

#[cfg(test)]
mod tests {
    use blaze_ids::bits::BitMatrix;
    use blaze_mem::{Tag, frame};

    use super::share_greedily;

    #[test]
    fn locals_join_the_first_group_none_of_whose_members_they_conflict_with() {
        // Rows 0 and 1 conflict, so do 1 and 2; 0 and 2 may share. Row 3 conflicts with none.
        let mut conflicts = BitMatrix::new(4, 4);
        for (a, b) in [(0, 1), (1, 2)] {
            conflicts.insert(a, b);
            conflicts.insert(b, a);
        }
        frame::task(Tag::Mir, |scratch| {
            let shared =
                share_greedily(&[(0, 8), (1, 16), (2, 8), (3, 4)], &conflicts, scratch);
            // Largest first: row 1 opens a group, row 0 opens the next (ties by row), row 2
            // joins row 0's, row 3 joins row 1's, the first it fits.
            assert_eq!(&shared.group_of[..], &[1, 0, 1, 0]);
            assert_eq!(&shared.first[..], &[1, 0]);
        });
    }
}
