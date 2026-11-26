//! Crisp Retina-native geometric vector icons drawn directly with `blaze-ui`'s GPU SDF
//! primitives (`ROUND_RECT`, `CIRCLE`, `LINE`). Zero bitmap blur, zero missing font glyphs.

use crate::theme;
use crate::view::Scene;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Folder,
    Rust,
    Lock,
    Config,
    Doc,
    Code,
    Symlink,
    File,
    Crate,
    Branch,
    Cpu,
    Memory,
    Clock,
    Host,
    Terminal,
    Error,
    Blaze,
    Grid,
    Table,
    Search,
    Stash,
    Laptop,
    Server,
    SplitV,
    SplitH,
    Plus,
    Close,
    Maximize,
    NewWindow,
}

/// Infer the best semantic icon and restrained `ix.dev/blaze` accent color for a filesystem entry.
pub fn icon_for_file(name: &str, kind: Option<&str>) -> (Icon, [f32; 4]) {
    if matches!(kind, Some("dir")) || name.ends_with('/') {
        return (Icon::Folder, theme::SERIES_BLUE);
    }
    if matches!(kind, Some("symlink")) {
        return (Icon::Symlink, theme::SERIES_BLUE);
    }
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".rs") {
        return (Icon::Rust, theme::SERIES_ORANGE);
    }
    if lower.ends_with(".lock") || lower == "cargo.lock" || lower == "uv.lock" || lower == "bun.lock" {
        return (Icon::Lock, theme::INK_3);
    }
    if lower.ends_with(".toml")
        || lower.ends_with(".json")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".ini")
    {
        return (Icon::Config, theme::SERIES_AQUA);
    }
    if lower.ends_with(".md")
        || lower.ends_with(".txt")
        || lower.starts_with("license")
        || lower.starts_with("readme")
        || lower == "agents.md"
    {
        return (Icon::Doc, theme::INK_2);
    }
    if lower.ends_with(".nix")
        || lower.ends_with(".sh")
        || lower.ends_with(".py")
        || lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".js")
        || lower.ends_with(".c")
        || lower.ends_with(".h")
        || lower.ends_with(".flecs")
        || lower.ends_with(".lean")
        || lower.ends_with(".css")
    {
        return (Icon::Code, theme::INK_2);
    }
    (Icon::File, theme::INK_3)
}

/// Infer a semantic icon for common structured record keys (e.g. `sys`, `Cargo.toml`, `help`).
pub fn icon_for_record_key(key: &str) -> Option<(Icon, [f32; 4])> {
    match key {
        "host" | "hostname" | "os" => Some((Icon::Host, theme::INK_3)),
        "arch" | "cpus" | "cpu" => Some((Icon::Cpu, theme::INK_3)),
        "mem_total" | "mem_used" | "mem" | "memory" => Some((Icon::Memory, theme::INK_3)),
        "uptime" | "modified" | "created" | "time" | "date" => Some((Icon::Clock, theme::INK_3)),
        "branch" | "git" | "fork" => Some((Icon::Branch, theme::INK_3)),
        "cwd" | "dir" | "path" | "home" => Some((Icon::Folder, theme::SERIES_BLUE)),
        "package" | "crate" | "version" => Some((Icon::Crate, theme::SERIES_ORANGE)),
        "command" | "shell" | "bin" => Some((Icon::Terminal, theme::INK_3)),
        _ => None,
    }
}

