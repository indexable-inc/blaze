//! Unified multi-server interactive `agent` fleet hub (combining `harness` + `blaze-shell`):
//! discovers live local agents (`claude`, `codex`, `harness` / `blaze-agent`, plus spawned
//! in-shell sessions) using zero-subprocess native macOS telemetry (`blaze-sysmon` +
//! `sysmon_driver`), deduplicates SSH server aliases from `blaze-ssh` with non-blocking
//! reachability probing (`agent_probe`), lazily tails live agent transcripts, and renders a
//! scrollable, resizable 2-pane Metal fleet inspector with safe kill controls.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use blaze_own::Alloc;
use blaze_sysmon::{SysMonEngine, app_display_name, extract_app_bundle_path};

use crate::agent_probe::{
    PreviewRole, ServerReachability, TranscriptEntry, invalidate_probe_cache,
    lookup_or_probe_server, read_agent_transcript_tail,
};
use crate::blocks::BlockStore;
use crate::icons::{Icon, draw_icon};
use crate::layout::{TOP_BAR_H, layout_top_bar};
use crate::prompt::{PromptInfo, abbreviate_home};
use crate::sysmon_view::{format_iec_bytes, format_uptime};
use crate::theme;
use crate::ui_widgets::{
    ScrollState, draw_pill_button, draw_scrollbar, draw_section_header, draw_status_pill,
};
use crate::view::{
    CellMetrics, HitAction, HitRect, Scene, TextFace, truncate_chars, truncate_middle,
};

// =============================================================================
// Fleet Hub Palette Tokens (harmonized with `ix.dev/blaze` charcoal)
// =============================================================================

const CARD_BG: [f32; 4] = theme::rgb(0x18, 0x18, 0x1a);
const WAITING_AMBER: [f32; 4] = theme::rgb(0xe5, 0xb5, 0x67);
const WAITING_BG: [f32; 4] = theme::rgba(0xe5, 0xb5, 0x67, 0.14);
const WAITING_BORDER: [f32; 4] = theme::rgba(0xe5, 0xb5, 0x67, 0.36);

const OK_BG: [f32; 4] = theme::rgba(0x4c, 0xc2, 0x7a, 0.13);
const OK_BORDER: [f32; 4] = theme::rgba(0x4c, 0xc2, 0x7a, 0.36);

const CLAUDE_FG: [f32; 4] = theme::rgb(0xf2, 0x95, 0x72);
const CLAUDE_BG: [f32; 4] = theme::rgba(0xe8, 0x78, 0x54, 0.18);
const CLAUDE_BORDER: [f32; 4] = theme::rgba(0xe8, 0x78, 0x54, 0.38);

const CODEX_FG: [f32; 4] = theme::rgb(0x63, 0xd8, 0x9e);
const CODEX_BG: [f32; 4] = theme::rgba(0x4c, 0xc9, 0x8a, 0.18);
const CODEX_BORDER: [f32; 4] = theme::rgba(0x4c, 0xc9, 0x8a, 0.38);

const HARNESS_FG: [f32; 4] = theme::rgb(0x7c, 0xb7, 0xff);
const HARNESS_BG: [f32; 4] = theme::rgba(0x58, 0x9d, 0xf6, 0.18);
const HARNESS_BORDER: [f32; 4] = theme::rgba(0x58, 0x9d, 0xf6, 0.38);

const SERVER_SKY: [f32; 4] = theme::rgb(0x9a, 0xd8, 0xff);
const SERVER_ICON_SKY: [f32; 4] = theme::rgb(0x60, 0xb8, 0xff);
const SERVER_ICON_BG: [f32; 4] = theme::rgba(0x60, 0xb8, 0xff, 0.14);
const SERVER_ICON_BORDER: [f32; 4] = theme::rgba(0x60, 0xb8, 0xff, 0.32);
const SERVER_WHITE: [f32; 4] = theme::rgb(0xff, 0xff, 0xff);

const PRIMARY_BG: [f32; 4] = theme::rgba(0x39, 0x87, 0xe5, 0.24);
const PRIMARY_BORDER: [f32; 4] = theme::rgba(0x58, 0x9d, 0xf6, 0.54);

const OFFLINE_ROSE: [f32; 4] = theme::rgb(0xd8, 0x7d, 0x84);
const OFFLINE_BG: [f32; 4] = theme::rgba(0xd8, 0x7d, 0x84, 0.12);
const OFFLINE_BORDER: [f32; 4] = theme::rgba(0xd8, 0x7d, 0x84, 0.32);

const KILL_BG: [f32; 4] = theme::rgba(0xff, 0x71, 0x68, 0.15);
const KILL_BORDER: [f32; 4] = theme::rgba(0xff, 0x71, 0x68, 0.44);

const USER_ROLE_FG: [f32; 4] = theme::rgb(0x88, 0xc0, 0xff);
const USER_ROLE_BG: [f32; 4] = theme::rgba(0x58, 0x9d, 0xf6, 0.16);
const USER_ROLE_BORDER: [f32; 4] = theme::rgba(0x58, 0x9d, 0xf6, 0.34);

const TOOL_ROLE_FG: [f32; 4] = theme::rgb(0xe8, 0xb0, 0x4b);
const TOOL_ROLE_BG: [f32; 4] = theme::rgba(0xe8, 0xb0, 0x4b, 0.14);
const TOOL_ROLE_BORDER: [f32; 4] = theme::rgba(0xe8, 0xb0, 0x4b, 0.32);

const MAX_ANCESTOR_DEPTH: usize = 32;
const TRANSCRIPT_TAIL_LIMIT: usize = 16;

// =============================================================================
// Core Data Model
// =============================================================================

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentStatus {
    /// Actively burning CPU or interactive in-shell running session (vivid emerald).
    Running,
    /// Live local process currently idle / waiting for user input (warm amber).
    Waiting,
    /// Configured local template or remote SSH server target (muted slate).
    #[default]
    Idle,
    Exited(i32),
}

/// One discovered or managed agent session on either the local laptop or a remote SSH server.
#[derive(Clone, Debug, Default)]
pub struct AgentRecord {
    pub id: String,
    pub host: String,
    pub is_remote: bool,
    pub name: String,
    /// Short semantic kind tag: `"claude"`, `"codex"`, `"harness"`, `"server"`, or `"local"`.
    pub kind_label: String,
    /// Concise high-signal subtitle (`Projects/blaze · 19m · in Ghostty` or `andrew@hil-compute-1`).
    pub prompt_summary: String,
    /// Right-aligned compact metric badge (`18%`, `524 MiB`, `12ms`, etc.).
    pub metric_badge: String,
    pub cwd: String,
    pub pid: Option<u32>,
    pub status: AgentStatus,
    /// Optional block/session index if launched directly inside `blaze-shell`.
    pub pty_session_idx: Option<usize>,
    /// Fallback structured telemetry / session preview lines shown when no live transcript exists.
    pub preview_lines: Vec<String>,
    /// Canonical SSH server connection fields (populated when `is_remote == true`).
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub aliases: Vec<String>,
    pub reachability: Option<ServerReachability>,
    /// Host macOS application metadata (`Ghostty`, `Antigravity`, etc.) for local agents.
    pub origin_app_name: String,
    pub origin_app_icon_path: Option<String>,
    /// Live native telemetry counters (`blaze-sysmon` / `sysmon_driver`).
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub threads: u32,
    pub uptime_secs: u64,
    pub cmdline: String,
    /// Lazily populated live transcript tail (never read per frame).
    pub transcript: Vec<TranscriptEntry>,
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

/// Interactive multi-server Agent Fleet Hub state (`agent` command).
pub struct AgentHub {
    pub local_host: String,
    /// All discovered local agents and deduplicated remote SSH servers.
    pub all_agents: Vec<AgentRecord>,
    /// Visible ordered agents shown in the sidebar (offline servers hidden unless `show_offline`).
    pub agents: Vec<AgentRecord>,
    pub selected: usize,
    /// Resizable left sidebar ratio (`0.18..=0.68`, draggable with `NSCursor::resizeLeftRightCursor`).
    pub sidebar_ratio: f32,
    pub composing_new: bool,
    pub new_prompt: String,
    /// When `false` (default), offline/unreachable SSH servers are hidden behind the `Offline: N` pill.
    pub show_offline: bool,
    pub offline_server_count: usize,
    pub online_server_count: usize,
    pub probing_server_count: usize,
    /// Pending inline kill confirmation `(pid, display_name)` for a local agent.
    pub confirm_kill: Option<(u32, String)>,
    /// Transient feedback banner (e.g. after sending `SIGTERM`/`SIGKILL` or re-probing).
    pub status_msg: Option<String>,
    /// Scroll viewports for both the left fleet sidebar and the right live preview card.
    pub sidebar_scroll: ScrollState,
    pub preview_scroll: ScrollState,
    pub last_home: String,
    pub last_cwd: String,
    pending_ensure_selected: bool,
    selected_transcript: Option<(String, Vec<TranscriptEntry>)>,
    sysmon: SysMonEngine<'static>,
}

impl AgentHub {
    pub fn open(local_host: &str, home: &str, cwd: &str, live_sessions: &[AgentRecord]) -> Self {
        let mut hub = Self {
            local_host: local_host.to_owned(),
            all_agents: Vec::new(),
            agents: Vec::new(),
            selected: 0,
            sidebar_ratio: 0.36,
            composing_new: false,
            new_prompt: String::new(),
            show_offline: false,
            offline_server_count: 0,
            online_server_count: 0,
            probing_server_count: 0,
            confirm_kill: None,
            status_msg: None,
            sidebar_scroll: ScrollState::new(),
            preview_scroll: ScrollState::new(),
            last_home: home.to_owned(),
            last_cwd: cwd.to_owned(),
            pending_ensure_selected: true,
            selected_transcript: None,
            sysmon: SysMonEngine::new_in(Alloc::global()),
        };
        hub.refresh(home, cwd, live_sessions);
        hub
    }

