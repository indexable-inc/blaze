//! Minimal zero-handwritten-WGSL WebGPU host HTML for `blaze-site-wasm`.
//!
//! Fetches the zero-import `blaze_site.wasm` module, `page.json`, and `BZG1` Slug font table,
//! reads the linked WGSL shader emitted directly by Rust `gpu::slug::emit_slug_site_wgsl`,
//! renders the entire front page + `den` Slug vector outlines + hover cards + native WebGPU
//! `Cmd+F` Find Bar + Zoom HUD Pill in ONE single WebGPU draw call, and wires smooth macOS
//! trackpad pinch-to-zoom (`wheel` `ctrlKey` + Safari `gesturechange`) so typography stays
//! razor-sharp at any zoom dimension (`25%`–`1600%`).

pub const SITE_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1,user-scalable=no">
<title>Blaze — Pure-Blaze WebGPU + Slug Font Engine</title>
<style>
  html, body {
    margin: 0;
    padding: 0;
    width: 100%;
    height: 100%;
    overflow: hidden;
    background: #101011;
    touch-action: none;
    overscroll-behavior: none;
    user-select: none;
    -webkit-user-select: none;
  }
  canvas {
    display: block;
    width: 100vw;
    height: 100vh;
    outline: none;
  }
  #err {
    position: fixed;
    inset: 24px;
    color: #ebeae6;
    font: 14px/1.6 system-ui, sans-serif;
    display: none;
    white-space: pre-wrap;
  }
</style>
</head>
<body>
<canvas id="c" tabindex="0"></canvas>
<div id="err"></div>
<script type="module">
const canvas = document.getElementById('c');
const errEl = document.getElementById('err');
const showErr = (msg) => { errEl.style.display = 'block'; errEl.textContent = String(msg); };
const td = new TextDecoder();

