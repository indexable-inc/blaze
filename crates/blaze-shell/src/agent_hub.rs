//! Unified multi-server interactive `agent` fleet hub (combining `harness` + `blaze-shell`):
//! discovers live local agents (`claude`, `codex`, `blaze-agent`, spawned `agent "<prompt>"`
//! sessions) with real per-PID `cwd`, CPU%, RAM, and uptime, plus remote SSH hosts from
//! `blaze-ssh`, and renders a resizable 2-pane Metal overview!

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use blaze_own::Alloc;

use crate::blocks::BlockStore;
use crate::icons::{Icon, draw_icon};
use crate::layout::{TOP_BAR_H, layout_top_bar};
use crate::prompt::{PromptInfo, abbreviate_home};
use crate::theme;
use crate::view::{CellMetrics, HitAction, HitRect, Scene, truncate_chars, truncate_middle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    /// Actively burning CPU or interactive in-shell running session (vivid emerald).
    Running,
    /// Live local process currently idle / waiting for user input (warm amber).
    Waiting,
    /// Configured local template or remote SSH server target (muted slate).
    Idle,
    Exited(i32),
}

/// One discovered or managed agent session on either the local laptop or a remote SSH server.
#[derive(Clone, Debug)]
pub struct AgentRecord {
    pub id: String,
    pub host: String,
    pub is_remote: bool,
    pub name: String,
    /// Concise high-signal subtitle (`~/blaze-shell-1124 · 214M · 12m` or `andrew@hil-compute-1`).
    pub prompt_summary: String,
    /// Right-aligned compact metric badge (`18%`, `412M`, `remote`, etc.).
    pub metric_badge: String,
    pub cwd: String,
    pub pid: Option<u32>,
    pub status: AgentStatus,
    /// Optional block/session index if launched directly inside `blaze-shell`.
    pub pty_session_idx: Option<usize>,
    /// Recent transcript / structured telemetry lines previewed in the right pane.
    pub preview_lines: Vec<String>,
}

/// Outcome returned by [`AgentHub::handle_key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentHubAction {
    Continue,
    Exit,
    /// Attach/enter an interactive agent session (local command or SSH remote).
    EnterAgent {
        host: String,
        is_remote: bool,
        cwd: String,
        prompt: Option<String>,
        pty_session_idx: Option<usize>,
    },
}

/// Interactive multi-server Agent Hub state (`agent` command).
pub struct AgentHub {
    pub local_host: String,
    pub agents: Vec<AgentRecord>,
    pub selected: usize,
    /// Resizable left sidebar ratio (`0.18..=0.68`, draggable with `NSCursor::resizeLeftRightCursor`).
    pub sidebar_ratio: f32,
    pub composing_new: bool,
    pub new_prompt: String,
}

impl AgentHub {
    pub fn open(local_host: &str, home: &str, cwd: &str, live_sessions: &[AgentRecord]) -> Self {
        let mut hub = Self {
            local_host: local_host.to_owned(),
            agents: Vec::new(),
            selected: 0,
            sidebar_ratio: 0.34,
            composing_new: false,
            new_prompt: String::new(),
        };
        hub.refresh(home, cwd, live_sessions);
        hub
    }

    pub fn set_sidebar_ratio(&mut self, ratio: f32) -> bool {
        let clamped = ratio.clamp(0.18, 0.68);
        if (clamped - self.sidebar_ratio).abs() > 0.001 {
            self.sidebar_ratio = clamped;
            true
        } else {
            false
        }
    }

