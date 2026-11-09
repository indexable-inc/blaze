//! The barrier an [`crate::Effect::Flush`] of a journal's watch waits on where the system
//! numbers its journal: NTFS's USN journal, which the Windows driver reads for every
//! [`crate::Effect::Journal`] (`iocp/usn.rs`).
//!
//! A flush is marked with where the volume's journal ends (`NextUsn`, from
//! `FSCTL_QUERY_USN_JOURNAL`) read after the flush was asked, so after every change made before
//! it: NTFS writes a change's record as it makes the change. It is told once the watch has
//! told every record before that mark. Nothing is written for it, in the watched directory or
//! anywhere: the mark is the journal's own position, as watchman's cookie file is a position
//! made by writing one.
//!
//! A live read in flight waits for the next record of a reason it asks for, which may never
//! come (nothing changes after the mark, or what does is of a reason not asked for), so a flush
//! that waits asks for the read to be asked again without waiting ([`Decision::Hurry`]): such
//! a read returns at once, at the journal's end. A read without waiting that moves no further
//! while a flush waits short of its mark fails that flush, naming the mark and where the
//! reader stopped: bounded (rule 18), never a loop and never a sleep.
//!
//! A journal deleted and made again has a new ID and starts over: a mark of the old one is
//! passed once the reader has moved on (its batch telling the whole directory lost went
//! first), and a mark of a new one the reader has not reached yet waits until it reads that one
//! up to it.
//!
//! Sans-IO (rule 14): the driver tells it marks and reads and takes its decisions; it owns no
//! handle, thread or clock.

use std::fmt::Write as _;

use crate::Tag;

/// A position in a journal: the journal's ID (`UsnJournalID`, new each time it is made) and a
/// USN in it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct At {
    pub(crate) journal: u64,
    pub(crate) usn: i64,
}

/// What the driver does.
#[derive(PartialEq, Eq, Debug)]
pub(crate) enum Decision {
    /// Every record before the flush's mark is told: complete it with `Done::Flushed`.
    Told(Tag),
    /// A flush waits past where the reader is: a read in flight that waits for a record is
    /// cancelled and asked again without waiting ([`Barrier::hurried`]).
    Hurry,
    /// The flush can't be told: why, naming its mark and how far the reader got.
    Failed { tag: Tag, why: String },
}

/// A flush marked and not yet told.
struct Waiting {
    tag: Tag,
    mark: At,
    /// [`Barrier::moves`] as it was marked.
    moves: u64,
}

/// The flushes of one journal's watch.
#[derive(Default)]
pub(crate) struct Barrier {
    /// Every record before this has been told, of the journal it names; none until the watch
    /// has opened.
    told: Option<At>,
    /// How many times the reader moved to another journal (deleted and made again).
    moves: u64,
    /// In the order they were marked.
    waiting: Vec<Waiting>,
    decisions: Vec<Decision>,
}

impl Barrier {
    pub(crate) fn new() -> Barrier {
        // alloc: none until a flush waits; then once per watch, grown to the most waiting.
        Barrier::default()
    }

    /// Whether everything before `mark`, marked when the reader had moved `moves` times, is told.
    fn passed(&self, mark: At, moves: u64) -> bool {
        match self.told {
            None => false,
            Some(told) if told.journal == mark.journal => told.usn >= mark.usn,
            // The reader left the journal it was on for another: everything of the old one
            // was told lost before.
            Some(_) => self.moves > moves,
        }
    }

    /// The flush `tag`, marked at `mark`: told now if the reader has told everything before
    /// it, else waiting, and the reader hurried.
    pub(crate) fn mark(&mut self, tag: Tag, mark: At) {
        if self.passed(mark, self.moves) {
            self.decisions.push(Decision::Told(tag));
            return;
        }
        // alloc: grown once to the most flushes waiting at once (amortized).
        self.waiting.push(Waiting { tag, mark, moves: self.moves });
        self.decisions.push(Decision::Hurry);
    }

    /// A read was taken and what it named told: every record before `told` is told now.
    /// `stalled`: it was asked without waiting and moved no further.
    pub(crate) fn read(&mut self, told: At, stalled: bool) {
        if self.told.is_some_and(|it| it.journal != told.journal) {
            self.moves += 1;
        }
        self.told = Some(told);
        let mut at = 0;
        while at < self.waiting.len() {
            let Waiting { tag, mark, moves } = self.waiting[at];
            if self.passed(mark, moves) {
                self.waiting.remove(at);
                self.decisions.push(Decision::Told(tag));
            } else {
                at += 1;
            }
        }
        if stalled && !self.waiting.is_empty() {
            for Waiting { tag, mark, .. } in self.waiting.drain(..) {
                // alloc: an error path.
                let mut why = String::new();
                _ = write!(
                    why,
                    "a read of the USN journal without waiting stopped at USN {} of journal {:#x}, short of \
                     the flush's mark, USN {} of journal {:#x}: the journal ends before where the volume said it did",
                    told.usn, told.journal, mark.usn, mark.journal,
                );
                self.decisions.push(Decision::Failed { tag, why });
            }
        }
    }

