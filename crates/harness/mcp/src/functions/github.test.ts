import { expect, test } from "bun:test";
import { GithubError, issueSearch, makeGithub, prSearch } from "./github.ts";

const TOKEN = "ghp_secret_never_shown";

/** A fake api.github.com: records each request, answers with `reply(body)`. */
function fake(reply: (req: { query: string; variables: Record<string, unknown> }) => { status?: number; json: unknown }) {
  const seen: { url: string; auth: string | null; query: string; variables: Record<string, unknown> }[] = [];
  let tokenReads = 0;
  const gh = makeGithub({
    token: async () => { tokenReads++; return TOKEN; },
    fetch: async (url, init) => {
      const body = JSON.parse(String(init?.body ?? "{}")) as { query: string; variables: Record<string, unknown> };
      seen.push({ url, auth: new Headers(init?.headers).get("authorization"), ...body });
      const r = reply(body);
      return Response.json(r.json, { status: r.status ?? 200 });
    },
  });
  return { gh, seen, tokenReads: () => tokenReads };
}

const pr = (n: number, o: Partial<Record<string, unknown>> = {}) => ({
  number: n, title: `PR ${n}`, state: "OPEN", isDraft: false, url: `https://github.com/o/r/pull/${n}`, author: { login: "andrew" },
  headRefName: `b${n}`, baseRefName: "main", createdAt: "2026-11-01T00:00:00Z", updatedAt: "2026-11-05T00:00:00Z",
  mergeable: "MERGEABLE", reviewDecision: "APPROVED", commits: { nodes: [{ commit: { oid: "abc", statusCheckRollup: { state: "FAILURE" } } }] }, ...o,
});

test("prs is one search query whose rows are a typed table", async () => {
  const { gh, seen } = fake(() => ({ json: { data: { search: { nodes: [pr(1), pr(2, { isDraft: true, reviewDecision: null, commits: { nodes: [] }, author: null }), {}] } } } }));
  const rows = await gh.prs({ repo: "indexable-inc/ix", author: "andrew", limit: 12 });
  expect(seen).toHaveLength(1);
  expect(seen[0]!.url).toBe("https://api.github.com/graphql");
  expect(seen[0]!.auth).toBe(`bearer ${TOKEN}`);
  expect(seen[0]!.variables).toEqual({ q: "repo:indexable-inc/ix is:pr is:open author:andrew sort:updated-desc", first: 12 });
  expect(rows.map((r) => [r.number, r.state, r.draft, r.ci, r.mergeable, r.review, r.author])).toEqual([
    [1, "open", false, "failure", "mergeable", "approved", "andrew"],
    [2, "open", true, null, "mergeable", null, "ghost"],
  ]);
  expect(rows[0]!.updated).toEqual(new Date("2026-11-05T00:00:00Z"));
});

test("search strings say state, author, assignee and labels the way GitHub search reads them", () => {
  expect(prSearch({ repo: "o/r", state: "merged" })).toBe("repo:o/r is:pr is:merged sort:updated-desc");
  expect(prSearch({ repo: "o/r", state: "closed" })).toBe("repo:o/r is:pr is:closed is:unmerged sort:updated-desc");
  expect(prSearch({ repo: "o/r", state: "all" })).toBe("repo:o/r is:pr sort:updated-desc");
  expect(issueSearch({ repo: "o/r", assignee: "me", labels: ["bug", "good first issue"] }))
    .toBe('repo:o/r is:issue is:open assignee:me label:bug label:"good first issue" sort:updated-desc');
});

test("pr gives files, checks, comments and reviews as tables", async () => {
  const { gh, seen } = fake(() => ({ json: { data: { repository: { pullRequest: {
    ...pr(7), body: "why", additions: 10, deletions: 2,
    files: { nodes: [{ path: "a.ts", changeType: "MODIFIED", additions: 10, deletions: 2 }] },
    comments: { nodes: [{ author: { login: "c" }, body: "lgtm", createdAt: "2026-11-02T00:00:00Z", url: "u" }] },
    reviews: { nodes: [{ author: { login: "r" }, state: "APPROVED", body: "", submittedAt: null }] },
    commits: { nodes: [{ commit: { oid: "deadbeef", statusCheckRollup: { state: "PENDING", contexts: { nodes: [
      { __typename: "CheckRun", name: "test", status: "COMPLETED", conclusion: "FAILURE", detailsUrl: "d", startedAt: "2026-11-02T00:00:00Z", completedAt: "2026-11-02T00:10:00Z", checkSuite: { workflowRun: { workflow: { name: "CI" } } } },
      { __typename: "StatusContext", context: "deploy", state: "PENDING", targetUrl: null, createdAt: "2026-11-02T00:00:00Z" },
    ] } } } }] },
  } } } } }));
  const p = await gh.pr({ repo: "o/r", n: 7 });
  expect(seen[0]!.variables).toEqual({ owner: "o", name: "r", n: 7 });
  expect(p.sha).toBe("deadbeef");
  expect(p.ci).toBe("pending");
  expect(p.files.map((f) => [f.path, f.change])).toEqual([["a.ts", "modified"]]);
  expect(p.checks.map((c) => [c.name, c.workflow, c.status, c.conclusion])).toEqual([["test", "CI", "completed", "failure"], ["deploy", null, "pending", "pending"]]);
  expect(p.comments[0]!.body).toBe("lgtm");
  expect(p.reviews[0]!.state).toBe("approved");
});

test("checks names a ref that is no commit", async () => {
  const { gh } = fake(() => ({ json: { data: { repository: { object: null } } } }));
  await expect(gh.checks({ repo: "o/r", ref: "nope" })).rejects.toThrow("github.checks: nope names no commit in o/r");
});

test("GitHub's errors name the operation, and the token appears in none of them", async () => {
  const { gh, tokenReads } = fake((req) => req.query.includes("Bad")
    ? { json: { errors: [{ message: "Field 'x' doesn't exist", path: ["query", "x"] }] } }
    : { status: 401, json: { message: "Bad credentials" } });
  const bad = await gh.graphql("query Bad { x }").catch((e: unknown) => e);
  expect(bad).toBeInstanceOf(GithubError);
  expect(String(bad)).toContain("github.graphql Bad: query.x: Field 'x' doesn't exist");
  const denied = await gh.graphql("query Me { viewer { login } }").catch((e: unknown) => e);
  expect(String(denied)).toContain("github.graphql Me: GitHub answered 401: Bad credentials");
  for (const e of [bad, denied]) expect(String(e) + ((e as Error).stack ?? "")).not.toContain(TOKEN);
  expect(tokenReads()).toBe(2);
});

test("a bad repo or limit is refused before any request", async () => {
  const { gh, seen } = fake(() => ({ json: {} }));
  await expect(gh.prs({ repo: "ix" })).rejects.toThrow('github.prs: repo "ix" is not "owner/name"');
  await expect(gh.issues({ repo: "o/r", limit: 500 })).rejects.toThrow("github.issues: limit 500 is not 1 to 100");
  expect(seen).toHaveLength(0);
});
