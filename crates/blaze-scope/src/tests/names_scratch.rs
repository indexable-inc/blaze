//! One path walk's glob lookups share one scratch (lane RP4, piece D): [`path_in`] threads the
//! caller's [`ThroughScratch`] through every lexical layer and segment, where each lookup
//! through globs made its own ([`blaze_surface::through`]: 7 vectors and a map grown from empty
//! per call). The oracle: every answer equals [`path`]'s, over every walk of the names_cells
//! family from every module of both crates (globs of globs in them); the floor: a warm pass over
//! the same walks grows no buffer of the held scratch.

use blaze_id::ItemKey;
use blaze_mem::{Frame, Tag};
use blaze_own::{AVec, Erased};
use blaze_surface::{Namespace, ThroughScratch};

use super::names_cells::{BEFORE, Side, family};
use crate::names::{Written, path, path_in};
use crate::names_record::{LoadNames, RecordNames};

/// The side's records, borrowed.
struct Borrowed<'s>(&'s [&'static [u8]]);

impl<'a> LoadNames<'a> for Borrowed<'_> {
    fn load(&self, krate: usize) -> Option<&'a [u8]> {
        self.0.get(krate).copied()
    }
}

/// Expected line: `test tests::names_scratch::a_walk_reuses_one_scratch ... ok`, printing
/// `NAMES SCRATCH: walks N, answers equal N, scratches 1, growth on the warm pass 0 (floor 0)`.
#[test]
fn a_walk_reuses_one_scratch() {
    let frame = Frame::new(Tag::Nameres).unwrap_or_else(|e| panic!("the test's frame: {e:?}"));
    let erased = Erased::new(&frame);
    let g = erased.handle();
    let before = Side::solve(&BEFORE);
    let world = RecordNames::new(&before.keys, Borrowed(&before.bytes), g);
    let mut modules: AVec<'_, ItemKey> = AVec::new_in(g);
    for krate in 0..before.keys.len() {
        modules.extend(before.record(krate).modules());
    }
    let walks = family();
    let mut scratch = ThroughScratch::new_in(g);
    let pass = |scratch: &mut ThroughScratch<'_>| {
        let (mut n, mut equal) = (0u64, 0u64);
        for module in modules.iter().copied() {
            for (kind, segments) in walks.iter() {
                for ns in [Namespace::Types, Namespace::Values] {
                    let written = Written { kind: *kind, segments: segments.as_slice() };
                    let fresh = path(&world, module, ns, written, g);
                    let held = path_in(&world, module, ns, written, scratch);
                    n += 1;
                    if fresh == held {
                        equal += 1;
                    } else {
                        std::println!("NAMES SCRATCH: {kind:?} {segments:?} from {module:?}: {fresh:?} != {held:?}");
                    }
                }
            }
        }
        (n, equal)
    };
    let (n, equal) = pass(&mut scratch);
    let cold = scratch.capacities();
    let (_, warm_equal) = pass(&mut scratch);
    let grown = cold.iter().zip(scratch.capacities().iter()).filter(|(a, b)| a != b).count();
    std::println!("NAMES SCRATCH: walks {n}, answers equal {equal}, scratches 1, growth on the warm pass {grown} (floor 0)");
    assert_eq!(equal, n, "a walk in a held scratch answered otherwise");
    assert_eq!(warm_equal, n, "a warm walk in a held scratch answered otherwise");
    assert!(scratch.finds() > 0, "no walk went through a glob: the fixture tests nothing");
    assert_eq!(grown, 0, "the warm pass grew the scratch: a walk allocated");
}
