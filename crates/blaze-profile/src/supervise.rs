//! What keeps the profile history writing when a part of it fails: the history can never stop
//! silently (docs/profiling.md, "When the history loses rows").
//!
//! Until 2026-11-12 one panic in a flush (arrow's `byte array offset overflow`: a text column
//! of one batch past the 2 GiB its `i32` offsets address) ended the `blaze-history` thread;
//! the daemon went on building and every later build's rows were dropped with nothing said, so
//! "why did 6,700 steps rerun" on tempo had no rows to be answered from. Three parts, each a
//! pure function or a state machine with no IO and no clock of its own (rule sans-io-cores):
//!
//! - [`split`]: a table's rows made into batches, and a batch whose making panics made again
//!   as two halves, down to one row; a row that cannot be written alone is dropped and
//!   counted, with the table named. No batch's size has to be guessed right in advance.
//! - [`Dropped`]: the rows the history could not write, by table, since the daemon began:
//!   what `blaze daemon status` and each build's account show.
//! - [`Supervisor`]: what the history's thread does when a turn of it panics outside a flush:
//!   say it once, go on, and after too many in a short time stop, loudly, with a bound (rule
//!   bounded-progress).

use std::time::Duration;

/// The rows the history could not write, by table, since the daemon began.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    by_table: Vec<(&'static str, u64)>,
}

impl Dropped {
    /// `rows` more of `table` were dropped.
    pub fn add(&mut self, table: &'static str, rows: u64) {
        if rows == 0 {
            return;
        }
        match self.by_table.iter_mut().find(|it| it.0 == table) {
            Some(it) => it.1 += rows,
            None => self.by_table.push((table, rows)), // alloc: once a table that ever dropped
        }
    }

    /// Every table's rows dropped, summed.
    pub fn total(&self) -> u64 {
        self.by_table.iter().map(|it| it.1).sum()
    }

    /// Each table that dropped rows, in the order it first did, with how many.
    pub fn tables(&self) -> &[(&'static str, u64)] {
        &self.by_table
    }

    /// `spans 120, events 3`; none when nothing was dropped.
    pub fn line(&self) -> Option<String> {
        if self.by_table.is_empty() {
            return None;
        }
        // alloc: error path, a line of what was dropped
        let parts: Vec<String> =
            self.by_table.iter().map(|(table, rows)| format!("{table} {rows}")).collect();
        Some(parts.join(", "))
    }
}

/// What [`split`] made of a table's rows.
#[derive(Debug, PartialEq, Eq)]
pub struct Split<B> {
    /// The batches, in the rows' order.
    pub batches: Vec<B>,
    /// Rows in no batch: each could not be made into a batch even alone, or was in a range
    /// left after [`MOST_SPLITS`].
    pub dropped: usize,
    /// How many times a range was halved.
    pub splits: u32,
    /// What the first panic said.
    pub why: Option<String>,
}

/// The most halvings one [`split`] makes (rule 18): 64 is a batch of 65,536 rows halved down to
/// single rows four times over. Past it the ranges still failing are dropped whole, counted.
pub const MOST_SPLITS: u32 = 64;

/// What a caught panic said.
pub fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    match panic.downcast_ref::<&str>() {
        Some(it) => (*it).to_owned(), // alloc: error path
        None => match panic.downcast_ref::<String>() {
            Some(it) => it.clone(), // clone: error path
            None => "a payload that is no text".to_owned(), // alloc: error path
        },
    }
}

/// `rows` as batches of at most `most` rows, in order, made by `make`. A range whose `make`
/// panics (arrow panics when a text column's bytes pass 2 GiB) is made again as two halves,
/// so every row that fits a batch alone is written; a row that panics alone is dropped and
/// counted. An `Err` of `make` is no matter of size and is the error, as it was. No rows is
/// one batch of none, so every table still has its part.
pub fn split<T, B, E>(
    rows: &[T],
    most: usize,
    make: impl Fn(&[T]) -> Result<B, E>,
) -> Result<Split<B>, E> {
    let attempt = |range: &[T]| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| make(range)))
            .map_err(|panic| panic_text(&*panic))
    };
    // alloc: a flush's parts, once a flush
    let mut out = Split { batches: Vec::new(), dropped: 0, splits: 0, why: None };
    if rows.is_empty() {
        match attempt(rows) {
            Ok(batch) => out.batches.push(batch?),
            Err(why) => out.why = Some(why),
        }
        return Ok(out);
    }
    let most = most.max(1);
    // The ranges still to make, the first last.
    // alloc: a flush's ranges, once a flush
    let mut todo: Vec<(usize, usize)> = (0..rows.len())
        .step_by(most)
        .map(|start| (start, start.saturating_add(most).min(rows.len())))
        .rev()
        .collect();
    while let Some((start, end)) = todo.pop() {
        match attempt(&rows[start..end]) {
            Ok(batch) => out.batches.push(batch?),
            Err(why) => {
                if out.why.is_none() {
                    out.why = Some(why);
                }
                if end - start == 1 || out.splits >= MOST_SPLITS {
                    out.dropped += end - start;
                    continue;
                }
                out.splits += 1;
                let mid = start + (end - start) / 2;
                todo.push((mid, end));
                todo.push((start, mid));
            }
        }
    }
    Ok(out)
}

