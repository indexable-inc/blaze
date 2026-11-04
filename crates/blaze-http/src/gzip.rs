//! A gzip body decoded as it comes: flate2's writing decoder into a vector, so each piece of
//! the body gives the bytes it decodes to, and the end checks the stream's CRC and length.
//!
//! The decoder hands over what a piece decodes to as the next piece comes (it keeps up to its
//! 32 KiB buffer between writes), and the rest at [`Gunzip::finish`]. It is never flushed
//! between pieces: flate2's flush unwraps the inflater's result, and a panic here would end
//! every request of the driver, not just this one.

#![blaze::sans_io]

use std::io::Write as _;

pub(crate) struct Gunzip {
    decoder: flate2::write::GzDecoder<Vec<u8>>,
}

impl Gunzip {
    pub(crate) fn new() -> Gunzip {
        // alloc: one per gzip body, the buffer its decoded pieces pass through.
        Gunzip { decoder: flate2::write::GzDecoder::new(Vec::new()) }
    }

    /// What `bytes`, the body's next piece, decode to. An error says why they are not gzip,
    /// or that they go on past the end of the stream.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        self.decoder.write_all(bytes).map_err(|e| said(&e))?;
        Ok(std::mem::take(self.decoder.get_mut()))
    }

    /// The rest, once the body has ended: an error when the stream stopped short or its CRC
    /// or length is not what it decoded to.
    pub(crate) fn finish(&mut self) -> Result<Vec<u8>, String> {
        self.decoder.try_finish().map_err(|e| said(&e))?;
        Ok(std::mem::take(self.decoder.get_mut()))
    }
}

fn said(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::WriteZero {
        return "bytes after the end of its gzip stream".to_owned();
    }
    // alloc: an error's message, made only when it fails.
    e.to_string()
}
