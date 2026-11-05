//! The socket's frames: `[type u8][len u32 BE][payload]`. Three types, each refused anywhere it does not belong:
//!
//! | type | payload | direction |
//! | --- | --- | --- |
//! | [`T_DATA`] (2) | raw terminal bytes | PTY output to an attached client, keyboard input to the daemon |
//! | [`T_CBOR`] (6) | one [`crate::msg::Up`] or [`crate::msg::Down`] | both |
//! | [`T_EVENT`] (7) | one chat [`crate::event::Event`] | daemon to a chat attach; also every record of a chat log |
//!
//! The JSON protocol before schema 1 used types 1, 3 and 4; [`read`] refuses them by name.
use std::io::{Read, Write};

/// Raw terminal bytes.
pub const T_DATA: u8 = 2;
/// A deterministic-CBOR message: [`crate::msg::Up`] from a client, [`crate::msg::Down`] from the daemon.
pub const T_CBOR: u8 = 6;
/// A deterministic-CBOR chat event, the same bytes as its record in the chat's log.
pub const T_EVENT: u8 = 7;

/// A frame's header: its type and its payload's length.
pub const HEAD: usize = 5;

/// The largest payload either side accepts.
pub const MAX_FRAME: usize = 64 << 20;

/// A frame's bytes: header and payload.
#[must_use]
pub fn frame(t: u8, payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(HEAD + payload.len());
    b.extend_from_slice(&head(t, payload.len()));
    b.extend_from_slice(payload);
    b
}

/// A frame's header.
///
/// # Panics
/// For a payload past [`MAX_FRAME`] (a bug in the writer: nothing harness writes is that large).
#[must_use]
pub fn head(t: u8, len: usize) -> [u8; HEAD] {
    assert!(len <= MAX_FRAME, "harness-proto: a {len}-byte frame payload is past the {MAX_FRAME}-byte limit");
    let mut h = [0u8; HEAD];
    h[0] = t;
    h[1..].copy_from_slice(&(len as u32).to_be_bytes());
    h
}

/// Why a header is refused: a type this schema does not have (naming the JSON protocol's), or a length past the limit.
///
/// # Errors
/// As above.
pub fn check(h: &[u8; HEAD]) -> std::io::Result<(u8, usize)> {
    let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
    match h[0] {
        T_DATA | T_CBOR | T_EVENT => {}
        t @ (1 | 3 | 4) => {
            return Err(std::io::Error::other(format!(
                "frame type {t} is the JSON protocol harness spoke before schema {}; this side speaks only deterministic CBOR \
                 (types {T_CBOR} and {T_EVENT}, and {T_DATA} for terminal bytes): update the other side",
                crate::SCHEMA
            )));
        }
        t => return Err(std::io::Error::other(format!("frame type {t} is not in harness schema {}", crate::SCHEMA))),
    }
    if len > MAX_FRAME {
        return Err(std::io::Error::other(format!("a frame of {len} bytes is past the {MAX_FRAME}-byte limit")));
    }
    Ok((h[0], len))
}

/// Write one frame (header and payload in one write, so frames from two threads never interleave mid-frame).
///
/// # Errors
/// The writer's.
pub fn write(w: &mut impl Write, t: u8, payload: &[u8]) -> std::io::Result<()> {
    w.write_all(&frame(t, payload))?;
    w.flush()
}

/// Read one frame; `Ok(None)` on a clean end before its first byte.
///
/// # Errors
/// The reader's, a stream that ends mid-frame, or a header [`check`] refuses.
pub fn read(r: &mut impl Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut h = [0u8; HEAD];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let (t, len) = check(&h)?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some((t, buf)))
}

/// The first whole frame in `buf`: its type, its payload, and how many bytes it took; `Ok(None)` while `buf` holds only
/// part of one.
///
/// # Errors
/// A header [`check`] refuses.
pub fn split(buf: &[u8]) -> std::io::Result<Option<(u8, &[u8], usize)>> {
    let Some(h) = buf.first_chunk::<HEAD>() else { return Ok(None) };
    let (t, len) = check(h)?;
    Ok(buf.get(HEAD..HEAD + len).map(|p| (t, p, HEAD + len)))
}

/// The same over tokio's async IO, for the daemon and the CLI.
#[cfg(feature = "tokio")]
pub mod aio {
    use super::{HEAD, check, frame};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    /// Write one frame.
    ///
    /// # Errors
    /// The writer's.
    pub async fn write<W: AsyncWrite + Unpin>(w: &mut W, t: u8, payload: &[u8]) -> std::io::Result<()> {
        w.write_all(&frame(t, payload)).await?;
        w.flush().await
    }

    /// Write bytes that are already whole frames (a chat's history, its live events) as they are.
    ///
    /// # Errors
    /// The writer's.
    pub async fn write_frames<W: AsyncWrite + Unpin>(w: &mut W, frames: &[u8]) -> std::io::Result<()> {
        w.write_all(frames).await?;
        w.flush().await
    }

    /// Read one frame; `Ok(None)` on a clean end before its first byte.
    ///
    /// # Errors
    /// The reader's, a stream that ends mid-frame, or a header [`check`] refuses.
    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        let mut h = [0u8; HEAD];
        match r.read_exact(&mut h).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let (t, len) = check(&h)?;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).await?;
        Ok(Some((t, buf)))
    }
}
