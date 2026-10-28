# site/renderer: the site's renderer in Rust

A sans-IO core takes a world (the site's content as Flecs entities, comments as entities under their blocks) and a
viewer, and returns one page's HTML. It is the site's only renderer: `gen/build.ts` renders the static pages with it
and the worker (`worker/pages.ts`) renders the discussable pages with it per request, both through the same wasm
module. TS is the host: the build's bake step, routing, auth, D1, the cache and headers.

```
content/*.flecs ──▶ site-snapshot.wasm (Flecs inside) ──▶ snapshot (.gen/site.cbor) ──▶ site.wasm ──▶ HTML
   gen/build.ts bake: highlight.js; bench charts ──┘                                       ▲
                                              comments (CBOR) + viewer (CBOR) ─────────────┘
```

| crate | what it is |
|---|---|
| `core` (`site-render`) | The renderer. `world.rs` is the entity store it reads, and `snapshot.rs` loads it from a flecs-snapshot DAG-CBOR document. `render.rs`, `inline.rs` and `comments.rs` are the page, inline text and thread rendering; `course.rs` lays out the course path as a graph (bands, rows, columns, gutter tracks) (ported from the former `render/*.ts`, byte for byte). `js.rs` holds the JS semantics the output depends on: whitespace, `String(n)`, `encodeURIComponent`, `String.replace`, UTF-16 slicing and `Date`. The core does no I/O, reads no clock and has no globals. Feature `scripts` runs the content in Flecs (`scripts.rs`). |
| `wasm` (`site-wasm`) | The core behind a C ABI: `site.wasm`, the worker's module and the build's renderer. No Flecs. |
| `snapshot` (`site-snapshot`) | The content scripts and the baked HTML to a snapshot, behind a C ABI: `site-snapshot.wasm`, Flecs's C compiled in (flecs-c for wasm32-unknown-unknown). Build time only. |
| `serve` (`site-serve`) | The whole site as a wasi:http component (wstd): pages, comments from a `CommentStore`, and the build's assets. A demo. |

flecs-snapshot (and flecs-c, through it) comes from indexable-inc/flecs-opinionated, pinned to a commit in `Cargo.toml`.

## Building, and where the modules come from

```
uv run --script renderer/build.py     # from site/: target/site/site.wasm and target/site/site-snapshot.wasm
uv run --script renderer/fetch.py     # or fetch them: CI's `site-renderer` artifact, or --scp HOST:DIR
```

`build.py` builds each module with the `wasm` profile (opt-level z, LTO, one codegen unit, panic=abort), in its own
cargo run (in one, the features would unify and `site.wasm` would get Flecs), then runs `wasm-opt -Oz` when it is on
PATH. Flecs's C needs a clang with the wasm32 backend: set `CC_wasm32_unknown_unknown` and `AR_wasm32_unknown_unknown`
when the default `clang` is a wrapper that adds host flags (nix: `llvmPackages.clang-unwrapped` and
`llvmPackages.bintools-unwrapped`). Cargo builds run in an ix VM or in CI, not on the Mac.

The modules are platform-independent build outputs, not checked in. CI (`.github/workflows/site.yaml`) builds them on
every run and uploads them as `site-renderer`; `fetch.py` downloads the latest green run's (this branch, else main), so
`bun run build` and `wrangler dev` work without a Rust toolchain. An artifact is built from its run's commit: after
changing `renderer/`, build in the lane's ix VM and copy `renderer/target/site` out with
`ix shell <lane> -- tar -C <dir>/renderer/target/site -c . | tar -x -C renderer/target/site`.

Measured 2026-10-25 (Rust 1.99.0, binaryen 133 on dev-aws-14, flecs-opinionated 959ccd4; `bun gen/build.ts` on the
Mac; the worker bundle row is from the earlier 736e38c build):

| measure | value |
|---|---|
| `site.wasm` | 463 KB from cargo, 391 KB after wasm-opt, 167 KB gzip -9 |
| `site-snapshot.wasm` | 1.73 MB from cargo, 1.36 MB after wasm-opt, 489 KB gzip -9 |
| snapshot (`site.cbor`) | 493 KB |
| worker bundle | 764 KiB, 231 KiB gzipped: `index.js` 57 KB, `site.wasm` 358 KB, `site.cbor` 367 KB |
| `bun gen/build.ts` | 140–170 ms for all 10 pages, scripts and snapshot included |

Earlier, on dev-aws-14 under node 22 with the opt-level s module: compile 1.1–1.4 ms, instantiate 0.1 ms, `site_load`
10–11 ms (decodes the snapshot and indexes the pages), first render 0.45 ms, a warm render of the charter with 5
comments, signed in, 0.40 ms.

## Wasm ABI

A C ABI rather than wasm-bindgen: the modules have no imports and need no generated JS glue, so they load in a
Worker, Bun, wasmtime or a browser with the ~100 lines of `worker/site-wasm.ts`. All state sits behind a handle, with
no globals.

`site.wasm`:

```
h = site_new()
p = site_alloc(n); write the snapshot at p; site_load(h, p, n); site_free(p, n)   0, or 1 with the error as output
site_pages(h)                                                                     output: JSON [{url, file, doc, discussable}]
site_discussable(h)                                                               output: JSON {doc: {title, blocks: {id: text}}}
site_render(h, url, url_len, comments, comments_len, discussion, discussion_len)  0 with the HTML, or 1 with the error
site_spoken(h, url, url_len)                                                      0 with JSON [{id, block, text, authored}], or 1
output: site_output_len(h) bytes at site_output_ptr(h);  site_drop(h)
```

- `comments` is plain CBOR (any encoder, any key order): a list of
  `{id, block, quote, body, created_at, user_id, login, name, avatar_url}`. These are the worker's D1 rows as they
  come back.
- `discussion` is `{viewer: null | {id, login, admin}, sign_in, error: null | {block, message}}`.
- An empty input means no comments, or anonymous.

The core spawns the comments under their blocks for one render and then rolls them back: under the block if that
block renders a thread, otherwise under the page. A Rust panic is a trap (panic=abort); the worker then drops the
instance and makes a new one on the next request.

`site-snapshot.wasm`: `snap_new`, `snap_alloc`, `snap_free`, `snap_build(h, input, len)` (0 with the snapshot, or 1 with
the error), `snap_output_ptr`, `snap_output_len`, `snap_drop`. The input is JSON
`{scripts: [{name, code}], bakes: {<entity path>: {highlighted?, baked?, audio?}}}`, the scripts in run order (`schema.flecs`,
`refs.flecs`, then the rest by path, then the ones the build generated, such as the benchmark charts).

## Highlighting and glyphs stay in TS, embedded as-is; charts are content

`gen/build.ts` pre-renders highlighted code (highlight.js) and each lesson's and update's glyph (gen/glyphs.ts), by entity path,
and the snapshot step sets that HTML on the entities as `Highlighted {html}` (Code, Command) and `Baked {html}` (a
lesson or update page). The core embeds it unchanged, so the core has no highlighter.

Charts are elements like any other (`RowChart`, `LineChart`, `Charts`, `Picker` in `content/schema.flecs`), drawn by
`core/src/chart.rs` as HTML rows and inline SVG with a table of their numbers. The benchmark page's are written by the build from the
history files (`gen/bench/`) as a Flecs script that runs after the content.

## wasi:http host

```
SITE_SNAPSHOT=site/.gen/site.cbor SITE_ASSETS=site/dist cargo build --release -p site-serve --target wasm32-wasip2
wasmtime serve -S cli target/wasm32-wasip2/release/site-serve.wasm
```

- It serves pages from the embedded snapshot and every non-HTML file of dist/ as an asset. Unknown paths get `/404`
  with status 404.
- Comments come from `MemoryStore`, which is seeded from `fixtures/comments.json`.
- It is a demo: the viewer comes from an `x-demo-user` header (`octo`, `mallory`, `admin`). `wasmtime serve` creates
  a fresh instance per request, so the store has no writes.
- A page request takes about 5.6 ms, including instantiation and the snapshot load.
- The component was 3.9 MB, of which 2.7 MB was `demo.avif` (since removed with the home page's demo).

## Parity with the former TS renderer

Before `site/render/*.ts` was removed (2026-10-25), the differential test (`difftest.py` and `oracle/oracle.ts`, removed
with it) compared every page byte for byte: the static pages and, for the discussable pages,
the cases in `fixtures/cases.json` (anonymous, user, admin, sign-in off, a form error on a block and on `_page`): all
identical, and `discussable.json` identical as data. The worker was also compared end to end under `wrangler dev`,
old against new, on the same D1 rows: every response identical but for the time of a comment posted during the run.
The fixtures stay: `serve` seeds its store from `fixtures/comments.json`.

## Flecs, and what is left

The core never touches Flecs; only `core/src/scripts.rs` does, behind the `scripts` feature, through flecs-snapshot's
`World`: `run` for the content scripts and for `bake.flecs` (which only defines `Highlighted`, `Baked` and `Audio`),
`set` for the baked HTML (checked against each component's layout), and `roots` for the root list (sorted bytewise).
The `world_*` C exports are behind flecs-snapshot's `ffi` feature, which the renderer leaves off, so they are not in
`site-snapshot.wasm`. `snapshot.rs` reads documents strictly, as flecs-snapshot's own reader does: an unknown key in
the document, an entity record or a component value is refused, and so are Bytes and Blob values.

Order, as flecs-snapshot documents it: pairs are sorted by encoding (the core restores the targets of one relationship
in document order) and children without `OrderedChildren` by name. RFC lists sort by `Number` (`World::rfcs`).

Left to do:

- Put the discussable pages' thread blocks into the snapshot, so `site_load` needn't render them (part of its 10 ms).
- Pre-initialise the wasi:http component with Wizer, so a request skips the snapshot load.
- A bug of the former TS renderer, faithfully reproduced: a comment hung under a Table block would render as an empty `<tr>`. No current
  thread block is a table, so it can't happen today.
