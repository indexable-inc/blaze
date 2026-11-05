/**
 * The lessons' sounds (client/learn.ts), synthesised with Web Audio: no files to load.
 *
 * - Instruments, not bare oscillators: a mallet (additive partials at inharmonic ratios, a fast strike with a breath of
 *   noise, exponential decay), an FM bell (modulator at 3.5 times the carrier, its index falling over the note), a warm
 *   pluck (two detuned saws through a low-pass whose cutoff falls fast), and a woody tick (filtered noise and a falling
 *   blip, never twice the same).
 * - One key, D major pentatonic, so every sound belongs with the others. A right answer is a quick rising figure that
 *   starts a step higher on each right answer in a row (back to the bottom on a miss); not quite is a soft falling
 *   minor third on the pluck, low-passed; Continue and a pick are the tick; a finished lesson is a rising arpeggio that
 *   lands on the tonic with a bell above it, in a longer room; a lesson with every question right the first time ends
 *   on a full chord instead.
 * - A room: two ConvolverNodes with impulses made here (decaying stereo noise, darker as it fades: 1.2 s for the
 *   room, 2.6 s for the finish), mixed low. Notes are spread a little left and right. Everything goes through a light
 *   compressor and an output gain set so the loudest sound peaks near -14 dBFS.
 * - One palette, the same for every reader: the mallet for right answers and the finish, a bell on top, the soft
 *   pluck for not quite, the wood tick.
 * - One AudioContext, made on the reader's first tap or key and resumed inside that handler (iOS Safari starts it
 *   only from a gesture). Every sound is played from a click or key handler, so nothing sounds before the reader has
 *   done something.
 * - On by default; the lesson's top bar has the switch (`blaze.sound`). All storage is wrapped, since it can be
 *   missing.
 *
 * Under client/lib/ (not bundled as an entry).
 */

const KEY = 'blaze.sound';

function read(key: string): string | null {
	try {
		return localStorage.getItem(key);
	} catch {
		return null;
	}
}
function write(key: string, v: string) {
	try {
		localStorage.setItem(key, v);
	} catch {
		// Kept for this visit only.
	}
}

let on = read(KEY) !== 'off';
export const enabled = () => on;

export function setEnabled(v: boolean) {
	on = v;
	write(KEY, v ? 'on' : 'off');
	if (v) unlock();
	else if (live?.ctx instanceof AudioContext) void live.ctx.suspend().catch(() => {});
}

// ---------- The key ----------

/** D major pentatonic from D3: degree 0 is D3, 5 is D4, 10 is D5. */
const STEPS = [0, 2, 4, 7, 9];
export function note(degree: number): number {
	const oct = Math.floor(degree / 5);
	const semis = 12 * oct + STEPS[((degree % 5) + 5) % 5]!;
	return 146.832 * 2 ** (semis / 12);
}
const D4 = 5;
const D5 = 10;

// ---------- The output: compressor, rooms, voices ----------

type Out = {
	ctx: BaseAudioContext;
	bus: GainNode;
	room: GainNode;
	hall: GainNode;
	noise: AudioBuffer;
};

/** Stereo noise that decays over `seconds`, darker as it goes (a one-pole low-pass closing), after a short gap. */
function impulse(ctx: BaseAudioContext, seconds: number, gap: number): AudioBuffer {
	const rate = ctx.sampleRate;
	const len = Math.floor(rate * (seconds + gap));
	const buf = ctx.createBuffer(2, len, rate);
	for (let ch = 0; ch < 2; ch++) {
		const d = buf.getChannelData(ch);
		let lp = 0;
		for (let i = Math.floor(rate * gap); i < len; i++) {
			const t = i / rate - gap;
			const k = 0.75 - 0.6 * (t / seconds); // the low-pass closes over the tail
			lp += k * (Math.random() * 2 - 1 - lp);
			d[i] = lp * Math.exp((-6.9 * t) / seconds); // -60 dB at `seconds`
		}
	}
	return buf;
}

