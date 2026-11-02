//! An expression store's source map (each id's syntax, and syntax back to ids) as the store
//! keeps it once cold (`crates/hir-def/src/expr_store/persist.rs`):
//! `<ExpressionStoreSourceMap as Persist>::read`, then `ExpressionStoreSourceMapRef::decode`, of
//! bytes from the store or another machine. The input is one of `hir_shared`'s two kinds: a
//! buffer as it came, or edits of the plain columns of a valid source map.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - `read` and `decode` never panic or hang (`read` checks each pointer's file kind, syntax
//!   kind and range, and each table's keys in strictly increasing order);
//! - no allocation of them is larger than `blaze_fuzz::allowed` of the buffer's length;
//! - a source map decoded from any buffer is written as a buffer that decodes to an equal map
//!   and is written again as the same bytes;
//! - a valid source map edited only in its columns (the maps back, the tables' keys and ids,
//!   the expansions) decodes to `Err`, or to a map written as exactly the edited bytes.
//!
//! The decoded map's files and macro calls are query IDs from the bytes; they are compared and
//! dropped, never looked up.

#![forbid(unsafe_code)]

use std::{sync::OnceLock, time::Duration};

use blaze_format::{FormatError, Persist};
use blaze_fuzzer::Target;
use hir_def::expr_store::ExpressionStoreSourceMap;

#[path = "hir_shared.rs"]
mod shared;

#[path = "hir_expr_shared.rs"]
mod expr;

use shared::{Build, Valid};

const TARGET: Target = Target {
    name: "hir_store_source_map",
    check,
    seeds,
    dictionary: &[
        b"BLAZEV2\x00",
        b"\xff\xff\xff\xff",
        b"\x00\x00\x00\x80",
        b"\x01\x00\x00\x00",
        b"\x02\x00\x00\x00",
        b"\x00\x00\x00\x00\x00\x00\x00\x00",
    ],
    max_len: 1 << 14,
    budget: Duration::from_secs(600),
};

fn decode(buffer: &[u8]) -> Result<ExpressionStoreSourceMap, FormatError> {
    ExpressionStoreSourceMap::read(buffer)?.decode()
}

fn check(data: &[u8]) {
    shared::check(data, valid(), decode);
}

fn seeds() -> Vec<Vec<u8>> {
    shared::inputs(valid())
}

/// `hir_expr_shared`'s source map, as `<ExpressionStoreSourceMap as Persist>::write` lays one
/// out.
fn source_map() -> Valid {
    let mut build = Build::new(ExpressionStoreSourceMap::SCHEMA);
    let cols = expr::source_map(&mut build);
    let mut root = build.root();
    expr::source_map_root(&mut root, &cols);
    build.finish(root)
}

/// The empty source map, through the crate's own writer, and [`source_map`].
fn valid() -> &'static [Valid] {
    static VALID: OnceLock<Vec<Valid>> = OnceLock::new();
    VALID.get_or_init(|| {
        let empty = ExpressionStoreSourceMap::default();
        vec![Valid { bytes: blaze_format::to_bytes(&empty), columns: Vec::new() }, source_map()]
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

    /// The hand-built source map is laid out as the writer lays one out, so the column edits
    /// keep reaching `read`'s checks.
    #[test]
    fn valid_source_maps_round_trip() {
        for (at, valid) in super::valid().iter().enumerate() {
            let map = super::decode(&Aligned::copy_from(&valid.bytes))
                .unwrap_or_else(|error| panic!("valid source map {at} does not decode: {error}"));
            assert!(blaze_format::to_bytes(&map) == valid.bytes, "valid source map {at} is written as other bytes");
        }
    }
}
