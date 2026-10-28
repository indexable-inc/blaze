# blaze site

https://ix.dev/blaze/ (where it is served and how: [Hosting](#hosting-ixdevblaze)). Every page is a [Flecs](https://github.com/SanderMertens/flecs) script. A Rust renderer
(`renderer/`, compiled to wasm) turns them into HTML, at build time for the static pages and per request in a
Cloudflare Worker for the pages with comments; TS is the host around it: the build, the worker's routing, auth, D1 and
headers, and the browser scripts.

## Architecture: Rust core, TS host

```
content/*.flecs ─┬─ gen/tree.ts (flecs-doc.wasm) ── bake: highlight.js; generated: bench charts ──┐
                 └──────────────────────────────────────────── site-snapshot.wasm ◀────────────────┘
                                                            │ snapshot (.gen/site.cbor)
                                                            ▼
                 gen/build.ts ── site.wasm ──▶ dist/*.html (every page, no comments)
                 worker/      ── site.wasm ──▶ discussable pages (comments from D1, the viewer), per request
```

- `renderer/`: the renderer, in Rust (`renderer/README.md`). `core` turns a snapshot of the site, a page's comments
  and a viewer into one page's HTML; it is sans-IO. Two wasm modules with a small C ABI and no imports:
  `site.wasm` (the renderer) and `site-snapshot.wasm` (the content scripts run in Flecs, compiled in, to the snapshot;
  build time only). Both are platform-independent build outputs, never checked in.
- `gen/build.ts`: the build, under Bun, with no native code. It runs the scripts under `flecs/flecs-doc.wasm` to bake
  what the renderer leaves to TS (highlighted code) and to write the scripts the content asks for (the benchmark
  charts, `gen/bench/`), gets the snapshot from `site-snapshot.wasm`,
  renders every page with `site.wasm` into `dist/`, bundles `client/*.ts`, concatenates `styles/` and copies `static/`.
  For the worker it writes `.gen/`: `site.wasm`, `site.cbor` (the snapshot), `pages.json`, `discussable.json` and
  `build.json` (a hash of the module, the snapshot and the client scripts).
- `worker/`: the Worker (below). `worker/site-wasm.ts` is the host side of both modules (about 100 lines, shared with
  the build); `worker/renderer.ts` loads `site.wasm` and the snapshot once per isolate.
- `content/*.flecs`: the pages. `schema.flecs` defines the element types (`P`, `Li`, `Code`, `Section`, `Issue`, ...),
  the relationships (`Status`, `Cites`, `DependsOn`) and the page prefabs (`Page`, `Doc`, `Rfc`); `refs.flecs` the
  references pages cite. An entity's path is its URL: `site.rfcs.capabilities` is `/rfcs/capabilities`.
- `flecs/`: Flecs compiled to wasm (`flecs-doc.wasm`, checked in; `python3 flecs/build.py <flecs checkout>` rebuilds it
  with zig). The build's bake step and the editor's `.flecs` type checking (`content/tsconfig.json`) read the scripts
  with it.
- `content/nav.flecs`: the sidebar's groups (`Group`, `Icon`) and outside links. A page joins one with `Nav` and
  `(InGroup, nav.x)`; `renderer/core/src/side.rs` draws the sidebar, crumb, pager and outline from them, in the ix
  docs' layout.
