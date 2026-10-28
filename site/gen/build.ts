/**
 * Builds the site into dist/ with the Rust renderer's two wasm modules (renderer/README.md), so the static pages and
 * the worker's per-request pages come from the same renderer. Nothing here is native: the modules are
 * platform-independent build outputs (`uv run --script renderer/build.py`, or fetched with renderer/fetch.py).
 *
 * 1. Runs content/*.flecs under Flecs (flecs/flecs-doc.wasm, gen/tree.ts) to bake what the renderer leaves to TS:
 *    highlighted code (highlight.js) and lessons' glyphs (gen/glyphs.ts), by entity path; and to write the scripts the content asks for: a BenchHistory
 *    names a directory of benchmark history, which gen/bench/ turns into charts under it, as Flecs script.
 * 2. site-snapshot.wasm runs the same scripts, then the generated ones, with the baked HTML set, and returns the
 *    snapshot.
 * 3. site.wasm loads it and renders every page without comments, anonymous, into dist/; it also lists the pages and
 *    the discussable blocks.
 *    Narration (audio/README.md): it asks site.wasm for each page's spoken lines (`.gen/spoken.json`, what
 *    audio/generate.py voices). A page whose manifest (audio/manifests/<doc>.json) matches those lines, and whose
 *    audio files are at hand (.audio/files/, or served from R2 when SITE_AUDIO_REMOTE=1), gets `Audio {manifest}` in a
 *    second snapshot, and the renderer adds the player; its manifest and files are copied to dist/audio/. Any other
 *    page simply has no player, so the build never needs audio.
 * 4. Bundles client/*.ts, concatenates styles/, copies static/, puts the chat translator's ONNX Runtime in dist/ort/
 *    (`ortFiles`) and the anonymous readers' animals in dist/animals/<hash>.svg (`animalSprite`). For the worker, under .gen/:
 *
 *      site.wasm, site.cbor   the renderer and the snapshot (worker/renderer.ts)
 *      pages.json             every page: [{url, file, doc, discussable}] (what the worker routes)
 *      discussable.json       doc -> title and each block's text (what comments may attach to)
 *      build.json             {hash} of the module, the snapshot and the client scripts (part of the cache key), and
 *                             {assets}, of the client scripts' and styles' sources (client/nav.ts, worker/http.ts),
 *                             and {at}, when it was built (worker/presence.ts tells an old Durable Object by it)
 *      spoken.json            every page's spoken lines and their hash (the input of audio/generate.py)
 *      site.json              {origin, base}: where the site is served (`Served` on `site` in content/site.flecs)
 *
 *    and dist/sitemap.xml, every page but the 404 at its full URL (origin + base + path), dist/install.sh, the
 *    repo's installer, and dist/search.json, what the sidebar's search (client/side.ts) looks through.
 *
 * SITE_WASM_DIR names another directory holding site.wasm and site-snapshot.wasm (default renderer/target/site/).
 *
 *   bun gen/build.ts           # once
 *   bun gen/build.ts --watch   # again on every change under content/, gen/, client/, styles/
 */
import { createHash } from 'node:crypto';
import { createRequire } from 'node:module';
import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, watch, writeFileSync } from 'node:fs';
import path from 'node:path';
import { Site, snapshot, type Page, type SnapshotInput, type SpokenLine } from '../worker/site-wasm';
import { ANIMALS, ROBOT, sprite, symbolId } from './animals';
import { benchScript } from './bench';
import { glyphSvg } from './glyphs';
import { highlight } from './highlight';
import { dedent, load, prose, type Node } from './tree';

const root = path.join(import.meta.dirname, '..');
const content = path.join(root, 'content');
const dist = path.join(root, 'dist');
const gen = path.join(root, '.gen');
const modules = process.env.SITE_WASM_DIR ? path.resolve(process.env.SITE_WASM_DIR) : path.join(root, 'renderer/target/site');

/** Every .flecs file under content/, relative to it: schema.flecs, refs.flecs and nav.flecs first, then the rest in path order. */
function scripts(): string[] {
	const files = readdirSync(content, { recursive: true, encoding: 'utf8' }).filter((f) => f.endsWith('.flecs'));
	const rank = (f: string) => (f === 'schema.flecs' ? 0 : f === 'refs.flecs' ? 1 : f === 'nav.flecs' ? 2 : 3);
	return files.sort((a, b) => rank(a) - rank(b) || (a < b ? -1 : a > b ? 1 : 0));
}

