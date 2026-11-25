//! Metal renderer using `blaze-ui`'s Rust-authored MSL 2.4 shaders (`ui_sdf_*` and
//! `ui_glyph_*`), fragment-shader rounded-rect clip table, 5-layer pass order (cards, glyphs,
//! over-rects, popup overlays, popup glyphs), and offscreen PNG screenshot capture.

use std::ffi::c_void;
use std::fs;
use std::path::Path;
use std::ptr::NonNull;

use blaze_ui::shaders::{ClipRectGpu, GlyphInst, RectInst, ViewportUniform};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBlendFactor, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder,
    MTLCommandQueue, MTLDevice, MTLLibrary, MTLLoadAction, MTLOrigin, MTLPixelFormat,
    MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLResourceOptions,
    MTLSamplerDescriptor, MTLSamplerMinMagFilter, MTLSamplerState, MTLSize, MTLStoreAction,
    MTLTexture, MTLTextureDescriptor, MTLTextureUsage, MTLRegion,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

use crate::font::{ATLAS, Slot};
use crate::image_atlas::{IMAGE_ATLAS_SIZE, ImageSlot};
use crate::svg_icons::{ICON_ATLAS_SIZE, IconSlot};

type Dev = Retained<ProtocolObject<dyn MTLDevice>>;

pub const PIXEL_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;

/// Layers submitted to [`Renderer::draw`] / [`Renderer::render_to_png`].
pub struct FrameDraw<'a> {
    pub vp: ViewportUniform,
    pub clear: [f32; 3],
    pub clips: &'a [ClipRectGpu],
    pub rects: &'a [RectInst],
    pub icons: &'a [GlyphInst],
    pub images: &'a [GlyphInst],
    pub glyphs: &'a [GlyphInst],
    pub over_rects: &'a [RectInst],
    pub overlay_rects: &'a [RectInst],
    pub overlay_icons: &'a [GlyphInst],
    pub overlay_glyphs: &'a [GlyphInst],
}

pub struct Renderer {
    dev: Dev,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    sdf: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    glyph: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    sprite: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    atlas: Retained<ProtocolObject<dyn MTLTexture>>,
    icon_atlas: Retained<ProtocolObject<dyn MTLTexture>>,
    image_atlas: Retained<ProtocolObject<dyn MTLTexture>>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    linear_sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
}

fn pipeline(
    dev: &Dev,
    src: &str,
    vs: &str,
    fs: &str,
) -> Retained<ProtocolObject<dyn MTLRenderPipelineState>> {
    let lib = dev
        .newLibraryWithSource_options_error(&NSString::from_str(src), None)
        .unwrap_or_else(|e| panic!("MSL from blaze-ui does not compile: {e}"));
    let f = |n: &str| {
        lib.newFunctionWithName(&NSString::from_str(n))
            .unwrap_or_else(|| panic!("missing MSL shader function `{n}`"))
    };
    let desc = MTLRenderPipelineDescriptor::new();
    desc.setVertexFunction(Some(&f(vs)));
    desc.setFragmentFunction(Some(&f(fs)));
    // SAFETY: attachment 0 always exists on a newly created `MTLRenderPipelineDescriptor`.
    let att = unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) };
    att.setPixelFormat(PIXEL_FORMAT);
    // `blaze-ui` fragment shaders return straight (unpremultiplied) RGB with coverage in alpha.
    att.setBlendingEnabled(true);
    att.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
    att.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    att.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    att.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    dev.newRenderPipelineStateWithDescriptor_error(&desc)
        .unwrap_or_else(|e| panic!("Metal pipeline {vs}/{fs} failed: {e}"))
}

