//! The report a sandbox's launcher leaves for the driver that ran it (what was read, or why
//! the sandbox was not made), written beside a run of user code: `blaze_usercode::trace::decode`.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - A report decoded is the bytes `trace::encode` writes for it, but for a `NotMade` reason
//!   that was not UTF-8 (read lossily); either way it decodes again to itself.
//! - Decoding asks for at most `blaze_fuzz::allowed(len)` bytes at once: no count is believed
//!   past the bytes there are.

#![forbid(unsafe_code)]

use std::{ffi::OsString, path::PathBuf, time::Duration};

use blaze_core::{Traced, Untraceable};
use blaze_fuzzer::Target;
use blaze_usercode::trace::{self, Report};

const TARGET: Target = Target {
    name: "usercode_trace_report",
    check,
    seeds,
    dictionary: &[
        b"blaze v2 report1",
        b"\x00\x00\x00\x00\x00\x00\x00\x00",
        b"\x01\x00\x00\x00\x00\x00\x00\x00",
        b"\xff\xff\xff\xff\xff\xff\xff\xff",
        b"clock",
        b"random",
        b"network",
        b"descriptor",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(120),
};

/// The magic, the kind and a `NotMade` reason's length: where its reason starts.
const REASON: usize = 16 + 1 + 8;

fn check(data: &[u8]) {
    let (most, got) = blaze_fuzz::largest_allocation(|| trace::decode(data));
    assert!(most <= blaze_fuzz::allowed(data.len()), "decoding {} bytes asked for {most} at once", data.len());
    let Ok(report) = got else { return };
    let again = trace::encode(&report);
    assert_eq!(trace::decode(&again).as_ref(), Ok(&report), "a report encoded again decodes to itself");
    let lossy = matches!(report, Report::NotMade(_)) && std::str::from_utf8(&data[REASON..]).is_err();
    if !lossy {
        assert_eq!(again, data, "a report decoded is the bytes it encodes to: {report:?}");
    }
}

/// Reports as a launcher writes them.
fn seeds() -> Vec<Vec<u8>> {
    let traced = Traced {
        files: vec![(PathBuf::from("/p/foo/build.rs"), Some([7; 32])), (PathBuf::from("/p/foo/missing"), None)],
        dirs: vec![(PathBuf::from("/p/foo/src"), [9; 32])],
        env: vec![(OsString::from("CC"), Some(OsString::from("cc"))), (OsString::from("NOT_SET"), None)],
        untraceable: vec![Untraceable::Clock, Untraceable::Random, Untraceable::Network, Untraceable::Descriptor],
    };
    [
        Report::NotMade("spawn: no such file or directory".into()),
        Report::Ran(None),
        Report::Ran(Some(Traced::default())),
        Report::Ran(Some(traced)),
    ]
    .iter()
    .map(trace::encode)
    .collect()
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
