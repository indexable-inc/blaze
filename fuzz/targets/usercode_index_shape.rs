//! A run's shape (which paths, listings and variables a run of user code read, without their
//! values), as the store keeps it in a slot of its run key's chain and a remote may send it:
//! `blaze_usercode::index::Shape::decode`.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - A shape decoded is exactly the bytes `Shape::encode` writes for it.
//! - Decoding asks for at most `blaze_fuzz::allowed(len)` bytes at once: no count is believed
//!   past the bytes there are.

#![forbid(unsafe_code)]

use std::{ffi::OsString, path::PathBuf, time::Duration};

use blaze_core::{Traced, Untraceable};
use blaze_fuzzer::Target;
use blaze_usercode::{index::Shape, trace};

const TARGET: Target = Target {
    name: "usercode_index_shape",
    check,
    seeds,
    dictionary: &[
        b"blaze v2 shape 1",
        b"\x00\x00\x00\x00\x00\x00\x00\x00",
        b"\x01\x00\x00\x00\x00\x00\x00\x00",
        b"\xff\xff\xff\xff\xff\xff\xff\xff",
        b"clock",
        b"random",
        b"network",
        b"descriptor",
        b"\x00*",
        b"\x00package",
        b"\x00tree",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(120),
};

fn check(data: &[u8]) {
    let (most, got) = blaze_fuzz::largest_allocation(|| Shape::decode(data));
    assert!(most <= blaze_fuzz::allowed(data.len()), "decoding {} bytes asked for {most} at once", data.len());
    if let Ok(shape) = got {
        assert_eq!(shape.encode(), data, "a shape decoded is the bytes it encodes to: {shape:?}");
    }
}

/// Shapes of what runs read, as `Shape::of` makes them.
fn seeds() -> Vec<Vec<u8>> {
    let traced = Traced {
        files: vec![(PathBuf::from("/p/foo/build.rs"), Some([7; 32])), (PathBuf::from("/p/foo/missing"), None)],
        dirs: vec![(PathBuf::from("/p/foo/src"), [9; 32]), (PathBuf::from("/p/foo").join(trace::PACKAGE), [1; 32])],
        env: vec![
            (OsString::from("CC"), Some(OsString::from("cc"))),
            (OsString::from(trace::ALL_ENV_NAME), Some(OsString::from("00"))),
            (OsString::from("NOT_SET"), None),
        ],
        untraceable: vec![Untraceable::Clock, Untraceable::Descriptor],
    };
    vec![Shape::default().encode(), Shape::of(&traced).encode()]
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
