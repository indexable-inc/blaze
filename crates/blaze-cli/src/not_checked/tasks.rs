//! TASKS not run because an input they read could not be made: the task half of not_checked.rs
//! beside its item half (items.rs), one mechanism (rule 30): the same "not checked because
//! <cause> failed: .." line.
//!
//! Such a task's answer is its record with one `work::NOT_RUN` finding whose one argument is the
//! cause's words (body_types.rs `Prepared::NotRun`, work.rs `not_run_record`); the input that
//! could not be made carries the finding that says why, in its own record, printed as itself.
//! A NOT_RUN finding is no error of its own (the tempo check printed one cause 14,489 times and
//! counted 14,500 errors): print.rs keeps each with its task's kind ([`keep`]) and draws none;
//! main.rs `close` prints one line a cause and kind ([`lines`]) and fails the command.

use blaze_drive::Kind;
use blaze_own::Vec;
use bstd::mem::Frame;

use crate::CliError;

/// One task not run, as print.rs kept it from its finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NotRun<'f> {
    /// What groups it: sorted first, so a cause's tasks are together.
    pub(crate) cause: &'f str,
    /// Its task's kind, as the drive names it (`Kind::name`).
    pub(crate) kind: &'static str,
}

/// The cause of a NOT_RUN finding's words, borrowed from the listed finding (its words live as
/// long as the list, `'f`) and pushed onto `into` with its task's `kind`: nothing is copied (the
/// tempo check kept 14,489 copies of one cause). Words with more than the one argument are a
/// typed refusal naming the rule (rule 3): never counted under some other cause.
pub(crate) fn keep<'f>(into: &mut Vec<NotRun<'f>, &'f Frame>, kind: Kind, words: &'f [u8]) -> Result<(), CliError> {
    let refused = CliError::Text { what: "a not-run finding whose words are not its one cause (blaze-cli work.rs `not_run_record`)" };
    if words.contains(&0x1f) {
        return Err(refused);
    }
    let Ok(cause) = core::str::from_utf8(words) else { return Err(refused) };
    into.try_reserve(1).map_err(|_| CliError::Text { what: "the command's frame has no room for one more task not run (blaze-cli not_checked/tasks.rs `keep`)" })?;
    into.push(NotRun { cause, kind: kind.name() });
    Ok(())
}

/// Each (cause, kind)'s line over `tasks`, in cause order, handed to `each` (the caller prints
/// it as a note): "not checked because <cause> failed: N <kind> task(s)". Returns how many
/// tasks were not run.
pub(crate) fn lines(tasks: &[NotRun<'_>], frame: &Frame, each: &mut dyn FnMut(&str) -> Result<(), CliError>) -> Result<usize, CliError> {
    let mut sorted: Vec<NotRun<'_>, &Frame> = Vec::with_capacity_in(tasks.len(), frame);
    sorted.extend(tasks.iter().copied());
    sorted.sort_unstable();
    let text = |_: core::fmt::Error| CliError::Text { what: "a not-run line's words" };
    let mut at = 0usize;
    while at < sorted.len() {
        let first = sorted[at];
        let end = sorted[at..].iter().position(|t| *t != first).map_or(sorted.len(), |n| at + n);
        let mut words = bstd::mem::String::new_in(frame);
        core::fmt::write(&mut words, format_args!("not checked because {} failed: {} {} task(s)", first.cause, end - at, first.kind)).map_err(text)?;
        each(words.as_str())?;
        at = end;
    }
    Ok(sorted.len())
}

#[cfg(test)]
mod tests {
    use super::{NotRun, keep, lines};
    use blaze_drive::Kind;

    /// Three bodies stopped by one cause and one by another: two lines, each cause once with
    /// its exact count, never a line a task. Prints "test ... ok". Control: print one line a
    /// task in `lines` (`end = at + 1`): five lines come out and the assertion fails.
    #[test]
    fn one_cause_is_one_line_with_its_count() {
        let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("a test frame");
        let mut kept: blaze_own::Vec<NotRun<'_>, &bstd::mem::Frame> = blaze_own::Vec::new_in(&frame);
        for cause in ["the crates' inputs", "the crates' inputs", "a lock", "the crates' inputs"] {
            keep(&mut kept, Kind::BodyCheck, cause.as_bytes()).expect("one cause");
        }
        let mut out = std::vec::Vec::new();
        let n = lines(&kept, &frame, &mut |line| {
            out.push(std::string::String::from(line));
            Ok(())
        })
        .expect("lines");
        let body = Kind::BodyCheck.name();
        assert_eq!(n, 4);
        assert_eq!(
            out,
            vec![format!("not checked because a lock failed: 1 {body} task(s)"), format!("not checked because the crates' inputs failed: 3 {body} task(s)")]
        );
        // Words of another form are refused, never counted.
        assert!(keep(&mut kept, Kind::BodyCheck, b"a\x1fb").is_err());
    }
}
