import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, renameSync, rmSync, statSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, parse, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { API as TsApi } from 'typescript/unstable/async';
import { inspectCell } from './cell-inspect.ts';
import { checkedReturns, declarations, lines, preludeText } from './cell-types.ts';
import type { CellDiagnostic, CheckerState, Inspection, LaneChecker, Prelude, ScopeKind } from './cell-types.ts';

/*
 * Ported from den's repl/src/type-service.ts (without den's live types). The type checker every lane of a server shares: one TypeScript 7 API process, owned by the server (core.ts), which
 * workers reach over their link (`types.*` calls). Measured on an M-series Mac, 2026-10-15, five cells per lane (lib and
 * Bun's types alone are 95 MB): 157, 220 and 255 MB for 1, 10 and 50 lanes, where a process per lane was 315 MB each.
 *
 * Lanes whose working directories share their path aliases (`paths` of the nearest tsconfig.json) share a project,
 * hence a program: lib, Bun's and the globals' types are parsed and bound once, and a lane adds only its cell modules.
 * Each lane's cells live in their own directory in the project; `roots.ts` imports the `cell.ts` of each lane in the
 * program, the most recently used `PROGRAM_LANES` of a project, and a lane that falls out keeps its scope and files and
 * comes back on its next check. Past `memoryMB` the process restarts with only the most recent lanes in its programs.
 *
 * A lane's packages: its directory is nested once per `node_modules` among its working directory's ancestors, each
 * level a link to one of them, so an import resolves exactly as from the working directory (nearest first, a
 * package's `exports` and types included). A placement is never reused, so the checker caches nothing stale when
 * the working directory moves.
 */