const audioDir = path.join(root, 'audio');
/** Generated audio files, content-addressed (gitignored; audio/generate.py writes them). */
const audioFiles = path.join(root, '.audio', 'files');

/** A page's narration as audio/generate.py writes it (audio/README.md has the format). */
type Manifest = {
	doc: string;
	/** The hash of the spoken lines it was generated from (`spokenHash`). */
	hash: string;
	lines: { id: string; block: string; src: string; duration: number; words: [string, number, number][] }[];
};

/** The hash a manifest must carry to match the page: over each line's id, block and text, in order. */
function spokenHash(lines: SpokenLine[]): string {
	const h = createHash('sha256');
	for (const l of lines) h.update(`${l.id}\t${l.block}\t${l.text}\n`);
	return h.digest('hex').slice(0, 16);
}

/** The page's entity path: `/rfcs/capabilities` is `site.rfcs.capabilities`, `/` is `site.index`. */
const entityOf = (p: Page) => `site.${p.doc.replaceAll('/', '.')}`;

/**
 * The scripts the content asks the build to write, run after it: for each `BenchHistory {dir}`, the charts of the
 * benchmark history in `dir` (from the repo root, or BENCH_HISTORY when set) under that entity (gen/bench/).
 */
function generated(all: Map<string, Node>): { name: string; code: string }[] {
	const out: { name: string; code: string }[] = [];
	for (const n of all.values()) {
		const h = n.values.get('BenchHistory');
		if (!h) continue;
		const dir = process.env.BENCH_HISTORY ? path.resolve(process.env.BENCH_HISTORY) : path.join(root, '..', String(h.dir ?? ''));
		out.push({ name: `${n.path} (generated)`, code: benchScript(dir, n.path) });
	}
	return out;
}

/**
 * Which pages can have the player: those whose manifest matches their spoken lines and whose files are available.
 * Copies their manifests (content-addressed names) and files into dist/audio/ and returns the bakes that set
 * `Audio {manifest}` on them.
 */
function narration(pages: Page[], spoken: Map<string, { hash: string; lines: SpokenLine[] }>): Baked {
	const bakes: Baked = {};
	const remote = process.env.SITE_AUDIO_REMOTE === '1';
	for (const p of pages) {
		const file = path.join(audioDir, 'manifests', `${p.doc}.json`);
		if (!existsSync(file)) continue;
		const raw = readFileSync(file);
		const m = JSON.parse(raw.toString('utf8')) as Manifest;
		const want = spoken.get(p.url)!;
		if (m.hash !== want.hash) {
			console.warn(`audio: ${p.url}: the manifest is for other spoken lines (run audio/generate.py); no player`);
			continue;
		}
		const missing = m.lines.filter((l) => !existsSync(path.join(audioFiles, l.src)));
		if (missing.length && !remote) {
			console.warn(`audio: ${p.url}: ${missing.length} of ${m.lines.length} audio files missing from .audio/files (run audio/generate.py, or SITE_AUDIO_REMOTE=1); no player`);
			continue;
		}
		mkdirSync(path.join(dist, 'audio'), { recursive: true });
		for (const l of m.lines) {
			const src = path.join(audioFiles, l.src);
			if (existsSync(src)) cpSync(src, path.join(dist, 'audio', l.src));
		}
		const name = `${p.doc.replaceAll('/', '-')}.${createHash('sha256').update(raw).digest('hex').slice(0, 12)}.json`;
		writeFileSync(path.join(dist, 'audio', name), raw);
		bakes[entityOf(p)] = { audio: `/audio/${name}` };
	}
	return bakes;
}

const STYLES = ['fonts.css', 'tokens.css', 'doc-code.css', 'base.css', 'site.css', 'shell.css', 'art.css', 'learn.css'];

type Baked = SnapshotInput['bakes'];

/**
 * What the renderer leaves to TS: highlighting (Code and Command, set as `Highlighted {html}`) and each lesson's
 * glyph (gen/glyphs.ts, set as the lesson page's `Baked {html}`). By entity path.
 */
