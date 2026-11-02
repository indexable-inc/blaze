//! A body (a function's or a constant's expressions and its parameters) as the store keeps it
//! once cold (`crates/hir-def/src/expr_store/persist.rs`): `<Body as Persist>::read`, then
//! `BodyRef::decode`, of bytes from the store or another machine. The input is one of
//! `hir_shared`'s two kinds: a buffer as it came, or edits of the plain columns of a valid body.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - `read` and `decode` never panic or hang (`read` also checks each parameter is a pattern
//!   and `self` a binding of the store);
//! - no allocation of them is larger than `blaze_fuzz::allowed` of the buffer's length;
//! - a body decoded from any buffer is written as a buffer that decodes to an equal body and is
//!   written again as the same bytes;
//! - a valid body edited only in the columns its decode copies byte for byte (the store's, and
//!   the parameters) decodes to `Err`, or to a body written as exactly the edited bytes.
//!
//! The decoded body's hygiene and block ids come from the bytes; they are compared and dropped,
//! never looked up.

#![forbid(unsafe_code)]

use std::{sync::OnceLock, time::Duration};

use blaze_format::{FormatError, Persist};
use blaze_fuzzer::Target;
use hir_def::expr_store::{Body, ExpressionStore};

#[path = "hir_shared.rs"]
mod shared;

#[path = "hir_expr_shared.rs"]
mod expr;

use shared::{Build, Valid};

const TARGET: Target = Target {
    name: "hir_body",
    check,
    seeds,
    dictionary: &[
        b"BLAZEV2\x00",
        b"\xff\xff\xff\xff",
        b"\x00\x00\x00\x80",
        b"\x00\x00\x00\x00",
        b"\x01\x00\x00\x00",
        b"\x03\x00\x00\x00",
    ],
    max_len: 1 << 14,
    budget: Duration::from_secs(600),
};

fn decode(buffer: &[u8]) -> Result<Body, FormatError> {
    Body::read(buffer)?.decode()
}

fn check(data: &[u8]) {
    shared::check(data, valid(), decode);
}

fn seeds() -> Vec<Vec<u8>> {
    shared::inputs(valid())
}

/// `hir_expr_shared`'s store with one parameter, its pattern both formal and as written, and no
/// `self`: as `<Body as Persist>::write` lays a body out.
fn body() -> Valid {
    let mut build = Build::new(Body::SCHEMA);
    let store = expr::store(&mut build);
    let params = build.column::<[u32; 2]>(&[[0, 0]], true);
    let self_param = build.column::<[u32; 2]>(&[], true);
    let mut root = build.root();
    expr::store_root(&mut root, &store);
    root.slice(params);
    root.slice(self_param);
    build.finish(root)
}

/// The empty body, through the crate's own writer, and [`body`].
fn valid() -> &'static [Valid] {
    static VALID: OnceLock<Vec<Valid>> = OnceLock::new();
    VALID.get_or_init(|| {
        let empty = Body { store: ExpressionStore::empty_owned(), params: Box::new([]), self_param: None };
        vec![Valid { bytes: blaze_format::to_bytes(&empty), columns: Vec::new() }, body()]
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

    /// The hand-built body is laid out as the writer lays one out, so the column edits keep
    /// reaching `decode`'s checks.
    #[test]
    fn valid_bodies_round_trip() {
        for (at, valid) in super::valid().iter().enumerate() {
            let body = super::decode(&Aligned::copy_from(&valid.bytes))
                .unwrap_or_else(|error| panic!("valid body {at} does not decode: {error}"));
            assert!(blaze_format::to_bytes(&body) == valid.bytes, "valid body {at} is written as other bytes");
        }
    }
}
