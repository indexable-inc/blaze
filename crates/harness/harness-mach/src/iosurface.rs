//! IOSurfaces across processes: a surface becomes a send right (`IOSurfaceCreateMachPort`) that rides a [`crate::Msg`],
//! and the receiver turns the right back into the same surface (`IOSurfaceLookupFromMachPort`), memory shared, no copy.
//! The surface stays alive while either side holds it; the host makes each ring's surfaces once and the app holds them
//! until the next ring.
use crate::SendRight;
use std::ffi::c_void;

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceCreateMachPort(surface: *mut c_void) -> u32;
    fn IOSurfaceLookupFromMachPort(port: u32) -> *mut c_void;
    fn IOSurfaceGetWidth(surface: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(surface: *mut c_void) -> usize;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

/// An IOSurfaceRef this process holds one reference to (released on drop).
#[derive(Debug)]
pub struct Surface(*mut c_void);

impl Surface {
    /// Hold `raw` (an IOSurfaceRef), taking a reference of our own.
    ///
    /// # Safety
    /// `raw` is a live IOSurfaceRef.
    #[must_use]
    pub unsafe fn retain(raw: *mut c_void) -> Self {
        // SAFETY: the caller's live IOSurfaceRef.
        unsafe { CFRetain(raw) };
        Self(raw)
    }

    /// Take over a reference the caller owns (a Create or Copy function's result).
    ///
    /// # Safety
    /// `raw` is a live IOSurfaceRef whose +1 reference the caller gives up.
    #[must_use]
    pub unsafe fn from_owned(raw: *mut c_void) -> Self {
        Self(raw)
    }

    /// The surface as a send right to put in a message.
    pub fn to_port(&self) -> Result<SendRight, crate::Error> {
        // SAFETY: a live IOSurfaceRef; the returned port is a send right we own.
        let p = unsafe { IOSurfaceCreateMachPort(self.0) };
        if p == 0 {
            return Err(crate::Error("IOSurfaceCreateMachPort returned MACH_PORT_NULL".into()));
        }
        // SAFETY: a send right IOSurfaceCreateMachPort gave us.
        Ok(unsafe { SendRight::from_raw(p) })
    }

    /// The surface a received right names (the right is released after; the surface keeps its own reference).
    pub fn from_port(port: &SendRight) -> Result<Self, crate::Error> {
        // SAFETY: a send right we hold; the result is +1 (Create rule) or NULL.
        let s = unsafe { IOSurfaceLookupFromMachPort(port.raw()) };
        if s.is_null() {
            return Err(crate::Error(format!("IOSurfaceLookupFromMachPort(port {}) found no surface", port.raw())));
        }
        Ok(Self(s))
    }

    /// Its size in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        // SAFETY: a live IOSurfaceRef.
        unsafe { (IOSurfaceGetWidth(self.0) as u32, IOSurfaceGetHeight(self.0) as u32) }
    }

    /// The IOSurfaceRef, for Metal's `newTextureWithDescriptor:iosurface:plane:`; valid while this lives.
    #[must_use]
    pub fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: our one reference.
        unsafe { CFRelease(self.0) };
    }
}
