//! An MP4 (ISO/IEC 14496-12, ISO BMFF) muxer for one HEVC track, as a pure state machine:
//! packets in ([`Muxer::push`]), bytes out ([`Muxer::bytes`], [`Muxer::finish`]). It reads no
//! clock and owns no file; the caller writes the bytes where it likes, in the order given.
//!
//! - **Times are data.** Creation and modification times are 0, never the wall clock; every
//!   time is a packet's, in the track's timescale. The same packets give the same bytes.
//! - **Two layouts** ([`Layout`]). `Progressive` holds every sample until [`Muxer::finish`],
//!   then writes `ftyp`, `moov` (first, so playback starts while the file downloads) and one
//!   `mdat`. `Fragmented` writes `ftyp` and an empty `moov` with `mvex`, then one `moof` and
//!   `mdat` for each run of packets from a key frame to the next: bytes come out as the
//!   packets go in, and memory holds one fragment.
//! - **The track**: `hvc1` with the encoder's `hvcC`, and the colour as `colr` (`nclx`), with
//!   HDR10's `mdcv` and `clli` when given (the HDR10 tagging harness-video's `hdr.rs` and
//!   `encode.rs` describe).
//! - **Timing**: decode times start at 0 at the first packet; each sample's composition offset
//!   is its presentation time less its decode time (never negative, `ctts`/`trun` version 0);
//!   an edit list starts the presentation at the first packet's presentation time, as
//!   AVAssetWriter writes it. A sample's duration is the next sample's decode time less its
//!   own; the last one's is its packet's.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::{alloc::Allocator, fmt};

use super::{Colour, Packet};

/// How the container is laid out (module docs).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Layout {
    /// `ftyp`, `moov`, `mdat`, all at [`Muxer::finish`].
    Progressive,
    /// `ftyp`, `moov` with `mvex`, then `moof` and `mdat` per key frame's run.
    Fragmented,
}

/// The one track: HEVC `width` x `height` (each at most 65535, the sample entry's field),
/// times in ticks of `timescale` a second, `hvcc` the encoder's `HEVCDecoderConfigurationRecord`
/// (lengths 4 bytes, as [`Packet`] holds them).
#[derive(Clone, Copy, Debug)]
pub struct Track<'a> {
    pub width: u32,
    pub height: u32,
    pub timescale: u32,
    pub hvcc: &'a [u8],
    pub colour: Colour,
}

/// The track's id: there is one track.
const TRACK_ID: u32 = 1;
/// `sample_flags` of a key frame: depends on no other sample.
const KEY_FLAGS: u32 = 0x0200_0000;
/// `sample_flags` of another frame: depends on others, and is not a sync sample.
const NON_KEY_FLAGS: u32 = 0x0101_0000;
/// `tfhd`'s default-base-is-moof: data offsets count from the `moof`.
const TFHD_BASE_IS_MOOF: u32 = 0x02_0000;
/// `trun`'s fields: data offset, then each sample's duration, size, flags and composition offset.
const TRUN_FIELDS: u32 = 0x0001 | 0x0100 | 0x0200 | 0x0400 | 0x0800;
/// The identity matrix of `mvhd` and `tkhd` (16.16, with 2.30 for the last column).
const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
/// `und`, ISO 639-2's undetermined language, packed as `mdhd` holds it.
const LANGUAGE_UND: u16 = (((b'u' - 0x60) as u16) << 10) | (((b'n' - 0x60) as u16) << 5) | ((b'd' - 0x60) as u16);

/// One sample, held until it is written.
#[derive(Clone, Copy, Debug)]
struct Sample {
    size: u32,
    /// Decode time, from the first packet's.
    dt: i64,
    /// Presentation time less decode time.
    offset: u32,
    /// Its packet's duration until the next sample's decode time replaces it.
    duration: u32,
    key: bool,
}

