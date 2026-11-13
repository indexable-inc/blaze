/**
 * A physically based fire, for the wordmark catching light and burning on.
 *
 * Ported from anim's src/fire_sound.rs (indexable-inc/anim), itself ported from procedural-fire's
 * `research/audio-ref/fire-audio-dsp.mjs` (MIT, Leon Lin, github.com/Leonxlnx/procedural-fire), trimmed to the flame
 * itself: no crickets, log shifts, distance or outdoor reverb.
 *
 * - Roar: pink noise through a one-pole knee, so its spectrum falls as f^-3 (Chadwick & James), its level
 *   log-normally modulated by flame puffing at 1.5/sqrt(D) Hz (Cetegen & Ahmed) plus 7 and 14 Hz flutter.
 * - Breath and lapping: the mid-band whoosh of the flame, and Farnell's 30 Hz band driven into tanh for its flapping
 *   harmonics.
 * - Hiss: outgassing, high-passed noise in z^4 surges.
 * - Crackle: power-law energies, each a short fracture sequence, arriving as a Hawkes process so one crackle sets
 *   off more.
 * - Pops: resin pockets bursting, a rupture click, a cavity ring, a crack, a thump for big ones, a micro-crackle
 *   cluster and a steam hiss tail.
 *
 * The level follows an intensity curve; a flare and a gust make the catch. It is rendered sample by sample into
 * plain arrays (no Web Audio here), in the soundtrack's worker (client/soundtrack.ts).
 *
 * Under client/lib/ (not bundled as an entry).
 */

import { Rng } from './rng';

const TAU = 2 * Math.PI;
/** Control-rate interval, samples. */
const CR = 32;
const SQRT3 = 1.7320508;

const db = (x: number) => 10 ** (x / 20);
const clamp = (x: number, a: number, b: number) => Math.min(b, Math.max(a, x));

/** RBJ biquad, transposed direct form II. */
class Biquad {
	private z0 = 0;
	private z1 = 0;
	private constructor(
		private b0: number,
		private b1: number,
		private b2: number,
		private a1: number,
		private a2: number,
	) {}

	private static set(b0: number, b1: number, b2: number, a0: number, a1: number, a2: number): Biquad {
		return new Biquad(b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0);
	}

	static hp(f: number, q: number, fs: number): Biquad {
		const w = (TAU * f) / fs;
		const c = Math.cos(w);
		const al = Math.sin(w) / (2 * q);
		return Biquad.set((1 + c) / 2, -(1 + c), (1 + c) / 2, 1 + al, -2 * c, 1 - al);
	}

	/** Band-pass, 0 dB at the peak. */
	static bp(f: number, q: number, fs: number): Biquad {
		const w = (TAU * f) / fs;
		const c = Math.cos(w);
		const al = Math.sin(w) / (2 * q);
		return Biquad.set(al, 0, -al, 1 + al, -2 * c, 1 - al);
	}

	static peak(f: number, q: number, gainDb: number, fs: number): Biquad {
		const a = 10 ** (gainDb / 40);
		const w = (TAU * f) / fs;
		const c = Math.cos(w);
		const al = Math.sin(w) / (2 * q);
		return Biquad.set(1 + al * a, -2 * c, 1 - al * a, 1 + al / a, -2 * c, 1 - al / a);
	}

	static highShelf(f: number, s: number, gainDb: number, fs: number): Biquad {
		const a = 10 ** (gainDb / 40);
		const w = (TAU * f) / fs;
		const c = Math.cos(w);
		const al = (Math.sin(w) / 2) * Math.sqrt((a + 1 / a) * (1 / s - 1) + 2);
		const sa = 2 * Math.sqrt(a) * al;
		return Biquad.set(
			a * (a + 1 + (a - 1) * c + sa),
			-2 * a * (a - 1 + (a + 1) * c),
			a * (a + 1 + (a - 1) * c - sa),
			a + 1 - (a - 1) * c + sa,
			2 * (a - 1 - (a + 1) * c),
			a + 1 - (a - 1) * c - sa,
		);
	}

	tick(x: number): number {
		const y = this.b0 * x + this.z0;
		this.z0 = this.b1 * x - this.a1 * y + this.z1;
		this.z1 = this.b2 * x - this.a2 * y;
		return y;
	}
}

/** Simper/Cytomic trapezoidal state-variable filter: stable under fast cutoff changes. `k * bp` is the unity-peak band-pass. */
class Svf {
	private ic0 = 0;
	private ic1 = 0;
	k = 0;
	private a0 = 0;
	private a1 = 0;
	private a2 = 0;
	bp = 0;

