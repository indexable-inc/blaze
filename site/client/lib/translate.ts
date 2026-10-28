/**
 * Chat messages in another language than the reader's, translated in the browser (client/translate-worker.ts runs
 * HY-MT1.5-1.8B on WebGPU). Only on a computer: a phone or tablet, a browser without WebGPU, a slow or metered
 * connection, little memory or little free storage never downloads the model. Nothing downloads until a message
 * needs translating and the reader has chatted (sent a message) or asked (a message's Translate button), and then
 * once: the model and the runtime stay in Cache Storage, and once there it translates without asking.
 *
 * A message's language comes from the browser's LanguageDetector when it has one (Chrome), else from its script
 * (Han, kana, Hangul, Cyrillic, Arabic, ...), so without the detector Latin-script languages go untranslated.
 * `localStorage['blaze.translate'] = 'off'` turns it off; the progress line's Stop does that.
 */

import { modelFile } from './hymt';

const OFF_KEY = 'blaze.translate';
/** Free storage the model needs, with room to spare. */
const NEED_BYTES = 2.5e9;

/** Languages HY-MT1.5 translates between (its model card), by ISO 639-1 code, with the name its prompt uses. */
const LANGS: Record<string, string> = {
	zh: 'Chinese', en: 'English', fr: 'French', pt: 'Portuguese', es: 'Spanish', ja: 'Japanese', tr: 'Turkish',
	ru: 'Russian', ar: 'Arabic', ko: 'Korean', th: 'Thai', it: 'Italian', de: 'German', vi: 'Vietnamese', ms: 'Malay',
	id: 'Indonesian', tl: 'Filipino', hi: 'Hindi', pl: 'Polish', cs: 'Czech', nl: 'Dutch', km: 'Khmer', my: 'Burmese',
	fa: 'Persian', gu: 'Gujarati', ur: 'Urdu', te: 'Telugu', mr: 'Marathi', he: 'Hebrew', bn: 'Bengali', ta: 'Tamil',
	uk: 'Ukrainian', bo: 'Tibetan', kk: 'Kazakh', mn: 'Mongolian', ug: 'Uyghur',
};

/** The script of a text when it names one language well enough, checked in this order. */
const SCRIPTS: [RegExp, string][] = [
	[/[\p{Script=Hiragana}\p{Script=Katakana}]/u, 'ja'],
	[/\p{Script=Hangul}/u, 'ko'],
	[/\p{Script=Han}/u, 'zh'],
	[/\p{Script=Cyrillic}/u, 'ru'],
	[/\p{Script=Arabic}/u, 'ar'],
	[/\p{Script=Hebrew}/u, 'he'],
	[/\p{Script=Thai}/u, 'th'],
	[/\p{Script=Devanagari}/u, 'hi'],
	[/\p{Script=Bengali}/u, 'bn'],
	[/\p{Script=Tamil}/u, 'ta'],
	[/\p{Script=Telugu}/u, 'te'],
	[/\p{Script=Gujarati}/u, 'gu'],
	[/\p{Script=Khmer}/u, 'km'],
	[/\p{Script=Myanmar}/u, 'my'],
	[/\p{Script=Tibetan}/u, 'bo'],
];

type Detector = { detect(text: string): Promise<{ detectedLanguage: string; confidence: number }[]> };
type DetectorApi = { availability(): Promise<string>; create(): Promise<Detector> };
type Gpu = { requestAdapter(o?: { powerPreference?: string }): Promise<{ features: { has(f: string): boolean } } | null> };
type Nav = Navigator & {
	gpu?: Gpu;
	userAgentData?: { mobile?: boolean };
	deviceMemory?: number;
	connection?: { saveData?: boolean; effectiveType?: string };
};

export type State =
	| { s: 'idle' }
	| { s: 'loading'; loaded: number; total: number }
	| { s: 'ready' }
	| { s: 'failed'; message: string };

const langOf = (lang: string) => lang.toLowerCase().split(/[-_]/)[0];
const target = langOf(navigator.language || 'en');
export const languageName = (code: string) => {
	try {
		return new Intl.DisplayNames([navigator.language || 'en'], { type: 'language' }).of(code) ?? LANGS[code] ?? code;
	} catch {
		return LANGS[code] ?? code;
	}
};

export function off(): boolean {
	try {
		return localStorage.getItem(OFF_KEY) === 'off';
	} catch {
		return false;
	}
}

/** Turn translation off for good (this browser), and stop a download under way. */
export function stop(): void {
	try {
		localStorage.setItem(OFF_KEY, 'off');
	} catch {
		// Storage refused: off for this page.
	}
	worker?.terminate();
	worker = null;
	setState({ s: 'idle' });
	for (const [, w] of waiting) w(null);
	waiting.clear();
}