    pub fn refresh(&mut self, home: &str, cwd: &str, live_sessions: &[AgentRecord]) {
        let mut list: Vec<AgentRecord> = live_sessions.to_vec();

        // 1. Discover live local agent processes (`claude`, `codex`, `harness`) with CPU%, RAM, uptime.
        struct RawProc {
            pid: u32,
            kind: &'static str,
            cpu: f32,
            rss_mb: u32,
            etime: String,
            running_state: bool,
            cmd: String,
        }
        let mut raw_procs: Vec<RawProc> = Vec::new();
        if let Ok(out) = Command::new("ps")
            .args(["-axo", "pid=,pcpu=,rss=,etime=,state=,command="])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            let txt = String::from_utf8_lossy(&out.stdout);
            for line in txt.lines() {
                let mut parts = line.split_whitespace();
                let (Some(pid_s), Some(cpu_s), Some(rss_s), Some(etime_s), Some(st_s)) = (
                    parts.next(),
                    parts.next(),
                    parts.next(),
                    parts.next(),
                    parts.next(),
                ) else {
                    continue;
                };
                let cmd = parts.collect::<Vec<_>>().join(" ");
                let lower = cmd.to_ascii_lowercase();
                let kind = if lower.starts_with("claude")
                    || lower.contains("/claude ")
                    || lower.ends_with("/claude")
                {
                    Some("claude")
                } else if lower.starts_with("codex")
                    || lower.contains("/codex ")
                    || lower.ends_with("/codex")
                {
                    Some("codex")
                } else if lower.contains("harness-native") || lower.contains("blaze-agent") {
                    Some("harness")
                } else {
                    None
                };
                if let Some(kind_name) = kind
                    && let Ok(pid) = pid_s.parse::<u32>()
                    && !list.iter().any(|a| a.pid == Some(pid))
                {
                    let cpu = cpu_s.parse::<f32>().unwrap_or(0.0);
                    let rss_kb = rss_s.parse::<u64>().unwrap_or(0);
                    let rss_mb = ((rss_kb + 512) / 1024) as u32;
                    let running_state = st_s.starts_with('R');
                    raw_procs.push(RawProc {
                        pid,
                        kind: kind_name,
                        cpu,
                        rss_mb,
                        etime: format_compact_etime(etime_s),
                        running_state,
                        cmd,
                    });
                }
            }
        }

        // Single batched `lsof` lookup for real working directories of all discovered agent PIDs!
        let pid_cwds = lookup_process_cwds(&raw_procs.iter().map(|p| p.pid).collect::<Vec<_>>());

        // Sort active/high-CPU agents first, then by RAM descending so the most relevant appear on top.
        raw_procs.sort_by(|a, b| {
            let a_active = a.running_state || a.cpu >= 1.5;
            let b_active = b.running_state || b.cpu >= 1.5;
            b_active
                .cmp(&a_active)
                .then_with(|| b.cpu.total_cmp(&a.cpu))
                .then_with(|| b.rss_mb.cmp(&a.rss_mb))
        });

        for p in raw_procs {
            let real_cwd = pid_cwds
                .get(&p.pid)
                .cloned()
                .unwrap_or_else(|| cwd.to_owned());
            let short_cwd = shorten_agent_path(&real_cwd, home);
            let folder = Path::new(&real_cwd)
                .file_name()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("~");
            let is_active = p.running_state || p.cpu >= 1.5;
            let status = if is_active {
                AgentStatus::Running
            } else {
                AgentStatus::Waiting
            };
            let metric_badge = if is_active {
                format!("{:.0}%", p.cpu.max(1.0))
            } else if p.rss_mb > 0 {
                format!("{}M", p.rss_mb)
            } else {
                p.etime.clone()
            };
            let short_cmd = abbreviate_cmd_paths(&p.cmd, home);
            let name = format!("{} · {}", p.kind, folder);
            let prompt_summary = if p.etime.is_empty() {
                short_cwd.clone()
            } else {
                format!("{short_cwd} · {}", p.etime)
            };

            list.push(AgentRecord {
                id: format!("local-{}", p.pid),
                host: self.local_host.clone(),
                is_remote: false,
                name,
                prompt_summary,
                metric_badge,
                cwd: real_cwd.clone(),
                pid: Some(p.pid),
                status,
                pty_session_idx: None,
                preview_lines: vec![
                    format!("state:  {}", if is_active { "working (active CPU)" } else { "waiting for input" }),
                    format!("cwd:    {short_cwd}"),
                    format!("host:   {} (local) · pid {}", self.local_host, p.pid),
                    format!("usage:  {:.1}% CPU · {} MB RAM · up {}", p.cpu, p.rss_mb, p.etime),
                    format!("cmd:    {short_cmd}"),
                    String::new(),
                    "Enter  attach fresh session here   ·   n  launch with task prompt".to_owned(),
                ],
            });
        }