	static at(f: number, q: number, fs: number): Svf {
		const s = new Svf();
		s.set(f, q, fs);
		return s;
	}

	set(f: number, q: number, fs: number) {
		const g = Math.tan((Math.PI * Math.min(f, fs * 0.45)) / fs);
		this.k = 1 / q;
		this.a0 = 1 / (1 + g * (g + this.k));
		this.a1 = g * this.a0;
		this.a2 = g * this.a1;
	}

	tick(v0: number) {
		const v3 = v0 - this.ic1;
		const v1 = this.a0 * this.ic0 + this.a1 * v3;
		const v2 = this.ic1 + this.a1 * this.ic0 + this.a2 * v3;
		this.ic0 = 2 * v1 - this.ic0;
		this.ic1 = 2 * v2 - this.ic1;
		this.bp = v1;
	}
}

class OnePole {
	a: number;
	private y = 0;

	constructor(fc: number, fs: number) {
		this.a = 1 - Math.exp((-TAU * fc) / fs);
	}

	lp(x: number): number {
		this.y += this.a * (x - this.y);
		return this.y;
	}
}

/** Std of a one-pole low-pass fed unit-variance white noise. */
const onePoleNoiseStd = (a: number) => Math.sqrt(a / (2 - a));

/** Paul Kellet's refined pink filter. */
class Pink {
	private b0 = 0;
	private b1 = 0;
	private b2 = 0;
	private b3 = 0;
	private b4 = 0;
	private b5 = 0;
	private b6 = 0;

	tick(w: number): number {
		this.b0 = 0.99886 * this.b0 + w * 0.0555179;
		this.b1 = 0.99332 * this.b1 + w * 0.0750759;
		this.b2 = 0.969 * this.b2 + w * 0.153852;
		this.b3 = 0.8665 * this.b3 + w * 0.3104856;
		this.b4 = 0.55 * this.b4 + w * 0.5329522;
		this.b5 = -0.7616 * this.b5 - w * 0.016898;
		const p = this.b0 + this.b1 + this.b2 + this.b3 + this.b4 + this.b5 + this.b6 + w * 0.5362;
		this.b6 = w * 0.115926;
		return p * 0.11;
	}
}

/** Narrow-band unit-variance Gaussian at control rate: quasi-periodic puffing. */
class NbMod {
	private svf: Svf;
	private norm: number;

	constructor(f: number, q: number, fcr: number) {
		this.svf = Svf.at(f, q, fcr);
		this.norm = 1 / Math.sqrt(((Math.PI / 2) * (f / q)) / (fcr / 2));
	}

	tick(rng: Rng): number {
		this.svf.tick(rng.gauss());
		return this.svf.k * this.svf.bp * this.norm;
	}
}

/** Slow unit-variance Gaussian at control rate. */
class SlowMod {
	private op: OnePole;
	private norm: number;

	constructor(fc: number, fcr: number) {
		this.op = new OnePole(fc, fcr);
		this.norm = 1 / onePoleNoiseStd(this.op.a);
	}

	tick(rng: Rng): number {
		return this.op.lp(rng.gauss()) * this.norm;
	}
}

/** Where a voice goes: its stereo gains (equal power, pan in [-1, 1]) and reverb send. */
type Out = { l: number; r: number; send: number };
function out(pan: number, send: number): Out {
	const th = ((clamp(pan, -1, 1) + 1) * Math.PI) / 4;
	return { l: Math.cos(th), r: Math.sin(th), send };
}
const NOWHERE: Out = { l: 0, r: 0, send: 0 };

/** A crackle: 1 to 5 noise impulses in a quick fracture sequence, exponentially decaying, part band-passed and part raw. */
class Crackle {
	active = false;
	out = NOWHERE;
	private svf = new Svf();
	private subT = new Uint32Array(6);
	private subA = new Float64Array(6);
	private nSub = 0;
	private si = 0;
	private t = 0;
	private env = 0;
	private amp = 0;
	private d = 0;
	private raw = 0;
	private lvl = 0;