async function boot() {
  if (!navigator.gpu) { showErr('WebGPU is not available in this browser.'); return; }
  const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
  if (!adapter) { showErr('No WebGPU adapter available.'); return; }
  const device = await adapter.requestDevice();
  const format = navigator.gpu.getPreferredCanvasFormat();
  const ctx = canvas.getContext('webgpu');
  ctx.configure({ device, format, alphaMode: 'premultiplied' });

  const [wasmRes, pageRes, fontRes] = await Promise.all([
    fetch('/blaze_site.wasm'),
    fetch('/page.json'),
    fetch('/fonts/text.bin'),
  ]);
  const [wasmBytes, pageBytes, fontBytes] = await Promise.all([
    wasmRes.arrayBuffer(),
    pageRes.arrayBuffer(),
    fontRes.arrayBuffer(),
  ]);

  const { instance } = await WebAssembly.instantiate(wasmBytes, {});
  const w = instance.exports;
  const mem = () => w.memory.buffer;
  const readStr = (ptr, len) => td.decode(new Uint8Array(mem(), ptr, len));

  const h = w.site_create();
  if (!h) { showErr('site_create() returned null'); return; }

  // Copy font table & page JSON into WASM scratch buffer
  const uploadScratch = (ab) => {
    const src = new Uint8Array(ab);
    const dstPtr = w.site_scratch_alloc(h, src.byteLength);
    new Uint8Array(mem(), dstPtr, src.byteLength).set(src);
    return src.byteLength;
  };
  if (!w.site_load_font_table_from_scratch(h, uploadScratch(fontBytes))) {
    showErr('Failed to decode BZG1 Slug font table');
    return;
  }
  if (!w.site_load_page_json_from_scratch(h, uploadScratch(pageBytes))) {
    showErr('Failed to parse page.json');
    return;
  }

  // Sync system dark/light preference
  const lightMq = window.matchMedia('(prefers-color-scheme: light)');
  const applyThemePref = () => {
    w.site_set_dark(h, lightMq.matches ? 0 : 1);
    document.body.style.background = lightMq.matches ? '#f2f1ee' : '#101011';
  };
  applyThemePref();
  lightMq.addEventListener('change', () => { applyThemePref(); dirty = true; kick(); });

  // Read Rust-emitted WGSL shader straight from WASM (ZERO handwritten WGSL!)
  const wgsl = readStr(w.site_wgsl_ptr(h), w.site_wgsl_len(h));
  const module = device.createShaderModule({ code: wgsl });
  const info = await module.getCompilationInfo();
  const wgslErrors = info.messages.filter((m) => m.type === 'error');
  if (wgslErrors.length) {
    showErr('WGSL compile error:\n' + wgslErrors.map((e) => `${e.lineNum}:${e.linePos} ${e.message}`).join('\n'));
    return;
  }

  const pipeline = device.createRenderPipeline({
    layout: 'auto',
    vertex: { module, entryPoint: 'slug_site_vs' },
    fragment: {
      module,
      entryPoint: 'slug_site_fs',
      targets: [{
        format,
        blend: {
          color: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' },
          alpha: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' },
        },
      }],
    },
    primitive: { topology: 'triangle-list' },
  });

  // Upload `den`-format Slug `curves` & `bands` GPU storage buffers ONCE!
  const curvesLen = w.site_curves_len(h);
  const curvesPtr = w.site_curves_ptr(h);
  const curvesBuf = device.createBuffer({
    size: Math.max(16, curvesLen * 16),
    usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
  });
  device.queue.writeBuffer(curvesBuf, 0, new Float32Array(mem(), curvesPtr, curvesLen * 4));

  const bandsLen = w.site_bands_len(h);
  const bandsPtr = w.site_bands_ptr(h);
  const bandsBuf = device.createBuffer({
    size: Math.max(4, bandsLen * 4),
    usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
  });
  device.queue.writeBuffer(bandsBuf, 0, new Uint32Array(mem(), bandsPtr, bandsLen));

  const uniformBuf = device.createBuffer({
    size: w.site_uniform_bytes(),
    usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST,
  });

  const sampler = device.createSampler({ magFilter: 'linear', minFilter: 'linear' });

  // Rasterize the 9 embedded SVGs from WASM into a crisp HiDPI sprite atlas (re-rasterizes on zoom!)
  const svgCount = w.site_svg_count();
  const svgSources = [];
  const svgAspects = [];
  for (let i = 0; i < svgCount; i++) {
    svgSources.push(readStr(w.site_svg_ptr(i), w.site_svg_len(i)));
    svgAspects.push(w.site_svg_aspect(i));
  }
  const svgImages = await Promise.all(svgSources.map((src) => new Promise((resolve) => {
    const img = new Image();
    const blob = new Blob([src], { type: 'image/svg+xml' });
    const url = URL.createObjectURL(blob);
    img.onload = () => { URL.revokeObjectURL(url); resolve(img); };
    img.onerror = () => { URL.revokeObjectURL(url); resolve(null); };
    img.src = url;
  })));

  let atlasTex = null;
  let atlasScale = 0;
  let bindGroup = null;
  let instCap = 4096;
  const instStride = w.site_inst_stride();
  let instBuf = device.createBuffer({
    size: instCap * instStride,
    usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
  });

  const rebuildBindGroup = () => {
    bindGroup = device.createBindGroup({
      layout: pipeline.getBindGroupLayout(0),
      entries: [
        { binding: 0, resource: { buffer: uniformBuf } },
        { binding: 1, resource: { buffer: instBuf } },
        { binding: 2, resource: { buffer: curvesBuf } },
        { binding: 3, resource: { buffer: bandsBuf } },
        { binding: 4, resource: sampler },
        { binding: 5, resource: atlasTex.createView() },
      ],
    });
  };

  const buildSvgAtlas = (scale) => {
    const eff = Math.max(1, Math.min(4, scale));
    atlasScale = eff;
    const iconH = Math.round(48 * eff);
    const sigW = Math.round(248 * eff);
    const sigH = Math.round(78 * eff);
    const pad = 4;
    const aw = Math.max(512, sigW + pad * 2);
    const ah = Math.max(256, iconH * 2 + sigH + pad * 6);
    const c2d = document.createElement('canvas');
    c2d.width = aw;
    c2d.height = ah;
    const g = c2d.getContext('2d');
    g.clearRect(0, 0, aw, ah);
    let x = pad, y = pad, rowH = 0;
    for (let i = 0; i < svgCount; i++) {
      const isSig = i === 8;
      const iw = isSig ? sigW : Math.max(8, Math.round(iconH * svgAspects[i]));
      const ih = isSig ? sigH : iconH;
      if (x + iw + pad > aw) { x = pad; y += rowH + pad; rowH = 0; }
      const img = svgImages[i];
      if (img) g.drawImage(img, x, y, iw, ih);
      w.site_set_svg_uv(h, i, x / aw, y / ah, iw / aw, ih / ah);
      x += iw + pad;
      rowH = Math.max(rowH, ih);
    }
    if (atlasTex) atlasTex.destroy();
    atlasTex = device.createTexture({
      size: [aw, ah],
      format: 'rgba8unorm',
      usage: GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST | GPUTextureUsage.RENDER_ATTACHMENT,
    });
    device.queue.copyExternalImageToTexture({ source: c2d }, { texture: atlasTex, premultipliedAlpha: true }, [aw, ah]);
    rebuildBindGroup();
  };

  buildSvgAtlas(window.devicePixelRatio || 1);

  const flushSideEffects = () => {
    const cLen = w.site_take_copy_len(h);
    if (cLen > 0) {
      const text = readStr(w.site_copy_ptr(h), cLen);
      navigator.clipboard?.writeText(text).catch(() => {});
    }
    const nLen = w.site_take_nav_len(h);
    if (nLen > 0) {
      const href = readStr(w.site_nav_ptr(h), nLen);
      window.open(href, '_blank', 'noopener,noreferrer');
    }
    canvas.style.cursor = readStr(w.site_cursor_ptr(h), w.site_cursor_len(h));
  };

  let dirty = true;
  let rafId = 0;
  const kick = () => {
    if (!rafId) rafId = requestAnimationFrame(frame);
  };

  const frame = (now) => {
    rafId = 0;
    const dpr = Math.max(1, Math.min(4, window.devicePixelRatio || 1));
    const cssW = window.innerWidth;
    const cssH = window.innerHeight;
    const devW = Math.max(1, Math.round(cssW * dpr));
    const devH = Math.max(1, Math.round(cssH * dpr));
    if (canvas.width !== devW || canvas.height !== devH) {
      canvas.width = devW;
      canvas.height = devH;
    }

    // Re-rasterize SVG atlas if zoom moved significantly so icons stay crisp at any zoom!
    const z = w.site_zoom(h);
    const targetAtlas = Math.max(1, Math.min(4, z * dpr));
    if (targetAtlas > atlasScale * 1.45 || targetAtlas < atlasScale * 0.55) {
      buildSvgAtlas(targetAtlas);
    }

    const wantsNext = w.site_render_frame(h, cssW, cssH, dpr, now);
    const instCount = w.site_insts_len(h);
    if (instCount > instCap) {
      instCap = Math.max(instCount * 2, instCap * 2);
      instBuf.destroy();
      instBuf = device.createBuffer({
        size: instCap * instStride,
        usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST,
      });
      rebuildBindGroup();
    }

    const uPtr = w.site_uniform_ptr(h);
    device.queue.writeBuffer(uniformBuf, 0, new Uint8Array(mem(), uPtr, w.site_uniform_bytes()));
    if (instCount > 0) {
      const iPtr = w.site_insts_ptr(h);
      device.queue.writeBuffer(instBuf, 0, new Uint8Array(mem(), iPtr, instCount * instStride));
    }

    const enc = device.createCommandEncoder();
    const pass = enc.beginRenderPass({
      colorAttachments: [{
        view: ctx.getCurrentTexture().createView(),
        loadOp: 'clear',
        storeOp: 'store',
        clearValue: { r: w.site_bg_r(h), g: w.site_bg_g(h), b: w.site_bg_b(h), a: 1 },
      }],
    });
    pass.setPipeline(pipeline);
    pass.setBindGroup(0, bindGroup);
    if (instCount > 0) pass.draw(6, instCount, 0, 0);
    pass.end();
    device.queue.submit([enc.finish()]);

    dirty = false;
    if (wantsNext) kick();
  };

  // Smooth macOS Trackpad Pinch-to-Zoom + 2-Finger Scroll/Pan.
  // Only zoom on true trackpad pinch (`e.ctrlKey` without `e.metaKey`, or Safari `gesturechange`),
  // never sticky key state or accidental `Cmd`+scroll after `Cmd+Tab` / `Cmd+C`.
  let inGesture = false;
  let gestureScale = 1;
  canvas.addEventListener('wheel', (e) => {
    e.preventDefault();
    if (inGesture) return;
    const scale = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? window.innerHeight : 1;
    const dx = e.deltaX * scale;
    const dy = e.deltaY * scale;
    if (e.ctrlKey && !e.metaKey) {
      if (Math.abs(dy) < 0.05) return;
      const clamped = Math.max(-40, Math.min(40, dy));
      const factor = Math.exp(-clamped * 0.008);
      w.site_pinch(h, e.clientX, e.clientY, factor);
    } else {
      w.site_wheel(h, dx, dy);
    }
    dirty = true;
    kick();
  }, { passive: false });

  // Safari trackpad pinch gesture events (guarded so wheel + gesture never double-apply)
  canvas.addEventListener('gesturestart', (e) => {
    e.preventDefault();
    inGesture = true;
    gestureScale = e.scale || 1;
  });
  canvas.addEventListener('gesturechange', (e) => {
    e.preventDefault();
    if (!inGesture || !e.scale || !Number.isFinite(e.scale)) return;
    const f = Math.max(0.5, Math.min(2.0, e.scale / (gestureScale || 1)));
    gestureScale = e.scale;
    w.site_pinch(h, e.clientX || window.innerWidth * 0.5, e.clientY || window.innerHeight * 0.5, f);
    dirty = true;
    kick();
  });
  const endGesture = (e) => { e?.preventDefault?.(); inGesture = false; gestureScale = 1; };
  canvas.addEventListener('gestureend', endGesture);
  window.addEventListener('blur', () => { inGesture = false; gestureScale = 1; });

  // Pointer interaction
  canvas.addEventListener('pointermove', (e) => {
    w.site_pointer_move(h, e.clientX, e.clientY, performance.now());
    flushSideEffects();
    dirty = true;
    kick();
  });
  canvas.addEventListener('pointerdown', (e) => {
    canvas.focus();
    canvas.setPointerCapture?.(e.pointerId);
    w.site_pointer_down(h, e.clientX, e.clientY, performance.now());
    flushSideEffects();
    dirty = true;
    kick();
  });
  canvas.addEventListener('pointerup', (e) => {
    w.site_pointer_up(h, e.clientX, e.clientY, performance.now());
    flushSideEffects();
    dirty = true;
    kick();
  });

  // Keyboard shortcuts (`Cmd+F`, `Cmd+C`, `Cmd+A`, `Cmd+`/`Cmd-`/`Cmd0`, `Cmd+Shift+L`)
  window.addEventListener('keydown', (e) => {
    const mod = e.metaKey || e.ctrlKey;
    const k = e.key;
    let code = 0;
    if (k === 'Escape') code = 1;
    else if (k === 'Enter') code = 2;
    else if (k === 'Backspace') code = 3;
    else if (k === 'Tab') code = 4;
    else if (mod && k.toLowerCase() === 'f') code = 10;
    else if (mod && k.toLowerCase() === 'c') code = 11;
    else if (mod && k.toLowerCase() === 'a') code = 12;
    else if (mod && (k === '=' || k === '+')) code = 13;
    else if (mod && k === '-') code = 14;
    else if (mod && k === '0') code = 15;
    else if (mod && e.shiftKey && k.toLowerCase() === 'l') code = 16;

    const now = performance.now();
    if (code) {
      if (w.site_key_down(h, code, e.shiftKey ? 1 : 0, mod ? 1 : 0, now)) {
        e.preventDefault();
        flushSideEffects();
        document.body.style.background = w.site_is_dark(h) ? '#101011' : '#f2f1ee';
        dirty = true;
        kick();
        return;
      }
    }
    if (!mod && k.length === 1 && w.site_find_open(h)) {
      if (w.site_text_char(h, k.codePointAt(0), now)) {
        e.preventDefault();
        dirty = true;
        kick();
      }
    }
  });

  window.addEventListener('resize', () => { dirty = true; kick(); });

  // Inspection & verification handle
  window.__BLAZE_SITE__ = {
    zoom: () => w.site_zoom(h),
    instCount: () => w.site_insts_len(h),
    glyphCount: () => w.site_glyph_count(h),
    frameCount: () => w.site_frame_count(h),
    wgslBytes: () => wgsl.length,
    curvesCount: () => curvesLen,
    bandsCount: () => bandsLen,
    findOpen: () => w.site_find_open(h) === 1,
    findMatches: () => w.site_find_matches(h),
    dark: () => w.site_is_dark(h) === 1,
    setDark: (b) => { w.site_set_dark(h, b ? 1 : 0); document.body.style.background = b ? '#101011' : '#f2f1ee'; kick(); },
    pinch: (sx, sy, f) => { w.site_pinch(h, sx, sy, f); kick(); },
    resetZoom: () => { w.site_reset_zoom(h); kick(); },
    scroll: (dx, dy) => { w.site_wheel(h, dx, dy); kick(); },
  };

  kick();
}

boot().catch(showErr);
</script>
</body>
</html>
"##;
