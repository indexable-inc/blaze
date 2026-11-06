//! Frames for the app: per watched view a ring of three IOSurfaces of the host's own (BGRA8), filled by Metal blits
//! from the IOSurface CEF hands `OnAcceleratedPaint`, plus a select box's popup kept in a texture of its own and blitted
//! over the view at its rect, so the app sees one picture.
//!
//! Which slot the host may write is the sans-IO core in slots.rs: never one the app latched or may still adopt.
//!
//! Lifetimes (the unsafe assumptions):
//! - CEF's IOSurface (`AcceleratedPaintInfo::shared_texture_io_surface`) belongs to CEF's pool and is valid only during
//!   the callback; the texture wrapping it is dropped before the callback returns, and every blit reading it has
//!   completed (`waitUntilCompleted`) by then. Nothing keeps a reference past the call.
//! - A ring slot's IOSurface is ours (+1 from `IOSurfaceCreate`, held by its `Surface`) and its texture holds its own
//!   reference to the surface, so both live as long as the `Slot`. The app holds its own references through the ports
//!   it was sent, so dropping a ring while the app still draws it is safe.
//! - Everything here runs on the UI (main) thread, where the device and queue were made.
use harness_mach::iosurface::Surface;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLOrigin,
    MTLPixelFormat, MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use std::ffi::c_void;

pub type Tex = Retained<ProtocolObject<dyn MTLTexture>>;

pub use crate::slots::SLOTS;
use crate::slots::Slots;

// ---------------------------------------------------------------- IOSurfaceCreate, by hand (four keys)

#[allow(non_upper_case_globals)]
#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    static kIOSurfaceWidth: *const c_void;
    static kIOSurfaceHeight: *const c_void;
    static kIOSurfaceBytesPerElement: *const c_void;
    static kIOSurfacePixelFormat: *const c_void;
    fn IOSurfaceCreate(properties: *const c_void) -> *mut c_void;
    fn IOSurfaceGetWidth(surface: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(surface: *mut c_void) -> usize;
}

#[allow(non_upper_case_globals)]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFDictionaryCreateMutable(alloc: *const c_void, capacity: isize, keys: *const c_void, values: *const c_void) -> *mut c_void;
    fn CFDictionarySetValue(dict: *mut c_void, key: *const c_void, value: *const c_void);
    fn CFNumberCreate(alloc: *const c_void, the_type: isize, value: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

const K_CF_NUMBER_SINT32: isize = 3;
/// 'BGRA' (kCVPixelFormatType_32BGRA).
const BGRA: i32 = 0x4247_5241;

/// A new BGRA8 IOSurface `w` by `h`.
fn create_surface(w: u32, h: u32) -> Result<Surface, String> {
    // SAFETY: CF calls with live CF objects we create and release here; the dictionary retains what it holds.
    unsafe {
        let dict = CFDictionaryCreateMutable(std::ptr::null(), 4, &raw const kCFTypeDictionaryKeyCallBacks, &raw const kCFTypeDictionaryValueCallBacks);
        if dict.is_null() {
            return Err("CFDictionaryCreateMutable returned NULL".into());
        }
        for (key, v) in [(kIOSurfaceWidth, w as i32), (kIOSurfaceHeight, h as i32), (kIOSurfaceBytesPerElement, 4), (kIOSurfacePixelFormat, BGRA)] {
            let n = CFNumberCreate(std::ptr::null(), K_CF_NUMBER_SINT32, (&raw const v).cast());
            CFDictionarySetValue(dict, key, n);
            CFRelease(n);
        }
        let s = IOSurfaceCreate(dict);
        CFRelease(dict);
        if s.is_null() {
            return Err(format!("IOSurfaceCreate({w}x{h} BGRA8) returned NULL"));
        }
        Ok(Surface::from_owned(s))
    }
}

// ---------------------------------------------------------------- the device

/// The host's own Metal device and its one queue.
pub struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
}

/// One copy of a blit: `size` pixels from `src` at `from` to `dst` at `to`.
pub struct Copy<'a> {
    pub src: &'a ProtocolObject<dyn MTLTexture>,
    pub from: (u32, u32),
    pub size: (u32, u32),
    pub dst: &'a ProtocolObject<dyn MTLTexture>,
    pub to: (u32, u32),
}

impl Gpu {
    /// # Errors
    /// No Metal device, or it gives no queue.
    pub fn new() -> Result<Self, String> {
        let device = MTLCreateSystemDefaultDevice().ok_or("MTLCreateSystemDefaultDevice returned nil (no Metal device)")?;
        let queue = device.newCommandQueue().ok_or("the Metal device gave no command queue")?;
        Ok(Self { device, queue })
    }

