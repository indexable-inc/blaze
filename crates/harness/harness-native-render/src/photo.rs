//! Photos: a PNG or JPEG file at an absolute path, decoded by CoreGraphics (`CGImageCreateWith{PNG,JPEG}DataProvider`
//! over `CGDataProviderCreateWithFilename`) into straight RGBA, at most [`MAX_EDGE`] texels along its longer edge and, when
//! asked, cut to a shape (centred, as CSS's `object-fit: cover` crops). What a generated UI's `Image` shows; drawn filtered
//! (paint.rs), unlike the pixel art a `Pixelated` picture is.
//!
//! An HDR photo (a PQ or HLG PNG, a JPEG with a gain map) is decoded by ImageIO in its real range when asked
//! ([`decode`]'s `hdr`): ImageIO's `kCGImageSourceDecodeToHDR` gives the extended picture, `CGImageGetContentHeadroom`
//! says whether it is HDR (above 1.0: measured 4.926 for PQ and HLG stills, 1.0 for SDR ones), and it is drawn into a
//! half-float extended linear sRGB bitmap, where 1.0 is SDR white (BT.2408's 203 nits: measured PQ 203 nits to 1.0, 1000
//! nits to 4.926, 4000 nits to 19.70). The content headroom is not the picture's peak, so the peak is the brightest
//! texel; the image pass tone-maps from it to the display's headroom (crate::hdr). An SDR photo takes the 8-bit path
//! either way, unchanged.
use std::path::Path;
use std::rc::Rc;

/// The longest edge a photo is decoded at (texels): a 2x screen's worth of a wide column.
pub const MAX_EDGE: u32 = 1600;

/// A decoded photo: its size in texels and its pixels, rows top first: straight RGBA8 sRGB, or, for an HDR photo (`peak`
/// is Some), premultiplied RGBA half floats of extended linear sRGB (8 bytes a texel, little-endian).
pub struct Photo {
    pub w: u32,
    pub h: u32,
    pub rgba: Rc<[u8]>,
    /// An HDR photo's brightest channel in SDR-white units (1.0 = SDR white); None for an SDR photo.
    pub peak: Option<f32>,
}

/// What kind of picture file `path` holds, by its first bytes: PNG or JPEG, else an error naming what it is not.
fn format(path: &Path) -> Result<bool, String> {
    use std::io::Read;
    let mut head = [0u8; 8];
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let n = f.read(&mut head).map_err(|e| format!("{}: {e}", path.display()))?;
    if n >= 8 && head == *b"\x89PNG\r\n\x1a\n" {
        Ok(true)
    } else if n >= 3 && head[..3] == [0xFF, 0xD8, 0xFF] {
        Ok(false)
    } else {
        Err(format!("{}: not a PNG or JPEG file (the photos an Image shows)", path.display()))
    }
}

#[cfg(target_vendor = "apple")]
fn open(path: &Path) -> Result<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>, String> {
    use objc2_core_graphics::{CGColorRenderingIntent, CGDataProvider, CGImage};
    use std::os::unix::ffi::OsStrExt;
    let png = format(path)?;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| format!("{}: a path with a NUL in it", path.display()))?;
    // SAFETY: `c` is a NUL-terminated path that outlives the call; the provider keeps its own copy.
    let provider = unsafe { CGDataProvider::with_filename(c.as_ptr()) }.ok_or_else(|| format!("{}: CoreGraphics cannot read it", path.display()))?;
    let intent = CGColorRenderingIntent::RenderingIntentDefault;
    // SAFETY: a null decode array is CoreGraphics's documented "no decode mapping".
    let img = unsafe {
        if png {
            CGImage::with_png_data_provider(Some(&provider), std::ptr::null(), true, intent)
        } else {
            CGImage::with_jpeg_data_provider(Some(&provider), std::ptr::null(), true, intent)
        }
    };
    let img = img.ok_or_else(|| format!("{}: not a {} CoreGraphics can decode", path.display(), if png { "PNG" } else { "JPEG" }))?;
    if CGImage::width(Some(&img)) == 0 || CGImage::height(Some(&img)) == 0 {
        return Err(format!("{}: a picture with no pixels", path.display()));
    }
    Ok(img)
}

