//! [`Script`]: deterministic unit-test [`Handler`] that drives any [`crate::Machine`] from an
//! explicit list of expected requests and scripted answers (or injected cancellations) with zero
//! simulator and zero global heap allocation.

use core::fmt;

use blaze_own::{Alloc, Vec};

use crate::{Handler, Perform, Request, Stopped};

/// One expected step in a [`Script`].
#[derive(Debug)]
pub struct Expect<Req, Ans> {
    pub request: Req,
    pub outcome: Perform<Ans>,
}

impl<Req, Ans> Expect<Req, Ans> {
    /// Expects `request` and delivers `answer`.
    #[inline]
    pub const fn answer(request: Req, answer: Ans) -> Self {
        Self { request, outcome: Perform::Answered(answer) }
    }

    /// Expects `request` and injects external cancellation / deadline expiry `stopped`.
    #[inline]
    pub const fn stop(request: Req, stopped: Stopped) -> Self {
        Self { request, outcome: Perform::Stopped(stopped) }
    }
}

/// Typed mismatch failure produced by [`Script`], naming the exact step index.
#[derive(PartialEq, Eq, Debug)]
pub enum ScriptError<Req> {
    /// Machine emitted a request after the script had no more expected steps.
    UnexpectedRequest { step: usize, actual: Req },
    /// Machine emitted a different request than the script expected at `step`.
    Mismatch { step: usize, expected: Req, actual: Req },
    /// Machine terminated early while `remaining` scripted steps were never performed.
    UnconsumedSteps { step: usize, remaining: usize },
}

impl<Req: fmt::Debug> fmt::Display for ScriptError<Req> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScriptError::UnexpectedRequest { step, actual } => {
                write!(f, "script step {step}: unexpected request {actual:?} after end of script")
            }
            ScriptError::Mismatch { step, expected, actual } => {
                write!(f, "script step {step}: expected {expected:?}, got {actual:?}")
            }
            ScriptError::UnconsumedSteps { step, remaining } => {
                write!(f, "script ended at step {step} with {remaining} unconsumed step(s)")
            }
        }
    }
}

/// Deterministic scripted [`Handler`] backed by an explicit caller allocator (`blaze_own::Vec`).
pub struct Script<'a, Req, Ans> {
    steps: Vec<Option<Expect<Req, Ans>>, Alloc<'a>>,
    cursor: usize,
}

impl<'a, Req, Ans> Script<'a, Req, Ans> {
    /// Creates an empty script in `alloc`.
    #[inline]
    pub fn new_in(alloc: Alloc<'a>) -> Self {
        Self { steps: Vec::new_in(alloc), cursor: 0 }
    }

    /// Appends one expected `(request, answer)` step.
    #[inline]
    pub fn expect_answer(&mut self, request: Req, answer: Ans) {
        self.steps.push(Some(Expect::answer(request, answer)));
    }

    /// Appends one expected `(request, Stopped)` cancellation step.
    #[inline]
    pub fn expect_stop(&mut self, request: Req, stopped: Stopped) {
        self.steps.push(Some(Expect::stop(request, stopped)));
    }

    /// Number of steps executed so far.
    #[inline]
    pub const fn steps_run(&self) -> usize {
        self.cursor
    }

    /// Verifies that every scripted step was executed by the machine.
    #[inline]
    pub fn finish(&self) -> Result<(), ScriptError<Req>> {
        let remaining = self.steps.len().saturating_sub(self.cursor);
        if remaining == 0 {
            Ok(())
        } else {
            Err(ScriptError::UnconsumedSteps { step: self.cursor, remaining })
        }
    }
}

impl<Req, Ans> Handler<Req> for Script<'_, Req, Ans>
where
    Req: Request + PartialEq,
    for<'s> Ans: Into<Req::Answer<'s>>,
{
    type Error = ScriptError<Req>;

    fn perform<Out>(
        &mut self,
        request: Req,
        deliver: impl FnOnce(Perform<Req::Answer<'_>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let step = self.cursor;
        let Some(slot) = self.steps.get_mut(step) else {
            return Err(ScriptError::UnexpectedRequest { step, actual: request });
        };
        let expect = slot.take().expect("script cursor advances monotonically");
        self.cursor += 1;
        if expect.request != request {
            return Err(ScriptError::Mismatch { step, expected: expect.request, actual: request });
        }
        let perf = match expect.outcome {
            Perform::Answered(ans) => Perform::Answered(ans.into()),
            Perform::Stopped(s) => Perform::Stopped(s),
        };
        Ok(deliver(perf))
    }
}
