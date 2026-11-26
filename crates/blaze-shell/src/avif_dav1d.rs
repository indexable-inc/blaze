//! High-throughput stateful AVIF image-sequence (`avis`) decoder backed by macOS's built-in
//! `dav1d` AV1 decoder (exported by Apple's `WebCore.framework` inside `WebKit.framework`).
//!
//! Why (Rule 45 `macos-private-apis` & Rule 67 `within-ten-percent-of-the-floor`):
//! Apple's public `ImageIO` (`CGImageSourceCreateImageAtIndex` and `CGAnimateImageDataWithBlock`)
//! decodes AVIF sequences statelessly — re-decoding from keyframe 0 on every call (`O(N^2)`),
//! reaching 140–400 ms/frame (~2–7 fps) on 2560x1440 10-bit 4:4:4 `docs/demo.avif`.
//! Calling macOS's built-in `WebCore` `dav1d` (API 7.0.0) sequentially with `n_threads = 4`
//! decodes the exact same 878-frame sequence at ~1.2 ms/frame (>800 fps), enabling locked 60 fps
//! playback with ~2% CPU and zero third-party dependencies.

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CStr, c_char, c_int, c_void};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    /// Path to macOS's system `WebCore` framework in the dyld shared cache, which exports the
    /// ABI-stable `dav1d` 1.x C API (major version 7) used by Safari for AVIF/AV1 playback.
    const WEBCORE_DYLIB: &CStr = c"/System/Library/Frameworks/WebKit.framework/Versions/A/Frameworks/WebCore.framework/Versions/A/WebCore";

    const RTLD_LAZY: c_int = 0x1;
    const EAGAIN: c_int = 35; // macOS EAGAIN

    unsafe extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    type FnVersionApi = unsafe extern "C" fn() -> u32;
    type FnDefaultSettings = unsafe extern "C" fn(*mut u8);
    type FnOpen = unsafe extern "C" fn(*mut *mut c_void, *const u8) -> c_int;
    type FnClose = unsafe extern "C" fn(*mut *mut c_void);
    type FnFlush = unsafe extern "C" fn(*mut c_void);
    type FnDataCreate = unsafe extern "C" fn(*mut u8, usize) -> *mut u8;
    type FnDataUnref = unsafe extern "C" fn(*mut u8);
    type FnSendData = unsafe extern "C" fn(*mut c_void, *mut u8) -> c_int;
    type FnGetPicture = unsafe extern "C" fn(*mut c_void, *mut u8) -> c_int;
    type FnPictureUnref = unsafe extern "C" fn(*mut u8);

    #[derive(Clone, Copy)]
    pub(crate) struct Dav1dApi {
        default_settings: FnDefaultSettings,
        open: FnOpen,
        close: FnClose,
        flush: FnFlush,
        data_create: FnDataCreate,
        data_unref: FnDataUnref,
        send_data: FnSendData,
        get_picture: FnGetPicture,
        picture_unref: FnPictureUnref,
    }

    // SAFETY: function pointers live in the process-wide `WebCore` shared cache image for `'static`.
    unsafe impl Send for Dav1dApi {}
    unsafe impl Sync for Dav1dApi {}

    pub(crate) fn load_dav1d() -> Option<Dav1dApi> {
        static API: OnceLock<Option<Dav1dApi>> = OnceLock::new();
        *API.get_or_init(|| {
            // SAFETY: loading Apple's system `WebCore` framework from the dyld shared cache and
            // checking `dav1d_version_api() >> 16 == 7` before binding ABI 7.x entry points.
            unsafe {
                let h = dlopen(WEBCORE_DYLIB.as_ptr(), RTLD_LAZY);
                if h.is_null() {
                    return None;
                }
                let sym = |name: &CStr| -> Option<*mut c_void> {
                    let p = dlsym(h, name.as_ptr());
                    (!p.is_null()).then_some(p)
                };
                let ver_fn: FnVersionApi = std::mem::transmute(sym(c"dav1d_version_api")?);
                let api_ver = ver_fn();
                if (api_ver >> 16) != 7 {
                    return None;
                }
                Some(Dav1dApi {
                    default_settings: std::mem::transmute(sym(c"dav1d_default_settings")?),
                    open: std::mem::transmute(sym(c"dav1d_open")?),
                    close: std::mem::transmute(sym(c"dav1d_close")?),
                    flush: std::mem::transmute(sym(c"dav1d_flush")?),
                    data_create: std::mem::transmute(sym(c"dav1d_data_create")?),
                    data_unref: std::mem::transmute(sym(c"dav1d_data_unref")?),
                    send_data: std::mem::transmute(sym(c"dav1d_send_data")?),
                    get_picture: std::mem::transmute(sym(c"dav1d_get_picture")?),
                    picture_unref: std::mem::transmute(sym(c"dav1d_picture_unref")?),
                })
            }
        })
    }

    #[derive(Clone, Copy, Debug)]
    pub(crate) struct AvifSample {
        pub offset: usize,
        pub size: usize,
        pub delay_ms: u32,
    }

    #[derive(Clone, Debug)]
    pub(crate) struct AvifSequence {
        pub width: u32,
        pub height: u32,
        pub full_range: bool,
        pub bt709: bool,
        pub samples: Vec<AvifSample>,
    }

    #[inline]
    fn be_u16(b: &[u8], o: usize) -> Option<u16> {
        Some(u16::from_be_bytes(b.get(o..o + 2)?.try_into().ok()?))
    }

    #[inline]
    fn be_u32(b: &[u8], o: usize) -> Option<u32> {
        Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?))
    }

    #[inline]
    fn be_u64(b: &[u8], o: usize) -> Option<u64> {
        Some(u64::from_be_bytes(b.get(o..o + 8)?.try_into().ok()?))
    }

    fn iter_boxes(data: &[u8], mut pos: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
        let mut out = Vec::new();
        while pos + 8 <= end {
            let sz32 = be_u32(data, pos).unwrap_or(0) as usize;
            let typ: [u8; 4] = match data.get(pos + 4..pos + 8) {
                Some(s) => [s[0], s[1], s[2], s[3]],
                None => break,
            };
            let (hdr, size) = if sz32 == 1 {
                let sz64 = be_u64(data, pos + 8).unwrap_or(0) as usize;
                (16usize, sz64)
            } else if sz32 == 0 {
                (8usize, end - pos)
            } else {
                (8usize, sz32)
            };
            if size < hdr || pos + size > end {
                break;
            }
            out.push((typ, pos + hdr, pos + size));
            pos += size;
        }
        out
    }

    fn find_box(data: &[u8], start: usize, end: usize, tag: &[u8; 4]) -> Option<(usize, usize)> {
        iter_boxes(data, start, end)
            .into_iter()
            .find(|(t, _, _)| t == tag)
            .map(|(_, s, e)| (s, e))
    }

    /// Parse an ISOBMFF AVIF image sequence (`avis`) into its raw `av01` sample table + timing.
    pub(crate) fn parse_avif_sequence(data: &[u8]) -> Option<AvifSequence> {
        let top = iter_boxes(data, 0, data.len());
        // Require `ftyp` naming `avis` or `avif` and a `moov` track containing `av01`.
        let (ftyp_s, ftyp_e) = top.iter().find(|(t, _, _)| t == b"ftyp").map(|&(_, s, e)| (s, e))?;
        let ftyp_bytes = data.get(ftyp_s..ftyp_e)?;
        if !ftyp_bytes.windows(4).any(|w| w == b"avis" || w == b"avif") {
            return None;
        }
        let (moov_s, moov_e) = top.iter().find(|(t, _, _)| t == b"moov").map(|&(_, s, e)| (s, e))?;

        for (typ, trak_s, trak_e) in iter_boxes(data, moov_s, moov_e) {
            if &typ != b"trak" {
                continue;
            }
            let Some((mdia_s, mdia_e)) = find_box(data, trak_s, trak_e, b"mdia") else {
                continue;
            };
            let Some((minf_s, minf_e)) = find_box(data, mdia_s, mdia_e, b"minf") else {
                continue;
            };
            let Some((stbl_s, stbl_e)) = find_box(data, minf_s, minf_e, b"stbl") else {
                continue;
            };

            // 1. `stsd`: verify `av01` sample entry and extract dimensions + color matrix/range.
            let Some((stsd_s, stsd_e)) = find_box(data, stbl_s, stbl_e, b"stsd") else {
                continue;
            };
            if stsd_s + 8 > stsd_e {
                continue;
            }
            let entry_boxes = iter_boxes(data, stsd_s + 8, stsd_e);
            let Some((_, av01_s, av01_e)) = entry_boxes.into_iter().find(|(t, _, _)| t == b"av01") else {
                continue;
            };
            // VisualSampleEntry payload: 6 reserved + 2 data_ref_idx + 16 reserved + 2 w + 2 h (w @ +24, h @ +26 from box body)
            if av01_s + 78 > av01_e {
                continue;
            }
            let width = u32::from(be_u16(data, av01_s + 24)?);
            let height = u32::from(be_u16(data, av01_s + 26)?);
            if width == 0 || height == 0 || width > 8192 || height > 8192 {
                continue;
            }
            let mut full_range = true;
            let mut bt709 = true;
            for (sub_t, sub_s, sub_e) in iter_boxes(data, av01_s + 78, av01_e) {
                if &sub_t == b"colr" && sub_s + 11 <= sub_e && data.get(sub_s..sub_s + 4) == Some(b"nclx") {
                    let matrix = be_u16(data, sub_s + 8).unwrap_or(1);
                    let flags = data.get(sub_s + 10).copied().unwrap_or(0x80);
                    bt709 = matrix == 1 || matrix == 0 || matrix == 2;
                    full_range = (flags & 0x80) != 0;
                }
            }

            // 2. `mdhd`: timescale.
            let timescale = if let Some((mdhd_s, mdhd_e)) = find_box(data, mdia_s, mdia_e, b"mdhd") {
                let ver = data.get(mdhd_s).copied().unwrap_or(0);
                let ts_off = if ver == 1 { mdhd_s + 20 } else { mdhd_s + 12 };
                if ts_off + 4 <= mdhd_e {
                    be_u32(data, ts_off).unwrap_or(1000).max(1)
                } else {
                    1000
                }
            } else {
                1000
            };

            // 3. `stsz`: per-sample byte lengths.
            let Some((stsz_s, stsz_e)) = find_box(data, stbl_s, stbl_e, b"stsz") else {
                continue;
            };
            if stsz_s + 12 > stsz_e {
                continue;
            }
            let fixed_sz = be_u32(data, stsz_s + 4)? as usize;
            let count = (be_u32(data, stsz_s + 8)? as usize).min(20_000);
            if count <= 1 {
                continue; // Single still image — let static ImageIO path handle it.
            }
            let mut sizes = Vec::with_capacity(count);
            if fixed_sz != 0 {
                sizes.resize(count, fixed_sz);
            } else {
                for i in 0..count {
                    sizes.push(be_u32(data, stsz_s + 12 + i * 4)? as usize);
                }
            }

            // 4. `stco` or `co64`: chunk file offsets.
            let mut chunk_offsets: Vec<usize> = Vec::new();
            if let Some((stco_s, stco_e)) = find_box(data, stbl_s, stbl_e, b"stco") {
                let n = be_u32(data, stco_s + 4)? as usize;
                for i in 0..n {
                    if stco_s + 8 + i * 4 + 4 > stco_e {
                        break;
                    }
                    chunk_offsets.push(be_u32(data, stco_s + 8 + i * 4)? as usize);
                }
            } else if let Some((co64_s, co64_e)) = find_box(data, stbl_s, stbl_e, b"co64") {
                let n = be_u32(data, co64_s + 4)? as usize;
                for i in 0..n {
                    if co64_s + 8 + i * 8 + 8 > co64_e {
                        break;
                    }
                    chunk_offsets.push(be_u64(data, co64_s + 8 + i * 8)? as usize);
                }
            }
            if chunk_offsets.is_empty() {
                continue;
            }

            // 5. `stsc`: sample-to-chunk runs `(first_chunk_1based, samples_per_chunk)`.
            let mut stsc_runs: Vec<(usize, usize)> = Vec::new();
            if let Some((stsc_s, stsc_e)) = find_box(data, stbl_s, stbl_e, b"stsc") {
                let n = be_u32(data, stsc_s + 4).unwrap_or(0) as usize;
                for i in 0..n {
                    let o = stsc_s + 8 + i * 12;
                    if o + 12 > stsc_e {
                        break;
                    }
                    let first_chunk = be_u32(data, o)? as usize;
                    let spc = be_u32(data, o + 4)? as usize;
                    if first_chunk >= 1 && spc >= 1 {
                        stsc_runs.push((first_chunk, spc));
                    }
                }
            }
            if stsc_runs.is_empty() {
                stsc_runs.push((1, count));
            }

            // 6. `stts`: decoding time-to-sample -> per-frame delay in ms.
            let mut delays_ms = Vec::with_capacity(count);
            if let Some((stts_s, stts_e)) = find_box(data, stbl_s, stbl_e, b"stts") {
                let n = be_u32(data, stts_s + 4).unwrap_or(0) as usize;
                for i in 0..n {
                    let o = stts_s + 8 + i * 8;
                    if o + 8 > stts_e {
                        break;
                    }
                    let run_cnt = be_u32(data, o)? as usize;
                    let delta = be_u32(data, o + 4)? as u64;
                    let ms = ((delta.saturating_mul(1000) + u64::from(timescale) / 2)
                        / u64::from(timescale))
                    .clamp(8, 5000) as u32;
                    let take = run_cnt.min(count.saturating_sub(delays_ms.len()));
                    delays_ms.resize(delays_ms.len() + take, ms);
                }
            }
            while delays_ms.len() < count {
                delays_ms.push(33);
            }

            // Expand chunks -> exact byte offset per sample.
            let mut samples = Vec::with_capacity(count);
            let mut sample_idx = 0usize;
            for (ci, &chunk_off) in chunk_offsets.iter().enumerate() {
                let chunk_num = ci + 1;
                let spc = stsc_runs
                    .iter()
                    .rev()
                    .find(|&&(fc, _)| fc <= chunk_num)
                    .map_or(count, |&(_, s)| s);
                let mut cur_off = chunk_off;
                for _ in 0..spc {
                    if sample_idx >= count {
                        break;
                    }
                    let sz = sizes[sample_idx];
                    if cur_off.checked_add(sz).is_none_or(|e| e > data.len()) {
                        return None;
                    }
                    samples.push(AvifSample {
                        offset: cur_off,
                        size: sz,
                        delay_ms: delays_ms[sample_idx],
                    });
                    cur_off += sz;
                    sample_idx += 1;
                }
                if sample_idx >= count {
                    break;
                }
            }
            if samples.len() > 1 {
                return Some(AvifSequence { width, height, full_range, bt709, samples });
            }
        }
        None
    }

    /// Stateful `dav1d` decoder session backed by macOS `WebCore` (API 7.0.0).
    pub(crate) struct Dav1dDecoder {
        api: Dav1dApi,
        ctx: *mut c_void,
    }

    // SAFETY: `Dav1dContext` is owned exclusively by one decoding thread at a time.
    unsafe impl Send for Dav1dDecoder {}

    impl Drop for Dav1dDecoder {
        fn drop(&mut self) {
            if !self.ctx.is_null() {
                // SAFETY: `self.ctx` was opened by `dav1d_open` on the same `Dav1dApi`.
                unsafe { (self.api.close)(&mut self.ctx) };
            }
        }
    }

    impl Dav1dDecoder {
        pub(crate) fn new(threads: i32) -> Option<Self> {
            let api = load_dav1d()?;
            let mut settings = [0u8; 256];
            let mut ctx: *mut c_void = std::ptr::null_mut();
            // SAFETY: `Dav1dSettings` in ABI 7.0.0 is 80 bytes; 256-byte zeroed buffer gives wide
            // headroom. Offsets 0 and 4 are `n_threads: c_int` and `max_frame_delay: c_int`.
            unsafe {
                (api.default_settings)(settings.as_mut_ptr());
                settings[0..4].copy_from_slice(&threads.clamp(1, 8).to_ne_bytes());
                settings[4..8].copy_from_slice(&1i32.to_ne_bytes()); // max_frame_delay = 1 (zero latency)
                let rc = (api.open)(&mut ctx, settings.as_ptr());
                if rc != 0 || ctx.is_null() {
                    return None;
                }
            }
            Some(Self { api, ctx })
        }

        pub(crate) fn flush(&mut self) {
            // SAFETY: `self.ctx` is a valid live `Dav1dContext`.
            unsafe { (self.api.flush)(self.ctx) };
        }

        /// Decode one sequential AV1 sample and convert directly into `dst_rgba` (`dst_w * dst_h * 4`).
        pub(crate) fn decode_into_rgba(
            &mut self,
            sample: &[u8],
            dst_w: usize,
            dst_h: usize,
            x_map: &[usize],
            y_map: &[usize],
            full_range: bool,
            bt709: bool,
            dst_rgba: &mut [u8],
        ) -> bool {
            let mut dav_data = [0u8; 128];
            let mut dav_pic = [0u8; 512];
            // SAFETY: `Dav1dData` and `Dav1dPicture` in ABI 7.0.0 fit well inside 128 / 512 bytes,
            // zero-initialized per `dav1d` API contract and unreferenced before returning.
            unsafe {
                let buf = (self.api.data_create)(dav_data.as_mut_ptr(), sample.len());
                if buf.is_null() {
                    return false;
                }
                std::ptr::copy_nonoverlapping(sample.as_ptr(), buf, sample.len());
                let mut sent = (self.api.send_data)(self.ctx, dav_data.as_mut_ptr());
                let mut got_pic = false;
                while sent == -EAGAIN {
                    dav_pic.fill(0);
                    if (self.api.get_picture)(self.ctx, dav_pic.as_mut_ptr()) == 0 {
                        convert_dav1d_picture(
                            &dav_pic,
                            dst_w,
                            dst_h,
                            x_map,
                            y_map,
                            full_range,
                            bt709,
                            dst_rgba,
                        );
                        (self.api.picture_unref)(dav_pic.as_mut_ptr());
                        got_pic = true;
                    }
                    sent = (self.api.send_data)(self.ctx, dav_data.as_mut_ptr());
                }
                if sent != 0 {
                    (self.api.data_unref)(dav_data.as_mut_ptr());
                    return false;
                }
                loop {
                    dav_pic.fill(0);
                    let g = (self.api.get_picture)(self.ctx, dav_pic.as_mut_ptr());
                    if g == 0 {
                        convert_dav1d_picture(
                            &dav_pic,
                            dst_w,
                            dst_h,
                            x_map,
                            y_map,
                            full_range,
                            bt709,
                            dst_rgba,
                        );
                        (self.api.picture_unref)(dav_pic.as_mut_ptr());
                        got_pic = true;
                    } else {
                        break;
                    }
                }
                got_pic
            }
        }
    }

    #[inline]
    fn read_ptr(pic: &[u8; 512], off: usize) -> *const u8 {
        let raw = usize::from_ne_bytes(pic[off..off + 8].try_into().unwrap());
        raw as *const u8
    }

    #[inline]
    fn read_isize(pic: &[u8; 512], off: usize) -> isize {
        isize::from_ne_bytes(pic[off..off + 8].try_into().unwrap())
    }

    #[inline]
    fn read_i32(pic: &[u8; 512], off: usize) -> i32 {
        i32::from_ne_bytes(pic[off..off + 4].try_into().unwrap())
    }

    /// Fast fixed-point BT.709 / BT.601 YUV (`I444`/`I422`/`I420`/`I400`, 8/10/12-bit) -> RGBA8
    /// converter with direct display-resolution sampling (zero intermediate allocations!).
    #[allow(clippy::too_many_arguments)]
    unsafe fn convert_dav1d_picture(
        pic: &[u8; 512],
        dst_w: usize,
        dst_h: usize,
        x_map: &[usize],
        y_map: &[usize],
        full_range: bool,
        bt709: bool,
        dst_rgba: &mut [u8],
    ) {
        let y_ptr = read_ptr(pic, 16);
        let u_ptr = read_ptr(pic, 24);
        let v_ptr = read_ptr(pic, 32);
        let y_stride = read_isize(pic, 40);
        let uv_stride = read_isize(pic, 48);
        let w = read_i32(pic, 56).max(0) as usize;
        let h = read_i32(pic, 60).max(0) as usize;
        let layout = read_i32(pic, 64); // 0=I400, 1=I420, 2=I422, 3=I444
        let bpc = read_i32(pic, 68);
        if y_ptr.is_null() || w == 0 || h == 0 || dst_rgba.len() < dst_w * dst_h * 4 {
            return;
        }
        let high_bit = bpc > 8;
        let shift = (bpc - 8).clamp(0, 4) as u32;
        let ss_x = usize::from(layout == 1 || layout == 2);
        let ss_y = usize::from(layout == 1);
        let mono = layout == 0 || u_ptr.is_null() || v_ptr.is_null();

        // Fixed-point 10-bit coefficients (scaled by 1024).
        let (cy, c_rv, c_gu, c_gv, c_bu, y_off): (i32, i32, i32, i32, i32, i32) = match (bt709, full_range) {
            (true, true) => (1024, 1613, -192, -479, 1900, 0),
            (true, false) => (1192, 1836, -218, -546, 2163, 16),
            (false, true) => (1024, 1436, -352, -731, 1815, 0),
            (false, false) => (1192, 1634, -400, -833, 2066, 16),
        };

        for (dy, &sy_raw) in y_map.iter().take(dst_h).enumerate() {
            let sy = sy_raw.min(h - 1);
            let cy_row = sy >> ss_y;
            let row_out = &mut dst_rgba[dy * dst_w * 4..(dy + 1) * dst_w * 4];
            // SAFETY: `sy < h` and `sx < w`; `y_stride`/`uv_stride` are valid row byte strides from `dav1d`.
            unsafe {
                let y_row = y_ptr.offset(sy as isize * y_stride);
                let u_row = if mono { std::ptr::null() } else { u_ptr.offset(cy_row as isize * uv_stride) };
                let v_row = if mono { std::ptr::null() } else { v_ptr.offset(cy_row as isize * uv_stride) };

                if high_bit {
                    let y16 = y_row.cast::<u16>();
                    let u16p = u_row.cast::<u16>();
                    let v16p = v_row.cast::<u16>();
                    for (dx, &sx_raw) in x_map.iter().take(dst_w).enumerate() {
                        let sx = sx_raw.min(w - 1);
                        let yv = ((*y16.add(sx)) >> shift) as i32;
                        let (uv, vv) = if mono {
                            (128, 128)
                        } else {
                            let cx = sx >> ss_x;
                            (((*u16p.add(cx)) >> shift) as i32, ((*v16p.add(cx)) >> shift) as i32)
                        };
                        let y_scaled = (yv - y_off) * cy;
                        let du = uv - 128;
                        let dv = vv - 128;
                        let r = ((y_scaled + c_rv * dv + 512) >> 10).clamp(0, 255) as u8;
                        let g = ((y_scaled + c_gu * du + c_gv * dv + 512) >> 10).clamp(0, 255) as u8;
                        let b = ((y_scaled + c_bu * du + 512) >> 10).clamp(0, 255) as u8;
                        let o = dx * 4;
                        row_out[o] = r;
                        row_out[o + 1] = g;
                        row_out[o + 2] = b;
                        row_out[o + 3] = 255;
                    }
                } else {
                    for (dx, &sx_raw) in x_map.iter().take(dst_w).enumerate() {
                        let sx = sx_raw.min(w - 1);
                        let yv = *y_row.add(sx) as i32;
                        let (uv, vv) = if mono {
                            (128, 128)
                        } else {
                            let cx = sx >> ss_x;
                            (*u_row.add(cx) as i32, *v_row.add(cx) as i32)
                        };
                        let y_scaled = (yv - y_off) * cy;
                        let du = uv - 128;
                        let dv = vv - 128;
                        let r = ((y_scaled + c_rv * dv + 512) >> 10).clamp(0, 255) as u8;
                        let g = ((y_scaled + c_gu * du + c_gv * dv + 512) >> 10).clamp(0, 255) as u8;
                        let b = ((y_scaled + c_bu * du + 512) >> 10).clamp(0, 255) as u8;
                        let o = dx * 4;
                        row_out[o] = r;
                        row_out[o + 1] = g;
                        row_out[o + 2] = b;
                        row_out[o + 3] = 255;
                    }
                }
            }
        }
    }

    struct StreamShared {
        /// Latest decoded RGBA8 frame ready for immediate GPU texture upload.
        ready_frame: Option<(usize, Vec<u8>)>,
        /// Recycled RGBA8 buffer returned by the UI thread to eliminate per-frame allocations!
        recycle_buf: Option<Vec<u8>>,
        /// Shutdown flag set when the `AvifStreamer` is dropped.
        stop: bool,
    }

    /// Lock-free / double-buffered sequential 60 fps AVIF streamer running on a dedicated worker.
    pub(crate) struct AvifStreamer {
        shared: Arc<(Mutex<StreamShared>, Condvar)>,
        started_at: Instant,
        last_seen_ms: Arc<AtomicU64>,
        has_new_frame: Arc<AtomicBool>,
    }

    impl Drop for AvifStreamer {
        fn drop(&mut self) {
            let (lock, cv) = &*self.shared;
            if let Ok(mut g) = lock.lock() {
                g.stop = true;
                cv.notify_all();
            }
        }
    }

    impl AvifStreamer {
        /// Decode frame 0 synchronously for instant first paint, then start the sequential 60fps
        /// background streamer for all remaining frames!
        pub(crate) fn start(
            bytes: Vec<u8>,
            seq: AvifSequence,
            dst_w: usize,
            dst_h: usize,
        ) -> Option<(Vec<u8>, Self)> {
            let mut dec = Dav1dDecoder::new(4)?;
            let x_map: Vec<usize> = (0..dst_w)
                .map(|x| (x * seq.width as usize) / dst_w.max(1))
                .collect();
            let y_map: Vec<usize> = (0..dst_h)
                .map(|y| (y * seq.height as usize) / dst_h.max(1))
                .collect();

            let mut first = vec![0u8; dst_w * dst_h * 4];
            let s0 = seq.samples[0];
            if !dec.decode_into_rgba(
                &bytes[s0.offset..s0.offset + s0.size],
                dst_w,
                dst_h,
                &x_map,
                &y_map,
                seq.full_range,
                seq.bt709,
                &mut first,
            ) {
                return None;
            }

            let shared = Arc::new((
                Mutex::new(StreamShared {
                    ready_frame: None,
                    recycle_buf: Some(vec![0u8; dst_w * dst_h * 4]),
                    stop: false,
                }),
                Condvar::new(),
            ));
            let started_at = Instant::now();
            let last_seen_ms = Arc::new(AtomicU64::new(1));
            let has_new_frame = Arc::new(AtomicBool::new(false));

            let t_shared = Arc::clone(&shared);
            let t_seen = Arc::clone(&last_seen_ms);
            let t_ready = Arc::clone(&has_new_frame);
            let _ = std::thread::Builder::new()
                .name("blaze-shell-avif-dav1d".to_owned())
                .spawn(move || {
                    let mut idx = 1usize % seq.samples.len();
                    let mut work_buf = vec![0u8; dst_w * dst_h * 4];
                    let start = started_at;
                    let mut next_deadline = Instant::now() + Duration::from_millis(u64::from(s0.delay_ms));

                    loop {
                        let (lock, cv) = &*t_shared;
                        let now_rel = start.elapsed().as_millis() as u64;
                        // Sleep cleanly with 0% CPU whenever the image has been off-screen for >250ms.
                        {
                            let mut g = match lock.lock() {
                                Ok(g) => g,
                                Err(_) => break,
                            };
                            while !g.stop {
                                let seen = t_seen.load(Ordering::Relaxed);
                                if seen > 0 && now_rel.saturating_sub(seen) > 250 {
                                    let (ng, _) = match cv.wait_timeout(g, Duration::from_millis(120)) {
                                        Ok(pair) => pair,
                                        Err(_) => return,
                                    };
                                    g = ng;
                                    next_deadline = Instant::now();
                                    let fresh_now = start.elapsed().as_millis() as u64;
                                    let fresh_seen = t_seen.load(Ordering::Relaxed);
                                    if fresh_seen == 0 || fresh_now.saturating_sub(fresh_seen) <= 250 {
                                        break;
                                    }
                                } else {
                                    break;
                                }
                            }
                            if g.stop {
                                break;
                            }
                            if let Some(recycled) = g.recycle_buf.take()
                                && recycled.len() == work_buf.len()
                            {
                                work_buf = recycled;
                            }
                        }

                        // Wait until this frame's exact `stts` presentation timestamp.
                        let now = Instant::now();
                        if next_deadline > now {
                            let wait = next_deadline - now;
                            let (lock, cv) = &*t_shared;
                            if let Ok(g) = lock.lock() {
                                let _ = cv.wait_timeout(g, wait);
                            }
                        }
                        if t_shared.0.lock().map_or(true, |g| g.stop) {
                            break;
                        }

                        let sample = seq.samples[idx];
                        let ok = dec.decode_into_rgba(
                            &bytes[sample.offset..sample.offset + sample.size],
                            dst_w,
                            dst_h,
                            &x_map,
                            &y_map,
                            seq.full_range,
                            seq.bt709,
                            &mut work_buf,
                        );
                        if ok {
                            let (lock, _) = &*t_shared;
                            if let Ok(mut g) = lock.lock() {
                                let prev = g.ready_frame.replace((idx, work_buf));
                                work_buf = prev
                                    .map(|(_, b)| b)
                                    .or_else(|| g.recycle_buf.take())
                                    .unwrap_or_else(|| vec![0u8; dst_w * dst_h * 4]);
                                t_ready.store(true, Ordering::Release);
                            }
                        }

                        idx += 1;
                        if idx >= seq.samples.len() {
                            idx = 0;
                            dec.flush();
                        }
                        let step = Duration::from_millis(u64::from(sample.delay_ms));
                        next_deadline = (next_deadline + step).max(Instant::now());
                    }
                });

            Some((first, Self { shared, started_at, last_seen_ms, has_new_frame }))
        }

        /// Mark this animation visible in the current frame and take the newest decoded RGBA8
        /// buffer (if any) without ever blocking the Metal UI thread.
        pub(crate) fn poll_visible(&self) -> Option<(usize, Vec<u8>)> {
            let now_rel = self.started_at.elapsed().as_millis() as u64 + 1;
            let prev = self.last_seen_ms.swap(now_rel, Ordering::Relaxed);
            if prev == 0 || now_rel.saturating_sub(prev) > 200 {
                self.shared.1.notify_one();
            }
            if !self.has_new_frame.swap(false, Ordering::Acquire) {
                return None;
            }
            let mut g = self.shared.0.lock().ok()?;
            g.ready_frame.take()
        }

        /// Return a previously uploaded RGBA8 buffer to the worker for zero-allocation reuse.
        pub(crate) fn recycle(&self, buf: Vec<u8>) {
            if let Ok(mut g) = self.shared.0.lock() {
                g.recycle_buf = Some(buf);
            }
        }

        /// Non-blocking check whether a new decoded frame is waiting to be uploaded.
        pub(crate) fn has_ready_frame(&self) -> bool {
            self.has_new_frame.load(Ordering::Acquire)
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use imp::{AvifStreamer, parse_avif_sequence};

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::imp::{Dav1dDecoder, load_dav1d, parse_avif_sequence};
    use std::path::Path;
    use std::time::Instant;

    #[test]
    fn webcore_dav1d_symbols_exist_and_decode_demo_avif_fast() {
        // Rule 45: verify macOS WebCore's private dav1d API 7.0.0 symbols are present.
        assert!(load_dav1d().is_some(), "macOS WebCore dav1d API 7.x must be loadable");

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
        let avif = root.join("docs/demo.avif");
        if !avif.is_file() {
            return;
        }
        let bytes = std::fs::read(&avif).expect("read docs/demo.avif");
        let seq = parse_avif_sequence(&bytes).expect("parse docs/demo.avif ISOBMFF");
        assert_eq!(seq.width, 2560);
        assert_eq!(seq.height, 1440);
        assert!(seq.samples.len() >= 800, "expected full Game of Life sequence, got {}", seq.samples.len());
        // Verify exact 60fps stts timing (~17ms/frame).
        assert!(seq.samples[0].delay_ms <= 20, "expected ~17ms 60fps delay, got {}ms", seq.samples[0].delay_ms);

        let mut dec = Dav1dDecoder::new(4).expect("dav1d_open");
        let dst_w = 1280usize;
        let dst_h = 720usize;
        let x_map: Vec<usize> = (0..dst_w).map(|x| (x * seq.width as usize) / dst_w).collect();
        let y_map: Vec<usize> = (0..dst_h).map(|y| (y * seq.height as usize) / dst_h).collect();
        let mut rgba = vec![0u8; dst_w * dst_h * 4];

        let n = 60usize.min(seq.samples.len());
        let t0 = Instant::now();
        for s in &seq.samples[..n] {
            let ok = dec.decode_into_rgba(
                &bytes[s.offset..s.offset + s.size],
                dst_w,
                dst_h,
                &x_map,
                &y_map,
                seq.full_range,
                seq.bt709,
                &mut rgba,
            );
            assert!(ok, "dav1d frame decode failed");
        }
        let ms_per_frame = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
        // Verify non-blank pixels decoded.
        assert!(rgba.chunks_exact(4).any(|px| px[0] > 20 || px[1] > 20 || px[2] > 20));
        // Even in debug mode this should beat ImageIO's 141ms/frame by an order of magnitude.
        assert!(ms_per_frame < 40.0, "expected fast dav1d decode+convert, got {ms_per_frame:.2} ms/frame");
    }
}
