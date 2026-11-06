// GitHub as typed calls: github.graphql(query, vars) and helpers that are each one GraphQL query (prs, pr, issues,
// checks), plus github.rest for the endpoints GraphQL lacks (Actions logs). Agents used to shell out to
// `gh api graphql -f query=...` and parse its JSON back; these return data, rows as Tables, and the person sees one
// step per call ("Queried GitHub · 12 PRs in indexable-inc/ix").
//
// The token. It is gh's own (`gh auth token`, which honours GH_TOKEN), read once per kernel on first use and kept only
// in this module's closure. It never goes into a step, a log, an error or a return value: a step names the repo, an
// error names the query and what GitHub said, and gh's stdout is never quoted.
//
// Plain fetch to api.github.com, not octokit: one POST per query is all @octokit/graphql would do, and the kernel's
// own code takes no package from the cell package cache (packages.ts).
import { table, type Table } from "../table.ts";
import { effect, oneLine, quiet } from "../effects.ts";

export class GithubError extends Error { override name = "GithubError"; }

export type GithubDeps = {
  /** The fetch requests go through (a fake in tests). */
  fetch: (url: string, init?: RequestInit) => Promise<Response>;
  /** The token, read once. */
  token: () => Promise<string>;
  /** https://api.github.com by default. */
  api?: string;
};

// ---- rows

export type PrState = "open" | "closed" | "merged";
/** The head commit's combined status, GitHub's StatusState lowercased; null when the commit has no checks. */
export type Rollup = "success" | "failure" | "error" | "pending" | "expected" | null;

export type PrRow = {
  number: number;
  title: string;
  state: PrState;
  draft: boolean;
  author: string;
  /** The head commit's CI rollup. */
  ci: Rollup;
  mergeable: "mergeable" | "conflicting" | "unknown";
  /** approved, changes_requested, review_required, or null when the repo needs no review. */
  review: "approved" | "changes_requested" | "review_required" | null;
  head: string;
  base: string;
  created: Date;
  updated: Date;
  url: string;
};

export type CheckRow = {
  name: string;
  /** The Actions workflow a check run belongs to, null for a commit status or a check from another app. */
  workflow: string | null;
  /** check run: queued, in_progress, completed, waiting, pending, requested; commit status: completed or pending. */
  status: string;
  /** success, failure, cancelled, skipped, neutral, timed_out, action_required, stale, startup_failure; a commit status's
   *  state (success, failure, error, pending, expected); null while a check run has not finished. */
  conclusion: string | null;
  url: string | null;
  started: Date | null;
  completed: Date | null;
};

export type PrFile = { path: string; change: string; additions: number; deletions: number };
export type CommentRow = { author: string; body: string; created: Date; url: string };
export type ReviewRow = { author: string; state: string; body: string; submitted: Date | null };

export type PrDetail = PrRow & {
  body: string;
  additions: number;
  deletions: number;
  /** The head commit's sha. */
  sha: string | null;
  files: Table<PrFile>;
  checks: Table<CheckRow>;
  comments: Table<CommentRow>;
  reviews: Table<ReviewRow>;
};

export type IssueRow = {
  number: number;
  title: string;
  state: "open" | "closed";
  author: string;
  labels: string[];
  assignees: string[];
  comments: number;
  created: Date;
  updated: Date;
  url: string;
};

// ---- GraphQL shapes (what the queries below ask for)

type Login = { login: string } | null;
type GqlRollup = { state: string; contexts?: { nodes: (GqlContext | null)[] } } | null;
type GqlContext =
  | { __typename: "CheckRun"; name: string; status: string; conclusion: string | null; detailsUrl: string | null; startedAt: string | null; completedAt: string | null; checkSuite: { workflowRun: { workflow: { name: string } } | null } | null }
  | { __typename: "StatusContext"; context: string; state: string; targetUrl: string | null; createdAt: string };
