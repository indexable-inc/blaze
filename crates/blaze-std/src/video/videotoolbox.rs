//! The VideoToolbox provider: macOS's hardware HEVC encoder (the media engine), frames in and
//! packets out. Moved from harness-video's `encode.rs` and reshaped: the compression session,
//! its properties and its pixel buffer pool are that file's; AVAssetWriter is gone (the packets
//! go to [`super::mp4::Muxer`], or wherever the caller sends them), and so are the Metal views
//! of the pool's buffers (a frame is bytes in the caller's allocator, copied into a pool buffer:
//! the zero-copy path waits for `Gpu` to own render targets).
//!
//! Every unsafe call is in this file, each with its `SAFETY`. It uses only public, documented
//! API of VideoToolbox, CoreMedia, CoreVideo and CoreFoundation: no private symbol.
//!
//! The session calls back on its own threads. Each call puts its sample buffer (or why there is
//! none) in the session's outbox, a named [`Lock`]; [`Session::receive`] takes from it and never
//! waits. The callback's refcon is the outbox, which the session holds until after it is
//! invalidated (field order: [`Live`] drops first), after which VideoToolbox calls no more.

use alloc::{alloc::Global, sync::Arc, vec::Vec};
use core::{alloc::Allocator, ffi::c_void, ptr::NonNull};

