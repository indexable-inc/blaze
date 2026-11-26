//! Hardware-accelerated native macOS image decoder & RGBA8 Metal texture atlas for rendered
//! Markdown/HTML `<picture>` and `<img>` elements (`webp`, `png`, `jpg`, `gif`, `heic`, `svg`).
//!
//! Uses macOS's built-in `ImageIO.framework` (`CGImageSource`) + `CoreGraphics` with zero
//! third-party image crates, unpremultiplying alpha for `blaze-ui`'s straight-alpha `ui_sprite`
//! Metal pipeline.

#[cfg(target_os = "macos")]
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::path::Path;
#[cfg(target_os = "macos")]
use std::ptr::NonNull;

#[cfg(target_os = "macos")]
use blaze_ui::shaders::GlyphInst;

#[cfg(target_os = "macos")]
use crate::view::ImageQuad;

/// Side length of the RGBA8 Metal image atlas texture in device pixels.
pub const IMAGE_ATLAS_SIZE: usize = 2048;

/// Packed rectangle inside the Metal image atlas texture, in device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageSlot {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

/// Probe the natural pixel dimensions `(width, height)` of a local image file without full decode.
#[cfg(target_os = "macos")]
pub fn probe_image_size(path: &Path) -> Option<(u32, u32)> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() || bytes.len() > 16 * 1024 * 1024 {
        return None;
    }
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
    {
        let text = std::str::from_utf8(&bytes).ok()?;
        return parse_svg_dims(text);
    }
    decode_cg_image_dims(&bytes)
}

#[cfg(not(target_os = "macos"))]
pub fn probe_image_size(_path: &std::path::Path) -> Option<(u32, u32)> {
    None
}

#[cfg(target_os = "macos")]
fn parse_svg_dims(svg: &str) -> Option<(u32, u32)> {
    let tag_start = svg.find("<svg")?;
    let tag_end = svg[tag_start..].find('>')? + tag_start;
    let header = &svg[tag_start..tag_end];
    let attr = |name: &str| -> Option<&str> {
        let pat = format!("{name}=\"");
        let i = header.find(&pat)? + pat.len();
        let rest = &header[i..];
        Some(&rest[..rest.find('"')?])
    };
    if let (Some(w), Some(h)) = (attr("width"), attr("height")) {
        let wp = w.trim().trim_end_matches("px").split('.').next()?.parse::<u32>().ok()?;
        let hp = h.trim().trim_end_matches("px").split('.').next()?.parse::<u32>().ok()?;
        if wp > 0 && hp > 0 {
            return Some((wp, hp));
        }
    }
    if let Some(vb) = attr("viewBox") {
        let nums: Vec<f32> = vb
            .split(|c: char| c.is_ascii_whitespace() || c == ',')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<f32>().ok())
            .collect();
        if nums.len() == 4 && nums[2] > 0.0 && nums[3] > 0.0 {
            return Some((nums[2].round() as u32, nums[3].round() as u32));
        }
    }
    None
}

#[cfg(target_os = "macos")]
mod mac_imageio {
    use super::*;