- `client/`: browser scripts (the sidebar's folds, drawer, search over `dist/search.json` and outline in `side.ts`;
  comment enhancements, the signup form, the charts' tooltips, copy buttons). `nav.ts` is the client router: a click on a link to
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

## Commands

```
bun install
uv run --script renderer/fetch.py   # once, and again when renderer/ changes
bun run build    # dist/ and .gen/ in about 150 ms
bun run check    # tsc over worker/, client/, gen/ and content/
bun run dev      # wrangler dev: rebuilds on change; apply the D1 migrations with --local first
bun run deploy   # build + wrangler deploy
```

## Hosting (ix.dev/blaze)

The site is the Cloudflare Worker `blaze` (account Indexable). `.github/workflows/site.yaml` deploys it on a push
to main once the repo secrets `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` are set (the token needs Workers
Scripts, Workers Routes and D1 edit on the account and the ix.dev zone); until then it only builds, and the site is
deployed by hand from an up-to-date main, logged in with `wrangler login`:

```
cd site && uv run --script renderer/fetch.py && SITE_AUDIO_REMOTE=1 bunx wrangler deploy
```

(`wrangler deploy` runs the build itself, so `SITE_AUDIO_REMOTE=1` has to be in its environment, or the pages lose
their narration players.) The move to ix.dev/blaze was deployed that way on 2026-10-27 (version
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
renderer prefixes every link, asset and form action with the base and writes the canonical and `og:` tags from the
origin; the build writes `.gen/site.json` (for the worker) and `dist/sitemap.xml`, and copies the repo's `install.sh`
to `dist/install.sh` (https://ix.dev/blaze/install.sh, and https://ix.dev/blaze/ itself for a client that doesn't
ask for HTML, so `curl -fsSL https://ix.dev/blaze | sh` installs). To move the site again, change `Served` and the routes.

## RFCs and comments

An RFC is a file in `content/rfcs/`: an instance of the `Rfc` prefab under `site.rfcs`. Every block in a `Doc` (the
charter, RFCs) has a name; its path under the page is the anchor comments attach to, so renaming a block orphans its
comments (they still show, with their quote, in the page's discussion).

## Narration (`audio/`)

Each page can be listened to. What is said is not the page read out: the core derives a spoken version (code and
reference lists skipped, tables read row by row, figures by their alt text), and a block can say something else with
`Say: {"..."}`, or nothing with `Silent`. `audio/generate.py` voices it on a Mac (Fish Audio Drama 3 on MLX); the build
adds the player only to pages whose audio matches their current text. [audio/README.md](audio/README.md) has the
details, and the steps for serving the files from R2.

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
every request reaches the worker: `base.ts` `enter` redirects other hosts, strips the base (`/blaze`) from the path and
the worker routes the site path (`/api/*`, `/auth/*`, `/updates`, `/audio/*`, discussable pages); anything else goes
to `env.ASSETS`, which serves `dist/` (built by `bun gen/build.ts`, laid out without the base). A missing page gets
`dist/404.html`. Since every response comes from the worker, a `_headers` file would not apply: `http.ts`
`assetHeaders` sets the CSP, nosniff and referrer policy on assets.

- `base.ts`: where the site is served (`Served` on `site` in `content/site.flecs`, written by the build to
  `.gen/site.json`), `outer` (a site path as the browser sees it) and the redirects from the old hosts.

- `auth.ts`: GitHub sign-in and the session cookie.
- `pages.ts`: discussable pages, rendered with their comments (below).
- `renderer.ts`, `site-wasm.ts`, `cbor.ts`: the Rust renderer's module, its C ABI and the CBOR its inputs go in.
  `modules.d.ts` types the module imports: wrangler bundles `.gen/site.wasm` as a compiled module and `.gen/site.cbor`
  as bytes (the `Data` rule in `wrangler.jsonc`).
- `comments.ts`: `/api/comments`.
- `presence.ts`: `/api/presence`, a WebSocket to the `Presence` Durable Object (one, named `site`, hibernating), which
  knows who is on which page. Signed-in readers are their GitHub login; anonymous ones get a key that is an HMAC of
  their IP and user agent under a random key the object makes each UTC day and deletes the next, so a name lasts a day
  and no IP is kept. `client/lib/presence.ts` (loaded by `side.ts`, so on every page) draws the pane's top line:
  Chat (opens the feed's history and input, as T does), the faces on this page (click for everyone and where), and the
  reader's GitHub login with Sign out, or Sign in with GitHub; and faces on the sidebar rows others are reading.
  On the page itself (`client/lib/cursors.ts`) the others there show as live pointers (desktop only; "Hide cursors"
  in the who-is-here list turns them off) and as faces on a rail at the right edge where each one's pointer is in the
  article (click a face to go there). A pointer is sent as what it is on (`client/lib/anchor.ts`): in the
  article a block and the character in it, elsewhere a region (the sidebar, the top line, "On this page") and
  the link or button in it, with fractions of that thing's box; so it lands on the same thing whatever
  each reader's window and layout, and a reader without that thing on screen (sidebar shut) doesn't draw
  it; the object passes them only to the other tabs on the same page, rate
  limited per person, and never stores them.
  Everything passing shows in one overlay feed at the bottom left of the page, like Minecraft's chat
  (`client/lib/feed.ts`): chat messages, who joined ("Merry Tiger joined on Charter") and who left (someone off the
  who-is-here list for 8 s, so a reload or a reconnect is no leave; neither in the 15 s after a connect or a deploy),
  mentions, a refused message and the site's own word (a new version, the translator's download). A line fades
  after 10 s (not while the pointer is on the feed); a chat line opens the feed's history at it, a join or a leave
  the page they were on. Mute (in the hint under the input) keeps chat, joins and leaves out of it, never what is for
  the reader.
  T (or Enter, for Vimium users; on a phone the feed's button; anywhere the top line's Chat) opens the feed's history
  over its one input: as it is typed the others on the page see the line by the reader's pointer (`say-live`, same
  budget, never stored), as a bubble under the pointer or out of the face on the rail; Enter sends it to the chat
  (signed in) and the bubble stays where it was a while; an anonymous reader's is only that bubble, not kept, and the
  hint under the input offers Sign in with GitHub. Esc closes it and keeps the draft.
  The feed is the chat's only view (`client/lib/chat.ts`); the same object holds the chat: its last 500 messages in
  the object's SQLite, readable by anyone, written by signed-in readers (5 per 15 s, 120 an hour), taken back by their
  author or a `COMMENT_ADMINS` id. A tab gets the last 80 on connect and keeps every one that comes after (up to
  500), all in the feed's history, which scrolls back through them. Authors and admins edit messages (↑ in the empty
  input edits your last one, or the pencil on the line in the open history; Enter saves, Esc gives the draft back;
  `edited_at`, the same rules as a new message) and take them back (the ×). The input completes `@login` from people
  online and recent authors; a message that mentions the reader is marked and shows in the feed even muted. Who is
  writing shows over the input ("ana is typing"). Someone else's message in another language is translated on the
  reader's computer where it can be (`client/lib/translate.ts`, after the reader chatted or pressed a line's
  Translate), and says so. The chat is flat: the object still has the threads (a `parent` column, `broadcast` for a
  reply also sent to the chat) and reactions (a `reactions` table) that an earlier side panel used; the feed shows a
  reply only when it was also sent to the chat or mentions the reader, and no reactions. Comment writes
  (`comments.ts`) tell the object too, and open pages refresh their comment threads.
  On every connect the object also sends the live build (`.gen/build.json`), and a deploy's code sends it to every
  open socket the first time it wakes (a deploy keeps hibernated sockets open), so each open tab hears of a new
  version as soon as anyone on the site moves (`client/lib/update.ts`). New scripts or styles
  reload the tab; new content alone is fetched again and swapped in place by the router. Either waits until the
  reader isn't typing, playing the narration or dragging (a hidden tab goes at once), with an "update ready" line in
  the feed and a button meanwhile, then says "Site updated". Drafts (the feed's input and whether it was open, and
  comments), an edit in progress, scroll positions and the narration's place go through sessionStorage and come back; a reload that still gets the old
  assets doesn't retry for a minute.