use blaze_core::lock::Lock;
use objc2_core_foundation::{CFArray, CFBoolean, CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMTimeFlags, kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
    kCMSampleAttachmentKey_NotSync, kCMTimeInvalid, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVAttachmentMode, CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress, kCVImageBufferChromaLocationBottomFieldKey, kCVImageBufferChromaLocationTopFieldKey,
    kCVImageBufferChromaLocation_Left, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_2020,
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferColorPrimaries_P3_D65, kCVImageBufferTransferFunctionKey,
    kCVImageBufferTransferFunction_ITU_R_2100_HLG, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_2020,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange, kCVReturnSuccess,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ContentLightLevelInfo,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_HDRMetadataInsertionMode,
    kVTCompressionPropertyKey_MasteringDisplayColorVolume, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_Quality, kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTHDRMetadataInsertionMode_Auto, kVTProfileLevel_HEVC_Main10_AutoLevel,
    kVTProfileLevel_HEVC_Main_AutoLevel, kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use super::{Colour, EncoderConfig, Frame, Packet, PixelFormat, VideoError};
use crate::{
    gpu::Gpu,
    mem::{self, Named, OutOfMemory, heap},
};

/// Frames handed to the encoder and not yet back, past which [`Session::send`] has the encoder
/// finish the oldest first: each holds a pool buffer (25 MB at 4K in P010).
const IN_FLIGHT: usize = 32;

/// A compressed sample buffer, moved from the encoder's thread to the receiver's.
struct Done(CFRetained<CMSampleBuffer>);

// SAFETY: a CMSampleBuffer is a CoreFoundation object (atomic retain and release); VideoToolbox
// hands it over finished and never touches it again, and after the hand-over only the
// receiving thread uses it.
unsafe impl Send for Done {}

/// One callback's outcome: its sample buffer, or why there is none.
type Outcome = Result<Done, VideoError>;

/// What the callback leaves for [`Session::receive`].
struct Outbox {
    /// Each finished frame, in decode order (the order VideoToolbox calls back in); those
    /// before `head` are taken (`None`), and go once they are half of it.
    // alloc: the outbox's queue, on the global heap: the encoder's threads have no caller's
    // allocator.
    ready: mem::Vec<Option<Outcome>, Global>,
    head: usize,
    /// Frames finished, dropped or failed: every callback.
    done: u64,
    /// The first growth of `ready` that found no room. The frame it held is lost, so every
    /// receive after it says so (rule no-fallback: the stream is not quietly short a frame).
    lost: Option<OutOfMemory>,
}

impl Outbox {
    /// `outcome` at the end of the queue; when the queue has no room, it is lost, and said.
    fn put(&mut self, outcome: Outcome) {
        if let Err(error) = self.ready.push(Some(outcome)) {
            self.lost.get_or_insert(error);
        }
    }

    /// The oldest outcome not yet taken, or `None`; the error once one was lost.
    fn take(&mut self) -> Result<Option<Outcome>, VideoError> {
        if let Some(lost) = self.lost {
            return Err(VideoError::OutOfMemory {
                what: "the encoder's outbox (a finished frame was lost)",
                asked: lost.asked,
            });
        }
        let next = self.ready.get_mut(self.head).and_then(Option::take);
        if next.is_some() {
            self.head += 1;
        }
        // The taken slots go once they are half the queue: each outcome moves at most once
        // per halving, so a take is constant time over a run.
        if self.head > 0 && self.head * 2 >= self.ready.len() {
            self.ready.retain(Option::is_some);
            self.head = 0;
        }
        Ok(next)
    }
}

/// The session, invalidated when it drops: after that VideoToolbox makes no more callbacks.
struct Live(CFRetained<VTCompressionSession>);

impl Drop for Live {
    fn drop(&mut self) {
        // SAFETY: a live session; Invalidate waits for any callback in progress, and none
        // comes after it, so the outbox the refcon points at may go once this returns.
        unsafe { self.0.invalidate() }
    }
}

/// The encoder's callback: `refcon` is the session's outbox, `frame` the frame's time as given
/// to `encode_frame`.
unsafe extern "C-unwind" fn encoded(
    refcon: *mut c_void,
    frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `refcon` is `Arc::as_ptr` of the outbox the Session holds; the Session invalidates
    // the session (`Live`, its first field) before the outbox can drop, and no callback runs
    // after Invalidate returns. Lock is Sync, so the encoder's threads may share it.
    let outbox = unsafe { &*refcon.cast_const().cast::<Lock<Outbox>>() };
    let time = frame.addr() as i64;
    let result = if status != 0 {
        Err(VideoError::Platform { call: "VTCompressionOutputCallback", status })
    } else if flags.contains(VTEncodeInfoFlags::FrameDropped) {
        Err(VideoError::Dropped { time })
    } else {
        match NonNull::new(sample) {
            // SAFETY: a live sample buffer VideoToolbox lends for this call; retaining it keeps
            // it past the call.
            Some(sample) => Ok(Done(unsafe { CFRetained::retain(sample) })),
            None => Err(VideoError::Missing { what: "the compressed frame's sample buffer" }),
        }
    };
    let mut outbox = outbox.lock();
    outbox.put(result);
    outbox.done += 1;
}

/// A CoreFoundation or VideoToolbox status, as a result naming the call.
fn checked(call: &'static str, status: i32) -> Result<(), VideoError> {
    if status == 0 { Ok(()) } else { Err(VideoError::Platform { call, status }) }
}

/// A CMTime of `value` ticks of `timescale` (checked to fit an i32 by the config).
fn time(value: i64, timescale: u32) -> CMTime {
    // SAFETY: CMTimeMake, a pure constructor, with a positive timescale.
    unsafe { CMTime::new(value, timescale as i32) }
}

/// The config's colour as CoreVideo's names: primaries, transfer and matrix. A code point it has
/// no name for is refused, never mapped to another.
#[expect(clippy::multiple_unsafe_ops_per_block, reason = "each extern static read is an unsafe op; one block of constants, one SAFETY")]
fn colour_names(colour: &Colour) -> Result<[&'static CFString; 3], VideoError> {
    if colour.full_range {
        return Err(VideoError::Config {
            field: "colour.full_range",
            why: "is true; the VideoToolbox provider takes limited-range (video range) frames only",
        });
    }
    // SAFETY: CoreVideo's string constants, alive for the process.
    unsafe {
        let primaries = match colour.primaries {
            1 => kCVImageBufferColorPrimaries_ITU_R_709_2,
            9 => kCVImageBufferColorPrimaries_ITU_R_2020,
            12 => kCVImageBufferColorPrimaries_P3_D65,
            _ => {
                return Err(VideoError::Config {
                    field: "colour.primaries",
                    why: "is not 1 (BT.709), 9 (BT.2020) or 12 (P3-D65), the ones the provider names",
                });
            }
        };
        let transfer = match colour.transfer {
            1 => kCVImageBufferTransferFunction_ITU_R_709_2,
            16 => kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
            18 => kCVImageBufferTransferFunction_ITU_R_2100_HLG,
            _ => {
                return Err(VideoError::Config {
                    field: "colour.transfer",
                    why: "is not 1 (BT.709), 16 (PQ) or 18 (HLG), the ones the provider names",
                });
            }
        };
        let matrix = match colour.matrix {
            1 => kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            9 => kCVImageBufferYCbCrMatrix_ITU_R_2020,
            _ => {
                return Err(VideoError::Config {
                    field: "colour.matrix",
                    why: "is not 1 (BT.709) or 9 (BT.2020), the ones the provider names",
                });
            }
        };
        Ok([primaries, transfer, matrix])
    }
}

/// One VideoToolbox compression session (module docs).
pub(super) struct Session {
    /// First, so it drops (and invalidates) before the outbox its callback writes.
    session: Live,
    pool: CFRetained<CVPixelBufferPool>,
    outbox: Arc<Lock<Outbox>>,
    config: EncoderConfig,
    /// CoreVideo's names for the config's colour, for each frame's attachments.
    colour: [&'static CFString; 3],
    /// The times of the last [`IN_FLIGHT`] frames sent: frame `n`'s at `n % IN_FLIGHT`.
    recent: [i64; IN_FLIGHT],
    sent: u64,
    last_time: Option<i64>,
    finished: bool,
    /// The stream's `HEVCDecoderConfigurationRecord`, from the first packet's format.
    // alloc: once a stream, its hundred-odd bytes.
    hvcc: Option<Vec<u8>>,
}

impl Session {
    /// A hardware HEVC session for `config` (checked by the caller). `_gpu` is the authority to
    /// use the machine's media engine; the placeholder holds nothing yet.
    #[expect(clippy::multiple_unsafe_ops_per_block, reason = "each extern static read is an unsafe op; one block of constants, one SAFETY")]
    pub(super) fn new(_gpu: &Gpu, config: &EncoderConfig) -> Result<Session, VideoError> {
        let colour = colour_names(&config.colour)?;
        let format = match config.format {
            PixelFormat::Nv12 => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            PixelFormat::P010 => kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
        };
        // SAFETY: VideoToolbox's string constants, alive for the process.
        let profile = unsafe {
            match config.format {
                PixelFormat::Nv12 => kVTProfileLevel_HEVC_Main_AutoLevel,
                PixelFormat::P010 => kVTProfileLevel_HEVC_Main10_AutoLevel,
            }
        };
        let (width, height) = (config.width as i32, config.height as i32);
        // SAFETY: CoreVideo and VideoToolbox string constants, alive for the process.
        let (spec, attrs) = unsafe {
            let spec = CFDictionary::<CFString, CFType>::from_slices(
                &[kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder],
                &[CFBoolean::new(true)],
            );
            let (fmt, w, h) =
                (CFNumber::new_i32(format as i32), CFNumber::new_i32(width), CFNumber::new_i32(height));
            let surface = CFDictionary::<CFString, CFType>::empty();
            let attrs = CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferWidthKey,
                    kCVPixelBufferHeightKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                ],
                &[&fmt, &w, &h, &surface],
            );
            (spec, attrs)
        };
        let out_of_memory = |error: OutOfMemory| VideoError::OutOfMemory { what: "the encoder's outbox", asked: error.asked };
        // alloc: the outbox's queue, on the global heap, which the encoder's threads share.
        let ready = mem::Vec::with_capacity_in(IN_FLIGHT + 1, Global).map_err(out_of_memory)?;
        let outbox = Lock::new("bstd.video.videotoolbox.outbox", Outbox { ready, head: 0, done: 0, lost: None });
        // Declared before the session, so it drops after it on every return below.
        // alloc: the outbox, shared with the encoder's threads.
        let outbox = heap::shared(outbox).map_err(out_of_memory)?;
        let mut out: *mut VTCompressionSession = core::ptr::null_mut();
        // SAFETY: the dictionaries hold the value types VideoToolbox documents for their keys
        // (CFBoolean, CFNumber, CFDictionary); the callback matches VTCompressionOutputCallback;
        // the refcon is the outbox, alive until after the session is invalidated (`Live`); `out`
        // is a valid place for the session, returned +1 (the Create rule).
        let status = unsafe {
            VTCompressionSession::create(
                None,
                width,
                height,
                kCMVideoCodecType_HEVC,
                Some(spec.as_opaque()),
                Some(attrs.as_opaque()),
                None,
                Some(encoded),
                Arc::as_ptr(&outbox).cast_mut().cast(),
                NonNull::from(&mut out),
            )
        };
        let Some(made) = NonNull::new(out).filter(|_| status == 0) else {
            // No session: nothing holds the refcon.
            return Err(VideoError::Platform {
                call: "VTCompressionSessionCreate (RequireHardwareAcceleratedVideoEncoder: no software encoder is taken)",
                status,
            });
        };
        // SAFETY: the +1 reference VTCompressionSessionCreate made.
        let session = Live(unsafe { CFRetained::from_raw(made) });
        configure(&session.0, config, profile, colour)?;
        // SAFETY: a live, configured session.
        checked("VTCompressionSessionPrepareToEncodeFrames", unsafe { session.0.prepare_to_encode_frames() })?;
        // SAFETY: a live session; the binding retains the pool it gives (the Get rule).
        let pool = unsafe { session.0.pixel_buffer_pool() }
            .ok_or(VideoError::Missing { what: "the compression session's pixel buffer pool" })?;
        Ok(Session {
            session,
            pool,
            outbox,
            config: *config,
            colour,
            recent: [0; IN_FLIGHT],
            sent: 0,
            last_time: None,
            finished: false,
            hvcc: None,
        })
    }

    /// `frame`, copied into a pool buffer, tagged with the config's colour and handed to the
    /// encoder.
    pub(super) fn send<A: Allocator>(&mut self, frame: &Frame<A>) -> Result<(), VideoError> {
        let at = frame.time();
        if self.finished {
            return Err(VideoError::Frame { time: at, why: "it was sent after the encoder was finished" });
        }
        if frame.format() != self.config.format {
            return Err(VideoError::Frame { time: at, why: "its pixel format is not the encoder's" });
        }
        if frame.size() != (self.config.width, self.config.height) {
            return Err(VideoError::Frame { time: at, why: "its size is not the encoder's" });
        }
        if frame.timescale() != self.config.timescale {
            return Err(VideoError::Frame { time: at, why: "its timescale is not the encoder's" });
        }
        if self.last_time.is_some_and(|last| at <= last) {
            return Err(VideoError::Frame { time: at, why: "its time is not after the frame before" });
        }
        let buffer = self.pixel_buffer(at)?;
        self.copy_in(&buffer, frame)?;
        tag(&buffer, self.colour);
        let (pts, duration) =
            (time(at, self.config.timescale), time(i64::from(frame.duration()), self.config.timescale));
        // SAFETY: a live session and a pixel buffer from its own pool; the frame refcon is the
        // frame's time, a number the callback only reads back; the info-flags pointer may be null.
        let status = unsafe {
            self.session.0.encode_frame(
                &buffer,
                pts,
                duration,
                None,
                core::ptr::without_provenance_mut(at as usize),
                core::ptr::null_mut(),
            )
        };
        checked("VTCompressionSessionEncodeFrame", status)?;
        let n = self.sent;
        self.sent += 1;
        self.last_time = Some(at);
        // Frame `n`'s slot held frame `n - IN_FLIGHT`'s time: the one sent IN_FLIGHT frames ago.
        let slot = (n % IN_FLIGHT as u64) as usize;
        let Some(cell) = self.recent.get_mut(slot) else {
            return Err(VideoError::Internal { what: "the recent frames' times", index: slot, len: IN_FLIGHT });
        };
        let oldest = core::mem::replace(cell, at);
        // The bound on pool buffers held: when more than IN_FLIGHT frames are still inside,
        // have the encoder finish every frame up to the one sent IN_FLIGHT frames ago.
        if n >= IN_FLIGHT as u64 {
            let done = self.outbox.lock().done;
            if self.sent - done > IN_FLIGHT as u64 {
                // SAFETY: a live session; a numeric time completes the frames up to it.
                let status = unsafe { self.session.0.complete_frames(time(oldest, self.config.timescale)) };
                checked("VTCompressionSessionCompleteFrames", status)?;
            }
        }
        Ok(())
    }

    /// A buffer from the session's pool, for the frame at `at`.
    fn pixel_buffer(&self, at: i64) -> Result<CFRetained<CVPixelBuffer>, VideoError> {
        let mut out: *mut CVPixelBuffer = core::ptr::null_mut();
        // SAFETY: a live pool and a valid place for the buffer, returned +1 (the Create rule).
        let status = unsafe { CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut out)) };
        match NonNull::new(out).filter(|_| status == kCVReturnSuccess) {
            // SAFETY: the +1 reference CVPixelBufferPoolCreatePixelBuffer made.
            Some(buffer) => Ok(unsafe { CFRetained::from_raw(buffer) }),
            None if status == kCVReturnSuccess => Err(VideoError::Frame { time: at, why: "the pool gave no pixel buffer" }),
            None => Err(VideoError::Platform { call: "CVPixelBufferPoolCreatePixelBuffer", status }),
        }
    }

    /// `frame`'s two planes, row by row, into `buffer`'s.
    fn copy_in<A: Allocator>(&self, buffer: &CVPixelBuffer, frame: &Frame<A>) -> Result<(), VideoError> {
        let at = frame.time();
        let (width, height) = frame.size();
        let row = width as usize * frame.format().sample_bytes();
        let rows = [height as usize, height as usize / 2];
        // SAFETY: a live buffer from the pool, locked for writing (flags 0) and unlocked below
        // on every path.
        checked("CVPixelBufferLockBaseAddress", unsafe {
            CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags(0))
        })?;
        let mut copied = Ok(());
        'planes: for (index, (plane, rows)) in frame.planes().iter().zip(rows).enumerate() {
            let base = CVPixelBufferGetBaseAddressOfPlane(buffer, index).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, index);
            if base.is_null() || stride < row || CVPixelBufferGetHeightOfPlane(buffer, index) < rows {
                copied = Err(VideoError::Frame { time: at, why: "the pool's buffer has a plane smaller than the frame's" });
                break;
            }
            for r in 0..rows {
                // Frame::new checked the plane holds every row (stride * (rows - 1) + row bytes,
                // which fits a usize), so this is never out of range; were it, it is an error.
                let start = r * plane.stride;
                let Some(source) = plane.bytes.get(start..start + row) else {
                    copied = Err(VideoError::Frame { time: at, why: "a plane's bytes are fewer than its rows" });
                    break 'planes;
                };
                // SAFETY: `base` is the locked plane, at least `rows` rows of `stride` bytes (just
                // checked), so row `r`'s start is in it.
                let target = unsafe { base.add(r * stride) };
                // SAFETY: row `r`'s first `row` bytes (row <= stride) are in the locked plane and
                // writable; the source is the caller's, which cannot overlap CoreVideo's buffer.
                unsafe { core::ptr::copy_nonoverlapping(source.as_ptr(), target, row) };
            }
        }
        // SAFETY: the lock taken above, with the same flags.
        let unlocked = unsafe { CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags(0)) };
        copied?;
        checked("CVPixelBufferUnlockBaseAddress", unlocked)
    }

    /// The next finished packet, in `alloc`, or `None`; never waits.
    pub(super) fn receive<A: Allocator + Named>(&mut self, alloc: A) -> Result<Option<Packet<A>>, VideoError> {
        let next = self.outbox.lock().take()?;
        let Some(next) = next else { return Ok(None) };
        let Done(sample) = next?;
        if self.hvcc.is_none() {
            self.hvcc = Some(hvcc_of(&sample)?);
        }
        let timescale = self.config.timescale;
        // SAFETY: a live sample buffer; the getter returns a value.
        let pts = unsafe { sample.presentation_time_stamp() };
        // SAFETY: a live sample buffer; the getter returns a value.
        let dts = unsafe { sample.decode_time_stamp() };
        // SAFETY: a live sample buffer; the getter returns a value.
        let duration = unsafe { sample.duration() };
        // SAFETY: a live sample buffer; the binding retains the block buffer it gives.
        let block = unsafe { sample.data_buffer() };
        let pts = ticks(pts, timescale)?.ok_or(VideoError::Missing { what: "the packet's presentation time" })?;
        // No reordering in a stretch: the decode time is the presentation time.
        let dts = ticks(dts, timescale)?.unwrap_or(pts);
        let duration = ticks(duration, timescale)?.unwrap_or(0);
        let duration = u32::try_from(duration)
            .map_err(|_| VideoError::Frame { time: pts, why: "the encoder gave its packet a duration past 2^32 - 1 ticks" })?;
        let block = block.ok_or(VideoError::Missing { what: "the packet's data buffer" })?;
        // SAFETY: a live block buffer.
        let len = unsafe { block.data_length() };
        if len == 0 {
            return Err(VideoError::Missing { what: "the packet's bytes" });
        }
        let out_of_memory = |error: OutOfMemory| VideoError::OutOfMemory { what: "a packet's bytes", asked: error.asked };
        let mut data = mem::Vec::with_capacity_in(len, alloc).map_err(out_of_memory)?;
        let bytes = data.extend_filled(len, 0).map_err(out_of_memory)?;
        // SAFETY: `bytes` is `len` writable bytes, the length the block buffer reports; the copy
        // reads only the block buffer.
        let status = unsafe { block.copy_data_bytes(0, len, NonNull::from(bytes).cast()) };
        checked("CMBlockBufferCopyDataBytes", status)?;
        Ok(Some(Packet { data, pts, dts, duration, timescale, key: is_key(&sample) }))
    }

    /// Every frame sent is finished and called back before this returns.
    pub(super) fn finish(&mut self) -> Result<(), VideoError> {
        // SAFETY: CoreMedia's constant, alive for the process.
        let every = unsafe { kCMTimeInvalid };
        // SAFETY: a live session; an invalid time completes every pending frame.
        let status = unsafe { self.session.0.complete_frames(every) };
        checked("VTCompressionSessionCompleteFrames", status)?;
        self.finished = true;
        Ok(())
    }

    pub(super) fn hvcc(&self) -> Option<&[u8]> {
        self.hvcc.as_deref()
    }
}

