//! Pure `#![no_std]`, zero-unsafe, sans-IO parser and resolver for `~/.config/blaze-shell/config.toml`
//! built on `blaze-own` explicit allocators (`Alloc<'a>`, `AVec`, `AString`) and `blaze-toml`.
//!
//! Supports:
//! - Custom host symbols/labels (`[host]`, `[hosts.<name>]`)
//! - Per-project symbols/labels (`[projects."/path/to/repo"]` or shorthand `"/path" = "👋🌎"`)
//! - Custom command-position abbreviations (`[abbreviations]`, e.g. `cl = "claude --dangerously-skip-permissions"`)
//! - Custom manual CLI flag/subcommand completion overrides (`[completions.<cmd>]`)

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

use blaze_data::Value as DataValue;
use blaze_own::{AString, AVec, Alloc, String, Vec};

/// Starter `~/.config/blaze-shell/config.toml` written by the thin host driver on first launch.
pub const STARTER_CONFIG: &str = r#"# blaze-shell configuration (~/.config/blaze-shell/config.toml)
# Edits reload automatically — no restart needed.

[host]
# Replace the default laptop icon with any emoji or symbol:
# symbol = "🦑"
# Set label = "" to show only the symbol (omit to keep your hostname):
# label = ""

# Per-hostname overrides (works for both local and SSH hosts):
# [hosts.hydra]
# symbol = "🦑"
# label = ""

# Per-project symbols and optional display names (exact path, `~/...`, prefix, or folder name):
# [projects."/Volumes/Projects/tmp/hello-world"]
# symbol = "👋🌎"
# label = "hello-world"
#
# Shorthand one-liners also work:
# [projects]
# "/Volumes/Projects/tmp/hello-world" = "👋🌎"

# Prompt display options:
# [prompt]
# show_toolchains = true

# Custom command-position abbreviations (expanded inline on Space/Enter):
# [abbreviations]
# cl = "claude --dangerously-skip-permissions"

# Manual CLI completion overrides (augments automatic --help / man / fish completion parsing):
# [completions.mytool.flags]
# "--dry-run" = "Show what would happen without executing"
# [completions.mytool.subcommands]
# "deploy" = "Deploy service to cluster"
"#;

/// Custom symbol and optional display-label override for a host or project directory.
#[derive(Debug, PartialEq, Eq)]
pub struct SymbolRule<'a> {
    /// Custom symbol / emoji string (e.g. `"🦑"`, `"👋🌎"`, `"🔥"`).
    pub symbol: Option<AString<'a>>,
    /// Optional label override: `Some("")` hides the text for symbol-only display;
    /// `None` keeps the default hostname or directory name.
    pub label: Option<AString<'a>>,
}

impl<'a> SymbolRule<'a> {
    pub const fn empty() -> Self {
        Self { symbol: None, label: None }
    }

    pub fn symbol_str(&self) -> Option<&str> {
        self.symbol.as_ref().map(AString::as_str)
    }

    pub fn label_str(&self) -> Option<&str> {
        self.label.as_ref().map(AString::as_str)
    }
}

/// One key/value entry inside `[completions.<cmd>]` or `[abbreviations]`.
#[derive(Debug, PartialEq, Eq)]
pub struct NamedDesc<'a> {
    pub name: AString<'a>,
    pub desc: AString<'a>,
}

/// User-defined flag or subcommand completion override in `[completions.<cmd>]`.
#[derive(Debug, PartialEq, Eq)]
pub struct CompletionSpecOverride<'a> {
    pub command: AString<'a>,
    pub flags: AVec<'a, NamedDesc<'a>>,
    pub subcommands: AVec<'a, NamedDesc<'a>>,
}

