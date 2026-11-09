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
    /// The reasoning effort the agent runs with, when the launch decides it (claude's `--effort`). None for shell and
    /// cursor, and for codex, whose app-server says its effort in its thread/start answer (chat/codex.rs).
    pub effort: Option<String>,
}

pub struct Ctx<'a> {
    pub session: u64,
    pub cwd: &'a str,
    pub sock: &'a std::path::Path,
    pub user_args: &'a [String],
    pub prompt: Option<&'a str>,
    /// A chat session (chat/mod.rs): the agent speaks its structured protocol on stdin and stdout, not its TUI.
    pub chat: bool,
}

pub const KINDS: &[&str] = &["claude", "codex", "cursor", "shell"];

/// The note every agent gets on top of its own system prompt (claude's `--append-system-prompt`, codex's
/// `developer_instructions`, cursor's MCP server instructions): the always-on topics, one file per topic under
/// `system_note/`, each with its topic's name (what a chat's context popover names its row), joined in this order with
/// a blank line between them, then the session's AGENTS.md files (instructions.rs), then the skills index ([`Note`]). A topic only some tasks need is a skill (skills.rs),
/// not a part here.
pub const SYSTEM_NOTE_PARTS: &[(&str, &str)] = &[
    ("tools", include_str!("system_note/tools.md")),     // the one tool, ts, and its typed calls
    ("output", include_str!("system_note/output.md")),   // the person already sees every cell: don't restate it
    ("ui", include_str!("system_note/ui.md")),           // cells as Svelte 5 UI: read the ui skill first
    ("replies", include_str!("system_note/replies.md")), // how replies to Andrew read
    ("prose", include_str!("system_note/prose.md")),     // prose, not monospace; inline components (<Path>, <Symbol>, <Link>, <PR>, <Issue>)
];

/// What one agent launch is told: [`SYSTEM_NOTE_PARTS`], then the index of the skills its session offers (its last
/// part, [`crate::skills::Skills::index`]), and those skills, which its kernel serves (`HARNESS_SKILLS`, [`kernel_env`]).
/// Built per launch: project skills depend on the session's directory, and an edited skill shows at the next launch
/// without a rebuild.
struct Note {
    text: String,
    skills: crate::skills::Skills,
}

impl Note {
    fn of(ctx: &Ctx) -> anyhow::Result<Note> {
        let cwd = std::path::Path::new(ctx.cwd);
        let skills = crate::skills::Skills::discover(cwd)?;
        let instructions = crate::instructions::Instructions::discover(cwd)?.note();
        let index = skills.index();
        let text = SYSTEM_NOTE_PARTS
            .iter()
            .map(|(_, part)| part.trim())
            .chain([instructions.as_str(), index.as_str()])
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(Note { text, skills })
    }
}

fn mcp_env(ctx: &Ctx) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("HARNESS_SESSION".into(), ctx.session.to_string());
    env.insert("HARNESS_SOCK".into(), ctx.sock.display().to_string());
    env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    env.insert("HOME".into(), std::env::var("HOME").unwrap_or_default());
    env
}

/// The kernel's environment: [`mcp_env`] plus the skills it serves.
fn kernel_env(ctx: &Ctx, note: &Note) -> BTreeMap<String, String> {
    let mut env = mcp_env(ctx);
    env.insert("HARNESS_SKILLS".into(), note.skills.env());
    env
}

fn mcp_args() -> anyhow::Result<Vec<String>> {
    Ok(vec!["run".into(), paths::root()?.join("src/mcp.ts").display().to_string()])
}

fn common_env(ctx: &Ctx) -> BTreeMap<String, String> {
    let mut env = mcp_env(ctx);
    // The terminal is Ghostty's core, so say so the way Ghostty does: xterm-ghostty with Ghostty's own terminfo
    // (Ghostty sets TERMINFO to its bundle too), or xterm-256color where that terminfo is not installed.
    let terminfo = PathBuf::from("/Applications/Ghostty.app/Contents/Resources/terminfo");
    if terminfo.join("78/xterm-ghostty").exists() {
        env.insert("TERM".into(), "xterm-ghostty".into());
        env.insert("TERMINFO".into(), terminfo.display().to_string());
    } else {
        env.insert("TERM".into(), "xterm-256color".into());
    }
    env.insert("TERM_PROGRAM".into(), "ghostty".into());
    env.insert("TERM_PROGRAM_VERSION".into(), harness_vt::ghostty_version().into());
    env.insert("COLORTERM".into(), "truecolor".into());
    env
}

