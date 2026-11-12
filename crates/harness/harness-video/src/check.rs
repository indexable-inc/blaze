//! Reading a finished mp4 back, with no decoder and no outside tool: the boxes say what the file is, and [`verify`] holds
//! them to the preset it was made with. What is checked: the `moov` comes before the `mdat`; one video track of the
//! stated codec, profile and size; exactly `seconds x fps` samples, every one the same length, whose presentation times
//! (decode time plus composition offset) are each frame's own with none missing or twice; a key frame at least every
//! `keyframe_seconds`; the colour tag; and the audio track's codec, rate, channels and length against the video's.
use crate::preset::{Audio, Codec, Colour, Export, Profile};

/// One track as its boxes describe it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Track {
    /// `vide` or `soun`.
    pub handler: String,
    /// The sample entry's type: `avc1`, `hvc1`, `mp4a`, `alac`.
    pub entry: String,
    pub timescale: u32,
    /// `mdhd`'s duration, in `timescale`.
    pub duration: u64,
    pub samples: u64,
    /// `stts`: runs of (count, delta).
    pub deltas: Vec<(u32, u32)>,
    /// `ctts`: runs of (count, offset); empty when every frame is shown in decode order.
    pub offsets: Vec<(u32, i64)>,
    /// `stss`: the key frames' sample numbers from 1; empty when every sample is one.
    pub keys: Vec<u32>,
    /// Video: pixels.
    pub width: u16,
    pub height: u16,
    /// Video: `avcC`'s or `hvcC`'s profile (H.264: 100 is High; HEVC: 1 Main, 2 Main 10).
    pub profile: u8,
    /// Video: `colr` `nclx`'s primaries, transfer, matrix and full-range flag.
    pub colour: Option<(u16, u16, u16, bool)>,
    /// Audio: channels and sample rate from the sample entry.
    pub channels: u16,
    pub rate: u32,
    /// Audio: `esds`'s object type (0x40 is MPEG-4 audio) and the audio object type of its config (2 is AAC-LC).
    pub object: (u8, u8),
    /// `elst`: the first edit's media time (what is skipped at the front, in `timescale`) and its length in the movie's
    /// timescale; None with no edit list.
    pub edit: Option<(i64, u64)>,
    /// The movie's timescale (`mvhd`'s), which an edit's length is in.
    pub movie_scale: u32,
}

/// A file as its boxes describe it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub bytes: u64,
    pub moov_first: bool,
    /// `mvhd`'s timescale.
    pub timescale: u32,
    pub tracks: Vec<Track>,
}

/// The boxes of `data`, in order: each its four-letter type and its payload.
fn boxes(data: &[u8]) -> Result<Vec<([u8; 4], &[u8])>, String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < data.len() {
        let head = data.get(at..at + 8).ok_or("a box header runs past the end")?;
        let kind = [head[4], head[5], head[6], head[7]];
        let (size, skip) = match u32::from_be_bytes([head[0], head[1], head[2], head[3]]) {
            0 => (data.len() - at, 8),
            1 => {
                let big = data.get(at + 8..at + 16).ok_or("a 64-bit box size runs past the end")?;
                (u64::from_be_bytes(big.try_into().map_err(|_| "a 64-bit box size")?) as usize, 16)
            }
            n => (n as usize, 8),
        };
        if size < skip || at + size > data.len() {
            return Err(format!("box {} at byte {at} says {size} bytes, past the end", String::from_utf8_lossy(&kind)));
        }
        out.push((kind, &data[at + skip..at + size]));
        at += size;
    }
    Ok(out)
}

fn find<'a>(data: &'a [u8], kind: &[u8; 4]) -> Result<Option<&'a [u8]>, String> {
    Ok(boxes(data)?.into_iter().find(|(k, _)| k == kind).map(|(_, d)| d))
}

fn need<'a>(data: &'a [u8], kind: &[u8; 4], inside: &str) -> Result<&'a [u8], String> {
    find(data, kind)?.ok_or_else(|| format!("no {} in {inside}", String::from_utf8_lossy(kind)))
}

/// Big-endian fields read in order; running out is an error naming the box.
struct Fields<'a>(&'a [u8], &'static str);

impl Fields<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        if self.0.len() < n {
            return Err(format!("{} is cut short", self.1));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from(self.u32()?) << 32 | u64::from(self.u32()?))
    }
}

