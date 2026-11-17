//! Completion cues follow a session's observed work, never the pane that happens to be selected.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cue {
    Done,
    Failed,
}

impl Cue {
    pub fn name(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "fail",
        }
    }
}

/// The busy session ids in the last authoritative snapshot. Scratch capacity is retained between frames.
#[derive(Default)]
pub struct Tracker {
    busy: Vec<u64>, // alloc: observed sessions for this window's lifetime
    next: Vec<u64>, // alloc: the next snapshot's busy ids, reused every frame
}

impl Tracker {
    /// A complete snapshot of `(session id, busy, failed)`. A busy session that disappears is forgotten, not finished.
    /// Several endings in one snapshot produce one cue, with failure taking priority.
    pub fn update(&mut self, snapshot: impl IntoIterator<Item = (u64, bool, bool)>) -> Option<Cue> {
        self.next.clear();
        let mut cue = None;
        for (sid, busy, failed) in snapshot {
            if busy {
                self.next.push(sid);
            } else if self.busy.binary_search(&sid).is_ok() {
                cue = cue.max(Some(if failed { Cue::Failed } else { Cue::Done }));
            }
        }
        self.next.sort_unstable();
        self.next.dedup();
        std::mem::swap(&mut self.busy, &mut self.next);
        cue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each frame has its current selection and the daemon's complete session snapshot. Selection never enters the
    /// production tracker; keeping it in the fixture lets the oracle catch the old focus-based implementation.
    fn completion_fixture(mut step: impl FnMut(u64, &[(u64, bool, bool)]) -> Option<Cue>) -> Result<(), usize> {
        use Cue::{Done, Failed};
        let frames: &[(u64, &[(u64, bool, bool)], Option<Cue>)] = &[
            (1, &[(1, false, false), (2, false, false)], None), // opening an idle history is silent
            (1, &[(1, true, false), (2, false, false)], None),
            (2, &[(1, true, false), (2, false, false)], None), // switching away from running work is silent
            (1, &[(1, true, false), (2, false, false)], None), // switching back is silent too
            (2, &[(1, false, false), (2, false, false)], Some(Done)), // background work actually ends
            (1, &[(1, false, false), (2, false, false)], None), // no duplicate on the next frame or selection
            (1, &[(1, true, false), (2, true, false)], None),
            (1, &[(1, false, false), (2, false, true)], Some(Failed)), // same-frame endings coalesce
            (2, &[(2, false, true)], None),
            (2, &[(2, true, false)], None),
            (1, &[], None), // a disconnected or vanished busy session did not finish
            (2, &[(2, false, false)], None), // reappearance is not a completion
            (2, &[(2, true, false)], None),
            (2, &[(2, false, false)], Some(Done)), // a new observed turn still completes normally
        ];
        for (i, (selected, snapshot, expected)) in frames.iter().enumerate() {
            if step(*selected, snapshot) != *expected {
                return Err(i);
            }
        }
        Ok(())
    }

    /// The invariant: only an observed busy-to-idle transition of the same present session emits a cue; pane changes,
    /// first snapshots, repeated snapshots and disappearance do not. Concurrent endings coalesce with failure priority.
    #[test]
    fn completion_tracks_sessions_through_navigation_and_disappearance() {
        let mut tracker = Tracker::default();
        assert_eq!(completion_fixture(|_, snapshot| tracker.update(snapshot.iter().copied())), Ok(()));
    }

    #[test]
    fn the_oracle_catches_focus_based_completion() {
        let mut was_front_busy = false;
        let result = completion_fixture(|selected, snapshot| {
            let front_busy = snapshot.iter().any(|&(sid, busy, _)| sid == selected && busy);
            let cue = (was_front_busy && !front_busy).then_some(Cue::Done);
            was_front_busy = front_busy;
            cue
        });
        assert_eq!(result, Err(2), "the old implementation rings when selection leaves running work");
    }
}
