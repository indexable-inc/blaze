/**
 * Builds the code graph page into a site output directory, as gen/build.ts does for dist/:
 *
 *   graph.html            the page, served at <base>/graph (the worker gives it a CSP that lets it compile wasm)
 *   graph/graph.js        main.ts bundled
 *   graph/graph.css
 *   graph/physics.wasm    the layout simulation (@codegraphy-dev/graph-renderer's, loaded by main.ts)
 *   graph/sample.json     the hand-made graph the page shows when no ?src= is given
 */
import { copyFileSync, mkdirSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';

const here = import.meta.dirname;

export async function buildGraphPage(out: string): Promise<void> {
	const dir = path.join(out, 'graph');
	mkdirSync(dir, { recursive: true });
	const bundled = await Bun.build({
		entrypoints: [path.join(here, 'main.ts')],
		outdir: dir,
		naming: 'graph.js',
		minify: true,
		target: 'browser',
		format: 'esm',
	});
	if (!bundled.success) throw new AggregateError(bundled.logs, 'graph page bundle failed (site/graph/main.ts)');
	const renderer = path.dirname(createRequire(import.meta.url).resolve('@codegraphy-dev/graph-renderer/package.json'));
	copyFileSync(path.join(renderer, 'dist/generated/physics.wasm'), path.join(dir, 'physics.wasm'));
	copyFileSync(path.join(here, 'graph.css'), path.join(dir, 'graph.css'));
	copyFileSync(path.join(here, 'sample.json'), path.join(dir, 'sample.json'));
	copyFileSync(path.join(here, 'graph.html'), path.join(out, 'graph.html'));
}