    #[repr(C)]
    pub(super) struct OpaqueCFData(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCGImageSource(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCGImage(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCGColorSpace(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCGContext(c_void);

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct CGRectRaw {
        pub x: f64,
        pub y: f64,
        pub w: f64,
        pub h: f64,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub(super) fn CFDataCreate(
            allocator: *const c_void,
            bytes: *const u8,
            length: isize,
        ) -> *mut OpaqueCFData;
        pub(super) fn CFRelease(cf: *const c_void);
    }

    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        pub(super) fn CGImageSourceCreateWithData(
            data: *const OpaqueCFData,
            options: *const c_void,
        ) -> *mut OpaqueCGImageSource;
        pub(super) fn CGImageSourceCreateImageAtIndex(
            isrc: *const OpaqueCGImageSource,
            index: usize,
            options: *const c_void,
        ) -> *mut OpaqueCGImage;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        pub(super) fn CGImageGetWidth(image: *const OpaqueCGImage) -> usize;
        pub(super) fn CGImageGetHeight(image: *const OpaqueCGImage) -> usize;
        pub(super) fn CGImageRelease(image: *mut OpaqueCGImage);
        pub(super) fn CGColorSpaceCreateDeviceRGB() -> *mut OpaqueCGColorSpace;
        pub(super) fn CGColorSpaceRelease(space: *mut OpaqueCGColorSpace);
        pub(super) fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: *const OpaqueCGColorSpace,
            bitmap_info: u32,
        ) -> *mut OpaqueCGContext;
        pub(super) fn CGContextDrawImage(
            ctx: *mut OpaqueCGContext,
            rect: CGRectRaw,
            image: *const OpaqueCGImage,
        );
        pub(super) fn CGContextSetInterpolationQuality(ctx: *mut OpaqueCGContext, quality: i32);
        pub(super) fn CGContextRelease(ctx: *mut OpaqueCGContext);
    }

    /// `kCGImageAlphaPremultipliedLast` (1) | `kCGBitmapByteOrder32Big` (4 << 12).
    pub(super) const RGBA8_PREMUL_LAST: u32 = 1 | (4 << 12);
    /// `kCGInterpolationHigh` (3).
    pub(super) const INTERP_HIGH: i32 = 3;
}

#[cfg(target_os = "macos")]
fn decode_cg_image_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    use mac_imageio::*;
    // SAFETY: `bytes` is a valid slice for the lifetime of `CFDataCreate`; all CF/CG handles are
    // checked for null and released before returning.
    unsafe {
        let cf_data = CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize);
        if cf_data.is_null() {
            return None;
        }
        let src = CGImageSourceCreateWithData(cf_data, std::ptr::null());
        CFRelease(cf_data.cast());
        if src.is_null() {
            return None;
        }
        let img = CGImageSourceCreateImageAtIndex(src, 0, std::ptr::null());
        CFRelease(src.cast());
        if img.is_null() {
            return None;
        }
        let w = CGImageGetWidth(img) as u32;
        let h = CGImageGetHeight(img) as u32;
        CGImageRelease(img);
        if w > 0 && h > 0 { Some((w, h)) } else { None }
    }
}

#[cfg(target_os = "macos")]
fn decode_rgba_at_size(path: &Path, dst_w: usize, dst_h: usize) -> Option<Vec<u8>> {
    use mac_imageio::*;
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() || bytes.len() > 16 * 1024 * 1024 || dst_w == 0 || dst_h == 0 {
        return None;
    }

    // SAFETY: `bytes` is valid for `CFDataCreate`, `rgba` has exact capacity `dst_w * dst_h * 4`
    // for `CGBitmapContextCreate`, and every CF/CG object is released before returning.
    unsafe {
        let cf_data = CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize);
        if cf_data.is_null() {
            return None;
        }
        let src = CGImageSourceCreateWithData(cf_data, std::ptr::null());
        CFRelease(cf_data.cast());
        if src.is_null() {
            return None;
        }
        let img = CGImageSourceCreateImageAtIndex(src, 0, std::ptr::null());
        CFRelease(src.cast());
        if img.is_null() {
            return None;
        }

        let space = CGColorSpaceCreateDeviceRGB();
        if space.is_null() {
            CGImageRelease(img);
            return None;
        }
        let mut rgba = vec![0u8; dst_w * dst_h * 4];
        let ctx = CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            dst_w,
            dst_h,
            8,
            dst_w * 4,
            space,
            RGBA8_PREMUL_LAST,
        );
        CGColorSpaceRelease(space);
        if ctx.is_null() {
            CGImageRelease(img);
            return None;
        }
        CGContextSetInterpolationQuality(ctx, INTERP_HIGH);
        CGContextDrawImage(
            ctx,
            CGRectRaw { x: 0.0, y: 0.0, w: dst_w as f64, h: dst_h as f64 },
            img,
        );
        CGContextRelease(ctx);
        CGImageRelease(img);

        // Convert premultiplied RGBA -> straight RGBA for `blaze-ui`'s straight-alpha `ui_sprite` shader.
        for px in rgba.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a == 0 {
                px[0] = 0;
                px[1] = 0;
                px[2] = 0;
            } else if a < 255 {
                let half = a / 2;
                px[0] = (((px[0] as u32) * 255 + half) / a).min(255) as u8;
                px[1] = (((px[1] as u32) * 255 + half) / a).min(255) as u8;
                px[2] = (((px[2] as u32) * 255 + half) / a).min(255) as u8;
            }
        }
        let _ = NonNull::new(rgba.as_mut_ptr());
        Some(rgba)
    }
}

