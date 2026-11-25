//! Unified multi-server interactive `agent` fleet hub (combining `harness` + `blaze-shell`):
//! discovers live local agents (`claude`, `codex`, `blaze-agent`, spawned `agent "<prompt>"`
//! sessions) and remote SSH hosts from `blaze-ssh`, renders a live 2-pane Metal overview with
//! Laptop/Server icons, and lets you jump between & enter any agent's interactive session!

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use blaze_own::Alloc;

use crate::blocks::BlockStore;
use crate::icons::{Icon, draw_icon};
use crate::layout::{TOP_BAR_H, layout_top_bar};
use crate::prompt::PromptInfo;
use crate::theme;
use crate::view::{CellMetrics, Scene, truncate_chars};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
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
    pub prompt_summary: String,
    pub cwd: String,
    pub pid: Option<u32>,
    pub status: AgentStatus,
    /// Optional block/session index if launched directly inside `blaze-shell`.
    pub pty_session_idx: Option<usize>,
    /// Recent transcript / status lines previewed in the right pane.
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
    pub composing_new: bool,
    pub new_prompt: String,
}

impl AgentHub {
    pub fn open(local_host: &str, home: &str, cwd: &str, live_sessions: &[AgentRecord]) -> Self {
        let mut hub = Self {
            local_host: local_host.to_owned(),
            agents: Vec::new(),
            selected: 0,
            composing_new: false,
            new_prompt: String::new(),
        };
        hub.refresh(home, cwd, live_sessions);
        hub
    }