/// Packets in, the bytes of an MP4 out (module docs). Every buffer is in the allocator handed
/// to [`Muxer::new`].
pub struct Muxer<A: Allocator + Clone> {
    layout: Layout,
    width: u16,
    height: u16,
    timescale: u32,
    hvcc: Vec<u8, A>,
    colour: Colour,
    /// Samples not yet written: all of them, or the fragment being gathered.
    samples: Vec<Sample, A>,
    /// Their bytes, in decode order.
    data: Vec<u8, A>,
    /// Bytes ready for the caller.
    out: Vec<u8, A>,
    /// Bytes the caller has taken.
    taken: u64,
    /// The first packet's decode and presentation times.
    first: Option<(i64, i64)>,
    last_dts: i64,
    /// Packets pushed.
    count: u64,
    /// The latest presentation end, from the first packet's presentation time.
    end: i64,
    /// Fragments written.
    sequence: u32,
    alloc: A,
}

impl<A: Allocator + Clone> Muxer<A> {
    /// A muxer of `track` laid out as `layout`, every buffer in `alloc`.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn new(track: &Track<'_>, layout: Layout, alloc: A) -> Result<Muxer<A>, MuxError> {
        let width = u16::try_from(track.width)
            .ok()
            .filter(|&w| w > 0)
            .ok_or(MuxError::Config { field: "width", why: "is not in 1..=65535" })?;
        let height = u16::try_from(track.height)
            .ok()
            .filter(|&h| h > 0)
            .ok_or(MuxError::Config { field: "height", why: "is not in 1..=65535" })?;
        if track.timescale == 0 {
            return Err(MuxError::Config { field: "timescale", why: "is 0" });
        }
        // configurationVersion 1, and the fixed header before the arrays (23 bytes).
        if track.hvcc.len() < 23 || track.hvcc[0] != 1 {
            return Err(MuxError::Config { field: "hvcc", why: "is not an HEVCDecoderConfigurationRecord (version 1, 23 bytes or more)" });
        }
        let mut hvcc = Vec::new_in(alloc.clone());
        hvcc.try_reserve_exact(track.hvcc.len())
            .map_err(|_| MuxError::OutOfMemory { what: "the track's hvcC", asked: track.hvcc.len() })?;
        hvcc.extend_from_slice(track.hvcc);
        Ok(Muxer {
            layout,
            width,
            height,
            timescale: track.timescale,
            hvcc,
            colour: track.colour,
            samples: Vec::new_in(alloc.clone()),
            data: Vec::new_in(alloc.clone()),
            out: Vec::new_in(alloc.clone()),
            taken: 0,
            first: None,
            last_dts: 0,
            count: 0,
            end: 0,
            sequence: 0,
            alloc,
        })
    }

    /// The next packet, in decode order: the first a key frame, decode times rising, no
    /// presentation before the first packet's, presentation never before decode.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn push<B: Allocator>(&mut self, packet: &Packet<B>) -> Result<(), MuxError> {
        let n = self.count;
        if packet.timescale != self.timescale {
            return Err(MuxError::Timescale { sample: n, got: packet.timescale, track: self.timescale });
        }
        let (d0, p0) = match self.first {
            Some(first) => {
                if packet.dts <= self.last_dts {
                    return Err(MuxError::Order { sample: n, why: "its decode time is not after the one before" });
                }
                first
            }
            None => {
                if !packet.key {
                    return Err(MuxError::Order { sample: n, why: "the first packet is not a key frame" });
                }
                (packet.dts, packet.pts)
            }
        };
        if packet.pts < p0 {
            return Err(MuxError::Order { sample: n, why: "it is presented before the first packet" });
        }
        let offset = packet.pts.checked_sub(packet.dts).ok_or(MuxError::Range {
            sample: n,
            what: "presentation less decode time",
            value: packet.pts,
        })?;
        if offset < 0 {
            return Err(MuxError::Order { sample: n, why: "it is presented before it is decoded" });
        }
        let offset = u32::try_from(offset).map_err(|_| MuxError::Range { sample: n, what: "composition offset", value: offset })?;
        let dt = packet.dts.checked_sub(d0).ok_or(MuxError::Range { sample: n, what: "decode time", value: packet.dts })?;
        let size = u32::try_from(packet.data.len()).map_err(|_| MuxError::Range {
            sample: n,
            what: "sample size",
            value: i64::try_from(packet.data.len()).unwrap_or(i64::MAX),
        })?;
        let shown_end = packet.pts.saturating_sub(p0).saturating_add(i64::from(packet.duration));
        if self.layout == Layout::Fragmented && packet.key && !self.samples.is_empty() {
            self.fragment(Some(dt))?;
        }
        self.samples
            .try_reserve(1)
            .map_err(|_| MuxError::OutOfMemory { what: "the sample table", asked: size_of::<Sample>() })?;
        self.data
            .try_reserve(packet.data.len())
            .map_err(|_| MuxError::OutOfMemory { what: "the samples' bytes", asked: packet.data.len() })?;
        self.samples.push(Sample { size, dt, offset, duration: packet.duration, key: packet.key });
        self.data.extend_from_slice(&packet.data);
        self.first = Some((d0, p0));
        self.last_dts = packet.dts;
        self.end = self.end.max(shown_end);
        self.count += 1;
        Ok(())
    }

    /// The bytes ready so far, to be written after those taken before. Progressive: none until
    /// [`Muxer::finish`].
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn bytes(&mut self) -> Vec<u8, A> {
        let ready = core::mem::replace(&mut self.out, Vec::new_in(self.alloc.clone()));
        self.taken += ready.len() as u64;
        ready
    }

    /// The rest of the bytes: the whole file (progressive) or the last fragment.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn finish(mut self) -> Result<Vec<u8, A>, MuxError> {
        if self.count == 0 {
            return Err(MuxError::Empty);
        }
        match self.layout {
            Layout::Fragmented => self.fragment(None)?,
            Layout::Progressive => self.progressive()?,
        }
        Ok(self.out)
    }

    /// Each held sample's duration: the next one's decode time less its own; the last one's is
    /// `next` less its own, or its packet's.
    fn durations(&mut self, next: Option<i64>) -> Result<(), MuxError> {
        let first = self.count - self.samples.len() as u64;
        for i in 0..self.samples.len() {
            let following = match self.samples.get(i + 1) {
                Some(s) => Some(s.dt),
                None => next,
            };
            if let Some(following) = following {
                let delta = following - self.samples[i].dt;
                self.samples[i].duration = u32::try_from(delta).map_err(|_| MuxError::Range {
                    sample: first + i as u64,
                    what: "sample duration",
                    value: delta,
                })?;
            }
        }
        Ok(())
    }

    fn ftyp(&mut self) -> Result<(), MuxError> {
        let compatible: [&[u8; 4]; 3] = match self.layout {
            Layout::Progressive => [b"isom", b"iso4", b"mp41"],
            Layout::Fragmented => [b"isom", b"iso6", b"mp41"],
        };
        let mut w = Out(&mut self.out);
        let at = w.open(b"ftyp")?;
        w.put(b"isom")?;
        w.u32(0x200)?;
        for brand in compatible {
            w.put(brand)?;
        }
        w.close(at, *b"ftyp")
    }

    /// `moov`: a progressive one holds every sample's table, a fragmented one empty tables and
    /// `mvex`. Returns where `stco`'s one chunk offset is, for the caller to fill in.
    fn moov(&mut self) -> Result<Option<usize>, MuxError> {
        let progressive = self.layout == Layout::Progressive;
        // The presentation's length: known for a progressive file, open for a fragmented one.
        let duration = if progressive { u64::try_from(self.end).unwrap_or(0) } else { 0 };
        let wide = duration > u64::from(u32::MAX);
        let version = u8::from(wide);
        let (p0, d0) = match self.first {
            Some((d0, p0)) => (p0, d0),
            None => (0, 0),
        };
        let media_start = p0 - d0;
        let Muxer { samples, out, hvcc, colour, width, height, timescale, .. } = self;
        let mut w = Out(out);
        let moov = w.open(b"moov")?;

        let mvhd = w.full(b"mvhd", version, 0)?;
        w.times(wide, *timescale, duration)?;
        w.u32(0x0001_0000)?; // rate 1.0
        w.u16(0x0100)?; // volume 1.0
        w.zeros(2 + 8)?;
        for m in MATRIX {
            w.u32(m)?;
        }
        w.zeros(24)?;
        w.u32(TRACK_ID + 1)?; // next_track_ID
        w.close(mvhd, *b"mvhd")?;

        let trak = w.open(b"trak")?;
        let tkhd = w.full(b"tkhd", version, 0x3)?; // enabled, in the movie
        if wide {
            w.u64(0)?;
            w.u64(0)?;
        } else {
            w.u32(0)?;
            w.u32(0)?;
        }
        w.u32(TRACK_ID)?;
        w.u32(0)?;
        if wide {
            w.u64(duration)?;
        } else {
            w.u32(duration as u32)?;
        }
        w.zeros(8)?;
        w.u16(0)?; // layer
        w.u16(0)?; // alternate_group
        w.u16(0)?; // volume: video has none
        w.u16(0)?;
        for m in MATRIX {
            w.u32(m)?;
        }
        w.u32(u32::from(*width) << 16)?;
        w.u32(u32::from(*height) << 16)?;
        w.close(tkhd, *b"tkhd")?;

        // The edit list: the presentation starts at the first packet's presentation time.
        let edts = w.open(b"edts")?;
        let elst = w.full(b"elst", 1, 0)?;
        w.u32(1)?;
        w.u64(duration)?; // a fragmented file's 0: the edit runs to the end of the fragments
        w.u64(u64::try_from(media_start).unwrap_or(0))?;
        w.u16(1)?; // media_rate 1.0
        w.u16(0)?;
        w.close(elst, *b"elst")?;
        w.close(edts, *b"edts")?;

        let mdia = w.open(b"mdia")?;
        let mdhd = w.full(b"mdhd", version, 0)?;
        w.times(wide, *timescale, duration)?;
        w.u16(LANGUAGE_UND)?;
        w.u16(0)?;
        w.close(mdhd, *b"mdhd")?;
        let hdlr = w.full(b"hdlr", 0, 0)?;
        w.u32(0)?;
        w.put(b"vide")?;
        w.zeros(12)?;
        w.put(b"VideoHandler\0")?;
        w.close(hdlr, *b"hdlr")?;

        let minf = w.open(b"minf")?;
        let vmhd = w.full(b"vmhd", 0, 1)?;
        w.zeros(8)?;
        w.close(vmhd, *b"vmhd")?;
        let dinf = w.open(b"dinf")?;
        let dref = w.full(b"dref", 0, 0)?;
        w.u32(1)?;
        let url = w.full(b"url ", 0, 1)?; // the data is in this file
        w.close(url, *b"url ")?;
        w.close(dref, *b"dref")?;
        w.close(dinf, *b"dinf")?;

        let stbl = w.open(b"stbl")?;
        let stsd = w.full(b"stsd", 0, 0)?;
        w.u32(1)?;
        let hvc1 = w.open(b"hvc1")?;
        w.zeros(6)?;
        w.u16(1)?; // data_reference_index
        w.zeros(16)?;
        w.u16(*width)?;
        w.u16(*height)?;
        w.u32(0x0048_0000)?; // 72 dpi
        w.u32(0x0048_0000)?;
        w.u32(0)?;
        w.u16(1)?; // frame_count
        w.zeros(32)?; // compressorname: none
        w.u16(0x0018)?; // depth
        w.u16(0xFFFF)?; // pre_defined -1
        let hvcc_box = w.open(b"hvcC")?;
        w.put(&hvcc[..])?;
        w.close(hvcc_box, *b"hvcC")?;
        let colr = w.open(b"colr")?;
        w.put(b"nclx")?;
        w.u16(colour.primaries)?;
        w.u16(colour.transfer)?;
        w.u16(colour.matrix)?;
        w.u8(u8::from(colour.full_range) << 7)?;
        w.close(colr, *b"colr")?;
        if let Some(mastering) = colour.mastering {
            let mdcv = w.open(b"mdcv")?;
            w.put(&mastering.to_bytes())?;
            w.close(mdcv, *b"mdcv")?;
        }
        if let Some(light) = colour.light {
            let clli = w.open(b"clli")?;
            w.put(&light.to_bytes())?;
            w.close(clli, *b"clli")?;
        }
        w.close(hvc1, *b"hvc1")?;
        w.close(stsd, *b"stsd")?;

        let held: &[Sample] = if progressive { &samples[..] } else { &[] };
        // stts: runs of equal durations.
        let stts = w.full(b"stts", 0, 0)?;
        let count_at = w.here();
        w.u32(0)?;
        let mut runs = 0u32;
        let mut i = 0;
        while i < held.len() {
            let mut j = i + 1;
            while j < held.len() && held[j].duration == held[i].duration {
                j += 1;
            }
            w.u32((j - i) as u32)?;
            w.u32(held[i].duration)?;
            runs += 1;
            i = j;
        }
        w.patch(count_at, runs);
        w.close(stts, *b"stts")?;
        // ctts: runs of equal composition offsets, when any is not 0.
        if held.iter().any(|s| s.offset != 0) {
            let ctts = w.full(b"ctts", 0, 0)?;
            let count_at = w.here();
            w.u32(0)?;
            let mut runs = 0u32;
            let mut i = 0;
            while i < held.len() {
                let mut j = i + 1;
                while j < held.len() && held[j].offset == held[i].offset {
                    j += 1;
                }
                w.u32((j - i) as u32)?;
                w.u32(held[i].offset)?;
                runs += 1;
                i = j;
            }
            w.patch(count_at, runs);
            w.close(ctts, *b"ctts")?;
        }
        // stss: the key frames, when not every sample is one.
        if held.iter().any(|s| !s.key) {
            let stss = w.full(b"stss", 0, 0)?;
            w.u32(held.iter().filter(|s| s.key).count() as u32)?;
            for (i, _) in held.iter().enumerate().filter(|(_, s)| s.key) {
                w.u32(i as u32 + 1)?;
            }
            w.close(stss, *b"stss")?;
        }
        // stsc: every sample in one chunk.
        let stsc = w.full(b"stsc", 0, 0)?;
        if held.is_empty() {
            w.u32(0)?;
        } else {
            w.u32(1)?;
            w.u32(1)?;
            w.u32(held.len() as u32)?;
            w.u32(1)?;
        }
        w.close(stsc, *b"stsc")?;
        let stsz = w.full(b"stsz", 0, 0)?;
        w.u32(0)?;
        w.u32(held.len() as u32)?;
        for s in held {
            w.u32(s.size)?;
        }
        w.close(stsz, *b"stsz")?;
        let stco = w.full(b"stco", 0, 0)?;
        let chunk_at = if held.is_empty() {
            w.u32(0)?;
            None
        } else {
            w.u32(1)?;
            let at = w.here();
            w.u32(0)?;
            Some(at)
        };
        w.close(stco, *b"stco")?;
        w.close(stbl, *b"stbl")?;
        w.close(minf, *b"minf")?;
        w.close(mdia, *b"mdia")?;
        w.close(trak, *b"trak")?;

        if !progressive {
            let mvex = w.open(b"mvex")?;
            let trex = w.full(b"trex", 0, 0)?;
            w.u32(TRACK_ID)?;
            w.u32(1)?; // default_sample_description_index
            w.zeros(12)?; // default duration, size and flags: each sample says its own
            w.close(trex, *b"trex")?;
            w.close(mvex, *b"mvex")?;
        }
        w.close(moov, *b"moov")?;
        Ok(chunk_at)
    }

    /// The whole progressive file: `ftyp`, `moov`, `mdat`.
    fn progressive(&mut self) -> Result<(), MuxError> {
        self.durations(None)?;
        self.ftyp()?;
        let chunk_at = self.moov()?;
        let data_len = self.data.len() as u64;
        let large = data_len + 8 > u64::from(u32::MAX);
        let mut w = Out(&mut self.out);
        if large {
            w.u32(1)?;
            w.put(b"mdat")?;
            w.u64(data_len + 16)?;
        } else {
            w.u32((data_len + 8) as u32)?;
            w.put(b"mdat")?;
        }
        let data_at = self.taken + self.out.len() as u64;
        if let Some(at) = chunk_at {
            let offset = u32::try_from(data_at).map_err(|_| MuxError::TooLarge { kind: *b"stco", size: data_at })?;
            Out(&mut self.out).patch(at, offset);
        }
        Out(&mut self.out).put(&self.data)?;
        self.samples.clear();
        self.data.clear();
        Ok(())
    }

    /// One fragment of the held samples (the init segment first, before the first one); `next`
    /// is the decode time of the sample after them, if one is coming.
    fn fragment(&mut self, next: Option<i64>) -> Result<(), MuxError> {
        if self.sequence == 0 {
            self.ftyp()?;
            self.moov()?;
        }
        self.durations(next)?;
        self.sequence += 1;
        let base = self.samples.first().map_or(0, |s| s.dt);
        let large = self.data.len() as u64 + 8 > u64::from(u32::MAX);
        let Muxer { samples, data, out, sequence, .. } = self;
        let mut w = Out(out);
        let moof = w.open(b"moof")?;
        let mfhd = w.full(b"mfhd", 0, 0)?;
        w.u32(*sequence)?;
        w.close(mfhd, *b"mfhd")?;
        let traf = w.open(b"traf")?;
        let tfhd = w.full(b"tfhd", 0, TFHD_BASE_IS_MOOF)?;
        w.u32(TRACK_ID)?;
        w.close(tfhd, *b"tfhd")?;
        let tfdt = w.full(b"tfdt", 1, 0)?;
        w.u64(u64::try_from(base).unwrap_or(0))?;
        w.close(tfdt, *b"tfdt")?;
        let trun = w.full(b"trun", 0, TRUN_FIELDS)?;
        w.u32(samples.len() as u32)?;
        let offset_at = w.here();
        w.u32(0)?;
        for s in samples.iter() {
            w.u32(s.duration)?;
            w.u32(s.size)?;
            w.u32(if s.key { KEY_FLAGS } else { NON_KEY_FLAGS })?;
            w.u32(s.offset)?;
        }
        w.close(trun, *b"trun")?;
        w.close(traf, *b"traf")?;
        w.close(moof, *b"moof")?;
        let moof_len = w.here() - moof;
        let header = if large { 16 } else { 8 };
        let offset = u32::try_from(moof_len + header).map_err(|_| MuxError::TooLarge { kind: *b"moof", size: moof_len as u64 })?;
        w.patch(offset_at, offset);
        let data_len = data.len() as u64;
        if large {
            w.u32(1)?;
            w.put(b"mdat")?;
            w.u64(data_len + 16)?;
        } else {
            w.u32((data_len + 8) as u32)?;
            w.put(b"mdat")?;
        }
        w.put(&data[..])?;
        samples.clear();
        data.clear();
        Ok(())
    }
}