/// Parsed `~/.config/blaze-shell/config.toml` allocated into an explicit [`Alloc`].
#[derive(Debug, PartialEq, Eq)]
pub struct UserConfig<'a> {
    /// Default symbol/label for the local machine (`[host]`).
    pub host: SymbolRule<'a>,
    /// Per-hostname overrides (`[hosts.<hostname>]`, case-insensitive match; works for SSH too).
    pub hosts: AVec<'a, (AString<'a>, SymbolRule<'a>)>,
    /// Per-project overrides (`[projects."/abs/path"]` or `[projects."~/path"]` or basename).
    pub projects: AVec<'a, (AString<'a>, SymbolRule<'a>)>,
    /// Command-position abbreviations (`[abbreviations]`).
    pub abbreviations: AVec<'a, NamedDesc<'a>>,
    /// Manual CLI flag/subcommand overrides (`[completions.<cmd>]`).
    pub completions: AVec<'a, CompletionSpecOverride<'a>>,
    /// Whether to display detected project toolchain/language badges (Rust, Python, Nix, etc.)
    /// in the prompt capsule (`[prompt] show_toolchains = true|false`, default `true`).
    pub show_toolchains: bool,
}

impl<'a> UserConfig<'a> {
    /// Empty config in `alloc`.
    pub fn empty_in(alloc: Alloc<'a>) -> Self {
        Self {
            host: SymbolRule::empty(),
            hosts: Vec::new_in(alloc),
            projects: Vec::new_in(alloc),
            abbreviations: Vec::new_in(alloc),
            completions: Vec::new_in(alloc),
            show_toolchains: true,
        }
    }

