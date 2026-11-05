import { existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

/*
 * The file an `Image`'s `src` or a `FileRef`'s `path` names, as the app is given it: an absolute path. The harness app draws only files
 * on disk, so a path (relative ones from the working directory, `~/` the home directory), a `file:` URL, `Bun.file(path)` or a `{path}`
 * (an `image()` result, a listing's entry) is one, and anything else (an http or data: URL, SVG markup, bytes) is an error that says so.
 * Whether the file is there and decodes is the app's to say when it draws it. Cut from den's repl/src/ui/media.ts, which also takes
 * URLs and bytes.
 */

const describe = (value: unknown): string => {
  if (value === null || typeof value !== 'object') return typeof value === 'string' ? JSON.stringify(value.length > 60 ? `${value.slice(0, 59)}…` : value) : String(value);
  return (value as object).constructor?.name ?? 'an object';
};

/** `path` made absolute: a leading `~/` is the home directory, a relative path the working directory's. */
const absolute = (path: string, cwd: string): string => {
  if (path === '~' || path.startsWith('~/')) return join(homedir(), path.slice(1));
  return isAbsolute(path) ? path : resolve(cwd, path);
};

/** What a file reference takes, as an error says it. */
const TAKES = 'a path (relative to the working directory, or ~/...), a file: URL, Bun.file(path) or {path}';
/** `a` or `an` before a URL's scheme as it is read aloud: an https: URL, an ftp: URL, a data: URL. */
const an = (scheme: string): string => /^(https?|ftp|s|[aeiou])/i.test(scheme) ? 'an' : 'a';
const FETCH = 'the harness app draws only files on disk; fetch it to a file first and give its path';
const WRITE = 'the harness app draws only files on disk; write them to a file first (Bun.write) and give its path';

/** The absolute path `value` names, or what it is instead and what to do. */
function pathOf(value: unknown, cwd: string): { path: string } | { is: string; fix: string } {
  if (typeof value === 'string') {
    if (value.trim() === '') return { is: 'an empty path', fix: `give ${TAKES}` };
    if (/^﻿?\s*</.test(value)) return { is: 'markup', fix: 'the harness app draws only files on disk; write it to a file first (Bun.write) and give its path, or draw with <svg>' };
    if (value.startsWith('file:')) {
      try { return { path: fileURLToPath(value) }; } catch { return { is: 'a file: URL that names no path', fix: `give ${TAKES}` }; }
    }
    const scheme = /^([a-z][a-z0-9+.-]*):/i.exec(value);
    if (scheme && scheme[1]!.length > 1) return { is: `${an(scheme[1]!)} ${scheme[1]!.toLowerCase()}: URL`, fix: FETCH };
    return { path: absolute(value, cwd) };
  }
  if (value instanceof URL) return value.protocol === 'file:' ? { path: fileURLToPath(value) } : { is: `${an(value.protocol.slice(0, -1))} ${value.protocol} URL`, fix: FETCH };
  if (value instanceof Blob) {
    const named = value as Blob & { exists?: unknown; name?: unknown };
    if (typeof named.exists === 'function' && typeof named.name === 'string') return { path: absolute(named.name, cwd) };
    return { is: 'bytes (a Blob)', fix: WRITE };
  }
  if (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) return { is: `bytes (${describe(value)})`, fix: WRITE };
  if (value !== null && typeof value === 'object' && typeof (value as { path?: unknown }).path === 'string') return pathOf((value as { path: string }).path, cwd);
  return { is: describe(value), fix: `give ${TAKES}` };
}

/** Why `value` names no file the app can draw, said as what it should be (`a file on disk: ...`); undefined when it names one. */
export function fileProblem(value: unknown, cwd: string = process.cwd()): string | undefined {
  const found = pathOf(value, cwd);
  return 'path' in found ? undefined : `a file on disk (it is ${found.is}): ${found.fix}`;
}

/** The absolute path `value` names; throws the TypeError saying what it is instead (`what` names the prop, `<Image src>`). */
export function resolveFile(value: unknown, what: string, cwd: string = process.cwd()): string {
  const found = pathOf(value, cwd);
  if ('path' in found) return found.path;
  throw new TypeError(`${what} is ${found.is}, not a file on disk: ${found.fix}`);
}

/** An http(s) URL as written: only a Video's `src` and a Shader's video texture take one (AVPlayer streams it). */
export const isWebUrl = (value: unknown): value is string => typeof value === 'string' && /^https?:\/\/\S+$/i.test(value.trim());

/** Why `value` names no file and no http(s) URL a Video plays, said as what it should be; undefined when it names one. */
export function mediaProblem(value: unknown, cwd: string = process.cwd()): string | undefined {
  if (isWebUrl(value)) return undefined;
  const problem = fileProblem(value, cwd);
  return problem && `${problem.replace(/^a file on disk/, 'a file on disk or an http(s) URL').replace(FETCH, 'only an http(s) URL plays as it is; fetch any other to a file first')}`;
}

/**
 * The absolute path of a file that must be there now (a Video's, a poster, a texture, a Shader's `.slang`), or, when `web` allows
 * it, an http(s) URL as written; throws the TypeError naming `what` and the path it looked for.
 */
export function resolveMedia(value: unknown, what: string, web = false, cwd: string = process.cwd()): string {
  if (web && isWebUrl(value)) return value.trim();
  const path = resolveFile(value, what, cwd);
  if (!existsSync(path)) {
    const from = typeof value === 'string' && !value.startsWith('/') && !value.startsWith('file:') ? ` (${JSON.stringify(value)} from ${value.startsWith('~') ? 'the home directory' : cwd})` : '';
    throw new TypeError(`${what} names ${path}${from}, which is not there`);
  }
  return path;
}
