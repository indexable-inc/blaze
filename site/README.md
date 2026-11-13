# blaze site

https://ix.dev/blaze/ (where it is served and how: [Hosting](#hosting-ixdevblaze)). Every page is a [Flecs](https://github.com/SanderMertens/flecs) script. A Rust renderer
(`renderer/`, compiled to wasm) turns them into HTML at build time; every page is static. TS is the host around it:
the build, a small Cloudflare Worker (redirects, the base path, `/latest.json`, the installer for curl, headers), and
the browser scripts.

## Architecture: Rust core, TS host

```
content/*.flecs ─┬─ gen/tree.ts (flecs-doc.wasm) ── bake: highlight.js; generated: bench charts ──┐
                 └──────────────────────────────────────────── site-snapshot.wasm ◀────────────────┘
                                                            │ snapshot (.gen/site.cbor)
                                                            ▼
                 gen/build.ts ── site.wasm ──▶ dist/*.html (every page)
```

- `renderer/`: the renderer, in Rust (`renderer/README.md`). `core` turns a snapshot of the site into one page's HTML; it is sans-IO. Two wasm modules with a small C ABI and no imports:
  `site.wasm` (the renderer) and `site-snapshot.wasm` (the content scripts run in Flecs, compiled in, to the snapshot;
  build time only). Both are platform-independent build outputs, never checked in.
- `gen/build.ts`: the build, under Bun, with no native code. It runs the scripts under `flecs/flecs-doc.wasm` to bake
  what the renderer leaves to TS (highlighted code) and to write the scripts the content asks for (the benchmark
  charts, `gen/bench/`), gets the snapshot from `site-snapshot.wasm`,
  renders every page with `site.wasm` into `dist/`, bundles `client/*.ts`, concatenates `styles/` and copies `static/`.
  Each build empties `dist/` and `.gen/` first. For the worker it writes `.gen/site.json` (where the site is served)
  and `.gen/build.json` (`assets`, a hash of the client scripts and styles); `.gen/site.cbor` is the snapshot, for
  `renderer/serve`. `gen/site-wasm.ts` is the host side of both modules (about 100 lines).
- `worker/`: the Worker (below).
- `content/*.flecs`: the pages. `schema.flecs` defines the element types (`P`, `Li`, `Code`, `Section`, `Issue`, ...),
  the relationships (`Status`, `Cites`, `DependsOn`) and the page prefabs (`Page`, `Rfc`); `refs.flecs` the
  references pages cite. An entity's path is its URL: `site.rfcs.capabilities` is `/rfcs/capabilities`.
- `flecs/`: Flecs compiled to wasm (`flecs-doc.wasm`, checked in; `python3 flecs/build.py <flecs checkout>` rebuilds it
  with zig). The build's bake step and the editor's `.flecs` type checking (`content/tsconfig.json`) read the scripts
  with it.
- `content/nav.flecs`: the sidebar's groups (`Group`, `Icon`) and outside links. A page joins one with `Nav` and
  `(InGroup, nav.x)`; `renderer/core/src/side.rs` draws the sidebar, crumb, pager and outline from them, in the ix
  docs' layout.