        // 2. Always include a quick-start local workspace agent entry if none exist.
        if list.is_empty() {
            let short_cwd = shorten_agent_path(cwd, home);
            list.push(AgentRecord {
                id: "local-new".to_owned(),
                host: self.local_host.clone(),
                is_remote: false,
                name: "local workspace agent".to_owned(),
                prompt_summary: short_cwd.clone(),
                metric_badge: "ready".to_owned(),
                cwd: cwd.to_owned(),
                pid: None,
                status: AgentStatus::Idle,
                pty_session_idx: None,
                preview_lines: vec![
                    format!("host:   {} (local)", self.local_host),
                    format!("cwd:    {short_cwd}"),
                    String::new(),
                    "Enter  start interactive agent   ·   n  give a task prompt".to_owned(),
                ],
            });
        }

        // 3. Discover remote servers directly from `~/.ssh/config` via our sans-IO `blaze-ssh`!
        let ssh_path = PathBuf::from(home).join(".ssh/config");
        if let Ok(raw) = fs::read_to_string(&ssh_path) {
            let alloc = Alloc::global();
            let cfg = blaze_ssh::parse_config(&raw, blaze_ssh::ConfigLimits::SHIPPED, alloc);
            for h in cfg.list_hosts(64, alloc).iter() {
                let alias = h.alias.as_str();
                if list.iter().any(|a| a.host == alias && a.is_remote) {
                    continue;
                }
                let target = h.hostname.as_ref().map(|s| s.as_str()).unwrap_or(alias);
                let user = h.user.as_ref().map(|s| s.as_str()).unwrap_or("root");
                let port = h.port.unwrap_or(22);
                let short_target = strip_tailscale_suffix(target);
                let port_s = if port == 22 { String::new() } else { format!(":{port}") };
                list.push(AgentRecord {
                    id: format!("ssh-{alias}"),
                    host: alias.to_owned(),
                    is_remote: true,
                    name: alias.to_owned(),
                    prompt_summary: format!("{user}@{short_target}{port_s}"),
                    metric_badge: "ssh".to_owned(),
                    cwd: "~".to_owned(),
                    pid: None,
                    status: AgentStatus::Idle,
                    pty_session_idx: None,
                    preview_lines: vec![
                        format!("server: {alias}"),
                        format!("target: {user}@{target}:{port}"),
                        "proto:  blaze-ssh (BSH1 / SSH)".to_owned(),
                        String::new(),
                        "Enter  connect interactive agent   ·   n  dispatch prompted task".to_owned(),
                    ],
                });
            }
        }