/// An `esds` descriptor's length: seven bits a byte, the top bit saying another follows.
fn descriptor_len(f: &mut Fields<'_>) -> Result<usize, String> {
    let mut n = 0usize;
    for _ in 0..4 {
        let b = f.u8()?;
        n = n << 7 | usize::from(b & 0x7f);
        if b & 0x80 == 0 {
            break;
        }
    }
    Ok(n)
}

/// `esds`: the object type and the audio object type of the decoder's config.
fn esds(data: &[u8]) -> Result<(u8, u8), String> {
    let mut f = Fields(data, "esds");
    f.take(4)?;
    if f.u8()? != 3 {
        return Err("esds does not start with an ES descriptor".into());
    }
    descriptor_len(&mut f)?;
    f.take(2)?;
    let flags = f.u8()?;
    if flags & 0x80 != 0 {
        f.take(2)?;
    }
    if flags & 0x40 != 0 {
        let n = usize::from(f.u8()?);
        f.take(n)?;
    }
    if flags & 0x20 != 0 {
        f.take(2)?;
    }
    if f.u8()? != 4 {
        return Err("esds has no decoder config descriptor".into());
    }
    descriptor_len(&mut f)?;
    let object = f.u8()?;
    f.take(12)?;
    if f.u8()? != 5 {
        return Err("esds has no decoder specific info".into());
    }
    descriptor_len(&mut f)?;
    Ok((object, f.u8()? >> 3))
}

fn track(trak: &[u8], movie_scale: u32) -> Result<Track, String> {
    let mut t = Track { movie_scale, ..Track::default() };
    if let Some(edts) = find(trak, b"edts")? {
        let mut f = Fields(need(edts, b"elst", "edts")?, "elst");
        let version = f.u8()?;
        f.take(3)?;
        if f.u32()? > 0 {
            t.edit = Some(if version == 1 {
                let len = f.u64()?;
                (f.u64()? as i64, len)
            } else {
                let len = u64::from(f.u32()?);
                (i64::from(f.u32()? as i32), len)
            });
        }
    }
    let mdia = need(trak, b"mdia", "trak")?;
    let mut f = Fields(need(mdia, b"mdhd", "mdia")?, "mdhd");
    let version = f.u8()?;
    f.take(3)?;
    if version == 1 {
        f.take(16)?;
        t.timescale = f.u32()?;
        t.duration = f.u64()?;
    } else {
        f.take(8)?;
        t.timescale = f.u32()?;
        t.duration = u64::from(f.u32()?);
    }
    let hdlr = need(mdia, b"hdlr", "mdia")?;
    t.handler = String::from_utf8_lossy(hdlr.get(8..12).ok_or("hdlr is cut short")?).into_owned();
    let stbl = need(need(mdia, b"minf", "mdia")?, b"stbl", "minf")?;
    let stsd = need(stbl, b"stsd", "stbl")?;
    let entries = boxes(stsd.get(8..).ok_or("stsd is cut short")?)?;
    let (kind, entry) = entries.first().ok_or("stsd has no sample entry")?;
    t.entry = String::from_utf8_lossy(kind).into_owned();
    match t.handler.as_str() {
        "vide" => {
            let mut f = Fields(entry, "the video sample entry");
            f.take(24)?;
            t.width = f.u16()?;
            t.height = f.u16()?;
            let inner = entry.get(78..).ok_or("the video sample entry is cut short")?;
            if let Some(avcc) = find(inner, b"avcC")? {
                t.profile = *avcc.get(1).ok_or("avcC is cut short")?;
            }
            if let Some(hvcc) = find(inner, b"hvcC")? {
                t.profile = hvcc.get(1).ok_or("hvcC is cut short")? & 0x1f;
            }
            if let Some(colr) = find(inner, b"colr")? {
                if colr.get(..4) == Some(b"nclx") {
                    let mut f = Fields(&colr[4..], "colr");
                    t.colour = Some((f.u16()?, f.u16()?, f.u16()?, f.u8()? & 0x80 != 0));
                }
            }
        }
        "soun" => {
            let mut f = Fields(entry, "the audio sample entry");
            f.take(8)?;
            let version = f.u16()?;
            f.take(6)?;
            t.channels = f.u16()?;
            f.take(6)?;
            t.rate = u32::from(f.u16()?);
            f.take(2)?;
            let inner = entry.get(if version == 1 { 44 } else { 28 }..).ok_or("the audio sample entry is cut short")?;
            if let Some(e) = find(inner, b"esds")? {
                t.object = esds(e)?;
            }
        }
        _ => {}
    }
    let mut f = Fields(need(stbl, b"stts", "stbl")?, "stts");
    f.take(4)?;
    for _ in 0..f.u32()? {
        let run = (f.u32()?, f.u32()?);
        t.samples += u64::from(run.0);
        t.deltas.push(run);
    }
    if let Some(ctts) = find(stbl, b"ctts")? {
        let mut f = Fields(ctts, "ctts");
        f.take(4)?;
        for _ in 0..f.u32()? {
            // Version 1's offsets are signed; version 0's, read as signed, are the same numbers for any real offset.
            t.offsets.push((f.u32()?, i64::from(f.u32()? as i32)));
        }
    }
    if let Some(stss) = find(stbl, b"stss")? {
        let mut f = Fields(stss, "stss");
        f.take(4)?;
        for _ in 0..f.u32()? {
            t.keys.push(f.u32()?);
        }
    }
    Ok(t)
}

