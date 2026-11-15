/**
 * The intro's sound (client/intro.ts): the soundtrack that goes with the wordmark's drop and catch, the campfire that
 * burns on after it, and the Start button's knock. All synthesised (client/lib/intro-soundtrack.ts and
 * intro-track.ts, ported from anim's src/audio.rs), no files.
 *
 * - Nothing sounds before the reader asks: `play` and `tap` are called from a click or a press, and only then is the
 *   context made or resumed (client/lib/sound.ts `unlock`, the site's one context and output: the compressor and the
 *   output gain, so the intro peaks near -14 dBFS like the lessons' sounds).
 * - The switch is the site's (`blaze.sound`, sound.ts): `play` turns it on, `stop` turns it off.
 * - The soundtrack is rendered once, in a worker (client/soundtrack.ts), the first time `play` is called; the
 *   campfire follows it and is laid in when it arrives.
 * - `play` answers with the moment, on the page's clock, at which the soundtrack's first sample is heard. The picture
 *   starts its timeline at that moment, so a letter lands on its note.
 *
 * Under client/lib/ (not bundled as an entry).
 */

import type { Ask, Made } from './intro-soundtrack';
import { CAMPFIRE_AT } from './intro-timeline';
import { type Stereo, select } from './intro-track';
import { enabled, setEnabled, unlock } from './sound';

/** The rate everything is rendered at; the context resamples if its own differs. */
const SR = 48000;
/**
 * Levels into the site's output (sound.ts: the compressor, then the output gain), measured through `output` on an
 * OfflineAudioContext in Chrome: the soundtrack's loudest moment (the catch) leaves it at -14.7 dBFS, the campfire's
 * pops near -17, and the Start button's knock at -18.5.
 */
const TRACK = 0.9;
const SELECT = 2.5;
/** Seconds between asking for the soundtrack and its first sample: time for the picture to clear. */
const LEAD = 0.08;
/** The time constant the sound fades out with when it is turned off. */
const HUSH = 0.1;

type Rendered = { track: Promise<Stereo>; fire: Promise<Stereo> };
let rendered: Rendered | null = null;

/** The soundtrack and the campfire, rendered once by the worker beside this script (/js/soundtrack.js). */
function render(): Rendered {
	if (rendered) return rendered;
	const worker = new Worker(import.meta.url.replace(/[^/]*$/, 'soundtrack.js'));
	const gate = () => {
		let ok!: (s: Stereo) => void;
		let fail!: (e: unknown) => void;
		const got = new Promise<Stereo>((res, rej) => {
			ok = res;
			fail = rej;
		});
		// A failure is reported by `play`, which awaits the soundtrack; the campfire's is the same one.
		got.catch(() => {});
		return { got, ok, fail };
	};
	const [track, fire] = [gate(), gate()];
	worker.onmessage = (e: MessageEvent<Made>) => {
		if (e.data.kind === 'track') track.ok(e.data);
		else {
			fire.ok(e.data);
			worker.terminate();
		}
	};
	worker.onerror = (e) => {
		// The next press tries again.
		rendered = null;
		track.fail(e);
		fire.fail(e);
		worker.terminate();
	};
	const ask: Ask = { sr: SR, seed: Math.floor(Math.random() * 4294967296) };
	worker.postMessage(ask);
	return (rendered = { track: track.got, fire: fire.got });
}

function source(ctx: BaseAudioContext, s: Stereo): AudioBufferSourceNode {
	const buf = ctx.createBuffer(2, s.l.length, SR);
	buf.getChannelData(0).set(s.l);
	buf.getChannelData(1).set(s.r);
	const src = ctx.createBufferSource();
	src.buffer = buf;
	return src;
}

/** When, on the page's clock (performance.now), the sample scheduled at context time `at` is heard. */
function heardAt(ctx: AudioContext, at: number): number {
	const now = performance.now();
	const stamp = typeof ctx.getOutputTimestamp === 'function' ? ctx.getOutputTimestamp() : undefined;
	if (stamp?.performanceTime && stamp.contextTime !== undefined) {
		const t = stamp.performanceTime + (at - stamp.contextTime) * 1000;
		// A context that has only just started can report a stale pair: trust it only when it lands near now.
		if (t >= now && t < now + 1000) return t;
	}
	return now + (at - ctx.currentTime + (ctx.outputLatency || ctx.baseLatency || 0)) * 1000;
}