function bake(all: Map<string, Node>): Baked {
	const baked: Baked = {};
	for (const n of all.values()) {
		const code = n.values.get('Code');
		if (code) {
			const lang = String(code.lang ?? '');
			const text = dedent(String(code.text ?? ''));
			if (lang === 'rust' || lang === 'toml' || lang === 'bash') (baked[n.path] ??= {}).highlighted = highlight(text, lang);
		}
		const cmd = n.values.get('Command');
		if (cmd) (baked[n.path] ??= {}).highlighted = highlight(prose(String(cmd.text ?? '')), 'bash');
		const lesson = n.values.get('Lesson');
		if (lesson) {
			try {
				(baked[n.path] ??= {}).baked = glyphSvg(String(lesson.glyph ?? ''));
			} catch (e) {
				throw new Error(`${n.path}: ${(e as Error).message}`);
			}
		}
	}
	return baked;
}

/**
 * Every discussable page's URL must reach the worker first (wrangler.jsonc `assets.run_worker_first`), or it would be
 * served as the static, comment-less file. `true` sends every request to the worker (it strips the base, worker/base.ts).
 */
function checkRoutes(urls: string[]) {
	const config = readFileSync(path.join(root, 'wrangler.jsonc'), 'utf8');
	if (/"run_worker_first"\s*:\s*true\b/.test(config)) return;
	const m = /"run_worker_first"\s*:\s*\[([^\]]*)\]/.exec(config);
	if (!m) throw new Error('wrangler.jsonc: no assets.run_worker_first');
	const patterns = [...m[1].matchAll(/"([^"]+)"/g)].map((x) => x[1]);
	const matches = (p: string, u: string) => (p.endsWith('/*') ? u.startsWith(p.slice(0, -1)) : p === u);
	for (const u of urls) {
		if (!patterns.some((p) => matches(p, u))) throw new Error(`wrangler.jsonc: run_worker_first has no pattern for the discussable page ${u}`);
	}
}

/** A module from renderer/build.py, or the command that gets it. */
function wasmModule(file: string): WebAssembly.Module {
	const p = path.join(modules, file);
	if (!existsSync(p)) {
		throw new Error(
			`${p}: missing. These are build outputs of the Rust renderer, not built on this machine. Fetch them (from site/):\n` +
				'  uv run --script renderer/fetch.py                 # from the latest green CI run of the site workflow\n' +
				'  uv run --script renderer/fetch.py --scp HOST:DIR  # or from a machine that ran renderer/build.py\n' +
				'or build them with the Rust toolchain: uv run --script renderer/build.py',
		);
	}
	return new WebAssembly.Module(readFileSync(p));
}

/** `Served` on `site` (content/site.flecs): the origin and the base path every page sits under; empty when unset. */
function served(tree: Map<string, Node>): { origin: string; base: string } {
	const v = tree.get('site')?.values.get('Served');
	return { origin: String(v?.origin ?? ''), base: String(v?.base ?? '') };
}

/** Text of an HTML fragment: tags (and the comment and narration buttons' markup) out, entities in, spaces collapsed. */
function textOf(html: string): string {
	return html
		.replace(/<(script|style|svg|button|form)\b[\s\S]*?<\/\1>/g, ' ')
		.replace(/<[^>]+>/g, ' ')
		.replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, '&')
		.replace(/\s+/g, ' ')
		.trim();
}

/**
 * search.json: per page (not the 404), one entry for the page (its text before the first section) and one per
 * top-level section, from the page as rendered: `{u: URL with the base, t: page title, h: section title, x: text}`.
 */
function searchIndex(pages: Page[], html: Map<string, string>, tree: Map<string, Node>, base: string) {
	const out: { u: string; t: string; h: string; x: string }[] = [];
	for (const p of pages) {
		if (p.url === '/404') continue;
		const page = html.get(p.url) ?? '';
		const main = /<main\b[^>]*>([\s\S]*)<\/main>/.exec(page)?.[1] ?? '';
		const title = String(tree.get(entityOf(p))?.values.get('Title')?.text ?? p.url);
		const parts = main.split(/(?=<section id=")/);
		const href = (hash: string) => `${base}${p.url}${hash}`;
		out.push({ u: href(''), t: title, h: '', x: textOf(parts[0]!.replace(/<nav class="pager"[\s\S]*$/, '')).slice(0, 800) });
		for (const part of parts.slice(1)) {
			const id = /^<section id="([^"]+)"/.exec(part)?.[1];
			const h = /<h2[^>]*>([\s\S]*?)<\/h2>/.exec(part)?.[1];
			if (!id || h === undefined) continue;
			const body = part.replace(/<h2[^>]*>[\s\S]*?<\/h2>/, '').replace(/<nav class="pager"[\s\S]*$/, '');
			out.push({ u: href(`#${id}`), t: title, h: textOf(h), x: textOf(body).slice(0, 800) });
		}
	}
	return out;
}

