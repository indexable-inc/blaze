/**
 * The front door, / (a `Hero` page, content/site.flecs `index`; render.rs writes `<main class="doc intro">` with the
 * h1 and the page's blocks in `.intro-after`; styles/intro.css). A scroll page in seven scenes:
 *
 * 1. the hero: the word, one line, two actions; a quiet WebGL glow behind it (a still CSS glow without WebGL);
 * 2. to 6. each Section is pinned (position: sticky) while the scroll passes through it, and its picture
 *    (client/lib/scene-*.ts) is a pure function of the scene's progress, so scrolling back plays it back and
 *    nothing is ever taken from the reader's scroll: no scroll-jacking, no snapping;
 * 7. the close, a plain block: the command, the two actions, the status line.
 *
 * Every picture is aria-hidden decoration; the heading and sentence beside it say the same, so a screen reader, a
 * reader without JS and a reader with motion reduced all get the whole page. Under prefers-reduced-motion nothing is
 * pinned and nothing moves: each picture is drawn once, at the frame that tells it.
 *
 * One requestAnimationFrame loop runs, and only while a scene or the hero is on screen (an IntersectionObserver says
 * which); it reads the scroll position against offsets measured on resize, so a frame does no layout reads. Motion on
 * the DOM is transform and opacity only; the canvases redraw only when their progress changed.
 */

import { onPage } from './lib/page';
import { type Picture, clamp } from './lib/scene';
import { found } from './lib/scene-found';
import { galaxy } from './lib/scene-galaxy';
import { loop } from './lib/scene-loop';
import { morph } from './lib/scene-morph';
import { seal } from './lib/scene-seal';

const PICTURES: Record<string, (section: HTMLElement, stage: HTMLElement) => Picture> = {
	edit: loop,
	grain: galaxy,
	superset: morph,
	sandbox: seal,
	shared: found,
};

type Scene = { section: HTMLElement; stage: HTMLElement; copy: HTMLElement; pic: Picture; top: number; len: number; p: number; on: boolean };

