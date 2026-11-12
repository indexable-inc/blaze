import { expect, test } from "bun:test";
import { checkShell, parse } from "./shell-rule.ts";

const said = (line: string, why?: string) => { const v = checkShell(line, why); return v.ok ? undefined : v.message; };
const typed = (line: string) => said(line)?.split("\n")[0]!.replace(/^Not run: use /, "");

test("a line that only reads files or the tree is refused with the typed call, filled from its plain arguments", () => {
  const cases: [string, string][] = [
    ["cat Cargo.toml", "await Bun.file('Cargo.toml').text()"],
    ["head -n 20 src/lib.rs", "(await Bun.file('src/lib.rs').text()).split('\\n').slice(0, 10)"],
    ["grep -rn 'fn main' crates", "await rg('fn main', { path: 'crates' })"],
    ["rg TODO", "await rg('TODO')"],
    ["cat a.txt | grep x | sort | uniq", "await Bun.file('a.txt').text()"],
    ["cd src && ls", undefined as unknown as string],
    ["ls -la src", "await ls('src')"],
    ["find . -name '*.rs' | wc -l", "await glob('**/*', { cwd: '.', stat: true })"],
    ["du -sh target", "await du('target')"],
    ["env LC_ALL=C sort x | head", undefined as unknown as string],
    ["time cat x", "await Bun.file('x').text()"],
    ["/bin/cat x", "await Bun.file('x').text()"],
  ];
  for (const [line, call] of cases) expect(typed(line), line).toBe(call);
  const refused = said("cat Cargo.toml")!;
  expect(refused).toContain("help('files')");
  expect(refused).toContain("# why: <reason>");
  expect(said("cat a; ls b")).toStartWith("Not run: use the typed calls:\n- await Bun.file('a').text()\n- await ls('b')");
});

test("lines that run programs, filter what a program printed, write, follow or compute run", () => {
  const programs = [
    "cargo build -p den 2>&1 | tail -20",
    'cargo test 2>&1 | grep -E "^(test|FAIL)"',
    "git status --short; git diff --stat",
    "git status; cat notes.md",
    "git log --oneline | head",
    "bun run dev &",
    "sed -i 's/a/b/' src/lib.rs",
    "sed -i '' 's/a/b/' src/lib.rs",
    "tail -f server.log",
    "echo hi | grep h",
    "cat > notes.md <<EOF\nhello\nEOF",
    'printf "%s\\n" a b | sort | uniq -c',
    "cat /dev/null > out.log",
    "git ls-files | head",
    "find . -name '*.orig' -delete",
    "find . -name '*.ts' | xargs prettier --write",
    "cat $(git ls-files '*.md')",
    "make -j8 && ./target/release/blaze --version",
    "grep x",
    "",
  ];
  for (const line of programs) expect(said(line), line).toBeUndefined();
});

test("a reason lets a read run and keeps the typed call for it", () => {
  expect(checkShell("ls -la /Volumes # why: glob does not cross mount points")).toEqual({ ok: true, equivalent: "await ls('/Volumes')" });
  expect(said("ls src # why:")).toStartWith("Not run:");
  expect(checkShell("cat Cargo.toml", "reading a fifo")).toEqual({ ok: true, equivalent: "await Bun.file('Cargo.toml').text()" });
  expect(checkShell("git status")).toEqual({ ok: true });
  expect(said("echo '# why: not a comment' | cat")).toBeUndefined();
});

test("gh's GitHub reads, curl and a URL to open are refused with github.*, fetch and browser.open; gh's writes run", () => {
  expect(typed("gh api repos/o/r/actions/runs")).toBe("await github.rest('repos/o/r/actions/runs')");
  expect(typed("gh api graphql -f query='{ viewer { login } }' | jq .data")).toContain("github.graphql");
  expect(said("gh pr list")).toContain("github.* in ts");
  expect(typed("gh pr view 5")).toContain("github.pr");
  expect(typed("gh issue list -R o/r")).toContain("github.issues");
  expect(typed("curl -s https://example.com/a.json | jq .")).toBe("await (await fetch('https://example.com/a.json')).text()");
  expect(said("curl https://x.dev")).toContain("HTTP is fetch in ts");
  expect(typed("git fetch && curl https://x.dev")).toContain("fetch('https://x.dev')");
  expect(typed("open -a Dia https://x.dev/a")).toBe("await browser.open('https://x.dev/a')");
  for (const runs of ["open .", "gh pr create --fill", "gh pr merge 3 --squash", "gh run view 5 --log", "gh auth status", "gh api user # why: checking gh's own auth"]) expect(said(runs), runs).toBeUndefined();
});

test("the words are bash's: quotes join, operators split, a redirection or substitution marks the line as more than a read", () => {
  expect(parse(`A=1 cat 'a b'"c" | grep -n "x y"; ls`).pipelines).toEqual([
    [{ program: "cat", args: ["a bc"] }, { program: "grep", args: ["-n", "x y"] }],
    [{ program: "ls", args: [] }],
  ]);
  expect(parse("cat a > b").complex).toBe(true);
  expect(parse("cargo test 2>&1 | tail").complex).toBe(false);
  expect(parse('echo "$(date)"').complex).toBe(true);
  expect(parse("cat 'unterminated").complex).toBe(true);
});
