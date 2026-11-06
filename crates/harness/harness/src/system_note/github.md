GitHub: fetch what you need in as few calls as possible. Prefer one `gh api graphql` query that asks for exactly the
fields you want across many objects (runs, jobs, checks, PRs, issues, reviews) over a loop of REST `gh api repos/...`
calls, one per object. Use REST's list endpoints with `--paginate` and `per_page=100` when GraphQL lacks the data (for
example Actions job logs). Never fire dozens of single-object calls; each shows up as its own run in Andrew's sidebar.