impl Renderer {
    pub fn new(dev: Dev) -> Renderer {
        let mut sdf_src = String::new();
        let mut glyph_src = String::new();
        let mut sprite_src = String::new();
        blaze_ui::shaders::emit_ui_sdf_msl(&mut sdf_src).expect("emit ui_sdf MSL");
        blaze_ui::shaders::emit_ui_glyph_msl(&mut glyph_src).expect("emit ui_glyph MSL");
        blaze_ui::shaders::emit_ui_sprite_msl(&mut sprite_src).expect("emit ui_sprite MSL");
        let sdf = pipeline(&dev, &sdf_src, "ui_sdf_vs", "ui_sdf_fs");
        let glyph = pipeline(&dev, &glyph_src, "ui_glyph_vs", "ui_glyph_fs");
        let sprite = pipeline(&dev, &sprite_src, "ui_glyph_vs", "ui_glyph_fs");
        let queue = dev.newCommandQueue().expect("Metal command queue");
        // SAFETY: valid 2D A8Unorm descriptor of size ATLAS x ATLAS.
        let td = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::A8Unorm,
                ATLAS,
                ATLAS,
                false,
            )
        };
        let atlas = dev.newTextureWithDescriptor(&td).expect("Metal glyph atlas texture");
        // SAFETY: valid 2D RGBA8Unorm descriptor of size ICON_ATLAS_SIZE x ICON_ATLAS_SIZE.
        let itd = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA8Unorm,
                ICON_ATLAS_SIZE,
                ICON_ATLAS_SIZE,
                false,
            )
        };
        let icon_atlas = dev.newTextureWithDescriptor(&itd).expect("Metal icon atlas texture");
        // SAFETY: valid 2D RGBA8Unorm descriptor of size IMAGE_ATLAS_SIZE x IMAGE_ATLAS_SIZE.
        let imtd = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA8Unorm,
                IMAGE_ATLAS_SIZE,
                IMAGE_ATLAS_SIZE,
                false,
            )
        };
        let image_atlas = dev.newTextureWithDescriptor(&imtd).expect("Metal image atlas texture");
        let sd = MTLSamplerDescriptor::new();
        sd.setMinFilter(MTLSamplerMinMagFilter::Nearest);
        sd.setMagFilter(MTLSamplerMinMagFilter::Nearest);
        let sampler = dev.newSamplerStateWithDescriptor(&sd).expect("Metal sampler");
        let lsd = MTLSamplerDescriptor::new();
        lsd.setMinFilter(MTLSamplerMinMagFilter::Linear);
        lsd.setMagFilter(MTLSamplerMinMagFilter::Linear);
        let linear_sampler = dev.newSamplerStateWithDescriptor(&lsd).expect("Metal linear sampler");
        Renderer {
            dev,
            queue,
            sdf,
            glyph,
            sprite,
            atlas,
            icon_atlas,
            image_atlas,
            sampler,
            linear_sampler,
        }
    }

    pub fn device(&self) -> &Dev {
        &self.dev
    }

    pub fn upload(&self, slot: Slot, bytes: &[u8]) {
        let region = MTLRegion {
            origin: MTLOrigin { x: slot.x, y: slot.y, z: 0 },
            size: MTLSize { width: slot.w, height: slot.h, depth: 1 },
        };
        // SAFETY: `bytes` holds `slot.w * slot.h` alpha texels and lives for the call; `region`
        // lies strictly inside the `ATLAS x ATLAS` atlas texture.
        unsafe {
            self.atlas.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(bytes.as_ptr().cast_mut().cast::<c_void>()).expect("non-null"),
                slot.w,
            );
        }
    }

    pub fn upload_icon(&self, slot: IconSlot, rgba_bytes: &[u8]) {
        let region = MTLRegion {
            origin: MTLOrigin { x: slot.x, y: slot.y, z: 0 },
            size: MTLSize { width: slot.w, height: slot.h, depth: 1 },
        };
        // SAFETY: `rgba_bytes` holds `slot.w * slot.h * 4` RGBA8 bytes inside `ICON_ATLAS_SIZE`.
        unsafe {
            self.icon_atlas.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(rgba_bytes.as_ptr().cast_mut().cast::<c_void>()).expect("non-null"),
                slot.w * 4,
            );
        }
    }

    pub fn upload_image(&self, slot: ImageSlot, rgba_bytes: &[u8]) {
        let region = MTLRegion {
            origin: MTLOrigin { x: slot.x, y: slot.y, z: 0 },
            size: MTLSize { width: slot.w, height: slot.h, depth: 1 },
        };
        // SAFETY: `rgba_bytes` holds `slot.w * slot.h * 4` RGBA8 bytes inside `IMAGE_ATLAS_SIZE`.
        unsafe {
            self.image_atlas.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(rgba_bytes.as_ptr().cast_mut().cast::<c_void>()).expect("non-null"),
                slot.w * 4,
            );
        }
    }

    fn buffer<T: Copy>(&self, items: &[T]) -> Option<Retained<ProtocolObject<dyn MTLBuffer>>> {
        let len = std::mem::size_of_val(items).max(16);
        if items.is_empty() {
            return self
                .dev
                .newBufferWithLength_options(len, MTLResourceOptions::StorageModeShared);
        }
        let ptr = NonNull::new(items.as_ptr().cast_mut().cast::<c_void>())?;
        // SAFETY: `items` is readable for `len` bytes (`len >= 16` for every instance/uniform type).
        unsafe {
            self.dev
                .newBufferWithBytes_length_options(ptr, len, MTLResourceOptions::StorageModeShared)
        }
    }

    fn encode_frame(
        &self,
        enc: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        frame: &FrameDraw<'_>,
    ) {
        let fallback_clip = [ClipRectGpu {
            rect: [-1.0e6, -1.0e6, 1.0e6, 1.0e6],
            params: [0.0; 4],
        }];
        let clips = if frame.clips.is_empty() { &fallback_clip[..] } else { frame.clips };
        let (Some(vpb), Some(clipb), Some(frag_sizes)) = (
            self.buffer(&[frame.vp]),
            self.buffer(clips),
            self.buffer(&[clips.len() as u32, 0, 0, 0]),
        ) else {
            return;
        };

        let draw_layer = |pso: &ProtocolObject<dyn MTLRenderPipelineState>,
                          insts: Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
                          n: usize,
                          tex: Option<(&ProtocolObject<dyn MTLTexture>, &ProtocolObject<dyn MTLSamplerState>)>| {
            if n == 0 {
                return;
            }
            let (Some(insts), Some(vert_sizes)) = (insts, self.buffer(&[n as u32, 0, 0, 0])) else {
                return;
            };
            enc.setRenderPipelineState(pso);
            // SAFETY: every Metal buffer is retained by the command encoder until the command
            // buffer finishes; buffer/texture/sampler slots match `blaze-ui`'s emitted MSL.
            unsafe {
                enc.setVertexBuffer_offset_atIndex(Some(&vpb), 0, 0);
                enc.setVertexBuffer_offset_atIndex(Some(&insts), 0, 1);
                enc.setVertexBuffer_offset_atIndex(Some(&vert_sizes), 0, 2);
                enc.setFragmentBuffer_offset_atIndex(Some(&clipb), 0, 0);
                enc.setFragmentBuffer_offset_atIndex(Some(&frag_sizes), 0, 1);
                if let Some((texture, samp)) = tex {
                    enc.setFragmentTexture_atIndex(Some(texture), 0);
                    enc.setFragmentSamplerState_atIndex(Some(samp), 0);
                }
                enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
                    MTLPrimitiveType::Triangle,
                    0,
                    6,
                    n,
                );
            }
        };

        draw_layer(&self.sdf, self.buffer(frame.rects), frame.rects.len(), None);
        draw_layer(
            &self.sprite,
            self.buffer(frame.icons),
            frame.icons.len(),
            Some((&*self.icon_atlas, &*self.linear_sampler)),
        );
        draw_layer(
            &self.sprite,
            self.buffer(frame.images),
            frame.images.len(),
            Some((&*self.image_atlas, &*self.linear_sampler)),
        );
        draw_layer(
            &self.glyph,
            self.buffer(frame.glyphs),
            frame.glyphs.len(),
            Some((&*self.atlas, &*self.sampler)),
        );
        draw_layer(&self.sdf, self.buffer(frame.over_rects), frame.over_rects.len(), None);
        draw_layer(&self.sdf, self.buffer(frame.overlay_rects), frame.overlay_rects.len(), None);
        draw_layer(
            &self.sprite,
            self.buffer(frame.overlay_icons),
            frame.overlay_icons.len(),
            Some((&*self.icon_atlas, &*self.linear_sampler)),
        );
        draw_layer(
            &self.glyph,
            self.buffer(frame.overlay_glyphs),
            frame.overlay_glyphs.len(),
            Some((&*self.atlas, &*self.sampler)),
        );
    }

    pub fn draw(&self, layer: &CAMetalLayer, frame: &FrameDraw<'_>) {
        let Some(drawable) = layer.nextDrawable() else {
            return;
        };
        let rpd = MTLRenderPassDescriptor::new();
        // SAFETY: color attachment 0 always exists on a freshly allocated descriptor.
        let att = unsafe { rpd.colorAttachments().objectAtIndexedSubscript(0) };
        att.setTexture(Some(&drawable.texture()));
        att.setLoadAction(MTLLoadAction::Clear);
        att.setStoreAction(MTLStoreAction::Store);
        att.setClearColor(MTLClearColor {
            red: f64::from(frame.clear[0]),
            green: f64::from(frame.clear[1]),
            blue: f64::from(frame.clear[2]),
            alpha: 1.0,
        });
        let Some(cb) = self.queue.commandBuffer() else {
            return;
        };
        let Some(enc) = cb.renderCommandEncoderWithDescriptor(&rpd) else {
            return;
        };
        self.encode_frame(&enc, frame);
        enc.endEncoding();
        cb.presentDrawable(ProtocolObject::from_ref(&*drawable));
        cb.commit();
    }

    /// Render a frame directly into an offscreen CPU-readable Metal texture and write a PNG to
    /// `path` (deterministic, zero Screen Recording permissions required).
    pub fn render_to_png(
        &self,
        width_px: usize,
        height_px: usize,
        frame: &FrameDraw<'_>,
        path: &Path,
    ) -> Result<(), String> {
        let w = width_px.max(1);
        let h = height_px.max(1);
        // SAFETY: valid 2D BGRA8Unorm descriptor of positive dimensions.
        let td = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                PIXEL_FORMAT,
                w,
                h,
                false,
            )
        };
        td.setUsage(MTLTextureUsage::RenderTarget);
        td.setResourceOptions(MTLResourceOptions::StorageModeShared);
        let tex = self
            .dev
            .newTextureWithDescriptor(&td)
            .ok_or_else(|| "failed to allocate offscreen Metal render target".to_owned())?;

        let rpd = MTLRenderPassDescriptor::new();
        // SAFETY: color attachment 0 always exists.
        let att = unsafe { rpd.colorAttachments().objectAtIndexedSubscript(0) };
        att.setTexture(Some(&tex));
        att.setLoadAction(MTLLoadAction::Clear);
        att.setStoreAction(MTLStoreAction::Store);
        att.setClearColor(MTLClearColor {
            red: f64::from(frame.clear[0]),
            green: f64::from(frame.clear[1]),
            blue: f64::from(frame.clear[2]),
            alpha: 1.0,
        });

        let cb = self
            .queue
            .commandBuffer()
            .ok_or_else(|| "Metal commandBuffer failed".to_owned())?;
        let enc = cb
            .renderCommandEncoderWithDescriptor(&rpd)
            .ok_or_else(|| "Metal renderCommandEncoder failed".to_owned())?;
        self.encode_frame(&enc, frame);
        enc.endEncoding();
        cb.commit();
        cb.waitUntilCompleted();

        let mut bgra = vec![0u8; w * h * 4];
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize { width: w, height: h, depth: 1 },
        };
        // SAFETY: `bgra` is `w * h * 4` bytes matching BGRA8Unorm, and `tex` uses
        // `StorageModeShared` after `waitUntilCompleted()`.
        unsafe {
            tex.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::new(bgra.as_mut_ptr().cast()).expect("non-null"),
                w * 4,
                region,
                0,
            );
        }

        // Convert BGRA8 -> RGBA8.
        let mut rgba = vec![0u8; w * h * 4];
        for (src, dst) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = 0xff;
        }

        let png = encode_png_rgba(w as u32, h as u32, &rgba);
        fs::write(path, png).map_err(|e| format!("write png `{}`: {e}", path.display()))
    }
}

