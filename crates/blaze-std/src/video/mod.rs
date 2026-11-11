//! Video as sans-IO cores (docs/v2/plan/design/std.md, "Gpu, video and images"):
//! an [`Encoder`] takes [`Frame`]s and gives [`Packet`]s, and a [`Muxer`] takes packets and
//! gives the bytes of a container. Neither takes a capability or reads a clock: a frame's time
//! is data, in ticks of a timescale the caller picks. Writing the bytes is the caller's: they go
//! to a `File` it holds.
//!
//! **One concrete type, its provider inside.** [`Encoder`] is a struct whose provider is a
//! private field, chosen where it is made:
//!
//! | provider | made by | where |
//! |---|---|---|
//! | VideoToolbox's hardware HEVC encoder | [`Encoder::hardware`] from a [`Gpu`] | macOS |
//!
//! There is no software HEVC encoder yet. Where no provider exists, [`Encoder::hardware`]
//! returns [`VideoError::NoProvider`] naming what is missing; nothing falls back to another
//! encoder. The muxer ([`mp4`]) is written here and runs everywhere.
//!
//! Frames are bytes in the caller's allocator (two planes, 4:2:0, biplanar); the
//! VideoToolbox provider copies each into a pixel buffer from its own pool. A zero-copy path
//! (the GPU writing straight into the encoder's buffers, as harness-video does) needs `Gpu` to
//! own render targets, which it does not yet.

pub mod mp4;
#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "VideoToolbox, CoreVideo and CoreMedia are C APIs: every call, each with its SAFETY"
)]
mod videotoolbox;

use alloc::vec::Vec;
use core::{alloc::Allocator, fmt};

pub use mp4::{Layout, MuxError, Muxer, Track};

use crate::gpu::Gpu;

/// How a frame's pixels are laid out: 4:2:0, a luma plane and an interleaved CbCr plane at
/// half width and half height, limited range.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PixelFormat {
    /// 8-bit samples (`NV12`, CoreVideo's `420v`).
    Nv12,
    /// 10-bit samples in the high bits of little-endian 16-bit words (`P010`, CoreVideo's
    /// `x420`).
    P010,
}

impl PixelFormat {
    /// Bytes a sample.
    pub const fn sample_bytes(self) -> usize {
        match self {
            PixelFormat::Nv12 => 1,
            PixelFormat::P010 => 2,
        }
    }
}

/// One plane: rows of `stride` bytes, the last one at least as long as a row's samples.
pub struct Plane<A: Allocator> {
    pub stride: usize,
    pub bytes: Vec<u8, A>,
}

/// One picture, and when it is shown: `time` and `duration` in ticks of `timescale` a second.
pub struct Frame<A: Allocator> {
    format: PixelFormat,
    width: u32,
    height: u32,
    /// Luma, then CbCr.
    planes: [Plane<A>; 2],
    time: i64,
    duration: u32,
    timescale: u32,
}

impl<A: Allocator> Frame<A> {
    /// A frame of `planes` (luma, then CbCr), checked against its format and size: the width
    /// and height are even and at least 2, each plane's stride holds a row and its bytes hold
    /// every row.
    pub fn new(
        format: PixelFormat,
        (width, height): (u32, u32),
        planes: [Plane<A>; 2],
        (time, duration, timescale): (i64, u32, u32),
    ) -> Result<Frame<A>, VideoError> {
        if width < 2 || width % 2 != 0 {
            return Err(VideoError::Frame { time, why: "its width is not even and at least 2" });
        }
        if height < 2 || height % 2 != 0 {
            return Err(VideoError::Frame { time, why: "its height is not even and at least 2" });
        }
        if timescale == 0 {
            return Err(VideoError::Frame { time, why: "its timescale is 0" });
        }
        // Both planes' rows are `width` samples wide: luma one a pixel, CbCr two a pair of pixels.
        let row = width as usize * format.sample_bytes();
        let rows = [height as usize, height as usize / 2];
        for (plane, rows) in planes.iter().zip(rows) {
            if plane.stride < row {
                return Err(VideoError::Frame { time, why: "a plane's stride is shorter than its row" });
            }
            let need = plane.stride * (rows - 1) + row;
            if plane.bytes.len() < need {
                return Err(VideoError::Frame { time, why: "a plane's bytes are fewer than its rows" });
            }
        }
        Ok(Frame { format, width, height, planes, time, duration, timescale })
    }

    pub fn format(&self) -> PixelFormat {
        self.format
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Luma, then CbCr.
    pub fn planes(&self) -> &[Plane<A>; 2] {
        &self.planes
    }

    /// When it is shown, in ticks of [`Frame::timescale`].
    pub fn time(&self) -> i64 {
        self.time
    }

    pub fn duration(&self) -> u32 {
        self.duration
    }

    pub fn timescale(&self) -> u32 {
        self.timescale
    }
}

/// One compressed picture: HEVC NAL units each behind a 4-byte big-endian length (the form an
/// MP4 sample holds, `hvcC`'s length size 4), with its decode and presentation times and
/// duration in ticks of `timescale`, and whether a decoder can start at it.
pub struct Packet<A: Allocator> {
    pub data: Vec<u8, A>,
    pub pts: i64,
    pub dts: i64,
    pub duration: u32,
    pub timescale: u32,
    pub key: bool,
}

impl<A: Allocator> fmt::Debug for Packet<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Packet({} bytes, pts {}, dts {}, duration {} of 1/{} s{})",
            self.data.len(),
            self.pts,
            self.dts,
            self.duration,
            self.timescale,
            if self.key { ", key" } else { "" }
        )
    }
}