/// Draw `icon` inside the square box `[x, y, x + s, y + s]` using GPU SDF primitives.
#[allow(clippy::too_many_arguments)]
pub fn draw_icon(
    scene: &mut Scene,
    icon: Icon,
    x: f32,
    y: f32,
    s: f32,
    color: [f32; 4],
    clip_id: u32,
) {
    let cx = x + s * 0.5;
    let cy = y + s * 0.5;
    let t = (s * 0.11).clamp(1.0, 1.8);

    match icon {
        Icon::Folder => {
            // Classic sleek filled folder: top-left tab + rounded body (matches ix.dev/blaze `target/`).
            let tab_w = s * 0.44;
            let tab_h = s * 0.22;
            scene.over_round_rect(
                x + s * 0.06,
                y + s * 0.16,
                x + s * 0.06 + tab_w,
                y + s * 0.16 + tab_h + s * 0.12,
                s * 0.10,
                color,
                clip_id,
            );
            scene.over_round_rect(
                x + s * 0.06,
                y + s * 0.30,
                x + s * 0.94,
                y + s * 0.86,
                s * 0.14,
                color,
                clip_id,
            );
        }
        Icon::Rust => {
            // Geometric gear/cog matching ix.dev/blaze's `build.rs` icon: ring + 6 radial teeth.
            let r_outer = s * 0.31;
            scene.over_ring(cx, cy, r_outer, s * 0.15, color, clip_id);
            let tooth_r = s * 0.41;
            let dot_r = s * 0.095;
            for i in 0..6 {
                let angle = (i as f32) * core::f32::consts::FRAC_PI_3;
                let tx = cx + tooth_r * angle.cos();
                let ty = cy + tooth_r * angle.sin();
                scene.over_circle(tx, ty, dot_r, color, clip_id);
            }
        }
        Icon::Lock => {
            // Padlock: upper shackle ring + solid rounded lower body.
            scene.over_ring(cx, y + s * 0.36, s * 0.22, t, color, clip_id);
            scene.over_round_rect(
                x + s * 0.18,
                y + s * 0.44,
                x + s * 0.82,
                y + s * 0.88,
                s * 0.12,
                color,
                clip_id,
            );
        }
        Icon::Config => {
            // Minimal 2-track sliders icon for `.toml` / `.json` / `.yaml`.
            let y1 = y + s * 0.32;
            let y2 = y + s * 0.68;
            scene.line(x + s * 0.10, y1, x + s * 0.90, y1, t, color, clip_id);
            scene.line(x + s * 0.10, y2, x + s * 0.90, y2, t, color, clip_id);
            scene.over_circle(x + s * 0.35, y1, s * 0.13, color, clip_id);
            scene.over_circle(x + s * 0.66, y2, s * 0.13, color, clip_id);
        }
        Icon::Doc => {
            // Clean document frame with 2 subtle text lines inside.
            scene.round_rect(
                x + s * 0.16,
                y + s * 0.08,
                x + s * 0.84,
                y + s * 0.92,
                s * 0.12,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            scene.line(x + s * 0.30, y + s * 0.38, x + s * 0.70, y + s * 0.38, t, color, clip_id);
            scene.line(x + s * 0.30, y + s * 0.60, x + s * 0.58, y + s * 0.60, t, color, clip_id);
        }
        Icon::Code => {
            // Clean `< >` angled code chevrons.
            scene.line(x + s * 0.36, y + s * 0.26, x + s * 0.12, cy, t, color, clip_id);
            scene.line(x + s * 0.12, cy, x + s * 0.36, y + s * 0.74, t, color, clip_id);
            scene.line(x + s * 0.64, y + s * 0.26, x + s * 0.88, cy, t, color, clip_id);
            scene.line(x + s * 0.88, cy, x + s * 0.64, y + s * 0.74, t, color, clip_id);
        }
        Icon::Symlink => {
            // Diagonal arrow out of a subtle box.
            scene.line(x + s * 0.22, y + s * 0.78, x + s * 0.78, y + s * 0.22, t, color, clip_id);
            scene.line(x + s * 0.44, y + s * 0.22, x + s * 0.78, y + s * 0.22, t, color, clip_id);
            scene.line(x + s * 0.78, y + s * 0.22, x + s * 0.78, y + s * 0.56, t, color, clip_id);
        }
        Icon::File => {
            // Minimal single-stroke document rectangle.
            scene.round_rect(
                x + s * 0.18,
                y + s * 0.10,
                x + s * 0.82,
                y + s * 0.90,
                s * 0.12,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
        }
        Icon::Crate => {
            // Isometric crate / package box outline with top divider.
            scene.round_rect(
                x + s * 0.12,
                y + s * 0.14,
                x + s * 0.88,
                y + s * 0.86,
                s * 0.14,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            scene.line(x + s * 0.14, y + s * 0.40, x + s * 0.86, y + s * 0.40, t, color, clip_id);
            scene.line(cx, y + s * 0.40, cx, y + s * 0.84, t, color, clip_id);
        }
        Icon::Branch => {
            // Crisp git branch glyph: left vertical stem + 2 circles + angled fork node.
            let lx = x + s * 0.28;
            let rx = x + s * 0.74;
            let ty = y + s * 0.22;
            let by = y + s * 0.78;
            let r = s * 0.13;
            scene.line(lx, ty + r, lx, by - r, t, color, clip_id);
            scene.line(lx, by - r * 1.4, rx, y + s * 0.36 + r, t, color, clip_id);
            scene.over_circle(lx, ty, r, color, clip_id);
            scene.over_circle(lx, by, r, color, clip_id);
            scene.over_circle(rx, y + s * 0.32, r, color, clip_id);
        }
        Icon::Cpu => {
            // Square chip die with tiny corner pins.
            scene.round_rect(
                x + s * 0.22,
                y + s * 0.22,
                x + s * 0.78,
                y + s * 0.78,
                s * 0.12,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            scene.over_round_rect(
                x + s * 0.40,
                y + s * 0.40,
                x + s * 0.60,
                y + s * 0.60,
                s * 0.05,
                color,
                clip_id,
            );
            for p in [0.36f32, 0.64] {
                scene.line(x + s * p, y + s * 0.08, x + s * p, y + s * 0.22, t, color, clip_id);
                scene.line(x + s * p, y + s * 0.78, x + s * p, y + s * 0.92, t, color, clip_id);
                scene.line(x + s * 0.08, y + s * p, x + s * 0.22, y + s * p, t, color, clip_id);
                scene.line(x + s * 0.78, y + s * p, x + s * 0.92, y + s * p, t, color, clip_id);
            }
        }
        Icon::Memory => {
            // Horizontal memory bar with 3 vertical banks.
            scene.round_rect(
                x + s * 0.10,
                y + s * 0.26,
                x + s * 0.90,
                y + s * 0.74,
                s * 0.12,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            for bx in [0.32f32, 0.50, 0.68] {
                scene.line(x + s * bx, y + s * 0.38, x + s * bx, y + s * 0.62, t * 1.1, color, clip_id);
            }
        }
        Icon::Clock => {
            // Minimal clock ring + 2 hands.
            scene.over_ring(cx, cy, s * 0.39, t, color, clip_id);
            scene.line(cx, cy, cx, y + s * 0.26, t, color, clip_id);
            scene.line(cx, cy, x + s * 0.70, cy + s * 0.06, t, color, clip_id);
        }
        Icon::Host => {
            // Minimal monitor outline + stand line.
            scene.round_rect(
                x + s * 0.10,
                y + s * 0.14,
                x + s * 0.90,
                y + s * 0.68,
                s * 0.12,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            scene.line(x + s * 0.34, y + s * 0.84, x + s * 0.66, y + s * 0.84, t, color, clip_id);
            scene.line(cx, y + s * 0.68, cx, y + s * 0.84, t, color, clip_id);
        }
        Icon::Terminal => {
            // Rounded mini terminal frame with `>` + `_`.
            scene.round_rect(
                x + s * 0.10,
                y + s * 0.14,
                x + s * 0.90,
                y + s * 0.86,
                s * 0.14,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            scene.line(x + s * 0.26, y + s * 0.38, x + s * 0.44, cy, t, color, clip_id);
            scene.line(x + s * 0.44, cy, x + s * 0.26, y + s * 0.62, t, color, clip_id);
            scene.line(x + s * 0.52, y + s * 0.64, x + s * 0.74, y + s * 0.64, t, color, clip_id);
        }
        Icon::Error => {
            // Clean circle + cross in `--danger`.
            scene.over_ring(cx, cy, s * 0.40, t, color, clip_id);
            let d = s * 0.17;
            scene.line(cx - d, cy - d, cx + d, cy + d, t, color, clip_id);
            scene.line(cx + d, cy - d, cx - d, cy + d, t, color, clip_id);
        }
        Icon::Blaze => {
            // Minimal blaze ember mark.
            scene.over_circle(cx, cy, s * 0.28, color, clip_id);
        }
        Icon::Grid => {
            // Crisp 2×2 rounded squares icon (switch to compact grid view).
            let g = s * 0.14;
            let b = s * 0.34;
            let r = s * 0.08;
            let x1 = x + s * 0.10;
            let x2 = x1 + b + g;
            let y1 = y + s * 0.10;
            let y2 = y1 + b + g;
            for (rx, ry) in [(x1, y1), (x2, y1), (x1, y2), (x2, y2)] {
                scene.round_rect(rx, ry, rx + b, ry + b, r, [0.0; 4], t, color, clip_id);
            }
        }
        Icon::Table => {
            // Crisp minimal table frame + header rule + vertical divider (switch to table view).
            scene.round_rect(
                x + s * 0.10,
                y + s * 0.14,
                x + s * 0.90,
                y + s * 0.86,
                s * 0.12,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            scene.line(x + s * 0.12, y + s * 0.38, x + s * 0.88, y + s * 0.38, t, color, clip_id);
            scene.line(x + s * 0.44, y + s * 0.38, x + s * 0.44, y + s * 0.84, t, color, clip_id);
        }
        Icon::Search => {
            // Crisp magnifying glass for Atuin-style history search palette.
            let r = s * 0.29;
            let mx = x + s * 0.43;
            let my = y + s * 0.43;
            scene.over_ring(mx, my, r, t, color, clip_id);
            scene.line(mx + r * 0.72, my + r * 0.72, x + s * 0.88, y + s * 0.88, t * 1.15, color, clip_id);
        }
        Icon::Stash => {
            // Crisp stacked layers icon representing git stash entries (replaces ANSI `[$N]`).
            let lx0 = x + s * 0.14;
            let lx1 = x + s * 0.86;
            scene.line(lx0, y + s * 0.28, lx1, y + s * 0.28, t * 1.05, color, clip_id);
            scene.line(lx0 + s * 0.06, y + s * 0.52, lx1 - s * 0.06, y + s * 0.52, t * 1.05, color, clip_id);
            scene.line(lx0 + s * 0.12, y + s * 0.76, lx1 - s * 0.12, y + s * 0.76, t * 1.05, color, clip_id);
        }
        Icon::Laptop => {
            // Crisp minimal MacBook silhouette: rounded screen frame + wider base ledge.
            scene.round_rect(
                x + s * 0.16,
                y + s * 0.18,
                x + s * 0.84,
                y + s * 0.70,
                s * 0.10,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            scene.line(x + s * 0.06, y + s * 0.80, x + s * 0.94, y + s * 0.80, t * 1.2, color, clip_id);
        }
        Icon::Server => {
            // Crisp stacked rack-server blades with status dots.
            scene.round_rect(
                x + s * 0.12,
                y + s * 0.14,
                x + s * 0.88,
                y + s * 0.46,
                s * 0.09,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            scene.round_rect(
                x + s * 0.12,
                y + s * 0.54,
                x + s * 0.88,
                y + s * 0.86,
                s * 0.09,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            scene.over_circle(x + s * 0.28, y + s * 0.30, s * 0.065, color, clip_id);
            scene.over_circle(x + s * 0.28, y + s * 0.70, s * 0.065, color, clip_id);
        }
        Icon::SplitV => {
            // Side-by-side vertical split icon (two columns inside a rounded frame).
            scene.round_rect(
                x + s * 0.10,
                y + s * 0.14,
                x + s * 0.90,
                y + s * 0.86,
                s * 0.14,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            scene.line(cx, y + s * 0.16, cx, y + s * 0.84, t, color, clip_id);
        }
        Icon::SplitH => {
            // Stacked horizontal split icon (two rows inside a rounded frame).
            scene.round_rect(
                x + s * 0.10,
                y + s * 0.14,
                x + s * 0.90,
                y + s * 0.86,
                s * 0.14,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            scene.line(x + s * 0.12, cy, x + s * 0.88, cy, t, color, clip_id);
        }
        Icon::Plus => {
            scene.line(cx, y + s * 0.20, cx, y + s * 0.80, t * 1.05, color, clip_id);
            scene.line(x + s * 0.20, cy, x + s * 0.80, cy, t * 1.05, color, clip_id);
        }
        Icon::Close => {
            let p0 = s * 0.24;
            let p1 = s * 0.76;
            scene.line(x + p0, y + p0, x + p1, y + p1, t * 1.05, color, clip_id);
            scene.line(x + p1, y + p0, x + p0, y + p1, t * 1.05, color, clip_id);
        }
        Icon::Maximize => {
            // Crisp diagonal expand corners for pane zoom (`Cmd+Shift+Enter`).
            let p0 = s * 0.18;
            let p1 = s * 0.82;
            let arm = s * 0.24;
            scene.line(x + p0, y + p0, x + p0 + arm, y + p0, t, color, clip_id);
            scene.line(x + p0, y + p0, x + p0, y + p0 + arm, t, color, clip_id);
            scene.line(x + p1, y + p1, x + p1 - arm, y + p1, t, color, clip_id);
            scene.line(x + p1, y + p1, x + p1, y + p1 - arm, t, color, clip_id);
        }
        Icon::NewWindow => {
            // Crisp macOS window frame with subtle top bar + plus sign (`Cmd+N`).
            scene.round_rect(
                x + s * 0.10,
                y + s * 0.14,
                x + s * 0.90,
                y + s * 0.86,
                s * 0.14,
                [0.0; 4],
                t,
                color,
                clip_id,
            );
            scene.line(x + s * 0.12, y + s * 0.36, x + s * 0.88, y + s * 0.36, t * 0.9, color, clip_id);
            scene.line(cx, y + s * 0.48, cx, y + s * 0.74, t * 0.95, color, clip_id);
            scene.line(cx - s * 0.13, y + s * 0.61, cx + s * 0.13, y + s * 0.61, t * 0.95, color, clip_id);
        }
    }
}
