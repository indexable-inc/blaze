/**
 * Scene 3, item grain: a workspace as a field of thousands of points, one per function, laid out like a galaxy. The
 * scroll moves in on one part of it; one point lights (the edit), then lines run to the few that depend on it and they
 * light too; the rest stays dark. Deterministic: the field is made from a fixed seed.
 */
import { Rng } from './rng';
import { type Inks, type Picture, canvas, clamp, inks, lerp, rgba, span } from './scene';

type Star = { x: number; y: number; r: number; a: number };

const COUNT = 2600;

function field(): Star[] {
	const rng = new Rng(7, 'galaxy');
	const stars: Star[] = [];
	for (let i = 0; i < COUNT; i++) {
		const arm = i % 3;
		const d = Math.pow(rng.next(), 0.62);
		const theta = (arm * 2 * Math.PI) / 3 + d * 5.2 + (rng.next() - 0.5) * (0.5 + d * 0.5);
		const spread = 0.06 + d * 0.05;
		const x = Math.cos(theta) * d + (rng.next() - 0.5) * spread;
		const y = Math.sin(theta) * d + (rng.next() - 0.5) * spread;
		stars.push({ x, y: y * 0.62, r: 0.6 + rng.next() * rng.next() * 1.3, a: 0.25 + rng.next() * 0.55 * (1 - d * 0.5) });
	}
	return stars;
}

export function galaxy(_section: HTMLElement, stage: HTMLElement): Picture {
	const c = canvas(stage);
	const stars = field();
	// The edit, and what depends on it: the point nearest a spot on an arm, and the six nearest it but not too near.
	const target = { x: 0.34, y: -0.08 };
	const byNear = (q: { x: number; y: number }) => stars.map((s, i) => ({ i, d: Math.hypot(s.x - q.x, s.y - q.y) })).sort((a, b) => a.d - b.d);
	const hit = byNear(target)[0]!.i;
	const h = stars[hit]!;
	const deps = byNear(h)
		.filter((n) => n.d > 0.05 && n.d < 0.22)
		.filter((_, j) => j % 5 === 0)
		.slice(0, 6)
		.map((n) => n.i);
	let w = 1;
	let ht = 1;
	let k: Inks = inks();

	return {
		len: 3,
		still: 0.92,
		resize(cw, ch, dpr) {
			w = cw;
			ht = ch;
			c.size(cw, ch, dpr);
			k = inks();
		},
		draw(p) {
			const ctx = c.ctx;
			ctx.clearRect(0, 0, w, ht);
			// Move in on the edit, and turn a little.
			const zoom = lerp(1, 2.1, span(p, 0.02, 0.4));
			const R = Math.min(w * 0.46, ht * 0.8) * zoom;
			const cx0 = w / 2;
			const cy0 = ht / 2;
			const fx = lerp(0, h.x, span(p, 0.02, 0.4));
			const fy = lerp(0, h.y, span(p, 0.02, 0.4));
			const rot = -0.25 + p * 0.25;
			const cos = Math.cos(rot);
			const sin = Math.sin(rot);
			const at = (s: { x: number; y: number }) => {
				const x = s.x - fx;
				const y = s.y - fy;
				return [cx0 + (x * cos - y * sin) * R, cy0 + (x * sin + y * cos) * R] as const;
			};
			const lit = span(p, 0.42, 0.5);
			const reach = span(p, 0.52, 0.78);
			const dim = 1 - 0.45 * span(p, 0.45, 0.6);
			const fade = span(p, 0, 0.12) * 0.35 + 0.65;
			ctx.fillStyle = k.ink;
			for (const s of stars) {
				const [x, y] = at(s);
				if (x < -4 || y < -4 || x > w + 4 || y > ht + 4) continue;
				ctx.globalAlpha = Math.min(1, s.a * 1.35) * dim * fade;
				const r = s.r * Math.sqrt(zoom) * 1.1;
				ctx.fillRect(x - r / 2, y - r / 2, r, r);
			}
			ctx.globalAlpha = 1;
			const [hx, hy] = at(h);
			// The lines to what depends on it, drawn out one after another.
			deps.forEach((i, j) => {
				const t = clamp(reach * deps.length - j);
				if (t <= 0) return;
				const [dx, dy] = at(stars[i]!);
				ctx.strokeStyle = rgba(k.fire2, 0.55);
				ctx.lineWidth = 1;
				ctx.beginPath();
				ctx.moveTo(hx, hy);
				ctx.lineTo(lerp(hx, dx, t), lerp(hy, dy, t));
				ctx.stroke();
				if (t >= 1) glowAt(ctx, dx, dy, 3.2, k.fire2, 0.9);
			});
			if (lit > 0) {
				// The ring the edit sends out, once, then the point itself.
				const ring = span(p, 0.42, 0.62);
				if (ring > 0 && ring < 1) {
					ctx.strokeStyle = rgba(k.fire, 0.6 * (1 - ring));
					ctx.lineWidth = 1.5;
					ctx.beginPath();
					ctx.arc(hx, hy, 6 + ring * Math.min(w, ht) * 0.22, 0, Math.PI * 2);
					ctx.stroke();
				}
				glowAt(ctx, hx, hy, 4.5 * lit, k.fire, lit);
			}
		},
	};
}

function glowAt(ctx: CanvasRenderingContext2D, x: number, y: number, r: number, colour: string, a: number) {
	const g = ctx.createRadialGradient(x, y, 0, x, y, r * 7);
	g.addColorStop(0, rgba(colour, 0.55 * a));
	g.addColorStop(1, rgba(colour, 0));
	ctx.fillStyle = g;
	ctx.fillRect(x - r * 7, y - r * 7, r * 14, r * 14);
	ctx.fillStyle = rgba(colour, a);
	ctx.beginPath();
	ctx.arc(x, y, r, 0, Math.PI * 2);
	ctx.fill();
}