pub fn launch(kind: &str, ctx: &Ctx) -> anyhow::Result<Launch> {
    let mut l = match kind {
        "claude" => claude(ctx, &Note::of(ctx)?)?,
        "codex" => codex(ctx, &Note::of(ctx)?)?,
        "cursor" => cursor(ctx, &Note::of(ctx)?)?,
        "shell" => Launch {
            program: login_shell(),
            args: vec!["-l".into()],
            env: common_env(ctx),
            effort: None,
        },
        other => anyhow::bail!("unknown agent kind {other:?} (expected one of {KINDS:?})"),
    };
    // A codex chat already took the user's args into its app-server config (codex()).
    if !(ctx.chat && kind == "codex") {
        l.args.extend(ctx.user_args.iter().cloned());
    }
    if let Some(p) = ctx.prompt {
        l.args.push(p.to_string());
    }
    Ok(l)
}

/// Claude's effort when neither the user's args nor their own settings name one: passed as `--effort` so the effort the
/// chat shows is the one it runs with, not claude's unstated built-in default.
const CLAUDE_EFFORT: &str = "high";

/// The effort a claude launch runs with, and whether the launch must pass it (false when the user's own args already
/// carry `--effort`). The user's `--effort X` / `--effort=X` first; then `effortLevel` from their real
/// `~/.claude/settings.json` (which `--setting-sources ""` would otherwise drop, the way cursor() inherits the user's
/// model); then [`CLAUDE_EFFORT`].
fn claude_effort(user_args: &[String]) -> (String, bool) {
    let mut it = user_args.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix("--effort=") {
            return (v.to_owned(), false);
        }
        if a == "--effort"
            && let Some(v) = it.next()
        {
            return (v.clone(), false);
        }
    }
    let user = std::env::var_os("HOME")
        .and_then(|h| std::fs::read(PathBuf::from(h).join(".claude/settings.json")).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("effortLevel").and_then(|e| e.as_str()).filter(|e| !e.is_empty()).map(str::to_owned));
    (user.unwrap_or_else(|| CLAUDE_EFFORT.to_owned()), true)
}