/** Peak target: the output gain is set so the loudest sound lands near -14 dBFS (measured with an OfflineAudioContext). */
const OUTPUT = 0.265;

function output(ctx: BaseAudioContext): Out {
	const comp = ctx.createDynamicsCompressor();
	comp.threshold.value = -16;
	comp.knee.value = 10;
	comp.ratio.value = 4;
	comp.attack.value = 0.003;
	comp.release.value = 0.2;
	const gain = ctx.createGain();
	gain.gain.value = OUTPUT;
	comp.connect(gain).connect(ctx.destination);
	const bus = ctx.createGain();
	bus.connect(comp);
	const room = ctx.createGain();
	const hall = ctx.createGain();
	for (const [send, seconds] of [
		[room, 1.2],
		[hall, 2.6],
	] as const) {
		const conv = ctx.createConvolver();
		conv.buffer = impulse(ctx, seconds, 0.012);
		send.connect(conv).connect(bus);
	}
	const noise = ctx.createBuffer(1, ctx.sampleRate, ctx.sampleRate);
	const nd = noise.getChannelData(0);
	for (let i = 0; i < nd.length; i++) nd[i] = Math.random() * 2 - 1;
	return { ctx, bus, room, hall, noise };
}

/** Where one note goes: panned into the bus, with a little sent to the room or the hall. */
function voice(o: Out, pan: number, room: number, hall = 0): AudioNode {
	const g = o.ctx.createGain();
	let to: AudioNode = o.bus;
	if (o.ctx.createStereoPanner) {
		const p = o.ctx.createStereoPanner();
		p.pan.value = pan;
		p.connect(o.bus);
		to = p;
	}
	g.connect(to);
	if (room) g.connect(gainOf(o.ctx, room)).connect(o.room);
	if (hall) g.connect(gainOf(o.ctx, hall)).connect(o.hall);
	return g;
}

function gainOf(ctx: BaseAudioContext, v: number): GainNode {
	const g = ctx.createGain();
	g.gain.value = v;
	return g;
}

/** An envelope: up to `peak` in `attack`, then an exponential fall to silence at `end`. */
function env(g: AudioParam, t: number, peak: number, attack: number, end: number) {
	g.setValueAtTime(0, t);
	g.linearRampToValueAtTime(peak, t + attack);
	g.exponentialRampToValueAtTime(0.0001, end);
}

// ---------- Instruments ----------

type Play = { pan?: number; room?: number; hall?: number };
type Instrument = (o: Out, t: number, f: number, v: number, p?: Play) => void;

/** A soft mallet (kalimba-like): partials at 1, 3.9 and 9.2 times the note, the high ones gone first, and a strike. */
const mallet: Instrument = (o, t, f, v, p = {}) => {
	const out = voice(o, p.pan ?? 0, p.room ?? 0.22, p.hall);
	const dur = Math.min(1.6, 1.1 * (440 / f) ** 0.35);
	for (const [ratio, amp, life] of [
		[1, 1, 1],
		[3.9, 0.3, 0.32],
		[9.2, 0.09, 0.12],
	] as const) {
		if (f * ratio > 15000) continue;
		const osc = o.ctx.createOscillator();
		osc.frequency.value = f * ratio;
		const g = o.ctx.createGain();
		env(g.gain, t, v * amp, 0.0025, t + dur * life);
		osc.connect(g).connect(out);
		osc.start(t);
		osc.stop(t + dur * life + 0.05);
	}
	strike(o, out, t, f * 2.5, v * 0.18, 0.018);
};

