/**
 * The intro's soundtrack: a port of anim's src/audio.rs (indexable-inc/anim), the Blaze logo's part. Everything is
 * generated from the same timeline constants the picture uses (client/lib/intro-timeline.ts), into one buffer, so
 * sound and picture can't drift apart: the player starts it at sample zero when the picture's clock starts.
 *
 * - The letters fall in left to right, each with the air it pushes, a knock as it lands and a mallet note, climbing
 *   A B C# E to the leading tone G# over an E pedal, each a little louder: a V that hangs unresolved until the fire
 *   catches on the I, A major, with a high A ringing over it as the peak.
 * - The catch: air drawn in just before, a whoomph that travels from the first letter to the last over a sub thump,
 *   then the fire itself (client/lib/fire-sound.ts), which grows as the word catches, flares, and tapers to a low
 *   burn so the chord rings over it.
 * - Burning on: a small campfire, a loop that takes over as the logo's fire dies down.
 * - The room: one Freeverb every sound sends to, then a gentle saturation, normalised.
 *
 * Rendered in a worker (client/soundtrack.ts): about seven seconds of fire and sixteen of campfire, sample by sample.
 *
 * Under client/lib/ (not bundled as an entry).
 */

import { Fire } from './fire-sound';
import { DROP_DUR, DROP_START, IGNITE, IGNITE_AT } from './intro-timeline';
import { Rng } from './rng';
import { type Stereo, Track, bass, bell, mallet, reverb, thud, whoosh } from './intro-track';

const TAU = 2 * Math.PI;
const { PI, exp, sin, min, max } = Math;

/** Level of the fire against the rest of the soundtrack. */
const FIRE_GAIN = 0.9;
/** Seconds the soundtrack runs past the catch: the chord's ring and the logo's fire dying down. */
const TAIL = 6;
/** Seconds of campfire before it loops, and the crossfade that hides the seam. */
export const CAMPFIRE = 16;
const CAMPFIRE_FADE = 1.5;

/** What the page asks the worker (client/soundtrack.ts) for, and the two answers: the soundtrack, then the campfire. */
export type Ask = { sr: number; seed: number };
export type Made = { kind: 'track' | 'campfire'; l: Float32Array; r: Float32Array };

const seedOf = (rng: Rng) => Math.floor(rng.next() * 4294967296) >>> 0;

/** Mixes a rendered fire into the track from `t0`, at `gain`. */
function lay(tr: Track, t0: number, gain: number, fire: { l: Float32Array; r: Float32Array; send: Float32Array }) {
	const a = tr.at(t0);
	const n = min(fire.l.length, tr.l.length - a);
	for (let i = 0; i < n; i++) {
		tr.l[a + i] += gain * fire.l[i]!;
		tr.r[a + i] += gain * fire.r[i]!;
		tr.send[a + i] += gain * fire.send[i]!;
	}
}

/**
 * The fire itself, physically modelled: it grows as the word catches, flares and gusts at the catch, then tapers to
 * a low burn so the chord rings over it, and dies down at the end.
 */
function fire(tr: Track, t0: number, spread: number, dur: number) {
	const taper = (t: number) => 0.3 + 0.7 * exp(-max(0, t - spread) / 0.8);
	const intensity = (t: number) => (0.15 + 0.85 * min(1, t / spread)) * taper(t) * min(1, max(0, (dur - t) / 1.5));
	const f = new Fire(tr.sr, seedOf(tr.rng), spread, intensity);
	f.flare(0.6);
	f.gust(0, 0.5, 1.2);
	f.pop(spread * 0.8);
	lay(tr, t0, 0.6 * FIRE_GAIN, f.render(tr.at(dur)));
}

/**
 * Fire catching across the word in `spread` seconds and burning for `dur`: air drawn in just before, a whoomph that
 * travels from the first letter to the last over a sub thump, then the fire itself.
 */
function ignite(tr: Track, t0: number, spread: number, dur: number) {
	const sr = tr.sr;
	const lp = (cutoff: number) => 1 - exp((-TAU * cutoff) / sr);
	// The intake: rising, brightening noise, cut right at the catch.
	let intake = 0;
	tr.add(t0 - 0.22, 0.22, 0, -0.3, 0.15, (t) => {
		const p = t / 0.22;
		intake += lp(400 + 5000 * p * p) * (tr.rng.range(-1, 1) - intake);
		return 0.35 * p ** 3 * intake;
	});
	// Sub thump, its pitch falling.
	let phase = 0;
	tr.add(t0, 1.2, 0, 0, 0.05, (t) => {
		phase += (TAU * (38 + 70 * exp(-t * 14))) / sr;
		return 0.5 * min(t / 0.008, 1) * exp(-t * 4) * sin(phase);
	});
	// The whoomph: low noise that opens up fast, then closes slowly, panning along the word as it catches.
	let low = 0;
	tr.add(t0, spread + 0.9, -0.6, 0.6, 0.4, (t) => {
		const open = min(t / 0.12, 1);
		const cutoff = 120 + 1600 * open * exp(-max(0, t - 0.12) * 2.5) + 250;
		low += lp(cutoff) * (tr.rng.range(-1, 1) - low);
		return 0.8 * min(t / 0.03, 1) * exp(-t * 2) * low;
	});
	fire(tr, t0, spread, dur);
}