- `client/`: browser scripts (the sidebar's folds, drawer, search over `dist/search.json`, outline and the top line's
  button that shuts it in `side.ts`; the course and its player, the charts' tooltips, copy buttons). `nav.ts` is the client router: a click on a link to
  another page fetches it (fetched ahead on hover) and swaps only the page column, the head's tags and the sidebar's
  open row, without a load, as the ix docs move between pages; anything unexpected falls back to a plain load. Each
  page script sets itself up through `client/lib/page.ts` `onPage`, again after every swap and undone before the
  next.

The flecs skill (`.agents/skills/flecs/`) has the script syntax and its traps.

### The wasm modules: build outputs, fetched

CI builds both modules on every run of the `site` workflow and uploads them as the `site-renderer` artifact. The build
looks for them in `renderer/target/site/` (or `SITE_WASM_DIR`) and, when they are missing, prints how to get them. A
machine without the Rust toolchain fetches them; changing `renderer/` itself means building them where Rust builds
run (an ix VM, CI) and fetching from there:

```
uv run --script renderer/fetch.py                                 # the latest green CI run: this branch, else main
uv run --script renderer/fetch.py --scp HOST:DIR [--port 9999]    # DIR = renderer/target/site of a build.py run
uv run --script renderer/build.py                                 # with the Rust toolchain, clang and wasm-opt
```

## Code graph (`/graph`)

`graph/`: a standalone page, not a Flecs page: blaze's symbols and their relations (from `blaze index`; a hand-made
sample until then) as a live force-directed graph on WebGPU. `gen/build.ts` builds it into `dist/graph.html` and
`dist/graph/`; the worker sends it a CSP that lets it compile its layout simulation's wasm. `graph/README.md` has the
rest, `graph/schema.ts` the file format.

## Commands

```
bun install
uv run --script renderer/fetch.py   # once, and again when renderer/ changes
bun run build    # dist/ and .gen/ in about 150 ms
bun run check    # tsc over worker/, client/, gen/ and content/
bun run dev      # wrangler dev: rebuilds on change
bun run deploy   # build + wrangler deploy
```

## Hosting (ix.dev/blaze)

The site is the Cloudflare Worker `blaze` (account Indexable). `.github/workflows/site.yaml` deploys it on a push
to main once the repo secrets `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` are set (the token needs Workers
Scripts and Workers Routes edit on the account and the ix.dev zone, and Workers R2 Storage edit for the bucket
below); until then it only builds, and the site is
deployed by hand from an up-to-date main, logged in with `wrangler login`:

```
cd site && uv run --script renderer/fetch.py && bunx wrangler deploy
```

(`wrangler deploy` runs the build itself.) The worker serves `/latest.json` (docs/release.md, "latest.json") from the
R2 bucket `blaze-releases` (binding `RELEASES`), which the release workflows write with their own secret `CLOUDFLARE_R2_API_TOKEN` (an R2 API token with Object Read &
Write on `blaze-releases` alone); the bucket is made once by hand, `bunx wrangler r2 bucket create blaze-releases`,
before the first deploy that binds it. The move to ix.dev/blaze was deployed that way on 2026-10-27 (version
2f7138f1-5ae3-42cf-b417-8538770f446e); nothing was changed in the Cloudflare dashboard or API by hand.

It is reached three ways, all declared in `wrangler.jsonc` `routes`, so `wrangler deploy` creates them:

- `ix.dev/blaze*`: a zone route on ix.dev. The rest of ix.dev (its `ix.dev/*` route to the `ix-web` worker, DNS,
  the other routes) is terraform in indexable-inc/ix (`nix/terraform/cloudflare`); Cloudflare picks the most specific
  route, so this one wins over `ix.dev/*` without touching that config. It ends in `*` because a pattern without one
  does not match a URL with a query.
- `blaze.ix.dev` (the old home) and `blaze.ix.dev`: Workers custom domains (Cloudflare makes their DNS records and
  certificates). The worker answers every request on them with a 301 (308 for a non-GET, which keeps a form post) to
  the same path and query under https://ix.dev/blaze.

`/blaze` itself is a 301 to `/blaze/`. The origin and base are `Served` on `site` in `content/site.flecs`: the
renderer prefixes every link and asset with the base and writes the canonical and `og:` tags from the
origin; the build writes `.gen/site.json` (for the worker) and `dist/sitemap.xml`, and copies `site/install.sh`
to `dist/install.sh` (https://ix.dev/blaze/install.sh, and https://ix.dev/blaze/ itself for a client that doesn't
ask for HTML, so `curl -fsSL https://ix.dev/blaze | sh` installs). To move the site again, change `Served` and the routes.

## RFCs

An RFC is a file in `content/rfcs/`: an instance of the `Rfc` prefab under `site.rfcs`.