/// What the history's thread does after a turn of it panicked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Go on with the next turn. `say`: what to tell the daemon, the first time and each time
    /// the panic is another one; none for a repeat already said.
    Restart { say: Option<String> },
    /// Too many panics in too short a time: the thread ends, and this is what every later
    /// build and `blaze daemon status` say.
    GiveUp { say: String },
}

/// The supervisor of the history's thread: a state machine over the panics of its turns, on
/// the caller's clock.
///
/// The invariant: a panic never ends the thread silently. Each is counted; the first of each
/// text is said; the thread goes on unless more than [`Supervisor::MOST`] fell within
/// [`Supervisor::WITHIN`], and then it ends saying so.
#[derive(Clone, Debug, Default)]
pub struct Supervisor {
    /// Every panic since the daemon began.
    panics: u64,
    /// When each of the last [`Supervisor::MOST`] panics was.
    recent: Vec<Duration>,
    /// The last panic's text.
    last: Option<String>,
    gave_up: bool,
}

impl Supervisor {
    /// The most panics within [`Supervisor::WITHIN`] the thread goes on after.
    pub const MOST: usize = 8;
    /// The time [`Supervisor::MOST`] panics must not fall within.
    pub const WITHIN: Duration = Duration::from_secs(60);

    /// A turn panicked at `now` (on any monotonic clock), doing `doing`, saying `what`.
    pub fn panicked(&mut self, now: Duration, doing: &str, what: &str) -> Verdict {
        self.panics += 1;
        self.recent.retain(|at| now.saturating_sub(*at) < Self::WITHIN);
        self.recent.push(now); // alloc: at most `MOST` + 1 held
        let repeat = self.last.as_deref() == Some(what);
        self.last = Some(what.to_owned()); // alloc: error path
        if self.recent.len() > Self::MOST {
            self.gave_up = true;
            // alloc: error path
            return Verdict::GiveUp {
                say: format!(
                    "the profile history's thread (`blaze-history`) panicked {} times within {} s, the last {doing}: {what}; it has stopped, so no later build's rows are written and `blaze why`, `blaze slow` and `blaze debug sql` read a history that ends here: `blaze stop`, then build again",
                    self.recent.len(),
                    Self::WITHIN.as_secs(),
                ),
            };
        }
        let say = (!repeat).then(|| {
            // alloc: error path
            format!(
                "the profile history's thread (`blaze-history`) panicked {doing}: {what}; that turn's rows may be missing from the history, and the thread goes on ({} panics since the daemon began)",
                self.panics
            )
        });
        Verdict::Restart { say }
    }

    /// Every panic since the daemon began.
    pub fn panics(&self) -> u64 {
        self.panics
    }