	start(rng: Rng, fs: number, amp: number, tauMs: number, fc: number, q: number, raw: number, to: Out, nSub: number, gapMs: number, lvl: number) {
		this.active = true;
		this.t = 0;
		this.env = 0;
		this.amp = amp;
		this.lvl = lvl;
		this.raw = raw;
		this.out = to;
		this.d = Math.exp(-1000 / (tauMs * fs));
		this.svf = Svf.at(fc, q, fs);
		this.nSub = nSub;
		this.si = 0;
		let tt = 0;
		for (let i = 0; i < nSub; i++) {
			this.subT[i] = Math.round(tt);
			this.subA[i] = amp * (i === 0 ? 1 : rng.range(0.25, 0.9));
			tt += (rng.range(0.3, 1) * gapMs * fs) / 1000 + 1;
		}
	}

	tick(rng: Rng): number {
		if (this.si < this.nSub && this.t === this.subT[this.si]) {
			this.env = Math.max(this.env, this.subA[this.si]!);
			this.si += 1;
		}
		const x = rng.bi() * this.env;
		this.svf.tick(x);
		// 2.2 makes up for the energy the band-pass takes.
		const y = (1 - this.raw) * this.svf.k * this.svf.bp * 2.2 + this.raw * x;
		this.env *= this.d;
		this.t += 1;
		if (this.si >= this.nSub && this.env < this.amp * 3e-4) this.active = false;
		return y * this.lvl;
	}
}

/** One damped sinusoid: its frequency, how long it rings (seconds), how loud. */
type Mode = [f: number, tau: number, amp: number];

/** Up to three damped sinusoids: cavity rings, thumps. */
class Modal {
	active = false;
	out = NOWHERE;
	private n = 0;
	private c0 = new Float64Array(3);
	private c1 = new Float64Array(3);
	private y1 = new Float64Array(3);
	private y2 = new Float64Array(3);
	private t = 0;
	private len = 0;

	start(fs: number, modes: Mode[], to: Out) {
		this.active = true;
		this.n = modes.length;
		this.t = 0;
		this.out = to;
		let maxTau = 0;
		modes.forEach(([f, tau, amp], i) => {
			const th = (TAU * Math.min(f, fs * 0.45)) / fs;
			const r = Math.exp(-1 / (tau * fs));
			this.c0[i] = 2 * r * Math.cos(th);
			this.c1[i] = -r * r;
			// Starting at phase 0 keeps it click-free.
			this.y1[i] = amp * r * Math.sin(th);
			this.y2[i] = 0;
			maxTau = Math.max(maxTau, tau);
		});
		this.len = Math.ceil(maxTau * 9.2 * fs);
	}

	tick(): number {
		let o = 0;
		for (let i = 0; i < this.n; i++) {
			const y1 = this.y1[i]!;
			const y2 = this.y2[i]!;
			o += y2;
			this.y1[i] = this.c0[i]! * y1 + this.c1[i]! * y2;
			this.y2[i] = y1;
		}
		this.t += 1;
		if (this.t > this.len) this.active = false;
		return o;
	}
}

/** A filtered noise burst with an attack and exponential decay, optionally sputtering. */
class Noise {
	active = false;
	out = NOWHERE;
	private svf = new Svf();
	private jit = new OnePole(40, 48000);
	private t = 0;
	private att = 1;
	private len = 0;
	private amp = 0;
	private env = 0;
	private d = 0;
	private jd = 0;
	private jn = 0;

	start(fs: number, amp: number, f: number, q: number, attackMs: number, tauMs: number, jitter: number, to: Out) {
		this.active = true;
		this.t = 0;
		this.amp = amp;
		this.out = to;
		this.env = 0;
		this.svf = Svf.at(f, q, fs);
		this.att = Math.max(1, Math.round((attackMs * fs) / 1000));
		this.d = Math.exp(-1000 / (tauMs * fs));
		this.len = this.att + Math.ceil((tauMs * 9.2 * fs) / 1000);
		this.jit = new OnePole(40, fs);
		this.jd = jitter;
		this.jn = jitter > 0 ? 1 / onePoleNoiseStd(this.jit.a) : 0;
	}

	tick(rng: Rng): number {
		let e: number;
		if (this.t < this.att) e = (this.t / this.att) * this.amp;
		else {
			this.env = this.t === this.att ? this.amp : this.env * this.d;
			e = this.env;
		}
		this.svf.tick(rng.bi());
		let y = this.svf.k * this.svf.bp;
		if (this.jd > 0) y *= Math.max(0, 1 + this.jd * this.jit.lp(rng.gauss()) * this.jn);
		this.t += 1;
		if (this.t > this.len) this.active = false;
		return y * e * 1.7;
	}
}

/** A free voice, else the next one round-robin. */
function take<T extends { active: boolean }>(pool: T[], rr: { at: number }): T {
	const free = pool.find((v) => !v.active);
	if (free) return free;
	const v = pool[rr.at]!;
	rr.at = (rr.at + 1) % pool.length;
	return v;
}

