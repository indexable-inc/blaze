//! The embedded browser's views (crates/harness/harness-browser, "App ⇄ host: Mach"): per view, the ring of IOSurfaces
//! the host paints into, each wrapped once in an `MTLTexture` with no copy, and which slot a Surface keyed
//! [`key`]`(view)` samples now.
//!
//! The latch. The host sends `Frame { generation, slot }` for its newest picture and never writes into the slot the app
//! latched nor the newest it sent. Media keeps showing the slot it latched until every window frame that sampled it has
//! finished on the GPU (its `sampled` serial at or below `done`); only then does it adopt the newest frame and report the
//! latch (the sink the app sets, which sends `AppUp::Latched`), so the host can never write into a slot a pass in flight
//! still reads. A ring or frame of an older generation than the view's newest ring is dropped: its surfaces are gone.
//!
//! Threading: the main thread only, like the rest of Media (the app's Mach source drains on the main queue).
use std::cell::Cell;
use std::collections::HashMap;
use std::time::Instant;

use harness_mach::iosurface::Surface;
use objc2::encode::{Encoding, RefEncode};
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLPixelFormat, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage};

use super::{BrowserSpec, FORGET_SECS};
use crate::gpu::{Dev, Tex};

/// The bit a browser view's Surface key carries over its view id: no flecs entity id sets it (genui's players use
/// 0x4000_0000_0000_0000), so a view's key never names an element.
pub const BROWSER_BIT: u64 = 0x2000_0000_0000_0000;

/// The Surface key that draws `view`'s frames.
#[must_use]
pub const fn key(view: u64) -> u64 {
    BROWSER_BIT | (view & !BROWSER_BIT)
}

/// The view a Surface key names, when it is a browser view's.
#[must_use]
pub const fn view_of(key: u64) -> Option<u64> {
    if key & BROWSER_BIT != 0 { Some(key & !BROWSER_BIT) } else { None }
}

/// What the app hears when a view's shown slot changes: `(view, generation, slot)`, to send as `AppUp::Latched`.
pub type LatchSink = Box<dyn FnMut(u64, u32, u8)>; // alloc: one for Media's life

/// An IOSurfaceRef as Metal's `newTextureWithDescriptor:iosurface:plane:` declares it (`^{__IOSurface=}`), so objc2's
/// encoding check passes without the objc2-io-surface crate.
#[repr(C)]
struct IOSurfaceOpaque {
    _p: [u8; 0],
}

// SAFETY: IOSurfaceRef is `struct __IOSurface *`, an opaque CF type; this is that struct's encoding.
unsafe impl RefEncode for IOSurfaceOpaque {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("__IOSurface", &[]));
}

/// One ring slot: the texture over the host's surface, and the window frame that last drew it.
struct Slot {
    tex: Tex,
    sampled: Cell<u64>,
    /// Holds the surface while the texture reads it (the texture retains it too; this is the app's own reference).
    _surface: Surface,
}

struct View {
    generation: u32,
    slots: Vec<Slot>,
    /// The slot Surfaces sample now (latched), if any frame has been adopted this generation.
    shown: Option<u8>,
    /// The host's newest frame not yet adopted: its slot and seq.
    pending: Option<(u8, u64)>,
    /// The newest seq adopted or pending, so a late frame never goes back in time.
    seq: u64,
    /// Last frame a Spec named it.
    seen: Instant,
}

pub(crate) struct Browsers {
    dev: Dev,
    views: HashMap<u64, View>,
    latch: Option<LatchSink>,
}

impl Browsers {
    pub(crate) fn new(dev: &Dev) -> Self {
        Self { dev: dev.clone(), views: HashMap::new(), latch: None }
    }

    pub(crate) fn set_latch(&mut self, sink: LatchSink) {
        self.latch = Some(sink);
    }