/** The repl's own directory: its tsconfig, its node_modules (TypeScript, @types/bun) and the module typing the globals. */
const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
/** The checker's process outlives a pause of this long across the whole server. */
const IDLE_MS = 5 * 60_000;
const CHECK_MS = 20_000;
/** What a warm-up checks: the globals agents reach for first, so their types are loaded before the first cell. */
const WARM = 'void [table, sql, ask, render, Bun.file, fetch, JSON.parse("1")];';
/** Lanes of one project in its program at once; the least recently used beyond this leave it. */
const PROGRAM_LANES = 32;
/** The process's memory past which it restarts, keeping `KEEP_ON_RESTART` lanes in its programs. */
const MEMORY_MB = 1024;
const KEEP_ON_RESTART = 8;
const MEASURE_MS = 10_000;
/** A cell that may hold a `parse<T>(…)` or a cast: only these are read for them (cell-inspect.ts). */
const INSPECTED = /\bparse\b|\bas\b|<\s*[\w{[(]/;

/**
 * The globals' declarations as a slim `.d.ts`: TypeScript's declaration emit of the prelude module and what it imports,
 * made once per version of the repl's sources, under node_modules/.cache so its imports resolve from the repl's
 * packages. The checker then reads declarations instead of every source behind the worker (471 files and 144 MB
 * against 557 and 175 MB, and half the cold check). Undefined when the emit fails, and the source module is used.
 */
export async function emitGlobals(module: string): Promise<string | undefined> {
  const src = dirname(module);
  const hash = createHash('sha256');
  for (const file of [join(ROOT, 'tsconfig.json'), join(ROOT, 'node_modules', 'typescript', 'package.json')]) hash.update(readFileSync(file));
  for (const file of readdirSync(src, { recursive: true, encoding: 'utf8' }).filter(name => name.endsWith('.ts')).sort()) hash.update(file).update(readFileSync(join(src, file)));
  const out = join(ROOT, 'node_modules', '.cache', 'repl-globals', hash.digest('hex').slice(0, 16));
  const target = join(out, `${parse(module).name}.d.ts`);
  if (existsSync(target)) return target;
  const staging = `${out}.${process.pid}`;
  const config = `${staging}.json`;
  mkdirSync(dirname(out), { recursive: true });
  writeFileSync(config, JSON.stringify({
    extends: join(ROOT, 'tsconfig.json'),
    compilerOptions: {
      noEmit: false, declaration: true, emitDeclarationOnly: true, outDir: staging, rootDir: src, noUncheckedIndexedAccess: false,
      typeRoots: [join(ROOT, 'node_modules', '@types')],
    },
    files: [module],
    include: [],
  }));
  // Type errors still emit; only a missing file is a failure.
  await new Promise<void>(resolve => execFile(process.execPath, [join(ROOT, 'node_modules', 'typescript', 'bin', 'tsc'), '-p', config], { timeout: 120_000 }, () => resolve()));
  rmSync(config, { force: true });
  if (existsSync(join(staging, `${parse(module).name}.d.ts`))) {
    try { renameSync(staging, out); } catch { /* another server made it first */ }
  }
  rmSync(staging, { recursive: true, force: true });
  return existsSync(target) ? target : undefined;
}

interface Project {
  key: string;
  dir: string;
  config: string;
  roots: string;
  opened: boolean;
}

interface Lane {
  key: string;
  id: string;
  scope: Map<string, { cell: string; kind: ScopeKind }>;
  /** Each cell module's text, by name (`c3`): rewritten wherever the lane is placed. */
  cells: Map<string, string>;
  count: number;
  lastUsed: number;
  inProgram: boolean;
  project?: Project;
  /** The placement's top directory, and the directory its cells are in (under one level per node_modules). */
  base?: string;
  dir?: string;
  signature?: string;
  /** `cell.ts` exists in this placement. */
  written?: boolean;
}

interface Session { api: TsApi }

export interface TypeServiceOptions {
  /** Restart the process past this resident size. Default 1024. */
  memoryMB?: number;
  /** Lanes of one project in its program at once. Default 32. */
  programLanes?: number;
  /** Check against the emitted `.d.ts` of the globals (emitGlobals). Default true. */
  slim?: boolean;
}

export interface TypeServiceStats { rssMB?: number; lanes: number; inProgram: number; projects: number; running: boolean }

/** The resident size of `pid` in MB, from ps; undefined when it cannot be read. */
function rssMB(pid: number): Promise<number | undefined> {
  return new Promise(resolve => execFile('ps', ['-o', 'rss=', '-p', String(pid)], (error, out) => {
    const kb = Number(String(out).trim());
    resolve(error || !Number.isFinite(kb) || kb <= 0 ? undefined : Math.round(kb / 1024));
  }));
}

const isDirectory = (path: string) => { try { return statSync(path).isDirectory(); } catch { return false; } };

export class TypeService {
  readonly dir = mkdtempSync(join(tmpdir(), 'repl-types-'));
  private session: Promise<Session> | undefined;
  private globals: Promise<Prelude> | undefined;
  private queue: Promise<unknown> = Promise.resolve();
  private idle: ReturnType<typeof setTimeout> | undefined;
  private readonly projects = new Map<string, Project>();
  private readonly lanes = new Map<string, Lane>();
  private readonly paths = new Map<string, Record<string, string[]>>();
  private pending = { created: [] as string[], changed: [] as string[] };
  private placements = 0;
  private measured = 0;
  private rss: number | undefined;
  private readonly memoryMB: number;
  private readonly programLanes: number;

  constructor(private readonly prelude: Prelude, private readonly options: TypeServiceOptions = {}) {
    this.memoryMB = options.memoryMB ?? MEMORY_MB;
    this.programLanes = options.programLanes ?? PROGRAM_LANES;
  }

  /** One lane's checker, whose working directory is `cwd()` at each call. */
  lane(key: string, cwd: () => string = () => process.cwd()): LaneChecker {
    return {
      diagnostics: (source) => this.check(key, source, cwd()),
      inspect: (source) => this.inspect(key, source, cwd()),
      commit: (source, bound) => this.commit(key, source, bound),
      save: () => this.save(key),
      load: (state, restored) => this.load(key, state, restored),
      warm: () => this.check(key, WARM, cwd()).then(() => undefined, () => undefined),
    };
  }

  /** Each call runs alone, in order: the process has one view of the files at a time. */
  private run<T>(work: () => Promise<T>): Promise<T> {
    const result = this.queue.then(work);
    this.queue = result.catch(() => undefined);
    return result;
  }

  private laneOf(key: string): Lane {
    let lane = this.lanes.get(key);
    if (!lane) this.lanes.set(key, lane = { key, id: `l${this.lanes.size + 1}`, scope: new Map(), cells: new Map(), count: 0, lastUsed: Date.now(), inProgram: false });
    return lane;
  }

  private start(): Promise<Session> {
    return this.session ??= (async () => {
      this.globals ??= this.options.slim === false
        ? Promise.resolve(this.prelude)
        : emitGlobals(this.prelude.module).then(module => module ? { ...this.prelude, module } : this.prelude, () => this.prelude);
      await this.globals;
      const { API } = await import('typescript/unstable/async');
      for (const project of this.projects.values()) project.opened = false;
      this.pending = { created: [], changed: [] };
      return { api: new API({ cwd: this.dir }) };
    })();
  }

  /** The path aliases of a tsconfig.json, every target absolute; none when it cannot be read. */
  private async aliases(file: string, api: TsApi): Promise<Record<string, string[]>> {
    const key = `${file}:${statSync(file).mtimeMs}`;
    const known = this.paths.get(key);
    if (known) return known;
    let found: Record<string, string[]> = {};
    try {
      const { options } = await api.parseConfigFile(file);
      const paths = options.paths;
      if (paths && typeof paths === 'object') {
        const base = typeof options.baseUrl === 'string' ? options.baseUrl : typeof options.pathsBasePath === 'string' ? options.pathsBasePath : dirname(file);
        found = Object.fromEntries(Object.entries(paths as Record<string, unknown>)
          .filter((entry): entry is [string, string[]] => Array.isArray(entry[1]))
          .map(([pattern, targets]) => [pattern, targets.map(target => isAbsolute(target) ? target : join(base, target))]));
      }
    } catch { /* an unreadable tsconfig gives no aliases */ }
    this.paths.set(key, found);
    return found;
  }

  private async project(key: string, paths: Record<string, string[]>): Promise<Project> {
    const known = this.projects.get(key);
    if (known) return known;
    const globals = await this.globals!;
    const dir = join(this.dir, `p${this.projects.size + 1}`);
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, 'tsconfig.json'), JSON.stringify({
      extends: join(ROOT, 'tsconfig.json'),
      compilerOptions: {
        noUncheckedIndexedAccess: false, typeRoots: [join(ROOT, 'node_modules', '@types')], noEmit: true,
        paths,
      },
      files: ['prelude.d.ts', 'roots.ts'],
    }));
    writeFileSync(join(dir, 'prelude.d.ts'), preludeText(globals));
    writeFileSync(join(dir, 'roots.ts'), 'export {};\n');
    const project = { key, dir, config: join(dir, 'tsconfig.json'), roots: join(dir, 'roots.ts'), opened: false };
    this.projects.set(key, project);
    return project;
  }

  /** Put the lane where its working directory's packages and aliases resolve: unchanged while they are the same. */
  private async place(lane: Lane, cwd: string, api: TsApi): Promise<void> {
    const modules: string[] = [];
    let tsconfig: string | undefined;
    for (let dir = cwd; ; dir = dirname(dir)) {
      if (isDirectory(join(dir, 'node_modules'))) modules.unshift(join(dir, 'node_modules'));
      if (!tsconfig && existsSync(join(dir, 'tsconfig.json'))) tsconfig = join(dir, 'tsconfig.json');
      if (dirname(dir) === dir) break;
    }
    const paths = tsconfig ? await this.aliases(tsconfig, api) : {};
    const key = createHash('sha256').update(JSON.stringify(paths)).digest('hex').slice(0, 12);
    const signature = [key, ...modules].join('\0');
    if (lane.signature === signature && lane.dir) return;
    const before = lane.project;
    const old = lane.base;
    const project = await this.project(key, paths);
    const base = join(project.dir, `${lane.id}-${++this.placements}`);
    let dir = base;
    modules.forEach((target, index) => {
      if (index) dir = join(dir, String(index));
      mkdirSync(dir, { recursive: true });
      symlinkSync(target, join(dir, 'node_modules'), 'dir');
    });
    mkdirSync(dir, { recursive: true });
    for (const [cell, text] of lane.cells) {
      writeFileSync(join(dir, `${cell}.ts`), text);
      this.pending.created.push(join(dir, `${cell}.ts`));
    }
    Object.assign(lane, { project, base, dir, signature, written: false });
    if (lane.inProgram) {
      lane.inProgram = false;
      if (before) this.writeRoots(before);
    }
    if (old) rmSync(old, { recursive: true, force: true });
  }

  private writeRoots(project: Project): void {
    const inside = [...this.lanes.values()].filter(lane => lane.inProgram && lane.project === project && lane.dir);
    writeFileSync(project.roots, `${inside.map(lane => `import './${relative(project.dir, join(lane.dir!, 'cell.ts'))}';`).join('\n')}\nexport {};\n`);
    this.pending.changed.push(project.roots);
  }

  /** The lane joins its project's program, and the least recently used beyond `programLanes` leave it. */
  private admit(lane: Lane): void {
    if (lane.inProgram) return;
    lane.inProgram = true;
    const inside = [...this.lanes.values()].filter(other => other.inProgram && other.project === lane.project).sort((a, b) => b.lastUsed - a.lastUsed);
    for (const other of inside.slice(this.programLanes)) other.inProgram = false;
    this.writeRoots(lane.project!);
  }

  /** The line in front of a cell: every scope name it does not declare itself, from the cell that bound it last. */
  private header(lane: Lane, own: ReadonlyMap<string, ScopeKind>): string {
    const imports = new Map<string, string[]>();
    const lets: string[] = [];
    for (const [name, { cell, kind }] of lane.scope) {
      if (own.has(name)) continue;
      if (kind === 'let') lets.push(`declare let ${name}: typeof import('./${cell}.ts').${name};`);
      else {
        const names = imports.get(cell) ?? [];
        imports.set(cell, names);
        names.push(kind === 'type' ? `type ${name}` : name);
      }
    }
    return [...[...imports].map(([cell, names]) => `import { ${names.join(', ')} } from './${cell}.ts';`), ...lets].map(line => `${line} `).join('');
  }

  /**
   * The diagnostics TypeScript reports for `source` as lane `key`'s next cell, run from `cwd`, on the cell's own lines.
   */
  async check(key: string, source: string, cwd: string): Promise<CellDiagnostic[]> {
    return (await this.inspect(key, source, cwd)).diagnostics;
  }

  /**
   * `check`, and what the cell's run needs from the checker: each `parse<T>(value)` compiled to its plan (cell-inspect.ts),
   * whose diagnostics (a type parse cannot check, an `as T` on an unknown value) are among the cell's.
   */
  inspect(key: string, source: string, cwd: string): Promise<Inspection> {
    return this.run(async () => {
      clearTimeout(this.idle);
      const lane = this.laneOf(key);
      lane.lastUsed = Date.now();
      try {
        const { api } = await this.start();
        await this.place(lane, cwd, api);
        this.admit(lane);
        const project = lane.project!;
        const file = join(lane.dir!, 'cell.ts');
        // The header shares the cell's first line, so every line keeps its number; a module, so the cell's own `const glob` shadows the global.
        const header = this.header(lane, declarations(source) ?? new Map());
        writeFileSync(file, `${header}${checkedReturns(source)}\nexport {};\n`);
        (lane.written ? this.pending.changed : this.pending.created).push(file);
        lane.written = true;
        const fileChanges = this.pending;
        this.pending = { created: [], changed: [] };
        let timer: ReturnType<typeof setTimeout> | undefined;
        const deadline = new Promise<never>((_, fail) => { timer = setTimeout(() => fail(new Error(`no answer in ${CHECK_MS / 1000} s`)), CHECK_MS); });
        try {
          const snapshot = await Promise.race([api.updateSnapshot({ ...(project.opened ? {} : { openProjects: [project.config] }), fileChanges }), deadline]);
          project.opened = true;
          try {
            const opened = snapshot.getProject(project.config);
            if (!opened) throw new Error('TypeScript opened no project for the cell');
            const found = await Promise.race([opened.program.getSemanticDiagnostics(file), deadline]);
            const lineOf = lines(source);
            const diagnostics = found.map(diagnostic => ({ line: lineOf(Math.max(0, diagnostic.pos - header.length)), code: diagnostic.code, text: flatten(diagnostic) }));
            if (!INSPECTED.test(source)) return { diagnostics, parses: [] };
            // tsgo's client keeps every file's syntax tree from the last snapshot when the update reports no changes, which
            // it does for `fileChanges`: the cell's tree would be the lane's previous cell.
            api.clearSourceFileCache();
            const inspected = await Promise.race([inspectCell(opened.program, opened.checker, file, header.length, 'prelude.d.ts', lineOf), deadline]);
            return { diagnostics: [...diagnostics, ...inspected.diagnostics], parses: inspected.parses };
          } finally { await snapshot.dispose().catch(() => undefined); }
        } finally { clearTimeout(timer); }
      } catch (error) {
        await this.stop();
        throw error;
      } finally {
        await this.bound();
        this.idle = setTimeout(() => { void this.run(() => this.stop()); }, IDLE_MS);
        this.idle.unref?.();
      }
    });
  }

  /** Past the memory bound, keep only the most recently used lanes in the programs and restart the process. */
  private async bound(): Promise<void> {
    if (Date.now() - this.measured < MEASURE_MS || !this.session) return;
    this.measured = Date.now();
    const session = await this.session.catch(() => undefined);
    const pid = session && processId(session.api);
    this.rss = pid ? await rssMB(pid) : undefined;
    if (this.rss === undefined || this.rss <= this.memoryMB) return;
    const keep = new Set([...this.lanes.values()].sort((a, b) => b.lastUsed - a.lastUsed).slice(0, KEEP_ON_RESTART));
    for (const lane of this.lanes.values()) lane.inProgram = keep.has(lane) && lane.inProgram;
    for (const project of this.projects.values()) this.writeRoots(project);
    await this.stop();
  }

  /**
   * `source` ran in lane `key`, and bound `bound` (all it declares when not given): it becomes the lane's next `cN.ts`
   * and its names join the scope. A name it declared but did not bind (its cell threw first) stays the earlier cell's,
   * as it does at run time. Needs no process: the file is written now if the lane is placed, else when it is.
   */
  commit(key: string, source: string, bound?: string[]): Promise<void> {
    return this.run(async () => {
      const own = declarations(source);
      if (!own) return;
      const lane = this.laneOf(key);
      const ran = bound && new Set(bound);
      const values = [...own].filter(([name, kind]) => kind !== 'type' && (!ran || ran.has(name)));
      const types = [...own].filter(([, kind]) => kind === 'type');
      if (!values.length && !types.length) return;
      const cell = `c${++lane.count}`;
      const exports = [
        values.length ? `export { ${values.map(([name]) => name).join(', ')} };` : '',
        types.length ? `export type { ${types.map(([name]) => name).join(', ')} };` : '',
      ].filter(Boolean).join(' ');
      const text = `${this.header(lane, own)}${source}\n${exports}\n`;
      lane.cells.set(cell, text);
      if (lane.dir) {
        writeFileSync(join(lane.dir, `${cell}.ts`), text);
        this.pending.created.push(join(lane.dir, `${cell}.ts`));
      }
      for (const [name, kind] of values) lane.scope.set(name, { cell, kind });
      // A type does not hide an earlier cell's value of the same name: the value is what the evaluator still has.
      for (const [name] of types) if (this.scope(lane, name) !== 'value') lane.scope.set(name, { cell, kind: 'type' });
    });
  }

  private scope(lane: Lane, name: string): 'value' | 'type' | undefined {
    const kind = lane.scope.get(name)?.kind;
    return kind === undefined ? undefined : kind === 'type' ? 'type' : 'value';
  }

  save(key: string): Promise<CheckerState> {
    return this.run(async () => {
      const lane = this.laneOf(key);
      return { cells: Object.fromEntries(lane.cells), scope: [...lane.scope] };
    });
  }

  load(key: string, state: CheckerState, restored: string[]): Promise<void> {
    return this.run(async () => {
      const lane = this.laneOf(key);
      const kept = new Set(restored);
      for (const [cell, text] of Object.entries(state.cells)) {
        lane.cells.set(cell, text);
        lane.count = Math.max(lane.count, Number(cell.slice(1)) || 0);
        if (lane.dir) {
          writeFileSync(join(lane.dir, `${cell}.ts`), text);
          this.pending.created.push(join(lane.dir, `${cell}.ts`));
        }
      }
      for (const [name, entry] of state.scope) {
        if (entry.kind === 'type') lane.scope.set(name, entry);
        else if (kept.has(name)) lane.scope.set(name, { cell: entry.cell, kind: 'let' });
      }
    });
  }

  /** The lane's evaluator is gone: its scope and files go too, and a new evaluator in the lane starts empty. */
  drop(key: string): Promise<void> {
    return this.run(async () => {
      const lane = this.lanes.get(key);
      if (!lane) return;
      this.lanes.delete(key);
      if (lane.inProgram && lane.project) this.writeRoots(lane.project);
      if (lane.base) rmSync(lane.base, { recursive: true, force: true });
    });
  }

  /** The process's resident size (as last measured, or now), and how many lanes and projects it holds. */
  async stats(): Promise<TypeServiceStats> {
    const session = await this.session?.catch(() => undefined);
    const pid = session && processId(session.api);
    if (pid) this.rss = await rssMB(pid);
    return {
      rssMB: this.rss, lanes: this.lanes.size, inProgram: [...this.lanes.values()].filter(lane => lane.inProgram).length,
      projects: this.projects.size, running: !!session,
    };
  }

  /** Stop the process; the next check starts it again over the same files. */
  private async stop(): Promise<void> {
    clearTimeout(this.idle);
    const session = this.session;
    this.session = undefined;
    if (!session) return;
    const { api } = await session.catch(() => ({ api: undefined }));
    await api?.close().catch(() => undefined);
  }

  /** Stop and remove every lane's files. */
  close(): Promise<void> {
    return this.run(async () => {
      await this.stop();
      rmSync(this.dir, { recursive: true, force: true });
    });
  }
}

/** The pid of the API's TypeScript process; its client keeps it private. */
function processId(api: TsApi): number | undefined {
  const child: unknown = Reflect.get(Reflect.get(api, 'client') ?? {}, 'process');
  const pid: unknown = child && typeof child === 'object' ? Reflect.get(child, 'pid') : undefined;
  return typeof pid === 'number' ? pid : undefined;
}

type Chained = { text: string; messageChain?: readonly Chained[] | undefined };
/** A diagnostic's text and its chain, depth first ("No overload matches this call. The last overload gave the following error. Object literal may only ..."). */
function flatten(diagnostic: Chained): string {
  const texts: string[] = [];
  const walk = (item: Chained) => { texts.push(item.text); for (const next of item.messageChain ?? []) walk(next); };
  walk(diagnostic);
  return texts.slice(0, 6).join(' ').replace(/\s+/g, ' ').trim();
}
