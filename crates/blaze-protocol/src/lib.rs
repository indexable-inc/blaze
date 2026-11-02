//! What the daemon and its clients say to each other, and how: frames of a four-byte
//! big-endian length and then one CBOR item in RFC 8949 section 4.2.1's core deterministic
//! encoding ([`cbor`]). One value has one encoding, so a recorded session can be hashed,
//! replayed into a renderer and compared byte for byte with another (docs/roadmap.md, "The
//! agent API").
//!
//! The item of a frame is a [`Message`]: the [`SCHEMA`] it is written in, the id of the
//! request it belongs to and its body. JSON is only a rendering of the same records for
//! people, never the wire.

#![forbid(unsafe_code)]

mod keyed;

pub mod bytes;
pub mod cbor;
pub mod events;
pub mod follow;

#[doc(hidden)]
pub use serde as __serde;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::{
    cell::RefCell,
    io::{self, IoSlice, Read, Write},
    thread::LocalKey,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// The version of the schema of every body: a reader of another refuses the message by
/// name rather than guess at its fields.
pub const SCHEMA: u32 = 2;

/// The longest item a frame carries: a build names every file it read.
pub const LONGEST_FRAME: u32 = 16 << 20;

in_key_order! {
    /// One frame's item: a body of [`SCHEMA`], answering or asking request `id`. Replies and
    /// events carry the id of the request they belong to, so a connection can carry several.
    // clone: two words and `T`, `Clone` only where `T` is; the daemon hands one to each subscriber.
    #[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct Message<T> {
        pub schema: u32,
        pub id: u64,
        pub body: T,
    }
    keys [id, body, schema]
}

impl<T> Message<T> {
    pub fn new(id: u64, body: T) -> Message<T> {
        Message { schema: SCHEMA, id, body }
    }
}

/// Writes `item`, one encoded CBOR item, as one frame: its length and the item in one
/// vectored write where `to` takes both, copying neither.
pub fn write_frame(to: &mut impl Write, item: &[u8]) -> io::Result<()> {
    let len = frame_len(item.len())
        .map_err(|said| io::Error::new(io::ErrorKind::InvalidInput, said))?;
    write_both(to, &len.to_be_bytes(), item)
}

/// The length a frame of `len` bytes says, or why it can have none.
fn frame_len(len: usize) -> Result<u32, String> {
    u32::try_from(len)
        .ok()
        .filter(|&len| len <= LONGEST_FRAME)
        // alloc: error path, a frame too long to send.
        .ok_or_else(|| format!("a frame of {len} bytes, longer than {LONGEST_FRAME}"))
}

/// All of `head` and then all of `body`, in one write for a writer that takes them both.
pub fn write_both(to: &mut impl Write, mut head: &[u8], mut body: &[u8]) -> io::Result<()> {
    while !head.is_empty() {
        match to.write_vectored(&[IoSlice::new(head), IoSlice::new(body)]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) if n < head.len() => head = &head[n..],
            Ok(n) => {
                body = &body[n - head.len()..];
                head = &[];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    to.write_all(body)
}

/// `value` as one frame on the end of `out`, encoded where it goes; on an error `out` is left
/// as it was.
pub fn append_frame<T: Serialize + ?Sized>(
    writer: &mut cbor::Writer,
    out: &mut Vec<u8>,
    value: &T,
) -> io::Result<()> {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    if let Err(error) = writer.append(out, value) {
        out.truncate(start);
        return Err(invalid(error));
    }
    match frame_len(out.len() - start - 4) {
        Ok(len) => {
            out[start..start + 4].copy_from_slice(&len.to_be_bytes());
            Ok(())
        }
        Err(said) => {
            out.truncate(start);
            Err(io::Error::new(io::ErrorKind::InvalidInput, said))
        }
    }
}

/// The item of the next frame, or `None` when the stream ends before one begins. A stream
/// that ends inside a frame is one whose writer did not live to finish it.
pub fn read_frame(from: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    // alloc: the item is handed out; `read_frame_into` and `receive` keep their buffer.
    let mut item = Vec::new();
    Ok(read_frame_into(from, &mut item)?.then_some(item))
}

/// [`read_frame`] into `item`, a buffer kept from frame to frame: false when the stream ends
/// before a frame begins.
pub fn read_frame_into(from: &mut impl Read, item: &mut Vec<u8>) -> io::Result<bool> {
    item.clear();
    let mut len = [0; 4];
    let mut read = 0;
    while read < len.len() {
        match from.read(&mut len[read..]) {
            Ok(0) if read == 0 => return Ok(false),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => read += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let len = u32::from_be_bytes(len);
    if len > LONGEST_FRAME {
        // alloc: error path, a peer's frame too long to read.
        let said = format!("a frame of {len} bytes, longer than {LONGEST_FRAME}");
        return Err(io::Error::new(io::ErrorKind::InvalidData, said));
    }
    // Room for at most a megabyte before the bytes are here, the length being the peer's
    // word: a frame of that much is read in one go.
    item.reserve((len as usize).min(1 << 20));
    from.take(u64::from(len)).read_to_end(item)?;
    if item.len() != len as usize {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(true)
}

/// Writes `value` as one frame, in one write, encoded into a buffer this thread keeps: a warm
/// send allocates nothing.
pub fn send<T: Serialize + ?Sized>(to: &mut impl Write, value: &T) -> io::Result<()> {
    with_kept(&SENDING, |kept| {
        kept.frame.clear();
        append_frame(&mut kept.writer, &mut kept.frame, value)?;
        to.write_all(&kept.frame)
    })
}

/// The value of the next frame, or `None` when the stream ends before one begins, read into a
/// buffer this thread keeps. An item not in the deterministic encoding is refused, naming the
/// first offence.
pub fn receive<T: DeserializeOwned>(from: &mut impl Read) -> io::Result<Option<T>> {
    with_kept(&RECEIVING, |kept| {
        if !read_frame_into(from, &mut kept.frame)? {
            return Ok(None);
        }
        cbor::from_slice(&kept.frame).map(Some).map_err(invalid)
    })
}

/// The most bytes [`send`] and [`receive`] keep between frames: one big frame does not hold
/// its size for good.
const KEPT_FRAME: usize = 1 << 20;

/// A frame buffer and encoder kept on a thread from one [`send`] or [`receive`] to the next.
#[derive(Default)]
struct Kept {
    writer: cbor::Writer,
    frame: Vec<u8>,
}

thread_local! {
    static SENDING: RefCell<Kept> = RefCell::default();
    static RECEIVING: RefCell<Kept> = RefCell::default();
}

/// `f` on this thread's `kept`, trimmed after to [`KEPT_FRAME`].
fn with_kept<R>(kept: &'static LocalKey<RefCell<Kept>>, f: impl FnOnce(&mut Kept) -> R) -> R {
    kept.with(|cell| match cell.try_borrow_mut() {
        Ok(mut kept) => {
            let out = f(&mut kept);
            if kept.frame.capacity() > KEPT_FRAME {
                drop(std::mem::take(&mut kept.frame));
            }
            out
        }
        // A value whose own `Serialize` or `Deserialize` sends or receives on this thread: its
        // frame gets a buffer of its own.
        Err(_) => f(&mut Kept::default()),
    })
}

fn invalid(error: cbor::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