/// What `data` (a whole mp4) is.
pub fn read(data: &[u8]) -> Result<Report, String> {
    let top = boxes(data)?;
    let place = |kind: &[u8; 4]| top.iter().position(|(k, _)| k == kind);
    let (moov_at, mdat_at) = (place(b"moov").ok_or("no moov")?, place(b"mdat").ok_or("no mdat")?);
    let moov = top[moov_at].1;
    let mut f = Fields(need(moov, b"mvhd", "moov")?, "mvhd");
    let version = f.u8()?;
    f.take(if version == 1 { 19 } else { 11 })?;
    let timescale = f.u32()?;
    let mut tracks = Vec::new();
    for (kind, trak) in boxes(moov)? {
        if &kind == b"trak" {
            tracks.push(track(trak, timescale)?);
        }
    }
    Ok(Report { bytes: data.len() as u64, moov_first: moov_at < mdat_at, timescale, tracks })
}

impl Track {
    /// Seconds shown: the edit's length when there is one (an AAC track's edit leaves out the encoder's priming at the
    /// front and its padding at the end), else the samples' lengths.
    pub fn seconds(&self) -> f64 {
        if let Some((_, len)) = self.edit {
            return len as f64 / f64::from(self.movie_scale.max(1));
        }
        let ticks: u64 = self.deltas.iter().map(|(n, d)| u64::from(*n) * u64::from(*d)).sum();
        let skipped = self.edit.map_or(0, |(start, _)| start.max(0)) as u64;
        ticks.saturating_sub(skipped) as f64 / f64::from(self.timescale.max(1))
    }
}

impl Report {
    pub fn video(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.handler == "vide")
    }

    pub fn audio(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.handler == "soun")
    }

    /// A few lines for a person.
    pub fn words(&self) -> String {
        let mut out = format!("{} bytes, moov {} the mdat\n", self.bytes, if self.moov_first { "before" } else { "AFTER" });
        for t in &self.tracks {
            match t.handler.as_str() {
                "vide" => out.push_str(&format!(
                    "video: {} profile {} {}x{}, {} frames, {:.3} s, frame lengths {:?} of 1/{} s, {} key frames, colour {:?}\n",
                    t.entry,
                    t.profile,
                    t.width,
                    t.height,
                    t.samples,
                    t.seconds(),
                    t.deltas,
                    t.timescale,
                    if t.keys.is_empty() { t.samples as usize } else { t.keys.len() },
                    t.colour
                )),
                "soun" => out.push_str(&format!(
                    "audio: {} object {:#x} type {} , {} channels at {} Hz, {} packets, {:.3} s (edit {:?})\n",
                    t.entry,
                    t.object.0,
                    t.object.1,
                    t.channels,
                    t.rate,
                    t.samples,
                    t.seconds(),
                    t.edit
                )),
                other => out.push_str(&format!("track {other}: {}\n", t.entry)),
            }
        }
        out
    }
}

