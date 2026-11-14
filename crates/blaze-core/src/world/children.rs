//! A child of the calling process, as its kernel has it, as a reference model.
//!
//! A child runs, then exits with a status and is a zombie until it is reaped; once reaped it is
//! no child any more, and every call about it fails with `ECHILD`. The rule a reap rests on:
//! a child's exit is reported once, by the call that reaps it, and only that call gives its
//! status. A wait for the exit that is told `WNOWAIT` looks without reaping and leaves the
//! child a zombie for that call; one that is not reaps it itself, and its status goes to that
//! wait. So a reap that waits first and reaps after must wait with `WNOWAIT`, or the reap after
//! finds no child and the exit is never reported (`wait/reaping.rs`).
//!
//! The model has no clock: when the child exits relative to the calls about it is the
//! caller's to say, by calling [`Child::exit`] between them.

#![blaze::sans_io]

/// Where a child is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Life {
    /// It runs.
    Running,
    /// It exited with this status and is not yet reaped: a zombie.
    Exited(i32),
    /// It was reaped, or was never a child.
    Reaped,
}

/// One child of the calling process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Child {
    pub life: Life,
}

/// What `ECHILD` says: no child by that pid, or one reaped already.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoChild;

impl Child {
    /// `wait4(WNOHANG)`: reaps it if it has exited, and gives its status; `None` while it runs.
    pub fn reap_now(&mut self) -> Result<Option<i32>, NoChild> {
        match self.life {
            Life::Running => Ok(None),
            Life::Exited(status) => {
                self.life = Life::Reaped;
                Ok(Some(status))
            }
            Life::Reaped => Err(NoChild),
        }
    }

    /// The child exits with `status`, where it runs; nothing otherwise.
    pub fn exit(&mut self, status: i32) {
        match self.life {
            Life::Running => self.life = Life::Exited(status),
            Life::Exited(_) | Life::Reaped => {}
        }
    }

    /// What a wait for its exit finds when it looks: `true` where it has exited, and then,
    /// without `keep` (no `WNOWAIT`), the wait reaps it; `false` where it runs; [`NoChild`]
    /// where there is no child.
    pub fn waited(&mut self, keep: bool) -> Result<bool, NoChild> {
        match self.life {
            Life::Running => Ok(false),
            Life::Exited(_) => {
                if !keep {
                    self.life = Life::Reaped;
                }
                Ok(true)
            }
            Life::Reaped => Err(NoChild),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(life: Life) -> Child {
        Child { life }
    }

    /// A reap reports a zombie once, with its status; a running child is left running; no
    /// child is `ECHILD`, and so is the same child reaped again.
    #[test]
    fn a_reap_reports_an_exit_once() {
        let mut running = child(Life::Running);
        assert_eq!(running.reap_now(), Ok(None));
        assert_eq!(running.life, Life::Running);

        let mut zombie = child(Life::Exited(7));
        assert_eq!(zombie.reap_now(), Ok(Some(7)));
        assert_eq!(zombie.life, Life::Reaped);
        assert_eq!(zombie.reap_now(), Err(NoChild));

        assert_eq!(child(Life::Reaped).reap_now(), Err(NoChild));
    }

    /// A child exits once, from running; an exit after that changes nothing.
    #[test]
    fn a_child_exits_once_and_only_from_running() {
        let mut it = child(Life::Running);
        it.exit(3);
        assert_eq!(it.life, Life::Exited(3));
        it.exit(4);
        assert_eq!(it.life, Life::Exited(3));

        let mut reaped = child(Life::Reaped);
        reaped.exit(5);
        assert_eq!(reaped.life, Life::Reaped);
    }

    /// A wait with `keep` leaves a zombie for the reap; one without reaps it, and the reap
    /// after finds no child. A running child is not exited; no child is `ECHILD` either way.
    #[test]
    fn a_wait_reaps_only_without_keep() {
        let mut kept = child(Life::Exited(9));
        assert_eq!(kept.waited(true), Ok(true));
        assert_eq!(kept.life, Life::Exited(9));
        assert_eq!(kept.reap_now(), Ok(Some(9)));

        let mut taken = child(Life::Exited(9));
        assert_eq!(taken.waited(false), Ok(true));
        assert_eq!(taken.life, Life::Reaped);
        assert_eq!(taken.reap_now(), Err(NoChild));

        for keep in [true, false] {
            let mut running = child(Life::Running);
            assert_eq!(running.waited(keep), Ok(false));
            assert_eq!(running.life, Life::Running);
            assert_eq!(child(Life::Reaped).waited(keep), Err(NoChild));
        }
    }
}
