//! [`FontSource`] on macOS, and all den's typesetter asks of it: CoreText chooses fonts and the font a character falls
//! back to (`CTFontCreateForString`, the system's cascade), measures them (metrics, `CTFontGetAdvancesForGlyphs` with
//! Apple's tracking in it), hands over their tables to shape from (`CTFontCopyTable`), and CoreGraphics rasterises
//! glyphs and formula outlines for the atlas. Shaping, breaking, bidi, carets and hits are den's own (text/shaper.rs).
use super::font::FontSource;
use crate::math::PathSeg;
use objc2_core_foundation::{
    CFArray, CFData, CFDictionary, CFNumber, CFRange, CFRetained, CFString, CFType,
    CGAffineTransform, CGFloat, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGDataProvider, CGFont, CGPath, CGPathElement,
    CGPathElementType,
};
use objc2_core_text::{
    CTFont, CTFontDescriptor, CTFontOrientation, CTFontSymbolicTraits, CTFontTableOptions,
    CTFontUIFontType, kCTFontFeatureSelectorIdentifierKey, kCTFontFeatureSettingsAttribute,
    kCTFontFeatureTypeIdentifierKey, kCTFontOpenTypeFeatureTag, kCTFontOpenTypeFeatureValue,
    kCTFontTraitsAttribute, kCTFontVariationAttribute, kCTFontWeightTrait,
};
use read_fonts::model::{Blob, Source};
use read_fonts::types::Tag;
use std::ptr::NonNull;
use std::sync::Arc;

/// A CoreText font at a size.
#[derive(Clone)]
pub struct Font(CFRetained<CTFont>);

unsafe extern "C-unwind" {
    fn CFDictionaryGetValue(
        dict: &CFDictionary,
        key: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
}

/// CoreText's normalized weight of 500 (`NSFontWeightMedium`).
const MEDIUM_WEIGHT: f32 = 0.23;

/// Tables shaping never reads: glyph images, some of them hundreds of megabytes (Apple Color Emoji's `sbix`).
const IMAGES: [&[u8; 4]; 8] =
    [b"sbix", b"CBDT", b"CBLC", b"EBDT", b"EBLC", b"bdat", b"bloc", b"SVG "];

/// A CoreText object that is safe to share between threads: a font (Apple: "font objects can be used simultaneously
/// by multiple operations, work queues, or threads") or an immutable `CFData`. read-fonts asks for both bounds.
struct Shared<T>(T);
// SAFETY: see above; only fonts and immutable data are wrapped.
unsafe impl<T> Send for Shared<T> {}
// SAFETY: as above.
unsafe impl<T> Sync for Shared<T> {}

impl<T> Shared<T> {
    /// What it shares (a closure that calls this holds the whole `Shared`, not its field).
    const fn get(&self) -> &T {
        &self.0
    }
}

impl AsRef<[u8]> for Shared<CFRetained<CFData>> {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: a table CoreText copied out is never changed.
        unsafe { self.0.as_bytes_unchecked() }
    }
}

/// A CoreFoundation object as a `CFType`: every CF type is one.
fn cf<T>(v: &T) -> &CFType {
    // SAFETY: only called with CoreFoundation objects, which are all CFTypes.
    unsafe { &*(std::ptr::from_ref::<T>(v).cast::<CFType>()) }
}

impl FontSource for Font {
    /// CoreText answers a missing name with another font: one whose name is not `name` is none.
    fn named(name: &str, size: f32) -> Option<Self> {
        // SAFETY: a valid name and size; a null matrix is the identity.
        let f = Self(unsafe {
            CTFont::with_name(&CFString::from_str(name), CGFloat::from(size), std::ptr::null())
        });
        (f.post_script_name() == name).then_some(f)
    }

