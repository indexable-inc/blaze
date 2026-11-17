/**
 * The random numbers the intro's sounds are made of (client/lib/fire-sound.ts, intro-track.ts, intro-soundtrack.ts):
 * one seed, a stream per layer, as anim's fire has it.
 *
 * Under client/lib/ (not bundled as an entry).
 */

const TAU = 2 * Math.PI;

/** mulberry32, seeded per layer so layers never share a stream. */
export class Rng {
	private s: number;
	private spare: number | null = null;

	constructor(seed: number, salt: string) {
		let h = (seed ^ 0x9e3779b9) >>> 0;
		for (let i = 0; i < salt.length; i++) {
			h = Math.imul(h ^ salt.charCodeAt(i), 0x85ebca6b) >>> 0;
			h = (h ^ (h >>> 13)) >>> 0;
		}
		h = Math.imul(h ^ (h >>> 16), 0xc2b2ae35) >>> 0;
		this.s = (h ^ (h >>> 16)) >>> 0;
	}

	/** Uniform in [0, 1). */
	next(): number {
		this.s = (this.s + 0x6d2b79f5) | 0;
		let t = Math.imul(this.s ^ (this.s >>> 15), this.s | 1);
		t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	}

	/** Uniform in [-1, 1). */
	bi(): number {
		return this.next() * 2 - 1;
	}

	range(a: number, b: number): number {
		return a + (b - a) * this.next();
	}

	logRange(a: number, b: number): number {
		return a * (b / a) ** this.next();
	}

	gauss(): number {
		if (this.spare !== null) {
			const g = this.spare;
			this.spare = null;
			return g;
		}
		const u = Math.max(this.next(), 1e-12);
		const v = this.next();
		const r = Math.sqrt(-2 * Math.log(u));
		this.spare = r * Math.sin(TAU * v);
		return r * Math.cos(TAU * v);
	}

	/** Truncated power law, pdf ~ x^-b on [lo, hi]. */
	pareto(lo: number, hi: number, b: number): number {
		const k = b - 1;
		const r = (lo / hi) ** k;
		return lo * (1 - this.next() * (1 - r)) ** (-1 / k);
	}
}