/** Layer levels: the original's mix (dB) times its measured calibration. */
const MIX = {
	ticks: db(-9) * 3.677,
	crackles: db(-4) * 15.53,
	roar: db(-7) * 0.1106,
	breath: db(-8) * 0.4097,
	lapping: db(-15) * 0.1996,
	hiss: db(-16) * 0.3187,
	pops: db(2) * 1.304,
	whoosh: db(-9) * 0.2219,
};

const SEND_TICKS = 0.12;
const SEND_CRACKLES = 0.2;
const SEND_ROAR = 0.08;
const SEND_HISS = 0.1;
const SEND_POPS = 0.4;
const SEND_WHOOSH = 0.15;
/** Pan range of the fire. */
const WIDTH = 0.55;

type FireEvent = { kind: 'micro'; pan: number; size: number } | { kind: 'pop' };

class Tick {
	hp: Biquad;
	bp: Biquad;
	env = 0;
	d = 0;
	route = 0;
	constructor(fs: number) {
		this.hp = Biquad.hp(2500, 0.7, fs);
		this.bp = Biquad.bp(5500, 0.9, fs);
	}
}

class RoarCh {
	pink = new Pink();
	knee: OnePole;
	hp: Biquad;
	body: Biquad;
	breath: Biquad;
	ms = 1e-4;
	constructor(fs: number) {
		this.knee = new OnePole(250, fs);
		this.hp = Biquad.hp(45, 0.7, fs);
		this.body = Biquad.peak(120, 0.8, 4, fs);
		this.breath = Biquad.peak(420, 1, 2, fs);
	}
}

class BreathCh {
	pink = new Pink();
	svf: Svf;
	hp: Biquad;
	constructor(fs: number) {
		this.svf = Svf.at(750, 0.55, fs);
		this.hp = Biquad.hp(220, 0.7, fs);
	}
}

class HissCh {
	hp: Biquad;
	shelf: Biquad;
	constructor(fs: number) {
		this.hp = Biquad.hp(1200, 0.6, fs);
		this.shelf = Biquad.highShelf(9000, 0.8, -4, fs);
	}
}

type Gust = { t0: number; s: number; att: number; rel: number };

/** `n` stereo frames and the mono feed for a reverb. */
export type Rendered = { l: Float32Array; r: Float32Array; send: Float32Array };

export class Fire {
	private n = 0;
	private queue: { n: number; ev: FireEvent }[] = [];
	private rTick: Rng;
	private rCrk: Rng;
	private rPop: Rng;
	private rRoar: Rng;
	private rHiss: Rng;
	private rMod: Rng;
	private rWhoosh: Rng;
	private rVoice: Rng;
	private i = 0;
	private ie = 0;
	private aSmooth: number;
	private flareBy = 0;
	private kFlare: number;
	private gusts: Gust[] = [];
	private gustNow = 0;
	private gustLag = 0;
	private kGustLag: number;
	private hawkes = 0;
	private kHawkes: number;
	private tickBurst = 0;
	private kTickBurst: number;
	private mPuff: NbMod;
	private mFlut: NbMod;
	private mFlap: NbMod;
	private mSlow: SlowMod;
	private mHiss: SlowMod;
	private mDens: SlowMod;
	private roarAmp = 0;
	private breathAmp = 0;
	private lapAmp = 0;
	private hissAmp = 0;
	private whooshAmp = 0;
	private whooshL = 0;
	private whooshR = 0;
	private pTick = 0;
	private pCrk = 0;
	private pPop = 0;
	private lastKnee = -1;
	private ticks: [Tick, Tick];
	private roar: [RoarCh, RoarCh];
	private breath: [BreathCh, BreathCh];
	private lapBp: Biquad;
	private lapHp: [Biquad, Biquad];
	private lapNorm: number;
	private hiss: [HissCh, HissCh];
	private whooshSvf: [Svf, Svf] = [new Svf(), new Svf()];
	private whooshPink: [Pink, Pink] = [new Pink(), new Pink()];
	private grBuf: [Float64Array, Float64Array] = [new Float64Array(256), new Float64Array(256)];
	private grIdx = 0;
	private grLp: [OnePole, OnePole];
	private grD: [number, number];
	private crackles: Crackle[] = Array.from({ length: 48 }, () => new Crackle());
	private modals: Modal[] = Array.from({ length: 40 }, () => new Modal());
	private noises: Noise[] = Array.from({ length: 16 }, () => new Noise());
	private rr: [{ at: number }, { at: number }, { at: number }] = [{ at: 0 }, { at: 0 }, { at: 0 }];
	/** Every pop so far: when, in seconds, and how big, 0.25 to 1.2. */
	pops: [t: number, size: number][] = [];

