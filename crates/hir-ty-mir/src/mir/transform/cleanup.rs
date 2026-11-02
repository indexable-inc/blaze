//! rustc's `RemoveNoopLandingPads`, which runs before drop elaboration: a cleanup block that
//! does nothing on the way out but end storage, and leads only to resuming the unwind or to
//! such blocks, is not unwound to. The call, drop or assert just goes on unwinding, and the
//! storage states of its scopes are never seen by the coroutine transform there.

use blaze_hash::HashSet;
use blaze_mem::{Frame, Tag, frame};

use crate::mir::{
    BasicBlockId, MirBody, StatementKind, TerminatorKind,
    framed::{Bits, bits_in, block_id, block_index},
};

/// The cleanup blocks of `body` that do nothing but go on unwinding: only storage markers,
/// fake reads and nops, ending in resuming the unwind or going to such a block.
pub fn noop_landing_pads(body: &MirBody<'_>) -> HashSet<BasicBlockId> {
    frame::task(Tag::Mir, |frame| {
        let noop = noop_in(body, frame);
        // alloc: codegen's set, kept for the function it builds
        let mut set = HashSet::with_capacity_and_hasher(noop.count(), Default::default());
        set.extend(noop.iter().map(block_id));
        set
    })
}

/// [`noop_landing_pads`] as a set of block indices in `frame`.
fn noop_in<'f>(body: &MirBody<'_>, frame: &'f Frame) -> Bits<'f> {
    let mut noop: Bits<'f> = bits_in(body.basic_blocks.len(), frame);
    loop {
        let mut grew = false;
        for (id, block) in body.basic_blocks.iter() {
            if !block.is_cleanup || noop.contains(block_index(id)) {
                continue;
            }
            let quiet = block.statements.iter().all(|it| {
                matches!(
                    it.kind,
                    StatementKind::StorageLive(_)
                        | StatementKind::StorageDead(_)
                        | StatementKind::FakeRead(_)
                        | StatementKind::Nop
                )
            });
            let ends = match block.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::UnwindResume) => true,
                Some(TerminatorKind::Goto { target }) => noop.contains(block_index(*target)),
                _ => false,
            };
            if quiet && ends {
                noop.insert(block_index(id));
                grew = true;
            }
        }
        if !grew {
            return noop;
        }
    }
}

pub(in crate::mir) fn remove_noop_landing_pads(body: &mut MirBody<'_>, frame: &Frame) {
    let noop = noop_in(body, frame);
    for (_, block) in body.basic_blocks.iter_mut() {
        let Some(terminator) = &mut block.terminator else { continue };
        if let Some(unwind) = terminator.kind.unwind_mut()
            && unwind.is_some_and(|it| noop.contains(block_index(it)))
        {
            *unwind = None;
        }
    }
}
