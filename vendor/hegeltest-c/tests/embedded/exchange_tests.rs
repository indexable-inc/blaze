//! Embedded tests for `src/exchange.rs`.

use super::*;
use alloc::string::ToString;
use alloc::vec;

use crate::backend::TestCaseResult;
use crate::native::core::NativeTestCase;
use crate::native::data_source::NativeDataSource;
use crate::native::external::{Proposal, Tail};

fn fresh_source() -> BoxedDataSource {
    let ntc = NativeTestCase::for_choices(&[], None, None);
    let (data_source, _handle) = NativeDataSource::new(ntc);
    Box::new(data_source)
}

#[test]
fn drive_hands_each_offered_case_to_run_case_and_returns_the_result() {
    let exchange = CaseExchange::new();
    let fut = async {
        for _ in 0..3 {
            exchange.offer(fresh_source()).await;
        }
        "done"
    };
    let mut seen = 0;
    let out = drive(&exchange, fut, |ds| {
        seen += 1;
        ds.mark_complete(&TestCaseResult::Valid);
    });
    assert_eq!(out, "done");
    assert_eq!(seen, 3);
}

#[test]
fn offer_resumes_only_after_the_driver_polls_again() {
    let exchange = CaseExchange::new();
    let resumed = std::cell::Cell::new(false);
    let fut = async {
        exchange.offer(fresh_source()).await;
        resumed.set(true);
    };
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    assert!(!resumed.get());
    exchange
        .take()
        .unwrap()
        .mark_complete(&TestCaseResult::Valid);
    assert!(fut.as_mut().poll(&mut cx).is_ready());
    assert!(resumed.get());
}

#[test]
fn take_errors_when_nothing_was_offered() {
    let err = CaseExchange::new().take().err().unwrap();
    let msg = err.to_string();
    assert!(
        msg.contains("suspended without offering a test case"),
        "{msg}"
    );
    assert!(msg.contains("bug in hegel"), "{msg}");
}

#[test]
fn default_is_an_empty_exchange() {
    let exchange = CaseExchange::default();
    let fut = async {
        exchange.offer(fresh_source()).await;
        7
    };
    let out = drive(&exchange, fut, |ds| {
        ds.mark_complete(&TestCaseResult::Valid);
    });
    assert_eq!(out, 7);
}

#[test]
fn drive_no_yield_returns_the_value_of_a_non_offering_future() {
    assert_eq!(drive_no_yield(async { 41 + 1 }), 42);
}

fn proposal(id: u64) -> Next {
    Next::Run(Proposal {
        id,
        prefix: Vec::new(),
        tail: Tail::Simplest,
    })
}

#[test]
fn request_proposal_suspends_until_the_driver_provides() {
    let exchange = CaseExchange::new();
    let fut = async { exchange.request_proposal().await };
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(!exchange.awaiting_proposal());
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    assert!(exchange.awaiting_proposal());
    assert!(matches!(exchange.take_handoff(), Ok(Handoff::Proposal)));
    assert!(exchange.provide(proposal(9)).is_ok());
    assert!(!exchange.awaiting_proposal());
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(Ok(Next::Run(p))) => assert_eq!(p.id, 9),
        _ => panic!("expected the provided proposal"),
    }
}

#[test]
fn provide_without_a_request_hands_the_answer_back() {
    let exchange = CaseExchange::new();
    assert!(matches!(exchange.provide(Next::Done), Err(Next::Done)));
}

#[test]
fn request_proposal_errors_when_resumed_without_one() {
    let exchange = CaseExchange::new();
    let fut = async { exchange.request_proposal().await };
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(Err(err)) => assert!(
            err.to_string()
                .contains("without the proposal it requested"),
            "{err}"
        ),
        _ => panic!("expected an internal error"),
    }
}

#[test]
fn take_handoff_prefers_an_offered_case() {
    let exchange = CaseExchange::new();
    let fut = async { exchange.offer(fresh_source()).await };
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    match exchange.take_handoff() {
        Ok(Handoff::Case(ds)) => ds.mark_complete(&TestCaseResult::Valid),
        _ => panic!("expected the offered case"),
    }
}

#[test]
fn take_handoff_errors_when_nothing_is_pending() {
    let err = CaseExchange::new().take_handoff().err().unwrap();
    assert!(
        err.to_string()
            .contains("suspended without offering a test case"),
        "{err}"
    );
}

#[test]
fn a_posted_report_is_taken_once_and_replaced_by_the_next() {
    let exchange = CaseExchange::new();
    assert_eq!(exchange.take_report(), None);
    exchange.post_report(vec![1]);
    exchange.post_report(vec![2]);
    assert_eq!(exchange.take_report(), Some(vec![2]));
    assert_eq!(exchange.take_report(), None);
}