	/**
	 * `intensity` is the target at a time: 0 out, 1 a healthy fire. `spread` is the seconds the fire takes to spread
	 * from the left of the stereo field to the right; crackle only comes from where it has caught.
	 */
	constructor(
		private fs: number,
		seed: number,
		private spread: number,
		private intensity: (t: number) => number,
	) {
		const fcr = fs / CR;
		// Puffing frequency of a pool fire of this diameter, metres.
		const diameter = 0.7;
		this.rTick = new Rng(seed, 'tick');
		this.rCrk = new Rng(seed, 'crackle');
		this.rPop = new Rng(seed, 'pop');
		this.rRoar = new Rng(seed, 'roar');
		this.rHiss = new Rng(seed, 'hiss');
		this.rWhoosh = new Rng(seed, 'whoosh');
		this.rVoice = new Rng(seed, 'voicenoise');
		this.rMod = new Rng(seed, 'mod');
		this.aSmooth = 1 - Math.exp(-1 / (0.15 * fcr));
		this.kFlare = Math.exp(-1 / (2 * fcr));
		this.kGustLag = 1 - Math.exp(-1 / (0.4 * fcr));
		this.kHawkes = Math.exp(-CR / (0.07 * fs));
		this.kTickBurst = Math.exp(-1 / (0.25 * fcr));
		this.mPuff = new NbMod(1.5 / Math.sqrt(diameter), 2.5, fcr);
		this.mFlut = new NbMod(7, 1.2, fcr);
		this.mFlap = new NbMod(14, 2, fcr);
		this.mSlow = new SlowMod(0.12, fcr);
		this.mHiss = new SlowMod(1, fcr);
		this.mDens = new SlowMod(0.6, fcr);
		this.ticks = [new Tick(fs), new Tick(fs)];
		this.roar = [new RoarCh(fs), new RoarCh(fs)];
		this.breath = [new BreathCh(fs), new BreathCh(fs)];
		this.lapBp = Biquad.bp(30, 5, fs);
		this.lapHp = [Biquad.hp(25, 0.7, fs), Biquad.hp(25, 0.7, fs)];
		this.lapNorm = 1 / Math.sqrt(((Math.PI / 2) * (30 / 5)) / (fs / 2));
		this.hiss = [new HissCh(fs), new HissCh(fs)];
		this.grLp = [new OnePole(2500, fs), new OnePole(2500, fs)];
		this.grD = [Math.round(0.00085 * fs), Math.round(0.00105 * fs)];
	}

	/** The fire flaring up by `amount`, decaying over a couple of seconds. */
	flare(amount: number) {
		this.flareBy += amount;
	}

	/** A gust of air at `t` seconds: strength, then total length. */
	gust(t: number, s: number, dur: number) {
		this.gusts.push({ t0: t, s, att: dur * 0.35, rel: dur * 0.4 });
	}

	/** A resin pocket bursting at `t` seconds. */
	pop(t: number) {
		this.schedule(t, { kind: 'pop' });
	}

	private schedule(t: number, ev: FireEvent) {
		const n = Math.max(this.n, Math.round(t * this.fs));
		let i = 0;
		while (i < this.queue.length && this.queue[i]!.n <= n) i++;
		this.queue.splice(i, 0, { n, ev });
	}

	private t(): number {
		return this.n / this.fs;
	}

	/** Pan for something new in the fire: only where it has caught so far. */
	private litPan(v: number): number {
		const lit = Math.min(1, this.t() / this.spread);
		return -WIDTH + 2 * WIDTH * lit * v;
	}

