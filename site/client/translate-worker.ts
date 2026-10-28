/**
 * The chat translator (client/lib/translate.ts starts it): HY-MT1.5-1.8B (Tencent) on WebGPU through
 * transformers.js, off the page's thread. Pages never load this; only a desktop with WebGPU that meets a message in
 * another language does.
 *
 * The model (about 1.2 GB as q4f16, 1.4 GB as q4) comes from Hugging Face at a pinned revision and stays in the
 * browser's Cache Storage (`transformers-cache`). ONNX Runtime's wasm comes from this site in pieces (gen/build.ts
 * `ortFiles`), joined here and kept in Cache Storage too (`blaze-ort`).
 *
 * In:  {t: "load", dtype: "q4f16" | "q4"}       then any number of
 *      {t: "run", id, text, to}                  `to` an English language name ("Russian")
 * Out: {t: "progress", loaded, total}           while the model downloads
 *      {t: "ready"}  {t: "done", id, text}  {t: "error", id?, message}
 */
import { env, pipeline, type TextGenerationPipeline } from '@huggingface/transformers';
import { MODEL, REVISION } from './lib/hymt';

declare const BLAZE_ORT: { mjs: string; parts: string[] };

const ORT_CACHE = 'blaze-ort';

type In = { t: 'load'; dtype: 'q4f16' | 'q4' } | { t: 'run'; id: number; text: string; to: string };

const ortUrl = (f: string) => new URL(`../ort/${f}`, import.meta.url).href;

/** The runtime's wasm: its pieces from the cache or the site, joined. Pieces of older builds are dropped. */
async function ortWasm(): Promise<Uint8Array> {
	const cache = await caches.open(ORT_CACHE);
	const want = new Set(BLAZE_ORT.parts.map(ortUrl));
	for (const req of await cache.keys()) if (!want.has(req.url)) await cache.delete(req);
	const parts = await Promise.all(
		[...want].map(async (url) => {
			let res = await cache.match(url);
			if (!res) {
				res = await fetch(url);
				if (!res.ok) throw new Error(`${url}: HTTP ${res.status}`);
				await cache.put(url, res.clone());
			}
			return new Uint8Array(await res.arrayBuffer());
		}),
	);
	const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
	let at = 0;
	for (const p of parts) {
		out.set(p, at);
		at += p.length;
	}
	return out;
}

let model: Promise<TextGenerationPipeline> | null = null;

async function load(dtype: 'q4f16' | 'q4'): Promise<TextGenerationPipeline> {
	const onnx = env.backends.onnx;
	if (!onnx.wasm) throw new Error('This build of transformers.js has no wasm backend.');
	onnx.wasm.wasmPaths = { mjs: ortUrl(BLAZE_ORT.mjs) };
	onnx.wasm.wasmBinary = await ortWasm();
	env.allowLocalModels = false;
	return pipeline('text-generation', MODEL, {
		revision: REVISION,
		dtype,
		device: 'webgpu',
		progress_callback: (p) => {
			if (p.status === 'progress_total') postMessage({ t: 'progress', loaded: p.loaded, total: p.total });
		},
	});
}

/** HY-MT's own prompts: one for Chinese, one for every other target (its model card). */
function prompt(text: string, to: string): string {
	return to === 'Chinese'
		? `将以下文本翻译为中文，注意只需要输出翻译后的结果，不要额外解释：\n\n${text}`
		: `Translate the following segment into ${to}, without additional explanation.\n\n${text}`;
}

function reply(out: unknown): string {
	const first = Array.isArray(out) ? (out[0] as { generated_text?: unknown }) : null;
	const g = first?.generated_text;
	const last = Array.isArray(g) ? (g.at(-1) as { content?: unknown } | undefined)?.content : g;
	if (typeof last !== 'string') throw new Error('The model gave no text.');
	return last.trim();
}

// One message at a time: the GPU does one generation at once anyway.
let queue = Promise.resolve();

onmessage = (e: MessageEvent<In>) => {
	const m = e.data;
	if (m.t === 'load') {
		model ??= load(m.dtype);
		model.then(
			() => postMessage({ t: 'ready' }),
			(err: unknown) => postMessage({ t: 'error', message: String(err instanceof Error ? err.message : err) }),
		);
		return;
	}
	queue = queue.then(async () => {
		try {
			if (!model) throw new Error('Not loaded.');
			const gen = await model;
			const out = await gen([{ role: 'user', content: prompt(m.text, m.to) }], { max_new_tokens: 512, do_sample: false });
			postMessage({ t: 'done', id: m.id, text: reply(out) });
		} catch (err) {
			postMessage({ t: 'error', id: m.id, message: String(err instanceof Error ? err.message : err) });
		}
	});
};
