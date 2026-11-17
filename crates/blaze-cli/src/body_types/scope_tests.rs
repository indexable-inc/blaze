//! A body job's check scopes over a large crate list, in a frame of the size a task really gets
//! (`bstd::mem::TASK_FRAME`, the request's own frame too): tempo's 1,072 crates, 2026-11-17
//! 00:34, "the allocator refused 12448 bytes aligned to 8 for `(usize, ..::Seen)`" (a panic of
//! the process, after 4.5 s) because each file's scopes were made in the request's frame, which
//! never frees.

use std::alloc::Global;

use blaze_own::Erased;
use blaze_world::Side;
use blaze_world::check_scope::{ScopeError, check_scope_in};
use blaze_world::crate_list::CrateGraph;
use bstd::mem::{Frame, TASK_FRAME, Tag};

use super::owner_wants;

/// More crates than tempo's, each depending on the one before: crate `i` sees `i + 1` crates.
const CRATES: usize = 1_500;

/// One scope over [`CRATES`] crates at its largest: the marks (`n` bytes), the work list (`8n`),
/// the readers (16 a crate seen), with room for alignment: 37,508 bytes, under 40 KiB.
const ONE_SCOPE: usize = 40 << 10;

/// A deep dependency chain of [`CRATES`] target-side library crates, no sysroot.
struct Chain;

impl CrateGraph<'static> for Chain {
    fn len(&self) -> usize {
        CRATES
    }
    fn name(&self, _at: usize) -> &'static str {
        "chain"
    }
    fn side(&self, _at: usize) -> Side {
        Side::Target
    }
    fn proc_macro(&self, _at: usize) -> bool {
        false
    }
    fn toolchain(&self, _at: usize) -> bool {
        false
    }
    fn refused(&self, _at: usize) -> bool {
        false
    }
    fn deps(&self, at: usize) -> impl Iterator<Item = usize> + '_ {
        at.checked_sub(1).into_iter()
    }
    fn sysroot(&self) -> [Option<usize>; 4] {
        [None; 4]
    }
}

/// The shape before (body_types.rs `prepare` up to 2026-11-17): both of an owner's scopes made
/// in the one frame the command keeps, nothing released. Over a deep chain of 1,500 crates
/// that is about 76 MB, past a task frame's 64 MB: refused, and the refusal is now a typed
/// [`ScopeError::Memory`] naming the owner (rule 20), where it was a panic of the process.
/// Should print nothing; on failure: "the old shape's scopes fit a task frame" or the panic.
#[test]
fn every_owners_scopes_in_one_unreleased_task_frame_are_refused_by_a_typed_error() {
    let frame = Frame::with_reserve(TASK_FRAME, Tag::Other).expect("a task frame");
    let erased = Erased::new(&frame);
    let mut kept = std::vec::Vec::new_in(Global);
    let mut refused = None;
    'owners: for owner in 0..CRATES {
        // `prepare`'s two asks of each owner: its wants, then the scope it keeps.
        for _ask in 0..2 {
            match check_scope_in(&Chain, owner, false, erased.handle()) {
                Ok(scope) => kept.push(scope),
                Err(error) => {
                    refused = Some((owner, error));
                    break 'owners;
                }
            }
        }
    }
    let Some((owner, error)) = refused else { panic!("the old shape's scopes fit a task frame: {} bytes used", frame.used()) };
    assert!(matches!(error, ScopeError::Memory { owner: "chain", .. }), "owner {owner}: {error}");
    assert!(owner > 0, "the first owner's scope alone was refused: {error}");
}

/// The shape now: each owner's wants through [`owner_wants`] in the task's scratch frame,
/// released per owner; the scope it keeps on the heap, freed when its file's job is made. Every
/// owner of the chain gets its scope, every crate it sees is wanted, and the frame's high-water
/// mark is one scope's (under [`ONE_SCOPE`], 40 KiB), not the sum (about 38 MB for one ask an
/// owner). Before the change there was no release, and the same calls were refused as above.
/// Should print nothing; on failure the owner, and the frame's peak against its bound.
#[test]
fn a_deep_chain_of_owners_keeps_a_task_frame_at_one_scope() {
    let mut frame = Frame::with_reserve(TASK_FRAME, Tag::Other).expect("a task frame");
    let heap = blaze_own::testing::heap();
    let mut wanted = 0usize;
    for owner in 0..CRATES {
        let mut seen = 0usize;
        owner_wants(&Chain, owner, &mut frame, |_| seen += 1).unwrap_or_else(|error| panic!("owner {owner}: {error}"));
        assert_eq!(seen, owner + 1, "owner {owner} sees itself and every crate below it");
        wanted += seen;
        let kept = check_scope_in(&Chain, owner, false, heap.alloc()).unwrap_or_else(|error| panic!("owner {owner}'s kept scope: {error}"));
        assert_eq!(kept.crates().count(), owner + 1);
        assert_eq!(frame.used(), 0, "owner {owner}: the scratch frame keeps bytes after its scope");
    }
    assert_eq!(wanted, CRATES * (CRATES + 1) / 2);
    assert!(frame.peak() <= ONE_SCOPE, "the scratch frame's peak {} is past one scope's bound {ONE_SCOPE}", frame.peak());
}
