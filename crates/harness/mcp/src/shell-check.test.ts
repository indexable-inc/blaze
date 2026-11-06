// Ported from den's repl/test/shell-check.test.ts.
import { expect, test } from 'bun:test';
import { checkShell, initShellCheck, vetShell } from './shell-check.ts';

const refusal = async (line: string) => (await vetShell(line)).refusal;
/** The typed call a refusal names: its first line, after "Not run: use ". */
const typed = async (line: string) => (await refusal(line))?.split('\n')[0]?.replace(/^Not run: use /, '');

test("the agent's largest-files command is refused with the glob call that does it", async () => {
  const line = "find . -type f -exec stat -f '%z %N' {} + | sort -rn | head -5";
  const said = await refusal(line);
  expect(said).toStartWith("Not run: use await glob('**/*', { stat: true, sort: 'size', limit: 5 })");
  // The way out is said, and so is how `stat -f` differs between the Mac and Linux.
  expect(said).toContain('# why: <reason>');
  expect(said).toContain('`stat -f` is a format on the Mac');
});

test('a read is refused behind -exec, xargs, a pipe, a substitution, sh -c, eval and wrappers', async () => {
  const lines = "async path => ({ path, lines: (await Bun.file(path).text()).split('\\n').length - 1 })";
  const cases: [string, string][] = [
    ["find src -name '*.rs' -exec wc -l {} +", `await pmap(await glob('src/**/*.rs'), ${lines})`],
    ['find . -type f -print0 | xargs -0 stat -c %s | sort -n | tail -3', "await glob('**/*', { stat: true, sort: 'size', limit: 3 })"],
    ['git ls-files | xargs wc -l', `await pmap(await glob('**/*'), ${lines})`],
    ['cat Cargo.toml | grep members', "await rg('members', { path: 'Cargo.toml' })"],
    ['cat Cargo.toml | wc -l', "(await Bun.file('Cargo.toml').text()).split('\\n').length - 1"],
    ['for f in $(find src -name "*.ts"); do echo "$f"; done', "await glob('src/**/*.ts')"],
    ['echo `cat VERSION`', "await Bun.file('VERSION').text()"],
    ["sh -c 'cat a.txt'", "await Bun.file('a.txt').text()"],
    ['bash -lc "head -n 20 src/main.rs"', "(await Bun.file('src/main.rs').text()).split('\\n').slice(0, 20)"],
    ["eval 'ls src'", "await ls('src')"],
    ['timeout 5 env FOO=1 cat notes.md', "await Bun.file('notes.md').text()"],
    ['sudo nice -n 5 time cat notes.md', "await Bun.file('notes.md').text()"],
    ['(cd crates && ls)', 'await ls()'],
    ['wc -l < Cargo.toml', "(await Bun.file('Cargo.toml').text()).split('\\n').length - 1"],
  ];
  for (const [line, call] of cases) expect(await typed(line), line).toBe(call);
  // Two reads in one line name both calls.
  const both = await refusal('diff <(cat a) <(cat b)');
  expect(both).toContain("await Bun.file('a').text()");
  expect(both).toContain("await Bun.file('b').text()");
});

test('the typed call is filled in from the arguments', async () => {
  const cases: [string, string][] = [
    ['ls -la', "await ls('.', { hidden: true })"],
    ['ls -lt src | head', "await glob('src/*', { stat: true, sort: 'mtime', limit: 10 })"],
    ['ls | wc -l', '(await ls()).length'],
    ['du -sh * | sort -h | tail -5', "await du('.', { limit: 5 })"],
    ['du -sh target', "(await du('target')).total"],
    ['du -h -d 2 crates | sort -rh | head', "await du('crates', { depth: 2, limit: 10 })"],
    ["sed -n '10,20p' src/lib.rs", "(await Bun.file('src/lib.rs').text()).split('\\n').slice(9, 20)"],
    ['tail -n 30 log.txt', "(await Bun.file('log.txt').text()).split('\\n').slice(-30)"],
    ['grep -rn "fn main" crates --include=\'*.rs\' | head -20', "await rg('fn main', { path: 'crates', glob: '*.rs', limit: 20 })"],
    ['grep -rli "fleet\\|aws" docs', "(await rg('fleet|aws', { path: 'docs', ignoreCase: true, filesOnly: true })).files"],
    ['rg -C 2 -t rust TODO', "await rg('TODO', { type: 'rust', context: 2 })"],
    ["rg --files -g '*.toml'", "await glob('**/*.toml')"],
    ['rg -l TODO | xargs sed -i s/TODO/DONE/', "await $`sed -i s/TODO/DONE/ ${(await rg('TODO', { filesOnly: true })).files}`"],
    ["find . -path ./target -prune -o -name '*.rs' -print", "await glob('**/*.rs', { ignore: 'target' })"],
    ["find . -name '*.orig' -delete", "await $`rm -f ${await glob('**/*.orig')}`"],
    ['fd -e rs -x rustfmt', "await $`rustfmt ${await glob('**/*.rs')}`"],
    ['tree -L 2 src', "await ls('src', { depth: 2, gitignore: true })"],
    ['stat README.md', "await Bun.file('README.md').stat()"],
    ['which cargo', "Bun.which('cargo')"],
  ];
  for (const [line, call] of cases) expect(await typed(line), line).toBe(call);
});

