//! gzip framing (RFC 1952): a header (ten fixed bytes, then an optional extra field, name,
//! comment and header CRC), a raw DEFLATE stream, and the CRC-32 and size of the output,
//! little-endian. A crates.io `.crate` file is one gzip member holding a tar archive.
//!
//! One member is read: bytes after its trailer are the caller's ([`GzipDecoder::over`] and
//! what [`GzipDecoder::feed`] leaves unconsumed). The header is bounded by
//! [`Limits::most_header`]: a name with no end is a typed refusal, not a buffer that grows.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_own::{
    Vec,
    planted::{NoPlant, Planted},
};

use crate::{
    checksum::Crc32,
    deflate::{Deflate, DeflateError, Level},
    error::{Error, Format, Why},
    inflate::{Fed, Limits, PlantedInflate},
    plant::DeflatePlant,
    zlib::{Begun, Trailer},
};

const FHCRC: u8 = 0x02;
const FEXTRA: u8 = 0x04;
const FNAME: u8 = 0x08;
const FCOMMENT: u8 = 0x10;
const RESERVED: u8 = 0xe0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Head {
    Fixed { have: u8 },
    ExtraLen { have: u8, len: u16 },
    Extra { left: u16 },
    Name,
    Comment,
    Crc { have: u8, crc: u16 },
    /// The DEFLATE stream, then the trailer, until the trailer is whole and checked.
    Body,
    /// The trailer checked: the member is read.
    Done,
}

/// A streaming gzip reader of one member. `P` is the plant seam (D165): shipped code names
/// [`GzipDecoder`], this with [`NoPlant`].
pub struct PlantedGzipDecoder<A: Allocator, P> {
    inflate: PlantedInflate<A, P>,
    head: Head,
    fixed: [u8; 10],
    /// The header's bytes so far, and their CRC-32 (whose low half is the header CRC).
    header_len: u64,
    header_crc: Crc32,
    most_header: u32,
    crc: Crc32,
    trailer: Trailer<8>,
    taken: u64,
    plant: P,
}

/// The gzip reader as it ships: [`PlantedGzipDecoder`] with no plant.
pub type GzipDecoder<A> = PlantedGzipDecoder<A, NoPlant>;

impl<A: Allocator + Clone> PlantedGzipDecoder<A, NoPlant> {
    pub fn new_in(limits: Limits, alloc: A) -> GzipDecoder<A> {
        PlantedGzipDecoder::new_planted(limits, alloc, NoPlant)
    }
}

impl<A: Allocator + Clone, P: Planted<DeflatePlant>> PlantedGzipDecoder<A, P> {
    /// [`GzipDecoder::new_in`] with its plant seam: `plant` is a control's [`DeflatePlant`], given
    /// to the inflater inside too.
    pub fn new_planted(limits: Limits, alloc: A, plant: P) -> PlantedGzipDecoder<A, P> {
        PlantedGzipDecoder {
            inflate: PlantedInflate::new_planted(limits, alloc, plant),
            head: Head::Fixed { have: 0 },
            fixed: [0; 10],
            header_len: 0,
            header_crc: Crc32::new(),
            most_header: limits.most_header,
            crc: Crc32::new(),
            trailer: Trailer::EMPTY,
            taken: 0,
            plant,
        }
    }
}

impl<A: Allocator, P: Planted<DeflatePlant>> PlantedGzipDecoder<A, P> {
    fn flags(&self) -> u8 {
        self.fixed[3]
    }

    /// Whether a header byte is still to come.
    fn in_header(&self) -> bool {
        !matches!(self.head, Head::Body | Head::Done)
    }

    /// The state after the part of the header `from` ends.
    fn next_part(&self, from: Head) -> Head {
        let flags = self.flags();
        let order = [
            (FEXTRA, Head::ExtraLen { have: 0, len: 0 }),
            (FNAME, Head::Name),
            (FCOMMENT, Head::Comment),
            (FHCRC, Head::Crc { have: 0, crc: 0 }),
        ];
        let rank = match from {
            Head::Fixed { .. } => 0,
            Head::ExtraLen { .. } | Head::Extra { .. } => 1,
            Head::Name => 2,
            Head::Comment => 3,
            Head::Crc { .. } | Head::Body | Head::Done => 4,
        };
        for &(flag, part) in &order[rank..] {
            if flags & flag != 0 {
                return part;
            }
        }
        Head::Body
    }

    /// One header byte; the header CRC's own bytes are not hashed.
    fn header_byte(&mut self, byte: u8) -> Result<(), Why> {
        let hashed = !matches!(self.head, Head::Crc { .. });
        self.header_len += 1;
        if self.header_len > 10 + u64::from(self.most_header) {
            return Err(Why::GzipHeaderOver { most: self.most_header });
        }
        if hashed {
            self.header_crc.update(&[byte]);
        }
        self.head = match self.head {
            Head::Fixed { have } => {
                self.fixed[usize::from(have)] = byte;
                match have {
                    1 if (self.fixed[0], byte) != (0x1f, 0x8b) => {
                        return Err(Why::GzipMagic { id1: self.fixed[0], id2: byte });
                    }
                    2 if byte != 8 => return Err(Why::GzipMethod(byte)),
                    3 if byte & RESERVED != 0 => return Err(Why::GzipFlags(byte)),
                    9 => self.next_part(Head::Fixed { have }),
                    _ => Head::Fixed { have: have + 1 },
                }
            }
            Head::ExtraLen { have: 0, .. } => Head::ExtraLen { have: 1, len: u16::from(byte) },
            Head::ExtraLen { len, .. } => {
                let len = len | u16::from(byte) << 8;
                match len {
                    0 => self.next_part(Head::Extra { left: 0 }),
                    _ => Head::Extra { left: len },
                }
            }
            Head::Extra { left: 1 } => self.next_part(Head::Extra { left: 0 }),
            Head::Extra { left } => Head::Extra { left: left - 1 },
            Head::Name if byte == 0 => self.next_part(Head::Name),
            Head::Comment if byte == 0 => self.next_part(Head::Comment),
            Head::Name => Head::Name,
            Head::Comment => Head::Comment,
            Head::Crc { have: 0, .. } => Head::Crc { have: 1, crc: u16::from(byte) },
            Head::Crc { crc, .. } => {
                let stored = crc | u16::from(byte) << 8;
                let computed = (self.header_crc.finish() & 0xffff) as u16;
                if stored != computed {
                    return Err(Why::GzipHeaderCrc { stored, computed });
                }
                Head::Body
            }
            Head::Body => Head::Body,
            Head::Done => Head::Done,
        };
        Ok(())
    }

