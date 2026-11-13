/**
 * The intro's sounds, synthesised sample by sample into one stereo buffer: a port of the parts of anim's
 * src/audio.rs (indexable-inc/anim) the site's intro uses. Each sound sits somewhere between the speakers and sends
 * some of itself to a shared reverb, which gives the room around it.
 *
 * This file is the track and its small instruments (the mallet, the knocks, the whoosh, the bell, the reverb): what
 * the page itself needs for the one sound it plays without the soundtrack (`select`, the Start button). The
 * soundtrack, with the fire, is client/lib/intro-soundtrack.ts, rendered in a worker.
 *
 * Plain arrays, no Web Audio: client/lib/intro-sound.ts plays what comes out.
 *
 * Under client/lib/ (not bundled as an entry).
 */

import { Rng } from './rng';

const TAU = 2 * Math.PI;
const { PI, SQRT2, exp, sin, min } = Math;

/** Risset's bell: partial ratios, loudness and how long each rings. */
const BELL: [ratio: number, amp: number, ring: number][] = [
	[0.56, 1, 1],
	[0.92, 0.67, 0.9],
	[1.19, 1, 0.65],
	[1.71, 1.8, 0.55],
	[2, 2.67, 0.33],
	[2.74, 1.67, 0.35],
	[3, 1.46, 0.25],
	[3.76, 1.33, 0.2],
	[4.07, 1.33, 0.15],
];

/** A stereo buffer. */
export type Stereo = { l: Float32Array; r: Float32Array };

export class Track {
	l: Float32Array;
	r: Float32Array;
	/** Mono feed into the reverb. */
	send: Float32Array;

	constructor(
		readonly sr: number,
		seconds: number,
		readonly rng: Rng,
	) {
		const len = Math.floor(seconds * sr);
		this.l = new Float32Array(len);
		this.r = new Float32Array(len);
		this.send = new Float32Array(len);
	}

	at(t: number): number {
		return Math.floor(Math.max(0, t) * this.sr);
	}

	/**
	 * Adds `f(seconds since start)` for `dur` seconds from `t0`. It pans from `pan0` to `pan1` (-1 left, 1 right) and
	 * sends `send` of itself to the reverb.
	 */
	add(t0: number, dur: number, pan0: number, pan1: number, send: number, f: (t: number) => number) {
		const a = this.at(t0);
		const n = Math.min(this.at(dur), this.l.length - a);
		const { l, r, send: wet, sr } = this;
		// Equal power, so a sound keeps its loudness as it moves.
		const gains = (p: number): [number, number] => {
			const angle = ((Math.min(1, Math.max(-1, p)) + 1) * PI) / 4;
			return [Math.cos(angle) * SQRT2, Math.sin(angle) * SQRT2];
		};
		let [gl, gr] = gains(pan0);
		const moves = pan0 !== pan1;
		const steps = this.at(dur);
		for (let i = 0; i < n; i++) {
			const v = f(i / sr);
			if (moves) [gl, gr] = gains(pan0 + (pan1 - pan0) * (i / steps));
			l[a + i] += v * gl;
			r[a + i] += v * gr;
			wet[a + i] += v * send;
		}
	}
}

/** A short noise tick with a ringing tone, for small clicks. */
export function click(tr: Track, t0: number, amp: number, freq: number, pan: number) {
	let prev = 0;
	tr.add(t0, 0.06, pan, pan, 0.2, (t) => {
		const n = tr.rng.range(-1, 1);
		const hp = n - prev;
		prev = n;
		return amp * (exp(-t * 600) * hp * 0.6 + exp(-t * 70) * sin(TAU * freq * t) * 0.5);
	});
}

/** The weight under a press: a short, round knock whose pitch drops a little, like a well damped key bottoming out. */
export function thock(tr: Track, t0: number, amp: number, freq: number, pan: number) {
	let phase = 0;
	tr.add(t0, 0.12, pan, pan, 0.1, (t) => {
		phase += (TAU * freq * (1 + 0.5 * exp(-t * 90))) / tr.sr;
		return amp * 0.35 * min(t / 0.0015, 1) * exp(-t * 38) * sin(phase);
	});
}

/** A letter landing: a low knock whose pitch falls, and a click on top. */
export function thud(tr: Track, t0: number, size: number) {
	let phase = 0;
	tr.add(t0, 0.4, 0, 0, 0.15, (t) => {
		phase += (TAU * (42 + 30 * exp(-t * 18))) / tr.sr;
		return 0.5 * size * exp(-t * 11) * sin(phase);
	});
	click(tr, t0, 0.12 * size, 1800, 0);
}

/** Filtered noise whose brightness follows `cutoff(progress)`: the air a falling letter pushes. */
export function whoosh(tr: Track, t0: number, dur: number, amp: number, pan0: number, pan1: number, cutoff: (p: number) => number) {
	let low = 0;
	tr.add(t0, dur, pan0, pan1, 0.25, (t) => {
		const p = t / dur;
		const k = 1 - exp((-TAU * cutoff(p)) / tr.sr);
		low += k * (tr.rng.range(-1, 1) - low);
		return amp * sin(PI * p) ** 2 * low * 3;
	});
}