    fn family(family: &str, size: f32) -> Option<Self> {
        let name = CFString::from_str(family);
        // SAFETY: a plain read of CoreText's key constant.
        let key = unsafe { objc2_core_text::kCTFontFamilyNameAttribute };
        let attrs = CFDictionary::<CFType, CFType>::from_slices(&[cf(key)], &[cf(&*name)]);
        // SAFETY: a dictionary of descriptor attributes.
        let desc = unsafe { CTFontDescriptor::with_attributes(attrs.as_opaque()) };
        // SAFETY: a live descriptor; a null matrix is the identity.
        let f = Self(unsafe {
            CTFont::with_font_descriptor(&desc, CGFloat::from(size), std::ptr::null())
        });
        // SAFETY: a plain getter on a live font.
        let got = unsafe { f.0.family_name() }.to_string();
        got.eq_ignore_ascii_case(family).then_some(f)
    }

    fn system(size: f32, bold: bool) -> Self {
        let kind = if bold { CTFontUIFontType::EmphasizedSystem } else { CTFontUIFontType::System };
        // SAFETY: valid UI font type and size; None language means the current one.
        Self(
            unsafe { CTFont::new_ui_font_for_language(kind, CGFloat::from(size), None) }
                .expect("system UI font"),
        )
    }

    fn fixed(size: f32) -> Self {
        // SAFETY: a valid UI font type and size; None is the current language.
        Self(
            unsafe {
                CTFont::new_ui_font_for_language(
                    CTFontUIFontType::UserFixedPitch,
                    CGFloat::from(size),
                    None,
                )
            }
            .expect("fixed-pitch font"),
        )
    }

    fn from_data(bytes: &[u8], size: f32) -> Option<Self> {
        let provider = CGDataProvider::with_cf_data(Some(&CFData::from_bytes(bytes)))?;
        let graphics = CGFont::with_data_provider(&provider)?;
        // SAFETY: a live graphics font; a null matrix is the identity and no descriptor is asked for.
        Some(Self(unsafe {
            CTFont::with_graphics_font(&graphics, CGFloat::from(size), std::ptr::null(), None)
        }))
    }

    /// Its descriptor with the weight trait set. The system UI font has no name to ask CoreText for (its Medium is only
    /// reached this way), and the weight trait picks it from the family's variations.
    fn medium(&self, size: f32) -> Option<Self> {
        let weight = CFNumber::new_f32(MEDIUM_WEIGHT);
        // SAFETY: plain reads of CoreText's key constants.
        let (weight_key, traits_key) = unsafe { (kCTFontWeightTrait, kCTFontTraitsAttribute) };
        let traits =
            CFDictionary::<CFType, CFType>::from_slices(&[cf(weight_key)], &[cf(&*weight)]);
        let attrs = CFDictionary::<CFType, CFType>::from_slices(&[cf(traits_key)], &[cf(&*traits)]);
        // SAFETY: a live font and a dictionary of descriptor attributes.
        let desc = unsafe { self.0.font_descriptor().copy_with_attributes(attrs.as_opaque()) };
        // SAFETY: a live descriptor; a null matrix is the identity.
        let font = Self(unsafe {
            CTFont::with_font_descriptor(&desc, CGFloat::from(size), std::ptr::null())
        });
        font.post_script_name().ends_with("Medium").then_some(font)
    }

    /// Both the AAT setting (number spacing, monospaced numbers) and OpenType's `tnum` are asked for, and the font keeps
    /// the one it has.
    fn tabular(&self, size: f32) -> Self {
        /// AAT: the number-spacing feature type and its monospaced-numbers selector.
        const NUMBER_SPACING: i32 = 6;
        const MONOSPACED_NUMBERS: i32 = 0;
        let (kind, selector) =
            (CFNumber::new_i32(NUMBER_SPACING), CFNumber::new_i32(MONOSPACED_NUMBERS));
        let (tag, on) = (CFString::from_str("tnum"), CFNumber::new_i32(1));
        // SAFETY: plain reads of CoreText's key constants.
        let (type_key, selector_key, tag_key, value_key, settings_key) = unsafe {
            (
                kCTFontFeatureTypeIdentifierKey,
                kCTFontFeatureSelectorIdentifierKey,
                kCTFontOpenTypeFeatureTag,
                kCTFontOpenTypeFeatureValue,
                kCTFontFeatureSettingsAttribute,
            )
        };
        let aat = CFDictionary::<CFType, CFType>::from_slices(
            &[cf(type_key), cf(selector_key)],
            &[cf(&*kind), cf(&*selector)],
        );
        let open_type = CFDictionary::<CFType, CFType>::from_slices(
            &[cf(tag_key), cf(value_key)],
            &[cf(&*tag), cf(&*on)],
        );
        let settings =
            CFArray::<CFDictionary<CFType, CFType>>::from_retained_objects(&[aat, open_type]);
        let attrs =
            CFDictionary::<CFType, CFType>::from_slices(&[cf(settings_key)], &[cf(&*settings)]);
        // SAFETY: a live font and a dictionary of descriptor attributes.
        let desc = unsafe { self.0.font_descriptor().copy_with_attributes(attrs.as_opaque()) };
        // SAFETY: a live descriptor; a null matrix is the identity.
        Self(unsafe { CTFont::with_font_descriptor(&desc, CGFloat::from(size), std::ptr::null()) })
    }