/// Minimal zero-dependency valid PNG encoder (RGBA8, uncompressed stored zlib blocks).
fn encode_png_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let row_bytes = width as usize * 4;
    let mut raw = Vec::with_capacity(height as usize * (1 + row_bytes));
    for y in 0..height as usize {
        raw.push(0); // filter type 0 (None)
        let start = y * row_bytes;
        raw.extend_from_slice(&rgba[start..start + row_bytes]);
    }

    // Wrap raw scanlines in valid zlib stream using uncompressed stored DEFLATE blocks (<= 65535B).
    let mut zlib = Vec::with_capacity(raw.len() + (raw.len() / 65535 + 1) * 5 + 6);
    zlib.push(0x78); // CMF: deflate, 32K window
    zlib.push(0x01); // FLG: no dict, check bits (0x7801 % 31 == 0)
    let mut offset = 0usize;
    while offset < raw.len() {
        let remaining = raw.len() - offset;
        let chunk_len = remaining.min(65_535);
        let is_final = offset + chunk_len >= raw.len();
        zlib.push(if is_final { 0x01 } else { 0x00 });
        let len_u16 = chunk_len as u16;
        let nlen_u16 = !len_u16;
        zlib.extend_from_slice(&len_u16.to_le_bytes());
        zlib.extend_from_slice(&nlen_u16.to_le_bytes());
        zlib.extend_from_slice(&raw[offset..offset + chunk_len]);
        offset += chunk_len;
    }
    let adler = adler32(&raw);
    zlib.extend_from_slice(&adler.to_be_bytes());

    let mut out = Vec::with_capacity(zlib.len() + 64);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");

    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8; // bit depth
    ihdr[9] = 6; // color type RGBA
    ihdr[10] = 0; // compression
    ihdr[11] = 0; // filter
    ihdr[12] = 0; // interlace
    write_png_chunk(&mut out, b"IHDR", &ihdr);
    write_png_chunk(&mut out, b"IDAT", &zlib);
    write_png_chunk(&mut out, b"IEND", &[]);
    out
}

fn write_png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = 0xffff_ffffu32;
    crc = crc32_update(crc, kind);
    crc = crc32_update(crc, data);
    out.extend_from_slice(&(!crc).to_be_bytes());
}

fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xedb8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

fn adler32(bytes: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in bytes {
        a = (a + u32::from(byte)) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}
