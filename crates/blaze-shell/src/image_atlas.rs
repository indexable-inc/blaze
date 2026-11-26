//! Hardware-accelerated native macOS image decoder & RGBA8 Metal texture atlas for rendered
//! Markdown/HTML `<picture>` and `<img>` elements:
//! - Static & **live multi-frame animated** raster images (`webp`, `avif`, `gif`, `png`, `jpg`,
//!   `heic`) via macOS `ImageIO.framework` (`CGImageSourceGetCount` + per-frame delay timing),
//!   updating the same Metal texture slot in place with zero atlas bloat.
//! - Native Retina **SVG chart & vector** rendering (`.svg` `<style>` CSS rules, `<path>`,
//!   `<line>`, `<rect>`, `<circle>`, `<text>`) via `CoreGraphics` + `CoreText` + `blaze-html`.

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

/// Side length of the RGBA8 Metal image atlas texture in device pixels (`4096×4096` holds 20+
/// simultaneous full-width Retina images without shelf wraparound).
pub const IMAGE_ATLAS_SIZE: usize = 4096;

/// Packed rectangle inside the Metal image atlas texture, in device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageSlot {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

/// Probe the natural pixel dimensions `(width, height)` of a local image file (`webp`, `avif`,
/// `png`, `jpg`, `gif`, `svg`, etc.), cached in memory so layout never re-reads disk per frame.
#[cfg(target_os = "macos")]
pub fn probe_image_size(path: &Path) -> Option<(u32, u32)> {
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    static DIM_CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<(u32, u32)>>>> = OnceLock::new();
    let cache = DIM_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(&cached) = guard.get(path)
    {
        return cached;
    }
    let probed = probe_image_size_uncached(path);
    if let Ok(mut guard) = cache.lock() {
        if guard.len() > 512 {
            guard.clear();
        }
        guard.insert(path.to_path_buf(), probed);
    }
    probed
}

#[cfg(target_os = "macos")]
fn probe_image_size_uncached(path: &Path) -> Option<(u32, u32)> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() || bytes.len() > 32 * 1024 * 1024 {
        return None;
    }
    if is_svg_path(path) {
        let text = std::str::from_utf8(&bytes).ok()?;
        return parse_svg_dims(text);
    }
    if let Some(seq) = crate::avif_dav1d::parse_avif_sequence(&bytes) {
        return Some((seq.width, seq.height));
    }
    decode_cg_image_dims(&bytes)
}

#[cfg(not(target_os = "macos"))]
pub fn probe_image_size(_path: &std::path::Path) -> Option<(u32, u32)> {
    None
}