/// `buffer`'s colour attachments: the config's primaries, transfer and matrix, chroma sited left.
#[expect(clippy::multiple_unsafe_ops_per_block, reason = "each extern static read is an unsafe op; the attachment keys' constants, one SAFETY")]
fn tag(buffer: &CVPixelBuffer, [primaries, transfer, matrix]: [&'static CFString; 3]) {
    // SAFETY: CoreVideo's documented attachment keys with their documented CFString values, on a
    // live buffer.
    unsafe {
        for (key, value) in [
            (kCVImageBufferColorPrimariesKey, primaries),
            (kCVImageBufferTransferFunctionKey, transfer),
            (kCVImageBufferYCbCrMatrixKey, matrix),
            (kCVImageBufferChromaLocationTopFieldKey, kCVImageBufferChromaLocation_Left),
            (kCVImageBufferChromaLocationBottomFieldKey, kCVImageBufferChromaLocation_Left),
        ] {
            buffer.set_attachment(key, value, CVAttachmentMode::ShouldPropagate);
        }
    }
}

#[expect(clippy::multiple_unsafe_ops_per_block, reason = "each extern static read is an unsafe op; the properties' constants, one SAFETY")]
/// Every property, each checked: one VideoToolbox refuses is an error naming it.
fn configure(
    session: &VTCompressionSession,
    config: &EncoderConfig,
    profile: &'static CFString,
    [primaries, transfer, matrix]: [&'static CFString; 3],
) -> Result<(), VideoError> {
    let set = |call: &'static str, key: &CFString, value: &CFType| {
        // SAFETY: a live session, and each value is the CF type its key documents.
        checked(call, unsafe { VTSessionSetProperty(session, key, Some(value)) })
    };
    // SAFETY: VideoToolbox's string constants, alive for the process.
    unsafe {
        set("VTSessionSetProperty(ProfileLevel)", kVTCompressionPropertyKey_ProfileLevel, profile)?;
        set("VTSessionSetProperty(Quality)", kVTCompressionPropertyKey_Quality, &CFNumber::new_f64(config.quality))?;
        set("VTSessionSetProperty(RealTime)", kVTCompressionPropertyKey_RealTime, CFBoolean::new(false))?;
        set(
            "VTSessionSetProperty(PrioritizeEncodingSpeedOverQuality)",
            kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
            CFBoolean::new(false),
        )?;
        set(
            "VTSessionSetProperty(AllowFrameReordering)",
            kVTCompressionPropertyKey_AllowFrameReordering,
            CFBoolean::new(true),
        )?;
        set(
            "VTSessionSetProperty(MaxKeyFrameInterval)",
            kVTCompressionPropertyKey_MaxKeyFrameInterval,
            &CFNumber::new_i32(config.keyframe_every as i32),
        )?;
        set(
            "VTSessionSetProperty(ExpectedFrameRate)",
            kVTCompressionPropertyKey_ExpectedFrameRate,
            &CFNumber::new_f64(config.fps),
        )?;
        set("VTSessionSetProperty(ColorPrimaries)", kVTCompressionPropertyKey_ColorPrimaries, primaries)?;
        set("VTSessionSetProperty(TransferFunction)", kVTCompressionPropertyKey_TransferFunction, transfer)?;
        set("VTSessionSetProperty(YCbCrMatrix)", kVTCompressionPropertyKey_YCbCrMatrix, matrix)?;
        if let Some(mastering) = config.colour.mastering {
            set(
                "VTSessionSetProperty(MasteringDisplayColorVolume)",
                kVTCompressionPropertyKey_MasteringDisplayColorVolume,
                &CFData::from_bytes(&mastering.to_bytes()),
            )?;
        }
        if let Some(light) = config.colour.light {
            set(
                "VTSessionSetProperty(ContentLightLevelInfo)",
                kVTCompressionPropertyKey_ContentLightLevelInfo,
                &CFData::from_bytes(&light.to_bytes()),
            )?;
        }
        if config.colour.mastering.is_some() || config.colour.light.is_some() {
            set(
                "VTSessionSetProperty(HDRMetadataInsertionMode)",
                kVTCompressionPropertyKey_HDRMetadataInsertionMode,
                kVTHDRMetadataInsertionMode_Auto,
            )?;
        }
    }
    Ok(())
}

/// A CMTime in ticks of `timescale`: `None` when it is not a valid, numeric time; an error when
/// it is in another timescale it does not convert to exactly.
fn ticks(t: CMTime, timescale: u32) -> Result<Option<i64>, VideoError> {
    if !t.flags.contains(CMTimeFlags::Valid) || t.timescale <= 0 {
        return Ok(None);
    }
    if t.timescale as u32 == timescale {
        return Ok(Some(t.value));
    }
    let scaled = i128::from(t.value) * i128::from(timescale);
    let theirs = i128::from(t.timescale);
    if scaled % theirs != 0 {
        return Err(VideoError::Frame { time: t.value, why: "the encoder gave a time not in whole ticks of the frames' timescale" });
    }
    i64::try_from(scaled / theirs)
        .map(Some)
        .map_err(|_| VideoError::Frame { time: t.value, why: "the encoder gave a time past i64 in the frames' timescale" })
}

/// Whether a decoder can start at `sample`: it is a sync sample unless its attachments say
/// `NotSync` is true.
fn is_key(sample: &CMSampleBuffer) -> bool {
    // SAFETY: a live sample buffer; with `false` nothing is created.
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else { return true };
    // SAFETY: CMSampleBufferGetSampleAttachmentsArray's documented content: one CFDictionary of
    // CFString keys a sample.
    let attachments = unsafe { CFRetained::cast_unchecked::<CFArray<CFDictionary<CFString, CFType>>>(attachments) };
    let Some(first) = attachments.get(0) else { return true };
    // SAFETY: CoreMedia's string constant, alive for the process.
    let not_sync = unsafe { kCMSampleAttachmentKey_NotSync };
    match first.get(not_sync).map(|value| value.downcast::<CFBoolean>()) {
        Some(Ok(flag)) => !flag.as_bool(),
        Some(Err(_)) | None => true,
    }
}

/// The `hvcC` atom VideoToolbox put in the sample's format description: the stream's
/// `HEVCDecoderConfigurationRecord`.
fn hvcc_of(sample: &CMSampleBuffer) -> Result<Vec<u8>, VideoError> {
    let missing = VideoError::Missing { what: "the hvcC atom in the first packet's format description" };
    // SAFETY: a live sample buffer; the binding retains what it gives.
    let format = unsafe { sample.format_description() }.ok_or(VideoError::Missing { what: "the first packet's format description" })?;
    // SAFETY: CoreMedia's string constant, alive for the process.
    let key = unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms };
    // SAFETY: a live format description.
    let atoms = unsafe { format.extension(key) }
        .ok_or(missing.clone())?;
    let atoms = atoms.downcast::<CFDictionary>().map_err(|_| missing.clone())?;
    // SAFETY: the documented content of SampleDescriptionExtensionAtoms: atom names (CFString)
    // to their payloads (CFData, or a CFArray of them).
    let atoms = unsafe { CFRetained::cast_unchecked::<CFDictionary<CFString, CFType>>(atoms) };
    let payload = atoms.get(&CFString::from_static_str("hvcC")).ok_or(missing.clone())?;
    let payload = payload.downcast::<CFData>().map_err(|_| missing)?;
    // SAFETY: a CFData the format description owns and nothing mutates while it is read here.
    let bytes = unsafe { payload.as_bytes_unchecked() };
    heap::bytes(bytes) // alloc: the stream's hvcC, once, kept for the muxer's track.
        .map_err(|error| VideoError::OutOfMemory { what: "the stream's hvcC", asked: error.asked })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{Encoder, Layout, Muxer, Plane, Track};

    /// `len` bytes of `value`, as a plane holds them.
    fn filled(len: usize, value: u8) -> mem::Vec<u8, Global> {
        let mut bytes = mem::Vec::new_in(Global);
        bytes.resize(len, value).expect("room for a plane");
        bytes
    }

    /// The provider's whole path on this Mac's media engine: frames in, an `hvcC` and packets
    /// (the first a key frame, decode times rising) out, muxed into an MP4. Needs the hardware
    /// encoder, which every Apple silicon Mac has.
    #[test]
    fn the_media_engine_encodes_frames_into_packets_an_mp4_takes() {
        let gpu = Gpu::offered(true).expect("macOS offers a Gpu to a native world");
        let config = EncoderConfig {
            width: 128,
            height: 64,
            format: PixelFormat::Nv12,
            timescale: 600,
            fps: 30.0,
            quality: 0.5,
            keyframe_every: 4,
            colour: Colour::BT709,
        };
        let mut encoder = Encoder::hardware(&gpu, &config).expect("a hardware HEVC session");
        let mut packets = Vec::new();
        for i in 0..10i64 {
            let shade = (i * 20) as u8;
            let luma = Plane { stride: 128, bytes: filled(128 * 64, shade) };
            let chroma = Plane { stride: 128, bytes: filled(128 * 32, 128) };
            let frame = Frame::new(PixelFormat::Nv12, (128, 64), [luma, chroma], (i * 20, 20, 600)).expect("a frame");
            encoder.send(&frame).expect("the encoder takes the frame");
            while let Some(p) = encoder.receive(std::alloc::Global).expect("a packet") {
                packets.push(p);
            }
        }
        encoder.finish().expect("the encoder finishes");
        while let Some(p) = encoder.receive(std::alloc::Global).expect("a packet") {
            packets.push(p);
        }
        assert_eq!(packets.len(), 10, "one packet a frame");
        assert!(packets[0].key, "the first packet is a key frame");
        assert!(packets.windows(2).all(|w| w[0].dts < w[1].dts), "decode times rise");
        let hvcc = encoder.hvcc().expect("the stream's hvcC");
        assert_eq!(hvcc[0], 1, "hvcC's configurationVersion");
        let track = Track { width: 128, height: 64, timescale: 600, hvcc, colour: Colour::BT709 };
        let mut muxer = Muxer::new(&track, Layout::Progressive, std::alloc::Global).expect("a muxer");
        for p in &packets {
            muxer.push(p).expect("the muxer takes the packet");
        }
        let file = muxer.finish().expect("an mp4");
        assert_eq!(&file[4..8], b"ftyp");
    }
}