    /// Construct a hub directly from pre-built records (for deterministic unit/layout tests).
    pub fn from_records(local_host: &str, records: Vec<AgentRecord>) -> Self {
        let mut hub = Self {
            local_host: local_host.to_owned(),
            all_agents: records,
            agents: Vec::new(),
            selected: 0,
            sidebar_ratio: 0.36,
            composing_new: false,
            new_prompt: String::new(),
            show_offline: false,
            offline_server_count: 0,
            online_server_count: 0,
            probing_server_count: 0,
            confirm_kill: None,
            status_msg: None,
            sidebar_scroll: ScrollState::new(),
            preview_scroll: ScrollState::new(),
            last_home: String::new(),
            last_cwd: String::new(),
            pending_ensure_selected: true,
            selected_transcript: None,
            sysmon: SysMonEngine::new_in(Alloc::global()),
        };
        hub.rebuild_visible();
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

    /// Scroll either the left fleet list or the right preview card depending on pointer `x`
    /// (relative to the pane's left edge) and `pane_w`.
    pub fn scroll_at(&mut self, x: f32, pane_w: f32, delta_px: f32) -> bool {
        let split_x = compute_split_x(pane_w, self.sidebar_ratio);
        if x < split_x {
            self.sidebar_scroll.scroll_by(delta_px)
        } else {
            self.preview_scroll.scroll_by(delta_px)
        }
    }

    /// Select an explicit visible row index, resetting preview scroll and lazily loading its transcript.
    pub fn select_index(&mut self, idx: usize) -> bool {
        if idx >= self.agents.len() {
            return false;
        }
        let changed = self.selected != idx || self.confirm_kill.is_some();
        self.selected = idx;
        self.confirm_kill = None;
        self.preview_scroll.offset_y = 0.0;
        self.pending_ensure_selected = true;
        self.ensure_selected_transcript();
        changed
    }

    /// Open the inline `n` task prompt composer for the currently selected agent/server.
    pub fn start_compose_new(&mut self) {
        self.confirm_kill = None;
        self.composing_new = true;
        self.new_prompt.clear();
    }

    /// Build the [`AgentHubAction::EnterAgent`] payload for the currently selected record, if any.
    pub fn attach_selected_action(&self) -> Option<AgentHubAction> {
        let a = self.agents.get(self.selected)?;
        Some(AgentHubAction::EnterAgent {
            host: a.host.clone(),
            is_remote: a.is_remote,
            cwd: a.cwd.clone(),
            prompt: None,
            pty_session_idx: a.pty_session_idx,
        })
    }

    /// Discover live local agents via zero-subprocess `sysmon_driver` + `SysMonEngine`, and
    /// deduplicate SSH servers from `~/.ssh/config` via sans-IO `blaze-ssh` + `agent_probe`.
    pub fn refresh(&mut self, home: &str, cwd: &str, live_sessions: &[AgentRecord]) {
        self.last_home = home.to_owned();
        self.last_cwd = cwd.to_owned();
        self.selected_transcript = None;

        let alloc = Alloc::global();
        let snap = crate::sysmon_driver::sample_snapshot(alloc);
        self.sysmon.ingest(snap);
        let wall_time_secs = self.sysmon.wall_time_secs();
        let own_pid = std::process::id();

        // Index every process's `(ppid, app_bundle_path)` for O(depth) ancestor `.app` resolution.
        let mut pid_ancestry: HashMap<u32, (u32, Option<String>)> =
            HashMap::with_capacity(self.sysmon.rows().len());
        let mut pid_row_idx: HashMap<u32, usize> =
            HashMap::with_capacity(self.sysmon.rows().len());
        for (i, row) in self.sysmon.rows().iter().enumerate() {
            let bundle = row
                .app_bundle_path
                .as_ref()
                .map(|p| p.as_str().to_owned())
                .or_else(|| extract_app_bundle_path(row.exe_path.as_str()).map(str::to_owned));
            pid_ancestry.insert(row.pid, (row.ppid, bundle));
            pid_row_idx.insert(row.pid, i);
        }

        // 1. Seed with in-shell managed agent sessions, enriched with live native telemetry.
        let mut discovered: Vec<AgentRecord> = Vec::new();
        for sess in live_sessions {
            let mut rec = sess.clone();
            if rec.kind_label.is_empty() {
                rec.kind_label = if rec.is_remote {
                    "server".to_owned()
                } else {
                    "claude".to_owned()
                };
            }
            if let Some(pid) = rec.pid
                && let Some(&ridx) = pid_row_idx.get(&pid)
            {
                let row = &self.sysmon.rows()[ridx];
                rec.cpu_pct = row.cpu_pct;
                rec.mem_bytes = row.mem_bytes;
                rec.threads = row.threads;
                rec.uptime_secs = if row.start_time_secs > 0 {
                    wall_time_secs.saturating_sub(row.start_time_secs)
                } else {
                    0
                };
                if !row.cwd.is_empty() {
                    rec.cwd = row.cwd.as_str().to_owned();
                }
                if rec.origin_app_name.is_empty() {
                    let (app_name, app_icon) = resolve_origin_app(pid, &pid_ancestry);
                    rec.origin_app_name = app_name;
                    rec.origin_app_icon_path = app_icon;
                }
            }
            discovered.push(rec);
        }

        // 2. Discover external live local agents (`claude`, `codex`, `harness` / `blaze-agent`)
        //    directly from `SysMonEngine::rows()` + `read_proc_cmdline(pid)` (zero `ps`/`lsof`!).
        for row in self.sysmon.rows() {
            if row.pid == own_pid || discovered.iter().any(|a| a.pid == Some(row.pid)) {
                continue;
            }
            let name_lower = row.name.as_str().to_ascii_lowercase();
            let exe_lower = row.exe_path.as_str().to_ascii_lowercase();
            if !is_agent_candidate(&name_lower, &exe_lower) {
                continue;
            }
            let raw_cmd = crate::sysmon_driver::read_proc_cmdline(row.pid);
            let Some(kind) = classify_agent_kind(&name_lower, &exe_lower, &raw_cmd) else {
                continue;
            };

            let real_cwd = if !row.cwd.is_empty() {
                row.cwd.as_str().to_owned()
            } else {
                cwd.to_owned()
            };
            let short_cwd = shorten_agent_path(&real_cwd, home);
            let folder = Path::new(&real_cwd)
                .file_name()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("~");

            let uptime_secs = if row.start_time_secs > 0 {
                wall_time_secs.saturating_sub(row.start_time_secs)
            } else {
                0
            };
            let (origin_app_name, origin_app_icon_path) =
                resolve_origin_app(row.pid, &pid_ancestry);

            let is_active = row.cpu_pct >= 1.5;
            let status = if is_active {
                AgentStatus::Running
            } else {
                AgentStatus::Waiting
            };
            let uptime_str = format_uptime_compact(uptime_secs);
            let mem_str = if row.mem_bytes > 0 {
                format_iec_bytes(row.mem_bytes)
            } else {
                String::new()
            };
            let metric_badge = if is_active {
                format!("{:.0}%", row.cpu_pct.max(1.0))
            } else if !mem_str.is_empty() {
                mem_str.clone()
            } else {
                uptime_str.clone()
            };

            let mut summary_parts: Vec<String> = Vec::with_capacity(3);
            summary_parts.push(short_cwd.clone());
            if !uptime_str.is_empty() {
                summary_parts.push(uptime_str.clone());
            }
            if !origin_app_name.is_empty() {
                summary_parts.push(format!("in {origin_app_name}"));
            }
            let prompt_summary = summary_parts.join(" · ");

            let effective_cmd = if !raw_cmd.is_empty() {
                abbreviate_cmd_paths(&raw_cmd, home)
            } else {
                abbreviate_cmd_paths(row.exe_path.as_str(), home)
            };

            discovered.push(AgentRecord {
                id: format!("local-{}", row.pid),
                host: self.local_host.clone(),
                is_remote: false,
                name: folder.to_owned(),
                kind_label: kind.to_owned(),
                prompt_summary,
                metric_badge,
                cwd: real_cwd,
                pid: Some(row.pid),
                status,
                pty_session_idx: None,
                preview_lines: vec![
                    format!("state:  {}", if is_active { "working (active CPU)" } else { "waiting for input" }),
                    format!("cwd:    {short_cwd}"),
                    format!("host:   {} (local) · PID {}", self.local_host, row.pid),
                    format!(
                        "usage:  {:.1}% CPU · {} · {} thr · up {}",
                        row.cpu_pct,
                        if mem_str.is_empty() { "0 B" } else { &mem_str },
                        row.threads,
                        format_uptime(uptime_secs),
                    ),
                    format!("cmd:    {effective_cmd}"),
                ],
                hostname: String::new(),
                user: String::new(),
                port: 0,
                aliases: Vec::new(),
                reachability: None,
                origin_app_name,
                origin_app_icon_path,
                cpu_pct: row.cpu_pct,
                mem_bytes: row.mem_bytes,
                threads: row.threads,
                uptime_secs,
                cmdline: effective_cmd,
                transcript: Vec::new(),
            });
        }

        // 3. Always include a quick-start local workspace agent entry if no local agents exist.
        if !discovered.iter().any(|a| !a.is_remote) {
            let short_cwd = shorten_agent_path(cwd, home);
            let folder = Path::new(cwd)
                .file_name()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("workspace");
            discovered.push(AgentRecord {
                id: "local-new".to_owned(),
                host: self.local_host.clone(),
                is_remote: false,
                name: folder.to_owned(),
                kind_label: "local".to_owned(),
                prompt_summary: format!("{short_cwd} · start new session"),
                metric_badge: "ready".to_owned(),
                cwd: cwd.to_owned(),
                pid: None,
                status: AgentStatus::Idle,
                pty_session_idx: None,
                preview_lines: vec![
                    format!("host:   {} (local)", self.local_host),
                    format!("cwd:    {short_cwd}"),
                    String::new(),
                    "Press Enter to launch an interactive agent in this directory, or n to give a task prompt."
                        .to_owned(),
                ],
                ..AgentRecord::default()
            });
        }

        // 4. Discover remote servers from `~/.ssh/config` via sans-IO `blaze-ssh`,
        //    deduplicating aliases that share `(user, hostname, port)` and querying O(1) reachability!
        let ssh_path = PathBuf::from(home).join(".ssh/config");
        if let Ok(raw) = fs::read_to_string(&ssh_path) {
            let cfg = blaze_ssh::parse_config(&raw, blaze_ssh::ConfigLimits::SHIPPED, alloc);
            struct SshGroup {
                user: String,
                hostname: String,
                port: u16,
                aliases: Vec<String>,
            }
            let mut groups: Vec<SshGroup> = Vec::new();
            let mut key_to_idx: HashMap<(String, String, u16), usize> = HashMap::new();

            for h in cfg.list_hosts(128, alloc).iter() {
                let alias = h.alias.as_str();
                if alias.is_empty() {
                    continue;
                }
                let target_hostname = h.hostname.as_ref().map(|s| s.as_str()).unwrap_or(alias);
                let user = h.user.as_ref().map(|s| s.as_str()).unwrap_or("root");
                let port = h.port.unwrap_or(22);
                let key = (
                    user.to_ascii_lowercase(),
                    target_hostname.to_ascii_lowercase(),
                    port,
                );
                if let Some(&gidx) = key_to_idx.get(&key) {
                    if !groups[gidx].aliases.iter().any(|a| a == alias) {
                        groups[gidx].aliases.push(alias.to_owned());
                    }
                } else {
                    let gidx = groups.len();
                    key_to_idx.insert(key, gidx);
                    groups.push(SshGroup {
                        user: user.to_owned(),
                        hostname: target_hostname.to_owned(),
                        port,
                        aliases: vec![alias.to_owned()],
                    });
                }
            }

            for group in groups {
                // Keep the longest / most descriptive alias (e.g. `dev-aws-1` over `da1`) as primary.
                let primary_idx = group
                    .aliases
                    .iter()
                    .enumerate()
                    .max_by(|(_, a), (_, b)| {
                        a.len()
                            .cmp(&b.len())
                            .then_with(|| a.contains('-').cmp(&b.contains('-')))
                            .then_with(|| b.cmp(a))
                    })
                    .map_or(0, |(idx, _)| idx);
                let primary = group.aliases[primary_idx].clone();
                if discovered.iter().any(|a| a.is_remote && a.host == primary) {
                    continue;
                }
                let mut secondary: Vec<String> = group
                    .aliases
                    .into_iter()
                    .enumerate()
                    .filter(|(i, _)| *i != primary_idx)
                    .map(|(_, a)| a)
                    .collect();
                secondary.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));

                let reach = lookup_or_probe_server(&group.hostname, group.port);
                let short_target = strip_tailscale_suffix(&group.hostname);
                let port_s = if group.port == 22 {
                    String::new()
                } else {
                    format!(":{}", group.port)
                };

                discovered.push(AgentRecord {
                    id: format!("ssh-{primary}"),
                    host: primary.clone(),
                    is_remote: true,
                    name: primary,
                    kind_label: "server".to_owned(),
                    prompt_summary: format!("{}@{short_target}{port_s}", group.user),
                    metric_badge: reachability_badge(reach),
                    cwd: "~".to_owned(),
                    pid: None,
                    status: AgentStatus::Idle,
                    pty_session_idx: None,
                    preview_lines: vec![
                        format!("target: {}@{}:{}", group.user, group.hostname, group.port),
                        "proto:  blaze-ssh (BSH1 / SSH)".to_owned(),
                        String::new(),
                        "Press Enter to open an interactive remote agent, or n to dispatch a prompted task."
                            .to_owned(),
                    ],
                    hostname: group.hostname,
                    user: group.user,
                    port: group.port,
                    aliases: secondary,
                    reachability: Some(reach),
                    ..AgentRecord::default()
                });
            }
        }