test('lines that run programs, or filter what a program printed, run', async () => {
  const programs = [
    'cargo build -p den 2>&1 | tail -20',
    'cargo test 2>&1 | grep -E "^(test|FAIL)"',
    'git status --short; git diff --stat',
    'git log --oneline | head',
    'just repl-test && bun test src/shell-check.test.ts',
    'bun run dev &',
    "sed -i 's/a/b/' src/lib.rs",
    "sed -i '' 's/a/b/' src/lib.rs",
    'tail -f server.log',
    'echo hi | grep h',
    'cat > notes.md <<EOF\nhello\nEOF',
    'printf "%s\\n" a b | sort | uniq -c',
    'cat /dev/null > out.log',
    'git ls-files | head',
    'make -j8 && ./target/release/blaze --version',
    '',
  ];
  for (const line of programs) expect(await refusal(line), line).toBeUndefined();
});

test('a # why: comment lets a read run and keeps the typed call for it', async () => {
  const verdict = await vetShell('find . -newer stamp -exec touch {} + # why: needs -newer against a stamp file');
  expect(verdict.refusal).toBeUndefined();
  expect(verdict.equivalent).toContain('await glob(');
  // An empty reason is no reason.
  expect((await vetShell('ls src # why:')).refusal).toStartWith('Not run:');
  expect((await vetShell('ls src', 'listing a FUSE mount the walker skips')).refusal).toBeUndefined();
});

test('checkShell is synchronous once the parser is loaded', async () => {
  await initShellCheck();
  expect(checkShell('git status')).toEqual({ ok: true, equivalent: undefined });
  const refused = checkShell('cat Cargo.toml');
  expect(refused.ok).toBe(false);
  if (!refused.ok) expect(refused.message).toStartWith("Not run: use await Bun.file('Cargo.toml').text()");
  expect(checkShell('cat Cargo.toml', 'reading a fifo')).toEqual({ ok: true, equivalent: "await Bun.file('Cargo.toml').text()" });
});

test("gh's GitHub reads and curl are refused with github.* and fetch; gh's writes run", async () => {
  const cases: [string, string][] = [
    ["gh api graphql -f query='{ viewer { login } }' | jq .data", "await github.graphql<{ ... }>(`query { ... }`, { ...vars })"],
    ['gh api repos/o/r/actions/runs', "await github.rest('repos/o/r/actions/runs')"],
    ['gh api -X POST /repos/o/r/dispatches', "await github.rest('repos/o/r/dispatches', { method: 'POST' })"],
    ['gh pr list -R indexable-inc/ix --author andrew -L 12', "await github.prs({ repo: 'indexable-inc/ix', author: 'andrew', limit: 12 })"],
    ['gh pr view 42 --repo o/r --json files', "await github.pr({ repo: 'o/r', n: 42 })"],
    ['gh pr checks https://github.com/o/r/pull/9', "await github.pr({ repo: 'o/r', n: 9 })"],
    ['gh pr view 5', "await github.pr({ repo: 'owner/name', n: 5 })"],
    ['gh issue list -R o/r -l bug,ui', "await github.issues({ repo: 'o/r', labels: ['bug', 'ui'] })"],
    ['curl -s https://example.com/a.json | jq .', "await (await fetch('https://example.com/a.json')).text()"],
    ['curl -L -o out.tgz https://example.com/x.tgz', "await Bun.write('out.tgz', await fetch('https://example.com/x.tgz'))"],
  ];
  for (const [line, call] of cases) expect(await typed(line), line).toBe(call);
  expect(await refusal('gh pr list')).toContain('github.* in ts');
  expect(await refusal('curl https://x.dev')).toContain('HTTP is fetch in ts');
  expect(await typed('open https://google.com')).toBe("await browser.open('https://google.com')");
  expect(await typed('open -a Dia https://x.dev/a')).toBe("await browser.open('https://x.dev/a')");
  expect(await refusal('open https://google.com')).toContain('harness browser');
  expect(await refusal('open .')).toBeUndefined();
  for (const runs of ['gh pr create --fill', 'gh pr merge 3 --squash', 'gh run view 5 --log', 'gh auth status', "gh api user # why: checking gh's own auth"]) {
    expect(await refusal(runs), runs).toBeUndefined();
  }
});