    /// Whether a flush waits: the next read is asked for without waiting.
    pub(crate) fn hurried(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// The watch ended (`why`): every flush waiting fails, naming it.
    pub(crate) fn end(&mut self, why: &str) {
        for Waiting { tag, .. } in self.waiting.drain(..) {
            // alloc: an error path.
            let why = format!(
                "the watch ended before every change made before the flush was told: {why}"
            );
            self.decisions.push(Decision::Failed { tag, why });
        }
    }

    /// What the driver does now, in order.
    pub(crate) fn take_decisions(&mut self) -> std::vec::Drain<'_, Decision> {
        self.decisions.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const J: u64 = 0x1d9_0000;

    fn at(journal: u64, usn: i64) -> At {
        At { journal, usn }
    }

    fn take(barrier: &mut Barrier) -> Vec<Decision> {
        barrier.take_decisions().collect()
    }

    /// The transcript of a checkout's flushes: one marked behind the reader is told at once;
    /// ones marked ahead wait, hurry the reader, and are told, in order, as reads reach them.
    #[test]
    fn a_flush_is_told_once_the_reader_has_told_every_record_before_its_mark() {
        let mut barrier = Barrier::new();
        // Asked before the watch opened: nothing is told yet.
        barrier.mark(Tag(1), at(J, 100));
        assert_eq!(take(&mut barrier), [Decision::Hurry]);
        barrier.read(at(J, 100), false);
        assert_eq!(take(&mut barrier), [Decision::Told(Tag(1))]);
        assert!(!barrier.hurried());

        // Nothing changed since: the mark is where the reader is.
        barrier.mark(Tag(2), at(J, 100));
        assert_eq!(take(&mut barrier), [Decision::Told(Tag(2))]);

        // Changes made before two requests; a read that reaches only the first mark.
        barrier.mark(Tag(3), at(J, 160));
        barrier.mark(Tag(4), at(J, 240));
        assert_eq!(take(&mut barrier), [Decision::Hurry, Decision::Hurry]);
        assert!(barrier.hurried());
        barrier.read(at(J, 200), false);
        assert_eq!(take(&mut barrier), [Decision::Told(Tag(3))]);
        assert!(barrier.hurried(), "the next read is asked without waiting");
        barrier.read(at(J, 260), false);
        assert_eq!(take(&mut barrier), [Decision::Told(Tag(4))]);
        assert!(!barrier.hurried());
    }

    /// A read without waiting that stops short of a mark fails the flush, naming the mark and
    /// where the reader stopped, instead of reading again forever.
    #[test]
    fn a_reader_that_stops_short_of_a_mark_fails_the_flush() {
        let mut barrier = Barrier::new();
        barrier.read(at(J, 100), false);
        barrier.mark(Tag(1), at(J, 300));
        barrier.mark(Tag(2), at(J, 150));
        take(&mut barrier);
        barrier.read(at(J, 200), true);
        let decisions = take(&mut barrier);
        assert_eq!(decisions[0], Decision::Told(Tag(2)));
        match &decisions[1..] {
            [Decision::Failed { tag: Tag(1), why }] => {
                assert!(why.contains("USN 200") && why.contains("USN 300"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!barrier.hurried());
    }

    /// The journal deleted and made again: a mark of the old journal is passed once the reader
    /// moved to the new one; a mark of the new one waits until the reader reads it up to there.
    #[test]
    fn a_journal_made_again_passes_old_marks_and_waits_for_new_ones() {
        let (old, new) = (J, J + 7);
        let mut barrier = Barrier::new();
        barrier.read(at(old, 5_000), false);
        barrier.mark(Tag(1), at(old, 6_000));
        // Marked on the new journal while the reader still reads the old one.
        barrier.mark(Tag(2), at(new, 40));
        take(&mut barrier);
        barrier.read(at(old, 5_500), false);
        assert!(take(&mut barrier).is_empty());
        // The reader restarts on the new journal at its start.
        barrier.read(at(new, 10), false);
        assert_eq!(take(&mut barrier), [Decision::Told(Tag(1))]);
        barrier.read(at(new, 40), false);
        assert_eq!(take(&mut barrier), [Decision::Told(Tag(2))]);
    }

    /// The watch ends: every flush waiting fails, naming why; none is left.
    #[test]
    fn the_watch_ending_fails_every_flush_waiting() {
        let mut barrier = Barrier::new();
        barrier.read(at(J, 1), false);
        barrier.mark(Tag(1), at(J, 2));
        barrier.mark(Tag(2), at(J, 3));
        take(&mut barrier);
        barrier.end("the watched directory was moved");
        let decisions = take(&mut barrier);
        assert_eq!(decisions.len(), 2);
        assert!(
            decisions
                .iter()
                .all(|it| matches!(it, Decision::Failed { why, .. } if why.contains("moved")))
        );
        assert!(!barrier.hurried());
    }
}