        self.all_agents = discovered;
        self.rebuild_visible();
        self.pending_ensure_selected = true;
    }

    /// Re-poll cached reachability for every remote server (O(1) non-blocking).
    /// Rebuilds visible ordering while preserving the currently selected agent by `id`.
    /// Returns `true` if any reachability state changed.
    pub fn sync_reachability(&mut self) -> bool {
        let mut changed = false;
        for ag in &mut self.all_agents {
            if !ag.is_remote {
                continue;
            }
            let target = if !ag.hostname.is_empty() {
                ag.hostname.as_str()
            } else {
                ag.host.as_str()
            };
            let port = if ag.port == 0 { 22 } else { ag.port };
            let next = lookup_or_probe_server(target, port);
            if ag.reachability != Some(next) {
                ag.reachability = Some(next);
                ag.metric_badge = reachability_badge(next);
                changed = true;
            }
        }
        if changed {
            self.rebuild_visible();
        }
        changed
    }

    /// Toggle whether unreachable/offline SSH servers are included in the visible sidebar list.
    pub fn toggle_show_offline(&mut self) {
        self.show_offline = !self.show_offline;
        self.rebuild_visible();
        self.pending_ensure_selected = true;
    }

    /// Invalidate cached server reachability results and kick off fresh background TCP probes.
    pub fn reprobe_servers(&mut self) {
        invalidate_probe_cache();
        let _ = self.sync_reachability();
        self.status_msg = Some("Re-probing SSH servers…".to_owned());
    }

    /// Enter inline kill confirmation for the currently selected local live agent process.
    pub fn begin_kill_selected(&mut self) {
        if let Some(ag) = self.agents.get(self.selected)
            && !ag.is_remote
            && let Some(pid) = ag.pid
        {
            let label = if !ag.kind_label.is_empty() && ag.kind_label != "local" {
                format!("{} · {}", ag.kind_label, ag.name)
            } else {
                ag.name.clone()
            };
            self.confirm_kill = Some((pid, label));
            self.status_msg = None;
        }
    }

    /// Dismiss pending inline kill confirmation.
    pub fn cancel_kill(&mut self) {
        self.confirm_kill = None;
    }

    /// Send `SIGTERM` (`sigkill == false`) or `SIGKILL` (`sigkill == true`) to the pending target.
    pub fn confirm_kill_selected(&mut self, sigkill: bool) -> bool {
        let Some((pid, name)) = self.confirm_kill.take() else {
            return false;
        };
        let sig_name = if sigkill { "SIGKILL" } else { "SIGTERM" };
        match crate::sysmon_driver::send_process_signal(pid, sigkill) {
            Ok(()) => {
                self.status_msg = Some(format!("Sent {sig_name} to {name} (PID {pid})"));
                self.all_agents.retain(|a| a.pid != Some(pid));
                self.selected_transcript = None;
                self.rebuild_visible();
                self.pending_ensure_selected = true;
                true
            }
            Err(err) => {
                self.status_msg = Some(format!("Failed {sig_name} {name} (PID {pid}): {err}"));
                false
            }
        }
    }

    fn rebuild_visible(&mut self) {
        let prev_id = self.agents.get(self.selected).map(|a| a.id.clone());

        let mut local: Vec<AgentRecord> = Vec::new();
        let mut online: Vec<AgentRecord> = Vec::new();
        let mut probing: Vec<AgentRecord> = Vec::new();
        let mut offline: Vec<AgentRecord> = Vec::new();

        for ag in &self.all_agents {
            if !ag.is_remote {
                local.push(ag.clone());
            } else {
                match ag.reachability {
                    Some(ServerReachability::Online { .. }) => online.push(ag.clone()),
                    Some(ServerReachability::Probing) | None => probing.push(ag.clone()),
                    Some(ServerReachability::Offline) => offline.push(ag.clone()),
                }
            }
        }

        // 1. Local agents: Running -> Waiting -> Idle -> Exited, then CPU% desc, then RAM desc.
        local.sort_by(|a, b| {
            status_rank(a.status)
                .cmp(&status_rank(b.status))
                .then_with(|| b.cpu_pct.total_cmp(&a.cpu_pct))
                .then_with(|| b.mem_bytes.cmp(&a.mem_bytes))
                .then_with(|| a.name.cmp(&b.name))
        });

        // 2. Confirmed online servers sorted by lowest RTT first, then alphabetically.
        online.sort_by(|a, b| {
            reach_sort_key(a.reachability)
                .cmp(&reach_sort_key(b.reachability))
                .then_with(|| a.name.cmp(&b.name))
        });

        // 3. Probing & offline servers alphabetically.
        probing.sort_by(|a, b| a.name.cmp(&b.name));
        offline.sort_by(|a, b| a.name.cmp(&b.name));

        self.online_server_count = online.len();
        self.probing_server_count = probing.len();
        self.offline_server_count = offline.len();

        // Hide unreachable and not-yet-confirmed servers by default so only reachable hosts
        // appear under ONLINE SERVERS (no flashing offline hosts during initial 650ms probes).
        let extra = if self.show_offline { probing.len() + offline.len() } else { 0 };
        let mut visible = Vec::with_capacity(local.len() + online.len() + extra);
        visible.extend(local);
        visible.extend(online);
        if self.show_offline {
            visible.extend(probing);
            visible.extend(offline);
        }

        self.agents = visible;
        if let Some(id) = prev_id {
            if let Some(pos) = self.agents.iter().position(|a| a.id == id) {
                self.selected = pos;
            } else if self.selected >= self.agents.len() {
                self.selected = self.agents.len().saturating_sub(1);
            }
        } else if self.selected >= self.agents.len() {
            self.selected = self.agents.len().saturating_sub(1);
        }

        self.ensure_selected_transcript();
    }

    fn move_selection(&mut self, delta: isize) {
        if self.agents.is_empty() {
            self.selected = 0;
            return;
        }
        let max_idx = (self.agents.len() - 1) as isize;
        let next = (self.selected as isize + delta).clamp(0, max_idx) as usize;
        if next != self.selected {
            self.selected = next;
            self.confirm_kill = None;
            self.preview_scroll.offset_y = 0.0;
            self.pending_ensure_selected = true;
            self.ensure_selected_transcript();
        }
    }

    /// Lazily load and cache the transcript tail for the selected agent ID (zero per-frame disk I/O).
    fn ensure_selected_transcript(&mut self) {
        let Some(ag) = self.agents.get_mut(self.selected) else {
            self.selected_transcript = None;
            return;
        };
        if self
            .selected_transcript
            .as_ref()
            .is_some_and(|(id, _)| id == &ag.id)
        {
            return;
        }

        let entries = if !ag.is_remote
            && !ag.cwd.is_empty()
            && !self.last_home.is_empty()
            && matches!(ag.kind_label.as_str(), "claude" | "codex" | "harness")
        {
            read_agent_transcript_tail(
                &ag.kind_label,
                &ag.cwd,
                &self.last_home,
                TRANSCRIPT_TAIL_LIMIT,
            )
        } else {
            ag.transcript.clone()
        };

        ag.transcript = entries.clone();
        self.selected_transcript = Some((ag.id.clone(), entries));
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

        if self.confirm_kill.is_some() {
            match (key, ch) {
                ("Enter", _) | (_, Some('y' | 'Y')) => {
                    self.confirm_kill_selected(false);
                }
                (_, Some('K')) => {
                    self.confirm_kill_selected(true);
                }
                _ => {
                    self.cancel_kill();
                }
            }
            return AgentHubAction::Continue;
        }

        match key {
            "Escape" => return AgentHubAction::Exit,
            "Up" => {
                self.move_selection(-1);
                return AgentHubAction::Continue;
            }
            "Down" => {
                self.move_selection(1);
                return AgentHubAction::Continue;
            }
            "PageUp" => {
                self.move_selection(-8);
                return AgentHubAction::Continue;
            }
            "PageDown" => {
                self.move_selection(8);
                return AgentHubAction::Continue;
            }
            "Enter" => {
                return self
                    .attach_selected_action()
                    .unwrap_or(AgentHubAction::Continue);
            }
            _ => {}
        }

        match ch {
            Some('q') => AgentHubAction::Exit,
            Some('j') => {
                self.move_selection(1);
                AgentHubAction::Continue
            }
            Some('k') => {
                self.move_selection(-1);
                AgentHubAction::Continue
            }
            Some('n') => {
                self.start_compose_new();
                AgentHubAction::Continue
            }
            Some('o') => {
                self.toggle_show_offline();
                AgentHubAction::Continue
            }
            Some('r') => {
                self.reprobe_servers();
                AgentHubAction::Continue
            }
            Some('x') => {
                self.begin_kill_selected();
                AgentHubAction::Continue
            }
            _ => AgentHubAction::Continue,
        }
    }
}

// =============================================================================
// Process Classification, Ancestor App Walk & Formatting Helpers
// =============================================================================

fn status_rank(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Running => 0,
        AgentStatus::Waiting => 1,
        AgentStatus::Idle => 2,
        AgentStatus::Exited(_) => 3,
    }
}

fn reach_sort_key(reach: Option<ServerReachability>) -> (u8, u32) {
    match reach {
        Some(ServerReachability::Online { rtt_ms }) => (0, rtt_ms),
        Some(ServerReachability::Probing) | None => (1, u32::MAX),
        Some(ServerReachability::Offline) => (2, u32::MAX),
    }
}

fn reachability_badge(reach: ServerReachability) -> String {
    match reach {
        ServerReachability::Online { rtt_ms } => format!("{rtt_ms}ms"),
        ServerReachability::Probing => "…".to_owned(),
        ServerReachability::Offline => "offline".to_owned(),
    }
}

/// Fast pre-filter before calling `read_proc_cmdline(pid)` so we only query `KERN_PROCARGS2`
/// on plausible agent binaries or JS/TS runtime hosts (`node`, `bun`, `deno`).
fn is_agent_candidate(name_lower: &str, exe_lower: &str) -> bool {
    if exe_lower.contains("claude.app/contents/") || exe_lower.contains("codex.app/contents/") {
        return false;
    }
    if name_lower.contains("helper") && exe_lower.contains(".app/contents/") {
        return false;
    }
    name_lower == "claude"
        || name_lower == "codex"
        || name_lower.contains("harness-native")
        || name_lower.contains("blaze-agent")
        || exe_lower.ends_with("/claude")
        || exe_lower.ends_with("/codex")
        || exe_lower.contains("/claude/")
        || exe_lower.contains("/codex/")
        || exe_lower.contains("/claude-code")
        || exe_lower.contains("harness-native")
        || exe_lower.contains("blaze-agent")
        || matches!(name_lower, "node" | "bun" | "deno")
}

