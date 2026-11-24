//! Minimal zero-handwritten-WGSL WebGPU host HTML for `blaze-site-wasm`, with automatic
//! **ECS → HTML projection** fallback when WebGPU is unavailable (or `?renderer=html`).
//!
//! - When WebGPU is available: fetches `blaze_site.wasm`, `page.json`, and `BZG1` Slug font table,
//!   reads the linked WGSL shader emitted directly by Rust `gpu::slug::emit_slug_site_wgsl`,
//!   renders the entire front page + `den` Slug vector outlines + hover cards + native WebGPU
//!   `Cmd+F` Find Bar + Zoom HUD Pill in ONE single WebGPU draw call, with cursor-anchored
//!   trackpad pinch-to-zoom (`25%`–`1600%`).
//! - When WebGPU is unavailable (or `?renderer=html`): skips downloading the font table and
//!   projects the exact same mounted `blaze_ecs::World` (`ChildOf` + `SiteNodeKind` + embedded
//!   SVGs + hover-card templates) into semantic, accessible, styled HTML via
//!   `site_project_html_len` / `site_project_html_ptr`!

pub const SITE_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Blaze — Pure-Blaze WebGPU + ECS HTML Projection</title>
<style>
  :root {
    --bg: #101011;
    --bg-2: #141415;
    --bg-3: #19191a;
    --ink-1: #ebeae6;
    --ink-2: #b3b2af;
    --ink-3: #878684;
    --ink-4: #57575a;
    --line: #28282a;
    --line-2: #333335;
    --s-tint: rgba(235, 234, 230, 0.06);
    --fire: #e0461a;
    --syn-text: #bcbec4;
    --syn-kw: #cf8e6d;
    --syn-fn: #56a8f5;
    --syn-ty: #c77dbb;
    --syn-param: #bcbec4;
    --syn-mark: #7a7e85;
  }
  [data-theme="light"] {
    --bg: #f2f1ee;
    --bg-2: #eae9e4;
    --bg-3: #f6f5f2;
    --ink-1: #161617;
    --ink-2: #46464a;
    --ink-3: #6b6a6e;
    --ink-4: #a2a19d;
    --line: #dcdbd5;
    --line-2: #cdccc6;
    --s-tint: rgba(22, 22, 23, 0.06);
    --fire: #d63c12;
    --syn-text: #080808;
    --syn-kw: #0033b3;
    --syn-fn: #00627a;
    --syn-ty: #871094;
    --syn-param: #080808;
    --syn-mark: #8c8c8c;
  }
  html, body {
    margin: 0;
    padding: 0;
    width: 100%;
    height: 100%;
    background: var(--bg);
    color: var(--ink-1);
    font-family: Inter, -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
    -webkit-font-smoothing: antialiased;
  }
  body.mode-gpu {
    overflow: hidden;
    touch-action: none;
    overscroll-behavior: none;
    user-select: none;
    -webkit-user-select: none;
  }
  body.mode-html {
    overflow-y: auto;
    user-select: text;
    -webkit-user-select: text;
  }
  canvas#c {
    display: block;
    width: 100vw;
    height: 100vh;
    outline: none;
  }
  body.mode-html canvas#c { display: none; }
  #ecs-html { display: none; }
  body.mode-html #ecs-html { display: block; }
  #err {
    position: fixed;
    inset: 24px;
    color: var(--ink-1);
    font: 14px/1.6 system-ui, sans-serif;
    display: none;
    white-space: pre-wrap;
  }

  /* Pure ECS -> HTML projection styling (matches tokens.css & layout.rs) */
  .page {
    max-width: 640px;
    margin: 0 auto;
    padding: 0 24px 72px;
    box-sizing: border-box;
  }
  .bar {
    height: 72px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
  }
  .bar-left {
    display: flex;
    align-items: center;
    gap: 14px;
    min-width: 0;
  }
  .wordmark {
    font-size: 16px;
    font-weight: 600;
    letter-spacing: -0.02em;
    color: var(--ink-1);
  }
  .bar-div {
    width: 1px;
    height: 14px;
    background: var(--line-2);
    flex: none;
  }
  .backed {
    font-size: 13px;
    color: var(--ink-3);
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .bar-links {
    display: flex;
    align-items: center;
    gap: 22px;
    font-size: 14px;
    color: var(--ink-3);
  }
  .bar-links .lnk, .backed .lnk, .footer .lnk {
    color: var(--ink-3);
    text-decoration: none;
  }
  .bar-links .lnk:hover, .backed .lnk:hover, .footer .lnk:hover {
    color: var(--ink-1);
  }
  .title {
    margin: clamp(32px, 6vh, 80px) 0 22px;
    font-size: clamp(32px, 5.2vw, 40px);
    font-weight: 600;
    line-height: 1.1;
    letter-spacing: -0.032em;
    text-wrap: balance;
    color: var(--ink-1);
  }
  .sup {
    font-size: 0.62em;
    vertical-align: 0.42em;
    font-weight: 500;
    color: var(--ink-3);
  }
  .label {
    margin: 30px 0 0;
    font-size: 13px;
    font-weight: 500;
    line-height: 1.5;
    color: var(--ink-3);
  }
  .cmd-box {
    margin-top: 28px;
    min-height: 60px;
    padding: 10px 10px 10px 22px;
    border-radius: 14px;
    background: var(--bg-2);
    border: 1px solid var(--line);
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    box-sizing: border-box;
  }
  .cmd-box.after-label { margin-top: 10px; }
  .cmd-line {
    font-family: "Berkeley Mono", ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 15px;
    line-height: 1.5;
    letter-spacing: -0.01em;
    overflow-x: auto;
    white-space: nowrap;
  }
  .cmd-part {
    padding: 2px 3px;
    margin: -2px -3px;
    border-radius: 7px;
    cursor: default;
  }
  .cmd-part:hover, .cmd-part:focus-visible {
    background: var(--s-tint);
    outline: none;
  }
  .copy-btn {
    flex: none;
    height: 36px;
    min-width: 64px;
    padding: 0 14px;
    border: 0;
    border-radius: 9px;
    background: var(--s-tint);
    color: var(--ink-2);
    font: 500 13px/1 inherit;
    cursor: pointer;
    transition: background 120ms, color 120ms;
  }
  .copy-btn:hover { background: var(--ink-1); color: var(--bg); }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .list.after-cmd { margin-top: 36px; }
  .soon { margin-top: 36px; }
  .soon .label { margin-bottom: 12px; }
  .item {
    position: relative;
    padding-left: 23px;
    margin-bottom: 14px;
    font-size: 17px;
    font-weight: 600;
    line-height: 1.55;
    letter-spacing: -0.012em;
    color: var(--ink-1);
  }
  .item::before {
    content: "";
    position: absolute;
    left: 6px;
    top: 11px;
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: var(--ink-4);
  }
  .item.item-ring::before {
    width: 6px;
    height: 6px;
    background: transparent;
    border: 1px solid var(--ink-4);
    box-sizing: border-box;
  }
  .sub-list {
    list-style: none;
    margin: 4px 0 0;
    padding: 0;
  }
  .sub {
    position: relative;
    padding-left: 21px;
    margin-bottom: 4px;
    font-size: 15px;
    font-weight: 400;
    line-height: 1.5;
    letter-spacing: 0;
    color: var(--ink-3);
  }
  .sub.sub-head {
    font-weight: 500;
    letter-spacing: -0.006em;
    color: var(--ink-1);
  }
  .sub::before {
    content: "";
    position: absolute;
    left: 5px;
    top: 9px;
    width: 4px;
    height: 4px;
    border-radius: 50%;
    border: 1px solid var(--ink-4);
    box-sizing: border-box;
  }
  .sub.sub-deep::before {
    border-radius: 1px;
    background: var(--ink-4);
    border: 0;
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    padding: 1px 6px;
    border-radius: 6px;
    background: var(--s-tint);
    font-family: "Berkeley Mono", ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 12.9px;
    font-weight: 400;
    line-height: 1.35;
    color: var(--ink-1);
    vertical-align: baseline;
  }
  .chip-badge {
    border-radius: 999px;
    background: var(--bg);
    border: 1px solid var(--line-2);
    font-family: inherit;
    font-size: 11.5px;
    font-weight: 500;
    color: var(--ink-2);
    padding: 1px 7px;
  }
  .chip .lnk {
    text-decoration: none;
    border-radius: 4px;
    padding: 0 2px;
    margin: 0 -2px;
  }
  .chip .lnk:hover {
    background: var(--s-tint);
    box-shadow: inset 0 0 0 1px var(--line-2);
  }
  .lnk {
    color: inherit;
    text-decoration: none;
  }
  .lnk-u {
    color: inherit;
    text-decoration: none;
    border-bottom: 1px solid var(--line-2);
    padding-bottom: 1px;
  }
  .lnk-u:hover { border-bottom-color: var(--ink-1); }
  .sub .lnk-u { color: var(--ink-2); }
  .ico {
    display: inline-flex;
    align-items: center;
    vertical-align: -0.15em;
    height: 1.05em;
    width: auto;
    line-height: 1;
  }
  .ico svg { height: 100%; width: auto; display: block; }
  .ico-mask { color: inherit; }
  .ico-mask svg { fill: currentColor; }
  .r-cmd { color: var(--ink-1); }
  .r-cmdDim { color: var(--ink-3); }
  .r-cmdFaint { color: var(--ink-4); }
  .r-cmdPrompt { color: var(--fire); }
  .r-code { color: var(--ink-1); font-weight: 500; }
  .r-codeArg { color: var(--ink-2); }
  .r-codeProg { color: var(--ink-1); font-weight: 600; }
  .r-codeMark { color: var(--ink-4); }
  .r-rsText { color: var(--syn-text); }
  .r-rsKw { color: var(--syn-kw); }
  .r-rsFn { color: var(--syn-fn); }
  .r-rsTy { color: var(--syn-ty); }
  .r-rsParam { color: var(--syn-param); }
  .r-rsMark { color: var(--syn-mark); }
  .r-tipCode {
    font-family: "Berkeley Mono", ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 11.2px;
    font-weight: 500;
    color: var(--ink-1);
  }
  .r-tipPrompt {
    font-family: "Berkeley Mono", ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 11.2px;
    color: var(--ink-4);
  }
  .sign { margin-top: 42px; }
  .closing {
    margin: 0 0 18px;
    font-size: 15px;
    line-height: 1.5;
    color: var(--ink-3);
  }
  .sig-svg {
    width: 202px;
    max-width: 100%;
    color: var(--ink-1);
    margin: 0 0 6px -2px;
  }
  .sig-svg svg { width: 100%; height: auto; display: block; fill: currentColor; }
  .sign-line, .sign-contact {
    margin: 0 0 2px;
    font-size: 13px;
    line-height: 1.5;
    color: var(--ink-3);
  }
  .sign-contact { display: flex; align-items: center; gap: 14px; flex-wrap: wrap; }
  .sign-contact .lnk { color: var(--ink-1); display: inline-flex; align-items: center; gap: 5px; }
  .notes { margin-top: 64px; }
  .note-rule {
    width: 40px;
    height: 1px;
    border: 0;
    background: var(--line-2);
    margin: 0 0 15px;
  }
  .note {
    margin: 0 0 6px;
    font-size: 12px;
    line-height: 1.6;
    color: var(--ink-3);
    border-radius: 10px;
    padding: 4px 6px;
    margin-left: -6px;
    transition: background 900ms ease-out;
  }
  .note.landed { background: rgba(235, 234, 230, 0.16); transition: none; }
  [data-theme="light"] .note.landed { background: rgba(22, 22, 23, 0.12); }
  .scramble { opacity: 0.45; }
  .footer {
    margin-top: 48px;
    display: flex;
    gap: 22px;
    flex-wrap: wrap;
    font-size: 14px;
    color: var(--ink-3);
  }
  .ecs-badge {
    margin-top: 28px;
    font-size: 11px;
    color: var(--ink-4);
    font-family: "Berkeley Mono", ui-monospace, SFMono-Regular, Menlo, monospace;
  }
  .tip-pop {
    position: fixed;
    z-index: 999;
    max-width: 288px;
    padding: 10px 13px 11px;
    border-radius: 11px;
    background: var(--bg-3);
    border: 1px solid var(--line-2);
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.24);
    color: var(--ink-2);
    font-size: 13px;
    line-height: 1.45;
    pointer-events: none;
  }
  .tip-title {
    font-weight: 600;
    color: var(--ink-1);
    margin-bottom: 3px;
    display: flex;
    align-items: center;
    gap: 5px;
  }
  .tip-rows { display: flex; flex-direction: column; gap: 5px; }
  .tip-row { display: flex; gap: 14px; align-items: baseline; }
  .tip-says { flex: none; min-width: 22px; }
  @media (max-width: 560px) {
    .bar { flex-wrap: wrap; height: auto; padding: 16px 0 8px; }
    .bar-div { display: none; }
    .cmd-box { padding: 8px 8px 8px 14px; min-height: 52px; }
    .cmd-line { font-size: 12px; }
  }
