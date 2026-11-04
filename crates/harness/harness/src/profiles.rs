//! How each agent kind is launched: binary, locked-down default flags and environment.
//! Defaults strip built-in tools, plugins, skills, other MCP servers and auto-update, so the
//! only tool an agent has is the harness `ts` REPL. User args are appended after the defaults.
use crate::paths;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

pub struct Ctx<'a> {
    pub session: u64,
    pub cwd: &'a str,
    pub sock: &'a std::path::Path,
    pub user_args: &'a [String],
    pub prompt: Option<&'a str>,
}

pub const KINDS: &[&str] = &["claude", "codex", "shell"];

const SYSTEM_NOTE: &str = "You are running inside harness. Your only tool is `ts`, a persistent TypeScript REPL whose \
description lists the functions in scope (web search, agents.spawn/send/screen/tree to run and talk to other agents, \
process and task tables). Use it for everything. To run another agent, use agents.spawn in ts (never a built-in \
sub-agent or collaboration tool), so it appears in the harness tree.";

fn mcp_env(ctx: &Ctx) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("HARNESS_SESSION".into(), ctx.session.to_string());
    env.insert("HARNESS_SOCK".into(), ctx.sock.display().to_string());
    env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    env.insert("HOME".into(), std::env::var("HOME").unwrap_or_default());
    env
}

fn mcp_args() -> Vec<String> {
    vec!["run".into(), paths::root().join("src/mcp.ts").display().to_string()]
}

fn common_env(ctx: &Ctx) -> BTreeMap<String, String> {
    let mut env = mcp_env(ctx);
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());
    env
}

pub fn launch(kind: &str, ctx: &Ctx) -> anyhow::Result<Launch> {
    let mut l = match kind {
        "claude" => claude(ctx),
        "codex" => codex(ctx)?,
        "shell" => Launch {
            program: std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into()),
            args: vec!["-l".into()],
            env: common_env(ctx),
        },
        other => anyhow::bail!("unknown agent kind {other:?} (expected one of {KINDS:?})"),
    };
    l.args.extend(ctx.user_args.iter().cloned());
    if let Some(p) = ctx.prompt {
        l.args.push(p.to_string());
    }
    Ok(l)
}

fn claude(ctx: &Ctx) -> Launch {
    let mcp = json!({ "mcpServers": { "harness": { "command": "bun", "args": mcp_args(), "env": mcp_env(ctx) } } });
    let args = [
        "--strict-mcp-config", "--mcp-config", &mcp.to_string(),
        "--tools", "",                          // no built-in tools (Bash, Read, Edit, WebFetch, ...)
        "--allowedTools", "mcp__harness__ts",   // the REPL runs without permission prompts
        "--setting-sources", "",                // ignore user/project/local settings: no hooks, plugins, permissions
        "--disable-slash-commands",             // no skills or custom commands
        "--no-chrome",
        "--append-system-prompt", SYSTEM_NOTE,
    ];
    let mut env = common_env(ctx);
    for (k, v) in [
        ("DISABLE_AUTOUPDATER", "1"),
        ("DISABLE_TELEMETRY", "1"),
        ("DISABLE_ERROR_REPORTING", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ] {
        env.insert(k.into(), v.into());
    }
    Launch { program: "claude".into(), args: args.iter().map(|s| s.to_string()).collect(), env }
}

/// Codex features that add tools or pull in outside config; all switched off.
const CODEX_DISABLE: &[&str] = &[
    "shell_tool", "unified_exec", "unified_exec_tty", "apps", "browser_use", "browser_use_external",
    "browser_use_full_cdp_access", "computer_use", "image_generation", "multi_agent", "multi_agent_v2",
    "plugins", "remote_plugin", "plugin_discovery_prefetch", "skill_search", "skill_mcp_dependency_install",
    "sleep_tool", "tool_suggest", "view_image", "hooks", "goals", "shell_snapshot",
];

fn codex(ctx: &Ctx) -> anyhow::Result<Launch> {
    // A private CODEX_HOME keeps the user's config.toml, AGENTS.md, MCP servers, skills and plugins out;
    // only auth is shared (symlinked).
    let home = paths::home().join("codex-home");
    std::fs::create_dir_all(&home)?;
    let auth_src = PathBuf::from(std::env::var("HOME")?).join(".codex/auth.json");
    let auth_dst = home.join("auth.json");
    if auth_src.exists() && !auth_dst.exists() {
        std::os::unix::fs::symlink(&auth_src, &auth_dst)?;
    }
    let toml_str = |s: &str| serde_json::to_string(s).unwrap(); // JSON strings are valid TOML basic strings
    // Pre-trust the session's directory so Codex doesn't stop on its folder-trust prompt.
    let config = home.join("config.toml");
    let existing = std::fs::read_to_string(&config).unwrap_or_default();
    let header = format!("[projects.{}]", toml_str(ctx.cwd));
    if !existing.contains(&header) {
        std::fs::write(&config, format!("{existing}\n{header}\ntrust_level = \"trusted\"\n"))?;
    }
    let env_table = mcp_env(ctx).iter().map(|(k, v)| format!("{k}={}", toml_str(v))).collect::<Vec<_>>().join(",");
    let args_list = mcp_args().iter().map(|a| toml_str(a)).collect::<Vec<_>>().join(",");
    let mut args: Vec<String> = vec![
        "-c".into(), format!("mcp_servers.harness.command={}", toml_str("bun")),
        "-c".into(), format!("mcp_servers.harness.args=[{args_list}]"),
        "-c".into(), format!("mcp_servers.harness.env={{{env_table}}}"),
        "-c".into(), "mcp_servers.harness.default_tools_approval_mode=\"approve\"".into(), // ts runs without prompts
        "-c".into(), "web_search=\"disabled\"".into(),
        "-c".into(), "include_apply_patch_tool=false".into(),
        "-c".into(), format!("developer_instructions={}", toml_str(SYSTEM_NOTE)),
        "-a".into(), "never".into(),
    ];
    for f in CODEX_DISABLE {
        args.push("--disable".into());
        args.push((*f).into());
    }
    let mut env = common_env(ctx);
    env.insert("CODEX_HOME".into(), home.display().to_string());
    let (program, mut pre) = match which("codex") {
        Some(p) => (p, vec![]),
        None => ("bunx".into(), vec!["@openai/codex".to_string()]),
    };
    pre.extend(args);
    Ok(Launch { program, args: pre, env })
}

fn which(bin: &str) -> Option<String> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| PathBuf::from(d).join(bin)).find(|p| p.is_file()).map(|p| p.display().to_string())
}
