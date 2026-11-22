//! zlib framing (RFC 1950): a two-byte header, a raw DEFLATE stream, and the Adler-32 of the
//! output, big-endian. Git's loose objects and the objects of a pack are zlib streams.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::{
    checksum::Adler32,
    deflate::{Deflate, DeflateError, Level},
    error::{Error, Format, Why},
    inflate::{Fed, Inflate, InflateInto, Limits},
};

const HEADER: u64 = 2;

/// The bytes a trailer reads after the bit buffer's tail, and those past it.
pub(crate) struct Trailer<const N: usize> {
    pub(crate) bytes: [u8; N],
    pub(crate) have: usize,
    pub(crate) over: [u8; 8],
    pub(crate) over_len: usize,
}

impl<const N: usize> Trailer<N> {
    pub(crate) const EMPTY: Trailer<N> = Trailer { bytes: [0; N], have: 0, over: [0; 8], over_len: 0 };

    /// The inflater's tail taken: the trailer's first bytes, then what lies past the stream.
    pub(crate) fn from_tail(&mut self, tail: &[u8]) {
        for &byte in tail {
            if self.have < N {
                self.bytes[self.have] = byte;
                self.have += 1;
            } else {
                self.over[self.over_len] = byte;
                self.over_len += 1;
            }
        }
    }

    /// Bytes of `input` taken until the trailer is whole.
    pub(crate) fn take(&mut self, input: &[u8]) -> usize {
        let n = (N - self.have).min(input.len());
        self.bytes[self.have..self.have + n].copy_from_slice(&input[..n]);
        self.have += n;
        n
    }

    pub(crate) fn is_whole(&self) -> bool {
        self.have == N
    }
}

/// Whether an encoder has written its frame's header yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Begun {
    /// Nothing written.
    No,
    /// The header is out.
    Written,
}

/// Where a zlib frame is: one state, no flags beside it (C5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// `have` of the header's two bytes taken.
    Header { have: u8 },
    /// The DEFLATE stream, then the trailer, until the trailer is whole and checked.
    Stream,
    /// The trailer checked: the stream is read.
    Done,
}

/// The zlib frame around either inflater: the header, the running Adler-32 and the trailer.
struct Frame {
    header: [u8; 2],
    stage: Stage,
    adler: Adler32,
    trailer: Trailer<4>,
    /// Bytes of input taken, the frame's included.
    taken: u64,
}

impl Frame {
    const NEW: Frame = Frame {
        header: [0; 2],
        stage: Stage::Header { have: 0 },
        adler: Adler32::new(),
        trailer: Trailer::EMPTY,
        taken: 0,
    };

    /// The header's bytes taken from `input`; whether it is whole and good.
    fn head(&mut self, input: &[u8], at: &mut usize) -> Result<bool, Error> {
        while let Stage::Header { have } = self.stage
            && *at < input.len()
        {
            self.header[usize::from(have)] = input[*at];
            *at += 1;
            match have {
                0 => self.stage = Stage::Header { have: 1 },
                _ => {
                    self.stage = Stage::Stream;
                    check_header(self.header[0], self.header[1]).map_err(|why| Error::new(Format::Zlib, 0, why))?;
                }
            }
        }
        Ok(!matches!(self.stage, Stage::Header { .. }))
    }

    /// After the stream ended (`inflated` its input bytes): the trailer's bytes taken and
    /// checked once whole.
    fn tail(&mut self, input: &[u8], at: &mut usize, inflated: u64) -> Result<(), Error> {
        if self.stage == Stage::Done {
            return Ok(());
        }
        *at += self.trailer.take(&input[*at..]);
        if self.trailer.is_whole() {
            let stored = u32::from_be_bytes(self.trailer.bytes);
            let computed = self.adler.finish();
            if stored != computed {
                return Err(Error::new(Format::Zlib, HEADER + inflated, Why::Adler32 { stored, computed }));
            }
            self.stage = Stage::Done;
        }
        Ok(())
    }

    fn is_done(&self) -> bool {
        self.stage == Stage::Done
    }

    fn finish(&self) -> Result<(), Error> {
        match self.stage {
            Stage::Done => Ok(()),
            Stage::Header { .. } | Stage::Stream => Err(Error::new(Format::Zlib, self.taken, Why::Truncated)),
        }
    }

    fn over(&self) -> &[u8] {
        &self.trailer.over[..self.trailer.over_len]
    }

    fn stream_len(&self) -> u64 {
        self.taken - self.trailer.over_len as u64
    }
}

/// A streaming zlib reader appending to the caller's vector.
pub struct ZlibDecoder<A: Allocator> {
    inflate: Inflate<A>,
    frame: Frame,
}

impl<A: Allocator + Clone> ZlibDecoder<A> {
    pub fn new_in(limits: Limits, alloc: A) -> ZlibDecoder<A> {
        ZlibDecoder { inflate: Inflate::new_in(limits, alloc), frame: Frame::NEW }
    }
}