/// The mastering display's colour volume (SMPTE ST 2086): its primaries in G, B, R order (as
/// HEVC's SEI and the `mdcv` box store them) and its white point, in units of 0.00002, and its
/// luminance range in units of 0.0001 nits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Mastering {
    pub primaries: [(u16, u16); 3],
    pub white: (u16, u16),
    pub max_luminance: u32,
    pub min_luminance: u32,
}

impl Mastering {
    /// Display P3 primaries, the D65 white point, 1000 nits peak and 0.0001 nits black: the
    /// usual HDR10 mastering display.
    pub const P3_D65_1000: Mastering = Mastering {
        primaries: [(13250, 34500), (7500, 3000), (34000, 16000)],
        white: (15635, 16450),
        max_luminance: 1000 * 10_000,
        min_luminance: 1,
    };

    /// The 24 bytes, big-endian, that the `mdcv` box and VideoToolbox's
    /// `MasteringDisplayColorVolume` both hold.
    pub fn to_bytes(&self) -> [u8; 24] {
        let mut out = [0u8; 24];
        for (i, (x, y)) in self.primaries.iter().enumerate() {
            out[i * 4..i * 4 + 2].copy_from_slice(&x.to_be_bytes());
            out[i * 4 + 2..i * 4 + 4].copy_from_slice(&y.to_be_bytes());
        }
        out[12..14].copy_from_slice(&self.white.0.to_be_bytes());
        out[14..16].copy_from_slice(&self.white.1.to_be_bytes());
        out[16..20].copy_from_slice(&self.max_luminance.to_be_bytes());
        out[20..24].copy_from_slice(&self.min_luminance.to_be_bytes());
        out
    }
}

/// Content light levels (CTA-861.3), nits: the brightest pixel and the brightest frame average.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ContentLight {
    pub max_cll: u16,
    pub max_fall: u16,
}

impl ContentLight {
    /// The 4 bytes, big-endian, that the `clli` box and VideoToolbox's
    /// `ContentLightLevelInfo` both hold.
    pub fn to_bytes(&self) -> [u8; 4] {
        let [a, b] = self.max_cll.to_be_bytes();
        let [c, d] = self.max_fall.to_be_bytes();
        [a, b, c, d]
    }
}

/// How a stream's colour is to be read: ITU-T H.273's code points (as `colr`'s `nclx` holds
/// them), and HDR10's metadata when there is any.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Colour {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    pub full_range: bool,
    pub mastering: Option<Mastering>,
    pub light: Option<ContentLight>,
}

impl Colour {
    /// HDR10: BT.2020 primaries and matrix (9, 9), the PQ transfer (16), limited range.
    pub const fn hdr10(mastering: Mastering, light: Option<ContentLight>) -> Colour {
        Colour { primaries: 9, transfer: 16, matrix: 9, full_range: false, mastering: Some(mastering), light }
    }

    /// BT.709 SDR: primaries, transfer and matrix 1, limited range.
    pub const BT709: Colour =
        Colour { primaries: 1, transfer: 1, matrix: 1, full_range: false, mastering: None, light: None };
}

/// What an [`Encoder`] makes: HEVC (Main for [`PixelFormat::Nv12`], Main 10 for
/// [`PixelFormat::P010`]) of `width` x `height` frames, at constant `quality` (0 to 1), a key
/// frame at least every `keyframe_every` frames, at about `fps` frames a second.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub timescale: u32,
    pub fps: f64,
    pub quality: f64,
    pub keyframe_every: u32,
    pub colour: Colour,
}

impl EncoderConfig {
    fn check(&self) -> Result<(), VideoError> {
        let refuse = |field, why| Err(VideoError::Config { field, why });
        if self.width < 2 || self.width % 2 != 0 || self.width > i32::MAX as u32 {
            return refuse("width", "is not even and in 2..=2^31 - 1");
        }
        if self.height < 2 || self.height % 2 != 0 || self.height > i32::MAX as u32 {
            return refuse("height", "is not even and in 2..=2^31 - 1");
        }
        if self.timescale == 0 || self.timescale > i32::MAX as u32 {
            return refuse("timescale", "is not in 1..=2^31 - 1");
        }
        if !(self.fps.is_finite() && self.fps > 0.0) {
            return refuse("fps", "is not a positive number");
        }
        if !(0.0..=1.0).contains(&self.quality) {
            return refuse("quality", "is not in 0..=1");
        }
        if self.keyframe_every == 0 || self.keyframe_every > i32::MAX as u32 {
            return refuse("keyframe_every", "is not in 1..=2^31 - 1");
        }
        Ok(())
    }
}