</style>
</head>
<body class="mode-gpu">
<canvas id="c" tabindex="0"></canvas>
<main id="ecs-html" aria-live="polite"></main>
<div id="err"></div>
<script type="module">
const canvas = document.getElementById('c');
const htmlMount = document.getElementById('ecs-html');
const errEl = document.getElementById('err');
const showErr = (msg) => { errEl.style.display = 'block'; errEl.textContent = String(msg); };
const td = new TextDecoder();

async function bootHtmlProjection(w, h, lightMq) {
  document.body.className = 'mode-html';
  const mem = () => w.memory.buffer;
  const readStr = (ptr, len) => td.decode(new Uint8Array(mem(), ptr, len));

  const renderHtml = () => {
    const dark = w.site_is_dark(h) === 1;
    document.documentElement.setAttribute('data-theme', dark ? 'dark' : 'light');
    const len = w.site_project_html_len(h);
    htmlMount.innerHTML = readStr(w.site_project_html_ptr(h), len);
  };

  w.site_set_dark(h, lightMq.matches ? 0 : 1);
  renderHtml();
  lightMq.addEventListener('change', () => {
    w.site_set_dark(h, lightMq.matches ? 0 : 1);
    renderHtml();
  });

  // Floating hover cards sourced from ECS-projected `<template data-tip-key>`
  let pop = null;
  const hideTip = () => { if (pop) { pop.remove(); pop = null; } };
  const showTip = (el) => {
    hideTip();
    const key = el.getAttribute('data-tip');
    if (!key) return;
    const tpl = htmlMount.querySelector(`template[data-tip-key="${CSS.escape(key)}"]`);
    if (!tpl) return;
    pop = document.createElement('div');
    pop.className = 'tip-pop';
    pop.appendChild(tpl.content.cloneNode(true));
    document.body.appendChild(pop);
    const ar = el.getBoundingClientRect();
    const pr = pop.getBoundingClientRect();
    const x = Math.max(8, Math.min(window.innerWidth - pr.width - 8, ar.left + (ar.width - pr.width) * 0.5));
    const above = ar.top - pr.height - 8 >= 8;
    const y = above ? ar.top - pr.height - 8 : ar.bottom + 8;
    pop.style.left = `${x}px`;
    pop.style.top = `${y}px`;
  };

  htmlMount.addEventListener('pointerover', (e) => {
    const t = e.target.closest?.('[data-tip]');
    if (t) showTip(t);
  });
  htmlMount.addEventListener('pointerout', (e) => {
    const t = e.target.closest?.('[data-tip]');
    if (t && !t.contains(e.relatedTarget)) hideTip();
  });
  htmlMount.addEventListener('focusin', (e) => {
    const t = e.target.closest?.('[data-tip]');
    if (t) showTip(t);
  });
  htmlMount.addEventListener('focusout', hideTip);
  window.addEventListener('scroll', hideTip, { passive: true });

  // Copy button & footnote jump flash
  htmlMount.addEventListener('click', (e) => {
    const btn = e.target.closest?.('.copy-btn');
    if (btn) {
      const cmd = btn.getAttribute('data-copy') || '';
      navigator.clipboard?.writeText(cmd).catch(() => {});
      const prev = btn.textContent;
      btn.textContent = 'Copied';
      setTimeout(() => { btn.textContent = prev || 'Copy'; }, 1500);
      return;
    }
    const noteLink = e.target.closest?.('a[data-note]');
    if (noteLink) {
      const id = noteLink.getAttribute('data-note');
      const target = id && document.getElementById(`note-${id}`);
      if (target) {
        e.preventDefault();
        hideTip();
        target.scrollIntoView({ behavior: 'smooth', block: 'center' });
        target.classList.remove('landed');
        void target.offsetWidth;
        target.classList.add('landed');
        setTimeout(() => target.classList.remove('landed'), 1300);
      }
    }
  });

  // 60ms scrambled letters animation (respects reduced motion)
  const calm = window.matchMedia('(prefers-reduced-motion: reduce)');
  if (!calm.matches) {
    const alphabet = 'acegikmnopqrsuvwxz';
    setInterval(() => {
      const spans = htmlMount.querySelectorAll('[data-scramble]');
      for (const s of spans) {
        s.textContent = alphabet[Math.floor(Math.random() * alphabet.length)];
      }
    }, 60);
  }

  // Cmd+Shift+L theme toggle
  window.addEventListener('keydown', (e) => {
    if ((e.metaKey || e.ctrlKey) && e.shiftKey && e.key.toLowerCase() === 'l') {
      e.preventDefault();
      const next = w.site_is_dark(h) === 1 ? 0 : 1;
      w.site_set_dark(h, next);
      document.documentElement.setAttribute('data-theme', next ? 'dark' : 'light');
    }
  });

  window.__BLAZE_SITE__ = {
    mode: () => 'html-ecs',
    entityCount: () => w.site_projected_entities(h),
    dark: () => w.site_is_dark(h) === 1,
    setDark: (b) => {
      w.site_set_dark(h, b ? 1 : 0);
      document.documentElement.setAttribute('data-theme', b ? 'dark' : 'light');
    },
  };
}

