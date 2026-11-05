import { dirname, join } from 'node:path';

/*
 * Where the pinned Svelte (vendor/svelte-*.tgz, den's docs/ui-runtime.md says why this one) keeps the files this runtime loads by
 * path. Its `exports` map hands a server build, which has no reactivity, to anything that does not set the `custom-renderer`
 * condition, so the client build is named here instead of resolved by condition (svelte.ts has the reason). Resolved on first
 * use, so a kernel whose node_modules lack svelte still runs every cell that draws nothing.
 */

let root: string | undefined;

/** A file of the installed svelte package's `src`. */
export function sveltePath(file: string): string {
  root ??= dirname(Bun.resolveSync('svelte/package.json', import.meta.dir));
  return join(root, 'src', file);
}

/** The client build of each module a cell's script may import, as the `custom-renderer` condition of svelte's `exports` names it. */
const CLIENT: Record<string, string> = {
  svelte: 'index-client.js',
  'svelte/reactivity': 'reactivity/index-client.js',
  'svelte/store': 'store/index-client.js',
  'svelte/renderer': 'renderer/index.js',
  'svelte/easing': 'easing/index.js',
};

/** The file a svelte specifier of a cell's own import means, or undefined when it is not one this runtime serves. */
export function svelteSpecifier(specifier: string): string | undefined {
  const file = CLIENT[specifier];
  return file ? sveltePath(file) : undefined;
}