onPage((signal) => {
	const main = document.querySelector<HTMLElement>('main.intro');
	if (!main) return;
	const still = matchMedia('(prefers-reduced-motion: reduce)');
	main.dataset.on = '';

	// The hero's blocks (the word, its line, the two actions) in one screen-tall box.
	const word = main.querySelector('.wordmark');
	const box = document.createElement('div');
	box.className = 'hero';
	if (word) {
		main.insertBefore(box, word);
		box.append(word);
		for (const el of main.querySelectorAll('.intro-after > .lede, .intro-after > .lede ~ .go')) box.append(el);
	}

	const scenes: Scene[] = [];
	for (const [id, make] of Object.entries(PICTURES)) {
		const section = main.querySelector<HTMLElement>(`section#${CSS.escape(id)}`);
		if (!section) continue;
		const pin = document.createElement('div');
		pin.className = 'pin';
		const copy = document.createElement('div');
		copy.className = 'copy';
		const stage = document.createElement('div');
		stage.className = 'stage';
		// The heading, the sentence and its note go in the copy. From the first h3 on (the superset's steps) is what
		// the picture is made from: it stays in the section for screen readers, visually hidden while the picture
		// shows it, and shown as it is under reduced motion (styles/intro.css, .kept).
		const kept = document.createElement('div');
		kept.className = 'kept';
		let past = false;
		for (const el of [...section.children]) {
			past ||= el.matches('h3');
			(past ? kept : copy).append(el);
		}
		pin.append(copy, stage);
		section.append(pin);
		if (kept.childElementCount) section.append(kept);
		section.classList.add('scene');
		const pic = make(section, stage);
		scenes.push({ section, stage, copy, pic, top: 0, len: 0, p: -1, on: false });
	}

	const glow = hero(main);

	let raf = 0;
	let vh = innerHeight;
	const measure = () => {
		vh = innerHeight;
		for (const s of scenes) {
			s.section.style.setProperty('--len', still.matches ? 'auto' : `${s.pic.len * 100}svh`);
			const r = s.stage.getBoundingClientRect();
			s.pic.resize?.(r.width, r.height, Math.min(devicePixelRatio || 1, 2));
		}
		for (const s of scenes) {
			const r = s.section.getBoundingClientRect();
			s.top = r.top + scrollY;
			s.len = Math.max(1, r.height - vh);
			s.p = -1;
			// A resize clears a canvas; under reduced motion no frame loop redraws it, so the still is drawn here.
			if (still.matches) {
				s.p = s.pic.still;
				s.pic.draw(s.p);
			}
		}
		glow?.resize();
		if (still.matches) glow?.draw(0, 0);
	};

	const frame = (now: number) => {
		raf = 0;
		const y = scrollY;
		main.toggleAttribute('data-bar', y > vh * 0.7);
		let busy = false;
		for (const s of scenes) {
			if (!s.on) continue;
			const p = still.matches ? s.pic.still : clamp((y - s.top) / s.len);
			if (p !== s.p) {
				s.p = p;
				s.pic.draw(p);
			}
			// Without CSS scroll-driven animations, the copy comes in from how far the scene has come in.
			if (!still.matches && !CSS.supports('animation-timeline: view()')) {
				const r = clamp((y + vh - s.top) / (vh * 0.6));
				s.copy.style.setProperty('--in', r.toFixed(3));
			}
			busy = true;
		}
		if (glow?.visible && !still.matches) {
			glow.draw(now, y / vh);
			busy = true;
		}
		if (busy && !still.matches) raf = requestAnimationFrame(frame);
	};
	const kick = () => {
		if (!raf) raf = requestAnimationFrame(frame);
	};

	const io = new IntersectionObserver(
		(entries) => {
			for (const e of entries) {
				const s = scenes.find((x) => x.section === e.target);
				if (s) s.on = e.isIntersecting;
				else if (glow && e.target === main.querySelector('.wordmark')) glow.visible = e.isIntersecting;
			}
			kick();
		},
		{ rootMargin: '10% 0px' },
	);
	for (const s of scenes) io.observe(s.section);
	if (word && glow) io.observe(word);

	const ro = new ResizeObserver(() => {
		measure();
		kick();
	});
	ro.observe(document.documentElement);
	addEventListener('scroll', kick, { signal, passive: true });
	still.addEventListener(
		'change',
		() => {
			measure();
			kick();
		},
		{ signal },
	);
	const scheme = matchMedia('(prefers-color-scheme: dark)');
	scheme.addEventListener(
		'change',
		() => {
			measure();
			kick();
		},
		{ signal },
	);
	measure();
	kick();

	return () => {
		cancelAnimationFrame(raf);
		io.disconnect();
		ro.disconnect();
		glow?.dispose();
	};
});

/**
 * The hero's light: slow embers rising under the word, in the fire's colours, drawn at half resolution by one
 * fragment shader. Without WebGL, the CSS glow (main.intro::before) stays.
 */
