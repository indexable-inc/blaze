# harness

The harness daemon and CLI (`harness`) and its native macOS app (`harness-native`, Metal and AppKit
on den's flecs stack, libghostty for terminal state). Imported with its history from the standalone
`indexable-inc/harness` repository on 2026-11-03.

harness is its own cargo workspace with its own `Cargo.lock`, excluded from blaze's workspace
(blaze's root `Cargo.toml`): blaze's gate does not build or check it, and blaze's astlog rules,
`harness/rustcsites.py` and `tools/docs.py` skip this directory. Build it with blaze, on the Mac:

    cd crates/harness && blaze build --release    # harness, harness-native, harness-video

Only blaze can: harness-native-render takes Kip's shaders from blaze's device build of
harness-native-shaders (`artifact = "gpu"`, docs/future/gpu/shaders.md), a dependency cargo
refuses to read.

## Map

| Path | What |
| --- | --- |
| `Cargo.toml`, `Cargo.lock` | the workspace: every `harness*` directory below is a member |
| `.cargo/config.toml` | `ZIG=scripts/zig` for libghostty-vt-sys's build |
| `harness/` | the daemon and CLI |
| `harness-proto/` | the one schema the daemon, every client and the chat logs speak: deterministic CBOR, frames, messages, chat events, the log format and its migration |
| `harness-vt/` | terminal state through libghostty-vt (built by zig) |
| `harness-ghostty-config/` | reads the user's Ghostty config |
| `harness-native/` | the macOS app, a library with a thin binary: `scene/*.flecs` (tokens, views, keys), fonts, icons, Metal shaders; `harness_native::scene::world()` is the scene every host loads (the app, its tests, harness-video) |
| `harness-native-render/` | the renderer the app and harness-video share: Metal (Slang shaders), text, vector marks, the paint pass |
| `harness-native/` | the macOS app: `scene/*.flecs` (tokens, views, keys), fonts, icons, Metal shaders |
| `harness-native-render/` | the renderer the app and harness-video share: Metal (shaders in Slang and in Rust), text, vector marks, the paint pass |
| `harness-native-blaze/` | blaze's fire wordmark, live (drop in, catch, burn to embers), and its synthesised soundtrack (from indexable-inc/anim) |
| `harness-video/` | videos from flecs scenes loaded into the app's own scene (its tokens, prims and `hn.*` widgets), a timeline of `Cue`s as data (blaze's launch video: `scene/launch.flecs`) |
| `harness-native-*` | the app's layers: model, flecs glue, ECS, UI layout/motion/draw/input/text and their `-api` crates, prims (`harness-native-ui-prims/scene/`), scroll physics |
| `vendor/` | the hyperion-* crates over flecs (flecs itself is the indexable-inc/flecs fork, pinned by rev in `vendor/hyperion-flecs-c/Cargo.toml`; `vendor/SOURCE.md`) |
| `scripts/zig` | fetches the pinned zig through Nix |
| `scripts/harness` | builds (with blaze) and runs the `harness` binary from this checkout |
| `mcp/` | the Bun/TypeScript kernel the daemon runs (`mcp/src/mcp.ts`; `bun install` in `mcp/`), with its smoke scripts in `mcp/scripts/` |
| `mcp/skills/` | harness's skills: one topic's instructions each (`<name>.md` with `name` and `description` front matter); the agent's note carries only their index, `await skill('name')` in ts loads one (`harness/src/skills.rs`, `mcp/src/skills.ts`) |
| `harness/src/system_note/` | the always-on part of every agent's note, one file per topic (`harness/src/profiles.rs`) |

## Skills

An agent's system note carries what every turn needs (`harness/src/system_note/`) and ends with an index of skills,
one line each (name and description), generated from the skill files at each launch. The full text of a skill loads
on demand: `await skill('github')` in ts returns it and the person sees a quiet "Read skill github" step; `skills()`
lists them. Skills come from `mcp/skills/` (harness's own), `$HARNESS_HOME/skills` (the user's) and the session's git
root's `.agents/skills` and `.claude/skills` (the project's), a later source overriding an earlier one by name. A
skill is `<name>.md` or `<name>/SKILL.md`, the Claude Code and Codex format; a broken one stops the launch with an
error naming its file. The daemon passes the list to the kernel as `HARNESS_SKILLS`, so the index and what `skill()`
serves are one list, for claude, codex and cursor alike. A topic only some tasks need goes in a skill, not the note.

The daemon finds the TypeScript kernel at `$HARNESS_ROOT/src/mcp.ts`, by default `crates/harness/mcp`
in the checkout it was built from. UI work: the `flecs-ui` skill (`.agents/skills/flecs-ui`).

## Protocol and chat logs

One versioned schema, `harness-proto` (its `SCHEMA`, now 1), covers the daemon socket and every chat
log. There is no JSON path. Its encoding is deterministic CBOR (ciborium plus a canonicalizing pass:
map keys sorted by encoded bytes, shortest integers and floats, definite lengths, integral floats
written as integers), so Rust and the kernel write the same bytes for the same value.

- Socket frames are `[type u8][len u32 BE][payload]`. `T_CBOR` (6) carries a message: `Up` from a
  client, `Down` from the daemon. `T_EVENT` (7) carries a chat event. `T_DATA` (2) carries raw
  terminal bytes. Any other type is refused by name.
- Every connection opens with the client's `Up::Hello { schema }`. The daemon answers with
  `Down::Hello { schema, pid }`, or with an error naming both versions, and then closes.
- A chat's log is `objects/<id>/chat`: the magic `harness-chat/1\n`, then the `T_EVENT` frames the
  socket carried, appended. Attaching replays those frames as they are, so the history is the same
  bytes as the live feed. A torn last frame (a crash mid-append) is cut off at open and reported as
  an error event.
- An NDJSON log from before schema 1 is converted once, at daemon start: write `chat.migrating`,
  fsync it, rename it over `chat`, fsync the directory, then set `meta.chat_log` in SQLite. The steps
  come from `harness_proto::migrate::plan`, a sans-IO plan. Its test crashes at every step and checks
  that every event survives exactly once, and planted bugs (a missing sync, marking first, trusting
  the mark) must each fail it.
- `harness log <id|s<n>|path>` prints a log as one JSON event per line. Bad records and a torn tail
  go to stderr.
- The kernel mirrors the types once, in `mcp/src/cbor.ts` and `mcp/src/wire.ts`.
  `mcp/src/wire.test.ts` round-trips the crate's `fixtures/` byte for byte. The kernel's `sql`
  decodes the logs into the `chat`, `chats` and `messages` views.