fn claude(ctx: &Ctx, note: &Note) -> anyhow::Result<Launch> {
    let mut out: Vec<String> = Vec::new();
    if ctx.chat {
        out.extend(crate::chat::claude::ARGS.iter().map(|a| (*a).to_owned()));
    }
    let (effort, pass) = claude_effort(ctx.user_args);
    if pass {
        out.extend(["--effort".to_owned(), effort.clone()]);
    }
    let mcp = json!({ "mcpServers": { "harness": { "command": "bun", "args": mcp_args()?, "env": kernel_env(ctx, note) } } });
    let args = [
        "--strict-mcp-config", "--mcp-config", &mcp.to_string(),
        "--tools", "",                          // no built-in tools (Bash, Read, Edit, WebFetch, ...)
        "--allowedTools", "mcp__harness__ts",   // the REPL runs without permission prompts
        "--setting-sources", "",                // ignore user/project/local settings: no hooks, plugins, permissions
        "--disable-slash-commands",             // no skills or custom commands
        "--no-chrome",
        "--dangerously-skip-permissions",       // yolo: nothing ever prompts
        "--settings", r#"{"skipDangerousModePermissionPrompt":true}"#, // flag settings apply despite --setting-sources ""
        "--append-system-prompt", note.text.as_str(),
    ];
    let mut env = common_env(ctx);
    for (k, v) in [
        ("DISABLE_AUTOUPDATER", "1"),
        ("DISABLE_TELEMETRY", "1"),
        ("DISABLE_ERROR_REPORTING", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        // No CLAUDE.md / AGENTS.md from claude's own loader: harness puts AGENTS.md in the note for every kind alike
        // (instructions.rs).
        ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
        ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
        ("CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS", "1"),
        // Show the real cursor at the input instead of hiding it (?25l) and painting an inverse block. Claude takes
        // that from its `tengu_native_cursor` flag, which DISABLE_TELEMETRY / NONESSENTIAL_TRAFFIC leave at false.
        ("CLAUDE_CODE_NATIVE_CURSOR", "1"),
        // The fullscreen renderer: the alt screen with Claude's own virtualized scrollback, so the input bar stays put
        // and the wheel (sent as mouse reports, src/app/panes.rs wheel) scrolls the conversation, not the terminal's
        // history. Same as the `tui: "fullscreen"` setting, which --setting-sources "" would drop.
        ("CLAUDE_CODE_NO_FLICKER", "1"),
    ] {
        env.insert(k.into(), v.into());
    }
    out.extend(args.iter().map(|s| s.to_string()));
    Ok(Launch { program: "claude".into(), args: out, env, effort: Some(effort) })
}

/// Codex features that add tools or pull in outside config; all switched off.
const CODEX_DISABLE: &[&str] = &[
    "shell_tool", "unified_exec", "unified_exec_tty", "apps", "browser_use", "browser_use_external",
    "browser_use_full_cdp_access", "computer_use", "image_generation", "multi_agent", "multi_agent_v2",
    "plugins", "remote_plugin", "plugin_discovery_prefetch", "skill_search", "skill_mcp_dependency_install",
    "sleep_tool", "tool_suggest", "view_image", "hooks", "goals", "shell_snapshot",
];

fn codex(ctx: &Ctx, note: &Note) -> anyhow::Result<Launch> {
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
    let env_table = kernel_env(ctx, note).iter().map(|(k, v)| format!("{k}={}", toml_str(v))).collect::<Vec<_>>().join(",");
    let args_list = mcp_args()?.iter().map(|a| toml_str(a)).collect::<Vec<_>>().join(",");
    let mut args: Vec<String> = vec![
        "-c".into(), format!("mcp_servers.harness.command={}", toml_str("bun")),
        "-c".into(), format!("mcp_servers.harness.args=[{args_list}]"),
        "-c".into(), format!("mcp_servers.harness.env={{{env_table}}}"),
        "-c".into(), "mcp_servers.harness.default_tools_approval_mode=\"approve\"".into(), // ts runs without prompts
        "-c".into(), "web_search=\"disabled\"".into(),
        "-c".into(), "tools.experimental_request_user_input={enabled=false}".into(),
        "-c".into(), "project_doc_max_bytes=0".into(), // AGENTS.md comes in the note (instructions.rs), not codex's loader
        "-c".into(), "skills.include_instructions=false".into(),
        "-c".into(), "skills.bundled={enabled=false}".into(),
        "-c".into(), format!("developer_instructions={}", toml_str(&note.text)),
    ];
    if ctx.chat {
        // `codex app-server` (chat/codex.rs) has no bypass flag: the same yolo as config (thread/start repeats it).
        args.insert(0, "app-server".into());
        args.extend(["-c", r#"approval_policy="never""#, "-c", r#"sandbox_mode="danger-full-access""#].map(String::from));
        // app-server takes only `-c`, `--enable` and `--disable`: a model asked for the TUI's way becomes its config.
        let mut user = ctx.user_args.iter();
        while let Some(a) = user.next() {
            match a.as_str() {
                "-m" | "--model" => {
                    let m = user.next().ok_or_else(|| anyhow::anyhow!("codex {a} needs a model name after it"))?;
                    args.extend(["-c".to_owned(), format!("model={}", toml_str(m))]);
                }
                _ => args.push(a.clone()),
            }
        }
    } else {
        args.push("--dangerously-bypass-approvals-and-sandbox".into()); // yolo: no approvals, no sandbox
    }
    if let Some(catalog) = codex_catalog(&home)? {
        args.push("-c".into());
        args.push(format!("model_catalog_json={}", toml_str(&catalog.display().to_string())));
    }
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
    Ok(Launch { program, args: pre, env, effort: None })
}

/// Codex's model catalog decides most built-in tools per model (`tool_mode: code_mode_only` adds
/// `functions.exec`/`wait`, `multi_agent_version` adds `collaboration.*`, plus apply_patch, clock and
/// web search). Rewrite the cached catalog so every model runs tools directly and gets none of those,
/// leaving only MCP tools. Returns None until Codex has fetched its catalog once.
fn codex_catalog(home: &std::path::Path) -> anyhow::Result<Option<PathBuf>> {
    let Ok(raw) = std::fs::read_to_string(home.join("models_cache.json")) else { return Ok(None) };
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let Some(models) = doc.get_mut("models").and_then(|m| m.as_array_mut()) else { return Ok(None) };
    for m in models.iter_mut().filter_map(|m| m.as_object_mut()) {
        m.insert("tool_mode".into(), "direct_model_only".into());
        m.remove("multi_agent_version");
        m.insert("apply_patch_tool_type".into(), serde_json::Value::Null);
        m.insert("experimental_supported_tools".into(), serde_json::json!([]));
        m.insert("supports_search_tool".into(), false.into());
    }
    let out = home.join("catalog.locked.json");
    std::fs::write(&out, serde_json::to_vec(&serde_json::json!({ "models": models }))?)?;
    Ok(Some(out))
}

/// Built-in Cursor tools are chosen by the server; the `x-cursor-agent-allowed-tools` request header
/// (what the hidden `--allowed-tools` flag sets) keeps only the MCP gateway, whose `get_mcp_tools`
/// the server requires alongside `mcp`.
const CURSOR_TOOLS: &str = "mcp_tool_call,get_mcp_tools_tool_call";

fn cursor(ctx: &Ctx, note: &Note) -> anyhow::Result<Launch> {
    // cursor-agent reads mcp.json, plugins, skills and cli-config.json from `homedir()/.cursor`, so it
    // runs with a private HOME. Auth lives in the macOS login keychain, found via $HOME/Library/Keychains.
    let real_home = PathBuf::from(std::env::var("HOME")?);
    let home = paths::home().join("cursor-home");
    let dot = home.join(".cursor");
    std::fs::create_dir_all(&dot)?;
    std::fs::create_dir_all(home.join("Library"))?;
    for (src, dst) in [("Library/Keychains", "Library/Keychains"), (".gitconfig", ".gitconfig")] {
        let (src, dst) = (real_home.join(src), home.join(dst));
        if src.exists() && std::fs::symlink_metadata(&dst).is_err() {
            std::os::unix::fs::symlink(&src, &dst)?;
        }
    }

    // Only the harness MCP server. The note goes in as MCP server instructions: cursor has no
    // user-settable system prompt, and user rules come from the account, not disk.
    // The file is shared by every cursor session, so per-session values are `${env:...}` references that
    // cursor fills from its own environment (MCP servers don't inherit it otherwise): the note and the skills too, since
    // project skills depend on the session's directory.
    let env = json!({
        "HOME": real_home.display().to_string(),
        "PATH": std::env::var("PATH").unwrap_or_default(),
        "HARNESS_SESSION": "${env:HARNESS_SESSION}",
        "HARNESS_SOCK": "${env:HARNESS_SOCK}",
        "HARNESS_CWD": "${env:HARNESS_CWD}",
        "HARNESS_MCP_INSTRUCTIONS": "${env:HARNESS_MCP_INSTRUCTIONS}",
        "HARNESS_SKILLS": "${env:HARNESS_SKILLS}",
    });
    let mcp = json!({ "mcpServers": { "harness": { "command": "bun", "args": mcp_args()?, "env": env } } });
    let mcp = serde_json::to_vec_pretty(&mcp)?;
    if std::fs::read(dot.join("mcp.json")).ok().as_deref() != Some(&mcp[..]) {
        std::fs::write(dot.join("mcp.json"), mcp)?;
    }

    // Permissions: ts runs without prompts, every built-in that could still slip through is denied.
    // The first launch inherits the user's model choice from their real config.
    let cfg_path = dot.join("cli-config.json");
    let mut cfg: serde_json::Value = std::fs::read(&cfg_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| {
            let user: serde_json::Value = std::fs::read(real_home.join(".cursor/cli-config.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            let mut c = json!({ "version": 1 });
            for k in ["model", "selectedModel", "modelParameters", "hasChangedDefaultModel", "privacyCache"] {
                if let Some(v) = user.get(k) {
                    c[k] = v.clone();
                }
            }
            c
        });
    cfg["permissions"] = json!({
        "allow": ["Mcp(harness:*)"],
        "deny": ["Shell(*)", "Read(**)", "Write(**)", "WebFetch(*)"],
    });
    cfg["approvalMode"] = "allowlist".into();
    cfg["autoAcceptWebSearch"] = false.into();
    std::fs::write(&cfg_path, serde_json::to_vec_pretty(&cfg)?)?;

    // Cursor loads AGENTS.md, CLAUDE.md and .cursor/rules from its workspace (and the server refuses
    // --exclude-workspace-context), so its workspace is an empty private dir; ts still runs in the
    // session's real cwd via HARNESS_CWD.
    let workspace = home.join("workspace");
    std::fs::create_dir_all(&workspace)?;
    // The disabled list is keyed by project: the TUI uses its process cwd, other paths the workspace.
    // Paths are used as given or resolved depending on the caller, so cover both.
    for dir in [workspace.clone(), PathBuf::from(ctx.cwd)] {
        let real = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        cursor_disable_plugin_mcps(&dot, &dir)?;
        cursor_disable_plugin_mcps(&dot, &real)?;
    }
    let mut args: Vec<String> = [
        "--force", "--sandbox", "disabled", // no prompts, no sandbox (the one tool is ts)
        "--approve-mcps", "--trust", "--disable-auto-update",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push("--workspace".into());
    args.push(workspace.display().to_string());
    // `--allowed-tools` only reaches the request in print mode; the interactive TUI sends -H headers.
    args.push("-H".into());
    args.push(format!("x-cursor-agent-allowed-tools: {CURSOR_TOOLS}"));
    let mut env = common_env(ctx);
    env.insert("HOME".into(), home.display().to_string());
    env.insert("HARNESS_CWD".into(), ctx.cwd.into());
    env.insert("HARNESS_MCP_INSTRUCTIONS".into(), note.text.clone());
    env.insert("HARNESS_SKILLS".into(), note.skills.env());
    let program = which("cursor-agent").unwrap_or_else(|| real_home.join(".local/bin/cursor-agent").display().to_string());
    Ok(Launch { program, args, env, effort: None })
}

/// Account-installed marketplace plugins (synced into the private home) bring their own MCP servers,
/// named `plugin-<plugin>-<server>`. Cursor has no global switch, so list them in the per-project
/// disabled file (`~/.cursor/projects/<slug of git root>/mcp-disabled.json`) for `cwd`.
fn cursor_disable_plugin_mcps(dot: &std::path::Path, cwd: &std::path::Path) -> anyhow::Result<()> {
    let mut names = std::collections::BTreeSet::new();
    // The private cache fills on cursor's first start, so also read the user's real one.
    let caches = [dot.join("plugins/cache"), PathBuf::from(std::env::var("HOME")?).join(".cursor/plugins/cache")];
    for market in caches.iter().flat_map(|c| std::fs::read_dir(c).into_iter().flatten().flatten()) {
        for plugin in std::fs::read_dir(market.path()).into_iter().flatten().flatten() {
            for version in std::fs::read_dir(plugin.path()).into_iter().flatten().flatten() {
                let read = |p: &str| -> Option<serde_json::Value> {
                    serde_json::from_slice(&std::fs::read(version.path().join(p)).ok()?).ok()
                };
                let Some(servers) = read("mcp.json").and_then(|m| m.get("mcpServers").and_then(|s| s.as_object().cloned())) else { continue };
                let name = read(".cursor-plugin/plugin.json")
                    .and_then(|p| p.get("name").and_then(|n| n.as_str()).map(String::from))
                    .unwrap_or_else(|| plugin.file_name().to_string_lossy().into_owned());
                names.extend(servers.keys().map(|k| format!("plugin-{name}-{k}")));
            }
        }
    }
    if names.is_empty() {
        return Ok(());
    }
    let root = cwd.ancestors().find(|d| d.join(".git").exists()).unwrap_or(cwd);
    let slug = root.display().to_string().split(|c: char| !c.is_ascii_alphanumeric()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
    let dir = dot.join("projects").join(slug);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("mcp-disabled.json");
    let mut have: Vec<String> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    for n in names {
        if !have.contains(&n) {
            have.push(n);
        }
    }
    std::fs::write(path, serde_json::to_vec_pretty(&have)?)?;
    Ok(())
}

fn which(bin: &str) -> Option<String> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| PathBuf::from(d).join(bin)).find(|p| p.is_file()).map(|p| p.display().to_string())
}

/// The user's login shell from the account database (what Terminal/Ghostty start), not `$SHELL`, which is whatever
/// shell happened to launch the daemon.
fn login_shell() -> String {
    // SAFETY: getpwuid returns a pointer into static storage or null; we copy the string out at once.
    let from_db = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        (!pw.is_null() && !(*pw).pw_shell.is_null())
            .then(|| std::ffi::CStr::from_ptr((*pw).pw_shell).to_string_lossy().into_owned())
    };
    from_db.filter(|s| !s.is_empty()).or_else(|| std::env::var("SHELL").ok()).unwrap_or_else(|| "/bin/zsh".into())
}
