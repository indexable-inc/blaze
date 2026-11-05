/**
 * Serves the code graph page alone on localhost, rebuilt on every request, without the rest of the site (which needs
 * the renderer's wasm modules) or the worker. It takes one optional JSON value:
 *
 *   bun graph/serve.ts                                   # http://localhost:8790/graph
 *   bun graph/serve.ts '{"port": 8790, "graph": "/path/to/index.json"}'
 *
 * `graph` names a graph file (`blaze index` output) to serve at /graph/index.json and open in place of the sample.
 * Sends the CSP the worker sends the page (worker/http.ts), so what works here works deployed.
 */
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { csp } from '../worker/csp';
import { buildGraphPage } from './build';

type Input = { readonly port: number; readonly graph?: string };

function parseInput(arg: string | undefined): Input {
	const raw: unknown = arg === undefined ? {} : JSON.parse(arg);
	if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) throw new Error('serve.ts: the input must be a JSON object');
	const o = raw as Record<string, unknown>;
	for (const k of Object.keys(o)) if (k !== 'port' && k !== 'graph') throw new Error(`serve.ts: unknown field "${k}" (known: port, graph)`);
	const port = o.port ?? 8790;
	if (typeof port !== 'number' || !Number.isInteger(port)) throw new Error('serve.ts: port must be an integer');
	if (o.graph !== undefined && typeof o.graph !== 'string') throw new Error('serve.ts: graph must be a path');
	const graph = o.graph === undefined ? undefined : path.resolve(o.graph);
	if (graph && !existsSync(graph)) throw new Error(`serve.ts: graph file ${graph} does not exist`);
	return Object.freeze({ port, graph });
}

const input = parseInput(process.argv[2]);
const out = mkdtempSync(path.join(tmpdir(), 'blaze-graph-'));
const headers = { 'content-security-policy': csp({ wasm: true }), 'x-content-type-options': 'nosniff' };

const server = Bun.serve({
	port: input.port,
	async fetch(req) {
		const p = new URL(req.url).pathname;
		if (p === '/' || p === '/graph/') return Response.redirect(input.graph ? '/graph?src=graph/index.json' : '/graph', 302);
		if (p === '/graph') {
			await buildGraphPage(out);
			return new Response(Bun.file(path.join(out, 'graph.html')), { headers: { ...headers, 'content-type': 'text/html; charset=utf-8' } });
		}
		if (p === '/graph/index.json' && input.graph) return new Response(Bun.file(input.graph), { headers });
		// The page's own files, then the site's static/ (the favicon), as dist/ lays them out.
		for (const dir of [out, path.join(import.meta.dirname, '..', 'static')]) {
			const file = path.join(dir, path.normalize(p));
			if (file.startsWith(dir + path.sep) && existsSync(file)) return new Response(Bun.file(file), { headers });
		}
		return new Response('Not found.', { status: 404 });
	},
});
console.log(`code graph: http://localhost:${server.port}/graph${input.graph ? '?src=graph/index.json' : ''}`);
