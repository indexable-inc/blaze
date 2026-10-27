//! Messages on a stream: four bytes of big-endian length, then that many bytes of CBOR.
//!
//! The first byte of a frame is always 0 (no message is longer than [`LONGEST`]), which no
//! line of text starts with: a server can tell a framed client from one that sends lines.

use std::io::{self, Read, Write};

/// The longest message, as the line protocol's longest request.
pub const LONGEST: usize = 16 << 20;

/// Writes `message` as one frame.
pub fn write(out: &mut impl Write, message: &[u8]) -> io::Result<()> {
    if message.len() > LONGEST {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a message longer than 16 MiB"));
    }
    let mut framed = Vec::with_capacity(4 + message.len());
    framed.extend((message.len() as u32).to_be_bytes());
    framed.extend_from_slice(message);
    out.write_all(&framed)
}

/// Reads one frame: `None` when the stream ends before one begins.
pub fn read(input: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0; 4];
    let mut got = 0;
    while got < 4 {
        match input.read(&mut len[got..])? {
            0 if got == 0 => return Ok(None),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => got += n,
        }
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > LONGEST {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "a frame longer than 16 MiB"));
    }
    let mut message = vec![0; len];
    input.read_exact(&mut message)?;
    Ok(Some(message))
}
