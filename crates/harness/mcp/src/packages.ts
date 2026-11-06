// npm packages in cells: `import { z } from "zod"`, `await import("lodash@4.17.21")`, `import x from "@scope/pkg@^2/sub"`.
//
// A cell's import goes through the REPL's `__import` (repl.ts), which sends a bare package specifier here. Each
// package version is installed once, by `bun install`, into its own folder of a cache every kernel shares:
//
//   $HARNESS_HOME/packages/<name>@<version>/node_modules/<name>     (a scoped name's / is a + in the folder's name)
//
// so two versions of one package, and their dependencies, never meet. The module is then resolved from that folder
// (Bun.resolveSync, which honours the package's `exports`) and imported by its absolute path.
//
// Pinning. A specifier names a package and, optionally, a version, a range or a dist-tag. The exact version it came to
// is recorded in $HARNESS_HOME/packages/pins.json under the specifier as written without its subpath ("lodash",
// "zod@^3", "react@next"), the first time it is resolved; every later import of that specifier, in any cell or
// session, gets the recorded version without asking the registry, so a cell re-run resolves the same code. An exact
// version needs no pin. To move a pin, import an exact version or a new range, or edit pins.json.
//
// Steps. Resolving and installing is one quiet `net` step of the cell ("Installed zod@3.23.8"); an import already in
// the cache records nothing. The app has no kind of its own for installs (harness-chat's Act::of_effect), so it is
// drawn as a fetch.
//
// No fallback: a registry that refuses the name, a range nothing matches, a failed `bun install` or a module that
// does not resolve in the installed package is an error naming the package and what the registry or bun said. The
// kernel's own node_modules are never consulted for a cell's package.
//
// Types. Cells are not type-checked: stripCell (ui/compile.ts) erases TypeScript with Bun.Transpiler and runs the
// JavaScript, so a package's types never gate a cell and a package without types works the same. The types an
// editor would use are in the installed folder (the package's own .d.ts, named by its `types`/`exports`).
import { builtinModules } from "node:module";
import { mkdir, readFile, rename, rm, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { effect } from "./effects.ts";

export class PackageError extends Error { override name = "PackageError"; }

/** A bare package specifier, split: `@scope/name@range/sub` is name `@scope/name`, range `range`, subpath `/sub`. */
export type PackageSpec = { name: string; range: string | null; subpath: string };

const NAME = /^(?:@[a-z0-9~-][a-z0-9._~-]*\/)?[a-z0-9~-][a-z0-9._~-]*$/;
/** An exact semver version: what may name a folder in the cache. */
const EXACT = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;
const BUILTINS = new Set(builtinModules);

/** The package a specifier names, or null when it is not a package: a relative or absolute path, a URL, a builtin
 *  (`fs`, `node:fs`, `bun`, `bun:test`). A malformed package name is an error naming it. */
export function parseSpec(spec: string): PackageSpec | null {
  if (spec === "" || spec.startsWith(".") || spec.startsWith("/") || /^[a-z][a-z0-9+.-]*:/i.test(spec) || spec === "bun") return null;
  if (BUILTINS.has(spec) || BUILTINS.has(spec.split("/")[0]!)) return null;
  let rest = spec;
  let scope = "";
  if (rest.startsWith("@")) {
    const slash = rest.indexOf("/");
    if (slash < 0) throw new PackageError(`import ${JSON.stringify(spec)}: a scoped package is @scope/name`);
    scope = rest.slice(0, slash + 1);
    rest = rest.slice(slash + 1);
  }
  // In the part after the scope, the name ends at the first @ (a version) or / (a subpath).
  const cut = rest.search(/[@/]/);
  const bare = cut < 0 ? rest : rest.slice(0, cut);
  let range: string | null = null;
  let subpath = "";
  if (cut >= 0 && rest[cut] === "@") {
    const after = rest.slice(cut + 1);
    const slash = after.indexOf("/");
    range = slash < 0 ? after : after.slice(0, slash);
    subpath = slash < 0 ? "" : after.slice(slash);
    if (range === "") throw new PackageError(`import ${JSON.stringify(spec)}: nothing after the @; give a version, range or tag, or leave the @ out`);
  } else if (cut >= 0) {
    subpath = rest.slice(cut);
  }
  const name = scope + bare;
  if (!NAME.test(name)) throw new PackageError(`import ${JSON.stringify(spec)}: ${JSON.stringify(name)} is not an npm package name`);
  return { name, range, subpath };
}

/** The key a specifier's pin is recorded under: its name and range as written, without the subpath. */
export const pinKey = (p: PackageSpec) => (p.range === null ? p.name : `${p.name}@${p.range}`);

/** What the registry says of a package: its dist-tags and every version it has (npm's abbreviated packument). */
type Packument = { "dist-tags"?: Record<string, string>; versions?: Record<string, unknown> };

/** The exact version a range or tag comes to now: a dist-tag's version, else the highest version the range takes. */
export function pickVersion(name: string, range: string | null, doc: Packument, registry: string): string {
  const tags = doc["dist-tags"] ?? {};
  const versions = Object.keys(doc.versions ?? {});
  const tag = tags[range ?? "latest"];
  if (tag !== undefined) return tag;
  if (range === null) throw new PackageError(`${name}: ${registry} lists no "latest" dist-tag; import an exact version (${name}@x.y.z)`);
  const fits = versions.filter((v) => Bun.semver.satisfies(v, range)).sort((a, b) => Bun.semver.order(b, a));
  if (fits[0] === undefined) {
    throw new PackageError(`${name}@${range}: no version on ${registry} matches; it has ${versions.length} versions, latest ${tags.latest ?? "(none)"}`);
  }
  return fits[0];
}

/** Puts `name@version` in `dir`'s node_modules, or throws naming the package and what the installer said. */
export type Installer = (dir: string, name: string, version: string, registry: string) => Promise<void>;

/** The installer cells use: `bun install` of a package.json naming only this version, against `registry`. */
export const bunInstall: Installer = async (dir, name, version, registry) => {
  await writeFile(join(dir, "package.json"), JSON.stringify({ name: "harness-package", private: true, dependencies: { [name]: version } }, null, 2));
  // The registry bun installs from is the one the version was picked from, so the two never disagree.
  const proc = Bun.spawn([process.execPath, "install", "--no-progress", "--no-summary", "--ignore-scripts"], {
    cwd: dir, stdout: "pipe", stderr: "pipe", stdin: "ignore", timeout: 300_000,
    env: { ...process.env, BUN_CONFIG_REGISTRY: registry, NO_COLOR: "1" },
  });
  const [code, out, err] = await Promise.all([proc.exited, new Response(proc.stdout).text(), new Response(proc.stderr).text()]);
  if (code !== 0) {
    const said = (err.trim() || out.trim()).split("\n").slice(-15).join("\n");
    throw new PackageError(`${name}@${version}: bun install from ${registry} ${proc.signalCode ? `was killed (${proc.signalCode}, 300 s limit)` : `exited ${code}`}${said ? `:\n${said}` : ""}`);
  }
};

export type PackagesOptions = {
  /** The shared cache: one folder per package version, and pins.json. */
  root: string;
  /** The npm registry versions are picked from and installed from. */
  registry: string;
  fetch: (url: string, init?: RequestInit) => Promise<Response>;
  install: Installer;
};

type Pins = { version: 1; pins: Record<string, string> };

/** The registry the user's npm config names, else npm's. */
export const defaultRegistry = () => (process.env.npm_config_registry ?? process.env.NPM_CONFIG_REGISTRY ?? "https://registry.npmjs.org").replace(/\/+$/, "");
export const defaultRoot = () => join(process.env.HARNESS_HOME ?? `${process.env.HOME}/.harness`, "packages");

export class Packages {
  /** Installs under way in this kernel, by folder, so two cells importing one package install it once. */
  readonly #installing = new Map<string, Promise<void>>();
  constructor(readonly opts: PackagesOptions) {}

  get pinsPath() { return join(this.opts.root, "pins.json"); }

  async pins(): Promise<Record<string, string>> {
    // node:fs, not Bun.file: the kernel wraps Bun.file into a step (effects.ts), and an import that hits the cache is silent.
    const text = await readFile(this.pinsPath, "utf8").catch((e: NodeJS.ErrnoException) => { if (e.code === "ENOENT") return undefined; throw e; });
    if (text === undefined) return {};
    const v: unknown = JSON.parse(text);
    const pins = (v as Partial<Pins> | null)?.pins;
    if (typeof v !== "object" || v === null || (v as Partial<Pins>).version !== 1 || typeof pins !== "object" || pins === null
      || !Object.values(pins).every((x) => typeof x === "string" && EXACT.test(x))) {
      throw new PackageError(`${this.pinsPath}: not a pins file ({ version: 1, pins: { "<spec>": "<exact version>" } }); fix or delete it`);
    }
    return pins;
  }

  /** Record `key` → `version`, re-reading the file first so another kernel's pins are kept; written by rename. */
  async #pin(key: string, version: string): Promise<void> {
    const pins = await this.pins();
    if (pins[key] === version) return;
    const next: Pins = { version: 1, pins: Object.fromEntries(Object.entries({ ...pins, [key]: version }).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))) };
    const tmp = `${this.pinsPath}.${process.pid}.${crypto.randomUUID()}`;
    await writeFile(tmp, JSON.stringify(next, null, 2) + "\n");
    await rename(tmp, this.pinsPath);
  }

  /** The folder a version lives in. */
  dirOf(name: string, version: string) { return join(this.opts.root, `${name.replace("/", "+")}@${version}`); }

  async installed(name: string, version: string): Promise<boolean> {
    return stat(join(this.dirOf(name, version), "node_modules", name, "package.json")).then(() => true, () => false);
  }

  /** The exact version `p` is already bound to, with no registry call: itself when exact, else its pin, if any. */
  async known(p: PackageSpec): Promise<string | undefined> {
    if (p.range !== null && EXACT.test(p.range)) return p.range;
    return (await this.pins())[pinKey(p)];
  }

  /** The exact version `p`'s range or tag comes to on the registry now. */
  async lookup(p: PackageSpec): Promise<string> {
    const { registry } = this.opts;
    const url = `${registry}/${p.name.replace("/", "%2f")}`;
    let r: Response;
    try {
      r = await this.opts.fetch(url, { headers: { accept: "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8" } });
    } catch (e) {
      throw new PackageError(`${p.name}: could not reach ${registry}: ${e instanceof Error ? e.message : String(e)}`);
    }
    if (!r.ok) {
      const text = (await r.text().catch(() => "")).slice(0, 300);
      throw new PackageError(`${p.name}: ${registry} answered ${r.status}${r.status === 404 ? " (no such package)" : ""}${text ? `: ${text}` : ""}`);
    }
    const version = pickVersion(p.name, p.range, (await r.json()) as Packument, registry);
    if (!EXACT.test(version)) throw new PackageError(`${p.name}: ${registry} named version ${JSON.stringify(version)}, not an exact semver version`);
    return version;
  }

  /** Install `name@version` into its folder unless it is there: into a fresh folder, then renamed into place, so a
   *  kernel that dies half way leaves no half-installed package and two kernels racing keep one. */
  ensure(name: string, version: string): Promise<void> {
    const dir = this.dirOf(name, version);
    // Looked up and set with no await between, so a second caller always finds the first one's job.
    const running = this.#installing.get(dir);
    if (running) return running;
    const job = (async () => {
      if (await this.installed(name, version)) return;
      await mkdir(this.opts.root, { recursive: true });
      const tmp = `${dir}.tmp-${process.pid}-${crypto.randomUUID()}`;
      await mkdir(tmp, { recursive: true });
      try {
        await this.opts.install(tmp, name, version, this.opts.registry);
        if (!(await stat(join(tmp, "node_modules", name, "package.json")).then(() => true, () => false))) {
          throw new PackageError(`${name}@${version}: the install finished but left no node_modules/${name}/package.json in ${tmp}`);
        }
        await rename(tmp, dir).catch(async (e: NodeJS.ErrnoException) => {
          // Another kernel put the same version in place first: theirs is as good as ours.
          if ((e.code === "ENOTEMPTY" || e.code === "EEXIST") && (await this.installed(name, version))) return;
          throw e;
        });
      } finally {
        await rm(tmp, { recursive: true, force: true });
      }
    })().finally(() => this.#installing.delete(dir));
    this.#installing.set(dir, job);
    return job;
  }

  /** The absolute path of the module `p` names in its installed version. */
  path(p: PackageSpec, version: string): string {
    const dir = this.dirOf(p.name, version);
    try {
      return Bun.resolveSync(p.name + p.subpath, dir);
    } catch (e) {
      throw new PackageError(`${p.name}@${version}${p.subpath}: does not resolve in ${dir}: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  /** Resolve, install and pin `spec` as needed (one step when any of it happens) and return the module's path. */
  async locate(spec: string, p: PackageSpec): Promise<string> {
    const bound = await this.known(p);
    if (bound !== undefined && (await this.installed(p.name, bound))) return this.path(p, bound);
    const label = p.range === null ? p.name : `${p.name}@${p.range}`;
    const version = await effect("net", `Installing ${label}`, `Installed ${label}`, spec, async () => {
      const v = bound ?? (await this.lookup(p));
      await this.ensure(p.name, v);
      if (bound === undefined) await this.#pin(pinKey(p), v);
      return v;
    }, (v) => ({ detail: `${p.name}@${v}` }));
    return this.path(p, version);
  }

  /** A cell's `import(spec)` of a package: the module of its pinned, installed version. */
  async import(spec: string, p: PackageSpec): Promise<unknown> {
    return import(await this.locate(spec, p));
  }
}

let shared: Packages | undefined;
/** The kernel's package cache, made on first use. */
export const packages = () => (shared ??= new Packages({ root: defaultRoot(), registry: defaultRegistry(), fetch: (url, init) => fetch(url, init), install: bunInstall }));
