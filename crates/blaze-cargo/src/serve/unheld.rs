//! A daemon that exits once nothing holds it ([`VAR`]): a test run's or a script's, whose
//! clients may be killed (nextest's time cap, `kill -9`) before they could say `blaze stop`.
//!
//! What holds it is a connection open at its sockets: the API's (`blaze_api::server::Server::
//! connections`) and the launchers' (`Steps::launchers_connected`). Both are counted where the
//! kernel says a connection ended, so a client killed however it dies lets go of the daemon:
//! its end is closed by the kernel, and the daemon's receive on it gets nothing. Once no
//! connection has been open for its grace (on the daemon's clock, from the last close, or from
//! its start when nothing ever connected), the daemon stops as `blaze stop` stops it, writing
//! its image, and ends with [`blaze_core::exit::Exit::Unheld`]. A connection that opens inside
//! the grace ends the countdown; the next close starts a new one.
//!
//! A core: plain data on the caller's clock, no IO. The daemon's loop tells it, each turn, how
//! many connections are open ([`Unheld::look`]), and wakes at its [`Unheld::deadline`], so its
//! wait between turns names the grace's end as what it waits for.
//!
//! The invariant (`serve::tests::unheld`, a deterministic simulation of the whole daemon): the
//! daemon never stops while a connection is open; once the last one closes it stops no sooner
//! than its grace and within one turn of it; an open inside the grace cancels the stop.

use std::time::Duration;

/// The variable whoever starts the daemon sets to have it exit once nothing holds it: `on`
/// for a grace of [`GRACE`], or a whole number of seconds above 0 for that grace. Unset, the
/// daemon exits only as [`super::idle::exit_due`] says, or when told. A daemon setting
/// ([`super::settings::STARTED_WITH`]): read once at the start, and a command that sets
/// another value than its daemon's is refused, naming both.
pub(crate) const VAR: &str = "BLAZE_DAEMON_EXIT_UNHELD";

/// The grace `on` gives: longer than the gap between two commands of one script or test (a
/// test builds, runs what it built, edits and builds again), short enough that a run whose
/// processes were all killed leaves no daemon a minute later.
pub(crate) const GRACE: Duration = Duration::from_secs(20);

/// The grace [`VAR`]'s `value` asks for: `None` unset; an error naming the value and what the
/// variable takes for anything else.
pub(crate) fn told(value: Option<&str>) -> Result<Option<Duration>, String> {
    let Some(value) = value else { return Ok(None) };
    if value == "on" {
        return Ok(Some(GRACE));
    }
    match value.parse::<u64>() {
        Ok(secs) if secs > 0 => Ok(Some(Duration::from_secs(secs))),
        // alloc: an error's text, once a start
        _ => Err(format!(
            "{VAR}={value:?}: it takes `on` (a grace of {} s) or a whole number of seconds above 0",
            GRACE.as_secs()
        )),
    }
}

/// A safeguard taken out, for the tests' controls: always `Plant::None` outside them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant a bug"))]
#[cfg_attr(not(test), expect(blaze::unwired, reason = "wired: a planted-bug control only its tests make"))]
pub(crate) enum Plant {
    None,
    /// Stops at the first look with nothing held, with no grace.
    ExitsOnClose,
    /// An open inside the grace does not end the countdown.
    OpenKeepsCountdown,
}

/// The countdown of a daemon that exits once nothing holds it.
#[derive(Debug)]
pub(crate) struct Unheld {
    grace: Duration,
    /// Since when nothing held it: its start, or the look that first saw nothing open after
    /// one that saw a connection. `None` while one is open.
    since: Option<Duration>,
    pub(crate) plant: Plant,
}

impl Unheld {
    /// The countdown of a daemon started at `now`: a daemon nothing connects to within its
    /// grace stops too.
    pub(crate) const fn new(grace: Duration, now: Duration) -> Unheld {
        Unheld { grace, since: Some(now), plant: Plant::None }
    }

    pub(crate) const fn grace(&self) -> Duration {
        self.grace
    }

    /// A look at `now`, with `held` the connections open: `true` when the daemon stops now.
    /// Never while `held` is above 0.
    #[blaze::pure]
    pub(crate) fn look(&mut self, now: Duration, held: usize) -> bool {
        if held > 0 {
            if self.plant != Plant::OpenKeepsCountdown {
                self.since = None;
            }
            return false;
        }
        let since = *self.since.get_or_insert(now);
        self.plant == Plant::ExitsOnClose || now.saturating_sub(since) >= self.grace
    }

    /// When the grace under way ends, on the same clock: the loop's wait ends then, so it
    /// stops on time with nothing else to wake it. `None` while a connection is open.
    pub(crate) fn deadline(&self) -> Option<Duration> {
        self.since.map(|since| since.saturating_add(self.grace))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variable's values: `on` is the default grace, a number its seconds, and anything
    /// else (0, a fraction, a word) an error naming the variable and the value.
    #[test]
    fn the_variable_takes_on_or_whole_seconds_and_names_anything_else() {
        assert_eq!(told(None), Ok(None));
        assert_eq!(told(Some("on")), Ok(Some(GRACE)));
        assert_eq!(told(Some("5")), Ok(Some(Duration::from_secs(5))));
        for bad in ["0", "1.5", "yes", ""] {
            let error = told(Some(bad)).unwrap_err();
            assert!(error.contains(VAR) && error.contains(&format!("{bad:?}")), "{error}");
        }
    }
}