## Benchmark history (`/bench`)

Built from `docs/benchmarks/history/*.jsonl` at the repo root, one line per benchmark run, appended by CI with
`harness/benchhistory.py` (its docstring describes a line). `content/bench.flecs` names the directory with
`BenchHistory history(dir: ...)`; at build time `gen/bench/` reads it and writes a Flecs script of Sections, Pickers
and charts (`RowChart`, `LineChart`, `Charts`, schema in `content/schema.flecs`) under that entity, which runs after the
content. The renderer draws them in `renderer/core/src/chart.rs`, coloured by `--series-N` (`styles/tokens.css`), each
with its numbers in a table under it. A row chart is one row per project or edit with its values written beside it: bars
on a linear axis from zero, or dots on a shared log axis (1-2-5 ticks) when the values span orders of magnitude, with the
speedup over the baseline. A line chart is inline SVG on a linear or log axis; with one run so far it shows that run as
numbers instead of a lone dot. `client/chart.ts` adds only the line charts' hover tooltip, and the pickers (platform,
backend, project, machine) are labelled radio buttons that work without JS. A new line shows after the next deploy; the `site` workflow deploys on a change under
`docs/benchmarks/`. Lines are grouped by machine, and the charts over time keep each machine's last 60 runs.

To look at the page with other data, point `BENCH_HISTORY` at a directory of `.jsonl` files:
`BENCH_HISTORY=/tmp/history bun run build`.

## Worker (`worker/`)

`worker/index.ts` is the Worker entry (plain `export default { fetch }`). `assets.run_worker_first` is `true`, so
every request reaches the worker: `base.ts` `enter` redirects other hosts and strips the base (`/blaze`) from the
path; then `/usage` is a 301 to `/` (its content moved into the home lesson), `/` for a client that doesn't ask for
HTML is `install.sh`, `/latest.json` comes from the RELEASES bucket, and anything else goes to `env.ASSETS`, which
serves `dist/` (built by `bun gen/build.ts`, laid out without the base). A missing page gets `dist/404.html`. Since
every response comes from the worker, a `_headers` file would not apply: `http.ts` `assetHeaders` sets the CSP
(`csp.ts`), nosniff, the referrer policy and `x-blaze-assets` (the client router swaps a page in only when it was built
with the same scripts and styles, so after a deploy an open tab's next click is a load).

- `base.ts`: where the site is served (`Served` on `site` in `content/site.flecs`, written by the build to
  `.gen/site.json`), `outer` (a site path as the browser sees it) and the redirects from the old hosts.
- `http.ts`: plain responses and the asset headers. `env.ts`: the `Env` type.

The worker imports `.gen/` (written by the build), so build before `wrangler dev` or `deploy` (both run the build
themselves):

```
bun gen/build.ts
bunx wrangler dev
bunx tsc --noEmit -p tsconfig.json                         # type-checks worker/ only
```

## Retired: comments, sign-in, presence and chat

The site once had per-paragraph and per-page comments, GitHub sign-in, live presence (who is reading, their
pointers) and a chat, all removed on 2026-11-12. What they left in Cloudflare:

- The D1 database `ra-jit-updates` (database id `c8d4a72a-3d01-48d5-9af5-46ce7b0f4670`) is no longer bound. It still
  holds its tables and rows: `users`, `sessions`, `comments`, `doc_versions` (the comments) and `signups`,
  `signup_attempts` (the older `/updates` signup form). Read it with
  `npx wrangler d1 execute ra-jit-updates --remote --command "select ..."`; delete it with `wrangler d1 delete` once
  nothing in it is wanted.
- The `Presence` Durable Object class is deleted by the `v2` migration in `wrangler.jsonc` (with its storage, the chat's
  messages); keep both migration tags.
- The secret `GITHUB_CLIENT_SECRET` can be removed (`wrangler secret delete GITHUB_CLIENT_SECRET`), and the GitHub OAuth
  App `blaze` in the indexable-inc org deleted.
