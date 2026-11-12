//! The mp4, natively and with no copy of a frame: each frame is a pixel buffer from the VideoToolbox session's own pool
//! (4:2:0 biplanar, video range, IOSurface-backed: `420v` 8-bit or `x420` 10-bit, as the preset's pixel format says), whose
//! two planes CoreVideo maps into Metal so the colour pass (`yuv.rs`) writes them directly; VideoToolbox encodes it on the
//! media engine (hardware required: no software encoder is ever taken) as the preset's codec (H.264 High, HEVC Main, HEVC
//! Main 10), and AVAssetWriter writes the compressed samples into the mp4 as they are (passthrough), moov first, with the
//! soundtrack beside them as the preset's audio: AAC-LC (AudioToolbox's encoder, inside the writer) or Apple Lossless.
//!
//! The stream is tagged with the preset's colour on every buffer and on the session: BT.709 (SDR), or HDR10 (BT.2020
//! primaries and matrix, the PQ transfer, the mastering display P3-D65 at 1000 nits as SEI); chroma sited left. The
//! encoder calls back on its own threads; each compressed sample is sent to the caller's thread, which appends it to the
//! writer, interleaved with the audio by time.
use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use block2::RcBlock;
use harness_native_render::gpu::{Dev, Tex};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterStatus, AVFileTypeMPEG4, AVMediaTypeAudio, AVMediaTypeVideo,
};
use objc2_avf_audio::{AVEncoderBitDepthHintKey, AVEncoderBitRateKey, AVFormatIDKey, AVNumberOfChannelsKey, AVSampleRateKey};
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
    kAudioFormatAppleLossless, kAudioFormatMPEG4AAC,
};
use objc2_core_foundation::{CFBoolean, CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMAudioFormatDescriptionCreate, CMAudioSampleBufferCreateReadyWithPacketDescriptions, CMBlockBuffer,
    CMFormatDescription, CMSampleBuffer, CMTime, kCMBlockBufferAssureMemoryNowFlag, kCMTimeInvalid,
    kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVAttachmentMode, CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture, CVPixelBuffer, CVPixelBufferPool,
    kCVImageBufferChromaLocationBottomFieldKey, kCVImageBufferChromaLocationTopFieldKey, kCVImageBufferChromaLocation_Left,
    kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_2020, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_2020,
    kCVMetalTextureUsage, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
    kCVReturnSuccess,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use objc2_metal::{MTLPixelFormat, MTLTextureUsage};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AllowOpenGOP, kVTCompressionPropertyKey_AverageBitRate, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_HDRMetadataInsertionMode, kVTCompressionPropertyKey_MasteringDisplayColorVolume,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_Quality, kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_SpatialAdaptiveQPLevel,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix, kVTHDRMetadataInsertionMode_Auto,
    kVTProfileLevel_HEVC_Main10_AutoLevel, kVTQPModulationLevel_Default,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use crate::preset::{Audio as AudioCodec, Codec, Colour, Export, Pixel, Profile};
use crate::yuv::PEAK_NITS;

/// An ALAC soundtrack's bits a sample (the YouTube master's: YouTube re-encodes whatever it is given, so that master's
/// audio is lossless, not a lossy AAC, whose Apple encoder also stops at 320 kb/s for 48 kHz stereo).
const AUDIO_BITS: i32 = 24;
/// Audio frames a sample buffer of the soundtrack holds.
const AUDIO_CHUNK: usize = 4096;
/// How far (seconds) the audio is fed ahead of the video appended so far: AVAssetWriter interleaves the two and holds back
/// whichever input runs too far ahead.
const AUDIO_LEAD: f64 = 1.0;
/// Frames handed to the encoder and not yet back: past this, the caller waits for one (each holds a pool buffer, 100 MB
/// at 8K).
const IN_FLIGHT: u32 = 32;
/// The longest any one wait (the encoder, the writer's readiness, the finish) may take before it is an error.
const WAIT: Duration = Duration::from_secs(120);

/// One compressed frame from the encoder, or why there is none.
type Encoded = Result<Sample, String>;

/// A compressed sample buffer, moved from the encoder's thread to the writer's.
struct Sample(CFRetained<CMSampleBuffer>);

// SAFETY: a CMSampleBuffer is a CoreFoundation object (atomic retain and release); VideoToolbox hands it over finished and
// never touches it again, and after the send only the receiving thread uses it.
unsafe impl Send for Sample {}

/// The encoder's callback: `refcon` is the session's boxed `Sender<Encoded>`, `frame` the frame number given to
/// `encode_frame`.
unsafe extern "C-unwind" fn encoded(
    refcon: *mut c_void,
    frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `refcon` is the Box<Sender<Encoded>> the Encoder owns, freed only after the session is invalidated (Drop),
    // after which VideoToolbox makes no more calls; Sender is Sync, so the encoder's threads may share it.
    let tx = unsafe { &*refcon.cast::<Sender<Encoded>>() };
    let i = frame as usize;
    let msg = if status != 0 {
        Err(format!("encode: VideoToolbox failed frame {i} (OSStatus {status})"))
    } else if flags.contains(VTEncodeInfoFlags::FrameDropped) {
        Err(format!("encode: VideoToolbox dropped frame {i}"))
    } else {
        match NonNull::new(sample) {
            // SAFETY: a live sample buffer VideoToolbox lends for this call; retaining it keeps it past the call.
            Some(s) => Ok(Sample(unsafe { CFRetained::retain(s) })),
            None => Err(format!("encode: VideoToolbox gave frame {i} no sample buffer")),
        }
    };
    // The receiver is gone only once the Encoder is dropped, when nothing is left to tell.
    let _ = tx.send(msg);
}

/// A frame's target: a pixel buffer from the encoder's pool and its two planes mapped into Metal.
pub struct Target {
    buf: CFRetained<CVPixelBuffer>,
    /// The luma plane, `w x h`: `R16Unorm` (10-bit) or `R8Unorm` (8-bit).
    pub luma: Tex,
    /// The CbCr plane, `w/2 x h/2`: `RG16Unorm` or `RG8Unorm`.
    pub chroma: Tex,
    /// While these live, CoreVideo keeps the planes' textures bound to `buf`'s IOSurface.
    _cv: [CFRetained<CVMetalTexture>; 2],
}

/// The soundtrack: interleaved stereo frames at `rate` Hz, fed as LPCM and encoded to ALAC by the writer.
struct Audio {
    rate: u32,
    frames: Vec<[f32; 2]>,
    /// Frames appended so far.
    at: usize,
    format: CFRetained<CMFormatDescription>,
    input: Option<Retained<AVAssetWriterInput>>,
}

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    pool: CFRetained<CVPixelBufferPool>,
    cache: CFRetained<CVMetalTextureCache>,
    /// The callback's refcon: freed after the session is invalidated (Drop).
    tx: *mut Sender<Encoded>,
    rx: Receiver<Encoded>,
    writer: Retained<AVAssetWriter>,
    export: Export,
    path: String,
    /// Made on the first compressed sample: its format description is the passthrough input's format hint.
    video: Option<Retained<AVAssetWriterInput>>,
    audio: Option<Audio>,
    /// Compressed samples received and not yet appended.
    pending: VecDeque<Sample>,
    w: usize,
    h: usize,
    /// A frame is `tick` units of a `timescale`-a-second clock.
    tick: i64,
    timescale: i32,
    sent: u32,
    received: u32,
    /// Where the video appended so far ends, seconds.
    video_end: f64,
    /// Every frame is sent and the encoder completed them all.
    done: bool,
}

