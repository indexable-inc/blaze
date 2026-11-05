// The file space for what a person asked a cell to make (an exported image, a report, a drawing): files on disk under
// `$HARNESS_HOME/artifacts/<session>/`, not in the project folder, so an output never lands as an edit to the repo. The app
// draws only files on disk (ui/media.ts), so a written file's path is what `<Image src>` and `<FileRef path>` take:
// `const f = await vfs.write('bed.png', bytes); render(svelte`<Image src={f} />`, { f })`.
//
// Ported from den's repl/src/vfs.ts, whose space is a CRDT document den's hubs replicate; harness has one machine and a
// state directory, so the space is a directory in it and every call is a plain file operation, recorded as a step.
import { mkdir, readdir, rename, rm, stat, writeFile, readFile } from "node:fs/promises";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { bytes as size, effect } from "./effects.ts";

/** The most one file may be: it is something to look at, not a build artifact. */
export const VFS_MAX_BYTES = 64 << 20;

const HOME = () => process.env.HARNESS_HOME ?? `${process.env.HOME}/.harness`;
/** This chat's artifact directory: one per harness session, else one per REPL process. */
export const artifactDir = (): string =>
  join(HOME(), "artifacts", process.env.HARNESS_SESSION ? `s${process.env.HARNESS_SESSION}` : `pid-${process.pid}`);

/** The absolute path `name` means: relative names are in this chat's artifact directory; absolute ones must be in the space. */
export function vfsPath(name = "."): string {
  if (typeof name !== "string" || name === "") throw new TypeError(`a file space path is a non-empty string, got ${JSON.stringify(name)}`);
  const root = join(HOME(), "artifacts");
  const p = isAbsolute(name) ? resolve(name) : resolve(artifactDir(), name);
  const rel = relative(root, p);
  if (rel.startsWith("..") || isAbsolute(rel)) throw new TypeError(`${name} is outside the file space (${root}); give a name relative to it`);
  return p;
}

/** What `vfs.write` stores: text, bytes, a Blob (Bun.file too) or an `image()` result, whose file is read. */
export type VfsData = string | Uint8Array | ArrayBuffer | ArrayBufferView | Blob | { path: string };
export type VfsEntry = { name: string; path: string; kind: "file" | "dir"; size: number };

async function bytesOf(data: VfsData): Promise<string | Uint8Array> {
  if (typeof data === "string" || data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  if (data instanceof Blob) return new Uint8Array(await data.arrayBuffer());
  if (data !== null && typeof data === "object" && typeof (data as { path?: unknown }).path === "string") return new Uint8Array(await readFile((data as { path: string }).path));
  throw new TypeError(`vfs.write takes text, bytes, a Blob, Bun.file(path) or an image() result, got ${data === null ? "null" : typeof data}`);
}

const shortName = (p: string) => relative(artifactDir(), p) || ".";

export const vfs = {
  /** Write `data` at `name`, making folders; returns its absolute path (what Image `src` and FileRef `path` take). */
  async write(name: string, data: VfsData): Promise<string> {
    const at = vfsPath(name);
    const body = await bytesOf(data);
    const n = typeof body === "string" ? Buffer.byteLength(body) : body.byteLength;
    if (n > VFS_MAX_BYTES) throw new RangeError(`${size(n)} is more than the file space keeps in one file (${size(VFS_MAX_BYTES)}); keep it on disk`);
    const s = shortName(at);
    await effect("write", `Saving ${s}`, `Saved ${s}`, at, async () => {
      await mkdir(dirname(at), { recursive: true });
      await writeFile(at, body);
    }, () => ({ detail: size(n) }));
    return at;
  },
  /** A file as text. */
  async text(name: string): Promise<string> {
    const at = vfsPath(name);
    return effect("read", `Reading ${shortName(at)}`, `Read ${shortName(at)}`, at, () => readFile(at, "utf8"));
  },
  /** A file as bytes. */
  async bytes(name: string): Promise<Uint8Array> {
    const at = vfsPath(name);
    return effect("read", `Reading ${shortName(at)}`, `Read ${shortName(at)}`, at, async () => new Uint8Array(await readFile(at)));
  },
  /** A text file as a string, a binary one (any NUL in its first 8 KB) as bytes. */
  async read(name: string): Promise<string | Uint8Array> {
    const b = await vfs.bytes(name);
    return b.subarray(0, 8192).includes(0) ? b : new TextDecoder().decode(b);
  },
  /** A folder's entries (default: this chat's artifacts); an absent folder is empty. */
  async list(name = "."): Promise<VfsEntry[]> {
    const at = vfsPath(name);
    return effect("list", `Listing ${shortName(at)}`, `Listed ${shortName(at)}`, at, async () => {
      const ents = await readdir(at, { withFileTypes: true }).catch((e: NodeJS.ErrnoException) => (e.code === "ENOENT" ? [] : Promise.reject(e)));
      return Promise.all(ents.map(async (e) => {
        const p = join(at, e.name);
        return { name: e.name, path: p, kind: e.isDirectory() ? "dir" as const : "file" as const, size: e.isDirectory() ? 0 : (await stat(p)).size };
      }));
    }, (r) => ({ detail: `${r.length} entries` }));
  },
  /** Move or rename; fails when `to` exists. */
  async move(from: string, to: string): Promise<void> {
    const a = vfsPath(from), b = vfsPath(to);
    if (await stat(b).then(() => true, () => false)) throw new Error(`vfs.move: ${to} exists`);
    await effect("write", `Moving ${shortName(a)}`, `Moved ${shortName(a)} to ${shortName(b)}`, b, async () => {
      await mkdir(dirname(b), { recursive: true });
      await rename(a, b);
    });
  },
  /** Delete a file, or a folder with everything in it. */
  async delete(name: string): Promise<void> {
    const at = vfsPath(name);
    await effect("delete", `Deleting ${shortName(at)}`, `Deleted ${shortName(at)}`, at, () => rm(at, { recursive: true }));
  },
  /** The absolute path `name` means (default: this chat's artifact folder). */
  path: (name = ".") => vfsPath(name),
};
export type Vfs = typeof vfs;