/** What is sounding: the soundtrack's gain and its sources. */
let playing: { gain: GainNode; sources: AudioBufferSourceNode[] } | null = null;
/** Counts every `play`, `stop` and `leave`: a soundtrack that arrives for an earlier one is dropped. */
let run = 0;

function hush(tau: number) {
	const p = playing;
	playing = null;
	if (!p) return;
	const ctx = p.gain.context;
	p.gain.gain.setTargetAtTime(0, ctx.currentTime, tau);
	for (const s of p.sources) s.stop(ctx.currentTime + 6 * tau);
	setTimeout(() => p.gain.disconnect(), 7000 * tau + 100);
}

export const sounding = () => playing !== null;

/**
 * Turns the sound on and plays the soundtrack from its start, then the campfire for as long as the sound is on.
 * Call it from a gesture's handler. Resolves with the moment (performance.now) its first sample is heard, or null
 * when there is no sound to play (no Web Audio, the worker failed, or it was stopped while it rendered).
 */
export async function play(): Promise<number | null> {
	const n = ++run;
	hush(0.02);
	setEnabled(true);
	const o = unlock();
	if (!o) return null;
	const ctx = o.ctx as AudioContext;
	let track: Stereo;
	let made: Rendered;
	try {
		made = render();
		track = await made.track;
		// Running before anything is scheduled, so the soundtrack's start is a real time on the context's clock.
		if (ctx.state !== 'running') await ctx.resume();
	} catch (e) {
		console.error('intro sound:', e);
		return null;
	}
	if (n !== run || ctx.state !== 'running') return null;
	const gain = ctx.createGain();
	gain.gain.value = TRACK;
	gain.connect(o.bus);
	const at = ctx.currentTime + LEAD;
	const src = source(ctx, track);
	src.connect(gain);
	src.start(at);
	const now = { gain, sources: [src] };
	playing = now;
	// The campfire fades in over about a second as the soundtrack's own fire dies down, and loops.
	made.fire.then(
		(f) => {
			if (playing !== now) return;
			const level = ctx.createGain();
			level.gain.value = 0;
			level.connect(gain);
			const loop = source(ctx, f);
			loop.loop = true;
			loop.connect(level);
			const from = Math.max(ctx.currentTime, at + CAMPFIRE_AT);
			loop.start(from);
			level.gain.setTargetAtTime(1, from, 1);
			now.sources.push(loop);
		},
		() => {},
	);
	return heardAt(ctx, at);
}

/** Turns the sound off: what is playing fades out, and the site's switch goes off. */
export function stop() {
	++run;
	hush(HUSH);
	setEnabled(false, 8 * HUSH);
}

/** The page is going away (or into the back/forward cache): silence, without touching the switch. */
export function leave() {
	++run;
	hush(0.01);
}

/** The tab was hidden or shown: a hidden tab does not keep a fire burning in the reader's ears. */
export function hidden(is: boolean) {
	if (!playing) return;
	const ctx = playing.gain.context as AudioContext;
	if (is) void ctx.suspend().catch(() => {});
	else void ctx.resume().catch(() => {});
}

let knock: Stereo | null = null;

/**
 * Gets the knock ready ahead of `tap`, when the site's sound is on: the context made or resumed and the knock
 * rendered, which is most of what `tap` would otherwise wait for. Call it from a gesture's handler (a press).
 */
export function ready() {
	if (!enabled() || !unlock()) return;
	knock ??= select(SR, 7);
}

/**
 * The Start button's knock, when the site's sound is on. Call it from a gesture's handler. Null when nothing will
 * sound; else a promise of the moment (performance.now) the knock is heard, so the page's exit can start on it. A
 * context made by this very press takes a moment to run, and the knock is scheduled once it does.
 */
export function tap(): Promise<number> | null {
	if (!enabled()) return null;
	const o = unlock();
	if (!o) return null;
	const ctx = o.ctx as AudioContext;
	try {
		knock ??= select(SR, 7);
		const g = ctx.createGain();
		g.gain.value = SELECT;
		g.connect(o.bus);
		const src = source(ctx, knock);
		src.connect(g);
		const start = () => {
			const at = ctx.currentTime + 0.02;
			src.start(at);
			return heardAt(ctx, at);
		};
		return ctx.state === 'running' ? Promise.resolve(start()) : ctx.resume().then(start);
	} catch {
		return null;
	}
}
