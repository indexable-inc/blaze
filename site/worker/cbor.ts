/**
 * The CBOR (RFC 8949) the renderer takes its inputs in: null, booleans, numbers, strings, arrays and string-keyed
 * maps, definite lengths only. Integers go out as integers (the renderer's ids are i64), other numbers as float64.
 */
export type Cbor = null | boolean | number | string | Cbor[] | { [key: string]: Cbor };

const utf8 = new TextEncoder();

export function encode(value: Cbor): Uint8Array {
	const parts: Uint8Array[] = [];
	let size = 0;
	const push = (b: Uint8Array) => {
		parts.push(b);
		size += b.length;
	};
	/** A major type and its argument (a length or an unsigned integer up to 2^53). */
	const head = (major: number, n: number) => {
		const m = major << 5;
		if (n < 24) return push(Uint8Array.of(m | n));
		if (n < 0x100) return push(Uint8Array.of(m | 24, n));
		if (n < 0x10000) return push(Uint8Array.of(m | 25, n >> 8, n & 0xff));
		const b = new Uint8Array(n < 0x100000000 ? 5 : 9);
		const v = new DataView(b.buffer);
		if (b.length === 5) {
			b[0] = m | 26;
			v.setUint32(1, n);
		} else {
			b[0] = m | 27;
			v.setBigUint64(1, BigInt(n));
		}
		push(b);
	};
	const item = (v: Cbor) => {
		if (v === null) return push(Uint8Array.of(0xf6));
		if (v === false) return push(Uint8Array.of(0xf4));
		if (v === true) return push(Uint8Array.of(0xf5));
		if (typeof v === 'number') {
			if (Number.isSafeInteger(v)) return v >= 0 ? head(0, v) : head(1, -1 - v);
			const b = new Uint8Array(9);
			b[0] = 0xfb;
			new DataView(b.buffer).setFloat64(1, v);
			return push(b);
		}
		if (typeof v === 'string') {
			const s = utf8.encode(v);
			head(3, s.length);
			return push(s);
		}
		if (Array.isArray(v)) {
			head(4, v.length);
			return v.forEach(item);
		}
		const entries = Object.entries(v);
		head(5, entries.length);
		for (const [k, x] of entries) {
			item(k);
			item(x);
		}
	};
	item(value);
	const out = new Uint8Array(size);
	let at = 0;
	for (const p of parts) {
		out.set(p, at);
		at += p.length;
	}
	return out;
}