/// Big-endian writes onto a byte vector, each growing it through `try_reserve`.
struct Out<'v, A: Allocator>(&'v mut Vec<u8, A>);

impl<A: Allocator> Out<'_, A> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), MuxError> {
        self.0
            .try_reserve(bytes.len())
            .map_err(|_| MuxError::OutOfMemory { what: "the container's bytes", asked: bytes.len() })?;
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    fn u8(&mut self, v: u8) -> Result<(), MuxError> {
        self.put(&[v])
    }

    fn u16(&mut self, v: u16) -> Result<(), MuxError> {
        self.put(&v.to_be_bytes())
    }

    fn u32(&mut self, v: u32) -> Result<(), MuxError> {
        self.put(&v.to_be_bytes())
    }

    fn u64(&mut self, v: u64) -> Result<(), MuxError> {
        self.put(&v.to_be_bytes())
    }

    fn zeros(&mut self, n: usize) -> Result<(), MuxError> {
        const ZEROS: [u8; 32] = [0; 32];
        let mut left = n;
        while left > 0 {
            let take = left.min(ZEROS.len());
            self.put(&ZEROS[..take])?;
            left -= take;
        }
        Ok(())
    }

    /// Creation and modification time (0: times are data, never the wall clock), the timescale
    /// and the duration, 32 or 64 bits wide as the box's version says.
    fn times(&mut self, wide: bool, timescale: u32, duration: u64) -> Result<(), MuxError> {
        if wide {
            self.u64(0)?;
            self.u64(0)?;
            self.u32(timescale)?;
            self.u64(duration)
        } else {
            self.u32(0)?;
            self.u32(0)?;
            self.u32(timescale)?;
            self.u32(duration as u32)
        }
    }

    fn here(&self) -> usize {
        self.0.len()
    }

    /// A box of `kind` begins: its size is filled in by [`Out::close`].
    fn open(&mut self, kind: &[u8; 4]) -> Result<usize, MuxError> {
        let at = self.here();
        self.u32(0)?;
        self.put(kind)?;
        Ok(at)
    }

    /// A full box: a box with a version and 24 bits of flags.
    fn full(&mut self, kind: &[u8; 4], version: u8, flags: u32) -> Result<usize, MuxError> {
        let at = self.open(kind)?;
        self.u32((u32::from(version) << 24) | (flags & 0x00FF_FFFF))?;
        Ok(at)
    }

    fn close(&mut self, at: usize, kind: [u8; 4]) -> Result<(), MuxError> {
        let size = self.here() - at;
        let size = u32::try_from(size).map_err(|_| MuxError::TooLarge { kind, size: size as u64 })?;
        self.patch(at, size);
        Ok(())
    }

    fn patch(&mut self, at: usize, v: u32) {
        self.0[at..at + 4].copy_from_slice(&v.to_be_bytes());
    }
}

