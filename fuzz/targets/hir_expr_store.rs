//! An expression store (a signature's types and expressions) as the store keeps it once cold
//! (`crates/hir-def/src/expr_store/persist.rs`): `<ExpressionStore as Persist>::read`, then
//! `ExpressionStoreRef::decode`, of bytes from the store or another machine. The input is one of
//! `hir_shared`'s two kinds: a buffer as it came, or edits of the plain columns of a valid store.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - `read` and `decode` never panic or hang (`read` checks kinds, side-table offsets, hygiene
//!   and roots; `decode` runs `HirColumns::check` and `TypeColumns::check`);
//! - no allocation of them is larger than `blaze_fuzz::allowed` of the buffer's length;
//! - a store decoded from any buffer is written as a buffer that decodes to an equal store and
//!   is written again as the same bytes;
//! - a valid store edited only in the columns its decode copies byte for byte (kinds, data,
//!   `extra`, lists, arms, hygiene, block scopes and editions, roots) decodes to `Err`, or to a
//!   store written as exactly the edited bytes.
//!
//! The decoded store's hygiene and block ids come from the bytes; they are compared and
//! dropped, never looked up.

#![forbid(unsafe_code)]

use std::{sync::OnceLock, time::Duration};

use blaze_format::{FormatError, Persist};
use blaze_fuzzer::Target;
use hir_def::expr_store::ExpressionStore;

#[path = "hir_shared.rs"]
mod shared;

#[path = "hir_expr_shared.rs"]
mod expr;

use shared::Valid;

const TARGET: Target = Target {
    name: "hir_expr_store",
    check,
    seeds,
    dictionary: &[
        b"BLAZEV2\x00",
        b"\xff\xff\xff\xff",
        b"\x00\x00\x00\x80",
        b"\x00\x00\x00\x00",
        b"\x01\x00\x00\x00",
        b"\x03\x00\x00\x00",
        b"\x1a",
        b"\x21",
    ],
    max_len: 1 << 14,
    budget: Duration::from_secs(600),
};

fn decode(buffer: &[u8]) -> Result<ExpressionStore, FormatError> {
    ExpressionStore::read(buffer)?.decode()
}

fn check(data: &[u8]) {
    shared::check(data, valid(), decode);
}

fn seeds() -> Vec<Vec<u8>> {
    shared::inputs(valid())
}

/// The empty store, through the crate's own writer, and `hir_expr_shared`'s.
fn valid() -> &'static [Valid] {
    static VALID: OnceLock<Vec<Valid>> = OnceLock::new();
    VALID.get_or_init(|| {
        let empty = Valid { bytes: blaze_format::to_bytes(ExpressionStore::empty()), columns: Vec::new() };
        vec![empty, expr::store_buffer(ExpressionStore::SCHEMA)]
    })
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use blaze_format::Aligned;

    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    /// The hand-built store is laid out as the writer lays one out, so the column edits keep
    /// reaching `decode`'s checks.
    #[test]
    fn valid_stores_round_trip() {
        for (at, valid) in super::valid().iter().enumerate() {
            let store = super::decode(&Aligned::copy_from(&valid.bytes))
                .unwrap_or_else(|error| panic!("valid store {at} does not decode: {error}"));
            assert!(blaze_format::to_bytes(&store) == valid.bytes, "valid store {at} is written as other bytes");
        }
    }
}
