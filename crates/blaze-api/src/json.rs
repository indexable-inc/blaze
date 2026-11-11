//! The rendering for people (`Accept: application/json` over HTTP, the CLI's `--json`): one
//! line of JSON per envelope. Never the wire, which is CBOR ([`crate::frames`]).
//!
//! Written by sonic-rs, whose strings are escaped 16 or 32 bytes a step, to the bytes
//! serde_json writes: the API's types hold no float, the one value the two write apart.

use serde::Serialize;

use crate::{Envelope, Request, Response};

/// `envelope` as one line of JSON, without its newline.
pub fn response(envelope: &Envelope<Response>) -> String {
    line(envelope)
}

/// `envelope` as one line of JSON, without its newline.
pub fn request(envelope: &Envelope<Request>) -> String {
    line(envelope)
}

/// The request `text` is in this rendering: what a foreign caller (blaze-client's C ABI and
/// Node-API addon) hands over, a `Request` as [`request`] writes an envelope's body. Bytes are
/// arrays of numbers, as they are written. Refused naming what the JSON said wrong.
pub fn request_of(text: &[u8]) -> Result<Request, crate::ApiError> {
    sonic_rs::from_slice(text).map_err(|error| {
        // alloc: error path, a request that does not parse.
        crate::ApiError::new(format!("a request that is not the API's JSON: {error}"), None)
    })
}

fn line<T: Serialize>(envelope: &Envelope<T>) -> String {
    // alloc: the line handed out is the buffer written into; `from_utf8` only checks it.
    let mut out = Vec::new();
    line_into(&mut out, envelope);
    // alloc: none, `from_utf8` takes `out` as it is.
    String::from_utf8(out).expect("JSON is UTF-8")
}

/// [`response`] onto the end of `out`, written where it goes.
pub(crate) fn line_into<T: Serialize>(out: &mut Vec<u8>, envelope: &Envelope<T>) {
    // The API's types key no map by anything but text, so every value has a JSON rendering.
    sonic_rs::to_writer(&mut *out, envelope).expect("an envelope of the API has a JSON rendering");
}

#[cfg(test)]
mod tests;