/** Which model build this computer runs, or null when it should not download one at all. */
const capable: Promise<'q4f16' | 'q4' | null> = (async () => {
	if (off() || !LANGS[target]) return null;
	const nav = navigator as Nav;
	const ua = navigator.userAgent;
	if (nav.userAgentData?.mobile || /Android|iPhone|iPad|iPod|Mobile/i.test(ua)) return null;
	if (/Macintosh/.test(ua) && navigator.maxTouchPoints > 1) return null; // An iPad asking for the desktop site.
	if (!matchMedia('(pointer: fine)').matches) return null;
	if (nav.connection?.saveData || /2g/.test(nav.connection?.effectiveType ?? '')) return null;
	if (nav.deviceMemory !== undefined && nav.deviceMemory < 8) return null;
	try {
		const { quota = 0, usage = 0 } = await navigator.storage.estimate();
		if (quota - usage < NEED_BYTES) return null;
		const adapter = await nav.gpu?.requestAdapter({ powerPreference: 'high-performance' });
		if (!adapter) return null;
		return adapter.features.has('shader-f16') ? 'q4f16' : 'q4';
	} catch {
		return null;
	}
})();

const detector: Promise<Detector | null> = (async () => {
	const api = (globalThis as { LanguageDetector?: DetectorApi }).LanguageDetector;
	try {
		return api && (await api.availability()) === 'available' ? await api.create() : null;
	} catch {
		return null;
	}
})();

/** The language of `text` as a code HY-MT knows, or null when unsure. */
async function detect(text: string): Promise<string | null> {
	const plain = text.replace(/https?:\/\/\S+/g, ' ').trim();
	if (!/\p{L}.*\p{L}/su.test(plain)) return null;
	const d = await detector;
	if (d) {
		const [top] = await d.detect(plain);
		if (top && top.confidence >= 0.6 && LANGS[langOf(top.detectedLanguage)]) return langOf(top.detectedLanguage);
	}
	for (const [re, lang] of SCRIPTS) if (re.test(plain)) return lang;
	return null;
}

let worker: Worker | null = null;
let base = '/';

/** Where the site is (`/blaze/`), for the worker's script. */
export function setup(siteBase: string): void {
	base = siteBase;
}
let state: State = { s: 'idle' };
let nextId = 1;
const waiting = new Map<number, (text: string | null) => void>();
const listeners = new Set<(s: State) => void>();

function setState(s: State): void {
	state = s;
	for (const l of listeners) l(s);
}

/** Calls `l` now and on every change of the translator's state, until the returned function is called. */
export function watch(l: (s: State) => void): () => void {
	listeners.add(l);
	l(state);
	return () => listeners.delete(l);
}

function start(dtype: 'q4f16' | 'q4'): Worker {
	const w = new Worker(new URL(`${base}js/translate-worker.js`, location.origin), {
		type: 'module',
		name: 'translator',
	});
	w.onmessage = (e: MessageEvent<{ t: string; id?: number; text?: string; message?: string; loaded?: number; total?: number }>) => {
		const m = e.data;
		if (m.t === 'progress') setState({ s: 'loading', loaded: m.loaded ?? 0, total: m.total ?? 0 });
		else if (m.t === 'ready') setState({ s: 'ready' });
		else if (m.t === 'done' && m.id !== undefined) {
			waiting.get(m.id)?.(m.text ?? null);
			waiting.delete(m.id);
		} else if (m.t === 'error') {
			if (m.id !== undefined) {
				waiting.get(m.id)?.(null);
				waiting.delete(m.id);
			} else {
				setState({ s: 'failed', message: m.message ?? 'The translator did not start.' });
				for (const [, r] of waiting) r(null);
				waiting.clear();
			}
		}
	};
	w.onerror = (e) => setState({ s: 'failed', message: e.message || 'The translator stopped.' });
	setState({ s: 'loading', loaded: 0, total: 0 });
	w.postMessage({ t: 'load', dtype });
	// Ask the browser to keep the model when storage runs short; it may say no.
	void navigator.storage.persist?.().catch(() => false);
	return w;
}

/** The language `text` is in when it should be translated for this reader, else null. Never downloads anything. */
export async function needs(text: string): Promise<string | null> {
	if (!(await capable) || off()) return null;
	const from = await detect(text);
	return from && from !== target ? from : null;
}

let allowed = false;

/** The reader chatted or asked: downloading the model is fine now. Lasts this page; once cached it no longer asks. */
export function allow(): void {
	allowed = true;
	for (const l of allowListeners) l();
}
const allowListeners = new Set<() => void>();
export function onAllow(l: () => void): void {
	allowListeners.add(l);
}

/** Whether translating would start a download the reader has not agreed to: true until they do or the model is cached. */
export async function mustAsk(): Promise<boolean> {
	if (allowed || worker) return false;
	const dtype = await capable;
	if (!dtype) return false;
	try {
		const cache = await caches.open('transformers-cache');
		if (await cache.match(modelFile(`onnx/model_${dtype}.onnx_data`))) return false;
	} catch {
		// No Cache Storage: ask.
	}
	return true;
}

/** `text` in the reader's language, or null when it could not be translated. Starts the translator on first use. */
export async function translate(text: string): Promise<string | null> {
	const dtype = await capable;
	if (!dtype || off() || state.s === 'failed') return null;
	worker ??= start(dtype);
	const id = nextId++;
	const done = new Promise<string | null>((r) => waiting.set(id, r));
	worker.postMessage({ t: 'run', id, text, to: LANGS[target] });
	return done;
}