/// Hold `r` to `export` for a video of `frames` frames at `fps` and `(w, h)` pixels. Err lists every difference.
pub fn verify(r: &Report, export: &Export, (w, h): (u32, u32), fps: u32, frames: u64) -> Result<(), String> {
    let mut bad = Vec::new();
    if !r.moov_first {
        bad.push("the moov is after the mdat (not fast-start)".to_owned());
    }
    match r.video() {
        None => bad.push("no video track".to_owned()),
        Some(v) => {
            let (entry, profile) = match (export.codec, export.profile) {
                (Codec::H264, _) => ("avc1", 100),
                (Codec::Hevc, Profile::Main10) => ("hvc1", 2),
                (Codec::Hevc, _) => ("hvc1", 1),
            };
            if v.entry != entry || v.profile != profile {
                bad.push(format!("video is {} profile {}, the preset says {entry} profile {profile}", v.entry, v.profile));
            }
            if (u32::from(v.width), u32::from(v.height)) != (w, h) {
                bad.push(format!("video is {}x{}, not {w}x{h}", v.width, v.height));
            }
            if v.samples != frames {
                bad.push(format!("{} frames, not {frames}", v.samples));
            }
            match v.deltas.as_slice() {
                [(_, delta)] if u64::from(*delta) * u64::from(fps) == u64::from(v.timescale) => {
                    // Presentation times: each frame's own slot, once.
                    let delta = i64::from(*delta);
                    let mut shown = Vec::with_capacity(v.samples as usize);
                    let mut offsets = v.offsets.iter().flat_map(|(n, o)| std::iter::repeat_n(*o, *n as usize));
                    for i in 0..v.samples as i64 {
                        shown.push(i * delta + offsets.next().unwrap_or(0));
                    }
                    shown.sort_unstable();
                    let first = shown.first().copied().unwrap_or(0);
                    if let Some(i) = shown.iter().enumerate().position(|(i, t)| *t != first + i as i64 * delta) {
                        bad.push(format!("presentation times are not one a frame: the {i}th shown is at {} of 1/{} s", shown[i], v.timescale));
                    }
                    if v.edit.is_some_and(|(start, _)| start != first) {
                        bad.push(format!("the video's edit starts at {:?}, its first frame is shown at {first}", v.edit));
                    }
                }
                other => bad.push(format!("frame lengths {other:?} of 1/{} s are not one constant 1/{fps} s", v.timescale)),
            }
            let limit = (export.keyframe_seconds * f64::from(fps)).round() as u32;
            let mut last = 1;
            for k in v.keys.iter().copied().chain([v.samples as u32 + 1]) {
                if k - last > limit {
                    bad.push(format!("{} frames from key frame {last} to the next, over {limit}", k - last));
                    break;
                }
                last = k;
            }
            if v.keys.first().is_some_and(|k| *k != 1) {
                bad.push("the first frame is not a key frame".to_owned());
            }
            let want = match export.colour {
                Colour::Bt709 => (1, 1, 1, false),
                Colour::Hdr10 => (9, 16, 9, false),
            };
            if v.colour != Some(want) {
                bad.push(format!("colour tag {:?}, the preset says {want:?} (primaries, transfer, matrix, full range)", v.colour));
            }
        }
    }
    match (export.audio, r.audio()) {
        (Audio::None, None) | (Audio::Alac, None) => {}
        (Audio::None, Some(a)) => bad.push(format!("an audio track ({}) where the preset says none", a.entry)),
        (Audio::AacLc, None) => bad.push("no audio track; the preset says aac-lc".to_owned()),
        (codec, Some(a)) => {
            let ok = match codec {
                Audio::AacLc => a.entry == "mp4a" && a.object == (0x40, 2),
                _ => a.entry == "alac",
            };
            if !ok {
                bad.push(format!("audio is {} (object {:#x}, type {}), the preset says {codec:?}", a.entry, a.object.0, a.object.1));
            }
            if (a.channels, a.rate) != (2, 48_000) {
                bad.push(format!("audio is {} channels at {} Hz, not 2 at 48000", a.channels, a.rate));
            }
            // Within one AAC frame (1024 samples) of the video's length.
            let video = frames as f64 / f64::from(fps);
            if (a.seconds() - video).abs() > 1024.0 / 48_000.0 {
                bad.push(format!("audio shows {:.4} s, the video {video:.4} s: more than one audio frame apart", a.seconds()));
            }
        }
    }
    if bad.is_empty() { Ok(()) } else { Err(bad.join("\n")) }
}