    pub fn refresh(&mut self, home: &str, cwd: &str, live_sessions: &[AgentRecord]) {
        let mut list: Vec<AgentRecord> = live_sessions.to_vec();

        // 1. Discover live local agent processes (`claude`, `codex`, `harness`) via bounded `ps`.
        if let Ok(out) = Command::new("ps")
            .args(["-axo", "pid=,state=,command="])
            .output()
        {
            let txt = String::from_utf8_lossy(&out.stdout);
            for line in txt.lines() {
                let mut parts = line.split_whitespace();
                let (Some(pid_s), Some(_st)) = (parts.next(), parts.next()) else {
                    continue;
                };
                let cmd = parts.collect::<Vec<_>>().join(" ");
                let lower = cmd.to_ascii_lowercase();
                let kind = if lower.starts_with("claude") || lower.contains("/claude ") || lower.ends_with("/claude") {
                    Some("claude")
                } else if lower.starts_with("codex") || lower.contains("/codex ") {
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
                    list.push(AgentRecord {
                        id: format!("local-{pid}"),
                        host: self.local_host.clone(),
                        is_remote: false,
                        name: format!("{kind_name} #{pid}"),
                        prompt_summary: truncate_chars(&cmd, 64),
                        cwd: cwd.to_owned(),
                        pid: Some(pid),
                        status: AgentStatus::Running,
                        pty_session_idx: None,
                        preview_lines: vec![
                            format!("host: {} (local)", self.local_host),
                            format!("pid:  {pid}"),
                            format!("cmd:  {cmd}"),
                            String::new(),
                            "press Enter to launch a fresh attached session, or `n` for a prompted agent.".to_owned(),
                        ],
                    });
                }
            }
        }

        // 2. Always include a quick-start local workspace agent entry.
        if list.is_empty() {
            list.push(AgentRecord {
                id: "local-new".to_owned(),
                host: self.local_host.clone(),
                is_remote: false,
                name: "local workspace agent".to_owned(),
                prompt_summary: format!("claude in {cwd}"),
                cwd: cwd.to_owned(),
                pid: None,
                status: AgentStatus::Idle,
                pty_session_idx: None,
                preview_lines: vec![
                    format!("host: {} (local)", self.local_host),
                    format!("cwd:  {cwd}"),
                    String::new(),
                    "press Enter to start an interactive agent here, or `n` to give a task prompt.".to_owned(),
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
                list.push(AgentRecord {
                    id: format!("ssh-{alias}"),
                    host: alias.to_owned(),
                    is_remote: true,
                    name: format!("{alias} · remote agent"),
                    prompt_summary: format!("{user}@{target}:{port}"),
                    cwd: "~".to_owned(),
                    pid: None,
                    status: AgentStatus::Idle,
                    pty_session_idx: None,
                    preview_lines: vec![
                        format!("server: {alias} ({user}@{target}:{port})"),
                        "proto:  blaze-ssh (BSH1 / SSH)".to_owned(),
                        String::new(),
                        "press Enter to connect & enter an interactive agent on this server.".to_owned(),
                        "press `n` to dispatch a prompted task directly to this server.".to_owned(),
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
                            prompt: if prompt_txt.trim().is_empty() { None } else { Some(prompt_txt) },
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

    // 1. Sub-header bar: `agent fleet · N hosts/sessions`.
    scene.round_rect(0.0, TOP_BAR_H, vw, body_top, 0.0, theme::BG_SURFACE, 0.0, [0.0; 4], 0);
    scene.line(0.0, body_top - 0.5, vw, body_top - 0.5, 1.0, theme::LINE, 0);
    let hty = TOP_BAR_H + (header_h - m.ch) * 0.5;
    draw_icon(&mut scene, Icon::Server, 14.0, hty + (m.ch - 12.0) * 0.5, 12.0, theme::INK_2, 0);
    let running_n = hub.agents.iter().filter(|a| a.status == AgentStatus::Running).count();
    let hdr_lbl = format!("agents · {} running · {} targets across servers", running_n, hub.agents.len());
    scene.text(32.0, hty, hdr_lbl, theme::INK_1, true, false, 0);

    // 2. Two-pane split: Left fleet list (38%) | Right live preview & prompt composer (62%).
    let split_max = (vw - 180.0).max(160.0).min(420.0);
    let split_min = 160.0f32.min(split_max);
    let split_x = (vw * 0.38).clamp(split_min, split_max);
    scene.line(split_x, body_top, split_x, body_bot, 1.0, theme::LINE_FAINT, 0);

    let left_clip = scene.push_clip(0.0, body_top, split_x - 1.0, body_bot, 0.0);
    let row_h = m.ch * 2.0 + 12.0;
    let mut ry = body_top + 8.0;
    for (i, ag) in hub.agents.iter().enumerate() {
        if ry + row_h > body_bot {
            break;
        }
        let is_sel = i == hub.selected;
        if is_sel {
            scene.round_rect(8.0, ry, split_x - 8.0, ry + row_h, 6.0, theme::rgb(0x1d, 0x1d, 0x22), 1.0, theme::LINE_STRONG, left_clip);
            scene.round_rect(8.0, ry + 6.0, 10.0, ry + row_h - 6.0, 1.0, theme::INK_1, 0.0, [0.0; 4], left_clip);
        }
        let ty1 = ry + 5.0;
        let ty2 = ty1 + m.ch + 2.0;
        let svg = if ag.is_remote { "server_rack" } else { "macbook_pro" };
        let ag_icon_s = m.icon_md();
        scene.svg_icon_alpha(17.0, ty1 + (m.ch - ag_icon_s) * 0.5, ag_icon_s, svg, if is_sel { 1.0 } else { 0.72 }, left_clip);

        // Status indicator dot.
        let dot_col = match ag.status {
            AgentStatus::Running => theme::OK,
            AgentStatus::Idle => theme::INK_4,
            AgentStatus::Exited(0) => theme::INK_3,
            AgentStatus::Exited(_) => theme::DANGER,
        };
        scene.over_circle(split_x - 20.0, ty1 + m.ch * 0.5, 3.0, dot_col, left_clip);

        let max_c = ((split_x - 68.0) / m.cw).floor().max(8.0) as usize;
        scene.text(36.0, ty1, truncate_chars(&ag.name, max_c), if is_sel { theme::INK_1 } else { theme::INK_2 }, is_sel, false, left_clip);
        let sub = format!("{} · {}", ag.host, ag.prompt_summary);
        scene.text(36.0, ty2, truncate_chars(&sub, max_c + 4), theme::INK_4, false, false, left_clip);
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
        let status_lbl = match ag.status {
            AgentStatus::Running => "running",
            AgentStatus::Idle => "ready",
            AgentStatus::Exited(c) => if c == 0 { "done" } else { "failed" },
        };
        let sw = status_lbl.len() as f32 * m.cw;
        scene.text(vw - 18.0 - sw, cy, status_lbl, if ag.status == AgentStatus::Running { theme::OK } else { theme::INK_3 }, false, false, right_clip);
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
            scene.round_rect(rx, box_y0, vw - 18.0, box_y1, 6.0, theme::rgb(0x18, 0x18, 0x1c), 1.0, theme::LINE_STRONG, right_clip);
            let ity = box_y0 + 8.0;
            scene.text(rx + 10.0, ity, "❯", theme::INK_1, true, false, right_clip);
            let ptxt = if hub.new_prompt.is_empty() {
                format!("task for {} (Enter to launch)…", ag.host)
            } else {
                hub.new_prompt.clone()
            };
            let pcol = if hub.new_prompt.is_empty() { theme::INK_4 } else { theme::INK_1 };
            scene.text(rx + 10.0 + 1.8 * m.cw, ity, truncate_chars(&ptxt, max_c.saturating_sub(4)), pcol, false, false, right_clip);
        }
    }

    // 3. Footer shortcuts bar.
    scene.round_rect(0.0, body_bot, vw, vh, 0.0, theme::BG_SURFACE, 0.0, [0.0; 4], 0);
    scene.line(0.0, body_bot, vw, body_bot, 1.0, theme::LINE, 0);
    let fty = body_bot + (footer_h - m.ch) * 0.5;
    scene.text(14.0, fty, "agent · harness fleet", theme::INK_2, true, false, 0);
    let hints = "j/k switch  ·  enter attach  ·  n new task  ·  q/esc back to shell";
    let hw = hints.chars().count() as f32 * m.cw;
    if vw - 14.0 - hw > 180.0 {
        scene.text(vw - 14.0 - hw, fty, hints, theme::INK_4, false, false, 0);
    }

    if draw_top_bar {
        layout_top_bar(&mut scene, store, prompt, vw, m, zoom_pct, inline_titlebar);
    }
    scene
}