    fn italic(&self, size: f32, bold: bool) -> Option<Self> {
        let traits = if bold {
            CTFontSymbolicTraits::ItalicTrait | CTFontSymbolicTraits::BoldTrait
        } else {
            CTFontSymbolicTraits::ItalicTrait
        };
        // SAFETY: a live font; a null matrix is the identity.
        unsafe {
            self.0.copy_with_symbolic_traits(CGFloat::from(size), std::ptr::null(), traits, traits)
        }
        .map(Self)
    }

    /// A copy with `kCTFontVariationAttribute` set to `axes` (keyed by each axis's tag as a number, as CoreText reports
    /// them); none unless CoreText then reports every one of them at its value (a font without the axis ignores it).
    fn varied(&self, size: f32, axes: &[(u32, f32)]) -> Option<Self> {
        if axes.is_empty() {
            return None;
        }
        let tags: Vec<CFRetained<CFNumber>> =
            axes.iter().map(|&(t, _)| CFNumber::new_i64(i64::from(t))).collect();
        let values: Vec<CFRetained<CFNumber>> =
            axes.iter().map(|&(_, v)| CFNumber::new_f64(f64::from(v))).collect();
        let keys: Vec<&CFType> = tags.iter().map(|k| cf(&**k)).collect();
        let vals: Vec<&CFType> = values.iter().map(|v| cf(&**v)).collect();
        let variation = CFDictionary::<CFType, CFType>::from_slices(&keys, &vals);
        // SAFETY: a plain read of CoreText's key constant.
        let key = unsafe { kCTFontVariationAttribute };
        let attrs = CFDictionary::<CFType, CFType>::from_slices(&[cf(key)], &[cf(&*variation)]);
        // SAFETY: a dictionary of descriptor attributes.
        let desc = unsafe { CTFontDescriptor::with_attributes(attrs.as_opaque()) };
        // SAFETY: a live font and descriptor; a null matrix is the identity.
        let font = Self(unsafe {
            self.0.copy_with_attributes(CGFloat::from(size), std::ptr::null(), Some(&desc))
        });
        let set = font.variations();
        axes.iter()
            .all(|&(t, v)| set.iter().any(|&(k, x)| k == t && (x - v).abs() < 0.5))
            .then_some(font)
    }

    /// This one when it has the text.
    fn fallback(&self, text: &str) -> Self {
        let s = CFString::from_str(text);
        // SAFETY: a live font, and a range inside the string.
        Self(unsafe { self.0.for_string(&s, CFRange { location: 0, length: s.length() }) })
    }

    fn post_script_name(&self) -> String {
        // SAFETY: a plain getter on a live font.
        unsafe { self.0.post_script_name() }.to_string()
    }

    fn size(&self) -> f32 {
        // SAFETY: a plain getter on a live font.
        unsafe { self.0.size() as f32 }
    }

    fn ascent(&self) -> f32 {
        // SAFETY: a plain metric getter.
        unsafe { self.0.ascent() as f32 }
    }

    fn descent(&self) -> f32 {
        // SAFETY: a plain metric getter.
        unsafe { self.0.descent() as f32 }
    }

    fn cap_height(&self) -> f32 {
        // SAFETY: a plain metric getter.
        unsafe { self.0.cap_height() as f32 }
    }