fn status(what: &str, r: i32) -> Result<(), String> {
    if r == 0 { Ok(()) } else { Err(format!("encode: {what} (OSStatus {r})")) }
}

/// An NSError's words.
fn words(e: Option<Retained<objc2_foundation::NSError>>) -> String {
    e.map_or_else(|| "AVFoundation gave no reason".to_owned(), |e| e.localizedDescription().to_string())
}

/// A CoreFoundation string constant as the NSString it is.
fn bridged(s: &CFString) -> &NSString {
    // SAFETY: CFStringRef and NSString* are toll-free bridged: the same object, usable as either.
    unsafe { &*std::ptr::from_ref(s).cast::<NSString>() }
}

/// The mastering display colour volume (SMPTE ST 2086) as VideoToolbox takes it: 24 bytes, big-endian, the primaries in
/// G, B, R order then the white point, in 0.00002 units, then the max and min luminance in 0.0001 nits. P3-D65 at 1000
/// nits, 0.0001 nits black.
fn mastering_display() -> [u8; 24] {
    let xy: [u16; 8] = [13250, 34500, 7500, 3000, 34000, 16000, 15635, 16450];
    let mut out = [0u8; 24];
    for (i, v) in xy.iter().enumerate() {
        out[i * 2..i * 2 + 2].copy_from_slice(&v.to_be_bytes());
    }
    out[16..20].copy_from_slice(&((PEAK_NITS * 10_000.0) as u32).to_be_bytes());
    out[20..24].copy_from_slice(&1u32.to_be_bytes());
    out
}