    /// Parse a TOML document into [`UserConfig`] using `blaze-toml` and explicit `alloc`.
    pub fn parse_in(toml_src: &str, alloc: Alloc<'a>) -> Self {
        let Ok(doc) = blaze_toml::Document::parse_in(toml_src, blaze_toml::Limits::SHIPPED, alloc)
        else {
            return Self::empty_in(alloc);
        };
        let val = doc.to_value_in(alloc);
        let DataValue::Map(ref root) = val else {
            return Self::empty_in(alloc);
        };

        let mut cfg = Self::empty_in(alloc);
        for (k, v) in root.iter() {
            match k {
                "host" => {
                    if let Some(rule) = parse_symbol_rule(v, alloc) {
                        cfg.host = rule;
                    }
                }
                "hosts" => {
                    if let DataValue::Map(m) = v {
                        for (hk, hv) in m.iter() {
                            if let Some(rule) = parse_symbol_rule(hv, alloc) {
                                cfg.hosts.push((lowercase_in(hk, alloc), rule));
                            }
                        }
                    }
                }
                "projects" => {
                    if let DataValue::Map(m) = v {
                        for (pk, pv) in m.iter() {
                            if let Some(rule) = parse_symbol_rule(pv, alloc) {
                                cfg.projects.push((String::from_str_in(pk, alloc), rule));
                            }
                        }
                    }
                }
                "abbreviations" | "abbr" => {
                    if let DataValue::Map(m) = v {
                        for (ak, av) in m.iter() {
                            if let DataValue::Text(t) = av {
                                let key = ak.trim();
                                let val_s = t.as_str().trim();
                                if !key.is_empty() && !val_s.is_empty() {
                                    cfg.abbreviations.push(NamedDesc {
                                        name: String::from_str_in(key, alloc),
                                        desc: String::from_str_in(val_s, alloc),
                                    });
                                }
                            }
                        }
                    }
                }
                "prompt" => {
                    if let DataValue::Map(m) = v {
                        for (pk, pv) in m.iter() {
                            if matches!(pk, "show_toolchains" | "toolchains")
                                && let DataValue::Bool(b) = *pv
                            {
                                cfg.show_toolchains = b;
                            }
                        }
                    }
                }
                "completions" => {
                    if let DataValue::Map(m) = v {
                        for (cmd_name, spec_val) in m.iter() {
                            if let Some(spec) = parse_completion_override(cmd_name, spec_val, alloc)
                            {
                                cfg.completions.push(spec);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        cfg
    }

    /// Resolve the effective symbol and label override for `(hostname, is_remote)`.
    pub fn resolve_host(&self, hostname: &str, is_remote: bool) -> Option<&SymbolRule<'a>> {
        let short = hostname.split('.').next().unwrap_or(hostname);
        for (hk, rule) in self.hosts.iter() {
            if hk.as_str().eq_ignore_ascii_case(hostname)
                || hk.as_str().eq_ignore_ascii_case(short)
            {
                return Some(rule);
            }
        }
        if !is_remote && (self.host.symbol.is_some() || self.host.label.is_some()) {
            return Some(&self.host);
        }
        None
    }

    /// Resolve the best matching project rule for `cwd` (longest path prefix wins; falls back to
    /// exact directory basename match when the config key has no `/`).
    pub fn resolve_project(&self, cwd: &str, home: &str) -> Option<&SymbolRule<'a>> {
        let norm_cwd = trim_trailing_slash(cwd.trim());
        let cwd_leaf = path_basename(norm_cwd);
        let home_trim = home.trim_end_matches('/');
        let mut best: Option<(usize, &SymbolRule<'a>)> = None;

        for (raw_key, rule) in self.projects.iter() {
            let key = raw_key.as_str().trim();
            if key.is_empty() {
                continue;
            }
            if !key.contains('/') && key != "~" {
                if cwd_leaf == key {
                    let score = 1usize;
                    if best.is_none_or(|(s, _)| score > s) {
                        best = Some((score, rule));
                    }
                }
                continue;
            }
            if let Some(matched_len) = match_path_prefix(norm_cwd, key, home_trim) {
                let score = matched_len + 100;
                if best.is_none_or(|(s, _)| score > s) {
                    best = Some((score, rule));
                }
            }
        }
        best.map(|(_, r)| r)
    }

    /// Look up user completion overrides for `command`.
    pub fn completion_override(&self, command: &str) -> Option<&CompletionSpecOverride<'a>> {
        self.completions
            .iter()
            .find(|c| c.command.as_str() == command)
    }
}

fn parse_symbol_rule<'a>(v: &DataValue<Alloc<'a>>, alloc: Alloc<'a>) -> Option<SymbolRule<'a>> {
    match v {
        DataValue::Text(s) => {
            let trimmed = s.as_str().trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(SymbolRule {
                    symbol: Some(String::from_str_in(trimmed, alloc)),
                    label: None,
                })
            }
        }
        DataValue::Map(m) => {
            let mut rule = SymbolRule::empty();
            for (k, val) in m.iter() {
                let DataValue::Text(t) = val else { continue };
                match k {
                    "symbol" | "icon" => {
                        let s = t.as_str().trim();
                        if !s.is_empty() {
                            rule.symbol = Some(String::from_str_in(s, alloc));
                        }
                    }
                    "label" | "name" => {
                        rule.label = Some(String::from_str_in(t.as_str(), alloc));
                    }
                    _ => {}
                }
            }
            if rule.symbol.is_some() || rule.label.is_some() {
                Some(rule)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn parse_completion_override<'a>(
    cmd_name: &str,
    v: &DataValue<Alloc<'a>>,
    alloc: Alloc<'a>,
) -> Option<CompletionSpecOverride<'a>> {
    let DataValue::Map(m) = v else {
        return None;
    };
    let mut flags = Vec::new_in(alloc);
    let mut subcommands = Vec::new_in(alloc);
    for (k, val) in m.iter() {
        match k {
            "flags" => {
                if let DataValue::Map(fm) = val {
                    for (fk, fv) in fm.iter() {
                        if let DataValue::Text(t) = fv {
                            flags.push(NamedDesc {
                                name: String::from_str_in(fk.trim(), alloc),
                                desc: String::from_str_in(t.as_str().trim(), alloc),
                            });
                        }
                    }
                }
            }
            "subcommands" | "commands" => {
                if let DataValue::Map(sm) = val {
                    for (sk, sv) in sm.iter() {
                        if let DataValue::Text(t) = sv {
                            subcommands.push(NamedDesc {
                                name: String::from_str_in(sk.trim(), alloc),
                                desc: String::from_str_in(t.as_str().trim(), alloc),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if flags.is_empty() && subcommands.is_empty() {
        None
    } else {
        Some(CompletionSpecOverride {
            command: String::from_str_in(cmd_name.trim(), alloc),
            flags,
            subcommands,
        })
    }
}

fn lowercase_in<'a>(s: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    for ch in s.chars() {
        out.push(ch.to_ascii_lowercase());
    }
    out
}

fn trim_trailing_slash(p: &str) -> &str {
    if p == "/" {
        "/"
    } else {
        p.trim_end_matches('/')
    }
}

fn path_basename(p: &str) -> &str {
    match p.rsplit_once('/') {
        Some((_, leaf)) => leaf,
        None => p,
    }
}

/// Zero-allocation directory-prefix match supporting leading `~` expansion.
fn match_path_prefix(norm_cwd: &str, raw_key: &str, home_trim: &str) -> Option<usize> {
    let key = trim_trailing_slash(raw_key);
    if key == "~" {
        return check_boundary_prefix(norm_cwd, home_trim);
    }
    if let Some(rest) = key.strip_prefix("~/") {
        let rest_cwd = norm_cwd.strip_prefix(home_trim)?.strip_prefix('/')?;
        let rest_key = trim_trailing_slash(rest);
        let sub_len = check_boundary_prefix(rest_cwd, rest_key)?;
        return Some(home_trim.len() + 1 + sub_len);
    }
    check_boundary_prefix(norm_cwd, key)
}

fn check_boundary_prefix(cwd: &str, key: &str) -> Option<usize> {
    if key.is_empty() {
        return None;
    }
    if cwd == key {
        return Some(key.len());
    }
    if key == "/" {
        return cwd.starts_with('/').then_some(1);
    }
    if cwd.len() > key.len()
        && cwd.starts_with(key)
        && cwd.as_bytes().get(key.len()) == Some(&b'/')
    {
        Some(key.len())
    } else {
        None
    }
}

/// Count visual grapheme clusters in a symbol/emoji string (correctly coalescing ZWJ sequences,
/// variation selectors `U+FE0E`/`U+FE0F`, skin-tone modifiers, and regional indicator flags) so
/// multi-emoji strings like `"👋🌎"` allocate exact proportional width without squishing.
pub fn symbol_cluster_count(s: &str) -> usize {
    let mut count = 0usize;
    let mut prev_zwj = false;
    let mut prev_regional = false;
    for ch in s.chars() {
        let cp = ch as u32;
        if cp == 0x200D {
            prev_zwj = true;
            prev_regional = false;
            continue;
        }
        if matches!(cp, 0xFE0E | 0xFE0F | 0x20E3)
            || (0x1F3FB..=0x1F3FF).contains(&cp)
            || (0xE0020..=0xE007F).contains(&cp)
        {
            continue;
        }
        if prev_zwj {
            prev_zwj = false;
            continue;
        }
        if (0x1F1E6..=0x1F1FF).contains(&cp) {
            if prev_regional {
                prev_regional = false;
                continue;
            }
            prev_regional = true;
            count += 1;
            continue;
        }
        prev_regional = false;
        count += 1;
    }
    count.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_project_abbreviations_and_completions_no_std() {
        let alloc = Alloc::global();
        let cfg = UserConfig::parse_in(
            r#"
[host]
symbol = "🦑"
label = ""

[projects."/Volumes/Projects/tmp/hello-world"]
symbol = "👋🌎"

[projects."~/work"]
symbol = "🔥"

[abbreviations]
cl = "claude --dangerously-skip-permissions"

[completions.mycli.flags]
"--fast" = "Enable turbo mode"

[completions.mycli.subcommands]
"run" = "Execute pipeline"
"#,
            alloc,
        );
        let h = cfg.resolve_host("hydra", false).expect("host rule");
        assert_eq!(h.symbol_str(), Some("🦑"));
        assert_eq!(h.label_str(), Some(""));
        assert_eq!(
            cfg.resolve_project("/Volumes/Projects/tmp/hello-world/src", "/Users/a")
                .and_then(SymbolRule::symbol_str),
            Some("👋🌎")
        );
        assert_eq!(
            cfg.resolve_project("/Users/a/work/sub", "/Users/a")
                .and_then(SymbolRule::symbol_str),
            Some("🔥")
        );
        assert_eq!(cfg.abbreviations.len(), 1);
        assert_eq!(cfg.abbreviations[0].name.as_str(), "cl");
        assert_eq!(
            cfg.abbreviations[0].desc.as_str(),
            "claude --dangerously-skip-permissions"
        );
        let ov = cfg.completion_override("mycli").expect("override");
        assert_eq!(ov.flags[0].name.as_str(), "--fast");
        assert_eq!(ov.subcommands[0].name.as_str(), "run");
        assert_eq!(symbol_cluster_count("🦑"), 1);
        assert_eq!(symbol_cluster_count("👋🌎"), 2);
    }
}