export function bell(tr: Track, t0: number, freq: number, amp: number, dur: number, pan: number) {
	for (const [ratio, a, d] of BELL) {
		const f = freq * ratio;
		const ring = dur * d;
		tr.add(t0, ring, pan, pan, 0.5, (t) => ((amp * a) / 14) * exp((-4.6 * t) / ring) * sin(TAU * f * t));
	}
}

/**
 * A warm mallet note for a letter landing: a soft felt attack, a marimba-like body whose overtones die first, and a
 * detuned twin on the other side for width, ringing long enough to join the chord.
 */
export function mallet(tr: Track, t0: number, freq: number, amp: number, ring: number, pan: number) {
	const detune = 1 + tr.rng.range(0.001, 0.0025);
	for (const [f, p, a] of [
		[freq, pan, 1],
		[freq * detune, -0.6 * pan, 0.45],
	] as const) {
		tr.add(t0, ring, p, p, 0.45, (t) => {
			const attack = min(t / 0.004, 1);
			const body = exp((-t * 3) / ring) * sin(TAU * f * t);
			const second = 0.22 * exp((-t * 9) / ring) * sin(TAU * 2 * f * t);
			const wood = 0.12 * exp(-t * 40) * sin(TAU * 3.93 * f * t);
			return amp * a * attack * (body + second + wood);
		});
	}
	// The felt: a few ms of soft, low-passed noise.
	const k = 1 - exp((-TAU * min(freq * 3, 5000)) / tr.sr);
	let low = 0;
	tr.add(t0, 0.03, pan, pan, 0.2, (t) => {
		low += k * (tr.rng.range(-1, 1) - low);
		return amp * 0.5 * exp(-t * 160) * low;
	});
}

/** A round sine bass, swelling in a little rather than plucked. */
export function bass(tr: Track, t0: number, freq: number, amp: number, dur: number) {
	tr.add(t0, dur, 0, 0, 0.15, (t) => {
		const env = min(t / 0.02, 1) * exp((-t * 2.2) / dur) * (1 - (t / dur) ** 4);
		return amp * env * (sin(TAU * freq * t) + 0.15 * sin(TAU * 2 * freq * t));
	});
}

/**
 * Freeverb: eight damped combs in parallel and four allpasses in series per side, the right side slightly detuned
 * from the left for width.
 */
export function reverb(input: Float32Array, sr: number): Stereo {
	const COMBS = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
	const ALLPASSES = [556, 441, 341, 225];
	const ROOM = 0.86;
	const DAMP = 0.3;
	const len = (n: number) => Math.max(1, Math.floor((n * sr) / 44100));
	const out: Stereo = { l: new Float32Array(input.length), r: new Float32Array(input.length) };
	for (const [ch, spread] of [
		[out.l, 0],
		[out.r, 23],
	] as const) {
		const combs = COMBS.map((n) => ({ buf: new Float32Array(len(n + spread)), pos: 0, store: 0 }));
		const passes = ALLPASSES.map((n) => ({ buf: new Float32Array(len(n + spread)), pos: 0 }));
		for (let i = 0; i < input.length; i++) {
			const x = input[i]! * 0.015;
			let y = 0;
			for (const c of combs) {
				const o = c.buf[c.pos]!;
				c.store = o * (1 - DAMP) + c.store * DAMP;
				c.buf[c.pos] = x + c.store * ROOM;
				c.pos = c.pos + 1 === c.buf.length ? 0 : c.pos + 1;
				y += o;
			}
			for (const p of passes) {
				const b = p.buf[p.pos]!;
				p.buf[p.pos] = y + b * 0.5;
				y = b - y;
				p.pos = p.pos + 1 === p.buf.length ? 0 : p.pos + 1;
			}
			ch[i] = y * 3;
		}
	}
	return out;
}

/**
 * The Start button's sound, anim's "an answer confirmed": a thock for the press and two mallet notes a fifth apart
 * (A5, E6: the key the letters land in), in the soundtrack's room. The press also sets the page alight
 * (client/lib/burn.ts), so under the notes is the flare anim's burn-out opens with: a breath of noise that brightens
 * and closes as the fire crosses the page. Short enough to render where it is played.
 */
export function select(sr: number, seed: number): Stereo {
	const tr = new Track(sr, 1.6, new Rng(seed, 'select'));
	thock(tr, 0, 0.8, 150, 0);
	mallet(tr, 0, 880, 0.07, 1, -0.15);
	mallet(tr, 0.07, 1318.51, 0.05, 0.9, 0.15);
	whoosh(tr, 0, 0.7, 0.05, -0.2, 0.2, (p) => 500 + 3500 * sin(PI * p));
	const wet = reverb(tr.send, sr);
	for (let i = 0; i < tr.l.length; i++) {
		tr.l[i] += 0.5 * wet.l[i]!;
		tr.r[i] += 0.5 * wet.r[i]!;
	}
	return { l: tr.l, r: tr.r };
}