impl Encoder {
    /// A hardware session of `export`'s codec for `w x h` frames at `fps`, writing `path` (replaced if it exists), with
    /// `sound` (interleaved stereo at its rate) as the audio track in `export`'s audio codec, or no audio track.
    pub fn new(
        dev: &Dev,
        path: &Path,
        (w, h): (u32, u32),
        fps: f32,
        export: &Export,
        sound: Option<(u32, Vec<[f32; 2]>)>,
    ) -> Result<Self, String> {
        if sound.is_some() && export.audio == AudioCodec::None {
            return Err("encode: a soundtrack was given to a preset whose audio is none".into());
        }
        let codec = match export.codec {
            Codec::H264 => kCMVideoCodecType_H264,
            Codec::Hevc => kCMVideoCodecType_HEVC,
        };
        let pixel = match export.pixel {
            Pixel::Yuv420p8 => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            Pixel::Yuv420p10 => kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
        };
        let shown = path.display().to_string();
        // SAFETY: CoreVideo and VideoToolbox string constants, alive for the process.
        let session = unsafe {
            let spec = CFDictionary::<CFString, CFType>::from_slices(
                &[kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder],
                &[CFBoolean::new(true)],
            );
            let (fmt, cw, ch) = (
                CFNumber::new_i32(pixel as i32),
                CFNumber::new_i32(w as i32),
                CFNumber::new_i32(h as i32),
            );
            let surface = CFDictionary::<CFString, CFType>::empty();
            let attrs = CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferWidthKey,
                    kCVPixelBufferHeightKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                    kCVPixelBufferMetalCompatibilityKey,
                ],
                &[&fmt, &cw, &ch, &surface, CFBoolean::new(true)],
            );
            (spec, attrs)
        };
        let (tx, rx) = std::sync::mpsc::channel::<Encoded>();
        let tx = Box::into_raw(Box::new(tx));
        let mut out: *mut VTCompressionSession = std::ptr::null_mut();
        // SAFETY: the dictionaries hold the key types VideoToolbox documents (CFBoolean, CFNumber, CFDictionary); the
        // callback matches VTCompressionOutputCallback and `tx` stays valid until the session is invalidated (Drop); `out`
        // is a valid place for the session, returned +1 (Create rule).
        let r = unsafe {
            VTCompressionSession::create(
                None,
                w as i32,
                h as i32,
                codec,
                Some(session.0.as_opaque()),
                Some(session.1.as_opaque()),
                None,
                Some(encoded),
                tx.cast(),
                NonNull::from(&mut out),
            )
        };
        let Some(s) = NonNull::new(out).filter(|_| r == 0) else {
            // SAFETY: no session was made, so nothing holds `tx`.
            drop(unsafe { Box::from_raw(tx) });
            return Err(format!(
                "encode: no hardware {:?} encoder for {w}x{h} {:?} (VTCompressionSessionCreate with \
                 RequireHardwareAcceleratedVideoEncoder, OSStatus {r}); a software encoder is never used",
                export.codec, export.pixel
            ));
        };
        // SAFETY: the +1 reference VTCompressionSessionCreate made.
        let session = unsafe { CFRetained::from_raw(s) };
        // From here the session owns the callback's `tx`: a failure below drops the Encoder (Drop invalidates, then frees).
        let mut cache_out: *mut CVMetalTextureCache = std::ptr::null_mut();
        // SAFETY: a live device, no attributes, and a valid place for the result (+1, Create rule).
        let r = unsafe { CVMetalTextureCache::create(None, None, dev, None, NonNull::from(&mut cache_out)) };
        let cache = match NonNull::new(cache_out).filter(|_| r == kCVReturnSuccess) {
            // SAFETY: the +1 reference CVMetalTextureCacheCreate made.
            Some(c) => unsafe { CFRetained::from_raw(c) },
            None => {
                // SAFETY: the session is live; after Invalidate it makes no more callbacks, so `tx` can go.
                unsafe {
                    session.invalidate();
                    drop(Box::from_raw(tx));
                }
                return Err(format!("encode: CoreVideo made no Metal texture cache (CVReturn {r})"));
            }
        };
        let setup = || -> Result<(CFRetained<CVPixelBufferPool>, Retained<AVAssetWriter>, Option<Audio>), String> {
            Self::configure(&session, fps, export)?;
            // SAFETY: the session is live and configured.
            status("VTCompressionSessionPrepareToEncodeFrames", unsafe { session.prepare_to_encode_frames() })?;
            // SAFETY: the session is live; the pool it gives is retained (Get rule, the binding retains).
            let pool = unsafe { session.pixel_buffer_pool() }
                .ok_or("encode: the VideoToolbox session has no pixel buffer pool")?;
            if path.exists() {
                std::fs::remove_file(path).map_err(|e| format!("{shown}: {e}"))?;
            }
            let url = NSURL::fileURLWithPath(&NSString::from_str(&shown));
            // SAFETY: AVFoundation's constant for the mp4 file type (present on every macOS it supports), and its
            // documented constructor with a file URL.
            let writer = unsafe {
                let mp4 = AVFileTypeMPEG4.ok_or("encode: AVFoundation has no AVFileTypeMPEG4")?;
                let writer = AVAssetWriter::assetWriterWithURL_fileType_error(&url, mp4)
                    .map_err(|e| format!("encode: AVAssetWriter for {shown}: {}", words(Some(e))))?;
                // The moov atom first (ffmpeg's +faststart): playable while it downloads.
                writer.setShouldOptimizeForNetworkUse(true);
                writer
            };
            let audio = match sound {
                None => None,
                Some((rate, frames)) => Some(Audio { rate, frames, at: 0, format: lpcm(rate)?, input: None }),
            };
            Ok((pool, writer, audio))
        };
        match setup() {
            Ok((pool, writer, audio)) => {
                let tick = 1000;
                let timescale = (f64::from(fps) * 1000.0).round() as i32;
                Ok(Self {
                    session,
                    pool,
                    cache,
                    tx,
                    rx,
                    writer,
                    export: export.clone(),
                    path: shown,
                    video: None,
                    audio,
                    pending: VecDeque::new(),
                    w: w as usize,
                    h: h as usize,
                    tick,
                    timescale,
                    sent: 0,
                    received: 0,
                    video_end: 0.0,
                    done: false,
                })
            }
            Err(e) => {
                // SAFETY: as above: invalidate, then free the callback's `tx`.
                unsafe {
                    session.invalidate();
                    drop(Box::from_raw(tx));
                }
                Err(e)
            }
        }
    }

    /// Every property, each checked: one VideoToolbox refuses is an error naming it.
    fn configure(s: &VTCompressionSession, fps: f32, export: &Export) -> Result<(), String> {
        let set = |name: &str, key: &CFString, value: &CFType| {
            // SAFETY: a live session, and each value is the CF type its key documents.
            let r = unsafe { VTSessionSetProperty(s, key, Some(value)) };
            status(&format!("VideoToolbox refused {name}"), r)
        };
        let gop = (f64::from(fps) * export.keyframe_seconds).round().max(1.0) as i32;
        // SAFETY: VideoToolbox and CoreVideo string constants, alive for the process.
        unsafe {
            match export.profile {
                Profile::High => set("ProfileLevel = H264_High_AutoLevel", kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel)?,
                Profile::Main => set("ProfileLevel = HEVC_Main_AutoLevel", kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main_AutoLevel)?,
                Profile::Main10 => set("ProfileLevel = HEVC_Main10_AutoLevel", kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main10_AutoLevel)?,
            }
            if let Some(kbps) = export.bitrate_kbps {
                let bits = i64::from(kbps) * 1000;
                set(&format!("AverageBitRate = {bits}"), kVTCompressionPropertyKey_AverageBitRate, &CFNumber::new_i64(bits))?;
            }
            if let Some(q) = export.quality {
                set(&format!("Quality = {q}"), kVTCompressionPropertyKey_Quality, &CFNumber::new_f64(q))?;
            }
            set("RealTime = false", kVTCompressionPropertyKey_RealTime, CFBoolean::new(false))?;
            set(
                "PrioritizeEncodingSpeedOverQuality = false",
                kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                CFBoolean::new(false),
            )?;
            set("AllowFrameReordering = true", kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(true))?;
            // H.264's key frames from VideoToolbox are IDR frames, so its groups are always closed; HEVC's may be open
            // (frames after a key frame in decode order that lean on ones before it) unless told not to.
            if export.codec == Codec::Hevc {
                set(
                    &format!("AllowOpenGOP = {}", !export.closed_gop),
                    kVTCompressionPropertyKey_AllowOpenGOP,
                    CFBoolean::new(!export.closed_gop),
                )?;
            }
            set(&format!("MaxKeyFrameInterval = {gop}"), kVTCompressionPropertyKey_MaxKeyFrameInterval, &CFNumber::new_i32(gop))?;
            set(
                &format!("MaxKeyFrameIntervalDuration = {}", export.keyframe_seconds),
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                &CFNumber::new_f64(export.keyframe_seconds),
            )?;
            set(&format!("ExpectedFrameRate = {fps}"), kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::new_f64(f64::from(fps)))?;
            match export.colour {
                Colour::Bt709 => {
                    set("ColorPrimaries = ITU_R_709_2", kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2)?;
                    set("TransferFunction = ITU_R_709_2", kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2)?;
                    set("YCbCrMatrix = ITU_R_709_2", kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2)?;
                }
                Colour::Hdr10 => {
                    let mastering = CFData::from_bytes(&mastering_display());
                    set(
                        "SpatialAdaptiveQPLevel = Default",
                        kVTCompressionPropertyKey_SpatialAdaptiveQPLevel,
                        &CFNumber::new_i32(kVTQPModulationLevel_Default),
                    )?;
                    set("ColorPrimaries = ITU_R_2020", kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_2020)?;
                    set(
                        "TransferFunction = SMPTE_ST_2084_PQ",
                        kVTCompressionPropertyKey_TransferFunction,
                        kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
                    )?;
                    set("YCbCrMatrix = ITU_R_2020", kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_2020)?;
                    set("MasteringDisplayColorVolume", kVTCompressionPropertyKey_MasteringDisplayColorVolume, &mastering)?;
                    set("HDRMetadataInsertionMode = Auto", kVTCompressionPropertyKey_HDRMetadataInsertionMode, kVTHDRMetadataInsertionMode_Auto)?;
                }
            }
        }
        Ok(())
    }

    /// The next frame's pixel buffer, tagged with the preset's colour, with its planes mapped into Metal for the colour
    /// pass to write.
    pub fn target(&mut self) -> Result<Target, String> {
        // CoreVideo asks for this periodically: it recycles textures no one holds.
        self.cache.flush(0);
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: a live pool and a valid place for the buffer (+1, Create rule).
        let r = unsafe { CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut out)) };
        let Some(buf) = NonNull::new(out).filter(|_| r == kCVReturnSuccess) else {
            return Err(format!("encode: frame {}: the encoder's pool gave no pixel buffer (CVReturn {r})", self.sent));
        };
        // SAFETY: the +1 reference CVPixelBufferPoolCreatePixelBuffer made.
        let buf = unsafe { CFRetained::from_raw(buf) };
        // SAFETY: CoreVideo's documented attachment keys with their documented CFString values, on a live buffer.
        unsafe {
            let (primaries, transfer, matrix) = match self.export.colour {
                Colour::Bt709 => (
                    kCVImageBufferColorPrimaries_ITU_R_709_2,
                    kCVImageBufferTransferFunction_ITU_R_709_2,
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                ),
                Colour::Hdr10 => (
                    kCVImageBufferColorPrimaries_ITU_R_2020,
                    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
                    kCVImageBufferYCbCrMatrix_ITU_R_2020,
                ),
            };
            for (key, value) in [
                (kCVImageBufferColorPrimariesKey, primaries),
                (kCVImageBufferTransferFunctionKey, transfer),
                (kCVImageBufferYCbCrMatrixKey, matrix),
                (kCVImageBufferChromaLocationTopFieldKey, kCVImageBufferChromaLocation_Left),
                (kCVImageBufferChromaLocationBottomFieldKey, kCVImageBufferChromaLocation_Left),
            ] {
                buf.set_attachment(key, value, CVAttachmentMode::ShouldPropagate);
            }
        }
        let usage = CFNumber::new_i64((MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite).bits() as i64);
        // SAFETY: a CoreVideo string constant, alive for the process.
        let attrs = CFDictionary::<CFString, CFType>::from_slices(&[unsafe { kCVMetalTextureUsage }], &[&usage]);
        let (w, h) = (self.w, self.h);
        let map = |plane: usize, format: MTLPixelFormat, pw: usize, ph: usize| -> Result<(CFRetained<CVMetalTexture>, Tex), String> {
            let mut out: *mut CVMetalTexture = std::ptr::null_mut();
            // SAFETY: a live cache and a live IOSurface-backed, Metal-compatible biplanar 4:2:0 buffer (the pool's attributes ask
            // for one) whose plane `plane` is `pw x ph` of the format given (16-bit luma, 2x16-bit CbCr at half size); the
            // attributes hold kCVMetalTextureUsage as a CFNumber; `out` is a valid place for the +1 result.
            let r = unsafe {
                CVMetalTextureCache::create_texture_from_image(
                    None,
                    &self.cache,
                    &buf,
                    Some(attrs.as_opaque()),
                    format,
                    pw,
                    ph,
                    plane,
                    NonNull::from(&mut out),
                )
            };
            let Some(cv) = NonNull::new(out).filter(|_| r == kCVReturnSuccess) else {
                return Err(format!("encode: CoreVideo could not map plane {plane} ({pw}x{ph}, {format:?}) into Metal (CVReturn {r})"));
            };
            // SAFETY: the +1 reference CVMetalTextureCacheCreateTextureFromImage made.
            let cv = unsafe { CFRetained::from_raw(cv) };
            let tex = CVMetalTextureGetTexture(&cv).ok_or_else(|| format!("encode: plane {plane} mapped to no Metal texture"))?;
            Ok((cv, tex))
        };
        let (one, two) = match self.export.pixel {
            Pixel::Yuv420p8 => (MTLPixelFormat::R8Unorm, MTLPixelFormat::RG8Unorm),
            Pixel::Yuv420p10 => (MTLPixelFormat::R16Unorm, MTLPixelFormat::RG16Unorm),
        };
        let (cv_luma, luma) = map(0, one, w, h)?;
        let (cv_chroma, chroma) = map(1, two, w / 2, h / 2)?;
        Ok(Target { buf, luma, chroma, _cv: [cv_luma, cv_chroma] })
    }

    /// Hand frame `i` (its planes written, the GPU done with them) to the encoder, and append what has come back.
    pub fn encode(&mut self, target: Target, i: u32) -> Result<(), String> {
        let Target { buf, luma, chroma, _cv } = target;
        // The Metal views go first: only the pixel buffer travels on.
        drop((luma, chroma, _cv));
        // SAFETY: CMTimeMake with a positive timescale.
        let (pts, dur) = unsafe { (CMTime::new(i64::from(i) * self.tick, self.timescale), CMTime::new(self.tick, self.timescale)) };
        // SAFETY: a live session and a pixel buffer from its own pool; the frame refcon is a number the callback only
        // reads back, and the info-flags out pointer may be null.
        let r = unsafe { self.session.encode_frame(&buf, pts, dur, None, i as usize as *mut c_void, std::ptr::null_mut()) };
        status(&format!("VTCompressionSessionEncodeFrame refused frame {i}"), r)?;
        self.sent += 1;
        // A bound on the pool buffers held: wait for the encoder once too many frames are in it.
        while self.sent - self.received > IN_FLIGHT {
            match self.rx.recv_timeout(WAIT) {
                Ok(m) => self.take(m)?,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "encode: frame {i}: the encoder returned nothing for {} s with {} frames in it",
                        WAIT.as_secs(),
                        self.sent - self.received
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => return Err("encode: the encoder's channel closed".into()),
            }
        }
        self.pump(false)
    }

    fn take(&mut self, m: Encoded) -> Result<(), String> {
        self.pending.push_back(m?);
        self.received += 1;
        Ok(())
    }

    /// The inputs and the writer, made on the first compressed sample (its format description is the passthrough hint).
    fn start(&mut self) -> Result<(), String> {
        let Some(first) = self.pending.front() else { return Ok(()) };
        // SAFETY: a live sample buffer; the format description it gives is retained.
        let hint = unsafe { first.0.format_description() }.ok_or("encode: the first compressed frame has no format description")?;
        // SAFETY: AVFoundation's media type constants (present on every macOS it supports); its documented constructors
        // and setters with live arguments; every input is checked with canAddInput (adding one it refuses throws) and the
        // audio settings with canApplyOutputSettings (the same).
        unsafe {
            let video_type = AVMediaTypeVideo.ok_or("encode: AVFoundation has no AVMediaTypeVideo")?;
            let video = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings_sourceFormatHint(video_type, None, Some(&hint));
            video.setExpectsMediaDataInRealTime(false);
            if !self.writer.canAddInput(&video) {
                return Err(format!("encode: {}: AVAssetWriter cannot take the {:?} track", self.path, self.export.codec));
            }
            self.writer.addInput(&video);
            self.video = Some(video);
            if let Some(a) = &mut self.audio {
                let audio_type = AVMediaTypeAudio.ok_or("encode: AVFoundation has no AVMediaTypeAudio")?;
                let key = |k: Option<&'static NSString>, name: &str| k.ok_or_else(|| format!("encode: AVFAudio has no {name}"));
                // The format, then what is only that format's: ALAC's bit depth, AAC-LC's bitrate.
                let (format, name, extra_key, extra) = match (self.export.audio, self.export.audio_kbps) {
                    (AudioCodec::Alac, _) => (
                        kAudioFormatAppleLossless,
                        format!("ALAC, {AUDIO_BITS} bits"),
                        key(AVEncoderBitDepthHintKey, "AVEncoderBitDepthHintKey")?,
                        AUDIO_BITS,
                    ),
                    // kAudioFormatMPEG4AAC is AAC Low Complexity (HE-AAC has its own format ids).
                    (AudioCodec::AacLc, Some(kbps)) => (
                        kAudioFormatMPEG4AAC,
                        format!("AAC-LC, {kbps} kb/s"),
                        key(AVEncoderBitRateKey, "AVEncoderBitRateKey")?,
                        kbps as i32 * 1000,
                    ),
                    other => return Err(format!("encode: a soundtrack for audio {other:?}")),
                };
                let keys = [
                    key(AVFormatIDKey, "AVFormatIDKey")?,
                    key(AVSampleRateKey, "AVSampleRateKey")?,
                    key(AVNumberOfChannelsKey, "AVNumberOfChannelsKey")?,
                    extra_key,
                ];
                let values = [
                    NSNumber::numberWithUnsignedInt(format),
                    NSNumber::numberWithDouble(f64::from(a.rate)),
                    NSNumber::numberWithInt(2),
                    NSNumber::numberWithInt(extra),
                ];
                let values: Vec<&AnyObject> = values.iter().map(|v| -> &AnyObject { v }).collect();
                let settings = NSDictionary::<NSString, AnyObject>::from_slices(&keys, &values);
                if !self.writer.canApplyOutputSettings_forMediaType(Some(&settings), audio_type) {
                    return Err(format!("encode: AVAssetWriter refuses {name} at {} Hz, 2 channels", a.rate));
                }
                let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(audio_type, Some(&settings));
                input.setExpectsMediaDataInRealTime(false);
                if !self.writer.canAddInput(&input) {
                    return Err(format!("encode: {}: AVAssetWriter cannot take the {name} track", self.path));
                }
                self.writer.addInput(&input);
                a.input = Some(input);
            }
            if !self.writer.startWriting() {
                return Err(format!("encode: {}: AVAssetWriter startWriting: {}", self.path, words(self.writer.error())));
            }
            self.writer.startSessionAtSourceTime(CMTime::new(0, self.timescale));
        }
        Ok(())
    }

    /// Append what can be: compressed frames as they come, audio up to [`AUDIO_LEAD`] past them (all of it once the
    /// video is done). With `wait`, until everything is appended, each stall bounded by [`WAIT`].
    fn pump(&mut self, wait: bool) -> Result<(), String> {
        let mut stalled = Instant::now();
        loop {
            while let Ok(m) = self.rx.try_recv() {
                self.take(m)?;
            }
            if self.video.is_none() {
                if self.pending.is_empty() {
                    if !wait {
                        return Ok(());
                    }
                    return Err(format!("encode: {}: the encoder gave no frames", self.path));
                }
                self.start()?;
            }
            let mut moved = false;
            let video = self.video.as_ref().ok_or("encode: no video input")?;
            // SAFETY: AVAssetWriterInput's readiness getter and append, with live sample buffers in decode order; the
            // writer is writing (started above).
            while !self.pending.is_empty() && unsafe { video.isReadyForMoreMediaData() } {
                let Some(s) = self.pending.pop_front() else { break };
                // SAFETY: a live sample buffer.
                let pts = unsafe { s.0.presentation_time_stamp() };
                if !unsafe { video.appendSampleBuffer(&s.0) } {
                    return Err(format!(
                        "encode: {}: AVAssetWriter took no video sample ({})",
                        self.path,
                        // SAFETY: AVAssetWriter's error getter.
                        words(unsafe { self.writer.error() })
                    ));
                }
                // SAFETY: CMTimeGetSeconds of a valid time.
                let end = unsafe { pts.seconds() } + self.tick as f64 / f64::from(self.timescale);
                self.video_end = self.video_end.max(end);
                moved = true;
            }
            let video_done = self.done && self.pending.is_empty();
            let mut audio_left = false;
            if let Some(a) = &mut self.audio {
                let input = a.input.as_ref().ok_or("encode: no audio input")?;
                let limit = if video_done { f64::INFINITY } else { self.video_end + AUDIO_LEAD };
                // SAFETY: AVAssetWriterInput's readiness getter.
                while a.at < a.frames.len() && (a.at as f64 / f64::from(a.rate)) < limit && unsafe { input.isReadyForMoreMediaData() } {
                    let end = (a.at + AUDIO_CHUNK).min(a.frames.len());
                    let s = chunk(&a.format, a.rate, a.at, &a.frames[a.at..end])?;
                    // SAFETY: a live LPCM sample buffer in the format the input's settings encode from.
                    if !unsafe { input.appendSampleBuffer(&s) } {
                        return Err(format!(
                            "encode: {}: AVAssetWriter took no audio at {} s ({})",
                            self.path,
                            a.at as f64 / f64::from(a.rate),
                            // SAFETY: AVAssetWriter's error getter.
                            words(unsafe { self.writer.error() })
                        ));
                    }
                    a.at = end;
                    moved = true;
                }
                audio_left = a.at < a.frames.len();
            }
            if !wait {
                return Ok(());
            }
            if video_done && !audio_left {
                return Ok(());
            }
            if moved {
                stalled = Instant::now();
            } else if stalled.elapsed() > WAIT {
                return Err(format!(
                    "encode: {}: AVAssetWriter took nothing for {} s ({} video samples waiting, audio {}); its status: {}",
                    self.path,
                    WAIT.as_secs(),
                    self.pending.len(),
                    if audio_left { "unfinished" } else { "done" },
                    // SAFETY: AVAssetWriter's error getter.
                    words(unsafe { self.writer.error() })
                ));
            } else {
                // The writer flips readiness from its own queue: look again shortly.
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Complete every frame, append the rest, and finish the file.
    pub fn finish(mut self) -> Result<(), String> {
        // SAFETY: a live session; kCMTimeInvalid (a constant) completes every pending frame before returning.
        status("VTCompressionSessionCompleteFrames", unsafe { self.session.complete_frames(kCMTimeInvalid) })?;
        while let Ok(m) = self.rx.try_recv() {
            self.take(m)?;
        }
        if self.received != self.sent {
            return Err(format!("encode: sent {} frames, the encoder gave back {}", self.sent, self.received));
        }
        self.done = true;
        self.pump(true)?;
        let video = self.video.as_ref().ok_or("encode: no video input")?;
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let handler = RcBlock::new(move || {
            let _ = tx.send(());
        });
        // SAFETY: every append has returned; markAsFinished on each input, then the documented asynchronous finish, whose
        // handler (a Send closure over a Sender) may run on any queue.
        unsafe {
            video.markAsFinished();
            if let Some(input) = self.audio.as_ref().and_then(|a| a.input.as_ref()) {
                input.markAsFinished();
            }
            self.writer.finishWritingWithCompletionHandler(&handler);
        }
        rx.recv_timeout(WAIT)
            .map_err(|_| format!("encode: {}: AVAssetWriter did not finish within {} s", self.path, WAIT.as_secs()))?;
        // SAFETY: AVAssetWriter's status and error getters.
        let (st, err) = unsafe { (self.writer.status(), self.writer.error()) };
        if st != AVAssetWriterStatus::Completed {
            return Err(format!("encode: {}: finishing failed (status {}): {}", self.path, st.0, words(err)));
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: invalidating stops every callback (it waits for any in progress), so the callback's Sender is freed only
        // after the last use; `tx` came from Box::into_raw and is freed once, here.
        unsafe {
            self.session.invalidate();
            drop(Box::from_raw(self.tx));
        }
    }
}

/// An LPCM format: interleaved stereo float32 at `rate`.
fn lpcm(rate: u32) -> Result<CFRetained<CMFormatDescription>, String> {
    let asbd = AudioStreamBasicDescription {
        mSampleRate: f64::from(rate),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 8,
        mFramesPerPacket: 1,
        mBytesPerFrame: 8,
        mChannelsPerFrame: 2,
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    let mut out: *const CMFormatDescription = std::ptr::null();
    // SAFETY: a valid ASBD (copied by the call), no channel layout, magic cookie or extensions, and a valid place for the
    // +1 result.
    let r = unsafe {
        CMAudioFormatDescriptionCreate(None, NonNull::from(&asbd), 0, std::ptr::null(), 0, std::ptr::null(), None, NonNull::from(&mut out))
    };
    status(&format!("CMAudioFormatDescriptionCreate for {rate} Hz stereo float"), r)?;
    let d = NonNull::new(out.cast_mut()).ok_or("encode: CMAudioFormatDescriptionCreate gave nothing")?;
    // SAFETY: the +1 reference it made.
    Ok(unsafe { CFRetained::from_raw(d) })
}

/// One sample buffer of the soundtrack: `frames` from frame `at`, copied into a block buffer.
fn chunk(format: &CMFormatDescription, rate: u32, at: usize, frames: &[[f32; 2]]) -> Result<CFRetained<CMSampleBuffer>, String> {
    let bytes = std::mem::size_of_val(frames);
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    // SAFETY: a block buffer CoreMedia allocates now (null memory block, the default allocator), `bytes` long; `block`
    // is a valid place for the +1 result.
    let r = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            std::ptr::null_mut(),
            bytes,
            None,
            std::ptr::null(),
            0,
            bytes,
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block),
        )
    };
    status("CMBlockBufferCreateWithMemoryBlock", r)?;
    let block = NonNull::new(block).ok_or("encode: CMBlockBufferCreateWithMemoryBlock gave nothing")?;
    // SAFETY: the +1 reference it made.
    let block = unsafe { CFRetained::from_raw(block) };
    // SAFETY: `frames` is `bytes` long and the block holds `bytes` from offset 0.
    status("CMBlockBufferReplaceDataBytes", unsafe { CMBlockBuffer::replace_data_bytes(NonNull::from(frames).cast(), &block, 0, bytes) })?;
    let mut out: *mut CMSampleBuffer = std::ptr::null_mut();
    // SAFETY: a live block of `frames.len()` packets of the LPCM format (constant size, so no packet descriptions), a
    // presentation time in samples, and a valid place for the +1 result.
    let r = unsafe {
        CMAudioSampleBufferCreateReadyWithPacketDescriptions(
            None,
            &block,
            format,
            frames.len() as isize,
            CMTime::new(at as i64, rate as i32),
            std::ptr::null(),
            NonNull::from(&mut out),
        )
    };
    status(&format!("CMAudioSampleBufferCreateReadyWithPacketDescriptions at frame {at}"), r)?;
    let s = NonNull::new(out).ok_or("encode: no audio sample buffer")?;
    // SAFETY: the +1 reference it made.
    Ok(unsafe { CFRetained::from_raw(s) })
}
