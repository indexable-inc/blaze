//! Minimal zero-handwritten-WGSL WebGPU host page for `blaze-ui-wasm`.
//!
//! Fetches `/blaze_ui.wasm`, pulls BOTH WGSL shaders directly from Rust's `gpu::emit`
//! pipeline (`ui_wgsl_sdf` and `ui_wgsl_glyph`), rasterizes a HiDPI 16×8 monospace ligature
//! atlas, uploads `Viewport` + `ClipRect[]` + `RectInst[]` + `GlyphInst[]`, and renders the
//! entire relational ECS UI at 120 FPS on one WebGPU canvas with zero DOM UI chrome.

/// Self-contained WebGPU host HTML shell (`zero handwritten WGSL`).
pub const WORKBENCH_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width,initial-scale=1" />
<title>Blaze UI · Relational ECS -> WebGPU Workbench</title>
<style>
  html, body {
    margin: 0; padding: 0; width: 100%; height: 100%;
    overflow: hidden; background: #101011; color: #ebeae6;
    font-family: "Berkeley Mono", ui-monospace, SFMono-Regular, Menlo, monospace;
  }
  canvas#gpu { display: block; width: 100vw; height: 100vh; touch-action: none; }
  #err {
    position: fixed; left: 16px; bottom: 16px; right: 16px;
    padding: 10px 14px; border-radius: 6px; font-size: 12px;
    background: rgba(25, 25, 26, 0.94); border: 1px solid #e0461a;
    color: #ff7168; display: none; white-space: pre-wrap; z-index: 99;
  }
</style>
</head>
<body>
<canvas id="gpu"></canvas>
<div id="err"></div>
<script type="module">
const ATLAS_COLS = 16, ATLAS_ROWS = 8, CELL_W = 24, CELL_H = 40;
const MAX_CLIPS = 2048, MAX_RECTS = 32768, MAX_GLYPHS = 65536;
const CLIP_STRIDE = 32, RECT_STRIDE = 80, GLYPH_STRIDE = 64;

window.__BLAZE_GPU_ERRORS__ = [];
window.__BLAZE_STATS__ = { ready: false, frames: 0, clips: 0, rects: 0, glyphs: 0, sdfWgslLen: 0, glyphWgslLen: 0 };

function reportErr(msg) {
  window.__BLAZE_GPU_ERRORS__.push(String(msg));
  const el = document.getElementById('err');
  el.style.display = 'block';
  el.textContent = window.__BLAZE_GPU_ERRORS__.join('\n');
  console.error('[blaze-ui]', msg);
}

async function buildAtlas() {
  try { await document.fonts.ready; } catch {}
  const c = document.createElement('canvas');
  c.width = ATLAS_COLS * CELL_W;
  c.height = ATLAS_ROWS * CELL_H;
  c.style.cssText = 'position:fixed;left:-9999px;top:-9999px;font-feature-settings:"liga" 1,"calt" 1;';
  document.body.appendChild(c);
  const ctx = c.getContext('2d');
  const fontPx = 28;
  ctx.font = `500 ${fontPx}px "Berkeley Mono", ui-monospace, SFMono-Regular, Menlo, monospace`;
  ctx.fillStyle = '#ffffff';
  ctx.textAlign = 'center';
  ctx.textBaseline = 'alphabetic';
  const advanceRatio = ctx.measureText('M').width / fontPx;
  const baseline = 29;

  for (let i = 0; i <= 94; i++) {
    const col = i % ATLAS_COLS, row = Math.floor(i / ATLAS_COLS);
    ctx.fillText(String.fromCharCode(32 + i), col * CELL_W + CELL_W * 0.5, row * CELL_H + baseline);
  }
  const syms = ['·', '×', 'µ', '→', 'Δ', '…', '—', '↵', '↗', '←', '▸', '▾', '●', '○', '◆', '⤢'];
  for (let i = 0; i < syms.length; i++) {
    ctx.fillText(syms[i], i * CELL_W + CELL_W * 0.5, 6 * CELL_H + baseline);
  }
  const ligs = ['->', '=>', '::', '!=', '==', '<=', '>=', '..'];
  for (let i = 0; i < ligs.length; i++) {
    ctx.fillText(ligs[i], i * 2 * CELL_W + CELL_W, 7 * CELL_H + baseline);
  }
  c.remove();
  return { canvas: c, advanceRatio };
}