    fn x_height(&self) -> f32 {
        // SAFETY: a plain metric getter.
        unsafe { self.0.x_height() as f32 }
    }

    fn underline_thickness(&self) -> f32 {
        // SAFETY: a plain metric getter.
        unsafe { self.0.underline_thickness() as f32 }
    }

    fn glyph(&self, ch: char) -> u16 {
        let mut units = [0u16; 2];
        let n = ch.encode_utf16(&mut units).len();
        let mut glyphs = [0u16; 2];
        // SAFETY: both buffers hold two units and at most `n` (one or two) are read and written.
        let found = unsafe {
            self.0.glyphs_for_characters(
                NonNull::from(&mut units).cast(),
                NonNull::from(&mut glyphs).cast(),
                n as isize,
            )
        };
        if found { glyphs[0] } else { 0 }
    }

    /// The font's own advance with Apple's tracking for the size, which is not always the `trak` table as HarfRust reads
    /// it (CoreText leaves a lone track other than the normal one off, as PingFang's; it spaces Apple Color Emoji by a
    /// curve of its own, a third of an em wider up to 16 points).
    fn advance(&self, glyph: u16) -> f64 {
        let mut g = glyph;
        let mut adv = CGSize { width: 0.0, height: 0.0 };
        // SAFETY: one glyph and one advance.
        unsafe {
            self.0.advances_for_glyphs(
                CTFontOrientation::Default,
                NonNull::from(&mut g),
                &mut adv,
                1,
            )
        };
        adv.width
    }

    /// Its tables, each read through CoreText when first asked for; the reader may be called from any thread.
    fn source(&self) -> Option<(Source, u32)> {
        let ct = Shared(self.0.clone());
        let tables = move |tag: Tag| -> Option<Blob> {
            if IMAGES.iter().any(|t| Tag::new(t) == tag) {
                return None;
            }
            // SAFETY: a live font and a four-byte tag.
            let data = unsafe {
                ct.get().table(u32::from_be_bytes(tag.to_be_bytes()), CTFontTableOptions(0))
            }?;
            Some(Blob::from(Arc::new(Shared(data)) as Arc<dyn AsRef<[u8]> + Send + Sync>))
        };
        Some((Source::from(Arc::new(tables) as Arc<dyn Fn(Tag) -> Option<Blob> + Send + Sync>), 0))
    }