- `updates.ts`: the `/updates` form.
- `http.ts`: cookies, origin check, `next` validation, tokens. `env.ts`: the `Env` type.

The worker imports `.gen/` (written by the build), so build before `wrangler dev` or `deploy` (both run the build
themselves):

```
bun gen/build.ts
bunx wrangler d1 migrations apply ra-jit-updates --local
bunx wrangler dev --var GITHUB_CLIENT_ID:<dev app id>     # secrets for dev go in .dev.vars (gitignored)
bunx tsc --noEmit -p tsconfig.json                         # type-checks worker/ only
```

Secrets (`wrangler secret put`, or `.dev.vars` locally; `.dev.vars*` is gitignored): `GITHUB_CLIENT_SECRET`,
`TURNSTILE_SECRET_KEY`.

The cookies are `__Secure-` cookies, so they are `Secure`. `wrangler dev` serves plain http://localhost by default:
Safari drops every `Secure` cookie on http://localhost, so sign-in can lose its cookie there. For local sign-in run
`bunx wrangler dev --local-protocol https` (and register `https://localhost:8787/blaze/auth/github/callback` on the dev
OAuth App). Anything without sign-in works over http.

## Comments (`/api/comments`, `/auth/*`)

Discussable pages are rendered by the worker (`worker/pages.ts`): it loads the doc's comments in one D1 query and
hands the rows, CBOR-encoded, to `site.wasm` with the viewer (signed-in user and admin, or null). The renderer hangs
each comment under its block (or under the page, for `_page` and for blocks gone from the page) for that one render,
and escapes comment text as it escapes everything else. If the module traps or returns an error, the worker logs the
detail (`render failed <url> ...`) and answers a plain 500 `Server error.`: it never sends part of a page, and the
cache is not written. After a trap the next request makes a new instance. The page works without JS: each block's count links to its
thread (shown by CSS `:target`), posting and deleting are plain form posts, and signed out each thread links to
sign-in. `client/comments.ts` takes the markup over: threads toggle in place, posts and deletes go through the JSON API
and then swap the thread for the worker's fresh rendering of it, times show relative.