function readUtf8(wasm, ptr, len) {
  return new TextDecoder().decode(new Uint8Array(wasm.memory.buffer, ptr, len));
}

async function boot() {
  const canvas = document.getElementById('gpu');
  const [wasmRes, atlas] = await Promise.all([fetch('/blaze_ui.wasm'), buildAtlas()]);
  const { instance } = await WebAssembly.instantiate(await wasmRes.arrayBuffer(), {});
  const w = instance.exports;
  const h = w.ui_new(atlas.advanceRatio);
  if (!h) { reportErr('ui_new returned null handle'); return; }

  // Pull Rust-authored WGSL directly from `crates/gpu` inside WASM linear memory!
  const sdfLen = w.ui_wgsl_sdf(h);
  const sdfWgsl = readUtf8(w, w.ui_out_ptr(h), sdfLen);
  const glyphLen = w.ui_wgsl_glyph(h);
  const glyphWgsl = readUtf8(w, w.ui_out_ptr(h), glyphLen);
  window.__BLAZE_STATS__.sdfWgslLen = sdfLen;
  window.__BLAZE_STATS__.glyphWgslLen = glyphLen;

  if (!navigator.gpu) { reportErr('WebGPU unavailable in this browser'); return; }
  const adapter = await navigator.gpu.requestAdapter();
  const device = await adapter?.requestDevice();
  if (!device) { reportErr('Failed to acquire WebGPU device'); return; }
  device.addEventListener('uncapturederror', (e) => reportErr(`WebGPU: ${e.error?.message || e}`));

  const gpuCtx = canvas.getContext('webgpu');
  const fmt = navigator.gpu.getPreferredCanvasFormat();
  gpuCtx.configure({ device, format: fmt, alphaMode: 'premultiplied' });

  const blend = {
    color: { srcFactor: 'src-alpha', dstFactor: 'one-minus-src-alpha', operation: 'add' },
    alpha: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha', operation: 'add' },
  };
  const sdfMod = device.createShaderModule({ label: 'blaze_ui_sdf', code: sdfWgsl });
  const glyphMod = device.createShaderModule({ label: 'blaze_ui_glyph', code: glyphWgsl });
  for (const [name, mod] of [['sdf', sdfMod], ['glyph', glyphMod]]) {
    const info = await mod.getCompilationInfo();
    for (const m of info.messages) {
      if (m.type === 'error') reportErr(`WGSL(${name}) L${m.lineNum}:${m.linePos} ${m.message}`);
    }
  }

  const sdfPipe = device.createRenderPipeline({
    layout: 'auto',
    vertex: { module: sdfMod, entryPoint: 'ui_sdf_vs' },
    fragment: { module: sdfMod, entryPoint: 'ui_sdf_fs', targets: [{ format: fmt, blend }] },
    primitive: { topology: 'triangle-list' },
  });
  const glyphPipe = device.createRenderPipeline({
    layout: 'auto',
    vertex: { module: glyphMod, entryPoint: 'ui_glyph_vs' },
    fragment: { module: glyphMod, entryPoint: 'ui_glyph_fs', targets: [{ format: fmt, blend }] },
    primitive: { topology: 'triangle-list' },
  });

  const vpBuf = device.createBuffer({ size: 16, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
  const clipBuf = device.createBuffer({ size: MAX_CLIPS * CLIP_STRIDE, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST });
  const rectBuf = device.createBuffer({ size: MAX_RECTS * RECT_STRIDE, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST });
  const glyphBuf = device.createBuffer({ size: MAX_GLYPHS * GLYPH_STRIDE, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST });

  const tex = device.createTexture({
    size: [atlas.canvas.width, atlas.canvas.height, 1],
    format: 'rgba8unorm',
    usage: GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST | GPUTextureUsage.RENDER_ATTACHMENT,
  });
  device.queue.copyExternalImageToTexture({ source: atlas.canvas }, { texture: tex }, [atlas.canvas.width, atlas.canvas.height]);
  const samp = device.createSampler({ magFilter: 'linear', minFilter: 'linear' });

  const sdfBG = device.createBindGroup({
    layout: sdfPipe.getBindGroupLayout(0),
    entries: [
      { binding: 0, resource: { buffer: vpBuf } },
      { binding: 1, resource: { buffer: rectBuf } },
      { binding: 2, resource: { buffer: clipBuf } },
    ],
  });
  const glyphBG = device.createBindGroup({
    layout: glyphPipe.getBindGroupLayout(0),
    entries: [
      { binding: 0, resource: { buffer: vpBuf } },
      { binding: 1, resource: { buffer: glyphBuf } },
      { binding: 2, resource: { buffer: clipBuf } },
      { binding: 3, resource: samp },
      { binding: 4, resource: tex.createView() },
    ],
  });

  // Forward all DOM events cleanly to Rust/WASM
  canvas.addEventListener('pointermove', (e) => w.ui_on_pointer_move(h, e.clientX, e.clientY));
  canvas.addEventListener('pointerdown', (e) => {
    canvas.setPointerCapture(e.pointerId);
    w.ui_on_pointer_down(h, e.clientX, e.clientY, e.button, e.shiftKey ? 1 : 0);
  });
  const up = (e) => w.ui_on_pointer_up(h, e.clientX, e.clientY, e.button);
  canvas.addEventListener('pointerup', up);
  canvas.addEventListener('pointercancel', up);
  canvas.addEventListener('dblclick', (e) => w.ui_on_dblclick(h, e.clientX, e.clientY));
  canvas.addEventListener('wheel', (e) => {
    e.preventDefault();
    w.ui_on_wheel(h, e.clientX, e.clientY, e.deltaX, e.deltaY, e.ctrlKey || e.metaKey ? 1 : 0);
  }, { passive: false });
  window.addEventListener('keydown', (e) => {
    const mod = e.ctrlKey || e.metaKey ? 1 : 0;
    const sh = e.shiftKey ? 1 : 0;
    if (e.key === 'Escape') w.ui_on_key(h, 1, sh, mod);
    else if (e.key === 'Enter') w.ui_on_key(h, 2, sh, mod);
    else if (e.key === 'Tab') { e.preventDefault(); w.ui_on_key(h, 3, sh, mod); }
    else if (e.key === 'Backspace') { e.preventDefault(); w.ui_on_key(h, 4, sh, mod); }
    else if (e.key === ' ') w.ui_on_key(h, 10, sh, mod);
    else if (e.key.length === 1 && !e.ctrlKey && !e.metaKey) w.ui_on_char(h, e.key.codePointAt(0));
  });

  const vpData = new Float32Array(4);
  const t0 = performance.now();
  window.__BLAZE_STATS__.ready = true;

  function frame(now) {
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const cssW = Math.max(1, window.innerWidth);
    const cssH = Math.max(1, window.innerHeight);
    const pxW = Math.round(cssW * dpr), pxH = Math.round(cssH * dpr);
    if (canvas.width !== pxW || canvas.height !== pxH) { canvas.width = pxW; canvas.height = pxH; }
    const time = (now - t0) * 0.001;

    const tRender0 = performance.now();
    w.ui_render_frame(h, cssW, cssH, dpr, time);
    const renderMs = performance.now() - tRender0;

    const cc = Math.min(w.ui_clips_len(h), MAX_CLIPS);
    const rc = Math.min(w.ui_rects_len(h), MAX_RECTS - 512);
    const gc = Math.min(w.ui_glyphs_len(h), MAX_GLYPHS - 2048);
    const ovRc = Math.min(w.ui_ov_rects_len(h), MAX_RECTS - rc);
    const ovGc = Math.min(w.ui_ov_glyphs_len(h), MAX_GLYPHS - gc);

    vpData[0] = cssW; vpData[1] = cssH; vpData[2] = dpr; vpData[3] = time;
    const isFull = w.ui_is_full_upload(h) !== 0;
    const rpCount = w.ui_rect_patches_len(h);
    const gpCount = w.ui_glyph_patches_len(h);
    const needsDraw = isFull || rpCount > 0 || gpCount > 0 || window.__BLAZE_STATS__.frames < 2;

    if (isFull) {
      device.queue.writeBuffer(vpBuf, 0, vpData);
      if (cc > 0) device.queue.writeBuffer(clipBuf, 0, new Uint8Array(w.memory.buffer, w.ui_clips_ptr(h), cc * CLIP_STRIDE));
      if (rc > 0) device.queue.writeBuffer(rectBuf, 0, new Uint8Array(w.memory.buffer, w.ui_rects_ptr(h), rc * RECT_STRIDE));
      if (ovRc > 0) device.queue.writeBuffer(rectBuf, rc * RECT_STRIDE, new Uint8Array(w.memory.buffer, w.ui_ov_rects_ptr(h), ovRc * RECT_STRIDE));
      if (gc > 0) device.queue.writeBuffer(glyphBuf, 0, new Uint8Array(w.memory.buffer, w.ui_glyphs_ptr(h), gc * GLYPH_STRIDE));
      if (ovGc > 0) device.queue.writeBuffer(glyphBuf, gc * GLYPH_STRIDE, new Uint8Array(w.memory.buffer, w.ui_ov_glyphs_ptr(h), ovGc * GLYPH_STRIDE));
    } else if (rpCount > 0 || gpCount > 0) {
      // O(Δ) partial WebGPU buffer patching: upload ONLY the dirty entity slab slices!
      const rBase = w.ui_rects_ptr(h);
      if (rpCount > 0) {
        const patches = new Uint32Array(w.memory.buffer, w.ui_rect_patches_ptr(h), rpCount * 2);
        for (let i = 0; i < rpCount; i++) {
          const start = patches[i * 2], count = patches[i * 2 + 1];
          if (start + count <= rc) {
            device.queue.writeBuffer(rectBuf, start * RECT_STRIDE, new Uint8Array(w.memory.buffer, rBase + start * RECT_STRIDE, count * RECT_STRIDE));
          }
        }
      }
      const gBase = w.ui_glyphs_ptr(h);
      if (gpCount > 0) {
        const patches = new Uint32Array(w.memory.buffer, w.ui_glyph_patches_ptr(h), gpCount * 2);
        for (let i = 0; i < gpCount; i++) {
          const start = patches[i * 2], count = patches[i * 2 + 1];
          if (start + count <= gc) {
            device.queue.writeBuffer(glyphBuf, start * GLYPH_STRIDE, new Uint8Array(w.memory.buffer, gBase + start * GLYPH_STRIDE, count * GLYPH_STRIDE));
          }
        }
      }
    }

    if (needsDraw) {
      const enc = device.createCommandEncoder();
      const pass = enc.beginRenderPass({
        colorAttachments: [{
          view: gpuCtx.getCurrentTexture().createView(),
          clearValue: { r: 0.063, g: 0.063, b: 0.067, a: 1.0 },
          loadOp: 'clear',
          storeOp: 'store',
        }],
      });
      if (rc > 0) { pass.setPipeline(sdfPipe); pass.setBindGroup(0, sdfBG); pass.draw(6, rc, 0, 0); }
      if (gc > 0) { pass.setPipeline(glyphPipe); pass.setBindGroup(0, glyphBG); pass.draw(6, gc, 0, 0); }
      if (ovRc > 0) { pass.setPipeline(sdfPipe); pass.setBindGroup(0, sdfBG); pass.draw(6, ovRc, 0, rc); }
      if (ovGc > 0) { pass.setPipeline(glyphPipe); pass.setBindGroup(0, glyphBG); pass.draw(6, ovGc, 0, gc); }
      pass.end();
      device.queue.submit([enc.finish()]);
    }

    const cur = readUtf8(w, w.ui_cursor_ptr(h), w.ui_cursor_len(h));
    if (canvas.style.cursor !== cur) canvas.style.cursor = cur;

    window.__BLAZE_STATS__.frames++;
    window.__BLAZE_STATS__.clips = cc;
    window.__BLAZE_STATS__.rects = rc + ovRc;
    window.__BLAZE_STATS__.glyphs = gc + ovGc;
    window.__BLAZE_STATS__.lastLayout = w.ui_last_layout_nodes(h);
    window.__BLAZE_STATS__.lastPaint = w.ui_last_paint_nodes(h);
    window.__BLAZE_STATS__.lastPatchedBytes = w.ui_last_patched_bytes(h);
    window.__BLAZE_STATS__.lastRenderMs = renderMs;
    if (isFull) window.__BLAZE_STATS__.fullFrames = (window.__BLAZE_STATS__.fullFrames || 0) + 1;
    else if (rpCount > 0 || gpCount > 0) window.__BLAZE_STATS__.partialFrames = (window.__BLAZE_STATS__.partialFrames || 0) + 1;
    else window.__BLAZE_STATS__.idleFrames = (window.__BLAZE_STATS__.idleFrames || 0) + 1;
    requestAnimationFrame(frame);
  }
  requestAnimationFrame(frame);
}
void boot().catch(reportErr);
</script>
</body>
</html>
"#;