    /// Takes what it can of `input`, appending the output to `out`; see [`crate::Inflate::feed`].
    pub fn feed<B: Allocator>(&mut self, input: &[u8], out: &mut Vec<u8, B>) -> Result<Fed, Error> {
        let mut at = 0;
        while self.in_header() && at < input.len() {
            let byte = input[at];
            self.header_byte(byte).map_err(|why| Error::new(Format::Gzip, self.taken + at as u64, why))?;
            at += 1;
        }
        if self.in_header() {
            self.taken += at as u64;
            return Ok(Fed { consumed: at, done: false });
        }
        if !self.inflate.is_done() {
            let before = out.len();
            let fed = self.inflate.feed(&input[at..], out).map_err(|e| e.framed(Format::Gzip, self.header_len))?;
            self.crc.update(&out[before..]);
            at += fed.consumed;
            if fed.done {
                self.trailer.from_tail(self.inflate.tail());
            }
        }
        if self.inflate.is_done() && self.head != Head::Done {
            at += self.trailer.take(&input[at..]);
            if self.trailer.is_whole() {
                self.check_trailer()?;
                self.head = Head::Done;
            }
        }
        self.taken += at as u64;
        Ok(Fed { consumed: at, done: self.head == Head::Done })
    }

    fn check_trailer(&self) -> Result<(), Error> {
        let t = self.trailer.bytes;
        let fault = |why| Error::new(Format::Gzip, self.header_len + self.inflate.taken(), why);
        let stored = u32::from_le_bytes([t[0], t[1], t[2], t[3]]);
        let computed = self.crc.finish();
        if stored != computed && !self.plant.has(DeflatePlant::GzipCrcUnchecked) {
            return Err(fault(Why::Crc32 { stored, computed }));
        }
        let stored = u32::from_le_bytes([t[4], t[5], t[6], t[7]]);
        // ISIZE is the size mod 2^32: the truncation is the format's.
        let computed = self.inflate.written() as u32;
        if stored != computed {
            return Err(fault(Why::Size { stored, computed }));
        }
        Ok(())
    }

    /// The input ended: an error unless the member did.
    pub fn finish(&self) -> Result<(), Error> {
        match self.head {
            Head::Done => Ok(()),
            _ => Err(Error::new(Format::Gzip, self.taken, Why::Truncated)),
        }
    }

    /// Bytes taken that lie past the member's end.
    pub fn over(&self) -> &[u8] {
        &self.trailer.over[..self.trailer.over_len]
    }

    /// Bytes of the input the member itself took.
    pub fn stream_len(&self) -> u64 {
        self.taken - self.trailer.over_len as u64
    }

    pub fn written(&self) -> u64 {
        self.inflate.written()
    }
}

/// A streaming gzip writer. Its header is fixed (no name, no time: MTIME 0, OS 255
/// "unknown"; XFL 4 for the fast level, else 0), so equal input and level give equal bytes.
pub struct GzipEncoder<A: Allocator> {
    deflate: Deflate<A>,
    level: Level,
    crc: Crc32,
    size: u64,
    head: Begun,
}

impl<A: Allocator + Clone> GzipEncoder<A> {
    pub fn new_in(level: Level, alloc: A) -> GzipEncoder<A> {
        GzipEncoder { deflate: Deflate::new_in(level, alloc), level, crc: Crc32::new(), size: 0, head: Begun::No }
    }
}

impl<A: Allocator> GzipEncoder<A> {
    fn start<B: Allocator>(&mut self, out: &mut Vec<u8, B>) -> Result<(), DeflateError> {
        if self.head == Begun::No {
            let xfl = match self.level {
                Level::Fast => 4,
                Level::Store | Level::Default => 0,
            };
            out.try_extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 0xff]).map_err(DeflateError::Alloc)?;
            self.head = Begun::Written;
        }
        Ok(())
    }

    pub fn write<B: Allocator>(&mut self, input: &[u8], out: &mut Vec<u8, B>) -> Result<(), DeflateError> {
        self.start(out)?;
        self.crc.update(input);
        self.size += input.len() as u64;
        self.deflate.write(input, out)
    }

    pub fn finish<B: Allocator>(&mut self, out: &mut Vec<u8, B>) -> Result<(), DeflateError> {
        self.start(out)?;
        self.deflate.finish(out)?;
        let mut trailer = [0u8; 8];
        trailer[..4].copy_from_slice(&self.crc.finish().to_le_bytes());
        // ISIZE is the size mod 2^32: the truncation is the format's.
        trailer[4..].copy_from_slice(&(self.size as u32).to_le_bytes());
        out.try_extend_from_slice(&trailer).map_err(DeflateError::Alloc)
    }
}