	private control() {
		const t = this.t();
		this.i += this.aSmooth * (this.intensity(t) - this.i);
		let g = 0;
		for (const gu of this.gusts) {
			const x = t - gu.t0;
			if (x < 0) continue;
			g += x < gu.att ? gu.s * 0.5 * (1 - Math.cos((Math.PI * x) / gu.att)) : gu.s * Math.exp(-(x - gu.att) / gu.rel);
		}
		const gust = Math.min(g, 1.5);
		this.gustNow = gust;
		this.gustLag += this.kGustLag * (gust - this.gustLag);
		this.flareBy *= this.kFlare;
		this.hawkes *= this.kHawkes;
		this.tickBurst *= this.kTickBurst;

		const ie = Math.max(0, this.i * (1 + this.flareBy));
		this.ie = ie;
		const rm = this.rMod;
		const up = this.mPuff.tick(rm);
		const uf = this.mFlut.tick(rm);
		const ufl = this.mFlap.tick(rm);
		const us = this.mSlow.tick(rm);
		const zh = this.mHiss.tick(rm);
		const ud = this.mDens.tick(rm);
		const flap = 0.12 + 0.35 * gust;
		const sd = Math.sqrt(0.32 ** 2 + 0.16 ** 2 + flap * flap + 0.22 ** 2);
		let lnAmp = 0.32 * up + 0.16 * uf + flap * ufl + 0.22 * us;
		// Soft-cap the swells at about 2.2 sigma.
		lnAmp = 2.2 * sd * Math.tanh(lnAmp / (2.2 * sd)) - 0.5 * sd * sd;
		this.roarAmp = ie ** 1.5 * Math.exp(lnAmp) * (1 + 1.4 * gust);
		const bf = 0.2 + 0.3 * gust;
		const lnB = 0.25 * up + 0.4 * uf + bf * ufl - 0.5 * (0.0625 + 0.16 + bf * bf);
		this.breathAmp = ie ** 1.3 * Math.exp(lnB) * (1 + gust);
		this.lapAmp = ie ** 1.2 * (1 + 1.2 * gust) * MIX.lapping;
		// Brighter when stronger or windier.
		const knee = Math.max(60, 150 + 220 * Math.min(ie, 1.5) + 260 * gust + 40 * up);
		if (Math.abs(knee - this.lastKnee) > 2) {
			this.lastKnee = knee;
			const a = new OnePole(knee, this.fs).a;
			this.roar[0].knee.a = a;
			this.roar[1].knee.a = a;
		}
		// Farnell's hiss surges, z^4, over a steady floor.
		this.hissAmp = ie * (0.4 + (0.6 * Math.min(zh * zh * zh * zh, 16)) / 3) * (1 + 0.8 * gust);
		const dens = Math.exp(0.35 * ud - 0.06) * (1 + 0.25 * up);
		this.pTick = Math.min(0.2, (260 * ie ** 1.2 * dens * (1 + 0.8 * this.gustLag) + this.tickBurst) / this.fs);
		this.pCrk = Math.min(0.2, (16 * ie ** 1.3 * (1 + 0.2 * up) * (1 + 1.5 * this.gustLag) + this.hawkes) / this.fs);
		this.pPop = (0.12 * ie ** 1.5 * (1 + this.gustLag)) / this.fs;
		this.whooshAmp = Math.min(gust, 1.5) ** 1.5;
		if (this.whooshAmp > 1e-4) {
			const wf = 280 + 750 * Math.min(gust, 1);
			this.whooshSvf[0].set(wf, 0.9, this.fs);
			this.whooshSvf[1].set(wf * 1.12, 0.9, this.fs);
		}
		const pan = out(clamp(gust * 0.6 - 0.3, -1, 1), 0);
		this.whooshL = pan.l;
		this.whooshR = pan.r;
	}

	private crackle(scale: number, around: number | null) {
		const r = this.rCrk;
		// Energy follows a power law, as acoustic emission from breaking wood does.
		const e = r.pareto(1, 100, 1.6) * scale;
		const sz = Math.min(1, Math.log10(e) / 2);
		const amp = 0.02 * e ** 0.4;
		const tau = 2 * e ** 0.15 * r.logRange(0.5, 2.5);
		const fc = r.logRange(1800, 11000) / (1 + 0.6 * sz);
		const q = r.range(0.8, 2.5);
		const raw = r.range(0.15, 0.45);
		const pv = r.next();
		const pan = around === null ? this.litPan(pv) : around + r.range(-1, 1) * 0.08;
		let nSub = 1;
		while (nSub < 5 && r.next() < 0.45) nSub += 1;
		take(this.crackles, this.rr[0]).start(this.rVoice, this.fs, amp, tau, fc, q, raw, out(pan, SEND_CRACKLES), nSub, 1.6 + 2 * sz, MIX.crackles);
		// Bigger crackles set off more.
		this.hawkes += Math.min(0.85, 0.45 * (0.6 + 0.8 * sz)) / 0.07;
	}

	private modal(modes: Mode[], pan: number, send: number) {
		take(this.modals, this.rr[1]).start(this.fs, modes, out(pan, send));
	}

