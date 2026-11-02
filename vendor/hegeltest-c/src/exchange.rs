//! The suspension point between the engine and whoever drives it.
//!
//! The engine ([`crate::native::test_runner`]) is written as async code with
//! exactly one kind of await point: offering a test case's
//! [`DataSource`](crate::backend::DataSource) to its driver via
//! [`CaseExchange::offer`]. The engine future never schedules wakeups — it is
//! only ever resumed by its driver polling it again — so no executor is
//! involved anywhere: drivers poll with a no-op waker on their own thread,
//! one poll per test case (`hegel_next_test_case`) or in a loop to
//! completion (the test-only `drive`).
//!
//! The protocol is strict alternation. Polling the engine future either
//! returns `Ready` (the run is finished) or `Pending`, in which case the
//! engine has stored exactly one offered case in the exchange for the driver
//! to [`take`](CaseExchange::take). The driver must finish that case —
//! everything through `mark_complete` — before polling again: the next poll
//! resumes the engine, which immediately reads the case's outcome off its
//! handle.
//!
//! A run under external generation has a second kind of await point: asking
//! its driver for the next generation case as data, via
//! [`CaseExchange::request_proposal`]. A poll that suspends there stores no
//! case; the driver sees [`Handoff::Proposal`], must
//! [`provide`](CaseExchange::provide) the proposal, and polls again. After
//! running a case built from a proposal, the engine posts that case's
//! report to the exchange, where it waits for the driver to
//! [`take_report`](CaseExchange::take_report) it.

use alloc::boxed::Box;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use alloc::vec::Vec;

use crate::backend::DataSource;
use crate::control::{InternalError, hegel_internal_unwrap};
use crate::native::external::Next;
use crate::sys::sync::Mutex;

/// A data source handed across the exchange, one per test case.
pub(crate) type BoxedDataSource = Box<dyn DataSource + Send + Sync>;

/// What a suspended engine is waiting on: a case it offered for the driver
/// to run, or a proposal it asked the driver for.
pub(crate) enum Handoff {
    Case(BoxedDataSource),
    Proposal,
}

/// The external-generation request slot. `Requested` from the engine's
/// suspension until the driver provides, `Provided` until the engine's next
/// poll takes it.
enum ProposalSlot {
    Idle,
    Requested,
    Provided(Next),
}

/// One engine-to-driver handoff slot. See the module docs for the protocol.
pub(crate) struct CaseExchange {
    slot: Mutex<Option<BoxedDataSource>>,
    proposal: Mutex<ProposalSlot>,
    report: Mutex<Option<Vec<u8>>>,
}

impl CaseExchange {
    pub(crate) fn new() -> Self {
        CaseExchange {
            slot: Mutex::new(None),
            proposal: Mutex::new(ProposalSlot::Idle),
            report: Mutex::new(None),
        }
    }

    /// Ask the driver for the next generation case. The returned future
    /// marks the request and suspends; it resolves on the next poll with
    /// what the driver [`provide`](Self::provide)d. `Err` if the engine was
    /// resumed without one, which the driver ABI rules out.
    pub(crate) fn request_proposal(&self) -> RequestProposal<'_> {
        RequestProposal {
            exchange: self,
            requested: false,
        }
    }

    /// Whether the engine is suspended on a proposal the driver has not
    /// provided yet.
    pub(crate) fn awaiting_proposal(&self) -> bool {
        matches!(*self.proposal.lock(), ProposalSlot::Requested)
    }

    /// Hand the engine the proposal it requested. Returns `next` back if no
    /// request is outstanding.
    pub(crate) fn provide(&self, next: Next) -> Result<(), Next> {
        let mut slot = self.proposal.lock();
        if !matches!(*slot, ProposalSlot::Requested) {
            return Err(next);
        }
        *slot = ProposalSlot::Provided(next);
        Ok(())
    }

    /// Post the encoded report of a case built from a proposal, replacing
    /// any report the driver did not take.
    pub(crate) fn post_report(&self, report: Vec<u8>) {
        *self.report.lock() = Some(report);
    }

    /// Take the report posted since the last take, if any.
    pub(crate) fn take_report(&self) -> Option<Vec<u8>> {
        self.report.lock().take()
    }

    /// What the engine suspended on: the case it offered, else a proposal
    /// request. `Err` if it suspended on neither, which the alternation
    /// protocol rules out.
    pub(crate) fn take_handoff(&self) -> Result<Handoff, InternalError> {
        if let Some(ds) = self.slot.lock().take() {
            return Ok(Handoff::Case(ds));
        }
        if self.awaiting_proposal() {
            return Ok(Handoff::Proposal);
        }
        crate::control::hegel_internal_error!("the engine suspended without offering a test case")
    }

    /// Yield `ds` to the driver. The returned future stores `ds` in the
    /// exchange and suspends; by the alternation protocol it resolves on the
    /// next poll, which the driver performs only once the case is complete.
    pub(crate) fn offer(&self, ds: BoxedDataSource) -> Offer<'_> {
        Offer {
            exchange: self,
            ds: Some(ds),
        }
    }

    /// Take the case the engine just offered. `Err` if the engine suspended
    /// without offering one, which the alternation protocol rules out — a
    /// bug in the engine, surfaced by the driver as a run-level error
    /// instead of a panic.
    #[cfg(any(test, feature = "__bench"))]
    pub(crate) fn take(&self) -> Result<BoxedDataSource, InternalError> {
        let taken = self.slot.lock().take();
        Ok(hegel_internal_unwrap!(
            taken,
            "the engine suspended without offering a test case"
        ))
    }
}

