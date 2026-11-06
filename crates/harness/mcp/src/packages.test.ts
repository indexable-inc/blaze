import { afterAll, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PackageError, Packages, parseSpec, pickVersion, type Installer } from "./packages.ts";
import { rewrite } from "./repl.ts";

const scratch = mkdtempSync(join(tmpdir(), "harness-packages-"));
afterAll(() => rmSync(scratch, { recursive: true, force: true }));

test("a specifier splits into name, range and subpath; paths, URLs and builtins are not packages", () => {
  expect(parseSpec("zod")).toEqual({ name: "zod", range: null, subpath: "" });
  expect(parseSpec("lodash@4.17.21")).toEqual({ name: "lodash", range: "4.17.21", subpath: "" });
  expect(parseSpec("lodash@^4/fp")).toEqual({ name: "lodash", range: "^4", subpath: "/fp" });
  expect(parseSpec("lodash/fp")).toEqual({ name: "lodash", range: null, subpath: "/fp" });
  expect(parseSpec("@octokit/graphql")).toEqual({ name: "@octokit/graphql", range: null, subpath: "" });
  expect(parseSpec("@octokit/graphql@9.0.1/dist")).toEqual({ name: "@octokit/graphql", range: "9.0.1", subpath: "/dist" });
  for (const notPackage of ["./a.ts", "../b", "/abs/c.js", "node:fs", "fs", "fs/promises", "path", "bun", "bun:test", "https://x.dev/m.js"]) {
    expect(parseSpec(notPackage), notPackage).toBeNull();
  }
  expect(() => parseSpec("Bad Name")).toThrow(PackageError);
  expect(() => parseSpec("lodash@")).toThrow(/nothing after the @/);
  expect(() => parseSpec("@scope")).toThrow(/@scope\/name/);
});

test("a range picks the highest version it takes; a tag its version; nothing matching names the package", () => {
  const doc = { "dist-tags": { latest: "2.1.0", next: "3.0.0-rc.1" }, versions: { "1.0.0": {}, "1.4.2": {}, "2.0.0": {}, "2.1.0": {}, "3.0.0-rc.1": {} } };
  expect(pickVersion("p", null, doc, "R")).toBe("2.1.0");
  expect(pickVersion("p", "^1", doc, "R")).toBe("1.4.2");
  expect(pickVersion("p", "next", doc, "R")).toBe("3.0.0-rc.1");
  expect(() => pickVersion("p", "^9", doc, "R")).toThrow("p@^9: no version on R matches; it has 5 versions, latest 2.1.0");
});

/** A registry whose answers the test changes, and an installer that writes a package exporting its own version. */
function world(name: string) {
  const registry = "https://registry.test";
  const state = { latest: "1.0.0", status: 200, asked: 0, installs: [] as string[], installFails: false };
  const fetch = async (url: string) => {
    state.asked++;
    expect(url).toBe(`${registry}/${name.replace("/", "%2f")}`);
    if (state.status !== 200) return new Response(`{"error":"Not found"}`, { status: state.status });
    return Response.json({ "dist-tags": { latest: state.latest }, versions: { "1.0.0": {}, "1.1.0": {}, "2.0.0": {} } });
  };
  const install: Installer = async (dir, n, version, reg) => {
    expect(reg).toBe(registry);
    state.installs.push(`${n}@${version}`);
    if (state.installFails) throw new PackageError(`${n}@${version}: bun install from ${reg} exited 1:\nerror: GET ${reg}/${n} - 503`);
    const pkg = join(dir, "node_modules", n);
    await mkdir(pkg, { recursive: true });
    await Bun.write(join(pkg, "package.json"), JSON.stringify({ name: n, version, main: "index.js" }));
    await Bun.write(join(pkg, "index.js"), `module.exports = { version: ${JSON.stringify(version)} };`);
  };
  const packages = new Packages({ root: mkdtempSync(join(scratch, "cache-")), registry, fetch, install });
  return { state, packages };
}

const versionOf = async (packages: Packages, spec: string) => {
  const m = (await packages.import(spec, parseSpec(spec)!)) as { version?: string; default?: { version: string } };
  return m.default?.version ?? m.version;
};

test("a bare import is pinned the first time and resolves the same version after the registry moves", async () => {
  const { state, packages } = world("left-pad");
  expect(await versionOf(packages, "left-pad")).toBe("1.0.0");
  expect((await packages.pins())["left-pad"]).toBe("1.0.0");
  state.latest = "2.0.0";
  // A re-run asks the registry nothing and installs nothing: the pin and the cache answer.
  const asked = state.asked;
  expect(await versionOf(packages, "left-pad")).toBe("1.0.0");
  expect(state.asked).toBe(asked);
  expect(state.installs).toEqual(["left-pad@1.0.0"]);
  // A range is its own pin; an exact version needs none and never asks the registry.
  expect(await versionOf(packages, "left-pad@^1")).toBe("1.1.0");
  expect((await packages.pins())["left-pad@^1"]).toBe("1.1.0");
  const before = state.asked;
  expect(await versionOf(packages, "left-pad@2.0.0")).toBe("2.0.0");
  expect(state.asked).toBe(before);
  expect((await packages.pins())["left-pad@2.0.0"]).toBeUndefined();
});

test("two cells importing one package at once install it once", async () => {
  const { state, packages } = world("@scope/two");
  const [a, b] = await Promise.all([versionOf(packages, "@scope/two"), versionOf(packages, "@scope/two")]);
  expect([a, b]).toEqual(["1.0.0", "1.0.0"]);
  expect(state.installs).toEqual(["@scope/two@1.0.0"]);
});

test("a registry refusal and a failed install are errors naming the package; nothing is pinned or left behind", async () => {
  const missing = world("nope");
  missing.state.status = 404;
  await expect(missing.packages.import("nope", parseSpec("nope")!)).rejects.toThrow("nope: https://registry.test answered 404 (no such package)");
  expect(await missing.packages.pins()).toEqual({});

  const broken = world("flaky");
  broken.state.installFails = true;
  await expect(broken.packages.import("flaky", parseSpec("flaky")!)).rejects.toThrow("flaky@1.0.0: bun install from https://registry.test exited 1");
  expect(await broken.packages.pins()).toEqual({});
  expect(await broken.packages.installed("flaky", "1.0.0")).toBe(false);
  // The failed install's scratch folder is gone.
  expect([...new Bun.Glob("*").scanSync({ cwd: broken.packages.opts.root, onlyFiles: false })]).toEqual([]);
});

test("a cell's dynamic import goes through __import like a static one", () => {
  const { body } = rewrite(`const m = await import("zod@3.23.8"); const f = async () => (await import('lodash')).default; f`);
  expect(body).toContain(`__import("zod@3.23.8")`);
  expect(body).toContain(`__import('lodash')`);
  expect(body).not.toMatch(/(?<!_)import\(/);
});
