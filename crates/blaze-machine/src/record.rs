//! [`RecordHandler`]: deterministic transcript recorder wrapping any [`Handler`].
//!
//! Captures every `(request, outcome)` step in an explicit allocator (`blaze_own::Vec`), then
//! converts any recorded live or simulated run directly into:
//! - an in-memory zero-IO [`Script`] via [`RecordHandler::into_script`], or
//! - copy-pasteable Rust [`Script`] unit-test source via [`RecordHandler::emit_script_source`].

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{Alloc, Vec};

use crate::{CancelReason, Handler, Perform, Request, Script, Stopped};

/// One recorded `(request, outcome)` transition captured by [`RecordHandler`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TranscriptEntry<Req, Ans> {
    /// The wrapped handler answered `req` with `ans`.
    Answered { req: Req, ans: Ans },
    /// The wrapped handler delivered external cancellation/deadline `stopped` for `req`.
    Stopped { req: Req, stopped: Stopped },
}

/// Transparent [`Handler`] wrapper that forwards every request to an inner handler `H` while
/// recording the exact `(request, outcome)` transcript in an explicit caller allocator `A`.
pub struct RecordHandler<'h, H, Req, Ans, A: Allocator> {
    inner: &'h mut H,
    entries: Vec<TranscriptEntry<Req, Ans>, A>,
}

impl<'h, H, Req, Ans, A: Allocator> RecordHandler<'h, H, Req, Ans, A> {
    /// Wraps `inner` and stores recorded transcript entries in `alloc`.
    #[inline]
    pub fn new_in(inner: &'h mut H, alloc: A) -> Self {
        Self { inner, entries: Vec::new_in(alloc) }
    }

    /// Borrows all recorded transcript entries in execution order.
    #[inline]
    pub fn entries(&self) -> &[TranscriptEntry<Req, Ans>] {
        self.entries.as_slice()
    }

    /// Number of recorded request/outcome steps.
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Borrows the wrapped inner handler.
    #[inline]
    pub fn inner(&self) -> &H {
        self.inner
    }

    /// Mutably borrows the wrapped inner handler.
    #[inline]
    pub fn inner_mut(&mut self) -> &mut H {
        self.inner
    }

    /// Consumes the recorder and returns the recorded entries vector.
    #[inline]
    pub fn into_entries(self) -> Vec<TranscriptEntry<Req, Ans>, A> {
        self.entries
    }

    /// Produces a zero-IO replayable [`Script`] in `alloc` from the recorded transcript.
    pub fn into_script<'a>(&self, alloc: Alloc<'a>) -> Script<'a, Req, Ans>
    where
        Req: Clone,
        Ans: Clone,
    {
        let mut script = Script::new_in(alloc);
        for entry in self.entries.as_slice() {
            match entry {
                TranscriptEntry::Answered { req, ans } => {
                    script.expect_answer(req.clone(), ans.clone());
                }
                TranscriptEntry::Stopped { req, stopped } => {
                    script.expect_stop(req.clone(), *stopped);
                }
            }
        }
        script
    }

    /// Emits copy-pasteable Rust [`Script`] unit-test source reproducing the exact transcript.
    pub fn emit_script_source(&self, out: &mut impl fmt::Write) -> fmt::Result
    where
        Req: fmt::Debug,
        Ans: fmt::Debug,
    {
        writeln!(out, "let mut script = Script::new_in(alloc);")?;
        for entry in self.entries.as_slice() {
            match entry {
                TranscriptEntry::Answered { req, ans } => {
                    writeln!(out, "script.expect_answer({req:?}, {ans:?});")?;
                }
                TranscriptEntry::Stopped { req, stopped } => {
                    let ctor = match stopped.reason {
                        CancelReason::Cancelled => "Stopped::cancelled",
                        CancelReason::Deadline => "Stopped::deadline",
                    };
                    writeln!(
                        out,
                        "script.expect_stop({req:?}, {ctor}(GraceBudget {{ max_ns: {}, max_steps: {} }}));",
                        stopped.grace.max_ns, stopped.grace.max_steps
                    )?;
                }
            }
        }
        Ok(())
    }
}

/// A [`Request`] whose answer can be cloned or copied into an owned recorded value for
/// [`RecordHandler`] (supporting both owned answers and borrowed scratch-slice answers).
pub trait RecordableRequest: Request {
    /// Owned recorded form of [`Request::Answer`], kept in [`TranscriptEntry`].
    type Recorded: Clone;

    /// Extracts an owned recorded copy from one completed answer.
    fn record_answer(answer: &Self::Answer<'_>) -> Self::Recorded;
}

impl<H, Req, A> Handler<Req> for RecordHandler<'_, H, Req, Req::Recorded, A>
where
    H: Handler<Req>,
    Req: RecordableRequest + Clone,
    A: Allocator,
{
    type Error = H::Error;

    fn perform<Out>(
        &mut self,
        request: Req,
        deliver: impl FnOnce(Perform<Req::Answer<'_>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let req_copy = request.clone();
        let entries = &mut self.entries;
        self.inner.perform(request, |perf| match perf {
            Perform::Answered(ans) => {
                let recorded = Req::record_answer(&ans);
                entries.push(TranscriptEntry::Answered { req: req_copy, ans: recorded });
                deliver(Perform::Answered(ans))
            }
            Perform::Stopped(stopped) => {
                entries.push(TranscriptEntry::Stopped { req: req_copy, stopped });
                deliver(Perform::Stopped(stopped))
            }
        })
    }
}