/// The size of the photo at `path` in pixels, from its header (nothing is decoded).
#[cfg(target_vendor = "apple")]
pub fn size(path: &Path) -> Result<(u32, u32), String> {
    use objc2_core_graphics::CGImage;
    let img = open(path)?;
    let dim = |n: usize| u32::try_from(n).map_err(|_| format!("{}: {n} pixels across", path.display()));
    Ok((dim(CGImage::width(Some(&img)))?, dim(CGImage::height(Some(&img)))?))
}

/// ImageIO's entry points the HDR decode uses (objc2 has no ImageIO crate in this tree's lock): a source over a data
/// provider and its first image, decoded with `kCGImageSourceDecodeRequest: kCGImageSourceDecodeToHDR` (macOS 15).
#[cfg(target_vendor = "apple")]
mod imageio {
    use objc2_core_foundation::{CFDictionary, CFString, CFType};
    use objc2_core_graphics::{CGDataProvider, CGImage};
    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        pub fn CGImageSourceCreateWithDataProvider(provider: &CGDataProvider, options: Option<&CFDictionary>) -> *mut CFType;
        pub fn CGImageSourceCreateImageAtIndex(source: &CFType, index: usize, options: Option<&CFDictionary>) -> *mut CGImage;
        pub static kCGImageSourceDecodeRequest: &'static CFString;
        pub static kCGImageSourceDecodeToHDR: &'static CFString;
    }
}

/// The photo at `path` decoded by ImageIO to its HDR range, when it is HDR (its content headroom is above 1.0); None for
/// an SDR photo.
#[cfg(target_vendor = "apple")]
fn open_hdr(path: &Path) -> Result<Option<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>>, String> {
    use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
    use objc2_core_graphics::{CGDataProvider, CGImage};
    use std::os::unix::ffi::OsStrExt;
    use std::ptr::NonNull;
    format(path)?;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| format!("{}: a path with a NUL in it", path.display()))?;
    // SAFETY: `c` is a NUL-terminated path that outlives the call; the provider keeps its own copy.
    let provider = unsafe { CGDataProvider::with_filename(c.as_ptr()) }.ok_or_else(|| format!("{}: CoreGraphics cannot read it", path.display()))?;
    // SAFETY: ImageIO's static option strings.
    let (key, value) = unsafe { (imageio::kCGImageSourceDecodeRequest, imageio::kCGImageSourceDecodeToHDR) };
    let opts = CFDictionary::<CFString, CFString>::from_slices(&[key], &[value]);
    // SAFETY: a live provider and options dictionary; both calls follow the Create rule (+1 or null), taken by
    // CFRetained::from_raw.
    let img = unsafe {
        let src = NonNull::new(imageio::CGImageSourceCreateWithDataProvider(&provider, None))
            .ok_or_else(|| format!("{}: ImageIO cannot read it", path.display()))?;
        let src = CFRetained::from_raw(src);
        let img = NonNull::new(imageio::CGImageSourceCreateImageAtIndex(&src, 0, Some(opts.as_opaque())))
            .ok_or_else(|| format!("{}: ImageIO cannot decode it", path.display()))?;
        CFRetained::from_raw(img)
    };
    Ok((CGImage::content_headroom(Some(&img)) > 1.0).then_some(img))
}