Caching: an anonymous request is served from the Cache API under
`<origin><path>?v=<version>&b=<build hash>&w=<worker version>&s=<sign-in on>`, where the version is `doc_versions`
(`migrations/0003_doc_versions.sql`), bumped by triggers in the same statement that writes, deletes or erases a comment
(and when a commenter's profile changes), so an entry is never stale and nothing is purged. The worker version is the
`CF_VERSION` binding's (`version_metadata`), new on every deploy, so a change of code or vars moves every key;
`wrangler dev` gives a random id per session. The version is memoised for 2 s per isolate. A request with a valid session is rendered for that reader and
sent `private, no-store`; it never reads or writes the cache, and the only code that writes it renders with no viewer.
Anonymous pages go to the browser as `no-cache, Vary: cookie`. Worker-served HTML carries a CSP (`script-src 'self'`;
`/updates` also allows Turnstile); the `x-render` header says how it was served (`hit`, `miss`, `anonymous`, `private`).

Pages tagged `Discussable` take comments, per block or on the page as a whole (block `_page`). The build lists them
(from the renderer, `site_discussable`) in `.gen/discussable.json` (doc -> title and each block's id and first 280 chars of text); the worker accepts only
docs and blocks listed there, and stores the first 140 chars of the block's text as the comment's `quote` (none for
`_page`). Tables `users`, `sessions`, `comments` (`migrations/0002_comments.sql`).

Sign-in is a GitHub OAuth App with no scopes: `GET /auth/github?next=/path` (the state is in the `__Secure-blaze_gh_oauth`
cookie, 10 minutes), callback `/auth/github/callback`, `POST /auth/logout` (form field `next`). The access token is
used once to read `/user`, then revoked (`DELETE /applications/{client_id}/token`, in the background). The
`__Secure-blaze_session` cookie (30 days) holds a random token; D1 keeps only its SHA-256. Both cookies are HttpOnly,
Secure, SameSite=Lax, Path=/blaze, no Domain, so they go only to ix.dev/blaze/ and never to the rest of ix.dev. (A
`__Host-` cookie must have Path=/, which would send the session to every ix.dev service.) `next` and the redirects
are site paths; the worker adds the base (`outer`).
`next` is parsed as a URL against a placeholder origin: a different origin (`//host`, `/\host`, ...), a control
character, or more than 1024 chars gives `/`; otherwise the parsed path, query and fragment (percent-encoded ASCII).

- `GET /api/comments?doc=<doc>`: `{comments, me, signIn, admin}`; 404 for a doc that takes no comments.
- `POST /api/comments` (JSON `{doc, block, body}`): 201 `{comment}`. Needs a session (401), same Origin (403), JSON
  (415), at most 64 KiB (413, checked from Content-Length before reading and again while reading). Body 1 to 4000
  chars after trimming. Limit: 10 per 10 minutes and 100 per day per user (429), counted from `comments`, deleted
  ones included, inside the INSERT itself (so parallel posts cannot slip past it).
- `POST /api/comments` as a form (`doc`, `block`, `body`; no JS): the same checks; 303 to the page `#c-<id>`, or to
  `?c-error=<code>&c-block=<block>#block-<block>`, which shows a fixed message in that block's form. A cross-site
  post, an unknown doc or block, or a body over 64 KiB get a plain-text error.
- `POST /api/comments/<id>/delete` (form, no JS): the same as DELETE; 303 to the page `#block-<block>`. Without a
  session, a plain 401 before the comment is looked up (so it does not tell which page the comment is on).
- `DELETE /api/comments/<id>`: the author or a user id in `COMMENT_ADMINS`. 204. Deleting erases the comment's text:
  the row stays (so ids and threads stay stable) with `deleted_at` set, `body` emptied and `quote` cleared. D1 Time Travel keeps earlier states of the
  database for up to 30 days, so the old text is gone from the site at once and from backups after that. Pages
  rendered before the delete can stay in edge caches under their old keys for up to 24 h; they are served again only
  if `doc_versions` goes backwards (a D1 Time Travel restore, say), so deploy after a restore, which moves every key.

The OAuth App is `blaze` in the indexable-inc org (client id in `wrangler.jsonc`). Setup from scratch: create a GitHub OAuth App with the callback `https://ix.dev/blaze/auth/github/callback`, put its client id in
`GITHUB_CLIENT_ID` (`wrangler.jsonc` `vars`; empty turns sign-in off) and its secret with
`wrangler secret put GITHUB_CLIENT_SECRET`. `COMMENT_ADMINS` is a comma-separated list of numeric GitHub user ids
(`curl https://api.github.com/users/<login>` shows `id`), compared with the signed-in user's id; logins are not used,
since a renamed login can be registered by someone else. Apply `0002_comments.sql` and `0003_doc_versions.sql` with `wrangler d1 migrations apply ra-jit-updates --remote`.

Reading comments:

```
npx wrangler d1 execute ra-jit-updates --remote --command \
  "select c.id, c.doc, c.block, u.login, c.body, c.created_at from comments c join users u on u.id = c.user_id where c.deleted_at is null order by c.id desc"
```

## Signups (`/updates`)

`worker/updates.ts`. GET serves `dist/updates.html` with the Turnstile widget (when `TURNSTILE_SITE_KEY` is set) and
the visitor's Referer filled in through HTMLRewriter. POST with `Accept: application/json` (`client/updates.ts`)
answers `{ok: true}` or `{error}`; a plain form post (no JS) gets the page back with the error shown, or the thank-you
note. Cross-site posts are refused (403).

One row per email in `signups` (email lowercased; a repeat signup updates `company` and `why`, keeps the first-touch
metadata). Metadata: `request.cf` country, region, city, asn, asOrganization, timezone; user-agent; referer (the page
before `/updates`); accept-language; `src` and `utm_*` from the `/updates` URL (the README button uses `?src=readme`).
No IPs are stored. Rate limit: 3 signups per email and 60 overall per minute (429), logged in `signup_attempts` by
the same statement that checks the limit. Only posts with a valid email that passed Turnstile are logged, so failed
checks cannot fill the global cap. A form post over 64 KiB is refused (413).

Turnstile: set `TURNSTILE_SITE_KEY` in `wrangler.jsonc` `vars` and the secret with `wrangler secret put`. With no
secret, verification is skipped; with no site key, the widget is not shown (dev).

### One-time setup

```
npx wrangler d1 create ra-jit-updates      # paste the printed database_id into wrangler.jsonc (TODO there)
npx wrangler d1 migrations apply ra-jit-updates --remote
npx wrangler secret put TURNSTILE_SECRET_KEY
bun gen/build.ts && npx wrangler deploy
```

Local dev: `npx wrangler d1 migrations apply ra-jit-updates --local`.

### Reading signups

```
npx wrangler d1 execute ra-jit-updates --remote --command \
  "select email, company, why, src, country, city, as_organization, created_at from signups order by created_at desc"
npx wrangler d1 execute ra-jit-updates --remote --command "select count(*) from signups"
```

Add `--json` for machine-readable output.
