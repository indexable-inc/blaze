//! Helper functions for efficient IO.

use std::io;

pub(crate) fn copy_wide(mut reader: impl io::Read, hasher: &mut crate::Hasher) -> io::Result<()> {
    // A reader that fills 64 KiB has more: the rest goes through a 1 MiB
    // buffer, filled before each update. On SME2, 1 MiB pieces hash at the
    // one-shot rate, where 64 KiB pieces each meet the SME unit's slow
    // state after the reads between them (M4 Max, files of 8-64 MiB 23-33%
    // less time).
    // The first 64 KiB lead the 1 MiB buffer, so its updates stay whole
    // 1 MiB subtrees (a Hasher's subtrees align to their own size). Every
    // byte read is hashed before an error returns: a caller that retries
    // after one (WouldBlock, say) loses nothing.
    let mut small = [0; 65536];
    let (n, read) = fill(&mut reader, &mut small);
    if n < small.len() || read.is_err() {
        hasher.update(&small[..n]);
        return read;
    }
    let mut buffer = std::vec![0; 1 << 20];
    buffer[..n].copy_from_slice(&small);
    let mut start = n;
    loop {
        let (filled, read) = fill(&mut reader, &mut buffer[start..]);
        let n = start + filled;
        hasher.update(&buffer[..n]);
        read?;
        if n < buffer.len() {
            return Ok(());
        }
        // The loop's variant: a pass that goes on filled the buffer, so it
        // read at least one byte; the reader has that many fewer.
        assert!(filled > 0, "a full buffer with nothing read");
        start = 0;
    }
}

/// Read into `buffer` until it is full, the reader ends, or it fails: the
/// bytes read, and the error if it failed.
fn fill(reader: &mut impl io::Read, buffer: &mut [u8]) -> (usize, io::Result<()>) {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            // see test_update_reader_interrupted
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return (filled, Err(e)),
        }
    }
    (filled, Ok(()))
}