    /// `kCTFontFeatureSettingsAttribute`'s OpenType settings.
    fn features(&self) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        // SAFETY: a plain getter on a live font; the array holds dictionaries.
        let Some(settings) = (unsafe { self.0.feature_settings() }) else { return out };
        // SAFETY: CoreText's key constants.
        let (tag_key, value_key) =
            unsafe { (kCTFontOpenTypeFeatureTag, kCTFontOpenTypeFeatureValue) };
        for i in 0..settings.count() {
            // SAFETY: the index is in range and every value is a dictionary; a missing key reads as null.
            unsafe {
                let dict = &*settings.value_at_index(i).cast::<CFDictionary>();
                let tag =
                    CFDictionaryGetValue(dict, std::ptr::from_ref::<CFString>(tag_key).cast())
                        .cast::<CFString>();
                let value =
                    CFDictionaryGetValue(dict, std::ptr::from_ref::<CFString>(value_key).cast())
                        .cast::<CFNumber>();
                let (Some(tag), Some(value)) = (tag.as_ref(), value.as_ref()) else { continue };
                let tag = tag.to_string();
                if let (Ok(t), Some(v)) = (<[u8; 4]>::try_from(tag.as_bytes()), value.as_i64()) {
                    out.push((u32::from_be_bytes(t), v as u32));
                }
            }
        }
        out
    }

    /// The axes CoreText reports.
    fn variations(&self) -> Vec<(u32, f32)> {
        let mut out = Vec::new();
        // SAFETY: a plain getter on a live font.
        if let Some(dict) = unsafe { self.0.variation() } {
            let n = dict.count() as usize;
            let (mut keys, mut values) = (vec![std::ptr::null(); n], vec![std::ptr::null(); n]);
            // SAFETY: both buffers hold the dictionary's count; its keys and values are numbers.
            unsafe { dict.keys_and_values(keys.as_mut_ptr(), values.as_mut_ptr()) };
            for (k, v) in keys.into_iter().zip(values) {
                // SAFETY: as above.
                let (k, v) = unsafe { (&*k.cast::<CFNumber>(), &*v.cast::<CFNumber>()) };
                if let (Some(k), Some(v)) = (k.as_i64(), v.as_f64()) {
                    out.push((k as u32, v as f32));
                }
            }
        }
        out
    }

    fn bounds(&self, glyph: u16) -> [f64; 4] {
        let mut g = glyph;
        let mut bb =
            CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: 0.0, height: 0.0 } };
        // SAFETY: one glyph and one rect, both valid.
        unsafe {
            self.0.bounding_rects_for_glyphs(
                CTFontOrientation::Default,
                NonNull::from(&mut g),
                &mut bb,
                1,
            )
        };
        [bb.origin.x, bb.origin.y, bb.size.width, bb.size.height]
    }

    fn colour(&self) -> bool {
        // SAFETY: a live font.
        unsafe { self.0.symbolic_traits() }.contains(CTFontSymbolicTraits::TraitColorGlyphs)
    }

    fn draw(
        &self,
        glyph: u16,
        buf: &mut [u8],
        w: usize,
        h: usize,
        x0: i32,
        y0: i32,
        scale: f64,
        subx: f64,
    ) {
        let Some(ctx) = context(buf, w, h, if self.colour() { 4 } else { 1 }) else { return };
        CGContext::translate_ctm(Some(&ctx), -f64::from(x0), -f64::from(y0));
        CGContext::scale_ctm(Some(&ctx), scale, scale);
        let mut g = glyph;
        let mut at = CGPoint { x: subx / scale, y: 0.0 };
        // SAFETY: one glyph and one position; the context is live.
        unsafe { self.0.draw_glyphs(NonNull::from(&mut g), NonNull::from(&mut at), 1, &ctx) };
    }
}

impl Font {
    /// The line gap the font asks for below its descent, points.
    pub fn leading(&self) -> f32 {
        // SAFETY: a plain metric getter.
        unsafe { self.0.leading() as f32 }
    }

    /// The font's table `tag`, as CoreText hands it over.
    pub fn table_bytes(&self, tag: &[u8; 4]) -> Option<Vec<u8>> {
        // SAFETY: a live font and a four-byte tag.
        let data = unsafe { self.0.table(u32::from_be_bytes(*tag), CTFontTableOptions(0)) }?;
        Some(data.to_vec())
    }

    /// `text` (one grapheme cluster) as CoreText sets it in this font: its glyphs in order. Apple Color Emoji's flag
    /// and ZWJ sequences are joined into their one glyph this way (its `morx` as CoreText applies it).
    pub fn ct_glyphs(&self, text: &str) -> Vec<u16> {
        use objc2_core_foundation::CFAttributedString;
        use objc2_core_text::{CTLine, CTRun, kCTFontAttributeName};
        let s = CFString::from_str(text);
        // SAFETY: a plain read of CoreText's key constant.
        let key = unsafe { kCTFontAttributeName };
        let attrs = CFDictionary::<CFType, CFType>::from_slices(&[cf(key)], &[cf(&*self.0)]);
        // SAFETY: a live string and a dictionary of string attributes.
        let Some(astr) =
            (unsafe { CFAttributedString::new(None, Some(&s), Some(attrs.as_opaque())) })
        else {
            return Vec::new();
        };
        // SAFETY: a live attributed string.
        let line = unsafe { CTLine::with_attributed_string(&astr) };
        // SAFETY: a live line; its runs array holds CTRuns.
        let runs = unsafe { line.glyph_runs() };
        let mut out = Vec::new();
        for i in 0..runs.count() {
            // SAFETY: the index is in range and every value is a CTRun.
            let run = unsafe { &*runs.value_at_index(i).cast::<CTRun>() };
            // SAFETY: a live run.
            let n = unsafe { run.glyph_count() };
            if n <= 0 {
                continue;
            }
            let mut glyphs = vec![0u16; n as usize];
            // SAFETY: the buffer holds the run's glyph count.
            unsafe {
                run.glyphs(
                    CFRange { location: 0, length: 0 },
                    NonNull::new_unchecked(glyphs.as_mut_ptr()),
                )
            };
            out.extend(glyphs);
        }
        out
    }