async function boot() {
  const params = new URLSearchParams(window.location.search);
  const forceHtml = params.get('renderer') === 'html';
  const lightMq = window.matchMedia('(prefers-color-scheme: light)');

  let adapter = null;
  let device = null;
  if (!forceHtml && navigator.gpu) {
    try {
      adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
      if (adapter) device = await adapter.requestDevice();
    } catch (_) {
      device = null;
    }
  }

  const asset = (key, fallback) =>
    document.body?.dataset?.[key] || document.documentElement?.dataset?.[key] || fallback;
  const wasmUrl = asset('wasm', '/blaze_site.wasm');
  const pageUrl = asset('page', '/page.json');
  const fontUrl = asset('font', '/fonts/text.bin');
  const fetches = [fetch(wasmUrl), fetch(pageUrl)];
  if (device) fetches.push(fetch(fontUrl));
  const responses = await Promise.all(fetches);
  for (const r of responses) {
    if (!r.ok) {
      if (r.status === 404 && !sessionStorage.getItem('blaze-reload-once')) {
        sessionStorage.setItem('blaze-reload-once', '1');
        window.location.reload();
        return;
      }
      throw new Error(`Asset fetch failed (${r.status}): ${r.url}`);
    }
  }
  const buffers = await Promise.all(responses.map((r) => r.arrayBuffer()));
  const [wasmBytes, pageBytes, fontBytes] = buffers;
  const magic = new Uint8Array(wasmBytes, 0, 4);
  if (magic[0] !== 0x00 || magic[1] !== 0x61 || magic[2] !== 0x73 || magic[3] !== 0x6d) {
    if (!sessionStorage.getItem('blaze-reload-once')) {
      sessionStorage.setItem('blaze-reload-once', '1');
      window.location.reload();
      return;
    }
    throw new Error(`Invalid WebAssembly response from ${wasmUrl}`);
  }
  sessionStorage.removeItem('blaze-reload-once');

  const { instance } = await WebAssembly.instantiate(wasmBytes, {});
  const w = instance.exports;
  const mem = () => w.memory.buffer;
  const readStr = (ptr, len) => td.decode(new Uint8Array(mem(), ptr, len));

  const h = w.site_create();
  if (!h) { showErr('site_create() returned null'); return; }

  const uploadScratch = (ab) => {
    const src = new Uint8Array(ab);
    const dstPtr = w.site_scratch_alloc(h, src.byteLength);
    new Uint8Array(mem(), dstPtr, src.byteLength).set(src);
    return src.byteLength;
  };

  if (!w.site_load_page_json_from_scratch(h, uploadScratch(pageBytes))) {
    showErr('Failed to parse page.json');
    return;
  }

  // Automatic ECS -> HTML projection fallback whenever WebGPU is unavailable (or `?renderer=html`)
  if (!device || !fontBytes) {
    await bootHtmlProjection(w, h, lightMq);
    return;
  }

  if (!w.site_load_font_table_from_scratch(h, uploadScratch(fontBytes))) {
    await bootHtmlProjection(w, h, lightMq);
    return;
  }

  const format = navigator.gpu.getPreferredCanvasFormat();
  const ctx = canvas.getContext('webgpu');
  if (!ctx) {
    await bootHtmlProjection(w, h, lightMq);
    return;
  }
  ctx.configure({ device, format, alphaMode: 'premultiplied' });

  const applyThemePref = () => {
    const dark = lightMq.matches ? 0 : 1;
    w.site_set_dark(h, dark);
    document.documentElement.setAttribute('data-theme', dark ? 'dark' : 'light');
  };
  applyThemePref();
  lightMq.addEventListener('change', () => { applyThemePref(); dirty = true; kick(); });

  // Read Rust-emitted WGSL shader straight from WASM (ZERO handwritten WGSL!)
  const wgsl = readStr(w.site_wgsl_ptr(h), w.site_wgsl_len(h));
  const module = device.createShaderModule({ code: wgsl });
  const info = await module.getCompilationInfo();
  const wgslErrors = info.messages.filter((m) => m.type === 'error');
  if (wgslErrors.length) {
    await bootHtmlProjection(w, h, lightMq);
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
      if (href.startsWith('/') || href.startsWith('#')) {
        window.location.assign(href);
      } else {
        window.open(href, '_blank', 'noopener,noreferrer');
      }
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
        document.documentElement.setAttribute('data-theme', w.site_is_dark(h) ? 'dark' : 'light');
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

  window.__BLAZE_SITE__ = {
    mode: () => 'webgpu-slug',
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
    setDark: (b) => { w.site_set_dark(h, b ? 1 : 0); document.documentElement.setAttribute('data-theme', b ? 'dark' : 'light'); kick(); },
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