function hero(main: HTMLElement): { visible: boolean; draw(t: number, scrolled: number): void; resize(): void; dispose(): void } | null {
	const el = document.createElement('canvas');
	el.className = 'glow';
	el.setAttribute('aria-hidden', 'true');
	const gl = el.getContext('webgl', { alpha: true, antialias: false, depth: false, premultipliedAlpha: true, powerPreference: 'low-power' });
	if (!gl) return null;
	const vs = `attribute vec2 a; void main() { gl_Position = vec4(a, 0.0, 1.0); }`;
	const fs = `precision mediump float;
uniform vec2 r; uniform float t; uniform float fade; uniform vec3 c1; uniform vec3 c2; uniform float light;
float h(vec2 p) { return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453); }
float n(vec2 p) { vec2 i = floor(p); vec2 f = fract(p); f = f * f * (3.0 - 2.0 * f);
  return mix(mix(h(i), h(i + vec2(1, 0)), f.x), mix(h(i + vec2(0, 1)), h(i + vec2(1, 1)), f.x), f.y); }
float fbm(vec2 p) { float s = 0.0; float a = 0.5; for (int i = 0; i < 5; i++) { s += a * n(p); p = p * 2.03 + 7.1; a *= 0.5; } return s; }
void main() {
  vec2 uv = gl_FragCoord.xy / r; vec2 p = vec2((uv.x - 0.5) * r.x / r.y, uv.y);
  float q = fbm(p * 2.2 + vec2(0.0, -t * 0.06));
  float f = fbm(p * 3.1 + vec2(q * 1.4, -t * 0.11));
  float base = exp(-pow(p.x * 1.15, 2.0) * 2.4) * smoothstep(0.95, 0.0, uv.y);
  float v = clamp(base * (0.35 + 0.95 * f * f) - 0.08, 0.0, 1.0);
  vec3 col = mix(c1, c2, smoothstep(0.25, 0.85, v));
  float a = v * fade * mix(0.85, 0.5, light);
  gl_FragColor = vec4(col * a, a);
}`;
	const sh = (type: number, src: string) => {
		const s = gl.createShader(type)!;
		gl.shaderSource(s, src);
		gl.compileShader(s);
		if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(`intro: shader: ${gl.getShaderInfoLog(s)}`);
		return s;
	};
	const prog = gl.createProgram()!;
	gl.attachShader(prog, sh(gl.VERTEX_SHADER, vs));
	gl.attachShader(prog, sh(gl.FRAGMENT_SHADER, fs));
	gl.linkProgram(prog);
	gl.useProgram(prog);
	const buf = gl.createBuffer();
	gl.bindBuffer(gl.ARRAY_BUFFER, buf);
	gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
	const a = gl.getAttribLocation(prog, 'a');
	gl.enableVertexAttribArray(a);
	gl.vertexAttribPointer(a, 2, gl.FLOAT, false, 0, 0);
	const u = (name: string) => gl.getUniformLocation(prog, name);
	const [ur, ut, ufade, uc1, uc2, ulight] = ['r', 't', 'fade', 'c1', 'c2', 'light'].map(u);
	const rgb = (hex: string) => {
		const n = parseInt(hex.replace('#', ''), 16);
		return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255] as const;
	};
	main.prepend(el);
	main.dataset.glow = '';
	const state = {
		visible: true,
		resize() {
			// Half resolution: the light is soft, and a quarter of the pixels is a quarter of the work.
			const scale = 0.5 * Math.min(devicePixelRatio || 1, 2);
			el.width = Math.max(1, Math.round(innerWidth * scale));
			el.height = Math.max(1, Math.round(innerHeight * scale));
			gl.viewport(0, 0, el.width, el.height);
			const s = getComputedStyle(document.body);
			const c1 = rgb(s.getPropertyValue('--l-fire').trim());
			const c2 = rgb(s.getPropertyValue('--l-fire-2').trim());
			gl.uniform3f(uc1!, ...c1);
			gl.uniform3f(uc2!, ...c2);
			gl.uniform1f(ulight!, matchMedia('(prefers-color-scheme: light)').matches ? 1 : 0);
		},
		draw(t: number, scrolled: number) {
			const fade = 1 - clamp(scrolled / 0.9);
			el.style.opacity = fade > 0 ? '1' : '0';
			if (fade <= 0) return;
			gl.uniform2f(ur!, el.width, el.height);
			gl.uniform1f(ut!, t / 1000);
			gl.uniform1f(ufade!, fade);
			gl.drawArrays(gl.TRIANGLES, 0, 3);
		},
		dispose() {
			gl.getExtension('WEBGL_lose_context')?.loseContext();
			el.remove();
		},
	};
	state.resize();
	return state;
}