        self.agents = list;
        if self.selected >= self.agents.len() {
            self.selected = self.agents.len().saturating_sub(1);
        }
    }

    pub fn handle_key(&mut self, key: &str, ch: Option<char>) -> AgentHubAction {
        if self.composing_new {
            match key {
                "Escape" => {
                    self.composing_new = false;
                    self.new_prompt.clear();
                }
                "Backspace" => {
                    self.new_prompt.pop();
                }
                "Enter" => {
                    let prompt_txt = core::mem::take(&mut self.new_prompt);
                    self.composing_new = false;
                    if let Some(a) = self.agents.get(self.selected) {
                        return AgentHubAction::EnterAgent {
                            host: a.host.clone(),
                            is_remote: a.is_remote,
                            cwd: a.cwd.clone(),
                            prompt: if prompt_txt.trim().is_empty() {
                                None
                            } else {
                                Some(prompt_txt)
                            },
                            pty_session_idx: None,
                        };
                    }
                }
                _ => {
                    if let Some(c) = ch
                        && !c.is_control()
                    {
                        self.new_prompt.push(c);
                    }
                }
            }
            return AgentHubAction::Continue;
        }

        match key {
            "Escape" => return AgentHubAction::Exit,
            "Up" => {
                self.selected = self.selected.saturating_sub(1);
                return AgentHubAction::Continue;
            }
            "Down" => {
                if self.selected + 1 < self.agents.len() {
                    self.selected += 1;
                }
                return AgentHubAction::Continue;
            }
            "Enter" => {
                if let Some(a) = self.agents.get(self.selected) {
                    return AgentHubAction::EnterAgent {
                        host: a.host.clone(),
                        is_remote: a.is_remote,
                        cwd: a.cwd.clone(),
                        prompt: None,
                        pty_session_idx: a.pty_session_idx,
                    };
                }
                return AgentHubAction::Continue;
            }
            _ => {}
        }

        match ch {
            Some('q') => AgentHubAction::Exit,
            Some('j') => {
                if self.selected + 1 < self.agents.len() {
                    self.selected += 1;
                }
                AgentHubAction::Continue
            }
            Some('k') => {
                self.selected = self.selected.saturating_sub(1);
                AgentHubAction::Continue
            }
            Some('n') => {
                self.composing_new = true;
                self.new_prompt.clear();
                AgentHubAction::Continue
            }
            _ => AgentHubAction::Continue,
        }
    }
}

/// Batched single-call lookup of real working directories for local PIDs via `lsof -a -d cwd -Fn`.
fn lookup_process_cwds(pids: &[u32]) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    if pids.is_empty() {
        return out;
    }
    let pid_list: String = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let Ok(res) = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-Fn", "-p", &pid_list])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return out;
    };
    let text = String::from_utf8_lossy(&res.stdout);
    let mut cur_pid: Option<u32> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            cur_pid = rest.trim().parse::<u32>().ok();
        } else if let Some(rest) = line.strip_prefix('n')
            && let Some(pid) = cur_pid
            && !rest.is_empty()
        {
            out.insert(pid, rest.to_owned());
        }
    }
    out
}

fn shorten_agent_path(path: &str, home: &str) -> String {
    let abbr = abbreviate_home(path, home);
    if let Some(rest) = abbr.strip_prefix("/Volumes/Projects/") {
        return format!("Projects/{rest}");
    }
    if abbr.starts_with("/private/var/folders/") {
        return "tmp (sandbox)".to_owned();
    }
    abbr
}

fn abbreviate_cmd_paths(cmd: &str, home: &str) -> String {
    let mut s = if !home.is_empty() {
        cmd.replace(home, "~")
    } else {
        cmd.to_owned()
    };
    if s.contains("/private/var/folders/") {
        // Trim noisy macOS temp bundle prefix from argv[0] while keeping binary name + args.
        let parts: Vec<&str> = s.split_whitespace().collect();
        if let Some(first) = parts.first()
            && first.starts_with("/private/var/folders/")
            && let Some(bin) = Path::new(first).file_name().and_then(|n| n.to_str())
        {
            let mut rebuilt = bin.to_owned();
            for a in &parts[1..] {
                rebuilt.push(' ');
                rebuilt.push_str(a);
            }
            s = rebuilt;
        }
    }
    s
}

fn strip_tailscale_suffix(host: &str) -> &str {
    if let Some(idx) = host.find(".tail")
        && host.ends_with(".ts.net")
    {
        return &host[..idx];
    }
    host
}

/// Format `ps` `etime` (`[[dd-]hh:]mm:ss`) concisely as `4m`, `2h15m`, `3d`.
fn format_compact_etime(raw: &str) -> String {
    let s = raw.trim();
    if let Some((days, rest)) = s.split_once('-') {
        let d = days.trim_start_matches('0');
        let d = if d.is_empty() { "0" } else { d };
        let h = rest.split(':').next().unwrap_or("0").trim_start_matches('0');
        return if h.is_empty() {
            format!("{d}d")
        } else {
            format!("{d}d{h}h")
        };
    }
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        3 => {
            let h: u32 = parts[0].parse().unwrap_or(0);
            let m: u32 = parts[1].parse().unwrap_or(0);
            if h > 0 {
                format!("{h}h{m}m")
            } else {
                format!("{m}m")
            }
        }
        2 => {
            let m: u32 = parts[0].parse().unwrap_or(0);
            let sec: u32 = parts[1].parse().unwrap_or(0);
            if m > 0 {
                format!("{m}m")
            } else {
                format!("{sec}s")
            }
        }
        _ => s.to_owned(),
    }
}

