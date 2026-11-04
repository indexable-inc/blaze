# harness

The harness daemon and CLI (`harness`) and its native macOS app (`harness-native`, Metal and AppKit
on den's flecs stack, libghostty for terminal state). Imported with its history from the standalone
`indexable-inc/harness` repository on 2026-11-03.

harness is its own cargo workspace with its own `Cargo.lock`, excluded from blaze's workspace
(blaze's root `Cargo.toml`): blaze's gate does not build or check it, and blaze's astlog rules,
`harness/rustcsites.py` and `tools/docs.py` skip this directory. Build it with cargo, on the Mac:

    cd crates/harness && cargo build --release    # target/release/harness, harness-native, harness-video

## Map

| Path | What |
| --- | --- |
| `Cargo.toml`, `Cargo.lock` | the workspace: every `harness*` directory below is a member |
| `.cargo/config.toml` | `ZIG=scripts/zig` for libghostty-vt-sys's build |
| `harness/` | the daemon and CLI |
| `harness-vt/` | terminal state through libghostty-vt (built by zig) |
| `harness-ghostty-config/` | reads the user's Ghostty config |
| `harness-native/` | the macOS app: `scene/*.flecs` (tokens, views, keys), fonts, icons, Metal shaders |
| `harness-native-render/` | the renderer the app and harness-video share: Metal (Slang shaders), text, vector marks, the paint pass |
| `harness-video/` | videos from flecs scenes on the same stack, a timeline of `Cue`s as data (blaze's launch video: `scene/launch.flecs`) |
| `harness-native-*` | the app's layers: model, flecs glue, ECS, UI layout/motion/draw/input/text and their `-api` crates, prims (`harness-native-ui-prims/scene/`), scroll physics |
| `vendor/` | the hyperion-* crates over flecs (flecs itself is the indexable-inc/flecs fork, pinned by rev in `vendor/hyperion-flecs-c/Cargo.toml`; `vendor/SOURCE.md`) |
| `scripts/zig` | fetches the pinned zig through Nix |
| `scripts/harness` | builds and runs the `harness` binary from this checkout |
| `mcp/` | the Bun/TypeScript kernel the daemon runs (`mcp/src/mcp.ts`; `bun install` in `mcp/`), with its smoke scripts in `mcp/scripts/` |

The daemon finds the TypeScript kernel at `$HARNESS_ROOT/src/mcp.ts`, by default `crates/harness/mcp`
in the checkout it was built from. UI work: the `flecs-ui` skill (`.agents/skills/flecs-ui`).