/**
 * The peak as the word lights: A major in full from a low A up, blooming in over a tenth of a second and ringing for
 * seconds, with the high A struck on top, the note the letters were climbing to.
 */
function resolve(tr: Track, t0: number) {
	bass(tr, t0, 55, 0.13, 4.5);
	const voices = [
		[110, 0.045, 0],
		[164.81, 0.03, -0.3],
		[220, 0.035, 0.3],
		[277.18, 0.03, -0.45],
		[329.63, 0.028, 0.45],
		[440, 0.025, -0.2],
		[554.37, 0.016, 0.2],
	] as const;
	for (const [f, a, pan] of voices) {
		const detune = tr.rng.range(-0.5, 0.5);
		const bloom = tr.rng.range(0.08, 0.16);
		tr.add(t0, 4.5, pan, pan, 0.45, (t) => {
			const env = min(t / bloom, 1) ** 2 * exp(-t * 0.75);
			return a * env * (sin(TAU * f * t) + 0.5 * sin(TAU * (f + detune) * t) + 0.12 * sin(TAU * 2 * f * t));
		});
	}
	mallet(tr, t0, 880, 0.09, 3.5, 0);
	bell(tr, t0 + 0.04, 1760, 0.03, 2.5, 0.1);
}

/** The dry track plus its reverb, gently saturated. */
function saturated(tr: Track): Stereo {
	const wet = reverb(tr.send, tr.sr);
	const sat = (dry: Float32Array, w: Float32Array) => {
		for (let i = 0; i < dry.length; i++) dry[i] = Math.tanh((dry[i]! + w[i]!) * 1.4) / 1.4;
		return dry;
	};
	return { l: sat(tr.l, wet.l), r: sat(tr.r, wet.r) };
}

/** The soundtrack at `sr` Hz, its peak at 0.85, and the gain that normalised it (the campfire is scaled by the same). */
export function soundtrack(sr: number, seed: number): Stereo & { gain: number } {
	const tr = new Track(sr, IGNITE_AT + TAIL, new Rng(seed, 'soundtrack'));
	DROP_START.forEach((start, i) => {
		const pan = -0.5 + 0.25 * i;
		const land = start + DROP_DUR + tr.rng.range(-0.004, 0.004);
		const last = i === DROP_START.length - 1;
		whoosh(tr, start, DROP_DUR, 0.022, pan, pan, (p) => 300 + 1800 * p);
		thud(tr, land, last ? 0.5 : 0.25);
		const amp = (0.05 + 0.008 * i) * tr.rng.range(0.92, 1.08);
		mallet(tr, land, [440, 493.88, 554.37, 659.25, 830.61][i]!, amp, last ? 1.2 : 1.8, pan);
	});
	// The E pedal under the climb, from the first landing until the fire has caught.
	const first = DROP_START[0] + DROP_DUR;
	bass(tr, first, 82.41, 0.07, IGNITE_AT - first + 0.08);
	ignite(tr, IGNITE_AT, IGNITE, 5.5);
	resolve(tr, IGNITE_AT);
	const out = saturated(tr);
	let peak = 1e-6;
	for (const ch of [out.l, out.r]) for (let i = 0; i < ch.length; i++) peak = max(peak, Math.abs(ch[i]!));
	const gain = 0.85 / peak;
	for (const ch of [out.l, out.r]) for (let i = 0; i < ch.length; i++) ch[i] *= gain;
	return { ...out, gain };
}

/**
 * A small campfire left burning under the page: a low murmur with crackles and the odd pop, `CAMPFIRE` seconds that
 * loop seamlessly (the tail crossfades, equal power, into the head), at the soundtrack's scale (`gain`, from
 * `soundtrack`) so it sits under it.
 */
export function campfire(sr: number, seed: number, gain: number): Stereo {
	const n = Math.floor(CAMPFIRE * sr);
	const x = Math.floor(CAMPFIRE_FADE * sr);
	// A second of lead so the fire and reverb have settled at the loop start.
	const lead = sr;
	const total = lead + n + x;
	const tr = new Track(sr, (total + 0.5) / sr, new Rng(seed, 'campfire'));
	const f = new Fire(sr, seedOf(tr.rng), 0.01, () => 0.4);
	for (let t = 1; t < total / sr - 0.5; t += tr.rng.range(1.2, 3.5)) f.pop(t);
	lay(tr, 0, 0.35, f.render(tr.l.length));
	const out = saturated(tr);
	const loop = (ch: Float32Array) => {
		const looped = ch.slice(lead, lead + n);
		for (let i = 0; i < x; i++) {
			const p = i / x;
			looped[i] = sin((PI / 2) * p) * looped[i]! + Math.cos((PI / 2) * p) * ch[lead + n + i]!;
		}
		for (let i = 0; i < n; i++) looped[i] *= gain;
		return looped;
	};
	return { l: loop(out.l), r: loop(out.r) };
}
