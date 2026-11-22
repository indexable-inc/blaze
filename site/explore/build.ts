/**
 * Builds the live explore page into a site output directory, as graph/build.ts does:
 *
 *   explore.html          the page, served at <base>/explore (worker gives it loopback connect CSP)
 *   explore/explore.js    main.ts bundled
 *   explore/explore.css
 */
import { copyFileSync, existsSync, mkdirSync } from 'node:fs';
import path from 'node:path';

const here = import.meta.dirname;

export async function buildExplorePage(out: string): Promise<void> {
	const dir = path.join(out, 'explore');
	mkdirSync(dir, { recursive: true });
	const bundled = await Bun.build({
		entrypoints: [path.join(here, 'main.ts')],
		outdir: dir,
		naming: 'explore.js',
		minify: true,
		target: 'browser',
		format: 'esm',
	});
	if (!bundled.success) throw new AggregateError(bundled.logs, 'explore page bundle failed (site/explore/main.ts)');
	copyFileSync(path.join(here, 'explore.css'), path.join(dir, 'explore.css'));
	copyFileSync(path.join(here, 'explore.html'), path.join(out, 'explore.html'));
	const wasmSrc = path.join(here, 'wasm/target/wasm32-unknown-unknown/release/blaze_explore_wasm.wasm');
	if (existsSync(wasmSrc)) {
		copyFileSync(wasmSrc, path.join(dir, 'explore_core.wasm'));
	}
}
