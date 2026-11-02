//! A cached result as the agent API carries it ([`blaze_api::TestResult::cached`]): a test not
//! run says so, with the outcome and duration of the run that stands and the set it was kept
//! under, never as a run of its own.

use std::time::Duration;

use blaze_api::{TestOutcome, TestResult};

use crate::select::Cached;

pub fn to_api(cached: &Cached) -> blaze_api::Cached {
    blaze_api::Cached {
        // clone: the message owns its reason, once per cached result sent
        basis: cached.basis.clone(),
        millis: cached.took.as_millis().min(u128::from(u64::MAX)) as u64,
        // Only passes are kept: a failure runs again.
        outcome: TestOutcome::Passed,
    }
}

/// The cached result of an API message; one whose outcome is not a pass is refused, as no
/// other is kept.
pub fn of_api(cached: &blaze_api::Cached) -> Result<Cached, String> {
    if cached.outcome != TestOutcome::Passed {
        // alloc: error path
        return Err(format!("a cached test result is a pass, not {:?}", cached.outcome));
    }
    Ok(Cached {
        took: Duration::from_millis(cached.millis),
        // clone: the result owns its reason, once per cached result read
        basis: cached.basis.clone(),
    })
}

/// The result of a test not run: it took nothing now, and its run that stands is `cached`.
/// `executable` is the daemon's index of `binary` among the request's test executables.
pub fn result(binary: &str, executable: u32, name: &str, cached: &Cached) -> TestResult {
    TestResult {
        // alloc: the message owns its names, once per result sent
        name: name.to_owned(),
        // alloc: the message owns its names, once per result sent
        binary: binary.to_owned(),
        cached: Some(to_api(cached)),
        millis: 0,
        // alloc: empty, allocates nothing
        output: Vec::new(),
        outcome: TestOutcome::Unreached,
        outside: false,
        executable,
    }
}

#[cfg(test)]
mod tests {
    use blaze_api::{Envelope, Response, SCHEMA};

    use super::*;
    use crate::select::Basis;

    fn round_trip(result: &TestResult) -> TestResult {
        let message = Envelope::new(9, Response::TestResult(result.clone()));
        let bytes = blaze_protocol::cbor::to_vec(&message).unwrap();
        let back: Envelope<Response> = blaze_protocol::cbor::from_slice(&bytes).unwrap();
        assert_eq!(back.schema, SCHEMA);
        assert_eq!(blaze_protocol::cbor::to_vec(&back).unwrap(), bytes, "the encoding is deterministic");
        match back.body {
            Response::TestResult(it) => it,
            other => panic!("a test result came back as {other:?}"),
        }
    }

    #[test]
    fn a_cached_pass_round_trips_with_its_outcome_duration_and_basis() {
        for basis in [Basis::Executed, Basis::Reached { why: "the test was killed after 60s".to_owned() }] {
            let cached = Cached { took: Duration::from_millis(1234), basis };
            let sent = result("tests/parse", 3, "parse::nested", &cached);
            let back = round_trip(&sent);
            assert_eq!(back, sent);
            assert_eq!(back.outcome, TestOutcome::Unreached);
            assert_eq!(back.millis, 0);
            let carried = back.cached.as_ref().unwrap();
            assert_eq!(carried.outcome, TestOutcome::Passed);
            assert_eq!(carried.millis, 1234);
            assert_eq!(of_api(carried).unwrap(), cached);
        }
    }

    #[test]
    fn a_run_carries_no_cached_result() {
        let ran = TestResult {
            name: "parse::nested".to_owned(),
            binary: "tests/parse".to_owned(),
            cached: None,
            millis: 7,
            output: b"ok".to_vec(),
            outcome: TestOutcome::Passed,
            outside: false,
            executable: 0,
        };
        assert_eq!(round_trip(&ran), ran);
    }

    #[test]
    fn a_duration_is_carried_in_whole_milliseconds() {
        let cached = Cached { took: Duration::from_micros(2_999), basis: Basis::Executed };
        assert_eq!(to_api(&cached).millis, 2);
        assert_eq!(of_api(&to_api(&cached)).unwrap().took, Duration::from_millis(2));
    }

    #[test]
    fn a_cached_failure_is_refused() {
        let mut cached = to_api(&Cached { took: Duration::from_millis(1), basis: Basis::Executed });
        cached.outcome = TestOutcome::Failed;
        assert!(of_api(&cached).is_err());
    }
}