	private noise(amp: number, f: number, q: number, attackMs: number, tauMs: number, jitter: number, pan: number, send: number) {
		take(this.noises, this.rr[2]).start(this.fs, amp, f, q, attackMs, tauMs, jitter, out(pan, send));
	}

	private firePop() {
		const r = this.rPop;
		const s = r.pareto(0.25, 1.2, 2.2);
		const pv = r.next();
		this.pops.push([this.t(), s]);
		const pan = this.litPan(pv);
		const l = MIX.pops;
		const send = SEND_POPS;
		const fs = this.fs;
		// Rupture click: a very short pressure pulse.
		take(this.crackles, this.rr[0]).start(this.rVoice, fs, 0.5 * l * s ** 0.8, 0.5, 7000, 0.7, 0.85, out(pan, send), 1, 0, 1);
		// The pocket rings like a Helmholtz cavity: bigger is lower.
		const f1 = r.logRange(1200, 4500) / s ** 0.4;
		const f2 = f1 * r.range(1.4, 2.3);
		const f3 = f1 * r.range(2.6, 3.8);
		const tau = r.range(0.004, 0.012) * Math.sqrt(s);
		const a = 0.11 * l * s;
		this.modal(
			[
				[f1, tau, a],
				[f2, tau * 0.6, a * 0.5],
				[f3, tau * 0.4, a * 0.25],
			],
			pan,
			send,
		);
		// The crack itself.
		const cf = r.range(2500, 5000) / Math.sqrt(s);
		const ctau = r.range(2, 6) * Math.sqrt(s);
		this.noise(0.16 * l * s, cf, 0.7, 0.1, ctau, 0, pan, send);
		// A thump from the gas pulse of big ones.
		if (s > 0.5) this.modal([[r.range(90, 200), 0.02 * s, 0.32 * l * s]], pan * 0.5, send * 0.5);
		if (s > 0.7) this.tickBurst += 700 * s;
		// The fracture runs on as a cluster of small crackles.
		const nMicro = Math.round(3 + 9 * s * r.next());
		let tt = 0;
		const now = this.t();
		for (let k = 0; k < nMicro; k++) {
			tt += r.range(0.002, 0.012);
			this.schedule(now + tt, { kind: 'micro', pan, size: 0.35 * s });
		}
		// Steam jetting out.
		if (r.next() < 0.3 + 0.3 * s) {
			const f = r.range(3500, 7000);
			const jet = r.range(60, 250) * s;
			this.noise(0.03 * l * s, f, 0.9, 8, jet, 0.6, pan, send);
		}
		this.hawkes += 35 * s;
	}

	/** Renders the next `n` frames. */
	render(n: number): Rendered {
		const res: Rendered = { l: new Float32Array(n), r: new Float32Array(n), send: new Float32Array(n) };
		for (let k = 0; k < n; k++) {
			if (this.n % CR === 0) this.control();
			while (this.queue.length && this.queue[0]!.n <= this.n) {
				const ev = this.queue.shift()!.ev;
				if (ev.kind === 'micro') this.crackle(ev.size, ev.pan);
				else this.firePop();
			}
			// Bernoulli per sample: Poisson with a time-varying rate.
			if (this.rCrk.next() < this.pCrk) this.crackle(1, null);
			if (this.rPop.next() < this.pPop) this.firePop();
			this.sample(res, k);
			this.n += 1;
		}
		return res;
	}

