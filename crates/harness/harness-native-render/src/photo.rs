//! Photos: a PNG or JPEG file at an absolute path, decoded by CoreGraphics (`CGImageCreateWith{PNG,JPEG}DataProvider`
//! over `CGDataProviderCreateWithFilename`) into straight RGBA, at most [`MAX_EDGE`] texels along its longer edge and, when
//! asked, cut to a shape (centred, as CSS's `object-fit: cover` crops). What a generated UI's `Image` shows; drawn filtered
//! (paint.rs), unlike the pixel art a `Pixelated` picture is.
use std::path::Path;
use std::rc::Rc;

/// The longest edge a photo is decoded at (texels): a 2x screen's worth of a wide column.
pub const MAX_EDGE: u32 = 1600;

/// A decoded photo: its size in texels and its pixels, straight RGBA, rows top first.
pub struct Photo {
    pub w: u32,
    pub h: u32,
    pub rgba: Rc<[u8]>,
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

#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
pub fn size(path: &Path) -> Result<(u32, u32), String> {
    use objc2_core_graphics::CGImage;
    let img = open(path)?;
    let dim = |n: usize| u32::try_from(n).map_err(|_| format!("{}: {n} pixels across", path.display()));
    Ok((dim(CGImage::width(Some(&img)))?, dim(CGImage::height(Some(&img)))?))
}

/// The photo at `path`, cut to the shape `aspect` (width over height; None keeps its own) about its middle, then scaled
/// down to at most [`MAX_EDGE`] along its longer edge.
#[cfg(target_os = "macos")]
pub fn decode(path: &Path, aspect: Option<f32>) -> Result<Photo, String> {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGInterpolationQuality};
    let img = open(path)?;
    let (w, h) = (CGImage::width(Some(&img)) as f64, CGImage::height(Some(&img)) as f64);
    // The part kept: all of it, or the middle of it in the asked shape.
    let (cw, ch) = match aspect.filter(|a| a.is_finite() && *a > 0.0).map(f64::from) {
        Some(a) if w / h > a => (h * a, h),
        Some(a) => (w, w / a),
        None => (w, h),
    };
    let k = (f64::from(MAX_EDGE) / cw.max(ch)).min(1.0);
    let (ow, oh) = ((cw * k).round().max(1.0) as usize, (ch * k).round().max(1.0) as usize);
    let mut buf = vec![0u8; ow * oh * 4];
    let space = CGColorSpace::new_device_rgb().ok_or("CoreGraphics has no device RGB colour space")?;
    // SAFETY: `buf` is ow*oh*4 bytes, 4 bytes a pixel (RGBA, kCGImageAlphaPremultipliedLast = 1); the context is dropped
    // before the buffer is read.
    let ctx = unsafe { CGBitmapContextCreate(buf.as_mut_ptr().cast(), ow, oh, 8, ow * 4, Some(&space), 1) }
        .ok_or_else(|| format!("{}: CoreGraphics made no {ow}x{oh} bitmap", path.display()))?;
    CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
    let rect = CGRect { origin: CGPoint { x: -(w - cw) * 0.5 * k, y: -(h - ch) * 0.5 * k }, size: CGSize { width: w * k, height: h * k } };
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
    Ok(Photo { w: ow as u32, h: oh as u32, rgba: buf.into() })
}

#[cfg(not(target_os = "macos"))]
pub fn size(path: &Path) -> Result<(u32, u32), String> {
    format(path)?;
    Err(format!("{}: photos are decoded only on macOS (CoreGraphics)", path.display()))
}

#[cfg(not(target_os = "macos"))]
pub fn decode(path: &Path, _aspect: Option<f32>) -> Result<Photo, String> {
    format(path)?;
    Err(format!("{}: photos are decoded only on macOS (CoreGraphics)", path.display()))
}