fn classify_agent_kind(
    name_lower: &str,
    exe_lower: &str,
    cmdline: &str,
) -> Option<&'static str> {
    if exe_lower.contains("claude.app/contents/") || exe_lower.contains("codex.app/contents/") {
        return None;
    }
    let cmd_lower = cmdline.to_ascii_lowercase();
    if name_lower.contains("harness-native")
        || name_lower.contains("blaze-agent")
        || exe_lower.contains("harness-native")
        || exe_lower.contains("blaze-agent")
        || cmd_lower.contains("harness-native")
        || cmd_lower.contains("blaze-agent")
    {
        return Some("harness");
    }
    if name_lower == "claude"
        || exe_lower.ends_with("/claude")
        || exe_lower.contains("/claude/")
        || exe_lower.contains("/claude-code")
        || cmd_matches_cli_agent(&cmd_lower, "claude")
        || cmd_lower.contains("claude-code")
    {
        return Some("claude");
    }
    if name_lower == "codex"
        || exe_lower.ends_with("/codex")
        || exe_lower.contains("/codex/")
        || cmd_matches_cli_agent(&cmd_lower, "codex")
    {
        return Some("codex");
    }
    None
}

fn cmd_matches_cli_agent(cmd_lower: &str, target: &str) -> bool {
    let mut parts = cmd_lower.split_whitespace();
    let Some(first) = parts.next() else {
        return false;
    };
    let first_base = first.rsplit('/').next().unwrap_or(first);
    if first_base == target {
        return true;
    }
    if matches!(first_base, "node" | "bun" | "deno") {
        let js_name = format!("{target}.js");
        let mjs_name = format!("{target}.mjs");
        for tok in parts.take(4) {
            let base = tok.rsplit('/').next().unwrap_or(tok);
            if base == target || base == js_name || base == mjs_name {
                return true;
            }
        }
    }
    false
}