    fn descriptor(w: u32, h: u32, storage: MTLStorageMode) -> Retained<MTLTextureDescriptor> {
        // SAFETY: a plain 2D descriptor; w and h are checked non-zero by the callers.
        let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::BGRA8Unorm, w as usize, h as usize, false) };
        d.setUsage(MTLTextureUsage::ShaderRead);
        d.setStorageMode(storage);
        d
    }

    /// A texture on IOSurface `surface` (an IOSurfaceRef, BGRA8, `w` by `h`).
    ///
    /// # Safety
    /// `surface` is a live IOSurfaceRef for as long as the texture is used (see the module doc).
    pub unsafe fn wrap(&self, surface: *mut c_void, w: u32, h: u32) -> Result<Tex, String> {
        if surface.is_null() || w == 0 || h == 0 {
            return Err(format!("an IOSurface {w}x{h} at {surface:?} cannot back a texture"));
        }
        // SAFETY: the caller's live IOSurfaceRef, viewed as objc2's opaque IOSurfaceRef type.
        let s: &IOSurfaceRef = unsafe { &*surface.cast::<IOSurfaceRef>() };
        // IOSurface-backed textures are managed storage on macOS (as cef-rs's own importer makes them).
        self.device
            .newTextureWithDescriptor_iosurface_plane(&Self::descriptor(w, h, MTLStorageMode::Managed), s, 0)
            .ok_or_else(|| format!("newTextureWithDescriptor:iosurface:plane: refused an IOSurface {w}x{h}"))
    }

    /// A private texture `w` by `h` (a popup's pixels).
    pub fn private(&self, w: u32, h: u32) -> Result<Tex, String> {
        if w == 0 || h == 0 {
            return Err(format!("a {w}x{h} texture is empty"));
        }
        self.device.newTextureWithDescriptor(&Self::descriptor(w, h, MTLStorageMode::Private)).ok_or_else(|| format!("newTextureWithDescriptor refused a private {w}x{h} texture"))
    }

    /// Run `copies` in one command buffer and wait for it: well under a millisecond, and required, since a CEF surface
    /// among the sources is only valid during the paint callback.
    pub fn blit(&self, copies: &[Copy<'_>]) -> Result<(), String> {
        let cb = self.queue.commandBuffer().ok_or("the Metal queue gave no command buffer")?;
        let enc = cb.blitCommandEncoder().ok_or("the command buffer gave no blit encoder")?;
        for c in copies {
            let (sw, sh) = (c.src.width() as u32, c.src.height() as u32);
            let (dw, dh) = (c.dst.width() as u32, c.dst.height() as u32);
            // Clamp to both textures: a popup partly outside the view, a surface a pixel short of the ring.
            let w = c.size.0.min(sw.saturating_sub(c.from.0)).min(dw.saturating_sub(c.to.0));
            let h = c.size.1.min(sh.saturating_sub(c.from.1)).min(dh.saturating_sub(c.to.1));
            if w == 0 || h == 0 {
                continue;
            }
            // SAFETY: both textures live through the wait below; the region is inside both (clamped above).
            unsafe {
                enc.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                    c.src,
                    0,
                    0,
                    MTLOrigin { x: c.from.0 as usize, y: c.from.1 as usize, z: 0 },
                    MTLSize { width: w as usize, height: h as usize, depth: 1 },
                    c.dst,
                    0,
                    0,
                    MTLOrigin { x: c.to.0 as usize, y: c.to.1 as usize, z: 0 },
                );
            }
        }
        enc.endEncoding();
        cb.commit();
        // Waits for: the GPU to finish this blit (the paint callback's surface must not be read after it returns).
        cb.waitUntilCompleted();
        let status = cb.status();
        if status != MTLCommandBufferStatus::Completed {
            return Err(format!("a ring blit ended in Metal status {status:?}: {:?}", cb.error().map(|e| e.localizedDescription().to_string())));
        }
        Ok(())
    }
}

/// An IOSurface's size in pixels.
///
/// # Safety
/// `surface` is a live IOSurfaceRef.
pub unsafe fn surface_size(surface: *mut c_void) -> (u32, u32) {
    // SAFETY: the caller's.
    unsafe { (IOSurfaceGetWidth(surface) as u32, IOSurfaceGetHeight(surface) as u32) }
}

// ---------------------------------------------------------------- one view's frames

pub struct Slot {
    pub surface: Surface,
    pub tex: Tex,
}

/// A view's ring: three slots of one size, one generation, and which of them the host may write.
pub struct Ring {
    pub w: u32,
    pub h: u32,
    pub slots: Vec<Slot>,
    pub state: Slots,
}

impl Ring {
    /// A new ring `w` by `h`.
    ///
    /// # Errors
    /// A surface or texture that cannot be made, naming the size.
    pub fn new(gpu: &Gpu, generation: u32, w: u32, h: u32) -> Result<Self, String> {
        let mut slots = Vec::with_capacity(SLOTS); // alloc: one ring per watched view per size
        for _ in 0..SLOTS {
            let surface = create_surface(w, h)?;
            // SAFETY: our own surface, held by the Slot beside the texture (the texture retains it too).
            let tex = unsafe { gpu.wrap(surface.as_ptr(), w, h)? };
            slots.push(Slot { surface, tex });
        }
        Ok(Self { w, h, slots, state: Slots::new(generation) })
    }
}

/// A select box's popup: where it is (view pixels) and its newest pixels.
#[derive(Default)]
pub struct Popup {
    pub shown: bool,
    /// x, y, w, h in view pixels.
    pub rect: (i32, i32, u32, u32),
    pub tex: Option<Tex>,
}

/// One view's frame state.
#[derive(Default)]
pub struct Frames {
    pub ring: Option<Ring>,
    /// The generation the next ring takes.
    pub next_generation: u32,
    pub popup: Popup,
    pub seq: u64,
    /// A refused paint was already reported (once per view, not per frame).
    pub reported: bool,
}

impl Frames {
    /// The popup's copy onto a `w` by `h` destination, when shown and painted.
    pub fn popup_copy<'a>(&'a self, dst: &'a ProtocolObject<dyn MTLTexture>) -> Option<Copy<'a>> {
        let p = &self.popup;
        let tex = p.tex.as_deref()?;
        if !p.shown {
            return None;
        }
        let (x, y, w, h) = p.rect;
        // A popup starting left of or above the view: skip its part outside.
        let (fx, tx) = if x < 0 { (x.unsigned_abs(), 0) } else { (0, x as u32) };
        let (fy, ty) = if y < 0 { (y.unsigned_abs(), 0) } else { (0, y as u32) };
        Some(Copy { src: tex, from: (fx, fy), size: (w.saturating_sub(fx), h.saturating_sub(fy)), dst, to: (tx, ty) })
    }
}