/// Lay out the unified multi-server `agent` fleet overview in Metal, keeping the top tab bar live!
#[allow(clippy::too_many_arguments)]
pub fn layout_agent_hub(
    hub: &AgentHub,
    store: &BlockStore,
    prompt: &PromptInfo,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    zoom_pct: u16,
    inline_titlebar: bool,
) -> Scene {
    layout_agent_hub_inner(hub, store, prompt, viewport_w, viewport_h, m, zoom_pct, inline_titlebar, true)
}

/// Lay out the unified multi-server `agent` fleet overview inside a tiled pane.
#[allow(clippy::too_many_arguments)]
pub fn layout_agent_hub_pane(
    hub: &AgentHub,
    store: &BlockStore,
    prompt: &PromptInfo,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    zoom_pct: u16,
) -> Scene {
    layout_agent_hub_inner(hub, store, prompt, viewport_w, viewport_h, m, zoom_pct, false, false)
}

#[allow(clippy::too_many_arguments)]
fn layout_agent_hub_inner(
    hub: &AgentHub,
    store: &BlockStore,
    prompt: &PromptInfo,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    zoom_pct: u16,
    inline_titlebar: bool,
    draw_top_bar: bool,
) -> Scene {
    let mut scene = Scene::new();
    let vw = viewport_w.max(240.0);
    let vh = viewport_h.max(200.0);

    let header_h = m.ch + 16.0;
    let footer_h = m.ch + 14.0;
    let body_top = TOP_BAR_H + header_h;
    let body_bot = (vh - footer_h).max(body_top + 60.0);

    // 1. Sub-header bar: active vs waiting vs remote counts.
    scene.round_rect(0.0, TOP_BAR_H, vw, body_top, 0.0, theme::BG_SURFACE, 0.0, [0.0; 4], 0);
    scene.line(0.0, body_top - 0.5, vw, body_top - 0.5, 1.0, theme::LINE, 0);
    let hty = TOP_BAR_H + (header_h - m.ch) * 0.5;
    draw_icon(&mut scene, Icon::Server, 14.0, hty + (m.ch - 12.0) * 0.5, 12.0, theme::INK_2, 0);
    let active_n = hub.agents.iter().filter(|a| a.status == AgentStatus::Running).count();
    let waiting_n = hub.agents.iter().filter(|a| a.status == AgentStatus::Waiting).count();
    let remote_n = hub.agents.iter().filter(|a| a.is_remote).count();
    let hdr_lbl = format!(
        "agents · {active_n} working · {waiting_n} waiting · {remote_n} remote"
    );
    scene.text(32.0, hty, hdr_lbl, theme::INK_1, true, false, 0);

    // 2. Resizable two-pane split: Left fleet list | Right inspector (draggable divider!).
    let split_max = (vw - 180.0).max(160.0);
    let split_min = 170.0f32.min(split_max);
    let split_x = (vw * hub.sidebar_ratio).clamp(split_min, split_max);
    scene.line(split_x, body_top, split_x, body_bot, 1.0, theme::LINE, 0);
    // Subtle center resize handle grip on the sidebar divider + 10px wide EW-resize hit target.
    let mid_y = (body_top + body_bot) * 0.5;
    scene.round_rect(
        split_x - 1.5,
        mid_y - 16.0,
        split_x + 1.5,
        mid_y + 16.0,
        1.5,
        theme::LINE_STRONG,
        0.0,
        [0.0; 4],
        0,
    );
    scene.push_hit(
        HitRect::new(split_x - 5.0, body_top, split_x + 5.0, body_bot),
        HitAction::ResizeAgentSidebar,
    );

    let left_clip = scene.push_clip(0.0, body_top, split_x - 1.0, body_bot, 0.0);
    let row_h = m.ch * 2.0 + 12.0;
    let mut ry = body_top + 8.0;
    let waiting_amber = theme::rgb(0xe5, 0xb5, 0x67);
    for (i, ag) in hub.agents.iter().enumerate() {
        if ry + row_h > body_bot {
            break;
        }
        let is_sel = i == hub.selected;
        let row_rect = HitRect::new(8.0, ry, split_x - 8.0, ry + row_h);
        if is_sel {
            scene.round_rect(
                row_rect.x0,
                row_rect.y0,
                row_rect.x1,
                row_rect.y1,
                6.0,
                theme::COMPLETION_SEL_BG,
                1.0,
                theme::rgba(0x58, 0x9d, 0xf6, 0.45),
                left_clip,
            );
            scene.round_rect(
                8.0,
                ry + 6.0,
                10.0,
                ry + row_h - 6.0,
                1.0,
                theme::MATCH_HIGHLIGHT,
                0.0,
                [0.0; 4],
                left_clip,
            );
        }
        scene.push_hit(row_rect, HitAction::SelectAgent(i));

        let ty1 = ry + 5.0;
        let ty2 = ty1 + m.ch + 2.0;
        let ag_icon_s = m.icon_md();
        if !ag.is_remote && let Some(sym) = prompt.host_symbol.as_deref() {
            scene.emoji_symbol(
                16.0,
                ty1 + (m.ch - ag_icon_s) * 0.5,
                ag_icon_s,
                sym,
                if is_sel { 1.0 } else { 0.85 },
                left_clip,
            );
        } else {
            let svg = if ag.is_remote { "server_rack" } else { "macbook_pro" };
            scene.svg_icon_alpha(
                17.0,
                ty1 + (m.ch - ag_icon_s) * 0.5,
                ag_icon_s,
                svg,
                if is_sel { 1.0 } else { 0.72 },
                left_clip,
            );
        }

        // Semantic status dot: vivid emerald = active working, warm amber = idle/waiting, gray = remote.
        let dot_col = match ag.status {
            AgentStatus::Running => theme::OK,
            AgentStatus::Waiting => waiting_amber,
            AgentStatus::Idle => theme::INK_4,
            AgentStatus::Exited(0) => theme::INK_3,
            AgentStatus::Exited(_) => theme::DANGER,
        };
        let dot_x = split_x - 18.0;
        scene.over_circle(dot_x, ty1 + m.ch * 0.5, 3.0, dot_col, left_clip);

        // Right-aligned compact metric badge (`18%`, `412M`, `ssh`) to the left of the status dot.
        let mut title_right = dot_x - 8.0;
        if !ag.metric_badge.is_empty() {
            let bw = ag.metric_badge.chars().count() as f32 * m.cw;
            if title_right - bw > 90.0 {
                title_right -= bw;
                let badge_col = match ag.status {
                    AgentStatus::Running => theme::OK,
                    AgentStatus::Waiting => theme::INK_3,
                    _ => theme::INK_4,
                };
                scene.text(
                    title_right,
                    ty1,
                    &ag.metric_badge,
                    badge_col,
                    ag.status == AgentStatus::Running,
                    false,
                    left_clip,
                );
                title_right -= 8.0;
            }
        }

        let max_title_c = ((title_right - 36.0) / m.cw).floor().max(6.0) as usize;
        scene.text(
            36.0,
            ty1,
            truncate_chars(&ag.name, max_title_c),
            if is_sel { theme::INK_1 } else { theme::INK_2 },
            is_sel,
            false,
            left_clip,
        );
        let max_sub_c = ((split_x - 48.0) / m.cw).floor().max(8.0) as usize;
        scene.text(
            36.0,
            ty2,
            truncate_middle(&ag.prompt_summary, max_sub_c),
            if is_sel { theme::INK_3 } else { theme::INK_4 },
            false,
            false,
            left_clip,
        );
        ry += row_h + 4.0;
    }

    // Right pane: live session inspector + inline `n` prompt composer.
    let right_clip = scene.push_clip(split_x + 1.0, body_top, vw, body_bot, 0.0);
    let rx = split_x + 18.0;
    let rw = (vw - rx - 18.0).max(120.0);
    if let Some(ag) = hub.agents.get(hub.selected) {
        let mut cy = body_top + 14.0;
        let icon = if ag.is_remote { Icon::Server } else { Icon::Laptop };
        draw_icon(&mut scene, icon, rx, cy + (m.ch - 13.0) * 0.5, 13.0, theme::INK_2, right_clip);
        scene.text(rx + 20.0, cy, &ag.name, theme::INK_1, true, false, right_clip);
        let (status_lbl, status_col) = match ag.status {
            AgentStatus::Running => ("working", theme::OK),
            AgentStatus::Waiting => ("waiting", waiting_amber),
            AgentStatus::Idle => (if ag.is_remote { "remote" } else { "ready" }, theme::INK_3),
            AgentStatus::Exited(0) => ("done", theme::INK_3),
            AgentStatus::Exited(_) => ("failed", theme::DANGER),
        };
        let sw = status_lbl.len() as f32 * m.cw;
        scene.text(vw - 18.0 - sw, cy, status_lbl, status_col, true, false, right_clip);
        cy += m.ch + 8.0;
        scene.line(rx, cy, vw - 18.0, cy, 1.0, theme::LINE_FAINT, right_clip);
        cy += 10.0;

        let max_c = (rw / m.cw).floor().max(16.0) as usize;
        for line in &ag.preview_lines {
            if cy + m.ch > body_bot - 48.0 {
                break;
            }
            scene.text(rx, cy, truncate_chars(line, max_c), theme::INK_2, false, false, right_clip);
            cy += m.ch + 3.0;
        }

        if hub.composing_new {
            let box_y0 = (body_bot - m.ch - 26.0).max(cy + 12.0);
            let box_y1 = box_y0 + m.ch + 16.0;
            scene.round_rect(
                rx,
                box_y0,
                vw - 18.0,
                box_y1,
                6.0,
                theme::rgb(0x18, 0x18, 0x1c),
                1.0,
                theme::LINE_STRONG,
                right_clip,
            );
            let ity = box_y0 + 8.0;
            scene.text(rx + 10.0, ity, "❯", theme::INK_1, true, false, right_clip);
            let ptxt = if hub.new_prompt.is_empty() {
                format!("task for {} (Enter to launch)…", ag.host)
            } else {
                hub.new_prompt.clone()
            };
            let pcol = if hub.new_prompt.is_empty() { theme::INK_4 } else { theme::INK_1 };
            scene.text(
                rx + 10.0 + 1.8 * m.cw,
                ity,
                truncate_chars(&ptxt, max_c.saturating_sub(4)),
                pcol,
                false,
                false,
                right_clip,
            );
        }
    }

    // 3. Footer shortcuts bar.
    scene.round_rect(0.0, body_bot, vw, vh, 0.0, theme::BG_SURFACE, 0.0, [0.0; 4], 0);
    scene.line(0.0, body_bot, vw, body_bot, 1.0, theme::LINE, 0);
    let fty = body_bot + (footer_h - m.ch) * 0.5;
    scene.text(14.0, fty, "agent · harness fleet", theme::INK_2, true, false, 0);
    let hints = "drag divider resize  ·  j/k switch  ·  enter attach  ·  n new task  ·  q/esc back";
    let hw = hints.chars().count() as f32 * m.cw;
    if vw - 14.0 - hw > 180.0 {
        scene.text(vw - 14.0 - hw, fty, hints, theme::INK_4, false, false, 0);
    }

    if draw_top_bar {
        layout_top_bar(&mut scene, store, prompt, vw, m, zoom_pct, inline_titlebar);
    }
    scene
}
