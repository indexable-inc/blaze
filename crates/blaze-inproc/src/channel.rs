//! The channel between a run process and the daemon: [`frame`](crate::frame)s over the socket
//! (Unix) or pipe (Windows) the start hook connected with. Blocking: each end reads it on a
//! thread of its own, never on a plane worker.

use std::time::Duration;

use crate::frame::{self, Frame, Kind};
use crate::{Error, sys};

/// Bytes read at once from the stream; a longer frame grows the buffer once.
const CHUNK: usize = 64 << 10;

pub struct Channel {
    stream: sys::Stream,
    /// Bytes read and not yet handed out: `buf[start..end]`.
    buf: Vec<u8>,
    start: usize,
    end: usize,
    /// The length of the frame [`Channel::recv`] last returned, consumed at the next call.
    last: usize,
    out: Vec<u8>,
    seq: u32,
}

impl Channel {
    pub(crate) fn new(stream: sys::Stream) -> Channel {
        Channel { stream, buf: vec![0; CHUNK], start: 0, end: 0, last: 0, out: Vec::new(), seq: 0 }
    }

    /// The channel of the fd or handle `raw`, which the caller hands over.
    #[cfg(all(test, unix))]
    pub(crate) fn from_raw(raw: i64) -> Result<Channel, Error> {
        Ok(Channel::new(sys::Stream::from_raw(raw)?))
    }

    /// The socket under the channel, for the rendezvous to ask the OS about its peer.
    #[cfg(target_os = "macos")]
    pub(crate) fn raw(&self) -> i64 {
        self.stream.raw()
    }

    pub(crate) fn stream(&mut self) -> &mut sys::Stream {
        &mut self.stream
    }

    /// The stream, once nothing more is framed on it: the asks' doorbell.
    pub(crate) fn into_stream(self) -> sys::Stream {
        self.stream
    }

    /// How long a [`Channel::recv`] waits before it fails; `None` waits for ever.
    pub fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), Error> {
        self.stream.set_timeout(timeout)
    }

    /// A sequence number of this end's own, for a request.
    pub fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    /// Sends one frame, whole.
    pub fn send(&mut self, kind: Kind, seq: u32, body: &[u8]) -> Result<(), Error> {
        self.out.clear();
        frame::encode(kind, seq, body, &mut self.out).map_err(|it| Error::new(format!("sending a frame: {it}")))?;
        self.stream.write_all(&self.out)
    }

    /// The next frame, read in place: it lives until the next call. The peer hanging up is an
    /// error saying so.
    pub fn recv(&mut self) -> Result<Frame<'_>, Error> {
        self.start += std::mem::take(&mut self.last);
        loop {
            match frame::decode(&self.buf[self.start..self.end]) {
                Err(error) => return Err(Error::new(format!("reading the channel: {error}"))),
                Ok(Some((_, len))) => {
                    self.last = len;
                    let (frame, _) = frame::decode(&self.buf[self.start..self.end])
                        .ok()
                        .flatten()
                        .ok_or_else(|| Error::new("a frame that decoded once did not decode again"))?;
                    return Ok(frame);
                }
                Ok(None) => self.fill()?,
            }
        }
    }

    /// Sends a request of `kind` with a fresh sequence number and waits for the frame that
    /// answers it, of the same number. Only for an end that alone sends requests on the
    /// channel and expects nothing else meanwhile: another frame is an error.
    pub fn request(&mut self, kind: Kind, body: &[u8]) -> Result<Frame<'_>, Error> {
        let seq = self.next_seq();
        self.send(kind, seq, body)?;
        let frame = self.recv()?;
        if frame.seq != seq {
            return Err(Error::new(format!(
                "a frame of kind {:#06x} answering request {}, where request {seq} was asked",
                frame.kind.0, frame.seq
            )));
        }
        Ok(frame)
    }

    /// Reads more bytes, moving what is left to the front and growing for a long frame.
    fn fill(&mut self) -> Result<(), Error> {
        if self.start != 0 {
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        let need = match self.buf[..self.end].first_chunk::<{ frame::HEADER }>() {
            Some(header) => frame::Header::decode(header)
                .map(|it| frame::HEADER + it.len as usize)
                .map_err(|it| Error::new(format!("reading the channel: {it}")))?,
            None => frame::HEADER,
        };
        if need > self.buf.len() {
            self.buf.resize(need.next_multiple_of(CHUNK), 0);
        }
        let read = self.stream.read(&mut self.buf[self.end..])?;
        if read == 0 {
            return Err(Error::new("the other end of the channel hung up"));
        }
        self.end += read;
        Ok(())
    }
}