    /// Whether the thread was told to end.
    pub fn gave_up(&self) -> bool {
        self.gave_up
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `make` that panics as arrow does when a batch holds more than `most` bytes of text.
    fn make(most: usize) -> impl Fn(&[usize]) -> Result<Vec<usize>, String> {
        move |rows: &[usize]| {
            let bytes: usize = rows.iter().sum();
            assert!(bytes <= most, "byte array offset overflow");
            Ok(rows.to_vec())
        }
    }

    /// The invariant: whatever the rows' sizes and the batch size, every row that fits a batch
    /// alone is in exactly one batch, in order, no batch holds more than fits, and each row
    /// that does not fit alone is counted dropped: rows written plus rows dropped is the rows
    /// given.
    #[hegel::test(test_cases = 300)]
    fn an_oversized_batch_is_split_and_every_row_that_fits_is_written_once(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let sizes: Vec<usize> = tc.draw(gs::vecs(gs::integers::<usize>().max_value(100)).max_size(60));
        let most_rows = 1 + tc.draw(gs::integers::<usize>().max_value(40));
        let most_bytes = tc.draw(gs::integers::<usize>().max_value(400));
        // The planted panics print as they are caught: the test's own noise.
        let made = split(&sizes, most_rows, make(most_bytes)).unwrap();
        let fit: Vec<usize> = sizes.iter().copied().filter(|it| *it <= most_bytes).collect();
        let written: Vec<usize> = made.batches.iter().flatten().copied().collect();
        // With no bound on halvings reached (60 rows are far under it), exactly the rows that
        // fit alone are written.
        assert!(made.splits < MOST_SPLITS);
        assert_eq!(written, fit);
        assert_eq!(written.len() + made.dropped, sizes.len());
        for batch in &made.batches {
            assert!(batch.len() <= most_rows && batch.iter().sum::<usize>() <= most_bytes);
        }
        assert_eq!(made.why.is_some(), made.splits > 0 || made.dropped > 0);
    }

    /// The control: without the halving (one batch of all the rows, as a flush made until
    /// 2026-11-12) the same rows panic and none is written.
    #[test]
    fn the_oracle_catches_a_flush_that_makes_one_batch_of_everything() {
        let sizes = vec![60usize; 10];
        let whole = std::panic::catch_unwind(|| make(100)(&sizes));
        let made = split(&sizes, usize::MAX, make(100));
        assert!(whole.is_err(), "one batch of 600 bytes is past the 100 a batch holds");
        let made = made.unwrap();
        assert_eq!(made.batches.len(), 10);
        assert_eq!((made.dropped, made.why.as_deref()), (0, Some("byte array offset overflow")));
    }

    #[test]
    fn a_row_too_large_alone_is_dropped_and_counted_and_an_error_is_no_split() {
        let made = split(&[10, 500, 10], 8, make(100)).unwrap();
        assert_eq!(made.batches, [vec![10], vec![10]]);
        assert_eq!(made.dropped, 1);
        // An error of the making is the table's error, never a reason to halve.
        let failed: Result<Split<Vec<usize>>, String> =
            split(&[1usize, 2, 3], 2, |_| Err("a column does not fit the schema".to_owned()));
        assert_eq!(failed.unwrap_err(), "a column does not fit the schema");
        // No rows is one batch of none.
        let none = split(&[], 8, make(100)).unwrap();
        assert_eq!(none.batches, [Vec::<usize>::new()]);
        let mut dropped = Dropped::default();
        dropped.add("spans", 120);
        dropped.add("events", 3);
        dropped.add("spans", 1);
        dropped.add("folded", 0);
        assert_eq!((dropped.total(), dropped.line().as_deref()), (124, Some("spans 121, events 3")));
        assert_eq!(Dropped::default().line(), None);
    }

    /// The invariant of the supervisor: a panic is said once, a repeat is counted and not
    /// said again, another panic is said, and the thread goes on.
    #[test]
    fn a_planted_panic_is_said_once_and_the_thread_goes_on() {
        let mut supervisor = Supervisor::default();
        let at = Duration::from_secs;
        let Verdict::Restart { say: Some(said) } =
            supervisor.panicked(at(1), "explaining build 4's misses", "byte array offset overflow")
        else {
            panic!("the first panic restarts the thread and is said");
        };
        assert!(said.contains("explaining build 4's misses") && said.contains("byte array offset overflow"), "{said}");
        assert_eq!(
            supervisor.panicked(at(100), "flushing", "byte array offset overflow"),
            Verdict::Restart { say: None }
        );
        assert!(matches!(
            supervisor.panicked(at(200), "flushing", "index out of bounds"),
            Verdict::Restart { say: Some(_) }
        ));
        assert_eq!((supervisor.panics(), supervisor.gave_up()), (3, false));
    }

    /// The bound (rule 18), and the control of "never silently": past [`Supervisor::MOST`]
    /// panics within [`Supervisor::WITHIN`] the thread is told to end, saying so; the same
    /// panics spread out never end it.
    #[test]
    fn a_thread_that_panics_every_turn_is_ended_loudly_and_a_slow_drip_is_not() {
        let mut storm = Supervisor::default();
        let mut verdicts = Vec::new();
        for turn in 0..=Supervisor::MOST as u64 {
            verdicts.push(storm.panicked(Duration::from_millis(turn), "flushing", "boom"));
        }
        assert!(verdicts[..Supervisor::MOST].iter().all(|it| matches!(it, Verdict::Restart { .. })));
        let Some(Verdict::GiveUp { say }) = verdicts.last() else {
            panic!("the panic past the bound ends the thread: {verdicts:?}");
        };
        assert!(say.contains("it has stopped") && say.contains("boom"), "{say}");
        assert!(storm.gave_up());
        let mut drip = Supervisor::default();
        for turn in 0..100u64 {
            let verdict = drip.panicked(Supervisor::WITHIN * turn as u32, "flushing", "boom");
            assert!(matches!(verdict, Verdict::Restart { .. }), "turn {turn}: {verdict:?}");
        }
    }
}
