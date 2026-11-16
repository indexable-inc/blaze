//! A gzip body decoded as it comes: blaze-deflate's streaming [`GzipDecoder`] fed each piece of
//! the body, giving the bytes that piece decodes to; the end checks the stream's CRC and length.
//!
//! The decoder is held to the request's bound: its output may not pass `most` (a body past it
//! is [`Kind::TooBig`], as one sent plain is), its header may not be longer than the body may
//! be, and its ratio may not pass DEFLATE's own largest, so no valid stream is refused for it.

#![blaze::sans_io]

use std::alloc::Global;

use blaze_deflate::{Error, Format, GzipDecoder, Limits, Why};

use crate::client::Kind;

/// DEFLATE's largest ratio of output to input is about 1032 to 1 (blaze-deflate's `inflate`
/// module): a bound of it refuses no valid stream.
const DEFLATE_MOST_RATIO: u64 = 1032;

pub(crate) struct Gunzip {
    decoder: GzipDecoder<Global>,
    /// What a piece decodes to, before it is handed over.
    out: blaze_own::Vec<u8, Global>,
    most: u64,
}

impl Gunzip {
    /// A decoder of a body that may be at most `most` bytes, as sent and once decoded.
    pub(crate) fn new(most: u64) -> Gunzip {
        let limits = Limits {
            most_out: most,
            most_ratio: DEFLATE_MOST_RATIO,
            // The header is part of the body as sent, already held to `most`: saturated at the
            // most a `u32` says.
            most_header: u32::try_from(most).unwrap_or(u32::MAX),
        };
        Gunzip { decoder: GzipDecoder::new_in(limits, Global), out: blaze_own::Vec::new_in(Global), most }
    }

    /// What `bytes`, the body's next piece, decode to. An error says why they are not gzip,
    /// that they go on past the end of the stream, or that the body decodes past its bound.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<Vec<u8>, Kind> {
        let fed = self.decoder.feed(bytes, &mut self.out).map_err(|e| self.refused(e))?;
        let extra = (bytes.len() - fed.consumed) + self.decoder.over().len();
        if extra > 0 {
            let at = self.decoder.stream_len();
            return Err(Kind::Gzip(Error { format: Format::Gzip, at, why: Why::AfterEnd { extra: extra as u64 } }));
        }
        Ok(self.take())
    }

    /// The rest, once the body has ended: an error when the stream stopped short. (Its CRC and
    /// length were checked as its trailer came.)
    pub(crate) fn finish(&mut self) -> Result<Vec<u8>, Kind> {
        self.decoder.finish().map_err(|e| self.refused(e))?;
        Ok(self.take())
    }

    /// The decoded bytes so far, handed over as the asker's vector.
    fn take(&mut self) -> Vec<u8> {
        // alloc: one per piece, the asker's copy of what it decoded to (blaze-own's vector has no
        // move into std's).
        let piece = self.out.as_slice().to_vec();
        self.out.clear();
        piece
    }

    /// `error` as the request's failure: past the bound is [`Kind::TooBig`], as for a plain body.
    fn refused(&self, error: Error) -> Kind {
        match error.why {
            Why::OutputOver { .. } => Kind::TooBig { most: self.most, said: None, decoded: true },
            _ => Kind::Gzip(error),
        }
    }
}