/** A short breath of band-passed noise: the mallet hitting the bar. */
function strike(o: Out, out: AudioNode, t: number, centre: number, v: number, len: number) {
	const n = o.ctx.createBufferSource();
	n.buffer = o.noise;
	const bp = o.ctx.createBiquadFilter();
	bp.type = 'bandpass';
	bp.frequency.value = Math.min(centre, 9000);
	bp.Q.value = 1.4;
	const g = o.ctx.createGain();
	env(g.gain, t, v, 0.001, t + len);
	n.connect(bp).connect(g).connect(out);
	n.start(t, Math.random() * 0.5);
	n.stop(t + len + 0.02);
}

/** An FM bell: a sine carrier, a modulator at 3.5 times it, and an index that falls from bright to pure. */
const bell: Instrument = (o, t, f, v, p = {}) => {
	const out = voice(o, p.pan ?? 0, p.room ?? 0.3, p.hall);
	const dur = Math.min(2.4, 1.8 * (440 / f) ** 0.25);
	const car = o.ctx.createOscillator();
	car.frequency.value = f;
	const mod = o.ctx.createOscillator();
	mod.frequency.value = f * 3.5;
	const depth = o.ctx.createGain();
	const index = 2.4;
	depth.gain.setValueAtTime(index * f * 3.5, t);
	depth.gain.exponentialRampToValueAtTime(0.12 * f * 3.5, t + dur * 0.5);
	mod.connect(depth).connect(car.frequency);
	const g = o.ctx.createGain();
	env(g.gain, t, v, 0.003, t + dur);
	car.connect(g).connect(out);
	// A faint octave, gone early: the shimmer of a struck bell.
	const hi = o.ctx.createOscillator();
	hi.frequency.value = f * 2.005;
	const hg = o.ctx.createGain();
	env(hg.gain, t, v * 0.12, 0.003, t + dur * 0.3);
	hi.connect(hg).connect(out);
	for (const osc of [car, mod, hi]) {
		osc.start(t);
		osc.stop(t + dur + 0.05);
	}
};

/** The pluck for "not quite": two saws a few cents apart through a resonant low-pass that opens only to 3.2 times the
 * note and closes fast, so it is warm and kind rather than a buzzer. */
const softPluck: Instrument = (o, t, f, v, p = {}) => {
	const out = voice(o, p.pan ?? 0, p.room ?? 0.16, p.hall);
	const dur = 0.55;
	const lp = o.ctx.createBiquadFilter();
	lp.type = 'lowpass';
	lp.Q.value = 1.6;
	lp.frequency.setValueAtTime(Math.min(f * 3.2, 8000), t);
	lp.frequency.exponentialRampToValueAtTime(f * 1.1, t + 0.2);
	const g = o.ctx.createGain();
	env(g.gain, t, v, 0.004, t + dur);
	lp.connect(g).connect(out);
	for (const cents of [-6, 6]) {
		const osc = o.ctx.createOscillator();
		osc.type = 'sawtooth';
		osc.frequency.value = f;
		osc.detune.value = cents;
		osc.connect(lp);
		osc.start(t);
		osc.stop(t + dur + 0.05);
	}
};

/** The tick: a woody knock of band-passed noise and a tiny falling blip, pitched a little differently every time. */
const wood = (o: Out, t: number, v: number) => {
	const centre = 1900;
	const blip = 820;
	const vary = 1 + (Math.random() - 0.5) * 0.14;
	const out = voice(o, (Math.random() - 0.5) * 0.2, 0.08);
	strike(o, out, t, centre * vary, v, 0.022);
	const osc = o.ctx.createOscillator();
	osc.frequency.setValueAtTime(blip * vary, t);
	osc.frequency.exponentialRampToValueAtTime(blip * vary * 0.62, t + 0.03);
	const g = o.ctx.createGain();
	env(g.gain, t, v * 0.7, 0.001, t + 0.035);
	osc.connect(g).connect(out);
	osc.start(t);
	osc.stop(t + 0.06);
};

// ---------- The palette ----------

type Palette = {
	right: Instrument;
	wrong: Instrument;
	done: Instrument;
	top: Instrument;
	tick: (o: Out, t: number, v: number) => void;
};

