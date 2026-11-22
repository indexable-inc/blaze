//! Holds the committed template table (table/arm64.bztab, made by `examples/gen_table.rs` from
//! enc.rs; backend-next.md D99) to its pin (table/arm64.json): the BLAKE3 digest of its bytes.
//! It only checks: it makes nothing, runs nothing and reads nothing but the two files, which it
//! includes. Whether the table is what today's encoders make is the test
//! `table_gen::tests::the_committed_table_is_the_generators`. The model is blaze-stencil's pin
//! (crates/blaze-stencil/pin.rs).

use core::fmt;

const TABLE: &[u8] = include_bytes!("table/arm64.bztab");
const PIN: &str = include_str!("table/arm64.json");
const KEY: &str = "\"blake3\": \"";

/// Why the table was refused: each names the file.
enum PinError {
    /// The pin has no 64-hex-digit `blake3` value.
    Malformed,
    /// The table's digest is not the pinned one.
    Digest { pinned: [u8; 32], have: [u8; 32] },
}

struct Hex<'a>(&'a [u8; 32]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A build script's `main` shows its error by `Debug`: the message is the one to read.
impl fmt::Debug for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PinError::Malformed => f.write_str("crates/blaze-emit/table/arm64.json holds no `\"blake3\": \"<64 hex digits>\"`"),
            PinError::Digest { pinned, have } => write!(
                f,
                "crates/blaze-emit/table/arm64.bztab hashes to {} but table/arm64.json pins {}: make both again with `cargo run -p blaze-emit --example gen_table` from the workspace's root",
                Hex(have),
                Hex(pinned)
            ),
        }
    }
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

fn pinned(pin: &str) -> Result<[u8; 32], PinError> {
    let at = pin.find(KEY).ok_or(PinError::Malformed)? + KEY.len();
    let hex = pin.as_bytes().get(at..at + 64).ok_or(PinError::Malformed)?;
    let mut out = [0u8; 32];
    for (k, pair) in hex.chunks_exact(2).enumerate() {
        out[k] = nibble(pair[0]).ok_or(PinError::Malformed)? << 4 | nibble(pair[1]).ok_or(PinError::Malformed)?;
    }
    Ok(out)
}

fn main() -> Result<(), PinError> {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=table/arm64.bztab");
    println!("cargo::rerun-if-changed=table/arm64.json");
    let pinned = pinned(PIN)?;
    let have = blaze_blake3::hash(TABLE);
    match have == pinned {
        true => Ok(()),
        false => Err(PinError::Digest { pinned, have }),
    }
}