#[cfg(target_os = "macos")]
fn is_svg_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
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
    pub(super) struct OpaqueCFDictionary(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCFString(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCFNumber(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCGImageSource(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCGImage(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCGColorSpace(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCGContext(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCTFont(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCTLine(c_void);
    #[repr(C)]
    pub(super) struct OpaqueCFAttributedString(c_void);

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
        pub(super) fn CFStringCreateWithBytes(
            alloc: *const c_void,
            bytes: *const u8,
            num_bytes: isize,
            encoding: u32,
            is_external: u8,
        ) -> *mut OpaqueCFString;
        pub(super) fn CFDictionaryGetValue(
            dict: *const OpaqueCFDictionary,
            key: *const c_void,
        ) -> *const c_void;
        pub(super) fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *mut OpaqueCFDictionary;
        pub(super) fn CFNumberGetValue(
            number: *const OpaqueCFNumber,
            the_type: isize,
            value_ptr: *mut c_void,
        ) -> u8;
        pub(super) fn CFAttributedStringCreate(
            alloc: *const c_void,
            string: *const OpaqueCFString,
            attributes: *const OpaqueCFDictionary,
        ) -> *mut OpaqueCFAttributedString;
        pub(super) fn CFRelease(cf: *const c_void);
        pub(super) static kCFTypeDictionaryKeyCallBacks: c_void;
        pub(super) static kCFTypeDictionaryValueCallBacks: c_void;
    }

    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        pub(super) fn CGImageSourceCreateWithData(
            data: *const OpaqueCFData,
            options: *const c_void,
        ) -> *mut OpaqueCGImageSource;
        pub(super) fn CGImageSourceGetCount(isrc: *const OpaqueCGImageSource) -> usize;
        pub(super) fn CGImageSourceCopyPropertiesAtIndex(
            isrc: *const OpaqueCGImageSource,
            index: usize,
            options: *const c_void,
        ) -> *mut OpaqueCFDictionary;
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
        pub(super) fn CGContextSetShouldAntialias(ctx: *mut OpaqueCGContext, antialias: bool);
        pub(super) fn CGContextSaveGState(ctx: *mut OpaqueCGContext);
        pub(super) fn CGContextRestoreGState(ctx: *mut OpaqueCGContext);
        pub(super) fn CGContextSetRGBFillColor(
            ctx: *mut OpaqueCGContext,
            r: f64,
            g: f64,
            b: f64,
            a: f64,
        );
        pub(super) fn CGContextSetRGBStrokeColor(
            ctx: *mut OpaqueCGContext,
            r: f64,
            g: f64,
            b: f64,
            a: f64,
        );
        pub(super) fn CGContextSetLineWidth(ctx: *mut OpaqueCGContext, width: f64);
        pub(super) fn CGContextSetLineCap(ctx: *mut OpaqueCGContext, cap: i32);
        pub(super) fn CGContextSetLineJoin(ctx: *mut OpaqueCGContext, join: i32);
        pub(super) fn CGContextBeginPath(ctx: *mut OpaqueCGContext);
        pub(super) fn CGContextMoveToPoint(ctx: *mut OpaqueCGContext, x: f64, y: f64);
        pub(super) fn CGContextAddLineToPoint(ctx: *mut OpaqueCGContext, x: f64, y: f64);
        pub(super) fn CGContextAddCurveToPoint(
            ctx: *mut OpaqueCGContext,
            cp1x: f64,
            cp1y: f64,
            cp2x: f64,
            cp2y: f64,
            x: f64,
            y: f64,
        );
        pub(super) fn CGContextAddQuadCurveToPoint(
            ctx: *mut OpaqueCGContext,
            cpx: f64,
            cpy: f64,
            x: f64,
            y: f64,
        );
        pub(super) fn CGContextClosePath(ctx: *mut OpaqueCGContext);
        pub(super) fn CGContextAddRect(ctx: *mut OpaqueCGContext, rect: CGRectRaw);
        pub(super) fn CGContextAddEllipseInRect(ctx: *mut OpaqueCGContext, rect: CGRectRaw);
        pub(super) fn CGContextFillPath(ctx: *mut OpaqueCGContext);
        pub(super) fn CGContextStrokePath(ctx: *mut OpaqueCGContext);
        pub(super) fn CGContextSetTextPosition(ctx: *mut OpaqueCGContext, x: f64, y: f64);
        pub(super) fn CGColorCreateGenericRGB(
            r: f64,
            g: f64,
            b: f64,
            a: f64,
        ) -> *mut c_void;
        pub(super) fn CGColorRelease(color: *mut c_void);
        pub(super) fn CGDataProviderCreateWithCFData(
            data: *const OpaqueCFData,
        ) -> *mut c_void;
        pub(super) fn CGDataProviderRelease(provider: *mut c_void);
        pub(super) fn CGFontCreateWithDataProvider(provider: *mut c_void) -> *mut c_void;
        pub(super) fn CGFontRelease(font: *mut c_void);
        pub(super) fn CGContextRelease(ctx: *mut OpaqueCGContext);
    }

    #[link(name = "CoreText", kind = "framework")]
    unsafe extern "C" {
        pub(super) fn CTFontCreateUIFontForLanguage(
            ui_type: u32,
            size: f64,
            language: *const c_void,
        ) -> *mut OpaqueCTFont;
        pub(super) fn CTFontCreateWithGraphicsFont(
            graphics_font: *mut c_void,
            size: f64,
            matrix: *const c_void,
            attributes: *const c_void,
        ) -> *mut OpaqueCTFont;
        pub(super) fn CTFontCreateWithName(
            name: *const OpaqueCFString,
            size: f64,
            matrix: *const c_void,
        ) -> *mut OpaqueCTFont;
        pub(super) fn CTLineCreateWithAttributedString(
            attr_str: *const OpaqueCFAttributedString,
        ) -> *mut OpaqueCTLine;
        pub(super) fn CTLineGetTypographicBounds(
            line: *const OpaqueCTLine,
            ascent: *mut f64,
            descent: *mut f64,
            leading: *mut f64,
        ) -> f64;
        pub(super) fn CTLineDraw(line: *const OpaqueCTLine, ctx: *mut OpaqueCGContext);
        pub(super) fn CTLineGetGlyphRuns(line: *const OpaqueCTLine) -> *const c_void;
        pub(super) fn CTRunGetGlyphCount(run: *const c_void) -> isize;
        pub(super) fn CTRunGetGlyphs(
            run: *const c_void,
            range: objc2_core_foundation::CFRange,
            buffer: *mut u16,
        );
        pub(super) static kCTFontAttributeName: *const c_void;
        pub(super) static kCTForegroundColorAttributeName: *const c_void;
        pub(super) static kCTLigatureAttributeName: *const c_void;
        pub(super) static kCTForegroundColorFromContextAttributeName: *const c_void;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub(super) fn CFNumberCreate(
            allocator: *const c_void,
            the_type: isize,
            value_ptr: *const c_void,
        ) -> *mut OpaqueCFNumber;
        pub(super) fn CFArrayGetCount(arr: *const c_void) -> isize;
        pub(super) fn CFArrayGetValueAtIndex(arr: *const c_void, idx: isize) -> *const c_void;
        pub(super) static kCFBooleanTrue: *const c_void;
    }

    /// `kCFNumberSInt32Type = 3`.
    pub(super) const CF_SINT32: isize = 3;

    /// `kCGImageAlphaPremultipliedLast` (1) | `kCGBitmapByteOrder32Big` (4 << 12).
    pub(super) const RGBA8_PREMUL_LAST: u32 = 1 | (4 << 12);
    /// `kCGInterpolationHigh` (3).
    pub(super) const INTERP_HIGH: i32 = 3;
    /// `kCFStringEncodingUTF8` (0x08000100).
    pub(super) const CF_UTF8: u32 = 0x0800_0100;
    /// `kCFNumberFloat64Type` (13).
    pub(super) const CF_F64: isize = 13;
    /// `kCTFontUIFontSystem` (2), `kCTFontUIFontEmphasizedSystem` (3).
    pub(super) const CT_UI_SYSTEM: u32 = 2;
    pub(super) const CT_UI_BOLD: u32 = 3;

    pub(super) unsafe fn cf_str(s: &str) -> *mut OpaqueCFString {
        // SAFETY: `s` is valid UTF-8 for the duration of `CFStringCreateWithBytes`.
        unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, CF_UTF8, 0) }
    }

    pub(super) fn unpremultiply_rgba(rgba: &mut [u8]) {
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
    }
}

#[cfg(target_os = "macos")]
fn decode_cg_image_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    use mac_imageio::*;
    // SAFETY: `bytes` is valid for `CFDataCreate`; every CF/CG handle is null-checked & released.
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
unsafe fn read_frame_delay_ms(src: *const mac_imageio::OpaqueCGImageSource, idx: usize) -> u64 {
    use mac_imageio::*;
    let dict = unsafe { CGImageSourceCopyPropertiesAtIndex(src, idx, std::ptr::null()) };
    if dict.is_null() {
        return 83;
    }
    let containers = ["{AVIS}", "{WebP}", "{GIF}", "{HEICS}", "{PNG}"];
    let delay_keys = ["UnclampedDelayTime", "DelayTime"];
    let mut result_ms: Option<u64> = None;

    'outer: for cname in containers {
        let ckey = unsafe { cf_str(cname) };
        if ckey.is_null() {
            continue;
        }
        let sub = unsafe { CFDictionaryGetValue(dict, ckey.cast()) };
        unsafe { CFRelease(ckey.cast()) };
        if sub.is_null() {
            continue;
        }
        for dname in delay_keys {
            let dkey = unsafe { cf_str(dname) };
            if dkey.is_null() {
                continue;
            }
            let num = unsafe { CFDictionaryGetValue(sub.cast(), dkey.cast()) };
            unsafe { CFRelease(dkey.cast()) };
            if num.is_null() {
                continue;
            }
            let mut secs = 0.0f64;
            let ok = unsafe { CFNumberGetValue(num.cast(), CF_F64, (&mut secs as *mut f64).cast()) };
            if ok != 0 && secs > 0.005 && secs.is_finite() {
                result_ms = Some((secs * 1000.0).round().clamp(12.0, 5000.0) as u64);
                break 'outer;
            }
        }
    }
    unsafe { CFRelease(dict.cast()) };
    result_ms.unwrap_or(83)
}

#[cfg(target_os = "macos")]
unsafe fn decode_frame_rgba(
    src: *const mac_imageio::OpaqueCGImageSource,
    frame_idx: usize,
    dst_w: usize,
    dst_h: usize,
) -> Option<Vec<u8>> {
    use mac_imageio::*;
    let img = unsafe { CGImageSourceCreateImageAtIndex(src, frame_idx, std::ptr::null()) };
    if img.is_null() {
        return None;
    }
    let space = unsafe { CGColorSpaceCreateDeviceRGB() };
    if space.is_null() {
        unsafe { CGImageRelease(img) };
        return None;
    }
    let mut rgba = vec![0u8; dst_w * dst_h * 4];
    let ctx = unsafe {
        CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            dst_w,
            dst_h,
            8,
            dst_w * 4,
            space,
            RGBA8_PREMUL_LAST,
        )
    };
    unsafe { CGColorSpaceRelease(space) };
    if ctx.is_null() {
        unsafe { CGImageRelease(img) };
        return None;
    }
    unsafe {
        CGContextSetInterpolationQuality(ctx, INTERP_HIGH);
        CGContextDrawImage(
            ctx,
            CGRectRaw { x: 0.0, y: 0.0, w: dst_w as f64, h: dst_h as f64 },
            img,
        );
        CGContextRelease(ctx);
        CGImageRelease(img);
    }
    unpremultiply_rgba(&mut rgba);
    let _ = NonNull::new(rgba.as_mut_ptr());
    Some(rgba)
}

/// Apply an antialiased SDF circular alpha mask to a straight-RGBA8 square bitmap so GitHub
/// user avatars render as crisp Retina circles in Metal without extra shader permutations.
#[cfg(target_os = "macos")]
fn apply_circle_mask_rgba(rgba: &mut [u8], w: usize, h: usize) {
    if w == 0 || h == 0 {
        return;
    }
    let cx = w as f32 * 0.5;
    let cy = h as f32 * 0.5;
    let r = cx.min(cy) - 0.5;
    for y in 0..h {
        let dy = (y as f32 + 0.5) - cy;
        for x in 0..w {
            let dx = (x as f32 + 0.5) - cx;
            let dist = (dx * dx + dy * dy).sqrt();
            let cov = (r + 0.75 - dist).clamp(0.0, 1.0);
            let i = (y * w + x) * 4;
            if i + 3 < rgba.len() {
                rgba[i + 3] = ((rgba[i + 3] as f32) * cov).round() as u8;
            }
        }
    }
}

/// Render an SVG file (`<style>` CSS classes, `<rect>`, `<line>`, `<circle>`, `<path>`, `<text>`)
/// natively via CoreGraphics + CoreText at Retina resolution (`dst_w x dst_h`).
#[cfg(target_os = "macos")]
fn decode_svg_rgba(path: &Path, dst_w: usize, dst_h: usize) -> Option<Vec<u8>> {
    use mac_imageio::*;
    let svg = std::fs::read_to_string(path).ok()?;
    if svg.is_empty() || svg.len() > 2 * 1024 * 1024 || dst_w == 0 || dst_h == 0 {
        return None;
    }
    let (vb_w, vb_h) = parse_svg_dims(&svg)
        .map(|(w, h)| (w as f64, h as f64))
        .unwrap_or((dst_w as f64, dst_h as f64));
    let sx = dst_w as f64 / vb_w.max(1.0);
    let sy = dst_h as f64 / vb_h.max(1.0);

    // 1. Parse `<style>` CSS rules (e.g. `.s0{stroke:#3987e5}`, `text{fill:#f0f6fc}`, `.b{font-weight:600}`).
    let mut css: HashMap<String, HashMap<String, String>> = HashMap::new();
    if let Some(s0) = svg.find("<style")
        && let Some(gt) = svg[s0..].find('>')
        && let Some(end) = svg[s0 + gt + 1..].find("</style>")
    {
        let body = &svg[s0 + gt + 1..s0 + gt + 1 + end];
        for rule in body.split('}') {
            if let Some((sel_raw, decls)) = rule.split_once('{') {
                let mut map = HashMap::new();
                for decl in decls.split(';') {
                    if let Some((k, v)) = decl.split_once(':') {
                        map.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
                    }
                }
                for sel in sel_raw.split(',') {
                    let s = sel.trim();
                    if !s.is_empty() {
                        css.entry(s.to_owned()).or_default().extend(map.clone());
                    }
                }
            }
        }
    }

    let space = unsafe { CGColorSpaceCreateDeviceRGB() };
    if space.is_null() {
        return None;
    }
    let mut rgba = vec![0u8; dst_w * dst_h * 4];
    let ctx = unsafe {
        CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            dst_w,
            dst_h,
            8,
            dst_w * 4,
            space,
            RGBA8_PREMUL_LAST,
        )
    };
    unsafe { CGColorSpaceRelease(space) };
    if ctx.is_null() {
        return None;
    }

    // Subtle dark chart backdrop for transparent benchmark SVGs.
    unsafe {
        CGContextSetShouldAntialias(ctx, true);
        CGContextSetRGBFillColor(ctx, 0x13 as f64 / 255.0, 0x14 as f64 / 255.0, 0x18 as f64 / 255.0, 1.0);
        CGContextAddRect(ctx, CGRectRaw { x: 0.0, y: 0.0, w: dst_w as f64, h: dst_h as f64 });
        CGContextFillPath(ctx);
    }

    let resolve_prop = |tag: &str, attrs: &HashMap<String, String>, prop: &str| -> Option<String> {
        if let Some(v) = attrs.get(prop) {
            return Some(v.clone());
        }
        if let Some(style) = attrs.get("style") {
            for decl in style.split(';') {
                if let Some((k, v)) = decl.split_once(':')
                    && k.trim().eq_ignore_ascii_case(prop)
                {
                    return Some(v.trim().to_owned());
                }
            }
        }
        let mut found = css.get(tag).and_then(|m| m.get(prop)).cloned();
        if let Some(classes) = attrs.get("class") {
            for cls in classes.split_whitespace() {
                let key = format!(".{cls}");
                if let Some(v) = css.get(&key).and_then(|m| m.get(prop)) {
                    found = Some(v.clone());
                }
            }
        }
        found
    };

    let parse_color = |raw: &str| -> Option<[f64; 4]> {
        let s = raw.trim();
        if s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("transparent") {
            return None;
        }
        let hex = s.strip_prefix('#')?;
        match hex.len() {
            3 => {
                let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
                let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
                let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
                Some([r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0, 1.0])
            }
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some([r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0, 1.0])
            }
            8 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
                Some([r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0, a as f64 / 255.0])
            }
            _ => None,
        }
    };

    let mut tok = blaze_html::Tokenizer::new(&svg);
    let alloc = blaze_own::Alloc::global();
    let mut in_text: Option<HashMap<String, String>> = None;
    let mut text_buf = String::new();
    let mut skip_style = false;

    while let Some(token) = tok.next_token(24, alloc) {
        match token {
            blaze_html::Token::StartTag { name, attrs, self_closing } => {
                let tag = name.to_ascii_lowercase();
                if tag == "style" || tag == "title" || tag == "defs" {
                    if !self_closing {
                        skip_style = true;
                    }
                    continue;
                }
                if skip_style {
                    continue;
                }
                let mut amap = HashMap::new();
                for a in attrs.iter() {
                    amap.insert(a.name.to_ascii_lowercase(), a.value.to_owned());
                }
                match tag.as_str() {
                    "text" => {
                        in_text = Some(amap);
                        text_buf.clear();
                    }
                    "rect" => {
                        let rx = amap.get("x").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        let ry = amap.get("y").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        let rw = amap.get("width").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        let rh = amap.get("height").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        if rw > 0.0 && rh > 0.0 {
                            let y_cg = dst_h as f64 - (ry + rh) * sy;
                            let r = CGRectRaw { x: rx * sx, y: y_cg, w: rw * sx, h: rh * sy };
                            if let Some(fc) = resolve_prop("rect", &amap, "fill").as_deref().and_then(parse_color) {
                                unsafe {
                                    CGContextSetRGBFillColor(ctx, fc[0], fc[1], fc[2], fc[3]);
                                    CGContextAddRect(ctx, r);
                                    CGContextFillPath(ctx);
                                }
                            }
                            if let Some(sc) = resolve_prop("rect", &amap, "stroke").as_deref().and_then(parse_color) {
                                let sw = resolve_prop("rect", &amap, "stroke-width")
                                    .and_then(|v| v.parse::<f64>().ok())
                                    .unwrap_or(1.0);
                                unsafe {
                                    CGContextSetRGBStrokeColor(ctx, sc[0], sc[1], sc[2], sc[3]);
                                    CGContextSetLineWidth(ctx, sw * sx);
                                    CGContextAddRect(ctx, r);
                                    CGContextStrokePath(ctx);
                                }
                            }
                        }
                    }
                    "line" => {
                        let x1 = amap.get("x1").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        let y1 = amap.get("y1").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        let x2 = amap.get("x2").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        let y2 = amap.get("y2").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        if let Some(sc) = resolve_prop("line", &amap, "stroke").as_deref().and_then(parse_color) {
                            let sw = resolve_prop("line", &amap, "stroke-width")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(1.0);
                            unsafe {
                                CGContextSetRGBStrokeColor(ctx, sc[0], sc[1], sc[2], sc[3]);
                                CGContextSetLineWidth(ctx, (sw * sx).max(1.0));
                                CGContextBeginPath(ctx);
                                CGContextMoveToPoint(ctx, x1 * sx, dst_h as f64 - y1 * sy);
                                CGContextAddLineToPoint(ctx, x2 * sx, dst_h as f64 - y2 * sy);
                                CGContextStrokePath(ctx);
                            }
                        }
                    }
                    "circle" => {
                        let cx = amap.get("cx").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        let cy = amap.get("cy").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        let rad = amap.get("r").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        if rad > 0.0 {
                            let rect = CGRectRaw {
                                x: (cx - rad) * sx,
                                y: dst_h as f64 - (cy + rad) * sy,
                                w: rad * 2.0 * sx,
                                h: rad * 2.0 * sy,
                            };
                            if let Some(fc) = resolve_prop("circle", &amap, "fill").as_deref().and_then(parse_color) {
                                unsafe {
                                    CGContextSetRGBFillColor(ctx, fc[0], fc[1], fc[2], fc[3]);
                                    CGContextAddEllipseInRect(ctx, rect);
                                    CGContextFillPath(ctx);
                                }
                            }
                            if let Some(sc) = resolve_prop("circle", &amap, "stroke").as_deref().and_then(parse_color) {
                                let sw = resolve_prop("circle", &amap, "stroke-width")
                                    .and_then(|v| v.parse::<f64>().ok())
                                    .unwrap_or(1.0);
                                unsafe {
                                    CGContextSetRGBStrokeColor(ctx, sc[0], sc[1], sc[2], sc[3]);
                                    CGContextSetLineWidth(ctx, sw * sx);
                                    CGContextAddEllipseInRect(ctx, rect);
                                    CGContextStrokePath(ctx);
                                }
                            }
                        }
                    }
                    "path" => {
                        if let Some(d) = amap.get("d") {
                            let fill_c = resolve_prop("path", &amap, "fill")
                                .as_deref()
                                .and_then(parse_color);
                            let stroke_c = resolve_prop("path", &amap, "stroke")
                                .as_deref()
                                .and_then(parse_color);
                            let sw = resolve_prop("path", &amap, "stroke-width")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(1.5);
                            if let Some(fc) = fill_c {
                                unsafe {
                                    CGContextSetRGBFillColor(ctx, fc[0], fc[1], fc[2], fc[3]);
                                    trace_svg_path(ctx, d, sx, sy, dst_h as f64);
                                    CGContextFillPath(ctx);
                                }
                            }
                            if let Some(sc) = stroke_c {
                                unsafe {
                                    CGContextSetRGBStrokeColor(ctx, sc[0], sc[1], sc[2], sc[3]);
                                    CGContextSetLineWidth(ctx, (sw * sx).max(1.0));
                                    CGContextSetLineCap(ctx, 1); // Round
                                    CGContextSetLineJoin(ctx, 1); // Round
                                    trace_svg_path(ctx, d, sx, sy, dst_h as f64);
                                    CGContextStrokePath(ctx);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            blaze_html::Token::Text(t) => {
                if in_text.is_some() && !skip_style {
                    let dec = blaze_html::decode_entities(t, alloc);
                    text_buf.push_str(dec.as_str());
                }
            }
            blaze_html::Token::EndTag { name } => {
                let tag = name.to_ascii_lowercase();
                if tag == "style" || tag == "title" || tag == "defs" {
                    skip_style = false;
                    continue;
                }
                if tag == "text"
                    && let Some(amap) = in_text.take()
                {
                    let label = text_buf.trim();
                    if !label.is_empty() {
                        let tx = amap.get("x").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        let ty = amap.get("y").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                        let fs = resolve_prop("text", &amap, "font-size")
                            .and_then(|v| v.trim_end_matches("px").parse::<f64>().ok())
                            .unwrap_or(12.0);
                        let bold = resolve_prop("text", &amap, "font-weight")
                            .is_some_and(|v| v == "bold" || v.parse::<u32>().is_ok_and(|w| w >= 600));
                        let col = resolve_prop("text", &amap, "fill")
                            .as_deref()
                            .and_then(parse_color)
                            .unwrap_or([0.94, 0.96, 0.99, 1.0]);
                        let anchor = resolve_prop("text", &amap, "text-anchor")
                            .unwrap_or_default();
                        unsafe {
                            draw_svg_text(
                                ctx,
                                label,
                                tx * sx,
                                dst_h as f64 - ty * sy,
                                (fs * sy).max(8.0),
                                bold,
                                col,
                                &anchor,
                            );
                        }
                    }
                    text_buf.clear();
                }
            }
            _ => {}
        }
    }

    unsafe { CGContextRelease(ctx) };
    unpremultiply_rgba(&mut rgba);
    Some(rgba)
}

#[cfg(target_os = "macos")]
unsafe fn draw_svg_text(
    ctx: *mut mac_imageio::OpaqueCGContext,
    text: &str,
    x: f64,
    y_baseline: f64,
    px_size: f64,
    bold: bool,
    col: [f64; 4],
    anchor: &str,
) {
    use mac_imageio::*;
    let font = unsafe {
        CTFontCreateUIFontForLanguage(
            if bold { CT_UI_BOLD } else { CT_UI_SYSTEM },
            px_size,
            std::ptr::null(),
        )
    };
    if font.is_null() {
        return;
    }
    let cg_col = unsafe { CGColorCreateGenericRGB(col[0], col[1], col[2], col[3]) };
    let cf_text = unsafe { cf_str(text) };
    if cg_col.is_null() || cf_text.is_null() {
        if !cg_col.is_null() {
            unsafe { CGColorRelease(cg_col) };
        }
        if !cf_text.is_null() {
            unsafe { CFRelease(cf_text.cast()) };
        }
        unsafe { CFRelease(font.cast()) };
        return;
    }
    let keys: [*const c_void; 2] = unsafe { [kCTFontAttributeName, kCTForegroundColorAttributeName] };
    let vals: [*const c_void; 2] = [font.cast(), cg_col];
    let attrs = unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            vals.as_ptr(),
            2,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    };
    let astr = unsafe { CFAttributedStringCreate(std::ptr::null(), cf_text, attrs) };
    let line = if !astr.is_null() {
        unsafe { CTLineCreateWithAttributedString(astr) }
    } else {
        std::ptr::null_mut()
    };
    if !line.is_null() {
        let mut asc = 0.0;
        let mut desc = 0.0;
        let mut lead = 0.0;
        let w = unsafe { CTLineGetTypographicBounds(line, &mut asc, &mut desc, &mut lead) };
        let draw_x = if anchor.eq_ignore_ascii_case("end") {
            x - w
        } else if anchor.eq_ignore_ascii_case("middle") {
            x - w * 0.5
        } else {
            x
        };
        unsafe {
            CGContextSaveGState(ctx);
            CGContextSetTextPosition(ctx, draw_x, y_baseline);
            CTLineDraw(line, ctx);
            CGContextRestoreGState(ctx);
            CFRelease(line.cast());
        }
    }
    if !astr.is_null() {
        unsafe { CFRelease(astr.cast()) };
    }
    if !attrs.is_null() {
        unsafe { CFRelease(attrs.cast()) };
    }
    unsafe {
        CFRelease(cf_text.cast());
        CGColorRelease(cg_col);
        CFRelease(font.cast());
    }
}

#[cfg(target_os = "macos")]
unsafe fn trace_svg_path(
    ctx: *mut mac_imageio::OpaqueCGContext,
    d: &str,
    sx: f64,
    sy: f64,
    dst_h: f64,
) {
    use mac_imageio::*;
    unsafe { CGContextBeginPath(ctx) };
    let bytes = d.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut cmd = b' ';
    let mut cx = 0.0f64;
    let mut cy = 0.0f64;
    let mut start_x = 0.0f64;
    let mut start_y = 0.0f64;

    let skip_sep = |idx: &mut usize| {
        while *idx < len && (bytes[*idx].is_ascii_whitespace() || bytes[*idx] == b',') {
            *idx += 1;
        }
    };
    let read_num = |idx: &mut usize| -> Option<f64> {
        skip_sep(idx);
        let s0 = *idx;
        if *idx < len && matches!(bytes[*idx], b'+' | b'-') {
            *idx += 1;
        }
        let mut saw_dot = false;
        while *idx < len {
            let b = bytes[*idx];
            if b.is_ascii_digit() {
                *idx += 1;
            } else if b == b'.' && !saw_dot {
                saw_dot = true;
                *idx += 1;
            } else if matches!(b, b'e' | b'E') && *idx > s0 {
                *idx += 1;
                if *idx < len && matches!(bytes[*idx], b'+' | b'-') {
                    *idx += 1;
                }
            } else {
                break;
            }
        }
        if *idx == s0 { None } else { d[s0..*idx].parse::<f64>().ok() }
    };

    while i < len {
        skip_sep(&mut i);
        if i >= len {
            break;
        }
        if bytes[i].is_ascii_alphabetic() {
            cmd = bytes[i];
            i += 1;
        }
        match cmd {
            b'M' | b'm' => {
                let Some(nx) = read_num(&mut i) else { break };
                let Some(ny) = read_num(&mut i) else { break };
                if cmd == b'm' {
                    cx += nx;
                    cy += ny;
                } else {
                    cx = nx;
                    cy = ny;
                }
                start_x = cx;
                start_y = cy;
                unsafe { CGContextMoveToPoint(ctx, cx * sx, dst_h - cy * sy) };
                cmd = if cmd == b'm' { b'l' } else { b'L' };
            }
            b'L' | b'l' => {
                let Some(nx) = read_num(&mut i) else { break };
                let Some(ny) = read_num(&mut i) else { break };
                if cmd == b'l' {
                    cx += nx;
                    cy += ny;
                } else {
                    cx = nx;
                    cy = ny;
                }
                unsafe { CGContextAddLineToPoint(ctx, cx * sx, dst_h - cy * sy) };
            }
            b'H' | b'h' => {
                let Some(nx) = read_num(&mut i) else { break };
                if cmd == b'h' { cx += nx; } else { cx = nx; }
                unsafe { CGContextAddLineToPoint(ctx, cx * sx, dst_h - cy * sy) };
            }
            b'V' | b'v' => {
                let Some(ny) = read_num(&mut i) else { break };
                if cmd == b'v' { cy += ny; } else { cy = ny; }
                unsafe { CGContextAddLineToPoint(ctx, cx * sx, dst_h - cy * sy) };
            }
            b'C' | b'c' => {
                let (Some(x1), Some(y1), Some(x2), Some(y2), Some(x), Some(y)) = (
                    read_num(&mut i),
                    read_num(&mut i),
                    read_num(&mut i),
                    read_num(&mut i),
                    read_num(&mut i),
                    read_num(&mut i),
                ) else {
                    break;
                };
                let (ax1, ay1, ax2, ay2, ax, ay) = if cmd == b'c' {
                    (cx + x1, cy + y1, cx + x2, cy + y2, cx + x, cy + y)
                } else {
                    (x1, y1, x2, y2, x, y)
                };
                cx = ax;
                cy = ay;
                unsafe {
                    CGContextAddCurveToPoint(
                        ctx,
                        ax1 * sx,
                        dst_h - ay1 * sy,
                        ax2 * sx,
                        dst_h - ay2 * sy,
                        ax * sx,
                        dst_h - ay * sy,
                    );
                }
            }
            b'Q' | b'q' => {
                let (Some(x1), Some(y1), Some(x), Some(y)) = (
                    read_num(&mut i),
                    read_num(&mut i),
                    read_num(&mut i),
                    read_num(&mut i),
                ) else {
                    break;
                };
                let (ax1, ay1, ax, ay) = if cmd == b'q' {
                    (cx + x1, cy + y1, cx + x, cy + y)
                } else {
                    (x1, y1, x, y)
                };
                cx = ax;
                cy = ay;
                unsafe {
                    CGContextAddQuadCurveToPoint(ctx, ax1 * sx, dst_h - ay1 * sy, ax * sx, dst_h - ay * sy);
                }
            }
            b'Z' | b'z' => {
                cx = start_x;
                cy = start_y;
                unsafe { CGContextClosePath(ctx) };
            }
            _ => break,
        }
    }
}

#[cfg(target_os = "macos")]
enum AnimBackend {
    /// Stateful 60fps AVIF sequence (`avis`) decoded sequentially via macOS WebCore `dav1d`
    /// on a dedicated background thread with recycled zero-allocation RGBA8 buffers!
    AvifDav1d(crate::avif_dav1d::AvifStreamer),
    /// Compact animations (e.g. 32-frame Blaze fire WebP): lazily decoded once into RAM cache.
    Cached {
        src: *mut mac_imageio::OpaqueCGImageSource,
        frames: Vec<Option<Vec<u8>>>,
    },
    /// Fallback background streamer for large non-AVIF multi-frame animations (e.g. huge GIFs).
    Streamed {
        req_tx: std::sync::mpsc::SyncSender<usize>,
        frame_rx: std::sync::mpsc::Receiver<(usize, Vec<u8>)>,
        in_flight: Option<usize>,
    },
}

#[cfg(target_os = "macos")]
impl Drop for AnimBackend {
    fn drop(&mut self) {
        if let AnimBackend::Cached { src, .. } = self
            && !src.is_null()
        {
            // SAFETY: owned `CGImageSource` handle created by `CGImageSourceCreateWithData`.
            unsafe { mac_imageio::CFRelease(src.cast()) };
            *src = std::ptr::null_mut();
        }
    }
}

#[cfg(target_os = "macos")]
struct AnimatedSource {
    frame_ends_ms: Vec<u64>,
    total_ms: u64,
    last_frame: usize,
    backend: AnimBackend,
}

#[cfg(target_os = "macos")]
struct SendImageSource(*mut mac_imageio::OpaqueCGImageSource);

// SAFETY: `CGImageSourceRef` is exclusively owned and accessed by the single background decoder
// thread until it exits and calls `CFRelease`.
#[cfg(target_os = "macos")]
unsafe impl Send for SendImageSource {}

#[cfg(target_os = "macos")]
impl Drop for SendImageSource {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { mac_imageio::CFRelease(self.0.cast()) };
            self.0 = std::ptr::null_mut();
        }
    }
}

#[cfg(target_os = "macos")]
struct AtlasEntry {
    slot: ImageSlot,
    anim: Option<AnimatedSource>,
}

/// Shelf-packed GPU RGBA8 image atlas supporting static & live animated (`webp`, `avif`, `gif`,
/// `png`, `jpg`, `svg`) Markdown/HTML images.
///
/// Keyed strictly by file path (not pixel size!) so live window resizes NEVER re-read files,
/// re-decode images, or restart animation workers — Metal's bilinear sampler scales quads at 120fps.
#[cfg(target_os = "macos")]
pub struct ImageAtlas {
    scale: f64,
    entries: HashMap<String, Option<AtlasEntry>>,
    next_x: usize,
    next_y: usize,
    row_h: usize,
    pub pending: Vec<(ImageSlot, Vec<u8>)>,
    pending_recycle: Vec<(String, ImageSlot, Vec<u8>)>,
}

#[cfg(target_os = "macos")]
impl ImageAtlas {
    pub fn new(scale: f64) -> Self {
        Self {
            scale: scale.max(1.0),
            entries: HashMap::new(),
            next_x: 0,
            next_y: 0,
            row_h: 0,
            pending: Vec::new(),
            pending_recycle: Vec::new(),
        }
    }

    pub fn set_scale(&mut self, scale: f64) {
        let s = scale.max(1.0);
        if (s - self.scale).abs() > f64::EPSILON {
            self.scale = s;
            self.entries.clear();
            self.next_x = 0;
            self.next_y = 0;
            self.row_h = 0;
            self.pending.clear();
            self.pending_recycle.clear();
        }
    }

    /// Upload all pending frames via `upload` and recycle streamed RGBA8 buffers back to their
    /// background decoders (zero per-frame heap allocation!).
    pub fn drain_pending_into(&mut self, mut upload: impl FnMut(ImageSlot, &[u8])) {
        for (slot, bytes) in self.pending.drain(..) {
            upload(slot, &bytes);
        }
        for (path, slot, bytes) in self.pending_recycle.drain(..) {
            upload(slot, &bytes);
            if let Some(Some(entry)) = self.entries.get(&path)
                && let Some(anim) = entry.anim.as_ref()
                && let AnimBackend::AvifDav1d(ref streamer) = anim.backend
            {
                streamer.recycle(bytes);
            }
        }
    }

    /// Fast O(N_loaded) check whether ANY loaded animated image actually has a new frame ready
    /// to present at `now_ms` (avoids needless full-scene redraws between animation frames!).
    pub fn has_ready_animation_frame(&self, now_ms: u64) -> bool {
        for entry in self.entries.values().flatten() {
            let Some(anim) = entry.anim.as_ref() else {
                continue;
            };
            match &anim.backend {
                AnimBackend::AvifDav1d(streamer) => {
                    if streamer.has_ready_frame() {
                        return true;
                    }
                }
                AnimBackend::Cached { .. } | AnimBackend::Streamed { .. } => {
                    if anim.total_ms > 0 && !anim.frame_ends_ms.is_empty() {
                        let t = now_ms % anim.total_ms;
                        let frame_idx = anim
                            .frame_ends_ms
                            .partition_point(|&end| end <= t)
                            .min(anim.frame_ends_ms.len() - 1);
                        if frame_idx != anim.last_frame {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    fn alloc_slot(&mut self, w_px: usize, h_px: usize) -> Option<ImageSlot> {
        let pad = 2usize;
        if w_px == 0 || h_px == 0 || w_px + pad > IMAGE_ATLAS_SIZE || h_px + pad > IMAGE_ATLAS_SIZE {
            return None;
        }
        if self.next_x + w_px + pad > IMAGE_ATLAS_SIZE {
            self.next_x = 0;
            self.next_y += self.row_h + pad;
            self.row_h = 0;
        }
        if self.next_y + h_px + pad > IMAGE_ATLAS_SIZE {
            self.entries.clear();
            self.next_x = 0;
            self.next_y = 0;
            self.row_h = 0;
        }
        let slot = ImageSlot { x: self.next_x, y: self.next_y, w: w_px, h: h_px };
        self.next_x += w_px + pad;
        self.row_h = self.row_h.max(h_px);
        Some(slot)
    }

    fn choose_decode_dims(&self, path: &Path, req_w: usize, req_h: usize, max_w: usize, max_h: usize) -> (usize, usize) {
        if let Some((nw, nh)) = probe_image_size(path)
            && nw > 0
            && nh > 0
        {
            let (mut w, mut h) = (nw as f64, nh as f64);
            // SVGs scale cleanly to crisp Retina resolution.
            if is_svg_path(path) {
                w = (w * self.scale).min(max_w as f64);
                h = (h * self.scale).min(max_h as f64);
            }
            if w > max_w as f64 {
                h *= max_w as f64 / w;
                w = max_w as f64;
            }
            if h > max_h as f64 {
                w *= max_h as f64 / h;
                h = max_h as f64;
            }
            return (w.round().clamp(1.0, max_w as f64) as usize, h.round().clamp(1.0, max_h as f64) as usize);
        }
        (req_w.clamp(1, max_w), req_h.clamp(1, max_h))
    }

    fn ensure_entry(
        &mut self,
        path: &str,
        w_px: usize,
        h_px: usize,
        circle: bool,
        now_ms: u64,
    ) -> Option<(ImageSlot, bool)> {
        let cache_key = if circle {
            format!("circle:{path}")
        } else {
            path.to_owned()
        };
        if !self.entries.contains_key(&cache_key) {
            let loaded = self.load_new_entry(Path::new(path), w_px, h_px, circle);
            self.entries.insert(cache_key.clone(), loaded);
        }
        let entry = self.entries.get_mut(&cache_key)?.as_mut()?;
        let slot = entry.slot;
        let mut is_animated = false;

        if let Some(anim) = entry.anim.as_mut() {
            is_animated = true;
            match &mut anim.backend {
                AnimBackend::AvifDav1d(streamer) => {
                    if let Some((got_idx, bytes)) = streamer.poll_visible() {
                        self.pending_recycle.push((cache_key.clone(), slot, bytes));
                        anim.last_frame = got_idx;
                    }
                }
                AnimBackend::Cached { src, frames } => {
                    if anim.total_ms > 0 && !anim.frame_ends_ms.is_empty() {
                        let t = now_ms % anim.total_ms;
                        let frame_idx = anim
                            .frame_ends_ms
                            .partition_point(|&end| end <= t)
                            .min(anim.frame_ends_ms.len() - 1);
                        if frame_idx != anim.last_frame {
                            let rgba = if let Some(cached) = frames[frame_idx].as_ref() {
                                Some(cached.clone())
                            } else {
                                let decoded = unsafe { decode_frame_rgba(*src, frame_idx, slot.w, slot.h) };
                                if let Some(ref bytes) = decoded {
                                    frames[frame_idx] = Some(bytes.clone());
                                }
                                decoded
                            };
                            if let Some(bytes) = rgba {
                                self.pending.push((slot, bytes));
                                anim.last_frame = frame_idx;
                            }
                        }
                    }
                }
                AnimBackend::Streamed { req_tx, frame_rx, in_flight } => {
                    if anim.total_ms > 0 && !anim.frame_ends_ms.is_empty() {
                        let t = now_ms % anim.total_ms;
                        let frame_idx = anim
                            .frame_ends_ms
                            .partition_point(|&end| end <= t)
                            .min(anim.frame_ends_ms.len() - 1);
                        let mut newest: Option<(usize, Vec<u8>)> = None;
                        while let Ok(done) = frame_rx.try_recv() {
                            if *in_flight == Some(done.0) {
                                *in_flight = None;
                            }
                            newest = Some(done);
                        }
                        if let Some((got_idx, bytes)) = newest {
                            self.pending.push((slot, bytes));
                            anim.last_frame = got_idx;
                        }
                        if frame_idx != anim.last_frame && *in_flight != Some(frame_idx) {
                            if req_tx.try_send(frame_idx).is_ok() {
                                *in_flight = Some(frame_idx);
                            }
                        }
                    }
                }
            }
        }
        Some((slot, is_animated))
    }

    fn load_new_entry(
        &mut self,
        path: &Path,
        req_w: usize,
        req_h: usize,
        circle: bool,
    ) -> Option<AtlasEntry> {
        if is_svg_path(path) {
            let (dw, dh) = self.choose_decode_dims(path, req_w, req_h, 1600, 1100);
            let rgba = decode_svg_rgba(path, dw, dh)?;
            let slot = self.alloc_slot(dw, dh)?;
            self.pending.push((slot, rgba));
            return Some(AtlasEntry { slot, anim: None });
        }

        let bytes = std::fs::read(path).ok()?;
        if bytes.is_empty() || bytes.len() > 32 * 1024 * 1024 {
            return None;
        }

        // Fast-path: multi-frame AVIF image sequences (`avis`) decoded sequentially at 60fps via
        // macOS WebCore's built-in `dav1d` (~1.2 ms/frame vs ImageIO's O(N^2) 140-400 ms/frame!).
        if let Some(seq) = crate::avif_dav1d::parse_avif_sequence(&bytes) {
            let (mut dw, mut dh) = (seq.width as f64, seq.height as f64);
            let max_w = 1280.0f64;
            let max_h = 720.0f64;
            if dw > max_w {
                dh *= max_w / dw;
                dw = max_w;
            }
            if dh > max_h {
                dw *= max_h / dh;
                dh = max_h;
            }
            let w_px = dw.round().clamp(1.0, max_w) as usize;
            let h_px = dh.round().clamp(1.0, max_h) as usize;
            if let Some((first_rgba, streamer)) =
                crate::avif_dav1d::AvifStreamer::start(bytes.clone(), seq, w_px, h_px)
                && let Some(slot) = self.alloc_slot(w_px, h_px)
            {
                self.pending.push((slot, first_rgba));
                return Some(AtlasEntry {
                    slot,
                    anim: Some(AnimatedSource {
                        frame_ends_ms: Vec::new(),
                        total_ms: 0,
                        last_frame: 0,
                        backend: AnimBackend::AvifDav1d(streamer),
                    }),
                });
            }
        }

        let (w_px, h_px) = if circle {
            let side = req_w.max(req_h).clamp(16, 128);
            (side, side)
        } else {
            self.choose_decode_dims(path, req_w, req_h, 1440, 960)
        };

        use mac_imageio::*;
        // SAFETY: `bytes` lives through `CFDataCreate`; ownership of `src` is either moved into
        // `AnimatedSource` (which frees it on Drop) or released before returning.
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
            let count = CGImageSourceGetCount(src);
            if count == 0 {
                CFRelease(src.cast());
                return None;
            }
            let mut first_rgba = match decode_frame_rgba(src, 0, w_px, h_px) {
                Some(b) => b,
                None => {
                    CFRelease(src.cast());
                    return None;
                }
            };
            if circle {
                apply_circle_mask_rgba(&mut first_rgba, w_px, h_px);
            }
            let Some(slot) = self.alloc_slot(w_px, h_px) else {
                CFRelease(src.cast());
                return None;
            };
            self.pending.push((slot, first_rgba.clone()));

            if count == 1 || circle {
                CFRelease(src.cast());
                return Some(AtlasEntry { slot, anim: None });
            }

            // Multi-frame WebP/GIF animation (`header-dark.webp`, animated GIFs).
            let capped = count.min(600);
            let mut frame_ends_ms = Vec::with_capacity(capped);
            let mut total_ms = 0u64;
            for i in 0..capped {
                total_ms = total_ms.saturating_add(read_frame_delay_ms(src, i));
                frame_ends_ms.push(total_ms);
            }
            let cache_enabled = capped <= 48 && w_px * h_px <= 1024 * 600;
            let backend = if cache_enabled {
                let mut frames = vec![None; capped];
                if !frames.is_empty() {
                    frames[0] = Some(first_rgba);
                }
                AnimBackend::Cached { src, frames }
            } else {
                let (req_tx, req_rx) = std::sync::mpsc::sync_channel::<usize>(2);
                let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<(usize, Vec<u8>)>(2);
                let owned_src = SendImageSource(src);
                let dw = slot.w;
                let dh = slot.h;
                let _ = std::thread::Builder::new()
                    .name("blaze-shell-anim".to_owned())
                    .spawn(move || {
                        let guard = owned_src;
                        while let Ok(mut target_idx) = req_rx.recv() {
                            while let Ok(newer) = req_rx.try_recv() {
                                target_idx = newer;
                            }
                            if let Some(rgba) = decode_frame_rgba(guard.0, target_idx, dw, dh) {
                                if frame_tx.send((target_idx, rgba)).is_err() {
                                    break;
                                }
                            }
                        }
                    });
                AnimBackend::Streamed { req_tx, frame_rx, in_flight: None }
            };
            Some(AtlasEntry {
                slot,
                anim: Some(AnimatedSource {
                    frame_ends_ms,
                    total_ms,
                    last_frame: 0,
                    backend,
                }),
            })
        }
    }

    /// Convert visible [`ImageQuad`]s into GPU [`GlyphInst`] quads, culling offscreen images so
    /// scrolled-away animations (`docs/demo.avif`, etc.) do zero decoding & zero GPU upload!
    pub fn rasterize_quads(
        &mut self,
        quads: &[ImageQuad],
        now_ms: u64,
        viewport_h: f32,
    ) -> (Vec<GlyphInst>, bool) {
        let mut out = Vec::with_capacity(quads.len());
        let mut any_anim = false;
        let inv = 1.0 / IMAGE_ATLAS_SIZE as f32;
        for q in quads {
            // Viewport cull: never decode or upload offscreen images/animations!
            if q.y + q.h < -32.0 || q.y > viewport_h + 32.0 {
                continue;
            }
            let w_px = ((q.w as f64) * self.scale).round().clamp(1.0, 1200.0) as usize;
            let h_px = ((q.h as f64) * self.scale).round().clamp(1.0, 840.0) as usize;
            let Some((slot, is_anim)) = self.ensure_entry(&q.path, w_px, h_px, q.circle, now_ms) else {
                continue;
            };
            if is_anim {
                any_anim = true;
            }
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
        (out, any_anim)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn decodes_animated_webp_and_avif_and_svg_chart() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let fire = root.join("docs/brand/fire/header-dark.webp");
        let life = root.join("docs/demo.avif");
        let chart = root.join("docs/benchmarks/charts/trend-macos-arm64-live-dark.svg");

        let mut atlas = ImageAtlas::new(2.0);
        if fire.is_file() {
            let q = ImageQuad {
                x: 0.0,
                y: 0.0,
                w: 344.0,
                h: 184.0,
                path: fire.to_string_lossy().into_owned(),
                alpha: 1.0,
                circle: false,
                clip_id: 0,
            };
            let (insts, anim) = atlas.rasterize_quads(std::slice::from_ref(&q), 0, 1000.0);
            assert_eq!(insts.len(), 1);
            assert!(anim, "header-dark.webp must be recognized as animated");
            atlas.pending.clear();
            let (_, _) = atlas.rasterize_quads(std::slice::from_ref(&q), 400, 1000.0);
            assert!(!atlas.pending.is_empty(), "advancing timestamp must upload next animation frame");
        }
        if life.is_file() {
            let dims = probe_image_size(&life);
            assert!(dims.is_some(), "docs/demo.avif dimensions must probe cleanly");
            let q = ImageQuad {
                x: 0.0,
                y: 0.0,
                w: 480.0,
                h: 300.0,
                path: life.to_string_lossy().into_owned(),
                alpha: 1.0,
                circle: false,
                clip_id: 0,
            };
            let (insts, anim) = atlas.rasterize_quads(std::slice::from_ref(&q), 0, 1000.0);
            assert_eq!(insts.len(), 1);
            assert!(anim, "docs/demo.avif must be recognized as animated");
        }
        if chart.is_file() {
            let dims = probe_image_size(&chart);
            assert_eq!(dims, Some((760, 444)));
            let q = ImageQuad {
                x: 0.0,
                y: 0.0,
                w: 600.0,
                h: 350.0,
                path: chart.to_string_lossy().into_owned(),
                alpha: 1.0,
                circle: false,
                clip_id: 0,
            };
            let (insts, _) = atlas.rasterize_quads(&[q], 0, 1000.0);
            assert_eq!(insts.len(), 1, "SVG chart must rasterize into ImageAtlas");
        }
    }
}

/// Rasterize a UTF-8 emoji or custom symbol string (`"🦑"`, `"👋🌎"`, `"🔥"`, `"λ"`) into a
/// straight-RGBA8 Retina bitmap using CoreText + Apple Color Emoji cascade.
#[cfg(target_os = "macos")]
pub(crate) fn rasterize_coretext_rgba(text: &str, w_px: usize, h_px: usize) -> Option<Vec<u8>> {
    use mac_imageio::*;
    if text.is_empty() || w_px == 0 || h_px == 0 {
        return None;
    }
    let mut buf = vec![0u8; w_px * h_px * 4];
    // SAFETY: every CF/CG/CT object is checked for null and released before returning; `buf`
    // lives past `CGContextRelease`.
    unsafe {
        let space = CGColorSpaceCreateDeviceRGB();
        if space.is_null() {
            return None;
        }
        let ctx = CGBitmapContextCreate(
            buf.as_mut_ptr().cast(),
            w_px,
            h_px,
            8,
            w_px * 4,
            space,
            RGBA8_PREMUL_LAST,
        );
        CGColorSpaceRelease(space);
        if ctx.is_null() {
            return None;
        }
        CGContextSetShouldAntialias(ctx, true);

        let cf_text = CFStringCreateWithBytes(
            std::ptr::null(),
            text.as_ptr(),
            text.len() as isize,
            CF_UTF8,
            0,
        );
        if cf_text.is_null() {
            CGContextRelease(ctx);
            return None;
        }

        let build_line = |pt: f64| -> (*mut OpaqueCTFont, *mut OpaqueCTLine, f64, f64, f64) {
            let font = CTFontCreateUIFontForLanguage(2, pt, std::ptr::null());
            if font.is_null() {
                return (std::ptr::null_mut(), std::ptr::null_mut(), 0.0, 0.0, 0.0);
            }
            let fg = CGColorCreateGenericRGB(0.93, 0.94, 0.96, 1.0);
            let keys = [kCTFontAttributeName, kCTForegroundColorAttributeName];
            let vals = [font as *const c_void, fg as *const c_void];
            let attrs = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                vals.as_ptr(),
                if fg.is_null() { 1 } else { 2 },
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            );
            if !fg.is_null() {
                CGColorRelease(fg);
            }
            if attrs.is_null() {
                CFRelease(font.cast());
                return (std::ptr::null_mut(), std::ptr::null_mut(), 0.0, 0.0, 0.0);
            }
            let attr_s = CFAttributedStringCreate(std::ptr::null(), cf_text, attrs);
            CFRelease(attrs.cast());
            if attr_s.is_null() {
                CFRelease(font.cast());
                return (std::ptr::null_mut(), std::ptr::null_mut(), 0.0, 0.0, 0.0);
            }
            let line = CTLineCreateWithAttributedString(attr_s);
            CFRelease(attr_s.cast());
            if line.is_null() {
                CFRelease(font.cast());
                return (std::ptr::null_mut(), std::ptr::null_mut(), 0.0, 0.0, 0.0);
            }
            let (mut asc, mut desc, mut lead) = (0.0f64, 0.0f64, 0.0f64);
            let lw = CTLineGetTypographicBounds(line, &mut asc, &mut desc, &mut lead);
            (font, line, lw, asc, desc)
        };

        let mut pt = (h_px as f64) * 0.80;
        let (mut font, mut line, mut lw, mut asc, mut desc) = build_line(pt);
        if !line.is_null() && lw > (w_px as f64 - 2.0) && lw > 1.0 {
            let fit_scale = ((w_px as f64 - 2.0) / lw).clamp(0.35, 1.0);
            CFRelease(line.cast());
            CFRelease(font.cast());
            pt *= fit_scale;
            let (f2, l2, w2, a2, d2) = build_line(pt);
            font = f2;
            line = l2;
            lw = w2;
            asc = a2;
            desc = d2;
        }

        if !line.is_null() {
            let draw_x = ((w_px as f64 - lw) * 0.5).max(0.5);
            let total_h = (asc + desc).max(1.0);
            let draw_y = ((h_px as f64 - total_h) * 0.5 + desc).max(0.5);
            CGContextSetTextPosition(ctx, draw_x, draw_y);
            CTLineDraw(line, ctx);
            CFRelease(line.cast());
        }
        if !font.is_null() {
            CFRelease(font.cast());
        }
        CFRelease(cf_text.cast());
        CGContextRelease(ctx);
    }

    // Un-premultiply RGBA for `ui_sprite_fs` straight-alpha blending.
    for px in buf.chunks_exact_mut(4) {
        let a = px[3];
        if a > 0 && a < 255 {
            let af = f32::from(a) / 255.0;
            px[0] = ((f32::from(px[0]) / af).round().clamp(0.0, 255.0)) as u8;
            px[1] = ((f32::from(px[1]) / af).round().clamp(0.0, 255.0)) as u8;
            px[2] = ((f32::from(px[2]) / af).round().clamp(0.0, 255.0)) as u8;
        }
    }
    Some(buf)
}

/// Rasterize a single Unicode `codepoint` from an in-memory TTF/OTF font buffer (`font_bytes`),
/// an installed macOS font name (`font_name`), or the system UI font into a centered straight-RGBA8
/// Retina bitmap (`w_px x h_px`). When `tint` is `None`, renders pure white so Metal's
/// `GlyphInst.color` can tint the glyph on the GPU.
#[cfg(target_os = "macos")]
pub(crate) fn rasterize_custom_font_glyph_rgba(
    font_name: Option<&str>,
    font_bytes: Option<&[u8]>,
    codepoint: u32,
    w_px: usize,
    h_px: usize,
    tint: Option<[u8; 4]>,
) -> Option<Vec<u8>> {
    use mac_imageio::*;
    if w_px == 0 || h_px == 0 {
        return None;
    }
    let ch = char::from_u32(codepoint)?;
    let mut ch_buf = [0u8; 4];
    let text = ch.encode_utf8(&mut ch_buf);
    let (fg_r, fg_g, fg_b, fg_a) = match tint {
        Some([r, g, b, a]) => (
            f64::from(r) / 255.0,
            f64::from(g) / 255.0,
            f64::from(b) / 255.0,
            f64::from(a) / 255.0,
        ),
        None => (1.0, 1.0, 1.0, 1.0),
    };

    let mut buf = vec![0u8; w_px * h_px * 4];
    // SAFETY: Every CF/CG/CT handle created below is null-checked and explicitly released before
    // returning; `buf` holds `w_px * h_px * 4` bytes and outlives `ctx`.
    let drew = unsafe {
        let space = CGColorSpaceCreateDeviceRGB();
        if space.is_null() {
            return None;
        }
        let ctx = CGBitmapContextCreate(
            buf.as_mut_ptr().cast(),
            w_px,
            h_px,
            8,
            w_px * 4,
            space,
            RGBA8_PREMUL_LAST,
        );
        CGColorSpaceRelease(space);
        if ctx.is_null() {
            return None;
        }
        CGContextSetShouldAntialias(ctx, true);

        let cf_text = cf_str(text);
        if cf_text.is_null() {
            CGContextRelease(ctx);
            return None;
        }

        let make_font = |pt: f64| -> *mut OpaqueCTFont {
            if let Some(bytes) = font_bytes.filter(|b| !b.is_empty()) {
                let cf_data = CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize);
                if cf_data.is_null() {
                    return std::ptr::null_mut();
                }
                let provider = CGDataProviderCreateWithCFData(cf_data);
                CFRelease(cf_data.cast());
                if provider.is_null() {
                    return std::ptr::null_mut();
                }
                let cg_font = CGFontCreateWithDataProvider(provider);
                CGDataProviderRelease(provider);
                if cg_font.is_null() {
                    return std::ptr::null_mut();
                }
                let ct_font =
                    CTFontCreateWithGraphicsFont(cg_font, pt, std::ptr::null(), std::ptr::null());
                CGFontRelease(cg_font);
                ct_font
            } else if let Some(name) = font_name.filter(|s| !s.is_empty()) {
                let cf_name = cf_str(name);
                if cf_name.is_null() {
                    return std::ptr::null_mut();
                }
                let ct_font = CTFontCreateWithName(cf_name, pt, std::ptr::null());
                CFRelease(cf_name.cast());
                ct_font
            } else {
                CTFontCreateUIFontForLanguage(CT_UI_SYSTEM, pt, std::ptr::null())
            }
        };

        let build_line = |pt: f64| -> (*mut OpaqueCTFont, *mut OpaqueCTLine, f64, f64, f64) {
            let font = make_font(pt);
            if font.is_null() {
                return (std::ptr::null_mut(), std::ptr::null_mut(), 0.0, 0.0, 0.0);
            }
            let fg = CGColorCreateGenericRGB(fg_r, fg_g, fg_b, fg_a);
            let keys = [kCTFontAttributeName, kCTForegroundColorAttributeName];
            let vals = [font as *const c_void, fg as *const c_void];
            let attrs = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                vals.as_ptr(),
                if fg.is_null() { 1 } else { 2 },
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            );
            if !fg.is_null() {
                CGColorRelease(fg);
            }
            if attrs.is_null() {
                CFRelease(font.cast());
                return (std::ptr::null_mut(), std::ptr::null_mut(), 0.0, 0.0, 0.0);
            }
            let attr_s = CFAttributedStringCreate(std::ptr::null(), cf_text, attrs);
            CFRelease(attrs.cast());
            if attr_s.is_null() {
                CFRelease(font.cast());
                return (std::ptr::null_mut(), std::ptr::null_mut(), 0.0, 0.0, 0.0);
            }
            let line = CTLineCreateWithAttributedString(attr_s);
            CFRelease(attr_s.cast());
            if line.is_null() {
                CFRelease(font.cast());
                return (std::ptr::null_mut(), std::ptr::null_mut(), 0.0, 0.0, 0.0);
            }
            let (mut asc, mut desc, mut lead) = (0.0f64, 0.0f64, 0.0f64);
            let lw = CTLineGetTypographicBounds(line, &mut asc, &mut desc, &mut lead);
            (font, line, lw, asc, desc)
        };

        let mut pt = (h_px as f64) * 0.84;
        let (mut font, mut line, mut lw, mut asc, mut desc) = build_line(pt);
        if !line.is_null() {
            let max_w = (w_px as f64 - 2.0).max(1.0);
            let max_h = (h_px as f64 - 2.0).max(1.0);
            let total_h = asc + desc;
            let scale_w = if lw > max_w && lw > 1.0 { max_w / lw } else { 1.0 };
            let scale_h = if total_h > max_h && total_h > 1.0 { max_h / total_h } else { 1.0 };
            let fit_scale = scale_w.min(scale_h);
            if fit_scale < 1.0 {
                CFRelease(line.cast());
                CFRelease(font.cast());
                pt *= fit_scale.clamp(0.25, 1.0);
                let (f2, l2, w2, a2, d2) = build_line(pt);
                font = f2;
                line = l2;
                lw = w2;
                asc = a2;
                desc = d2;
            }
        }

        let ok = if !line.is_null() {
            let draw_x = ((w_px as f64 - lw) * 0.5).max(0.5);
            let total_h = (asc + desc).max(1.0);
            let draw_y = ((h_px as f64 - total_h) * 0.5 + desc).max(0.5);
            CGContextSetTextPosition(ctx, draw_x, draw_y);
            CTLineDraw(line, ctx);
            CFRelease(line.cast());
            true
        } else {
            false
        };
        if !font.is_null() {
            CFRelease(font.cast());
        }
        CFRelease(cf_text.cast());
        CGContextRelease(ctx);
        ok
    };

    if !drew {
        return None;
    }
    unpremultiply_rgba(&mut buf);
    Some(buf)
}

/// Decode in-memory PNG bytes via ImageIO and rasterize centered with preserved aspect ratio
/// into a straight-RGBA8 Retina bitmap (`w_px x h_px`).
#[cfg(target_os = "macos")]
pub(crate) fn rasterize_png_bytes_rgba(
    png_bytes: &[u8],
    w_px: usize,
    h_px: usize,
) -> Option<Vec<u8>> {
    use mac_imageio::*;
    if png_bytes.is_empty() || w_px == 0 || h_px == 0 {
        return None;
    }
    let mut buf = vec![0u8; w_px * h_px * 4];
    // SAFETY: `png_bytes` is valid for `CFDataCreate`; every CF/CG handle is null-checked and
    // released before returning; `buf` holds `w_px * h_px * 4` bytes and outlives `ctx`.
    let ok = unsafe {
        let cf_data = CFDataCreate(std::ptr::null(), png_bytes.as_ptr(), png_bytes.len() as isize);
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
        let src_w = CGImageGetWidth(img);
        let src_h = CGImageGetHeight(img);
        if src_w == 0 || src_h == 0 {
            CGImageRelease(img);
            return None;
        }
        let space = CGColorSpaceCreateDeviceRGB();
        if space.is_null() {
            CGImageRelease(img);
            return None;
        }
        let ctx = CGBitmapContextCreate(
            buf.as_mut_ptr().cast(),
            w_px,
            h_px,
            8,
            w_px * 4,
            space,
            RGBA8_PREMUL_LAST,
        );
        CGColorSpaceRelease(space);
        if ctx.is_null() {
            CGImageRelease(img);
            return None;
        }
        let scale = (w_px as f64 / src_w as f64).min(h_px as f64 / src_h as f64);
        let draw_w = src_w as f64 * scale;
        let draw_h = src_h as f64 * scale;
        let draw_x = (w_px as f64 - draw_w) * 0.5;
        let draw_y = (h_px as f64 - draw_h) * 0.5;
        CGContextSetInterpolationQuality(ctx, INTERP_HIGH);
        CGContextDrawImage(ctx, CGRectRaw { x: draw_x, y: draw_y, w: draw_w, h: draw_h }, img);
        CGContextRelease(ctx);
        CGImageRelease(img);
        true
    };
    if !ok {
        return None;
    }
    unpremultiply_rgba(&mut buf);
    Some(buf)
}

/// Shape an ASCII operator sequence (`->`, `-->`, `=>`, `!=`, `|>`, etc.) with CoreText OpenType
/// `calt` + `liga` ligatures. Returns `Some(alpha_bytes)` of size `w * h` ONLY when CoreText
/// actually substitutes ligature glyphs different from `plain_gids`.
#[cfg(target_os = "macos")]
pub(crate) fn shape_and_raster_ligature_alpha(
    font_ptr: *const c_void,
    text: &str,
    plain_gids: &[u16],
    w: usize,
    h: usize,
    baseline: f64,
) -> Option<Vec<u8>> {
    use mac_imageio::*;
    use objc2_core_foundation::CFRange;
    use objc2_core_graphics::{CGBitmapContextCreate as ObjCGBitmap, CGColorSpace, CGContext};

    if font_ptr.is_null() || text.is_empty() || w == 0 || h == 0 {
        return None;
    }
    let lig_val: i32 = 2;
    let mut buf = vec![0u8; w * h];
    let mut ligated = false;

    // SAFETY: Every CF/CT object is checked for null and released before returning; `buf` lives
    // past the `CGContext` drop so CoreGraphics never writes out of bounds.
    unsafe {
        let cf_str = CFStringCreateWithBytes(
            std::ptr::null(),
            text.as_ptr(),
            text.len() as isize,
            CF_UTF8,
            0,
        );
        if cf_str.is_null() {
            return None;
        }
        let cf_lig = CFNumberCreate(
            std::ptr::null(),
            CF_SINT32,
            (&lig_val as *const i32).cast(),
        );
        if cf_lig.is_null() {
            CFRelease(cf_str.cast());
            return None;
        }
        let keys = [
            kCTFontAttributeName,
            kCTLigatureAttributeName,
            kCTForegroundColorFromContextAttributeName,
        ];
        let vals = [font_ptr, cf_lig.cast::<c_void>(), kCFBooleanTrue];
        let attrs = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            vals.as_ptr(),
            keys.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        CFRelease(cf_lig.cast());
        if attrs.is_null() {
            CFRelease(cf_str.cast());
            return None;
        }
        let attr_str = CFAttributedStringCreate(std::ptr::null(), cf_str, attrs);
        CFRelease(cf_str.cast());
        CFRelease(attrs.cast());
        if attr_str.is_null() {
            return None;
        }
        let line = CTLineCreateWithAttributedString(attr_str);
        CFRelease(attr_str.cast());
        if line.is_null() {
            return None;
        }

        let runs = CTLineGetGlyphRuns(line);
        let mut shaped_gids: Vec<u16> = Vec::with_capacity(plain_gids.len());
        if !runs.is_null() {
            let run_count = CFArrayGetCount(runs);
            for ri in 0..run_count {
                let run = CFArrayGetValueAtIndex(runs, ri);
                if run.is_null() {
                    continue;
                }
                let gc = CTRunGetGlyphCount(run);
                if gc > 0 && gc <= 8 {
                    let mut gbuf = [0u16; 8];
                    CTRunGetGlyphs(run, CFRange { location: 0, length: gc }, gbuf.as_mut_ptr());
                    shaped_gids.extend_from_slice(&gbuf[..gc as usize]);
                }
            }
        }

        if shaped_gids.as_slice() != plain_gids {
            ligated = true;
            if let Some(space) = CGColorSpace::new_device_gray()
                && let Some(ctx) = ObjCGBitmap(
                    buf.as_mut_ptr().cast(),
                    w,
                    h,
                    8,
                    w,
                    Some(&space),
                    7,
                )
            {
                CGContext::set_should_antialias(Some(&ctx), true);
                CGContext::set_allows_font_smoothing(Some(&ctx), false);
                CGContext::set_gray_fill_color(Some(&ctx), 1.0, 1.0);
                let raw_ctx = objc2_core_foundation::CFRetained::as_ptr(&ctx)
                    .as_ptr()
                    .cast::<OpaqueCGContext>();
                CGContextSetTextPosition(raw_ctx, 0.0, h as f64 - baseline);
                CTLineDraw(line, raw_ctx);
            }
        }
        CFRelease(line.cast());
    }

    if !ligated || buf.iter().all(|&a| a == 0) {
        return None;
    }
    Some(buf)
}