const pal: Palette = { right: mallet, wrong: softPluck, done: mallet, top: bell, tick: wood };

// ---------- The sounds ----------

/** Right answers in a row: each one starts a step higher, up to an octave. */
let streak = 0;

type Sound = (o: Out, t: number, level?: number) => void;

const figure = (o: Out, t: number, from: number) => {
	// Three notes up the scale, the last one quieter and higher: it sparkles rather than shouts.
	pal.right(o, t, note(from), 0.55, { pan: -0.18 });
	pal.right(o, t + 0.075, note(from + 2), 0.5, { pan: 0.05 });
	pal.right(o, t + 0.15, note(from + 4), 0.32, { pan: 0.22 });
};

const sounds: Record<string, Sound> = {
	tick: (o, t) => pal.tick(o, t, 0.1),
	pick: (o, t) => pal.tick(o, t, 0.13),
	// D5 F#5 B5 to start; each right answer in a row starts a step higher, up to D6 F#6 B6.
	right: (o, t, level = 0) => figure(o, t, D5 + Math.min(level, 5)),
	wrong: (o, t) => {
		// A falling minor third, A3 to F#3, soft and low-passed.
		pal.wrong(o, t, note(3), 0.33, { pan: -0.08 });
		pal.wrong(o, t + 0.14, note(2), 0.28, { pan: 0.08 });
	},
	done: (o, t) => {
		// D4 F#4 A4 B4 D5: up the scale, landing on the tonic, a bell an octave above it.
		const run = [D4, D4 + 2, D4 + 3, D4 + 4, D5];
		run.forEach((d, i) => pal.done(o, t + i * 0.085, note(d), i === run.length - 1 ? 0.5 : 0.4, { pan: -0.3 + i * 0.15, hall: 0.18 }));
		const land = t + (run.length - 1) * 0.085;
		pal.top(o, land + 0.02, note(D5 + 5), 0.3, { pan: 0.12, hall: 0.35 });
	},
	milestone: (o, t) => {
		sounds.done!(o, t);
		// Then the whole chord, D A E F# D, rolled upward, with the bell on top and the long room.
		const at = t + 0.62;
		[D4 - 5, D4 - 2, D4 + 1, D4 + 2, D5].forEach((d, i) => pal.done(o, at + i * 0.028, note(d), 0.3, { pan: -0.35 + i * 0.17, hall: 0.3 }));
		pal.top(o, at + 0.16, note(D5 + 2), 0.24, { pan: 0.25, hall: 0.4 });
		pal.top(o, at + 0.2, note(D5 + 5), 0.2, { pan: -0.2, hall: 0.4 });
	},
};

export type SoundName = 'tick' | 'pick' | 'right' | 'wrong' | 'done' | 'milestone';

// ---------- Playing ----------

let live: Out | null = null;

type AudioCtor = typeof AudioContext;

/** Makes or resumes the context. Call it from inside a gesture's handler. */
function unlock(): Out | null {
	if (!on) return null;
	try {
		if (!live) {
			const AC: AudioCtor | undefined = window.AudioContext ?? (window as unknown as { webkitAudioContext?: AudioCtor }).webkitAudioContext;
			if (!AC) return null;
			live = output(new AC());
		}
		const ctx = live.ctx as AudioContext;
		if (ctx.state !== 'running') void ctx.resume().catch(() => {});
		return live;
	} catch {
		return null;
	}
}

function play(name: SoundName, level?: number) {
	const o = unlock();
	if (!o) return;
	try {
		sounds[name]!(o, o.ctx.currentTime + 0.012, level);
	} catch {
		// A browser without some node: silence, never an error in the lesson.
	}
}

export const sound = {
	tick: () => play('tick'),
	pick: () => play('pick'),
	right: () => {
		play('right', streak);
		streak++;
	},
	wrong: () => {
		play('wrong');
		streak = 0;
	},
	done: () => play('done'),
	milestone: () => play('milestone'),
};