/// Walk `ppid` ancestors (bounded to 32 levels) to attribute a CLI agent to its host `.app`
/// (`Ghostty`, `Antigravity`, `Terminal`, `iTerm`, `Cursor`, etc.) and resolve its cached icon PNG.
fn resolve_origin_app(
    pid: u32,
    ancestry: &HashMap<u32, (u32, Option<String>)>,
) -> (String, Option<String>) {
    let mut visited = [0u32; MAX_ANCESTOR_DEPTH + 1];
    visited[0] = pid;
    let mut visited_len = 1usize;

    let mut cur = ancestry.get(&pid).map_or(0, |(ppid, _)| *ppid);
    for _ in 0..MAX_ANCESTOR_DEPTH {
        if cur <= 1 || visited[..visited_len].contains(&cur) {
            break;
        }
        visited[visited_len] = cur;
        visited_len += 1;

        let Some((next_ppid, bundle)) = ancestry.get(&cur) else {
            break;
        };
        if let Some(bundle_path) = bundle.as_deref() {
            let name = app_display_name(bundle_path).to_owned();
            let icon = crate::sysmon_driver::lookup_or_cache_app_icon(bundle_path);
            return (name, icon);
        }
        cur = *next_ppid;
    }

    if let Some((_, Some(self_bundle))) = ancestry.get(&pid) {
        let name = app_display_name(self_bundle).to_owned();
        let icon = crate::sysmon_driver::lookup_or_cache_app_icon(self_bundle);
        return (name, icon);
    }

    (String::new(), None)
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

fn format_uptime_compact(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        let h = secs / 3_600;
        let m = (secs % 3_600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    } else {
        let d = secs / 86_400;
        let h = (secs % 86_400) / 3_600;
        if h == 0 {
            format!("{d}d")
        } else {
            format!("{d}d{h}h")
        }
    }
}

fn compute_split_x(pane_w: f32, ratio: f32) -> f32 {
    let vw = pane_w.max(240.0);
    let split_max = (vw - 180.0).max(160.0);
    let split_min = 170.0f32.min(split_max);
    (vw * ratio).clamp(split_min, split_max)
}

fn pill_button_w(label: &str, bold: bool, m: CellMetrics) -> f32 {
    m.measure_styled(label, TextFace::UiSmall, bold) + 16.0
}

fn pill_button_h(m: CellMetrics) -> f32 {
    m.line_h(TextFace::UiSmall) + 7.0
}

fn kind_chip_palette(kind: &str) -> ([f32; 4], [f32; 4], [f32; 4]) {
    match kind {
        "claude" => (CLAUDE_FG, CLAUDE_BG, CLAUDE_BORDER),
        "codex" => (CODEX_FG, CODEX_BG, CODEX_BORDER),
        "harness" => (HARNESS_FG, HARNESS_BG, HARNESS_BORDER),
        _ => (theme::INK_2, theme::BG_CHIP, theme::LINE),
    }
}

/// Draw a compact non-interactive rounded chip badge (`claude`, `codex`, `da1`, `12ms`, `offline`).
/// Returns the chip width in logical pixels.
#[allow(clippy::too_many_arguments)]
fn draw_compact_chip(
    scene: &mut Scene,
    x0: f32,
    y_center: f32,
    label: &str,
    fg: [f32; 4],
    bg: [f32; 4],
    border: [f32; 4],
    bold: bool,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let text_h = m.line_h(TextFace::UiSmall);
    let h = (text_h + 3.0).max(15.0);
    let w = m.measure_styled(label, TextFace::UiSmall, bold) + 10.0;
    let y0 = y_center - h * 0.5;
    let bw = if border[3] > 0.0 { 1.0 } else { 0.0 };
    scene.round_rect(x0, y0, x0 + w, y0 + h, 4.0, bg, bw, border, clip_id);
    let ty = y0 + (h - text_h) * 0.5;
    scene.ui_small(x0 + 5.0, ty, label, fg, bold, false, clip_id);
    w
}

/// Word-wrap `text` to fit within `max_chars` columns, preserving explicit newlines.
fn wrap_text_lines(text: &str, max_chars: usize) -> Vec<String> {
    let limit = max_chars.max(12);
    let mut out = Vec::new();
    for raw_line in text.lines() {
        let trimmed = raw_line.trim_end();
        if trimmed.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cur_len = 0usize;
        for word in trimmed.split_whitespace() {
            let wlen = word.chars().count();
            if cur_len == 0 {
                if wlen <= limit {
                    cur.push_str(word);
                    cur_len = wlen;
                } else {
                    // Hard-break an extra-long token (e.g. file path or hash).
                    let mut chunk = String::new();
                    let mut clen = 0usize;
                    for ch in word.chars() {
                        if clen >= limit {
                            out.push(core::mem::take(&mut chunk));
                            clen = 0;
                        }
                        chunk.push(ch);
                        clen += 1;
                    }
                    cur = chunk;
                    cur_len = clen;
                }
            } else if cur_len + 1 + wlen <= limit {
                cur.push(' ');
                cur.push_str(word);
                cur_len += 1 + wlen;
            } else {
                out.push(core::mem::take(&mut cur));
                if wlen <= limit {
                    cur.push_str(word);
                    cur_len = wlen;
                } else {
                    let mut chunk = String::new();
                    let mut clen = 0usize;
                    for ch in word.chars() {
                        if clen >= limit {
                            out.push(core::mem::take(&mut chunk));
                            clen = 0;
                        }
                        chunk.push(ch);
                        clen += 1;
                    }
                    cur = chunk;
                    cur_len = clen;
                }
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

// =============================================================================
// Metal Scene Layout (`layout_agent_hub` / `layout_agent_hub_pane`)
// =============================================================================

/// Lay out the unified multi-server `agent` fleet overview in Metal, keeping the top tab bar live!
#[allow(clippy::too_many_arguments)]
pub fn layout_agent_hub(
    hub: &mut AgentHub,
    store: &BlockStore,
    prompt: &PromptInfo,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    zoom_pct: u16,
    inline_titlebar: bool,
) -> Scene {
    layout_agent_hub_inner(
        hub,
        store,
        prompt,
        viewport_w,
        viewport_h,
        m,
        zoom_pct,
        inline_titlebar,
        true,
    )
}

/// Lay out the unified multi-server `agent` fleet overview inside a tiled pane.
#[allow(clippy::too_many_arguments)]
pub fn layout_agent_hub_pane(
    hub: &mut AgentHub,
    store: &BlockStore,
    prompt: &PromptInfo,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    zoom_pct: u16,
) -> Scene {
    layout_agent_hub_inner(
        hub,
        store,
        prompt,
        viewport_w,
        viewport_h,
        m,
        zoom_pct,
        false,
        false,
    )
}

enum SidebarEntry {
    SectionHeader { label: &'static str, count: String },
    AgentRow { idx: usize },
    OfflineHelper { count: usize },
}

#[allow(clippy::too_many_arguments)]
fn layout_agent_hub_inner(
    hub: &mut AgentHub,
    store: &BlockStore,
    prompt: &PromptInfo,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    zoom_pct: u16,
    inline_titlebar: bool,
    draw_top_bar: bool,
) -> Scene {
    hub.ensure_selected_transcript();

    let mut scene = Scene::new();
    let vw = viewport_w.max(240.0);
    let vh = viewport_h.max(200.0);

    let header_h = (m.ch + 18.0).max(34.0);
    let footer_h = (m.ch + 12.0).max(26.0);
    let body_top = TOP_BAR_H + header_h;
    let body_bot = (vh - footer_h).max(body_top + 60.0);
    let body_h = (body_bot - body_top).max(1.0);

    // -------------------------------------------------------------------------
    // 1. Sub-header Bar (counts on left + `[Offline: N]` & `[↻ Probe]` pills on right)
    // -------------------------------------------------------------------------
    scene.round_rect(0.0, TOP_BAR_H, vw, body_top, 0.0, theme::BG_SURFACE, 0.0, [0.0; 4], 0);
    scene.line(0.0, body_top - 0.5, vw, body_top - 0.5, 1.0, theme::LINE, 0);

    let btn_h = pill_button_h(m);
    let btn_y0 = TOP_BAR_H + (header_h - btn_h) * 0.5;
    let mut hdr_rx = vw - 12.0;

    // Rightmost: `[↻ Probe]` pill
    let probe_lbl = "↻ Probe";
    let probe_w = pill_button_w(probe_lbl, true, m);
    if hdr_rx - probe_w > 110.0 {
        hdr_rx -= probe_w;
        draw_pill_button(
            &mut scene,
            hdr_rx,
            btn_y0,
            probe_lbl,
            theme::INK_2,
            theme::BG_CHIP,
            theme::LINE,
            true,
            m,
            0,
            HitAction::AgentReprobeServers,
        );
        hdr_rx -= 6.0;
    }

    // Left of Probe: `[Offline: N]` toggle pill
    let offline_lbl = format!("Offline: {}", hub.offline_server_count);
    let offline_w = pill_button_w(&offline_lbl, hub.show_offline, m);
    if hdr_rx - offline_w > 96.0 {
        hdr_rx -= offline_w;
        let (ofg, obg, oborder) = if hub.show_offline {
            (OFFLINE_ROSE, OFFLINE_BG, OFFLINE_BORDER)
        } else {
            (theme::INK_3, theme::BG_CHIP, theme::LINE)
        };
        draw_pill_button(
            &mut scene,
            hdr_rx,
            btn_y0,
            &offline_lbl,
            ofg,
            obg,
            oborder,
            hub.show_offline,
            m,
            0,
            HitAction::AgentToggleOffline,
        );
        hdr_rx -= 10.0;
    }

    let hdr_left_clip = scene.push_clip(0.0, TOP_BAR_H, hdr_rx.max(80.0), body_top, 0.0);
    let hty = TOP_BAR_H + (header_h - m.line_h(TextFace::Ui)) * 0.5;
    draw_icon(
        &mut scene,
        Icon::Server,
        14.0,
        TOP_BAR_H + (header_h - 13.0) * 0.5,
        13.0,
        SERVER_ICON_SKY,
        hdr_left_clip,
    );
    let active_n = hub
        .all_agents
        .iter()
        .filter(|a| !a.is_remote && a.status == AgentStatus::Running)
        .count();
    let waiting_n = hub
        .all_agents
        .iter()
        .filter(|a| !a.is_remote && a.status == AgentStatus::Waiting)
        .count();
    let hdr_lbl = if hub.probing_server_count > 0 {
        format!(
            "agents · {active_n} working · {waiting_n} waiting · {} online · +{} probing",
            hub.online_server_count, hub.probing_server_count
        )
    } else {
        format!(
            "agents · {active_n} working · {waiting_n} waiting · {} online",
            hub.online_server_count
        )
    };
    let max_hdr_chars = ((hdr_rx - 36.0) / (m.cw * 0.88).max(4.0)).floor().max(8.0) as usize;
    scene.ui_text(
        34.0,
        hty,
        truncate_chars(&hdr_lbl, max_hdr_chars),
        theme::INK_1,
        true,
        false,
        hdr_left_clip,
    );

    // -------------------------------------------------------------------------
    // 2. Resizable Two-Pane Divider (`NSCursor::resizeLeftRightCursor`)
    // -------------------------------------------------------------------------
    let split_x = compute_split_x(vw, hub.sidebar_ratio);
    scene.line(split_x, body_top, split_x, body_bot, 1.0, theme::LINE, 0);
    let mid_y = (body_top + body_bot) * 0.5;
    scene.round_rect(
        split_x - 1.5,
        mid_y - 18.0,
        split_x + 1.5,
        mid_y + 18.0,
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

    // -------------------------------------------------------------------------
    // 3. Left Sidebar: Sectioned, Scrollable Fleet List
    // -------------------------------------------------------------------------
    let left_clip = scene.push_clip(0.0, body_top, split_x - 1.0, body_bot, 0.0);
    let sec_h = m.line_h(TextFace::UiSmall) + 10.0;
    let row_h = (m.line_h(TextFace::Ui) + m.line_h(TextFace::UiSmall) + 16.0).max(46.0);
    let helper_h = m.line_h(TextFace::UiSmall) + 16.0;

    let mut local_indices: Vec<usize> = Vec::new();
    let mut online_indices: Vec<usize> = Vec::new();
    let mut offline_indices: Vec<usize> = Vec::new();
    for (idx, ag) in hub.agents.iter().enumerate() {
        if !ag.is_remote {
            local_indices.push(idx);
        } else if matches!(ag.reachability, Some(ServerReachability::Offline)) {
            offline_indices.push(idx);
        } else {
            online_indices.push(idx);
        }
    }

    // Build exact content-space geometry first so `set_bounds` & `ensure_visible` run before drawing.
    let mut items: Vec<(f32, f32, SidebarEntry)> = Vec::new();
    let mut cur_y = 6.0f32;
    let mut sel_bounds: Option<(f32, f32)> = None;

    if !local_indices.is_empty() {
        items.push((
            cur_y,
            cur_y + sec_h,
            SidebarEntry::SectionHeader {
                label: "LOCAL AGENTS",
                count: local_indices.len().to_string(),
            },
        ));
        cur_y += sec_h + 4.0;
        for idx in local_indices {
            let y0 = cur_y;
            let y1 = y0 + row_h;
            if idx == hub.selected {
                sel_bounds = Some((y0, y1));
            }
            items.push((y0, y1, SidebarEntry::AgentRow { idx }));
            cur_y = y1 + 4.0;
        }
        cur_y += 6.0;
    }

    if !online_indices.is_empty() {
        let badge = if hub.probing_server_count > 0 {
            format!("{} +{}…", hub.online_server_count, hub.probing_server_count)
        } else {
            online_indices.len().to_string()
        };
        items.push((
            cur_y,
            cur_y + sec_h,
            SidebarEntry::SectionHeader {
                label: "ONLINE SERVERS",
                count: badge,
            },
        ));
        cur_y += sec_h + 4.0;
        for idx in online_indices {
            let y0 = cur_y;
            let y1 = y0 + row_h;
            if idx == hub.selected {
                sel_bounds = Some((y0, y1));
            }
            items.push((y0, y1, SidebarEntry::AgentRow { idx }));
            cur_y = y1 + 4.0;
        }
        cur_y += 6.0;
    } else if hub.online_server_count == 0
        && hub.probing_server_count == 0
        && hub.offline_server_count > 0
        && !hub.show_offline
    {
        items.push((
            cur_y,
            cur_y + helper_h,
            SidebarEntry::OfflineHelper {
                count: hub.offline_server_count,
            },
        ));
        cur_y += helper_h + 6.0;
    }

    if hub.show_offline && !offline_indices.is_empty() {
        items.push((
            cur_y,
            cur_y + sec_h,
            SidebarEntry::SectionHeader {
                label: "OFFLINE SERVERS",
                count: offline_indices.len().to_string(),
            },
        ));
        cur_y += sec_h + 4.0;
        for idx in offline_indices {
            let y0 = cur_y;
            let y1 = y0 + row_h;
            if idx == hub.selected {
                sel_bounds = Some((y0, y1));
            }
            items.push((y0, y1, SidebarEntry::AgentRow { idx }));
            cur_y = y1 + 4.0;
        }
    }

    let sidebar_content_h = cur_y + 8.0;
    hub.sidebar_scroll.set_bounds(sidebar_content_h, body_h);
    if hub.pending_ensure_selected {
        if let Some((sy0, sy1)) = sel_bounds {
            hub.sidebar_scroll.ensure_visible(sy0, sy1, 10.0);
        }
        hub.pending_ensure_selected = false;
    }

    let left_scroll_y = hub.sidebar_scroll.offset_y;
    let row_x0 = 8.0;
    let row_x1 = if hub.sidebar_scroll.is_scrollable() {
        (split_x - 13.0).max(row_x0 + 60.0)
    } else {
        (split_x - 8.0).max(row_x0 + 60.0)
    };

    for (cy0, cy1, entry) in &items {
        let sy0 = body_top + *cy0 - left_scroll_y;
        let sy1 = body_top + *cy1 - left_scroll_y;
        if sy1 < body_top - 4.0 || sy0 > body_bot + 4.0 {
            continue;
        }

        match entry {
            SidebarEntry::SectionHeader { label, count } => {
                let _ = draw_section_header(
                    &mut scene,
                    row_x0 + 4.0,
                    row_x1 - 4.0,
                    sy0,
                    label,
                    Some(count.as_str()),
                    m,
                    left_clip,
                );
            }
            SidebarEntry::OfflineHelper { count } => {
                let rect = HitRect::new(row_x0, sy0, row_x1, sy1);
                scene.round_rect(
                    rect.x0,
                    rect.y0,
                    rect.x1,
                    rect.y1,
                    6.0,
                    theme::BG_SURFACE,
                    1.0,
                    theme::LINE_FAINT,
                    left_clip,
                );
                let msg = format!(
                    "All {count} SSH servers unreachable · click Offline or press o"
                );
                let max_c = ((row_x1 - row_x0 - 16.0) / (m.cw * 0.78).max(4.0))
                    .floor()
                    .max(12.0) as usize;
                let ty = sy0 + (helper_h - m.line_h(TextFace::UiSmall)) * 0.5;
                scene.ui_small(
                    row_x0 + 8.0,
                    ty,
                    truncate_chars(&msg, max_c),
                    theme::INK_3,
                    false,
                    false,
                    left_clip,
                );
                scene.push_hit_clipped(rect, left_clip, HitAction::AgentToggleOffline);
            }
            SidebarEntry::AgentRow { idx } => {
                let Some(ag) = hub.agents.get(*idx) else {
                    continue;
                };
                draw_sidebar_agent_row(
                    &mut scene,
                    ag,
                    *idx,
                    *idx == hub.selected,
                    row_x0,
                    sy0,
                    row_x1,
                    sy1,
                    prompt,
                    m,
                    left_clip,
                );
            }
        }
    }

    draw_scrollbar(
        &mut scene,
        hub.sidebar_scroll,
        split_x - 7.0,
        body_top + 6.0,
        split_x - 3.0,
        body_bot - 6.0,
        left_clip,
    );

    // -------------------------------------------------------------------------
    // 4. Right Pane: Rich Inspector + Kill Controls + Scrollable Live Preview
    // -------------------------------------------------------------------------
    let right_clip = scene.push_clip(split_x + 1.0, body_top, vw, body_bot, 0.0);
    let rx = split_x + 18.0;
    let right_edge = (vw - 18.0).max(rx + 120.0);
    let rw = (right_edge - rx).max(120.0);

    if let Some(ag) = hub.agents.get(hub.selected).cloned() {
        let mut cy = body_top + 14.0;

        // 4a. Inspector Header + Right-Aligned Action Toolbar
        let tool_h = pill_button_h(m);
        let hdr_row_h = tool_h.max(m.line_h(TextFace::Heading)).max(26.0);
        let tool_y0 = cy + (hdr_row_h - tool_h) * 0.5;
        let mut tool_x = right_edge;

        // Crimson `[Kill]` pill for live local agents
        if !ag.is_remote && ag.pid.is_some() {
            let lbl = "Kill";
            let bw = pill_button_w(lbl, true, m);
            if tool_x - bw > rx + 90.0 {
                tool_x -= bw;
                draw_pill_button(
                    &mut scene,
                    tool_x,
                    tool_y0,
                    lbl,
                    theme::DANGER,
                    KILL_BG,
                    KILL_BORDER,
                    true,
                    m,
                    right_clip,
                    HitAction::AgentRequestKill,
                );
                tool_x -= 6.0;
            }
        }

        // Neutral `[Task]` pill
        let task_lbl = "Task";
        let task_w = pill_button_w(task_lbl, false, m);
        if tool_x - task_w > rx + 80.0 {
            tool_x -= task_w;
            draw_pill_button(
                &mut scene,
                tool_x,
                tool_y0,
                task_lbl,
                theme::INK_2,
                theme::BG_CHIP,
                theme::LINE,
                false,
                m,
                right_clip,
                HitAction::AgentNewTaskSelected,
            );
            tool_x -= 6.0;
        }

        // Sky-blue primary `[Attach ↵]` pill
        let attach_lbl = "Attach ↵";
        let attach_w = pill_button_w(attach_lbl, true, m);
        if tool_x - attach_w > rx + 60.0 {
            tool_x -= attach_w;
            draw_pill_button(
                &mut scene,
                tool_x,
                tool_y0,
                attach_lbl,
                SERVER_WHITE,
                PRIMARY_BG,
                PRIMARY_BORDER,
                true,
                m,
                right_clip,
                HitAction::AgentAttachSelected,
            );
            tool_x -= 10.0;
        }

        // Left side of inspector header: icon + optional kind chip + bold title + status pill
        let title_clip = scene.push_clip(rx, cy - 2.0, tool_x.max(rx + 40.0), cy + hdr_row_h + 2.0, 0.0);
        let mut lx = rx;
        let icon_s = (m.ch * 0.95).clamp(15.0, 20.0);
        let icon_y = cy + (hdr_row_h - icon_s) * 0.5;
        if !ag.is_remote
            && let Some(icon_path) = ag.origin_app_icon_path.as_ref()
        {
            scene.image(lx, icon_y, icon_s, icon_s, icon_path.clone(), title_clip);
            lx += icon_s + 8.0;
        } else if ag.is_remote {
            let is_offline = matches!(ag.reachability, Some(ServerReachability::Offline));
            draw_icon(
                &mut scene,
                Icon::Server,
                lx,
                cy + (hdr_row_h - 15.0) * 0.5,
                15.0,
                if is_offline { theme::INK_4 } else { SERVER_ICON_SKY },
                title_clip,
            );
            lx += 21.0;
        } else {
            draw_icon(
                &mut scene,
                Icon::Laptop,
                lx,
                cy + (hdr_row_h - 15.0) * 0.5,
                15.0,
                theme::INK_2,
                title_clip,
            );
            lx += 21.0;
        }

        if !ag.is_remote && !ag.kind_label.is_empty() && ag.kind_label != "local" {
            let (fg, bg, border) = kind_chip_palette(&ag.kind_label);
            let cw = draw_compact_chip(
                &mut scene,
                lx,
                cy + hdr_row_h * 0.5,
                &ag.kind_label,
                fg,
                bg,
                border,
                true,
                m,
                title_clip,
            );
            lx += cw + 8.0;
        }

        let (st_label, st_dot, st_fg, st_bg, st_border) = status_pill_spec(&ag);
        let status_est_w = m.measure_styled(&st_label, TextFace::UiSmall, true) + 28.0;
        let title_budget = (tool_x - lx - status_est_w - 10.0).max(48.0);
        let max_title_chars = (title_budget / (m.cw * 0.92).max(4.0)).floor().max(6.0) as usize;
        let shown_title = truncate_chars(&ag.name, max_title_chars);
        let title_col = if ag.is_remote {
            if matches!(ag.reachability, Some(ServerReachability::Offline)) {
                theme::INK_2
            } else {
                SERVER_WHITE
            }
        } else {
            theme::INK_1
        };
        let title_y = cy + (hdr_row_h - m.line_h(TextFace::Ui)) * 0.5;
        scene.ui_text(lx, title_y, &shown_title, title_col, true, false, title_clip);
        lx += m.measure_styled(&shown_title, TextFace::Ui, true) + 10.0;

        if lx + status_est_w <= tool_x + 2.0 {
            let pill_h = m.line_h(TextFace::UiSmall) + 6.0;
            let pill_y0 = cy + (hdr_row_h - pill_h) * 0.5;
            let _ = draw_status_pill(
                &mut scene,
                lx,
                pill_y0,
                &st_label,
                st_dot,
                st_fg,
                st_bg,
                st_border,
                m,
                title_clip,
            );
        }

        cy += hdr_row_h + 10.0;

        // 4b. Inline Kill Confirmation Banner (when armed)
        if let Some((kpid, kname)) = hub.confirm_kill.as_ref() {
            let banner_h = (pill_button_h(m) + 14.0).max(36.0);
            scene.round_rect(
                rx,
                cy,
                right_edge,
                cy + banner_h,
                8.0,
                KILL_BG,
                1.0,
                KILL_BORDER,
                right_clip,
            );
            let b_y0 = cy + (banner_h - pill_button_h(m)) * 0.5;
            let mut bx = right_edge - 10.0;

            let cancel_lbl = "Cancel (Esc)";
            let cancel_w = pill_button_w(cancel_lbl, false, m);
            bx -= cancel_w;
            draw_pill_button(
                &mut scene,
                bx,
                b_y0,
                cancel_lbl,
                theme::INK_2,
                theme::BG_CHIP,
                theme::LINE,
                false,
                m,
                right_clip,
                HitAction::AgentCancelKill,
            );
            bx -= 6.0;

            let kill9_lbl = "SIGKILL (K)";
            let kill9_w = pill_button_w(kill9_lbl, true, m);
            if bx - kill9_w > rx + 90.0 {
                bx -= kill9_w;
                draw_pill_button(
                    &mut scene,
                    bx,
                    b_y0,
                    kill9_lbl,
                    SERVER_WHITE,
                    theme::rgba(0xd8, 0x3b, 0x3b, 0.45),
                    KILL_BORDER,
                    true,
                    m,
                    right_clip,
                    HitAction::AgentConfirmKill { sigkill: true },
                );
                bx -= 6.0;
            }

            let term_lbl = "SIGTERM (y)";
            let term_w = pill_button_w(term_lbl, true, m);
            if bx - term_w > rx + 70.0 {
                bx -= term_w;
                draw_pill_button(
                    &mut scene,
                    bx,
                    b_y0,
                    term_lbl,
                    theme::DANGER,
                    theme::rgba(0xff, 0x71, 0x68, 0.20),
                    KILL_BORDER,
                    true,
                    m,
                    right_clip,
                    HitAction::AgentConfirmKill { sigkill: false },
                );
                bx -= 10.0;
            }

            let prompt_msg = format!("Terminate {kname} (PID {kpid})?");
            let max_km = ((bx - rx - 14.0) / (m.cw * 0.85).max(4.0)).floor().max(10.0) as usize;
            let kty = cy + (banner_h - m.line_h(TextFace::UiSmall)) * 0.5;
            scene.ui_small(
                rx + 12.0,
                kty,
                truncate_chars(&prompt_msg, max_km),
                theme::DANGER,
                true,
                false,
                right_clip,
            );
            cy += banner_h + 10.0;
        } else if let Some(msg) = &hub.status_msg {
            let strip_h = m.line_h(TextFace::UiSmall) + 12.0;
            scene.round_rect(
                rx,
                cy,
                right_edge,
                cy + strip_h,
                6.0,
                theme::BG_SURFACE,
                1.0,
                theme::LINE,
                right_clip,
            );
            let max_sc = ((rw - 20.0) / (m.cw * 0.82).max(4.0)).floor().max(12.0) as usize;
            scene.ui_small(
                rx + 10.0,
                cy + (strip_h - m.line_h(TextFace::UiSmall)) * 0.5,
                truncate_chars(msg, max_sc),
                theme::INK_2,
                false,
                false,
                right_clip,
            );
            cy += strip_h + 10.0;
        }

        // 4c. Structured Metadata Card (2-column key/value grid)
        let meta_h = draw_metadata_card(
            &mut scene,
            &ag,
            &hub.last_home,
            rx,
            cy,
            right_edge,
            m,
            right_clip,
        );
        cy += meta_h + 12.0;

        // 4d. Live Scrollable Preview Card (fills remaining height above optional `n` composer)
        let composer_reserve = if hub.composing_new {
            m.ch + 28.0
        } else {
            0.0
        };
        let card_y1 = (body_bot - 12.0 - composer_reserve).max(cy + 48.0);
        if card_y1 > cy + 36.0 {
            let transcript_slice = hub
                .selected_transcript
                .as_ref()
                .filter(|(id, _)| id == &ag.id)
                .map(|(_, entries)| entries.as_slice())
                .unwrap_or(ag.transcript.as_slice());

            draw_preview_card(
                &mut scene,
                &ag,
                transcript_slice,
                &mut hub.preview_scroll,
                rx,
                cy,
                right_edge,
                card_y1,
                m,
                right_clip,
            );
        }

        // 4e. Inline `n` Prompt Composer
        if hub.composing_new {
            let box_h = m.ch + 16.0;
            let box_y0 = (body_bot - box_h - 10.0).max(body_top + 40.0);
            let box_y1 = box_y0 + box_h;
            scene.round_rect(
                rx,
                box_y0,
                right_edge,
                box_y1,
                7.0,
                theme::rgb(0x16, 0x16, 0x19),
                1.0,
                PRIMARY_BORDER,
                right_clip,
            );
            let ity = box_y0 + (box_h - m.ch) * 0.5;
            scene.text(rx + 10.0, ity, "❯", SERVER_ICON_SKY, true, false, right_clip);
            let ptxt = if hub.new_prompt.is_empty() {
                format!("task for {} (Enter to launch, Esc to cancel)…", ag.host)
            } else {
                hub.new_prompt.clone()
            };
            let pcol = if hub.new_prompt.is_empty() {
                theme::INK_4
            } else {
                theme::INK_1
            };
            let max_pc = ((rw - 34.0) / m.cw).floor().max(12.0) as usize;
            scene.text(
                rx + 10.0 + 1.8 * m.cw,
                ity,
                truncate_chars(&ptxt, max_pc),
                pcol,
                false,
                false,
                right_clip,
            );
        }
    } else {
        let msg = "No local agents or online SSH servers found.";
        scene.ui_text(rx, body_top + 24.0, msg, theme::INK_3, false, false, right_clip);
    }

    // -------------------------------------------------------------------------
    // 5. Footer Shortcuts Bar
    // -------------------------------------------------------------------------
    scene.round_rect(0.0, body_bot, vw, vh, 0.0, theme::BG_SURFACE, 0.0, [0.0; 4], 0);
    scene.line(0.0, body_bot, vw, body_bot, 1.0, theme::LINE, 0);
    let fty = body_bot + (footer_h - m.line_h(TextFace::UiSmall)) * 0.5;
    scene.ui_small(14.0, fty, "agent · fleet hub", theme::INK_2, true, false, 0);
    let hints = "j/k switch · enter attach · n task · x kill · o offline · r probe · esc back";
    let hw = m.measure(hints, TextFace::UiSmall);
    if vw - 14.0 - hw > 140.0 {
        scene.ui_small(vw - 14.0 - hw, fty, hints, theme::INK_4, false, false, 0);
    }

    if draw_top_bar {
        layout_top_bar(&mut scene, store, prompt, vw, m, zoom_pct, inline_titlebar);
    }
    scene
}

#[allow(clippy::too_many_arguments)]
fn draw_sidebar_agent_row(
    scene: &mut Scene,
    ag: &AgentRecord,
    idx: usize,
    is_sel: bool,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    prompt: &PromptInfo,
    m: CellMetrics,
    clip_id: u32,
) {
    let row_h = y1 - y0;
    let row_rect = HitRect::new(x0, y0, x1, y1);
    if is_sel {
        scene.round_rect(
            row_rect.x0,
            row_rect.y0,
            row_rect.x1,
            row_rect.y1,
            7.0,
            theme::COMPLETION_SEL_BG,
            1.0,
            theme::rgba(0x58, 0x9d, 0xf6, 0.48),
            clip_id,
        );
        scene.round_rect(
            x0,
            y0 + 7.0,
            x0 + 2.5,
            y1 - 7.0,
            1.25,
            theme::MATCH_HIGHLIGHT,
            0.0,
            [0.0; 4],
            clip_id,
        );
    }
    scene.push_hit_clipped(row_rect, clip_id, HitAction::SelectAgent(idx));

    let ty1 = y0 + 6.0;
    let line1_h = m.line_h(TextFace::Ui);
    let ty2 = ty1 + line1_h + 3.0;
    let icon_s = (m.ch * 0.92).clamp(14.0, 18.0);
    let icon_x = x0 + 9.0;
    let icon_y = y0 + (row_h - icon_s) * 0.5;
    let text_left = icon_x + icon_s + 9.0;

    if ag.is_remote {
        let is_offline = matches!(ag.reachability, Some(ServerReachability::Offline));
        let is_online = matches!(ag.reachability, Some(ServerReachability::Online { .. }));

        // Server icon: bright sky-blue accented `server_rack` badge for online/probing, dimmed for offline.
        if !is_offline {
            scene.round_rect(
                icon_x - 2.0,
                icon_y - 2.0,
                icon_x + icon_s + 2.0,
                icon_y + icon_s + 2.0,
                5.0,
                SERVER_ICON_BG,
                1.0,
                SERVER_ICON_BORDER,
                clip_id,
            );
            scene.svg_icon_alpha(
                icon_x,
                icon_y,
                icon_s,
                "server_rack",
                if is_sel { 1.0 } else { 0.92 },
                clip_id,
            );
        } else {
            scene.svg_icon_alpha(icon_x, icon_y, icon_s, "server_rack", 0.36, clip_id);
        }

        // Right-aligned status dot + latency / offline pill
        let mut right_cursor = x1 - 10.0;
        if is_online {
            scene.over_circle(right_cursor, ty1 + line1_h * 0.5, 3.0, theme::OK, clip_id);
            right_cursor -= 10.0;
        } else if !is_offline {
            scene.over_circle(right_cursor, ty1 + line1_h * 0.5, 2.8, WAITING_AMBER, clip_id);
            right_cursor -= 10.0;
        }

        let badge_txt = match ag.reachability {
            Some(ServerReachability::Online { rtt_ms }) => format!("{rtt_ms}ms"),
            Some(ServerReachability::Probing) | None => "…".to_owned(),
            Some(ServerReachability::Offline) => "offline".to_owned(),
        };
        let badge_w = m.measure_styled(&badge_txt, TextFace::UiSmall, !is_offline) + 10.0;
        if right_cursor - badge_w > text_left + 44.0 {
            right_cursor -= badge_w;
            let (bfg, bbg, bborder) = match ag.reachability {
                Some(ServerReachability::Online { .. }) => (theme::OK, OK_BG, OK_BORDER),
                Some(ServerReachability::Probing) | None => {
                    (WAITING_AMBER, WAITING_BG, WAITING_BORDER)
                }
                Some(ServerReachability::Offline) => (OFFLINE_ROSE, OFFLINE_BG, OFFLINE_BORDER),
            };
            let _ = draw_compact_chip(
                scene,
                right_cursor,
                ty1 + line1_h * 0.5,
                &badge_txt,
                bfg,
                bbg,
                bborder,
                is_online,
                m,
                clip_id,
            );
            right_cursor -= 6.0;
        }

        // Optional subtle alias chip (e.g. `da1`) after the bold server name if space permits.
        let alias_chip = ag.aliases.first().map(|a| {
            if ag.aliases.len() > 1 {
                format!("{a} +{}", ag.aliases.len() - 1)
            } else {
                a.clone()
            }
        });
        let name_col = if is_offline {
            if is_sel { theme::INK_2 } else { theme::INK_3 }
        } else if is_sel {
            SERVER_WHITE
        } else {
            SERVER_SKY
        };
        let avail_name_w = (right_cursor - text_left).max(24.0);
        let max_name_c = (avail_name_w / (m.cw * 0.88).max(4.0)).floor().max(5.0) as usize;
        let shown_name = truncate_chars(&ag.name, max_name_c);
        scene.ui_text(text_left, ty1, &shown_name, name_col, true, false, clip_id);

        if let Some(alias_lbl) = alias_chip {
            let name_w = m.measure_styled(&shown_name, TextFace::Ui, true);
            let alias_x = text_left + name_w + 6.0;
            let alias_w = m.measure_styled(&alias_lbl, TextFace::UiSmall, false) + 10.0;
            if alias_x + alias_w <= right_cursor {
                let _ = draw_compact_chip(
                    scene,
                    alias_x,
                    ty1 + line1_h * 0.5,
                    &alias_lbl,
                    theme::INK_3,
                    theme::BG_CHIP,
                    theme::LINE_FAINT,
                    false,
                    m,
                    clip_id,
                );
            }
        }

        // Subtitle: clean `user@host`
        let max_sub_c = ((x1 - text_left - 10.0) / (m.cw * 0.78).max(4.0))
            .floor()
            .max(8.0) as usize;
        scene.ui_small(
            text_left,
            ty2,
            truncate_middle(&ag.prompt_summary, max_sub_c),
            if is_offline {
                theme::INK_4
            } else if is_sel {
                theme::INK_2
            } else {
                theme::INK_3
            },
            false,
            false,
            clip_id,
        );
        return;
    }

    // Local agent row: native host `.app` icon (or host emoji / MacBook Pro SVG).
    if let Some(icon_path) = ag.origin_app_icon_path.as_ref() {
        scene.image(icon_x, icon_y, icon_s, icon_s, icon_path.clone(), clip_id);
    } else if let Some(sym) = prompt.host_symbol.as_deref() {
        scene.emoji_symbol(
            icon_x,
            icon_y,
            icon_s,
            sym,
            if is_sel { 1.0 } else { 0.88 },
            clip_id,
        );
    } else {
        scene.svg_icon_alpha(
            icon_x,
            icon_y,
            icon_s,
            "macbook_pro",
            if is_sel { 1.0 } else { 0.78 },
            clip_id,
        );
    }

    // Right-aligned semantic status dot + CPU% / RAM metric badge.
    let dot_col = match ag.status {
        AgentStatus::Running => theme::OK,
        AgentStatus::Waiting => WAITING_AMBER,
        AgentStatus::Idle => theme::INK_4,
        AgentStatus::Exited(0) => theme::INK_3,
        AgentStatus::Exited(_) => theme::DANGER,
    };
    let dot_x = x1 - 10.0;
    scene.over_circle(dot_x, ty1 + line1_h * 0.5, 3.0, dot_col, clip_id);

    let mut right_cursor = dot_x - 8.0;
    if !ag.metric_badge.is_empty() {
        let is_running = ag.status == AgentStatus::Running;
        let bw = m.measure_styled(&ag.metric_badge, TextFace::UiSmall, is_running);
        if right_cursor - bw > text_left + 50.0 {
            right_cursor -= bw;
            let bcol = if is_running {
                theme::OK
            } else if ag.status == AgentStatus::Waiting {
                theme::INK_3
            } else {
                theme::INK_4
            };
            let bty = ty1 + (line1_h - m.line_h(TextFace::UiSmall)) * 0.5;
            scene.ui_small(
                right_cursor,
                bty,
                &ag.metric_badge,
                bcol,
                is_running,
                false,
                clip_id,
            );
            right_cursor -= 8.0;
        }
    }

    // Top line: colored agent brand chip (`claude` / `codex` / `harness`) + bold project folder name.
    let mut title_x = text_left;
    if !ag.kind_label.is_empty() && ag.kind_label != "local" {
        let chip_w = m.measure_styled(&ag.kind_label, TextFace::UiSmall, true) + 10.0;
        if title_x + chip_w + 28.0 < right_cursor {
            let (cfg, cbg, cborder) = kind_chip_palette(&ag.kind_label);
            let drawn_w = draw_compact_chip(
                scene,
                title_x,
                ty1 + line1_h * 0.5,
                &ag.kind_label,
                cfg,
                cbg,
                cborder,
                true,
                m,
                clip_id,
            );
            title_x += drawn_w + 6.0;
        }
    }

    let max_title_c = ((right_cursor - title_x) / (m.cw * 0.88).max(4.0))
        .floor()
        .max(5.0) as usize;
    scene.ui_text(
        title_x,
        ty1,
        truncate_chars(&ag.name, max_title_c),
        theme::INK_1,
        true,
        false,
        clip_id,
    );

    // Subtitle: `cwd · uptime · in Ghostty`
    let max_sub_c = ((x1 - text_left - 10.0) / (m.cw * 0.78).max(4.0))
        .floor()
        .max(8.0) as usize;
    scene.ui_small(
        text_left,
        ty2,
        truncate_middle(&ag.prompt_summary, max_sub_c),
        if is_sel { theme::INK_2 } else { theme::INK_3 },
        false,
        false,
        clip_id,
    );
}

fn status_pill_spec(ag: &AgentRecord) -> (String, [f32; 4], [f32; 4], [f32; 4], [f32; 4]) {
    if ag.is_remote {
        return match ag.reachability {
            Some(ServerReachability::Online { rtt_ms }) => (
                format!("online · {rtt_ms}ms"),
                theme::OK,
                theme::OK,
                OK_BG,
                OK_BORDER,
            ),
            Some(ServerReachability::Probing) | None => (
                "probing".to_owned(),
                WAITING_AMBER,
                WAITING_AMBER,
                WAITING_BG,
                WAITING_BORDER,
            ),
            Some(ServerReachability::Offline) => (
                "offline".to_owned(),
                OFFLINE_ROSE,
                OFFLINE_ROSE,
                OFFLINE_BG,
                OFFLINE_BORDER,
            ),
        };
    }

    match ag.status {
        AgentStatus::Running => (
            "working".to_owned(),
            theme::OK,
            theme::OK,
            OK_BG,
            OK_BORDER,
        ),
        AgentStatus::Waiting => (
            "waiting".to_owned(),
            WAITING_AMBER,
            WAITING_AMBER,
            WAITING_BG,
            WAITING_BORDER,
        ),
        AgentStatus::Idle => (
            "ready".to_owned(),
            theme::INK_3,
            theme::INK_2,
            theme::BG_CHIP,
            theme::LINE,
        ),
        AgentStatus::Exited(0) => (
            "done".to_owned(),
            theme::INK_3,
            theme::INK_2,
            theme::BG_CHIP,
            theme::LINE,
        ),
        AgentStatus::Exited(_) => (
            "failed".to_owned(),
            theme::DANGER,
            theme::DANGER,
            KILL_BG,
            KILL_BORDER,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_metadata_card(
    scene: &mut Scene,
    ag: &AgentRecord,
    home: &str,
    x0: f32,
    y0: f32,
    x1: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    struct MetaRow {
        key: &'static str,
        val: String,
        val_col: [f32; 4],
        mono: bool,
        icon_path: Option<String>,
    }

    let mut rows: Vec<MetaRow> = Vec::with_capacity(4);
    if ag.is_remote {
        let target_host = if ag.hostname.is_empty() {
            ag.host.as_str()
        } else {
            ag.hostname.as_str()
        };
        let user = if ag.user.is_empty() { "root" } else { ag.user.as_str() };
        let port = if ag.port == 0 { 22 } else { ag.port };
        rows.push(MetaRow {
            key: "Target",
            val: format!("{user}@{target_host}:{port}"),
            val_col: SERVER_SKY,
            mono: true,
            icon_path: None,
        });
        let (st_txt, st_col) = match ag.reachability {
            Some(ServerReachability::Online { rtt_ms }) => {
                (format!("Online · {rtt_ms}ms RTT"), theme::OK)
            }
            Some(ServerReachability::Probing) | None => {
                ("Probing TCP connectivity…".to_owned(), WAITING_AMBER)
            }
            Some(ServerReachability::Offline) => {
                ("Offline (unreachable)".to_owned(), OFFLINE_ROSE)
            }
        };
        rows.push(MetaRow {
            key: "Status",
            val: st_txt,
            val_col: st_col,
            mono: false,
            icon_path: None,
        });
        if !ag.aliases.is_empty() {
            rows.push(MetaRow {
                key: "Aliases",
                val: ag.aliases.join(", "),
                val_col: theme::INK_2,
                mono: true,
                icon_path: None,
            });
        }
        rows.push(MetaRow {
            key: "Protocol",
            val: "blaze-ssh (BSH1 / SSH)".to_owned(),
            val_col: theme::INK_2,
            mono: false,
            icon_path: None,
        });
    } else {
        let host_val = if !ag.origin_app_name.is_empty() {
            format!("{} · in {}", ag.host, ag.origin_app_name)
        } else {
            format!("{} (local)", ag.host)
        };
        rows.push(MetaRow {
            key: "Host",
            val: host_val,
            val_col: theme::INK_1,
            mono: false,
            icon_path: ag.origin_app_icon_path.clone(),
        });
        rows.push(MetaRow {
            key: "Directory",
            val: shorten_agent_path(&ag.cwd, home),
            val_col: theme::PROMPT_DIR,
            mono: true,
            icon_path: None,
        });
        if let Some(pid) = ag.pid {
            let mem_s = format_iec_bytes(ag.mem_bytes);
            let up_s = format_uptime(ag.uptime_secs);
            rows.push(MetaRow {
                key: "Process",
                val: format!(
                    "PID {pid} · up {up_s} · {mem_s} · {:.1}% CPU · {} thr",
                    ag.cpu_pct, ag.threads
                ),
                val_col: theme::INK_2,
                mono: true,
                icon_path: None,
            });
        }
        if !ag.cmdline.is_empty() {
            rows.push(MetaRow {
                key: "Command",
                val: ag.cmdline.clone(),
                val_col: theme::INK_2,
                mono: true,
                icon_path: None,
            });
        }
    }

    let row_h = (m.line_h(TextFace::UiSmall) + 7.0).max(20.0);
    let pad_y = 9.0;
    let total_h = pad_y * 2.0 + rows.len() as f32 * row_h;

    scene.round_rect(x0, y0, x1, y0 + total_h, 8.0, CARD_BG, 1.0, theme::LINE, clip_id);

    let key_x = x0 + 12.0;
    let key_col_w = 74.0f32.min((x1 - x0) * 0.28);
    let val_x = key_x + key_col_w;
    let val_w = (x1 - val_x - 12.0).max(40.0);

    let mut ry = y0 + pad_y;
    for row in rows {
        let key_y = ry + (row_h - m.line_h(TextFace::UiSmall)) * 0.5;
        scene.ui_small(key_x, key_y, row.key, theme::INK_3, false, false, clip_id);

        let mut vx = val_x;
        let mut avail_w = val_w;
        if let Some(icon_p) = row.icon_path {
            let isz = 13.0;
            scene.image(vx, ry + (row_h - isz) * 0.5, isz, isz, icon_p, clip_id);
            vx += isz + 6.0;
            avail_w = (avail_w - isz - 6.0).max(24.0);
        }

        if row.mono {
            let max_c = (avail_w / m.cw).floor().max(8.0) as usize;
            let shown = if row.key == "Directory" {
                truncate_middle(&row.val, max_c)
            } else {
                truncate_chars(&row.val, max_c)
            };
            let ty = ry + (row_h - m.ch) * 0.5;
            scene.text(vx, ty, shown, row.val_col, false, false, clip_id);
        } else {
            let max_c = (avail_w / (m.cw * 0.82).max(4.0)).floor().max(8.0) as usize;
            let shown = truncate_chars(&row.val, max_c);
            let ty = ry + (row_h - m.line_h(TextFace::UiSmall)) * 0.5;
            scene.ui_small(vx, ty, shown, row.val_col, false, false, clip_id);
        }
        ry += row_h;
    }

    total_h
}

#[allow(clippy::too_many_arguments)]
fn draw_preview_card(
    scene: &mut Scene,
    ag: &AgentRecord,
    transcript: &[TranscriptEntry],
    scroll: &mut ScrollState,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    m: CellMetrics,
    clip_id: u32,
) {
    scene.round_rect(x0, y0, x1, y1, 8.0, CARD_BG, 1.0, theme::LINE, clip_id);

    let has_transcript = !transcript.is_empty();
    let header_title = if has_transcript {
        "LIVE TRANSCRIPT"
    } else {
        "SESSION PREVIEW"
    };
    let badge = if has_transcript {
        Some(format!("{} entries", transcript.len()))
    } else {
        None
    };

    let hdr_h = draw_section_header(
        scene,
        x0 + 12.0,
        x1 - 12.0,
        y0 + 4.0,
        header_title,
        badge.as_deref(),
        m,
        clip_id,
    );

    let vp_y0 = y0 + 4.0 + hdr_h + 4.0;
    let vp_y1 = (y1 - 8.0).max(vp_y0 + 10.0);
    let vp_h = (vp_y1 - vp_y0).max(1.0);
    let inner_x0 = x0 + 12.0;
    let inner_x1 = (x1 - 14.0).max(inner_x0 + 60.0);
    let inner_w = (inner_x1 - inner_x0).max(60.0);

    let preview_clip = scene.push_clip(x0 + 1.0, vp_y0, x1 - 1.0, vp_y1, 0.0);

    if has_transcript {
        struct WrappedEntry {
            y0: f32,
            y1: f32,
            role: PreviewRole,
            role_label: &'static str,
            lines: Vec<String>,
            is_mono: bool,
        }

        let agent_role_lbl: &'static str = match ag.kind_label.as_str() {
            "claude" => "CLAUDE",
            "codex" => "CODEX",
            "harness" => "HARNESS",
            _ => "AGENT",
        };
        let badge_h = (m.line_h(TextFace::UiSmall) + 4.0).max(16.0);
        let ui_lh = m.line_h(TextFace::Ui) + 2.0;
        let mono_lh = m.ch + 2.0;
        let small_lh = m.line_h(TextFace::UiSmall) + 2.0;

        let mut wrapped: Vec<WrappedEntry> = Vec::with_capacity(transcript.len());
        let mut cy = 4.0f32;
        for entry in transcript {
            let (role_label, is_mono, chars_per_line, lh) = match entry.role {
                PreviewRole::User => (
                    "YOU",
                    false,
                    (inner_w / (m.cw * 0.88).max(4.0)).floor().max(16.0) as usize,
                    ui_lh,
                ),
                PreviewRole::Assistant => (
                    agent_role_lbl,
                    false,
                    (inner_w / (m.cw * 0.88).max(4.0)).floor().max(16.0) as usize,
                    ui_lh,
                ),
                PreviewRole::Tool => (
                    "TOOL",
                    true,
                    (inner_w / m.cw).floor().max(16.0) as usize,
                    mono_lh,
                ),
                PreviewRole::Meta => (
                    "INFO",
                    false,
                    (inner_w / (m.cw * 0.80).max(4.0)).floor().max(16.0) as usize,
                    small_lh,
                ),
            };
            let lines = wrap_text_lines(&entry.text, chars_per_line);
            let h = badge_h + 4.0 + lines.len() as f32 * lh + 8.0;
            wrapped.push(WrappedEntry {
                y0: cy,
                y1: cy + h,
                role: entry.role,
                role_label,
                lines,
                is_mono,
            });
            cy += h + 4.0;
        }

        let content_h = cy + 6.0;
        scroll.set_bounds(content_h, vp_h);
        let off_y = scroll.offset_y;

        for item in &wrapped {
            let sy0 = vp_y0 + item.y0 - off_y;
            let sy1 = vp_y0 + item.y1 - off_y;
            if sy1 < vp_y0 - 4.0 || sy0 > vp_y1 + 4.0 {
                continue;
            }

            let (rfg, rbg, rborder, body_col) = match item.role {
                PreviewRole::User => (USER_ROLE_FG, USER_ROLE_BG, USER_ROLE_BORDER, theme::INK_1),
                PreviewRole::Assistant => {
                    let (fg, bg, bd) = kind_chip_palette(&ag.kind_label);
                    (fg, bg, bd, theme::INK_2)
                }
                PreviewRole::Tool => (TOOL_ROLE_FG, TOOL_ROLE_BG, TOOL_ROLE_BORDER, theme::INK_2),
                PreviewRole::Meta => (theme::INK_3, theme::BG_CHIP, theme::LINE, theme::INK_3),
            };

            let _ = draw_compact_chip(
                scene,
                inner_x0,
                sy0 + badge_h * 0.5,
                item.role_label,
                rfg,
                rbg,
                rborder,
                true,
                m,
                preview_clip,
            );

            let mut ly = sy0 + badge_h + 4.0;
            for line in &item.lines {
                if ly + m.ch >= vp_y0 - 4.0 && ly <= vp_y1 + 4.0 {
                    if item.is_mono {
                        scene.text(inner_x0 + 2.0, ly, line, body_col, false, false, preview_clip);
                    } else if matches!(item.role, PreviewRole::Meta) {
                        scene.ui_small(inner_x0 + 2.0, ly, line, body_col, false, false, preview_clip);
                    } else {
                        scene.ui_text(inner_x0 + 2.0, ly, line, body_col, false, false, preview_clip);
                    }
                }
                ly += if item.is_mono {
                    mono_lh
                } else if matches!(item.role, PreviewRole::Meta) {
                    small_lh
                } else {
                    ui_lh
                };
            }
        }
    } else {
        // Fallback live session context (PTY/in-shell agents, remote servers, or agents with no JSONL yet).
        let max_c = (inner_w / m.cw).floor().max(16.0) as usize;
        let mut display_lines: Vec<(String, [f32; 4])> = Vec::new();
        for line in &ag.preview_lines {
            for wrapped_l in wrap_text_lines(line, max_c) {
                display_lines.push((wrapped_l, theme::INK_2));
            }
        }
        if !ag.is_remote && ag.pid.is_some() {
            display_lines.push((String::new(), theme::INK_3));
            for l in wrap_text_lines(
                "No recent transcript log found for this directory yet. Press Enter to attach a fresh session here, n to launch with a task prompt, or x to terminate this process.",
                max_c,
            ) {
                display_lines.push((l, theme::INK_3));
            }
        } else if ag.is_remote {
            display_lines.push((String::new(), theme::INK_3));
            let hint = match ag.reachability {
                Some(ServerReachability::Offline) => {
                    "Host did not answer TCP probe. Press r to re-probe, or Enter to attempt SSH anyway."
                }
                Some(ServerReachability::Probing) | None => {
                    "Checking SSH port reachability in background… Press Enter to connect now."
                }
                Some(ServerReachability::Online { .. }) => {
                    "Server is reachable. Press Enter to attach over blaze-ssh, or n to dispatch a prompted task."
                }
            };
            for l in wrap_text_lines(hint, max_c) {
                display_lines.push((l, theme::INK_3));
            }
        }

        let lh = m.ch + 4.0;
        let content_h = 8.0 + display_lines.len() as f32 * lh;
        scroll.set_bounds(content_h, vp_h);
        let off_y = scroll.offset_y;

        let mut ly = vp_y0 + 4.0 - off_y;
        for (line, col) in display_lines {
            if ly + lh >= vp_y0 - 4.0 && ly <= vp_y1 + 4.0 && !line.is_empty() {
                scene.text(inner_x0, ly, line, col, false, false, preview_clip);
            }
            ly += lh;
        }
    }

    draw_scrollbar(
        scene,
        *scroll,
        x1 - 7.0,
        vp_y0 + 4.0,
        x1 - 3.0,
        vp_y1 - 4.0,
        preview_clip,
    );
}
