//! The pure parts of the runner's hvbox executor (`spawn_hvbox.rs`, READINESS.md prompt W3).
//!
//! NO TEST HERE STARTS THE HELPER, AND NO TEST CONSTRUCTS AN `HvboxExecutor` WITH A REAL HELPER
//! PATH (D94: the helper uses Hypervisor.framework, and a run of it kernel-panicked the owner's
//! Mac on 2026-11-16). Only the status mapping and the file names are tested, with no process.
//!
//! Run: `lanecheck.sh blaze-proc-macro test spawn_hvbox`; expected `2 passed, 0 failed`.

use blaze_ids::ContentId;

use crate::spawn_hvbox::{
    HELPER_NOT_ALLOWED, HELPER_REFUSED, HELPER_TIMED_OUT, HELPER_USAGE, HelperLine, HelperWhy, KeyFileEnd, LINE_BYTES,
    StatusMeaning, Tail, key_file, status_to_result,
};

/// Every row of the helper's statuses (READINESS.md 2d, prompt W3): 0 is the one success; 121 is
/// "the owner has not allowed hvbox" (D94); 120 a refusal carrying the helper's last line; 2 a
/// usage error (a bug in blaze); 124 the deadline; a signal a failure whatever the code, named;
/// any other status and no status at all, failures. A wrong mapping (121 read as a helper
/// failure, a signal with code 0 read as success, 124 read as an exit) fails a row here.
/// Prints nothing; the harness's line is `test spawn_hvbox_tests::every_status_row_has_its_meaning ... ok`.
#[test]
fn every_status_row_has_its_meaning() {
    let helper = |status: i32, why: HelperWhy, line: &str| -> Result<(), StatusMeaning> {
        Err(StatusMeaning::Helper { status, why, line: HelperLine::of(line) })
    };

    assert_eq!(status_to_result(Some(0), None, "anything"), Ok(()));
    assert_eq!(status_to_result(Some(HELPER_NOT_ALLOWED), None, "guard file absent"), Err(StatusMeaning::NotAllowed));
    assert_eq!(
        status_to_result(Some(HELPER_REFUSED), None, "hv_vm_map failed"),
        helper(120, HelperWhy::Refused, "hv_vm_map failed")
    );
    assert_eq!(status_to_result(Some(HELPER_USAGE), None, "usage: spawn"), helper(2, HelperWhy::Usage, "usage: spawn"));
    assert_eq!(status_to_result(Some(HELPER_TIMED_OUT), None, ""), Err(StatusMeaning::Deadline));
    assert_eq!(status_to_result(None, Some(6), "panicked"), helper(134, HelperWhy::Signal(6), "panicked"));
    // A signal wins over a code of 0: an ended-by-signal helper wrote no answer.
    assert_eq!(status_to_result(Some(0), Some(9), ""), helper(137, HelperWhy::Signal(9), ""));
    assert_eq!(status_to_result(Some(1), None, "boom"), helper(1, HelperWhy::Other, "boom"));
    assert_eq!(status_to_result(Some(7), None, ""), helper(7, HelperWhy::Other, ""));
    assert_eq!(status_to_result(None, None, ""), helper(-1, HelperWhy::NoStatus, ""));

    // The helper's line is kept in place, cut at a character boundary past LINE_BYTES.
    let long = "é".repeat(LINE_BYTES);
    match status_to_result(Some(HELPER_REFUSED), None, &long) {
        Err(StatusMeaning::Helper { line, .. }) => {
            assert!(line.cut(), "a line past LINE_BYTES is marked cut");
            assert_eq!(line.as_str().len(), LINE_BYTES, "two-byte characters fill LINE_BYTES exactly");
            assert!(long.starts_with(line.as_str()));
        }
        other => panic!("status 120 must be a helper refusal, got {other:?}"),
    }

    // The last non-empty line of standard error, fed in pieces split mid-line.
    let mut tail = Tail::new();
    tail.feed(b"loading the guest\nhv_vcpu_cr");
    tail.feed(b"eate failed: 0xfae94001\n\n");
    assert_eq!(tail.line().as_str(), "hv_vcpu_create failed: 0xfae94001");
    tail.feed(b"no newline at the end");
    assert_eq!(tail.line().as_str(), "no newline at the end");
}

/// The request and answer files are named by ALL 64 hex digits of the request's key (a key's
/// `Display` shows only 12; a name from it would let two requests share a file), lowercase,
/// then `.req` or `.ans`. Prints nothing; the harness's line is
/// `test spawn_hvbox_tests::the_request_file_is_named_by_the_key_hex ... ok`.
#[test]
fn the_request_file_is_named_by_the_key_hex() {
    let mut bytes = [0u8; 32];
    for (at, byte) in bytes.iter_mut().enumerate() {
        *byte = (at as u8).wrapping_mul(17).wrapping_add(0xa0);
    }
    let key = ContentId(bytes);
    // Byte i is (17 i + 0xa0) mod 256.
    let hex = "a0b1c2d3e4f5061728394a5b6c7d8e9fb0c1d2e3f405162738495a6b7c8d9eaf";
    assert_eq!(hex.len(), 64);

    let request = key_file(key, KeyFileEnd::Request);
    let answer = key_file(key, KeyFileEnd::Answer);
    assert_eq!(request.as_str(), format!("{hex}.req"));
    assert_eq!(answer.as_str(), format!("{hex}.ans"));
    assert_eq!(request.as_str(), format!("{key:x}.req"), "the name agrees with the key's full hex form");

    // One byte of the key changed changes the name.
    let mut other = bytes;
    other[31] ^= 1;
    assert_ne!(key_file(ContentId(other), KeyFileEnd::Request), request);
}
