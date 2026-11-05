// `image(input, options)`: an image to show or keep, without a shell command. Ported from den's repl/src/image.ts. An SVG
// (a file, its markup or its bytes) stays an SVG: a file is its own path, markup or bytes go in the file space (vfs.ts).
// Only a raster or PDF the cell asks for (`format`, or an `out` that is not `.svg`) is converted: an SVG by `rsvg-convert`
// (librsvg), a raster file by `sips` (macOS). The file the cell asked for is recorded as a write step; the converter is not
// an exec step of its own.
import { stat, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { extname, resolve } from "node:path";
import { bytes as size, effect, quiet } from "./effects.ts";
import { vfs } from "./vfs.ts";

export interface ImageOptions {
  /** Where to write. Default: the input path with the new format's extension; required to convert markup or bytes. */
  out?: string;
  /** Converts: `"png"` (or the extension of `out`), `"jpeg"` (raster inputs), `"pdf"` (SVG inputs). Without it an SVG stays one. */
  format?: "png" | "jpeg" | "pdf";
  /** Pixels of a conversion; one of the two keeps the aspect ratio. An SVG kept as one is sized where it is shown (`<Image width>`). */
  width?: number;
  height?: number;
  signal?: AbortSignal;
}
export interface ImageResult {
  /** Absolute path of the file (an SVG kept from markup is in the file space, vfs.ts). */
  path: string;
  /** Its size in bytes. */
  bytes?: number;
  format: "svg" | "png" | "jpeg" | "pdf";
}
export interface ImagePlan { program: "rsvg-convert" | "sips"; args: string[]; out: string; format: Exclude<ImageResult["format"], "svg">; stdin?: string | Uint8Array }

const EXTENSION = { png: "png", jpeg: "jpg", pdf: "pdf" } as const;
const FORMAT: Record<string, ImagePlan["format"]> = { png: "png", jpg: "jpeg", jpeg: "jpeg", pdf: "pdf" };
const isMarkup = (text: string) => text.replace(/^\uFEFF/, "").trimStart().startsWith("<");
const pixels = (name: string, value: number | undefined): string[] => {
  if (value === undefined) return [];
  if (!Number.isInteger(value) || value < 1) throw new RangeError(`${name} takes a whole number of pixels, got ${String(value)}`);
  return [String(value)];
};

/** The program, arguments and output file that convert `input`; nothing is run. */
export function planImage(input: string | Uint8Array, options: ImageOptions = {}): ImagePlan {
  if (typeof input !== "string" && !(input instanceof Uint8Array)) throw new TypeError("image(input, options) takes a path, SVG markup or SVG bytes");
  const text = typeof input === "string" ? input : Buffer.from(input.subarray(0, 512)).toString("utf8");
  const inline = isMarkup(text);
  const source = typeof input === "string" && !inline ? resolve(input) : undefined;
  if (!source && !inline) throw new TypeError("image() reads bytes only for SVG; write other images to a file and pass its path");
  const svg = inline || /\.svg$/i.test(source ?? "");
  const format = options.format ?? (options.out ? FORMAT[extname(options.out).slice(1).toLowerCase()] : undefined) ?? "png";
  if (!(format in EXTENSION)) throw new RangeError(`image format must be png, jpeg or pdf, got ${String(format)}`);
  if (!options.out && !source) throw new TypeError("image(markup) needs { out: path } to say where the file goes");
  const out = resolve(options.out ?? source!.replace(/\.[^./]*$/, "") + `.${EXTENSION[format]}`);
  const width = pixels("width", options.width);
  const height = pixels("height", options.height);
  if (svg) {
    if (format === "jpeg") throw new RangeError("an SVG is rasterised to png, or written as pdf");
    if (out === source) throw new RangeError("image() would overwrite its input: pass { out }");
    return {
      program: "rsvg-convert", format, out,
      args: ["-f", format, ...(width.length ? ["-w", ...width] : []), ...(height.length ? ["-h", ...height] : []), "-o", out, ...(source ? [source] : [])],
      ...(source ? {} : { stdin: input }),
    };
  }
  if (format === "pdf") throw new RangeError("only an SVG converts to pdf; a raster image converts to png or jpeg");
  const resize = width.length && height.length ? ["--resampleHeightWidth", ...height, ...width] : width.length ? ["--resampleWidth", ...width] : height.length ? ["--resampleHeight", ...height] : [];
  return { program: "sips", format, out, args: ["-s", "format", format, ...resize, source!, "--out", out] };
}

/** Whether `options` ask for a raster or PDF rather than the SVG itself. */
const converts = (options: ImageOptions = {}) => options.format !== undefined || (options.out !== undefined && extname(options.out).toLowerCase() !== ".svg");

async function keepSvg(input: string | Uint8Array, options: ImageOptions): Promise<ImageResult> {
  const markup = typeof input === "string" ? isMarkup(input) : true;
  const out = options.out === undefined ? undefined : resolve(options.out);
  if (!markup && (out === undefined || out === resolve(input as string))) {
    const file = resolve(input as string);
    return { path: file, bytes: (await stat(file)).size, format: "svg" };
  }
  const body = typeof input === "string" && markup ? new TextEncoder().encode(input) : typeof input === "string" ? new Uint8Array(await Bun.file(input).arrayBuffer()) : input;
  if (out === undefined) {
    const name = `media/${createHash("sha256").update(body).digest("hex").slice(0, 16)}.svg`;
    return { path: await vfs.write(name, body), bytes: body.byteLength, format: "svg" };
  }
  await effect("write", `Writing ${out}`, `Wrote ${out}`, out, () => writeFile(out, body), () => ({ detail: size(body.byteLength) }));
  return { path: out, bytes: body.byteLength, format: "svg" };
}

/** Convert or keep an image; see the file's head. */
export async function image(input: string | Uint8Array, options?: ImageOptions): Promise<ImageResult> {
  if (options !== undefined && (typeof options !== "object" || options === null)) throw new TypeError("image(input, options) takes an options object");
  const svg = input instanceof Uint8Array ? isMarkup(Buffer.from(input.subarray(0, 512)).toString("utf8")) : typeof input === "string" && (isMarkup(input) || /\.svg$/i.test(input));
  if (svg && !converts(options)) return keepSvg(input, options ?? {});
  const plan = planImage(input, options);
  if (!Bun.which(plan.program)) throw new Error(`${plan.program} is not installed: ${plan.program === "rsvg-convert" ? "install librsvg to rasterise SVG" : "sips ships with macOS; elsewhere convert with a library or sh('magick ...')"}`);
  return effect("write", `Converting to ${plan.out}`, `Wrote ${plan.out}`, plan.out, async () => {
    const sub = quiet(() => Bun.spawn([plan.program, ...plan.args], {
      stdin: plan.stdin === undefined ? "ignore" : new Blob([plan.stdin as BlobPart]), stdout: "ignore", stderr: "pipe", signal: options?.signal,
    }));
    const [code, stderr] = await Promise.all([sub.exited, new Response(sub.stderr).text()]);
    if (code !== 0) throw new Error(`${plan.program} failed (exit ${code}): ${stderr.trim() || "no message"}`);
    const { size: n } = await stat(plan.out);
    return { path: plan.out, bytes: n, format: plan.format } as ImageResult;
  }, (r) => ({ detail: `${r.format} · ${size(r.bytes ?? 0)}` }));
}