impl<A: Allocator> ZlibDecoder<A> {
    /// Takes what it can of `input`, appending the output to `out`; see [`Inflate::feed`].
    pub fn feed<B: Allocator>(&mut self, input: &[u8], out: &mut Vec<u8, B>) -> Result<Fed, Error> {
        let mut at = 0;
        if self.frame.head(input, &mut at)? {
            if !self.inflate.is_done() {
                let before = out.len();
                let fed = self.inflate.feed(&input[at..], out).map_err(|e| e.framed(Format::Zlib, HEADER))?;
                self.frame.adler.update(&out[before..]);
                at += fed.consumed;
                if fed.done {
                    self.frame.trailer.from_tail(self.inflate.tail());
                }
            }
            if self.inflate.is_done() {
                self.frame.tail(input, &mut at, self.inflate.taken())?;
            }
        }
        self.frame.taken += at as u64;
        Ok(Fed { consumed: at, done: self.frame.is_done() })
    }

    /// The input ended: an error unless the stream did.
    pub fn finish(&self) -> Result<(), Error> {
        self.frame.finish()
    }

    /// Bytes taken that lie past the stream's end (the bit buffer's read-ahead): the next
    /// thing's first bytes.
    pub fn over(&self) -> &[u8] {
        self.frame.over()
    }

    /// Bytes of the input the stream itself took: what was taken less [`ZlibDecoder::over`].
    pub fn stream_len(&self) -> u64 {
        self.frame.stream_len()
    }

    pub fn written(&self) -> u64 {
        self.inflate.written()
    }
}

/// A streaming zlib reader writing into one slice the caller sized (see [`InflateInto`]).
pub struct ZlibInto {
    inflate: InflateInto,
    frame: Frame,
}

impl ZlibInto {
    pub const fn new(limits: Limits) -> ZlibInto {
        ZlibInto { inflate: InflateInto::new(limits), frame: Frame::NEW }
    }

    /// Takes what it can of `input`, writing the output into `out` after the
    /// [`ZlibInto::written`] bytes already there; see [`InflateInto::feed`].
    pub fn feed(&mut self, input: &[u8], out: &mut [u8]) -> Result<Fed, Error> {
        let mut at = 0;
        if self.frame.head(input, &mut at)? {
            if !self.inflate.is_done() {
                let before = self.inflate.written() as usize;
                let fed = self.inflate.feed(&input[at..], out).map_err(|e| e.framed(Format::Zlib, HEADER))?;
                self.frame.adler.update(&out[before..self.inflate.written() as usize]);
                at += fed.consumed;
                if fed.done {
                    self.frame.trailer.from_tail(self.inflate.tail());
                }
            }
            if self.inflate.is_done() {
                self.frame.tail(input, &mut at, self.inflate.taken())?;
            }
        }
        self.frame.taken += at as u64;
        Ok(Fed { consumed: at, done: self.frame.is_done() })
    }

    pub fn finish(&self) -> Result<(), Error> {
        self.frame.finish()
    }

    pub fn over(&self) -> &[u8] {
        self.frame.over()
    }

    pub fn stream_len(&self) -> u64 {
        self.frame.stream_len()
    }

    /// Bytes written into the slice.
    pub fn written(&self) -> u64 {
        self.inflate.written()
    }
}

/// The header refused or not: method 8, a window of at most 32 KiB, the check, no dictionary.
fn check_header(cmf: u8, flg: u8) -> Result<(), Why> {
    let check = (u16::from(cmf) << 8 | u16::from(flg)) % 31 == 0;
    if cmf & 0x0f != 8 || cmf >> 4 > 7 || !check {
        return Err(Why::ZlibHeader { cmf, flg });
    }
    if flg & 0x20 != 0 {
        return Err(Why::ZlibDictionary);
    }
    Ok(())
}

/// A streaming zlib writer. Its header says the level as zlib's does (`FLEVEL` 0 for store and
/// fast, 2 for default), so equal input and level give equal bytes.
pub struct ZlibEncoder<A: Allocator> {
    deflate: Deflate<A>,
    level: Level,
    adler: Adler32,
    head: Begun,
}

impl<A: Allocator + Clone> ZlibEncoder<A> {
    pub fn new_in(level: Level, alloc: A) -> ZlibEncoder<A> {
        ZlibEncoder { deflate: Deflate::new_in(level, alloc), level, adler: Adler32::new(), head: Begun::No }
    }
}

impl<A: Allocator> ZlibEncoder<A> {
    fn start<B: Allocator>(&mut self, out: &mut Vec<u8, B>) -> Result<(), DeflateError> {
        if self.head == Begun::No {
            let flg = match self.level {
                Level::Store | Level::Fast => 0x01,
                Level::Default => 0x9c,
            };
            out.try_extend_from_slice(&[0x78, flg]).map_err(DeflateError::Alloc)?;
            self.head = Begun::Written;
        }
        Ok(())
    }

    pub fn write<B: Allocator>(&mut self, input: &[u8], out: &mut Vec<u8, B>) -> Result<(), DeflateError> {
        self.start(out)?;
        self.adler.update(input);
        self.deflate.write(input, out)
    }

    pub fn finish<B: Allocator>(&mut self, out: &mut Vec<u8, B>) -> Result<(), DeflateError> {
        self.start(out)?;
        self.deflate.finish(out)?;
        out.try_extend_from_slice(&self.adler.finish().to_be_bytes()).map_err(DeflateError::Alloc)
    }
}
