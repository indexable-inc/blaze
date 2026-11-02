//! An elided build script's output against what its run (user code) printed, under
//! `BLAZE_ELIDE=verify`: `blaze_build_script::verify(elided, ran)`. The input's first byte
//! says how the run's output is made from the rest, the elided one: the same, one byte
//! replaced, inserted or removed, cut short, the rest split in two, or a byte added; the next
//! two bytes (little-endian) are where, and the fourth the byte.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - `verify` agrees exactly when the two are the same bytes.
//! - When they differ, its error names the first line (each ended by its `\n`, the last
//!   perhaps not) that differs, counted from 1, and shows that line of each, or nothing where
//!   one has no such line, in the words of `verify_names_the_first_line_that_differs`.
//! - Verifying asks for at most `blaze_fuzz::allowed(len)` bytes at once.

#![forbid(unsafe_code)]

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use blaze_build_script::{Error, elide, verify};
use blaze_fuzzer::Target;

const TARGET: Target = Target {
    name: "usercode_script_verify",
    check,
    seeds,
    dictionary: &[b"\n", b"\r\n", b"cargo:", b"cargo::", b"\xff", b"\xc3\xa9", b"\"", b"\\"],
    max_len: 1 << 16,
    budget: Duration::from_secs(120),
};

/// The elided output and the run's, as the input makes them.
fn outputs(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (head, rest) = data.split_at(data.len().min(4));
    let byte = |at: usize| head.get(at).copied().unwrap_or(0);
    let at = usize::from(u16::from_le_bytes([byte(1), byte(2)]));
    let (value, elided) = (byte(3), rest.to_vec());
    let mut ran = elided.clone();
    let len = ran.len();
    match byte(0) % 7 {
        0 => {}
        1 if len > 0 => ran[at % len] = value,
        2 => ran.insert(at % (len + 1), value),
        3 if len > 0 => _ = ran.remove(at % len),
        4 => ran.truncate(at % (len + 1)),
        5 => {
            let split = at % (len + 1);
            return (elided[..split].to_vec(), elided[split..].to_vec());
        }
        _ => ran.push(value),
    }
    (elided, ran)
}

/// What `verify` says of `elided` and `ran`, worked out line by line.
fn model(elided: &[u8], ran: &[u8]) -> Result<(), Error> {
    if elided == ran {
        return Ok(());
    }
    let said: Vec<&[u8]> = elided.split_inclusive(|it| *it == b'\n').collect();
    let printed: Vec<&[u8]> = ran.split_inclusive(|it| *it == b'\n').collect();
    let line = (0..=said.len().max(printed.len()))
        .find(|it| said.get(*it) != printed.get(*it))
        .expect("bytes that differ have a line that does");
    let shown = |it: Option<&&[u8]>| match it {
        Some(it) => format!("{:?}", String::from_utf8_lossy(it)),
        None => "nothing".to_owned(),
    };
    Err(Error(format!(
        "the elided build script's output differs from its run's at line {}: elision says {}, the run printed {}",
        line + 1,
        shown(said.get(line)),
        shown(printed.get(line))
    )))
}

fn check(data: &[u8]) {
    let (elided, ran) = outputs(data);
    let (most, said) = blaze_fuzz::largest_allocation(|| verify(&elided, &ran));
    let len = elided.len() + ran.len();
    assert!(most <= blaze_fuzz::allowed(len), "verifying {len} bytes asked for {most} at once");
    assert_eq!(said, model(&elided, &ran), "`verify` names the first line that differs");
}

/// What elision makes of scripts, under each way of making the run's output.
fn seeds() -> Vec<Vec<u8>> {
    let root = PathBuf::from("/p/foo/build.rs");
    let elided = |source: &str| {
        let files = BTreeMap::from([(root.clone(), source.as_bytes().to_vec())]);
        elide(&root, &files).expect("a script of the forms elision reads")
    };
    let outputs = [
        elided("fn main() {\n    println!(\"cargo:rerun-if-changed=build.rs\");\n    println!(\"cargo:rustc-cfg=foo\");\n}\n"),
        elided("fn main() { print!(\"cargo:a=1\\ncargo:b=é\\ncargo:c=\"); print!(\"3\\n\"); }"),
        b"cargo:a=1\ncargo:b=2\n".to_vec(),
    ];
    let mut seeds = Vec::new();
    for output in &outputs {
        for (op, at, value) in [(0, 0, 0), (1, 13, b'3'), (2, 5, b'\n'), (3, 10, 0), (4, 12, 0), (5, 10, 0), (6, 0, b'x')] {
            seeds.push([&[op, at, 0, value][..], &output[..]].concat());
        }
    }
    seeds
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