/// The photo at `path`, cut to the shape `aspect` (width over height; None keeps its own) about its middle, then scaled
/// down to at most [`MAX_EDGE`] along its longer edge. With `hdr`, an HDR photo keeps its range (module doc); without
/// (a shader's input, which samples 8-bit sRGB), every photo is SDR.
#[cfg(target_vendor = "apple")]
pub fn decode(path: &Path, aspect: Option<f32>, hdr: bool) -> Result<Photo, String> {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGInterpolationQuality, kCGColorSpaceExtendedLinearSRGB};
    let extended = if hdr { open_hdr(path)? } else { None };
    let img = match &extended {
        Some(img) => img.clone(),
        None => open(path)?,
    };
    let (w, h) = (CGImage::width(Some(&img)) as f64, CGImage::height(Some(&img)) as f64);
    // The part kept: all of it, or the middle of it in the asked shape.
    let (cw, ch) = match aspect.filter(|a| a.is_finite() && *a > 0.0).map(f64::from) {
        Some(a) if w / h > a => (h * a, h),
        Some(a) => (w, w / a),
        None => (w, h),
    };
    let k = (f64::from(MAX_EDGE) / cw.max(ch)).min(1.0);
    let (ow, oh) = ((cw * k).round().max(1.0) as usize, (ch * k).round().max(1.0) as usize);
    let rect = CGRect { origin: CGPoint { x: -(w - cw) * 0.5 * k, y: -(h - ch) * 0.5 * k }, size: CGSize { width: w * k, height: h * k } };
    if extended.is_some() {
        let mut buf = vec![0u16; ow * oh * 4];
        // SAFETY: a static colour space name.
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceExtendedLinearSRGB }))
            .ok_or("CoreGraphics has no extended linear sRGB colour space")?;
        // Premultiplied last (1) | float components (1 << 8) | 16-bit little-endian (1 << 12): RGBA half floats.
        const HALF_RGBA: u32 = 1 | (1 << 8) | (1 << 12);
        // SAFETY: `buf` is ow*oh*4 halves, 8 bytes a pixel as HALF_RGBA lays them; the context is dropped before the
        // buffer is read.
        let ctx = unsafe { CGBitmapContextCreate(buf.as_mut_ptr().cast(), ow, oh, 16, ow * 8, Some(&space), HALF_RGBA) }
            .ok_or_else(|| format!("{}: CoreGraphics made no {ow}x{oh} half-float bitmap", path.display()))?;
        CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
        CGContext::draw_image(Some(&ctx), rect, Some(&img));
        drop(ctx);
        // The peak: the brightest straight channel of any covered texel.
        let peak = buf
            .chunks_exact(4)
            .filter_map(|px| {
                let a = crate::gpu::half_to_f32(px[3]);
                let m = px[..3].iter().map(|c| crate::gpu::half_to_f32(*c)).fold(0.0f32, f32::max);
                (a > 0.0 && m.is_finite()).then_some(m / a)
            })
            .fold(1.0f32, f32::max);
        let bytes: Vec<u8> = buf.iter().flat_map(|h| h.to_le_bytes()).collect();
        return Ok(Photo { w: ow as u32, h: oh as u32, rgba: bytes.into(), peak: Some(peak) });
    }
    let mut buf = vec![0u8; ow * oh * 4];
    let space = CGColorSpace::new_device_rgb().ok_or("CoreGraphics has no device RGB colour space")?;
    // SAFETY: `buf` is ow*oh*4 bytes, 4 bytes a pixel (RGBA, kCGImageAlphaPremultipliedLast = 1); the context is dropped
    // before the buffer is read.
    let ctx = unsafe { CGBitmapContextCreate(buf.as_mut_ptr().cast(), ow, oh, 8, ow * 4, Some(&space), 1) }
        .ok_or_else(|| format!("{}: CoreGraphics made no {ow}x{oh} bitmap", path.display()))?;
    CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
    CGContext::draw_image(Some(&ctx), rect, Some(&img));
    drop(ctx);
    // CoreGraphics premultiplies; the image shader takes straight colour.
    for px in buf.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Ok(Photo { w: ow as u32, h: oh as u32, rgba: buf.into(), peak: None })
}

#[cfg(not(target_vendor = "apple"))]
pub fn size(path: &Path) -> Result<(u32, u32), String> {
    format(path)?;
    Err(format!("{}: photos are decoded only on Apple systems (CoreGraphics)", path.display()))
}

#[cfg(not(target_vendor = "apple"))]
pub fn decode(path: &Path, _aspect: Option<f32>, _hdr: bool) -> Result<Photo, String> {
    format(path)?;
    Err(format!("{}: photos are decoded only on Apple systems (CoreGraphics)", path.display()))
}
