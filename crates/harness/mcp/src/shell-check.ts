import { createRequire } from 'node:module';
import { homedir } from 'node:os';
import { Language, Parser } from 'web-tree-sitter';
import type { Node } from 'web-tree-sitter';

/*
 * Ported from den's repl/src/shell-check.ts.
 *
 * The shell is for running programs: blaze, git, just, bun, builds and servers. Reading the file tree (find, fd, ls,
 * tree, stat, du), reading files (cat, head, tail, wc, sed -n, awk) and searching them (grep, rg, ag) is what the
 * cell's typed helpers do as data: glob, ls, du, rg and Bun.file. An agent that shells out for them guesses at flags
 * (the host's shell is the Mac's or Linux's own, so `stat -f '%z'` is a format on one and the file system on the
 * other) and parses text back: one asked for the largest files ran `find ... -exec stat` for 24 s.
 *
 * So a command (`sh(...)`, `$\`...\``) is parsed with tree-sitter-bash, and a read of the tree or of a file is
 * refused wherever it sits: alone, in a pipeline, behind xargs or find -exec, inside $(...) or <(...), under env,
 * time, nice or timeout, or in the script of `sh -c`, `bash -c` or `eval`. The refusal names the typed call, filled in from the command's own
 * arguments and the pipeline after it (`| sort -rn | head -5` becomes `sort: 'size', limit: 5`): a message the model
 * reads at the moment of the mistake teaches it better than a rule in its instructions.
 *
 * The same goes for the programs whose work has a typed call of its own: GitHub's reads through `gh` (`gh api`, `gh pr
 * list|status|view|checks`, `gh issue list|view`) are github.* (functions/github.ts), and HTTP through curl or wget is
 * fetch. `gh`'s writes (`gh pr create`, `gh pr merge`) and what has no call (`gh run view --log`) run.
 *
 * A filter on a program's output is not a read: `cargo test 2>&1 | grep FAIL` and `git log | head` run. The escape
 * for a real need is a comment that says why, `# why: <reason>`, anywhere in the command (or `.why(reason)` on a
 * `$\`...\`` template); the command then runs, and the verdict keeps the typed call as its `equivalent`.
 */

const load = createRequire(import.meta.url);
let loading: Promise<Parser> | undefined;
let loaded: Parser | undefined;
/** tree-sitter-bash in WebAssembly, loaded once (about 20 ms) on first call. */
export function bashParser(): Promise<Parser> {
  return loading ??= (async () => {
    await Parser.init();
    const bash = await Language.load(load.resolve('tree-sitter-bash/tree-sitter-bash.wasm'));
    const parser = new Parser();
    parser.setLanguage(bash);
    return loaded = parser;
  })();
}
/** Load the parser; after it resolves, `checkShell` is synchronous. Safe to call any number of times. */
export async function initShellCheck(): Promise<void> { await bashParser(); }

/** A word of a command: its value when nothing in it expands (quotes removed, `~/` resolved), else its text as written. */
interface Word { text: string; literal: boolean }
/** A simple command, wrappers taken off. */
interface Cmd {
  /** The program's name without its directory. */
  name: string;
  args: Word[];
  /** Reads the pipe from the command before it. */
  piped: boolean;
  /** Its stdin is this file (`< file`). */
  stdinFile?: string;
  /** Its stdin is text written in the command (a heredoc or here-string). */
  stdinText: boolean;
  /** Gets its operands from outside its words: `xargs cmd`, `find -exec cmd {}`. */
  fed: boolean;
  /** The command as written (for xargs and -exec, the xargs or find command), for the message. */
  source: string;
}
type Pipeline = Cmd[];

