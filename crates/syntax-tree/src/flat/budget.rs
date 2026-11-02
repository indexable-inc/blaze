//! What a tree's bytes count against: the [`Tag`] of the task that made it (a file's parse
//! runs as [`Tag::Parse`]), or [`Tag::Parse`] outside a task, so admission sees the trees a
//! workspace keeps as the bytes they are and evicts by them, not by a count of files.
//!
//! A block adds its bytes to its tag's line when made, one atomic add a tree: a tree is one
//! allocation ([`super::block`]), so this is as coarse as the counting gets. Frees gather per
//! thread and reach the line at [`FOLD_AT`] or when the thread ends; a line is never less than
//! what its live trees hold, only more by what threads gathered (frees run on another thread
//! from the parse, `syntax`'s dropper).
//!
//! A lazy node's part counts against its tree's tag, whichever task first walks into it
//! ([`charging`]).

use std::cell::Cell;

use blaze_mem::{frame, tag, Tag};

/// Bytes of frees a thread gathers for a tag before taking them off its line.
const FOLD_AT: usize = 256 << 10;

/// The frees the thread gathered and has not taken off each tag's line yet.
struct Freed([Cell<usize>; Tag::COUNT]);

impl Drop for Freed {
    fn drop(&mut self) {
        for (tag, freed) in Tag::ALL.into_iter().zip(&self.0) {
            tag::sub(tag, freed.replace(0));
        }
    }
}

thread_local! {
    static FREED: Freed = const { Freed([const { Cell::new(0) }; Tag::COUNT]) };
    /// The tag the thread's trees count against while [`charging`] runs.
    static CHARGING: Cell<Option<Tag>> = const { Cell::new(None) };
}

/// The tag a tree made now counts against.
pub(super) fn now() -> Tag {
    if let Ok(Some(tag)) = CHARGING.try_with(Cell::get) {
        return tag;
    }
    match frame::in_task() {
        true => frame::current(|it| it.tag()),
        false => Tag::Parse,
    }
}

/// A block of `bytes` counting against `tag` was made.
pub(super) fn made(tag: Tag, bytes: usize) {
    tag::add(tag, bytes);
}

/// A block of `bytes` counting against `tag` was freed.
pub(super) fn freed(tag: Tag, bytes: usize) {
    if bytes == 0 {
        return;
    }
    let full = FREED.try_with(|freed| {
        let gathered = &freed.0[tag.index()];
        let bytes = gathered.get() + bytes;
        if bytes < FOLD_AT {
            gathered.set(bytes);
            return 0;
        }
        gathered.set(0);
        bytes
    });
    // A thread being torn down has nothing gathered left: the bytes go straight to the line.
    tag::sub(tag, full.unwrap_or(bytes));
}

/// Runs `f` with the trees it makes counting against `tag`.
pub(super) fn charging<R>(tag: Tag, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Tag>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = CHARGING.try_with(|it| it.set(self.0));
        }
    }
    let _restore = Restore(CHARGING.try_with(|it| it.replace(Some(tag))).unwrap_or(None));
    f()
}

#[cfg(test)]
mod tests {
    use blaze_mem::{frame, tag, Tag};

    use crate::{FlatTreeBuilder, SyntaxKind};

    fn live(tag: Tag) -> u64 {
        tag::snapshot().get(tag).live
    }

    /// A tree made in a task counts its block's bytes against the task's tag (no other test of
    /// this crate makes a tree of [`Tag::Diagnostics`]: the tests run at once and a tag's count
    /// is the process's) until it is freed and the thread's frees are folded, at the latest when
    /// the thread ends.
    #[test]
    fn a_tree_counts_against_its_tasks_tag_until_freed() {
        let tag = Tag::Diagnostics;
        let before = live(tag);
        let held = std::thread::spawn(move || {
            let tree = frame::task(tag, |_| {
                let mut builder = FlatTreeBuilder::new();
                builder.start_node(SyntaxKind(1));
                for _ in 0..1000 {
                    builder.token(SyntaxKind(2), "ab");
                }
                builder.finish_node();
                builder.finish()
            });
            let size = tree.block.size() as u64;
            assert!(size >= 4000, "a word a token at least");
            assert_eq!(live(tag), before + size, "the block's bytes, added as it was made");
            drop(tree);
            live(tag)
        })
        .join()
        .expect("the thread ran");
        assert!(held >= before, "a line never drops below what its live trees hold");
        assert_eq!(live(tag), before, "the thread's frees, folded as it ended");
    }

    /// A lazy node's part, parsed outside any task, counts against its tree's tag: [`Tag::Mir`],
    /// which no other test of this crate makes a tree of.
    #[test]
    fn a_part_counts_against_its_trees_tag() {
        fn expand(
            kind: SyntaxKind,
            source: &crate::Text,
            range: crate::TextRange,
            _: u32,
        ) -> (crate::FlatTree, Vec<(crate::TextRange, Box<str>)>) {
            let mut builder = FlatTreeBuilder::sourced_range(source.clone(), range);
            builder.start_node(kind);
            builder.token(SyntaxKind(2), &source[range]);
            builder.finish_node();
            (builder.finish(), Vec::new())
        }
        let tag = Tag::Mir;
        std::thread::spawn(move || {
            let tree = frame::task(tag, |_| {
                let mut builder = FlatTreeBuilder::sourced("{body}".into());
                builder.start_node(SyntaxKind(1));
                builder.lazy(SyntaxKind(3), 6);
                builder.finish_node();
                builder.expand_with(expand, 0);
                builder.finish()
            });
            let before = live(tag);
            let part = tree.part(1).block.size() as u64;
            assert_eq!(live(tag), before + part, "the part's block, against the tree's tag");
        })
        .join()
        .expect("the thread ran");
    }
}
