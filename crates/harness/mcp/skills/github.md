---
name: github
description: GitHub through github.* in ts (graphql, prs, pr, issues, checks, rest), typed rows and one step per call, never gh api in a shell; read before any GitHub work.
---
GitHub is `github.*` in a ts cell. It uses gh's token (`gh auth token`, read once, never shown), returns data (rows as
Tables), and each call is one step the person sees ("Queried GitHub · 12 PRs in indexable-inc/ix"). Never run
`gh api`, `gh pr list/view/checks/status` or `gh issue list/view` in a shell: `$` and `sh` refuse them with the
call to use. gh's writes (`gh pr create`, `gh pr merge`, `gh pr comment`) still run in the shell.

The helpers, each one GraphQL query:

- `await github.prs({ repo: 'owner/name', state?: 'open'|'closed'|'merged'|'all', author?, limit? })` (limit 1 to
  100, default 30, newest update first): `Table<{ number, title, state, draft, author, ci, mergeable, review, head,
  base, created, updated, url }>`. `ci` is the head commit's rollup (success, failure, pending, error, or null with
  no checks); `review` is approved, changes_requested, review_required or null.
- `await github.pr({ repo, n })`: that row plus `body`, `sha`, `additions`, `deletions` and Tables `files`
  (path, change, additions, deletions), `checks`, `comments` (author, body, created, url) and `reviews` (author,
  state, body, submitted); the first 100 of each.
- `await github.issues({ repo, state?: 'open'|'closed'|'all', author?, assignee?, labels?: string[], limit? })`:
  `Table<{ number, title, state, author, labels, assignees, comments, created, updated, url }>`.
- `await github.checks({ repo, ref })` (a branch, tag or sha): `Table<{ name, workflow, status, conclusion, url,
  started, completed }>`; failing ones are `checks.filter(c => c.status === 'completed' && c.conclusion === 'failure')`.

Anything else is one query: `await github.graphql<{ repository: { ... } }>(query, vars)` returns `data`, typed by the
parameter you give; GitHub's errors throw naming the operation (name your queries: `query Runs($owner: String!) {...}`).
Ask for exactly the fields you need across many objects in one query, never a loop of single-object calls: each call
is a step in Andrew's sidebar. Use `await github.rest('repos/o/r/actions/jobs/123/logs')` (`{ method, body }` for
writes) only where GraphQL lacks the data, such as Actions job logs; list endpoints take `?per_page=100`.