impl Default for CaseExchange {
    fn default() -> Self {
        Self::new()
    }
}

/// Future returned by [`CaseExchange::offer`]: stores the data source and
/// returns `Pending` on the first poll, `Ready` on the next.
pub(crate) struct Offer<'a> {
    exchange: &'a CaseExchange,
    ds: Option<BoxedDataSource>,
}

impl Future for Offer<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        match this.ds.take() {
            Some(ds) => {
                *this.exchange.slot.lock() = Some(ds);
                Poll::Pending
            }
            None => Poll::Ready(()),
        }
    }
}

/// Future returned by [`CaseExchange::request_proposal`]: marks the request
/// and returns `Pending` on the first poll, then resolves with the provided
/// proposal.
pub(crate) struct RequestProposal<'a> {
    exchange: &'a CaseExchange,
    requested: bool,
}

impl Future for RequestProposal<'_> {
    type Output = Result<Next, InternalError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut slot = this.exchange.proposal.lock();
        if !this.requested {
            this.requested = true;
            *slot = ProposalSlot::Requested;
            return Poll::Pending;
        }
        let provided = match core::mem::replace(&mut *slot, ProposalSlot::Idle) {
            ProposalSlot::Provided(next) => Some(next),
            ProposalSlot::Idle | ProposalSlot::Requested => None,
        };
        Poll::Ready(unwrap_provided(provided))
    }
}

fn unwrap_provided(provided: Option<Next>) -> Result<Next, InternalError> {
    Ok(hegel_internal_unwrap!(
        provided,
        "the engine was resumed without the proposal it requested"
    ))
}

/// Run an engine future to completion on the calling thread, handing each
/// test case it offers through `exchange` to `run_case`. `run_case` must
/// finish the case — everything through `mark_complete` — before returning,
/// upholding the alternation protocol. Test-only: the C ABI drives the
/// engine future one poll per `hegel_next_test_case` call instead.
#[cfg(any(test, feature = "__bench"))]
pub(crate) fn drive<F: Future>(
    exchange: &CaseExchange,
    fut: F,
    mut run_case: impl FnMut(BoxedDataSource),
) -> F::Output {
    let mut fut = core::pin::pin!(fut);
    let mut cx = Context::from_waker(core::task::Waker::noop());
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(out) => return out,
            Poll::Pending => run_case(exchange.take().unwrap()),
        }
    }
}

/// Test helper: run a future that must complete without offering any test
/// case — e.g. a shrinker driven by a synchronous probe.
#[cfg(test)]
pub(crate) fn drive_no_yield<F: Future>(fut: F) -> F::Output {
    let mut fut = core::pin::pin!(fut);
    match fut
        .as_mut()
        .poll(&mut Context::from_waker(core::task::Waker::noop()))
    {
        Poll::Ready(out) => out,
        Poll::Pending => unreachable!("future offered a test case but none was expected"),
    }
}

#[cfg(test)]
#[path = "../tests/embedded/exchange_tests.rs"]
mod tests;
