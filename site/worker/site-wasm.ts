/**
 * The host side of the renderer's wasm modules (site/renderer, README.md "Wasm ABI"), for the worker and for the
 * build (gen/build.ts, under Bun). Neither module has imports. No Bun, Node or Workers APIs here.
 *
 * - `Site` (site.wasm): a snapshot loaded once, then one page's HTML per call.
 * - `snapshot` (site-snapshot.wasm): the content scripts (and those the build generates) and the baked HTML to the
 *   snapshot, at build time.
 */
import { encode, type Cbor } from './cbor';

/** A comment as the renderer takes it: the D1 row of worker/pages.ts's query. */
export type CommentRow = {
	id: number;
	block: string;
	quote: string;
	body: string;
	created_at: string;
	user_id: number;
	login: string;
	name: string;
	avatar_url: string;
};

/** Who reads the page and what their thread shows. */
export type Discussion = {
	viewer: { id: number; login: string; admin: boolean } | null;
	signIn: boolean;
	error?: { block: string; message: string };
};

/** A page as the renderer lists it. */
export type Page = { url: string; file: string; doc: string; discussable: boolean };

/** One line of a page's spoken version (renderer core `spoken.rs`): what the voice says for one block. */
export type SpokenLine = { id: string; block: string; text: string; authored: boolean };

/** Each discussable doc: its title and the blocks comments may attach to, with their text. */
export type Discussable = Record<string, { title: string; blocks: Record<string, string> }>;

/** The module answered with an error (status 1) rather than output. Its instance is still sound. */
export class RenderError extends Error {}

const utf8 = new TextEncoder();
const fromUtf8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

/** The calls both modules share, under their prefix (`site_`, `snap_`). */
type Abi = {
	memory: WebAssembly.Memory;
	alloc(len: number): number;
	free(ptr: number, len: number): void;
	outputPtr(h: number): number;
	outputLen(h: number): number;
	/** Any other export under the prefix. */
	fn(name: string): (...args: number[]) => number;
};

function abi(instance: WebAssembly.Instance, prefix: string): Abi {
	const x = instance.exports;
	const fn = (name: string) => {
		const f = x[prefix + name];
		if (typeof f !== 'function') throw new Error(`wasm module: no export ${prefix}${name}`);
		return f as (...args: number[]) => number;
	};
	const memory = x.memory;
	if (!(memory instanceof WebAssembly.Memory)) throw new Error('wasm module: no exported memory');
	return { memory, alloc: fn('alloc'), free: fn('free'), outputPtr: fn('output_ptr'), outputLen: fn('output_len'), fn };
}

/** Copies `inputs` into the module's memory, runs `f` on them, frees them. A trap skips the frees: the instance is dead. */
function call(a: Abi, inputs: Uint8Array[], f: (at: { ptr: number; len: number }[]) => number): number {
	const at = inputs.map((b) => {
		if (b.length === 0) return { ptr: 0, len: 0 };
		const ptr = a.alloc(b.length);
		new Uint8Array(a.memory.buffer, ptr, b.length).set(b);
		return { ptr, len: b.length };
	});
	const status = f(at);
	for (const x of at) if (x.len) a.free(x.ptr, x.len);
	return status;
}

/** The handle's output, copied out (memory may have grown during the call, so the buffer is taken fresh). */
const output = (a: Abi, h: number): Uint8Array => new Uint8Array(a.memory.buffer, a.outputPtr(h), a.outputLen(h)).slice();

/** site.wasm with a snapshot loaded. */
export class Site {
	private constructor(
		private readonly a: Abi,
		private readonly h: number,
	) {}

	static load(module: WebAssembly.Module, snapshot: Uint8Array): Site {
		const a = abi(new WebAssembly.Instance(module, {}), 'site_');
		const site = new Site(a, a.fn('new')());
		const status = call(a, [snapshot], ([s]) => a.fn('load')(site.h, s.ptr, s.len));
		if (status !== 0) throw new RenderError(`site_load: ${site.text()}`);
		return site;
	}

	private text(): string {
		return fromUtf8.decode(output(this.a, this.h));
	}

	private json<T>(name: string): T {
		if (this.a.fn(name)(this.h) !== 0) throw new RenderError(`site_${name}: ${this.text()}`);
		return JSON.parse(this.text()) as T;
	}

	pages(): Page[] {
		return this.json('pages');
	}

	discussable(): Discussable {
		return this.json('discussable');
	}

	/** The page's spoken lines, in order (the narration's text; audio/generate.py voices them). */
	spoken(url: string): SpokenLine[] {
		const status = call(this.a, [utf8.encode(url)], ([u]) => this.a.fn('spoken')(this.h, u.ptr, u.len));
		if (status !== 0) throw new RenderError(`site_spoken ${url}: ${this.text()}`);
		return JSON.parse(this.text()) as SpokenLine[];
	}

	/** The page at `url` with its comments, for `d`'s viewer: the whole HTML, or an exception. */
	render(url: string, comments: CommentRow[], d: Discussion): string {
		const discussion: Cbor = { viewer: d.viewer, sign_in: d.signIn, error: d.error ?? null };
		const status = call(this.a, [utf8.encode(url), encode(comments), encode(discussion)], ([u, c, s]) =>
			this.a.fn('render')(this.h, u.ptr, u.len, c.ptr, c.len, s.ptr, s.len),
		);
		if (status !== 0) throw new RenderError(`site_render ${url}: ${this.text()}`);
		return this.text();
	}
}

/** The input of site-snapshot.wasm: the scripts in run order, and what the build baked, by entity path. */
export type SnapshotInput = {
	scripts: { name: string; code: string }[];
	/** `audio` goes on a page entity: its narration manifest's URL (the renderer then adds the player). */
	bakes: Record<string, { highlighted?: string; baked?: string; audio?: string }>;
};

/** Runs the content scripts in site-snapshot.wasm (a fresh instance: Flecs's world is freed with it). */
export function snapshot(module: WebAssembly.Module, input: SnapshotInput): Uint8Array {
	const a = abi(new WebAssembly.Instance(module, {}), 'snap_');
	const h = a.fn('new')();
	const status = call(a, [utf8.encode(JSON.stringify(input))], ([i]) => a.fn('build')(h, i.ptr, i.len));
	const out = output(a, h);
	if (status !== 0) throw new RenderError(`snap_build: ${fromUtf8.decode(out)}`);
	return out;
}
