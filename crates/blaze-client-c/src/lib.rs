//! blaze-client's C ABI (`include/blaze_client.h`, docs/client.md): a connection, a request in
//! the API's JSON or CBOR with each response to a callback, a call by name, a close.
//!
//! [`abi`] is all of it, and the one module with unsafe code: every function of the header
//! takes and gives raw pointers.

#![deny(unsafe_code)]

#[expect(unsafe_code, reason = "a C ABI: raw pointers in and out, each with its rule stated")]
pub mod abi;