type GqlPr = {
  number: number; title: string; state: string; isDraft: boolean; url: string; author: Login; headRefName: string; baseRefName: string;
  createdAt: string; updatedAt: string; mergeable: string; reviewDecision: string | null;
  commits: { nodes: ({ commit: { oid: string; statusCheckRollup: GqlRollup } } | null)[] };
};
type GqlPrDetail = GqlPr & {
  body: string; additions: number; deletions: number;
  files: { nodes: ({ path: string; changeType: string; additions: number; deletions: number } | null)[] } | null;
  comments: { nodes: ({ author: Login; body: string; createdAt: string; url: string } | null)[] };
  reviews: { nodes: ({ author: Login; state: string; body: string; submittedAt: string | null } | null)[] } | null;
};
type GqlIssue = {
  number: number; title: string; state: string; url: string; author: Login; createdAt: string; updatedAt: string;
  labels: { nodes: ({ name: string } | null)[] } | null; assignees: { nodes: ({ login: string } | null)[] }; comments: { totalCount: number };
};

const PR_FIELDS = `number title state isDraft url author { login } headRefName baseRefName createdAt updatedAt mergeable reviewDecision`;
const CONTEXTS = `contexts(first: 100) { nodes { __typename
  ... on CheckRun { name status conclusion detailsUrl startedAt completedAt checkSuite { workflowRun { workflow { name } } } }
  ... on StatusContext { context state targetUrl createdAt } } }`;

// ---- mapping

const lower = <T extends string>(s: string) => s.toLowerCase() as T;
const who = (a: Login) => a?.login ?? "ghost";
const date = (s: string | null) => (s === null ? null : new Date(s));
const nodes = <T>(c: { nodes: (T | null)[] } | null | undefined): T[] => (c?.nodes ?? []).filter((n): n is T => n !== null);

function prRow(p: GqlPr): PrRow {
  const head = nodes(p.commits)[0]?.commit;
  return {
    number: p.number, title: p.title, state: lower<PrState>(p.state), draft: p.isDraft, author: who(p.author),
    ci: head?.statusCheckRollup ? lower<Exclude<Rollup, null>>(head.statusCheckRollup.state) : null,
    mergeable: lower<PrRow["mergeable"]>(p.mergeable),
    review: p.reviewDecision === null ? null : lower<Exclude<PrRow["review"], null>>(p.reviewDecision),
    head: p.headRefName, base: p.baseRefName, created: new Date(p.createdAt), updated: new Date(p.updatedAt), url: p.url,
  };
}

function checkRows(rollup: GqlRollup): Table<CheckRow> {
  return table(nodes(rollup?.contexts).map((c): CheckRow => c.__typename === "CheckRun"
    ? { name: c.name, workflow: c.checkSuite?.workflowRun?.workflow.name ?? null, status: c.status.toLowerCase(), conclusion: c.conclusion?.toLowerCase() ?? null, url: c.detailsUrl, started: date(c.startedAt), completed: date(c.completedAt) }
    : { name: c.context, workflow: null, status: c.state === "PENDING" ? "pending" : "completed", conclusion: c.state.toLowerCase(), url: c.targetUrl, started: date(c.createdAt), completed: null }));
}

function issueRow(i: GqlIssue): IssueRow {
  return {
    number: i.number, title: i.title, state: lower<IssueRow["state"]>(i.state), author: who(i.author),
    labels: nodes(i.labels).map((l) => l.name), assignees: nodes(i.assignees).map((a) => a.login), comments: i.comments.totalCount,
    created: new Date(i.createdAt), updated: new Date(i.updatedAt), url: i.url,
  };
}