    /// This font at another size.
    #[must_use]
    pub fn sized(&self, size: f32) -> Self {
        // SAFETY: a live font; a null matrix is the identity and no attributes change.
        Self(unsafe { self.0.copy_with_attributes(CGFloat::from(size), std::ptr::null(), None) })
    }

    /// `glyph` drawn into `buf` (as [`FontSource::draw`]) with its outline scaled `sx` by `sy` pixels a point and its
    /// origin at (`tx`, `ty`) pixels, the bitmap's bottom-left pixel being (`x0`, `y0`): a glyph stretched or moved into
    /// a terminal cell.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_placed(
        &self,
        glyph: u16,
        buf: &mut [u8],
        w: usize,
        h: usize,
        x0: i32,
        y0: i32,
        sx: f64,
        sy: f64,
        tx: f64,
        ty: f64,
    ) {
        let Some(ctx) = context(buf, w, h, if self.colour() { 4 } else { 1 }) else { return };
        CGContext::translate_ctm(Some(&ctx), tx - f64::from(x0), ty - f64::from(y0));
        CGContext::scale_ctm(Some(&ctx), sx, sy);
        let mut g = glyph;
        let mut at = CGPoint { x: 0.0, y: 0.0 };
        // SAFETY: one glyph and one position; the context is live.
        unsafe { self.0.draw_glyphs(NonNull::from(&mut g), NonNull::from(&mut at), 1, &ctx) };
    }
}

/// Register the font files at `paths` for this process (`CTFontManagerRegisterFontURLs`, process scope), so CoreText
/// finds them by name and family like installed ones. Waits for registration to finish; the files that failed, by error.
pub fn register_fonts(paths: &[std::path::PathBuf]) -> Result<(), String> {
    use objc2_core_foundation::CFURL;
    use objc2_core_text::{CTFontManagerRegisterFontURLs, CTFontManagerScope};
    let urls: Vec<CFRetained<CFURL>> = paths.iter().filter_map(CFURL::from_file_path).collect();
    if urls.len() != paths.len() {
        return Err(format!("not file paths: {paths:?}"));
    }
    let array = CFArray::<CFURL>::from_retained_objects(&urls);
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    // objc2 has no block encoding for C `bool`; `Bool` is the same one byte on Apple's targets.
    let handler = block2::RcBlock::new(
        move |errors: NonNull<CFArray>, done: objc2::runtime::Bool| -> objc2::runtime::Bool {
            let done = done.as_bool();
            // SAFETY: CoreText hands a live array of CFErrors for the duration of the call.
            let n = unsafe { errors.as_ref() }.count();
            if n > 0 {
                let _ = tx.send(Err(format!("{n} font file(s) failed to register")));
            }
            if done {
                let _ = tx.send(Ok(()));
            }
            objc2::runtime::Bool::YES
        },
    );
    // SAFETY: the same block layout and calling convention; only the declared argument types differ (Bool for bool).
    let handler: &block2::DynBlock<dyn Fn(NonNull<CFArray>, bool) -> bool> =
        unsafe { &*std::ptr::from_ref(&*handler).cast() };
    // SAFETY: an array of file URLs and a handler that only sends on a channel.
    unsafe {
        CTFontManagerRegisterFontURLs(
            array.as_opaque(),
            CTFontManagerScope::Process,
            true,
            Some(&handler),
        )
    };
    let mut errs = Vec::new();
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => break,
            Ok(Err(e)) => errs.push(e),
            Err(e) => {
                errs.push(format!("font registration did not finish: {e}"));
                break;
            }
        }
    }
    if errs.is_empty() { Ok(()) } else { Err(errs.join("; ")) }
}