function wordOf(node: Node): Word {
  const text = node.text;
  switch (node.type) {
    case 'word': {
      const home = text.startsWith('~/') ? homedir() + text.slice(1) : text;
      return { text: home.replace(/\\(.)/g, '$1'), literal: !/[*?[]/.test(text) && (text === home || !text.startsWith('~')) };
    }
    case 'number': return { text, literal: true };
    case 'raw_string': return { text: text.slice(1, -1), literal: true };
    case 'string':
      if (node.namedChildren.some(child => child.type !== 'string_content')) return { text, literal: false };
      return { text: text.slice(1, -1).replace(/\\([$`"\\\n])/g, '$1'), literal: true };
    case 'concatenation': {
      const parts = node.namedChildren.map(wordOf);
      return { text: parts.every(part => part.literal) ? parts.map(part => part.text).join('') : text, literal: parts.every(part => part.literal) };
    }
    default: return { text, literal: false };
  }
}
const basename = (path: string) => path.slice(path.lastIndexOf('/') + 1);

const WRAPPERS = new Set(['env', 'command', 'nice', 'nohup', 'time', 'exec', 'stdbuf', 'timeout', 'sudo', 'doas', 'caffeinate', 'unbuffer']);
const SHELLS = new Set(['sh', 'bash', 'zsh', 'dash', 'ksh']);
/** xargs options that take a value as the next word. */
const XARGS_VALUED = new Set(['-I', '-n', '-P', '-L', '-s', '-d', '-E', '-a', '--max-args', '--max-procs', '--delimiter', '--arg-file', '--replace']);
const FIND_EXECS = ['-exec', '-execdir', '-ok', '-okdir'];
const FD_EXECS = ['-x', '--exec', '-X', '--exec-batch'];

/** The command a wrapper runs (`env A=1 cmd`, `timeout 10 cmd`, `nice -n 5 cmd`), or undefined when it runs none. */
function unwrap(name: string, args: Word[]): { name: string; args: Word[] } | undefined {
  let at = 0;
  const flag = () => at < args.length && args[at]!.text.startsWith('-');
  if (name === 'env') while (at < args.length && (args[at]!.text.includes('=') || flag())) at++;
  else if (name === 'timeout') { while (flag()) at++; at++; }
  else while (flag()) at += /^-[nu]$/.test(args[at]!.text) ? 2 : 1;
  const inner = args[at];
  return inner ? { name: basename(inner.text), args: args.slice(at + 1) } : undefined;
}

const STDIN_FILE = /^\d*<(?![<&>])/;
/** The commands of a parsed script, grouped by pipeline in source order; the script of `sh -c` and `eval` too. */
function pipelines(root: Node, parser: Parser, depth = 0): Pipeline[] {
  const groups = new Map<number, { start: number; cmds: Cmd[] }>();
  const nested: Pipeline[] = [];
  for (const node of root.descendantsOfType('command')) {
    const nameNode = node.childForFieldName('name');
    if (!nameNode) continue;
    let element: Node = node;
    let stdinFile: string | undefined;
    let stdinText = false;
    const redirects = [...node.namedChildren];
    const outer = node.parent;
    if (outer?.type === 'redirected_statement' && outer.childForFieldName('body')?.id === node.id) {
      element = outer;
      redirects.push(...outer.childrenForFieldName('redirect'));
    }
    for (const redirect of redirects) {
      if (redirect.type === 'heredoc_redirect' || redirect.type === 'herestring_redirect') stdinText = true;
      else if (redirect.type === 'file_redirect' && STDIN_FILE.test(redirect.text)) {
        const destination = redirect.childForFieldName('destination');
        if (destination) stdinFile = wordOf(destination).text;
      }
    }
    const pipeline = element.parent?.type === 'pipeline' ? element.parent : undefined;
    const piped = pipeline !== undefined && pipeline.namedChildren.filter(child => child.type !== 'comment').findIndex(child => child.id === element.id) > 0;
    let name = basename(wordOf(nameNode).text);
    let args = node.childrenForFieldName('argument').map(wordOf);
    for (let hops = 0; WRAPPERS.has(name) && hops < 4; hops++) {
      const inner = unwrap(name, args);
      if (!inner) break;
      ({ name, args } = inner);
    }
    const source = node.text;
    const cmds: Cmd[] = [{ name, args, piped, stdinFile, stdinText, fed: false, source }];
    const fed = (words: Word[]): Cmd => ({ name: basename(words[0]!.text), args: words.slice(1).filter(word => word.text !== '{}'), piped: false, stdinText: false, fed: true, source });
    if (name === 'xargs') {
      let at = 0;
      while (at < args.length && args[at]!.text.startsWith('-')) at += XARGS_VALUED.has(args[at]!.text) ? 2 : 1;
      // With no command xargs runs echo: it only reshapes what it reads.
      if (args[at]) cmds.push(fed(args.slice(at)));
    }
    if (name === 'find' || name === 'fd') {
      const execs = name === 'find' ? FIND_EXECS : FD_EXECS;
      for (let at = 0; at < args.length; at++) {
        if (!execs.includes(args[at]!.text)) continue;
        const end = args.findIndex((word, index) => index > at && (word.text === ';' || word.text === '+'));
        const words = args.slice(at + 1, end < 0 ? undefined : end);
        if (words.length) cmds.push(fed(words));
        at = end < 0 ? args.length : end;
      }
    }
    if ((SHELLS.has(name) || name === 'eval') && depth < 3) {
      const flagAt = args.findIndex(word => /^-[a-z]*c[a-z]*$/.test(word.text));
      const words = name === 'eval' ? args : flagAt < 0 ? [] : args.slice(flagAt + 1, flagAt + 2);
      if (words.length && words.every(word => word.literal)) {
        const tree = parser.parse(words.map(word => word.text).join(' '));
        if (tree) { nested.push(...pipelines(tree.rootNode, parser, depth + 1)); tree.delete(); }
      }
    }
    const key = pipeline?.id ?? node.id;
    const group = groups.get(key) ?? { start: (pipeline ?? node).startIndex, cmds: [] };
    group.cmds.push(...cmds);
    groups.set(key, group);
  }
  return [...[...groups.values()].sort((a, b) => a.start - b.start).map(group => group.cmds), ...nested];
}

interface Parsed {
  /** The words that are neither options nor an option's value. */
  operands: Word[];
  flags: string[];
  values: Map<string, string>;
  /** The first operand of a program that takes a pattern or script first (grep, sed, awk), unless `-e`/`-f` gave it. */
  script?: Word;
}
/** A command's words split into options and operands, given which options take a value. */
function parse(args: Word[], valued: Set<string>, scriptFirst = false): Parsed {
  const flags: string[] = [];
  const values = new Map<string, string>();
  const rest: Word[] = [];
  let script: Word | undefined;
  let scriptGiven = !scriptFirst;
  let ended = false;
  for (let at = 0; at < args.length; at++) {
    const { text } = args[at]!;
    if (!ended && text === '--') { ended = true; continue; }
    if (!ended && text.startsWith('-') && text !== '-') {
      const eq = text.indexOf('=');
      const flag = text.startsWith('--') && eq > 0 ? text.slice(0, eq) : text;
      flags.push(flag);
      if (text.startsWith('--') && eq > 0) values.set(flag, text.slice(eq + 1));
      else if (valued.has(flag) && at + 1 < args.length) values.set(flag, args[++at]!.text);
      else if (!text.startsWith('--') && text.length > 2 && valued.has(text.slice(0, 2))) values.set(text.slice(0, 2), text.slice(2));
      if (['-e', '-f', '--regexp', '--file', '--expression'].includes(flag.slice(0, flag.startsWith('--') ? undefined : 2))) scriptGiven = true;
      continue;
    }
    if (!scriptGiven) { script = args[at]; scriptGiven = true; continue; }
    rest.push(args[at]!);
  }
  if (scriptFirst && !script) {
    const given = values.get('-e') ?? values.get('--regexp') ?? values.get('--expression');
    if (given !== undefined) script = { text: given, literal: true };
  }
  return { operands: rest, flags, values, script };
}
/** Whether a short flag letter is set in any group of flags (`-la` has `l` and `a`), or one of the long forms is. */
const has = (flags: string[], letter: string, ...long: string[]) => flags.some(flag => (/^-[A-Za-z0-9]+$/.test(flag) && flag.includes(letter)) || long.includes(flag));

/** Code put into a call as written, not as a string. */
class Code { constructor(readonly text: string) {} }
/** A string as a JavaScript literal. */
const js = (text: string) => `'${text.replace(/\\/g, '\\\\').replace(/'/g, "\\'").replace(/\n/g, '\\n')}'`;
const show = (value: unknown): string => value instanceof Code ? value.text : typeof value === 'string' ? js(value) : Array.isArray(value) ? `[${value.map(show).join(', ')}]` : String(value);
/** An options object without its undefined entries, or '' when none is left. */
function object(entries: [string, unknown][]): string {
  const kept = entries.filter(([, value]) => value !== undefined);
  return kept.length ? `{ ${kept.map(([key, value]) => `${key}: ${show(value)}`).join(', ')} }` : '';
}
const call = (name: string, ...args: string[]) => `await ${name}(${args.filter(Boolean).join(', ')})`;
/** `undefined` for false, so the option is left out. */
const yes = (value: boolean) => value || undefined;
const isPattern = (text: string) => /[*?[{]/.test(text);
const isDevice = (text: string) => text === '-' || text.startsWith('/dev/') || text.startsWith('/proc/');
/** A directory as a pattern prefix: '' for the working directory. */
const prefix = (root: string | undefined) => !root || root === '.' || root === './' ? '' : `${root.replace(/\/+$/, '')}/`;

/** What the commands after a read in its pipeline do with what it printed. */
interface After {
  /** A sort, and whether it is numeric (`sort -n`, `-rn`, `-h`, `-k`). */
  sorted: boolean;
  numeric: boolean;
  /** `head -N`, or `tail -N` after a sort: how many to keep. */
  limit?: number;
  /** `wc -l`. */
  count: boolean;
  /** A grep over the lines: a filter on what was read. */
  grep?: Cmd;
  /** The command each path goes to: `xargs stat`, `xargs wc -l`, `-exec grep x {} +`. */
  each?: Cmd;
}
const HEAD_VALUED = new Set(['-n', '-c', '--lines', '--bytes']);
function countOf(cmd: Cmd): number {
  const { values, flags } = parse(cmd.args, HEAD_VALUED);
  const given = values.get('-n') ?? values.get('--lines') ?? flags.find(flag => /^-\d+$/.test(flag))?.slice(1);
  const number = given === undefined ? 10 : Number(given.replace(/^\+/, ''));
  return Number.isInteger(number) && number > 0 ? number : 10;
}
function after(rest: Cmd[]): After {
  const shape: After = { sorted: false, numeric: false, count: false };
  for (const cmd of rest) {
    if (cmd.fed) shape.each ??= cmd;
    else if (cmd.name === 'sort') { shape.sorted = true; shape.numeric = cmd.args.some(word => /^-[a-z]*[nhgk]/.test(word.text)); }
    else if (cmd.name === 'head' || (cmd.name === 'tail' && shape.sorted)) shape.limit = countOf(cmd);
    else if (cmd.name === 'wc' && cmd.args.some(word => word.text === '-l')) shape.count = true;
    else if (GREPS.has(cmd.name) && !shape.each) shape.grep ??= cmd;
  }
  return shape;
}
/** Programs that, given each path, mostly report its size or time. */
const SIZERS = new Set(['stat', 'du', 'ls', 'gstat', 'gdu']);

/** A program run once on paths (`xargs sed -i ...`, `-exec rustfmt {} +`): the paths go in as one array, each its own word. */
const feed = (each: Cmd, paths: string) => `await $\`${[each.name, ...each.args.map(word => word.text)].join(' ').replace(/[`\\]|\$(?=\{)/g, '\\$&')} \${${paths}}\``;

/** A function run on each path, in parallel. */
const eachPath = (paths: string, fn: string) => `await pmap(${paths}, ${fn})`;

/** The typed call for the paths a pattern finds, given what the pipeline did with them. */
function globCall(pattern: string, options: [string, unknown][], shape: After, extra: { stat?: boolean; time?: boolean; sort?: 'size' | 'mtime'; filter?: boolean } = {}): string {
  const found = (opts: [string, unknown][]) => `await glob(${[js(pattern), object(opts)].filter(Boolean).join(', ')})`;
  // find's -newer, -size and -mtime are a filter on the entries; what it is stays the agent's to write.
  const paths = extra.filter ? `(${found([...options, ['stat', true]])}).filter(entry => ...).map(entry => entry.path)` : found(options);
  const each = shape.each;
  if (each && !reads(each)) return feed(each, paths);
  if (each?.name === 'wc') return eachPath(paths, "async path => ({ path, lines: (await Bun.file(path).text()).split('\\n').length - 1 })");
  if (each && GREPS.has(each.name)) return grepCall({ ...each, fed: false }, after([]), pattern);
  if (each && READERS.has(each.name)) return eachPath(paths, 'async path => ({ path, text: await Bun.file(path).text() })');
  const stat = extra.stat || extra.filter || extra.sort !== undefined || each !== undefined || (shape.sorted && shape.numeric);
  const sort = extra.sort ?? (shape.sorted && shape.numeric ? extra.time ? 'mtime' : 'size' : undefined);
  const listed = found([...options, ['stat', yes(stat === true)], ['sort', sort], ['limit', shape.limit]]);
  const text = extra.filter ? `(${listed}).filter(entry => ...)` : listed;
  if (shape.count) return `(${text}).length`;
  const needle = shape.grep && parse(shape.grep.args, GREP_VALUED, true).script?.text;
  return needle && !stat ? `(${text}).filter(path => path.includes(${js(needle)}))` : text;
}

function findCall(cmd: Cmd, shape: After): string {
  const args = cmd.args;
  let at = 0;
  let follow = false;
  for (; at < args.length && /^-[HLP]$/.test(args[at]!.text); at++) if (args[at]!.text === '-L') follow = true;
  const roots: string[] = [];
  while (at < args.length && !/^[-(!]/.test(args[at]!.text)) roots.push(args[at++]!.text);
  let name: string | undefined;
  let path: string | undefined;
  let type: string | undefined;
  let maxDepth: number | undefined;
  let stat = false;
  let time = false;
  let filter = false;
  let remove = false;
  const ignore: string[] = [];
  let last: 'name' | 'path' | undefined;
  for (; at < args.length; at++) {
    const word = args[at]!.text;
    const next = args[at + 1]?.text;
    if (word === '-name' || word === '-iname') { name = next; last = 'name'; at++; }
    else if (word === '-path' || word === '-ipath' || word === '-wholename') { path = next; last = 'path'; at++; }
    else if (word === '-prune' && last) {
      // `-path ./target -prune -o ...`: what is pruned is left out, and the pattern is what follows.
      const pruned = last === 'name' ? name : path;
      if (pruned) ignore.push(pruned.replace(/^\.\//, '').replace(prefix(roots[0]), ''));
      if (last === 'name') name = undefined; else path = undefined;
      last = undefined;
    }
    else if (word === '-type') { type = next; at++; }
    else if (word === '-maxdepth') { maxDepth = Number(next); at++; }
    else if (/^-(size|newer|[acm](time|min))$/.test(word)) { filter = true; at++; }
    else if (word === '-printf' || word === '-fprintf') { stat = /%[sTtAC]/.test(next ?? '') || stat; time = /%[TtAC]/.test(next ?? '') && !next?.includes('%s'); at++; }
    else if (word === '-ls') stat = true;
    else if (word === '-delete') remove = true;
    else if (FIND_EXECS.includes(word)) { const end = args.findIndex((item, index) => index > at && (item.text === ';' || item.text === '+')); at = end < 0 ? args.length : end; }
  }
  const pattern = path !== undefined
    ? path.replace(/^\.\//, '').replace(/^\*\//, '**/').replace(/\/\*$/, '/**')
    : `${prefix(roots[0])}${maxDepth === 1 ? '' : '**/'}${name ?? '*'}`;
  const options: [string, unknown][] = [
    ['dot', yes((name ?? path ?? '').replace(/^\.\//, '').startsWith('.'))],
    ['onlyDirs', yes(type === 'd')],
    ['followSymlinks', yes(follow)],
    ['ignore', ignore.length > 1 ? ignore : ignore[0]],
  ];
  const rm: Cmd = { name: 'rm', args: [{ text: '-f', literal: true }], piped: false, stdinText: false, fed: true, source: cmd.source };
  return globCall(pattern, options, remove ? { ...shape, each: rm } : shape, { stat, time, filter });
}

function fdCall(cmd: Cmd, shape: After): string {
  const execAt = cmd.args.findIndex(word => FD_EXECS.includes(word.text));
  const words = execAt < 0 ? cmd.args : cmd.args.slice(0, execAt);
  const { operands, flags, values } = parse(words, new Set(['-e', '--extension', '-t', '--type', '-d', '--max-depth', '-E', '--exclude', '-S', '--size', '--changed-within', '--changed-before']));
  const [needle, root] = operands.map(word => word.text);
  const ext = values.get('-e') ?? values.get('--extension');
  const name = ext ? `*${needle ? `${needle}*` : ''}.${ext}` : needle ? isPattern(needle) || has(flags, 'g', '--glob') ? needle : `*${needle}*` : '*';
  const options: [string, unknown][] = [
    ['dot', yes(has(flags, 'H', '--hidden'))],
    ['gitignore', has(flags, 'I', '--no-ignore') ? false : undefined],
    ['onlyDirs', yes((values.get('-t') ?? values.get('--type'))?.startsWith('d') === true)],
  ];
  return globCall(`${prefix(root)}**/${name}`, options, shape, { stat: flags.includes('-S') || flags.includes('--size'), time: flags.some(flag => flag.startsWith('--changed')) });
}

function lsCall(cmd: Cmd, shape: After): string {
  const { operands, flags, values } = parse(cmd.args, new Set(['-I', '--ignore', '-w', '--width', '--sort', '--time-style', '--format']));
  const hidden = yes(has(flags, 'a') || has(flags, 'A'));
  const sort = has(flags, 'S') || values.get('--sort') === 'size' ? 'size' : has(flags, 't') || values.get('--sort') === 'time' ? 'mtime' : undefined;
  const dirs = operands.length ? operands.map(word => word.text) : ['.'];
  return dirs.map(dir => {
    if (isPattern(dir) || sort) return globCall(isPattern(dir) ? dir : `${prefix(dir)}*`, [['dot', hidden]], shape, { sort });
    const opts = object([['recursive', yes(has(flags, 'R', '--recursive'))], ['hidden', hidden], ['limit', shape.limit]]);
    const text = call('ls', dir === '.' && !opts ? '' : js(dir), opts);
    return shape.count ? `(${text}).length` : text;
  }).join('; ');
}

function treeCall(cmd: Cmd): string {
  const { operands, flags, values } = parse(cmd.args, new Set(['-L', '-I', '-P']));
  const depth = values.has('-L') ? Number(values.get('-L')) : undefined;
  return call('ls', js(operands[0]?.text ?? '.'), object([['recursive', yes(depth === undefined)], ['depth', depth], ['hidden', yes(has(flags, 'a'))], ['gitignore', true]]));
}

function statCall(cmd: Cmd): string {
  // `-f` takes a format in BSD's stat, the habit this guards against; in GNU's it is a flag.
  const files = parse(cmd.args, new Set(['-c', '--format', '--printf', '-t', '-f'])).operands.map(word => word.text);
  if (cmd.fed || !files.length) return "await glob('**/*', { stat: true })";
  if (files.some(isPattern)) return files.filter(isPattern).map(pattern => `await glob(${js(pattern)}, { stat: true })`).join('; ');
  if (files.length === 1) return `await Bun.file(${js(files[0]!)}).stat()`;
  return `await Promise.all([${files.map(js).join(', ')}].map(path => Bun.file(path).stat()))`;
}

function duCall(cmd: Cmd, shape: After): string {
  const { operands, flags, values } = parse(cmd.args, new Set(['-d', '--max-depth', '-B', '--block-size', '-t', '--threshold', '--exclude']));
  const given = values.get('-d') ?? values.get('--max-depth');
  const depth = given === undefined ? undefined : Number(given);
  const targets = operands.length ? operands.map(word => word.text) : ['.'];
  return [...new Set(targets.map(target => {
    // `du -sh dir/*` is each entry of dir; `du -sh dir` is dir's total.
    const each = target === '*' || target.endsWith('/*');
    const dir = each ? target.replace(/\/?\*$/, '') || '.' : target;
    const opts = object([['depth', depth && depth > 1 ? depth : undefined], ['limit', shape.limit]]);
    const text = call('du', dir === '.' && !opts ? '' : js(dir), opts);
    return has(flags, 's', '--summarize') && !each && operands.length <= 1 ? `(${text}).total` : text;
  }))].join('; ');
}

/** A file's lines as a typed expression, `slice` applied, else shaped by what came after (`| head -5`, `| wc -l`). */
function linesOf(file: string, shape: After, slice = ''): string {
  const text = `(await Bun.file(${js(file)}).text())`;
  if (slice) return `${text}.split('\\n')${slice}`;
  if (shape.grep) return grepCall({ ...shape.grep, args: [...shape.grep.args, { text: file, literal: true }], piped: false }, { ...shape, grep: undefined });
  if (shape.count) return `${text}.split('\\n').length - 1`;
  if (shape.limit !== undefined) return `${text}.split('\\n').slice(0, ${shape.limit})`;
  return `await Bun.file(${js(file)}).text()`;
}
const SCRIPT_VALUED: Record<string, Set<string>> = {
  sed: new Set(['-e', '-f', '-l', '--expression', '--file']),
  awk: new Set(['-F', '-v', '-f']),
  gawk: new Set(['-F', '-v', '-f']),
};
const readParse = (cmd: Cmd) => parse(cmd.args, SCRIPT_VALUED[cmd.name] ?? HEAD_VALUED, cmd.name in SCRIPT_VALUED);

function readCall(cmd: Cmd, shape: After): string {
  const parsed = readParse(cmd);
  const files = [...parsed.operands.map(word => word.text).filter(file => !isDevice(file)), ...(cmd.stdinFile ? [cmd.stdinFile] : [])];
  if (cmd.fed && !files.length) files.push('**/*');
  const pattern = files.find(isPattern);
  if (pattern) return globCall(pattern, [], { ...shape, each: { ...cmd, fed: true } });
  const one = (file: string): string => {
    switch (cmd.name) {
      case 'head': {
        const bytes = parsed.values.get('-c') ?? parsed.values.get('--bytes');
        return bytes !== undefined ? `(await Bun.file(${js(file)}).text()).slice(0, ${Number(bytes)})` : linesOf(file, shape, `.slice(0, ${countOf(cmd)})`);
      }
      case 'tail': {
        const fromStart = (parsed.values.get('-n') ?? parsed.flags.find(flag => /^[-+]\d+$/.test(flag)) ?? '').startsWith('+');
        return linesOf(file, shape, fromStart ? `.slice(${countOf(cmd) - 1})` : `.slice(-${countOf(cmd)})`);
      }
      case 'wc':
        if (has(parsed.flags, 'c', '--bytes')) return `Bun.file(${js(file)}).size`;
        if (has(parsed.flags, 'w', '--words')) return `(await Bun.file(${js(file)}).text()).split(/\\s+/).filter(Boolean).length`;
        return `(await Bun.file(${js(file)}).text()).split('\\n').length - 1`;
      case 'sed': {
        const range = /^(\d+)(?:,(\d+|\$))?p$/.exec(parsed.script?.text.trim() ?? '');
        if (!range || !has(parsed.flags, 'n', '--quiet', '--silent')) return linesOf(file, shape, '.map(line => ...)');
        const [, from, to] = range;
        return linesOf(file, shape, `.slice(${Number(from) - 1}${to === undefined ? `, ${from}` : to === '$' ? '' : `, ${to}`})`);
      }
      case 'awk': case 'gawk': return linesOf(file, shape, '.map(line => line.split(/\\s+/))');
      default: return linesOf(file, shape);
    }
  };
  if (files.length <= 1) return one(files[0] ?? 'path');
  return `await Promise.all([${files.map(js).join(', ')}].map(path => Bun.file(path).text()))`;
}

const GREP_VALUED = new Set(['-e', '-f', '-m', '-A', '-B', '-C', '-d', '-D', '--regexp', '--file', '--max-count', '--after-context', '--before-context', '--context', '--include', '--exclude', '--exclude-dir', '--label', '-g', '--glob', '--iglob', '-t', '--type', '-T', '--type-not', '-M', '--max-columns', '--max-depth', '-j', '--threads', '--replace', '--encoding', '--sort', '--sortr', '--colors', '-G']);
/** ripgrep's -r is --replace and -E --encoding; grep's are recursive and extended. */
const RG_VALUED = new Set([...GREP_VALUED, '-r', '-E']);
const grepParse = (cmd: Cmd) => parse(cmd.args, cmd.name === 'rg' || cmd.name === 'ag' ? RG_VALUED : GREP_VALUED, !cmd.args.some(word => word.text === '--files'));
/** grep's basic regex as ripgrep reads it: there `\|`, `\(` and `\+` are the operators. */
const fromBasic = (pattern: string) => pattern.replace(/\\([|(){}+?])/g, '$1');

function grepCall(cmd: Cmd, shape: After, globPattern?: string): string {
  const isRg = cmd.name === 'rg' || cmd.name === 'ag';
  const { operands, flags, values, script } = grepParse(cmd);
  const noIgnore = flags.includes('--no-ignore') || flags.some(flag => /^-u+$/.test(flag));
  if (isRg && flags.includes('--files')) {
    const where = operands[0]?.text;
    return globCall(`${prefix(where)}**/${values.get('-g') ?? values.get('--glob') ?? '*'}`, [['dot', yes(flags.includes('--hidden'))], ['gitignore', noIgnore ? false : undefined]], shape);
  }
  const fixed = has(flags, 'F', '--fixed-strings') || cmd.name === 'fgrep';
  const extended = isRg || fixed || cmd.name === 'egrep' || has(flags, 'E', '--extended-regexp') || has(flags, 'P', '--perl-regexp');
  const pattern = script ? extended ? script.text : fromBasic(script.text) : 'pattern';
  const context = values.get('-C') ?? values.get('--context');
  const before = values.get('-B') ?? values.get('--before-context');
  const afterLines = values.get('-A') ?? values.get('--after-context');
  const paths = operands.map(word => word.text).filter(path => !isDevice(path) && path !== '.');
  const globRoot = globPattern?.includes('**/') ? globPattern.slice(0, globPattern.indexOf('**/')).replace(/\/$/, '') : undefined;
  const include = values.get('--include') ?? values.get('-g') ?? values.get('--glob') ?? (globPattern ? basename(globPattern) : undefined);
  const filesOnly = has(flags, 'l', '--files-with-matches');
  const options = object([
    ['path', paths.length > 1 ? paths : paths[0] ?? (globRoot || undefined)],
    ['ignoreCase', yes(has(flags, 'i', '--ignore-case'))],
    ['smartCase', yes(isRg && has(flags, 'S', '--smart-case'))],
    ['fixed', yes(fixed)],
    ['word', yes(has(flags, 'w', '--word-regexp'))],
    ['glob', include && include !== '*' ? include : undefined],
    ['type', values.get('-t') ?? values.get('--type')],
    ['context', context !== undefined ? Number(context) : before !== undefined || afterLines !== undefined ? new Code(`{ before: ${Number(before ?? 0)}, after: ${Number(afterLines ?? 0)} }`) : undefined],
    ['maxCount', values.has('-m') ? Number(values.get('-m')) : undefined],
    ['hidden', yes(flags.includes('--hidden'))],
    ['noIgnore', yes(noIgnore)],
    ['filesOnly', yes(filesOnly)],
    ['limit', shape.limit],
  ]);
  const text = call('rg', js(pattern), options);
  const each = shape.each;
  if (filesOnly && each && !reads(each)) return feed(each, `(${text}).files`);
  if (filesOnly) return `(${text}).files`;
  if (has(flags, 'c', '--count') || shape.count) return `(${text}).count`;
  return text;
}

const GREPS = new Set(['grep', 'egrep', 'fgrep', 'rg', 'ag']);
const READERS = new Set(['cat', 'head', 'tail', 'wc', 'sed', 'awk', 'gawk']);
/** Programs that only read the tree, whatever their arguments. */
const LISTERS = new Set(['find', 'fd', 'ls', 'tree', 'stat', 'du', 'which']);
/** GNU's g-prefixed names on a Mac (gfind, gstat, gdu) are the same programs. */
const programOf = (cmd: Cmd) => cmd.name.replace(/^g(?=find$|stat$|du$|ls$|cat$|head$|tail$|wc$|sed$|grep$)/, '');

/** Whether a command reads the tree or a file, rather than a pipe, a here-doc, or nothing. */
function reads(cmd: Cmd): boolean {
  const name = programOf(cmd);
  if (LISTERS.has(name)) return true;
  const readsStdinFile = cmd.stdinFile !== undefined && !isDevice(cmd.stdinFile);
  if (READERS.has(name)) {
    if (name === 'tail' && cmd.args.some(word => /^-[a-zA-Z]*[fF]$|^--follow/.test(word.text))) return false;
    if (name === 'sed' && cmd.args.some(word => /^-[a-zA-Z]*i|^--in-place/.test(word.text))) return false;
    return readsStdinFile || cmd.fed || readParse({ ...cmd, name }).operands.some(word => !isDevice(word.text));
  }
  if (GREPS.has(name)) {
    const { operands, flags } = grepParse({ ...cmd, name });
    if (readsStdinFile || cmd.fed || flags.includes('--files') || operands.some(word => !isDevice(word.text))) return true;
    // With no path, ripgrep and ag search the working directory unless they read a pipe; grep does with -r.
    const recursive = name === 'rg' || name === 'ag' || has(flags, 'r', '--recursive') || has(flags, 'R', '--dereference-recursive');
    return recursive && !cmd.piped && !cmd.stdinText;
  }
  return false;
}

function callFor(cmd: Cmd, shape: After): string {
  const program = { ...cmd, name: programOf(cmd) };
  switch (program.name) {
    case 'find': return findCall(program, shape);
    case 'fd': return fdCall(program, shape);
    case 'ls': return lsCall(program, shape);
    case 'tree': return treeCall(program);
    case 'stat': return statCall(program);
    case 'du': return duCall(program, shape);
    case 'which': return program.args.filter(word => !word.text.startsWith('-')).map(word => `Bun.which(${js(word.text)})`).join('; ') || "Bun.which('name')";
    default: return GREPS.has(program.name) ? grepCall(program, shape) : readCall(program, shape);
  }
}

/** A flag's value: `--state open`, `--state=open`, `-s open`. */
function flagValue(args: Word[], ...names: string[]): string | undefined {
  for (let at = 0; at < args.length; at++) {
    const text = args[at]!.text;
    if (names.includes(text)) return args[at + 1]?.text;
    for (const name of names) if (name.startsWith('--') && text.startsWith(`${name}=`)) return text.slice(name.length + 1);
  }
  return undefined;
}

/** What a program with a typed call does instead, and why: GitHub's reads through `gh`, HTTP through curl or wget, and a
 *  web page opened with `open` (the Mac's default browser, not the harness browser the person sees). */
type Covered = { call: string; kind: 'github' | 'http' | 'browser' };

/** The typed call for a `gh` read: the api, a pull request's or issue's list or view. Writes (`gh pr create`, `merge`,
 *  `comment`) and what has no call (`gh run view --log`, `gh auth`) run. */
function ghCall(args: Word[]): string | undefined {
  const words = args.map(word => word.text);
  const [group, verb] = words;
  const fromUrl = words.map(word => /github\.com\/([\w.-]+\/[\w.-]+)\/(?:pull|issues)\/(\d+)/.exec(word)).find(match => match !== null);
  const repo = flagValue(args, '-R', '--repo') ?? fromUrl?.[1];
  const repoArg: [string, unknown] = ['repo', repo ?? new Code("'owner/name'")];
  const numberOf = () => {
    if (fromUrl) return Number(fromUrl[2]);
    for (const word of words.slice(2)) {
      const n = /^#?(\d+)$/.exec(word);
      if (n) return Number(n[1]);
    }
    return undefined;
  };
  if (group === 'api') {
    // The endpoint is the first operand: not a flag, nor the value a flag takes.
    const valued = new Set(['-X', '--method', '-f', '--raw-field', '-F', '--field', '-H', '--header', '-q', '--jq', '-t', '--template', '--input', '--hostname', '--cache', '-p', '--preview']);
    let path: string | undefined;
    for (let at = 1; at < words.length && path === undefined; at++) {
      if (valued.has(words[at]!)) at++;
      else if (!words[at]!.startsWith('-')) path = words[at];
    }
    if (path === 'graphql') return "await github.graphql<{ ... }>(`query { ... }`, { ...vars })";
    const method = flagValue(args, '-X', '--method')?.toUpperCase();
    return call('github.rest', js((path ?? 'repos/owner/name/...').replace(/^\/+/, '')), method && method !== 'GET' ? object([['method', method]]) : '');
  }
  if (group === 'pr' && (verb === 'list' || verb === 'status')) {
    const state = flagValue(args, '-s', '--state');
    const author = flagValue(args, '-A', '--author');
    const limit = flagValue(args, '-L', '--limit');
    return call('github.prs', object([repoArg, ['state', state], ['author', author], ['limit', limit === undefined ? undefined : Number(limit)]]));
  }
  if (group === 'pr' && (verb === 'view' || verb === 'checks')) {
    return call('github.pr', object([repoArg, ['n', numberOf() ?? new Code('n')]]));
  }
  if (group === 'issue' && verb === 'list') {
    const labels = flagValue(args, '-l', '--label');
    return call('github.issues', object([repoArg, ['state', flagValue(args, '-s', '--state')], ['author', flagValue(args, '-A', '--author')],
      ['assignee', flagValue(args, '-a', '--assignee')], ['labels', labels === undefined ? undefined : labels.split(',')]]));
  }
  if (group === 'issue' && verb === 'view') return "await github.graphql<{ ... }>(`query { repository(owner: ..., name: ...) { issue(number: n) { title body comments(first: 100) { nodes { body } } } } }`)";
  return undefined;
}

/** The typed call a command has that is not a read of files: `gh`'s GitHub reads and HTTP. */
function coveredBy(cmd: Cmd): Covered | undefined {
  if (cmd.name === 'gh') {
    const c = ghCall(cmd.args);
    return c === undefined ? undefined : { call: c, kind: 'github' };
  }
  if (cmd.name === 'curl' || cmd.name === 'wget') {
    const url = cmd.args.map(word => word.text).find(text => /^https?:\/\//.test(text)) ?? 'https://...';
    const out = flagValue(cmd.args, '-o', '--output', '-O', '--output-document');
    const fetched = `await fetch(${js(url)})`;
    return { call: out && out !== '-' ? `await Bun.write(${js(out)}, ${fetched})` : `await (${fetched}).text()`, kind: 'http' };
  }
  if (cmd.name === 'open') {
    const url = cmd.args.map(word => word.text).find(text => /^https?:\/\//.test(text));
    if (url !== undefined) return { call: `await browser.open(${js(url)})`, kind: 'browser' };
  }
  return undefined;
}

/** Where the host's tools differ between the Mac (BSD) and Linux (GNU): the typed call is the same on both. */
function platformNote(cmd: Cmd): string | undefined {
  const name = programOf(cmd);
  if (name === 'stat' && cmd.args.some(word => /^-[fc]$|^--format|^--printf/.test(word.text))) return "This shell is the host's: `stat -f` is a format on the Mac (BSD) but the file system (blocks, inodes) on Linux (GNU), whose format flag is `-c`. The typed call gives `size` and `mtime` on both.";
  return undefined;
}

/** The reason of a `# why: ...` comment in the command, if it has one. */
function why(root: Node): string | undefined {
  for (const comment of root.descendantsOfType('comment')) {
    const match = /^#\s*why:\s*(\S.*)$/i.exec(comment.text);
    if (match) return match[1]!.trim();
  }
  return undefined;
}

export interface ShellVerdict {
  /** Why the command is not run, naming the typed call to use instead; undefined when it may run. */
  refusal?: string;
  /** The typed call for a command that reads files but runs anyway (it said why): named again if it runs long. */
  equivalent?: string;
}

const clip = (text: string, max: number) => text.length > max ? `${text.slice(0, max - 1)}…` : text;

/**
 * Whether `script` may run. A read of the tree or of a file anywhere in it is refused with the typed call that does
 * it; a `# why:` comment, or a `reason` (`.why()`), lets it run and keeps the call as its equivalent.
 */
export async function vetShell(script: string, reason?: string): Promise<ShellVerdict> {
  return vet(await bashParser(), script, reason);
}

export type ShellCheck = { ok: true; equivalent?: string } | { ok: false; message: string };
/**
 * `vetShell`, synchronously: needs `await initShellCheck()` (or any `vetShell`) once before, and throws if not.
 * `{ ok: false, message }` is a refusal naming the typed call; `equivalent` is that call for a read that runs because
 * it said why.
 */
export function checkShell(script: string, reason?: string): ShellCheck {
  if (!loaded) throw new Error('checkShell: the bash parser is not loaded; `await initShellCheck()` first');
  const verdict = vet(loaded, script, reason);
  return verdict.refusal !== undefined ? { ok: false, message: verdict.refusal } : { ok: true, equivalent: verdict.equivalent };
}

function vet(parser: Parser, script: string, reason?: string): ShellVerdict {
  const tree = parser.parse(script);
  if (!tree) return {};
  try {
    const found: { cmd: Cmd; call: string; kind: 'read' | Covered['kind'] }[] = [];
    const all: Cmd[] = [];
    for (const pipeline of pipelines(tree.rootNode, parser)) {
      all.push(...pipeline);
      for (let at = 0; at < pipeline.length; at++) {
        const cmd = pipeline[at]!;
        const covered = coveredBy(cmd);
        if (covered) { found.push({ cmd, ...covered }); continue; }
        if (!reads(cmd)) continue;
        // A path-taking read fed by an earlier read (`find | xargs wc -l`) is part of that read's call.
        if (cmd.fed && pipeline.slice(0, at).some(item => !item.fed && reads(item))) continue;
        const own = (item: Cmd) => item.fed && item.source === cmd.source;
        const rest = pipeline.slice(at + 1);
        const shape = after(rest.filter(item => !own(item)));
        // find's -exec is the first thing done with each path, before anything piped.
        const inline = rest.find(own);
        found.push({ cmd, call: callFor(cmd, inline ? { ...shape, each: inline } : shape), kind: 'read' });
        // What follows a lister in its pipeline is folded into its call.
        if (LISTERS.has(programOf(cmd))) break;
      }
    }
    if (!found.length) return {};
    const calls = [...new Set(found.map(item => item.call))];
    if (reason?.trim() || why(tree.rootNode)) return { equivalent: calls.join('; ') };
    const namesOf = (kind: (typeof found)[number]['kind']) => [...new Set(found.filter(item => item.kind === kind).map(item => `\`${programOf(item.cmd)}\``))];
    const notes = [...new Set(all.map(platformNote).filter((note): note is string => note !== undefined))];
    const lead = calls.length === 1
      ? `Not run: use ${calls[0]}`
      : `Not run: use the typed calls:\n${found.map(item => `- ${item.call}   (for \`${clip(item.cmd.source, 60)}\`)`).join('\n')}`;
    const readers = namesOf('read');
    const said = [
      readers.length ? `${readers.join(', ')} only ${readers.length === 1 ? 'reads' : 'read'} files or the tree, which the typed helpers return as data.` : '',
      namesOf('github').length ? "GitHub's reads are github.* in ts (rows as Tables, gh's token, one step per call; await skill('github')), not `gh` in a shell." : '',
      namesOf('http').length ? 'HTTP is fetch in ts (one step per request), not curl or wget.' : '',
      namesOf('browser').length ? "A web page opens in the harness browser with browser.open in ts (the person sees it as a card and in the browser pane; you can read and drive it), not `open` in the Mac's default browser." : '',
    ].filter(Boolean).join(' ');
    return {
      refusal: [
        lead,
        `${said} The shell is for running programs no typed call covers (blaze, git, just, bun, builds, servers). If this really needs the shell, add a comment saying why, \`# why: <reason>\`, and it runs.`,
        ...notes,
      ].join('\n'),
    };
  } finally {
    tree.delete();
  }
}

/** The error a refused command throws: its message alone, with no stack (the frames would be the harness's, not the cell's). */
export function refusal(message: string): Error {
  const e = new Error(message);
  e.stack = `Error: ${message}`;
  return e;
}

// Loaded with the module, so sh() and spawn('proc') (which check synchronously) never meet an unloaded parser.
await initShellCheck();