/// The provider inside an [`Encoder`]: a target with none has no variant, and no `Encoder` can
/// be made there.
enum Provider {
    #[cfg(target_os = "macos")]
    VideoToolbox(videotoolbox::Session),
}

/// Frames in, packets out: one concrete type whatever does the work (module docs).
pub struct Encoder {
    provider: Provider,
}

#[cfg(target_os = "macos")]
fn hardware(gpu: &Gpu, config: &EncoderConfig) -> Result<Provider, VideoError> {
    Ok(Provider::VideoToolbox(videotoolbox::Session::new(gpu, config)?))
}

/// The system this is compiled for, as a refusal names it.
#[cfg(not(target_os = "macos"))]
const TARGET_OS: &str = if cfg!(target_os = "linux") {
    "linux"
} else if cfg!(target_os = "windows") {
    "windows"
} else if cfg!(target_os = "ios") {
    "ios"
} else {
    "this system"
};

#[cfg(not(target_os = "macos"))]
fn hardware(_gpu: &Gpu, _config: &EncoderConfig) -> Result<Provider, VideoError> {
    Err(VideoError::NoProvider { what: "a hardware HEVC encoder", target: TARGET_OS })
}

impl Encoder {
    /// The platform's hardware HEVC encoder on `gpu`'s machine: VideoToolbox on macOS (the media
    /// engine; a software encoder is never taken in its place). Elsewhere
    /// [`VideoError::NoProvider`].
    pub fn hardware(gpu: &Gpu, config: &EncoderConfig) -> Result<Encoder, VideoError> {
        config.check()?;
        Ok(Encoder { provider: hardware(gpu, config)? })
    }

    /// Hand `frame` to the encoder. Frames go in presentation order with rising times, each the
    /// size and format the encoder was made for. At most a bounded number are held inside: past
    /// it, this waits for the encoder to finish the oldest (the provider's own completion, which
    /// reads no clock).
    #[cfg_attr(not(target_os = "macos"), expect(unused_variables, reason = "no provider here: no Encoder exists"))]
    pub fn send<A: Allocator>(&mut self, frame: &Frame<A>) -> Result<(), VideoError> {
        match self.provider {
            #[cfg(target_os = "macos")]
            Provider::VideoToolbox(ref mut session) => session.send(frame),
        }
    }

    /// The next packet the encoder has finished, in decode order, made in `alloc`; `None` when
    /// none is ready yet. Never waits.
    #[cfg_attr(not(target_os = "macos"), expect(unused_variables, reason = "no provider here: no Encoder exists"))]
    pub fn receive<A: Allocator>(&mut self, alloc: A) -> Result<Option<Packet<A>>, VideoError> {
        match self.provider {
            #[cfg(target_os = "macos")]
            Provider::VideoToolbox(ref mut session) => session.receive(alloc),
        }
    }

    /// Every frame sent is finished: after this, [`Encoder::receive`] gives the rest of the
    /// packets and then `None`.
    pub fn finish(&mut self) -> Result<(), VideoError> {
        match self.provider {
            #[cfg(target_os = "macos")]
            Provider::VideoToolbox(ref mut session) => session.finish(),
        }
    }

    /// The stream's decoder configuration, an `HEVCDecoderConfigurationRecord` (`hvcC`'s
    /// payload, which a [`Track`] takes): known once the first packet has been received.
    pub fn hvcc(&self) -> Option<&[u8]> {
        match self.provider {
            #[cfg(target_os = "macos")]
            Provider::VideoToolbox(ref session) => session.hvcc(),
        }
    }
}

/// Why a frame could not be encoded: each names the call, the field or the frame.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum VideoError {
    /// No provider of `what` exists on `target` (rule no-fallback: none is substituted).
    NoProvider { what: &'static str, target: &'static str },
    /// The encoder's config: `field` `why`.
    Config { field: &'static str, why: &'static str },
    /// The frame at `time`: `why`.
    Frame { time: i64, why: &'static str },
    /// The platform's `call` failed with `status` (an `OSStatus` or `CVReturn`).
    Platform { call: &'static str, status: i32 },
    /// The encoder dropped the frame at `time`.
    Dropped { time: i64 },
    /// The platform gave no `what` where it must.
    Missing { what: &'static str },
    /// The allocator handed in could not give `asked` bytes for `what`.
    OutOfMemory { what: &'static str, asked: usize },
}

impl fmt::Display for VideoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoError::NoProvider { what, target } => {
                write!(f, "video: no provider of {what} on {target}; none is substituted (rule no-fallback)")
            }
            VideoError::Config { field, why } => write!(f, "video: the encoder's {field} {why}"),
            VideoError::Frame { time, why } => write!(f, "video: the frame at {time}: {why}"),
            VideoError::Platform { call, status } => write!(f, "video: {call} failed with status {status}"),
            VideoError::Dropped { time } => write!(f, "video: the encoder dropped the frame at {time}"),
            VideoError::Missing { what } => write!(f, "video: the platform gave no {what}"),
            VideoError::OutOfMemory { what, asked } => {
                write!(f, "video: the allocator could not give {asked} bytes for {what}")
            }
        }
    }
}

impl core::error::Error for VideoError {}