	private sample(res: Rendered, k: number) {
		let o0 = 0;
		let o1 = 0;
		let s0 = 0;
		let s1 = 0;
		// Ticks: independent per side, through two fixed colours.
		const rt = this.rTick;
		for (let c = 0; c < 2; c++) {
			const tk = this.ticks[c]!;
			if (rt.next() < this.pTick) {
				const a = db(-17 + 3.5 * rt.gauss());
				if (a > tk.env) {
					tk.env = a;
					tk.d = Math.exp(-1 / (rt.range(0.08, 0.3) * 1e-3 * this.fs));
					tk.route = rt.next();
				}
			}
			let x0 = 0;
			let x1 = 0;
			if (tk.env > 1e-5) {
				const x = tk.env * rt.bi();
				tk.env *= tk.d;
				if (tk.route < 0.55) x0 = x;
				else x1 = x;
			}
			const y = (tk.hp.tick(x0) + tk.bp.tick(x1) * 1.5) * MIX.ticks;
			if (c === 0) {
				o0 += y;
				s0 += y * SEND_TICKS;
			} else {
				o1 += y;
				s1 += y * SEND_TICKS;
			}
		}
		// Roar: correlated stereo, soft-saturated at three times its running RMS.
		const rr = this.rRoar;
		const mid = rr.bi();
		const side = rr.bi() * 0.45;
		for (let c = 0; c < 2; c++) {
			const ch = this.roar[c]!;
			let x = ch.pink.tick(c === 0 ? mid + side : mid - side);
			x = ch.breath.tick(ch.body.tick(ch.hp.tick(ch.knee.lp(x))));
			ch.ms += 2e-5 * (x * x - ch.ms);
			const cl = 3 * Math.sqrt(ch.ms + 1e-12);
			const y = cl * Math.tanh(x / cl) * this.roarAmp * MIX.roar * 6;
			if (c === 0) {
				o0 += y;
				s0 += y * SEND_ROAR;
			} else {
				o1 += y;
				s1 += y * SEND_ROAR;
			}
		}
		// Lapping: a 30 Hz band driven into tanh, for the flapping harmonics.
		const lb = this.lapBp.tick(rr.bi() * SQRT3) * this.lapNorm;
		const ly = this.lapHp[1].tick(this.lapHp[0].tick(Math.tanh(1.6 * lb))) * this.lapAmp;
		o0 += ly;
		o1 += ly;
		// Breath: the flame's mid-band whoosh.
		for (let c = 0; c < 2; c++) {
			const ch = this.breath[c]!;
			ch.svf.tick(ch.pink.tick(rr.bi() * SQRT3));
			const y = ch.hp.tick(ch.svf.k * ch.svf.bp) * this.breathAmp * MIX.breath;
			if (c === 0) {
				o0 += y;
				s0 += y * SEND_ROAR;
			} else {
				o1 += y;
				s1 += y * SEND_ROAR;
			}
		}
		// Hiss: gas escaping.
		for (let c = 0; c < 2; c++) {
			const ch = this.hiss[c]!;
			const y = ch.shelf.tick(ch.hp.tick(this.rHiss.bi())) * this.hissAmp * MIX.hiss * 0.35;
			if (c === 0) {
				o0 += y;
				s0 += y * SEND_HISS;
			} else {
				o1 += y;
				s1 += y * SEND_HISS;
			}
		}
		// Gust band.
		if (this.whooshAmp > 1e-4) {
			const wa = this.whooshAmp * MIX.whoosh;
			const a = this.whooshSvf[0];
			a.tick(this.whooshPink[0].tick(this.rWhoosh.bi() * SQRT3));
			const y0 = a.bp * a.k * wa * 2.5;
			const b = this.whooshSvf[1];
			b.tick(this.whooshPink[1].tick(this.rWhoosh.bi() * SQRT3));
			const y1 = b.bp * b.k * wa * 2.5;
			o0 += y0 * this.whooshL * 1.2 + y1 * 0.3;
			o1 += y1 * this.whooshR * 1.2 + y0 * 0.3;
			s0 += y0 * SEND_WHOOSH;
			s1 += y1 * SEND_WHOOSH;
		}
		for (const v of this.crackles) {
			if (!v.active) continue;
			const y = v.tick(this.rVoice);
			o0 += y * v.out.l;
			o1 += y * v.out.r;
			s0 += y * v.out.l * v.out.send;
			s1 += y * v.out.r * v.out.send;
		}
		for (const v of this.modals) {
			if (!v.active) continue;
			const y = v.tick();
			o0 += y * v.out.l;
			o1 += y * v.out.r;
			s0 += y * v.out.l * v.out.send;
			s1 += y * v.out.r * v.out.send;
		}
		for (const v of this.noises) {
			if (!v.active) continue;
			const y = v.tick(this.rVoice);
			o0 += y * v.out.l;
			o1 += y * v.out.r;
			s0 += y * v.out.l * v.out.send;
			s1 += y * v.out.r * v.out.send;
		}
		// A ground reflection, a millisecond late and dulled.
		const gi = this.grIdx;
		this.grBuf[0][gi & 255] = o0;
		o0 += 0.25 * this.grLp[0].lp(this.grBuf[0][(gi - this.grD[0]) & 255]!);
		this.grBuf[1][gi & 255] = o1;
		o1 += 0.25 * this.grLp[1].lp(this.grBuf[1][(gi - this.grD[1]) & 255]!);
		this.grIdx = gi + 1;
		res.l[k] = o0;
		res.r[k] = o1;
		res.send[k] = 0.5 * (s0 + s1);
	}
}