const xmlEsc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

/**
 * The WebGPU ONNX Runtime the chat translator runs on (client/translate-worker.ts), from the onnxruntime-web that
 * @huggingface/transformers bundles: its loader as `<hash>.mjs` and its wasm (about 27 MB) in ORT_PART-sized pieces
 * `<hash>.<i>.wasm`, since Workers serves no static file over 25 MiB. The worker fetches the pieces and joins them.
 * Named by content, so a browser keeps them until onnxruntime-web changes.
 */
const ORT_PART = 16 * 1024 * 1024;
function ortFiles(): { mjs: string; parts: string[] } {
	const ort = path.dirname(createRequire(createRequire(path.join(root, 'package.json')).resolve('@huggingface/transformers')).resolve('onnxruntime-web/package.json'));
	const out = path.join(dist, 'ort');
	rmSync(out, { recursive: true, force: true });
	mkdirSync(out, { recursive: true });
	const mjs = readFileSync(path.join(ort, 'dist', 'ort-wasm-simd-threaded.asyncify.mjs'));
	const wasm = readFileSync(path.join(ort, 'dist', 'ort-wasm-simd-threaded.asyncify.wasm'));
	const id = createHash('sha256').update(mjs).update(wasm).digest('hex').slice(0, 16);
	writeFileSync(path.join(out, `${id}.mjs`), mjs);
	const parts: string[] = [];
	for (let at = 0; at < wasm.length; at += ORT_PART) {
		const name = `${id}.${parts.length}.wasm`;
		writeFileSync(path.join(out, name), wasm.subarray(at, at + ORT_PART));
		parts.push(name);
	}
	return { mjs: `${id}.mjs`, parts };
}

/**
 * The anonymous readers' animals (gen/animals.ts) as one sprite, dist/animals/<hash>.svg, named by its content so a
 * browser may keep it; what the client gets as `BLAZE_ANIMALS` (client/lib/presence.ts): the sprite's path under the
 * base, the names, in order, and the robot's symbol (a bot's face).
 */
function animalSprite(): { sprite: string; names: string[]; robot: string } {
	const svg = sprite();
	const name = `animals/${createHash('sha256').update(svg).digest('hex').slice(0, 16)}.svg`;
	rmSync(path.join(dist, 'animals'), { recursive: true, force: true });
	write(path.join(dist, name), svg);
	return { sprite: name, names: ANIMALS.map((a) => a.name), robot: symbolId(ROBOT.name) };
}

function write(file: string, data: string | Uint8Array) {
	mkdirSync(path.dirname(file), { recursive: true });
	writeFileSync(file, data);
}

