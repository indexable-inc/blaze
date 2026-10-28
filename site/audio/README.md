# Narration

Every page (the home page, the charter, the RFCs) can be listened to. What is said is a spoken version of the page,
not the page read out: code is described or skipped, reference lists are skipped, tables are read row by row, and any
block can say something written for the ear.

```
content/*.flecs ──site.wasm (spoken.rs)──▶ .gen/spoken.json ──audio/generate.py (Mac)──▶ .audio/files/<sha>.webm
                                                                                   └──▶ audio/manifests/<doc>.json
gen/build.ts: manifest matches the page's lines? ──▶ Audio {manifest} baked in ──▶ player + data-say on blocks
```

## What is said: data in the content, derived in the core

`renderer/core/src/spoken.rs` turns a page into ordered lines `(id, block, text)`, one per block, deterministically.
`site.wasm` exports it as `site_spoken`; the build writes every page's lines to `.gen/spoken.json`.

| block | said |
|---|---|
| h1 | the title, or the page's `Intro` |
| Section | "Next: title." (the first one plainly), then its body |
| P, Li, Lede, Callout | the text with markup stripped; a code span as words (`mem::forget` is "mem forget", `/Fs` "slash Fs", `Vec<T>` "Vec of T"); citations like `([[aeneas]])` dropped; "s" and "ms" after a number as "seconds" and "milliseconds" |
| Issue | "Open question: title." |
| Table | row by row against the header |
| Img | "On screen: alt." |
| Code, Command | nothing, unless it has a `Say` |
| References, BenchHistory, RowChart, LineChart, Picker | nothing (a chart's numbers are in its table, on the page) |
| RfcIndex | "RFC n, title." for each |

Authored lines (in the `.flecs` files, schema in `content/schema.flecs`):

- `Say: {"..."}` replaces what a block says. On a container (Section, Issue, Li, Ul, Ol) it replaces the whole
  subtree: its children are not read. Text in square brackets is a direction for the voice (`[warm]`), and paired
  tags like `<emphasis>must</emphasis>` mark how the words between them are said; the player shows neither.
- `Silent` drops the block and everything under it.
- `Intro: {"..."}` and `Outro: {"..."}` on the page entity open and close the narration.

Derived text never carries `[`, `]`, `<`, `>`, `{`, `}` or `|`, so it cannot be taken for a voice direction.

## Generating (on a Mac with Apple Silicon)

```
bun gen/build.ts                                         # writes .gen/spoken.json
uv run --script audio/generate.py /charter /rfcs/linear-types
uv run --script audio/generate.py --check /charter       # Whisper transcript vs. text, per line
```

One warm `speak.py --serve` (Fish Audio Drama 3, q8 experts, the voice repo's `tools/tts`) voices every line not in
the cache; it returns a WAV and word timings (Qwen3-ForcedAligner). ffmpeg normalises it to -18 LUFS and encodes Opus.

- Cache key: sha256 of the spoken text (after the lexicon, below), the voice reference (ref.wav and ref.txt), the model revision (Drama 3 and drama3-mlx
  commits), the engine (`stream.py`), the quantisation, the seed and the encoding. The seed comes from the text, so a
  rerun asks for the same take. A take whose length is far from what its words need is redone with the next seed, up
  to three times.
- `.audio/files/<sha>.webm` is named by the hash of its bytes, so a file never changes under a URL. `.audio/` is
  gitignored (it can be rebuilt; tens of MB for the whole site).
- `audio/manifests/<doc>.json` is small and checked in:
  `{doc, url, hash, voice: {id, attribution}, model, duration, lines: [{id, block, src, duration, words: [[w, start, end]]}]}`.
  `hash` is the hash of the page's spoken lines when it was voiced.

## Pronunciation: `audio/lexicon.toml`

The voice reads technical words badly ("blaze" as "Rajeed", "1.99.0" as "1009.0"). Just before synthesis,
generate.py rewrites each line through the lexicon; the model gets the spoken form, the page and manifest keep theirs.

```toml
[words]                      # whole word, case-sensitive: display form = spoken form
"rustc" = "rust C"
"blaze" = "R A jit"

[[rules]]                    # regexes (Python `re`), applied after the words
name = "version"
pattern = '(?<![\w.])(\d+)\.(\d+)\.(\d+)(?![\w]|\.\d)'
spoken = '\1 point \2 point \3'
```

- A word matches where it is not next to a letter, digit or underscore, so `blaze's`, `(rustc)` and `cap-std` match,
  `rustcx` does not. The longest entry wins (`DAG-CBOR` before `CBOR`). Words and rules work within one
  whitespace-separated word. Text inside `[directions]` and the `<tag>`/`</tag>` markers themselves is never
  touched; the words between an opening and closing tag are (`<emphasis>rustc</emphasis>` becomes
  `<emphasis>rust C</emphasis>`). A spoken form may not contain `[ ] < >`.
- Add an entry only when the default reading is wrong: synthesise a short sentence with it and its candidates and run
  Whisper over them (what was done to seed the file). Capitalised and spelled forms matter: "R A" is said as letters,
  "Ra" is not.
- Word timings: the aligner times the spoken words; the manifest gets one entry per page word, from the start of its
  first spoken word to the end of its last (`blaze` spans "R A jit"). listen.ts matches manifest words to the page by
  their letters and digits, so the manifest words must be the page's.
- The cache key holds the spoken text, so editing an entry regenerates only the lines it changes.
- `--check` puts both the text and the transcript through the lexicon (the transcript in any case), spells numbers
  out, and joins runs of single letters, so "R A jit", "BLAZE" and "blaze" compare equal.

## The build

`gen/build.ts` gives a page a player only if its manifest's `hash` matches the page's current lines and every file is
in `.audio/files/` (or `SITE_AUDIO_REMOTE=1`, below). Otherwise it warns and the page has no player. So the build
never needs audio (CI has none), and an edited page loses its player until it is voiced again, rather than playing
audio that no longer matches it.

The manifest goes to `dist/audio/<doc>.<sha12>.json`, the files to `dist/audio/`, and the snapshot gets
`Audio {manifest}` on the page. The renderer then adds `<div class="listen" data-audio="...">` under the title,
`data-say="<block>"` on each spoken block, and `/js/listen.js`.

## The player (`client/listen.ts`)

Play/Pause, previous and next block, elapsed and total time, and speed (0.75x to 2x, `preservesPitch`, remembered in
localStorage). With focus in the player, Space or k plays and pauses, Left and Right skip a block. The block being read
is highlighted and, from the word timings and the CSS Custom Highlight API, the word being said; with
`prefers-reduced-motion` the word highlight is off and scrolling is instant. Each block has a "play from here" button
beside its comment button. Nothing plays until the reader presses something. The page CSP allows `media-src 'self'`
(worker/http.ts `csp`, for worker pages and static files alike).

Format: Opus in WebM, mono, 32 kb/s. Chrome, Firefox and Safari 17.4+ (macOS and iOS) play it; Opus in Ogg needs
Safari 18.4.

## Production: files in R2

The files should not be in the Worker's static assets (they change as pages change, and the asset count is limited).
The plan, none of it done (each step needs approval):

1. `wrangler r2 bucket create ra-jit-audio`.
2. In `wrangler.jsonc`: `"r2_buckets": [{ "binding": "AUDIO", "bucket_name": "ra-jit-audio" }]`. `worker/audio.ts`
   already serves `/audio/<sha>.webm` from `AUDIO` when it is bound (with Range, which Safari needs, and
   `immutable` caching), and from the assets otherwise.
3. Upload from the Mac after generating: for each file in `.audio/files/`,
   `wrangler r2 object put blaze-audio/<name> --file .audio/files/<name> --content-type audio/webm --remote`.
   Names are content hashes, so uploading is add-only and safe to repeat.
4. Commit `audio/manifests/`, and build with `SITE_AUDIO_REMOTE=1` in the `site` workflow, so manifests ship and the
   files are taken from R2.

## Licences

- Drama 3 is under the Fish Audio Community License v1.0: free for non-commercial use, commercial use only while
  annual revenue is under USD 500k. Check before blaze's site counts as commercial.
- The default voice, hifi-92, is from the Hi-Fi Multi-Speaker English TTS Dataset (Bakhturina et al., NVIDIA, 2021,
  https://www.openslr.org/109/), CC BY 4.0, audio from LibriVox. The manifest carries the attribution; credit it on
  the site when narration ships.