/** "owner/name", split; anything else is an error naming the argument. */
export function repoOf(call: string, repo: string): { owner: string; name: string } {
  const m = /^([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)$/.exec(repo);
  if (!m) throw new GithubError(`github.${call}: repo ${JSON.stringify(repo)} is not "owner/name"`);
  return { owner: m[1]!, name: m[2]! };
}

function limitOf(call: string, limit: number): number {
  if (!Number.isInteger(limit) || limit < 1 || limit > 100) throw new GithubError(`github.${call}: limit ${limit} is not 1 to 100 (one query returns at most 100; page with github.graphql and a cursor for more)`);
  return limit;
}

/** A search qualifier's value, quoted when it has a space. */
const q = (v: string) => (/[\s"]/.test(v) ? `"${v.replaceAll('"', "")}"` : v);

/** The search query github.prs runs (exported for its test). */
export function prSearch(o: { repo: string; state?: PrState | "all"; author?: string }): string {
  const state = o.state ?? "open";
  return [`repo:${o.repo}`, "is:pr", state === "open" ? "is:open" : state === "closed" ? "is:closed is:unmerged" : state === "merged" ? "is:merged" : "",
    o.author ? `author:${q(o.author)}` : "", "sort:updated-desc"].filter(Boolean).join(" ");
}

/** The search query github.issues runs (exported for its test). */
export function issueSearch(o: { repo: string; state?: "open" | "closed" | "all"; author?: string; assignee?: string; labels?: string[] }): string {
  const state = o.state ?? "open";
  return [`repo:${o.repo}`, "is:issue", state === "all" ? "" : `is:${state}`, o.author ? `author:${q(o.author)}` : "", o.assignee ? `assignee:${q(o.assignee)}` : "",
    ...(o.labels ?? []).map((l) => `label:${q(l)}`), "sort:updated-desc"].filter(Boolean).join(" ");
}

/** The operation's name (`query Prs(...)`), else the query's first words, for a step and an error. */
const opName = (query: string) => /\b(?:query|mutation)\s+([A-Za-z_]\w*)/.exec(query)?.[1] ?? oneLine(query, 60);

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** The token from `gh auth token`, as a kernel reads it: once, quietly (no step), its output never quoted. */
export function ghToken(): () => Promise<string> {
  let got: Promise<string> | undefined;
  return () => (got ??= quiet(async () => {
    const gh = Bun.which("gh");
    if (!gh) throw new GithubError("github: `gh` is not on PATH; install the GitHub CLI and run `gh auth login`");
    const proc = Bun.spawn([gh, "auth", "token"], { stdout: "pipe", stderr: "pipe", stdin: "ignore", timeout: 30_000 });
    const [code, out, err] = await Promise.all([proc.exited, new Response(proc.stdout).text(), new Response(proc.stderr).text()]);
    const token = out.trim();
    if (code !== 0 || token === "") throw new GithubError(`github: \`gh auth token\` ${code !== 0 ? `exited ${code}` : "printed nothing"}${err.trim() ? `: ${oneLine(err, 200)}` : ""}; run \`gh auth login\``);
    return token;
  }).catch((e) => { got = undefined; throw e; }));
}

export function makeGithub(deps: GithubDeps) {
  const api = (deps.api ?? "https://api.github.com").replace(/\/+$/, "");
  const headers = async () => ({
    authorization: `bearer ${await deps.token()}`, "user-agent": "blaze-harness", accept: "application/vnd.github+json", "x-github-api-version": "2022-11-28",
  });

  /** One GraphQL request; GitHub's errors as one error naming the operation. No step: the caller makes it. */
  async function post<T>(query: string, vars: Record<string, unknown>): Promise<T> {
    const r = await deps.fetch(`${api}/graphql`, { method: "POST", headers: { ...(await headers()), "content-type": "application/json" }, body: JSON.stringify({ query, variables: vars }) });
    const text = await r.text();
    let body: { data?: T | null; errors?: { message: string; path?: (string | number)[]; type?: string }[]; message?: string };
    try { body = JSON.parse(text) as typeof body; }
    catch { throw new GithubError(`github.graphql ${opName(query)}: GitHub answered ${r.status} with no JSON: ${oneLine(text, 200)}`); }
    if (!r.ok) throw new GithubError(`github.graphql ${opName(query)}: GitHub answered ${r.status}: ${body.message ?? body.errors?.map((e) => e.message).join("; ") ?? oneLine(text, 200)}`);
    if (body.errors?.length) {
      throw new GithubError(`github.graphql ${opName(query)}: ${body.errors.map((e) => `${e.path?.length ? `${e.path.join(".")}: ` : ""}${e.message}`).join("; ")}`);
    }
    if (body.data == null) throw new GithubError(`github.graphql ${opName(query)}: GitHub answered ${r.status} with no data`);
    return body.data;
  }

  /** Any GraphQL query or mutation; `T` is the shape of its `data`. */
  function graphql<T = unknown>(query: string, vars: Record<string, unknown> = {}): Promise<T> {
    const op = opName(query);
    return effect("net", "Querying GitHub", "Queried GitHub", op, () => post<T>(query, vars), () => ({ detail: op }));
  }

  /** A REST call, for what GraphQL lacks (Actions job logs, workflow dispatch). JSON comes back parsed; anything else
   *  as text. A non-2xx answer is an error naming the method, path and GitHub's message. */
  function rest(path: string, o: { method?: string; body?: unknown } = {}): Promise<unknown> {
    const method = (o.method ?? "GET").toUpperCase();
    const p = path.replace(/^\/+/, "");
    return effect("net", `Calling GitHub ${method} ${p}`, `Called GitHub ${method} ${p}`, p, async () => {
      const r = await deps.fetch(`${api}/${p}`, {
        method, headers: { ...(await headers()), ...(o.body === undefined ? {} : { "content-type": "application/json" }) },
        body: o.body === undefined ? undefined : JSON.stringify(o.body),
      });
      const text = await r.text();
      const json = (r.headers.get("content-type") ?? "").includes("json") && text !== "";
      if (!r.ok) {
        const message = json ? ((JSON.parse(text) as { message?: string }).message ?? "") : oneLine(text, 200);
        throw new GithubError(`github.rest ${method} ${p}: GitHub answered ${r.status}${message ? `: ${message}` : ""}`);
      }
      return json ? (JSON.parse(text) as unknown) : text;
    });
  }

  /** Pull requests, newest update first: state open (default), closed (unmerged), merged or all, optionally by author. */
  async function prs(o:{ repo: string; state?: PrState | "all"; author?: string; limit?: number }): Promise<Table<PrRow>> {
    repoOf("prs", o.repo);
    const first = limitOf("prs", o.limit ?? 30);
    const query = `query Prs($q: String!, $first: Int!) { search(query: $q, type: ISSUE, first: $first) { nodes { ... on PullRequest {
      ${PR_FIELDS} commits(last: 1) { nodes { commit { oid statusCheckRollup { state } } } } } } } }`;
    return effect("net", `Querying GitHub for PRs in ${o.repo}`, "Queried GitHub", o.repo, async () => {
      const d = await post<{ search: { nodes: (GqlPr | Record<string, never> | null)[] } }>(query, { q: prSearch(o), first });
      return table(d.search.nodes.filter((n): n is GqlPr => n !== null && "number" in n).map(prRow));
    }, (rows) => ({ detail: `${plural(rows.length, "PR")} in ${o.repo}`, show: rows }));
  }

  /** One pull request with its files, the head commit's checks, its comments and reviews (first 100 of each). */
  async function pr(o:{ repo: string; n: number }): Promise<PrDetail> {
    const { owner, name } = repoOf("pr", o.repo);
    const query = `query Pr($owner: String!, $name: String!, $n: Int!) { repository(owner: $owner, name: $name) { pullRequest(number: $n) {
      ${PR_FIELDS} body additions deletions
      files(first: 100) { nodes { path changeType additions deletions } }
      comments(first: 100) { nodes { author { login } body createdAt url } }
      reviews(first: 100) { nodes { author { login } state body submittedAt } }
      commits(last: 1) { nodes { commit { oid statusCheckRollup { state ${CONTEXTS} } } } } } } }`;
    return effect("net", `Querying GitHub for ${o.repo}#${o.n}`, "Queried GitHub", `${o.repo}#${o.n}`, async (): Promise<PrDetail> => {
      const d = await post<{ repository: { pullRequest: GqlPrDetail | null } | null }>(query, { owner, name, n: o.n });
      const p = d.repository?.pullRequest;
      if (!p) throw new GithubError(`github.pr: ${o.repo}#${o.n} is not a pull request${d.repository ? "" : " (no such repository)"}`);
      const head = nodes(p.commits)[0]?.commit ?? null;
      return {
        ...prRow(p), body: p.body, additions: p.additions, deletions: p.deletions, sha: head?.oid ?? null,
        files: table(nodes(p.files).map((f) => ({ path: f.path, change: f.changeType.toLowerCase(), additions: f.additions, deletions: f.deletions }))),
        checks: checkRows(head?.statusCheckRollup ?? null),
        comments: table(nodes(p.comments).map((c) => ({ author: who(c.author), body: c.body, created: new Date(c.createdAt), url: c.url }))),
        reviews: table(nodes(p.reviews).map((r) => ({ author: who(r.author), state: r.state.toLowerCase(), body: r.body, submitted: date(r.submittedAt) }))),
      };
    }, (p) => ({ detail: `${o.repo}#${o.n}: ${plural(p.files.length, "file")}, ${plural(p.checks.length, "check")}, ${plural(p.comments.length, "comment")}` }));
  }

  /** Issues, newest update first: state open (default), closed or all, filtered by author, assignee and labels. */
  async function issues(o:{ repo: string; state?: "open" | "closed" | "all"; author?: string; assignee?: string; labels?: string[]; limit?: number }): Promise<Table<IssueRow>> {
    repoOf("issues", o.repo);
    const first = limitOf("issues", o.limit ?? 30);
    const query = `query Issues($q: String!, $first: Int!) { search(query: $q, type: ISSUE, first: $first) { nodes { ... on Issue {
      number title state url author { login } createdAt updatedAt labels(first: 20) { nodes { name } } assignees(first: 10) { nodes { login } } comments { totalCount } } } } }`;
    return effect("net", `Querying GitHub for issues in ${o.repo}`, "Queried GitHub", o.repo, async () => {
      const d = await post<{ search: { nodes: (GqlIssue | Record<string, never> | null)[] } }>(query, { q: issueSearch(o), first });
      return table(d.search.nodes.filter((n): n is GqlIssue => n !== null && "number" in n).map(issueRow));
    }, (rows) => ({ detail: `${plural(rows.length, "issue")} in ${o.repo}`, show: rows }));
  }

  /** The checks and commit statuses on a ref (a branch, tag or sha) of a repo. */
  async function checks(o:{ repo: string; ref: string }): Promise<Table<CheckRow>> {
    const { owner, name } = repoOf("checks", o.repo);
    const query = `query Checks($owner: String!, $name: String!, $ref: String!) { repository(owner: $owner, name: $name) { object(expression: $ref) {
      ... on Commit { oid statusCheckRollup { state ${CONTEXTS} } } } } }`;
    return effect("net", `Querying GitHub for checks on ${o.repo}@${o.ref}`, "Queried GitHub", `${o.repo}@${o.ref}`, async () => {
      const d = await post<{ repository: { object: { oid?: string; statusCheckRollup?: GqlRollup } | null } | null }>(query, { owner, name, ref: o.ref });
      const obj = d.repository?.object;
      if (!obj) throw new GithubError(`github.checks: ${o.ref} names no commit in ${o.repo}${d.repository ? "" : " (no such repository)"}`);
      if (obj.oid === undefined) throw new GithubError(`github.checks: ${o.ref} in ${o.repo} is not a commit`);
      return checkRows(obj.statusCheckRollup ?? null);
    }, (rows) => {
      const failing = rows.filter((c) => c.conclusion !== null && !["success", "skipped", "neutral"].includes(c.conclusion) && c.status === "completed").length;
      const running = rows.filter((c) => c.status !== "completed").length;
      return { detail: `${plural(rows.length, "check")} on ${o.repo}@${o.ref}${failing ? `, ${failing} failing` : ""}${running ? `, ${running} running` : ""}`, show: rows };
    });
  }

  return { graphql, rest, prs, pr, issues, checks };
}

/** github.* as cells get it: gh's token, the kernel's fetch. */
export const github = makeGithub({ fetch: (url, init) => fetch(url, init), token: ghToken() });