async function build() {
	const t0 = performance.now();
	const files = scripts();
	const tree = await load(content, files.map((f) => path.join(content, f)), ['site', 'refs', 'nav']);
	const input: SnapshotInput = {
		scripts: [...files.map((name) => ({ name, code: readFileSync(path.join(content, name), 'utf8') })), ...generated(tree)],
		bakes: bake(tree),
	};
	let snap = snapshot(wasmModule('site-snapshot.wasm'), input);
	let site = Site.load(wasmModule('site.wasm'), snap);
	const pages = site.pages();
	checkRoutes(pages.filter((p) => p.discussable).map((p) => p.url));

	const spoken = new Map(pages.map((p) => {
		const lines = site.spoken(p.url);
		return [p.url, { hash: spokenHash(lines), lines }] as const;
	}));
	write(path.join(gen, 'spoken.json'), JSON.stringify(pages.map((p) => ({ ...p, ...spoken.get(p.url)! })), null, '\t'));
	rmSync(path.join(dist, 'audio'), { recursive: true, force: true });
	const audio = narration(pages, spoken);
	if (Object.keys(audio).length) {
		for (const [k, v] of Object.entries(audio)) input.bakes[k] = { ...input.bakes[k], ...v };
		snap = snapshot(wasmModule('site-snapshot.wasm'), input);
		site = Site.load(wasmModule('site.wasm'), snap);
	}

	const client = readdirSync(path.join(root, 'client')).filter((f) => f.endsWith('.ts')).map((f) => path.join(root, 'client', f));
	// What the pages run and look with: the client router swaps in a page only when its build had the same (nav.ts).
	const assetsHash = createHash('sha256');
	for (const dir of ['client', 'styles']) {
		for (const f of (readdirSync(path.join(root, dir), { recursive: true }) as string[]).sort()) {
			const file = path.join(root, dir, f);
			if (statSync(file).isFile()) assetsHash.update(`${dir}/${f}`).update(readFileSync(file));
		}
	}
	const animals = animalSprite();
	assetsHash.update(animals.sprite);
	const assets = assetsHash.digest('hex').slice(0, 16);
	// `root` keeps the output flat (dist/js/<name>.js).
	const bundled = await Bun.build({
		entrypoints: client,
		root: path.join(root, 'client'),
		outdir: path.join(dist, 'js'),
		minify: true,
		target: 'browser',
		define: { BLAZE_ASSETS: JSON.stringify(assets), BLAZE_ORT: JSON.stringify(ortFiles()), BLAZE_ANIMALS: JSON.stringify(animals) },
	});
	if (!bundled.success) throw new AggregateError(bundled.logs, 'client bundle failed');

	rmSync(path.join(dist, 'rfcs'), { recursive: true, force: true });
	const rendered = new Map<string, string>();
	for (const p of pages) {
		const html = site.render(p.url, [], { viewer: null, signIn: true });
		rendered.set(p.url, html);
		write(path.join(dist, p.file), html);
	}
	writeFileSync(path.join(dist, 'site.css'), STYLES.map((s) => readFileSync(path.join(root, 'styles', s), 'utf8')).join('\n'));
	cpSync(path.join(root, 'static'), dist, { recursive: true });
	// The installer, at <base>/install.sh, from the repo root; the worker also answers <base>/ with it for curl
	// (`curl -fsSL https://ix.dev/blaze | sh`).
	cpSync(path.join(root, '..', 'install.sh'), path.join(dist, 'install.sh'));

	write(path.join(gen, 'site.cbor'), snap);
	cpSync(path.join(modules, 'site.wasm'), path.join(gen, 'site.wasm'));
	write(path.join(gen, 'pages.json'), JSON.stringify(pages, null, '\t'));
	const at = served(tree);
	write(path.join(gen, 'site.json'), JSON.stringify(at, null, '\t'));
	if (at.origin) {
		const urls = pages.filter((p) => p.url !== '/404').map((p) => `<url><loc>${xmlEsc(at.origin + at.base + p.url)}</loc></url>`);
		write(path.join(dist, 'sitemap.xml'), `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${urls.join('\n')}\n</urlset>\n`);
	}
	write(path.join(dist, 'search.json'), JSON.stringify(searchIndex(pages, rendered, tree, at.base)));
	write(path.join(gen, 'discussable.json'), JSON.stringify(site.discussable(), null, '\t'));
	const hash = createHash('sha256');
	for (const f of ['site.cbor', 'site.wasm']) hash.update(f).update(readFileSync(path.join(gen, f)));
	for (const f of readdirSync(path.join(dist, 'js')).sort()) hash.update(f).update(readFileSync(path.join(dist, 'js', f)));
	write(path.join(gen, 'build.json'), JSON.stringify({ hash: hash.digest('hex').slice(0, 16), assets, at: Date.now() }));
	const narrated = Object.keys(audio).length ? `, ${Object.keys(audio).length} with audio` : '';
	console.log(`built ${pages.length} pages${narrated} (snapshot ${snap.length} bytes) in ${Math.round(performance.now() - t0)} ms`);
}

await build();
if (process.argv.includes('--watch')) {
	let timer: Timer | undefined;
	for (const dir of ['content', 'gen', 'client', 'styles', 'static', 'audio/manifests']) {
		if (!existsSync(path.join(root, dir))) continue;
		watch(path.join(root, dir), { recursive: true }, () => {
			clearTimeout(timer);
			timer = setTimeout(() => build().catch((e) => console.error(String(e))), 50);
		});
	}
}