/// Shelf-packed GPU RGBA8 image atlas caching decoded Markdown/HTML images (`webp`, `png`, `jpg`, `gif`).
#[cfg(target_os = "macos")]
pub struct ImageAtlas {
    scale: f64,
    slots: HashMap<(String, usize, usize), Option<ImageSlot>>,
    next_x: usize,
    next_y: usize,
    row_h: usize,
    pub pending: Vec<(ImageSlot, Vec<u8>)>,
}

#[cfg(target_os = "macos")]
impl ImageAtlas {
    pub fn new(scale: f64) -> Self {
        Self {
            scale: scale.max(1.0),
            slots: HashMap::new(),
            next_x: 0,
            next_y: 0,
            row_h: 0,
            pending: Vec::new(),
        }
    }

    pub fn set_scale(&mut self, scale: f64) {
        let s = scale.max(1.0);
        if (s - self.scale).abs() > f64::EPSILON {
            self.scale = s;
            self.slots.clear();
            self.next_x = 0;
            self.next_y = 0;
            self.row_h = 0;
            self.pending.clear();
        }
    }

    fn slot(&mut self, path: &str, w_px: usize, h_px: usize) -> Option<ImageSlot> {
        let key = (path.to_owned(), w_px, h_px);
        if let Some(cached) = self.slots.get(&key) {
            return *cached;
        }
        let pad = 2usize;
        if w_px == 0 || h_px == 0 || w_px + pad > IMAGE_ATLAS_SIZE || h_px + pad > IMAGE_ATLAS_SIZE {
            self.slots.insert(key, None);
            return None;
        }
        let Some(rgba) = decode_rgba_at_size(Path::new(path), w_px, h_px) else {
            self.slots.insert(key, None);
            return None;
        };
        if self.next_x + w_px + pad > IMAGE_ATLAS_SIZE {
            self.next_x = 0;
            self.next_y += self.row_h + pad;
            self.row_h = 0;
        }
        if self.next_y + h_px + pad > IMAGE_ATLAS_SIZE {
            // Atlas full: reset shelf cleanly so newly opened documents always render.
            self.slots.clear();
            self.next_x = 0;
            self.next_y = 0;
            self.row_h = 0;
        }
        let slot = ImageSlot { x: self.next_x, y: self.next_y, w: w_px, h: h_px };
        self.next_x += w_px + pad;
        self.row_h = self.row_h.max(h_px);
        self.slots.insert(key, Some(slot));
        self.pending.push((slot, rgba));
        Some(slot)
    }

    /// Convert laid-out [`ImageQuad`]s into GPU [`GlyphInst`] quads sampled from the image atlas.
    pub fn rasterize_quads(&mut self, quads: &[ImageQuad]) -> Vec<GlyphInst> {
        let mut out = Vec::with_capacity(quads.len());
        let inv = 1.0 / IMAGE_ATLAS_SIZE as f32;
        for q in quads {
            let w_px = ((q.w as f64) * self.scale).round().clamp(1.0, 1024.0) as usize;
            let h_px = ((q.h as f64) * self.scale).round().clamp(1.0, 1024.0) as usize;
            let Some(slot) = self.slot(&q.path, w_px, h_px) else {
                continue;
            };
            let u0 = slot.x as f32 * inv;
            let v0 = slot.y as f32 * inv;
            let u1 = (slot.x + slot.w) as f32 * inv;
            let v1 = (slot.y + slot.h) as f32 * inv;
            out.push(GlyphInst {
                rect: [q.x, q.y, q.x + q.w, q.y + q.h],
                uv: [u0, v0, u1, v1],
                color: [1.0, 1.0, 1.0, q.alpha],
                clip_info: [q.clip_id, 0, 0, 0],
            });
        }
        out
    }
}
