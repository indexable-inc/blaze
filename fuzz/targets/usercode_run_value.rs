//! A run's result as the store keeps it (exit, what user code printed, its `OUT_DIR`'s files),
//! which a remote may send: `blaze_usercode::run::Value::decode`, postcard after a magic.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - A value decoded encodes to bytes that decode to it again.
//! - Decoding asks for at most `blaze_fuzz::allowed(len)` bytes at once: no length is
//!   believed past the bytes there are.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_fuzzer::Target;
use blaze_usercode::run::{OutFile, Value};

const TARGET: Target = Target {
    name: "usercode_run_value",
    check,
    seeds,
    dictionary: &[b"blaze v2 run value 1", b"\x00", b"\x01", b"\x80\x80\x80\x80\x0f", b"\xff\xff\xff\xff\x0f"],
    max_len: 1 << 16,
    budget: Duration::from_secs(120),
};

fn check(data: &[u8]) {
    let (most, got) = blaze_fuzz::largest_allocation(|| Value::decode(data));
    assert!(most <= blaze_fuzz::allowed(data.len()), "decoding {} bytes asked for {most} at once", data.len());
    if let Ok(value) = got {
        assert_eq!(Value::decode(&value.encode()).as_ref(), Ok(&value), "a value encoded again decodes to itself");
    }
}

/// Values of runs that passed, failed and were killed.
fn seeds() -> Vec<Vec<u8>> {
    let file = |path: &str, executable, bytes: Option<&[u8]>| OutFile { path: path.into(), executable, bytes: bytes.map(<[u8]>::to_vec) };
    [
        Value { code: Some(0), signal: None, stdout: b"cargo:rustc-cfg=foo\n".to_vec(), stderr: Vec::new(), files: Vec::new() },
        Value {
            code: Some(0),
            signal: None,
            stdout: b"cargo:rustc-link-lib=static=z\n".to_vec(),
            stderr: b"warning: cc\n".to_vec(),
            files: vec![file("include", false, None), file("include/z.h", false, Some(b"int z;\n")), file("bin/tool", true, Some(&[0x7f, b'E', b'L', b'F']))],
        },
        Value { code: Some(101), signal: None, stdout: Vec::new(), stderr: b"thread 'main' panicked\n".to_vec(), files: Vec::new() },
        Value { code: None, signal: Some(9), stdout: Vec::new(), stderr: Vec::new(), files: Vec::new() },
    ]
    .iter()
    .map(Value::encode)
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