/// Why packets could not be muxed: each names the sample (from 0, in decode order), the field
/// or the box.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MuxError {
    /// The track's `field` `why`.
    Config { field: &'static str, why: &'static str },
    /// Sample `sample` is in ticks of 1/`got` s; the track's are 1/`track` s.
    Timescale { sample: u64, got: u32, track: u32 },
    /// Sample `sample` is out of order: `why`.
    Order { sample: u64, why: &'static str },
    /// Sample `sample`'s `what` is `value`, past what its field holds.
    Range { sample: u64, what: &'static str, value: i64 },
    /// Box `kind` would be `size` bytes, past what its size field holds.
    TooLarge { kind: [u8; 4], size: u64 },
    /// [`Muxer::finish`] was called with no packet pushed: an MP4 needs one.
    Empty,
    /// The allocator handed in could not give `asked` bytes for `what`.
    OutOfMemory { what: &'static str, asked: usize },
}

impl fmt::Display for MuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MuxError::Config { field, why } => write!(f, "mp4: the track's {field} {why}"),
            MuxError::Timescale { sample, got, track } => {
                write!(f, "mp4: sample {sample} is in ticks of 1/{got} s; the track's are 1/{track} s")
            }
            MuxError::Order { sample, why } => write!(f, "mp4: sample {sample} is out of order: {why}"),
            MuxError::Range { sample, what, value } => {
                write!(f, "mp4: sample {sample}'s {what} is {value}, past what its field holds")
            }
            MuxError::TooLarge { kind, size } => {
                write!(f, "mp4: box {} would be {size} bytes, past what its size field holds", kind.escape_ascii())
            }
            MuxError::Empty => write!(f, "mp4: finished with no packet; an MP4 needs one"),
            MuxError::OutOfMemory { what, asked } => {
                write!(f, "mp4: the allocator could not give {asked} bytes for {what}")
            }
        }
    }
}

impl core::error::Error for MuxError {}
