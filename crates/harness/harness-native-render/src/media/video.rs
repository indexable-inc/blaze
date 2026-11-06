//! Videos: an AVPlayer per element over an AVPlayerItem (an AVURLAsset of a file or an http(s) URL) whose frames an
//! AVPlayerItemVideoOutput hands over as RGBA half-float pixel buffers, mapped into Metal by CoreVideo's texture cache with
//! no copy.
//!
//! Everything is polled from `prepare` (no KVO, no notifications): the item's status, its time and duration, the end (looped
//! by seeking to zero). A frame is pulled only for a player something visible shows; a hidden one keeps its clock (AVPlayer
//! runs on its own) and costs no copies.
//!
//! The frames are RGBA16Float holding the video's own signal, unconverted: an SDR video's Rec. 709 values (its transfer
//! is close to sRGB's, the encoding every other colour the UI draws uses, so it composites like a photo, as before), an
//! HDR video's PQ or HLG code values. AVFoundation's own conversion to linear light clips at 1.0 (measured: a PQ clip's
//! 1000-nit patch came back as 1.0), so it cannot carry HDR; the raw signal can (203 nits reads 0.580, PQ's code for
//! it). Each frame keeps how it is encoded ([`Frame::hdr`], from the pixel buffer's attachments) and the surface pass
//! decodes it (crate::hdr's PQ and HLG, BT.2020 to BT.709) and tone-maps it to the display's headroom.
//!
//! AVPlayer and AVPlayerItem are main-thread-only (objc2 marks them `MainThreadOnly`): players are made only when
//! `prepare` runs on the main thread, and a video asked for elsewhere fails naming why. AVFoundation posts status changes
//! through the main queue, so without a running main run loop (the offscreen shot) the item never becomes ready unless the
//! caller pumps it ([`super::Media::pump`]).
use std::collections::{HashMap, HashSet, VecDeque};
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker};
use objc2_av_foundation::{
    AVPlayer, AVPlayerActionAtItemEnd, AVPlayerItem, AVPlayerItemStatus, AVPlayerItemVideoOutput, AVPlayerStatus,
    AVPlayerTimeControlStatus, AVURLAsset,
};
use objc2_core_foundation::{CFData, CFRetained, CFString};
use objc2_core_media::{CMTime, CMTimeFlags};
use objc2_core_video::{
    CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture, CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_2020, kCVImageBufferContentLightLevelInfoKey,
    kCVImageBufferMasteringDisplayColorVolumeKey, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_2100_HLG,
    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ, kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelFormatType_64RGBAHalf, kCVReturnSuccess,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use objc2_metal::{MTLPixelFormat, MTLTexture};

use super::{FORGET_SECS, VideoSpec, VideoState};
use crate::gpu::{Dev, Tex};
use crate::hdr::{Transfer, WHITE_NITS};

/// Frames a player keeps mapped: the one shown and those the window's frames in flight (gpu.rs FRAMES) still sample. One
/// older than these is let go only once no pass or shader render still reads it.
pub const KEEP_FRAMES: usize = 4;
/// How close to the end (seconds) counts as the end: the last frame's time is a frame short of the duration.
const END_SLACK: f64 = 0.05;
/// The timescale a seek asks in (CMTime's units a second): finer than any frame rate.
const SEEK_TIMESCALE: i32 = 600;

/// One video frame mapped into Metal, alive while anything may sample it.
pub(crate) struct Frame {
    pub(crate) tex: Tex,
    /// How its texels encode light, as `SurfaceInst::hdr`: x the transfer (`Transfer`), y the peak in SDR-white units
    /// from its light-level metadata (0: unknown), z 1 for BT.2020 primaries.
    pub(crate) hdr: [f32; 4],
    /// The window frame that last drew it (Media's serial).
    sampled: std::cell::Cell<u64>,
    /// The CoreVideo texture and its pixel buffer: while these live, AVFoundation does not recycle the buffer `tex` reads.
    _cv: CFRetained<CVMetalTexture>,
    _buf: Retained<CVPixelBuffer>,
}

struct Player {
    src: String,
    player: Retained<AVPlayer>,
    item: Retained<AVPlayerItem>,
    output: Retained<AVPlayerItemVideoOutput>,
    /// Newest last.
    frames: VecDeque<Rc<Frame>>,
    /// Frames pulled so far: a shader that samples it renders again when this moves.
    version: u64,
    /// What was last applied of the spec's play and muted (None: nothing yet), so a change applies once.
    play: Option<bool>,
    muted: Option<bool>,
    looped: bool,
    ended: bool,
    /// A paused video with no frame yet was sought to its start once, so its output has a first frame to hand over.
    prerolled: bool,
    /// Pulls frames this prepare (visible, or sampled by a visible shader).
    pulling: bool,
    error: Option<String>,
}

/// A player, or why there is none (the error names the src).
enum Entry {
    Live(Box<Player>),
    Broken { src: String, error: String },
}

struct Slot {
    entry: Entry,
    seen: Instant,
}

pub(crate) struct Videos {
    dev: Dev,
    /// Made on first use: CoreVideo's Metal texture cache on the Renderer's device (or why it could not be made).
    cache: Option<Result<CFRetained<CVMetalTextureCache>, String>>,
    slots: HashMap<u64, Slot>,
}

/// Seconds of a CMTime, when it is a number (not invalid, indefinite or infinite).
fn secs(t: CMTime) -> Option<f64> {
    // A copy: CMTime is packed(4).
    let flags = { t.flags };
    if !flags.contains(CMTimeFlags::Valid) || flags.intersects(CMTimeFlags::ImpliedValueFlagsMask) {
        return None;
    }
    // SAFETY: CMTimeGetSeconds reads a valid, finite CMTime passed by value.
    let s = unsafe { t.seconds() };
    s.is_finite().then_some(s)
}

const ZERO: CMTime = CMTime { value: 0, timescale: 1, flags: CMTimeFlags::Valid, epoch: 0 };

/// An NSError's words.
fn words(e: Option<Retained<objc2_foundation::NSError>>) -> String {
    e.map_or_else(|| "AVFoundation gave no reason".to_owned(), |e| e.localizedDescription().to_string())
}

/// A CoreFoundation string constant as the NSString it is (toll-free bridged).
fn bridged(s: &CFString) -> &NSString {
    // SAFETY: CFStringRef and NSString* are toll-free bridged: the same object, usable as either.
    unsafe { &*std::ptr::from_ref(s).cast::<NSString>() }
}

impl Player {
    /// Open `src` (an absolute path or an http(s) URL), paused at its start.
    fn open(src: &str, mtm: MainThreadMarker) -> Result<Self, String> {
        let url = if src.starts_with("http://") || src.starts_with("https://") {
            NSURL::URLWithString(&NSString::from_str(src)).ok_or_else(|| format!("{src}: not a URL AVFoundation can read"))?
        } else {
            let meta = std::fs::metadata(src).map_err(|e| format!("{src}: {e}"))?;
            if !meta.is_file() {
                return Err(format!("{src}: not a file"));
            }
            NSURL::fileURLWithPath(&NSString::from_str(src))
        };
        // The output's buffers: RGBA half floats of the unconverted signal (module doc), IOSurface-backed so CoreVideo can
        // map them into Metal.
        // SAFETY: two CoreVideo string constants, alive for the process.
        let (format_key, metal_key) = unsafe { (kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferMetalCompatibilityKey) };
        let format = NSNumber::numberWithUnsignedInt(kCVPixelFormatType_64RGBAHalf);
        let metal = NSNumber::numberWithBool(true);
        let (format, metal): (&AnyObject, &AnyObject) = (&format, &metal);
        let attrs = NSDictionary::<NSString, AnyObject>::from_slices(&[bridged(format_key), bridged(metal_key)], &[format, metal]);
        // SAFETY: AVFoundation's documented constructors and setters with live arguments; the attributes dictionary holds
        // CoreVideo keys with NSNumber values, the types those keys take. The item, output and player are retained here and
        // released (output first taken off the item) by Drop.
        unsafe {
            let asset = AVURLAsset::URLAssetWithURL_options(&url, None);
            let item = AVPlayerItem::playerItemWithAsset(&asset, mtm);
            let output = AVPlayerItemVideoOutput::initWithPixelBufferAttributes(AVPlayerItemVideoOutput::alloc(), Some(&attrs));
            item.addOutput(&output);
            let player = AVPlayer::playerWithPlayerItem(Some(&item), mtm);
            // The end is handled here (loop or stop), polled.
            player.setActionAtItemEnd(AVPlayerActionAtItemEnd::Pause);
            Ok(Self {
                src: src.to_owned(),
                player,
                item,
                output,
                frames: VecDeque::new(),
                version: 0,
                play: None,
                muted: None,
                looped: false,
                ended: false,
                prerolled: false,
                pulling: false,
                error: None,
            })
        }
    }

    fn seek(&self, to: CMTime) {
        // SAFETY: AVPlayer's documented seek, exact (zero tolerance both ways), with valid CMTimes.
        unsafe { self.player.seekToTime_toleranceBefore_toleranceAfter(to, ZERO, ZERO) };
    }

    fn ready(&self) -> bool {
        // SAFETY: AVPlayerItem's status getter.
        (unsafe { self.item.status() }) == AVPlayerItemStatus::ReadyToPlay
    }

    /// Why it cannot play, once AVFoundation says.
    fn failure(&self) -> Option<String> {
        // SAFETY: AVPlayerItem's and AVPlayer's status and error getters.
        unsafe {
            if self.item.status() == AVPlayerItemStatus::Failed {
                return Some(format!("{}: {}", self.src, words(self.item.error())));
            }
            if self.player.status() == AVPlayerStatus::Failed {
                return Some(format!("{}: {}", self.src, words(self.player.error())));
            }
        }
        None
    }

    fn time(&self) -> f64 {
        // SAFETY: AVPlayerItem's currentTime getter.
        secs(unsafe { self.item.currentTime() }).unwrap_or(0.0)
    }

    fn duration(&self) -> Option<f64> {
        // SAFETY: AVPlayerItem's duration getter (indefinite until loaded).
        secs(unsafe { self.item.duration() }).filter(|d| *d > 0.0)
    }

    /// One prepare: apply the spec's changes, note an error, loop or stop at the end, pull a frame when `pull`.
    fn step(&mut self, spec: &VideoSpec, pull: bool, cache: Result<&CVMetalTextureCache, &str>, done: u64) {
        self.pulling = pull;
        if self.error.is_some() {
            return;
        }
        if let Some(why) = self.failure() {
            // SAFETY: AVPlayer's pause.
            unsafe { self.player.pause() };
            self.error = Some(why);
            return;
        }
        self.looped = spec.looped;
        if self.muted != Some(spec.muted) {
            // SAFETY: AVPlayer's muted setter.
            unsafe { self.player.setMuted(spec.muted) };
            self.muted = Some(spec.muted);
        }
        if let Some(s) = spec.seek.filter(|s| s.is_finite()) {
            // SAFETY: CMTimeMakeWithSeconds of a finite number.
            self.seek(unsafe { CMTime::with_seconds(s.max(0.0), SEEK_TIMESCALE) });
            self.ended = false;
        }
        if self.play != Some(spec.play) {
            // SAFETY: AVPlayer's play and pause.
            unsafe {
                if spec.play {
                    if self.ended {
                        self.seek(ZERO);
                        self.ended = false;
                    }
                    self.player.play();
                } else {
                    self.player.pause();
                }
            }
            self.play = Some(spec.play);
        }
        if spec.play
            && !self.ended
            && let Some(end) = self.duration()
            && self.time() >= end - END_SLACK
        {
            if self.looped {
                self.seek(ZERO);
                // SAFETY: AVPlayer's play.
                unsafe { self.player.play() };
            } else {
                // SAFETY: AVPlayer's pause.
                unsafe { self.player.pause() };
                self.ended = true;
            }
        }
        // A paused video shows its first frame: an exact seek to the start makes the output hand one over.
        if !spec.play && self.frames.is_empty() && !self.prerolled && self.ready() {
            self.seek(ZERO);
            self.prerolled = true;
        }
        if pull {
            match cache {
                Ok(cache) => self.pull(cache),
                Err(why) => self.error = Some(format!("{}: {why}", self.src)),
            }
        }
        // Let go of frames past the last KEEP_FRAMES once no window pass (sampled <= done) or shader render (another Rc)
        // still reads them.
        while self.frames.len() > KEEP_FRAMES
            && self.frames.front().is_some_and(|f| f.sampled.get() <= done && Rc::strong_count(f) == 1)
        {
            self.frames.pop_front();
        }
    }

    /// The frame for now, when the output has one newer than the last taken.
    fn pull(&mut self, cache: &CVMetalTextureCache) {
        let host = objc2_quartz_core::CACurrentMediaTime();
        // SAFETY: the video output's documented calls with a valid item time; the copied buffer is ours (Retained releases
        // it), and the out pointer for the display time may be null.
        let buf = unsafe {
            let at = self.output.itemTimeForHostTime(host);
            if !self.output.hasNewPixelBufferForItemTime(at) {
                return;
            }
            self.output.copyPixelBufferForItemTime_itemTimeForDisplay(at, std::ptr::null_mut())
        };
        // A null buffer means nothing is to be shown for that time: keep the last frame.
        let Some(buf) = buf else { return };
        let (w, h) = (CVPixelBufferGetWidth(&buf), CVPixelBufferGetHeight(&buf));
        let mut out: *mut CVMetalTexture = std::ptr::null_mut();
        // SAFETY: a live cache and a live, Metal-compatible RGBA half-float pixel buffer (the output's attributes ask for one), mapped
        // whole as plane 0; `out` is a valid place for the result, which comes with +1 retain count (Create rule).
        let r = unsafe {
            CVMetalTextureCache::create_texture_from_image(
                None,
                cache,
                &buf,
                None,
                MTLPixelFormat::RGBA16Float,
                w,
                h,
                0,
                NonNull::from(&mut out),
            )
        };
        let Some(cv) = NonNull::new(out).filter(|_| r == kCVReturnSuccess) else {
            self.error = Some(format!("{}: CoreVideo could not map a {w}x{h} frame into Metal (CVReturn {r})", self.src));
            return;
        };
        // SAFETY: `cv` is the +1 reference CVMetalTextureCacheCreateTextureFromImage made.
        let cv = unsafe { CFRetained::from_raw(cv) };
        let Some(tex) = CVMetalTextureGetTexture(&cv) else {
            self.error = Some(format!("{}: CoreVideo gave a frame with no Metal texture", self.src));
            return;
        };
        let hdr = encoding(&buf);
        self.frames.push_back(Rc::new(Frame { tex, hdr, sampled: std::cell::Cell::new(0), _cv: cv, _buf: buf }));
        self.version += 1;
    }

    fn state(&self) -> VideoState {
        // SAFETY: AVPlayer's and AVPlayerItem's getters.
        let (control, size) = unsafe { (self.player.timeControlStatus(), self.item.presentationSize()) };
        let natural = (size.width >= 1.0 && size.height >= 1.0).then(|| (size.width.round() as u32, size.height.round() as u32));
        let mapped = self.frames.back().map(|f| (f.tex.width() as u32, f.tex.height() as u32));
        VideoState {
            ready: self.ready(),
            time: self.time(),
            duration: self.duration().unwrap_or(0.0),
            playing: !self.ended && self.error.is_none() && control != AVPlayerTimeControlStatus::Paused,
            ended: self.ended,
            size: natural.or(mapped),
            error: self.error.clone(),
        }
    }

    /// Visible and changing: playing, or still waiting for its first frame.
    fn moving(&self) -> bool {
        if !self.pulling || self.error.is_some() {
            return false;
        }
        // SAFETY: AVPlayer's timeControlStatus getter.
        let control = unsafe { self.player.timeControlStatus() };
        self.frames.is_empty() || (!self.ended && control != AVPlayerTimeControlStatus::Paused)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: AVPlayer's pause; the output is taken off its item before both are released.
        unsafe {
            self.player.pause();
            self.item.removeOutput(&self.output);
        }
    }
}

impl Videos {
    pub(crate) fn new(dev: &Dev) -> Self {
        Self { dev: dev.clone(), cache: None, slots: HashMap::new() }
    }

    /// CoreVideo's Metal texture cache, made once.
    fn cache(&mut self) -> Result<&CVMetalTextureCache, &str> {
        let dev = &self.dev;
        let made = self.cache.get_or_insert_with(|| {
            let mut out: *mut CVMetalTextureCache = std::ptr::null_mut();
            // SAFETY: a live device, no attributes, and a valid place for the result (+1, Create rule).
            let r = unsafe { CVMetalTextureCache::create(None, None, dev, None, NonNull::from(&mut out)) };
            match NonNull::new(out).filter(|_| r == kCVReturnSuccess) {
                // SAFETY: the +1 reference CVMetalTextureCacheCreate made.
                Some(c) => Ok(unsafe { CFRetained::from_raw(c) }),
                None => Err(format!("CoreVideo made no Metal texture cache (CVReturn {r})")),
            }
        });
        match made {
            Ok(c) => Ok(&**c),
            Err(why) => Err(why.as_str()),
        }
    }

    /// Step every player `specs` names (making new ones), pulling frames for the visible and for those in `sampled`; drop
    /// those unasked for [`FORGET_SECS`].
    pub(crate) fn prepare(&mut self, specs: &[&VideoSpec], sampled: &HashSet<u64>, now: Instant, done: u64) {
        let mtm = MainThreadMarker::new();
        if self.cache.is_none() && !specs.is_empty() {
            let _ = self.cache();
        }
        if let Some(Ok(c)) = &self.cache {
            // CoreVideo asks for this periodically: it recycles textures no one holds.
            c.flush(0);
        }
        for spec in specs {
            let stale = self.slots.get(&spec.key).is_none_or(|s| match &s.entry {
                Entry::Live(p) => p.src != spec.src,
                Entry::Broken { src, .. } => *src != spec.src,
            });
            if stale {
                let entry = match mtm {
                    None => Entry::Broken {
                        src: spec.src.clone(),
                        error: format!(
                            "{}: videos play only on the main thread (AVPlayer is main-thread only), and Media::prepare ran on another",
                            spec.src
                        ),
                    },
                    Some(mtm) => match Player::open(&spec.src, mtm) {
                        Ok(p) => Entry::Live(Box::new(p)),
                        Err(error) => Entry::Broken { src: spec.src.clone(), error },
                    },
                };
                self.slots.insert(spec.key, Slot { entry, seen: now });
            }
            let pull = spec.visible || sampled.contains(&spec.key);
            // The cache is borrowed apart from the slot (two fields).
            let cache = match &self.cache {
                Some(Ok(c)) => Ok(&**c),
                Some(Err(why)) => Err(why.as_str()),
                None => Err("CoreVideo's Metal texture cache was not made"),
            };
            let Some(slot) = self.slots.get_mut(&spec.key) else { continue };
            slot.seen = now;
            if let Entry::Live(p) = &mut slot.entry {
                p.step(spec, pull, cache, done);
            }
        }
        // Players unasked for: paused and released (Drop). One whose frames a pass may still sample keeps them alive through
        // its Rc'd frames only if a shader holds one; the window's command buffer retains the textures it binds.
        self.slots.retain(|_, s| now.duration_since(s.seen).as_secs_f64() < FORGET_SECS);
    }

    /// The newest frame of `key`'s video, marked drawn by window frame `serial`.
    pub(crate) fn front(&self, key: u64, serial: u64) -> Option<&Tex> {
        let Entry::Live(p) = &self.slots.get(&key)?.entry else { return None };
        let f = p.frames.back()?;
        f.sampled.set(serial);
        Some(&f.tex)
    }

    /// How the newest frame of `key`'s video encodes light (`Frame::hdr`); None when it has no frame.
    pub(crate) fn encoding(&self, key: u64) -> Option<[f32; 4]> {
        let Entry::Live(p) = &self.slots.get(&key)?.entry else { return None };
        p.frames.back().map(|f| f.hdr)
    }

    /// The newest frame of `key`'s video and how many it has pulled, for a shader that samples it (it holds the Rc while
    /// its render is in flight).
    pub(crate) fn frame(&self, key: u64) -> Option<(Rc<Frame>, u64)> {
        let Entry::Live(p) = &self.slots.get(&key)?.entry else { return None };
        p.frames.back().map(|f| (f.clone(), p.version))
    }

    /// Whether `key` names a video (live or broken).
    pub(crate) fn has(&self, key: u64) -> bool {
        self.slots.contains_key(&key)
    }

    pub(crate) fn state(&self, key: u64) -> Option<VideoState> {
        match &self.slots.get(&key)?.entry {
            Entry::Live(p) => Some(p.state()),
            Entry::Broken { error, .. } => Some(VideoState { error: Some(error.clone()), ..VideoState::default() }),
        }
    }

    pub(crate) fn moving(&self) -> bool {
        self.slots.values().any(|s| matches!(&s.entry, Entry::Live(p) if p.moving()))
    }

    /// Every pulled video has its first frame or an error.
    pub(crate) fn settled(&self) -> bool {
        self.slots.values().all(|s| match &s.entry {
            Entry::Live(p) => !p.pulling || p.error.is_some() || !p.frames.is_empty(),
            Entry::Broken { .. } => true,
        })
    }
}

/// How a pixel buffer's texels encode light (`Frame::hdr`), from the attachments the decoder puts on it: the transfer
/// function and primaries, and the peak from the content light level (MaxCLL) or else the mastering display's maximum
/// luminance. A buffer with neither PQ nor HLG is SDR (x 0), drawn as it is.
fn encoding(buf: &CVPixelBuffer) -> [f32; 4] {
    // SAFETY: CoreVideo's attachment keys and values, static for the process; `attachment` takes a live buffer and a
    // null mode pointer (the mode is not wanted) and returns a +1 reference or None.
    let (transfer, primaries, cll, mastering) = unsafe {
        let get = |key: &CFString| buf.attachment(key, std::ptr::null_mut());
        (
            get(kCVImageBufferTransferFunctionKey),
            get(kCVImageBufferColorPrimariesKey),
            get(kCVImageBufferContentLightLevelInfoKey),
            get(kCVImageBufferMasteringDisplayColorVolumeKey),
        )
    };
    let is = |v: &Option<CFRetained<objc2_core_foundation::CFType>>, want: &CFString| {
        v.as_ref().and_then(|v| v.downcast_ref::<CFString>()).is_some_and(|s| s == want)
    };
    // SAFETY: CoreVideo's static string constants.
    let (pq, hlg, bt2020) = unsafe {
        (kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ, kCVImageBufferTransferFunction_ITU_R_2100_HLG, kCVImageBufferColorPrimaries_ITU_R_2020)
    };
    let transfer = if is(&transfer, pq) {
        Transfer::Pq
    } else if is(&transfer, hlg) {
        Transfer::Hlg
    } else {
        return [0.0; 4];
    };
    let bytes = |v: Option<CFRetained<objc2_core_foundation::CFType>>| v.and_then(|v| v.downcast_ref::<CFData>().map(CFData::to_vec));
    // MaxCLL: two big-endian u16, MaxCLL then MaxFALL, in cd/m² (CTA-861.3). Mastering display colour volume: six u16
    // primaries, two u16 white point, then max and min luminance as big-endian u32 in 0.0001 cd/m² (SMPTE ST 2086).
    let max_cll = bytes(cll).filter(|b| b.len() >= 2).map(|b| f32::from(u16::from_be_bytes([b[0], b[1]])));
    let mastering_max = bytes(mastering)
        .filter(|b| b.len() >= 20)
        .map(|b| u32::from_be_bytes([b[16], b[17], b[18], b[19]]) as f32 * 0.0001);
    let nits = max_cll.filter(|n| *n > 0.0).or(mastering_max.filter(|n| *n > 0.0));
    let peak = nits.map_or(0.0, |n| n / WHITE_NITS);
    [transfer as u32 as f32, peak, if is(&primaries, bt2020) { 1.0 } else { 0.0 }, 0.0]
}