    /// `view`'s frames now go to `surfaces` (slot order), `w` by `h` pixels, generation `generation`: wrap each once.
    /// A ring older than the view's current one is ignored. The error names the view and the slot.
    pub(crate) fn ring(&mut self, view: u64, generation: u32, w: u32, h: u32, surfaces: Vec<Surface>) -> Result<(), String> {
        if self.views.get(&view).is_some_and(|v| generation < v.generation) {
            return Ok(());
        }
        if surfaces.is_empty() {
            return Err(format!("browser view {view}: ring generation {generation} carried no surfaces"));
        }
        let mut slots = Vec::with_capacity(surfaces.len()); // alloc: one ring's slots, kept until its next generation
        for (i, s) in surfaces.into_iter().enumerate() {
            if s.size() != (w, h) {
                let (sw, sh) = s.size();
                return Err(format!(
                    "browser view {view}: ring generation {generation} slot {i} is {sw}x{sh} pixels, the ring says {w}x{h}"
                ));
            }
            let tex = wrap(&self.dev, &s, w, h)
                .ok_or_else(|| format!("browser view {view}: Metal made no {w}x{h} texture over ring slot {i}'s IOSurface"))?;
            slots.push(Slot { tex, sampled: Cell::new(0), _surface: s });
        }
        // The previous generation's textures may still be bound by a pass in flight: its command buffer retains them.
        self.views.insert(view, View { generation, slots, shown: None, pending: None, seq: 0, seen: Instant::now() });
        Ok(())
    }

    /// Slot `slot` of generation `generation` holds `view`'s newest picture. Dropped when stale (another generation,
    /// an older seq) or out of the ring.
    pub(crate) fn frame(&mut self, view: u64, generation: u32, slot: u8, seq: u64) -> Result<(), String> {
        let Some(v) = self.views.get_mut(&view) else { return Ok(()) };
        if generation != v.generation || seq < v.seq {
            return Ok(());
        }
        if usize::from(slot) >= v.slots.len() {
            return Err(format!(
                "browser view {view}: frame {seq} names slot {slot} of generation {generation}, whose ring has {}",
                v.slots.len()
            ));
        }
        v.seq = seq;
        v.pending = Some((slot, seq));
        Ok(())
    }

    /// The host closed `view`: its ring goes.
    pub(crate) fn forget(&mut self, view: u64) {
        self.views.remove(&view);
    }

    /// Adopt each named view's pending frame once its shown slot is free on the GPU, reporting each latch; forget views no
    /// Spec named for [`FORGET_SECS`].
    pub(crate) fn prepare(&mut self, specs: &[&BrowserSpec], now: Instant, done: u64) {
        for s in specs {
            if let Some(v) = self.views.get_mut(&s.view) {
                v.seen = now;
            }
        }
        for (&view, v) in &mut self.views {
            let Some((slot, _)) = v.pending else { continue };
            let free = v.shown.is_none_or(|s| v.slots[usize::from(s)].sampled.get() <= done);
            if !free {
                continue;
            }
            v.shown = Some(slot);
            v.pending = None;
            if let Some(sink) = self.latch.as_mut() {
                sink(view, v.generation, slot);
            }
        }
        self.views.retain(|_, v| now.duration_since(v.seen).as_secs_f64() < FORGET_SECS);
    }

    /// The slot `key`'s view shows now, marked drawn by window frame `serial`.
    pub(crate) fn front(&self, key: u64, serial: u64) -> Option<&Tex> {
        let v = self.views.get(&view_of(key)?)?;
        let s = &v.slots[usize::from(v.shown?)];
        s.sampled.set(serial);
        Some(&s.tex)
    }

    /// A frame waits for its slot to come free: draw again next frame.
    pub(crate) fn moving(&self) -> bool {
        self.views.values().any(|v| v.pending.is_some())
    }

    /// Whether `view` has a picture to show.
    pub(crate) fn showing(&self, view: u64) -> bool {
        self.views.get(&view).is_some_and(|v| v.shown.is_some())
    }
}

/// A BGRA8 texture over `s` (sampled as a non-sRGB format so the values stay sRGB-encoded, as video frames and every other
/// colour the UI draws are).
fn wrap(dev: &Dev, s: &Surface, w: u32, h: u32) -> Option<Tex> {
    // SAFETY: a plain descriptor factory.
    let d = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm,
            w.max(1) as usize,
            h.max(1) as usize,
            false,
        )
    };
    d.setUsage(MTLTextureUsage::ShaderRead);
    d.setStorageMode(MTLStorageMode::Shared);
    // SAFETY: `s` is a live IOSurfaceRef (held by the Slot for the texture's life as well as by the texture itself);
    // plane 0 of a BGRA8 surface; the selector is in the `new` family, so the result is +1 and Retained takes it.
    let surf = unsafe { &*s.as_ptr().cast::<IOSurfaceOpaque>() };
    let dev: &ProtocolObject<dyn objc2_metal::MTLDevice> = dev;
    let t: Option<Retained<ProtocolObject<dyn MTLTexture>>> =
        unsafe { msg_send![dev, newTextureWithDescriptor: &*d, iosurface: surf, plane: 0usize] };
    t
}