/// A bitmap context over `buf`, `w` by `h` pixels of `px` bytes: device RGB with premultiplied alpha
/// (`kCGImageAlphaPremultipliedLast`), or alpha only (`kCGImageAlphaOnly`).
fn context(buf: &mut [u8], w: usize, h: usize, px: usize) -> Option<CFRetained<CGContext>> {
    thread_local! {
        static SPACES: (Option<CFRetained<CGColorSpace>>, Option<CFRetained<CGColorSpace>>) = (CGColorSpace::new_device_rgb(), CGColorSpace::new_device_gray());
    }
    let (space, info) =
        SPACES.with(|(rgb, gray)| if px == 4 { (rgb.clone(), 1) } else { (gray.clone(), 7) });
    let space = space?;
    // SAFETY: the buffer is w*h*px bytes, px bytes per pixel; callers drop the context before reading the buffer.
    let ctx = unsafe {
        CGBitmapContextCreate(buf.as_mut_ptr().cast(), w, h, 8, w * px, Some(&space), info)
    }?;
    CGContext::set_should_antialias(Some(&ctx), true);
    CGContext::set_allows_antialiasing(Some(&ctx), true);
    // No font smoothing: it thickens every stem (0.15 px a side at 16 px to the em), and den's text is a browser's
    // with `-webkit-font-smoothing: antialiased`, as the brand site sets it (fed1eb428d:docs/research/colour-pipeline.md).
    CGContext::set_allows_font_smoothing(Some(&ctx), false);
    CGContext::set_should_smooth_fonts(Some(&ctx), false);
    Some(ctx)
}

/// [`super::font::fill_outline`] with CoreGraphics: a filled outline even-odd, each piece (from one move to the next) on its
/// own as RaTeX's renderer fills them, or stroked `stroke` ems wide.
#[allow(clippy::too_many_arguments)]
pub fn fill_outline(segs: &[PathSeg], fill: bool, stroke: f32, buf: &mut [u8], w: usize, h: usize, x0: i32, y0: i32, s: f64, subx: f64) {
    let Some(ctx) = context(buf, w, h, 1) else { return };
    CGContext::set_gray_fill_color(Some(&ctx), 1.0, 1.0);
    CGContext::set_gray_stroke_color(Some(&ctx), 1.0, 1.0);
    CGContext::translate_ctm(Some(&ctx), -f64::from(x0), -f64::from(y0));
    CGContext::translate_ctm(Some(&ctx), subx, 0.0);
    CGContext::scale_ctm(Some(&ctx), s, -s);
    let pt = |p: [f32; 2]| (f64::from(p[0]), f64::from(p[1]));
    let mut open = false;
    let finish = |ctx: &CGContext, open: &mut bool| {
        if *open {
            if fill {
                CGContext::eo_fill_path(Some(ctx));
            } else {
                CGContext::stroke_path(Some(ctx));
            }
            *open = false;
        }
    };
    CGContext::set_line_width(Some(&ctx), f64::from(stroke));
    CGContext::begin_path(Some(&ctx));
    for seg in segs {
        match *seg {
            PathSeg::Move(a) => {
                // A filled outline's pieces are filled one at a time: where they overlap they must not cancel.
                if fill {
                    finish(&ctx, &mut open);
                    CGContext::begin_path(Some(&ctx));
                }
                let (x, y) = pt(a);
                CGContext::move_to_point(Some(&ctx), x, y);
                open = true;
            }
            PathSeg::Line(a) => {
                let (x, y) = pt(a);
                CGContext::add_line_to_point(Some(&ctx), x, y);
            }
            PathSeg::Quad(a, b) => {
                let ((cx, cy), (x, y)) = (pt(a), pt(b));
                CGContext::add_quad_curve_to_point(Some(&ctx), cx, cy, x, y);
            }
            PathSeg::Cubic(a, b, c) => {
                let ((c1x, c1y), (c2x, c2y), (x, y)) = (pt(a), pt(b), pt(c));
                CGContext::add_curve_to_point(Some(&ctx), c1x, c1y, c2x, c2y, x, y);
            }
            PathSeg::Close => CGContext::close_path(Some(&ctx)),
        }
    }
    finish(&ctx, &mut open);
}
